#!/usr/bin/env python
# -*- coding: utf-8 -*-
"""第53轮 · 鼠标指针判据脚本

用户报："一个鼠标指针图标经常变成死的问题修了，经常不自动更新正确的鼠标指针"。

判据读**系统当前光标句柄**（GetCursorInfo），不看画面：
  C1 悬停分隔线              -> IDC_SIZEWE
  C2 悬停内容区              -> IDC_ARROW
  C3 悬停地址栏/搜索框        -> IDC_IBEAM
  C4 拖完分隔线松手后再动一次  -> 必须已是 ARROW（旧代码慢一拍，仍是 SIZEWE）
  C5 上面那次之后原地不动 1s   -> 仍是 ARROW
  C6 悬停分隔线→移出窗口→移回  -> 回到内容区必须是 ARROW（旧代码慢一拍）
  C7 菜单里                  -> ARROW（不会继承打开菜单那一刻的形状）
  C8 ESC 关掉菜单            -> ARROW

C4/C6 的"慢一拍"才是"指针变死"的机理：WM_SETCURSOR 比 WM_MOUSEMOVE **先**到，
旧代码在 WM_SETCURSOR 里读的是上一次 move 存下的 hover_split 快照；鼠标停在
原地不动就再没有下一次 WM_SETCURSOR，光标于是永久停在旧形状上。

用法：/c/ProgramData/anaconda3/python.exe verify_cursor.py
"""
import ctypes
import ctypes.wintypes as wt
import json
import os
import subprocess
import sys
import time

u = ctypes.windll.user32

EXE = r"C:\Users\Administrator\Desktop\project\File\workspace\target\release\ZenFile.exe"
SETTINGS = os.path.join(os.environ["APPDATA"], "ZenFile", "settings.json")
DEFAULT_SIDEBAR = 157


class POINT(ctypes.Structure):
    _fields_ = [("x", wt.LONG), ("y", wt.LONG)]


class CURSORINFO(ctypes.Structure):
    _fields_ = [("cbSize", wt.DWORD), ("flags", wt.DWORD),
                ("hCursor", wt.HANDLE), ("ptScreenPos", POINT)]


class RECT(ctypes.Structure):
    _fields_ = [("l", wt.LONG), ("t", wt.LONG), ("r", wt.LONG), ("b", wt.LONG)]


def cur():
    ci = CURSORINFO()
    ci.cbSize = ctypes.sizeof(ci)
    u.GetCursorInfo(ctypes.byref(ci))
    return ci.hCursor


u.LoadCursorW.restype = wt.HANDLE
u.LoadCursorW.argtypes = [wt.HANDLE, ctypes.c_wchar_p]
ARROW = u.LoadCursorW(None, ctypes.c_wchar_p(32512))
IBEAM = u.LoadCursorW(None, ctypes.c_wchar_p(32513))
SIZEWE = u.LoadCursorW(None, ctypes.c_wchar_p(32644))
SIZENS = u.LoadCursorW(None, ctypes.c_wchar_p(32645))
NAMES = {ARROW: "ARROW", IBEAM: "IBEAM", SIZEWE: "SIZEWE", SIZENS: "SIZENS"}
nm = lambda h: NAMES.get(h, "other(%s)" % h)

res = []


def check(tag, ok, detail=""):
    res.append(ok)
    print("%-4s %-26s %s" % ("PASS" if ok else "FAIL", tag, detail))


def move(x, y, s=0.10):
    u.SetCursorPos(int(x), int(y))
    time.sleep(s)


def restore_sidebar():
    try:
        d = json.load(open(SETTINGS, encoding="utf-8"))
        if d.get("ui", {}).get("sidebar_width") != DEFAULT_SIDEBAR:
            d.setdefault("ui", {})["sidebar_width"] = DEFAULT_SIDEBAR
            open(SETTINGS, "w", encoding="utf-8").write(
                json.dumps(d, ensure_ascii=False, indent=2))
            print("[restore] sidebar_width -> %d" % DEFAULT_SIDEBAR)
    except Exception as e:
        print("[restore] skip:", e)


def main():
    subprocess.Popen([EXE], cwd=os.path.dirname(EXE))
    hwnd = 0
    for _ in range(40):
        time.sleep(0.25)
        hwnd = u.FindWindowW("ZenFileMainWindow", None)
        if hwnd:
            break
    if not hwnd:
        print("FATAL: 主窗口没起来")
        return 1
    time.sleep(1.5)
    r = RECT()
    u.GetWindowRect(hwnd, ctypes.byref(r))
    L, T, R, B = r.l, r.t, r.r, r.b
    print("window rect =", L, T, R, B)
    u.SetWindowPos(hwnd, -1, L, T, R - L, B - T, 0x0004 | 0x0040)
    time.sleep(0.5)
    y0 = T + int((B - T) * 0.6)

    # ---- 找侧栏分隔线
    split_x = None
    for x in range(L + 40, L + 460, 3):
        move(x, y0, 0.06)
        if cur() == SIZEWE:
            split_x = x
            break
    check("C0 找到侧栏分隔线", split_x is not None, "x=%s" % split_x)
    if split_x is None:
        u.PostMessageW(hwnd, 0x0010, 0, 0)
        return 1

    move(split_x, y0)
    check("C1 悬停分隔线=SIZEWE", cur() == SIZEWE, nm(cur()))

    row_x = split_x + 240
    move(row_x, y0)
    check("C2 悬停内容区=ARROW", cur() == ARROW, "x=%d %s" % (row_x, nm(cur())))

    got = False
    for x in range(L + 120, min(R - 60, L + 800), 8):
        move(x, T + 34 + 20, 0.06)
        if cur() == IBEAM:
            got = True
            break
    check("C3 地址栏/搜索框=IBEAM", got, "")

    # ---- C4：拖分隔线 → 松手 → **只动一次**就读（旧代码这一步还是 SIZEWE）
    move(split_x, y0)
    u.mouse_event(0x0002, 0, 0, 0, 0)
    time.sleep(0.08)
    for x in range(split_x, split_x + 200, 25):
        move(x, y0, 0.05)
    drop_x = split_x + 200
    move(drop_x, y0)
    u.mouse_event(0x0004, 0, 0, 0, 0)
    time.sleep(0.25)
    move(drop_x + 170, y0)          # 松手之后**第一次**移动
    c = cur()
    check("C4 拖完松手后动一次=ARROW", c == ARROW, nm(c))
    time.sleep(1.0)
    c = cur()
    check("C5 原地不动 1s 仍 ARROW", c == ARROW, nm(c))

    # ---- C6：悬停分隔线 → 移出窗口 → 移回内容区（只读第一次回来那一下）
    split2 = None
    for x in range(L + 40, L + 700, 3):
        move(x, y0, 0.06)
        if cur() == SIZEWE:
            split2 = x
            break
    if split2 is not None:
        move(max(L - 220, 5), y0, 0.25)     # 移出窗口左边
        move(split2 + 260, y0)              # 移回内容区，只读这一下
        c = cur()
        check("C6 移出窗口再移回=ARROW", c == ARROW, "split2=%d %s" % (split2, nm(c)))
    else:
        check("C6 移出窗口再移回=ARROW", False, "没找到分隔线")

    # ---- C7/C8：菜单
    px, py = min(R - 260, L + 900), y0
    move(px, py)
    u.mouse_event(0x0008, 0, 0, 0, 0)
    u.mouse_event(0x0010, 0, 0, 0, 0)
    time.sleep(0.8)
    mh = u.FindWindowW("ZenFileMenu", None)
    move(px + 30, py + 40)
    c = cur()
    check("C7 菜单里=ARROW", c == ARROW, "menu=%s %s" % (mh, nm(c)))
    u.keybd_event(0x1B, 0, 0, 0)
    u.keybd_event(0x1B, 0, 2, 0)
    time.sleep(0.8)
    c = cur()
    check("C8 菜单关闭后=ARROW", c == ARROW, nm(c))

    ok = sum(res)
    print("\n==== %d/%d PASS ====" % (ok, len(res)))
    u.PostMessageW(hwnd, 0x0010, 0, 0)
    time.sleep(0.6)
    restore_sidebar()
    return 0 if ok == len(res) else 2


if __name__ == "__main__":
    sys.exit(main())
