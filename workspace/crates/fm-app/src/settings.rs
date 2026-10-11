//! 设置页（第 19 轮第 1 项：全部重做为可视化界面）。
//!
//! 顶部五个小标签：**界面 / 右键菜单 / 高级 / 插件 / 关于**。
//!
//! * 「界面」与「右键菜单」是**可拖拽的排布编辑器**：拖卡片改顺序，拖进底部
//!   的「更多」区即隐藏该组件（被隐藏的组件以原始样式陈列在「更多」里，
//!   再拖回来就恢复）。排布结果落在 `%APPDATA%\ZenFile\ui_settings.json`。
//! * 「高级」沿用原来的开关/下拉；
//! * 「插件」列出子模块并可直接启用/停用；
//! * 「关于」写软件自身的信息 —— 菜单里的「关于」现在**跳转到这里**，不再弹窗。
//!
//! 几何纪律照旧：绘制与命中测试共用同一个 `geom()`，绝不两边各写一份常量。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, DrawTextW, FillRect, RoundRect, SelectObject, SetBkMode,
    SetTextColor, DT_CENTER, DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HBRUSH,
    TRANSPARENT,
};

use crate::app::{App, SETTINGS_TABS};
use crate::paint::{self, Layout};
use crate::theme::Theme;

// ---------------------------------------------------------------- 组件/条目表

/// 界面组件。下标即 id，与 `ui_order` / `ui_hidden` 里的数字一一对应。
pub const UI_PART_NAMES: [&str; 9] = [
    "标题栏", "标签栏", "地址栏", "搜索框", "工具栏", "导航窗格", "内容区", "状态栏", "详情面板",
];
pub const UI_PART_GLYPHS: [&str; 9] = [
    "\u{E7FD}", "\u{E8A7}", "\u{E8B7}", "\u{E721}", "\u{E80A}", "\u{E8B7}", "\u{E8A5}",
    "\u{E75B}", "\u{E9E9}",
];
/// 固定组件：不允许拖进「更多」（没有它们这个窗口就不成立了）。
pub const UI_PART_FIXED: [bool; 9] = [true, false, false, false, false, false, true, false, false];

/// 右键菜单条目。下标即 id。
pub const MENU_ITEM_NAMES: [&str; 14] = [
    "打开", "打开方式", "剪切", "复制", "粘贴", "重命名", "批量重命名", "删除", "新建", "全选",
    "刷新", "查看", "排序方式", "属性",
];
pub const MENU_ITEM_GLYPHS: [&str; 14] = [
    "\u{E8A7}", "\u{E7AC}", "\u{E8C6}", "\u{E8C8}", "\u{E77F}", "\u{E8AC}", "\u{E8AC}", "\u{E74D}",
    "\u{E8F4}", "\u{E762}", "\u{E72C}", "\u{E7B3}", "\u{E8CB}", "\u{E946}",
];

// ---------------------------------------------------------------- 持久化

#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    /// 「界面」里可见组件的排列顺序
    pub ui_order: Vec<u8>,
    /// 被拖进「更多」的组件
    pub ui_hidden: Vec<u8>,
    /// 「右键菜单」里可见条目的排列顺序
    pub menu_order: Vec<u16>,
    /// 被拖进「更多」的菜单条目
    pub menu_hidden: Vec<u16>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            ui_order: (0..UI_PART_NAMES.len() as u8).collect(),
            ui_hidden: Vec::new(),
            menu_order: (0..MENU_ITEM_NAMES.len() as u16).collect(),
            menu_hidden: Vec::new(),
        }
    }
}

impl Settings {
    /// 补齐/去重：配置被手改坏或组件表变长时都能自愈，不至于崩在下标越界上。
    pub fn normalize(&mut self) {
        let n = UI_PART_NAMES.len() as u8;
        self.ui_order.retain(|i| *i < n && !self.ui_hidden.contains(i));
        for i in 0..n {
            if !self.ui_order.contains(&i) && !self.ui_hidden.contains(&i) {
                self.ui_order.push(i);
            }
        }
        self.ui_hidden.retain(|i| *i < n && !self.ui_order.contains(i));
        self.ui_hidden.sort_unstable();
        self.ui_hidden.dedup();

        let m = MENU_ITEM_NAMES.len() as u16;
        self.menu_order.retain(|i| *i < m && !self.menu_hidden.contains(i));
        for i in 0..m {
            if !self.menu_order.contains(&i) && !self.menu_hidden.contains(&i) {
                self.menu_order.push(i);
            }
        }
        self.menu_hidden.retain(|i| *i < m && !self.menu_order.contains(i));
        self.menu_hidden.sort_unstable();
        self.menu_hidden.dedup();
    }

    pub fn path() -> Option<PathBuf> {
        let base = std::env::var_os("APPDATA").map(PathBuf::from)?;
        Some(base.join("ZenFile").join("ui_settings.json"))
    }

    pub fn load() -> Settings {
        let mut s = Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<Settings>(&t).ok())
            .unwrap_or_default();
        s.normalize();
        s
    }

    pub fn save(&self) {
        let Some(p) = Self::path() else { return };
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        match serde_json::to_string_pretty(self) {
            Ok(t) => {
                let _ = std::fs::write(&p, t);
            }
            Err(e) => eprintln!("排布保存失败：{e}"),
        }
    }
}

// ---------------------------------------------------------------- 几何

/// 一次排布计算的结果。绘制与命中测试**共用这一份**。
pub struct Geom {
    pub tabs: [RECT; SETTINGS_TABS.len()],
    pub content: RECT,
    /// 当前标签下的卡片（顺序即排布顺序）
    pub cards: Vec<RECT>,
    pub card_ids: Vec<u32>,
    /// 底部「更多」区
    pub dock: RECT,
    /// 「更多」里陈列的被隐藏组件
    pub chips: Vec<RECT>,
    pub chip_ids: Vec<u32>,
    /// 「高级」/「插件」的普通行（复用 paint::SettingRow 的绘制）
    pub rows: Vec<paint::SettingRow>,
}

/// 命中结果。
pub enum Hit {
    /// 顶部第 n 个小标签
    Tab(u8),
    /// 第 n 张卡片（可拖）
    Card(usize),
    /// 「更多」区里第 n 个被隐藏项（可拖回来）
    Chip(usize),
    /// 「高级」行：动作编号（paint::SET_*）
    Row(u32),
}

/// 排布编辑器里正在被拖的那一张。
pub struct SettingsDrag {
    /// 哪个标签（界面 / 右键菜单）
    pub tab: u8,
    /// 组件或菜单条目的 id
    pub id: u32,
    /// 按下时的鼠标位置（窗口坐标），用来算位移
    pub grab: (i32, i32),
    /// 当前鼠标位置
    pub cur: (i32, i32),
    /// 卡片原本的矩形：拖动时按位移平移它，视觉上就是整块跟着走
    pub rect: RECT,
    /// 是否是从「更多」区里拖出来的（拖回去 = 恢复显示）
    pub from_dock: bool,
}

pub fn geom(app: &App, l: &Layout) -> Geom {
    let s = app.metrics.scale;
    let view = l.view;
    let px = |v: f32| (v * s).round() as i32;

    // —— 顶部小标签 ——
    let tab_h = px(30.0);
    let mut tabs = [RECT::default(); SETTINGS_TABS.len()];
    let mut x = view.left + px(16.0);
    let top = view.top + px(10.0);
    let hf = paint::ui_font(s);
    let hdc = unsafe { windows::Win32::Graphics::Gdi::GetDC(Some(app.hwnd)) };
    for (i, name) in SETTINGS_TABS.iter().enumerate() {
        let w = if hdc.0.is_null() {
            px(70.0)
        } else {
            let old = unsafe { SelectObject(hdc, hf.into()) };
            let w = unsafe { paint::text_width(hdc, hf, name) };
            unsafe {
                SelectObject(hdc, old);
            }
            w + px(26.0)
        };
        tabs[i] = RECT { left: x, top, right: x + w, bottom: top + tab_h };
        x += w + px(6.0);
    }
    if !hdc.0.is_null() {
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::ReleaseDC(Some(app.hwnd), hdc);
        }
    }
    let _ = hf;

    let content = RECT {
        left: view.left + px(16.0),
        top: top + tab_h + px(12.0),
        right: view.right - px(16.0),
        bottom: view.bottom - px(10.0),
    };

    let mut g = Geom {
        tabs,
        content,
        cards: Vec::new(),
        card_ids: Vec::new(),
        dock: RECT::default(),
        chips: Vec::new(),
        chip_ids: Vec::new(),
        rows: Vec::new(),
    };

    match app.settings_tab {
        crate::app::SETTINGS_TAB_UI | crate::app::SETTINGS_TAB_MENU => {
            let dock_h = px(84.0);
            g.dock = RECT {
                left: content.left,
                top: content.bottom - dock_h,
                right: content.right,
                bottom: content.bottom,
            };
            let list_bottom = g.dock.top - px(12.0);
            let card_h = px(38.0);
            let gap = px(6.0);
            let ids: Vec<u32> = if app.settings_tab == crate::app::SETTINGS_TAB_UI {
                app.ui_settings.ui_order.iter().map(|i| *i as u32).collect()
            } else {
                app.ui_settings.menu_order.iter().map(|i| *i as u32).collect()
            };
            let mut y = content.top;
            for id in &ids {
                if y + card_h > list_bottom {
                    break;
                }
                g.cards.push(RECT { left: content.left, top: y, right: content.right, bottom: y + card_h });
                g.card_ids.push(*id);
                y += card_h + gap;
            }
            // 「更多」里的被隐藏项：横向流式排布，排满换行（超出的裁掉）
            let hidden: Vec<u32> = if app.settings_tab == crate::app::SETTINGS_TAB_UI {
                app.ui_settings.ui_hidden.iter().map(|i| *i as u32).collect()
            } else {
                app.ui_settings.menu_hidden.iter().map(|i| *i as u32).collect()
            };
            let cw = px(120.0);
            let ch = px(28.0);
            let cg = px(8.0);
            let mut cx = g.dock.left + px(12.0);
            let mut cy = g.dock.top + px(30.0);
            for id in hidden {
                if cx + cw > g.dock.right - px(12.0) {
                    cx = g.dock.left + px(12.0);
                    cy += ch + cg;
                }
                if cy + ch > g.dock.bottom - px(6.0) {
                    break;
                }
                g.chips.push(RECT { left: cx, top: cy, right: cx + cw, bottom: cy + ch });
                g.chip_ids.push(id);
                cx += cw + cg;
            }
        }
        crate::app::SETTINGS_TAB_ADVANCED => {
            g.rows = advanced_rows(app, &content);
        }
        crate::app::SETTINGS_TAB_PLUGIN => {
            g.rows = plugin_rows(app, &content);
        }
        _ => {}
    }
    g
}

pub fn hit(app: &App, l: &Layout, x: i32, y: i32) -> Option<Hit> {
    let g = geom(app, l);
    for (i, r) in g.tabs.iter().enumerate() {
        if inside(r, x, y) {
            return Some(Hit::Tab(i as u8));
        }
    }
    for (i, r) in g.cards.iter().enumerate() {
        if inside(r, x, y) {
            return Some(Hit::Card(i));
        }
    }
    for (i, r) in g.chips.iter().enumerate() {
        if inside(r, x, y) {
            return Some(Hit::Chip(i));
        }
    }
    for r in &g.rows {
        if r.action != 0 && inside(&r.rect, x, y) {
            return Some(Hit::Row(r.action));
        }
    }
    None
}

fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

// ---------------------------------------------------------------- 行定义

fn row(rect: RECT, label: &str, value: String, action: u32, toggle: Option<bool>) -> paint::SettingRow {
    paint::SettingRow {
        rect,
        label: label.to_string(),
        value,
        action,
        on: toggle.unwrap_or(false),
        toggle,
    }
}

/// 「高级」：沿用原来的开关与多选项，只是搬进新标签。
fn advanced_rows(app: &App, content: &RECT) -> Vec<paint::SettingRow> {
    let s = app.metrics.scale;
    let rh = (34.0 * s).round() as i32;
    let items: Vec<(&str, String, u32, Option<bool>)> = vec![
        ("显示隐藏的项目", String::new(), paint::SET_HIDDEN, Some(app.show_hidden)),
        ("显示受保护的系统文件", String::new(), paint::SET_SYSTEM, Some(app.show_system)),
        ("显示文件扩展名", String::new(), paint::SET_EXT, Some(app.cfg.files.view.show_extensions)),
        ("显示导航窗格", String::new(), paint::SET_SIDEBAR, Some(app.cfg.ui.sidebar_visible)),
        ("显示状态栏", String::new(), paint::SET_STATUS, Some(app.cfg.ui.statusbar_visible)),
        ("导航窗格跟随当前文件夹展开", String::new(), paint::SET_FOLLOW, Some(app.cfg.ui.sidebar_follow_current)),
        ("「此电脑」默认展开到盘符", String::new(), paint::SET_THISPC, Some(app.cfg.ui.sidebar_thispc_expanded)),
        (
            "大小单位",
            app.cfg.files.view.size_unit_of().label().to_string(),
            paint::SET_UNIT,
            None,
        ),
        (
            "新标签默认视图",
            crate::app::default_view_label(&app.cfg.files.view.default_mode).to_string(),
            paint::SET_VIEW,
            None,
        ),
    ];
    let mut out = Vec::new();
    let mut y = content.top;
    for (label, value, action, toggle) in items {
        out.push(
            row(
                RECT { left: content.left, top: y, right: content.right, bottom: y + rh },
                label,
                value,
                action,
                toggle,
            ),
        );
        y += rh + (4.0 * s).round() as i32;
    }
    // 搜索语法速查：高级语法写在搜索框里，UI 上没有任何入口的话用户永远发现不了。
    // action = 0 → 不可点（hit() 会跳过），纯粹是说明文字。
    y += (14.0 * s).round() as i32;
    for (label, value) in [
        ("搜索语法", ""),
        ("  报告 预算", "多个词 = 同时满足"),
        ("  \"年度 报告\"", "引号内当一个整体"),
        ("  a OR b", "满足任一即可"),
        ("  -草稿", "排除含「草稿」的"),
        ("  *.txt  item?.log", "* 与 ? 通配"),
        ("  ext:txt", "按扩展名"),
        ("  kind:file  kind:folder", "按类型（也认 file/dir/文件/目录）"),
        ("  size:>10mb", "> < >= <=，单位 b/kb/mb/gb/tb"),
        ("  date:>2026-01-01", "也认 today / week"),
        ("  path:github", "完整路径里含"),
    ] {
        out.push(
            row(
                RECT { left: content.left, top: y, right: content.right, bottom: y + rh },
                label,
                value.to_string(),
                0,
                None,
            ),
        );
        y += rh + (4.0 * s).round() as i32;
    }
    // 卷索引状态：顺手把"秒搜"的依据摆出来
    y += (10.0 * s).round() as i32;
    let (label, value) = match (&app.index_state, app.index.as_ref()) {
        (_, Some(idx)) => (
            "卷索引",
            format!("{} · {} 条 · 上次查询 {:.1} ms", idx.root, idx.len(), app.search_ms),
        ),
        (crate::app::IndexState::Building(n), _) => ("卷索引", format!("正在建立…（已扫 {n} 项）")),
        (crate::app::IndexState::Loading, _) => ("卷索引", "正在载入缓存…".to_string()),
        (crate::app::IndexState::Failed(why), _) => ("卷索引", format!("不可用（{why}）")),
        _ => ("卷索引", "尚未建立（搜索时自动开始）".to_string()),
    };
    out.push(row(
        RECT { left: content.left, top: y, right: content.right, bottom: y + rh },
        label,
        value,
        0,
        None,
    ));
    out
}

/// 「插件」：列出子模块，可直接开关。
fn plugin_rows(app: &App, content: &RECT) -> Vec<paint::SettingRow> {
    let s = app.metrics.scale;
    let rh = (34.0 * s).round() as i32;
    let mut out = Vec::new();
    let mut y = content.top;
    if app.cfg.modules.is_empty() {
        out.push(row(
            RECT { left: content.left, top: y, right: content.right, bottom: y + rh },
            "没有已注册的插件模块",
            String::new(),
            0,
            None,
        ));
        return out;
    }
    for (id, m) in &app.cfg.modules {
        // 动作编号用 1000 + 模块下标，命中后反查 id
        let idx = app.cfg.modules.keys().position(|k| k == id).unwrap_or(0);
        out.push(row(
            RECT { left: content.left, top: y, right: content.right, bottom: y + rh },
            &format!("{id}（fm-{id}.exe）"),
            String::new(),
            1000 + idx as u32,
            Some(m.enabled),
        ));
        y += rh + (4.0 * s).round() as i32;
    }
    out
}

// ---------------------------------------------------------------- 绘制

pub fn paint(app: &mut App, hdc: HDC, l: &Layout) {
    let g = geom(app, l);

    let s = app.metrics.scale;
    let th: Theme = app.theme;

    // 顶部小标签
    for (i, r) in g.tabs.iter().enumerate() {
        let active = app.settings_tab == i as u8;
        let hover = app.settings_hover_tab == Some(i as u8);
        let fill = if active {
            th.surface_alt
        } else if hover {
            th.row_hover
        } else {
            th.surface
        };
        fill_rect(hdc, r, fill);
        if active {
            // 选中：底部一条强调色，与窗口标题栏的标签语言一致
            let bar = RECT {
                left: r.left + 4,
                top: r.bottom - (3.0 * s).round() as i32,
                right: r.right - 4,
                bottom: r.bottom,
            };
            fill_rect(hdc, &bar, th.accent);
        }
        text(
            hdc,
            r,
            SETTINGS_TABS[i],
            if active { th.text } else { th.text_dim },
            paint::ui_font(s),
        );
    }

    match app.settings_tab {
        crate::app::SETTINGS_TAB_UI | crate::app::SETTINGS_TAB_MENU => {
            paint_editor(app, hdc, &g, th, s);
        }
        crate::app::SETTINGS_TAB_ADVANCED | crate::app::SETTINGS_TAB_PLUGIN => {
            for r in &g.rows {
                paint_row(app, hdc, r, th, s);
            }
        }
        _ => paint_about(app, hdc, &g, th, s),
    }

    // 正在被拖的那张卡片画在最上层
    if let Some(d) = &app.settings_drag {
        let dx = d.cur.0 - d.grab.0;
        let dy = d.cur.1 - d.grab.1;
        let base = d.rect;
        let r = RECT {
            left: base.left + dx,
            top: base.top + dy,
            right: base.right + dx,
            bottom: base.bottom + dy,
        };
        let (name, glyph) = label_of(d.tab, d.id);
        draw_card(hdc, &r, &name, &glyph, th, s, true, false);
    }
}

/// 排布编辑器：卡片列表 + 底部「更多」区。
fn paint_editor(app: &App, hdc: HDC, g: &Geom, th: Theme, s: f32) {
    for (i, r) in g.cards.iter().enumerate() {
        let id = g.card_ids[i];
        let (name, glyph) = label_of(app.settings_tab, id);
        let dragging = matches!(&app.settings_drag, Some(d) if !d.from_dock && d.id == id);
        let hover = app.settings_hover_card == Some(id) && !dragging;
        draw_card(hdc, r, &name, &glyph, th, s, false, hover);
    }

    // 「更多」区
    fill_rect(hdc, &g.dock, th.surface_alt);
    let hint = RECT {
        left: g.dock.left + (12.0 * s).round() as i32,
        top: g.dock.top + (6.0 * s).round() as i32,
        right: g.dock.right,
        bottom: g.dock.top + (26.0 * s).round() as i32,
    };
    set_color(hdc, th.text_dim);
    paint::draw_text(
        hdc,
        "更多 — 拖到这里即隐藏；从这儿拖回去即恢复",
        hint,
        DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS | DT_VCENTER,
    );
    for (i, r) in g.chips.iter().enumerate() {
        let id = g.chip_ids[i];
        let (name, glyph) = label_of(app.settings_tab, id);
        let dragging = matches!(&app.settings_drag, Some(d) if d.from_dock && d.id == id);
        draw_chip(hdc, r, &name, &glyph, th, s, dragging);
    }
}

fn label_of(tab: u8, id: u32) -> (String, &'static str) {
    if tab == crate::app::SETTINGS_TAB_UI {
        let i = id as usize;
        (
            UI_PART_NAMES.get(i).unwrap_or(&"?").to_string(),
            UI_PART_GLYPHS.get(i).copied().unwrap_or("\u{E783}"),
        )
    } else {
        let i = id as usize;
        (
            MENU_ITEM_NAMES.get(i).unwrap_or(&"?").to_string(),
            MENU_ITEM_GLYPHS.get(i).copied().unwrap_or("\u{E783}"),
        )
    }
}

/// 一张卡片：原始样式 = 行底色 + 左侧图标 + 名称（与列表里看到的观感一致）。
fn draw_card(hdc: HDC, r: &RECT, name: &str, glyph: &str, th: Theme, s: f32, dragged: bool, hover: bool) {
    let fill = if dragged {
        th.accent
    } else if hover {
        th.row_hover
    } else {
        th.surface
    };
    fill_rect(hdc, r, fill);
    let glyph_r = RECT {
        left: r.left + (10.0 * s).round() as i32,
        top: r.top,
        right: r.left + (38.0 * s).round() as i32,
        bottom: r.bottom,
    };
    set_color(hdc, if dragged { th.selection_text } else { th.text_dim });
    let gf = paint::glyph_font(s);
    unsafe {
        SelectObject(hdc, gf.into());
    }
    paint::draw_text(
        hdc,
        glyph,
        glyph_r,
        DT_SINGLELINE | DT_NOPREFIX | DT_CENTER | DT_VCENTER,
    );
    set_color(hdc, if dragged { th.selection_text } else { th.text });
    let name_r = RECT {
        left: r.left + (42.0 * s).round() as i32,
        top: r.top,
        right: r.right - (60.0 * s).round() as i32,
        bottom: r.bottom,
    };
    let uf = paint::ui_font(s);
    unsafe {
        SelectObject(hdc, uf.into());
    }
    paint::draw_text(hdc, name, name_r, DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS);
    // 右侧"抓手"提示
    set_color(hdc, if dragged { th.selection_text } else { th.text_dim });
    let grip = RECT { left: r.right - (34.0 * s).round() as i32, top: r.top, right: r.right - (10.0 * s).round() as i32, bottom: r.bottom };
    paint::draw_text(hdc, "\u{E76F}", grip, DT_SINGLELINE | DT_NOPREFIX | DT_CENTER | DT_VCENTER);
}

/// 「更多」里的一枚 chip：同样保留原始样式（图标 + 名称）。
fn draw_chip(hdc: HDC, r: &RECT, name: &str, glyph: &str, th: Theme, s: f32, dragged: bool) {
    fill_rect(hdc, r, if dragged { th.accent } else { th.surface });
    let glyph_r = RECT { left: r.left + (6.0 * s).round() as i32, top: r.top, right: r.left + (28.0 * s).round() as i32, bottom: r.bottom };
    set_color(hdc, if dragged { th.selection_text } else { th.text_dim });
    unsafe {
        SelectObject(hdc, paint::glyph_font(s).into());
    }
    paint::draw_text(hdc, glyph, glyph_r, DT_SINGLELINE | DT_NOPREFIX | DT_CENTER | DT_VCENTER);
    set_color(hdc, if dragged { th.selection_text } else { th.text });
    let name_r = RECT { left: r.left + (30.0 * s).round() as i32, top: r.top, right: r.right - (4.0 * s).round() as i32, bottom: r.bottom };
    unsafe {
        SelectObject(hdc, paint::ui_font(s).into());
    }
    paint::draw_text(hdc, name, name_r, DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS);
}

fn paint_row(app: &App, hdc: HDC, r: &paint::SettingRow, th: Theme, s: f32) {
    fill_rect(hdc, &r.rect, th.surface);
    set_color(hdc, th.text);
    let label_r = RECT {
        left: r.rect.left + (12.0 * s).round() as i32,
        top: r.rect.top,
        right: r.rect.right - (140.0 * s).round() as i32,
        bottom: r.rect.bottom,
    };
    unsafe {
        SelectObject(hdc, paint::ui_font(s).into());
    }
    paint::draw_text(hdc, &r.label, label_r, DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS);
    match r.toggle {
        Some(on) => paint::draw_toggle(hdc, r, app, on),
        None => {
            let vr = paint::setting_value_rect(r, s);
            fill_rect(hdc, &vr, th.surface_alt);
            set_color(hdc, th.text);
            unsafe {
                SelectObject(hdc, paint::ui_font(s).into());
            }
            paint::draw_text(hdc, &r.value, vr, DT_SINGLELINE | DT_NOPREFIX | DT_CENTER | DT_VCENTER);
        }
    }
}

/// 「关于」：软件自身的信息。
fn paint_about(app: &App, hdc: HDC, g: &Geom, th: Theme, s: f32) {
    let s2 = s;
    let mut y = g.content.top + (8.0 * s2).round() as i32;
    let big = (crate::paint::header_font(s2));
    set_color(hdc, th.text);
    unsafe {
        SelectObject(hdc, big.into());
    }
    let title = format!("{} {}", crate::APP_TITLE, crate::APP_VERSION);
    paint::draw_text(
        hdc,
        &title,
        RECT { left: g.content.left, top: y, right: g.content.right, bottom: y + (34.0 * s2).round() as i32 },
        DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER,
    );
    y += (44.0 * s2).round() as i32;

    let cfg_dir = fm_core::config::app_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "（不可用）".to_string());
    let idx_dir = std::env::var_os("LOCALAPPDATA")
        .map(|p| PathBuf::from(p).join("ZenFile").join("index"))
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "（不可用）".to_string());
    let lines = vec![
        format!("轻量资源管理器 · {}", crate::APP_VERSION),
        String::new(),
        "渲染：Win32 + GDI 自绘，无 WebView2、无 UI 框架".to_string(),
        "搜索：自建文件名索引（卷级缓存）+ 现地递归兜底".to_string(),
        "文件操作：自研多线程传输引擎（可暂停/取消）".to_string(),
        String::new(),
        format!("配置目录：{cfg_dir}"),
        format!("索引缓存：{idx_dir}"),
        String::new(),
        format!("窗口数：{}（本进程）", unsafe { crate::WINDOWS.len() }),
    ];
    unsafe {
        SelectObject(hdc, paint::ui_font(s2).into());
    }
    for line in lines {
        set_color(hdc, if line.is_empty() { th.text } else { th.text_dim });
        paint::draw_text(
            hdc,
            &line,
            RECT { left: g.content.left, top: y, right: g.content.right, bottom: y + (24.0 * s2).round() as i32 },
            DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS,
        );
        y += (24.0 * s2).round() as i32;
    }
}

// ---------------------------------------------------------------- 小工具

fn fill_rect(hdc: HDC, r: &RECT, color: COLORREF) {
    unsafe {
        let br: HBRUSH = CreateSolidBrush(color);
        FillRect(hdc, r, br);
        let _ = DeleteObject(br.into());
    }
}

fn set_color(hdc: HDC, c: COLORREF) {
    unsafe {
        SetTextColor(hdc, c);
        SetBkMode(hdc, windows::Win32::Graphics::Gdi::TRANSPARENT);
    }
}

fn text(hdc: HDC, r: &RECT, s: &str, color: COLORREF, font: windows::Win32::Graphics::Gdi::HFONT) {
    set_color(hdc, color);
    unsafe {
        SelectObject(hdc, font.into());
        let mut buf: Vec<u16> = s.encode_utf16().collect();
        let mut rr = *r;
        DrawTextW(hdc, &mut buf, &mut rr, DT_SINGLELINE | DT_NOPREFIX | DT_CENTER | DT_VCENTER);
    }
}

/// 圆角矩形描边（分隔/边框用；这里只画框不填充，避免盖住底色层次）。
#[allow(dead_code)]
fn stroke_round(hdc: HDC, r: &RECT, color: COLORREF, _s: f32) {
    unsafe {
        let br = windows::Win32::Graphics::Gdi::CreateSolidBrush(color);
        let old = SelectObject(hdc, br.into());
        RoundRect(hdc, r.left, r.top, r.right, r.bottom, 6, 6);
        SelectObject(hdc, old);
        let _ = DeleteObject(br.into());
    }
}

// ---------------------------------------------------------------- 拖拽落点

/// 固定组件（标题栏 / 内容区）不允许被收进「更多」——没有它们窗口就不成立了。
fn is_fixed(tab: u8, id: u32) -> bool {
    tab == crate::app::SETTINGS_TAB_UI
        && UI_PART_FIXED.get(id as usize).copied().unwrap_or(true)
}

/// 开始拖一张卡（列表里的或「更多」里的）。
pub fn begin_drag(app: &mut App, l: &Layout, hit: &Hit, x: i32, y: i32) -> bool {
    let g = geom(app, l);
    let (id, rect, from_dock) = match hit {
        Hit::Card(i) => match (g.card_ids.get(*i), g.cards.get(*i)) {
            (Some(id), Some(r)) => (*id, *r, false),
            _ => return false,
        },
        Hit::Chip(i) => match (g.chip_ids.get(*i), g.chips.get(*i)) {
            (Some(id), Some(r)) => (*id, *r, true),
            _ => return false,
        },
        _ => return false,
    };
    app.settings_drag = Some(SettingsDrag {
        tab: app.settings_tab,
        id,
        grab: (x, y),
        cur: (x, y),
        rect,
        from_dock,
    });
    true
}

/// 拖拽结束：落在「更多」区就隐藏，落在列表里就插到那个位置。
pub fn finish_drag(app: &mut App, l: &Layout, x: i32, y: i32) {
    let Some(d) = app.settings_drag.take() else { return };
    let g = geom(app, l);
    // 「高级 / 插件 / 关于」没有「更多」区，dock 是全零的占位矩形
    let has_dock = g.dock.right > g.dock.left && g.dock.bottom > g.dock.top;
    let in_dock = has_dock
        && x >= g.dock.left
        && x < g.dock.right
        && y >= g.dock.top
        && y < g.dock.bottom;
    if d.from_dock {
        // 从「更多」拖回列表区 = 恢复显示
        if !in_dock {
            restore(app, d.tab, d.id);
        }
        return;
    }
    if in_dock {
        hide(app, d.tab, d.id);
        return;
    }
    let mut target = g.cards.len();
    for (i, r) in g.cards.iter().enumerate() {
        if y < (r.top + r.bottom) / 2 {
            target = i;
            break;
        }
    }
    reorder(app, d.tab, d.id, target);
}

/// 收进「更多」：从可见序列里摘掉，加进隐藏清单。
pub fn hide(app: &mut App, tab: u8, id: u32) {
    if is_fixed(tab, id) {
        return;
    }
    if tab == crate::app::SETTINGS_TAB_UI {
        let i = id as u8;
        app.ui_settings.ui_order.retain(|v| *v != i);
        if !app.ui_settings.ui_hidden.contains(&i) {
            app.ui_settings.ui_hidden.push(i);
        }
    } else {
        let i = id as u16;
        app.ui_settings.menu_order.retain(|v| *v != i);
        if !app.ui_settings.menu_hidden.contains(&i) {
            app.ui_settings.menu_hidden.push(i);
        }
    }
    after_change(app);
}

/// 从「更多」拖回来：恢复显示，插到末尾。
pub fn restore(app: &mut App, tab: u8, id: u32) {
    if tab == crate::app::SETTINGS_TAB_UI {
        let i = id as u8;
        app.ui_settings.ui_hidden.retain(|v| *v != i);
        if !app.ui_settings.ui_order.contains(&i) {
            app.ui_settings.ui_order.push(i);
        }
    } else {
        let i = id as u16;
        app.ui_settings.menu_hidden.retain(|v| *v != i);
        if !app.ui_settings.menu_order.contains(&i) {
            app.ui_settings.menu_order.push(i);
        }
    }
    after_change(app);
}

/// 改顺序：把 `id` 挪到可见序列的第 `target` 个位置。
pub fn reorder(app: &mut App, tab: u8, id: u32, target: usize) {
    if tab == crate::app::SETTINGS_TAB_UI {
        let i = id as u8;
        let o = &mut app.ui_settings.ui_order;
        let old = match o.iter().position(|v| *v == i) {
            Some(p) => p,
            None => return,
        };
        o.remove(old);
        let t = target.min(o.len());
        o.insert(t, i);
    } else {
        let i = id as u16;
        let o = &mut app.ui_settings.menu_order;
        let old = match o.iter().position(|v| *v == i) {
            Some(p) => p,
            None => return,
        };
        o.remove(old);
        let t = target.min(o.len());
        o.insert(t, i);
    }
    after_change(app);
}

fn after_change(app: &mut App) {
    app.ui_settings.normalize();
    app.ui_settings.save();
    apply_visibility(app);
    app.invalidate();
}

/// 把"被收进「更多」的组件"真正作用到配置上。
///
/// 目前导航窗格与状态栏有真实开关，收进「更多」就等于关掉它们（并写回用户层）。
/// 其余组件（标签栏/地址栏/搜索框/工具栏/详情面板）暂时没有独立显示开关，
/// 排布照样记下来，等开关接上即可生效 —— 这里不假装已经生效。
pub fn apply_visibility(app: &mut App) {
    let h = app.ui_settings.ui_hidden.clone();
    let sidebar = !h.contains(&5);
    let status = !h.contains(&7);
    let mut changed = false;
    if app.cfg.ui.sidebar_visible != sidebar {
        app.cfg.ui.sidebar_visible = sidebar;
        changed = true;
    }
    if app.cfg.ui.statusbar_visible != status {
        app.cfg.ui.statusbar_visible = status;
        changed = true;
    }
    if changed {
        app.persist_ui_pref();
        crate::paint::reposition_chrome(app);
        let _ = unsafe { windows::Win32::Graphics::Gdi::InvalidateRect(Some(app.hwnd), None, true) };
    }
}
