"""第十轮验证（15 项里能像素化/配置化判定的部分）：

1) 大图标排成**网格**（横向排满再换行）
2) 列表内容被裁在可视区：滚动时**不会**越到工具栏 / 表头上（表头置顶且不透明）
3) 标题栏三大金刚键（最小化 / 最大化 / 关闭）真的能用
4) 设置页：以标签页形式打开，点开关即时生效并落盘
5) 侧栏回收站能打开（条目直读 $Recycle.Bin）
6) 空白处右键不再判定到条目上（选中高亮必须消失）
7) 属性对话框有内容（不是一片空白）

判定一律抓屏取像素 + 读配置文件，不用"看着像"。
素材全部落在 %TEMP%，绝不动桌面。
"""
import ctypes
import ctypes.wintypes as wt
import json
import os
import shutil
import struct
import subprocess

# 脚本会改写 %APPDATA%\NexFile 下的设置来构造场景 —— 必须隔离，否则跑完
# 用户自己的软件就带着脚本遗留的状态运行（详见 verify_env.py 的说明）。
import verify_env

verify_env.isolate()
import time

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
u.PostMessageW.restype = ctypes.c_bool
u.PostMessageW.argtypes = [wt.HWND, ctypes.c_uint, ctypes.c_ulong, ctypes.c_long]
u.GetClassNameW.restype = ctypes.c_int
u.GetClassNameW.argtypes = [wt.HWND, ctypes.c_wchar_p, ctypes.c_int]
u.EnumWindows.restype = wt.BOOL
u.IsWindowVisible.restype = wt.BOOL
u.IsIconic.restype = wt.BOOL
u.IsZoomed.restype = wt.BOOL
u.GetDpiForWindow.restype = ctypes.c_uint
u.GetDpiForWindow.argtypes = [wt.HWND]
ENUMCB = ctypes.WINFUNCTYPE(ctypes.c_bool, wt.HWND, ctypes.c_long)
u.EnumWindows.argtypes = [ENUMCB, ctypes.c_long]
g.CreateCompatibleDC.argtypes = [wt.HDC]
g.CreateCompatibleDC.restype = wt.HDC
g.CreateCompatibleBitmap.restype = wt.HBITMAP
g.CreateCompatibleBitmap.argtypes = [wt.HDC, ctypes.c_int, ctypes.c_int]
g.SelectObject.argtypes = [wt.HDC, wt.HGDIOBJ]
g.SelectObject.restype = wt.HGDIOBJ
g.GetDIBits.restype = ctypes.c_int
g.GetDIBits.argtypes = [wt.HDC, wt.HBITMAP, ctypes.c_uint, ctypes.c_uint,
                        ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint]

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\fm-app.exe"
APPDATA = os.environ.get("APPDATA", r"C:\Users\Administrator\AppData\Roaming")
CFG_DIR = os.path.join(APPDATA, "NexFile")
# 应用写的是"用户层" settings.json；settings.user.json 是最高优先级覆盖层，
# 测试如果写它，会把应用的写入整个盖住，读出来永远是初始值。
USER_CFG = os.path.join(CFG_DIR, "settings.json")
OVERRIDE_CFG = os.path.join(CFG_DIR, "settings.user.json")

WM_MOUSEMOVE = 0x0200
WM_LBUTTONDOWN = 0x0201
WM_LBUTTONUP = 0x0202
WM_RBUTTONUP = 0x0205
WM_MOUSEWHEEL = 0x020A
WM_NCLBUTTONDOWN = 0x00A1
WM_NCLBUTTONUP = 0x00A2
HTMINBUTTON, HTMAXBUTTON, HTCLOSE = 8, 9, 20

WINDOW_BG = (0x20, 0x20, 0x20)
SEL_BG = (0x47, 0x47, 0x47)

BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "NexVerify10")
NOTEPAD = os.path.join(os.environ.get("WINDIR", r"C:\Windows"), "System32", "notepad.exe")

ok_all = True


def mark(ok, msg):
    global ok_all
    ok_all = ok_all and ok
    print(f"    {'PASS' if ok else 'FAIL'}  {msg}")


def kill_all():
    subprocess.run(["taskkill", "/F", "/IM", "fm-app.exe"], capture_output=True)
    time.sleep(0.5)


def write_cfg(obj):
    os.makedirs(CFG_DIR, exist_ok=True)
    with open(USER_CFG, "w", encoding="utf-8") as f:
        json.dump(obj, f)
    try:
        os.remove(OVERRIDE_CFG)
    except OSError:
        pass


def read_cfg():
    try:
        with open(USER_CFG, encoding="utf-8") as f:
            return json.load(f)
    except Exception:
        return {}


def launch(arg=None, wait=2.5):
    kill_all()
    subprocess.Popen([EXE] + ([arg] if arg else []))
    hwnd = None
    for _ in range(80):
        time.sleep(0.25)
        hwnd = u.FindWindowW("NexFileMainWindow", None)
        if hwnd:
            break
    assert hwnd, "主窗口没起来"
    time.sleep(wait)
    return hwnd


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


def px(raw, stride, x, y):
    o = y * stride + x * 4
    return (raw[o + 2], raw[o + 1], raw[o])


def addr_text(hwnd):
    buf = (ctypes.c_wchar * 512)()
    u.SendMessageW(u.GetDlgItem(hwnd, 101), 0x000D, 512, ctypes.addressof(buf))
    return buf.value


def click(hwnd, x, y):
    u.PostMessageW(hwnd, WM_LBUTTONDOWN, 0, (y << 16) | (x & 0xFFFF))
    u.PostMessageW(hwnd, WM_LBUTTONUP, 0, (y << 16) | (x & 0xFFFF))
    time.sleep(0.35)


def rclick(hwnd, x, y):
    u.PostMessageW(hwnd, WM_RBUTTONUP, 0, (y << 16) | (x & 0xFFFF))
    time.sleep(0.5)


def nc_click(hwnd, hit, sx, sy):
    u.PostMessageW(hwnd, WM_NCLBUTTONDOWN, hit, (sy << 16) | (sx & 0xFFFF))
    u.PostMessageW(hwnd, WM_NCLBUTTONUP, hit, (sy << 16) | (sx & 0xFFFF))
    time.sleep(0.5)


def wheel(hwnd, ticks, delta=-120, cx=None, cy=None):
    """滚轮必须带**真实的屏幕坐标**：lParam=0 意味着光标在屏幕左上角，
    而左上角的 x 小于导航窗格右缘 —— 现在滚轮在侧栏上是滚树的，
    于是列表一动不动，脚本会误报"滚动无效"。"""
    r = wt.RECT()
    u.GetWindowRect(hwnd, ctypes.byref(r))
    w = r.right - r.left
    hh = r.bottom - r.top
    if cx is None:
        cx = w - 120  # 明确落在内容区（远在侧栏右边）
    if cy is None:
        cy = hh // 2
    lp = (((r.top + cy) & 0xFFFF) << 16) | ((r.left + cx) & 0xFFFF)
    for _ in range(ticks):
        u.PostMessageW(hwnd, WM_MOUSEWHEEL, (delta & 0xFFFF) << 16, lp)
        time.sleep(0.2)
    time.sleep(0.5)


def windows_of(cls):
    found = []

    @ENUMCB
    def cb(h, _lp):
        c = ctypes.create_unicode_buffer(64)
        u.GetClassNameW(h, c, 64)
        if c.value == cls:
            rr = wt.RECT()
            u.GetWindowRect(h, ctypes.byref(rr))
            found.append((int(h), rr.left, rr.top, rr.right - rr.left, rr.bottom - rr.top))
        return True

    u.EnumWindows(cb, 0)
    return found


def sc(scale, v):
    return int(round(v * scale))


def geom(hwnd):
    """布局几何一律现场推导，绝不复用被测代码的常量。"""
    s = u.GetDpiForWindow(hwnd) / 96.0
    r = wt.RECT()
    u.GetWindowRect(hwnd, ctypes.byref(r))
    return {
        "s": s,
        "w": r.right - r.left,
        "h": r.bottom - r.top,
        "title_h": sc(s, 34),
        "toolbar_h": sc(s, 40),
        "status_h": sc(s, 23),
        "sidebar_w": sc(s, 157),
        "header_h": sc(s, 26),
        "row_h": sc(s, 21),
        "tile_h": sc(s, 128),
        "menu_row": sc(s, 28),
        "btn_w": sc(s, 44),
        "left": r.left,
        "top": r.top,
    }


def segs_on_row(raw, stride, x0, x1, y, thr=150):
    xs = [x for x in range(x0, x1) if sum(px(raw, stride, x, y)) > thr]
    if not xs:
        return []
    out, start, prev = [], xs[0], xs[0]
    for x in xs[1:]:
        if x - prev > 24:
            out.append((start, prev))
            start = x
        prev = x
    out.append((start, prev))
    return out


def strip_diff(a, sa, b, sb, x0, x1, y0, y1):
    n = 0
    for y in range(y0, y1):
        for x in range(x0, x1):
            pa, pb = px(a, sa, x, y), px(b, sb, x, y)
            if abs(pa[0] - pb[0]) + abs(pa[1] - pb[1]) + abs(pa[2] - pb[2]) > 30:
                n += 1
    return n


def band_count(raw, stride, x0, x1, y0, y1, rgb, tol=6):
    """每一行里等于给定颜色的像素数，返回最大值（选中行是整条色带）。"""
    best = 0
    for y in range(y0, y1):
        c = 0
        for x in range(x0, x1, 2):
            p = px(raw, stride, x, y)
            if abs(p[0] - rgb[0]) <= tol and abs(p[1] - rgb[1]) <= tol and abs(p[2] - rgb[2]) <= tol:
                c += 1
        best = max(best, c)
    return best


def ink_count(raw, stride, x0, x1, y0, y1, thr=150):
    return sum(
        1 for y in range(y0, y1) for x in range(x0, x1)
        if sum(px(raw, stride, x, y)) > thr
    )


# ============================================================ 素材
print("=" * 70)
print("第十轮验证")
print(f"素材目录: {BASE}")
shutil.rmtree(BASE, ignore_errors=True)
os.makedirs(os.path.join(BASE, "Grid"), exist_ok=True)
# 60 个才能撑出滚动条（滚动前后对比是"有没有越界"的关键证据）
for i in range(60):
    open(os.path.join(BASE, "Grid", f"item{i}.txt"), "w").write("x" * (i + 1) * 37)
for n in ("app.exe", "app2.exe"):
    if os.path.exists(NOTEPAD):
        shutil.copyfile(NOTEPAD, os.path.join(BASE, "Grid", n))
    else:
        open(os.path.join(BASE, "Grid", n), "wb").write(b"MZ")
# 只有 3 个文件的目录：列表底部才有真正的"空白处"可供右键
os.makedirs(os.path.join(BASE, "Few"), exist_ok=True)
for i in range(3):
    open(os.path.join(BASE, "Few", f"f{i}.txt"), "w").write("x" * 17)

# ---------------------------------------------------------------- 1 大图标网格
print("\n[1] 大图标：网格排布 + 不越过标题栏")
write_cfg({"files": {"view": {"default_mode": "large_icons", "show_extensions": True}}})
hwnd = launch(os.path.join(BASE, "Grid"))
G = geom(hwnd)
view_top = G["title_h"] + G["toolbar_h"]
view_left = G["sidebar_w"]
raw, stride, w, h = grab(hwnd)

segs1 = segs_on_row(raw, stride, view_left + 4, w - 20, view_top + sc(G["s"], 40))
mark(len(segs1) >= 2, f"第一行横向排了 {len(segs1)} 个条目（≥2 才是网格，不是一列）")
segs2 = segs_on_row(raw, stride, view_left + 4, w - 20, view_top + G["tile_h"] + sc(G["s"], 40))
mark(len(segs2) >= 2, f"第二行横向排了 {len(segs2)} 个条目（说明会换行）")

# 滚动前后，工具栏条带必须**完全不变**：条目若越过表头/工具栏，条带会被画上内容
a, sa, _, _ = raw, stride, w, h
wheel(hwnd, 3)
b, sb, _, _ = grab(hwnd)
over = strip_diff(a, sa, b, sb, view_left, w, 0, view_top)
mark(over == 0, f"滚动后工具栏条带差异像素 = {over}（0 才说明没越界）")
body = strip_diff(a, sa, b, sb, view_left, w, view_top, h - G["status_h"])
mark(body > 200, f"滚动确实改变了列表区（差异 {body}），说明上面的对比有效")

# ---------------------------------------------------------------- 2 表头置顶且不透明
print("\n[2] 详细信息表头：置顶 + 背景不透明")
write_cfg({"files": {"view": {"default_mode": "details", "show_extensions": True}}})
hwnd = launch(os.path.join(BASE, "Grid"))
G = geom(hwnd)
view_top = G["title_h"] + G["toolbar_h"]
view_left = G["sidebar_w"]
header_h = G["header_h"]
list_top = view_top + header_h
a, sa, w, h = grab(hwnd)
wheel(hwnd, 3)
b, sb, _, _ = grab(hwnd)

mark(strip_diff(a, sa, b, sb, view_left, w, 0, view_top) == 0,
     "滚动后工具栏条带无变化（条目没溢出到工具栏）")
mark(strip_diff(a, sa, b, sb, view_left, w, view_top, list_top) == 0,
     "滚动后表头条带无变化（条目没压到表头上，表头置顶且不透明）")
mark(strip_diff(a, sa, b, sb, view_left, w, list_top, h - G["status_h"]) > 200,
     "滚动确实改变了列表区（对比有效）")

raw = b
stride = sb
band_bg = 0
for y in range(view_top + 2, list_top - 2):
    for x in range(view_left + 4, w - 4):
        if px(raw, stride, x, y) == WINDOW_BG:
            band_bg += 1
total = (header_h - 4) * (w - 8 - view_left)
mark(band_bg / max(1, total) > 0.8, f"表头带纯底色占比 {band_bg*100//max(1,total)}%（应 >80%）")
mark(ink_count(raw, stride, view_left + 4, w - 4, view_top, list_top) > 60, "表头文字已绘制")

# ---------------------------------------------------------------- 3 三大金刚键
print("\n[3] 标题栏三大金刚键")
hwnd = launch(None)
G = geom(hwnd)
cy = G["title_h"] // 2
bw = G["btn_w"]
nc_click(hwnd, HTMAXBUTTON, G["left"] + G["w"] - int(bw * 1.5), G["top"] + cy)
time.sleep(0.8)
zoomed = bool(u.IsZoomed(hwnd))
mark(zoomed, f"最大化生效（IsZoomed={zoomed}）")
if zoomed:
    r = wt.RECT()
    u.GetWindowRect(hwnd, ctypes.byref(r))
    nc_click(hwnd, HTMAXBUTTON, r.left + (r.right - r.left) - int(bw * 1.5), r.top + 22)
    time.sleep(0.8)
mark(not u.IsZoomed(hwnd), "再点一次最大化 → 还原")
r = wt.RECT()
u.GetWindowRect(hwnd, ctypes.byref(r))
nc_click(hwnd, HTMINBUTTON, r.left + (r.right - r.left) - int(bw * 2.5), r.top + 22)
time.sleep(0.8)
mark(bool(u.IsIconic(hwnd)), f"最小化生效（IsIconic={u.IsIconic(hwnd)}）")
u.ShowWindow(hwnd, 9)
time.sleep(0.8)
r = wt.RECT()
u.GetWindowRect(hwnd, ctypes.byref(r))
nc_click(hwnd, HTCLOSE, r.left + (r.right - r.left) - bw // 2, r.top + 22)
time.sleep(1.0)
mark(not u.IsWindowVisible(hwnd), "关闭生效（窗口不再可见）")
kill_all()

# ---------------------------------------------------------------- 4 设置页
print("\n[4] 设置页：标签页形式打开 + 开关即时生效并落盘")
write_cfg({"files": {"view": {"show_extensions": True, "default_mode": "details"}}})
hwnd = launch(os.path.join(BASE, "Grid"))
G = geom(hwnd)
view_top = G["title_h"] + G["toolbar_h"]
view_left = G["sidebar_w"]
# 标题栏「更多」按钮 → 菜单 → 第 10 项"设置"
click(hwnd, G["w"] - int(G["btn_w"] * 3.5), sc(G["s"], 17))
menus = windows_of("NexFileMenu")
mark(bool(menus), f"「更多」菜单弹出（{len(menus)} 个）")
if menus:
    mh = menus[0][0]
    mw = menus[0][3]
    row = G["menu_row"]
    # 顺序：新建标签页/刷新/sep/视图/排序方式/sep/隐藏导航窗格/隐藏状态栏/sep/打开回收站/设置
    y = 4 + row * 2 + 6 + row * 2 + 6 + row * 2 + 6 + row + row // 2
    click(mh, mw // 2, y)
time.sleep(0.6)
mark(addr_text(hwnd) == "设置", f"设置以标签页打开（地址栏=「{addr_text(hwnd)}」）")

before = read_cfg().get("files", {}).get("view", {}).get("show_extensions")
# 设置行几何：标题(30) + 8 + 组标题(30) + 前两行(34×2) → 第三行"显示文件扩展名"
s = G["s"]
y_row = (view_top + sc(s, 16) + sc(s, 30) + sc(s, 8) + sc(s, 30)
         + sc(s, 34) * 2 + sc(s, 34) // 2)
click(hwnd, view_left + sc(s, 60), y_row)
time.sleep(0.5)
after = read_cfg().get("files", {}).get("view", {}).get("show_extensions")
mark(before is True and after is False,
     f"「显示文件扩展名」点击生效并落盘（{before} → {after}）")
# 再点回去，顺便验证"大小单位"可以循环
click(hwnd, view_left + sc(s, 60), y_row + sc(s, 34))
time.sleep(0.5)
unit = read_cfg().get("files", {}).get("view", {}).get("size_unit")
mark(unit is not None, f"「大小单位」可切换并落盘（当前 {unit}）")

# ---------------------------------------------------------------- 5 回收站
print("\n[5] 侧栏回收站")


def sidebar_live_rows(hwnd, G):
    """侧栏里"有字"的行，返回 [(y_top, 墨迹像素数)]。

    不能按行号写死：侧栏现在是 主页 + 常用文件夹 + 此电脑 + 盘符 + 回收站 的树，
    盘符个数随机器的盘而变，写死第 6 行会点到"图片"上去。
    """
    raw, stride, w, h = grab(hwnd)
    s = G["s"]
    row = sc(s, 34)
    view_top = G["title_h"] + G["toolbar_h"]
    out = []
    y = view_top
    while y + row < h - sc(s, 40):
        ink = max(
            sum(1 for x in range(20, G["sidebar_w"] - 4)
                if sum(px(raw, stride, x, yy)) > 210)
            for yy in range(y + 2, y + row - 2)
        )
        if ink > 12:
            out.append((y, ink))
        y += row
    return out


hwnd = launch(None)
G = geom(hwnd)
rows_live = sidebar_live_rows(hwnd, G)
mark(len(rows_live) >= 10, f"侧栏可见 {len(rows_live)} 行（主页+常用文件夹+此电脑+盘符+回收站 ≥10）")
y_bin = rows_live[-1][0] + sc(G["s"], 34) // 2
click(hwnd, 90, y_bin)
time.sleep(1.2)
mark(addr_text(hwnd) == "回收站", f"侧栏回收站可打开（地址栏=「{addr_text(hwnd)}」）")
raw, stride, w, h = grab(hwnd)
view_left = G["sidebar_w"]
body_top = view_top + sc(G["s"], 44)
ink = ink_count(raw, stride, view_left + 10, w - 10, body_top, h - G["status_h"])
mark(ink > 40, f"回收站视图有内容（墨迹像素 {ink}）")
kill_all()

# ---------------------------------------------------------------- 6 空白处右键
print("\n[6] 空白处右键不再判定到条目上")
write_cfg({"files": {"view": {"default_mode": "details", "show_extensions": True}}})
# 用只有 3 个文件的目录，列表下面才真的有空白处
hwnd = launch(os.path.join(BASE, "Few"))
G = geom(hwnd)
view_top = G["title_h"] + G["toolbar_h"]
view_left = G["sidebar_w"]
list_top = view_top + G["header_h"]
row_h = G["row_h"]
click(hwnd, view_left + 250, list_top + row_h // 2)
raw, stride, w, h = grab(hwnd)
sel_before = band_count(raw, stride, view_left + 4, w - 4, list_top, h - G["status_h"], SEL_BG)
mark(sel_before > 200, f"点第一行后出现选中色带（最长 {sel_before} px）")
rclick(hwnd, view_left + 250, h - sc(G["s"], 60))
kill_all()
hwnd = launch(os.path.join(BASE, "Few"))
G = geom(hwnd)
view_top = G["title_h"] + G["toolbar_h"]
view_left = G["sidebar_w"]
list_top = view_top + G["header_h"]
click(hwnd, view_left + 250, list_top + G["row_h"] // 2)
raw, stride, w, h = grab(hwnd)
had = band_count(raw, stride, view_left + 4, w - 4, list_top, h - G["status_h"], SEL_BG)
# 右键空白处：落在所有条目之下
rclick(hwnd, view_left + 250, h - sc(G["s"], 60))
time.sleep(0.4)
raw, stride, w, h = grab(hwnd)
still = band_count(raw, stride, view_left + 4, w - 4, list_top, h - G["status_h"], SEL_BG)
mark(had > 200 and still < 60,
     f"空白处右键后选中色带消失（{had} → {still}）")

# ---------------------------------------------------------------- 7 属性对话框
print("\n[7] 属性对话框有内容")
kill_all()
hwnd = launch(os.path.join(BASE, "Grid"))
G = geom(hwnd)
view_top = G["title_h"] + G["toolbar_h"]
view_left = G["sidebar_w"]
list_top = view_top + G["header_h"]
click(hwnd, view_left + 250, list_top + G["row_h"] // 2)
rclick(hwnd, view_left + 250, list_top + G["row_h"] // 2)
menus = windows_of("NexFileMenu")
mark(bool(menus), f"右键菜单弹出（{len(menus)} 个：{[(m[3], m[4]) for m in menus]}）")
if menus:
    # 顶层菜单永远是"最高"的那一个（子菜单更矮）
    mh, _, _, mw, mhh = max(menus, key=lambda m: m[4])
    row = G["menu_row"]
    # 文件菜单顺序：0打开 1打开方式 2剪切 3复制 4重命名 5批量重命名 6删除 7[分隔]
    #              8粘贴 9新建 10[分隔] 11全选 12刷新 13查看 14排序方式 15属性
    # 属性顶边 = 4 + 7×28 + 6 + 2×28 + 6 + 4×28。只从这一带开始扫 ——
    # 从上面扫起会先点到"刷新"，菜单直接关掉，就再也扫不到属性了。
    for y in range(4 + row * 7 + 6 + row * 2 + 6 + row * 4, mhh - 4, max(8, row // 2)):
        click(mh, mw // 2, y)
        if windows_of("NexFileProps"):
            break
        if not windows_of("NexFileMenu"):
            break
    time.sleep(0.8)
props = windows_of("NexFileProps")
mark(bool(props), f"属性对话框弹出（{len(props)} 个）")
if props:
    ph = props[0][0]
    pw = props[0][3]
    praw, pstride, _, _ = grab(ph)
    ink = ink_count(praw, pstride, 20, pw - 20, 100, 300, thr=170)
    mark(ink > 300, f"属性对话框信息区有内容（墨迹像素 {ink}）")
kill_all()

print("\n" + "=" * 70)
print("第十轮全部通过" if ok_all else "第十轮存在失败项")
shutil.rmtree(BASE, ignore_errors=True)
