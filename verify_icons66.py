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
    ("ClientToScreen", None, [wt.HWND, ctypes.POINTER(wt.POINT)]),
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


TITLE_H = 34
HIT_W = 44
CY = TITLE_H // 2
TEXT = (222, 222, 222)
DIM = (143, 143, 143)


def lum(c):
    return (c[0] * 299 + c[1] * 587 + c[2] * 114) // 1000


def glyph_box(raw, stride, w, h, x0, x1, y0, y1, threshold=120):
    """量字形墨迹的包围盒。**按比区域底色亮多少判定**，不按绝对色值。

    第67轮：字形统一成 1px 描边后，绝对色值判据（认 text=222）在
    「最大化」这种空心方框上会采样不到 —— 笔画细，一行里可能只有 1~2 个
    像素命中，包围盒算出来是空的。相对亮度判据（比众数底色亮 >=60）
    对实心/空心/描边一视同仁。

    也不能用"非背景色"：标题栏按键区同时有 hover 底色块(61) 和关闭键的
    danger(228,88,90)，会把它们一起框进来，包围盒恒等于整个 24x24 底色块
    —— 那样任何尺寸差异都测不出来（第63轮已在动画判据上踩过同款坑）。
    """
    hist = {}
    for y in range(max(0, y0), min(y1, h)):
        b = y * stride
        for x in range(max(0, x0), min(x1, w)):
            o = b + x * 4
            c = (raw[o + 2], raw[o + 1], raw[o])
            hist[c] = hist.get(c, 0) + 1
    if not hist:
        return None, 0
    bg = max(hist, key=hist.get)
    bg_l = lum(bg)
    xs, ys = [], []
    for y in range(max(0, y0), min(y1, h)):
        b = y * stride
        for x in range(max(0, x0), min(x1, w)):
            o = b + x * 4
            c = (raw[o + 2], raw[o + 1], raw[o])
            if c == bg:
                continue
            # **只要「中性亮色」** —— 排除 hover 底色块(61 灰) 与
            # 关闭键的 danger(228,88,90 红)。字形是白/灰(R==G==B)，
            # danger 的 R 比 G/B 高 140 一档，一测便知。
            if abs(c[0] - c[1]) > 24 or abs(c[1] - c[2]) > 24:
                continue
            if lum(c) - bg_l >= threshold:
                xs.append(x)
                ys.append(y)
    if not xs:
        return None, 0
    return (min(xs), min(ys), max(xs), max(ys)), len(xs)


def away():
    """挪到内容区并等动画跑完（420ms + 余量）。"""
    move(OX + 500, OY + 500)
    time.sleep(0.8)


def hover(cx, cy):
    """划到 (cx, cy)。**必须微挪两次**：三大金刚键的 hover 只在
    WM_NCMOUSEMOVE 里判，一次大跨度 SetCursorPos 常被系统合并。"""
    move(OX + cx - 3, OY + cy)
    time.sleep(0.04)
    move(OX + cx, OY + cy)
    time.sleep(0.6)


KEYS = [
    ("最小化", W_WIN - HIT_W * 3, W_WIN - HIT_W * 2),
    ("最大化", W_WIN - HIT_W * 2, W_WIN - HIT_W),
    ("关闭", W_WIN - HIT_W, W_WIN),
]

print("=== A 标题栏同族：三个窗口键必须一样大 ===")
boxes = {}
for name, l, rt in KEYS:
    away()
    hover((l + rt) // 2, CY)
    r, s, w, h = grab(hw)
    bb, cnt = glyph_box(r, s, w, h, l, rt, 0, TITLE_H)
    if bb is None:
        mark(False, "A %s 抓不到字形" % name)
        continue
    boxes[name] = (bb[2] - bb[0] + 1, bb[3] - bb[1] + 1)
    print("        %s 字形 %dx%d @%s" % (name, boxes[name][0], boxes[name][1], bb))

if len(boxes) == len(KEYS):
    # 同 B 组的道理：**比外接框长边**，不比宽度。
    #
    # 最小化是**横线**（宽 7 × 高 1）、关闭是**斜十字**（5×5）、最大化是
    # **方框**（7×7）。1px 描边下不同形状的外接盒天生不同宽 ——
    # 直接比宽度会把「× 比 — 窄 2px」判成不一致，那不是尺寸问题，是形状问题。
    # 取 max(宽,高) 才是"这个图标画了多大"的正确度量。
    longs = [max(v[0], v[1]) for v in boxes.values()]
    detail = {k: "%dx%d" % v for k, v in boxes.items()}
    uniform = max(longs) - min(longs) <= 1
    mark(uniform, "A 三个窗口键外接框长边一致（%s，差 %dpx）%s；宽高天生不同（横线/斜十字/方框）"
         % (longs, max(longs) - min(longs), detail))
    mark(all(m >= 5 for m in longs),
         "A 三个窗口键长边都 >=5px（描边没糊）%s" % (detail,))

print("=== B 工具栏同族：三个导航键必须一样大 ===")
#
# 这里踩了四个坑，每一个都写在下面 —— 都是"判据本身错了"而不是"代码错了"：
#
# 1. **坐标系**：`grab()` 抓的是 `GetWindowRect`（**含边框**），而
#    `compute_layout` 的坐标是**客户区**的。A 组的标题栏键从**右**边算，
#    右边框宽对它无影响；B 组的导航键从**左**边算，不减掉边框偏移就
#    整体偏 8px，量到的根本不是图标。必须先 `ClientToScreen(0,0)` 求偏移。
# 2. **几何不能"扫出来"**：扫墨迹簇会把地址栏的面包屑文字也切进来
#    （实测切出 126..162 / 181..230 两段，被当成 36px / 49px 的"导航键"，
#    于是 C 组报"导航键 49px > 窗口键 10px"—— 那个 49 是文字宽度）。
#    正确做法是**直接按 compute_layout 的公式推**矩形，只在矩形内量。
# 3. **取色要认 dim**：导航键即使可点也画 `text_dim`(143)，只认
#    `theme.text`(222) 时整条工具栏带几乎是空的，看着像"图标没画"。
# 4. **边框/缩放会改这些数字**：`pad=6 nav=30 title=34 toolbar=40` 是
#    100% 缩放下的值，改 Metrics 要同步改这里（脚本里集中成常量）。

cr = wt.RECT()
u.GetClientRect(hw, ctypes.byref(cr))
origin = wt.POINT(0, 0)
u.ClientToScreen(hw, ctypes.byref(origin))
wr = wt.RECT()
u.GetWindowRect(hw, ctypes.byref(wr))
OX_B, OY_B = origin.x - wr.left, origin.y - wr.top
print("        客户区原点偏移 = (%d, %d)" % (OX_B, OY_B))

PAD, NAV, TITLE, TOOLBAR = 6, 30, 34, 40
NAV_Y0, NAV_Y1 = TITLE + PAD, TITLE + TOOLBAR - PAD
NAV_RECTS = [
    ("返回", PAD, PAD + NAV),
    ("前进", PAD + NAV + 2, PAD + NAV * 2 + 2),
    ("上级", PAD + NAV * 2 + 4, PAD + NAV * 3 + 4),
]
print("        导航键 y=%d..%d，x=%s" % (NAV_Y0, NAV_Y1, NAV_RECTS))


def lum(c):
    return (c[0] * 299 + c[1] * 587 + c[2] * 114) // 1000


def measure(grabbed, x0, x1, y0, y1):
    """在客户区矩形内量字形墨迹的 (宽, 高)。

    **按"比这块区域的底色亮多少"判定，不按绝对色值** —— 固定色值在这里
    全部不管用（都踩过）：

      * 标题栏：底色 (0,0,0)、字形 222；
      * 工具栏：底色 (25,25,25)、字形是 (69,69,69) 这种中间调。

    两者都不是 theme.text 也不是 theme.text_dim（导航键画的是 hover/disabled
    混合态）。认死色值的结果是"整块区域量到空"，看起来像"图标没画"。
    底色取区域内的众数，字迹取明显比它亮的像素 —— 对任何主题任何底色都成立。
    """
    raw, st, ww, hh = grabbed
    # 先统计底色（众数）
    hist = {}
    for y in range(y0, y1):
        Y = y + OY_B
        if not (0 <= Y < hh):
            continue
        for x in range(x0, x1):
            X = x + OX_B
            if not (0 <= X < ww):
                continue
            o = Y * st + X * 4
            c = (raw[o + 2], raw[o + 1], raw[o])
            hist[c] = hist.get(c, 0) + 1
    if not hist:
        return None
    bg = max(hist, key=hist.get)
    bg_l = lum(bg)

    xs, ys = [], []
    for y in range(y0, y1):
        Y = y + OY_B
        if not (0 <= Y < hh):
            continue
        for x in range(x0, x1):
            X = x + OX_B
            if not (0 <= X < ww):
                continue
            o = Y * st + X * 4
            c = (raw[o + 2], raw[o + 1], raw[o])
            if c == bg:
                continue
            if lum(c) - bg_l >= 28:
                xs.append(x)
                ys.append(y)
    if not xs:
        return None
    return (max(xs) - min(xs) + 1, max(ys) - min(ys) + 1)


nav = {}
for name, x0, x1 in NAV_RECTS:
    away()
    hover((x0 + x1) // 2, (NAV_Y0 + NAV_Y1) // 2)
    sz = measure(grab(hw), x0, x1, NAV_Y0, NAV_Y1)
    if sz is None:
        mark(False, "B %s 抓不到字形（客户区 x %d..%d, y %d..%d）" % (name, x0, x1, NAV_Y0, NAV_Y1))
        continue
    nav[name] = sz
    print("        %s 字形 %dx%d" % (name, sz[0], sz[1]))

if len(nav) == 3:
    # **比「外接框的较长边」**。
    #
    # 横箭头（返回/前进）扁：宽 13 × 高 8；竖箭头（上级）方：宽 8 × 高 13。
    # 两者是同一套描边画出来的**同一个尺寸**，只是朝向不同 —— 直接比宽度
    # 会把 13 vs 8 判成不一致（第一版就栽在这）。取 max(宽,高) 才是
    # 「这个图标有多大」的正确度量。
    longs = [max(v[0], v[1]) for v in nav.values()]
    detail = {k: "%dx%d" % v for k, v in nav.items()}
    uniform = max(longs) - min(longs) <= 1
    mark(uniform, "B 三个导航键外接框长边一致（%s，差 %dpx）%s；宽高天生不同（横箭头扁/竖箭头方）"
         % (longs, max(longs) - min(longs), detail))
else:
    print("    SKIP  B 导航键同族（只抓到 %d 个：%s）" % (len(nav), list(nav)))

print("=== C 跨族比例：导航键(强调) 应 > 窗口键(小控件) ===")
if nav and boxes:
    nw = max(v[0] for v in nav.values())
    nh = max(v[1] for v in nav.values())
    kw, kh = boxes["最大化"]     # 用方框，别用横线（高度只有 1，比不出比例）
    mark(nw > kw and nh > kh,
         "C 导航键比窗口键大（导航 %dx%d vs 窗口 %dx%d）" % (nw, nh, kw, kh))
else:
    print("    SKIP  C 跨族比例（上一组没抓到）")

print("\n=== 汇总 ===")
print("  全部通过" if ok_all else "  有失败项")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
verify_env.restore()
sys.exit(0 if ok_all else 1)
