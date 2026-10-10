"""菜单悬停展开：回归探针。

背景（真实缺陷）：菜单一 SetCapture，系统会按"光标当前位置"补发一条 WM_MOUSEMOVE。
子菜单照它处理就会认为"用户把鼠标移回父菜单了"，于是刚弹出就自关 —— 表现为
"悬停展开时灵时不灵"，灵不灵取决于物理光标当时恰好在哪。
因此这里把物理光标放到三种位置分别验证，只要有一种挂掉就是缺陷。

素材只在 %TEMP%。
"""
import ctypes
import ctypes.wintypes as wt
import os
import struct
import subprocess
import time

import verify_env

verify_env.isolate()

u = ctypes.WinDLL("user32", use_last_error=True)
g = ctypes.WinDLL("gdi32", use_last_error=True)
u.FindWindowW.restype = wt.HWND
u.FindWindowW.argtypes = [ctypes.c_wchar_p, ctypes.c_wchar_p]
u.GetWindowRect.restype = wt.BOOL
u.GetWindowRect.argtypes = [wt.HWND, ctypes.POINTER(wt.RECT)]
u.GetWindowDC.restype = wt.HDC
u.PrintWindow.restype = wt.BOOL
u.PrintWindow.argtypes = [wt.HWND, wt.HDC, ctypes.c_uint]
u.PostMessageW.restype = ctypes.c_bool
u.PostMessageW.argtypes = [wt.HWND, ctypes.c_uint, ctypes.c_ulong, ctypes.c_long]
u.GetClassNameW.restype = ctypes.c_int
u.GetClassNameW.argtypes = [wt.HWND, ctypes.c_wchar_p, ctypes.c_int]
u.IsWindowVisible.restype = wt.BOOL
u.SetCursorPos.restype = wt.BOOL
u.SetCursorPos.argtypes = [ctypes.c_int, ctypes.c_int]
u.GetCursorPos.restype = wt.BOOL
u.GetCursorPos.argtypes = [ctypes.POINTER(wt.POINT)]
u.EnumWindows.restype = wt.BOOL
ENUMCB = ctypes.WINFUNCTYPE(ctypes.c_bool, wt.HWND, ctypes.c_long)
u.EnumWindows.argtypes = [ENUMCB, ctypes.c_long]
g.CreateCompatibleDC.argtypes = [wt.HDC]
g.CreateCompatibleDC.restype = wt.HDC
g.CreateCompatibleBitmap.restype = wt.HBITMAP
g.CreateCompatibleBitmap.argtypes = [wt.HDC, ctypes.c_int, ctypes.c_int]
g.SelectObject.argtypes = [wt.HDC, wt.HGDIOBJ]
g.SelectObject.restype = wt.HGDIOBJ
g.GetDIBits.restype = ctypes.c_int
g.GetDIBits.argtypes = [wt.HDC, wt.HBITMAP, ctypes.c_uint, ctypes.c_uint,
                        ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint]

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\ZenFile.exe"
WM_MOUSEMOVE = 0x0200
WM_RBUTTONUP = 0x0205

BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "NexMenuProbe")
os.makedirs(BASE, exist_ok=True)
for i in range(6):
    open(os.path.join(BASE, f"f{i}.txt"), "w").write("x" * i)


def windows_of(cls):
    found = []

    @ENUMCB
    def cb(h, _lp):
        if not u.IsWindowVisible(h):
            return True
        c = ctypes.create_unicode_buffer(64)
        u.GetClassNameW(h, c, 64)
        if c.value == cls:
            rr = wt.RECT()
            u.GetWindowRect(h, ctypes.byref(rr))
            found.append((int(h), rr.left, rr.top,
                          rr.right - rr.left, rr.bottom - rr.top))
        return True

    u.EnumWindows(cb, 0)
    return found


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
    hdr = struct.pack("<IiiHHIIiiII", 40, w, -h, 1, 32, 0, stride * h, 0, 0, 0, 0)
    buf = ctypes.create_string_buffer(stride * h)
    g.GetDIBits(mdc, bmp, 0, h, buf, hdr, 0)
    return buf.raw, stride, w, h


def px(raw, stride, x, y):
    o = y * stride + x * 4
    return (raw[o + 2], raw[o + 1], raw[o])


def hx(c):
    return "#%02X%02X%02X" % c


def runs(values, gap=6):
    out, start, prev = [], None, None
    for v in values:
        if start is None:
            start, prev = v, v
        elif v - prev > gap:
            out.append((start, prev))
            start, prev = v, v
        else:
            prev = v
    if start is not None:
        out.append((start, prev))
    return out


ok_all = True


def run_case(tag, cur):
    global ok_all
    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    time.sleep(0.5)
    subprocess.Popen([EXE, BASE])
    hwnd = None
    for _ in range(60):
        time.sleep(0.25)
        hwnd = u.FindWindowW("ZenFileMainWindow", None)
        if hwnd:
            break
    time.sleep(1.5)
    u.SetCursorPos(*cur)
    time.sleep(0.3)
    u.PostMessageW(hwnd, WM_RBUTTONUP, 0, (152 << 16) | (400 & 0xFFFF))
    time.sleep(1.0)
    menus = windows_of("ZenFileMenu")
    mh, ml, mt, mw, mhh = menus[0]
    raw, stride, w, h = grab(mh)
    arrow_ys = [y for y in range(4, h - 4)
                if sum(1 for x in range(w - 26, w - 8)
                       if sum(px(raw, stride, x, y)) > 200) >= 3]
    arrows = [(a + b) // 2 for a, b in runs(arrow_ys) if b - a >= 2]
    pt = wt.POINT()
    u.GetCursorPos(ctypes.byref(pt))
    print(f"\n=== {tag}  物理光标 ({pt.x},{pt.y})  菜单 ({ml},{mt}) {mw}x{mhh}")
    print(f"    箭头行 y = {arrows}")
    for y in arrows[:4]:
        u.PostMessageW(mh, WM_MOUSEMOVE, 0, (y << 16) | (60 & 0xFFFF))
        time.sleep(0.5)
        r2, s2, w2, h2 = grab(mh)
        bg = hx(px(r2, s2, 200, y))
        subs = [x for x in windows_of("ZenFileMenu") if x[0] != mh]
        good = bool(subs) and bg == "#3D3D3D"
        ok_all = ok_all and good
        print(f"    悬停 y={y:3d}: 该行底色 {bg}  子菜单 {len(subs)} 个"
              f" {subs[0][1:] if subs else ''}  -> {'PASS' if good else 'FAIL'}")
        # 收起，避免影响下一行
        u.PostMessageW(mh, WM_MOUSEMOVE, 0, (18 << 16) | (60 & 0xFFFF))
        time.sleep(0.3)


run_case("光标远离菜单（左上角）", (5, 5))
run_case("光标落在菜单内部某行", (600, 500))
run_case("光标在菜单左侧不远处", (300, 400))

print("\n总判定:", "PASS 悬停展开在所有光标位置都成立" if ok_all else "FAIL 仍有位置展开不出来")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
