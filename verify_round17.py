"""第 17 轮像素级回归（侧栏 / 回收站 / 表头 / 空文件夹）。

判定全部走像素与窗口事实，不靠肉眼。覆盖：

  侧栏  S3 分组之间有空隙（"文件夹"组 与 回收站 之间那一格是空的）
        S4 侧栏折叠按钮已删除（侧栏顶部右上不再有独立方块）
  回收站 R2 有 名称/原位置/删除时间/大小 列头
         R3 三个按钮右对齐到视图最右侧
         R4 悬停条目有 hover 底色（比选中弱一档）
  整体  G21 空文件夹提示行内居中
        G4 详细信息最后一列不再一路画到窗口右边缘
        G15 右键「新建」是系统同一份 ShellNew 模板（文件夹 + N 个模板）
        G17 新建完立刻进入改名（Edit 框在位），文件名带「新建」前缀
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
    ("SetWindowPos", wt.BOOL, [wt.HWND, wt.HWND, ctypes.c_int, ctypes.c_int,
                               ctypes.c_int, ctypes.c_int, ctypes.c_uint]),
    ("SendInput", ctypes.c_uint, [ctypes.c_uint, ctypes.c_void_p, ctypes.c_int]),
    ("GetSystemMetrics", ctypes.c_int, [ctypes.c_int]),
    ("ReleaseDC", ctypes.c_int, [wt.HWND, wt.HDC]),
):
    getattr(u, f).restype = restype
    getattr(u, f).argtypes = argtypes
for f, restype, argtypes in (
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

for f, restype, argtypes in (
    ("EnumWindows", wt.BOOL, [ctypes.c_void_p, ctypes.c_void_p]),
    ("EnumChildWindows", wt.BOOL, [wt.HWND, ctypes.c_void_p, ctypes.c_void_p]),
    ("GetClassNameW", ctypes.c_int, [wt.HWND, ctypes.c_wchar_p, ctypes.c_int]),
    ("IsWindowVisible", wt.BOOL, [wt.HWND]),
    ("GetWindowTextW", ctypes.c_int, [wt.HWND, ctypes.c_wchar_p, ctypes.c_int]),
):
    getattr(u, f).restype = restype
    getattr(u, f).argtypes = argtypes

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\ZenFile.exe"
BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "ZenR17")
if os.path.isdir(BASE):
    for _r, _d, _f in os.walk(BASE):
        for _n in _d + _f:
            try:
                os.chmod(os.path.join(_r, _n), 0o666)
            except OSError:
                pass
    shutil.rmtree(BASE, ignore_errors=True)
os.makedirs(os.path.join(BASE, "empty_dir"), exist_ok=True)
for i in range(3):
    open(os.path.join(BASE, "f%d.txt" % i), "w").write("x")

W_WIN, H_WIN = 1180, 780
TITLE_H, TOOL_H = 34, 40
SIDEBAR_W = 157
VIEW_TOP = TITLE_H + TOOL_H
SIDE_ROW_H = 34

WINDOW_BG = (0x20, 0x20, 0x20)
CHROME_BG = (0x19, 0x19, 0x19)
ROW_HOVER = (0x2E, 0x2E, 0x2E)
SELECTION = (0x47, 0x47, 0x47)
SURFACE_ALT = (0x2B, 0x2B, 0x2B)
BORDER = (0x53, 0x53, 0x53)

MOUSEEVENTF_MOVE = 0x0001
MOUSEEVENTF_ABSOLUTE = 0x8000
MOUSEEVENTF_LEFTDOWN = 0x0002
MOUSEEVENTF_LEFTUP = 0x0004
MOUSEEVENTF_RIGHTDOWN = 0x0008
MOUSEEVENTF_RIGHTUP = 0x0010

ok_all = True


def mark(ok, msg):
    global ok_all
    ok_all = ok_all and ok
    print(f"    {'PASS' if ok else 'FAIL'}  {msg}")


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [("dx", ctypes.c_long), ("dy", ctypes.c_long), ("mouseData", ctypes.c_uint),
                ("dwFlags", ctypes.c_uint), ("time", ctypes.c_uint),
                ("dwExtraInfo", ctypes.POINTER(ctypes.c_ulong))]


class _U(ctypes.Union):
    _fields_ = [("mi", MOUSEINPUT)]


class INPUT(ctypes.Structure):
    _fields_ = [("type", ctypes.c_uint), ("u", _U)]


SMW = u.GetSystemMetrics(0)
SMH = u.GetSystemMetrics(1)


def _send(items):
    arr = (INPUT * len(items))(*items)
    u.SendInput(len(items), ctypes.byref(arr), ctypes.sizeof(INPUT))


def move(x, y):
    ax = int(x * 65535 / max(SMW - 1, 1))
    ay = int(y * 65535 / max(SMH - 1, 1))
    _send([INPUT(0, _U(mi=MOUSEINPUT(ax, ay, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE, 0, None)))])
    time.sleep(0.05)


def click(x, y):
    move(x, y)
    _send([INPUT(0, _U(mi=MOUSEINPUT(0, 0, 0, MOUSEEVENTF_LEFTDOWN, 0, None)))])
    time.sleep(0.05)
    _send([INPUT(0, _U(mi=MOUSEINPUT(0, 0, 0, MOUSEEVENTF_LEFTUP, 0, None)))])
    time.sleep(0.15)


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [("wVk", ctypes.c_ushort), ("wScan", ctypes.c_ushort), ("dwFlags", ctypes.c_uint),
                ("time", ctypes.c_uint), ("dwExtraInfo", ctypes.POINTER(ctypes.c_ulong))]


class _KU(ctypes.Union):
    _fields_ = [("mi", MOUSEINPUT), ("ki", KEYBDINPUT)]


class KINPUT(ctypes.Structure):
    _fields_ = [("type", ctypes.c_uint), ("u", _KU)]


def clip_files():
    """直接读系统剪贴板里的 CF_HDROP —— 复制到底有没有写进去，一目了然。"""
    sh = ctypes.WinDLL("shell32", use_last_error=True)
    u.OpenClipboard.argtypes = [wt.HWND]
    u.OpenClipboard.restype = wt.BOOL
    u.GetClipboardData.restype = wt.HANDLE
    u.GetClipboardData.argtypes = [ctypes.c_uint]
    sh.DragQueryFileW.argtypes = [wt.HANDLE, ctypes.c_uint, ctypes.c_void_p, ctypes.c_uint]
    sh.DragQueryFileW.restype = ctypes.c_uint
    if not u.OpenClipboard(None):
        return None
    try:
        hdrop = u.GetClipboardData(15)  # CF_HDROP
        if not hdrop:
            return []
        n = sh.DragQueryFileW(hdrop, 0xFFFFFFFF, None, 0)
        out = []
        for i in range(n):
            buf = ctypes.create_unicode_buffer(300)
            sh.DragQueryFileW(hdrop, i, buf, 300)
            out.append(buf.value)
        return out
    finally:
        u.CloseClipboard()


def _ksend(vk, up):
    arr = (KINPUT * 1)(KINPUT(1, _KU(ki=KEYBDINPUT(vk, 0, 0x0002 if up else 0, 0, None))))
    u.SendInput(1, ctypes.byref(arr), ctypes.sizeof(KINPUT))
    time.sleep(0.08)


def addr_edit(hwnd):
    """找到目录栏那个 Edit（靠上、且很宽的那个）。"""
    found = []
    ENUMCB4 = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)

    @ENUMCB4
    def cb(h, lp):
        b = ctypes.create_unicode_buffer(64)
        u.GetClassNameW(h, b, 64)
        if b.value == "Edit" and u.IsWindowVisible(h):
            r = wt.RECT()
            u.GetWindowRect(h, ctypes.byref(r))
            found.append((int(h), (r.left - 60, r.top - 60, r.right - 60, r.bottom - 60)))
        return True

    u.EnumChildWindows(hwnd, cb, 0)
    for hh, r in found:
        # 目录栏在左侧（x≈106），搜索框在右侧（x≈1004），靠左沿区分
        if r[1] < 90 and r[0] < 500:
            return hh, r
    print("    目录栏候选:", found)
    return None, None


def type_into(hwnd, text):
    """往 Edit 里**打字**：跨进程 WM_SETTEXT 改不到被测进程自己读的那份文本。"""
    u.SendMessageW.restype = ctypes.c_ssize_t
    u.SendMessageW.argtypes = [wt.HWND, ctypes.c_uint, ctypes.c_size_t, ctypes.c_ssize_t]
    u.SendMessageW(hwnd, 0x00B1, 0, -1)  # EM_SETSEL 全选
    for ch in text:
        u.SendMessageW(hwnd, 0x0102, ord(ch), 0)  # WM_CHAR
        time.sleep(0.01)


def menu_row_centers(mh):
    """量出菜单每一行的中心 y（窗口内坐标）。

    行高随 DPI 放大（26×1.25≈33），拍脑袋写 26 会整体错位一整行 ——
    点"复制"点到"重命名"上去就是这么来的。
    """
    raw, stride, w, h = grab(mh)
    bands = ink_rows(raw, stride, 30, w - 8, 2, h - 2, thr=0x70, merge=4)
    return [(b[0] + b[1]) // 2 for b in bands if b[1] - b[0] >= 10]


def clip_effect():
    """读剪贴板上的 Preferred DropEffect：1=复制 2=剪切。"""
    u.RegisterClipboardFormatW.restype = ctypes.c_uint
    u.RegisterClipboardFormatW.argtypes = [ctypes.c_wchar_p]
    u.CloseClipboard.restype = wt.BOOL
    k32 = ctypes.WinDLL("kernel32", use_last_error=True)
    k32.GlobalLock.restype = ctypes.c_void_p
    k32.GlobalLock.argtypes = [wt.HANDLE]
    k32.GlobalUnlock.restype = wt.BOOL
    k32.GlobalUnlock.argtypes = [wt.HANDLE]
    if not u.OpenClipboard(None):
        return None
    try:
        fmt = u.RegisterClipboardFormatW("Preferred DropEffect")
        h = u.GetClipboardData(fmt)
        if not h:
            return None
        p = k32.GlobalLock(h)
        if not p:
            return None
        val = ctypes.cast(p, ctypes.POINTER(ctypes.c_uint)).contents.value
        k32.GlobalUnlock(h)
        return val
    finally:
        u.CloseClipboard()


def key(vk):
    """真键盘：按下 + 抬起。"""
    _ksend(vk, False)
    _ksend(vk, True)


def ctrl_key(vk):
    _ksend(0x11, False)
    _ksend(vk, False)
    _ksend(vk, True)
    _ksend(0x11, True)


def rclick(x, y):
    """真鼠标右键（WM_NCHITTEST 会真跑一遍，等同人手操作）。"""
    move(x, y)
    for fl in (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP):
        _send([INPUT(0, _U(mi=MOUSEINPUT(0, 0, 0, fl, 0, None)))])
        time.sleep(0.06)


ENUMCB = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)


def menus():
    """当前所有可见的自绘菜单窗口（子菜单也是同一个类名）。"""
    out = []

    @ENUMCB
    def cb(h, lp):
        b = ctypes.create_unicode_buffer(64)
        u.GetClassNameW(h, b, 64)
        if b.value == "ZenFileMenu" and u.IsWindowVisible(h):
            r = wt.RECT()
            u.GetWindowRect(h, ctypes.byref(r))
            out.append((int(h), (r.left, r.top, r.right, r.bottom)))
        return True

    u.EnumWindows(cb, 0)
    return out


def launch(arg=None):
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
    u.SetWindowPos(hw, None, 60, 60, W_WIN, H_WIN, 0x0004)
    time.sleep(2.2)
    u.SetForegroundWindow(hw)
    time.sleep(0.8)
    return hw


def grab(hwnd, tries=8):
    raw = stride = w = h = None
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
        g.DeleteObject(bmp)
        g.DeleteDC(mdc)
        u.ReleaseDC(hwnd, src)
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


print("== A. 侧栏：分组空隙 / 折叠按钮已删 ==")
hw = launch(BASE)
raw, stride, w, h = grab(hw)
# 侧栏行从第 0 行起，行高 34（scale=1）。找所有"有内容"的行带。
rows = ink_rows(raw, stride, 4, SIDEBAR_W - 4, VIEW_TOP + 2, h - 40, thr=0x70, merge=8)
print("    侧栏行带:", rows[:14])
mark(len(rows) >= 4, f"侧栏至少 4 组内容（实测 {len(rows)} 段）")

# 空隙：相邻两段之间应有 >= 8px 的纯空白
gaps = [(rows[i + 1][0] - rows[i][1]) for i in range(len(rows) - 1)]
big = [gp for gp in gaps if gp >= 8]
print("    行间空隙:", gaps)
mark(len(big) >= 2, f"至少两处分组空隙 >= 8px（实测 {big}）")

# 折叠按钮已删：侧栏右上角（宽 40 高 24 的那一格）不应再有一个独立方块
# 折叠按钮原本在侧栏顶行右侧，现在整块应当是导航窗格底色
corner = count(raw, stride, SIDEBAR_W - 40, SIDEBAR_W - 4, VIEW_TOP + 2, VIEW_TOP + 26, CHROME_BG, tol=10)
corner_px = (40 - 4) * (26 - 2)
mark(corner >= corner_px * 0.9, f"侧栏右上没有折叠按钮（底色像素 {corner}/{corner_px}）")

print("== B. 空文件夹提示居中（G21） ==")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.45)
hw = launch(os.path.join(BASE, "empty_dir"))
raw, stride, w, h = grab(hw)
# 内容区水平中线附近应有墨迹，且左右两侧对称留白
mid = (SIDEBAR_W + w) // 2
bands = ink_rows(raw, stride, SIDEBAR_W, w, VIEW_TOP, h - 30, thr=0x70, merge=6)
print("    内容区行带:", bands[:5])
if bands:
    t, b = bands[-1]
    ymid = (t + b) // 2
    left = -1
    right = -1
    for x in range(SIDEBAR_W, w):
        for y in range(t, b):
            if min(px(raw, stride, x, y)) >= 0x70:
                if left < 0:
                    left = x
                right = x
                break
    print(f"    文字横向范围 {left}..{right}，内容区中线 {mid}")
    mark(left > SIDEBAR_W + 10, "提示文字不再贴着内容区左沿（说明不是左对齐）")
    if left > 0 and right > 0:
        mark(abs((left + right) // 2 - mid) <= 40, f"提示文字水平居中（中心 {(left+right)//2} vs {mid}）")
    else:
        mark(False, "没量到提示文字")
else:
    mark(False, "空文件夹里没画出提示")

print("== C. 回收站：列头 / 按钮右对齐 / 悬停 ==")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.45)
hw = launch()
# 点侧栏里的回收站（最后一段内容）
raw, stride, w, h = grab(hw)
rows = ink_rows(raw, stride, 4, SIDEBAR_W - 4, VIEW_TOP + 2, h - 40, thr=0x70, merge=8)
if rows:
    # 抓图坐标是**窗口**坐标，SendInput 要的是屏幕坐标：窗口左上角在 (60,60)
    click(60 + 60, 60 + rows[-1][0] + 12)
# 回收站是**后台**扫描的（几万项要几秒），等它真的列出来再验
time.sleep(3.0)
raw, stride, w, h = grab(hw)
# 列头：工具条 44px 之后那 26px 应当是 surface_alt 的一条带
head_top = VIEW_TOP + 44
head_band = count(raw, stride, SIDEBAR_W + 2, w - 2, head_top + 2, head_top + 24, SURFACE_ALT, tol=10)
band_px = (w - SIDEBAR_W - 4) * 22
mark(head_band >= band_px * 0.6, f"回收站有列头条带（{head_band}/{band_px}）")

# 列头文字墨迹（"名称/原位置/删除时间/大小"）
hdr_ink = 0
for x in range(SIDEBAR_W, w):
    for y in range(head_top, head_top + 26):
        if min(px(raw, stride, x, y)) >= 0x70:
            hdr_ink += 1
mark(hdr_ink >= 120, f"列头有文字（墨迹 {hdr_ink} 像素）")

# 按钮右对齐：三个 surface_alt 小方块，最右一个的右沿应贴近视图右沿
btn_rows = []
for y in range(VIEW_TOP, VIEW_TOP + 44):
    n = count(raw, stride, w - 400, w - 4, y, y + 1, SURFACE_ALT, tol=10)
    btn_rows.append(n)
mark(max(btn_rows) >= 200, f"顶部有按钮条（最宽一行 {max(btn_rows)} px）")
# 右沿 12px 内应能找到按钮底色
right_edge = count(raw, stride, w - 16, w - 10, VIEW_TOP + 8, VIEW_TOP + 38, SURFACE_ALT, tol=10)
mark(right_edge >= 20, f"按钮贴到最右侧（右沿附近底色 {right_edge}）")

# 悬停：鼠标移到第一条数据行上，应出现 row_hover 底色。
# 顶边**不能**写死：title_h/toolbar_h 都随 DPI 放大，scale=1.25 时整块往下挪了
# 20px，写死 74 就会把鼠标放到列头上去（那里没有条目，自然量不到 hover）。
# 直接量"列头条带的底边"，往下一行就是第一条数据。
last_hdr = None
for y in range(90, 300):
    if count(raw, stride, SIDEBAR_W + 2, w - 2, y, y + 1, SURFACE_ALT, tol=10) > (w - SIDEBAR_W) * 0.5:
        last_hdr = y
list_top = (last_hdr + 1) if last_hdr else (VIEW_TOP + 70)
print(f"    列头底边 y={last_hdr}，第一条数据行取 y={list_top + 20}")
move(60 + (SIDEBAR_W + w) // 2, 60 + list_top + 20)
time.sleep(0.6)
raw2, stride2, w2, h2 = grab(hw)
rows_ink = ink_rows(raw2, stride2, SIDEBAR_W + 4, w2 - 4, list_top + 2, list_top + 200, thr=0x70, merge=6)
print("    数据行墨迹段:", rows_ink[:4])
hov = count(raw2, stride2, SIDEBAR_W + 4, w2 - 4, list_top + 2, list_top + 40, ROW_HOVER, tol=10)
mark(hov >= 300, f"回收站条目有 hover 底色（{hov} 像素）")

print("== D. 详细信息最后一列不铺满（G4） ==")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.45)
hw = launch(BASE)
raw, stride, w, h = grab(hw)
# 不假设 DPI：直接量"内容区里最右边的墨迹"离右沿还有多远。
# 列宽是随 DPI 放大的，写死 815 会误判。
rightmost = -1
for x in range(w - 1, SIDEBAR_W, -1):
    if any(min(px(raw, stride, x, y)) >= 0x70 for y in range(VIEW_TOP + 30, VIEW_TOP + 150)):
        rightmost = x
        break
print(f"    最右墨迹 x={rightmost}，窗口宽 {w}")
mark(0 < rightmost < w - 40, f"最后一列不铺满整行（右沿留白 {w - rightmost}px）")

print("== E. 右键「新建」= 系统同款模板，建完自动改名（G15/G17） ==")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.45)
before = set(os.listdir(BASE))
hw = launch(BASE)
# 内容区靠底部的空白处右键（条目只有 3 个，这里一定是空白）
rclick(60 + W_WIN - 520, 60 + H_WIN - 90)
time.sleep(0.9)
ms = menus()
mark(len(ms) >= 1, f"空白处能弹出右键菜单（{len(ms)} 个菜单窗口）")
top = None
for _h, _r in ms:
    if top is None or (_r[2] - _r[0]) > (top[1][2] - top[1][0]):
        top = (_h, _r)
if top is None:
    mark(False, "没抓到菜单窗口")
else:
    # 逐行悬停，找第一个会弹出子菜单的行（右键菜单第一行组就是「新建」）
    sub = None
    for row in range(0, 6):
        y = top[1][1] + 10 + row * 26
        if y > top[1][3] - 4:
            break
        move((top[1][0] + top[1][2]) // 2, y)
        time.sleep(0.3)
        for _h2, _r2 in menus():
            if _r2[0] >= top[1][2] - 6 and (sub is None or (_r2[3] - _r2[1]) > (sub[1][3] - sub[1][1])):
                sub = (_h2, _r2)
        if sub:
            break
    mark(sub is not None, "「新建」能弹出子菜单")
    if sub:
        h_sub = sub[1][3] - sub[1][1]
        rows_sub = h_sub // 30
        # 文件夹 + 本机 ShellNew 模板（本机 .txt/.rtf/.bmp 共 3 个）
        mark(rows_sub >= 3, f"「新建」子菜单含系统模板（高 {h_sub}px ≈ {rows_sub} 行）")
        # 点第二个模板行（第 0 行是「文件夹」）
        click((sub[1][0] + sub[1][2]) // 2, sub[1][1] + 10 + 26)
        time.sleep(1.6)
        after = set(os.listdir(BASE))
        added = [n for n in after - before if not n.startswith("f")]
        print("    新建出来的:", added)
        mark(len(added) >= 1, f"点模板真的建出了文件（{added}）")
        mark(any(n.startswith("新建") for n in added), "新文件名跟系统一样带「新建」前缀")
        # 建完应立刻进入改名：主窗口里出现 Edit 框
        eds = []
        ENUMCB2 = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)

        @ENUMCB2
        def _cb2(h, lp):
            b = ctypes.create_unicode_buffer(64)
            u.GetClassNameW(h, b, 64)
            if b.value == "Edit" and u.IsWindowVisible(h):
                r = wt.RECT()
                u.GetWindowRect(h, ctypes.byref(r))
                # 窗口原点固定在 (60,60)，换算成窗口内坐标
                eds.append((int(h), r.top - 60))
            return True

        u.EnumChildWindows(hw, _cb2, 0)
        print("    可见 Edit 框（窗口内 y）:", [e[1] for e in eds])
        # 地址栏那个 Edit 就在目录栏里（y 很小），改名框必须落在列表区
        mark(any(y > 130 for _, y in eds), f"新建完自动进入改名（列表区里的改名框 {[y for _, y in eds]}）")

print("== F. 右键 复制 → 粘贴 端到端（G8） ==")
# 之前只验过键盘 Ctrl+C/V，没走过"右键菜单"这条路，而用户报的就是那条路。
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.45)
before_f = set(os.listdir(BASE))
hw = launch(BASE)
# Home 键把焦点行定到第 0 行 —— 这样"哪一行是条目"不用靠猜坐标
key(0x24)
time.sleep(0.7)
raw, stride, w, h = grab(hw)
hit_ys = [y for y in range(VIEW_TOP + 20, h - 60)
          if count(raw, stride, SIDEBAR_W + 6, w - 6, y, y + 1, SELECTION, tol=12)
          > (w - SIDEBAR_W) * 0.4]
# 选中色带一定是**连续的一整段**（行高 26×scale≈33）；目录栏上的按钮/边框
# 也会碰巧是同一个灰，但它们只有几行高，取最长那段才不会点到工具栏上去。
bands, run = [], None
for y in hit_ys:
    if run is None:
        run = [y, y]
    elif y - run[1] <= 2:
        run[1] = y
    else:
        bands.append(tuple(run))
        run = [y, y]
if run:
    bands.append(tuple(run))
print("    选中色带候选:", bands)
band = max(bands, key=lambda b: b[1] - b[0]) if bands else None
print("    取中:", band)
mark(band is not None and band[1] - band[0] >= 14, f"Home 能选中第一条（色带 {band}）")
if band:
    row_mid = (band[0] + band[1]) // 2
    rclick(60 + (SIDEBAR_W + w) // 2, 60 + row_mid)
    time.sleep(0.9)
    ms = menus()
    mark(len(ms) >= 1, f"条目右键有菜单（{len(ms)} 个）")
    if ms:
        mh, top = max(ms, key=lambda m: m[1][2] - m[1][0])
        centers = menu_row_centers(mh)
        print("    条目菜单 %d 行，行心 %s" % (len(centers), centers[:8]))
        # 目录恒定排在前面，所以第一行是文件夹 → 没有「打开方式」：0打开 1剪切 2复制
        # （行高随 DPI 放大，行心必须实测，不能拿 26 去乘）
        mark(len(centers) >= 3, f"菜单行数够（{len(centers)}）")
        click((top[0] + top[2]) // 2, top[1] + centers[2])
        time.sleep(0.9)
        cf = clip_files()
        print("    点「复制」后剪贴板:", cf, "DropEffect =", clip_effect())
        mark(bool(cf), f"右键「复制」把文件写进了剪贴板（{cf}）")
        mark(clip_effect() == 1, f"复制的 DropEffect 是 1（复制）")
        # (a) 键盘粘贴
        ctrl_key(0x56)
        time.sleep(3.5)
        add_kb = sorted(set(os.listdir(BASE)) - before_f)
        print("    Ctrl+V 后新增:", add_kb)
        mark(len(add_kb) >= 1, f"Ctrl+V 能粘贴出文件（{add_kb}）")
        # (b) 空白处右键 → 粘贴（背景菜单第 0 行）
        rclick(60 + w - 520, 60 + H_WIN - 90)
        time.sleep(0.9)
        ms2 = menus()
        mark(len(ms2) >= 1, f"空白处右键有菜单（{len(ms2)} 个）")
        if ms2:
            top2 = max(ms2, key=lambda m: m[1][2] - m[1][0])[1]
            print("    背景菜单高 %dpx，顶点 y=%d" % (top2[3] - top2[1], top2[1]))
            click((top2[0] + top2[2]) // 2, top2[1] + 10)
            time.sleep(3.5)
            added = sorted(set(os.listdir(BASE)) - before_f - set(add_kb))
            print("    菜单粘贴后新增:", added)
            mark(len(added) >= 1, f"右键「粘贴」能粘出文件（{added}）")
            # 就地粘贴的副本名要跟资源管理器一致："X - 副本"
            mark(any("副本" in n for n in added), f"副本命名跟系统一致（{added}）")

print("== G. 右键 剪切 → 粘贴到别的目录（G8 另一半） ==")
# 目标目录放在 BASE 外面：粘进自己所在的目录树会触发"不能把文件夹粘进它自己"
DEST = BASE + "_dest"
shutil.rmtree(DEST, ignore_errors=True)
os.makedirs(DEST, exist_ok=True)
before_g = set(os.listdir(BASE))
hw = launch(BASE)
key(0x24)
time.sleep(0.7)
raw, stride, w, h = grab(hw)
hit_ys = [y for y in range(VIEW_TOP + 20, h - 60)
          if count(raw, stride, SIDEBAR_W + 6, w - 6, y, y + 1, SELECTION, tol=12)
          > (w - SIDEBAR_W) * 0.4]
bands, run = [], None
for y in hit_ys:
    if run is None:
        run = [y, y]
    elif y - run[1] <= 2:
        run[1] = y
    else:
        bands.append(tuple(run))
        run = [y, y]
if run:
    bands.append(tuple(run))
band = max(bands, key=lambda b: b[1] - b[0]) if bands else None
print("    选中色带:", band)
if band:
    rclick(60 + (SIDEBAR_W + w) // 2, 60 + (band[0] + band[1]) // 2)
    time.sleep(0.9)
    ms = menus()
    if ms:
        mh, top = max(ms, key=lambda m: m[1][2] - m[1][0])
        centers = menu_row_centers(mh)
        # 第一行是文件夹（目录优先）→ 0打开 1剪切 2复制
        click((top[0] + top[2]) // 2, top[1] + centers[1])
        time.sleep(0.9)
        cf = clip_files()
        print("    点「剪切」后剪贴板:", cf, "DropEffect =", clip_effect())
        mark(bool(cf), f"右键「剪切」把文件写进了剪贴板（{cf}）")
        mark(clip_effect() == 2, "剪切的 DropEffect 是 2（移动）")
        # 目录栏跳到 DEST：先点一下目录栏把它切成输入框（Edit 是进编辑态才建的）
        click(60 + 300, 60 + 54)
        time.sleep(0.6)
        addr, _ = addr_edit(hw)
        if addr:
            type_into(addr, DEST)
            key(0x0D)
            time.sleep(2.2)
            ctrl_key(0x56)
            time.sleep(3.5)
            atxt = ctypes.create_unicode_buffer(300)
            u.GetWindowTextW(addr, atxt, 300)
            print("    目录栏现在是:", atxt.value)
            moved_in = sorted(set(os.listdir(DEST)))
            print("    BASE 现在有:", sorted(os.listdir(BASE)))
            print("    DEST 里现在有:", moved_in)
            mark(len(moved_in) >= 1, f"剪切→粘贴真的把文件搬过去了（{moved_in}）")
            gone = before_g - set(os.listdir(BASE))
            print("    源目录少了:", sorted(gone))
            mark(any(n in moved_in for n in gone), f"剪切是移动不是复制（源已不在：{sorted(gone)}）")
        else:
            mark(False, "没找到目录栏输入框")

subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
print("\n=== 第 17 轮回归：", "全部通过" if ok_all else "有失败项", "===")
