"""用**真实鼠标输入**（SendInput）验证标题栏交互与拖拽。

## 为什么必须单独有这么一份脚本
其余 verify_*.py 一律用 `PostMessageW` 合成 WM_LBUTTONDOWN / WM_MOUSEMOVE。
合成消息会**整条跳过 WM_NCHITTEST**，而真实鼠标的第一件事就是问
`WM_NCHITTEST` —— 命中码决定了这条按下到底是走 `WM_LBUTTONDOWN`（客户区）
还是 `WM_NCLBUTTONDOWN`（非客户区）。于是有一整类缺陷"脚本全绿、真人一点就废"：

  1) 打开设置后点标题栏的标签没反应（快捷键却有效）
  2) 标题栏拖不动窗口；双击最大化之后再也缩不回去
  3) 搜索框点不动
  4) 文件拖拽 / 空白区拖框，鼠标操作完全没效果

这份脚本一律走 SendInput，和真人操作走同一条消息路径。
"""
import ctypes
import ctypes.wintypes as wt
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
    ("SendInput", ctypes.c_uint, [ctypes.c_uint, ctypes.c_void_p, ctypes.c_int]),
    ("GetGUIThreadInfo", wt.BOOL, [ctypes.c_uint, ctypes.c_void_p]),
    ("GetWindowThreadProcessId", ctypes.c_uint, [wt.HWND, ctypes.POINTER(ctypes.c_uint)]),
    ("GetDlgItem", wt.HWND, [wt.HWND, ctypes.c_int]),
    ("GetClassNameW", ctypes.c_int, [wt.HWND, ctypes.c_wchar_p, ctypes.c_int]),
    ("GetWindowLongW", ctypes.c_long, [wt.HWND, ctypes.c_int]),
    ("SetWindowPos", wt.BOOL, [wt.HWND, wt.HWND, ctypes.c_int, ctypes.c_int,
                               ctypes.c_int, ctypes.c_int, ctypes.c_uint]),
    ("GetCursorPos", wt.BOOL, [ctypes.POINTER(wt.POINT)]),
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
BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "ZenReal")
if os.path.isdir(BASE):
    for _root, _dirs, _files in os.walk(BASE):
        for _n in _dirs + _files:
            try:
                os.chmod(os.path.join(_root, _n), 0o666)
            except OSError:
                pass
    shutil.rmtree(BASE, ignore_errors=True)
os.makedirs(BASE, exist_ok=True)
for i in range(4):
    open(os.path.join(BASE, f"file{i}.txt"), "w").write("x" * (i * 50 + 1))
os.makedirs(os.path.join(BASE, "subdir"), exist_ok=True)

INPUT_MOUSE = 0
MOUSEEVENTF_MOVE = 0x0001
MOUSEEVENTF_LEFTDOWN = 0x0002
MOUSEEVENTF_LEFTUP = 0x0004
MOUSEEVENTF_ABSOLUTE = 0x8000
GWL_STYLE = -16
WS_MAXIMIZE = 0x01000000
TITLE_H, TOOL_H, HEADER_H, ROW_H = 34, 40, 26, 21

ok_all = True


def mark(ok, msg):
    global ok_all
    ok_all = ok_all and ok
    print(f"    {'PASS' if ok else 'FAIL'}  {msg}")


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [("dx", ctypes.c_long), ("dy", ctypes.c_long), ("mouseData", ctypes.c_uint),
                ("dwFlags", ctypes.c_uint), ("time", ctypes.c_uint),
                ("dwExtraInfo", ctypes.POINTER(ctypes.c_ulong))]


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [("wVk", ctypes.c_ushort), ("wScan", ctypes.c_ushort),
                ("dwFlags", ctypes.c_uint), ("time", ctypes.c_uint),
                ("dwExtraInfo", ctypes.POINTER(ctypes.c_ulong))]


class INPUT(ctypes.Structure):
    class _I(ctypes.Union):
        _fields_ = [("mi", MOUSEINPUT), ("ki", KEYBDINPUT)]
    _anonymous_ = ("i",)
    _fields_ = [("type", ctypes.c_uint), ("i", _I)]


def _send(items):
    arr = (INPUT * len(items))(*items)
    u.SendInput(len(items), ctypes.byref(arr), ctypes.sizeof(INPUT))


def mouse_move(sx, sy):
    it = INPUT()
    it.type = INPUT_MOUSE
    it.mi.dx = int(sx * 65535 / max(ctypes.windll.user32.GetSystemMetrics(0) - 1, 1))
    it.mi.dy = int(sy * 65535 / max(ctypes.windll.user32.GetSystemMetrics(1) - 1, 1))
    it.mi.dwFlags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE
    _send([it])
    time.sleep(0.05)


def left_down():
    it = INPUT()
    it.type = INPUT_MOUSE
    it.mi.dwFlags = MOUSEEVENTF_LEFTDOWN
    _send([it])
    time.sleep(0.08)


def left_up():
    it = INPUT()
    it.type = INPUT_MOUSE
    it.mi.dwFlags = MOUSEEVENTF_LEFTUP
    _send([it])
    time.sleep(0.08)


def drag(sx0, sy0, sx1, sy1, steps=12, hold=0.35):
    mouse_move(sx0, sy0)
    left_down()
    for i in range(1, steps + 1):
        mouse_move(sx0 + (sx1 - sx0) * i // steps, sy0 + (sy1 - sy0) * i // steps)
    time.sleep(hold)
    return True


def release(sx, sy):
    mouse_move(sx, sy)
    left_up()
    time.sleep(0.5)


def click(sx, sy):
    mouse_move(sx, sy)
    left_down()
    left_up()


def launch(arg=None, w=1180, h=760):
    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    time.sleep(0.5)
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
    time.sleep(1.0)
    return hw


def rect(hw):
    r = wt.RECT()
    u.GetWindowRect(hw, ctypes.byref(r))
    return r


def grab(hwnd, tries=8):
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
            return raw, stride, w, h
        time.sleep(0.35)
    return raw, stride, w, h


def px(raw, stride, x, y):
    o = y * stride + x * 4
    return (raw[o + 2], raw[o + 1], raw[o])


def near(p, q, tol=14):
    return all(abs(a - b) <= tol for a, b in zip(p, q))


def screen_of(hw, x, y):
    r = rect(hw)
    return (r.left + x, r.top + y)


class GUITHREADINFO(ctypes.Structure):
    _fields_ = [("cbSize", ctypes.c_uint), ("flags", ctypes.c_uint),
                ("hwndActive", wt.HWND), ("hwndFocus", wt.HWND), ("hwndCapture", wt.HWND),
                ("hwndMenuOwner", wt.HWND), ("hwndMoveSize", wt.HWND), ("hwndCaret", wt.HWND),
                ("rcCaret", wt.RECT)]


def focused_hwnd(hw):
    """被测进程里真正拿到焦点的那个窗口。

    `GetFocus()` 是**线程局部**的：脚本自己这条线程没有焦点窗口，跨进程
    调它永远返回 NULL，拿它来判断"点中了没有"必然假阴性。
    """
    pid = ctypes.c_uint()
    tid = u.GetWindowThreadProcessId(hw, ctypes.byref(pid))
    gti = GUITHREADINFO()
    gti.cbSize = ctypes.sizeof(GUITHREADINFO)
    if u.GetGUIThreadInfo(tid, ctypes.byref(gti)):
        return gti.hwndFocus
    return None


print("=" * 74)
print("真实鼠标输入回归（SendInput，不合成消息）")

# ================================================== 2) 标题栏拖窗口 + 双击最大化
print("\n[2] 标题栏要能拖窗口，双击能最大化也能还原")
hw = launch(BASE)
r0 = rect(hw)
# 标题栏空白处（标签栏右端与「更多」之间，避开所有自绘控件）
sx, sy = screen_of(hw, 700, 17)
drag(sx, sy, sx + 90, sy + 60)
r_mid = rect(hw)
moved = (r_mid.left - r0.left, r_mid.top - r0.top)
release(sx + 90, sy + 60)
r1 = rect(hw)
print(f"  拖动前 ({r0.left},{r0.top}) → 拖动后 ({r1.left},{r1.top})；拖动中位移 {moved}")
mark(abs(r1.left - r0.left) >= 40 or abs(r1.top - r0.top) >= 20,
     f"按住标题栏空白处能把窗口拖走（位移 {r1.left - r0.left},{r1.top - r0.top}）")

# 双击最大化 / 再双击还原
sx, sy = screen_of(hw, 700, 17)
click(sx, sy)
click(sx, sy)
time.sleep(1.0)
st = u.GetWindowLongW(hw, GWL_STYLE)
maxed = bool(st & WS_MAXIMIZE)
print(f"  双击后 WS_MAXIMIZE = {maxed}")
mark(maxed, "双击标题栏空白处能最大化")
rmax = rect(hw)
print(f"  最大化后窗口 {rmax.right - rmax.left}x{rmax.bottom - rmax.top}")
# 坐标必须**重新取**：最大化之后标题栏跑到屏幕顶端了，还用最大化之前算出来的
# 屏幕坐标去点，点的是桌面空白处，当然还原不了（这是脚本的锅不是产品的锅）。
sx, sy = (rmax.left + rmax.right) // 2 - 300, rmax.top + 17
click(sx, sy)
click(sx, sy)
time.sleep(1.0)
st2 = u.GetWindowLongW(hw, GWL_STYLE)
r_back = rect(hw)
print(f"  再双击后 WS_MAXIMIZE = {bool(st2 & WS_MAXIMIZE)}，窗口 {r_back.right - r_back.left}x{r_back.bottom - r_back.top}")
mark(not (st2 & WS_MAXIMIZE), "再双击能还原（不会卡在最大化）")

# ================================================== 1) 设置页里点标题栏的标签
print("\n[1] 打开设置后，标题栏上的标签依然要能点")
hw = launch("shell:Settings")
u.SetForegroundWindow(hw)
time.sleep(0.5)
raw, stride, W, H = grab(hw)


def active_tab_seg(raw, stride, W):
    """当前标签底色 tab_active=#2B2B2B → 量出它的左右边界。"""
    segs, run, start = [], 0, None
    for x in range(40, W - 176):
        if near(px(raw, stride, x, 17), (0x2B, 0x2B, 0x2B), 16):
            if run == 0:
                start = x
            run += 1
        else:
            if run > 30:
                segs.append((start, x))
            run = 0
    if run > 30:
        segs.append((start, W - 176))
    return segs


one = active_tab_seg(raw, stride, W)
print(f"  设置页打开时，活动标签段 {one}")
# Ctrl+T 再开一个标签，然后**用真实鼠标**点回第 0 个标签
u.keybd_event(0x11, 0, 0, 0)
u.keybd_event(0x54, 0, 0, 0)
time.sleep(0.1)
u.keybd_event(0x54, 0, 2, 0)
u.keybd_event(0x11, 0, 2, 0)
time.sleep(1.0)
raw, stride, W, H = grab(hw)
two = active_tab_seg(raw, stride, W)
print(f"  Ctrl+T 之后活动标签段 {two}")
sx, sy = screen_of(hw, 40 + 180 * 0 + 90, 17)
click(sx, sy)
time.sleep(0.9)
raw, stride, W, H = grab(hw)
back = active_tab_seg(raw, stride, W)
print(f"  真实鼠标点第 0 个标签之后 {back}")
mark(back and one and abs(back[0][0] - one[0][0]) <= 6,
     "设置页里用鼠标也能切标签")
# 点第 1 个标签的 ×（真实鼠标）
sx, sy = screen_of(hw, 40 + 180 * 1 + 180 - 11, 17)
click(sx, sy)
time.sleep(0.9)
raw, stride, W, H = grab(hw)
after = active_tab_seg(raw, stride, W)
print(f"  真实鼠标点第 1 个标签的 × 之后 {after}")
mark(len(after) == 1, "设置页里用鼠标也能关标签")

# 再验一遍更大窗口下的同一条路径（几何不同，别只在一个尺寸下绿）
hw = launch("shell:Settings", w=1400, h=880)
u.keybd_event(0x11, 0, 0, 0)
u.keybd_event(0x54, 0, 0, 0)
time.sleep(0.1)
u.keybd_event(0x54, 0, 2, 0)
u.keybd_event(0x11, 0, 2, 0)
time.sleep(1.0)
raw, stride, W, H = grab(hw)
two = active_tab_seg(raw, stride, W)
sx, sy = screen_of(hw, 40 + 90, 17)
click(sx, sy)
time.sleep(0.9)
raw, stride, W, H = grab(hw)
back2 = active_tab_seg(raw, stride, W)
print(f"  大窗口下：Ctrl+T 后 {two}，鼠标点第 0 个标签后 {back2}")
mark(back2 and two and abs(back2[0][0] - two[0][0]) > 100,
     "设置页 + 大窗口下，鼠标点标签依然能切")

# 最大化下再来一遍：用户很可能一直开着最大化，而命中测试走的是同一套几何，
# 最大化不该把它改坏。
u.ShowWindow.restype = wt.BOOL
u.ShowWindow.argtypes = [wt.HWND, ctypes.c_int]
hw = launch("shell:Settings")
u.keybd_event(0x11, 0, 0, 0)
u.keybd_event(0x54, 0, 0, 0)
time.sleep(0.1)
u.keybd_event(0x54, 0, 2, 0)
u.keybd_event(0x11, 0, 2, 0)
time.sleep(0.9)
u.ShowWindow(hw, 3)  # SW_MAXIMIZE
time.sleep(1.2)
u.SetForegroundWindow(hw)
time.sleep(0.6)
raw, stride, W, H = grab(hw)
mx_seg = active_tab_seg(raw, stride, W)
sx, sy = screen_of(hw, 40 + 90, 17)
click(sx, sy)
time.sleep(0.9)
raw, stride, W, H = grab(hw)
mx_after = active_tab_seg(raw, stride, W)
print(f"  最大化下：{mx_seg} → 点第 0 个标签后 {mx_after}")
mark(mx_after and mx_seg and abs(mx_after[0][0] - mx_seg[0][0]) > 100,
     "设置页 + 最大化下，鼠标点标签依然能切")

# ================================================== 3) 搜索框
print("\n[3] 搜索框要能点、要有放大镜和 placeholder")
hw = launch(BASE)
u.SetForegroundWindow(hw)
time.sleep(0.5)
srch = u.GetDlgItem(hw, 102)
sr = wt.RECT()
u.GetWindowRect(srch, ctypes.byref(sr))
mr = rect(hw)
bx0, by0 = sr.left - mr.left, sr.top - mr.top
bx1, by1 = sr.right - mr.left, sr.bottom - mr.top


def search_ink(raw, stride):
    """放大镜（左端 40px）与 placeholder（其右侧）各自"偏离底色"的像素数。

    不能用"够不够亮"当判据：放大镜是 14px 的小字形，抗锯齿之后大部分像素
    是半亮的中间色，纯亮像素只有十几个，阈值一高就被判成"没画"。
    """
    bg = (0x2B, 0x2B, 0x2B)

    def diff(x0, x1):
        return sum(1 for y in range(by0 + 2, by1 - 2) for x in range(x0, x1)
                   if not near(px(raw, stride, x, y), bg, 6))

    return diff(bx0 + 2, bx0 + 42), diff(bx0 + 44, bx1 - 10)


raw, stride, W, H = grab(hw)
ic0, ph0 = search_ink(raw, stride)
print(f"  搜索框客户区 ({bx0},{by0})-({bx1},{by1})；未点击时 放大镜 {ic0}px / placeholder {ph0}px")
mark(ic0 >= 40, f"搜索框左侧有放大镜（{ic0}px 偏离底色）")
mark(ph0 >= 100, f"空态显示 placeholder（{ph0}px）")

# 真正的回归点：**点上去之后装饰还在不在**。没有 WS_CLIPCHILDREN 时，父窗口
# 一重绘就把整块搜索框刷成底色，子 EDIT 刚画好的放大镜和提示字全被盖掉 ——
# 不点看不出来（子控件自己画过一次），一点就只剩一块空板。
sx, sy = screen_of(hw, (bx0 + bx1) // 2, (by0 + by1) // 2)
click(sx, sy)
time.sleep(0.6)
foc = focused_hwnd(hw)
print(f"  真实点击后焦点窗口 == 搜索框? {foc == srch}")
mark(foc == srch, "鼠标点一下搜索框就能聚焦")
raw, stride, W, H = grab(hw)
ic1, ph1 = search_ink(raw, stride)
print(f"  点击之后 放大镜 {ic1}px / placeholder {ph1}px（点之前 {ic0}/{ph0}）")
mark(ic1 >= ic0 * 0.6, f"点过之后放大镜还在（{ic0} → {ic1}）")
mark(ph1 >= ph0 * 0.6, f"点过之后 placeholder 还在（{ph0} → {ph1}）")

# ================================================== 4) 拖拽影像 + 空白区拖框
print("\n[4] 鼠标拖文件要有影像；空白处长按要出框")
hw = launch(BASE)
u.SetForegroundWindow(hw)
time.sleep(0.5)
raw0, stride, W, H = grab(hw)
row0 = TITLE_H + TOOL_H + HEADER_H + 10
x_item = 157 + 21 + 60
sx, sy = screen_of(hw, x_item, row0)
drag(sx, sy, sx + 150, sy + 90)
rawd, stride, W, H = grab(hw)
# 影像卡片：光标右下 12px 起
cx, cy = x_item + 150 + 12, row0 + 90 + 12
card = sum(1 for y in range(cy, min(cy + 40, H - 2)) for x in range(cx, min(cx + 120, W - 2))
           if near(px(rawd, stride, x, y), (0x2B, 0x2B, 0x2B), 40))
print(f"  拖拽影像卡片像素 {card}")
mark(card >= 400, f"鼠标拖文件画出了跟手影像（{card}px）")
release(sx + 150, sy + 90)

# 空白区拖框：先量最后一行在哪，起点必须落在真空白
raws, stride, W, H = grab(hw)
ink = TITLE_H + TOOL_H + HEADER_H
for yy in range(TITLE_H + TOOL_H + HEADER_H, H - 40):
    for xx in range(178, W - 40, 3):
        p = px(raws, stride, xx, yy)
        if p[0] >= 0x33 or p[1] >= 0x33 or p[2] >= 0x33:
            ink = yy
by0 = ink + 30
by1 = min(by0 + 80, H - 60)
bx = 157 + 21 + 30
print(f"  列表最后有内容的行 y={ink}；框取在空白 y {by0}..{by1}")
sx, sy = screen_of(hw, bx, by0)
drag(sx, sy, sx + 260, sy + 80)
rawm, stride, W, H = grab(hw)
changed = sum(1 for y in range(by0, by1) for x in range(bx, bx + 260)
              if not near(px(raws, stride, x, y), px(rawm, stride, x, y), 12))
area = 260 * (by1 - by0)
box_bg = px(rawm, stride, bx + 8, by0 + 8)
print(f"  空白区拖框：框内变化 {changed}/{area}，框色 {box_bg}")
mark(changed >= area * 0.9, f"鼠标在空白处拖出了选择框（{changed}/{area}）")
mark((0x20, 0x20, 0x20)[0] < box_bg[0] < (0x43, 0x75, 0xCE)[0],
     f"框是半透明的（{box_bg} 介于底色与强调色之间）")
release(sx + 260, sy + 80)

subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
print("\n" + ("真实输入回归全部通过" if ok_all else "有失败项"))
