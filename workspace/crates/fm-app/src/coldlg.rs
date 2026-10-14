//! 「选择详细信息」对话框（表头右键 → 其他(M)...）。
//!
//! 资源管理器同款：列出全部可选列，勾上=显示，勾掉=隐藏，右侧两个按钮调
//! 顺序，确定/取消收尾。**每文件夹各一套**（与 `App::apply_visible_cols`
//! 一致）。
//!
//! 为什么自己画而不用原生对话框：
//!   * 软件整窗是暗色自绘，原生对话框是白底系统控件，跳出来格格不入；
//!   * 原生 ListView + LVS_EX_CHECKBOXES 的行高/勾选框在暗色下没法配色。
//! 做法照抄 `progress.rs`：WS_POPUP 自绘 + 双缓冲，标题栏在 WM_NCHITTEST
//! 里返回 HTCAPTION（系统因此负责拖窗口）。

use std::cell::Cell;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreatePen, CreateSolidBrush, DeleteDC,
    DeleteObject, EndPaint, FillRect, InvalidateRect, LineTo, MoveToEx, RoundRect, SelectObject,
    IntersectClipRect, RestoreDC, SaveDC, SetBkMode, SetTextColor, BeginPaint, DT_CENTER,
    DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HDC, PAINTSTRUCT, PS_SOLID,
    SRCCOPY, TRANSPARENT,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::paint::{draw_text, ui_font};

const CLASS: &str = "ZenFileColDlg";

/// 窗口尺寸（DIP），按 DPI 缩放。
///
/// 高度**按行数动态算**（第54轮）：可选列从 11 涨到 21 后，固定 432 只装得下
/// 11 行，剩下的得滚。算出来的高度再钳到屏幕内，装不下就出滚动区。
const W_DIP: f32 = 380.0;
const TITLE_H: f32 = 34.0;
const ROW_H: f32 = 26.0;
const BTN_H: f32 = 28.0;
const LIST_TOP: f32 = TITLE_H + 46.0;
/// 底部按钮区（确定/取消 30 + 下边距 14 + 与列表的间隙 14）
const BTN_ZONE: f32 = 58.0;

struct Row {
    id: String,
    label: &'static str,
    on: bool,
    /// 名称列：恒显恒在第一位，勾不掉也移不动（资源管理器同款规矩）
    locked: bool,
}

struct Dlg {
    hwnd: HWND,
    app: *mut crate::app::App,
    rows: Vec<Row>,
    /// 当前选中行（上移/下移作用在它身上）
    sel: usize,
    hover_row: i32,
    hover_btn: u8,
    press_btn: u8,
    /// 列表可视高度（像素）—— 窗口高 - 标题/提示行 - 底部按钮区
    list_h: i32,
    /// 滚动偏移（像素），0..=max_scroll
    scroll: i32,
}

impl Dlg {
    fn max_scroll(&self, s: f32) -> i32 {
        let content = (self.rows.len() as i32) * px(ROW_H, s);
        (content - self.list_h).max(0)
    }
}

static mut DLG: Option<Dlg> = None;

/// 按钮矩形，绘制时写入、命中时读回 —— 两边各写一份常量必然错位。
thread_local! {
    static R_UP: Cell<RECT> = const { Cell::new(RECT { left: 0, top: 0, right: 0, bottom: 0 }) };
    static R_DOWN: Cell<RECT> = const { Cell::new(RECT { left: 0, top: 0, right: 0, bottom: 0 }) };
    static R_OK: Cell<RECT> = const { Cell::new(RECT { left: 0, top: 0, right: 0, bottom: 0 }) };
    static R_CANCEL: Cell<RECT> = const { Cell::new(RECT { left: 0, top: 0, right: 0, bottom: 0 }) };
    static R_CLOSE: Cell<RECT> = const { Cell::new(RECT { left: 0, top: 0, right: 0, bottom: 0 }) };
}

/// 打开对话框。已经开着就先关掉（同一时刻只留一个）。
pub fn open(app: &mut crate::app::App) {
    unsafe {
        close_now();
        register();
        let owner = app.hwnd;
        let s = if owner.0.is_null() {
            1.0f32
        } else {
            (GetDpiForWindow(owner) as f32 / 96.0).max(0.5)
        };
        let w = (W_DIP * s).round() as i32;
        // 行序：可见的按当前顺序在前，隐藏的按 COL_ALL 顺序在后。
        // 这样"勾掉再勾回来"会回到原来的位置，而不是被塞到末尾。
        let vis = app.visible_cols();
        let mut rows: Vec<Row> = Vec::new();
        for id in &vis {
            let locked = id == fm_core::columns::C_NAME;
            rows.push(Row {
                id: id.clone(),
                label: fm_core::columns::label_of(id),
                on: true,
                locked,
            });
        }
        for id in fm_core::columns::COL_ALL {
            if !vis.iter().any(|x| x == id) {
                rows.push(Row {
                    id: (*id).to_string(),
                    label: fm_core::columns::label_of(id),
                    on: false,
                    locked: *id == fm_core::columns::C_NAME,
                });
            }
        }
        // 高度按行数动态算（第54轮）：21 列 × 26px 早就不是 432 能装下的了。
        // 装得下多少算多少，钳到屏幕内，剩余的出滚动区（滚轮 + 拖滚动偏移）。
        let screen_h = GetSystemMetrics(SM_CYSCREEN);
        let h_want = px(LIST_TOP, s) + (rows.len() as i32) * px(ROW_H, s) + px(BTN_ZONE, s);
        let h = h_want.min(screen_h - 80).max(px(LIST_TOP, s) + 4 * px(ROW_H, s) + px(BTN_ZONE, s));
        let list_h = h - px(LIST_TOP, s) - px(BTN_ZONE, s);
        let (cx, cy) = if owner.0.is_null() {
            (w / 2 + 120, 240)
        } else {
            let mut or = RECT::default();
            let _ = GetWindowRect(owner, &mut or);
            ((or.left + or.right) / 2, (or.top + or.bottom) / 2)
        };
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            PCWSTR(HSTRING::from(CLASS).as_ptr()),
            PCWSTR(HSTRING::from("选择详细信息").as_ptr()),
            WS_POPUP | WS_CLIPCHILDREN,
            cx - w / 2,
            (cy - h / 2).max(0),
            w,
            h,
            if owner.0.is_null() { None } else { Some(owner) },
            None,
            None,
            None,
        );
        let Ok(hwnd) = hwnd else { return };
        if hwnd.0.is_null() {
            return;
        }
        let sel = rows.iter().position(|r| !r.locked).unwrap_or(0);
        DLG = Some(Dlg {
            hwnd,
            app: app as *mut crate::app::App,
            rows,
            sel,
            hover_row: -1,
            hover_btn: 0,
            press_btn: 0,
            list_h,
            scroll: 0,
        });
        let _ = ShowWindow(hwnd, SW_SHOW);
        // windows-rs 0.62 没导出 SetFocus，用 SetForegroundWindow 把键盘（Esc/上下键）
        // 送到这个对话框上；它带 owner，不会把主窗丢到别的应用后面。
        let _ = SetForegroundWindow(hwnd);
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
}

pub fn is_open() -> bool {
    unsafe { DLG.is_some() }
}

pub fn close_now() {
    unsafe {
        if let Some(d) = DLG.take() {
            let _ = DestroyWindow(d.hwnd);
        }
    }
}

fn register() {
    unsafe {
        static mut DONE: bool = false;
        if DONE {
            return;
        }
        DONE = true;
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(proc),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: PCWSTR(HSTRING::from(CLASS).as_ptr()),
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(std::ptr::null_mut()),
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
}

fn xy(lparam: LPARAM) -> (i32, i32) {
    (
        (lparam.0 & 0xFFFF) as i16 as i32,
        ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
    )
}

fn in_rect(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn scale_of(hwnd: HWND) -> f32 {
    (unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0).max(0.5)
}

fn px(v: f32, s: f32) -> i32 {
    (v * s).round() as i32
}

fn theme() -> crate::theme::Theme {
    crate::app_ref_opt()
        .map(|a| a.theme)
        .unwrap_or_else(crate::theme::dark)
}

/// 1=上移 2=下移 3=确定 4=取消 5=标题栏关闭
fn hit_btn(x: i32, y: i32) -> u8 {
    let mut h = 0u8;
    R_UP.with(|c| if in_rect(&c.get(), x, y) { h = 1 });
    R_DOWN.with(|c| if in_rect(&c.get(), x, y) { h = 2 });
    R_OK.with(|c| if in_rect(&c.get(), x, y) { h = 3 });
    R_CANCEL.with(|c| if in_rect(&c.get(), x, y) { h = 4 });
    R_CLOSE.with(|c| if in_rect(&c.get(), x, y) { h = 5 });
    h
}

fn hit_row(x: i32, y: i32, s: f32) -> Option<usize> {
    let top = px(LIST_TOP, s);
    let rh = px(ROW_H, s);
    let (list_h, scroll) = unsafe {
        DLG.as_ref()
            .map(|d| (d.list_h, d.scroll))
            .unwrap_or((0, 0))
    };
    // 只认滚动可视区里的行
    if y < top || y >= top + list_h {
        return None;
    }
    let i = (y - top + scroll) / rh.max(1);
    let n = unsafe { DLG.as_ref().map(|d| d.rows.len()).unwrap_or(0) };
    // 行的可点宽度到"上移/下移"按钮左沿为止
    let right = px(W_DIP - 116.0, s);
    if x < px(12.0, s) || x > right || i < 0 || (i as usize) >= n {
        return None;
    }
    Some(i as usize)
}

fn can_up() -> bool {
    unsafe {
        match DLG.as_ref() {
            Some(d) => {
                d.sel > 0 && !d.rows[d.sel].locked && !d.rows[d.sel - 1].locked
            }
            None => false,
        }
    }
}

fn can_down() -> bool {
    unsafe {
        DLG.as_ref()
            .map(|d| d.sel + 1 < d.rows.len() && !d.rows[d.sel].locked)
            .unwrap_or(false)
    }
}

fn apply_and_close() {
    unsafe {
        let Some(d) = DLG.take() else { return };
        let v: Vec<String> = d.rows.iter().filter(|r| r.on).map(|r| r.id.clone()).collect();
        let hwnd = d.hwnd;
        DLG = Some(d); // 先放回去，apply 之后再统一销毁（避免借用打架）
        let app = DLG.as_ref().map(|x| x.app).unwrap_or(std::ptr::null_mut());
        if !app.is_null() {
            (*app).apply_visible_cols(&v);
            let ow = (*app).hwnd;
            let _ = InvalidateRect(Some(ow), None, true);
        }
        DLG = None;
        let _ = DestroyWindow(hwnd);
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        // 标题栏那一条返回 HTCAPTION，系统自己起拖动循环；按钮位置必须让给
        // 客户区（返回 HTCLIENT），否则关闭按钮点不动（主窗口踩过 4 次的坑）。
        WM_NCHITTEST => {
            // lparam 是**屏幕**坐标，而按钮矩形是每次 WM_PAINT 按客户区算出来的
            // —— 不减窗口原点的话，关闭按钮的判据会整体偏出去一个窗口位置，
            // 表现就是"点在关闭上没反应、点标题栏别处反而关了窗"。
            let mut wr = RECT::default();
            let _ = GetWindowRect(hwnd, &mut wr);
            let x = (lparam.0 & 0xFFFF) as i16 as i32 - wr.left;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32 - wr.top;
            if R_CLOSE.with(|c| in_rect(&c.get(), x, y)) {
                return LRESULT(HTCLIENT as isize);
            }
            let s = scale_of(hwnd);
            if y < px(TITLE_H, s) {
                return LRESULT(HTCAPTION as isize);
            }
            LRESULT(HTCLIENT as isize)
        }
        WM_MOUSEMOVE => {
            let (x, y) = xy(lparam);
            let s = scale_of(hwnd);
            let b = hit_btn(x, y);
            let r = hit_row(x, y, s).map(|i| i as i32).unwrap_or(-1);
            if let Some(d) = DLG.as_mut() {
                if d.hover_btn != b || d.hover_row != r {
                    d.hover_btn = b;
                    d.hover_row = r;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = xy(lparam);
            if let Some(d) = DLG.as_mut() {
                d.press_btn = hit_btn(x, y);
            }
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = xy(lparam);
            let s = scale_of(hwnd);
            let b = hit_btn(x, y);
            let pressed = DLG.as_ref().map(|d| d.press_btn).unwrap_or(0);
            if let Some(d) = DLG.as_mut() {
                d.press_btn = 0;
            }
            if b != 0 && b == pressed {
                match b {
                    1 => {
                        if can_up() {
                            if let Some(d) = DLG.as_mut() {
                                d.rows.swap(d.sel, d.sel - 1);
                                d.sel -= 1;
                            }
                        }
                    }
                    2 => {
                        if can_down() {
                            if let Some(d) = DLG.as_mut() {
                                d.rows.swap(d.sel, d.sel + 1);
                                d.sel += 1;
                            }
                        }
                    }
                    3 => {
                        apply_and_close();
                        return LRESULT(0);
                    }
                    4 | 5 => {
                        close_now();
                        return LRESULT(0);
                    }
                    _ => {}
                }
                let _ = InvalidateRect(Some(hwnd), None, false);
                return LRESULT(0);
            }
            // 行：点一下 = 选中 + 反勾（资源管理器同款）。名称列不可动。
            if let Some(i) = hit_row(x, y, s) {
                if let Some(d) = DLG.as_mut() {
                    d.sel = i;
                    if !d.rows[i].locked {
                        d.rows[i].on = !d.rows[i].on;
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        // Esc = 取消，Enter = 确定，↑/↓ 只挪选中行（不动顺序）
        WM_KEYDOWN => {
            let vk = wparam.0 as u32;
            if vk == 0x1B {
                close_now();
                return LRESULT(0);
            }
            if vk == 0x0D {
                apply_and_close();
                return LRESULT(0);
            }
            if vk == 0x26 || vk == 0x28 {
                if let Some(d) = DLG.as_mut() {
                    let n = d.rows.len();
                    if vk == 0x26 && d.sel > 0 {
                        d.sel -= 1;
                    } else if vk == 0x28 && d.sel + 1 < n {
                        d.sel += 1;
                    }
                    // 选中行滚出可视区时把滚动跟着挪过去
                    let s = scale_of(hwnd);
                    let rh = px(ROW_H, s);
                    let row_top = (d.sel as i32) * rh;
                    if row_top < d.scroll {
                        d.scroll = row_top;
                    } else if row_top + rh > d.scroll + d.list_h {
                        d.scroll = row_top + rh - d.list_h;
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                return LRESULT(0);
            }
            // 滚轮在独立的 WM_MOUSEWHEEL 分支里处理（见下）
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        // 滚轮：列表比可视区高时滚动（第54轮起 21 列基本必然超出）
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
            let s = scale_of(hwnd);
            if let Some(d) = DLG.as_mut() {
                let step = px(ROW_H, s) * 3;
                d.scroll = (d.scroll - (delta / 120) * step).clamp(0, d.max_scroll(s));
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            paint(hwnd, hdc);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_DESTROY => {
            if let Some(true) = DLG.as_ref().map(|d| d.hwnd == hwnd) {
                DLG = None;
            }
            LRESULT(0)
        }
        // 自绘窗口也要自己定光标：类光标虽然有 IDC_ARROW，但本窗口全程
        // 返回 HTCLIENT，命中码永远到不了系统那条路。
        WM_SETCURSOR => {
            if let Ok(c) = LoadCursorW(None, IDC_ARROW) {
                SetCursor(Some(c));
            }
            LRESULT(1)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn draw_check(hdc: HDC, r: &RECT, on: bool, th: &crate::theme::Theme) {
    unsafe {
        let br = CreateSolidBrush(if on { th.accent } else { th.surface_alt });
        let pen = CreatePen(PS_SOLID, 1, if on { th.accent } else { th.border });
        let ob = SelectObject(hdc, br.into());
        let op = SelectObject(hdc, pen.into());
        RoundRect(hdc, r.left, r.top, r.right, r.bottom, 3, 3);
        SelectObject(hdc, ob);
        SelectObject(hdc, op);
        let _ = DeleteObject(br.into());
        let _ = DeleteObject(pen.into());
        if on {
            let tp = CreatePen(PS_SOLID, 2, COLORREF(0x00FFFFFF));
            let ot = SelectObject(hdc, tp.into());
            let _ = MoveToEx(hdc, r.left + 4, r.top + 8, None);
            let _ = LineTo(hdc, r.left + 7, r.top + 11);
            let _ = LineTo(hdc, r.left + 13, r.top + 4);
            SelectObject(hdc, ot);
            let _ = DeleteObject(tp.into());
        }
    }
}

fn paint(hwnd: HWND, hdc: HDC) {
    unsafe {
        let d = match DLG.as_ref() {
            Some(d) => d,
            None => return,
        };
        let th = theme();
        let s = scale_of(hwnd);
        let mut r = RECT::default();
        let _ = GetClientRect(hwnd, &mut r);
        let w = r.right - r.left;
        let h = r.bottom - r.top;
        if w <= 0 || h <= 0 {
            return;
        }
        let mem = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, w, h);
        let old_bmp = SelectObject(mem, bmp.into());

        let bg = CreateSolidBrush(th.window_bg);
        FillRect(mem, &r, bg);
        let _ = DeleteObject(bg.into());

        let f = ui_font(s);
        let old_f = SelectObject(mem, f.into());
        SetBkMode(mem, TRANSPARENT);

        // ---- 标题栏
        let title_h = px(TITLE_H, s);
        let tr = RECT { left: 0, top: 0, right: w, bottom: title_h };
        let tb = CreateSolidBrush(th.title_bg);
        FillRect(mem, &tr, tb);
        let _ = DeleteObject(tb.into());
        SetTextColor(mem, th.text);
        let t2 = RECT { left: px(14.0, s), top: 0, right: w - px(46.0, s), bottom: title_h };
        draw_text(
            mem,
            "选择详细信息",
            t2,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        let cr = RECT {
            left: w - px(38.0, s),
            top: px(1.0, s),
            right: w - px(38.0, s) + px(32.0, s),
            bottom: title_h - px(1.0, s),
        };
        R_CLOSE.with(|c| c.set(cr));
        if d.hover_btn == 5 {
            let hb = CreateSolidBrush(th.danger);
            FillRect(mem, &cr, hb);
            let _ = DeleteObject(hb.into());
        }
        SetTextColor(mem, if d.hover_btn == 5 { COLORREF(0x00FFFFFF) } else { th.text });
        let gf = crate::paint::glyph_font(s);
        let og = SelectObject(mem, gf.into());
        draw_text(mem, "\u{E8BB}", cr, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        SelectObject(mem, og);
        SetTextColor(mem, th.text);

        // ---- 提示行
        let hint = RECT {
            left: px(16.0, s),
            top: title_h + px(10.0, s),
            right: w - px(16.0, s),
            bottom: title_h + px(32.0, s),
        };
        SetTextColor(mem, th.text_dim);
        draw_text(
            mem,
            "选择要显示的列，用「上移/下移」调整顺序：",
            hint,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        SetTextColor(mem, th.text);

        // ---- 行
        let rh = px(ROW_H, s);
        let top = px(LIST_TOP, s);
        let row_right = px(W_DIP - 116.0, s);
        // 行可能比可视区多（21 列 > 列表高度）：**先裁剪再画**，否则超出的行
        // 会漫过底部按钮区。裁剪链要用 SaveDC/RestoreDC 包住 —— 记忆 DC 是
        // 整窗口共用的，裸 IntersectClipRect 会把后面画的按钮一起裁掉。
        let saved_clip = SaveDC(mem);
        let _ = IntersectClipRect(mem, 0, top, w, top + d.list_h);
        for (i, row) in d.rows.iter().enumerate() {
            let rt = RECT {
                left: px(12.0, s),
                top: top + (i as i32) * rh - d.scroll,
                right: row_right,
                bottom: top + (i as i32 + 1) * rh - d.scroll,
            };
            if rt.bottom <= top || rt.top >= top + d.list_h {
                continue;
            }
            let selected = i == d.sel;
            let hot = d.hover_row == i as i32;
            if selected || hot {
                let hb = CreateSolidBrush(if selected { th.selection_bg } else { th.hover_bg });
                FillRect(mem, &rt, hb);
                let _ = DeleteObject(hb.into());
            }
            let cb = RECT {
                left: rt.left + px(6.0, s),
                top: rt.top + (rh - px(16.0, s)) / 2,
                right: rt.left + px(6.0, s) + px(16.0, s),
                bottom: rt.top + (rh - px(16.0, s)) / 2 + px(16.0, s),
            };
            draw_check(mem, &cb, row.on, &th);
            SetTextColor(mem, if row.locked { th.text_dim } else { th.text });
            let lr = RECT {
                left: cb.right + px(10.0, s),
                top: rt.top,
                right: rt.right,
                bottom: rt.bottom,
            };
            draw_text(
                mem,
                row.label,
                lr,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
        }
        // 裁剪到此为止：上移/下移/确定/取消都在裁剪区外，必须正常画
        RestoreDC(mem, saved_clip);

        // ---- 上移 / 下移
        let bw = px(92.0, s);
        let bh = px(BTN_H, s);
        let bx = w - px(24.0, s) - bw;
        let up_r = RECT { left: bx, top: top + px(46.0, s), right: bx + bw, bottom: top + px(46.0, s) + bh };
        let dn_r = RECT { left: bx, top: up_r.bottom + px(10.0, s), right: bx + bw, bottom: up_r.bottom + px(10.0, s) + bh };
        R_UP.with(|c| c.set(up_r));
        R_DOWN.with(|c| c.set(dn_r));
        for (tag, rect, enabled) in [("上移", up_r, can_up()), ("下移", dn_r, can_down())] {
            let hb = CreateSolidBrush(if !enabled {
                th.surface_alt
            } else if d.hover_btn == if tag == "上移" { 1 } else { 2 } {
                th.hover_bg
            } else {
                th.surface
            });
            FillRect(mem, &rect, hb);
            let _ = DeleteObject(hb.into());
            SetTextColor(mem, if enabled { th.text } else { th.text_dim });
            draw_text(mem, tag, rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        }

        // ---- 确定 / 取消
        let ok_w = px(88.0, s);
        let bh2 = px(30.0, s);
        let by = h - px(14.0, s) - bh2;
        let cancel_r = RECT { left: w - px(24.0, s) - ok_w, top: by, right: w - px(24.0, s), bottom: by + bh2 };
        let ok_r = RECT {
            left: cancel_r.left - px(10.0, s) - ok_w,
            top: by,
            right: cancel_r.left - px(10.0, s),
            bottom: by + bh2,
        };
        R_OK.with(|c| c.set(ok_r));
        R_CANCEL.with(|c| c.set(cancel_r));
        // 确定：主色填充（对话框里唯一的主按钮）
        let ob = CreateSolidBrush(th.accent);
        FillRect(mem, &ok_r, ob);
        let _ = DeleteObject(ob.into());
        SetTextColor(mem, COLORREF(0x00FFFFFF));
        draw_text(mem, "确定", ok_r, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        let cb2 = CreateSolidBrush(if d.hover_btn == 4 { th.hover_bg } else { th.surface_alt });
        FillRect(mem, &cancel_r, cb2);
        let _ = DeleteObject(cb2.into());
        SetTextColor(mem, th.text);
        draw_text(mem, "取消", cancel_r, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);

        SelectObject(mem, old_f);
        let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
        SelectObject(mem, old_bmp);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
    }
}
