//! 应用状态与命令分发。UI 层的所有业务动作都收敛在这里，
//! 便于将来替换为 GPUI 编排层时只重写事件绑定，不动逻辑。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};

use fm_core::clipboard::{self, ClipMode};
use fm_core::config::Config;
use fm_core::engine::{self, EngineMsg, SearchRequest};
use fm_core::ops::{
    copy_items, delete_items, move_items, new_file, new_folder, rename_item, transfer_async,
    ComGuard, SharedStats, TransferKind,
};
use fm_core::paths;
use fm_core::process::ProcessTree;
use fm_core::types::{filter_entries, FileEntry, FileKind, SortKey, SortSpec, ViewMode};
use fm_core::watch::{DirWatcher, WatchMsg};
use windows::Win32::Foundation::HWND;
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
pub const SETTINGS_TAB_MENU: u8 = 1;
pub const SETTINGS_TAB_ADVANCED: u8 = 2;
pub const SETTINGS_TAB_PLUGIN: u8 = 3;
pub const SETTINGS_TAB_ABOUT: u8 = 4;
/// 设置页标签的显示名。顺序与上面的下标一一对应。
pub const SETTINGS_TABS: [&str; 5] = ["界面", "右键菜单", "高级", "插件", "关于"];

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
    use fm_core::types::FileKind;
    let profile = std::env::var("USERPROFILE").unwrap_or_default();
    let mut out: Vec<FileEntry> = Vec::new();

    let mk = |name: &str, path: String| FileEntry {
        name: name.to_string(),
        path: PathBuf::from(path),
        kind: FileKind::Directory,
        size: 0,
        modified: 0,
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

    for d in fm_core::drives::list() {
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
    pub fn matches(&self, e: &fm_core::FileEntry) -> bool {
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

pub struct App {
    pub hwnd: HWND,
    pub hwnd_address: HWND,
    pub hwnd_search: HWND,
    pub tabs: Vec<Tab>,
    pub active: usize,
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
    pub drag_origin: Option<(i32, i32)>,
    pub fast_mode: bool,
    pub transfer: Option<SharedStats>,
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
    pub drives: HashMap<String, fm_core::drives::DriveInfo>,
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
    /// 焦点行要不要**画**出来。
    ///
    /// `focus_row` 永远存在（第 0 行默认就是焦点行），但资源管理器在刚打开一个
    /// 文件夹时，是不会给第 0 行刷一条高亮的 —— 那一行看起来就像"默认被勾选了"。
    /// 只有用户真的用鼠标点过或用键盘移动过，焦点行才显形。
    pub focus_visible: bool,
    /// 标题栏上鼠标悬停的标签序号（用于画关闭按钮）。
    pub hover_tab: Option<usize>,
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
    pub recycle_rx: Option<Receiver<Vec<fm_core::recycle::Item>>>,
    /// 回收站枚举的世代号。切走又切回来时，上一趟的迟到结果要丢掉，
    /// 否则会把别的视图的显示数据顶掉。
    pub recycle_gen: u64,
    /// 回收站的排序：`(列序号, 是否降序)`。列序号同 `recycle_columns`：
    /// 0=名称 1=原位置 2=删除时间 3=大小。点列头就切这一列，再点一次换方向。
    pub recycle_sort: (usize, bool),
    /// 鼠标悬停的回收站条目下标（`recycle_items` 的下标）。
    pub hover_recycle: Option<usize>,
    /// 回收站顶部三个按钮的悬停位：0=无，1=还原，2=彻底删除，3=清空回收站。
    pub hover_recycle_btn: u8,
    pub recycle_items: Vec<fm_core::recycle::Item>,
    /// 回收站里被选中的项（按 `$R` 实体名索引）。
    pub recycle_sel: HashSet<String>,
    /// 设置页里鼠标悬停的行（存的是该行的动作编号，行位置会随开关文案变化）。
    pub settings_hover: Option<u32>,
    /// 设置页当前停在哪个小标签（见 `SETTINGS_TABS`）。
    pub settings_tab: u8,
    /// 设置页顶部小标签的悬停下标。
    pub settings_hover_tab: Option<u8>,
    /// 排布编辑器里鼠标悬停的组件/条目 id。
    pub settings_hover_card: Option<u32>,
    /// 排布编辑器里正在拖的卡片。
    pub settings_drag: Option<crate::settings::SettingsDrag>,
    /// 「界面 / 右键菜单」的排布（顺序 + 被收进「更多」的那些）。
    pub ui_settings: crate::settings::Settings,
    /// 文件名索引（Everything 式秒搜）。整卷一份，常驻内存。
    pub index: Option<std::sync::Arc<fm_core::index::Index>>,
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
    /// 标题栏三大金刚键的悬停位：0=无，1=最小化，2=最大化，3=关闭。
    pub hover_caption: u8,
    /// 按下未抬起的那一个（0=无，1/2/3 同上）。
    pub pressed_caption: u8,
    /// 地址栏是否处于"文本编辑"态。
    ///
    /// Win11 的地址栏平时是**面包屑**（一节一节可点的路径），只有点空白处或
    /// 按 Ctrl+L/F4 才变成可输入的文本框。平时把 EDIT 藏着，编辑时再显示出来。
    pub addr_editing: bool,
    /// 鼠标悬停的面包屑下标（hover 反馈）。
    pub addr_hover: Option<usize>,
    /// 左键拖拽（拖条目 / 空白处框选）。松手或取消后置回 None。
    pub drag: Option<Drag>,
    /// 拖拽当前悬着的可投放目标。
    pub drop_target: Option<DropTarget>,
    /// 正在拖哪一条分隔线（导航窗格宽度 / 详细信息表头的某一列）。
    ///
    /// `None` 之外还要记住"按下时 cursor 在分隔线上的偏移"，否则拖动一开始
    /// 分隔线会直接跳到光标正下方（看着像被瞬移了一下）。
    pub drag_split: Option<SplitDrag>,
    /// 鼠标正悬停在哪条分隔线上（只为了把光标换成左右箭头）。
    pub hover_split: Option<Split>,
    /// 用户在 ZenFile 里手动改过的视图：路径(小写,无尾斜杠) -> 视图。
    ///
    /// 优先级高于资源管理器那套 bag。原因很实在：bag 只有在资源管理器
    /// **去过**这个文件夹时才存在，写不进去的时候（没去过 / 解析不出节点）
    /// 至少本进程里来回切换不该被默认值吃掉。
    pub view_overrides: HashMap<String, ViewMode>,
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
#[derive(Clone)]
pub struct DropTarget {
    pub path: PathBuf,
    pub kind: DropKind,
}

#[derive(Clone, Copy)]
pub enum DropKind {
    /// 内容区第 n 行（一个文件夹）
    Row(usize),
    /// 地址栏第 n 节面包屑（#12：拖到哪一级就进哪一级）
    Crumb(usize),
    /// 导航窗格第 n 行
    Sidebar(usize),
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

pub enum IndexMsg {
    /// 建索引过程中周期性回报已扫条目数
    Progress(usize),
    Ready(std::sync::Arc<fm_core::index::Index>),
    Failed(String),
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
pub fn drive_of<'a>(app: &'a App, path: &Path) -> Option<&'a fm_core::drives::DriveInfo> {
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
    use fm_core::paths::known_folder;
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
        Self {
            hwnd,
            hwnd_address: HWND(std::ptr::null_mut()),
            hwnd_search: HWND(std::ptr::null_mut()),
            tabs: Vec::new(),
            active: 0,
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
            drag_origin: None,
            fast_mode: false,
            transfer: None,
            io_tx,
            io_rx,
            io_pending: 0,
            exit_to_tray: false,
            hovering_close: false,
            hover_chrome: 0,
            hover_bulk: false,
            drives: HashMap::new(),
            drives_stamp: None,
            sidebar_expanded: HashSet::new(),
            sidebar_children: HashMap::new(),
            sidebar_hover: None,
            hover_row: None,
            hover_sidebar_collapse: false,
            hover_nav: 0,
            hover_header: None,
            cut_set: HashSet::new(),
            sidebar_scroll: 0,
            display_names: HashMap::new(),
            focus_visible: false,
            hover_tab: None,
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
            recycle_sort: (2, true),
            hover_recycle: None,
            hover_recycle_btn: 0,
            settings_hover: None,
            settings_tab: 0,
            settings_hover_tab: None,
            settings_hover_card: None,
            settings_drag: None,
            ui_settings: crate::settings::Settings::load(),
            hover_caption: 0,
            pressed_caption: 0,
            addr_editing: false,
            addr_hover: None,
            index: None,
            index_root: None,
            index_state: IndexState::Idle,
            index_pending: false,
            index_rx: None,
            index_cancel: None,
            drag_split: None,
            hover_split: None,
            drag: None,
            drop_target: None,
            view_overrides: HashMap::new(),
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

    pub fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    pub fn tab_mut(&mut self) -> &mut Tab {
        let i = self.active;
        &mut self.tabs[i]
    }

    pub fn invalidate(&self) {
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(self.hwnd), None, true);
        }
    }

    // ---------------------------------------------------------------- 导航

    pub fn open_tab(&mut self, path: PathBuf) {
        // 导航树要用驱动器清单，而它只在"进主页 / 定时兜底 / 设备插拔"时刷新。
        // 首次建标签时那张表还是空的（侧栏会只剩"此电脑"没有盘符），这里补一次。
        if self.drives.is_empty() {
            self.refresh_drives();
            if self.cfg.ui.sidebar_thispc_expanded {
                self.sidebar_expanded.insert(SIDEBAR_THISPC.to_string());
            }
        }
        let mut tab = Tab::new(path.clone());
        tab.view_mode = parse_view(&self.cfg.files.view.default_mode);
        // 同上：新标签也要按"这个文件夹自己的视图"来
        if self.cfg.files.view.default_mode == "system" {
            if let Some(m) = self.view_overrides.get(&norm_key(&path)).copied() {
                tab.view_mode = m;
            } else if let Some(m) = crate::sysview::system_view(&path) {
                tab.view_mode = m;
            }
        }
        tab.sort = SortSpec { key: parse_sort(&self.cfg.files.view.sort_key), descending: self.cfg.files.view.sort_descending };
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.reload();
        self.invalidate();
    }

    /// 新建标签页。与资源管理器一致：新标签落在"主页"，而不是复制当前目录。
    pub fn new_tab(&mut self) {
        self.open_tab(PathBuf::from(HOME_PATH));
    }

    pub fn close_tab(&mut self, index: usize) {
        // 只剩一个标签也允许关：点了 × 窗口却纹丝不动，是"按钮坏了"的典型观感。
        // 资源管理器在这里是关掉整个窗口，而我们有托盘模式，关窗口 ≠ 退出，
        // 所以照样关，然后补一个新的主页标签回来。
        if self.tabs.len() <= 1 {
            self.tabs.clear();
            self.active = 0;
            self.open_tab(PathBuf::from(HOME_PATH));
            self.invalidate();
            return;
        }
        self.tabs.remove(index);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
        self.hover_tab = None;
        self.reload();
        self.invalidate();
    }

    pub fn navigate(&mut self, path: PathBuf) {
        // 入口就把 verbatim 前缀剥掉：调用方可能传的是 canonicalize 的结果
        // （带 `\\?\`），那个前缀会让目录枚举返回空列表。
        let path = fm_core::paths::strip_verbatim(&path);
        // "跟随展开到当前文件夹"：默认关。开着的时候把当前路径的所有祖先展开，
        // 让导航窗格始终显示"我在哪"。逐层读目录清单是同步的，所以这个开关
        // 关着的时候绝不会有任何额外 IO。
        if self.cfg.ui.sidebar_follow_current && !is_virtual(&path) {
            self.reveal_in_sidebar(&path);
        }
        // 换了目录，"鼠标指着哪一项"就不再成立（条目下标指向的是上一页的清单）。
        // 不清的话新页面第一行会莫名顶着一条悬停底色。
        self.hover_row = None;
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
            let tab = self.tab_mut();
            tab.home = is_home_path(&path);
            tab.recycle = is_recycle_path(&path);
            tab.settings = is_settings_path(&path);
            tab.path = path.clone();
            tab.title = title;
            tab.selected.clear();
            tab.scroll = 0;
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
        // 地址栏文本在这里一次更新掉：原来靠每个调用方自己记得调 sync_address，
        // 漏一个就表现为"点了面包屑看着没反应"（其实目录已经切了，只是地址栏
        // 还写着旧路径）。刷新放在导航里，调用方就不可能忘。
        crate::paint::sync_address(self);
        // 视图要在这个目录上重新定一次：资源管理器就是"每个文件夹各记一套"，
        // 从大图标的文件夹走进详细信息文件夹，视图跟着变才是对的。
        self.apply_view_for(&path);
        self.reload();
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
        self.apply_history(&target, Some(from));
    }

    pub fn go_forward(&mut self) {
        if !self.can_forward() {
            return;
        }
        let from = self.tab().path.clone();
        self.tab_mut().history_pos += 1;
        let target = self.tab().history[self.tab().history_pos].clone();
        self.apply_history(&target, Some(from));
    }

    fn apply_history(&mut self, target: &Path, select: Option<PathBuf>) {
        let tab = self.tab_mut();
        tab.home = is_home_path(target);
        tab.path = target.to_path_buf();
        tab.title = if tab.home {
            "此电脑".to_string()
        } else {
            display_title(target)
        };
        tab.selected.clear();
        tab.scroll = 0;
        tab.focus_row = 0;
        tab.filter.clear();
        self.pending_select = select;
        self.focus_visible = false;
        // 前进/后退也是换目录，视图同样要按目标文件夹重定一次
        self.apply_view_for(target);
        self.reload();
        self.try_pending_select();
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
        for d in fm_core::drives::list() {
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
            let tab = self.tab_mut();
            tab.generation += 1;
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
            let tab = self.tab_mut();
            tab.generation += 1;
            tab.entries.clear();
            tab.visible.clear();
            tab.loading = false;
            return;
        }

        // 先在取得 tab 的可变借用之前把需要的共享资源取出，避免借用冲突
        let tx = self.engine_tx.clone();
        let search_limit = self.cfg.files.navigation.search_limit;
        let worker_threads = self.cfg.files.operations.worker_threads;

        let tab = self.tab_mut();
        tab.generation += 1;
        tab.loading = true;
        tab.entries.clear();
        tab.visible.clear();
        let generation = tab.generation;
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
        let q = self.tab().filter.trim().to_string();
        let cur = self.tab().path.clone();
        let Some(root) = fm_core::index::volume_root(&cur) else {
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
            self.reload();
            return;
        }
        {
            let tab = self.tab_mut();
            tab.search_root = Some(PathBuf::from(&root));
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
            tab.filter = q.clone();
        }
        self.ensure_index(&root);
        self.reload();
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
        let has_cache = fm_core::index::Index::load(root).is_some();
        self.index_state = if has_cache {
            IndexState::Loading
        } else {
            IndexState::Building(0)
        };
        let root_s = root.to_string();
        std::thread::spawn(move || {
            // 有缓存：直接反序列化，比遍历快两个数量级
            if let Some(idx) = fm_core::index::Index::load(&root_s) {
                let _ = tx.send(IndexMsg::Ready(std::sync::Arc::new(idx)));
                return;
            }
            // 没有：遍历一遍。中途回报条数，好让界面上有可见的进度
            let last = std::cell::Cell::new(0usize);
            let idx = fm_core::index::Index::build(&root_s, &cancel, Some(&|n: usize| {
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
    pub fn apply_index_hits(&mut self, idx: &fm_core::index::Index, q: &str) {
        let limit = self.cfg.files.navigation.search_limit.max(50);
        let hits = idx.query(q, limit);
        let (show_hidden, show_system) = (self.show_hidden, self.show_system);
        let mut entries: Vec<FileEntry> = hits
            .into_iter()
            .map(|h| FileEntry {
                name: h.name,
                path: h.path,
                kind: if h.dir { FileKind::Directory } else { FileKind::File },
                size: h.size,
                modified: h.modified,
                attributes: if h.dir { 0x10 } else { 0 } | if h.hidden { 0x2 } else { 0 },
                hidden: h.hidden,
            })
            .collect();
        let tab = self.tab_mut();
        tab.generation += 1;
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
        let mut changed = self.drain_index();
        // 同 reload()：把开关拷出来，避免循环体内对 self.tabs 的可变借用与 self 字段读取冲突
        let (show_hidden, show_system) = (self.show_hidden, self.show_system);
        while let Ok(msg) = self.engine_rx.try_recv() {
            match msg {
                EngineMsg::Batch(batch) => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.generation == batch.generation) {
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
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.generation == gen) {
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
        self.set_focus_row(row);
        let tab = self.tab_mut();
        let Some(idx) = tab.visible.get(row).copied() else { return };
        let Some(entry) = tab.entries.get(idx) else { return };
        let k = key_of(entry);
        if range {
            let start = tab.focus_row.min(row);
            let end = tab.focus_row.max(row);
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
            fm_core::ViewMode::LargeIcons | fm_core::ViewMode::MediumIcons => {
                let g = crate::paint::icon_grid(l, &metrics, mode);
                crate::paint::grid_content_h(&g, count)
            }
            _ => count as i32 * metrics.row_height_for(mode),
        };
        let max = (content - view_h).max(0);
        let tab = self.tab_mut();
        tab.scroll = tab.scroll.min(max).max(0);
    }

    pub fn select_all(&mut self) {
        let tab = self.tab_mut();
        tab.selected.clear();
        for e in &tab.entries {
            tab.selected.insert(key_of(e));
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
        }
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
        if self.tabs.len() < 2 {
            return;
        }
        let len = self.tabs.len() as isize;
        let cur = self.active as isize;
        self.active = (((cur + delta) % len + len) % len) as usize;
        self.reload();
        self.invalidate();
    }

    /// 把选中项（或焦点项）的完整路径以文本形式放进剪贴板
    pub fn copy_paths_text(&mut self) -> Result<(), fm_core::Error> {
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
        let path = fm_core::paths::strip_verbatim(path);
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
        if self.cfg.files.operations.confirm_delete && confirm(self.hwnd, &format!(
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
                let label = if mode == ClipMode::Move { "粘贴（移动）" } else { "粘贴（复制）" };
                // 东西已经落下去了，原处那层"虚化"必须跟着撤掉
                self.cut_set.clear();
                if self.fast_mode || self.cfg.files.operations.fast_mode {
                    self.transfer = Some(transfer_async(
                        if mode == ClipMode::Move { TransferKind::Move } else { TransferKind::Copy },
                        &dest,
                        files,
                        self.cfg.files.operations.worker_threads,
                    ));
                    return;
                }
                // 同 drop_items：真正的搬迁去后台线程，UI 线程不阻塞、不被重入
                self.spawn_io(label, move || {
                    if mode == ClipMode::Move {
                        move_items(0, &dest, &files)
                    } else {
                        copy_items(0, &dest, &files)
                    }
                });
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
        F: FnOnce() -> fm_core::Result<()> + Send + 'static,
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
            let tab = self.tab_mut();
            tab.generation += 1;
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
            fm_core::recycle::list_streaming(|batch| {
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
        let tab = self.tab_mut();
        tab.generation += 1;
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
            let tab = self.tab_mut();
            tab.generation += 1;
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
        let label = if mode == ClipMode::Move { "移动" } else { "复制" };
        if self.fast_mode || self.cfg.files.operations.fast_mode {
            self.transfer = Some(transfer_async(
                if mode == ClipMode::Move { TransferKind::Move } else { TransferKind::Copy },
                &dest,
                items,
                self.cfg.files.operations.worker_threads,
            ));
            return;
        }
        // 关键：**不在松手这条消息里真的去搬文件**。搬迁交给后台线程，
        // 完成后由 WM_APP_IO_DONE 触发 reload —— 这里的 reload 会等到那一步。
        self.spawn_io(label, move || {
            if mode == ClipMode::Move {
                move_items(0, &dest, &items)
            } else {
                copy_items(0, &dest, &items)
            }
        });
        // 立刻重画一次：让"正在移动"的进度条出现
        self.invalidate();
    }

    /// 拖拽到另一个标签页不适用时的替代：粘贴到指定目录（供外部调用）。
    pub fn paste_into(&mut self, dest: &Path, files: Vec<PathBuf>, mode: ClipMode) {
        let owner = self.hwnd.0 as isize;
        let _ = match mode {
            ClipMode::Move => move_items(owner, dest, &files),
            ClipMode::Copy => copy_items(owner, dest, &files),
        };
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

        let mut entries = own_menu_rows(self, &items);
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
        }
    }

    pub fn on_menu_activation(&mut self, action: MenuAction) {
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

    pub fn run_command(&mut self, cmd: Command) {
        match cmd {
            Command::Open => self.open_focused(),
            Command::OpenWith => {
                // 有多个选中就对第一个文件问：资源管理器也是这么做的。
                // `selected_paths()` 在选中为空时会退回焦点行，正合适。
                let target = self.tab().selected_paths().into_iter().find(|p| p.is_file());
                if let Some(p) = target {
                    let ptr = self as *mut App;
                    crate::openwith::open(ptr, p);
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
            Command::ViewIcons => self.set_view(ViewMode::LargeIcons),
            Command::ViewList => self.set_view(ViewMode::List),
            Command::ViewDetails => self.set_view(ViewMode::Details),
            Command::SortByName => self.sort_column(fm_core::SortKey::Name),
            Command::SortByModified => self.sort_column(fm_core::SortKey::Modified),
            Command::SortByType => self.sort_column(fm_core::SortKey::Type),
            Command::SortBySize => self.sort_column(fm_core::SortKey::Size),
            // 只改方向：点"递减顺序"时不该把列也一起换掉（资源管理器同理）
            Command::SortAscending => self.set_sort_direction(false),
            Command::SortDescending => self.set_sort_direction(true),
            Command::ToggleSidebar => self.toggle_sidebar(),
            Command::ToggleStatusbar => self.toggle_statusbar(),
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
        if let Err(e) = fm_core::config::Config::save_user_patch(&serde_json::json!({
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
        if let Err(e) = fm_core::config::Config::save_user_patch(&serde_json::json!({
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
        let u = fm_core::SizeUnit::ALL.get(idx).copied().unwrap_or(fm_core::SizeUnit::Auto);
        self.cfg.files.view.size_unit = u.name().to_string();
        self.persist_view_pref();
        self.invalidate();
    }

    pub fn set_theme(&mut self, dark: bool) {
        let next = if dark { "dark" } else { "light" };
        self.cfg.core.theme = next.to_string();
        self.theme = if dark { theme::dark() } else { theme::light() };
        let _ = fm_core::config::Config::save_user_patch(&serde_json::json!({
            "core": { "theme": next }
        }));
        self.invalidate();
    }

    pub fn set_default_view(&mut self, idx: usize) {
        let next = DEFAULT_VIEW_CYCLE.get(idx).copied().unwrap_or("system");
        self.cfg.files.view.default_mode = next.to_string();
        self.persist_view_pref();
        self.invalidate();
    }

    pub fn cycle_size_unit(&mut self) {
        let cur = fm_core::SizeUnit::parse(&self.cfg.files.view.size_unit);
        let i = fm_core::SizeUnit::ALL.iter().position(|u| *u == cur).unwrap_or(0);
        let next = fm_core::SizeUnit::ALL[(i + 1) % fm_core::SizeUnit::ALL.len()];
        self.cfg.files.view.size_unit = next.name().to_string();
        self.persist_view_pref();
        self.invalidate();
    }

    /// 切换明暗主题。
    pub fn cycle_theme(&mut self) {
        let next = if self.cfg.core.theme == "light" { "dark" } else { "light" };
        self.cfg.core.theme = next.to_string();
        self.theme = if next == "light" { theme::light() } else { theme::dark() };
        let _ = fm_core::config::Config::save_user_patch(&serde_json::json!({
            "core": { "theme": next }
        }));
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
        if let Some(i) = self.tabs.iter().position(|t| t.settings) {
            self.active = i;
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
        if let Some(i) = self.tabs.iter().position(|t| t.recycle) {
            self.active = i;
            self.reload();
        } else {
            self.open_tab(PathBuf::from(RECYCLE_PATH));
        }
        crate::paint::sync_address(self);
        self.invalidate();
    }

    /// 还原回收站里选中的项。
    pub fn restore_recycle(&mut self) {
        let picked: Vec<fm_core::recycle::Item> = self
            .recycle_items
            .iter()
            .filter(|it| self.recycle_sel.contains(&it.id))
            .cloned()
            .collect();
        if picked.is_empty() {
            return;
        }
        let n = fm_core::recycle::restore(&picked);
        report(self.hwnd, &format!("已还原 {n} 项"));
        self.reload();
    }

    /// 彻底删除回收站里选中的项。
    pub fn purge_recycle(&mut self) {
        let picked: Vec<fm_core::recycle::Item> = self
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
        let n = fm_core::recycle::remove(&picked);
        report(self.hwnd, &format!("已彻底删除 {n} 项"));
        self.reload();
    }

    /// 清空回收站。
    pub fn empty_recycle(&mut self) {
        if !confirm(self.hwnd, "清空回收站？此操作不可撤销。") {
            return;
        }
        let _ = fm_core::recycle::empty();
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

    /// 只把 UI 开关写回 settings.user.json，不覆盖其它字段。
    pub fn persist_ui_pref(&self) {
        if let Err(e) = fm_core::config::Config::save_user_patch(&serde_json::json!({
            "ui": {
                "sidebar_visible": self.cfg.ui.sidebar_visible,
                "statusbar_visible": self.cfg.ui.statusbar_visible,
                "sidebar_follow_current": self.cfg.ui.sidebar_follow_current,
                "sidebar_thispc_expanded": self.cfg.ui.sidebar_thispc_expanded,
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
        let _ = fm_core::config::Config::save_user_patch(&serde_json::json!({
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
                if fm_core::index::volume_root(p).as_deref() == Some(s.as_str()) {
                    " · 整卷搜索".to_string()
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
                    fm_core::natural::natural_cmp(
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
                fm_core::paths::strip_verbatim(&t.path).to_path_buf(),
            )
        };
        if home {
            return vec![Crumb { label: "此电脑".into(), target: None, ellipsis: false }];
        }
        if recycle {
            return vec![Crumb { label: "回收站".into(), target: None, ellipsis: false }];
        }
        if settings {
            return vec![Crumb { label: "设置".into(), target: None, ellipsis: false }];
        }

        let segs = path_segments(&path);
        if segs.is_empty() {
            return vec![Crumb {
                label: path.to_string_lossy().to_string(),
                target: None,
                ellipsis: false,
            }];
        }

        let mut out = Vec::with_capacity(segs.len() + 1);
        out.push(Crumb { label: "此电脑".into(), target: None, ellipsis: false });
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
            out.push(Crumb { label, target: Some(p.clone()), ellipsis: false });
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

/// 「排序方式」下拉的内容：四个排序列（当前列打点）+ 方向二选一。
///
/// 右键菜单和标题栏「更多」菜单共用这一份，避免两边顺序/文案对不上
/// （改一边忘另一边是必然的，不如只有一份）。
fn sort_rows(app: &App) -> Vec<menu::MenuEntry> {
    let key = app.tab().sort.key;
    let desc = app.tab().sort.descending;
    vec![
        pick_row("名称", Command::SortByName, key == fm_core::SortKey::Name),
        pick_row("修改日期", Command::SortByModified, key == fm_core::SortKey::Modified),
        pick_row("类型", Command::SortByType, key == fm_core::SortKey::Type),
        pick_row("大小", Command::SortBySize, key == fm_core::SortKey::Size),
        menu::MenuEntry::sep(),
        pick_row("递增顺序", Command::SortAscending, !desc),
        pick_row("递减顺序", Command::SortDescending, desc),
    ]
}

/// 视图三选一。当前视图用行首橙色竖线标出来（见 `pick_row`）。
fn view_rows(app: &App) -> Vec<menu::MenuEntry> {
    let mode = app.tab().view_mode;
    vec![
        pick_row("大图标", Command::ViewIcons, mode == fm_core::ViewMode::LargeIcons),
        pick_row("列表", Command::ViewList, mode == fm_core::ViewMode::List),
        pick_row("详细信息", Command::ViewDetails, mode == fm_core::ViewMode::Details),
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

/// 自绘菜单的第一屏：图标行 + 常规动作（§7 产品化解法）。
///
/// 第 19 轮：条目的**顺序与显隐由设置页「右键菜单」标签决定**（拖出来的排布存在
/// `ui_settings.json`）。所以这里先按 id 收进一张表，最后再按排布吐出去 ——
/// 而不是像以前那样写死一串 push。id 与 `settings::MENU_ITEM_NAMES` 一一对应。
fn own_menu_rows(app: &App, items: &[PathBuf]) -> Vec<menu::MenuEntry> {
    let mut tagged: Vec<(u16, menu::MenuEntry)> = Vec::new();
    let mut push = |id: u16, e: menu::MenuEntry| tagged.push((id, e));

    if !items.is_empty() {
        push(0, shell_row("打开", Some("\u{E8A7}"), Command::Open));
        // 「打开方式」只对文件有意义：文件夹的"打开方式"在资源管理器里也没有。
        if items.iter().any(|p| p.is_file()) {
            push(1, shell_row("打开方式", Some("\u{E7AC}"), Command::OpenWith));
        }
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
    let order = &app.ui_settings.menu_order;
    let hidden = &app.ui_settings.menu_hidden;
    let mut out: Vec<menu::MenuEntry> = Vec::new();
    for id in order {
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

pub fn human(bytes: u64) -> String {
    fm_core::SizeUnit::Auto.format(bytes)
}

/// 按用户选定的单位格式化（设置页里的"大小单位"）。
pub fn human_unit(bytes: u64, unit: fm_core::SizeUnit) -> String {
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
    res != IDOK
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
    let s = fm_core::paths::strip_verbatim(p).to_string_lossy().replace('/', "\\");
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
pub fn init_com() {
    let _guard = ComGuard::init();
    // 刻意泄漏：COM 必须保持初始化到进程结束。
    std::mem::forget(_guard);
}
