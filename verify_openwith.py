"""验证第 11 轮第 8 项：右键文件 -> 「打开方式」 -> 自绘选择程序弹窗。

判定全用像素 + 窗口事实，不靠肉眼：

  1) 点文件菜单第 1 行（「打开方式」）-> 弹出 `ZenFileOpenWith`
     点**文件夹**菜单第 1 行（那时是「剪切」）-> 不弹出：说明这一行是文件专属
  2) 弹窗不是空板，标题/文件名都画出来了
  3) 列出 >= 2 个候选程序（列表区逐行墨迹条数）
  4) 点某一行 -> 那一行高亮，原来高亮的那行变回去（像素差）
  5) 悬停有 hover 反馈
  6) 「始终使用」复选框可切换
  7) 底部「浏览…/确定/取消」三个按钮都画出来了
  8) 点「确定」-> 弹窗关闭，**并且真的拉起了目标程序**（新顶层窗口出现）
  9) 点「取消」-> 关窗且不拉起任何东西

踩过的坑（都写在这儿免得再犯一次）：
  * 主窗口每次重新 launch 都会把上一次的菜单窗口销毁掉。先开两个菜单再回来点
    第一个，点的就是一个已经死了的 HWND —— 什么都不发生。要**现开现点**。
  * 「文件夹菜单比文件菜单少一行」不成立：Shell 的一级动词是追加在自有条目后面
    的，文件夹那一堆动词本身就比 .txt 的多，高度反而更高。所以只能用"点了弹不弹"
    来判，不能用高度差。
  * 不点「浏览」：那是模态的 GetOpenFileNameW，PostMessage 驱动下会把消息循环卡死。

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
u.GetWindowThreadProcessId.restype = ctypes.c_uint32
u.GetWindowThreadProcessId.argtypes = [wt.HWND, ctypes.POINTER(ctypes.c_uint32)]
u.IsWindowVisible.restype = ctypes.c_bool
u.GetClassNameW.restype = ctypes.c_int
u.GetClassNameW.argtypes = [wt.HWND, ctypes.c_wchar_p, ctypes.c_int]
u.GetWindowTextW.restype = ctypes.c_int
u.GetWindowTextW.argtypes = [wt.HWND, ctypes.c_wchar_p, ctypes.c_int]

g.CreateCompatibleDC.argtypes = [wt.HDC]
g.CreateCompatibleDC.restype = wt.HDC
g.CreateCompatibleBitmap.restype = wt.HBITMAP
g.CreateCompatibleBitmap.argtypes = [wt.HDC, ctypes.c_int, ctypes.c_int]
g.SelectObject.argtypes = [wt.HDC, wt.HGDIOBJ]
g.SelectObject.restype = wt.HGDIOBJ
g.GetDIBits.restype = ctypes.c_int
g.GetDIBits.argtypes = [ctypes.c_void_p, wt.HBITMAP, ctypes.c_uint, ctypes.c_uint,
                        ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint]

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\ZenFile.exe"
WM_LBUTTONDOWN, WM_LBUTTONUP = 0x0201, 0x0202
WM_RBUTTONUP, WM_MOUSEMOVE = 0x0205, 0x0200

BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "NexOpenWith")
# 只放一个文件：右键第 0 行必然是那个文件
FDIR = os.path.join(BASE, "f")
os.makedirs(FDIR, exist_ok=True)
TARGET = os.path.join(FDIR, "a.txt")
open(TARGET, "w").write("hello open with\n")
# 只放一个子文件夹：用来确认「打开方式」是文件专属的
DDIR = os.path.join(BASE, "d")
os.makedirs(DDIR, exist_ok=True)
os.makedirs(os.path.join(DDIR, "sub"), exist_ok=True)

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
    return buf.raw, stride, w, h


def click(hwnd, x, y):
    u.PostMessageW(hwnd, WM_LBUTTONDOWN, 0, (y << 16) | (x & 0xFFFF))
    u.PostMessageW(hwnd, WM_LBUTTONUP, 0, (y << 16) | (x & 0xFFFF))
    time.sleep(0.45)


def move(hwnd, x, y):
    u.PostMessageW(hwnd, WM_MOUSEMOVE, 0, (y << 16) | (x & 0xFFFF))
    time.sleep(0.3)


# 注意：ink / diff / bands 四个参数一律是 (x0, x1, y0, y1)，不是 (x0, y0, x1, y1)
def ink(raw, stride, x0, x1, y0, y1, thr=110):
    n = 0
    for y in range(y0, y1):
        for x in range(x0, x1):
            o = y * stride + x * 4
            if raw[o] + raw[o + 1] + raw[o + 2] > thr * 3:
                n += 1
    return n


def diff(a, b, stride, x0, x1, y0, y1):
    n = 0
    for y in range(y0, y1):
        o = y * stride
        for x in range(x0, x1):
            i = o + x * 4
            if abs(a[i] - b[i]) + abs(a[i + 1] - b[i + 1]) + abs(a[i + 2] - b[i + 2]) > 24:
                n += 1
    return n


def bands(raw, stride, x0, x1, y0, y1, thr=110, minh=6):
    out, run, start = [], 0, 0
    for y in range(y0, y1):
        hit = False
        for x in range(x0, x1):
            o = y * stride + x * 4
            if raw[o] + raw[o + 1] + raw[o + 2] > thr * 3:
                hit = True
                break
        if hit:
            if run == 0:
                start = y
            run += 1
        else:
            if run >= minh:
                out.append((start, y))
            run = 0
    if run >= minh:
        out.append((start, y1))
    return out


ENUMCB = ctypes.WINFUNCTYPE(ctypes.c_bool, wt.HWND, wt.LPARAM)
u.EnumWindows.restype = ctypes.c_bool
u.EnumWindows.argtypes = [ENUMCB, wt.LPARAM]


def windows_of(cls):
    """列出某个类名下所有可见窗口。EnumWindows 回调必须用这里定义好的装饰器。"""
    out = []

    @ENUMCB
    def cb(h, _l):
        buf = ctypes.create_unicode_buffer(64)
        u.GetClassNameW(h, buf, 64)
        if buf.value == cls and u.IsWindowVisible(h):
            r = wt.RECT()
            u.GetWindowRect(h, ctypes.byref(r))
            out.append((h, r.left, r.top, r.right - r.left, r.bottom - r.top))
        return True

    u.EnumWindows(cb, 0)
    return out


def top_level_snapshot():
    """所有可见顶层窗口的 (pid, class, title)，用来判断"真的拉起了程序"。"""
    out = []

    @ENUMCB
    def cb(h, _l):
        if not u.IsWindowVisible(h):
            return True
        cb2 = ctypes.create_unicode_buffer(64)
        u.GetClassNameW(h, cb2, 64)
        tb = ctypes.create_unicode_buffer(256)
        u.GetWindowTextW(h, tb, 256)
        pid = ctypes.c_uint32()
        u.GetWindowThreadProcessId(h, ctypes.byref(pid))
        out.append((pid.value, cb2.value, tb.value))
        return True

    u.EnumWindows(cb, 0)
    return out


def menu_row_height(mh, mw):
    """悬停第 0 行，量出 hover 色带的上下边界 = 内容顶边与行高。"""
    u.PostMessageW(mh, WM_MOUSEMOVE, 0, (10 << 16) | ((mw // 2) & 0xFFFF))
    time.sleep(0.25)
    rm, stm, wm, hm = grab(mh)
    band = [y for y in range(0, hm)
            if all(abs(a - b) <= 8 for a, b in zip(px(rm, stm, mw // 2, y), (0x3D, 0x3D, 0x3D)))]
    return (band[0], band[-1] - band[0] + 1) if band else (4, 28)


def px(raw, stride, x, y):
    o = y * stride + x * 4
    return (raw[o + 2], raw[o + 1], raw[o])


def row_bands(raw, stride, x0, x1, y0, y1):
    out, run, start = [], 0, 0
    for y in range(y0, y1):
        hit = False
        for x in range(x0, x1):
            o = y * stride + x * 4
            if raw[o] + raw[o + 1] + raw[o + 2] > 330:
                hit = True
                break
        if hit:
            if run == 0:
                start = y
            run += 1
        else:
            if run:
                out.append((start, y))
            run = 0
    return out


def fresh_menu_row1(dir_path):
    """现开现点：右键第 0 行 -> 返回菜单句柄。菜单窗口在下次 launch 时就没了。"""
    hw = launch(dir_path)
    if hw is None:
        return None
    raw, stride, w, h = grab(hw)
    bs = [b for b in row_bands(raw, stride, 170, 600, 90, h - 40) if b[1] - b[0] >= 8]
    if not bs:
        return None
    sy = (bs[0][0] + bs[0][1]) // 2
    click(hw, 300, sy)
    time.sleep(0.3)
    u.PostMessageW(hw, WM_RBUTTONUP, 0, (sy << 16) | (300 & 0xFFFF))
    time.sleep(0.8)
    ms = windows_of("ZenFileMenu")
    if len(ms) != 1:
        return None
    return ms[0][0]


def click_menu_row1(dir_path, wait=4.0):
    """点右键菜单的第 1 行，返回是否弹出了「打开方式」窗口。"""
    mh = fresh_menu_row1(dir_path)
    if mh is None:
        return None
    r = wt.RECT()
    u.GetWindowRect(mh, ctypes.byref(r))
    mw = r.right - r.left
    top0, row_h = menu_row_height(mh, mw)
    y = int(top0 + row_h + row_h / 2)
    click(mh, mw // 2, y)
    for _ in range(int(wait / 0.1)):
        time.sleep(0.1)
        ws = windows_of("ZenFileOpenWith")
        if ws:
            return ws[0][0]
    return None


def wait_ow_class(timeout=1.2):
    t0 = time.time()
    while time.time() - t0 < timeout:
        if windows_of("ZenFileOpenWith"):
            return True
        time.sleep(0.1)
    return False


print("=" * 70)
print("右键「打开方式」：菜单项 + 自绘选择程序弹窗")

# ---------------------------------------------------------------- 1. 只文件才有
print("\n[1] 「打开方式」是文件专属的")
ow = click_menu_row1(FDIR)
mark(ow is not None, "点文件菜单第 1 行 -> 弹出 ZenFileOpenWith")
ow_dir = click_menu_row1(DDIR)
mark(ow_dir is None, "点文件夹菜单第 1 行 -> 不弹出（那行是「剪切」）")
if ow is None:
    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    raise SystemExit(1)
# 文件夹那次把窗口冲掉了，重新开一次
ow = click_menu_row1(FDIR)
mark(ow is not None, "重新弹出（后续检查用它）")
if ow is None:
    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    raise SystemExit(1)

# ---------------------------------------------------------------- 2. 不是空板
print("\n[2] 弹窗内容")
# 刚 ShowWindow 的第一帧可能还没画：抓到空板就重抓（踩过）
for _ in range(8):
    raw, stride, W, H = grab(ow)
    if ink(raw, stride, 10, W - 10, 40, H - 40) > 300:
        break
    time.sleep(0.35)
raw, stride, W, H = grab(ow)
base = raw
scale = W / 470.0
print(f"  弹窗 {W}x{H}（scale≈{scale:.2f}）")


def S(v):
    return int(round(v * scale))


mark(ink(base, stride, 10, 200, 0, S(34)) > 60, "标题栏有「打开方式」文字")
mark(ink(base, stride, 10, 300, S(34), S(34) + 26) > 40, "头部显示了文件名")

# ---------------------------------------------------------------- 3. 候选列表
print("\n[3] 候选程序列表")
LIST = (S(12) + 2, W - S(12) - 2, S(34 + 44), H - S(104))
print(f"  列表区 x {LIST[0]}..{LIST[1]}  y {LIST[2]}..{LIST[3]}")
rows = [b for b in bands(base, stride, LIST[0], LIST[1], LIST[2], LIST[3], minh=20)
        if b[1] - b[0] >= int(20 * scale)]
print(f"  列表行带: {rows}")
mark(len(rows) >= 2, f"列出了 {len(rows)} 个候选程序（>=2）")

# ---------------------------------------------------------------- 4. 选中高亮
print("\n[4] 点某一行 -> 那一行高亮")
r0, r1 = rows[0], rows[1]
cy1 = (r1[0] + r1[1]) // 2
click(ow, (LIST[0] + LIST[1]) // 2, cy1)
raw2, stride2, _, _ = grab(ow)
d0 = diff(base, raw2, stride, LIST[0], LIST[1], r0[0], r0[1])
d1 = diff(base, raw2, stride, LIST[0], LIST[1], r1[0], r1[1])
print(f"  点第 1 行：第 0 行变化 {d0} px，第 1 行变化 {d1} px")
mark(d1 > 200, f"被点的第 1 行高亮了（{d1} px 变化）")
mark(d0 > 200, f"原来高亮的第 0 行变回去了（{d0} px 变化）")

# ---------------------------------------------------------------- 5. 悬停
print("\n[5] 悬停")
raw_h = raw2
if len(rows) >= 3:
    r2 = rows[2]
    move(ow, (LIST[0] + LIST[1]) // 2, (r2[0] + r2[1]) // 2)
    raw_h, _, _, _ = grab(ow)
    dh = diff(raw2, raw_h, stride, LIST[0], LIST[1], r2[0], r2[1])
    print(f"  悬停第 2 行变化 {dh} px")
    mark(dh > 100, f"悬停有 hover 反馈（{dh} px）")
else:
    mark(True, "候选不足 3 行，跳过悬停检查")

# ---------------------------------------------------------------- 6. 始终使用
print("\n[6] 「始终使用此应用打开此类文件」")
AR = (S(14), W - S(14), H - S(96), H - S(70))
click(ow, AR[0] + S(8), (AR[2] + AR[3]) // 2)
raw4, _, _, _ = grab(ow)
da = diff(raw_h, raw4, stride, *AR)
print(f"  复选框区变化 {da} px")
mark(da > 60, f"复选框可切换（{da} px）")
click(ow, AR[0] + S(8), (AR[2] + AR[3]) // 2)  # 切回去，别真改注册表
time.sleep(0.3)

# ---------------------------------------------------------------- 7. 按钮
print("\n[7] 底部三个按钮")
BR = (S(14), S(14) + S(120), H - S(62), H - S(62) + S(28))
OKR = (W - S(14) - S(92) - S(8) - S(92), W - S(14) - S(92) - S(8), H - S(44), H - S(44) + S(28))
CNR = (W - S(14) - S(92), W - S(14), H - S(44), H - S(44) + S(28))
raw5, _, _, _ = grab(ow)
for name, r in (("浏览…", BR), ("确定", OKR), ("取消", CNR)):
    iv = ink(raw5, stride, *r)
    print(f"  {name} 墨迹 {iv}")
    mark(iv > 40, f"「{name}」按钮画出来了（{iv} px）")

# ---------------------------------------------------------------- 8. 确定 -> 拉起程序
print("\n[8] 点「确定」-> 关窗并拉起目标程序")
snap_before = set(top_level_snapshot())
click(ow, (OKR[0] + OKR[1]) // 2, (OKR[2] + OKR[3]) // 2)
gone = not wait_ow_class(2.0)
mark(gone, "点「确定」后弹窗关闭")
launched = None
t0 = time.time()
while time.time() - t0 < 6.0:
    new = list(set(top_level_snapshot()) - snap_before)
    if new:
        launched = new
        break
    time.sleep(0.3)
print(f"  新出现的顶层窗口: {[(x[1], x[2][:30]) for x in (launched or [])][:4]}")
mark(launched is not None, f"真的拉起了程序（{launched[0][1] if launched else '无'}）")
if launched:
    for pid, cls, title in launched:
        subprocess.run(["taskkill", "/F", "/PID", str(pid)], capture_output=True)
    time.sleep(0.3)

# ---------------------------------------------------------------- 9. 取消
print("\n[9] 点「取消」-> 关窗且不拉起任何东西")
ow2 = click_menu_row1(FDIR)
mark(ow2 is not None, "再次弹出")
if ow2:
    for _ in range(8):
        raw9, stride9, W9, H9 = grab(ow2)
        if ink(raw9, stride9, 10, W9 - 10, 40, H9 - 40) > 300:
            break
        time.sleep(0.35)
    snap2 = set(top_level_snapshot())
    click(ow2, (CNR[0] + CNR[1]) // 2, (CNR[2] + CNR[3]) // 2)
    gone2 = not wait_ow_class(2.0)
    mark(gone2, "点「取消」后弹窗关闭")
    time.sleep(1.5)
    extra = [x for x in set(top_level_snapshot()) - snap2 if x[1] != "Shell_TrayWnd"]
    print(f"  关闭后新增窗口: {[(x[1], x[2][:30]) for x in extra][:4]}")
    mark(not extra, f"「取消」没有拉起任何程序（新增 {len(extra)} 个）")

subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
print("\n" + "=" * 70)
print("打开方式全部通过" if ok_all else "打开方式存在失败项")
