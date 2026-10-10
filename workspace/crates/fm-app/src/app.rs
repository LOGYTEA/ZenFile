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
use fm_core::types::{
    filter_entries, FileEntry, SortKey, SortSpec, ViewMode,
};
use fm_core::watch::{DirWatcher, WatchMsg};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::InvalidateRect;

use crate::icons::IconCache;
use crate::menu::{self, Command, MenuAction, ShellMenuBridge};
use crate::theme::{self, Metrics, Theme};

/// 主页（快速访问）的哨兵路径。它不是真实目录，由 App 合成条目。
pub const HOME_PATH: &str = "shell:Home";

pub fn is_home_path(p: &Path) -> bool {
    p.to_string_lossy().eq_ignore_ascii_case(HOME_PATH)
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

    for d in paths::logical_drives() {
        let letter = d.trim_end_matches('\\').to_string();
        out.push(mk(&format!("本地磁盘 ({letter})"), d));
    }
    out
}

pub struct Tab {
    pub path: PathBuf,
    pub title: String,
    /// 主页（快速访问）虚拟目录，不参与真实枚举
    pub home: bool,
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
        } else {
            display_title(&path)
        };
        Self {
            path: path.clone(),
            title,
            home: is_home_path(&path),
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

    pub fn recompute_filter(&mut self) {
        self.visible = filter_entries(&self.entries, &self.filter);
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

    pub fn content_height(&self, mode: ViewMode, view_h: i32, metrics: &Metrics) -> i32 {
        self.visible.len() as i32 * metrics.row_height_for(mode) - view_h
    }

    pub fn clamp_scroll(&mut self, view_h: i32, metrics: &Metrics) {
        let max = self.content_height(self.view_mode, view_h, metrics).max(0);
        self.scroll = self.scroll.min(max).max(0);
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
    /// 主页"设备和驱动器"的容量缓存：`盘符 -> (总容量, 可用)`。
    /// 只在进入主页时刷新一次，避免每帧都做磁盘 IO。
    pub drive_space: HashMap<String, (u64, u64)>,
    /// 焦点行要不要**画**出来。
    ///
    /// `focus_row` 永远存在（第 0 行默认就是焦点行），但资源管理器在刚打开一个
    /// 文件夹时，是不会给第 0 行刷一条高亮的 —— 那一行看起来就像"默认被勾选了"。
    /// 只有用户真的用鼠标点过或用键盘移动过，焦点行才显形。
    pub focus_visible: bool,
}

/// 取某个驱动器的容量`(总容量, 可用)`，供首页渲染容量条。
pub fn drive_space_of(app: &App, path: &Path) -> Option<(u64, u64)> {
    let key = path.to_string_lossy().to_uppercase();
    app.drive_space.get(&key).copied()
}

pub const HOVER_SIDEBAR_TOGGLE: u8 = 0b01;
pub const HOVER_MENU_BTN: u8 = 0b10;

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
            drive_space: HashMap::new(),
            focus_visible: false,
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
        self.reload();
        self.invalidate();
    }

    pub fn navigate(&mut self, path: PathBuf) {
        // 入口就把 verbatim 前缀剥掉：调用方可能传的是 canonicalize 的结果
        // （带 `\\?\`），那个前缀会让目录枚举返回空列表。
        let path = fm_core::paths::strip_verbatim(&path);
        let depth = self.cfg.files.navigation.history_depth;
        {
            let tab = self.tab_mut();
            tab.home = is_home_path(&path);
            tab.path = path.clone();
            tab.title = if tab.home { "主页".to_string() } else { display_title(&path) };
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
        self.tab_mut().history_pos -= 1;
        let target = self.tab().history[self.tab().history_pos].clone();
        self.apply_history(&target);
    }

    pub fn go_forward(&mut self) {
        if !self.can_forward() {
            return;
        }
        self.tab_mut().history_pos += 1;
        let target = self.tab().history[self.tab().history_pos].clone();
        self.apply_history(&target);
    }

    fn apply_history(&mut self, target: &Path) {
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
        self.reload();
        self.invalidate();
    }

    pub fn go_up(&mut self) {
        // 主页已经是顶层，没有上级
        if self.tab().home {
            return;
        }
        if let Some(parent) = paths::parent_of(&self.tab().path) {
            self.navigate(parent);
        }
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
            // 容量条数据一次性取好：sort 之后局势固定，取数失败就不画条
            self.drive_space.clear();
            for e in &entries {
                if fm_core::paths::is_drive_root(&e.path) {
                    let key = e.path.to_string_lossy().to_uppercase();
                    if let Some(space) = fm_core::paths::drive_space(&e.path.to_string_lossy()) {
                        self.drive_space.insert(key, space);
                    }
                }
            }
            let tab = self.tab_mut();
            tab.generation += 1;
            tab.entries = entries;
            tab.loading = false;
            tab.recompute_filter();
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

    pub fn drain(&mut self) {
        let mut changed = false;
        while let Ok(msg) = self.engine_rx.try_recv() {
            match msg {
                EngineMsg::Batch(batch) => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.generation == batch.generation) {
                        if batch.done {
                            tab.loading = false;
                        }
                        tab.entries.extend(batch.entries);
                        tab.sort.sort(&mut tab.entries);
                        tab.recompute_filter();
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

    pub fn select_all(&mut self) {
        let tab = self.tab_mut();
        tab.selected.clear();
        for e in &tab.entries {
            tab.selected.insert(key_of(e));
        }
    }

    pub fn sort_column(&mut self, key: SortKey) {
        let tab = self.tab_mut();
        tab.sort.toggle(key);
        tab.sort.sort(&mut tab.entries);
        tab.recompute_filter();
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
        let items = self.tab().selected_paths();
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
            let system = menu::entries_from_bridge(bridge);
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
                // 只有目标本身是"主页"这个虚拟视图时才无属性可开 —— 主页里选中
                // 的条目（桌面/下载/…）是真实目录，必须能正常打开属性。
                let target = self
                    .tab()
                    .selected_paths()
                    .first()
                    .cloned()
                    .unwrap_or_else(|| self.tab().path.clone());
                if crate::app::is_home_path(&target) {
                    report(self.hwnd, "“主页”是虚拟视图，没有对应的文件夹属性");
                } else {
                    self.show_properties(&target);
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

    pub fn toggle_statusbar(&mut self) {
        self.cfg.ui.statusbar_visible = !self.cfg.ui.statusbar_visible;
        self.persist_ui_pref();
        crate::paint::reposition_chrome(self);
        let _ = unsafe { InvalidateRect(Some(self.hwnd), None, true) };
    }

    /// 只把 UI 开关写回 settings.user.json，不覆盖其它字段。
    fn persist_ui_pref(&self) {
        if let Err(e) = fm_core::config::Config::save_user_patch(&serde_json::json!({
            "ui": {
                "sidebar_visible": self.cfg.ui.sidebar_visible,
                "statusbar_visible": self.cfg.ui.statusbar_visible,
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

    /// 打开资源管理器同款"属性"对话框。
    ///
    /// 关键点：必须走 `ShellExecuteExW` + `SEE_MASK_INVOKEIDLIST`。
    /// 只用 `ShellExecuteW("properties")` 时，Shell 不会把动词交给该项的
    /// IContextMenu，"属性"对话框经常弹不出来（这是原来点它没反应的原因）。
    fn show_properties(&self, path: &Path) -> bool {
        use windows::Win32::UI::Shell::{
            ShellExecuteExW, SHELLEXECUTEINFOW, SEE_MASK_FLAG_NO_UI, SEE_MASK_INVOKEIDLIST,
            SEE_MASK_NOCLOSEPROCESS,
        };
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOW;
        let file: Vec<u16> = path
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let verb: Vec<u16> = "properties\0".encode_utf16().collect();
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_INVOKEIDLIST | SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI,
            hwnd: self.hwnd,
            lpVerb: windows::core::PCWSTR(verb.as_ptr()),
            lpFile: windows::core::PCWSTR(file.as_ptr()),
            nShow: SW_SHOW.0 as i32,
            ..Default::default()
        };
        unsafe {
            if ShellExecuteExW(&mut info).is_err() {
                report(self.hwnd, "无法打开属性对话框");
                return false;
            }
        }
        true
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
        if sel > 0 {
            format!("{visible} / {total} 个项目 · 已选 {sel} 项 {bytes} · 视图 {view} · {path}", bytes = human(bytes), path = path)
        } else {
            format!("{visible} / {total} 个项目 · 视图 {view} · {path}", path = path)
        }
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
    // 无选中项时给出背景菜单语义说明
    if items.is_empty() {
        rows.push(menu::MenuEntry::sep());
        let mut note = shell_row("当前为空文件夹区域", None, Command::Refresh);
        note.disabled = true;
        rows.push(note);
    }
    let _ = app;
    rows
}

pub fn human(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else if bytes > 0 {
        format!("{bytes} 字节")
    } else {
        "0 字节".to_string()
    }
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
