//! 应用状态与命令分发。UI 层的所有业务动作都收敛在这里，
//! 便于将来替换为 GPUI 编排层时只重写事件绑定，不动逻辑。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};

use zenfile_core::clipboard::{self, ClipMode};
use zenfile_core::config::Config;
use zenfile_core::engine::{self, EngineMsg, SearchRequest};
use zenfile_core::ops::{
    copy_items, delete_items, move_items, new_file, new_folder, rename_item, transfer_async,
    ComGuard, SharedStats, TransferKind,
};
use zenfile_core::paths;
use zenfile_core::process::ProcessTree;
use zenfile_core::types::{filter_entries, FileEntry, FileKind, SortKey, SortSpec, ViewMode};
use zenfile_core::watch::{DirWatcher, WatchMsg};
use windows::Win32::Foundation::{COLORREF, HWND, RECT};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::UI::WindowsAndMessaging::{DestroyWindow, IsWindow};

use crate::icons::IconCache;
use crate::menu::{self, Command, MenuAction, ShellMenuBridge};
use crate::theme::{self, Metrics, Theme};

/// 主页（快速访问）的哨兵路径。它不是真实目录，由 App 合成条目。
pub const HOME_PATH: &str = "shell:Home";
/// 回收站哨兵路径。同样是虚拟视图，条目来自 `$Recycle.Bin` 直读。
pub const RECYCLE_PATH: &str = "shell:RecycleBin";
/// 设置页哨兵路径。以标签页形式打开，不参与目录枚举。
pub const SETTINGS_PATH: &str = "shell:Settings";

/// 设置页顶部小标签的下标（第 19 轮第 1 项）。
pub const SETTINGS_TAB_UI: u8 = 0;
pub const SETTINGS_TAB_LOOK: u8 = 1;
pub const SETTINGS_TAB_INTERACT: u8 = 2;
pub const SETTINGS_TAB_MENU: u8 = 3;
pub const SETTINGS_TAB_HOTKEY: u8 = 4;
pub const SETTINGS_TAB_ADVANCED: u8 = 5;
pub const SETTINGS_TAB_PLUGIN: u8 = 6;
pub const SETTINGS_TAB_ABOUT: u8 = 7;
/// 设置页标签的显示名。顺序与上面的下标一一对应。
pub const SETTINGS_TABS: [&str; 8] =
    ["界面", "外观", "交互", "右键菜单", "快捷键", "高级", "插件", "关于"];

pub fn is_home_path(p: &Path) -> bool {
    p.to_string_lossy().eq_ignore_ascii_case(HOME_PATH)
}

pub fn is_recycle_path(p: &Path) -> bool {
    p.to_string_lossy().eq_ignore_ascii_case(RECYCLE_PATH)
}

pub fn is_settings_path(p: &Path) -> bool {
    p.to_string_lossy().eq_ignore_ascii_case(SETTINGS_PATH)
}

/// 非真实目录的虚拟视图：不枚举、不监听、不走目录操作。
pub fn is_virtual(p: &Path) -> bool {
    is_home_path(p) || is_recycle_path(p) || is_settings_path(p)
}

/// 合成主页条目：**先**常用文件夹（8 个），**后**本机驱动器。
///
/// 顺序即分区：首页用平铺布局渲染时，靠 `paths::is_drive_root` 把这两段分开，
/// 所以这里必须保证驱动器排在最后。
pub fn home_entries() -> Vec<FileEntry> {
    use zenfile_core::types::FileKind;
    let profile = std::env::var("USERPROFILE").unwrap_or_default();
    let mut out: Vec<FileEntry> = Vec::new();

    let mk = |name: &str, path: String| FileEntry {
        name: name.to_string(),
        path: PathBuf::from(path),
        kind: FileKind::Directory,
        size: 0,
        modified: 0,
        created: 0,
        accessed: 0,
        attributes: 16, // FILE_ATTRIBUTE_DIRECTORY
        hidden: false,
    };

    if !profile.is_empty() {
        out.push(mk("主文件夹", profile.clone()));
        // "库"是虚拟命名空间，这里退化为它背后的真实目录（双击 .library-ms 会打开对应库）
        let libs = std::env::var("APPDATA")
            .map(|a| format!("{a}\\Microsoft\\Windows\\Libraries"))
            .unwrap_or_default();
        if !libs.is_empty() && std::path::Path::new(&libs).is_dir() {
            out.push(mk("库", libs));
        }
        for (label, sub) in [
            ("桌面", "Desktop"),
            ("下载", "Downloads"),
            ("文档", "Documents"),
            ("图片", "Pictures"),
            ("音乐", "Music"),
            ("视频", "Videos"),
        ] {
            out.push(mk(label, format!("{profile}\\{sub}")));
        }
    }

    for d in zenfile_core::drives::list() {
        // 名字跟着资源管理器走：有卷标用卷标，没有就用类型名。
        // 之前这里写死成"本地磁盘"，于是 U 盘、光驱、网络盘全叫本地磁盘。
        out.push(mk(&d.display_name(), d.root.clone()));
    }
    out
}

/// 表头下拉里的"按这一列筛选"（资源管理器同款）。
///
/// 载荷一律用"桶序号"，菜单里第 0 项是"全部"（= 不筛选），所以
/// `桶 0` 等价于 `None`。
#[derive(Clone, Debug, PartialEq)]
pub enum ColFilter {
    /// 名称按首字母分桶：0 全部 / 1 `#` / 2 A–H / 3 I–P / 4 Q–T / 5 U–Z / 6 其它
    Name(usize),
    /// 修改日期：0 全部 / 1 今天 / 2 昨天 / 3 本周 / 4 本月 / 5 今年 / 6 更早
    Date(usize),
    /// 类型：具体某一个类型名
    Kind(String),
    /// 大小：0 全部 / 1 空 / 2 极小 / 3 小 / 4 中 / 5 大 / 6 巨大
    Size(usize),
}

pub const NAME_BUCKETS: [&str; 7] = ["全部", "#", "A–H", "I–P", "Q–T", "U–Z", "其它"];
pub const DATE_BUCKETS: [&str; 7] = ["全部", "今天", "昨天", "本周", "本月", "今年", "更早"];
pub const SIZE_BUCKETS: [&str; 7] = [
    "全部",
    "空（0 B）",
    "极小（< 16 KB）",
    "小（16 KB – 1 MB）",
    "中（1 – 128 MB）",
    "大（128 MB – 1 GB）",
    "巨大（> 1 GB）",
];

/// 今天的"日序号"（1970-01-01 = 0）。日期分桶全靠它，没它就得引 chrono。
fn today_days() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 / 86400)
        .unwrap_or(0)
}

impl ColFilter {
    /// 这一项是不是通过了筛选。
    pub fn matches(&self, e: &zenfile_core::FileEntry) -> bool {
        match self {
            ColFilter::Name(b) => {
                let c = e.name.chars().next().unwrap_or(' ').to_ascii_uppercase();
                let got = if !c.is_ascii_alphanumeric() {
                    1
                } else if c.is_ascii_digit() {
                    1
                } else if ('A'..='H').contains(&c) {
                    2
                } else if ('I'..='P').contains(&c) {
                    3
                } else if ('Q'..='T').contains(&c) {
                    4
                } else if ('U'..='Z').contains(&c) {
                    5
                } else {
                    6
                };
                *b == 0 || got == *b
            }
            ColFilter::Date(b) => {
                if *b == 0 {
                    return true;
                }
                if e.modified <= 0 {
                    return *b == 6;
                }
                let d = (e.modified / 10_000_000 - 11_644_473_600) / 86400;
                let today = today_days();
                // 星期一 = 0，和资源管理器"本周"的口径一致
                let wd = (today + 3) % 7;
                let week_start = today - wd;
                let (y, m, _) = crate::paint::civil_from_days(d);
                let (ty, tm, _) = crate::paint::civil_from_days(today);
                match b {
                    1 => d == today,
                    2 => d == today - 1,
                    3 => d >= week_start,
                    4 => y == ty && m == tm,
                    5 => y == ty,
                    _ => d < week_start - 7 && y < ty,
                }
            }
            ColFilter::Kind(k) => e.type_label() == *k,
            ColFilter::Size(b) => {
                let s = e.size;
                let got = match s {
                    0 => 1,
                    ..=15_999 => 2,
                    ..=1_048_575 => 3,
                    ..=134_217_727 => 4,
                    ..=1_073_741_823 => 5,
                    _ => 6,
                };
                *b == 0 || got == *b
            }
        }
    }

    /// 菜单上这一组里哪一项是勾上的（`pick_row` 用）。
    pub fn bucket(&self) -> usize {
        match self {
            ColFilter::Name(b) | ColFilter::Date(b) | ColFilter::Size(b) => *b,
            ColFilter::Kind(_) => usize::MAX,
        }
    }
}

/// `Clone` 是为了"四等分"：多出来的格子沿用原来正在看的目录，
/// 直接克隆当前标签（连已加载的条目一起）比重建一个再重新枚举快得多。
#[derive(Clone)]
pub struct Tab {
    pub path: PathBuf,
    pub title: String,
    /// 主页（快速访问）虚拟目录，不参与真实枚举
    pub home: bool,
    /// 回收站虚拟视图
    pub recycle: bool,
    /// 设置页（以标签页形式打开）
    pub settings: bool,
    pub entries: Vec<FileEntry>,
    /// 通过即时筛选的行索引
    pub visible: Vec<usize>,
    /// 选中集合按绝对路径小写存储，刷新后仍保持选中
    pub selected: HashSet<String>,
    pub focus_row: usize,
    pub scroll: i32,
    pub generation: u64,
    pub loading: bool,
    pub sort: SortSpec,
    pub view_mode: ViewMode,
    pub history: Vec<PathBuf>,
    pub history_pos: usize,
    pub filter: String,
    /// 表头下拉里选的"按列筛选"。`None` = 不筛选。
    pub col_filter: Option<ColFilter>,
    pub search_root: Option<PathBuf>,
}

impl Tab {
    pub fn new(path: PathBuf) -> Self {
        let title = if is_home_path(&path) {
            "此电脑".to_string()
        } else if is_recycle_path(&path) {
            "回收站".to_string()
        } else if is_settings_path(&path) {
            "设置".to_string()
        } else {
            display_title(&path)
        };
        Self {
            path: path.clone(),
            title,
            home: is_home_path(&path),
            recycle: is_recycle_path(&path),
            settings: is_settings_path(&path),
            entries: Vec::new(),
            visible: Vec::new(),
            selected: HashSet::new(),
            focus_row: 0,
            scroll: 0,
            generation: 0,
            loading: true,
            sort: SortSpec::default(),
            view_mode: ViewMode::Details,
            history: vec![path],
            history_pos: 0,
            filter: String::new(),
            col_filter: None,
            search_root: None,
        }
    }

    pub fn selected_paths(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|e| self.selected.contains(&key_of(e)))
            .map(|e| e.path.clone())
            .collect();
        if out.is_empty() {
            if let Some(idx) = self.visible.get(self.focus_row) {
                if let Some(e) = self.entries.get(*idx) {
                    out.push(e.path.clone());
                }
            }
        }
        out
    }

    pub fn recompute_filter(&mut self, show_hidden: bool, show_system: bool) {
        self.visible = filter_entries(&self.entries, &self.filter, show_hidden, show_system);
        // 表头下拉里选的"按列筛选"再叠一层：它是"在这一列上只留某一档"，
        // 和目录栏里的名字过滤是两件事，各自独立生效。
        if let Some(f) = &self.col_filter {
            let f = f.clone();
            self.visible.retain(|&i| self.entries.get(i).map(|e| f.matches(e)).unwrap_or(false));
        }
        if self.focus_row >= self.visible.len() {
            self.focus_row = self.visible.len().saturating_sub(1);
        }
    }

    /// 只按"隐藏/受保护系统项"两道开关重算可见项，**不按名字过滤**。
    ///
    /// 搜索结果已经是查过的了，再拿查询串当子串过一遍是错的：通配符查询
    /// （`a*.txt`）命中项的名字里并不含 `a*.txt`，会被整份滤掉，界面上看起来
    /// 就是"通配符搜不到任何东西"。
    pub fn recompute_visibility(&mut self, show_hidden: bool, show_system: bool) {
        self.visible = filter_entries(&self.entries, "", show_hidden, show_system);
        if self.focus_row >= self.visible.len() {
            self.focus_row = self.visible.len().saturating_sub(1);
        }
    }

    pub fn selected_bytes(&self) -> u64 {
        self.entries
            .iter()
            .filter(|e| self.selected.contains(&key_of(e)))
            .map(|e| e.size)
            .sum()
    }

}

pub fn key_of(e: &FileEntry) -> String {
    e.path.to_string_lossy().to_lowercase()
}

pub fn display_title(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.len() <= 3 && s.ends_with(':') {
        return format!("{s}\\");
    }
    match path.file_name() {
        Some(n) if !path.to_string_lossy().ends_with(":\\") => n.to_string_lossy().to_string(),
        _ => s.to_string(),
    }
}

pub enum Nav {
    New,
    Same,
}

/// 归一化矩形（0..1，相对内容区）。用归一化是为了窗口缩放时各窗格比例不变。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct RectF {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl RectF {
    pub const FULL: RectF = RectF { x: 0.0, y: 0.0, w: 1.0, h: 1.0 };
    /// 落成像素矩形。`view` 是整个内容区。
    pub fn to_px(&self, view: &RECT) -> RECT {
        let vw = (view.right - view.left) as f32;
        let vh = (view.bottom - view.top) as f32;
        RECT {
            left: view.left + (self.x * vw).round() as i32,
            top: view.top + (self.y * vh).round() as i32,
            right: view.left + ((self.x + self.w) * vw).round() as i32,
            bottom: view.top + ((self.y + self.h) * vh).round() as i32,
        }
    }
}

/// 一个窗格：自己的一排标签 + 自己当前的目录视图。
pub struct Pane {
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub rect: RectF,
}

impl Default for Pane {
    fn default() -> Self {
        Pane { tabs: Vec::new(), active: 0, rect: RectF::FULL }
    }
}

/// 拖着标签在内容区里松手时贴到哪。8 个方向 + 中央（合并）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DockZone {
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    /// 合进目标窗格，不做分栏
    Center,
}

/// 把 `r` 按 `zone` 一分为二：返回 (旧窗格留下的, 新窗格占的)。
///
/// 只服务**四边**。四角走 `dock_quarter` 做真正的 2x2 四等分 ——
/// 早先让角落也走"一分为二"，于是旧窗格 keep 住对面那 1/2、新窗格只拿 1/4，
/// 剩下那 1/4 谁都不属于，点也点不中（用户报的"一边的用不了点不动"）。
/// 这里按 Windows 贴靠的实际语义处理 —— 角落贴等价于"先按左右分，
/// 新窗格再在它那一半里占上下的一半"，于是新窗格拿到的是一个
/// **1/4 大小的规则矩形**，旧窗格 keep 住对面那一半（1/2）。
/// 把 `r` 从 `from` 块的归一化空间线性映射到 `to` 块（贴靠后现窗格压缩用）。
///
/// 保相对位置与占比：`from` 里的 (相对x, 宽占比) 原样落到 `to`。这样
/// "对侧半"里的窗格布局不变、只是等比缩小 —— 既不重叠也不留缺口。
fn remap_rect(r: RectF, from: RectF, to: RectF) -> RectF {
    let fx = if from.w > 0.0 { (r.x - from.x) / from.w } else { 0.0 };
    let fy = if from.h > 0.0 { (r.y - from.y) / from.h } else { 0.0 };
    let fw = if from.w > 0.0 { r.w / from.w } else { 1.0 };
    let fh = if from.h > 0.0 { r.h / from.h } else { 1.0 };
    RectF {
        x: to.x + fx * to.w,
        y: to.y + fy * to.h,
        w: fw * to.w,
        h: fh * to.h,
    }
}

fn split_rect(r: RectF, zone: DockZone) -> (RectF, RectF) {
    let (ow, oh) = (r.w, r.h);
    match zone {
        DockZone::Left => (
            RectF { x: r.x + ow * 0.5, y: r.y, w: ow * 0.5, h: oh },
            RectF { x: r.x, y: r.y, w: ow * 0.5, h: oh },
        ),
        DockZone::Right => (
            RectF { x: r.x, y: r.y, w: ow * 0.5, h: oh },
            RectF { x: r.x + ow * 0.5, y: r.y, w: ow * 0.5, h: oh },
        ),
        DockZone::Top => (
            RectF { x: r.x, y: r.y + oh * 0.5, w: ow, h: oh * 0.5 },
            RectF { x: r.x, y: r.y, w: ow, h: oh * 0.5 },
        ),
        DockZone::Bottom => (
            RectF { x: r.x, y: r.y, w: ow, h: oh * 0.5 },
            RectF { x: r.x, y: r.y + oh * 0.5, w: ow, h: oh * 0.5 },
        ),
        // 角落：新窗格占这一角的 1/4，旧窗格只让出"新窗格所在那一侧的上/下半"，
        // 于是两块都是矩形，且新窗格确实是 1/4 大小。
        DockZone::TopLeft => (
            RectF { x: r.x + ow * 0.5, y: r.y, w: ow * 0.5, h: oh },
            RectF { x: r.x, y: r.y, w: ow * 0.5, h: oh * 0.5 },
        ),
        DockZone::TopRight => (
            RectF { x: r.x, y: r.y, w: ow * 0.5, h: oh },
            RectF { x: r.x + ow * 0.5, y: r.y, w: ow * 0.5, h: oh * 0.5 },
        ),
        DockZone::BottomLeft => (
            RectF { x: r.x + ow * 0.5, y: r.y, w: ow * 0.5, h: oh },
            RectF { x: r.x, y: r.y + oh * 0.5, w: ow * 0.5, h: oh * 0.5 },
        ),
        DockZone::BottomRight => (
            RectF { x: r.x, y: r.y, w: ow * 0.5, h: oh },
            RectF { x: r.x + ow * 0.5, y: r.y + oh * 0.5, w: ow * 0.5, h: oh * 0.5 },
        ),
        DockZone::Center => (r, r),
    }
}

/// 子 EDIT"当前画在它自己身上的内容"的快照（第75轮）。
///
/// 这两个子窗口是**真窗口控件**，父窗口带 `WS_CLIPCHILDREN`，父窗重绘永远
/// 不碰它们那两块。想让它们跟着变，只能显式失效。问题在于：以前每帧都失效
/// （于是每帧 3 次重绘），而它们真正需要重画的原因很少。
///
/// 判据取"决定它们像素的那几项状态的合集"：
///
/// * `hint` —— 空态提示词（放大镜右边那句"搜索 X"）。**跟着当前标签变**：
///   "搜索 此电脑" / "搜索 回收站" / "搜索设置" / "搜索 桌面"…。标签切换
///   只重绘父窗口，不显式让子 EDIT 重画，提示词就停在旧标签的文字上。
///   这是第74轮之前那句无条件失效的**唯一真实理由**。
/// * `text_dim` —— 提示词与放大镜的墨色（`WM_CTLCOLOREDIT` 的文字色、
///   `paint_search_decor` 的 `SetTextColor` 都走它）。切明暗主题会变。
/// * `scale_bits` —— 字号、放大镜半径、左右边距都随它变（`ui_font` /
///   `search_left_margin` / `icon_spec::size_of`）。改缩放会变。
/// * `icon_theme` —— 主题接管时放大镜走主题字形（`draw_ui_icon`）。换图标
///   主题、或明暗翻转（优化套按明暗重烘）都会变。
/// * `visible` —— 只有地址框用得上：它平时 `SW_HIDE`，`InvalidateRect`
///   对隐藏窗口不产生任何重绘（实测全程 0 次 paint），所以"不可见"这一态
///   要显式记进快照，否则从隐藏切到显示那一帧会被判成"没变"而漏画。
///
/// **为什么不用"只比提示词"**：只比 `hint` 会漏掉切主题 / 改缩放 / 换图标
/// 主题这三种 —— 它们各自都要求子 EDIT 重画，而提示词一个字都没变。
/// 多比这三项的代价是每帧一次 `String` 分配 + 一次 `PartialEq`
/// （几十字节、亚微秒级），换来的是"判据与真实依赖一一对应、既不漏也不多"。
#[derive(Clone, PartialEq, Eq, Default, Debug)]
pub struct ChildEditKey {
    pub hint: String,
    pub text_dim: COLORREF,
    pub scale_bits: u32,
    pub icon_theme: String,
    pub visible: bool,
}

/// 地址框当前**应该显示**的文本（第75轮：从 `sync_address` 里原样提出来）。
///
/// **纯函数**，只吃一个标签 —— 这样"写进 EDIT 的文本"与"失效判据用的文本"
/// 必然同源（两者都调它），也让本轮新增的不变式可以直接被单测钉住。
///
/// 主页那一页（常用文件夹 + 本机驱动器）在资源管理器里地址栏写的就是"主页"，
/// 侧栏虽然把首页并到了"此电脑"这一格，但**地址栏要标的是"这是哪一页"**，
/// 不是"从哪点进来的"，所以这里写"主页"。
pub fn address_display_text_for(tab: &Tab) -> String {
    if tab.home {
        return "主页".to_string();
    }
    if tab.recycle {
        return "回收站".to_string();
    }
    if tab.settings {
        return "设置".to_string();
    }
    // 路径进过 canonicalize 之后会带上 `\\?\` 前缀（Win32 长路径约定）。
    // 它是给 API 看的，不是给人看的 —— 直接显示在地址栏会很怪。
    zenfile_core::paths::strip_verbatim(&tab.path)
        .to_string_lossy()
        .to_string()
}

pub struct App {
    pub hwnd: HWND,
    pub hwnd_address: HWND,
    pub hwnd_search: HWND,
    /// 窗格。绝大多数时候只有一个（占满内容区）；把标签拖到内容区做"8 向贴"
    /// 之后会有两个以上，每个窗格有自己的标签条和当前目录。
    ///
    /// **不要直接索引它**：凡是"当前标签"一律走 `tab()` / `tab_mut()`，
    /// 它们按 `focus_pane` 取。绘制与命中测试在每个窗格开始前会把
    /// `focus_pane` 指到那一个窗格、并把 `Layout::view` 覆写成它的矩形，
    /// 于是下面近两百处 `app.tab()` 一行都不用改。
    pub panes: Vec<Pane>,
    /// 当前焦点窗格在 `panes` 里的下标。
    pub focus_pane: usize,
    pub cfg: Config,
    pub metrics: Metrics,
    pub theme: Theme,
    pub icons: IconCache,
    pub engine_rx: Receiver<EngineMsg>,
    pub engine_tx: Sender<EngineMsg>,
    pub watch_rx: Option<Receiver<WatchMsg>>,
    pub watcher: Option<DirWatcher>,
    pub menu_hwnd: Option<HWND>,
    pub bridge: Option<ShellMenuBridge>,
    pub rename_hwnd: Option<HWND>,
    pub rename_index: Option<usize>,
    pub tree: Option<ProcessTree>,
    pub dirty: bool,
    pub last_size: (i32, i32),
    /// 第74轮：**按窗口尺寸缓存复用**的全窗口离屏位图。
    ///
    /// 挂在 `App` 上而不是 `static mut`：多窗口时每个 `App` 一份自己的缓存，
    /// 互不干扰（这也是本轮没走全局变量的原因）。生命周期由
    /// `paint::invalidate_back_surface` 统一管：
    ///
    /// * `WM_SIZE` / `WM_DPICHANGED` / `WM_DISPLAYCHANGE` / `WM_DESTROY`
    ///   里**显式销毁**（不等下一帧 lazy 重建，否则最大化之后旧的大图
    ///   一直被攥着）；
    /// * 尺寸变化时由 `paint::acquire_back_surface` 先销毁旧的再建新的；
    /// * 客户区 0x0（最小化）时直接丢弃。
    ///
    /// 生命周期内的正常路径上它**不会被 `Drop` 兜底**（GDI 句柄要在
    /// `WM_DESTROY` 里、`hwnd` 还没失效时删），所以忘记在 `WM_DESTROY`
    /// 里调 `invalidate_back_surface` 就等于漏一个 DC + 一张整窗位图。
    pub back_surface: crate::paint::BackSurface,
    pub drag_origin: Option<(i32, i32)>,
    pub fast_mode: bool,
    /// 正在跑的自研传输（复制/移动）。跑完由 `drain_transfer` 收口并重列目录。
    ///
    /// 取代了原来的 `SharedStats`（那是 `ops::transfer_async` 的，只给一个
    /// 百分比，没有"已处理文件/剩余/已用时间/预计剩余"，也没有取消）。
    pub xfer: Option<zenfile_core::transfer::Transfer>,
    /// 后台文件操作的回执通道。worker 干完把错误串（成功则是空串）丢进来，
    /// UI 线程在 `WM_APP_IO_DONE` 里统一收口。
    pub io_tx: Sender<String>,
    pub io_rx: Receiver<String>,
    /// 正在后台跑的文件操作数。>0 时目录栏下沿的进度条要动起来。
    pub io_pending: usize,
    pub exit_to_tray: bool,
    pub hovering_close: bool,
    /// 标题栏自有按钮的悬停位：bit0=侧栏开关，bit1=更多菜单
    pub hover_chrome: u8,
    /// 鼠标是不是悬在目录栏最右侧的「批量重命名」按钮上
    pub hover_bulk: bool,
    /// 插件运行时（第65轮）。管着一堆插件进程的生命周期。
    ///
    /// **面板上那个"插件"页读的就是它**，不再读 `cfg.modules` 里那份
    /// 从来没人填过的空配置 —— 之前那页永远显示"没有已注册的插件模块"。
    pub plugins: zenfile_core::plugin::PluginHost,
    pub show_hidden: bool,
    /// 是否显示"受保护的操作系统文件"（带 FILE_ATTRIBUTE_SYSTEM 的项）。
    /// 与 show_hidden 相互独立 —— 资源管理器也是两道分开的开关。
    pub show_system: bool,
    /// 目录的本地化显示名缓存（"Users"→"用户"、"Desktop"→"桌面"）。
    pub display_names: HashMap<PathBuf, String>,
    /// 本机驱动器清单：`形如 "C:\" 的大写路径 -> 驱动器信息`。
    ///
    /// 主页容量条 / 导航窗格树 / 属性窗口共用这一份，避免各处各自猜类型。
    /// 只在"进入主页 + 定时兜底 + 设备插拔"三种时机重取，避免每帧做磁盘 IO。
    pub drives: HashMap<String, zenfile_core::drives::DriveInfo>,
    /// 上一次刷新驱动器清单的时刻（用于 WM_TIMER 里的节流）。
    pub drives_stamp: Option<std::time::Instant>,
    /// 导航窗格里**处于展开状态**的节点 key。
    ///
    /// key 的约定：目录节点用大写的绝对路径；虚拟节点用固定串（`thispc`）。
    /// 目录节点展开时的子目录清单另存在 `sidebar_children` 里 —— 每帧都去
    /// `read_dir` 会把 UI 的响应速度按挂在树的深度上。
    pub sidebar_expanded: HashSet<String>,
    /// 已展开节点的子目录清单。`None` 表示还没取过。
    pub sidebar_children: HashMap<String, Vec<PathBuf>>,
    /// 鼠标悬停的导航行下标（用于 hover 反馈）。
    pub sidebar_hover: Option<usize>,
    /// 鼠标悬停的**条目**下标（`tab.entries` 的下标，不是可见行号）。
    ///
    /// 用条目下标而不是行号：滚动/排序之后行号会变，而"鼠标指着这个东西"不会变。
    /// 列表行和首页磁贴共用这一个字段。
    pub hover_row: Option<usize>,
    /// `hover_row` 属于第几个窗格（分栏时才有意义）。
    ///
    /// `hover_row` 是**条目下标**，两个窗格各自的 entries 下标互不相干 ——
    /// 只记下标的话，指着右窗格第 3 行，左窗格的第 3 条也会跟着亮。
    /// 绘制端（paint_panes 逐个窗格切 focus_pane 画）用它把悬停高亮
    /// 限制在光标真正指着的那个窗格里。
    pub hover_pane: Option<usize>,
    /// 鼠标是不是悬在导航窗格右上角的折叠按钮上。
    pub hover_sidebar_collapse: bool,
    /// 工具栏左上三个导航按钮的悬停位：0=无，1=返回，2=前进，3=向上。
    ///
    /// 这三个按钮以前只有"可用/不可用"两种字色，鼠标移上去毫无反馈，
    /// 看着像三个图标而不是三个按钮。
    pub hover_nav: u8,
    /// 鼠标悬在表头的哪一列上（用于画"筛选"小箭头）。
    pub hover_header: Option<usize>,
    /// 已被"剪切"的那些条目的路径（小写）。
    ///
    /// 资源管理器里剪切过的东西在原地是**虚掉**的：它已经不在原来的位置上了，
    /// 只是还没落下去。不画这一层，剪切和复制在界面上长得一模一样。
    pub cut_set: HashSet<String>,
    /// 导航窗格的滚动偏移（像素）。树比窗格长时才非 0。
    pub sidebar_scroll: i32,

    // —— 界面动画（第57轮：设置-交互页「动画」组）——
    //
    // 都是"触发时播一次"的过渡动画（切目录 / 悬停 / 选中），没有常驻循环
    // 动效：常驻会一直占着定时器，也会让静态截图的像素判据永远读不准。
    /// 动画定时器是不是还挂着。只用它避免重复 SetTimer 把周期重置
    /// （每帧 InvalidateRect 都去 Set 一次的话，定时器永远等不到 16ms 期满）。
    anim_timer_on: std::cell::Cell<bool>,
    /// 上一帧动画推进的时刻（ms）。用来算每帧间隔 dt，把缓动做成
    /// **与帧率无关**：掉帧时不拖慢、不抢跑，动画在墙上时钟里匀速。
    anim_last: std::cell::Cell<u64>,
    /// 地址栏的进 / 出场动画。`kind`: 0=进入（从右滑入 + 回弹），1=退出（向右滑出）。
    pub addr_anim: Option<AddrAnim>,
    /// 标题栏按键的悬停动画（键编号 -> 状态）。键编号见 `CAP_*`。
    ///
    /// 第63轮：三大金刚键 + 侧栏开关 + 更多菜单 + 新建标签，鼠标刚划上去时
    /// 各弹一下（形变方向按键的语义不同），播完归位。**受设置-交互页
    /// 「标题栏按键」那个开关控制**，关掉就完全不进这张表。
    pub cap_anim: HashMap<u8, CapAnim>,
    /// 已经弹过一次的键（`cap_anim` 出表后靠它挡住重复触发）。
    ///
    /// 没有这张表的话，鼠标在同一个键上停满 420ms 后下一帧又会插一条新记录，
    /// 动画**无限循环**，16ms 定时器也永远 Kill 不掉。
    pub cap_fired: HashSet<u8>,
    /// 导航窗格"当前位置"指示条**此刻画在哪个 y**（每帧向 target 缓动）。
    pub sb_active_y: Option<f32>,
    /// 上面那个 y 这一帧的目标（由 paint 按行几何写入）。
    pub sb_active_target: Option<f32>,
    /// 导航窗格悬停高亮条此刻画在哪个 y。
    pub sb_hover_y: Option<f32>,
    pub sb_hover_target: Option<f32>,
    /// 导航窗格平滑滚动的目标偏移（切目录时把"我在哪"那一行滚进视野）。
    pub sb_scroll_target: Option<i32>,
    /// 上一次定位过的"当前位置"行下标。只有它变了才需要重新算滚动 ——
    /// 否则每帧算一次会把用户自己滚的位置抢走。
    pub sb_last_active: Option<usize>,
    /// 条目图标的当前放大系数（大图标 / 中图标视图，key = 条目 key）。
    pub icon_zoom: HashMap<String, f32>,
    /// 条目图标这一帧的放大**目标**（paint 每帧重写，没写到的项 = 0）。
    pub icon_zoom_tgt: HashMap<String, f32>,
    /// 正在"弹一下"的条目（key -> 开始时刻 ms，详细信息 / 列表视图）。
    pub icon_bounce: HashMap<String, u64>,
    /// 这一帧处于 悬停 / 选中 / 焦点 的条目 key。
    pub icon_active: HashSet<String>,
    /// 上一帧处于 悬停 / 选中 / 焦点 的条目 key —— "是不是刚变成这个状态"
    /// 靠它判（只在刚变的那一帧弹一次，不是一直弹）。
    pub icon_prev_active: HashSet<String>,
    /// 焦点行要不要**画**出来。
    ///
    /// `focus_row` 永远存在（第 0 行默认就是焦点行），但资源管理器在刚打开一个
    /// 文件夹时，是不会给第 0 行刷一条高亮的 —— 那一行看起来就像"默认被勾选了"。
    /// 只有用户真的用鼠标点过或用键盘移动过，焦点行才显形。
    pub focus_visible: bool,
    /// 标题栏上鼠标悬停的标签序号（用于画关闭按钮）。
    pub hover_tab: Option<usize>,
    /// 鼠标正悬停的标签**关闭按钮（×）**。
    ///
    /// 之前只记 `hover_tab`（落在标签任意位置都为真），于是 × 的底色跟着整个
    /// 标签一起亮 —— 鼠标离 × 还有一个字宽时它就亮了，鼠标真压在 × 上时反而
    /// 和"标签悬停"分不出来。两者必须**分开记**。
    pub hover_tab_close: Option<usize>,
    /// 鼠标正悬停的「新建标签（+）」键。第63轮加：它原先恒不传 hover，
    /// 画出来是个永远不响的按钮。
    pub hover_new_tab: bool,
    /// 状态栏布局切换按钮的悬停位：0=无，1=大图标，2=详细信息。
    pub hover_view_btn: u8,
    /// "返回/上一级之后要把哪一项选中"。
    ///
    /// 目录枚举是异步的：go_up 的瞬间目标目录一行都没有，直接查找必然落空。
    /// 所以先把"要选中的路径"记下来，等枚举批次到达（drain）后再落地。
    pub pending_select: Option<PathBuf>,
    /// 选中落地之后**顺手进入重命名**。
    ///
    /// 资源管理器里"新建文件夹"之后光标直接在新文件夹的名字上等着改 ——
    /// 建完不进重命名，就得先找到它、右键、改名，三步走路。
    pub pending_rename: bool,
    /// 新建后的重命名要不要"只选主部"。建 .txt 时要（保住扩展名），建文件夹时不分。
    pub rename_stem_only: bool,
    /// 上一次单击落在哪个**条目**上（`tab.entries` 下标），以及当时的时刻（ms）。
    ///
    /// 用来区分"双击打开"和"两次慢速单击改名"：资源管理器里再点一下已经选中的
    /// 名字（间隔超过双击时限）就进重命名，连着两下是打开。
    pub last_click: Option<(usize, u64)>,
    /// 本次右键菜单要作用在哪些条目上。
    ///
    /// `Some(vec![])` 表示右键点在了空白处；`None` 表示不是从右键进来的。
    /// 之前直接调 `selected_paths()`，而它在选中为空时会**退回焦点行** ——
    /// 于是"空白处右键"拿到的还是上一次那个条目，菜单判定的就是错的。
    pub context_targets: Option<Vec<PathBuf>>,
    /// 回收站条目（进入回收站视图时刷新一次）。
    /// 后台枚举回收站的结果通道。`Some` 表示有一趟枚举还在路上。
    ///
    /// 回收站是逐盘直读 `$Recycle.Bin` 的：盘多或者垃圾多的时候能到几百毫秒，
    /// 放在 UI 线程里做就是"点一下回收站卡一下"。所以和目录枚举一样丢后台，
    /// 结果由 `WM_APP_RECYCLE` 收口。
    pub recycle_rx: Option<Receiver<Vec<zenfile_core::recycle::Item>>>,
    /// 回收站枚举的世代号。切走又切回来时，上一趟的迟到结果要丢掉，
    /// 否则会把别的视图的显示数据顶掉。
    pub recycle_gen: u64,
    /// 目录枚举世代号的**全局**发号器（所有标签共用一条计数）。
    ///
    /// 不能按 tab 各自 `generation += 1`：两个标签都从 0 起数就会撞号，
    /// `find_tab_mut` 按 generation 找 tab 时把 A 标签的枚举批次塞进 B 标签
    /// —— 实测左右分栏后左窗格显示 a∪b 合并、右窗格永久空白（第45轮）。
    pub generation_seq: u64,
    /// 回收站的排序：`(列序号, 是否降序)`。列序号同 `recycle_columns`：
    /// 0=名称 1=原位置 2=删除时间 3=大小。点列头就切这一列，再点一次换方向。
    pub recycle_sort: (usize, bool),
    /// 鼠标悬停的回收站条目下标（`recycle_items` 的下标）。
    pub hover_recycle: Option<usize>,
    /// 回收站顶部三个按钮的悬停位：0=无，1=还原，2=彻底删除，3=清空回收站。
    pub hover_recycle_btn: u8,
    pub recycle_items: Vec<zenfile_core::recycle::Item>,
    /// 回收站里被选中的项（按 `$R` 实体名索引）。
    pub recycle_sel: HashSet<String>,
    /// 设置页里鼠标悬停的行（存的是该行的动作编号，行位置会随开关文案变化）。
    pub settings_hover: Option<u32>,
    /// 设置页当前停在哪个小标签（见 `SETTINGS_TABS`）。
    pub settings_tab: u8,
    /// 设置页「右键菜单」正在编辑哪一类（下标见 `MenuKind`）。
    ///
    /// 第55轮：照 ContextMenuManager 那样按"右键落在什么上"分类型管理 ——
    /// 文件 / 文件夹 / 目录背景 / 多选 / 驱动器 各一套排布。
    pub settings_ctx_kind: u8,
    /// 设置页顶部小标签的悬停下标。
    pub settings_hover_tab: Option<u8>,
    /// 排布编辑器里鼠标悬停的组件/条目 id。
    pub settings_hover_card: Option<u32>,
    /// 「外观」圆角树里当前选中的项 id（"global" / "sidebar" / "sidebar.hover"）。
    pub settings_radius_sel: String,
    /// 「外观」圆角树里已展开的节点 id。
    pub settings_tree_open: HashSet<String>,
    /// 「外观」圆角树里鼠标悬停的第几行。
    pub settings_hover_tree: Option<usize>,
    /// 「外观」圆角滑杆正被拖着（按下后不松手连续改值）。
    pub settings_radius_drag: bool,
    /// 「交互」页：当前选中的树节点（None = 只展开分组，没选具体项）。
    pub settings_interact_sel: Option<&'static str>,
    /// 「交互」页：哪些分组是展开的。
    pub settings_interact_open: std::collections::BTreeSet<&'static str>,
    /// 「快捷键」：正在等一个新按键组合的动作名（`None` = 没在录）。
    pub hotkey_capture: Option<String>,
    /// 设置页「右键菜单」右上那块「打开方式」的真实候选程序（程序名 + 图标）。
    ///
    /// 只在切到那一页时算一次：`openwith::candidates()` 要去注册表和 Shell
    /// 问一圈，每帧重算会把设置页拖慢。
    pub openwith_preview: Option<Vec<(String, isize)>>,
    /// 「右键菜单」页里喂给实时菜单渲染的"样本文件"。
    ///
    /// 设置页自己是个虚拟标签（`entries` 是空的），照它渲染出来的会是"空白处
    /// 右键"那种 6 行的短菜单；而用户真正想调的是"对着文件右键"那一整套。
    /// 所以切进那一页时挑一个**真实存在的文件**存下来，菜单就按它长。
    pub menu_preview_items: Option<Vec<PathBuf>>,
    /// 「右键菜单」页里鼠标悬停在第几条菜单项上（下标对应 `own_menu_rows` 的输出）。
    pub settings_hover_menu: Option<usize>,
    /// 「右键菜单」页右上那颗「查找可用的打开软件」按钮的悬停态。
    pub settings_hover_find: bool,
    /// 「右键菜单」页右侧预览区：鼠标停在第几个格式样本上。
    pub settings_prev_hover: Option<usize>,
    /// 「右键菜单」页右侧预览区的滚动量（格子排不下就滚，见 `prev_content_h`）。
    pub settings_prev_scroll: i32,
    /// 预览菜单里刚点过一条的时刻（`GetTickCount64()`）—— 用来在预览面板
    /// 标题那一行闪一句"不会真的执行"。0 = 没有待显示的提示。
    pub preview_note_at: u64,
    /// 此刻弹出来的菜单是**预览区里弹的**（设置页右侧那些格式样本）。
    ///
    /// 预览菜单只用来看长相：`run_command` 见到它就啥也不干，只提示一句 ——
    /// 否则"右键一个 .docx 样本然后点了删除"，会真的去动当前标签里的东西。
    pub preview_menu: bool,
    /// 「右键菜单」页右上已经做过一次全量"查找可用的打开软件"。
    pub settings_ow_found: bool,
    /// 排布编辑器里正在拖的卡片。
    pub settings_drag: Option<crate::settings::SettingsDrag>,
    /// 「界面 / 右键菜单」的排布（顺序 + 被收进「更多」的那些）。
    pub ui_settings: crate::settings::Settings,
    /// 文件名索引（Everything 式秒搜）。整卷一份，常驻内存。
    pub index: Option<std::sync::Arc<zenfile_core::index::Index>>,
    /// `index` 属于哪个卷根（`C:\`）。换了卷要重新载入。
    pub index_root: Option<String>,
    /// 索引当前在干什么：给状态栏/列表顶上那行提示用。
    pub index_state: IndexState,
    /// 后台"建索引"线程的消息通道。
    pub index_rx: Option<Receiver<IndexMsg>>,
    /// 让正在跑的建索引线程尽快收工（换卷 / 关窗时用）。
    pub index_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// 用户回车搜索了，但索引还没到位：此刻列表里顶着的是"当前目录"的递归结果，
    /// 索引一到就要换成全卷结果。
    pub index_pending: bool,
    /// 搜索框防抖：还剩几个 100ms 节拍才真的去搜。每敲一个键都重置，
    /// 这样"wind" 不会拆成 w / wi / win / wind 四次全卷查询。
    pub search_ticks: u8,
    /// 防抖窗口内查询串又变过（为真才需要在归零时跑一次）。
    pub search_dirty: bool,
    /// 上一次整卷查询的结果条数与耗时（毫秒），给状态栏显示。
    pub search_hits: usize,
    pub search_ms: f64,
    /// 标题栏三大金刚键的悬停位：0=无，1=最小化，2=最大化，3=关闭。
    pub hover_caption: u8,
    /// 按下未抬起的那一个（0=无，1/2/3 同上）。
    pub pressed_caption: u8,
    /// TrackMouseEvent 是否已挂上（挂一次，收到 WM_MOUSELEAVE 才允许重挂；
    /// 每次移动都重挂会吃到非客户区→客户区切换的滞回 leave，悬停全灭）。
    pub mouse_tracked: bool,
    /// 地址栏是否处于"文本编辑"态。
    ///
    /// Win11 的地址栏平时是**面包屑**（一节一节可点的路径），只有点空白处或
    /// 按 Ctrl+L/F4 才变成可输入的文本框。平时把 EDIT 藏着，编辑时再显示出来。
    pub addr_editing: bool,
    // ---- 第75轮：子 EDIT 条件失效的快照缓存 ----------------------------
    //
    // `paint()` 每帧都要决定"要不要让两个子 EDIT 跟着重画"。第74轮之前这个
    // 决定是**无条件**的（每帧各一次 `InvalidateRect(..., erase=true)`），
    // 于是每帧其实是 3 次重绘而不是 1 次。现在改成"内容真的变了才失效"，
    // 判据全部落在这两个快照上。
    //
    // 为什么判据要覆盖得比"提示词"更宽：`paint_search_decor` 画在子 EDIT
    // 自己身上，它读的是 `theme.text_dim`（提示词与放大镜的墨色）、
    // `metrics.scale`（字号/放大镜半径）和 `icon_theme`（放大镜字形）。
    // 早先只按提示词判据做的话，切明暗主题 / 改缩放 / 换图标主题这三件事
    // 会留下旧像素 —— 这正是"每帧无条件失效"一直在顺手掩盖的。
    // 所以判据取这四项的合集：**变了才失效，一个都不会漏**。
    /// 搜索框当前画在它自己身上的那套内容的快照（提示词 + 墨色 + 缩放 + 图标主题）。
    pub child_search_key: ChildEditKey,
    /// 地址框的同类快照；另外带了"当时是否可见"。
    pub child_address_key: ChildEditKey,
    /// 鼠标悬停的面包屑下标（hover 反馈）。
    pub addr_hover: Option<usize>,
    /// 左键拖拽（拖条目 / 空白处框选）。松手或取消后置回 None。
    pub drag: Option<Drag>,
    /// 拖标签。与 `drag` 分开：两者可以同时挂着（拖文件的同时不会拖标签），
    /// 但状态字段混在一起会让"松手时到底在干嘛"变成猜谜。
    pub tab_drag: Option<TabDrag>,
    /// 拖着**文件**悬在标签上时：记住"从第几毫秒开始悬在这个标签上"，
    /// 满了 600ms 就自动切过去（资源管理器的行为：拖到标签上等一下会自动翻页）。
    pub tab_hover_since: Option<(usize, std::time::Instant)>,
    /// 拖拽当前悬着的可投放目标。
    pub drop_target: Option<DropTarget>,
    /// 拖着**一个文件夹**悬在内容区边缘时算出来的贴靠方向。
    ///
    /// 空手落在内容区中间是"什么也不做"，落在边上则是"把这个文件夹开成一个新窗格" ——
    /// 这就是用户说的"分栏拖进去的文件夹"。与 `drop_target` 分开存：
    /// 前者是"搬到某个目录"，后者是"开一个新视图"，落地动作完全不同。
    pub drag_dock: Option<DockZone>,
    /// 正在拖哪一条分隔线（导航窗格宽度 / 详细信息表头的某一列）。
    ///
    /// `None` 之外还要记住"按下时 cursor 在分隔线上的偏移"，否则拖动一开始
    /// 分隔线会直接跳到光标正下方（看着像被瞬移了一下）。
    pub drag_split: Option<SplitDrag>,
    /// 鼠标正悬停在哪条分隔线上（只为了把光标换成左右箭头）。
    pub hover_split: Option<Split>,
    /// 鼠标正悬停在**窗格之间**的分隔线上：`(是否竖线, 左/上窗格, 右/下窗格)`。
    ///
    /// 只为换光标（↔ / ↕）与绘制高亮，不参与任何业务判定。
    pub hover_pane_div: Option<(bool, usize, usize)>,
    /// 正在拖窗格之间的分隔线（自由调整两个窗格的布局大小）。
    pub pane_div_drag: Option<PaneDivDrag>,
    /// 用户在 ZenFile 里手动改过的视图：路径(小写,无尾斜杠) -> 视图。
    ///
    /// 优先级高于资源管理器那套 bag。原因很实在：bag 只有在资源管理器
    /// **去过**这个文件夹时才存在，写不进去的时候（没去过 / 解析不出节点）
    /// 至少本进程里来回切换不该被默认值吃掉。
    pub view_overrides: HashMap<String, ViewMode>,
    /// 每个目录的**滚动位置记忆**：路径(小写,无尾斜杠) -> scroll 像素。
    ///
    /// 用户要求：从子文件夹返回上一级时，要回到"打开它之前"的那一行，
    /// 而不是跳到列表最顶。以前 `apply_history` 无条件 `scroll = 0`，
    /// 于是每次返回都把用户"滚到一半的位置"扔掉。这里按目录各记一份，
    /// 前进/后退时取回来（纯内存，不落盘 —— 跨进程的滚动位置没有价值）。
    pub scroll_memory: HashMap<String, i32>,
}

/// 哪一条分隔线。
#[derive(Clone, Copy, PartialEq)]
pub enum Split {
    /// 导航窗格与内容区之间（改导航窗格宽度）
    Sidebar,
    /// 详细信息表头第 n 列的右边线（改该列宽度）
    Column(usize),
}

pub struct SplitDrag {
    pub which: Split,
    /// 按下时光标的 x：拖动量一律按"相对按下点"算，
    /// 否则一按下分隔线就会瞬移到光标正下方。
    pub start_x: i32,
    /// 拖动开始时的宽度（DIP）
    pub start_w: i32,
}

/// 拖**窗格之间**的分隔线：把两个相邻窗格的宽/高按拖动量重新分配。
///
/// 只处理"两个窗格共享一条边"的情况（1/2 二分贴靠的产物）。多格 2x2
/// 四分不在支持范围 —— 四角贴靠已按用户要求砍掉。
pub struct PaneDivDrag {
    /// true = 竖分隔线（左右两窗格，改宽度）；false = 横线（上下，改高度）
    pub vertical: bool,
    /// 分隔线左/上侧那个窗格的下标
    pub a: usize,
    /// 右/下侧
    pub b: usize,
    /// 按下时光标的归一化坐标（0..1，相对内容区）——竖线用 x，横线用 y。
    /// 拖动量按"相对按下点"算，否则一按下分隔线就瞬移到光标正下方。
    pub start_n: f32,
    /// 按下时两个窗格的归一化矩形（快照，拖动过程中反复从它推导）
    pub ra: RectF,
    pub rb: RectF,
}

/// 拖标签松手之后要做什么。`App::finish_tab_drag` 只负责算，执行在 WndProc。
pub enum TabDrop {
    Nothing,
    /// 同一个窗格内换位置
    Reorder { pane: usize, from: usize, to: usize },
    /// 贴进内容区的某个方向（会真的分出一个窗格）
    Dock { pane: usize, index: usize, zone: DockZone },
    /// 拖到窗口外：把这个标签拆成独立进程
    TearOff { pane: usize, index: usize, path: PathBuf },
}

/// 拖着**标签**在动。三个去处：换顺序、贴进内容区（8 向）、拖出窗口拆成新进程。
pub struct TabDrag {
    /// 这个标签原来在哪个窗格
    pub pane: usize,
    pub index: usize,
    pub origin: (i32, i32),
    pub cur: (i32, i32),
    /// 越过阈值才算真的在拖（普通单击也会有几个像素的位移）
    pub active: bool,
    /// 还悬在标签条上时的重排落点
    pub over_tab: Option<usize>,
    /// 悬在内容区里时的贴靠方向
    pub dock: Option<DockZone>,
    /// 已经拖出窗口客户区（松手就拆成新进程）
    pub outside: bool,
}

/// 左键拖拽的当前状态（拖条目 或 空白处框选）。
pub struct Drag {
    pub origin: (i32, i32),
    pub cur: (i32, i32),
    pub kind: DragKind,
    /// 越过阈值之后才算真的在拖：普通单击也会产生几像素的移动，
    /// 不给阈值的话每次点一下都会闪一下框选矩形。
    pub active: bool,
    /// Items 拖拽时按下那一刻就定下来的清单 —— 拖的过程中选中集不该再变。
    pub items: Vec<PathBuf>,
    /// 是不是"复制"语义：按住 Ctrl，或者本次拖拽的消息里带了 MK_CONTROL。
    pub copy: bool,
}

#[derive(Clone, Copy, PartialEq)]
pub enum DragKind {
    /// 拖着选中的条目去别处
    Items,
    /// 在空白处拉框选
    Marquee,
}

/// 拖拽此刻悬在哪一个可投放目标上（画高亮 + 松手时落地用）。
#[derive(Clone, Debug)]
pub struct DropTarget {
    pub path: PathBuf,
    pub kind: DropKind,
}

#[derive(Clone, Copy, Debug)]
pub enum DropKind {
    /// 内容区第 n 行（一个文件夹）
    Row(usize),
    /// 地址栏第 n 节面包屑（#12：拖到哪一级就进哪一级）
    Crumb(usize),
    /// 导航窗格第 n 行
    Sidebar(usize),
    /// **标签条第 n 个标签**：把文件拖到标签上，落到那个标签所在目录
    Tab(usize),
}

/// 导航窗格宽度与列宽的取值/夹取范围（DIP）。太窄图标都画不下，太宽占满窗口。
pub const SIDEBAR_W_MIN: i32 = 110;
pub const SIDEBAR_W_MAX: i32 = 600;
pub const COL_W_MIN: i32 = 40;
pub const COL_W_MAX: i32 = 900;

/// 文件名索引在后台的进度状态。
#[derive(Clone, PartialEq)]
pub enum IndexState {
    /// 没在动
    Idle,
    /// 正在从磁盘缓存载入
    Loading,
    /// 正在遍历整卷（数字 = 已扫到的条目数，用于给个可见的进度）
    Building(usize),
    /// 索引已就绪
    Ready,
    /// 建不了（非 NTFS / 没权限 / UNC），搜索回退到现地递归
    Failed(String),
}

/// 边打字边搜的防抖节拍数。TIMER_DRAIN 每 100ms 一跳，2 跳 ≈ 200ms：
/// 再短就会在用户还没停手时白跑整卷查询，再长就失去"即时"的手感。
pub const SEARCH_DEBOUNCE_TICKS: u8 = 2;

pub enum IndexMsg {
    /// 建索引过程中周期性回报已扫条目数
    Progress(usize),
    Ready(std::sync::Arc<zenfile_core::index::Index>),
    Failed(String),
}

/// 地址栏的进 / 出场动画（第57轮）。
///
/// `kind == 0`：进入新目录，面包屑从地址框右沿滑进来，到位后回弹一下。
/// `kind == 1`：退出（后退），**切换前**那一串面包屑整体向右滑出去 ——
/// 所以要把它存下来（`prev`），不然画的时候拿到的已经是新路径了。
#[derive(Clone)]
pub struct AddrAnim {
    pub kind: u8,
    /// 开始的时刻（`GetTickCount64()`）
    pub start: u64,
    /// 退出时画的那串面包屑
    pub prev: Vec<Crumb>,
}

/// 动画时长（ms）。写死而不是做成可调项：这些数字是"看着顺"的经验值，
/// 给用户一个 100~600 的滑杆只会让人调出一个卡手的界面。
pub const ANIM_ADDR_IN_MS: u64 = 300;
pub const ANIM_ADDR_OUT_MS: u64 = 200;
pub const ANIM_BOUNCE_MS: u64 = 380;
/// 大图标 / 中图标下"悬停 / 选中"放大的幅度（比例）。
pub const ANIM_ZOOM: f32 = 0.10;
/// 详细信息 / 列表下回弹的幅度与振荡圈数。
pub const ANIM_BOUNCE_AMP: f32 = 0.18;
pub const ANIM_BOUNCE_FREQ: f32 = 1.6;

/// 标题栏按键悬停动画：一次播放完就归位的时长（ms）。
pub const ANIM_CAPTION_MS: u64 = 420;

/// 标题栏按键的编号（`paint::draw_caption_button` 的 `which` 同口径，
/// 0 号留给"侧栏开关 / 更多菜单 / 新建标签"那类自有键）。
pub const CAP_MIN: u8 = 1;
pub const CAP_MAX: u8 = 2;
pub const CAP_CLOSE: u8 = 3;
/// `which` 取这两个值时表示自有键（侧栏开关 / 更多菜单 / 新建标签）。
pub const CAP_TOGGLE: u8 = 10;
pub const CAP_MENU: u8 = 11;
pub const CAP_NEW_TAB: u8 = 12;
/// 标签上的关闭按钮（×）。**单独一个编号**，不能复用 `CAP_CLOSE`：
/// 三个窗口键任何时候只有一个可能悬停，而标签 × 是**每个标签都有一个**，
/// 共用一个 key 会让"记一个键的动画"变成"记一堆键的动画"。
pub const CAP_CLOSE_TAB: u8 = 20;

/// 一个标题栏按键的悬停动画（弹一下 → 归位）。
///
/// 只存"开始时刻"：进度每帧现算即可。`start` 用 `Option` 包起来是有意的 ——
/// 键被移开鼠标时**整条记录删掉**（而不是把时刻改小），这样"还有没有动画在跑"
/// 的判据和"这一刻该画成什么样"是同一个数据源，不会出现"定时器还挂着但画的是
/// 静止态"这种空转。
#[derive(Clone, Copy)]
pub struct CapAnim {
    /// 开始时刻（`GetTickCount64()`）
    pub start: u64,
}


/// 地址栏面包屑的一节。
#[derive(Clone)]
pub struct Crumb {
    /// 显示文字。目录走本地化显示名（"Users"→"用户"），盘符走卷标。
    pub label: String,
    /// 点击后要导航到的路径；`None` = 虚拟节点（主页 / 回收站 / 设置 / 此电脑）。
    pub target: Option<PathBuf>,
    /// 这一节是"折叠起来的中间层级"占位（`…`），点击跳到被折叠的最深一层。
    pub ellipsis: bool,
    /// 这一节是搜索状态芯片（`搜索“xx”`，学 Pulse）：底色是强调色淡染、
    /// 白字，**不参与导航**（点击把焦点交还搜索框）。
    pub chip: bool,
}

/// 向 Shell 问目录的本地化显示名（见 `App::display_name`）。
fn shell_display_name(path: &Path) -> Option<String> {
    crate::icons::IconCache::display_name(path)
}

/// 把路径拆成"逐级累积"的若干节：`C:\Users\a` → [`C:\`, `C:\Users`, `C:\Users\a`]。
///
/// 不用 `Path::components` + `PathBuf::push` 去拼：`PathBuf::push` 遇到以分隔符
/// 开头的片段会**整条替换**（`push("\\")` 把 `C:` 变成 `\`），而且 `C:`（盘符前缀）
/// 会和 `C:\`（盘符根）分成两节 —— 前者是"该盘上的当前目录"，点下去会跳错地方。
/// 这里按字符串逐级累积，第一节就带根分隔符。
fn path_segments(path: &Path) -> Vec<PathBuf> {
    let s = path.to_string_lossy().to_string();
    let mut segs: Vec<PathBuf> = Vec::new();
    if s.is_empty() {
        return segs;
    }
    if let Some(rest) = s.strip_prefix(r"\\") {
        // UNC：`\\server\share\...`
        let mut acc = String::from(r"\\");
        for p in rest.split('\\').filter(|p| !p.is_empty()) {
            if acc != r"\\" {
                acc.push('\\');
            }
            acc.push_str(p);
            segs.push(PathBuf::from(&acc));
        }
        return segs;
    }
    let b = s.as_bytes();
    if b.len() >= 2 && b[1] == b':' {
        // 盘符路径：第一节必须带上根分隔符（`C:\`，不能是 `C:`）
        let mut acc = String::from(&s[..2]);
        if s.len() > 2 && b[2] == b'\\' {
            acc.push('\\');
        }
        segs.push(PathBuf::from(&acc));
        let rest = s[acc.len()..].trim_start_matches('\\');
        for p in rest.split('\\').filter(|p| !p.is_empty()) {
            if !acc.ends_with('\\') {
                acc.push('\\');
            }
            acc.push_str(p);
            segs.push(PathBuf::from(&acc));
        }
        return segs;
    }
    // 相对路径等退化情况：按分隔符逐级累积
    let mut acc = String::new();
    for p in s.split('\\').filter(|p| !p.is_empty()) {
        if !acc.is_empty() {
            acc.push('\\');
        }
        acc.push_str(p);
        segs.push(PathBuf::from(&acc));
    }
    segs
}

/// 取某个驱动器的完整信息（类型 / 卷标 / 容量），供首页容量条与导航窗格使用。
pub fn drive_of<'a>(app: &'a App, path: &Path) -> Option<&'a zenfile_core::drives::DriveInfo> {
    let key = path.to_string_lossy().to_uppercase();
    app.drives.get(&key)
}

pub const HOVER_SIDEBAR_TOGGLE: u8 = 0b01;
pub const HOVER_MENU_BTN: u8 = 0b10;

/// 首页驱动器信息的兜底刷新间隔（秒）。
const DRIVES_TTL_SECS: u64 = 5;

/// 导航窗格一行的图标来源。
#[derive(Clone, Debug)]
pub enum SidebarIcon {
    /// 段首导航项没有真实路径，只能用 Segoe 字形
    Glyph(&'static str),
    /// 有真实路径：交给 `IconCache` 去问系统图标（含 `desktop.ini` 自定义图标、
    /// 驱动器符号），拿到什么就画什么，和资源管理器一致。
    Path(PathBuf),
    /// 系统库存图标（`SHGetStockIconInfo`），存的是 SIID 常量值。
    /// 回收站这种没有实体路径的目标只能走这里。
    Stock(i32),
}

/// 导航窗格的一行。**渲染与命中测试共用这份排列**，不存在"看得见但点不到"。
#[derive(Clone, Debug)]
pub struct SidebarRow {
    pub label: String,
    /// 点击这一行要导航到哪（虚拟视图用 `shell:xxx` 哨兵值）
    pub target: String,
    pub depth: usize,
    /// 有子节点 worth showing（决定要不要画 V 形箭头）
    pub expandable: bool,
    pub expanded: bool,
    /// 展开状态用的 key；不可展开的行为空串
    pub key: String,
    pub icon: SidebarIcon,
    /// 树线：`guides[k]` 为真就在第 k 层缩进上画一条贯穿整行的竖线。
    ///
    /// 长度等于 `depth`（第 0 层是根，恒为 false —— 根那一列不画竖线）。
    /// 没有这一份信息就只能画成"每层一条平行线"，那是栅格不是树：
    /// 树的关键在于**最后一个孩子那里线要收口**，后面的兄弟不再有线。
    pub guides: Vec<bool>,
    /// 自己是不是父节点的最后一个孩子（决定连接线要在行中线收口成 └）。
    pub last_child: bool,
    /// 有没有父节点（根节点没有，根节点不画连接线）。
    pub has_parent: bool,
    /// 分组之间的空行：只占一格高度，**不画也不参与命中**。
    ///
    /// 之前"此电脑/盘符/常用文件夹/回收站"是一路平铺下来的，三组之间没有任何
    /// 分隔，扫一眼分不清哪几行是一拨（用户：中间加一点空隙）。
    pub spacer: bool,
}

impl SidebarRow {
    /// 分组之间的空行。
    pub fn spacer() -> Self {
        SidebarRow {
            label: String::new(),
            target: String::new(),
            depth: 0,
            expandable: false,
            expanded: false,
            key: String::new(),
            icon: SidebarIcon::Glyph(""),
            guides: Vec::new(),
            last_child: false,
            has_parent: false,
            spacer: true,
        }
    }
}

/// "此电脑"这个虚拟节点在 `sidebar_expanded` 里的 key。
pub const SIDEBAR_THISPC: &str = "thispc";

/// 一级常用文件夹。顺序照抄资源管理器左侧的上半段。
fn user_folders() -> Vec<(&'static str, String)> {
    use zenfile_core::paths::known_folder;
    [
        ("主文件夹", ""),
        ("桌面", "Desktop"),
        ("下载", "Downloads"),
        ("文档", "Documents"),
        ("图片", "Pictures"),
        ("音乐", "Music"),
        ("视频", "Videos"),
    ]
    .iter()
    .map(|(label, clsid)| {
        // 已知文件夹一律走 Shell 自己给的路径（用户可能把它重定向到 D 盘），
        // 不要凭 USERPROFILE 去拼 —— 拼出来的和实际位置可以完全无关。
        let path = if clsid.is_empty() {
            std::env::var("USERPROFILE").unwrap_or_default()
        } else {
            known_folder(clsid).unwrap_or_default()
        };
        (*label, path)
    })
    .filter(|(_, p)| !p.is_empty() && std::path::Path::new(p).is_dir())
    .collect()
}

impl App {
    pub fn new(hwnd: HWND, cfg: Config) -> Self {
        let (engine_tx, engine_rx) = channel::<EngineMsg>();
        let (io_tx, io_rx) = channel::<String>();
        let theme = if cfg.core.theme == "light" { theme::light() } else { theme::dark() };
        // 上次会话里手动切过的视图：启动时从配置加载进覆盖表（第29轮）。
        // 之前这张表是纯内存的，重启即丢 —— 用户反馈"文件夹的布局样式
        // 压根不会保存"。只认已知的视图名，脏值直接丢（parse_view 兜底
        // 会把未知串都读成"详细信息"，反而把默认值顶掉）。
        let view_overrides: HashMap<String, ViewMode> = cfg
            .files
            .view
            .overrides
            .iter()
            .filter(|(_, v)| matches!(v.as_str(), "details" | "list" | "medium" | "large"))
            .map(|(k, v)| (k.clone(), parse_view(v)))
            .collect();
        // 图标主题在第一帧绘制之前就位（default = 系统图标，等价于不接管）。
        crate::icon_theme::set_theme(
            &cfg.ui.icon_theme,
            cfg.core.theme != "light",
        );
        Self {
            hwnd,
            hwnd_address: HWND(std::ptr::null_mut()),
            hwnd_search: HWND(std::ptr::null_mut()),
            panes: Vec::new(),
            focus_pane: 0,
            show_hidden: cfg.files.view.show_hidden,
            show_system: cfg.files.view.show_system,
            cfg,
            metrics: Metrics::new(96),
            theme,
            icons: IconCache::new(),
            engine_rx,
            engine_tx,
            watch_rx: None,
            watcher: None,
            menu_hwnd: None,
            bridge: None,
            rename_hwnd: None,
            rename_index: None,
            tree: None,
            dirty: true,
            last_size: (0, 0),
            back_surface: crate::paint::BackSurface::default(),
            drag_origin: None,
            tab_drag: None,
            tab_hover_since: None,
            drag_dock: None,
            settings_radius_sel: "global".to_string(),
            // 圆角树默认**收起**（第38轮回退过一次"默认全展开"）：
            // verify_settings20 的 B2/B3/C2 都按"点一下才展开"来定位树行，
            // 默认展开会把行号假设全部打乱 —— 而且那不是本轮要改的东西。
            settings_tree_open: HashSet::new(),
            settings_hover_tree: None,
            settings_radius_drag: false,
            // 两个分组默认都展开：这一页一共四项，全展开才 6 行，
            // 藏着反而让用户多点一次（外观页的三级圆角树默认展开也是这个理由）
            settings_interact_sel: Some("selbar.w"),
            settings_interact_open: ["selbar", "anim"].into_iter().collect(),
            hotkey_capture: None,
            openwith_preview: None,
            menu_preview_items: None,
            settings_hover_menu: None,
            settings_hover_find: false,
            settings_prev_hover: None,
            settings_prev_scroll: 0,
            preview_note_at: 0,
            preview_menu: false,
            settings_ow_found: false,
            fast_mode: false,
            xfer: None,
            io_tx,
            io_rx,
            io_pending: 0,
            exit_to_tray: false,
            hovering_close: false,
            hover_chrome: 0,
            hover_bulk: false,
            plugins: zenfile_core::plugin::PluginHost::new(
                std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ZenFile.exe")),
            ),
            drives: HashMap::new(),
            drives_stamp: None,
            sidebar_expanded: HashSet::new(),
            sidebar_children: HashMap::new(),
            sidebar_hover: None,
            hover_row: None,
            hover_pane: None,
            hover_sidebar_collapse: false,
            hover_nav: 0,
            hover_header: None,
            cut_set: HashSet::new(),
            sidebar_scroll: 0,
            anim_timer_on: std::cell::Cell::new(false),
            anim_last: std::cell::Cell::new(0),
            addr_anim: None,
            cap_anim: HashMap::new(),
            cap_fired: HashSet::new(),
            sb_active_y: None,
            sb_active_target: None,
            sb_hover_y: None,
            sb_hover_target: None,
            sb_scroll_target: None,
            sb_last_active: None,
            icon_zoom: HashMap::new(),
            icon_zoom_tgt: HashMap::new(),
            icon_bounce: HashMap::new(),
            icon_active: HashSet::new(),
            icon_prev_active: HashSet::new(),
            display_names: HashMap::new(),
            focus_visible: false,
            hover_tab: None,
            hover_tab_close: None,
            hover_new_tab: false,
            hover_view_btn: 0,
            pending_select: None,
            pending_rename: false,
            rename_stem_only: false,
            context_targets: None,
            last_click: None,
            recycle_items: Vec::new(),
            recycle_sel: HashSet::new(),
            recycle_rx: None,
            recycle_gen: 0,
            generation_seq: 0,
            recycle_sort: (2, true),
            hover_recycle: None,
            hover_recycle_btn: 0,
            settings_hover: None,
            settings_tab: 0,
            settings_ctx_kind: 0,
            settings_hover_tab: None,
            settings_hover_card: None,
            settings_drag: None,
            ui_settings: crate::settings::Settings::load(),
            hover_caption: 0,
            pressed_caption: 0,
            mouse_tracked: false,
            addr_editing: false,
            // 第75轮：初值取 `Default::default()`（空串 / 全零 / `visible=false`）。
            // 它与真实状态几乎必然不相等，所以窗口刚建出来、`hwnd_search` 刚拿到
            // 句柄的第一帧就会把两个子 EDIT 各失效一次 —— 正是我们要的"先画上"。
            child_search_key: Default::default(),
            child_address_key: Default::default(),
            addr_hover: None,
            index: None,
            index_root: None,
            index_state: IndexState::Idle,
            index_pending: false,
            search_ticks: 0,
            search_dirty: false,
            search_hits: 0,
            search_ms: 0.0,
            index_rx: None,
            index_cancel: None,
            drag_split: None,
            hover_split: None,
            hover_pane_div: None,
            pane_div_drag: None,
            drag: None,
            drop_target: None,
            view_overrides,
            scroll_memory: HashMap::new(),
        }
    }

    /// 把配置里的"导航窗格宽度"落到实际量度上。
    ///
    /// `Metrics` 是按 DPI 算出来的一份量度，导航窗格宽度却是用户能拖的东西，
    /// 所以两者必须分开：DPI 一变宽度要跟着重新乘一次（否则 150% 缩放下
    /// 拖好的宽度会被 96 DPI 的默认值顶掉）。
    pub fn sync_metrics(&mut self) {
        let s = self.metrics.scale;
        let dip = self.cfg.ui.sidebar_width.max(SIDEBAR_W_MIN as u32) as f32;
        self.metrics.sidebar_w =
            ((dip * s).round() as i32).min((SIDEBAR_W_MAX as f32 * s).round() as i32);
    }

    /// 详细信息各列的当前宽度（DIP）。拖表头时直接改这里。
    pub fn col_widths(&self) -> [i32; 4] {
        let v = &self.cfg.files.view;
        [
            v.col_name_w as i32,
            v.col_date_w as i32,
            v.col_kind_w as i32,
            v.col_size_w as i32,
        ]
    }

    // —— 列布局（第33轮：列头右键自定义，每文件夹各一套）——

    /// 当前目录的**显示列序列**（已按名称列在首位规范化）。
    ///
    /// 三级优先：Explorer bag（资源管理器真的去过这个目录）→ ZenFile 每目录
    /// 配置 → 默认四列。bag 优先是为了满足"与资源管理器同步"，配置兜底是
    /// 因为 bag 只在资源管理器去过某目录时才存在。
    pub fn visible_cols(&self) -> Vec<String> {
        // 搜索态强制多一个「位置」列，插在名称后面 —— 搜索结果需要知道在哪
        // （资源管理器搜索结果视图同样如此，且此时"选择列"是禁用的）。
        if self.tab().search_root.is_some() {
            let mut v = self.saved_visible_cols();
            if !v.iter().any(|x| x == zenfile_core::columns::C_PATH) {
                v.insert(1, zenfile_core::columns::C_PATH.to_string());
            }
            return v;
        }
        self.saved_visible_cols()
    }

    /// 磁盘上的列序列（不含搜索态注入的「位置」列）。
    fn saved_visible_cols(&self) -> Vec<String> {
        if self.tab().home || self.tab().settings || self.tab().recycle {
            return zenfile_core::columns::default_visible();
        }
        let key = norm_key(&self.tab().path);
        if !key.is_empty() {
            if let Some(v) = crate::sysview::system_columns(&self.tab().path) {
                return zenfile_core::columns::normalize_visible(&v);
            }
            if let Some(l) = self.cfg.files.view.col_layouts.get(&key) {
                if !l.visible.is_empty() {
                    return zenfile_core::columns::normalize_visible(&l.visible);
                }
            }
        }
        zenfile_core::columns::default_visible()
    }

    /// 某列在当前目录的宽度（DIP）。
    pub fn col_width(&self, id: &str) -> i32 {
        // 先问磁盘上那份 bag —— 资源管理器拖过的列宽要能直接生效
        if !self.tab().home && !self.tab().settings && !self.tab().recycle {
            let key = norm_key(&self.tab().path);
            if !key.is_empty() {
                if let Some(w) = crate::sysview::system_col_width(&self.tab().path, id) {
                    return w as i32;
                }
                if let Some(l) = self.cfg.files.view.col_layouts.get(&key) {
                    if let Some(w) = l.widths.get(id) {
                        return *w as i32;
                    }
                }
            }
        }
        // 旧的三段固定宽度（用户在这轮之前拖过的）映射到同名列，
        // 否则升级后列宽会全体跳回默认值 —— 那是"我的设置被重置了"。
        let legacy = match id {
            zenfile_core::columns::C_NAME => self.cfg.files.view.col_name_w,
            zenfile_core::columns::C_DATE_MOD => self.cfg.files.view.col_date_w,
            zenfile_core::columns::C_TYPE => self.cfg.files.view.col_kind_w,
            zenfile_core::columns::C_SIZE => self.cfg.files.view.col_size_w,
            _ => 0,
        };
        if legacy > 0 {
            return legacy as i32;
        }
        zenfile_core::columns::default_width(id) as i32
    }

    /// 改一列的宽度：写 bag（让资源管理器也认）+ 写本目录配置（兜底）。
    pub fn set_col_width(&mut self, index: usize, w: i32) {
        let w = (w.max(COL_W_MIN).min(COL_W_MAX)) as u32;
        let v = &mut self.cfg.files.view;
        match index {
            0 => v.col_name_w = w,
            1 => v.col_date_w = w,
            2 => v.col_kind_w = w,
            _ => v.col_size_w = w,
        }
    }

    /// 新的一列宽度（第33轮按列 id 存）。同时写 bag 与本目录配置。
    pub fn set_col_width_by_id(&mut self, id: &str, w: i32) {
        let w = (w.max(zenfile_core::columns::W_MIN as i32) as u32).min(zenfile_core::columns::W_MAX);
        if self.tab().home || self.tab().settings || self.tab().recycle {
            return;
        }
        let path = self.tab().path.clone();
        crate::sysview::write_system_col_width(&path, id, w);
        let key = norm_key(&path);
        let entry = self
            .cfg
            .files
            .view
            .col_layouts
            .entry(key)
            .or_default();
        entry.widths.insert(id.to_string(), w);
        // 落盘：深合并（不覆盖用户在别的位置的设置）
        let patch = serde_json::json!({
            "files": { "view": { "col_layouts": {
                norm_key(&path): { "widths": { id: w } }
            }}}
        });
        let _ = zenfile_core::config::Config::save_user_patch(&patch);
        self.invalidate();
    }

    /// 设置某列在当前目录是否可见（名称列不可隐藏，与资源管理器一致）。
    pub fn set_col_visible(&mut self, id: &str, visible: bool) {
        if id == zenfile_core::columns::C_NAME || self.tab().home {
            return;
        }
        let mut v = self.saved_visible_cols();
        v.retain(|x| x != id);
        if visible {
            // 追加到末尾（保持"名称在首位、其余按添加顺序"）
            if !v.iter().any(|x| x == id) {
                v.push(id.to_string());
            }
        }
        v = zenfile_core::columns::normalize_visible(&v);
        self.apply_visible_cols(&v);
    }

    /// 应用一整套列序列：写 bag + 写本目录配置 + 重画。
    pub fn apply_visible_cols(&mut self, visible: &[String]) {
        let v = zenfile_core::columns::normalize_visible(visible);
        let path = self.tab().path.clone();
        if !self.tab().home && !self.tab().settings && !self.tab().recycle {
            crate::sysview::write_system_columns(&path, &v);
            let key = norm_key(&path);
            self.cfg
                .files
                .view
                .col_layouts
                .entry(key.clone())
                .or_default()
                .visible = v.clone();
            let patch = serde_json::json!({
                "files": { "view": { "col_layouts": { key.clone(): { "visible": v } } } }
            });
            let _ = zenfile_core::config::Config::save_user_patch(&patch);
        }
        self.invalidate();
    }

    /// 某一列当前是否可见（"选择列"菜单打勾用）。
    pub fn is_col_visible(&self, id: &str) -> bool {
        self.saved_visible_cols().iter().any(|x| x == id)
    }

    /// 「将列调整为合适的大小」：按当前**实际最长内容**算宽度。
    ///
    /// 只看可见行（最多采样 200 行）—— 全量扫 9 万个条目来量文字宽度，
    /// 打开目录要多等好几秒，这个代价不值当。资源管理器也是按可见范围算的。
    pub fn autofit_cols(&mut self) {
        let cols = self.visible_cols();
        let show_ext = self.cfg.files.view.show_extensions;
        let take = self.tab().visible.len().min(200);
        for (i, id) in cols.iter().enumerate() {
            if id == zenfile_core::columns::C_SIZE {
                // 大小列固定 80：内容全是 "12.3 MB"，按它算也还是那个宽度
                self.set_col_width_by_id(id, 80);
                continue;
            }
            // 表头文字本身是下限
            let mut maxw = text_width_units(zenfile_core::columns::label_of(id));
            for k in 0..take {
                let Some(ei) = self.tab().visible.get(k).and_then(|x| self.tab().entries.get(*x))
                else {
                    continue;
                };
                let s = match zenfile_core::columns::cell(id, ei, show_ext) {
                    Some(s) => s,
                    None => continue,
                };
                let wpx = text_width_units(&s) + 12; // 左右各留 6px
                if wpx > maxw {
                    maxw = wpx;
                }
            }
            // 名称列额外给图标与缩进留位
            if id == zenfile_core::columns::C_NAME {
                maxw += 40;
            }
            let w = maxw
                .max(zenfile_core::columns::W_MIN as i32)
                .min(zenfile_core::columns::W_MAX as i32);
            self.set_col_width_by_id(id, w);
            let _ = i;
            let _ = i;
        }
        self.invalidate();
    }

    pub fn tab(&self) -> &Tab {
        let p = &self.panes[self.focus_pane];
        &p.tabs[p.active]
    }

    pub fn tab_mut(&mut self) -> &mut Tab {
        let p = &mut self.panes[self.focus_pane];
        let i = p.active;
        &mut p.tabs[i]
    }

    /// 焦点窗格（画它的标签条、决定它是不是"当前"时用）。
    pub fn pane(&self) -> &Pane {
        &self.panes[self.focus_pane]
    }

    pub fn pane_mut(&mut self) -> &mut Pane {
        let i = self.focus_pane;
        &mut self.panes[i]
    }

    /// 焦点窗格的标签数。**不能直接读 `panes[..].tabs.len()`**：
    /// 绘制每个窗格时 `focus_pane` 会被临时改掉，这里跟着变才是对的。
    pub fn tab_count(&self) -> usize {
        self.panes[self.focus_pane].tabs.len()
    }

    /// 焦点窗格第 i 个标签。
    pub fn tab_at(&self, i: usize) -> Option<&Tab> {
        self.panes[self.focus_pane].tabs.get(i)
    }

    /// 当前是不是分栏（>1 个窗格）。分栏时窗口顶部那条标签栏要让位给
    /// 每个窗格自己的标签条，否则同一个标签会画两遍。
    pub fn split(&self) -> bool {
        self.panes.len() > 1
    }

    /// 分栏时顶部标签要显示成「A丨B丨C」：把所有窗格当前目录名用 丨 串起来
    /// （用户要求：一个标签内多个窗口时标签名改成 xx丨xx，2 个以上同样）。
    ///
    /// 不分栏（或只剩一个窗格）返回 None，标签照旧用普通单目录名。
    pub fn split_title(&self) -> Option<String> {
        if !self.split() {
            return None;
        }
        let names: Vec<String> = self
            .panes
            .iter()
            .filter_map(|p| p.tabs.get(p.active))
            .map(|t| t.title.clone())
            .collect();
        if names.len() >= 2 {
            Some(names.join("丨"))
        } else {
            None
        }
    }

    /// 把一个标签"贴"进某个窗格区域。
    ///
    /// `zone` 决定新窗格放在哪一侧；已有窗格被压缩到剩下那一侧。
    /// 返回 true 表示真的分出去了。
    pub fn dock_tab(&mut self, src_pane: usize, src_tab: usize, zone: DockZone) -> bool {
        if src_pane >= self.panes.len() || src_tab >= self.panes[src_pane].tabs.len() {
            return false;
        }
        // 角贴要按"所有窗格合起来"的整块来四等分，必须**在 remove 之前**量好：
        // 拖空的源窗格一被收掉，它的地盘就从 panes 里消失了，那时再算就只剩
        // 目标窗格那一块（实测右半边整块变背景色，点上去没有任何窗格响应）。
        let block = self.panes_block();
        let dst = self.focus_pane.min(self.panes.len() - 1);
        // 一个窗格里就这一个标签还往它自己身上贴 = 什么都没发生
        if src_pane == dst && self.panes[dst].tabs.len() <= 1 && self.panes.len() == 1 {
            return false;
        }
        let t = self.panes[src_pane].tabs.remove(src_tab);
        self.clamp_pane_active(src_pane);
        // 源窗格被拖空了就收掉。注意这会让它后面的窗格下标**整体前移一位**，
        // 所以 `dst` 必须跟着减 —— 不减就会贴到隔壁窗格身上（"贴左边却分到了右边"）。
        let mut dst = dst;
        if self.panes[src_pane].tabs.is_empty() {
            self.panes.remove(src_pane);
            if dst > src_pane {
                dst -= 1;
            }
        }
        match zone {
            DockZone::Center => {
                // 中央 = 合进目标窗格，不做分栏。贴边窗格最多一个标签，
                // 所以这里不是 push 而是**替换**目标窗格那一个。
                let p = &mut self.panes[dst];
                p.tabs.clear();
                p.tabs.push(t);
                p.active = 0;
                self.focus_pane = dst;
            }
            DockZone::Left | DockZone::Right | DockZone::Top | DockZone::Bottom => {
                // **窗口级 1/2 贴靠**（第27轮）：把整个窗格块沿 zone 二分 ——
                // zone 侧半给新标签，对侧半 = 现有窗格按原比例**压缩重排**
                //（线性映射，相对布局不变）。
                //
                // 早先只对焦点窗格二分：源窗格被拖空收掉后，它腾出的地盘没有
                // 任何窗格接收 —— 那一带 pane_index_at=None，点上去全无反应
                //（用户 #2"贴完两边都操作不了"，日志实锤缺 0..0.25 无主）。
                // 全局二分后 block 恒被铺满：不重叠、不缺口。
                // 1/4 四角贴靠已砍掉（用户要求只做 1/2），这里全是 1/2。
                let (keep, give) = split_rect(block, zone);
                for p in self.panes.iter_mut() {
                    p.rect = remap_rect(p.rect, block, keep);
                }
                self.panes.push(Pane { tabs: vec![t], active: 0, rect: give });
                self.focus_pane = self.panes.len() - 1;
            }
            _ => self.dock_quarter(dst, t, zone, block),
        }
        self.ensure_pane();
        true
    }

    /// 所有窗格合起来的外接矩形（归一化坐标）。四角贴靠按这一整块四等分。
    ///
    /// 不能只看"鼠标底下那一格"：跨窗格拖时源窗格会被 remove，它的地盘就没了主人
    /// —— 那一块会变成点不动的死区（实测左右分栏后把右边的标签拖到左上角，
    /// 右半边 4x4 网格墨迹全 0）。
    fn panes_block(&self) -> RectF {
        let mut x0 = f32::MAX;
        let mut y0 = f32::MAX;
        let mut x1 = f32::MIN;
        let mut y1 = f32::MIN;
        for p in &self.panes {
            x0 = x0.min(p.rect.x);
            y0 = y0.min(p.rect.y);
            x1 = x1.max(p.rect.x + p.rect.w);
            y1 = y1.max(p.rect.y + p.rect.h);
        }
        if x1 > x0 && y1 > y0 {
            RectF { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
        } else {
            RectF::FULL
        }
    }

    /// 四角贴靠：整块切成 **2x2 四等分**，四格铺满、一格不空。
    ///
    /// 之前是"新窗格 1/4 + 旧窗格缩到对侧 1/2"，于是剩下一个 1/4 谁都不属于 ——
    /// 点是点不中的（命中测试找不到窗格），看起来就是"一边用不了点不动"。
    /// 四等分之后每格都是一个真窗格：拖进来的那个落在松手的那一格，
    /// 原来那个挪到**对角**（离新标签最远），其余两格沿用原来正在看的目录
    /// （VS Code 的 2x2 网格也是这个行为）。
    fn dock_quarter(&mut self, dst: usize, dragged: Tab, zone: DockZone, block: RectF) {
        let r = block;
        let (hw, hh) = (r.w * 0.5, r.h * 0.5);
        let quads = [
            RectF { x: r.x, y: r.y, w: hw, h: hh },                 // 0 左上
            RectF { x: r.x + hw, y: r.y, w: hw, h: hh },            // 1 右上
            RectF { x: r.x, y: r.y + hh, w: hw, h: hh },            // 2 左下
            RectF { x: r.x + hw, y: r.y + hh, w: hw, h: hh },       // 3 右下
        ];
        let want = match zone {
            DockZone::TopLeft => 0usize,
            DockZone::TopRight => 1,
            DockZone::BottomLeft => 2,
            _ => 3,
        };
        let keep = 3 - want; // 对角
        let target = std::mem::take(&mut self.panes[dst].tabs);
        let target_active = self.panes[dst].active;
        let fill = target.get(target_active).cloned();
        // 其余窗格的标签整排整排地摊进剩下两格（按窗格顺序，保持视觉顺序不乱）
        let mut spare: Vec<Vec<Tab>> = self
            .panes
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != dst)
            .map(|(_, p)| p.tabs.clone())
            .filter(|t| !t.is_empty())
            .collect();
        let mut built: Vec<Pane> = Vec::new();
        let mut focus = 0usize;
        for i in 0..4usize {
            let (tabs, active) = if i == keep {
                let a = target_active.min(target.len().saturating_sub(1));
                (target.clone(), a)
            } else if i == want {
                (vec![dragged.clone()], 0usize)
            } else {
                match spare.pop() {
                    Some(t) if !t.is_empty() => (t, 0usize),
                    _ => match &fill {
                        Some(f) => (vec![f.clone()], 0usize),
                        None => (Vec::new(), 0usize),
                    },
                }
            };
            if tabs.is_empty() {
                continue;
            }
            if i == want {
                focus = built.len();
            }
            built.push(Pane { tabs, active, rect: quads[i] });
        }
        // 格子不够时剩下的标签并进对角那一格，绝不能静悄悄丢掉
        for rest in spare {
            if let Some(p) = built.get_mut(keep) {
                p.tabs.extend(rest);
            }
        }
        self.panes = built;
        self.focus_pane = focus.min(self.panes.len().saturating_sub(1));
    }

    /// 直接把一个目录贴成一个新窗格（把**文件夹**拖进内容区时走这里）。
    ///
    /// 借 `dock_tab` 的落位逻辑：先把标签塞进当前窗格，再当成"被拖的那个"贴出去。
    pub fn open_split(&mut self, path: PathBuf, zone: DockZone) {
        if zone == DockZone::Center {
            self.open_tab(path);
            return;
        }
        let dst = self.focus_pane.min(self.panes.len().saturating_sub(1));
        if self.panes.is_empty() {
            self.panes.push(Pane::default());
        }
        let t = Tab::new(path);
        self.panes[dst].tabs.push(t);
        let idx = self.panes[dst].tabs.len() - 1;
        self.dock_tab(dst, idx, zone);
        // 焦点已经落在新建的那个窗格上，这里把它的条目读出来
        self.reload();
    }

    /// 分栏时窗口标题栏上那一行"目录A 丨 目录B"。
    ///
    /// 按**视觉顺序**排（先上后下、同行先左后右），读起来和屏幕上看到的一致。
    pub fn split_caption(&self) -> String {
        if self.panes.len() <= 1 {
            return String::new();
        }
        let mut order: Vec<usize> = (0..self.panes.len()).collect();
        order.sort_by(|&a, &b| {
            let ra = self.panes[a].rect;
            let rb = self.panes[b].rect;
            // 上下差异超过半个窗格高才算"不同行"，避免浮点误差把同一行拆开
            let ay = (ra.y * 100.0) as i32;
            let by = (rb.y * 100.0) as i32;
            if (ay - by).abs() > 5 {
                ay.cmp(&by)
            } else {
                ((ra.x * 100.0) as i32).cmp(&((rb.x * 100.0) as i32))
            }
        });
        let mut out: Vec<String> = Vec::new();
        for &i in order.iter() {
            let p = &self.panes[i];
            let Some(t) = p.tabs.get(p.active) else { continue };
            let name = if t.title.is_empty() {
                t.path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| t.path.to_string_lossy().to_string())
            } else {
                t.title.clone()
            };
            // **按名字去重**：四等分会造出三个"沿用原来目录"的窗格，
            // 不去重的话标题就变成「A 丨 A 丨 B 丨 A」这种谁也读不懂的东西。
            if !out.iter().any(|n| *n == name) {
                out.push(name);
            }
        }
        out.join(" 丨 ")
    }

    /// 算一次「打开方式」的真实候选程序（供设置页那一块显示）。
    ///
    /// 取当前目录里第一个**文件**的扩展名去问；一个文件都没有就退回 `.txt` ——
    /// 空目录里也要能看到"打开方式长什么样"，不然那一块永远是空的。
    /// 切进「右键菜单」页时算一次：① 实时菜单要用的样本文件；② 「打开方式」
    /// 的候选程序（按该样本的真实扩展名去问 Shell）。
    ///
    /// 两样都只算一次存起来 —— `openwith::candidates()` 要翻注册表和 Shell，
    /// 每帧重算会把设置页拖得很慢。
    pub fn ensure_openwith_preview(&mut self) {
        if self.openwith_preview.is_some() && self.menu_preview_items.is_some() {
            return;
        }
        let kind = MenuKind::from_u8(self.settings_ctx_kind);
        let sample = self.menu_preview_sample_for(kind);
        // 目录类（文件夹/背景/多选/驱动器）没有扩展名：候选按"谁能吃目录"找。
        // 文件类按样本的真实扩展名找。
        let list = if kind == MenuKind::File {
            let ext = sample
                .first()
                .and_then(|p| p.extension().map(|x| x.to_string_lossy().to_string()))
                .unwrap_or_else(|| "txt".to_string());
            crate::openwith::candidates(&ext)
        } else {
            crate::openwith::candidates_dir()
        }
        .into_iter()
        .map(|c| (c.name, c.icon.0 as isize))
        .collect::<Vec<_>>();
        self.openwith_preview = Some(list);
        self.menu_preview_items = Some(sample);
    }

    /// 给设置页按**正在编辑的那一类**挑样本。
    ///
    /// 目录背景要的是"没有选中"（菜单里才有新建/查看/排序），多选要两个样本，
    /// 驱动器要一个盘符根 —— 照样本渲染出来的菜单才跟真窗口对得上。
    pub fn menu_preview_sample_for(&self, kind: MenuKind) -> Vec<PathBuf> {
        let dir = self
            .panes
            .iter()
            .flat_map(|p| p.tabs.iter())
            .find(|t| !t.settings && !t.recycle && !t.home)
            .map(|t| t.path.clone());
        let dir = dir.unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("C:\\"))
        });
        match kind {
            MenuKind::Background => Vec::new(),
            MenuKind::Multi => {
                let mut v = self.menu_preview_sample();
                if v.len() < 2 {
                    if let Ok(rd) = std::fs::read_dir(&dir) {
                        v.extend(rd.filter_map(|e| e.ok()).take(2).map(|e| e.path()));
                    }
                }
                v.truncate(2);
                v
            }
            MenuKind::Drive => {
                let s = dir.to_string_lossy().to_string();
                let root = if s.len() >= 2 {
                    format!("{}:\\", s.chars().next().unwrap_or('C'))
                } else {
                    "C:\\".to_string()
                };
                vec![PathBuf::from(root)]
            }
            MenuKind::Folder => {
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    if let Some(p) = rd.filter_map(|e| e.ok()).find(|e| e.path().is_dir()) {
                        return vec![p.path()];
                    }
                }
                vec![dir.clone()]
            }
            MenuKind::File => self.menu_preview_sample(),
        }
    }

    /// 给设置页的实时菜单挑一个"右键它"的样本。
    ///
    /// 优先级：① 当前标签真的选中的东西；② 当前目录里第一个文件；
    /// ③ 设置页是虚拟标签，就退回**上一个真实标签**的目录里第一个文件；
    /// ④ 都没有才用自己这个 exe（一定存在，于是「打开方式」那行也在）。
    fn menu_preview_sample(&self) -> Vec<PathBuf> {
        if !self.tab().selected.is_empty() {
            return self.tab().selected_paths();
        }
        if let Some(e) = self.tab().entries.iter().find(|e| !e.is_dir()) {
            return vec![e.path.clone()];
        }
        let dir = self
            .panes
            .iter()
            .flat_map(|p| p.tabs.iter())
            .find(|t| !t.settings && !t.recycle && !t.home)
            .map(|t| t.path.clone());
        if let Some(d) = dir {
            if let Ok(rd) = std::fs::read_dir(&d) {
                for ent in rd.flatten() {
                    let p = ent.path();
                    if p.is_file() {
                        return vec![p];
                    }
                }
            }
        }
        std::env::current_exe().map(|p| vec![p]).unwrap_or_default()
    }

    /// 某个圆角项当前的值（DIP）。没在配置里出现过就用树上的默认值 ——
    /// 于是"加了新圆角项"不需要迁移旧配置。
    /// 某个圆角项当前的值（DIP）。
    ///
    /// 取值顺序是**从细到粗**：三级本项 → 它所属的二级页面 → 全局 → 出厂默认。
    /// 于是「全局圆角」真的是全局、"侧栏圆角"真的是整个侧栏的基准，某个具体
    /// 控件单独调过才用它自己的值。
    pub fn radius_of(&self, id: &str) -> u32 {
        let r = &self.cfg.ui.radius;
        if let Some(v) = r.get(id).copied() {
            return v;
        }
        let max = crate::settings::radius_max(id);
        // 「子项没单独设 → 用它所在分组的值」。这一条**本来就实现了**
        // （第35轮之前就有），所以「标题栏圆角」这类分组并非完全空壳 ——
        // 前提是子项**自己没有**被写进配置。
        if let Some(page) = id.split('.').next() {
            if page != id {
                if let Some(v) = r.get(page).copied() {
                    return v.min(max);
                }
            }
        }
        if id != "global" {
            if let Some(g) = r.get("global").copied() {
                return g.min(max);
            }
        }
        crate::settings::radius_default(id)
    }

    /// 「分组节点」当前的实际取值（第35轮）。
    ///
    /// 一个分组（标题栏圆角 / 地址栏圆角 / …）**不对应界面上一块独立的底板**，
    /// 它是"本组里那些圆角块的共同默认值"。所以用户选中一个分组时：
    ///   - 如果本组下**所有**叶子都还没单独设过值 → 改分组就等于批量改它们，
    ///     此时必须**把值写进每个叶子**（否则叶子下次 `radius_of` 又回落到
    ////   默认值，改动看着"没生效"）；
    ///   - 如果已有叶子单独设过 → 只改那些"跟随分组"的，保留用户自己的设定。
    ///
    /// 这才是"分组节点不是空壳"的正确实现方式。返回受影响的叶子 id 列表。
    pub fn radius_group_children(&self, group: &str) -> Vec<String> {
        crate::settings::radius_leaves_of(group)
    }

    /// 改一个圆角项并写回用户层。
    pub fn set_radius(&mut self, id: &str, v: u32) {
        let v = v.min(crate::settings::radius_max(id));
        self.cfg.ui.radius.insert(id.to_string(), v);
        let mut patch = serde_json::json!({ "ui": { "radius": { id: v } } });
        // **分组节点要落到它的每一个叶子上**（第35轮）。
        //
        // 为什么必须写叶子：`radius_of` 的回落链是「叶子自己的值 → 分组的值
        // → global → 硬编码默认」，所以只写分组时叶子确实会跟着变 —— 但那只在
        // 叶子**还没被写进配置**时成立。用户先前拖过某个叶子（比如把
        // title.tab 单独调大），那个值就永久盖住了分组，以后再调分组对这一项
        // 就完全没反应 —— 树里明明有这一项，界面却不动，正是"空壳"。
        //
        // 批量写叶子 = 语义明确："我现在把这一组统一下设成 v"，用户的单独
        // 设定被这一组的新值取代（这正是他此刻的操作意图）。
        if crate::settings::radius_has_kids(id) {
            for k in crate::settings::radius_leaves_of(id) {
                let kv = v.min(crate::settings::radius_max(&k));
                self.cfg.ui.radius.insert(k.clone(), kv);
            }
            let mut m = serde_json::Map::new();
            for k in crate::settings::radius_leaves_of(id) {
                let kv = self.cfg.ui.radius.get(&k).copied().unwrap_or(v);
                m.insert(k, serde_json::Value::from(kv));
            }
            patch["ui"]["radius"] = serde_json::Value::Object(m);
        }
        let _ = zenfile_core::config::Config::save_user_patch(&patch);
    }

    /// 改「选中行首竖线」的形状并写回用户层（设置-交互页）。
    ///
    /// 每次改都把整个 sel_bar 对象深合并进去 —— save_user_patch 是 deep_merge，
    /// 单字段 patch 也行，整体写更不容易漏状态。
    pub fn set_sel_bar(&mut self, f: impl FnOnce(&mut zenfile_core::config::SelBarConfig)) {
        f(&mut self.cfg.ui.sel_bar);
        let _ = zenfile_core::config::Config::save_user_patch(&serde_json::json!({
            "ui": { "sel_bar": self.cfg.ui.sel_bar }
        }));
    }

    /// 改「动画」开关并写回用户层（设置-交互页）。
    pub fn set_anim(&mut self, f: impl FnOnce(&mut zenfile_core::config::AnimConfig)) {
        f(&mut self.cfg.ui.anim);
        let _ = zenfile_core::config::Config::save_user_patch(&serde_json::json!({
            "ui": { "anim": self.cfg.ui.anim }
        }));
    }

    // ------------------------------------------------------------ 动画驱动
    //
    // 一个 16ms 的定时器（`crate::TIMER_ANIM`）推着所有过渡动画走，**没有
    // 动画在跑的时候它是被 Kill 掉的**（`sync_anim_timer`）—— 常驻一个 60Hz
    // 定时器只为几段 300ms 的过渡，白白阻止系统降频。

    /// 起一段地址栏动画。`prev` 只在退出（kind=1）时用：画的是切换前的面包屑。
    pub fn start_addr_anim(&mut self, kind: u8, prev: Vec<Crumb>) {
        if !self.cfg.ui.anim.address {
            return;
        }
        let start = unsafe { windows::Win32::System::SystemInformation::GetTickCount64() };
        self.addr_anim = Some(AddrAnim { kind, start, prev });
        self.sync_anim_timer();
    }

    /// 记一笔"这个标题栏按键刚被划到（或刚被划开）"，让它弹一下。
    ///
    /// **"只弹一次"靠 `cap_fired` 这张表保证**，不是靠 `cap_anim` 本身：
    /// 动画播完 420ms 就从 `cap_anim` 出表，而此时鼠标多半还停在那个键上 ——
    /// 下一帧 `on=true` 又会插一条新记录，于是动画**无限循环**下去，定时器
    /// 也永远 Kill 不掉。所以记过的键要进 `cap_fired`，只有"移开又划回来"
    /// （`on` 先 false 后 true）才允许重播。这与 `icon_bounce` 靠
    /// `icon_active`/`icon_prev_active` 判"刚变成"是同一个思路，但用集合
    /// 更直接：这套是"同一时刻只有一个键可能悬停"，不需要帧间对比。
    ///
    /// 开关关着时**连状态都不进**：`cap_fired` 与 `cap_anim` 一起清空，
    /// 否则会有"动画已关但上一次记录还挂着"导致定时器空转。
    pub fn note_cap_hover(&mut self, which: u8, on: bool) {
        if !self.cfg.ui.anim.caption {
            self.cap_anim.remove(&which);
            self.cap_fired.remove(&which);
            return;
        }
        if on {
            if self.cap_fired.insert(which) {
                self.cap_anim.insert(
                    which,
                    CapAnim { start: unsafe { windows::Win32::System::SystemInformation::GetTickCount64() } },
                );
            }
        } else {
            // 移开鼠标：动画立刻作废（不回弹、不留残影），并允许下次重播。
            // 不作废的话，鼠标快速划过时每个键都会留一段没播完的记录，
            // 画面上是一串互相打架的形变。
            self.cap_anim.remove(&which);
            self.cap_fired.remove(&which);
        }
    }

    /// 某个标题栏按键的动画相位：`0`=静止（默认），`1`=峰值。
    ///
    /// 用 `sin(π·t)` 而不是线性缓动：起手弹到最大再落回默认，中间不需要
    /// 回弹震荡 —— 用户要的是"弹一下"，一次就够，反复震荡在这么小的符号上
    /// 看久了只会晕。`t<0`（还没起）或 `t>1`（已结束）都夹到 0。
    pub fn cap_phase(&self, which: u8) -> f32 {
        if !self.cfg.ui.anim.caption {
            return 0.0;
        }
        let Some(a) = self.cap_anim.get(&which) else {
            return 0.0;
        };
        let now = unsafe { windows::Win32::System::SystemInformation::GetTickCount64() };
        let t = now.saturating_sub(a.start) as f32 / ANIM_CAPTION_MS as f32;
        if !(0.0..=1.0).contains(&t) {
            return 0.0;
        }
        (std::f32::consts::PI * t).sin()
    }

    /// 每帧开始调用一次（paint 入口）：把"这一帧悬停/选中了谁"滚成"上一帧"，
    /// 用来判定"是不是刚变成这个状态"（刚变才弹一下）。
    pub fn begin_anim_frame(&mut self) {
        std::mem::swap(&mut self.icon_active, &mut self.icon_prev_active);
        self.icon_active.clear();
        self.icon_zoom_tgt.clear();
    }

    /// 还有没有动画在跑（决定定时器要不要继续挂着）。
    pub fn anim_active(&self) -> bool {
        if self.addr_anim.is_some()
            || self.sb_scroll_target.is_some()
            || !self.icon_bounce.is_empty()
            || !self.cap_anim.is_empty()
        {
            return true;
        }
        for (c, t) in [(self.sb_active_y, self.sb_active_target), (self.sb_hover_y, self.sb_hover_target)] {
            if let (Some(c), Some(t)) = (c, t) {
                if (t - c).abs() > 0.5 {
                    return true;
                }
            }
        }
        for (k, v) in &self.icon_zoom {
            if (self.icon_zoom_tgt.get(k).copied().unwrap_or(0.0) - *v).abs() > 0.002 {
                return true;
            }
        }
        for k in self.icon_zoom_tgt.keys() {
            if !self.icon_zoom.contains_key(k) {
                return true;
            }
        }
        false
    }

    /// 把"每帧靠近 p%"换算成**与帧率无关**的缓动系数。
    ///
    /// `rate` 是 60Hz 下每帧靠近的比例（如 0.28）。真实每帧间隔 `dt_ms` 可能
    /// 因为掉帧而变大，直接用固定比例会让动画在掉帧时变慢。改成指数衰减
    /// `f = 1 - e^(-k·dt)`，其中 `k = -ln(1-rate)/0.0167`，于是无论跑在 30Hz
    /// 还是 120Hz，动画在墙上时钟里都走同样长、同样顺。
    fn anim_ease(rate: f32, dt_ms: f32) -> f32 {
        // 60Hz 下与旧"每帧靠近 rate"等价的速率常数
        let k = (-(1.0 - rate as f64).ln() / 0.0167) as f32;
        let dt = (dt_ms / 1000.0).clamp(0.0, 0.05); // 钳到 50ms：极端掉帧也不瞬移
        1.0 - (-k * dt).exp()
    }

    /// 推进一帧动画。返回 true = 画面变了，调用方该重画。
    pub fn tick_anim(&mut self) -> bool {
        let now = unsafe { windows::Win32::System::SystemInformation::GetTickCount64() };
        // 帧间隔：首帧（anim_last 还没初始化）按 16ms 算，避免突跳。
        let last = self.anim_last.get();
        let dt = if last == 0 { 16.0 } else { (now - last) as f32 };
        self.anim_last.set(now);
        let mut dirty = false;

        // —— 地址栏：跑完就撤 ——
        if self.addr_anim.is_some() {
            let done = self
                .addr_anim
                .as_ref()
                .map(|a| {
                    let dur = if a.kind == 0 { ANIM_ADDR_IN_MS } else { ANIM_ADDR_OUT_MS };
                    now.saturating_sub(a.start) >= dur
                })
                .unwrap_or(false);
            if done {
                self.addr_anim = None;
            }
            dirty = true;
        }

        // —— 导航窗格：平滑滚动到目标行 ——
        if let Some(t) = self.sb_scroll_target {
            let cur = self.sidebar_scroll as f32;
            let d = t as f32 - cur;
            if d.abs() <= 0.5 {
                self.sidebar_scroll = t;
                self.sb_scroll_target = None;
            } else {
                // 与帧率无关的缓动：60Hz 下每帧靠近 24%，约 150ms 到位
                self.sidebar_scroll = (cur + d * Self::anim_ease(0.24, dt)).round() as i32;
            }
            dirty = true;
        }

        // —— 导航窗格：指示条 / 悬停条缓动到位 ——
        match (self.sb_active_y, self.sb_active_target) {
            (_, None) => self.sb_active_y = None,
            (None, Some(t)) => self.sb_active_y = Some(t),
            (Some(c), Some(t)) => {
                let d = t - c;
                if d.abs() <= 0.5 {
                    self.sb_active_y = Some(t);
                } else {
                    self.sb_active_y = Some(c + d * Self::anim_ease(0.28, dt));
                    dirty = true;
                }
            }
        }
        match (self.sb_hover_y, self.sb_hover_target) {
            (_, None) => self.sb_hover_y = None,
            (None, Some(t)) => self.sb_hover_y = Some(t),
            (Some(c), Some(t)) => {
                let d = t - c;
                if d.abs() <= 0.5 {
                    self.sb_hover_y = Some(t);
                } else {
                    self.sb_hover_y = Some(c + d * Self::anim_ease(0.4, dt));
                    dirty = true;
                }
            }
        }

        // —— 图标放大（大图标 / 中图标）——
        if !self.icon_zoom.is_empty() || !self.icon_zoom_tgt.is_empty() {
            let mut keys: Vec<String> = self.icon_zoom.keys().cloned().collect();
            for k in self.icon_zoom_tgt.keys() {
                if !self.icon_zoom.contains_key(k) {
                    keys.push(k.clone());
                }
            }
            let f = Self::anim_ease(0.28, dt);
            for k in keys {
                let cur = self.icon_zoom.get(&k).copied().unwrap_or(0.0);
                let tgt = self.icon_zoom_tgt.get(&k).copied().unwrap_or(0.0);
                if (cur - tgt).abs() <= 0.002 {
                    if tgt <= 0.0 {
                        self.icon_zoom.remove(&k);
                    } else {
                        self.icon_zoom.insert(k, tgt);
                    }
                } else {
                    self.icon_zoom.insert(k, cur + (tgt - cur) * f);
                    dirty = true;
                }
            }
        }

        // —— 图标回弹（详细信息 / 列表）：到点就出表 ——
        if !self.icon_bounce.is_empty() {
            self.icon_bounce.retain(|_, st| now.saturating_sub(*st) < ANIM_BOUNCE_MS);
            dirty = true;
        }

        // —— 标题栏按键：弹一下 → 到点归位 ——
        //
        // 到点就**从表里删掉**，不是把进度置 0：留着一条"已结束"的记录会让
        // `anim_active()` 永远为真，16ms 定时器就永远 Kill 不掉（第58轮
        // 取消选中放大卡住缩不回的同源坑）。
        if !self.cap_anim.is_empty() {
            self.cap_anim.retain(|_, a| now.saturating_sub(a.start) < ANIM_CAPTION_MS);
            dirty = true;
        }

        dirty
    }

    /// 按"还有没有动画"起 / 停定时器。状态没变就不动它 —— 每帧都 SetTimer
    /// 会把 16ms 周期一直往后推，动画直接卡住不动。
    pub fn sync_anim_timer(&mut self) {
        // 第68轮：不再用 `SetTimer`，改启停 `anim_clock` 的后台线程。
        //
        // 原因（`--bench-timer` 实测）：`SetTimer(16)` 的真实投递间隔是
        // **31ms**，整 2 倍。`WM_TIMER` 会被量化到系统时钟节拍，且优先级
        // 最低、忙时还被合并 —— 实测动画帧率只有 8~15fps，就是用户说的
        // "有种只有 15 帧的感觉"。改 `SetTimer` 的参数（16/10/8）都没用，
        // 它们全都被量化到同一个节拍。
        //
        // 后台线程用 waitable timer 按 15ms 主动 `PostMessage`，节奏由线程
        // 自己的等待决定，不经过节拍量化。
        let on = self.anim_active();
        if on == self.anim_timer_on.get() {
            return;
        }
        if on {
            // 刚挂上：把"上一帧时刻"对齐到此刻，首拍 dt 才不会是一段很长的
            // 空闲间隔（否则第一帧会瞬间跳一大步）。
            self.anim_last
                .set(unsafe { windows::Win32::System::SystemInformation::GetTickCount64() });
            crate::anim_clock::AnimClock::start(self.hwnd);
        } else {
            crate::anim_clock::AnimClock::stop();
        }
        self.anim_timer_on.set(on);
    }

    /// 扫一遍插件目录，把要跑的插件拉起来。
    ///
    /// **整个进程只做一次**：插件是进程级共享资源，而 `App` 是**每个窗口一个**
    /// （多开窗口时每个 `spawn_window` 都 new 一个）。每个窗口都 discover
    /// 的话，开 4 个窗口就拉起 4 份插件进程 —— 用户看到的是任务管理器里
    /// 同一插件出现四遍、互相抢同一根管道。
    ///
    /// 用 `static` 的一次性门守住。
    pub fn discover_plugins(&mut self) {
        use std::sync::OnceLock;
        static DONE: OnceLock<()> = OnceLock::new();
        if DONE.get().is_some() {
            return;
        }
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ZenFile.exe"));
        let dir = zenfile_core::plugin::plugins_dir_for(&exe);
        self.plugins.discover(&dir);
        zenfile_core::plugin::ensure_all(&mut self.plugins);
        let _ = DONE.set(());
        self.log_plugins("discover");
    }

    /// 把插件现状写进 `$TEMP\zenfile_plugins.log`。
    ///
    /// Windows subsystem 下 `eprintln!` / stderr 拿不到（`--selftest` 要靠
    /// `attach_console` 才能看见，普通启动连那个都没有）—— 排查插件问题时
    /// 唯一可靠的办法就是写文件。
    pub fn log_plugins(&self, tag: &str) {
        use std::io::Write;
        let path = std::env::temp_dir().join("zenfile_plugins.log");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let exe = std::env::current_exe().unwrap_or_default();
            let dir = zenfile_core::plugin::plugins_dir_for(&exe);
            let _ = writeln!(
                f,
                "[{}] exe={} plugins_dir={} exists={} 槽位数={}",
                tag,
                exe.display(),
                dir.display(),
                dir.exists(),
                self.plugins.slots().count()
            );
            for s in self.plugins.slots() {
                let _ = writeln!(
                    f,
                    "  - id={} name={:?} state={} pid={} wanted={} crashes={} err={:?}",
                    s.manifest.id,
                    s.manifest.name,
                    s.state.as_str(),
                    s.pid,
                    s.wanted,
                    s.crashes,
                    s.error
                );
            }
        }
    }

    /// 推进插件状态机。返回 true = 有插件状态变了，调用方该重画。
    ///
    /// 挂在消息循环的定时器上按帧调。**不能省这一步**：省了的话插件崩溃
    /// 之后宿主根本不知道，它会在面板上永远显示"运行中"。
    pub fn tick_plugins(&mut self) -> bool {
        let dirty = self.plugins.tick();
        if dirty {
            zenfile_core::plugin::ensure_all(&mut self.plugins);
        }
        dirty
    }

    /// 用户在面板上启 / 停一个插件。
    pub fn set_plugin_enabled(
        &mut self,
        id: &str,
        on: bool,
    ) -> zenfile_core::error::Result<()> {
        // 同步落盘，否则下次启动又回到配置里的旧值 —— 用户会以为开关没生效。
        self.cfg.modules.entry(id.to_string()).or_insert_with(|| {
            zenfile_core::config::ModuleConfig {
                enabled: on,
                autostart: false,
                settings: Default::default(),
            }
        }).enabled = on;
        let r = self.plugins.set_enabled(id, on);
        let _ = zenfile_core::config::Config::save_user_patch(&serde_json::json!({
            "modules": self.cfg.modules
        }));
        r
    }

    /// 当前标签下标越界了就夹回来（删标签、拖走标签之后都要）。
    fn clamp_pane_active(&mut self, i: usize) {
        if i >= self.panes.len() {
            return;
        }
        let p = &mut self.panes[i];
        if p.tabs.is_empty() {
            p.active = 0;
        } else if p.active >= p.tabs.len() {
            p.active = p.tabs.len() - 1;
        }
    }

    /// 收尾：一个窗格都没有就补一个；只剩一个就让它占满。
    fn ensure_pane(&mut self) {
        if self.panes.is_empty() {
            self.panes.push(Pane::default());
        }
        if self.focus_pane >= self.panes.len() {
            self.focus_pane = self.panes.len() - 1;
        }
        if self.panes.len() == 1 {
            self.panes[0].rect = RectF::FULL;
        }
    }

    /// 标签重排：把第 `from` 个挪到第 `to` 个位置，当前标签跟着它走。
    pub fn reorder_tab(&mut self, pane: usize, from: usize, to: usize) {
        if pane >= self.panes.len() {
            return;
        }
        let n = self.panes[pane].tabs.len();
        if from >= n || to > n {
            return;
        }
        let active = self.panes[pane].active;
        let t = self.panes[pane].tabs.remove(from);
        let to = to.min(self.panes[pane].tabs.len());
        self.panes[pane].tabs.insert(to, t);
        self.panes[pane].active = if active == from {
            to
        } else {
            let mut a = active;
            if from < active {
                a -= 1;
            }
            if to <= a {
                a += 1;
            }
            a.min(self.panes[pane].tabs.len().saturating_sub(1))
        };
        self.reload();
        self.invalidate();
    }

    pub fn invalidate(&self) {
        unsafe {
            let _ = unsafe { windows::Win32::Graphics::Gdi::InvalidateRect(Some(self.hwnd), None, true) };
        }
    }

    // ---------------------------------------------------------------- 导航

    pub fn open_tab(&mut self, path: PathBuf) {
        // 一个窗格都没有（刚 new 出来 / 分栏时被拖空了）就先补一个满格的
        if self.panes.is_empty() {
            self.panes.push(Pane::default());
            self.focus_pane = 0;
        }
        // 导航树要用驱动器清单，而它只在"进主页 / 定时兜底 / 设备插拔"时刷新。
        // 首次建标签时那张表还是空的（侧栏会只剩"此电脑"没有盘符），这里补一次。
        if self.drives.is_empty() {
            self.refresh_drives();
            if self.cfg.ui.sidebar_thispc_expanded {
                self.sidebar_expanded.insert(SIDEBAR_THISPC.to_string());
            }
        }
        let mut tab = Tab::new(path.clone());
        // 新标签按"这个文件夹自己的视图"来，优先级与 apply_view_for 一致：
        // 本进程改过的（含跨重启持久化的 overrides）→ 资源管理器记的
        // （仅 default_mode=system 时）→ 配置默认。
        // 之前整段包在 `if default_mode == "system"` 里 —— 默认模式不是
        // system 时 override 表压根不查，重启后切好的视图全被默认值顶掉
        // （第29轮 D2 实锤：cfg 里明明有 c:\windows=large，重启仍落 details）。
        let dv = || parse_view(&self.cfg.files.view.default_mode);
        tab.view_mode = match self.view_overrides.get(&norm_key(&path)).copied() {
            Some(m) => m,
            None if self.cfg.files.view.default_mode == "system" => {
                crate::sysview::system_view(&path).unwrap_or_else(dv)
            }
            None => dv(),
        };
        tab.sort = SortSpec { key: parse_sort(&self.cfg.files.view.sort_key), descending: self.cfg.files.view.sort_descending };
        // 贴边窗格最多一个标签（用户要求）：分栏时新建标签不叠加，直接
        // **替换**当前窗格的那一个。顶部总览条照旧可切到别的窗格。
        if self.split() {
            let p = self.pane_mut();
            p.tabs.clear();
            p.tabs.push(tab);
            p.active = 0;
        } else {
            self.pane_mut().tabs.push(tab);
            self.pane_mut().active = self.pane().tabs.len() - 1;
        }
        self.reload();
        self.invalidate();
    }

    /// 新建标签页。与资源管理器一致：新标签落在"主页"，而不是复制当前目录。
    pub fn new_tab(&mut self) {
        self.open_tab(PathBuf::from(HOME_PATH));
    }

    pub fn close_tab(&mut self, index: usize) {
        // 贴边窗格只有一个标签：关掉它 = 收掉整个窗格（用户要求贴边窗口
        // 最多一个标签，没有标签的窗格没有存在意义）。全收完就退回单窗格。
        if self.split() && self.pane().tabs.len() <= 1 {
            let i = self.focus_pane.min(self.panes.len() - 1);
            self.panes.remove(i);
            if self.panes.is_empty() {
                self.panes.push(Pane::default());
                self.focus_pane = 0;
                self.open_tab(PathBuf::from(HOME_PATH));
            } else {
                if self.focus_pane >= self.panes.len() {
                    self.focus_pane = self.panes.len() - 1;
                }
                // 剩下唯一窗格时让它占满，不留残缺矩形
                self.ensure_pane();
                self.reload();
            }
            self.hover_tab = None;
            self.invalidate();
            return;
        }
        // 只剩一个标签也允许关：点了 × 窗口却纹丝不动，是"按钮坏了"的典型观感。
        // 资源管理器在这里是关掉整个窗口，而我们有托盘模式，关窗口 ≠ 退出，
        // 所以照样关，然后补一个新的主页标签回来。
        if self.tab_count() <= 1 {
            self.pane_mut().tabs.clear();
            self.pane_mut().active = 0;
            self.open_tab(PathBuf::from(HOME_PATH));
            self.invalidate();
            return;
        }
        self.pane_mut().tabs.remove(index);
        {
            let p = self.pane_mut();
            if p.active >= p.tabs.len() {
                p.active = p.tabs.len() - 1;
            }
        }
        self.hover_tab = None;
        self.reload();
        self.invalidate();
    }

    pub fn navigate(&mut self, path: PathBuf) {
        // 入口就把 verbatim 前缀剥掉：调用方可能传的是 canonicalize 的结果
        // （带 `\\?\`），那个前缀会让目录枚举返回空列表。
        let path = zenfile_core::paths::strip_verbatim(&path);
        // "跟随展开到当前文件夹"：默认关。开着的时候把当前路径的所有祖先展开，
        // 让导航窗格始终显示"我在哪"。逐层读目录清单是同步的，所以这个开关
        // 关着的时候绝不会有任何额外 IO。
        if self.cfg.ui.sidebar_follow_current && !is_virtual(&path) {
            self.reveal_in_sidebar(&path);
        }
        // 换了目录，"鼠标指着哪一项"就不再成立（条目下标指向的是上一页的清单）。
        // 不清的话新页面第一行会莫名顶着一条悬停底色。
        self.hover_row = None;
        self.hover_pane = None;
        let depth = self.cfg.files.navigation.history_depth;
        // 标签标题也走本地化显示名：资源管理器里 `C:\Users` 的标签写的是"用户"，
        // 直接显示盘上真名会让人对不上号。
        let title = if is_home_path(&path) {
            "此电脑".to_string()
        } else if is_recycle_path(&path) {
            "回收站".to_string()
        } else if is_settings_path(&path) {
            "设置".to_string()
        } else if path.is_dir() {
            self.display_name(&path, &display_title(&path))
        } else {
            display_title(&path)
        };
        {
            // 离开前记住这个目录的滚动位置：从子文件夹走"上一级"进来时
            // apply_history 会取回它（与返回键同一条路）。纯内存，不落盘。
            let cur_key = norm_key(&self.tab().path);
            if !cur_key.is_empty() {
                let cur_scroll = self.tab().scroll;
                self.scroll_memory.insert(cur_key, cur_scroll);
            }
            // （历史记录本身在这里 append，所以此刻查表拿到的是"上一次
            //  离开这个目录"时存的值 —— 正是想要的语义。必须在
            //  tab_mut() 之前取值：可变借用会一直持有到块尾。）
            let want_scroll = self.scroll_memory.get(&norm_key(&path)).copied().unwrap_or(0);
            let tab = self.tab_mut();
            tab.home = is_home_path(&path);
            tab.recycle = is_recycle_path(&path);
            tab.settings = is_settings_path(&path);
            tab.path = path.clone();
            tab.title = title;
            tab.selected.clear();
            // 打开一个目录时回到它上次的滚动位置；没记过 = 顶部。
            tab.scroll = want_scroll;
            tab.focus_row = 0;
            tab.filter.clear();
            tab.search_root = None;
            // 历史：截断前进分支后追加
            tab.history.truncate(tab.history_pos + 1);
            tab.history.push(path.clone());
            if tab.history.len() > depth {
                let cut = tab.history.len() - depth;
                tab.history.drain(..cut);
            }
            tab.history_pos = tab.history.len() - 1;
        }
        // 刚打开的目录不该有"默认选中的第一行"
        self.focus_visible = false;
        // 主动跳转到一个新目录时，"回退后要选中谁"这件事就作废了
        self.pending_select = None;
        // 搜索的防抖挂起一并作废（旧目录敲了一半的查询不该在新目录里炸开），
        // 框里的旧搜索词也立刻清掉 —— 用户报"取消搜索了之前搜索的东西还在"（第40轮）。
        self.search_dirty = false;
        self.search_ticks = 0;
        self.sync_search_edit();
        // 地址栏文本在这里一次更新掉：原来靠每个调用方自己记得调 sync_address，
        // 漏一个就表现为"点了面包屑看着没反应"（其实目录已经切了，只是地址栏
        // 还写着旧路径）。刷新放在导航里，调用方就不可能忘。
        crate::paint::sync_address(self);
        // 视图要在这个目录上重新定一次：资源管理器就是"每个文件夹各记一套"，
        // 从大图标的文件夹走进详细信息文件夹，视图跟着变才是对的。
        self.apply_view_for(&path);
        self.reload();
        // 地址栏进入动画：面包屑从右沿滑进来（kind=0，不需要旧的那一串）
        self.start_addr_anim(0, Vec::new());
        self.invalidate();
    }

    pub fn can_back(&self) -> bool {
        self.tab().history_pos > 0
    }

    pub fn can_forward(&self) -> bool {
        self.tab().history_pos + 1 < self.tab().history.len()
    }

    pub fn go_back(&mut self) {
        if !self.can_back() {
            return;
        }
        // 先记住"当前正待在哪个文件夹"：返回之后，它在上一级目录里应该保持选中
        let from = self.tab().path.clone();
        self.tab_mut().history_pos -= 1;
        let target = self.tab().history[self.tab().history_pos].clone();
        self.apply_history(&target, Some(from), true);
    }

    pub fn go_forward(&mut self) {
        if !self.can_forward() {
            return;
        }
        let from = self.tab().path.clone();
        self.tab_mut().history_pos += 1;
        let target = self.tab().history[self.tab().history_pos].clone();
        self.apply_history(&target, Some(from), false);
    }

    fn apply_history(&mut self, target: &Path, select: Option<PathBuf>, back: bool) {
        // 后退时地址栏要"把旧的那串向右滑出去"，所以切换前先把面包屑抄一份
        let prev_crumbs = self.address_crumbs();
        // 离开前先把当前目录的滚动位置存起来（回头再回来要用）。
        let cur_key = norm_key(&self.tab().path);
        if !cur_key.is_empty() {
            let cur_scroll = self.tab().scroll;
            self.scroll_memory.insert(cur_key, cur_scroll);
        }
        // **取回这个目录上次的滚动位置**（没记过 = 0 = 顶部）。
        // 用户明确要求"返回上一级要回到打开它之前的位置"，原来这里硬写 0。
        let want = self.scroll_memory.get(&norm_key(target)).copied().unwrap_or(0);
        let tab = self.tab_mut();
        tab.home = is_home_path(target);
        tab.path = target.to_path_buf();
        tab.title = if tab.home {
            "此电脑".to_string()
        } else {
            display_title(target)
        };
        tab.selected.clear();
        tab.scroll = want;
        tab.focus_row = 0;
        tab.filter.clear();
        self.pending_select = select;
        self.focus_visible = false;
        // 前进/后退也是换目录，视图同样要按目标文件夹重定一次
        self.apply_view_for(target);
        self.reload();
        self.try_pending_select();
        // 滚动位置是"目录"的属性，**排序一变行高/顺序就变**，老位置会指到
        // 完全无关的行 —— 与其猜，不如重新钳一次（clamp_scroll_to 走同一函数）。
        self.clamp_scroll_to(&crate::paint::layout_for(self));
        // 后退 = 旧面包屑向右滑出（kind=1）；前进 = 新的从右滑入（kind=0）
        self.start_addr_anim(if back { 1 } else { 0 }, prev_crumbs);
        self.invalidate();
    }

    pub fn go_up(&mut self) {
        // 虚拟视图（主页/回收站/设置）没有"上一级"，一律退回主页
        if is_virtual(&self.tab().path) {
            self.navigate(PathBuf::from(HOME_PATH));
            return;
        }
        if let Some(parent) = paths::parent_of(&self.tab().path) {
            // 返回上一级后，刚才从哪一级进来的那个文件夹要保留选中 —— 资源管理器的行为。
            // 侧键/Alt+↑/Backspace 都走这条路径，所以选中效果的落点在这里统一处理。
            let from = self.tab().path.clone();
            self.navigate(parent);
            self.pending_select = Some(from);
            self.try_pending_select();
        }
    }

    /// 尝试把 `pending_select` 落地为真正的选中行；找不到就继续挂着等下一批。
    pub fn try_pending_select(&mut self) {
        let Some(path) = self.pending_select.clone() else { return };
        let mut hit = false;
        {
            let tab = self.tab_mut();
            let row = tab.visible.iter().position(|&idx| {
                tab.entries.get(idx).map(|e| e.path == path).unwrap_or(false)
            });
            if let Some(r) = row {
                if let Some(e) = tab.entries.get(tab.visible[r]) {
                    let k = key_of(e);
                    tab.selected.clear();
                    tab.selected.insert(k);
                    tab.focus_row = r;
                    hit = true;
                }
            }
        }
        if hit {
            self.pending_select = None;
            // 这是"用户从子目录回退"带来的选中，焦点框必须显形，否则等于没选中
            self.focus_visible = true;
            let rename = self.pending_rename;
            self.pending_rename = false;
            self.invalidate();
            if rename {
                // 建完立刻进重命名：光标直接落在新名字上等着改。
                let stem = self.rename_stem_only;
                self.begin_rename_after_create(stem);
            }
        }
    }

    /// 刚建出来的那一项进入重命名。
    ///
    /// `select_stem` 为真时只选中**文件名主部**、把扩展名留在选区外
    /// （新建 .txt 时用户要改的是"新建 文本文档"那几个字，不是把 .txt 一起删了）。
    pub fn begin_rename_after_create(&mut self, select_stem: bool) {
        self.set_focus_row(self.tab().focus_row);
        let Some(idx) = self.tab().visible.get(self.tab().focus_row).copied() else {
            return;
        };
        self.rename_index = Some(idx);
        crate::paint::create_rename_editor(self, select_stem);
    }

    /// 重取本机驱动器清单（类型 / 卷标 / 容量）。
    ///
    /// 一次 `drives::list()` 会对每个盘符做 3 次 syscall，量级很小但不该每帧跑。
    pub fn refresh_drives(&mut self) {
        self.drives.clear();
        for d in zenfile_core::drives::list() {
            let key = d.root.to_uppercase();
            self.drives.insert(key, d);
        }
        self.drives_stamp = Some(std::time::Instant::now());
    }

    /// 停在首页时的数据兜底刷新。
    ///
    /// 文件系统变化不会通知到"主页"这个虚拟视图（它不是真实目录、没有监听句柄），
    /// 所以拷完文件后容量条会一直停在旧值上 —— 看着就是"条不准"。这里在消息循环
    /// 的定时器里每隔 DRIVES_TTL 重取一次；插拔设备另有 WM_DEVICECHANGE 立即触发。
    pub fn tick_refresh_home(&mut self) -> bool {
        if !self.tab().home {
            return false;
        }
        let stale = match self.drives_stamp {
            None => true,
            Some(t) => t.elapsed() > std::time::Duration::from_secs(DRIVES_TTL_SECS),
        };
        if !stale {
            return false;
        }
        self.refresh_drives();
        let entries = home_entries();
        let (show_hidden, show_system) = (self.show_hidden, self.show_system);
        let tab = self.tab_mut();
        tab.entries = entries;
        tab.recompute_filter(show_hidden, show_system);
        true
    }

    /// 启动当前目录枚举。generation 自增使旧批次自然作废（§5 UI 契约）。
    pub fn reload(&mut self) {
        // 目录图标缓存只在这里收口：切换目录 = 一次重绘的起点，此刻销毁旧句柄
        // 绝不会碰到"这一帧已经取出、还没画出去"的图标。
        self.icons.trim_dir_cache();

        // 主页是虚拟目录，直接合成条目，不启动枚举也不建监听
        if self.tab().home {
            // 首页**不能排序**：合成顺序本身就是"常用文件夹在前、驱动器在后"的分区依据，
            // 一旦按名称重排，两类条目会交错，平铺布局就会分错组。
            let entries = home_entries();
            // 驱动器信息一次性取好：类型、卷标、容量都在里面，之后不再碰磁盘。
            // 之前缓存的只有两个 u64，类型写死成"本地磁盘"（U 盘也叫本地磁盘），
            // 而且只在进主页时取一次 —— 拷完一个大文件后，条和数就双双过期了。
            self.refresh_drives();
            // 先把两个开关拷出来：下面要取 tab 的可变借用，届时再读 self 会撞借用检查
            let (show_hidden, show_system) = (self.show_hidden, self.show_system);
            let gen = self.next_generation();
            let tab = self.tab_mut();
            tab.generation = gen;
            tab.entries = entries;
            tab.loading = false;
            tab.recompute_filter(show_hidden, show_system);
            return;
        }

        // 回收站/设置同样是虚拟视图：合成显示数据即可，绝不走目录枚举。
        // 回收站直读 $Recycle.Bin（不走 Shell 的 IShellFolder）。
        // 逐盘扫 `$Recycle.Bin` 在盘多/垃圾多时能到几百毫秒，同步做就是
        // "点一下回收站卡一下"，所以丢后台线程，结果由 WM_APP_RECYCLE 收口。
        if self.tab().recycle {
            self.spawn_recycle_scan();
            return;
        }
        if self.tab().settings {
            let gen = self.next_generation();
            let tab = self.tab_mut();
            tab.generation = gen;
            tab.entries.clear();
            tab.visible.clear();
            tab.loading = false;
            return;
        }

        // 先在取得 tab 的可变借用之前把需要的共享资源取出，避免借用冲突
        let tx = self.engine_tx.clone();
        let search_limit = self.cfg.files.navigation.search_limit;
        let worker_threads = self.cfg.files.operations.worker_threads;

        let generation = self.next_generation();
        let tab = self.tab_mut();
        tab.generation = generation;
        tab.loading = true;
        tab.entries.clear();
        tab.visible.clear();
        let path = tab.path.clone();
        let sort = tab.sort;

        if let Some(search_root) = tab.search_root.clone() {
            let query = tab.filter.clone();
            let request = SearchRequest {
                root: search_root,
                query,
                generation,
                limit: search_limit,
                threads: worker_threads,
            };
            std::thread::spawn(move || engine::run_search(request, tx));
            return;
        }

        std::thread::spawn(move || {
            let _ = engine::enumerate_dir(&path, generation, sort, &tx);
        });

        self.start_watch();
    }

    fn start_watch(&mut self) {
        if let Some(w) = self.watcher.take() {
            w.stop();
        }
        if !self.cfg.files.navigation.watch_current_dir {
            return;
        }
        let (wtx, wrx) = channel::<WatchMsg>();
        match DirWatcher::start(self.tab().path.clone(), wtx) {
            Ok(w) => {
                self.watcher = Some(w);
                self.watch_rx = Some(wrx);
            }
            Err(_) => self.watch_rx = None,
        }
    }

    // ------------------------------------------------------------ 整卷搜索（索引）

    /// 搜索框回车：搜**整个卷**，而不是只搜当前目录。
    ///
    /// 索引在内存里就直接查（毫秒级，不再碰磁盘）；不在就后台载入/建立，
    /// 期间先跑现地递归搜索顶着，索引好了再换成全卷结果。
    pub fn begin_volume_search(&mut self) {
        self.run_search(false);
    }

    /// 执行一次搜索。`live` = 由"边打字边搜"触发。
    ///
    /// 两种触发的差别只有一处：索引没就绪时，打字走**内存筛选**（EN_CHANGE
    /// 已经筛过了，再 reload 就是每敲一键读一次盘，大目录会顿），回车才走 reload。
    pub fn run_search(&mut self, live: bool) {
        let q = self.tab().filter.trim().to_string();
        let cur = self.tab().path.clone();
        let Some(root) = zenfile_core::index::volume_root(&cur) else {
            // UNC / 拿不到卷根：退回"当前子树递归"，行为跟以前一样
            let tab = self.tab_mut();
            tab.search_root = Some(tab.path.clone());
            self.reload();
            return;
        };
        if q.is_empty() {
            let tab = self.tab_mut();
            tab.search_root = None;
            tab.filter.clear();
            self.search_hits = 0;
            self.search_ms = 0.0;
            self.reload();
            return;
        }
        {
            let tab = self.tab_mut();
            tab.filter = q.clone();
        }
        if self.index_root.as_deref() == Some(root.as_str()) {
            if let Some(idx) = self.index.clone() {
                self.index_pending = false;
                self.apply_index_hits(&idx, &q);
                return;
            }
        }
        // 索引还没就绪：**只搜当前目录**（有界，几十毫秒就出结果），绝不去递归
        // 整个卷 —— 那边索引线程正在全卷扫盘，再叠一个全卷递归会把机器压死，
        // 而且两个结果集先后到达，用户会看到列表先变一次再变一次。
        self.index_pending = true;
        {
            let tab = self.tab_mut();
            tab.search_root = Some(cur.clone());
        }
        self.ensure_index(&root);
        if live {
            let (sh, ss) = (self.show_hidden, self.show_system);
            self.tab_mut().recompute_filter(sh, ss);
            self.invalidate();
        } else {
            self.reload();
        }
    }

    /// 搜索框内容变了：先做即时的本地筛选（零延迟手感），再挂一个防抖，
    /// 停手 ~200ms 后才跑真正的整卷查询。
    pub fn on_search_text_changed(&mut self) {
        self.search_dirty = true;
        self.search_ticks = SEARCH_DEBOUNCE_TICKS;
    }

    /// 把**当前标签**的搜索词同步进搜索框（全窗口唯一的那个子 EDIT）。
    ///
    /// 搜索框是全局共享的，而 `filter` 是每个标签各一份：切标签 / 导航 /
    /// 取消搜索之后，框里还留着上一段查询 —— 用户报"取消搜索了，之前
    /// 搜索的东西还在"（第40轮）。文本没变时只做一次比较，不写。
    pub fn sync_search_edit(&mut self) {
        if self.hwnd_search.0.is_null() {
            return;
        }
        let want = self.tab().filter.clone();
        if crate::paint::read_window_text(self.hwnd_search) == want {
            return;
        }
        crate::paint::set_window_text_utf16(self.hwnd_search, &want);
    }

    /// 由 100ms 的 TIMER_DRAIN 驱动，返回是否真的跑了一次搜索。
    pub fn tick_search(&mut self) -> bool {
        if !self.search_dirty {
            return false;
        }
        if self.search_ticks > 0 {
            self.search_ticks -= 1;
            return false;
        }
        self.search_dirty = false;
        // 空的查询串不需要整卷搜：清空就完了
        if self.tab().filter.trim().is_empty() {
            self.run_search(true);
            return true;
        }
        self.run_search(true);
        true
    }

    /// 保证 `root` 这个卷的索引在后台被载入或建立好。
    pub fn ensure_index(&mut self, root: &str) {
        if self.index_root.as_deref() == Some(root)
            && !matches!(self.index_state, IndexState::Idle | IndexState::Failed(_))
        {
            return;
        }
        // 换卷：先把上一个卷的建索引线程收掉，别两个线程一起扫盘
        if let Some(c) = &self.index_cancel {
            c.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.index = None;
        self.index_root = Some(root.to_string());
        let (tx, rx) = channel::<IndexMsg>();
        self.index_rx = Some(rx);
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.index_cancel = Some(cancel.clone());
        // 只判断缓存文件在不在，**绝不能在这里 Index::load()** ——
        // 那是在 UI 线程上把几十 MB 的缓存整个反序列化一遍（实测 70ms，
        // 盘更大时更久），用户按下回车那一下界面就是卡住的。
        let has_cache = zenfile_core::index::Index::cache_path(root)
            .map(|p| p.exists())
            .unwrap_or(false);
        self.index_state = if has_cache {
            IndexState::Loading
        } else {
            IndexState::Building(0)
        };
        let root_s = root.to_string();
        std::thread::spawn(move || {
            // 有缓存：直接反序列化，比遍历快两个数量级
            if let Some(idx) = zenfile_core::index::Index::load(&root_s) {
                let _ = tx.send(IndexMsg::Ready(std::sync::Arc::new(idx)));
                return;
            }
            // 没有：遍历一遍。中途回报条数，好让界面上有可见的进度
            let last = std::cell::Cell::new(0usize);
            let idx = zenfile_core::index::Index::build(&root_s, &cancel, Some(&|n: usize| {
                if n >= last.get() + 20_000 {
                    last.set(n);
                    let _ = tx.send(IndexMsg::Progress(n));
                }
            }));
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            // 存下来，下次启动/下次搜索直接命中缓存
            let _ = idx.save();
            let _ = tx.send(IndexMsg::Ready(std::sync::Arc::new(idx)));
        });
    }

    /// 用索引里的结果替换当前标签的内容。
    pub fn apply_index_hits(&mut self, idx: &zenfile_core::index::Index, q: &str) {
        let limit = self.cfg.files.navigation.search_limit.max(50);
        let t0 = std::time::Instant::now();
        let hits = idx.query(q, limit);
        self.search_ms = t0.elapsed().as_secs_f64() * 1000.0;
        self.search_hits = hits.len();
        let (show_hidden, show_system) = (self.show_hidden, self.show_system);
        let mut entries: Vec<FileEntry> = hits
            .into_iter()
            .map(|h| FileEntry {
                name: h.name,
                path: h.path,
                kind: if h.dir { FileKind::Directory } else { FileKind::File },
                size: h.size,
                modified: h.modified,
                created: 0,
                accessed: 0,
                attributes: if h.dir { 0x10 } else { 0 } | if h.hidden { 0x2 } else { 0 },
                hidden: h.hidden,
            })
            .collect();
        let gen = self.next_generation();
        let tab = self.tab_mut();
        tab.generation = gen;
        tab.entries.clear();
        tab.entries.append(&mut entries);
        tab.sort.sort(&mut tab.entries);
        tab.search_root = Some(PathBuf::from(&idx.root));
        tab.loading = false;
        tab.scroll = 0;
        tab.focus_row = 0;
        tab.recompute_visibility(show_hidden, show_system);
        self.focus_visible = false;
        self.invalidate();
    }

    /// 收索引线程的消息（在消息循环里每帧调一次）。
    pub fn drain_index(&mut self) -> bool {
        let mut changed = false;
        let msgs: Vec<IndexMsg> = match &self.index_rx {
            Some(rx) => rx.try_iter().collect(),
            None => Vec::new(),
        };
        for msg in msgs {
            match msg {
                IndexMsg::Progress(n) => {
                    self.index_state = IndexState::Building(n);
                    changed = true;
                }
                IndexMsg::Ready(idx) => {
                    self.index_root = Some(idx.root.clone());
                    self.index_state = IndexState::Ready;
                    self.index = Some(idx.clone());
                    // 索引刚到位：如果当前正处于搜索态，立刻换成全卷结果。
                    // `index_pending` 覆盖"索引还没好、先顶着的是当前目录结果"这一种；
                    // `root_match` 覆盖"用户已经搜过一次、索引后来才到"这一种。
                    let root_match = self
                        .tab()
                        .search_root
                        .as_ref()
                        .map(|p| p.to_string_lossy() == idx.root)
                        .unwrap_or(false);
                    let q = self.tab().filter.trim().to_string();
                    if !q.is_empty() && (self.index_pending || root_match) {
                        self.index_pending = false;
                        self.apply_index_hits(&idx, &q);
                    }
                    changed = true;
                }
                IndexMsg::Failed(why) => {
                    self.index_state = IndexState::Failed(why);
                    changed = true;
                }
            }
        }
        changed
    }

    pub fn drain(&mut self) {
        // 搜索框文本跟随当前标签（TIMER_DRAIN 每 100ms 一次；文本没变只是
        // 一次字符串比较）。放在这里而不是散在各切标签点位：active 的赋值
        // 分散在十几个地方，漏一个就又出现"旧查询还挂在框上"。
        self.sync_search_edit();
        let mut changed = self.drain_index();
        // 同 reload()：把开关拷出来，避免循环体内对 self.tabs 的可变借用与 self 字段读取冲突
        let (show_hidden, show_system) = (self.show_hidden, self.show_system);
        while let Ok(msg) = self.engine_rx.try_recv() {
            match msg {
                EngineMsg::Batch(batch) => {
                    // 必须在**所有窗格**里找这个 generation，不能只搜焦点窗格
                    // （第39轮审计 #8）：左右分栏时左窗格正在枚举，用户点一下
                    // 右窗格 → 批次到达时 pane_mut() 已指向右窗格、找不到就
                    // 静默丢弃 → 左窗格永久空白 + loading 永远 true（进度条空转），
                    // 而且这个状态下没有任何报错线索。
                    if let Some(tab) = self.find_tab_mut(batch.generation) {
                        if batch.done {
                            tab.loading = false;
                        }
                        tab.entries.extend(batch.entries);
                        tab.sort.sort(&mut tab.entries);
                        // 搜索批次已经被引擎按查询串筛过了，这里只补"隐藏/受保护"
                        // 两道开关；否则通配符查询的命中项会被子串规则全滤掉。
                        if tab.search_root.is_some() {
                            tab.recompute_visibility(show_hidden, show_system);
                        } else {
                            tab.recompute_filter(show_hidden, show_system);
                        }
                        changed = true;
                    }
                }
                EngineMsg::Error(gen, e) => {
                    if let Some(tab) = self.find_tab_mut(gen) {
                        tab.loading = false;
                    }
                    eprintln!("枚举失败: {e}");
                    changed = true;
                }
            }
        }
        if let Some(rx) = &self.watch_rx {
            let mut touched = false;
            while rx.try_recv().is_ok() {
                touched = true;
            }
            if touched {
                self.reload();
            }
        }
        // 新一批条目到了：如果还挂着"回退后要选中谁"，就趁现在落地
        self.try_pending_select();
        if changed {
            self.invalidate();
        }
    }

    /// 在**所有窗格的所有标签**里按 generation 找可变引用。
    ///
    /// 枚举结果属于"发起那次导航的标签"，跟当前焦点在哪个窗格无关 ——
    /// 只搜焦点窗格会把别的窗格的批次静默丢掉（第39轮审计 #8）。
    fn find_tab_mut(&mut self, generation: u64) -> Option<&mut Tab> {
        for pane in self.panes.iter_mut() {
            if let Some(t) = pane.tabs.iter_mut().find(|t| t.generation == generation) {
                return Some(t);
            }
        }
        None
    }

    /// 取下一个目录枚举世代号（**全局唯一**，见 `generation_seq` 的注释）。
    fn next_generation(&mut self) -> u64 {
        self.generation_seq += 1;
        self.generation_seq
    }

    // ------------------------------------------------------------ 选择/视图

    pub fn row_hit(&self, y: i32, view_top: i32, view_h: i32) -> Option<usize> {
        let tab = self.tab();
        let row_h = self.metrics.row_height_for(tab.view_mode);
        let local = y - view_top + tab.scroll;
        if local < 0 {
            return None;
        }
        let idx = (local / row_h) as usize;
        if idx < tab.visible.len() && y >= view_top && y <= view_top + view_h {
            Some(idx)
        } else {
            None
        }
    }

    pub fn set_focus_row(&mut self, row: usize) {
        let len = self.tab().visible.len();
        // 走到这里说明是用户主动把焦点放到某一行（点击/键盘），焦点框从此显形。
        // navigate() 会把它重新置回 false —— 换个目录又是"没有默认选中"的状态。
        self.focus_visible = true;
        if len == 0 {
            self.tab_mut().focus_row = 0;
            return;
        }
        let clamped = row.min(len - 1);
        self.tab_mut().focus_row = clamped;
    }

    /// 右键命中某一行时的选中处理（对齐资源管理器）：
    ///   - 该行**尚未**选中 → 丢弃原多选，改为只选中它；
    ///   - 该行**已**在选中集合里 → 保持现有选中不变（便于对多选整体操作）。
    /// 返回是否真的命中了该行。
    ///
    /// 注意 `selected_paths()` 在选中为空时会退回到 focus_row，所以右键前必须先把
    /// focus_row 落到光标行，否则出现"右键 B 却作用在 A 上"。
    pub fn context_select_row(&mut self, row: usize) -> bool {
        let some_idx = self.tab().visible.get(row).copied();
        let Some(idx) = some_idx else { return false };
        let Some(entry) = self.tab().entries.get(idx) else { return false };
        let key = key_of(entry);
        let already = self.tab().selected.contains(&key);

        self.set_focus_row(row);
        if !already {
            let tab = self.tab_mut();
            tab.selected.clear();
            if let Some(idx) = tab.visible.get(row).copied() {
                if let Some(e) = tab.entries.get(idx) {
                    tab.selected.insert(key_of(e));
                }
            }
        }
        true
    }

    pub fn toggle_select(&mut self, row: usize, additive: bool, range: bool) {
        // 锚点必须在 set_focus_row **之前**取：Shift 范围选择的起点是
        // "上一次的焦点行"，先落焦点再取锚点会得到 start==end==row ——
        // 范围选择退化成"只选当前一行"，而且下面的 clear() 连原有选中
        // 一起清掉（第39轮审计 #5：Shift+点第 10 行只选中第 10 行）。
        let anchor = self.tab().focus_row;
        self.set_focus_row(row);
        let tab = self.tab_mut();
        let Some(idx) = tab.visible.get(row).copied() else { return };
        let Some(entry) = tab.entries.get(idx) else { return };
        let k = key_of(entry);
        if range {
            let start = anchor.min(row);
            let end = anchor.max(row);
            tab.selected.clear();
            for r in start..=end {
                if let Some(i) = tab.visible.get(r) {
                    if let Some(e) = tab.entries.get(*i) {
                        tab.selected.insert(key_of(e));
                    }
                }
            }
        } else if additive {
            if !tab.selected.insert(k.clone()) {
                tab.selected.remove(&k);
            }
        } else {
            tab.selected.clear();
            tab.selected.insert(k);
        }
    }

    /// 滚动钳制。网格视图的内容高度是"行数 × 行高"，而行数由**列数**决定、
    /// 列数又来自布局宽度 —— 所以只看 metrics 算出来的高度是错的，必须拿 Layout。
    pub fn clamp_scroll_to(&mut self, l: &crate::paint::Layout) {
        let view_h = l.view.bottom - l.view.top;
        let metrics = self.metrics;
        let mode = self.tab().view_mode;
        let count = self.tab().visible.len();
        let content = match mode {
            zenfile_core::ViewMode::LargeIcons | zenfile_core::ViewMode::MediumIcons => {
                let g = crate::paint::icon_grid(l, &metrics, mode);
                crate::paint::grid_content_h(&g, count)
            }
            _ => count as i32 * metrics.row_height_for(mode),
        };
        let max = (content - view_h).max(0);
        let tab = self.tab_mut();
        tab.scroll = tab.scroll.min(max).max(0);
    }

    /// 滚轮钳制（分栏）：约束的是**光标所在窗格**的标签，不是焦点窗格的。
    ///
    /// `pl` 是那个窗格覆写过的布局（列数/行数按窗格宽算），`l` 是全局布局
    /// （用来认光标落在哪个窗格）。非分栏时退化成 `clamp_scroll_to(pl)`。
    pub fn clamp_pane_scroll(&mut self, x: i32, y: i32, l: &crate::paint::Layout, pl: &crate::paint::Layout) {
        if !self.split() {
            self.clamp_scroll_to(pl);
            return;
        }
        let pi = crate::paint::pane_index_at(self, l, x, y).unwrap_or(self.focus_pane);
        let view_h = pl.view.bottom - pl.view.top;
        let metrics = self.metrics;
        let pi = pi.min(self.panes.len() - 1);
        let p = &mut self.panes[pi];
        if p.tabs.is_empty() {
            return;
        }
        let ai = p.active.min(p.tabs.len() - 1);
        let tab = &mut p.tabs[ai];
        let content = match tab.view_mode {
            zenfile_core::ViewMode::LargeIcons | zenfile_core::ViewMode::MediumIcons => {
                let g = crate::paint::icon_grid(pl, &metrics, tab.view_mode);
                crate::paint::grid_content_h(&g, tab.visible.len())
            }
            _ => tab.visible.len() as i32 * metrics.row_height_for(tab.view_mode),
        };
        let max = (content - view_h).max(0);
        tab.scroll = tab.scroll.min(max).max(0);
    }

    pub fn select_all(&mut self) {
        let tab = self.tab_mut();
        tab.selected.clear();
        // 只选**可见**条目（第39轮审计 #4）：`entries` 是全量，含隐藏项与
        // 被筛选掉的项。遍历它会出现"状态栏显示选中 500、屏幕只高亮 3 行"，
        // 此时按 Delete 会把用户根本看不见的 480 个文件一起删掉。
        // 所有其它选择路径（点击/Shift/方向键）走的都是 visible，这里必须一致。
        for i in &tab.visible {
            if let Some(e) = tab.entries.get(*i) {
                tab.selected.insert(key_of(e));
            }
        }
    }

    pub fn sort_column(&mut self, key: SortKey) {
        let (show_hidden, show_system) = (self.show_hidden, self.show_system);
        let tab = self.tab_mut();
        tab.sort.toggle(key);
        tab.sort.sort(&mut tab.entries);
        tab.recompute_filter(show_hidden, show_system);
        self.invalidate();
    }

    /// 表头右键：直接按某一列排序并给定方向（列与方向一次到位）。
    /// `sem` 是 `paint::header_col_semantic` 的语义列号。
    pub fn header_sort(&mut self, sem: u8, descending: bool) {
        let key = match sem {
            0 => SortKey::Name,
            1 => SortKey::Modified,
            2 => SortKey::Type,
            _ => SortKey::Size,
        };
        let (show_hidden, show_system) = (self.show_hidden, self.show_system);
        let tab = self.tab_mut();
        tab.sort.key = key;
        tab.sort.descending = descending;
        tab.sort.sort(&mut tab.entries);
        tab.recompute_filter(show_hidden, show_system);
        tab.scroll = 0;
        self.invalidate();
    }

    /// 只改排序方向，排序列保持不变。
    pub fn set_sort_direction(&mut self, descending: bool) {
        let (show_hidden, show_system) = (self.show_hidden, self.show_system);
        let tab = self.tab_mut();
        if tab.sort.descending == descending {
            return;
        }
        tab.sort.descending = descending;
        tab.sort.sort(&mut tab.entries);
        tab.recompute_filter(show_hidden, show_system);
        self.invalidate();
    }

    /// 左键按下：记下拖拽起点。此刻还不确定是不是真的要拖 —— 手抖几像素不算。
    pub fn begin_drag(&mut self, x: i32, y: i32, kind: DragKind, ctrl: bool) {
        let items = if kind == DragKind::Items {
            self.tab().selected_paths()
        } else {
            Vec::new()
        };
        self.drag = Some(Drag { origin: (x, y), cur: (x, y), kind, active: false, items, copy: ctrl });
        self.drop_target = None;
    }

    /// 鼠标移动：更新当前点，越过 4px 阈值才真正进入拖拽态。
    pub fn update_drag(&mut self, x: i32, y: i32, ctrl: bool) {
        // 两个来源都要认：真实键盘状态（人手按住 Ctrl）和消息里的 MK_CONTROL
        // （自动化测试 / 某些输入堆栈只带标志位不带按键状态）。
        let live = ctrl || self.drag_copy();
        if let Some(d) = self.drag.as_mut() {
            d.cur = (x, y);
            d.copy = d.copy || live;
            if !d.active {
                let dx = x - d.origin.0;
                let dy = y - d.origin.1;
                if dx * dx + dy * dy > 16 {
                    d.active = true;
                }
            }
            // 框选要**边拖边更新选中集**（资源管理器就这样）—— 不然用户拖到
            // 哪条都没反应，松开那一瞬才一次性全亮，体感像"卡住"了。
            // 计算本身不重（仅 set/clear 一个 HashSet + 遍历可见条目矩形），
            // 不在这里直接 `invalidate` —— 由主循环每帧 WM_MOUSEMOVE 后
            // 统一 InvalidateRect，避免本函数被多次调用时反复失效。
        }
    }

    /// 按下标签：记一份"可能在拖"的状态。真正进入拖拽态要等越过阈值。
    pub fn begin_tab_drag(&mut self, pane: usize, index: usize, x: i32, y: i32) {
        self.tab_drag = Some(TabDrag {
            pane,
            index,
            origin: (x, y),
            cur: (x, y),
            active: false,
            over_tab: None,
            dock: None,
            outside: false,
        });
    }

    /// 松手：算出来这一拖到底要干嘛。**只算不做** —— 开新进程、重列目录
    /// 这类动作交给 WndProc，App 不该在自己里面偷偷起进程。
    pub fn finish_tab_drag(&mut self) -> TabDrop {
        let Some(d) = self.tab_drag.take() else {
            return TabDrop::Nothing;
        };
        self.tab_hover_since = None;
        if !d.active {
            return TabDrop::Nothing;
        }
        if d.pane >= self.panes.len() || d.index >= self.panes[d.pane].tabs.len() {
            return TabDrop::Nothing;
        }
        if d.outside {
            return TabDrop::TearOff {
                pane: d.pane,
                index: d.index,
                path: self.panes[d.pane].tabs[d.index].path.clone(),
            };
        }
        if let Some(z) = d.dock {
            return TabDrop::Dock { pane: d.pane, index: d.index, zone: z };
        }
        if let Some(to) = d.over_tab {
            if to != d.index {
                return TabDrop::Reorder { pane: d.pane, from: d.index, to };
            }
        }
        TabDrop::Nothing
    }

    /// 拖着文件悬在标签上：悬够 600ms 就把那个标签翻出来（资源管理器同款）。
    ///
    /// 由 `WM_TIMER` 调用。返回 true 表示这一跳真的切了标签（调用方去重画）。
    pub fn tick_tab_hover(&mut self) -> bool {
        let Some((i, t0)) = self.tab_hover_since else {
            return false;
        };
        if t0.elapsed().as_millis() < 600 {
            return false;
        }
        if self.drag.is_none() {
            self.tab_hover_since = None;
            return false;
        }
        if i >= self.panes[self.focus_pane].tabs.len() {
            self.tab_hover_since = None;
            return false;
        }
        // 已经切过去了就别反复刷（否则每次 reload 一遍）
        if self.panes[self.focus_pane].active == i {
            self.tab_hover_since = None;
            return false;
        }
        self.panes[self.focus_pane].active = i;
        self.tab_hover_since = None;
        self.reload();
        true
    }

    /// 拖拽时按住 Ctrl = 复制（与资源管理器一致）；不按就是移动。
    pub fn drag_copy(&self) -> bool {
        unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(0x11) < 0 }
    }

    pub fn cycle_view(&mut self) {
        let next = self.tab().view_mode.next();
        self.set_view(next);
    }

    /// 直接切到指定视图（Ctrl+1..4 / 右键"查看" / 设置页）。
    ///
    /// 改完要**回写给资源管理器**（尽力而为，见 `sysview`）：用户在 ZenFile 里
    /// 把某文件夹切成大图标，回到资源管理器里打开它也应该还是大图标。
    pub fn set_view(&mut self, mode: ViewMode) {
        let path = self.tab().path.clone();
        self.tab_mut().view_mode = mode;
        // 记进内存覆盖表：bag 可能写不进去（资源管理器从没去过这个文件夹），
        // 那样至少在本进程里来回切换不该被默认值吃掉。
        self.view_overrides.insert(norm_key(&path), mode);
        crate::sysview::write_system_view(&path, mode);
        // 再落盘：内存表只保本进程，重启就没了（第29轮"窗口位置和布局
        // 样式都不保存"的布局一侧）。每次全量写整张表 —— 启动时表是从
        // 同一个文件读的，全量写 = 内存即文件，深合并不会残留旧键。
        let map: serde_json::Map<String, serde_json::Value> = self
            .view_overrides
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(view_name(*v).to_string())))
            .collect();
        if let Err(e) = zenfile_core::config::Config::save_user_patch(&serde_json::json!({
            "files": { "view": { "overrides": map } }
        })) {
            eprintln!("保存视图覆盖失败: {e}");
        }
        self.invalidate();
    }

    /// 进目录时决定用哪种视图：
    ///   1. 本进程里手动改过的（最高优先级）
    ///   2. 资源管理器给这个文件夹记的（default_mode=system 时）
    ///   3. 配置里的默认视图
    pub fn apply_view_for(&mut self, path: &Path) {
        let key = norm_key(path);
        let mut mode = self.view_overrides.get(&key).copied();
        if mode.is_none() && self.cfg.files.view.default_mode == "system" {
            mode = crate::sysview::system_view(path);
        }
        let mode = mode.unwrap_or_else(|| parse_view(&self.cfg.files.view.default_mode));
        self.tab_mut().view_mode = mode;
    }

    /// 标签页循环切换（Ctrl+Tab / Ctrl+Shift+Tab）
    pub fn cycle_tab(&mut self, delta: isize) {
        if self.tab_count() < 2 {
            return;
        }
        let len = self.tab_count() as isize;
        let cur = self.pane().active as isize;
        self.pane_mut().active = (((cur + delta) % len + len) % len) as usize;
        self.reload();
        self.invalidate();
    }

    /// 把选中项（或焦点项）的完整路径以文本形式放进剪贴板
    pub fn copy_paths_text(&mut self) -> Result<(), zenfile_core::Error> {
        let paths = self.tab().selected_paths();
        if paths.is_empty() {
            return Ok(());
        }
        let mut text = String::new();
        for p in &paths {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&p.to_string_lossy());
        }
        clipboard::set_text(&text)
    }

    // ------------------------------------------------------------ 文件操作

    pub fn open_focused(&mut self) {
        let Some(target) = self.tab().selected_paths().first().cloned() else { return };
        let target = std::fs::canonicalize(&target).unwrap_or(target);
        if target.is_dir() {
            self.navigate(target);
        } else {
            self.launch_file(&target);
        }
    }

    pub fn launch_file(&self, path: &Path) {
        use windows::Win32::UI::Shell::ShellExecuteW;
        // ShellExecute 同样不接受 verbatim 前缀
        let path = zenfile_core::paths::strip_verbatim(path);
        unsafe {
            let file = windows::core::HSTRING::from(path.to_string_lossy().as_ref());
            let _ = ShellExecuteW(
                Some(self.hwnd),
                &windows::core::HSTRING::from("open"),
                &file,
                None,
                None,
                windows::Win32::UI::WindowsAndMessaging::SW_SHOW,
            );
        }
    }

    pub fn delete_selected(&mut self, permanent: bool) {
        let items = self.tab().selected_paths();
        if items.is_empty() {
            return;
        }
        if self.cfg.files.operations.confirm_delete && !confirm(self.hwnd, &format!(
            "确定要{} {} 个项目吗？",
            if permanent { "永久删除" } else { "删除到回收站" },
            items.len()
        )) {
            return;
        }
        let owner = self.hwnd.0 as isize;
        if let Err(e) = delete_items(owner, &items, !permanent) {
            report(self.hwnd, &format!("删除失败：{e}"));
        }
        self.reload();
    }

    /// 新建之后的收尾：重列 + 把新建那一项选中 + 进重命名。
    ///
    /// 目录枚举是异步的，所以"选中"只能挂成 `pending_select`，等枚举批次到了再
    /// 落地（见 `try_pending_select`），重命名也跟着那一刻一起触发。
    fn after_create(&mut self, path: PathBuf, select_stem: bool) {
        self.pending_select = Some(path);
        self.pending_rename = true;
        self.rename_stem_only = select_stem;
        self.reload();
    }

    pub fn new_folder(&mut self) {
        let dir = self.tab().path.clone();
        let owner = self.hwnd.0 as isize;
        if let Err(e) = new_folder(owner, &dir) {
            report(self.hwnd, &format!("创建失败：{e}"));
            return;
        }
        // new_folder 用的是 Shell 那套，拿不到确切名字；按它的命名规则推出候选
        let p = paths::unique_name(&dir, "新建文件夹", "");
        self.after_create(p, false);
    }

    pub fn new_file(&mut self) {
        let dir = self.tab().path.clone();
        let p = paths::unique_name(&dir, "新建文本文档", ".txt");
        let fname = p.file_name().unwrap_or_default().to_string_lossy().to_string();
        let owner = self.hwnd.0 as isize;
        if let Err(e) = new_file(owner, &dir, &fname) {
            report(self.hwnd, &format!("创建失败：{e}"));
            return;
        }
        self.after_create(p, true);
    }

    /// 按系统 ShellNew 模板新建（"新建 → 文本文档 / RTF 文档 / …"）。
    pub fn new_from_template(&mut self, ext: &str) {
        let dir = self.tab().path.clone();
        match crate::shellnew::create(&dir, ext) {
            Ok(p) => self.after_create(p, true),
            Err(e) => report(self.hwnd, &format!("创建失败：{e}")),
        }
    }

    pub fn begin_rename(&mut self) {
        self.set_focus_row(self.tab().focus_row);
        let (idx, name) = {
            let tab = self.tab();
            let Some(idx) = tab.visible.get(tab.focus_row).copied() else { return };
            let Some(entry) = tab.entries.get(idx) else { return };
            (idx, entry.name.clone())
        };
        self.rename_index = Some(idx);
        let _ = name;
        crate::paint::create_rename_editor(self, true);
    }

    pub fn commit_rename(&mut self, new_name: &str) {
        // 顺序很关键：**先**把 rename_index 取走，再销毁编辑框。
        // DestroyWindow 会同步发 WM_KILLFOCUS，而 KILLFOCUS 走的是 cancel 分支，
        // 它正好把 rename_index 清成 None。先销毁的话，回到这里 take() 拿到 None，
        // 回车就永远改不了名 —— 用户看到"输入框关了但文件没变"。
        let Some(idx) = self.rename_index.take() else { return };
        self.destroy_rename_editor();
        let Some(entry) = self.tab().entries.get(idx) else { return };
        let target = entry.path.clone();
        if new_name.is_empty() || new_name == entry.name {
            return;
        }
        let owner = self.hwnd.0 as isize;
        if let Err(e) = rename_item(owner, &target, new_name) {
            report(self.hwnd, &format!("重命名失败：{e}"));
        }
        self.reload();
    }

    pub fn cancel_rename(&mut self) {
        self.destroy_rename_editor();
        self.rename_index = None;
    }

    /// 把编辑框里现在的文字当作新名字提交（供"点一下别处"收尾用）。
    pub fn commit_rename_from_editor(&mut self) {
        let Some(h) = self.rename_hwnd else { return };
        let name = crate::paint::read_window_text(h);
        self.commit_rename(&name);
    }

    fn destroy_rename_editor(&mut self) {
        if let Some(h) = self.rename_hwnd.take() {
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(h);
            }
        }
    }

    /// 复制/剪切写入标准 CF_HDROP，与 Explorer 双向互通（§6.4）。
    pub fn clipboard_write(&mut self, mode: ClipMode) {
        let items = self.tab().selected_paths();
        if items.is_empty() {
            return;
        }
        if let Err(e) = clipboard::set_files(&items, mode, self.hwnd.0 as isize) {
            report(self.hwnd, &format!("剪贴板写入失败：{e}"));
            return;
        }
        // 剪切的条目要在原地虚掉（见 `cut_set`）；复制的不动。
        // 这一份清单只服务于视觉 —— 真正的"拿走什么"在剪贴板里。
        self.cut_set = match mode {
            ClipMode::Move => items.iter().map(|p| p.to_string_lossy().to_lowercase()).collect(),
            ClipMode::Copy => HashSet::new(),
        };
        self.invalidate();
    }

    pub fn paste(&mut self) {
        match clipboard::get_files() {
            Ok(Some((files, mode))) if !files.is_empty() => {
                let dest = self.tab().path.clone();
                // 只有"把某个文件夹粘进它自己"才是真做不了（会无限递归）。
                // 原目录里粘贴是**合法且常用**的操作：资源管理器会生成"X - 副本"。
                // 之前写成"源与目标相同就跳过"，于是最常用的"复制完就地粘贴"
                // 什么都不发生，看着就像粘贴功能坏了。
                if files.iter().any(|f| f == &dest) {
                    report(self.hwnd, "不能把文件夹粘贴到它自己里面");
                    return;
                }
                // 东西已经落下去了，原处那层"虚化"必须跟着撤掉
                self.cut_set.clear();
                // 一律走自研引擎：不再调 `IFileOperation`（它那个进度窗是模态的，
                // 会把 WndProc 重入进 `&mut App`；而且只给百分比，拿不到
                // "已处理/剩余/已用/预计剩余"）。
                let kind = if mode == ClipMode::Move {
                    zenfile_core::transfer::Kind::Move
                } else {
                    zenfile_core::transfer::Kind::Copy
                };
                self.start_transfer(kind, dest, files);
                self.invalidate();
            }
            Ok(Some(_)) | Ok(None) => {}
            Err(e) => report(self.hwnd, &format!("剪贴板读取失败：{e}")),
        }
    }

    /// 后台文件操作完成：UI 线程在这里收口（报错 + 重列）。
    ///
    /// 由 `WM_APP_IO_DONE` 调用。空串 = 成功。
    pub fn drain_io(&mut self) {
        let mut err: Option<String> = None;
        while let Ok(msg) = self.io_rx.try_recv() {
            self.io_pending = self.io_pending.saturating_sub(1);
            if !msg.is_empty() && err.is_none() {
                err = Some(msg);
            }
        }
        if let Some(e) = err {
            report(self.hwnd, &e);
        }
    }

    /// 起一次自研传输：后台线程搬字节 + 自绘进度窗。
    ///
    /// 刻意**不**用 `spawn_io`：那一条路是给 `IFileOperation` 用的，
    /// 这里要的是能随时读到进度、能取消的句柄。
    pub fn start_transfer(
        &mut self,
        kind: zenfile_core::transfer::Kind,
        dest: PathBuf,
        items: Vec<PathBuf>,
    ) {
        if items.is_empty() {
            return;
        }
        // 上一次还没跑完就再来一次 → 弹窗只能显示一个，直接拒掉，
        // 免得两个线程同时往同一个目录里写。
        if self.xfer.is_some() {
            report(self.hwnd, "还有一次复制/移动没结束");
            return;
        }
        let threads = self.cfg.files.operations.worker_threads.clamp(2, 32);
        let t = zenfile_core::transfer::start(kind, &dest, items, threads);
        crate::progress::open(kind, &dest, t.stats.clone(), t.cancel.clone(), self.hwnd);
        self.xfer = Some(t);
    }

    /// 自研传输的收口：`finished` 一置起来就重列目录、报错。
    ///
    /// 由 `WM_TIMER`（10 跳/秒）调用。弹窗自己也在轮询同一份 stats，
    /// 所以"弹窗关了但活还在跑"是完全合法的 —— 这里照样能收尾。
    pub fn drain_transfer(&mut self) {
        let Some(t) = self.xfer.as_ref() else {
            return;
        };
        let (finished, err, failed, canceled) = {
            let Ok(s) = t.stats.lock() else { return };
            (s.finished, s.error.clone(), s.failed, s.canceled)
        };
        if !finished {
            return;
        }
        self.xfer = None;
        if let Some(e) = err {
            report(self.hwnd, &format!("传输失败：{e}"));
        } else if canceled {
            // 取消过就别弹框打扰，状态栏里能看出来
        } else if failed > 0 {
            report(self.hwnd, &format!("有 {failed} 项没处理成功，其余已完成"));
        }
        self.reload();
    }

    /// 把一次 Shell 文件操作丢到**独立线程**上去跑。
    ///
    /// `IFileOperation::PerformOperations` 是模态的：它会在 COM 自己的循环里泵
    /// 消息，于是 WndProc 被重入 —— 而调用方（比如松手那条 `WM_LBUTTONUP`）此刻
    /// 正握着 `&mut App`。两个 `&mut` 同时存在是未定义行为，落到界面上就是
    /// "拖完一松手就僵住"。放到独立线程上，UI 线程既不阻塞也不会被重入；
    /// 搬大文件时界面照样能刷能点。
    ///
    /// owner 一律传 0：Shell 的进度/确认框因此是无属主的顶级窗口，不会反过来
    /// 把我们的窗口 disable 掉（那才是真的"卡死"）。
    fn spawn_io<F>(&mut self, label: &str, job: F)
    where
        F: FnOnce() -> zenfile_core::Result<()> + Send + 'static,
    {
        let tx = self.io_tx.clone();
        let hwnd_raw = self.hwnd.0 as usize;
        let label = label.to_string();
        self.io_pending += 1;
        std::thread::spawn(move || {
            // IFileOperation 的前置条件是 STA，而且 COM 要**每线程**各自初始化。
            let _ = ComGuard::init();
            let r = job();
            let msg = match r {
                Ok(()) => String::new(),
                Err(e) => format!("{label}失败：{e}"),
            };
            let _ = tx.send(msg);
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(windows::Win32::Foundation::HWND(hwnd_raw as *mut core::ffi::c_void)),
                    crate::WM_APP_IO_DONE,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(0),
                );
            }
        });
    }

    /// 后台枚举回收站。UI 线程只负责发一趟活，扫描本身在 worker 上。
    ///
    /// 与目录枚举同理：`$Recycle.Bin` 的直读会摸到每个盘，慢的时候几百毫秒，
    /// 放在 `reload()` 里就是"点一下回收站要愣一下"。
    fn spawn_recycle_scan(&mut self) {
        // 上一趟还没回来就不再派新的：连续刷新只会堆线程，不会更快。
        if self.recycle_rx.is_some() {
            let gen = self.next_generation();
            let tab = self.tab_mut();
            tab.generation = gen;
            tab.entries.clear();
            tab.visible.clear();
            return;
        }
        self.recycle_gen += 1;
        let gen = self.recycle_gen;
        // 分批到达是**追加**，所以起新一趟之前必须先把旧的清掉，
        // 否则刷新一次条目就翻一倍。
        self.recycle_items.clear();
        self.recycle_sel.clear();
        let (tx, rx) = std::sync::mpsc::channel();
        self.recycle_rx = Some(rx);
        let hwnd_raw = self.hwnd.0 as usize;
        std::thread::spawn(move || {
            // COM 必须**每线程**各自初始化：扫描过程要问 Shell 拿显示名和盘符信息。
            // UI 线程上这些都靠启动时那次 CoInitialize 兜着，新开的线程没有 ——
            // 不补上就是"回收站打开后一条都不显示"。
            let _guard = ComGuard::init();
            // 分盘回调：扫完一个盘就发一批，界面能边扫边显示，
            // 不用等最后一个盘扫完才出第一条（几万条时要等好几秒）。
            zenfile_core::recycle::list_streaming(|batch| {
                let _ = tx.send(batch);
                unsafe {
                    windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                        Some(windows::Win32::Foundation::HWND(hwnd_raw as *mut core::ffi::c_void)),
                        crate::WM_APP_RECYCLE,
                        windows::Win32::Foundation::WPARAM(gen as usize),
                        windows::Win32::Foundation::LPARAM(0),
                    );
                }
            });
            // 空批 = 扫完了
            let _ = tx.send(Vec::new());
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(windows::Win32::Foundation::HWND(hwnd_raw as *mut core::ffi::c_void)),
                    crate::WM_APP_RECYCLE,
                    windows::Win32::Foundation::WPARAM(gen as usize),
                    windows::Win32::Foundation::LPARAM(1),
                );
            }
        });
        // 先把视图切成"正在读"：旧的条目不清（避免闪一下空白），
        // 只把 loading 点亮，进度条会动。
        let gen = self.next_generation();
        let tab = self.tab_mut();
        tab.generation = gen;
        tab.entries.clear();
        tab.visible.clear();
        tab.loading = true;
    }

    /// `WM_APP_RECYCLE` 的收口：把后台枚举到的条目装进视图。
    ///
    /// 世代号对不上说明这次结果已经过期（用户早就切走了），直接丢。
    ///
    /// `done`（LPARAM=1）表示这是最后一趟，此时才收起进度条并把结果整体排序。
    pub fn drain_recycle(&mut self, gen: u64, done: bool) {
        let Some(rx) = self.recycle_rx.take() else {
            return;
        };
        // 世代号对不上说明这一趟已经作废（早就切走了），连同通道一起丢掉
        if gen != self.recycle_gen {
            return;
        }
        while let Ok(batch) = rx.try_recv() {
            self.recycle_items.extend(batch);
        }
        if done {
            // 分批到达时每批内部有序、批与批之间没有，最后统一排一次
            self.sort_recycle_items();
            let gen = self.next_generation();
            let tab = self.tab_mut();
            tab.generation = gen;
            tab.entries.clear();
            tab.visible.clear();
            tab.loading = false;
        } else {
            self.recycle_rx = Some(rx);
        }
    }

    /// 按当前 `recycle_sort` 重排回收站条目。
    fn sort_recycle_items(&mut self) {
        let (col, desc) = self.recycle_sort;
        self.recycle_items.sort_by(|a, b| {
            let ord = match col {
                0 => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                1 => a.original.to_string_lossy().to_lowercase().cmp(&b.original.to_string_lossy().to_lowercase()),
                2 => a.deleted.cmp(&b.deleted),
                _ => a.size.cmp(&b.size),
            };
            if desc { ord.reverse() } else { ord }
        });
    }

    /// 点回收站列头：切到这一列；再点一次换方向。
    pub fn sort_recycle_by(&mut self, col: usize) {
        if self.recycle_sort.0 == col {
            self.recycle_sort.1 = !self.recycle_sort.1;
        } else {
            // 新列默认方向：时间/大小从大到小更符合"先看最近的"，其余升序
            self.recycle_sort = (col, matches!(col, 2 | 3));
        }
        self.sort_recycle_items();
        self.invalidate();
    }

    /// 设置/清除"按列筛选"，并立刻重算可见项。
    pub fn set_col_filter(&mut self, f: Option<ColFilter>) {
        self.tab_mut().col_filter = f;
        let (hidden, system) = (self.show_hidden, self.show_system);
        if self.tab().search_root.is_some() {
            self.tab_mut().recompute_visibility(hidden, system);
        } else {
            self.tab_mut().recompute_filter(hidden, system);
        }
        self.tab_mut().scroll = 0;
        self.focus_visible = false;
        self.invalidate();
    }

    /// 拖拽落地：把 `items` 放进 `dest`。
    ///
    /// `copy` 为假时是移动，跨卷自动退化成复制（与资源管理器一致：不同盘之间
    /// 默认是复制而不是移动）。
    pub fn drop_items(&mut self, dest: PathBuf, items: Vec<PathBuf>, copy: bool) {
        if items.is_empty() || dest.as_os_str().is_empty() {
            return;
        }
        // 拖到自己所在的目录 = 什么都没发生；拖到自己身上更是无意义
        let cur = self.tab().path.clone();
        if dest == cur {
            return;
        }
        let items: Vec<PathBuf> = items
            .into_iter()
            .filter(|p| p.parent().map(|q| q != dest).unwrap_or(true))
            .collect();
        if items.is_empty() {
            return;
        }
        let cross = !same_volume(&items[0], &dest);
        let mode = if copy || cross { ClipMode::Copy } else { ClipMode::Move };
        // 关键：**不在松手这条消息里真的去搬文件**。自研引擎自己开线程，
        // 完成后由 `drain_transfer` 触发 reload（曾因为在这里同步跑
        // `IFileOperation`，它的模态循环把 WndProc 重入进 `&mut App`，
        // 表现为"拖完一松手就僵住"）。
        let kind = if mode == ClipMode::Move {
            zenfile_core::transfer::Kind::Move
        } else {
            zenfile_core::transfer::Kind::Copy
        };
        self.start_transfer(kind, dest, items);
        self.invalidate();
    }

    /// 拖拽到另一个标签页不适用时的替代：粘贴到指定目录（供外部调用）。
    pub fn paste_into(&mut self, dest: &Path, files: Vec<PathBuf>, mode: ClipMode) {
        let kind = if mode == ClipMode::Move {
            zenfile_core::transfer::Kind::Move
        } else {
            zenfile_core::transfer::Kind::Copy
        };
        self.start_transfer(kind, dest.to_path_buf(), files);
    }

    // ------------------------------------------------------------ 右键菜单

    pub fn show_context_menu(&mut self, x: i32, y: i32, extended: bool) {
        // 右键菜单的作用对象**必须**来自右键那一刻的命中判定，不能用
        // `selected_paths()`：它在选中为空时会退回焦点行，于是在空白处右键
        // 仍然会拿到上一次那个条目（右键菜单"判定到条目上"的根因）。
        let items = self.context_targets.clone().unwrap_or_default();
        let cwd = self.tab().path.clone();
        let parent = items
            .first()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| cwd.clone());

        if extended {
            if !menu::show_native(self.hwnd, &parent, &items) {
                report(self.hwnd, "无法调起原生菜单");
            }
            return;
        }

        // 空白处右键要问的是**这个目录的背景菜单**（第三方扩展都挂在那儿），
        // 选中了东西才走"对这些条目"的那条路。
        self.bridge = if items.is_empty() {
            menu::build_background_bridge(self.hwnd, &cwd)
        } else {
            menu::build_shell_bridge(self.hwnd, &parent, &items, false)
        };

        let kind = menu_kind_of(&items);
        let mut entries = own_menu_rows(self, &items, kind);
        if let Some(bridge) = &self.bridge {
            let mut system = menu::entries_from_bridge(bridge);
            // 去掉与自有条目重名的一级项：Shell 菜单里也有"打开/剪切/复制/重命名/
            // 删除/属性"，全列出来就是两个"打开"，看着像 bug。
            let owned: Vec<String> = entries.iter().map(|e| menu::menu_key(&e.text)).collect();
            system.retain(|e| !owned.iter().any(|o| *o == menu::menu_key(&e.text)));
            if !system.is_empty() {
                entries.push(menu::MenuEntry::sep());
                entries.extend(system);
            }
        }
        // 再整体去重一遍：Shell 自己内部也会给重复的"打开"（不同扩展各贡献一条）
        entries = dedupe_rows(entries);
        entries.push(menu::MenuEntry::sep());
        entries.push(shell_row("显示更多选项（原生菜单）", None, Command::ShowNativeMenu));

        let app_ptr = self as *mut App as *mut u32;
        if let Some(hwnd_menu) = menu::open_menu(
            app_ptr as *mut App,
            self.hwnd,
            entries,
            x,
            y,
            self.metrics.scale,
            self.theme,
            None, // 顶层菜单：没有父级菜单窗口
            false, // 右键弹出：不存在"同一次点击的抬起"
        ) {
            self.menu_hwnd = Some(hwnd_menu);
        }
    }

    /// 回收站视图里的右键菜单。
    pub fn show_recycle_menu(&mut self, x: i32, y: i32) {
        let entries = vec![
            shell_row("还原", Some("\u{E7A7}"), Command::RecycleRestore),
            shell_row("彻底删除", Some("\u{E74D}"), Command::RecycleDelete),
            menu::MenuEntry::sep(),
            shell_row("全选", Some("\u{E762}"), Command::RecycleSelectAll),
            menu::MenuEntry::sep(),
            shell_row("清空回收站", Some("\u{E74D}"), Command::RecycleEmpty),
            menu::MenuEntry::sep(),
            shell_row("刷新", Some("\u{E72C}"), Command::Refresh),
        ];
        let app_ptr = self as *mut App;
        if let Some(h) = menu::open_menu(
            app_ptr,
            self.hwnd,
            entries,
            x,
            y,
            self.metrics.scale,
            self.theme,
            None,
            false,
        ) {
            self.menu_hwnd = Some(h);
        }
    }

    pub fn notify_menu_closed(&mut self, hwnd: HWND) {
        if self.menu_hwnd == Some(hwnd) {
            self.menu_hwnd = None;
            self.bridge = None;
            // 菜单关了：光标还压在主窗口上却没人来设它（菜单期间主窗口不接
            // WM_SETCURSOR），鼠标不动的话就一直挂着菜单里的那一支。这里补一次。
            let owner = self.hwnd;
            unsafe { crate::apply_cursor(owner, self) };
        }
    }

    /// 应用失焦时把还开着的菜单收掉。
    ///
    /// 自绘菜单是 `SW_SHOWNOACTIVATE` 的弹出窗口，**从不拿焦点**，所以 alt+tab
    /// 切走时它收不到 WM_KILLFOCUS（那条路只在真的失焦时才走）—— 表现就是
    /// "切到别的软件了，右键菜单还挂在屏幕上"。改由宿主的 WM_ACTIVATEAPP 收口。
    pub fn dismiss_menus(&mut self) {
        if let Some(h) = self.menu_hwnd.take() {
            if unsafe { IsWindow(Some(h)) }.as_bool() {
                unsafe {
                    let _ = DestroyWindow(h);
                }
            }
            self.bridge = None;
            let owner = self.hwnd;
            unsafe { crate::apply_cursor(owner, self) };
        }
    }

    /// 鼠标整体离开窗口：把所有 hover_* 一次清干净。
    ///
    /// 以前只有标题栏三大金刚键的 hover 在 WM_MOUSELEAVE 那条路上清，于是
    /// "鼠标都移出窗口了，内容区那一行还亮着、分隔线还保持高亮、侧栏的行也还
    /// 反着白" —— 回到窗口之前画面一直是旧的。
    ///
    /// **只清悬停态**：拖拽是按下之后才开始的，鼠标划出窗口不该把它打断
    /// （拖到窗口外面松手仍要能落地）。光标不归这里管，见 `apply_cursor`。
    pub fn clear_hover(&mut self) {
        self.settings_prev_hover = None;
        self.hover_caption = 0;
        self.pressed_caption = 0;
        self.hovering_close = false;
        self.hover_chrome = 0;
        self.hover_tab = None;
        self.hover_tab_close = None;
        self.hover_new_tab = false;
        self.hover_view_btn = 0;
        self.settings_hover = None;
        self.settings_hover_tab = None;
        self.settings_hover_card = None;
        self.settings_hover_menu = None;
        self.settings_hover_tree = None;
        self.sidebar_hover = None;
        self.hover_sidebar_collapse = false;
        self.addr_hover = None;
        self.hover_split = None;
        self.hover_pane_div = None;
        self.hover_bulk = false;
        self.hover_row = None;
        self.hover_pane = None;
        self.hover_nav = 0;
        self.hover_header = None;
        self.hover_recycle = None;
        self.hover_recycle_btn = 0;
        self.tab_hover_since = None;
    }

    pub fn on_menu_activation(&mut self, action: MenuAction) {
        // 预览菜单（设置页右侧那些格式样本上弹出来的）**不真的执行**：
        // 它只是"这个类型的菜单长什么样"，点了也不该动任何东西。
        //
        // 反馈走预览面板标题那一行的临时提示，**不用 MessageBox**：预览区是
        // 让人反复点着看的地方，弹一个模态框出来既挡事又烦。
        if self.preview_menu {
            self.preview_menu = false;
            self.preview_note_at = unsafe {
                windows::Win32::System::SystemInformation::GetTickCount64()
            };
            let _ = unsafe { windows::Win32::Graphics::Gdi::InvalidateRect(Some(self.hwnd), None, true) };
            return;
        }
        match action {
            MenuAction::Own(cmd) => self.run_command(cmd),
            MenuAction::Shell(offset) => {
                let owner = self.hwnd;
                if let Some(bridge) = &self.bridge {
                    if !menu::invoke(bridge, owner, offset) {
                        report(self.hwnd, "菜单项执行失败");
                    }
                }
                self.reload();
            }
            MenuAction::None => {}
        }
    }

    /// 对当前选中项执行「固定到…」。
    ///
    /// 失败要给话说清楚：这三项依赖 Shell 的 verb，不同 Windows 版本给的 verb
    /// 不一样（实测 Win10 LTSC 19044 只有 `PinToStartScreen` 真在菜单里），静悄悄
    /// 失败最容易被当成"菜单坏了"。
    fn pin_selected(&mut self, target: menu::PinTarget) {
        let items = self
            .context_targets
            .clone()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| self.tab().selected_paths());
        let Some(first) = items.first().cloned() else {
            report(self.hwnd, "没有选中任何项目");
            return;
        };
        let parent = first
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.tab().path.clone());
        let one = vec![first];
        if menu::pin_invoke(self.hwnd, &parent, &one, target) {
            return;
        }
        report(
            self.hwnd,
            match target {
                menu::PinTarget::Taskbar => "当前系统不支持“固定到任务栏”",
                menu::PinTarget::StartMenu => "当前系统不支持“固定到开始菜单”",
                menu::PinTarget::StartScreen => "当前系统不支持“固定到开始屏幕”",
            },
        );
    }

    pub fn run_command(&mut self, cmd: Command) {
        match cmd {
            Command::Open => self.open_focused(),
            Command::OpenWith => {
                let ptr = self as *mut App;
                // 选中为空 = 空白处右键 → 目标是**当前目录**；
                // 有选中就对第一个文件问（资源管理器也是这么做的）；
                // 全是文件夹/盘符 → 用目录模式（VS Code / 终端 / 资源管理器…）。
                //
                // 注意：这里**不能**用 `selected_paths()` —— 它在选中为空时会
                // 退回焦点行，于是空白处右键会莫名其妙打开某个文件。
                if self.tab().selected.is_empty() {
                    let dir = self.tab().path.clone();
                    if !is_virtual(&dir) {
                        crate::openwith::open_dir(ptr, dir);
                    }
                } else {
                    let sel = self.tab().selected_paths();
                    let file = sel.iter().find(|p| p.is_file()).cloned();
                    if let Some(p) = file {
                        crate::openwith::open(ptr, p);
                    } else if let Some(d) = sel.into_iter().next() {
                        crate::openwith::open_dir(ptr, d);
                    }
                }
            }
            Command::Copy => self.clipboard_write(ClipMode::Copy),
            Command::Cut => self.clipboard_write(ClipMode::Move),
            Command::Paste => self.paste(),
            Command::Rename => self.begin_rename(),
            Command::BulkRename(mode) => {
                let items = self.tab().selected_paths();
                if items.is_empty() {
                    report(self.hwnd, "没有选中任何项目：先选中要改名的文件");
                    return;
                }
                let ptr = self as *mut App;
                crate::rename::open(ptr, items, mode);
            }
            Command::Delete => self.delete_selected(false),
            Command::DeletePermanent => self.delete_selected(true),
            Command::NewFolder => self.new_folder(),
            Command::NewFile => self.new_file(),
            Command::NewTemplate(ext) => self.new_from_template(&ext),
            Command::SelectAll => self.select_all(),
            Command::Refresh => self.reload(),
            Command::ViewModeCycle => self.cycle_view(),
            Command::ShowNativeMenu => {
                let items = self.tab().selected_paths();
                let parent = items
                    .first()
                    .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                    .unwrap_or_else(|| self.tab().path.clone());
                // 不能再在这里 reload()：原生菜单是模态的，它关闭之后用户点的
                // "空白处"会被当成"刷新一下"——表现就是"点一下空白，文件夹内容
                // 又重新加载了一遍"。目录变更本来就有 ReadDirectoryChangesW 监听，
                // Shell 真的改了东西会自己通知过来。
                let _ = menu::show_native(self.hwnd, &parent, &items);
            }
            Command::Properties => {
                // 无选中项时打开"当前文件夹"的属性，与资源管理器空白处右键一致。
                // 目标取自右键那一刻的命中结果（没有则退回当前选中/焦点行），
                // 否则空白处右键会把属性开在别的条目上。
                let target = self
                    .context_targets
                    .clone()
                    .unwrap_or_default()
                    .first()
                    .cloned()
                    .or_else(|| self.tab().selected_paths().first().cloned())
                    .unwrap_or_else(|| self.tab().path.clone());
                if crate::app::is_home_path(&target) {
                    report(self.hwnd, "“主页”是虚拟视图，没有对应的文件夹属性");
                } else {
                    // 自绘属性对话框：不走 ShellExecuteEx("properties")，
                    // 那个路径要加载属性页扩展、构建属性存储，慢一个数量级。
                    crate::props::open(self as *mut App, target);
                }
            }
            Command::PinTaskbar => self.pin_selected(menu::PinTarget::Taskbar),
            Command::PinStartMenu => self.pin_selected(menu::PinTarget::StartMenu),
            Command::PinStartScreen => self.pin_selected(menu::PinTarget::StartScreen),
            Command::ViewIcons => self.set_view(ViewMode::LargeIcons),
            Command::ViewList => self.set_view(ViewMode::List),
            Command::ViewDetails => self.set_view(ViewMode::Details),
            Command::SortByName => self.sort_column(zenfile_core::SortKey::Name),
            Command::SortByModified => self.sort_column(zenfile_core::SortKey::Modified),
            Command::SortByType => self.sort_column(zenfile_core::SortKey::Type),
            Command::SortBySize => self.sort_column(zenfile_core::SortKey::Size),
            // 只改方向：点"递减顺序"时不该把列也一起换掉（资源管理器同理）
            Command::SortAscending => self.set_sort_direction(false),
            Command::SortDescending => self.set_sort_direction(true),
            Command::ToggleSidebar => self.toggle_sidebar(),
            Command::ToggleStatusbar => self.toggle_statusbar(),
            Command::ToggleGpuRender => self.toggle_gpu_render(),
            Command::ToggleFollowCurrent => self.toggle_follow_current(),
            Command::ToggleThispcExpanded => self.toggle_thispc_expanded(),
            Command::ToggleHidden => self.toggle_hidden(),
            Command::ToggleSystem => self.toggle_system(),
            Command::ToggleExtensions => self.toggle_extensions(),
            Command::CycleSizeUnit => self.cycle_size_unit(),
            Command::SetSizeUnitIdx(i) => self.set_size_unit(i),
            Command::SetThemeDark(dark) => self.set_theme(dark),
            Command::SetDefaultViewIdx(i) => self.set_default_view(i),
            Command::ColFilter(col, bucket) => self.set_col_filter(if bucket == 0 {
                None
            } else {
                match col {
                    0 => Some(ColFilter::Name(bucket)),
                    1 => Some(ColFilter::Date(bucket)),
                    3 => Some(ColFilter::Size(bucket)),
                    _ => None,
                }
            }),
            Command::HeaderSort(sem, desc) => self.header_sort(sem, desc),
            // 「选择列…」：勾掉/勾上某一列。**只改当前目录**（资源管理器
            // 的列设置就是每个文件夹各一套），同时写回 bag 让资源管理器
            // 也认（第33轮）。
            Command::ToggleColumn(id) => {
                if !id.is_empty() {
                    let on = !self.is_col_visible(&id);
                    self.set_col_visible(&id, on);
                }
            }
            // 「其他(M)...」：资源管理器那套「选择详细信息」对话框（第53轮）。
            // 在这里开而不在 main.rs 开，是因为对话框要能改 **本目录** 的列集合，
            // 而那套读写（bag + 每目录配置）都收在 App 里。
            Command::ChooseColumns => crate::coldlg::open(self),
            Command::AutofitColumns | Command::AutofitAllColumns => self.autofit_cols(),
            Command::ColFilterKind(k) => {
                self.set_col_filter(if k.is_empty() { None } else { Some(ColFilter::Kind(k)) })
            }
            Command::CycleTheme => self.cycle_theme(),
            Command::CycleDefaultView => self.cycle_default_view(),
            Command::OpenSettings => self.open_settings(),
            Command::OpenRecycle => self.open_recycle(),
            Command::RecycleRestore => self.restore_recycle(),
            Command::RecycleDelete => self.purge_recycle(),
            Command::RecycleEmpty => self.empty_recycle(),
            Command::RecycleSelectAll => self.recycle_select_all(),
            Command::NewTab => self.new_tab(),
            // 新窗口开在本进程里（共享图标缓存 / 文件名索引 / 配置），
            // 这是"多开不吃资源"的做法。
            Command::NewWindow => crate::new_window(PathBuf::from(HOME_PATH)),
            Command::FocusWindow(i) => crate::focus_window(i),
            Command::QuitApp => unsafe { crate::close_all_windows() },
            // 「关于」不再弹 MessageBox：跳到设置页的「关于」标签（第 19 轮第 1 项）
            Command::About => self.open_settings_at(SETTINGS_TAB_ABOUT),
        }
    }

    /// 折叠/展开左侧导航窗格，并把结果写入用户层配置（§10 三层配置）。
    pub fn toggle_sidebar(&mut self) {
        self.cfg.ui.sidebar_visible = !self.cfg.ui.sidebar_visible;
        self.persist_ui_pref();
        crate::paint::reposition_chrome(self);
        let _ = unsafe { InvalidateRect(Some(self.hwnd), None, true) };
    }

    /// 显示/隐藏"隐藏的项目"。
    ///
    /// 与"受保护的操作系统文件"是**两道独立开关** —— 资源管理器在"查看 → 显示"
    /// 里也是分开的两项：隐藏属性是用户自己设的，系统属性是系统保护的，混成一个
    /// 开关会让想看隐藏文件的人被迫看见一堆系统文件。
    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.cfg.files.view.show_hidden = self.show_hidden;
        self.persist_view_pref();
        self.apply_filter();
    }

    pub fn toggle_system(&mut self) {
        self.show_system = !self.show_system;
        self.cfg.files.view.show_system = self.show_system;
        self.persist_view_pref();
        self.apply_filter();
    }

    /// 开关变化后只需要重算可见集，不用重新枚举磁盘
    fn apply_filter(&mut self) {
        let (sh, ss) = (self.show_hidden, self.show_system);
        let tab = self.tab_mut();
        tab.recompute_filter(sh, ss);
        let _ = unsafe { InvalidateRect(Some(self.hwnd), None, true) };
    }

    pub fn persist_view_pref(&self) {
        if let Err(e) = zenfile_core::config::Config::save_user_patch(&serde_json::json!({
            "files": {
                "view": {
                    "show_hidden": self.cfg.files.view.show_hidden,
                    "show_system": self.cfg.files.view.show_system,
                    "show_extensions": self.cfg.files.view.show_extensions,
                    "size_unit": self.cfg.files.view.size_unit,
                    "default_mode": self.cfg.files.view.default_mode,
                    // 拖过的列宽也要存：不存的话下次打开又回到默认，等于白拖
                    "col_name_w": self.cfg.files.view.col_name_w,
                    "col_date_w": self.cfg.files.view.col_date_w,
                    "col_kind_w": self.cfg.files.view.col_kind_w,
                    "col_size_w": self.cfg.files.view.col_size_w,
                }
            }
        })) {
            eprintln!("保存视图偏好失败: {e}");
        }
    }

    /// 拖过导航窗格分隔线之后把宽度记下来。
    pub fn persist_sidebar_width(&self) {
        if let Err(e) = zenfile_core::config::Config::save_user_patch(&serde_json::json!({
            "ui": { "sidebar_width": self.cfg.ui.sidebar_width }
        })) {
            eprintln!("保存导航窗格宽度失败: {e}");
        }
    }

    /// 显示/隐藏文件扩展名。资源管理器在"查看 → 显示/隐藏"里也给了这一项。
    pub fn toggle_extensions(&mut self) {
        self.cfg.files.view.show_extensions = !self.cfg.files.view.show_extensions;
        self.persist_view_pref();
        self.invalidate();
    }

    /// 切换"大小"列的默认单位：自动 → B → KB → MB → GB → TB → PB 循环。
    /// 设置页下拉里**直接选中**某一档（不再"点一下循环到下一个"）。
    ///
    /// 循环切换在只有两三个选项时还能用，选项一多（大小单位 7 档）就得连点
    /// 好几下才能到想要的那一档，而且不知道点下去会变成什么。下拉菜单一次看全。
    pub fn set_size_unit(&mut self, idx: usize) {
        let u = zenfile_core::SizeUnit::ALL.get(idx).copied().unwrap_or(zenfile_core::SizeUnit::Auto);
        self.cfg.files.view.size_unit = u.name().to_string();
        self.persist_view_pref();
        self.invalidate();
    }

    pub fn set_theme(&mut self, dark: bool) {
        let next = if dark { "dark" } else { "light" };
        self.cfg.core.theme = next.to_string();
        self.theme = if dark { theme::dark() } else { theme::light() };
        let _ = zenfile_core::config::Config::save_user_patch(&serde_json::json!({
            "core": { "theme": next }
        }));
        // 主题切换后图标系统若接管需重烘（optimized 套随明暗刷新缓存键）。
        self.apply_icon_theme();
        self.invalidate();
    }

    /// 图标主题：`default`（系统图标）/ `optimized`（自绘优化套）。
    pub fn set_icon_theme(&mut self, theme: &str) {
        if self.cfg.ui.icon_theme == theme {
            return;
        }
        self.cfg.ui.icon_theme = theme.to_string();
        self.persist_icon_prefs();
        self.apply_icon_theme();
        self.invalidate();
    }

    fn persist_icon_prefs(&self) {
        if let Err(e) = zenfile_core::config::Config::save_user_patch(&serde_json::json!({
            "ui": { "icon_theme": self.cfg.ui.icon_theme }
        })) {
            eprintln!("保存图标偏好失败: {e}");
        }
    }

    /// 把当前配置的图标主题落到图标系统上（启动 / 切换 / 换明暗时调）。
    pub fn apply_icon_theme(&mut self) {
        let dark = self.cfg.core.theme != "light";
        crate::icon_theme::set_theme(&self.cfg.ui.icon_theme, dark);
        self.icons.on_theme_changed();
    }

    pub fn set_default_view(&mut self, idx: usize) {
        let next = DEFAULT_VIEW_CYCLE.get(idx).copied().unwrap_or("system");
        self.cfg.files.view.default_mode = next.to_string();
        self.persist_view_pref();
        self.invalidate();
    }

    pub fn cycle_size_unit(&mut self) {
        let cur = zenfile_core::SizeUnit::parse(&self.cfg.files.view.size_unit);
        let i = zenfile_core::SizeUnit::ALL.iter().position(|u| *u == cur).unwrap_or(0);
        let next = zenfile_core::SizeUnit::ALL[(i + 1) % zenfile_core::SizeUnit::ALL.len()];
        self.cfg.files.view.size_unit = next.name().to_string();
        self.persist_view_pref();
        self.invalidate();
    }

    /// 切换明暗主题。
    pub fn cycle_theme(&mut self) {
        let next = if self.cfg.core.theme == "light" { "dark" } else { "light" };
        self.cfg.core.theme = next.to_string();
        self.theme = if next == "light" { theme::light() } else { theme::dark() };
        let _ = zenfile_core::config::Config::save_user_patch(&serde_json::json!({
            "core": { "theme": next }
        }));
        // 主题切换后图标系统若接管需重烘（同 set_theme）。
        self.apply_icon_theme();
        self.invalidate();
    }

    /// 切换默认视图模式（设置页用）。
    ///
    /// 注意存的是**配置名**（`details`/`system`…）而不是中文标签 —— 早先存标签
    /// 进去，`parse_view` 认不出来，于是每次循环都退回"详细信息"，看着像点了没反应。
    pub fn cycle_default_view(&mut self) {
        let cur = self.cfg.files.view.default_mode.as_str();
        let i = DEFAULT_VIEW_CYCLE.iter().position(|s| *s == cur).unwrap_or(0);
        self.cfg.files.view.default_mode = DEFAULT_VIEW_CYCLE[(i + 1) % DEFAULT_VIEW_CYCLE.len()].to_string();
        self.persist_view_pref();
        self.invalidate();
    }

    // ------------------------------------------------------------ 虚拟视图

    /// 以标签页形式打开设置页。已经开过就直接切过去，避免堆一串重复标签。
    pub fn open_settings(&mut self) {
        self.open_settings_at(self.settings_tab);
    }

    /// 打开设置页并停到指定标签（「关于」从这里进）。
    pub fn open_settings_at(&mut self, tab: u8) {
        if let Some(i) = self.pane().tabs.iter().position(|t| t.settings) {
            self.pane_mut().active = i;
        } else {
            self.open_tab(PathBuf::from(SETTINGS_PATH));
        }
        self.settings_tab = tab;
        self.settings_hover = None;
        crate::paint::sync_address(self);
        self.invalidate();
    }

    /// 打开回收站视图。
    pub fn open_recycle(&mut self) {
        if let Some(i) = self.pane().tabs.iter().position(|t| t.recycle) {
            self.pane_mut().active = i;
            self.reload();
        } else {
            self.open_tab(PathBuf::from(RECYCLE_PATH));
        }
        crate::paint::sync_address(self);
        self.invalidate();
    }

    /// 还原回收站里选中的项。
    pub fn restore_recycle(&mut self) {
        let picked: Vec<zenfile_core::recycle::Item> = self
            .recycle_items
            .iter()
            .filter(|it| self.recycle_sel.contains(&it.id))
            .cloned()
            .collect();
        if picked.is_empty() {
            return;
        }
        let n = zenfile_core::recycle::restore(&picked);
        report(self.hwnd, &format!("已还原 {n} 项"));
        self.reload();
    }

    /// 彻底删除回收站里选中的项。
    pub fn purge_recycle(&mut self) {
        let picked: Vec<zenfile_core::recycle::Item> = self
            .recycle_items
            .iter()
            .filter(|it| self.recycle_sel.contains(&it.id))
            .cloned()
            .collect();
        if picked.is_empty() {
            return;
        }
        if !confirm(self.hwnd, &format!("彻底删除这 {} 项？此操作不可撤销。", picked.len())) {
            return;
        }
        let n = zenfile_core::recycle::remove(&picked);
        report(self.hwnd, &format!("已彻底删除 {n} 项"));
        self.reload();
    }

    /// 清空回收站。
    pub fn empty_recycle(&mut self) {
        if !confirm(self.hwnd, "清空回收站？此操作不可撤销。") {
            return;
        }
        let _ = zenfile_core::recycle::empty();
        self.reload();
    }

    pub fn recycle_select_all(&mut self) {
        self.recycle_sel = self.recycle_items.iter().map(|i| i.id.clone()).collect();
        self.invalidate();
    }

    pub fn toggle_statusbar(&mut self) {
        self.cfg.ui.statusbar_visible = !self.cfg.ui.statusbar_visible;
        self.persist_ui_pref();
        crate::paint::reposition_chrome(self);
        let _ = unsafe { InvalidateRect(Some(self.hwnd), None, true) };
    }

    /// 导航窗格"跟随展开到当前文件夹"。
    ///
    /// 打开的那一刻立刻按当前目录补一次展开，否则开关点了要下次导航才见效，
    /// 用户会以为没生效。
    pub fn toggle_follow_current(&mut self) {
        self.cfg.ui.sidebar_follow_current = !self.cfg.ui.sidebar_follow_current;
        self.persist_ui_pref();
        if self.cfg.ui.sidebar_follow_current && !is_virtual(&self.tab().path.clone()) {
            let p = self.tab().path.clone();
            self.reveal_in_sidebar(&p);
        }
        let _ = unsafe { InvalidateRect(Some(self.hwnd), None, true) };
    }

    /// "此电脑"是否默认展开（展开后才看得见具体盘符）。
    pub fn toggle_thispc_expanded(&mut self) {
        self.cfg.ui.sidebar_thispc_expanded = !self.cfg.ui.sidebar_thispc_expanded;
        self.persist_ui_pref();
        if self.cfg.ui.sidebar_thispc_expanded {
            self.sidebar_expanded.insert(SIDEBAR_THISPC.to_string());
        }
        let _ = unsafe { InvalidateRect(Some(self.hwnd), None, true) };
    }

    /// 「使用 GPU 渲染」开关（第79轮）。
    ///
    /// 本轮**只接线**：翻转字段 → 落到 `cfg.ui.gpu_render` → 写回用户层 →
    /// 重绘。真正的渲染后端由后续任务读这个字段去分流（true 走 Direct2D，
    /// false 走现有 CPU 多线程 GDI 路径）。
    ///
    /// 关键取舍：**存用户层、不碰覆盖层**，与 `persist_view_pref` 同一套纪律 ——
    /// 设置页的写入永远是浅 patch，深合并由 `save_user_patch` 负责，
    /// 所以在这里把字段重复列进去不会吃掉别人写的其它 `ui` 键。
    pub fn toggle_gpu_render(&mut self) {
        self.cfg.ui.gpu_render = !self.cfg.ui.gpu_render;
        self.persist_ui_pref();
        self.invalidate();
    }

    /// 只把 UI 开关写回 settings.user.json，不覆盖其它字段。
    pub fn persist_ui_pref(&self) {
        if let Err(e) = zenfile_core::config::Config::save_user_patch(&serde_json::json!({
            "ui": {
                "sidebar_visible": self.cfg.ui.sidebar_visible,
                "statusbar_visible": self.cfg.ui.statusbar_visible,
                "sidebar_follow_current": self.cfg.ui.sidebar_follow_current,
                "sidebar_thispc_expanded": self.cfg.ui.sidebar_thispc_expanded,
                "gpu_render": self.cfg.ui.gpu_render,
            }
        })) {
            eprintln!("保存 UI 偏好失败: {e}");
        }
    }

    /// 标题栏最右侧「更多」按钮（§3 布局：最小化左侧）。
    pub fn show_more_menu(&mut self, x: i32, y: i32) {
        let mut entries = Vec::new();
        entries.push(shell_row("新建标签页", Some("\u{E8A7}"), Command::NewTab));
        entries.push(shell_row("刷新", Some("\u{E72C}"), Command::Refresh));
        entries.push(menu::MenuEntry::sep());

        let mut view_children = view_rows(self);
        view_children.push(menu::MenuEntry::sep());
        view_children.push(pick_row("隐藏的项目", Command::ToggleHidden, self.show_hidden));
        view_children.push(pick_row(
            "受保护的系统文件",
            Command::ToggleSystem,
            self.show_system,
        ));
        view_children.push(pick_row(
            "文件扩展名",
            Command::ToggleExtensions,
            self.cfg.files.view.show_extensions,
        ));
        entries.push(submenu_row("视图", Some("\u{E7B3}"), view_children));

        let sort_row = submenu_row("排序方式", Some("\u{E8CB}"), sort_rows(self));
        entries.push(sort_row);
        entries.push(menu::MenuEntry::sep());

        entries.push(shell_row(
            if self.cfg.ui.sidebar_visible { "隐藏导航窗格" } else { "显示导航窗格" },
            Some("\u{E700}"),
            Command::ToggleSidebar,
        ));
        entries.push(shell_row(
            if self.cfg.ui.statusbar_visible { "隐藏状态栏" } else { "显示状态栏" },
            Some("\u{E75B}"),
            Command::ToggleStatusbar,
        ));
        entries.push(menu::MenuEntry::sep());
        entries.push(shell_row("打开回收站", Some("\u{E74D}"), Command::OpenRecycle));
        // 设置：以标签页形式打开，所有开关都收在里面（§10 三层配置的用户层）
        entries.push(shell_row("设置", Some("\u{E713}"), Command::OpenSettings));
        entries.push(menu::MenuEntry::sep());
        entries.push(shell_row("关于 ZenFile", Some("\u{E946}"), Command::About));

        let app_ptr = self as *mut App;
        if let Some(hwnd_menu) = menu::open_menu(
            app_ptr,
            self.hwnd,
            entries,
            x,
            y,
            self.metrics.scale,
            self.theme,
            None, // 顶层菜单：没有父级菜单窗口
            // **不能**吞抬起：这个菜单是在宿主的 `WM_LBUTTONUP` 里弹出来的，
            // 那一次抬起已经处理完了，之后收到的第一个 up 就是用户真正的点击。
            // 设成 true 的话菜单会把用户第一次点击整个吞掉 —— 表现是「更多」
            // 菜单弹出来之后怎么点都没反应（实测点「设置」毫无动静）。
            false,
        ) {
            self.menu_hwnd = Some(hwnd_menu);
        }
    }

    // ------------------------------------------------------------ 模块与配置

    /// 启用/停用一个子模块（设置页「插件」标签用）。
    ///
    /// 只改用户层配置并落盘；已经跑着的进程不动它 —— 下一次启动才按新开关拉起，
    /// 这比"边跑边杀" predictable 得多。
    pub fn set_module_enabled(&mut self, id: &str, on: bool) {
        if let Some(m) = self.cfg.modules.get_mut(id) {
            m.enabled = on;
        }
        let _ = zenfile_core::config::Config::save_user_patch(&serde_json::json!({
            "modules": { id: { "enabled": on } }
        }));
        self.invalidate();
    }

    pub fn spawn_modules(&mut self, exe_dir: &Path) {
        let mut tree = match ProcessTree::new() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("无法创建 Job Object: {e}");
                return;
            }
        };
        let mut spawned = Vec::new();
        for (id, m) in &self.cfg.modules {
            if !m.enabled {
                continue;
            }
            let exe = exe_dir.join(format!("fm-{id}.exe"));
            if exe.exists() {
                match tree.spawn::<&str>(&exe, &["--module"]) {
                    Ok(pid) => spawned.push((id.clone(), pid)),
                    Err(e) => eprintln!("模块 {id} 启动失败: {e}"),
                }
            }
        }
        if !spawned.is_empty() {
            println!("已拉起 {} 个子模块（子进程将折叠在宿主之下）", spawned.len());
        }
        self.tree = Some(tree);
    }

    /// 搜索框空态的提示文字：跟资源管理器一样带上当前位置（"搜索 桌面"）。
    pub fn search_hint(&self) -> String {
        let tab = self.tab();
        if tab.home {
            return "搜索 此电脑".to_string();
        }
        if tab.recycle {
            return "搜索 回收站".to_string();
        }
        if tab.settings {
            return "搜索设置".to_string();
        }
        let name = tab
            .path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                // 盘符根目录 C:\ 没有 file_name，退回 "C:"
                tab.path
                    .to_string_lossy()
                    .trim_end_matches('\\')
                    .to_string()
            });
        format!("搜索 {name}")
    }

    /// 地址框当前**应该显示**的文本（第75轮：从 `sync_address` 里原样提出来）。
    ///
    /// 主页那一页（常用文件夹 + 本机驱动器）在资源管理器里地址栏写的就是"主页"，
    /// 侧栏虽然把首页并到了"此电脑"这一格，但**地址栏要标的是"这是哪一页"**，
    /// 不是"从哪点进来的"，所以这里写"主页"。
    pub fn address_display_text(&self) -> String {
        address_display_text_for(self.tab())
    }

    /// 算出"搜索框当前画在它自己身上的内容"的快照（第75轮）。
    ///
    /// `search_hint()` 的依赖只有**当前标签**：home / recycle / settings 三个
    /// 标志先短路，剩下取 `tab().path` 的末段（盘符根退回 "C:"）。也就是说
    /// 提示词既跟着标签变，也跟着**当前目录名**变 —— 快照判据天然同时覆盖
    /// 这两者，不需要额外区分"切标签"和"导航"。
    ///
    /// 另三项（墨色 / 缩放 / 图标主题）是 `paint_search_decor` 里另外读到的
    /// 状态，不在这句提示词里，但同样决定这块像素 —— 一并列进来才是完整的
    /// 依赖集。详见 `ChildEditKey` 的注释。
    pub fn search_child_key(&self) -> ChildEditKey {
        ChildEditKey {
            hint: self.search_hint(),
            text_dim: self.theme.text_dim,
            // f32 不能直接进 Eq，用位模式：scale 每次都从 dpi/96 重算，
            // 位模式相同就等价于数值相同。
            scale_bits: self.metrics.scale.to_bits(),
            icon_theme: self.cfg.ui.icon_theme.clone(),
            visible: true, // 搜索框常驻可见（创建时带 WS_VISIBLE 且从不隐藏）
        }
    }

    /// 地址框的同类快照（第75轮）。`hint` 是它显示的文本 —— 即
    /// `sync_address` 算出来的"主页 / 回收站 / 设置 / 去掉 \?\ 前缀的路径"。
    ///
    /// `visible` 取 `app.addr_editing` 而不是 `IsWindowVisible(hwnd)`：它是
    /// 同一件事的纯状态侧记，`begin_address_edit` / `end_address_edit` 会成对
    /// 维护它，判据不必去问系统。地址框平时 `SW_HIDE`（`create_children`），
    /// 只有进编辑态才 `SW_SHOW`（`begin_address_edit`）。
    pub fn address_child_key(&self) -> ChildEditKey {
        ChildEditKey {
            hint: self.address_display_text(),
            text_dim: self.theme.text,
            scale_bits: self.metrics.scale.to_bits(),
            icon_theme: self.cfg.ui.icon_theme.clone(),
            visible: self.addr_editing,
        }
    }

    /// 现在是不是"有事在跑"：正在列目录、正在搜索、正在建索引。
    /// 目录栏下沿的进度条只看这一个开关。
    pub fn busy(&self) -> bool {
        if self.tab().loading || self.index_pending || self.io_pending > 0 {
            return true;
        }
        matches!(self.index_state, IndexState::Loading | IndexState::Building(_))
    }

    pub fn status_text(&self) -> String {
        let tab = self.tab();
        if tab.recycle {
            let bytes: u64 = self.recycle_items.iter().map(|i| i.size).sum();
            let unit = self.cfg.files.view.size_unit_of();
            return if self.recycle_sel.is_empty() {
                format!(
                    "回收站 · {} 项 · 共 {}",
                    self.recycle_items.len(),
                    human_unit(bytes, unit)
                )
            } else {
                let sel_bytes: u64 = self
                    .recycle_items
                    .iter()
                    .filter(|i| self.recycle_sel.contains(&i.id))
                    .map(|i| i.size)
                    .sum();
                format!(
                    "回收站 · {} 项 · 已选 {} 项 {}",
                    self.recycle_items.len(),
                    self.recycle_sel.len(),
                    human_unit(sel_bytes, unit)
                )
            };
        }
        if tab.settings {
            return "设置 · 改动即时写入 %APPDATA%\\ZenFile\\settings.user.json".to_string();
        }
        // 索引在建/在载入时给一句可见的话：整卷第一次搜索会花几秒，
        // 界面上什么都不说，用户只会以为"搜索框坏了"。
        let index_note = match &self.index_state {
            IndexState::Loading => " · 正在载入卷索引…".to_string(),
            IndexState::Building(n) => format!(" · 正在建立卷索引（{} 项）…", n),
            IndexState::Failed(why) => format!(" · 索引不可用（{why}）"),
            _ => String::new(),
        };
        let total = tab.entries.len();
        let visible = tab.visible.len();
        let sel = tab.selected.len();
        let bytes = tab.selected_bytes();
        let view = tab.view_mode.label();
        let unit = self.cfg.files.view.size_unit_of();
        // 被两道开关过滤掉的项要显式说一声：不然用户会以为文件真的没了
        let mut hidden_note = String::new();
        if total > visible {
            hidden_note = format!(" · 已隐藏 {} 项", total - visible);
        }
        // 搜索范围要说明白：搜当前目录和搜整个卷的差别人眼看不出来，
        // 不给提示用户会以为"结果就这么点，软件没找到"。
        let scope_note = match tab.search_root.as_ref() {
            None => String::new(),
            Some(p) => {
                let s = p.to_string_lossy().to_string();
                if zenfile_core::index::volume_root(p).as_deref() == Some(s.as_str()) {
                    // 把命中数与耗时摆出来：这既是"秒搜"的证据，也方便一眼看出
                    // 是索引查的还是临时兜的。
                    if self.search_ms > 0.0 {
                        format!(
                            " · 整卷「{}」{} 项 / {:.1} ms",
                            tab.filter, self.search_hits, self.search_ms
                        )
                    } else {
                        " · 整卷搜索".to_string()
                    }
                } else {
                    " · 仅当前目录（卷索引尚未就绪）".to_string()
                }
            }
        };
        // 状态栏**不显示目录**：路径已经在地址栏里了，再写一遍只会把状态文本
        // 挤得老长，还容易和右侧的视图切换按钮打架。
        if sel > 0 {
            format!(
                "{visible} / {total} 个项目{hidden_note}{index_note}{scope_note} · 已选 {sel} 项 {bytes} · 视图 {view}",
                bytes = human_unit(bytes, unit),
            )
        } else {
            format!("{visible} / {total} 个项目{hidden_note}{index_note}{scope_note} · 视图 {view}")
        }
    }

    /// 展开状态键：一律大写。
    fn node_key(path: &Path) -> String {
        path.to_string_lossy().to_uppercase()
    }

    fn is_expanded(&self, key: &str) -> bool {
        self.sidebar_expanded.contains(key)
    }

    /// 读取某目录的**直接子目录**清单并存进缓存。
    ///
    /// 只在展开/刷新的那一刻读盘，不在每帧绘制时读 —— 树深一层就等于每帧多一次
    /// `read_dir`，叠几层下来滚动就卡了。
    fn load_sidebar_children(&mut self, key: &str) {
        let children = std::fs::read_dir(key)
            .map(|rd| {
                let mut v: Vec<PathBuf> = rd
                    .flatten()
                    .filter(|e| {
                        let Ok(m) = e.metadata() else { return false };
                        if !m.is_dir() {
                            return false;
                        }
                        // 隐藏/系统项跟随两道开关；Recycle.Bin、System Volume
                        // Information 这类受保护容器永远不列：列出来只会让用户
                        // 在树里点到一堆打不开的东西。
                        use std::os::windows::fs::MetadataExt;
                        let attrs = m.file_attributes();
                        let hidden = attrs & 0x2 != 0;
                        let system = attrs & 0x4 != 0;
                        (!hidden || self.show_hidden) && (!system || self.show_system)
                    })
                    .map(|e| e.path())
                    .collect();
                v.sort_by(|a, b| {
                    zenfile_core::natural::natural_cmp(
                        &a.file_name().unwrap_or_default().to_string_lossy(),
                        &b.file_name().unwrap_or_default().to_string_lossy(),
                    )
                });
                v
            })
            .unwrap_or_default();
        self.sidebar_children.insert(key.to_string(), children);
    }

    /// 展开 / 收起一个导航节点。**不做导航**：点箭头就只该动树，不该换目录。
    pub fn toggle_sidebar_node(&mut self, key: &str) {
        if self.sidebar_expanded.contains(key) {
            // 收起：子清单一并丢弃，下次展开重新读盘（目录内容早变了）
            self.sidebar_expanded.remove(key);
            self.sidebar_children.remove(key);
            return;
        }
        self.sidebar_expanded.insert(key.to_string());
        if key != SIDEBAR_THISPC {
            self.load_sidebar_children(key);
        }
    }

    /// 导航窗格滚动。偏移按"整棵树的内容高度 vs 窗格高度"钳制，滚到底不会留空白。
    pub fn scroll_sidebar(&mut self, delta: i32, l: &crate::paint::Layout) {
        let row_h = crate::paint::sidebar_row_h(self.metrics.scale);
        let total = self.sidebar_rows().len() as i32 * row_h;
        let view_h = l.sidebar.bottom - l.sidebar.top - row_h;
        let max = (total - view_h).max(0);
        self.sidebar_scroll = (self.sidebar_scroll - delta).clamp(0, max);
        let _ = unsafe { InvalidateRect(Some(self.hwnd), None, false) };
    }

    /// 树收起/展开后把滚动偏移重新钳回合法范围（否则会停在空白区）。
    pub fn clamp_sidebar_scroll(&mut self, l: &crate::paint::Layout) {
        self.scroll_sidebar(0, l);
    }

    /// 保证某路径的所有祖先都处于展开状态（"跟随展开到当前文件夹"用）。
    ///
    /// 必须逐级 `load_sidebar_children`：只把 key 塞进集合，树依旧是空的 ——
    /// 展开标记和子清单是两件事，缺一个这一层就画不出来。
    pub fn reveal_in_sidebar(&mut self, path: &Path) {
        let full = path.to_string_lossy().to_uppercase();
        for drive in self.drives.keys().cloned().collect::<Vec<_>>() {
            let Some(rest) = full.strip_prefix(drive.as_str()) else { continue };
            self.sidebar_expanded.insert(SIDEBAR_THISPC.to_string());
            // **盘符自己那一层也要展开**（第57轮）：节点的 key 是 `C:\`
            // （带反斜杠，`node_key(PathBuf::from(root))`），而下面循环里累计的
            // `acc` 从 `C:` 起、**不带**反斜杠 —— 少了这一行，盘符节点永远
            // 不在展开集合里，树里就只有"此电脑"一个空箭头，"跟随展开到当前
            // 文件夹"等于什么都没做。
            self.sidebar_expanded.insert(drive.clone());
            let mut acc = drive.trim_end_matches('\\').to_string();
            for comp in rest.split('\\') {
                if comp.is_empty() {
                    continue;
                }
                acc = format!("{acc}\\{comp}");
                if acc.eq_ignore_ascii_case(&full) {
                    break;
                }
                self.sidebar_expanded.insert(acc.clone());
                self.load_sidebar_children(&acc);
            }
            break;
        }
    }

    /// 导航窗格当前要画的那些行（扁平序列，`depth` 决定缩进）。
    ///
    /// 渲染与命中测试共用这一份排列：不存在"看得见却点不到"的行。
    pub fn sidebar_rows(&mut self) -> Vec<SidebarRow> {
        let mut rows = Vec::new();
        // 顺序按用户要的来：**常用文件夹在最上面**，然后是"此电脑 + 盘符"，
        // 回收站垫底。之前"此电脑"排第一，于是每次开软件第一眼看到的是盘符，
        // 而真正高频的桌面/下载被挤到中间去了。
        for (label, path) in user_folders() {
            rows.push(SidebarRow {
                label: label.to_string(),
                target: path.clone(),
                depth: 0,
                expandable: false,
                expanded: false,
                key: String::new(),
                icon: SidebarIcon::Path(PathBuf::from(path)),
                guides: Vec::new(),
                last_child: false,
                has_parent: false,
                spacer: false,
            });
        }
        // 常用文件夹与"此电脑"这一组之间留一格
        rows.push(SidebarRow::spacer());
        // "此电脑"：展开 = 本机全部驱动器（含 U 盘、光驱）。
        let thispc = self.is_expanded(SIDEBAR_THISPC);
        rows.push(SidebarRow {
            label: "此电脑".to_string(),
            target: HOME_PATH.to_string(),
            depth: 0,
            expandable: true,
            expanded: thispc,
            key: SIDEBAR_THISPC.to_string(),
            icon: SidebarIcon::Stock(windows::Win32::UI::Shell::SIID_DESKTOPPC.0),
            guides: Vec::new(),
            last_child: false,
            has_parent: false,
            spacer: false,
        });
        // 盘符**紧跟**"此电脑"：之前它们被排在常用文件夹之后，于是这一层缩进
        // 的行跟自己的父节点中间隔了七八行，看着就是"此电脑下面什么都没有，
        // 下面那些缩进行不知道是谁的孩子"。
        if thispc {
            let mut all: Vec<_> = self.drives.values().cloned().collect();
            all.sort_by(|a, b| a.letter.cmp(&b.letter));
            let n = all.len();
            for (di, d) in all.into_iter().enumerate() {
                let key = Self::node_key(&PathBuf::from(&d.root));
                let expanded = self.is_expanded(&key);
                rows.push(SidebarRow {
                    label: d.display_name(),
                    target: d.root.clone(),
                    depth: 1,
                    expandable: true,
                    expanded,
                    key: key.clone(),
                    icon: SidebarIcon::Path(PathBuf::from(&d.root)),
                    // 第 1 层没有可画的祖先竖线（第 0 层是根，不画），只有一条
                    // 从"此电脑"伸下来的肘形连接线（由 last_child 决定收口）。
                    guides: vec![false],
                    last_child: di + 1 == n,
                    has_parent: true,
                    spacer: false,
                });
                if expanded {
                    // anc_last[0] = 此电脑（根）恒 false；anc_last[1] = 该盘符
                    // 是不是最后一个盘符。
                    let anc = vec![false, di + 1 == n];
                    self.push_children_rows_into(&key, 2, &anc, &mut rows);
                }
            }
        }
        // "此电脑 + 盘符"这一组和回收站之间同样留一格
        rows.push(SidebarRow::spacer());

        // 回收站：空 / 满用不同的系统库存图标，和资源管理器一样一眼能分辨
        let siid = if self.recycle_items.is_empty() {
            windows::Win32::UI::Shell::SIID_RECYCLER.0
        } else {
            windows::Win32::UI::Shell::SIID_RECYCLERFULL.0
        };
        rows.push(SidebarRow {
            label: "回收站".to_string(),
            target: RECYCLE_PATH.to_string(),
            depth: 0,
            expandable: false,
            expanded: false,
            key: String::new(),
            icon: SidebarIcon::Stock(siid),
            guides: Vec::new(),
            last_child: false,
            has_parent: false,
            spacer: false,
        });
        rows
    }

    /// 把某个已展开节点的子目录递归展开成行（深度上限 8，防止符号链接成环）。
    ///
    /// `anc_last[k]` = 第 k 层（depth = k）的祖先是不是它父节点的最后一个孩子。
    /// 它是画树线的唯一依据：第 k 层的竖线能不能贯穿这一行，取决于路径上
    /// **第 k+1 层那个节点**后面还有没有兄弟。
    fn push_children_rows_into(
        &mut self,
        key: &str,
        depth: usize,
        anc_last: &[bool],
        out: &mut Vec<SidebarRow>,
    ) {
        if depth > 8 {
            return;
        }
        if !self.sidebar_children.contains_key(key) {
            self.load_sidebar_children(key);
        }
        let Some(children) = self.sidebar_children.get(key).cloned() else { return };
        let n = children.len();
        for (ci, child) in children.into_iter().enumerate() {
            let ckey = Self::node_key(&child);
            let expanded = self.is_expanded(&ckey);
            let is_last = ci + 1 == n;
            // 路径上第 j 层那个节点是不是最后一个孩子
            let path_last = |j: usize| -> bool {
                if j < depth {
                    anc_last.get(j).copied().unwrap_or(false)
                } else {
                    is_last
                }
            };
            let mut guides = vec![false; depth];
            for k in 1..depth {
                guides[k] = !path_last(k + 1);
            }
            let fb = child.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let name = self.display_name(&child, &fb);
            out.push(SidebarRow {
                label: name,
                target: child.to_string_lossy().to_string(),
                depth,
                expandable: true,
                expanded,
                key: ckey.clone(),
                icon: SidebarIcon::Path(child.clone()),
                guides,
                last_child: is_last,
                has_parent: true,
                spacer: false,
            });
            if expanded {
                let mut next: Vec<bool> = anc_last.to_vec();
                while next.len() < depth {
                    next.push(false);
                }
                next.push(is_last);
                self.push_children_rows_into(&ckey, depth + 1, &next, out);
            }
        }
    }

    /// 目录的**本地化显示名**（资源管理器里"Users"显示为"用户"、"Desktop"显示为
    /// "桌面"）。取自 Shell 的 `SHGFI_DISPLAYNAME`，它会读 `desktop.ini` 的
    /// `LocalizedResourceName`，与资源管理器完全一致。
    ///
    /// 只对目录查询（文件一律用真实文件名），结果按路径缓存 ——
    /// 每个目录都问一次 Shell 会把大目录的枚举拖慢。
    pub fn display_name(&mut self, path: &Path, fallback: &str) -> String {
        if let Some(v) = self.display_names.get(path) {
            return v.clone();
        }
        let v = shell_display_name(path).unwrap_or_else(|| fallback.to_string());
        // 上界保护：极端情况下（同一个视图里几万个目录）不至于无限增长
        if self.display_names.len() < 4096 {
            self.display_names.insert(path.to_path_buf(), v.clone());
        }
        v
    }

    // ------------------------------------------------------------ 地址栏面包屑

    /// 当前路径拆成一节一节的面包屑（Win11 地址栏的样子）。
    ///
    /// 首节恒为"此电脑"（点它回主页），其后是盘符、各级目录。目录用本地化
    /// 显示名（资源管理器里 `C:\Users` 显示"用户"），盘符用卷标 + 盘符。
    /// 虚拟视图（主页 / 回收站 / 设置）只有一节，不可再拆。
    pub fn address_crumbs(&mut self) -> Vec<Crumb> {
        let (home, recycle, settings, path) = {
            let t = self.tab();
            (
                t.home,
                t.recycle,
                t.settings,
                zenfile_core::paths::strip_verbatim(&t.path).to_path_buf(),
            )
        };
        // 搜索态判定：过滤词非空且确实处于搜索结果视图（边打字边筛的
        // 过滤态 search_root 还没置上，那一瞬不画芯片，防抖后才出现 ——
        // 和 Pulse"开始搜了才有芯片"一致）。
        let search_chip = {
            let t = self.tab();
            let q = t.filter.trim().to_string();
            if t.search_root.is_some() && !q.is_empty() {
                Some(q)
            } else {
                None
            }
        };
        let mut out = if home {
            vec![Crumb { label: "此电脑".into(), target: None, ellipsis: false, chip: false }]
        } else if recycle {
            vec![Crumb { label: "回收站".into(), target: None, ellipsis: false, chip: false }]
        } else if settings {
            vec![Crumb { label: "设置".into(), target: None, ellipsis: false, chip: false }]
        } else {
            let segs = path_segments(&path);
            if segs.is_empty() {
                vec![Crumb {
                    label: path.to_string_lossy().to_string(),
                    target: None,
                    ellipsis: false,
                    chip: false,
                }]
            } else {
                let mut out = Vec::with_capacity(segs.len() + 1);
                out.push(Crumb { label: "此电脑".into(), target: None, ellipsis: false, chip: false });
                for (i, p) in segs.iter().enumerate() {
                    let label = if i == 0 {
                        // 盘符：优先卷标（"FurFHT (F:)"），没卷标就退回类型名 + 盘符
                        let mut root = p.to_string_lossy().to_string();
                        if !root.ends_with('\\') {
                            root.push('\\');
                        }
                        match drive_of(self, &PathBuf::from(&root)) {
                            Some(d) => d.display_name(),
                            None => root.trim_end_matches('\\').to_string(),
                        }
                    } else {
                        let fallback = p
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| p.to_string_lossy().to_string());
                        self.display_name(p, &fallback)
                    };
                    out.push(Crumb { label, target: Some(p.clone()), ellipsis: false, chip: false });
                }
                out
            }
        };
        if let Some(q) = search_chip {
            // 学 Pulse：搜索时末尾挂一枚`搜索“xx”`芯片。超长查询截断，
            // 免得把整条地址栏吃掉（芯片只是状态展示，完整词在搜索框里）。
            let short = if q.chars().count() > 16 {
                let mut s: String = q.chars().take(16).collect();
                s.push('\u{2026}');
                s
            } else {
                q.clone()
            };
            out.push(Crumb {
                label: format!("搜索\u{201C}{short}\u{201D}"),
                target: None,
                ellipsis: false,
                chip: true,
            });
        }
        out
    }

}

pub fn shell_row(text: &str, glyph: Option<&'static str>, cmd: Command) -> menu::MenuEntry {
    menu::MenuEntry {
        text: text.to_string(),
        glyph,
        bmp: None,
        separator: false,
        disabled: false,
        selected: false,
        has_submenu: false,
        submenu: Vec::new(),
        action: MenuAction::Own(cmd),
    }
}

/// 二级菜单里"当前生效的那一项"。
///
/// 之前用 `● 名称` 的圆点标记，两处别扭：圆点挤在文字前面让同级项对不齐，
/// 而且它和"鼠标正悬停"的反馈完全是两套语言。改成 `selected`：渲染层会在行首
/// 画一条**橙色竖线**，形状与悬停高亮一致、颜色区分开。
pub fn pick_row(text: &str, cmd: Command, on: bool) -> menu::MenuEntry {
    let mut r = shell_row(text, None, cmd);
    r.selected = on;
    r
}

/// 禁用态条目：字淡一档，且点上去不会有任何反应（与资源管理器一致）。
fn disabled_row(text: &str, glyph: Option<&'static str>, cmd: Command) -> menu::MenuEntry {
    let mut r = shell_row(text, glyph, cmd);
    r.disabled = true;
    r
}

/// 三个「固定到…」在当前系统 + 当前条目上能不能用（[任务栏, 开始菜单, 开始屏幕]）。
///
/// 只对**第一个**选中项探：这三项都是"对这个东西做什么"，多选时资源管理器也是
/// 只按第一个来（而且 Shell 菜单跨目录多选本来就建不出来）。
/// 探测要建 Shell 菜单桥，有成本，但只在开菜单时走一次。
fn pin_avail(app: &App, items: &[PathBuf]) -> [bool; 3] {
    let Some(first) = items.first() else {
        return [false; 3];
    };
    let parent = first
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| app.tab().path.clone());
    let one = vec![first.clone()];
    menu::pin_verbs_available(app.hwnd, &parent, &one)
}

/// 「固定到…」三项：能用就正常项，用不了就禁用态（不是隐藏）。
fn pin_row(text: &str, ok: bool, cmd: Command) -> menu::MenuEntry {
    let glyph: Option<&'static str> = Some("\u{E718}");
    if ok {
        shell_row(text, glyph, cmd)
    } else {
        disabled_row(text, glyph, cmd)
    }
}

/// 「排序方式」下拉的内容：四个排序列（当前列打点）+ 方向二选一。
///
/// 右键菜单和标题栏「更多」菜单共用这一份，避免两边顺序/文案对不上
/// （改一边忘另一边是必然的，不如只有一份）。
fn sort_rows(app: &App) -> Vec<menu::MenuEntry> {
    let key = app.tab().sort.key;
    let desc = app.tab().sort.descending;
    vec![
        pick_row("名称", Command::SortByName, key == zenfile_core::SortKey::Name),
        pick_row("修改日期", Command::SortByModified, key == zenfile_core::SortKey::Modified),
        pick_row("类型", Command::SortByType, key == zenfile_core::SortKey::Type),
        pick_row("大小", Command::SortBySize, key == zenfile_core::SortKey::Size),
        menu::MenuEntry::sep(),
        pick_row("递增顺序", Command::SortAscending, !desc),
        pick_row("递减顺序", Command::SortDescending, desc),
    ]
}

/// 视图三选一。当前视图用行首橙色竖线标出来（见 `pick_row`）。
fn view_rows(app: &App) -> Vec<menu::MenuEntry> {
    let mode = app.tab().view_mode;
    vec![
        pick_row("大图标", Command::ViewIcons, mode == zenfile_core::ViewMode::LargeIcons),
        pick_row("列表", Command::ViewList, mode == zenfile_core::ViewMode::List),
        pick_row("详细信息", Command::ViewDetails, mode == zenfile_core::ViewMode::Details),
    ]
}

/// 剪贴板此刻能不能往当前目录里粘贴。
///
/// 两件事都要看：剪贴板里得有文件，而且当前页得是一个真目录
/// （主页/回收站/设置都是虚拟视图，往里"粘贴"没有落点）。
fn can_paste_into(app: &App) -> bool {
    !is_virtual(&app.tab().path) && clipboard::has_files()
}

/// 当前视图里的条目能不能"复制出去"。虚拟视图（主页/回收站/设置）的条目是
/// 合成的，没有真实路径可复制，此时「复制/剪切」必须是禁用态。
fn copyable_here(app: &App) -> bool {
    !is_virtual(&app.tab().path)
}

/// 「新建」子菜单：文件夹 + 本机全部 ShellNew 模板，每项带系统自己的图标。
///
/// 之前硬编码成"文件夹 / 文本文档"两项：装了 Office/WPS/画图之后系统里能新建的
/// 有十几种，菜单里却只有两个 —— 跟资源管理器对不上，也建不出 .docx/.rtf/.bmp。
/// 现在直接扫 `HKCR\<ext>\ShellNew`，显示名和图标都问 Shell 要，与系统同源。
fn new_rows() -> Vec<menu::MenuEntry> {
    let mut v = vec![menu::MenuEntry {
        text: "文件夹".to_string(),
        glyph: None,
        bmp: crate::icons::folder_menu_bmp(),
        separator: false,
        disabled: false,
        selected: false,
        has_submenu: false,
        submenu: Vec::new(),
        action: MenuAction::Own(menu::Command::NewFolder),
    }];
    for tpl in crate::shellnew::templates() {
        v.push(menu::MenuEntry {
            text: tpl.label,
            glyph: None,
            bmp: crate::icons::menu_bmp_for(&tpl.ext),
            separator: false,
            disabled: false,
            selected: false,
            has_submenu: false,
            submenu: Vec::new(),
            action: MenuAction::Own(menu::Command::NewTemplate(tpl.ext)),
        });
    }
    v
}

/// 带下级菜单的条目。资源管理器的右键菜单本来就是分层的（新建/查看/排序方式都有
/// 下拉），做成平铺会把"悬停展开"这条交互整个丢掉。
pub fn submenu_row(text: &str, glyph: Option<&'static str>, children: Vec<menu::MenuEntry>) -> menu::MenuEntry {
    menu::MenuEntry {
        text: text.to_string(),
        glyph,
        bmp: None,
        separator: false,
        disabled: false,
        selected: false,
        has_submenu: !children.is_empty(),
        submenu: children,
        action: MenuAction::None,
    }
}

/// 右键菜单的**类型**：右键落在什么上。
///
/// 不同类型的菜单条目集合本来就不一样（文件有"打开"、空白处有"新建/查看/
/// 排序方式"），所以顺序与显隐也要各记一套 —— 这就是设置页照
/// ContextMenuManager 分类型的由来。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MenuKind {
    /// 选中了一个文件
    File,
    /// 选中了一个文件夹
    Folder,
    /// 空白处（目录背景）
    Background,
    /// 选中了多个
    Multi,
    /// 选中了盘符（C:\）
    Drive,
}

pub const MENU_KIND_NAMES: [&str; 5] = ["文件", "文件夹", "目录背景", "多选", "驱动器"];
/// 落盘用的键（写进 `ui_settings.json` 的 `menu_sets`）。
pub const MENU_KIND_KEYS: [&str; 5] = ["file", "folder", "background", "multi", "drive"];

impl MenuKind {
    pub fn from_u8(i: u8) -> Self {
        match i {
            1 => MenuKind::Folder,
            2 => MenuKind::Background,
            3 => MenuKind::Multi,
            4 => MenuKind::Drive,
            _ => MenuKind::File,
        }
    }
    pub fn index(self) -> u8 {
        match self {
            MenuKind::File => 0,
            MenuKind::Folder => 1,
            MenuKind::Background => 2,
            MenuKind::Multi => 3,
            MenuKind::Drive => 4,
        }
    }
}

/// 按"右键落在什么上"判定菜单类型。
pub fn menu_kind_of(items: &[PathBuf]) -> MenuKind {
    if items.is_empty() {
        return MenuKind::Background;
    }
    if items.len() > 1 {
        return MenuKind::Multi;
    }
    let p = &items[0];
    if p.is_file() {
        return MenuKind::File;
    }
    // 盘符根（C:\）单独一类：资源管理器里它的右键菜单也是另一套
    let st = p.to_string_lossy();
    let is_root = st.len() <= 3 && st.chars().nth(1) == Some(':');
    if is_root {
        MenuKind::Drive
    } else {
        MenuKind::Folder
    }
}

/// 自绘菜单的第一屏：图标行 + 常规动作（§7 产品化解法）。
///
/// 第 19 轮：条目的**顺序与显隐由设置页「右键菜单」标签决定**（拖出来的排布存在
/// `ui_settings.json`）。所以这里先按 id 收进一张表，最后再按排布吐出去 ——
/// 而不是像以前那样写死一串 push。id 与 `settings::MENU_ITEM_NAMES` 一一对应。
pub fn own_menu_rows(app: &App, items: &[PathBuf], kind: MenuKind) -> Vec<menu::MenuEntry> {
    let mut tagged: Vec<(u16, menu::MenuEntry)> = Vec::new();
    let mut push = |id: u16, e: menu::MenuEntry| tagged.push((id, e));

    if !items.is_empty() {
        push(0, shell_row("打开", Some("\u{E8A7}"), Command::Open));
        // 「打开方式」**每一类都有**（第55轮）：
        //   · 文件     → 用它打开这个文件
        //   · 文件夹   → 用它打开这个文件夹（VS Code / 终端 / 资源管理器…）
        //   · 多选     → 对选中里的第一个问
        //   · 驱动器   → 同文件夹
        // 以前只在"选中里有文件"时才列，于是文件夹右键永远看不到 —— 而装了
        // VS Code 的人最想要的恰恰是"用 Code 打开这个文件夹"。
        push(1, shell_row("打开方式", Some("\u{E7AC}"), Command::OpenWith));
        // 「复制」这类"要拿走点什么"的动作，在没有可复制内容时必须是**禁用态**：
        // 资源管理器里它就是淡的、点不动。之前不管有没有选中都照常列出，
        // 点下去静悄悄什么都不发生，看着像菜单坏了。
        if copyable_here(app) {
            push(2, shell_row("剪切", Some("\u{E8C6}"), Command::Cut));
            push(3, shell_row("复制", Some("\u{E8C8}"), Command::Copy));
        } else {
            push(2, disabled_row("剪切", Some("\u{E8C6}"), Command::Cut));
            push(3, disabled_row("复制", Some("\u{E8C8}"), Command::Copy));
        }
        // 粘贴要有东西可粘才可用：剪贴板里没有 CF_HDROP 时同样是禁用态
        if clipboard::has_files() {
            push(4, shell_row("粘贴", Some("\u{E77F}"), Command::Paste));
        } else {
            push(4, disabled_row("粘贴", Some("\u{E77F}"), Command::Paste));
        }
        push(5, shell_row("重命名", Some("\u{E8AC}"), Command::Rename));
        // 「批量重命名」搬到了目录栏最右侧的按钮（带下拉），不再占右键菜单的位置
        push(7, shell_row("删除", Some("\u{E74D}"), Command::Delete));
        // —— 三个「固定到…」——
        //
        // Windows 没有公开的 pin API，只能问 Shell 菜单要 verb，而各版本 Windows
        // 给的 verb 不一样（实测 Win10 LTSC 19044 只有 `PinToStartScreen` 真在菜单里）。
        // 所以**每次开菜单都探一次**，用不了就给禁用态 —— 宁可让用户看见"用不了"，
        // 也不能点了静悄悄没反应。
        let pin = pin_avail(app, items);
        push(14, pin_row("固定到任务栏", pin[0], Command::PinTaskbar));
        push(15, pin_row("固定到开始菜单", pin[1], Command::PinStartMenu));
        push(16, pin_row("固定到“开始”屏幕", pin[2], Command::PinStartScreen));
    }
    if items.is_empty() {
        // —— 以下只属于"空白处右键"（背景菜单）——
        //
        // 资源管理器里右键**某个文件/文件夹**时，菜单里没有"查看/排序方式/刷新/新建/全选"：
        // 那些是"对当前这个视图做什么"，不是"对这个东西做什么"。之前不管右键落在
        // 哪都照单全列，点"刷新"的时候看着像菜单做多了。
        push(8, submenu_row("新建", Some("\u{E8F4}"), new_rows()));
        push(9, shell_row("全选", Some("\u{E762}"), Command::SelectAll));
        push(10, shell_row("刷新", Some("\u{E72C}"), Command::Refresh));
        let mut view_children = view_rows(app);
        view_children.push(menu::MenuEntry::sep());
        // 三道开关分开列：隐藏项是用户自己设的，系统项是系统保护的，
        // 扩展名是纯显示开关 —— 混成一个开关没法单独控制。
        view_children.push(pick_row("隐藏的项目", Command::ToggleHidden, app.show_hidden));
        view_children.push(pick_row("受保护的系统文件", Command::ToggleSystem, app.show_system));
        view_children.push(pick_row(
            "文件扩展名",
            Command::ToggleExtensions,
            app.cfg.files.view.show_extensions,
        ));
        push(11, submenu_row("查看", Some("\u{E7B3}"), view_children));
        push(12, submenu_row("排序方式", Some("\u{E8CB}"), sort_rows(app)));
        // 空白处右键的「打开方式」= 用选中的程序打开**当前这个目录**
        // （等价于 VS Code 在背景菜单上挂的"Open with Code"）。
        push(1, shell_row("打开方式（此文件夹）", Some("\u{E7AC}"), Command::OpenWith));
    }
    push(
        13,
        shell_row(
            if items.is_empty() { "属性（当前文件夹）" } else { "属性" },
            Some("\u{E946}"),
            Command::Properties,
        ),
    );
    // 「设置」从右键菜单里去掉了：它是"改软件"的入口，混在"对这个东西做什么"
    // 的菜单里既不合语义又容易被误点（用户要求）。入口保留在「更多」菜单。

    // —— 按排布输出 ——
    let mut order = crate::settings::menu_order_for(app, kind);
    let hidden = crate::settings::menu_hidden_for(app, kind);
    if items.is_empty() {
        // 背景菜单里「打开方式（此文件夹）」排到**属性之前**（也就是最后）：
        // 它是"拿这个目录去干什么"，跟新建/全选/刷新那一堆摆在一起太冒失，
        // 按 id 自然序会跑到第一行去。
        order.retain(|i| *i != 1);
        let pos = order.iter().position(|i| *i == 13).unwrap_or(order.len());
        order.insert(pos, 1);
    }
    let mut out: Vec<menu::MenuEntry> = Vec::new();
    for id in order.iter() {
        if hidden.contains(id) {
            continue;
        }
        // 分隔线跟着分组走：粘贴是"改这个东西"的收尾，属性单独一节，
        // 空白处右键的"新建…"另起一节。
        let sep_before = (*id == 8 && items.is_empty()) || *id == 13;
        if sep_before {
            out.push(menu::MenuEntry::sep());
        }
        if let Some((_, e)) = tagged.iter().find(|(i, _)| i == id) {
            out.push(e.clone());
        }
    }
    // 排布里没记的条目（以后新增的）补在末尾，不至于凭空消失
    for (id, e) in &tagged {
        if !order.contains(id) && !hidden.contains(id) {
            out.push(e.clone());
        }
    }
    while matches!(out.last(), Some(e) if e.separator) {
        out.pop();
    }
    out
}

/// 菜单全局去重：同名的一级项只留第一条。
///
/// 自有条目和 Shell 条目各来一个"打开"是最常见的重复（Shell 的带加速键
/// `打开(&O)`，肉眼就是两个「打开」）。之前只在"追加 Shell 条目"那一步比对
/// 自有列表，而 Shell 自己内部也可能给两遍，于是去重不干净。
fn dedupe_rows(rows: Vec<menu::MenuEntry>) -> Vec<menu::MenuEntry> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::with_capacity(rows.len());
    for e in rows {
        if e.separator {
            out.push(e);
            continue;
        }
        let key = menu::menu_key(&e.text);
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(e);
    }
    // 去重之后可能留下"开头就是分隔线"或"两条挨着的分隔线"，一并收掉
    let mut clean: Vec<menu::MenuEntry> = Vec::with_capacity(out.len());
    for e in out {
        if e.separator && (clean.is_empty() || clean.last().map(|x: &menu::MenuEntry| x.separator).unwrap_or(false)) {
            continue;
        }
        clean.push(e);
    }
    while clean.last().map(|x| x.separator).unwrap_or(false) {
        clean.pop();
    }
    clean
}


/// 两个路径是不是同一个卷（只看盘符）。跨卷拖拽按 Windows 惯例走复制。
pub fn same_volume(a: &Path, b: &Path) -> bool {
    let vol = |p: &Path| -> String {
        let s = p.to_string_lossy();
        // "C:\a\b" -> "C:\"；UNC 路径取 "\\server\share"
        if s.starts_with("\\\\") {
            s.split('\\').take(5).collect::<Vec<_>>().join("\\")
        } else {
            s.chars().take(3).collect()
        }
    };
    vol(a) == vol(b)
}

/// 估算一段文字在当前字体下有多宽（像素）。
///
/// 「将列调整为合适的大小」要在**改宽度之前**就知道内容有多宽，而那一刻
/// 往往还没有 DC 可用（不能为了量宽度去建一个内存 DC 再销毁，每次点击都
/// 来一遍太浪费）。所以用一张保守的估算表：ASCII 约 7px、CJK 全角 13px，
/// 再按当前 scale 放大。真要精确时由绘制侧 `DT_END_ELLIPSIS` 兜底 ——
/// 估宽偏大只会让列略宽一点，不会截字。
pub fn text_width_units(s: &str) -> i32 {
    let mut w = 0i32;
    for c in s.chars() {
        w += if (c as u32) < 0x80 {
            7
        } else if is_wide(c) {
            13
        } else {
            8
        };
    }
    w
}

/// 是不是需要双倍宽度的字符（CJK / 全角标点 / 假名 / 谚文）。
fn is_wide(c: char) -> bool {
    let c = c as u32;
    (0x1100..=0x115F).contains(&c)      // 谚文字母
        || (0x2E80..=0xA4CF).contains(&c) // CJK 部首 ~ 彝文
        || (0xAC00..=0xD7A3).contains(&c) // 谚文音节
        || (0xF900..=0xFAFF).contains(&c) // CJK 兼容表意
        || (0xFE30..=0xFE4F).contains(&c) // CJK 兼容形式
        || (0xFF00..=0xFF60).contains(&c) // 全角 ASCII
        || (0xFFE0..=0xFFE6).contains(&c) // 全角符号
}

pub fn human(bytes: u64) -> String {
    zenfile_core::SizeUnit::Auto.format(bytes)
}

/// 按用户选定的单位格式化（设置页里的"大小单位"）。
pub fn human_unit(bytes: u64, unit: zenfile_core::SizeUnit) -> String {
    unit.format(bytes)
}


pub fn confirm(hwnd: HWND, text: &str) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONWARNING, MB_OKCANCEL, IDOK};
    let mut t: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let mut title: Vec<u16> = "确认".encode_utf16().chain(std::iter::once(0)).collect();
    let res = unsafe {
        MessageBoxW(
            Some(hwnd),
            windows::core::PCWSTR(t.as_mut_ptr()),
            windows::core::PCWSTR(title.as_mut_ptr()),
            MB_OKCANCEL | MB_ICONWARNING,
        )
    };
    // 用户点「确定」= 同意 = true。曾经返回 `res != IDOK`，于是清空回收站/
    // 彻底删除那两处 `if !confirm() { return; }` 变成"点取消才真删"（第39轮）。
    res == IDOK
}

pub fn report(hwnd: HWND, text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let mut t: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let mut title: Vec<u16> = "ZenFile".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let _ = MessageBoxW(
            Some(hwnd),
            windows::core::PCWSTR(t.as_mut_ptr()),
            windows::core::PCWSTR(title.as_mut_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn parse_view(s: &str) -> ViewMode {
    match s {
        "list" => ViewMode::List,
        "medium" | "medium_icons" => ViewMode::MediumIcons,
        "large" | "large_icons" => ViewMode::LargeIcons,
        _ => ViewMode::Details,
    }
}

/// 归一化成"视图覆盖表"的键：小写、去掉 `\\?\`、去掉尾斜杠。
fn norm_key(p: &Path) -> String {
    let s = zenfile_core::paths::strip_verbatim(p).to_string_lossy().replace('/', "\\");
    let s = s.trim_end_matches('\\').to_lowercase();
    s.to_string()
}

/// 视图模式 -> 配置文件里存的名字（`default_mode`）。
fn view_name(m: ViewMode) -> &'static str {
    match m {
        ViewMode::Details => "details",
        ViewMode::List => "list",
        ViewMode::MediumIcons => "medium",
        ViewMode::LargeIcons => "large",
    }
}

/// 设置页"默认视图"这一行的循环次序。`system` = 跟随资源管理器。
pub const DEFAULT_VIEW_CYCLE: [&str; 5] = ["system", "details", "list", "medium", "large"];

pub fn default_view_label(s: &str) -> &'static str {
    match s {
        "details" => "详细信息",
        "list" => "列表",
        "medium" => "中等图标",
        "large" => "大图标",
        _ => "跟随系统",
    }
}

fn parse_sort(s: &str) -> SortKey {
    match s {
        "size" => SortKey::Size,
        "modified" => SortKey::Modified,
        "type" => SortKey::Type,
        _ => SortKey::Name,
    }
}

/// COM 守卫在 App 创建前调用，保证 STA 语义（§2 / §7.1）。
///
/// 这里用 `OleInitialize` 而不是 `ComGuard::init()`（`CoInitializeEx` STA）：
/// **OLE 拖放要求 OLE 自己那一份初始化**。`OleInitialize` 内部会先做
/// `CoInitializeEx(COINIT_APARTMENTTHREADED)`，再初始化 OLE —— 其中包括拖放
/// 要用到的那个隐藏窗口。只 `CoInitializeEx` 的话，`DoDragDrop` 会返回
/// `E_INVALIDARG`/`E_OUTOFMEMORY` 之类，拖出去时松手什么都不会发生。
///
/// `ComGuard` 本身不改：它给 worker 线程用（后台复制/移动只做文件操作，
/// 不需要 OLE，多初始化一份反而拖慢线程启动）。
pub fn init_com() {
    // windows-rs 的 `OleInitialize` 已经把 HRESULT 折成了 `Result`：`S_FALSE`
    // （"本线程已经初始化过了"）在这里是 `Ok`，不用再单独判。
    //
    // 刻意泄漏：COM/OLE 必须保持初始化到进程结束，不调 `OleUninitialize`。
    let r = unsafe { windows::Win32::System::Ole::OleInitialize(None) };
    if let Err(e) = r {
        // `RPC_E_CHANGED_MODE` (0x80010106) = 本线程已经以别的模式初始化过
        // COM。那也是"COM 可用"，只是 OLE 那一层没起来，同样不算致命。
        if e.code() != windows::core::HRESULT(0x80010106u32 as i32) {
            // 真正的失败（内存不足等）也**不能**让程序起不来：OLE 起不来只是
            // Shell 上下文菜单/拖放不可用，文件列表照样要能看。
            eprintln!("[zenfile] OleInitialize 失败: {e}（Shell 集成与拖放将不可用）");
        }
    }
}

// --------------------------------------------------------------------
// 第75轮：`ChildEditKey` 判据的不变式
// --------------------------------------------------------------------
//
// 本轮把"每帧无条件让两个子 EDIT 重画"换成"内容真变了才重画"。这唯一的
// 功能风险是：**任何没被快照覆盖的外观依赖，都会留下旧像素**。
//
// 像素级验证（`%TEMP%\qa74\verify_hint75.py`）证明了提示词这一条在真窗口上
// 成立；这里把其余几条用纯逻辑钉死，防止以后有人往 `paint_search_decor` /
// `sync_address` 里再加一个依赖却忘了进快照 —— 那种改动在 review 里看不出问题，
// 只会在用户切主题/改缩放时看到一块旧像素。

#[cfg(test)]
mod child_key_tests {
    use super::*;

    fn tab_at(path: &str) -> Tab {
        Tab::new(PathBuf::from(path))
    }

    /// 搜索提示词与地址文本都只依赖 `Tab`，所以用真实 `Tab::new` 就够验，
    /// 不需要整个 `App`（那需要真实 HWND）。
    fn search_hint_of(tab: &Tab) -> String {
        if tab.home {
            return "搜索 此电脑".to_string();
        }
        if tab.recycle {
            return "搜索 回收站".to_string();
        }
        if tab.settings {
            return "搜索设置".to_string();
        }
        let name = tab
            .path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                tab.path
                    .to_string_lossy()
                    .trim_end_matches('\\')
                    .to_string()
            });
        format!("搜索 {name}")
    }

    #[test]
    fn address_display_text_special_pages() {
        // 三个虚拟页各有各的字，且**必须**与写入 EDIT 的是同一份 —— 因为
        // `sync_address` 与地址框失效判据都调 `address_display_text_for`。
        let mut t = tab_at(r"C:\Users\Administrator\Desktop");
        assert_eq!(address_display_text_for(&t), r"C:\Users\Administrator\Desktop");

        t.home = true;
        assert_eq!(address_display_text_for(&t), "主页");

        t.home = false;
        t.recycle = true;
        assert_eq!(address_display_text_for(&t), "回收站");

        t.recycle = false;
        t.settings = true;
        assert_eq!(address_display_text_for(&t), "设置");
    }

    #[test]
    fn address_display_text_strips_verbatim_prefix() {
        // canonicalize 会加 `\\?\` 前缀；那是给 API 看的，不该显示给人。
        let t = tab_at("\\\\?\\C:\\Users\\Administrator\\Desktop");
        assert_eq!(
            address_display_text_for(&t),
            r"C:\Users\Administrator\Desktop",
            "地址栏不该显示 Win32 长路径前缀"
        );

        // UNC 的那一种前缀也要还原成普通的 UNC 路径。
        let u = tab_at("\\\\?\\UNC\\server\\share");
        assert_eq!(address_display_text_for(&u), r"\\server\share");
    }

    #[test]
    fn address_display_text_is_stable_and_distinguishing() {
        // 判据不能有单向漂移：状态回到原样，文本必须回到原值；
        // 不同目录又必须给出不同文本（否则切目录后地址框会漏画）。
        let a = tab_at(r"C:\Users\Administrator\Desktop");
        let b = tab_at(r"C:\Users\Administrator\Documents");
        assert_ne!(address_display_text_for(&a), address_display_text_for(&b));
        assert_eq!(address_display_text_for(&a), address_display_text_for(&a));
    }

    #[test]
    fn search_hint_follows_tab_flags_and_directory_name() {
        // 提示词跟着三个虚拟标志短路；
        let mut t = tab_at(r"C:\Users\Administrator\Desktop");
        let base = search_hint_of(&t);
        assert!(base.contains("Desktop"), "应含当前目录名，实得 {base}");

        t.settings = true;
        assert_eq!(search_hint_of(&t), "搜索设置");
        t.settings = false;

        t.recycle = true;
        assert_eq!(search_hint_of(&t), "搜索 回收站");
        t.recycle = false;

        t.home = true;
        assert_eq!(search_hint_of(&t), "搜索 此电脑");
        t.home = false;

        // 也跟着当前目录的末段名走 —— 快照判据天然同时覆盖"切标签"与"导航"。
        let p = tab_at(r"C:\Users\Administrator\Pictures");
        assert!(search_hint_of(&p).contains("Pictures"));
        assert_ne!(search_hint_of(&p), base);
    }

    #[test]
    fn search_hint_at_drive_root_falls_back_to_drive_letter() {
        // 盘符根 `C:\` 没有 file_name，必须退回 "C:" 而不是显示空名字。
        let t = tab_at("C:\\");
        assert_eq!(search_hint_of(&t), "搜索 C:");
    }

    #[test]
    fn child_edit_key_compares_all_appearance_inputs() {
        // 判据 = 真实依赖的合集。逐项确认"这一项变了，判据就变"：
        //
        //   hint        提示词（跟标签 + 当前目录名）
        //   text_dim    提示词与放大镜的墨色（切明暗主题会变）
        //   scale_bits  字号 / 放大镜半径 / 左右边距（改缩放会变）
        //   icon_theme  放大镜字形（换图标主题会变）
        //
        // 后三项**不体现在 hint 里**，所以它们是"只按提示词判据"会漏掉的部分。
        let a = ChildEditKey {
            hint: "搜索 桌面".to_string(),
            text_dim: theme::dark().text_dim,
            scale_bits: 1.0f32.to_bits(),
            icon_theme: "default".to_string(),
            visible: true,
        };
        let mut b = a.clone();
        assert_eq!(a, b, "完全相同的两份快照必须判等（否则每帧都会重画）");

        b.hint = "搜索 图片".to_string();
        assert_ne!(a, b, "提示词变了必须判不等");

        let mut c = a.clone();
        c.text_dim = theme::light().text_dim;
        assert_ne!(a, c, "墨色变了必须判不等（切明暗主题要能触发重画）");

        let mut d = a.clone();
        d.scale_bits = 1.5f32.to_bits();
        assert_ne!(a, d, "缩放变了必须判不等（字号/放大镜半径要能触发重画）");

        let mut e = a.clone();
        e.icon_theme = "optimized".to_string();
        assert_ne!(a, e, "图标主题变了必须判不等（放大镜字形要能触发重画）");

        let mut f = a.clone();
        f.visible = false;
        assert_ne!(a, f, "可见性变了必须判不等（地址框隐藏<->显示那一帧不能漏）");
    }

    #[test]
    fn dark_and_light_text_dim_actually_differ() {
        // 上一条测试的前提：`text_dim` 在两个主题里确实不同。哪天两个主题
        // 的次要文字色被调成一样，这条断言会提醒我们判据里那一项失效了。
        assert_ne!(
            theme::dark().text_dim,
            theme::light().text_dim,
            "两个主题的 text_dim 必须不同，否则 ChildEditKey::text_dim 这一项形同虚设"
        );
        assert_ne!(
            theme::dark().text,
            theme::light().text,
            "两个主题的正文色必须不同，否则地址框的 text 判据项形同虚设"
        );
    }
}
