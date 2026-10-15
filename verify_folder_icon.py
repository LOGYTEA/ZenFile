"""验证第 3 项：带自定义图标的文件夹必须用自定义图标，而不是通用黄文件夹。

分两轮，素材都造在 Desktop 下：

  素材目录：%TEMP%/ZenIconVerify（**不写桌面**）

  轮 1（合成 ini）：A_IconFile / B_IconResource / C_Plain
        —— 覆盖 IconFile+IconIndex、IconResource+环境变量展开、无 ini 三种情况。
  轮 2（真实 ini）：把系统给 桌面/下载/音乐/图片/视频 写的 **原始 desktop.ini**
        原样复制过来（UTF-16LE 带 BOM、IconResource 负数资源 ID），再看图标。

实现要点（踩过的坑都记在这）：
  1. 目录图标必须 **不带 SHGFI_USEFILEATTRIBUTES** 去问 Shell。带这个标志 Shell
     不访问磁盘，永远只给通用黄文件夹（实测 Pictures/Downloads 都是 #FFE896）。
  2. Shell 只在文件夹带 system/read-only 属性时才认 desktop.ini —— 这正是
     Windows 给文件夹设自定义图标时的做法，测试素材必须复现（attrib +s）。
  3. 导航别用地址栏 SetWindowText + WM_KEYDOWN，实测不生效；用启动参数直达。
  4. 跨进程读 EDIT 文本要用本地缓冲 + WM_GETTEXT，GetWindowTextW 拿不到。

判定全部走抓屏取像素主色，不靠"看着像"。
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
sh = ctypes.WinDLL("shell32", use_last_error=True)

u.FindWindowW.restype = wt.HWND
u.FindWindowW.argtypes = [ctypes.c_wchar_p, ctypes.c_wchar_p]
u.GetWindowRect.restype = wt.BOOL
u.GetWindowRect.argtypes = [wt.HWND, ctypes.POINTER(wt.RECT)]
u.GetWindowDC.restype = wt.HDC
u.PrintWindow.restype = wt.BOOL
u.PrintWindow.argtypes = [wt.HWND, wt.HDC, ctypes.c_uint]
u.GetDlgItem.restype = wt.HWND
u.SendMessageW.restype = ctypes.c_ssize_t
g.CreateCompatibleDC.argtypes = [wt.HDC]
g.CreateCompatibleDC.restype = wt.HDC
g.CreateCompatibleBitmap.restype = wt.HBITMAP
g.CreateCompatibleBitmap.argtypes = [wt.HDC, ctypes.c_int, ctypes.c_int]
g.SelectObject.argtypes = [wt.HDC, wt.HGDIOBJ]
g.SelectObject.restype = wt.HGDIOBJ
g.GetDIBits.restype = ctypes.c_int
g.GetDIBits.argtypes = [wt.HDC, wt.HBITMAP, ctypes.c_uint, ctypes.c_uint,
                        ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint]
sh.ExtractIconExW.restype = ctypes.c_uint
sh.ExtractIconExW.argtypes = [ctypes.c_wchar_p, ctypes.c_int,
                              ctypes.POINTER(wt.HICON), ctypes.POINTER(wt.HICON), ctypes.c_uint]

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\ZenFile.exe"
WM_GETTEXT = 0x000D

# 素材一律落在 TEMP：测试脚本不许往用户桌面写东西
DESK = os.path.join(os.environ.get("TEMP", "C:\\Windows\\Temp"), "ZenIconVerify")
ICO = os.path.join(DESK, "nex_red.ico")
ICO2 = os.path.join(DESK, "nex_green.ico")

# 内容区左边界 157（verify_align 实测），图标左边距 21，图标 16
ICON_X0, ICON_X1 = 157 + 21, 157 + 21 + 16

FOLDER_YELLOW = lambda c: c[0] > 200 and c[1] > 170 and (c[0] - c[2]) > 45
# 第九轮起，带 system 属性的项图标会被**调淡**（隐藏/系统项的视觉区分）。
# 这两个素材目录正是 attrib +s 的（Shell 只在带 system 时才认 desktop.ini），
# 所以主色不再是纯红/纯绿，而是与暗底混合后的 #801212 / #128012。
# 判定因此改成"色相"而不是"色值"：红 = R 明显高于 G/B，绿 = G 明显高于 R/B。
IS_RED = lambda c: c[0] > 60 and c[0] > c[1] + 40 and c[0] > c[2] + 40
IS_GREEN = lambda c: c[1] > 60 and c[1] > c[0] + 40 and c[1] > c[2] + 40


# ------------------------------------------------------------------ 素材
def make_ico(path, rgb=(255, 0, 0), size=32):
    w = h = size
    r, gg, b = rgb            # rgb 是 (R,G,B)；图标 XOR 是 BGRA，别写反
    xor = bytearray()
    for _ in range(h):
        for _x in range(w):
            xor += bytes((b, gg, r, 255))
    andmask = bytearray(w * h // 8)
    bih = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, len(xor) + len(andmask), 0, 0, 0, 0)
    img = bih + bytes(xor) + bytes(andmask)
    hdr = struct.pack("<HHH", 0, 1, 1)
    ent = struct.pack("<BBBBHHII", w, h, 0, 0, 1, 32, len(img), 22)
    with open(path, "wb") as f:
        f.write(hdr + ent + img)


def rmdir(p):
    if os.path.isdir(p):
        subprocess.run(["cmd", "/c", "rmdir", "/S", "/Q", p], capture_output=True)


def setup_synthetic(base):
    os.makedirs(DESK, exist_ok=True)
    os.makedirs(base, exist_ok=True)
    make_ico(ICO, (255, 0, 0))
    make_ico(ICO2, (0, 255, 0))
    for name in ("A_IconFile", "B_IconResource", "C_Plain"):
        os.makedirs(os.path.join(base, name), exist_ok=True)
    with open(os.path.join(base, "A_IconFile", "desktop.ini"), "w", encoding="utf-8") as f:
        f.write("[.ShellClassInfo]\r\nIconFile=%s\r\nIconIndex=0\r\n" % ICO)
    # 这个字面量没经过 % 格式化，所以环境变量只能写一个 %。
    # 路径要跟着 ICO2 实际所在位置走（素材在 TEMP，不能写死 Desktop）。
    rel = os.path.relpath(ICO2, os.environ.get("TEMP", "C:\\Windows\\Temp"))
    with open(os.path.join(base, "B_IconResource", "desktop.ini"), "w", encoding="utf-8") as f:
        f.write("[.ShellClassInfo]\r\nIconResource=%TEMP%\\{},0\r\n"
                .format(rel.replace("/", "\\")))
    p = os.path.join(base, "C_Plain", "desktop.ini")
    if os.path.exists(p):
        os.remove(p)
    for name in ("A_IconFile", "B_IconResource"):
        subprocess.run(["attrib", "+s", os.path.join(base, name)], capture_output=True)
    return [
        ("A_IconFile      IconFile+IconIndex 纯红", IS_RED),
        ("B_IconResource  IconResource+环境变量 纯绿", IS_GREEN),
        ("C_Plain         无 desktop.ini 通用黄", FOLDER_YELLOW),
    ]


def setup_real(base):
    """把系统给已知文件夹写的原始 desktop.ini 复制过来（UTF-16LE 带 BOM）。"""
    os.makedirs(base, exist_ok=True)
    home = os.environ["USERPROFILE"]
    pairs = [("1_Desktop", "Desktop"), ("2_Downloads", "Downloads"),
             ("3_Music", "Music"), ("4_Pictures", "Pictures"), ("5_Videos", "Videos")]
    real = {}
    for dst, src in pairs:
        src_ini = os.path.join(home, src, "desktop.ini")
        if not os.path.isfile(src_ini):
            print(f"  跳过 {src}：系统没有 desktop.ini")
            continue
        d = os.path.join(base, dst)
        os.makedirs(d, exist_ok=True)
        shutil.copyfile(src_ini, os.path.join(d, "desktop.ini"))
        subprocess.run(["attrib", "+s", d], capture_output=True)
        real[dst] = src_ini
    os.makedirs(os.path.join(base, "9_Plain"), exist_ok=True)
    subprocess.run(["attrib", "-s", os.path.join(base, "9_Plain")], capture_output=True)
    expect = [(f"{d}   ← 系统真实 ini({os.path.basename(s)})",
               (lambda c: not FOLDER_YELLOW(c))) for d, s in real.items()]
    expect.append(("9_Plain   无 ini 通用黄", FOLDER_YELLOW))
    return expect


# ------------------------------------------------------------------ 抓屏
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


def row_bands(raw, stride, h, min_h=3):
    """在图标列里找"有内容"的纵向条带 —— 自动定位每一行的图标，
    行距/表头高度一变也不会整段结论失效。"""
    inky = []
    for y in range(h):
        hit = False
        for x in range(ICON_X0, ICON_X1):
            if sum(px(raw, stride, x, y)) > 140:      # 背景是 #202020 级近黑
                hit = True
                break
        inky.append(hit)
    bands = []
    y = 0
    while y < h:
        if inky[y]:
            y0 = y
            while y < h and inky[y]:
                y += 1
            if y - y0 >= min_h:
                bands.append((y0, y))
        else:
            y += 1
    return bands


def row_bg(raw, stride, w, y0, y1):
    """取这一行的真实底色：采样行最右端的空白区（大小列右侧）。
    焦点行/hover 行会被刷成 hover_bg，不先把它认出来，图标主色就会被背景吃掉。"""
    cnt = {}
    for y in range(y0, y1):
        for x in range(w - 45, w - 12):
            cnt[px(raw, stride, x, y)] = cnt.get(px(raw, stride, x, y), 0) + 1
    return max(cnt.items(), key=lambda kv: kv[1])[0] if cnt else (0x20, 0x20, 0x20)


def dominant(raw, stride, y0, y1, bg):
    cnt = {}
    for y in range(y0, y1):
        for x in range(ICON_X0, ICON_X1):
            c = px(raw, stride, x, y)
            if max(abs(c[k] - bg[k]) for k in range(3)) < 16:
                continue      # 与行底色同色 → 是背景不是图标
            cnt[c] = cnt.get(c, 0) + 1
    if not cnt:
        return None, 0
    top = sorted(cnt.items(), key=lambda kv: -kv[1])
    return top[0], sum(cnt.values())


def addr_text(hwnd):
    edit = u.GetDlgItem(hwnd, 101)
    buf = (ctypes.c_wchar * 512)()
    u.SendMessageW(edit, WM_GETTEXT, 512, ctypes.addressof(buf))
    return buf.value


def kill_all():
    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    time.sleep(0.6)


def run(base, expect, title):
    print(f"\n===== {title}  ({base})")
    kill_all()
    subprocess.Popen([EXE, base])
    hwnd = None
    for _ in range(80):
        time.sleep(0.25)
        hwnd = u.FindWindowW("ZenFileMainWindow", None)
        if hwnd:
            break
    if not hwnd:
        print("窗口没起来")
        return False
    time.sleep(2.5)
    # 先把鼠标挪到状态栏角落：否则残留的 hover 会盖住某个图标，主色采样就失真了
    u.PostMessageW(hwnd, 0x0200, 0, (300 << 16) | (80 & 0xFFFF))  # 导航窗格，远离列表
    time.sleep(0.4)
    addr = addr_text(hwnd)
    print(f"地址栏: {addr!r}")
    if base.lower() not in addr.lower():
        print("导航失败")
        return False

    raw, stride, w, h = grab(hwnd)
    bands = [b for b in row_bands(raw, stride, h) if b[0] > 90 and b[0] < h - 40]
    print(f"窗口 {w}x{h}，图标列 x={ICON_X0}..{ICON_X1}，图标条带 {len(bands)} 个")
    if len(bands) < len(expect):
        print(f"条目数不足：期望 {len(expect)} 行，只有 {len(bands)} 个图标")
        return False

    ok = True
    for i, (nm, judge) in enumerate(expect):
        y0, y1 = bands[i]
        bg = row_bg(raw, stride, w, y0, y1)
        (c, hits), npx = dominant(raw, stride, y0, y1, bg)
        print(f"      行底色 #{bg[0]:02X}{bg[1]:02X}{bg[2]:02X}")
        good = judge(c)
        print(f"  行{i} y={y0:3d} {nm:44s} #{c[0]:02X}{c[1]:02X}{c[2]:02X} "
              f"ink={npx:3d} -> {'PASS' if good else 'FAIL'}")
        ok = ok and good
    return ok


# ------------------------------------------------------------------ main
if __name__ == "__main__":
    n = sh.ExtractIconExW(ICO, 0, ctypes.byref(wt.HICON()), None, 1)
    n2 = sh.ExtractIconExW(ICO2, 0, ctypes.byref(wt.HICON()), None, 1)

    os.makedirs(DESK, exist_ok=True)
    base1 = os.path.join(DESK, "ZenIconTest")
    rmdir(base1)
    make_ico(ICO, (255, 0, 0))
    make_ico(ICO2, (0, 255, 0))
    exp1 = setup_synthetic(base1)
    print(f"素材自检 ExtractIconExW red n={n} / green n={n2}")
    # 素材文件夹为了复刻 Windows 的行为都打了 attrib +s（Shell 只在带 system 属性时
    # 才认 desktop.ini）。第九轮新增的"受保护的系统文件"开关默认关闭，会把它们
    # 全部过滤掉 —— 这个测试要显式打开该开关，否则测不到图标，只测到了过滤器。
    import json as _json
    _cfg = os.path.join(os.environ.get("APPDATA", ""), "ZenFile", "settings.user.json")
    os.makedirs(os.path.dirname(_cfg), exist_ok=True)
    with open(_cfg, "w", encoding="utf-8") as _f:
        _json.dump({"files": {"view": {"show_system": True, "show_hidden": True}}}, _f)
    ok1 = run(base1, exp1, "轮1 合成 desktop.ini")

    base2 = os.path.join(DESK, "ZenRealIni")
    rmdir(base2)
    exp2 = setup_real(base2)
    ok2 = run(base2, exp2, "轮2 系统真实 desktop.ini")

    try:
        os.remove(_cfg)
    except OSError:
        pass
    kill_all()
    print(f"\n轮1: {'PASS' if ok1 else 'FAIL'}   轮2: {'PASS' if ok2 else 'FAIL'}")
    print("总判定:", "PASS" if (ok1 and ok2) else "FAIL")
    sys.exit(0 if (ok1 and ok2) else 1)
