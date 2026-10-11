//! 自研的文件传输进度窗。
//!
//! 为什么不用 `IFileOperation` 自带的那一个：
//!   * 它的进度窗由 Shell 画，跟软件自己的暗色界面完全是两回事；
//!   * 拿不到"已处理文件 / 剩余文件 / 已用时间 / 预计剩余"这些数 ——
//!     `IFileOperationProgressSink` 只给一个百分比；
//!   * 它是**模态**的，会在自己的循环里泵消息，把 WndProc 重入进来。
//!
//! 这里就是一个 WS_POPUP + 自绘：标题栏那一条在 `WM_NCHITTEST` 里返回
//! `HTCAPTION`（系统因此能拖窗口），100ms 一跳读 `fm_core::transfer::Stats`。

use std::cell::Cell;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC,
    DeleteObject, EndPaint, FillRect, InvalidateRect, SelectObject, SetBkMode, SetTextColor,
    DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_TOP, DT_VCENTER,
    HDC, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::paint::{draw_text, ui_font};

const CLASS: &str = "ZenFileProgress";
const TIMER_TICK: usize = 1;
/// 完成后停留多久再自动关。太短用户看不清"已完成"，太长碍事。
const HOLD_MS: u64 = 900;

/// 当前这个弹窗的状态。同一时刻只显示一个传输，所以放全局就够。
struct Prog {
    hwnd: HWND,
    stats: fm_core::transfer::Shared,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    kind: fm_core::transfer::Kind,
    /// 目标目录（画在"到"那一行）
    dest: String,
    /// 完成/取消之后开始计时的那一刻
    done_at: Option<std::time::Instant>,
    hover_btn: u8,
    press_btn: u8,
}

static mut PROG: Option<Prog> = None;

/// 两个按钮的矩形。绘制时算出、命中测试时读回 —— 两边各写一份常量就会错位。
thread_local! {
    static BTN_CANCEL: Cell<RECT> = const { Cell::new(RECT { left: 0, top: 0, right: 0, bottom: 0 }) };
    static BTN_CLOSE: Cell<RECT> = const { Cell::new(RECT { left: 0, top: 0, right: 0, bottom: 0 }) };
}

/// 打开进度窗。已经在显示就先关掉旧的。
pub fn open(
    kind: fm_core::transfer::Kind,
    dest: &std::path::Path,
    stats: fm_core::transfer::Shared,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    owner: HWND,
) {
    unsafe {
        if PROG.is_some() {
            close_now();
        }
        register();
        let s = if owner.0.is_null() {
            1.0f32
        } else {
            (GetDpiForWindow(owner) as f32 / 96.0).max(0.5)
        };
        let w = (520.0 * s).round() as i32;
        let h = (212.0 * s).round() as i32;
        // 居中到主窗口
        let (cx, cy) = if owner.0.is_null() {
            (w / 2 + 100, h / 2 + 100)
        } else {
            let mut or = RECT::default();
            let _ = GetWindowRect(owner, &mut or);
            ((or.left + or.right) / 2, (or.top + or.bottom) / 2)
        };
        let title = HSTRING::from(if kind == fm_core::transfer::Kind::Move {
            "正在移动"
        } else {
            "正在复制"
        });
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            PCWSTR(HSTRING::from(CLASS).as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_POPUP | WS_CLIPCHILDREN,
            cx - w / 2,
            cy - h / 2,
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
        PROG = Some(Prog {
            hwnd,
            stats,
            cancel,
            kind,
            dest: dest.to_string_lossy().to_string(),
            done_at: None,
            hover_btn: 0,
            press_btn: 0,
        });
        let _ = SetTimer(Some(hwnd), TIMER_TICK, 100, None);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
}

pub fn is_open() -> bool {
    unsafe { PROG.is_some() }
}

/// 立刻关掉（用户点了取消 / 关窗 / 停留结束）。
pub fn close_now() {
    unsafe {
        if let Some(p) = PROG.take() {
            let _ = KillTimer(Some(p.hwnd), TIMER_TICK);
            let _ = DestroyWindow(p.hwnd);
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

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        // 标题栏那一条要能拖窗口：返回 HTCAPTION 之后系统自己起拖动模态循环。
        // 吞掉这条消息 = 窗口拖不动（主窗口那边踩过三次的坑）。
        WM_NCHITTEST => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut r = RECT::default();
            let _ = GetClientRect(hwnd, &mut r);
            let s = scale_of(hwnd);
            if BTN_CLOSE.with(|c| point_in(&c.get(), x, y)) {
                return LRESULT(HTCLIENT as isize);
            }
            if y < px(34.0, s) && x < r.right - px(46.0, s) {
                return LRESULT(HTCAPTION as isize);
            }
            LRESULT(HTCLIENT as isize)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_TICK {
                // 引擎报"跑完了"就开始倒数自动关。`done_at` 只能在这里置位 ——
                // 主窗口那条 `drain_transfer` 拿的是同一个 finished，两边都置会打架。
                let finished = PROG
                    .as_ref()
                    .and_then(|p| p.stats.lock().ok())
                    .map(|s| s.finished)
                    .unwrap_or(false);
                if finished {
                    if let Some(p) = PROG.as_mut() {
                        if p.done_at.is_none() {
                            p.done_at = Some(std::time::Instant::now());
                        }
                    }
                }
                let hold = PROG.as_ref().and_then(|p| p.done_at);
                if let Some(t) = hold {
                    if t.elapsed().as_millis() as u64 >= HOLD_MS {
                        close_now();
                        return LRESULT(0);
                    }
                }
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = xy(lparam);
            let h = hit_btn(x, y);
            if let Some(p) = PROG.as_mut() {
                if p.hover_btn != h {
                    p.hover_btn = h;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = xy(lparam);
            if let Some(p) = PROG.as_mut() {
                p.press_btn = hit_btn(x, y);
            }
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = xy(lparam);
            let h = hit_btn(x, y);
            let pressed = PROG.as_ref().map(|p| p.press_btn).unwrap_or(0);
            if let Some(p) = PROG.as_mut() {
                p.press_btn = 0;
            }
            if h != 0 && h == pressed {
                match h {
                    1 => {
                        // 取消：引擎会在下一个块边界自己停下
                        if let Some(p) = PROG.as_ref() {
                            p.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        close_now();
                    }
                    // 关窗：传输继续在后台跑，主窗口照常刷新
                    2 => close_now(),
                    _ => {}
                }
                return LRESULT(0);
            }
            let _ = InvalidateRect(Some(hwnd), None, false);
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
            let _ = KillTimer(Some(hwnd), TIMER_TICK);
            if let Some(true) = PROG.as_ref().map(|p| p.hwnd == hwnd) {
                PROG = None;
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn xy(lparam: LPARAM) -> (i32, i32) {
    (
        (lparam.0 & 0xFFFF) as i16 as i32,
        ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
    )
}

fn point_in(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// 1 = 取消按钮，2 = 标题栏关闭。
fn hit_btn(x: i32, y: i32) -> u8 {
    if BTN_CANCEL.with(|c| point_in(&c.get(), x, y)) {
        return 1;
    }
    if BTN_CLOSE.with(|c| point_in(&c.get(), x, y)) {
        return 2;
    }
    0
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

fn paint(hwnd: HWND, hdc: HDC) {
    unsafe {
        let Some(p) = PROG.as_ref() else { return };
        let th = theme();
        let s = scale_of(hwnd);
        let mut r = RECT::default();
        let _ = GetClientRect(hwnd, &mut r);
        let w = r.right - r.left;
        let h = r.bottom - r.top;
        if w <= 0 || h <= 0 {
            return;
        }

        // 双缓冲：一次 BitBlt，别让逐条 FillRect 在窗口上闪
        let mem = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, w, h);
        let old_bmp = SelectObject(mem, bmp.into());

        let bg = CreateSolidBrush(th.window_bg);
        FillRect(mem, &r, bg);
        let _ = DeleteObject(bg.into());
        let title_h = px(34.0, s);
        let tr = RECT { left: 0, top: 0, right: w, bottom: title_h };
        let tb = CreateSolidBrush(th.title_bg);
        FillRect(mem, &tr, tb);
        let _ = DeleteObject(tb.into());

        let f = ui_font(s);
        let old_f = SelectObject(mem, f.into());
        SetBkMode(mem, TRANSPARENT);

        // 一次性快照：别画到一半数字变了，看着像界面自己打架
        let st = p.stats.lock().map(|g| g.clone()).unwrap_or_default();
        let done = p.done_at.is_some();

        // ---- 标题
        SetTextColor(mem, th.text);
        let title = if done {
            if st.canceled {
                "已取消"
            } else if st.failed > 0 {
                "已完成（有部分失败）"
            } else {
                "已完成"
            }
        } else if p.kind == fm_core::transfer::Kind::Move {
            "正在移动…"
        } else {
            "正在复制…"
        };
        let t2 = RECT { left: px(14.0, s), top: 0, right: w - px(46.0, s), bottom: title_h };
        draw_text(
            mem,
            title,
            t2,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );

        // ---- 右上角关闭
        let cr = RECT {
            left: w - px(38.0, s),
            top: px(1.0, s),
            right: w - px(38.0, s) + px(32.0, s),
            bottom: title_h - px(1.0, s),
        };
        BTN_CLOSE.with(|c| c.set(cr));
        let hot = p.hover_btn == 2;
        if hot {
            let hb = CreateSolidBrush(th.danger);
            FillRect(mem, &cr, hb);
            let _ = DeleteObject(hb.into());
        }
        SetTextColor(mem, if hot { COLORREF(0x00FFFFFF) } else { th.text });
        let gf = crate::paint::glyph_font(s);
        let old_g = SelectObject(mem, gf.into());
        draw_text(mem, "\u{E8BB}", cr, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        SelectObject(mem, old_g);
        SetTextColor(mem, th.text);

        // ---- 进度条
        let bar_top = title_h + px(18.0, s);
        let bar = RECT {
            left: px(16.0, s),
            top: bar_top,
            right: w - px(16.0, s),
            bottom: bar_top + px(8.0, s),
        };
        let track = CreateSolidBrush(th.surface_alt);
        FillRect(mem, &bar, track);
        let _ = DeleteObject(track.into());
        let prog = if done { 1.0f64 } else { st.progress() };
        let fill_w = ((bar.right - bar.left) as f64 * prog).round() as i32;
        if fill_w > 0 {
            let fr = RECT { left: bar.left, top: bar.top, right: bar.left + fill_w, bottom: bar.bottom };
            let fb = CreateSolidBrush(th.accent);
            FillRect(mem, &fr, fb);
            let _ = DeleteObject(fb.into());
        }
        let pr = RECT {
            left: bar.left,
            top: bar.top + px(10.0, s),
            right: bar.right,
            bottom: bar.top + px(28.0, s),
        };
        draw_text(
            mem,
            &format!("{}%", (prog * 100.0).round() as i32),
            pr,
            DT_RIGHT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX,
        );

        // ---- 当前文件
        let mut y = bar.top + px(34.0, s);
        SetTextColor(mem, th.text_dim);
        let lr = RECT { left: px(16.0, s), top: y, right: w - px(16.0, s), bottom: y + px(20.0, s) };
        let cur = if done || st.current.is_empty() {
            String::new()
        } else {
            format!("正在处理：{}", st.current)
        };
        draw_text(mem, &cur, lr, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);

        // ---- 四格数据：已处理 / 剩余 / 已用 / 预计剩余
        y += px(26.0, s);
        let left = px(16.0, s);
        let colw = (w - px(32.0, s)) / 2;
        let rows: [String; 4] = [
            format!("已处理文件：{} / {}", st.files_done, st.files_total),
            format!("剩余文件：{}", st.files_left()),
            format!("已用时间：{}", fm_core::transfer::fmt_duration(st.elapsed_ms)),
            format!(
                "预计剩余：{}",
                if done { "—".to_string() } else { fm_core::transfer::fmt_eta(st.eta_ms) }
            ),
        ];
        for (i, t) in rows.iter().enumerate() {
            let (c, rw) = ((i % 2) as i32, (i / 2) as i32);
            let rr = RECT {
                left: left + c * colw,
                top: y + rw * px(22.0, s),
                right: left + (c + 1) * colw,
                bottom: y + (rw + 1) * px(22.0, s),
            };
            draw_text(mem, t, rr, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
        }
        y += px(48.0, s);

        // ---- 速度 + 目标
        let speed_txt = if done {
            String::new()
        } else {
            format!(
                "{} / 秒",
                crate::app::human_unit(st.speed as u64, fm_core::SizeUnit::Auto)
            )
        };
        let sr = RECT { left: px(16.0, s), top: y, right: w - px(16.0, s), bottom: y + px(20.0, s) };
        draw_text(
            mem,
            &format!("{speed_txt}    到：{}", p.dest),
            sr,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );

        // ---- 取消按钮（完成后不再显示）
        let bw = px(96.0, s);
        let bh = px(28.0, s);
        let br = RECT {
            left: w - px(16.0, s) - bw,
            top: h - px(14.0, s) - bh,
            right: w - px(16.0, s),
            bottom: h - px(14.0, s),
        };
        if done {
            BTN_CANCEL.with(|c| c.set(RECT::default()));
        } else {
            BTN_CANCEL.with(|c| c.set(br));
            let hot = p.hover_btn == 1;
            let bbr = CreateSolidBrush(if hot { th.hover_bg } else { th.surface_alt });
            FillRect(mem, &br, bbr);
            let _ = DeleteObject(bbr.into());
            SetTextColor(mem, th.text);
            draw_text(mem, "取消", br, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        }

        SelectObject(mem, old_f);
        let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
        SelectObject(mem, old_bmp);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
    }
}
