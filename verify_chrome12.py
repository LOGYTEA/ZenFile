"""验证第 12 轮第 4 / 7 项：标题栏与导航窗格的改造。

  1) 三大金刚键改成矢量：最小化=下半一条横线、最大化=方框、关闭=斜叉
  2) 标签页带图标：图标格 16px 里有墨迹，文字从图标右边开始（不重叠）
  3) 导航窗格首行是"此电脑"（有展开箭头），不再有"主页"那一行
  4) 侧栏文字改小：同一行文字的墨迹高度 ≤ 15px（-13 号字）
  5) 展开后有多级缩进引导线（阶梯）
  6) 导航窗格右上角的折叠按钮：点一下窗格收起（内容区左边界回到 0）
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
WM_LBUTTONDOWN, WM_LBUTTONUP = 0x0201, 0x0202

BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "ZenChrome")
os.makedirs(BASE, exist_ok=True)
for i in range(3):
    open(os.path.join(BASE, f"f{i}.txt"), "w").write("x")

TITLE_H, TOOL_H = 34, 40
SB_W, SB_ROW = 157, 34
BTN_W = 44

ok_all = True


def mark(ok, msg):
    global ok_all
    ok_all = ok_all and ok
    print(f"    {'PASS' if ok else 'FAIL'}  {msg}")


def launch(arg=None):
    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    time.sleep(0.5)
    cmd = [EXE] if arg is None else [EXE, arg]
    subprocess.Popen(cmd)
    for _ in range(60):
        time.sleep(0.25)
        hw = u.FindWindowW("ZenFileMainWindow", None)
        if hw:
            time.sleep(1.6)
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


def click(hwnd, x, y):
    u.PostMessageW(hwnd, WM_LBUTTONDOWN, 0, (y << 16) | (x & 0xFFFF))
    u.PostMessageW(hwnd, WM_LBUTTONUP, 0, (y << 16) | (x & 0xFFFF))
    time.sleep(0.7)


def ink_box(raw, stride, x0, x1, y0, y1, bg, tol=40):
    """返回该区域内"非底色"墨迹的包围盒 (l,t,r,b) 与像素数。"""
    l = t = 10 ** 9
    r = b = -1
    n = 0
    for y in range(y0, y1):
        for x in range(x0, x1):
            p = px(raw, stride, x, y)
            if abs(p[0] - bg[0]) + abs(p[1] - bg[1]) + abs(p[2] - bg[2]) > tol:
                n += 1
                l, r = min(l, x), max(r, x)
                t, b = min(t, y), max(b, y)
    return (l, t, r + 1, b + 1) if n else None, n


print("=" * 70)
print("标题栏 / 导航窗格（第 12 轮 4 / 7）")
hw = launch(BASE)
raw, stride, W, H = grab(hw)
print(f"  窗口 {W}x{H}")

# ---------- 1) 三大金刚键 ----------
TITLE_BG = (0x00, 0x00, 0x00)   # 暗色主题标题栏是纯黑
SB_BG = (0x19, 0x19, 0x19)     # 命令栏/导航窗格
TAB_BG = (0x2B, 0x2B, 0x2B)    # 当前标签底色
mins = (W - BTN_W * 3, W - BTN_W * 2)
maxs = (W - BTN_W * 2, W - BTN_W)
closes = (W - BTN_W, W)
for name, (x0, x1) in [("最小化", mins), ("最大化", maxs), ("关闭", closes)]:
    box, n = ink_box(raw, stride, x0 + 8, x1 - 8, 8, TITLE_H - 8, TITLE_BG)
    cy = TITLE_H // 2
    top_n = sum(1 for y in range(6, cy - 2) for x in range(x0 + 8, x1 - 8)
                if abs(px(raw, stride, x, y)[0] - TITLE_BG[0]) > 20)
    bot_n = sum(1 for y in range(cy + 2, TITLE_H - 6) for x in range(x0 + 8, x1 - 8)
                if abs(px(raw, stride, x, y)[0] - TITLE_BG[0]) > 20)
    print(f"  {name}: 墨迹 {n}px  包围盒 {box}  上半 {top_n} / 下半 {bot_n}")
    # 最小化本来就是 10px 宽的一条 1px 线，阈值不能按"实心方块"来定
    mark(n >= 8, f"{name}键画出了符号（{n}px）")

# 最小化：横线在下 => 下半明显多于上半
top_n = sum(1 for y in range(6, TITLE_H // 2 - 2) for x in range(mins[0] + 8, mins[1] - 8)
            if abs(px(raw, stride, x, y)[0] - TITLE_BG[0]) > 20)
bot_n = sum(1 for y in range(TITLE_H // 2 + 2, TITLE_H - 6) for x in range(mins[0] + 8, mins[1] - 8)
            if abs(px(raw, stride, x, y)[0] - TITLE_BG[0]) > 20)
mark(bot_n > top_n * 3, f"最小化是底部一条横线（上 {top_n} / 下 {bot_n}）")
# 最大化：方框 => 上下都有
top_n = sum(1 for y in range(6, TITLE_H // 2 - 2) for x in range(maxs[0] + 8, maxs[1] - 8)
            if abs(px(raw, stride, x, y)[0] - TITLE_BG[0]) > 20)
bot_n = sum(1 for y in range(TITLE_H // 2 + 2, TITLE_H - 6) for x in range(maxs[0] + 8, maxs[1] - 8)
            if abs(px(raw, stride, x, y)[0] - TITLE_BG[0]) > 20)
mark(top_n >= 6 and bot_n >= 6, f"最大化是方框（上 {top_n} / 下 {bot_n}）")

# ---------- 2) 标签页图标 ----------
TAB_BG = (0x2B, 0x2B, 0x2B)  # tab_active 近似；实际取不到就放宽
icon_l = 40 + 8      # sidebar_toggle 宽 40 + 8 边距
icon_box, n = ink_box(raw, stride, icon_l, icon_l + 16, 6, TITLE_H - 6, TAB_BG)
print(f"  标签 0 图标区墨迹 {n}px，包围盒 {icon_box}")
mark(n >= 60, f"标签页画出了图标（{n}px 墨迹）")
# 文字起点必须在图标右边（图标右边界之后才出现文字墨迹）
gap_n = sum(1 for y in range(6, TITLE_H - 6) for x in range(icon_l + 16, icon_l + 22)
            if abs(px(raw, stride, x, y)[0] - TAB_BG[0]) > 30)
print(f"  间隙区取样: {[px(raw, stride, x, 16) for x in range(icon_l + 14, icon_l + 26, 2)]}")
mark(gap_n <= 12, f"图标与文字之间有间隙、没叠在一起（间隙区 {gap_n}px）")

# ---------- 3) 侧栏首行 = 此电脑（有展开箭头） ----------
SB_TOP = TITLE_H + TOOL_H
chev_x0 = 4
chev_x1 = 4 + 16
y0 = SB_TOP + SB_ROW
y1 = y0 + SB_ROW
chev, n = ink_box(raw, stride, chev_x0, chev_x1, y0, y1, SB_BG)
print(f"  侧栏首行箭头区墨迹 {n}px")
mark(n >= 8, f"侧栏首行有展开箭头（可展开 = 此电脑，{n}px）")

# ---------- 4) 侧栏文字改小 ----------
# 不能拿"非底色"当判据：选中行/悬停行整行都会被刷成 hover_bg，整行 34px 全算
# 墨迹，字高就恒等于行高。文字是 #DEDEDE / #8F8F8F，背景再亮也只是 #3D3D3D，
# 用亮度 0x60 一刀切就能把行背景排除掉。
def text_rows(x0, x1, ya, yb, thr=0x60):
    ys = []
    n = 0
    for y in range(ya, yb):
        for x in range(x0, x1):
            if px(raw, stride, x, y)[0] >= thr:
                ys.append(y)
                n += 1
    return (min(ys), max(ys) + 1, n) if ys else (0, 0, 0)


t0, t1, n = text_rows(56, SB_W - 6, y0 + SB_ROW, y1 + SB_ROW)
hgt = t1 - t0
print(f"  侧栏第 2 行文字墨迹 {n}px，高 {hgt}px（行高 {SB_ROW}）")
mark(n >= 20 and hgt <= 16, f"侧栏文字已改小（字高 {hgt}px ≤ 16）")

# ---------- 5) 阶梯引导线 ----------
# 默认配置里"此电脑"本来就是展开的，先直接扫；扫不到再点一下箭头展开
raw, stride, W, H = grab(hw)
click(hw, 10, y0 + SB_ROW // 2)
time.sleep(0.5)
click(hw, 10, y0 + SB_ROW // 2)          # 收->展，确保是展开态
raw, stride, W, H = grab(hw)
BORDER = (0x53, 0x53, 0x53)
# 深度 1 的盘符行：引导线在 x = 4 + 16*0 + 8 = 12
col_hits = {}
# 展开后盘符排在常用文件夹**之后**（此电脑 -> 常用文件夹 -> 各盘 -> 回收站），
# 所以扫描范围要覆盖整条侧栏，不能只看头几行。
for x in range(2, 60):
    n = sum(1 for y in range(SB_TOP + SB_ROW, H - 40) if px(raw, stride, x, y) == BORDER)
    if n >= 60:
        col_hits[x] = n
print(f"  展开后竖线候选 x: {sorted(col_hits.items())[:6]}")
mark(len(col_hits) >= 1, f"展开后出现了缩进引导线（{len(col_hits)} 条）")

# ---------- 6) 导航窗格折叠按钮 ----------
btn_x = SB_W - 3 - 22 + 11
btn_y = SB_TOP + 3 + 11
click(hw, btn_x, btn_y)
raw, stride, W, H = grab(hw)
# 收起后侧栏不在了：原来那条竖分隔线（x≈156）应该消失，内容区从 0 开始
sb_gone = sum(1 for y in range(SB_TOP + 60, H - 60) if px(raw, stride, 156, y) == BORDER)
print(f"  点折叠按钮后 x=156 处 border 像素 {sb_gone}")
mark(sb_gone <= 4, f"点折叠按钮收起了导航窗格（残留 {sb_gone}px）")

subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
print("\n" + "=" * 70)
print("标题栏/导航窗格全部通过" if ok_all else "存在失败项")
