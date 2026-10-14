"""第52轮验收：表头「选择列」菜单的选中态 = 右键菜单同款行首橙色竖线。

判据（不靠肉眼，靠像素）：
  1) 菜单左侧 3px 带里出现**橙色**竖线段，段数 == 当前可见列数（默认 4）
  2) 存在没有任何竖线的行（= 未显示的列），证明"选中/未选中"确实靠竖线区分
  3) 悬停那条蓝色强调线不参与统计（截图前把鼠标移开）

橙/蓝取 theme.rs：dark 主题 orange=0xE8A33D、accent=0xCE7543。
GetDIBits 原始缓冲是 BGRA，所以 px() 返回的是 (B,G,R)。
"""
import ctypes, ctypes.wintypes as wt, json, os, shutil, struct, subprocess, time

u = ctypes.WinDLL("user32", use_last_error=True)
g = ctypes.WinDLL("gdi32", use_last_error=True)
u.FindWindowW.restype = wt.HWND
u.FindWindowW.argtypes = [ctypes.c_wchar_p, ctypes.c_wchar_p]
u.GetWindowRect.restype = wt.BOOL
u.GetWindowRect.argtypes = [wt.HWND, ctypes.POINTER(wt.RECT)]
u.GetWindowDC.restype = wt.HDC
u.PrintWindow.restype = wt.BOOL
u.PrintWindow.argtypes = [wt.HWND, wt.HDC, ctypes.c_uint]
u.PostMessageW.restype = ctypes.c_bool
u.PostMessageW.argtypes = [wt.HWND, ctypes.c_uint, ctypes.c_ulong, ctypes.c_long]
u.SetCursorPos.restype = wt.BOOL
g.CreateCompatibleDC.restype = wt.HDC
g.CreateCompatibleDC.argtypes = [wt.HDC]
g.CreateCompatibleBitmap.restype = wt.HBITMAP
g.CreateCompatibleBitmap.argtypes = [wt.HDC, ctypes.c_int, ctypes.c_int]
g.SelectObject.restype = wt.HGDIOBJ
g.SelectObject.argtypes = [wt.HDC, wt.HGDIOBJ]
g.GetDIBits.restype = ctypes.c_int
g.GetDIBits.argtypes = [wt.HDC, wt.HBITMAP, ctypes.c_uint, ctypes.c_uint,
                        ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint]
g.DeleteObject.restype = wt.BOOL
g.DeleteObject.argtypes = [wt.HGDIOBJ]
g.DeleteDC.restype = wt.BOOL
g.DeleteDC.argtypes = [wt.HDC]

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\ZenFile.exe"
CFG_DIR = os.path.join(os.environ["APPDATA"], "ZenFile")
APP_CFG = os.path.join(CFG_DIR, "settings.json")
BAK = APP_CFG + ".bak"
BASE = os.path.join(os.environ["TEMP"], "NexHeadMenu")

VIEW_LEFT, VIEW_TOP, HEADER_H = 157, 74, 26
ROW_H = 28                   # MENU_ROW * scale(1)
ORANGE = (61, 163, 232)      # 0xE8A33D 的 BGRA
ACCENT = (67, 117, 206)      # 0xCE7543（悬停蓝条，仅用于排除）


def grab(hwnd):
    r = wt.RECT()
    u.GetWindowRect(hwnd, ctypes.byref(r))
    w, h = r.right - r.left, r.bottom - r.top
    src = u.GetWindowDC(hwnd)
    mdc = g.CreateCompatibleDC(src)
    bmp = g.CreateCompatibleBitmap(src, w, h)
    g.SelectObject(mdc, bmp)
    u.PrintWindow(hwnd, mdc, 2)
    stride = ((w * 32 + 31) // 32) * 4
    hdr = struct.pack("<IiiHHIIiiII", 40, w, -h, 1, 32, 0, stride * h, 0, 0, 0, 0)
    buf = ctypes.create_string_buffer(stride * h)
    g.GetDIBits(mdc, bmp, 0, h, buf, hdr, 0)
    g.DeleteObject(bmp); g.DeleteDC(mdc)
    return buf.raw, stride, w, h


def px(raw, stride, x, y):
    o = y * stride + x * 4
    return (raw[o], raw[o + 1], raw[o + 2])


def near(p, c, tol=30):
    return abs(p[0]-c[0]) + abs(p[1]-c[1]) + abs(p[2]-c[2]) <= tol


def row_has_bar(raw, stride, y0, color):
    """某一行（y0..y0+ROW_H）左侧 0..5px 带里有没有指定颜色的竖线。

    行与行是紧挨着的（菜单 top 每次 += row_h），所以"连续段"会并成一条，
    必须按行高逐行判，不能数段。
    """
    for y in range(y0 + 6, y0 + ROW_H - 6):
        if any(near(px(raw, stride, x, y), color) for x in range(0, 6)):
            return True
    return False


# 菜单行几何：top 从 4 起；前两条命令各 28，一条分隔线 6 → 列从 y=66 开始。
# 第54轮改成"前 8 项内联 + 其他(M)..."：内联的是 COL_ALL 的前 8 个。
COL_NAMES = ["名称", "修改日期", "类型", "大小", "创建日期", "访问日期",
             "属性", "文件夹路径"]
# normalize_visible 默认四列（名称/修改日期/类型/大小）
DEFAULT_VISIBLE = 4


def main():
    fail = []
    # 配置：暗色 + 详细信息，保证表头存在、列可见性走默认四列
    if os.path.exists(APP_CFG):
        shutil.copy2(APP_CFG, BAK)
    json.dump({"core": {"theme": "dark"},
               "files": {"view": {"default_mode": "details"}}},
              open(APP_CFG, "w", encoding="utf-8"))

    shutil.rmtree(BASE, ignore_errors=True)
    os.makedirs(BASE, exist_ok=True)
    for i in range(5):
        open(os.path.join(BASE, f"f{i}.txt"), "w").write("x")

    try:
        subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
        time.sleep(0.5)
        subprocess.Popen([EXE, BASE])
        hw = None
        for _ in range(60):
            time.sleep(0.25)
            hw = u.FindWindowW("ZenFileMainWindow", None)
            if hw:
                break
        time.sleep(2.0)
        if not hw:
            print("FAIL: 主窗口没起来"); return 1

        # 表头右键 → 选择列菜单
        x, y = VIEW_LEFT + 160, VIEW_TOP + HEADER_H // 2
        u.PostMessageW(hw, 0x0204, 0x0002, (y << 16) | x)   # WM_RBUTTONDOWN
        u.PostMessageW(hw, 0x0205, 0x0000, (y << 16) | x)   # WM_RBUTTONUP
        time.sleep(1.0)
        mh = u.FindWindowW("ZenFileMenu", None)
        if not mh:
            print("FAIL: 菜单窗口没弹出（类名 ZenFileMenu 找不到）"); return 1

        # 鼠标移开，避免悬停蓝条混入统计
        u.SetCursorPos(2, 2)
        time.sleep(0.6)

        raw, stride, w, h = grab(mh)
        print(f"菜单窗口 {w}x{h}")

        # 逐行判：前 4 行（默认可见列）必须有橙色竖线，其余列不能有误
        got, wrong = [], []
        for i, name in enumerate(COL_NAMES):
            y0 = 66 + i * ROW_H
            if y0 + ROW_H > h:
                print(f"  {name}: 超出可视区，跳过")
                continue
            has = row_has_bar(raw, stride, y0, ORANGE)
            if has:
                got.append(name)
            want = i < DEFAULT_VISIBLE
            if has != want:
                wrong.append(f"{name}({'应有' if want else '应无'}竖线，实测{'有' if has else '无'})")
            print(f"  {name:<6} y={y0:>3}  竖线={'有' if has else '无'}  {'OK' if has == want else '×'}")

        accent = sum(1 for i in range(len(COL_NAMES))
                     if row_has_bar(raw, stride, 66 + i * ROW_H, ACCENT))
        print(f"带竖线的列: {got}")

        if wrong:
            fail.append("; ".join(wrong))
        if len(got) != DEFAULT_VISIBLE:
            fail.append(f"竖线列数 {len(got)} != 默认可见列数 {DEFAULT_VISIBLE}")
        if accent:
            fail.append(f"{accent} 行出现悬停蓝条（鼠标没移干净）")

        print("PASS" if not fail else "FAIL: " + "; ".join(fail))
        return 0 if not fail else 1
    finally:
        subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
        if os.path.exists(BAK):
            shutil.move(BAK, APP_CFG)
            print("settings 已还原")


if __name__ == "__main__":
    raise SystemExit(main())
