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
use std::collections::BTreeMap;
use std::path::PathBuf;
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::Graphics::Gdi::{
    CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, FillRect, GetStockObject, LineTo,
    GetViewportOrgEx, MoveToEx, IntersectClipRect, Polygon, Rectangle, RestoreDC,
    RoundRect, SaveDC, SelectObject, SetBkMode,
    SetTextColor, SetViewportOrgEx, DT_CENTER,
    DT_END_ELLIPSIS, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, HBRUSH, HPEN, NULL_BRUSH,
    PS_SOLID, TRANSPARENT,
};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::HICON;

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

/// 一个分组下面**所有叶子**的 id（第35轮）。
///
/// 分组节点（「标题栏圆角」这种）不对应界面上某块独立底板，它是
/// "本组里那些圆角块的共同默认值"。用户拖分组滑杆时，要落到这些叶子上，
/// 否则就出现"树里有这一项、真界面却什么都不变"的空壳。
pub fn radius_leaves_of(group: &str) -> Vec<String> {
    let tree = radius_tree();
    let Some(n) = radius_find(group, &tree) else {
        return Vec::new();
    };
    fn walk(n: &RadiusNode, out: &mut Vec<String>) {
        if n.kids.is_empty() {
            out.push(n.id.to_string());
            return;
        }
        for k in &n.kids {
            walk(k, out);
        }
    }
    let mut out = Vec::new();
    walk(n, &mut out);
    out
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
    /// 「右键菜单」里可见条目的排列顺序（**默认排布**，也是老配置的兼容字段）
    pub menu_order: Vec<u16>,
    /// 被拖进「更多」的菜单条目（默认排布）
    pub menu_hidden: Vec<u16>,
    /// 第55轮：**每一类**右键菜单各自的排布。
    ///
    /// 键见 `crate::app::MENU_KIND_KEYS`（file / folder / background / multi /
    /// drive）。照 ContextMenuManager 那样按"右键落在什么上"分类型 —— 文件与
    /// 空白处的菜单条目集合本来就不一样，却共用一套顺序，怎么拖都别扭。
    /// 某个类型没有记录时沿用上面那份默认排布（老配置无痛升级）。
    #[serde(default)]
    pub menu_sets: BTreeMap<String, MenuSet>,
    /// 预览画布里**已经放下的**组件（id, x, y）。
    ///
    /// round 25 起，预览画布改成"画布"形态：组件拖进预览里就落下到
    /// (x, y) 位置；拖出预览就删；最多装一次。每个 wid 在这里只出现一次。
    #[serde(default)]
    pub canvas_items: Vec<CanvasItem>,
}

/// 某一类右键菜单的排布。
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct MenuSet {
    pub order: Vec<u16>,
    pub hidden: Vec<u16>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct CanvasItem {
    pub wid: u16,
    /// 相对预览画布左上角（0..1 归一化坐标）
    pub nx: f32,
    pub ny: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            ui_order: (0..UI_PART_NAMES.len() as u8).collect(),
            ui_hidden: Vec::new(),
            bars: default_bars(),
            menu_order: (0..MENU_ITEM_NAMES.len() as u16).collect(),
            menu_hidden: Vec::new(),
            menu_sets: BTreeMap::new(),
            canvas_items: Vec::new(),
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
        for ms in self.menu_sets.values_mut() {
            ms.order
                .retain(|i| *i < m && !ms.hidden.contains(i) && !MENU_ITEM_RETIRED.contains(&i));
            for i in 0..m {
                if !ms.order.contains(&i)
                    && !ms.hidden.contains(&i)
                    && !MENU_ITEM_RETIRED.contains(&i)
                {
                    ms.order.push(i);
                }
            }
            ms.hidden
                .retain(|i| *i < m && !ms.order.contains(i) && !MENU_ITEM_RETIRED.contains(&i));
            ms.hidden.sort_unstable();
            ms.hidden.dedup();
        }
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
    /// 「交互」页树：与 `tree_rows` 一一对应的可调 action（分组行是 None）
    pub tree_actions: Vec<Option<u32>>,
    /// 「外观」：右侧实时预览
    pub preview: RECT,
    /// 「外观」：圆角滑杆
    pub slider: RECT,
    /// 「外观」：图标主题 2 颗按钮（0默认=系统 1优化）。
    ///
    /// 点击区与绘制共用；对应 `Hit::Row(4101 + i)`。
    pub icon_btns: [RECT; 2],
    /// 「外观」：图标样例条里每枚样例图标的位置（绘制用，不可点）。
    pub icon_samples: Vec<RECT>,
    /// 「右键菜单」：左侧那块**真实渲染**的菜单面板
    pub menu_panel: RECT,
    /// 菜单面板里每一条（分隔线不占）的矩形与条目下标
    pub menu_rows: Vec<RECT>,
    pub menu_idx: Vec<usize>,
    /// 「右键菜单」：顶部那一排"右键落在什么上"的类型 chip
    pub kind_chips: Vec<RECT>,
    /// 「右键菜单」：右上「打开方式」面板
    pub ow_panel: RECT,
    /// 「右键菜单」：右上面板底部那颗「查找可用的打开软件」按钮
    pub ow_find: RECT,
    /// 界面页：上面那条（小组件）
    pub ui_top: RECT,
    /// 界面页：右边那条（长组件）
    pub ui_right: RECT,
    /// 界面页：预览画布（左侧垂直居中）。**死数据** C:\Windows 详细列表。
    pub ui_preview: RECT,
    /// 界面页：5 个整条矩形（标题栏 / 工具栏 / 状态栏 / 目录栏 / 用户自定义空行）。
    pub ui_strips: [RECT; 5],
    /// 界面页：5 个整条顶部 y
    pub ui_strips_top: i32,
    /// 兼容旧字段（drag 旧版可能引用到）
    pub ui_pv_bars: [RECT; 3],
    /// 打开方式里每一条候选程序
    pub ow_rows: Vec<RECT>,
    /// 「右键菜单」：右下系统集成开关
    pub sys_panel: RECT,
    /// 「右键菜单」右 40%：格式样本**实时预览**面板
    pub prev_panel: RECT,
    /// 预览面板去掉标题那一条之后的"列表区"（裁剪与命中都按它）
    pub prev_view: RECT,
    /// 每一格的矩形。**虚拟坐标**（y 从 0 起），绘制/命中时减 `settings_prev_scroll`；
    /// 下标与 `fmtprev::samples()` 一一对应。
    pub prev_cells: Vec<RECT>,
    /// 每个字母徽标（矩形 + 字符），同样是虚拟坐标
    pub prev_letters: Vec<(RECT, char)>,
    /// 网格内容总高（虚拟），算滚动上界用
    pub prev_content_h: i32,
    /// 「右键菜单」左上那条**标题栏**（装类型 chip 的那一条）
    pub ctx_bar: RECT,
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
    /// 「右键菜单」顶部的类型 chip（下标见 `MENU_KIND_NAMES`）
    CtxKind(u8),
    /// 「右键菜单」右上面板底部那颗「查找可用的打开软件」按钮
    OwFind,
    /// 「右键菜单」右侧预览区里第 n 个格式样本（下标即 `fmtprev::samples()`）
    Preview(usize),
    /// 「右键菜单」右侧预览区的**空白处** —— 在这里右键 = 目录右键
    PreviewArea,
    /// 预览画布里第 n 个已放置组件（可拖动改位置 / 拖出预览删除）
    Canvas(usize),
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
    /// 是不是从预览画布里某个**已放置组件**上起的拖（下标见 `canvas_item_rects`）。
    /// 松手时：还在预览内 = 改位置，拖出预览 = 删除。
    pub from_canvas: Option<usize>,
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
        tree_actions: Vec::new(),
        preview: RECT::default(),
        slider: RECT::default(),
        icon_btns: [RECT::default(); 2],
        icon_samples: Vec::new(),
        menu_panel: RECT::default(),
        menu_rows: Vec::new(),
        menu_idx: Vec::new(),
        kind_chips: Vec::new(),
        ow_panel: RECT::default(),
        ow_find: RECT::default(),
        ow_rows: Vec::new(),
        sys_panel: RECT::default(),
        prev_panel: RECT::default(),
        prev_view: RECT::default(),
        prev_cells: Vec::new(),
        prev_letters: Vec::new(),
        prev_content_h: 0,
        ctx_bar: RECT::default(),
        ui_top: RECT::default(),
        ui_right: RECT::default(),
        ui_preview: RECT::default(),
        ui_strips: [RECT::default(); 5],
        ui_strips_top: 0,
        ui_pv_bars: [RECT::default(); 3],
    };

    match app.settings_tab {
        crate::app::SETTINGS_TAB_MENU => {
            // 第56轮：整页**左右分栏** ——
            //   左 60%：各种类型（**左上那条标题栏上的 chip 切换**）+ 该类型
            //           真实渲染的菜单 + 能找到的打开软件 + 系统开关；
            //   右 40%：各格式样本的**实时预览**（大图标网格、分字母、可滚动），
            //           右键某一格 = 该格式的右键菜单，空白处 = 目录右键。
            let total_w = content.right - content.left;
            let gap = px(12.0);
            let left_w = total_w * 60 / 100;
            g.prev_panel = RECT {
                left: content.left + left_w + gap / 2,
                top: content.top,
                right: content.right,
                bottom: content.bottom,
            };
            // 下面那一整套（chip / 菜单 / 打开方式 / 系统开关）都只在左栏里排
            let content = RECT {
                left: content.left,
                top: content.top,
                right: content.left + left_w - gap / 2,
                bottom: content.bottom,
            };
            // 第55轮（照 ContextMenuManager）：最上面一排是"右键落在什么上"的
            // 类型 chip，切换它 → 下面三块（菜单预览 / 打开方式 / 系统集成）
            // 全部跟着换。左边是**按该类型的当前排布真实渲染**的菜单，右边上半
            // 是该类型"能找到的打开软件"，右边下半是写回 Windows 的系统开关。
            let chip_h = px(28.0);
            let mut cx = content.left;
            // 注意：上面量标签用的那个 `hdc` **已经 ReleaseDC 了**（就在这段之前），
            // 拿它继续量文字，GetTextExtentPoint32W 直接失败返回 0 —— chip 会被压成
            // 22px（只剩 padding），字全糊在一起。这里必须自己再取一个。
            let cdc = unsafe { windows::Win32::Graphics::Gdi::GetDC(Some(app.hwnd)) };
            for name in crate::app::MENU_KIND_NAMES {
                let cw = if cdc.is_invalid() {
                    px(64.0)
                } else {
                    let w = unsafe { paint::text_width(cdc, hf, name) };
                    w + px(22.0)
                };
                g.kind_chips.push(RECT { left: cx, top: content.top, right: cx + cw, bottom: content.top + chip_h });
                cx += cw + px(6.0);
            }
            if !cdc.is_invalid() {
                unsafe {
                    let _ = windows::Win32::Graphics::Gdi::ReleaseDC(Some(app.hwnd), cdc);
                }
            }
            let content = RECT {
                left: content.left,
                top: content.top + chip_h + px(10.0),
                right: content.right,
                bottom: content.bottom,
            };
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
            // 「查找可用的打开软件」按钮贴在右上面板底部
            let fh = px(30.0);
            g.ow_find = RECT {
                left: g.ow_panel.left,
                top: g.ow_panel.bottom - fh,
                right: g.ow_panel.right,
                bottom: g.ow_panel.bottom,
            };
            let n = app.openwith_preview.as_ref().map(|v| v.len()).unwrap_or(0);
            let orh = px(38.0);
            let mut oy = g.ow_panel.top + px(26.0);
            for _ in 0..n {
                if oy + orh > g.ow_find.top - px(4.0) {
                    break;
                }
                g.ow_rows.push(RECT { left: g.ow_panel.left, top: oy, right: g.ow_panel.right, bottom: oy + orh });
                oy += orh + px(4.0);
            }
            g.rows = system_rows(app, &g.sys_panel);

            // —— 右 40%：格式样本网格 ——
            //
            // 排布规则（用户原话）：**一个字母占一段**，排不下就往下一排续，
            // 这一组完了立刻换行给下一个字母 —— 绝不会两个字母挤在同一排。
            {
                let pv = g.prev_panel;
                let head = px(26.0);
                g.prev_view = RECT {
                    left: pv.left + px(2.0),
                    top: pv.top + head,
                    right: pv.right - px(2.0),
                    bottom: pv.bottom - px(2.0),
                };
                let pad = px(8.0);
                let cell_w = px(86.0);
                let cell_h = px(74.0);
                let letter_w = px(24.0);
                let x0 = g.prev_view.left + pad;
                let x1 = g.prev_view.right - pad;
                let mut x = x0;
                let mut y = 0i32;
                let mut first = true;
                for (c, idxs) in crate::fmtprev::groups() {
                    if !first {
                        // 一组排完 → 直接换行，下一个字母从头起
                        x = x0;
                        y += cell_h;
                    }
                    first = false;
                    if x + letter_w > x1 {
                        x = x0;
                        y += cell_h;
                    }
                    g.prev_letters
                        .push((RECT { left: x, top: y, right: x + letter_w, bottom: y + cell_h }, c));
                    x += letter_w;
                    for i in idxs {
                        if x + cell_w > x1 {
                            x = x0;
                            y += cell_h;
                        }
                        g.prev_cells
                            .push(RECT { left: x, top: y, right: x + cell_w, bottom: y + cell_h });
                        x += cell_w;
                    }
                }
                // 末尾留一段**空白**：它是"目录右键"的地盘（用户原话"再空白区域
                // 就是目录右键"）。没有这一段，滚到底也全是格子，目录右键就
                // 没地方点了。行尾那些没排满的空隙同样算空白（命中里自然覆盖）。
                g.prev_content_h = y + cell_h + px(76.0);
            }
        }
        crate::app::SETTINGS_TAB_UI => {
            // round 25 全新布局（按用户原话"最后再明确一遍"）：
            //
            //   ┌──────── 8 个小标签（界面/外观/交互/...）────────┐
            //   ├──── 顶：小组件横排（按钮） ────┐   ┌── 右侧：长组件 ──┐
            //   │  [标签][新建][-][□][X][←][→]  │   │ [地址栏][搜索框]  │
            //   │  [↑][…][侧栏][批量]            │   │ [属性行][...]     │
            //   ├──────────────────────────────┤   └──────────────────┘
            //   │ 预览画布（C:\Windows 详细列表） │
            //   │ —— 死数据，仅渲染圆角 + 内部组件
            //   ├─────────────────────────────────┤
            //   │ 标题栏 | 工具栏 | 状态栏 | 目录栏 | 自定义空行 │
            //   └─────────────────────────────────┘
            //
            // 圆角（外观页那一套）只作用于预览画布 + 调色板卡片；
            // 其它都是死数据（顶/底整条占位条），不调真绘制函数。
            let gap = px(10.0);
            // 上排 = 小组件（横排整行）
            let h_top = px(48.0);
            g.ui_top = RECT {
                left: content.left,
                top: content.top,
                right: content.right,
                bottom: content.top + h_top,
            };
            // 下排 = 5 个整条矩形（标题栏/工具栏/状态栏/目录栏/用户空行）
            let strip_h = px(38.0);
            let strips_top = content.bottom - strip_h;
            g.ui_strips_top = strips_top;
            // 中间行区：小组件带下方 .. 整条上方
            let row_top = g.ui_top.bottom + gap;
            let row_bot = strips_top - gap;
            // 长组件区 = 行区**右侧整列**（用户原话："预览窗口的右边"）
            let w_long = (content.right - content.left) * 26 / 100;
            g.ui_right = RECT {
                left: content.right - w_long,
                top: row_top,
                right: content.right,
                bottom: row_bot,
            };
            // 预览画布 = 行区左侧，16:9 按比例缩，**垂直居中**（不占满，
            // 上下右三边都留白 —— 用户原话）
            let pv_left = content.left;
            let pv_right = g.ui_right.left - gap;
            let pv_w = pv_right - pv_left;
            let pv_h = (pv_w * 9 / 16)
                .min(px(430.0))
                .max(px(200.0))
                .min((row_bot - row_top - 2).max(px(200.0)));
            let pv_top = row_top + ((row_bot - row_top - pv_h) / 2).max(0);
            g.ui_preview = RECT {
                left: pv_left,
                top: pv_top,
                right: pv_right,
                bottom: pv_top + pv_h,
            };
            // 5 个整条矩形横向均匀分
            let strips_left = content.left;
            let strips_right = content.right;
            let strip_w = (strips_right - strips_left - gap * 4) / 5;
            g.ui_strips = [RECT::default(); 5];
            for (i, slot) in g.ui_strips.iter_mut().enumerate() {
                *slot = RECT {
                    left: strips_left + (i as i32) * (strip_w + gap),
                    top: strips_top,
                    right: strips_left + (i as i32) * (strip_w + gap) + strip_w,
                    bottom: strips_top + strip_h,
                };
            }
            // 兼容旧字段（旧 1:1 预览代码保留但不再用）
            g.ui_pv_bars = [RECT::default(); 3];

            // —— 上排：小组件横排（+20 避让 band_title 的 16px 标题行）——
            let mut tx = g.ui_top.left + px(8.0);
            let ty = g.ui_top.top + px(20.0);
            let sh = px(26.0);
            for (i, wd) in WIDGETS.iter().enumerate() {
                if wd.long || WIDGET_RETIRED.contains(&(i as u16)) {
                    continue;
                }
                let ww = px(wd.w).max(px(28.0));
                if tx + ww > g.ui_top.right - px(4.0) {
                    break;
                }
                g.cards.push(RECT { left: tx, top: ty, right: tx + ww, bottom: ty + sh });
                g.card_ids.push(i as u32);
                tx += ww + px(4.0);
            }

            // —— 右侧：长组件**竖排**一列（预览右边） ——
            let lx0 = g.ui_right.left + px(8.0);
            let lx1 = g.ui_right.right - px(8.0);
            let mut ly = g.ui_right.top + px(20.0);
            let lh = px(30.0);
            for (i, wd) in WIDGETS.iter().enumerate() {
                if !wd.long || WIDGET_RETIRED.contains(&(i as u16)) {
                    continue;
                }
                if ly + lh > g.ui_right.bottom - px(4.0) {
                    break;
                }
                g.cards.push(RECT { left: lx0, top: ly, right: lx1, bottom: ly + lh });
                g.card_ids.push(i as u32);
                ly += lh + px(4.0);
            }

            // 这一页没有「更多」区
            g.dock = RECT::default();
        }
        crate::app::SETTINGS_TAB_LOOK => {
            // 左 44% 是三级树，右边是**实时预览**（上半）+ 圆角滑杆（下半）
            // + 图标主题（第48轮：风格/颜色 + 样例条）。
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
            // 右列排布（自上而下）：预览 → 滑杆 → 图标主题按钮 → 样例条。
            // 预览按剩余空间算而不是固定比例 —— 下面多了图标区，62% 会挤爆。
            let icon_h = px(30.0) + px(8.0) + px(46.0) + px(12.0);
            let pv_h = ((content.bottom - content.top) * 52 / 100)
                .min((content.bottom - content.top) - px(12.0) - px(34.0) - icon_h)
                .max(px(150.0));
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
            // 图标主题：单行两颗按钮（默认 / 优化）
            let iy = g.slider.bottom + px(12.0);
            let bw = px(72.0);
            let gap = px(6.0);
            let label_w = px(52.0);
            let ry = iy;
            let x0 = g.slider.right - label_w - bw * 2 - gap * 2;
            for k in 0..2 {
                let bx = x0 + (bw + gap) * k as i32;
                g.icon_btns[k] = RECT {
                    left: bx,
                    top: ry,
                    right: bx + bw,
                    bottom: ry + px(30.0),
                };
            }
            // 样例条：按钮下方一排 6 枚样例图标（跟随当前主题实时变）
            let sy = iy + px(30.0) + px(8.0);
            g.icon_samples = Vec::new();
            let isz = px(32.0);
            let istep = px(42.0);
            for k in 0..6 {
                let x0 = g.slider.right - label_w - istep * 6 + (istep - isz) / 2 + istep * k as i32;
                if x0 + isz <= g.slider.right {
                    g.icon_samples.push(RECT { left: x0, top: sy + px(6.0), right: x0 + isz, bottom: sy + px(6.0) + isz });
                }
            }
        }
        crate::app::SETTINGS_TAB_HOTKEY => {
            g.rows = hotkey_rows(app, &content);
        }
        crate::app::SETTINGS_TAB_ADVANCED => {
            g.rows = advanced_rows(app, &content);
        }
        crate::app::SETTINGS_TAB_INTERACT => {
            // 第34轮：改成与「外观」页**同构**的左树 + 右预览。
            //
            // 之前这一页是四行参数平铺在左边、底部一条样例，参数一多就挤成
            // 一列竖条，用户扫不出"这些参数管的是同一件事"。左边树按语义
            // 分组（选中样式 / 选中样式续），右边是当前选中项的**大预览**，
            // 改参数当场看得见。
            let tree_w = (content.right - content.left) * 44 / 100;
            let rh = px_of(app, 28.0);
            let mut y = content.top;
            for r in interact_tree_rows(app) {
                if y + rh > content.bottom {
                    break;
                }
                g.tree_rows.push(RECT {
                    left: content.left,
                    top: y,
                    right: content.left + tree_w,
                    bottom: y + rh,
                });
                g.tree_ids.push(r.id.to_string());
                g.tree_actions.push(r.action);
                y += rh;
            }
            // 右边整块都是预览（上）+ 当前项的参数行（下）
            let pv_left = content.left + tree_w + px_of(app, 14.0);
            // 参数行紧贴内容区**底边**（用户要"左边设置项、右边预览"，
            // 参数行是预览的附属，不该浮在中间留一大片空白）。
            // 预览占"顶到参数行上方"之间的全部空间。
            let row_h2 = px_of(app, 34.0);
            let row_top = content.bottom - row_h2;
            g.preview = RECT {
                left: pv_left,
                top: content.top,
                right: content.right,
                bottom: row_top - px_of(app, 12.0),
            };
            let rh2 = row_h2;
            let cur = app.settings_interact_sel;
            if let Some(a) = cur.and_then(|id| interact_find(id, &interact_tree()).and_then(|n| n.action)) {
                g.rows.push(row(
                    RECT {
                        left: pv_left,
                        top: row_top,
                        right: content.right,
                        bottom: row_top + rh2,
                    },
                    interact_param_label(cur.unwrap()),
                    interact_param_value(app, cur.unwrap()),
                    a,
                    None,
                ));
            }
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
    // 画布里已放置的组件（后放的画在上层，所以倒序测）
    for (i, r) in canvas_item_rects(app, &g).into_iter().rev() {
        if inside(&r, x, y) {
            return Some(Hit::Canvas(i));
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
    // 右侧预览区：**先判格、再判空白** —— 空白处右键 = 目录右键。
    if inside(&g.prev_view, x, y) {
        let vy = y - g.prev_view.top + app.settings_prev_scroll;
        for (i, r) in g.prev_cells.iter().enumerate() {
            if x >= r.left && x < r.right && vy >= r.top && vy < r.bottom {
                return Some(Hit::Preview(i));
            }
        }
        return Some(Hit::PreviewArea);
    }
    if inside(&g.prev_panel, x, y) {
        return Some(Hit::PreviewArea);
    }
    for (i, r) in g.ow_rows.iter().enumerate() {
        if inside(r, x, y) {
            return Some(Hit::OpenWith(i));
        }
    }
    if g.ow_find.right > g.ow_find.left && inside(&g.ow_find, x, y) {
        return Some(Hit::OwFind);
    }
    for (i, r) in g.kind_chips.iter().enumerate() {
        if inside(r, x, y) {
            return Some(Hit::CtxKind(i as u8));
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
    // 图标主题 2 颗按钮：动作号 4101..=4102。
    // 排在圆角树之后、通用行之前 —— 它们在右列，与左侧树不重叠。
    for (i, r) in g.icon_btns.iter().enumerate() {
        if r.right > r.left && inside(r, x, y) {
            return Some(Hit::Row(4101 + i as u32));
        }
    }
    // 「交互」页：行右端 [−][+] 两颗小按钮，点按钮才动值；点标签区 = 无操作
    //（4099 占位，防它掉进 main.rs 里 3000+ 的系统菜单 Row 分支）。
    if app.settings_tab == crate::app::SETTINGS_TAB_INTERACT {
        let bw = (26.0 * app.metrics.scale).round() as i32;
        for r in &g.rows {
            if inside(&r.rect, x, y) {
                let a = r.action;
                return if x >= r.rect.right - bw {
                    Some(Hit::Row(a + 2)) // [+]
                } else if x >= r.rect.right - 2 * bw {
                    Some(Hit::Row(a + 1)) // [−]
                } else if a == 4009 {
                    Some(Hit::Row(4010)) // 空心行点标签也切换
                } else {
                    Some(Hit::Row(4099)) // 无操作占位
                };
            }
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
/// 树的可见行（展开的分组才列孩子）。
pub struct InteractRow {
    pub id: &'static str,
    pub depth: usize,
    pub name: &'static str,
    pub has_kids: bool,
    pub open: bool,
    /// Some = 这一行可调（点它把右侧参数区切到这一项）
    pub action: Option<u32>,
}

pub fn interact_tree_rows(app: &App) -> Vec<InteractRow> {
    let mut out = Vec::new();
    fn walk(app: &App, nodes: &[InteractNode], depth: usize, out: &mut Vec<InteractRow>) {
        for n in nodes {
            let open = app.settings_interact_open.contains(n.id);
            out.push(InteractRow {
                id: n.id,
                depth,
                name: n.name,
                has_kids: !n.kids.is_empty(),
                open,
                action: n.action,
            });
            if open {
                walk(app, &n.kids, depth + 1, out);
            }
        }
    }
    walk(app, &interact_tree(), 0, &mut out);
    out
}

/// 缩放后的像素（DIP → px）。局部小工具，避免每处都写 `(v * s).round()`。
fn px_of(app: &App, v: f32) -> i32 {
    (v * app.metrics.scale).round() as i32
}

/// 这个树节点是不是分组（有孩子）。
pub fn interact_has_kids(id: &str) -> bool {
    interact_find(id, &interact_tree()).map(|n| !n.kids.is_empty()).unwrap_or(false)
}

/// 运行期 id（来自 `Geom.tree_ids`，是 `String`）→ 树里的 `&'static str`。
/// 树里的 id 是编译期常量，这里做一次查表拿回静态串，`settings_interact_sel`
/// 才能存 `&'static str`（不引入 Rc/泄漏）。
pub fn interact_id_static(id: &str) -> Option<&'static str> {
    fn walk(id: &str, nodes: &[InteractNode]) -> Option<&'static str> {
        for n in nodes {
            if n.id == id {
                return Some(n.id);
            }
            if let Some(f) = walk(id, &n.kids) {
                return Some(f);
            }
        }
        None
    }
    walk(id, &interact_tree())
}

/// 当前选中项的参数名（「选中样式 › 竖线宽度」里只显示末级）。
fn interact_param_label(id: &str) -> &'static str {
    match id {
        "selbar.w" => "宽度（厚度）",
        "selbar.h" => "高度（0 = 跟行高）",
        "selbar.stroke" => "描边粗细（空心时生效）",
        "selbar.hollow" => "空心（只描边不填色）",
        _ => "参数",
    }
}

/// 当前选中项的参数值文本。
fn interact_param_value(app: &App, id: &str) -> String {
    let sb = &app.cfg.ui.sel_bar;
    match id {
        "selbar.w" => format!("{} DIP", sb.w),
        "selbar.h" => {
            if sb.h == 0 {
                "自动（跟行高）".to_string()
            } else {
                format!("{} DIP", sb.h)
            }
        }
        "selbar.stroke" => format!("{} DIP", sb.stroke),
        "selbar.hollow" => if sb.hollow { "开" } else { "关" }.to_string(),
        _ => String::new(),
    }
}

/// 「交互」页的树定义（第34轮：改成与「外观」同构的左树 + 右预览）。
///
/// 结构照「外观」页的三级圆角树：一组语义分组，组下是可调项。分组本身
/// 不可调（点它只是展开/收起）—— 交互设置不像圆角那样有"组级默认值"，
/// 硬造一个组级滑杆只会让人调了个不存在的东西。
pub struct InteractNode {
    pub id: &'static str,
    pub name: &'static str,
    /// Some(action) = 可调项；None = 纯分组
    pub action: Option<u32>,
    pub kids: Vec<InteractNode>,
}

fn inode(id: &'static str, name: &'static str, action: Option<u32>, kids: Vec<InteractNode>) -> InteractNode {
    InteractNode { id, name, action, kids }
}

pub fn interact_tree() -> Vec<InteractNode> {
    vec![inode(
        "selbar",
        "选中行首竖线",
        None,
        vec![
            inode("selbar.w", "宽度", Some(4000), vec![]),
            inode("selbar.h", "高度", Some(4003), vec![]),
            inode("selbar.stroke", "描边粗细", Some(4006), vec![]),
            inode("selbar.hollow", "空心", Some(4009), vec![]),
        ],
    )]
}

fn interact_find<'a>(id: &str, nodes: &'a [InteractNode]) -> Option<&'a InteractNode> {
    for n in nodes {
        if n.id == id {
            return Some(n);
        }
        if let Some(f) = interact_find(id, &n.kids) {
            return Some(f);
        }
    }
    None
}

/// 选中项的完整路径，例如 "选中样式 › 竖线宽度"。
pub fn interact_label(id: &str) -> String {
    fn walk(id: &str, nodes: &[InteractNode], acc: &mut Vec<&'static str>) -> bool {
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
    if walk(id, &interact_tree(), &mut acc) {
        acc.join(" › ")
    } else {
        id.to_string()
    }
}

/// 「交互」页四行参数：**选中行首竖线**的形状。
///
/// action 编码（避开 1000 插件 / 2000 快捷键 / 3000 系统菜单段）：
///   宽 base=4000（−=4001  +=4002）
///   高 base=4003（−=4004  +=4005）
///   粗细 base=4006（−=4007 +=4008）
///   空心 base=4009（−=4010 +=4011，两颗按钮都只是切换）
/// hit 里点行的标签区返回 4099（无操作占位，防止掉进别的 Row 分支）。
fn interact_rows(app: &App, content: &RECT) -> Vec<paint::SettingRow> {
    let s = app.metrics.scale;
    let rh = (34.0 * s).round() as i32;
    let sb = &app.cfg.ui.sel_bar;
    let h_val = if sb.h == 0 {
        "自动（跟行高）".to_string()
    } else {
        format!("{} DIP", sb.h)
    };
    let items: [(&str, String, u32); 4] = [
        ("竖线宽度（厚度）", format!("{} DIP", sb.w), 4000),
        ("竖线高度", h_val, 4003),
        ("描边粗细（空心时生效）", format!("{} DIP", sb.stroke), 4006),
        ("空心（只描边不填色）", if sb.hollow { "开".to_string() } else { "关".to_string() }, 4009),
    ];
    let mut out = Vec::new();
    let mut y = content.top;
    for (label, value, action) in items {
        out.push(
            row(
                RECT { left: content.left, top: y, right: content.right, bottom: y + rh },
                label,
                value,
                action,
                None,
            ),
        );
        y += rh + (6.0 * s).round() as i32;
    }
    out
}

/// 执行「交互」页参数行的一次步进（由 hit 的 Row action 直接驱动）。
pub fn apply_sel_bar_action(app: &mut App, a: u32) {
    match a {
        4001 | 4002 => {
            let d = if a == 4002 { 1 } else { -1 };
            app.set_sel_bar(|sb| sb.w = ((sb.w as i32) + d).clamp(1, 12) as u32);
        }
        4004 | 4005 => {
            let d = if a == 4005 { 1 } else { -1 };
            app.set_sel_bar(|sb| sb.h = ((sb.h as i32) + d).clamp(0, 48) as u32);
        }
        4007 | 4008 => {
            let d = if a == 4008 { 1 } else { -1 };
            app.set_sel_bar(|sb| sb.stroke = ((sb.stroke as i32) + d).clamp(1, 6) as u32);
        }
        4009..=4011 => {
            app.set_sel_bar(|sb| sb.hollow = !sb.hollow);
        }
        _ => {}
    }
}

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
            paint_interact(app, hdc, &g, th, s);
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

/// round 25：界面页 = 画布拖拽形态。
/// 上 = 小组件横排；右上 = 长组件横排；左侧垂直居中 = 预览画布
/// （**死数据** C:\Windows 详细列表，圆角渲染）；
/// 下 = 5 个整条矩形（标题栏/工具栏/状态栏/目录栏/用户自定义空行）。
fn paint_ui_tab(app: &mut App, hdc: HDC, g: &Geom, th: Theme, s: f32) {
    let px = |v: f32| (v * s).round() as i32;

    // —— 上排（小组件）——
    band_title(
        hdc,
        &g.ui_top,
        "小组件 —— 暗的=已装/不可用；点一下/拖到画布里就能添加",
        th,
        s,
    );
    for (i, r) in g.cards.iter().enumerate() {
        let id = g.card_ids[i];
        if id & 0x8000_0000 != 0 { continue; }
        // 只画"短"组件（长组件在 ui_right 调色板）
        let wid = (id & 0xFF) as usize;
        let dragging = matches!(&app.settings_drag, Some(d) if d.id == id);
        let hover = app.settings_hover_card == Some(id) && !dragging;
        let placed = widget_is_placed(app, wid as u16);
        draw_widget_ex(app, hdc, r, wid, th, s, hover, dragging, placed);
    }

    // —— 右侧（长组件）：标题 + 卡片（卡片已在 g.cards 里统一画）——
    band_title(
        hdc,
        &g.ui_right,
        "长组件（地址栏/搜索框等）",
        th,
        s,
    );

    // —— 预览画布：边框 + 死数据 C:\Windows 详细列表 + 圆角渲染 ——
    paint_preview_canvas(app, hdc, &g.ui_preview, th, s);

    // —— 画布里已经放下的组件：按画布坐标渲染（可拖动改位置）——
    let rects = canvas_item_rects(app, g);
    let drag_ci = app.settings_drag.as_ref().and_then(|d| d.from_canvas);
    for (i, r0) in rects.iter() {
        if Some(*i) == drag_ci {
            continue; // 拖动中的那个最后画（跟着鼠标走）
        }
        let Some(it) = app.ui_settings.canvas_items.get(*i) else { continue };
        draw_widget_ex(app, hdc, r0, it.wid as usize, th, s, false, false, false);
    }
    if let (Some(d), Some(ci)) = (&app.settings_drag, drag_ci) {
        if let Some((_, r0)) = rects.get(ci) {
            let dx = d.cur.0 - d.grab.0;
            let dy = d.cur.1 - d.grab.1;
            let r = RECT {
                left: r0.left + dx,
                top: r0.top + dy,
                right: r0.right + dx,
                bottom: r0.bottom + dy,
            };
            let wid = app.ui_settings.canvas_items.get(ci).map(|c| c.wid as usize);
            if let Some(wid) = wid {
                draw_widget_ex(app, hdc, &r, wid, th, s, false, true, false);
            }
        }
    }

    // —— 下排 5 个整条矩形（标题栏/工具栏/状态栏/目录栏/自定义空行）——
    let strip_titles = ["标题栏", "工具栏", "状态栏", "目录栏", "自定义空行"];
    for (i, sr) in g.ui_strips.iter().enumerate() {
        if sr.right <= sr.left || sr.bottom <= sr.top { continue; }
        // 死数据底板（surface）+ 1px border
        unsafe {
            let bg = CreateSolidBrush(th.surface);
            let _ = FillRect(hdc, sr, bg);
            let _ = DeleteObject(bg.into());
            let p = CreatePen(PS_SOLID, 1, th.border);
            let op = SelectObject(hdc, p.into());
            let ob = SelectObject(hdc, GetStockObject(NULL_BRUSH));
            let _ = Rectangle(hdc, sr.left, sr.top, sr.right, sr.bottom);
            let _ = SelectObject(hdc, ob);
            let _ = SelectObject(hdc, op);
            let _ = DeleteObject(p.into());
        }
        // 标题（左上）
        let title_r = RECT {
            left: sr.left + px(6.0),
            top: sr.top + px(2.0),
            right: sr.right - px(4.0),
            bottom: sr.top + px(16.0),
        };
        set_color(hdc, th.text_dim);
        paint::draw_text(hdc, strip_titles[i], title_r,
            DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER);
        // 内容：i<3 → bars[i] 里的组件名（死数据展示）；i==3 目录栏 → 死数据；i==4 空
        let sub_r = RECT {
            left: sr.left + px(6.0),
            top: sr.top + px(20.0),
            right: sr.right - px(6.0),
            bottom: sr.bottom - px(2.0),
        };
        set_color(hdc, th.text);
        let text = if i < 3 {
            let ids = app.ui_settings.bars.get(i).cloned().unwrap_or_default();
            ids.iter()
                .filter_map(|&w| WIDGETS.get(w as usize).map(|x| x.name))
                .collect::<Vec<_>>()
                .join("  ·  ")
        } else if i == 3 {
            "(目录栏：侧栏开/关按钮)".to_string()
        } else {
            "（空 —— 拖组件到这里）".to_string()
        };
        paint::draw_text(hdc, &text, sub_r, DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS);
    }

    // —— 拖动高亮：拖到预览 = accent 边框 + 拖到整条 = accent 边框 ——
    if let Some(d) = &app.settings_drag {
        let target = drop_target(app, g, d.cur.0, d.cur.1);
        if let Some(t) = target {
            let br = match t {
                DropTarget::Preview => g.ui_preview,
                DropTarget::Strip(i) => g.ui_strips[i],
            };
            if br.right > br.left && br.bottom > br.top {
                unsafe {
                    let p = CreatePen(PS_SOLID, 2, th.accent);
                    let op = SelectObject(hdc, p.into());
                    let ob = SelectObject(hdc, GetStockObject(NULL_BRUSH));
                    let _ = Rectangle(hdc, br.left, br.top, br.right, br.bottom);
                    let _ = SelectObject(hdc, ob);
                    let _ = SelectObject(hdc, op);
                    let _ = DeleteObject(p.into());
                }
            }
        }
    }
}

/// 画布里组件的像素尺寸（和 `canvas_item_rects` 保持一致）。
fn canvas_widget_w(app: &App, wid: u16) -> i32 {
    let s = app.metrics.scale;
    let w = WIDGETS.get(wid as usize).map(|wd| wd.w).unwrap_or(28.0);
    ((w * s).round() as i32).max((28.0 * s).round() as i32)
}
fn canvas_widget_h(app: &App, wid: u16) -> i32 {
    let s = app.metrics.scale;
    let long = WIDGETS.get(wid as usize).map(|wd| wd.long).unwrap_or(false);
    (if long { 30.0 } else { 28.0 } * s).round() as i32
}

/// 组件是否已经在**预览画布**里 —— 已存在的调色板卡片变灰、不可选中。
fn widget_is_placed(app: &App, wid: u16) -> bool {
    app.ui_settings.canvas_items.iter().any(|c| c.wid == wid)
}

/// 画布里已放置组件的矩形（绝对窗口坐标，由预览画布 + 归一化偏移推出来）。
///
/// **绘制与命中测试共用这一个函数** —— 拆成两份早晚会错位。
fn canvas_item_rects(app: &App, g: &Geom) -> Vec<(usize, RECT)> {
    let pv = g.ui_preview;
    let s = app.metrics.scale;
    let px = |v: f32| (v * s).round() as i32;
    let pw = pv.right - pv.left;
    let ph = pv.bottom - pv.top;
    if pw <= 0 || ph <= 0 {
        return Vec::new();
    }
    app.ui_settings
        .canvas_items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| {
            let wd = WIDGETS.get(it.wid as usize)?;
            let w = px(wd.w).max(px(28.0));
            let h = if wd.long { px(30.0) } else { px(28.0) };
            let left = pv.left + (it.nx * pw as f32).round() as i32;
            let top = pv.top + (it.ny * ph as f32).round() as i32;
            Some((
                i,
                RECT {
                    left,
                    top,
                    right: (left + w).min(pv.right),
                    bottom: (top + h).min(pv.bottom),
                },
            ))
        })
        .collect()
}

/// round 25：死数据画 C:\Windows 详细列表预览。
/// 上方顶 chrome（标题栏 26）+ 左侧侧栏（120）+ 中部列表（不可滚动）；
/// 圆角渲染（按外观页"全局"圆角）。
/// 预览画布死数据行/侧栏项的图标来源。
///
/// 目录一律走**真实路径**（`dir_icon` → `SHGetFileInfoW`，红线：不带
/// `SHGFI_USEFILEATTRIBUTES`，否则 desktop.ini 的自定义图标全丢）；
/// 回收站、此电脑这类没有真实路径的系统对象才走库存图标。
enum RowIcon<'a> {
    Real { path: &'a str, drive: bool },
    Stock(i32),
}

/// 取死数据行的图标。size 由调用方按**预览缩放**给出（第38轮：
/// 原来固定 14px，与真窗口的 16px 对不上，缩略图里图标偏小）。
fn preview_row_icon(app: &mut App, src: &RowIcon<'_>, size: i32) -> windows::Win32::UI::WindowsAndMessaging::HICON {
    let _ = size;
    use windows::Win32::UI::WindowsAndMessaging::HICON;
    use windows::Win32::UI::Shell::{SIID_DRIVEFIXED, SIID_FOLDER};
    match *src {
        RowIcon::Real { path, drive } => {
            let p = std::path::Path::new(path);
            if p.exists() {
                app.icons.dir_icon(p, false)
            } else {
                // 死数据里的 D:\ 这台机器可能没有：盘符行给硬盘图标，其余给通用文件夹
                crate::icons::stock_icon(
                    if drive { SIID_DRIVEFIXED.0 } else { SIID_FOLDER.0 },
                    false,
                )
                .unwrap_or_else(HICON::default)
            }
        }
        RowIcon::Stock(s) => crate::icons::stock_icon(s, false).unwrap_or_else(HICON::default),
    }
}

/// 预览专用的圆角半径：**DIP → 预览像素**。
///
/// `paint_preview_canvas` 是把真窗口按 k≈0.43 缩到几百像素宽的画布上，
/// 半径必须跟着缩。不缩的话 `rr()` 里的 `d = (radius*2).clamp(0, min(w,h))`
/// 会把侧栏行（9px 高）上的 radius≥4.5 全夹成同一个值 ——
/// 表现就是"拖滑杆预览不动"（第38轮 verify_settings20 C2）。
fn prad(app: &App, id: &str, scale: f32) -> u32 {
    ((app.radius_of(id) as f32) * scale).round().max(0.0) as u32
}

fn paint_preview_canvas(app: &mut App, hdc: HDC, r: &RECT, th: Theme, _s: f32) {
    if r.right <= r.left || r.bottom <= r.top { return; }
    let w = r.right - r.left;
    let h = r.bottom - r.top;

    // ────────────────────────────────────────────────────────────────
    // 第38轮：预览改成「真窗口的等比缩放」，不再手搓一套仿制尺寸。
    //
    // 原来这里把标题栏写死 26、侧栏 110、行高 18、图标 14…
    // 而真窗口是 34 / 157 / 21 / 16。两套数字各写一份，
    // 于是「预览」和「实际」永远对不上（用户三轮都提"预览有问题"）。
    //
    // 现在：所有几何都由 `Metrics::new(dpi)` 推导，与真窗口同一个
    // 数据源；只有 scale 被换成「把这块画布缩到多小」，比例关系完全保留
    // —— 也就是真窗口缩略后的样子，不是另画一张长得像的图。
    //
    // 缩放系数 = min(画布宽/1280, 画布高/850)，保证整幅窗口完整装进画布。
    // ────────────────────────────────────────────────────────────────
    const REF_W: f32 = 1280.0;
    const REF_H: f32 = 850.0;
    let k = (w as f32 / REF_W).min(h as f32 / REF_H).max(0.04);
    let vm = crate::theme::Metrics::new((96.0 * k) as u32);
    let px = |v: f32| (v * vm.scale).round() as i32;

    // 死数据行对应的真实路径（图标按真实路径问 Shell，与真窗口同款）
    let up = std::env::var("USERPROFILE").unwrap_or_default();
    let p_profile = up.clone();
    let p_desktop = format!("{}\\Desktop", up);
    let p_downloads = format!("{}\\Downloads", up);
    let p_docs = format!("{}\\Documents", up);
    let p_pics = format!("{}\\Pictures", up);
    let p_music = format!("{}\\Music", up);
    let p_videos = format!("{}\\Videos", up);
    let p_win = "C:\\Windows".to_string();
    let p_c = "C:\\".to_string();
    let p_d = "D:\\".to_string();

    // 真实布局：侧栏开着、状态栏开着 —— 与用户截图里的真窗口一致
    let lay = crate::paint::compute_layout(w, h, &vm, true, true);
    let isz = ((16.0 * vm.scale).round() as i32).max(7);

    unsafe {
        let font = paint::ui_font(vm.scale);
        let oldf = SelectObject(hdc, font.into());
        SetBkMode(hdc, TRANSPARENT);

        // 画布原点平移（第38轮踩过）：`compute_layout` 算出来的矩形全部
        // 从 (0,0) 起算，而这块画布在窗口里的位置是 (r.left, r.top)。
        // 不平移的话，整个迷你窗口会画到**窗口左上角**压住真标题栏/工具栏，
        // 而画布框里只剩一个空边框 —— 用户截图里就是这个现象。
        let mut old_org = POINT::default();
        let _ = GetViewportOrgEx(hdc, &mut old_org);
        let _ = SetViewportOrgEx(hdc, r.left, r.top, None);

        // ── 标题栏（title_bg 纯黑，与真窗口一致）──
        let tb = CreateSolidBrush(th.title_bg);
        let _ = FillRect(hdc, &lay.title, tb);
        let _ = DeleteObject(tb.into());
        mini_box_icon(hdc, lay.sidebar_toggle, th, vm.scale);
        // 标签：第一个激活（亮底 + 2px accent 下划线），其余暗色
        let tab_w = (((lay.tabs.right - lay.tabs.left) as f32) * 0.22).round() as i32;
        for (ti, name) in ["此电脑", "下载", "图片"].iter().enumerate() {
            let ti = ti as i32;
            let tr = RECT {
                left: lay.tabs.left + ti * tab_w,
                top: lay.title.top,
                right: lay.tabs.left + (ti + 1) * tab_w,
                bottom: lay.title.bottom,
            };
            if ti == 0 {
                // 激活标签：surface 底（圆角走 title.tab，真窗口同一 id）
                // + **底部 2px accent 指示条**（照 paint_tab_strip，不是整圈描边）。
                rr(hdc, &tr, prad(app, "title.tab", vm.scale), th.surface, None);
                let line = RECT { left: tr.left, top: tr.bottom - 2, right: tr.right, bottom: tr.bottom };
                let ab2 = CreateSolidBrush(th.accent);
                let _ = FillRect(hdc, &line, ab2);
                let _ = DeleteObject(ab2.into());
            }
            set_color(hdc, if ti == 0 { th.text } else { th.text_dim });
            let lr = RECT {
                left: tr.left + px(8.0),
                top: tr.top,
                right: tr.right - px(8.0),
                bottom: tr.bottom,
            };
            paint::draw_text(hdc, name, lr,
                DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS);
        }
        // 右侧：菜单键（三条杠）+ 三个窗口键（与真窗口同宽同高）
        mini_glyph(hdc, lay.menu_btn, th, vm.scale, MiniGlyph::Hamburger);
        mini_caption(hdc, &lay.min_btn, th, vm.scale, MiniCap::Min);
        mini_caption(hdc, &lay.max_btn, th, vm.scale, MiniCap::Max);
        mini_caption(hdc, &lay.close_btn, th, vm.scale, MiniCap::Close);

        // ── 工具栏（window_bg）──
        let wb = CreateSolidBrush(th.window_bg);
        let _ = FillRect(hdc, &lay.toolbar, wb);
        let _ = DeleteObject(wb.into());
        for (i, nav) in [lay.back, lay.forward, lay.up].iter().enumerate() {
            rr(hdc, nav, prad(app, "toolbar.button", vm.scale), th.chrome_bg, None);
            mini_arrow(hdc, nav, th, vm.scale, i);
        }
        // 地址栏（surface + 1px 边框）—— 圆角走 address，拖滑杆这里跟着变
        rr(hdc, &lay.address, prad(app, "address", vm.scale), th.surface, Some(th.border));
        set_color(hdc, th.text);
        paint::draw_text(hdc, "此电脑",
            RECT { left: lay.address.left + px(8.0), top: lay.address.top,
                   right: lay.address.left + px(70.0), bottom: lay.address.bottom },
            DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER);
        // 搜索框（surface + 1px 边框）—— 圆角走 address.edit
        rr(hdc, &lay.search, prad(app, "address.edit", vm.scale), th.surface, Some(th.border));
        set_color(hdc, th.text_dim);
        paint::draw_text(hdc, "搜索 此电脑",
            RECT { left: lay.search.left + px(22.0), top: lay.search.top,
                   right: lay.search.right - px(6.0), bottom: lay.search.bottom },
            DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS);

        // ── 侧栏（chrome_bg）：快速访问 + 「此电脑」树 ──
        let cbg = CreateSolidBrush(th.chrome_bg);
        let _ = FillRect(hdc, &lay.sidebar, cbg);
        let _ = DeleteObject(cbg.into());
        let row_h = paint::sidebar_row_h(vm.scale);
        let item_x = lay.sidebar.left + px(10.0);
        set_color(hdc, th.text_dim);
        paint::draw_text(hdc, "快速访问",
            RECT { left: item_x, top: lay.sidebar.top + px(6.0),
                   right: lay.sidebar.right - px(4.0),
                   bottom: lay.sidebar.top + px(6.0) + row_h },
            DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER);
        let quick: [(&str, RowIcon<'_>); 4] = [
            ("桌面", RowIcon::Real { path: &p_desktop, drive: false }),
            ("下载", RowIcon::Real { path: &p_downloads, drive: false }),
            ("文档", RowIcon::Real { path: &p_docs, drive: false }),
            ("图片", RowIcon::Real { path: &p_pics, drive: false }),
        ];
        let mut sy = lay.sidebar.top + px(6.0) + row_h;
        for (qi, (name, ic)) in quick.iter().enumerate() {
            // 第 2 条画成「悬停」态：它是 sidebar.hover 唯一的演示位 ——
            // 没有这一块，选中「鼠标悬停圆角」拖滑杆预览不会有任何反应。
            if qi == 1 {
                let hr = RECT {
                    left: item_x - px(6.0),
                    top: sy,
                    right: lay.sidebar.right - px(6.0),
                    bottom: sy + row_h,
                };
                rr(hdc, &hr, prad(app, "sidebar.hover", vm.scale), th.row_hover, None);
            }
            let ih = preview_row_icon(app, ic, isz);
            if !ih.0.is_null() {
                app.icons.draw(hdc, ih, item_x, sy + (row_h - isz) / 2, isz);
            }
            set_color(hdc, th.text);
            paint::draw_text(hdc, name,
                RECT { left: item_x + isz + px(6.0), top: sy,
                       right: lay.sidebar.right - px(4.0), bottom: sy + row_h },
                DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS);
            sy += row_h;
        }
        // 「此电脑」：展开三角 + 图标 + 文字，下面挂三个盘（带树线）
        sy += px(4.0);
        let group_top = sy;
        mini_tri(hdc, item_x + px(4.0), sy + row_h / 2, vm.scale, th.text_dim);
        let pc_ih = preview_row_icon(app,
            &RowIcon::Stock(windows::Win32::UI::Shell::SIID_DESKTOPPC.0), isz);
        if !pc_ih.0.is_null() {
            app.icons.draw(hdc, pc_ih, item_x + px(12.0),
                sy + (row_h - isz) / 2, isz);
        }
        set_color(hdc, th.text);
        paint::draw_text(hdc, "此电脑",
            RECT { left: item_x + px(12.0) + isz + px(6.0), top: sy,
                   right: lay.sidebar.right - px(4.0), bottom: sy + row_h },
            DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS);
        sy += row_h;
        // 树竖线：从「此电脑」行下沿贯穿到最后一个盘
        let tree_x = item_x + px(12.0) + isz / 2;
        let drives: [(&str, &str); 3] = [
            ("本地磁盘 (C:)", &p_c),
            ("本地磁盘 (D:)", &p_d),
            ("本地磁盘 (E:)", "E:\\"),
        ];
        let n = drives.len() as i32;
        let tree_bottom = sy + row_h * (n - 1) + row_h / 2;
        let tpen = CreatePen(PS_SOLID, 1, th.border);
        let top_op = SelectObject(hdc, tpen.into());
        let _ = MoveToEx(hdc, tree_x, group_top + row_h, None);
        let _ = LineTo(hdc, tree_x, tree_bottom);
        let _ = SelectObject(hdc, top_op);
        let _ = DeleteObject(tpen.into());
        for (di, (name, path)) in drives.iter().enumerate() {
            if sy + row_h > lay.sidebar.bottom { break; }
            // 第一个盘 = 选中态（sidebar.selected 的演示位）
            if di == 0 {
                let sr = RECT {
                    left: item_x - px(6.0),
                    top: sy,
                    right: lay.sidebar.right - px(6.0),
                    bottom: sy + row_h,
                };
                rr(hdc, &sr, prad(app, "sidebar.selected", vm.scale), th.selection_bg, None);
            }
            let tpen2 = CreatePen(PS_SOLID, 1, th.border);
            let top2 = SelectObject(hdc, tpen2.into());
            let _ = MoveToEx(hdc, tree_x, sy + row_h / 2, None);
            let _ = LineTo(hdc, tree_x + px(14.0), sy + row_h / 2);
            let _ = SelectObject(hdc, top2);
            let _ = DeleteObject(tpen2.into());
            let ih = preview_row_icon(app,
                &RowIcon::Real { path, drive: true }, isz);
            if !ih.0.is_null() {
                app.icons.draw(hdc, ih, tree_x + px(14.0),
                    sy + (row_h - isz) / 2, isz);
            }
            set_color(hdc, th.text);
            paint::draw_text(hdc, name,
                RECT { left: tree_x + px(14.0) + isz + px(6.0), top: sy,
                       right: lay.sidebar.right - px(4.0), bottom: sy + row_h },
                DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS);
            sy += row_h;
            let _ = di;
        }
        // 回收站
        if sy + row_h + px(4.0) <= lay.sidebar.bottom {
            sy += px(6.0);
            let ri = preview_row_icon(app,
                &RowIcon::Stock(windows::Win32::UI::Shell::SIID_RECYCLER.0), isz);
            if !ri.0.is_null() {
                app.icons.draw(hdc, ri, item_x, sy + (row_h - isz) / 2, isz);
            }
            set_color(hdc, th.text);
            paint::draw_text(hdc, "回收站",
                RECT { left: item_x + isz + px(6.0), top: sy,
                       right: lay.sidebar.right - px(4.0), bottom: sy + row_h },
                DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS);
        }

        // ── 内容区（window_bg）：表头 + 详细列表 4 列 ──
        let vbg = CreateSolidBrush(th.window_bg);
        let _ = FillRect(hdc, &lay.view, vbg);
        let _ = DeleteObject(vbg.into());
        // 右上角视图切换键（详情 / 大图标），与真窗口同一位置
        let vbtn = px(30.0);
        let vby = lay.view.top + px(8.0);
        for k in 0..2 {
            let vr = RECT { left: lay.view.right - px(10.0) - vbtn * (2 - k),
                            top: vby,
                            right: lay.view.right - px(10.0) - vbtn * (1 - k),
                            bottom: vby + vbtn };
            rr(hdc, &vr, prad(app, "toolbar.button", vm.scale), th.chrome_bg, None);
            mini_glyph(hdc, vr, th, vm.scale,
                if k == 0 { MiniGlyph::ListView } else { MiniGlyph::IconView });
        }
        // 表头（surface）+ 四列
        let header_h = px(26.0);
        let header_r = RECT { left: lay.view.left, top: lay.view.top,
                              right: lay.view.right,
                              bottom: lay.view.top + header_h };
        let hb = CreateSolidBrush(th.surface);
        let _ = FillRect(hdc, &header_r, hb);
        let _ = DeleteObject(hb.into());
        // 列宽与真窗口同比例：名称 40% / 修改日期 23% / 类型 15% / 大小 22%
        let vw = lay.view.right - lay.view.left;
        let col_date_w = vw * 23 / 100;
        let col_size_w = vw * 22 / 100;
        let col_kind_w = vw * 15 / 100;
        let col_name_w = vw - col_date_w - col_size_w - col_kind_w;
        // 表头「名称」要给图标让位（真窗口表头文字与行图标左对齐）
        let name_pad = px(8.0) + isz + px(5.0);
        set_color(hdc, th.text_dim);
        let mut hx = lay.view.left;
        for (i, (label, w)) in [
            ("名称", col_name_w), ("修改日期", col_date_w),
            ("类型", col_kind_w), ("大小", col_size_w),
        ].iter().enumerate()
        {
            let x0 = if i == 0 { hx + name_pad } else { hx };
            paint::draw_text(hdc, label,
                RECT { left: x0, top: header_r.top,
                       right: hx + w - px(8.0), bottom: header_r.bottom },
                DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS);
            hx += w;
        }
        // 死数据 11 行（行高 = 真实 row_h）
        let rows: [(&str, &str, &str, &str, RowIcon<'_>); 11] = [
            ("文件夹", "2026/09/03 14:22", "文件夹", "", RowIcon::Real { path: &p_win, drive: false }),
            ("主文件夹", "2026/09/03 14:22", "文件夹", "", RowIcon::Real { path: &p_profile, drive: false }),
            ("桌面", "2026/09/03 14:22", "文件夹", "", RowIcon::Real { path: &p_desktop, drive: false }),
            ("下载", "2026/09/03 14:22", "文件夹", "", RowIcon::Real { path: &p_downloads, drive: false }),
            ("文档", "2026/09/03 14:22", "文件夹", "", RowIcon::Real { path: &p_docs, drive: false }),
            ("图片", "2026/09/03 14:22", "文件夹", "", RowIcon::Real { path: &p_pics, drive: false }),
            ("音乐", "2026/09/03 14:22", "文件夹", "", RowIcon::Real { path: &p_music, drive: false }),
            ("视频", "2026/09/03 14:22", "文件夹", "", RowIcon::Real { path: &p_videos, drive: false }),
            ("本地磁盘 (C:)", "2026/09/03 14:22", "本地磁盘", "221 GB / 369 GB", RowIcon::Real { path: &p_c, drive: true }),
            ("本地磁盘 (D:)", "2026/09/03 14:22", "本地磁盘", "393 GB / 954 GB", RowIcon::Real { path: &p_d, drive: true }),
            ("回收站", "2026/09/03 14:22", "系统文件夹", "", RowIcon::Stock(
                if app.recycle_items.is_empty() {
                    windows::Win32::UI::Shell::SIID_RECYCLER.0
                } else {
                    windows::Win32::UI::Shell::SIID_RECYCLERFULL.0
                })),
        ];
        let row_h2 = vm.row_h;
        let mut ry = header_r.bottom;
        for (ri, row) in rows.iter().enumerate() {
            if ry + row_h2 > lay.view.bottom { break; }
            // 第 1 行选中 / 第 2 行悬停：content.selected、content.hover 的演示位。
            // 圆角走真窗口同一套 id，拖滑杆时这两行的角跟着变。
            let row_r = RECT {
                left: lay.view.left,
                top: ry,
                right: lay.view.right,
                bottom: ry + row_h2,
            };
            if ri == 0 {
                rr(hdc, &row_r, prad(app, "content.selected", vm.scale), th.selection_bg, None);
            } else if ri == 1 {
                rr(hdc, &row_r, prad(app, "content.hover", vm.scale), th.row_hover, None);
            }
            let ih = preview_row_icon(app, &row.4, isz);
            if !ih.0.is_null() {
                app.icons.draw(hdc, ih, lay.view.left + px(8.0),
                    ry + (row_h2 - isz) / 2, isz);
            }
            set_color(hdc, th.text);
            let mut cx = lay.view.left;
            for (i, txt) in [row.0, row.1, row.2, row.3].iter().enumerate() {
                let cw = [col_name_w, col_date_w, col_kind_w, col_size_w][i];
                let tx0 = if i == 0 {
                    lay.view.left + px(8.0) + isz + px(5.0)
                } else {
                    cx
                };
                paint::draw_text(hdc, txt,
                    RECT { left: tx0, top: ry, right: cx + cw - px(8.0),
                           bottom: ry + row_h2 },
                    DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS);
                cx += cw;
            }
            ry += row_h2;
        }

        // ── 状态栏（status_bg）──
        let stb = CreateSolidBrush(th.status_bg);
        let _ = FillRect(hdc, &lay.status, stb);
        let _ = DeleteObject(stb.into());
        set_color(hdc, th.text_dim);
        paint::draw_text(hdc, "11 个项目",
            RECT { left: lay.status.left + px(8.0), top: lay.status.top,
                   right: lay.status.right - px(70.0), bottom: lay.status.bottom },
            DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER);
        let vb2 = px(18.0);
        let vy = lay.status.top + (lay.status.bottom - lay.status.top - vb2) / 2;
        for k in 0..2 {
            let vr = RECT { left: lay.status.right - px(10.0) - vb2 * (2 - k),
                            top: vy,
                            right: lay.status.right - px(10.0) - vb2 * (1 - k),
                            bottom: vy + vb2 };
            rr(hdc, &vr, prad(app, "status.button", vm.scale), th.chrome_bg, None);
            mini_glyph(hdc, vr, th, vm.scale,
                if k == 0 { MiniGlyph::ListView } else { MiniGlyph::IconView });
        }

        // ── 外框 1px 描边 + 圆角（最后画，压住所有内容）──
        // 这里用的是**窗口绝对坐标** r，所以先把原点还原再画，
        // 否则边框会跟着平移偏出画布。
        let _ = SetViewportOrgEx(hdc, old_org.x, old_org.y, None);
        let rad = prad(app, "global", vm.scale) as i32;
        let d = (rad * 2).min(w).min(h);
        let fp = CreatePen(PS_SOLID, 1, th.border);
        let fop = SelectObject(hdc, fp.into());
        let fob = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        let _ = RoundRect(hdc, r.left, r.top, r.right, r.bottom, d, d);
        let _ = SelectObject(hdc, fob);
        let _ = SelectObject(hdc, fop);
        let _ = DeleteObject(fp.into());

        SelectObject(hdc, oldf);
    }
}

/// 预览里那些小图标的画法，**全部按 `vm.scale` 等比缩放**，与真窗口
/// 的 `draw_chrome_glyph` / `draw_caption_glyph` 同一套口径。
///
/// 为什么不用 Segoe MDL2 字形：字体在极小尺寸下（画布只有几百像素宽，
/// scale 常常 < 0.5）字形会被 hinting 挤变形，而真窗口里同一枚字形
/// 在正常尺寸下是好的。改成矢量线段后缩放到多小都不失真。
enum MiniGlyph { Hamburger, ListView, IconView }
enum MiniCap { Min, Max, Close }

fn mini_pen(th: Theme, weight: i32) -> HPEN {
    unsafe { CreatePen(PS_SOLID, weight.max(1), th.text) }
}

fn mini_glyph(hdc: HDC, r: RECT, th: Theme, s: f32, g: MiniGlyph) {
    let cx = (r.left + r.right) / 2;
    let cy = (r.top + r.bottom) / 2;
    unsafe {
        let pen = mini_pen(th, ((1.4 * s).round() as i32).max(1));
        let op = SelectObject(hdc, pen.into());
        match g {
            MiniGlyph::Hamburger => {
                let w = ((11.0 * s).round() as i32).max(5);
                let step = ((4.0 * s).round() as i32).max(3);
                for k in -1..=1 {
                    let y = cy + k * step;
                    let _ = MoveToEx(hdc, cx - w / 2, y, None);
                    let _ = LineTo(hdc, cx + w / 2, y);
                }
            }
            MiniGlyph::ListView => {
                let w = ((11.0 * s).round() as i32).max(5);
                let step = ((4.0 * s).round() as i32).max(3);
                for k in -1..=1 {
                    let y = cy + k * step;
                    let _ = MoveToEx(hdc, cx - w / 2, y, None);
                    let _ = LineTo(hdc, cx + w / 2, y);
                }
            }
            MiniGlyph::IconView => {
                // 四宫格：四条短斜线交叉成 X（与真窗口"大图标"按钮同款观感）
                let b = ((4.0 * s).round() as i32).max(3);
                let g2 = b * 2;
                for (dx, dy) in [(-1, -1), (1, -1), (-1, 1), (1, 1)] {
                    let x = cx + dx * g2;
                    let y = cy + dy * g2;
                    let _ = MoveToEx(hdc, x - b / 2, y - b / 2, None);
                    let _ = LineTo(hdc, x + b / 2, y + b / 2);
                }
            }
        }
        let _ = SelectObject(hdc, op);
        let _ = DeleteObject(pen.into());
    }
}

fn mini_caption(hdc: HDC, r: &RECT, th: Theme, s: f32, c: MiniCap) {
    let cx = (r.left + r.right) / 2;
    let cy = (r.top + r.bottom) / 2;
    unsafe {
        let pen = mini_pen(th, ((1.4 * s).round() as i32).max(1));
        let op = SelectObject(hdc, pen.into());
        let w = ((11.0 * s).round() as i32).max(4);
        match c {
            MiniCap::Min => {
                let _ = MoveToEx(hdc, cx - w / 2, cy + w / 4, None);
                let _ = LineTo(hdc, cx + w / 2, cy + w / 4);
            }
            MiniCap::Max => {
                let _ = MoveToEx(hdc, cx - w / 2, cy - w / 2, None);
                let _ = LineTo(hdc, cx + w / 2, cy - w / 2);
                let _ = LineTo(hdc, cx + w / 2, cy + w / 2);
                let _ = LineTo(hdc, cx - w / 2, cy + w / 2);
                let _ = LineTo(hdc, cx - w / 2, cy - w / 2);
            }
            MiniCap::Close => {
                let _ = MoveToEx(hdc, cx - w / 2, cy - w / 2, None);
                let _ = LineTo(hdc, cx + w / 2, cy + w / 2);
                let _ = MoveToEx(hdc, cx + w / 2, cy - w / 2, None);
                let _ = LineTo(hdc, cx - w / 2, cy + w / 2);
            }
        }
        let _ = SelectObject(hdc, op);
        let _ = DeleteObject(pen.into());
    }
}

/// 侧栏折叠键的方框图标（真窗口是四条短线组成的框 + 一条竖分隔）。
fn mini_box_icon(hdc: HDC, r: RECT, th: Theme, s: f32) {
    let cx = (r.left + r.right) / 2;
    let cy = (r.top + r.bottom) / 2;
    unsafe {
        let pen = mini_pen(th, ((1.4 * s).round() as i32).max(1));
        let op = SelectObject(hdc, pen.into());
        let w = ((14.0 * s).round() as i32).max(6);
        let h = ((11.0 * s).round() as i32).max(5);
        let _ = MoveToEx(hdc, cx - w / 2, cy - h / 2, None);
        let _ = LineTo(hdc, cx + w / 2, cy - h / 2);
        let _ = LineTo(hdc, cx + w / 2, cy + h / 2);
        let _ = LineTo(hdc, cx - w / 2, cy + h / 2);
        let _ = LineTo(hdc, cx - w / 2, cy - h / 2);
        let _ = MoveToEx(hdc, cx - w / 6, cy - h / 2, None);
        let _ = LineTo(hdc, cx - w / 6, cy + h / 2);
        let _ = SelectObject(hdc, op);
        let _ = DeleteObject(pen.into());
    }
}

/// 侧栏树的展开三角（实心朝下）。
fn mini_tri(hdc: HDC, x: i32, y: i32, s: f32, color: COLORREF) {
    unsafe {
        let b = CreateSolidBrush(color);
        let w = ((8.0 * s).round() as i32).max(4);
        let h = ((5.0 * s).round() as i32).max(3);
        let mut pts = [
            POINT { x: x - w / 2, y: y - h / 2 },
            POINT { x: x + w / 2, y: y - h / 2 },
            POINT { x: x, y: y + h / 2 },
        ];
        let _ = Polygon(hdc, &mut pts);
        let _ = DeleteObject(b.into());
    }
}

/// 工具栏导航键的箭头（0=返回 1=前进 2=上级）。
///
/// 画成一个**实心三角**而不是两条线段 —— 线段版本要为三种朝向各推一遍
/// 端点公式，写出来自己都看错（第一版就多给/少给了 MoveToEx 的第 4 参）。
/// 三角只要算三个点，三种朝向共 6 行。
fn mini_arrow(hdc: HDC, r: &RECT, th: Theme, s: f32, kind: usize) {
    let cx = (r.left + r.right) / 2;
    let cy = (r.top + r.bottom) / 2;
    let a = ((5.5 * s).round() as i32).max(3);
    unsafe {
        let b = CreateSolidBrush(th.text);
        // (tip, base1, base2)：tip 是箭头指向的那一端
        let (tip, b1, b2) = match kind {
            0 => ((cx - a, cy), (cx + a / 2, cy - a), (cx + a / 2, cy + a)),
            1 => ((cx + a, cy), (cx - a / 2, cy - a), (cx - a / 2, cy + a)),
            _ => ((cx, cy - a), (cx - a, cy + a / 2), (cx + a, cy + a / 2)),
        };
        let mut pts = [
            POINT { x: tip.0, y: tip.1 },
            POINT { x: b1.0, y: b1.1 },
            POINT { x: b2.0, y: b2.1 },
        ];
        let _ = Polygon(hdc, &mut pts);
        let _ = DeleteObject(b.into());
    }
}


/// 落点判定（round 25 画布形态）：
/// - 落在预览画布里 → DropTarget::Preview
/// - 落在某个整条矩形里 → DropTarget::Strip(i)
/// - 否则 None
#[derive(Copy, Clone, Debug)]
enum DropTarget {
    Preview,
    Strip(usize),
}

fn drop_target(_app: &App, g: &Geom, x: i32, y: i32) -> Option<DropTarget> {
    let _ = _app;
    // 预览
    let pv = g.ui_preview;
    if pv.right > pv.left && pv.bottom > pv.top
        && x >= pv.left && x < pv.right && y >= pv.top && y < pv.bottom
    {
        return Some(DropTarget::Preview);
    }
    // 整条
    for (i, sr) in g.ui_strips.iter().enumerate() {
        if sr.right > sr.left && sr.bottom > sr.top
            && x >= sr.left && x < sr.right && y >= sr.top && y < sr.bottom
        {
            return Some(DropTarget::Strip(i));
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
#[allow(dead_code)]
fn preview_layout(app: &App, host: &RECT) -> paint::Layout {
    let mut pl = paint::layout_sized(app, host.right - host.left, host.bottom - host.top);
    paint::translate_layout(&mut pl, host.left, host.top);
    pl
}

/// 预览里三条带（标题栏/工具栏/状态栏）的屏幕矩形。
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
fn paint_look(app: &mut App, hdc: HDC, g: &Geom, th: Theme, s: f32) {
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

    // —— 实时预览（真窗口缩略图 + 当前项高亮框）——
    // sel 必须先 clone：`&app.settings_radius_sel` 与 `draw_preview(app, &mut …)`
    // 在同一个调用里对同一个 app 一借一借可变，借用检查不过。
    let sel = app.settings_radius_sel.clone();
    draw_preview(app, hdc, &g.preview, &sel, th, s);

    // —— 滑杆 ——
    draw_slider(app, hdc, &g.slider, th, s);

    // —— 图标主题（第48轮）：风格/颜色两行按钮 + 样例条 ——
    draw_icon_section(app, hdc, g, th, s);
}

/// 「外观」页底部的图标主题区块：单行两枚分段按钮 + 一排实时样例。
///
/// 样例直接走 `IconCache` —— 主题一切换，样例跟着变，所见即所得。
fn draw_icon_section(app: &mut App, hdc: HDC, g: &Geom, th: Theme, s: f32) {
    if g.icon_btns[0].right <= g.icon_btns[0].left {
        return;
    }
    let px = |v: f32| (v * s).round() as i32;
    let font = paint::ui_font(s);

    // 行标签（左对齐在按钮组左边）
    let names: [&str; 2] = ["默认", "优化"];
    let cur = if app.cfg.ui.icon_theme == "optimized" { 1 } else { 0 };
    let btn0 = g.icon_btns[0];
    let mut lr = RECT {
        left: btn0.left - px(58.0),
        top: btn0.top,
        right: btn0.left - px(6.0),
        bottom: btn0.bottom,
    };
    text(hdc, &lr, "图标主题", th.text_dim, font);
    for k in 0..2 {
        let r = g.icon_btns[k];
        let selected = cur == k;
        let hover = app.settings_hover == Some(4101 + k as u32);
        let bg = if selected {
            th.selection_bg
        } else if hover {
            th.hover_bg
        } else {
            th.surface
        };
        // 按钮底：圆角跟「标题栏按钮」那档走（都是 chrome 小控件）
        unsafe { paint::fill_radius(app, hdc, "title.button", &r, bg, None) };
        if selected {
            // 选中态加一条强调下划线，与标签激活态同一套语言
            fill_rect(
                hdc,
                &RECT { left: r.left + px(4.0), top: r.bottom - px(3.0), right: r.right - px(4.0), bottom: r.bottom - px(1.0) },
                th.accent,
            );
        }
        let mut tr = r;
        tr.top += px(1.0);
        text(hdc, &tr, names[k], if selected { th.text } else { th.text_dim }, font);
    }

    // —— 样例条：6 枚当前主题的图标（文件夹/文本/图片/压缩包/驱动器/回收站）——
    if g.icon_samples.is_empty() {
        return;
    }
    let label_r = RECT {
        left: g.icon_samples[0].left - px(58.0),
        top: g.icon_samples[0].top,
        right: g.icon_samples[0].left - px(6.0),
        bottom: g.icon_samples[0].bottom,
    };
    text(hdc, &label_r, "样例", th.text_dim, font);
    let icons: Vec<HICON> = vec![
        app.icons.icon(true, "", true),
        app.icons.icon(false, "txt", true),
        app.icons.icon(false, "png", true),
        app.icons.icon(false, "zip", true),
        crate::icons::stock_icon(windows::Win32::UI::Shell::SIID_DRIVEFIXED.0, true)
            .unwrap_or_default(),
        crate::icons::stock_icon(
            windows::Win32::UI::Shell::SIID_RECYCLER.0,
            true,
        )
        .unwrap_or_default(),
    ];
    for (i, r) in g.icon_samples.iter().enumerate() {
        if let Some(h) = icons.get(i) {
            if !h.0.is_null() {
                app.icons.draw(hdc, *h, r.left, r.top, r.right - r.left);
            }
        }
    }
}

/// 「交互」页：**选中行首竖线**的四个参数 + 实时样例。
///
/// 所见即所得：样例区就是一条选中态行背景 + 按当前参数画的竖线，
/// 参数一改（main.rs 的 Row 分支 → apply_sel_bar_action → InvalidateRect）
/// 立刻重画，真窗口的列表行/磁贴走同一个 draw_sel_bar，所以样例即真窗口。
fn paint_interact(app: &App, hdc: HDC, g: &Geom, th: Theme, s: f32) {
    let px = |v: f32| (v * s).round() as i32;
    let btn_w = (26.0 * s).round() as i32;

    // —— 左侧树（与外观页同一套画法：缩进 + 展开三角 + 行尾当前值）——
    let rows = interact_tree_rows(app);
    for (i, r) in g.tree_rows.iter().enumerate() {
        let info = rows.get(i);
        let id = g.tree_ids.get(i).map(|s| s.as_str()).unwrap_or("");
        // 选中判定：树节点 id 与当前选中项相同**或同属一棵子树**
        let selected = app
            .settings_interact_sel
            .map(|sel| sel == id || sel.starts_with(&format!("{id}.")))
            .unwrap_or(false);
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
        let indent = px(14.0) * depth as i32 + px(6.0);
        let mut gr = *r;
        gr.left += indent;
        gr.right = gr.left + px(16.0);
        if info.map(|v| v.has_kids).unwrap_or(false) {
            let glyph = if info.map(|v| v.open).unwrap_or(false) { "\u{E70D}" } else { "\u{E76C}" };
            text(hdc, &gr, glyph, th.text_dim, paint::glyph_font(s));
        }
        let mut tr = *r;
        tr.left = gr.right;
        tr.right = r.right - px(66.0);
        set_color(hdc, if selected { th.text } else { th.text_dim });
        paint::draw_text(
            hdc,
            info.map(|v| v.name).unwrap_or("?"),
            tr,
            DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS,
        );
        // 行尾写当前值（可调项才有 —— 分组行右边留空）
        if info.and_then(|v| v.action).is_some() && !id.is_empty() {
            let mut vr = *r;
            vr.left = tr.right;
            vr.right = r.right - px(6.0);
            text(hdc, &vr, &interact_param_value(app, id), th.text_dim, paint::ui_font(s));
        }
    }

    // —— 右侧：当前选中项的大预览 ——
    let pv = g.preview;
    if pv.right > pv.left && pv.bottom > pv.top {
        // 标题：完整路径（"选中样式 › 竖线宽度"）
        let mut head = RECT { left: pv.left, top: pv.top, right: pv.right, bottom: pv.top + px(20.0) };
        set_color(hdc, th.text_dim);
        paint::draw_text(
            hdc,
            &app
                .settings_interact_sel
                .map(interact_label)
                .unwrap_or_else(|| "选一个设置项".to_string()),
            head,
            DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS,
        );

        // 三条样例：未选中 / 悬停 / 选中 —— 选中那条才画竖线，
        // 这样用户能对照出"竖线只出现在选中态"这件事。
        // 三条**等分**剩下的高度，但每条有上限（56px）；预览区比三条所需
        // 高得多时**整体垂直居中** —— 让三条挤在顶部、底下留一大片空白，
        // 看着像没画完（第一版就是这样：参数行被甩到窗口最底）。
        let head_h = px(28.0);
        let avail = (pv.bottom - pv.top - head_h - px(10.0)).max(px(60.0));
        let gap = px(10.0);
        let want = ((avail - gap * 2) / 3).min(px(56.0)).max(px(22.0));
        let block_h = want * 3 + gap * 2;
        let top = pv.top + head_h + ((avail - block_h) / 2).max(0);
        let row_h = want;
        // 第38轮：样例改成**真实的详细信息行** —— 图标 + 文件名 + 修改日期
        // + 大小，四列，与真窗口列表行同一套画法。真窗口里就是这样的行，
        // 示意图对不上就等于没说清"这个参数改的是什么东西"。
        let isz = ((16.0 * s).round() as i32).max(8);
        let samples: [(&str, &str, &str, COLORREF, bool); 3] = [
            ("未选中", "文件夹", "", th.window_bg, false),
            ("鼠标悬停", "主文件夹", "2026/09/03 14:22", th.row_hover, false),
            ("已选中", "本地磁盘 (C:)", "2026/09/03 14:22", th.selection_bg, true),
        ];
        // 列宽按**行本体宽**算（预览宽 - 左侧标签那段）
        let vw = pv.right - pv.left - px(66.0) - px(8.0);
        let c_date = vw * 30 / 100;
        let c_size = vw * 22 / 100;
        let c_name = vw - c_date - c_size;
        // 状态标签占**左侧一段**，行本体从标签右边开始 ——
        // 标签塞进行里会把「大小」列挤没，画在行上方又会压住上一行
        let lbl_w = px(66.0);
        for (k, (label, fname, date, fill, sel)) in samples.iter().enumerate() {
            let sr = RECT {
                left: pv.left + lbl_w + px(8.0),
                top: top + k as i32 * (row_h + px(10.0)),
                right: pv.right,
                bottom: top + k as i32 * (row_h + px(10.0)) + row_h,
            };
            set_color(hdc, th.text_dim);
            let mut lr2 = RECT {
                left: pv.left + px(2.0), top: sr.top,
                right: pv.left + lbl_w, bottom: sr.bottom,
            };
            paint::draw_text(hdc, label, lr2,
                DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS);
            // 行底（圆角走 content.selected，与真窗口列表行同一个 id）
            rr(hdc, &sr, app.radius_of("content.selected"), *fill, None);
            // 行首强调竖线 —— **真窗口的 draw_sel_bar**，不是自己画一根线
            if *sel {
                unsafe { paint::draw_sel_bar(app, hdc, sr, th) };
            }
            // 图标：accent 小方块代表真实文件图标，位置与真窗口一致
            // （竖线右边、名称列起点）
            let ib = RECT {
                left: sr.left + px(14.0) + isz + px(5.0),
                top: sr.top + (row_h - isz) / 2,
                right: sr.left + px(14.0) + isz + px(5.0) + isz,
                bottom: sr.top + (row_h - isz) / 2 + isz,
            };
            unsafe {
                let br = CreateSolidBrush(th.accent);
                let _ = FillRect(hdc, &ib, br);
                let _ = DeleteObject(br.into());
            }
            let txt_col = if *sel { th.selection_text } else { th.text };
            let dim_col = if *sel { th.selection_text } else { th.text_dim };
            // 名称
            let mut tr = sr;
            tr.left = ib.right + px(5.0);
            tr.right = sr.left + px(14.0) + isz + px(5.0) + c_name - px(8.0);
            set_color(hdc, txt_col);
            paint::draw_text(hdc, fname, tr,
                DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS);
            // 修改日期
            let mut dr = sr;
            dr.left = sr.left + px(14.0) + isz + px(5.0) + c_name;
            dr.right = dr.left + c_date - px(8.0);
            set_color(hdc, dim_col);
            paint::draw_text(hdc, date, dr,
                DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS);
            // 大小
            let mut zr = sr;
            zr.left = dr.right + px(8.0);
            zr.right = sr.right - px(8.0);
            set_color(hdc, dim_col);
            paint::draw_text(hdc, if *sel { "221 GB / 369 GB" } else { "" }, zr,
                DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS);
        }
    }

    // —— 右侧下方：当前选中项的参数行（[−] 值 [+]）——
    for r in &g.rows {
        fill_rect(hdc, &r.rect, th.surface);
        // 标签
        set_color(hdc, th.text);
        let mut lr = r.rect;
        lr.left += px(10.0);
        lr.right = r.rect.right - btn_w * 2 - px(60.0);
        paint::draw_text(hdc, &r.label, lr, DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS);
        // 当前值（贴着按钮左边、右对齐）
        let mut vr = r.rect;
        vr.left = lr.right + px(4.0);
        vr.right = r.rect.right - btn_w * 2 - px(6.0);
        set_color(hdc, th.text_dim);
        paint::draw_text(hdc, &r.value, vr, DT_RIGHT | DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER);
        // 右端 [−][+] 两颗小按钮（点按钮才动值）
        for (k, glyph) in ["\u{2212}", "+"].iter().enumerate() {
            let bx = r.rect.right - btn_w * (2 - k as i32);
            let br = RECT {
                left: bx + px(2.0),
                top: r.rect.top + px(5.0),
                right: bx + btn_w - px(2.0),
                bottom: r.rect.bottom - px(5.0),
            };
            fill_rect(hdc, &br, th.surface_alt);
            set_color(hdc, th.text);
            paint::draw_text(hdc, glyph, br, DT_CENTER | DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER);
        }
    }
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
/// 「外观」页右侧预览。
///
/// 第38轮重写。原来这里画的是**示意图** —— 标签是两个空矩形、面包屑是
/// 三个空块、侧栏条目没文字没图标。真窗口里那些块都有内容，示意图对不上，
/// 于是"把圆角调到 16"看不出来圆的是哪儿。
///
/// 现在：**每个圆角 id 都对应一块真实样式的元素**（真配色、真图标、
/// 真文字），选中树里哪一项就高亮那几块 —— 和真窗口里那几块的观感一致。
/// 布局按真窗口的比例：标题栏 : 工具栏 : 侧栏宽 : 行高 都取自 `Metrics`。
fn draw_preview(app: &mut App, hdc: HDC, r: &RECT, sel: &str, th: Theme, s: f32) {
    if r.right <= r.left || r.bottom <= r.top {
        return;
    }
    // 第38轮：预览 = **真窗口的等比缩放**，与「界面」页共用同一个绘制函数。
    //
    // 旧版是一张手搓的组件拼贴（标题带、面包屑块、侧栏条、菜单样例各画一份），
    // 尺寸/颜色/间距全是写死的另一套数字，于是"预览"和"实际"永远对不上
    // —— 用户连续三轮都提"预览有问题"。现在两边是同一份代码，
    // 真窗口改了这里跟着改，物理上不可能再分叉。
    paint_preview_canvas(app, hdc, r, th, s);
    highlight_radius_block(app, hdc, r, sel, th, s);
}

/// 把「当前选中的圆角 id」对应到真窗口里的那一块，套一个 accent 描边。
///
/// 这一步是外观页预览存在的**全部意义**：选「标题栏圆角」时，
/// 用户要看见的是*真窗口的标题栏*被框出来，而不是一张画得像的示意图。
fn highlight_radius_block(app: &App, hdc: HDC, r: &RECT, sel: &str, th: Theme, s: f32) {
    let w = r.right - r.left;
    let h = r.bottom - r.top;
    if w <= 0 || h <= 0 {
        return;
    }
    // 与 paint_preview_canvas 完全相同的缩放口径 —— 必须逐字一致，
    // 否则高亮框会和画出来的窗口错位。
    const REF_W: f32 = 1280.0;
    const REF_H: f32 = 850.0;
    let k = (w as f32 / REF_W).min(h as f32 / REF_H).max(0.04);
    let vm = crate::theme::Metrics::new((96.0 * k) as u32);
    let px = |v: f32| (v * vm.scale).round() as i32;
    let lay = crate::paint::compute_layout(w, h, &vm, true, true);

    // 表头高（paint_preview_canvas 里写死 26 DIP，这里同源）
    let header_h = px(26.0);
    let header = RECT {
        left: lay.view.left,
        top: lay.view.top,
        right: lay.view.right,
        bottom: lay.view.top + header_h,
    };
    // 首个数据行 = "content.selected / content.hover" 要框的那一条
    let row0 = RECT {
        left: lay.view.left,
        top: header.bottom,
        right: lay.view.right,
        bottom: header.bottom + vm.row_h,
    };

    let target: RECT = match sel {
        // —— 标题栏 ——
        "title" => lay.title,
        "title.tab" => RECT {
            left: lay.tabs.left,
            top: lay.title.top,
            right: lay.tabs.left
                + ((lay.tabs.right - lay.tabs.left) as f32 * 0.22).round() as i32,
            bottom: lay.title.bottom,
        },
        "title.button" => lay.menu_btn,

        // —— 工具栏 ——
        "toolbar" => lay.toolbar,
        "toolbar.button" => lay.back, // 导航键（返回）是工具栏里最典型的圆角块

        // —— 地址栏 ——
        "address" | "address.crumb" | "address.edit" => lay.address,

        // —— 侧栏 ——
        "sidebar" | "sidebar.hover" | "sidebar.selected" => lay.sidebar,

        // —— 内容区 ——
        "content" => lay.view,
        "content.header" => header,
        "content.selected" | "content.hover" => row0,

        // —— 状态栏 ——
        "status" | "status.button" => lay.status,

        // —— 菜单（预览里没有常开的菜单，临时画一个再框它）——
        // **只算位置，别在这里画**：下面统一 += r.left/r.top 才是窗口坐标，
        // 在 match 里当场画会把弹窗甩到窗口左上角去（第38轮踩过）。
        "menu" | "menu.hover" | "menu.panel" => menu_popup_rect(&lay, &vm),

        // global 或未知 id：框整幅窗口。
        // **必须用 0 基坐标**（下面统一 += r.left/r.top 平移到画布原点）——
        // 第一版这里写的是已经是绝对坐标的 r，结果又平移一次，
        // 框被推出屏幕外，"global 选中时预览没框" 就是这么来的。
        _ => RECT { left: 0, top: 0, right: w, bottom: h },
    };

    // 坐标平移到画布原点（paint_preview_canvas 里也是这么平移的）
    let mut t = target;
    t.left += r.left;
    t.right += r.left;
    t.top += r.top;
    t.bottom += r.top;
    if t.right <= t.left || t.bottom <= t.top {
        return;
    }

    // 菜单：位置平移完再画（顺序不能反，见上面的注释）
    if matches!(sel, "menu" | "menu.hover" | "menu.panel") {
        draw_preview_menu(hdc, &t, &vm, th, sel, prad(app, "menu.panel", vm.scale));
    }

    // accent 描边 2px：压在所有内容之上，一眼看出"我调的是这一块"
    unsafe {
        let pen = CreatePen(PS_SOLID, ((2.0 * s).round() as i32).max(2), th.accent);
        let oldp = SelectObject(hdc, pen.into());
        let oldb = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        let _ = Rectangle(hdc, t.left, t.top, t.right, t.bottom);
        let _ = SelectObject(hdc, oldb);
        let _ = SelectObject(hdc, oldp);
        let _ = DeleteObject(pen.into());
    }
}

/// 预览里临时弹出的菜单面板位置（挂在菜单键正下方）。
fn menu_popup_rect(lay: &crate::paint::Layout, vm: &crate::theme::Metrics) -> RECT {
    let scale = vm.scale;
    let w = ((150.0 * scale).round() as i32).max(70);
    let h = ((78.0 * scale).round() as i32).max(40);
    let left = (lay.menu_btn.left - w).max(0);
    RECT {
        left,
        top: lay.title.bottom + ((4.0 * scale).round() as i32),
        right: left + w,
        bottom: lay.title.bottom + ((4.0 * scale).round() as i32) + h,
    }
}

/// 画一个真实样式的弹出菜单（三行 + 圆角 + 悬停行高亮），
/// 供 "menu / menu.hover / menu.panel" 高亮时用。
fn draw_preview_menu(
    hdc: HDC,
    r: &RECT,
    vm: &crate::theme::Metrics,
    th: Theme,
    sel: &str,
    rad: u32,
) {
    let scale = vm.scale;
    let px = |v: f32| (v * scale).round() as i32;
    // 面板底 + 边框 + 圆角
    // 圆角吃 menu.panel（拖滑杆时这里跟着变），不要写死 6
    rr(hdc, r, rad, th.surface, Some(th.border));
    let row_h = ((24.0 * scale).round() as i32).max(14);
    for (i, label) in ["打开", "复制", "删除"].iter().enumerate() {
        let top = r.top + px(4.0) + i as i32 * row_h;
        let row = RECT {
            left: r.left + px(3.0),
            top,
            right: r.right - px(3.0),
            bottom: top + row_h,
        };
        if i == 1 && sel == "menu.hover" {
            // 悬停行：用同一档 hover 底色（真菜单就是这一档）
            rr(hdc, &row, 4, th.row_hover, None);
        }
        set_color(hdc, if i == 1 && sel == "menu.hover" { th.text } else { th.text_dim });
        let mut tr = row;
        tr.left += px(8.0);
        tr.right -= px(6.0);
        paint::draw_text(
            hdc,
            label,
            tr,
            DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS,
        );
    }
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
    let kind = crate::app::MenuKind::from_u8(app.settings_ctx_kind);
    let items: Vec<std::path::PathBuf> = app.menu_preview_items.clone().unwrap_or_default();
    crate::app::own_menu_rows(app, &items, kind)
}

/// 某一类菜单当前的排布（没记录就退回默认那份）。
pub fn menu_order_for(app: &App, kind: crate::app::MenuKind) -> Vec<u16> {
    let key = crate::app::MENU_KIND_KEYS[kind.index() as usize];
    match app.ui_settings.menu_sets.get(key) {
        Some(ms) if !ms.order.is_empty() => ms.order.clone(),
        _ => app.ui_settings.menu_order.clone(),
    }
}

pub fn menu_hidden_for(app: &App, kind: crate::app::MenuKind) -> Vec<u16> {
    let key = crate::app::MENU_KIND_KEYS[kind.index() as usize];
    match app.ui_settings.menu_sets.get(key) {
        Some(ms) => ms.hidden.clone(),
        None => app.ui_settings.menu_hidden.clone(),
    }
}

/// 写回某一类菜单的排布。
pub fn menu_set_write(app: &mut App, kind: crate::app::MenuKind, order: Vec<u16>, hidden: Vec<u16>) {
    let key = crate::app::MENU_KIND_KEYS[kind.index() as usize].to_string();
    app.ui_settings.menu_sets.insert(key, MenuSet { order, hidden });
}

/// 设置页正在编辑的那一类的排布。
fn cur_menu_set(app: &App) -> (Vec<u16>, Vec<u16>) {
    let kind = crate::app::MenuKind::from_u8(app.settings_ctx_kind);
    (menu_order_for(app, kind), menu_hidden_for(app, kind))
}

fn commit_menu_set(app: &mut App, order: Vec<u16>, hidden: Vec<u16>) {
    let kind = crate::app::MenuKind::from_u8(app.settings_ctx_kind);
    menu_set_write(app, kind, order, hidden);
}

/// 当前**可见**的菜单条目 id，顺序即渲染顺序。
///
/// 渲染出来的第 n 条（分隔线不算）就对应这里的第 n 个 —— 于是"在实时菜单里
/// 把第 3 条拖到第 1 条"可以直接换算成排布表里的位置交换。
pub fn visible_menu_ids(app: &App) -> Vec<u16> {
    let kind = crate::app::MenuKind::from_u8(app.settings_ctx_kind);
    let order = menu_order_for(app, kind);
    let hidden = menu_hidden_for(app, kind);
    order
        .iter()
        .copied()
        .filter(|i| !hidden.contains(i))
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
    // —— 左上标题栏：类型 chip（右键落在什么上）——
    fill_rect(hdc, &g.ctx_bar, th.surface);
    for (i, r) in g.kind_chips.iter().enumerate() {
        let on = app.settings_ctx_kind as usize == i;
        fill_rect(hdc, r, if on { th.accent } else { th.surface_alt });
        set_color(hdc, if on { th.selection_text } else { th.text });
        paint::draw_text(
            hdc,
            crate::app::MENU_KIND_NAMES[i],
            *r,
            DT_SINGLELINE | DT_NOPREFIX | DT_CENTER | DT_VCENTER | DT_END_ELLIPSIS,
        );
    }

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
    let kn = crate::app::MENU_KIND_NAMES
        .get(app.settings_ctx_kind as usize)
        .copied()
        .unwrap_or("文件");
    set_color(hdc, th.text);
    paint::draw_text(
        hdc,
        &format!("查找可用的打开软件 · {kn}"),
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

    // —— 右上底部：「查找可用的打开软件」按钮 ——
    let fr = g.ow_find;
    fill_rect(hdc, &fr, if app.settings_hover_find { th.hover_bg } else { th.surface_alt });
    let (p, op, ob) = unsafe {
        let p = windows::Win32::Graphics::Gdi::CreatePen(
            windows::Win32::Graphics::Gdi::PS_SOLID,
            1,
            th.border,
        );
        let op = SelectObject(hdc, p.into());
        let ob = SelectObject(hdc, windows::Win32::Graphics::Gdi::GetStockObject(NULL_BRUSH));
        let _ = windows::Win32::Graphics::Gdi::Rectangle(hdc, fr.left, fr.top, fr.right, fr.bottom);
        (p, op, ob)
    };
    unsafe {
        SelectObject(hdc, ob);
        SelectObject(hdc, op);
        let _ = DeleteObject(p.into());
    }
    set_color(hdc, th.text);
    paint::draw_text(
        hdc,
        if app.settings_ow_found {
            "查找可用的打开软件（已列出本机全部程序）"
        } else {
            "查找可用的打开软件…"
        },
        fr,
        DT_SINGLELINE | DT_NOPREFIX | DT_CENTER | DT_VCENTER | DT_END_ELLIPSIS,
    );

    // —— 右下：系统集成 ——
    for r in &g.rows {
        paint_row(app, hdc, r, th, s);
    }

    // —— 右 40%：格式样本实时预览 ——
    paint_prev_panel(app, hdc, g, th, s);
}

/// 右 40% 那块**格式样本预览**：大图标网格 + 分字母 + 滚动。
///
/// 两类目标都在这一块里右键：
///   · 某一格 → 该格式的右键菜单（`Command` 全都不会真的执行）；
///   · 空白处 → 目录右键（背景菜单）。
/// 于是"每一类右键菜单长什么样"不用再靠想象，直接在这儿试。
fn paint_prev_panel(app: &App, hdc: HDC, g: &Geom, th: Theme, s: f32) {
    let pv = g.prev_panel;
    if pv.right <= pv.left || pv.bottom <= pv.top {
        return;
    }
    let px = |v: f32| (v * s).round() as i32;
    fill_rect(hdc, &pv, th.surface);
    let hdr = RECT {
        left: pv.left + px(10.0),
        top: pv.top,
        right: pv.right - px(10.0),
        bottom: pv.top + px(26.0),
    };
    // 刚在预览菜单里点过一条 → 标题这行闪一句"不会真的执行"（约 2.6 秒）。
    // 预览区就是让人反复点着看的地方，弹模态框既挡事又烦，所以走这条轻提示。
    let note = unsafe { windows::Win32::System::SystemInformation::GetTickCount64() }
        .wrapping_sub(app.preview_note_at)
        < 2600
        && app.preview_note_at != 0;
    if note {
        set_color(hdc, th.accent);
        paint::draw_text(
            hdc,
            "预览菜单：只展示长相，不会真的执行",
            hdr,
            DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS,
        );
    } else {
        set_color(hdc, th.text_dim);
        paint::draw_text(
            hdc,
            "预览 · 右键任意格式看它的菜单，空白处 = 目录右键",
            hdr,
            DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS,
        );
    }

    let sc = app.settings_prev_scroll;
    let isz = px(48.0);
    let samples = crate::fmtprev::samples();
    unsafe {
        // 记忆 DC 是全窗口共享的：裁剪必须 SaveDC/RestoreDC 包住
        let _sv = SaveDC(hdc);
        let _ = IntersectClipRect(
            hdc,
            g.prev_view.left,
            g.prev_view.top,
            g.prev_view.right,
            g.prev_view.bottom,
        );
        // —— 字母徽标 ——
        for (r, c) in &g.prev_letters {
            let t = g.prev_view.top + r.top - sc;
            let r2 = RECT { left: r.left, top: t, right: r.right, bottom: t + (r.bottom - r.top) };
            if r2.bottom <= g.prev_view.top || r2.top >= g.prev_view.bottom {
                continue;
            }
            set_color(hdc, th.accent);
            paint::draw_text(
                hdc,
                &c.to_string(),
                r2,
                DT_SINGLELINE | DT_NOPREFIX | DT_CENTER | DT_VCENTER,
            );
        }
        // —— 每一格：大图标 + 格式名 ——
        for (i, r) in g.prev_cells.iter().enumerate() {
            let t = g.prev_view.top + r.top - sc;
            let r2 = RECT { left: r.left, top: t, right: r.right, bottom: t + (r.bottom - r.top) };
            if r2.bottom <= g.prev_view.top || r2.top >= g.prev_view.bottom {
                continue;
            }
            let Some(smp) = samples.get(i) else { continue };
            let hov = app.settings_prev_hover == Some(i);
            if hov {
                // 用 `hover_bg`（菜单悬停那一档），**不用 `row_hover`**：
                // 这一块的底是 surface(43)，row_hover 只有 46 —— 差 3 级，
                // 看上去等于没亮。hover_bg(61) 才看得出来"我指在这一格上"。
                fill_rect(hdc, &r2, th.hover_bg);
            }
            let ix = (r2.left + r2.right - isz) / 2;
            let iy = r2.top + px(5.0);
            let h = crate::fmtprev::icon(&smp.ext);
            if h != 0 {
                app.icons.draw(hdc, crate::fmtprev::hicon(h), ix, iy, isz);
            }
            let tr = RECT {
                left: r2.left + px(2.0),
                top: iy + isz + px(2.0),
                right: r2.right - px(2.0),
                bottom: r2.bottom - px(2.0),
            };
            set_color(hdc, if hov { th.text } else { th.text_dim });
            paint::draw_text(
                hdc,
                &smp.name,
                tr,
                DT_SINGLELINE | DT_NOPREFIX | DT_CENTER | DT_END_ELLIPSIS,
            );
        }
        let _ = RestoreDC(hdc, -1);
    }

    // 外框：只描边不填充（`Rectangle` 会用当前画刷填内部）
    let (pen, op, ob) = unsafe {
        let pen = CreatePen(PS_SOLID, 1, th.border);
        let op = SelectObject(hdc, pen.into());
        let ob = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        let _ = Rectangle(hdc, pv.left, pv.top, pv.right, pv.bottom);
        (pen, op, ob)
    };
    unsafe {
        SelectObject(hdc, ob);
        SelectObject(hdc, op);
        let _ = DeleteObject(pen.into());
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
    let (id, rect, from_dock, from_canvas) = match hit {
        Hit::Card(i) => match (g.card_ids.get(*i), g.cards.get(*i)) {
            (Some(id), Some(r)) => {
                // 变灰 = 已经在画布里，最多只允许添加一次 → 不可选中
                if widget_is_placed(app, (*id & 0xFF) as u16) {
                    return false;
                }
                (*id, *r, false, None)
            }
            _ => return false,
        },
        Hit::Canvas(i) => match canvas_item_rects(app, &g).get(*i) {
            Some((_, r)) => {
                let wid = match app.ui_settings.canvas_items.get(*i) {
                    Some(c) => c.wid,
                    None => return false,
                };
                (wid as u32, *r, false, Some(*i))
            }
            None => return false,
        },
        Hit::Chip(i) => match (g.chip_ids.get(*i), g.chips.get(*i)) {
            (Some(id), Some(r)) => (*id, *r, true, None),
            _ => return false,
        },
        // 「右键菜单」页里那块**真实渲染的菜单**：直接拖条目改顺序
        Hit::MenuRow(i) => {
            let id = visible_menu_ids(app).get(*i).copied().unwrap_or(0) as u32;
            match g.menu_rows.get(*i) {
                Some(r) => (id, *r, false, None),
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
        from_canvas,
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
    // 「界面」页（round 25 画布形态）：
    //   - 从画布起的拖：还在预览内 = 改位置；拖出预览 = 删除元素
    //   - 从调色板起的拖：落预览 = 放下到该位置；落整条 = 装进那条整条
    //   - 原地单击 = 快速添加到画布中心
    //   - 组件最多添加一次（已在画布 → 调色板变灰、不可选中）
    if d.tab == crate::app::SETTINGS_TAB_UI {
        let pv = g.ui_preview;
        let pw = (pv.right - pv.left).max(1);
        let ph = (pv.bottom - pv.top).max(1);

        // —— 画布元素：改位置 / 删除 ——
        if let Some(ci) = d.from_canvas {
            if x >= pv.left && x < pv.right && y >= pv.top && y < pv.bottom {
                // 保持按下时的抓取偏移，元素左上被钳在预览内
                let off_x = d.grab.0 - d.rect.left;
                let off_y = d.grab.1 - d.rect.top;
                let w = d.rect.right - d.rect.left;
                let h = d.rect.bottom - d.rect.top;
                let left = (x - off_x - pv.left).clamp(0, (pw - w).max(0));
                let top = (y - off_y - pv.top).clamp(0, (ph - h).max(0));
                if let Some(it) = app.ui_settings.canvas_items.get_mut(ci) {
                    it.nx = left as f32 / pw as f32;
                    it.ny = top as f32 / ph as f32;
                }
            } else if ci < app.ui_settings.canvas_items.len() {
                // 拖到预览窗口外 = 删除这个元素
                app.ui_settings.canvas_items.remove(ci);
            }
            after_change(app);
            return;
        }

        let wid = (d.id & 0xFF) as u16;
        // 唯一性兜底：已经在画布里就不允许再加（正常路径 begin_drag 已拦）
        if widget_is_placed(app, wid) {
            return;
        }
        // 原地单击（位移 ≤3px）= 快速添加到画布中心
        if (d.cur.0 - d.grab.0).abs() + (d.cur.1 - d.grab.1).abs() <= 3 {
            let w = canvas_widget_w(app, wid);
            let h = canvas_widget_h(app, wid);
            app.ui_settings.canvas_items.push(CanvasItem {
                wid,
                nx: (((pw - w) / 2) as f32 / pw as f32).clamp(0.0, 1.0),
                ny: (((ph - h) / 2) as f32 / ph as f32).clamp(0.0, 1.0),
            });
            after_change(app);
            return;
        }

        match drop_target(app, &g, x, y) {
            Some(DropTarget::Preview) => {
                // 落在预览里 = 以落点为组件中心放下
                let w = canvas_widget_w(app, wid);
                let h = canvas_widget_h(app, wid);
                let left = (x - pv.left - w / 2).clamp(0, (pw - w).max(0));
                let top = (y - pv.top - h / 2).clamp(0, (ph - h).max(0));
                app.ui_settings.canvas_items.push(CanvasItem {
                    wid,
                    nx: left as f32 / pw as f32,
                    ny: top as f32 / ph as f32,
                });
            }
            Some(DropTarget::Strip(i)) => {
                // 落在整条（标题栏/工具栏/状态栏/目录栏/自定义空行）= 装进那条
                while app.ui_settings.bars.len() <= i {
                    app.ui_settings.bars.push(Vec::new());
                }
                for b in app.ui_settings.bars.iter_mut() {
                    b.retain(|v| *v != wid);
                }
                app.ui_settings.bars[i].push(wid);
            }
            None => {
                // 落在别处 = 不添加（也不删任何已有元素）
            }
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
        let (mut o, mut h) = cur_menu_set(app);
        o.retain(|v| *v != i);
        if !h.contains(&i) {
            h.push(i);
        }
        commit_menu_set(app, o, h);
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
        let (mut o, mut h) = cur_menu_set(app);
        h.retain(|v| *v != i);
        if !o.contains(&i) {
            o.push(i);
        }
        commit_menu_set(app, o, h);
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
        let (mut o, h) = cur_menu_set(app);
        let old = match o.iter().position(|v| *v == i) {
            Some(p) => p,
            None => return,
        };
        o.remove(old);
        let t = target.min(o.len());
        o.insert(t, i);
        commit_menu_set(app, o, h);
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
