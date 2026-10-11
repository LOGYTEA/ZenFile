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

use app::{init_com, App};
use fm_core::config;
use fm_core::hotkey;
use fm_core::paths::Target;

/// App 实例的全局裸指针。WndProc 与子控件回调需要通过它访问状态；
/// 全程单线程 UI，因此不存在数据竞争。
static mut APP: *mut App = std::ptr::null_mut();

const WM_TRAY: u32 = WM_APP + 1;
const TIMER_DRAIN: usize = 1;
/// 目录栏下沿进度条的动画节拍。一直挂着，但只有 busy() 时才真的重画。
const TIMER_BUSY: usize = 2;
const HOTKEY_MAIN: i32 = 1;

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
        println!("fm-app {} (protocol {})", env!("CARGO_PKG_VERSION"), fm_core::PROTOCOL_VERSION);
        return;
    }

    let tray_mode = args.iter().any(|a| a == "--tray");

    // 单实例锁 §3.1（第 4 条）
    let mutex = unsafe {
        windows::Win32::System::Threading::CreateMutexW(None, false, &HSTRING::from("ZenFile.SingleInstance"))
    };
    if let Err(e) = &mutex {
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            eprintln!("已有一个实例在运行：{e:?}");
            return;
        }
    }

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

    menu::register_menu_class();
    props::register_props_class();
    openwith::register_class();
    rename::register_class();

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(MAIN_CLASS.as_ptr()),
            PCWSTR(HSTRING::from("ZenFile").as_ptr()),
            WS_OVERLAPPEDWINDOW, // 含 WS_THICKFRAME：保留贴靠与最大化动画的前提 §4
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            (1180.0) as i32,
            760,
            None,
            None,
            instance,
            None,
        )
    }
    .expect("创建主窗口失败");

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

    let start_path = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(PathBuf::from)
        // 无参数时落在"主页"（与资源管理器首页一致），而不是进程的当前目录
        .unwrap_or_else(|| PathBuf::from(app::HOME_PATH));

    let mut boxed = Box::new(App::new(hwnd, cfg));
    boxed.metrics = theme::Metrics::new(unsafe { GetDpiForWindow(hwnd) });
    // 量度按 DPI 算完之后，再把"用户拖过的导航窗格宽度"盖回去
    boxed.sync_metrics();
    boxed.open_tab(start_path);
    let app = Box::into_raw(boxed);
    unsafe {
        APP = app;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, app as isize);
    }

    let app = unsafe { &mut *APP };
    paint::create_children(app, instance);
    paint::sync_address(app);

    // 托盘（唯一）§3.1 第 3 条
    add_tray(hwnd);

    // 全局热键 §8：默认 Ctrl+Alt+*，Win 组合保留给用户自行配置
    register_hotkeys(app, hwnd);

    // 已启用模块作为子进程启动（导致任务管理器里的树状折叠）
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            app.spawn_modules(dir);
        }
    }

    unsafe {
        let _ = SetTimer(Some(hwnd), TIMER_DRAIN, 100, None);
        let _ = SetTimer(Some(hwnd), TIMER_BUSY, 50, None);
        if tray_mode {
            let _ = ShowWindow(hwnd, SW_HIDE);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = windows::Win32::Graphics::Gdi::UpdateWindow(hwnd);
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
        let _ = KillTimer(Some(hwnd), TIMER_DRAIN);
        let _ = KillTimer(Some(hwnd), TIMER_BUSY);
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_MAIN);
        remove_tray(hwnd);
        let app = &mut *APP;
        if let Some(tree) = app.tree.take() {
            drop(tree);
        }
        if let Some(w) = app.watcher.take() {
            w.stop();
        }
        let _ = drop_app();
        let _ = CloseHandle(mutex.unwrap());
    }
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
    for (i, c) in "ZenFile 资源管理器".encode_utf16().take(127).enumerate() {
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
        WM_SETCURSOR => {
            let app = &*app_ptr;
            if app.drag_split.is_some() || app.hover_split.is_some() {
                if let Ok(c) = LoadCursorW(None, IDC_SIZEWE) {
                    SetCursor(Some(c));
                    return LRESULT(1);
                }
            }
            LRESULT(0)
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
            // 设置页的悬停行：点哪一行要高亮，否则不知道自己会改到哪一项
            let hover_setting = if app.tab().settings {
                paint::settings_hit(app, &l, x, y)
            } else {
                None
            };
            // 导航窗格悬停行
            let hover_sidebar = if app.cfg.ui.sidebar_visible {
                paint::sidebar_hit(app, &l, x, y, app.metrics.scale).map(|(i, _)| i)
            } else {
                None
            };
            let hover_collapse = app.cfg.ui.sidebar_visible
                && paint::sidebar_collapse_hit(&l, app.metrics.scale, x, y);
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
                || hover_collapse != app.hover_sidebar_collapse
            {
                app.hover_sidebar_collapse = hover_collapse;
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
            app.pressed_caption = caption_hit(&l, x, y);
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
            }
            LRESULT(0)
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
            if caption_hit(&l, x, y) != 0 {
                run_caption(hwnd, caption_hit(&l, x, y));
            }
            LRESULT(0)
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
            // 设置页：点一行就切换/循环对应开关
            if app.tab().settings {
                if let Some(action) = paint::settings_hit(app, &l, x, y) {
                    if let Some(cmd) = settings_command(action) {
                        app.run_command(cmd);
                    }
                }
                return LRESULT(0);
            }
            // 回收站视图：先判顶部三个按钮，再判条目
            if app.tab().recycle {
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
            // 导航窗格右上角的折叠按钮：压在首行之上那条预留带里，先判它
            if app.cfg.ui.sidebar_visible && paint::sidebar_collapse_hit(&l, app.metrics.scale, x, y) {
                app.toggle_sidebar();
                let _ = InvalidateRect(Some(hwnd), None, true);
                return LRESULT(0);
            }
            if app.cfg.ui.sidebar_visible && x < l.sidebar.right && y > l.sidebar.top {
                if sidebar_click(app, x, y) {
                    // 必须重画：点箭头只改树的状态，不像 navigate 那样自己会触发
                    // 重绘。少了这一句，展开/收起看起来就是"点了没反应"。
                    let _ = InvalidateRect(Some(hwnd), None, true);
                    return LRESULT(0);
                }
            }
            // 首页是平铺布局，行列式命中在这里不适用
            if app.tab().home && y >= l.view.top {
                if home_select(app, &l, x, y) {
                    let _ = InvalidateRect(Some(hwnd), None, true);
                } else {
                    let _ = InvalidateRect(Some(hwnd), None, true);
                }
                return LRESULT(0);
            }
            let header_h = header_height(app);
            let view_top = l.view.top + header_h;
            if y >= l.view.top && y < view_top && app.tab().view_mode == fm_core::ViewMode::Details {
                header_click(app, &l, x);
                return LRESULT(0);
            }
            if let Some(row) = view_row_at(app, &l, x, y, view_top) {
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
                app.open_focused();
                paint::sync_address(app);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            let app = &mut *app_ptr;
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
                // 先把"这一次要作用在哪些条目上"算出来存进 App，
                // 菜单里绝不能再调 selected_paths() —— 它在选中为空时会退回焦点行，
                // 那正是"空白处右键却判定到条目上"的根因。
                let targets: Vec<PathBuf> = if app.tab().home {
                    if y >= l.view.top && x >= l.view.left {
                        home_select(app, &l, x, y);
                        app.tab().selected_paths()
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
        WM_TRAY => {
            if lparam.0 as u32 == 0x0201 || lparam.0 as u32 == 0x0203 {
                let hidden = IsWindowVisible(hwnd).0 == 0;
                let _ = ShowWindow(hwnd, if hidden { SW_SHOW } else { SW_HIDE });
                if hidden {
                    let _ = SetForegroundWindow(hwnd);
                }
            } else if lparam.0 as u32 == 0x0205 {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
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
fn select_marquee(app: &mut App, l: &paint::Layout, drag: &app::Drag) {
    let r = paint::marquee_rect(drag);
    let view_top = l.view.top + header_height(app);
    let n = app.tab().visible.len();
    let mut keys: Vec<String> = Vec::new();
    for i in 0..n {
        let ir = paint::item_rect(app, l, i, view_top);
        if ir.right > r.left && ir.left < r.right && ir.bottom > r.top && ir.top < r.bottom {
            if let Some(e) = app.tab().visible.get(i).and_then(|k| app.tab().entries.get(*k)) {
                keys.push(crate::app::key_of(e));
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

        if pt.y < l.title.bottom {
        let plus = l.new_tab_rect(app.tabs.len(), &app.metrics);
        // 标题栏上的自有按钮（侧栏开关 / 更多菜单 / 新建标签）要能收到
        // WM_LBUTTONUP，必须判为 HTCLIENT，否则系统把它当标题栏拖拽吞掉。
        // 标签本身也必须判 HTCLIENT：之前整条标签栏都返回 HTCAPTION，
        // 系统把按下当成"拖窗口"直接吞掉，窗口永远收不到 WM_LBUTTONDOWN
        // —— 表现就是"标签点不动、切不了"。标签之间的空白仍留给拖拽。
        let on_tab = (0..app.tabs.len())
            .any(|i| inside(&l.tab_rect(i, app.tabs.len(), &app.metrics), pt.x, pt.y));
        if inside(&l.sidebar_toggle, pt.x, pt.y)
            || inside(&l.menu_btn, pt.x, pt.y)
            || on_tab
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
    let top = pt.y >= l.sidebar.top && pt.y <= l.sidebar.top + border;
    if top {
        return LRESULT(12); // HTTOP
    }
    if pt.x < border && pt.y > l.view.top {
        return LRESULT(10); // HTLEFT
    }
    if pt.x > w - border && pt.y > l.view.top {
        return LRESULT(11); // HTRIGHT
    }
    if pt.y > h - border {
        return LRESULT(15); // HTBOTTOM
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
    // 点行：导航过去；可展开的节点顺手展开，省一次点击
    if row.expandable && !row.expanded {
        app.toggle_sidebar_node(&row.key);
    }
    if row.target.is_empty() {
        app.navigate(PathBuf::from(app::HOME_PATH));
    } else {
        app.navigate(PathBuf::from(&row.target));
    }
    paint::sync_address(app);
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
