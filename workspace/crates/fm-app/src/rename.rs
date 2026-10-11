//! 批量重命名对话框 + 右键菜单里那个「批量重命名 ▸」下拉。
//!
//! ## 为什么自绘
//!
//! 系统没有可复用的批量重命名对话框（资源管理器那个是 Win11 才加的，且藏在
//! COM 后面）。这里自绘一个：模式下拉 + 参数输入 + 高级选项 + **实时预览**。
//! 预览是核心 —— 批量改名一旦算错就是几十个文件一起错，必须让用户先看见结果。
//!
//! ## 落盘
//!
//! 全部放进一个 `IFileOperation` 提交（见 `ops::rename_items`）：要么一起成，
//! 要么一起失败，Ctrl+Z 一次撤销全部。逐个调用会留下"改了一半"的目录。
//!
//! ## 名字怎么改
//!
//! 规则部分在 `fm_core::bulk`（纯函数、有自检），这里只管收参数、调 `plan`、
//! 判冲突、画预览。冲突（新名互撞 / 撞到没参与改名的文件）会让「重命名」按钮
//! 变灰，和资源管理器一致。

use std::collections::HashSet;
use std::path::PathBuf;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC, DeleteObject,
    DrawTextW, EndPaint, FillRect, InvalidateRect, SelectObject, SetBkMode, SetTextColor,
    DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DRAW_TEXT_FORMAT,
    HDC, HFONT, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowLongPtrW,
    GetWindowTextW, RegisterClassExW, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow,
    GWLP_USERDATA, GWLP_WNDPROC, HMENU, SET_WINDOW_POS_FLAGS, SW_HIDE, SW_SHOW, WINDOW_STYLE,
    WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_NCHITTEST, WM_PAINT, WM_TIMER, WNDCLASSEXW, WS_CHILD, WS_POPUP, WS_VISIBLE,
    WS_EX_WINDOWEDGE, ES_AUTOHSCROLL,
};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::Graphics::Gdi::BeginPaint;

use crate::app::App;
use crate::theme::Theme;

const BASE_W: i32 = 580;
const BASE_H: i32 = 590;
const TITLE_H: i32 = 34;
const ROW_H: i32 = 24;

/// 模式下拉里的条目顺序 —— 和下面的 `mode_rule` 一一对应。
pub const MODE_LABELS: [&str; 5] = ["查找并替换", "添加前缀", "添加后缀", "名称 + 序号", "删除字符"];

/// 模式 -> 规则。参数从输入框实时读，切模式时把输入框内容按新模式的含义重置。
fn default_param(mode: usize) -> (String, String) {
    match mode {
        0 => (String::new(), String::new()),
        1 => ("【新】".to_string(), String::new()),
        2 => ("-副本".to_string(), String::new()),
        3 => ("新建名称".to_string(), String::new()),
        _ => ("1".to_string(), "3".to_string()),
    }
}

struct Bulk {
    app: *mut App,
    items: Vec<PathBuf>,
    mode: usize,
    keep_ext: bool,
    case_sensitive: bool,
    first_only: bool,
    start: i32,
    digits: i32,
    news: Vec<String>,
    conflict: Vec<bool>,
    hover: Hit,
    scroll: i32,
    edits: [HWND; 4],
    scale: f32,
    theme: Theme,
    bold: HFONT,
    w: i32,
    h: i32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    None,
    Close,
    Mode,
    Adv(usize),
    Ok,
    Cancel,
}

/// 高级选项区四个槽位：0/1 是第一行左右，2/3 是第二行左右。
enum Slot {
    None,
    Check(&'static str, bool),
    Num(&'static str, i32),
}

impl Bulk {
    fn s(&self, v: i32) -> i32 {
        (v as f32 * self.scale).round() as i32
    }
    fn old_name(&self, i: usize) -> String {
        self.items[i]
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    }
    fn rule(&self) -> fm_core::bulk::Rule {
        use fm_core::bulk::Rule;
        let p1 = text_of(self.edits[0]);
        let p2 = text_of(self.edits[1]);
        let num = |d: i32| text_of(self.edits[2]).parse::<i32>().unwrap_or(d);
        match self.mode {
            0 => Rule::Replace {
                find: p1,
                repl: p2,
                case_sensitive: self.case_sensitive,
                first_only: self.first_only,
            },
            1 => Rule::Prefix(p1),
            2 => Rule::Suffix(p1),
            3 => Rule::Number {
                template: p1,
                start: num(self.start),
                digits: text_of(self.edits[3]).parse::<i32>().unwrap_or(self.digits),
            },
            _ => Rule::Strip { at: num(1), len: p2.parse::<i32>().unwrap_or(0) },
        }
    }
    fn mode_rect(&self) -> RECT {
        RECT { left: self.s(94), top: self.s(66), right: self.w - self.s(14), bottom: self.s(92) }
    }
    fn p1_rect(&self) -> RECT {
        RECT { left: self.s(94), top: self.s(98), right: self.w - self.s(14), bottom: self.s(124) }
    }
    fn p2_rect(&self) -> RECT {
        RECT { left: self.s(94), top: self.s(130), right: self.w - self.s(14), bottom: self.s(156) }
    }
    fn slot_rect(&self, i: usize) -> RECT {
        let (row, col) = (i / 2, i % 2);
        let top = self.s(188 + row as i32 * 30);
        let (left, right) = if col == 0 {
            (self.s(94), self.s(320))
        } else {
            (self.s(330), self.w - self.s(14))
        };
        RECT { left, top, right, bottom: top + self.s(26) }
    }
    fn prev_rect(&self) -> RECT {
        RECT {
            left: self.s(14),
            top: self.s(272),
            right: self.w - self.s(14),
            bottom: self.h - self.s(58),
        }
    }
    fn ok_rect(&self) -> RECT {
        let top = self.h - self.s(46);
        let right = self.w - self.s(14) - self.s(112) - self.s(8);
        RECT { left: right - self.s(112), top, right, bottom: top + self.s(28) }
    }
    fn cancel_rect(&self) -> RECT {
        let top = self.h - self.s(46);
        RECT { left: self.w - self.s(14) - self.s(112), top, right: self.w - self.s(14), bottom: top + self.s(28) }
    }
    fn close_rect(&self) -> RECT {
        RECT { left: self.w - self.s(44), top: 0, right: self.w, bottom: self.s(TITLE_H) }
    }
    fn slots(&self) -> [Slot; 4] {
        let none = || Slot::None;
        match self.mode {
            0 => [
                Slot::Check("区分大小写", self.case_sensitive),
                Slot::Check("仅替换首个匹配", self.first_only),
                Slot::Check("保留扩展名", self.keep_ext),
                none(),
            ],
            1 | 2 | 4 => [Slot::Check("保留扩展名", self.keep_ext), none(), none(), none()],
            3 => [
                Slot::Num("起始序号", self.start),
                Slot::Num("序号位数", self.digits),
                none(),
                none(),
            ],
            _ => [none(), none(), none(), none()],
        }
    }
    fn changed(&self) -> usize {
        self.news
            .iter()
            .enumerate()
            .filter(|(i, n)| **n != self.old_name(*i))
            .count()
    }
    fn can_apply(&self) -> bool {
        self.changed() > 0
            && !self.conflict.iter().any(|c| *c)
            && !self.news.iter().any(|n| n.is_empty())
    }
}

fn text_of(h: HWND) -> String {
    if h.0.is_null() {
        return String::new();
    }
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(h, &mut buf) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize]).to_string()
}

// ---------------------------------------------------------------- 打开

pub fn register_class() {
    let cls = HSTRING::from("ZenFileRename");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(br_proc),
        lpszClassName: PCWSTR(cls.as_ptr()),
        ..Default::default()
    };
    unsafe {
        RegisterClassExW(&wc);
    }
    register_combo();
}

/// `mode` 是右键菜单下拉里选中的那一项的序号（见 `MODE_LABELS`）。
pub fn open(app_ptr: *mut App, items: Vec<PathBuf>, mode: usize) {
    if app_ptr.is_null() || items.is_empty() {
        return;
    }
    let app = unsafe { &mut *app_ptr };
    let scale = app.metrics.scale;
    let theme = app.theme;
    let w = (BASE_W as f32 * scale).round() as i32;
    let h = (BASE_H as f32 * scale).round() as i32;
    let bold = unsafe { crate::paint::make_font((-15.0 * scale) as i32, 700, "Segoe UI") };
    let (d1, d2) = default_param(mode);

    let mut st = Box::new(Bulk {
        app: app_ptr,
        items,
        mode,
        keep_ext: true,
        case_sensitive: false,
        first_only: false,
        start: 1,
        digits: 3,
        news: Vec::new(),
        conflict: Vec::new(),
        hover: Hit::None,
        scroll: 0,
        edits: [HWND::default(); 4],
        scale,
        theme,
        bold,
        w,
        h,
    });
    recompute(&mut st);

    let state = Box::into_raw(st);
    let cls = HSTRING::from("ZenFileRename");
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_WINDOWEDGE,
            PCWSTR(cls.as_ptr()),
            PCWSTR(HSTRING::from("批量重命名").as_ptr()),
            WS_POPUP,
            0,
            0,
            w,
            h,
            Some(app.hwnd),
            None,
            None,
            Some(state as *const core::ffi::c_void),
        )
    };
    let hwnd = match hwnd {
        Ok(h) => h,
        Err(_) => {
            let mut back = unsafe { Box::from_raw(state) };
            if !back.bold.0.is_null() {
                unsafe {
                    let _ = DeleteObject(back.bold.into());
                }
            }
            return;
        }
    };
    unsafe {
        let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
        let st = &mut *state;
        for (i, id) in [ID_P1, ID_P2, ID_START, ID_DIGITS].iter().enumerate() {
            let (r, txt) = match i {
                0 => (st.p1_rect(), d1.clone()),
                1 => (st.p2_rect(), d2.clone()),
                2 => (st.slot_rect(0), st.start.to_string()),
                _ => (st.slot_rect(1), st.digits.to_string()),
            };
            st.edits[i] = make_edit(hwnd, *id, &r, &txt, scale);
        }
        layout_edits(st);
        recompute(st);

        let mut pr = RECT::default();
        let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(app.hwnd, &mut pr);
        let cx = (pr.left + pr.right) / 2 - w / 2;
        let cy = (pr.top + pr.bottom) / 2 - h / 2;
        let _ = SetWindowPos(
            hwnd,
            None,
            cx,
            cy,
            0,
            0,
            SET_WINDOW_POS_FLAGS(0x0001) | SET_WINDOW_POS_FLAGS(0x0004),
        );
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = InvalidateRect(Some(hwnd), None, true);
        // 除了 EN_CHANGE，再挂一个 200ms 的定时器兜底：EN_CHANGE 在
        // WM_SETTEXT（程序自己填内容）时并不保证发出，光靠它预览会失灵。
        let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(Some(hwnd), 1, 200, None);
    }
}

const ID_P1: u16 = 1;
const ID_P2: u16 = 2;
const ID_START: u16 = 3;
const ID_DIGITS: u16 = 4;

/// 子控件 EDIT：锚点是用户要打字的地方，用系统 EDIT 而不是自绘 —— 自绘要自己
/// 处理光标、选区、中文输入法，做不全就是"打不出字"。
fn make_edit(parent: HWND, id: u16, r: &RECT, text: &str, scale: f32) -> HWND {
    unsafe {
        let cls = HSTRING::from("EDIT");
        let h = CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WS_EX_CLIENTEDGE,
            PCWSTR(cls.as_ptr()),
            PCWSTR(HSTRING::from(text).as_ptr()),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            Some(parent),
            Some(HMENU(id as *mut core::ffi::c_void)),
            None,
            None,
        )
        .unwrap_or_default();
        let _ = SetWindowLongPtrW(h, GWLP_USERDATA, parent.0 as isize);
        EDIT_OLD = SetWindowLongPtrW(h, GWLP_WNDPROC, (edit_proc as *const ()) as isize);
        crate::paint::set_edit_font(h, scale);
        crate::paint::set_edit_margins(h, scale, 6, 6);
        h
    }
}

static mut EDIT_OLD: isize = 0;

unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_KEYDOWN {
        let parent = HWND(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut core::ffi::c_void);
        match wparam.0 as i32 {
            0x0D => {
                // 回车 = 确定：在输入框里打字的人不会想去够鼠标
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(parent),
                    WM_APP_OK,
                    WPARAM(0),
                    LPARAM(0),
                );
                return LRESULT(0);
            }
            0x1B => {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(parent),
                    WM_APP_CANCEL,
                    WPARAM(0),
                    LPARAM(0),
                );
                return LRESULT(0);
            }
            _ => {}
        }
    }
    let prev: windows::Win32::UI::WindowsAndMessaging::WNDPROC =
        std::mem::transmute::<isize, windows::Win32::UI::WindowsAndMessaging::WNDPROC>(EDIT_OLD);
    CallWindowProcW(prev, hwnd, msg, wparam, lparam)
}

/// 按当前模式摆放/显示输入框：参数 2 和"起始序号/位数"只在需要的模式出现。
fn layout_edits(st: &mut Bulk) {
    let need_p2 = st.rule().param_count() > 1;
    let is_num = st.mode == 3;
    let p2 = st.p2_rect();
    let s0 = st.slot_rect(0);
    let s1 = st.slot_rect(1);
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            st.edits[1],
            if need_p2 { SW_SHOW } else { SW_HIDE },
        );
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            st.edits[2],
            if is_num { SW_SHOW } else { SW_HIDE },
        );
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            st.edits[3],
            if is_num { SW_SHOW } else { SW_HIDE },
        );
        // 序号模式里两个数字框要落到"高级选项"的槽位上，不能留在参数行
        for (i, r) in [(2, s0), (3, s1)] {
            let _ = SetWindowPos(
                st.edits[i],
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SET_WINDOW_POS_FLAGS(0x0004) | SET_WINDOW_POS_FLAGS(0x0002),
            );
        }
        let _ = SetWindowPos(
            st.edits[1],
            None,
            p2.left,
            p2.top,
            p2.right - p2.left,
            p2.bottom - p2.top,
            SET_WINDOW_POS_FLAGS(0x0004) | SET_WINDOW_POS_FLAGS(0x0002),
        );
    }
}

fn recompute(st: &mut Bulk) {
    let names: Vec<String> = (0..st.items.len()).map(|i| st.old_name(i)).collect();
    let rule = st.rule();
    let news = fm_core::bulk::plan(&names, &rule, st.keep_ext);
    let mut conflict = fm_core::bulk::duplicates(&news);

    // 撞到目录里没参与改名的文件也算冲突：NTFS 不允许同名
    let mut taken: HashSet<String> = HashSet::new();
    if let Some(dir) = st.items.first().and_then(|p| p.parent()) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                taken.insert(e.file_name().to_string_lossy().to_lowercase());
            }
        }
    }
    for n in &names {
        taken.remove(&n.to_lowercase());
    }
    for i in 0..news.len() {
        if taken.contains(&news[i].to_lowercase()) {
            conflict[i] = true;
        }
    }
    st.news = news;
    st.conflict = conflict;
}

// ---------------------------------------------------------------- 消息

const WM_APP_PICK: u32 = 0x8005;
const WM_APP_OK: u32 = 0x8006;
const WM_APP_CANCEL: u32 = 0x8007;

unsafe extern "system" fn br_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Bulk;
    if p.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let st = &mut *p;
    match msg {
        WM_ERASEBKGND => return LRESULT(1),
        WM_NCHITTEST => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = (lparam.0 >> 16) as i16 as i32;
            let mut sp = POINT { x, y };
            let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut sp);
            if sp.y < st.s(TITLE_H) {
                return if sp.x >= st.close_rect().left { LRESULT(1) } else { LRESULT(2) };
            }
            LRESULT(1)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mem = CreateCompatibleDC(Some(hdc));
            let bmp = CreateCompatibleBitmap(hdc, st.w, st.h);
            let old = SelectObject(mem, bmp.into());
            paint(mem, st);
            let _ = BitBlt(hdc, 0, 0, st.w, st.h, Some(mem), 0, 0, SRCCOPY);
            SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
            let _ = EndPaint(hwnd, &mut ps);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = (lparam.0 >> 16) as i16 as i32;
            let h = hit_test(st, x, y);
            if h != st.hover {
                st.hover = h;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let step = if delta > 0 { -3 } else { 3 };
            let max_scroll = (st.items.len() as i32 - visible_rows(st)).max(0);
            let next = (st.scroll + step).clamp(0, max_scroll);
            if next != st.scroll {
                st.scroll = next;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = (lparam.0 >> 16) as i16 as i32;
            match hit_test(st, x, y) {
                Hit::Close | Hit::Cancel => {
                    let _ = DestroyWindow(hwnd);
                }
                Hit::Mode => {
                    let r = st.mode_rect();
                    let mut pt = POINT { x: r.left, y: r.bottom };
                    let _ = windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut pt);
                    open_combo(
                        hwnd,
                        MODE_LABELS.iter().map(|s| s.to_string()).collect(),
                        st.mode,
                        pt.x,
                        pt.y,
                        r.right - r.left,
                        st.scale,
                        st.theme,
                    );
                }
                Hit::Adv(i) => {
                    match st.slots()[i] {
                        Slot::Check(_, _) => match i {
                            0 => st.case_sensitive = !st.case_sensitive,
                            1 => st.first_only = !st.first_only,
                            _ => st.keep_ext = !st.keep_ext,
                        },
                        Slot::Num(_, _) => {
                            let e = if i == 0 { st.edits[2] } else { st.edits[3] };
                            let _ = SetFocus(Some(e));
                        }
                        Slot::None => {}
                    }
                    recompute(st);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                Hit::Ok => {
                    if st.can_apply() {
                        apply(st);
                        let _ = DestroyWindow(hwnd);
                    }
                }
                Hit::None => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            if (wparam.0 >> 16) & 0xFFFF == 0x0300 {
                // EN_CHANGE：输入框内容变了，预览要跟着变
                refresh(st, hwnd);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            refresh(st, hwnd);
            LRESULT(0)
        }
        WM_APP_PICK => {
            let i = wparam.0 as usize;
            if i < MODE_LABELS.len() && i != st.mode {
                st.mode = i;
                st.keep_ext = true;
                let (d1, d2) = default_param(i);
                let _ = SetWindowTextW(st.edits[0], PCWSTR(HSTRING::from(&d1).as_ptr()));
                let _ = SetWindowTextW(st.edits[1], PCWSTR(HSTRING::from(&d2).as_ptr()));
                layout_edits(st);
                recompute(st);
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_APP_OK => {
            if st.can_apply() {
                apply(st);
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_APP_CANCEL => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = windows::Win32::UI::WindowsAndMessaging::KillTimer(Some(hwnd), 1);
            for e in st.edits {
                if !e.0.is_null() {
                    let _ = DestroyWindow(e);
                }
            }
            let bold = st.bold;
            drop(Box::from_raw(p));
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if !bold.0.is_null() {
                let _ = DeleteObject(bold.into());
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 重算预览；结果变了才重画（定时器每 200ms 会走一次，别无谓地刷屏）。
fn refresh(st: &mut Bulk, hwnd: HWND) {
    let before = st.news.clone();
    recompute(st);
    if st.news != before {
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
}

fn visible_rows(st: &Bulk) -> i32 {
    let r = st.prev_rect();
    ((r.bottom - r.top) / st.s(ROW_H)).max(1)
}

fn apply(st: &mut Bulk) {
    let owner = unsafe { (*st.app).hwnd.0 as isize };
    let pairs: Vec<(PathBuf, String)> = st
        .items
        .iter()
        .zip(st.news.iter())
        .filter(|(p, n)| {
            p.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default() != **n
        })
        .map(|(p, n)| (p.clone(), n.clone()))
        .collect();
    if pairs.is_empty() {
        return;
    }
    match fm_core::ops::rename_items(owner, &pairs) {
        Ok(()) => {
            unsafe {
                (*st.app).reload();
            }
        }
        Err(e) => unsafe {
            crate::app::report((*st.app).hwnd, &format!("批量重命名失败：{e}"));
            (*st.app).reload();
        },
    }
}

fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x <= r.right && y >= r.top && y <= r.bottom
}

fn hit_test(st: &Bulk, x: i32, y: i32) -> Hit {
    if inside(&st.close_rect(), x, y) {
        return Hit::Close;
    }
    if inside(&st.ok_rect(), x, y) {
        return Hit::Ok;
    }
    if inside(&st.cancel_rect(), x, y) {
        return Hit::Cancel;
    }
    if inside(&st.mode_rect(), x, y) {
        return Hit::Mode;
    }
    for i in 0..4 {
        if let Slot::Check(_, _) | Slot::Num(_, _) = st.slots()[i] {
            if inside(&st.slot_rect(i), x, y) {
                return Hit::Adv(i);
            }
        }
    }
    Hit::None
}

// ---------------------------------------------------------------- 绘制

unsafe fn fill(hdc: HDC, r: &RECT, color: COLORREF) {
    let b = CreateSolidBrush(color);
    let _ = FillRect(hdc, r, b);
    let _ = DeleteObject(b.into());
}

unsafe fn text(hdc: HDC, s: &str, r: RECT, color: COLORREF, flags: DRAW_TEXT_FORMAT) {
    SetTextColor(hdc, color);
    let mut v: Vec<u16> = s.encode_utf16().collect();
    let _ = DrawTextW(hdc, &mut v, &mut r.clone(), flags | DT_NOPREFIX);
}

unsafe fn label(hdc: HDC, s: &str, r: RECT, color: COLORREF) {
    text(hdc, s, r, color, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
}

unsafe fn paint(hdc: HDC, st: &Bulk) {
    let t = &st.theme;
    fill(hdc, &RECT { left: 0, top: 0, right: st.w, bottom: st.h }, t.surface);
    fill(hdc, &RECT { left: 0, top: 0, right: st.w, bottom: st.s(TITLE_H) }, t.title_bg);
    let font = crate::paint::ui_font(st.scale);
    let old = SelectObject(hdc, font.into());
    SetBkMode(hdc, TRANSPARENT);
    text(
        hdc,
        "批量重命名",
        RECT { left: st.s(14), top: 0, right: st.w - st.s(44), bottom: st.s(TITLE_H) },
        t.text,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    if st.hover == Hit::Close {
        fill(hdc, &st.close_rect(), t.danger);
    }
    let og = SelectObject(hdc, crate::paint::glyph_font(st.scale).into());
    text(hdc, "\u{E8BB}", st.close_rect(), t.text, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    SelectObject(hdc, og);

    // 头部：几个项目
    let ob = SelectObject(hdc, st.bold.into());
    text(
        hdc,
        &format!("将重命名 {} 个项目", st.items.len()),
        RECT { left: st.s(14), top: st.s(40), right: st.w - st.s(14), bottom: st.s(60) },
        t.text,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    SelectObject(hdc, ob);

    // 模式下拉：框 + 文字 + 向下的箭头（箭头是"这是下拉"的唯一提示）
    let mr = st.mode_rect();
    label(hdc, "模式", RECT { left: st.s(14), top: mr.top, right: st.s(90), bottom: mr.bottom }, t.text);
    let cb = RECT { left: mr.left + (mr.right - mr.left) / 2, top: mr.top, right: mr.right, bottom: mr.bottom };
    fill(hdc, &cb, if st.hover == Hit::Mode { t.hover_bg } else { t.surface_alt });
    for e in edges(&cb) {
        fill(hdc, &e, t.border);
    }
    label(hdc, MODE_LABELS[st.mode], RECT { left: mr.left + st.s(8), top: mr.top, right: cb.left, bottom: mr.bottom }, t.text);
    let og = SelectObject(hdc, crate::paint::glyph_font(st.scale).into());
    text(
        hdc,
        "\u{E70E}",
        RECT { left: cb.right - st.s(24), top: cb.top, right: cb.right, bottom: cb.bottom },
        t.text,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
    SelectObject(hdc, og);

    // 参数行的标签（EDIT 自己画内容，这里只补左边的字）
    let p1 = st.p1_rect();
    label(hdc, st.rule().param_labels()[0], RECT { left: st.s(14), top: p1.top, right: st.s(90), bottom: p1.bottom }, t.text);
    if st.rule().param_count() > 1 {
        let p2 = st.p2_rect();
        label(hdc, st.rule().param_labels()[1], RECT { left: st.s(14), top: p2.top, right: st.s(90), bottom: p2.bottom }, t.text);
    }

    // 高级选项
    text(
        hdc,
        "高级选项",
        RECT { left: st.s(14), top: st.s(166), right: st.w - st.s(14), bottom: st.s(184) },
        t.text_dim,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    let slots = st.slots();
    let mut any = false;
    for i in 0..4 {
        let r = st.slot_rect(i);
        match slots[i] {
            Slot::None => {}
            Slot::Check(name, on) => {
                any = true;
                if st.hover == Hit::Adv(i) {
                    fill(hdc, &r, t.hover_bg);
                }
                let bs = st.s(16);
                let cy = (r.top + r.bottom) / 2;
                let bx = RECT { left: r.left, top: cy - bs / 2, right: r.left + bs, bottom: cy - bs / 2 + bs };
                fill(hdc, &bx, if on { t.accent } else { t.surface_alt });
                for e in edges(&bx) {
                    fill(hdc, &e, t.border);
                }
                if on {
                    let og = SelectObject(hdc, crate::paint::glyph_font(st.scale).into());
                    text(hdc, "\u{E73E}", bx, t.selection_text, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
                    SelectObject(hdc, og);
                }
                label(
                    hdc,
                    name,
                    RECT { left: bx.right + st.s(8), top: r.top, right: r.right, bottom: r.bottom },
                    t.text,
                );
            }
            Slot::Num(name, _) => {
                any = true;
                // 数字框本身是 EDIT（在 slot 右半边），左边只放标签
                label(
                    hdc,
                    name,
                    RECT { left: r.left, top: r.top, right: r.left + st.s(80), bottom: r.bottom },
                    t.text,
                );
            }
        }
    }
    if !any {
        text(
            hdc,
            "（此模式没有额外选项）",
            RECT { left: st.s(94), top: st.s(188), right: st.w - st.s(14), bottom: st.s(214) },
            t.text_dim,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    }

    // 预览：旧名 -> 新名。撞名的一行标红，这是用户唯一能提前发现问题的地方
    let pr = st.prev_rect();
    fill(hdc, &pr, t.surface_alt);
    for e in edges(&pr) {
        fill(hdc, &e, t.border);
    }
    text(
        hdc,
        &format!("预览（{} 项将改变）", st.changed()),
        RECT { left: pr.left, top: pr.top - st.s(18), right: pr.right, bottom: pr.top },
        t.text_dim,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    let row_h = st.s(ROW_H);
    let rows = visible_rows(st);
    for k in 0..rows {
        let i = st.scroll as usize + k as usize;
        if i >= st.items.len() {
            break;
        }
        let top = pr.top + st.s(3) + k * row_h;
        let rr = RECT { left: pr.left + st.s(4), top, right: pr.right - st.s(4), bottom: top + row_h - st.s(2) };
        let changed = st.news[i] != st.old_name(i);
        let bad = st.conflict[i];
        let col = if bad {
            t.danger
        } else if changed {
            t.accent
        } else {
            t.text_dim
        };
        let mid = rr.left + (rr.right - rr.left) / 2;
        label(hdc, &st.old_name(i), RECT { left: rr.left, top: rr.top, right: mid - st.s(20), bottom: rr.bottom }, t.text_dim);
        let og = SelectObject(hdc, crate::paint::glyph_font(st.scale).into());
        text(hdc, "\u{E970}", RECT { left: mid - st.s(20), top: rr.top, right: mid, bottom: rr.bottom }, t.text_dim, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        SelectObject(hdc, og);
        label(hdc, &st.news[i], RECT { left: mid, top: rr.top, right: rr.right, bottom: rr.bottom }, col);
        if bad {
            label(hdc, "（重名）", RECT { left: rr.right - st.s(60), top: rr.top, right: rr.right, bottom: rr.bottom }, t.danger);
        }
    }

    // 按钮
    for (r, lbl, hit, accent) in [
        (st.ok_rect(), "重命名", Hit::Ok, true),
        (st.cancel_rect(), "取消", Hit::Cancel, false),
    ] {
        let enabled = if accent { st.can_apply() } else { true };
        fill(
            hdc,
            &r,
            if !enabled {
                t.surface_alt
            } else if st.hover == hit {
                t.hover_bg
            } else if accent {
                t.accent
            } else {
                t.surface_alt
            },
        );
        for e in edges(&r) {
            fill(hdc, &e, t.border);
        }
        text(
            hdc,
            lbl,
            r,
            if !enabled {
                t.text_dim
            } else if accent {
                t.selection_text
            } else {
                t.text
            },
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
    }

    SelectObject(hdc, old);
}

fn edges(r: &RECT) -> [RECT; 4] {
    [
        RECT { left: r.left, top: r.top, right: r.right, bottom: r.top + 1 },
        RECT { left: r.left, top: r.bottom - 1, right: r.right, bottom: r.bottom },
        RECT { left: r.left, top: r.top, right: r.left + 1, bottom: r.bottom },
        RECT { left: r.right - 1, top: r.top, right: r.right, bottom: r.bottom },
    ]
}

// ---------------------------------------------------------------- 模式下拉

const WM_COMBO_PICK: u32 = 0x8008;

struct Combo {
    owner: HWND,
    items: Vec<String>,
    current: usize,
    hover: Option<usize>,
    scale: f32,
    theme: Theme,
}

fn register_combo() {
    let cls = HSTRING::from("ZenFileRenameCombo");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(combo_proc),
        lpszClassName: PCWSTR(cls.as_ptr()),
        ..Default::default()
    };
    unsafe {
        RegisterClassExW(&wc);
    }
}

fn open_combo(owner: HWND, items: Vec<String>, current: usize, x: i32, y: i32, w: i32, scale: f32, theme: Theme) {
    let row_h = (24.0 * scale).round() as i32;
    let h = row_h * items.len() as i32 + (8.0 * scale).round() as i32;
    let state = Box::into_raw(Box::new(Combo { owner, items, current, hover: None, scale, theme }));
    let cls = HSTRING::from("ZenFileRenameCombo");
    let hwnd = unsafe {
        CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WS_EX_TOPMOST,
            PCWSTR(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            x,
            y,
            w,
            h,
            Some(owner),
            None,
            None,
            Some(state as *const core::ffi::c_void),
        )
    };
    match hwnd {
        Ok(hw) => unsafe {
            let _ = SetWindowLongPtrW(hw, GWLP_USERDATA, state as isize);
            let _ = ShowWindow(hw, SW_SHOW);
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(hw);
            let _ = InvalidateRect(Some(hw), None, true);
        },
        Err(_) => {
            drop(unsafe { Box::from_raw(state) });
        }
    }
}

unsafe extern "system" fn combo_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Combo;
    if p.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let st = &mut *p;
    match msg {
        WM_ERASEBKGND => return LRESULT(1),
        WM_NCHITTEST => return LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut r = RECT::default();
            let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut r);
            let mem = CreateCompatibleDC(Some(hdc));
            let bmp = CreateCompatibleBitmap(hdc, r.right, r.bottom);
            let old = SelectObject(mem, bmp.into());
            let t = &st.theme;
            fill(mem, &r, t.surface_alt);
            for e in edges(&r) {
                fill(mem, &e, t.border);
            }
            let font = crate::paint::ui_font(st.scale);
            let of = SelectObject(mem, font.into());
            SetBkMode(mem, TRANSPARENT);
            let row_h = (24.0 * st.scale).round() as i32;
            let pad = (4.0 * st.scale).round() as i32;
            for (i, item) in st.items.clone().iter().enumerate() {
                let top = pad + i as i32 * row_h;
                let rr = RECT { left: 1, top, right: r.right - 1, bottom: top + row_h };
                if st.hover == Some(i) {
                    fill(mem, &rr, t.hover_bg);
                }
                text(
                    mem,
                    item,
                    RECT { left: rr.left + (10.0 * st.scale).round() as i32, top: rr.top, right: rr.right, bottom: rr.bottom },
                    t.text,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE,
                );
                if i == st.current {
                    let og = SelectObject(mem, crate::paint::glyph_font(st.scale).into());
                    text(
                        mem,
                        "\u{E73E}",
                        RECT { left: rr.right - (24.0 * st.scale).round() as i32, top: rr.top, right: rr.right, bottom: rr.bottom },
                        t.accent,
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                    );
                    SelectObject(mem, og);
                }
            }
            SelectObject(mem, of);
            let _ = BitBlt(hdc, 0, 0, r.right, r.bottom, Some(mem), 0, 0, SRCCOPY);
            SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
            let _ = EndPaint(hwnd, &mut ps);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let y = (lparam.0 >> 16) as i16 as i32;
            let row_h = (24.0 * st.scale).round() as i32;
            let pad = (4.0 * st.scale).round() as i32;
            let i = ((y - pad) / row_h) as usize;
            let h = if i < st.items.len() { Some(i) } else { None };
            if h != st.hover {
                st.hover = h;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let y = (lparam.0 >> 16) as i16 as i32;
            let row_h = (24.0 * st.scale).round() as i32;
            let pad = (4.0 * st.scale).round() as i32;
            let i = ((y - pad) / row_h) as usize;
            if i < st.items.len() {
                let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(st.owner),
                    WM_APP_PICK,
                    WPARAM(i),
                    LPARAM(0),
                );
                let _ = InvalidateRect(Some(st.owner), None, false);
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            drop(Box::from_raw(p));
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 给右键菜单用：把 MODE_LABELS 变成菜单条目。
pub fn menu_modes() -> &'static [&'static str; 5] {
    &MODE_LABELS
}

