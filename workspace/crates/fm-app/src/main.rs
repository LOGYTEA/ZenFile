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
mod paint;
mod props;
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
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, RegisterHotKey, UnregisterHotKey};
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
const HOTKEY_MAIN: i32 = 1;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--selftest" || a == "--self-test") {
        // Windows 子系统下 CRT 的 stdout 是空的，先把父进程的控制台挂回来
        unsafe { attach_console() };
        std::process::exit(fm_core::selftest::run());
    }
    if args.iter().any(|a| a == "--version") {
        unsafe { attach_console() };
        println!("fm-app {} (protocol {})", env!("CARGO_PKG_VERSION"), fm_core::PROTOCOL_VERSION);
        return;
    }

    let tray_mode = args.iter().any(|a| a == "--tray");

    // 单实例锁 §3.1（第 4 条）
    let mutex = unsafe {
        windows::Win32::System::Threading::CreateMutexW(None, false, &HSTRING::from("NexFile.SingleInstance"))
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

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(MAIN_CLASS.as_ptr()),
            PCWSTR(HSTRING::from("NexFile").as_ptr()),
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
        MAIN_CLASS = HSTRING::from("NexFileMainWindow");
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
    for (i, c) in "NexFile 资源管理器".encode_utf16().take(127).enumerate() {
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
            app.last_size = ((lparam.0 & 0xFFFF) as i32, (lparam.0 >> 16) as i32);
            paint::reposition(app);
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let app = &mut *app_ptr;
            app.metrics = theme::Metrics::new(GetDpiForWindow(hwnd));
            paint::reposition(app);
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_DRAIN {
                let app = &mut *app_ptr;
                app.drain();
                if let Some(stats) = &app.transfer {
                    if stats.lock().map(|s| s.finished).unwrap_or(true) {
                        app.transfer = None;
                        app.reload();
                    }
                }
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
            if in_close != app.hovering_close
                || chrome != app.hover_chrome
                || hover_tab != app.hover_tab
                || hover_view != app.hover_view_btn
            {
                app.hovering_close = in_close;
                app.hover_chrome = chrome;
                app.hover_tab = hover_tab;
                app.hover_view_btn = hover_view;
                let _ = InvalidateRect(Some(hwnd), None, false);
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
            let ctrl = GetKeyState(0x11) < 0;
            let shift = GetKeyState(0x10) < 0;
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
            // 状态栏右下角：资源管理器同款的"大图标 / 详细信息"快速切换
            if app.cfg.ui.statusbar_visible {
                if let Some(mode) = paint::status_view_hit(&l, &app.metrics, x, y) {
                    app.set_view(mode);
                    return LRESULT(0);
                }
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
            if app.cfg.ui.sidebar_visible && x < l.sidebar.right && y > l.sidebar.top {
                if sidebar_click(app, x, y) {
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
            let header_h = if app.tab().view_mode == fm_core::ViewMode::Details {
                (26.0 * app.metrics.scale) as i32
            } else {
                0
            };
            let view_top = l.view.top + header_h;
            if y >= l.view.top && y < header_h + l.view.top && app.tab().view_mode == fm_core::ViewMode::Details {
                header_click(app, &l, x);
                return LRESULT(0);
            }
            if let Some(row) = app.row_hit(y, view_top, l.view.bottom - view_top) {
                app.toggle_select(row, ctrl, shift);
                let _ = InvalidateRect(Some(hwnd), None, true);
            } else if y >= view_top {
                app.tab_mut().selected.clear();
                let _ = InvalidateRect(Some(hwnd), None, true);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = client_xy(hwnd, lparam);
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
            let header_h = if app.tab().view_mode == fm_core::ViewMode::Details {
                (26.0 * app.metrics.scale) as i32
            } else {
                0
            };
            if let Some(row) = app.row_hit(y, l.view.top + header_h, l.view.bottom - l.view.top - header_h) {
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
            let header_h = if app.tab().view_mode == fm_core::ViewMode::Details {
                (26.0 * app.metrics.scale) as i32
            } else {
                0
            };
            let view_top = l.view.top + header_h;
            if app.tab().home {
                // 首页：右键前的选中判定走磁贴几何，菜单才会作用在光标下的那个文件夹上
                if y >= l.view.top && x >= l.view.left {
                    home_select(app, &l, x, y);
                    let _ = InvalidateRect(Some(hwnd), None, true);
                }
            } else if x >= l.view.left && y >= view_top && y < l.view.bottom {
                // 只有落在文件列表区才改选中；命中行按资源管理器语义处理，点空白则清空选中，
                // 这样右键菜单作用的对象就是"光标下的那个文件夹"而不是上一次遗留的选中项。
                if let Some(row) = app.row_hit(y, view_top, l.view.bottom - view_top) {
                    app.context_select_row(row);
                } else {
                    app.tab_mut().selected.clear();
                }
                let _ = InvalidateRect(Some(hwnd), None, true);
            }
            let mut pt = POINT { x, y };
            let _ = ClientToScreen(hwnd, &mut pt);
            let extended = GetKeyState(0x10) < 0; // Shift+右键 → Explorer 原生菜单 §7
            app.show_context_menu(pt.x, pt.y, extended);
            LRESULT(0)
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
            let metrics = app.metrics;
            {
                let tab = app.tab_mut();
                tab.scroll -= delta / 2;
                tab.clamp_scroll(l.view.bottom - l.view.top, &metrics);
            }
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
    // 列宽与 paint_view 同源，否则点击命中的列和看到的列错位
    let mut cursor = l.view.left;
    for (_, key) in columns {
        let width = match key {
            fm_core::SortKey::Name => (274.0 * app.metrics.scale) as i32,
            fm_core::SortKey::Modified => (144.0 * app.metrics.scale) as i32,
            fm_core::SortKey::Type => (120.0 * app.metrics.scale) as i32,
            fm_core::SortKey::Size => (80.0 * app.metrics.scale) as i32,
        };
        if x >= cursor && x < cursor + width {
            app.sort_column(key);
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
    // 行高必须与绘制端同一个函数算，否则改了字体/行高后点击就点偏了
    let row = paint::sidebar_row_h(app.metrics.scale);
    let mut top = l.sidebar.top + row;
    for (_, target) in paint::sidebar_entries() {
        if y >= top && y < top + row && x < l.sidebar.right {
            if target.is_empty() {
                app.navigate(PathBuf::from(app::HOME_PATH));
            } else {
                app.navigate(PathBuf::from(target));
            }
            paint::sync_address(app);
            return true;
        }
        top += row;
    }
    false
}

fn handle_key(app: &mut App, key: i32) {
    let ctrl = unsafe { GetKeyState(0x11) < 0 };
    let shift = unsafe { GetKeyState(0x10) < 0 };
    let alt = unsafe { GetKeyState(0x12) < 0 };
    match () {
        _ if ctrl && !shift && key == 0x41 => app.select_all(),
        _ if ctrl && key == 0x43 => app.clipboard_write(fm_core::clipboard::ClipMode::Copy),
        _ if ctrl && key == 0x58 => app.clipboard_write(fm_core::clipboard::ClipMode::Move),
        _ if ctrl && key == 0x56 => app.paste(),
        _ if ctrl && key == 0x46 => paint::focus_edit(app.hwnd_search),
        _ if ctrl && key == 0x4C => paint::focus_edit(app.hwnd_address),
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
        _ if key == 0x73 || (alt && key == 0x44) => paint::focus_edit(app.hwnd_address),
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
    // 回车 = 递归搜索整个子树；仅输入则是即时筛选当前目录 §5
    let tab = app.tab_mut();
    tab.search_root = Some(tab.path.clone());
    tab.filter = text;
    app.reload();
    app.invalidate();
}

pub(crate) fn on_edit_escape() {
    let app = app_ref();
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
