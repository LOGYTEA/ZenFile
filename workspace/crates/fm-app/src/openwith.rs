//! 右键「打开方式」：列出能打开这个文件的程序，选一个就用它打开。
//!
//! ## 候选程序从哪来
//!
//! 按 Windows 自己的顺序找，越靠前越可能是用户想要的：
//!   1. `HKCU\...\FileExts\<ext>\UserChoice` —— 用户选过的（最高优先级）
//!   2. `HKCU\...\FileExts\<ext>\OpenWithList` —— "打开方式"里出现过的
//!   3. `HKCR\<ext>` 的默认 ProgID 及其 `shell\open\command`
//!   4. `HKCR\SystemFileAssociations\<ext>` 下的 open 命令
//!   5. `App Paths` 里装着的常见程序（记事本、画图、写字板…）
//!
//! 名字取三处之一：exe 的版本资源 FileDescription、`App Paths` 的默认值、
//! 再不济就是文件名。图标一律 `SHGetFileInfoW` 取真实图标（不走
//! `SHGFI_USEFILEATTRIBUTES`，否则拿到的永远是同一个白板图标）。
//!
//! ## 为什么自绘而不用系统那个
//!
//! 系统的"打开方式"是一个 UWP/COM 混合的对话框，弹出常常要 1 秒以上，还会
//! 拉起一堆 Shell 扩展。这里只做几次注册表读 + 几次图标查询，几十毫秒。

use std::path::{Path, PathBuf};

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC,
    DeleteObject, DrawTextW, EndPaint, FillRect, SelectObject, SetBkMode, SetTextColor, DT_CENTER,
    DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DRAW_TEXT_FORMAT, HDC, HFONT,
    InvalidateRect, IntersectClipRect, RestoreDC, SaveDC, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_SZ,
    RRF_RT_REG_SZ,
};
use windows::Win32::UI::Shell::{
    SHChangeNotify, SHGetFileInfoW, SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHFILEINFOW,
    SHGFI_ICON, SHGFI_SMALLICON,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DrawIconEx, GetWindowLongPtrW, RegisterClassExW,
    SetWindowLongPtrW, ShowWindow, GWLP_USERDATA, HICON, SW_SHOW, DI_NORMAL, WM_CLOSE, WM_DESTROY,
    WM_ERASEBKGND, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCHITTEST, WM_PAINT, WNDCLASSEXW,
    WS_POPUP,
};

use crate::app::App;
use crate::theme::Theme;

const TITLE_H: i32 = 34;
const BASE_W: i32 = 470;
const BASE_H: i32 = 430;
const ROW_H: i32 = 46;

/// 一条候选程序。
pub struct AppCandidate {
    pub name: String,
    pub exe: PathBuf,
    /// 已经填好 `%1` 位置的命令行模板里我们只用 exe 路径，参数自己拼
    pub icon: HICON,
    /// 这条是"当前默认"（UserChoice / 关联 ProgID）时为 true
    pub is_default: bool,
}

struct OpenWith {
    app: *mut App,
    path: PathBuf,
    name: String,
    cands: Vec<AppCandidate>,
    sel: usize,
    hover: Hit,
    always: bool,
    /// File = 对文件；Dir = 对文件夹 / 目录背景 / 驱动器
    kind: TargetKind,
    /// 已经做过一次全量"查找可用的打开软件"
    found: bool,
    /// 列表滚动到了第几条
    scroll: i32,
    scale: f32,
    theme: Theme,
    bold: HFONT,
    w: i32,
    h: i32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    None,
    Close,
    Row(usize),
    Always,
    Browse,
    /// 「查找可用的打开软件」：把本机装着的程序全部补进列表
    Find,
    Ok,
    Cancel,
}

impl OpenWith {
    fn s(&self, v: i32) -> i32 {
        (v as f32 * self.scale).round() as i32
    }
    fn row_rect(&self, i: usize) -> RECT {
        let top = self.s(TITLE_H + 44) + (i as i32 - self.scroll) * self.s(ROW_H);
        RECT { left: self.s(12), top, right: self.w - self.s(12), bottom: top + self.s(ROW_H) - self.s(2) }
    }
    fn list_rect(&self) -> RECT {
        RECT {
            left: self.s(12),
            top: self.s(TITLE_H + 44),
            right: self.w - self.s(12),
            bottom: self.h - self.s(104),
        }
    }
    fn always_rect(&self) -> RECT {
        RECT { left: self.s(14), top: self.h - self.s(96), right: self.w - self.s(14), bottom: self.h - self.s(70) }
    }
    fn browse_rect(&self) -> RECT {
        let top = self.h - self.s(62);
        RECT { left: self.s(14), top, right: self.s(14) + self.s(120), bottom: top + self.s(28) }
    }
    fn find_rect(&self) -> RECT {
        let top = self.h - self.s(62);
        RECT {
            left: self.s(14) + self.s(128),
            top,
            right: self.s(14) + self.s(128) + self.s(186),
            bottom: top + self.s(28),
        }
    }
    fn ok_rect(&self) -> RECT {
        let top = self.h - self.s(44);
        let right = self.w - self.s(14) - self.s(92) - self.s(8);
        RECT { left: right - self.s(92), top, right, bottom: top + self.s(28) }
    }
    fn cancel_rect(&self) -> RECT {
        let top = self.h - self.s(44);
        RECT { left: self.w - self.s(14) - self.s(92), top, right: self.w - self.s(14), bottom: top + self.s(28) }
    }
    fn close_rect(&self) -> RECT {
        RECT { left: self.w - self.s(44), top: 0, right: self.w, bottom: self.s(TITLE_H) }
    }
}

pub fn register_class() {
    let cls = HSTRING::from("ZenFileOpenWith");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(ow_proc),
        lpszClassName: PCWSTR(cls.as_ptr()),
        ..Default::default()
    };
    unsafe {
        RegisterClassExW(&wc);
    }
}

/// 打开「打开方式」对话框（文件）。
pub fn open(app_ptr: *mut App, path: PathBuf) {
    open_for(app_ptr, path, TargetKind::File)
}

/// 打开「打开方式」对话框（文件夹 / 目录背景 / 驱动器）。
pub fn open_dir(app_ptr: *mut App, path: PathBuf) {
    open_for(app_ptr, path, TargetKind::Dir)
}

pub fn open_for(app_ptr: *mut App, path: PathBuf, kind: TargetKind) {
    if app_ptr.is_null() {
        return;
    }
    let app = unsafe { &mut *app_ptr };
    let scale = app.metrics.scale;
    let theme = app.theme;
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    let cands = match kind {
        TargetKind::Dir => {
            let v = candidates_dir();
            if v.is_empty() {
                // 一个候选都没有：退化成"用资源管理器打开这里"，别弹个空框
                let _ = open_with_system(&path);
                return;
            }
            v
        }
        TargetKind::File => {
            let v = candidates(&ext);
            if v.is_empty() {
                // 一个候选都没有：直接给系统的"打开方式"，别弹个空框
                let _ = open_with_system(&path);
                return;
            }
            v
        }
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());
    let w = (BASE_W as f32 * scale).round() as i32;
    let h = (BASE_H as f32 * scale).round() as i32;
    let bold = unsafe { crate::paint::make_font((-15.0 * scale) as i32, 700, "Segoe UI") };

    let state = Box::into_raw(Box::new(OpenWith {
        app: app_ptr,
        path: path.clone(),
        name,
        cands,
        sel: 0,
        hover: Hit::None,
        always: false,
        kind,
        found: false,
        scroll: 0,
        scale,
        theme,
        bold,
        w,
        h,
    }));
    let cls = HSTRING::from("ZenFileOpenWith");
    let hwnd = unsafe {
        CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WS_EX_WINDOWEDGE,
            PCWSTR(cls.as_ptr()),
            PCWSTR(HSTRING::from("打开方式").as_ptr()),
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
}

// ---------------------------------------------------------------- 候选发现

pub(crate) fn reg_sz(root: HKEY, sub: &str, value: &str) -> Option<String> {
    let sub = HSTRING::from(sub);
    let val = HSTRING::from(value);
    let mut buf = vec![0u8; 4096];
    let mut len = buf.len() as u32;
    let rc = unsafe {
        RegGetValueW(
            root,
            &sub,
            &val,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            Some(&mut len),
        )
    };
    if rc.0 != 0 || len == 0 {
        return None;
    }
    // RegGetValueW 写出的是 UTF-16，含结尾 NUL
    let n = (len as usize).min(buf.len()) / 2;
    let u: Vec<u16> = (0..n).map(|i| u16::from_ne_bytes([buf[i * 2], buf[i * 2 + 1]])).collect();
    let s = String::from_utf16_lossy(&u);
    let s = s.trim_end_matches('\0').trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// 从 `"C:\a\b.exe" "%1"` 里抠出 exe 路径。
pub(crate) fn exe_from_command(cmd: &str) -> Option<PathBuf> {
    let cmd = cmd.trim();
    let (head, _) = if let Some(rest) = cmd.strip_prefix('"') {
        match rest.find('"') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        }
    } else {
        match cmd.find(' ') {
            Some(i) => (&cmd[..i], &cmd[i..]),
            None => (cmd, ""),
        }
    };
    let head = head.trim_matches('"').trim();
    if head.is_empty() {
        return None;
    }
    // rundll32 / UWP 这类壳不是 exe，列出来也没法"用它打开"
    if head.to_lowercase().ends_with(".dll") {
        return None;
    }
    let expanded = expand_env(head);
    let p = PathBuf::from(&expanded);
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

pub(crate) fn expand_env(s: &str) -> String {
    // 只处理 %SystemRoot% 之类最常见的几个：完整 ExpandEnvironmentStrings
    // 也能做，但这条路径上出现的多半就是它们。
    let mut out = s.to_string();
    for var in ["SystemRoot", "windir", "ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        let token = format!("%{var}%");
        if out.contains(&token) {
            if let Ok(v) = std::env::var(var) {
                out = out.replace(&token, &v);
            }
        }
    }
    out
}

pub(crate) fn icon_of(exe: &Path) -> HICON {
    let mut sfi = SHFILEINFOW::default();
    let res = unsafe {
        SHGetFileInfoW(
            &HSTRING::from(exe.to_string_lossy().as_ref()),
            windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut sfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_SMALLICON,
        )
    };
    if res != 0 && !sfi.hIcon.0.is_null() {
        return sfi.hIcon;
    }
    HICON::default()
}

/// 显示名：优先 exe 里的 FileDescription，其次 App Paths 的默认值，最后文件名。
pub(crate) fn friendly_name(exe: &Path) -> String {
    if let Some(desc) = file_description(exe) {
        if !desc.trim().is_empty() {
            return desc.trim().to_string();
        }
    }
    let fname = exe
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if let Some(v) = reg_sz(
        HKEY_LOCAL_MACHINE,
        &format!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{fname}"),
        "",
    ) {
        if !v.trim().is_empty() {
            return v.trim().to_string();
        }
    }
    fname.trim_end_matches(".exe").to_string()
}

/// 读 exe 版本资源里的 FileDescription。
fn file_description(exe: &Path) -> Option<String> {
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    };
    let path = HSTRING::from(exe.to_string_lossy().as_ref());
    let size = unsafe { GetFileVersionInfoSizeW(&path, None) };
    if size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    if unsafe { GetFileVersionInfoW(&path, Some(0), size, buf.as_mut_ptr() as *mut core::ffi::c_void) }.is_err() {
        return None;
    }
    // 语言/代码页表在 `\VarFileInfo\Translation`
    let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut len: u32 = 0;
    let trans = HSTRING::from("\\VarFileInfo\\Translation");
    let ok = unsafe { VerQueryValueW(buf.as_ptr() as *const core::ffi::c_void, &trans, &mut ptr, &mut len) };
    if ok.as_bool() && !ptr.is_null() && len >= 4 {
        let lang = unsafe { *(ptr as *const u16) };
        let cp = unsafe { *((ptr as *const u16).add(1)) };
        let sub = format!("\\StringFileInfo\\{lang:04X}{cp:04X}\\FileDescription");
        let sub = HSTRING::from(&sub);
        let mut p2: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut l2: u32 = 0;
        if unsafe { VerQueryValueW(buf.as_ptr() as *const core::ffi::c_void, &sub, &mut p2, &mut l2) }.as_bool()
            && !p2.is_null()
        {
            let u: Vec<u16> = (0..l2 as usize)
                .map(|i| unsafe { *(p2 as *const u16).add(i) })
                .collect();
            return Some(String::from_utf16_lossy(&u).trim_end_matches('\0').to_string());
        }
    }
    None
}

/// 常见程序兜底列表：没有关联时也得给用户几个能点的东西。
const FALLBACK_APPS: [&str; 5] = ["notepad.exe", "mspaint.exe", "write.exe", "cmd.exe", "WindowsPowerShell\\v1.0\\powershell.exe"];

pub fn candidates(ext: &str) -> Vec<AppCandidate> {
    let mut order: Vec<(PathBuf, bool)> = Vec::new();
    let mut push = |p: PathBuf, dflt: bool, order: &mut Vec<(PathBuf, bool)>| {
        if !order.iter().any(|(x, _)| x == &p) {
            order.push((p, dflt));
        }
    };

    if !ext.is_empty() {
        // 1) 用户选过的
        if let Some(prog) = reg_sz(
            HKEY_CURRENT_USER,
            &format!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FileExts\\{ext}\\UserChoice"),
            "ProgId",
        ) {
            if let Some(cmd) = reg_sz(HKEY_CLASSES_ROOT, &format!("{prog}\\shell\\open\\command"), "") {
                if let Some(p) = exe_from_command(&cmd) {
                    push(p, true, &mut order);
                }
            }
        }
        // 2) 打开方式里出现过的（a/b/c… 是 MRU 顺序）
        for k in ["a", "b", "c", "d", "e"] {
            if let Some(v) = reg_sz(
                HKEY_CURRENT_USER,
                &format!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FileExts\\{ext}\\OpenWithList"),
                k,
            ) {
                let p = if let Some(full) = reg_sz(
                    HKEY_LOCAL_MACHINE,
                    &format!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{v}"),
                    "",
                ) {
                    PathBuf::from(expand_env(&full.trim_matches('"')))
                } else {
                    PathBuf::from(expand_env(&v))
                };
                if p.exists() {
                    push(p, false, &mut order);
                }
            }
        }
        // 3) 关联的 ProgID
        if let Some(progid) = reg_sz(HKEY_CLASSES_ROOT, ext, "") {
            if let Some(cmd) = reg_sz(HKEY_CLASSES_ROOT, &format!("{progid}\\shell\\open\\command"), "") {
                if let Some(p) = exe_from_command(&cmd) {
                    push(p, true, &mut order);
                }
            }
        }
        // 4) SystemFileAssociations 里的 open
        if let Some(cmd) = reg_sz(
            HKEY_CLASSES_ROOT,
            &format!("SystemFileAssociations\\{ext}\\shell\\open\\command"),
            "",
        ) {
            if let Some(p) = exe_from_command(&cmd) {
                push(p, false, &mut order);
            }
        }
    }

    // 5) 兜底的常见程序
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    for f in FALLBACK_APPS {
        let p = PathBuf::from(&root).join("system32").join(f);
        if p.exists() {
            push(p, false, &mut order);
        } else {
            let p2 = PathBuf::from(&root).join(f);
            if p2.exists() {
                push(p2, false, &mut order);
            }
        }
    }

    order
        .into_iter()
        .map(|(exe, dflt)| AppCandidate {
            name: friendly_name(&exe),
            icon: icon_of(&exe),
            exe,
            is_default: dflt,
        })
        .collect()
}

/// 「打开方式」的目标类型。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    File,
    /// 文件夹 / 目录背景 / 驱动器：没有扩展名，只能按"谁能吃目录"找
    Dir,
}

/// 目录类目标的候选：① 自己注册过 Directory/Background 动词的（VS Code 的
/// "Open with Code"、Windows 终端的"在终端中打开"……）；② 常见能吃目录参数的程序。
///
/// 之前候选一律按扩展名找，目录没有扩展名 → 一个候选都没有 → 直接弹系统的
/// UWP 对话框（1 秒以上、还会拉一堆 Shell 扩展）。
pub fn candidates_dir() -> Vec<AppCandidate> {
    let mut out: Vec<AppCandidate> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (_label, exe) in crate::appfind::folder_verbs() {
        if !seen.insert(exe.to_string_lossy().to_lowercase()) {
            continue;
        }
        out.push(AppCandidate {
            name: friendly_name(&exe),
            icon: icon_of(&exe),
            exe,
            is_default: true,
        });
    }
    for a in crate::appfind::folder_apps() {
        if !seen.insert(a.exe.to_string_lossy().to_lowercase()) {
            continue;
        }
        out.push(AppCandidate {
            name: a.name.clone(),
            icon: a.hicon(),
            exe: a.exe,
            is_default: false,
        });
    }
    // 兜底：资源管理器本身（一定存在，"在这里打开"永远说得通）
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    let ex = PathBuf::from(&root).join("explorer.exe");
    if ex.exists() && !seen.contains(&ex.to_string_lossy().to_lowercase()) {
        out.push(AppCandidate { name: "文件资源管理器".to_string(), icon: icon_of(&ex), exe: ex, is_default: false });
    }
    out
}

/// 「查找可用的打开软件」：把本机装着的程序全部补进列表。
///
/// 关联的那些只占前几条，用户真正想找的（装了但没抢过关联的）藏在后面 —— 这一按
/// 就是把它们全列出来，让他自己挑。已经列过的按 exe 路径去重，不会重复出现。
fn find_more(st: &mut OpenWith) {
    let have: std::collections::HashSet<String> = st
        .cands
        .iter()
        .map(|c| c.exe.to_string_lossy().to_lowercase())
        .collect();
    let mut added = 0;
    for a in crate::appfind::installed() {
        if have.contains(&a.exe.to_string_lossy().to_lowercase()) {
            continue;
        }
        st.cands.push(AppCandidate {
            name: a.name.clone(),
            icon: a.hicon(),
            exe: a.exe.clone(),
            is_default: false,
        });
        added += 1;
    }
    if added > 0 {
        st.found = true;
    }
}

impl OpenWith {
    /// 滚轮滚几行（一次三行，与系统一致），并按列表长度钳住。
    fn scroll_by(&self, delta: i32) -> i32 {
        let list = self.list_rect();
        let per = ((list.bottom - list.top) / self.s(ROW_H)).max(1);
        let max = (self.cands.len() as i32 - per + 1).max(0);
        let d = if delta < 0 { 3 } else { -3 };
        (self.scroll + d).clamp(0, max)
    }
}

// ---------------------------------------------------------------- 打开

/// 用指定程序打开：`ShellExecuteW(open, exe, "exe" "file")`。
///
/// 参数用 `\"` 包起来是必须的 —— 路径里有空格时，不引起来会被拆成两个参数。
fn launch_with(exe: &Path, file: &Path) -> bool {
    let verb = HSTRING::from("open");
    let file_s = file.to_string_lossy().to_string();
    let params = format!("\"{}\"", file_s);
    let params = HSTRING::from(&params);
    let exe_s = exe.to_string_lossy().to_string();
    let exe_w = HSTRING::from(&exe_s);
    let dir_s = exe
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let dir_h = HSTRING::from(&dir_s);
    let dir = if dir_s.is_empty() {
        PCWSTR::null()
    } else {
        PCWSTR(dir_h.as_ptr())
    };
    let r = unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            &verb,
            &exe_w,
            &params,
            dir,
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    r.0 as isize > 32
}

/// 交给系统的"打开方式"（`rundll32 shell32.dll,OpenAs_RunDLL`）。
fn open_with_system(file: &Path) -> bool {
    let rundll = PathBuf::from(
        std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string()),
    )
    .join("system32")
    .join("rundll32.exe");
    let verb = HSTRING::from("open");
    let exe_w = HSTRING::from(rundll.to_string_lossy().as_ref());
    let params = HSTRING::from(format!(
        "shell32.dll,OpenAs_RunDLL \"{}\"",
        file.to_string_lossy()
    ));
    let r = unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            &verb,
            &exe_w,
            &params,
            PCWSTR::null(),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    r.0 as isize > 32
}

/// "始终使用此应用打开"：登记到用户层的 Applications 下并通知 Shell。
///
/// 说明白一点：Windows 10 之后真正生效的是 `FileExts\<ext>\UserChoice`，
/// 而它要求一个只对微软签名的哈希，第三方写进去大概率被忽略。所以这里做的是
/// **用户层能做的那部分**（注册应用 + OpenWithList），并显式告知 Shell 关联变了。
fn set_always(exe: &Path, ext: &str) {
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegSetValueExW, KEY_WRITE, REG_OPTION_NON_VOLATILE,
    };
    let fname = exe
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if fname.is_empty() {
        return;
    }
    let base = format!("Software\\Classes\\Applications\\{fname}");
    unsafe {
        let mut hk = HKEY::default();
        let sub = HSTRING::from(format!("{base}\\shell\\open\\command"));
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &sub,
            Some(0),
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hk,
            None,
        )
        .0 == 0
        {
            let cmd = format!("\"{}\" \"%1\"", exe.to_string_lossy());
            let wide: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes: Vec<u8> = wide.iter().flat_map(|u| u.to_le_bytes()).collect();
            let _ = RegSetValueExW(hk, &HSTRING::from(""), Some(0), REG_SZ, Some(&bytes));
            let _ = RegCloseKey(hk);
        }
        let mut hk = HKEY::default();
        let sub = HSTRING::from(format!("{base}\\Application"));
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &sub,
            Some(0),
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hk,
            None,
        )
        .0 == 0
        {
            let name = friendly_name(exe);
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes: Vec<u8> = wide.iter().flat_map(|u| u.to_le_bytes()).collect();
            let _ = RegSetValueExW(
                hk,
                &HSTRING::from("ApplicationName"),
                Some(0),
                REG_SZ,
                Some(&bytes),
            );
            let _ = RegCloseKey(hk);
        }
    }
    if !ext.is_empty() {
        // 同时记进 OpenWithList：下次这个文件类型的"打开方式"里它就排在前头
        use windows::Win32::System::Registry::{RegCloseKey, RegCreateKeyExW, RegSetValueExW, KEY_WRITE, REG_OPTION_NON_VOLATILE};
        let sub = format!(
            "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FileExts\\{ext}\\OpenWithList"
        );
        unsafe {
            let mut hk = HKEY::default();
            let sub_w = HSTRING::from(&sub);
            if RegCreateKeyExW(
                HKEY_CURRENT_USER,
                &sub_w,
                Some(0),
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut hk,
                None,
            )
            .0 == 0
            {
                let wide: Vec<u16> = fname.encode_utf16().chain(std::iter::once(0)).collect();
                let bytes: Vec<u8> = wide.iter().flat_map(|u| u.to_le_bytes()).collect();
                let _ = RegSetValueExW(hk, &HSTRING::from("a"), Some(0), REG_SZ, Some(&bytes));
                let _ = RegCloseKey(hk);
            }
        }
    }
    unsafe {
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None);
    }
}

// ---------------------------------------------------------------- 消息与绘制

unsafe extern "system" fn ow_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut OpenWith;
    if p.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let st = &mut *p;
    match msg {
        WM_ERASEBKGND => return LRESULT(1),
        WM_NCHITTEST => {
            let y = (lparam.0 >> 16) as i16 as i32;
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let mut sp = POINT { x, y };
            let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut sp);
            if sp.y < st.s(TITLE_H) {
                if sp.x >= st.close_rect().left {
                    return LRESULT(1);
                }
                return LRESULT(2);
            }
            LRESULT(1)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mem = CreateCompatibleDC(Some(hdc));
            let bmp = CreateCompatibleBitmap(hdc, st.w, st.h);
            let old = SelectObject(mem, bmp.into());
            paint(mem, st);
            let _ = BitBlt(hdc, 0, 0, st.w, st.h, Some(mem), 0, 0, SRCCOPY);
            SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
            let _ = EndPaint(hwnd, &mut ps);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = (lparam.0 >> 16) as i16 as i32;
            let h = hit_test(st, x, y);
            if h != st.hover {
                st.hover = h;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = (lparam.0 >> 16) as i16 as i32;
            match hit_test(st, x, y) {
                Hit::Close => {
                    let _ = DestroyWindow(hwnd);
                }
                Hit::Cancel => {
                    let _ = DestroyWindow(hwnd);
                }
                Hit::Row(i) => {
                    st.sel = i;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                Hit::Always => {
                    st.always = !st.always;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                Hit::Find => {
                    find_more(st);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                Hit::Browse => {
                    if let Some(p) = crate::props::pick_file(
                        hwnd,
                        "选择用来打开的程序",
                        "程序\0*.exe\0所有文件\0*.*\0\0",
                        "exe",
                    ) {
                        let exe = PathBuf::from(p);
                        st.cands.push(AppCandidate {
                            name: friendly_name(&exe),
                            icon: icon_of(&exe),
                            exe,
                            is_default: false,
                        });
                        st.sel = st.cands.len() - 1;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                Hit::Ok => {
                    if let Some(c) = st.cands.get(st.sel) {
                        if st.always && st.kind == TargetKind::File {
                            let ext = st
                                .path
                                .extension()
                                .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
                                .unwrap_or_default();
                            set_always(&c.exe, &ext);
                        }
                        launch_with(&c.exe, &st.path);
                    }
                    let _ = DestroyWindow(hwnd);
                }
                Hit::None => {}
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = (wparam.0 >> 16) as i16 as i32;
            st.scroll = st.scroll_by(delta);
            st.hover = Hit::None;
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            let bold = st.bold;
            for c in &st.cands {
                if !c.icon.0.is_null() {
                    let _ = windows::Win32::UI::WindowsAndMessaging::DestroyIcon(c.icon);
                }
            }
            drop(Box::from_raw(p));
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if !bold.0.is_null() {
                let _ = DeleteObject(bold.into());
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x <= r.right && y >= r.top && y <= r.bottom
}

fn hit_test(st: &OpenWith, x: i32, y: i32) -> Hit {
    if inside(&st.close_rect(), x, y) {
        return Hit::Close;
    }
    if inside(&st.ok_rect(), x, y) {
        return Hit::Ok;
    }
    if inside(&st.cancel_rect(), x, y) {
        return Hit::Cancel;
    }
    if inside(&st.browse_rect(), x, y) {
        return Hit::Browse;
    }
    if inside(&st.find_rect(), x, y) {
        return Hit::Find;
    }
    if st.kind == TargetKind::File && inside(&st.always_rect(), x, y) {
        return Hit::Always;
    }
    // 列表行：只认落在列表框内的，超出的（滚出可视区的候选）不算
    let list = st.list_rect();
    for i in st.scroll as usize..st.cands.len() {
        let r = st.row_rect(i);
        if r.top < list.top {
            continue;
        }
        if r.bottom > list.bottom {
            break;
        }
        if inside(&r, x, y) {
            return Hit::Row(i);
        }
    }
    Hit::None
}

unsafe fn fill(hdc: HDC, r: &RECT, color: windows::Win32::Foundation::COLORREF) {
    let b = CreateSolidBrush(color);
    let _ = FillRect(hdc, r, b);
    let _ = DeleteObject(b.into());
}

unsafe fn text(hdc: HDC, s: &str, r: RECT, color: windows::Win32::Foundation::COLORREF, flags: DRAW_TEXT_FORMAT) {
    SetTextColor(hdc, color);
    let mut v: Vec<u16> = s.encode_utf16().collect();
    let _ = DrawTextW(hdc, &mut v, &mut r.clone(), flags | DT_NOPREFIX);
}

unsafe fn paint(hdc: HDC, st: &OpenWith) {
    let t = &st.theme;
    fill(hdc, &RECT { left: 0, top: 0, right: st.w, bottom: st.h }, t.surface);
    fill(hdc, &RECT { left: 0, top: 0, right: st.w, bottom: st.s(TITLE_H) }, t.title_bg);
    let font = crate::paint::ui_font(st.scale);
    let old = SelectObject(hdc, font.into());
    SetBkMode(hdc, TRANSPARENT);
    text(
        hdc,
        "打开方式",
        RECT { left: st.s(14), top: 0, right: st.w - st.s(44), bottom: st.s(TITLE_H) },
        t.text,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    if st.hover == Hit::Close {
        fill(hdc, &st.close_rect(), t.danger);
    }
    let og = SelectObject(hdc, crate::paint::glyph_font(st.scale).into());
    text(hdc, "\u{E8BB}", st.close_rect(), t.text, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    SelectObject(hdc, og);

    // 头部：文件名 + 一句提示
    let ob = SelectObject(hdc, st.bold.into());
    text(
        hdc,
        &st.name,
        RECT { left: st.s(14), top: st.s(TITLE_H + 6), right: st.w - st.s(14), bottom: st.s(TITLE_H + 26) },
        t.text,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
    );
    SelectObject(hdc, ob);
    text(
        hdc,
        if st.kind == TargetKind::Dir {
            "选择要用哪个程序打开这个文件夹："
        } else {
            "选择要用来打开此文件的程序："
        },
        RECT { left: st.s(14), top: st.s(TITLE_H + 26), right: st.w - st.s(14), bottom: st.s(TITLE_H + 42) },
        t.text_dim,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    // 候选列表
    let list = st.list_rect();
    fill(hdc, &list, t.surface_alt);
    let _saved = SaveDC(hdc);
    let _ = IntersectClipRect(hdc, list.left, list.top, list.right, list.bottom);
    let visible = ((list.bottom - list.top) / st.s(ROW_H)) as usize + 1;
    for i in st.scroll as usize..st.cands.len().min(st.scroll as usize + visible) {
        let c = &st.cands[i];
        let r = st.row_rect(i);
        if i == st.sel {
            fill(hdc, &r, t.selection_bg);
        } else if st.hover == Hit::Row(i) {
            fill(hdc, &r, t.hover_bg);
        }
        let icon_s = st.s(24);
        let iy = (r.top + r.bottom) / 2 - icon_s / 2;
        let _ = DrawIconEx(hdc, r.left + st.s(10), iy, c.icon, icon_s, icon_s, 0, None, DI_NORMAL);
        let tx = r.left + st.s(10) + icon_s + st.s(10);
        text(
            hdc,
            &c.name,
            RECT { left: tx, top: r.top + st.s(4), right: r.right - st.s(8), bottom: r.top + st.s(22) },
            if i == st.sel { t.selection_text } else { t.text },
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        let sub = if c.is_default {
            format!("{} · 当前默认", c.exe.to_string_lossy())
        } else {
            c.exe.to_string_lossy().to_string()
        };
        text(
            hdc,
            &sub,
            RECT { left: tx, top: r.top + st.s(22), right: r.right - st.s(8), bottom: r.bottom - st.s(2) },
            t.text_dim,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
    }

    let _ = RestoreDC(hdc, -1);

    // "始终使用"复选框（目录类目标没有"此类文件"，不画）
    if st.kind != TargetKind::File {
        // 目录：占位留空，避免底部控件整体上移
    } else {
    let ar = st.always_rect();
    if st.hover == Hit::Always {
        fill(hdc, &ar, t.hover_bg);
    }
    let box_s = st.s(16);
    let cy = (ar.top + ar.bottom) / 2;
    let bx = RECT { left: ar.left, top: cy - box_s / 2, right: ar.left + box_s, bottom: cy - box_s / 2 + box_s };
    fill(hdc, &bx, if st.always { t.accent } else { t.surface_alt });
    for e in [
        RECT { left: bx.left, top: bx.top, right: bx.right, bottom: bx.top + 1 },
        RECT { left: bx.left, top: bx.bottom - 1, right: bx.right, bottom: bx.bottom },
        RECT { left: bx.left, top: bx.top, right: bx.left + 1, bottom: bx.bottom },
        RECT { left: bx.right - 1, top: bx.top, right: bx.right, bottom: bx.bottom },
    ] {
        fill(hdc, &e, t.border);
    }
    if st.always {
        let og = SelectObject(hdc, crate::paint::glyph_font(st.scale).into());
        text(hdc, "\u{E73E}", bx, t.selection_text, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        SelectObject(hdc, og);
    }
    text(
        hdc,
        "始终使用此应用打开此类文件",
        RECT { left: ar.left + box_s + st.s(8), top: ar.top, right: ar.right, bottom: ar.bottom },
        t.text,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    }

    for (r, label, hit, accent) in [
        (st.find_rect(), "查找可用的打开软件", Hit::Find, false),
        (st.browse_rect(), "浏览…", Hit::Browse, false),
        (st.ok_rect(), "确定", Hit::Ok, true),
        (st.cancel_rect(), "取消", Hit::Cancel, false),
    ] {
        fill(
            hdc,
            &r,
            if st.hover == hit {
                t.hover_bg
            } else if accent {
                t.accent
            } else {
                t.surface_alt
            },
        );
        for e in [
            RECT { left: r.left, top: r.top, right: r.right, bottom: r.top + 1 },
            RECT { left: r.left, top: r.bottom - 1, right: r.right, bottom: r.bottom },
            RECT { left: r.left, top: r.top, right: r.left + 1, bottom: r.bottom },
            RECT { left: r.right - 1, top: r.top, right: r.right, bottom: r.bottom },
        ] {
            fill(hdc, &e, t.border);
        }
        text(
            hdc,
            label,
            r,
            if accent { t.selection_text } else { t.text },
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
    }

    SelectObject(hdc, old);
}
