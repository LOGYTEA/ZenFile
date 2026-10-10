"""验证第 12 轮第 12 / 13 项：左键拖条目落地、Ctrl=复制、拖到面包屑、空白处框选。

行几何全部按源码常量算（资源管理器实测值）：
    title_h=34  toolbar_h=40  表头=26  details 行高=21  侧栏宽=157

  1) 拖文件到同级的文件夹行 -> 真的移过去了（源不在、目标在）
  2) 按住 Ctrl 拖 -> 变成复制（源还在、目标也在）
  3) 拖到地址栏某一级面包屑 -> 移动到那一层（第 12 项）
  4) 空白处按住左键拉框 -> 框住的若干行被选中（按选中底色统计行数）
  5) 单纯单击不该触发拖拽（阈值生效）
素材只在 %TEMP%。
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

g.CreateCompatibleDC.argtypes = [wt.HDC]
g.CreateCompatibleDC.restype = wt.HDC
g.CreateCompatibleBitmap.restype = wt.HBITMAP
g.CreateCompatibleBitmap.argtypes = [wt.HDC, ctypes.c_int, ctypes.c_int]
g.SelectObject.argtypes = [wt.HDC, wt.HGDIOBJ]
g.SelectObject.restype = wt.HGDIOBJ
g.GetDIBits.restype = ctypes.c_int
g.GetDIBits.argtypes = [wt.HDC, wt.HBITMAP, ctypes.c_uint, ctypes.c_uint,
                        ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint]
g.DeleteObject.argtypes = [wt.HGDIOBJ]
g.DeleteDC.argtypes = [wt.HDC]
u.ReleaseDC.argtypes = [wt.HWND, wt.HDC]

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\ZenFile.exe"
WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE = 0x0201, 0x0202, 0x0200
MK_CONTROL = 0x0008
SEL = (0x47, 0x47, 0x47)
HOVER = (0x3D, 0x3D, 0x3D)

SIDEBAR_W = 157
VIEW_X = SIDEBAR_W
ROW_Y0 = 34 + 40 + 26      # 表头之下第一行
ROW_H = 21
AY0 = 34 + 6               # 地址框顶
COL_X = VIEW_X + 200       # 名称列里的一个 x

BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "ZenDrag")

ok_all = True


def mark(ok, msg):
    global ok_all
    ok_all = ok_all and ok
    print(f"    {'PASS' if ok else 'FAIL'}  {msg}")


def rowy(i):
    return ROW_Y0 + i * ROW_H + 8


def fresh(many=True):
    """重建素材。目录优先排序，所以行序是：deep(0) dst(1) alpha(2) beta(3) ..."""
    if os.path.isdir(BASE):
        shutil.rmtree(BASE, ignore_errors=True)
    os.makedirs(os.path.join(BASE, "dst"), exist_ok=True)
    os.makedirs(os.path.join(BASE, "deep", "inner"), exist_ok=True)
    names = ["alpha.txt", "beta.txt", "gamma.txt", "delta.txt"] if many else ["alpha.txt"]
    for n in names:
        open(os.path.join(BASE, n), "w").write(n)


def launch(arg):
    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    time.sleep(0.5)
    subprocess.Popen([EXE, arg])
    for _ in range(60):
        time.sleep(0.25)
        hw = u.FindWindowW("ZenFileMainWindow", None)
        if hw:
            time.sleep(1.5)
            return hw
    return None


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
    g.DeleteObject(bmp)
    g.DeleteDC(mdc)
    u.ReleaseDC(hwnd, src)
    return buf.raw, stride, w, h


def px(raw, stride, x, y):
    o = y * stride + x * 4
    return (raw[o + 2], raw[o + 1], raw[o])


def drag(hwnd, x0, y0, x1, y1, ctrl=False):
    mk = MK_CONTROL if ctrl else 0
    u.PostMessageW(hwnd, WM_LBUTTONDOWN, mk, (y0 << 16) | (x0 & 0xFFFF))
    time.sleep(0.1)
    for i in range(1, 9):
        xx = x0 + (x1 - x0) * i // 8
        yy = y0 + (y1 - y0) * i // 8
        u.PostMessageW(hwnd, WM_MOUSEMOVE, mk, (yy << 16) | (xx & 0xFFFF))
        time.sleep(0.04)
    u.PostMessageW(hwnd, WM_LBUTTONUP, 0, (y1 << 16) | (x1 & 0xFFFF))
    time.sleep(1.3)


def selected_rows(raw, stride, w, y0, y1):
    """内容区里被刷成选中底色的行数（按行统计，不按像素）。"""
    rows = 0
    y = y0
    while y < y1:
        n = sum(1 for x in range(VIEW_X + 40, VIEW_X + 300, 6) if px(raw, stride, x, y) == SEL)
        if n >= 8:
            rows += 1
            y += ROW_H
        else:
            y += 2
    return rows


print("=" * 70)
print("左键拖拽 / 框选")

# ---------- 1) 拖到同级文件夹（移动）：第 2 行 alpha -> 第 1 行 dst ----------
fresh()
hw = launch(BASE)
drag(hw, COL_X, rowy(2), COL_X, rowy(1))
print(f"  alpha.txt 在 BASE: {os.path.exists(os.path.join(BASE, 'alpha.txt'))}"
      f" / 在 dst: {os.path.exists(os.path.join(BASE, 'dst', 'alpha.txt'))}")
mark(not os.path.exists(os.path.join(BASE, "alpha.txt"))
     and os.path.exists(os.path.join(BASE, "dst", "alpha.txt")),
     "拖到同级文件夹 = 移动（源不在、目标在）")

# ---------- 2) Ctrl 拖 = 复制：第 3 行 beta -> 第 1 行 dst ----------
fresh()
hw = launch(BASE)
drag(hw, COL_X, rowy(3), COL_X, rowy(1), ctrl=True)
print(f"  beta.txt 在 BASE: {os.path.exists(os.path.join(BASE, 'beta.txt'))}"
      f" / 在 dst: {os.path.exists(os.path.join(BASE, 'dst', 'beta.txt'))}")
mark(os.path.exists(os.path.join(BASE, "beta.txt"))
     and os.path.exists(os.path.join(BASE, "dst", "beta.txt")),
     "按住 Ctrl 拖 = 复制（源和目标都在）")

# ---------- 3) 拖到面包屑（上一级） ----------
fresh(many=False)
open(os.path.join(BASE, "deep", "carry.txt"), "w").write("x")
hw = launch(os.path.join(BASE, "deep"))
# 用悬停高亮反推每一节的矩形，取倒数第二节（= BASE）
bands = []
x = VIEW_X + 40
while x < 900:
    u.PostMessageW(hw, WM_MOUSEMOVE, 0, ((AY0 + 10) << 16) | (x & 0xFFFF))
    time.sleep(0.08)
    rr, ss, _, _ = grab(hw)
    if px(rr, ss, x, AY0 + 3) == HOVER:
        lo = hi = x
        while lo - 1 > 0 and px(rr, ss, lo - 1, AY0 + 3) == HOVER:
            lo -= 1
        while hi + 1 < 900 and px(rr, ss, hi + 1, AY0 + 3) == HOVER:
            hi += 1
        if not any(abs(lo - b[0]) < 4 for b in bands):
            bands.append((lo, hi + 1))
        x = hi + 3
    else:
        x += 6
print(f"  面包屑 {len(bands)} 节")
mark(len(bands) >= 4, f"地址栏拆出了 {len(bands)} 节面包屑")
if len(bands) >= 2:
    tx = (bands[-2][0] + bands[-2][1]) // 2
    # deep 里：第 0 行 inner（目录），第 1 行 carry.txt
    drag(hw, COL_X, rowy(1), tx, AY0 + 10)
    moved = os.path.exists(os.path.join(BASE, "carry.txt"))
    print(f"  carry.txt 到了 BASE: {moved}")
    mark(moved, "拖到面包屑那一节 = 移动到该级目录")

# ---------- 4) 空白处框选 ----------
fresh()
hw = launch(BASE)
raw, stride, W, H = grab(hw)
before = selected_rows(raw, stride, W, ROW_Y0, rowy(5) + 4)
drag(hw, COL_X, rowy(5) + 30, COL_X, rowy(0))
raw, stride, W, H = grab(hw)
after = selected_rows(raw, stride, W, ROW_Y0, rowy(5) + 4)
print(f"  框选前后选中行数：{before} -> {after}")
mark(after >= 4, f"空白处拉框真的选中了多行（{after} 行）")

# ---------- 5) 单纯单击不该当拖拽 ----------
fresh()
hw = launch(BASE)
drag(hw, COL_X, rowy(2), COL_X + 3, rowy(2) + 2)
print(f"  微移之后 alpha.txt 还在 BASE: {os.path.exists(os.path.join(BASE, 'alpha.txt'))}")
mark(os.path.exists(os.path.join(BASE, "alpha.txt")), "只挪 3px 不算拖拽（阈值生效）")

subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
print("\n" + "=" * 70)
print("拖拽/框选全部通过" if ok_all else "存在失败项")
