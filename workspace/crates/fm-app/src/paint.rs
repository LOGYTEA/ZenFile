//! 自绘渲染与布局（技术方案 §4 自绘标题栏 / §5 虚拟化列表）。
//!
//! 全部内容绘制在一张内存位图上再一次性 BitBlt，避免大列表滚动时的撕裂。
//! 列表是虚拟化的：只为可见行请求数据与图标 index。

use windows::core::HSTRING;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreateSolidBrush, DeleteDC,
    DeleteObject, DrawTextW, FillRect, GetBkColor, InvalidateRect, SelectObject, SetBkMode,
    SetTextColor, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE,
    DT_VCENTER, HBRUSH, HDC,
    CreateCompatibleDC as _, DRAW_TEXT_FORMAT, FONT_CHARSET, FONT_CLIP_PRECISION,
    FONT_OUTPUT_PRECISION, FONT_QUALITY, HFONT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::UI::Controls::EM_SETSEL;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::Foundation::HINSTANCE;
use windows::Win32::UI::WindowsAndMessaging::HICON;

use crate::app::{human, App};
use crate::icons::wide;
use crate::theme::Metrics;

// ---------------------------------------------------------------- 字体

static mut UI_FONT: HFONT = HFONT(std::ptr::null_mut());
static mut UI_FONT_SCALE: f32 = 0.0;
static mut GLYPH_FONT: HFONT = HFONT(std::ptr::null_mut());
static mut HEADER_FONT: HFONT = HFONT(std::ptr::null_mut());

pub unsafe fn make_font(height: i32, weight: i32, face: &str) -> HFONT {
    CreateFontW(
        height,
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        FONT_CHARSET(1),
        FONT_OUTPUT_PRECISION(0),
        FONT_CLIP_PRECISION(0),
        FONT_QUALITY(5), // CLEARTYPE_QUALITY
        0,
        &HSTRING::from(face),
    )
}

pub fn ui_font(scale: f32) -> HFONT {
    unsafe {
        if UI_FONT.0.is_null() || (UI_FONT_SCALE - scale).abs() > 0.01 {
            if !UI_FONT.0.is_null() {
                let _ = DeleteObject(UI_FONT.into());
            }
            UI_FONT = make_font((-13.0 * scale).round() as i32, 400, "Microsoft YaHei UI");
            UI_FONT_SCALE = scale;
        }
        UI_FONT
    }
}

pub fn header_font(scale: f32) -> HFONT {
    unsafe {
        if HEADER_FONT.0.is_null() {
            HEADER_FONT = make_font((-12.0 * scale).round() as i32, 600, "Microsoft YaHei UI");
        }
        HEADER_FONT
    }
}

pub fn glyph_font(scale: f32) -> HFONT {
    unsafe {
        if GLYPH_FONT.0.is_null() {
            GLYPH_FONT = make_font((-14.0 * scale).round() as i32, 400, "Segoe MDL2 Assets");
        }
        GLYPH_FONT
    }
}

/// 菜单正文：比列表正文（-13）大一号。导航窗格放大到 -16 之后，菜单还停在 -13
/// 会显得明显小一号，两处放一起不协调。
static mut MENU_FONT: HFONT = HFONT(std::ptr::null_mut());
static mut MENU_FONT_SCALE: f32 = 0.0;

pub fn menu_font(scale: f32) -> HFONT {
    unsafe {
        if MENU_FONT.0.is_null() || (MENU_FONT_SCALE - scale).abs() > 0.01 {
            if !MENU_FONT.0.is_null() {
                let _ = DeleteObject(MENU_FONT.into());
            }
            MENU_FONT = make_font((-14.0 * scale).round() as i32, 400, "Microsoft YaHei UI");
            MENU_FONT_SCALE = scale;
        }
        MENU_FONT
    }
}

/// 导航窗格专用字体：比列表正文（-13）大一号，导航窗格本来就比内容区更"重"。
static mut SIDEBAR_FONT: HFONT = HFONT(std::ptr::null_mut());
static mut SIDEBAR_FONT_SCALE: f32 = 0.0;
/// 导航窗格里的字形（如"主页"）：图标格是 20px，字形字号要跟着放大才不显小。
static mut SIDEBAR_GLYPH_FONT: HFONT = HFONT(std::ptr::null_mut());
static mut SIDEBAR_GLYPH_SCALE: f32 = 0.0;

pub fn sidebar_glyph_font(scale: f32) -> HFONT {
    unsafe {
        if SIDEBAR_GLYPH_FONT.0.is_null() || (SIDEBAR_GLYPH_SCALE - scale).abs() > 0.01 {
            if !SIDEBAR_GLYPH_FONT.0.is_null() {
                let _ = DeleteObject(SIDEBAR_GLYPH_FONT.into());
            }
            SIDEBAR_GLYPH_FONT = make_font((-19.0 * scale).round() as i32, 400, "Segoe MDL2 Assets");
            SIDEBAR_GLYPH_SCALE = scale;
        }
        SIDEBAR_GLYPH_FONT
    }
}

pub fn sidebar_font(scale: f32) -> HFONT {
    unsafe {
        if SIDEBAR_FONT.0.is_null() || (SIDEBAR_FONT_SCALE - scale).abs() > 0.01 {
            if !SIDEBAR_FONT.0.is_null() {
                let _ = DeleteObject(SIDEBAR_FONT.into());
            }
            SIDEBAR_FONT = make_font((-16.0 * scale).round() as i32, 400, "Microsoft YaHei UI");
            SIDEBAR_FONT_SCALE = scale;
        }
        SIDEBAR_FONT
    }
}

/// 导航窗格行高。**绘制与点击命中必须共用这一个值**，否则"看得见的行"
/// 和"点得到的行"会错位（之前两边各写一份 22，改字体后就对不上了）。
pub fn sidebar_row_h(scale: f32) -> i32 {
    (34.0 * scale).round() as i32
}

/// 导航窗格条目几何：返回 `(该行矩形, 图标矩形, 文字矩形)`。
pub fn sidebar_row_rects(l: &Layout, index: usize, scale: f32) -> (RECT, RECT, RECT) {
    let row = sidebar_row_h(scale);
    let top = l.sidebar.top + row + index as i32 * row;
    let row_r = RECT { left: l.sidebar.left, top, right: l.sidebar.right, bottom: top + row };
    let icon_s = (20.0 * scale).round() as i32;
    let icon_x = l.sidebar.left + (14.0 * scale).round() as i32;
    let icon_y = top + (row - icon_s) / 2;
    let icon_r = RECT { left: icon_x, top: icon_y, right: icon_x + icon_s, bottom: icon_y + icon_s };
    let text_r = RECT {
        left: icon_x + icon_s + (12.0 * scale).round() as i32,
        top,
        right: l.sidebar.right - (6.0 * scale).round() as i32,
        bottom: top + row,
    };
    (row_r, icon_r, text_r)
}

// ---------------------------------------------------------------- 布局

#[derive(Clone, Copy)]
pub struct Layout {
    pub title: RECT,
    pub toolbar: RECT,
    pub sidebar: RECT,
    pub view: RECT,
    pub status: RECT,
    pub back: RECT,
    pub forward: RECT,
    pub up: RECT,
    pub address: RECT,
    pub search: RECT,
    pub min_btn: RECT,
    pub max_btn: RECT,
    pub close_btn: RECT,
    /// 最小化左侧的"更多菜单"按钮
    pub menu_btn: RECT,
    /// 标题栏最左侧的侧栏折叠开关
    pub sidebar_toggle: RECT,
    pub tabs: RECT,
}

pub const TAB_W: i32 = 180;

pub fn compute_layout(w: i32, h: i32, m: &Metrics, sidebar: bool, status: bool) -> Layout {
    let title_h = m.title_h;
    let toolbar_h = m.toolbar_h;
    let status_h = if status { m.status_h } else { 0 };
    let sidebar_w = if sidebar { m.sidebar_w } else { 0 };

    let pad = (6.0 * m.scale).round() as i32;
    let nav = (30.0 * m.scale).round() as i32;
    let top = title_h;
    let tb_top = top + toolbar_h;
    let view_top = tb_top;
    let view_bottom = h - status_h;
    let search_w = (170.0 * m.scale).round() as i32;

    let btn_w = (44.0 * m.scale).round() as i32;
    let toggle_w = (40.0 * m.scale).round() as i32;
    Layout {
        title: RECT { left: 0, top: 0, right: w, bottom: title_h },
        tabs: RECT { left: toggle_w, top: 0, right: w - btn_w * 4, bottom: title_h },
        sidebar_toggle: RECT { left: 0, top: 0, right: toggle_w, bottom: title_h },
        menu_btn: RECT { left: w - btn_w * 4, top: 0, right: w - btn_w * 3, bottom: title_h },
        toolbar: RECT { left: 0, top, right: w, bottom: tb_top },
        sidebar: RECT { left: 0, top: view_top, right: sidebar_w, bottom: view_bottom },
        view: RECT { left: sidebar_w, top: view_top, right: w, bottom: view_bottom },
        status: RECT { left: 0, top: view_bottom, right: w, bottom: h },
        back: RECT { left: pad, top: top + pad, right: pad + nav, bottom: tb_top - pad },
        forward: RECT { left: pad + nav + 2, top: top + pad, right: pad + nav * 2 + 2, bottom: tb_top - pad },
        up: RECT { left: pad + nav * 2 + 4, top: top + pad, right: pad + nav * 3 + 4, bottom: tb_top - pad },
        address: RECT {
            left: pad + nav * 3 + 10,
            top: top + pad,
            right: w - pad - search_w - 8,
            bottom: tb_top - pad,
        },
        search: RECT { left: w - pad - search_w, top: top + pad, right: w - pad, bottom: tb_top - pad },
        min_btn: RECT { left: w - btn_w * 3, top: 0, right: w - btn_w * 2, bottom: title_h },
        max_btn: RECT { left: w - btn_w * 2, top: 0, right: w - btn_w, bottom: title_h },
        close_btn: RECT { left: w - btn_w, top: 0, right: w, bottom: title_h },
    }
}

impl Layout {
    pub fn tab_rect(&self, index: usize, count: usize, m: &Metrics) -> RECT {
        let available = (self.tabs.right - self.tabs.left).max(TAB_W);
        let base = TAB_W.min((available / count.max(1) as i32).max(90));
        let width = (base as f32 * m.scale).round() as i32;
        RECT {
            left: self.tabs.left + index as i32 * width,
            top: self.tabs.top + (4.0 * m.scale) as i32,
            right: self.tabs.left + (index + 1) as i32 * width - (2.0 * m.scale) as i32,
            bottom: self.tabs.bottom - (2.0 * m.scale) as i32,
        }
    }

    /// 最后一个标签右侧的"新建标签"按钮（+）。宽度固定，不随标签数伸缩。
    pub fn new_tab_rect(&self, count: usize, m: &Metrics) -> RECT {
        let w = (30.0 * m.scale).round() as i32;
        let left = if count == 0 {
            self.tabs.left
        } else {
            self.tab_rect(count - 1, count, m).right + (2.0 * m.scale) as i32
        };
        RECT {
            left,
            top: self.tabs.top + (4.0 * m.scale) as i32,
            right: left + w,
            bottom: self.tabs.bottom - (2.0 * m.scale) as i32,
        }
    }
}

// ---------------------------------------------------------------- 绘制

pub fn paint(app: &mut App, hdc: HDC, width: i32, height: i32) {
    unsafe {
        let mem = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, width, height);
        let old_bmp = SelectObject(mem, bmp.into());
        let l = compute_layout(
            width,
            height,
            &app.metrics,
            app.cfg.ui.sidebar_visible,
            app.cfg.ui.statusbar_visible,
        );
        let t = app.theme;
        let bg = CreateSolidBrush(t.window_bg);
        let all = RECT { left: 0, top: 0, right: width, bottom: height };
        FillRect(mem, &all, bg);
        let _ = DeleteObject(bg.into());

        paint_title_bar(app, mem, &l);
        paint_toolbar(app, mem, &l);
        if app.cfg.ui.sidebar_visible {
            paint_sidebar(app, mem, &l);
        }
        paint_view(app, mem, &l);
        if app.cfg.ui.statusbar_visible {
            paint_status(app, mem, &l);
        }

        let _ = BitBlt(hdc, 0, 0, width, height, Some(mem), 0, 0, SRCCOPY);
        SelectObject(mem, old_bmp);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
    }
}

fn paint_title_bar(app: &App, hdc: HDC, l: &Layout) {
    let t = &app.theme;
    unsafe {
        let brush = CreateSolidBrush(t.title_bg);
        FillRect(hdc, &l.title, brush);
        let _ = DeleteObject(brush.into());
        let font = ui_font(app.metrics.scale);
        let old = SelectObject(hdc, font.into());
        SetBkMode(hdc, TRANSPARENT);

        // 标签页
        for (i, tab) in app.tabs.iter().enumerate() {
            let r = l.tab_rect(i, app.tabs.len(), &app.metrics);
            let (bg, fg) = if i == app.active {
                (t.tab_active, t.text)
            } else {
                (t.chrome_bg, t.text_dim)
            };
            let b = CreateSolidBrush(bg);
            FillRect(hdc, &r, b);
            let _ = DeleteObject(b.into());
            SetTextColor(hdc, fg);
            draw_text(hdc, &tab.title, pad_rect_width(&r, 10), DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            if i == app.active {
                let accent = CreateSolidBrush(t.accent);
                let line = RECT { left: r.left, top: r.bottom - 2, right: r.right, bottom: r.bottom };
                FillRect(hdc, &line, accent);
                let _ = DeleteObject(accent.into());
            }
        }

        // 新建标签（+）：紧跟最后一个标签
        let plus = l.new_tab_rect(app.tabs.len(), &app.metrics);
        if plus.right < l.tabs.right {
            draw_chrome_button(app, hdc, &plus, "\u{E710}", false); // Add
        }
        // 侧栏折叠开关（最左）与更多菜单（最小化左侧）
        draw_chrome_button(
            app,
            hdc,
            &l.sidebar_toggle,
            "\u{E700}", // GlobalNavButton
            app.hover_chrome & crate::app::HOVER_SIDEBAR_TOGGLE != 0,
        );
        draw_chrome_button(
            app,
            hdc,
            &l.menu_btn,
            "\u{E712}", // More
            app.hover_chrome & crate::app::HOVER_MENU_BTN != 0,
        );
        // 标题按钮（自绘，命中由 WM_NCHITTEST 交给系统处理 maximize/minimize 动画）
        draw_caption_button(app, hdc, &l.min_btn, "\u{E921}");
        draw_caption_button(app, hdc, &l.max_btn, "\u{E922}");
        let close = CreateSolidBrush(if app.hovering_close { t.danger } else { t.title_bg });
        FillRect(hdc, &l.close_btn, close);
        let _ = DeleteObject(close.into());
        SetTextColor(hdc, t.text);
        let g = glyph_font(app.metrics.scale);
        let old_g = SelectObject(hdc, g.into());
        draw_text(hdc, "\u{E8BB}", l.close_btn, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_CENTER);
        SelectObject(hdc, old_g);
        SelectObject(hdc, old);
    }
}

fn pad_rect_width(r: &RECT, pad: i32) -> RECT {
    RECT { left: r.left + pad, top: r.top, right: r.right - pad, bottom: r.bottom }
}

fn draw_caption_button(app: &App, hdc: HDC, r: &RECT, glyph: &str) {
    let t = &app.theme;
    unsafe {
        let g = glyph_font(app.metrics.scale);
        let old = SelectObject(hdc, g.into());
        SetTextColor(hdc, t.text);
        draw_text(hdc, glyph, *r, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_CENTER);
        SelectObject(hdc, old);
    }
}

/// 标题栏自有按钮（侧栏开关 / 更多菜单）：带悬停底色，观感对齐资源管理器。
fn draw_chrome_button(app: &App, hdc: HDC, r: &RECT, glyph: &str, hover: bool) {
    let t = &app.theme;
    unsafe {
        let bg = CreateSolidBrush(if hover { t.hover_bg } else { t.chrome_bg });
        FillRect(hdc, r, bg);
        let _ = DeleteObject(bg.into());
        let g = glyph_font(app.metrics.scale);
        let old = SelectObject(hdc, g.into());
        SetTextColor(hdc, t.text);
        draw_text(hdc, glyph, *r, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_CENTER);
        SelectObject(hdc, old);
    }
}

/// 文本绘制统一入口：内部完成 UTF-16 转换。
///
/// 注意：传给 DrawTextW 的切片**必须去掉结尾的 NUL**。windows-rs 用切片长度
/// 作为 cchText，若把结尾 `\0` 也算进去，U+0000 会被当成一个字符画出来，
/// 字体把它渲染成 .notdef 空心方框 —— 表现就是"每个图标右边多一个小方块"。
pub fn draw_text(hdc: HDC, text: &str, mut rect: RECT, flags: DRAW_TEXT_FORMAT) {
    let mut buf: Vec<u16> = text.encode_utf16().collect();
    if buf.is_empty() {
        return;
    }
    unsafe {
        let _ = DrawTextW(hdc, &mut buf, &mut rect, flags);
    }
}

fn paint_toolbar(app: &App, hdc: HDC, l: &Layout) {
    let t = &app.theme;
    unsafe {
        let brush = CreateSolidBrush(t.chrome_bg);
        FillRect(hdc, &l.toolbar, brush);
        let _ = DeleteObject(brush.into());
        let g = glyph_font(app.metrics.scale);
        let old_g = SelectObject(hdc, g.into());
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, if app.can_back() { t.text } else { t.text_dim });
        draw_text(hdc, "\u{E72B}", l.back, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_CENTER);
        SetTextColor(hdc, if app.can_forward() { t.text } else { t.text_dim });
        draw_text(hdc, "\u{E72A}", l.forward, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_CENTER);
        SetTextColor(hdc, t.text);
        draw_text(hdc, "\u{E74A}", l.up, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_CENTER);
        SelectObject(hdc, old_g);

        // 输入框底色 + 1px 边框：否则 EDIT 与工具栏同色，肉眼不可见
        for r in [&l.address, &l.search] {
            let bg = CreateSolidBrush(t.surface);
            FillRect(hdc, r, bg);
            let _ = DeleteObject(bg.into());
            let pf = CreateSolidBrush(t.border);
            let edges = [
                RECT { left: r.left, top: r.top, right: r.right, bottom: r.top + 1 },
                RECT { left: r.left, top: r.bottom - 1, right: r.right, bottom: r.bottom },
                RECT { left: r.left, top: r.top, right: r.left + 1, bottom: r.bottom },
                RECT { left: r.right - 1, top: r.top, right: r.right, bottom: r.bottom },
            ];
            for e in &edges {
                FillRect(hdc, e, pf);
            }
            let _ = DeleteObject(pf.into());
        }

        // 工具栏下分隔线，否则与列表区糊成一片
        let lb = CreateSolidBrush(t.border);
        FillRect(hdc, &RECT { left: 0, top: l.toolbar.bottom - 1, right: l.toolbar.right, bottom: l.toolbar.bottom }, lb);
        let _ = DeleteObject(lb.into());
    }
}

fn paint_sidebar(app: &mut App, hdc: HDC, l: &Layout) {
    let t = app.theme;
    let scale = app.metrics.scale;
    unsafe {
        let brush = CreateSolidBrush(t.chrome_bg);
        FillRect(hdc, &l.sidebar, brush);
        let _ = DeleteObject(brush.into());

        // 右分隔线：导航窗格和内容区之间要有明确边界
        let lb = CreateSolidBrush(t.border);
        FillRect(
            hdc,
            &RECT { left: l.sidebar.right - 1, top: l.sidebar.top, right: l.sidebar.right, bottom: l.sidebar.bottom },
            lb,
        );
        let _ = DeleteObject(lb.into());

        let font = sidebar_font(scale);
        let old = SelectObject(hdc, font.into());
        SetBkMode(hdc, TRANSPARENT);

        let entries = sidebar_entries();
        let cur = std::fs::canonicalize(&app.tab().path).unwrap_or_default();
        // 先把每行的图标句柄取好再画：dir_icon 需要 &mut，不能一边借 entries 一边改缓存
        let mut icons: Vec<Option<HICON>> = Vec::with_capacity(entries.len());
        for (_, target) in &entries {
            let p = std::path::PathBuf::from(target);
            icons.push(if is_home_target(target) {
                None // "主页"是虚拟项，用字形代替
            } else {
                let h = app.icons.dir_icon(&p, true);
                if h.0.is_null() { None } else { Some(h) }
            });
        }

        for (i, (label, target)) in entries.iter().enumerate() {
            let (row_r, icon_r, text_r) = sidebar_row_rects(l, i, scale);
            if row_r.top >= l.sidebar.bottom {
                break;
            }
            // "主页"是虚拟项（shell:Home），没有真实路径，只能单独判定；
            // 否则当前停在首页时导航窗格一格都不亮。
            let active = if app.tab().home {
                is_home_target(target)
            } else {
                !cur.as_os_str().is_empty()
                    && std::fs::canonicalize(std::path::PathBuf::from(target))
                        .map(|p| p == cur)
                        .unwrap_or(false)
            };
            if active {
                let hb = CreateSolidBrush(t.hover_bg);
                FillRect(hdc, &row_r, hb);
                let _ = DeleteObject(hb.into());
                // 左侧 3px 强调条：和菜单悬停保持一致的语言，指示"当前位置"
                let ab = CreateSolidBrush(t.accent);
                FillRect(hdc, &RECT { left: row_r.left, top: row_r.top, right: row_r.left + (3.0 * scale) as i32, bottom: row_r.bottom }, ab);
                let _ = DeleteObject(ab.into());
            }
            SetTextColor(hdc, if active { t.text } else { t.text_dim });

            match icons[i] {
                Some(h) => {
                    app.icons.draw(hdc, h, icon_r.left, icon_r.top, icon_r.right - icon_r.left);
                }
                None => {
                    // 虚拟项（主页）用 Segoe MDL2 字形 \u{E80F} Home 代替图标
                    let g = sidebar_glyph_font(scale);
                    let old2 = SelectObject(hdc, g.into());
                    let mut gbuf: Vec<u16> = "\u{E80F}".encode_utf16().collect();
                    DrawTextW(
                        hdc,
                        &mut gbuf,
                        &mut icon_r.clone(),
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                    );
                    SelectObject(hdc, old2);
                }
            }
            draw_text(hdc, label, text_r, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        }
        SelectObject(hdc, old);
    }
}

/// 该导航项是不是虚拟的"主页"（没有真实路径可问 Shell 要图标）。
pub fn is_home_target(target: &str) -> bool {
    target == crate::app::HOME_PATH
}

/// 导航窗格条目。第一项必须是"主页"：与资源管理器一致，点它进首页
/// （常用文件夹 + 本机驱动器），而不是直接落进 C 盘。
pub fn sidebar_entries() -> Vec<(&'static str, String)> {
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    vec![
        ("主页", crate::app::HOME_PATH.to_string()),
        ("桌面", format!("{home}\\Desktop")),
        ("下载", format!("{home}\\Downloads")),
        ("文档", format!("{home}\\Documents")),
        ("图片", format!("{home}\\Pictures")),
    ]
}

// ---------------------------------------------------------------- 首页平铺布局

/// 首页一块磁贴的几何信息。绘制与鼠标命中判定共用同一份计算结果，
/// 保证"看得见的位置"和"点得到的位置"永远一致。
pub struct HomeTile {
    /// 命中矩形
    pub rect: RECT,
    pub icon_x: i32,
    pub icon_y: i32,
    pub icon_size: i32,
    /// 标题文字区
    pub label: RECT,
    /// 驱动器容量条；文件夹磁贴为 None
    pub bar: Option<RECT>,
    /// 对应 tab.entries 的下标
    pub index: usize,
}

/// 首页分区：`k` 是 tag0/<ratios 数组长度，这里返回（标题, 起始 y, 高度）
pub struct HomeGroup {
    pub title: &'static str,
    pub top: i32,
    pub bottom: i32,
}

const QUICK_TILE_W: i32 = 200;
const QUICK_TILE_H: i32 = 46;
const DRIVE_TILE_W: i32 = 250;
const DRIVE_TILE_H: i32 = 86;
const TILE_GAP: i32 = 10;
const HOME_PAD: i32 = 22;

fn grid_columns(avail: i32, tile_w: i32) -> i32 {
    ((avail + TILE_GAP) / (tile_w + TILE_GAP)).max(1)
}

/// 计算首页全部磁贴与分区。纯函数：同样的入参必然得到同样的布局。
pub fn home_layout(app: &App, view: &RECT) -> (Vec<HomeGroup>, Vec<HomeTile>) {
    let s = app.metrics.scale;
    let gap = (TILE_GAP as f32 * s).round() as i32;
    let pad = (HOME_PAD as f32 * s).round() as i32;
    let avail = (view.right - view.left - pad * 2).max(80);

    let quick_w = (QUICK_TILE_W as f32 * s).round() as i32;
    let quick_h = (QUICK_TILE_H as f32 * s).round() as i32;
    let drive_w = (DRIVE_TILE_W as f32 * s).round() as i32;
    let drive_h = (DRIVE_TILE_H as f32 * s).round() as i32;

    let entries = &app.tab().entries;
    let mut tiles = Vec::new();
    let mut groups = Vec::new();

    let header_h = (34.0 * s).round() as i32;
    let mut y = view.top + (10.0 * s).round() as i32;

    // 第一段：常用文件夹（非盘符根）
    let mut i = 0usize;
    while i < entries.len() && !fm_core::paths::is_drive_root(&entries[i].path) {
        i += 1;
    }
    let (quick, drives) = entries.split_at(i);

    groups.push(HomeGroup { title: "文件夹", top: y, bottom: y });
    y += header_h;
    let cols = grid_columns(avail, quick_w);
    let start = y;
    for (n, _) in quick.iter().enumerate() {
        let col = (n as i32) % cols;
        let row = (n as i32) / cols;
        let left = view.left + pad + col * (quick_w + gap);
        let top = start + row * (quick_h + gap);
        let rect = RECT { left, top, right: left + quick_w, bottom: top + quick_h };
        let icon_size = (32.0 * s).round() as i32;
        tiles.push(HomeTile {
            rect,
            icon_x: left + (8.0 * s).round() as i32,
            icon_y: top + (quick_h - icon_size) / 2,
            icon_size,
            label: RECT {
                left: left + icon_size + (16.0 * s).round() as i32,
                top,
                right: rect.right - (6.0 * s).round() as i32,
                bottom: top + quick_h,
            },
            bar: None,
            index: n,
        });
    }
    let rows = ((quick.len() as i32 + cols - 1) / cols).max(1);
    y = start + rows * (quick_h + gap) + (14.0 * s).round() as i32;
    if let Some(g) = groups.last_mut() {
        g.bottom = y;
    }

    // 第二段：设备和驱动器
    if !drives.is_empty() {
        groups.push(HomeGroup { title: "设备和驱动器", top: y, bottom: y });
        y += header_h;
        let dcols = grid_columns(avail, drive_w);
        let start = y;
        for (n, _) in drives.iter().enumerate() {
            let col = (n as i32) % dcols;
            let row = (n as i32) / dcols;
            let left = view.left + pad + col * (drive_w + gap);
            let top = start + row * (drive_h + gap);
            let rect = RECT { left, top, right: left + drive_w, bottom: top + drive_h };
            let icon_size = (32.0 * s).round() as i32;
            let bar_h = (6.0 * s).round().max(3.0) as i32;
            let bar = RECT {
                left: left + (10.0 * s).round() as i32,
                top: rect.bottom - bar_h - (10.0 * s).round() as i32,
                right: rect.right - (10.0 * s).round() as i32,
                bottom: rect.bottom - (10.0 * s).round() as i32,
            };
            tiles.push(HomeTile {
                rect,
                icon_x: left + (10.0 * s).round() as i32,
                icon_y: top + (12.0 * s).round() as i32,
                icon_size,
                label: RECT {
                    left: left + icon_size + (16.0 * s).round() as i32,
                    top: top + (6.0 * s).round() as i32,
                    right: rect.right - (8.0 * s).round() as i32,
                    bottom: bar.top - 2,
                },
                bar: Some(bar),
                index: i + n,
            });
        }
        let drows = ((drives.len() as i32 + dcols - 1) / dcols).max(1);
        y = start + drows * (drive_h + gap);
        if let Some(g) = groups.last_mut() {
            g.bottom = y;
        }
    }

    (groups, tiles)
}

/// 首页命中测试：返回被点中的磁贴在 `tiles` 里的下标。
pub fn home_hit(tiles: &[HomeTile], x: i32, y: i32) -> Option<usize> {
    tiles.iter().position(|t| {
        x >= t.rect.left && x < t.rect.right && y >= t.rect.top && y < t.rect.bottom
    })
}

fn paint_home(app: &mut App, hdc: HDC, l: &Layout) {
    let t = app.theme;
    let scale = app.metrics.scale;
    unsafe {
        let brush = CreateSolidBrush(t.window_bg);
        FillRect(hdc, &l.view, brush);
        let _ = DeleteObject(brush.into());

        let (groups, tiles) = home_layout(&*app, &l.view);
        if tiles.is_empty() {
            SetTextColor(hdc, t.text_dim);
            let font = ui_font(scale);
            let old = SelectObject(hdc, font.into());
            draw_text(hdc, "此文件夹为空", l.view, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
            SelectObject(hdc, old);
            return;
        }

        // 先把图标句柄与选中态取好，避免绘制期间反复借用 App
        let mut icons: Vec<HICON> = Vec::with_capacity(tiles.len());
        let mut selected: Vec<bool> = Vec::with_capacity(tiles.len());
        {
            for tile in &tiles {
                // 先在不可变借用期内把需要的字段拷出来，再取 mut 借用的 icons
                let Some((is_dir, ext, key)) = app.tab().entries.get(tile.index).map(|e| {
                    let ext = e
                        .path
                        .extension()
                        .map(|x| format!(".{}", x.to_string_lossy()))
                        .unwrap_or_default();
                    (e.is_dir(), ext, crate::app::key_of(e))
                }) else {
                    icons.push(HICON(std::ptr::null_mut()));
                    selected.push(false);
                    continue;
                };
                // 目录走 dir_icon：优先 desktop.ini 的自定义图标，否则通用文件夹图标
                icons.push(if is_dir {
                    let p = app.tab().entries[tile.index].path.clone();
                    app.icons.dir_icon(&p, true)
                } else {
                    app.icons.icon(false, &ext, true)
                }); // 磁贴用 32×32
                selected.push(app.tab().selected.contains(&key));
            }
        }
        let focus_row = app.tab().focus_row;

        let font = ui_font(scale);
        let old_font = SelectObject(hdc, font.into());
        SetBkMode(hdc, TRANSPARENT);

        for g in &groups {
            let hf = header_font(scale);
            let old = SelectObject(hdc, hf.into());
            SetTextColor(hdc, t.text_dim);
            draw_text(
                hdc,
                g.title,
                RECT { left: l.view.left + (HOME_PAD as f32 * scale) as i32, top: g.top, right: l.view.right, bottom: g.top + (30.0 * scale) as i32 },
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(hdc, old);
        }

        for (n, tile) in tiles.iter().enumerate() {
            let sel = selected[n];
            // 磁贴底色：选中/聚焦时给一层淡淡的高亮，和列表行保持一致的手感。
            // 聚焦同样受 focus_visible 约束，首页一打开第 0 块磁贴不该是亮的。
            if sel || (app.focus_visible && n == focus_row) {
                let b = CreateSolidBrush(if sel { t.selection_bg } else { t.hover_bg });
                FillRect(hdc, &tile.rect, b);
                let _ = DeleteObject(b.into());
            }
            SetTextColor(hdc, if sel { t.selection_text } else { t.text });
            app.icons.draw(hdc, icons[n], tile.icon_x, tile.icon_y, tile.icon_size);
            let Some(e) = app.tab().entries.get(tile.index) else { continue };

            if let Some(bar) = tile.bar {
                // 驱动磁贴：名称 + 容量条 + 可用/总量
                let mut name: Vec<u16> = e.name.encode_utf16().collect();
                let mut nr = tile.label;
                nr.bottom = nr.top + (20.0 * scale) as i32;
                DrawTextW(hdc, &mut name, &mut nr, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);

                let text = match crate::app::drive_space_of(app, &e.path) {
                    Some((total, free)) => format!("{} 可用，共 {}", human(free), human(total)),
                    None => "容量未知".to_string(),
                };
                let mut sub: Vec<u16> = text.encode_utf16().collect();
                SetTextColor(hdc, if sel { t.selection_text } else { t.text_dim });
                let mut sr = tile.label;
                sr.top = nr.bottom;
                sr.bottom = bar.top - 2;
                DrawTextW(hdc, &mut sub, &mut sr, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX);

                // 容量条：底色 + 已用占比
                let bg = CreateSolidBrush(t.surface_alt);
                FillRect(hdc, &bar, bg);
                let _ = DeleteObject(bg.into());
                if let Some((total, free)) = crate::app::drive_space_of(app, &e.path) {
                    if total > 0 {
                        let used = total.saturating_sub(free);
                        let w = ((bar.right - bar.left) as f64 * (used as f64 / total as f64)) as i32;
                        if w > 0 {
                            let fill = RECT { right: bar.left + w, ..bar };
                            let fb = CreateSolidBrush(t.accent);
                            FillRect(hdc, &fill, fb);
                            let _ = DeleteObject(fb.into());
                        }
                    }
                }
            } else {
                let mut name: Vec<u16> = e.name.encode_utf16().collect();
                let mut lr = tile.label;
                DrawTextW(hdc, &mut name, &mut lr, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
            }
        }
        SelectObject(hdc, old_font);
    }
}

fn paint_view(app: &mut App, hdc: HDC, l: &Layout) {
    if app.tab().home {
        paint_home(app, hdc, l);
        return;
    }
    let t = app.theme;
    let mode = app.tab().view_mode;
    let row_h = app.metrics.row_height_for(mode);
    unsafe {
        let brush = CreateSolidBrush(t.window_bg);
        FillRect(hdc, &l.view, brush);
        let _ = DeleteObject(brush.into());

        let font = ui_font(app.metrics.scale);
        let old_font = SelectObject(hdc, font.into());
        SetBkMode(hdc, TRANSPARENT);

        // 列原点：表头与行共用同一组 x，保证竖向对齐。
        // 注意所有列 x 都必须以 l.view.left 为基准 —— 之前行内用的是相对
        // 偏移 text_pad，导致文件名被画到侧边栏上。
        // 列几何照抄资源管理器实测值（窗口 1123 宽时列分隔线在 431/575/695/775，
        // 内容区从 157 起）→ 相对宽度 274 / 144 / 120 / 80，图标左边距 21。
        let text_pad = (21.0 * app.metrics.scale) as i32;
        let icon_gap = (3.0 * app.metrics.scale) as i32;
        let large_icon = matches!(mode, fm_core::ViewMode::MediumIcons | fm_core::ViewMode::LargeIcons);
        let icon_size = if large_icon {
            if mode == fm_core::ViewMode::LargeIcons {
                (96.0 * app.metrics.scale) as i32
            } else {
                (48.0 * app.metrics.scale) as i32
            }
        } else {
            (16.0 * app.metrics.scale) as i32
        };
        let col_name = l.view.left + text_pad + icon_size + icon_gap;
        let col_date = col_name + (274.0 * app.metrics.scale) as i32;
        let col_type = col_date + (144.0 * app.metrics.scale) as i32;
        let col_size = col_type + (120.0 * app.metrics.scale) as i32;

        // 表头（详细信息视图）
        let header_h = (26.0 * app.metrics.scale) as i32;
        if mode == fm_core::ViewMode::Details {
            let sort = app.tab().sort;
            let hf = header_font(app.metrics.scale);
            let old = SelectObject(hdc, hf.into());
            for (label, key, x0, w) in [
                ("名称", fm_core::SortKey::Name, col_name, (274.0 * app.metrics.scale) as i32),
                ("修改日期", fm_core::SortKey::Modified, col_date, (144.0 * app.metrics.scale) as i32),
                ("类型", fm_core::SortKey::Type, col_type, (120.0 * app.metrics.scale) as i32),
                ("大小", fm_core::SortKey::Size, col_size, (80.0 * app.metrics.scale) as i32),
            ] {
                // 排序指示：当前排序列标出方向，否则点了看不出按哪列排
                let text = if sort.key == key {
                    format!("{label} {}", if sort.descending { "\u{25BC}" } else { "\u{25B2}" })
                } else {
                    label.to_string()
                };
                let r = RECT { left: x0, top: l.view.top, right: x0 + w, bottom: l.view.top + header_h };
                SetTextColor(hdc, if sort.key == key { t.text } else { t.text_dim });
                draw_text(hdc, &text, r, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            }
            SetTextColor(hdc, t.text_dim);
            SelectObject(hdc, old);
        }

        let visible: Vec<usize> = app.tab().visible.clone();
        let list_top = l.view.top + if mode == fm_core::ViewMode::Details { header_h } else { 0 };
        let view_h = l.view.bottom - list_top;
        let scroll = app.tab().scroll;
        let first = (scroll / row_h) as usize;
        let rows_fit = (view_h / row_h + 2) as usize;

        // 只为可见行解析 icon index（独占借用 icons），之后再取 tab 的只读引用绘制
        let mut icon_handles: Vec<HICON> = Vec::with_capacity(rows_fit);
        for i in first..(first + rows_fit).min(visible.len()) {
            let index = visible[i];
            let Some(e) = app.tab().entries.get(index) else {
                icon_handles.push(HICON(std::ptr::null_mut()));
                continue;
            };
            let ext = e.path.extension().map(|x| format!(".{}", x.to_string_lossy())).unwrap_or_default();
            // 目录优先用 desktop.ini 指定的自定义图标（§12：无自定义才用通用图标）
            let big = icon_size > 24;
            icon_handles.push(if e.is_dir() {
                let p = e.path.clone();
                app.icons.dir_icon(&p, big)
            } else {
                app.icons.icon(false, &ext, big)
            });
        }

        let tab = app.tab();
        for i in first..(first + rows_fit).min(visible.len()) {
            let index = visible[i];
            let Some(entry) = tab.entries.get(index) else { continue };
            let y = list_top + i as i32 * row_h - scroll;
            if y > l.view.bottom || y + row_h < list_top {
                continue;
            }
            let r = RECT { left: l.view.left, top: y, right: l.view.right, bottom: y + row_h };
            let selected = tab.selected.contains(&crate::app::key_of(entry));
            // 焦点行只在用户真的交互过之后才画出来：否则每次打开文件夹，
            // 第 0 行都顶着一条高亮，看着像"默认就勾选了第一行"。
            let focused = app.focus_visible && i == tab.focus_row;
            if selected || focused {
                let fill = CreateSolidBrush(if selected { t.selection_bg } else { t.hover_bg });
                FillRect(hdc, &r, fill);
                let _ = DeleteObject(fill.into());
            }
            SetTextColor(hdc, if selected { t.selection_text } else { t.text });

            let icon = icon_handles.get(i - first).copied().unwrap_or(HICON(std::ptr::null_mut()));
            if mode == fm_core::ViewMode::Details {
                app.icons.draw(hdc, icon, col_name - icon_size - icon_gap, y + (row_h - icon_size) / 2, icon_size);
                let name_r = RECT { left: col_name, top: y, right: col_date, bottom: y + row_h };
                draw_text(hdc, &entry.name, name_r, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
                SetTextColor(hdc, if selected { t.selection_text } else { t.text_dim });
                draw_text(hdc, &format_date(entry.modified), RECT { left: col_date, top: y, right: col_type, bottom: y + row_h }, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
                draw_text(hdc, &entry.type_label(), RECT { left: col_type, top: y, right: col_size, bottom: y + row_h }, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
                draw_text(hdc, &size_of(&entry), RECT { left: col_size, top: y, right: l.view.right, bottom: y + row_h }, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
            } else if mode == fm_core::ViewMode::List {
                let x = l.view.left + text_pad;
                app.icons.draw(hdc, icon, x, y + (row_h - icon_size) / 2, icon_size);
                let name_r = RECT { left: x + icon_size + 8, top: y, right: l.view.right, bottom: y + row_h };
                draw_text(hdc, &entry.name, name_r, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            } else {
                let cx = r.left + (r.right - r.left) / 2 - icon_size / 2;
                app.icons.draw(hdc, icon, cx, y + (8.0 * app.metrics.scale) as i32, icon_size);
                let name_r = RECT { left: r.left + 4, top: y + icon_size + (4.0 * app.metrics.scale) as i32, right: r.right - 4, bottom: y + row_h };
                draw_text(hdc, &entry.name, name_r, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_CENTER);
            }
        }

        if app.tab().loading {
            SetTextColor(hdc, t.text_dim);
            draw_text(hdc, "正在列举…", RECT { left: l.view.left + 12, top: list_top + 4, right: l.view.right, bottom: list_top + header_h }, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        } else if visible.is_empty() {
            SetTextColor(hdc, t.text_dim);
            draw_text(hdc, "此文件夹为空", RECT { left: l.view.left + 12, top: list_top + 4, right: l.view.right, bottom: list_top + header_h }, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        }

        // 滚动条：没有它，文件多时看不出还有多少内容可用
        let content_h = visible.len() as i32 * row_h;
        let viewport_h = l.view.bottom - list_top;
        if content_h > viewport_h && viewport_h > 0 {
            let sb_w = ((8.0 * app.metrics.scale).round() as i32).max(4);
            let track = RECT { left: l.view.right - sb_w, top: list_top, right: l.view.right, bottom: l.view.bottom };
            let tb = CreateSolidBrush(t.surface_alt);
            FillRect(hdc, &track, tb);
            let _ = DeleteObject(tb.into());
            let min_thumb = (24.0 * app.metrics.scale).round() as i32;
            let thumb_h = (((viewport_h as f32 / content_h as f32) * viewport_h as f32) as i32).max(min_thumb).min(viewport_h);
            let max_scroll = content_h - viewport_h;
            let frac = if max_scroll > 0 { (scroll as f32 / max_scroll as f32).clamp(0.0, 1.0) } else { 0.0 };
            let thumb_y = list_top + (frac * (viewport_h - thumb_h) as f32) as i32;
            let thumb = RECT { left: track.left, top: thumb_y, right: track.right, bottom: thumb_y + thumb_h };
            let tbf = CreateSolidBrush(t.text_dim);
            FillRect(hdc, &thumb, tbf);
            let _ = DeleteObject(tbf.into());
        }

        SelectObject(hdc, old_font);
    }
}

fn size_of(e: &fm_core::FileEntry) -> String {
    if e.is_dir() {
        String::new()
    } else {
        human(e.size)
    }
}

fn format_date(ft: i64) -> String {
    if ft <= 0 {
        return String::new();
    }
    // FILETIME(100ns since 1601) -> Unix seconds
    let secs = ft / 10_000_000 - 11_644_473_600;
    let days = secs / 86400;
    // 简化换算：避免引入 chrono 依赖，直接算年月日（1970-01-01 起）
    let days_i = days as i64;
    let (y, m, d) = civil_from_days(days_i);
    format!("{y:04}/{m:02}/{d:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as i64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn paint_status(app: &App, hdc: HDC, l: &Layout) {
    let t = &app.theme;
    unsafe {
        let brush = CreateSolidBrush(t.status_bg);
        FillRect(hdc, &l.status, brush);
        let _ = DeleteObject(brush.into());
        // 资源管理器状态栏顶部没有分隔线，靠底色区分（#202020 → #333333），照抄。
        let font = ui_font(app.metrics.scale);
        let old = SelectObject(hdc, font.into());
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, t.text_dim);
        let text = app.status_text();
        draw_text(hdc, &text, RECT { left: l.status.left + 10, top: l.status.top, right: l.status.right, bottom: l.status.bottom }, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        SelectObject(hdc, old);
    }
}

// ---------------------------------------------------------------- 子控件

const ID_ADDRESS: isize = 101;
const ID_SEARCH: isize = 102;
const ID_RENAME: isize = 103;

static mut ADDR_OLD: isize = 0;
static mut SEARCH_OLD: isize = 0;
static mut RENAME_OLD: isize = 0;

pub fn create_children(app: &mut App, instance: Option<HINSTANCE>) {
    let edit_class = HSTRING::from("EDIT");
    unsafe {
        let addr = CreateWindowExW(
            WINDOW_EX_STYLE(0), // 去掉 WS_EX_CLIENTEDGE：暗色主题下立体白边非常突兀
            windows::core::PCWSTR(edit_class.as_ptr()),
            windows::core::PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            0,
            0,
            10,
            10,
            Some(app.hwnd),
            Some(HMENU(ID_ADDRESS as *mut core::ffi::c_void)),
            instance,
            None,
        )
        .unwrap_or_default();
        app.hwnd_address = addr;
        ADDR_OLD = SetWindowLongPtrW(addr, GWLP_WNDPROC, (addr_proc as *const ()) as isize);
        set_edit_font(addr, app.metrics.scale);

        let search = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            windows::core::PCWSTR(edit_class.as_ptr()),
            windows::core::PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            0,
            0,
            10,
            10,
            Some(app.hwnd),
            Some(HMENU(ID_SEARCH as *mut core::ffi::c_void)),
            instance,
            None,
        )
        .unwrap_or_default();
        app.hwnd_search = search;
        SEARCH_OLD = SetWindowLongPtrW(search, GWLP_WNDPROC, (search_proc as *const ()) as isize);
        set_edit_font(search, app.metrics.scale);
    }
}

/// 给 EDIT 子控件套上 UI 字体（默认是系统 12px 位图字体，与自绘 UI 完全不搭）。
fn set_edit_font(hwnd: HWND, scale: f32) {
    unsafe {
        let f = ui_font(scale);
        let _ = SendMessageW(hwnd, WM_SETFONT, Some(WPARAM(f.0 as usize)), Some(LPARAM(1)));
    }
}

fn position_children(app: &App) {
    let l = compute_layout(
        app.last_size.0,
        app.last_size.1,
        &app.metrics,
        app.cfg.ui.sidebar_visible,
        app.cfg.ui.statusbar_visible,
    );
    unsafe {
        let _ = SetWindowPos(app.hwnd_address, None, l.address.left, l.address.top, l.address.right - l.address.left, l.address.bottom - l.address.top, SWP_NOZORDER | SWP_NOACTIVATE);
        let _ = SetWindowPos(app.hwnd_search, None, l.search.left, l.search.top, l.search.right - l.search.left, l.search.bottom - l.search.top, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

pub(crate) fn reposition(app: &App) {
    position_children(app);
}

/// 对外入口：布局参数变化（导航窗格/状态栏显隐）后重排子控件并请求重绘。
pub fn reposition_chrome(app: &App) {
    position_children(app);
}

/// 行内重命名的编辑框：覆盖在被重命名行之上。
pub fn create_rename_editor(app: &mut App, initial: &str) {
    if app.rename_hwnd.is_some() {
        return;
    }
    let edit_class = HSTRING::from("EDIT");
    let row_h = app.metrics.row_height_for(app.tab().view_mode);
    let l = compute_layout(
        app.last_size.0,
        app.last_size.1,
        &app.metrics,
        app.cfg.ui.sidebar_visible,
        app.cfg.ui.statusbar_visible,
    );
    let header_h = if app.tab().view_mode == fm_core::ViewMode::Details {
        (26.0 * app.metrics.scale) as i32
    } else {
        0
    };
    let y = l.view.top + header_h + app.tab().focus_row as i32 * row_h - app.tab().scroll;
    unsafe {
        let hwnd = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            windows::core::PCWSTR(edit_class.as_ptr()),
            windows::core::PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            l.view.left + (30.0 * app.metrics.scale) as i32,
            y,
            (300.0 * app.metrics.scale) as i32,
            row_h,
            Some(app.hwnd),
            Some(HMENU(ID_RENAME as *mut core::ffi::c_void)),
            None,
            None,
        )
        .unwrap_or_default();
        app.rename_hwnd = Some(hwnd);
        RENAME_OLD = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, (rename_proc as *const ()) as isize);
        let text = wide(initial);
        let _ = SetWindowTextW(hwnd, windows::core::PCWSTR(text.as_ptr()));
        let _ = windows::Win32::UI::WindowsAndMessaging::SendMessageW(
            hwnd,
            EM_SETSEL,
            Some(WPARAM(0)),
            Some(LPARAM(-1)),
        );
        let _ = SetFocus(Some(hwnd));
    }
}

unsafe fn call_old(old: isize, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let prev: WNDPROC = std::mem::transmute::<isize, WNDPROC>(old);
    CallWindowProcW(prev, hwnd, msg, wparam, lparam)
}

unsafe extern "system" fn addr_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_KEYDOWN && wparam.0 as i32 == 0x0D {
        crate::on_address_enter();
        return LRESULT(0);
    }
    if msg == WM_KEYDOWN && wparam.0 as i32 == 0x1B {
        crate::on_edit_escape();
        return LRESULT(0);
    }
    call_old(ADDR_OLD, hwnd, msg, wparam, lparam)
}

unsafe extern "system" fn search_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let r = call_old(SEARCH_OLD, hwnd, msg, wparam, lparam);
    if msg == WM_KEYDOWN && wparam.0 as i32 == 0x0D {
        crate::on_search_enter();
        return LRESULT(0);
    }
    if msg == WM_KEYDOWN && wparam.0 as i32 == 0x1B {
        crate::on_edit_escape();
        return LRESULT(0);
    }
    r
}

unsafe extern "system" fn rename_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_KEYDOWN && wparam.0 as i32 == 0x0D {
        crate::on_rename_commit();
        return LRESULT(0);
    }
    if msg == WM_KEYDOWN && wparam.0 as i32 == 0x1B {
        crate::on_rename_cancel();
        return LRESULT(0);
    }
    if msg == WM_KILLFOCUS {
        crate::on_rename_commit();
        return LRESULT(0);
    }
    call_old(RENAME_OLD, hwnd, msg, wparam, lparam)
}

pub fn read_window_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 2];
        GetWindowTextW(hwnd, &mut buf);
        crate::icons::from_wide(&buf)
    }
}

pub fn set_window_text_utf16(hwnd: HWND, text: &str) {
    let buf = wide(text);
    unsafe {
        let _ = SetWindowTextW(hwnd, windows::core::PCWSTR(buf.as_ptr()));
    }
}

pub fn sync_address(app: &App) {
    let text = if app.tab().home {
        "主页".to_string()
    } else {
        // 路径进过 canonicalize 之后会带上 `\\?\` 前缀（Win32 长路径约定）。
        // 它是给 API 看的，不是给人看的 —— 直接显示在地址栏会很怪。
        fm_core::paths::strip_verbatim(&app.tab().path)
            .to_string_lossy()
            .to_string()
    };
    set_window_text_utf16(app.hwnd_address, &text);
}

pub fn focus_edit(hwnd: HWND) {
    unsafe {
        let _ = SetFocus(Some(hwnd));
        let _ = SendMessageW(hwnd, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
    }
}

/// 显示隐藏文件标记（供配置联动使用）。
pub fn hidden_attr_flag() -> u32 {
    2 // FILE_ATTRIBUTE_HIDDEN
}
