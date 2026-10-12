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

// ---------------------------------------------------------------- 组件表
//
// 「界面」页不再是"卡片列表"，而是**这个软件窗口的实时预览**，组件按长短
// 分三条带围在预览四周：
//   · 上面：文字只有几个字的**小组件**（按钮）—— 新建 / 剪切 / 复制 / 标签…
//   · 右边：长一点的 —— 侧栏那些横条、地址栏、详情页属性行
//   · 最下面：**最长的整条** —— 标题栏 / 工具栏 / 状态栏
// 三条整条里装的东西就是"组件里的子组件"，能拖进来、拖出去、拖着换位置，
// 中间的预览当场跟着变。

pub struct Widget {
    pub name: &'static str,
    pub glyph: &'static str,
    /// 自然宽度（DIP）
    pub w: f32,
    /// 0 = 从左往右排；1 = 靠右排（最小化 / 最大化 / 关闭这类）
    pub align: u8,
    /// false = 短组件（画成按钮）；true = 长组件（画成横条）
    pub long: bool,
}

const fn wg(name: &'static str, glyph: &'static str, wd: f32, align: u8, long: bool) -> Widget {
    Widget { name, glyph, w: wd, align, long }
}

pub const WIDGETS: [Widget; 29] = [
    wg("标签", "\u{E8A7}", 56.0, 0, false),
    wg("新建标签", "\u{E710}", 26.0, 0, false),
    wg("最小化", "\u{E921}", 24.0, 1, false),
    wg("最大化", "\u{E922}", 24.0, 1, false),
    wg("关闭", "\u{E8BB}", 24.0, 1, false),
    wg("返回", "\u{E72B}", 26.0, 0, false),
    wg("前进", "\u{E72A}", 26.0, 0, false),
    wg("上级", "\u{E7BA}", 26.0, 0, false),
    wg("新建", "\u{E8F4}", 56.0, 0, false),
    wg("剪切", "\u{E8C6}", 56.0, 0, false),
    wg("复制", "\u{E8C8}", 56.0, 0, false),
    wg("粘贴", "\u{E77F}", 56.0, 0, false),
    wg("重命名", "\u{E8AC}", 56.0, 0, false),
    wg("删除", "\u{E74D}", 44.0, 0, false),
    wg("视图", "\u{E7B3}", 56.0, 0, false),
    wg("排序", "\u{E8CB}", 56.0, 0, false),
    wg("更多", "\u{E712}", 26.0, 0, false),
    wg("地址面包屑", "\u{E8B7}", 46.0, 0, true),
    wg("地址输入框", "\u{E8B7}", 120.0, 0, true),
    wg("搜索框", "\u{E721}", 120.0, 0, true),
    wg("侧栏分组标题", "\u{E8B7}", 120.0, 0, true),
    wg("侧栏条目", "\u{E8B7}", 120.0, 0, true),
    wg("详情属性行", "\u{E9E9}", 120.0, 0, true),
    wg("项目数", "\u{E75B}", 56.0, 0, true),
    wg("已选大小", "\u{E762}", 64.0, 0, true),
    wg("容量条", "\u{E7F1}", 96.0, 0, true),
    wg("侧栏开关", "\u{E8B7}", 40.0, 0, false),
    wg("批量重命名", "\u{E8AC}", 30.0, 0, false),
    wg("视图切换", "\u{E7B3}", 56.0, 0, true),
];

/// 真窗口里**根本不存在**、或不是"能挪位置的一块"的槽位。
///
/// 上一版照着 Win11 命令栏列了一串（新建/剪切/复制/粘贴/重命名/删除/视图/排序…），
/// 但这个软件压根没有那条命令栏 —— 于是用户可以兴高采烈地把它们拖来拖去，
/// 真窗口一个像素都不变。编排页里出现的东西必须真的能在窗口里找到，
/// 否则就是"看着能调、调了没用"。
pub const WIDGET_RETIRED: [u16; 14] =
    [8, 9, 10, 11, 12, 13, 14, 15, 18, 20, 21, 22, 24, 25];

// —— 真窗口排布用的组件 id（`paint::apply_chrome` 按这些 id 摆位）——
pub const W_TABS: u16 = 0;
pub const W_NEW_TAB: u16 = 1;
pub const W_MIN: u16 = 2;
pub const W_MAX: u16 = 3;
pub const W_CLOSE: u16 = 4;
pub const W_BACK: u16 = 5;
pub const W_FORWARD: u16 = 6;
pub const W_UP: u16 = 7;
pub const W_MORE: u16 = 16;
pub const W_ADDRESS: u16 = 17;
pub const W_SEARCH: u16 = 19;
pub const W_STATUS_TEXT: u16 = 23;
pub const W_SIDEBAR_TOGGLE: u16 = 26;
pub const W_BULK: u16 = 27;
pub const W_VIEW_SWITCH: u16 = 28;

/// 最下面那三条最长的带。
pub const BAR_NAMES: [&str; 3] = ["标题栏", "工具栏", "状态栏"];

/// 三条带出厂时各装什么。
pub fn default_bars() -> Vec<Vec<u16>> {
    vec![
        // 标题栏：左起 侧栏开关 → 标签条（弹性）→ 新建标签，右端 更多/最小/最大/关闭
        vec![W_SIDEBAR_TOGGLE, W_TABS, W_NEW_TAB, W_MORE, W_MIN, W_MAX, W_CLOSE],
        // 工具栏：左端三个导航，中间地址面包屑（弹性），右端 批量重命名 / 搜索框
        vec![W_BACK, W_FORWARD, W_UP, W_ADDRESS, W_BULK, W_SEARCH],
        // 状态栏：左边一行状态文字，右边视图切换
        vec![W_STATUS_TEXT, W_VIEW_SWITCH],
    ]
}

/// 右键菜单条目。下标即 id。
pub const MENU_ITEM_NAMES: [&str; 14] = [
    "打开", "打开方式", "剪切", "复制", "粘贴", "重命名", "批量重命名", "删除", "新建", "全选",
    "刷新", "查看", "排序方式", "属性",
];
/// 已经**不在右键菜单里**的槽位。
///
/// 「批量重命名」搬到了目录栏最右侧那个按钮上，`own_menu_rows` 不再生成它。
/// 但设置页的排布表是按 `MENU_ITEM_NAMES` 的下标建的，于是它仍占一行还能拖
/// —— 拖完真菜单纹丝不动（实测：拖到第一位，真菜单一个字都没变）。
/// 排布表必须和"菜单里真的会出现哪些项"严格一致，否则用户调了半天没反应。
pub const MENU_ITEM_RETIRED: [u16; 1] = [6];

pub const MENU_ITEM_GLYPHS: [&str; 14] = [
    "\u{E8A7}", "\u{E7AC}", "\u{E8C6}", "\u{E8C8}", "\u{E77F}", "\u{E8AC}", "\u{E8AC}", "\u{E74D}",
    "\u{E8F4}", "\u{E762}", "\u{E72C}", "\u{E7B3}", "\u{E8CB}", "\u{E946}",
];

// ---------------------------------------------------------------- 圆角（外观）
//
// 三级树：一级「全局圆角」，二级**按页面分**（标题栏 / 地址栏 / 工具栏 / 侧栏 /
// 内容区 / 状态栏 / 菜单），三级是该页面里**可调节**的圆角项，按名称的拼音首字母
// 排序。名字一律按"它管的是哪一块"来取 —— 「侧栏圆角」管侧栏这一整块，
// 它下面的「鼠标悬停圆角」管侧栏里鼠标悬停那条底色块的四个角。
//
// 按用户要求**先不做窗口圆角**：整个软件窗口的外框、以及侧栏那条外边框，
// 都不在这棵树里。

pub struct RadiusNode {
    /// 配置键。二级是 "sidebar" 这种纯页面名，三级是 "sidebar.hover"。
    pub id: &'static str,
    pub name: &'static str,
    /// 名称的拼音首字母，只用来给三级排序（`page()` 里排一次）。
    pub py: &'static str,
    /// 默认半径（DIP）
    pub def: u32,
    /// 上限（DIP）。再大就不是圆角而是胶囊了。
    pub max: u32,
    pub kids: Vec<RadiusNode>,
}

fn leaf(id: &'static str, name: &'static str, py: &'static str, def: u32, max: u32) -> RadiusNode {
    RadiusNode { id, name, py, def, max, kids: Vec::new() }
}

fn page(
    id: &'static str,
    name: &'static str,
    def: u32,
    max: u32,
    mut kids: Vec<RadiusNode>,
) -> RadiusNode {
    // 三级按首字母排序：用户点开一个页面后要能**顺着找**，而不是靠记忆。
    kids.sort_by(|a, b| a.py.cmp(b.py).then_with(|| a.name.cmp(b.name)));
    RadiusNode { id, name, py: "", def, max, kids }
}

pub fn radius_tree() -> Vec<RadiusNode> {
    vec![RadiusNode {
        id: "global",
        name: "全局圆角",
        py: "",
        def: 4,
        max: 20,
        kids: vec![
            page("title", "标题栏圆角", 4, 12, vec![
                leaf("title.button", "按钮圆角", "A", 4, 12),
                leaf("title.tab", "标签圆角", "B", 6, 16),
            ]),
            page("address", "地址栏圆角", 4, 12, vec![
                leaf("address.crumb", "面包屑圆角", "M", 4, 12),
                leaf("address.edit", "输入框圆角", "S", 6, 16),
            ]),
            page("toolbar", "工具栏圆角", 4, 12, vec![
                leaf("toolbar.button", "按钮圆角", "A", 4, 12),
            ]),
            // 只列**真实界面里真的有这块底色**的项：给没有的地方留开关，
            // 用户调了半天界面纹丝不动，等于又一个"看着做完了"的假功能。
            // 常态没有底色的（侧栏条目、磁贴、表头、状态栏整条）一概不列，
            // 它们要调就用上面一级的「侧栏圆角 / 内容区圆角 / 状态栏圆角」。
            page("sidebar", "侧栏圆角", 4, 12, vec![
                leaf("sidebar.hover", "鼠标悬停圆角", "S", 4, 12),
                leaf("sidebar.selected", "选中圆角", "X", 4, 12),
            ]),
            page("content", "内容区圆角", 6, 16, vec![
                leaf("content.hover", "鼠标悬停圆角", "S", 6, 16),
                leaf("content.selected", "选中圆角", "X", 6, 16),
            ]),
            page("status", "状态栏圆角", 3, 10, vec![
                leaf("status.button", "按钮圆角", "A", 3, 10),
            ]),
            page("menu", "菜单圆角", 6, 16, vec![
                leaf("menu.hover", "鼠标悬停圆角", "S", 4, 12),
                leaf("menu.panel", "面板圆角", "M", 6, 16),
            ]),
        ],
    }]
}

fn radius_find<'a>(id: &str, nodes: &'a [RadiusNode]) -> Option<&'a RadiusNode> {
    for n in nodes {
        if n.id == id {
            return Some(n);
        }
        if let Some(f) = radius_find(id, &n.kids) {
            return Some(f);
        }
    }
    None
}

pub fn radius_default(id: &str) -> u32 {
    radius_find(id, &radius_tree()).map(|n| n.def).unwrap_or(4)
}

pub fn radius_max(id: &str) -> u32 {
    radius_find(id, &radius_tree()).map(|n| n.max).unwrap_or(16)
}

pub fn radius_has_kids(id: &str) -> bool {
    radius_find(id, &radius_tree())
        .map(|n| !n.kids.is_empty())
        .unwrap_or(false)
}

/// 选中项的完整路径，例如 "全局圆角 › 侧栏圆角 › 鼠标悬停圆角"。
pub fn radius_label(id: &str) -> String {
    fn walk(id: &str, nodes: &[RadiusNode], acc: &mut Vec<&'static str>) -> bool {
        for n in nodes {
            let mut a2 = acc.clone();
            a2.push(n.name);
            if n.id == id {
                *acc = a2;
                return true;
            }
            if walk(id, &n.kids, &mut a2) {
                *acc = a2;
                return true;
            }
        }
        false
    }
    let mut acc = Vec::new();
    if walk(id, &radius_tree(), &mut acc) {
        acc.join(" › ")
    } else {
        id.to_string()
    }
}

/// 树的**可见行**（展开的节点才列孩子）。绘制与命中共用这一份。
pub struct RadiusRow {
    pub id: String,
    pub depth: usize,
    pub name: &'static str,
    pub has_kids: bool,
    pub open: bool,
}

pub fn radius_rows(app: &App) -> Vec<RadiusRow> {
    let mut out = Vec::new();
    fn walk(app: &App, nodes: &[RadiusNode], depth: usize, out: &mut Vec<RadiusRow>) {
        for n in nodes {
            let open = app.settings_tree_open.contains(n.id);
            out.push(RadiusRow {
                id: n.id.to_string(),
                depth,
                name: n.name,
                has_kids: !n.kids.is_empty(),
                open,
            });
            if open {
                walk(app, &n.kids, depth + 1, out);
            }
        }
    }
    walk(app, &radius_tree(), 0, &mut out);
    out
}

// ---------------------------------------------------------------- 快捷键表

/// 动作 id → 中文名。顺序即「快捷键」页的显示顺序（按窗口/导航/文件/视图分组）。
pub const HOTKEY_LABELS: [(&str, &str); 18] = [
    ("global.show_window", "显示/隐藏窗口"),
    ("window.new_tab", "新建标签"),
    ("window.close_tab", "关闭标签"),
    ("nav.back", "后退"),
    ("nav.forward", "前进"),
    ("nav.up", "上一级"),
    ("file.new_folder", "新建文件夹"),
    ("file.rename", "重命名"),
    ("file.delete", "删除到回收站"),
    ("file.delete_permanent", "永久删除"),
    ("file.copy", "复制"),
    ("file.cut", "剪切"),
    ("file.paste", "粘贴"),
    ("file.select_all", "全选"),
    ("file.refresh", "刷新"),
    ("view.cycle", "切换视图"),
    ("ui.focus_address", "定位到地址栏"),
    ("ui.focus_search", "定位到搜索框"),
];

fn hotkey_rows(app: &App, content: &RECT) -> Vec<paint::SettingRow> {
    let s = app.metrics.scale;
    let rh = (34.0 * s).round() as i32;
    let mut out = Vec::new();
    let mut y = content.top;
    let cap = app.hotkey_capture.as_deref();
    for (i, (id, name)) in HOTKEY_LABELS.iter().enumerate() {
        let value = if cap == Some(*id) {
            "按下新组合键…（Esc 取消）".to_string()
        } else {
            app.cfg
                .core
                .hotkeys
                .get(*id)
                .map(|v| v.to_uppercase())
                .unwrap_or_else(|| "未设置".to_string())
        };
        out.push(row(
            RECT { left: content.left, top: y, right: content.right, bottom: y + rh },
            name,
            value,
            2000 + i as u32,
            None,
        ));
        y += rh + (4.0 * s).round() as i32;
    }
    out
}

// ---------------------------------------------------------------- 持久化

#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    /// 「界面」里可见组件的排列顺序
    pub ui_order: Vec<u8>,
    /// 被拖进「更多」的组件
    pub ui_hidden: Vec<u8>,
    /// 三条最长的带（标题栏 / 工具栏 / 状态栏）各自装了哪些组件。
    ///
    /// 存的是 `WIDGETS` 的下标。一个组件只能待在一处 —— 拖进另一条就等于
    /// 从原来那条里挪走。
    #[serde(default = "default_bars")]
    pub bars: Vec<Vec<u16>>,
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
            bars: default_bars(),
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
        let _ = &WIDGET_RETIRED;
        self.ui_hidden.retain(|i| *i < n && !self.ui_order.contains(i));
        self.ui_hidden.sort_unstable();
        self.ui_hidden.dedup();

        // 三条整条：条数不对就整份复位；越界的、重复的（一个组件只能待一处）丢掉
        if self.bars.len() != BAR_NAMES.len() {
            self.bars = default_bars();
        }
        let wn = WIDGETS.len() as u16;
        let mut seen = std::collections::HashSet::new();
        for b in self.bars.iter_mut() {
            b.retain(|v| *v < wn && !WIDGET_RETIRED.contains(v) && seen.insert(*v));
        }

        let m = MENU_ITEM_NAMES.len() as u16;
        self.menu_order
            .retain(|i| *i < m && !self.menu_hidden.contains(i) && !MENU_ITEM_RETIRED.contains(&i));
        for i in 0..m {
            if !self.menu_order.contains(&i)
                && !self.menu_hidden.contains(&i)
                && !MENU_ITEM_RETIRED.contains(&i)
            {
                self.menu_order.push(i);
            }
        }
        self.menu_hidden
            .retain(|i| *i < m && !self.menu_order.contains(i) && !MENU_ITEM_RETIRED.contains(&i));
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
    /// 「高级」/「插件」/「快捷键」的普通行（复用 paint::SettingRow 的绘制）
    pub rows: Vec<paint::SettingRow>,
    /// 「外观」：左侧圆角三级树的行
    pub tree_rows: Vec<RECT>,
    /// 与 `tree_rows` 一一对应的圆角项 id
    pub tree_ids: Vec<String>,
    /// 「外观」：右侧实时预览
    pub preview: RECT,
    /// 「外观」：圆角滑杆
    pub slider: RECT,
    /// 「右键菜单」：左侧那块**真实渲染**的菜单面板
    pub menu_panel: RECT,
    /// 菜单面板里每一条（分隔线不占）的矩形与条目下标
    pub menu_rows: Vec<RECT>,
    pub menu_idx: Vec<usize>,
    /// 「右键菜单」：右上「打开方式」面板
    pub ow_panel: RECT,
    /// 界面页：上面那条（小组件）
    pub ui_top: RECT,
    /// 界面页：右边那条（长组件）
    pub ui_right: RECT,
    /// 界面页：中间的窗口实时预览
    pub ui_preview: RECT,
    /// 界面页：**预览里**那三条带（标题栏/工具栏/状态栏）的屏幕矩形。
    /// 预览不是装饰 —— 它就是唯一的投放目标（1:1，比三条 30px 的整条好点多了）。
    pub ui_pv_bars: [RECT; 3],
    /// 打开方式里每一条候选程序
    pub ow_rows: Vec<RECT>,
    /// 「右键菜单」：右下系统集成开关
    pub sys_panel: RECT,
}

/// 命中结果。
pub enum Hit {
    /// 顶部第 n 个小标签
    Tab(u8),
    /// 第 n 张卡片（可拖）
    Card(usize),
    /// 「更多」区里第 n 个被隐藏项（可拖回来）
    Chip(usize),
    /// 「高级」/「快捷键」行：动作编号（paint::SET_* / 2000+i）
    Row(u32),
    /// 「外观」圆角树第 n 行
    Tree(usize),
    /// 「外观」圆角滑杆（按下处的横坐标）
    RadiusSlider(i32),
    /// 「右键菜单」实时菜单里第 n 条（下标见 `Geom::menu_idx`）
    MenuRow(usize),
    /// 「右键菜单」打开方式列表里第 n 个程序
    OpenWith(usize),
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
        tree_rows: Vec::new(),
        tree_ids: Vec::new(),
        preview: RECT::default(),
        slider: RECT::default(),
        menu_panel: RECT::default(),
        menu_rows: Vec::new(),
        menu_idx: Vec::new(),
        ow_panel: RECT::default(),
        ow_rows: Vec::new(),
        sys_panel: RECT::default(),
        ui_top: RECT::default(),
        ui_right: RECT::default(),
        ui_preview: RECT::default(),
        ui_pv_bars: [RECT::default(); 3],
    };

    match app.settings_tab {
        crate::app::SETTINGS_TAB_MENU => {
            // 左边是**按当前排布真实渲染**的右键菜单，右边上半是「打开方式」
            // 的真实候选程序，右边下半是写回 Windows 的系统右键菜单开关。
            let mw = px(300.0).min((content.right - content.left) * 42 / 100);
            g.menu_panel = RECT {
                left: content.left,
                top: content.top,
                right: content.left + mw,
                bottom: content.bottom,
            };
            let rh = px(30.0);
            let mut top = g.menu_panel.top + px(4.0);
            for (i, e) in menu_preview_entries(app).iter().enumerate() {
                if e.separator {
                    top += px(6.0);
                    continue;
                }
                if top + rh > g.menu_panel.bottom - px(4.0) {
                    break;
                }
                g.menu_rows.push(RECT {
                    left: g.menu_panel.left,
                    top,
                    right: g.menu_panel.right,
                    bottom: top + rh,
                });
                g.menu_idx.push(i);
                top += rh;
            }
            let rx = g.menu_panel.right + px(14.0);
            let rh2 = (content.bottom - content.top) * 62 / 100;
            g.ow_panel = RECT { left: rx, top: content.top, right: content.right, bottom: content.top + rh2 };
            g.sys_panel = RECT { left: rx, top: content.top + rh2 + px(10.0), right: content.right, bottom: content.bottom };
            let n = app.openwith_preview.as_ref().map(|v| v.len()).unwrap_or(0);
            let orh = px(38.0);
            let mut oy = g.ow_panel.top + px(26.0);
            for _ in 0..n {
                if oy + orh > g.ow_panel.bottom {
                    break;
                }
                g.ow_rows.push(RECT { left: g.ow_panel.left, top: oy, right: g.ow_panel.right, bottom: oy + orh });
                oy += orh + px(4.0);
            }
            g.rows = system_rows(app, &g.sys_panel);
        }
        crate::app::SETTINGS_TAB_UI => {
            // 上面两条调色板（小组件 / 长组件），下面一整块 **1:1 的真窗口预览**。
            //
            // 上一版是"预览缩到 0.49 倍 + 底部三条 30px 整条"：预览小到看不清，
            // 而且是手绘示意图（侧栏没字、内容区没列头），和真窗口长得不一样。
            // 现在预览占满整页宽、用真绘制函数原尺寸画 —— 它就是软件窗口本身，
            // 也是唯一的投放目标。三条整条删了：同一个信息画两遍，还把预览挤瘪。
            let gap = px(10.0);
            let h_top = px(48.0);
            let h_long = px(50.0);
            g.ui_top = RECT {
                left: content.left,
                top: content.top,
                right: content.right,
                bottom: content.top + h_top,
            };
            g.ui_right = RECT {
                left: content.left,
                top: g.ui_top.bottom + gap,
                right: content.right,
                bottom: g.ui_top.bottom + gap + h_long,
            };
            g.ui_preview = RECT {
                left: content.left,
                top: g.ui_right.bottom + gap,
                right: content.right,
                bottom: content.bottom,
            };
            // 预览里三条带的位置：和 draw_window_preview 共用同一份真布局，
            // 于是"看起来在这条带上"和"判定为这条带"必然一致。
            g.ui_pv_bars = preview_bands(app, &g.ui_preview);

            // —— 第一行：小组件，横向流式排布，排满换行 ——
            let mut tx = g.ui_top.left;
            let mut ty = g.ui_top.top + px(16.0);
            let sh = px(28.0);
            for (i, wd) in WIDGETS.iter().enumerate() {
                if wd.long || WIDGET_RETIRED.contains(&(i as u16)) {
                    continue;
                }
                let ww = px(wd.w).max(px(28.0));
                if tx + ww > g.ui_top.right {
                    tx = g.ui_top.left;
                    ty += sh + px(4.0);
                }
                if ty + sh > g.ui_top.bottom {
                    break;
                }
                g.cards.push(RECT { left: tx, top: ty, right: tx + ww, bottom: ty + sh });
                g.card_ids.push(i as u32);
                tx += ww + px(4.0);
            }

            // —— 第二行：长组件 ——
            let mut lx = g.ui_right.left;
            let ly = g.ui_right.top + px(16.0);
            let lh = px(30.0);
            for (i, wd) in WIDGETS.iter().enumerate() {
                if !wd.long || WIDGET_RETIRED.contains(&(i as u16)) {
                    continue;
                }
                let ww = px(wd.w).max(px(56.0));
                if lx + ww > g.ui_right.right {
                    break;
                }
                g.cards.push(RECT { left: lx, top: ly, right: lx + ww, bottom: ly + lh });
                g.card_ids.push(i as u32);
                lx += ww + px(4.0);
            }

            // 这一页没有「更多」区了：拖出预览带就是拿掉，不需要第二个框
            g.dock = RECT::default();
        }
        crate::app::SETTINGS_TAB_LOOK => {
            // 左 44% 是三级树，右边是**实时预览**（上半）+ 圆角滑杆（下半）。
            // 预览画的是这个软件自己那一套组件的真实样式，改圆角当场看得见。
            let tree_w = (content.right - content.left) * 44 / 100;
            let rh = px(28.0);
            let mut y = content.top;
            for r in radius_rows(app) {
                if y + rh > content.bottom {
                    break;
                }
                g.tree_rows.push(RECT {
                    left: content.left,
                    top: y,
                    right: content.left + tree_w,
                    bottom: y + rh,
                });
                g.tree_ids.push(r.id);
                y += rh;
            }
            let pv_left = content.left + tree_w + px(14.0);
            let pv_h = (content.bottom - content.top) * 62 / 100;
            g.preview = RECT {
                left: pv_left,
                top: content.top,
                right: content.right,
                bottom: content.top + pv_h,
            };
            g.slider = RECT {
                left: pv_left,
                top: content.top + pv_h + px(12.0),
                right: content.right,
                bottom: content.top + pv_h + px(12.0) + px(34.0),
            };
        }
        crate::app::SETTINGS_TAB_HOTKEY => {
            g.rows = hotkey_rows(app, &content);
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
    for (i, r) in g.menu_rows.iter().enumerate() {
        if inside(r, x, y) {
            return Some(Hit::MenuRow(i));
        }
    }
    for (i, r) in g.ow_rows.iter().enumerate() {
        if inside(r, x, y) {
            return Some(Hit::OpenWith(i));
        }
    }
    for (i, r) in g.tree_rows.iter().enumerate() {
        if inside(r, x, y) {
            return Some(Hit::Tree(i));
        }
    }
    if g.slider.right > g.slider.left && inside(&g.slider, x, y) {
        return Some(Hit::RadiusSlider(x));
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
        crate::app::SETTINGS_TAB_UI => {
            paint_ui_tab(app, hdc, &g, th, s);
        }
        crate::app::SETTINGS_TAB_MENU => paint_menu_tab(app, hdc, &g, th, s),
        crate::app::SETTINGS_TAB_ADVANCED
        | crate::app::SETTINGS_TAB_PLUGIN
        | crate::app::SETTINGS_TAB_HOTKEY => {
            for r in &g.rows {
                paint_row(app, hdc, r, th, s);
            }
        }
        crate::app::SETTINGS_TAB_LOOK => paint_look(app, hdc, &g, th, s),
        crate::app::SETTINGS_TAB_INTERACT => {
            // 按用户要求**先留空**：这一页的开关还没想好怎么排。
            // 空白页也要写一句话说明"这是有意的空"，否则看着像没做出来。
            let r = RECT {
                left: g.content.left,
                top: g.content.top,
                right: g.content.right,
                bottom: g.content.top + (34.0 * s).round() as i32,
            };
            set_color(hdc, th.text_dim);
            paint::draw_text(
                hdc,
                "（这一页先留空 —— 交互类的开关还没定好怎么分）",
                r,
                DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER,
            );
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
        if d.tab == crate::app::SETTINGS_TAB_UI {
            draw_widget(app, hdc, &r, (d.id & 0xFF) as usize, th, s, false, true);
        } else {
            let (name, glyph) = label_of(d.tab, d.id);
            draw_card(hdc, &r, &name, &glyph, th, s, true, false);
        }
    }
}

// ---------------------------------------------------------------- 界面页

/// 界面页：中间是这个软件窗口的实时预览，四周三条组件带。
fn paint_ui_tab(app: &mut App, hdc: HDC, g: &Geom, th: Theme, s: f32) {
    // —— 两条调色板 ——
    band_title(
        hdc,
        &g.ui_top,
        "小组件 —— 暗的=没装，点一下装上；拖进预览可放到指定位置",
        th,
        s,
    );
    band_title(
        hdc,
        &g.ui_right,
        "长组件 —— 地址栏、搜索框那些；同样点一下装/不装",
        th,
        s,
    );

    // —— 中间：整个软件窗口的 1:1 实时预览（真绘制函数画的） ——
    let pv = g.ui_preview;
    draw_window_preview(app, hdc, &pv, th, s);

    // —— 调色板里的组件：没装在任何带子里的画成暗的 ——
    for (i, r) in g.cards.iter().enumerate() {
        let id = g.card_ids[i];
        if id & 0x8000_0000 != 0 {
            continue;
        }
        let wid = (id & 0xFF) as usize;
        let dragging = matches!(&app.settings_drag, Some(d) if d.id == id);
        let hover = app.settings_hover_card == Some(id) && !dragging;
        let ghost = widget_bar_of(app, wid as u16).is_none();
        draw_widget_ex(app, hdc, r, wid, th, s, hover, dragging, ghost);
    }

    // 拖着东西的时候，把**会接收它的那条带**在预览里框出来 —— 不然用户
    // 不知道往哪儿松手（"拖过去没反应"有一半是这个原因）。
    if let Some(d) = &app.settings_drag {
        if let Some((bi, _)) = drop_target(app, g, d.cur.0, d.cur.1) {
            {
                let br = g.ui_pv_bars[bi];
                if br.right > br.left && br.bottom > br.top {
                    unsafe {
                        let p = windows::Win32::Graphics::Gdi::CreatePen(
                            windows::Win32::Graphics::Gdi::PS_SOLID,
                            2,
                            th.accent,
                        );
                        let op = SelectObject(hdc, p.into());
                        let ob = SelectObject(
                            hdc,
                            windows::Win32::Graphics::Gdi::GetStockObject(
                                windows::Win32::Graphics::Gdi::NULL_BRUSH,
                            ),
                        );
                        let _ = windows::Win32::Graphics::Gdi::Rectangle(
                            hdc, br.left, br.top, br.right, br.bottom,
                        );
                        let _ = SelectObject(hdc, ob);
                        let _ = SelectObject(hdc, op);
                        let _ = DeleteObject(p.into());
                    }
                }
            }
        }
    }
}

/// 组件当前装在哪条带（没装返回 None）。
pub fn widget_bar_of(app: &App, wid: u16) -> Option<usize> {
    app.ui_settings.bars.iter().position(|b| b.contains(&wid))
}

/// 组件的"老家"：出厂排布里它在哪条带。点一下装上时用这个。
fn home_bar_of(wid: u16) -> usize {
    for (bi, b) in default_bars().iter().enumerate() {
        if b.contains(&wid) {
            return bi;
        }
    }
    1
}

/// 没装 → 装到它本来该待的那条带；装了 → 摘掉（回到调色板等着再装）。
///
/// 出厂时 15 个组件全装在带子里，于是"添加"这件事根本无从下手 —— 拖到预览里
/// 反而变成"拿掉"。给调色板加这条"点一下就装/不装"的通路，添加才真的可行。
fn toggle_widget(app: &mut App, wid: u16) {
    if let Some(bi) = widget_bar_of(app, wid) {
        app.ui_settings.bars[bi].retain(|v| *v != wid);
    } else {
        let bi = home_bar_of(wid);
        let v = &mut app.ui_settings.bars[bi];
        if !v.contains(&wid) {
            v.push(wid);
        }
    }
    after_change(app);
}

/// 落点判定：预览里那三条 1:1 的真带。
///
/// `finish_drag()`（真的改配置）和 `paint_ui_tab()`（画投放高亮）共用这一份。
fn drop_target(app: &App, g: &Geom, x: i32, y: i32) -> Option<(usize, Vec<RECT>)> {
    let tol = (6.0 * app.metrics.scale).round() as i32;
    for bi in 0..3 {
        let br = &g.ui_pv_bars[bi];
        if br.right > br.left
            && br.bottom > br.top
            && x >= br.left
            && x < br.right
            && y >= br.top - tol
            && y < br.bottom + tol
        {
            let items = preview_item_rects(app, &g.ui_preview, bi)
                .into_iter()
                .map(|(_, r)| r)
                .collect();
            return Some((bi, items));
        }
    }
    None
}

fn band_title(hdc: HDC, r: &RECT, name: &str, th: Theme, s: f32) {
    let mut t = *r;
    t.bottom = t.top + (16.0 * s).round() as i32;
    set_color(hdc, th.text_dim);
    let f = paint::ui_font(s);
    unsafe {
        SelectObject(hdc, f.into());
    }
    paint::draw_text(hdc, name, t, DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER);
}

/// 一个组件画成**它实际的样子**：按钮就是按钮，横条就是横条，圆角取外观页
/// 里那一套（于是改圆角当场看得见）。
fn draw_widget(
    app: &App,
    hdc: HDC,
    r: &RECT,
    wid: usize,
    th: Theme,
    s: f32,
    hover: bool,
    dragged: bool,
) {
    draw_widget_ex(app, hdc, r, wid, th, s, hover, dragged, false);
}

/// `ghost` = 这个组件**当前没装在任何一条带里**（真窗口里不会出现）。
/// 调色板里这种画成暗的 + 描边，于是"哪些能添加"一眼看得出来。
fn draw_widget_ex(
    app: &App,
    hdc: HDC,
    r: &RECT,
    wid: usize,
    th: Theme,
    s: f32,
    hover: bool,
    dragged: bool,
    ghost: bool,
) {
    let Some(wd) = WIDGETS.get(wid) else { return };
    let rid = if wd.long {
        "sidebar.item"
    } else if wd.align == 1 {
        "title.button"
    } else {
        "toolbar.button"
    };
    let fill = if dragged {
        th.accent
    } else if hover {
        th.hover_bg
    } else if ghost {
        th.window_bg
    } else if wd.long {
        th.surface_alt
    } else {
        th.surface
    };
    rr(
        hdc,
        r,
        app.radius_of(rid),
        fill,
        if hover && !dragged {
            Some(th.accent)
        } else if ghost {
            Some(th.border)
        } else {
            None
        },
    );
    let gw = (22.0 * s).round() as i32;
    let mut gr = *r;
    gr.right = gr.left + gw;
    set_color(hdc, if dragged { th.selection_text } else { th.text_dim });
    let gf = paint::glyph_font(s);
    unsafe {
        SelectObject(hdc, gf.into());
    }
    paint::draw_text(hdc, wd.glyph, gr, DT_SINGLELINE | DT_NOPREFIX | DT_CENTER | DT_VCENTER);
    if r.right - r.left > (36.0 * s).round() as i32 {
        let f = paint::ui_font(s);
        unsafe {
            SelectObject(hdc, f.into());
        }
        let mut tr = *r;
        tr.left += gw;
        tr.right -= (4.0 * s).round() as i32;
        set_color(hdc, if dragged {
            th.selection_text
        } else if ghost {
            th.text_dim
        } else {
            th.text
        });
        paint::draw_text(
            hdc,
            wd.name,
            tr,
            DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS,
        );
    }
}

/// 预览里那份**真布局**：按预览的宽高算一份真窗口布局，再整体搬进预览矩形。
///
/// 画（draw_window_preview）和落点判定（preview_item_rects）共用这一份 ——
/// 画在哪就判定在哪，不会出现"看着能放、放了没反应"。
fn preview_layout(app: &App, host: &RECT) -> paint::Layout {
    let mut pl = paint::layout_sized(app, host.right - host.left, host.bottom - host.top);
    paint::translate_layout(&mut pl, host.left, host.top);
    pl
}

/// 预览里三条带（标题栏/工具栏/状态栏）的屏幕矩形。
pub fn preview_bands(app: &App, host: &RECT) -> [RECT; 3] {
    if host.right <= host.left || host.bottom <= host.top {
        return [RECT::default(); 3];
    }
    let pl = preview_layout(app, host);
    [pl.title, pl.toolbar, pl.status]
}

/// 预览带里每个组件真实占的矩形（组件 id → 矩形），用来算插入位置。
///
/// 直接从真布局里取（pl.back / pl.address / pl.min_btn …），和画出来的
/// 一像素不差 —— 上一版用 layout_bar 自算一份，插进去的位置和看着的会歪。
fn preview_item_rects(app: &App, host: &RECT, bar: usize) -> Vec<(u16, RECT)> {
    if bar >= BAR_NAMES.len() {
        return Vec::new();
    }
    let pl = preview_layout(app, host);
    let m = app.metrics;
    let Some(items) = app.ui_settings.bars.get(bar) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|wid| {
            let r = match *wid {
                W_SIDEBAR_TOGGLE => pl.sidebar_toggle,
                W_TABS => pl.tabs,
                W_NEW_TAB => RECT {
                    left: pl.tabs.right,
                    top: pl.tabs.top,
                    right: pl.tabs.right + (26.0 * m.scale).round() as i32,
                    bottom: pl.tabs.bottom,
                },
                W_MORE => pl.menu_btn,
                W_MIN => pl.min_btn,
                W_MAX => pl.max_btn,
                W_CLOSE => pl.close_btn,
                W_BACK => pl.back,
                W_FORWARD => pl.forward,
                W_UP => pl.up,
                W_ADDRESS => pl.address,
                W_BULK => pl.bulk_btn,
                W_SEARCH => pl.search,
                W_STATUS_TEXT => RECT {
                    left: pl.status.left + m.pad,
                    top: pl.status.top,
                    right: pl.status.left + m.pad + (200.0 * m.scale).round() as i32,
                    bottom: pl.status.bottom,
                },
                W_VIEW_SWITCH => paint::status_view_rects(&pl, &m).1,
                _ => return None,
            };
            Some((*wid, r))
        })
        .collect()
}

/// 中间那块：整个软件窗口的 **1:1 实时预览**。
///
/// 调的是**和真窗口同一批绘制函数**（paint_title_bar / paint_toolbar /
/// paint_sidebar / paint_panes / paint_status），布局用 layout_sized 按预览
/// 宽高现算一份再搬进来。于是预览里是真标签、真地址栏、真侧栏图标、真列头
/// —— 不是手绘示意图。上一版缩到 0.49 倍还画的是示意块，用户原话：
/// "挤在一起看都看不清，和我的软件界面也不一样"。
///
/// 整个过程必须 SaveDC/IntersectClipRect/RestoreDC 包住（记忆 DC 全窗口共享）。
fn draw_window_preview(app: &mut App, hdc: HDC, host: &RECT, th: Theme, s: f32) {
    let _ = s;
    let r = *host;
    if r.right <= r.left || r.bottom <= r.top {
        return;
    }
    let pl = preview_layout(app, &r);
    // 内容区必须画一个**真实目录**：此刻活动标签正是设置页本身，直接调
    // paint_panes 会顺着 tab.settings 递归回设置页 —— 整块预览被铺成一片底色。
    // 临时把活动标签指到第一个真实目录（paint_view 走的是只读路径），画完指回。
    let pane = app.focus_pane;
    let saved_tab = app.panes[pane].active;
    let alt = (0..app.panes[pane].tabs.len()).find(|i| {
        let t = &app.panes[pane].tabs[*i];
        !t.settings && !t.home && !t.recycle
    });
    if let Some(i) = alt {
        app.panes[pane].active = i;
    }
    unsafe {
        let saved = windows::Win32::Graphics::Gdi::SaveDC(hdc);
        let _ = windows::Win32::Graphics::Gdi::IntersectClipRect(
            hdc, r.left, r.top, r.right, r.bottom,
        );
        fill_rect(hdc, &r, th.window_bg);
        paint::paint_title_bar(app, hdc, &pl);
        paint::paint_toolbar(app, hdc, &pl);
        if app.cfg.ui.sidebar_visible {
            paint::paint_sidebar(app, hdc, &pl);
        }
        if alt.is_some() {
            paint::paint_panes(app, hdc, &pl);
        }
        if app.cfg.ui.statusbar_visible {
            paint::paint_status(app, hdc, &pl);
        }
        let _ = windows::Win32::Graphics::Gdi::RestoreDC(hdc, saved);
    }
    app.panes[pane].active = saved_tab;
}

/// 排布编辑器：卡片列表 + 底部「更多」区。（界面页改成了实时预览，这里留着给别的页复用。）
#[allow(dead_code)]
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
        // 高位是"来自哪条带的第几格"，低位才是组件下标
        let wd = WIDGETS.get((id & 0xFF) as usize);
        (
            wd.map(|w| w.name).unwrap_or("?").to_string(),
            wd.map(|w| w.glyph).unwrap_or("\u{E783}"),
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

/// 「外观」：左树 + 右预览 + 滑杆。
fn paint_look(app: &App, hdc: HDC, g: &Geom, th: Theme, s: f32) {
    // —— 三级树 ——
    let rows = radius_rows(app);
    for (i, r) in g.tree_rows.iter().enumerate() {
        let info = rows.get(i);
        let id = g.tree_ids.get(i).cloned().unwrap_or_default();
        let selected = app.settings_radius_sel == id;
        let hover = app.settings_hover_tree == Some(i);
        let bg = if selected {
            th.selection_bg
        } else if hover {
            th.row_hover
        } else {
            th.surface
        };
        fill_rect(hdc, r, bg);
        let depth = info.map(|v| v.depth).unwrap_or(0);
        let indent = (14.0 * s).round() as i32 * depth as i32 + (6.0 * s).round() as i32;
        // 展开/收起三角。没有它用户看不出哪一行还能往下钻。
        let mut gr = *r;
        gr.left += indent;
        gr.right = gr.left + (16.0 * s).round() as i32;
        if info.map(|v| v.has_kids).unwrap_or(false) {
            let glyph = if info.map(|v| v.open).unwrap_or(false) { "\u{E70D}" } else { "\u{E76C}" };
            text(hdc, &gr, glyph, th.text_dim, paint::glyph_font(s));
        }
        let mut tr = *r;
        tr.left = gr.right;
        tr.right = r.right - (52.0 * s).round() as i32;
        set_color(hdc, if selected { th.text } else { th.text_dim });
        paint::draw_text(
            hdc,
            info.map(|v| v.name).unwrap_or("?"),
            tr,
            DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS,
        );
        // 行尾直接把当前值写出来：不用点开也能扫一眼全局
        let mut vr = *r;
        vr.left = tr.right;
        vr.right = r.right - (6.0 * s).round() as i32;
        text(
            hdc,
            &vr,
            &format!("{} px", app.radius_of(&id)),
            th.text_dim,
            paint::ui_font(s),
        );
    }

    // —— 实时预览 ——
    draw_preview(app, hdc, &g.preview, &app.settings_radius_sel, th, s);

    // —— 滑杆 ——
    draw_slider(app, hdc, &g.slider, th, s);
}

/// 圆角矩形：填充 + 可选描边。`RoundRect` 的椭圆宽高是**直径**的两倍关系，
/// 这里统一按"半径"传参，省得每个调用点自己乘 2（早先漏乘过一次，圆角看着
/// 只有应有的一半）。
fn rr(hdc: HDC, r: &RECT, radius: u32, fill: COLORREF, outline: Option<COLORREF>) {
    let w = (r.right - r.left).min(r.bottom - r.top);
    let d = ((radius as i32) * 2).clamp(0, w.max(0));
    unsafe {
        let br = CreateSolidBrush(fill);
        let ob = SelectObject(hdc, br.into());
        let (oc, pen) = match outline {
            Some(c) => (c, 2),
            None => (fill, 1),
        };
        let p = windows::Win32::Graphics::Gdi::CreatePen(
            windows::Win32::Graphics::Gdi::PS_SOLID,
            pen,
            oc,
        );
        let op = SelectObject(hdc, p.into());
        let _ = RoundRect(hdc, r.left, r.top, r.right, r.bottom, d, d);
        SelectObject(hdc, op);
        SelectObject(hdc, ob);
        let _ = DeleteObject(p.into());
        let _ = DeleteObject(br.into());
    }
}

/// 预览里某一块是不是"当前选中的那一块"。
///
/// 选中「侧栏圆角」这种二级时，它下面所有三级块一起高亮 ——
/// 不然选了个页面却发现什么都不亮，用户会以为没生效。
fn hot(id: &str, sel: &str, th: Theme) -> Option<COLORREF> {
    if sel == id || sel.starts_with(&format!("{id}.")) || id.starts_with(&format!("{sel}.")) {
        Some(th.accent)
    } else {
        None
    }
}

/// 实时预览：把这个软件自己的那几块组件**按当前圆角值**真画出来。
///
/// 不是示意图 —— 每一块的半径都取自 `app.radius_of()`，改滑杆当场变形，
/// 于是"圆角到底圆在哪儿"不用猜。
fn draw_preview(app: &App, hdc: HDC, r: &RECT, sel: &str, th: Theme, s: f32) {
    if r.right <= r.left || r.bottom <= r.top {
        return;
    }
    fill_rect(hdc, r, th.window_bg);
    let pad = (8.0 * s).round() as i32;
    let l = r.left + pad;
    let t = r.top + pad;
    let rt = r.right - pad;
    let bt = r.bottom - pad;
    let gap = (4.0 * s).round() as i32;

    let title_h = (20.0 * s).round() as i32;
    let tb_h = (18.0 * s).round() as i32;
    let st_h = (14.0 * s).round() as i32;
    let sb_w = ((rt - l) * 24 / 100).max((40.0 * s).round() as i32);

    // 标题栏：三个按钮 + 两个标签
    let title = RECT { left: l, top: t, right: rt, bottom: t + title_h };
    fill_rect(hdc, &title, th.title_bg);
    let bw = (16.0 * s).round() as i32;
    let mut bx = rt - bw - (4.0 * s).round() as i32;
    for _ in 0..3 {
        let b = RECT { left: bx, top: t + gap, right: bx + bw, bottom: t + title_h - gap };
        rr(hdc, &b, app.radius_of("title.button") , th.chrome_bg, hot("title.button", sel, th));
        bx -= bw + (3.0 * s).round() as i32;
    }
    let tw = (52.0 * s).round() as i32;
    for k in 0..2 {
        let tb = RECT {
            left: l + (4.0 * s).round() as i32 + k * (tw + (4.0 * s).round() as i32),
            top: t + gap,
            right: l + (4.0 * s).round() as i32 + k * (tw + (4.0 * s).round() as i32) + tw,
            bottom: t + title_h - gap,
        };
        rr(
            hdc,
            &tb,
            app.radius_of("title.tab"),
            if k == 0 { th.tab_active } else { th.chrome_bg },
            hot("title.tab", sel, th),
        );
    }

    // 地址栏：面包屑 + 输入框
    let mut y = title.bottom + gap;
    let ad = RECT { left: l, top: y, right: rt, bottom: y + tb_h };
    fill_rect(hdc, &ad, th.chrome_bg);
    let cw = (34.0 * s).round() as i32;
    for k in 0..3 {
        let c = RECT { left: l + gap + k * (cw + gap), top: y + gap, right: l + gap + k * (cw + gap) + cw, bottom: y + tb_h - gap };
        rr(hdc, &c, app.radius_of("address.crumb"), th.surface, hot("address.crumb", sel, th));
    }
    let ed = RECT { left: l + gap + 3 * (cw + gap), top: y + gap, right: rt - gap, bottom: y + tb_h - gap };
    rr(hdc, &ed, app.radius_of("address.edit"), th.window_bg, hot("address.edit", sel, th));

    // 工具栏：两个按钮
    y = ad.bottom + gap;
    let tb2 = RECT { left: l, top: y, right: rt, bottom: y + tb_h };
    fill_rect(hdc, &tb2, th.chrome_bg);
    for k in 0..2 {
        let b = RECT { left: l + gap + k * (bw + gap), top: y + gap, right: l + gap + k * (bw + gap) + bw, bottom: y + tb_h - gap };
        rr(hdc, &b, app.radius_of("toolbar.button"), th.surface, hot("toolbar.button", sel, th));
    }

    // 主体：侧栏 + 内容区
    y = tb2.bottom + gap;
    let body_b = bt - st_h - gap;
    let sb = RECT { left: l, top: y, right: l + sb_w, bottom: body_b };
    fill_rect(hdc, &sb, th.chrome_bg);
    let ct = RECT { left: l + sb_w + gap, top: y, right: rt, bottom: body_b };
    fill_rect(hdc, &ct, th.window_bg);

    // 侧栏：分组标题 / 条目 / 悬停 / 选中，四种状态各画一条
    let rh = (body_b - y - 5 * gap) / 4;
    let sw = sb_w - 2 * gap;
    for k in 0..4 {
        let it = RECT { left: sb.left + gap, top: y + gap + k * (rh + gap), right: sb.left + gap + sw, bottom: y + gap + k * (rh + gap) + rh };
        let (id, fill) = match k {
            0 => ("sidebar.group", th.surface),
            1 => ("sidebar.item", th.surface_alt),
            2 => ("sidebar.hover", th.hover_bg),
            _ => ("sidebar.selected", th.selection_bg),
        };
        rr(hdc, &it, app.radius_of(id), fill, hot(id, sel, th));
    }

    // 内容区：表头 + 2x2 磁贴（一块悬停、一块选中）
    let hh = (14.0 * s).round() as i32;
    let hd = RECT { left: ct.left + gap, top: ct.top + gap, right: ct.right - gap, bottom: ct.top + gap + hh };
    rr(hdc, &hd, app.radius_of("content"), th.surface, hot("content.header", sel, th));
    let gw = (ct.right - ct.left - 5 * gap) / 2;
    let gh = (body_b - hd.bottom - 3 * gap) / 2;
    for k in 0..4 {
        let col = k % 2;
        let row = k / 2;
        let tl = RECT {
            left: ct.left + gap + col * (gw + gap),
            top: hd.bottom + gap + row * (gh + gap),
            right: ct.left + gap + col * (gw + gap) + gw,
            bottom: hd.bottom + gap + row * (gh + gap) + gh,
        };
        let (id, fill) = match k {
            0 => ("content.tile", th.surface),
            1 => ("content.hover", th.hover_bg),
            2 => ("content.tile", th.surface),
            _ => ("content.selected", th.selection_bg),
        };
        rr(hdc, &tl, app.radius_of(id), fill, hot(id, sel, th));
    }

    // 状态栏
    let st = RECT { left: l, top: body_b + gap, right: rt, bottom: bt };
    fill_rect(hdc, &st, th.status_bg);
    rr(
        hdc,
        &RECT { left: st.left + gap, top: st.top + gap, right: st.right - gap, bottom: st.bottom - gap },
        app.radius_of("status"),
        th.surface,
        hot("status.panel", sel, th),
    );
    let sb2 = RECT { left: st.right - gap - bw * 2, top: st.top + gap, right: st.right - gap - bw, bottom: st.bottom - gap };
    rr(hdc, &sb2, app.radius_of("status.button"), th.surface_alt, hot("status.button", sel, th));
}

/// 圆角滑杆：轨道 + 已填段 + 圆点 + 数值。拖动/点击都按横坐标换算。
fn draw_slider(app: &App, hdc: HDC, r: &RECT, th: Theme, s: f32) {
    let id = app.settings_radius_sel.clone();
    let max = radius_max(&id).max(1);
    let v = app.radius_of(&id);

    let mut lr = *r;
    lr.bottom = lr.top + (18.0 * s).round() as i32;
    set_color(hdc, th.text);
    paint::draw_text(
        hdc,
        &radius_label(&id),
        lr,
        DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS,
    );
    let mut nr = *r;
    nr.left = nr.right - (56.0 * s).round() as i32;
    nr.bottom = nr.top + (18.0 * s).round() as i32;
    text(hdc, &nr, &format!("{v} px"), th.accent, paint::ui_font(s));

    let mut tr = *r;
    tr.top = lr.bottom + (4.0 * s).round() as i32;
    tr.bottom = tr.top + (10.0 * s).round() as i32;
    tr.right -= (56.0 * s).round() as i32 + (8.0 * s).round() as i32;
    let w = (tr.right - tr.left).max(1);
    let filled = ((v as f32 / max as f32) * w as f32).round() as i32;
    rr(hdc, &tr, 5, th.surface_alt, None);
    let fr = RECT { left: tr.left, top: tr.top, right: tr.left + filled.min(w), bottom: tr.bottom };
    rr(hdc, &fr, 5, th.accent, None);
    // 圆点
    let d = (14.0 * s).round() as i32;
    let cx = tr.left + filled.clamp(0, w);
    let kn = RECT { left: cx - d / 2, top: tr.bottom - d / 2 + (2.0 * s).round() as i32, right: cx + d - d / 2, bottom: tr.bottom + d / 2 + (2.0 * s).round() as i32 };
    rr(hdc, &kn, (d / 2) as u32, th.text, None);
}


// ================================================================ 右键菜单页

/// 预览用的菜单条目：**走真实构建函数**，不是照着菜单再摆一份假条目。
/// 于是设置页里排的顺序/显隐，当场就能看出来。
fn menu_preview_entries(app: &App) -> Vec<crate::menu::MenuEntry> {
    // 样本在切进这一页时算好存起来了（`App::menu_preview_items`）——
    // 设置页自己是虚拟标签，照它渲染只能得到"空白处右键"的 6 行短菜单，
    // 而这里要展示的是"对文件右键"那一整套。
    let items: Vec<std::path::PathBuf> = app.menu_preview_items.clone().unwrap_or_default();
    crate::app::own_menu_rows(app, &items)
}

/// 当前**可见**的菜单条目 id，顺序即渲染顺序。
///
/// 渲染出来的第 n 条（分隔线不算）就对应这里的第 n 个 —— 于是"在实时菜单里
/// 把第 3 条拖到第 1 条"可以直接换算成排布表里的位置交换。
pub fn visible_menu_ids(app: &App) -> Vec<u16> {
    app.ui_settings
        .menu_order
        .iter()
        .copied()
        .filter(|i| !app.ui_settings.menu_hidden.contains(i))
        .filter(|i| !MENU_ITEM_RETIRED.contains(&i))
        .collect()
}

/// 「打开方式」那一块下面真正的候选程序（程序名 + 图标句柄）。
fn ow_list(app: &App) -> Vec<(String, isize)> {
    app.openwith_preview.clone().unwrap_or_default()
}

/// 系统集成的两行开关。
fn system_rows(app: &App, r: &RECT) -> Vec<paint::SettingRow> {
    let s = app.metrics.scale;
    let rh = (34.0 * s).round() as i32;
    let mut out = Vec::new();
    let mut y = r.top;
    for (i, label) in ["文件右键菜单里加入「用 ZenFile 打开」", "文件夹右键菜单里加入「用 ZenFile 打开」"]
        .iter()
        .enumerate()
    {
        out.push(row(
            RECT { left: r.left, top: y, right: r.right, bottom: y + rh },
            label,
            String::new(),
            3000 + i as u32,
            Some(system_menu_enabled(i)),
        ));
        y += rh + (4.0 * s).round() as i32;
    }
    // 说明：写的是 HKCU，不需要管理员，也随时能在这页关掉
    out.push(row(
        RECT { left: r.left, top: y, right: r.right, bottom: y + rh },
        "写在 HKEY_CURRENT_USER（不用管理员；关掉开关即撤销）",
        String::new(),
        0,
        None,
    ));
    out
}

fn paint_menu_tab(app: &App, hdc: HDC, g: &Geom, th: Theme, s: f32) {
    // —— 左：真实渲染的右键菜单 ——
    let entries = menu_preview_entries(app);
    let row_h = (30.0 * s).round() as i32;
    let w = g.menu_panel.right - g.menu_panel.left;
    let h = g.menu_panel.bottom - g.menu_panel.top;
    if w > 0 && h > 0 {
        unsafe {
            // 菜单渲染器内部会 IntersectClipRect —— 记忆 DC 是全窗口共享的，
            // 必须 SaveDC/RestoreDC 包住，否则后面画「打开方式」那块会被裁掉。
            let _saved = windows::Win32::Graphics::Gdi::SaveDC(hdc);
            crate::menu::paint_menu_into(
                hdc,
                &entries,
                w,
                h,
                row_h,
                s,
                &th,
                app.settings_hover_menu,
                g.menu_panel.left,
                g.menu_panel.top,
            );
            // 先把 DC 还原回 SaveDC 那一刻的状态，**再**画边框。
            //
            // 顺序反了会踩两个坑（都是实测出来的）：
            //   1. `paint_menu_into` 内部改过窗口原点，还原前画出来的矩形会
            //      整体平移一个面板的宽高，落到菜单旁边的空白处；
            //   2. `Rectangle` 会**用当前画刷填内部**，而记忆 DC 的默认画刷
            //      是 WHITE_BRUSH —— 于是多出一大块纯白实心头。必须显式选
            //      NULL_BRUSH，让它只描边不填充。
            let _ = windows::Win32::Graphics::Gdi::RestoreDC(hdc, -1);
            let p = windows::Win32::Graphics::Gdi::CreatePen(
                windows::Win32::Graphics::Gdi::PS_SOLID,
                1,
                th.border,
            );
            let op = SelectObject(hdc, p.into());
            let ob = SelectObject(
                hdc,
                windows::Win32::Graphics::Gdi::GetStockObject(
                    windows::Win32::Graphics::Gdi::NULL_BRUSH,
                ),
            );
            let _ = windows::Win32::Graphics::Gdi::Rectangle(
                hdc,
                g.menu_panel.left,
                g.menu_panel.top,
                g.menu_panel.right,
                g.menu_panel.bottom,
            );
            SelectObject(hdc, ob);
            SelectObject(hdc, op);
            let _ = DeleteObject(p.into());
        }
    }

    // —— 右上：「打开方式」的真实程序列表 ——
    fill_rect(hdc, &g.ow_panel, th.surface);
    let hdr = RECT {
        left: g.ow_panel.left + (10.0 * s).round() as i32,
        top: g.ow_panel.top + (4.0 * s).round() as i32,
        right: g.ow_panel.right,
        bottom: g.ow_panel.top + (26.0 * s).round() as i32,
    };
    set_color(hdc, th.text);
    paint::draw_text(
        hdc,
        "打开方式（当前目录里那个文件的真实候选）",
        hdr,
        DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS,
    );
    let list = ow_list(app);
    let isz = (24.0 * s).round() as i32;
    for (i, r) in g.ow_rows.iter().enumerate() {
        let Some((name, icon)) = list.get(i) else { break };
        let hover = app.settings_hover_menu.is_none() && false;
        let _ = hover;
        fill_rect(hdc, r, th.surface_alt);
        let ix = r.left + (8.0 * s).round() as i32;
        let iy = (r.top + r.bottom - isz) / 2;
        if *icon != 0 {
            app.icons.draw(hdc, windows::Win32::UI::WindowsAndMessaging::HICON(*icon as *mut _), ix, iy, isz);
        }
        let mut tr = *r;
        tr.left = ix + isz + (8.0 * s).round() as i32;
        tr.right -= (8.0 * s).round() as i32;
        set_color(hdc, th.text);
        paint::draw_text(
            hdc,
            name,
            tr,
            DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS,
        );
    }

    // —— 右下：系统集成 ——
    for r in &g.rows {
        paint_row(app, hdc, r, th, s);
    }
}

// ================================================================ 系统右键菜单

/// 两类目标：0 = 文件（`*`），1 = 文件夹（`Directory`）。
fn sys_menu_path(kind: usize) -> &'static str {
    if kind == 0 {
        r"Software\Classes\*\shell\ZenFile"
    } else {
        r"Software\Classes\Directory\shell\ZenFile"
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn system_menu_enabled(kind: usize) -> bool {
    use windows::Win32::System::Registry::{RegCloseKey, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_READ};
    unsafe {
        let p = wide(sys_menu_path(kind));
        let mut hk = HKEY::default();
        let ok = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(p.as_ptr()),
            Some(0),
            KEY_READ,
            &mut hk,
        )
        .is_ok();
        if ok {
            let _ = RegCloseKey(hk);
        }
        ok
    }
}

fn reg_set_sz(hk: windows::Win32::System::Registry::HKEY, name: Option<&str>, val: &str) {
    use windows::Win32::System::Registry::{RegSetValueExW, REG_SZ};
    let n = name.map(wide);
    let mut v: Vec<u16> = wide(val);
    let bytes =
        unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 2) };
    let _ = unsafe {
        RegSetValueExW(
            hk,
            n.as_ref()
                .map(|x| windows::core::PCWSTR(x.as_ptr()))
                .unwrap_or(windows::core::PCWSTR::null()),
            Some(0),
            REG_SZ,
            Some(bytes),
        )
    };
    let _ = v.len();
}

/// 在 Windows 资源管理器的右键菜单里加入 / 移除「用 ZenFile 打开」。
///
/// 只写 `HKCU\Software\Classes`：① 不需要管理员；② 不影响系统级条目；
/// ③ 这页的开关一关就整棵删掉，随时可撤。
pub fn set_system_menu(kind: usize, on: bool) {
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, HKEY, HKEY_CURRENT_USER, KEY_WRITE,
        REG_OPTION_NON_VOLATILE,
    };
    let p = wide(sys_menu_path(kind));
    unsafe {
        if !on {
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, windows::core::PCWSTR(p.as_ptr()));
            return;
        }
        let mut hk = HKEY::default();
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(p.as_ptr()),
            Some(0),
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hk,
            None,
        )
        .is_err()
        {
            return;
        }
        let exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        reg_set_sz(hk, None, "用 ZenFile 打开(&Z)");
        reg_set_sz(hk, Some("Icon"), &format!("\"{exe}\""));
        let mut ck = HKEY::default();
        let sub = wide("command");
        if RegCreateKeyExW(
            hk,
            windows::core::PCWSTR(sub.as_ptr()),
            Some(0),
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut ck,
            None,
        )
        .is_ok()
        {
            reg_set_sz(ck, None, &format!("\"{exe}\" \"%1\""));
            let _ = RegCloseKey(ck);
        }
        let _ = RegCloseKey(hk);
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
        // 「右键菜单」页里那块**真实渲染的菜单**：直接拖条目改顺序
        Hit::MenuRow(i) => {
            let id = visible_menu_ids(app).get(*i).copied().unwrap_or(0) as u32;
            match g.menu_rows.get(*i) {
                Some(r) => (id, *r, false),
                None => return false,
            }
        }
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
    // 「界面」页：落点按**那三条整条**算 —— 拖进去就是装进来，拖到条外面就是
    // 拿掉，在同一条里挪就是换位置。整条里装的正是"组件里的子组件"。
    if d.tab == crate::app::SETTINGS_TAB_UI {
        let wid = (d.id & 0xFF) as u16;
        let from_bar = d.id & 0x8000_0000 != 0;
        let src_bar = if from_bar {
            Some(((d.id >> 16) & 0xFF) as usize)
        } else {
            None
        };
        let src_slot = ((d.id >> 8) & 0xFF) as usize;
        // 原地单击（鼠标没动）= 装/不装切换。出厂时 15 个组件全装满了，
        // 光靠拖是"搬"不是"加"，这条通路才让"添加组件"真的做得成。
        if !from_bar && (d.cur.0 - d.grab.0).abs() + (d.cur.1 - d.grab.1).abs() <= 3 {
            toggle_widget(app, wid);
            return;
        }
        // 一个组件只能待在一处：先从所有带子里摘掉
        for b in app.ui_settings.bars.iter_mut() {
            b.retain(|v| *v != wid);
        }
        if let Some((bi, items)) = drop_target(app, &g, x, y) {
            let mut slot = items.len();
            for (k, br) in items.iter().enumerate() {
                if x < (br.left + br.right) / 2 {
                    slot = k;
                    break;
                }
            }
            // 从同一条里往后拖时，源槽位已经被摘掉了，插入点要跟着前移一格
            if src_bar == Some(bi) && src_slot < slot {
                slot -= 1;
            }
            let v = &mut app.ui_settings.bars[bi];
            if slot > v.len() {
                slot = v.len();
            }
            v.insert(slot, wid);
        }
        after_change(app);
        return;
    }
    // 「右键菜单」页：落点按**渲染出来的菜单行**算（那里没有 cards）
    if d.tab == crate::app::SETTINGS_TAB_MENU {
        let mut target = g.menu_rows.len();
        for (i, r) in g.menu_rows.iter().enumerate() {
            if y < (r.top + r.bottom) / 2 {
                target = i;
                break;
            }
        }
        reorder(app, d.tab, d.id, target);
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
    // 排布变了必须重排子控件：搜索框/地址栏是真 EDIT 子窗口，靠 l.search /
    // l.address 定位。只重绘的话装饰画回来了、EDIT 还留在旧矩形里 —— 实测
    // "把搜索框拖回去，框画出来了但点不进去" 就是这一句没调。
    crate::paint::reposition_chrome(app);
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
