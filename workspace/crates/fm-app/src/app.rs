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

use crate::icons::IconCache;
use crate::menu::{self, Command, MenuAction, ShellMenuBridge};
use crate::theme::{self, Metrics, Theme};

/// 主页（快速访问）的哨兵路径。它不是真实目录，由 App 合成条目。
pub const HOME_PATH: &str = "shell:Home";
/// 回收站哨兵路径。同样是虚拟视图，条目来自 `$Recycle.Bin` 直读。
pub const RECYCLE_PATH: &str = "shell:RecycleBin";
/// 设置页哨兵路径。以标签页形式打开，不参与目录枚举。
pub const SETTINGS_PATH: &str = "shell:Settings";

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
    pub search_root: Option<PathBuf>,
}

impl Tab {
    pub fn new(path: PathBuf) -> Self {
        let title = if is_home_path(&path) {
            "主页".to_string()
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
    pub exit_to_tray: bool,
    pub hovering_close: bool,
    /// 标题栏自有按钮的悬停位：bit0=侧栏开关，bit1=更多菜单
    pub hover_chrome: u8,
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
    /// 本次右键菜单要作用在哪些条目上。
    ///
    /// `Some(vec![])` 表示右键点在了空白处；`None` 表示不是从右键进来的。
    /// 之前直接调 `selected_paths()`，而它在选中为空时会**退回焦点行** ——
    /// 于是"空白处右键"拿到的还是上一次那个条目，菜单判定的就是错的。
    pub context_targets: Option<Vec<PathBuf>>,
    /// 回收站条目（进入回收站视图时刷新一次）。
    pub recycle_items: Vec<fm_core::recycle::Item>,
    /// 回收站里被选中的项（按 `$R` 实体名索引）。
    pub recycle_sel: HashSet<String>,
    /// 设置页里鼠标悬停的行（存的是该行的动作编号，行位置会随开关文案变化）。
    pub settings_hover: Option<u32>,
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
}

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
            exit_to_tray: false,
            hovering_close: false,
            hover_chrome: 0,
            drives: HashMap::new(),
            drives_stamp: None,
            sidebar_expanded: HashSet::new(),
            sidebar_children: HashMap::new(),
            sidebar_hover: None,
            sidebar_scroll: 0,
            display_names: HashMap::new(),
            focus_visible: false,
            hover_tab: None,
            hover_view_btn: 0,
            pending_select: None,
            context_targets: None,
            recycle_items: Vec::new(),
            recycle_sel: HashSet::new(),
            settings_hover: None,
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
        let mut tab = Tab::new(path);
        tab.view_mode = parse_view(&self.cfg.files.view.default_mode);
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
        if self.tabs.len() <= 1 {
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
        let depth = self.cfg.files.navigation.history_depth;
        // 标签标题也走本地化显示名：资源管理器里 `C:\Users` 的标签写的是"用户"，
        // 直接显示盘上真名会让人对不上号。
        let title = if is_home_path(&path) {
            "主页".to_string()
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
            "主页".to_string()
        } else {
            display_title(target)
        };
        tab.selected.clear();
        tab.scroll = 0;
        tab.focus_row = 0;
        tab.filter.clear();
        self.pending_select = select;
        self.focus_visible = false;
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
            self.invalidate();
        }
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
        // 回收站直读 $Recycle.Bin（不走 Shell 的 IShellFolder），几千项也是几十毫秒。
        if self.tab().recycle {
            self.recycle_items = fm_core::recycle::list();
            self.recycle_sel.clear();
            let tab = self.tab_mut();
            tab.generation += 1;
            tab.entries.clear();
            tab.visible.clear();
            tab.loading = false;
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

    pub fn cycle_view(&mut self) {
        let tab = self.tab_mut();
        tab.view_mode = tab.view_mode.next();
        self.invalidate();
    }

    /// 直接切到指定视图（Ctrl+1..4）
    pub fn set_view(&mut self, mode: ViewMode) {
        self.tab_mut().view_mode = mode;
        self.invalidate();
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

    pub fn new_folder(&mut self) {
        let dir = self.tab().path.clone();
        let owner = self.hwnd.0 as isize;
        if let Err(e) = new_folder(owner, &dir) {
            report(self.hwnd, &format!("创建失败：{e}"));
        }
        self.reload();
    }

    pub fn new_file(&mut self) {
        let dir = self.tab().path.clone();
        let name = paths::unique_name(&dir, "新建文本文档", ".txt");
        let fname = name.file_name().unwrap_or_default().to_string_lossy().to_string();
        let owner = self.hwnd.0 as isize;
        if let Err(e) = new_file(owner, &dir, &fname) {
            report(self.hwnd, &format!("创建失败：{e}"));
        }
        self.reload();
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
        crate::paint::create_rename_editor(self, &name);
    }

    pub fn commit_rename(&mut self, new_name: &str) {
        self.destroy_rename_editor();
        let Some(idx) = self.rename_index.take() else { return };
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
        if let Err(e) = clipboard::set_files(&items, mode) {
            report(self.hwnd, &format!("剪贴板写入失败：{e}"));
        }
    }

    pub fn paste(&mut self) {
        match clipboard::get_files() {
            Ok(Some((files, mode))) if !files.is_empty() => {
                let dest = self.tab().path.clone();
                if dest.to_string_lossy() == files[0].parent().map(|p| p.to_string_lossy()).unwrap_or_default() && mode == ClipMode::Copy {
                    report(self.hwnd, "源与目标相同，跳过");
                    return;
                }
                let owner = self.hwnd.0 as isize;
                let result = if self.fast_mode || self.cfg.files.operations.fast_mode {
                    self.transfer = Some(transfer_async(
                        if mode == ClipMode::Move { TransferKind::Move } else { TransferKind::Copy },
                        &dest,
                        files,
                        self.cfg.files.operations.worker_threads,
                    ));
                    Ok(())
                } else if mode == ClipMode::Move {
                    move_items(owner, &dest, &files)
                } else {
                    copy_items(owner, &dest, &files)
                };
                if let Err(e) = result {
                    report(self.hwnd, &format!("粘贴失败：{e}"));
                }
                self.reload();
            }
            Ok(Some(_)) | Ok(None) => {}
            Err(e) => report(self.hwnd, &format!("剪贴板读取失败：{e}")),
        }
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

        self.bridge = if items.is_empty() {
            None
        } else {
            menu::build_shell_bridge(self.hwnd, &parent, &items, false)
        };

        let mut entries = own_menu_rows(self, &items);
        if let Some(bridge) = &self.bridge {
            let mut system = menu::entries_from_bridge(bridge);
            // 去掉与自有条目重名的一级项：Shell 菜单里也有"打开/剪切/复制/重命名/
            // 删除/属性"，全列出来就是两个"打开"，看着像 bug。
            let owned: Vec<String> = entries.iter().map(|e| menu::strip_accel(&e.text)).collect();
            system.retain(|e| !owned.iter().any(|o| *o == menu::strip_accel(&e.text)));
            if !system.is_empty() {
                entries.push(menu::MenuEntry::sep());
                entries.extend(system);
            }
        }
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
            Command::Delete => self.delete_selected(false),
            Command::DeletePermanent => self.delete_selected(true),
            Command::NewFolder => self.new_folder(),
            Command::NewFile => self.new_file(),
            Command::SelectAll => self.select_all(),
            Command::Refresh => self.reload(),
            Command::ViewModeCycle => self.cycle_view(),
            Command::ShowNativeMenu => {
                let items = self.tab().selected_paths();
                let parent = items
                    .first()
                    .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                    .unwrap_or_else(|| self.tab().path.clone());
                let _ = menu::show_native(self.hwnd, &parent, &items);
                self.reload();
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
            Command::ToggleSidebar => self.toggle_sidebar(),
            Command::ToggleStatusbar => self.toggle_statusbar(),
            Command::ToggleFollowCurrent => self.toggle_follow_current(),
            Command::ToggleThispcExpanded => self.toggle_thispc_expanded(),
            Command::ToggleHidden => self.toggle_hidden(),
            Command::ToggleSystem => self.toggle_system(),
            Command::ToggleExtensions => self.toggle_extensions(),
            Command::CycleSizeUnit => self.cycle_size_unit(),
            Command::CycleTheme => self.cycle_theme(),
            Command::CycleDefaultView => self.cycle_default_view(),
            Command::OpenSettings => self.open_settings(),
            Command::OpenRecycle => self.open_recycle(),
            Command::RecycleRestore => self.restore_recycle(),
            Command::RecycleDelete => self.purge_recycle(),
            Command::RecycleEmpty => self.empty_recycle(),
            Command::RecycleSelectAll => self.recycle_select_all(),
            Command::NewTab => self.new_tab(),
            Command::About => report(self.hwnd, "NexFile 轻量资源管理器\nWin32 + GDI 自绘，无 WebView2 依赖"),
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

    fn persist_view_pref(&self) {
        if let Err(e) = fm_core::config::Config::save_user_patch(&serde_json::json!({
            "files": {
                "view": {
                    "show_hidden": self.cfg.files.view.show_hidden,
                    "show_system": self.cfg.files.view.show_system,
                    "show_extensions": self.cfg.files.view.show_extensions,
                    "size_unit": self.cfg.files.view.size_unit,
                    "default_mode": self.cfg.files.view.default_mode,
                }
            }
        })) {
            eprintln!("保存视图偏好失败: {e}");
        }
    }

    /// 显示/隐藏文件扩展名。资源管理器在"查看 → 显示/隐藏"里也给了这一项。
    pub fn toggle_extensions(&mut self) {
        self.cfg.files.view.show_extensions = !self.cfg.files.view.show_extensions;
        self.persist_view_pref();
        self.invalidate();
    }

    /// 切换"大小"列的默认单位（自动 / 字节 / KB / MB / GB 循环）。
    pub fn cycle_size_unit(&mut self) {
        let next = match fm_core::SizeUnit::parse(&self.cfg.files.view.size_unit) {
            fm_core::SizeUnit::Auto => "bytes",
            fm_core::SizeUnit::Bytes => "kb",
            fm_core::SizeUnit::KB => "mb",
            fm_core::SizeUnit::MB => "gb",
            fm_core::SizeUnit::GB => "auto",
        };
        self.cfg.files.view.size_unit = next.to_string();
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
    pub fn cycle_default_view(&mut self) {
        let cur = parse_view(&self.cfg.files.view.default_mode);
        self.cfg.files.view.default_mode = cur.next().label().to_string();
        self.persist_view_pref();
        self.invalidate();
    }

    // ------------------------------------------------------------ 虚拟视图

    /// 以标签页形式打开设置页。已经开过就直接切过去，避免堆一串重复标签。
    pub fn open_settings(&mut self) {
        if let Some(i) = self.tabs.iter().position(|t| t.settings) {
            self.active = i;
        } else {
            self.open_tab(PathBuf::from(SETTINGS_PATH));
        }
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
    fn persist_ui_pref(&self) {
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
        let view = self.tab().view_mode;
        let mut entries = Vec::new();
        entries.push(shell_row("新建标签页", Some("\u{E8A7}"), Command::NewTab));
        entries.push(shell_row("刷新", Some("\u{E72C}"), Command::Refresh));
        entries.push(menu::MenuEntry::sep());

        let mut view_row = menu::MenuEntry {
            text: "视图".to_string(),
            glyph: Some("\u{E7B3}"),
            bmp: None,
            separator: false,
            disabled: false,
            has_submenu: true,
            submenu: Vec::new(),
            action: MenuAction::None,
        };
        for (label, cmd, mode) in [
            ("大图标", Command::ViewIcons, ViewMode::LargeIcons),
            ("列表", Command::ViewList, ViewMode::List),
            ("详细信息", Command::ViewDetails, ViewMode::Details),
        ] {
            let mut r = shell_row(label, None, cmd);
            r.text = if view == mode {
                format!("● {label}")
            } else {
                format!("    {label}")
            };
            view_row.submenu.push(r);
        }
        view_row.submenu.push(menu::MenuEntry::sep());
        view_row
            .submenu
            .push(check_row("隐藏的项目", Command::ToggleHidden, self.show_hidden));
        view_row
            .submenu
            .push(check_row("受保护的系统文件", Command::ToggleSystem, self.show_system));
        view_row.submenu.push(check_row(
            "文件扩展名",
            Command::ToggleExtensions,
            self.cfg.files.view.show_extensions,
        ));
        entries.push(view_row);

        let mut sort_row = menu::MenuEntry {
            text: "排序方式".to_string(),
            glyph: Some("\u{E8CB}"),
            bmp: None,
            separator: false,
            disabled: false,
            has_submenu: true,
            submenu: Vec::new(),
            action: MenuAction::None,
        };
        for (label, cmd) in [
            ("名称", Command::SortByName),
            ("修改日期", Command::SortByModified),
            ("类型", Command::SortByType),
            ("大小", Command::SortBySize),
        ] {
            sort_row.submenu.push(shell_row(label, None, cmd));
        }
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
        entries.push(shell_row("关于 NexFile", Some("\u{E946}"), Command::About));

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
        ) {
            self.menu_hwnd = Some(hwnd_menu);
        }
    }

    // ------------------------------------------------------------ 模块与配置

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
            return "设置 · 改动即时写入 %APPDATA%\\NexFile\\settings.user.json".to_string();
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
        let path = if tab.home {
            "主页".to_string()
        } else {
            tab.path.display().to_string()
        };
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
        if sel > 0 {
            format!(
                "{visible} / {total} 个项目{hidden_note}{index_note}{scope_note} · 已选 {sel} 项 {bytes} · 视图 {view} · {path}",
                bytes = human_unit(bytes, unit),
                path = path
            )
        } else {
            format!("{visible} / {total} 个项目{hidden_note}{index_note}{scope_note} · 视图 {view} · {path}", path = path)
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
        rows.push(SidebarRow {
            label: "主页".to_string(),
            target: HOME_PATH.to_string(),
            depth: 0,
            expandable: false,
            expanded: false,
            key: String::new(),
            icon: SidebarIcon::Glyph("\u{E80F}"),
        });
        for (label, path) in user_folders() {
            rows.push(SidebarRow {
                label: label.to_string(),
                target: path.clone(),
                depth: 0,
                expandable: false,
                expanded: false,
                key: String::new(),
                icon: SidebarIcon::Path(PathBuf::from(path)),
            });
        }

        // 此电脑：展开就是本机全部驱动器（含 U 盘、光驱、网络盘）
        let thispc = self.is_expanded(SIDEBAR_THISPC);
        rows.push(SidebarRow {
            label: "此电脑".to_string(),
            target: HOME_PATH.to_string(),
            depth: 0,
            expandable: true,
            expanded: thispc,
            key: SIDEBAR_THISPC.to_string(),
            icon: SidebarIcon::Glyph("\u{F158}"),
        });
        if thispc {
            let mut all: Vec<_> = self.drives.values().cloned().collect();
            all.sort_by(|a, b| a.letter.cmp(&b.letter));
            for d in all {
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
                });
                if expanded {
                    let kids = self.push_children_rows_into(&key, 2, &mut rows);
                    let _ = kids;
                }
            }
        }

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
        });
        rows
    }

    /// 把某个已展开节点的子目录递归展开成行（深度上限 8，防止符号链接成环）。
    fn push_children_rows_into(&mut self, key: &str, depth: usize, out: &mut Vec<SidebarRow>) {
        if depth > 8 {
            return;
        }
        if !self.sidebar_children.contains_key(key) {
            self.load_sidebar_children(key);
        }
        let Some(children) = self.sidebar_children.get(key).cloned() else { return };
        for child in children {
            let ckey = Self::node_key(&child);
            let expanded = self.is_expanded(&ckey);
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
            });
            if expanded {
                self.push_children_rows_into(&ckey, depth + 1, out);
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
            return vec![Crumb { label: "主页".into(), target: None, ellipsis: false }];
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

fn shell_row(text: &str, glyph: Option<&'static str>, cmd: Command) -> menu::MenuEntry {
    menu::MenuEntry {
        text: text.to_string(),
        glyph,
        bmp: None,
        separator: false,
        disabled: false,
        has_submenu: false,
        submenu: Vec::new(),
        action: MenuAction::Own(cmd),
    }
}

/// 带勾选状态的条目：开 = "● 名称"，关 = 同样宽度的空白占位（保持文字对齐）。
fn check_row(text: &str, cmd: Command, on: bool) -> menu::MenuEntry {
    let mut r = shell_row(text, None, cmd);
    r.text = if on { format!("● {text}") } else { format!("    {text}") };
    r
}

/// 带下级菜单的条目。资源管理器的右键菜单本来就是分层的（新建/查看/排序方式都有
/// 下拉），做成平铺会把"悬停展开"这条交互整个丢掉。
fn submenu_row(text: &str, glyph: Option<&'static str>, children: Vec<menu::MenuEntry>) -> menu::MenuEntry {
    menu::MenuEntry {
        text: text.to_string(),
        glyph,
        bmp: None,
        separator: false,
        disabled: false,
        has_submenu: !children.is_empty(),
        submenu: children,
        action: MenuAction::None,
    }
}

/// 自绘菜单的第一屏：图标行 + 常规动作（§7 产品化解法）。
fn own_menu_rows(app: &App, items: &[PathBuf]) -> Vec<menu::MenuEntry> {
    let mut rows = Vec::new();
    if !items.is_empty() {
        rows.push(shell_row("打开", Some("\u{E8A7}"), Command::Open));
        // 「打开方式」只对文件有意义：文件夹的"打开方式"在资源管理器里也没有。
        if items.iter().any(|p| p.is_file()) {
            rows.push(shell_row("打开方式", Some("\u{E7AC}"), Command::OpenWith));
        }
        rows.push(shell_row("剪切", Some("\u{E8C6}"), Command::Cut));
        rows.push(shell_row("复制", Some("\u{E8C8}"), Command::Copy));
        rows.push(shell_row("重命名", Some("\u{E8AC}"), Command::Rename));
        rows.push(shell_row("删除", Some("\u{E74D}"), Command::Delete));
        rows.push(menu::MenuEntry::sep());
    }
    rows.push(shell_row("粘贴", Some("\u{E77F}"), Command::Paste));
    rows.push(submenu_row(
        "新建",
        Some("\u{E8F4}"),
        vec![
            shell_row("文件夹", None, Command::NewFolder),
            shell_row("文本文档", None, Command::NewFile),
        ],
    ));
    rows.push(menu::MenuEntry::sep());
    rows.push(shell_row("全选", Some("\u{E762}"), Command::SelectAll));
    rows.push(shell_row("刷新", Some("\u{E72C}"), Command::Refresh));
    rows.push(submenu_row(
        "查看",
        Some("\u{E7B3}"),
        vec![
            shell_row("大图标", None, Command::ViewIcons),
            shell_row("列表", None, Command::ViewList),
            shell_row("详细信息", None, Command::ViewDetails),
            menu::MenuEntry::sep(),
            // 三道开关分开列：隐藏项是用户自己设的，系统项是系统保护的，
            // 扩展名是纯显示开关 —— 混成一个开关没法单独控制。
            check_row("隐藏的项目", Command::ToggleHidden, app.show_hidden),
            check_row("受保护的系统文件", Command::ToggleSystem, app.show_system),
            check_row("文件扩展名", Command::ToggleExtensions, app.cfg.files.view.show_extensions),
        ],
    ));
    rows.push(submenu_row(
        "排序方式",
        Some("\u{E8CB}"),
        vec![
            shell_row("名称", None, Command::SortByName),
            shell_row("修改日期", None, Command::SortByModified),
            shell_row("类型", None, Command::SortByType),
            shell_row("大小", None, Command::SortBySize),
        ],
    ));
    rows.push(shell_row(
        if items.is_empty() { "属性（当前文件夹）" } else { "属性" },
        Some("\u{E946}"),
        Command::Properties,
    ));
    rows.push(menu::MenuEntry::sep());
    rows.push(shell_row("设置", Some("\u{E713}"), Command::OpenSettings));
    // 无选中项时给出背景菜单语义说明
    if items.is_empty() {
        rows.push(menu::MenuEntry::sep());
        let mut note = shell_row("当前为空文件夹区域", None, Command::Refresh);
        note.disabled = true;
        rows.push(note);
    }
    rows
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
    let mut title: Vec<u16> = "NexFile".encode_utf16().chain(std::iter::once(0)).collect();
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
