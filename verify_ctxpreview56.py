"""第56轮验收：设置-右键菜单改成**预览形式**（右侧 40% 格式样本网格）。

判据全是**真窗口事实**（像素 / 弹出来的菜单行数），不靠肉眼：

  P0 右 40% 那块真的铺满了格式样本（大图标：彩色像素数量）
  P1 分字母排布：字母徽标全在**每一行的最左边**（一个字母一段，换段就换行）
  P2 鼠标停在某一格上 → 那一格亮起来（悬停反馈）
  P3 右键某一格 → 弹**该格式的右键菜单**（文件菜单 8 行）
  P4 在预览菜单里点一条 → **不会真的执行**，标题行闪一句提示
  P5 空白处右键 → 弹**目录右键**（背景菜单 7 行）
  P6 滚轮能滚（格子铺不下 → 上下滚得动）

进设置页用启动参数 `shell:Settings`（比点「更多 → 设置」稳）。
"""
import ctypes
import ctypes.wintypes as wt
import os

import verify_env

verify_env.isolate()

import verify_common as V

u = V.u
u.FindWindowW.restype = wt.HWND
u.FindWindowW.argtypes = [ctypes.c_wchar_p, ctypes.c_wchar_p]
u.GetDpiForWindow.restype = ctypes.c_uint
u.GetDpiForWindow.argtypes = [wt.HWND]
u.PostMessageW.restype = ctypes.c_bool
u.PostMessageW.argtypes = [wt.HWND, ctypes.c_uint, ctypes.c_size_t, ctypes.c_ssize_t]

ACCENT = (67, 117, 206)   # theme.rs accent = rgb(0x4375CE)
OS_X, OS_Y = 60, 60       # launch() 里 SetWindowPos 定的窗口左上角
WM_MOUSEWHEEL = 0x020A


def geom(hw):
    dpi = u.GetDpiForWindow(hw) or 96
    s = dpi / 96.0
    r = wt.RECT()
    u.GetWindowRect(hw, ctypes.byref(r))
    W, H = r.right - r.left, r.bottom - r.top
    title, tool, status = round(34 * s), round(40 * s), round(23 * s)
    return {"s": s, "W": W, "H": H, "title": title, "tool": tool, "status": status,
            "side": round(157 * s), "view_top": title + tool, "view_bottom": H - status}


def col_runs(raw, stride, x0, x1, y0, y1, thr=0x70, gap=6):
    """横扫一条横带：返回有墨的连续列段（用来定位一横排标签 / chip）。"""
    has = [any(min(V.px(raw, stride, x, y)) >= thr for y in range(y0, y1))
           for x in range(x0, x1)]
    runs, cur = [], None
    for i, h in enumerate(has):
        if h and cur is None:
            cur = i
        elif not h and cur is not None:
            runs.append((x0 + cur, x0 + i))
            cur = None
    if cur is not None:
        runs.append((x0 + cur, x1))
    out = []
    for a, b in runs:
        if out and a - out[-1][1] <= gap:
            out[-1] = (out[-1][0], b)
        else:
            out.append((a, b))
    return out


def colorful(raw, stride, x0, x1, y0, y1, d=40):
    """数"彩色"像素（三通道最大差值 > d）—— 大图标就是一片彩色。"""
    n = 0
    for y in range(y0, y1, 2):
        for x in range(x0, x1, 2):
            p = V.px(raw, stride, x, y)
            if max(p) - min(p) > d:
                n += 1
    return n


def diff_px(a, sa, b, sb, x0, x1, y0, y1):
    n = 0
    for y in range(y0, y1, 2):
        for x in range(x0, x1, 2):
            pa, pb = V.px(a, sa, x, y), V.px(b, sb, x, y)
            if abs(pa[0] - pb[0]) + abs(pa[1] - pb[1]) + abs(pa[2] - pb[2]) > 24:
                n += 1
    return n


def menu_bands(mh):
    raw, stride, w, h = V.grab(mh)
    bands = V.ink_rows(raw, stride, 30, w - 8, 2, h - 2, thr=0x70, merge=4)
    return [(b[0] + b[1]) // 2 for b in bands]


def close_all_menus():
    for mh, _r in V.menus():
        V.time.sleep(0.05)
        u.PostMessageW(mh, 0x0100, 0x1B, 0)
        u.PostMessageW(mh, 0x0101, 0x1B, 0)
        V.time.sleep(0.35)


# ============================================================ 进设置页 → 右键菜单页
print("== 进设置页（shell:Settings）→ 右键菜单页 ==")
hw = V.launch("shell:Settings")
G = geom(hw)
s = G["s"]
W, H = G["W"], G["H"]
tab_y0 = G["view_top"] + round(10 * s)
tab_y1 = tab_y0 + round(30 * s)
raw, stride, W2, H2 = V.grab(hw)
tabs = col_runs(raw, stride, G["side"] + 4, W - 4, tab_y0, tab_y1)
print("    顶部小标签:", len(tabs))
assert len(tabs) >= 4, "没找到设置页小标签"
a, b = tabs[3]      # 0界面 1外观 2交互 3右键菜单
V.click(OS_X + (a + b) // 2, OS_Y + (tab_y0 + tab_y1) // 2)
V.time.sleep(1.6)

# —— 照 settings::geom() 推右 40% 预览面板的矩形 ——
content_l = G["side"] + round(16 * s)
content_r = W - round(16 * s)
content_t = tab_y1 + round(12 * s)
content_b = G["view_bottom"] - round(10 * s)
total_w = content_r - content_l
left_w = total_w * 60 // 100
gap = round(12 * s)
pv_l = content_l + left_w + gap // 2
pv_r = content_r
pv_t = content_t
pv_b = content_b
head = round(26 * s)
vw_l, vw_r = pv_l + round(2 * s), pv_r - round(2 * s)
vw_t, vw_b = pv_t + head, pv_b - round(2 * s)
pad = round(8 * s)
cell_w, cell_h, letter_w = round(86 * s), round(74 * s), round(24 * s)
x0 = vw_l + pad
print(f"    预览面板 ({pv_l},{pv_t})-({pv_r},{pv_b})  列表区 ({vw_l},{vw_t})-({vw_r},{vw_b})")

# 第一格：字母徽标之后那一格（第一组是 '#'，里面是 3GP）
c1x = x0 + letter_w + cell_w // 2
c1y = vw_t + cell_h // 2
cell1 = (x0 + letter_w, vw_t, x0 + letter_w + cell_w, vw_t + cell_h)

# ---------------------------------------------------------------- P0
print("== P0 右 40% 铺着格式样本（大图标）==")
raw, stride, W2, H2 = V.grab(hw)
n_col = colorful(raw, stride, vw_l, vw_r, vw_t, vw_b)
print(f"    列表区彩色像素 {n_col}")
V.mark(n_col > 400, f"P0 预览区铺着大图标（彩色像素 {n_col} > 400）")

# ---------------------------------------------------------------- P1
print("== P1 分字母：字母徽标都在每行最左边 ==")
raw, stride, W2, H2 = V.grab(hw)
n_in = V.count(raw, stride, x0 - 2, x0 + round(26 * s), vw_t, vw_b, ACCENT, tol=10)
n_out = V.count(raw, stride, x0 + round(28 * s), vw_r, vw_t, vw_b, ACCENT, tol=10)
print(f"    徽标条内 accent {n_in}，条外 accent {n_out}")
V.mark(n_in > 20 and n_out <= n_in * 0.6,
       f"P1 字母徽标只出现在行首（{n_in} vs {n_out}）→ 一个字母一段，换段换行")

# ---------------------------------------------------------------- P2
print("== P2 悬停某一格 → 那一格亮起来 ==")
raw_a, sa, _, _ = V.grab(hw)
V.move(OS_X + c1x, OS_Y + c1y)
V.time.sleep(0.6)
raw_b, sb, _, _ = V.grab(hw)
d = diff_px(raw_a, sa, raw_b, sb, cell1[0] + 4, cell1[2] - 4, cell1[1] + 4, cell1[3] - 4)
print(f"    第 1 格像素变化 {d}")
V.mark(d > 30, f"P2 悬停那一格 → 该格高亮（变化像素 {d}）")

# ---------------------------------------------------------------- P3
print("== P3 右键某一格 → 弹这个格式的右键菜单 ==")
ok_p3, rows3 = False, 0
V.rclick(OS_X + c1x, OS_Y + c1y)
V.time.sleep(1.0)
ms = V.menus()
if not ms:
    print("    FAIL 菜单没弹出")
else:
    mh = ms[0][0]
    cs = menu_bands(mh)
    rows3 = len(cs)
    print(f"    文件菜单文字行 {rows3} 条，中心 y={cs[:4]}")
    # 文件：打开/打开方式/剪切/复制/粘贴/重命名/删除/属性 = 8
    ok_p3 = rows3 == 8
    if cs:
        mr = wt.RECT()
        u.GetWindowRect(mh, ctypes.byref(mr))
        # ------------------------------------------------------------ P4
        print("== P4 预览菜单里点一条 → 不真的执行 + 标题闪提示 ==")
        V.click(mr.left + 60, mr.top + cs[0])
        V.time.sleep(0.8)
        raw, stride, W2, H2 = V.grab(hw)
        tabs2 = col_runs(raw, stride, G["side"] + 4, W - 4, tab_y0, tab_y1)
        hint = V.count(raw, stride, pv_l + 6, pv_r - 6, pv_t + 2, pv_t + head - 2,
                       ACCENT, tol=24)
        print(f"    点「打开」后：小标签 {len(tabs2)} 个（仍在设置页），标题行 accent {hint}")
        V.mark(len(tabs2) == 8, "P4a 点了预览菜单也没跳走（仍在设置页 = 没真的执行）")
        V.mark(hint > 20, f"P4b 标题行闪出提示（accent 像素 {hint}）")
    close_all_menus()
V.mark(ok_p3, f"P3 右键格式样本 → 弹文件右键菜单（{rows3} 行，应为 8）")


# ---------------------------------------------------------------- P5
def wheel(x, y, delta=-120, n=1):
    for _ in range(n):
        lp = (((OS_Y + y) & 0xFFFF) << 16) | ((OS_X + x) & 0xFFFF)
        wp = (delta << 16) & 0xFFFFFFFF
        u.PostMessageW(hw, WM_MOUSEWHEEL, wp, lp)
        V.time.sleep(0.03)
    V.time.sleep(0.5)


print("== P6 滚轮能滚（格子铺不下）==")
raw_a, sa, _, _ = V.grab(hw)
wheel(c1x, c1y, -120, 6)
raw_b, sb, _, _ = V.grab(hw)
d = diff_px(raw_a, sa, raw_b, sb, vw_l, vw_r, vw_t, vw_b)
print(f"    滚 6 格后列表区像素变化 {d}")
V.mark(d > 200, f"P6 滚轮真的滚了（变化像素 {d}）")

print("== P5 空白处右键 → 目录右键 ==")
wheel(c1x, c1y, -120, 60)          # 一路滚到底（末尾留了一段空白）
V.time.sleep(0.6)
ok_p5, rows5 = False, 0
V.rclick(OS_X + vw_r - 16, OS_Y + vw_b - 12)
V.time.sleep(1.0)
ms = V.menus()
if not ms:
    print("    FAIL 空白处没弹出菜单")
else:
    mh = ms[0][0]
    cs = menu_bands(mh)
    rows5 = len(cs)
    print(f"    背景菜单文字行 {rows5} 条")
    # 目录背景：新建/全选/刷新/查看/排序方式/打开方式（此文件夹）/属性 = 7
    ok_p5 = rows5 == 7
    close_all_menus()
V.mark(ok_p5, f"P5 空白处右键 → 目录右键（{rows5} 行，应为 7）")

print("\n总判定:", "PASS 预览式右键菜单" if V.ok_all else "FAIL")
raise SystemExit(0 if V.ok_all else 1)
