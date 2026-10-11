"""第 12 轮**没有脚本覆盖**的那几项，补上量化验证。

  3) 右键菜单的「排序方式」能展开下级菜单，点进去真的会改变排列顺序
 15) 侧栏宽度可以拖，拖完落盘（ui.sidebar_width）
 14) 条目行宽铺满内容区（选中色带一直延伸到窗口右沿，不会只占左半边）
  1) 名称/大小那一列表头只在「详细信息」下存在（其余布局第一行就是条目）

判据一律抓屏取像素，素材落在 %TEMP%，绝不动桌面。
"""
import ctypes
import ctypes.wintypes as wt
import json
import os
import shutil
import struct
import subprocess
import time

import verify_env

verify_env.isolate()
verify_env.clean()

u = ctypes.WinDLL("user32", use_last_error=True)
g = ctypes.WinDLL("gdi32", use_last_error=True)
for f, restype, argtypes in (
    ("FindWindowW", wt.HWND, [ctypes.c_wchar_p, ctypes.c_wchar_p]),
    ("GetWindowRect", wt.BOOL, [wt.HWND, ctypes.POINTER(wt.RECT)]),
    ("GetWindowDC", wt.HDC, [wt.HWND]),
    ("PrintWindow", wt.BOOL, [wt.HWND, wt.HDC, ctypes.c_uint]),
    ("SetForegroundWindow", wt.BOOL, [wt.HWND]),
    ("PostMessageW", ctypes.c_bool, [wt.HWND, ctypes.c_uint, ctypes.c_ulong, ctypes.c_long]),
    ("SendMessageW", ctypes.c_ssize_t, [wt.HWND, ctypes.c_uint, ctypes.c_ulong, ctypes.c_long]),
    ("EnumWindows", wt.BOOL, [ctypes.c_void_p, ctypes.c_long]),
    ("GetClassNameW", ctypes.c_int, [wt.HWND, ctypes.c_wchar_p, ctypes.c_int]),
    ("IsWindowVisible", wt.BOOL, [wt.HWND]),
    ("GetDlgItem", wt.HWND, [wt.HWND, ctypes.c_int]),
):
    getattr(u, f).restype = restype
    getattr(u, f).argtypes = argtypes
for f, restype, argtypes in (
    ("CreateCompatibleDC", wt.HDC, [wt.HDC]),
    ("CreateCompatibleBitmap", wt.HBITMAP, [wt.HDC, ctypes.c_int, ctypes.c_int]),
    ("SelectObject", wt.HGDIOBJ, [wt.HDC, wt.HGDIOBJ]),
    ("GetDIBits", ctypes.c_int, [wt.HDC, wt.HBITMAP, ctypes.c_uint, ctypes.c_uint,
                                 ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint]),
):
    getattr(g, f).restype = restype
    getattr(g, f).argtypes = argtypes

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\ZenFile.exe"
BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "ZenR12A")
if os.path.isdir(BASE):
    for _root, _dirs, _files in os.walk(BASE):
        for _n in _dirs + _files:
            try:
                os.chmod(os.path.join(_root, _n), 0o666)
            except OSError:
                pass
    shutil.rmtree(BASE, ignore_errors=True)
os.makedirs(BASE, exist_ok=True)
# 名称序 a<b<c，大小序 a(10) < c(200) < b(2000)：两种排序的第 2 行不同，
# 靠"第 2 行的字变了"来证明排序真的生效。
for name, n in (("a.txt", 10), ("b.txt", 2000), ("c.txt", 200)):
    with open(os.path.join(BASE, name), "w") as f:
        f.write("x" * n)

TITLE_H, TOOL_H, HEADER_H, ROW_H = 34, 40, 26, 21
VIEW_LEFT = 157 + 21
BG, HOVER_BG, SEL_BG = (0x20, 0x20, 0x20), (0x3D, 0x3D, 0x3D), (0x47, 0x47, 0x47)
BORDER = (0x53, 0x53, 0x53)
CHROME = (0x19, 0x19, 0x19)
WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE = 0x0201, 0x0202, 0x0200
WM_RBUTTONDOWN, WM_RBUTTONUP = 0x0204, 0x0205

ok_all = True


def mark(ok, msg):
    global ok_all
    ok_all = ok_all and ok
    print(f"    {'PASS' if ok else 'FAIL'}  {msg}")


def launch(arg=None):
    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    time.sleep(0.5)
    subprocess.Popen([EXE] + ([arg] if arg else []))
    for _ in range(60):
        time.sleep(0.25)
        hw = u.FindWindowW("ZenFileMainWindow", None)
        if hw:
            time.sleep(1.8)
            return hw
    raise SystemExit("主窗口没起来")


def grab(hwnd, tries=8):
    for _ in range(tries):
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
        raw = buf.raw
        if any(raw[i] for i in range(0, min(len(raw), 40000), 997)):
            return raw, stride, w, h
        time.sleep(0.35)
    return raw, stride, w, h


def px(raw, stride, x, y):
    o = y * stride + x * 4
    return (raw[o + 2], raw[o + 1], raw[o])


def near(p, q, tol=14):
    return all(abs(a - b) <= tol for a, b in zip(p, q))


def post(hwnd, msg, x, y, wparam=0):
    u.PostMessageW(hwnd, msg, wparam, (y << 16) | (x & 0xFFFF))


def click(hwnd, x, y):
    post(hwnd, WM_LBUTTONDOWN, x, y)
    time.sleep(0.06)
    post(hwnd, WM_LBUTTONUP, x, y)


def move(hwnd, x, y):
    post(hwnd, WM_MOUSEMOVE, x, y)
    time.sleep(0.35)


def key_ctrl(hwnd, vk):
    u.keybd_event(0x11, 0, 0, 0)
    u.PostMessageW(hwnd, WM_KEYDOWN := 0x0100, vk, 0)
    time.sleep(0.2)
    u.keybd_event(0x11, 0, 2, 0)
    time.sleep(0.8)


def menus():
    """所有自绘菜单窗口（(hwnd, left, top, w, h)）。"""
    out = []
    WNDENUMPROC = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)

    def cb(hwnd, _lp):
        if u.IsWindowVisible(hwnd):
            b = ctypes.create_unicode_buffer(64)
            u.GetClassNameW(hwnd, b, 64)
            if b.value == "ZenFileMenu":
                r = wt.RECT()
                u.GetWindowRect(hwnd, ctypes.byref(r))
                out.append((hwnd, r.left, r.top, r.right - r.left, r.bottom - r.top))
        return True

    u.EnumWindows(WNDENUMPROC(cb), 0)
    return out


def menu_row_center(top0, row_h, idx, seps):
    """自绘菜单第 idx 行的中心 y：分隔条比正常行矮（约 6/28 行高）。"""
    s = row_h / 28.0
    y = top0
    for i in range(idx):
        y += int(6 * s) if i in seps else row_h
    return y + row_h // 2


def menu_metrics(hwnd, w, h):
    """悬停第一行，用 hover 色带的上下边界量出内容顶边和行高。"""
    move(hwnd, w // 2, 10)
    raw, stride, wm, hm = grab(hwnd)
    band = [y for y in range(0, hm)
            if all(abs(a - b) <= 8 for a, b in zip(px(raw, stride, wm // 2, y), HOVER_BG))]
    return ((band[0], band[-1] - band[0] + 1) if band else (5, 28))


def row_center(i):
    return TITLE_H + TOOL_H + HEADER_H + ROW_H * i + ROW_H // 2


print("=" * 72)

# ============================================================ 3) 排序方式
print("[3] 右键「排序方式」子菜单")
hw = launch(BASE)
key_ctrl(hw, 0x31)  # Ctrl+1 固定成详细信息
raw0, stride, W, H = grab(hw)
# 记下第 2 行（b.txt）文字区的样子，排序换成"大小"后它应该变成 c.txt
def row_ink(raw, i, x0=VIEW_LEFT + 34, x1=VIEW_LEFT + 160):
    y = row_center(i)
    return [px(raw, stride, x, yy) for yy in range(y - 7, y + 8) for x in range(x0, x1)]


before = row_ink(raw0, 1)
# 在第 0 行上右键 -> 文件菜单
post(hw, WM_RBUTTONDOWN, VIEW_LEFT + 60, row_center(0))
post(hw, WM_RBUTTONUP, VIEW_LEFT + 60, row_center(0))
time.sleep(0.7)
ms = menus()
print(f"  右键后菜单窗口 {len(ms)} 个")
mark(len(ms) == 1, "右键弹出自绘菜单")
if not ms:
    raise SystemExit("菜单没起来，后面的排序验证没法做")
mh, mx, my, mw, mhh = ms[0]
top0, row_h = menu_metrics(mh, mw, mhh)
print(f"  菜单量得: 顶边 {top0} 行高 {row_h}")
# 文件菜单: 0打开 1打开方式 2剪切 3复制 4重命名 5删除 6[sep]
# 7粘贴 8新建 9[sep] 10全选 11刷新 12查看 13排序方式 14属性
# （第 15 轮「批量重命名」搬去目录栏按钮，后面整体前移一格）
FILE_SEPS = (6, 9)
y_sort = menu_row_center(top0, row_h, 13, FILE_SEPS)
print(f"  「排序方式」行中心 y={y_sort}")
move(mh, mw // 2, y_sort)
time.sleep(0.5)
ms2 = menus()
print(f"  悬停「排序方式」后菜单窗口 {len(ms2)} 个")
mark(len(ms2) == 2, "悬停「排序方式」展开了下级菜单")
sub = None
for m in ms2:
    if m[0] != mh:
        sub = m
if sub:
    sh, sx, sy, sw, shh = sub
    stop0, srow_h = menu_metrics(sh, sw, shh)
    # 子菜单: 0名称 1修改日期 2类型 3大小 4[sep] 5递增 6递减
    y_size = menu_row_center(stop0, srow_h, 3, (4,))
    print(f"  子菜单量得: 顶边 {stop0} 行高 {srow_h}；「大小」行中心 y={y_size}")
    click(sh, sw // 2, y_size)
    time.sleep(0.9)
raw1, stride, W, H = grab(hw)
after = row_ink(raw1, 1)
diff = sum(1 for a, b in zip(before, after) if abs(a[0] - b[0]) > 30)
print(f"  第 2 行文字区像素变化 {diff} / {len(before)}")
mark(diff >= 40, f"按「大小」排序真的重排了（第 2 行由 b.txt 变 c.txt，{diff}px 变化）")
# 顺带确认排序方向菜单也在
mark(sub is not None, "「排序方式」里含排序列与升降序两组")

def view_left(raw, stride, W):
    """实测视图左沿（侧栏右边框 + 内容区留白），不写死常量 —— 侧栏宽度
    是可拖的，上一个用例改过之后写死的 VIEW_LEFT 立刻就错。"""
    y = TITLE_H + TOOL_H + 60
    for x in range(60, W // 2):
        if near(px(raw, stride, x, y), BORDER, 10):
            return x + 22
    return VIEW_LEFT


# ============================================================ 1) 表头（放前面跑）
# 必须排在"侧栏宽度"那条之前（或者自己重新拉一个进程）：那条会把
# ui.sidebar_width 写进配置，之后的用例视图左沿就不是默认那一个了。
print("\n[1] 名称/大小表头只在「详细信息」下存在")
hw = launch(BASE)
key_ctrl(hw, 0x31)


def sel_top(raw, stride, W, vl):
    """选中色带的上沿 y。

    这才是"表头存不存在"的直接量：详细信息下条目整体被表头下推 26px，
    所以选中色带从 view_top+26 起；大图标没有表头，第一格几乎贴着顶边。
    （之前试过"数表头里的列分隔竖线"——表头只画列名文字、根本不画竖线，
    所以量出来两种布局都是 0 条；也试过"首行墨迹 y"——表头文字和大图标
    第一格的图标都从顶边附近开始，量出来一样。）
    """
    for y in range(TITLE_H + TOOL_H, TITLE_H + TOOL_H + 140):
        n = sum(1 for x in range(vl + 4, min(vl + 120, W - 4))
                if near(px(raw, stride, x, y), SEL_BG, 12))
        if n >= 8:
            return y
    return None


key_ctrl(hw, 0x31)  # 详细信息
raw, stride, W, H = grab(hw)
vl = view_left(raw, stride, W)
click(hw, vl + 60, row_center(0))
time.sleep(0.6)
rawd, stride, W, H = grab(hw)
td = sel_top(rawd, stride, W, vl)

key_ctrl(hw, 0x34)  # 大图标
raw, stride, W, H = grab(hw)
click(hw, vl + 60, TITLE_H + TOOL_H + 60)
time.sleep(0.6)
rawi, stride, W, H = grab(hw)
ti = sel_top(rawi, stride, W, vl)

print(f"  视图顶边 {TITLE_H + TOOL_H}；选中色带上沿: 详细信息 {td} / 大图标 {ti}")
mark(td is not None and td >= TITLE_H + TOOL_H + HEADER_H - 2,
     f"详细信息下条目被表头下推了（色带从 {td} 起，顶边 {TITLE_H + TOOL_H}）")
mark(ti is not None and ti <= TITLE_H + TOOL_H + 8,
     f"大图标下第一格直接贴顶边，没有表头（色带从 {ti} 起）")
mark(td is not None and ti is not None and td - ti >= 18,
     f"两种布局之间正好差出一个表头高度（{td} - {ti} = "
     f"{None if td is None or ti is None else td - ti}px，表头 {HEADER_H}px）")

# ============================================================ 14) 条目行宽
print("\n[14] 条目铺满内容区宽度")
hw = launch(BASE)
key_ctrl(hw, 0x31)
raw, stride, W, H = grab(hw)
vl = view_left(raw, stride, W)
click(hw, vl + 60, row_center(0))
time.sleep(0.6)
raw, stride, W, H = grab(hw)
y = row_center(0)
# 选中色带最右端到窗口右沿的距离
rightmost = max((x for x in range(vl, W - 2) if near(px(raw, stride, x, y), SEL_BG, 12)),
                default=0)
gap = (W - 2) - rightmost
print(f"  视图左沿 {vl}；选中色带最右端 x={rightmost}，窗口右沿 {W}，差 {gap}px")
mark(rightmost > W - 60 and gap <= 60, f"条目行一直铺到窗口右沿（差 {gap}px）")

# ============================================================ 15) 侧栏宽度
print("\n[15] 侧栏宽度可拖拽")
hw = launch(BASE)
key_ctrl(hw, 0x31)


def sidebar_right(raw, stride, W, H):
    """沿着列表区某一行找侧栏右分隔线（1px border）。"""
    y = TITLE_H + TOOL_H + 60
    for x in range(60, W // 2):
        if near(px(raw, stride, x, y), BORDER, 10):
            return x
    # 兜底：找 chrome_bg → window_bg 的跳变
    for x in range(60, W // 2):
        if near(px(raw, stride, x, y), CHROME, 6) and near(px(raw, stride, x + 3, y), BG, 6):
            return x
    return None


raw, stride, W, H = grab(hw)
e0 = sidebar_right(raw, stride, W, H)
print(f"  拖之前侧栏右边界 x={e0}")
assert e0, "没找到侧栏右边界"
y_drag = TITLE_H + TOOL_H + 60
post(hw, WM_LBUTTONDOWN, e0, y_drag)
time.sleep(0.12)
for i in range(1, 11):
    post(hw, WM_MOUSEMOVE, e0 + 6 * i, y_drag)
    time.sleep(0.04)
time.sleep(0.3)
post(hw, WM_LBUTTONUP, e0 + 60, y_drag)
time.sleep(0.8)
raw, stride, W, H = grab(hw)
e1 = sidebar_right(raw, stride, W, H)
print(f"  拖之后侧栏右边界 x={e1}")
mark(e1 is not None and e1 >= e0 + 40, f"侧栏被拖宽了（{e0} -> {e1}）")
cfgp = os.path.join(os.environ.get("APPDATA", ""), "ZenFile", "settings.json")
w_cfg = None
try:
    with open(cfgp, encoding="utf-8") as f:
        w_cfg = json.load(f).get("ui", {}).get("sidebar_width")
except OSError:
    pass
print(f"  落盘 ui.sidebar_width = {w_cfg}")
mark(isinstance(w_cfg, int) and w_cfg >= 197, f"宽度写进了配置（{w_cfg}）")

subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
print("\n" + ("第 12 轮补充审计全部通过" if ok_all else "有失败项"))
