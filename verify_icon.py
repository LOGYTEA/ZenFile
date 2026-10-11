"""把 ZenFile 列表里的文件夹图标与真实资源管理器的文件夹图标做 16x16 逐像素对比。"""
import ctypes
import ctypes.wintypes as wt
import struct
import sys

u = ctypes.WinDLL("user32", use_last_error=True)
g = ctypes.WinDLL("gdi32", use_last_error=True)

u.FindWindowW.restype = wt.HWND
u.FindWindowW.argtypes = [ctypes.c_wchar_p, ctypes.c_wchar_p]
u.GetWindowRect.restype = wt.BOOL
u.GetWindowRect.argtypes = [wt.HWND, ctypes.POINTER(wt.RECT)]
u.GetDC.restype = wt.HDC
u.EnumWindows.restype = wt.BOOL
WNDENUMPROC = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)
u.EnumWindows.argtypes = [WNDENUMPROC, wt.LPARAM]
u.GetClassNameW.restype = ctypes.c_int
u.GetClassNameW.argtypes = [wt.HWND, ctypes.c_wchar_p, ctypes.c_int]
u.IsWindowVisible.restype = wt.BOOL
g.CreateCompatibleDC.restype = wt.HDC
g.CreateCompatibleDC.argtypes = [wt.HDC]
g.CreateCompatibleBitmap.restype = wt.HBITMAP
g.CreateCompatibleBitmap.argtypes = [wt.HDC, ctypes.c_int, ctypes.c_int]
g.SelectObject.restype = wt.HGDIOBJ
g.SelectObject.argtypes = [wt.HDC, wt.HGDIOBJ]
g.BitBlt.restype = wt.BOOL
g.BitBlt.argtypes = [wt.HDC, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int,
                     wt.HDC, ctypes.c_int, ctypes.c_int, ctypes.c_uint]
g.GetDIBits.restype = ctypes.c_int
g.GetDIBits.argtypes = [wt.HDC, wt.HBITMAP, ctypes.c_uint, ctypes.c_uint,
                        ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint]


def grab(hwnd, print_window=False):
    """自绘窗口（ZenFile）必须走 PrintWindow(PW_RENDERFULLCONTENT)：
    它的内容全画在一张内存位图上，直接从 GetDC 上 BitBlt 抓到的往往是一帧
    半成品 —— 量出来的"图标"就会是一块实心方块。资源管理器是普通窗口，
    BitBlt 反而更稳（PrintWindow 对 CabinetWClass 不一定买账）。"""
    r = wt.RECT()
    u.GetWindowRect(hwnd, ctypes.byref(r))
    w, h = r.right - r.left, r.bottom - r.top
    hdc = u.GetDC(hwnd)
    mem = g.CreateCompatibleDC(hdc)
    bmp = g.CreateCompatibleBitmap(hdc, w, h)
    g.SelectObject(mem, bmp)
    if print_window:
        u.PrintWindow.restype = wt.BOOL
        u.PrintWindow.argtypes = [wt.HWND, wt.HDC, ctypes.c_uint]
        u.PrintWindow(hwnd, mem, 2)
    else:
        g.BitBlt(mem, 0, 0, w, h, hdc, 0, 0, 0x00CC0020)
    hdr = struct.pack("<IiiHHIIiiII", 40, w, h, 1, 32, 0, w * h, 0, 0, 0, 0)
    buf = ctypes.create_string_buffer(w * h * 4)
    g.GetDIBits(mem, bmp, 0, h, buf, hdr, 0)
    return buf.raw, w, h


def mk(raw, w, h):
    def at(x, y):
        o = ((h - 1 - y) * w + x) * 4
        return (raw[o], raw[o + 1], raw[o + 2])
    return at


def find_folder_icon(raw, w, h, x0, x1, y0, y1):
    x1=min(x1,w-20); y1=min(y1,h-20)
    """在给定矩形内找第一个"黄色文件夹"图标的左上角。"""
    at = mk(raw, w, h)
    bg = at((x0 + x1) // 2, y0 + 3)
    best = None
    for y in range(y0, y1 - 16):
        for x in range(x0, x1 - 16):
            c = at(x, y)
            # 文件夹黄 #FFE896 → BGR (150,232,255)
            if abs(c[0] - 150) < 24 and abs(c[1] - 232) < 24 and abs(c[2] - 255) < 24:
                n = 0
                for dy in range(16):
                    for dx in range(16):
                        cc = at(x + dx, y + dy)
                        if abs(cc[0] - 150) < 40 and abs(cc[1] - 232) < 40 and abs(cc[2] - 255) < 40:
                            n += 1
                if best is None or n > best[0]:
                    best = (n, x, y)
    return best


def shape(at, x, y, bg):
    out = []
    for dy in range(16):
        line = ""
        for dx in range(16):
            c = at(x + dx, y + dy)
            d = max(abs(c[k] - bg[k]) for k in range(3))
            line += "#" if d > 30 else "."
        out.append(line)
    return out


# --- ZenFile ---
# 这是个**手工对照脚本**（要同时开着资源管理器做逐像素比对），但也不能因为
# 没开着 ZenFile 就直接 IndexError —— 自己拉起来一个再抓。
import os as _os
import subprocess as _sp
import time as _t

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\ZenFile.exe"
CMP_DIR = _os.path.join(_os.environ.get("TEMP", r"C:\Windows\Temp"), "ZenIconCmp")
for _i in range(3):
    _os.makedirs(_os.path.join(CMP_DIR, f"folder{_i}"), exist_ok=True)

hw = u.FindWindowW("ZenFileMainWindow", None)
if not hw:
    _sp.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    _t.sleep(0.4)
    _sp.Popen([EXE, CMP_DIR])
    for _ in range(60):
        _t.sleep(0.25)
        hw = u.FindWindowW("ZenFileMainWindow", None)
        if hw:
            _t.sleep(1.8)
            break
if not hw:
    print("ZenFile 起不来")
    sys.exit(1)
raw, w, h = grab(hw, print_window=True)
if w < 400 or h < 300:
    print(f"抓到的窗口尺寸不对（{w}x{h}）：BitBlt 对自绘双缓冲窗口拿不到内容，"
          f"请确认 ZenFile 已在前台可见")
    sys.exit(1)
at = mk(raw, w, h)
res = find_folder_icon(raw, w, h, 157, 400, 100, 500)
if not res:
    print("ZenFile 未找到文件夹图标")
    sys.exit(1)
n, nx, ny = res
bg_n = at(nx - 6, ny)
s_n = shape(at, nx, ny, bg_n)
print(f"ZenFile 文件夹图标 @({nx},{ny}) 命中 {n}/256")
for l in s_n:
    print("  " + l)

# --- Explorer ---
found = []


def cb(hwnd, lp):
    if u.IsWindowVisible(hwnd):
        b = ctypes.create_unicode_buffer(256)
        u.GetClassNameW(hwnd, b, 256)
        if b.value == "CabinetWClass":
            # 最小化的窗口矩形是 320x210 的"图标标题"尺寸而非真实客户区，
            # BitBlt 出来全黑，量出来的 w/h 也和业务无关，直接跳过。
            r = wt.RECT()
            u.GetWindowRect(hwnd, ctypes.byref(r))
            if r.right - r.left >= 400 and r.bottom - r.top >= 300:
                found.append(hwnd)
    return True


u.EnumWindows(WNDENUMPROC(cb), 0)
if not found:
    print("\n（未打开资源管理器或窗口被最小化，跳过对比）")
    sys.exit(0)
raw2, w2, h2 = grab(found[0])
if w2 < 400 or h2 < 300:
    print(f"\n（资源管理器窗口尺寸 {w2}x{h2} 不可用，跳过对比）")
    sys.exit(0)
at2 = mk(raw2, w2, h2)
res2 = find_folder_icon(raw2, w2, h2, 157, min(w2-20,600), 100, min(h2-20,600))
if not res2:
    print("\nExplorer 未找到文件夹图标")
    sys.exit(0)
n2, ex, ey = res2
bg_e = at2(ex - 6, ey)
s_e = shape(at2, ex, ey, bg_e)
print(f"\nExplorer 文件夹图标 @({ex},{ey}) 命中 {n2}/256")
for l in s_e:
    print("  " + l)

diff = sum(1 for i in range(16) for j in range(16) if s_n[i][j] != s_e[i][j])
print(f"\n形状逐像素差异：{diff}/256 = {100.0*diff/256:.0f}%")
print("判定：", "PASS 与资源管理器一致" if diff <= 12 else "FAIL 形状不一致")
