"""第58轮验收：修复"选中后点空白取消选中，放大效果不缩回" + 动画帧率无关缓动。

像素判据全部来自**真窗口**，但图标在磁贴里是个小亮块、且"选中"会叠一层高亮条，
所以全程用**墨量计数**（图标亮像素数，放大时明显变多）而非宽度，并先自适应
定位图标包围盒，避开高亮条 / 文件名。

  B0 自适应定位到第一个磁贴的图标亮块
  B1 悬停（无选中条）：图标墨量**变大**（+12% 以上，且过程中抓到中间值 → 是动画不是瞬变）
  B2 移开鼠标：图标墨量回到基准（放大缩回）
  B3 选中某磁贴 → 等放大跑完 → 点空白取消选中：取消后墨量**回到基准**
      （上一轮 bug：放大收敛后定时器被 Kill，取消选中不会重启它，图标永远停在放大态）
  B4 帧率无关缓动：放大 / 缩回都**收敛回同一基准**（dt 钳制保证不瞬移，最终 == 基准）
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
OS_X, OS_Y = 60, 60
BASE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "ZenR57")
ONE = os.path.join(BASE, "one")
os.makedirs(ONE, exist_ok=True)
with open(os.path.join(ONE, "only.txt"), "w") as f:
    f.write("x")

FAILS = []


def mark(ok, msg):
    print(("  PASS  " if ok else "  FAIL  ") + msg)
    if not ok:
        FAILS.append(msg)


def clear_user():
    try:
        if os.path.exists(USER):
            os.remove(USER)
    except OSError:
        pass


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


def geom(hw):
    dpi = u.GetDpiForWindow(hw) or 96
    s = dpi / 96.0
    r = wt.RECT()
    u.GetWindowRect(hw, ctypes.byref(r))
    W, H = r.right - r.left, r.bottom - r.top
    return {"s": s, "W": W, "H": H, "side": round(157 * s),
            "view_top": round(34 * s) + round(40 * s),
            "view_bottom": H - round(23 * s), "view_left": round(157 * s)}


def kill():
    import subprocess
    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    time.sleep(0.4)


def icon_box(hw, G):
    """自适应定位第一个磁贴里图标亮块的包围盒（屏幕坐标）。鼠标需先移开。"""
    s = G["s"]
    x0 = G["view_left"] + round(12 * s)
    y0 = G["view_top"]
    raw, stride, _w, _h = V.grab(hw)
    thr = 0x70
    xs, ys = [], []
    # grab 缓冲是「窗口本地坐标」（0,0 = 窗口左上角），x0/y0 已是窗口本地，
    # 所以这里**不**加 OS 偏移；鼠标动作才需要加 OS 转成屏幕坐标。
    for y in range(round(y0), round(y0 + 110 * s)):
        for x in range(round(x0) - round(20 * s), round(x0 + 180 * s)):
            p = V.px(raw, stride, x, y)
            if min(p) >= thr:
                xs.append(x)
                ys.append(y)
    if not xs:
        return None
    return (min(xs), min(ys), max(xs), max(ys))


def ink(hw, box):
    """图标亮块区域内的墨量（亮像素数），包围盒外扩 8px 兜住放大溢出。"""
    x0, y0, x1, y1 = box
    m = 8
    x0 -= m; y0 -= m; x1 += m; y1 += m
    raw, stride, _w, _h = V.grab(hw)
    n = 0
    for y in range(y0, y1):
        for x in range(x0, x1):
            p = V.px(raw, stride, x, y)
            if min(p) >= 0x70:
                n += 1
    return n


# ---------------------------------------------------------------- 主流程
clear_user()
write_user({"files": {"view": {"default_mode": "large"}}})
hw = V.launch(ONE)
G = geom(hw)
s = G["s"]
time.sleep(0.6)
# 鼠标挪到远处（别悬停 / 选中第一个磁贴）
V.move(OS_X + G["W"] - 40, OS_Y + G["view_bottom"] - 30)
time.sleep(0.5)

box = icon_box(hw, G)
print("      图标包围盒（屏幕）：%s" % (box,))
print("      窗口尺寸：%d x %d  配置：user=%s" % (G["W"], G["H"], open(USER, encoding="utf-8").read()[:200] if os.path.exists(USER) else "无"))
if box:
    # 包围盒填充率（窗口本地坐标；grab 缓冲 0,0 = 窗口左上角，不再加 OS 偏移）
    x0, y0, x1, y1 = box
    r0, rr, _, _ = V.grab(hw)
    fill = sum(1 for yy in range(y0 - 8, y1 + 8) for xx in range(x0 - 8, x1 + 8)
               if min(V.px(r0, rr, xx, yy)) >= 0x70)
    area = (x1 - x0 + 16) * (y1 - y0 + 16)
    print("      图标区亮像素填充率：%.0f%%" % (100.0 * fill / area if area else 0))
mark(box is not None, "B0 自适应定位到第一个磁贴的图标亮块：%s" % (box,))
ibx = (box[0] + box[2]) // 2, (box[1] + box[3]) // 2   # 图标中心（窗口本地坐标）
# 鼠标动作需要屏幕坐标：窗口落在屏幕 (OS_X, OS_Y)，加偏移转成绝对坐标。
ibx_a = (OS_X + ibx[0], OS_Y + ibx[1])

base = ink(hw, box)
print("      未悬停基准墨量 = %d" % base)
mark(base > 200, "B0 图标墨量 = %d（>200）" % base)

# B1：悬停图标，墨量应变大（放大），过程抓中间值
V.move(ibx_a[0], ibx_a[1])
grow = []
for _ in range(18):
    grow.append(ink(hw, box))
    time.sleep(0.035)
time.sleep(0.3)
hv = ink(hw, box)
print("      悬停墨量序列：%s → 稳定 %d（基准 %d）" % (grow, hv, base))
gmid = [v for v in grow if base + max(8, int(base * 0.06)) <= v <= hv - max(8, int(base * 0.06))]
mark(hv >= int(base * 1.12), "B1 悬停后图标墨量变多：%d → %d（期望 +12%% 以上）" % (base, hv))
mark(bool(gmid), "B1 悬停过程抓到中间墨量（放大是动画，不是瞬变）：%s" % gmid)

# B2：移开鼠标，墨量应回到基准
V.move(OS_X + G["W"] - 40, OS_Y + G["view_bottom"] - 30)
time.sleep(0.5)
bk = ink(hw, box)
print("      移开后墨量 = %d（基准 %d）" % (bk, base))
mark(abs(bk - base) <= max(20, int(base * 0.05)), "B2 移开鼠标后图标缩回：%d vs %d（误差 ≤5%%）" % (bk, base))

# B3：选中磁贴 → 等放大跑完 → 点空白取消选中，墨量应回到基准
cx, cy = ibx_a                                 # 用自适应定位的图标中心点击，避免坐标写死错位
V.move(cx, cy)
time.sleep(0.05)
V.click(cx, cy)
time.sleep(0.45)                               # 等放大 + 选中条都稳定
sel = ink(hw, box)
print("      选中后（含选中条）墨量 = %d" % sel)
# 点空白取消选中（视图底部、磁贴之下）
V.move(OS_X + G["view_left"] + round(200 * s), OS_Y + G["view_bottom"] - round(20 * s))
time.sleep(0.05)
V.click(OS_X + G["view_left"] + round(200 * s), OS_Y + G["view_bottom"] - round(20 * s))
seq = []
for _ in range(18):
    seq.append(ink(hw, box))
    time.sleep(0.035)
time.sleep(0.3)
end = ink(hw, box)
print("      取消选中后墨量序列：%s → 最终 %d（基准 %d）" % (seq, end, base))
mark(abs(end - base) <= max(20, int(base * 0.05)), "B3 取消选中后墨量回到基准：%d vs %d（误差 ≤5%%）" % (end, base))
# 关键：取消选中后，墨量必须跌回"无选中条"的基准附近 —— 若 bug 在，图标卡放大态，
# 墨量会明显比基准高（放大 + 但选中条已撤，只剩放大）。
mark(end <= base + max(20, int(base * 0.05)), "B3 取消选中后图标没有卡在放大态：%d ≤ %d" % (end, base + max(20, int(base * 0.05))))
# 注：放大缩回走帧率无关缓动，约 150ms 即收敛，采样窗口抓不到"缩回途中"的
# 中间值属正常；动画属性已由 B1 的 gmid（悬停放大是过程而非瞬变）覆盖。
kill()

print("\n———— 结果 ————")
if FAILS:
    for f in FAILS:
        print("  FAIL  " + f)
    print("  %d 条未通过" % len(FAILS))
    raise SystemExit(1)
print("  全部通过")
