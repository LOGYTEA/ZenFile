//! 自绘渲染与布局（技术方案 §4 自绘标题栏 / §5 虚拟化列表）。
//!
//! 全部内容绘制在一张内存位图上再一次性 BitBlt，避免大列表滚动时的撕裂。
//! 列表是虚拟化的：只为可见行请求数据与图标 index。

use windows::core::HSTRING;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CombineRgn, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen,
    CreateRectRgn, CreateRoundRectRgn, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, Ellipse,
    FillRect, FillRgn, GetDC,
    GetTextExtentPoint32W, IntersectClipRect, InvalidateRect, LineTo, MoveToEx, PS_SOLID, Rectangle,
    ReleaseDC, RestoreDC, RoundRect, SaveDC, SelectObject, SetBkMode, SetTextColor, DT_CENTER,
    DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, DRAW_TEXT_FORMAT,
    FONT_CHARSET, FONT_CLIP_PRECISION, FONT_OUTPUT_PRECISION, FONT_QUALITY, GetStockObject, HBITMAP,
    HDC, HFONT, NULL_BRUSH, NULL_PEN, Polygon, SRCCOPY, TRANSPARENT, RGN_AND,
};
use windows::Win32::Foundation::POINT;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Controls::EM_SETSEL;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::Foundation::HINSTANCE;
use windows::Win32::UI::WindowsAndMessaging::HICON;

use crate::app::{human, App};
use crate::icons::wide;
use crate::theme::Metrics;
use crate::theme::Theme;

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

/// 按钮角标用的小号字形（下拉三角）。和上面几个字体一样缓存：每帧
/// `CreateFont` + `DeleteObject` 会在高刷屏上白白多出几百次 GDI 调用。
static mut CARET_FONT: HFONT = HFONT(std::ptr::null_mut());
static mut CARET_FONT_SCALE: f32 = 0.0;

pub fn caret_font(scale: f32) -> HFONT {
    unsafe {
        if CARET_FONT.0.is_null() || (CARET_FONT_SCALE - scale).abs() > 0.01 {
            if !CARET_FONT.0.is_null() {
                let _ = DeleteObject(CARET_FONT.into());
            }
            CARET_FONT = make_font((-8.0 * scale).round() as i32, 400, "Segoe MDL2 Assets");
            CARET_FONT_SCALE = scale;
        }
        CARET_FONT
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
            // -13 与列表正文同号，但导航窗格看起来还是偏重（用户嫌大）。
            // 再降一档到 -12：比重列表轻一号，才是资源管理器导航窗格的分量。
            SIDEBAR_FONT = make_font((-12.0 * scale).round() as i32, 400, "Microsoft YaHei UI");
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

/// 导航窗格条目几何：返回 `(该行矩形, 展开箭头矩形, 图标矩形, 文字矩形)`。
///
/// 箭头槽位**每一行都留**（不管有没有子节点），这样不同层级的图标才能竖着对齐 ——
/// 否则展开过的行缩进、没展开的不缩进，整列图标会歪成锯齿。
pub fn sidebar_row_geoms(
    l: &Layout,
    index: usize,
    depth: usize,
    scale: f32,
    scroll: i32,
) -> (RECT, RECT, RECT, RECT) {
    let row = sidebar_row_h(scale);
    // `scroll` 是像素偏移：树比窗格长的时候靠它上下滚，否则展开几层就只能看见
    // 前十几行，下面的盘符/回收站永远滚不出来。
    let top = l.sidebar.top + row + index as i32 * row - scroll;
    let row_r = RECT { left: l.sidebar.left, top, right: l.sidebar.right, bottom: top + row };
    let indent = (16.0 * scale).round() as i32 * depth as i32;
    let chev_s = (16.0 * scale).round() as i32;
    let chev_x = l.sidebar.left + (4.0 * scale).round() as i32 + indent;
    let chev_y = top + (row - chev_s) / 2;
    let chev_r = RECT { left: chev_x, top: chev_y, right: chev_x + chev_s, bottom: chev_y + chev_s };
    let icon_s = (20.0 * scale).round() as i32;
    let icon_x = chev_x + chev_s + (2.0 * scale).round() as i32;
    let icon_y = top + (row - icon_s) / 2;
    let icon_r = RECT { left: icon_x, top: icon_y, right: icon_x + icon_s, bottom: icon_y + icon_s };
    let text_r = RECT {
        left: icon_x + icon_s + (12.0 * scale).round() as i32,
        top,
        right: l.sidebar.right - (6.0 * scale).round() as i32,
        bottom: top + row,
    };
    (row_r, chev_r, icon_r, text_r)
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
    /// 目录栏最右侧的「批量重命名 ▾」按钮。
    ///
    /// 画/命中共用这一份几何（同一条纪律）。点它弹出五种重命名模式的下拉，
    /// 右键菜单里不再放这一项 —— 右键菜单要留给"对选中项做什么"的高频动作。
    pub bulk_btn: RECT,
    pub tabs: RECT,
    /// 标签条右端那个「+」此刻是否显示（「界面」页可以把它收进「更多」）。
    pub new_tab_visible: bool,
}

pub const TAB_W: i32 = 180;

/// 把整份布局平移 (dx, dy)。
///
/// 设置页的「界面」预览要按真实窗口画一遍 —— 用 layout_sized 按预览的
/// 宽高算一份真布局，再整体搬到预览矩形里，然后调用**和真窗口同一批绘制
/// 函数**。于是预览和软件长得一模一样，不是因为画了两份像的。
pub fn translate_layout(l: &mut Layout, dx: i32, dy: i32) {
    for r in [
        &mut l.title, &mut l.toolbar, &mut l.sidebar, &mut l.view, &mut l.status,
        &mut l.back, &mut l.forward, &mut l.up, &mut l.address, &mut l.search,
        &mut l.min_btn, &mut l.max_btn, &mut l.close_btn, &mut l.menu_btn,
        &mut l.sidebar_toggle, &mut l.bulk_btn, &mut l.tabs,
    ] {
        r.left += dx;
        r.top += dy;
        r.right += dx;
        r.bottom += dy;
    }
}

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
    // 「批量重命名」图标按钮：目录栏最右侧、**常驻**（置顶显示）。
    //
    // 上一版是 132px 的"批量重命名 ▾"文字按钮，把地址栏挤掉了小一半 ——
    // 面包屑一长就只剩"此电脑 › …"，等于挡住了目录。改成 30px 的纯图标按钮
    // （字形 E8AC + 右下角一个小三角表示有下拉），地址栏拿回 100px。
    let bulk_w = (30.0 * m.scale).round() as i32;

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
            right: w - pad - search_w - bulk_w - 16,
            bottom: tb_top - pad,
        },
        // 搜索框回到**最右**（资源管理器就是最右），「批量重命名」图标按钮排在
        // 它左边一格 —— 用户要的是"搜索栏左边那个栏里"。
        search: RECT {
            left: w - pad - search_w,
            top: top + pad,
            right: w - pad,
            bottom: tb_top - pad,
        },
        bulk_btn: RECT {
            left: w - pad - search_w - 8 - bulk_w,
            top: top + pad,
            right: w - pad - search_w - 8,
            bottom: tb_top - pad,
        },
        min_btn: RECT { left: w - btn_w * 3, top: 0, right: w - btn_w * 2, bottom: title_h },
        max_btn: RECT { left: w - btn_w * 2, top: 0, right: w - btn_w, bottom: title_h },
        close_btn: RECT { left: w - btn_w, top: 0, right: w, bottom: title_h },
        new_tab_visible: true,
    }
}

/// 真窗口的布局入口：先按尺寸算一份，再套用「界面」页那三条带的排布。
///
/// 之前 `ui_settings.bars` 只有设置页自己在读 —— 用户在编排页里把组件拖来拖去、
/// 拖进拖出「更多」，真窗口一个像素都不动，整页等于空壳。现在**布局和命中都从
/// 这里走**，于是"挪位置 / 收起来 / 放回来"立刻在真窗口上生效。
pub fn layout_for(app: &App) -> Layout {
    layout_sized(app, app.last_size.0, app.last_size.1)
}

/// 同上，但尺寸由调用方给（ 拿到的是客户区尺寸，可能还没落到
///  上）。WM_PAINT 必须走这个 —— 之前它自己算了一份不带排布的
/// 布局，于是设置页改的排布对真窗口完全无效。
pub fn layout_sized(app: &App, width: i32, height: i32) -> Layout {
    let mut l = compute_layout(
        width,
        height,
        &app.metrics,
        app.cfg.ui.sidebar_visible,
        app.cfg.ui.statusbar_visible,
    );
    apply_chrome(&mut l, app);
    l
}

/// 组件此刻该不该出现：**在某条带里** 且 **没被收进「更多」**。
///
/// 画和命中都问这一个函数 —— 两边各判一次，收起来的按钮迟早会出现
/// "看得见点不到"或"点得到看不见"。
pub fn chrome_on(app: &App, id: u16) -> bool {
    !app.ui_settings.ui_hidden.contains(&(id as u8)) && app.ui_settings.bars.iter().any(|b| b.contains(&id))
}

/// 把「界面」页的排布套到布局上。
///
/// 组件被收起来就地赋一个零矩形：绘制时零矩形画不出东西，命中时零矩形点不中，
/// 于是"隐藏"不用在十几处绘制代码里各加一个 if。
fn apply_chrome(l: &mut Layout, app: &App) {
    use crate::settings::{
        W_ADDRESS, W_BACK, W_BULK, W_CLOSE, W_FORWARD, W_MAX, W_MIN, W_MORE, W_NEW_TAB,
        W_SEARCH, W_SIDEBAR_TOGGLE, W_TABS, W_UP,
    };
    let s = app.metrics.scale;
    let z = RECT::default();
    let pad = (6.0 * s).round() as i32;
    let nav = (30.0 * s).round() as i32;
    let btn_w = (44.0 * s).round() as i32;
    let toggle_w = (40.0 * s).round() as i32;
    let search_w = (170.0 * s).round() as i32;
    let bulk_w = (30.0 * s).round() as i32;
    let plus_w = (32.0 * s).round() as i32;
    let gap2 = (2.0 * s).round() as i32;
    let gap8 = (8.0 * s).round() as i32;
    let w = l.title.right.max(1);
    let hid = &app.ui_settings.ui_hidden;

    let band = |i: usize| -> Vec<u16> { app.ui_settings.bars.get(i).cloned().unwrap_or_default() };
    let take = |v: &[u16], ids: &[u16]| -> Vec<u16> {
        v.iter()
            .copied()
            .filter(|i| ids.contains(i) && !hid.contains(&(*i as u8)))
            .collect()
    };

    // ————————————————————————————— 标题栏
    let t0 = band(0);
    // 右端一排固定宽的按钮，按带里的顺序从左往右摆
    let right = take(&t0, &[W_MORE, W_MIN, W_MAX, W_CLOSE]);
    let right_start = w - btn_w * right.len() as i32;
    l.menu_btn = z;
    l.min_btn = z;
    l.max_btn = z;
    l.close_btn = z;
    let mut x = right_start;
    for id in &right {
        let r = RECT { left: x, top: 0, right: x + btn_w, bottom: l.title.bottom };
        match *id {
            W_MORE => l.menu_btn = r,
            W_MIN => l.min_btn = r,
            W_MAX => l.max_btn = r,
            _ => l.close_btn = r,
        }
        x += btn_w;
    }
    // 左端：侧栏开关（固定宽）→ 标签条（吃掉剩下全部）→ 「+」
    if t0.contains(&W_SIDEBAR_TOGGLE) && !hid.contains(&(W_SIDEBAR_TOGGLE as u8)) {
        l.sidebar_toggle = RECT { left: 0, top: 0, right: toggle_w, bottom: l.title.bottom };
        l.tabs.left = toggle_w;
    } else {
        l.sidebar_toggle = z;
        l.tabs.left = 0;
    }
    let plus_reserve = if t0.contains(&W_NEW_TAB) && !hid.contains(&(W_NEW_TAB as u8)) {
        plus_w
    } else {
        0
    };
    l.new_tab_visible = plus_reserve > 0;
    l.tabs.right = (right_start - plus_reserve).max(l.tabs.left + 1);
    if !t0.contains(&W_TABS) || hid.contains(&(W_TABS as u8)) {
        l.tabs = z;
        l.new_tab_visible = false;
    }

    // ————————————————————————————— 工具栏
    let t1 = band(1);
    let left = take(&t1, &[W_BACK, W_FORWARD, W_UP]);
    l.back = z;
    l.forward = z;
    l.up = z;
    let mut x = pad;
    for id in &left {
        let r = RECT {
            left: x,
            top: l.toolbar.top + pad,
            right: x + nav,
            bottom: l.toolbar.bottom - pad,
        };
        match *id {
            W_BACK => l.back = r,
            W_FORWARD => l.forward = r,
            _ => l.up = r,
        }
        x += nav + gap2;
    }
    let left_end = if left.is_empty() { pad } else { x };

    // 右端一组：按带里的顺序从左往右占，整体右对齐
    let r1 = take(&t1, &[W_BULK, W_SEARCH]);
    let mut total = 0;
    for (k, id) in r1.iter().enumerate() {
        total += if *id == W_SEARCH { search_w } else { bulk_w };
        if k + 1 < r1.len() {
            total += gap8;
        }
    }
    l.bulk_btn = z;
    l.search = z;
    let right_left = w - pad - total;
    let mut x = right_left;
    for id in &r1 {
        let wd = if *id == W_SEARCH { search_w } else { bulk_w };
        let r = RECT {
            left: x,
            top: l.toolbar.top + pad,
            right: x + wd,
            bottom: l.toolbar.bottom - pad,
        };
        if *id == W_SEARCH {
            l.search = r;
        } else {
            l.bulk_btn = r;
        }
        x += wd + gap8;
    }
    if t1.contains(&W_ADDRESS) && !hid.contains(&(W_ADDRESS as u8)) {
        l.address = RECT {
            left: left_end + (10.0 * s).round() as i32,
            top: l.toolbar.top + pad,
            right: (right_left - gap8).max(left_end + 1),
            bottom: l.toolbar.bottom - pad,
        };
    } else {
        l.address = z;
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

    /// 标签页右上角的关闭按钮。
    ///
    /// 绘制和命中必须共用这一个函数 —— 两边各写一份常量，改一次就错位（踩过）。
    /// 资源管理器只在"当前标签"和"鼠标悬停的标签"上画它，所以调用方还要看 hover 态。
    pub fn tab_close_rect(&self, index: usize, count: usize, m: &Metrics) -> RECT {
        let r = self.tab_rect(index, count, m);
        let d = (18.0 * m.scale).round() as i32;
        let gap = (2.0 * m.scale).round() as i32;
        let cy = (r.top + r.bottom) / 2;
        RECT {
            left: (r.right - gap - d).max(r.left),
            top: cy - d / 2,
            right: (r.right - gap).max(r.left + 2),
            bottom: cy - d / 2 + d,
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

// ------------------------------------------------- 大图标/中图标的网格几何

/// 大图标 / 中图标视图的网格几何。
///
/// 绘制、命中测试、滚动高度**必须共用这一个结构**：早期版本三处各算一份，
/// 结果条目竖着排成一列，还一路画到标题栏上。
pub struct IconGrid {
    /// 一行能放几个格子
    pub cols: i32,
    pub tile_w: i32,
    pub tile_h: i32,
    /// 内容区左右留白
    pub pad: i32,
}

pub fn icon_grid(l: &Layout, m: &Metrics, mode: fm_core::ViewMode) -> IconGrid {
    let icon = if mode == fm_core::ViewMode::LargeIcons { 96.0 } else { 48.0 };
    let tile_w = ((icon + 48.0) * m.scale).round() as i32;
    let tile_h = m.row_height_for(mode);
    let pad = (12.0 * m.scale).round() as i32;
    let avail = (l.view.right - l.view.left - pad * 2).max(tile_w);
    let cols = (avail / tile_w).max(1);
    IconGrid { cols, tile_w, tile_h, pad }
}

/// 网格视图里这一屏要画哪些下标（线性视图退化成连续的一段）。
fn index_range(
    grid: &Option<IconGrid>,
    total: usize,
    view_h: i32,
    tile_h: i32,
    row_h: i32,
    scroll: i32,
) -> (usize, usize) {
    match grid {
        Some(g) => {
            let line0 = (scroll / g.tile_h.max(1)) as usize;
            let lines = (view_h / tile_h.max(1) + 2) as usize;
            let from = line0 * g.cols as usize;
            let to = ((line0 + lines) * g.cols as usize).min(total);
            (from.min(total), to)
        }
        None => {
            let i0 = (scroll / row_h.max(1)) as usize;
            let to = (i0 + (view_h / row_h.max(1) + 2) as usize).min(total);
            (i0.min(total), to)
        }
    }
}

/// 网格视图的命中测试：(x, y) 落在第几个格子上。
pub fn grid_hit(
    g: &IconGrid,
    l: &Layout,
    x: i32,
    y: i32,
    list_top: i32,
    scroll: i32,
    total: usize,
) -> Option<usize> {
    let local_x = x - (l.view.left + g.pad);
    let local_y = y - list_top + scroll;
    if local_x < 0 || local_y < 0 {
        return None;
    }
    let col = local_x / g.tile_w.max(1);
    let line = local_y / g.tile_h.max(1);
    if col >= g.cols {
        return None;
    }
    let idx = (line * g.cols + col) as usize;
    if idx < total {
        Some(idx)
    } else {
        None
    }
}

/// 第 `index` 个条目在内容区里的矩形（**未裁剪**到可视范围）。
///
/// 网格视图和线性视图的几何差别很大，框选命中与绘制必须共用这一个函数 ——
/// 两边各写一份常量，改一次就错位（这条踩过）。
pub fn item_rect(app: &App, l: &Layout, index: usize, view_top: i32) -> RECT {
    let tab = app.tab();
    let mode = tab.view_mode;
    if matches!(mode, fm_core::ViewMode::LargeIcons | fm_core::ViewMode::MediumIcons) {
        let g = icon_grid(l, &app.metrics, mode);
        let cols = g.cols.max(1);
        let col = (index as i32) % cols;
        let line = (index as i32) / cols;
        let x = l.view.left + g.pad + col * g.tile_w;
        let y = view_top + line * g.tile_h - tab.scroll;
        RECT { left: x, top: y, right: x + g.tile_w, bottom: y + g.tile_h }
    } else {
        let row_h = app.metrics.row_height_for(mode);
        let y = view_top + index as i32 * row_h - tab.scroll;
        RECT { left: l.view.left, top: y, right: l.view.right, bottom: y + row_h }
    }
}

/// 网格视图的内容总高度（用于滚动条与 scroll 钳制）。
pub fn grid_content_h(g: &IconGrid, count: usize) -> i32 {
    let lines = (count as i32 + g.cols - 1) / g.cols.max(1);
    lines * g.tile_h
}

// ---------------------------------------------------------------- 绘制

pub fn paint(app: &mut App, hdc: HDC, width: i32, height: i32) {
    unsafe {
        let mem = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, width, height);
        let old_bmp = SelectObject(mem, bmp.into());
        let l = layout_sized(app, width, height);
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
        paint_panes(app, mem, &l);
        if app.cfg.ui.statusbar_visible {
            paint_status(app, mem, &l);
        }

        paint_drag_overlay(app, mem, &l);
        // 拖标签时的"8 向贴"预览：压在最上面
        paint_dock_overlay(app, mem, &l);

        let _ = BitBlt(hdc, 0, 0, width, height, Some(mem), 0, 0, SRCCOPY);
        SelectObject(mem, old_bmp);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        // 地址框 / 搜索框的空态提示（放大镜 + "搜索 X"）画在**子 EDIT 自己身上**，
        // 而主窗口带 WS_CLIPCHILDREN，父窗口重绘永远不碰它们那两块。提示词跟着
        // 当前标签变（"搜索设置" / "搜索 Windows"），标签切换却只重绘父窗口 ——
        // 不在这里显式让子 EDIT 跟着重画，提示词就停在旧标签的文字上（实测：
        // 把搜索框从排布里摘掉再装回，提示一直是"搜索设置"）。
        // 子 EDIT 重画不会再失效父窗口，这里不会成环。
        let _ = InvalidateRect(Some(app.hwnd_search), None, true);
        let _ = InvalidateRect(Some(app.hwnd_address), None, true);
    }
}

/// 拖拽覆盖层：框选矩形 + 投放目标高亮 + 跟着光标的"移动到…"提示。
///
/// 必须画在**所有内容之上**（所以放在 paint() 的最后）：框选是从空白处起手的，
/// 矩形会盖住条目；拖到面包屑上时高亮也要压在地址栏上。
/// 分栏时每个窗格顶部那条标签条的高度（DIP）。
pub const PANE_STRIP_H: f32 = 26.0;

pub fn pane_strip_px(scale: f32) -> i32 {
    (PANE_STRIP_H * scale).round() as i32
}

/// 点 (x, y) 落在第几个窗格里。不分栏时就是焦点窗格。
pub fn pane_index_at(app: &App, l: &Layout, x: i32, y: i32) -> Option<usize> {
    if app.panes.len() <= 1 {
        return Some(app.focus_pane);
    }
    for (i, p) in app.panes.iter().enumerate() {
        let pr = p.rect.to_px(&l.view);
        if x >= pr.left && x < pr.right && y >= pr.top && y < pr.bottom {
            return Some(i);
        }
    }
    None
}

/// 把 `Layout` 里"内容区 / 标签条"那几个矩形覆写成第 `i` 个窗格的。
///
/// 这是分栏能成立的关键：绘制和命中测试都先调它，于是下面那一大片只认
/// `app.tab()` / `l.view` 的老代码（近两百处）完全不用知道窗格的存在。
pub fn pane_layout(app: &App, l: &Layout, i: usize) -> Layout {
    if app.panes.len() <= 1 {
        return *l;
    }
    let pr = app.panes[i.min(app.panes.len() - 1)].rect.to_px(&l.view);
    let strip = pane_strip_px(app.metrics.scale);
    let mut o = *l;
    o.tabs = RECT { left: pr.left, top: pr.top, right: pr.right, bottom: pr.top + strip };
    o.title = o.tabs;
    o.view = RECT { left: pr.left, top: pr.top + strip, right: pr.right, bottom: pr.bottom };
    o
}

/// 内容区里的"8 向贴"判定：四边各占 30% 的带，两带相交处是四角，
/// 正中间那块是 Center（合并进目标窗格，不分栏）。
pub fn dock_zone_at(l: &Layout, x: i32, y: i32) -> crate::app::DockZone {
    use crate::app::DockZone::*;
    let r = l.view;
    let w = r.right - r.left;
    let h = r.bottom - r.top;
    if w <= 0 || h <= 0 {
        return Center;
    }
    let mx = ((w as f32) * 0.30) as i32;
    let my = ((h as f32) * 0.30) as i32;
    let left = x < r.left + mx;
    let right = x >= r.right - mx;
    let top = y < r.top + my;
    let bottom = y >= r.bottom - my;
    // **四向，不做 1/4 四角**（用户明确要求：四格布局先砍掉，只留左右/上下
    // 二分）。顺序上左右先于上下：不然左上角会先命中 `top` 落成"上半"，
    // 贴左边时角上那段手感很怪。四角区域自然归入左右边。
    match (top, bottom, left, right) {
        (_, _, true, _) => Left,
        (_, _, _, true) => Right,
        (true, _, _, _) => Top,
        (_, true, _, _) => Bottom,
        _ => Center,
    }
}

/// 某个贴靠区对应的矩形。四角是 1/4 大小的规则矩形 —— 口径必须和
/// `app::split_rect` 一致，两边各写一份就会"预览在这、落在那"。
pub fn dock_zone_rect(l: &Layout, z: crate::app::DockZone) -> RECT {
    use crate::app::DockZone::*;
    let r = l.view;
    let w = r.right - r.left;
    let h = r.bottom - r.top;
    let hw = w / 2;
    let hh = h / 2;
    match z {
        Left => RECT { left: r.left, top: r.top, right: r.left + hw, bottom: r.bottom },
        Right => RECT { left: r.left + hw, top: r.top, right: r.right, bottom: r.bottom },
        Top => RECT { left: r.left, top: r.top, right: r.right, bottom: r.top + hh },
        Bottom => RECT { left: r.left, top: r.top + hh, right: r.right, bottom: r.bottom },
        TopLeft => RECT { left: r.left, top: r.top, right: r.left + hw, bottom: r.top + hh },
        TopRight => RECT { left: r.left + hw, top: r.top, right: r.right, bottom: r.top + hh },
        BottomLeft => RECT { left: r.left, top: r.top + hh, right: r.left + hw, bottom: r.bottom },
        BottomRight => RECT { left: r.left + hw, top: r.top + hh, right: r.right, bottom: r.bottom },
        Center => r,
    }
}

/// 贴靠预览：把"松手之后新标签会落到哪一块"真的画出来。
///
/// 只高亮一块还不够 —— 用户看不见另外 7 个方向在哪，所以同时把十字分隔线
/// 淡淡勾出来，鼠标一带就能看出分界在哪。
pub fn paint_dock_overlay(app: &App, hdc: HDC, l: &Layout) {
    // 两种来源：拖标签（`tab_drag`）和拖文件夹（`drag_dock`）。
    // 画法完全一样，合成一处就不必担心两边长歪。
    let zone = match app.tab_drag.as_ref() {
        Some(d) if d.active => d.dock,
        Some(_) => None,
        None => app.drag_dock,
    };
    let Some(zone) = zone else { return };
    let t = app.theme;
    let pl = pane_layout(app, l, app.focus_pane);
    let r = dock_zone_rect(&pl, zone);
    unsafe {
        let mw = ((2.0 * app.metrics.scale).round() as i32).max(1);
        // 十字骨架
        let guide = CreateSolidBrush(t.surface_alt);
        let g = pl.view;
        let hw = (g.right - g.left) / 2;
        let hh = (g.bottom - g.top) / 2;
        for gr in [
            RECT { left: g.left + hw, top: g.top, right: g.left + hw + mw, bottom: g.bottom },
            RECT { left: g.left, top: g.top + hh, right: g.right, bottom: g.top + hh + mw },
        ] {
            FillRect(hdc, &gr, guide);
        }
        let _ = DeleteObject(guide.into());
        // 命中的那块：半透明强调色 + 一圈实边
        alpha_fill_rect(hdc, &r, t.accent, 70);
        let edge = CreateSolidBrush(t.accent);
        for e in [
            RECT { left: r.left, top: r.top, right: r.right, bottom: r.top + mw },
            RECT { left: r.left, top: r.bottom - mw, right: r.right, bottom: r.bottom },
            RECT { left: r.left, top: r.top, right: r.left + mw, bottom: r.bottom },
            RECT { left: r.right - mw, top: r.top, right: r.right, bottom: r.bottom },
        ] {
            FillRect(hdc, &e, edge);
        }
        let _ = DeleteObject(edge.into());
        let label = match zone {
            crate::app::DockZone::Center => "合并到这个窗格",
            crate::app::DockZone::Left => "贴到左侧",
            crate::app::DockZone::Right => "贴到右侧",
            crate::app::DockZone::Top => "贴到上方",
            crate::app::DockZone::Bottom => "贴到下方",
            crate::app::DockZone::TopLeft => "贴到左上",
            crate::app::DockZone::TopRight => "贴到右上",
            crate::app::DockZone::BottomLeft => "贴到左下",
            crate::app::DockZone::BottomRight => "贴到右下",
        };
        let f = ui_font(app.metrics.scale);
        let old = SelectObject(hdc, f.into());
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, t.text);
        draw_text(hdc, label, r, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        SelectObject(hdc, old);
    }
}

fn paint_drag_overlay(app: &mut App, hdc: HDC, l: &Layout) {
    // 先把要用的标量抄出来：下面取面包屑矩形要 &mut app，不能让 &app.drag
    // 的借用一直挂着。
    let (kind, cur, items_n, marquee) = match app.drag.as_ref() {
        Some(d) if d.active => (d.kind, d.cur, d.items.len(), marquee_rect(d)),
        _ => return,
    };
    let t = app.theme;
    let s = app.metrics.scale;
    let accent = t.accent;
    let copy = app.drag_copy();
    let target_kind = app.drop_target.as_ref().map(|d| d.kind);
    let target_name = app.drop_target.as_ref().map(|d| {
        d.path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| d.path.to_string_lossy().trim_end_matches('\\').to_string())
    });
    let ax = (accent.0 & 0xFF) as i32;
    let ay = ((accent.0 >> 8) & 0xFF) as i32;
    let az = ((accent.0 >> 16) & 0xFF) as i32;
    let bx = (t.window_bg.0 & 0xFF) as i32;
    let by = ((t.window_bg.0 >> 8) & 0xFF) as i32;
    let bz = ((t.window_bg.0 >> 16) & 0xFF) as i32;
    // 半透明填充用手动混色省掉一次 AlphaBlend（覆盖层是纯装饰，够用）
    let blend = COLORREF(((ax + bx) / 2 | (((ay + by) / 2) << 8) | (((az + bz) / 2) << 16)) as u32);
    let items: Vec<std::path::PathBuf> = match app.drag.as_ref() {
        Some(d) if d.active => d.items.clone(),
        _ => Vec::new(),
    };
    unsafe {
        let pen = CreatePen(PS_SOLID, 1, accent);
        let old_pen = SelectObject(hdc, pen.into());
        let brush = CreateSolidBrush(blend);
        let old_brush = SelectObject(hdc, brush.into());

        // 1) 投放目标：描一圈 accent 边（只描边不填充，里面的内容还得看得见）
        let target_rect = match target_kind {
            Some(crate::app::DropKind::Row(i)) => {
                Some(item_rect(app, l, i, l.view.top + header_height_of(app)))
            }
            Some(crate::app::DropKind::Crumb(i)) => address_bar(app, l).rects.get(i).copied(),
            Some(crate::app::DropKind::Sidebar(i)) => app
                .sidebar_rows()
                .get(i)
                .map(|row| sidebar_row_geoms(l, i, row.depth, s, app.sidebar_scroll).0),
            // 拖到标签上：把那个标签整块描出来（"文件会落进这个标签的目录"）
            Some(crate::app::DropKind::Tab(i)) => Some(l.tab_rect(i, app.tab_count(), &app.metrics)),
            None => None,
        };
        if let Some(r) = target_rect {
            let old_b = SelectObject(hdc, brush.into());
            MoveToEx(hdc, r.left, r.top, None);
            LineTo(hdc, r.right - 1, r.top);
            LineTo(hdc, r.right - 1, r.bottom - 1);
            LineTo(hdc, r.left, r.bottom - 1);
            LineTo(hdc, r.left, r.top);
            SelectObject(hdc, old_b);
        }

        // 2) 框选矩形：Win11 那种"淡蓝半透明遮罩 + accent 边"。
        //
        // 之前用 `Rectangle()` 直接填实色构图填搞定：实色把底下的文件条目整块盖住，
        // 拖的时候只能看见一块板，看不见自己框进去的是什么。这里改成真·半透明：
        // 先把 accent 铺进一张离屏图，再用 AlphaBlend 以 60/255 压上去 —— 底下
        // 的文字还能透出来（混的是真实内容，不是混背景色）。
        if kind == crate::app::DragKind::Marquee {
            let r = marquee;
            alpha_fill_rect(hdc, &r, accent, 60);
            let ob = SelectObject(hdc, GetStockObject(NULL_BRUSH).into());
            MoveToEx(hdc, r.left, r.top, None);
            LineTo(hdc, r.right - 1, r.top);
            LineTo(hdc, r.right - 1, r.bottom - 1);
            LineTo(hdc, r.left, r.bottom - 1);
            LineTo(hdc, r.left, r.top);
            SelectObject(hdc, ob);
        }

        // 3) 拖拽影像：**一张**卡片，把"拎着什么"和"要放到哪"都写在同一张上。
        //
        // 之前卡片和"移动 3 项到 下载"的提示牌各画各的、都挂在光标右下 12~14px，
        // 于是两块矩形叠在一起（"两个叠一起了"）。合并成一张后，卡片上半是图标
        // + 名字，下半只有真的悬到投放目标上时才多出一行"移动到 X"。
        if kind == crate::app::DragKind::Items && !items.is_empty() {
            let hint = match &target_name {
                Some(nm) => Some(format!("{}到 {}", if copy { "复制" } else { "移动" }, nm)),
                None => None,
            };
            draw_drag_image(app, hdc, cur, &items, copy, hint.as_deref(), items_n);
        }

        SelectObject(hdc, old_brush);
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(brush.into());
        let _ = DeleteObject(pen.into());
    }
}

/// 拖拽影像里的图标：目录走 dir_icon（认 desktop.ini 的自定义图标），文件按扩展名。
fn drag_icon(app: &mut App, path: &std::path::Path) -> HICON {
    if path.is_dir() {
        app.icons.dir_icon(path, true)
    } else {
        let ext = path
            .extension()
            .map(|x| format!(".{}", x.to_string_lossy()))
            .unwrap_or_default();
        app.icons.file_icon(path, &ext, true)
    }
}

/// 给一个矩形铺上半透明色（真·AlphaBlend）。
///
/// 不能图省事把颜色和目标底色手动混一混再实心填充：那样混出来的是**不透明**
/// 的一块板，底下的内容全被盖住。这里把色块画进离屏图，再以 alpha 压上去，
/// 底下是什么就还能看见什么（这是 AlphaBlend 和普通 FillRect 唯一的本质区别）。
fn alpha_fill_rect(hdc: HDC, r: &RECT, color: COLORREF, alpha: u8) {
    let w = (r.right - r.left).max(1);
    let h = (r.bottom - r.top).max(1);
    unsafe {
        let mem = CreateCompatibleDC(Some(hdc));
        if mem.is_invalid() {
            return;
        }
        let bmp = CreateCompatibleBitmap(hdc, w, h);
        let old = SelectObject(mem, bmp.into());
        let br = CreateSolidBrush(color);
        FillRect(mem, &RECT { left: 0, top: 0, right: w, bottom: h }, br);
        let _ = DeleteObject(br.into());
        let mut bf: windows::Win32::Graphics::Gdi::BLENDFUNCTION = std::mem::zeroed();
        bf.BlendOp = windows::Win32::Graphics::Gdi::AC_SRC_OVER as u8;
        bf.BlendFlags = 0;
        bf.SourceConstantAlpha = alpha;
        bf.AlphaFormat = 0;
        let _ = windows::Win32::Graphics::Gdi::AlphaBlend(
            hdc, r.left, r.top, w, h, mem, 0, 0, w, h, bf,
        );
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
    }
}

/// 跟手的拖拽影像：一张半透明小卡片，图标 + 名字（多项时堆叠两枚图标）。
///
/// 位置在光标右下 12px 处（资源管理器同款），整体按 80% 不透明度压上去 ——
/// 拖拽影像必须能透出底下内容，否则松手前看不见自己待会儿要落在哪。
fn draw_drag_image(
    app: &mut App,
    hdc: HDC,
    cur: (i32, i32),
    items: &[std::path::PathBuf],
    copy: bool,
    hint: Option<&str>,
    items_n: usize,
) {
    let s = app.metrics.scale;
    let icon_s = (32.0 * s).round() as i32;
    let pad = (8.0 * s).round() as i32;
    let gap = (6.0 * s).round() as i32;
    let first = &items[0];
    let name = if items.len() == 1 {
        crate::app::display_title(first)
    } else {
        format!("{} 项", items.len())
    };
    let font = ui_font(s);
    let sub_font = ui_font(s);
    // 第二行（"移动到 X"）只有真的悬到投放目标上时才出现，卡片因此会长一截
    let line_h = (16.0 * s).round() as i32;
    let hint_h = if hint.is_some() { line_h } else { 0 };
    let tw = unsafe {
        let old = SelectObject(hdc, font.into());
        let w = text_width(hdc, font, &name);
        SelectObject(hdc, old);
        w
    };
    let hint_w = match hint {
        Some(t) => unsafe {
            let old = SelectObject(hdc, sub_font.into());
            let w = text_width(hdc, sub_font, t);
            SelectObject(hdc, old);
            w
        },
        None => 0,
    };
    // 多项时把第二枚图标错开一点，做出"一叠"的感觉
    let stack = if items.len() > 1 { (6.0 * s).round() as i32 } else { 0 };
    let w = (pad * 2 + icon_s + stack + gap + tw).max(pad * 2 + hint_w) + (4.0 * s) as i32;
    let h = pad * 2 + icon_s.max((16.0 * s).round() as i32) + hint_h;
    let x = cur.0 + (12.0 * s).round() as i32;
    let y = cur.1 + (12.0 * s).round() as i32;
    // 卡片先画进离屏图（底色/accent 边/图标/文字），整张再半透明压上去
    unsafe {
        let mem = CreateCompatibleDC(Some(hdc));
        if mem.is_invalid() {
            return;
        }
        let bmp = CreateCompatibleBitmap(hdc, w, h);
        let old = SelectObject(mem, bmp.into());
        let bg = CreateSolidBrush(app.theme.surface);
        FillRect(mem, &RECT { left: 0, top: 0, right: w, bottom: h }, bg);
        let _ = DeleteObject(bg.into());
        let pen = CreatePen(
            windows::Win32::Graphics::Gdi::PS_SOLID,
            (1.0 * s).round().max(1.0) as i32,
            app.theme.accent,
        );
        let op = SelectObject(mem, pen.into());
        let ob = SelectObject(mem, GetStockObject(NULL_BRUSH).into());
        MoveToEx(mem, 0, 0, None);
        let _ = LineTo(mem, w - 1, 0);
        let _ = LineTo(mem, w - 1, h - 1);
        let _ = LineTo(mem, 0, h - 1);
        let _ = LineTo(mem, 0, 0);
        SelectObject(mem, ob);
        SelectObject(mem, op);
        let _ = DeleteObject(pen.into());

        let icons: Vec<HICON> = items
            .iter()
            .take(2)
            .map(|p| drag_icon(app, p))
            .collect();
        if items.len() > 1 {
            app.icons.draw(mem, icons[1], pad + stack, pad + stack / 2, icon_s);
            app.icons.draw(mem, icons[0], pad, pad, icon_s);
        } else {
            app.icons.draw(mem, icons[0], pad, pad, icon_s);
        }
        let of = SelectObject(mem, font.into());
        SetBkMode(mem, TRANSPARENT);
        SetTextColor(mem, app.theme.text);
        // 上半行：图标那一段的高度里放名字（和图标竖着居中）
        let top_h = h - hint_h;
        let mut n: Vec<u16> = name.encode_utf16().collect();
        DrawTextW(
            mem,
            &mut n,
            &mut RECT { left: pad + icon_s + stack + gap, top: 0, right: w - pad, bottom: top_h },
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        // 下半行：真悬到投放目标上才出现的"移动到 X"
        if let Some(t) = hint {
            SetTextColor(mem, app.theme.text_dim);
            let label = if items_n > 1 {
                format!("{t} · {items_n} 项")
            } else {
                t.to_string()
            };
            let mut nn: Vec<u16> = label.encode_utf16().collect();
            DrawTextW(
                mem,
                &mut nn,
                &mut RECT { left: pad, top: top_h, right: w - pad, bottom: h },
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
        }
        SelectObject(mem, of);
        // 复制时左上角挂一个小 "+"：这是资源管理器区分移动/复制的唯一视觉信号
        if copy {
            let d = (14.0 * s).round() as i32;
            let br = CreateSolidBrush(app.theme.accent);
            FillRect(mem, &RECT { left: 0, top: 0, right: d, bottom: d }, br);
            let _ = DeleteObject(br.into());
            let gf = glyph_font(s);
            let og = SelectObject(mem, gf.into());
            SetTextColor(mem, app.theme.text);
            let mut plus: Vec<u16> = "\u{E710}".encode_utf16().collect();
            DrawTextW(
                mem,
                &mut plus,
                &mut RECT { left: 0, top: 0, right: d, bottom: d },
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(mem, og);
        }

        let mut bf: windows::Win32::Graphics::Gdi::BLENDFUNCTION = std::mem::zeroed();
        bf.BlendOp = windows::Win32::Graphics::Gdi::AC_SRC_OVER as u8;
        bf.BlendFlags = 0;
        bf.SourceConstantAlpha = 205;
        bf.AlphaFormat = 0;
        let _ = windows::Win32::Graphics::Gdi::AlphaBlend(
            hdc, x, y, w, h, mem, 0, 0, w, h, bf,
        );
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
    }
}

/// 框选矩形（把起点/当前点规整成左上-右下）。
pub fn marquee_rect(drag: &crate::app::Drag) -> RECT {
    let (x0, y0) = drag.origin;
    let (x1, y1) = drag.cur;
    RECT {
        left: x0.min(x1),
        top: y0.min(y1),
        right: x0.max(x1),
        bottom: y0.max(y1),
    }
}

/// 同上，但只吃两个 i32 坐标；让框选逻辑在持有 `&app::Drag` 借用的同时
/// 也能调（比如 `WM_MOUSEMOVE` 边拖边算选中时）。
pub fn marquee_rect_from(origin: (i32, i32), cur: (i32, i32)) -> RECT {
    let (x0, y0) = origin;
    let (x1, y1) = cur;
    RECT {
        left: x0.min(x1),
        top: y0.min(y1),
        right: x0.max(x1),
        bottom: y0.max(y1),
    }
}

fn header_height_of(app: &App) -> i32 {
    if app.tab().view_mode == fm_core::ViewMode::Details {
        (26.0 * app.metrics.scale) as i32
    } else {
        0
    }
}

pub fn paint_title_bar(app: &mut App, hdc: HDC, l: &Layout) {
    let t = app.theme;
    unsafe {
        let brush = CreateSolidBrush(t.title_bg);
        FillRect(hdc, &l.title, brush);
        let _ = DeleteObject(brush.into());
        let font = ui_font(app.metrics.scale);
        let old = SelectObject(hdc, font.into());
        SetBkMode(hdc, TRANSPARENT);

        // 标签页。单窗格画焦点窗格的整条；**分栏时顶部也要有这条多标签**
        // （用户明确要求：最顶上的软件标题栏还是有多标签，不是变成一行
        // "目录A 丨 目录B" 文字）。窗格自己那条 26px 的 strip 作为局部
        // 指示保留在各窗格顶上 —— 一个是总览、一个是局部，不算重复。
        paint_tab_strip(app, hdc, l, app.focus_pane);
        // （原"- ZenFile"字样已按用户要求删掉：标题栏右侧那块地盘留给标签）
        // 侧栏折叠开关（最左）与更多菜单（最小化左侧）
        draw_chrome_button(
            app,
            hdc,
            &l.sidebar_toggle,
            ChromeGlyph::PaneToggle,
            app.hover_chrome & crate::app::HOVER_SIDEBAR_TOGGLE != 0,
        );
        draw_chrome_button(
            app,
            hdc,
            &l.menu_btn,
            ChromeGlyph::MoreDots,
            app.hover_chrome & crate::app::HOVER_MENU_BTN != 0,
        );
        // 标题按钮（自绘）。命中与动作由 WM_NC* 消息自己接管 —— 客户区已扩展到
        // 整窗，系统那套非客户区默认处理在这里靠不住（三个键点不动）。
        // 悬停/按下都要有反馈：没有反馈的按钮在用户眼里就是"坏的"。
        draw_caption_button(app, hdc, &l.min_btn, 1);
        draw_caption_button(app, hdc, &l.max_btn, 2);
        draw_caption_button(app, hdc, &l.close_btn, 3);
        SelectObject(hdc, old);
    }
}

/// 画一排标签条。窗口顶部那条（`paint_title_bar`）和分栏后每个窗格自己那条
/// 都走这里 —— 两边各写一份必然会长歪。
///
/// `pi` 是窗格下标；`l.tabs` 决定这一条画在哪（调用方把它覆写成窗格的顶边）。
fn paint_tab_strip(app: &mut App, hdc: HDC, l: &Layout, pi: usize) {
    let t = app.theme;
    let n = app.panes[pi].tabs.len();
    if n == 0 {
        return;
    }
    unsafe {
        let font = ui_font(app.metrics.scale);
        let old = SelectObject(hdc, font.into());
        SetBkMode(hdc, TRANSPARENT);
        let mut tab_icons: Vec<Option<HICON>> = vec![None; n];
        let mut tab_glyphs: Vec<Option<&'static str>> = vec![None; n];
        // 先把要的东西拷出来：`dir_icon` 要 &mut app.icons，
        // 拿着 `app.panes[pi].tabs` 的不可变借用不放会跟它打架。
        let infos: Vec<(bool, bool, bool, std::path::PathBuf)> = app
            .pane()
            .tabs
            .iter()
            .map(|t| (t.home, t.recycle, t.settings, t.path.clone()))
            .collect();
        for (i, (home, recycle, settings, path)) in infos.iter().enumerate() {
            if *home {
                tab_icons[i] = crate::icons::stock_icon(
                    windows::Win32::UI::Shell::SIID_DESKTOPPC.0,
                    false,
                );
            } else if *recycle {
                tab_icons[i] = crate::icons::stock_icon(
                    if app.recycle_items.is_empty() {
                        windows::Win32::UI::Shell::SIID_RECYCLER.0
                    } else {
                        windows::Win32::UI::Shell::SIID_RECYCLERFULL.0
                    },
                    false,
                );
            } else if *settings {
                tab_glyphs[i] = Some("\u{E713}"); // Settings，与右键菜单同一枚
            } else if !path.as_os_str().is_empty() {
                let h = app.icons.dir_icon(path, false);
                if !h.0.is_null() {
                    tab_icons[i] = Some(h);
                }
            }
        }

        for (i, tab) in app.panes[pi].tabs.iter().enumerate() {
            let r = l.tab_rect(i, n, &app.metrics);
            let hovered = pi == app.focus_pane && app.hover_tab == Some(i);
            let (bg, fg) = if i == app.panes[pi].active {
                (t.tab_active, t.text)
            } else if hovered {
                // 悬停也要看得出来：不然鼠标扫过标签栏没有任何反馈
                (t.hover_bg, t.text)
            } else {
                (t.chrome_bg, t.text_dim)
            };
            fill_radius(app, hdc, "title.tab", &r, bg, None);
            SetTextColor(hdc, fg);
            // 图标 16px 压在左边，文字从它右边开始（资源管理器也是这个排布）
            let icon_s = (16.0 * app.metrics.scale).round() as i32;
            let icon_x = r.left + (8.0 * app.metrics.scale).round() as i32;
            let icon_y = (r.top + r.bottom - icon_s) / 2;
            if let Some(h) = tab_icons[i] {
                app.icons.draw(hdc, h, icon_x, icon_y, icon_s);
            } else if let Some(g) = tab_glyphs[i] {
                let gf = glyph_font(app.metrics.scale);
                let og = SelectObject(hdc, gf.into());
                draw_text(
                    hdc,
                    g,
                    RECT { left: icon_x, top: icon_y, right: icon_x + icon_s, bottom: icon_y + icon_s },
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                );
                SelectObject(hdc, og);
            }
            // 标题文字右侧要留出关闭按钮的位置，否则长标题会被 × 压住
            let mut tr = pad_rect_width(&r, 10);
            tr.left = icon_x + icon_s + (6.0 * app.metrics.scale).round() as i32;
            // 现在每个标签都有 ×，这块地方无条件留出来，不能按 hover 态变
            tr.right -= (18.0 * app.metrics.scale) as i32;
            draw_text(hdc, &tab.title, tr, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
            if i == app.panes[pi].active {
                let accent = CreateSolidBrush(t.accent);
                let line = RECT { left: r.left, top: r.bottom - 2, right: r.right, bottom: r.bottom };
                FillRect(hdc, &line, accent);
                let _ = DeleteObject(accent.into());
            }
            // 关闭按钮：**每个标签都常画**。
            // 早先学资源管理器"只画当前/悬停那个"，结果用户找不到 ×，反馈就是
            // "标签关不掉" —— 看不见的按钮等于不存在。代价是标签栏稍挤一点，
            // 但"能关"比"像"重要。非活动标签的 × 画淡一点，视觉重心仍在当前标签。
            {
                let cr = l.tab_close_rect(i, n, &app.metrics);
                if hovered {
                    fill_radius(app, hdc, "title.tab", &cr, t.hover_bg, None);
                }
                SetTextColor(hdc, if i == app.panes[pi].active || hovered { t.text } else { t.text_dim });
                let g = glyph_font(app.metrics.scale);
                let old_g = SelectObject(hdc, g.into());
                draw_text(hdc, "\u{E8BB}", cr, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
                SelectObject(hdc, old_g);
            }
        }

        // 新建标签（+）：紧跟最后一个标签
        let plus = l.new_tab_rect(n, &app.metrics);
        if l.new_tab_visible && plus.right < l.tabs.right {
            draw_chrome_button(app, hdc, &plus, ChromeGlyph::Plus, false);
        }
        // （原"- ZenFile"字样已按用户要求删掉：标题栏右侧那块地盘留给标签）
        SelectObject(hdc, old);
    }
}

/// 画内容区。分栏时逐个窗格来：先把 `focus_pane` 指到那一个窗格、
/// 再把 `Layout::view` 覆写成它的矩形，于是下面那一大片只认
/// `app.tab()` / `l.view` 的旧代码一行都不用改。
pub fn paint_panes(app: &mut App, hdc: HDC, l: &Layout) {
    if app.panes.len() <= 1 {
        paint_view(app, hdc, l);
        return;
    }
    let saved = app.focus_pane;
    let n = app.panes.len();
    let strip = (26.0 * app.metrics.scale).round() as i32;
    let outer = l.view;
    let t = app.theme;
    for i in 0..n {
        app.focus_pane = i;
        let pr = app.panes[i].rect.to_px(&outer);
        let mut l2 = *l;
        l2.tabs = RECT { left: pr.left, top: pr.top, right: pr.right, bottom: pr.top + strip };
        l2.title = l2.tabs;
        l2.view = RECT {
            left: pr.left,
            top: pr.top + strip,
            right: pr.right,
            bottom: pr.bottom,
        };
        unsafe {
            let b = CreateSolidBrush(t.window_bg);
            FillRect(hdc, &pr, b);
            let _ = DeleteObject(b.into());
        }
        paint_tab_strip(app, hdc, &l2, i);
        paint_view(app, hdc, &l2);
        // 窗格之间的缝：只有"右边和下边"需要画，最外圈本来就是窗口边界。
        unsafe {
            let line = CreateSolidBrush(if i == saved { t.accent } else { t.surface_alt });
            let edge = RECT { left: pr.left, top: pr.bottom - 2, right: pr.right, bottom: pr.bottom };
            FillRect(hdc, &edge, line);
            let edge2 = RECT { left: pr.right - 2, top: pr.top, right: pr.right, bottom: pr.bottom };
            FillRect(hdc, &edge2, line);
            let _ = DeleteObject(line.into());
        }
    }
    app.focus_pane = saved;
}

/// 标题栏自有按钮的形状。**一律用几何图形画，不依赖图标字体** ——
/// Segoe MDL2 Assets 在部分精简系统上没有，缺字时整排按钮变成空心方块。
/// 这几个形状本来就只有横竖和圆点，画出来比字体更干净也更可控。
#[derive(Clone, Copy)]
pub enum ChromeGlyph {
    /// 导航窗格开关：一个方框 + 左侧填实的一栏（"这一栏可以收起来"）
    PaneToggle,
    /// 更多菜单：三个横排圆点
    MoreDots,
    /// 新建标签：十字
    Plus,
}

fn draw_chrome_glyph(app: &App, hdc: HDC, r: &RECT, which: ChromeGlyph) {
    let s = app.metrics.scale;
    let cx = (r.left + r.right) / 2;
    let cy = (r.top + r.bottom) / 2;
    unsafe {
        let pen = CreatePen(PS_SOLID, (1.0 * s).round().max(1.0) as i32, app.theme.text);
        let old_pen = SelectObject(hdc, pen.into());
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        match which {
            ChromeGlyph::PaneToggle => {
                let w = (14.0 * s).round() as i32;
                let h = (11.0 * s).round() as i32;
                let left = cx - w / 2;
                let top = cy - h / 2;
                // 外框
                let _ = Rectangle(hdc, left, top, left + w, top + h);
                // 左侧填实的一栏（宽 5px）：表示"被收起的那一栏"
                let b = CreateSolidBrush(app.theme.text);
                FillRect(hdc, &RECT { left: left + 1, top: top + 1, right: left + (5.0 * s).round() as i32, bottom: top + h - 1 }, b);
                let _ = DeleteObject(b.into());
            }
            ChromeGlyph::MoreDots => {
                let d = ((3.0 * s).round() as i32).max(2);
                let gap = (4.0 * s).round() as i32;
                let b = CreateSolidBrush(app.theme.text);
                for k in -1..=1 {
                    let x = cx + k * (d + gap) - d / 2;
                    let _ = Ellipse(hdc, x, cy - d / 2, x + d, cy - d / 2 + d);
                }
                let _ = DeleteObject(b.into());
            }
            ChromeGlyph::Plus => {
                // 十字用两个**实心细条**画，不用两条 1px 线：GDI 的线在
                // 非整数位置会落半格，交点处还会缺一个像素，看着就是"歪的 +"。
                let arm = (9.0 * s).round() as i32;
                let th = (1.0 * s).round().max(1.0) as i32;
                let b = CreateSolidBrush(app.theme.text);
                FillRect(hdc, &RECT { left: cx - arm / 2, top: cy - th / 2, right: cx + arm / 2 + arm % 2, bottom: cy - th / 2 + th }, b);
                FillRect(hdc, &RECT { left: cx - th / 2, top: cy - arm / 2, right: cx - th / 2 + th, bottom: cy + arm / 2 + arm % 2 }, b);
                let _ = DeleteObject(b.into());
            }
        }
        SelectObject(hdc, old_brush);
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(pen.into());
    }
}

/// 三大金刚键的符号：用直线**画**出来，不走图标字体。
///
/// 之前用的是 Segoe MDL2 Assets 的字符（E921/E922/E923/E8BB）：机器上没装这个
/// 字体时整排键位都变成空心方块，而它又不像菜单图标那样能退回系统图标 ——
/// 标题栏是最不该出错的地方。这三个符号本来就是纯几何图形，用 MoveTo/LineTo
/// 画既不会缺字，也和资源管理器画出来的完全一样。
fn draw_caption_glyph(app: &App, hdc: HDC, r: &RECT, which: u8) {
    let s = app.metrics.scale;
    let cx = (r.left + r.right) / 2;
    let cy = (r.top + r.bottom) / 2;
    // 符号本体 11px（100% 缩放）：1px 笔画在小尺寸下才不至于糊成一团。
    let half = (5.5 * s).round() as i32;
    let box_w = half * 2;
    let th = (1.0 * s).round().max(1.0) as i32;
    unsafe {
        // 横竖笔画一律用**实心细条**画，不用 MoveTo/LineTo：GDI 的线端点不补齐，
        // 转角处会缺一个像素，10px 的尺寸下看着就是"歪的/断的"。
        let brush = CreateSolidBrush(app.theme.text);
        let bar = |left: i32, top: i32, right: i32, bottom: i32| {
            FillRect(hdc, &RECT { left, top, right, bottom }, brush);
        };
        match which {
            1 => {
                // 最小化：底部一条横线
                bar(cx - half, cy + half - th, cx - half + box_w, cy + half);
            }
            2 => {
                let top = cy - half;
                let left = cx - half;
                if IsZoomed(app.hwnd).0 != 0 {
                    // 还原：两个错开的方框（后面那个只画上边和右边）
                    let o = (2.0 * s).round() as i32;
                    bar(left + o, top + o, left + box_w, top + o + th);
                    bar(left + box_w - th, top + o, left + box_w, top + box_w);
                    bar(left, top + o, left + o + th, top + box_w);
                    bar(left, top + o, left + box_w - o, top + o + th);
                    bar(left, top + box_w - th, left + box_w - o, top + box_w);
                    bar(left + box_w - o - th, top + o, left + box_w - o, top + box_w);
                } else {
                    // 最大化：一个方框（四边分别画，比 Rectangle 描边更可控：
                    // Rectangle 的右下角会被裁掉一格）
                    bar(left, top, left + box_w, top + th);
                    bar(left, top + box_w - th, left + box_w, top + box_w);
                    bar(left, top, left + th, top + box_w);
                    bar(left + box_w - th, top, left + box_w, top + box_w);
                }
            }
            _ => {
                // 关闭：一个叉。斜线没法用实心条画，但内缩半格后端点不会顶到边。
                let pen = CreatePen(PS_SOLID, th, app.theme.text);
                let old = SelectObject(hdc, pen.into());
                let a = half - 1;
                MoveToEx(hdc, cx - a, cy - a, None);
                LineTo(hdc, cx + a, cy + a);
                MoveToEx(hdc, cx + a, cy - a, None);
                LineTo(hdc, cx - a, cy + a);
                SelectObject(hdc, old);
                let _ = DeleteObject(pen.into());
            }
        }
        let _ = DeleteObject(brush.into());
    }
}

fn pad_rect_width(r: &RECT, pad: i32) -> RECT {
    RECT { left: r.left + pad, top: r.top, right: r.right - pad, bottom: r.bottom }
}

/// 标题栏按钮（最小化/最大化/关闭）。`which`：1=最小化，2=最大化，3=关闭。
fn draw_caption_button(app: &App, hdc: HDC, r: &RECT, which: u8) {
    let t = &app.theme;
    unsafe {
        let hovered = app.hover_caption == which;
        let pressed = app.pressed_caption == which;
        let bg = if which == 3 && hovered {
            t.danger
        } else if pressed {
            t.surface
        } else if hovered {
            t.hover_bg
        } else {
            t.title_bg
        };
        let b = CreateSolidBrush(bg);
        FillRect(hdc, r, b);
        let _ = DeleteObject(b.into());
        draw_caption_glyph(app, hdc, r, which);
    }
}

/// 标题栏自有按钮（侧栏开关 / 更多菜单）：带悬停底色，观感对齐资源管理器。
fn draw_chrome_button(app: &App, hdc: HDC, r: &RECT, glyph: ChromeGlyph, hover: bool) {
    let t = &app.theme;
    unsafe {
        let bg = if hover { t.hover_bg } else { t.chrome_bg };
        fill_radius(app, hdc, "title.button", r, bg, None);
        draw_chrome_glyph(app, hdc, r, glyph);
    }
}

/// 画一个**圆角**的实心矩形 —— 半径取自「外观」页那棵树。
///
/// 之前这个文件里到处是 `FillRect`，用户在外观点拖的滑杆一点都作用不到真实
/// 界面上，设置页成了摆设。凡是"用户能在外观页调圆角"的色块，一律走这里。
///
/// 两个坑（都踩过）：
/// - `RoundRect` 的第 5/6 个参数是**椭圆的直径**不是半径，直接传半径会画出
///   只有一半大的圆角；
/// - 它用**当前画笔描边、当前刷子填充**：不换笔的话每块圆角色块外面都套一圈
///   默认黑边。所以这里总是显式造一支笔 —— 不描边时就用和填充同色的笔。
pub(crate) unsafe fn fill_radius(
    app: &App,
    hdc: HDC,
    id: &str,
    r: &RECT,
    fill: COLORREF,
    outline: Option<COLORREF>,
) {
    let w = r.right - r.left;
    let h = r.bottom - r.top;
    if w <= 0 || h <= 0 {
        return;
    }
    let rad = app.radius_of(id) as i32;

    if rad <= 0 {
        let br = CreateSolidBrush(fill);
        let _ = FillRect(hdc, r, br);
        let _ = DeleteObject(br.into());
        return;
    }
    let d = (rad * 2).min(w).min(h);
    let pen = CreatePen(PS_SOLID, 1, outline.unwrap_or(fill));
    let old_pen = SelectObject(hdc, pen.into());
    let br = CreateSolidBrush(fill);
    let old_br = SelectObject(hdc, br.into());
    let _ = RoundRect(hdc, r.left, r.top, r.right, r.bottom, d, d);
    SelectObject(hdc, old_br);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(br.into());
    let _ = DeleteObject(pen.into());
}

/// 选中/焦点条目**行首的强调竖线**（列表行、大图标磁贴共用这一套）。
///
/// 形状四参数由「设置-交互」页调整：宽（厚度）/ 高（纵向长度，0 = 跟宿主
/// 背景同高）/ 粗细（空心时的描边线宽）/ 是否空心。
///
/// **竖条必须完全落在宿主底色之内，且不能短得看不见**（用户要求："不要显示到
/// 白色框外边，不允许超出"）。
///
/// 做法是**用 Region 裁剪**，而不是拍脑袋算纵向内缩：
/// 先造一个跟底色**完全同参数**的圆角矩形 Region（`CreateRoundRectRgn`
/// 与 `fill_radius` 同一套半径算法），再和"左起 bar_w 宽、纵向 inset..bottom-inset"
/// 的矩形求交（RGN_AND）。结果：
///   - 左端形状与底色弧**逐像素同形**，物理上不可能戳到框外；
///   - 圆角多大就自动贴多少，不用维护内缩公式（之前那个半吊子公式在
///     rad=14 / 行高 21 时只算出 1px，竖线几乎整个消失）。
pub(crate) unsafe fn draw_sel_bar(app: &App, hdc: HDC, host: RECT, t: Theme) {
    let sb = &app.cfg.ui.sel_bar;
    let sc = app.metrics.scale;
    let host_w = host.right - host.left;
    let host_h = host.bottom - host.top;
    if host_h <= 0 || host_w <= 0 {
        return;
    }
    let bar_w = (((sb.w as f32) * sc).round().max(1.0) as i32).min(host_w);
    // 与 fill_radius 完全同口径的有效半径（椭圆直径 = 半径×2，再被宽高夹住）
    let r = ((app.radius_of("content.selected") as i32) * 2).min(host_w).min(host_h) / 2;
    // 纵向内缩：留出圆角占掉的高度，但**至少给竖条留一半高度**，否则 rad 大于
    // 行高一半时（rad=14 / 行高 21 → 有效半径 10）竖线会被压没。
    let inset = (r).min((host_h - bar_w.max(1)) / 2).max(0);
    let inner_top = host.top + inset;
    let inner_bot = host.bottom - inset;
    let bar_h = if sb.h == 0 {
        inner_bot - inner_top
    } else {
        (((sb.h as f32) * sc).round().max(1.0) as i32).min(inner_bot - inner_top)
    };
    if bar_h <= 0 {
        return;
    }
    let bar_top = inner_top + (inner_bot - inner_top - bar_h) / 2;
    let clip = CreateRoundRectRgn(
        host.left, host.top, host.right, host.bottom, r * 2, r * 2,
    );
    let band = CreateRectRgn(host.left, bar_top, host.left + bar_w, bar_top + bar_h);
    let _ = CombineRgn(Some(clip), Some(clip), Some(band), RGN_AND);
    let _ = DeleteObject(band.into());

    let saved = SaveDC(hdc);
    let _ = SelectObject(hdc, clip.into());
    if sb.hollow {
        // 空心：只描边不填色。线宽太粗会盖满整个条，clamp 到条的短边以内。
        let stroke = (((sb.stroke as f32) * sc).round().max(1.0) as i32)
            .min(bar_w)
            .min(bar_h);
        let p = CreatePen(PS_SOLID, stroke, t.accent);
        let op = SelectObject(hdc, p.into());
        let ob = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        // GDI Rectangle 画的是 [left, right) —— +1 让可见宽正好 bar_w，
        // 与实心版同一口径。
        let _ = Rectangle(
            hdc,
            host.left,
            bar_top,
            host.left + bar_w + 1,
            bar_top + bar_h + 1,
        );
        let _ = SelectObject(hdc, ob);
        let _ = SelectObject(hdc, op);
        let _ = DeleteObject(p.into());
    } else {
        let br = CreateSolidBrush(t.accent);
        let _ = FillRgn(hdc, clip, br);
        let _ = DeleteObject(br.into());
    }
    let _ = RestoreDC(hdc, saved);
    let _ = DeleteObject(clip.into());
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

// ---------------------------------------------------------------- 地址栏（Win11 面包屑）

/// 面包屑排版结果。**绘制与命中测试共用这一份**（老纪律：两边各写一份常量，
/// 改一次就错位）。
pub struct AddressBar {
    pub crumbs: Vec<crate::app::Crumb>,
    /// 与 `crumbs` 一一对应的命中矩形
    pub rects: Vec<RECT>,
    /// 相邻两节之间的分隔箭头矩形（纯装饰，不参与命中）
    pub seps: Vec<RECT>,
}

/// 量一段文字在指定字体下的像素宽度。
///
/// 面包屑排版必须量真实宽度：按"字数 × 平均宽"估算在中英混排下必然错位
/// （"用户" 两字和 "Administrator" 十三个字母完全不是一个尺度）。
pub(crate) unsafe fn text_width(hdc: HDC, font: HFONT, text: &str) -> i32 {
    let old = SelectObject(hdc, font.into());
    let w: Vec<u16> = text.encode_utf16().collect();
    let mut sz = SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &w, &mut sz);
    SelectObject(hdc, old);
    sz.cx
}

/// 把当前路径排成资源管理器那种"一节一节"的地址栏。
///
/// 宽度不够时按 Win11 的做法折叠中间层级：保留"此电脑"和末尾几节，中间压成一个
/// `…`（点它跳到被折叠的最深一层）。**先量宽再决定折叠** —— 少了这一步，路径一深
/// 就会画出右边框外面去。
pub fn address_bar(app: &mut App, l: &Layout) -> AddressBar {
    let scale = app.metrics.scale;
    let mut crumbs = app.address_crumbs();
    let pad = (8.0 * scale).round() as i32;
    let cpad = (5.0 * scale).round() as i32;
    let avail = (l.address.right - l.address.left - pad * 2).max(20);
    let font = ui_font(scale);
    let hwnd = app.hwnd;
    let hdc = unsafe { windows::Win32::Graphics::Gdi::GetDC(Some(hwnd)) };
    if hdc.is_invalid() {
        return AddressBar { crumbs, rects: Vec::new(), seps: Vec::new() };
    }
    let width_of = |s: &str| unsafe { text_width(hdc, font, s) };
    let sep_w = width_of("\u{203A}") + (2.0 * scale) as i32;
    let ell_w = width_of("\u{2026}") + cpad * 2;

    let widths: Vec<i32> = crumbs.iter().map(|c| width_of(&c.label) + cpad * 2).collect();
    let n = widths.len();
    let total: i32 = widths.iter().sum::<i32>() + sep_w * n.saturating_sub(1) as i32;
    if total > avail && n > 2 {
        // 从右往左累计能塞下的节；剩下的中间层级折成一个 `…`
        let mut used = widths[n - 1];
        let mut start = n - 1;
        let mut i = n - 1;
        while i > 1 {
            i -= 1;
            let need = widths[i] + sep_w;
            if used + need + sep_w + ell_w + sep_w + widths[0] > avail {
                break;
            }
            used += need;
            start = i;
        }
        if start > 1 {
            let ell_target = crumbs[start - 1].target.clone();
            let mut v = vec![crumbs[0].clone()];
            v.push(crate::app::Crumb {
                label: "\u{2026}".into(),
                target: ell_target,
                ellipsis: true,
            });
            v.extend_from_slice(&crumbs[start..]);
            crumbs = v;
        // 连"此电脑 + … + 最后一节"都放不下：只留最后一节，宁可截断也不越界
        } else {
            crumbs = vec![crumbs[n - 1].clone()];
        }
    }

    let widths: Vec<i32> = crumbs.iter().map(|c| width_of(&c.label) + cpad * 2).collect();
    let mut rects = Vec::with_capacity(crumbs.len());
    let mut seps = Vec::with_capacity(crumbs.len());
    let top = l.address.top + 1;
    let bottom = l.address.bottom - 1;
    let mut x = l.address.left + pad;
    let limit = l.address.right - 2;
    for (i, w) in widths.iter().enumerate() {
        let r = RECT { left: x, top, right: (x + w).min(limit), bottom };
        rects.push(r);
        x += w;
        if i + 1 < widths.len() {
            seps.push(RECT { left: x, top, right: x + sep_w, bottom });
            x += sep_w;
        }
    }
    unsafe {
        let _ = ReleaseDC(Some(hwnd), hdc);
    }
    AddressBar { crumbs, rects, seps }
}

/// 命中测试：返回被点到的面包屑下标。
pub fn address_hit(bar: &AddressBar, x: i32, y: i32) -> Option<usize> {
    for (i, r) in bar.rects.iter().enumerate() {
        if x >= r.left && x <= r.right && y >= r.top && y <= r.bottom {
            return Some(i);
        }
    }
    None
}

/// 画地址栏：平时是面包屑，编辑态时让 EDIT 自己画（它此时才显示出来）。
fn paint_address(app: &mut App, hdc: HDC, l: &Layout) {
    let t = app.theme;
    unsafe {
        // 底 + 1px 边框（EDIT 与工具栏同色，没有框就等于看不见）
        fill_radius(app, hdc, "address.edit", &l.address, t.surface, Some(t.border));
        if app.addr_editing {
            return;
        }

        let bar = address_bar(app, l);
        // 必须 Save/Restore：这是**整窗共用的内存 DC**，裁剪区一改，后面画的
        // 导航窗格、列表、状态栏全被裁进地址框里（整窗变成一片空白）。
        let saved = SaveDC(hdc);
        let _ = IntersectClipRect(
            hdc,
            l.address.left + 1,
            l.address.top + 1,
            l.address.right - 1,
            l.address.bottom - 1,
        );
        let font = ui_font(app.metrics.scale);
        let old = SelectObject(hdc, font.into());
        SetBkMode(hdc, TRANSPARENT);
        for (i, c) in bar.crumbs.iter().enumerate() {
            let r = match bar.rects.get(i) {
                Some(r) => *r,
                None => continue,
            };
            let last = i + 1 == bar.crumbs.len();
            if app.addr_hover == Some(i) {
                fill_radius(app, hdc, "address.crumb", &r, t.hover_bg, None);
            }
            SetTextColor(hdc, if last { t.text } else { t.text_dim });
            let mut w: Vec<u16> = c.label.encode_utf16().collect();
            DrawTextW(
                hdc,
                &mut w,
                &mut r.clone(),
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
        }
        SetTextColor(hdc, t.text_dim);
        for s in &bar.seps {
            let mut w: Vec<u16> = "\u{203A}".encode_utf16().collect();
            DrawTextW(
                hdc,
                &mut w,
                &mut s.clone(),
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
        }
        SelectObject(hdc, old);
        let _ = RestoreDC(hdc, saved);
    }
}

pub fn paint_toolbar(app: &mut App, hdc: HDC, l: &Layout) {
    let t = app.theme;
    unsafe {
        let brush = CreateSolidBrush(t.chrome_bg);
        FillRect(hdc, &l.toolbar, brush);
        let _ = DeleteObject(brush.into());
        let g = glyph_font(app.metrics.scale);
        let old_g = SelectObject(hdc, g.into());
        SetBkMode(hdc, TRANSPARENT);
        // 三个导航按钮：悬停要给一层底色，否则它们看着是三个图标而不是三个按钮。
        // 配色沿用列表行的那一套（悬停 row_hover、不可用则不亮）。
        for (i, r) in [(1u8, l.back), (2, l.forward), (3, l.up)] {
            if app.hover_nav != i {
                continue;
            }
            fill_radius(app, hdc, "toolbar.button", &r, t.row_hover, None);
        }
        SetTextColor(hdc, if app.can_back() { t.text } else { t.text_dim });
        draw_text(hdc, "\u{E72B}", l.back, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_CENTER);
        SetTextColor(hdc, if app.can_forward() { t.text } else { t.text_dim });
        draw_text(hdc, "\u{E72A}", l.forward, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_CENTER);
        SetTextColor(hdc, t.text);
        draw_text(hdc, "\u{E74A}", l.up, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_CENTER);
        SelectObject(hdc, old_g);

        // 搜索框底色 + 1px 边框：否则 EDIT 与工具栏同色，肉眼不可见。
        // 地址栏交给 paint_address —— 它平时是面包屑，只有编辑态才露出 EDIT。
        paint_address(app, hdc, l);
        {
            let r = &l.search;
            fill_radius(app, hdc, "address.edit", r, t.surface, Some(t.border));
        }

        // 「批量重命名」图标按钮：**无条件画**（置顶显示）。
        // 虚拟视图（主页/回收站/设置）里画成淡化的不可用态，而不是整块消失 ——
        // 一个按钮时有时无，用户会以为它丢了。
        paint_bulk_button(app, hdc, l);

        // 工具栏下分隔线，否则与列表区糊成一片
        let lb = CreateSolidBrush(t.border);
        FillRect(hdc, &RECT { left: 0, top: l.toolbar.bottom - 1, right: l.toolbar.right, bottom: l.toolbar.bottom }, lb);
        let _ = DeleteObject(lb.into());

        // 加载/搜索进度条画在这一条线上（见 paint_busy_strip）
        paint_busy_strip(app, hdc, l);
    }
}

/// 目录栏最右侧这个按钮**此刻能不能用**。虚拟视图（主页/回收站/设置）没有
/// 可批量重命名的条目 —— 但按钮照样画在那儿，只是淡化且点不动。
///
/// 绘制与命中都走这一个判断，免得"看得见点不到"。
pub fn bulk_rename_available(app: &App) -> bool {
    !crate::app::is_virtual(&app.tab().path)
}

/// 「批量重命名」图标按钮。点一下在按钮正下方弹出五种模式的下拉菜单。
///
/// 做成纯图标（E8AC"重命名" + 右下角一个小三角表示有下拉）而不是文字按钮：
/// 文字版占 132px，会把地址栏的面包屑挤没。图标版 30px，地址栏拿回 100px。
fn paint_bulk_button(app: &App, hdc: HDC, l: &Layout) {
    let t = app.theme;
    let s = app.metrics.scale;
    let enabled = bulk_rename_available(app);
    let hovered = app.hover_bulk && enabled;
    unsafe {
        let r = &l.bulk_btn;
        fill_radius(
            app,
            hdc,
            "toolbar.button",
            r,
            if hovered { t.hover_bg } else { t.surface },
            Some(if hovered { t.accent } else { t.border }),
        );

        SetBkMode(hdc, TRANSPARENT);
        // 主字形：占据整块（略偏左上，给右下角的小三角腾位置）
        let g = glyph_font(s);
        let og = SelectObject(hdc, g.into());
        SetTextColor(hdc, if enabled { t.text } else { t.text_dim });
        let gr = RECT {
            left: r.left - (2.0 * s) as i32,
            top: r.top - (3.0 * s) as i32,
            right: r.right - (1.0 * s) as i32,
            bottom: r.bottom - (3.0 * s) as i32,
        };
        draw_text(hdc, "\u{E8AC}", gr, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        // 右下角小三角："这是一个下拉，不是普通按钮"。用一个更小的字形字号，
        // 免得它和主字形抢视觉重量。
        let small = caret_font(s);
        let os = SelectObject(hdc, small.into());
        SetTextColor(hdc, t.text_dim);
        let ar = RECT {
            left: r.left + (16.0 * s) as i32,
            top: r.bottom - (14.0 * s) as i32,
            right: r.right,
            bottom: r.bottom - (2.0 * s) as i32,
        };
        draw_text(hdc, "\u{E70D}", ar, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        SelectObject(hdc, os);
        SelectObject(hdc, og);
    }
}

/// 目录栏下沿的不定量进度条（Win11 资源管理器同款）。
///
/// 列目录、整卷搜索、建索引都给不出百分比，只能"有事在跑"这一个布尔量 ——
/// 所以画的是来回滑动的一小段，而不是假的确定进度。原先在列表区写
/// "正在列举…" 的纯文字已经去掉了。
fn paint_busy_strip(app: &App, hdc: HDC, l: &Layout) {
    if !app.busy() {
        return;
    }
    let scale = app.metrics.scale;
    let t = app.theme;
    let h = ((2.0 * scale).round() as i32).max(2);
    let left = l.toolbar.left;
    let right = l.toolbar.right;
    let w = (right - left).max(1);
    let top = l.toolbar.bottom - h;
    unsafe {
        let base = CreateSolidBrush(t.surface);
        FillRect(hdc, &RECT { left, top, right, bottom: l.toolbar.bottom }, base);
        let _ = DeleteObject(base.into());
        // 一段长度约 1/4 宽的亮条，在 [0, w+seg] 上循环滑动。
        // GetTickCount64 直接取，不额外存状态：反正是纯装饰，掉几帧无所谓。
        let seg = ((w as f32) * 0.25).round().max(24.0) as i32;
        let span = w + seg;
        let phase = (GetTickCount64() % 1100) as f32 / 1100.0;
        let x0 = left - seg + (span as f32 * phase).round() as i32;
        let x1 = (x0 + seg).min(right);
        if x1 > left {
            let b = CreateSolidBrush(t.accent);
            FillRect(hdc, &RECT { left: x0.max(left), top, right: x1, bottom: l.toolbar.bottom }, b);
            let _ = DeleteObject(b.into());
        }
    }
}

/// 进度条在动，得有人周期性地擦它。只重画这一条，别整窗重绘。
pub fn tick_busy_strip(hwnd: HWND, app: &App) {
    if !app.busy() {
        return;
    }
    let l = layout_for(app);
    let scale = app.metrics.scale;
    let h = ((2.0 * scale).round() as i32).max(2);
    let r = RECT {
        left: l.toolbar.left,
        top: l.toolbar.bottom - h,
        right: l.toolbar.right,
        bottom: l.toolbar.bottom,
    };
    unsafe {
        let _ = InvalidateRect(Some(hwnd), Some(&r), false);
    }
}

/// 侧栏/菜单共用的树形箭头：一个 7px 的**实心**三角。
///
/// 收起 = 朝右（▶），展开 = 朝下（▼）。尺寸比之前的字形小近一半 —— 之前是
/// 16px 的 Segoe MDL2 字符，图标格大半是留白，看着又大又糊。
pub fn draw_tree_chevron(hdc: HDC, r: &RECT, color: COLORREF, down: bool) {
    let cx = (r.left + r.right) / 2;
    let cy = (r.top + r.bottom) / 2;
    let a = ((r.right - r.left) * 7 / 16).max(3); // 槽位 16px → 三角 7px
    let pts = if down {
        // ▼：顶点在下
        [
            POINT { x: cx - a, y: cy - a / 2 },
            POINT { x: cx + a, y: cy - a / 2 },
            POINT { x: cx, y: cy + a / 2 + 1 },
        ]
    } else {
        // ▶：顶点在右
        [
            POINT { x: cx - a / 2, y: cy - a },
            POINT { x: cx - a / 2, y: cy + a },
            POINT { x: cx + a / 2 + 1, y: cy },
        ]
    };
    unsafe {
        let brush = CreateSolidBrush(color);
        let old = SelectObject(hdc, brush.into());
        let old_pen = SelectObject(hdc, GetStockObject(NULL_PEN));
        let _ = Polygon(hdc, &pts);
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old);
        let _ = DeleteObject(brush.into());
    }
}

/// 表头上的排序指示：**朝上/朝下**的一对实心三角。
///
/// 之前直接拿树形箭头来画，而树形箭头的两种形态是"朝右/朝下" —— 于是升序那
/// 一下画出来的是一个**横着的**箭头（"有一个箭头变成侧的了"就是这么来的）。
/// 排序只有上下两个方向，必须是一对专门的上/下三角。
pub fn draw_sort_arrow(hdc: HDC, r: &RECT, color: COLORREF, descending: bool) {
    let cx = (r.left + r.right) / 2;
    let cy = (r.top + r.bottom) / 2;
    let a = ((r.right - r.left) * 6 / 16).max(3);
    let pts = if descending {
        // ▼
        [
            POINT { x: cx - a, y: cy - a / 2 },
            POINT { x: cx + a, y: cy - a / 2 },
            POINT { x: cx, y: cy + a / 2 + 1 },
        ]
    } else {
        // ▲
        [
            POINT { x: cx - a, y: cy + a / 2 },
            POINT { x: cx + a, y: cy + a / 2 },
            POINT { x: cx, y: cy - a / 2 - 1 },
        ]
    };
    unsafe {
        let brush = CreateSolidBrush(color);
        let old = SelectObject(hdc, brush.into());
        let old_pen = SelectObject(hdc, GetStockObject(NULL_PEN));
        let _ = Polygon(hdc, &pts);
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old);
        let _ = DeleteObject(brush.into());
    }
}

pub fn paint_sidebar(app: &mut App, hdc: HDC, l: &Layout) {
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

        // 导航树：主页 + 常用文件夹 + 此电脑（展开=全部盘符）+ 回收站。
        // 排列本身由 App 产出，绘制与命中测试共用同一份，不存在"看得见点不到"。
        let rows = app.sidebar_rows();
        let cur = std::fs::canonicalize(&app.tab().path).unwrap_or_default();
        // 图标句柄先取好再画：dir_icon 需要 &mut，不能一边借 rows 一边改缓存
        let mut icons: Vec<Option<HICON>> = vec![None; rows.len()];
        let mut glyphs: Vec<Option<&'static str>> = vec![None; rows.len()];
        for (i, r) in rows.iter().enumerate() {
            match &r.icon {
                crate::app::SidebarIcon::Glyph(g) => glyphs[i] = Some(g),
                crate::app::SidebarIcon::Path(p) => {
                    let h = app.icons.dir_icon(p, true);
                    if !h.0.is_null() {
                        icons[i] = Some(h);
                    }
                }
                // 回收站这类没有实体路径的目标：走系统库存图标（空/满两态）
                crate::app::SidebarIcon::Stock(siid) => icons[i] = crate::icons::stock_icon(*siid, true),
            }
        }

        // 整棵树的绘制裁剪在导航窗格矩形内。
        //
        // 之前只跳过"整行都在窗格之上"的那些行，**横跨上沿的那一行照画**，
        // 于是滚动时最上面那一行会有一截画到工具栏上去 —— 正好压在"返回/
        // 前进/向上"三个按钮上（用户看到的"侧栏溢出到三个按钮上"）。
        let _ = SaveDC(hdc);
        let _ = IntersectClipRect(hdc, l.sidebar.left, l.sidebar.top, l.sidebar.right, l.sidebar.bottom);

        for (i, row) in rows.iter().enumerate() {
            // 分组之间的空行：只占位、不画也不参与命中（见 `SidebarRow.spacer`）
            if row.spacer {
                continue;
            }
            let (row_r, chev_r, icon_r, text_r) =
                sidebar_row_geoms(l, i, row.depth, scale, app.sidebar_scroll);
            if row_r.top >= l.sidebar.bottom {
                break;
            }
            // 滚到窗格上沿之上的行：整行跳过（不画也不参与命中）
            if row_r.bottom <= l.sidebar.top {
                continue;
            }
            // 树线：这一行才是"树"和"栅格"的分界。
            //
            // 之前是"每一级缩进都画一条通天竖线"，结果是一排等距平行线，
            // 看着像表格。真树的关键在于两件事：
            //   1) 竖线只在**后面还有兄弟**的那一层画（最后一个孩子的子树里
            //      就没有这条线了 —— `guides[k]` 就是干这个的）；
            //   2) 每个节点都有一条从父级竖线伸出来的**肘形连接线**，
            //      末位孩子那条父级竖线在行中线收口（画成 └ 而不是 ├）。
            if row.has_parent && row.depth > 0 {
                let lb = CreateSolidBrush(t.border);
                let chev_s = (16.0 * scale).round() as i32;
                let step = (16.0 * scale).round() as i32;
                let base = l.sidebar.left + (4.0 * scale).round() as i32;
                let line_x = |k: usize| base + step * k as i32 + chev_s / 2;
                for k in 1..row.depth {
                    if row.guides.get(k).copied().unwrap_or(false) {
                        let x = line_x(k);
                        FillRect(
                            hdc,
                            &RECT { left: x, top: row_r.top, right: x + 1, bottom: row_r.bottom },
                            lb,
                        );
                    }
                }
                // 肘形连接线：从父级那一层的竖线拉到本行的箭头上
                let px = line_x(row.depth - 1);
                let cy = (row_r.top + row_r.bottom) / 2;
                let vbottom = if row.last_child { cy + 1 } else { row_r.bottom };
                FillRect(hdc, &RECT { left: px, top: row_r.top, right: px + 1, bottom: vbottom }, lb);
                let hx1 = base + step * row.depth as i32 + 2;
                FillRect(hdc, &RECT { left: px, top: cy, right: hx1, bottom: cy + 1 }, lb);
                let _ = DeleteObject(lb.into());
            }

            let active = sidebar_row_active(app, row, &cur);
            let hovered = app.sidebar_hover == Some(i);
            if active || hovered {
                let only_hover = hovered && !active;
                fill_radius(
                    app,
                    hdc,
                    if only_hover { "sidebar.hover" } else { "sidebar.selected" },
                    &row_r,
                    if only_hover { t.surface } else { t.hover_bg },
                    None,
                );
            }
            if active {
                // 左侧 3px 强调条：和菜单悬停保持一致的语言，指示"当前位置"。
                // 跟着条目的圆角一起倒角 —— 底色是圆的、竖线是方的，竖线就会
                // 从圆角外面戳出来一小截。
                fill_radius(
                    app,
                    hdc,
                    "sidebar.selected",
                    &RECT { left: row_r.left, top: row_r.top, right: row_r.left + (3.0 * scale) as i32, bottom: row_r.bottom },
                    t.accent,
                    None,
                );
            }
            SetTextColor(hdc, if active { t.text } else { t.text_dim });

            // 展开箭头：可展开的节点才画，指向随状态变（▶ / ▼）。
            //
            // 用**实心三角形**画而不是图标字体：字形在 16px 里带一堆留白和抗锯齿
            // 灰边，暗色下看着就是一坨糊的；实心三角 7px，边界干净、尺寸可控。
            if row.expandable {
                draw_tree_chevron(hdc, &chev_r, t.text_dim, row.expanded);
            }

            match icons[i] {
                Some(h) => {
                    app.icons.draw(hdc, h, icon_r.left, icon_r.top, icon_r.right - icon_r.left);
                }
                None => {
                    if let Some(glyph) = glyphs[i] {
                        let g = sidebar_glyph_font(scale);
                        let old2 = SelectObject(hdc, g.into());
                        let mut gbuf: Vec<u16> = glyph.encode_utf16().collect();
                        DrawTextW(hdc, &mut gbuf, &mut icon_r.clone(), DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
                        SelectObject(hdc, old2);
                    }
                }
            }
            draw_text(hdc, &row.label, text_r, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
        }
        SelectObject(hdc, old);

        // （原导航窗格右上角的折叠按钮已按用户要求删除：折叠走标题栏最左那个开关，
        //  窗格里再放一个既占位又容易误点）
        let _ = RestoreDC(hdc, -1);
    }
}

/// 导航窗格与内容区之间的分隔线（可拖宽/拖窄导航窗格）。
///
/// 命中带 3px 容差：分隔线本身只有 1px，让人去对准 1px 是折磨。
pub fn sidebar_split_rect(l: &Layout) -> RECT {
    RECT {
        left: l.sidebar.right - 3,
        top: l.sidebar.top,
        right: l.sidebar.right + 3,
        bottom: l.sidebar.bottom,
    }
}

/// 详细信息表头上可拖的几条竖线：`(列序号, x)`。
///
/// 规则是"第 n 列的右边线 = 下一列的起点"，最后一列也一样（资源管理器允许
/// 拖 大小 的右边线）。列序号固定为 0=名称 1=修改日期 2=类型 3=大小 ——
/// 搜索态多出来的"位置"列不改序号，否则拖一次就拖错列。
pub fn header_splits(app: &App, l: &Layout) -> Vec<(usize, i32)> {
    if app.tab().view_mode != fm_core::ViewMode::Details {
        return Vec::new();
    }
    // 详细信息视图的图标恒为 16（与 paint_view 里同一套取值）
    let icon_size = (16.0 * app.metrics.scale) as i32;
    let cols = detail_cols(app, l, icon_size);
    let next_name = cols.path.unwrap_or(cols.date);
    vec![
        (0, next_name),
        (1, cols.kind),
        (2, cols.size),
        (3, cols.size + cols.size_w),
    ]
}

/// 光标落在表头的哪一列上（0=名称，搜索态 1=位置，其后 修改日期/类型/大小）。
///
/// 绘制与命中共用 `detail_cols` 同一份列几何。表头的"筛选下拉"和右键菜单都
/// 靠它定位：各自去数一遍列的话，拖过一次列宽就对不上了。
pub fn header_col_hit(app: &App, l: &Layout, x: i32, y: i32) -> Option<usize> {
    if app.tab().view_mode != fm_core::ViewMode::Details {
        return None;
    }
    let header_h = (26.0 * app.metrics.scale) as i32;
    if y < l.view.top || y >= l.view.top + header_h || x < l.view.left || x >= l.view.right {
        return None;
    }
    let icon_size = (16.0 * app.metrics.scale) as i32;
    let cols = detail_cols(app, l, icon_size);
    let mut list: Vec<(i32, i32)> = vec![(cols.name, cols.name_w)];
    if let Some(p) = cols.path {
        list.push((p, cols.path_w));
    }
    list.push((cols.date, cols.date_w));
    list.push((cols.kind, cols.kind_w));
    list.push((cols.size, cols.size_w));
    for (i, (x0, w)) in list.iter().enumerate() {
        let right = (*x0 + w).min(l.view.right);
        if x >= *x0 && x < right.max(*x0) {
            return Some(i);
        }
    }
    None
}

/// 表头里"筛选下拉"那个小三角的矩形（悬停时才画）。绘制与命中共用。
pub fn header_filter_rect(app: &App, l: &Layout, col: usize) -> RECT {
    let s = app.metrics.scale;
    let icon_size = (16.0 * s) as i32;
    let cols = detail_cols(app, l, icon_size);
    let mut list: Vec<(i32, i32)> = vec![(cols.name, cols.name_w)];
    if let Some(p) = cols.path {
        list.push((p, cols.path_w));
    }
    list.push((cols.date, cols.date_w));
    list.push((cols.kind, cols.kind_w));
    list.push((cols.size, cols.size_w));
    let (x0, w) = list.get(col).copied().unwrap_or((l.view.left, 0));
    let right = (x0 + w).min(l.view.right);
    let aw = (16.0 * s) as i32;
    RECT {
        left: (right - aw).max(x0),
        top: l.view.top,
        right,
        bottom: l.view.top + (26.0 * s) as i32,
    }
}

/// 把 `header_col_hit` 给出的**显示序号**翻译成语义列号：
/// 0=名称 1=修改日期 2=类型 3=大小 4=位置。
///
/// 搜索态会在名称后面插一列"位置"，于是显示序号和语义列号不再是同一个东西
/// —— 筛选和排序认的是语义列号，各自在调用点手写一遍这个映射迟早错位。
pub fn header_col_semantic(app: &App, i: usize) -> u8 {
    let searching = app.tab().search_root.is_some();
    if searching {
        match i {
            0 => 0,
            1 => 4,
            2 => 1,
            3 => 2,
            _ => 3,
        }
    } else {
        match i {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 3,
        }
    }
}

/// 光标落在哪条分隔线上（带 4px 容差）。
pub fn split_hit(app: &App, l: &Layout, x: i32, y: i32) -> Option<crate::app::Split> {
    if app.cfg.ui.sidebar_visible {
        let r = sidebar_split_rect(l);
        if y >= l.sidebar.top && y <= l.sidebar.bottom && x >= r.left && x <= r.right {
            return Some(crate::app::Split::Sidebar);
        }
    }
    // 列分隔线只在"详细信息"下存在 —— 别的布局压根没有表头（名称/大小那一行），
    // 不判模式的话，网格视图顶部 26px 里也会莫名冒出可拖的列边界。
    if app.tab().view_mode != fm_core::ViewMode::Details {
        return None;
    }
    let header_h = (26.0 * app.metrics.scale) as i32;
    if y >= l.view.top && y <= l.view.top + header_h {
        for (i, sx) in header_splits(app, l) {
            if (x - sx).abs() <= 4 {
                return Some(crate::app::Split::Column(i));
            }
        }
    }
    None
}

/// 该导航项是不是虚拟的"主页"（没有真实路径可问 Shell 要图标）。
pub fn is_home_target(target: &str) -> bool {
    target == crate::app::HOME_PATH
}

/// 该导航项是不是虚拟的"回收站"。
pub fn is_recycle_target(target: &str) -> bool {
    target == crate::app::RECYCLE_PATH
}

/// 导航窗格条目。第一项必须是"主页"：与资源管理器一致，点它进首页
/// （常用文件夹 + 本机驱动器），而不是直接落进 C 盘。
/// 命中测试：光标落在导航窗格的第几行上（超出可见区返回 None）。
pub fn sidebar_hit(
    app: &mut crate::app::App,
    l: &Layout,
    x: i32,
    y: i32,
    scale: f32,
) -> Option<(usize, bool)> {
    let scroll = app.sidebar_scroll;
    // 光标不在导航窗格的**可见范围**里就直接不算命中。
    // 少了这一句，滚出窗格上沿的那些行（画的时候被裁掉了）依然点得中，
    // 于是"在工具栏三个按钮上点一下"会命中看不见的导航行。
    if y < l.sidebar.top || y >= l.sidebar.bottom || x < l.sidebar.left || x >= l.sidebar.right {
        return None;
    }
    let rows = app.sidebar_rows();
    for (i, r) in rows.iter().enumerate() {
        // 分组空行不参与命中：点在它上面不该选中任何东西
        if r.spacer {
            continue;
        }
        let (row_r, chev_r, _, _) = sidebar_row_geoms(l, i, r.depth, scale, scroll);
        if row_r.top >= l.sidebar.bottom {
            break;
        }
        if y < row_r.top || y >= row_r.bottom || x < row_r.left || x >= row_r.right {
            continue;
        }
        let on_chevron = r.expandable
            && x >= chev_r.left - 2
            && x <= chev_r.right + 2
            && y >= chev_r.top
            && y <= chev_r.bottom;
        return Some((i, on_chevron));
    }
    None
}

/// 这一行是不是"当前所在位置"。
fn sidebar_row_active(
    app: &crate::app::App,
    row: &crate::app::SidebarRow,
    cur: &std::path::Path,
) -> bool {
    if app.tab().home {
        // 首页已经并到"此电脑"这一格（不再有单独的"主页"），所以停在主页时
        // 亮的就是它。
        return row.key == crate::app::SIDEBAR_THISPC;
    }
    if app.tab().recycle {
        return is_recycle_target(&row.target);
    }
    if is_home_target(&row.target) || is_recycle_target(&row.target) {
        return false;
    }
    // "此电脑"这一行自己也指向首页，不该在停在某个盘里时跟着亮
    if row.key == crate::app::SIDEBAR_THISPC {
        return false;
    }
    !cur.as_os_str().is_empty()
        && std::fs::canonicalize(std::path::PathBuf::from(&row.target))
            .map(|p| p == cur)
            .unwrap_or(false)
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
            // 首页磁贴同样给悬停反馈，强度比选中弱一档（见列表行的注释）
            let focused = app.focus_visible && n == focus_row;
            if !sel && !focused && app.hover_row == Some(tile.index) {
                fill_radius(app, hdc, "content.hover", &tile.rect, t.row_hover, None);
            }
            if sel || focused {
                fill_radius(
                    app,
                    hdc,
                    "content.selected",
                    &tile.rect,
                    if sel { t.selection_bg } else { t.hover_bg },
                    None,
                );
                // 与列表行 / 菜单悬停项同一套语言：选中态在左沿压一条强调竖线
                //（形状与列表行同源，「设置-交互」页统一调）
                draw_sel_bar(app, hdc, tile.rect, t);
            }
            SetTextColor(hdc, if sel { t.selection_text } else { t.text });
            app.icons.draw(hdc, icons[n], tile.icon_x, tile.icon_y, tile.icon_size);
            let Some(e) = app.tab().entries.get(tile.index) else { continue };

            if let Some(bar) = tile.bar {
                // 驱动磁贴：名称 + 容量条 + 可用/总量
                let drive = crate::app::drive_of(app, &e.path);
                let mut name: Vec<u16> = e.name.encode_utf16().collect();
                let mut nr = tile.label;
                nr.bottom = nr.top + (20.0 * scale) as i32;
                DrawTextW(hdc, &mut name, &mut nr, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);

                let text = match drive.and_then(|d| Some((d.total?, d.available?))) {
                    // 资源管理器的措辞就是"可用/共"，口径保持一致
                    Some((total, free)) => format!("{} 可用，共 {}", human(free), human(total)),
                    // 空读卡器、没放盘的光驱：不是"未知"，是"还没就绪"
                    None => match drive {
                        Some(d) if !d.ready => "未就绪".to_string(),
                        _ => "".to_string(),
                    },
                };
                let mut sub: Vec<u16> = text.encode_utf16().collect();
                SetTextColor(hdc, if sel { t.selection_text } else { t.text_dim });
                let mut sr = tile.label;
                sr.top = nr.bottom;
                sr.bottom = bar.top - 2;
                DrawTextW(hdc, &mut sub, &mut sr, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX);

                // 容量条：轨道 + **剩余**占比。
                //
                // 两点精度要求：
                // 1) 宽度一律走**整数四舍五入**（`(used*w + total/2)/total`）。
                //    之前用 f64 乘完直接 `as i32` 截断，最多差 1px；条本身不长，
                //    一眼能看出条和数字对不上。
                // 2) 轨道画一圈 1px 描边。没有描边时，填充比例为 0 的盘看着像
                //    "条没画出来"，很难判断是真的没空间还是渲染没生效。
                let bg = CreateSolidBrush(t.surface_alt);
                FillRect(hdc, &bar, bg);
                let _ = DeleteObject(bg.into());
                if let Some(d) = drive {
                    if let (Some(total), Some(free)) = (d.total, d.available) {
                        if total > 0 {
                            // 条**填的是剩余空间**（不是已用）。旁边的文案写的是
                            // "X 可用，共 Y"，条和数字口径一致才读得懂：以前按
                            // 已用填，数字报"236 GB 可用"、条却只填到 36%，
                            // 看着像"条坏了"。
                            let full = bar.right - bar.left;
                            let w = ((free as u128 * full as u128 + (total as u128) / 2)
                                / total as u128) as i32;
                            if w > 0 && w <= full {
                                let fill = RECT { right: bar.left + w, ..bar };
                                let color = if d.critical() { t.danger } else { t.accent };
                                let fb = CreateSolidBrush(color);
                                FillRect(hdc, &fill, fb);
                                let _ = DeleteObject(fb.into());
                            }
                        }
                    }
                }
                let edge = CreateSolidBrush(t.border);
                for r in [
                    RECT { bottom: bar.top + 1, ..bar },
                    RECT { top: bar.bottom - 1, ..bar },
                    RECT { right: bar.left + 1, ..bar },
                    RECT { left: bar.right - 1, ..bar },
                ] {
                    FillRect(hdc, &r, edge);
                }
                let _ = DeleteObject(edge.into());
            } else {
                let mut name: Vec<u16> = e.name.encode_utf16().collect();
                let mut lr = tile.label;
                DrawTextW(hdc, &mut name, &mut lr, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
            }
        }
        SelectObject(hdc, old_font);
    }
}

/// 详细信息视图的列几何：**绘制与表头共用这一份**（老纪律：两边各写一份常量，
/// 改一次就错位 —— 这条已经踩过三次）。
///
/// 搜索结果（`tab.search_root` 非空）额外多一列"位置"：全卷搜出来的同名文件
/// 只显示文件名根本分不清是哪个，资源管理器也是这么加列的。
pub struct DetailCols {
    pub name: i32,
    pub path: Option<i32>,
    pub date: i32,
    pub kind: i32,
    pub size: i32,
    pub name_w: i32,
    pub path_w: i32,
    pub date_w: i32,
    pub kind_w: i32,
    pub size_w: i32,
}

pub fn detail_cols(app: &App, l: &Layout, icon_size: i32) -> DetailCols {
    let s = app.metrics.scale;
    let text_pad = (21.0 * s) as i32;
    let icon_gap = (3.0 * s) as i32;
    let name = l.view.left + text_pad + icon_size + icon_gap;
    let searching = app.tab().search_root.is_some();
    // 列宽是用户能拖的（存在配置里），这里只是把它按 DPI 放大。
    // 搜索态多一列"位置"，名称列让出空间（照资源管理器搜索结果的做法）。
    let w = app.col_widths();
    let (name_w, path_w) = if searching {
        ((w[0].min(190) as f32 * s) as i32, (270.0 * s) as i32)
    } else {
        ((w[0] as f32 * s) as i32, 0)
    };
    let date = name + name_w + path_w;
    let date_w = (w[1] as f32 * s) as i32;
    let kind = date + date_w;
    let kind_w = (w[2] as f32 * s) as i32;
    let size = kind + kind_w;
    let size_w = (w[3] as f32 * s) as i32;
    DetailCols {
        name,
        path: if searching { Some(name + name_w) } else { None },
        date,
        kind,
        size,
        name_w,
        path_w,
        date_w,
        kind_w,
        size_w,
    }
}

/// 画一个条目的图标：有缩略图就用缩略图，否则退回 HICON（隐藏项再压暗）。
///
/// 大图标视图里必须先试缩略图 —— `SHGFI_LARGEICON` 只给 32×32，拉伸到 64/96
/// 必然发虚；而 IShellItemImageFactory 对图片/视频给的就是真内容。
fn draw_item_icon(
    app: &App,
    hdc: HDC,
    icon: HICON,
    thumb: Option<HBITMAP>,
    x: i32,
    y: i32,
    size: i32,
    alpha: u8,
) {
    if let Some(b) = thumb {
        if app.icons.draw_thumb(hdc, b, x, y, size) {
            return;
        }
    }
    app.icons.draw_faded(hdc, icon, x, y, size, alpha);
}

fn paint_view(app: &mut App, hdc: HDC, l: &Layout) {
    if app.tab().home {
        paint_home(app, hdc, l);
        return;
    }
    // 设置 / 回收站同样是虚拟视图：不走文件列表那条路（它们没有 FileEntry）
    if app.tab().settings {
        paint_settings(app, hdc, l);
        return;
    }
    if app.tab().recycle {
        paint_recycle(app, hdc, l);
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
        let cols = detail_cols(app, l, icon_size);
        let col_name = cols.name;
        let col_date = cols.date;
        let col_type = cols.kind;
        let col_size = cols.size;

        let visible: Vec<usize> = app.tab().visible.clone();
        // 当前正在被拖走的那些条目的 key（小写路径）—— 画的时候要调淡
        let dragged: Vec<String> = app
            .drag
            .as_ref()
            .filter(|d| d.active)
            .map(|d| {
                d.items
                    .iter()
                    .map(|p| p.to_string_lossy().to_lowercase())
                    .collect()
            })
            .unwrap_or_default();
        let header_h = (26.0 * app.metrics.scale) as i32;
        let list_top = l.view.top + if mode == fm_core::ViewMode::Details { header_h } else { 0 };
        let view_h = l.view.bottom - list_top;
        let scroll = app.tab().scroll;

        // 大图标/中图标是**网格**排布（横向排满一行再换行）。之前按"一行一条"画，
        // 于是所有条目竖着排成一列、还一直画到标题栏上面去。
        // 几何统一由 icon_grid() 产出，绘制 / 命中测试 / 滚动高度三处共用。
        let grid = if large_icon {
            Some(icon_grid(l, &app.metrics, mode))
        } else {
            None
        };
        let tile_h = grid.as_ref().map(|g| g.tile_h).unwrap_or(row_h);
        let (from, to) = index_range(&grid, visible.len(), view_h, tile_h, row_h, scroll);
        let n = to.saturating_sub(from);

        // 只为可见项解析图标 / 显示名（这一段独占借用 app），之后取 tab 的只读引用绘制
        let mut icon_handles: Vec<HICON> = Vec::with_capacity(n);
        // 大图标视图下的真实缩略图（与 icon_handles 一一对应；小尺寸时为 None）。
        // 16×16/32×32 的 HICON 硬拉到 64+ 就是一团糊，只有走缩略图才清晰。
        let mut icon_thumbs: Vec<Option<HBITMAP>> = Vec::with_capacity(n);
        // 每一项的图标不透明度：隐藏项调淡、受保护的系统文件调得更淡。
        // 资源管理器就是这么区分的（不换图标，只把图标压暗），照抄能让人一眼认出。
        let mut icon_alpha: Vec<u8> = Vec::with_capacity(n);
        let mut names: Vec<String> = Vec::with_capacity(n);
        let mut size_texts: Vec<String> = Vec::with_capacity(n);
        let show_ext = app.cfg.files.view.show_extensions;
        let unit = app.cfg.files.view.size_unit_of();
        for i in from..to {
            let index = visible[i];
            // 先把这一项拷成自有数据：下面 icons / display_name 都要独占借用 app，
            // 拿着 tab 的只读引用不放会撞 E0502。
            let Some(snap) = app.tab().entries.get(index).map(|e| {
                (
                    e.path.clone(),
                    e.name.clone(),
                    e.size,
                    e.is_dir(),
                    e.is_hidden(),
                    e.is_system(),
                )
            }) else {
                icon_handles.push(HICON(std::ptr::null_mut()));
                icon_thumbs.push(None);
                icon_alpha.push(255);
                names.push(String::new());
                size_texts.push(String::new());
                continue;
            };
            let (path, name, size, is_dir, is_hidden, is_system) = snap;
            let mut alpha = if is_system { 110 } else if is_hidden { 150 } else { 255 };
            // 正在被拖走的那些条目要淡下去。资源管理器就是这么表示的：东西已经
            // "拎在手上"，原地剩下的是一个占位影子，不然根本看不出拖的是哪几个。
            if dragged.contains(&path.to_string_lossy().to_lowercase()) {
                alpha = alpha.min(90);
            }
            // 剪切过的条目同样要虚掉：它已经"不在原地"了，只是还没落下去。
            // 不画这一层的话，剪切和复制在界面上长得一模一样，看不出差别。
            let low = path.to_string_lossy().to_lowercase();
            if app.cut_set.contains(&low) {
                alpha = alpha.min(130);
            }
            icon_alpha.push(alpha);
            let ext = path
                .extension()
                .map(|x| format!(".{}", x.to_string_lossy()))
                .unwrap_or_default();
            let big = icon_size > 24;
            icon_handles.push(if is_dir {
                // 目录优先用 desktop.ini 指定的自定义图标（§12：无自定义才用通用图标）
                app.icons.dir_icon(&path, big)
            } else {
                // exe/dll/lnk/ico… 的图标只属于它自己，必须按**真实路径**问 Shell，
                // 走扩展名缓存拿到的永远是通用图标。
                app.icons.file_icon(&path, &ext, big)
            });
            // 超过 32px 才值得问 Shell 要缩略图：小于等于 32 时 HICON 本身就是
            // 原尺寸，再要一张图纯属浪费（而且每张缩略图要占 size²×4 字节）。
            //
            // **目录也走 thumb**：文件夹的"内容图案"（download 里的 ISO 光盘、
            // games 里的齿轮…）只有 IShellItemImageFactory 的 THUMBNAILONLY 给，
            // 资源管理器显示的就是它 —— 禁掉目录 thumb 会丢图案，颜色观感和
            // Explorer 对不上（用户反馈）。质量由 draw_thumb 的重采样负责。
            icon_thumbs.push(if icon_size > 32 {
                app.icons.thumb(&path, icon_size)
            } else {
                None
            });
            if is_dir {
                // 本地化显示名：Shell 会读 desktop.ini 的 LocalizedResourceName，
                // 于是 Users 显示成"用户"、Desktop 显示成"桌面"，与资源管理器一致。
                names.push(app.display_name(&path, &name));
                size_texts.push(String::new());
            } else {
                names.push(if show_ext {
                    name
                } else {
                    path.file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or(name)
                });
                size_texts.push(crate::app::human_unit(size, unit));
            }
        }

        // 条目一律裁剪在表头以下：否则滚动时行会画到表头上面，跟表头重叠
        let _ = SaveDC(hdc);
        let _ = IntersectClipRect(hdc, l.view.left, list_top, l.view.right, l.view.bottom);

        let tab = app.tab();
        for i in from..to {
            let index = visible[i];
            let Some(entry) = tab.entries.get(index) else { continue };
            let k = i - from;
            let (x, y, w_cell) = match &grid {
                Some(g) => {
                    let col = i as i32 % g.cols;
                    let line = i as i32 / g.cols;
                    (
                        l.view.left + g.pad + col * g.tile_w,
                        list_top + line * g.tile_h - scroll,
                        g.tile_w,
                    )
                }
                // 行式布局的高亮条右沿：**与表头分隔线同口径**。
                //
                // 详细列表：截止到最后一列（大小）右沿 = `detail_cols().size + size_w`
                // —— 表头的竖分隔线也画在这里，行若越过它就会在表头右边空地上
                // 多出一条"一整条"的横杠（用户截图反馈："渲染有变成一整条了"）。
                // `view_row_at` 的空白判定用的正是同一个值，绘制与命中天然一致。
                //
                // 列表视图没有列分隔，整行就该整行可点可画（行式列表的语义）。
                None => {
                    let w = if mode == fm_core::ViewMode::Details {
                        let icon_size = (16.0 * app.metrics.scale) as i32;
                        let cols = detail_cols(app, l, icon_size);
                        ((cols.size + cols.size_w).min(l.view.right) - l.view.left).max(10)
                    } else {
                        l.view.right - l.view.left
                    };
                    (l.view.left, list_top + i as i32 * row_h - scroll, w)
                }
            };
            if y > l.view.bottom || y + tile_h < list_top {
                continue;
            }
            let r = RECT { left: x, top: y, right: x + w_cell, bottom: y + tile_h };
            let selected = tab.selected.contains(&crate::app::key_of(entry));
            // 焦点行只在用户真的交互过之后才画出来：否则每次打开文件夹，
            // 第 0 行都顶着一条高亮，看着像"默认就勾选了第一行"。
            let focused = app.focus_visible && i == tab.focus_row;
            // 悬停：和选中同一套语言（一层底色），但**故意弱一档**。
            // 选中 = 底色 + 行首强调竖线；悬停 = 只有一档更淡的底色，没有竖线。
            // 两者一样重就分不清"我已经选了它"和"我只是指在这儿"。
            let hovered = !selected && !focused && app.hover_row == Some(index);
            if hovered {
                fill_radius(app, hdc, "content.hover", &r, t.row_hover, None);
            }
            if selected || focused {
                fill_radius(
                    app,
                    hdc,
                    "content.selected",
                    &r,
                    if selected { t.selection_bg } else { t.hover_bg },
                    None,
                );
                // 行首一条竖线：和自绘右键菜单的悬停项**同一套语言**
                // （那里是"底色 + 竖线"）。只有一层底色时，暗色主题下
                // "选中了哪几行"要逐个比对亮度才看得出来；加了竖线，扫一眼
                // 左边缘就知道。形状（宽/高/粗细/空心）由「设置-交互」页调。
                draw_sel_bar(app, hdc, r, t);
            }
            SetTextColor(hdc, if selected { t.selection_text } else { t.text });

            let icon = icon_handles.get(k).copied().unwrap_or(HICON(std::ptr::null_mut()));
            let thumb = icon_thumbs.get(k).copied().flatten();
            let alpha = icon_alpha.get(k).copied().unwrap_or(255);
            let name = names.get(k).cloned().unwrap_or_default();
            if mode == fm_core::ViewMode::Details {
                draw_item_icon(app, hdc, icon, thumb, col_name - icon_size - icon_gap, y + (row_h - icon_size) / 2, icon_size, alpha);
                let name_r = RECT {
                    left: col_name,
                    top: y,
                    right: cols.path.unwrap_or(col_date),
                    bottom: y + row_h,
                };
                // 必须带 DT_END_ELLIPSIS：名字长了不省略就会一路画到下一列上
                // （"条目长度"不对的真因 —— 长文件名的字压在"修改日期"上面）。
                draw_text(
                    hdc,
                    &name,
                    name_r,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
                );
                SetTextColor(hdc, if selected { t.selection_text } else { t.text_dim });
                if let Some(px0) = cols.path {
                    // 搜索结果里的"位置"：显示所在文件夹，超长省略中间
                    let folder = entry
                        .path
                        .parent()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default();
                    draw_text(
                        hdc,
                        &folder,
                        RECT { left: px0, top: y, right: col_date, bottom: y + row_h },
                        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
                    );
                }
                // 后三列同样要夹在各自列宽里：少了 NOPREFIX 会把文件名里的 `&`
                // 画出来，少了 END_ELLIPSIS 就会串列。
                let cell = DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS;
                draw_text(hdc, &format_date(entry.modified), RECT { left: col_date, top: y, right: col_type, bottom: y + row_h }, cell);
                draw_text(hdc, &entry.type_label(), RECT { left: col_type, top: y, right: col_size, bottom: y + row_h }, cell);
                // 最后一列也要夹在自己的列宽里，不能一路画到窗口右边缘 ——
                // 那样"大小"那列看着比表头长一截，和表头对不上。
                draw_text(hdc, size_texts.get(k).unwrap_or(&String::new()), RECT { left: col_size, top: y, right: col_size + cols.size_w, bottom: y + row_h }, cell);
            } else if mode == fm_core::ViewMode::List {
                let ix = l.view.left + text_pad;
                draw_item_icon(app, hdc, icon, thumb, ix, y + (row_h - icon_size) / 2, icon_size, alpha);
                let name_r = RECT { left: ix + icon_size + 8, top: y, right: l.view.right, bottom: y + row_h };
                draw_text(hdc, &name, name_r, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
            } else {
                // 网格磁贴：图标在格子顶部居中，名字压在图标下方、居中并省略过长部分
                let cx = x + (w_cell - icon_size) / 2;
                draw_item_icon(app, hdc, icon, thumb, cx, y + (8.0 * app.metrics.scale) as i32, icon_size, alpha);
                let name_r = RECT {
                    left: x + 4,
                    top: y + icon_size + (4.0 * app.metrics.scale) as i32,
                    right: x + w_cell - 4,
                    bottom: y + tile_h,
                };
                draw_text(hdc, &name, name_r, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
            }
        }
        let _ = RestoreDC(hdc, -1);

        // 表头**最后**画，且底色不透明：滚动时条目绝不会压到它上面
        if mode == fm_core::ViewMode::Details {
            let hb = CreateSolidBrush(t.window_bg);
            let hr = RECT { left: l.view.left, top: l.view.top, right: l.view.right, bottom: l.view.top + header_h };
            let _ = FillRect(hdc, &hr, hb);
            let _ = DeleteObject(hb.into());
            let sort = app.tab().sort;
            let hf = header_font(app.metrics.scale);
            let old = SelectObject(hdc, hf.into());
            let mut cols_hdr = [
                ("名称", fm_core::SortKey::Name, col_name, cols.name_w),
                ("修改日期", fm_core::SortKey::Modified, col_date, cols.date_w),
                ("类型", fm_core::SortKey::Type, col_type, cols.kind_w),
                ("大小", fm_core::SortKey::Size, col_size, cols.size_w),
            ];
            let mut path_hdr = ("位置", fm_core::SortKey::Name, 0, 0);
            let mut all: Vec<(&str, fm_core::SortKey, i32, i32)> = Vec::new();
            all.push(cols_hdr[0]);
            if let Some(px0) = cols.path {
                path_hdr.2 = px0;
                path_hdr.3 = cols.path_w;
                all.push(path_hdr);
            }
            all.extend_from_slice(&cols_hdr[1..]);
            for (ci, (label, key, x0, w)) in all.iter().enumerate() {
                let label: &str = *label;
                let _ = &mut cols_hdr;
                // 列名一律**夹在自己那一列的宽度里**：不夹的话列宽拖窄之后，
                // 列名会一路画到右边那一列的头上（"列拖不动/看着乱"的观感来源）。
                let right = (x0 + w).min(l.view.right);
                let rr = RECT { left: *x0, top: l.view.top, right: right.max(*x0), bottom: l.view.top + header_h };
                let active = sort.key == *key;
                SetTextColor(hdc, if active { t.text } else { t.text_dim });
                // 列名在**自己这一列里居中**（资源管理器 Win11 表头就是这样）：
                // 贴着左沿画的时候，它紧贴上一列的分隔线，看着像那一列的尾巴。
                let tw = unsafe { text_width(hdc, hf, label) };
                let arrow_w = if active { (14.0 * app.metrics.scale) as i32 } else { 0 };
                let block_w = tw + arrow_w;
                let bx = *x0 + ((rr.right - rr.left) - block_w) / 2;
                let tr = RECT { left: bx, top: rr.top, right: bx + tw, bottom: rr.bottom };
                draw_text(
                    hdc,
                    label,
                    tr,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
                );
                // 排序指示：上/下**一对**实心三角（不是侧栏那套"右/下"箭头 ——
                // 那会让升序画成一个横着的箭头）。
                if active {
                    let ar = RECT {
                        left: bx + tw,
                        top: rr.top,
                        right: bx + tw + arrow_w,
                        bottom: rr.bottom,
                    };
                    if ar.left < rr.right {
                        draw_sort_arrow(hdc, &ar, t.text, sort.descending);
                    }
                }
                // 悬停在这一列上时，右端露出一个"筛选"小箭头（资源管理器同款）：
                // 点它下拉出这一列的筛选项。没有它用户不知道表头还能筛。
                if app.hover_header == Some(ci) {
                    let aw = (16.0 * app.metrics.scale) as i32;
                    let fr = RECT {
                        left: (rr.right - aw).max(rr.left),
                        top: rr.top,
                        right: rr.right,
                        bottom: rr.bottom,
                    };
                    draw_tree_chevron(hdc, &fr, t.text, true);
                }
            }
            // 列分隔竖线：告诉用户"这条可以拖"。没有它，表头就是一行光秃秃的
            // 文字，看不出每一列到哪儿结束、哪儿能拖 —— 也就是"内部各个属性
            // 要做成可拉伸的样式"缺的那一半。位置复用 header_splits（命中判定
            // 用的就是它），绘制与命中同一份几何，拖起来才不会差几像素。
            let sep = CreateSolidBrush(t.border);
            for (_, sx) in header_splits(app, l) {
                if sx <= l.view.left || sx >= l.view.right {
                    continue;
                }
                FillRect(
                    hdc,
                    &RECT { left: sx, top: l.view.top + (5.0 * app.metrics.scale) as i32, right: sx + 1, bottom: l.view.top + header_h - (5.0 * app.metrics.scale) as i32 },
                    sep,
                );
            }
            let _ = DeleteObject(sep.into());
            SelectObject(hdc, old);
        }

        // 加载中不给任何文字提示：进度条在目录栏下沿（paint_busy_strip）。
        // 而且这时候条目表是空的，若照旧判空就会闪一句"此文件夹为空"。
        if !app.tab().loading && visible.is_empty() {
            // 居中写在**内容区正中**：之前贴在左上角一行里，看着像某一列的
            // 内容而不是"这个文件夹没东西"。高度给一整屏，垂直也居中。
            SetTextColor(hdc, t.text_dim);
            let r = RECT {
                left: l.view.left,
                top: list_top,
                right: l.view.right,
                bottom: l.view.bottom.max(list_top + 40),
            };
            draw_text(hdc, "此文件夹为空", r, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        }

        // 滚动条：没有它，文件多时看不出还有多少内容可用。
        // 网格视图的高度必须按"行数 × 行高"算 —— 直接"条目数 × 行高"会把内容
        // 高度放大成一列的长度，滑块永远缩在顶部。
        let content_h = match &grid {
            Some(g) => grid_content_h(g, visible.len()),
            None => visible.len() as i32 * row_h,
        };
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

// ---------------------------------------------------------------- 设置页

/// 设置项的动作编号。`main.rs` 按它把点击翻译成 `Command`。
pub const SET_HIDDEN: u32 = 1;
pub const SET_SYSTEM: u32 = 2;
pub const SET_EXT: u32 = 3;
pub const SET_UNIT: u32 = 4;
pub const SET_THEME: u32 = 5;
pub const SET_VIEW: u32 = 6;
pub const SET_SIDEBAR: u32 = 7;
pub const SET_STATUS: u32 = 8;
/// 导航窗格"跟随展开到当前文件夹"。默认关闭（默认配置里就是 false）。
pub const SET_FOLLOW: u32 = 9;
/// "此电脑"节点默认展开（展开后才看得见盘符）
pub const SET_THISPC: u32 = 10;

/// 设置页的一行。绘制与命中测试**共用**这份几何（同一条纪律：两边各写一份常量，
/// 改一次就错位）。`action == 0` 是分组标题，不可点。
pub struct SettingRow {
    pub rect: RECT,
    pub label: String,
    pub value: String,
    pub action: u32,
    pub on: bool,
    /// `Some(开/关)` = 这一项是开关，画成真正的 Toggle 控件；
    /// `None` = 多选项（主题/单位/默认视图），画成带当前值的下拉按钮。
    ///
    /// 之前一律用"开"/"关"两个**文字**表示状态，一行行读下来全是字，
    /// 哪个是开关哪个是当前值要逐行分辨；开关就该长成开关。
    pub toggle: Option<bool>,
}

/// 开关控件的矩形。绘制与命中共用（同一条纪律）。
pub fn setting_switch_rect(row: &SettingRow, s: f32) -> RECT {
    let w = (38.0 * s).round() as i32;
    let h = (18.0 * s).round() as i32;
    let cy = (row.rect.top + row.rect.bottom) / 2;
    RECT {
        left: row.rect.right - w,
        top: cy - h / 2,
        right: row.rect.right,
        bottom: cy - h / 2 + h,
    }
}

/// 多选项那个"当前值 ▾"按钮的矩形。
pub fn setting_value_rect(row: &SettingRow, s: f32) -> RECT {
    let w = (120.0 * s).round() as i32;
    let h = (22.0 * s).round() as i32;
    let cy = (row.rect.top + row.rect.bottom) / 2;
    RECT {
        left: row.rect.right - w,
        top: cy - h / 2,
        right: row.rect.right,
        bottom: cy - h / 2 + h,
    }
}

pub fn settings_rows(app: &App, l: &Layout) -> Vec<SettingRow> {
    let s = app.metrics.scale;
    let row_h = (34.0 * s).round() as i32;
    let head_h = (30.0 * s).round() as i32;
    let left = l.view.left + (24.0 * s).round() as i32;
    let right = l.view.right - (24.0 * s).round() as i32;
    let unit = app.cfg.files.view.size_unit_of();
    let dark = app.cfg.core.theme != "light";

    let groups: Vec<(&str, Vec<(String, String, u32, Option<bool>)>)> = vec![
        (
            "文件与文件夹",
            vec![
                (
                    "显示隐藏的项目".to_string(),
                    String::new(),
                    SET_HIDDEN,
                    Some(app.show_hidden),
                ),
                (
                    "显示受保护的系统文件".to_string(),
                    String::new(),
                    SET_SYSTEM,
                    Some(app.show_system),
                ),
                (
                    "显示文件扩展名".to_string(),
                    String::new(),
                    SET_EXT,
                    Some(app.cfg.files.view.show_extensions),
                ),
                ("大小单位".to_string(), unit.label().to_string(), SET_UNIT, None),
                (
                    "新标签默认视图".to_string(),
                    crate::app::default_view_label(&app.cfg.files.view.default_mode).to_string(),
                    SET_VIEW,
                    None,
                ),
            ],
        ),
        (
            "外观",
            vec![
                (
                    "主题".to_string(),
                    if dark { "暗色".to_string() } else { "亮色".to_string() },
                    SET_THEME,
                    None,
                ),
                (
                    "显示导航窗格".to_string(),
                    String::new(),
                    SET_SIDEBAR,
                    Some(app.cfg.ui.sidebar_visible),
                ),
                (
                    "导航窗格跟随当前文件夹展开".to_string(),
                    String::new(),
                    SET_FOLLOW,
                    Some(app.cfg.ui.sidebar_follow_current),
                ),
                (
                    "「此电脑」默认展开到盘符".to_string(),
                    String::new(),
                    SET_THISPC,
                    Some(app.cfg.ui.sidebar_thispc_expanded),
                ),
                (
                    "显示状态栏".to_string(),
                    String::new(),
                    SET_STATUS,
                    Some(app.cfg.ui.statusbar_visible),
                ),
            ],
        ),
    ];

    let mut out = Vec::new();
    let mut top = l.view.top + (16.0 * s).round() as i32;
    let title_h = (30.0 * s).round() as i32;
    out.push(SettingRow {
        rect: RECT { left, top, right, bottom: top + title_h },
        label: "设置".to_string(),
        value: String::new(),
        action: 0,
        on: false,
        toggle: None,
    });
    top += title_h + (8.0 * s).round() as i32;
    for (group, rows) in groups {
        out.push(SettingRow {
            rect: RECT { left, top, right, bottom: top + head_h },
            label: group.to_string(),
            value: String::new(),
            action: 0,
            on: false,
            toggle: None,
        });
        top += head_h;
        for (label, value, action, toggle) in rows {
            out.push(SettingRow {
                rect: RECT { left, top, right, bottom: top + row_h },
                label,
                value,
                action,
                on: toggle.unwrap_or(false),
                toggle,
            });
            top += row_h;
        }
        top += (10.0 * s).round() as i32;
    }
    // 提示：所有开关实时写入用户层配置
    out.push(SettingRow {
        rect: RECT { left, top, right, bottom: top + head_h },
        label: "改动即时生效，并写入 %APPDATA%\\ZenFile\\settings.user.json".to_string(),
        value: String::new(),
        action: 0,
        on: false,
        toggle: None,
    });
    out
}

/// 设置页命中测试：返回被点中的动作编号。
pub fn settings_hit(app: &App, l: &Layout, x: i32, y: i32) -> Option<u32> {
    settings_rows(app, l)
        .iter()
        .find(|r| r.action != 0 && x >= r.rect.left && x < r.rect.right && y >= r.rect.top && y < r.rect.bottom)
        .map(|r| r.action)
}

/// Windows 11 风格的开关：胶囊轨道 + 圆滑块。开 = 强调色轨道、滑块靠右。
pub(crate) fn draw_toggle(hdc: HDC, row: &SettingRow, app: &App, on: bool) {
    let t = app.theme;
    let s = app.metrics.scale;
    let r = setting_switch_rect(row, s);
    let d = r.bottom - r.top;
    unsafe {
        // 轨道：RoundRect 用当前刷子填充 + 当前画笔描边
        let track = CreateSolidBrush(if on { t.accent } else { t.border });
        let old = SelectObject(hdc, track.into());
        let _ = RoundRect(hdc, r.left, r.top, r.right, r.bottom, d / 2, d / 2);
        SelectObject(hdc, old);
        let _ = DeleteObject(track.into());
        // 滑块：直径比轨道高度小 4px，左右各留 2px
        let kd = d - (4.0 * s).round() as i32;
        let pad = (2.0 * s).round() as i32;
        let kleft = if on { r.right - pad - kd } else { r.left + pad };
        let knob = CreateSolidBrush(if on { t.selection_text } else { t.surface });
        SelectObject(hdc, knob.into());
        let _ = Ellipse(hdc, kleft, r.top + pad, kleft + kd, r.top + pad + kd);
        let _ = DeleteObject(knob.into());
        SelectObject(hdc, old);
    }
}

/// 多选项（主题 / 大小单位 / 默认视图）：画成一个"当前值 ▾"按钮，
/// 一眼看得出这一行是可以点开换值的。
pub(crate) fn draw_value_button(hdc: HDC, row: &SettingRow, app: &App) {
    let t = app.theme;
    let s = app.metrics.scale;
    let r = setting_value_rect(row, s);
    unsafe {
        let bg = CreateSolidBrush(t.surface);
        let old = SelectObject(hdc, bg.into());
        let pen = CreatePen(PS_SOLID, 1, t.border);
        let old_pen = SelectObject(hdc, pen.into());
        let _ = RoundRect(hdc, r.left, r.top, r.right, r.bottom, (4.0 * s) as i32, (4.0 * s) as i32);
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(pen.into());
        SelectObject(hdc, old);
        let _ = DeleteObject(bg.into());

        let font = ui_font(s);
        let of = SelectObject(hdc, font.into());
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, t.text);
        let tr = RECT { left: r.left + (8.0 * s) as i32, top: r.top, right: r.right - (16.0 * s) as i32, bottom: r.bottom };
        draw_text(hdc, &row.value, tr, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
        let g = glyph_font(s);
        let og = SelectObject(hdc, g.into());
        SetTextColor(hdc, t.text_dim);
        let ar = RECT { left: r.right - (16.0 * s) as i32, top: r.top, right: r.right - (2.0 * s) as i32, bottom: r.bottom };
        draw_text(hdc, "\u{E70D}", ar, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        SelectObject(hdc, og);
        SelectObject(hdc, of);
    }
}

/// 设置页（第 19 轮第 1 项整体重做）。
///
/// 底色铺完就交给 `crate::settings`：顶部五个小标签、可拖拽的排布编辑器、
/// 「更多」收纳区、高级开关、插件清单、关于页，全在那边。
fn paint_settings(app: &mut App, hdc: HDC, l: &Layout) {
    let t = app.theme;
    unsafe {
        let bg = CreateSolidBrush(t.window_bg);
        let r = RECT { left: l.view.left, top: l.view.top, right: l.view.right, bottom: l.view.bottom };
        FillRect(hdc, &r, bg);
        let _ = DeleteObject(bg.into());
    }
    crate::settings::paint(app, hdc, l);
}

/// 老的设置页（分组开关清单）。「高级」标签现在的行定义还借用它的分组表，
/// 因此保留为公开函数；新设置页的绘制走 `crate::settings::paint`。
#[allow(dead_code)]
pub fn paint_settings_legacy(app: &mut App, hdc: HDC, l: &Layout) {
    let t = app.theme;
    let s = app.metrics.scale;
    let rows = settings_rows(app, l);
    unsafe {
        let bg = CreateSolidBrush(t.window_bg);
        let r = RECT { left: l.view.left, top: l.view.top, right: l.view.right, bottom: l.view.bottom };
        FillRect(hdc, &r, bg);
        let _ = DeleteObject(bg.into());
        let font = ui_font(s);
        let old = SelectObject(hdc, font.into());
        SetBkMode(hdc, TRANSPARENT);

        let mut hover_rect: Option<RECT> = None;
        for (i, row) in rows.iter().enumerate() {
            if i == 0 {
                let bf = crate::paint::make_font((-16.0 * s) as i32, 700, "Segoe UI");
                let ob = SelectObject(hdc, bf.into());
                SetTextColor(hdc, t.text);
                draw_text(hdc, &row.label, row.rect, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
                SelectObject(hdc, ob);
                let _ = DeleteObject(bf.into());
                continue;
            }
            if row.action == 0 {
                SetTextColor(hdc, t.text_dim);
                draw_text(hdc, &row.label, row.rect, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
                continue;
            }
            if app.settings_hover == Some(row.action) {
                hover_rect = Some(row.rect);
            }
            SetTextColor(hdc, t.text);
            let label_rect = RECT { left: row.rect.left, top: row.rect.top, right: row.rect.right - (150.0 * s) as i32, bottom: row.rect.bottom };
            draw_text(hdc, &row.label, label_rect, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            // 开关画成开关：轨道 + 滑块，开=强调色、关=描边灰。
            // 之前这里是两个汉字"开"/"关"，一页设置读下来全是文字，
            // 分不清哪行是开关、哪行是"当前值"。
            match row.toggle {
                Some(on) => draw_toggle(hdc, row, app, on),
                None => draw_value_button(hdc, row, app),
            }
            // 行分隔线
            let lb = CreateSolidBrush(t.border);
            FillRect(hdc, &RECT { left: row.rect.left, top: row.rect.bottom - 1, right: row.rect.right, bottom: row.rect.bottom }, lb);
            let _ = DeleteObject(lb.into());
        }
        // 悬停底色最后铺一层描边式高亮，避免盖住文字
        if let Some(r) = hover_rect {
            let hb = CreateSolidBrush(t.hover_bg);
            let hr = RECT { left: r.left - (8.0 * s) as i32, top: r.top, right: r.right + (8.0 * s) as i32, bottom: r.bottom - 1 };
            // 先画底再画字做不到（字已画完），这里只画左右两条强调边做反馈
            let ab = CreateSolidBrush(t.accent);
            FillRect(hdc, &RECT { left: hr.left, top: hr.top, right: hr.left + (3.0 * s) as i32, bottom: hr.bottom }, ab);
            let _ = DeleteObject(ab.into());
            let _ = DeleteObject(hb.into());
        }
        SelectObject(hdc, old);
    }
}

// ---------------------------------------------------------------- 回收站视图

/// 回收站条目的行高。绘制与命中共用。
pub fn recycle_row_h(scale: f32) -> i32 {
    (44.0 * scale).round() as i32
}

/// 回收站顶部工具条的高度（三个按钮那一行）。
fn recycle_toolbar_h(scale: f32) -> i32 {
    (44.0 * scale).round() as i32
}

/// 回收站列头行的高度。
pub fn recycle_header_h(scale: f32) -> i32 {
    (26.0 * scale).round() as i32
}

/// 列表区（第一条数据行）的顶边 y。命中测试与绘制共用。
pub fn recycle_list_top(scale: f32, view_top: i32) -> i32 {
    view_top + recycle_toolbar_h(scale) + recycle_header_h(scale)
}

/// 回收站四列的横向范围：`[名称, 原位置, 删除时间, 大小]`。
///
/// 表头文字和数据行**共用**这一份几何，改一处两边都跟着动 —— 之前两边各写
/// 一套常量，改完列头对不齐数据，看着像"表头是装饰品"。
pub fn recycle_columns(app: &App, l: &Layout) -> [RECT; 4] {
    let s = app.metrics.scale;
    let pad = (12.0 * s) as i32;
    let w_time = (150.0 * s) as i32;
    let w_size = (90.0 * s) as i32;
    let w_orig = (230.0 * s) as i32;
    // 名称列要给图标让位：左内边距 + 图标 + 图标后的间隙
    let icon_block = pad + (24.0 * s).round() as i32 + (12.0 * s) as i32;
    let x0 = l.view.left + pad;
    let x1 = (l.view.right - pad - w_time - w_size - w_orig).max(x0 + (120.0 * s) as i32);
    [
        RECT { left: x0 + icon_block, top: 0, right: x1, bottom: 0 },
        RECT { left: x1, top: 0, right: x1 + w_orig, bottom: 0 },
        RECT { left: x1 + w_orig, top: 0, right: x1 + w_orig + w_time, bottom: 0 },
        RECT { left: x1 + w_orig + w_time, top: 0, right: l.view.right - pad, bottom: 0 },
    ]
}

/// 光标落在回收站表头的哪一列上（0=名称 1=原位置 2=删除时间 3=大小）。
pub fn recycle_header_hit(app: &App, l: &Layout, x: i32, y: i32) -> Option<usize> {
    let s = app.metrics.scale;
    let top = l.view.top + recycle_toolbar_h(s);
    if y < top || y >= top + recycle_header_h(s) {
        return None;
    }
    let cols = recycle_columns(app, l);
    cols.iter().position(|c| x >= c.left && x < c.right)
}

/// 回收站视图命中测试：返回条目下标。
pub fn recycle_hit(app: &App, l: &Layout, y: i32) -> Option<usize> {
    let row_h = recycle_row_h(app.metrics.scale);
    let top = recycle_list_top(app.metrics.scale, l.view.top);
    let local = y - top;
    if local < 0 {
        return None;
    }
    let idx = (local / row_h.max(1)) as usize;
    if idx < app.recycle_items.len() {
        Some(idx)
    } else {
        None
    }
}

fn paint_recycle(app: &mut App, hdc: HDC, l: &Layout) {
    let t = app.theme;
    let s = app.metrics.scale;
    let row_h = recycle_row_h(s);
    let bar_h = recycle_toolbar_h(s);
    let head_h = recycle_header_h(s);
    let list_top = recycle_list_top(s, l.view.top);
    let unit = app.cfg.files.view.size_unit_of();
    let cols = recycle_columns(app, l);

    // 图标按"还原后的原路径"的扩展名取，看起来和删除前一模一样
    let mut icons: Vec<HICON> = Vec::with_capacity(app.recycle_items.len());
    for it in &app.recycle_items {
        let ext = it
            .original
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        icons.push(if it.is_dir {
            app.icons.dir_icon(&it.original, false)
        } else {
            app.icons.file_icon(&it.original, &ext, false)
        });
    }

    unsafe {
        let bg = CreateSolidBrush(t.window_bg);
        FillRect(hdc, &RECT { left: l.view.left, top: l.view.top, right: l.view.right, bottom: l.view.bottom }, bg);
        let _ = DeleteObject(bg.into());
        let font = ui_font(s);
        let old = SelectObject(hdc, font.into());
        SetBkMode(hdc, TRANSPARENT);

        // —— 顶部工具条：三个按钮右对齐到最右侧，左边留"共 N 项" ——
        // 之前按钮贴左排，看着像"内容的第一行"而不是工具条。
        let btns = recycle_buttons(app, l);
        for (i, (label, enabled)) in [
            ("还原", !app.recycle_sel.is_empty()),
            ("彻底删除", !app.recycle_sel.is_empty()),
            ("清空回收站", !app.recycle_items.is_empty()),
        ]
        .iter()
        .enumerate()
        {
            let r = btns[i];
            let hovered = app.hover_recycle_btn == (i + 1) as u8 && *enabled;
            fill_radius(
                app,
                hdc,
                "toolbar.button",
                &r,
                if hovered { t.row_hover } else { t.surface_alt },
                None,
            );
            SetTextColor(hdc, if *enabled { t.text } else { t.text_dim });
            draw_text(hdc, label, r, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        }
        SetTextColor(hdc, t.text_dim);
        draw_text(
            hdc,
            &format!("共 {} 项", app.recycle_items.len()),
            RECT { left: l.view.left + (12.0 * s) as i32, top: l.view.top, right: btns[0].left - (12.0 * s) as i32, bottom: l.view.top + bar_h },
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );

        // —— 列头：名称 / 原位置 / 删除时间 / 大小 ——
        // 回收站之前压根没有列头，四列数字飘在行尾，看不出哪一列是什么。
        let hr = RECT { left: l.view.left, top: l.view.top + bar_h, right: l.view.right, bottom: l.view.top + bar_h + head_h };
        let hb = CreateSolidBrush(t.surface_alt);
        FillRect(hdc, &hr, hb);
        let _ = DeleteObject(hb.into());
        // 列头可以点：点一下按这一列排，再点一次换方向（和资源管理器一样）。
        // 之前回收站的表头只是一行字，"快速排序"根本没接上。
        let (scol, sdesc) = app.recycle_sort;
        let hfont = header_font(s);
        let old_hf = SelectObject(hdc, hfont.into());
        for (i, label) in ["名称", "原位置", "删除时间", "大小"].iter().enumerate() {
            let c = cols[i];
            let rr = RECT { left: c.left, top: hr.top, right: c.right, bottom: hr.bottom };
            SetTextColor(hdc, if scol == i { t.text } else { t.text_dim });
            let tw = text_width(hdc, hfont, label);
            let aw = if scol == i { (14.0 * s) as i32 } else { 0 };
            let bx = c.left + ((c.right - c.left) - (tw + aw)) / 2;
            draw_text(
                hdc,
                label,
                RECT { left: bx.max(c.left), top: hr.top, right: (bx + tw).min(c.right), bottom: hr.bottom },
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
            if scol == i {
                draw_sort_arrow(
                    hdc,
                    &RECT { left: bx + tw, top: hr.top, right: bx + tw + aw, bottom: hr.bottom },
                    t.text,
                    sdesc,
                );
            }
        }
        SelectObject(hdc, old_hf);
        SetTextColor(hdc, t.text_dim);
        let sep = CreateSolidBrush(t.border);
        FillRect(hdc, &RECT { left: hr.left, top: hr.bottom - 1, right: hr.right, bottom: hr.bottom }, sep);
        let _ = DeleteObject(sep.into());

        let _ = SaveDC(hdc);
        let _ = IntersectClipRect(hdc, l.view.left, list_top, l.view.right, l.view.bottom);
        for (i, it) in app.recycle_items.iter().enumerate() {
            let y = list_top + i as i32 * row_h;
            if y > l.view.bottom || y + row_h < list_top {
                continue;
            }
            let r = RECT { left: l.view.left, top: y, right: l.view.right, bottom: y + row_h };
            let sel = app.recycle_sel.contains(&it.id);
            // 悬停反馈用比选中弱一档的 row_hover，和文件列表一个手感
            let hovered = app.hover_recycle == Some(i);
            if sel {
                fill_radius(app, hdc, "content.selected", &r, t.selection_bg, None);
            } else if hovered {
                fill_radius(app, hdc, "content.hover", &r, t.row_hover, None);
            }
            let icon = icons.get(i).copied().unwrap_or(HICON(std::ptr::null_mut()));
            let ix = l.view.left + (12.0 * s) as i32;
            let isz = (24.0 * s).round() as i32;
            app.icons.draw(hdc, icon, ix, y + (row_h - isz) / 2, isz);

            let fg = if sel { t.selection_text } else { t.text };
            SetTextColor(hdc, fg);
            draw_text(
                hdc,
                &it.name,
                RECT { left: cols[0].left, top: y, right: cols[0].right, bottom: y + row_h },
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
            );

            SetTextColor(hdc, if sel { t.selection_text } else { t.text_dim });
            draw_text(
                hdc,
                &it.original.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
                RECT { left: cols[1].left, top: y, right: cols[1].right, bottom: y + row_h },
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
            draw_text(
                hdc,
                &format_date(it.deleted),
                RECT { left: cols[2].left, top: y, right: cols[2].right, bottom: y + row_h },
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
            draw_text(
                hdc,
                &crate::app::human_unit(it.size, unit),
                RECT { left: cols[3].left, top: y, right: cols[3].right, bottom: y + row_h },
                DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
        }
        let _ = RestoreDC(hdc, -1);

        if app.recycle_items.is_empty() && !app.tab().loading {
            SetTextColor(hdc, t.text_dim);
            draw_text(
                hdc,
                "回收站是空的",
                RECT { left: l.view.left, top: list_top, right: l.view.right, bottom: l.view.bottom.max(list_top + 40) },
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
        }
        SelectObject(hdc, old);
    }
}

/// 回收站顶部三个按钮的矩形（绘制与命中共用）。**右对齐**到视图最右侧。
pub fn recycle_buttons(app: &App, l: &Layout) -> [RECT; 3] {
    let s = app.metrics.scale;
    let btn_w = (110.0 * s).round() as i32;
    let btn_h = (26.0 * s).round() as i32;
    let gap = (8.0 * s).round() as i32;
    let pad = (12.0 * s).round() as i32;
    let total = 3 * btn_w + 2 * gap;
    let bx0 = l.view.right - pad - total;
    let by = l.view.top + (recycle_toolbar_h(s) - btn_h) / 2;
    let mut out = [RECT::default(); 3];
    for i in 0..3 {
        let bx = bx0 + i as i32 * (btn_w + gap);
        out[i] = RECT { left: bx, top: by, right: bx + btn_w, bottom: by + btn_h };
    }
    out
}

/// 鼠标落在回收站顶部工具条的哪个按钮上：0=无，1..3 对应三个按钮。
pub fn recycle_button_hit(app: &App, l: &Layout, x: i32, y: i32) -> u8 {
    for (i, r) in recycle_buttons(app, l).iter().enumerate() {
        if x >= r.left && x < r.right && y >= r.top && y < r.bottom {
            return (i + 1) as u8;
        }
    }
    0
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

pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
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

/// 状态栏右下角"快速布局切换"的两个按钮 —— 资源管理器同款。
///
/// 返回 (大图标, 详细信息)。**绘制和命中测试必须共用这一个函数**：两边各写一份
/// 常量，改一次尺寸就错位（这条纪律是踩过坑才立的）。
/// 状态栏右下角两个布局切换按钮的几何：`(大图标, 详细信息)`。
///
/// **顺序**刻意做成"详细信息在左、大图标在最右"：资源管理器右下角那对按钮
/// 就是这个次序（左边是列表、右边是磁贴），两条挨着的最右边那个才和窗口右下
/// 角对齐。之前做反了，点"最右边"出来的是详细信息，和资源管理器对不上。
pub fn status_view_rects(l: &Layout, m: &Metrics) -> (RECT, RECT) {
    let btn_w = (30.0 * m.scale).round() as i32;
    let btn_h = (20.0 * m.scale).round() as i32;
    let gap = (2.0 * m.scale).round() as i32;
    let cy = (l.status.top + l.status.bottom) / 2;
    let top = cy - btn_h / 2;
    let right = l.status.right - (6.0 * m.scale).round() as i32;
    // 从最右端往左排：大图标贴着右下角，详细信息在它左边一个按钮位
    let icons = RECT { left: right - btn_w, top, right, bottom: top + btn_h };
    let details = RECT { left: right - btn_w * 2 - gap, top, right: right - btn_w - gap, bottom: top + btn_h };
    (icons, details)
}

/// 两个按钮一起算出来的最左边界：状态栏左侧文字要避开整块按钮区。
pub fn status_view_left(l: &Layout, m: &Metrics) -> i32 {
    let (icons, details) = status_view_rects(l, m);
    icons.left.min(details.left)
}

/// 命中状态栏的布局切换按钮，返回被点到的按钮对应的视图模式。
pub fn status_view_hit(l: &Layout, m: &Metrics, x: i32, y: i32) -> Option<fm_core::ViewMode> {
    let (icons, details) = status_view_rects(l, m);
    if x >= icons.left && x <= icons.right && y >= icons.top && y <= icons.bottom {
        return Some(fm_core::ViewMode::LargeIcons);
    }
    if x >= details.left && x <= details.right && y >= details.top && y <= details.bottom {
        return Some(fm_core::ViewMode::Details);
    }
    None
}

/// 两个按钮的图标是**手绘**的：不用 Segoe MDL2 的字形码（不同系统版本的码位
/// 不一致，容易画出一个空白方框）。2×2 方块 = 大图标；三条横线 = 详细信息。
/// 状态栏右下角那两个布局按钮的图标。
///
/// 之前"大图标"是四个实心方块、"详细信息"是三条横线 —— 三个像素条读不出
/// "这是什么意思"，用户只能靠猜。重做成一看就懂的两个：
///   * 大图标 = 2×2 的**方格子**（外框 + 内部留白，像缩略图格子）
///   * 详细信息 = 一个**表格**：顶部一条表头线 + 若干行，行里还有列分隔
unsafe fn draw_view_glyph(hdc: HDC, r: &RECT, grid: bool, color: COLORREF) {
    let brush = CreateSolidBrush(color);
    let w = r.right - r.left;
    let h = r.bottom - r.top;
    if grid {
        // 2×2 格子：每格画成空心方框（描边），比实心块更像"图标视图"
        let cell = ((h.min(w) - 10) / 2).max(3);
        let gap = 2;
        let x0 = r.left + (w - (cell * 2 + gap)) / 2;
        let y0 = r.top + (h - (cell * 2 + gap)) / 2;
        for gy in 0..2 {
            for gx in 0..2 {
                let l = x0 + gx * (cell + gap);
                let t = y0 + gy * (cell + gap);
                // 方框：四条 1px 边（上/下/左/右）
                let _ = FillRect(hdc, &RECT { left: l, top: t, right: l + cell, bottom: t + 1 }, brush);
                let _ = FillRect(hdc, &RECT { left: l, top: t + cell - 1, right: l + cell, bottom: t + cell }, brush);
                let _ = FillRect(hdc, &RECT { left: l, top: t, right: l + 1, bottom: t + cell }, brush);
                let _ = FillRect(hdc, &RECT { left: l + cell - 1, top: t, right: l + cell, bottom: t + cell }, brush);
            }
        }
    } else {
        // 表格：一条表头线（略粗）+ 3 行，行里用一条竖线分出列
        let rows = 3;
        let line_h = 2;
        let gap = 3;
        let head_h = 2;
        let total = head_h + 2 + rows * line_h + (rows - 1) * gap;
        let x0 = r.left + 6;
        let bw = (w - 12).max(6);
        let y0 = r.top + (h - total) / 2;
        let _ = FillRect(hdc, &RECT { left: x0, top: y0, right: x0 + bw, bottom: y0 + head_h }, brush);
        let mut y = y0 + head_h + 2;
        for _k in 0..rows {
            let _ = FillRect(hdc, &RECT { left: x0, top: y, right: x0 + bw, bottom: y + line_h }, brush);
            // 列分隔：第一行之后那一段竖线，做出"表格有列"的样子
            y += line_h + gap;
        }
        let _ = FillRect(
            hdc,
            &RECT { left: x0 + bw * 3 / 5, top: y0 + head_h + 2, right: x0 + bw * 3 / 5 + 1, bottom: y - gap },
            brush,
        );
    }
    let _ = DeleteObject(brush.into());
}

/// 画状态栏右下角的两个布局切换按钮。
fn paint_view_switch(app: &App, hdc: HDC, l: &Layout) {
    let t = &app.theme;
    let (icons, details) = status_view_rects(l, &app.metrics);
    let cur = app.tab().view_mode;
    let is_details = matches!(cur, fm_core::ViewMode::Details);
    unsafe {
        for (i, (r, active, grid)) in [(icons, !is_details, true), (details, is_details, false)].iter().enumerate() {
            let hovered = app.hover_view_btn == (i as u8) + 1;
            if hovered || *active {
                fill_radius(
                    app,
                    hdc,
                    "status.button",
                    r,
                    if hovered { t.hover_bg } else { t.selection_bg },
                    None,
                );
            }
            let color = if *active || hovered { t.text } else { t.text_dim };
            draw_view_glyph(hdc, r, *grid, color);
        }
    }
}

pub fn paint_status(app: &App, hdc: HDC, l: &Layout) {
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
        // 状态文字与视图切换是「界面」页状态栏那条带里的两个独立组件，
        // 收起哪一个都得真的消失（并且另一个要能吃掉空出来的地盘）。
        let show_switch = chrome_on(app, crate::settings::W_VIEW_SWITCH);
        if chrome_on(app, crate::settings::W_STATUS_TEXT) {
            let text = app.status_text();
            let view_left = if show_switch {
                status_view_left(l, &app.metrics)
            } else {
                l.status.right
            };
            draw_text(
                hdc,
                &text,
                RECT { left: l.status.left + 10, top: l.status.top, right: view_left - 8, bottom: l.status.bottom },
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
        }
        SelectObject(hdc, old);
        if show_switch {
            paint_view_switch(app, hdc, l);
        }
    }
}

// ---------------------------------------------------------------- 子控件

const ID_ADDRESS: isize = 101;
const ID_SEARCH: isize = 102;
const ID_RENAME: isize = 103;

const EM_SETMARGINS: u32 = 0x00D3;
const EC_LEFTMARGIN: u32 = 0x0001;
const EC_RIGHTMARGIN: u32 = 0x0002;

/// 搜索框内边距：左 26（放大镜 + 间隙）、右 8。`EM_SETMARGINS` 的 lparam
/// 是 **低 16 位左、高 16 位右**，写反了图标和文字就叠在一起。
fn margins_for(scale: f32) -> isize {
    let l = (26.0 * scale).round() as i32;
    let r = (8.0 * scale).round() as i32;
    ((l & 0xFFFF) | ((r & 0xFFFF) << 16)) as isize
}

/// 输入框统一的内边距（左右各 8）。系统 EDIT 默认左 1 右 1，文字几乎贴着边框，
/// 和自绘 UI 那套"内容离边 8px"的规矩对不上。
pub fn edit_margins(scale: f32, left: i32, right: i32) -> isize {
    let l = ((left as f32) * scale).round() as i32;
    let r = ((right as f32) * scale).round() as i32;
    ((l & 0xFFFF) | ((r & 0xFFFF) << 16)) as isize
}

/// 给任意 EDIT 设左右内边距。低 16 位是左、高 16 位是右，写反了文字就贴错边。
pub fn set_edit_margins(hwnd: HWND, scale: f32, left: i32, right: i32) {
    unsafe {
        let _ = SendMessageW(
            hwnd,
            EM_SETMARGINS,
            Some(WPARAM((EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize)),
            Some(LPARAM(edit_margins(scale, left, right))),
        );
    }
}

/// 搜索框里的放大镜 + 空态提示文字。
///
/// EDIT 自己画完背景和文字之后再补画这两样 —— 画在它上面，而不是画在父窗口上
/// （父窗口先画，随后被 EDIT 的整块背景盖掉，什么都看不见）。
fn paint_search_decor(hwnd: HWND, hdc_in: Option<HDC>) {
    let app = match crate::app_ref_opt() {
        Some(a) => a,
        None => return,
    };
    let t = app.theme;
    let scale = app.metrics.scale;
    let empty = read_window_text(hwnd).is_empty();
    unsafe {
        let hdc = match hdc_in {
            Some(h) => h,
            None => GetDC(Some(hwnd)),
        };
        if hdc.is_invalid() {
            return;
        }
        let mut r = RECT::default();
        let _ = GetClientRect(hwnd, &mut r);
        SetBkMode(hdc, TRANSPARENT);
        // 放大镜：垂直居中，左边距 6
        let g = glyph_font(scale);
        let old = SelectObject(hdc, g.into());
        SetTextColor(hdc, t.text_dim);
        let side = ((r.bottom - r.top) as f32 * 1.1).round() as i32;
        let ir = RECT {
            left: (6.0 * scale).round() as i32,
            top: (r.top + r.bottom - side) / 2,
            right: (6.0 * scale).round() as i32 + side,
            bottom: (r.top + r.bottom + side) / 2,
        };
        draw_text(hdc, "\u{E721}", ir, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        SelectObject(hdc, old);
        if empty {
            // 提示文字：跟着输入的起点（左边距之后），垂直居中
            let f = ui_font(scale);
            let old2 = SelectObject(hdc, f.into());
            SetTextColor(hdc, t.text_dim);
            let tr = RECT {
                left: (26.0 * scale).round() as i32,
                top: r.top,
                right: r.right - (8.0 * scale).round() as i32,
                bottom: r.bottom,
            };
            draw_text(
                hdc,
                &app.search_hint(),
                tr,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
            SelectObject(hdc, old2);
        }
        if hdc_in.is_none() {
            let _ = ReleaseDC(Some(hwnd), hdc);
        }
    }
}

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
        set_edit_margins(addr, app.metrics.scale, 8, 8);
        // 平时是面包屑，EDIT 只在编辑态才露出来（Win11 的资源管理器也是这样）。
        let _ = ShowWindow(addr, SW_HIDE);

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
        // 左边留出放大镜的位置（EC_LEFTMARGIN），否则输入的文字会压在图标下面
        let _ = SendMessageW(
            search,
            EM_SETMARGINS,
            Some(WPARAM((EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize)),
            Some(LPARAM(margins_for(app.metrics.scale))),
        );
    }
}

/// 给 EDIT 子控件套上 UI 字体（默认是系统 12px 位图字体，与自绘 UI 完全不搭）。
pub(crate) fn set_edit_font(hwnd: HWND, scale: f32) {
    unsafe {
        let f = ui_font(scale);
        let _ = SendMessageW(hwnd, WM_SETFONT, Some(WPARAM(f.0 as usize)), Some(LPARAM(1)));
    }
}

fn position_children(app: &App) {
    let l = layout_for(app);
    unsafe {
        let _ = SetWindowPos(app.hwnd_address, None, l.address.left, l.address.top, l.address.right - l.address.left, l.address.bottom - l.address.top, SWP_NOZORDER | SWP_NOACTIVATE);
        let _ = SetWindowPos(app.hwnd_search, None, l.search.left, l.search.top, l.search.right - l.search.left, l.search.bottom - l.search.top, SWP_NOZORDER | SWP_NOACTIVATE);
        // 两个子 EDIT 的空态提示（放大镜 + "搜索 X"）画在**它们自己身上**，而
        // 主窗口带 WS_CLIPCHILDREN，父窗口重绘永远不碰那一块 —— 换标签、挪
        // 位置之后必须显式让它们重画。否则提示词停在旧标签的文字上（实测：
        // 把搜索框从排布里摘掉再装回，提示停在"搜索设置"不回来）。
        let _ = InvalidateRect(Some(app.hwnd_address), None, true);
        let _ = InvalidateRect(Some(app.hwnd_search), None, true);
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
///
/// `select_stem`：只把**文件名主部**圈进选区，把扩展名留在外面。
/// 资源管理器按 F2 / 新建之后都是这个选法 —— 用户要改的是名字本身，
/// 全选的话一敲字连 `.txt` 一起被吃掉，文件类型就没了。
/// 目录没有扩展名，退化成全选。
pub fn create_rename_editor(app: &mut App, select_stem: bool) {
    if app.rename_hwnd.is_some() {
        return;
    }
    let initial = app
        .rename_index
        .and_then(|i| app.tab().entries.get(i).map(|e| e.name.clone()))
        .unwrap_or_default();
    let edit_class = HSTRING::from("EDIT");
    let row_h = app.metrics.row_height_for(app.tab().view_mode);
    let font = ui_font(app.metrics.scale);
    let l = layout_for(app);
    let header_h = if app.tab().view_mode == fm_core::ViewMode::Details {
        (26.0 * app.metrics.scale) as i32
    } else {
        0
    };
    let y = l.view.top + header_h + app.tab().focus_row as i32 * row_h - app.tab().scroll;
    // 编辑框宽度跟着名字长度走，但**最长不得超过"名称"这一列的右沿**：
    // 以前写死 300px，短名字后面拖一条空框、长名字又顶出列外压到"修改日期"上。
    let x = l.view.left + (30.0 * app.metrics.scale) as i32;
    let max_right = if app.tab().view_mode == fm_core::ViewMode::Details {
        let cols = detail_cols(app, &l, (16.0 * app.metrics.scale) as i32);
        (cols.name + cols.name_w).min(l.view.right)
    } else {
        l.view.right - (8.0 * app.metrics.scale) as i32
    };
    let tw = unsafe {
        let hdc = GetDC(Some(app.hwnd));
        let old = SelectObject(hdc, font.into());
        let w = text_width(hdc, font, &initial);
        SelectObject(hdc, old);
        let _ = ReleaseDC(Some(app.hwnd), hdc);
        w
    };
    let w = (tw + (28.0 * app.metrics.scale) as i32)
        .max((90.0 * app.metrics.scale) as i32)
        .min((max_right - x).max((70.0 * app.metrics.scale) as i32));
    unsafe {
        let hwnd = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            windows::core::PCWSTR(edit_class.as_ptr()),
            windows::core::PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            x,
            y,
            w,
            row_h,
            Some(app.hwnd),
            Some(HMENU(ID_RENAME as *mut core::ffi::c_void)),
            None,
            None,
        )
        .unwrap_or_default();
        app.rename_hwnd = Some(hwnd);
        RENAME_OLD = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, (rename_proc as *const ()) as isize);
        set_edit_font(hwnd, app.metrics.scale);
        set_edit_margins(hwnd, app.metrics.scale, 4, 4);
        let text = wide(&initial);
        let _ = SetWindowTextW(hwnd, windows::core::PCWSTR(text.as_ptr()));
        // 选区终点：默认整串；只选主部时停在最后一个点之前。
        // 点号在**第 0 位**（".gitignore" 这类）不算扩展名分隔符，仍然全选。
        let end = if select_stem {
            match initial.rfind('.') {
                Some(p) if p > 0 => p as isize,
                _ => -1isize,
            }
        } else {
            -1isize
        };
        let _ = windows::Win32::UI::WindowsAndMessaging::SendMessageW(
            hwnd,
            EM_SETSEL,
            Some(WPARAM(0)),
            Some(LPARAM(end)),
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
    // 焦点离开地址框就收回面包屑（不抢焦点，避免来回踢）
    if msg == WM_KILLFOCUS {
        crate::on_address_blur();
        return LRESULT(0);
    }
    call_old(ADDR_OLD, hwnd, msg, wparam, lparam)
}

unsafe extern "system" fn search_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // 拿到/失去焦点时 EDIT 只会**局部**重画（通常只有插入符那一小块），而装饰是
    // 画在 `GetDC` 拿到的 DC 上的 —— 裁剪区不是整块，放大镜和提示字就只剩一点点
    // 残影，整框看着像被清空了（实测点一下：非底色像素 568 → 26）。所以焦点一变
    // 就整块无效掉，逼它把装饰连同背景一起重画一遍。
    if msg == WM_SETFOCUS || msg == WM_KILLFOCUS {
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
    // 文本被外部改掉（清空搜索框、Ctrl+F 预热等）也要整块重画：放大镜要不要留、
    // 空态提示要不要显示，都取决于"现在是不是空的"。
    if msg == WM_SETTEXT {
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
    let r = call_old(SEARCH_OLD, hwnd, msg, wparam, lparam);
    // 放大镜和空态提示必须画在 EDIT **自己画完之后**：画在父窗口上会被 EDIT
    // 的整块背景盖掉。WM_PRINT 那条分支是给 PrintWindow 用的（抓图验证走这条路，
    // 它给子窗口发的是 WM_PRINT 而不是 WM_PAINT）。
    if msg == WM_PAINT {
        paint_search_decor(hwnd, None);
    } else if msg == WM_PRINT || msg == WM_PRINTCLIENT {
        paint_search_decor(hwnd, Some(HDC(wparam.0 as *mut core::ffi::c_void)));
    }
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

/// 进入地址栏编辑态：把平时藏着的 EDIT 显示出来、填入真实路径并全选。
///
/// Win11 的地址栏点了空白处才变成输入框 —— 平时是面包屑，直接拿一个常驻的
/// EDIT 当地址栏，既不像资源管理器，也没法一节一节点着跳。
pub fn begin_address_edit(app: &mut App) {
    if app.addr_editing {
        return;
    }
    app.addr_hover = None;
    // 先把真实路径写进去，**之后**再上锁：sync_address 在编辑态下是禁写的
    // （见它自己的注释），顺序反了框里就是空的。
    sync_address(app);
    app.addr_editing = true;
    unsafe {
        let _ = ShowWindow(app.hwnd_address, SW_SHOW);
        let _ = SetFocus(Some(app.hwnd_address));
        let _ = SendMessageW(app.hwnd_address, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
        let _ = InvalidateRect(Some(app.hwnd), None, true);
    }
}

/// 退出编辑态。`refocus` 为 false 时不要把焦点抢回主窗口 —— 在 WM_KILLFOCUS
/// 里再调 SetFocus 会来回踢焦点。
pub fn end_address_edit(app: &mut App, refocus: bool) {
    if !app.addr_editing {
        return;
    }
    app.addr_editing = false;
    unsafe {
        let _ = ShowWindow(app.hwnd_address, SW_HIDE);
        if refocus {
            let _ = SetFocus(Some(app.hwnd));
        }
        let _ = InvalidateRect(Some(app.hwnd), None, true);
    }
}

pub fn sync_address(app: &App) {
    // 编辑态下**绝不重写**地址框。sync_address 是把"当前路径"灌回 EDIT 的，
    // 而它会在各种时机被顺手调一次（导航、切标签、快捷键……）；正在手打路径的
    // 时候被任何一次撞上，刚敲进去的字就会被路径顶掉 —— 表现就是
    // "输入内容后动一下鼠标，内容没了"。唯一的例外是 begin_address_edit，
    // 它先把文本写好再上锁（那边特意调换了顺序）。
    if app.addr_editing {
        return;
    }
    // 主页那一页（常用文件夹 + 本机驱动器）在资源管理器里地址栏写的就是"主页"，
    // 侧栏虽然把首页并到了"此电脑"这一格，但**地址栏要标的是"这是哪一页"**，
    // 不是"从哪点进来的"，所以这里写"主页"。
    let text = if app.tab().home {
        "主页".to_string()
    } else if app.tab().recycle {
        "回收站".to_string()
    } else if app.tab().settings {
        "设置".to_string()
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
