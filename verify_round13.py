"""验证用户这一轮点的 6 件事。全靠像素/注册表量化，不靠肉眼。

  1) 首页磁盘容量条填的是**剩余**空间（不是已用）
  2) 拖文件/文件夹时有跟手的拖拽影像，源条目同时变淡
  3) 设置页也能切标签、也能关标签（每个标签都画了 ×）
  4) 侧栏文字比列表正文更小
  5) 界面上看得到产品名 ZenFile
  6) 空白处按住拖出来的选择框看得见、且不挡住底下的内容
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
verify_env.clean()

u = ctypes.WinDLL("user32", use_last_error=True)
g = ctypes.WinDLL("gdi32", use_last_error=True)
k = ctypes.WinDLL("kernel32", use_last_error=True)
for f, restype, argtypes in (
    ("FindWindowW", wt.HWND, [ctypes.c_wchar_p, ctypes.c_wchar_p]),
    ("GetWindowRect", wt.BOOL, [wt.HWND, ctypes.POINTER(wt.RECT)]),
    ("GetWindowDC", wt.HDC, [wt.HWND]),
    ("PrintWindow", wt.BOOL, [wt.HWND, wt.HDC, ctypes.c_uint]),
    ("SetForegroundWindow", wt.BOOL, [wt.HWND]),
    ("PostMessageW", ctypes.c_bool, [wt.HWND, ctypes.c_uint, ctypes.c_ulong, ctypes.c_long]),
    ("ReleaseDC", None, [wt.HWND, wt.HDC]),
):
    getattr(u, f).restype = restype
    getattr(u, f).argtypes = argtypes
for f, restype, argtypes in (
    ("CreateCompatibleDC", wt.HDC, [wt.HDC]),
    ("CreateCompatibleBitmap", wt.HBITMAP, [wt.HDC, ctypes.c_int, ctypes.c_int]),
    ("SelectObject", wt.HGDIOBJ, [wt.HDC, wt.HGDIOBJ]),
    ("GetDIBits", ctypes.c_int, [wt.HDC, wt.HBITMAP, ctypes.c_uint, ctypes.c_uint,
                                 ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint]),
):
    getattr(g, f).restype = restype
    getattr(g, f).argtypes = argtypes
k.GetDiskFreeSpaceExW.restype = wt.BOOL
k.GetDiskFreeSpaceExW.argtypes = [ctypes.c_wchar_p, ctypes.POINTER(ctypes.c_ulonglong),
                                  ctypes.POINTER(ctypes.c_ulonglong), ctypes.POINTER(ctypes.c_ulonglong)]

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\ZenFile.exe"
BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "ZenR13")
# 必须整目录重建：上一轮跑完 BASE 里会残留十几个条目，"空白区"就不存在了，
# 拖框起点会正落在某条目上，于是触发的是「拖条目」而不是「拖框」——
# 表现就是框只有一行高（352..372），看着像框被裁剪了，其实是场景搭错了。
if os.path.isdir(BASE):
    for _root, _dirs, _files in os.walk(BASE):
        for _n in _dirs + _files:
            try:
                os.chmod(os.path.join(_root, _n), 0o666)
            except OSError:
                pass
    shutil.rmtree(BASE, ignore_errors=True)
os.makedirs(BASE, exist_ok=True)
for i in range(5):
    open(os.path.join(BASE, f"file{i:02d}.txt"), "w").write("x" * (i * 11 + 1))
os.makedirs(os.path.join(BASE, "subdir"), exist_ok=True)

WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE = 0x0201, 0x0202, 0x0200
TITLE_H, TOOL_H, HEADER_H = 34, 40, 26
BG = (0x20, 0x20, 0x20)
ACCENT = (0x43, 0x75, 0xCE)
SURFACE = (0x2B, 0x2B, 0x2B)

ok_all = True


def mark(ok, msg):
    global ok_all
    ok_all = ok_all and ok
    print(f"    {'PASS' if ok else 'FAIL'}  {msg}")


def launch(arg=None):
    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    time.sleep(0.5)
    subprocess.Popen([EXE] + ([arg] if arg else []))
    for _ in range(60):
        time.sleep(0.25)
        hw = u.FindWindowW("ZenFileMainWindow", None)
        if hw:
            time.sleep(1.8)
            return hw
    raise SystemExit("主窗口没起来")


def grab(hwnd):
    for _ in range(8):
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
        raw = buf.raw
        if any(raw[i] for i in range(0, min(len(raw), 40000), 997)):
            return raw, stride, w, h
        time.sleep(0.35)
    return raw, stride, w, h


def px(raw, stride, x, y):
    o = y * stride + x * 4
    return (raw[o + 2], raw[o + 1], raw[o])


def near(p, q, tol=30):
    return abs(p[0] - q[0]) + abs(p[1] - q[1]) + abs(p[2] - q[2]) <= tol


def post(hwnd, msg, x, y, wparam=0):
    u.PostMessageW(hwnd, msg, wparam, (y << 16) | (x & 0xFFFF))


def hold(hwnd, x0, y0, x1, y1, steps=10, pause=0.3):
    post(hwnd, WM_LBUTTONDOWN, x0, y0, 1)
    time.sleep(0.12)
    for i in range(1, steps + 1):
        post(hwnd, WM_MOUSEMOVE,
             x0 + (x1 - x0) * i // steps, y0 + (y1 - y0) * i // steps, 1)
        time.sleep(0.04)
    time.sleep(pause)


def release(hwnd, x, y):
    post(hwnd, WM_LBUTTONUP, x, y)
    time.sleep(0.7)


def longest_run(raw, stride, W, y_lo, y_hi, color, tol=30, x_lo=0, x_hi=None):
    """返回最贴近给定颜色的那条横向连续段的 (y, 长度, 起点)。"""
    x_hi = x_hi or W
    best = (0, 0, 0)
    for y in range(y_lo, y_hi):
        run = 0
        for x in range(x_lo, x_hi):
            if near(px(raw, stride, x, y), color, tol):
                run += 1
            else:
                if run > best[1]:
                    best = (y, run, x - run)
                run = 0
        if run > best[1]:
            best = (y, run, x_hi - run)
    return best


def disk(letter):
    free = ctypes.c_ulonglong(); total = ctypes.c_ulonglong(); tf = ctypes.c_ulonglong()
    k.GetDiskFreeSpaceExW(letter, ctypes.byref(free), ctypes.byref(total), ctypes.byref(tf))
    return total.value, free.value


print("=" * 72)

# ------------------------------------------------------------------ 1) 容量条
print("[1] 首页磁盘容量条应张 show 剩余空间")
tot, free = disk("C:\\")
expect = 230 * free / tot
print(f"  C: 总 {tot/2**30:.0f} GB / 剩余 {free/2**30:.0f} GB "
      f"=> 剩余占比 {free/tot*100:.1f}%（条宽 230 → 期望 {expect:.0f}px；"
      f"若按已用画则是 {230*(tot-free)/tot:.0f}px）")
hw = launch()
raw, stride, W, H = grab(hw)
y, run, x0 = longest_run(raw, stride, W, TITLE_H + TOOL_H + 4, H - 30, ACCENT, 24, x_lo=170)
print(f"  实测最长 accent 横段: y={y} 长 {run}px 起点 x={x0}")
mark(run >= 60 and abs(run - expect) <= 18,
     f"容量条宽度对得上\"剩余 {free/tot*100:.0f}%\"（实测 {run} / 期望 {expect:.0f}）")

# ------------------------------------------------------------------ 5) 产品名
print("\n[5] 界面上要能看到 ZenFile")
# 标题栏最后一个标签右侧到三大金刚之间那一段（tabs.right = W-176）
cols = [x for x in range(40, W - 176)
        if any(px(raw, stride, x, yy)[0] > 0x55 for yy in range(8, 26))]
right = cols[-1] if cols else 0
print(f"  标题栏最右侧有文字的列 x={right}（tabs.right={W-176}）")
mark(W - 176 - right < 160, f"标题栏右端出现了程序名（- ZenFile）")

# ------------------------------------------------------------------ 4) 侧栏字号
print("\n[4] 侧栏文字要比正文更小")
hw = launch(BASE)
raw, stride, W, H = grab(hw)
SB_TOP = TITLE_H + TOOL_H
SB_ROW = 34


def text_height(x0, x1, ya, yb, thr=0x60):
    ys, n = [], 0
    for yy in range(ya, yb):
        for xx in range(x0, x1):
            if px(raw, stride, xx, yy)[0] >= thr:
                ys.append(yy)
                n += 1
    return (min(ys), max(ys) + 1, n) if ys else (0, 0, 0)


sb_t0, sb_t1, sb_n = text_height(56, 151, SB_TOP + 2 * SB_ROW, SB_TOP + 3 * SB_ROW)
sb_h = sb_t1 - sb_t0
list_t0, list_t1, _ = text_height(157 + 21 + 30, W - 40, TITLE_H + TOOL_H + HEADER_H + 2,
                                  TITLE_H + TOOL_H + HEADER_H + 21)
list_h = list_t1 - list_t0
print(f"  侧栏某行字高 {sb_h}px（{sb_n}px 墨迹），列表某行字高 {list_h}px")
mark(0 < sb_h <= 11, f"侧栏字更小了（{sb_h}px，列表 {list_h}px）")

# ------------------------------------------------------------------ 3) 标签
print("\n[3] 打开设置后仍要能切标签、关标签")
hw = launch("shell:Settings")
u.SetForegroundWindow(hw)
raw, stride, W, H = grab(hw)


def active_tab(raw, stride, W):
    segs, run, start = [], 0, None
    for x in range(40, W - 176):
        if near(px(raw, stride, x, 17), (0x2B, 0x2B, 0x2B), 16):
            if run == 0:
                start = x
            run += 1
        else:
            if run > 30:
                segs.append((start, x))
            run = 0
    if run > 30:
        segs.append((start, W - 176))
    return segs


one = active_tab(raw, stride, W)
# 每个标签都要能看见 ×：非活动标签的 × 区域也要有墨迹（text_dim #8F8F8F）
u.SetForegroundWindow(hw)
for vk in (0x11, 0x54):
    u.keybd_event(vk, 0, 0, 0)
u.keybd_event(0x54, 0, 2, 0)
u.keybd_event(0x11, 0, 2, 0)
time.sleep(1.0)
raw, stride, W, H = grab(hw)
two = active_tab(raw, stride, W)
print(f"  打开设置 + Ctrl+T 后活动标签段: {two}（应落在 index 1）")
mark(one and two and two[0][0] > one[0][0] + 100, "Ctrl+T 之后焦点到了新标签")

# 非活动标签（index 0）的关闭按钮位置：对 tabs.right 之外算(tab_rect(0).right-2-18)
close0_n = sum(1 for yy in range(8, 28) for xx in range(40 + 180 - 2 - 18, 40 + 180 - 2)
               if px(raw, stride, xx, yy)[0] > 0x55)
print(f"  非活动标签的 × 区域墨迹 {close0_n}px")
mark(close0_n >= 8, "每个标签都画了关闭按钮（不再只有当前标签有）")

# 切回 index 0
post(hw, WM_LBUTTONDOWN, 40 + 90, 17)
post(hw, WM_LBUTTONUP, 40 + 90, 17)
time.sleep(0.8)
raw, stride, W, H = grab(hw)
back = active_tab(raw, stride, W)
print(f"  点 index 0 之后活动标签段: {back}")
mark(back and abs(back[0][0] - one[0][0]) <= 6, "在设置页也切得动标签")

# 关掉只剩一个标签时的最后那个
for idx_x in (40 + 180 * 2 - 11, 40 + 180 - 11):
    post(hw, WM_LBUTTONDOWN, idx_x, 17)
    post(hw, WM_LBUTTONUP, idx_x, 17)
    time.sleep(0.8)
raw, stride, W, H = grab(hw)
last = active_tab(raw, stride, W)
print(f"  连点两次 × 之后活动标签段: {last}")
mark(len(last) == 1, "最后一个标签也能关掉（关掉后补一个主页标签，窗口不消失）")

# ------------------------------------------------------------------ 2) 拖拽影像
print("\n[2] 拖文件要有拖拽影像，源条目要变淡")
hw = launch(BASE)
time.sleep(0.5)
raw0, stride, W, H = grab(hw)
row0 = TITLE_H + TOOL_H + HEADER_H + 10
x0 = 157 + 21 + 40
cur = (x0 + 120, row0 + 63)
hold(hw, x0, row0, cur[0], cur[1])
rawd, stride, W, H = grab(hw)
# 影像卡片：光标右下 12px 起，底色 surface
cx, cy = cur[0] + 12, cur[1] + 12
card = sum(1 for yy in range(cy, cy + 40) for xx in range(cx, cx + 120)
           if near(px(rawd, stride, xx, yy), SURFACE, 40))
print(f"  光标右下卡片区 surface 底色像素 {card}")
mark(card >= 400, f"画出跟手的拖拽影像（{card}px）")
# 卡片里得有图标（不是一块纯色板）：统计不同颜色数
variety = len({px(rawd, stride, xx, yy) for yy in range(cy + 6, cy + 38)
               for xx in range(cx + 6, cx + 40)})
print(f"  卡片图标区颜色种类 {variety}")
mark(variety >= 8, f"影像里真的有图标（{variety} 种颜色）")
# 源条目变淡：第 0 行图标区域变暗
src_diff = sum(1 for yy in range(row0 - 8, row0 + 8) for xx in range(157 + 26, 157 + 48)
               if abs(px(raw0, stride, xx, yy)[0] - px(rawd, stride, xx, yy)[0]) > 12)
print(f"  源条目图标区像素变化 {src_diff}px")
mark(src_diff >= 30, f"被拖走的条目淡下去了（{src_diff}px 变化）")
release(hw, cur[0], cur[1])

# ------------------------------------------------------------------ 6) 框选
print("\n[6] 空白处拖出来的选择框")
hw = launch(BASE)
u.SetForegroundWindow(hw)
# 必须先固定成"详细信息"：defolt_mode=system 时会去继承资源管理器给这个文件夹
# 记的视图，图标布局下条目行又高又宽，"空白处"根本不存在，框选永远触发不到。
u.keybd_event(0x11, 0, 0, 0)
u.keybd_event(0x31, 0, 0, 0)
time.sleep(0.08)
u.keybd_event(0x31, 0, 2, 0)
u.keybd_event(0x11, 0, 2, 0)
time.sleep(0.9)
raws, stride, W, H = grab(hw)

LIST_TOP = TITLE_H + TOOL_H + HEADER_H
bx = 157 + 21 + 30


def last_ink(raw, stride, y_from, y_to, x_from, x_to, thr=0x33):
    """列表区里最后一个有内容的 y —— 它下面才谈得上是空白。"""
    lo = y_from
    for yy in range(y_from, y_to):
        for xx in range(x_from, x_to, 2):
            p = px(raw, stride, xx, yy)
            if p[0] >= thr or p[1] >= thr or p[2] >= thr:
                lo = yy
                break
    return lo


# 起点必须在真空白区：条目最后一行往下再留 30px 余量。
ink = last_ink(raws, stride, LIST_TOP, H - 40, 157 + 21, W - 40)
by0 = ink + 30
by1 = min(by0 + 90, H - 60)
print(f"  列表最后有内容的行 y={ink}；框取在空白区 y {by0}..{by1}")
assert by1 - by0 >= 60, "窗口太小，凑不出空白区"
before = px(raws, stride, bx, by0)
print(f"  起点底色 {before}（应为窗口底色 {BG}）")
mark(near(before, BG, 14), "拖框起点确实在空白区（没有压在任何条目上）")

# 判据要避开"实时选中色带"：框到的行会当场刷成选中色，整行通到窗口右端，
# 用它当证据就分不清"画了框"还是"画了选中"。所以先框一块**不与任何条目
# 相交**的空白区，单独看框本身。
hold(hw, bx, by0, bx + 300, by1)
rawm, stride, W, H = grab(hw)
h = by1 - by0
area = 300 * h
hits = [(xx, yy) for yy in range(by0, by1) for xx in range(bx, bx + 300)
        if not near(px(raws, stride, xx, yy), px(rawm, stride, xx, yy), 12)]
changed = len(hits)
if hits:
    hx = [a for a, _ in hits]
    hy = [b for _, b in hits]
    print(f"  变化包围盒 x {min(hx)}..{max(hx)}  y {min(hy)}..{max(hy)}"
          f"（框应为 x {bx}..{bx + 300} y {by0}..{by1}）")
    mark(min(hy) >= by0 - 2 and max(hy) >= by1 - 4 and
         min(hx) <= bx + 2 and max(hx) >= bx + 296,
         f"框的包围盒就是拖出来的矩形（高 {max(hy) - min(hy) + 1} / 期望 {h}）")
print(f"  空白区拖框：框内变化像素 {changed} / {area}")
mark(changed >= area * 0.9, f"空白处按住拖，框真的画出来了（{changed}/{area}）")
# 「半透明」不能用"框内颜色种类"来判：这块区域本来就是一片 #202020 空白，
# 就算真的是半透明叠上去，混出来也必然只有 1 种颜色 —— 那是场景的锅不是画的锅。
# 真能区分实心/半透明的是两点：
#   (a) 空白处的框色必须**严格落在底色和强调色之间**（说明是按 alpha 混的，
#       不是拿强调色实心铺满，也不是压根没画）；
#   (b) 框压在条目上时，底下的文字还得读得出来（实心板会把字整块盖掉）。
box_bg = px(rawm, stride, bx + 8, by0 + 8)
print(f"  空白处的框色 {box_bg}（底色 {BG} → 强调色 {ACCENT}，实心铺满会是 {ACCENT}）")
mark(BG[0] < box_bg[0] < ACCENT[0] and BG[2] < box_bg[2] < ACCENT[2],
     f"框是按 alpha 混上去的（{box_bg} 介于底色与强调色之间）")
release(hw, bx + 300, by1)
time.sleep(0.4)

# 再拖一次：从空白区往上扫进条目区，看框压住条目时文字还透不透得出来
hold(hw, bx, by0, bx + 300, LIST_TOP + 5)
rawm2, stride, W, H = grab(hw)
overlap = max(LIST_TOP + 8, LIST_TOP + 5)  # 框与条目区的交集上沿
brightest = 0
for yy in range(overlap, min(by0, ink + 2)):
    for xx in range(bx + 4, bx + 296, 2):
        brightest = max(brightest, px(rawm2, stride, xx, yy)[0])
print(f"  框压住条目时区域内最亮像素 R={brightest}（0x{brightest:02X}；"
      f"实心板会把字盖掉 → 只剩框色 0x{box_bg[0]:02X}）")
mark(brightest >= 0x70 and brightest > box_bg[0] + 40,
     f"框是半透明的，底下的文字还看得见（最亮 0x{brightest:02X}）")
release(hw, bx + 300, LIST_TOP + 5)
rawr, stride, W, H = grab(hw)
sel = sum(1 for yy in range(LIST_TOP, H - 30) for xx in range(157, W)
          if near(px(rawr, stride, xx, yy), (0x47, 0x47, 0x47), 12))
print(f"  向上框选后选中色带像素 {sel}")
mark(sel >= 300, f"框选真的选中了条目（{sel}px）")

subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
print("\n" + ("全部通过" if ok_all else "有失败项"))
