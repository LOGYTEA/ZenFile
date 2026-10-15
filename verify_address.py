"""验证第 11 轮第 3 项：地址栏改成 Win11 面包屑样式。

不用"按墨迹切段"去数面包屑 —— 相邻两节之间只有一个 ≈8px 的分隔符 `›`，
它自己有墨迹，会把两节粘成一整条，数出来的段数毫无意义。

改用**悬停高亮**反推几何：悬停时那一节整条刷成 hover 色，抓图后找那条色带的
左右边界就得到这一节的真实矩形。既能数出几节，又顺带验了悬停反馈。

  1) 深路径下能数出多节，且各节矩形互不重叠、按 x 递增
  2) 点某一节 -> 真的导航到那一层
  3) 窗口拉窄后中间层级折叠，且不越出地址框右边框
  4) 点空白处 -> 进编辑态（EDIT 显形 + 完整真实路径）
  5) 编辑态回车 -> 收回面包屑
素材只在 %TEMP%。
"""
import ctypes
import ctypes.wintypes as wt
import os
import struct
import subprocess
import time

import verify_env

verify_env.isolate()

u = ctypes.WinDLL("user32", use_last_error=True)
g = ctypes.WinDLL("gdi32", use_last_error=True)
u.FindWindowW.restype = wt.HWND
u.FindWindowW.argtypes = [ctypes.c_wchar_p, ctypes.c_wchar_p]
u.GetWindowRect.restype = wt.BOOL
u.GetWindowRect.argtypes = [wt.HWND, ctypes.POINTER(wt.RECT)]
u.SetWindowPos.restype = wt.BOOL
u.SetWindowPos.argtypes = [wt.HWND, wt.HWND, ctypes.c_int, ctypes.c_int,
                           ctypes.c_int, ctypes.c_int, ctypes.c_uint]
u.GetWindowDC.restype = wt.HDC
u.PrintWindow.restype = wt.BOOL
u.PrintWindow.argtypes = [wt.HWND, wt.HDC, ctypes.c_uint]
u.GetDlgItem.restype = wt.HWND
u.SendMessageW.restype = ctypes.c_ssize_t
u.PostMessageW.restype = ctypes.c_bool
u.PostMessageW.argtypes = [wt.HWND, ctypes.c_uint, ctypes.c_ulong, ctypes.c_long]
u.IsWindowVisible.restype = wt.BOOL

g.CreateCompatibleDC.argtypes = [wt.HDC]
g.CreateCompatibleDC.restype = wt.HDC
g.CreateCompatibleBitmap.restype = wt.HBITMAP
g.CreateCompatibleBitmap.argtypes = [wt.HDC, ctypes.c_int, ctypes.c_int]
g.SelectObject.argtypes = [wt.HDC, wt.HGDIOBJ]
g.SelectObject.restype = wt.HGDIOBJ
g.GetDIBits.restype = ctypes.c_int
g.GetDIBits.argtypes = [wt.HDC, wt.HBITMAP, ctypes.c_uint, ctypes.c_uint,
                        ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint]

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\ZenFile.exe"
WM_LBUTTONDOWN, WM_LBUTTONUP = 0x0201, 0x0202
WM_GETTEXT = 0x000D
WM_KEYDOWN = 0x0100
WM_MOUSEMOVE = 0x0200
HOVER = (0x3D, 0x3D, 0x3D)

BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "ZenAddr")
os.makedirs(BASE, exist_ok=True)
DEEP = os.path.join(BASE, "Level1", "Level2", "Level3", "Level4")
os.makedirs(DEEP, exist_ok=True)
for i in range(3):
    open(os.path.join(DEEP, f"f{i}.txt"), "w").write("x")

ok_all = True


def mark(ok, msg):
    global ok_all
    ok_all = ok_all and ok
    print(f"    {'PASS' if ok else 'FAIL'}  {msg}")


def launch(arg):
    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    time.sleep(0.5)
    subprocess.Popen([EXE, arg])
    for _ in range(60):
        time.sleep(0.25)
        hw = u.FindWindowW("ZenFileMainWindow", None)
        if hw:
            time.sleep(1.8)
            return hw
    return None


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
    return buf.raw, stride, w, h


def px(raw, stride, x, y):
    o = y * stride + x * 4
    return (raw[o + 2], raw[o + 1], raw[o])


def addr_text(hwnd):
    buf = (ctypes.c_wchar * 512)()
    n = u.SendMessageW(u.GetDlgItem(hwnd, 101), WM_GETTEXT, 512,
                       ctypes.cast(buf, ctypes.c_void_p).value)
    return buf.value if n and n > 0 else ""


def click(hwnd, x, y):
    u.PostMessageW(hwnd, WM_LBUTTONDOWN, 0, (y << 16) | (x & 0xFFFF))
    u.PostMessageW(hwnd, WM_LBUTTONUP, 0, (y << 16) | (x & 0xFFFF))
    time.sleep(0.6)


def geom(hwnd):
    r = wt.RECT()
    u.GetWindowRect(hwnd, ctypes.byref(r))
    title_h, toolbar_h = 34, 40
    pad, nav = 6, 30
    return {
        "w": r.right - r.left,
        "h": r.bottom - r.top,
        # 右沿 = 窗口 - pad - 搜索框(170) - 「批量重命名」按钮 - 16。
        # 第 15 轮那个按钮是 132px 的文字按钮，第 16 轮改成了 30px 的图标按钮
        # （地址栏反而拿回 100px）—— 这里必须跟着改，否则"点地址栏空白处"
        # 会点到搜索框上去。
        "addr": (pad + nav * 3 + 10, title_h + pad,
                 (r.right - r.left) - pad - 170 - 30 - 16, title_h + toolbar_h - pad),
    }


def hover_band(hwnd, box, x):
    """悬停 x 处，返回被点亮的那一节的 (left, right)，没点亮返回 None。

    采样行取**文字上方**（地址框顶边 +4）：那里是纯底色，不会撞上文字抗锯齿
    产生的灰阶像素 —— 抗锯齿边缘里正好就有 #3D3D3D，会在中间行被误判成高亮。
    """
    x0, y0, x1, y1 = box
    yc = (y0 + y1) // 2
    yp = y0 + 4
    u.PostMessageW(hwnd, WM_MOUSEMOVE, 0, (yc << 16) | (x & 0xFFFF))
    time.sleep(0.12)
    raw, stride, w, h = grab(hwnd)
    if px(raw, stride, x, yp) != HOVER:
        return None, raw, stride, w, h
    lo = hi = x
    while lo - 1 > x0 and px(raw, stride, lo - 1, yp) == HOVER:
        lo -= 1
    while hi + 1 < x1 and px(raw, stride, hi + 1, yp) == HOVER:
        hi += 1
    return (lo, hi + 1), raw, stride, w, h


def enumerate_crumbs(hwnd, box, step=7):
    """扫一遍地址栏，靠悬停高亮反推出每一节的矩形。"""
    x0, y0, x1, y1 = box
    found = []
    x = x0 + 3
    while x < x1 - 2:
        band, *_ = hover_band(hwnd, box, x)
        if band and band[1] - band[0] >= 12:
            if band not in found:
                found.append(band)
            x = band[1] + 2
        else:
            x += step
    return sorted(found)


print("=" * 70)
print("地址栏（Win11 面包屑）")
hw = launch(DEEP)
G = geom(hw)
A = G["addr"]
ed = u.GetDlgItem(hw, 101)
print(f"  窗口 {G['w']}x{G['h']}  地址框 x {A[0]}..{A[2]}  y {A[1]}..{A[3]}")

crumbs = enumerate_crumbs(hw, A)
print(f"  悬停反推出 {len(crumbs)} 节: {crumbs}")
mark(len(crumbs) >= 4, f"深路径拆成了 {len(crumbs)} 节（此电脑+盘符+各级目录 ≥4）")
mark(all(crumbs[i][1] <= crumbs[i + 1][0] for i in range(len(crumbs) - 1)),
     "各节矩形按 x 递增且互不重叠（几何没错位）")

# 悬停反馈：某一节悬停前后，该节上方那一行的颜色必须变
if len(crumbs) >= 2:
    c = crumbs[1]
    yp = A[1] + 4
    u.PostMessageW(hw, WM_MOUSEMOVE, 0, (5 << 16) | (A[0] - 20 & 0xFFFF))
    time.sleep(0.2)
    raw0, s0, _, _ = grab(hw)
    before = px(raw0, s0, (c[0] + c[1]) // 2, yp)
    band, raw1, s1, _, _ = hover_band(hw, A, (c[0] + c[1]) // 2)
    after = px(raw1, s1, (c[0] + c[1]) // 2, yp)
    print(f"  第 2 节上方像素：未悬停 {before} -> 悬停 {after}")
    mark(before != after, "悬停有高亮反馈（不是点了没反应）")

# 点某一节 -> 导航到那一层。期望值按路径算出来，不能只判"包含"
parent = os.path.dirname(DEEP)
cases = [(0, "主页"), (1, "C:\\"), (2, "C:\\Users"), (-1, DEEP), (-2, parent)]
for idx, want in cases:
    hw = launch(DEEP)
    G = geom(hw)
    A = G["addr"]
    ed = u.GetDlgItem(hw, 101)
    cs = enumerate_crumbs(hw, A)
    if len(cs) < abs(idx) + 1:
        print(f"  节数不足，跳过 idx={idx}")
        continue
    c = cs[idx]
    before = addr_text(hw)
    click(hw, (c[0] + c[1]) // 2, (A[1] + A[3]) // 2)
    after = addr_text(hw)
    went_edit = bool(u.IsWindowVisible(ed))
    print(f"  点第 {idx} 节：'{before}' -> '{after}'（EDIT 可见={went_edit}）")
    mark(not went_edit, f"点第 {idx} 节命中了面包屑（没误进编辑态）")
    mark(after.lower() == want.lower(), f"点第 {idx} 节跳到了 {want!r}")

# 拉窄窗口：折叠
print("\n[折叠] 窗口拉到 620 宽")
hw = launch(DEEP)
u.SetWindowPos(hw, None, 100, 100, 620, 700, 0x0004)
time.sleep(1.0)
G = geom(hw)
A = G["addr"]
cs = enumerate_crumbs(hw, A)
print(f"  地址框 x {A[0]}..{A[2]}，{len(cs)} 节: {cs}")
mark(3 <= len(cs) < len(crumbs), f"窄窗口下折叠成 {len(cs)} 节（宽窗口 {len(crumbs)} 节）")
# 第 2 节应该是被折叠出来的 `…`（宽度明显小于真正的目录名）
mark(len(cs) >= 2 and cs[1][1] - cs[1][0] <= 30,
     f"折叠处出现 `…` 占位（第 2 节宽 {cs[1][1] - cs[1][0]}px）")
if len(cs) >= 2:
    before = addr_text(hw)
    click(hw, (cs[1][0] + cs[1][1]) // 2, (A[1] + A[3]) // 2)
    after = addr_text(hw)
    print(f"  点 `…`：'{before}' -> '{after}'")
    # 地址栏越窄，被折叠掉的层级越多，`…` 指向的那一层就越靠近末尾 —— 所以这里
    # 不能写死某一层，判据只能是"跳到了当前路径的某个**严格祖先**上"。
    is_ancestor = (
        before.lower().startswith(after.lower())
        and after.lower() != before.lower()
        and after.lower().rstrip("\\") != "c:"
    )
    mark(is_ancestor, f"点 `…` 跳到被折叠的那一层（{before} -> {after}）")
raw, stride, w, h = grab(hw)
# 不能拿"地址框右边 30px 内没有亮像素"当判据：那块地盘现在是**搜索框**，
# 它自己的放大镜和占位文字本来就该亮在那儿（第十轮加的）。真正要守的是
# 面包屑自己别越过地址框右沿 —— 折叠完最后一节的右边界必须在框内。
last_right = cs[-1][1] if cs else A[0]
gap = sum(
    1 for y in range(A[1] + 3, A[3] - 3)
    for x in range(A[2] + 1, min(A[2] + 6, w - 1))
    if sum(px(raw, stride, x, y)) > 200
)
print(f"  最后一节右沿 {last_right} / 地址框右沿 {A[2]}；紧邻右侧 5px 亮像素 {gap}")
mark(last_right <= A[2] - 1, f"折叠后面包屑仍收在地址框内（右沿 {last_right} ≤ {A[2]}）")
mark(gap == 0, f"地址框右侧留有空隙，没有糊到下一个控件上（{gap} px）")

# 编辑态
print("\n[编辑态] 点地址栏空白处")
hw = launch(DEEP)
G = geom(hw)
A = G["addr"]
ed = u.GetDlgItem(hw, 101)
cs = enumerate_crumbs(hw, A)
blank_x = (cs[-1][1] + A[2]) // 2 if cs else A[0] + 10
vis_before = bool(u.IsWindowVisible(ed))
click(hw, blank_x, (A[1] + A[3]) // 2)
vis_after = bool(u.IsWindowVisible(ed))
print(f"  点空白 x={blank_x}: EDIT 可见 {vis_before} -> {vis_after}")
mark(not vis_before and vis_after, "点空白处切到可输入地址栏（EDIT 显形）")
txt = addr_text(hw)
print(f"  EDIT 文本 = '{txt}'")
mark(txt.endswith("Level4"), "编辑态里填的是完整真实路径")
u.PostMessageW(ed, WM_KEYDOWN, 0x0D, 0x000C001)
time.sleep(0.8)
mark(not bool(u.IsWindowVisible(ed)), "回车之后收回面包屑（EDIT 重新隐藏）")

subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
print("\n" + "=" * 70)
print("地址栏全部通过" if ok_all else "地址栏存在失败项")
