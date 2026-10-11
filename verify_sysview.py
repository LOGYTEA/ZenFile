"""验证第 12 轮第 2 项：查看布局**默认继承 Windows 资源管理器**，在软件里改了也**同步回系统**。

两条链路都要验：

  继承（系统 -> 软件）
      手动把某个文件夹在资源管理器 bag 里的 Vid 改成"大图标"，
      再开 ZenFile 进这个文件夹，看它是不是真的用大图标画（96px 图标块）。

  同步（软件 -> 系统）
      在 ZenFile 里按 Ctrl+Shift+V 换视图，关掉之后去注册表里看
      同一个文件夹的 bag 是不是也被改了。

怎么定位"这个文件夹的 bag 号"：不解析 ITEMIDLIST（格式随版本变过），
而是**前后各扫一遍全部 bag，取那个值变了的** —— 软件一次只可能改当前目录，
所以变化的那个必然就是目标。跑完按原值还原，不打扰用户真实设置。
"""
import ctypes
import ctypes.wintypes as wt
import json
import os
import struct
import subprocess
import time
import winreg

import verify_env

verify_env.isolate()
verify_env.clean()

u = ctypes.WinDLL("user32", use_last_error=True)
g = ctypes.WinDLL("gdi32", use_last_error=True)
u.FindWindowW.restype = wt.HWND
u.FindWindowW.argtypes = [ctypes.c_wchar_p, ctypes.c_wchar_p]
u.GetWindowRect.restype = wt.BOOL
u.GetWindowRect.argtypes = [wt.HWND, ctypes.POINTER(wt.RECT)]
u.GetWindowDC.restype = wt.HDC
u.PrintWindow.restype = wt.BOOL
u.PrintWindow.argtypes = [wt.HWND, wt.HDC, ctypes.c_uint]
u.SetForegroundWindow.restype = wt.BOOL
u.SetForegroundWindow.argtypes = [wt.HWND]
u.keybd_event.restype = None
u.keybd_event.argtypes = [ctypes.c_uint8, ctypes.c_uint8, ctypes.c_uint, ctypes.c_ulong]
u.ReleaseDC.argtypes = [wt.HWND, wt.HDC]
g.CreateCompatibleDC.argtypes = [wt.HDC]
g.CreateCompatibleDC.restype = wt.HDC
g.CreateCompatibleBitmap.restype = wt.HBITMAP
g.CreateCompatibleBitmap.argtypes = [wt.HDC, ctypes.c_int, ctypes.c_int]
g.SelectObject.argtypes = [wt.HDC, wt.HGDIOBJ]
g.SelectObject.restype = wt.HGDIOBJ
g.GetDIBits.restype = ctypes.c_int
g.GetDIBits.argtypes = [wt.HDC, wt.HBITMAP, ctypes.c_uint, ctypes.c_uint,
                        ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint]
g.DeleteObject.argtypes = [wt.HGDIOBJ]
g.DeleteDC.argtypes = [wt.HDC]

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\ZenFile.exe"
APPDATA = os.environ.get("APPDATA", os.path.expanduser("~"))
CFG_DIR = os.path.join(APPDATA, "ZenFile")
TARGET = r"E:\\"

VID_DETAILS = "{137E7700-3573-11CF-AE69-08002B2E1262}"
VID_ICONS = "{0057D0E0-3573-11CF-AE69-08002B2E1262}"
VID_LIST = "{0E1FA5E0-3573-11CF-AE69-08002B2E1262}"
VID_TILES = "{65F125E5-7BE1-4810-BA9D-D271C8432CE3}"
VID_NAME = {VID_DETAILS: "详细信息", VID_ICONS: "大图标", VID_LIST: "列表", VID_TILES: "中等图标"}

ROOTS = [
    r"Software\Classes\Local Settings\Software\Microsoft\Windows\Shell\Bags",
    r"Software\Microsoft\Windows\Shell\Bags",
]

ok_all = True


def mark(ok, msg):
    global ok_all
    ok_all = ok_all and ok
    print(f"    {'PASS' if ok else 'FAIL'}  {msg}")


# ---------------------------------------------------------------- 注册表

def snapshot():
    """扫一遍所有 bag 的 Shell 键（含 FolderType 子键），记下四个视图值。"""
    out = {}
    for root in ROOTS:
        try:
            base = winreg.OpenKey(winreg.HKEY_CURRENT_USER, root)
        except OSError:
            continue
        i = 0
        while True:
            try:
                slot = winreg.EnumKey(base, i)
            except OSError:
                break
            i += 1
            for sub in ("Shell",):
                shell = f"{root}\\{slot}\\{sub}"
                try:
                    h = winreg.OpenKey(winreg.HKEY_CURRENT_USER, shell)
                except OSError:
                    continue
                out[shell] = read_vals(h)
                winreg.CloseKey(h)
                j = 0
                while True:
                    try:
                        ft = winreg.EnumKey(h, j)
                    except OSError:
                        break
                    j += 1
                    k = f"{shell}\\{ft}"
                    try:
                        h2 = winreg.OpenKey(winreg.HKEY_CURRENT_USER, k)
                    except OSError:
                        continue
                    out[k] = read_vals(h2)
                    winreg.CloseKey(h2)
        winreg.CloseKey(base)
    return out


def read_vals(h):
    vals = {}
    for name in ("Vid", "Mode", "LogicalViewMode", "IconSize"):
        try:
            vals[name] = winreg.QueryValueEx(h, name)[0]
        except OSError:
            pass
    return vals


def write_vals(key, vals, vid, mode, logical, icon):
    h = winreg.OpenKey(winreg.HKEY_CURRENT_USER, key, 0, winreg.KEY_SET_VALUE)
    winreg.SetValueEx(h, "Vid", 0, winreg.REG_SZ, vid)
    winreg.SetValueEx(h, "Mode", 0, winreg.REG_DWORD, mode)
    winreg.SetValueEx(h, "LogicalViewMode", 0, winreg.REG_DWORD, logical)
    if "IconSize" in vals:
        winreg.SetValueEx(h, "IconSize", 0, winreg.REG_DWORD, icon)
    winreg.CloseKey(h)


def restore_vals(key, vals, now):
    """还原成 vals；现在有、原本没有的值要**删掉**（不是留着）——
    否则跑一次脚本就给用户那个文件夹多出几个资源管理器本不认识的值。"""
    h = winreg.OpenKey(winreg.HKEY_CURRENT_USER, key, 0, winreg.KEY_SET_VALUE)
    for name in now:
        if name not in vals:
            try:
                winreg.DeleteValue(h, name)
            except OSError:
                pass
    for name, v in vals.items():
        winreg.SetValueEx(h, name, 0,
                          winreg.REG_SZ if isinstance(v, str) else winreg.REG_DWORD, v)
    winreg.CloseKey(h)


# ---------------------------------------------------------------- 抓图

def launch(arg):
    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    time.sleep(0.5)
    subprocess.Popen([EXE, arg])
    for _ in range(60):
        time.sleep(0.25)
        hw = u.FindWindowW("ZenFileMainWindow", None)
        if hw:
            time.sleep(1.8)
            return hw
    return None


def grab(hwnd):
    for _ in range(8):
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
        u.ReleaseDC(hwnd, src)
        raw = buf.raw
        if any(raw[i] for i in range(0, min(len(raw), 40000), 997)):
            return raw, stride, w, h
        time.sleep(0.35)
    return raw, stride, w, h


def px(raw, stride, x, y):
    o = y * stride + x * 4
    return (raw[o + 2], raw[o + 1], raw[o])


def tap(hwnd, vk, ctrl=False, shift=False):
    """发一个组合键。keybd_event 走的是真实的键盘输入流，GetKeyState 才读得到
    修饰键状态（PostMessage(WM_KEYDOWN) 不行，修饰键状态不会同步）。"""
    u.SetForegroundWindow(hwnd)
    time.sleep(0.3)
    if ctrl:
        u.keybd_event(0x11, 0, 0, 0)
    if shift:
        u.keybd_event(0x10, 0, 0, 0)
    u.keybd_event(vk, 0, 0, 0)
    time.sleep(0.06)
    u.keybd_event(vk, 0, 2, 0)
    time.sleep(0.06)
    if shift:
        u.keybd_event(0x10, 0, 2, 0)
    if ctrl:
        u.keybd_event(0x11, 0, 2, 0)
    time.sleep(0.7)


# ---------------------------------------------------------------- 判据

TITLE_H, TOOLBAR_H, HEADER_H = 34, 40, 26
WINDOW_BG = (0x20, 0x20, 0x20)
SB_W = 157


def vsplit_x(raw, stride, w, h):
    """导航窗格右边界（border 色竖线）。"""
    best, bestn = None, 0
    for x in range(100, min(300, w)):
        n = sum(1 for y in range(TITLE_H + TOOLBAR_H + 25, h - 40, 4)
                if abs(px(raw, stride, x, y)[0] - 0x53) <= 12
                and abs(px(raw, stride, x, y)[1] - 0x53) <= 12)
        if n > bestn:
            best, bestn = x, n
    return best if bestn >= 8 else SB_W


def ink_col(raw, stride, x, y0, y1):
    """这一竖列上有多少像素不是窗口底色。"""
    n = 0
    for y in range(y0, y1):
        p = px(raw, stride, x, y)
        if abs(p[0] - WINDOW_BG[0]) > 14 or abs(p[1] - WINDOW_BG[1]) > 14 or abs(p[2] - WINDOW_BG[2]) > 14:
            n += 1
    return n


def max_icon_block(raw, stride, w, h):
    """列表区第一格里"最实的一竖列"有多少非背景像素。

    大图标：图标 96px 高，中间某列几乎整列都是像素 -> 接近 96。
    详细信息：行高 21，该区间塞了表头 + 5 行文字，任何一列都远不到 70。
    """
    sx = vsplit_x(raw, stride, w, h)
    left = sx + 21
    top = TITLE_H + TOOLBAR_H
    best = 0
    for x in range(left + 36, min(left + 132, w - 2)):
        best = max(best, ink_col(raw, stride, x, top + 6, top + 110))
    return best, sx


# ---------------------------------------------------------------- 主流程

print("=" * 70)
print("第 2 项：查看布局与资源管理器互认")

before = snapshot()
print(f"  扫描到 {len(before)} 个 bag 键")

# ---- 链路 A：软件 -> 系统 ----
print("\n[同步] 在 ZenFile 里换视图，看注册表跟着变")
hw = launch(TARGET)
if hw is None:
    mark(False, "主窗口没起来")
    raise SystemExit(1)
tap(hw, 0x33, ctrl=True)         # Ctrl+3 -> 中等图标（与系统原值不同，diff 才看得见）
subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
time.sleep(0.8)

after = snapshot()
changed = sorted([k for k in after if before.get(k) != after[k]], key=len)
print(f"  值发生变化的 bag 键：{changed[:3]}（共 {len(changed)}）")
mark(len(changed) >= 1, f"换视图之后资源管理器的 bag 被改写了（{len(changed)} 个）")

target_key = changed[0] if changed else None
orig = before.get(target_key, {}) if target_key else {}
if target_key:
    print(f"  目标 bag：{target_key}")
    print(f"    {VID_NAME.get(before[target_key].get('Vid'), '?')} -> "
          f"{VID_NAME.get(after[target_key].get('Vid'), '?')}")
    mark(after[target_key].get("Vid") == VID_TILES,
         f"写回的是\"中等图标\"的 Vid（实际 {VID_NAME.get(after[target_key].get('Vid'), '?')}）")

# ---- 链路 B：系统 -> 软件 ----
if target_key:
    print("\n[继承] 把系统侧改成大图标，开 ZenFile 看它跟不跟")
    write_vals(target_key, orig, VID_ICONS, 1, 3, 96)
    hw = launch(TARGET)
    raw, stride, W, H = grab(hw)
    big, sx = max_icon_block(raw, stride, W, H)
    print(f"  列表区左起点 x={sx + 21}，第一格最实竖列非背景像素 = {big}")
    mark(big >= 70, f"呈现为大图标（96px 图标块，实测 {big}）")

    print("\n[不跟随] 把 default_mode 固定为 details，不该再被系统带跑")
    os.makedirs(CFG_DIR, exist_ok=True)
    with open(os.path.join(CFG_DIR, "settings.json"), "w", encoding="utf-8") as f:
        json.dump({"files": {"view": {"default_mode": "details"}}}, f)
    hw = launch(TARGET)
    raw, stride, W, H = grab(hw)
    small, sx = max_icon_block(raw, stride, W, H)
    print(f"  固定 details 后同一位置非背景像素 = {small}")
    mark(small < big - 25, f"回到详细信息布局（{big} -> {small}）")

    # 还原：bag 恢复原值；写回过程中被新建出来的键删掉
    for k in changed:
        # 键名里的第一段是 root，root 之后才是相对路径
        rel = k.split("\\Bags\\", 1)[1]
        for root in ROOTS:
            if k.startswith(root):
                rel = k[len(root):].lstrip("\\")
                break
        if k not in before:
            try:
                winreg.DeleteKey(winreg.HKEY_CURRENT_USER, k)
                print(f"  删除写入过程中新建的键 {rel}")
            except OSError:
                pass
            continue
        try:
            restore_vals(k, before[k], after.get(k, {}))
            print(f"  已还原 {rel} -> {VID_NAME.get(before[k].get('Vid'), before[k].get('Vid'))}")
        except OSError as e:
            print(f"  还原失败：{rel} {e}")

subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
print("\n" + ("全部通过" if ok_all else "有失败项"))
