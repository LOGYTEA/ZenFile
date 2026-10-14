//! 「查找可用的打开软件」：扫出本机装着的、能拿来打开东西的程序。
//!
//! ## 为什么要自己扫
//!
//! 「打开方式」原来只认**文件扩展名的关联**（`UserChoice` / ProgID /
//! SystemFileAssociations），于是：
//!   * 文件夹、目录背景、驱动器**没有扩展名** → 一个候选都没有，只能弹系统的
//!     那个 UWP 对话框（1 秒以上、还会拉一堆 Shell 扩展）；
//!   * 装了但没抢过关联的程序（VS Code 装在旁边、PyCharm、Sublime…）**永远不出现**。
//!
//! 用户要的是"查找可用的打开软件" —— 那就把本机装着的程序**列出来让他挑**，
//! 而不是猜他可能想用哪个。
//!
//! ## 数据来源（全部是 Windows 自己维护的，与资源管理器同源）
//!
//!   1. `HKCR\Applications\<exe>` —— 「打开方式」列表的正式来源。显示名取它的
//!      `FriendlyAppName`（可能是 `@xxx.dll,-1234` 这种间接串，要
//!      `SHLoadIndirectString` 解一次，拿到的是"记事本"这种本地化名字）。
//!   2. `HKCR\Applications\<exe>\shell\<verb>\command` —— 真正能跑的命令行
//!      （UWP 壳、需要前置参数的都只有这里才有）。
//!   3. `HKLM / HKCU\...\App Paths\<exe>` —— 补完整路径（HKCR 里常常只有文件名）。
//!
//! ## 目录类目标（文件夹 / 目录背景 / 驱动器）
//!
//! 谁给自己注册过"用我打开这个文件夹"，谁就是候选 —— 也就是
//! `HKCR\Directory\shell\*\command` 与 `HKCR\Directory\Background\shell\*\command`
//! （VS Code 的"Open with Code"、Windows 终端的"在终端中打开"都是这么挂的）。
//! 再叠一张常见的"能吃目录参数"的程序名单兜底。
//!
//! 结果**进程内缓存一次**：翻注册表 + 取几百个图标是有成本的，不能每帧重算。

use std::path::PathBuf;

use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::WIN32_ERROR;
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_CLASSES_ROOT,
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_SZ,
};
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHLoadIndirectString, SHGFI_ICON, SHGFI_SMALLICON,
};
use windows::Win32::UI::WindowsAndMessaging::HICON;

use crate::openwith::{exe_from_command, expand_env, friendly_name, icon_of};

/// 一条"本机装着的程序"。
#[derive(Clone)]
pub struct FoundApp {
    /// 显示名（本地化后的，如"记事本"）
    pub name: String,
    pub exe: PathBuf,
    /// 图标句柄的**整数值**。
    ///
    /// 不存  本身：它里面是裸指针，不是 ，放不进进程级 
    /// 缓存（编译期就卡住）。图标由本进程持有到退出，句柄值一直有效。
    pub icon: isize,
    /// 能不能拿目录当参数打开（注册过 Directory/Background 动词，或在常见名单里）
    pub dir_capable: bool,
}

impl FoundApp {
    pub fn hicon(&self) -> HICON {
        HICON(self.icon as *mut core::ffi::c_void)
    }
}

static CACHE: std::sync::OnceLock<Vec<FoundApp>> = std::sync::OnceLock::new();

/// 本机装着的全部程序（去重、去垃圾、按名字排序；结果缓存）。
pub fn installed() -> &'static Vec<FoundApp> {
    CACHE.get_or_init(|| {
        let mut v = scan();
        v.sort_by(|a, b| {
            // 能吃目录的排前面（文件夹的"打开方式"第一屏就是它们），其次按名字
            b.dir_capable
                .cmp(&a.dir_capable)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        v
    })
}

/// 只看能吃目录参数的那些。
pub fn folder_apps() -> Vec<FoundApp> {
    installed().iter().filter(|a| a.dir_capable).cloned().collect()
}

// ---------------------------------------------------------------- 扫描

fn scan() -> Vec<FoundApp> {
    let mut out: Vec<FoundApp> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let dir_exes = dir_registered_exes();

    // 1) HKCR\Applications —— 正式来源
    for name in enum_subkeys(HKEY_CLASSES_ROOT, "Applications") {
        let base = format!("Applications\\{name}");
        let app_name = friendly_of(&base).unwrap_or_default();
        let exe = exe_of_application(&base, &name);
        let Some(exe) = exe else { continue };
        if junk(&exe, &app_name) {
            continue;
        }
        let key = exe.to_string_lossy().to_lowercase();
        if !seen.insert(key.clone()) {
            continue;
        }
        out.push(FoundApp {
            name: if app_name.is_empty() {
                friendly_name(&exe)
            } else {
                app_name
            },
            dir_capable: dir_exes.contains(&key),
            icon: icon_of(&exe).0 as isize,
            exe,
        });
        if out.len() >= 400 {
            break;
        }
    }

    // 2) App Paths 兜底：HKCR\Applications 没收录的（不少绿色软件只在这里）
    for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        let sub = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths";
        for name in enum_subkeys(root, sub) {
            let Some(cmd) = reg_sz(root, &format!("{sub}\\{name}"), "") else {
                continue;
            };
            let p = PathBuf::from(expand_env(cmd.trim_matches('"').trim()));
            if !p.exists() || junk(&p, "") {
                continue;
            }
            let key = p.to_string_lossy().to_lowercase();
            if !seen.insert(key.clone()) {
                continue;
            }
            out.push(FoundApp {
                name: friendly_name(&p),
                dir_capable: dir_exes.contains(&key),
                icon: icon_of(&p).0 as isize,
                exe: p,
            });
            if out.len() >= 400 {
                return out;
            }
        }
    }
    out
}

/// 一个应用程序键下真正能跑的那个 exe。
fn exe_of_application(base: &str, keyname: &str) -> Option<PathBuf> {
    // ① shell\open\command；没有 open 就取 shell 下第一个动词
    let mut verbs = enum_subkeys(HKEY_CLASSES_ROOT, &format!("{base}\\shell"));
    if let Some(pos) = verbs.iter().position(|v| v.eq_ignore_ascii_case("open")) {
        verbs.swap(0, pos);
    }
    for v in verbs {
        if let Some(cmd) = reg_sz(HKEY_CLASSES_ROOT, &format!("{base}\\shell\\{v}\\command"), "") {
            if let Some(p) = exe_from_command(&cmd) {
                return Some(p);
            }
        }
    }
    // ② App Paths：HKCR 里常常只留了个文件名
    for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        if let Some(v) = reg_sz(
            root,
            &format!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{keyname}"),
            "",
        ) {
            let p = PathBuf::from(expand_env(v.trim_matches('"').trim()));
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

/// 显示名：优先 `FriendlyAppName`（可能是间接串），其次 `ApplicationName`。
fn friendly_of(base: &str) -> Option<String> {
    for v in ["FriendlyAppName", "ApplicationName"] {
        if let Some(s) = reg_sz(HKEY_CLASSES_ROOT, base, v) {
            if let Some(t) = resolve_indirect(&s) {
                if !t.trim().is_empty() {
                    return Some(t.trim().to_string());
                }
            }
        }
    }
    // 有些程序把名字挂在动词上（"Open with Code"）
    if let Some(s) = reg_sz(HKEY_CLASSES_ROOT, &format!("{base}\\shell\\open"), "MUIVerb") {
        if let Some(t) = resolve_indirect(&s) {
            if !t.trim().is_empty() {
                return Some(t.trim().to_string());
            }
        }
    }
    None
}

/// `@C:\...\app.exe,-1234` 这种间接串 → 真正的本地化文字。
fn resolve_indirect(s: &str) -> Option<String> {
    let t = s.trim();
    if !t.starts_with('@') {
        return Some(t.to_string());
    }
    let src = HSTRING::from(t);
    let mut buf = [0u16; 512];
    let ok = unsafe { SHLoadIndirectString(PCWSTR(src.as_ptr()), &mut buf, None) };
    if ok.is_err() {
        return None;
    }
    let n = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]))
}

/// 注册过"用我打开这个文件夹"的程序（Directory / Directory\Background 的动词）。
///
/// 返回 exe 的**小写**路径集合，用来给扫出来的程序打 `dir_capable` 标记。
fn dir_registered_exes() -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    for root in [HKEY_CLASSES_ROOT, HKEY_CURRENT_USER] {
        let prefix = if root == HKEY_CLASSES_ROOT {
            String::new()
        } else {
            "Software\\Classes\\".to_string()
        };
        for base in ["Directory\\shell", "Directory\\Background\\shell", "Drive\\shell"] {
            let base = format!("{prefix}{base}");
            for verb in enum_subkeys(root, &base) {
                if verb.eq_ignore_ascii_case("ZENFILE") {
                    continue; // 自己注册的"用 ZenFile 打开"不算候选
                }
                if let Some(cmd) = reg_sz(root, &format!("{base}\\{verb}\\command"), "") {
                    if let Some(p) = exe_from_command(&cmd) {
                        set.insert(p.to_string_lossy().to_lowercase());
                    }
                }
            }
        }
    }
    // 常见"能吃目录参数"的程序：用户多半就是想用这些打开一个文件夹，
    // 它们不一定去注册 Directory 动词（尤其是绿色版 / 手动装的）。
    for f in FOLDER_EXES {
        for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
            if let Some(v) = reg_sz(
                root,
                &format!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{f}"),
                "",
            ) {
                let p = PathBuf::from(expand_env(v.trim_matches('"').trim()));
                if p.exists() {
                    set.insert(p.to_string_lossy().to_lowercase());
                }
            }
        }
    }
    set
}

/// 常见的"能拿目录当参数"的程序（exe 文件名小写）。
const FOLDER_EXES: [&str; 22] = [
    "code.exe",
    "Code.exe",
    "devenv.exe",
    "idea64.exe",
    "pycharm64.exe",
    "webstorm64.exe",
    "goland64.exe",
    "rider64.exe",
    "clion64.exe",
    "sublime_text.exe",
    "notepad++.exe",
    "wt.exe",
    "WindowsTerminal.exe",
    "powershell.exe",
    "WindowsPowerShell\\v1.0\\powershell.exe",
    "cmd.exe",
    "explorer.exe",
    "TOTALCMD64.EXE",
    "TOTALCMD.EXE",
    "Everything.exe",
    "7zFM.exe",
    "WinRAR.exe",
];

/// 垃圾过滤：卸载器、安装器、运行时宿主、系统组件 —— 列出来只会把列表淹掉。
fn junk(exe: &std::path::Path, name: &str) -> bool {
    let fname = exe
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if fname.is_empty() || !fname.ends_with(".exe") {
        return true;
    }
    const BAD: [&str; 26] = [
        "unins", "uninst", "setup", "install", "update", "updater", "crash", "report",
        "helper", "service", "launcher", "activation", "vcredist", "dxsetup", "dotnet",
        "regsvr32", "rundll32", "dllhost", "conhost", "wscript", "cscript", "msiexec",
        "svchost", "taskhostw", "backgroundtaskhost", "applicationframehost",
    ];
    if BAD.iter().any(|b| fname.contains(b)) {
        return true;
    }
    let low = exe.to_string_lossy().to_lowercase();
    if low.contains("\\windows\\winsxs\\") || low.contains("\\windows\\servicing\\") {
        return true;
    }
    let nl = name.to_lowercase();
    if nl.contains("uninstall") || nl.contains("卸载") || nl.contains("安装") {
        return true;
    }
    false
}

// ---------------------------------------------------------------- 注册表小工具

pub fn reg_sz(root: HKEY, sub: &str, value: &str) -> Option<String> {
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

/// 枚举某个键下的全部子键名。
fn enum_subkeys(root: HKEY, path: &str) -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let mut hk = HKEY::default();
        let sub = HSTRING::from(path);
        if RegOpenKeyExW(root, &sub, Some(0), KEY_READ, &mut hk) != WIN32_ERROR(0) {
            return out;
        }
        let mut i = 0u32;
        loop {
            let mut name = [0u16; 512];
            let mut len = name.len() as u32;
            if RegEnumKeyExW(
                hk,
                i,
                Some(PWSTR(name.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            ) != WIN32_ERROR(0)
            {
                break;
            }
            i += 1;
            if len == 0 || len as usize > name.len() {
                continue;
            }
            out.push(String::from_utf16_lossy(&name[..len as usize]));
            if out.len() >= 2000 {
                break;
            }
        }
        let _ = RegCloseKey(hk);
    }
    out
}

/// 目录/背景/驱动器上**已经注册**的动词：(显示名, exe)。
///
/// 与 `installed()` 分开：这些是"程序自己声明能吃目录"的，排第一屏。
pub fn folder_verbs() -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for root in [HKEY_CLASSES_ROOT, HKEY_CURRENT_USER] {
        let prefix = if root == HKEY_CLASSES_ROOT {
            String::new()
        } else {
            "Software\\Classes\\".to_string()
        };
        for base in ["Directory\\shell", "Directory\\Background\\shell", "Drive\\shell"] {
            let base = format!("{prefix}{base}");
            for verb in enum_subkeys(root, &base) {
                if verb.eq_ignore_ascii_case("ZENFILE") {
                    continue;
                }
                let Some(cmd) = reg_sz(root, &format!("{base}\\{verb}\\command"), "") else {
                    continue;
                };
                let Some(exe) = exe_from_command(&cmd) else { continue };
                if !seen.insert(exe.to_string_lossy().to_lowercase()) {
                    continue;
                }
                let label = reg_sz(root, &format!("{base}\\{verb}"), "MUIVerb")
                    .and_then(|s| resolve_indirect(&s))
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| friendly_name(&exe));
                out.push((label, exe));
            }
        }
    }
    out
}

/// 与 `openwith::icon_of` 同源（这里只是把壳再包一层，省得调用方引两个模块）。
pub fn icon_for(exe: &std::path::Path) -> HICON {
    let mut sfi = windows::Win32::UI::Shell::SHFILEINFOW::default();
    let res = unsafe {
        SHGetFileInfoW(
            &HSTRING::from(exe.to_string_lossy().as_ref()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut sfi),
            std::mem::size_of::<windows::Win32::UI::Shell::SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_SMALLICON,
        )
    };
    if res != 0 && !sfi.hIcon.0.is_null() {
        return sfi.hIcon;
    }
    HICON::default()
}
