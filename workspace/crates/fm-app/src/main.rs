//! fm-app 主体进程入口。
//!
//! 职责边界（技术方案 §3.1 / §3.6）：
//!   - 只有本进程注册托盘与开机自启；
//!   - 所有子模块由本进程 CreateProcess 拉起，宿主常驻；
//!   - 退出顺序固定：先终止子进程，宿主最后退出。

// 以 Windows 子系统构建：否则双击运行时系统会额外分配一个控制台窗口（用户看到的
// "cmd 窗口"）。命令行模式（--selftest / --version）再挂回父进程控制台输出。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod icons;
mod menu;
mod openwith;
mod rename;
mod paint;
mod props;
mod shellnew;
mod settings;
mod sysview;
mod theme;

use std::path::PathBuf;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, COLORREF, ERROR_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT,
    POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Dwm::DwmExtendFrameIntoClientArea;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, EndPaint, InvalidateRect, PAINTSTRUCT, SetBkColor, SetTextColor,
};
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::Shell_NotifyIconW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, NOTIFY_ICON_DATA_FLAGS,
};
use windows::Win32::Graphics::Gdi::{ClientToScreen, HBRUSH, HDC, ScreenToClient};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, RegisterHotKey, ReleaseCapture, SetCapture, UnregisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::DataExchange::COPYDATASTRUCT;

use app::{init_com, App};
use fm_core::config;
use fm_core::hotkey;
use fm_core::paths::Target;

/// App 实例的全局裸指针。WndProc 与子控件回调需要通过它访问状态；
/// 全程单线程 UI，因此不存在数据竞争。
///
/// 多窗口（第 19 轮第 0 项）：每个主窗口各自持有一份 `App`（放在自己的
/// GWLP_USERDATA 里），这里保留的是**第一个**窗口的 App —— 托盘、全局热键
/// 这些"进程级"的东西挂在它身上。`WINDOWS` 是全部主窗口的清单。
static mut APP: *mut App = std::ptr::null_mut();
/// 本进程所有主窗口。最后一个关掉时才退出消息循环。
pub(crate) static mut WINDOWS: Vec<HWND> = Vec::new();
/// 托盘宿主的隐藏窗口类名。新实例靠 `FindWindowW` 找到它，把"要开的路径"
/// 用 WM_COPYDATA 递过去然后自己退出 —— 于是多个窗口跑在**同一个进程**里，
/// 图标缓存、文件名索引、配置缓存全部共用，不会因为多开而翻倍。
static mut TRAY_CLASS: HSTRING = HSTRING::new();
/// 托盘宿主的隐藏窗口句柄；本进程不是宿主时为 null。
static mut TRAY_HWND: HWND = HWND(std::ptr::null_mut());

const WM_TRAY: u32 = WM_APP + 1;
/// 后台文件操作（复制/移动/拖拽落地）完成。由 worker 线程 Post 回来，
/// UI 线程在这条消息里收口：报错 + 重列当前目录。**不能**在 worker 里直接碰 UI。
pub const WM_APP_IO_DONE: u32 = WM_APP + 2;
/// 后台枚举回收站完成。WPARAM = 世代号，对不上说明结果已过期。
pub const WM_APP_RECYCLE: u32 = WM_APP + 3;
const TIMER_DRAIN: usize = 1;
/// 对外显示的产品名。托盘提示、窗口标题、标题栏落款共用这一份，改一处就够。
pub const APP_TITLE: &str = "ZenFile";
/// 对外显示的版本串。
///
/// Cargo.toml 里的 `version` 只能写 semver（"Demo 0.3 Daily 2026-10-11" 不是合法
/// semver，写了 cargo 直接报错），所以"人看的那一版号"单独放这里：
/// `ZenFile.exe --version`、窗口标题、「关于」对话框三处共用。
/// 发新版时改这一行（0.3 → 下一段号）并把日期换成当天即可。
pub const APP_VERSION: &str = "Demo 0.3 Daily 2026-10-11";
/// 目录栏下沿进度条的动画节拍。一直挂着，但只有 busy() 时才真的重画。
const TIMER_BUSY: usize = 2;
const HOTKEY_MAIN: i32 = 1;
/// 系统双击时限之上一点点：低于它是"连着两下"（= 打开），高于它算"两次慢速单击"（= 改名）。
const DOUBLE_CLICK_MS: u64 = 500;

/// fm-app 自己的自检项（`fm-core::selftest` 碰不到 UI 层这边的模块）。
fn app_selftest() -> i32 {
    let cases = crate::sysview::selftest();
    let mut pass = 0usize;
    println!("\n—— fm-app 附加自检 ——");
    for (name, ok, detail) in &cases {
        if *ok {
            pass += 1;
            if detail.is_empty() {
                println!("  [OK]   {name}");
            } else {
                println!("  [OK]   {name} — {detail}");
            }
        } else {
            println!("  [FAIL] {name} — {detail}");
        }
    }
    println!("通过 {}/{} 项", pass, cases.len());
    if pass == cases.len() { 0 } else { 1 }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--selftest" || a == "--self-test") {
        // Windows 子系统下 CRT 的 stdout 是空的，先把父进程的控制台挂回来
        unsafe { attach_console() };
        let rc = fm_core::selftest::run();
        let rc2 = app_selftest();
        std::process::exit(if rc != 0 || rc2 != 0 { 1 } else { 0 });
    }
    if args.iter().any(|a| a == "--version") {
        unsafe { attach_console() };
        println!(
            "{APP_TITLE} {APP_VERSION} (pkg {} / protocol {})",
            env!("CARGO_PKG_VERSION"),
            fm_core::PROTOCOL_VERSION
        );
        return;
    }

    let tray_mode = args.iter().any(|a| a == "--tray");
    /// 强制新开进程（标签拖出窗口 → 拆分窗口时用）。默认不开：见下。
    let force_process = args.iter().any(|a| a == "--new-process");
    let start_path = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(PathBuf::from)
        // 无参数时落在"主页"（与资源管理器首页一致），而不是进程的当前目录
        .unwrap_or_else(|| PathBuf::from(app::HOME_PATH));

    // ---------------------------------------------------------------- 多窗口（第 19 轮第 0 项）
    //
    // 老行为是"抢不到单实例互斥量就退出"，于是**永远只有一个窗口**。现在改成：
    //   * 已经有一个实例在跑 → 把要开的路径递给它，它在本进程里再开一个窗口，
    //     本进程随即退出。多个窗口共享同一份图标缓存 / 文件名索引 / 配置，
    //     这就是"多开但不吃资源"的做法（实测第二个窗口只多几百 KB）。
    //   * `--new-process`（标签拖出窗口）才真的新起进程；它抢不到托盘互斥量，
    //     因此**托盘始终只有一个**，且卫星进程不注册热键、不拉子模块。
    if !force_process && hand_off_to_running_instance(&start_path) {
        return;
    }

    // 托盘归属锁：抢到的那个进程负责托盘。卫星进程抢不到 → 不注册托盘。
    // 这样无论开多少个窗口/进程，系统托盘里始终只有一个 ZenFile 图标。
    let tray_mutex = unsafe {
        windows::Win32::System::Threading::CreateMutexW(None, false, &HSTRING::from("ZenFile.TrayOwner"))
    };
    // 注意：windows-rs 的 CreateMutexW 只看句柄是否为 NULL —— 名字已存在时
    // 系统照样返回一个有效句柄，所以这里**必须自己看 GetLastError**。
    // 只看 Ok/Err 的话每个进程都会以为自己是宿主，托盘图标就会有好几个。
    let tray_owner = unsafe { GetLastError() } != ERROR_ALREADY_EXISTS;

    // COM 必须在 STA（§2 / §7.1）：上下文菜单与 IFileOperation 的前置条件。
    init_com();

    let cfg = match (|| -> fm_core::Result<fm_core::config::Config> {
        let dir = config::app_dir()?;
        std::fs::create_dir_all(&dir)?;
        fm_core::config::Config::load(&dir)
    })() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("配置加载失败：{e}，使用默认值");
            fm_core::config::Config::defaults().unwrap()
        }
    };

    let instance: Option<HINSTANCE> = unsafe {
        GetModuleHandleW(PCWSTR::null()).ok().map(|m| windows::Win32::Foundation::HINSTANCE(m.0))
    };
    register_classes(instance);
    register_tray_class(instance);

    menu::register_menu_class();
    props::register_props_class();
    openwith::register_class();
    rename::register_class();

    // 托盘宿主：一个**隐藏的顶层窗口**（不能是 HWND_MESSAGE —— 那种窗口
    // FindWindowW 找不到，新实例就没法把路径递过来）。它永不显示，只负责收
    // WM_TRAY 与 WM_COPYDATA。挂在隐藏窗口上而不是某个主窗口上，是为了
    // "第一个窗口关了托盘就跟着没了"这种情况不再发生。
    if tray_owner {
        let th = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(TRAY_CLASS.as_ptr()),
                PCWSTR(HSTRING::from("ZenFileTrayHost").as_ptr()),
                WS_POPUP,
                0, 0, 0, 0,
                None, None, instance, None,
            )
        };
        if let Ok(th) = th {
            unsafe {
                TRAY_HWND = th;
                add_tray(th);
            }
        }
    }

    let Some(hwnd) = spawn_window(instance, cfg.clone(), start_path.clone(), true, !tray_mode) else {
        eprintln!("创建主窗口失败");
        return;
    };

    // 已启用模块作为子进程启动（导致任务管理器里的树状折叠）
    // 只有宿主进程拉子模块：卫星进程再拉一份就是纯浪费。
    if tray_owner {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let app = unsafe { &mut *APP };
                app.spawn_modules(dir);
            }
        }
    }

    let mut msg = MSG::default();
    loop {
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 == 0 || r.0 == -1 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }

    // 退出顺序 §3.6：先终止子进程再让宿主退出
    unsafe {
        if !TRAY_HWND.0.is_null() {
            remove_tray(TRAY_HWND);
        }
        if !APP.is_null() {
            let app = &mut *APP;
            if let Some(tree) = app.tree.take() {
                drop(tree);
            }
            if let Some(w) = app.watcher.take() {
                w.stop();
            }
        }
        let _ = drop_app();
        if let Ok(m) = tray_mutex {
            let _ = CloseHandle(m);
        }
    }
}

/// 新建一个主窗口（连同它自己的 `App`）。本进程内可以有任意多个。
///
/// `primary` 表示这是本进程的第一个窗口：全局热键只注册一次（同一个热键
/// 在同一线程里注册两次会失败），托盘由隐藏的托盘宿主窗口负责，与这里无关。
fn spawn_window(
    instance: Option<HINSTANCE>,
    cfg: fm_core::config::Config,
    start: PathBuf,
    primary: bool,
    show: bool,
) -> Option<HWND> {
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(MAIN_CLASS.as_ptr()),
            // 窗口标题带上版本号：alt-tab / 任务栏一眼能看出跑的是哪一版
            PCWSTR(HSTRING::from(format!("{APP_TITLE} {APP_VERSION}")).as_ptr()),
            // WS_CLIPCHILDREN 不能省：搜索框是一个子 EDIT 控件，放大镜和空态
            // 提示都是画在它**自己**的 DC 上的。父窗口每次重绘会把整条工具栏
            // （含搜索框那块矩形）刷成底色，没有这个样式就会把子控件刚画好的
            // 装饰整块盖掉 —— 表现是"不点它的时候有图标和 placeholder，
            // 一点上去父窗口重绘一次，就只剩一块空板了"。
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN, // 含 WS_THICKFRAME：保留贴靠与最大化动画的前提 §4
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1180,
            760,
            None,
            None,
            instance,
            None,
        )
    };
    let Ok(hwnd) = hwnd else { return None };

    // 注意：这里必须是全 0。早期版本把 cyTopHeight 设为 1 想保留 DWM 圆角，
    // 结果那 1px 玻璃区域在窗口激活时被 DWM 渲染成白色 —— 实测窗口顶部 y=0
    // 整行 1180 个近白像素，也就是"顶部白线"和"标题栏按钮上的白框"，
    // 两者是同一个根因。贴靠/最大化动画由 WM_NCHITTEST 返回 HTMAXBUTTON 保证，
    // 不依赖这 1px 扩展。
    unsafe {
        let margins = MARGINS {
            cxLeftWidth: 0,
            cxRightWidth: 0,
            cyTopHeight: 0,
            cyBottomHeight: 0,
        };
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
    }

    let mut boxed = Box::new(App::new(hwnd, cfg));
    boxed.metrics = theme::Metrics::new(unsafe { GetDpiForWindow(hwnd) });
    // 量度按 DPI 算完之后，再把"用户拖过的导航窗格宽度"盖回去
    boxed.sync_metrics();
    boxed.open_tab(start);
    let app = Box::into_raw(boxed);
    unsafe {
        if APP.is_null() {
            APP = app;
        }
        let cascade = WINDOWS.len() as i32;
        WINDOWS.push(hwnd);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, app as isize);
        // 第二个及以后的窗口错开一点，否则新窗口把旧的整个盖住，看着像没开
        if cascade > 0 {
            let _ = SetWindowPos(
                hwnd,
                None,
                80 + cascade * 34,
                60 + cascade * 34,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    let app = unsafe { &mut *app };
    paint::create_children(app, instance);
    paint::sync_address(app);

    // 全局热键 §8：默认 Ctrl+Alt+*，Win 组合保留给用户自行配置。
    // 只能注册一次 —— 同一个线程里对同一个 ID 注册第二次会失败。
    if primary {
        register_hotkeys(app, hwnd);
    }

    unsafe {
        let _ = SetTimer(Some(hwnd), TIMER_DRAIN, 100, None);
        let _ = SetTimer(Some(hwnd), TIMER_BUSY, 50, None);
        if show {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = windows::Win32::Graphics::Gdi::UpdateWindow(hwnd);
        }
    }
    Some(hwnd)
}

/// 关掉本进程所有窗口（托盘菜单「退出」）。
pub unsafe fn close_all_windows() {
    let all: Vec<HWND> = WINDOWS.clone();
    for h in all {
        let _ = DestroyWindow(h);
    }
    if !TRAY_HWND.0.is_null() {
        let _ = DestroyWindow(TRAY_HWND);
    }
}

/// 在本进程里再开一个主窗口（Ctrl+N / 托盘菜单 / 标签拆分）。
///
/// 复用当前进程的 App 配置对象：图标缓存、文件名索引、配置都只有一份，
/// 第二个窗口不会把内存翻一倍。
pub fn new_window(path: PathBuf) {
    let instance = unsafe { GetModuleHandleW(PCWSTR::null()).ok().map(|m| HINSTANCE(m.0)) };
    let cfg = match app_ref_opt() {
        Some(a) => a.cfg.clone(),
        None => match fm_core::config::Config::defaults() {
            Ok(c) => c,
            Err(_) => return,
        },
    };
    if let Some(h) = spawn_window(instance, cfg, path, false, true) {
        unsafe {
            let _ = SetForegroundWindow(h);
        }
    }
}

/// 把第 n 个窗口提到前台（托盘菜单里的窗口清单）。
pub fn focus_window(i: usize) {
    unsafe {
        let Some(h) = WINDOWS.get(i).copied() else { return };
        if IsIconic(h).0 != 0 {
            let _ = ShowWindow(h, SW_RESTORE);
        }
        if IsWindowVisible(h).0 == 0 {
            let _ = ShowWindow(h, SW_SHOW);
        }
        let _ = SetForegroundWindow(h);
    }
}

// ---------------------------------------------------------------- 托盘宿主窗口

fn register_tray_class(instance: Option<HINSTANCE>) {
    unsafe {
        TRAY_CLASS = HSTRING::from("ZenFileTrayHost");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(tray_proc),
            hInstance: instance.unwrap_or_default(),
            lpszClassName: PCWSTR(TRAY_CLASS.as_ptr()),
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
}

/// 把"要打开的路径"交给已经在跑的那个实例，成功则返回 true（本进程随即退出）。
///
/// 这是"多开不吃资源"的关键：新窗口开在老进程里，图标缓存、文件名索引、
/// 配置对象全是同一份，第二个窗口只多几百 KB，而不是再来一个 27MB。
fn hand_off_to_running_instance(path: &std::path::Path) -> bool {
    unsafe { TRAY_CLASS = HSTRING::from("ZenFileTrayHost") };
    let host = unsafe { FindWindowW(PCWSTR(TRAY_CLASS.as_ptr()), None) };
    let Ok(host) = host else { return false };
    if host.0.is_null() {
        return false;
    }
    let mut text: Vec<u16> = path.to_string_lossy().encode_utf16().collect();
    text.push(0);
    let mut cds = COPYDATASTRUCT {
        dwData: 1, // 1 = "开一个新窗口"
        cbData: (text.len() * 2) as u32,
        lpData: text.as_mut_ptr() as *mut core::ffi::c_void,
    };
    unsafe {
        let _ = SendMessageW(
            host,
            WM_COPYDATA,
            Some(WPARAM(0)),
            Some(LPARAM(&mut cds as *mut COPYDATASTRUCT as isize)),
        );
    }
    true
}

unsafe extern "system" fn tray_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COPYDATA => {
            let cds = &*(lparam.0 as *const COPYDATASTRUCT);
            if cds.dwData == 1 && !cds.lpData.is_null() && cds.cbData > 1 {
                let slice = std::slice::from_raw_parts(cds.lpData as *const u16, (cds.cbData as usize) / 2);
                let s = String::from_utf16_lossy(slice);
                let s = s.trim_end_matches('\0');
                let p = if s.is_empty() {
                    PathBuf::from(app::HOME_PATH)
                } else {
                    PathBuf::from(s)
                };
                let instance = GetModuleHandleW(PCWSTR::null())
                    .ok()
                    .map(|m| HINSTANCE(m.0));
                let cfg = app_ref_opt()
                    .map(|a| a.cfg.clone())
                    .unwrap_or_else(|| fm_core::config::Config::defaults().unwrap());
                if let Some(h) = spawn_window(instance, cfg, p, false, true) {
                    let _ = ShowWindow(h, SW_SHOW);
                    let _ = SetForegroundWindow(h);
                }
            }
            LRESULT(1)
        }
        WM_TRAY => {
            match lparam.0 as u32 {
                // 左键单击 / 双击：有窗口就把它们全叫回来（含"关闭时退到托盘"
                // 藏起来的那些），一个都没有才新开一个。
                0x0201 | 0x0203 => {
                    if WINDOWS.is_empty() {
                        let instance = GetModuleHandleW(PCWSTR::null())
                            .ok()
                            .map(|m| HINSTANCE(m.0));
                        let cfg = app_ref_opt()
                            .map(|a| a.cfg.clone())
                            .unwrap_or_else(|| fm_core::config::Config::defaults().unwrap());
                        if let Some(h) =
                            spawn_window(instance, cfg, PathBuf::from(app::HOME_PATH), false, true)
                        {
                            let _ = SetForegroundWindow(h);
                        }
                    } else {
                        let mut first = None;
                        for h in WINDOWS.iter() {
                            let hh = *h;
                            if IsIconic(hh).0 != 0 {
                                let _ = ShowWindow(hh, SW_RESTORE);
                            }
                            if IsWindowVisible(hh).0 == 0 {
                                let _ = ShowWindow(hh, SW_SHOW);
                            }
                            if first.is_none() {
                                first = Some(hh);
                            }
                        }
                        if let Some(h) = first {
                            let _ = SetForegroundWindow(h);
                        }
                    }
                }
                // 右键：托盘菜单（新建窗口 / 窗口清单 / 退出）
                0x0205 => show_tray_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            remove_tray(hwnd);
            // 托盘宿主没了：如果还有主窗口在跑，说明是用户关了最后一个窗口
            // 又退了宿主 —— 这时候整个进程该收工。
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 托盘右键菜单。用自绘菜单（和窗口内右键同一套观感）。
fn show_tray_menu(hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    let (scale, theme, app) = match app_ref_opt() {
        Some(a) => (a.metrics.scale, a.theme.clone(), a as *mut App),
        None => (1.0, theme::dark(), std::ptr::null_mut()),
    };
    if app.is_null() {
        return;
    }
    let mut entries: Vec<menu::MenuEntry> = vec![
        app::shell_row("新建窗口", Some("\u{E8A7}"), menu::Command::NewWindow),
        app::shell_row("设置", Some("\u{E713}"), menu::Command::OpenSettings),
        menu::MenuEntry::sep(),
    ];
    unsafe {
        for (i, h) in WINDOWS.iter().enumerate() {
            let mut buf = [0u16; 256];
            let n = GetWindowTextW(*h, &mut buf);
            let title = String::from_utf16_lossy(&buf[..n as usize]);
            let short = if title.chars().count() > 34 {
                format!("{}…", title.chars().take(34).collect::<String>())
            } else {
                title
            };
            let label = if IsWindowVisible(*h).0 != 0 {
                short
            } else {
                format!("{short}（已隐藏）")
            };
            entries.push(app::shell_row(&label, Some("\u{E7B3}"), menu::Command::FocusWindow(i)));
        }
    }
    entries.push(menu::MenuEntry::sep());
    entries.push(app::shell_row("退出 ZenFile", Some("\u{E7E8}"), menu::Command::QuitApp));
    let _ = menu::open_menu(app, hwnd, entries, pt.x, pt.y, scale, theme, None, false);
}

static mut MAIN_CLASS: HSTRING = HSTRING::new();

fn register_classes(instance: Option<windows::Win32::Foundation::HINSTANCE>) {
    unsafe {
        MAIN_CLASS = HSTRING::from("ZenFileMainWindow");
        let hinstance = instance.unwrap_or_default();
        println!("instance handle present: {}", hinstance.0 as usize != 0);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            // CS_DBLCLKS：不注册这个样式，系统**根本不会**投递 WM_LBUTTONDBLCLK，
            // 双击打开就永远是死代码（窗口只收到两次独立的 WM_LBUTTONDOWN）。
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wnd_proc),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
            lpszClassName: PCWSTR(MAIN_CLASS.as_ptr()),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
}

unsafe fn drop_app() {
    if !APP.is_null() {
        drop(Box::from_raw(APP));
        APP = std::ptr::null_mut();
    }
}

fn app_ref() -> &'static mut App {
    unsafe { &mut *APP }
}

/// 拿不到 App 就返回 None（窗口还没建好 / 已经销毁），不能 panic ——
/// 子控件的 WM_PAINT 会在这些时刻到达，一 panic 就是进程直接崩。
pub(crate) fn app_ref_opt() -> Option<&'static mut App> {
    let p = unsafe { APP };
    if p.is_null() {
        None
    } else {
        Some(unsafe { &mut *p })
    }
}

extern "C" {
    fn __acrt_iob_func(index: u32) -> *mut core::ffi::c_void;
    fn freopen(
        filename: *const i8,
        mode: *const i8,
        stream: *mut core::ffi::c_void,
    ) -> *mut core::ffi::c_void;
}

/// release 构建是 Windows 子系统，没有控制台，println! 会静默丢弃。
/// 这里挂回父进程控制台并把 CRT 的 stdout/stderr 重新指向 CONOUT$。
///
/// 只在 GUI 子系统构建下生效：debug 构建本身就是控制台子系统，
/// 若也走这条路，AllocConsole 会把输出从继承的句柄上抢走，反而全丢。
#[cfg(all(windows, not(debug_assertions)))]
unsafe fn attach_console() {
    use windows::Win32::System::Console::{
        AllocConsole, AttachConsole, GetConsoleWindow, ATTACH_PARENT_PROCESS,
    };
    if !GetConsoleWindow().0.is_null() {
        return;
    }
    if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
        let _ = AllocConsole();
    }
    let name = b"CONOUT$\0".as_ptr() as *const i8;
    let mode = b"w\0".as_ptr() as *const i8;
    freopen(name, mode, __acrt_iob_func(1));
    freopen(name, mode, __acrt_iob_func(2));
}

#[cfg(not(all(windows, not(debug_assertions))))]
unsafe fn attach_console() {}

// ---------------------------------------------------------------- 托盘

fn add_tray(hwnd: HWND) {
    let mut tip: [u16; 128] = [0; 128];
    for (i, c) in format!("{APP_TITLE} 资源管理器").encode_utf16().take(127).enumerate() {
        tip[i] = c;
    }
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NOTIFY_ICON_DATA_FLAGS(NIF_MESSAGE.0 | NIF_ICON.0 | NIF_TIP.0),
        uCallbackMessage: WM_TRAY,
        hIcon: unsafe { LoadIconW(None, IDI_APPLICATION).unwrap_or_default() },
        szTip: tip,
        ..Default::default()
    };
    unsafe {
        if Shell_NotifyIconW(NIM_ADD, &nid).0 == 0 {
            eprintln!("托盘图标添加失败");
        }
        nid.uFlags = NIF_ICON;
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    }
}

fn remove_tray(hwnd: HWND) {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NOTIFY_ICON_DATA_FLAGS(NIF_MESSAGE.0 | NIF_ICON.0 | NIF_TIP.0),
        uCallbackMessage: WM_TRAY,
        ..Default::default()
    };
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

// ---------------------------------------------------------------- 热键

/// §8.2：Win 键组合注册成功不代表能收到回调，因此做失败显式提示。
fn register_hotkeys(app: &App, hwnd: HWND) {
    if let Some(spec) = app.cfg.hotkey("global.show_window") {
        match hotkey::parse(spec) {
            Some(hk) => unsafe {
                if RegisterHotKey(Some(hwnd), HOTKEY_MAIN, hk.modifiers, hk.vk).is_err() {
                    eprintln!("全局热键 {spec} 注册失败（可能已被占用）");
                } else if hk.involves_win() {
                    eprintln!("提示：{spec} 含 Win 键，系统可能保留该组合，请在实际使用中验证");
                }
            },
            None => eprintln!("无法解析热键配置：{spec}"),
        }
    }
}

// ---------------------------------------------------------------- WndProc

/// §4：客户区扩展到整窗，但保留 WS_THICKFRAME，Win11 贴靠/动画仍然可用。
unsafe fn handle_nc_calc_size(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // 关键：无论 wParam 是 TRUE 还是 FALSE，rgrc[0] 都位于 lParam 所指结构的
    // 偏移 0 处；返回 0 且不修改该矩形 => 客户区 == 整个窗口矩形，边框被移除。
    // 之前对 wParam=0 走了 DefWindowProc，由系统把矩形内缩一个边框厚度，
    // 实测探针输出 "[NCCALCSIZE] wparam=0"，边框因此始终移除不掉。
    if IsZoomed(hwnd).0 != 0 {
        let params = &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS);
        let dpi = (GetDpiForWindow(hwnd) as i32).max(96);
        let pad = (GetSystemMetrics(SM_CXSIZEFRAME) + GetSystemMetrics(SM_CXPADDEDBORDER)) * dpi / 96;
        params.rgrc[0].left += pad;
        params.rgrc[0].top += pad;
        params.rgrc[0].right -= pad;
        params.rgrc[0].bottom -= pad;
    }
    LRESULT(0)
}

/// 子控件（地址栏 / 搜索框）暗色配色：文字、底色、缓存背景刷。
unsafe fn handle_ctl_color(app_ptr: *mut App, wparam: WPARAM) -> LRESULT {
    if app_ptr.is_null() {
        return LRESULT(0);
    }
    let app = &*app_ptr;
    let hdc = HDC(wparam.0 as *mut core::ffi::c_void);
    SetTextColor(hdc, app.theme.text);
    SetBkColor(hdc, app.theme.surface);
    LRESULT(edit_brush(app.theme.surface).0 as isize)
}

static mut EDIT_BRUSH: HBRUSH = HBRUSH(std::ptr::null_mut());

unsafe fn edit_brush(color: COLORREF) -> HBRUSH {
    if EDIT_BRUSH.0.is_null() {
        EDIT_BRUSH = CreateSolidBrush(color);
    }
    EDIT_BRUSH
}


unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // 框架关键消息必须先于 App 判空守卫处理：GWLP_USERDATA 是在 CreateWindowExW
    // 返回之后才设置的，而 WM_NCCALCSIZE 在创建期间就会到达。若先落入
    // DefWindowProc，客户区几何会在那一刻被系统定型，之后无论返回什么都
    // 无法再移除边框（实测客户区 1164x721 ≠ 窗口 1180x760，自绘内容整体
    // 被原生标题栏下推 30px，这就是"双标题栏"的根因）。
    match msg {
        WM_NCCALCSIZE => return handle_nc_calc_size(hwnd, wparam, lparam),
        WM_NCHITTEST => return handle_hit_test(hwnd, lparam),
        WM_ERASEBKGND => return LRESULT(1),
        _ => {}
    }

    let app_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    if app_ptr.is_null() && msg != WM_CREATE {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }

    match msg {
        // 鼠标侧键：XBUTTON1 后退 / XBUTTON2 前进（与资源管理器一致）
        WM_XBUTTONUP => {
            let button = ((wparam.0 >> 16) & 0xFFFF) as u32;
            let app = &mut *app_ptr;
            if button == 1 {
                app.go_back();
            } else if button == 2 {
                app.go_forward();
            }
            LRESULT(1)
        }
        WM_CTLCOLOREDIT => handle_ctl_color(app_ptr, wparam),
        // 后台复制/移动干完了：收错误、重列目录。**只在这条消息里 reload** ——
        // 在 worker 线程里碰 UI 状态会跨线程改 App，在松手那条消息里做又会
        // 阻塞输入（IFileOperation 是模态的）。
        crate::WM_APP_IO_DONE => {
            let app = &mut *app_ptr;
            app.drain_io();
            // 文件真的落地了：路径图标缓存要作废。
            // dir 缓存存的是 Option，一次查询失败会被记成 None 并且**再也不问** ——
            // 刚拷进来的那一瞬间 Shell 还没给它图标，于是永久变成"没图标"，
            // 只有重启软件（缓存跟着进程一起没）才恢复。
            app.icons.clear_path_icons();
            app.reload();
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        crate::WM_APP_RECYCLE => {
            let app = &mut *app_ptr;
            app.drain_recycle(wparam.0 as u64, lparam.0 != 0);
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rect = RECT::default();
            GetClientRect(hwnd, &mut rect).unwrap_or(());
            let app = &mut *app_ptr;
            paint::paint(app, hdc, rect.right, rect.bottom);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_SIZE => {
            let app = &mut *app_ptr;
            app.metrics = theme::Metrics::new(GetDpiForWindow(hwnd));
            app.sync_metrics();
            app.last_size = ((lparam.0 & 0xFFFF) as i32, (lparam.0 >> 16) as i32);
            paint::reposition(app);
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let app = &mut *app_ptr;
            app.metrics = theme::Metrics::new(GetDpiForWindow(hwnd));
            app.sync_metrics();
            paint::reposition(app);
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        // 分隔线上换 ↔ 光标。WM_SETCURSOR 不带坐标（lparam 低位是命中码），
        // 所以直接复用 WM_MOUSEMOVE 记下的悬停位 —— 反正它也是同一个来源。
        // 光标一律自己定，不再退回 DefWindowProc。
        // 之前只有"悬停在分隔线上"才 SetCursor(↔)，其余情况返回 0 让系统去套
        // 窗口类的 hCursor —— 而 SetCursor 一旦把光标改成 ↔，只要鼠标不跨到别的
        // 窗口，系统不会主动再设一次，于是拖过一次侧栏之后鼠标在软件里就**永远
        // 是拉伸图标**。现在每次 WM_SETCURSOR 都显式给出该用的那支光标。
        WM_SETCURSOR => {
            let app = &*app_ptr;
            // lparam 低位是这次命中码（HTLEFT/HTRIGHT/…），由 handle_hit_test 给出
            let hit = (lparam.0 & 0xFFFF) as u32;
            let id = match hit {
                10 | 11 => IDC_SIZEWE,   // HTLEFT / HTRIGHT
                12 | 15 => IDC_SIZENS,   // HTTOP / HTBOTTOM
                13 | 17 => IDC_SIZENWSE, // HTTOPLEFT / HTBOTTOMRIGHT
                14 | 16 => IDC_SIZENESW, // HTTOPRIGHT / HTBOTTOMLEFT
                _ => {
                    if app.drag_split.is_some() || app.hover_split.is_some() {
                        IDC_SIZEWE
                    } else if over_text_input(app, hwnd) {
                        // 地址栏/搜索框上要出 I 形光标：它们平时是画在父窗口上的
                        // 面包屑/装饰（真正的 EDIT 只在编辑态才建），没有子窗口来
                        // 替我们设光标，于是"移到输入框上光标却没变化"。
                        IDC_IBEAM
                    } else {
                        IDC_ARROW
                    }
                }
            };
            if let Ok(c) = LoadCursorW(None, id) {
                SetCursor(Some(c));
            }
            LRESULT(1)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_BUSY {
                let app = &mut *app_ptr;
                paint::tick_busy_strip(hwnd, app);
            } else if wparam.0 == TIMER_DRAIN {
                let app = &mut *app_ptr;
                app.drain();
                // 首页是虚拟视图，拿不到目录变更通知 —— 拷完一个大文件之后
                // 容量条会一直停在旧数值上（看着就是"条不准"）。这里做兜底刷新。
                if app.tick_refresh_home() {
                    let _ = InvalidateRect(Some(hwnd), None, true);
                }
                if let Some(stats) = &app.transfer {
                    if stats.lock().map(|s| s.finished).unwrap_or(true) {
                        app.transfer = None;
                        app.reload();
                    }
                }
            }
            LRESULT(0)
        }
        // U 盘插拔 / 光驱换盘：盘符集合变了，驱动器清单与当前视图都要重取。
        // 顶部窗口收到的是 WM_DEVICECHANGE 的 DBT_DEVICEARRIVAL/REMOVAL 分支，
        // 这里不区分具体事件类型 —— 重取一次的代价远低于两边列表不一致。
        WM_DEVICECHANGE => {
            let app = &mut *app_ptr;
            app.refresh_drives();
            app.reload();
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(1)
        }
        // 整个应用被切走（alt+tab / 点到别的窗口）：还挂着的菜单要立刻收掉。
        // 自绘菜单是 SW_SHOWNOACTIVATE 弹出的，压根没拿过焦点，所以永远等不到
        // 它自己的 WM_KILLFOCUS —— 只能由宿主的这一条消息统一收口。
        WM_ACTIVATEAPP => {
            if wparam.0 == 0 {
                let app = &mut *app_ptr;
                app.dismiss_menus();
            }
            LRESULT(0)
        }
        // 窗口本身失活（点到同线程别的窗口、或系统菜单弹出）同样收菜单，
        // 与 WM_ACTIVATEAPP 互为补充：前者管"应用级"，这一条管"窗口级"。
        WM_ACTIVATE => {
            if (wparam.0 & 0xFFFF) == 0 {
                let app = &mut *app_ptr;
                app.dismiss_menus();
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let source = (wparam.0 & 0xFFFF) as i32;
            let code = (wparam.0 >> 16) as i32;
            if source == 102 && code == 0x0300 {
                // EN_CHANGE：即时筛选
                let app = &mut *app_ptr;
                let text = paint::read_window_text(app.hwnd_search);
                // 先拷出开关，再取 tab 的可变借用（否则撞借用检查 E0503）
                let (sh, ss) = (app.show_hidden, app.show_system);
                let tab = app.tab_mut();
                tab.filter = text;
                tab.recompute_filter(sh, ss);
                let _ = InvalidateRect(Some(hwnd), None, true);
                // 搜索框自己也必须重画：放大镜和空态提示是画在**子 EDIT** 上的，
                // 而主窗口带了 WS_CLIPCHILDREN，父窗口重绘不再碰它那一块 ——
                // 只无效父窗口的话，输入之后提示字不会消失、清空之后也不会回来。
                let _ = InvalidateRect(Some(app.hwnd_search), None, true);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let app = &mut *app_ptr;
            let (x, y) = client_xy(hwnd, lparam);
            let l = paint::compute_layout(
                app.last_size.0,
                app.last_size.1,
                &app.metrics,
                app.cfg.ui.sidebar_visible,
                app.cfg.ui.statusbar_visible,
            );
            // 设置页排布编辑器：正在拖卡片，只更新位置并重画，别的命中一概不理。
            if app.tab().settings && app.settings_drag.is_some() {
                if let Some(d) = &mut app.settings_drag {
                    d.cur = (x, y);
                }
                let _ = InvalidateRect(Some(hwnd), None, false);
                return LRESULT(0);
            }
            // 正在拖条目 / 拉框选：更新当前点、重算投放目标并重画。
            // 必须排在别的命中之前 —— 拖拽期间不该改变选中集。
            if app.drag.is_some() {
                let ctrl_now = (wparam.0 & 0x0008) != 0 || GetKeyState(0x11) < 0;
                app.update_drag(x, y, ctrl_now);
                let target = drop_target_at(app, &l, x, y);
                app.drop_target = target;
                let _ = InvalidateRect(Some(hwnd), None, true);
                return LRESULT(0);
            }
            // 正在拖分隔线：只改宽度并重画，别的命中一概不理
            if let Some(drag) = app.drag_split.take() {
                let delta = ((x - drag.start_x) as f32 / app.metrics.scale).round() as i32;
                let want = drag.start_w + delta;
                match drag.which {
                    app::Split::Sidebar => {
                        app.cfg.ui.sidebar_width =
                            (want.max(app::SIDEBAR_W_MIN).min(app::SIDEBAR_W_MAX)) as u32;
                        app.sync_metrics();
                    }
                    app::Split::Column(i) => app.set_col_width(i.min(3), want),
                }
                app.drag_split = Some(drag);
                paint::reposition(app);
                let _ = InvalidateRect(Some(hwnd), None, true);
                return LRESULT(0);
            }
            // 分隔线悬停（只为了把光标换成 ↔）
            let hover_split = paint::split_hit(app, &l, x, y);
            let in_close = inside(&l.close_btn, x, y);
            let mut chrome = 0u8;
            if inside(&l.sidebar_toggle, x, y) {
                chrome |= app::HOVER_SIDEBAR_TOGGLE;
            }
            if inside(&l.menu_btn, x, y) {
                chrome |= app::HOVER_MENU_BTN;
            }
            // 标签栏悬停：决定了哪个标签显示关闭按钮
            let hover_tab = hit_tab(app, &l, x, y);
            // 目录栏最右侧「批量重命名」按钮的悬停。虚拟视图里不画，就也不该有悬停态
            let hover_bulk = paint::bulk_rename_available(app) && inside(&l.bulk_btn, x, y);
            // 内容区里鼠标指着的**条目**（条目下标，不是行号）。
            // 行号会随滚动/排序变，用它记 hover 会"指着 A 却高亮了 B"。
            let hover_row = if x >= l.view.left && y >= l.view.top && y < l.view.bottom {
                if app.tab().home {
                    home_entry_at(app, &l, x, y)
                } else if y >= l.view.top + header_height(app) {
                    view_row_at(app, &l, x, y, l.view.top + header_height(app))
                        .and_then(|r| app.tab().visible.get(r).copied())
                } else {
                    None
                }
            } else {
                None
            };
            // 状态栏布局切换按钮悬停
            let hover_view = if app.cfg.ui.statusbar_visible {
                match paint::status_view_hit(&l, &app.metrics, x, y) {
                    Some(fm_core::ViewMode::LargeIcons) => 1u8,
                    Some(_) => 2u8,
                    None => 0u8,
                }
            } else {
                0u8
            };
            // 设置页的悬停：顶部小标签 + 排布卡片/收纳项 + 开关行
            let (hover_setting, hover_set_tab, hover_set_card) = if app.tab().settings {
                if app.settings_drag.is_some() {
                    (app.settings_hover, app.settings_hover_tab, app.settings_hover_card)
                } else {
                    let mut ht = None;
                    let mut hc = None;
                    let mut hr = None;
                    match settings::hit(app, &l, x, y) {
                        Some(settings::Hit::Tab(i)) => ht = Some(i),
                        Some(settings::Hit::Card(i)) => {
                            hc = settings::geom(app, &l).card_ids.get(i).copied()
                        }
                        Some(settings::Hit::Chip(i)) => {
                            hc = settings::geom(app, &l).chip_ids.get(i).copied()
                        }
                        Some(settings::Hit::Row(a)) => hr = Some(a),
                        None => {}
                    }
                    (hr, ht, hc)
                }
            } else {
                (None, None, None)
            };
            // 工具栏「后退/前进/向上」三个导航按钮的悬停。
            // 它们本质是按钮，不给底色就只像三个浮着的图标。
            let hover_nav = if inside(&l.back, x, y) {
                1u8
            } else if inside(&l.forward, x, y) {
                2u8
            } else if inside(&l.up, x, y) {
                3u8
            } else {
                0u8
            };
            // 表头列的悬停：悬停时才在最右端露出筛选下拉的箭头
            let hover_header = if app.tab().recycle {
                paint::recycle_header_hit(app, &l, x, y)
            } else {
                paint::header_col_hit(app, &l, x, y)
            };
            // 导航窗格悬停行
            let hover_sidebar = if app.cfg.ui.sidebar_visible {
                paint::sidebar_hit(app, &l, x, y, app.metrics.scale).map(|(i, _)| i)
            } else {
                None
            };
            // 回收站：条目悬停 + 顶部三个按钮的悬停
            let (hover_rec, hover_rec_btn) = if app.tab().recycle {
                (
                    if x >= l.view.left && x < l.view.right && y >= l.view.top {
                        paint::recycle_hit(app, &l, y)
                    } else {
                        None
                    },
                    paint::recycle_button_hit(app, &l, x, y),
                )
            } else {
                (None, 0u8)
            };
            // 地址栏面包屑悬停：只在非编辑态、且光标确实落在地址框里时算
            let hover_addr = if !app.addr_editing && inside(&l.address, x, y) {
                let bar = paint::address_bar(app, &l);
                paint::address_hit(&bar, x, y)
            } else {
                None
            };
            if in_close != app.hovering_close
                || chrome != app.hover_chrome
                || hover_tab != app.hover_tab
                || hover_view != app.hover_view_btn
                || hover_setting != app.settings_hover
                || hover_sidebar != app.sidebar_hover
                || hover_addr != app.addr_hover
                || hover_split != app.hover_split
                || hover_bulk != app.hover_bulk
                || hover_row != app.hover_row
                || hover_nav != app.hover_nav
                || hover_header != app.hover_header
                || hover_rec != app.hover_recycle
                || hover_rec_btn != app.hover_recycle_btn
                || hover_set_tab != app.settings_hover_tab
                || hover_set_card != app.settings_hover_card
            {
                app.settings_hover_tab = hover_set_tab;
                app.settings_hover_card = hover_set_card;
                app.hover_nav = hover_nav;
                app.hover_header = hover_header;
                app.hover_recycle = hover_rec;
                app.hover_recycle_btn = hover_rec_btn;
                app.hover_bulk = hover_bulk;
                app.hover_row = hover_row;
                app.hover_split = hover_split;
                app.hovering_close = in_close;
                app.hover_chrome = chrome;
                app.hover_tab = hover_tab;
                app.hover_view_btn = hover_view;
                app.settings_hover = hover_setting;
                app.sidebar_hover = hover_sidebar;
                app.addr_hover = hover_addr;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        // —— 标题栏三大金刚键 ——
        // 客户区已经扩展到整窗，系统的非客户区默认处理不再可靠（实测三个键点了没
        // 反应）。这里自己接管：按下记状态、抬起执行、双击最大化等同按下两次。
        WM_NCMOUSEMOVE => {
            let app = &mut *app_ptr;
            let (x, y) = screen_xy(hwnd, lparam);
            let l = paint::compute_layout(
                app.last_size.0,
                app.last_size.1,
                &app.metrics,
                app.cfg.ui.sidebar_visible,
                app.cfg.ui.statusbar_visible,
            );
            let h = caption_hit(&l, x, y);
            if h != app.hover_caption {
                app.hover_caption = h;
                app.hovering_close = h == 3;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_NCLBUTTONDOWN => {
            let app = &mut *app_ptr;
            let (x, y) = screen_xy(hwnd, lparam);
            let l = paint::compute_layout(
                app.last_size.0,
                app.last_size.1,
                &app.metrics,
                app.cfg.ui.sidebar_visible,
                app.cfg.ui.statusbar_visible,
            );
            let hit = caption_hit(&l, x, y);
            // 没落在三大金刚键上就是 HTCAPTION（标题栏空白处）。**必须交回
            // DefWindowProc**：拖窗口的模态循环是系统在这条消息里启动的，我们
            // 自己 return 0 就等于把循环吞掉 —— 表现是"标题栏拖不动窗口"。
            if hit == 0 {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            app.pressed_caption = hit;
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_NCLBUTTONUP => {
            let app = &mut *app_ptr;
            let (x, y) = screen_xy(hwnd, lparam);
            let l = paint::compute_layout(
                app.last_size.0,
                app.last_size.1,
                &app.metrics,
                app.cfg.ui.sidebar_visible,
                app.cfg.ui.statusbar_visible,
            );
            let hit = caption_hit(&l, x, y);
            // 按下和抬起必须在同一个键上，否则拖出去再松手也会触发动作
            let which = if hit != 0 && hit == app.pressed_caption { hit } else { 0 };
            app.pressed_caption = 0;
            if which != 0 {
                run_caption(hwnd, which);
                return LRESULT(0);
            }
            // 抬在标题栏空白处：同样要交回系统，让它结束这一次拖拽循环
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_NCLBUTTONDBLCLK => {
            let app = &mut *app_ptr;
            let (x, y) = screen_xy(hwnd, lparam);
            let l = paint::compute_layout(
                app.last_size.0,
                app.last_size.1,
                &app.metrics,
                app.cfg.ui.sidebar_visible,
                app.cfg.ui.statusbar_visible,
            );
            let hit = caption_hit(&l, x, y);
            if hit != 0 {
                run_caption(hwnd, hit);
                return LRESULT(0);
            }
            // 双击标题栏空白处 = 最大化/还原。这也是系统在这条消息里做的，
            // 我们吞掉它的后果就是"双击最大化之后再也缩不回去"。
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            let app = &mut *app_ptr;
            let (x, y) = client_xy(hwnd, lparam);
            let l = paint::compute_layout(
                app.last_size.0,
                app.last_size.1,
                &app.metrics,
                app.cfg.ui.sidebar_visible,
                app.cfg.ui.statusbar_visible,
            );
            // 重命名编辑框还开着时，点任何地方都是"先收掉它"。
            // 我们的 WM_LBUTTONDOWN 走完就 return，从不交给 DefWindowProc，
            // 于是焦点压根不会从 EDIT 上挪开 —— KILLFOCUS 永远不来，框就一直
            // 挂在列表上（用户看到的"点空白处/回车也关不掉"）。这一下点击
            // 只做收尾，不再往下派发，和资源管理器一致。
            if app.rename_hwnd.is_some() {
                app.commit_rename_from_editor();
                app.invalidate();
                return LRESULT(0);
            }
            // 设置页排布编辑器：按下卡片/收纳项 → 开始拖
            if app.tab().settings {
                if let Some(h) = settings::hit(app, &l, x, y) {
                    if settings::begin_drag(app, &l, &h, x, y) {
                        let _ = SetCapture(hwnd);
                        let _ = InvalidateRect(Some(hwnd), None, true);
                        return LRESULT(0);
                    }
                }
            }
            // 分隔线（导航窗格宽度 / 表头列宽）：排在虚拟视图分支之前，
            // 否则打开设置页之后整条消息在设置分支里就 return 了。
            if let Some(which) = paint::split_hit(app, &l, x, y) {
                let start_w = match which {
                    app::Split::Sidebar => app.cfg.ui.sidebar_width as i32,
                    app::Split::Column(i) => app.col_widths()[i.min(3)],
                };
                app.drag_split = Some(app::SplitDrag { which, start_x: x, start_w });
                SetCapture(hwnd);
                return LRESULT(0);
            }
            // 标签栏的命中**必须排在设置页/回收站之前**：那两个分支处理完就
            // 直接 return 了，放在前面的话一打开设置整个标题栏就点不动
            // （表现：打开设置后切不了标签、也关不掉标签）。
            if let Some(i) = hit_tab(app, &l, x, y) {
                // 关闭按钮压在标签右上角，命中优先级必须高于"切换标签"，
                // 否则每次点 × 都会先切一次标签（然后才关掉别的）。
                if inside(&l.tab_close_rect(i, app.tabs.len(), &app.metrics), x, y) {
                    app.close_tab(i);
                } else {
                    app.active = i;
                }
                paint::sync_address(app);
                let _ = InvalidateRect(Some(hwnd), None, true);
                return LRESULT(0);
            }
            // 导航窗格的命中**必须排在设置页/回收站之前**：那两个分支处理完就
            // 直接 return，于是"进了设置/回收站就没办法对侧栏操作"（点盘符
            // 想换个目录，点了没反应，只能先切回去）。
            if app.cfg.ui.sidebar_visible && x < l.sidebar.right && y > l.sidebar.top {
                if sidebar_click(app, x, y) {
                    // 必须重画：点箭头只改树的状态，不像 navigate 那样自己会触发
                    // 重绘。少了这一句，展开/收起看起来就是"点了没反应"。
                    let _ = InvalidateRect(Some(hwnd), None, true);
                    return LRESULT(0);
                }
            }
            // 设置页：顶部小标签切换；开关点一下就切；多选项点一下展开下拉
            if app.tab().settings {
                match settings::hit(app, &l, x, y) {
                    Some(settings::Hit::Tab(i)) => {
                        app.settings_tab = i;
                        app.settings_hover_card = None;
                        let _ = InvalidateRect(Some(hwnd), None, true);
                    }
                    Some(settings::Hit::Row(action)) if action >= 1000 => {
                        // 插件行：动作编号 1000 + 模块下标
                        let idx = (action - 1000) as usize;
                        let key = app.cfg.modules.keys().nth(idx).cloned();
                        if let Some(k) = key {
                            let on = app.cfg.modules.get(&k).map(|m| m.enabled).unwrap_or(false);
                            app.set_module_enabled(&k, !on);
                        }
                    }
                    Some(settings::Hit::Row(action)) => {
                        if matches!(action, paint::SET_THEME | paint::SET_UNIT | paint::SET_VIEW) {
                            show_setting_dropdown(hwnd, app, &l, action);
                        } else if let Some(cmd) = settings_command(action) {
                            app.run_command(cmd);
                        }
                    }
                    _ => {}
                }
                return LRESULT(0);
            }
            // 回收站视图：**先判列头**（它在顶部工具条下面、条目区上面），
            // 再判三个按钮，最后才是条目。
            //
            // 列头判定曾经写在下面的"表头"那一节里，而回收站的这一整段是
            // `return LRESULT(0)` 收尾的 —— 那段代码永远到不了，于是"回收站
            // 用表头排序"是个死的。点列头反而落进了"空白处 → 清选中 + 拉框选"。
            if app.tab().recycle {
                if let Some(col) = paint::recycle_header_hit(app, &l, x, y) {
                    app.sort_recycle_by(col);
                    let _ = InvalidateRect(Some(hwnd), None, true);
                    return LRESULT(0);
                }
                let btns = paint::recycle_buttons(app, &l);
                if inside(&btns[0], x, y) {
                    app.run_command(menu::Command::RecycleRestore);
                } else if inside(&btns[1], x, y) {
                    app.run_command(menu::Command::RecycleDelete);
                } else if inside(&btns[2], x, y) {
                    app.run_command(menu::Command::RecycleEmpty);
                } else if let Some(idx) = paint::recycle_hit(app, &l, y) {
                    let id = app.recycle_items.get(idx).map(|i| i.id.clone());
                    let ctrl = GetKeyState(0x11) < 0;
                    if let Some(id) = id {
                        if ctrl {
                            if !app.recycle_sel.insert(id.clone()) {
                                app.recycle_sel.remove(&id);
                            }
                        } else {
                            app.recycle_sel.clear();
                            app.recycle_sel.insert(id);
                        }
                    }
                } else {
                    app.recycle_sel.clear();
                    // 空白处起手 = 拉框选。回收站之前也是直接 return，
                    // 于是"长按框选在回收站不生效"。
                    app.begin_drag(x, y, app::DragKind::Marquee, false);
                    SetCapture(hwnd);
                }
                let _ = InvalidateRect(Some(hwnd), None, true);
                return LRESULT(0);
            }
            let ctrl = GetKeyState(0x11) < 0;
            let shift = GetKeyState(0x10) < 0;
            // 状态栏右下角：资源管理器同款的"大图标 / 详细信息"快速切换
            if app.cfg.ui.statusbar_visible {
                if let Some(mode) = paint::status_view_hit(&l, &app.metrics, x, y) {
                    app.set_view(mode);
                    return LRESULT(0);
                }
            }
            // 地址栏（Win11 面包屑）：点某一节跳到那一层，点空白进编辑态
            if inside(&l.address, x, y) {
                if app.addr_editing {
                    return LRESULT(0);
                }
                let bar = paint::address_bar(app, &l);
                match paint::address_hit(&bar, x, y) {
                    Some(i) => {
                        // 必须用"命中的那一节自己带的 target"，不能拿下标回完整
                        // 路径列表里取：窗口一窄，中间层级会被折成一个 `…`，
                        // 折叠后的下标和完整列表的下标根本不是一回事（实测点 `…`
                        // 会跳到 C:\ —— 那是完整列表的第 1 节）。
                        let target = bar.crumbs[i].target.clone();
                        app.navigate(target.unwrap_or_else(|| PathBuf::from(app::HOME_PATH)));
                    }
                    None => paint::begin_address_edit(app),
                }
                return LRESULT(0);
            }
            if inside(&l.back, x, y) {
                app.go_back();
                paint::sync_address(app);
                return LRESULT(0);
            }
            if inside(&l.forward, x, y) {
                app.go_forward();
                paint::sync_address(app);
                return LRESULT(0);
            }
            if inside(&l.up, x, y) {
                app.go_up();
                paint::sync_address(app);
                return LRESULT(0);
            }
            // 目录栏最右侧「批量重命名 ▾」：下拉形式弹出五种模式
            if paint::bulk_rename_available(app) && inside(&l.bulk_btn, x, y) {
                show_bulk_rename_menu(hwnd, app, &l);
                return LRESULT(0);
            }
            // 首页是平铺布局，行列式命中在这里不适用
            if app.tab().home && y >= l.view.top {
                let hit = home_select(app, &l, x, y);
                // 空白处起手同样要能拉框选 —— 首页之前直接 return，于是"长按框选
                // 在首页不生效"。磁贴布局不是行列式的，框选命中走 home_layout
                // 的磁贴矩形（见 select_marquee）。
                if !hit && !ctrl && !shift {
                    app.tab_mut().selected.clear();
                }
                app.begin_drag(x, y, app::DragKind::Marquee, ctrl);
                SetCapture(hwnd);
                let _ = InvalidateRect(Some(hwnd), None, true);
                return LRESULT(0);
            }
            let header_h = header_height(app);
            let view_top = l.view.top + header_h;
            if y >= l.view.top && y < view_top && app.tab().view_mode == fm_core::ViewMode::Details {
                // 先判列最右端那个「筛选」小三角：它压在列里，先判整列的话
                // 点三角永远只会变成"再排一次序"。
                if let Some(col) = paint::header_col_hit(app, &l, x, y) {
                    if inside(&paint::header_filter_rect(app, &l, col), x, y) {
                        show_header_filter_menu(hwnd, app, &l, col);
                        return LRESULT(0);
                    }
                }
                header_click(app, &l, x);
                return LRESULT(0);
            }
            if let Some(row) = view_row_at(app, &l, x, y, view_top) {
                // 「再点一下已经选中的名字 = 改名」。
                // 判据是**间隔**：连着两下（小于双击时限）是打开，隔一会儿再点
                // 才是改名。不判间隔的话双击打开就永远变成改名了。
                let now = unsafe { windows::Win32::System::SystemInformation::GetTickCount64() };
                let entry_idx = app.tab().visible.get(row).copied();
                let sole_selected = entry_idx
                    .map(|i| app.tab().selected.len() == 1 && app.tab().selected.contains(&crate::app::key_of(&app.tab().entries[i].clone())))
                    .unwrap_or(false);
                let mut renamed = false;
                if !ctrl && !shift && sole_selected {
                    if let (Some(i), Some((prev_i, prev_t))) = (entry_idx, app.last_click) {
                        if prev_i == i && now.saturating_sub(prev_t) > DOUBLE_CLICK_MS {
                            app.begin_rename();
                            app.last_click = None;
                            renamed = true;
                        }
                    }
                    if !renamed {
                        app.last_click = entry_idx.map(|i| (i, now));
                    }
                } else {
                    app.last_click = None;
                }
                if renamed {
                    let _ = InvalidateRect(Some(hwnd), None, true);
                    return LRESULT(0);
                }
                app.toggle_select(row, ctrl, shift);
                // 左键拖拽的起点：按下先记账，超过阈值才真的开始（见 WM_MOUSEMOVE）
                app.begin_drag(x, y, app::DragKind::Items, ctrl);
                SetCapture(hwnd);
                let _ = InvalidateRect(Some(hwnd), None, true);
            } else if y >= view_top {
                // 点空白处：清空选中，并且**把焦点框也收起来**。
                // 只清 selected 的话，焦点行还留在原地，于是"先前选中的那一项还在，
                // 只是淡了一点" —— 用户以为没清掉。
                if !ctrl && !shift {
                    app.tab_mut().selected.clear();
                }
                app.focus_visible = false;
                // 空白处起手 = 拉框选（资源管理器同款）
                app.begin_drag(x, y, app::DragKind::Marquee, ctrl);
                SetCapture(hwnd);
                let _ = InvalidateRect(Some(hwnd), None, true);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = client_xy(hwnd, lparam);
            // 设置页排布编辑器：松手落地（收进「更多」/ 插到新位置 / 拖回恢复）
            {
                let app = &mut *app_ptr;
                if app.settings_drag.is_some() {
                    let _ = ReleaseCapture();
                    let l = paint::compute_layout(
                        app.last_size.0,
                        app.last_size.1,
                        &app.metrics,
                        app.cfg.ui.sidebar_visible,
                        app.cfg.ui.statusbar_visible,
                    );
                    settings::finish_drag(app, &l, x, y);
                    let _ = InvalidateRect(Some(hwnd), None, true);
                    return LRESULT(0);
                }
            }
            // 拖完条目 / 拉完框：落地
            {
                let app = &mut *app_ptr;
                if app.drag.is_some() {
                    let _ = ReleaseCapture();
                    let l = paint::compute_layout(
                        app.last_size.0,
                        app.last_size.1,
                        &app.metrics,
                        app.cfg.ui.sidebar_visible,
                        app.cfg.ui.statusbar_visible,
                    );
                    finish_drag(app, &l, x, y);
                    let _ = InvalidateRect(Some(hwnd), None, true);
                    return LRESULT(0);
                }
            }
            // 拖完分隔线：松开才落盘（拖动过程中每帧写一次 JSON 太亏）
            {
                let app = &mut *app_ptr;
                if let Some(drag) = app.drag_split.take() {
                    let _ = ReleaseCapture();
                    match drag.which {
                        app::Split::Sidebar => app.persist_sidebar_width(),
                        app::Split::Column(_) => app.persist_view_pref(),
                    }
                    paint::reposition(app);
                    let _ = InvalidateRect(Some(hwnd), None, true);
                    return LRESULT(0);
                }
            }
            let (l, plus) = {
                let app = &*app_ptr;
                let l = paint::compute_layout(
                    app.last_size.0,
                    app.last_size.1,
                    &app.metrics,
                    app.cfg.ui.sidebar_visible,
                    app.cfg.ui.statusbar_visible,
                );
                let plus = l.new_tab_rect(app.tabs.len(), &app.metrics);
                (l, plus)
            };
            if y < l.title.bottom {
                if plus.right < l.tabs.right && inside(&plus, x, y) {
                    let app = &mut *app_ptr;
                    app.new_tab();
                    paint::sync_address(app);
                    return LRESULT(0);
                }
                if inside(&l.sidebar_toggle, x, y) {
                    // 标题栏最左侧：折叠/展开导航窗格
                    let app = &mut *app_ptr;
                    app.toggle_sidebar();
                    paint::sync_address(app);
                    return LRESULT(0);
                }
                if inside(&l.menu_btn, x, y) {
                    // 最小化左侧：更多菜单
                    let app = &mut *app_ptr;
                    let mut pt = POINT { x: l.menu_btn.left, y: l.menu_btn.bottom };
                    let _ = ClientToScreen(hwnd, &mut pt);
                    app.show_more_menu(pt.x, pt.y);
                    return LRESULT(0);
                }
            }
            LRESULT(0)
        }
        WM_MBUTTONDOWN => {
            // 鼠标中键：点标签 = 关闭该标签（资源管理器/浏览器的通用约定）
            let app = &mut *app_ptr;
            let (x, y) = client_xy(hwnd, lparam);
            let l = paint::compute_layout(
                app.last_size.0,
                app.last_size.1,
                &app.metrics,
                app.cfg.ui.sidebar_visible,
                app.cfg.ui.statusbar_visible,
            );
            if let Some(i) = hit_tab(app, &l, x, y) {
                app.close_tab(i);
                app.hover_tab = None;
                paint::sync_address(app);
                let _ = InvalidateRect(Some(hwnd), None, true);
            }
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            let app = &mut *app_ptr;
            let (x, y) = client_xy(hwnd, lparam);
            let l = paint::compute_layout(
                app.last_size.0,
                app.last_size.1,
                &app.metrics,
                app.cfg.ui.sidebar_visible,
                app.cfg.ui.statusbar_visible,
            );
            if y < l.title.bottom {
                // 双击标签栏空白处 = 新建标签（同资源管理器）；
                // 双击标签本身 = 最大化/还原。绝不能在这里关标签 —— 双击是"打开"手势，
                // 用它来关闭会让人莫名其妙丢标签。
                let tab_w = (paint::TAB_W as f32 * app.metrics.scale) as i32;
                let i = ((x - l.tabs.left) / tab_w.max(1)) as usize;
                if i >= app.tabs.len() {
                    app.new_tab();
                } else {
                    let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                        hwnd,
                        if IsZoomed(hwnd).0 != 0 {
                            windows::Win32::UI::WindowsAndMessaging::SW_RESTORE
                        } else {
                            windows::Win32::UI::WindowsAndMessaging::SW_MAXIMIZE
                        },
                    );
                }
                return LRESULT(0);
            }
            // 导航窗格：双击 = 展开/收起（单击只导航，见 sidebar_click）。
            // 必须在进内容区判定之前拦下来：导航窗格压在窗口左侧，
            // 不拦就会落到"双击打开条目"那条路上去。
            if app.cfg.ui.sidebar_visible && x < l.sidebar.right && y > l.sidebar.top {
                if sidebar_dblclick(app, x, y) {
                    let _ = InvalidateRect(Some(hwnd), None, true);
                }
                return LRESULT(0);
            }
            // 设置页/回收站没有"双击打开"的语义（回收站里的项双击 = 还原更符合直觉，
            // 但误触代价太高，这里只做选中，还原走顶部按钮）
            if app.tab().settings || app.tab().recycle {
                return LRESULT(0);
            }
            // 首页：双击磁贴进入对应文件夹
            if app.tab().home {
                if let Some(idx) = home_entry_at(app, &l, x, y) {
                    let path = app.tab().entries.get(idx).map(|e| e.path.clone());
                    if let Some(p) = path {
                        app.navigate(p);
                        paint::sync_address(app); // 否则地址栏还停在"主页"
                    }
                }
                return LRESULT(0);
            }
            let header_h = header_height(app);
            if let Some(row) = view_row_at(app, &l, x, y, l.view.top + header_h) {
                app.set_focus_row(row);
                let tab = app.tab_mut();
                tab.selected.clear();
                if let Some(i) = tab.visible.get(row).copied() {
                    if let Some(e) = tab.entries.get(i) {
                        tab.selected.insert(crate::app::key_of(e));
                    }
                }
                // Alt+双击 = 属性（资源管理器同款）。Alt 不按就是打开。
                // 放在选中之后：属性窗口要作用在刚选中的那一项上。
                if GetKeyState(0x12) < 0 {
                    app.context_targets = Some(app.tab().selected_paths());
                    app.run_command(menu::Command::Properties);
                    app.context_targets = None;
                    return LRESULT(0);
                }
                app.open_focused();
                paint::sync_address(app);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            let app = &mut *app_ptr;
            // 同上：开着重命名框时右键也先把它收掉（焦点同样不会自己挪）。
            if app.rename_hwnd.is_some() {
                app.commit_rename_from_editor();
                app.invalidate();
                return LRESULT(0);
            }
            let (x, y) = client_xy(hwnd, lparam);
            let l = paint::compute_layout(
                app.last_size.0,
                app.last_size.1,
                &app.metrics,
                app.cfg.ui.sidebar_visible,
                app.cfg.ui.statusbar_visible,
            );
            let header_h = header_height(app);
            let view_top = l.view.top + header_h;

            if app.tab().settings {
                // 设置页没有文件可选，右键不弹菜单
                app.context_targets = Some(Vec::new());
                LRESULT(0)
            } else if app.tab().recycle {
                if let Some(idx) = paint::recycle_hit(app, &l, y) {
                    let id = app.recycle_items.get(idx).map(|i| i.id.clone());
                    if let Some(id) = id {
                        if !app.recycle_sel.contains(&id) {
                            app.recycle_sel.clear();
                            app.recycle_sel.insert(id);
                        }
                    }
                } else {
                    app.recycle_sel.clear();
                }
                let _ = InvalidateRect(Some(hwnd), None, true);
                let mut pt = POINT { x, y };
                let _ = ClientToScreen(hwnd, &mut pt);
                app.show_recycle_menu(pt.x, pt.y);
                LRESULT(0)
            } else {
                // 表头右键 = 排序/筛选菜单，不是文件右键。放在取 target 之前
                // 拦下来，否则它会落进"清空选中 + 弹目录菜单"那条路。
                if app.tab().view_mode == fm_core::ViewMode::Details
                    && y >= l.view.top
                    && y < view_top
                    && x >= l.view.left
                    && x < l.view.right
                {
                    if let Some(col) = paint::header_col_hit(app, &l, x, y) {
                        show_header_context_menu(hwnd, app, &l, col, x, y);
                        return LRESULT(0);
                    }
                }
                // 先把"这一次要作用在哪些条目上"算出来存进 App，
                // 菜单里绝不能再调 selected_paths() —— 它在选中为空时会退回焦点行，
                // 那正是"空白处右键却判定到条目上"的根因。
                let targets: Vec<PathBuf> = if app.tab().home {
                    if y >= l.view.top && x >= l.view.left && x < l.view.right {
                        if home_select(app, &l, x, y) {
                            app.tab().selected_paths()
                        } else {
                            // 主页空白处：必须清掉选中再给空目标。之前不管命中
                            // 与否都取 selected_paths()，于是"在主页空白处右键"
                            // 弹出来的是上一次选中那个文件的菜单 —— 这就是
                            // 主页右键判定飘忽的真因。
                            app.tab_mut().selected.clear();
                            app.focus_visible = false;
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    }
                } else if x >= l.view.left && y >= view_top && y < l.view.bottom {
                    match view_row_at(app, &l, x, y, view_top) {
                        Some(row) => {
                            app.context_select_row(row);
                            app.tab().selected_paths()
                        }
                        None => {
                            app.tab_mut().selected.clear();
                            app.focus_visible = false;
                            Vec::new()
                        }
                    }
                } else {
                    Vec::new()
                };
                app.context_targets = Some(targets);
                let _ = InvalidateRect(Some(hwnd), None, true);
                let mut pt = POINT { x, y };
                let _ = ClientToScreen(hwnd, &mut pt);
                let extended = GetKeyState(0x10) < 0; // Shift+右键 → Explorer 原生菜单 §7
                app.show_context_menu(pt.x, pt.y, extended);
                LRESULT(0)
            }
        }
        WM_MOUSEWHEEL => {
            let app = &mut *app_ptr;
            let delta = (wparam.0 >> 16) as i16 as i32;
            let l = paint::compute_layout(
                app.last_size.0,
                app.last_size.1,
                &app.metrics,
                app.cfg.ui.sidebar_visible,
                app.cfg.ui.statusbar_visible,
            );
            // 光标在导航窗格里时滚轮滚的是**树**，不是右边的文件列表 ——
            // 否则展开几层之后树就再也滚不出来了。
            let mut pt = windows::Win32::Foundation::POINT {
                x: (lparam.0 & 0xFFFF) as i16 as i32,
                y: (lparam.0 >> 16) as i16 as i32,
            };
            unsafe { let _ = ScreenToClient(hwnd, &mut pt); }
            if app.cfg.ui.sidebar_visible && pt.x < l.sidebar.right {
                app.scroll_sidebar(delta / 2, &l);
                return LRESULT(0);
            }
            {
                let tab = app.tab_mut();
                tab.scroll -= delta / 2;
            }
            // 钳制必须知道布局：网格视图的内容高度是"行数 × 行高"，行数取决于列数
            app.clamp_scroll_to(&l);
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let app = &mut *app_ptr;
            // 地址框/搜索框正在编辑时，按键属于那个 EDIT（回车、ESC 由它自己的
            // 子类过程处理）。主窗口再插一脚会把字母当快捷键、还会顺手把地址栏
            // 文本刷回当前路径 —— 输入到一半的内容就这么没了。
            if app.addr_editing {
                return LRESULT(0);
            }
            handle_key(app, wparam.0 as i32);
            paint::sync_address(app);
            LRESULT(0)
        }
        WM_HOTKEY => {
            let hidden = unsafe { IsWindowVisible(hwnd).0 == 0 };
            let _ = if hidden {
                ShowWindow(hwnd, SW_SHOW)
            } else {
                ShowWindow(hwnd, SW_HIDE)
            };
            if hidden {
                let _ = SetForegroundWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let app = &mut *app_ptr;
            if app.cfg.core.exit_to_tray {
                let _ = ShowWindow(hwnd, SW_HIDE);
                return LRESULT(0);
            }
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        // 托盘消息不再走这里（见 `tray_proc`）：托盘挂在隐藏的托盘宿主窗口上，
        // 于是"关掉第一个窗口"不会把托盘图标一起带走。
        WM_DESTROY => {
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_DRAIN);
                let _ = KillTimer(Some(hwnd), TIMER_BUSY);
                if std::ptr::eq(app_ptr, APP) {
                    let _ = UnregisterHotKey(Some(hwnd), HOTKEY_MAIN);
                }
                WINDOWS.retain(|h| *h != hwnd);
                if !app_ptr.is_null() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    let mut boxed = Box::from_raw(app_ptr);
                    // 后台线程必须在这里收掉：它们 PostMessage 回这个 hwnd，
                    // 窗口没了之后每帧一次的 drain 也跟着没了，线程会一直挂着。
                    if let Some(w) = boxed.watcher.take() {
                        w.stop();
                    }
                    if let Some(c) = boxed.index_cancel.take() {
                        c.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    drop(boxed);
                    if std::ptr::eq(app_ptr, APP) {
                        APP = std::ptr::null_mut();
                    }
                }
                if WINDOWS.is_empty() {
                    // 最后一个窗口关了：进程收工。托盘宿主窗口一起拆掉，
                    // 否则系统托盘里会留下一个点不动的残影。
                    if !TRAY_HWND.0.is_null() {
                        let _ = DestroyWindow(TRAY_HWND);
                        TRAY_HWND = HWND(std::ptr::null_mut());
                    }
                    PostQuitMessage(0);
                }
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 列表命中：大图标/中图标走**网格**几何，其余走行几何。
///
/// 三个调用点（单击 / 双击 / 右键）必须共用这一个函数。之前大图标视图仍然按
/// "一行一条"算，画出来是网格、点进去是行，位置完全对不上。
fn view_row_at(app: &App, l: &paint::Layout, x: i32, y: i32, view_top: i32) -> Option<usize> {
    let tab = app.tab();
    let mode = tab.view_mode;
    if matches!(mode, fm_core::ViewMode::LargeIcons | fm_core::ViewMode::MediumIcons) {
        let g = paint::icon_grid(l, &app.metrics, mode);
        paint::grid_hit(&g, l, x, y, view_top, tab.scroll, tab.visible.len())
    } else {
        app.row_hit(y, view_top, l.view.bottom - view_top)
    }
}

/// 拖拽落点判定：面包屑 → 导航窗格 → 内容区里的文件夹行。
///
/// 顺序有意义：地址栏压在工具栏里、导航窗格又压着内容区左边缘，
/// 从最具体的开始判才不会"想拖到面包屑结果落进了下面的行"。
fn drop_target_at(app: &mut App, l: &paint::Layout, x: i32, y: i32) -> Option<app::DropTarget> {
    // 1) 地址栏面包屑（第 12 轮第 12 项：拖到哪一级就进哪一级）
    if inside(&l.address, x, y) && !app.addr_editing {
        let bar = paint::address_bar(app, l);
        if let Some(i) = paint::address_hit(&bar, x, y) {
            if let Some(p) = bar.crumbs.get(i).and_then(|c| c.target.clone()) {
                return Some(app::DropTarget { path: p, kind: app::DropKind::Crumb(i) });
            }
        }
    }
    // 2) 导航窗格（回收站/主页是虚拟目标，不能当落地目录）
    if app.cfg.ui.sidebar_visible && y >= l.sidebar.top && x < l.sidebar.right {
        if let Some((i, _)) = paint::sidebar_hit(app, l, x, y, app.metrics.scale) {
            let rows = app.sidebar_rows();
            if let Some(row) = rows.get(i) {
                let t = &row.target;
                if !t.is_empty()
                    && !paint::is_home_target(t)
                    && !paint::is_recycle_target(t)
                    && t != app::SETTINGS_PATH
                {
                    return Some(app::DropTarget {
                        path: PathBuf::from(t),
                        kind: app::DropKind::Sidebar(i),
                    });
                }
            }
        }
    }
    // 3) 内容区里的文件夹行
    let view_top = l.view.top + header_height(app);
    if y >= view_top && y < l.view.bottom {
        if let Some(row) = view_row_at(app, l, x, y, view_top) {
            let idx = app.tab().visible.get(row).copied();
            let entry = idx.and_then(|i| app.tab().entries.get(i).cloned());
            if let Some(e) = entry {
                if e.is_dir() {
                    return Some(app::DropTarget { path: e.path.clone(), kind: app::DropKind::Row(row) });
                }
            }
        }
    }
    None
}

/// 目录栏最右侧「批量重命名 ▾」的下拉：五种模式，各对应 `fm_core::bulk` 一条规则。
///
/// 菜单定位在按钮**正下方**左对齐 —— 下拉菜单的通行摆法，别从鼠标位置弹。
fn show_bulk_rename_menu(hwnd: HWND, app: &mut App, l: &paint::Layout) {
    let entries: Vec<menu::MenuEntry> = crate::rename::MODE_LABELS
        .iter()
        .enumerate()
        .map(|(i, label)| {
            crate::app::shell_row(label, Some("\u{E8AC}"), menu::Command::BulkRename(i))
        })
        .collect();
    let mut pt = POINT { x: l.bulk_btn.left, y: l.bulk_btn.bottom };
    let _ = unsafe { ClientToScreen(hwnd, &mut pt) };
    let app_ptr = app as *mut App;
    if let Some(h) = menu::open_menu(
        app_ptr,
        hwnd,
        entries,
        pt.x,
        pt.y,
        app.metrics.scale,
        app.theme,
        None,
        true, // 由一次左键按下弹出：那一次的抬起要吞掉
    ) {
        app.menu_hwnd = Some(h);
    }
}

/// 松手：条目拖拽去落地 / 框选去选中。
fn finish_drag(app: &mut App, l: &paint::Layout, x: i32, y: i32) {
    let Some(drag) = app.drag.take() else {
        return;
    };
    app.drop_target = None;
    if !drag.active {
        return;
    }
    match drag.kind {
        app::DragKind::Marquee => select_marquee(app, l, &drag),
        app::DragKind::Items => {
            if let Some(t) = drop_target_at(app, l, x, y) {
                let copy = drag.copy || app.drag_copy();
                app.drop_items(t.path.clone(), drag.items.clone(), copy);
            }
        }
    }
}

/// 框选：矩形与条目矩形相交的都选中。
///
/// 三种布局的"条目矩形"算法完全不同（列表行 / 网格磁贴 / 首页磁贴），
/// 回收站更是另一套（按下标 + id 选中）。四个分支各自用自己的几何，
/// 绝不拿列表那套去套首页 —— 那正是"框选在首页/回收站不生效"的根因。
fn select_marquee(app: &mut App, l: &paint::Layout, drag: &app::Drag) {
    let r = paint::marquee_rect(drag);
    let hits = |a: &RECT, b: &RECT| a.right > b.left && a.left < b.right && a.bottom > b.top && a.top < b.bottom;

    if app.tab().recycle {
        let row_h = paint::recycle_row_h(app.metrics.scale);
        let top = paint::recycle_list_top(app.metrics.scale, l.view.top);
        let mut ids: Vec<String> = Vec::new();
        for (i, it) in app.recycle_items.iter().enumerate() {
            let ir = RECT { left: l.view.left, top: top + i as i32 * row_h, right: l.view.right, bottom: top + (i as i32 + 1) * row_h };
            if hits(&ir, &r) {
                ids.push(it.id.clone());
            }
        }
        if ids.is_empty() {
            return;
        }
        app.recycle_sel.clear();
        for id in ids {
            app.recycle_sel.insert(id);
        }
        return;
    }

    let mut keys: Vec<String> = Vec::new();
    if app.tab().home {
        let (_, tiles) = paint::home_layout(app, &l.view);
        for t in &tiles {
            if hits(&t.rect, &r) {
                if let Some(e) = app.tab().entries.get(t.index) {
                    keys.push(crate::app::key_of(e));
                }
            }
        }
    } else {
        let view_top = l.view.top + header_height(app);
        let n = app.tab().visible.len();
        for i in 0..n {
            let ir = paint::item_rect(app, l, i, view_top);
            if hits(&ir, &r) {
                if let Some(e) = app.tab().visible.get(i).and_then(|k| app.tab().entries.get(*k)) {
                    keys.push(crate::app::key_of(e));
                }
            }
        }
    }
    if keys.is_empty() {
        return;
    }
    let tab = app.tab_mut();
    tab.selected.clear();
    for k in keys {
        tab.selected.insert(k);
    }
    app.focus_visible = true;
}

/// 详细信息视图表头的高度；其它视图没有表头。
fn header_height(app: &App) -> i32 {
    if app.tab().view_mode == fm_core::ViewMode::Details {
        (26.0 * app.metrics.scale) as i32
    } else {
        0
    }
}

/// 首页命中测试：返回光标下的平铺磁贴对应的**条目下标**。
///
/// 几何由 `paint::home_layout` 统一给出，绘制与命中判定共用，不会出现
/// "显示在这里、点却在别处"的错位。
fn home_entry_at(app: &App, l: &paint::Layout, x: i32, y: i32) -> Option<usize> {
    if !app.tab().home {
        return None;
    }
    let (_, tiles) = paint::home_layout(app, &l.view);
    paint::home_hit(&tiles, x, y).and_then(|t| tiles.get(t).map(|t| t.index))
}

/// 首页磁贴的选中（单选为主；ctrl 保留已有多选语义与列表视图一致）。
fn home_select(app: &mut App, l: &paint::Layout, x: i32, y: i32) -> bool {
    match home_entry_at(app, l, x, y) {
        Some(idx) => {
            app.tab_mut().focus_row = idx;
            if let Some(e) = app.tab().entries.get(idx) {
                let k = crate::app::key_of(e);
                let tab = app.tab_mut();
                if !tab.selected.contains(&k) {
                    tab.selected.clear();
                    tab.selected.insert(k);
                }
            }
            true
        }
        None => {
            app.tab_mut().selected.clear();
            false
        }
    }
}

/// 非客户区消息的坐标是**屏幕坐标**，只做一次 ScreenToClient。
/// （`client_xy` 会先 ClientToScreen 再 ScreenToClient，用在屏幕坐标上是错的。）
fn screen_xy(hwnd: HWND, lparam: LPARAM) -> (i32, i32) {
    let mut pt = POINT {
        x: (lparam.0 & 0xFFFF) as i16 as i32,
        y: (lparam.0 >> 16) as i16 as i32,
    };
    unsafe {
        let _ = ScreenToClient(hwnd, &mut pt);
    }
    (pt.x, pt.y)
}

/// 标题栏三大金刚键的命中编号：0=无，1=最小化，2=最大化，3=关闭。
fn caption_hit(l: &paint::Layout, x: i32, y: i32) -> u8 {
    if inside(&l.min_btn, x, y) {
        1
    } else if inside(&l.max_btn, x, y) {
        2
    } else if inside(&l.close_btn, x, y) {
        3
    } else {
        0
    }
}

/// 执行标题栏按钮的动作。
///
/// 这里**不依赖 DefWindowProc**：客户区早已扩展到整窗（WM_NCCALCSIZE 返回 0），
/// 系统那套非客户区默认处理在这种情况下并不可靠 —— 表现就是"三个键点不动"。
unsafe fn run_caption(hwnd: HWND, which: u8) {
    match which {
        1 => {
            let _ = ShowWindow(hwnd, SW_MINIMIZE);
        }
        2 => {
            let _ = ShowWindow(hwnd, if IsZoomed(hwnd).0 != 0 { SW_RESTORE } else { SW_MAXIMIZE });
        }
        3 => {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        _ => {}
    }
}

/// 设置页里某一行的点击翻译成命令。
/// 设置页里多选项（主题 / 大小单位 / 默认视图）的下拉。
///
/// 之前这三项是"点一下循环到下一档"，选项一多就得连点好几下，而且点下去
/// 会变成什么完全靠猜。改成下拉：一次看全，点哪个就是哪个。
fn show_setting_dropdown(hwnd: HWND, app: &mut App, l: &paint::Layout, action: u32) {
    let entries: Vec<menu::MenuEntry> = match action {
        paint::SET_THEME => {
            let dark = app.cfg.core.theme != "light";
            vec![
                crate::app::pick_row("暗色", menu::Command::SetThemeDark(true), dark),
                crate::app::pick_row("亮色", menu::Command::SetThemeDark(false), !dark),
            ]
        }
        paint::SET_UNIT => {
            let cur = fm_core::SizeUnit::parse(&app.cfg.files.view.size_unit);
            fm_core::SizeUnit::ALL
                .iter()
                .enumerate()
                .map(|(i, u)| {
                    crate::app::pick_row(u.label(), menu::Command::SetSizeUnitIdx(i), *u == cur)
                })
                .collect()
        }
        paint::SET_VIEW => {
            let cur = app.cfg.files.view.default_mode.clone();
            crate::app::DEFAULT_VIEW_CYCLE
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    crate::app::pick_row(
                        crate::app::default_view_label(s),
                        menu::Command::SetDefaultViewIdx(i),
                        *s == cur,
                    )
                })
                .collect()
        }
        _ => return,
    };
    // 锚点：那一行右侧"当前值 ▾"按钮的左下角 —— 菜单从按钮底下长出来
    let row = paint::settings_rows(app, l).into_iter().find(|r| r.action == action);
    let r = match row {
        Some(r) => paint::setting_value_rect(&r, app.metrics.scale),
        None => l.view,
    };
    let mut pt = POINT { x: r.left, y: r.bottom };
    let _ = unsafe { ClientToScreen(hwnd, &mut pt) };
    let app_ptr = app as *mut App;
    if let Some(h) = menu::open_menu(
        app_ptr,
        hwnd,
        entries,
        pt.x,
        pt.y,
        app.metrics.scale,
        app.theme,
        None,
        true, // 由一次左键按下弹出：那一次的抬起要吞掉
    ) {
        app.menu_hwnd = Some(h);
    }
}

/// 某一列的"筛选"下拉里都有些什么。
///
/// 名称/日期/大小用固定分桶（与资源管理器"按…分组"的口径一致），类型是**当前
/// 目录里真实出现过的类型** —— 静态列一份"文件夹/文本文档/…"必然漏类型。
fn header_filter_rows(app: &App, sem: u8) -> Vec<menu::MenuEntry> {
    let cur = app.tab().col_filter.clone();
    let bucket = |want: u8| -> usize {
        match &cur {
            Some(app::ColFilter::Name(b)) if want == 0 => *b,
            Some(app::ColFilter::Date(b)) if want == 1 => *b,
            Some(app::ColFilter::Size(b)) if want == 3 => *b,
            _ => 0,
        }
    };
    match sem {
        0 => app::NAME_BUCKETS
            .iter()
            .enumerate()
            .map(|(i, s)| app::pick_row(s, menu::Command::ColFilter(0, i), bucket(0) == i))
            .collect(),
        1 => app::DATE_BUCKETS
            .iter()
            .enumerate()
            .map(|(i, s)| app::pick_row(s, menu::Command::ColFilter(1, i), bucket(1) == i))
            .collect(),
        3 => app::SIZE_BUCKETS
            .iter()
            .enumerate()
            .map(|(i, s)| app::pick_row(s, menu::Command::ColFilter(3, i), bucket(3) == i))
            .collect(),
        2 => {
            let mut kinds: Vec<String> = app.tab().entries.iter().map(|e| e.type_label()).collect();
            kinds.sort();
            kinds.dedup();
            let active = matches!(&cur, Some(app::ColFilter::Kind(k)) if kinds.contains(k));
            let mut rows = vec![app::pick_row(
                "全部",
                menu::Command::ColFilterKind(String::new()),
                !active,
            )];
            for k in kinds {
                let on = matches!(&cur, Some(app::ColFilter::Kind(c)) if *c == k);
                let label = k.clone();
                rows.push(app::pick_row(&k, menu::Command::ColFilterKind(label), on));
            }
            rows
        }
        // 「位置」列没有可筛的东西（搜索态专有，本身就是一层筛选）
        _ => Vec::new(),
    }
}

/// 点表头最右端的「筛选 ▾」：菜单从三角底下长出来。
fn show_header_filter_menu(hwnd: HWND, app: &mut App, l: &paint::Layout, col: usize) {
    let sem = paint::header_col_semantic(app, col);
    let entries = header_filter_rows(app, sem);
    if entries.is_empty() {
        return;
    }
    let r = paint::header_filter_rect(app, l, col);
    let mut pt = POINT { x: r.left, y: r.bottom };
    let _ = unsafe { ClientToScreen(hwnd, &mut pt) };
    let app_ptr = app as *mut App;
    if let Some(h) = menu::open_menu(
        app_ptr,
        hwnd,
        entries,
        pt.x,
        pt.y,
        app.metrics.scale,
        app.theme,
        None,
        true, // 由一次左键按下弹出：那一次的抬起要吞掉
    ) {
        app.menu_hwnd = Some(h);
    }
}

/// 表头右键：排序方向 + 按这一列筛选（资源管理器同款）。
fn show_header_context_menu(hwnd: HWND, app: &mut App, l: &paint::Layout, col: usize, x: i32, y: i32) {
    let sem = paint::header_col_semantic(app, col);
    let label = match sem {
        0 => "名称",
        1 => "修改日期",
        2 => "类型",
        3 => "大小",
        _ => "位置",
    };
    let desc = app.tab().sort.descending;
    let cur_col = match app.tab().sort.key {
        fm_core::SortKey::Name => 0u8,
        fm_core::SortKey::Modified => 1,
        fm_core::SortKey::Type => 2,
        _ => 3,
    };
    let on_this = cur_col == sem;
    let mut entries: Vec<menu::MenuEntry> = Vec::new();
    // 列与方向一次到位：只给"方向"的话，右键 大小 选递增却仍在按名称排，
    // 看着就像这一项没生效。
    entries.push(app::pick_row(
        &format!("按{}递增排序", label),
        menu::Command::HeaderSort(sem, false),
        on_this && !desc,
    ));
    entries.push(app::pick_row(
        &format!("按{}递减排序", label),
        menu::Command::HeaderSort(sem, true),
        on_this && desc,
    ));
    let filters = header_filter_rows(app, sem);
    if !filters.is_empty() {
        entries.push(menu::MenuEntry::sep());
        entries.push(crate::app::submenu_row("筛选", None, filters));
    }
    if app.tab().col_filter.is_some() {
        entries.push(menu::MenuEntry::sep());
        entries.push(crate::app::pick_row(
            "清除筛选",
            menu::Command::ColFilterKind(String::new()),
            false,
        ));
    }
    let mut pt = POINT { x, y };
    let _ = unsafe { ClientToScreen(hwnd, &mut pt) };
    let app_ptr = app as *mut App;
    if let Some(h) = menu::open_menu(
        app_ptr,
        hwnd,
        entries,
        pt.x,
        pt.y,
        app.metrics.scale,
        app.theme,
        None,
        false,
    ) {
        app.menu_hwnd = Some(h);
    }
}

fn settings_command(action: u32) -> Option<menu::Command> {
    use paint::{
        SET_EXT, SET_FOLLOW, SET_HIDDEN, SET_SIDEBAR, SET_STATUS, SET_SYSTEM, SET_THEME,
        SET_THISPC, SET_UNIT, SET_VIEW,
    };
    match action {
        SET_HIDDEN => Some(menu::Command::ToggleHidden),
        SET_SYSTEM => Some(menu::Command::ToggleSystem),
        SET_EXT => Some(menu::Command::ToggleExtensions),
        SET_UNIT => Some(menu::Command::CycleSizeUnit),
        SET_THEME => Some(menu::Command::CycleTheme),
        SET_VIEW => Some(menu::Command::CycleDefaultView),
        SET_SIDEBAR => Some(menu::Command::ToggleSidebar),
        SET_FOLLOW => Some(menu::Command::ToggleFollowCurrent),
        SET_THISPC => Some(menu::Command::ToggleThispcExpanded),
        SET_STATUS => Some(menu::Command::ToggleStatusbar),
        _ => None,
    }
}

fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// 此刻光标是不是停在"能打字的地方"上（地址栏 / 搜索框）。
///
/// WM_SETCURSOR 不带坐标，只能反过来问系统鼠标在哪。地址栏在编辑态才是真 EDIT
/// （那时 EDIT 自己会设 I 形），非编辑态是父窗口画的面包屑，得由这里补上。
fn over_text_input(app: &App, hwnd: HWND) -> bool {
    let mut pt = POINT::default();
    if unsafe { GetCursorPos(&mut pt) }.is_err() {
        return false;
    }
    let _ = unsafe { ScreenToClient(hwnd, &mut pt) };
    let l = paint::compute_layout(
        app.last_size.0,
        app.last_size.1,
        &app.metrics,
        app.cfg.ui.sidebar_visible,
        app.cfg.ui.statusbar_visible,
    );
    // 编辑态下地址栏里已经是一个真 EDIT，父窗口不该再抢它的光标
    (!app.addr_editing && inside(&l.address, pt.x, pt.y)) || inside(&l.search, pt.x, pt.y)
}

fn client_xy(hwnd: HWND, lparam: LPARAM) -> (i32, i32) {
    let mut pt = POINT {
        x: (lparam.0 & 0xFFFF) as i16 as i32,
        y: (lparam.0 >> 16) as i16 as i32,
    };
    unsafe {
        let _ = ClientToScreen(hwnd, &mut pt);
        let _ = ScreenToClient(hwnd, &mut pt);
    }
    (pt.x, pt.y)
}

/// §4 第 4 条：标题栏/边框的命中区域由我们自行返回，
/// 保留 WS_THICKFRAME 之后，最大化/贴靠动画仍由系统负责。
unsafe fn handle_hit_test(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let app_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    if app_ptr.is_null() {
        return DefWindowProcW(hwnd, WM_NCHITTEST, WPARAM(0), lparam);
    }
    let app = &*app_ptr;
    let mut pt = POINT {
        x: (lparam.0 & 0xFFFF) as i16 as i32,
        y: (lparam.0 >> 16) as i16 as i32,
    };
    let _ = ScreenToClient(hwnd, &mut pt);
    let (w, h) = app.last_size;
    let l = paint::compute_layout(w, h, &app.metrics, app.cfg.ui.sidebar_visible, app.cfg.ui.statusbar_visible);
    let border = (4.0 * app.metrics.scale).round() as i32;
    let plus = l.new_tab_rect(app.tabs.len(), &app.metrics);
    // 标题栏上的自有控件（侧栏开关 / 更多菜单 / 新建标签 / 三大金刚键）所在
    // 的位置一律不算边框 —— 它们本来就压在窗口最外侧那几像素上。
    let on_caption_ctrl = inside(&l.sidebar_toggle, pt.x, pt.y)
        || inside(&l.menu_btn, pt.x, pt.y)
        || inside(&l.min_btn, pt.x, pt.y)
        || inside(&l.max_btn, pt.x, pt.y)
        || inside(&l.close_btn, pt.x, pt.y)
        || (0..app.tabs.len()).any(|i| inside(&l.tab_rect(i, app.tabs.len(), &app.metrics), pt.x, pt.y))
        || (plus.right < l.tabs.right && inside(&plus, pt.x, pt.y));

    // 四边 + 四角。**必须放在标题栏判定之前**：标题栏占着窗口顶上那一长条，
    // 先判标题栏的话顶边永远拿不到 HTTOP。
    // 之前这一节有三个错叠在一起：左右两边多带了 `pt.y > l.view.top`（顶边那一
    // 截直接漏成 HTCLIENT），上边拿 `l.sidebar.top`（**窗格**的上沿）当窗口上沿，
    // 四角又完全没判 —— 于是四面只有底边那 4px 能拉伸。
    if !on_caption_ctrl {
        let on_left = pt.x < border;
        let on_right = pt.x >= w - border;
        let on_top = pt.y < border;
        let on_bottom = pt.y >= h - border;
        match (on_top, on_bottom, on_left, on_right) {
            (true, _, true, _) => return LRESULT(13),  // HTTOPLEFT
            (true, _, _, true) => return LRESULT(14),  // HTTOPRIGHT
            (_, true, true, _) => return LRESULT(16),  // HTBOTTOMLEFT
            (_, true, _, true) => return LRESULT(17),  // HTBOTTOMRIGHT
            (true, _, _, _) => return LRESULT(12),     // HTTOP
            (_, true, _, _) => return LRESULT(15),     // HTBOTTOM
            (_, _, true, _) => return LRESULT(10),     // HTLEFT
            (_, _, _, true) => return LRESULT(11),     // HTRIGHT
            _ => {}
        }
    }

    if pt.y < l.title.bottom {
        // 标题栏上的自有按钮（侧栏开关 / 更多菜单 / 新建标签）要能收到
        // WM_LBUTTONUP，必须判为 HTCLIENT，否则系统把它当标题栏拖拽吞掉。
        // 标签本身也必须判 HTCLIENT：之前整条标签栏都返回 HTCAPTION，
        // 系统把按下当成"拖窗口"直接吞掉，窗口永远收不到 WM_LBUTTONDOWN
        // —— 表现就是"标签点不动、切不了"。标签之间的空白仍留给拖拽。
        if inside(&l.sidebar_toggle, pt.x, pt.y)
            || inside(&l.menu_btn, pt.x, pt.y)
            || (0..app.tabs.len()).any(|i| inside(&l.tab_rect(i, app.tabs.len(), &app.metrics), pt.x, pt.y))
            || (plus.right < l.tabs.right && inside(&plus, pt.x, pt.y))
        {
            return LRESULT(1); // HTCLIENT
        }
        if inside(&l.min_btn, pt.x, pt.y) {
            return LRESULT(8); // HTMINBUTTON
        }
        if inside(&l.max_btn, pt.x, pt.y) {
            return LRESULT(9); // HTMAXBUTTON
        }
        if inside(&l.close_btn, pt.x, pt.y) {
            return LRESULT(20); // HTCLOSE
        }
        return LRESULT(2); // HTCAPTION
    }
    LRESULT(1) // HTCLIENT
}

fn hit_tab(app: &App, l: &paint::Layout, x: i32, y: i32) -> Option<usize> {
    if y >= l.title.top && y <= l.title.bottom {
        for i in 0..app.tabs.len() {
            if inside(&l.tab_rect(i, app.tabs.len(), &app.metrics), x, y) {
                return Some(i);
            }
        }
    }
    None
}

fn header_click(app: &mut App, l: &paint::Layout, x: i32) {
    let columns = [
        (("名称"), fm_core::SortKey::Name),
        (("修改日期"), fm_core::SortKey::Modified),
        (("类型"), fm_core::SortKey::Type),
        (("大小"), fm_core::SortKey::Size),
    ];
    // 列宽与 paint_view 同源（用户拖过的宽度也在这里生效），
    // 否则点击命中的列和看到的列错位
    let w = app.col_widths();
    let widths = [
        (w[0] as f32 * app.metrics.scale) as i32,
        (w[1] as f32 * app.metrics.scale) as i32,
        (w[2] as f32 * app.metrics.scale) as i32,
        (w[3] as f32 * app.metrics.scale) as i32,
    ];
    let mut cursor = l.view.left;
    for (i, (_, key)) in columns.iter().enumerate() {
        let width = widths[i];
        if x >= cursor && x < cursor + width {
            app.sort_column(*key);
            return;
        }
        cursor += width;
    }
}

fn sidebar_click(app: &mut App, x: i32, y: i32) -> bool {
    let l = paint::compute_layout(
        app.last_size.0,
        app.last_size.1,
        &app.metrics,
        app.cfg.ui.sidebar_visible,
        app.cfg.ui.statusbar_visible,
    );
    // 命中判定复用绘制端那一份排列（`App::sidebar_rows`），不在这里重算 ——
    // 树会展开/收起，本地再算一遍必然和画出来的行错位。
    let hit = paint::sidebar_hit(app, &l, x, y, app.metrics.scale);
    let Some((index, on_chevron)) = hit else { return false };
    let row = app.sidebar_rows().get(index).cloned();
    let Some(row) = row else { return false };
    if on_chevron {
        // 点箭头：只动树，不换目录（资源管理器就是这个语义）
        app.toggle_sidebar_node(&row.key);
        // 收起之后整棵树变短了，滚动偏移要跟着收回去，否则会停在空白区
        app.clamp_sidebar_scroll(&l);
        return true;
    }
    // 单击**只导航，不展开**。之前这里顺手把节点展开了，结果想换个目录就
    // 被迫把树撑开一大片，滚都滚不回来；树的展开/收起交给箭头和双击。
    if row.target.is_empty() {
        app.navigate(PathBuf::from(app::HOME_PATH));
    } else {
        app.navigate(PathBuf::from(&row.target));
    }
    paint::sync_address(app);
    true
}

/// 导航窗格**双击**：展开 / 收起。
///
/// 单击只导航（见 `sidebar_click`），双击才动树 —— 这是资源管理器的导航窗格语义：
/// "点一下是进去看看，双击是把它摊开"。混在一起的话想进个目录就顺带把树撑开。
fn sidebar_dblclick(app: &mut App, x: i32, y: i32) -> bool {
    let l = paint::compute_layout(
        app.last_size.0,
        app.last_size.1,
        &app.metrics,
        app.cfg.ui.sidebar_visible,
        app.cfg.ui.statusbar_visible,
    );
    let Some((index, _)) = paint::sidebar_hit(app, &l, x, y, app.metrics.scale) else {
        return false;
    };
    let row = app.sidebar_rows().get(index).cloned();
    let Some(row) = row else { return false };
    // 不可展开的节点（回收站、常用文件夹）双击没有第二层含义，交给单击那次导航
    if !row.expandable {
        return true;
    }
    app.toggle_sidebar_node(&row.key);
    app.clamp_sidebar_scroll(&l);
    true
}

fn handle_key(app: &mut App, key: i32) {
    let ctrl = unsafe { GetKeyState(0x11) < 0 };
    let shift = unsafe { GetKeyState(0x10) < 0 };
    let alt = unsafe { GetKeyState(0x12) < 0 };
    match () {
        _ if ctrl && !shift && key == 0x41 => app.select_all(),
        _ if ctrl && key == 0x43 => app.clipboard_write(fm_core::clipboard::ClipMode::Copy),
        _ if ctrl && key == 0x58 => app.clipboard_write(fm_core::clipboard::ClipMode::Move),
        // 必须带 !shift：否则 Ctrl+Shift+V（循环视图）会被这一条截胡 ——
        // match 是按书写顺序匹配的，粘贴排在前面就永远轮不到下面那行。
        _ if ctrl && !shift && key == 0x56 => app.paste(),
        _ if ctrl && !shift && key == 0x46 => paint::focus_edit(app.hwnd_search),
        // Ctrl+L / F4 / Alt+D：地址栏。Win11 下这三个键都是"切到可输入的地址栏"，
        // 所以不能只 focus 那个平时藏着的 EDIT，得先把它显示出来。
        _ if ctrl && key == 0x4C => paint::begin_address_edit(app),
        _ if ctrl && key == 0x54 => app.new_tab(),
        // Ctrl+N：新开一个**窗口**（与 Ctrl+T 新标签区分）。开在本进程里，资源共享。
        _ if ctrl && !shift && key == 0x4E => crate::new_window(PathBuf::from(app::HOME_PATH)),
        // Ctrl+,：打开设置（Windows 应用的通用设置快捷键）
        _ if ctrl && key == 0xBC => app.open_settings(),
        _ if ctrl && key == 0x57 => app.close_tab(app.active),
        _ if ctrl && shift && key == 0x4E => app.new_folder(),
        _ if ctrl && shift && key == 0x56 => app.cycle_view(),
        _ if ctrl && shift && key == 0x43 => {
            if let Err(e) = app.copy_paths_text() {
                app::report(app.hwnd, &format!("复制路径失败：{e}"));
            }
        }
        // 标签页切换
        _ if ctrl && !shift && key == 0x09 => app.cycle_tab(1),
        _ if ctrl && shift && key == 0x09 => app.cycle_tab(-1),
        // 地址栏 / 搜索框（Explorer 用 F4 与 Alt+D 定位地址栏）
        _ if key == 0x73 || (alt && key == 0x44) => paint::begin_address_edit(app),
        _ if ctrl && key == 0x45 => paint::focus_edit(app.hwnd_search),
        _ if key == 0x72 => paint::focus_edit(app.hwnd_search),
        // 刷新（F5 之外的 Ctrl+R）
        _ if ctrl && key == 0x52 => app.reload(),
        // 属性 Alt+Enter
        _ if alt && key == 0x0D => app.run_command(menu::Command::Properties),
        // Ctrl+1..4 直接切视图
        _ if ctrl && !shift && key == 0x31 => app.set_view(fm_core::ViewMode::Details),
        _ if ctrl && !shift && key == 0x32 => app.set_view(fm_core::ViewMode::List),
        _ if ctrl && !shift && key == 0x33 => app.set_view(fm_core::ViewMode::MediumIcons),
        _ if ctrl && !shift && key == 0x34 => app.set_view(fm_core::ViewMode::LargeIcons),
        _ if key == 0x70 => app.reload(),
        _ if key == 0x71 => app.begin_rename(),
        _ if key == 0x2E && !shift => app.delete_selected(false),
        _ if key == 0x2E && shift => app.delete_selected(true),
        _ if key == 0x0D => app.open_focused(),
        _ if key == 0x08 => app.go_up(),
        _ if alt && key == 0x25 => app.go_back(),
        _ if alt && key == 0x27 => app.go_forward(),
        _ if alt && key == 0x26 => app.go_up(),
        _ if key == 0x28 => app.set_focus_row(app.tab().focus_row + 1),
        _ if key == 0x26 && !alt => app.set_focus_row(app.tab().focus_row.saturating_sub(1)),
        _ if key == 0x22 => app.set_focus_row(app.tab().focus_row + 10),
        _ if key == 0x21 => app.set_focus_row(app.tab().focus_row.saturating_sub(10)),
        _ if key == 0x24 => app.set_focus_row(0),
        _ if key == 0x23 => {
            let last = app.tab().visible.len().saturating_sub(1);
            app.set_focus_row(last);
        }
        _ => {}
    }
    app.invalidate();
}

// ---------------------------------------------------------------- 输入框回调

pub(crate) fn on_address_enter() {
    let app = app_ref();
    let text = paint::read_window_text(app.hwnd_address);
    let cwd = app.tab().path.clone();
    // 回车之后地址栏要收回面包屑 —— 不收回的话它会一直挂在屏幕上挡着路径
    paint::end_address_edit(app, true);
    match fm_core::paths::resolve(&text, &cwd) {
        Ok(Target::Navigate(p)) => {
            if p.is_dir() {
                app.navigate(p.clone());
            } else {
                app_ref().launch_file(&p);
            }
            paint::sync_address(app);
        }
        Ok(Target::ShellCommand(shell)) => {
            launch_shell_in(app, &shell, &cwd);
        }
        Err(e) => app::report(app.hwnd, &format!("无法解析路径：{e}")),
    }
    unsafe {
        let _ = SetForegroundWindow(app.hwnd);
    }
}

/// 地址框失去焦点：收回面包屑（不抢焦点，否则会和 SetFocus 互相踢）。
pub(crate) fn on_address_blur() {
    let app = app_ref();
    paint::end_address_edit(app, false);
}

/// §13：地址栏输入 cmd/powershell/wsl 时，在当前目录启动对应 shell。
fn launch_shell_in(app: &App, shell: &str, cwd: &std::path::Path) {
    let program = match shell {
        "powershell" | "pwsh" => "powershell.exe",
        "wsl" => "wsl.exe",
        _ => "cmd.exe",
    };
    unsafe {
        let file = HSTRING::from(program);
        let dir = HSTRING::from(cwd.to_string_lossy().as_ref());
        let _ = windows::Win32::UI::Shell::ShellExecuteW(
            Some(app.hwnd),
            &HSTRING::from("open"),
            &file,
            None,
            &dir,
            SW_SHOW,
        );
    }
}

pub(crate) fn on_search_enter() {
    let app = app_ref();
    let text = paint::read_window_text(app.hwnd_search);
    if text.is_empty() {
        let tab = app.tab_mut();
        tab.search_root = None;
        tab.filter.clear();
        app.reload();
        return;
    }
    // 回车 = 搜整个卷（走文件名索引，毫秒级）；仅输入则是即时筛选当前目录 §5
    let tab = app.tab_mut();
    tab.filter = text;
    app.begin_volume_search();
    app.invalidate();
}

pub(crate) fn on_edit_escape() {
    let app = app_ref();
    paint::end_address_edit(app, true);
    paint::sync_address(app);
    paint::set_window_text_utf16(app.hwnd_search, "");
    {
        let (sh, ss) = (app.show_hidden, app.show_system);
        let tab = app.tab_mut();
        tab.filter.clear();
        tab.search_root = None;
        tab.recompute_filter(sh, ss);
    }
    app.reload();
    app.invalidate();
}

pub(crate) fn on_rename_commit() {
    let app = app_ref();
    let hwnd = match app.rename_hwnd {
        Some(h) => h,
        None => return,
    };
    let name = paint::read_window_text(hwnd);
    app.commit_rename(&name);
    app.invalidate();
}

pub(crate) fn on_rename_cancel() {
    let app = app_ref();
    app.cancel_rename();
    app.invalidate();
}
