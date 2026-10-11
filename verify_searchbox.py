"""验证第 12 轮第 10 项：搜索框内放大镜 + placeholder，目录栏下沿加载进度条。

  1) 空态：搜索框左侧 26px 内有放大镜墨迹
  2) 空态：放大镜右边有 placeholder 文字墨迹
  3) 打字后 placeholder 消失（右侧墨迹切换成真正的输入文字）
  4) 打字后文字起点在 x>=24（EM_SETMARGINS 生效，没被图标压住）
  5) 加载时目录栏下沿出现 accent 色进度条，而不是纯文字
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
u.GetDlgItem.restype = wt.HWND
u.SendMessageW.restype = ctypes.c_ssize_t
u.SendMessageW.argtypes = [wt.HWND, ctypes.c_uint, ctypes.c_void_p, ctypes.c_void_p]
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
WM_CHAR = 0x0102
EM_SETSEL = 0x00B1
ACCENT = (0x43, 0x75, 0xCE)

BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "ZenSearchBox")
os.makedirs(BASE, exist_ok=True)
for i in range(4):
    open(os.path.join(BASE, f"alpha{i}.txt"), "w").write("x")
os.makedirs(os.path.join(BASE, "beta"), exist_ok=True)

ok_all = True


def mark(ok, msg):
    global ok_all
    ok_all = ok_all and ok
    print(f"    {'PASS' if ok else 'FAIL'}  {msg}")


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


def ink(raw, stride, x0, x1, y0, y1, bg=(0x2B, 0x2B, 0x2B), tol=26):
    n = 0
    for y in range(y0, y1):
        for x in range(x0, x1):
            p = px(raw, stride, x, y)
            if abs(p[0] - bg[0]) + abs(p[1] - bg[1]) + abs(p[2] - bg[2]) > tol:
                n += 1
    return n


def type_text(hwnd, edit, text):
    u.SendMessageW(edit, EM_SETSEL, ctypes.c_void_p(0), ctypes.c_void_p(-1))
    for ch in text:
        u.PostMessageW(edit, WM_CHAR, ord(ch), 0)
        time.sleep(0.02)
    time.sleep(0.4)


print("=" * 70)
print("搜索框装饰 + 加载进度条")
hw = launch(BASE)
r = wt.RECT()
u.GetWindowRect(hw, ctypes.byref(r))
W = r.right - r.left
# 与 paint.rs compute_layout 一致：search_w=170, bulk_w=132, pad=6,
# title_h=34, toolbar_h=40。第 15 轮目录栏最右侧多了「批量重命名」按钮（132px），
# 搜索框整体左移，这里必须跟着改 —— 否则量的是按钮那一块，墨迹当然是 0。
SX0, SX1 = W - 6 - 170 - 132 - 8, W - 6 - 132 - 8
SY0, SY1 = 34 + 6, 34 + 40 - 6
TB_BOTTOM = 34 + 40
ed_search = u.GetDlgItem(hw, 102)
print(f"  窗口宽 {W}，搜索框 x {SX0}..{SX1}  y {SY0}..{SY1}")

raw, stride, w, h = grab(hw)
icon_ink = ink(raw, stride, SX0 + 4, SX0 + 24, SY0 + 4, SY1 - 4)
hint_ink = ink(raw, stride, SX0 + 26, SX1 - 8, SY0 + 4, SY1 - 4)
print(f"  空态：放大镜区墨迹 {icon_ink}px，提示文字区墨迹 {hint_ink}px")
mark(icon_ink >= 40, f"搜索框左侧画出了放大镜（{icon_ink}px 墨迹）")
mark(hint_ink >= 60, f"空态显示了 placeholder 文字（{hint_ink}px 墨迹）")

type_text(hw, ed_search, "alpha")
raw2, stride2, _, _ = grab(hw)
after_icon = ink(raw2, stride2, SX0 + 4, SX0 + 24, SY0 + 4, SY1 - 4)
after_left = ink(raw2, stride2, SX0 + 2, SX0 + 22, SY0 + 4, SY1 - 4)
after_text = ink(raw2, stride2, SX0 + 26, SX1 - 8, SY0 + 4, SY1 - 4)
print(f"  输入 'alpha' 后：图标区 {after_icon}px，文字区 {after_text}px")
mark(after_text >= 60, f"输入后框里真的有文字（{after_text}px）")
mark(abs(after_icon - icon_ink) <= 12, "放大镜还在（没被输入内容顶掉）")
# 文字起点必须落在左边距之后：x<22 区域内不该新增文字墨迹
mark(after_left <= icon_ink + 20, f"文字没压在图标区（左侧 {after_left}px ≈ 图标 {icon_ink}px）")

# 清空后 placeholder 应该回来
u.SendMessageW(ed_search, EM_SETSEL, ctypes.c_void_p(0), ctypes.c_void_p(-1))
u.PostMessageW(ed_search, WM_CHAR, 8, 0)  # backspace 不够，直接删整串
for _ in range(8):
    u.PostMessageW(ed_search, WM_CHAR, 8, 0)
time.sleep(0.6)
raw3, stride3, _, _ = grab(hw)
back_hint = ink(raw3, stride3, SX0 + 26, SX1 - 8, SY0 + 4, SY1 - 4)
print(f"  清空后提示文字区墨迹 {back_hint}px")
mark(back_hint >= 60, "清空后 placeholder 重新出现")

# ---- 进度条：进一个超大目录（枚举是异步分批的），抓帧找 accent 色 ----
print("\n[进度条] 打开 C:\\Windows\\WinSxS 期间抓帧")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.5)
subprocess.Popen([EXE, r"C:\Windows\WinSxS"])
hw = None
for _ in range(80):
    time.sleep(0.05)
    hw = u.FindWindowW("ZenFileMainWindow", None)
    if hw:
        break
hits = 0
frames = 0
for _ in range(14):
    rr, ss, ww, hh = grab(hw)
    frames += 1
    n = 0
    for y in range(TB_BOTTOM - 3, TB_BOTTOM):
        if y >= hh:
            break
        for x in range(0, ww):
            p = px(rr, ss, x, y)
            if abs(p[0] - ACCENT[0]) + abs(p[1] - ACCENT[1]) + abs(p[2] - ACCENT[2]) < 60:
                n += 1
    if n > 0:
        hits += 1
        if hits == 1:
            print(f"    第 {frames} 帧命中 {n}px accent 色")
    time.sleep(0.05)
print(f"  {frames} 帧里 {hits} 帧有进度条")
mark(hits >= 1, f"加载时目录栏下沿出现 accent 色进度条（{hits}/{frames} 帧）")

subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
print("\n" + "=" * 70)
print("搜索框/进度条全部通过" if ok_all else "存在失败项")
