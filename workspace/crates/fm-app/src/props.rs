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

use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::core::HSTRING;
use windows::Win32::Foundation::{
    COLORREF, FILETIME, HWND, LPARAM, LRESULT, RECT, SYSTEMTIME, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC,
    DeleteObject, DrawTextW, EndPaint, FillRect, SelectObject, SetBkMode, SetTextColor,
    DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DRAW_TEXT_FORMAT,
    HDC, HFONT, InvalidateRect, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::Storage::FileSystem::{
    FileTimeToLocalFileTime, GetDiskFreeSpaceW, SetFileAttributesW, FILE_ATTRIBUTE_ARCHIVE,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SYSTEM,
    FILE_FLAGS_AND_ATTRIBUTES,
};
use windows::Win32::System::Time::FileTimeToSystemTime;
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHFILEINFOW, SHGFI_TYPENAME,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DrawIconEx, GetWindowLongPtrW,
    PostMessageW, RegisterClassExW, SetWindowLongPtrW, ShowWindow, GWLP_USERDATA, HICON, SW_SHOW,
    DI_NORMAL, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_NCHITTEST,
    WM_PAINT, WNDCLASSEXW, WS_POPUP,
};

use crate::app::App;
use crate::theme::Theme;

/// 后台统计线程算完之后通知窗口重画
const WM_STAT_DONE: u32 = 0x8001;

const TITLE_H: i32 = 34;
const BASE_W: i32 = 440;
const BASE_H: i32 = 480;
/// 信息行的行高（基准 DPI）
const ROW_H: i32 = 26;
const ROW_TOP: i32 = 104;
const CHECK_TOP: i32 = 336;
const CHECK_H: i32 = 26;
const LABEL_W: i32 = 78;

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
    is_dir: bool,
    icon: HICON,
    stat: Arc<Mutex<DirStat>>,
    /// 悬停项：0..4 = 四个属性复选框，10 = 确定，11 = 取消，12 = 关闭
    hover: Option<usize>,
    scale: f32,
    theme: Theme,
    bold: HFONT,
    w: i32,
    h: i32,
}

impl Props {
    fn s(&self, v: i32) -> i32 {
        (v as f32 * self.scale).round() as i32
    }
    /// 信息行矩形（绘制与命中测试共用，避免两边各写一份常量而错位）
    fn row_rect(&self, i: usize) -> RECT {
        let top = self.s(ROW_TOP) + i as i32 * self.s(ROW_H);
        RECT { left: self.s(20), top, right: self.w - self.s(20), bottom: top + self.s(ROW_H) }
    }
    fn check_rect(&self, i: usize) -> RECT {
        let top = self.s(CHECK_TOP) + i as i32 * self.s(CHECK_H);
        RECT { left: self.s(20), top, right: self.w - self.s(20), bottom: top + self.s(CHECK_H) }
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
}

pub fn register_props_class() {
    let cls = HSTRING::from("NexFileProps");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(props_proc),
        lpszClassName: windows::core::PCWSTR(cls.as_ptr()),
        ..Default::default()
    };
    unsafe {
        RegisterClassExW(&wc);
    }
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
        is_dir,
        icon,
        stat: stat.clone(),
        hover: None,
        scale,
        theme,
        bold,
        w,
        h,
    }));

    let cls = HSTRING::from("NexFileProps");
    let hwnd = unsafe {
        CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WS_EX_WINDOWEDGE,
            windows::core::PCWSTR(cls.as_ptr()),
            windows::core::PCWSTR(HSTRING::from("属性").as_ptr()),
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
        let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
            hwnd,
            None,
            cx,
            cy,
            0,
            0,
            windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE
                | windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER,
        );
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
    let root = path
        .to_string_lossy()
        .chars()
        .take(3)
        .collect::<String>();
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
    use windows::Win32::Storage::FileSystem::{
        FindClose, FindFirstFileW, WIN32_FIND_DATAW,
    };
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
        let to_ft = |t: Option<SystemTime>| -> i64 {
            t.and_then(to_filetime).map(ft_i64).unwrap_or(0)
        };
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
    let ft = FILETIME {
        dwLowDateTime: ft as u32,
        dwHighDateTime: (ft >> 32) as u32,
    };
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
            let mut sp = windows::Win32::Foundation::POINT { x, y };
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
            if h != state.hover {
                state.hover = h;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = (lparam.0 >> 16) as i16 as i32;
            match hit_test(state, x, y) {
                Some(12) => {
                    let _ = DestroyWindow(hwnd);
                }
                Some(11) => {
                    let _ = DestroyWindow(hwnd);
                }
                Some(10) => {
                    let _ = DestroyWindow(hwnd);
                }
                Some(i) if i < 4 => toggle_attr(state, i, hwnd),
                _ => {}
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
            drop(Box::from_raw(state_ptr));
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if !bold.0.is_null() {
                let _ = DeleteObject(bold.into());
            }
            // 属性可能改过（隐藏/只读），让主窗口重新枚举一次
            if !app.is_null() {
                (*app).reload();
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

const ATTR_FLAGS: [(u32, &str); 4] = [
    (FILE_ATTRIBUTE_READONLY.0, "只读"),
    (FILE_ATTRIBUTE_HIDDEN.0, "隐藏"),
    (FILE_ATTRIBUTE_ARCHIVE.0, "存档"),
    (FILE_ATTRIBUTE_SYSTEM.0, "系统"),
];

fn hit_test(state: &Props, x: i32, y: i32) -> Option<usize> {
    let inside = |r: &RECT| x >= r.left && x <= r.right && y >= r.top && y <= r.bottom;
    if inside(&state.close_rect()) {
        return Some(12);
    }
    if inside(&state.ok_rect()) {
        return Some(10);
    }
    if inside(&state.cancel_rect()) {
        return Some(11);
    }
    for i in 0..ATTR_FLAGS.len() {
        if inside(&state.check_rect(i)) {
            return Some(i);
        }
    }
    None
}

/// 切换一个属性位。系统属性（受保护的操作系统文件）不允许在这里改 ——
/// 清掉它会让 Shell 立刻"丢失"该文件，属于高风险操作，资源管理器也要二次确认。
fn toggle_attr(state: &mut Props, index: usize, hwnd: HWND) {
    let (flag, _) = ATTR_FLAGS[index];
    if flag == FILE_ATTRIBUTE_SYSTEM.0 {
        return;
    }
    let target = state.attrs ^ flag;
    let wide: Vec<u16> = state
        .path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let ok = unsafe {
        SetFileAttributesW(
            windows::core::PCWSTR(wide.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(target),
        )
    };
    if ok.is_ok() {
        state.attrs = target;
        let _ = unsafe { InvalidateRect(Some(hwnd), None, true) };
    }
}

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
    if s.hover == Some(12) {
        fill(hdc, &s.close_rect(), t.danger);
    }
    let g = crate::paint::glyph_font(s.scale);
    let old_g = SelectObject(hdc, g.into());
    text(hdc, "\u{E8BB}", s.close_rect(), t.text, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    SelectObject(hdc, old_g);

    // 头部：图标 + 名称
    let icon_size = s.s(32);
    let _ = DrawIconEx(hdc, s.s(20), s.s(48), s.icon, icon_size, icon_size, 0, None, DI_NORMAL);
    let old_b = SelectObject(hdc, s.bold.into());
    text(
        hdc,
        &s.name,
        RECT { left: s.s(64), top: s.s(48), right: s.w - s.s(20), bottom: s.s(48) + icon_size },
        t.text,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
    );
    SelectObject(hdc, old_b);

    // 分隔线
    fill(
        hdc,
        &RECT { left: s.s(20), top: s.s(96), right: s.w - s.s(20), bottom: s.s(96) + 1 },
        t.border,
    );

    // 信息行
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

    // 属性复选框
    text(
        hdc,
        "属性",
        RECT { left: s.s(20), top: s.s(316), right: s.w - s.s(20), bottom: s.s(CHECK_TOP) },
        t.text_dim,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    for (i, (flag, label)) in ATTR_FLAGS.iter().enumerate() {
        let r = s.check_rect(i);
        if s.hover == Some(i) && *flag != FILE_ATTRIBUTE_SYSTEM.0 {
            fill(hdc, &r, t.hover_bg);
        }
        let box_s = s.s(16);
        let cy = (r.top + r.bottom) / 2;
        let bx = RECT { left: r.left, top: cy - box_s / 2, right: r.left + box_s, bottom: cy - box_s / 2 + box_s };
        let on = s.attrs & *flag != 0;
        fill(hdc, &bx, if on { t.accent } else { t.surface_alt });
        fill(
            hdc,
            &RECT { left: bx.left, top: bx.top, right: bx.right, bottom: bx.top + 1 },
            t.border,
        );
        fill(
            hdc,
            &RECT { left: bx.left, top: bx.bottom - 1, right: bx.right, bottom: bx.bottom },
            t.border,
        );
        fill(
            hdc,
            &RECT { left: bx.left, top: bx.top, right: bx.left + 1, bottom: bx.bottom },
            t.border,
        );
        fill(
            hdc,
            &RECT { left: bx.right - 1, top: bx.top, right: bx.right, bottom: bx.bottom },
            t.border,
        );
        if on {
            let old_g2 = SelectObject(hdc, crate::paint::glyph_font(s.scale).into());
            text(hdc, "\u{E73E}", bx, t.selection_text, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            SelectObject(hdc, old_g2);
        }
        text(
            hdc,
            label,
            RECT { left: r.left + box_s + s.s(8), top: r.top, right: r.right, bottom: r.bottom },
            if *flag == FILE_ATTRIBUTE_SYSTEM.0 { t.text_dim } else { t.text },
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    }

    // 底部按钮
    for (i, (r, label)) in [(s.ok_rect(), "确定"), (s.cancel_rect(), "取消")].iter().enumerate() {
        let hovered = s.hover == Some(10 + i);
        fill(hdc, r, if hovered { t.hover_bg } else { t.surface_alt });
        text(
            hdc,
            *label,
            *r,
            t.text,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
    }

    SelectObject(hdc, old);
}
