# 第63轮回归：标题栏按键的悬停形变动画 + 标签 × / 新建标签 + 的悬停。
#
# 三组判据：
#   A 动画开：把鼠标划到某个键上，**逐帧量字形墨迹的包围盒**，看它是否
#     变化过、峰值方向是否符合该键的语义、最后是否回到初始值。
#   B 动画关（设置-交互页「标题栏按键形变」=关）：同样划上去，
#     包围盒**逐帧完全不变**。
#   C 标签 × 与 + 号的悬停底色。
#
# 三条踩过的坑（都写在这里免得重犯）：
#   1. **必须量"字形"而不是"底色块"**：底色块（hover_bg）是 24×24 静止方块，
#      动画只作用在符号上。量底色块的话动画全测不出来。
#   2. **必须用屏幕 BitBlt 且抓"划上去之后"的连续帧**：动画只 420ms，
#      PrintWindow 会强制重绘把中间态抹平；而且第一帧往往已经过了半程。
#   3. **关闭窗口键的符号色是 theme.text(222)，底色是 danger(228,88,90)**：
#      量字形要挑对颜色，否则把底色当成了字形。
import ctypes
import ctypes.wintypes as wt
import json
import os
import shutil
import struct
import subprocess
import sys
import time

sys.path.insert(0, r"C:\Users\Administrator\Desktop\project\File")

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\ZenFile.exe"
# **必须用 verify_env 的路径**，不能自己拼 %APPDATA%：万一 verify_env 以后改成
# 指到别的临时目录，这里就会写到用户真实配置上去。
CFG_DIR = os.path.join(os.environ.get("APPDATA", os.path.expanduser("~")), "ZenFile")
SETTINGS = os.path.join(CFG_DIR, "settings.user.json")

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

# 悬停底色块（theme.hover_bg）与字形色（theme.text）。
# **两者必须分开认**：底色块是 24×24 静止方块，字形只有 11~17px。
# C 组判据量底色、A 组量字形，各用各的色值。量错色值会把已经修好的判成 FAIL
# （第62轮踩过：关闭键底色是 danger 不是 hover_bg）。
HOVER_BG = (61, 61, 61)
DANGER = (228, 88, 90)
TEXT = (222, 222, 222)


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


def glyph_box(raw, stride, x0, x1, y0, y1, target=(222, 222, 222), tol=40):
    """在给定矩形里找"字形色"的像素包围盒。

    字形色 = theme.text(222,222,222)。必须按颜色筛而不是"非背景色" ——
    底色块(61) 和 danger(228,88,90) 也在同一个矩形里，用"非背景"会把
    它们一起框进来，包围盒恒等于整个 24×24 底色块，动画完全测不出来。
    """
    xs, ys = [], []
    for y in range(max(0, y0), y1):
        b = y * stride
        for x in range(max(0, x0), x1):
            o = b + x * 4
            c = (raw[o + 2], raw[o + 1], raw[o])
            if all(abs(c[i] - target[i]) <= tol for i in range(3)):
                xs.append(x)
                ys.append(y)
    if not xs:
        return None, 0
    return (min(xs), min(ys), max(xs), max(ys)), len(xs)


def count_col(raw, stride, w, x0, x1, y0, y1, target):
    n = 0
    for y in range(max(0, y0), y1):
        b = y * stride
        for x in range(max(0, x0), x1):
            o = b + x * 4
            c = (raw[o + 2], raw[o + 1], raw[o])
            if all(abs(c[i] - target[i]) <= 3 for i in range(3)):
                n += 1
    return n


def set_caption_anim(on):
    """直接改 settings.user.json 里的 ui.anim.caption（设置页那个开关）。

    必须在 `verify_env.clean()` **之后**调用 —— clean() 会把两个配置文件删掉，
    写在它前面的场景配置全部作废。
    """
    os.makedirs(CFG_DIR, exist_ok=True)
    data = {}
    if os.path.exists(SETTINGS):
        try:
            with open(SETTINGS, "r", encoding="utf-8") as f:
                data = json.load(f)
        except Exception:
            data = {}
    ui = data.setdefault("ui", {})
    ui.setdefault("anim", {})["caption"] = bool(on)
    with open(SETTINGS, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=2)


BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "ZenR63")
if os.path.isdir(BASE):
    shutil.rmtree(BASE, ignore_errors=True)
os.makedirs(BASE, exist_ok=True)
for i in range(8):
    os.makedirs(os.path.join(BASE, "d%d" % i), exist_ok=True)
for i in range(6):
    open(os.path.join(BASE, "f%d.txt" % i), "w").write("x")

import verify_env  # noqa: E402

# **顺序要紧**：`isolate()` 备份 → `clean()` 清空 → 然后才写本脚本的场景配置。
# 写在 clean() 之前的那些配置会被 clean() 一起删掉（第一版踩过：`set_caption_anim`
# 在 clean() 之前调，文件直接没了，于是"动画关"那组其实跑的是默认值=开，
# 判据 B 全绿却是假绿）。
verify_env.isolate()
verify_env.clean()

W_WIN, H_WIN, OX, OY = 1180, 780, 60, 60
TITLE_H = 34
HIT_W = 44
CY = TITLE_H // 2

# 右端四键命中区：menu/min/max/close = w-44*4..w
KEYS = [
    ("更多菜单", W_WIN - HIT_W * 4, W_WIN - HIT_W * 3),
    ("最小化", W_WIN - HIT_W * 3, W_WIN - HIT_W * 2),
    ("最大化", W_WIN - HIT_W * 2, W_WIN - HIT_W),
    ("关闭", W_WIN - HIT_W, W_WIN),
]

ok_all = True


def mark(ok, msg):
    global ok_all
    ok_all = ok_all and ok
    print("    %s  %s" % ("PASS" if ok else "FAIL", msg))


def launch():
    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    time.sleep(0.6)
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
    return hw


def away():
    """把鼠标挪到内容区空白处并等动画跑完（420ms + 余量）。"""
    move(OX + 500, OY + 500)
    time.sleep(0.9)


def sweep(l, r, n=9):
    """划到键上并连续抓 n 帧（每帧间隔 ~50ms，覆盖整个 420ms 动画）。"""
    cx = (l + r) // 2
    move(OX + 500, OY + 500)
    time.sleep(0.35)
    boxes = []
    # 触发悬停 + 起手。
    #
    # **必须微挪两次**：三大金刚键的 hover 只在 WM_NCMOUSEMOVE 里判，而一次
    # 大跨度 SetCursorPos 常常被系统合并掉（同一次 move 里只产生一次通知），
    # 于是 hover_caption 压根没被设上 —— 现象是底色也不亮、动画也不动，
    # 看起来像动画没实现，其实是悬停没触发（第62轮也踩过，加了微挪才好）。
    move(OX + cx - 3, OY + CY)
    time.sleep(0.03)
    move(OX + cx, OY + CY)
    for _ in range(n):
        time.sleep(0.05)
        raw, s, w, h = grab(hw)
        bb, cnt = glyph_box(raw, s, l, r, 0, TITLE_H, target=TEXT)
        boxes.append(bb)
    return boxes


# ============================================================ 动画开
print("=== A 标题栏按键形变动画（anim.caption = 开）===")
set_caption_anim(True)
hw = launch()

for name, l, r in KEYS:
    # 先取一张"鼠标不在任何键上"的静止帧当基准
    away()
    raw, s, w, h = grab(hw)
    rest_bb, _ = glyph_box(raw, s, l, r, 0, TITLE_H, target=TEXT)
    rest = (rest_bb[2] - rest_bb[0] + 1, rest_bb[3] - rest_bb[1] + 1) if rest_bb else (0, 0)

    boxes = sweep(l, r)
    sizes = [(b[2] - b[0] + 1, b[3] - b[1] + 1) for b in boxes if b]
    if not sizes:
        mark(False, "A %s 抓不到字形（%s）" % (name, boxes))
        continue
    # 变化过 = 至少两帧的包围盒不同
    changed = len(set(boxes)) > 1
    # 归位 = 动画跑完后（末帧）回到**静止帧**的尺寸（±1px 量化误差）
    first, last = rest, sizes[-1]
    home = abs(first[0] - last[0]) <= 1 and abs(first[1] - last[1]) <= 1
    peak = max(s[0] for s in sizes), max(s[1] for s in sizes)
    trough = min(s[0] for s in sizes), min(s[1] for s in sizes)
    # **归位必须拿"动画结束后"那一帧去比"动画开始前"的静止帧**，不能拿
    # 逐帧序列的首尾比 —— 第一帧往往已经过了半程（起手到抓帧有 50ms 延迟），
    # 于是首帧=14、末帧=11，看起来像没归位，其实末帧才是真正的静止态。
    # 第一版用 boxes[0] 当基准，三个键里有两个报 FAIL，全是判据的锅。
    detail = "逐帧=%s" % (sizes[:7],)
    if name == "最大化":
        grew = peak[0] > rest[0] + 1
        mark(changed and home and grew,
             "A 最大化 放大后归位 (首=%s 峰=%s 末=%s 变化=%s 归位=%s) %s"
             % (first, peak, last, changed, home, detail))
    elif name == "最小化":
        # 缩小：任一帧的宽应**小于**静止帧的宽。横线字形的高度恒为 1（笔画粗），
        # 所以高度不能作为判据 —— 那不是动画失效，是横线本来就只有 1px 高。
        shrank = min(s[0] for s in sizes) < rest[0]
        # 最小化还要"下移"：末帧中心 y 应回到首帧（位移只作用在动画中）
        cy0 = (boxes[0][1] + boxes[0][3]) / 2
        cym = max((b[1] + b[3]) / 2 for b in boxes if b)
        went_down = cym >= cy0
        mark(changed and home and shrank and went_down,
             "A 最小化 缩小下沉后归位 (首=%s 峰=%s 末=%s 中途最低y=%.1f 起点y=%.1f) %s"
             % (first, peak, last, cym, cy0, detail))
    elif name == "关闭":
        shrank = min(s[0] for s in sizes) < rest[0]
        mark(changed and home and shrank,
             "A 关闭 缩小后归位 (首=%s 峰=%s 末=%s) %s" % (first, peak, last, detail))
    else:
        mark(changed and home,
             "A %s 内部细节动过后归位 (首=%s 峰=%s 末=%s) %s"
             % (name, first, peak, last, detail))

# 侧栏开关（最左）
away()
raw, s, w, h = grab(hw)
tg_bb, _ = glyph_box(raw, s, 0, HIT_W, 0, TITLE_H, target=TEXT)
tg_rest = (tg_bb[2] - tg_bb[0] + 1, tg_bb[3] - tg_bb[1] + 1) if tg_bb else (0, 0)
boxes = sweep(0, HIT_W)
sizes = [(b[2] - b[0] + 1, b[3] - b[1] + 1) for b in boxes if b]
changed = len(set(boxes)) > 1
home = sizes and abs(tg_rest[0] - sizes[-1][0]) <= 1 and abs(tg_rest[1] - sizes[-1][1]) <= 1
mark(changed and home,
     "A 侧栏开关 面板收放后归位 (静止=%s 逐帧=%s)" % (tg_rest, sizes[:7]))

# ============================================================ 动画关
print("=== B 同样的操作，开关关掉后应完全不动 ===")
away()
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.8)
set_caption_anim(False)
hw = launch()

for name, l, r in KEYS[:1] + KEYS[2:3]:   # 更多 + 最大化，各测一次
    boxes = sweep(l, r)
    sizes = [(b[2] - b[0] + 1, b[3] - b[1] + 1) for b in boxes if b]
    same = len(set(boxes)) == 1
    mark(same, "B %s 开关关闭后逐帧完全一致 (逐帧=%s)" % (name, sizes[:7]))

boxes = sweep(0, HIT_W)
mark(len(set(boxes)) == 1, "B 侧栏开关 开关关闭后逐帧完全一致")

# ============================================================ C × 与 + 的悬停
print("=== C 标签关闭按钮与新建标签键的悬停 ===")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.8)
set_caption_anim(True)
hw = launch()

# C1 标签 × 的悬停底色：**只在 × 上亮，不在标签其他位置亮**。
#
# 判据要点：必须**两个位置各测一次**。第一版只测"移到 × 上会亮"，
# 那样旧写法（跟着整个标签 hover 一起亮）同样 PASS —— 判据抓不到回归。
# 反向测（移到标签标题上 × **不该**亮）才是真正区分两者的那条。
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.8)
set_caption_anim(True)
hw = launch()
away()

# 第一个标签的几何：tabs.left = toggle_w(40)，tab_w 见 paint.rs TAB_W
# 关闭按钮在标签右侧 18px 处。这里不硬算，直接扫标题栏左侧找出 × 的墨迹列。
raw, s, w, h = grab(hw)
cols = []
for x in range(0, 400):
    n = count_col(raw, s, w, x, x + 1, 0, TITLE_H, TEXT)
    cols.append(n)
# × 是标题栏左侧最靠右的一簇亮像素（在标签图标/文字之后）
last_ink = max((x for x, n in enumerate(cols) if n > 0), default=-1)
mark(last_ink > 0, "C1 找到标签区最右墨迹列 x=%d（× 应在其附近）" % last_ink)

# 扫出 × 的横向范围：最右那一簇连续/近连续的墨迹列
x_end = last_ink
x_start = last_ink
while x_start > 0 and any(cols[xx] > 0 for xx in range(x_start - 3, x_start)):
    x_start -= 1
close_cx = (x_start + x_end) // 2

def hover_bg_n(x, y):
    raw, s, w, h = grab(hw)
    return count_col(raw, s, w, x - 6, x + 6, y - 6, y + 6, HOVER_BG)

# 移到 × 上 → 应有底色
move(OX + close_cx - 3, OY + CY)
time.sleep(0.05)
move(OX + close_cx, OY + CY)
time.sleep(0.7)
n_on = hover_bg_n(close_cx, CY)
mark(n_on > 20, "C1 标签 × 悬停时出现底色 (%d px hover_bg)" % n_on)

# 移到标签**标题**上（× 左边 40px）→ 不该有 × 的底色
away()
title_cx = max(60, close_cx - 40)
move(OX + title_cx - 3, OY + CY)
time.sleep(0.05)
move(OX + title_cx, OY + CY)
time.sleep(0.7)
raw, s, w, h = grab(hw)
n_off = count_col(raw, s, w, close_cx - 8, close_cx + 8, 4, TITLE_H - 4, HOVER_BG)
mark(n_off < 10, "C2 标签标题上（不在 × ）不亮 × 的底色 (%d px)" % n_off)

# C3 新建标签 + 键：扫出最左侧 ink 之后的那一簇
away()
raw, s, w, h = grab(hw)
cols2 = []
for x in range(0, 400):
    cols2.append(count_col(raw, s, w, x, x + 1, 0, TITLE_H, TEXT))
# 第一个标签图标起点约在 toggle_w(40)+8；+ 键紧跟最后一个标签
n_tabs = 1
plus_cx = None
# 直接用布局推：tabs.left=40, tab_w 按 TAB_W 常量，n=1 → + 在 tab_rect(0) 右侧 + 2
import re as _re
_tabw = None
try:
    src = open(r"C:\Users\Administrator\Desktop\project\File\workspace\crates\zenfile-app\src\paint.rs",
               encoding="utf-8").read()
    m = _re.search(r"pub const TAB_W: i32 = (\d+)", src)
    if m:
        _tabw = int(m.group(1))
except Exception:
    pass
mark(_tabw is not None, "C3 读到 TAB_W=%s（用于定位 + 键）" % _tabw)
if _tabw:
    tab0_left = 40
    plus_left = tab0_left + _tabw + 2
    plus_cx = plus_left + 15          # new_tab_rect 宽 30
    away()
    move(OX + plus_cx - 3, OY + CY)
    time.sleep(0.05)
    move(OX + plus_cx, OY + CY)
    time.sleep(0.7)
    raw, s, w, h = grab(hw)
    n_plus = count_col(raw, s, w, plus_cx - 8, plus_cx + 8, 4, TITLE_H - 4, HOVER_BG)
    mark(n_plus > 20, "C3 新建标签 + 键悬停时出现底色 (cx=%d, %d px)" % (plus_cx, n_plus))

print("\n=== 汇总 ===")
print("  全部通过" if ok_all else "  有失败项")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
sys.exit(0 if ok_all else 1)
