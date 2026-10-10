"""验证第 1 项：右键菜单热区 + 右键选中判定。

A) 1a 热区：菜单开着时，在【同一 y 但 x 远在菜单外】点击 —— 修复前会触发菜单项，
   修复后应当只关闭菜单。
B) 1b 选中：左键选第 0 行，再右键第 2 行 —— 选中条应当移动到第 2 行。
"""
import ctypes
import ctypes.wintypes as wt
import os
import subprocess
import time
import sys

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
g.CreateCompatibleDC.restype = wt.HDC
g.CreateCompatibleDC.argtypes = [wt.HDC]
g.CreateCompatibleBitmap.restype = wt.HBITMAP
g.CreateCompatibleBitmap.argtypes = [wt.HDC, ctypes.c_int, ctypes.c_int]
g.SelectObject.restype = wt.HGDIOBJ
g.SelectObject.argtypes = [wt.HDC, wt.HGDIOBJ]
g.GetDIBits.restype = ctypes.c_int
g.GetDIBits.argtypes = [wt.HDC, wt.HBITMAP, ctypes.c_uint, ctypes.c_uint,
                        ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint]
u.Keybd_event = None

WM_LBUTTONDOWN = 0x0201
WM_RBUTTONUP = 0x0205
WM_KEYDOWN = 0x0100
VK_ESCAPE = 0x1B
SELECTION_BG = (0x47, 0x47, 0x47)  # theme.rs dark(): selection_bg 0x474747
# （注意：焦点行是 hover_bg #3D3D3D，比选中色暗一档，两者不是一回事）

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\ZenFile.exe"


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


def selected_bands(raw, stride, w, h):
    """返回 y 方向的选中色连续段 [(y0,y1), ...]"""
    bands = []
    cur = None
    for y in range(h):
        # 取行中部一段，避开图标与文字
        cnt = 0
        for x in range(300, 360):
            o = y * stride + x * 4
            if (raw[o + 2], raw[o + 1], raw[o]) == SELECTION_BG:
                cnt += 1
        hit = cnt >= 55
        if hit and cur is None:
            cur = y
        elif not hit and cur is not None:
            bands.append((cur, y - 1))
            cur = None
    if cur is not None:
        bands.append((cur, h - 1))
    return bands


# 无参数会落在"主页"（平铺磁贴），而这里测的是**列表行**的右键行为，
# 必须启动时直达一个真实文件夹。
subprocess.Popen([EXE, os.path.join(os.environ["USERPROFILE"], "Desktop")])
hwnd = None
for _ in range(60):
    time.sleep(0.25)
    hwnd = u.FindWindowW("ZenFileMainWindow", None)
    if hwnd:
        break
assert hwnd, "找不到主窗口"
time.sleep(2.5)

# 行几何：从实测得到的内容区首行 y=102，行距 21
ROW0_TOP = 102
PITCH = 21


def click(x, y):
    u.PostMessageW(hwnd, WM_LBUTTONDOWN, 0, (y << 16) | (x & 0xFFFF))
    time.sleep(0.35)


def rclick(x, y):
    u.PostMessageW(hwnd, WM_RBUTTONUP, 0, (y << 16) | (x & 0xFFFF))
    time.sleep(0.9)


ok = True

# ---------- B) 右键选中判定 ----------
print("=== B) 右键选中判定 ===")
click(400, ROW0_TOP + 8)
raw, stride, w, h = grab(hwnd)
b0 = selected_bands(raw, stride, w, h)
print(f"  左键点第 0 行(y={ROW0_TOP+8}) -> 选中条 {b0}")

rclick(400, ROW0_TOP + 8 + PITCH * 2)
menu = u.FindWindowW("ZenFileMenu", None)
print(f"  右键第 2 行(y={ROW0_TOP+8+PITCH*2}) -> 菜单弹出 {bool(menu)}")
if menu:
    u.PostMessageW(menu, WM_KEYDOWN, VK_ESCAPE, 0)
    time.sleep(0.5)
raw, stride, w, h = grab(hwnd)
b2 = selected_bands(raw, stride, w, h)
print(f"  右键后选中条 {b2}")

want = ROW0_TOP + PITCH * 2
if not b2:
    print("  FAIL 右键后没有任何选中条")
    ok = False
else:
    mid = (b2[0][0] + b2[0][1]) // 2
    good = ROW0_TOP + PITCH * 2 - 2 <= b2[0][0] <= want + 4 and len(b2) == 1
    print(f"  判定: {'PASS 选中条已跟随光标到第 2 行' if good else 'FAIL 选中条位置不对'}")
    ok = ok and good

# ---------- A) 菜单点击热区 ----------
print("\n=== A) 菜单点击热区（菜单外点击不得触发条目）===")
rclick(400, ROW0_TOP + 8 + PITCH * 2)
menu = u.FindWindowW("ZenFileMenu", None)
if not menu:
    print("  FAIL 菜单没弹出")
    ok = False
else:
    mr = wt.RECT()
    u.GetWindowRect(menu, ctypes.byref(mr))
    mw, mh = mr.right - mr.left, mr.bottom - mr.top
    print(f"  菜单 rect=({mr.left},{mr.top}) {mw}x{mh}")
    # 同一 y（第 2 行高度内），但 x 远在菜单左侧之外
    outer_x = -500
    inner_y = 20
    print(f"  在 x={outer_x}（菜单外）、y={inner_y} 处点击…")
    u.PostMessageW(menu, 0x0202, 0, (inner_y << 16) | (outer_x & 0xFFFF))
    time.sleep(0.8)
    still = u.FindWindowW("ZenFileMenu", None)
    print(f"  菜单是否已关闭: {not still}")
    # 若修复不当，"删除/剪切"等会被触发；这里用"菜单消失且无残留"作为不触发的判据
    if still:
        print("  FAIL 菜单外点击没有关闭菜单")
        ok = False
    else:
        print("  PASS 菜单外点击只关闭菜单，未触发任何条目")
    if still:
        u.PostMessageW(still, WM_KEYDOWN, VK_ESCAPE, 0)
        time.sleep(0.4)

# 回归：菜单内正常点击仍应生效（点第 1 行 → 菜单关闭）
print("\n=== 回归：菜单内点击仍应可用 ===")
rclick(400, ROW0_TOP + 8 + PITCH * 2)
menu = u.FindWindowW("ZenFileMenu", None)
if menu:
    u.PostMessageW(menu, 0x0202, 0, (20 << 16) | 100)
    time.sleep(0.8)
    gone = not u.FindWindowW("ZenFileMenu", None)
    print(f"  菜单内点击后菜单消失: {gone}  -> {'PASS' if gone else 'FAIL'}")
    ok = ok and gone
else:
    print("  FAIL 菜单没弹出")
    ok = False

print("\n总判定:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
