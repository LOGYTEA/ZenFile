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
    GetTextExtentPoint32W, GetTextMetricsW, IntersectClipRect, InvalidateRect, LineTo, MoveToEx,
    PS_SOLID, Rectangle,
    TEXTMETRICW,
    ReleaseDC, RestoreDC, RoundRect, SaveDC, SelectClipRgn, SelectObject, SetBkMode, SetTextColor,
    DT_CENTER,
    DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, DRAW_TEXT_FORMAT,
    FONT_CHARSET, FONT_CLIP_PRECISION, FONT_OUTPUT_PRECISION, FONT_QUALITY, GetStockObject, HBITMAP,
    HDC, HGDIOBJ, HFONT, NULL_BRUSH, NULL_PEN, Polygon, SetBkColor, SRCCOPY, OPAQUE, TRANSPARENT,
    RGN_AND,
    BLACK_PEN, SYSTEM_FONT, WHITE_BRUSH,
};
use windows::Win32::Foundation::POINT;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Controls::EM_SETSEL;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::Foundation::HINSTANCE;
use windows::Win32::UI::WindowsAndMessaging::HICON;

use crate::app::{human, App};
use crate::app::{CAP_CLOSE_TAB, CAP_MENU, CAP_NEW_TAB, CAP_TOGGLE};
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

/// ui_font 的**字体单元格**高度（tmHeight，含内部行距）。
///
/// 单行 EDIT 的字形永远画在单元格**顶部**，它自己根本不做垂直居中
/// （第40轮纯 Win32 探针证实：28px 高的 EDIT 里墨迹落在第 14..19 行）。
/// YaHei 的内部行距大，于是所有输入框的文字都整体偏上约 5px —— 用户报
/// "所有的输入框都会有这个问题（文字贴顶）"。要居中文字，只能把 EDIT
/// 本身的位置按单元格来摆（见 `edit_vcenter`）。
fn ui_font_cell_h(scale: f32) -> i32 {
    unsafe {
        if CELL_H == 0 || (CELL_H_SCALE - scale).abs() > 0.01 {
            let hdc = GetDC(None);
            let old = SelectObject(hdc, ui_font(scale).into());
            let mut tm = TEXTMETRICW::default();
            let _ = GetTextMetricsW(hdc, &mut tm);
            SelectObject(hdc, old);
            let _ = ReleaseDC(None, hdc);
            CELL_H = tm.tmHeight.max(1);
            CELL_H_SCALE = scale;
        }
        CELL_H
    }
}
static mut CELL_H: i32 = 0;
static mut CELL_H_SCALE: f32 = -1.0;

/// 把一个"跟内容行等高"的矩形调整成子 EDIT 的实际摆放位置：
/// **底边不动、顶部下移到字体单元格垂直居中**的位置。
///
/// 单行 EDIT 的字形画在单元格顶部（见 `ui_font_cell_h`），所以想让文字
/// 在工具栏里看起来居中，必须居中的是"单元格"而不是让 EDIT 摆满行高 ——
/// 摆满的话文字就贴着上沿（地址栏/搜索框实测偏上 4.5px，第40轮）。
pub fn edit_vcenter(r: &RECT, scale: f32) -> RECT {
    let cell = ui_font_cell_h(scale);
    let h = (r.bottom - r.top).max(0);
    let top = r.top + ((h - cell) / 2).max(0);
    RECT {
        left: r.left,
        top,
        right: r.right,
        bottom: r.bottom,
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

pub fn icon_grid(l: &Layout, m: &Metrics, mode: zenfile_core::ViewMode) -> IconGrid {
    let icon = if mode == zenfile_core::ViewMode::LargeIcons { 96.0 } else { 48.0 };
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
    if matches!(mode, zenfile_core::ViewMode::LargeIcons | zenfile_core::ViewMode::MediumIcons) {
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

/// 第74轮：一张**按窗口尺寸缓存复用**的全窗口离屏位图。
///
/// 之前每个 `WM_PAINT` 都重新 `CreateCompatibleDC` + `CreateCompatibleBitmap`
/// 一张**整窗大小**的 DDB，画完 `BitBlt` 上屏就 `DeleteObject` + `DeleteDC`。
/// 实测（第73轮埋点，静态重绘路径）这两步合计约 **1.15 ms/帧**，而画在位图上
/// 的内容只有几 ms —— 也就是说每帧有相当大一块开销纯粹花在"申请 / 归还 GDI
/// 内存"上，与要画什么无关。窗口尺寸不变时这张位图**完全可复用**：里面每一帧
/// 都会被 `FillRect` 整个铺满一遍（第 4 步 `sub_fill` 一个字没删），所以不会
/// 有上一帧的残影。
///
/// 字段与生命周期：
///
/// * `dc` —— 内存 DC，**整个窗口生命周期内一直存在**，每帧 `SelectObject`
///   换一张位图进去，画完换回 `old_bmp`。
/// * `bmp` —— 当前这张全窗口 DDB；`w` / `h` 记着它是按什么尺寸建的。
/// * `old_bmp` —— `dc` 上"换走 `bmp` 之前选中的那个 GDI 对象"（建 DC 时系统
///   给的 1x1 单色库存位图）。画完一帧把它换回去，好让 `bmp` 处于"未被选中"
///   的状态 —— 删一个正被选中的 GDI 对象是不允许的，而销毁走的正是这条路。
///   它是**库存对象，不能 `DeleteObject`**，销毁时只换回、不删。
///
/// `Default` 出来的实例是"空的"，可以直接挂在 `App` 上。
#[derive(Clone, Copy)]
pub struct BackSurface {
    /// 内存 DC。空 = 还没建。
    pub dc: HDC,
    /// 当前选在这张 DC 上的离屏位图。
    pub bmp: HBITMAP,
    /// 换走 `bmp` 之前 DC 上选中的对象（建 DC 时是库存 1x1 单色位图）。
    pub old_bmp: HGDIOBJ,
    /// `bmp` 的宽度（像素）。
    pub w: i32,
    /// `bmp` 的高度（像素）。
    pub h: i32,
}

impl Default for BackSurface {
    fn default() -> Self {
        Self {
            dc: HDC(std::ptr::null_mut()),
            bmp: HBITMAP(std::ptr::null_mut()),
            old_bmp: HGDIOBJ(std::ptr::null_mut()),
            w: 0,
            h: 0,
        }
    }
}

impl BackSurface {
    /// 现在这张位图能不能直接给 `width x height` 用。
    #[inline]
    fn matches(&self, width: i32, height: i32) -> bool {
        !self.dc.is_invalid()
            && !self.bmp.is_invalid()
            && self.w == width
            && self.h == height
    }

    /// 销毁位图与 DC（幂等：空实例直接返回，重复调用安全）。
    ///
    /// `unsafe`：内部调 GDI。
    pub unsafe fn destroy(&mut self) {
        if !self.dc.is_invalid() {
            // 只有位图是我们自己建的才 DeleteObject：`old_bmp` 是库存对象，
            // 释放它会让整个进程后面所有 DC 的默认位图变成野指针。
            if !self.bmp.is_invalid() {
                // 先把位图从 DC 上摘下来再删：删一个正选中的 GDI 对象不会失败，
                // 但 DC 会留下一句"选中了已删对象"的记录，之后 SelectObject
                // 会失败（返回 0），整窗就再也画不出东西了。
                SelectObject(self.dc, self.old_bmp);
                let _ = DeleteObject(self.bmp.into());
            }
            let _ = DeleteDC(self.dc);
        }
        *self = Self::default();
    }
}

/// 第74轮：销毁主窗口那张缓存的全窗口离屏位图。
///
/// `WM_SIZE` / `WM_DPICHANGED` / `WM_DISPLAYCHANGE` / `WM_DESTROY` 都要显式调它 ——
/// 等下一帧 lazy 重建的话，**旧的大图会被一直占着**（最大化之后再也不动的话，
/// 一张 4K 位图能挂在进程里到关窗口为止）。
pub fn invalidate_back_surface(app: &mut App) {
    unsafe { app.back_surface.destroy() };
}

/// 第74轮：诊断开关。设了 `ZENFILE_SURF_DEBUG=1` 就把"这一帧是新建还是复用
/// 离屏位图"打到 stderr。
///
/// 用 `OnceLock` **缓存一次**而不是每帧 `env::var_os` —— 后者每帧都要进
/// 一次环境块加锁 + 分配，而这一段是要被测量的热路径。
fn surf_debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("ZENFILE_SURF_DEBUG").is_some())
}

/// 第74轮：这一帧要用的离屏 DC（**位图已经选在上面，可以直接画**）。
///
/// * 尺寸没变 → 复用缓存那张，只补一次 `SelectObject` + 状态复位；
/// * 尺寸变了 → 先把旧图按"先销毁再建"处理，再按新尺寸建；
/// * 宽或高为 0（最小化时 `GetClientRect` 就是 0x0）→ **建都不建**，返回
///   `None` 让 `paint()` 整帧跳过。`CreateCompatibleBitmap(0, h)` 会失败，
///   拿到的 `HBITMAP` 是空句柄，往 DC 里一选整窗就再也没法画东西了。
///
/// 副作用：`app.back_surface` 被就地改成这一帧该用的那一张（新建或复用）。
fn acquire_back_surface(
    app: &mut App,
    hdc: HDC,
    width: i32,
    height: i32,
) -> Option<HDC> {
    if width <= 0 || height <= 0 {
        // 最小化：这一帧什么都不画。也顺手把旧图放掉 —— 最小化期间客户区是
        // 0x0，继续攥着一张全尺寸图纯属白占内存。
        invalidate_back_surface(app);
        return None;
    }
    let dbg = surf_debug();
    if !app.back_surface.matches(width, height) {
        // 尺寸对不上：旧的按"先销毁再建"处理（不要在这张旧图上接着画）。
        // 不这么做的话，从 800x600 拖到 4K 的路上进程里会同时挂着几十张
        // 不同尺寸的整窗位图。
        unsafe { app.back_surface.destroy() };
        unsafe {
            let dc = CreateCompatibleDC(Some(hdc));
            if !dc.is_invalid() {
                let bmp = CreateCompatibleBitmap(hdc, width, height);
                if bmp.is_invalid() {
                    // 位图没建出来就把 DC 一起收掉，绝不留下"有 DC 没位图"的
                    // 半截状态（否则这里就漏一个 DC）。
                    let _ = DeleteDC(dc);
                } else {
                    let old_bmp = SelectObject(dc, bmp.into());
                    app.back_surface =
                        BackSurface { dc, bmp, old_bmp, w: width, h: height };
                }
            }
        }
        if dbg {
            eprintln!("[surf] CREATE {width}x{height} ok={}", app.back_surface.dc.is_invalid());
        }
    } else if dbg {
        eprintln!("[surf] reuse {width}x{height}");
    }
    if app.back_surface.dc.is_invalid() {
        return None;
    }
    let dc = app.back_surface.dc;
    unsafe {
        // **复用路径的关键动作**：把缓存位图选到 DC 上。
        //
        // 上一帧末尾 `release_back_surface` 已经把位图摘回 `old_bmp` 了
        // （不摘的话 `destroy()` 里"先换回再删"的顺序对不上），所以这里
        // **必须重新选一次** —— 少了这一句，DC 上选着的是建 DC 时系统给的
        // 1x1 单色库存位图，整窗画上去的东西全被裁成左上角一个像素，
        // 而 `BitBlt` 依然"成功"（画面全黑，但没有任何报错）。
        SelectObject(dc, app.back_surface.bmp.into());
        // 复用路径的第二件必做事：把 DC 拨回"刚建出来"的状态。DC 上还留着
        // 上一帧的字体/笔/刷/`SetBkMode`/文字色，以及 `SaveDC`/`RestoreDC`
        // 没能复位时收窄过的裁剪区 —— 新建 DC 时这些全是系统默认值，
        // 所以"新建"和"复用"并不天然等价。
        //
        // 顺带一个好处：`reset_surface_dc` 排在 `FillRect` 之前，于是
        // `fill` 这一步量到的不再是"顺带把 DC 拨回去"的时间，而是纯粹的
        // 整窗铺色，两种路径的 `fill` 数字从此可比。
        reset_surface_dc(dc);
    }
    Some(dc)
}

/// 第74轮：把这一帧留在缓存 DC 上的位图摘下来（画完、`BitBlt` 之后调用）。
///
/// **只摘不删**：位图还在 `app.back_surface` 里，下一帧尺寸不变就能接着用。
/// 摘的动作不能省 —— DC 上选中一张位图时它不能被删除，而 `destroy()` 里
/// 是"换回 `old_bmp` 再删"的顺序，两边必须对得上。
#[inline]
fn release_back_surface(app: &mut App) {
    if !app.back_surface.dc.is_invalid() && !app.back_surface.bmp.is_invalid() {
        unsafe {
            SelectObject(app.back_surface.dc, app.back_surface.old_bmp);
        }
    }
}

/// 把缓存 DC 重置成一张"干净的白底、默认字体黑笔、无裁剪"的画布。
///
/// 见 `acquire_back_surface` 里那段说明：复用位图之后 DC 上会**留着上一帧
/// 的绘图状态**，新建 DC 时这些全是系统默认值，所以每帧都要显式拨回去。
///
/// 选 `GetStockObject` 的那几个对象**不能删**：它们属于 GDI，不归我们管。
/// `WHITE_BRUSH` / `BLACK_PEN` / `SYSTEM_FONT` 正是 `CreateCompatibleDC`
/// 刚建出来时 DC 上默认选中的那三个。
fn reset_surface_dc(dc: HDC) {
    unsafe {
        let _ = SelectObject(dc, GetStockObject(WHITE_BRUSH));
        let _ = SelectObject(dc, GetStockObject(BLACK_PEN));
        let _ = SelectObject(dc, GetStockObject(SYSTEM_FONT));
        // `SelectClipRgn(None)` 把裁剪区恢复成整个 DC —— 上一帧
        // `IntersectClipRect` 收窄过的区域必须在这里松开。
        let _ = SelectClipRgn(dc, None);
        SetBkMode(dc, OPAQUE);
        SetBkColor(dc, COLORREF(0x00FF_FFFF));
        SetTextColor(dc, COLORREF(0x0000_0000));
    }
}

/// 让两个子 EDIT **按需**重画：内容真变了才 `InvalidateRect`（第75轮）。
///
/// 取代第74轮之前 `paint()` 里那两句无条件
/// `InvalidateRect(hwnd, None, true)` —— 那两句让每帧多出两次子窗口
/// `WM_ERASEBKGND` + `WM_PAINT`，而它们真正要解决的只有一件事：让空态提示词
/// 跟着当前标签/目录变。判据见 `App::search_child_key` /
/// `address_child_key` 与 `ChildEditKey`。
///
/// 失效条件就一条：
///
/// ```text
/// 该画 ⟺ 快照变了 ∨ 焦点在这个 EDIT 上 ∨ （仅地址框）它现在可见
/// ```
///
/// * **快照变了** —— 常态下唯一会成立的项，也是第74轮之前那句无条件失效的
///   唯一真实理由。
/// * **焦点在这个 EDIT 上** —— 编辑态**一律放行**。正在打字时框里那个插入符、
///   选区高亮都由 EDIT 自己管，我们插队既不会让输入更跟手，只会在同一帧里多
///   一次子窗口重绘；但快照判据在编辑态期间常常是"没变"（提示词一个字不动），
///   于是输入过程中一帧都不失效。把焦点这一档显式放行，等于承认"编辑态的像素
///   由焦点驱动"这件事，代价只有编辑态那几帧 —— 换来的是绝不会有"正在输入
///   时框里只剩半截装饰"这种坑。
/// * **地址框可见** —— 见下。
///
/// # 地址框不是"永远隐藏"（原判断有误）
///
/// `create_children` 里它是 `SW_HIDE`，`begin_address_edit` 会
/// `ShowWindow(SW_SHOW)`（点地址栏空白处 / 按 Ctrl+L / F4 就进编辑态）。所以
/// 判据是"**可见 ∧ 显示文本（或墨色/缩放）变了**"，不能无条件删掉那句失效。
/// 隐藏态则**什么都不做**：原来那句每帧都对隐藏窗口调一次 `InvalidateRect`，
/// 全程不产生任何重绘（实测 `addr` 行 `paint_cnt`/`erase_cnt` 恒为 0），纯属
/// 白花约 2.5µs/帧。
///
/// `erase=true` 保持不变：装饰（放大镜 + 提示词）是画在 EDIT 整块背景**之上**
/// 的，不连背景一起擦就会留下残影（`search_proc_inner` 里
/// `WM_SETFOCUS` / `WM_SETTEXT` 那两处无条件失效用的也是 `true`，同一道理）。
fn invalidate_child_edits(app: &mut App) {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetFocus;
    unsafe {
        // 只问一次焦点：两个 EDIT 互斥，同一帧里最多命中一个。
        let focus = GetFocus();

        // ---- 搜索框（常驻可见）----
        if !app.hwnd_search.0.is_null() {
            let key = app.search_child_key();
            let changed = key != app.child_search_key;
            if changed {
                app.child_search_key = key;
            }
            if changed || focus == app.hwnd_search {
                crate::anim_clock::trace_invalidate(0);
                let _ = InvalidateRect(Some(app.hwnd_search), None, true);
            }
        }

        // ---- 地址框（编辑态才可见）----
        if !app.hwnd_address.0.is_null() {
            let key = app.address_child_key();
            let changed = key != app.child_address_key;
            // `visible` 要在把 `key` 移进缓存**之前**读出来 —— 移走之后
            // 就不能再看它了（`ChildEditKey` 不做 `Copy`，String 字段是唯一的）。
            let visible = key.visible;
            if changed {
                app.child_address_key = key;
            }
            if visible && (changed || focus == app.hwnd_address) {
                crate::anim_clock::trace_invalidate(1);
                let _ = InvalidateRect(Some(app.hwnd_address), None, true);
            }
        }
    }
}

pub fn paint(app: &mut App, hdc: HDC, width: i32, height: i32) {
    use crate::anim_clock as ac;
    // 第70轮埋点：`begin_anim_frame` 前打点。
    ac::trace_sub_begin(ac::SUB_BEGIN_ANIM);
    // 动画的"每帧开始"：把上一帧的 悬停/选中 集合滚成 prev（判"刚变成
    // 这个状态"用），并清空这一帧的放大目标（没写到的项 = 不放大）。
    app.begin_anim_frame();
    ac::trace_sub_end(ac::SUB_BEGIN_ANIM);
    unsafe {
        // ---- 取离屏表面：尺寸不变就复用缓存那张（第74轮）----
        //
        // `sub_create_surface` 现在的内容 = 尺寸判等 + （变了才走的）重建
        // + 一次 `SelectObject` + DC 状态复位。尺寸不变时它不再申请/归还
        // 任何 GDI 内存，基线里那 0.2~0.9 ms 就此消失。
        ac::trace_sub_begin(ac::SUB_CREATE_SURFACE);
        let Some(mem) = acquire_back_surface(app, hdc, width, height) else {
            // 客户区是 0x0（最小化）：整帧不画，直接返回。
            //
            // 剩下那些子步骤**故意一个打点都不写**：`anim_clock` 的约定是
            // "`sub_begin[i] == 0` 才算这一步没跑"（`sub_end[i] == 0` 还要
            // 兼容 QPC 读失败）。留 0 就等于如实告诉分析脚本"这一帧的
            // create_surface 之后的步骤全部没执行"，正好是我们想表达的。
            //
            // `begin_anim_frame` 已经在上面跑过了 —— 它与窗口尺寸无关，
            // 而且这一帧确实推进了一次动画状态，不能因为"没画面"就跳过。
            return;
        };
        ac::trace_sub_end(ac::SUB_CREATE_SURFACE);

        // ---- 布局计算（~200 处窗格 / 标签 / 列宽）----
        ac::trace_sub_begin(ac::SUB_LAYOUT);
        let l = layout_sized(app, width, height);
        ac::trace_sub_end(ac::SUB_LAYOUT);

        // 第70轮：布局算完，此刻采一次判据快照 —— 布局里的 `l.view` 与
        // `app` 的视图状态在这一帧内不再变，采一次就够。
        trace_paint_context(app, width, height, &l);

        // **复用位图后这一步一个字都不能删**：离屏图上还有上一帧的画面，
        // 不铺满一遍就会看到残影（滚动/切目录时是"上一屏的文件名糊在下面"）。
        ac::trace_sub_begin(ac::SUB_FILL);
        let t = app.theme;
        let bg = CreateSolidBrush(t.window_bg);
        let all = RECT { left: 0, top: 0, right: width, bottom: height };
        FillRect(mem, &all, bg);
        let _ = DeleteObject(bg.into());
        ac::trace_sub_end(ac::SUB_FILL);

        ac::trace_sub_begin(ac::SUB_TITLE);
        paint_title_bar(app, mem, &l);
        ac::trace_sub_end(ac::SUB_TITLE);

        ac::trace_sub_begin(ac::SUB_TOOLBAR);
        paint_toolbar(app, mem, &l);
        ac::trace_sub_end(ac::SUB_TOOLBAR);

        if app.cfg.ui.sidebar_visible {
            ac::trace_sub_begin(ac::SUB_SIDEBAR);
            paint_sidebar(app, mem, &l);
            ac::trace_sub_end(ac::SUB_SIDEBAR);
        }
        ac::trace_sub_begin(ac::SUB_PANES);
        paint_panes(app, mem, &l);
        ac::trace_sub_end(ac::SUB_PANES);

        if app.cfg.ui.statusbar_visible {
            ac::trace_sub_begin(ac::SUB_STATUS);
            paint_status(app, mem, &l);
            ac::trace_sub_end(ac::SUB_STATUS);
        }

        ac::trace_sub_begin(ac::SUB_DRAG_OVERLAY);
        paint_drag_overlay(app, mem, &l);
        ac::trace_sub_end(ac::SUB_DRAG_OVERLAY);

        // 拖标签时的"8 向贴"预览：压在最上面
        ac::trace_sub_begin(ac::SUB_DOCK_OVERLAY);
        paint_dock_overlay(app, mem, &l);
        ac::trace_sub_end(ac::SUB_DOCK_OVERLAY);

        // ---- 全窗口上传 ----
        ac::trace_sub_begin(ac::SUB_BITBLT);
        let _ = BitBlt(hdc, 0, 0, width, height, Some(mem), 0, 0, SRCCOPY);
        ac::trace_sub_end(ac::SUB_BITBLT);

        // 第74轮：把位图从 DC 上摘下来（**不删**，留给下一帧复用）。
        // 这一步替代了原来的 `SelectObject(mem, old_bmp)` + `DeleteObject`
        // + `DeleteDC`，所以 `sub_cleanup` 现在只剩一次 `SelectObject`。
        ac::trace_sub_begin(ac::SUB_CLEANUP);
        release_back_surface(app);
        ac::trace_sub_end(ac::SUB_CLEANUP);

        ac::trace_sub_begin(ac::SUB_INVALIDATE);
        // 第75轮：两个子 EDIT 从"每帧无条件失效"改成"内容真变了才失效"。
        //
        // 背景（也是这件事唯一的真实理由）：地址框 / 搜索框的空态提示（放大镜
        // + "搜索 X"）画在**子 EDIT 自己身上**，而主窗口带 `WS_CLIPCHILDREN`，
        // 父窗口重绘永远不碰它们那两块。提示词跟着当前标签变
        // （"搜索设置" / "搜索 回收站" / "搜索 桌面"），标签切换却只重绘父
        // 窗口 —— 不在这里显式让子 EDIT 跟着重画，提示词就停在旧标签的文字上
        // （实测：把搜索框从排布里摘掉再装回，提示一直是"搜索设置"）。
        //
        // 但"每帧都失效"是**为了上面那一个理由付出的每帧代价**：实测每帧多出
        // 两次子窗口 `WM_ERASEBKGND` + 两次 `WM_PAINT`（搜索框单次 paint 中位
        // 0.123ms），整轮 723 帧约 89ms；地址框因为平时 `SW_HIDE`，这两次调用
        // 全程 0 次重绘，纯浪费。于是每帧其实是 3 次重绘而不是 1 次。
        //
        // 现在的判据是"**决定那块像素的那几项状态的合集**变了才失效"
        // （`ChildEditKey`：提示词 / 墨色 / 缩放 / 图标主题 / 可见性）——
        // 与真实依赖一一对应，既不会漏（该画时一定画）也不会多（不变时一帧都
        // 不画）。子 EDIT 重画不会再失效父窗口，这里不会成环。
        //
        // `paint()` 拿的是 `&mut App`，所以快照就在这里就地更新 —— 不需要额外
        // 的一层可变状态，也不怕"判据读到的是上一帧的值"。
        invalidate_child_edits(app);
        ac::trace_sub_end(ac::SUB_INVALIDATE);
    }
    ac::trace_sub_begin(ac::SUB_SYNC_TIMER);
    // 每帧末尾统一校准动画定时器：本帧 paint 已经把各动画目标（`icon_zoom_tgt`
    // 等）按"当前悬停/选中"重写完了，这里看一眼"还有没有没跑完的"——
    // 比如刚取消选中的条目，放大目标被 `begin_anim_frame` 清成了 0、但 `icon_zoom`
    // 还停在 0.10，必须由定时器把它缓动回 0。只在该开/该关时才动定时器
    // （`sync_anim_timer` 内部有去重），所以无动画时空跑是零成本。
    app.sync_anim_timer();
    ac::trace_sub_end(ac::SUB_SYNC_TIMER);
}

/// 第70轮埋点：把"这一帧是什么帧"的判据一次性交给 `anim_clock`。
///
/// **只读，不改任何状态** —— 它调用的每个字段都是纯读取，`anim_active()`
/// 本身也只读字段。判据取值口径：
///
/// * `entries` 用 `tab().entries.len()`（筛选**前**），`visible` 用
///   `tab().visible.len()`（筛选**后**）。两个都记，因为"筛选把 5000 行压到
///   20 行"是一种优化，若只记一个会看不出是列表变短还是别的。
/// * `view_mode` 映射成 0..3，**顺序与 `zenfile_core::ViewMode` 的声明
///   顺序一致**（Details/List/MediumIcons/LargeIcons），分析脚本里要写死这个映射。
/// * `d_flags` 里每一位对应 `anim_clock` 里的一个 `CTX_*` 常量。
#[inline(always)]
fn trace_paint_context(app: &App, width: i32, height: i32, l: &Layout) {
    use crate::anim_clock as ac;
    // 关着 trace 时整段只花一次 relaxed 原子读 —— 所有 `&app` 字段访问
    // 都在这个判断之后，编译器会把它们一起优化掉。
    if !ac::trace_on() {
        return;
    }
    let tab = app.tab();
    let mode = match tab.view_mode {
        zenfile_core::ViewMode::Details => 0u32,
        zenfile_core::ViewMode::List => 1,
        zenfile_core::ViewMode::MediumIcons => 2,
        zenfile_core::ViewMode::LargeIcons => 3,
    };
    let mut flags = 0u32;
    if app.addr_anim.is_some() {
        flags |= ac::CTX_ADDR_ANIM;
    }
    if app.sb_scroll_target.is_some() {
        flags |= ac::CTX_SB_SCROLL;
    }
    if let (Some(c), Some(t)) = (app.sb_active_y, app.sb_active_target) {
        if (t - c).abs() > 0.5 {
            flags |= ac::CTX_SB_ACTIVE;
        }
    }
    if let (Some(c), Some(t)) = (app.sb_hover_y, app.sb_hover_target) {
        if (t - c).abs() > 0.5 {
            flags |= ac::CTX_SB_HOVER;
        }
    }
    if app.drag.as_ref().is_some_and(|d| d.active) {
        flags |= ac::CTX_DRAG;
    }
    if app.pane_div_drag.is_some() {
        flags |= ac::CTX_DIV_DRAG;
    }
    if app.cfg.ui.sidebar_visible {
        flags |= ac::CTX_SIDEBAR_VISIBLE;
    }
    if app.cfg.ui.statusbar_visible {
        flags |= ac::CTX_STATUS_VISIBLE;
    }
    if tab.home {
        flags |= ac::CTX_HOME;
    }
    if tab.recycle {
        flags |= ac::CTX_RECYCLE;
    }
    if tab.settings {
        flags |= ac::CTX_SETTINGS;
    }
    if tab.loading {
        flags |= ac::CTX_LOADING;
    }
    if tab.col_filter.is_some() {
        flags |= ac::CTX_COL_FILTER;
    }
    if app.panes.len() > 1 {
        flags |= ac::CTX_SPLIT;
    }
    if !app.icon_zoom_tgt.is_empty() {
        flags |= ac::CTX_ICON_ZOOM_TGT;
    }
    ac::trace_ctx(
        width,
        height,
        l.view,
        app.panes.len(),
        tab.entries.len(),
        tab.visible.len(),
        mode,
        app.icon_zoom.len(),
        app.icon_bounce.len(),
        app.cap_anim.len(),
        flags,
        app.anim_active(),
    );
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

/// 点 (x, y) 是否落在**两个窗格之间的分隔线**上。
///
/// 返回 `(是否竖线, 左/上窗格, 右/下窗格)`。不分栏（≤1 个窗格）恒 None。
///
/// 判据：两个窗格在该轴上**紧贴**（间距 ≤ 容差），且光标离那条公共边
/// ≤ 容差、落在两者在另一轴上的重叠区间内。窗格矩形是归一化的，先落到
/// 像素再比 —— 归一化坐标下"紧贴"会被 round 摆动干扰。
pub fn pane_div_at(app: &App, l: &Layout, x: i32, y: i32) -> Option<(bool, usize, usize)> {
    if app.panes.len() < 2 {
        return None;
    }
    let outer = l.view;
    let tol = (5.0 * app.metrics.scale).round() as i32;
    let rects: Vec<RECT> = app.panes.iter().map(|p| p.rect.to_px(&outer)).collect();
    for i in 0..rects.len() {
        for j in (i + 1)..rects.len() {
            let (a, b) = (rects[i], rects[j]);
            // 竖线：a 在左、b 在右，a.right ≈ b.left
            if (a.right - b.left).abs() <= tol && (x - a.right).abs() <= tol {
                let y0 = a.top.max(b.top);
                let y1 = a.bottom.min(b.bottom);
                if y >= y0 && y <= y1 {
                    return Some((true, i, j));
                }
            }
            // b 在左、a 在右
            if (b.right - a.left).abs() <= tol && (x - b.right).abs() <= tol {
                let y0 = a.top.max(b.top);
                let y1 = a.bottom.min(b.bottom);
                if y >= y0 && y <= y1 {
                    return Some((true, j, i));
                }
            }
            // 横线：a 在上、b 在下，a.bottom ≈ b.top
            if (a.bottom - b.top).abs() <= tol && (y - a.bottom).abs() <= tol {
                let x0 = a.left.max(b.left);
                let x1 = a.right.min(b.right);
                if x >= x0 && x <= x1 {
                    return Some((false, i, j));
                }
            }
            // b 在上、a 在下
            if (b.bottom - a.top).abs() <= tol && (y - b.bottom).abs() <= tol {
                let x0 = a.left.max(b.left);
                let x1 = a.right.min(b.right);
                if x >= x0 && x <= x1 {
                    return Some((false, j, i));
                }
            }
        }
    }
    None
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
        Some(d) if d.active => (
            d.kind,
            d.cur,
            d.items.len(),
            // 第60轮：钳进显示区 —— 框选框不许越出文件/文件夹区域。
            // l.view 已是当前窗格（分栏由 pane_layout 覆写过）。
            marquee_rect_in(d, l.view),
        ),
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

        // 1) 投放目标：描一圈 accent 边（只描边不填充，里面的内容还得看得见）。
        // **只属于拖文件**（第54轮）：拉框选时 target 恒为 None（WM_MOUSEMOVE
        // 那边已按种类过滤），这里再挡一道 —— 细节视图的行是全宽的，一旦给
        // 框选描出来，顶边就是一条横贯内容区的蓝线。
        let target_rect = if kind != crate::app::DragKind::Items {
            None
        } else {
            match target_kind {
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
            }
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

/// 框选矩形（把起点/当前点规整成左上-右下），并**钳进显示区 `bounds`**。
///
/// 钳制是用户明确要求的：框选框不许越出文件/文件夹显示区。早先只做
/// min/max，光标拖到状态栏甚至侧栏上，遮罩就跟着盖过去。
///
/// 绘制（`paint_drag_overlay`）与命中（`select_marquee_into`）都必须走这里，
/// 否则会出现"框看着停在区内、却选中了区外的条目"这类错位。
pub fn marquee_rect_in(drag: &crate::app::Drag, bounds: RECT) -> RECT {
    clamp_to(marquee_rect(drag), bounds)
}

/// 同上，但只吃两个 i32 坐标；让框选逻辑在持有 `&app::Drag` 借用的同时
/// 也能调（比如 `WM_MOUSEMOVE` 边拖边算选中时）。
pub fn marquee_rect_from_in(origin: (i32, i32), cur: (i32, i32), bounds: RECT) -> RECT {
    clamp_to(marquee_rect_from(origin, cur), bounds)
}

/// 把 `r` 收进 `b`（左上-右下规整后取交集）。`b` 退化时原样返回，
/// 免得出现负宽高把 GDI 画崩。
fn clamp_to(r: RECT, b: RECT) -> RECT {
    let left = r.left.max(b.left);
    let top = r.top.max(b.top);
    let right = r.right.min(b.right);
    let bottom = r.bottom.min(b.bottom);
    if right <= left || bottom <= top {
        return r;
    }
    RECT { left, top, right, bottom }
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

/// 同上，但只吃两个 i32 坐标。**未钳制**，仅供 `marquee_rect_from_in` /
/// `marquee_rect_in` 内部与单测使用；框选实际路径一律走 `_in` 版本。
fn marquee_rect_from(origin: (i32, i32), cur: (i32, i32)) -> RECT {
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
    if app.tab().view_mode == zenfile_core::ViewMode::Details {
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
        //
        // 第63轮：**先记动画，再画**。`note_cap_hover` 只在"这一帧刚变成悬停"
        // 时插一条记录（`or_insert` 保证同一键不会重置），而绘制端随后调
        // `cap_phase()` 读到它 —— 顺序反了就永远读到上一帧的相位（恒为 0），
        // 表现为"动画不播"。
        let toggle_on = app.hover_chrome & crate::app::HOVER_SIDEBAR_TOGGLE != 0;
        let menu_on = app.hover_chrome & crate::app::HOVER_MENU_BTN != 0;
        app.note_cap_hover(CAP_TOGGLE, toggle_on);
        app.note_cap_hover(CAP_MENU, menu_on);
        for w in [1u8, 2, 3] {
            app.note_cap_hover(w, app.hover_caption == w);
        }
        // 侧栏折叠开关（最左）与更多菜单（最小化左侧）
        draw_chrome_button(app, hdc, &l.sidebar_toggle, ChromeGlyph::PaneToggle, toggle_on);
        draw_chrome_button(app, hdc, &l.menu_btn, ChromeGlyph::MoreDots, menu_on);
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
    // 分栏（一个标签内多个窗口）时，顶部这个标签的文字 = 「A丨B丨C」
    // （用户要求：2 个以上同样）。每窗格单标签，故顶部条就这一个标签。
    let split_title = app.split_title();
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

        // 第63轮：标签 × 与 `+` 键的动画也在这里起手。放在绘制循环**之前**，
        // 否则标签 i 的 × 读到的是"上一帧"的相位（恒 0）。
        app.note_cap_hover(CAP_CLOSE_TAB, app.hover_tab_close.is_some());
        app.note_cap_hover(CAP_NEW_TAB, app.hover_new_tab);

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
                // 设置齿轮：主题接管时走自绘 Gear 字形
                //
                // 第66轮：齿轮**查规格**（Md），并在 16px 槽位里居中。
                // 之前直接用 `icon_s`(=16, shell 图标槽位) 画 —— 一个 UI 控件
                // 图标套着文件图标的尺寸，这就是"同一种风格另一种风格"的
                // 典型来源：两个数字都是 16，但它们属于**不同的尺寸体系**。
                let gs = crate::icon_spec::size_of(crate::icon_theme::Ui::Gear, app.metrics.scale);
                let ir = crate::icon_spec::centered(
                    &RECT { left: icon_x, top: icon_y, right: icon_x + icon_s, bottom: icon_y + icon_s },
                    gs,
                );
                if !(g == "\u{E713}"
                    && draw_ui_icon(hdc, crate::icon_theme::Ui::Gear, &ir, gs, fg)) {
                    let gf = glyph_font(app.metrics.scale);
                    let og = SelectObject(hdc, gf.into());
                    draw_text(hdc, g, ir, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
                    SelectObject(hdc, og);
                }
            }
            // 标题文字右侧要留出关闭按钮的位置，否则长标题会被 × 压住
            let mut tr = pad_rect_width(&r, 10);
            tr.left = icon_x + icon_s + (6.0 * app.metrics.scale).round() as i32;
            // 现在每个标签都有 ×，这块地方无条件留出来，不能按 hover 态变
            tr.right -= (18.0 * app.metrics.scale) as i32;
            // 分栏时唯一标签显示「A丨B丨C」，单窗格照旧显示自己的目录名
            let title = split_title
                .as_deref()
                .filter(|_| i == app.panes[pi].active)
                .unwrap_or(&tab.title);
            draw_text(hdc, title, tr, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
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
            //
            // 第63轮：底色块**只在鼠标真压在 × 上时**亮（之前跟着整个标签的
            // hover 一起亮，鼠标离 × 还差一个字宽它就亮了），并且 × 在悬停
            // 瞬间**放大一下**再归位。
            {
                let cr = l.tab_close_rect(i, n, &app.metrics);
                let on_close = app.hover_tab_close == Some(i);
                let cp = if on_close { app.cap_phase(CAP_CLOSE_TAB) } else { 0.0 };
                if on_close {
                    fill_radius(app, hdc, "title.button", &cr, t.hover_bg, None);
                }
                let cc = if i == app.panes[pi].active || hovered { t.text } else { t.text_dim };
                // 第66轮：× 与三大金刚键的 × **共用 Sm 档**（之前这里是 11，
                // 窗口键是 13 —— 两个"×"外形完全一样、大小却差 2px，
                // 一眼就能看出不是一套）。
                let base = crate::icon_spec::size_of(crate::icon_theme::Ui::Close, app.metrics.scale);
                let ics = ((base as f32) * (1.0 + 0.45 * cp)).round() as i32;
                if !draw_ui_icon(
                    hdc,
                    crate::icon_theme::Ui::Close,
                    &crate::icon_spec::centered(&cr, ics),
                    ics,
                    cc,
                ) {
                    SetTextColor(hdc, cc);
                    let g = glyph_font(app.metrics.scale);
                    let old_g = SelectObject(hdc, g.into());
                    draw_text(hdc, "\u{E8BB}", cr, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
                    SelectObject(hdc, old_g);
                }
            }
        }

        // 新建标签（+）：紧跟最后一个标签
        let plus = l.new_tab_rect(n, &app.metrics);
        if l.new_tab_visible && plus.right < l.tabs.right {
            // 第63轮：`+` 键原先恒传 `hover=false`，画出来是个**永远不响**的
            // 按钮。现在把真实悬停态接上，底色块与形变都跟其他键同一套。
            draw_chrome_button(app, hdc, &plus, ChromeGlyph::Plus, app.hover_new_tab);
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
    let strip = pane_strip_px(app.metrics.scale);
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
        // 贴边窗格顶部是**标题条**（只显示当前文件夹名），不再是多标签条
        //（用户要求：贴边窗口最多一个标签，不要里面还套一层 ×/+）。
        //
        // `i == saved` 判焦点 —— 不能在 paint_pane_title 里拿 `app.focus_pane`
        // 比，因为这个循环逐窗格把 focus_pane 设成 i，比出来恒真（两个窗格
        // 都画出强调下划线 = 又一圈蓝边）。
        paint_pane_title(app, hdc, &l2, i, i == saved);
        paint_view(app, hdc, &l2);
        // 窗格之间的缝：只有"右边和下边"需要画，最外圈本来就是窗口边界。
        //
        // 颜色恒为中性分隔色 —— 早先焦点窗格用强调色（蓝）描边，用户明确
        // 要求去掉（"这个窗口会有蓝边"）。焦点改由标题条底部的强调下划线
        // 与标签高亮表达，不再给整圈边框上色。
        //
        // 但**正在拖的那条分隔线**（或正悬停的）仍给强调色：那是操作反馈，
        // 鼠标一松就消失，不是常驻边框。
        unsafe {
            let line = CreateSolidBrush(t.surface_alt);
            let edge = RECT { left: pr.left, top: pr.bottom - 2, right: pr.right, bottom: pr.bottom };
            FillRect(hdc, &edge, line);
            let edge2 = RECT { left: pr.right - 2, top: pr.top, right: pr.right, bottom: pr.bottom };
            FillRect(hdc, &edge2, line);
            let _ = DeleteObject(line.into());
        }
    }
    app.focus_pane = saved;
    // 高亮正在拖 / 悬停的那条窗格分隔线（覆盖在中性缝之上）
    let hot = app
        .pane_div_drag
        .as_ref()
        .map(|d| Some((d.vertical, d.a, d.b)))
        .unwrap_or(app.hover_pane_div);
    if let Some((vertical, a, b)) = hot {
        if a < n && b < n {
            let ra = app.panes[a].rect.to_px(&outer);
            let rb = app.panes[b].rect.to_px(&outer);
            let accent = app.theme.accent;
            unsafe {
                let br = CreateSolidBrush(accent);
                if vertical {
                    let x = ra.right;
                    let y0 = ra.top.max(rb.top);
                    let y1 = ra.bottom.min(rb.bottom);
                    if y1 > y0 {
                        let r = RECT { left: x - 1, top: y0, right: x + 1, bottom: y1 };
                        FillRect(hdc, &r, br);
                    }
                } else {
                    let y = ra.bottom;
                    let x0 = ra.left.max(rb.left);
                    let x1 = ra.right.min(rb.right);
                    if x1 > x0 {
                        let r = RECT { left: x0, top: y - 1, right: x1, bottom: y + 1 };
                        FillRect(hdc, &r, br);
                    }
                }
                let _ = DeleteObject(br.into());
            }
        }
    }
}

/// 贴边窗格顶部的**标题条**：只画当前文件夹的图标 + 名字。
///
/// 不再画多标签（×/+）——用户要求贴边窗口最多一个标签，顶部显示文件夹名。
/// 焦点态用标题条底部一条强调下划线表达（替代原先整圈蓝边）。
/// `focused` 由调用方传入：循环里 focus_pane 被逐窗格改写，这里比不准。
fn paint_pane_title(app: &mut App, hdc: HDC, l: &Layout, pi: usize, focused: bool) {
    let t = app.theme;
    let Some(tab) = app.panes.get(pi).and_then(|p| p.tabs.get(p.active)) else {
        return;
    };
    let title = tab.title.clone();
    let home = tab.home;
    let recycle = tab.recycle;
    let settings = tab.settings;
    let path = tab.path.clone();
    let r = l.title;
    unsafe {
        // 底色与标签条一致（chrome_bg），让标题条看起来像"一个恒定的标签"
        let bg = CreateSolidBrush(t.chrome_bg);
        FillRect(hdc, &r, bg);
        let _ = DeleteObject(bg.into());

        let font = ui_font(app.metrics.scale);
        let old = SelectObject(hdc, font.into());
        SetBkMode(hdc, TRANSPARENT);

        // 图标 16px 压在左边
        let icon_s = (16.0 * app.metrics.scale).round() as i32;
        let icon_x = r.left + (10.0 * app.metrics.scale).round() as i32;
        let icon_y = (r.top + r.bottom - icon_s) / 2;
        let mut drew_icon = false;
        if home {
            if let Some(h) = crate::icons::stock_icon(
                windows::Win32::UI::Shell::SIID_DESKTOPPC.0,
                false,
            ) {
                app.icons.draw(hdc, h, icon_x, icon_y, icon_s);
                drew_icon = true;
            }
        } else if recycle {
            if let Some(h) = crate::icons::stock_icon(
                if app.recycle_items.is_empty() {
                    windows::Win32::UI::Shell::SIID_RECYCLER.0
                } else {
                    windows::Win32::UI::Shell::SIID_RECYCLERFULL.0
                },
                false,
            ) {
                app.icons.draw(hdc, h, icon_x, icon_y, icon_s);
                drew_icon = true;
            }
        } else if settings {
            // 第66轮：同上面的齿轮 —— 查规格(Md)并在槽位里居中，不再用
            // shell 图标的 `icon_s`(16)。
            let gs = crate::icon_spec::size_of(crate::icon_theme::Ui::Gear, app.metrics.scale);
            let ir = crate::icon_spec::centered(
                &RECT { left: icon_x, top: icon_y, right: icon_x + icon_s, bottom: icon_y + icon_s },
                gs,
            );
            if !draw_ui_icon(hdc, crate::icon_theme::Ui::Gear, &ir, gs, t.text) {
                let gf = glyph_font(app.metrics.scale);
                let og = SelectObject(hdc, gf.into());
                draw_text(hdc, "\u{E713}", ir, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
                SelectObject(hdc, og);
            }
            drew_icon = true;
        } else if !path.as_os_str().is_empty() {
            let h = app.icons.dir_icon(&path, false);
            if !h.0.is_null() {
                app.icons.draw(hdc, h, icon_x, icon_y, icon_s);
                drew_icon = true;
            }
        }
        let _ = drew_icon;

        // 文字：跟着图标右边，右侧留白（没有 × 了，不用留关闭位）
        let mut tr = r;
        tr.left = icon_x + icon_s + (7.0 * app.metrics.scale).round() as i32;
        tr.right -= (12.0 * app.metrics.scale).round() as i32;
        SetTextColor(hdc, t.text);
        draw_text(
            hdc,
            &title,
            tr,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );

        // 焦点下划线：只有焦点窗格画，替代原先的整圈强调边框
        if focused {
            let accent = CreateSolidBrush(t.accent);
            let line = RECT { left: r.left, top: r.bottom - 2, right: r.right, bottom: r.bottom };
            FillRect(hdc, &line, accent);
            let _ = DeleteObject(accent.into());
        }
        SelectObject(hdc, old);
    }
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

/// 主题接管时画一枚 UI 字形（居中于 `r`，边长 `size`），返回是否画了。
///
/// 返回 false = 主题没接管（直角黑白默认）或渲染失败 —— 调用方继续走
/// 原来的字体字形 / GDI 手绘路径，零回归。
/// 画一枚 UI 字形。返回 false = 渲染失败，调用方走各自的降级路径。
///
/// # 第67轮：自绘字形是**唯一实现**，false 只在异常时出现
///
/// 之前 `default` 主题下 `ui_icon` 必返 None，于是每个调用点都准备了一套
/// GDI 手绘字形 —— **同一个软件两套字形**（详见 `icon_theme::ui_icon` 的
/// 对照表）。现在自绘对所有主题生效，`false` 只剩"HICON 创建失败"这一种
/// 异常情况。下面的降级代码因此变成"保险"而不是"日常路径"，但仍然保留：
/// 渲染失败时**什么都不画**比画一个错的形状更糟。
pub fn draw_ui_icon(hdc: HDC, kind: crate::icon_theme::Ui, r: &RECT, size: i32, color: COLORREF) -> bool {
    let Some(h) = crate::icon_theme::ui_icon(kind, size, color) else {
        return false;
    };
    let x = (r.left + r.right - size) / 2;
    let y = (r.top + r.bottom - size) / 2;
    unsafe {
        let _ = DrawIconEx(hdc, x, y, h, size, size, 0, None, DI_NORMAL);
    }
    true
}

/// 标题栏自有键的字形。`phase` 是 `App::cap_phase(which)`（0=静止，1=峰值）。
///
/// 第63轮：三个键各有各的"内部动法"，都是**几何形变而不是换图**：
/// - 侧栏开关：左边那栏实心面板宽度随 phase 收窄（像真的在折叠）
/// - 更多菜单：三条杠逐条**左右错开**（上/中/下往不同方向推）
/// - 新建标签：整条十字**转 45°**，摆出 X 的姿态再转回来
///
/// 形变都作用在字形自身的绘制参数上，所以主题接管（`draw_ui_icon`）时
/// 走的是同一条路，不会有两套动画。
///
/// 第67轮：`draw_ui_icon` 对所有主题都生效（之前 `default` 主题恒 false，
/// 落进下面的 GDI 手绘），所以这段 fallback 现在只在渲染失败时触发。
fn draw_chrome_glyph(app: &App, hdc: HDC, r: &RECT, which: ChromeGlyph, phase: f32) {
    let s = app.metrics.scale;
    // (边长系数, 自转角度)。取自上表。
    let (k, spin) = match which {
        ChromeGlyph::PaneToggle => (1.0 + 0.10 * phase, 0.0),
        ChromeGlyph::MoreDots => (1.0 + 0.14 * phase, 0.0),
        ChromeGlyph::Plus => (1.0 + 0.30 * phase, 45.0 * phase),
    };
    // 第66轮：**尺寸不再各写各的**，一律查 `icon_spec::spec`。
    // 之前这里是 `PaneToggle 17 / Hamburger 15 / Plus 13` 三个裸数字，
    // 与 `draw_caption_glyph` 里的 13、标签 × 的 11 各不相干。
    let kind = match which {
        ChromeGlyph::PaneToggle => crate::icon_theme::Ui::PaneToggle,
        ChromeGlyph::MoreDots => crate::icon_theme::Ui::Hamburger,
        ChromeGlyph::Plus => crate::icon_theme::Ui::Plus,
    };
    let sz = (crate::icon_spec::size_of(kind, s) as f32 * k).round() as i32;
    let cx = (r.left + r.right) / 2;
    let cy = (r.top + r.bottom) / 2;
    let gr = RECT { left: cx - sz / 2, top: cy - sz / 2, right: cx - sz / 2 + sz, bottom: cy - sz / 2 + sz };
    // 主题接管时**一律走主题图标**（只看尺寸变化，不看 spin）。
    //
    // 别写成 `if spin.abs() <= 0.01 && draw_ui_icon(...)`：那样 `+` 在动画
    // 期间会从主题圆角图标**切回 GDI 直角手绘**，动画结束又切回去 ——
    // 看起来是"字形先变成另一个东西再变回来"，比不旋转难看得多。
    // 主题图标没法绕中心旋转（要走贴图变换），所以主题下 `+` 只有缩放，
    // 旋转是直角黑白主题才有的额外一档。
    if draw_ui_icon(hdc, kind, &gr, sz, app.theme.text) {
        return;
    }
    unsafe {
        // 笔画粗细也查规格（统一光学粗细），不再各处写 `(1.0 * s)`。
        let pen = CreatePen(PS_SOLID, crate::icon_spec::stroke_px(sz, s), app.theme.text);
        let old_pen = SelectObject(hdc, pen.into());
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        match which {
            ChromeGlyph::PaneToggle => {
                let w = ((14.0 * s).round() as f32 * k).round() as i32;
                let h = ((11.0 * s).round() as f32 * k).round() as i32;
                let left = cx - w / 2;
                let top = cy - h / 2;
                // 外框
                let _ = Rectangle(hdc, left, top, left + w, top + h);
                // 左侧填实的一栏（宽 5px，phase 越大越窄）：表示"被收起的那一栏"
                // —— 悬停时这栏往里收，像真的在折叠。
                let panel = (((5.0 * s).round() as f32) * (1.0 - 0.7 * phase)).round() as i32;
                if panel > 0 {
                    let b = CreateSolidBrush(app.theme.text);
                    FillRect(hdc, &RECT { left: left + 1, top: top + 1, right: left + 1 + panel, bottom: top + h - 1 }, b);
                    let _ = DeleteObject(b.into());
                }
            }
            ChromeGlyph::MoreDots => {
                // **三条杠**（第33轮：用户要求把"更多"菜单从三个点改成
                // hamburger —— 三个点看着像"还有更多内容"，而这个键的实际
                // 语义是"主菜单"，三条杠才是那个意思）。
                //
                // 尺寸与三个窗口键（最小化/最大化/关闭）**同一套口径**：
                // 宽 11px、高 11px、线宽 1px，所以它跟旁边那排键看上去
                // 是同一族，不会一个胖一个瘦。
                //
                // 第63轮：上/中/下三条随 phase **逐条左右错开**（-1/0/+1），
                // 一眼能看出"这排东西在动"，而不是整块一起抖。
                let w = ((11.0 * s).round() as f32 * k).round() as i32;
                let th = (1.0 * s).round().max(1.0) as i32;
                // 三条线的 y 间距：4px（11px 的框里放 3 条，视觉重心居中）
                let step = (4.0 * s).round().max(3.0) as i32;
                let b = CreateSolidBrush(app.theme.text);
                for (kk, yk) in [(-1, -1), (0, 0), (1, 1)] {
                    let y = cy + yk * step - th / 2;
                    let dx = (kk as f32 * (2.0 * s) * phase).round() as i32;
                    let _ = FillRect(hdc, &RECT { left: cx - w / 2 + dx, top: y, right: cx - w / 2 + dx + w, bottom: y + th }, b);
                }
                let _ = DeleteObject(b.into());
            }
            ChromeGlyph::Plus => {
                // 十字用两个**实心细条**画，不用两条 1px 线：GDI 的线在
                // 非整数位置会落半格，交点处还会缺一个像素，看着就是"歪的 +"。
                //
                // 第63轮：悬停时整条十字转 45°。用 **LineTo** 而不是实心条 ——
                // 实心条没法转，只有斜线能摆出对角姿态。
                let arm = ((9.0 * s).round() as f32 * k).round() as i32;
                let th = (1.0 * s).round().max(1.0) as i32;
                let b = CreateSolidBrush(app.theme.text);
                if spin.abs() <= 0.01 {
                    FillRect(hdc, &RECT { left: cx - arm / 2, top: cy - th / 2, right: cx + arm / 2 + arm % 2, bottom: cy - th / 2 + th }, b);
                    FillRect(hdc, &RECT { left: cx - th / 2, top: cy - arm / 2, right: cx - th / 2 + th, bottom: cy + arm / 2 + arm % 2 }, b);
                    let _ = DeleteObject(b.into());
                } else {
                    // 转 45°：两条对角线。实心条没法转，只有斜线能摆出对角姿态 ——
                    // 笔画略细，但在这个尺寸上"在转"这件事比粗细更显眼。
                    let _ = DeleteObject(b.into());
                    let p2 = CreatePen(PS_SOLID, th.max(1), app.theme.text);
                    let oldp2 = SelectObject(hdc, p2.into());
                    let diag = ((arm as f32) * std::f32::consts::FRAC_1_SQRT_2).round() as i32;
                    MoveToEx(hdc, cx - diag, cy - diag, None);
                    LineTo(hdc, cx + diag, cy + diag);
                    MoveToEx(hdc, cx + diag, cy - diag, None);
                    LineTo(hdc, cx - diag, cy + diag);
                    SelectObject(hdc, oldp2);
                    let _ = DeleteObject(p2.into());
                }
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
///
/// 第63轮：`phase` 是 `App::cap_phase(which)`，0=静止、1=弹到最大。
/// **形变方向按键的语义给**，这样"弹一下"本身就有信息：
///
/// | 键 | 形变 | 为什么 |
/// |---|---|---|
/// | 最大化 | 放大 1.35x | "往上顶"，直觉上是变大 |
/// | 最小化 | 缩到 0.82x + 下移 | "沉下去"，与缩到窗口语义一致 |
/// | 关闭 | 缩到 0.7x | "收起来"，越收越小的消失感 |
///
/// 三者都用**同一个 `sin(πt)` 包络**（`cap_phase` 里算好），所以播放一次就
/// 归位，不留残余偏移。
fn draw_caption_glyph(app: &App, hdc: HDC, r: &RECT, which: u8, phase: f32) {
    let s = app.metrics.scale;
    // (缩放系数, 纵向位移)。取自上表。
    let (k, dy) = match which {
        1 => (1.0 - 0.18 * phase, (2.0 * s).round() as i32 * phase as i32),
        2 => (1.0 + 0.35 * phase, 0),
        _ => (1.0 - 0.30 * phase, 0),
    };
    // 符号框：按 k 在命中区里缩放并按 dy 竖移。
    //
    // 第66轮：base 查 `icon_spec::spec` 而不是写死 13.0 —— 窗口键、
    // 新建标签 +、标签关闭 × 现在共用 Sm 这一档，规格只有一处定义。
    let (cx0, cy0) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
    // 主题接管时走自绘字形；还原态（两个错开方框）用专门的 Restore 字形。
    let kind = match which {
        1 => Some(crate::icon_theme::Ui::Min),
        2 => {
            if unsafe { IsZoomed(app.hwnd) }.0 != 0 {
                Some(crate::icon_theme::Ui::Restore)
            } else {
                Some(crate::icon_theme::Ui::Max)
            }
        }
        _ => Some(crate::icon_theme::Ui::Close),
    };
    let base = kind
        .map(|k| crate::icon_spec::size_of(k, s))
        .unwrap_or_else(|| crate::icon_spec::Size::Sm.scaled(s));
    let sz = ((base as f32 * k).round() as i32).max(1);
    let half_sz = sz / 2;
    let gr = RECT {
        left: cx0 - half_sz,
        top: cy0 + dy - half_sz,
        right: cx0 - half_sz + sz,
        bottom: cy0 + dy - half_sz + sz,
    };
    if let Some(k) = kind {
        if draw_ui_icon(hdc, k, &gr, sz, app.theme.text) {
            return;
        }
    }
    let cx = (gr.left + gr.right) / 2;
    let cy = (gr.top + gr.bottom) / 2;
    // GDI 降级路径：符号本体按**边长的 0.42** 算，笔画查规格 —— 与主题路径
    // 光学一致。之前的 `5.5 * s` 是又一个独立数字：13px 档下 5.5 ≈ 0.42×13，
    // 但换档后它不会跟着变。
    let half = (((sz as f32 * 0.42).round() as i32) as f32 * k).round() as i32;
    let half = half.max(2);
    let box_w = half * 2;
    let th = crate::icon_spec::stroke_px(sz, s);
    unsafe {
        // 横竖笔画一律用**实心细条**画，不用 MoveTo/LineTo：GDI 的线端点不补齐，
        // 转角处会缺一个像素，10px 的尺寸下看着就是"歪的/断的"。
        let brush = CreateSolidBrush(app.theme.text);
        let bar = |left: i32, top: i32, right: i32, bottom: i32| {
            FillRect(hdc, &RECT { left, top, right, bottom }, brush);
        };
        match which {
            1 => {
                // 最小化：一条横线。**y 位置刻意压在几何中心之下 2px** ——
                // 贴到符号框底边（原来的 `cy + half`）看着就是"掉在地上"，
                // 用户原话"最小化不要贴地上"。资源管理器的减号也是这个位置：
                // 符号框高 11px，横线在第 8~9 行，上下留白 3/2。
                let th = (1.0 * s).round().max(1.0) as i32;
                let y = cy + (half - 2 * s.round() as i32).max(1);
                bar(cx - half, y, cx - half + box_w, y + th);
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

/// 标题栏按键的**悬停底色块**：一个固定的圆角小方块，不再铺满整条标题栏。
///
/// 第62轮用户报"悬停上去效果是全部占满标题栏"。改之前每个键的命中区就是
/// 整条 `44×title_h` 的竖条，`FillRect` 铺满整列 —— 悬停时一整块 44×34
/// 全变色，视觉上像标题栏被劈掉一条。
///
/// 现在把"命中区"和"底色块"分开：**命中区仍按原来的竖条算**（点哪儿都算点
/// 到这个键上，不能因为底色变小了就把边角处的点击丢掉），但底色块改成按
/// `CAPTION_HOVER_GAP` 收到固定尺寸后在命中区里居中的圆角矩形，资源管理器
/// 就是这个观感。
///
/// 尺寸取 `title_h - 10`（上下各留 5px）—— 标题栏才 34px，留 10px 刚好
/// 是一个"浮在标题栏上的小块"，不顶到边框。
const CAPTION_HOVER_GAP: i32 = 5;

/// 标题栏按钮（最小化/最大化/关闭）。`which`：1=最小化，2=最大化，3=关闭。
///
/// **底色走 `fill_radius` + `title.button`**，和侧栏折叠开关、更多菜单键
/// 同一批绘制函数 —— 那两个键第33轮就已经是"悬停浮出圆角块"了，只有三大
/// 金刚键还停在铺满标题栏的老写法上，现在三者终于一致。颜色一个没动。
///
/// 第63轮：字形按 `cap_phase(which)` 做**各不相同的形变**，弹一下再归位：
/// - 最大化：放大一点（"顶上去"）
/// - 最小化：缩小 + 下移（"沉下去"）
/// - 关闭：缩小一点（"收起来"）
///
/// 形变作用于**字形**，底色块不参与 —— 底色是"这个键被选中了"的静态提示，
/// 让它跟着一起抖会像按钮在抽搐。
fn draw_caption_button(app: &App, hdc: HDC, r: &RECT, which: u8) {
    let t = &app.theme;
    unsafe {
        let hovered = app.hover_caption == which;
        let pressed = app.pressed_caption == which;
        if hovered || pressed {
            let bg = if which == 3 && hovered {
                t.danger
            } else if pressed {
                t.surface
            } else {
                t.hover_bg
            };
            fill_radius(app, hdc, "title.button", &caption_hover_rect(r), bg, None);
        }
        // 字形仍按**整条命中区**居中：视觉上仍是标题栏正中那个符号，
        // 只有底色块缩小了，不该顺手把符号也挪走。
        draw_caption_glyph(app, hdc, r, which, app.cap_phase(which));
    }
}

/// 底色块在命中区里居中的矩形，高度收到 `title_h - 2*CAPTION_HOVER_GAP`。
///
/// **宽高都钳成同一个值**（正方形）—— 资源管理器的关闭键底色就是正方块，
/// 压扁的长条看着像"一块贴在边上的补丁"。
fn caption_hover_rect(r: &RECT) -> RECT {
    let g = CAPTION_HOVER_GAP;
    let h = (r.bottom - r.top - g * 2).max(1);
    let cy = (r.top + r.bottom) / 2;
    let left = (r.left + r.right) / 2;
    let x = left - h / 2;
    RECT { left: x, top: cy - h / 2, right: x + h, bottom: cy - h / 2 + h }
}

/// 标题栏自有按钮（侧栏开关 / 更多菜单 / 新建标签）：带悬停底色与形变动画。
///
/// **「更多」这个键是主菜单入口**，第33轮用户要求"永远自己就带一层白色背景"，
/// 但第62轮那条要求把"背景"重新定义了一遍：**不是铺满整条标题栏的竖条，
/// 而是一个固定大小的圆角块**。原来这层底色直接铺满 44×34 的整条命中区，
/// 悬停时（而更多键是常驻的）等于标题栏上永远糊着一块 44 宽的色条。
/// 现在底色块和三大金刚键走**同一个** `caption_hover_rect`，四个键彻底一致。
///
/// 第63轮：`which` 用来取 `cap_phase` 的形变相位，也决定"内部细节"怎么动：
/// - 侧栏开关：左侧那栏面板**横向收放**（像真的在折叠）
/// - 更多菜单：三条杠**左右错开**一下（像在呼吸）
/// - 新建标签：十字**转 45°**变成对角（第一次见"加号变叉"的动感）
///
/// 侧栏折叠开关保持"平时透明、hover 才亮"（它真的是那种次要控件），
/// 但亮起来的形状同样跟着改小。
fn draw_chrome_button(app: &App, hdc: HDC, r: &RECT, glyph: ChromeGlyph, hover: bool) {
    let t = &app.theme;
    unsafe {
        // 两个自有按键（菜单键 / 侧栏折叠键）平时**一个像素都不画**，
        // 只在鼠标悬停时才浮出圆角块。
        //
        // 注意：**不能靠"填成 chrome_bg"来当透明**。暗色主题里标题栏底色是
        // `title_bg` = #000000，而 `chrome_bg` = #191919 —— 差 25 级，
        // 方块照样在标题栏上显形（用户截图里那个深灰块就是这么来的，
        // 第36轮只把 `t.surface` 换回 `t.chrome_bg`，等于没去掉）。
        // 真正的透明就是**不调 FillRect**，只画图标。
        if hover {
            // 第62轮：底色块收成固定大小，不再占满整条标题栏。
            fill_radius(app, hdc, "title.button", &caption_hover_rect(r), t.hover_bg, None);
        }
        let which = match glyph {
            ChromeGlyph::PaneToggle => CAP_TOGGLE,
            ChromeGlyph::MoreDots => CAP_MENU,
            ChromeGlyph::Plus => CAP_NEW_TAB,
        };
        draw_chrome_glyph(app, hdc, r, glyph, app.cap_phase(which));
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
    // band 放宽：空心时条可能比 bar_w 宽（stroke×2），留足余量给 Region 裁
    let band_w = (bar_w + 24).min(host.right - host.left);
    let band = CreateRectRgn(host.left, bar_top, host.left + band_w, bar_top + bar_h);
    let _ = CombineRgn(Some(clip), Some(clip), Some(band), RGN_AND);
    let _ = DeleteObject(band.into());

    let saved = SaveDC(hdc);
    let _ = SelectObject(hdc, clip.into());
    if sb.hollow {
        // 空心：只描边不填色。
        //
        // **描边粗细必须真的可调**（第35轮）：原来这里 `.min(bar_w)` 把线宽
        // 夹到条宽以内，而空心条的宽度本来就 = w（3px），于是 stroke 无论
        // 调 1 还是 6 都被夹成 3 —— 参数在真窗口上完全无效，是个空壳。
        //
        // 正确做法：空心时**条的外框按 stroke 撑开**（w 视为最小宽度），
        // 这样 stroke 就是真的可见边厚；超出宿主底色的部分交给 Region 裁剪，
        // 不会溢出（Region 已经在上面裁好了）。
        let stroke = (((sb.stroke as f32) * sc).round().max(1.0) as i32)
            .min(bar_h)
            .max(1);
        // 空心条的可见宽度 = max(w, stroke*2)（左右各 stroke）
        let hollow_w = (stroke * 2).max(bar_w);
        let p = CreatePen(PS_SOLID, stroke, t.accent);
        let op = SelectObject(hdc, p.into());
        let ob = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        // GDI Rectangle 画的是 [left, right) —— +1 让可见宽正好 bar_w，
        // 与实心版同一口径。
        let _ = Rectangle(
            hdc,
            host.left,
            bar_top,
            host.left + hollow_w + 1,
            bar_top + bar_h + 1,
        );
        let _ = SelectObject(hdc, ob);
        let _ = SelectObject(hdc, op);
        let _ = DeleteObject(p.into());
    } else {
        // 实心：只填**竖条本身**（bar_w 宽）。
        //
        // 不能直接 FillRgn(clip)：clip = 圆角宿主 ∩ band，而 band 是给
        // 空心描边留的余量（bar_w+24，第35轮放宽的）—— 实心跟着一起填，
        // w=1 的竖条会画成 25px 宽的一块蓝砖；半径一大 band 被压成 1px 高，
        // 又变成一条横线（radius35 回归第39轮抓到，肉眼即错）。
        let bar = CreateRectRgn(host.left, bar_top, host.left + bar_w, bar_top + bar_h);
        let _ = CombineRgn(Some(clip), Some(clip), Some(bar), RGN_AND);
        let _ = DeleteObject(bar.into());
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
    let crumbs = app.address_crumbs();
    layout_crumbs(app, l, crumbs)
}

/// 把**任意一串**面包屑排进地址框（第57轮：退出动画要排的是切换前那一串）。
fn layout_crumbs(app: &mut App, l: &Layout, mut crumbs: Vec<crate::app::Crumb>) -> AddressBar {
    let scale = app.metrics.scale;
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
                chip: false,
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

/// 把一个矩形整体平移 `dx`（地址栏进出场动画用）。
fn shift_x(r: RECT, dx: i32) -> RECT {
    if dx == 0 {
        r
    } else {
        RECT { left: r.left + dx, top: r.top, right: r.right + dx, bottom: r.bottom }
    }
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

/// 把矩形上下各收进 `v` 像素（芯片比面包屑条矮一截，学 Pulse 的内边距）。
fn inset_v(r: &RECT, v: i32) -> RECT {
    RECT { left: r.left, top: r.top + v, right: r.right, bottom: (r.bottom - v).max(r.top + 1) }
}

/// 搜索芯片底色：强调色按 `f` 混进 surface（GDI COLORREF 是 0x00BBGGRR）。
///
/// Pulse 实测暗色芯片 #263846 ≈ 强调蓝与面板底 18% 混合；亮色主题下同一个
/// 公式混出淡蓝块，配 `selection_text` 深字，正好是系统选中态的观感。
fn chip_color(t: &crate::theme::Theme, f: f32) -> COLORREF {
    let mix = |a: u32, b: u32| -> u32 {
        (a as f32 * (1.0 - f) + b as f32 * f).round() as u32
    };
    let (ar, ag, ab) = (t.accent.0 & 0xFF, (t.accent.0 >> 8) & 0xFF, (t.accent.0 >> 16) & 0xFF);
    let (br, bg, bb) = (t.surface.0 & 0xFF, (t.surface.0 >> 8) & 0xFF, (t.surface.0 >> 16) & 0xFF);
    COLORREF((mix(ab, bb) << 16) | (mix(ag, bg) << 8) | mix(ar, br))
}

/// 画地址栏：平时是面包屑，编辑态时让 EDIT 自己画（它此时才显示出来）。
fn paint_address(app: &mut App, hdc: HDC, l: &Layout) {
    let t = app.theme;
    let scale = app.metrics.scale;
    unsafe {
        // 底 + 1px 边框（EDIT 与工具栏同色，没有框就等于看不见）
        fill_radius(app, hdc, "address.edit", &l.address, t.surface, Some(t.border));
        if app.addr_editing {
            return;
        }

        // 第57轮：地址栏进 / 出场动画。
        //   进入（kind=0）：**末尾那一级**从地址框右沿滑进来，到位后回弹一下；
        //   退出（kind=1）：**切换前**那一串的末尾一级向右滑出去（栈式弹出）。
        //
        // 第59轮调整：**只有最后一级动，前面的父级与分隔符全部原地不动**。
        // 早先整串一起平移，父级也跟着滑，看着像"整条地址都在动"，反而脏；
        // 而地址栏真正表达的是"我又深了一层"，动的只该是新增的那一级。
        // 位移量仍按地址框宽度算，末节从右沿外滑入 —— 父级不动的前提下，
        // 这样末节入场的路径最自然（它自己那段距离被完整走完）。
        //
        // 动画只改末节的 x，命中矩形（address_bar 那份）不变 —— 过渡期间
        // 没人会去点面包屑，真点也点的是动画结束后的位置。
        let anim = app.addr_anim.clone();
        let bar = match &anim {
            Some(a) if a.kind == 1 => layout_crumbs(app, l, a.prev.clone()),
            _ => address_bar(app, l),
        };
        let mut dx = 0i32;
        if let Some(a) = &anim {
            let now = GetTickCount64();
            let w = (l.address.right - l.address.left).max(1) as f32;
            let (dur, p) = if a.kind == 0 {
                (crate::app::ANIM_ADDR_IN_MS, (now.saturating_sub(a.start)) as f32 / crate::app::ANIM_ADDR_IN_MS as f32)
            } else {
                (crate::app::ANIM_ADDR_OUT_MS, (now.saturating_sub(a.start)) as f32 / crate::app::ANIM_ADDR_OUT_MS as f32)
            };
            let _ = dur;
            let p = p.clamp(0.0, 1.0);
            dx = if a.kind == 0 {
                // 先 3 次 ease-out 滑到位，后 38% 走一段抛物线"弹一下"
                let e = 1.0 - (1.0 - p).powi(3);
                let mut d = (1.0 - e) * w;
                if p > 0.62 {
                    let q = (p - 0.62) / 0.38;
                    d -= 7.0 * scale * (1.0 - (2.0 * q - 1.0).powi(2));
                }
                d as i32
            } else {
                // 退出：加速向右滑出（越滑越快，像被推出栈）
                (p * p * w) as i32
            };
        }
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
        let n_crumbs = bar.crumbs.len();
        for (i, c) in bar.crumbs.iter().enumerate() {
            let last = i + 1 == n_crumbs;
            // 只有末级吃位移，父级 dx=0（shift_x 对 0 是恒等，这里直接传值即可）
            let r = match bar.rects.get(i) {
                Some(r) => shift_x(*r, if last { dx } else { 0 }),
                None => continue,
            };
            if c.chip {
                // 搜索状态芯片（学 Pulse）：强调色淡染的圆角块 + 白字。
                // Pulse 实测底色 #263846 = 强调蓝与面板底 ~18% 混合，这里
                // 同配方，亮色主题下自然变成淡蓝块 + 深字（selection_text
                // 两套主题分别配好了）。
                let base = if app.addr_hover == Some(i) { 0.30 } else { 0.18 };
                let vin = (2.0 * scale).round() as i32;
                fill_radius(app, hdc, "address.crumb", &inset_v(&r, vin), chip_color(&t, base), None);
                SetTextColor(hdc, t.selection_text);
            } else {
                if app.addr_hover == Some(i) {
                    fill_radius(app, hdc, "address.crumb", &r, t.hover_bg, None);
                }
                SetTextColor(hdc, if last { t.text } else { t.text_dim });
            }
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
            // 分隔符不参与位移（父级静止），直接用原矩形的一份拷贝 ——
            // DrawTextW 要 &mut RECT，而 seps 是共享借用，只能克隆。
            let mut sr = *s;
            DrawTextW(
                hdc,
                &mut w,
                &mut sr,
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
        // 第66轮：导航键尺寸查规格（Xl），不再写死 `20.0 * scale`。
        // 三个键共用 `nav_sz`，且 `icon_spec` 保证 Back/Forward/Up 同档 ——
        // 之前它们同用 20 没错，但**没人保证下一个新增的导航键也会跟上**。
        let nav_sz = crate::icon_spec::size_of(crate::icon_theme::Ui::Back, app.metrics.scale);
        let c_back = if app.can_back() { t.text } else { t.text_dim };
        if !draw_ui_icon(hdc, crate::icon_theme::Ui::Back, &l.back, nav_sz, c_back) {
            SetTextColor(hdc, c_back);
            draw_text(hdc, "\u{E72B}", l.back, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_CENTER);
        }
        let c_fwd = if app.can_forward() { t.text } else { t.text_dim };
        if !draw_ui_icon(hdc, crate::icon_theme::Ui::Forward, &l.forward, nav_sz, c_fwd) {
            SetTextColor(hdc, c_fwd);
            draw_text(hdc, "\u{E72A}", l.forward, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_CENTER);
        }
        if !draw_ui_icon(hdc, crate::icon_theme::Ui::Up, &l.up, nav_sz, t.text) {
            SetTextColor(hdc, t.text);
            draw_text(hdc, "\u{E74A}", l.up, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_CENTER);
        }
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
        let gr = RECT {
            left: r.left - (2.0 * s) as i32,
            top: r.top - (3.0 * s) as i32,
            right: r.right - (1.0 * s) as i32,
            bottom: r.bottom - (3.0 * s) as i32,
        };
        let main_col = if enabled { t.text } else { t.text_dim };
        // 第66轮：尺寸查规格（Xl），与导航键同档 —— 它们在工具栏同一排，
        // 大小必须一致（之前是 19 vs 20）。
        if !draw_ui_icon(
            hdc,
            crate::icon_theme::Ui::Rename,
            &gr,
            crate::icon_spec::size_of(crate::icon_theme::Ui::Rename, s),
            main_col,
        ) {
            let g = glyph_font(s);
            let og = SelectObject(hdc, g.into());
            SetTextColor(hdc, main_col);
            draw_text(hdc, "\u{E8AC}", gr, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            SelectObject(hdc, og);
        }
        // 右下角小三角："这是一个下拉，不是普通按钮"。用一个更小的字形字号，
        // 免得它和主字形抢视觉重量。
        let ar = RECT {
            left: r.left + (16.0 * s) as i32,
            top: r.bottom - (14.0 * s) as i32,
            right: r.right,
            bottom: r.bottom - (2.0 * s) as i32,
        };
        // 第71轮：尺寸改走 `icon_spec::size_of` —— 之前这里是硬编码
        // `(11.0 * s)`，而 `ChevronDown` 在 `icon_spec.rs` 里归 `Xs`(10px) 档，
        // 同一个字形因此有**两套尺寸来源**（10 档 vs 11 裸数字），
        // 改档位时这里会静默不跟着变。
        let chev_sz = crate::icon_spec::size_of(crate::icon_theme::Ui::ChevronDown, s);
        if !draw_ui_icon(hdc, crate::icon_theme::Ui::ChevronDown, &ar, chev_sz, t.text_dim) {
            let small = caret_font(s);
            let os = SelectObject(hdc, small.into());
            SetTextColor(hdc, t.text_dim);
            draw_text(hdc, "\u{E70D}", ar, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            SelectObject(hdc, os);
        }
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
    // 主题接管时（第49轮）：树箭头也走主题字形（圆角套把三角的角倒圆）。
    //
    // 第66轮：尺寸查规格（Xs），**不再按格子实算**。之前是
    // `(min(w,h)).max(8)` —— 槽位放大一点图标就跟着变大，同一个箭头在
    // 不同地方大小不同，正是"改一处另一处就变"的根源。
    let kind = if down { crate::icon_theme::Ui::ChevronDown } else { crate::icon_theme::Ui::ChevronRight };
    let sz = crate::icon_spec::size_of(kind, 1.0);
    if draw_ui_icon(hdc, kind, &crate::icon_spec::centered(r, sz), sz, color) {
        return;
    }
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
        // 第76轮：这里原本也是每帧一次真实 IO。它只有 1 次/帧（不是 10~30 次），
        // 但同样与帧无关，所以走同一张缓存（key 的 `cur` 分量用它自己）。
        //
        // 注意语义差别：原代码 `unwrap_or_default()` 得到的是**空 PathBuf**，
        // 而 `canonicalize_cached` 是"判是否等于 cur"的布尔口径。所以这里
        // 保持原口径单列一个取值的缓存入口，见 `cur_path_cached`。
        let cur = cur_path_cached(&app.tab().path);
        // 第57轮：动画要"知道当前位置在哪一行"，所以先整棵判一遍 active。
        // 画的时候也复用这份结果（原来是在循环里逐行判，一帧要问一遍 Shell）。
        let actives: Vec<bool> = rows
            .iter()
            .map(|r| !r.spacer && sidebar_row_active(app, r, &cur))
            .collect();
        let active_idx = actives.iter().position(|v| *v);
        if app.cfg.ui.anim.sidebar {
            // 目标 y **每帧按当前滚动量重算**：滚动本身也在动，写死一个值
            // 会让指示条滑到旧位置上。
            let y_of = |i: usize| -> Option<f32> {
                if i >= rows.len() || rows[i].spacer {
                    return None;
                }
                Some(sidebar_row_geoms(l, i, rows[i].depth, scale, app.sidebar_scroll).0.top as f32)
            };
            let at = active_idx.and_then(y_of);
            let ht = app.sidebar_hover.and_then(y_of);
            app.sb_active_target = at;
            app.sb_hover_target = ht;
            // 只有"我在哪"那一行**换了**才重新算滚动目标 —— 每帧算一次会把
            // 用户自己滚的位置抢回来（表现为"手动滚完又被拽回去"）。
            if let Some(ai) = active_idx {
                if app.sb_last_active != Some(ai) {
                    app.sb_last_active = Some(ai);
                    if let Some(top) = active_idx.and_then(y_of) {
                        let rh = sidebar_row_h(scale);
                        let top = top as i32;
                        let need = if top < l.sidebar.top {
                            top - l.sidebar.top
                        } else if top + rh > l.sidebar.bottom {
                            top + rh - l.sidebar.bottom
                        } else {
                            0
                        };
                        if need != 0 {
                            // 钳制口径与 scroll_sidebar 完全一致（同一个 max）
                            let total = rows.len() as i32 * rh;
                            let view_h = l.sidebar.bottom - l.sidebar.top - rh;
                            let max = (total - view_h).max(0);
                            app.sb_scroll_target = Some((app.sidebar_scroll + need).clamp(0, max));
                            app.sync_anim_timer();
                        }
                    }
                }
            }
        } else {
            app.sb_active_target = None;
            app.sb_hover_target = None;
            app.sb_scroll_target = None;
            app.sb_last_active = None;
            app.sb_active_y = None;
            app.sb_hover_y = None;
        }
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

            let active = actives.get(i).copied().unwrap_or(false);
            let hovered = app.sidebar_hover == Some(i);
            // 第57轮：动画开着时高亮条画在**缓动后的 y** 上 —— 从上一行滑过来，
            // 而不是"消失 + 在新行出现"。关掉动画就完全照旧（用行本体的 y）。
            let bar_top = if app.cfg.ui.anim.sidebar {
                let eased = if active { app.sb_active_y } else { app.sb_hover_y };
                eased.map(|v| v.round() as i32).unwrap_or(row_r.top)
            } else {
                row_r.top
            };
            let bar_h = row_r.bottom - row_r.top;
            if active || hovered {
                let only_hover = hovered && !active;
                let r2 = RECT { left: row_r.left, top: bar_top, right: row_r.right, bottom: bar_top + bar_h };
                fill_radius(
                    app,
                    hdc,
                    if only_hover { "sidebar.hover" } else { "sidebar.selected" },
                    &r2,
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
                    &RECT { left: row_r.left, top: bar_top, right: row_r.left + (3.0 * scale) as i32, bottom: bar_top + bar_h },
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
/// 表头分隔线的位置：每一列的右沿 + 最后补一条"内容区右沿"。
///
/// 返回 `(列下标, x)`。下标是**当前可见列的顺序**（第33轮列可增删改序），
/// 拖列宽/命中都按这个下标走，不再有"0=名称 1=日期 2=类型 3=大小"的硬编码。
pub fn header_splits(app: &App, l: &Layout) -> Vec<(usize, i32)> {
    if app.tab().view_mode != zenfile_core::ViewMode::Details {
        return Vec::new();
    }
    // 详细信息视图的图标恒为 16（与 paint_view 里同一套取值）
    let icon_size = (16.0 * app.metrics.scale) as i32;
    let boxes = col_boxes(app, l, icon_size);
    let mut out: Vec<(usize, i32)> = boxes.iter().enumerate().map(|(i, b)| (i, b.x + b.w)).collect();
    // 最后一条：整块列区右沿（拖它 = 拉伸最后一列）
    if let Some(last) = boxes.last() {
        out.push((boxes.len(), last.x + last.w));
    }
    out
}

/// 光标落在表头的哪一列上（0=名称，搜索态 1=位置，其后 修改日期/类型/大小）。
///
/// 绘制与命中共用 `detail_cols` 同一份列几何。表头的"筛选下拉"和右键菜单都
/// 靠它定位：各自去数一遍列的话，拖过一次列宽就对不上了。
pub fn header_col_hit(app: &App, l: &Layout, x: i32, y: i32) -> Option<usize> {
    if app.tab().view_mode != zenfile_core::ViewMode::Details {
        return None;
    }
    let header_h = (26.0 * app.metrics.scale) as i32;
    if y < l.view.top || y >= l.view.top + header_h || x < l.view.left || x >= l.view.right {
        return None;
    }
    let icon_size = (16.0 * app.metrics.scale) as i32;
    let boxes = col_boxes(app, l, icon_size);
    for (i, b) in boxes.iter().enumerate() {
        let right = (b.x + b.w).min(l.view.right);
        if x >= b.x && x < right.max(b.x) {
            return Some(i);
        }
    }
    None
}

/// 表头里"筛选下拉"那个小三角的矩形（悬停时才画）。绘制与命中共用。
pub fn header_filter_rect(app: &App, l: &Layout, col: usize) -> RECT {
    let s = app.metrics.scale;
    let icon_size = (16.0 * s) as i32;
    let boxes = col_boxes(app, l, icon_size);
    let (x0, w) = boxes
        .get(col)
        .map(|b| (b.x, b.w))
        .unwrap_or((l.view.left, 0));
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
    if app.tab().view_mode != zenfile_core::ViewMode::Details {
        return None;
    }
    let header_h = (26.0 * app.metrics.scale) as i32;
    // 分栏时列分隔线画在**窗格自己的表头**上（paint_panes 逐窗格把 view 覆写成
    // 窗格矩形 + 26px 局部标签条）。拿全局 l.view 判：y 带偏一条标签条、x 原点
    // 偏半扇窗格 —— 表头上的列边界要么点不中、要么踩到隔壁窗格的列上。
    let hl = if app.panes.len() > 1 {
        match pane_index_at(app, l, x, y) {
            Some(pi) => pane_layout(app, l, pi),
            None => *l,
        }
    } else {
        *l
    };
    if y >= hl.view.top && y <= hl.view.top + header_h {
        for (i, sx) in header_splits(app, &hl) {
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
    if cur.as_os_str().is_empty() {
        return false;
    }
    // 第76轮：`canonicalize` = `GetFullPathNameW` + 开句柄 +
    // `DeviceIoControl(IOCTL_GET_VOLUME_INFORMATION)`。而这一行是**逐行**判的
    // （见 paint 的 `actives` 构造），侧栏 10~30 行 ⇒ **每帧 10~30 次真实磁盘
    // IO**，机械盘/网络路径上单次能到 ms 级 —— 这是 `sidebar` 那 1.16ms 的一大块。
    //
    // 结果只依赖 `(row.target, cur)` 两个字符串，**与帧无关**，所以缓存。
    // 命中后这里的磁盘 IO 归零（代价退化成一次 Mutex 查找）。
    canonicalize_cached(&row.target, cur)
}

/// 第76轮：`canonicalize` 的进程级缓存。
///
/// # key 与失效
///
/// key 是 `(target, cur)` 的原样字符串对 —— 两者都是结果的**全部**输入
/// （`canonicalize` 是纯函数，同输入必同输出）。`cur` 进了 key 意味着"切了目录
/// 必然重算"，`target` 进了 key 意味着"侧栏换了快捷方式必然重算"。
///
/// # TTL 为什么取 3 秒
///
/// 唯一的漏检场景是：`row.target` 与 `cur` 两个字符串都没变，但**磁盘上那条路径
/// 的"规范形式"变了** —— 实际只可能来自"目录被删/重建成了另一个卷上的同名目录"
/// 或"网络驱动器重新映射到别的卷"。前者要求用户恰好在侧栏停留的那几秒内删掉
/// 并重建目录；后者在盘符仍可用的前提下不可观测。换来的是：静止侧栏下
/// 每帧 10~30 次真实 IO **完全归零**，而这正是本轮要治的病。
///
/// 代价上界：最多 3 秒的显示滞后，且只在这两种极低频操作下发生。
/// 真要做到零滞后要接 `ReadDirectoryChangesW` 驱动失效，那是改动量大得多的一轮，
/// 本轮先取 TTL。容量上限 512 条，远大于任何侧栏的行数，纯粹是防泄漏兜底。
static CANON_CACHE: crate::paint_cache::LazyCache<(String, String), Option<std::path::PathBuf>> =
    crate::paint_cache::LazyCache::new(std::time::Duration::from_secs(3), 512);

/// 带缓存的 `canonicalize`。返回值 `None` 表示"解析失败"，与原始语义一致。
fn canonicalize_cached(target: &str, cur: &std::path::Path) -> bool {
    let key = (target.to_string(), cur.as_os_str().to_string_lossy().into_owned());
    if let Some(hit) = CANON_CACHE.get(&key) {
        return hit.as_deref() == Some(cur);
    }
    // 未命中：真做一次 IO。结果**连同失败一起缓存** —— 失败也是稳定的
    // （路径不存在就是不存在），否则每帧都要为一个不存在的路径付一次 IO。
    let resolved = std::fs::canonicalize(std::path::PathBuf::from(target)).ok();
    CANON_CACHE.insert(key, resolved.clone());
    resolved.as_deref() == Some(cur)
}

/// 第76轮：`cur` 自身的 `canonicalize`（每帧 1 次），失败口径与原
/// `unwrap_or_default()` 严格一致：解析不出来就是空 `PathBuf`。
///
/// 与 [`CANON_CACHE`] 分开是因为**结果口径不同**（一个是 `PathBuf`、一个是
/// 布尔），共用一张表会让两种含义互相覆盖。TTL/容量同样 3 秒 / 512。
static CUR_CACHE: crate::paint_cache::LazyCache<String, std::path::PathBuf> =
    crate::paint_cache::LazyCache::new(std::time::Duration::from_secs(3), 512);

/// 带缓存的 `canonicalize`，`unwrap_or_default()` 口径。
fn cur_path_cached(path: &std::path::Path) -> std::path::PathBuf {
    let key = path.as_os_str().to_string_lossy().into_owned();
    if let Some(hit) = CUR_CACHE.get(&key) {
        return hit;
    }
    let resolved = std::fs::canonicalize(path).unwrap_or_default();
    CUR_CACHE.insert(key, resolved.clone());
    resolved
}

// ================================================ 第76轮：Details 非名称列文本缓存

/// 列文本缓存的 key。**每一个影响文本的输入都必须在这里**，漏一个就是
/// "改了设置但画面不变"。
///
/// - `dir`：文本来自 `entry`，换目录必然换 key。
/// - `from..to`：可见窗口。滚动一格就变。
/// - `cols_sig`：可见列定义（`other_cols` 拼的串 + `show_ext` 标记）。
///   增删列 / 开关扩展名都会改它。
/// - `data_fp`：**可见条目自身的数据指纹**（见 [`data_fingerprint`]）。
///   这是最关键的一项：改排序 / 改筛选 / 重新枚举之后，`visible` 里的
///   下标会重排、条目内容会换，而这些**未必**改变 `dir` 与窗口。
#[derive(PartialEq, Eq, Hash, Clone)]
struct ColTextKey {
    dir: String,
    from: usize,
    to: usize,
    cols_sig: String,
    data_fp: u64,
}

/// 可见窗口内**条目数据本身**的指纹（FNV-1a 64 滚动）。
///
/// 拌进去的量 = 下标 + `kind` + 路径 + `size` —— 恰好是四类列文本的**全部**
/// 输入：`type_label` 看 `kind` 与 `extension`（都在路径里）、`C_PATH` 看
/// `path.parent()`、大小列看 `size`、其余列走 `cell()` 而它内部本来也按
/// `(列, 路径)` 自己缓存。
///
/// 为什么不用"版本号/世代号"而要真的算指纹：
///   * 用 `Tab::generation` 只能覆盖"重新枚举"，**覆盖不了改排序/改筛选**
///     （那两者只重排 `visible`，不重新枚举）—— 画面上就是"排序点了没反应"。
///   * 而指纹是直接判"喂进去的数据变没变"，漏判面最小。
///
/// 代价：57 行 × 几次整数/指针运算，约 1~2µs，与它省掉的 171 次 `format!`
/// 分配相比可以忽略。
fn data_fingerprint(visible: &[usize], entries: &[zenfile_core::FileEntry], from: usize, to: usize) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |v: u64| {
        h ^= v;
        h = h.wrapping_mul(0x100_0000_01b3);
    };
    mix((to.saturating_sub(from)) as u64);
    for &i in visible.iter().take(to).skip(from) {
        mix(i as u64);
        let Some(e) = entries.get(i) else {
            mix(u64::MAX);
            continue;
        };
        // `kind` 的判别值（Directory/File/ReparsePoint）。
        mix(match e.kind {
            zenfile_core::FileKind::Directory => 0,
            zenfile_core::FileKind::File => 1,
            zenfile_core::FileKind::ReparsePoint => 2,
        });
        for b in e.path.as_os_str().to_string_lossy().as_bytes() {
            mix(*b as u64);
        }
        mix(0xff);
        mix(e.size);
    }
    h
}

/// 列文本缓存本体。
///
/// **TTL 5 秒 + 容量 4096 组**：一组 = 一次 `(窗口, 列定义)` 下所有可见行的
/// 全部非名称列文本。滚动时窗口每帧都在动，理论上不该有跨帧命中 —— 但
/// "静止不动时反复重绘"（hover、动画尾巴、窗口微调）才是常态，那才是收益来源。
/// TTL 是**兜底**：正常路径靠 key 精确失效，TTL 只防止极端情况下堆积。
static COL_TEXT_CACHE: crate::paint_cache::LazyCache<ColTextKey, Vec<Vec<String>>> =
    crate::paint_cache::LazyCache::new(std::time::Duration::from_secs(5), 4096);

/// 取这一窗口的列文本；命中不了就现算一遍并写入。
///
/// `epoch` 由调用方给（`app` 里维护的自增计数器：重新枚举 / 读配置后 +1）。
fn col_texts_for(
    app: &App,
    key: ColTextKey,
    boxes: &[ColBox],
    other_cols: &[&'static str],
    visible: &[usize],
    from: usize,
    to: usize,
) -> Vec<Vec<String>> {
    if let Some(hit) = COL_TEXT_CACHE.get(&key) {
        return hit;
    }
    let show_ext = app.cfg.files.view.show_extensions;
    let unit = app.cfg.files.view.size_unit_of();
    let entries = &app.tab().entries;
    let mut out: Vec<Vec<String>> = Vec::with_capacity(to.saturating_sub(from));
    for i in from..to {
        let Some(entry) = visible.get(i).and_then(|&ix| entries.get(ix)) else {
            out.push(other_cols.iter().map(|_| String::new()).collect());
            continue;
        };
        let row: Vec<String> = other_cols
            .iter()
            .map(|id| match *id {
                zenfile_core::columns::C_TYPE => entry.type_label(),
                zenfile_core::columns::C_PATH => entry
                    .path
                    .parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default(),
                // 大小列：目录恒画 `—`，文件用格式化后的值（与原口径一致）。
                zenfile_core::columns::C_SIZE => {
                    if entry.is_dir() {
                        "—".to_string()
                    } else {
                        crate::app::human_unit(entry.size, unit)
                    }
                }
                _ => match zenfile_core::columns::cell(id, entry, show_ext) {
                    Some(s) => s,
                    None => "—".to_string(),
                },
            })
            .collect();
        out.push(row);
    }
    COL_TEXT_CACHE.insert(key, out.clone());
    out
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
    while i < entries.len() && !zenfile_core::paths::is_drive_root(&entries[i].path) {
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
            if !sel
                && !focused
                && app.hover_row == Some(tile.index)
                && app.hover_pane == Some(app.focus_pane)
            {
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

// —— 通用列布局（第33轮：列可显示/隐藏/排序）——

/// 一列的落位：左沿 x + 宽度（像素）。
pub struct ColBox {
    pub id: &'static str,
    pub x: i32,
    pub w: i32,
}

/// 详细列表**当前实际画的那几列**的几何。绘制 / 命中 / 拖列宽 /
/// 表头右键**全部读这一份**（老纪律：几何只有一处来源）。
///
/// 名称列左移让出图标位（21px 缩进 + 图标宽 + 3px 间隙），其余列紧挨排列。
/// 搜索态的「位置」列由 `App::visible_cols` 注入，所以这里不用特殊处理。
pub fn col_boxes(app: &App, l: &Layout, icon_size: i32) -> Vec<ColBox> {
    let s = app.metrics.scale;
    let text_pad = (21.0 * s) as i32;
    let icon_gap = (3.0 * s) as i32;
    let searching = app.tab().search_root.is_some();
    let mut out: Vec<ColBox> = Vec::new();
    let mut x = l.view.left;
    for id in app.visible_cols() {
        let idc: &'static str = zenfile_core::columns::COL_ALL
            .iter()
            .copied()
            .find(|c| *c == id)
            .unwrap_or("name");
        let w = if searching && idc == zenfile_core::columns::C_NAME {
            // 搜索态名称列收窄给「位置」列腾地方（照资源管理器）
            ((app.col_width(idc).min(190) as f32 * s) as i32)
        } else {
            ((app.col_width(idc) as f32 * s) as i32)
        };
        if idc == zenfile_core::columns::C_NAME {
            x = l.view.left + text_pad + icon_size + icon_gap;
        }
        out.push(ColBox { id: idc, x, w });
        x += w;
    }
    out
}

/// 找某一列的几何（按 id）。
pub fn col_box_of<'a>(boxes: &'a [ColBox], id: &str) -> Option<&'a ColBox> {
    boxes.iter().find(|b| b.id == id)
}

/// 列区**总右沿** = 最后一列的 x + w。
///
/// 行高亮/悬停底色的右沿、点击判空白的边界都用它 —— 与表头分隔线同一来源，
/// 天然对齐（第32轮 #1 的教训：这两个必须同口径）。
pub fn cols_right(boxes: &[ColBox]) -> i32 {
    boxes.last().map(|b| b.x + b.w).unwrap_or(0)
}

/// 光标落在哪一列上（按可见列顺序，返回下标）。
pub fn col_index_at(boxes: &[ColBox], x: i32) -> Option<usize> {
    boxes.iter().position(|b| x >= b.x && x < b.x + b.w)
}

/// 列下标 → 列 id（命中测试与排序都靠它翻译）。
pub fn col_id_at(app: &App, boxes: &[ColBox], x: i32) -> Option<&'static str> {
    col_index_at(boxes, x).and_then(|i| boxes.get(i)).map(|b| b.id)
}

/// 旧结构的兼容壳：把通用列几何翻译成"名称/日期/类型/大小"四个字段。
///
/// 存量代码（筛选菜单、排序命中、拖列宽）暂时还用这套；等它们各自迁到
/// `col_boxes` 之后再删。不另起一套几何 —— 这里只做字段翻译。
pub fn detail_cols(app: &App, l: &Layout, icon_size: i32) -> DetailCols {
    let boxes = col_boxes(app, l, icon_size);
    let get = |id: &str| -> (i32, i32) {
        col_box_of(&boxes, id).map(|b| (b.x, b.w)).unwrap_or((0, 0))
    };
    let (name, name_w) = get(zenfile_core::columns::C_NAME);
    let (path, path_w) = get(zenfile_core::columns::C_PATH);
    let (date, date_w) = get(zenfile_core::columns::C_DATE_MOD);
    let (kind, kind_w) = get(zenfile_core::columns::C_TYPE);
    let (size, size_w) = get(zenfile_core::columns::C_SIZE);
    DetailCols {
        name,
        path: if path_w > 0 { Some(path) } else { None },
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
/// 第57轮：条目图标**这一帧该画多大**。
///
/// 两件事叠在一起：
///   * 大图标 / 中图标：悬停或选中时放大 `ANIM_ZOOM`，缓动进出；
///   * 详细信息 / 列表：刚变成悬停/选中的那一下"果冻"回弹（阻尼振荡）。
///
/// 返回值**量化到 2px**：放大过程中每帧一个新尺寸会把图标的两张派生缓存
/// （`upscaled` / `scaled_thumbs`，key 里都含尺寸）撑爆，跑一遍动画就
/// 把正常尺寸的条目全挤出去，反而更慢。
fn icon_draw_size(app: &App, key: &str, base: i32, large: bool) -> i32 {
    let mut s = base as f32;
    if large {
        if let Some(z) = app.icon_zoom.get(key) {
            s *= 1.0 + z;
        }
    }
    if let Some(t0) = app.icon_bounce.get(key) {
        let now = unsafe { GetTickCount64() };
        let e = now.saturating_sub(*t0) as f32 / crate::app::ANIM_BOUNCE_MS as f32;
        if e < 1.0 {
            // 阻尼余弦：t=0 立刻鼓一下，之后来回衰减到 1.0（果冻感）
            let w = 2.0 * std::f32::consts::PI * crate::app::ANIM_BOUNCE_FREQ * e;
            s *= 1.0 + crate::app::ANIM_BOUNCE_AMP * (1.0 - e) * w.cos();
        }
    }
    (((s / 2.0).round() * 2.0) as i32).max(4)
}

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
        let large_icon = matches!(mode, zenfile_core::ViewMode::MediumIcons | zenfile_core::ViewMode::LargeIcons);
        let icon_size = if large_icon {
            if mode == zenfile_core::ViewMode::LargeIcons {
                (96.0 * app.metrics.scale) as i32
            } else {
                (48.0 * app.metrics.scale) as i32
            }
        } else {
            (16.0 * app.metrics.scale) as i32
        };
        // 列几何：算**一次**，行循环与表头循环共用（第33轮：列可显示/隐藏/排序）
        let boxes = col_boxes(app, l, icon_size);
        let col_name = col_box_of(&boxes, zenfile_core::columns::C_NAME).map(|b| b.x).unwrap_or(l.view.left);
        let col_date = 0;
        let col_type = 0;
        let col_size = 0;

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
        let list_top = l.view.top + if mode == zenfile_core::ViewMode::Details { header_h } else { 0 };
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
        // 文字要不要用暗色（缓存文件夹）。图标淡了字还是全亮的话，
        // 看上去只是"图标旧了"而不是"这个目录是临时的"。
        let mut dim_text: Vec<bool> = Vec::with_capacity(n);
        let mut size_texts: Vec<String> = Vec::with_capacity(n);
        // 第76轮：其余可见列（类型/位置/所有者/版本…）的**纯文本**。
        //
        // 原来这些文本是在**行循环里**（paint 另一处 `match b.id`）现算的：
        // Details 模式 57 行 × 3 个非名列 = **每帧 171 次 `format!` 分配**，
        // 全是与帧无关的纯格式化。搬到这里一次算完，行循环里只按下标取。
        //
        // 为什么这里安全（借用不冲突）：本循环体已经"独占借用 app"去做
        // icons / display_name，所以**在同一个循环里**顺手多算几列纯文本
        // 不增加任何借用负担。取 `tab.entries` 的只读引用在这个位置也合法
        // （紧随其后的 `app.icons.*` 调用同样已经是这个模式）。
        //
        // key 用 `col_boxes` 已产出的可见列定义（`boxes`），所以
        // "增删列 / 改列可见性"会自动换 key；`show_ext`（影响 `cell()` 的输出）
        // 也进了判断。
        let details_mode = mode == zenfile_core::ViewMode::Details;
        let show_ext = app.cfg.files.view.show_extensions;
        // 非名称列的 id 清单（`boxes` 去掉名称列，与取值时的下标严格对齐）。
        let other_cols: Vec<&'static str> = boxes
            .iter()
            .filter(|b| b.id != zenfile_core::columns::C_NAME)
            .map(|b| b.id)
            .collect();
        // 列定义变了就得重算：把 (id, show_ext) 拼成一个可比对的串。
        let cols_sig = other_cols
            .iter()
            .map(|id| format!("{id}\u{1}"))
            .collect::<String>()
            + if show_ext { "E" } else { "" };
        let mut col_texts: Vec<Vec<String>> = Vec::new();
        let unit = app.cfg.files.view.size_unit_of();
        // 第76轮：整窗口的列文本在**进循环之前**一次取好（命中缓存就是零成本）。
        // key 覆盖了全部影响文本的输入，见 `ColTextKey` 的注释。
        if details_mode {
            col_texts = col_texts_for(
                app,
                ColTextKey {
                    dir: app.tab().path.as_os_str().to_string_lossy().into_owned(),
                    from,
                    to,
                    cols_sig: cols_sig.clone(),
                    data_fp: data_fingerprint(&visible, &app.tab().entries, from, to),
                },
                &boxes,
                &other_cols,
                &visible,
                from,
                to,
            );
        }
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
                    zenfile_core::columns::is_cache_dir(&e.name) && e.is_dir(),
                )
            }) else {
                icon_handles.push(HICON(std::ptr::null_mut()));
                icon_thumbs.push(None);
                icon_alpha.push(255);
                names.push(String::new());
                size_texts.push(String::new());
                dim_text.push(false);
                continue;
            };
            let (path, name, size, is_dir, is_hidden, is_system, is_cache) = snap;
            let mut alpha = if is_system { 110 } else if is_hidden { 150 } else { 255 };
            // 缓存/临时文件夹：图标与文字一起压淡（第33轮）。
            //
            // 为什么不复用 hidden 的 150：那档是"你看不见它"，而缓存目录是
            // "它就在那儿、但内容是丢掉也没关系的" —— 用另一档（120）才能
            // 跟隐藏项区分开，扫一眼就知道这个目录别去备份。
            if is_cache {
                alpha = alpha.min(120);
            }
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
            dim_text.push(is_cache);
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

        // 第57轮：这一帧"悬停 / 选中 / 焦点"的条目（key, 是不是大图标视图）。
        // 画完之后统一登记动画 —— 循环里改不了 App：`tab` 正借着它。
        let mut hot_items: Vec<(String, bool)> = Vec::new();
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
                    // 列区右沿（第32轮 #1）：Details 用**循环外**已经算好的那份
                    // `boxes`（表头分隔线同源），**绝不在行循环里重算**。
                    //
                    // 第72轮：这里原来每行调一次 `col_boxes`，而它内部每列都要
                    // `App::col_width` → `sysview::system_col_width`，
                    // 后者每次做 `RegOpenKey`+`RegQueryValue`+`RegCloseKey`。
                    // 57 行 × 4 列 = **每帧 228 次注册表往返**。
                    // 埋点实测 Details 模式单帧 paint 的 P95 是 38.6ms、
                    // 整窗 58.8% 的墙钟都耗在 paint 上 —— 这 228 次往返就是主因。
                    //
                    // 关键：`w` 是**整列几何的常量**，与行号 `i` 无关，所以
                    // 循环外算一次、循环内复用，结果**逐字节不变**。
                    let w = if mode == zenfile_core::ViewMode::Details {
                        (cols_right(&boxes).min(l.view.right) - l.view.left).max(10)
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
            let ekey = crate::app::key_of(entry);
            let selected = tab.selected.contains(&ekey);
            // 焦点行只在用户真的交互过之后才画出来：否则每次打开文件夹，
            // 第 0 行都顶着一条高亮，看着像"默认就勾选了第一行"。
            let focused = app.focus_visible && i == tab.focus_row;
            // 悬停：和选中同一套语言（一层底色），但**故意弱一档**。
            // 选中 = 底色 + 行首强调竖线；悬停 = 只有一档更淡的底色，没有竖线。
            // 两者一样重就分不清"我已经选了它"和"我只是指在这儿"。
            let hovered =
                !selected && !focused && app.hover_row == Some(index) && app.hover_pane == Some(app.focus_pane);
            if hovered || selected || focused {
                hot_items.push((ekey.clone(), large_icon));
            }
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
            // 第57轮：图标动画后的实际尺寸 + 让图标**中心不动**的偏移量
            let isz = icon_draw_size(app, &ekey, icon_size, large_icon);
            let ioff = (isz - icon_size) / 2;
            if mode == zenfile_core::ViewMode::Details {
                // 名称列特殊：左边要让出图标位，所以 x 由 col_boxes 给
                draw_item_icon(app, hdc, icon, thumb, col_name - icon_size - icon_gap - ioff, y + (row_h - icon_size) / 2 - ioff, isz, alpha);
                // 名称列文字：右沿 = 名称列自己的右沿（隐藏了「位置」列时
                // 就是下一列的左沿）。必须带 DT_END_ELLIPSIS：名字长了不省略
                // 就会一路画到下一列上（长文件名压在"修改日期"上面）。
                if let Some(nb) = col_box_of(&boxes, zenfile_core::columns::C_NAME) {
                    let dim = dim_text.get(k).copied().unwrap_or(false);
                    if dim && !selected {
                        SetTextColor(hdc, t.text_dim);
                    }
                    draw_text(
                        hdc,
                        &name,
                        RECT { left: nb.x, top: y, right: nb.x + nb.w - 4, bottom: y + row_h },
                        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
                    );
                    if dim && !selected {
                        SetTextColor(hdc, t.text);
                    }
                }
                // 其余列**按当前可见列逐个画**（第33轮：列可显示/隐藏/排序，
                // 不能再写死"日期/类型/大小"三个字段）。每列都夹在自己的
                // [x, x+w) 里，DT_END_ELLIPSIS 兜底 —— 少一个就会串列。
                SetTextColor(hdc, if selected { t.selection_text } else { t.text_dim });
                let cell = DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS;
                for (ci, b) in boxes.iter().enumerate() {
                    if b.id == zenfile_core::columns::C_NAME {
                        continue;
                    }
                    // 第76轮：文本从**循环外**一次算好的 `col_texts` 里按下标取。
                    // `other_cols` 是 `boxes` 去掉名称列后的顺序，两者严格对齐。
                    // `ci` 是 `boxes` 的下标，减 1 换算成 `other_cols` 的下标。
                    let txt = match ci.checked_sub(1).and_then(|j| col_texts.get(k))
                    {
                        Some(row) => row.get(ci - 1).cloned().unwrap_or_default(),
                        // 理论上不可达（col_texts 与 boxes 同源同长度）；
                        // 真到了也不能崩，退回空串即可。
                        None => String::new(),
                    };
                    let mut cx = b.x;
                    // 搜索态「位置」列靠右对齐更像资源管理器，其余左对齐
                    let mut fl = cell;
                    if b.id == zenfile_core::columns::C_SIZE {
                        fl = DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS;
                    }
                    let _ = &mut cx;
                    draw_text(
                        hdc,
                        &txt,
                        RECT { left: b.x, top: y, right: b.x + b.w - 4, bottom: y + row_h },
                        fl,
                    );
                }
            } else if mode == zenfile_core::ViewMode::List {
                let ix = l.view.left + text_pad;
                draw_item_icon(app, hdc, icon, thumb, ix - ioff, y + (row_h - icon_size) / 2 - ioff, isz, alpha);
                let name_r = RECT { left: ix + icon_size + 8, top: y, right: l.view.right, bottom: y + row_h };
                draw_text(hdc, &name, name_r, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
            } else {
                // 网格磁贴：图标在格子顶部居中，名字压在图标下方、居中并省略过长部分
                let cx = x + (w_cell - isz) / 2;
                draw_item_icon(app, hdc, icon, thumb, cx, y + (8.0 * app.metrics.scale) as i32 - ioff, isz, alpha);
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

        // 第57轮：登记这一帧的图标动画（`tab` 的借用到此结束，可以改 App 了）。
        //
        // 大图标 / 中图标 = **放大**（悬停或选中期间一直大着，缓动进出）；
        // 详细信息 / 列表 = **回弹**（只在"刚变成悬停/选中"那一帧弹一次，
        // 判据是它不在上一帧的 hot 集合里 —— 一直在上面就不会反复弹）。
        if !hot_items.is_empty() {
            let anim = app.cfg.ui.anim;
            let mut touched = false;
            for (k, large) in hot_items.drain(..) {
                if large && anim.icon_zoom {
                    app.icon_zoom_tgt.insert(k.clone(), crate::app::ANIM_ZOOM);
                    touched = true;
                }
                if !large && anim.icon_bounce && !app.icon_prev_active.contains(&k) {
                    let now = GetTickCount64();
                    app.icon_bounce.insert(k.clone(), now);
                    touched = true;
                }
                app.icon_active.insert(k);
            }
            if touched {
                app.sync_anim_timer();
            }
        }

        // 表头**最后**画，且底色不透明：滚动时条目绝不会压到它上面
        if mode == zenfile_core::ViewMode::Details {
            let hb = CreateSolidBrush(t.window_bg);
            let hr = RECT { left: l.view.left, top: l.view.top, right: l.view.right, bottom: l.view.top + header_h };
            let _ = FillRect(hdc, &hr, hb);
            let _ = DeleteObject(hb.into());
            let sort = app.tab().sort;
            let hf = header_font(app.metrics.scale);
            let old = SelectObject(hdc, hf.into());
            // 表头也走**当前可见列**（第33轮）：列名与列几何同一来源，
            // 隐藏掉的列这里不会露出一个空标题。
            for (ci, b) in boxes.iter().enumerate() {
                let label: &str = zenfile_core::columns::label_of(b.id);
                // 列名一律**夹在自己那一列的宽度里**：不夹的话列宽拖窄之后，
                // 列名会一路画到右边那一列的头上（"列拖不动/看着乱"的观感来源）。
                let right = (b.x + b.w).min(l.view.right);
                let rr = RECT { left: b.x, top: l.view.top, right: right.max(b.x), bottom: l.view.top + header_h };
                // 只有可排序的列才有排序箭头；作者/标记这类列点了没反应
                let key: Option<zenfile_core::SortKey> = match b.id {
                    zenfile_core::columns::C_NAME => Some(zenfile_core::SortKey::Name),
                    zenfile_core::columns::C_DATE_MOD
                    | zenfile_core::columns::C_DATE_CREATED
                    | zenfile_core::columns::C_DATE_ACCESSED => Some(zenfile_core::SortKey::Modified),
                    zenfile_core::columns::C_TYPE => Some(zenfile_core::SortKey::Type),
                    zenfile_core::columns::C_SIZE => Some(zenfile_core::SortKey::Size),
                    _ => None,
                };
                let active = key.map(|k| sort.key == k).unwrap_or(false);
                SetTextColor(hdc, if active { t.text } else { t.text_dim });
                // 列名在**自己这一列里居中**（资源管理器 Win11 表头就是这样）：
                // 贴着左沿画的时候，它紧贴上一列的分隔线，看着像那一列的尾巴。
                let tw = unsafe { text_width(hdc, hf, label) };
                let arrow_w = if active { (14.0 * app.metrics.scale) as i32 } else { 0 };
                let block_w = tw + arrow_w;
                let bx = b.x + ((rr.right - rr.left) - block_w) / 2;
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
                if app.hover_header == Some(ci) && key.is_some() {
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

fn size_of(e: &zenfile_core::FileEntry) -> String {
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
pub fn status_view_hit(l: &Layout, m: &Metrics, x: i32, y: i32) -> Option<zenfile_core::ViewMode> {
    let (icons, details) = status_view_rects(l, m);
    if x >= icons.left && x <= icons.right && y >= icons.top && y <= icons.bottom {
        return Some(zenfile_core::ViewMode::LargeIcons);
    }
    if x >= details.left && x <= details.right && y >= details.top && y <= details.bottom {
        return Some(zenfile_core::ViewMode::Details);
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
    // 主题接管时（第49轮）：这两个视图字形也走主题。
    //
    // 第66轮：尺寸查规格（Xs），不再"格子减 4"。状态栏那两个键的格子
    // 与树箭头一样是微型档，规格必须同源。
    let kind = if grid { crate::icon_theme::Ui::ViewGrid } else { crate::icon_theme::Ui::ViewList };
    let sz = crate::icon_spec::size_of(kind, 1.0);
    if draw_ui_icon(hdc, kind, &crate::icon_spec::centered(r, sz), sz, color) {
        return;
    }
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
    let is_details = matches!(cur, zenfile_core::ViewMode::Details);
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
/// 搜索框的左边距（放大镜占位）。
///
/// 与 `paint_search_decor` 里画的放大镜**同一个公式** —— 这两个值必须
/// 一起改。以前这里写死 26 而放大镜按 scale 缩放，缩放不为 1 时文字就会
/// 压在放大镜上。
fn search_left_margin(scale: f32) -> i32 {
    let m = ((28.0 * scale).round() as f32 * 0.28).round().max(4.0) as i32;
    let pad = (6.0 * scale).round() as i32;
    pad + m * 2 + (4.0 * scale).round() as i32
}

fn margins_for(scale: f32) -> isize {
    let l = search_left_margin(scale);
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

/// 输入框统一内边距（DIP）。全应用一个口径。
///
/// 第33轮之前是五处各写各的（地址栏 8/8、搜索框 26/8、重命名 4/4、
/// 批量重命名 6/6），同一个界面里输入框的文字起点差十几像素，看着就是
/// "没对齐"。搜索框的左边距要额外让出放大镜，由 `search_left_margin` 单独算。
pub(crate) const EDIT_PAD: i32 = 8;

/// 给任意 EDIT 套上"本应用的输入框"标准外观：UI 字体 + 统一内边距 +
/// 去掉系统那圈立体边框（暗色主题下 WS_EX_CLIENTEDGE 的白边非常突兀，
/// 跟自绘控件完全不是一路）。
pub(crate) fn style_edit(hwnd: HWND, scale: f32, left: i32, right: i32) {
    unsafe {
        // 去掉 WS_EX_CLIENTEDGE（0x200）：暗色主题下系统那圈立体白边非常突兀。
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex & !(WS_EX_CLIENTEDGE.0 as isize));
    }
    set_edit_font(hwnd, scale);
    set_edit_margins(hwnd, scale, left, right);
    // 改了 EXSTYLE/边距之后要重排一次，否则控件还按旧的边框留白
    unsafe {
        let _ = SendMessageW(hwnd, WM_SIZE, Some(WPARAM(0)), Some(LPARAM(0)));
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
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
        // 放大镜：**自己用椭圆 + 斜线画**，不用字体字形。
        //
        // 第33轮用户截图报"搜索栏扭曲成竖条"：原来是把 Segoe MDL2 的 U+E721
        // 当**文字**用 DrawTextW 画的，而字形字体的行高受 DT_SINGLELINE 限制，
        // 那个图标字符被压扁成了一条竖着的条纹。矢量画在物理上不可能畸变。
        let m = ((r.bottom - r.top) as f32 * 0.28).round().max(4.0) as i32; // 半径
        let lw = crate::icon_spec::stroke_px(12, scale);
        let cx = (6.0 * scale).round() as i32 + m;
        let cy = (r.top + r.bottom) / 2 - m / 4;   // 偏上一点点：给下方手柄留位，整体仍与文字齐平
        // 主题接管时：放大镜也走主题字形（第49轮）。占位宽度仍按 m 算，
        // 免得提示文字的起点随主题切换跳动。
        //
        // 第66轮：**尺寸查规格**（Xl，与导航键同档），不再用 `2*m+2` 那个
        // 跟着搜索框高度漂的动态值 —— 那是全软件唯一"尺寸随别的控件走"
        // 的图标，缩放一次它就变一次。
        let themed = draw_ui_icon(
            hdc,
            crate::icon_theme::Ui::Search,
            &RECT { left: cx - m, top: cy - m, right: cx + m, bottom: cy + m },
            crate::icon_spec::size_of(crate::icon_theme::Ui::Search, scale),
            t.text_dim,
        );
        if !themed {
            let pen = CreatePen(PS_SOLID, lw, t.text_dim);
            let op = SelectObject(hdc, pen.into());
            let ob = SelectObject(hdc, GetStockObject(NULL_BRUSH));
            let _ = Ellipse(hdc, cx - m, cy - m, cx + m, cy + m);
            // 手柄：从圆右下 45° 伸出去的那一小段
            let h = ((m as f32 * 0.9) as i32).max(3);
            let _ = MoveToEx(hdc, cx + m / 2, cy + m / 2, None);
            let _ = LineTo(hdc, cx + m / 2 + h, cy + m / 2 + h);
            let _ = SelectObject(hdc, ob);
            let _ = SelectObject(hdc, op);
            let _ = DeleteObject(pen.into());
        }
        if empty {
            // 提示文字：起点跟着**放大镜实际占位**（左内边距 + 直径 + 间隙），
            // 不能写死 26 —— 半径随 scale 变，写死就会出现字压在放大镜上。
            let text_left = cx + m + (4.0 * scale).round() as i32;
            let f = ui_font(scale);
            let old2 = SelectObject(hdc, f.into());
            SetTextColor(hdc, t.text_dim);
            let tr = RECT {
                left: text_left,
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
        style_edit(addr, app.metrics.scale, EDIT_PAD, EDIT_PAD);
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
        // 左边留出放大镜的位置（EC_LEFTMARGIN），否则输入的文字会压在图标下面。
        // 数值与 paint_search_decor 里画的放大镜同源（search_left_margin）。
        style_edit(
            search,
            app.metrics.scale,
            search_left_margin(app.metrics.scale),
            EDIT_PAD,
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
        // 两个子 EDIT 都要用 edit_vcenter 摆位：单行 EDIT 的字形单元格贴顶，
        // 摆满行高时文字偏上 ~5px（第40轮"所有输入框文字贴顶"）。
        let ar = edit_vcenter(&l.address, app.metrics.scale);
        let _ = SetWindowPos(app.hwnd_address, None, ar.left, ar.top, ar.right - ar.left, ar.bottom - ar.top, SWP_NOZORDER | SWP_NOACTIVATE);
        let sr = edit_vcenter(&l.search, app.metrics.scale);
        let _ = SetWindowPos(app.hwnd_search, None, sr.left, sr.top, sr.right - sr.left, sr.bottom - sr.top, SWP_NOZORDER | SWP_NOACTIVATE);
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
    let header_h = if app.tab().view_mode == zenfile_core::ViewMode::Details {
        (26.0 * app.metrics.scale) as i32
    } else {
        0
    };
    let y = l.view.top + header_h + app.tab().focus_row as i32 * row_h - app.tab().scroll;
    // 编辑框宽度跟着名字长度走，但**最长不得超过"名称"这一列的右沿**：
    // 以前写死 300px，短名字后面拖一条空框、长名字又顶出列外压到"修改日期"上。
    let x = l.view.left + (30.0 * app.metrics.scale) as i32;
    let max_right = if app.tab().view_mode == zenfile_core::ViewMode::Details {
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
            WINDOW_EX_STYLE(0), // 边框由 style_edit 统一去掉（第33轮：输入框视觉对齐）
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
        style_edit(hwnd, app.metrics.scale, EDIT_PAD, EDIT_PAD);
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
    // 第70轮埋点：地址框自己的 `WM_PAINT` / `WM_ERASEBKGND` 计时。
    //
    // 关键：子 EDIT 的 `WM_PAINT` **归不上任何动画帧**（`TRACE_PENDING`
    // 已经被主窗口那次 paint 认领走了），所以它只会累加分类计数，
    // 一帧的时刻都不会被污染 —— 这正是我们要的：既量到了它的代价，
    // 又不干扰外层时间线。
    //
    // `CHILD_ADDR` 在这里写死而不是靠 ID 反查：能走到 `addr_proc`
    // 的窗口按定义就是地址框，反查反而多一次 `GetDlgCtrlID`。
    //
    // 用一层 wrapper 而不是把 enter/exit 塞进分支里：`addr_proc` 有好几处
    // `return`，把配对语句撒在分支里一定会漏掉一条路径，那样 enter/exit
    // 就会错配、把下一次 paint 的耗时算到上一次上。
    if msg == WM_ERASEBKGND {
        crate::anim_clock::trace_child_erase(crate::anim_clock::CHILD_ADDR);
    } else if msg == WM_PAINT {
        crate::anim_clock::trace_child_paint_enter(crate::anim_clock::CHILD_ADDR);
        let r = addr_proc_inner(hwnd, msg, wparam, lparam);
        crate::anim_clock::trace_child_paint_exit(crate::anim_clock::CHILD_ADDR);
        return r;
    }
    addr_proc_inner(hwnd, msg, wparam, lparam)
}

unsafe extern "system" fn addr_proc_inner(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // 滚轮转发（第41轮）：WM_MOUSEWHEEL 发给**焦点窗口**，焦点在框里时
    // 主窗口收不到 → 列表滚不动，看着像卡死。主窗口按屏幕坐标取点，
    // lparam 原样转发即可。
    if msg == windows::Win32::UI::WindowsAndMessaging::WM_MOUSEWHEEL {
        if let Ok(parent) = windows::Win32::UI::WindowsAndMessaging::GetParent(hwnd) {
            if !parent.0.is_null() {
                let _ = SendMessageW(parent, msg, Some(wparam), Some(lparam));
                return LRESULT(0);
            }
        }
    }
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
    // 第70轮埋点：搜索框自己的 `WM_PAINT` / `WM_ERASEBKGND` 计时。
    //
    // `search_proc` 是本轮最该盯的子窗口：它比地址框多一层
    // `paint_search_decor`（放大镜 + 空态提示文字，每次都要拿 `GetDC`、
    // 画字、还窗），所以单次 paint 大概率比地址框贵。
    //
    // 与 `addr_proc` 同样用 wrapper 保证 enter/exit 配对（这个函数有
    // 5 处 `return`，配对语句不能撒在分支里）。
    if msg == WM_ERASEBKGND {
        crate::anim_clock::trace_child_erase(crate::anim_clock::CHILD_SEARCH);
    } else if msg == WM_PAINT {
        crate::anim_clock::trace_child_paint_enter(crate::anim_clock::CHILD_SEARCH);
        let r = search_proc_inner(hwnd, msg, wparam, lparam);
        crate::anim_clock::trace_child_paint_exit(crate::anim_clock::CHILD_SEARCH);
        return r;
    }
    search_proc_inner(hwnd, msg, wparam, lparam)
}

unsafe extern "system" fn search_proc_inner(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
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
    // 滚轮转发（第41轮）：WM_MOUSEWHEEL 发给**焦点窗口**，焦点在框里时
    // 主窗口收不到 → 列表滚不动，看着像卡死。主窗口按屏幕坐标取点，
    // lparam 原样转发即可。
    if msg == windows::Win32::UI::WindowsAndMessaging::WM_MOUSEWHEEL {
        if let Ok(parent) = windows::Win32::UI::WindowsAndMessaging::GetParent(hwnd) {
            if !parent.0.is_null() {
                let _ = SendMessageW(parent, msg, Some(wparam), Some(lparam));
                return LRESULT(0);
            }
        }
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
    // 第70轮埋点：重命名框归入 `CHILD_OTHER`。
    //
    // 它平时不存在（只有 F2 才建），录制期间若没按 F2 计数恒为 0 ——
    // 这本身也是有用信息：说明"每帧那两次额外重绘"**不来自**重命名框。
    if msg == WM_ERASEBKGND {
        crate::anim_clock::trace_child_erase(crate::anim_clock::CHILD_OTHER);
    } else if msg == WM_PAINT {
        crate::anim_clock::trace_child_paint_enter(crate::anim_clock::CHILD_OTHER);
        let r = rename_proc_inner(hwnd, msg, wparam, lparam);
        crate::anim_clock::trace_child_paint_exit(crate::anim_clock::CHILD_OTHER);
        return r;
    }
    rename_proc_inner(hwnd, msg, wparam, lparam)
}

unsafe extern "system" fn rename_proc_inner(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
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
        // **焦点必须显式交出去**（第33轮用户报"单击目录栏变成输入的样子，
        // 无法退出"）。EDIT 是 WS_CHILD：它隐藏之后焦点仍然停在它身上，
        // 键盘消息全进了一个不可见的窗口 —— 表现为"打不了字、Esc 也无效、
        // 点内容区也没反应"，界面永远卡在编辑态。
        //
        // `refocus=false`（WM_KILLFOCUS 来的）时不能抢焦点，但**父窗口必须
        // 拿到键盘**：`WM_NEXTFOCUS` 让系统自己把焦点顺着 Z 序给下一个窗口，
        // 父窗口就是下一个。
        if refocus {
            let _ = SetFocus(Some(app.hwnd));
        }
        let _ = ShowWindow(app.hwnd_address, SW_HIDE);
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
    //
    // 第75轮：这段计算已原样提进 `App::address_display_text()`，让"写进 EDIT
    // 的文本"与"地址框失效判据用的文本"共用同一个函数 —— 否则两者将来必然
    // 会各自漂移，漂移的那一边就会漏画或白画。
    set_window_text_utf16(app.hwnd_address, &app.address_display_text());
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
