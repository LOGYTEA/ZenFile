"""第53轮验收：表头右键 → 「其他(M)...」→「选择详细信息」对话框。

判据全部走真窗口像素，不看代码自证：
  D1 点「其他(M)...」后类名 ZenFileColDlg 的窗口真的出现，且尺寸≈380x432
  D2 列表渲染出 11 行勾选框：默认 4 列勾上（主色填充），其余空框
  D3 点第 5 行 → 该行勾上（勾选框变主色）
  D4 点「下移」→ 勾上的那一格与下一格互换（第5行空、第6行满）
  D5 点「确定」→ 对话框关闭
  D6 主窗口真的吃到了新列集合：重开表头菜单，橙色竖线行数 4 → 5

GetDIBits 原始缓冲是 BGRA，所以 px() 返回 (B,G,R)。
"""
import ctypes, ctypes.wintypes as wt, json, os, shutil, struct, subprocess, time

u = ctypes.WinDLL("user32", use_last_error=True)
g = ctypes.WinDLL("gdi32", use_last_error=True)
u.FindWindowW.restype = wt.HWND
u.FindWindowW.argtypes = [ctypes.c_wchar_p, ctypes.c_wchar_p]
u.GetWindowRect.restype = wt.BOOL
u.GetWindowRect.argtypes = [wt.HWND, ctypes.POINTER(wt.RECT)]
u.GetClientRect.restype = wt.BOOL
u.GetClientRect.argtypes = [wt.HWND, ctypes.POINTER(wt.RECT)]
u.GetWindowDC.restype = ctypes.c_void_p
u.GetWindowDC.argtypes = [wt.HWND]
u.PrintWindow.restype = wt.BOOL
u.PrintWindow.argtypes = [wt.HWND, ctypes.c_void_p, ctypes.c_uint]
u.PostMessageW.restype = ctypes.c_bool
u.PostMessageW.argtypes = [wt.HWND, ctypes.c_uint, ctypes.c_ulong, ctypes.c_long]
u.SetCursorPos.restype = wt.BOOL
u.SetCursorPos.argtypes = [ctypes.c_int, ctypes.c_int]
u.mouse_event.restype = None
u.mouse_event.argtypes = [ctypes.c_uint, ctypes.c_uint, ctypes.c_uint, ctypes.c_uint, ctypes.c_ulong]

# GDI 句柄一律按 c_void_p 收：HDC/HBITMAP 的值常常超过 2^31，
# 用 wintypes 的默认整型会在 ctypes 里直接 OverflowError。
g.CreateCompatibleDC.restype = ctypes.c_void_p
g.CreateCompatibleDC.argtypes = [ctypes.c_void_p]
g.CreateCompatibleBitmap.restype = ctypes.c_void_p
g.CreateCompatibleBitmap.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int]
g.SelectObject.restype = ctypes.c_void_p
g.SelectObject.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
g.GetDIBits.restype = ctypes.c_int
g.GetDIBits.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint, ctypes.c_uint,
                        ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint]
g.DeleteObject.restype = wt.BOOL
g.DeleteObject.argtypes = [ctypes.c_void_p]
g.DeleteDC.restype = wt.BOOL
g.DeleteDC.argtypes = [ctypes.c_void_p]

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\ZenFile.exe"
CFG_DIR = os.path.join(os.environ["APPDATA"], "ZenFile")
APP_CFG = os.path.join(CFG_DIR, "settings.json")
BAK = APP_CFG + ".bak"
BASE = os.path.join(os.environ["TEMP"], "NexColDlg")

VIEW_LEFT, VIEW_TOP, HEADER_H = 157, 74, 26
ROW_H = 28
ORANGE = (61, 163, 232)      # 0xE8A33D（表头菜单选中竖线）
ACCENT = (206, 117, 67)      # accent = rgb(0x4375CE) → COLORREF 0xCE7543 → 字节序 B,G,R

# 对话框几何（DIP * scale=1）。高度按行数动态算：LIST_TOP 80 + 21 行 × 26 + 底部 58
DLG_W = 380
LIST_TOP, DLG_ROW_H = 80, 26
BTN_ZONE = 58
CB_X0, CB_Y0, CB_SZ = 18, 5, 16     # 勾选框左上角（相对行顶）
COL_ALL = ["名称", "修改日期", "类型", "大小", "创建日期", "访问日期",
           "属性", "文件夹路径", "扩展名", "所有者", "文件版本", "文件说明",
           "产品名称", "公司", "版权", "磁盘占用", "MS-DOS 名称", "尺寸",
           "作者", "标记", "标题"]
DLG_H = LIST_TOP + len(COL_ALL) * DLG_ROW_H + BTN_ZONE
DEFAULT_VISIBLE = 4


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


def near(p, c, tol=40):
    return abs(p[0]-c[0]) + abs(p[1]-c[1]) + abs(p[2]-c[2]) <= tol


def win_origin(hwnd):
    r = wt.RECT()
    u.GetWindowRect(hwnd, ctypes.byref(r))
    return r.left, r.top


u.WindowFromPoint.restype = wt.HWND
u.WindowFromPoint.argtypes = [wt.POINT]
u.SetWindowPos.restype = wt.BOOL
u.SetWindowPos.argtypes = [wt.HWND, wt.HWND, ctypes.c_int, ctypes.c_int,
                           ctypes.c_int, ctypes.c_int, ctypes.c_uint]
HWND_TOPMOST = wt.HWND(-1)
SWP_NOSIZE, SWP_NOMOVE, SWP_SHOWWINDOW = 0x0001, 0x0002, 0x0040


def topmost(hwnd):
    """把窗口顶到最前：不置顶的话点击会被别的窗口吃掉，表现为"点了没反应"。"""
    u.SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0,
                   SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW)
    time.sleep(0.3)


def click_client(hwnd, cx, cy):
    """真鼠标点窗口客户区某点（PostMessage 会漏掉命中测试类缺陷）。"""
    ox, oy = win_origin(hwnd)
    u.SetCursorPos(ox + cx, oy + cy)
    time.sleep(0.15)
    p = wt.POINT(ox + cx, oy + cy)
    got = u.WindowFromPoint(p)
    if int(got or 0) != int(hwnd):
        print(f"    ! 点击落点被别的窗口接走了: {int(got or 0)} != {int(hwnd)}")
    u.mouse_event(0x0002, 0, 0, 0, 0)      # LEFTDOWN
    time.sleep(0.08)
    u.mouse_event(0x0004, 0, 0, 0, 0)      # LEFTUP
    time.sleep(0.4)


def cb_on(raw, stride, i):
    """第 i 行的勾选框是不是"已勾"（主色填充）。

    整框统计占比，不能只取框心：勾上的框里还画着一条 2px 白色对勾，
    正好穿过几何中心，只取中心那几个点会被白线吃掉，把"已勾"读成"未勾"。
    """
    x0 = CB_X0
    y0 = LIST_TOP + i * DLG_ROW_H + CB_Y0
    tot = hit = 0
    for y in range(y0 + 2, y0 + CB_SZ - 2):
        for x in range(x0 + 2, x0 + CB_SZ - 2):
            tot += 1
            if near(px(raw, stride, x, y), ACCENT):
                hit += 1
    return tot > 0 and hit >= tot * 0.4


def row_center_y(i):
    return LIST_TOP + i * DLG_ROW_H + DLG_ROW_H // 2


def menu_bar_rows(raw, stride, h):
    """表头菜单里带橙色竖线的行号集合（逐行判，行与行紧挨着）。"""
    got = []
    for i in range(len(COL_ALL)):
        y0 = 66 + i * ROW_H
        if y0 + ROW_H > h:
            break
        if any(near(px(raw, stride, x, y), ORANGE)
               for y in range(y0 + 6, y0 + ROW_H - 6) for x in range(0, 6)):
            got.append(i)
    return got


def main():
    fail = []
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
        time.sleep(0.6)
        subprocess.Popen([EXE, BASE])
        hw = None
        for _ in range(60):
            time.sleep(0.25)
            hw = u.FindWindowW("ZenFileMainWindow", None)
            if hw:
                break
        time.sleep(2.0)
        if not hw:
            print("FAIL D0: 主窗口没起来")
            return 1
        topmost(hw)

        # ---------- 打开表头菜单，点「其他(M)...」
        # 行序：0 自动调整 / 1 全部自动调整 / sep(6) / 11 列 / sep(6) / 其他
        # 其他 行 top = 4 + 28*2 + 6 + 28*11 + 6 = 380 → 中心 394
        def open_header_menu():
            """表头右键（真鼠标，带重试 —— PostMessage 的假点击偶发不生效）。"""
            for _ in range(4):
                x, y = VIEW_LEFT + 160, VIEW_TOP + HEADER_H // 2
                ox, oy = win_origin(hw)
                u.SetCursorPos(ox + x, oy + y)
                time.sleep(0.15)
                u.mouse_event(0x0008, 0, 0, 0, 0)   # RIGHTDOWN
                time.sleep(0.08)
                u.mouse_event(0x0010, 0, 0, 0, 0)   # RIGHTUP
                time.sleep(1.0)
                m = u.FindWindowW("ZenFileMenu", None)
                if m:
                    return m
                time.sleep(0.6)
            return None

        mh = open_header_menu()
        if not mh:
            print("FAIL D1: 表头菜单没弹出")
            return 1
        mr = wt.RECT(); u.GetWindowRect(mh, ctypes.byref(mr))
        # 「其他(M)...」是菜单最后一行：按菜单底边定位（列数变了行号会整体平移）
        my = (mr.bottom - mr.top) - 14
        print(f"D1 菜单 {mr.right-mr.left}x{mr.bottom-mr.top}，点「其他(M)...」(y={my})")
        click_client(mh, (mr.right - mr.left) // 2, my)
        time.sleep(1.2)

        dh = u.FindWindowW("ZenFileColDlg", None)
        if not dh:
            print("FAIL D1: 「选择详细信息」对话框没出现（找不到 ZenFileColDlg）")
            return 1
        dr = wt.RECT(); u.GetWindowRect(dh, ctypes.byref(dr))
        dw, dhgt = dr.right - dr.left, dr.bottom - dr.top
        print(f"D1 OK 对话框 {dw}x{dhgt} @ {dr.left},{dr.top}")
        topmost(dh)
        if abs(dw - DLG_W) > 24 or abs(dhgt - DLG_H) > 24:
            fail.append(f"D1 尺寸 {dw}x{dhgt} 偏离 {DLG_W}x{DLG_H}（21 列动态高度）")

        # ---------- D2 初始勾选态
        raw, stride, w, h = grab(dh)
        state = [cb_on(raw, stride, i) for i in range(len(COL_ALL))]
        print("D2 初始勾选:", "".join("■" if s else "□" for s in state))
        for i, s in enumerate(state):
            want = i < DEFAULT_VISIBLE
            if s != want:
                fail.append(f"D2 {COL_ALL[i]} 勾选={s} 期望={want}")
        if not fail:
            print("D2 OK 默认 4 列勾上、其余空框")

        # ---------- D3 点第 5 行（创建日期，下标 4）→ 勾上
        click_client(dh, 100, row_center_y(4))
        time.sleep(0.5)
        raw, stride, w, h = grab(dh)
        s3 = [cb_on(raw, stride, i) for i in range(len(COL_ALL))]
        print("D3 点第5行后:", "".join("■" if s else "□" for s in s3))
        if not s3[4]:
            fail.append("D3 点第 5 行后它没被勾上")
        elif s3[:5] == [True]*5 and not any(s3[5:]):
            print("D3 OK 第 5 行勾上")

        # ---------- D4 点「下移」：勾上的格子下移一格
        # 上移 rect: x 264..356 y 126..154；下移: y 164..192
        click_client(dh, 310, 178)
        time.sleep(0.5)
        raw, stride, w, h = grab(dh)
        s4 = [cb_on(raw, stride, i) for i in range(len(COL_ALL))]
        print("D4 下移后:  ", "".join("■" if s else "□" for s in s4))
        if s4[4] or not s4[5]:
            fail.append(f"D4 下移没生效（第5行={s4[4]} 第6行={s4[5]}，期望 False/True）")
        elif sum(s4) != 5:
            fail.append(f"D4 下移后勾上的总数 {sum(s4)} != 5")
        else:
            print("D4 OK 勾中的那格与下一格互换")

        # ---------- D5 确定 → 关闭（按钮贴着动态高度的底边）
        click_client(dh, 214, DLG_H - 29)
        time.sleep(1.0)
        if u.FindWindowW("ZenFileColDlg", None):
            fail.append("D5 点「确定」后对话框还在")
        else:
            print("D5 OK 对话框已关闭")

        # ---------- D6 主窗口吃到新列集合
        mh2 = open_header_menu()
        if not mh2:
            print("FAIL D6: 重开表头菜单失败")
            fail.append("D6 菜单没弹出")
        else:
            raw2, st2, w2, h2 = grab(mh2)
            bars = menu_bar_rows(raw2, st2, h2)
            print(f"D6 表头菜单橙色竖线行: {bars} → {[COL_ALL[i] for i in bars]}")
            if len(bars) != 5:
                fail.append(f"D6 可见列数 {len(bars)} != 5（下移后应保持 5 列）")
            else:
                print("D6 OK 主窗口列集合已按对话框结果更新（4 → 5）")

        print("PASS" if not fail else "FAIL: " + " | ".join(fail))
        return 0 if not fail else 1
    finally:
        subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
        if os.path.exists(BAK):
            shutil.move(BAK, APP_CFG)
            print("settings 已还原")


if __name__ == "__main__":
    raise SystemExit(main())
