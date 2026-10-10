"""端到端验证「更多菜单」：点标题栏菜单按钮 → 菜单弹出 → 点「隐藏导航窗格」→ 侧栏真的消失。"""
import ctypes
import ctypes.wintypes as wt
import os
import struct
import subprocess
import time
import sys

u = ctypes.WinDLL("user32", use_last_error=True)
g = ctypes.WinDLL("gdi32", use_last_error=True)

u.FindWindowW.restype = wt.HWND
u.FindWindowW.argtypes = [ctypes.c_wchar_p, ctypes.c_wchar_p]
u.GetWindowRect.restype = wt.BOOL
u.GetWindowRect.argtypes = [wt.HWND, ctypes.POINTER(wt.RECT)]
u.GetDC.restype = wt.HDC
u.PostMessageW.restype = ctypes.c_bool
u.PostMessageW.argtypes = [wt.HWND, ctypes.c_uint, ctypes.c_ulong, ctypes.c_long]
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

WM_LBUTTONUP = 0x0202

# 自己拉起一个实例再测，别依赖"刚好有人开着一个窗口"
EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\ZenFile.exe"
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.6)
subprocess.Popen([EXE, os.path.join(os.environ["USERPROFILE"], "Desktop")])
hwnd = None
for _ in range(80):
    time.sleep(0.25)
    hwnd = u.FindWindowW("ZenFileMainWindow", None)
    if hwnd:
        break
assert hwnd, "找不到主窗口"
time.sleep(2.5)


def grab(hw):
    r = wt.RECT()
    u.GetWindowRect(hw, ctypes.byref(r))
    w, h = r.right - r.left, r.bottom - r.top
    hdc = u.GetDC(hw)
    mem = g.CreateCompatibleDC(hdc)
    bmp = g.CreateCompatibleBitmap(hdc, w, h)
    g.SelectObject(mem, bmp)
    g.BitBlt(mem, 0, 0, w, h, hdc, 0, 0, 0x00CC0020)
    hdr = struct.pack("<IiiHHIIiiII", 40, w, h, 1, 32, 0, w * h, 0, 0, 0, 0)
    buf = ctypes.create_string_buffer(w * h * 4)
    g.GetDIBits(mem, bmp, 0, h, buf, hdr, 0)
    return buf.raw, w, h


def nav_present():
    raw, w, h = grab(hwnd)
    o = ((h - 1 - 300) * w + 80) * 4
    c = (raw[o], raw[o + 1], raw[o + 2])
    return "#%02X%02X%02X" % (c[2], c[1], c[0])


print("1) 初始导航窗格取色:", nav_present())

# 点菜单按钮（x = w - 44*4 + 20, y = 16）
_, w, _ = grab(hwnd)
u.PostMessageW(hwnd, WM_LBUTTONUP, 0, (16 << 16) | (w - 44 * 4 + 20))
# 轮询等菜单出现：固定 sleep 偶发会赶在窗口创建完成之前就去查
menu = None
for _ in range(30):
    time.sleep(0.1)
    menu = u.FindWindowW("ZenFileMenu", None)
    if menu:
        break
print("2) 菜单弹出:", bool(menu))
if not menu:
    sys.exit(1)

# 菜单行索引 6 = 「隐藏导航窗格」：4 + 28*2 + 6*2 + 28*2 = 128 → 中心 142
u.PostMessageW(menu, WM_LBUTTONUP, 0, (142 << 16) | 40)
time.sleep(0.8)
after = nav_present()
print("3) 点击「隐藏导航窗格」后取色:", after)
print("   菜单已关闭:", not u.FindWindowW("ZenFileMenu", None))

ok = after != "#191919"
print("\n判定:", "PASS 侧栏已隐藏" if ok else "FAIL 侧栏仍在")

# 恢复
u.PostMessageW(hwnd, WM_LBUTTONUP, 0, (16 << 16) | 12)
time.sleep(0.6)
print("4) 再次点侧栏开关恢复:", nav_present())
