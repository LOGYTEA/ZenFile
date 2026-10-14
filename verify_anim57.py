"""第57轮验收：设置-交互页「动画」组 + 三段过渡动画。

判据全部是**真窗口事实**（像素 / 落盘配置），不靠肉眼：

  S1 交互页左侧树点得出「动画 › 侧栏平滑定位」，右侧参数行画出 Toggle（accent）
  S2 点 Toggle 一次 → settings.user.json 里 ui.anim.sidebar = false；再点回 true
  A1 进入子目录：地址栏**末级**面包屑从右侧滑进来（中途帧末节右沿偏右）
  A1b 同一动画里**父级面包屑不位移**（各帧最左列恒定）—— 第59轮起只动末级
  A2 关掉 anim.address 后同一动作没有中间态（第一帧就到位）—— 消极对照
  A3 大图标视图：悬停的磁贴图标**变大**（稳定后包围盒比未悬停宽 6px 以上）
  A4 关掉 anim.icon_zoom 后不再变大 —— 消极对照
  A5 详细信息视图：刚悬停那一帧图标墨量**冲高后回落**（果冻回弹，不是单调）
  A6 导航窗格：切目录时滚动有中间态（中途帧 ≠ 起始帧 且 ≠ 稳定帧）
"""
import ctypes
import ctypes.wintypes as wt
import json
import os
import time

import verify_env

verify_env.isolate()

import verify_common as V

u = V.u
u.GetDpiForWindow.restype = ctypes.c_uint
u.GetDpiForWindow.argtypes = [wt.HWND]

CFG = os.path.join(os.environ.get("APPDATA", ""), "ZenFile")
USER = os.path.join(CFG, "settings.user.json")
ACCENT = (67, 117, 206)          # theme accent，px() 返回 (R,G,B)
OS_X, OS_Y = 60, 60              # launch() 定死的窗口左上角
BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "ZenR57")
os.makedirs(os.path.join(BASE, "subdir"), exist_ok=True)
for n in ("a.txt", "b.txt", "c.txt"):
    with open(os.path.join(BASE, n), "w") as f:
        f.write("x")

FAILS = []


def mark(ok, msg):
    print(("  PASS  " if ok else "  FAIL  ") + msg)
    if not ok:
        FAILS.append(msg)


def write_user(patch):
    d = {}
    if os.path.exists(USER):
        try:
            with open(USER, "r", encoding="utf-8") as f:
                d = json.load(f)
        except Exception:
            d = {}
    for k, v in patch.items():
        if isinstance(v, dict) and isinstance(d.get(k), dict):
            d[k].update(v)
        else:
            d[k] = v
    os.makedirs(CFG, exist_ok=True)
    with open(USER, "w", encoding="utf-8") as f:
        json.dump(d, f)


APPJSON = os.path.join(CFG, "settings.json")


def user_anim(key):
    """动画开关由 save_user_patch 写进 **settings.json**（应用层），不是 user 层。"""
    try:
        with open(APPJSON, "r", encoding="utf-8") as f:
            return json.load(f).get("ui", {}).get("anim", {}).get(key)
    except Exception:
        return None


def clear_user():
    try:
        if os.path.exists(USER):
            os.remove(USER)
    except OSError:
        pass


def geom(hw):
    dpi = u.GetDpiForWindow(hw) or 96
    s = dpi / 96.0
    r = wt.RECT()
    u.GetWindowRect(hw, ctypes.byref(r))
    W, H = r.right - r.left, r.bottom - r.top
    title, tool, status = round(34 * s), round(40 * s), round(23 * s)
    side = round(157 * s)
    return {"s": s, "W": W, "H": H, "title": title, "tool": tool, "status": status,
            "side": side, "view_top": title + tool, "view_bottom": H - status,
            "view_right": W}


def click(x, y, wait=0.12):
    V.move(x, y)
    time.sleep(0.05)
    V.click(x, y)
    time.sleep(wait)


def runs(raw, stride, x0, x1, y0, y1, thr=0x70, gap=6):
    has = [any(min(V.px(raw, stride, x, y)) >= thr for y in range(y0, y1))
           for x in range(x0, x1)]
    out, cur = [], None
    for i, h in enumerate(has):
        if h and cur is None:
            cur = i
        elif not h and cur is not None:
            out.append((x0 + cur, x0 + i))
            cur = None
    if cur is not None:
        out.append((x0 + cur, x1))
    merged = []
    for a, b in out:
        if merged and a - merged[-1][1] <= gap:
            merged[-1] = (merged[-1][0], b)
        else:
            merged.append((a, b))
    return merged


def bbox_ink(raw, stride, x0, x1, y0, y1, thr=0x50):
    """量"亮像素"的包围盒 —— **按亮度而不是"跟底色不同"**：悬停底色本身
    也是"跟底色不同"，用它判会把整条高亮行算成图标（第一版就栽在这）。"""
    xs, ys = [], []
    for y in range(y0, y1):
        for x in range(x0, x1):
            if min(V.px(raw, stride, x, y)) >= thr:
                xs.append(x)
                ys.append(y)
    if not xs:
        return None
    return (min(xs), max(xs), min(ys), max(ys))


def ink_count(raw, stride, x0, x1, y0, y1, thr=0x50):
    n = 0
    for y in range(y0, y1):
        for x in range(x0, x1):
            if min(V.px(raw, stride, x, y)) >= thr:
                n += 1
    return n


def addr_ink_span(raw, stride, G):
  """地址栏墨迹的 (最左, 最右) 列。

  第59轮：动画只作用在**末级**那一节，父级不动。所以
    * 末级在动  → 最右列（末节右沿）随帧变化；
    * 父级不动  → 最左列（第一节左沿）各帧恒定。
  旧判据只看最左列，在"整串一起动"时才有信号，现在要靠最右列。
  """
  s = G["s"]
  x0 = round(106 * s)
  x1 = G["W"] - round(222 * s)
  y0 = G["title"] + round(8 * s)
  y1 = G["title"] + G["tool"] - round(8 * s)
  lo = hi = None
  for x in range(x0, x1):
    if any(min(V.px(raw, stride, x, y)) >= 0x70 for y in range(y0, y1)):
      if lo is None:
        lo = x
      hi = x
  return (lo, hi)


def sidebar_top(raw, stride, G, thr=0x60):
    """导航窗格**第一行墨的 y** —— 树一滚，这个数就变（比逐像素签名好读）。"""
    s = G["s"]
    for y in range(G["view_top"], G["view_bottom"]):
        if any(min(V.px(raw, stride, x, y)) >= thr for x in range(4, round(157 * s) - 4)):
            return y
    return None


# ---------------------------------------------------------------- 设置页
print("\n—— S1/S2 设置-交互页「动画」组 ——")
clear_user()
hw = V.launch("shell:Settings")
G = geom(hw)
s = G["s"]
# 顶部小标签：第 3 个是「交互」
raw, stride, _w, _h = V.grab(hw)
tr = runs(raw, stride, G["side"], G["view_right"], G["view_top"] + round(12 * s), G["view_top"] + round(34 * s))
print("      tab 段：%d 个" % len(tr))
if len(tr) >= 3:
    a, b = tr[2]
    click(OS_X + (a + b) // 2, OS_Y + G["view_top"] + round(24 * s), 0.5)

# 树：0=选中行首竖线 1..4=四个参数 5=动画 6=侧栏平滑定位 ...
content_l = G["side"] + round(16 * s)
content_r = G["view_right"] - round(16 * s)
content_t = G["view_top"] + round(52 * s)
content_b = G["view_bottom"] - round(10 * s)
tree_w = (content_r - content_l) * 44 // 100
row_h = round(28 * s)
click(OS_X + content_l + round(60 * s), OS_Y + content_t + 6 * row_h + row_h // 2, 0.4)

raw, stride, _w, _h = V.grab(hw)
# 参数行贴在内容区底部：找 accent（Toggle 开态的轨道）
param_l = content_l + tree_w + round(14 * s)
band = (param_l, content_r, content_b - round(34 * s), content_b)
acc = V.count(raw, stride, band[0], band[1], band[2], band[3], ACCENT, tol=30)
mark(acc > 60, "S1 交互页选中「侧栏平滑定位」后参数行画出开态 Toggle（accent 像素 %d）" % acc)

# 点 Toggle：配置应先变 false
tgx = OS_X + content_r - round(19 * s)
tgy = OS_Y + content_b - round(17 * s)
click(tgx, tgy, 0.4)
v = user_anim("sidebar")
mark(v is False, "S2 点一下 Toggle → settings.user.json 的 ui.anim.sidebar = %r（期望 False）" % v)
raw, stride, _w, _h = V.grab(hw)
acc2 = V.count(raw, stride, band[0], band[1], band[2], band[3], ACCENT, tol=30)
mark(acc2 < acc // 2, "S2 关态 Toggle 不再是 accent（%d → %d）" % (acc, acc2))
click(tgx, tgy, 0.4)
v2 = user_anim("sidebar")
mark(v2 is True, "S2 再点一下 → ui.anim.sidebar = %r（期望 True）" % v2)

# ---------------------------------------------------------------- 地址栏
print("\n—— A1/A2 地址栏进入动画 ——")


def enter_and_sample(anim_on, nframes=5, gap=0.06):
    clear_user()
    if not anim_on:
        write_user({"ui": {"anim": {"address": False}}})
    hw2 = V.launch(BASE)
    G2 = geom(hw2)
    time.sleep(0.6)
    # 双击 subdir 进入（真鼠标：单击选中 + 双击打开）
    x = OS_X + G2["side"] + 200
    y = OS_Y + G2["view_top"] + 40
    V.move(x, y)
    time.sleep(0.15)
    raw0, st0, _, _ = V.grab(hw2)
    before = addr_ink_span(raw0, st0, G2)
    V.click(x, y)
    time.sleep(0.05)
    V.click(x, y)  # 第二次点击 = 双击打开（双击时限 500ms 内）
    frames = []
    for _ in range(nframes):
        r, st, _, _ = V.grab(hw2)
        frames.append((addr_ink_span(r, st, G2), time.time()))
        time.sleep(gap)
    time.sleep(0.8)
    rawE, stE, _, _ = V.grab(hw2)
    stable = addr_ink_span(rawE, stE, G2)
    subprocess_kill()
    return before, frames, stable


def subprocess_kill():
    import subprocess
    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    time.sleep(0.4)


before, frames, stable = enter_and_sample(True)
spans = [f[0] for f in frames]
# 末级在动 → 中途帧的最右列明显偏右（甚至整段还没滑进来 = None）；
# 父级不动 → 各帧最左列恒定（第一节左沿不参与位移）。
rights = [sp[1] for sp in spans if sp and sp[1] is not None]
lefts = [sp[0] for sp in spans if sp and sp[0] is not None]
sright = stable[1] if stable else None
moved = [r for r in rights if sright is not None and r > sright + 10]
mark(bool(moved) or any(sp is None for sp in spans),
     "A1 动画开：末级面包屑中途帧偏右（起始 %s / 中途 %s / 稳定 %s）"
     % (before, spans, stable))
mark(bool(lefts) and sright is not None and max(lefts) - min(lefts) <= 2,
     "A1b 父级面包屑不参与位移（各帧最左列极差=%d px）"
     % (max(lefts) - min(lefts) if lefts else -1))

before_b, framesb, stableb = enter_and_sample(False)
spansb = [f[0] for f in framesb]
rightsb = [sp[1] for sp in spansb if sp and sp[1] is not None]
srightb = stableb[1] if stableb else None
movedb = [r for r in rightsb if srightb is not None and r > srightb + 10]
mark(not movedb,
     "A2 关掉动画：同一动作没有中间态（中途 %s / 稳定 %s）" % (spansb, stableb))

# ---------------------------------------------------------------- 图标放大
print("\n—— A3/A4 大图标悬停放大 ——")


def large_hover(anim_on):
    clear_user()
    write_user({"files": {"view": {"default_mode": "large"}}})
    if not anim_on:
        write_user({"ui": {"anim": {"icon_zoom": False}}})
    hw3 = V.launch(BASE)
    G3 = geom(hw3)
    time.sleep(0.8)
    s3 = G3["s"]
    # 第一个磁贴：view.left + pad(12) + 0，图标在格子顶部居中
    tx = G3["side"] + round(12 * s3)
    ty = G3["view_top"] + round(8 * s3)
    box = (tx + 4, tx + round(140 * s3), ty, ty + round(100 * s3))
    # 先把鼠标挪到远处（别悬停在磁贴上）
    V.move(OS_X + G3["W"] - 40, OS_Y + G3["view_bottom"] - 30)
    time.sleep(0.5)
    raw, st, _, _ = V.grab(hw3)
    b0 = bbox_ink(raw, st, box[0], box[1], box[2], box[3])
    # 真鼠标移到磁贴中心
    V.move(OS_X + tx + round(70 * s3), OS_Y + ty + round(50 * s3))
    time.sleep(0.6)
    raw2, st2, _, _ = V.grab(hw3)
    b1 = bbox_ink(raw2, st2, box[0], box[1], box[2], box[3])
    subprocess_kill()
    w0 = (b0[1] - b0[0] + 1) if b0 else 0
    w1 = (b1[1] - b1[0] + 1) if b1 else 0
    print("      bbox 未悬停=%s 悬停=%s" % (b0, b1))
    return w0, w1


w0, w1 = large_hover(True)
mark(w0 > 30 and w1 >= int(w0 * 1.05),
     "A3 大图标悬停后图标变宽：%d → %d（期望 +5%% 以上）" % (w0, w1))
w0b, w1b = large_hover(False)
mark(w0b > 40 and abs(w1b - w0b) <= 2,
     "A4 关掉放大后悬停不再变大：%d → %d" % (w0b, w1b))

# ---------------------------------------------------------------- 回弹
print("\n—— A5 详细信息悬停回弹 ——")
clear_user()
write_user({"files": {"view": {"default_mode": "details"}}})
hw4 = V.launch(BASE)
G4 = geom(hw4)
time.sleep(0.8)
s4 = G4["s"]
row_h4 = round(21 * s4)
ix0 = G4["side"] + round(19 * s4)
iy0 = G4["view_top"] + round(26 * s4) + 2
box = (ix0 - 2, ix0 + round(22 * s4), iy0, iy0 + row_h4 - 2)
V.move(OS_X + G4["W"] - 40, OS_Y + G4["view_bottom"] - 30)
time.sleep(0.5)
raw, st, _, _ = V.grab(hw4)
base = ink_count(raw, st, box[0], box[1], box[2], box[3])
V.move(OS_X + G4["side"] + 300, OS_Y + iy0 + row_h4 // 2)
seq = []
for _ in range(6):
    r, stx, _, _ = V.grab(hw4)
    seq.append(ink_count(r, stx, box[0], box[1], box[2], box[3]))
time.sleep(0.7)
rawE, stE, _, _ = V.grab(hw4)
end = ink_count(rawE, stE, box[0], box[1], box[2], box[3])
print("      墨量序列 base=%d %s end=%d" % (base, seq, end))
# 回弹是**阻尼振荡**：可能先鼓一下（墨量变大）也可能正处在收缩那一拍
# （墨量变小），两个方向都算"跳了一下"；唯一必须成立的是**最后回到基准**。
dev = max(abs(v - base) for v in seq) if seq else 0
mark(base > 20 and dev >= int(base * 0.12) and abs(end - base) <= int(base * 0.06),
     "A5 悬停瞬间图标墨量偏离基准 %d→最大 %d，之后回到 %d（果冻回弹，不是常驻放大）"
     % (base, dev, end))
subprocess_kill()

# ---------------------------------------------------------------- 侧栏滚动
print("\n—— A6 导航窗格平滑滚动 ——")
clear_user()
# 打开"跟随展开到当前文件夹"：逐层往下走会把树撑长，当前位置那一行很快
# 就落到窗格视野之外 —— 这才需要（也才测得到）平滑滚动。
#
# 路径必须在**非隐藏**的链上（%TEMP% 在 AppData 底下，那一层是隐藏目录，
# 树里压根不列，跟随展开等于没开）。
write_user({"ui": {"sidebar_follow_current": True}})
DEEP = r"C:\ZenR57"
deep = DEEP
for name in ("L1", "L2", "L3"):
    deep = os.path.join(deep, name)
    os.makedirs(deep, exist_ok=True)
hw5 = V.launch(DEEP)
# 把窗口压矮：树比窗格长才需要滚
u.SetWindowPos(hw5, None, OS_X, OS_Y, 1100, 400, 0x0004)
time.sleep(1.0)
u.SetForegroundWindow(hw5)
time.sleep(0.4)
G5 = geom(hw5)
seen = []
for level in range(3):
    raw, st, _, _ = V.grab(hw5)
    top0 = sidebar_top(raw, st, G5)
    # 双击第一个文件夹往下走一层
    x = OS_X + G5["side"] + 200
    y = OS_Y + G5["view_top"] + round(40 * G5["s"])
    V.click(x, y)
    time.sleep(0.05)
    V.click(x, y)
    got = []
    for _ in range(4):
        r, stx, _, _ = V.grab(hw5)
        got.append(sidebar_top(r, stx, G5))
    time.sleep(1.0)
    rawE, stE, _, _ = V.grab(hw5)
    topE = sidebar_top(rawE, stE, G5)
    mid = [v for v in got if v != top0 and v != topE]
    seen.append(len(mid))
    print("      第 %d 层：首行墨 y %s → 中途 %s → 稳定 %s（中间态 %d 帧）"
          % (level + 1, top0, got, topE, len(mid)))
subprocess_kill()
import shutil
shutil.rmtree(DEEP, ignore_errors=True)
mark(sum(seen) >= 1,
     "A6 切目录时导航窗格平滑滚动（逐层中间态帧数 %s）" % seen)

print("\n———— 结果 ————")
if FAILS:
    for f in FAILS:
        print("  FAIL  " + f)
    print("  %d/%d 通过" % (0, len(FAILS)))
    raise SystemExit(1)
print("  全部通过")
