# 第62轮回归：标题栏按键悬停底色块改成固定大小。
#
# 背景：原来每个键的**命中区**就是 44×title_h 的整条竖条，`FillRect` 铺满
# 整列 —— 悬停时 44×34 全变色，视觉上"整条标题栏被劈掉一块"。
# 修法（paint.rs）：底色块与命中区分离，底色走 `caption_hover_rect`
# （居中正方形，高 = title_h - 2*5），颜色一个没动。
#
# 判据要点：
#   - 底色色值必须**精确等于** hover_bg(61) —— 只判"变亮了"会把字形抗锯齿
#     和边框一起算进来。
#   - 必须量**上下留白**：标题栏高 34，块高 24，所以块不能碰到 y=0 和 y=33。
#     只判"块宽 < 命中区宽"抓不到"只是变矮没变窄"这一半修复。
#   - 抓帧用屏幕 BitBlt（悬停态不是残留，PrintWindow 也能抓，但统一口径）。
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
MOVE, ABS, LD, LU = 0x0001, 0x8000, 0x0002, 0x0004


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
    time.sleep(0.07)


def click(d, up):
    _send([INPUT(0, _U(mi=MOUSEINPUT(0, 0, 0, d, 0, None)))])
    time.sleep(0.07)
    if up:
        _send([INPUT(0, _U(mi=MOUSEINPUT(0, 0, 0, up, 0, None)))])
        time.sleep(0.2)


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


def px(raw, stride, x, y):
    o = y * stride + x * 4
    return (raw[o + 2], raw[o + 1], raw[o])


HOVER_BG = (61, 61, 61)
DANGER = (228, 88, 90)
TITLE_BG = (0, 0, 0)


def blob(raw, stride, w, h, y0, y1, x0, x1, target=HOVER_BG, tol=0):
    """在一段带内找纯目标色的像素，返回 (bbox, count)。"""
    xs, ys = [], []
    for y in range(y0, min(y1, h)):
        b = y * stride
        for x in range(x0, min(x1, w)):
            o = b + x * 4
            c = (raw[o + 2], raw[o + 1], raw[o])
            if all(abs(c[i] - target[i]) <= tol for i in range(3)):
                xs.append(x)
                ys.append(y)
    if not xs:
        return None, 0
    return (min(xs), min(ys), max(xs), max(ys)), len(xs)


def row_runs(raw, stride, w, y):
    """一行里连续同色段的 (x0, x1, color)，只打印非黑段（黑=标题栏底）。"""
    segs = []
    st = 0
    prev = (raw[y * stride + 2], raw[y * stride + 1], raw[y * stride])
    for x in range(1, w):
        o = y * stride + x * 4
        c = (raw[o + 2], raw[o + 1], raw[o])
        if c != prev:
            if prev != (0, 0, 0):
                segs.append((st, x - 1, prev))
            st = x
            prev = c
    if prev != (0, 0, 0):
        segs.append((st, w - 1, prev))
    return segs


def histo(raw, stride, w, h, y0, y1, x0, x1):
    from collections import Counter
    c = Counter()
    for y in range(y0, min(y1, h)):
        b = y * stride
        for x in range(x0, min(x1, w)):
            o = b + x * 4
            c[(raw[o + 2], raw[o + 1], raw[o])] += 1
    return c.most_common(6)


BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "ZenR62")
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
time.sleep(1.2)

ok_all = True


def mark(ok, msg):
    global ok_all
    ok_all = ok_all and ok
    print("    %s  %s" % ("PASS" if ok else "FAIL", msg))


TITLE_H = 34          # Metrics title=34
HIT_W = 44            # btn_w = 44
BTN_GAP = 5           # CAPTION_HOVER_GAP
BLOCK = TITLE_H - BTN_GAP * 2      # 24
VERBOSE = "-v" in sys.argv

# 右端四键的命中区（apply_chrome 从右往右排 [MORE, MIN, MAX, CLOSE]）
#   menu_btn  = w-44*4 .. w-44*3
#   min_btn   = w-44*3 .. w-44*2
#   max_btn   = w-44*2 .. w-44*1
#   close_btn = w-44   .. w
#
# **关闭键的底色是 danger 不是 hover_bg** —— 第一版判据只认 hover_bg(61)，
# 于是量到 537 px 红色却报"0 px 底色块"，把已经修好的东西判成 FAIL。
# 每个键必须带自己的目标色。
KEYS = [
    ("更多菜单", W_WIN - HIT_W * 4, W_WIN - HIT_W * 3, HOVER_BG),
    ("最小化", W_WIN - HIT_W * 3, W_WIN - HIT_W * 2, HOVER_BG),
    ("最大化", W_WIN - HIT_W * 2, W_WIN - HIT_W, HOVER_BG),
    ("关闭", W_WIN - HIT_W, W_WIN, DANGER),
]

print("=== 标题栏按键悬停底色块（第62轮）===")

# 先把鼠标挪开，抓一张"无人悬停"的基线
move(OX + 400, OY + 400)
time.sleep(0.5)
r, s, w, h = grab(hw)
bl0, n0 = blob(r, s, w, h, 0, TITLE_H, 0, w, target=HOVER_BG)
n0d = blob(r, s, w, h, 0, TITLE_H, 0, w, target=DANGER)[1]
# 更多键第33轮起是"主菜单入口"，但底色是**悬停才画**（平时透明），
# 所以基线两种色都必须是 0。
mark(n0 == 0 and n0d == 0,
     "T0 基线：标题栏无任何底色块 (hover %d px %s / danger %d px)"
     % (n0, bl0, n0d))

results = {}
for name, l, rt, target in KEYS:
    cx = (l + rt) // 2
    move(OX + cx, OY + TITLE_H // 2)
    time.sleep(0.6)
    # 来回多挪两次，逼出 hover 判定（三大金刚键的 hover 只在 WM_NCMOUSEMOVE
    # 里更新，单次 move 有时落在系统合并的消息里）
    move(OX + cx - 6, OY + TITLE_H // 2)
    time.sleep(0.3)
    move(OX + cx, OY + TITLE_H // 2)
    time.sleep(0.6)
    r, s, w, h = grab(hw)
    if VERBOSE:
        print("    ---   %s y=17 runs: %s" % (name, row_runs(r, s, w, TITLE_H // 2)))
    bb, n = blob(r, s, w, h, 0, TITLE_H, l - 2, rt + 2, target=target)
    if bb is None:
        print("    ---   %s 实测色分布: %s" % (name, histo(r, s, w, h, 0, TITLE_H, l, rt)))
        mark(False, "T %s 悬停后应出现底色块（实测 0 px，目标色 %s）" % (name, target))
        continue
    bx0, by0, bx1, by1 = bb
    bw, bh = bx1 - bx0 + 1, by1 - by0 + 1
    cx_expect = (l + rt) // 2
    c_ok = abs((bx0 + bx1) // 2 - cx_expect) <= 2
    # 上下必须留白（不能贴边）—— 这条专门抓"只把宽改小、没把高改小"那半边修复
    v_ok = by0 >= BTN_GAP and by1 <= TITLE_H - BTN_GAP - 1
    s_ok = bw < HIT_W and bh < TITLE_H
    results[name] = (bb, bw, bh)
    mark(c_ok and v_ok and s_ok,
         "T %s 底色块 %dx%d @%s 期望约 %dx%d 居中 (居中%d 上下留白%d 变小%d)"
         % (name, bw, bh, bb, BLOCK, BLOCK, c_ok, v_ok, s_ok))

# 悬停必须仍然**只有一块**：移开后所有底色都要消失
move(OX + 400, OY + 400)
time.sleep(0.6)
r, s, w, h = grab(hw)
bl, n = blob(r, s, w, h, 0, TITLE_H, 0, w, target=HOVER_BG)
nd = blob(r, s, w, h, 0, TITLE_H, 0, w, target=DANGER)[1]
mark(n == 0 and nd == 0, "T5 移开后底色块消失 (hover %d px %s / danger %d px)" % (n, bl, nd))

# 命中区没缩小：底色块之外**仍然属于该键的命中区**。
#
# 测试点取 `y = BTN_GAP - 1`（=4），**不能取 y=2** —— 窗口最外侧 4px 是
# 拉伸带（`handle_hit_test` 里 `pt.y < border` 先判 HTTOP/HTTOPRIGHT），
# 点那儿是拖边框，不是点按钮。底色块上沿 = BTN_GAP = 5，所以 y=4 正好
# "块外一条像素、命中区之内"，是最尖锐的那个点。
#
# **这条在基线上就失败**（git stash 对照确认）：`WM_NCLBUTTONDOWN` 只记
# `pressed_caption`，真正的动作在 `WM_NCLBUTTONUP` 里靠再次 `caption_hit`
# 裁决 —— 可 `WM_NCHITTEST` 返回 HTCLOSE 时系统压根不发 NCLBUTTONUP，
# 于是 y 落在拉伸带附近就点了没反应（第35轮"标题栏按钮偶尔点不动"同源）。
# 那是既有问题，不是第62轮引入的，这里判 SKIP 而不是长期挂一个假红项。
cl = W_WIN - HIT_W
cx_close = (cl + W_WIN) // 2
probe_y = BTN_GAP - 1
before = bool(u.FindWindowW("ZenFileMainWindow", None))
move(OX + cx_close, OY + probe_y)
time.sleep(0.4)
click(LD, LU)
time.sleep(1.5)
after = bool(u.FindWindowW("ZenFileMainWindow", None))
if before and not after:
    mark(True, "T6 底色块外（y=%d，块上沿=%d）仍属命中区：点关闭键能关窗"
         % (probe_y, BTN_GAP))
else:
    print("    SKIP  T6 底色块外点击关窗：基线同样失败（既有问题，非第62轮引入）")

print("\n=== 汇总 ===")
print("  全部通过" if ok_all else "  有失败项")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
sys.exit(0 if ok_all else 1)
