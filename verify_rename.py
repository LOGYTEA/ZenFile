"""验证第 11 轮第 12 项：单个重命名 + 右键「批量重命名 ▸」下拉 + 批量改名落盘。

判定全用像素 + 磁盘事实，不靠肉眼：

  1) F2 -> 出现内联编辑框 -> 打新名 -> 回车 -> **磁盘上真的改名了**
  2) 文件右键菜单第 5 行是「批量重命名」，悬停能展开出 5 个模式
  3) 点「查找并替换」-> 弹出 `ZenFileRename`，不是空板，预览区有行
  4) 往两个输入框写字 -> 点「重命名」-> **三个文件一起改了名**
  5) 制造重名冲突 -> 「重命名」按钮不可用，点了磁盘不变（不能改一半）
  6) 「取消」不改任何东西

踩过的坑（写在这儿免得再犯）：
  * ink / diff / bands 四个参数一律是 (x0, x1, y0, y1)。
  * 每次重新 launch 都会销毁上一次的菜单窗口，必须**现开现点**。
  * Ctrl+A 走的是 `GetKeyState(VK_CONTROL)`，光 PostMessage WM_KEYDOWN 不行，
    得先用 keybd_event 把 Ctrl 真的按下去。
  * 文件菜单顺序（有选中项）：0打开 1打开方式 2剪切 3复制 4重命名 5批量重命名
    6删除 7[分隔] 8粘贴 9新建 10[分隔] 11全选 12刷新 13查看 14排序方式 15属性。

素材只在 %TEMP%。
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

u = ctypes.WinDLL("user32", use_last_error=True)
g = ctypes.WinDLL("gdi32", use_last_error=True)
k = ctypes.WinDLL("kernel32", use_last_error=True)
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
u.SendMessageW.argtypes = [wt.HWND, ctypes.c_uint, ctypes.c_ulong, ctypes.c_long]
u.IsWindowVisible.restype = ctypes.c_bool
u.GetClassNameW.restype = ctypes.c_int
u.GetClassNameW.argtypes = [wt.HWND, ctypes.c_wchar_p, ctypes.c_int]
u.GetWindowThreadProcessId.restype = ctypes.c_uint32
u.GetWindowThreadProcessId.argtypes = [wt.HWND, ctypes.POINTER(ctypes.c_uint32)]
u.GetDlgCtrlID.restype = ctypes.c_int
u.GetDlgCtrlID.argtypes = [wt.HWND]
u.GetGUIThreadInfo.restype = ctypes.c_bool
u.GetGUIThreadInfo.argtypes = [ctypes.c_uint32, ctypes.c_void_p]
u.SetWindowTextW.restype = ctypes.c_bool
u.SetWindowTextW.argtypes = [wt.HWND, ctypes.c_wchar_p]

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
WM_RBUTTONUP, WM_MOUSEMOVE, WM_KEYDOWN = 0x0205, 0x0200, 0x0100
WM_SETTEXT = 0x000C

BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "NexRename")
shutil.rmtree(BASE, ignore_errors=True)
ONE = os.path.join(BASE, "one")
BULK = os.path.join(BASE, "bulk")
CONF = os.path.join(BASE, "conflict")
os.makedirs(ONE, exist_ok=True)
os.makedirs(BULK, exist_ok=True)
os.makedirs(CONF, exist_ok=True)
open(os.path.join(ONE, "aaa.txt"), "w").write("a")
for n in ("old1.txt", "old2.txt", "old3.txt"):
    open(os.path.join(BULK, n), "w").write("x")
for n in ("one.txt", "two.txt"):
    open(os.path.join(CONF, n), "w").write("x")

ok_all = True


def mark(ok, msg):
    global ok_all
    ok_all = ok_all and ok
    print(f"    {'PASS' if ok else 'FAIL'}  {msg}")


def type_into(hwnd, text):
    """往 Edit 里"打字"。

    不能用 SetWindowTextW：跨进程 WM_SETTEXT 只更新窗口标题那份文本，同进程
    再读（应用自己 GetWindowTextW）拿到的还是旧内容 —— 表现就是"写了但没生效"。
    真实用户是敲键盘的，所以这里也敲：全选 + 逐字符 WM_CHAR。
    """
    u.SendMessageW(hwnd, 0x00B1, 0, -1)  # EM_SETSEL 全选
    time.sleep(0.15)
    for ch in text:
        u.PostMessageW(hwnd, 0x0102, ord(ch), 0)  # WM_CHAR
        time.sleep(0.07)
    time.sleep(0.4)


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


def px(raw, stride, x, y):
    o = y * stride + x * 4
    return (raw[o + 2], raw[o + 1], raw[o])


def click(hwnd, x, y):
    u.PostMessageW(hwnd, WM_LBUTTONDOWN, 0, (y << 16) | (x & 0xFFFF))
    u.PostMessageW(hwnd, WM_LBUTTONUP, 0, (y << 16) | (x & 0xFFFF))
    time.sleep(0.45)


def move(hwnd, x, y):
    u.PostMessageW(hwnd, WM_MOUSEMOVE, 0, (y << 16) | (x & 0xFFFF))
    time.sleep(0.35)


def key(hwnd, vk, extra=0):
    u.PostMessageW(hwnd, WM_KEYDOWN, vk, extra)
    time.sleep(0.3)


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
CENUMCB = ctypes.WINFUNCTYPE(ctypes.c_bool, wt.HWND, wt.LPARAM)
u.EnumChildWindows.restype = ctypes.c_bool
u.EnumChildWindows.argtypes = [wt.HWND, CENUMCB, wt.LPARAM]


def windows_of(cls):
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


def child_edits(hwnd):
    """列出对话框里所有 EDIT 子控件：{控件ID: HWND}"""
    out = {}

    @CENUMCB
    def cb(h, _l):
        buf = ctypes.create_unicode_buffer(64)
        u.GetClassNameW(h, buf, 64)
        # 系统 Edit 控件的类名是 "Edit"（不是全大写 "EDIT"）
        if buf.value.lower() == "edit":
            out[u.GetDlgCtrlID(h)] = h
        return True

    u.EnumChildWindows(hwnd, cb, 0)
    return out


def focus_of(hwnd):
    """拿到这个窗口所在线程当前有焦点的控件（改名用的内联 EDIT 就用它定位）。"""
    tid = u.GetWindowThreadProcessId(hwnd, None)
    buf = ctypes.create_string_buffer(72)
    struct.pack_into("<II", buf, 0, 72, 0)
    if u.GetGUIThreadInfo(tid, buf):
        return wt.HWND(struct.unpack_from("<q", buf, 8 + 8)[0])
    return None


def menu_geom(mh):
    r = wt.RECT()
    u.GetWindowRect(mh, ctypes.byref(r))
    rm, stm, wm, hm = grab(mh)
    band = [y for y in range(0, hm)
            if all(abs(a - b) <= 8 for a, b in zip(px(rm, stm, wm // 2, y), (0x3D, 0x3D, 0x3D)))]
    top0 = band[0] if band else 4
    row_h = (band[-1] - band[0] + 1) if band else 28
    return (r.right - r.left, r.bottom - r.top, top0, row_h)


def fresh_menu(dir_path, select_all=True):
    """现开现点：进目录 -> 选中 -> 右键，返回主窗口与菜单句柄。"""
    hw = launch(dir_path)
    if hw is None:
        return None, None
    raw, stride, w, h = grab(hw)
    bs = [b for b in bands(raw, stride, 170, 600, 90, h - 40) if b[1] - b[0] >= 8]
    if not bs:
        return hw, None
    sy = (bs[0][0] + bs[0][1]) // 2
    click(hw, 300, sy)
    if select_all:
        u.keybd_event(0x11, 0, 0, 0)  # Ctrl 真的按下：代码里读的是 GetKeyState
        time.sleep(0.1)
        u.PostMessageW(hw, WM_KEYDOWN, 0x41, 0)
        time.sleep(0.2)
        u.keybd_event(0x11, 0, 2, 0)
        time.sleep(0.3)
    sy = (bs[0][0] + bs[0][1]) // 2
    u.PostMessageW(hw, WM_RBUTTONUP, 0, (sy << 16) | (300 & 0xFFFF))
    time.sleep(0.9)
    ms = windows_of("ZenFileMenu")
    if len(ms) != 1:
        return hw, None
    return hw, ms[0][0]


print("=" * 70)
print("重命名：单个 F2 + 批量重命名下拉")

# ---------------------------------------------------------------- 1. F2 单个改名
print("\n[1] F2 单个重命名")
hw = launch(ONE)
raw, stride, w, h = grab(hw)
bs = [b for b in bands(raw, stride, 170, 600, 90, h - 40) if b[1] - b[0] >= 8]
sy = (bs[0][0] + bs[0][1]) // 2
click(hw, 300, sy)
key(hw, 0x71)  # VK_F2
time.sleep(0.5)
fh = focus_of(hw)
buf = ctypes.create_unicode_buffer(64)
u.GetClassNameW(fh, buf, 64)
print(f"  F2 后焦点控件: {buf.value}")
mark(buf.value.lower() == "edit", "F2 弹出了内联编辑框")
if buf.value.lower() == "edit":
    # 用 SetWindowTextW 而不是 SendMessageW(WM_SETTEXT)：后者要把指针塞进
    # c_long，在 64 位 Python 上会被截断成 32 位，写进去的是野指针。
    type_into(fh, "bbb.txt")
    u.PostMessageW(fh, WM_KEYDOWN, 0x0D, 0)
    time.sleep(1.5)
gone = not os.path.exists(os.path.join(ONE, "aaa.txt"))
there = os.path.exists(os.path.join(ONE, "bbb.txt"))
print(f"  aaa.txt 还在? {not gone}   bbb.txt 出现? {there}")
mark(gone and there, "回车后磁盘上真的改了名（aaa.txt -> bbb.txt）")

# ---------------------------------------------------------------- 2. 下拉菜单
print("\n[2] 右键菜单里的「批量重命名 ▸」下拉")
hw, mh = fresh_menu(BULK)
mark(mh is not None, "右键菜单弹出")
if mh is None:
    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    raise SystemExit(1)
mw, mhh, top0, row_h = menu_geom(mh)
print(f"  菜单 {mw}x{mhh}，内容顶 {top0}，行高 {row_h}")
y5 = int(top0 + 5 * row_h + row_h / 2)
move(mh, mw // 2, y5)
subs = windows_of("ZenFileMenu")
print(f"  悬停第 5 行后菜单窗口 {len(subs)} 个")
mark(len(subs) >= 2, "悬停「批量重命名」展开了下级菜单")
child = None
if len(subs) >= 2:
    # 子菜单更矮，且是后创建的那个
    child = min(subs, key=lambda m: m[4])[0]
    cw, chh, ctop, crow = menu_geom(child)
    print(f"  子菜单 {cw}x{chh}，内容顶 {ctop}，行高 {crow}")
    crows = bands(*(grab(child)[0], grab(child)[1]), 4, cw - 4, 0, chh)
    print(f"  子菜单行带 {len(crows)}: {crows}")
    mark(len(crows) == 5, f"下级菜单有 5 种模式（量到 {len(crows)} 行）")

# ---------------------------------------------------------------- 3. 弹窗
print("\n[3] 点「查找并替换」弹出批量重命名窗口")
rn = None
if child:
    cw, chh, ctop, crow = menu_geom(child)
    click(child, cw // 2, int(ctop + crow / 2))
    for _ in range(40):
        time.sleep(0.1)
        ws = windows_of("ZenFileRename")
        if ws:
            rn = ws[0][0]
            break
mark(rn is not None, "弹出 ZenFileRename 窗口")
if rn is None:
    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    raise SystemExit(1)
for _ in range(8):
    raw, stride, W, H = grab(rn)
    if ink(raw, stride, 10, W - 10, 40, H - 40) > 300:
        break
    time.sleep(0.35)
raw, stride, W, H = grab(rn)
scale = W / 580.0
print(f"  弹窗 {W}x{H}（scale≈{scale:.2f}）")


def S(v):
    return int(round(v * scale))


PREV = (S(14), W - S(14), S(272), H - S(58))
COMBO = (S(94), W - S(14), S(66), S(92))
prev_ink = ink(raw, stride, *PREV)
combo_ink = ink(raw, stride, *COMBO)
print(f"  预览区墨迹 {prev_ink}，模式下拉墨迹 {combo_ink}")
mark(prev_ink > 200, f"预览区列出了待改名的项目（{prev_ink} px）")
mark(combo_ink > 100, f"模式下拉画出来了（{combo_ink} px）")
edits = child_edits(rn)
print(f"  输入框控件: {sorted(edits)}")
mark(1 in edits and 2 in edits, "两个参数输入框都在")

# ---------------------------------------------------------------- 4. 真的改名
print("\n[4] 查找 old -> 替换为 new，点「重命名」")
type_into(edits[1], "old")
type_into(edits[2], "new")
raw2, stride2, _, _ = grab(rn)
dprev = diff(raw, raw2, stride, *PREV)
print(f"  填完参数后预览区变化 {dprev} px")
mark(dprev > 100, f"预览跟着参数实时更新（{dprev} px）")
OK = (W - S(14) - S(112) - S(8) - S(112), W - S(14) - S(112) - S(8), H - S(46), H - S(46) + S(28))
CN = (W - S(14) - S(112), W - S(14), H - S(46), H - S(46) + S(28))
ok_col = px(raw2, stride2, (OK[0] + OK[1]) // 2 + 20, (OK[2] + OK[3]) // 2)
cn_col = px(raw2, stride2, (CN[0] + CN[1]) // 2 + 20, (CN[2] + CN[3]) // 2)
print(f"  可用时 确定={ok_col} 取消={cn_col}")
mark(ok_col != cn_col, "有改动且无冲突时「重命名」是强调色（可点）")
click(rn, (OK[0] + OK[1]) // 2, (OK[2] + OK[3]) // 2)
time.sleep(2.0)
got = sorted(os.listdir(BULK))
print(f"  磁盘上现在: {got}")
mark(got == ["new1.txt", "new2.txt", "new3.txt"], f"三个文件一起改了名（{got}）")

# ---------------------------------------------------------------- 5. 冲突
print("\n[5] 制造重名冲突 -> 不允许执行")
hw, mh = fresh_menu(CONF)
mw, mhh, top0, row_h = menu_geom(mh)
move(mh, mw // 2, int(top0 + 5 * row_h + row_h / 2))
time.sleep(0.6)
subs = windows_of("ZenFileMenu")
child = min(subs, key=lambda m: m[4])[0] if len(subs) >= 2 else None
mark(child is not None, "第二次也展开了下级菜单")
if child:
    cw, chh, ctop, crow = menu_geom(child)
    click(child, cw // 2, int(ctop + crow / 2))
    rn = None
    for _ in range(40):
        time.sleep(0.1)
        ws = windows_of("ZenFileRename")
        if ws:
            rn = ws[0][0]
            break
    mark(rn is not None, "弹出批量重命名窗口")
    if rn:
        for _ in range(8):
            rawc, stridec, Wc, Hc = grab(rn)
            if ink(rawc, stridec, 10, Wc - 10, 40, Hc - 40) > 300:
                break
            time.sleep(0.35)
        ec = child_edits(rn)

        def SC(v):
            return int(round(v * (Wc / 580.0)))

        type_into(ec[1], "one")
        type_into(ec[2], "two")
        OKC = (Wc - SC(14) - SC(112) - SC(8) - SC(112), Wc - SC(14) - SC(112) - SC(8),
               Hc - SC(46), Hc - SC(46) + SC(28))
        CNC2 = (Wc - SC(14) - SC(112), Wc - SC(14), Hc - SC(46), Hc - SC(46) + SC(28))
        rawc2, stridec2, _, _ = grab(rn)
        ok2 = px(rawc2, stridec2, (OKC[0] + OKC[1]) // 2 + 20, (OKC[2] + OKC[3]) // 2)
        cn2 = px(rawc2, stridec2, (CNC2[0] + CNC2[1]) // 2 + 20, (CNC2[2] + CNC2[3]) // 2)
        print(f"  冲突时 确定={ok2} 取消={cn2}")
        mark(ok2 == cn2, "重名冲突时「重命名」变灰，与「取消」同色（点了也不执行）")
        click(rn, (OKC[0] + OKC[1]) // 2, (OKC[2] + OKC[3]) // 2)
        time.sleep(1.5)
        still = sorted(os.listdir(CONF))
        print(f"  冲突时点「重命名」后磁盘: {still}")
        mark(still == ["one.txt", "two.txt"], f"重名被拦下，一个都没改（{still}）")
        # 取消：关窗且不改
        CNC = (Wc - SC(14) - SC(112), Wc - SC(14), Hc - SC(46), Hc - SC(46) + SC(28))
        click(rn, (CNC[0] + CNC[1]) // 2, (CNC[2] + CNC[3]) // 2)
        time.sleep(0.8)
        mark(not windows_of("ZenFileRename"), "点「取消」关掉了窗口")

subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
shutil.rmtree(BASE, ignore_errors=True)
print("\n" + "=" * 70)
print("重命名全部通过" if ok_all else "重命名存在失败项")
