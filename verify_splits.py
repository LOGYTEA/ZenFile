"""验证第 12 轮第 15 / 16 项：导航窗格宽度可拖、详细信息表头列宽可拖。

判定不靠肉眼：
  1) 导航窗格右分隔线（border 色竖线）的 x 真的跟着拖动走了
  2) 配置里落了盘（settings.user.json 的 ui.sidebar_width）
  3) 表头第 1 条分隔线拖过去之后，"修改日期"那一列表头文字的墨迹整体右移
  4) 列宽同样落盘（files.view.col_name_w）
素材只在 %TEMP%。
"""
import ctypes
import ctypes.wintypes as wt
import json
import os
import struct
import subprocess
import time

import verify_env

verify_env.isolate()
verify_env.clean()

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
BORDER = (0x53, 0x53, 0x53)
APPDATA = os.environ.get("APPDATA", os.path.expanduser("~"))
USER_CFG = os.path.join(APPDATA, "ZenFile", "settings.user.json")

BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "ZenSplits")
os.makedirs(BASE, exist_ok=True)
for i in range(8):
    open(os.path.join(BASE, f"file{i:02d}.txt"), "w").write("x" * (i * 37 + 1))

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


def near(p, q, tol=30):
    return abs(p[0] - q[0]) + abs(p[1] - q[1]) + abs(p[2] - q[2]) <= tol


def drag(hwnd, x0, y, x1):
    u.PostMessageW(hwnd, WM_LBUTTONDOWN, 1, (y << 16) | (x0 & 0xFFFF))
    time.sleep(0.1)
    steps = 8
    for i in range(1, steps + 1):
        xx = x0 + (x1 - x0) * i // steps
        u.PostMessageW(hwnd, WM_MOUSEMOVE, 1, (y << 16) | (xx & 0xFFFF))
        time.sleep(0.05)
    u.PostMessageW(hwnd, WM_LBUTTONUP, 0, (y << 16) | (x1 & 0xFFFF))
    time.sleep(0.6)


def find_vline(raw, stride, w, y0, y1, lo=60, hi=400):
    """在给定 y 段里找那条 border 色竖线：返回命中次数最多的 x。"""
    best, bestn = None, 0
    for x in range(lo, min(hi, w)):
        n = sum(1 for y in range(y0, y1, 4) if near(px(raw, stride, x, y), BORDER))
        if n > bestn:
            best, bestn = x, n
    return best if bestn >= (y1 - y0) // 8 else None


def user_cfg():
    """应用层写的是 settings.json（save_user_patch 的目标），settings.user.json
    是更高优先级的覆盖层 —— 两个都读，后者覆盖前者。"""
    out = {}
    for name in ("settings.json", "settings.user.json"):
        path = os.path.join(APPDATA, "ZenFile", name)
        try:
            with open(path, "r", encoding="utf-8") as f:
                out.update(json.load(f))
        except Exception:
            pass
    return out


print("=" * 70)
print("导航窗格宽度 / 表头列宽 拖拽")

hw = launch(BASE)
raw, stride, W, H = grab(hw)
# 导航窗格竖线在内容区（y 从 toolbar 底 74 到状态栏顶）
Y0, Y1 = 74 + 20, H - 40
x_before = find_vline(raw, stride, W, Y0, Y1)
print(f"  拖动前导航窗格右边界 x={x_before}")
mark(x_before is not None and 140 <= x_before <= 175, f"默认宽度 157（实测竖线 x={x_before}）")

drag(hw, x_before, (Y0 + Y1) // 2, x_before + 90)
raw, stride, W, H = grab(hw)
x_after = find_vline(raw, stride, W, Y0, Y1)
print(f"  拖动后导航窗格右边界 x={x_after}")
mark(x_after is not None and x_after >= x_before + 60,
     f"拖 90px 之后真的变宽了（{x_before} -> {x_after}）")
cfg = user_cfg()
sw = cfg.get("ui", {}).get("sidebar_width")
print(f"  settings.user.json: ui.sidebar_width = {sw}")
mark(isinstance(sw, int) and 200 <= sw <= 260, f"宽度落盘了（{sw}）")

# ---- 表头列宽 ----
print("\n[表头列宽] 拖「名称」列右边线")
hw = launch(BASE)
raw, stride, W, H = grab(hw)
sb = find_vline(raw, stride, W, 74 + 20, H - 40) or 157
# 详细信息：名称列右边线 = view.left + 21 + 16 + 3 + 274（scale=1）
split0 = sb + 21 + 16 + 3 + 274
HEAD_Y = 74 + 13


def header_ink_span(raw, stride, x_lo, x_hi, y0, y1):
    """返回该 x 区间里表头文字墨迹的最左/最右 x（没有就 None）。"""
    lo = hi = None
    for x in range(x_lo, x_hi):
        n = 0
        for y in range(y0, y1):
            p = px(raw, stride, x, y)
            # 表头文字是浅灰 #DEDEDE / #8F8F8F，底色 window_bg
            if p[0] > 0x70 and abs(p[0] - p[1]) < 12 and abs(p[1] - p[2]) < 12:
                n += 1
        if n >= 2:
            if lo is None:
                lo = x
            hi = x
    return lo, hi


# 「修改日期」表头在 split0 之后
lo0, hi0 = header_ink_span(raw, stride, split0 + 2, split0 + 200, 74 + 2, 74 + 24)
print(f"  拖动前「修改日期」表头墨迹 x {lo0}..{hi0}（列分隔线 x={split0}）")
mark(lo0 is not None, "找到了「修改日期」表头文字")

drag(hw, split0, HEAD_Y, split0 + 80)
raw, stride, W, H = grab(hw)
lo1, hi1 = header_ink_span(raw, stride, split0 + 2, W - 2, 74 + 2, 74 + 24)
print(f"  拖动后「修改日期」表头墨迹 x {lo1}..{hi1}")
mark(lo1 is not None and lo1 >= lo0 + 55,
     f"拖 80px 之后列真的变宽了（表头起始 {lo0} -> {lo1}）")
cfg = user_cfg()
cn = cfg.get("files", {}).get("view", {}).get("col_name_w")
print(f"  settings.user.json: files.view.col_name_w = {cn}")
mark(isinstance(cn, int) and 330 <= cn <= 380, f"列宽落盘了（{cn}）")

subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
verify_env.restore()
print("\n" + "=" * 70)
print("分隔线拖拽全部通过" if ok_all else "存在失败项")
