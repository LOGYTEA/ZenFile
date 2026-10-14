"""第59轮回归：拖动框选不再误把第 0 行当焦点高亮（"莫名选中第一个文件/文件夹"）。

复现场景：
  Details 视图、文件夹里放 20 个文件；在内容区中下部「空白」处起手竖直拉框，
  框选一批中间行（明确不含第 0 行）。松手后：
    * 修复前：select_marquee_into 把 focus_visible 置 true、却不动 focus_row
              （默认 0）→ 第 0 行顶着一条 accent 竖线 + 浅高亮，看着像
              "莫名其妙选中了第一个文件/文件夹"。
    * 修复后：focus_row 落到被框到的顶行，第 0 行既不是选中也不是焦点，
              左侧不应再出现 accent 竖线，且 focus_row != 0。

判定（真窗口像素，双保险）：
  1) 先确认框选确实选中了一批行（内容区出现 >= 3 条 accent 竖条），否则判无效；
  2) 再看第 0 行：它的背景必须与"永不被选中的第 1 行"一致（即普通 window_bg），
     而不能是焦点高亮用的 hover_bg。修复前第 0 行 R=61（focus），修复后 R=32。

配置经 verify_env 隔离，退出还原。
"""
import os
import shutil
import subprocess
import sys
import time

import verify_common as V
import verify_env

verify_env.isolate()
verify_env.clean()

u = V.u
EXE = V.EXE
W, H = V.W_WIN, V.H_WIN


def set_view(mode):
    import json
    for nm in ("settings.json", "settings.user.json"):
        p = os.path.join(os.environ["APPDATA"], "ZenFile", nm)
        cfg = {}
        if os.path.exists(p):
            try:
                cfg = json.load(open(p, encoding="utf-8"))
            except Exception:
                cfg = {}
        cfg.setdefault("files", {})["view"] = {"default_mode": mode}
        cfg.setdefault("ui", {})["sidebar_visible"] = False
        os.makedirs(os.path.dirname(p), exist_ok=True)
        json.dump(cfg, open(p, "w"), ensure_ascii=False, indent=2)


def launch(folder):
    subprocess.run(["taskkill", "/F", "/IM", "ZenFile.exe"], capture_output=True)
    time.sleep(0.6)
    subprocess.Popen([EXE, "--new-process", folder])
    hw = None
    for _ in range(60):
        time.sleep(0.25)
        hw = u.FindWindowW("ZenFileMainWindow", None)
        if hw:
            break
    if not hw:
        raise SystemExit("主窗口没起来")
    u.SetWindowPos(hw, None, 60, 60, W, H, 0x0004)
    time.sleep(1.6)
    for _ in range(10):
        u.SetForegroundWindow(hw)
        time.sleep(0.15)
        fg = u.GetForegroundWindow()
        if int(fg) == int(hw):
            break
    time.sleep(0.6)
    return hw


def drag(x0, y0, x1, y1, steps=20):
    V.move(x0, y0)
    V._send([V.INPUT(0, V._U(mi=V.MOUSEINPUT(0, 0, 0, V.MOUSEEVENTF_LEFTDOWN, 0, None)))])
    time.sleep(0.08)
    for s in range(1, steps + 1):
        t = s / steps
        V.move(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t)
        time.sleep(0.03)
    V._send([V.INPUT(0, V._U(mi=V.MOUSEINPUT(0, 0, 0, V.MOUSEEVENTF_LEFTUP, 0, None)))])
    time.sleep(0.3)


def px(raw, stride, x, y):
    return V.px(raw, stride, x, y)


def bg_red_at(raw, stride, w, h, x, yc):
    """取 (x, yc) 附近几行像素的 R 通道中位数，用来判断该行背景色。"""
    xs = x if x < w else w - 1
    vals = []
    for dy in (-2, -1, 0, 1, 2):
        yy = yc + dy
        if 0 <= yy < h:
            p = px(raw, stride, xs, yy)
            vals.append(p[0])
    return sorted(vals)[len(vals) // 2] if vals else 0


def accent_bar_ys(raw, stride, w, h, y0=30, y1=None):
    y1 = y1 or (h - 50)

    def acc(p):
        return p[0] > 165 and 80 <= p[1] <= 150 and p[2] < 110

    rows = []
    for x in range(w):
        run = None
        for y in range(y0, y1):
            if acc(px(raw, stride, x, y)):
                if run is None:
                    run = y
            else:
                if run is not None:
                    if y - run >= 6:
                        rows.append((x, (run + y) // 2))
                    run = None
        if run is not None and (y1 - run) >= 6:
            rows.append((x, (run + y1) // 2))
    seen = []
    for x, yc in sorted(rows, key=lambda t: t[1]):
        if not seen or yc - seen[-1][1] > 12:
            seen.append((x, yc))
    return seen


ok_all = True


def mark(ok, msg):
    global ok_all
    ok_all = ok_all and ok
    print(f"    {'PASS' if ok else 'FAIL'}  {msg}")


def main():
    TEMP = os.environ.get("TEMP", r"C:\Windows\Temp")
    FOLDER = os.path.join(TEMP, "ZenR59_reg")
    if os.path.isdir(FOLDER):
        shutil.rmtree(FOLDER, ignore_errors=True)
    os.makedirs(FOLDER, exist_ok=True)
    for i in range(20):
        open(os.path.join(FOLDER, "file_%02d.txt" % i), "w").write("x")

    set_view("details")
    hw = launch(FOLDER)

    # 反复尝试直到框选真的选中了一批行（鼠标模拟偶尔会失焦导致只点中第 0 行）
    valid = False
    for attempt in range(4):
        u.SetForegroundWindow(hw)
        time.sleep(0.2)
        drag(600, 660, 600, 300, steps=20)
        time.sleep(0.2)
        raw, stride, w, h = V.grab(hw)
        bars = accent_bar_ys(raw, stride, w, h)
        ys = sorted(b[1] for b in bars)
        group = [b for b in ys if b > 120]  # 框选选中的行（远在第 0 行之下）
        print("    attempt %d: bars=%s group_len=%d" % (attempt, ys, len(group)))
        if len(group) >= 3:
            valid = True
            break
        # 失焦了：先在空白处点一下清掉选中，再重试
        V.move(600, 700)
        V._send([V.INPUT(0, V._U(mi=V.MOUSEINPUT(0, 0, 0, V.MOUSEEVENTF_LEFTDOWN, 0, None)))])
        time.sleep(0.05)
        V._send([V.INPUT(0, V._U(mi=V.MOUSEINPUT(0, 0, 0, V.MOUSEEVENTF_LEFTUP, 0, None)))])
        time.sleep(0.2)
    if not valid:
        print("    FAIL  框选未能稳定选中行（鼠标模拟失焦），无法判定")
        sys.exit(1)

    # ---- 像素判定：第 0 行背景应当是普通 window_bg，而非 hover_bg（焦点高亮）。
    # 焦点高亮的行用 hover_bg 填充，比普通行（window_bg）明显更亮；与紧邻的
    # 第 1 行（本场景永不选中/焦点）做差即可稳健判定，不依赖具体主题色值。
    # content_top(客户端)≈100、行高 21 → 第 0 行中心≈110、第 1 行中心≈131。
    r0 = bg_red_at(raw, stride, w, h, 100, 110)
    r1 = bg_red_at(raw, stride, w, h, 100, 131)
    print("    第0行背景R=%d  第1行背景R=%d  差值=%d" % (r0, r1, r0 - r1))
    mark(r0 - r1 <= 6, "第 0 行没有无辜焦点高亮（背景与第 1 行一致，差值=%d<=6）" % (r0 - r1))

    # 框选确实选中了一批行（确认上面的判定建立在一个有效框选之上）
    mark(len(group) >= 3, "框选确实选中了一批行（组行数=%d）" % len(group))

    print("    RESULT:", "PASS" if ok_all else "FAIL")
    sys.exit(0 if ok_all else 1)


if __name__ == "__main__":
    main()
