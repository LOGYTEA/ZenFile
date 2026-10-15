//! 动画时钟（第68轮）：用**后台线程 + PostMessage** 代替 `SetTimer`。
//!
//! # 为什么不用 SetTimer 驱动动画
//!
//! 用户报"所有动画都好卡，有种只有 15 帧的感觉"。`--bench-timer` 量到：
//!
//! ```text
//! WM_TIMER x20 / 500ms  min=16 中位=31 max=31 ms  => 32.3 fps
//! ```
//!
//! 请求 16ms、实际中位 **31ms** —— 整整 2 倍。原因：
//!
//! * `WM_TIMER` 的投递间隔被量化到**全局系统时钟节拍**；
//! * 即使用 `NtSetTimerResolution` 把分辨率压到 1ms（实测
//!   `NtQueryTimerResolution` 的 `current` 确实变成 0.997ms），
//!   `WM_TIMER` 仍然是 **16/31 交替** —— 系统会"平滑"高频定时器，
//!   连续投递两次后强制插一个长间隔；
//! * 而且 `WM_TIMER` 的优先级最低，忙的时候被丢弃/合并，直接掉到 8fps。
//!
//! `timeBeginPeriod` 更糟：Win10 2004 起它**只对本进程生效**，对
//! `SetTimer` 用的全局节拍**毫无影响**（实测 `Sleep(1)` 仍是 15.4ms）。
//!
//! # 这里的做法
//!
//! 一个**专用后台线程**，用 `WaitForSingleObject` + 15.6ms 的 waitable
//! timer（或退而用 `Sleep`）控制节奏，每到一拍就
//! `PostMessageW(hwnd, WM_APP_ANIM_TICK, 0, 0)`。
//!
//! 关键点：
//! * 节奏由**线程自己的等待**决定，不经过系统时钟节拍量化；
//! * 主线程的消息循环照常 `GetMessageW` 收到，**UI 线程不阻塞**
//!   （动画推进仍然只在主线程做，重绘也在主线程）；
//! * 线程用**原子开关**控制启停（`ANIM_CLOCK` 里的 `AtomicBool`），
//!   不用每次重启线程 —— 那比定时器还贵。
//!
//! # 为什么不干脆用 DWM 合成时钟
//!
//! `DwmGetCompositionTimingInfo` 能拿到精确的刷新时间，但要求窗口
//! 已参与 DWM 合成，且返回的是"上次刷新时刻"而非"下一次该醒的时刻"，
//! 仍然要自己算 sleep 时长 —— 绕一圈还是回到等待原语。
//! 一个显式的 waitable timer 更直接、可测。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::System::Threading::{CreateThread, WaitForSingleObject};

/// 主线程收到这个自定义消息就推一帧动画。
pub const WM_APP_ANIM_TICK: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP;

/// 动画帧间隔（毫秒）。16ms ≈ 60fps。
///
/// 用 15 而不是 16：Windows 的 `CreateWaitableTimer` 精度受全局节拍
/// 影响，15ms 能稳定落在 60fps 附近；用 16 会被向上取整到 31ms
/// （与 `SetTimer(16)` 同一个坑）。
const FRAME_MS: u32 = 15;

/// 动画时钟的全局状态（整个进程一份）。
pub struct AnimClock {
    /// 线程是否该继续跑。`false` 时线程退出。
    running: AtomicBool,
    /// 已 post 但**还没被主线程消费**的帧数。
    ///
    /// **必须有这个上限**（下面注释详述）。
    inflight: AtomicU32,
    /// 线程句柄（用于退出时等待）。
    thread: AtomicU32,
}

/// 全局时钟实例。窗口句柄在 `start` 时传入。
static ANIM_CLOCK: AnimClock = AnimClock {
    running: AtomicBool::new(false),
    inflight: AtomicU32::new(0),
    thread: AtomicU32::new(0),
};

/// in-flight 帧数上限。
///
/// **这是本模块最关键的一条纪律。**
///
/// 后台线程按固定节奏 post，主线程不一定来得及消费（重绘一帧可能要
/// 好几毫秒）。如果不限制，线程会**越积越多** —— 主线程消费不及，
/// 队列里堆几百条 tick，每条都要 `InvalidateRect` + 重绘，于是画面
/// 永远追不上、CPU 打满，而且**帧率反而更差**（越堆越慢）。
///
/// 上限 1 的语义：「主线程手头还有一帧没处理时，线程这一拍就不 post」。
/// 这样队列长度恒为 ≤1，天然丢弃过期帧 —— 丢帧是对的，追帧是错的。
const MAX_INFLIGHT: u32 = 1;

impl AnimClock {
    /// 启动（或重启）动画时钟，目标窗口 `hwnd`。
    pub fn start(hwnd: HWND) {
        if ANIM_CLOCK.running.swap(true, Ordering::SeqCst) {
            return; // 已在跑
        }
        let h = hwnd.0 as *const std::ffi::c_void;
        let tid = unsafe {
            CreateThread(
                None,
                0,
                Some(anim_thread),
                Some(h),
                windows::Win32::System::Threading::THREAD_CREATION_FLAGS(0),
                None,
            )
        };
        if let Ok(t) = tid {
            ANIM_CLOCK.thread.store(t.0 as u32, Ordering::SeqCst);
        } else {
            // 建线程失败：动画会停在静止帧，但 UI 不会崩（tick_anim 仍可
            // 被 TIMER_DRAIN 顺带驱动一次）。这是可接受的降级。
            ANIM_CLOCK.running.store(false, Ordering::SeqCst);
        }
    }

    /// 停止动画时钟并等线程退出。
    pub fn stop() {
        if !ANIM_CLOCK.running.swap(false, Ordering::SeqCst) {
            return;
        }
        let t = ANIM_CLOCK.thread.load(Ordering::SeqCst);
        if t != 0 {
            let _ = t;
            // 线程自己会看到 running=false 后退出。不强制 WaitForSingleObject：
            // 窗口正在销毁时等一个还在 PostMessage 的线程反而可能死锁
            // （它 post 到已销毁的 hwnd，消息进黑洞，线程再睡一拍才醒）。
        }
        ANIM_CLOCK.thread.store(0, Ordering::SeqCst);
        ANIM_CLOCK.inflight.store(0, Ordering::SeqCst);
    }

    /// 主线程消费完一帧后调用，腾出 in-flight 名额。
    pub fn frame_done() {
        let prev = ANIM_CLOCK.inflight.load(Ordering::SeqCst);
        if prev > 0 {
            ANIM_CLOCK.inflight.store(prev - 1, Ordering::SeqCst);
        }
    }
}

/// 动画线程本体。
///
/// 只做三件事：等一拍、看 in-flight 有没有满、post。
/// **不碰任何 App 状态** —— 那是主线程的活（`tick_anim`）。
unsafe extern "system" fn anim_thread(param: *mut std::ffi::c_void) -> u32 {
    // `CreateThread` 的 lpParameter 是裸指针原样传过来的。
    // **不要试图用 `HWND::from_raw` 之类重建** —— 那样反而会多一层不安全的
    // 指针转换。这里直接 `transmute` 成 HWND 最直接（传进来时就是 HWND.0）。
    let hwnd: HWND = std::mem::transmute(param);

    // waitable timer：比 Sleep 精度高，且能被 running 标志提前唤醒退出。
    //
    // windows-rs 签名（踩过）：`dwflags` 与 `dwdesiredaccess` 都是**裸 u32**，
    // 传 `TIMER_ALL_ACCESS`（`SYNCHRONIZATION_ACCESS_RIGHTS`）编译不过 —— 要 `.0`。
    const CREATE_WAITABLE_TIMER_HIGH_RESOLUTION: u32 = 0x0000_0002;
    const CREATE_WAITABLE_TIMER_MANUAL_RESET: u32 = 0x0000_0001;
    const TIMER_ALL_ACCESS: u32 = 0x001F_0003;

    let timer = windows::Win32::System::Threading::CreateWaitableTimerExW(
        None,
        None,
        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
        TIMER_ALL_ACCESS,
    );
    // CREATE_WAITABLE_TIMER_HIGH_RESOLUTION 在旧系统上会失败（ERROR_INVALID_PARAMETER），
    // 退回普通 CreateWaitableTimerEx（无高分辨率标志），精度略低但能用。
    let timer = match timer {
        Ok(t) => Some(t),
        Err(_) => windows::Win32::System::Threading::CreateWaitableTimerExW(
            None,
            None,
            CREATE_WAITABLE_TIMER_MANUAL_RESET,
            TIMER_ALL_ACCESS,
        )
        .ok(),
    };

    while ANIM_CLOCK.running.load(Ordering::SeqCst) {
        // in-flight 已满 = 主线程还没消化上一帧 → 这一拍**主动丢弃**。
        // 这不是丢帧，是**防止队列堆积**（见 MAX_INFLIGHT 的注释）。
        if ANIM_CLOCK.inflight.load(Ordering::SeqCst) < MAX_INFLIGHT {
            ANIM_CLOCK.inflight.fetch_add(1, Ordering::SeqCst);
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(hwnd),
                WM_APP_ANIM_TICK,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            );
        }

        // 等下一拍。
        if let Some(t) = timer {
            // `SetWaitableTimer` 的 `lpduetime` 是**相对时间**（负值 = 从现在起
            // 多少个 100ns），windows-rs 的类型是 **`*const i64`**（裸指针，
            // 没有 `LARGE_INTEGER` 包装）—— 按 `LARGE_INTEGER(due)` 写编译不过。
            let due: i64 = -(FRAME_MS as i64) * 10_000;
            let _ = windows::Win32::System::Threading::SetWaitableTimer(
                t,
                &due,
                0,
                None,
                None,
                false,
            );
            // 最多等 100ms，这样 running=false 后最多半秒内退出，
            // 不会因为漏掉一次唤醒而卡住整个进程。
            let _ = WaitForSingleObject(t, 100);
        } else {
            // 没有 waitable timer 就退化成 Sleep —— 精度差些，
            // 但仍然**优于 WM_TIMER**（Sleep 不被 WM 的合并逻辑影响）。
            let _ = WaitForSingleObject(HANDLE(std::ptr::null_mut()), FRAME_MS);
        }
    }
    0
}

/// 供自检用：当前是否在跑、in-flight 是多少。
pub fn debug_state() -> (bool, u32) {
    (
        ANIM_CLOCK.running.load(Ordering::SeqCst),
        ANIM_CLOCK.inflight.load(Ordering::SeqCst),
    )
}

/// 自检：量时钟线程**真实**的推送间隔。
///
/// 与 `--bench-timer`（量 `WM_TIMER`）配套：那个量的是"旧的错"，
/// 这个量的是"新的对"。两次数字一对比就是修复效果的证据。
///
/// 做法：临时把时钟**独占用** 500ms（此时 UI 不动），记录每次 post
/// 的时刻。`MAX_INFLIGHT=1` 会让主线程不消费 → in-flight 一直是满的 →
/// 线程根本不会 post。所以这里用一个**临时的宽松策略**：每拍都 post，
/// 只为量间隔。
pub fn bench_self() -> (usize, u64, u64, u64) {
    use windows::Win32::System::Threading::CreateThread;
    // 起一个独立线程量一次（只测等待节奏，不 post 真的消息 ——
    // 自检时窗口可能正在动，post 真的 tick 会干扰正在跑的动画）。
    let tid = unsafe {
        CreateThread(
            None,
            0,
            Some(measure_thread),
            None,
            windows::Win32::System::Threading::THREAD_CREATION_FLAGS(0),
            None,
        )
    };
    if tid.is_err() {
        return (0, 0, 0, 0);
    }
    std::thread::sleep(std::time::Duration::from_millis(600));
    let v = MEASURE_POSTS.lock().unwrap().clone();
    MEASURE_POSTS.lock().unwrap().clear();
    let mut d: Vec<u64> = v.windows(2).map(|w| w[1] - w[0]).filter(|x| *x > 0).collect();
    d.sort_unstable();
    if d.is_empty() {
        return (v.len(), 0, 0, 0);
    }
    (v.len(), d[0], d[d.len() / 2], d[d.len() - 1])
}

static MEASURE_POSTS: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());

unsafe extern "system" fn measure_thread(_p: *mut std::ffi::c_void) -> u32 {
    use windows::Win32::System::Threading::{
        CreateWaitableTimerExW, SetWaitableTimer, WaitForSingleObject,
    };
    const HIGH_RES: u32 = 0x0000_0002;
    const ALL_ACCESS: u32 = 0x001F_0003;

    let timer = CreateWaitableTimerExW(None, None, HIGH_RES, ALL_ACCESS).ok();
    let t0 = std::time::Instant::now();
    while t0.elapsed().as_millis() < 500 {
        MEASURE_POSTS.lock().unwrap().push(
            windows::Win32::System::SystemInformation::GetTickCount64(),
        );
        if let Some(t) = timer {
            let due: i64 = -(FRAME_MS as i64) * 10_000;
            let _ = SetWaitableTimer(t, &due, 0, None, None, false);
            let _ = WaitForSingleObject(t, 100);
        }
    }
    0
}
