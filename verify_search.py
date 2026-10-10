"""验证第 11 轮第 4 项：Everything 式整卷秒搜。

判定全部用像素，不用肉眼，也不用"我觉得应该有"：

  1) 列表行数用**竖向墨迹带**数出来（行距 21px，表头 11px 高，两者能分开）
  2) "搜到了别的目录里的文件" = 结果行数 > 当前目录里的匹配数
  3) "位置"列 = 行内在 x 390..465 这段有墨迹（非搜索态这段是空的）
  4) 秒搜 = 从回车到行数变化的时间

素材只在 %TEMP%；索引缓存先备份再还原，不留在用户机器上。
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
WM_SETTEXT = 0x000C
WM_KEYDOWN = 0x0100
CACHE = os.path.join(os.environ.get("LOCALAPPDATA", ""), "NexFile", "index", "C.bin")

BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "NexSearch")
A = os.path.join(BASE, "A")
B = os.path.join(BASE, "B")
D = os.path.join(B, "deep")
for d in (A, B, D):
    os.makedirs(d, exist_ok=True)
for i in range(3):
    open(os.path.join(A, f"plain{i}.txt"), "w").write("x")
open(os.path.join(A, "nexq7x_alpha.txt"), "w").write("x")
open(os.path.join(B, "nexq7x_beta.txt"), "w").write("x")
open(os.path.join(D, "nexq7x_gamma.txt"), "w").write("x")
# A 里只有 1 个匹配；B 与 B\deep 各 1 个 —— 搜出 3 行才说明真的跨出了当前目录
IN_A, TOTAL = 1, 3

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


def row_count(hwnd, view):
    """数文件行：表头 1 条（11px）+ 每个文件行 1 条（17px）。

    空列表会画一行"此文件夹为空"占位，它跟真行一样是一条墨迹带 —— 用占位**只有
    左边 90px 有字**、真行一定会画到"修改日期/大小"那一带（x > view.left+380）
    把两者分开，否则"搜不到"会被数成"搜到 1 个"。
    """
    x0, y0, x1, y1 = view
    raw, stride, w, h = grab(hwnd)
    near, far = [], []
    for y in range(y0 + 2, y0 + 300):
        a = b = False
        for x in range(x0 + 30, x0 + 120):
            o = y * stride + x * 4
            if raw[o] + raw[o + 1] + raw[o + 2] > 330:
                a = True
                break
        if a:
            for x in range(x0 + 380, x1 - 20):
                o = y * stride + x * 4
                if raw[o] + raw[o + 1] + raw[o + 2] > 330:
                    b = True
                    break
        near.append(a)
        far.append(b)
    bands, run, cur = [], 0, False
    for a, b in zip(near, far):
        if a:
            run += 1
            cur = cur or b
        else:
            if run >= 6:
                bands.append(cur)
            run, cur = 0, False
    if run >= 6:
        bands.append(cur)
    # 表头那条也带右侧墨迹（列标题），减掉
    return max(0, sum(1 for c in bands if c) - 1)


def ink_in(raw, stride, x0, x1, y0, y1, thr=110):
    n = 0
    for y in range(y0, y1):
        for x in range(x0, x1):
            o = y * stride + x * 4
            if raw[o] + raw[o + 1] + raw[o + 2] > thr * 3:
                n += 1
    return n


def search(hwnd, text):
    ed = u.GetDlgItem(hwnd, 102)
    u.SendMessageW(ed, WM_SETTEXT, 0, ctypes.c_wchar_p(text))
    time.sleep(0.15)
    u.PostMessageW(ed, WM_KEYDOWN, 0x0D, 0x000C001)


def wait_rows(hwnd, view, want, timeout, compare=lambda a, b: a >= b):
    """轮询到行数满足条件，返回 (耗时秒, 最终行数)。超时返回 (None, 最后行数)。"""
    t0 = time.time()
    last = -1
    while time.time() - t0 < timeout:
        n = row_count(hwnd, view)
        last = n
        if compare(n, want):
            return time.time() - t0, n
        time.sleep(0.25)
    return None, last


print("=" * 70)
print("整卷秒搜（Everything 式文件名索引）")
print(f"  素材 {BASE}")
print(f"  A 里匹配 {IN_A} 个，A+B+deep 共 {TOTAL} 个")

# ---------------------------------------------------------------- 1. 冷启动：无缓存
print("\n[1] 冷启动（删掉索引缓存，模拟第一次搜索）")
backup = None
if os.path.exists(CACHE):
    backup = CACHE + ".verifybak"
    shutil.move(CACHE, backup)
    print(f"  已备份缓存 -> {backup}")

hw = launch(A)
r = wt.RECT()
u.GetWindowRect(hw, ctypes.byref(r))
W, H = r.right - r.left, r.bottom - r.top
VIEW = (157, 74, W, H - 23)   # 侧栏 157 / 标题 34 + 工具条 40 / 状态栏 23
print(f"  窗口 {W}x{H}  view={VIEW}")

base_rows = row_count(hw, VIEW)
print(f"  未搜索时 A 目录 {base_rows} 行（应有 4：3 个 plain + 1 个 alpha）")
mark(base_rows == 4, f"基线：普通目录列表 {base_rows} 行")

t0 = time.time()
search(hw, "nexq7x")
dt, n = wait_rows(hw, VIEW, IN_A, 8.0)
print(f"  {(dt or -1):.2f}s 后 {n} 行（索引未就绪时应只出当前目录的 {IN_A} 个）")
mark(dt is not None and n == IN_A, "索引未就绪时先出当前目录结果（界面不空着，也没去递归全卷）")

dt2, n2 = wait_rows(hw, VIEW, TOTAL, 240.0)
print(f"  索引就绪：{dt2 if dt2 is None else round(dt2,1)}s 后 {n2} 行")
mark(dt2 is not None, f"索引建好后自动换成全卷结果（{n2} 行，跨出当前目录）")
mark(os.path.exists(CACHE), f"索引缓存已落盘 {CACHE}"
     + (f"（{os.path.getsize(CACHE)//1024//1024} MB）" if os.path.exists(CACHE) else ""))

# 位置列：搜索态下"位置"那一列必须有内容。
# 列几何：name = view.left+21+16+3 = 197，搜索态 name 宽 190 -> 位置列从 387 开始；
# 非搜索态 name 宽 274，下一列从 471 才开始，所以 392..470 这段只有搜索态才有字。
raw, stride, _, _ = grab(hw)
PX0, PY0, PX1, PY1 = 392, VIEW[1] + 26, 470, VIEW[1] + 68
path_ink = ink_in(raw, stride, PX0, PX1, PY0, PY1)
print(f"  搜索态 行内 x {PX0}..{PX1} 墨迹 {path_ink}")
mark(path_ink > 40, f"搜索结果带\"位置\"列（该列有 {path_ink} px 文字墨迹）")

# ---------------------------------------------------------------- 2. 热缓存：秒搜
print("\n[2] 热缓存（索引已在磁盘上）")
if backup and not os.path.exists(CACHE):
    shutil.move(backup, CACHE)
hw = launch(A)
VIEW = (157, 74, W, H - 23)
t0 = time.time()
search(hw, "nexq7x")
dt, n = wait_rows(hw, VIEW, TOTAL, 30.0)
print(f"  首次搜索（含载入索引）{(dt if dt else -1):.2f}s -> {n} 行")
mark(dt is not None and dt < 10.0, f"热缓存下首次搜索 {(dt or -1):.2f}s 出全卷结果")

# 第二次搜索：索引已在内存，纯子串匹配
search(hw, "nexq7x_beta")
dt2, n2 = wait_rows(hw, VIEW, 1, 10.0)
print(f"  第二次搜索 {dt2 if dt2 is None else round(dt2,2)}s -> {n2} 行")
mark(dt2 is not None and dt2 < 1.5, f"索引在内存时的搜索 {dt2 if dt2 else -1:.2f}s（秒搜）")

# 大小写不敏感
search(hw, "NEXQ7X")
dt3, n3 = wait_rows(hw, VIEW, TOTAL, 10.0)
print(f"  大写查询 -> {n3} 行")
mark(n3 == TOTAL, f"大小写不敏感（大写查出 {n3} 行，应 {TOTAL}）")

# 通配符
search(hw, "nexq7x_*.txt")
dt4, n4 = wait_rows(hw, VIEW, TOTAL, 10.0)
print(f"  通配符 nexq7x_*.txt -> {n4} 行")
mark(n4 == TOTAL, f"通配符可用（{n4} 行）")

# 搜不到 -> 只剩表头
search(hw, "zzzz_no_such_file_9z")
dt5, n5 = wait_rows(hw, VIEW, 0, 6.0, compare=lambda a, b: a <= b)
print(f"  无匹配查询 -> {n5} 行")
mark(n5 == 0, f"查不到时列表清空而不是留着旧结果（{n5} 行）")

# 清空 -> 回到目录列表
search(hw, "")
dt6, n6 = wait_rows(hw, VIEW, 4, 8.0)
print(f"  清空搜索框 -> {n6} 行")
mark(n6 == 4, f"清空搜索框回到目录列表（{n6} 行，应 4）")

# ---------------------------------------------------------------- 3. 非搜索态不该有位置列
print("\n[3] 非搜索态不应出现位置列")
search(hw, "")
time.sleep(1.0)
raw, stride, _, _ = grab(hw)
norm_ink = ink_in(raw, stride, PX0, PX1, PY0, PY1)
print(f"  非搜索态 同一段 x {PX0}..{PX1} 墨迹 {norm_ink}")
mark(norm_ink * 4 < path_ink, f"普通目录不画位置列（{norm_ink} vs 搜索态 {path_ink}）")

# ---------------------------------------------------------------- 收尾
subprocess.run(["taskkill", "/F", "/IM", "fm-app.exe"], capture_output=True)
if backup and os.path.exists(backup):
    if os.path.exists(CACHE):
        os.remove(CACHE)
    shutil.move(backup, CACHE)
    print(f"  已还原索引缓存")
print("\n" + "=" * 70)
print("整卷秒搜全部通过" if ok_all else "整卷秒搜存在失败项")
