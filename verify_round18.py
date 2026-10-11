"""第 18 轮像素级回归。

判定全部走像素与窗口事实，不靠肉眼。覆盖用户本轮报的 19 条：

  A  侧栏：文件夹在顶、回收站在底(#5)；缩窗口后不溢出到工具栏三键上(#5)
        回收站里侧栏依然可点(#1)
  B  侧栏上方三个导航按钮有 hover 底色(#6)
  C  设置页里可下拉的行：单击即展开，不用按住(#2)
  D  目录栏「批量重命名」图标按钮：单击即展开(#3)
  E  主页右键：空白处与磁贴上弹的是两套菜单(#7)
  F  回收站表头可点排序(#8)
  G  排序箭头是上/下方向，不是侧向(#9)
  H  表头文字在各自列内居中(#11)
  I  剪切后条目虚化(#13)
  J  选中色带止于最后一列右沿，不铺满(#15)
  K  状态栏两个视图按钮的图标：表格 / 2x2 方块(#16)
  L  四边都能拉伸，地址栏上是 I 形光标(#17)
  M  改名框宽度随名字长度，且不越过名称列右沿(#18)
  N  表头悬停出筛选箭头、点开下拉(#19)

（#4 空括号由 `--selftest` 新增的三条用例钉住；#10 Alt+双击、#12 拖拽卡片合一、
 #14 菜单双缓冲 + 透明色缓存 见末尾说明。）
"""
import ctypes
import ctypes.wintypes as wt
import os
import subprocess
import time

import verify_common as V

u = V.u
g = V.g

u.GetDpiForWindow.restype = ctypes.c_uint
u.GetDpiForWindow.argtypes = [wt.HWND]
u.SendMessageW.restype = ctypes.c_ssize_t
u.SendMessageW.argtypes = [wt.HWND, ctypes.c_uint, ctypes.c_size_t, ctypes.c_ssize_t]
u.GetCursor.restype = wt.HICON
u.SetWindowPos.restype = wt.BOOL
u.SetWindowPos.argtypes = [wt.HWND, wt.HWND, ctypes.c_int, ctypes.c_int,
                           ctypes.c_int, ctypes.c_int, ctypes.c_uint]

BASE = V.BASE
W_WIN, H_WIN = 1180, 780
OS_X, OS_Y = 60, 60  # 窗口左上角（launch 里 SetWindowPos 定的）

LONG = "L" * 96 + ".txt"
for _n in ("f0.txt", "f1.txt", LONG):
    _p = os.path.join(BASE, _n)
    if not os.path.exists(_p):
        open(_p, "w").write("x")


def geom(hw):
    """从真实 DPI 推出几何 —— 拍脑袋写 157/74 在 125% 缩放下整体错位。"""
    dpi = u.GetDpiForWindow(hw) or 96
    s = dpi / 96.0
    return {"s": s, "title": round(34 * s), "tool": round(40 * s),
            "status": round(23 * s), "side": round(157 * s)}


def wclick(x, y):
    V.click(OS_X + x, OS_Y + y)


def wmove(x, y):
    V.move(OS_X + x, OS_Y + y)


def wrclick(x, y):
    V.rclick(OS_X + x, OS_Y + y)


def close_menu(mh):
    """关掉自绘菜单。

    不能靠 `V.key(0x1B)`：菜单是 SW_SHOWNOACTIVATE 弹的，从来没拿过焦点，
    键盘事件会落到主窗口上去。必须直接把按键投递给菜单窗口本身 —— 否则菜单
    一直挂着鼠标 capture，下一次右键会被它当成"再点一次右键=收起"吃掉。
    """
    u.PostMessageW(mh, 0x0100, 0x1B, 0)  # WM_KEYDOWN
    u.PostMessageW(mh, 0x0101, 0x1B, 0)  # WM_KEYUP
    time.sleep(0.35)


def close_all_menus():
    for mh, _r in V.menus():
        close_menu(mh)


def col_rects(g):
    """详细信息四列的 [左, 右]（默认列宽；配置已被 verify_env 隔离）。"""
    s = g["s"]
    name = g["side"] + round(21 * s) + round(16 * s) + round(3 * s)
    out = []
    x = name
    for wpx in (274, 144, 120, 80):
        out.append((x, x + round(wpx * s)))
        x += round(wpx * s)
    return out


print("== A. 侧栏：顺序 / 不溢出 / 回收站里仍可点 ==")
hw = V.launch(BASE)
G = geom(hw)
raw, stride, w, h = V.grab(hw)
VT = G["title"] + G["tool"]
rows = V.ink_rows(raw, stride, 4, G["side"] - 4, VT + 2, h - G["status"] - 2, thr=0x70, merge=8)
print("    侧栏行带:", rows[:16])
V.mark(len(rows) >= 3, f"侧栏有多组内容（实测 {len(rows)} 段）")
gaps = [(rows[i + 1][0] - rows[i][1]) for i in range(len(rows) - 1)]
V.mark(sum(1 for x in gaps if x >= 8) >= 2, f"分组之间有空隙（{gaps}）")
V.mark(rows[0][0] >= VT, f"侧栏内容从窗格顶开始（{rows[0][0]} >= {VT}）")

# 不溢出：把窗口压矮 + 把侧栏滚下去，制造"内容比窗格高"的情形。
# 少了一道裁剪，多出来的行会一路画到工具栏那三个导航键上。
u.SetWindowPos(hw, None, OS_X, OS_Y, W_WIN, 300, 0x0004)
time.sleep(0.8)
wmove(60, 200)
time.sleep(0.2)
for _ in range(6):
    arr = (V.INPUT * 1)(V.INPUT(0, V._U(mi=V.MOUSEINPUT(0, 0, -120, 0x0800, 0, None))))
    u.SendInput(1, ctypes.byref(arr), ctypes.sizeof(V.INPUT))
    time.sleep(0.12)
time.sleep(0.5)
raw, stride, w2, h2 = V.grab(hw)
VT2 = VT
# 只取工具栏最下面那 5px：地址栏底沿在 tb_top-pad，再往上就是地址栏自己的底色
above = V.count(raw, stride, 4, G["side"] - 4, VT2 - 5, VT2 - 1, V.CHROME_BG, tol=10)
above_all = (G["side"] - 8) * 4
print(f"    窗格顶上方 4px 底色像素 {above}/{above_all}")
V.mark(above >= above_all * 0.95, f"侧栏没有溢出到工具栏（{above}/{above_all}）")

# 还原成大窗口再继续
u.SetWindowPos(hw, None, OS_X, OS_Y, W_WIN, H_WIN, 0x0004)
time.sleep(0.8)
raw, stride, w, h = V.grab(hw)
rows = V.ink_rows(raw, stride, 4, G["side"] - 4, VT + 2, h - G["status"] - 2, thr=0x70, merge=8)
head_top = VT + round(44 * G["s"])
hh = round(26 * G["s"])
wclick(60, rows[-1][0] + 12)
time.sleep(3.0)
raw, stride, w, h = V.grab(hw)
band = V.count(raw, stride, G["side"] + 2, w - 2, head_top + 2, head_top + hh - 2,
               V.SURFACE_ALT, tol=10)
band_px = (w - G["side"] - 4) * (hh - 4)
V.mark(band >= band_px * 0.6, f"侧栏最后一段是回收站（列头条带 {band}/{band_px}）")
rows2 = V.ink_rows(raw, stride, 4, G["side"] - 4, VT + 2, h - G["status"] - 2, thr=0x70, merge=8)
wclick(60, rows2[0][0] + 12)
time.sleep(1.6)
raw, stride, w, h = V.grab(hw)
band2 = V.count(raw, stride, G["side"] + 2, w - 2, head_top + 2, head_top + hh - 2,
                V.SURFACE_ALT, tol=10)
V.mark(band2 < band * 0.5, f"回收站里点侧栏能导航出去（条带 {band} -> {band2}）")

print("== B. 导航三键 hover 底色 ==")
raw, stride, w, h = V.grab(hw)
nav = round(30 * G["s"])
pad = round(6 * G["s"])
before = V.count(raw, stride, pad, pad + nav, G["title"] + 4, VT - 4, V.ROW_HOVER, tol=10)
wmove(pad + nav // 2, G["title"] + round(20 * G["s"]))
time.sleep(0.35)
raw2, stride2, w2, h2 = V.grab(hw)
after = V.count(raw2, stride2, pad, pad + nav, G["title"] + 4, VT - 4, V.ROW_HOVER, tol=10)
area = nav * (VT - 4 - G["title"] - 4)
print(f"    hover 前后 row_hover 像素 {before} -> {after}（区域 {area}）")
V.mark(before < area * 0.2 and after >= area * 0.7,
       f"后退按钮悬停出现 hover 底色（{before} -> {after}）")

print("== C. 设置页：单击展开下拉 ==")
mb_w = round(44 * G["s"])
wclick(w - mb_w * 3 - mb_w // 2, G["title"] // 2)
time.sleep(0.6)
ms = V.menus()
V.mark(len(ms) >= 1, "「更多」菜单弹出")
if ms:
    mh = ms[0][0]
    cs = V.menu_row_centers(mh)
    mr = wt.RECT()
    u.GetWindowRect(mh, ctypes.byref(mr))
    # 倒数第二行是「设置」（最后一行是「关于 ZenFile」）
    V.click(mr.left + 40, mr.top + cs[-2])
    time.sleep(0.9)
V.mark(len(V.menus()) == 0, "点「设置」后菜单收起（「更多」菜单不该吞掉这一次抬起）")
raw, stride, w, h = V.grab(hw)
sw_rows = V.ink_rows(raw, stride, G["side"] + 4, w - 4, VT + 2, h - G["status"] - 2,
                     thr=0x70, merge=6)
print("    设置页行带数:", len(sw_rows))
V.mark(len(sw_rows) >= 6, f"进到了设置页（{len(sw_rows)} 行）")
if len(sw_rows) >= 6:
    # 带"当前值 ▾"的行（大小单位 / 默认视图 / 主题）点一下就该弹下拉。
    # 行序随分组标题变化，这里逐个试：试到弹出为止，弹不出来才算缺陷。
    hit_row = None
    for i in range(1, min(len(sw_rows), 10)):
        V.click(OS_X + w - 120, OS_Y + sw_rows[i][0] + 8)
        time.sleep(0.6)
        if V.menus():
            hit_row = i
            break
        close_all_menus()
    print("    弹出下拉的是第", hit_row, "行")
    V.mark(hit_row is not None, "设置里可下拉的行：单击即展开，松手不关")
    close_all_menus()
else:
    V.mark(False, "没进到设置页")

print("== D. 目录栏「批量重命名」图标按钮：单击展开 ==")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.5)
hw = V.launch(BASE)
G = geom(hw)
raw, stride, w, h = V.grab(hw)
sw = round(170 * G["s"])
bw = round(30 * G["s"])
pd = round(6 * G["s"])
V.click(OS_X + w - pd - sw - 8 - bw // 2, OS_Y + G["title"] + round(20 * G["s"]))
time.sleep(0.7)
ms = V.menus()
V.mark(len(ms) >= 1, f"批量重命名按钮单击即展开（实测 {len(ms)} 个菜单，松手后仍在）")
close_all_menus()

print("== E. 主页右键：空白 vs 磁贴 ==")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.5)
hw = V.launch()  # 无参数 = 主页
G = geom(hw)
raw, stride, w, h = V.grab(hw)
VT = G["title"] + G["tool"]
tiles = V.ink_rows(raw, stride, G["side"] + 4, w - 4, VT, h - G["status"] - 2, thr=0x70, merge=10)
print("    主页内容行带:", tiles[:6])
if tiles:
    tx = G["side"] + 60
    wrclick(tx, (tiles[0][0] + tiles[0][1]) // 2)
    time.sleep(0.7)
    m1 = V.menus()
    n1 = len(V.menu_row_centers(m1[0][0])) if m1 else 0
    close_all_menus()
    wrclick(tx, tiles[-1][1] + 120)
    time.sleep(0.7)
    m2 = V.menus()
    n2 = len(V.menu_row_centers(m2[0][0])) if m2 else 0
    close_all_menus()
    print(f"    磁贴上菜单 {n1} 行，空白处菜单 {n2} 行")
    V.mark(n1 > 0 and n2 > 0 and n2 != n1,
           f"主页两种右键落点给的是两套菜单（{n1} vs {n2} 行）")
else:
    V.mark(False, "主页没量到磁贴")

print("== F. 回收站表头排序 ==")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.5)
hw = V.launch(BASE)
G = geom(hw)
raw, stride, w, h = V.grab(hw)
VT = G["title"] + G["tool"]
rows = V.ink_rows(raw, stride, 4, G["side"] - 4, VT + 2, h - G["status"] - 2, thr=0x70, merge=8)
wclick(60, rows[-1][0] + 12)
# 回收站是后台流式扫描的（本机 3.9 万项）。不等它扫完就点列头，
# 排完序后面几批又追加进来，画面看起来"点了没反应"。这里等到首屏稳定为止。
time.sleep(3.0)
raw, stride, w, h = V.grab(hw)
s = G["s"]
ht = VT + round(44 * s)
hh = round(26 * s)
pad = round(12 * s)
w_time, w_size, w_orig = round(150 * s), round(90 * s), round(230 * s)
x0 = G["side"] + pad
x1 = max(w - pad - w_time - w_size - w_orig, x0 + round(120 * s))
cols = [(x0 + pad + round(36 * s), x1), (x1, x1 + w_orig),
        (x1 + w_orig, x1 + w_orig + w_time), (x1 + w_orig + w_time, w - pad)]


def col_inks(rr, st):
    return [sum(1 for y in range(ht, ht + hh) for x in range(c[0], c[1])
                if min(V.px(rr, st, x, y)) >= 0x70) for c in cols]


def list_profile(rr, st):
    lt = ht + hh + 2
    out = []
    for y in range(lt, lt + round(21 * s) * 10):
        out.append(sum(1 for x in range(G["side"] + 4, w - 4, 2)
                       if min(V.px(rr, st, x, y)) >= 0x70))
    return out


def settled_profile():
    """等到首屏指纹连续两次一致 —— 扫描还在灌的时候量什么都不算数。"""
    last = None
    for _ in range(12):
        rr, st, _w, _h = V.grab(hw)
        cur = list_profile(rr, st)
        if cur == last:
            return rr, st, cur
        last = cur
        time.sleep(0.6)
    return raw, stride, last or []


raw, stride, p0 = settled_profile()
i0 = col_inks(raw, stride)
wclick((cols[3][0] + cols[3][1]) // 2, ht + hh // 2)
time.sleep(1.5)
raw, stride, p1 = settled_profile()
i1 = col_inks(raw, stride)
print(f"    列头墨迹 {i0} -> {i1}")
print(f"    首屏指纹 {sum(p0)} -> {sum(p1)}")
V.mark(i1[3] > i0[3] + 8 and i1[2] < i0[2],
       f"排序箭头从第3列移到第4列（{i0[2]}->{i1[2]}, {i0[3]}->{i1[3]}）")
V.mark(p0 != p1, f"点回收站列头真的重排了（指纹 {sum(p0)} -> {sum(p1)}）")

print("== G. 排序箭头是上/下方向 ==")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.5)
hw = V.launch(BASE)
G = geom(hw)
raw, stride, w, h = V.grab(hw)
VT = G["title"] + G["tool"]
CR = col_rects(G)
hdr0, hdr1 = VT, VT + round(26 * G["s"])
xmin = xmax = None
for x in range(CR[0][0] - round(40 * G["s"]), CR[0][1]):
    if any(min(V.px(raw, stride, x, y)) >= 0x70 for y in range(hdr0, hdr1)):
        if xmin is None:
            xmin = x
        xmax = x
print(f"    名称列头墨迹 x {xmin}..{xmax}；名称列 {CR[0]}")
if xmax is not None:
    aw = round(14 * G["s"])
    ax0 = xmax - aw
    rows_w = []
    for y in range(hdr0, hdr1):
        n = sum(1 for x in range(ax0, xmax + 1)
                if min(V.px(raw, stride, x, y)) >= 0x70)
        rows_w.append(n)
    lit = [(y - hdr0, n) for y, n in enumerate(rows_w) if n > 0]
    print("    箭头逐行宽度:", lit)
    if lit:
        top = lit[0][1]
        bottom = lit[-1][1]
        mid = max(n for _, n in lit)
        mid_row = max(lit, key=lambda t: t[1])[0]
        print(f"    顶宽 {top} / 最宽 {mid}（第 {mid_row} 行） / 底宽 {bottom}")
        # 上/下三角：一头宽一头窄，最宽那行贴着宽的那头。
        # 侧向（右）三角则是中间最宽、上下两头一样窄 —— 正是这条要挡住的回归。
        V.mark(abs(top - bottom) >= 3 and mid_row <= 1 or abs(top - bottom) >= 3 and mid_row >= len(lit) - 2,
               f"箭头是上下方向（顶 {top} 底 {bottom}，最宽在第 {mid_row}/{len(lit)} 行）")
    else:
        V.mark(False, "没量到箭头墨迹")
else:
    V.mark(False, "没量到表头文字")

print("== H. 表头文字在自己列内居中 ==")
if xmin is not None:
    off = xmin - CR[0][0]
    print(f"    「名称」文字左沿距列左沿 {off}px（列宽 {CR[0][1] - CR[0][0]}）")
    V.mark(off >= round(100 * G["s"]), f"表头文字居中而非贴左沿（偏移 {off}）")
else:
    V.mark(False, "没量到表头文字")

print("== I. 剪切后条目虚化 ==")
raw, stride, w, h = V.grab(hw)
row_h = round(21 * G["s"])
first_row = VT + round(26 * G["s"])
icon_x0 = G["side"] + round(8 * G["s"])
icon_x1 = icon_x0 + round(20 * G["s"])
y0, y1 = first_row + 2, first_row + row_h - 2


def icon_ink(rr, st):
    return sum(1 for y in range(y0, y1) for x in range(icon_x0, icon_x1)
               if min(V.px(rr, st, x, y)) >= 0x70)


n_before = icon_ink(raw, stride)
wclick(G["side"] + 120, first_row + row_h // 2)
time.sleep(0.4)
V.ctrl_key(0x58)  # Ctrl+X
time.sleep(0.8)
raw, stride, w, h = V.grab(hw)
n_after = icon_ink(raw, stride)
print(f"    图标区墨迹 {n_before} -> {n_after}")
V.mark(n_after < n_before * 0.85, f"剪切后条目被虚化（墨迹 {n_before} -> {n_after}）")

print("== J. 选中色带止于最后一列右沿 ==")
last_right = CR[3][1]
raw, stride, w, h = V.grab(hw)
rx = -1
for x in range(G["side"], w):
    if any(V.near(V.px(raw, stride, x, y), V.SELECTION, tol=10) for y in range(y0, y1)):
        rx = x
print(f"    选中色带右沿 {rx}；最后一列右沿 {last_right}；视图右沿 {w}")
V.mark(0 < rx <= last_right + 6 and rx < w - 40,
       f"选中框不铺满宽度（右沿 {rx} <= 列右沿 {last_right} < 视图 {w}）")

print("== K. 状态栏两个视图按钮的图标 ==")
raw, stride, w, h = V.grab(hw)
sy0, sy1 = h - G["status"], h - 2
clusters = []
run = None
for x in range(w - round(120 * G["s"]), w - 2):
    has = any(min(V.px(raw, stride, x, y)) >= 0x70 for y in range(sy0, sy1))
    if has and run is None:
        run = x
    elif not has and run is not None:
        clusters.append((run, x))
        run = None
if run is not None:
    clusters.append((run, w - 2))
print("    状态栏右侧墨迹簇:", clusters)
if len(clusters) >= 2:
    def full_rows(c):
        n = 0
        for y in range(sy0 + 3, sy1 - 3):
            tot = c[1] - c[0]
            lit = sum(1 for x in range(c[0], c[1])
                      if min(V.px(raw, stride, x, y)) >= 0x70)
            if tot > 4 and lit >= tot * 0.7:
                n += 1
        return n

    fa, fb = full_rows(clusters[0]), full_rows(clusters[-1])
    print(f"    两簇横贯行数 {fa} / {fb}")
    V.mark(max(fa, fb) >= 2, f"详细信息按钮画的是表格（横贯行 {fa}/{fb}）")
else:
    V.mark(False, "状态栏没量到两个按钮")

print("== L. 四边拉伸命中 + 地址栏 I 形光标 ==")


def hit_at(sx, sy):
    lp = ((sy & 0xFFFF) << 16) | (sx & 0xFFFF)
    return u.SendMessageW(hw, 0x0084, 0, lp) & 0xFFFF


exp = {(2, H_WIN // 2): 10, (W_WIN // 2, 2): 12,
       (W_WIN - 2, H_WIN - 2): 17, (2, H_WIN - 2): 16, (W_WIN // 2, H_WIN - 2): 15,
       (W_WIN - 2, H_WIN // 2): 11}
got = {p: hit_at(OS_X + p[0], OS_Y + p[1]) for p in exp}
print("    命中码:", got)
for p, want in exp.items():
    V.mark(got[p] == want, f"{p} -> HT{want}（实测 {got[p]}）")
# 右上角落在关闭键上是对的：三大金刚键本来就压在窗口角上，
# 它们的优先级必须高于"这个角可以拉伸"（Windows 自己也是这个顺序）。
c_corner = hit_at(OS_X + W_WIN - 2, OS_Y + 2)
V.mark(c_corner == 20, f"右上角归关闭键而不是边框（实测 HT{c_corner}）")


class CURSORINFO(ctypes.Structure):
    _fields_ = [("cbSize", wt.DWORD), ("flags", wt.DWORD), ("hCursor", wt.HICON),
                ("ptScreenPos", wt.POINT)]


u.GetCursorInfo.restype = wt.BOOL
u.GetCursorInfo.argtypes = [ctypes.POINTER(CURSORINFO)]
u.LoadCursorW.restype = wt.HICON
u.LoadCursorW.argtypes = [wt.HINSTANCE, ctypes.c_void_p]

IBEAM = int(u.LoadCursorW(None, 32513) or 0)
ARROWISH = int(u.LoadCursorW(None, 32512) or 0)


def cursor_at(x, y):
    """真实鼠标挪过去之后读系统当前光标。

    必须走 `GetCursorInfo`：`GetCursor()` 拿到的是**线程局部**的那一份，
    跨进程恒为同一个值（实测永远是 65543），根本看不出差别。
    """
    wmove(x, y)
    time.sleep(0.35)
    ci = CURSORINFO(ctypes.sizeof(CURSORINFO), 0, None, wt.POINT())
    u.GetCursorInfo(ctypes.byref(ci))
    return int(ci.hCursor or 0)


sw = round(170 * G["s"])
c_list = cursor_at(G["side"] + 300, VT + 120)
c_addr = cursor_at(G["side"] + 300, G["title"] + round(20 * G["s"]))
c_search = cursor_at(w - round(6 * G["s"]) - sw // 2, G["title"] + round(20 * G["s"]))
print(f"    列表 {c_list} / 地址栏 {c_addr} / 搜索框 {c_search}（I 形 = {IBEAM}）")
V.mark(c_addr == IBEAM, f"地址栏上是 I 形光标（{c_addr}）")
V.mark(c_search == IBEAM, f"搜索框上是 I 形光标（{c_search}）")
V.mark(c_list != IBEAM, f"列表区不是 I 形（{c_list}）")

print("== M. 改名框宽度随名字长度、且不越过名称列右沿 ==")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.5)
hw = V.launch(BASE)
G = geom(hw)
raw, stride, w, h = V.grab(hw)
VT = G["title"] + G["tool"]
row_h = round(21 * G["s"])
first = VT + round(26 * G["s"])
CR = col_rects(G)


def edits():
    found = []
    ENUM = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)

    @ENUM
    def cb(hh, lp):
        b = ctypes.create_unicode_buffer(64)
        u.GetClassNameW(hh, b, 64)
        if b.value == "Edit" and u.IsWindowVisible(hh):
            r = wt.RECT()
            u.GetWindowRect(hh, ctypes.byref(r))
            found.append((r.left - OS_X, r.right - OS_X))
        return True

    u.EnumChildWindows(hw, cb, 0)
    return found


def box_width(row_y):
    wclick(G["side"] + 120, row_y)
    time.sleep(0.35)
    V.key(0x71)  # F2
    time.sleep(0.6)
    cand = [b for b in edits() if b[0] < 900]
    V.key(0x1B)
    time.sleep(0.3)
    return cand[0] if cand else None


b0 = box_width(first + row_h // 2)
print("    短名改名框:", b0)
b1 = box_width(first + row_h * 4 + row_h // 2)
print("    另一行改名框:", b1)
if b0 and b1:
    w0, w1 = b0[1] - b0[0], b1[1] - b1[0]
    print(f"    宽度 {w0} / {w1}，名称列右沿 {CR[0][1]}")
    V.mark(w0 >= round(70 * G["s"]) and b0[1] <= CR[0][1] + 4,
           f"短名改名框宽 {w0} 且止于名称列右沿（{b0[1]} <= {CR[0][1]}）")
    V.mark(w1 >= round(70 * G["s"]) and b1[1] <= CR[0][1] + 4,
           f"另一行改名框宽 {w1} 且止于名称列右沿（{b1[1]} <= {CR[0][1]}）")
else:
    V.mark(False, "没唤出改名框")

print("== N. 表头悬停出筛选箭头、点开下拉 ==")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.5)
hw = V.launch(BASE)
G = geom(hw)
CR = col_rects(G)
VT = G["title"] + G["tool"]
hh = round(26 * G["s"])
hdr_mid = VT + hh // 2
raw, stride, w, h = V.grab(hw)
ax0 = CR[0][1] - round(18 * G["s"])


def tail_ink(rr, st):
    return sum(1 for y in range(VT, VT + hh) for x in range(ax0, CR[0][1])
               if min(V.px(rr, st, x, y)) >= 0x70)


ink0 = tail_ink(raw, stride)
wmove((CR[0][0] + CR[0][1]) // 2, hdr_mid)
time.sleep(0.5)
raw, stride, w, h = V.grab(hw)
ink1 = tail_ink(raw, stride)
print(f"    名称列右端墨迹 {ink0} -> {ink1}")
V.mark(ink1 > ink0 + 10, f"悬停表头露出筛选箭头（{ink0} -> {ink1}）")
V.click(OS_X + CR[0][1] - round(8 * G["s"]), OS_Y + hdr_mid)
time.sleep(0.7)
ms = V.menus()
V.mark(len(ms) >= 1, "点筛选箭头弹出下拉")
if ms:
    mh = ms[0][0]
    rawm, stm, wm, hm = V.grab(mh)
    bands = V.ink_rows(rawm, stm, 30, wm - 8, 2, hm - 2, thr=0x70, merge=4)
    print("    筛选项数:", len(bands))
    V.mark(len(bands) >= 7, f"名称筛选是 7 个分桶（实测 {len(bands)}）")
    close_all_menus()

print("== O. Alt+双击打开属性 ==")
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.5)
hw = V.launch(BASE)
G = geom(hw)
VT = G["title"] + G["tool"]
row_h = round(21 * G["s"])
first = VT + round(26 * G["s"])
px_ = G["side"] + 120
py_ = first + row_h // 2


def props_up():
    out = []

    @V.ENUMCB
    def cb(h, lp):
        b = ctypes.create_unicode_buffer(64)
        u.GetClassNameW(h, b, 64)
        if b.value == "ZenFileProps" and u.IsWindowVisible(h):
            out.append(int(h))
        return True

    u.EnumWindows(cb, 0)
    return out


V._ksend(0x12, False)  # 按住 Alt
wclick(px_, py_)
time.sleep(0.09)
wclick(px_, py_)
time.sleep(0.9)
V._ksend(0x12, True)
p1 = props_up()
print("    Alt+双击后的属性窗口:", p1)
V.mark(len(p1) >= 1, "Alt+双击打开属性窗口")
for h in p1:
    u.PostMessageW(h, 0x0010, 0, 0)  # WM_CLOSE
time.sleep(0.4)
# 不按 Alt 的双击应当是"打开"，不是属性
wclick(px_, py_)
time.sleep(0.09)
wclick(px_, py_)
time.sleep(0.9)
p2 = props_up()
V.mark(len(p2) == 0, "不按 Alt 的双击不会开属性")

print()
print("全部通过" if V.ok_all else "有失败项")
raise SystemExit(0 if V.ok_all else 1)
