"""第55轮验收：设置-右键菜单按类型管理（ContextMenuManager 式）+ 每一类都有「查找可用的打开软件」。

判据全部是**真窗口事实**（像素 / 窗口存在），不靠肉眼：

  M1 真窗口：右键**文件夹** → 菜单第 2 行「打开方式」能弹出「打开方式」对话框
  M2 真窗口：右键**空白处** → 「打开方式（此文件夹）」能弹出同一个对话框
  S1 设置页顶部出现 5 个类型 chip，当前选中的那个是 accent 色
  S2 点另一个 chip → 选中态转移过去
  S3 切类型 → 左侧按该类型渲染的菜单跟着变（文件 8 行 / 目录背景 7 行）
  S4 右上「查找可用的打开软件」按钮点了以后按钮文字真的变了

分两段跑：先开真目录测菜单（M1/M2），再 `shell:Settings` 直达设置页测 S1~S4
（设置页是虚拟标签，用启动参数进去比点「更多 → 设置」稳）。
"""
import atexit
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

CFG_DIR = os.path.join(os.environ.get("APPDATA", ""), "ZenFile")
UIS = os.path.join(CFG_DIR, "ui_settings.json")
_bak = None
if os.path.exists(UIS):
    with open(UIS, "rb") as f:
        _bak = f.read()


def _restore_ui():
    """「右键菜单」页会把每一类的排布写进 ui_settings.json —— 跑完必须还原，
    否则用户下次打开软件带着脚本留下的排布跑。"""
    try:
        if _bak is not None:
            with open(UIS, "wb") as f:
                f.write(_bak)
        elif os.path.exists(UIS):
            os.remove(UIS)
    except OSError:
        pass


atexit.register(_restore_ui)

BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "ZenR55")
os.makedirs(os.path.join(BASE, "subdir"), exist_ok=True)
with open(os.path.join(BASE, "a.txt"), "w") as f:
    f.write("x")

ACCENT = (67, 117, 206)   # theme.rs accent = rgb(0x4375CE) -> (R,G,B)
OS_X, OS_Y = 60, 60       # launch() 里 SetWindowPos 定的窗口左上角


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
    """横扫一条横带：返回有墨的连续列段（用来定位一横排 chip / 标签）。"""
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


def find_ow():
    return bool(u.FindWindowW("ZenFileOpenWith", None))


def close_ow():
    h = u.FindWindowW("ZenFileOpenWith", None)
    if h:
        u.PostMessageW(h, 0x0010, 0, 0)
        V.time.sleep(0.5)


def close_all_menus():
    for mh, _r in V.menus():
        V.time.sleep(0.05)
        u.PostMessageW(mh, 0x0100, 0x1B, 0)
        u.PostMessageW(mh, 0x0101, 0x1B, 0)
        V.time.sleep(0.35)


def menu_bands(mh):
    """菜单里每一条文字行的中心 y（窗口内坐标）。"""
    raw, stride, w, h = V.grab(mh)
    bands = V.ink_rows(raw, stride, 30, w - 8, 2, h - 2, thr=0x70, merge=4)
    return [(b[0] + b[1]) // 2 for b in bands]


# ======================================================================== M1 / M2
print("== M1 右键**文件夹** -> 第 2 行「打开方式」==")
hw = V.launch(BASE)
G = geom(hw)
raw, stride, W, H = V.grab(hw)
# 跳过 26px 表头：把表头文字当成第一行会右键到表头上去（弹的是列菜单）
rows = V.ink_rows(raw, stride, G["side"] + 40, W - 40,
                  G["view_top"] + round(28 * G["s"]), G["view_bottom"] - 4,
                  thr=0x70, merge=4)
print("    列表行带:", rows[:6])
ok_m1 = False
if rows:
    y0 = (rows[0][0] + rows[0][1]) // 2
    V.rclick(OS_X + G["side"] + 300, OS_Y + y0)
    V.time.sleep(0.9)
    ms = V.menus()
    if not ms:
        print("    FAIL 菜单没弹出")
    else:
        mh = ms[0][0]
        cs = menu_bands(mh)
        print(f"    菜单文字行 {len(cs)} 条，前 4 条中心 y={cs[:4]}")
        mr = wt.RECT()
        u.GetWindowRect(mh, ctypes.byref(mr))
        if len(cs) >= 2:
            V.click(mr.left + 60, mr.top + cs[1])
            V.time.sleep(1.3)
            ok_m1 = find_ow()
            print(f"    点第 2 行 -> 「打开方式」对话框: {ok_m1}")
            close_ow()
        close_all_menus()
V.mark(ok_m1, "M1 右键文件夹 -> 「打开方式」能弹出（文件夹也找得到可用的打开软件）")

print("== M2 右键**空白处** -> 「打开方式（此文件夹）」==")
V.rclick(OS_X + G["side"] + 300, OS_Y + G["view_bottom"] - 40)
V.time.sleep(0.9)
ok_m2 = False
ms = V.menus()
if not ms:
    print("    FAIL 背景菜单没弹出")
else:
    mh = ms[0][0]
    cs = menu_bands(mh)
    print(f"    背景菜单文字行 {len(cs)} 条")
    mr = wt.RECT()
    u.GetWindowRect(mh, ctypes.byref(mr))
    # 背景菜单自有 7 项：新建/全选/刷新/查看/排序方式/打开方式（此文件夹）/属性
    if len(cs) >= 6:
        V.click(mr.left + 60, mr.top + cs[5])
        V.time.sleep(1.3)
        ok_m2 = find_ow()
        print(f"    点第 6 条 -> 「打开方式」对话框: {ok_m2}")
        close_ow()
    close_all_menus()
V.mark(ok_m2, "M2 右键空白处 -> 「打开方式（此文件夹）」能弹出")

# ======================================================================== 设置页
print("== 进设置页（shell:Settings）==")
hw = V.launch("shell:Settings")
G = geom(hw)
s = G["s"]
tab_y0 = G["view_top"] + round(10 * s)
tab_y1 = tab_y0 + round(30 * s)
raw, stride, W, H = V.grab(hw)
tabs = col_runs(raw, stride, G["side"] + 4, W - 4, tab_y0, tab_y1)
print("    顶部小标签:", len(tabs), tabs[:9])
V.mark(len(tabs) == 8, f"设置页 8 个小标签（实测 {len(tabs)}）")
if len(tabs) >= 4:
    a, b = tabs[3]
    V.click(OS_X + (a + b) // 2, OS_Y + (tab_y0 + tab_y1) // 2)
    V.time.sleep(1.5)

print("== S1 类型 chip（文件/文件夹/目录背景/多选/驱动器）==")
chip_y0 = tab_y1 + round(12 * s)
chip_y1 = chip_y0 + round(28 * s)
raw, stride, W, H = V.grab(hw)
chips = col_runs(raw, stride, G["side"] + 4, W - 4, chip_y0, chip_y1)
print("    chip:", len(chips), chips[:8])
V.mark(len(chips) == 5, f"顶部 5 个类型 chip（实测 {len(chips)}）")


def chip_accent(i):
    raw, stride, W2, H2 = V.grab(hw)
    a, b = chips[i]
    area = (b - a) * (chip_y1 - chip_y0)
    n = V.count(raw, stride, a, b, chip_y0 + 2, chip_y1 - 2, ACCENT, tol=18)
    return n, area


if len(chips) == 5:
    n0, a0 = chip_accent(0)
    print(f"    chip0 accent {n0}/{a0}")
    V.mark(n0 >= a0 * 0.5, "S1 默认选中的「文件」chip 是 accent 色")

    print("== S2 点「文件夹」chip -> 选中态转移 ==")
    a, b = chips[1]
    V.click(OS_X + (a + b) // 2, OS_Y + (chip_y0 + chip_y1) // 2)
    V.time.sleep(1.0)
    n1, _ = chip_accent(1)
    n0b, _ = chip_accent(0)
    print(f"    chip1 accent {n1}，chip0 accent {n0b}")
    V.mark(n1 >= a0 * 0.45 and n0b <= a0 * 0.15, "S2 选中态转移到「文件夹」")

    print("== S3 切类型 -> 左侧渲染的菜单跟着换 ==")
    panel_l = G["side"] + round(16 * s)
    mw_px = min(round(300 * s), ((W - round(16 * s)) - panel_l) * 42 // 100)
    panel_r = panel_l + mw_px
    panel_t = chip_y1 + round(10 * s)
    panel_b = G["view_bottom"] - round(10 * s)

    def preview_bands():
        raw, stride, W2, H2 = V.grab(hw)
        return V.ink_rows(raw, stride, panel_l + 30, panel_r - 6, panel_t + 2,
                          panel_b - 2, thr=0x70, merge=4)

    def goto(i):
        a, b = chips[i]
        V.click(OS_X + (a + b) // 2, OS_Y + (chip_y0 + chip_y1) // 2)
        V.time.sleep(1.5)

    goto(0)  # 文件
    fl = preview_bands()
    print(f"    「文件」菜单行 {len(fl)}")
    goto(2)  # 目录背景
    bg = preview_bands()
    print(f"    「目录背景」菜单行 {len(bg)}")
    # 文件：打开/打开方式/剪切/复制/粘贴/重命名/删除/属性 = 8
    V.mark(len(fl) == 8, f"S3a 文件类型渲染 8 行（含「打开方式」），实测 {len(fl)}")
    # 目录背景：新建/全选/刷新/查看/排序方式/打开方式（此文件夹）/属性 = 7
    V.mark(len(bg) == 7, f"S3b 目录背景渲染 7 行（含「打开方式（此文件夹）」），实测 {len(bg)}")

    print("== S4 「查找可用的打开软件」按钮 ==")
    goto(0)
    ow_bottom = panel_t + (panel_b - panel_t) * 62 // 100
    fh = round(30 * s)
    fy0, fy1 = ow_bottom - fh, ow_bottom
    fx0, fx1 = panel_r + round(14 * s), W - round(16 * s)

    def ink():
        raw, stride, W2, H2 = V.grab(hw)
        return sum(1 for y in range(fy0 + 3, fy1 - 3) for x in range(fx0, fx1)
                   if min(V.px(raw, stride, x, y)) >= 0x70)

    before = ink()
    V.click(OS_X + (fx0 + fx1) // 2, OS_Y + (fy0 + fy1) // 2)
    V.time.sleep(1.8)
    after = ink()
    print(f"    按钮区墨点 {before} -> {after}")
    V.mark(before > 30 and after > before * 1.2,
           f"S4 点了「查找可用的打开软件」按钮文字变了（{before} -> {after}）")

print("\n总判定:", "PASS" if V.ok_all else "FAIL")
raise SystemExit(0 if V.ok_all else 1)
