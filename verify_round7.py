"""第七轮验证：双击打开 / 导航窗格图标+字号 / 菜单悬停与子菜单展开。

全部素材落在 %TEMP%，不在桌面留任何东西。
判定一律抓屏取像素，不用"看着像"。
"""
import ctypes
import ctypes.wintypes as wt
import os
import shutil
import struct
import subprocess
import sys
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
ENUMCB = ctypes.WINFUNCTYPE(ctypes.c_bool, wt.HWND, ctypes.c_long)
u.EnumWindows.restype = wt.BOOL
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

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\ZenFile.exe"
WM_GETTEXT = 0x000D
WM_LBUTTONDOWN = 0x0201
WM_LBUTTONUP = 0x0202
WM_LBUTTONDBLCLK = 0x0203
WM_RBUTTONUP = 0x0205
WM_MOUSEMOVE = 0x0200
MENU_ROW = 28

BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "NexVerify7")


def kill_all():
    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    time.sleep(0.6)


def launch(arg=None):
    kill_all()
    subprocess.Popen([EXE] + ([arg] if arg else []))
    hwnd = None
    for _ in range(80):
        time.sleep(0.25)
        hwnd = u.FindWindowW("ZenFileMainWindow", None)
        if hwnd:
            break
    assert hwnd, "窗口没起来"
    time.sleep(2.5)
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


def hx(c):
    return "#%02X%02X%02X" % c


def addr_text(hwnd):
    buf = (ctypes.c_wchar * 512)()
    u.SendMessageW(u.GetDlgItem(hwnd, 101), WM_GETTEXT, 512, ctypes.addressof(buf))
    return buf.value


def dblclick(hwnd, x, y):
    u.PostMessageW(hwnd, WM_LBUTTONDBLCLK, 0, (y << 16) | (x & 0xFFFF))
    time.sleep(1.2)


def content_rows(raw, stride, w, h, y0=100, y1=700):
    """内容区里有多少条"有墨迹"的行段。"""
    out, cur = [], None
    for y in range(y0, min(y1, h - 30)):
        c = 0
        for x in range(160, min(700, w - 5)):
            o = y * stride + x * 4
            if (raw[o + 2] + raw[o + 1] + raw[o]) > 200:
                c += 1
        if c > 3:
            if cur is None:
                cur = y
        else:
            if cur is not None:
                out.append((cur, y))
                cur = None
    if cur is not None:
        out.append((cur, min(y1, h - 30)))
    return out


def menu_windows():
    found = []

    @ENUMCB
    def cb(h, _lp):
        cls = ctypes.create_unicode_buffer(64)
        u.GetClassNameW(h, cls, 64)
        if cls.value == "ZenFileMenu":
            rr = wt.RECT()
            u.GetWindowRect(h, ctypes.byref(rr))
            found.append((int(h), rr.left, rr.top, rr.right - rr.left, rr.bottom - rr.top))
        return True

    u.EnumWindows(cb, 0)
    return found


def setup():
    shutil.rmtree(BASE, ignore_errors=True)
    os.makedirs(os.path.join(BASE, "FolderA"), exist_ok=True)
    os.makedirs(os.path.join(BASE, "FolderB"), exist_ok=True)
    # FolderA 必须**非空**：双击进去后列表要是空的，说明导航路径出了问题
    for n in range(4):
        with open(os.path.join(BASE, "FolderA", f"f{n}.txt"), "w", encoding="utf-8") as f:
            f.write("x")


ok_all = True
print(f"测试目录: {BASE}")

# ================================================== 1) 双击打开
print("\n===== 1) 双击打开 =====")
setup()
hwnd = launch(BASE)
print(f"  起始地址: {addr_text(hwnd)!r}")
dblclick(hwnd, 400, 100 + 8)
a1 = addr_text(hwnd)
print(f"  双击第 0 行后: {a1!r}")
ok1 = a1.endswith("FolderA")
ok1b = not a1.startswith("\\\\?\\")
print(f"  判定: {'PASS 双击进入子文件夹' if ok1 else 'FAIL 双击无反应'}")
print(f"  地址栏可读性: {'PASS 无 \\\\?\\ 前缀' if ok1b else 'FAIL 带 \\\\?\\ 前缀'}")
# 关键：进去之后必须**真的有内容**。canonicalize 带来的 `\\?\` 前缀会让枚举返回空，
# 而地址栏看起来完全正常 —— 只验地址栏会漏掉这个缺陷。
raw, stride, w, h = grab(hwnd)
segs = content_rows(raw, stride, w, h)
print(f"  进入后列表墨迹段 {len(segs)} 条（FolderA 里有 4 个文件）")
ok1c = len(segs) >= 4
print(f"  判定: {'PASS 列表有内容' if ok1c else 'FAIL 列表是空的'}")
ok_all = ok_all and ok1 and ok1b and ok1c

# ================================================== 2) 导航窗格
print("\n===== 2) 导航窗格：图标 + 字号 =====")
raw, stride, w, h = grab(hwnd)
row_h, first_top = 34, 74 + 34
icons, heights = 0, []
for i in range(5):
    top = first_top + i * row_h
    if top + row_h > h - 30:
        break
    ink = sum(1 for y in range(top, top + row_h) for x in range(14, 34)
              if sum(px(raw, stride, x, y)) > 150)
    ys = [y for y in range(top, top + row_h)
          if any(sum(px(raw, stride, x, y)) > 260 for x in range(46, 150))]
    heights.append(ys[-1] - ys[0] + 1 if ys else 0)
    icons += 1 if ink >= 30 else 0
    print(f"    行{i} y={top:3d} 图标墨迹 {ink:3d}px  文字墨迹高 {heights[-1]}px")
ok2 = icons == 5
print(f"  有图标的行 {icons}/5 -> {'PASS' if ok2 else 'FAIL'}")
avg = sum(heights) / max(1, len(heights))
ok2b = avg >= 11
print(f"  文字平均墨迹高 {avg:.1f}px（13px 字体≈10，16px 字体≈15）-> {'PASS 已放大' if ok2b else 'FAIL'}")
ok_all = ok_all and ok2 and ok2b

# ================================================== 3) 菜单悬停 + 子菜单
print("\n===== 3) 菜单悬停高亮 + 悬停展开子菜单 =====")
# 右键落在**空白处**（第 16 轮起，带下拉的 新建/查看/排序方式 只出现在背景菜单；
# 右键某个文件时菜单里没有可展开的项，扫 20 行也扫不出子菜单）。
# 此时已进入 FolderA（4 个文件），第 6 行以下是空的。
u.PostMessageW(hwnd, WM_RBUTTONUP, 0, (230 << 16) | (400 & 0xFFFF))
time.sleep(1.0)
mw = menu_windows()
if not mw:
    print("  FAIL 菜单没弹出")
    ok_all = False
else:
    menu = mw[0][0]
    print(f"  菜单 hwnd={menu} rect=({mw[0][1]},{mw[0][2]}) {mw[0][3]}x{mw[0][4]}")

    def row_bg(mh, idx):
        raw2, st2, w2, h2 = grab(mh)
        y = 4 + idx * MENU_ROW + MENU_ROW // 2
        if y >= h2:
            return None
        return hx(px(raw2, st2, w2 // 2, y))

    base = row_bg(menu, 1)
    u.PostMessageW(menu, WM_MOUSEMOVE, 0, ((4 + MENU_ROW + 14) << 16) | (60 & 0xFFFF))
    time.sleep(0.6)
    hov = row_bg(menu, 1)
    ok3 = base != hov
    print(f"  第 1 行 未悬停={base} 悬停={hov} -> {'PASS 悬停有变化' if ok3 else 'FAIL 无变化'}")
    ok_all = ok_all and ok3

    # 逐行悬停，找能展开子菜单的项
    _, _, _, mh_h = mw[0][3], mw[0][1], mw[0][2], mw[0][4]
    rows = (mh_h - 8) // MENU_ROW
    opened = None
    for i in range(min(rows, 20)):
        u.PostMessageW(menu, WM_MOUSEMOVE, 0, ((4 + i * MENU_ROW + 14) << 16) | (60 & 0xFFFF))
        time.sleep(0.4)
        wl = menu_windows()
        if len(wl) > 1:
            opened = (i, wl)
            break
    if opened:
        i, wl = opened
        print(f"  悬停第 {i} 行展开子菜单，当前 {len(wl)} 个菜单窗口:")
        for (hh, l, t, ww, hh2) in wl:
            print(f"    hwnd={hh} rect=({l},{t}) {ww}x{hh2}")
        parent = min(wl, key=lambda f: f[1])
        child = max(wl, key=lambda f: f[1])
        ok3b = child[1] >= parent[1] + parent[3] - 10 and child[3] > 0 and child[4] > 0
        print(f"  子菜单在父菜单右侧（父右缘 {parent[1]+parent[3]}，子左缘 {child[1]}）"
              f" -> {'PASS' if ok3b else 'FAIL'}")
        # 子菜单里也得能悬停高亮
        cy = child[4] // 2
        u.PostMessageW(child[0], WM_MOUSEMOVE, 0, (cy << 16) | (child[3] // 2 & 0xFFFF))
        time.sleep(0.5)
        c1 = row_bg(child[0], 0)
        u.PostMessageW(child[0], WM_MOUSEMOVE, 0, ((4 + MENU_ROW + 14) << 16) | (child[3] // 2 & 0xFFFF))
        time.sleep(0.5)
        c2 = row_bg(child[0], 1)
        print(f"  子菜单第0行={c1} 第1行(悬停)={c2}")
        ok3c = c1 is not None and c2 is not None and c1 != c2
        print(f"  子菜单悬停高亮 -> {'PASS' if ok3c else 'FAIL'}")
        ok_all = ok_all and ok3b and ok3c
    else:
        print("  FAIL 悬停任何一行都没展开出子菜单")
        ok_all = False

# ================================================== 4) 打开后不该有"默认高亮的第一行"
print("\n===== 4) 打开文件夹后第 0 行不该自带高亮 =====")
for name, arg in (("桌面", os.path.join(os.environ["USERPROFILE"], "Desktop")),
                  ("用户目录", os.environ["USERPROFILE"])):
    hwnd = launch(arg)
    raw, stride, w, h = grab(hwnd)
    r0 = px(raw, stride, w - 30, 106)      # 第 0 行右侧空白
    r1 = px(raw, stride, w - 30, 106 + 21)  # 第 1 行右侧空白
    same = r0 == r1
    print(f"  {name:6s} 第0行 {hx(r0)}  第1行 {hx(r1)}  "
          f"-> {'PASS 无默认高亮' if same else 'FAIL 第0行被默认点亮'}")
    ok_all = ok_all and same

kill_all()
print("\n总判定:", "PASS" if ok_all else "FAIL")
sys.exit(0 if ok_all else 1)
