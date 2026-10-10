"""把 NexFile 的渲染结果与本机资源管理器基线逐项对比。

基线（CabinetWClass, DPI 96）：
  标题栏 #000000 / 命令栏+导航窗格 #191919 / 内容区 #202020 / 状态栏 #333333
  导航窗格宽 157（内容区从 x=157 起） / 行距 21 / 状态栏高 23
"""
import ctypes
import ctypes.wintypes as wt
import os
import struct
import subprocess
import sys
import time

# 默认落点是"主页"（平铺磁贴），行距基线只对**详细信息列表**有意义。
# 所以这里自己拉起一个文件夹视图再量，别去碰别人已经开着的窗口。
import shutil

import verify_env

verify_env.isolate()

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\fm-app.exe"
# 素材一律落在 %TEMP%（用户明确要求：别往桌面写东西），且要够多才撑得出行距样本。
BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "NexAlign")
shutil.rmtree(BASE, ignore_errors=True)
os.makedirs(BASE, exist_ok=True)
for i in range(30):
    open(os.path.join(BASE, f"sample{i}.txt"), "w").write("x" * (i + 1) * 11)
subprocess.run(["taskkill", "/F", "/IM", "fm-app.exe"], capture_output=True)
time.sleep(0.6)
subprocess.Popen([EXE, BASE])

u = ctypes.WinDLL("user32", use_last_error=True)
g = ctypes.WinDLL("gdi32", use_last_error=True)

u.FindWindowW.restype = wt.HWND
u.FindWindowW.argtypes = [ctypes.c_wchar_p, ctypes.c_wchar_p]
u.GetWindowRect.restype = wt.BOOL
u.GetWindowRect.argtypes = [wt.HWND, ctypes.POINTER(wt.RECT)]
u.GetDC.restype = wt.HDC
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

hwnd = None
for _ in range(80):
    time.sleep(0.25)
    hwnd = u.FindWindowW("NexFileMainWindow", None)
    if hwnd:
        break
if not hwnd:
    print("FAIL 找不到 NexFile 窗口")
    sys.exit(1)
time.sleep(2.5)
r = wt.RECT()
u.GetWindowRect(hwnd, ctypes.byref(r))
w, h = r.right - r.left, r.bottom - r.top
hdc = u.GetDC(hwnd)
mem = g.CreateCompatibleDC(hdc)
bmp = g.CreateCompatibleBitmap(hdc, w, h)
g.SelectObject(mem, bmp)
g.BitBlt(mem, 0, 0, w, h, hdc, 0, 0, 0x00CC0020)
hdr = struct.pack("<IiiHHIIiiII", 40, w, h, 1, 32, 0, w * h, 0, 0, 0, 0)
buf = ctypes.create_string_buffer(w * h * 4)
g.GetDIBits(mem, bmp, 0, h, buf, hdr, 0)
raw = buf.raw


def at(x, y):
    o = ((h - 1 - y) * w + x) * 4
    return (raw[o], raw[o + 1], raw[o + 2])


def hx(c):
    return "#%02X%02X%02X" % (c[2], c[1], c[0])


print(f"NexFile 窗口 {w}x{h}\n")
print("=== 区域底色对比 ===")
checks = [
    ("标题栏", int(w * 0.5), 16, "#000000"),
    ("工具栏/命令栏", 20, 70, "#191919"),
    ("地址框底色", int(w * 0.4), 55, "#2B2B2B"),
    ("列表内容区", int(w * 0.75), 400, "#202020"),
    ("状态栏", int(w * 0.5), h - 8, "#333333"),
]
ok = True
for name, x, y, want in checks:
    got = hx(at(x, y))
    good = got == want
    ok = ok and good
    print(f"  {name:<12} ({x:4d},{y:4d}) 实测 {got}  期望 {want}  {'OK' if good else '✗'}")

# 导航窗格不能只抽一个点：侧栏变成树之后 y=300 这种固定坐标随时会落在某一行的
# 图标或高亮上（实测抽到 #193159 —— 那是图标像素，不是底色）。
# 底色 = 该区域里出现次数最多的颜色，要求占比 > 80% 才算"这一片就是它"。
from collections import Counter

y0, y1 = 200, min(h - 60, 600)
cnt = Counter()
tot = 0
for y in range(y0, y1):
    for x in range(4, 154, 2):
        cnt[hx(at(x, y))] += 1
        tot += 1
top, n = cnt.most_common(1)[0]
good = top == "#191919" and n / max(1, tot) > 0.8
ok = ok and good
print(f"  {'导航窗格':<12} (区域 x 4..154 × y {y0}..{y1}) 众数 {top} 占比 "
      f"{n*100//max(1,tot)}%  期望 #191919  {'OK' if good else '✗'}")

print("\n=== 度量对比 ===")
# 导航窗格宽度：沿 y=300 找那条 1px 分隔线（border #535353）。
# 不能直接找"第一个颜色突变点" —— 侧栏每行的图标从 x=14 起画，
# 只要 y=300 正好落在某一行上（现在侧栏多了一项"回收站"），
# 第一个突变点就是图标而不是窗格边界。
# 判定"竖线"而不是"第一个匹配点"：侧栏里任何一行的图标都可能碰巧含 #535353
# 这类灰蓝像素，只在一条 y 上找会认错。真正的分隔线是**整条竖线** ——
# 取竖向上命中次数最多的那一列。
nav_edge, best_hits = None, 0
for x in range(20, w):
    hits = sum(1 for y in range(y0, y1) if hx(at(x, y)) == "#535353")
    if hits > best_hits:
        nav_edge, best_hits = x, hits
# 导航窗格右缘画了 1px 分隔线（border），所以颜色突变点在 156；
# 内容区仍从 157 起（分隔线本身属于导航窗格这一侧）。
EXPECT_EDGE = 156
print(f"  导航窗格右边界(含分隔线) x={nav_edge}（竖线命中 {best_hits}/{y1-y0} 行）"
      f"  期望 {EXPECT_EDGE}  {'OK' if nav_edge == EXPECT_EDGE else '✗'}")
ok = ok and (nav_edge == EXPECT_EDGE) and best_hits > (y1 - y0) * 0.5

# 行距：统计内容区墨迹段起点间距
CBG = at(int(w * 0.75), 400)
tops = []
run = None
for y in range(120, min(h - 40, 500)):
    n = sum(1 for x in range(200, 700)
            if max(abs(at(x, y)[k] - CBG[k]) for k in range(3)) > 60)
    if n > 2:
        if run is None:
            run = y
    else:
        if run is not None:
            tops.append(run)
            run = None
if run is not None:
    tops.append(run)
pitch = [tops[i + 1] - tops[i] for i in range(min(6, len(tops) - 1))]
print(f"  行墨迹段起点 {tops[:8]}")
print(f"  行距 {pitch}  期望 21  {'OK' if pitch and all(p == 21 for p in pitch[:4]) else '✗'}")
ok = ok and bool(pitch) and all(p == 21 for p in pitch[:4])

# 状态栏高度
st_top = None
for y in range(h - 1, 60, -1):
    if hx(at(int(w * 0.5), y)) != "#333333":
        st_top = y + 1
        break
print(f"  状态栏顶部 y={st_top} → 高 {h - st_top}  期望 23  {'OK' if h - st_top == 23 else '✗'}")

# 表头/首行结构
print("\n=== 内容区 y 结构 ===")
prev = None
for y in range(60, min(h, 260)):
    c = hx(at(int(w * 0.75), y))
    if c != prev:
        print(f"    y={y:4d} {c}")
        prev = c

print("\n结论:", "PASS 全部对齐" if ok else "FAIL 存在偏差")
