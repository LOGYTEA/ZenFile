"""第 15 轮 8 项的像素级回归。

  1) 拖拽文件松手不卡死：搬过去 + 松手后窗口仍然响应
  2) 选中行左沿有一条强调竖线（和右键菜单悬停同一套语言）
  3) 「批量重命名」改到目录栏最右侧按钮，右键菜单里不再有它
  4) 设置页的开关是真控件（开=强调色胶囊），不是"开/关"两个汉字
  5) 剪贴板写入成功：CF_HDROP 真的进剪贴板，且没有"全 0 错误"弹窗
  6) 剪贴板为空时右键「粘贴」是淡化禁用态（文字只用 text_dim，且不参与悬停）
  7) 子菜单里"当前生效的那一项"行首是**橙色**竖线
  8) 属性窗口带 WS_EX_APPWINDOW（任务栏有它）

判定全部走像素 / 窗口事实，不靠肉眼。
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
k = ctypes.WinDLL("kernel32", use_last_error=True)
for f, restype, argtypes in (
    ("FindWindowW", wt.HWND, [ctypes.c_wchar_p, ctypes.c_wchar_p]),
    ("GetWindowRect", wt.BOOL, [wt.HWND, ctypes.POINTER(wt.RECT)]),
    ("GetWindowDC", wt.HDC, [wt.HWND]),
    ("PrintWindow", wt.BOOL, [wt.HWND, wt.HDC, ctypes.c_uint]),
    ("SetForegroundWindow", wt.BOOL, [wt.HWND]),
    ("SendInput", ctypes.c_uint, [ctypes.c_uint, ctypes.c_void_p, ctypes.c_int]),
    ("SetWindowPos", wt.BOOL, [wt.HWND, wt.HWND, ctypes.c_int, ctypes.c_int,
                               ctypes.c_int, ctypes.c_int, ctypes.c_uint]),
    ("GetWindowLongW", ctypes.c_long, [wt.HWND, ctypes.c_int]),
    ("EnumWindows", wt.BOOL, [ctypes.c_void_p, ctypes.c_void_p]),
    ("GetClassNameW", ctypes.c_int, [wt.HWND, ctypes.c_wchar_p, ctypes.c_int]),
    ("IsWindowVisible", wt.BOOL, [wt.HWND]),
    ("GetWindowTextW", ctypes.c_int, [wt.HWND, ctypes.c_wchar_p, ctypes.c_int]),
    ("OpenClipboard", wt.BOOL, [wt.HWND]),
    ("EmptyClipboard", wt.BOOL, []),
    ("CloseClipboard", wt.BOOL, []),
    ("EnumClipboardFormats", ctypes.c_uint, [ctypes.c_uint]),
    ("keybd_event", None, [ctypes.c_ubyte, ctypes.c_ubyte, ctypes.c_uint, ctypes.c_void_p]),
):
    getattr(u, f).restype = restype
    if argtypes is not None:
        getattr(u, f).argtypes = argtypes
u.SendMessageTimeoutW.argtypes = [wt.HWND, ctypes.c_uint, ctypes.c_void_p, ctypes.c_void_p,
                                  ctypes.c_uint, ctypes.c_uint, ctypes.POINTER(ctypes.c_ulong)]
u.SendMessageTimeoutW.restype = ctypes.c_void_p
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
BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "ZenR15")
if os.path.isdir(BASE):
    for _root, _dirs, _files in os.walk(BASE):
        for _n in _dirs + _files:
            try:
                os.chmod(os.path.join(_root, _n), 0o666)
            except OSError:
                pass
    shutil.rmtree(BASE, ignore_errors=True)
os.makedirs(BASE, exist_ok=True)
os.makedirs(os.path.join(BASE, "AAA_target"), exist_ok=True)
for i in range(3):
    open(os.path.join(BASE, f"f{i}.txt"), "w").write("x" * (i * 40 + 1))

INPUT_MOUSE = 0
MOUSEEVENTF_MOVE = 0x0001
MOUSEEVENTF_LEFTDOWN = 0x0002
MOUSEEVENTF_LEFTUP = 0x0004
MOUSEEVENTF_ABSOLUTE = 0x8000
WM_NULL = 0x0000
WM_MOUSEMOVE = 0x0200
WM_LBUTTONDOWN = 0x0201
WM_LBUTTONUP = 0x0202
WM_RBUTTONDOWN = 0x0204
WM_RBUTTONUP = 0x0205
SMTO_ABORTIFHUNG = 0x0002
GWL_EXSTYLE = -20
WS_EX_APPWINDOW = 0x00040000
CF_HDROP = 15

TITLE_H, TOOL_H, HEADER_H, ROW_H = 34, 40, 26, 21
VIEW_LEFT = 157
ACCENT = (0x43, 0x75, 0xCE)      # 0x4375CE
ORANGE = (0xE8, 0xA3, 0x3D)      # 0xE8A33D
TEXT = (0xDE, 0xDE, 0xDE)
TEXT_DIM = (0x8F, 0x8F, 0x8F)
HOVER = (0x3D, 0x3D, 0x3D)
SURFACE = (0x2B, 0x2B, 0x2B)

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


def mmove(sx, sy):
    it = INPUT()
    it.type = INPUT_MOUSE
    it.mi.dx = int(sx * 65535 / max(ctypes.windll.user32.GetSystemMetrics(0) - 1, 1))
    it.mi.dy = int(sy * 65535 / max(ctypes.windll.user32.GetSystemMetrics(1) - 1, 1))
    it.mi.dwFlags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE
    _send([it])
    time.sleep(0.05)


def ldown():
    it = INPUT()
    it.type = INPUT_MOUSE
    it.mi.dwFlags = MOUSEEVENTF_LEFTDOWN
    _send([it])
    time.sleep(0.08)


def lup():
    it = INPUT()
    it.type = INPUT_MOUSE
    it.mi.dwFlags = MOUSEEVENTF_LEFTUP
    _send([it])
    time.sleep(0.10)


def launch(arg=None, w=1180, h=760):
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


def screen_of(hw, x, y):
    r = rect(hw)
    return (r.left + x, r.top + y)


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


def near(p, q, tol=16):
    return all(abs(a - b) <= tol for a, b in zip(p, q))


def count(raw, stride, x0, x1, y0, y1, color, tol=16):
    return sum(1 for y in range(y0, y1) for x in range(x0, x1)
               if near(px(raw, stride, x, y), color, tol))


def bright(raw, stride, x0, x1, y0, y1, thr=0xC0):
    """亮像素数：正文 #DEDEDE 算亮，禁用态 #8F8F8F 不算。"""
    return sum(1 for y in range(y0, y1) for x in range(x0, x1)
               if min(px(raw, stride, x, y)) >= thr)


def alive(hw, ms=1200):
    out = ctypes.c_ulong()
    return bool(u.SendMessageTimeoutW(hw, WM_NULL, 0, 0, SMTO_ABORTIFHUNG, ms,
                                      ctypes.byref(out)))


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


def dialogs():
    """所有可见的 MessageBox（类名 #32770）及其正文。"""
    got = []

    def cb(h, _l):
        if u.IsWindowVisible(h):
            b = ctypes.create_unicode_buffer(64)
            u.GetClassNameW(h, b, 64)
            if b.value == "#32770":
                t = ctypes.create_unicode_buffer(512)
                u.GetWindowTextW(h, t, 512)
                parts = []

                def ccb(ch, _x):
                    bb = ctypes.create_unicode_buffer(1024)
                    u.GetWindowTextW(ch, bb, 1024)
                    if bb.value:
                        parts.append(bb.value)
                    return True
                u.EnumChildWindows.argtypes = [wt.HWND, ctypes.c_void_p, ctypes.c_void_p]
                u.EnumChildWindows(h, ENUMCB(ccb), 0)
                got.append((t.value, " | ".join(parts)))
        return True
    u.EnumWindows(ENUMCB(cb), 0)
    return got


def clip_formats():
    if not u.OpenClipboard(None):
        return []
    fmts = []
    f = 0
    for _ in range(40):
        f = u.EnumClipboardFormats(f)
        if f == 0:
            break
        fmts.append(f)
    u.CloseClipboard()
    return fmts


def clear_clipboard():
    if u.OpenClipboard(None):
        u.EmptyClipboard()
        u.CloseClipboard()


def menu_metrics(hwnd, w, h):
    """悬停第 0 行量出内容顶边 + 行高。"""
    u.PostMessageW(hwnd, WM_MOUSEMOVE, 0, (10 << 16) | (w // 2))
    time.sleep(0.25)
    raw, stride, wm, hm = grab(hwnd)
    band = [y for y in range(0, hm) if near(px(raw, stride, wm // 2, y), HOVER, 10)]
    return ((band[0], band[-1] - band[0] + 1) if band else (5, 28))


def menu_row_center(top0, row_h, idx, seps):
    s = row_h / 28.0
    y = top0
    for i in range(idx):
        y += int(6 * s) if i in seps else row_h
    return y + row_h // 2


def rclick(hw, x, y):
    u.PostMessageW(hw, WM_RBUTTONDOWN, 0, (y << 16) | (x & 0xFFFF))
    u.PostMessageW(hw, WM_RBUTTONUP, 0, (y << 16) | (x & 0xFFFF))
    time.sleep(0.8)


print("=" * 74)
print("第 15 轮回归（8 项）")

# ============================================================ 1) 拖拽不卡死
print("\n[1] 拖拽文件松手：搬过去 + 界面不僵")
if os.path.exists(os.path.join(BASE, "AAA_target", "f1.txt")):
    os.remove(os.path.join(BASE, "AAA_target", "f1.txt"))
if not os.path.exists(os.path.join(BASE, "f1.txt")):
    open(os.path.join(BASE, "f1.txt"), "w").write("x" * 41)
hw = launch(BASE)
sx, sy = screen_of(hw, VIEW_LEFT + 60, TITLE_H + TOOL_H + HEADER_H + ROW_H * 1 + 10)
dx, dy = screen_of(hw, VIEW_LEFT + 60, TITLE_H + TOOL_H + HEADER_H + ROW_H * 0 + 10)
mmove(sx, sy)
ldown()
for i in range(1, 13):
    mmove(sx + (dx - sx) * i // 12, sy + (dy - sy) * i // 12)
time.sleep(0.3)
lup()
dead = None
for i in range(12):
    time.sleep(0.4)
    if not alive(hw, 1000):
        dead = i
        break
# 行序是"目录在前"，所以第 1 行是第一个**文件**，名字随排序而定 —— 只看目标
# 目录里是不是多了一个东西，别写死文件名。
landed = os.listdir(os.path.join(BASE, "AAA_target"))
moved = bool(landed)
mark(dead is None, f"松手后窗口 12 次探活全响应（{'卡死在第 %d 次' % dead if dead is not None else '无卡死'}）")
mark(moved, f"文件真的被搬进了目标文件夹（AAA_target 里现在有 {landed}）")
mark(not dialogs(), f"没有报错弹窗（{dialogs()}）")

# ============================================================ 2) 选中竖线
print("\n[2] 选中行左沿有强调竖线（与菜单悬停同一套语言）")
hw = launch(BASE)
u.PostMessageW(hw, WM_LBUTTONDOWN, 0, ((TITLE_H + TOOL_H + HEADER_H + 10) << 16) | (VIEW_LEFT + 200))
u.PostMessageW(hw, WM_LBUTTONUP, 0, ((TITLE_H + TOOL_H + HEADER_H + 10) << 16) | (VIEW_LEFT + 200))
time.sleep(0.7)
raw, stride, W, H = grab(hw)
y0 = TITLE_H + TOOL_H + HEADER_H
y1 = y0 + ROW_H
bar = count(raw, stride, VIEW_LEFT, VIEW_LEFT + 4, y0 + 1, y1 - 1, ACCENT, 30)
print(f"  第 0 行左沿 4px 内强调色像素 {bar}")
mark(bar >= ROW_H, f"选中行左沿画出了强调竖线（{bar}px ≥ 行高 {ROW_H}）")

# ============================================================ 3) 批量重命名按钮
print("\n[3] 「批量重命名」在目录栏最右侧，右键菜单里已移除")
hw = launch(BASE)
raw, stride, W, H = grab(hw)
# 第 16 轮：按钮从 132px 文字按钮改成 30px **图标**按钮（右沿仍是 W-6）
bx0, bx1 = W - 6 - 30, W - 6
by0, by1 = TITLE_H + 6, TITLE_H + TOOL_H - 6
btn_border = count(raw, stride, bx0, bx1, by0, by1, SURFACE, 12)
btn_text = sum(1 for y in range(by0 + 4, by1 - 4) for x in range(bx0 + 2, bx1 - 2)
               if min(px(raw, stride, x, y)) >= 0x80)
print(f"  按钮区 {bx0}..{bx1}：surface 底色 {btn_border}px，图标墨迹 {btn_text}px")
mark(btn_border >= 300, f"目录栏最右侧画出了按钮底（{btn_border}px）")
mark(btn_text >= 20, f"按钮上画出了图标（{btn_text}px 墨迹）")
u.PostMessageW(hw, WM_LBUTTONDOWN, 0, (((by0 + by1) // 2) << 16) | ((bx0 + 15) & 0xFFFF))
u.PostMessageW(hw, WM_LBUTTONUP, 0, (((by0 + by1) // 2) << 16) | ((bx0 + 15) & 0xFFFF))
time.sleep(0.9)
ms = windows_of("ZenFileMenu")
print(f"  点按钮后菜单窗口 {len(ms)} 个")
mark(len(ms) == 1, "点按钮弹出下拉（只一个菜单窗口）")
if ms:
    mh, mx, my, mw, mhh = ms[0]
    raw2, st2, _, _ = grab(mh)
    bands = []
    run = None
    for y in range(0, mhh):
        ink = sum(1 for x in range(20, mw - 4) if min(px(raw2, st2, x, y)) >= 0xC0)
        if ink > 0 and run is None:
            run = y
        elif ink == 0 and run is not None:
            if y - run >= 6:
                bands.append((run, y))
            run = None
    print(f"  下拉里文字行 {len(bands)} 条")
    mark(len(bands) == 5, f"下拉有 5 种重命名模式（量到 {len(bands)} 条）")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
# 右键文件：整份菜单都不该再有可展开的项（批量重命名去了目录栏；
# 第 16 轮起 新建/查看/排序方式 也只在空白处右键才出现）
hw = launch(BASE)
rclick(hw, VIEW_LEFT + 60, TITLE_H + TOOL_H + HEADER_H + 10)
ms = windows_of("ZenFileMenu")
if ms:
    mh, mx, my, mw, mhh = ms[0]
    top0, row_h = menu_metrics(mh, mw, mhh)
    worst = 1
    for i in range(0, 9):
        yi = menu_row_center(top0, row_h, i, (6,))
        if yi < 2 or yi > mhh - 2:
            continue
        u.PostMessageW(mh, WM_MOUSEMOVE, 0, (yi << 16) | (mw // 2))
        time.sleep(0.35)
        worst = max(worst, len(windows_of("ZenFileMenu")))
    print(f"  逐行悬停前 9 行，最多同时出现 {worst} 个菜单窗口")
    mark(worst == 1, "文件右键菜单里没有任何可展开的项")

# ============================================================ 4) 设置页开关
print("\n[4] 设置页的开关是真控件")
hw = launch("shell:Settings")
time.sleep(1.0)
raw, stride, W, H = grab(hw)
row_h = 34
# 第一个开关行（显示隐藏的项目）的开关矩形：行右沿 W-24，开关宽 38
sw_x0, sw_x1 = W - 24 - 38, W - 24
row0_top = TITLE_H + TOOL_H + 84
cy = row0_top + row_h // 2
sw_y0, sw_y1 = cy - 9, cy + 9
before_accent = count(raw, stride, sw_x0, sw_x1, sw_y0, sw_y1, ACCENT, 30)
print(f"  开关矩形 {sw_x0}..{sw_x1} x {sw_y0}..{sw_y1}：点之前强调色像素 {before_accent}")
mark(before_accent <= 4, f"默认关闭时开关不是强调色（{before_accent}px）")
u.PostMessageW(hw, WM_LBUTTONDOWN, 0, (cy << 16) | 400)
u.PostMessageW(hw, WM_LBUTTONUP, 0, (cy << 16) | 400)
time.sleep(1.0)
raw2, stride2, W, H = grab(hw)
after_accent = count(raw2, stride2, sw_x0, sw_x1, sw_y0, sw_y1, ACCENT, 30)
print(f"  点这一行之后强调色像素 {after_accent}")
mark(after_accent >= 100, f"打开后开关变成强调色胶囊（{after_accent}px）")

# ============================================================ 5) 剪贴板
print("\n[5] 复制真的写进剪贴板（不再报全 0 错误）")
clear_clipboard()
hw = launch(BASE)
u.SetForegroundWindow(hw)
time.sleep(0.6)
u.PostMessageW(hw, WM_LBUTTONDOWN, 0, ((TITLE_H + TOOL_H + HEADER_H + 10) << 16) | (VIEW_LEFT + 200))
u.PostMessageW(hw, WM_LBUTTONUP, 0, ((TITLE_H + TOOL_H + HEADER_H + 10) << 16) | (VIEW_LEFT + 200))
time.sleep(0.4)
u.keybd_event(0x11, 0, 0, 0)
u.keybd_event(0x43, 0, 0, 0)
time.sleep(0.1)
u.keybd_event(0x43, 0, 2, 0)
u.keybd_event(0x11, 0, 2, 0)
time.sleep(1.2)
fmts = clip_formats()
dlg = dialogs()
print(f"  剪贴板格式 {fmts}；弹窗 {dlg}")
mark(CF_HDROP in fmts, "剪贴板里真的有 CF_HDROP")
mark(not dlg, f"没有「剪贴板写入失败」弹窗（{dlg}）")

# ============================================================ 6) 禁用态
print("\n[6] 剪贴板为空时「粘贴」是淡化不可点")
clear_clipboard()
hw = launch(BASE)
# 必须右键**文件**行：文件夹菜单没有「打开方式」，后面整体差一格。
rclick(hw, VIEW_LEFT + 60, TITLE_H + TOOL_H + HEADER_H + ROW_H + 10)
ms = windows_of("ZenFileMenu")
if not ms:
    mark(False, "右键菜单没弹出")
else:
    mh, mx, my, mw, mhh = ms[0]
    top0, row_h = menu_metrics(mh, mw, mhh)
    # 0打开 1打开方式 2剪切 3复制 4重命名 5删除 6[sep] 7粘贴 8新建 9[sep] 10全选 …
    y_paste = menu_row_center(top0, row_h, 7, (6, 9))
    y_new = menu_row_center(top0, row_h, 8, (6, 9))
    raw, stride, _, _ = grab(mh)
    pb = bright(raw, stride, 30, mw - 24, y_paste - 8, y_paste + 8)
    nb = bright(raw, stride, 30, mw - 24, y_new - 8, y_new + 8)
    print(f"  「粘贴」行亮像素 {pb}；「新建」行亮像素 {nb}")
    mark(pb * 2 < nb, f"「粘贴」文字淡化到不可用（{pb} vs 可用项 {nb}）")
    # 悬停禁用项：不该出现 hover 底色
    u.PostMessageW(mh, WM_MOUSEMOVE, 0, (y_paste << 16) | (mw // 2))
    time.sleep(0.4)
    raw2, st2, _, _ = grab(mh)
    hov = count(raw2, st2, mw // 2 - 3, mw // 2 + 3, y_paste - 6, y_paste + 6, HOVER, 12)
    print(f"  悬停「粘贴」时 hover 底色像素 {hov}")
    mark(hov <= 20, f"禁用项不参与悬停高亮（{hov}px）")

# ============================================================ 7) 橙色竖线
print("\n[7] 子菜单里当前项行首是橙色竖线")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.4)
hw = launch(BASE)
# 标题栏「更多」按钮：menu_btn = W-44*4 .. W-44*3
u.PostMessageW(hw, WM_LBUTTONDOWN, 0, (17 << 16) | ((W - 44 * 4 + 22) & 0xFFFF))
u.PostMessageW(hw, WM_LBUTTONUP, 0, (17 << 16) | ((W - 44 * 4 + 22) & 0xFFFF))
time.sleep(0.9)
ms = windows_of("ZenFileMenu")
orange_ok = False
subs = []
if ms:
    mh, mx, my, mw, mhh = ms[0]
    top0, row_h = menu_metrics(mh, mw, mhh)
    # 0新建标签页 1刷新 2[sep] 3视图 4排序方式
    y_view = menu_row_center(top0, row_h, 3, (2,))
    u.PostMessageW(mh, WM_MOUSEMOVE, 0, (y_view << 16) | (mw // 2))
    time.sleep(0.8)
    subs = [m for m in windows_of("ZenFileMenu") if m[0] != mh]
    print(f"  悬停「视图」后子菜单 {len(subs)} 个")
    if subs:
        sh, sx, sy, sw, shh = subs[0]
        raw, stride, _, _ = grab(sh)
        orange = count(raw, stride, 0, 4, 0, shh, ORANGE, 30)
        blue = count(raw, stride, 0, 4, 0, shh, ACCENT, 30)
        print(f"  子菜单左沿 4px：橙色 {orange}px / 蓝色 {blue}px")
        mark(orange >= 20, f"当前生效项画了橙色竖线（{orange}px）")
        orange_ok = orange >= 20
if not ms:
    mark(False, "「更多」菜单没弹出")
elif not orange_ok and not subs:
    mark(False, "「视图」没有展开下级菜单")

# ============================================================ 8) 属性进任务栏
print("\n[8] 属性窗口带 WS_EX_APPWINDOW（任务栏有它）")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.4)
hw = launch(BASE)
# 同样是文件行（见 [6]）：文件夹菜单的行号整体差一格
rclick(hw, VIEW_LEFT + 60, TITLE_H + TOOL_H + HEADER_H + ROW_H + 10)
ms = windows_of("ZenFileMenu")
ph = None
if ms:
    mh, mx, my, mw, mhh = ms[0]
    top0, row_h = menu_metrics(mh, mw, mhh)
    # 第 16 轮：0打开 1打开方式 2剪切 3复制 4重命名 5删除 6[sep] 7粘贴 8属性
    for mul in (0, -1, 1):
        yy = menu_row_center(top0, row_h, 8, (6,)) + mul * row_h
        if yy < 2 or yy > mhh - 2:
            continue
        u.PostMessageW(mh, WM_MOUSEMOVE, 0, (yy << 16) | (mw // 2))
        u.PostMessageW(mh, WM_LBUTTONDOWN, 0, (yy << 16) | (mw // 2))
        u.PostMessageW(mh, WM_LBUTTONUP, 0, (yy << 16) | (mw // 2))
        for _ in range(40):
            time.sleep(0.05)
            p = windows_of("ZenFileProps")
            if p:
                ph = p[0][0]
                break
        if ph:
            break
mark(ph is not None, "属性窗口能打开")
if ph:
    ex = u.GetWindowLongW(ph, GWL_EXSTYLE)
    print(f"  属性窗口 WS_EX = 0x{ex & 0xFFFFFFFF:08X}")
    mark(bool(ex & WS_EX_APPWINDOW), "属性窗口带 WS_EX_APPWINDOW（任务栏有按钮）")

subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
print("\n" + ("第 15 轮全部通过" if ok_all else "第 15 轮存在失败项"))
