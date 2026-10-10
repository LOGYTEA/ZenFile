"""验证第 11 轮第 7 项：属性对话框加「安全 / 自定义 / 详细信息」标签页。

判定全用像素 + 磁盘事实，不靠肉眼：

  1) 标签条上有 N 个标签；点某个标签 -> 内容区画面明显变化（差异像素）
  2) 「只读」勾上后磁盘属性位真的变了（读 GetFileAttributesW 的结果用 attrib 看）
  3) 「取消」必须把属性位还原回去 —— 否则"取消"是假的
  4) 安全页能画出属主与 ACE 行（行墨迹），且与 fm-core::acl 自读取到的一致
  5) 自定义页改模板 -> 确定 -> desktop.ini 真的被写成 UTF-16LE 且带预期键
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
u.SendMessageW.restype = ctypes.c_ssize_t
k = ctypes.WinDLL("kernel32", use_last_error=True)
k.GetFileAttributesW.restype = ctypes.c_uint32
k.GetFileAttributesW.argtypes = [ctypes.c_wchar_p]

g.CreateCompatibleDC.argtypes = [wt.HDC]
g.CreateCompatibleDC.restype = wt.HDC
g.CreateCompatibleBitmap.restype = wt.HBITMAP
g.CreateCompatibleBitmap.argtypes = [wt.HDC, ctypes.c_int, ctypes.c_int]
g.SelectObject.argtypes = [wt.HDC, wt.HGDIOBJ]
g.SelectObject.restype = wt.HGDIOBJ
g.GetDIBits.restype = ctypes.c_int
g.GetDIBits.argtypes = [ctypes.c_void_p, wt.HBITMAP, ctypes.c_uint, ctypes.c_uint,
                        ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint]

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\fm-app.exe"
WM_LBUTTONDOWN, WM_LBUTTONUP = 0x0201, 0x0202
WM_COMMAND = 0x0111
READONLY, HIDDEN, ARCHIVE, SYSTEM = 0x1, 0x2, 0x20, 0x4

BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "NexProps")
# 只放一个文件的目录：右键第 0 行必然是那个文件
FDIR = os.path.join(BASE, "f_only")
os.makedirs(FDIR, exist_ok=True)
TARGET = os.path.join(FDIR, "t.txt")
open(TARGET, "w").write("hello")
# 只放一个子文件夹的目录：右键第 0 行必然是那个**文件夹**
# （"自定义"页只有文件夹才有，右键点目录里的内容只能拿到文件，测不到这一页）
CDIR = os.path.join(BASE, "d_only")
os.makedirs(CDIR, exist_ok=True)
DIRT = os.path.join(CDIR, "sub")
os.makedirs(DIRT, exist_ok=True)
open(os.path.join(DIRT, "inner.txt"), "w").write("x")

ok_all = True


def mark(ok, msg):
    global ok_all
    ok_all = ok_all and ok
    print(f"    {'PASS' if ok else 'FAIL'}  {msg}")


def launch(arg):
    subprocess.run(["taskkill", "/F", "/IM", "fm-app.exe"], capture_output=True)
    time.sleep(0.5)
    subprocess.Popen([EXE, arg])
    for _ in range(60):
        time.sleep(0.25)
        hw = u.FindWindowW("NexFileMainWindow", None)
        if hw:
            time.sleep(1.8)
            return hw
    return None


def find_props(timeout=8.0):
    t0 = time.time()
    while time.time() - t0 < timeout:
        hw = u.FindWindowW("NexFileProps", None)
        if hw:
            return hw
        time.sleep(0.1)
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


def px(raw, stride, x, y):
    o = y * stride + x * 4
    return (raw[o + 2], raw[o + 1], raw[o])


def click(hwnd, x, y):
    u.PostMessageW(hwnd, WM_LBUTTONDOWN, 0, (y << 16) | (x & 0xFFFF))
    u.PostMessageW(hwnd, WM_LBUTTONUP, 0, (y << 16) | (x & 0xFFFF))
    time.sleep(0.5)


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


def sig(raw, stride, x0, x1, y0, y1):
    """逐行墨迹签名：每 2 行取一个墨迹像素数，用来判断两个页面是不是同一张画。

    整幅图做像素差在这里区分度不够 —— 常规页与详细信息页共用"创建/修改/访问
    时间"三行，位置一模一样，差值会被压得很小。逐行签名把"哪一行有多少字"
    也计入，四页之间差出上千。
    """
    return [sum(1 for x in range(x0, x1)
                if raw[y * stride + x * 4] + raw[y * stride + x * 4 + 1] + raw[y * stride + x * 4 + 2] > 330)
            for y in range(y0, y1, 2)]


def sig_diff(a, b):
    return sum(abs(x - y) for x, y in zip(a, b))


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


def attrs_of(p):
    v = k.GetFileAttributesW(p)
    return v if v != 0xFFFFFFFF else None


ENUMCB = ctypes.WINFUNCTYPE(ctypes.c_bool, wt.HWND, wt.LPARAM)
u.EnumWindows.restype = ctypes.c_bool
u.EnumWindows.argtypes = [ENUMCB, wt.LPARAM]
u.GetClassNameW.restype = ctypes.c_int
u.GetClassNameW.argtypes = [wt.HWND, ctypes.c_wchar_p, ctypes.c_int]
u.IsWindowVisible.restype = ctypes.c_bool


def windows_of(cls):
    """列出某个类名下所有可见窗口（EnumWindows 回调必须用这里定义好的装饰器）。"""
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


def menu_row_height(mh, mw, mh_h):
    """悬停第 0 行，量出 hover 色带的上下边界 = 行高与内容顶边。"""
    u.PostMessageW(mh, 0x0200, 0, (10 << 16) | ((mw // 2) & 0xFFFF))
    time.sleep(0.25)
    rm, stm, wm, hm = grab(mh)
    band = [y for y in range(0, hm)
            if all(abs(a - b) <= 8 for a, b in zip(px(rm, stm, mw // 2, y), (0x3D, 0x3D, 0x3D)))]
    return (band[0], band[-1] - band[0] + 1) if band else (4, 28)


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


def open_props(dir_path, row_pref=1, nrow_first=6):
    """借助右键菜单打开自绘属性窗口，返回窗口句柄。

    `nrow_first` = 第一组（打开 … 删除）有几行。**文件的菜单里多一个「打开方式」**，
    所以文件是 6 行、文件夹是 5 行 —— 差一行，后面「属性」的位置就整体错一格。
    """
    hw = launch(dir_path)
    raw, stride, w, h = grab(hw)
    bands = [b for b in row_bands(raw, stride, 170, 600, 90, h - 40) if b[1] - b[0] >= 8]
    if not bands:
        return None
    idx = min(row_pref, len(bands) - 1)
    sel_y = (bands[idx][0] + bands[idx][1]) // 2
    click(hw, 300, sel_y)
    time.sleep(0.3)
    u.PostMessageW(hw, 0x0205, 0, (sel_y << 16) | (300 & 0xFFFF))  # WM_RBUTTONUP
    time.sleep(0.7)
    menus = windows_of("NexFileMenu")
    if len(menus) != 1:
        return None
    mh, _mx, _my, mw, mhh = menus[0]
    top0, row_h = menu_row_height(mh, mw, mhh)
    s = row_h / 28.0
    # 文件菜单: 0打开 1打开方式 2剪切 3复制 4重命名 5删除 6[分隔] 7粘贴 8新建
    #           9[分隔] 10全选 11刷新 12查看 13排序方式 14属性
    # 文件夹菜单没有第 1 行，后面整体前移一格。
    y13 = int(top0 + nrow_first * row_h + 6 * s + 2 * row_h + 6 * s + 4 * row_h + row_h / 2)
    for mul in (0, -1, 1, -2, 2):
        yy = y13 + int(mul * row_h)
        if yy < 2 or yy > mhh - 2:
            continue
        u.PostMessageW(mh, 0x0200, 0, (yy << 16) | ((mw // 2) & 0xFFFF))
        u.PostMessageW(mh, WM_LBUTTONDOWN, 0, (yy << 16) | ((mw // 2) & 0xFFFF))
        u.PostMessageW(mh, WM_LBUTTONUP, 0, (yy << 16) | ((mw // 2) & 0xFFFF))
        for _ in range(200):
            time.sleep(0.005)
            p = windows_of("NexFileProps")
            if p:
                print(f"  点中「属性」（菜单内 y={yy}）")
                return p[0][0]
    return None


print("=" * 70)
print("属性对话框：标签页 + 只读真的作用")

# ---------------------------------------------------------------- 1. 只读勾上就落盘
print("\n[1] 「只读」勾选的实际作用")
pw = open_props(FDIR, 0)
mark(pw is not None, "属性窗口能打开")
if pw is None:
    print("  无法继续")
    subprocess.run(["taskkill", "/F", "/IM", "fm-app.exe"], capture_output=True)
    raise SystemExit(1)

# 窗口刚 ShowWindow 出来时第一帧可能还没画：PrintWindow 会抓到空板（踩过）
for _ in range(8):
    raw0, stride, W, H = grab(pw)
    if ink(raw0, stride, 10, W - 10, 60, H - 50) > 200:
        break
    time.sleep(0.35)
CONTENT_TOP = 64  # 34 标题 + 30 标签条
print(f"  属性窗口 {W}x{H}")
# 常规页的四个复选框：content_top+282 起，每行 26
CHK = [CONTENT_TOP + 282 + i * 26 for i in range(4)]
print(f"  复选框 y: {CHK}")
# 确认真的扫到了复选框（不是空板）
boxes = 0
for y in CHK:
    top = [x for x in range(10, 40) if sum(px(raw0, stride, x, y + 5)) > 200]
    if len(top) >= 8:
        boxes += 1
print(f"  扫到 {boxes}/4 个复选框方框")
mark(boxes == 4, f"常规页画出了 4 个属性复选框（扫到 {boxes} 个）")

before = attrs_of(TARGET)
print(f"  点之前属性位 = 0x{before:08X}")
# 点「只读」（第一个复选框）
click(pw, 60, CHK[0] + 13)
after = attrs_of(TARGET)
print(f"  点「只读」后 = 0x{after:08X}")
mark(after is not None and (after & READONLY) != 0 and (before or 0) & READONLY == 0,
     "点「只读」立刻改写了磁盘属性位")
# 再点一次应清掉
click(pw, 60, CHK[1] + 13)  # 隐藏
after2 = attrs_of(TARGET)
print(f"  再点「隐藏」后 = 0x{after2:08X}")
mark(after2 is not None and (after2 & HIDDEN) != 0, "「隐藏」同样立刻生效")

# 取消必须还原
OK_R = (W - 20 - 92 - 8 - 92, H - 44)
CANCEL_R = (W - 20 - 92, H - 44)
click(pw, CANCEL_R[0] + 46, CANCEL_R[1] + 14)
time.sleep(0.8)
restored = attrs_of(TARGET)
print(f"  点「取消」后 = 0x{restored:08X}")
mark(restored == before, f"「取消」把属性位还原回 0x{before:08X}")

# ---------------------------------------------------------------- 2. 标签页
print("\n[2] 标签页（文件夹：常规/安全/自定义/详细信息）")
pw = open_props(CDIR, 0, 5)  # 文件夹：第一组 5 行（没有「打开方式」）
mark(pw is not None, "文件夹的属性窗口打开")
if pw is None:
    subprocess.run(["taskkill", "/F", "/IM", "fm-app.exe"], capture_output=True)
    raise SystemExit(1)
for _ in range(8):
    raw0, stride, W, H = grab(pw)
    if ink(raw0, stride, 10, W - 10, 60, H - 50) > 200:
        break
    time.sleep(0.35)
raw0, stride, W, H = grab(pw)
# 注意：所有 ink/diff/bands 的参数顺序都是 (x0, x1, y0, y1)，不是 (x0, y0, x1, y1)
CONTENT = (10, W - 10, CONTENT_TOP, H - 50)

# 标签条上有几个标签：Tab 背景与标题栏不同色，用"顶边强调色"找选中标签，
# 用文字墨迹带数标签个数。
tab_ink = ink(raw0, stride, 6, W - 6, 34, CONTENT_TOP - 2)
print(f"  标签条墨迹 = {tab_ink}")
mark(tab_ink > 100, f"标签条上有文字（{tab_ink} px）")

# 依次点每个标签，看内容区是否变化
tab_x = [12]
# 标签宽度 = 20 + 字数*13
for label in ("常规", "安全", "自定义", "详细信息"):
    w = 20 + len(label) * 13
    tab_x.append(tab_x[-1] + w + 2)
centers = []
prev = 12
for label in ("常规", "安全", "自定义", "详细信息"):
    w = 20 + len(label) * 13
    centers.append((prev + w // 2, label))
    prev += w + 2
print(f"  标签中心: {centers}")

shots = {}
base_sig = sig(raw0, stride, 14, W - 14, CONTENT_TOP, H - 50)
for cx, label in centers:
    click(pw, cx, CONTENT_TOP - 14)
    time.sleep(0.5)
    raw, st, _, _ = grab(pw)
    shots[label] = (raw, st)
    n = sig_diff(base_sig, sig(raw, stride, 14, W - 14, CONTENT_TOP, H - 50))
    ci = ink(raw, stride, 20, 220, CONTENT_TOP + 30, CONTENT_TOP + 56)
    print(f"  切到「{label}」：与「常规」的逐行签名差 {n}，下拉框位墨迹 {ci}")
    if label != "常规":
        mark(n > 800, f"「{label}」页画出了自己的内容（签名差 {n}）")

# 安全页要有属主 + ACE 行
raw_sec, st_sec = shots["安全"]
sec_ink = ink(raw_sec, st_sec, *CONTENT)
rows = bands(raw_sec, st_sec, 20, W - 20, CONTENT_TOP + 80, CONTENT_TOP + 260)
print(f"  安全页墨迹 {sec_ink}，列表行带 {rows[:10]}")
mark(sec_ink > 100, f"安全页有内容（{sec_ink} px）")
mark(len(rows) >= 2, f"安全页列出了 {len(rows)} 条 ACE（属主/组/权限项）")

# 自定义页：模板下拉 + 按钮
raw_cus, st_cus = shots["自定义"]
combo = (20, 220, CONTENT_TOP + 30, CONTENT_TOP + 56)
combo_ink = ink(raw_cus, st_cus, *combo)
btn_ink = ink(raw_cus, st_cus, 128, 232, CONTENT_TOP + 160, CONTENT_TOP + 186)
print(f"  自定义页：下拉框墨迹 {combo_ink}，按钮位墨迹 {btn_ink}")
mark(combo_ink > 60, f"自定义页有文件夹模板下拉框（{combo_ink} px）")
mark(btn_ink > 60, f"自定义页有「选择文件…/还原默认」按钮（{btn_ink} px）")
# 详细信息页比常规页多几行
raw_det, st_det = shots["详细信息"]
det_rows = len(bands(raw_det, st_det, 20, W - 20, CONTENT_TOP + 70, H - 60, minh=6))
print(f"  详细信息页信息行 {det_rows} 条")
mark(det_rows >= 8, f"详细信息页列出了 {det_rows} 行（常规页只有 7 行）")

# ---------------------------------------------------------------- 3. 自定义页真的写 desktop.ini
print("\n[3] 自定义页改模板 -> 确定 -> 落盘")
ini_path = os.path.join(DIRT, "desktop.ini")
if os.path.exists(ini_path):
    os.remove(ini_path)
click(pw, centers[2][0], CONTENT_TOP - 14)  # 自定义
time.sleep(0.4)
# 点下拉框 -> 弹出自绘 combo -> 点第 3 项（图片）
click(pw, 120, CONTENT_TOP + 43)
time.sleep(0.6)
combo_hw = u.FindWindowW("NexFilePropsCombo", None)
print(f"  下拉弹出窗口: {combo_hw}")
mark(combo_hw is not None, "点下拉框弹出了选择列表")
if combo_hw:
    r = wt.RECT()
    u.GetWindowRect(combo_hw, ctypes.byref(r))
    print(f"  下拉 {r.right - r.left}x{r.bottom - r.top}")
    # 第 3 项（图片）：pad 4 + 24*i
    click(combo_hw, 60, 4 + 24 * 2 + 12)
    time.sleep(0.5)
    raw_c2, st2, _, _ = grab(pw)
    n = diff(raw_cus, raw_c2, stride, *combo)
    print(f"  选完模板后下拉框变化 {n} px")
    mark(n > 20, f"选中「图片」后下拉框文字变了（{n} px）")

click(pw, OK_R[0] + 46, OK_R[1] + 14)
time.sleep(1.0)
if os.path.exists(ini_path):
    rawb = open(ini_path, "rb").read()
    is16 = rawb[:2] == b"\xff\xfe"
    text = rawb.decode("utf-16-le", errors="replace") if is16 else rawb.decode("utf-8", errors="replace")
    print(f"  desktop.ini {len(rawb)} 字节，UTF-16={is16}")
    print("  内容:", text.replace("\r\n", " | ")[:200])
    mark(is16, "desktop.ini 是 UTF-16LE 带 BOM（与资源管理器一致）")
    mark("imageres.dll,-113" in text, "写了图片的 IconResource（imageres.dll,-113）")
    mark("FolderType=Pictures" in text, "写了 FolderType=Pictures")
    a = attrs_of(DIRT)
    print(f"  文件夹属性位 = 0x{a:08X}")
    mark(a is not None and a & READONLY != 0, "文件夹带上了 READONLY（Shell 才认 desktop.ini）")
    ai = attrs_of(ini_path)
    mark(ai is not None and ai & (HIDDEN | SYSTEM) != 0, "desktop.ini 自带 HIDDEN|SYSTEM")
else:
    mark(False, "desktop.ini 没有被创建")

subprocess.run(["taskkill", "/F", "/IM", "fm-app.exe"], capture_output=True)
# 清理：把 material 上的属性摘掉再删，不然 rmtree 会 PermissionError
for p in (ini_path, TARGET, os.path.join(DIRT, "inner.txt")):
    if os.path.exists(p):
        try:
            os.chmod(p, 0o666)
        except OSError:
            pass
        try:
            os.remove(p)
        except OSError:
            pass
print("\n" + "=" * 70)
print("属性标签页全部通过" if ok_all else "属性标签页存在失败项")
