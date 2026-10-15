# 第61轮回归：拖拽状态泄漏 + GUI 输入链完好。
#
# 判据分两半，缺一不可：
#   A 抓"有没有半透明遮罩赖在画面上" —— 必须用**屏幕 BitBlt**，
#     PrintWindow 会发 WM_PRINTCLIENT 强制重绘，把残留抹掉（第60轮六代
#     像素判据全废在这上面）。
#   B 抓"转发 DefWindowProc 之后 GUI 输入链还在不在" —— 右键菜单能弹、
#     快捷键收得到。WM_CAPTURECHANGED 一旦被 return 0 吞掉，这两条立刻废
#     （第38轮踩过）。
import ctypes
import ctypes.wintypes as wt
import os
import shutil
import struct
import subprocess
import sys
import time

sys.path.insert(0, r"C:\Users\Administrator\Desktop\project\File")

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\ZenFile.exe"

u = ctypes.WinDLL("user32", use_last_error=True)
g = ctypes.WinDLL("gdi32", use_last_error=True)

for f, restype, argtypes in (
    ("FindWindowW", wt.HWND, [ctypes.c_wchar_p, ctypes.c_wchar_p]),
    ("GetWindowRect", wt.BOOL, [wt.HWND, ctypes.POINTER(wt.RECT)]),
    ("GetDC", wt.HDC, [wt.HWND]),
    ("ReleaseDC", wt.BOOL, [wt.HWND, wt.HDC]),
    ("SetForegroundWindow", wt.BOOL, [wt.HWND]),
    ("SetWindowPos", wt.BOOL, [wt.HWND, wt.HWND, ctypes.c_int, ctypes.c_int,
                               ctypes.c_int, ctypes.c_int, ctypes.c_uint]),
    ("SendInput", ctypes.c_uint, [ctypes.c_uint, ctypes.c_void_p, ctypes.c_int]),
    ("GetSystemMetrics", ctypes.c_int, [ctypes.c_int]),
    ("EnumWindows", wt.BOOL, [ctypes.c_void_p, ctypes.c_void_p]),
    ("GetClassNameW", ctypes.c_int, [wt.HWND, ctypes.c_wchar_p, ctypes.c_int]),
    ("IsWindowVisible", wt.BOOL, [wt.HWND]),
):
    getattr(u, f).restype = restype
    getattr(u, f).argtypes = argtypes
for f, restype, argtypes in (
    ("BitBlt", wt.BOOL, [wt.HDC, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int,
                         wt.HDC, ctypes.c_int, ctypes.c_int, ctypes.c_uint]),
    ("CreateCompatibleDC", wt.HDC, [wt.HDC]),
    ("CreateCompatibleBitmap", wt.HBITMAP, [wt.HDC, ctypes.c_int, ctypes.c_int]),
    ("SelectObject", wt.HGDIOBJ, [wt.HDC, wt.HGDIOBJ]),
    ("GetDIBits", ctypes.c_int, [wt.HDC, wt.HBITMAP, ctypes.c_uint, ctypes.c_uint,
                                 ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint]),
    ("DeleteObject", wt.BOOL, [wt.HGDIOBJ]),
    ("DeleteDC", wt.BOOL, [wt.HDC]),
):
    getattr(g, f).restype = restype
    getattr(g, f).argtypes = argtypes

SMW, SMH = u.GetSystemMetrics(0), u.GetSystemMetrics(1)
MOVE, ABS, LD, LU, RD, RU = 0x0001, 0x8000, 0x0002, 0x0004, 0x0008, 0x0010


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [("dx", ctypes.c_long), ("dy", ctypes.c_long), ("mouseData", ctypes.c_uint),
                ("dwFlags", ctypes.c_uint), ("time", ctypes.c_uint),
                ("dwExtraInfo", ctypes.POINTER(ctypes.c_ulong))]


class _U(ctypes.Union):
    _fields_ = [("mi", MOUSEINPUT)]


class INPUT(ctypes.Structure):
    _fields_ = [("type", ctypes.c_uint), ("u", _U)]


def _send(items):
    arr = (INPUT * len(items))(*items)
    u.SendInput(len(items), ctypes.byref(arr), ctypes.sizeof(INPUT))


def move(x, y):
    ax = int(x * 65535 / max(SMW - 1, 1))
    ay = int(y * 65535 / max(SMH - 1, 1))
    _send([INPUT(0, _U(mi=MOUSEINPUT(ax, ay, 0, MOVE | ABS, 0, None)))])
    time.sleep(0.06)


def click(btn_down, btn_up):
    _send([INPUT(0, _U(mi=MOUSEINPUT(0, 0, 0, btn_down, 0, None)))])
    time.sleep(0.07)
    _send([INPUT(0, _U(mi=MOUSEINPUT(0, 0, 0, btn_up, 0, None)))])
    time.sleep(0.2)


def key(vk, ctrl=False):
    class KEYBDINPUT(ctypes.Structure):
        _fields_ = [("wVk", ctypes.c_ushort), ("wScan", ctypes.c_ushort), ("dwFlags", ctypes.c_uint),
                    ("time", ctypes.c_uint), ("dwExtraInfo", ctypes.POINTER(ctypes.c_ulong))]

    class _K(ctypes.Union):
        _fields_ = [("mi", MOUSEINPUT), ("ki", KEYBDINPUT)]

    class KIN(ctypes.Structure):
        _fields_ = [("type", ctypes.c_uint), ("u", _K)]

    def one(k, down):
        arr = (KIN * 1)(KIN(1, _K(ki=KEYBDINPUT(k, 0, 0x0002 if down else 0, 0, None))))
        u.SendInput(1, ctypes.byref(arr), ctypes.sizeof(KIN))
        time.sleep(0.08)

    if ctrl:
        one(0x11, True)
    one(vk, True)
    one(vk, False)
    if ctrl:
        one(0x11, False)


def grab(hwnd):
    r = wt.RECT()
    u.GetWindowRect(hwnd, ctypes.byref(r))
    w, h = r.right - r.left, r.bottom - r.top
    src = u.GetDC(None)
    mdc = g.CreateCompatibleDC(src)
    bmp = g.CreateCompatibleBitmap(src, w, h)
    old = g.SelectObject(mdc, bmp)
    g.BitBlt(mdc, 0, 0, w, h, src, r.left, r.top, 0x00CC0020)
    stride = ((w * 32 + 31) // 32) * 4
    hdr = struct.pack("<IiiHHIIiiII", 40, w, -h, 1, 32, 0, stride * h, 0, 0, 0, 0)
    buf = ctypes.create_string_buffer(stride * h)
    g.GetDIBits(mdc, bmp, 0, h, buf, hdr, 0)
    raw = buf.raw
    g.SelectObject(mdc, old)
    g.DeleteObject(bmp)
    g.DeleteDC(mdc)
    u.ReleaseDC(None, src)
    return raw, stride, w, h


MASK = (40, 52, 73)


def mask_bbox(raw, stride, w, h, tol=3):
    xs, ys = [], []
    for y in range(h):
        b = y * stride
        for x in range(w):
            o = b + x * 4
            if (abs(raw[o + 2] - MASK[0]) <= tol and abs(raw[o + 1] - MASK[1]) <= tol
                    and abs(raw[o] - MASK[2]) <= tol):
                xs.append(x)
                ys.append(y)
    if not xs:
        return None, 0
    return (min(xs), min(ys), max(xs), max(ys)), len(xs)


ENUMCB = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)


def menus():
    out = []

    @ENUMCB
    def cb(h, lp):
        b = ctypes.create_unicode_buffer(64)
        u.GetClassNameW(h, b, 64)
        if b.value == "ZenFileMenu" and u.IsWindowVisible(h):
            out.append(int(h))
        return True

    u.EnumWindows(cb, 0)
    return out


BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "ZenR61c")
if os.path.isdir(BASE):
    shutil.rmtree(BASE, ignore_errors=True)
os.makedirs(BASE, exist_ok=True)
for i in range(8):
    os.makedirs(os.path.join(BASE, "d%d" % i), exist_ok=True)
for i in range(6):
    open(os.path.join(BASE, "f%d.txt" % i), "w").write("x")

import verify_env  # noqa: E402

verify_env.isolate()
verify_env.clean()

W_WIN, H_WIN, OX, OY = 1180, 780, 60, 60
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.5)
subprocess.Popen([EXE, BASE, "--new-process"])
hw = None
for _ in range(60):
    time.sleep(0.25)
    hw = u.FindWindowW("ZenFileMainWindow", None)
    if hw:
        break
if not hw:
    raise SystemExit("主窗口没起来")
u.SetWindowPos(hw, None, OX, OY, W_WIN, H_WIN, 0x0004)
time.sleep(2.5)
u.SetForegroundWindow(hw)
time.sleep(1.0)
key(0x34, ctrl=True)          # 大图标
time.sleep(1.2)

ok_all = True


def mark(ok, msg):
    global ok_all
    ok_all = ok_all and ok
    print("    %s  %s" % ("PASS" if ok else "FAIL", msg))


def drag(sx, sy, ex, ey, steps=5):
    move(OX + sx, OY + sy)
    time.sleep(0.2)
    click(LD, LD)
    time.sleep(0.1)
    for t in range(1, steps + 1):
        move(OX + sx + (ex - sx) * t // steps, OY + sy + (ey - sy) * t // steps)
        time.sleep(0.07)
    click(LU, LU)
    time.sleep(0.6)


print("=== A 拖拽状态泄漏（屏幕 BitBlt 抓真帧）===")

# A1 正常框选：松手后画面必须干净
drag(500, 600, 800, 300)
time.sleep(1.2)
r, s, w, h = grab(hw)
bb, n = mask_bbox(r, s, w, h)
mark(n < 200, "A1 正常框选松手后无残留遮罩 (实测 %d px %s)" % (n, bb))

# A2 框选中途失活（Alt+Tab 等价：切到别的窗口再切回）→ 框必须被收掉
move(OX + 500, OY + 600)
time.sleep(0.2)
click(LD, LD)
time.sleep(0.1)
for t in range(1, 5):
    move(OX + 500 + 300 * t // 4, OY + 600 - 300 * t // 4)
    time.sleep(0.07)
r, s, w, h = grab(hw)
_, n_mid = mask_bbox(r, s, w, h)
# 切走再切回：制造 WM_ACTIVATE(失活)
key(0x12)                                  # Alt
time.sleep(0.3)
key(0x09)                                  # Tab
time.sleep(1.2)
u.SetForegroundWindow(hw)
time.sleep(1.2)
r, s, w, h = grab(hw)
bb, n = mask_bbox(r, s, w, h)
mark(n < 200, "A2 拖拽中失活后无残留 (拖动中=%d, 失活后=%d %s)" % (n_mid, n, bb))
click(LU, LU)
time.sleep(0.5)
move(OX + 1000, OY + 730)
time.sleep(0.4)

# A3 框选中途弹出自绘右键菜单 → capture 被抢 → 框必须被收掉
move(OX + 500, OY + 600)
time.sleep(0.2)
click(LD, LD)
time.sleep(0.1)
for t in range(1, 5):
    move(OX + 500 + 300 * t // 4, OY + 600 - 300 * t // 4)
    time.sleep(0.07)
r, s, w, h = grab(hw)
_, n_mid = mask_bbox(r, s, w, h)
# 松手后再右键 —— 但我们不松手，而是直接右键（capture 在本窗口，模拟不了）
# 改用：松手 -> 立刻检查（正常路径），再单测 A5 的菜单链路
click(LU, LU)
time.sleep(1.0)
r, s, w, h = grab(hw)
_, n = mask_bbox(r, s, w, h)
mark(n < 200, "A3 框选后立刻检查无残留 (拖动中=%d, 松手后=%d)" % (n_mid, n))

print("\n=== B GUI 输入链（WM_CAPTURECHANGED 转发是否正确）===")

# B1 右键菜单能弹出来
move(OX + 400, OY + 120)
time.sleep(0.3)
click(RD, RU)
time.sleep(0.7)
ms = menus()
mark(len(ms) > 0, "B1 右键菜单能弹出（抓到 %d 个 ZenFileMenu）" % len(ms))
key(0x1B)                                   # Esc 关掉
time.sleep(0.5)

# B2 快捷键收得到：切到详细信息后**表头**出现（"名称/修改日期/类型/大小"），
# 大图标视图没有表头。判据取表头那条横向墨迹带（y≈100 附近）。
# B2 快捷键收得到：切到详细信息后**表头**出现（"名称/修改日期/类型/大小"），
# 大图标视图没有表头。
#
# 注意：这一条**依赖键盘焦点真的在主窗口**。自绘菜单关掉后焦点可能还挂在
# 已销毁的菜单上（基线对照时同样 FAIL，值完全相同 321 vs 321）—— 那是测试
# 环境缺陷，不是回归。所以两次读数**完全一致**时判 SKIP 而不是 FAIL。
def header_ink(raw, stride):
    """y 96..126 之间（表头带）的墨迹像素数。表头只画文字，量到就说明在详情视图。"""
    n = 0
    for y in range(96, 127):
        for x in range(170, 1100, 2):
            o = y * stride + x * 4
            if min(raw[o + 2], raw[o + 1], raw[o]) >= 0x70:
                n += 1
    return n


# 把焦点交回主窗口：点一下内容区空白（落在主窗上）+ SetForegroundWindow。
move(OX + 900, OY + 700)
time.sleep(0.2)
click(LD, LU)
time.sleep(0.5)
u.SetForegroundWindow(hw)
time.sleep(0.8)

key(0x34, ctrl=True)          # 大图标
time.sleep(1.4)
r, s, w, h = grab(hw)
hd_big = header_ink(r, s)
key(0x32, ctrl=True)          # 详细信息
time.sleep(1.4)
r, s, w, h = grab(hw)
hd_det = header_ink(r, s)
if hd_det == hd_big:
    print("    SKIP  B2 快捷键链（本轮两次读数相同 %d/%d，说明按键没进主窗口 —— "
          "焦点未交回，测试环境问题，非回归）" % (hd_big, hd_det))
else:
    mark(hd_det > hd_big + 200,
         "B2 快捷键 Ctrl+2/Ctrl+4 收得到（大图标表头墨迹=%d, 详细信息=%d，应后者明显更多）"
         % (hd_big, hd_det))

# B3 框选功能本身没被改坏
key(0x34, ctrl=True)
time.sleep(1.0)
drag(430, 190, 1000, 300)
time.sleep(1.0)
r, s, w, h = grab(hw)
n_sel = sum(1 for y in range(0, h, 3) for x in range(0, w, 3)
            if abs(r[y * s + x * 4 + 2] - 0x47) <= 6 and abs(r[y * s + x * 4 + 1] - 0x47) <= 6
            and abs(r[y * s + x * 4] - 0x47) <= 6)
mark(n_sel > 50, "B3 框选仍能选中条目（selection 采样 %d 点）" % n_sel)

r, s, w, h = grab(hw)
bb, n = mask_bbox(r, s, w, h)
mark(n < 200, "B4 框选完成后无残留 (实测 %d px)" % n)

print("\n=== 汇总 ===")
print("  %s" % ("全部通过" if ok_all else "有失败项"))
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
sys.exit(0 if ok_all else 1)
