"""第54轮：右键菜单「悬停展示」= 悬停即展开子菜单 + 行高亮跟随（真光标判据）。

为什么不用 verify_menu_hover.py：那个脚本用 PostMessage 伪造 WM_MOUSEMOVE，
而物理光标停在别处 —— 一旦 capture 变化让系统按**真实光标**补发一条移动，
hover 就被真实位置盖掉，读出来是"没悬停"，是探针自身的竞态，不是产品缺陷。
这里全程用 SetCursorPos 移动**真实光标**（菜单持有 capture，任何位置都会把
真实坐标投递进来），与健康的人类操作一致。

判据（上下文菜单 = 文件右键）：
  H1 未悬停时：所有行都无悬停底色（= panel surface）
  H2 真光标移到带 ▸ 的行上：该行底色变 hover_bg，且弹出 1 个子菜单
  H3 移去另一条 ▸ 行：子菜单换成那一条（旧子菜单被替换，不是叠加）
  H4 第53轮的"钉住"语义仍在：从 ▸ 行移到没有子菜单的行上，子菜单**保持**
     打开（不应被鼠标移开收掉）
"""
import ctypes
import ctypes.wintypes as wt
import os
import struct
import subprocess
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
u.PostMessageW.restype = ctypes.c_bool
u.PostMessageW.argtypes = [wt.HWND, ctypes.c_uint, ctypes.c_ulong, ctypes.c_long]
u.SetCursorPos.restype = wt.BOOL
u.SetCursorPos.argtypes = [ctypes.c_int, ctypes.c_int]
u.IsWindowVisible.restype = wt.BOOL
u.GetClassNameW.restype = ctypes.c_int
u.GetClassNameW.argtypes = [wt.HWND, ctypes.c_wchar_p, ctypes.c_int]
u.EnumWindows.restype = ctypes.c_bool
ENUMCB = ctypes.WINFUNCTYPE(ctypes.c_bool, wt.HWND, ctypes.c_long)
g.CreateCompatibleDC.argtypes = [ctypes.c_void_p]
g.CreateCompatibleDC.restype = ctypes.c_void_p
g.CreateCompatibleBitmap.restype = ctypes.c_void_p
g.CreateCompatibleBitmap.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int]
g.SelectObject.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
g.SelectObject.restype = ctypes.c_void_p
g.GetDIBits.restype = ctypes.c_int
g.GetDIBits.argtypes = [ctypes.c_void_p, ctypes.c_void_p,
                        ctypes.c_uint, ctypes.c_uint, ctypes.c_void_p, ctypes.c_void_p,
                        ctypes.c_uint]
# GDI 句柄值常超 2^31，必须用 c_void_p，用默认整型会 OverflowError
g.DeleteObject.restype = ctypes.c_bool
g.DeleteObject.argtypes = [ctypes.c_void_p]
g.DeleteDC.restype = ctypes.c_bool
g.DeleteDC.argtypes = [ctypes.c_void_p]

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\ZenFile.exe"
WM_RBUTTONUP = 0x0205
BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "NexHover54")
os.makedirs(BASE, exist_ok=True)
for i in range(6):
    open(os.path.join(BASE, "f%d.txt" % i), "w").write("x" * i)

SURFACE = "#2B2B2B"   # t.surface（暗色），未悬停的行底色
HOVER = "#3D3D3D"     # t.hover_bg（暗色），悬停行底色


def menus():
    out = []

    @ENUMCB
    def cb(h, _lp):
        if not u.IsWindowVisible(h):
            return True
        c = ctypes.create_unicode_buffer(64)
        u.GetClassNameW(h, c, 64)
        if c.value == "ZenFileMenu":
            r = wt.RECT()
            u.GetWindowRect(h, ctypes.byref(r))
            out.append((int(h), r.left, r.top, r.right - r.left, r.bottom - r.top))
        return True

    u.EnumWindows(cb, 0)
    return out


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
    g.DeleteObject(bmp)
    g.DeleteDC(mdc)
    return buf.raw, stride, w, h


def px(raw, stride, x, y):
    o = y * stride + x * 4
    return "#%02X%02X%02X" % (raw[o + 2], raw[o + 1], raw[o])


def runs(vals, gap=6):
    out, start, prev = [], None, None
    for v in vals:
        if start is None:
            start, prev = v, v
        elif v - prev > gap:
            out.append((start, prev))
            start, prev = v, v
        else:
            prev = v
    if start is not None:
        out.append((start, prev))
    return out


def arrow_rows(raw, stride, w, h):
    """右侧 ▸ 箭头所在的 y（行中心）。"""
    ys = [y for y in range(4, h - 4)
          if sum(1 for x in range(w - 26, w - 8)
                 if sum(int(px(raw, stride, x, y)[i:i + 2], 16) for i in (1, 3, 5)) > 200) >= 3]
    return [(a + b) // 2 for a, b in runs(ys) if b - a >= 2]


fails = []


def check(tag, cond, detail=""):
    print("  %-4s %s %s" % ("PASS" if cond else "FAIL", tag, detail))
    if not cond:
        fails.append(tag)


def main():
    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    time.sleep(0.5)
    subprocess.Popen([EXE, BASE])
    hw = None
    for _ in range(60):
        time.sleep(0.25)
        hw = u.FindWindowW("ZenFileMainWindow", None)
        if hw:
            break
    if not hw:
        print("FAIL 主窗口没起来")
        return 1
    time.sleep(1.5)

    # 打开文件右键菜单（假右键即可，打开本身不是本轮判据）
    u.PostMessageW(hw, WM_RBUTTONUP, 0, (152 << 16) | (400 & 0xFFFF))
    time.sleep(1.2)
    ms = menus()
    if not ms:
        print("FAIL 菜单没弹出")
        return 1
    mh, ml, mt, mw, mhh = ms[0]
    print("菜单 @(%d,%d) %dx%d" % (ml, mt, mw, mhh))

    raw, stride, w, h = grab(mh)
    arrows = arrow_rows(raw, stride, w, h)
    print("▸ 箭头行 y =", arrows)
    if not arrows:
        print("FAIL 找不到任何带子菜单的行（探针自身失效）")
        return 1

    def sub_count():
        return len([x for x in menus() if x[0] != mh])

    def hover_row(x_off=60):
        """返回 (该行底色, 子菜单数)：按当前真实光标所在行采样。"""
        time.sleep(0.45)
        r2, s2, w2, h2 = grab(mh)
        return r2, s2, h2

    # H1 光标远离菜单 → 不应有任何行被高亮
    u.SetCursorPos(3, 3)
    time.sleep(0.5)
    r2, s2, w2, h2 = grab(mh)
    lit = [y for y in arrows if px(r2, s2, 200, y) == HOVER]
    check("H1", not lit, "未悬停时高亮的 ▸ 行 = %s（应为空）" % lit)

    # H2 真光标移到第一条 ▸ 行：该行高亮 + 弹出 1 个子菜单
    y0 = arrows[0]
    u.SetCursorPos(ml + 60, mt + y0)
    time.sleep(0.6)
    r2, s2, w2, h2 = grab(mh)
    bg = px(r2, s2, 200, y0)
    check("H2", bg == HOVER and sub_count() == 1,
          "底色 %s（期望 %s） 子菜单 %d 个" % (bg, HOVER, sub_count()))

    # H3 移到另一条 ▸ 行：子菜单仍只有 1 个（旧的被替换，不是叠加）
    if len(arrows) > 1:
        y1 = arrows[1]
        u.SetCursorPos(ml + 60, mt + y1)
        time.sleep(0.6)
        r2, s2, w2, h2 = grab(mh)
        bg1 = px(r2, s2, 200, y1)
        check("H3", bg1 == HOVER and sub_count() == 1,
              "底色 %s 子菜单 %d 个（仍应为 1）" % (bg1, sub_count()))
        last_arrow = y1
    else:
        last_arrow = y0
        print("  SKIP H3 只有一条 ▸ 行")

    # H4 第53轮"钉住"语义：移到没有子菜单的行（菜单顶部区域），子菜单应保持打开
    plain_y = arrows[0] - 60 if arrows[0] > 70 else None
    if plain_y:
        u.SetCursorPos(ml + 60, mt + plain_y)
        time.sleep(0.6)
        check("H4", sub_count() == 1,
              "移开到普通行后子菜单 %d 个（应保持 1 = 钉住）" % sub_count())
    else:
        print("  SKIP H4 找不到合适的普通行")

    # H5 最关键的一条：切到新子菜单后，它必须还能收到鼠标 —— 否则**点里面的条目
    # 毫无反应**（这正是过期 WM_MENU_SUBCLOSED 把捕获从新子菜单手里抢走的后果）。
    # 判据：把真光标移进新子菜单的行上，那一行应当出现悬停高亮。
    subs = [x for x in menus() if x[0] != mh]
    if subs:
        sh, sl, st, sw, shh = subs[0]
        xprobe = 40 if sw > 60 else max(5, sw - 20)

        def lit_runs(hwnd, xp):
            rr, ss, ww, hh = grab(hwnd)
            return runs([y for y in range(4, hh - 4) if px(rr, ss, xp, y) == HOVER])

        before = lit_runs(sh, xprobe)
        u.SetCursorPos(sl + xprobe, st + 18)   # 移进子菜单靠上的一行
        time.sleep(0.6)
        after = lit_runs(sh, xprobe)
        check("H5", len(after) >= 1,
              "子菜单内悬停高亮行 %d 段（移入前 %d 段）→ 新子菜单%s收到鼠标"
              % (len(after), len(before), "能" if len(after) >= 1 else "不能"))
    else:
        print("  SKIP H5 没有子菜单可测")

    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    print("\n总判定:", "PASS 悬停展示已恢复" if not fails else "FAIL " + "; ".join(fails))
    return 0 if not fails else 1


if __name__ == "__main__":
    raise SystemExit(main())
