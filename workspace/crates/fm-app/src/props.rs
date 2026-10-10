//! 自绘"属性"对话框（替代 Windows 原生属性页）。
//!
//! 为什么不用原生：`ShellExecuteExW("properties")` 走的是 IContextMenu::InvokeCommand，
//! Shell 会为该项加载全部属性页扩展、构建 IShellItem2 属性存储 —— 打开一个 exe
//! 常常要等 1~3 秒才出窗口。这里只做最小 IO：
//!   - 一次 `std::fs::metadata`（大小 / 三个时间戳）
//!   - 一次 `GetFileAttributesW`（属性位）
//!   - 一次 `SHGetFileInfoW`（类型名 + 图标，图标走本进程已有缓存）
//! 文件夹的"包含 N 个项目"这类递归统计放**后台线程**，窗口先显示、数字后到。
//! 所以从点下"属性"到看见窗口，正常是几毫秒量级。
//!
//! 窗口是自绘的（WS_POPUP + GDI），和主窗口共用同一个消息循环 —— 不进嵌套模态循环，
//! 避免"属性窗口开着时主窗口完全卡住"。
//!
//! 标签页照资源管理器分：常规 / 安全 / 自定义（仅文件夹）/ 详细信息。
//!   * 安全：一次 `GetNamedSecurityInfoW` 读整张 DACL，SID → 账户名；
//!   * 自定义：改 `desktop.ini`（文件夹模板 / 文件夹图片 / 文件夹图标）。

use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    COLORREF, FILETIME, HWND, LPARAM, LRESULT, POINT, RECT, SYSTEMTIME, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetTextExtentPoint32W,
    SelectObject, SetBkMode, SetTextColor, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX,
    DT_SINGLELINE, DT_VCENTER, DRAW_TEXT_FORMAT, HDC, HFONT, InvalidateRect, PAINTSTRUCT, SRCCOPY,
    TRANSPARENT,
};
use windows::Win32::Storage::FileSystem::{
    FileTimeToLocalFileTime, GetDiskFreeSpaceW, SetFileAttributesW, FILE_ATTRIBUTE_ARCHIVE,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SYSTEM,
    FILE_FLAGS_AND_ATTRIBUTES,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Time::FileTimeToSystemTime;
use windows::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OPENFILENAMEW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::Shell::{
    ExtractIconW, SHGetFileInfoW, SHFILEINFOW, SHGFI_TYPENAME,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DrawIconEx, GetWindowLongPtrW, PostMessageW,
    RegisterClassExW, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    GWLP_USERDATA, HICON, SET_WINDOW_POS_FLAGS, SWP_NOZORDER, SW_SHOW, DI_NORMAL, WM_CLOSE,
    WM_DESTROY, WM_ERASEBKGND, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCHITTEST, WM_PAINT,
    WNDCLASSEXW, WS_POPUP,
};

use crate::app::App;
use crate::theme::Theme;

/// 后台统计线程算完之后通知窗口重画
const WM_STAT_DONE: u32 = 0x8001;
/// 自绘下拉框选中了某一项：wparam = 下标
const WM_COMBO_PICK: u32 = 0x8002;

const TITLE_H: i32 = 34;
/// 标签条高度（内容区从它下面开始）
const TAB_H: i32 = 30;
const BASE_W: i32 = 450;
const BASE_H: i32 = 512;
/// 信息行的行高（基准 DPI）
const ROW_H: i32 = 26;
const LABEL_W: i32 = 78;

/// 内容区相对窗口顶部的偏移（标签条下面）
const CONTENT_TOP: i32 = TITLE_H + TAB_H;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    General,
    Security,
    /// 只有文件夹才有（资源管理器也是这样）
    Custom,
    Details,
}

const TAB_LABELS: [(&str, Tab); 4] = [
    ("常规", Tab::General),
    ("安全", Tab::Security),
    ("自定义", Tab::Custom),
    ("详细信息", Tab::Details),
];

/// 命中测试的结果。用枚举而不是裸下标 —— 四个页面各自的控件编号会撞车，
/// 裸下标早晚点错地方（侧栏行号那次就是这么踩的）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    None,
    Close,
    Ok,
    Cancel,
    Tab(Tab),
    /// 四个属性复选框
    Attr(usize),
    /// 安全页里的 ACE 行
    Ace(usize),
    /// 自定义页：文件夹模板下拉
    TypeCombo,
    /// 自定义页：把模板应用到子文件夹
    ApplySub,
    PickImage,
    ClearImage,
    PickIcon,
    ClearIcon,
}

#[derive(Clone, Copy, Default)]
struct DirStat {
    files: u64,
    dirs: u64,
    bytes: u64,
    done: bool,
}

struct Props {
    app: *mut App,
    path: PathBuf,
    name: String,
    type_name: String,
    location: String,
    size: u64,
    alloc: u64,
    created: String,
    modified: String,
    accessed: String,
    attrs: u32,
    /// 打开时的属性位：`取消`要还原到它
    orig_attrs: u32,
    is_dir: bool,
    icon: HICON,
    stat: Arc<Mutex<DirStat>>,
    hover: Option<Hit>,
    scale: f32,
    theme: Theme,
    bold: HFONT,
    w: i32,
    h: i32,

    tab: Tab,
    /// 安全页：切过去才读，避免每个属性窗口都白读一次 ACL
    sec: Option<fm_core::acl::Security>,
    /// 安全页里选中的 ACE 行
    sec_sel: usize,
    /// ACE 列表的滚动位置
    sec_scroll: usize,
    /// 自定义页：`desktop.ini` 的当前内容（落盘前的编辑态）
    ini: fm_core::folder_ini::FolderIni,
    /// 自定义页：模板下标
    type_index: usize,
    /// 自定义页：是否把模板应用到子文件夹
    apply_sub: bool,
    /// 自定义页：文件夹图标的预览句柄（自 ExtractIconW 取，要自己销毁）
    custom_icon: HICON,
}

impl Props {
    fn s(&self, v: i32) -> i32 {
        (v as f32 * self.scale).round() as i32
    }
    fn content_top(&self) -> i32 {
        self.s(CONTENT_TOP)
    }

    fn tab_rect(&self, t: Tab) -> RECT {
        let mut x = self.s(12);
        let top = self.s(TITLE_H);
        let bottom = self.s(CONTENT_TOP);
        for (label, tt) in TAB_LABELS {
            if tt == Tab::Custom && !self.is_dir {
                continue;
            }
            let w = self.s(20 + label.chars().count() as i32 * 13);
            if tt == t {
                return RECT { left: x, top, right: x + w, bottom };
            }
            x += w + self.s(2);
        }
        RECT::default()
    }
    fn visible_tabs(&self) -> Vec<Tab> {
        TAB_LABELS
            .iter()
            .filter(|(_, t)| *t != Tab::Custom || self.is_dir)
            .map(|(_, t)| *t)
            .collect()
    }

    /// 信息行矩形（绘制与命中测试共用，避免两边各写一份常量而错位）
    fn row_rect(&self, i: usize) -> RECT {
        let top = self.content_top() + self.s(74) + i as i32 * self.s(ROW_H);
        RECT { left: self.s(20), top, right: self.w - self.s(20), bottom: top + self.s(ROW_H) }
    }
    fn check_rect(&self, i: usize) -> RECT {
        let top = self.content_top() + self.s(282) + i as i32 * self.s(26);
        RECT { left: self.s(20), top, right: self.w - self.s(20), bottom: top + self.s(26) }
    }
    fn ok_rect(&self) -> RECT {
        let h = self.s(28);
        let top = self.h - self.s(44);
        let right = self.w - self.s(20) - self.s(92) - self.s(8);
        RECT { left: right - self.s(92), top, right, bottom: top + h }
    }
    fn cancel_rect(&self) -> RECT {
        let h = self.s(28);
        let top = self.h - self.s(44);
        RECT { left: self.w - self.s(20) - self.s(92), top, right: self.w - self.s(20), bottom: top + h }
    }
    fn close_rect(&self) -> RECT {
        RECT { left: self.w - self.s(44), top: 0, right: self.w, bottom: self.s(TITLE_H) }
    }

    // ---- 安全页 ----
    fn ace_rect(&self, i: usize) -> RECT {
        let top = self.content_top() + self.s(88) + i as i32 * self.s(22);
        RECT { left: self.s(20), top, right: self.w - self.s(20), bottom: top + self.s(22) }
    }
    /// 列表最多显示几行
    fn ace_visible(&self) -> usize {
        8
    }
    fn ace_view_rect(&self) -> RECT {
        let top = self.content_top() + self.s(88);
        RECT { left: self.s(20), top, right: self.w - self.s(20), bottom: top + self.ace_visible() as i32 * self.s(22) }
    }
    fn perm_rect(&self, i: usize) -> RECT {
        let cols: i32 = 2;
        let (c, r) = (i as i32 % cols, i as i32 / cols);
        let left = self.s(20) + c * ((self.w - self.s(40)) / cols);
        let top = self.content_top() + self.s(294) + r * self.s(24);
        RECT { left, top, right: left + (self.w - self.s(40)) / cols, bottom: top + self.s(24) }
    }

    // ---- 自定义页 ----
    fn type_combo_rect(&self) -> RECT {
        let top = self.content_top() + self.s(30);
        RECT { left: self.s(20), top, right: self.s(220), bottom: top + self.s(26) }
    }
    fn apply_sub_rect(&self) -> RECT {
        let top = self.content_top() + self.s(64);
        RECT { left: self.s(20), top, right: self.w - self.s(20), bottom: top + self.s(26) }
    }
    fn image_box_rect(&self) -> RECT {
        let top = self.content_top() + self.s(128);
        RECT { left: self.s(20), top, right: self.s(20) + self.s(96), bottom: top + self.s(72) }
    }
    fn image_path_rect(&self) -> RECT {
        let top = self.content_top() + self.s(128);
        RECT { left: self.s(128), top, right: self.w - self.s(20), bottom: top + self.s(24) }
    }
    fn btn_rect(&self, top: i32, left: i32, width: i32) -> RECT {
        let top = self.content_top() + self.s(top);
        RECT { left: self.s(left), top, right: self.s(left) + self.s(width), bottom: top + self.s(26) }
    }
    fn pick_image_rect(&self) -> RECT {
        self.btn_rect(160, 128, 104)
    }
    fn clear_image_rect(&self) -> RECT {
        self.btn_rect(160, 240, 96)
    }
    fn custom_icon_rect(&self) -> RECT {
        let top = self.content_top() + self.s(240);
        RECT { left: self.s(20), top, right: self.s(20) + self.s(32), bottom: top + self.s(32) }
    }
    fn custom_icon_path_rect(&self) -> RECT {
        let top = self.content_top() + self.s(240);
        RECT { left: self.s(64), top, right: self.w - self.s(20), bottom: top + self.s(24) }
    }
    fn pick_icon_rect(&self) -> RECT {
        self.btn_rect(278, 64, 104)
    }
    fn clear_icon_rect(&self) -> RECT {
        self.btn_rect(278, 176, 96)
    }
}

pub fn register_props_class() {
    let cls = HSTRING::from("NexFileProps");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(props_proc),
        lpszClassName: PCWSTR(cls.as_ptr()),
        ..Default::default()
    };
    unsafe {
        RegisterClassExW(&wc);
    }
    register_combo_class();
}

/// 打开属性窗口。`app_ptr` 用于取主题、DPI、图标缓存与刷新列表。
pub fn open(app_ptr: *mut App, path: PathBuf) {
    if app_ptr.is_null() {
        return;
    }
    let app = unsafe { &mut *app_ptr };
    let scale = app.metrics.scale;
    let theme = app.theme;

    // 属性与时间戳一律走 `FindFirstFileW`：
    // `std::fs::metadata` 对一部分路径会直接失败（超长路径、末尾带空格/点的名字、
    // 重解析点、回收站里的 `$R` 实体…），一旦失败这里就整片空 —— 表现就是
    // "有的文件属性是空的"。FindFirstFileW 一次调用就把属性位、大小、三个
    // FILETIME 全部带回，比 metadata 少一次系统调用，覆盖面也更大。
    let st = stat_of(&path);
    let attrs = st.map(|s| s.attrs).unwrap_or_else(|| unsafe {
        windows::Win32::Storage::FileSystem::GetFileAttributesW(&HSTRING::from(
            path.to_string_lossy().as_ref(),
        ))
    });
    let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
    let size = st.map(|s| s.size).unwrap_or(0);
    let created = st.map(|s| fmt_filetime(s.created)).unwrap_or_else(unavailable);
    let modified = st.map(|s| fmt_filetime(s.modified)).unwrap_or_else(unavailable);
    let accessed = st.map(|s| fmt_filetime(s.accessed)).unwrap_or_else(unavailable);

    let type_name = type_name_of(&path, is_dir);
    let location = path
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());

    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let icon = if is_dir {
        app.icons.dir_icon(&path, true)
    } else {
        app.icons.file_icon(&path, &ext, true)
    };

    // 自定义页的初值：现在盘上是什么样，页上就先显示什么样
    let ini = if is_dir {
        fm_core::folder_ini::read(&path)
    } else {
        fm_core::folder_ini::FolderIni::default()
    };
    let type_index = ini.type_index();

    let w = (BASE_W as f32 * scale).round() as i32;
    let h = (BASE_H as f32 * scale).round() as i32;
    let bold = unsafe { crate::paint::make_font((-15.0 * scale) as i32, 700, "Segoe UI") };

    let stat = Arc::new(Mutex::new(DirStat::default()));
    let state = Box::into_raw(Box::new(Props {
        app: app_ptr,
        path: path.clone(),
        name,
        type_name,
        location,
        size,
        alloc: alloc_size_of(&path, size, is_dir),
        created,
        modified,
        accessed,
        attrs,
        orig_attrs: attrs,
        is_dir,
        icon,
        stat: stat.clone(),
        hover: None,
        scale,
        theme,
        bold,
        w,
        h,
        tab: Tab::General,
        sec: None,
        sec_sel: 0,
        sec_scroll: 0,
        ini,
        type_index,
        apply_sub: false,
        custom_icon: HICON::default(),
    }));

    let cls = HSTRING::from("NexFileProps");
    let hwnd = unsafe {
        CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WS_EX_WINDOWEDGE,
            PCWSTR(cls.as_ptr()),
            PCWSTR(HSTRING::from("属性").as_ptr()),
            WS_POPUP,
            0,
            0,
            w,
            h,
            Some(app.hwnd),
            None,
            None,
            Some(state as *const core::ffi::c_void),
        )
    };
    let hwnd = match hwnd {
        Ok(h) => h,
        Err(_) => {
            drop(unsafe { Box::from_raw(state) });
            return;
        }
    };
    unsafe {
        let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
    }
    // 居中：拿主窗口矩形算一次即可
    unsafe {
        let mut pr = RECT::default();
        let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(app.hwnd, &mut pr);
        let cx = (pr.left + pr.right) / 2 - w / 2;
        let cy = (pr.top + pr.bottom) / 2 - h / 2;
        let _ = SetWindowPos(hwnd, None, cx, cy, 0, 0, SET_WINDOW_POS_FLAGS(0x0001) | SWP_NOZORDER);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = InvalidateRect(Some(hwnd), None, true);
    }

    // 文件夹的递归统计：后台慢慢算，算完通知窗口重画（不阻塞显示）
    if is_dir {
        let thread_stat = stat.clone();
        // HWND 是裸指针，不是 Send：过线程边界时转成 usize，进去之后再还原。
        // （窗口在整个统计期间不会销毁 —— 销毁时线程已被 Join 不了，但 Post 到
        //   已失效句柄只是返回 false，不会崩。）
        let hwnd_raw = hwnd.0 as usize;
        std::thread::spawn(move || {
            let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
            let mut acc = DirStat::default();
            let mut last_post = std::time::Instant::now();
            walk(&path, &mut acc, &thread_stat, &mut last_post, hwnd, 0);
            if let Ok(mut g) = thread_stat.lock() {
                g.done = true;
            }
            let _ = unsafe { PostMessageW(Some(hwnd), WM_STAT_DONE, WPARAM(0), LPARAM(0)) };
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn walk(
    dir: &Path,
    acc: &mut DirStat,
    shared: &Arc<Mutex<DirStat>>,
    last_post: &mut std::time::Instant,
    hwnd: HWND,
    depth: usize,
) {
    if depth > 32 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(m) = e.metadata() else { continue };
        if m.is_dir() {
            acc.dirs += 1;
            let p = e.path();
            walk(&p, acc, shared, last_post, hwnd, depth + 1);
        } else {
            acc.files += 1;
            acc.bytes += m.len();
        }
    }
    // 每 120ms 同步一次进度：窗口上的数字是"活"的，而不是最后一次性跳变
    if last_post.elapsed() > Duration::from_millis(120) {
        *last_post = std::time::Instant::now();
        if let Ok(mut g) = shared.lock() {
            *g = *acc;
        }
        let _ = unsafe { PostMessageW(Some(hwnd), WM_STAT_DONE, WPARAM(0), LPARAM(0)) };
    }
}

/// 类型名：一次 SHGetFileInfoW，比原生属性页构建属性存储快一个数量级。
fn type_name_of(path: &Path, is_dir: bool) -> String {
    let s = path.to_string_lossy();
    let mut sfi = SHFILEINFOW::default();
    let res = unsafe {
        SHGetFileInfoW(
            &HSTRING::from(s.as_ref()),
            if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_FLAGS_AND_ATTRIBUTES(0) },
            Some(&mut sfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_TYPENAME,
        )
    };
    if res != 0 {
        let v: Vec<u16> = sfi.szTypeName.iter().take_while(|&&c| c != 0).copied().collect();
        let t = crate::icons::from_wide(&v);
        if !t.is_empty() {
            return t;
        }
    }
    if is_dir {
        "文件夹".to_string()
    } else {
        match path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default()
            .as_str()
        {
            "exe" => "应用程序".to_string(),
            "dll" => "应用程序扩展".to_string(),
            "lnk" => "快捷方式".to_string(),
            "" => "文件".to_string(),
            e => format!("{e} 文件"),
        }
    }
}

/// 占用空间：按簇大小向上取整。簇大小取不到就按 4 KiB 兜底。
fn alloc_size_of(path: &Path, size: u64, is_dir: bool) -> u64 {
    if is_dir || size == 0 {
        return size;
    }
    let root = path.to_string_lossy().chars().take(3).collect::<String>();
    let mut spc = 0u32;
    let mut bps = 0u32;
    let mut free = 0u32;
    let mut total = 0u32;
    let ok = unsafe {
        GetDiskFreeSpaceW(
            &HSTRING::from(root.as_str()),
            Some(&mut spc),
            Some(&mut bps),
            Some(&mut free),
            Some(&mut total),
        )
    };
    let cluster = if ok.is_ok() && spc > 0 && bps > 0 {
        spc as u64 * bps as u64
    } else {
        4096
    };
    ((size + cluster - 1) / cluster) * cluster
}

/// 一次 `FindFirstFileW` 取回的全部静态信息。
#[derive(Clone, Copy)]
struct Stat {
    attrs: u32,
    size: u64,
    created: i64,
    modified: i64,
    accessed: i64,
}

fn ft_i64(ft: FILETIME) -> i64 {
    ((ft.dwHighDateTime as i64) << 32) | ft.dwLowDateTime as i64
}

/// 读不到时不要显示空白 —— 空白会被当成"这个字段本来就没值"。
fn unavailable() -> String {
    "（无法读取）".to_string()
}

fn stat_of(path: &Path) -> Option<Stat> {
    use windows::Win32::Storage::FileSystem::{FindClose, FindFirstFileW, WIN32_FIND_DATAW};
    unsafe {
        let raw = path.to_string_lossy().to_string();
        // 先按原样试；失败再补 `\\?\` 前缀（Win32 长路径约定）重试一次 ——
        // 超过 MAX_PATH 的路径不加前缀是打不开的。
        let candidates: Vec<String> = if raw.starts_with("\\\\?\\") {
            vec![raw.clone()]
        } else {
            let abs = std::fs::canonicalize(path)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let verbatim = if abs.is_empty() {
                format!("\\\\?\\{raw}")
            } else {
                format!("\\\\?\\{}", abs.trim_start_matches("\\\\?\\"))
            };
            vec![raw.clone(), verbatim]
        };
        for c in candidates {
            let mut fd = WIN32_FIND_DATAW::default();
            let h = FindFirstFileW(&HSTRING::from(c.as_str()), &mut fd);
            let Ok(h) = h else { continue };
            let _ = FindClose(h);
            return Some(Stat {
                attrs: fd.dwFileAttributes,
                size: ((fd.nFileSizeHigh as u64) << 32) | fd.nFileSizeLow as u64,
                created: ft_i64(fd.ftCreationTime),
                modified: ft_i64(fd.ftLastWriteTime),
                accessed: ft_i64(fd.ftLastAccessTime),
            });
        }
        // FindFirstFile 也不行：退回 std::fs::metadata，最后一道兜底
        let m = std::fs::metadata(path).ok()?;
        let to_ft = |t: Option<SystemTime>| -> i64 { t.and_then(to_filetime).map(ft_i64).unwrap_or(0) };
        Some(Stat {
            attrs: m.file_attributes(),
            size: m.len(),
            created: to_ft(m.created().ok()),
            modified: to_ft(m.modified().ok()),
            accessed: to_ft(m.accessed().ok()),
        })
    }
}

fn fmt_filetime(ft: i64) -> String {
    if ft <= 0 {
        return "—".to_string();
    }
    let ft = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
    unsafe {
        let mut local = FILETIME::default();
        if FileTimeToLocalFileTime(&ft, &mut local).is_err() {
            return "—".to_string();
        }
        let mut st = SYSTEMTIME::default();
        if FileTimeToSystemTime(&local, &mut st).is_err() {
            return "—".to_string();
        }
        format!(
            "{:04}/{:02}/{:02} {:02}:{:02}:{:02}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
        )
    }
}

fn to_filetime(t: SystemTime) -> Option<FILETIME> {
    let d = t.duration_since(UNIX_EPOCH).ok()?;
    let secs = d.as_secs() + 11_644_473_600; // 1601-01-01 → 1970-01-01
    let v = secs * 10_000_000 + (d.subsec_nanos() / 100) as u64;
    Some(FILETIME { dwLowDateTime: v as u32, dwHighDateTime: (v >> 32) as u32 })
}

fn fmt_time(t: Option<SystemTime>) -> String {
    let Some(t) = t else { return "—".to_string() };
    let Some(ft) = to_filetime(t) else { return "—".to_string() };
    unsafe {
        let mut local = FILETIME::default();
        if FileTimeToLocalFileTime(&ft, &mut local).is_err() {
            return "—".to_string();
        }
        let mut st = SYSTEMTIME::default();
        if FileTimeToSystemTime(&local, &mut st).is_err() {
            return "—".to_string();
        }
        format!(
            "{:04}/{:02}/{:02} {:02}:{:02}:{:02}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
        )
    }
}

fn human(bytes: u64) -> String {
    crate::app::human(bytes)
}

unsafe fn fill(hdc: HDC, r: &RECT, color: COLORREF) {
    let b = CreateSolidBrush(color);
    let _ = FillRect(hdc, r, b);
    let _ = DeleteObject(b.into());
}

unsafe fn text(hdc: HDC, s: &str, r: RECT, color: COLORREF, flags: DRAW_TEXT_FORMAT) {
    SetTextColor(hdc, color);
    let mut v: Vec<u16> = s.encode_utf16().collect();
    let _ = DrawTextW(hdc, &mut v, &mut r.clone(), flags | DT_NOPREFIX);
}

fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x <= r.right && y >= r.top && y <= r.bottom
}

// ---------------------------------------------------------------- 消息循环

unsafe extern "system" fn props_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Props;
    if state_ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let state = &mut *state_ptr;

    match msg {
        WM_ERASEBKGND => return LRESULT(1),
        WM_NCHITTEST => {
            let y = (lparam.0 >> 16) as i16 as i32;
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let mut sp = POINT { x, y };
            let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut sp);
            if sp.y < state.s(TITLE_H) {
                // 关闭按钮要能收到点击；其余部分交回给系统当作标题栏拖拽
                if sp.x >= state.close_rect().left {
                    return LRESULT(1); // HTCLIENT
                }
                return LRESULT(2); // HTCAPTION
            }
            return LRESULT(1); // HTCLIENT
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mem = CreateCompatibleDC(Some(hdc));
            let bmp = CreateCompatibleBitmap(hdc, state.w, state.h);
            let old = SelectObject(mem, bmp.into());
            paint_props(mem, state);
            let _ = BitBlt(hdc, 0, 0, state.w, state.h, Some(mem), 0, 0, SRCCOPY);
            SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = (lparam.0 >> 16) as i16 as i32;
            let h = hit_test(state, x, y);
            if Some(h) != state.hover {
                state.hover = Some(h);
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            // 安全页的 ACE 列表可以滚。delta 取符号即可，一次滚一行。
            if state.tab == Tab::Security {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let n = state.sec.as_ref().map(|s| s.aces.len()).unwrap_or(0);
                let max_scroll = n.saturating_sub(state.ace_visible());
                let cur = state.sec_scroll as i32;
                let next = if delta < 0 { cur + 1 } else { cur - 1 }.clamp(0, max_scroll as i32);
                if next != cur {
                    state.sec_scroll = next as usize;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_COMBO_PICK => {
            let i = wparam.0;
            if i < fm_core::folder_ini::FOLDER_TYPES.len() {
                state.type_index = i;
                let (_, key, res) = fm_core::folder_ini::FOLDER_TYPES[i];
                state.ini.folder_type = Some(key.to_string());
                state.ini.icon_resource = res.map(|s| s.to_string());
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = (lparam.0 >> 16) as i16 as i32;
            match hit_test(state, x, y) {
                Hit::Close => {
                    let _ = DestroyWindow(hwnd);
                }
                Hit::Cancel => {
                    // 属性复选框是"点了就落盘"的（用户要的就是立刻生效），
                    // 所以取消必须把属性位还原回去，否则"取消"就是假的。
                    if state.attrs != state.orig_attrs {
                        set_attrs(state, state.orig_attrs);
                    }
                    let _ = DestroyWindow(hwnd);
                }
                Hit::Ok => {
                    if state.tab == Tab::Custom {
                        apply_custom(state, hwnd);
                    }
                    let _ = DestroyWindow(hwnd);
                }
                Hit::Tab(t) => {
                    state.tab = t;
                    if t == Tab::Security && state.sec.is_none() {
                        state.sec = Some(fm_core::acl::read(&state.path));
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                Hit::Attr(i) => toggle_attr(state, i, hwnd),
                Hit::Ace(i) => {
                    state.sec_sel = state.sec_scroll + i;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                Hit::TypeCombo => {
                    let r = state.type_combo_rect();
                    let mut pt = POINT { x: r.left, y: r.bottom };
                    let _ = ClientToScreen(hwnd, &mut pt);
                    let labels: Vec<String> = fm_core::folder_ini::FOLDER_TYPES
                        .iter()
                        .map(|(n, _, _)| (*n).to_string())
                        .collect();
                    open_combo(
                        hwnd,
                        labels,
                        state.type_index,
                        pt.x,
                        pt.y,
                        r.right - r.left,
                        state.scale,
                        state.theme,
                    );
                }
                Hit::ApplySub => {
                    state.apply_sub = !state.apply_sub;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                Hit::PickImage => {
                    if let Some(p) = pick_file(hwnd, "选择文件夹图片", PIC_FILTER, "") {
                        state.ini.area_image = Some(p);
                        // 选了图片就顺手把模板切成"图片"：资源管理器也是这么联动的
                        if state.type_index == 0 {
                            state.type_index = 2;
                            state.ini.folder_type = Some("Pictures".to_string());
                            state.ini.icon_resource =
                                fm_core::folder_ini::FOLDER_TYPES[2].2.map(|s| s.to_string());
                        }
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                Hit::ClearImage => {
                    state.ini.area_image = None;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                Hit::PickIcon => {
                    let cur = state.ini.icon_file.clone().unwrap_or_default();
                    let idx = state.ini.icon_index.unwrap_or(0);
                    if let Some((p, i)) = pick_icon(hwnd, &cur, idx) {
                        state.ini.icon_file = Some(p.clone());
                        state.ini.icon_index = Some(i);
                        state.custom_icon = extract_icon(&p, i);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                Hit::ClearIcon => {
                    state.ini.icon_file = None;
                    state.ini.icon_index = None;
                    if !state.custom_icon.0.is_null() {
                        let _ = windows::Win32::UI::WindowsAndMessaging::DestroyIcon(state.custom_icon);
                        state.custom_icon = HICON::default();
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                Hit::None => {}
            }
            LRESULT(0)
        }
        WM_STAT_DONE => {
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            let bold = state.bold;
            let app = state.app;
            if !state.custom_icon.0.is_null() {
                let _ = windows::Win32::UI::WindowsAndMessaging::DestroyIcon(state.custom_icon);
            }
            drop(Box::from_raw(state_ptr));
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if !bold.0.is_null() {
                let _ = DeleteObject(bold.into());
            }
            // 属性可能改过（隐藏/只读/自定义图标），让主窗口重新枚举一次
            if !app.is_null() {
                (*app).reload();
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 常规页那四个属性位。
///
/// 顺序与资源管理器一致：只读 / 隐藏 / 存档 / 系统。
const ATTR_FLAGS: [(u32, &str); 4] = [
    (FILE_ATTRIBUTE_READONLY.0, "只读"),
    (FILE_ATTRIBUTE_HIDDEN.0, "隐藏"),
    (FILE_ATTRIBUTE_ARCHIVE.0, "存档"),
    (FILE_ATTRIBUTE_SYSTEM.0, "系统"),
];

fn hit_test(state: &Props, x: i32, y: i32) -> Hit {
    if inside(&state.close_rect(), x, y) {
        return Hit::Close;
    }
    if inside(&state.ok_rect(), x, y) {
        return Hit::Ok;
    }
    if inside(&state.cancel_rect(), x, y) {
        return Hit::Cancel;
    }
    for t in state.visible_tabs() {
        if inside(&state.tab_rect(t), x, y) {
            return Hit::Tab(t);
        }
    }
    match state.tab {
        Tab::General => {
            for i in 0..ATTR_FLAGS.len() {
                if inside(&state.check_rect(i), x, y) {
                    return Hit::Attr(i);
                }
            }
        }
        Tab::Security => {
            if let Some(sec) = &state.sec {
                let n = sec.aces.len().saturating_sub(state.sec_scroll).min(state.ace_visible());
                for i in 0..n {
                    if inside(&state.ace_rect(i), x, y) {
                        return Hit::Ace(i);
                    }
                }
            }
        }
        Tab::Custom => {
            if inside(&state.type_combo_rect(), x, y) {
                return Hit::TypeCombo;
            }
            if inside(&state.apply_sub_rect(), x, y) {
                return Hit::ApplySub;
            }
            if inside(&state.pick_image_rect(), x, y) {
                return Hit::PickImage;
            }
            if inside(&state.clear_image_rect(), x, y) {
                return Hit::ClearImage;
            }
            if inside(&state.pick_icon_rect(), x, y) {
                return Hit::PickIcon;
            }
            if inside(&state.clear_icon_rect(), x, y) {
                return Hit::ClearIcon;
            }
        }
        Tab::Details => {}
    }
    Hit::None
}

fn set_attrs(state: &Props, attrs: u32) -> bool {
    let wide: Vec<u16> = state
        .path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_FLAGS_AND_ATTRIBUTES(attrs)).is_ok()
    }
}

/// 切换一个属性位：**立刻落盘**。
///
/// 资源管理器是点"确定"才写，这里改成点一下就写 —— 反馈更直接，也符合
/// "勾了就要真的作用"的预期。代价是"取消"必须把属性位还原回去（见 WM_LBUTTONDOWN）。
fn toggle_attr(state: &mut Props, index: usize, hwnd: HWND) {
    let (flag, _) = ATTR_FLAGS[index];
    let target = state.attrs ^ flag;
    if set_attrs(state, target) {
        state.attrs = target;
        let _ = unsafe { InvalidateRect(Some(hwnd), None, true) };
    }
}

// ---------------------------------------------------------------- 自定义页动作

/// 把自定义页的编辑结果写到 `desktop.ini`。
///
/// 文件夹要带上 READONLY：Shell 只在文件夹带 system/read-only 时才认
/// `desktop.ini` —— 少了这一步，"改了图标但没变化"。
fn apply_custom(state: &mut Props, hwnd: HWND) {
    let dir = state.path.clone();
    if fm_core::folder_ini::write(&dir, &state.ini).unwrap_or(false) {
        let wide: Vec<u16> = dir
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // 先摘掉只读才能改属性，再戴回去
        let cur = state.attrs;
        let _ = unsafe {
            SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_FLAGS_AND_ATTRIBUTES(cur & !FILE_ATTRIBUTE_READONLY.0))
        };
        let _ = unsafe {
            SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_FLAGS_AND_ATTRIBUTES(cur | FILE_ATTRIBUTE_READONLY.0))
        };
        state.attrs = cur | FILE_ATTRIBUTE_READONLY.0;
        state.orig_attrs = state.attrs;
    } else {
        // 内容被清空了：文件删掉了，只读位也就没必要留着
        let wide: Vec<u16> = dir
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let cur = state.attrs & !FILE_ATTRIBUTE_READONLY.0;
        let _ = unsafe {
            SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_FLAGS_AND_ATTRIBUTES(cur))
        };
        state.attrs = cur;
        state.orig_attrs = cur;
    }
    // 应用到子文件夹
    if state.apply_sub {
        let mut ini = state.ini.clone();
        ini.area_image = None; // 文件夹图片不往下传：每个子文件夹都贴同一张图不是用户想要的
        let mut stack: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|rd| rd.flatten().filter(|e| e.path().is_dir()).map(|e| e.path()).collect())
            .unwrap_or_default();
        let mut depth = 0usize;
        while let Some(d) = stack.pop() {
            depth += 1;
            if depth > 2000 {
                break;
            }
            let _ = fm_core::folder_ini::write(&d, &ini);
            if let Ok(rd) = std::fs::read_dir(&d) {
                for e in rd.flatten() {
                    if e.path().is_dir() {
                        stack.push(e.path());
                    }
                }
            }
        }
    }
    let _ = unsafe { InvalidateRect(Some(hwnd), None, true) };
}

fn extract_icon(file: &str, index: i32) -> HICON {
    // ExtractIconW 的索引是 i32，负值（资源 ID）要按有符号传
    let wide: Vec<u16> = file.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { ExtractIconW(None, PCWSTR(wide.as_ptr()), index as u32) }
}

const PIC_FILTER: &str = "图片\0*.bmp;*.jpg;*.jpeg;*.png;*.gif;*.tif;*.tiff\0所有文件\0*.*\0\0";
const ICON_FILTER: &str = "图标\0*.ico;*.icl\0程序\0*.exe;*.dll\0所有文件\0*.*\0\0";

/// `GetOpenFileNameW` 的一层薄封装：返回选中的完整路径。
pub(crate) fn pick_file(hwnd: HWND, title: &str, filter: &str, def_ext: &str) -> Option<String> {
    let mut buf = vec![0u16; 4096];
    let wide_filter: Vec<u16> = filter.encode_utf16().collect();
    let wide_title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let wide_ext: Vec<u16> = def_ext.encode_utf16().chain(std::iter::once(0)).collect();
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: hwnd,
        lpstrFilter: PCWSTR(wide_filter.as_ptr()),
        lpstrFile: PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        lpstrTitle: PCWSTR(wide_title.as_ptr()),
        Flags: OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_HIDEREADONLY,
        lpstrDefExt: if def_ext.is_empty() { PCWSTR::null() } else { PCWSTR(wide_ext.as_ptr()) },
        ..Default::default()
    };
    let ok = unsafe { GetOpenFileNameW(&mut ofn) };
    if ok.as_bool() {
        let s = unsafe { PWSTR(buf.as_mut_ptr()).to_string() }.unwrap_or_default();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// 系统"更改图标"对话框（`shell32!PickIconDlg`）。
///
/// 它是 shell32 的导出（序号 62），windows-rs 没有绑定，只能自己取地址。
/// 取不到就退回"打开文件"选 .ico —— 能用，只是少了图标浏览。
fn pick_icon(hwnd: HWND, current: &str, index: i32) -> Option<(String, i32)> {
    type PickIconDlg =
        unsafe extern "system" fn(HWND, PWSTR, u32, *mut i32) -> i32;
    unsafe {
        let dll = LoadLibraryW(&HSTRING::from("shell32.dll")).ok()?;
        // GetProcAddress 的第二个参数是 LPCSTR：按名字取，取不到再按序号 62 试。
        let name = b"PickIconDlg\0";
        let proc = GetProcAddress(dll, windows::core::PCSTR(name.as_ptr()));
        let raw = match proc {
            Some(p) => Some(p),
            None => GetProcAddress(dll, windows::core::PCSTR(62usize as *const u8)),
        };
        let f: Option<PickIconDlg> = match raw {
            Some(p) => std::mem::transmute::<windows::Win32::Foundation::FARPROC, Option<PickIconDlg>>(Some(p)),
            None => None,
        };
        let Some(f) = f else {
            return pick_file(hwnd, "选择图标文件", ICON_FILTER, "ico").map(|p| (p, 0));
        };
        let mut buf = vec![0u16; 4096];
        let seed = if current.is_empty() {
            // 给个默认起点：资源管理器也是先落在 shell32.dll 上
            std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string())
                + "\\system32\\shell32.dll"
        } else {
            current.to_string()
        };
        for (i, c) in seed.encode_utf16().enumerate() {
            if i < buf.len() - 1 {
                buf[i] = c;
            }
        }
        let mut idx = index;
        if f(hwnd, PWSTR(buf.as_mut_ptr()), buf.len() as u32, &mut idx) == 0 {
            return None;
        }
        let s = PWSTR(buf.as_mut_ptr()).to_string().unwrap_or_default();
        if s.is_empty() {
            return None;
        }
        Some((s, idx))
    }
}

// ---------------------------------------------------------------- 绘制

unsafe fn paint_props(hdc: HDC, s: &Props) {
    let t = &s.theme;
    let whole = RECT { left: 0, top: 0, right: s.w, bottom: s.h };
    fill(hdc, &whole, t.surface);
    // 自绘标题栏
    fill(hdc, &RECT { left: 0, top: 0, right: s.w, bottom: s.s(TITLE_H) }, t.title_bg);
    let font = crate::paint::ui_font(s.scale);
    let old = SelectObject(hdc, font.into());
    SetBkMode(hdc, TRANSPARENT);
    text(
        hdc,
        "属性",
        RECT { left: s.s(14), top: 0, right: s.w - s.s(44), bottom: s.s(TITLE_H) },
        t.text,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    // 关闭按钮
    if s.hover == Some(Hit::Close) {
        fill(hdc, &s.close_rect(), t.danger);
    }
    let g = crate::paint::glyph_font(s.scale);
    let old_g = SelectObject(hdc, g.into());
    text(hdc, "\u{E8BB}", s.close_rect(), t.text, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    SelectObject(hdc, old_g);

    paint_tabs(hdc, s);

    match s.tab {
        Tab::General => paint_general(hdc, s),
        Tab::Security => paint_security(hdc, s),
        Tab::Custom => paint_custom(hdc, s),
        Tab::Details => paint_details(hdc, s),
    }

    // 底部按钮
    for (i, (r, label)) in [(s.ok_rect(), "确定"), (s.cancel_rect(), "取消")].iter().enumerate() {
        let hovered = s.hover == Some(if i == 0 { Hit::Ok } else { Hit::Cancel });
        fill(hdc, r, if hovered { t.hover_bg } else { t.surface_alt });
        text(hdc, label, *r, t.text, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    }

    SelectObject(hdc, old);
}

unsafe fn paint_tabs(hdc: HDC, s: &Props) {
    let t = &s.theme;
    let top = s.s(TITLE_H);
    let bottom = s.s(CONTENT_TOP);
    // 标签条底边那条横线：选中的标签靠"顶上的高亮条 + 与内容同色"区分
    fill(hdc, &RECT { left: 0, top: bottom - 1, right: s.w, bottom }, t.border);
    for tab in s.visible_tabs() {
        let r = s.tab_rect(tab);
        let on = s.tab == tab;
        fill(hdc, &r, if on { t.surface } else { t.title_bg });
        if s.hover == Some(Hit::Tab(tab)) && !on {
            fill(hdc, &r, t.hover_bg);
        }
        if on {
            // 顶边 2px 强调色：Win11 的标签就是这样
            fill(hdc, &RECT { left: r.left, top, right: r.right, bottom: top + s.s(2) }, t.accent);
        }
        let label = TAB_LABELS.iter().find(|(_, tt)| *tt == tab).map(|(l, _)| *l).unwrap_or("");
        text(
            hdc,
            label,
            RECT { left: r.left, top: top + s.s(2), right: r.right, bottom },
            if on { t.text } else { t.text_dim },
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
    }
}

/// 头部：图标 + 名称 + 分隔线（常规/详细信息两页共用）。
unsafe fn paint_header(hdc: HDC, s: &Props) {
    let t = &s.theme;
    let icon_size = s.s(32);
    let top = s.content_top() + s.s(14);
    let _ = DrawIconEx(hdc, s.s(20), top, s.icon, icon_size, icon_size, 0, None, DI_NORMAL);
    let old_b = SelectObject(hdc, s.bold.into());
    text(
        hdc,
        &s.name,
        RECT { left: s.s(64), top, right: s.w - s.s(20), bottom: top + icon_size },
        t.text,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
    );
    SelectObject(hdc, old_b);
    fill(
        hdc,
        &RECT { left: s.s(20), top: s.content_top() + s.s(66), right: s.w - s.s(20), bottom: s.content_top() + s.s(66) + 1 },
        t.border,
    );
}

unsafe fn paint_rows(hdc: HDC, s: &Props, rows: &[(&str, String)]) {
    let t = &s.theme;
    for (i, (k, v)) in rows.iter().enumerate() {
        let r = s.row_rect(i);
        text(
            hdc,
            k,
            RECT { left: r.left, top: r.top, right: r.left + s.s(LABEL_W), bottom: r.bottom },
            t.text_dim,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
        text(
            hdc,
            v,
            RECT { left: r.left + s.s(LABEL_W) + s.s(8), top: r.top, right: r.right, bottom: r.bottom },
            t.text,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
    }
}

unsafe fn paint_general(hdc: HDC, s: &Props) {
    let t = &s.theme;
    paint_header(hdc, s);
    let stat = match s.stat.lock() {
        Ok(g) => *g,
        Err(_) => DirStat::default(),
    };
    let rows: Vec<(&str, String)> = if s.is_dir {
        vec![
            ("类型", s.type_name.clone()),
            ("位置", s.location.clone()),
            (
                "大小",
                if stat.done {
                    format!("{}（{} 字节）", human(stat.bytes), stat.bytes)
                } else {
                    format!("{}（计算中…）", human(stat.bytes))
                },
            ),
            (
                "包含",
                if stat.done {
                    format!("{} 个文件，{} 个文件夹", stat.files, stat.dirs)
                } else {
                    format!("{} 个文件，{} 个文件夹 …", stat.files, stat.dirs)
                },
            ),
            ("创建时间", s.created.clone()),
            ("修改时间", s.modified.clone()),
            ("访问时间", s.accessed.clone()),
        ]
    } else {
        vec![
            ("类型", s.type_name.clone()),
            ("位置", s.location.clone()),
            ("大小", format!("{}（{} 字节）", human(s.size), s.size)),
            ("占用空间", format!("{}（{} 字节）", human(s.alloc), s.alloc)),
            ("创建时间", s.created.clone()),
            ("修改时间", s.modified.clone()),
            ("访问时间", s.accessed.clone()),
        ]
    };
    paint_rows(hdc, s, &rows);

    text(
        hdc,
        "属性",
        RECT { left: s.s(20), top: s.content_top() + s.s(268), right: s.w - s.s(20), bottom: s.content_top() + s.s(282) },
        t.text_dim,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    for (i, (flag, label)) in ATTR_FLAGS.iter().enumerate() {
        let r = s.check_rect(i);
        if s.hover == Some(Hit::Attr(i)) {
            fill(hdc, &r, t.hover_bg);
        }
        let box_s = s.s(16);
        let cy = (r.top + r.bottom) / 2;
        let bx = RECT { left: r.left, top: cy - box_s / 2, right: r.left + box_s, bottom: cy - box_s / 2 + box_s };
        let on = s.attrs & *flag != 0;
        fill(hdc, &bx, if on { t.accent } else { t.surface_alt });
        fill(hdc, &RECT { left: bx.left, top: bx.top, right: bx.right, bottom: bx.top + 1 }, t.border);
        fill(hdc, &RECT { left: bx.left, top: bx.bottom - 1, right: bx.right, bottom: bx.bottom }, t.border);
        fill(hdc, &RECT { left: bx.left, top: bx.top, right: bx.left + 1, bottom: bx.bottom }, t.border);
        fill(hdc, &RECT { left: bx.right - 1, top: bx.top, right: bx.right, bottom: bx.bottom }, t.border);
        if on {
            let old_g2 = SelectObject(hdc, crate::paint::glyph_font(s.scale).into());
            text(hdc, "\u{E73E}", bx, t.selection_text, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            SelectObject(hdc, old_g2);
        }
        text(
            hdc,
            label,
            RECT { left: r.left + box_s + s.s(8), top: r.top, right: r.right, bottom: r.bottom },
            t.text,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    }
}

/// 安全页：属主 / 组 / ACE 列表 / 选中条目的权限。
///
/// 这一页只读 —— 改 ACL 要处理继承与规范顺序，写错就谁都打不开这个文件。
unsafe fn paint_security(hdc: HDC, s: &Props) {
    let t = &s.theme;
    let top = s.content_top();
    let sec = match &s.sec {
        Some(x) => x,
        None => return,
    };
    let mut y = top + s.s(8);
    let line = |y: i32| RECT { left: s.s(20), top: y, right: s.w - s.s(20), bottom: y + s.s(22) };

    if let Some(why) = &sec.error {
        text(hdc, &format!("无法读取安全信息：{why}"), line(y), t.text_dim, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        return;
    }
    text(hdc, &format!("所有者：{}", sec.owner), line(y), t.text_dim, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    y += s.s(24);
    text(hdc, &format!("组：{}", sec.group), line(y), t.text, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    y += s.s(22);
    fill(hdc, &RECT { left: s.s(20), top: y + s.s(4), right: s.w - s.s(20), bottom: y + s.s(5) }, t.border);
    y += s.s(14);
    text(hdc, "组或用户名：", line(y), t.text_dim, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    y += s.s(16);

    // 列表框
    let view = s.ace_view_rect();
    fill(hdc, &view, t.surface_alt);
    let old_g = SelectObject(hdc, crate::paint::glyph_font(s.scale).into());
    let _ = old_g;
    SelectObject(hdc, crate::paint::ui_font(s.scale).into());
    let n = sec.aces.len();
    for i in 0..s.ace_visible() {
        let idx = s.sec_scroll + i;
        let Some(ace) = sec.aces.get(idx) else { break };
        let r = s.ace_rect(i);
        if idx == s.sec_sel {
            fill(hdc, &r, t.selection_bg);
        } else if s.hover == Some(Hit::Ace(i)) {
            fill(hdc, &r, t.hover_bg);
        }
        let label = if ace.account.is_empty() { ace.sid.clone() } else { ace.account.clone() };
        text(
            hdc,
            &label,
            RECT { left: r.left + s.s(6), top: r.top, right: r.right - s.s(90), bottom: r.bottom },
            if idx == s.sec_sel { t.selection_text } else { t.text },
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        let kind = if !ace.allow {
            "拒绝".to_string()
        } else if ace.inherited {
            format!("{}（继承）", ace.right)
        } else {
            ace.right.clone()
        };
        text(
            hdc,
            &kind,
            RECT { left: r.right - s.s(88), top: r.top, right: r.right - s.s(6), bottom: r.bottom },
            t.text_dim,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    }
    if n > s.ace_visible() {
        text(
            hdc,
            &format!("共 {n} 项（滚轮查看其余）",),
            RECT { left: s.s(20), top: view.bottom + s.s(2), right: s.w - s.s(20), bottom: view.bottom + s.s(20) },
            t.text_dim,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    }

    // 选中条目的权限
    let sel = sec.aces.get(s.sec_sel);
    text(
        hdc,
        &match sel {
            Some(a) => format!(
                "{} 的权限{}",
                if a.account.is_empty() { a.sid.clone() } else { a.account.clone() },
                if a.allow { "" } else { "（拒绝）" }
            ),
            None => "权限".to_string(),
        },
        RECT { left: s.s(20), top: s.content_top() + s.s(270), right: s.w - s.s(20), bottom: s.content_top() + s.s(292) },
        t.text_dim,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    let all = ["完全控制", "修改", "读取和执行", "列出文件夹内容", "读取", "写入"];
    for (i, name) in all.iter().enumerate() {
        let r = s.perm_rect(i);
        let on = sel.map(|a| a.right == *name).unwrap_or(false);
        let box_s = s.s(14);
        let cy = (r.top + r.bottom) / 2;
        let bx = RECT { left: r.left, top: cy - box_s / 2, right: r.left + box_s, bottom: cy - box_s / 2 + box_s };
        fill(hdc, &bx, if on { t.accent } else { t.surface_alt });
        fill(hdc, &RECT { left: bx.left, top: bx.top, right: bx.right, bottom: bx.top + 1 }, t.border);
        fill(hdc, &RECT { left: bx.left, top: bx.bottom - 1, right: bx.right, bottom: bx.bottom }, t.border);
        fill(hdc, &RECT { left: bx.left, top: bx.top, right: bx.left + 1, bottom: bx.bottom }, t.border);
        fill(hdc, &RECT { left: bx.right - 1, top: bx.top, right: bx.right, bottom: bx.bottom }, t.border);
        if on {
            let og = SelectObject(hdc, crate::paint::glyph_font(s.scale).into());
            text(hdc, "\u{E73E}", bx, t.selection_text, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            SelectObject(hdc, og);
        }
        text(
            hdc,
            name,
            RECT { left: r.left + box_s + s.s(6), top: r.top, right: r.right, bottom: r.bottom },
            if sel.is_some() { t.text } else { t.text_dim },
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    }
    text(
        hdc,
        &match sel {
            Some(a) if a.right == "特别" => format!("特别权限（掩码 0x{:08X}）", a.mask),
            _ => "本页只读展示；要修改请用系统的「高级安全设置」。".to_string(),
        },
        RECT { left: s.s(20), top: s.content_top() + s.s(374), right: s.w - s.s(20), bottom: s.content_top() + s.s(396) },
        t.text_dim,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
    );
}

/// 自定义页：文件夹模板 / 文件夹图片 / 文件夹图标。
unsafe fn paint_custom(hdc: HDC, s: &Props) {
    let t = &s.theme;
    if !s.is_dir {
        text(
            hdc,
            "只有文件夹可以自定义",
            RECT { left: s.s(20), top: s.content_top() + s.s(20), right: s.w - s.s(20), bottom: s.content_top() + s.s(48) },
            t.text_dim,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
        return;
    }
    let label_rect = |top: i32| RECT {
        left: s.s(20),
        top: s.content_top() + s.s(top),
        right: s.w - s.s(20),
        bottom: s.content_top() + s.s(top) + s.s(22),
    };
    text(hdc, "您想要哪种文件夹类型？", label_rect(6), t.text, DT_LEFT | DT_VCENTER | DT_SINGLELINE);

    // 下拉框
    let cb = s.type_combo_rect();
    fill(hdc, &cb, if s.hover == Some(Hit::TypeCombo) { t.hover_bg } else { t.surface_alt });
    fill(hdc, &RECT { left: cb.left, top: cb.top, right: cb.right, bottom: cb.top + 1 }, t.border);
    fill(hdc, &RECT { left: cb.left, top: cb.bottom - 1, right: cb.right, bottom: cb.bottom }, t.border);
    fill(hdc, &RECT { left: cb.left, top: cb.top, right: cb.left + 1, bottom: cb.bottom }, t.border);
    fill(hdc, &RECT { left: cb.right - 1, top: cb.top, right: cb.right, bottom: cb.bottom }, t.border);
    let name = fm_core::folder_ini::FOLDER_TYPES
        .get(s.type_index)
        .map(|(n, _, _)| *n)
        .unwrap_or("常规项");
    text(
        hdc,
        name,
        RECT { left: cb.left + s.s(8), top: cb.top, right: cb.right - s.s(24), bottom: cb.bottom },
        t.text,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    let og = SelectObject(hdc, crate::paint::glyph_font(s.scale).into());
    text(
        hdc,
        "\u{E70E}",
        RECT { left: cb.right - s.s(24), top: cb.top, right: cb.right, bottom: cb.bottom },
        t.text_dim,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
    SelectObject(hdc, og);

    // "应用到子文件夹"复选框
    let ar = s.apply_sub_rect();
    if s.hover == Some(Hit::ApplySub) {
        fill(hdc, &ar, t.hover_bg);
    }
    let box_s = s.s(16);
    let cy = (ar.top + ar.bottom) / 2;
    let bx = RECT { left: ar.left, top: cy - box_s / 2, right: ar.left + box_s, bottom: cy - box_s / 2 + box_s };
    fill(hdc, &bx, if s.apply_sub { t.accent } else { t.surface_alt });
    for e in [
        RECT { left: bx.left, top: bx.top, right: bx.right, bottom: bx.top + 1 },
        RECT { left: bx.left, top: bx.bottom - 1, right: bx.right, bottom: bx.bottom },
        RECT { left: bx.left, top: bx.top, right: bx.left + 1, bottom: bx.bottom },
        RECT { left: bx.right - 1, top: bx.top, right: bx.right, bottom: bx.bottom },
    ] {
        fill(hdc, &e, t.border);
    }
    if s.apply_sub {
        let og = SelectObject(hdc, crate::paint::glyph_font(s.scale).into());
        text(hdc, "\u{E73E}", bx, t.selection_text, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        SelectObject(hdc, og);
    }
    text(
        hdc,
        "把此模板应用到所有子文件夹",
        RECT { left: ar.left + box_s + s.s(8), top: ar.top, right: ar.right, bottom: ar.bottom },
        t.text,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    // 文件夹图片
    text(hdc, "文件夹图片", label_rect(112), t.text_dim, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    let ib = s.image_box_rect();
    fill(hdc, &ib, t.surface_alt);
    for e in [
        RECT { left: ib.left, top: ib.top, right: ib.right, bottom: ib.top + 1 },
        RECT { left: ib.left, top: ib.bottom - 1, right: ib.right, bottom: ib.bottom },
        RECT { left: ib.left, top: ib.top, right: ib.left + 1, bottom: ib.bottom },
        RECT { left: ib.right - 1, top: ib.top, right: ib.right, bottom: ib.bottom },
    ] {
        fill(hdc, &e, t.border);
    }
    text(
        hdc,
        s.ini.area_image.as_deref().unwrap_or("（未设置）"),
        s.image_path_rect(),
        if s.ini.area_image.is_some() { t.text } else { t.text_dim },
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
    );
    button(hdc, s, s.pick_image_rect(), "选择文件…", Hit::PickImage);
    button(hdc, s, s.clear_image_rect(), "还原默认", Hit::ClearImage);

    // 文件夹图标
    text(hdc, "文件夹图标", label_rect(224), t.text_dim, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    let ir = s.custom_icon_rect();
    let icon = if !s.custom_icon.0.is_null() {
        s.custom_icon
    } else {
        s.icon
    };
    let _ = DrawIconEx(hdc, ir.left, ir.top, icon, s.s(32), s.s(32), 0, None, DI_NORMAL);
    let desc = match (&s.ini.icon_file, s.ini.icon_index) {
        (Some(f), Some(i)) => format!("{f},{i}"),
        (Some(f), None) => f.clone(),
        _ => s.ini
            .icon_resource
            .clone()
            .unwrap_or_else(|| "（系统默认图标）".to_string()),
    };
    text(
        hdc,
        &desc,
        s.custom_icon_path_rect(),
        if s.ini.icon_file.is_some() { t.text } else { t.text_dim },
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
    );
    button(hdc, s, s.pick_icon_rect(), "更改图标…", Hit::PickIcon);
    button(hdc, s, s.clear_icon_rect(), "还原默认", Hit::ClearIcon);
}

unsafe fn button(hdc: HDC, s: &Props, r: RECT, label: &str, hit: Hit) {
    let t = &s.theme;
    fill(hdc, &r, if s.hover == Some(hit) { t.hover_bg } else { t.surface_alt });
    for e in [
        RECT { left: r.left, top: r.top, right: r.right, bottom: r.top + 1 },
        RECT { left: r.left, top: r.bottom - 1, right: r.right, bottom: r.bottom },
        RECT { left: r.left, top: r.top, right: r.left + 1, bottom: r.bottom },
        RECT { left: r.right - 1, top: r.top, right: r.right, bottom: r.bottom },
    ] {
        fill(hdc, &e, t.border);
    }
    text(hdc, label, r, t.text, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
}

/// 详细信息页：把能问到的都列出来，不重复常规页的"属性复选框"。
unsafe fn paint_details(hdc: HDC, s: &Props) {
    paint_header(hdc, s);
    let stat = match s.stat.lock() {
        Ok(g) => *g,
        Err(_) => DirStat::default(),
    };
    let attrs_text = {
        let mut names: Vec<&str> = Vec::new();
        for (f, n) in ATTR_FLAGS {
            if s.attrs & f != 0 {
                names.push(n);
            }
        }
        if names.is_empty() {
            format!("0x{:08X}（无）", s.attrs)
        } else {
            format!("0x{:08X}（{}）", s.attrs, names.join("、"))
        }
    };
    let owner = s
        .sec
        .as_ref()
        .map(|x| x.owner.clone())
        .unwrap_or_else(|| "（切到「安全」页读取）".to_string());
    let mut rows: Vec<(&str, String)> = vec![
        ("名称", s.name.clone()),
        ("类型", s.type_name.clone()),
        ("位置", s.location.clone()),
        ("属性", attrs_text),
        ("所有者", owner),
        ("创建时间", s.created.clone()),
        ("修改时间", s.modified.clone()),
        ("访问时间", s.accessed.clone()),
    ];
    if s.is_dir {
        rows.push(("包含", format!("{} 个文件，{} 个文件夹", stat.files, stat.dirs)));
    } else {
        rows.push(("大小", format!("{} 字节", s.size)));
        rows.push(("占用空间", format!("{} 字节", s.alloc)));
    }
    // 详情页的行从分隔线下面第一行开始，比常规页高一点，别压到图标
    let t = &s.theme;
    for (i, (k, v)) in rows.iter().enumerate() {
        let top = s.content_top() + s.s(74) + i as i32 * s.s(ROW_H);
        let r = RECT { left: s.s(20), top, right: s.w - s.s(20), bottom: top + s.s(ROW_H) };
        text(
            hdc,
            k,
            RECT { left: r.left, top: r.top, right: r.left + s.s(LABEL_W), bottom: r.bottom },
            t.text_dim,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
        text(
            hdc,
            v,
            RECT { left: r.left + s.s(LABEL_W) + s.s(8), top: r.top, right: r.right, bottom: r.bottom },
            t.text,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
    }
}

// ---------------------------------------------------------------- 自绘下拉框

const WM_COMBO_SELF_PICK: u32 = 0x8003;

struct Combo {
    owner: HWND,
    items: Vec<String>,
    current: usize,
    hover: Option<usize>,
    scale: f32,
    theme: Theme,
}

static COMBO_CLASS: std::sync::Once = std::sync::Once::new();

fn register_combo_class() {
    COMBO_CLASS.call_once(|| {
        let cls = HSTRING::from("NexFilePropsCombo");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(combo_proc),
            lpszClassName: PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        unsafe {
            RegisterClassExW(&wc);
        }
    });
}

fn open_combo(
    owner: HWND,
    items: Vec<String>,
    current: usize,
    x: i32,
    y: i32,
    w: i32,
    scale: f32,
    theme: Theme,
) {
    let row_h = (24.0 * scale).round() as i32;
    let h = row_h * items.len() as i32 + (8.0 * scale).round() as i32;
    let state = Box::into_raw(Box::new(Combo {
        owner,
        items,
        current,
        hover: None,
        scale,
        theme,
    }));
    let cls = HSTRING::from("NexFilePropsCombo");
    let hwnd = unsafe {
        CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WS_EX_TOPMOST,
            PCWSTR(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            x,
            y,
            w,
            h,
            Some(owner),
            None,
            None,
            Some(state as *const core::ffi::c_void),
        )
    };
    match hwnd {
        Ok(hw) => unsafe {
            let _ = SetWindowLongPtrW(hw, GWLP_USERDATA, state as isize);
            let _ = ShowWindow(hw, SW_SHOW);
            // 抓住鼠标：点框外也能收到消息，好把自己关掉
            let _ = SetCapture(hw);
            let _ = InvalidateRect(Some(hw), None, true);
        },
        Err(_) => {
            drop(unsafe { Box::from_raw(state) });
        }
    }
}

unsafe extern "system" fn combo_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Combo;
    if p.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let st = &mut *p;
    match msg {
        WM_ERASEBKGND => return LRESULT(1),
        WM_NCHITTEST => return LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut r = RECT::default();
            let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut r);
            let mem = CreateCompatibleDC(Some(hdc));
            let bmp = CreateCompatibleBitmap(hdc, r.right, r.bottom);
            let old = SelectObject(mem, bmp.into());
            let t = &st.theme;
            fill(mem, &r, t.surface_alt);
            fill(mem, &RECT { left: 0, top: 0, right: r.right, bottom: 1 }, t.border);
            fill(mem, &RECT { left: 0, top: r.bottom - 1, right: r.right, bottom: r.bottom }, t.border);
            fill(mem, &RECT { left: 0, top: 0, right: 1, bottom: r.bottom }, t.border);
            fill(mem, &RECT { left: r.right - 1, top: 0, right: r.right, bottom: r.bottom }, t.border);
            let font = crate::paint::ui_font(st.scale);
            let of = SelectObject(mem, font.into());
            SetBkMode(mem, TRANSPARENT);
            let row_h = (24.0 * st.scale).round() as i32;
            let pad = (4.0 * st.scale).round() as i32;
            for (i, item) in st.items.clone().iter().enumerate() {
                let top = pad + i as i32 * row_h;
                let rr = RECT { left: 1, top, right: r.right - 1, bottom: top + row_h };
                if st.hover == Some(i) {
                    fill(mem, &rr, t.hover_bg);
                }
                text(
                    mem,
                    item,
                    RECT { left: rr.left + (10.0 * st.scale).round() as i32, top: rr.top, right: rr.right, bottom: rr.bottom },
                    t.text,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE,
                );
                if i == st.current {
                    let og = SelectObject(mem, crate::paint::glyph_font(st.scale).into());
                    text(
                        mem,
                        "\u{E73E}",
                        RECT { left: rr.right - (24.0 * st.scale).round() as i32, top: rr.top, right: rr.right, bottom: rr.bottom },
                        t.accent,
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                    );
                    SelectObject(mem, og);
                }
            }
            SelectObject(mem, of);
            let _ = BitBlt(hdc, 0, 0, r.right, r.bottom, Some(mem), 0, 0, SRCCOPY);
            SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
            let _ = EndPaint(hwnd, &mut ps);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let y = (lparam.0 >> 16) as i16 as i32;
            let row_h = (24.0 * st.scale).round() as i32;
            let pad = (4.0 * st.scale).round() as i32;
            let i = if y < pad || y >= pad + row_h * st.items.len() as i32 {
                None
            } else {
                Some(((y - pad) / row_h) as usize)
            };
            if i != st.hover {
                st.hover = i;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let y = (lparam.0 >> 16) as i16 as i32;
            let row_h = (24.0 * st.scale).round() as i32;
            let pad = (4.0 * st.scale).round() as i32;
            let i = if y < pad || y >= pad + row_h * st.items.len() as i32 {
                None
            } else {
                Some(((y - pad) / row_h) as usize)
            };
            if let Some(i) = i {
                let _ = PostMessageW(Some(st.owner), WM_COMBO_PICK, WPARAM(i), LPARAM(0));
            }
            let _ = ReleaseCapture();
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            drop(Box::from_raw(p));
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 量一段文字的宽度（下拉框宽度要跟着最长的那一项走）。
#[allow(dead_code)]
fn text_width(hdc: HDC, s: &str) -> i32 {
    let mut size = windows::Win32::Foundation::SIZE::default();
    let v: Vec<u16> = s.encode_utf16().collect();
    unsafe {
        if GetTextExtentPoint32W(hdc, &v, &mut size).as_bool() {
            size.cx
        } else {
            0
        }
    }
}
