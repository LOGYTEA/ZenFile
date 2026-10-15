"""第九轮验证（7 项）：

1) exe 等文件的真实预览图标
2) 隐藏项 / 受保护系统文件：视觉区分 + 两道独立开关
3) 右键子菜单悬停展开、悬停高亮、悬停后菜单还能关掉
4) 状态栏右下角「大图标 / 详细信息」快速切换
5) 返回上一级后保留原进入文件夹的选中效果
6) 标签：点击切换 / 悬停关闭按钮 / 中键关闭
7) 自绘属性对话框（不是 Windows 原生），且打开更快

判定一律抓屏取像素，不用"看着像"。素材全部落在 %TEMP%，绝不动桌面。
"""
import ctypes
import ctypes.wintypes as wt
import json
import os
import shutil
import struct
import subprocess
import time

# 脚本会改写 %APPDATA%\ZenFile 下的设置来构造场景 —— 必须隔离，否则跑完
# 用户自己的软件就带着脚本遗留的状态运行（详见 verify_env.py 的说明）。
import verify_env

verify_env.isolate()

u = ctypes.WinDLL("user32", use_last_error=True)
g = ctypes.WinDLL("gdi32", use_last_error=True)
k = ctypes.WinDLL("kernel32", use_last_error=True)

u.FindWindowW.restype = wt.HWND
u.FindWindowW.argtypes = [ctypes.c_wchar_p, ctypes.c_wchar_p]
u.GetWindowRect.restype = wt.BOOL
u.GetWindowRect.argtypes = [wt.HWND, ctypes.POINTER(wt.RECT)]
u.GetWindowDC.restype = wt.HDC
u.PrintWindow.restype = wt.BOOL
u.PrintWindow.argtypes = [wt.HWND, ctypes.c_wchar_p and wt.HDC, ctypes.c_uint]
u.GetDlgItem.restype = wt.HWND
u.SendMessageW.restype = ctypes.c_ssize_t
u.PostMessageW.restype = ctypes.c_bool
u.PostMessageW.argtypes = [wt.HWND, ctypes.c_uint, ctypes.c_ulong, ctypes.c_long]
u.GetClassNameW.restype = ctypes.c_int
u.GetClassNameW.argtypes = [wt.HWND, ctypes.c_wchar_p, ctypes.c_int]
u.EnumWindows.restype = wt.BOOL
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
k.GetFileAttributesW.restype = ctypes.c_uint32
k.GetFileAttributesW.argtypes = [ctypes.c_wchar_p]

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\ZenFile.exe"
APPDATA = os.environ.get("APPDATA", r"C:\Users\Administrator\AppData\Roaming")
USER_CFG = os.path.join(APPDATA, "ZenFile", "settings.user.json")

WM_MOUSEMOVE = 0x0200
WM_LBUTTONDOWN = 0x0201
WM_LBUTTONUP = 0x0202
WM_LBUTTONDBLCLK = 0x0203
WM_MBUTTONDOWN = 0x0207
WM_RBUTTONUP = 0x0205
WM_XBUTTONDOWN = 0x020B
WM_KEYDOWN = 0x0100
WM_CLOSE = 0x0010

BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "ZenVerify9")
NOTEPAD = os.path.join(os.environ.get("WINDIR", r"C:\Windows"), "System32", "notepad.exe")

SEL_BG = (0x47, 0x47, 0x47)      # selection_bg
HOVER_BG = (0x3D, 0x3D, 0x3D)    # hover_bg

ok_all = True


def mark(ok, msg):
    global ok_all
    ok_all = ok_all and ok
    print(f"    {'PASS' if ok else 'FAIL'}  {msg}")


def kill_all():
    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    time.sleep(0.5)


def write_cfg(obj):
    os.makedirs(os.path.dirname(USER_CFG), exist_ok=True)
    with open(USER_CFG, "w", encoding="utf-8") as f:
        json.dump(obj, f)


def drop_cfg():
    try:
        os.remove(USER_CFG)
    except OSError:
        pass


def launch(arg=None, wait=2.5):
    kill_all()
    subprocess.Popen([EXE] + ([arg] if arg else []))
    hwnd = None
    for _ in range(80):
        time.sleep(0.25)
        hwnd = u.FindWindowW("ZenFileMainWindow", None)
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


def move(hwnd, x, y):
    u.PostMessageW(hwnd, WM_MOUSEMOVE, 0, (y << 16) | (x & 0xFFFF))
    time.sleep(0.12)


def key(hwnd, vk):
    u.PostMessageW(hwnd, WM_KEYDOWN, vk, 0)
    time.sleep(0.4)


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


def row_bands(raw, stride, w, h, x0, x1, y0, y1, thr=210):
    """在竖列区间里找"有墨迹"的横向行段。"""
    out, cur = [], None
    for y in range(y0, min(y1, h)):
        c = sum(1 for x in range(x0, min(x1, w))
                if sum(px(raw, stride, x, y)) > thr)
        if c > 2:
            if cur is None:
                cur = y
        else:
            if cur is not None:
                out.append((cur, y))
                cur = None
    if cur is not None:
        out.append((cur, min(y1, h)))
    return out


def box_sig(raw, stride, x, y, size):
    """取一块方形区域的像素签名（用于比较两个图标是否相同）。"""
    return [px(raw, stride, x + i, y + j) for j in range(size) for i in range(size)]


def box_ink(raw, stride, x, y, size):
    """一块区域的总亮度（用于判断图标是不是被调淡了）。"""
    return sum(sum(px(raw, stride, x + i, y + j)) for j in range(size) for i in range(size))


def diff_ratio(a, b):
    n = min(len(a), len(b))
    d = sum(1 for i in range(n) if sum(abs(x - y) for x, y in zip(a[i], b[i])) > 40)
    return d / max(1, n)


# ============================================================ 素材
print("=" * 70)
print("第九轮验证")
print(f"素材目录: {BASE}")
# 上一轮可能给素材加了只读/系统属性（属性对话框那一项会真的改写磁盘属性），
# 带这些属性的文件 os.remove 会直接 PermissionError —— 先清属性再删。
# 注意：本机 attrib 对 "-s -h -r /S /D" 这种写法报“参数格式不正确”，
# 所以一律用 Python 的 os.chmod 逐文件解除只读，再 rmtree。
def force_rmtree(path: str) -> None:
    if not os.path.isdir(path):
        return
    import stat as _st
    for root, dirs, files in os.walk(path):
        for n in dirs + files:
            try:
                os.chmod(os.path.join(root, n), _st.S_IWRITE | _st.S_IREAD)
            except OSError:
                pass
    shutil.rmtree(path, ignore_errors=True)


force_rmtree(BASE)
os.makedirs(os.path.join(BASE, "Icons"), exist_ok=True)
os.makedirs(os.path.join(BASE, "Attr"), exist_ok=True)
os.makedirs(os.path.join(BASE, "Nav", "Sub1"), exist_ok=True)
os.makedirs(os.path.join(BASE, "Nav", "Sub2"), exist_ok=True)
if os.path.exists(NOTEPAD):
    shutil.copyfile(NOTEPAD, os.path.join(BASE, "Icons", "app.exe"))
else:
    open(os.path.join(BASE, "Icons", "app.exe"), "wb").write(b"MZ")
for n in ["note.txt", "weird.qqq"]:
    open(os.path.join(BASE, "Icons", n), "w").write("x")
for n in ["normal.txt", "hidden.txt", "sys.txt"]:
    p = os.path.join(BASE, "Attr", n)
    open(p, "w").write("x")
subprocess.run(["attrib", "+h", os.path.join(BASE, "Attr", "hidden.txt")], capture_output=True)
subprocess.run(["attrib", "+s", os.path.join(BASE, "Attr", "sys.txt")], capture_output=True)
open(os.path.join(BASE, "Nav", "Sub1", "inner.txt"), "w").write("x")
open(os.path.join(BASE, "Nav", "Sub2", "inner.txt"), "w").write("x")
drop_cfg()

# ================================================== 1) exe 真实图标
print("\n===== 1) exe 等文件的预览图标 =====")
write_cfg({"files": {"view": {"default_mode": "list", "show_hidden": True, "show_system": False}}})
hwnd = launch(os.path.join(BASE, "Icons"))
raw, stride, w, h = grab(hwnd)
bands = row_bands(raw, stride, w, h, 170, 600, 90, h - 40)
print(f"  列表里 {len(bands)} 行")
sigs = []
for (y0, y1) in bands:
    # 图标是这一行最左边的墨迹块
    xs = [x for x in range(158, 260) if sum(px(raw, stride, x, (y0 + y1) // 2)) > 150]
    if not xs:
        continue
    ix = xs[0]
    sigs.append((ix, y0, box_sig(raw, stride, ix, y0, 16), box_ink(raw, stride, ix, y0, 16)))
print(f"  取到 {len(sigs)} 个图标块")
ok1 = len(sigs) >= 3
mark(ok1, f"列表里 3 个文件各画出了图标（{len(sigs)} 个）")
if len(sigs) >= 3:
    d01 = diff_ratio(sigs[0][2], sigs[1][2])
    d02 = diff_ratio(sigs[0][2], sigs[2][2])
    d12 = diff_ratio(sigs[1][2], sigs[2][2])
    print(f"    图标两两差异: {d01:.2f} {d02:.2f} {d12:.2f}")
    mark(min(d01, d02, d12) > 0.12, "exe / txt / 未知类型三者的图标互不相同（不是同一个通用图标）")
    mark(min(v[3] for v in sigs) > 2000, "图标区域确实有内容（不是空白方块）")
drop_cfg()

# ================================================== 2) 隐藏 / 系统
print("\n===== 2) 隐藏项与受保护系统文件 =====")


def attr_rows(cfg, tag):
    write_cfg({"files": {"view": {"default_mode": "list", "show_hidden": cfg[0], "show_system": cfg[1]}}})
    hw = launch(os.path.join(BASE, "Attr"))
    rw, st, ww, hh = grab(hw)
    bs = row_bands(rw, st, ww, hh, 170, 600, 90, hh - 40)
    print(f"  show_hidden={cfg[0]} show_system={cfg[1]} -> {len(bs)} 行")
    return hw, rw, st, ww, hh, bs


hw2, rw2, st2, ww2, hh2, bs2 = attr_rows((False, False), "both off")
mark(len(bs2) == 1, f"两道开关全关：只看到 normal.txt（{len(bs2)} 行）")

hw2, rw2, st2, ww2, hh2, bs2 = attr_rows((True, False), "hidden on")
mark(len(bs2) == 2, f"只开「隐藏的项目」：多出 hidden.txt，sys.txt 仍不显示（{len(bs2)} 行）")
inks = []
for (y0, y1) in bs2:
    xs = [x for x in range(158, 260) if sum(px(rw2, st2, x, (y0 + y1) // 2)) > 120]
    if xs:
        inks.append((xs[0], box_ink(rw2, st2, xs[0], y0, 16)))
if len(inks) == 2:
    # 排序后 normal 在前；两个都是 .txt，底图相同，隐藏项应明显更暗
    ratio = min(inks[0][1], inks[1][1]) / max(1, max(inks[0][1], inks[1][1]))
    print(f"    两行图标亮度 {inks[0][1]} / {inks[1][1]}  -> 较暗/较亮 = {ratio:.2f}")
    mark(ratio < 0.9, "隐藏项的图标被调淡（与普通项肉眼可区分）")

hw2, rw2, st2, ww2, hh2, bs2 = attr_rows((True, True), "both on")
mark(len(bs2) == 3, f"两道开关全开：三个文件都在（{len(bs2)} 行）")
drop_cfg()

# ================================================== 3) 菜单悬停 / 关得掉
print("\n===== 3) 右键子菜单：悬停展开 / 悬停高亮 / 能关掉 =====")
write_cfg({"files": {"view": {"default_mode": "list"}}})
hwnd = launch(os.path.join(BASE, "Icons"))
u.PostMessageW(hwnd, WM_RBUTTONUP, 0, (300 << 16) | (400 & 0xFFFF))
time.sleep(0.6)
menus = windows_of("ZenFileMenu")
print(f"  右键后菜单窗口 {len(menus)} 个")
mark(len(menus) == 1, "右键菜单弹出")
if menus:
    mh, mx, my, mw, mhh = menus[0]
    before = grab(mh)
    print(f"  菜单窗口 {mw}x{mhh}（扫描 y=6..{min(mhh - 4, 260)}）")
    # 逐行往下扫，直到出现第二个菜单窗口 = 悬停展开了子菜单
    sub = None
    sub_at = None
    seen = []
    for yy in range(6, min(mhh - 4, 260), 3):
        move(mh, mw // 2, yy)
        ms = windows_of("ZenFileMenu")
        seen.append(len(ms))
        if len(ms) >= 2:
            sub = [m for m in ms if m[0] != mh]
            sub_at = yy
            break
    if sub is None:
        print(f"    扫描全程菜单窗口数序列 {seen[:40]}")
    mark(sub is not None, f"悬停可展开子菜单（y={sub_at}）")
    if sub:
        sh, sx, sy, sw, shh = sub[0][0], sub[0][1], sub[0][2], sub[0][3], sub[0][4]
        # 子菜单里悬停某一行要有高亮：抓子菜单两帧对比
        b1 = grab(sh)
        move(sh, sw // 2, shh // 2)
        b2 = grab(sh)
        d = sum(1 for i in range(0, len(b1[0]) - 4, 4)
                if abs(b1[0][i] - b2[0][i]) > 12)
        print(f"    子菜单悬停后变化像素样本 {d}")
        mark(d > 50, "子菜单里鼠标悬停有高亮反馈")
        # 移回父菜单：子菜单必须消失，且父菜单还能继续响应悬停
        move(sh, -20, 20)
        move(mh, mw // 2, 10)
        time.sleep(0.3)
        left = [m for m in windows_of("ZenFileMenu") if m[0] != mh]
        mark(len(left) == 0, f"鼠标移回父菜单后子菜单自动收起（残留 {len(left)} 个）")
        # 换到明显不同的另一行再比较（同一行内移动本来就不会有变化）
        a1 = grab(mh)
        move(mh, mw // 2, 8)
        a1 = grab(mh)
        move(mh, mw // 2, 8 + 3 * 30)
        a2 = grab(mh)
        d2 = sum(1 for i in range(0, len(a1[0]) - 4, 4) if abs(a1[0][i] - a2[0][i]) > 12)
        print(f"    父菜单继续响应悬停，变化样本 {d2}")
        mark(d2 > 20, "交还鼠标后父菜单仍然响应悬停（capture 没丢）")
    # 关键回归点：在外面点一下，整条菜单链必须全关
    click(mh, -300, -300)
    time.sleep(0.5)
    rest = windows_of("ZenFileMenu")
    mark(len(rest) == 0, f"点空白处整条菜单链关闭（残留 {len(rest)} 个窗口）")
drop_cfg()

# ================================================== 4) 状态栏快速切换
print("\n===== 4) 状态栏右下角「大图标 / 详细信息」快速切换 =====")
hwnd = launch(os.path.join(BASE, "Icons"))
raw0, stride0, w0, h0 = grab(hwnd)
sb_top = h0 - 30
# 一个按钮的图形中间可能有 2px 的缝（2×2 方块），所以相邻墨迹的间隔 <= 8px 要并成一组
clusters = []
run = None
last_ink = None
for x in range(w0 - 220, w0):
    ink = sum(1 for y in range(sb_top, h0) if sum(px(raw0, stride0, x, y)) > 200)
    if ink > 1:
        if run is None:
            run = x
        last_ink = x
    else:
        if run is not None and (x - last_ink) > 8:
            clusters.append((run, last_ink + 1))
            run = None
if run is not None:
    clusters.append((run, last_ink + 1))
print(f"  状态栏右侧墨迹簇: {clusters}")
mark(len(clusters) >= 2, f"状态栏右下角画出了两个切换按钮（{len(clusters)} 个墨迹簇）")
if len(clusters) >= 2:
    left_c, right_c = clusters[0], clusters[1]
    lx = (left_c[0] + left_c[1]) // 2
    rx = (right_c[0] + right_c[1]) // 2
    cy = (sb_top + h0) // 2
    # 次序跟随资源管理器：**最右边**那颗是大图标，它左边一颗是详细信息
    # （第 11 轮按用户要求互换过，脚本的预期必须跟着换，不然是把对的判成错的）。
    click(hwnd, rx, cy)
    time.sleep(0.6)
    raw1, stride1, _, _ = grab(hwnd)
    d = sum(1 for i in range(0, len(raw0) - 4, 4) if abs(raw0[i] - raw1[i]) > 20)
    print(f"    点最右按钮后整窗变化样本 {d}")
    mark(d > 300, "点最右按钮切到大图标视图（布局明显变化）")
    click(hwnd, lx, cy)
    time.sleep(0.6)
    raw2, stride2, _, _ = grab(hwnd)
    d2 = sum(1 for i in range(0, len(raw0) - 4, 4) if abs(raw0[i] - raw2[i]) > 20)
    print(f"    点左按钮后与初始详情视图的差异样本 {d2}")
    mark(d2 < d / 3, "点左按钮回到详细信息视图（与初始画面基本一致）")

# ================================================== 5) 返回后保留选中
print("\n===== 5) 返回上一级后保留进入文件夹的选中 =====")
hwnd = launch(os.path.join(BASE, "Nav"))
raw, stride, w, h = grab(hwnd)
bands = row_bands(raw, stride, w, h, 170, 600, 90, h - 40)
print(f"  Nav 里有 {len(bands)} 行")
if bands:
    y = (bands[0][0] + bands[0][1]) // 2
    u.PostMessageW(hwnd, WM_LBUTTONDBLCLK, 0, (y << 16) | (300 & 0xFFFF))
    time.sleep(1.2)
    print(f"    双击进入: {addr_text(hwnd)!r}")
    key(hwnd, 0x08)  # Backspace = 上一级
    time.sleep(1.0)
    print(f"    Backspace 之后: {addr_text(hwnd)!r}")
    raw, stride, w, h = grab(hwnd)
    sel_rows = []
    for (y0, y1) in row_bands(raw, stride, w, h, 170, 600, 90, h - 40):
        cnt = 0
        tot = 0
        for yy in range(y0, y1):
            for xx in range(170, 600, 3):
                tot += 1
                if px(raw, stride, xx, yy) == SEL_BG:
                    cnt += 1
        if tot and cnt / tot > 0.5:
            sel_rows.append((y0, y1))
    print(f"    高亮（选中）行 {len(sel_rows)} 条")
    mark(len(sel_rows) == 1, "返回上一级后，刚进入的那个文件夹保持选中（且只有一行选中）")

# ================================================== 6) 标签
print("\n===== 6) 标签：点击切换 / 悬停关闭按钮 / 中键关闭 =====")


def active_tab_rect(rw, st, ww, hh):
    """当前激活标签的底色 tab_active=#2B2B2B，标题栏底色是 #000000，
    靠它把"当前标签"这一段的左右边界量出来 —— 不猜 DPI。"""
    yy = 18
    xs = [x for x in range(0, ww) if px(rw, st, x, yy) == (0x2B, 0x2B, 0x2B)]
    if not xs:
        return None
    return (min(xs), max(xs))


def strip_ink_right(rw, st, ww, hh):
    """标题栏里（避开右侧窗口按钮）最右边的墨迹 x —— 也就是 + 按钮的位置。"""
    for x in range(ww - 260, 40, -1):
        if sum(1 for y in range(6, 30) if sum(px(rw, st, x, y)) > 180) > 0:
            return x
    return None


hwnd = launch(os.path.join(BASE, "Icons"))
raw, stride, w, h = grab(hwnd)
tab0 = active_tab_rect(raw, stride, w, h)
print(f"  激活标签横向范围 {tab0}")
if tab0:
    tabs_left, tab_w = tab0[0], tab0[1] - tab0[0]
    plus_x = tabs_left + tab_w + 17
    print(f"  推定: tabs_left={tabs_left} tab_w={tab_w} + 按钮 x≈{plus_x}")
    click(hwnd, plus_x, 16)
    time.sleep(0.9)
raw1, stride1, w, h = grab(hwnd)
d = sum(1 for i in range(0, len(raw) - 4, 4) if abs(raw[i] - raw1[i]) > 20)
print(f"  新建标签后画面变化样本 {d}")
if d < 200 and tab0:
    # 兜底：用 Ctrl+T 建标签（点 + 的定位可能偏了）
    u.keybd_event(0x11, 0, 0, 0)
    u.PostMessageW(hwnd, WM_KEYDOWN, 0x54, 0)
    time.sleep(0.4)
    u.keybd_event(0x11, 0, 2, 0)
    time.sleep(0.8)
    raw1, stride1, w, h = grab(hwnd)
    d = sum(1 for i in range(0, len(raw) - 4, 4) if abs(raw[i] - raw1[i]) > 20)
    print(f"    Ctrl+T 兜底后变化样本 {d}")
mark(d > 200, "新建标签并切到新标签（主页）")

tab_now = active_tab_rect(raw1, stride1, w, h)
print(f"  新标签是激活态: {tab_now}")
if tab_now and tab0:
    # 点回标签 0：它就在 tab_now 左边一个标签宽处
    click(hwnd, tab_now[0] - tab_w // 2, 16)
    time.sleep(0.9)
    raw2, stride2, w, h = grab(hwnd)
    d2 = sum(1 for i in range(0, len(raw1) - 4, 4) if abs(raw1[i] - raw2[i]) > 20)
    print(f"  点标签 0 后画面变化样本 {d2}")
    mark(d2 > 200, "点击标签可以切换（点回原来的文件夹标签）")
    # 现在激活的是标签 0，标签 1 是非激活态 —— 悬停它才能看出"关闭按钮是否出现"
    tab1_left = tab_now[0]
    rr = (tab1_left, tab1_left + tab_w)
    t1_before = grab(hwnd)
    move(hwnd, rr[0] + tab_w // 2, 16)
    t1_after = grab(hwnd)
    # 只看标签 1 右侧那一小块（关闭按钮该出现的地方）。
    # 注意：第十三轮改成**每个标签都常画 ×**（原来只有当前/悬停那个才画，
    # 用户反馈就是"标签关不掉"）。所以判据不能再是"悬停后才冒出来"，
    # 而是两条：① 不悬停也看得见 ×；② 悬停时 × 底下要有 hover 底色反馈。
    def close_ink(rw, st, pred):
        return sum(1 for y in range(6, 30) for x in range(rr[1] - 26, rr[1] - 4)
                   if pred(px(rw, st, x, y)))

    glyph = lambda c: sum(c) > 200
    hover_fill = lambda c: all(abs(a - b) <= 12 for a, b in zip(c, HOVER_BG))
    g_before, g_after = close_ink(t1_before[0], t1_before[1], glyph), \
        close_ink(t1_after[0], t1_after[1], glyph)
    f_before, f_after = close_ink(t1_before[0], t1_before[1], hover_fill), \
        close_ink(t1_after[0], t1_after[1], hover_fill)
    print(f"    标签1 × 墨迹: 悬停前 {g_before} -> 悬停后 {g_after}；"
          f"hover 底色 {f_before} -> {f_after}")
    mark(g_before >= 8, "非激活标签也常画关闭按钮（不必悬停才出现）")
    mark(f_after > f_before + 20, "悬停时关闭按钮有底色反馈")
    # 中键关闭标签 1
    p_before = strip_ink_right(t1_after[0], t1_after[1], w, h)
    u.PostMessageW(hwnd, WM_MBUTTONDOWN, 0, (16 << 16) | ((rr[0] + tab_w // 2) & 0xFFFF))
    time.sleep(0.9)
    rw3, st3, w, h = grab(hwnd)
    p_after = strip_ink_right(rw3, st3, w, h)
    print(f"    中键点击前右侧墨迹 x={p_before} -> 后 x={p_after}")
    mark(p_before is not None and p_after is not None and p_after < p_before - 40,
         "鼠标中键关闭标签（标题栏内容左移一个标签宽）")

# ================================================== 7) 自绘属性对话框
print("\n===== 7) 自绘属性对话框 =====")
hwnd = launch(os.path.join(BASE, "Icons"))
raw, stride, w, h = grab(hwnd)
bands = row_bands(raw, stride, w, h, 170, 600, 90, h - 40)
# 表头文字本身也会形成一条 2~3px 的墨迹带，必须滤掉，否则 bands[0] 是表头、
# bands[1] 其实是第 0 行（app.exe），"选中 note.txt"就成了选中 app.exe。
bands = [b for b in bands if b[1] - b[0] >= 8]
# 选中第 2 行（note.txt）后右键：这样菜单是"文件菜单"，行序固定
sel_y = (bands[1][0] + bands[1][1]) // 2 if len(bands) > 1 else 150
click(hwnd, 300, sel_y)
time.sleep(0.4)
u.PostMessageW(hwnd, WM_RBUTTONUP, 0, (sel_y << 16) | (300 & 0xFFFF))
time.sleep(0.6)
menus = windows_of("ZenFileMenu")
ok_menu = len(menus) == 1
if ok_menu:
    mh, mx, my, mw, mhh = menus[0]
    # 行高不猜：悬停第 0 行，量出 hover_bg 色带的上下边界就是行高和内容顶边
    move(mh, mw // 2, 10)
    rm, stm, wm, hm = grab(mh)
    band = [y for y in range(0, hm)
            if all(abs(a - b) <= 8 for a, b in zip(px(rm, stm, mw // 2, y), HOVER_BG))]
    top0, row_h = (band[0], band[-1] - band[0] + 1) if band else (4, 28)
    s = row_h / 28.0
    print(f"  量得: 内容顶边 {top0} 行高 {row_h}px (scale≈{s:.2f})")
    # 文件菜单顺序: 0打开 1打开方式 2剪切 3复制 4重命名 5删除 6[分隔] 7粘贴 8属性
    # （第 15 轮「批量重命名」移到目录栏按钮；第 16 轮 新建/全选/刷新/查看/排序方式
    #   只在空白处右键才出现，于是「属性」从第 14 行前移到了第 8 行）
    y13 = int(top0 + 6 * row_h + 6 * s + row_h + row_h / 2)
    print(f"  推定「属性」行中心 y={y13}")
    target = None
    for mul in (0, -1, 1, -2, 2):
        yy = y13 + int(mul * row_h)
        if yy < 2 or yy > hm - 2:
            continue
        move(mh, mw // 2, yy)
        # 计时必须紧贴在投递点击的那一刻（click() 里自带的 sleep 会污染数据）
        t0 = time.time()
        u.PostMessageW(mh, WM_LBUTTONDOWN, 0, (yy << 16) | ((mw // 2) & 0xFFFF))
        u.PostMessageW(mh, WM_LBUTTONUP, 0, (yy << 16) | ((mw // 2) & 0xFFFF))
        for _ in range(200):
            time.sleep(0.005)
            if windows_of("ZenFileProps"):
                break
        if windows_of("ZenFileProps"):
            target = (yy, time.time() - t0)
            break
    if target:
        print(f"  点中「属性」（菜单内 y={target[0]}），窗口出现耗时 {target[1]*1000:.0f} ms")
        props = windows_of("ZenFileProps")[0]
        print(f"  属性窗口 {props[3]}x{props[4]}")
        mark(True, "右键「属性」打开的是自绘窗口（ZenFileProps），不是原生 #32770")
        mark(target[1] < 1.0, f"打开耗时 {target[1]*1000:.0f}ms < 1000ms（原生属性页常 1~3s）")
        mark(props[3] > 300 and props[4] > 300, "窗口尺寸正常，不是空壳")
        # 点一下「只读」复选框，磁盘上的属性必须真的变
        p = os.path.join(BASE, "Icons", "note.txt")
        a0 = k.GetFileAttributesW(p)
        ph = props[0]
        # 窗口刚 ShowWindow 出来时可能还没画第一帧，PrintWindow 抓到的是空板 ——
        # 于是"复选框一个都扫不到"。重抓几次等到内容出现为止。
        for attempt in range(8):
            rp, stp, wp, hp = grab(ph)
            # 复选框在窗口下部：扫描带边框的小方框（16px 见方的描边框）
            boxes = []
            for y in range(int(hp * 0.6), int(hp * 0.85)):
                bright = lambda yy: [x for x in range(10, 40)
                                     if sum(px(rp, stp, x, yy)) > 200]
                row = bright(y)
                low = bright(y + 15)
                mid = bright(y + 7)
                # 复选框是 16px 见方的**描边框**，四个判据缺一不可：
                #   1. 上边（y）是一条 ≥8px 的横线；
                #   2. 下边（y+15）也是一条 ≥8px 的横线；
                #   3. 中间（y+7）只剩左右两根竖边 —— 这是"框"和"文字"的分水岭。
                #      只判 1、2 会误命中"属性"分组标题的文字行（它上边够亮，
                #      y+15 又正好压到下一个复选框的上边），而那个位置不在任何
                #      复选框的命中矩形里，点下去毫无反应。
                #   4. 三行的左右端点对齐，否则可能是两段不相干的笔画。
                if len(row) >= 8 and len(low) >= 8 and len(mid) >= 2 \
                        and mid[0] == row[0] and mid[-1] == row[-1] \
                        and low[0] == row[0]:
                    boxes.append((y, row[0]))
            if boxes:
                break
            time.sleep(0.35)
        print(f"    扫到的复选框候选 {boxes[:5]}（窗口 {wp}x{hp}，重抓 {attempt + 1} 次）")
        if boxes:
            by, bx = boxes[0]
            # 点在方框正中心（而不是上边线上），避开 1px 描边的舍入误差
            click(ph, bx + 8, by + 8)
            time.sleep(0.5)
            a1 = k.GetFileAttributesW(p)
            print(f"    点复选框: 属性 {a0:#x} -> {a1:#x}")
            mark(a1 != a0, "点「只读」复选框真的改写了磁盘属性")
            click(ph, bx + 8, by + 8)
            time.sleep(0.4)
        u.PostMessageW(ph, WM_CLOSE, 0, 0)
        time.sleep(0.5)
        mark(len(windows_of("ZenFileProps")) == 0, "属性窗口可以关闭")
    else:
        mark(False, "没能在右键菜单里点中「属性」")
else:
    mark(False, "右键菜单没弹出")

kill_all()
print("\n" + "=" * 70)
print("总判定:", "全部 PASS" if ok_all else "有 FAIL，见上")
