"""验证第 2 项：首页 Windows 式平铺布局（8 个常用文件夹 + 本机驱动器，平铺）。

脚本**独立重算**磁贴几何（不调用被测代码），再据此点击，避免自证。
跨进程读 EDIT 文本用 VirtualAllocEx + WM_GETTEXT（GetWindowTextW 在这里取不到内容）。
"""
import ctypes
import ctypes.wintypes as wt
import subprocess
import time
import sys

u = ctypes.WinDLL("user32", use_last_error=True)
g = ctypes.WinDLL("gdi32", use_last_error=True)
k = ctypes.WinDLL("kernel32", use_last_error=True)

u.FindWindowW.restype = wt.HWND
u.FindWindowW.argtypes = [ctypes.c_wchar_p, ctypes.c_wchar_p]
u.GetWindowRect.restype = wt.BOOL
u.GetWindowRect.argtypes = [wt.HWND, ctypes.POINTER(wt.RECT)]
u.GetWindowDC.restype = wt.HDC
u.PrintWindow.restype = wt.BOOL
u.PrintWindow.argtypes = [wt.HWND, wt.HDC, ctypes.c_uint]
u.PostMessageW.restype = ctypes.c_bool
u.PostMessageW.argtypes = [wt.HWND, ctypes.c_uint, ctypes.c_ulong, ctypes.c_long]
u.SendMessageW.restype = ctypes.c_ssize_t
u.SendMessageW.argtypes = [wt.HWND, ctypes.c_uint, wt.WPARAM, wt.LPARAM]
u.GetDlgItem.restype = wt.HWND
u.GetWindowThreadProcessId.restype = wt.DWORD
u.GetWindowThreadProcessId.argtypes = [wt.HWND, ctypes.POINTER(wt.DWORD)]
g.CreateCompatibleDC.restype = wt.HDC
g.CreateCompatibleDC.argtypes = [wt.HDC]
g.CreateCompatibleBitmap.restype = wt.HBITMAP
g.CreateCompatibleBitmap.argtypes = [wt.HDC, ctypes.c_int, ctypes.c_int]
g.SelectObject.restype = wt.HGDIOBJ
g.SelectObject.argtypes = [wt.HDC, wt.HGDIOBJ]
g.GetDIBits.restype = ctypes.c_int
g.GetDIBits.argtypes = [wt.HDC, wt.HBITMAP, ctypes.c_uint, ctypes.c_uint,
                        ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint]
k.OpenProcess.restype = wt.HANDLE
k.OpenProcess.argtypes = [wt.DWORD, wt.BOOL, wt.DWORD]
k.VirtualAllocEx.restype = ctypes.c_void_p
k.VirtualAllocEx.argtypes = [wt.HANDLE, ctypes.c_void_p, ctypes.c_size_t, wt.DWORD, wt.DWORD]
k.VirtualFreeEx.restype = wt.BOOL
k.VirtualFreeEx.argtypes = [wt.HANDLE, ctypes.c_void_p, ctypes.c_size_t, wt.DWORD]
k.ReadProcessMemory.restype = wt.BOOL
k.ReadProcessMemory.argtypes = [wt.HANDLE, ctypes.c_void_p, ctypes.c_void_p,
                                ctypes.c_size_t, ctypes.POINTER(ctypes.c_size_t)]

WM_GETTEXT = 0x000D
WM_GETTEXTLENGTH = 0x000E
WM_LBUTTONDBLCLK = 0x0203
WM_LBUTTONDOWN = 0x0201
WM_KEYDOWN = 0x0100
WM_SYSKEYDOWN = 0x0104
VK_MENU, VK_LEFT = 0x12, 0x25
MEM_COMMIT, MEM_RESERVE, MEM_RELEASE = 0x1000, 0x2000, 0x8000
PAGE_READWRITE = 0x04
PROCESS_ALL = 0x1F0FFF

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\fm-app.exe"

VIEW_LEFT, VIEW_TOP = 157, 74
WIN_W = 1180
PAD, GAP = 22, 10
HEADER_H = 34
QUICK_W, QUICK_H = 200, 46
DRIVE_W, DRIVE_H = 250, 86


def remote_edit_text(hwnd_edit):
    """跨进程读 EDIT：WM_GETTEXT 由系统做封送，本地 buffer 即可。
    （GetWindowTextW / GetWindowTextLengthW 跨进程取不到这个控件的内容。）"""
    buf = (ctypes.c_wchar * 512)()
    ptr = ctypes.cast(buf, ctypes.c_void_p).value
    n = u.SendMessageW(hwnd_edit, WM_GETTEXT, 512, ptr)
    return buf.value if n and n > 0 else ""


def grab(hwnd):
    r = wt.RECT()
    u.GetWindowRect(hwnd, ctypes.byref(r))
    w, h = r.right - r.left, r.bottom - r.top
    src = u.GetWindowDC(hwnd)
    mdc = g.CreateCompatibleDC(src)
    bmp = g.CreateCompatibleBitmap(src, w, h)
    g.SelectObject(mdc, bmp)
    u.PrintWindow(hwnd, mdc, 2)
    stride = ((w * 32 + 31) // 32) * 4
    import struct
    hdr = struct.pack("<IiiHHIIiiII", 40, w, -h, 1, 32, 0, stride * h, 0, 0, 0, 0)
    buf = ctypes.create_string_buffer(stride * h)
    g.GetDIBits(mdc, bmp, 0, h, buf, hdr, 0)
    return buf.raw, stride, w, h


def cols_of(avail, tile_w):
    return max(1, (avail + GAP) // (tile_w + GAP))


def quick_tile(i, qcols):
    col, row = i % qcols, i // qcols
    left = VIEW_LEFT + PAD + col * (QUICK_W + GAP)
    top = VIEW_TOP + 10 + HEADER_H + row * (QUICK_H + GAP)
    return left, top, left + QUICK_W, top + QUICK_H


def drive_tile(i, qcols, dcols):
    qrows = max(1, (8 + qcols - 1) // qcols)
    after_q = VIEW_TOP + 10 + HEADER_H + qrows * (QUICK_H + GAP) + 14
    dstart = after_q + HEADER_H
    col, row = i % dcols, i // dcols
    left = VIEW_LEFT + PAD + col * (DRIVE_W + GAP)
    top = dstart + row * (DRIVE_H + GAP)
    return left, top, left + DRIVE_W, top + DRIVE_H


subprocess.Popen([EXE])
hwnd = None
for _ in range(60):
    time.sleep(0.25)
    hwnd = u.FindWindowW("NexFileMainWindow", None)
    if hwnd:
        break
assert hwnd
time.sleep(2.5)
edit = u.GetDlgItem(hwnd, 101)
avail = WIN_W - VIEW_LEFT - PAD * 2
qcols, dcols = cols_of(avail, QUICK_W), cols_of(avail, DRIVE_W)
drives = [c for c in "ABCDEFGHIJKLMNOPQRSTUVWXYZ"
          if k.GetLogicalDrives() >> (ord(c) - 65) & 1]
print(f"内容宽 {avail} → 常用 {qcols} 列 / 驱动器 {dcols} 列；本机盘符 {drives}")

ok = True
print("\n=== 起始页 ===")
home_text = remote_edit_text(edit)
print(f"  地址栏: {home_text!r}")
if home_text != "主页":
    print("  FAIL 没有落在主页")
    ok = False

print("\n=== 每个磁贴都有内容（像素）===")
raw, stride, w, h = grab(hwnd)


def ink(box):
    l, t, r, b = box
    n = 0
    for y in range(t + 2, min(b - 2, h)):
        for x in range(l + 2, min(r - 2, w)):
            o = y * stride + x * 4
            if raw[o] > 0x30 or raw[o + 1] > 0x30 or raw[o + 2] > 0x30:
                n += 1
    return n


bad = [("quick", i) for i in range(8) if ink(quick_tile(i, qcols)) < 40]
bad += [("drive", i) for i in range(len(drives)) if ink(drive_tile(i, qcols, dcols)) < 40]
print(f"  空格 {bad if bad else '无'}")
ok = ok and not bad

print("\n=== 逐个双击常用文件夹磁贴 ===")
expect = ["主文件夹", "库", "桌面", "下载", "文档", "图片", "音乐", "视频"]
for i in range(8):
    l, t, _r, _b = quick_tile(i, qcols)
    u.PostMessageW(hwnd, WM_LBUTTONDBLCLK, 0,
                   ((t + QUICK_H // 2) << 16) | ((l + QUICK_W // 2) & 0xFFFF))
    time.sleep(0.7)
    txt = remote_edit_text(edit)
    lower = txt.lower()
    want = expect[i]
    hit = (want in txt) or (want == "主文件夹" and txt.startswith("C:\\Users")) or (
        {"桌面": "desktop", "下载": "downloads", "文档": "documents", "图片": "pictures",
         "音乐": "music", "视频": "videos", "库": "libraries"}.get(want, want) in lower)
    print(f"  磁贴[{i}] 期望≈{want:5s} → 地址 {txt!r}  {'OK' if hit else 'MISMATCH'}")
    ok = ok and hit
    # 点工具栏"后退"按钮（back rect ≈ (6,38)-(36,68)）回到主页
    u.PostMessageW(hwnd, WM_LBUTTONDOWN, 0, (53 << 16) | 20)
    time.sleep(0.6)
    if remote_edit_text(edit) != "主页":
        print("    (未能退回主页，继续)")

print("\n=== 双击驱动器磁贴 ===")
for i in range(len(drives)):
    l, t, _r, _b = drive_tile(i, qcols, dcols)
    u.PostMessageW(hwnd, WM_LBUTTONDBLCLK, 0,
                   ((t + DRIVE_H // 2) << 16) | ((l + DRIVE_W // 2) & 0xFFFF))
    time.sleep(0.7)
    txt = remote_edit_text(edit)
    hit = txt.upper().startswith(drives[i] + ":\\")
    print(f"  驱动器[{i}] → 地址 {txt!r}  {'OK' if hit else 'MISMATCH'}")
    ok = ok and hit
    u.PostMessageW(hwnd, WM_LBUTTONDOWN, 0, (53 << 16) | 20)
    time.sleep(0.6)

print("\n总判定:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
