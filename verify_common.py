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
BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "ZenR18")
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

