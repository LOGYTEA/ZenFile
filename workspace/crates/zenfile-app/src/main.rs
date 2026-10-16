//! zenfile-app 主体进程入口。
//!
//! 职责边界（技术方案 §3.1 / §3.6）：
//!   - 只有本进程注册托盘与开机自启；
//!   - 所有子模块由本进程 CreateProcess 拉起，宿主常驻；
//!   - 退出顺序固定：先终止子进程，宿主最后退出。

// 以 Windows 子系统构建：否则双击运行时系统会额外分配一个控制台窗口（用户看到的
// "cmd 窗口"）。命令行模式（--selftest / --version）再挂回父进程控制台输出。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod appfind;
mod coldlg;
mod dnd;
mod fmtprev;
mod icon_theme;
mod anim_clock;
mod icon_spec_design;
mod icons;
mod icon_spec;
mod menu;
mod openwith;
mod plugin_host;
mod rename;
mod paint;
mod paint_cache;
mod props;
mod shellnew;
mod progress;
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
    GetKeyState, RegisterHotKey, ReleaseCapture, SetCapture, TrackMouseEvent, UnregisterHotKey,
    TRACKMOUSEEVENT, TRACKMOUSEEVENT_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::*;

/// `WM_MOUSELEAVE` 属鼠标消息域，`WindowsAndMessaging::*` 的通配带不到
/// （编译报 not bound in all patterns），这里自己定义一份。
const WM_MOUSELEAVE: u32 = 0x02A3;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::DataExchange::COPYDATASTRUCT;

use app::{init_com, App};
use zenfile_core::config;
use zenfile_core::hotkey;
use zenfile_core::paths::Target;

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
pub const APP_VERSION: &str = "0.3 Beta Weekly26-10-14";
/// 目录栏下沿进度条的动画节拍。一直挂着，但只有 busy() 时才真的重画。
const TIMER_BUSY: usize = 2;
/// 界面过渡动画的节拍（16ms ≈ 60Hz，第57轮）。
///
/// **不是常驻的**：有动画在跑才由 `App::sync_anim_timer` 挂上，跑完立刻
/// Kill 掉 —— 为几段 300ms 的过渡常驻一个 60Hz 定时器纯属浪费，还阻止
/// 系统降频。
pub const TIMER_ANIM: usize = 3;
const HOTKEY_MAIN: i32 = 1;
/// 系统双击时限之上一点点：低于它是"连着两下"（= 打开），高于它算"两次慢速单击"（= 改名）。
const DOUBLE_CLICK_MS: u64 = 500;

/// zenfile-app 自己的自检项（`zenfile-core::selftest` 碰不到 UI 层这边的模块）。
fn app_selftest() -> i32 {
    let mut cases = crate::sysview::selftest();
    // 第66轮：图标规格自检 —— 用户报"所有图标风格不统一、大小都不一样"。
    // 把规格本身变成可核对的输出：全软件到底有几种尺寸、各档笔画多粗。
    cases.extend(crate::icon_spec::selftest());
    let mut pass = 0usize;
    println!("\n—— zenfile-app 附加自检 ——");
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

/// `--bench-timer`：量 `SetTimer(16)` 的**真实投递间隔**（第68轮）。
///
/// # 为什么要有这个模式
///
/// 动画卡不卡取决于 `WM_TIMER` 实际多久来一次，而**任何间接判据都会量错**：
///
/// | 判据 | 量到什么 | 为什么不能用 |
/// |---|---|---|
/// | `Sleep(1)` | **调度延迟** | 这台机器上恒为 15.5ms，与 `SetTimer` 无关 |
/// | `NtQueryTimerResolution` | 全局节拍 | 已是 0.997ms，但 `WM_TIMER` 仍 31ms |
/// | 画面像素变化 | DWM 合成 + 抓帧开销 | 读数偏小，且夹着合成延迟 |
///
/// 唯一可信的是**直接在 wnd_proc 里打点** —— 那正是动画驱动收到消息的时刻。
///
/// # 为什么要自建窗口
///
/// 主窗的 `TIMER_ANIM` 是**按需挂/摘**的（`sync_anim_timer`），静止时根本没
/// 定时器在跑，量不到东西。
///
/// # 一个必须记住的坑
///
/// 消息循环要 **Peek + Dispatch 两步**。只 `PeekMessage(PM_REMOVE)` 会把消息
/// 从队列取走但**不调用 wnd_proc**，量出来是"0 次"。（第一版就漏了。）
/// 帧链路埋点录制的两条自定义消息（第69轮）。
const WM_APP_TRACE_STOP: u32 = WM_APP + 10;

/// 埋点录制驱动用的窗口定时器 ID（与 `TIMER_BUSY` / `TIMER_DRAIN` 不冲突）。
const TRACE_TIMER_ID: usize = 9;

/// 埋点 dump 的目标文件（写 `$TEMP%`，release 是 windows 子系统，
/// `println!` 全被丢弃 —— 调试信息只能走文件）。
fn anim_trace_path() -> PathBuf {
    std::env::temp_dir().join("zenfile_anim_trace.csv")
}

/// 第69轮：挂一个"到点 dump"的链路。
///
/// 流程（全部在**主线程**，所以 dump 与录制之间不存在撕裂）：
///
///   1. 挂一只 500ms 的 `TIMER_TRACE`：满两拍后（≈1s 预热）才开 trace
///      —— 预热期丢掉是因为刚开窗那几帧的 `dt` 与消息队列状态都不是
///      稳态，录进来会污染分布。
///   2. 第一次触发 → 开 `TRACE_ON`。
///   3. 第二次触发 → 关 `TRACE_ON`、写 CSV、Kill 定时器。
///
/// 用**消息定时器**而不是 `std::thread::sleep` + `PostMessage`：dump
/// 必须在 UI 线程做（后台线程去 `dump_trace` 会和 `trace_paint_enter`
/// 抢锁，虽然有 Mutex 不会崩，但会把"最后一帧 paint"打乱）。
/// `TIMER_TRACE` 是独立 ID，不与 `TIMER_BUSY` / `TIMER_DRAIN` 冲突。
fn spawn_anim_trace_timer(hwnd: HWND, record_ms: u64) {
    unsafe {
        // 500ms 一拍：两拍开录，之后按 record_ms 累计到点收网。
        let _ = SetTimer(Some(hwnd), TRACE_TIMER_ID, 500, None);
    }
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(1000 + record_ms);
    TRACE_DEADLINE
        .lock()
        .map(|mut d| *d = Some(deadline))
        .unwrap_or(());
    TRACE_STAGE
        .lock()
        .map(|mut s| *s = 0)
        .unwrap_or(());
}

/// `--anim-trace` 的时间线状态（阶段 + 收网时刻），仅主线程读写，
/// 用 `Mutex` 而不是 `static mut` 只是为了不引入新的 `unsafe` 全局。
static TRACE_DEADLINE: std::sync::Mutex<Option<std::time::Instant>> =
    std::sync::Mutex::new(None);
static TRACE_STAGE: std::sync::Mutex<u32> = std::sync::Mutex::new(0);

/// 埋点录制的一次心跳（`WM_TIMER` / `TRACE_TIMER_ID`）。
///
/// 阶段机（每次进来只推进一步）：
///
///   0 → 1   预热结束，**开** trace（`TRACE_ON = true`）
///   1 → 2   到 deadline，**关** trace 并 dump 到 `$TEMP%`
///
/// 之所以用"阶段 + deadline"而不是"开一只定时器再等"：录制时长是
/// 命令行给的任意毫秒数，而 `SetTimer` 的周期要在挂的时候就知道，
/// 两拍 500ms 的心跳 + 绝对时刻比较，是唯一既能任意时长、又能保证
/// dump 落在**主线程消息处理里**的做法。
///
/// dump 完成后**不退出进程**、不 Kill 掉窗口：留给人工继续操作窗口，
/// 需要再录一次就再跑一遍命令行。录制开关已被 dump 置 false，
/// 后续操作不会再有埋点开销。
fn tick_anim_trace(hwnd: HWND) {
    let stage = match TRACE_STAGE.lock() {
        Ok(mut s) => {
            let cur = *s;
            *s += 1;
            cur
        }
        Err(_) => return,
    };
    match stage {
        0 => {
            // 预热完毕，开录。
            crate::anim_clock::trace_set(true);
        }
        1 => {
            let deadline = TRACE_DEADLINE.lock().ok().and_then(|d| *d);
            if let Some(dl) = deadline {
                if std::time::Instant::now() < dl {
                    // 还不到收网时刻：把阶段退回去，下一拍再判。
                    if let Ok(mut s) = TRACE_STAGE.lock() {
                        *s = 1;
                    }
                    return;
                }
            }
            let path = anim_trace_path();
            let pstr = path.to_string_lossy().to_string();
            // dump_trace 内部会先把 TRACE_ON 置 false 再取缓冲，
            // 所以这里**不需要**再手动关一次（顺序反了会漏掉最后一帧）。
            match crate::anim_clock::dump_trace(&pstr) {
                Ok(n) => {
                    let orphans = crate::anim_clock::trace_orphan_paints();
                    let note = format!(
                        "frames={n} orphan_paints={orphans} path={}\n",
                        pstr
                    );
                    let _ = std::fs::write(format!("{pstr}.done"), note.as_bytes());
                }
                Err(e) => {
                    let _ = std::fs::write(
                        format!("{pstr}.done"),
                        format!("dump failed: {e}\n").as_bytes(),
                    );
                }
            }
            // 收工：撤掉这只定时器，窗口继续正常跑（埋点已关）。
            unsafe {
                let _ = KillTimer(Some(hwnd), TRACE_TIMER_ID);
            }
        }
        _ => {
            unsafe {
                let _ = KillTimer(Some(hwnd), TRACE_TIMER_ID);
            }
        }
    }
}

fn bench_timer() -> i32 {
    use std::sync::Mutex;
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::*;

    static TICKS: Mutex<Vec<u64>> = Mutex::new(Vec::new());

    unsafe extern "system" fn probe(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if msg == WM_TIMER && wp.0 == crate::TIMER_ANIM {
            TICKS.lock().unwrap().push(
                windows::Win32::System::SystemInformation::GetTickCount64());
        } else if msg == WM_DESTROY {
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }

    unsafe {
        let Ok(inst) = GetModuleHandleW(None) else {
            println!("bench::timer  拿不到模块句柄");
            return 1;
        };
        // `WNDCLASSW.lpszClassName` 是裸 `PCWSTR`（不是 HSTRING），
        // `hInstance` 是 `HINSTANCE`（不是 HMODULE）—— 与 `spawn_window` 同款。
        let class: Vec<u16> = "ZenTmrProbe".encode_utf16()
            .chain(std::iter::once(0)).collect();
        let title = HSTRING::from("bench");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(probe),
            hInstance: HINSTANCE(inst.0),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            0, 0, 120, 80,
            None, None, Some(HINSTANCE(inst.0)), None,
        ) else {
            println!("bench::timer  建窗口失败");
            return 1;
        };
        // 与主窗动画定时器**完全相同**的参数
        SetTimer(Some(hwnd), crate::TIMER_ANIM, 16, None);
        let t0 = std::time::Instant::now();
        let mut msg = MSG::default();
        while t0.elapsed().as_millis() < 500 {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_micros(400));
        }
        KillTimer(Some(hwnd), crate::TIMER_ANIM);

        let ticks = TICKS.lock().unwrap();
        let mut d: Vec<u64> = ticks.windows(2).map(|w| w[1]-w[0])
            .filter(|x| *x > 0).collect();
        d.sort_unstable();
        if d.is_empty() {
            println!("bench::timer  没收到 WM_TIMER");
            return 1;
        }
        let med = d[d.len()/2];
        println!("bench::timer  WM_TIMER x{} / 500ms  min={} 中位={} max={} ms  => {:.1} fps",
            ticks.len(), d[0], med, d[d.len()-1], 1000.0/med as f64);
        // <=20ms：16ms 附近（+4ms 调度抖动容差）。
        // 30ms 左右说明被量化到 15.6ms 节拍 —— 这就是动画卡成幻灯片的原因。
        let ok = med <= 20;
        println!("bench::timer  {} WM_TIMER 中位间隔 {}ms（阈值 20ms）",
            if ok { "PASS" } else { "FAIL" }, med);

        // —— 对照：新时钟线程（anim_clock）同样的等待节奏 ——
        let (n, mn, mdmx, mx) = crate::anim_clock::bench_self();
        let ok2 = mdmx > 0 && mdmx <= 20;
        if mdmx > 0 {
            println!("bench::clock  新时钟线程 x{} / 500ms  min={} 中位={} max={} ms  => {:.1} fps",
                n, mn, mdmx, mx, 1000.0 / mdmx as f64);
            println!("bench::clock  {} 新时钟中位间隔 {}ms",
                if ok2 { "PASS" } else { "FAIL" }, mdmx);
        }
        // 两个都过才算过：WM_TIMER 慢是"已知的旧错"（它只用于 bench_timer
        // 自己，动画已经不依赖它了），新时钟必须达标。
        let _ = ok;
        if ok2 { 0 } else { 1 }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // ---------------------------------------------------------------- 插件进程（第65轮）
    //
    // **必须在所有窗口 / 托盘 / 单实例逻辑之前 return**。
    //
    // 插件是"同一个 exe 的另一个进程"：`ZenFile.exe --plugin <id>`。
    // 它绝不能走后面的窗口创建、单实例互斥、托盘注册 —— 那些是**宿主**
    // 的职责。漏掉任何一条的后果：
    //   * 走到单实例转交 → 插件把 id 当路径递给宿主然后自己退出；
    //   * 走到托盘注册 → 每个插件在系统托盘挂一个图标；
    //   * 走到窗口创建 → 插件弹出第二个文件管理器窗口。
    //
    // 所以这里的第一件事就是把 `--plugin` 摘出来，其余参数一概不看。
    if let Some(id) = args
        .iter()
        .position(|a| a == "--plugin")
        .and_then(|i| args.get(i + 1))
        .cloned()
    {
        let code = crate::plugin_host::run_plugin_process(&id, &args);
        std::process::exit(code);
    }

    // -------------------------------------------------- 帧链路埋点录制（第69轮）
    //
    // `ZenFile.exe --anim-trace <毫秒数>`：**带着正常窗口启动**，后台录制
    // 指定的时长，到点把 CSV 写到 `$TEMP%\zenfile_anim_trace.csv`，
    // 然后窗口照常留着继续跑。
    //
    // 为什么**不**像 `--plugin` / `--bench-timer` 那样 early-return 自退：
    // 动画只有在**真实窗口 + 真实绘制**下才存在。凭空造一个 headless
    // 的 tick 循环，测出来的是"理想情况下的链路"，恰恰把我们要查的
    // 排队/合成延迟全绕过去了 —— 那样量出来的数字没有意义。
    // 所以这里只**记下参数**，让流程继续往下走完正常开窗的每一步，
    // 真正常见的是后面 `spawn_anim_trace_timer` 挂的那个 dump 定时器。
    //
    // 关于单实例：**刻意放在 `hand_off_to_running_instance` 之前取参数、
    // 但不 return**，因此单实例逻辑本身一行未改。要配合 `--new-process`
    // 使用（否则会被转交给已有实例，本进程根本走不到开窗）；实测
    // `--new-process --anim-trace 5000` 会正常开出新窗口并录到帧。
    let anim_trace_ms: Option<u64> = args
        .iter()
        .position(|a| a == "--anim-trace")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0);

    if args.iter().any(|a| a == "--bench-timer") {
        // 定时器基准（第68轮）：量 `SetTimer(16)` 的**真实投递间隔**。
        //
        // 为什么要有这个模式：动画卡不卡取决于 `WM_TIMER` 实际多久来一次，
        // 而**任何间接判据都会量错**：
        //   * 用 `Sleep` 量 —— 那测的是**调度延迟**，这台机器上恒为 15.5ms，
        //     与 `SetTimer` 无关（全局分辨率已经是 0.997ms，Sleep 仍 15.5ms）；
        //   * 用画面像素变化量 —— 中间夹着 DWM 合成与抓帧开销，读数偏小。
        //
        // 唯一可信的是**直接在 wnd_proc 里打点**：那正是动画驱动收到消息的时刻。
        unsafe { attach_console() };
        std::process::exit(bench_timer());
    }

    if args.iter().any(|a| a == "--selftest" || a == "--self-test") {
        // Windows 子系统下 CRT 的 stdout 是空的，先把父进程的控制台挂回来
        unsafe { attach_console() };
        let rc = zenfile_core::selftest::run();
        let rc2 = app_selftest();
        std::process::exit(if rc != 0 || rc2 != 0 { 1 } else { 0 });
    }
    if args.iter().any(|a| a == "--version") {
        unsafe { attach_console() };
        println!(
            "{APP_TITLE} {APP_VERSION} (pkg {} / protocol {})",
            env!("CARGO_PKG_VERSION"),
            zenfile_core::PROTOCOL_VERSION
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

    let cfg = match (|| -> zenfile_core::Result<zenfile_core::config::Config> {
        let dir = config::app_dir()?;
        std::fs::create_dir_all(&dir)?;
        zenfile_core::config::Config::load(&dir)
    })() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("配置加载失败：{e}，使用默认值");
            zenfile_core::config::Config::defaults().unwrap()
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

    // 帧链路埋点录制（第69轮）：窗口已建好，现在才开 trace。
    //
    // 放在开窗**之后**才开 trace，而不是进 `main` 就开：开窗那一大段
    // （读配置、建子控件、`open_tab` 首次枚举目录）会占掉几百毫秒，
    // 那段里的重绘跟动画链路无关，录进来只会把分布的尾巴拉脏。
    if let Some(ms) = anim_trace_ms {
        spawn_anim_trace_timer(hwnd, ms);
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
        // 归还定时器精度（第68轮）。必须与 `timeBeginPeriod(1)` 配对 ——
        // 系统把它记成**全局引用计数**，不归还的话这个设置会一直留着，
        // 让整台机器的系统定时器都比原来忙。
    }
}

/// 新建一个主窗口（连同它自己的 `App`）。本进程内可以有任意多个。
///
/// `primary` 表示这是本进程的第一个窗口：全局热键只注册一次（同一个热键
/// 在同一线程里注册两次会失败），托盘由隐藏的托盘宿主窗口负责，与这里无关。
fn spawn_window(
    instance: Option<HINSTANCE>,
    cfg: zenfile_core::config::Config,
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
        // 插件（第65轮）：只在本进程**第一个**窗口发现插件。
        //
        // 每个窗口都 discover 一次会开出 4 个插件进程（开 4 个窗口 = 4 份），
        // 而插件是进程级的共享资源，不该按窗口数复制。
        app.discover_plugins();
    }

    unsafe {
        let _ = SetTimer(Some(hwnd), TIMER_DRAIN, 100, None);
        let _ = SetTimer(Some(hwnd), TIMER_BUSY, 50, None);
        // 第29轮：恢复上次退出时的窗口位置/大小/最大化 —— 只对本进程第一个
        // 窗口做（后面的窗口保持上面的级联错开）。之前完全没这段逻辑，
        // 每次启动都落在 CW_USEDEFAULT 上，用户反馈"窗口位置压根不会保存"。
        let mut restore_max = false;
        if primary {
            let wc = app.cfg.ui.window;
            if wc.w >= 480 && wc.h >= 320 && rect_on_screen(wc.x, wc.y, wc.w, wc.h) {
                let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
                wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
                if GetWindowPlacement(hwnd, &mut wp).is_ok() {
                    wp.rcNormalPosition = RECT {
                        left: wc.x,
                        top: wc.y,
                        right: wc.x + wc.w,
                        bottom: wc.y + wc.h,
                    };
                    // showCmd 不动：此刻窗口还没显示，Get 拿回来的就是
                    // SW_HIDE，原样 Set 回去只改几何，不会把它提前闪出来。
                    if SetWindowPlacement(hwnd, &wp).is_ok() {
                        restore_max = wc.maximized;
                    }
                }
            }
        }
        if show {
            let _ = ShowWindow(hwnd, if restore_max { SW_SHOWMAXIMIZED } else { SW_SHOW });
            let _ = windows::Win32::Graphics::Gdi::UpdateWindow(hwnd);
        }
    }
    Some(hwnd)
}

// —— 第29轮：窗口位置持久化 ————————————————————————————————

/// 上次写盘时的"最大化"状态（-1 = 还没记过）。
///
/// 交互调整大小时 WM_SIZE 会连发几十条，逐条写盘既浪费，还会把**测试
/// 脚本 SetWindowPos 的临时坐标**灌进用户配置。所以判据是"状态变了
/// 或强制"：程序化改位置（状态没变）一律不写；交互结束
/// （WM_EXITSIZEMOVE）与关闭（WM_CLOSE）强制写终值。
static RECT_SAVED_MAX: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// 把窗口的位置/大小/最大化状态写进配置 `ui.window`。
/// 坐标取 `WINDOWPLACEMENT.rcNormalPosition`（还原态矩形）——
/// 最大化时 GetWindowRect 拿到的是撑满屏幕的那个框，存它下次就回不去了。
fn persist_window_rect(hwnd: HWND, force: bool) {
    use std::sync::atomic::Ordering;
    unsafe {
        if IsIconic(hwnd).0 != 0 {
            // 最小化时 IsZoomed 恒为假："最大化 → 最小化 → 关闭"会被
            // 记成非最大化。跳过这次，等状态真正变化时再写。
            return;
        }
        let max = IsZoomed(hwnd).0 != 0;
        let max_i = max as i32;
        let prev = RECT_SAVED_MAX.swap(max_i, Ordering::Relaxed);
        if !force && (prev == -1 || prev == max_i) {
            return;
        }
        let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
        wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
        if GetWindowPlacement(hwnd, &mut wp).is_err() {
            return;
        }
        let r = wp.rcNormalPosition;
        let wc = zenfile_core::config::WindowConfig {
            x: r.left,
            y: r.top,
            w: r.right - r.left,
            h: r.bottom - r.top,
            maximized: max,
        };
        let _ = zenfile_core::config::Config::save_user_patch(&serde_json::json!({
            "ui": { "window": wc }
        }));
    }
}

/// 存下来的矩形是否仍落在某个显示器的可见范围内。
///
/// 换分辨率 / 拔掉副屏之后，旧坐标可能整块跑到屏幕外 —— 那时候原样
/// 恢复等于"窗口不见了"，比恢复默认位置更糟，宁可放弃这次恢复。
fn rect_on_screen(x: i32, y: i32, w: i32, h: i32) -> bool {
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if vw <= 0 || vh <= 0 {
        return false;
    }
    // 与虚拟屏相交，且顶部 60px（标题栏）与左右各 80px 露在屏内 —— 抓得住、点得到
    x < vx + vw - 80 && x + w > vx + 80 && y < vy + vh - 60 && y + h > vy + 60
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
        None => match zenfile_core::config::Config::defaults() {
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
                    .unwrap_or_else(|| zenfile_core::config::Config::defaults().unwrap());
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
                            .unwrap_or_else(|| zenfile_core::config::Config::defaults().unwrap());
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
    // 动画帧由**后台时钟线程**推来（第68轮），不走 WM_TIMER。
    //
    // `SetTimer(16)` 的实测投递间隔是 **31ms**（`--bench-timer`），整 2 倍，
    // 动画因此卡成幻灯片。改用专用线程按 15ms `PostMessage` 主动推帧，
    // 节奏由线程自己的 waitable timer 决定，不被系统时钟节拍量化。
    //
    // 放在框架关键消息**之前**：它要读写 `App`，但 `GWLP_USERDATA` 在
    // 窗口创建后才设 —— 动画时钟只在那之后才启动，所以这里必然已有值。
    //
    // 第69轮埋点：这一分支上依次打了 4 个时刻
    // （进分支 / tick 后 / sync 后 / frame_done 后），**只加打点，
    // 不动任何控制流** —— 返回值、调用顺序、InvalidateRect 的参数全部
    // 与第68轮一致，这样测出来的分布才是"现状的分布"。
    if msg == crate::anim_clock::WM_APP_ANIM_TICK {
        crate::anim_clock::trace_handler_enter();
        let app = unsafe { &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App) };
        if app.tick_anim() {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        crate::anim_clock::trace_tick_end();
        app.sync_anim_timer();
        crate::anim_clock::trace_sync_end();
        // 消费完一帧，腾出 in-flight 名额让时钟线程可以继续 post
        let after = crate::anim_clock::AnimClock::frame_done();
        crate::anim_clock::trace_frame_done(after);
        return LRESULT(0);
    }
    // 框架关键消息必须先于 App 判空守卫处理：GWLP_USERDATA 是在 CreateWindowExW
    // 返回之后才设置的，而 WM_NCCALCSIZE 在创建期间就会到达。若先落入
    // DefWindowProc，客户区几何会在那一刻被系统定型，之后无论返回什么都
    // 无法再移除边框（实测客户区 1164x721 ≠ 窗口 1180x760，自绘内容整体
    // 被原生标题栏下推 30px，这就是"双标题栏"的根因）。
    match msg {
        WM_NCCALCSIZE => return handle_nc_calc_size(hwnd, wparam, lparam),
        WM_NCHITTEST => return handle_hit_test(hwnd, lparam),
        // 第70轮埋点：主窗口的 `WM_ERASEBKGND` 计数。
        //
        // 主窗口这里直接 `return LRESULT(1)`（"擦过了"），所以**它自己不擦**。
        // 记这个计数是为了和两个子 EDIT 的对照：子控件那边每次
        // `InvalidateRect(..., erase=true)` 都会真发一次 `WM_ERASEBKGND`，
        // 主窗口这边则一条都不会有 —— 差值就是"每帧额外擦了几次"。
        WM_ERASEBKGND => {
            crate::anim_clock::trace_child_erase(crate::anim_clock::CHILD_MAIN);
            return LRESULT(1);
        }
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
            // 第70轮：主窗口这一次的 `WM_PAINT` 也进子窗口统计（`CHILD_MAIN`），
            // 口径与两个子 EDIT 完全一致 —— 同一把尺子量三处，才能直接比大小。
            crate::anim_clock::trace_child_paint_enter(crate::anim_clock::CHILD_MAIN);
            // 纯绘制耗时的边界。放在 BeginPaint **之后**、paint 之前 ——
            // BeginPaint 本身要做 UpdateRegion 的校验与裁剪，属于"上屏"那一侧
            // 的成本，不该算进绘制里（否则会把 (c) 混进 paint 耗时）。
            crate::anim_clock::trace_paint_enter();
            paint::paint(app, hdc, rect.right, rect.bottom);
            crate::anim_clock::trace_paint_exit();
            crate::anim_clock::trace_child_paint_exit(crate::anim_clock::CHILD_MAIN);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_SIZE => {
            let app = &mut *app_ptr;
            app.metrics = theme::Metrics::new(GetDpiForWindow(hwnd));
            app.sync_metrics();
            app.last_size = ((lparam.0 & 0xFFFF) as i32, (lparam.0 >> 16) as i32);
            // 第74轮：尺寸变了，缓存的那张全窗口离屏位图当场销毁。
            //
            // 刻意**不等下一帧 lazy 重建**：`lparam` 给的可能是"正在拖动"的
            // 尺寸，于是每次 WM_SIZE 都换一张图，旧的必须立刻还回去 ——
            // 否则从 800x600 拖到 4K 的一路上，进程里会同时存在几十张不同
            // 尺寸的整窗位图（每张几 MB）。而且最小化那一跳的 `0x0` 也要走
            // 这条路，否则最小化期间一直攥着上一次的整窗大图。
            paint::invalidate_back_surface(app);
            paint::reposition(app);
            let _ = InvalidateRect(Some(hwnd), None, true);
            // 位置持久化（第29轮）：状态没变时内部直接跳过，不会逐条写盘
            persist_window_rect(hwnd, false);
            LRESULT(0)
        }
        WM_EXITSIZEMOVE => {
            // 交互式拖动/调整结束 —— 一次性强制写盘。拖动过程中的 WM_SIZE
            // 都被"最大化状态没变"挡掉了，终值在这里落。
            persist_window_rect(hwnd, true);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let app = &mut *app_ptr;
            app.metrics = theme::Metrics::new(GetDpiForWindow(hwnd));
            app.sync_metrics();
            // 第74轮：DPI 一变 `Metrics` 全部重算，量出来的窗口客户区尺寸
            // 通常也跟着变。与其等下一帧 `paint` 里的尺寸判等，不如现在就把
            // 缓存图还回去 —— 拖动跨屏时会连续来好几条 WM_DPICHANGED，
            // 每一张中间尺寸的整窗位图都是几 MB。
            paint::invalidate_back_surface(app);
            paint::reposition(app);
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        // 第74轮：显示模式变了（改分辨率 / 换显示器 / DWM 开关）。DDB 与创建它
        // 时的显示格式绑定，跨格式拿旧位图来 BitBlt 会出现色彩不对，所以
        // 一次性销毁，下一帧按新格式重建。
        WM_DISPLAYCHANGE => {
            let app = &mut *app_ptr;
            paint::invalidate_back_surface(app);
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        // 光标一律自己定，不再退回 DefWindowProc（窗口类的 hCursor 只在
        // DefWindowProc 里生效，而我们全程自己 SetCursor）。
        // 具体怎么算见 `apply_cursor`：位置每次现取，不读 hover_* 快照。
        WM_SETCURSOR => {
            let app = &*app_ptr;
            apply_cursor(hwnd, app);
            LRESULT(1)
        }
        WM_TIMER => {
            // 第68轮：**动画帧不再走这里**。
            //
            // `SetTimer(16)` 的实测投递间隔是 31ms（`--bench-timer` 可复现），
            // 整 2 倍 —— 动画卡成幻灯片。现在动画由 `anim_clock` 的后台线程
            // 用 `WM_APP_ANIM_TICK` 推来（见 wnd_proc 开头）。
            //
            // 这里**不留 TIMER_ANIM 的兜底分支**：动画时钟建不起来时
            // `AnimClock::start` 已经把 `running` 复位，真要兜底也得在那里
            // 显式 `SetTimer`，而不是在这里埋一个会被 `sync_anim_timer`
            // 反复摘挂的定时器（两者会互相打架）。
            if wparam.0 == TIMER_BUSY {
                let app = &mut *app_ptr;
                paint::tick_busy_strip(hwnd, app);
            } else if wparam.0 == TRACE_TIMER_ID {
                // 第69轮埋点录制：两拍开录、到点收网写 CSV。
                tick_anim_trace(hwnd);
            } else if wparam.0 == TIMER_DRAIN {
                let app = &mut *app_ptr;
                // 插件状态机（第65轮）：查进程存活 + 心跳超时，该重启的重启。
                //
                // 挂在 100ms 这只定时器上而不是新建一只：心跳超时是 5 秒，
                // 100ms 的检查频率绰绰有余，省一只定时器。
                //
                // **这一步不能省**：省了的话插件崩溃宿主完全不知道，面板上
                // 会永远显示"运行中"，而插件早就没了。
                if app.tick_plugins() {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                app.drain();
                // 搜索框防抖到期：这一跳才真的去搜（见 App::tick_search）
                if app.tick_search() {
                    let _ = InvalidateRect(Some(hwnd), None, true);
                }
                // 首页是虚拟视图，拿不到目录变更通知 —— 拷完一个大文件之后
                // 容量条会一直停在旧数值上（看着就是"条不准"）。这里做兜底刷新。
                if app.tick_refresh_home() {
                    let _ = InvalidateRect(Some(hwnd), None, true);
                }
                // 拖着文件悬在标签上满 600ms → 自动切到那个标签
                if app.tick_tab_hover() {
                    let _ = InvalidateRect(Some(hwnd), None, true);
                }
                // 自研传输跑完就重列目录（弹窗自己也在轮询同一份 stats）
                app.drain_transfer();
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
                // 第61轮：失活 = **capture 极可能已被别的窗口抢走**，那么
                // WM_LBUTTONUP 永远送不到这里，框选矩形 / 拖拽影像就会
                // 永远赖在画面上（用户截图里那个"空白处多一个半透明蓝框"）。
                cancel_drag(app);
                // 窗口失活时清标题栏按键的悬停/按下态（第36轮用户报
                // "鼠标都移出去了还有背景色"）。这里是最可靠的信号 ——
                // 没有 TrackMouseEvent 时鼠标从非客户区直接划出窗口，
                // 可能一条 WM_MOUSEMOVE 都收不到。
                let dirty = app.hover_caption != 0 || app.hovering_close;
                app.hover_caption = 0;
                app.hovering_close = false;
                if dirty {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        // 第61轮：capture 丢失时收掉拖拽状态。
        //
        // 之前只在 WM_MOUSEMOVE 里靠 GetKeyState 兜底，可松手后鼠标不动，
        // 那条兜底一辈子不触发 —— 截图里"空白处多一个半透明蓝框"就是它。
        //
        // **WM_CAPTURECHANGED 必须转发 DefWindowProc**（第38轮踩过）：
        // 那条消息是系统在鼠标捕获转移时发的，我们 `return 0` 等于告诉系统
        // "这次捕获变化我处理了"，后续点击/菜单全部失灵（实测右键菜单弹不出、
        // 快捷键收不到，整条 GUI 输入链断掉）。清理归清理，转发不能省。
        //
        // 刻意**不**挂 WM_KILLFOCUS：改名编辑框（rename.rs）拿焦点时主窗口
        // 会收到它，挂上去等于"按 F2 改名刚开始就被取消拖拽"；窗口失活那条路
        // 已经由上面 WM_ACTIVATE 的 cancel_drag 覆盖了。
        WM_CANCELMODE => {
            let app = &mut *app_ptr;
            if app.drag.is_some() || app.tab_drag.is_some() {
                cancel_drag(app);
                let _ = InvalidateRect(Some(hwnd), None, true);
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            let app = &mut *app_ptr;
            if app.drag.is_some() || app.tab_drag.is_some() {
                cancel_drag(app);
                let _ = InvalidateRect(Some(hwnd), None, true);
            }
            DefWindowProcW(hwnd, WM_CAPTURECHANGED, wparam, lparam)
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
                // 挂防抖：停手 ~200ms 后由 TIMER_DRAIN 跑真正的整卷查询。
                // 不能在这里直接搜 —— 那样 "wind" 会变成 4 次全卷查询，
                // 而且每敲一键都要等一次磁盘/索引往返，输入框会卡键。
                app.on_search_text_changed();
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
            // 重挂"鼠标离开"跟踪：不挂就永远收不到 WM_MOUSELEAVE，鼠标划出
            // 窗口之后悬停态会一直赖在画面上（见 track_mouse_leave）。
            // **只在没挂上时挂**（第54轮）：每次移动都重挂的话，光标从窗口边框
            // （非客户区）划进客户区的那一下，系统会把"离开了客户区"的滞回
            // 事件补发成一次 WM_MOUSELEAVE —— 刚设好的悬停当场被清掉，鼠标
            // 停住不动就再也亮不起来（"悬停效果没了"真因）。
            track_mouse_leave(app, hwnd);
            let l = paint::layout_for(app);
            // 圆角滑杆拖动。**必须排在 settings_drag 判定之前**：按下滑杆时
            // 只置了 radius_drag 标志、并没有设 settings_drag —— 早先这段
            // 处理整个被包在 `settings_drag.is_some()` 里，于是按下能写进
            // 一次值、按住拖全程无反应（用户报的"圆角改了没反应"就是它）。
            if app.tab().settings && app.settings_radius_drag {
                // 兜底：捕获可能被弹出的菜单/别的窗口抢走，那条 WM_LBUTTONUP
                // 就到不了这里。左键已抬起就当场收摊并**继续走正常命中**
                //（不 return），否则标志永真 —— 光标扫过设置页就跟着改值 +
                // 反复写盘，设置页还点不动（第39轮审计 #11）。
                if GetKeyState(0x01) >= 0 {
                    app.settings_radius_drag = false;
                    let _ = ReleaseCapture();
                } else {
                    let id = app.settings_radius_sel.clone();
                    let g = settings::geom(app, &l);
                    let max = settings::radius_max(&id).max(1);
                    let w = (g.slider.right - g.slider.left).max(1);
                    let frac = ((x - g.slider.left) as f32 / w as f32).clamp(0.0, 1.0);
                    app.set_radius(&id, (frac * max as f32).round() as u32);
                    let _ = InvalidateRect(Some(hwnd), None, true);
                    return LRESULT(0);
                }
            }
            // 设置页排布编辑器：正在拖卡片，只更新位置并重画，别的命中一概不理。
            if app.tab().settings && app.settings_drag.is_some() {
                if let Some(d) = &mut app.settings_drag {
                    d.cur = (x, y);
                }
                let _ = InvalidateRect(Some(hwnd), None, false);
                return LRESULT(0);
            }
            // 正在拖**标签**：算去处（换顺序 / 8 向贴 / 拖出窗口）并重画。
            // 也要排在别的命中之前：拖标签期间不该去动选中集。
            if app.tab_drag.is_some() {
                update_tab_drag(app, hwnd, &l, x, y);
                return LRESULT(0);
            }
            // 正在拖条目 / 拉框选：更新当前点、重算投放目标并重画。
            // 必须排在别的命中之前 —— 拖拽期间不该改变选中集。
            if app.drag.is_some() {
                // 兜底（第54轮）：捕获被弹出的菜单/对话框抢走的话，WM_LBUTTONUP
                // 永远到不了这里，框选矩形就**永远赖在画面上**（用户截到的"半透
                // 明蓝块"就是它），期间悬停也被这条分支吞掉。左键已经不在按下
                // 态 = 松手消息丢了，就地收摊，然后继续往下走正常悬停。
                if GetKeyState(0x01) >= 0 {
                    finish_drag(app, &l, x, y);
                    let _ = ReleaseCapture();
                    let _ = InvalidateRect(Some(hwnd), None, true);
                }
                let ctrl_now = (wparam.0 & 0x0008) != 0 || GetKeyState(0x11) < 0;
                let was_active = app.drag.as_ref().map(|d| d.active).unwrap_or(false);
                app.update_drag(x, y, ctrl_now);
                // 框选要**边拖边更新选中集**：不更新用户拖到哪条都没反应，
                // 松开才一次性全亮（资源管理器是每帧都更新）。只对 Marquee
                // 调 select_marquee（Items 拖的是条目，不该改选中）。
                // 必须 drag.active 已置位之后才算（4px 阈值内还在按，没真拖）。
                if let Some(d) = app.drag.as_ref() {
                    if d.active && matches!(d.kind, app::DragKind::Marquee) {
                        // 把 d 的不可变借用放掉再调（select_marquee 要 &mut App）。
                        // 只需要 origin/cur 两个 i32，clone 一下比整体 clone Drag 轻。
                        let origin = d.origin;
                        let cur = d.cur;
                        select_marquee_into(app, &l, origin, cur);
                    }
                }
                // 投放目标（描边/悬停标签自动翻开）只属于**拖文件**。框选不是
                // 拖放：早先这里不分种类一律算 target，于是拉框选的过程中，
                // 光标下面那一行被描上一圈 accent 边 —— 细节视图的行是全宽的，
                // 描边顶边就是一条横贯整个内容区的蓝线（用户截图里那条）。
                let target = match app.drag.as_ref().map(|d| d.kind) {
                    Some(app::DragKind::Items) => drop_target_at(app, &l, x, y),
                    _ => None,
                };
                // 拖着文件悬在某个标签上：记下"从第几毫秒开始悬着"，
                // 满 600ms 由 WM_TIMER 自动把那个标签翻出来（资源管理器同款）。
                if let Some(t) = &target {
                    if let app::DropKind::Tab(i) = t.kind {
                        if app.tab_hover_since.map(|(j, _)| j) != Some(i) {
                            app.tab_hover_since = Some((i, std::time::Instant::now()));
                        }
                    } else {
                        app.tab_hover_since = None;
                    }
                } else {
                    app.tab_hover_since = None;
                }
                app.drop_target = target;
                // 拖着**一个文件夹**、又没压在任何可投放目标上，再看它是不是悬在
                // 内容区边缘 —— 是的话这次松手要的是"开个新窗格"（分栏），不是搬文件。
                // 只有真的悬在边缘带里才算，落在正中间仍然是"什么也不做"，
                // 否则随手一放就分栏会让人莫名其妙。
                app.drag_dock = None;
                if app.drop_target.is_none() {
                    if let Some(d) = app.drag.as_ref() {
                        if d.active && d.items.len() == 1 && d.items[0].is_dir() {
                            let pl = if app.split() {
                                match paint::pane_index_at(app, &l, x, y) {
                                    Some(pi) => paint::pane_layout(app, &l, pi),
                                    None => l,
                                }
                            } else {
                                l
                            };
                            if x >= pl.view.left
                                && x < pl.view.right
                                && y >= pl.view.top
                                && y < pl.view.bottom
                            {
                                let z = paint::dock_zone_at(&pl, x, y);
                                if z != crate::app::DockZone::Center {
                                    app.drag_dock = Some(z);
                                }
                            }
                        }
                    }
                }
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
            // 正在拖窗格分隔线：按相对按下点的拖动量重新分配两个窗格的宽/高
            if let Some(drag) = app.pane_div_drag.take() {
                let outer = l.view;
                let span = if drag.vertical {
                    (outer.right - outer.left).max(1) as f32
                } else {
                    (outer.bottom - outer.top).max(1) as f32
                };
                let cur = if drag.vertical {
                    (x - outer.left) as f32 / span
                } else {
                    (y - outer.top) as f32 / span
                };
                let d = (cur - drag.start_n).clamp(-0.9, 0.9);
                // 最小尺寸：任一边不窄于内容区的 12%，否则一拖就把某个窗格挤没
                let min = 0.12f32;
                let (ra, rb) = (drag.ra, drag.rb);
                if drag.vertical {
                    // a 的右沿 + d = 新分隔线位置；a 占 [a.left, 新位置]，b 占 [新位置, b.right]
                    let mut nx = ra.x + ra.w + d;
                    nx = nx.max(ra.x + min).min(ra.x + ra.w + rb.w - min);
                    app.panes[drag.a].rect.w = nx - ra.x;
                    app.panes[drag.b].rect.x = nx;
                    app.panes[drag.b].rect.w = (ra.x + ra.w + rb.w) - nx;
                } else {
                    let mut ny = ra.y + ra.h + d;
                    ny = ny.max(ra.y + min).min(ra.y + ra.h + rb.h - min);
                    app.panes[drag.a].rect.h = ny - ra.y;
                    app.panes[drag.b].rect.y = ny;
                    app.panes[drag.b].rect.h = (ra.y + ra.h + rb.h) - ny;
                }
                app.pane_div_drag = Some(drag);
                let _ = InvalidateRect(Some(hwnd), None, true);
                return LRESULT(0);
            }
            // 分隔线悬停（只为了把光标换成 ↔）
            let hover_split = paint::split_hit(app, &l, x, y);
            // 窗格分隔线悬停：换 ↔ / ↕ 光标并高亮那条线
            let hover_pane_div = paint::pane_div_at(app, &l, x, y);
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
            // 第63轮：`+`（新建标签）与标签上的 `×` 各自的悬停。
            //
            // **两者都必须独立于 `hover_tab` 判**：`hover_tab` 在鼠标落在标签
            // **任意位置**时都为真，若拿它当 `×` 的悬停判据，鼠标离 `×` 还有
            // 一个字宽时 `×` 就亮了，而真压在 `×` 上时和"标签悬停"分不出来。
            // 两个悬停必须有各自的命中矩形。
            let hover_tab_close = if y >= l.tabs.top && y <= l.tabs.bottom {
                let pn = pl_tab_count(app);
                (0..pn).find(|&i| inside(&l.tab_close_rect(i, pn, &app.metrics), x, y))
            } else {
                None
            };
            let hover_new_tab = {
                let plus = l.new_tab_rect(pl_tab_count(app), &app.metrics);
                l.new_tab_visible && plus.right < l.tabs.right && inside(&plus, x, y)
            };
            // 目录栏最右侧「批量重命名」按钮的悬停。虚拟视图里不画，就也不该有悬停态
            let hover_bulk = paint::bulk_rename_available(app) && inside(&l.bulk_btn, x, y);
            // 内容区里鼠标指着的**条目**（条目下标，不是行号）。
            // 行号会随滚动/排序变，用它记 hover 会"指着 A 却高亮了 B"。
            //
            // 分栏时几何和条目清单都取**光标所在窗格**那一份：绘制端 paint_panes
            // 逐窗格把 view 覆写成「窗格矩形 + 26px 局部标签条」再画，拿全局
            // l.view 算悬停 —— y 带偏一条标签条、x 原点偏半扇窗格，高亮就落在
            // 旁边的行上（用户报的"点击错位"，悬停同根）。做法与绘制端一致：
            // 临时把 focus_pane 指到光标那个窗格，算完还原（hover 不能抢焦点）。
            let hover_pane_saved = app.focus_pane;
            if app.split() {
                if let Some(pi) = paint::pane_index_at(app, &l, x, y) {
                    app.focus_pane = pi;
                }
            }
            let hpl = paint::pane_layout(app, &l, app.focus_pane);
            let hover_row = if x >= hpl.view.left
                && x < hpl.view.right
                && y >= hpl.view.top
                && y < hpl.view.bottom
            {
                if app.tab().home {
                    home_entry_at(app, &hpl, x, y)
                } else if y >= hpl.view.top + header_height(app) {
                    view_row_at(app, &hpl, x, y, hpl.view.top + header_height(app))
                        .and_then(|r| app.tab().visible.get(r).copied())
                } else {
                    None
                }
            } else {
                None
            };
            let hover_pane = if hover_row.is_some() { Some(app.focus_pane) } else { None };
            // 状态栏布局切换按钮悬停
            let hover_view = if app.cfg.ui.statusbar_visible {
                match paint::status_view_hit(&l, &app.metrics, x, y) {
                    Some(zenfile_core::ViewMode::LargeIcons) => 1u8,
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
                    app.settings_hover_find = false;
                    app.settings_prev_hover = None;
                    let mut ht = None;
                    let mut hc = None;
                    let mut hr = None;
                    let mut htree = None;
                    match settings::hit(app, &hpl, x, y) {
                        Some(settings::Hit::Tab(i)) => ht = Some(i),
                        Some(settings::Hit::Card(i)) => {
                            hc = settings::geom(app, &hpl).card_ids.get(i).copied()
                        }
                        Some(settings::Hit::Chip(i)) => {
                            hc = settings::geom(app, &hpl).chip_ids.get(i).copied()
                        }
                        Some(settings::Hit::Row(a)) => hr = Some(a),
                        Some(settings::Hit::Tree(i)) => {
                            htree = Some(i);
                        }
                        Some(settings::Hit::MenuRow(i)) => {
                            app.settings_hover_menu =
                                settings::geom(app, &hpl).menu_idx.get(i).copied();
                        }
                        Some(settings::Hit::OpenWith(_)) => {}
                        Some(settings::Hit::OwFind) => {
                            app.settings_hover_find = true;
                        }
                        Some(settings::Hit::CtxKind(_)) => {}
                        Some(settings::Hit::Preview(i)) => {
                            app.settings_prev_hover = Some(i);
                        }
                        Some(settings::Hit::PreviewArea) => {
                            // 空白处：清掉"停在哪一格上"，但保持"在这一块里"
                            app.settings_prev_hover = None;
                            app.settings_hover_menu = None;
                        }
                        Some(settings::Hit::Canvas(_)) => {}
                        None => {
                            app.settings_prev_hover = None;
                            app.settings_hover_menu = None;
                        }
                        Some(settings::Hit::RadiusSlider(_)) => {}
                        None => {}
                    }
                    if app.settings_hover_tree != htree {
                        app.settings_hover_tree = htree;
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
                paint::recycle_header_hit(app, &hpl, x, y)
            } else {
                paint::header_col_hit(app, &hpl, x, y)
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
                    if x >= hpl.view.left && x < hpl.view.right && y >= hpl.view.top {
                        paint::recycle_hit(app, &hpl, y)
                    } else {
                        None
                    },
                    paint::recycle_button_hit(app, &hpl, x, y),
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
            // 上面这一整段悬停计算临时把 focus_pane 指到光标所在窗格（分栏时），
            // 算完必须还原 —— hover 不能改"当前窗格"，否则鼠标扫过隔壁窗格
            // 就把焦点抢走了。
            app.focus_pane = hover_pane_saved;
            if in_close != app.hovering_close
                || chrome != app.hover_chrome
                || hover_tab != app.hover_tab
                || hover_view != app.hover_view_btn
                || hover_setting != app.settings_hover
                || hover_sidebar != app.sidebar_hover
                || hover_addr != app.addr_hover
                || hover_split != app.hover_split
                || hover_pane_div != app.hover_pane_div
                || hover_bulk != app.hover_bulk
                || hover_row != app.hover_row
                || hover_pane != app.hover_pane
                || hover_nav != app.hover_nav
                || hover_header != app.hover_header
                || hover_rec != app.hover_recycle
                || hover_rec_btn != app.hover_recycle_btn
                || hover_set_tab != app.settings_hover_tab
                || hover_set_card != app.settings_hover_card
                || hover_tab_close != app.hover_tab_close
                || hover_new_tab != app.hover_new_tab
            {
                app.settings_hover_tab = hover_set_tab;
                app.settings_hover_card = hover_set_card;
                app.hover_nav = hover_nav;
                app.hover_header = hover_header;
                app.hover_recycle = hover_rec;
                app.hover_recycle_btn = hover_rec_btn;
                app.hover_bulk = hover_bulk;
                app.hover_row = hover_row;
                app.hover_pane = hover_pane;
                app.hover_split = hover_split;
                app.hover_pane_div = hover_pane_div;
                app.hovering_close = in_close;
                app.hover_chrome = chrome;
                app.hover_tab = hover_tab;
                app.hover_view_btn = hover_view;
                app.settings_hover = hover_setting;
                app.sidebar_hover = hover_sidebar;
                app.addr_hover = hover_addr;
                app.hover_tab_close = hover_tab_close;
                app.hover_new_tab = hover_new_tab;
            // 三大金刚键的 hover **只**在 WM_NCMOUSEMOVE 里更新（那才是
            // 非客户区消息）。鼠标从标题栏划进客户区时走的是这条
            // WM_MOUSEMOVE，而这里原本没清 `hover_caption` ——
            // 于是底色永远卡住，鼠标移到窗口外也不消失（第36轮用户报）。
            //
            // **判据必须是"鼠标不在任何键上"**而不是"收到 WM_MOUSEMOVE"：
            // 客户区和非客户区是两个消息域，鼠标在两者之间来回时两个
            // move 消息交替到达，早先一收到就清零，结果从标题栏往里划
            // 还没划到键上就被清掉了 —— 悬停底色反而全没了（第36轮踩过）。
            let cap = caption_hit(&paint::layout_for(app), x, y);
            if app.hover_caption != 0 && cap == 0 {
                app.hover_caption = 0;
                app.hovering_close = false;
                if app.pressed_caption != 0 {
                    app.pressed_caption = 0;
                }
            }
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            // 悬停态刚被改写过：光标按新状态再定一次，别等下一次移动。
            apply_cursor(hwnd, app);
            LRESULT(0)
        }
        // —— 标题栏三大金刚键 ——
        // 客户区已经扩展到整窗，系统的非客户区默认处理不再可靠（实测三个键点了没
        // 反应）。这里自己接管：按下记状态、抬起执行、双击最大化等同按下两次。
        WM_NCMOUSEMOVE => {
            let app = &mut *app_ptr;
            let (x, y) = screen_xy(hwnd, lparam);
            let l = paint::layout_for(app);
            let h = caption_hit(&l, x, y);
            if h != app.hover_caption {
                app.hover_caption = h;
                app.hovering_close = h == 3;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        // 鼠标完全离开窗口：三大金刚键的悬停/按下态必须一次清干净。
        // 非客户区与客户区是两个跟踪域，两条 leave 都要接。
        //
        // **绝对不能顺手把 WM_CAPTURECHANGED 放进来**（第38轮踩过）：
        // 那条消息是系统在鼠标捕获转移时发的，我们 return 0 就等于
        // 告诉系统"这次捕获变化我处理了" —— 后续的点击/菜单全部失灵
        // （实测：右键菜单弹不出来、快捷键收不到，整个 GUI 输入链断掉）。
        // **这两条必须分开处理**（第54轮"悬停效果没了"真因）：
        // `WM_NCMOUSELEAVE` 不只在整个窗口失去鼠标时来 —— 光标从窗口边框
        // （非客户区）**划进客户区**的那一下也会来（离开非客户区=进入客户区）。
        // 早先把它和 WM_MOUSELEAVE 并成一个分支全清悬停，等于每次从边缘进
        // 内容区都把悬停清光；鼠标停住不动就再也没有 WM_MOUSEMOVE 来补救，
        // 画面上就是"悬停哪一行都不亮"。
        WM_NCMOUSELEAVE => {
            let app = &mut *app_ptr;
            // 只清**非客户区**自己的悬停态（三大金刚键）。客户区的悬停归
            // WM_MOUSEMOVE 管：真划出窗口时紧接着还有 WM_MOUSELEAVE 收尾。
            app.hover_caption = 0;
            app.pressed_caption = 0;
            app.hovering_close = false;
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            let app = &mut *app_ptr;
            // 跟踪已消费：允许下一次 WM_MOUSEMOVE 重新挂上
            app.mouse_tracked = false;
            // 鼠标**整个**离开窗口：全部悬停态一次清干净。以前只清标题栏那三个
            // 键，于是鼠标划出窗口之后内容区那一行还亮着、分隔线还保持高亮、
            // 侧栏的行也还反着白 —— 回到窗口之前画面一直是旧的。
            // 拖拽状态不清：那是按下之后才有的，鼠标划出去不该打断它。
            app.clear_hover();
            // 光标也跟着复位（离开时还挂着 ↔ 的话，回来之前一直是拉伸图标）
            apply_cursor(hwnd, app);
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_NCLBUTTONDOWN => {
            let app = &mut *app_ptr;
            let (x, y) = screen_xy(hwnd, lparam);
            let l = paint::layout_for(app);
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
            let l = paint::layout_for(app);
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
            let l = paint::layout_for(app);
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
            let l = paint::layout_for(app);
            // 地址栏编辑态：点它**以外**的任何地方，先收掉输入框再继续
            // 处理这次点击（第40轮用户报"无法退出输入模式"）。EDIT 是子
            // 窗口，点击父窗口客户区并不会让它失去焦点 —— WM_KILLFOCUS
            // 根本不来，退出只剩 Esc 一条路（像素探针实锤：点列表后仍是
            // 活动选中态）。收掉之后这次点击照常生效（点行选中行），与
            // 资源管理器一致。
            if app.addr_editing && !inside(&l.address, x, y) {
                paint::end_address_edit(app, true);
                paint::sync_address(app);
            }
            // 搜索框同理（第41轮用户报"点搜索栏外的位置关不掉了"）：子 EDIT
            // 的通病——点父窗口客户区**不会让它失焦**，WM_KILLFOCUS 不来，
            // 光标永远闪在框里、之后敲的字全进搜索框、快捷键还被 WM_KEYDOWN
            // 的"焦点在 EDIT"分支吞掉。焦点探针实锤：点列表后 GetFocus 仍是
            // 搜索框。点在框上（含它那圈边框）不算——那是框自己的地盘。
            if !inside(&l.search, x, y) {
                unsafe {
                    if windows::Win32::UI::Input::KeyboardAndMouse::GetFocus() == app.hwnd_search
                        && !app.hwnd_search.0.is_null()
                    {
                        let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(app.hwnd));
                    }
                }
            }
            // 分栏时"点哪一边哪一边就是当前窗格"。必须在任何命中测试之前做：
            // 下面所有分支读的都是 app.tab()，也就是**焦点窗格**的那个标签。
            if app.split() {
                if let Some(pi) = paint::pane_index_at(app, &l, x, y) {
                    app.focus_pane = pi;
                }
            }
            // 内容区的几何同样取**焦点窗格**覆写过的那份（view = 窗格矩形 +
            // 26px 局部标签条）。下面各分支读 app.tab()（焦点窗格的标签）却拿
            // 全局 l.view 算行/表头/磁贴位置 —— y 带偏一条标签条、x 原点偏
            // 半扇窗格，于是"点这行选中的是旁边那行"（第45轮分窗点击错位）。
            // 工具栏/侧边栏/状态栏不在窗格里，pane_layout 原样透传，照用 l。
            let pl = if app.split() { paint::pane_layout(app, &l, app.focus_pane) } else { l };
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
                if let Some(h) = settings::hit(app, &pl, x, y) {
                    // 「右键菜单」页那块实时菜单：按住就能拖着改顺序
                    // （卡片排布那套在别的页照旧）
                    if settings::begin_drag(app, &pl, &h, x, y) {
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
            // 窗格之间的分隔线：按下开始拖，自由调整两个窗格的宽/高。
            // 必须排在标签/内容命中之前 —— 它压在两个窗格的公共边上。
            if let Some((vertical, a, b)) = paint::pane_div_at(app, &l, x, y) {
                let outer = l.view;
                let n = if vertical {
                    (x - outer.left) as f32 / (outer.right - outer.left).max(1) as f32
                } else {
                    (y - outer.top) as f32 / (outer.bottom - outer.top).max(1) as f32
                };
                app.pane_div_drag = Some(app::PaneDivDrag {
                    vertical,
                    a,
                    b,
                    start_n: n,
                    ra: app.panes[a].rect,
                    rb: app.panes[b].rect,
                });
                app.hover_pane_div = Some((vertical, a, b));
                SetCapture(hwnd);
                let _ = InvalidateRect(Some(hwnd), None, false);
                return LRESULT(0);
            }
            // 标签栏的命中**必须排在设置页/回收站之前**：那两个分支处理完就
            // 直接 return 了，放在前面的话一打开设置整个标题栏就点不动
            // （表现：打开设置后切不了标签、也关不掉标签）。
            if let Some(i) = hit_tab(app, &l, x, y) {
                // 关闭按钮压在标签右上角，命中优先级必须高于"切换标签"，
                // 否则每次点 × 都会先切一次标签（然后才关掉别的）。
                // 点在**顶部标题栏那条**（单窗格 / 分栏总览）→ 用全局布局算
                // × 的位置；点在窗格自己那条 26px strip 里 → 用窗格布局。
                let on_top = y >= l.title.top && y <= l.title.bottom;
                let pl = if on_top {
                    l.clone()
                } else if app.split() {
                    paint::pane_layout(app, &l, app.focus_pane)
                } else {
                    l.clone()
                };
                if inside(&pl.tab_close_rect(i, pl_tab_count(app), &app.metrics), x, y) {
                    app.close_tab(i);
                } else {
                    app.pane_mut().active = i;
                    // 记一份"可能在拖标签"。真进入拖拽态要等鼠标越过阈值
                    // （见 WM_MOUSEMOVE）—— 普通单击也会抖几像素。
                    app.begin_tab_drag(app.focus_pane, i, x, y);
                    SetCapture(hwnd);
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
                match settings::hit(app, &pl, x, y) {
                    Some(settings::Hit::Tab(i)) => {
                        app.settings_tab = i;
                        app.settings_hover_card = None;
                        // 「右键菜单」那一页要问 Shell 拿真实的"打开方式"程序，
                        // 切进去的时候算一次，别放到每帧里。
                        if i == crate::app::SETTINGS_TAB_MENU {
                            app.ensure_openwith_preview();
                        }
                        let _ = InvalidateRect(Some(hwnd), None, true);
                    }
                    Some(settings::Hit::CtxKind(i)) => {
                        // 切「右键落在什么上」：样本、候选、菜单全都要跟着换
                        if app.settings_ctx_kind != i {
                            app.settings_ctx_kind = i;
                            app.openwith_preview = None;
                            app.menu_preview_items = None;
                            app.settings_ow_found = false;
                            app.settings_hover_menu = None;
                            app.ensure_openwith_preview();
                        }
                        let _ = InvalidateRect(Some(hwnd), None, true);
                    }
                    Some(settings::Hit::OwFind) => {
                        // 「查找可用的打开软件」：把本机装着的程序全补进这一栏。
                        // 关联的只有几条，装了却没抢过关联的那些全在后面 —— 这一按
                        // 就是把它们列出来（按 exe 名去重，已列的不重复出现）。
                        let have: std::collections::HashSet<String> = app
                            .openwith_preview
                            .as_ref()
                            .map(|v| v.iter().map(|(n, _)| n.to_lowercase()).collect())
                            .unwrap_or_default();
                        let mut list = app.openwith_preview.clone().unwrap_or_default();
                        for a in crate::appfind::installed() {
                            if have.contains(&a.name.to_lowercase()) {
                                continue;
                            }
                            list.push((a.name.clone(), a.icon));
                        }
                        app.openwith_preview = Some(list);
                        app.settings_ow_found = true;
                        let _ = InvalidateRect(Some(hwnd), None, true);
                    }
                    Some(settings::Hit::Tree(i)) => {
                        let g = settings::geom(app, &pl);
                        if app.settings_tab == crate::app::SETTINGS_TAB_INTERACT {
                            // 「交互」页的树（第34轮）：同一份 Hit::Tree，按页分派。
                            // 分组行 = 展开/收起；叶子行 = 选中它（右侧参数区跟着换）。
                            if let Some(id) = g.tree_ids.get(i).cloned() {
                                if settings::interact_has_kids(&id) {
                                    // 分组：展开/收起（集合存的是 &'static str，
                                    // 这里把运行期 String 换回静态串）
                                    if let Some(sid) = settings::interact_id_static(&id) {
                                        if !app.settings_interact_open.remove(sid) {
                                            app.settings_interact_open.insert(sid);
                                        }
                                    }
                                } else if let Some(sid) = settings::interact_id_static(&id) {
                                    app.settings_interact_sel = Some(sid);
                                }
                            }
                            let _ = InvalidateRect(Some(hwnd), None, true);
                            return LRESULT(0);
                        }
                        if let Some(id) = g.tree_ids.get(i).cloned() {
                            // 点父节点 = 展开/收起；点叶子 = 只是选中。
                            // 两种都把滑杆指过去，于是顺手就能调。
                            if settings::radius_has_kids(&id) {
                                if !app.settings_tree_open.remove(&id) {
                                    app.settings_tree_open.insert(id.clone());
                                }
                            }
                            app.settings_radius_sel = id;
                        }
                        let _ = InvalidateRect(Some(hwnd), None, true);
                    }
                    Some(settings::Hit::RadiusSlider(sx)) => {
                        app.settings_radius_drag = true;
                        // **必须 SetCapture**（第39轮审计 #11）：不捕获时把
                        // 滑杆拖出窗口外松手，WM_LBUTTONUP 收不到 → 标志永真，
                        // 之后光标扫过设置页就跟着改值 + 反复写盘，设置页还
                        // 点不动。与 drag_split / tab_drag 同款做法。
                        SetCapture(hwnd);
                        let id = app.settings_radius_sel.clone();
                        let g = settings::geom(app, &pl);
                        let max = settings::radius_max(&id).max(1);
                        let w = (g.slider.right - g.slider.left).max(1);
                        let frac = ((sx - g.slider.left) as f32 / w as f32).clamp(0.0, 1.0);
                        app.set_radius(&id, (frac * max as f32).round() as u32);
                        let _ = InvalidateRect(Some(hwnd), None, true);
                    }
                    // 「交互」页的竖线参数步进（4001..4011）。必须排在
                    // 下面 3000+/2000+/1000+ 的 guard **之前** —— 否则会先
                    // 被 >=3000 拦走当成系统菜单开关。
                    Some(settings::Hit::Row(action)) if (4001..=4099).contains(&action) => {
                        settings::apply_sel_bar_action(app, action);
                        let _ = InvalidateRect(Some(hwnd), None, true);
                    }
                    // 图标主题（设置-外观页）：4101=默认(系统)，4102=优化。
                    // **必须排在下面 >= 3000 的系统菜单分支之前**，否则会被当成
    // 注册表开关（第35轮同款顺序坑）。
                    Some(settings::Hit::Row(action)) if (4101..=4102).contains(&action) => {
                        match action {
                            4101 => app.set_icon_theme("default"),
                            _ => app.set_icon_theme("optimized"),
                        }
                        let _ = InvalidateRect(Some(hwnd), None, true);
                    }
                    Some(settings::Hit::Row(action)) if action >= 3000 => {
                        // 系统右键菜单开关：直接写 HKCU，写完重画（开关状态从注册表读）
                        let kind = (action - 3000) as usize;
                        let on = settings::system_menu_enabled(kind);
                        settings::set_system_menu(kind, !on);
                        let _ = InvalidateRect(Some(hwnd), None, true);
                    }
                    Some(settings::Hit::Row(action)) if action >= 2000 => {
                        // 快捷键行：进"录制"态，下一次按键写进去
                        let idx = (action - 2000) as usize;
                        let key = settings::HOTKEY_LABELS.get(idx).map(|(k, _)| k.to_string());
                        app.hotkey_capture = key;
                        let _ = InvalidateRect(Some(hwnd), None, true);
                    }
                    Some(settings::Hit::Row(action)) if action >= 1000 => {
                        // 插件行：动作编号 1000 + **当前槽位下标**（第65轮）。
                        //
                        // 之前这里是 `app.cfg.modules.keys().nth(idx)` —— 那份
                        // 配置从来没人填过内容，所以这个开关点下去**永远什么
                        // 都不发生**，而面板上显示的是编出来的 `fm-{id}.exe`。
                        // 现在按运行时槽位反查真实 id，再切 `app.plugins`。
                        settings::plugin_toggle(app, (action - 1000) as usize);
                        let _ = InvalidateRect(Some(hwnd), None, true);
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
                if let Some(col) = paint::recycle_header_hit(app, &pl, x, y) {
                    app.sort_recycle_by(col);
                    let _ = InvalidateRect(Some(hwnd), None, true);
                    return LRESULT(0);
                }
                let btns = paint::recycle_buttons(app, &pl);
                if inside(&btns[0], x, y) {
                    app.run_command(menu::Command::RecycleRestore);
                } else if inside(&btns[1], x, y) {
                    app.run_command(menu::Command::RecycleDelete);
                } else if inside(&btns[2], x, y) {
                    app.run_command(menu::Command::RecycleEmpty);
                } else if let Some(idx) = paint::recycle_hit(app, &pl, y) {
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
                        // 搜索芯片：纯状态展示（Pulse 实测点击无反应），
                        // 但点了给它一个合理去处 —— 焦点交回搜索框改词。
                        // 绝不能走下面的 navigate：芯片 target 是 None，
                        // 会被 unwrap_or 当成"点回主页"。
                        if bar.crumbs.get(i).map(|c| c.chip).unwrap_or(false) {
                            paint::focus_edit(app.hwnd_search);
                            return LRESULT(0);
                        }
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
            if app.tab().home && y >= pl.view.top {
                let hit = home_select(app, &pl, x, y);
                // 空白处起手同样要能拉框选 —— 首页之前直接 return，于是"长按框选
                // 在首页不生效"。磁贴布局不是行列式的，框选命中走 home_layout
                // 的磁贴矩形（见 select_marquee）。
                if !hit && !ctrl && !shift {
                    app.tab_mut().selected.clear();
                }
                // **点空白要把焦点态一起撤掉**（与列表视图同一句）：磁贴的
                // 高亮是 `sel || focused` 两条腿，只清 selected 的话焦点那一条
                // 还撑着，选中看着"消不掉"（用户反馈：从磁盘退出到主页，
                // 选中磁盘再点空白 = 高亮不消失）。
                if !hit {
                    app.focus_visible = false;
                }
                app.begin_drag(x, y, app::DragKind::Marquee, ctrl);
                SetCapture(hwnd);
                let _ = InvalidateRect(Some(hwnd), None, true);
                return LRESULT(0);
            }
            let header_h = header_height(app);
            let view_top = pl.view.top + header_h;
            if y >= pl.view.top && y < view_top && app.tab().view_mode == zenfile_core::ViewMode::Details {
                // 先判列最右端那个「筛选」小三角：它压在列里，先判整列的话
                // 点三角永远只会变成"再排一次序"。
                if let Some(col) = paint::header_col_hit(app, &pl, x, y) {
                    if inside(&paint::header_filter_rect(app, &pl, col), x, y) {
                        show_header_filter_menu(hwnd, app, &pl, col);
                        return LRESULT(0);
                    }
                }
                header_click(app, &pl, x);
                return LRESULT(0);
            }
            if let Some(row) = view_row_at(app, &pl, x, y, view_top) {
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
            // 圆角滑杆的拖拽到此为止（按住不放的那段由 WM_MOUSEMOVE 连续改值）
            {
                let app = &mut *app_ptr;
                if app.settings_radius_drag {
                    app.settings_radius_drag = false;
                    let _ = ReleaseCapture(); // 与按下时的 SetCapture 成对
                    apply_cursor(hwnd, app); // 松手时光标立刻复位，不等下一次移动
                }
            }
            // 拖标签：松手就落地（换顺序 / 8 向贴 / 拆成新进程）。
            // 必须排在最前面 —— 拖标签期间别的分支都不该插手。
            {
                let app = &mut *app_ptr;
                if app.tab_drag.is_some() {
                    end_tab_drag(app, hwnd);
                    let _ = ReleaseCapture();
                    apply_cursor(hwnd, app);
                    return LRESULT(0);
                }
            }
            // 设置页排布编辑器：松手落地（收进「更多」/ 插到新位置 / 拖回恢复）
            {
                let app = &mut *app_ptr;
                if app.settings_drag.is_some() {
                    let _ = ReleaseCapture();
                    let l = paint::layout_for(app);
                    settings::finish_drag(app, &l, x, y);
                    let _ = InvalidateRect(Some(hwnd), None, true);
                    apply_cursor(hwnd, app);
                    return LRESULT(0);
                }
            }
            // 拖完条目 / 拉完框：落地
            {
                let app = &mut *app_ptr;
                if app.drag.is_some() {
                    let _ = ReleaseCapture();
                    let l = paint::layout_for(app);
                    finish_drag(app, &l, x, y);
                    let _ = InvalidateRect(Some(hwnd), None, true);
                    apply_cursor(hwnd, app);
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
                    // 松手那一刻鼠标多半还压在分隔线上（光标仍是 ↔，是对的），
                    // 但状态已从"拖拽中"变回"悬停"，得按新状态重算一次。
                    apply_cursor(hwnd, app);
                    return LRESULT(0);
                }
            }
            // 拖完窗格分隔线：松手收摊，清掉高亮
            {
                let app = &mut *app_ptr;
                if app.pane_div_drag.take().is_some() {
                    let _ = ReleaseCapture();
                    app.hover_pane_div = None;
                    let _ = InvalidateRect(Some(hwnd), None, true);
                    apply_cursor(hwnd, app);
                    return LRESULT(0);
                }
            }
            let (l, plus) = {
                let app = &*app_ptr;
                let l = paint::layout_for(app);
                let plus = l.new_tab_rect(app.tab_count(), &app.metrics);
                (l, plus)
            };
            if y < l.title.bottom {
                if l.new_tab_visible && plus.right < l.tabs.right && inside(&plus, x, y) {
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
            // 普通点击也可能改布局（折叠侧栏、切标签…），光标按新布局再定一次
            apply_cursor(hwnd, &*app_ptr);
            LRESULT(0)
        }
        WM_MBUTTONDOWN => {
            // 鼠标中键：点标签 = 关闭该标签（资源管理器/浏览器的通用约定）
            let app = &mut *app_ptr;
            let (x, y) = client_xy(hwnd, lparam);
            let l = paint::layout_for(app);
            if let Some(i) = hit_tab(app, &l, x, y) {
                // 必须先落到**光标所在的窗格**（与 WM_LBUTTONDOWN 同款）：
                // hit_tab 在分栏时可能命中的是某个窗格自己的局部标签条，
                // 返回的 i 属于那个窗格；而 close_tab 作用在**焦点窗格**上 ——
                // 不切焦点就会关掉另一个窗格的第 i 个标签（第39轮审计 #8 案例B：
                // 右窗格点第 3 个，关掉的却是左窗格的第 3 个）。
                // 顶部总览条画的就是焦点窗格，那种情况不需要切。
                if app.split() && !(y >= l.title.top && y <= l.title.bottom) {
                    if let Some(pi) = paint::pane_index_at(app, &l, x, y) {
                        app.focus_pane = pi;
                    }
                }
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
            let l = paint::layout_for(app);
            if y < l.title.bottom {
                // 双击标签栏空白处 = 新建标签（同资源管理器）；
                // 双击标签本身 = 最大化/还原。绝不能在这里关标签 —— 双击是"打开"手势，
                // 用它来关闭会让人莫名其妙丢标签。
                let tab_w = (paint::TAB_W as f32 * app.metrics.scale) as i32;
                let i = ((x - l.tabs.left) / tab_w.max(1)) as usize;
                if i >= app.tab_count() {
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
            // 分栏时双击同样要先落到光标所在窗格（单击的 WM_LBUTTONDOWN 也会切，
            // 但双击的第二下没有独立的 DOWN 消息，光标若先移了就会切晚一步）。
            if app.split() {
                if let Some(pi) = paint::pane_index_at(app, &l, x, y) {
                    app.focus_pane = pi;
                }
            }
            let pl = if app.split() { paint::pane_layout(app, &l, app.focus_pane) } else { l };
            // 首页：双击磁贴进入对应文件夹
            if app.tab().home {
                if let Some(idx) = home_entry_at(app, &pl, x, y) {
                    let path = app.tab().entries.get(idx).map(|e| e.path.clone());
                    if let Some(p) = path {
                        app.navigate(p);
                        paint::sync_address(app); // 否则地址栏还停在"主页"
                    }
                }
                return LRESULT(0);
            }
            let header_h = header_height(app);
            if let Some(row) = view_row_at(app, &pl, x, y, pl.view.top + header_h) {
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
            let l = paint::layout_for(app);
            // 地址栏编辑态：右键别处同样先收掉（同 WM_LBUTTONDOWN 的理由：
            // 子 EDIT 不会因为点了父窗口就失焦，WM_KILLFOCUS 不来）。
            if app.addr_editing && !inside(&l.address, x, y) {
                paint::end_address_edit(app, true);
                paint::sync_address(app);
            }
            // 搜索框：右键别处同样收焦点（同 WM_LBUTTONDOWN，第41轮）
            if !inside(&l.search, x, y) {
                unsafe {
                    if windows::Win32::UI::Input::KeyboardAndMouse::GetFocus() == app.hwnd_search
                        && !app.hwnd_search.0.is_null()
                    {
                        let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(app.hwnd));
                    }
                }
            }
            // 分栏时右键同样先落到光标所在窗格 —— 菜单作用在 app.tab()
            // （焦点窗格的标签）上，不切的话在右窗格右键弹出来的是左窗格的菜单。
            if app.split() {
                if let Some(pi) = paint::pane_index_at(app, &l, x, y) {
                    app.focus_pane = pi;
                }
            }
            let pl = if app.split() { paint::pane_layout(app, &l, app.focus_pane) } else { l };
            let header_h = header_height(app);
            let view_top = pl.view.top + header_h;

            if app.tab().settings {
                // 设置页：只有右侧那块**格式样本预览**能右键 ——
                //   · 某一格   → 这个格式的右键菜单（实时预览，命令不会真的执行）；
                //   · 空白处   → 目录右键（背景菜单那一套）。
                // 于是"每一类菜单长什么样"在这儿当场就能试，不用去猜。
                let in_prev = {
                    let g = settings::geom(app, &l);
                    inside(&g.prev_panel, x, y)
                };
                if in_prev {
                    let (targets, kind) = match settings::hit(app, &l, x, y) {
                        Some(settings::Hit::Preview(i)) => {
                            let p = crate::fmtprev::samples()
                                .get(i)
                                .map(|s| s.path.clone())
                                .unwrap_or_default();
                            (vec![p], crate::app::MenuKind::File)
                        }
                        // 空白处 = 目录右键（items 为空 → 背景菜单）
                        _ => (Vec::new(), crate::app::MenuKind::Background),
                    };
                    app.context_targets = Some(targets.clone());
                    // 预览菜单里的命令一律不执行（见 `App::on_menu_activation`）
                    app.preview_menu = true;
                    let entries = crate::app::own_menu_rows(app, &targets, kind);
                    let mut pt = POINT { x, y };
                    let _ = unsafe { ClientToScreen(hwnd, &mut pt) };
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
                } else {
                    app.context_targets = Some(Vec::new());
                }
                let _ = InvalidateRect(Some(hwnd), None, true);
                LRESULT(0)
            } else if app.tab().recycle {
                if let Some(idx) = paint::recycle_hit(app, &pl, y) {
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
                if app.tab().view_mode == zenfile_core::ViewMode::Details
                    && y >= pl.view.top
                    && y < view_top
                    && x >= pl.view.left
                    && x < pl.view.right
                {
                    if let Some(col) = paint::header_col_hit(app, &pl, x, y) {
                        show_header_context_menu(hwnd, app, &pl, col, x, y);
                        return LRESULT(0);
                    }
                }
                // 先把"这一次要作用在哪些条目上"算出来存进 App，
                // 菜单里绝不能再调 selected_paths() —— 它在选中为空时会退回焦点行，
                // 那正是"空白处右键却判定到条目上"的根因。
                let targets: Vec<PathBuf> = if app.tab().home {
                    if y >= pl.view.top && x >= pl.view.left && x < pl.view.right {
                        if home_select(app, &pl, x, y) {
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
                } else if x >= pl.view.left && y >= view_top && y < pl.view.bottom {
                    match view_row_at(app, &pl, x, y, view_top) {
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
            let l = paint::layout_for(app);
            // 光标在导航窗格里时滚轮滚的是**树**，不是右边的文件列表 ——
            // 否则展开几层之后树就再也滚不出来了。
            let mut pt = windows::Win32::Foundation::POINT {
                x: (lparam.0 & 0xFFFF) as i16 as i32,
                y: (lparam.0 >> 16) as i16 as i32,
            };
            unsafe { let _ = ScreenToClient(hwnd, &mut pt); }
            // 设置页右侧的格式样本预览区：格子铺不下就滚它
            if app.tab().settings {
                let (in_prev, max_s) = {
                    let g = settings::geom(app, &l);
                    let m = (g.prev_content_h - (g.prev_view.bottom - g.prev_view.top)).max(0);
                    (inside(&g.prev_panel, pt.x, pt.y), m)
                };
                if in_prev {
                    app.settings_prev_scroll =
                        (app.settings_prev_scroll - delta / 2).clamp(0, max_s);
                    let _ = InvalidateRect(Some(hwnd), None, true);
                    return LRESULT(0);
                }
            }
            if app.cfg.ui.sidebar_visible && pt.x < l.sidebar.right {
                app.scroll_sidebar(delta / 2, &l);
                return LRESULT(0);
            }
            // 分栏时滚轮滚**光标所在窗格**（资源管理器同款）——不是焦点窗格，
            // 否则鼠标停在右窗格滚滚轮，动的是左边那个。只滚动、不切焦点：
            // Explorer 滚非活动窗格也不会把它激活。
            let mut wpl = l;
            if app.split() {
                if let Some(pi) = paint::pane_index_at(app, &l, pt.x, pt.y) {
                    wpl = paint::pane_layout(app, &l, pi);
                    let d = delta / 2;
                    let p = &mut app.panes[pi];
                    if !p.tabs.is_empty() {
                        let ai = p.active.min(p.tabs.len() - 1);
                        let tab = &mut p.tabs[ai];
                        tab.scroll -= d;
                    }
                } else {
                    let tab = app.tab_mut();
                    tab.scroll -= delta / 2;
                }
            } else {
                let tab = app.tab_mut();
                tab.scroll -= delta / 2;
            }
            // 钳制必须知道布局：网格视图的内容高度是"行数 × 行高"，行数取决于列数。
            // 约束的是"光标那个窗格"的标签，所以布局也必须是它的（行数/高度按
            // 窗格宽算，网格列数随宽度变）。
            app.clamp_pane_scroll(pt.x, pt.y, &l, &wpl);
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let app = &mut *app_ptr;
            // 地址框/搜索框正在编辑时，按键属于那个 EDIT（回车、ESC 由它自己的
            // 子类过程处理）。主窗口再插一脚会把字母当快捷键、还会顺手把地址栏
            // 文本刷回当前路径 —— 输入到一半的内容就这么没了。
            //
            // 但**判据必须是"焦点在不在 EDIT 上"，不能只看 addr_editing 标志**
            // （第33轮用户报"单击目录栏变成输入的样子，无法退出"）：焦点一旦
            // 从 EDIT 上漏掉（隐藏 EDIT 却没交出焦点那种情况），标志还是 true，
            // 按键就全被这里吞掉 —— 表现为"Esc 没反应、打字没反应、界面死了"。
            // 焦点不在 EDIT 却还打着编辑旗标的，一律**先把编辑态收掉**再继续处理。
            if app.addr_editing {
                let focus = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetFocus() };
                if focus != app.hwnd_address {
                    paint::end_address_edit(app, true);
                    paint::sync_address(app);
                } else {
                    return LRESULT(0);
                }
            }
            // 「快捷键」页正在录一个新组合键：这一下按键归它，不往下走
            if let Some(act) = app.hotkey_capture.clone() {
                let vk = wparam.0 as u32;
                if vk == 0x1B {
                    app.hotkey_capture = None;
                    let _ = InvalidateRect(Some(hwnd), None, true);
                    return LRESULT(0);
                }
                if let Some(name) = vk_name(vk) {
                    let mut acc = String::new();
                    mod_down(0x11, "ctrl+", &mut acc);
                    mod_down(0x12, "alt+", &mut acc);
                    mod_down(0x10, "shift+", &mut acc);
                    acc.push_str(name);
                    app.cfg.core.hotkeys.insert(act.clone(), acc.clone());
                    let _ = zenfile_core::config::Config::save_user_patch(&serde_json::json!({
                        "core": { "hotkeys": { act: acc } }
                    }));
                    app.hotkey_capture = None;
                    let _ = InvalidateRect(Some(hwnd), None, true);
                }
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
            // 「选择详细信息」是挂在**本窗口**上的弹出窗，主窗没了它必须跟着走，
            // 否则会留一个改不到任何目录的孤儿对话框。
            coldlg::close_now();
            // 关闭前把位置定格写盘（第29轮）：托盘隐藏走的也是这条路，
            // 之后用户从托盘再开窗口时位置才是对的。
            persist_window_rect(hwnd, true);
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
                let _ = KillTimer(Some(hwnd), TIMER_ANIM);
                // 第74轮：销毁缓存的那张全窗口离屏位图与它的 DC。
                //
                // 必须放在 `Box::from_raw` **之前** —— 之后 `App` 已经被
                // drop（`BackSurface` 刻意没有 `Drop` 实现，GDI 句柄要显式
                // 删），这里就再也拿不到那两个句柄了，进程退出前会一直挂着
                // 一个 DC + 一张整窗位图（多次开关窗口就线性堆积）。
                if !app_ptr.is_null() {
                    paint::invalidate_back_surface(&mut *app_ptr);
                }
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
    if matches!(mode, zenfile_core::ViewMode::LargeIcons | zenfile_core::ViewMode::MediumIcons) {
        let g = paint::icon_grid(l, &app.metrics, mode);
        paint::grid_hit(&g, l, x, y, view_top, tab.scroll, tab.visible.len())
    } else {
        let row = app.row_hit(y, view_top, l.view.bottom - view_top)?;
        // 详细信息/列表视图：右侧空白处不应当成"命中行"。
        // 视觉上行背景现在**铺满整行**（见 paint 那边的注释），所以不能再用
        // item_rect 的 right 判（它是 l.view.right，恒真 = 判断从来没生效过 ——
        // 用户"说过几遍"的那条）。改用**列区右沿**：最后一列（大小）结束处。
        if mode == zenfile_core::ViewMode::Details {
            let icon_size = (16.0 * app.metrics.scale) as i32;
            // 列区右沿 = 最后一列右沿（与行高亮/表头分隔线同源，第32轮 #1）
            let boxes = paint::col_boxes(app, l, icon_size);
            if x >= paint::cols_right(&boxes) {
                return None;
            }
        }
        Some(row)
    }
}

/// 拖拽落点判定：面包屑 → 导航窗格 → 内容区里的文件夹行。
///
/// 顺序有意义：地址栏压在工具栏里、导航窗格又压着内容区左边缘，
/// 从最具体的开始判才不会"想拖到面包屑结果落进了下面的行"。
/// 焦点窗格的标签数。分栏时 `tab_count()` 跟 `focus_pane` 走，
/// 这里只是把它写短一点，免得到处 `app.panes[app.focus_pane].tabs.len()`。
fn pl_tab_count(app: &App) -> usize {
    app.panes[app.focus_pane].tabs.len()
}

/// 拖着标签移动：算这一刻它会落到哪。
///
/// 三种去处互斥，优先级从高到低：
///   1. 出了窗口客户区 → 拆成新进程
///   2. 落在内容区     → 8 向贴
///   3. 还在标签条上   → 换顺序
fn update_tab_drag(app: &mut App, hwnd: HWND, l: &paint::Layout, x: i32, y: i32) {
    // 先把"是否越过阈值"这一件事做完并**放开借用**：下面要拿 &app 去问
    // 窗格几何，一直攥着 &mut 会和它打架。
    {
        let Some(d) = app.tab_drag.as_mut() else { return };
        d.cur = (x, y);
        if !d.active {
            let dx = x - d.origin.0;
            let dy = y - d.origin.1;
            // 4px 阈值：普通单击也会抖几像素，不给阈值的话点一下就算"拖"
            if dx * dx + dy * dy <= 16 {
                return;
            }
            d.active = true;
        }
    }
    let (w, h) = app.last_size;
    let outside = x < 0 || y < 0 || x >= w || y >= h;
    let (dock, over) = if outside {
        (None, None)
    } else {
        // 分栏时"贴到哪个窗格"由光标所在窗格决定，顺手把它设为焦点
        let pl = if app.split() {
            if let Some(pi) = paint::pane_index_at(app, l, x, y) {
                app.focus_pane = pi;
            }
            paint::pane_layout(app, l, app.focus_pane)
        } else {
            *l
        };
        if y > pl.view.top && y < pl.view.bottom && x > pl.view.left && x < pl.view.right {
            (Some(paint::dock_zone_at(&pl, x, y)), None)
        } else {
            (None, hit_tab(app, &pl, x, y))
        }
    };
    if let Some(d) = app.tab_drag.as_mut() {
        d.outside = outside;
        d.dock = dock;
        d.over_tab = over;
    }
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
}

/// 拖标签松手：把 `finish_tab_drag` 算出来的结果真的执行掉。
fn end_tab_drag(app: &mut App, hwnd: HWND) {
    match app.finish_tab_drag() {
        app::TabDrop::Nothing => {}
        app::TabDrop::Reorder { pane, from, to } => {
            app.reorder_tab(pane, from, to);
            paint::sync_address(app);
        }
        app::TabDrop::Dock { pane, index, zone } => {
            // dock_tab 认的是"焦点窗格"当目标，拖拽期间 update_tab_drag
            // 已经把焦点挪到光标所在那个窗格了，这里直接用。
            app.dock_tab(pane, index, zone);
            app.reload();
            paint::sync_address(app);
            paint::reposition(app);
        }
        app::TabDrop::TearOff { pane, index, path } => {
            // 拆成新进程：先起新的，再关本窗口这个 —— 顺序反了的话
            // （只剩一个标签时会先补一个主页标签）新窗口拿不到原路径。
            crate::new_window(path);
            app.focus_pane = pane.min(app.panes.len() - 1);
            app.close_tab(index);
            paint::sync_address(app);
        }
    }
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
}

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
    // 2.5) **标签条**：拖到某个标签上 = 落到那个标签所在的目录。
    // 必须排在内容区之前 —— 标签条压在窗口顶上，但它的 y 也可能落进
    // `l.view` 的范围（分栏时窗格的标签条就在内容区顶上）。
    let pl = if app.split() {
        match paint::pane_index_at(app, l, x, y) {
            Some(pi) => paint::pane_layout(app, l, pi),
            None => *l,
        }
    } else {
        *l
    };
    if let Some(i) = hit_tab(app, &pl, x, y) {
        let n = if app.split() { app.panes[app.focus_pane].tabs.len() } else { app.tab_count() };
        if i < n {
            let path = if app.split() {
                app.panes[app.focus_pane].tabs[i].path.clone()
            } else {
                app.pane().tabs[i].path.clone()
            };
            return Some(app::DropTarget { path, kind: app::DropKind::Tab(i) });
        }
    }
    // 3) 内容区里的文件夹行
    let view_top = pl.view.top + header_height(app);
    if y >= view_top && y < pl.view.bottom {
        if let Some(row) = view_row_at(app, &pl, x, y, view_top) {
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

/// 目录栏最右侧「批量重命名 ▾」的下拉：五种模式，各对应 `zenfile_core::bulk` 一条规则。
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

/// 修饰键是否按下（VK_CONTROL 0x11 / VK_MENU 0x12 / VK_SHIFT 0x10）。
fn mod_down(vk: i32, tag: &str, acc: &mut String) {
    // GetKeyState 在 windows-rs 0.62 里是 unsafe 的（别处那几处调用点其实漏了
    // unsafe，靠的是旧版签名 —— 这里照规矩包起来）
    if unsafe { GetKeyState(vk) } < 0 {
        acc.push_str(tag);
    }
}

/// 虚拟键 → 配置里那个名字。只列真会有人拿来当快捷键的键，
/// 认不出来的直接放弃（宁可不录，也不能录进一个解析不回来的串）。
fn vk_name(vk: u32) -> Option<&'static str> {
    Some(match vk {
        0x08 => "backspace",
        0x09 => "tab",
        0x0D => "enter",
        0x20 => "space",
        0x21 => "pageup",
        0x22 => "pagedown",
        0x23 => "end",
        0x24 => "home",
        0x25 => "left",
        0x26 => "up",
        0x27 => "right",
        0x28 => "down",
        0x2D => "insert",
        0x2E => "delete",
        0x30..=0x39 => DIGITS[(vk - 0x30) as usize],
        0x41..=0x5A => LETTERS[(vk - 0x41) as usize],
        0x70..=0x87 => FKEYS[(vk - 0x70) as usize],
        _ => return None,
    })
}

const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
const LETTERS: [&str; 26] = [
    "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r", "s",
    "t", "u", "v", "w", "x", "y", "z",
];
const FKEYS: [&str; 24] = [
    "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12", "f13", "f14",
    "f15", "f16", "f17", "f18", "f19", "f20", "f21", "f22", "f23", "f24",
];

/// 强制收掉一切拖拽状态，**不做任何落地**（不选中、不搬文件、不分栏）。
///
/// 第61轮新增。丢松手消息的路径有好几条（capture 被菜单/对话框抢走、
/// 窗口失活、最小化、Alt+Tab），每一条的后果都一样：框选矩形 / 拖拽影像
/// 永远赖在画面上。以前只在 `WM_MOUSEMOVE` 里靠 `GetKeyState` 兜底，
/// 可用户松手后鼠标不动的话那条兜底一辈子不触发 —— 截图里"空白处多一个
/// 半透明蓝框"就是这个。
///
/// 刻意不复用 `finish_drag`：那条会拿松手坐标去算投放目标，
/// 而失活/取消场景下那个坐标毫无意义，误判成"落在某个文件夹上"
/// 就会把文件搬到用户没打算去的地方。
fn cancel_drag(app: &mut App) {
    let mut had = app.drag.take().is_some();
    app.drag_dock = None;
    app.drop_target = None;
    app.tab_hover_since = None;
    if app.tab_drag.take().is_some() {
        had = true;
    }
    if had {
        let _ = unsafe { ReleaseCapture() };
    }
}

/// 松手：条目拖拽去落地 / 框选去选中。
fn finish_drag(app: &mut App, l: &paint::Layout, x: i32, y: i32) {
    let Some(drag) = app.drag.take() else {
        return;
    };
    let dock_zone = app.drag_dock.take();
    app.drop_target = None;
    if !drag.active {
        return;
    }
    match drag.kind {
        app::DragKind::Marquee => select_marquee(app, l, &drag),
        app::DragKind::Items => {
            // 拖着文件夹松在内容区边缘 = "把这个文件夹开成一个新窗格"。
            // 分栏之后标题栏会显示成「A 丨 B」，所以用户拖进去的那个目录名一定看得到。
            if let Some(zone) = dock_zone {
                if drag.items.len() == 1 && drag.items[0].is_dir() {
                    if let Some(pi) = paint::pane_index_at(app, l, x, y) {
                        app.focus_pane = pi;
                    }
                    app.open_split(drag.items[0].clone(), zone);
                    return;
                }
            }
            if let Some(t) = drop_target_at(app, l, x, y) {
                let copy = drag.copy || app.drag_copy();
                // **先搬，再切标签**。反过来的话 `drop_items` 里的
                // "目标 == 当前目录就跳过"会把它当成自己拖给自己 ——
                // 表现就是"拖到标签上松手，什么都没发生"。
                app.drop_items(t.path.clone(), drag.items.clone(), copy);
                if let app::DropKind::Tab(i) = t.kind {
                    if i < app.panes[app.focus_pane].tabs.len() {
                        app.panes[app.focus_pane].active = i;
                        app.reload();
                    }
                }
            }
        }
    }
}

/// 框选：矩形与条目矩形相交的都选中。
///
/// 三种布局的"条目矩形"算法完全不同（列表行 / 网格磁贴 / 首页磁贴），
/// 回收站更是另一套（按下标 + id 选中）。四个分支各自用自己的几何，
/// 绝不拿列表那套去套首页 —— 那正是"框选在首页/回收站不生效"的根因。
///
/// 既给松手时 `end_drag` 用，也给框选过程中 `WM_MOUSEMOVE` 实时调用：
/// 用户拖着框走的每一步都要能看到哪些条目被命中，松手才看到就是"卡了"。
fn select_marquee(app: &mut App, l: &paint::Layout, drag: &app::Drag) {
    select_marquee_into(app, l, drag.origin, drag.cur);
}

fn select_marquee_into(app: &mut App, l: &paint::Layout, origin: (i32, i32), cur: (i32, i32)) {
    // 分栏时框选发生在**焦点窗格**里：条目矩形按窗格几何算（与按下那下
    // WM_LBUTTONDOWN 用的 pane_layout 同一份），拿全局 view 算会选到错的行。
    let pl = if app.split() { paint::pane_layout(app, l, app.focus_pane) } else { *l };
    let l = &pl;
    // 第60轮：钳进显示区，与 paint_drag_overlay 的绘制用同一份几何 ——
    // 框选框不许越出文件/文件夹区域，命中范围也必须跟着收，
    // 否则会出现"框看着停在区内却选中了区外条目"的错位。
    let r = paint::marquee_rect_from_in(origin, cur, l.view);
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
    // 第59轮修复：框选后焦点框必须落到"真正被框到的那一行"，而不是留在
    // 默认的 focus_row=0 —— 否则松开后第 0 行会顶着一条高亮（与选中同款
    // 底色 + 行首竖线），看着就像"莫名其妙选中了第一个文件/文件夹"。
    // 循环按 0..n 顺序推进，所以第一个命中就是下标最小、位置最靠上的命中。
    let mut first_hit: Option<usize> = None;
    if app.tab().home {
        let (_, tiles) = paint::home_layout(app, &l.view);
        for (n, t) in tiles.iter().enumerate() {
            if hits(&t.rect, &r) {
                if let Some(e) = app.tab().entries.get(t.index) {
                    keys.push(crate::app::key_of(e));
                    if first_hit.is_none() {
                        first_hit = Some(n);
                    }
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
                    if first_hit.is_none() {
                        first_hit = Some(i);
                    }
                }
            }
        }
    }
    if keys.is_empty() {
        return;
    }
    {
        let tab = app.tab_mut();
        tab.selected.clear();
        for k in keys {
            tab.selected.insert(k);
        }
    }
    // 把焦点行挪到被框选的顶行：既消掉"第 0 行无故高亮"的错觉，又给
    // 后续键盘操作（方向键续选）一个合理的锚点（与资源管理器一致）。
    app.set_focus_row(first_hit.unwrap_or(0));
}

/// 详细信息视图表头的高度；其它视图没有表头。
fn header_height(app: &App) -> i32 {
    if app.tab().view_mode == zenfile_core::ViewMode::Details {
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
            let cur = zenfile_core::SizeUnit::parse(&app.cfg.files.view.size_unit);
            zenfile_core::SizeUnit::ALL
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

/// 表头右键菜单（第44轮，照用户给的资源管理器截图；第54轮改成"前 8 项 + 其他"）：
///   将列调整为合适的大小(S)
///   将所有列均调整为合适的大小(A)
///   ─────────
///   ▎名称          ← 名称恒显且不可点（同 Explorer）
///   ▎修改日期      ← 已显示的列 = 行首橙色竖线（第52轮：
///    类型            与右键菜单的选中态同款，不再画 ✓ 勾）
///    大小
///    创建日期
///    访问日期
///    属性
///    文件夹路径
///   ─────────
///   其他(M)...     ← 打开完整的「选择详细信息」对话框（21 项全列 + 滚动）
///
/// 资源管理器也是这套"常用列内联、冷门列收进「更多…」"的折法：菜单里只摆
/// 最常用的前 N 个，剩下的全塞进「其他(M)...」弹出的对话框里。对话框按
/// Explorer 样子做成了**可滚动**（不是一次性全摊开），21 项再多也不会把窗口
/// 撑爆（第54轮 `coldlg.rs` 的动态高度 + WM_MOUSEWHEEL + 裁剪）。
///
/// 之前顶上还有"按X递增/递减排序"和"筛选"两节 —— 用户明确不要
/// （"不要筛选和排序那些乱七八糟的"）。排序仍然点表头就能做。
///
/// 注意：内联的"前 8 项"是 `COL_ALL` 的前 8 个（`take(8)`），顺序与对话框一致；
/// 调 `COL_ALL` 顺序时这里会跟着变，不用单独同步。
fn header_choose_columns_rows(app: &App) -> Vec<menu::MenuEntry> {
    let mut v: Vec<menu::MenuEntry> = Vec::new();
    v.push(menu::MenuEntry::item(
        "将列调整为合适的大小(S)",
        menu::Command::AutofitColumns,
    ));
    v.push(menu::MenuEntry::item(
        "将所有列均调整为合适的大小(A)",
        menu::Command::AutofitAllColumns,
    ));
    v.push(menu::MenuEntry::sep());
    // 搜索态下"选择列"禁用（资源管理器同样：搜索结果的列是固定的）
    let locked = app.tab().search_root.is_some();
    // 名称恒在、恒显、不可点 —— 与 Explorer 一致（没有任何列表能没有"名称"）
    // 只内联**前 8 个**常用列；其余 13 个走「其他(M)...」对话框（滚动列出）。
    for id in zenfile_core::columns::COL_ALL.iter().take(8) {
        let on = app.is_col_visible(id);
        // 选中态 = 右键菜单同款行首橙色竖线（`selected`），悬停时让位给蓝色
        // 悬停条（渲染层的既有规则），与「查看 → 详细信息」一套语言。
        let e = menu::MenuEntry::item(
            zenfile_core::columns::label_of(id),
            menu::Command::ToggleColumn((*id).to_string()),
        )
        .with_selected(on);
        if *id == zenfile_core::columns::C_NAME || locked {
            v.push(e.with_disabled(true));
        } else {
            v.push(e);
        }
    }
    v.push(menu::MenuEntry::sep());
    // 「其他(M)...」：完整的「选择详细信息」对话框（勾选 + 上移/下移 + 确定/取消），
    // 里面**全部 21 列滚动呈现**（不是一次全摊开）。搜索态下列集合是固定的，
    // 跟着一起禁用（资源管理器同样）。
    v.push(menu::MenuEntry::item("其他(M)...", menu::Command::ChooseColumns)
        .with_disabled(locked));
    v
}

/// 表头右键 = 「选择列」菜单本身（第44轮拍平，不再有排序/筛选外层）。
fn show_header_context_menu(hwnd: HWND, app: &mut App, _l: &paint::Layout, _col: usize, x: i32, y: i32) {
    let entries = header_choose_columns_rows(app);
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

/// 列 id → 旧的"语义列号"（0=名称 1=修改日期 2=类型 3=大小）。
/// 筛选/排序这两套老逻辑还是按语义号组织的，先转过去。
fn header_sem_of(id: &str) -> u8 {
    match id {
        zenfile_core::columns::C_NAME => 0,
        zenfile_core::columns::C_DATE_MOD
        | zenfile_core::columns::C_DATE_CREATED
        | zenfile_core::columns::C_DATE_ACCESSED => 1,
        zenfile_core::columns::C_TYPE => 2,
        zenfile_core::columns::C_SIZE => 3,
        _ => 4,
    }
}

/// 列 id → SortKey（不可排序的列返回 None）。
fn sort_key_of_col(id: &str) -> Option<zenfile_core::SortKey> {
    match id {
        zenfile_core::columns::C_NAME => Some(zenfile_core::SortKey::Name),
        zenfile_core::columns::C_DATE_MOD
        | zenfile_core::columns::C_DATE_CREATED
        | zenfile_core::columns::C_DATE_ACCESSED => Some(zenfile_core::SortKey::Modified),
        zenfile_core::columns::C_TYPE => Some(zenfile_core::SortKey::Type),
        zenfile_core::columns::C_SIZE => Some(zenfile_core::SortKey::Size),
        _ => None,
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
    let l = paint::layout_for(app);
    // 编辑态下地址栏里已经是一个真 EDIT，父窗口不该再抢它的光标
    (!app.addr_editing && inside(&l.address, pt.x, pt.y)) || inside(&l.search, pt.x, pt.y)
}

/// 按「当前鼠标位置 + 当前状态」立刻重设光标。
///
/// 这是"鼠标指针经常变成死的 / 不自动更新"的根治点，三个坑一起填：
///   1. **WM_SETCURSOR 比 WM_MOUSEMOVE 先到**。早先它读的是上一次 move 存进
///      `hover_split / hover_pane_div` 的快照 —— 天然慢一拍；鼠标停在原地不动
///      （拖完分隔线松手、滚完滚轮、菜单关掉）就再也收不到新的 WM_SETCURSOR，
///      光标于是**永久卡在旧的那支**。这里一律 GetCursorPos 现取位置现算命中，
///      不读任何 hover_* 快照，从根上不存在"慢一拍"。
///   2. 拖拽期间 WM_MOUSEMOVE 走到一半就 return 了，hover_* 根本没更新过，
///      方向只能由 `drag_*` 状态自己给。
///   3. 凡是"状态变了但鼠标没动"的收尾处（松手、滚滚轮、菜单关闭、切窗格）
///      都要主动调一次，不能干等下一次移动。
unsafe fn apply_cursor(hwnd: HWND, app: &App) {
    // 菜单开着时光标归菜单窗口管，主窗口别去抢（两边会来回打架）
    if app.menu_hwnd.is_some() {
        return;
    }
    let mut pt = POINT::default();
    if GetCursorPos(&mut pt).is_err() {
        return;
    }
    // 光标压在自家子窗口（地址栏/搜索框/重命名框的真 EDIT）上时让子窗口自己
    // 决定 —— 它们要的是 I 形光标，父窗口在这里插一脚会把它按回箭头。
    let under = WindowFromPoint(pt);
    if under.0 != std::ptr::null_mut() && under != hwnd && IsChild(hwnd, under).as_bool() {
        return;
    }
    let mut c = pt;
    let _ = ScreenToClient(hwnd, &mut c);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    // 鼠标在窗口外（或被别家窗口盖住）不参与：这时候算出来的"边框命中"是假的，
    // 会把 ↔ 设到别人家窗口上去。
    if c.x < rc.left || c.x >= rc.right || c.y < rc.top || c.y >= rc.bottom {
        set_cursor_id(IDC_ARROW);
        return;
    }
    let hit = handle_hit_test(hwnd, LPARAM(pack_point(pt.x, pt.y))).0 as u32 & 0xFFFF;
    let id = match hit {
        10 | 11 => IDC_SIZEWE,   // HTLEFT / HTRIGHT
        12 | 15 => IDC_SIZENS,   // HTTOP / HTBOTTOM
        13 | 17 => IDC_SIZENWSE, // HTTOPLEFT / HTBOTTOMRIGHT
        14 | 16 => IDC_SIZENESW, // HTTOPRIGHT / HTBOTTOMLEFT
        _ => {
            // 正在拖窗格分隔线：方向由这次拖拽自己说了算
            if let Some(vertical) = app.pane_div_drag.as_ref().map(|d| d.vertical) {
                if vertical {
                    IDC_SIZEWE
                } else {
                    IDC_SIZENS
                }
            } else if app.drag_split.is_some() {
                IDC_SIZEWE
            } else if app.drag.is_some()
                || app.tab_drag.is_some()
                || app.settings_drag.is_some()
                || app.settings_radius_drag
            {
                // 拖文件 / 拖标签 / 拖设置卡片 / 拖圆角滑杆：一律箭头。
                // 早先这些状态一个都没覆盖，拖完侧栏再随手拖个文件，光标就一直
                // 挂着 ↔（"指针变死"最常见的那一次）。
                IDC_ARROW
            } else {
                let l = paint::layout_for(app);
                if let Some((vertical, _, _)) = paint::pane_div_at(app, &l, c.x, c.y) {
                    if vertical {
                        IDC_SIZEWE
                    } else {
                        IDC_SIZENS
                    }
                } else if paint::split_hit(app, &l, c.x, c.y).is_some() {
                    IDC_SIZEWE
                } else if over_text_input(app, hwnd) {
                    // 地址栏/搜索框上要出 I 形光标：它们平时是画在父窗口上的
                    // 面包屑/装饰（真 EDIT 只在编辑态才建），没有子窗口替我们设，
                    // 于是"移到输入框上光标却没变化"。
                    IDC_IBEAM
                } else {
                    IDC_ARROW
                }
            }
        }
    };
    set_cursor_id(id);
}

unsafe fn set_cursor_id(id: PCWSTR) {
    if let Ok(c) = LoadCursorW(None, id) {
        SetCursor(Some(c));
    }
}

/// WM_NCHITTEST 用的是"打包过的屏幕坐标"（低位 x，高位 y）。
fn pack_point(x: i32, y: i32) -> isize {
    ((((y as u32) & 0xFFFF) << 16) | ((x as u32) & 0xFFFF)) as isize
}

/// 让窗口在鼠标离开时真的收到 WM_MOUSELEAVE。
///
/// 之前这条消息根本不会来（从来没调过 TrackMouseEvent），于是"鼠标都移出窗口
/// 了，内容区那一行还亮着、分隔线还保持高亮"，只有标题栏三个键靠 WM_ACTIVATE
/// 那条路勉强清过一次。
///
/// **挂载语义是"挂一次，收到 leave 后才允许再挂"**（`app.mouse_tracked` 把门）：
/// TrackMouseEvent 的 TME_LEAVE 跟踪的是**客户区**，光标在非客户区（窗口边框那
/// 4px）时算"不在客户区"。要是每次 WM_MOUSEMOVE 都重挂，光标从边框划进内容区
/// 的那一下，系统会把刚才那次"离开客户区"的滞回事件补发成 WM_MOUSELEAVE ——
/// 悬停刚设好就被清掉，鼠标停住不动就再也亮不起来（第54轮"悬停效果没了"）。
/// 不带 TME_NONCLIENT：整个窗口离开时客户区必然也被离开，一条 WM_MOUSELEAVE
/// 就够；WM_NCMOUSELEAVE 的处理分支保留着做兜底。
fn track_mouse_leave(app: &mut App, hwnd: HWND) {
    if app.mouse_tracked {
        return;
    }
    app.mouse_tracked = true;
    let mut tme = TRACKMOUSEEVENT {
        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TRACKMOUSEEVENT_FLAGS(windows::Win32::UI::Input::KeyboardAndMouse::TME_LEAVE.0),
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    let _ = unsafe { TrackMouseEvent(&mut tme) };
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
    let l = paint::layout_for(app);
    let border = (4.0 * app.metrics.scale).round() as i32;
    let plus = l.new_tab_rect(app.tab_count(), &app.metrics);
    // 窗口顶部那条标签栏**单窗格和分栏都画**（分栏 = 焦点窗格的总览条，
    // 见 paint_title_bar）—— 命中必须跟着一起开。之前这里写 `!app.split()`，
    // 分栏后顶部标签全被判成 HTCAPTION：按下被系统当"拖窗口"吞掉，窗口
    // 收不到 WM_LBUTTONDOWN —— 表现是"顶部标签点不动、拖不动，还会把整个
    // 窗口拖跑"（绘制与命中必须共用同一判定）。
    let has_top_tabs = true;

    // 四边 + 四角。**必须放在标题栏判定之前**：标题栏占着窗口顶上那一长条，
    // 先判标题栏的话顶边永远拿不到 HTTOP。
    // 之前这一节有三个错叠在一起：左右两边多带了 `pt.y > l.view.top`（顶边那一
    // 截直接漏成 HTCLIENT），上边拿 `l.sidebar.top`（**窗格**的上沿）当窗口上沿，
    // 四角又完全没判 —— 于是四面只有底边那 4px 能拉伸。
    //
    // **无条件判**：这一节早先还包在一个 `if !on_caption_ctrl` 里 —— 标题栏自有
    // 控件（侧栏开关 / 新建标签 / 三大金刚键）压着窗口最外侧那几像素时，整条拉伸
    // 带被吃掉，表现是"顶边左端、右上角、左边上端这些位置鼠标不变成拖拽指针"。
    // 拉伸带只有 border(≈4px) 宽，只盖住按钮贴边的最顶端几像素，和原生窗口的
    // resize 框行为一致 —— 按钮本体（y > border）照常命中 HTMINBUTTON/HTCLOSE。
    {
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
            || (has_top_tabs
                && (0..app.tab_count()).any(|i| inside(&l.tab_rect(i, app.tab_count(), &app.metrics), pt.x, pt.y)))
            || (has_top_tabs && plus.right < l.tabs.right && inside(&plus, pt.x, pt.y))
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
    // 顶部标题栏 tab 条：**单窗格和分栏都画**（分栏 = 焦点窗格的总览条），
    // 所以先测顶部。
    //
    // 贴边窗格自己那条 26px 顶带现在是**标题条**（只显示文件夹名，没有
    // 标签、没有 ×），所以这里不再对它做标签命中 —— 点它就是点窗格本身。
    if y >= l.title.top && y <= l.title.bottom {
        let n = app.panes.get(app.focus_pane).map(|p| p.tabs.len()).unwrap_or(0);
        for i in 0..n {
            if inside(&l.tab_rect(i, n, &app.metrics), x, y) {
                return Some(i);
            }
        }
    }
    None
}

fn header_click(app: &mut App, l: &paint::Layout, x: i32) {
    // 点表头 = 按那一列排序。列几何**必须**取绘制端那一份（col_boxes），
    // 否则隐藏过列 / 拖过列宽之后，点击命中的列和看到的列会错位
    //（第33轮之前这里是自己数一遍 widths，与绘制各写一份）。
    let icon_size = (16.0 * app.metrics.scale) as i32;
    let boxes = paint::col_boxes(app, l, icon_size);
    let Some(id) = paint::col_id_at(app, &boxes, x) else {
        return;
    };
    let key = match id {
        zenfile_core::columns::C_NAME => Some(zenfile_core::SortKey::Name),
        zenfile_core::columns::C_DATE_MOD
        | zenfile_core::columns::C_DATE_CREATED
        | zenfile_core::columns::C_DATE_ACCESSED => Some(zenfile_core::SortKey::Modified),
        zenfile_core::columns::C_TYPE => Some(zenfile_core::SortKey::Type),
        zenfile_core::columns::C_SIZE => Some(zenfile_core::SortKey::Size),
        _ => None,
    };
    if let Some(k) = key {
        app.sort_column(k);
    }
}

fn sidebar_click(app: &mut App, x: i32, y: i32) -> bool {
    let l = paint::layout_for(app);
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
    let l = paint::layout_for(app);
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
        _ if ctrl && key == 0x43 => app.clipboard_write(zenfile_core::clipboard::ClipMode::Copy),
        _ if ctrl && key == 0x58 => app.clipboard_write(zenfile_core::clipboard::ClipMode::Move),
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
        _ if ctrl && key == 0x57 => app.close_tab(app.pane().active),
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
        _ if ctrl && !shift && key == 0x31 => app.set_view(zenfile_core::ViewMode::Details),
        _ if ctrl && !shift && key == 0x32 => app.set_view(zenfile_core::ViewMode::List),
        _ if ctrl && !shift && key == 0x33 => app.set_view(zenfile_core::ViewMode::MediumIcons),
        _ if ctrl && !shift && key == 0x34 => app.set_view(zenfile_core::ViewMode::LargeIcons),
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
    match zenfile_core::paths::resolve(&text, &cwd) {
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
    // 防抖挂起一并作废：不然取消后 ~200ms，TIMER_DRAIN 还会拿旧状态再跑一次
    app.search_dirty = false;
    app.search_ticks = 0;
    app.reload();
    app.invalidate();
    // Esc 之后焦点别留在空框里闪（第41轮"关不掉"的另一半）：和资源管理器
    // 一致，交还主窗口。地址栏那条在 end_address_edit(true) 里已交过，
    // 这里补搜索框 —— 焦点还挂在它身上就说明刚才 Esc 是从搜索框按的。
    unsafe {
        if windows::Win32::UI::Input::KeyboardAndMouse::GetFocus() == app.hwnd_search
            && !app.hwnd_search.0.is_null()
        {
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(app.hwnd));
        }
    }
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
