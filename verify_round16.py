"""第 16 轮 5 项的像素级回归。

  1) 侧栏改成真树线：每层竖线只在"后面还有兄弟"时画，末位孩子收口 + 肘形连接线
  2) 「批量重命名」做成图标按钮，目录栏最右侧、常驻、不挤占地址栏
  3) 右键菜单按右键对象自适应：右键文件/文件夹时不再有 查看/排序方式/刷新/新建
  4) 鼠标悬停到条目上有 hover 反馈，强度比选中弱一档
  5) 侧栏单击只导航不展开，双击才展开

判定全部走像素 / 窗口事实，不靠肉眼。
"""
import ctypes
import ctypes.wintypes as wt
import os
import shutil
import struct
import subprocess
import time
from collections import Counter

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
    ("SendInput", ctypes.c_uint, [ctypes.c_uint, ctypes.c_void_p, ctypes.c_int]),
    ("SetWindowPos", wt.BOOL, [wt.HWND, wt.HWND, ctypes.c_int, ctypes.c_int,
                               ctypes.c_int, ctypes.c_int, ctypes.c_uint]),
    ("EnumWindows", wt.BOOL, [ctypes.c_void_p, ctypes.c_void_p]),
    ("GetClassNameW", ctypes.c_int, [wt.HWND, ctypes.c_wchar_p, ctypes.c_int]),
    ("IsWindowVisible", wt.BOOL, [wt.HWND]),
    ("GetWindowTextW", ctypes.c_int, [wt.HWND, ctypes.c_wchar_p, ctypes.c_int]),
):
    getattr(u, f).restype = restype
    if argtypes is not None:
        getattr(u, f).argtypes = argtypes
u.PostMessageW.argtypes = [wt.HWND, ctypes.c_uint, ctypes.c_void_p, ctypes.c_void_p]
u.PostMessageW.restype = wt.BOOL
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
BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "ZenR16")
if os.path.isdir(BASE):
    for _root, _dirs, _files in os.walk(BASE):
        for _n in _dirs + _files:
            try:
                os.chmod(os.path.join(_root, _n), 0o666)
            except OSError:
                pass
    shutil.rmtree(BASE, ignore_errors=True)
os.makedirs(os.path.join(BASE, "AAA_dir"), exist_ok=True)
for i in range(3):
    open(os.path.join(BASE, f"f{i}.txt"), "w").write("x" * (i * 40 + 1))

INPUT_MOUSE = 0
MOUSEEVENTF_MOVE = 0x0001
MOUSEEVENTF_LEFTDOWN = 0x0002
MOUSEEVENTF_LEFTUP = 0x0004
MOUSEEVENTF_ABSOLUTE = 0x8000
WM_MOUSEMOVE = 0x0200
WM_LBUTTONDOWN = 0x0201
WM_LBUTTONUP = 0x0202
WM_LBUTTONDBLCLK = 0x0203
WM_RBUTTONDOWN = 0x0204
WM_RBUTTONUP = 0x0205

TITLE_H, TOOL_H, HEADER_H, ROW_H = 34, 40, 26, 21
SIDEBAR_W = 157
SIDE_ROW_H = 34
SIDE_TOP = TITLE_H + TOOL_H + SIDE_ROW_H      # 第 0 行的顶边

WINDOW_BG = (0x20, 0x20, 0x20)
ROW_HOVER = (0x2E, 0x2E, 0x2E)       # 悬停：比选中弱一档
HOVER_BG = (0x3D, 0x3D, 0x3D)        # 焦点行
SELECTION = (0x47, 0x47, 0x47)       # 选中
SURFACE = (0x2B, 0x2B, 0x2B)
BORDER = (0x53, 0x53, 0x53)
CHROME = (0x19, 0x19, 0x19)
ACCENT = (0x43, 0x75, 0xCE)

ok_all = True


def mark(ok, msg):
    global ok_all
    ok_all = ok_all and ok
    print(f"    {'PASS' if ok else 'FAIL'}  {msg}")


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [("dx", ctypes.c_long), ("dy", ctypes.c_long), ("mouseData", ctypes.c_uint),
                ("dwFlags", ctypes.c_uint), ("time", ctypes.c_uint),
                ("dwExtraInfo", ctypes.POINTER(ctypes.c_ulong))]


class INPUT(ctypes.Structure):
    class _I(ctypes.Union):
        _fields_ = [("mi", MOUSEINPUT)]
    _anonymous_ = ("i",)
    _fields_ = [("type", ctypes.c_uint), ("i", _I)]


def _send(items):
    arr = (INPUT * len(items))(*items)
    u.SendInput(len(items), ctypes.byref(arr), ctypes.sizeof(INPUT))


def launch(arg=None, w=1180, h=780):
    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    time.sleep(0.45)
    subprocess.Popen([EXE] + ([arg] if arg else []))
    hw = None
    for _ in range(60):
        time.sleep(0.25)
        hw = u.FindWindowW("ZenFileMainWindow", None)
        if hw:
            break
    if not hw:
        raise SystemExit("主窗口没起来")
    u.SetWindowPos(hw, None, 60, 60, w, h, 0x0004)
    time.sleep(2.2)
    u.SetForegroundWindow(hw)
    time.sleep(0.8)
    return hw


def rect(hw):
    r = wt.RECT()
    u.GetWindowRect(hw, ctypes.byref(r))
    return r


def grab(hwnd, tries=8):
    raw = None
    stride = w = h = 0
    for _ in range(tries):
        r = rect(hwnd)
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
            break
        time.sleep(0.35)
    return raw, stride, w, h


def px(raw, stride, x, y):
    o = y * stride + x * 4
    return (raw[o + 2], raw[o + 1], raw[o])


def near(p, q, tol=14):
    return all(abs(a - b) <= tol for a, b in zip(p, q))


def count(raw, stride, x0, x1, y0, y1, color, tol=14):
    return sum(1 for y in range(max(y0, 0), max(y1, 0)) for x in range(max(x0, 0), max(x1, 0))
               if near(px(raw, stride, x, y), color, tol))


def ink_rows(raw, stride, x0, x1, y0, y1, thr=0xB0, merge=0):
    """在给定竖直条带里找"有内容的行"：返回 [(top, bottom), ...]。

    `merge` 把间隔小于它的两段并成一段：汉字笔画、图标内部都有空隙，
    不合并的话一行会被切成好几段（实测侧栏 14 行能量出 38 段）。
    """
    bands = []
    run = None
    for y in range(y0, y1):
        has = any(min(px(raw, stride, x, y)) >= thr for x in range(x0, x1))
        if has and run is None:
            run = y
        elif not has and run is not None:
            bands.append((run, y))
            run = None
    if run is not None:
        bands.append((run, y1))
    if merge <= 0:
        return bands
    out = []
    for (t, b) in bands:
        if out and t - out[-1][1] < merge:
            out[-1] = (out[-1][0], b)
        else:
            out.append((t, b))
    return out


def leftmost_ink(raw, stride, x0, x1, y0, y1, thr=0xB0):
    for x in range(x0, x1):
        for y in range(y0, y1):
            if min(px(raw, stride, x, y)) >= thr:
                return x
    return -1


def dominant(raw, stride, x0, x1, y0, y1):
    c = Counter(px(raw, stride, x, y) for y in range(y0, y1) for x in range(x0, x1))
    return c.most_common(1)[0]


ENUMCB = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, ctypes.c_void_p)


def windows_of(cls):
    out = []

    def cb(h, _l):
        if u.IsWindowVisible(h):
            b = ctypes.create_unicode_buffer(64)
            u.GetClassNameW(h, b, 64)
            if b.value == cls:
                r = rect(h)
                out.append((h, r.left, r.top, r.right - r.left, r.bottom - r.top))
        return True
    u.EnumWindows(ENUMCB(cb), 0)
    return out


def sidebar_right(raw, stride, ymid):
    """实测侧栏右沿（那条竖直分隔线），别写死 157 —— 宽度可以拖。"""
    for x in range(200, 60, -1):
        if near(px(raw, stride, x, ymid), BORDER, 10) and near(px(raw, stride, x, ymid - 8), BORDER, 10):
            return x
    return SIDEBAR_W


DIM = 0x70   # 次要文字 #8F8F8F 算墨迹；树线 #535353、底色 #191919 都不算


def side_rows(raw, stride, SR, view_bottom):
    """侧栏里所有"有内容"的行：返回 [(row_index, top, bottom)]。

    行号由 y 反推（每行 34px、起点固定），深度另按箭头所在的 x 判定 ——
    不能靠文字左沿：图标本身就占了一大块 x，左沿会被图标带偏。
    """
    out = []
    seen = set()
    for (t, b) in ink_rows(raw, stride, 50, SR - 6, SIDE_TOP, view_bottom, DIM, merge=8):
        i = (t - SIDE_TOP) // SIDE_ROW_H
        if i in seen:
            continue
        seen.add(i)
        out.append((i, SIDE_TOP + SIDE_ROW_H * i, SIDE_TOP + SIDE_ROW_H * (i + 1)))
    return out


def has_ink(raw, stride, top, bottom, x0, x1, thr=DIM):
    return any(min(px(raw, stride, x, y)) >= thr
               for y in range(top, bottom) for x in range(x0, x1))


def side_depth(raw, stride, top, bottom):
    """按**图标**所在的列判深度，不能按文字左沿（图标自己就占一大块 x）。

    图标 x：depth0 = 22..42，depth1 = 38..58，depth2 = 54..74。
    于是 60..68 只可能是 depth2 的图标，44..52 只可能是 depth1 的图标
    （depth2 那一格的箭头在 36..52，但 depth2 已被前一条判走）。
    """
    if has_ink(raw, stride, top, bottom, 60, 68):
        return 2
    if has_ink(raw, stride, top, bottom, 44, 52):
        return 1
    return 0


def menu_profile(mh, mw, mhh):
    """菜单的 (文字行, 每行右侧的下级菜单箭头墨迹数)。"""
    raw, st, _, _ = grab(mh)
    # 阈值取 0x80：**禁用态**的文字是 #8F8F8F，用 0xB0 会把它整行漏掉
    # （背景菜单里"粘贴"通常是灰的）—— 菜单底色 #2B2B2B 远低于它，不会误判。
    rows = [b for b in ink_rows(raw, st, 24, mw - 26, 2, mhh - 2, 0x80, merge=8)
            if b[1] - b[0] >= 6]
    arrows = []
    for (t, b) in rows:
        arrows.append(sum(1 for y in range(t, b) for x in range(mw - 22, mw - 6)
                          if min(px(raw, st, x, y)) >= 0x90))
    return rows, arrows


def click(hw, x, y, dbl=False):
    msg = WM_LBUTTONDBLCLK if dbl else WM_LBUTTONDOWN
    u.PostMessageW(hw, msg, 0, (y << 16) | (x & 0xFFFF))
    if not dbl:
        u.PostMessageW(hw, WM_LBUTTONUP, 0, (y << 16) | (x & 0xFFFF))
    time.sleep(0.9)


def rclick(hw, x, y):
    u.PostMessageW(hw, WM_RBUTTONDOWN, 0, (y << 16) | (x & 0xFFFF))
    u.PostMessageW(hw, WM_RBUTTONUP, 0, (y << 16) | (x & 0xFFFF))
    time.sleep(0.9)


def rclick_menu(hw, x, y, tries=4):
    """右键并等到菜单窗口出现。菜单偶发不弹（PostMessage 时序），重来一次就好。"""
    for _ in range(tries):
        rclick(hw, x, y)
        ms = windows_of("ZenFileMenu")
        if ms:
            return ms
        time.sleep(0.6)
    return []


print("=" * 74)
print("第 16 轮回归（5 项）")

# ====================================================== 1) 侧栏真树线
print("\n[1] 侧栏树线：竖线按兄弟关系收口 + 肘形连接线")
hw = launch(BASE)
raw, stride, W, H = grab(hw)
ymid = TITLE_H + TOOL_H + 200
SR = sidebar_right(raw, stride, ymid)
print(f"  实测侧栏右沿 x={SR}")
view_bottom = H - 23
rows = side_rows(raw, stride, SR, view_bottom)
d1 = [(i, t, b) for (i, t, b) in rows if side_depth(raw, stride, t, b) == 1]
print(f"  侧栏行 {len(rows)} 条，其中 depth1（盘符）{len(d1)} 条：{[x[0] for x in d1]}")
mark(len(d1) >= 2, f"侧栏里认出了盘符层级（{len(d1)} 条 ≥ 2）")

# 肘形连接线：depth1 的父级竖线在 x=12，横线从 x=12 拉到 x=22
elbows = []
for (i, t, b) in d1:
    cy = (t + b) // 2
    elbows.append(count(raw, stride, 12, 23, cy - 1, cy + 2, BORDER, 12))
print(f"  各行肘形横线像素 {elbows}")
mark(len(elbows) >= 2 and all(e >= 6 for e in elbows),
     f"每行都有一条从父级伸出来的肘形横线（最少 {min(elbows) if elbows else 0}px）")

# 竖线：非末位盘符贯穿整行，末位盘符只画到中线（画成 └）
if len(d1) >= 2:
    _, ftop, fbot = d1[0]
    _, ltop, lbot = d1[-1]
    fcy, lcy = (ftop + fbot) // 2, (ltop + lbot) // 2
    full = count(raw, stride, 12, 14, ftop, fbot, BORDER, 12)
    up = count(raw, stride, 12, 14, ltop, lcy - 1, BORDER, 12)
    down = count(raw, stride, 12, 14, lcy + 3, lbot, BORDER, 12)
    print(f"  首位盘符竖线 {full}px（整行 {SIDE_ROW_H}）；末位盘符上半 {up}px / 下半 {down}px")
    mark(full >= SIDE_ROW_H - 8, f"非末位节点的竖线贯穿整行（{full}px）")
    mark(up >= 10 and down <= 2, f"末位节点的竖线在中线收口（上半 {up}px、下半 {down}px）")

# ====================================================== 5) 双击才展开
print("\n[5] 侧栏：单击只导航，双击才展开")
before = len(rows)
if d1:
    _, t, b = d1[0]
    cy = (t + b) // 2
    click(hw, 60, cy)
    raw, stride, W, H = grab(hw)
    after1 = side_rows(raw, stride, SR, view_bottom)
    print(f"  单击前 {before} 行 → 单击后 {len(after1)} 行")
    mark(len(after1) == before, f"单击没有把树撑开（{before} → {len(after1)}）")
    click(hw, 60, cy, dbl=True)
    raw, stride, W, H = grab(hw)
    rows2 = side_rows(raw, stride, SR, view_bottom)
    deep = [r for r in rows2 if side_depth(raw, stride, r[1], r[2]) >= 2]
    print(f"  双击后 {len(rows2)} 行，其中 depth≥2 的行 {len(deep)} 条")
    mark(len(deep) >= 1, f"双击展开了下一层（depth≥2 的行 {len(deep)} 条）")
    if deep:
        # depth2 那一层的竖线在 x=28 —— 证明更深的层级也走同一套树线
        v = count(raw, stride, 28, 30, SIDE_TOP, view_bottom, BORDER, 12)
        print(f"  depth2 层竖线（x=28）像素 {v}")
        mark(v >= 20, f"更深层级的树线同样画出来了（{v}px）")

# ====================================================== 3) 右键菜单自适应
print("\n[3] 右键菜单按对象自适应（文件上不要 查看/排序方式/刷新）")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.5)
hw = launch(BASE)
# 行序：目录在前，所以第 1 行是第一个文件
ms = rclick_menu(hw, SIDEBAR_W + 60, TITLE_H + TOOL_H + HEADER_H + ROW_H + 10)
file_rows = file_sub = 0
if ms:
    mh, mx, my, mw, mhh = ms[0]
    r, a = menu_profile(mh, mw, mhh)
    file_rows = len(r)
    # 自有那一段固定是前 9 行；Shell 追加的项排在后面，不该算进来
    file_sub = sum(1 for v in a[:9] if v >= 3)
print(f"  文件上右键：菜单共 {file_rows} 行，自有段落里带下拉的 {file_sub} 行")
mark(file_sub == 0, f"文件右键菜单里没有 查看/排序方式/新建（{file_sub} 行带下拉，应为 0）")
mark(file_rows >= 9, f"菜单还有内容（{file_rows} 行）")

subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.5)
hw = launch(BASE)
ms = rclick_menu(hw, SIDEBAR_W + 60, TITLE_H + TOOL_H + HEADER_H + ROW_H * 8)
bg_rows = bg_sub = 0
if ms:
    mh, mx, my, mw, mhh = ms[0]
    r, a = menu_profile(mh, mw, mhh)
    bg_rows = len(r)
    bg_sub = sum(1 for v in a[:9] if v >= 3)
print(f"  空白处右键：菜单共 {bg_rows} 行，带下拉的 {bg_sub} 行")
mark(bg_sub >= 3, f"空白处右键仍然有 新建/查看/排序方式（{bg_sub} 行 ≥ 3）")
mark(bg_rows == 9, f"背景菜单是 8 项 + 显示更多选项（{bg_rows}）")

# ====================================================== 2) 图标按钮
print("\n[2] 「批量重命名」是目录栏最右侧的图标按钮，且不挤占地址栏")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.5)
hw = launch(BASE)
raw, stride, W, H = grab(hw)
tby = (TITLE_H + 6 + TITLE_H + TOOL_H - 6) // 2
# 量按钮**边框**的两条竖线，而不是量底色：底色被居中的字形切断，
# 从左沿扫过去会在字形处停住（实测只量到 13px）。
cols = [x for x in range(W - 70, W - 4) if near(px(raw, stride, x, tby), BORDER, 10)]
btn_w = (cols[-1] - cols[-2] + 1) if len(cols) >= 2 else 0
print(f"  按钮边框列 {cols[-3:]}，宽 {btn_w}px")
mark(20 <= btn_w <= 42, f"按钮是图标尺寸，不是 132px 的文字按钮（{btn_w}px）")
# 地址栏右沿：找那条 border 竖线，期望 ≈ W-222（旧的文字按钮版本是 W-324）
addr_r = None
for x in range(W - 200, W - 300, -1):
    if near(px(raw, stride, x, tby), BORDER, 10):
        addr_r = x
        break
print(f"  地址栏右沿 x={addr_r}（期望 ≈ {W - 223}）")
mark(addr_r is not None and abs(addr_r - (W - 223)) <= 16,
     f"地址栏拿回了被文字按钮占掉的宽度（{addr_r} ≈ {W - 223}）")
# 字形墨迹：必须扫**整块**按钮，只扫右半边会漏掉居中偏左的图标
btn_ink = sum(1 for y in range(TITLE_H + 8, TITLE_H + TOOL_H - 8)
              for x in range(W - 38, W - 7) if min(px(raw, stride, x, y)) >= 0x80)
print(f"  按钮区墨迹 {btn_ink}px")
mark(btn_ink >= 20, f"按钮上画出了图标（{btn_ink}px）")
click(hw, W - 21, tby)
ms = windows_of("ZenFileMenu")
if ms:
    mh, mx, my, mw, mhh = ms[0]
    raw, st, _, _ = grab(mh)
    rows = len(ink_rows(raw, st, 24, mw - 26, 2, mhh - 2, 0x80, merge=8))
    print(f"  点按钮弹出下拉，{rows} 行")
    mark(rows == 5, f"下拉里是 5 种重命名模式（{rows}）")
else:
    mark(False, "点按钮没有弹出下拉")

# 「置顶显示」：主页/回收站这类虚拟视图里按钮照样在那儿（只是淡化、点不动），
# 不能像上一版那样整块消失 —— 一个时有时无的按钮看着像丢了。
hw = launch()
raw, stride, W, H = grab(hw)
cols2 = [x for x in range(W - 70, W - 4) if near(px(raw, stride, x, tby), BORDER, 10)]
w2 = (cols2[-1] - cols2[-2] + 1) if len(cols2) >= 2 else 0
print(f"  主页（虚拟视图）里按钮边框列 {cols2[-2:]}，宽 {w2}px")
mark(20 <= w2 <= 42, f"主页里按钮依然常驻，不是时有时无（{w2}px）")

# ====================================================== 4) 悬停反馈
print("\n[4] 条目悬停：有底色，但比选中弱一档")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.5)
hw = launch(BASE)
y1 = TITLE_H + TOOL_H + HEADER_H + ROW_H + 10
u.PostMessageW(hw, WM_MOUSEMOVE, 0, (y1 << 16) | (SIDEBAR_W + 300))
time.sleep(0.6)
raw, stride, W, H = grab(hw)
hcol, hn = dominant(raw, stride, SIDEBAR_W + 200, SIDEBAR_W + 380, y1 - 6, y1 + 6)
hbar = count(raw, stride, SIDEBAR_W, SIDEBAR_W + 4, y1 - 8, y1 + 8, ACCENT, 30)
print(f"  悬停行底色 {hcol}（{hn} 票），左沿强调色 {hbar}px")
mark(near(hcol, ROW_HOVER, 8), f"悬停行铺了一层 {ROW_HOVER} 的底色（量到 {hcol}）")
mark(hbar == 0, f"悬停**不**画选中那条强调竖线（{hbar}px，应为 0）")

click(hw, SIDEBAR_W + 300, y1)
raw, stride, W, H = grab(hw)
scol, sn = dominant(raw, stride, SIDEBAR_W + 200, SIDEBAR_W + 380, y1 - 6, y1 + 6)
sbar = count(raw, stride, SIDEBAR_W, SIDEBAR_W + 4, y1 - 8, y1 + 8, ACCENT, 30)
print(f"  选中行底色 {scol}（{sn} 票），左沿强调色 {sbar}px")
mark(near(scol, SELECTION, 8), f"选中行是 {SELECTION}（量到 {scol}）")
mark(sbar >= ROW_H, f"选中行有强调竖线（{sbar}px ≥ 行高 {ROW_H}）")
mark(sum(scol) > sum(hcol), f"选中明显强于悬停（{sum(scol)} > {sum(hcol)}）")

subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
print("\n" + ("第 16 轮全部通过" if ok_all else "第 16 轮有失败项"))
