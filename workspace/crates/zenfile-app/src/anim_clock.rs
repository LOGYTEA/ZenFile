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

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use windows::Win32::Foundation::{HANDLE, HWND, RECT};
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
    ///
    /// 返回递减后的 in-flight 值，供埋点记录（判假设 (d)：若它没降到 0，
    /// 说明递减逻辑本身失真，或有一帧被 `frame_done` 减了两次）。
    ///
    /// **注意这里用的是 load + store 而不是 `fetch_sub`**：这是既有行为，
    /// 本轮**刻意不改** —— 改它就改变了动画驱动语义，混进埋点结论里就
    /// 分不清"测出来的长尾"和"我顺手改出来的东西"了。埋点只观察。
    pub fn frame_done() -> u32 {
        let prev = ANIM_CLOCK.inflight.load(Ordering::SeqCst);
        if prev > 0 {
            ANIM_CLOCK.inflight.store(prev - 1, Ordering::SeqCst);
        }
        ANIM_CLOCK.inflight.load(Ordering::SeqCst)
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
        //
        // 埋点（第69轮）：这里同时记下 post 前后的 in-flight。丢帧的那一拍
        // 也会留记录（`t_handler_enter=0`），否则"这一拍没 post"与"post 了
        // 但主线程没取"在 CSV 里长得一样，无法区分假设 (a) 与 (b)。
        let before = ANIM_CLOCK.inflight.load(Ordering::SeqCst);
        if before < MAX_INFLIGHT {
            ANIM_CLOCK.inflight.fetch_add(1, Ordering::SeqCst);
            // post **之前**取时刻：差值 `t_post → t_handler_enter` 里含的
            // 就是 PostMessage 本身 + 排队，两者在本埋点精度下无法再分开，
            // 但这对定位已经足够（PostMessage 是微秒级，排队是毫秒级）。
            crate::anim_clock::trace_post(before, before + 1);
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(hwnd),
                WM_APP_ANIM_TICK,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            );
        } else {
            crate::anim_clock::trace_post(before, before);
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

// ====================================================================
// 帧链路时间戳埋点（第69轮）
// ====================================================================
//
// # 要回答的问题
//
// 已知事实：中位帧间隔 16.6ms、UI 线程 P95 响应 2.77ms（每帧计算很便宜），
// 但存在 **50~80ms 的尾部长延迟**。四个互斥假设：
//
//   (a) 后台线程没按时 post（waitable timer 精度 / 线程被调度出去）
//   (b) post 了，但消息在队列里排队很久才被主线程取走
//   (c) 消息很快被消费，但 InvalidateRect 之后合成/上屏延迟
//   (d) frame_done() 的调用位置导致 in-flight 计数失真
//
// 区分它们只需要**同一帧的若干时刻**：
//
//   post → handler_enter 大        ⇒ (a) 或 (b)
//   handler_enter → paint_exit 大  ⇒ 主线程在算或在排别的消息
//   paint_enter → paint_exit 大    ⇒ (c) 的绘制部分
//
// 把 (a) 和 (b) 分开的**唯一**办法是看 in-flight：
//
//   * 帧间隔大 **且** `inflight_before` 恒为 0 ⇒ 线程是"到点就 post"，
//     post 完到被取走之间隔了很久 = 消息躺在队列里 ⇒ **(b) 消息排队**。
//   * `inflight_before` 频繁为 1（= MAX_INFLIGHT）⇒ 线程是被主线程
//     卡住才没 post，队列侧已经堆着 ⇒ (b) 的极端形态（堆积）。
//   * 帧间隔大 **且** 帧间隔 ≈ FRAME_MS 的整数倍 ⇒ 更像 (a)（定时器
//     本身被量化/线程被调度出去），而不是队列忙。
//
// # 时间基准：为什么是 QPC 而不是 GetTickCount64
//
// `GetTickCount64` 默认**只有 ~15.6ms 分辨率**（等于系统时钟节拍）。
// 本任务要量的差异是 **2ms 级**（UI 线程 P95 才 2.77ms）：用它的话
// 一半以上采样会直接是 0，整个分布被量化噪声淹没。
// `QueryPerformanceCounter` 是硬件计数器（现代 Windows 上频率 = 实际
// CPU 晶振，10MHz~24MHz 不等），分辨率在纳秒级。
//
// 注意这跟 `anim_thread` 里的 `FRAME_MS` 无关 —— 那走 waitable timer。
//
// # 零常态开销：这是本模块唯一的硬约束
//
// 埋点插在**每帧 6 个位置**（post / handler / tick / sync / paint×2）。
// 常态下若每处都 `QueryPerformanceCounter` + `Mutex::lock` + `push`，
// 埋点本身就成了它要测量的那个瓶颈 —— 测出来的长尾全是自己造的。
//
// 所以每处都写成 `if TRACE_ON.load(Relaxed) { ... }`：关闭时机器码就是
// **一次 relaxed 原子读 + 一个跳转**（约 1~2ns），没有系统调用、没有
// 取锁、没有分配。对比一帧 2.77ms 的实际计算量是 6 个数量级以下。
//
// 用 `Relaxed` 而不是 `SeqCst` 是安全的：这里只需要"本线程刚写的
// true 自己看得到"，不需要任何跨线程**顺序**保证（没有数据依赖
// TRACE_ON 去保护别的内存）。dump 时会显式先写 false 再取锁，保证
// 读到的是关闭点之前的完整数据（见 `dump_trace`）。
//
// # 锁的纪律
//
// 后台线程和主线程**都**要写这个缓冲，所以要锁。纪律：锁只包住
// 一次字段写入 / 一次 push，**持锁期间不调用任何别的函数**（不取
// QPC、不碰 App、不做 I/O）。临界区因此只有几十纳秒，不存在 UI
// 线程等后台线程的可能。格式化与文件 I/O 全部在锁外。

use std::sync::Mutex;

/// 环境变量名。置为 `1` 才开埋点。
pub const TRACE_ENV: &str = "ZENFILE_ANIM_TRACE";

/// 埋点总开关。默认 false = 常态零开销。
static TRACE_ON: AtomicBool = AtomicBool::new(false);

/// **动画帧**环形缓冲容量（第74轮：拆分环形缓冲，见 `TRACE_CAP_STATIC`）。
///
/// 依据：一次 `--anim-trace` 录制最长 `--anim-trace 13000` 毫秒，
/// 时钟线程按 15ms 一拍 → 13000/15 ≈ 867 条；开录前还有 1s 预热不录。
/// 取 4096 留了约 4.7 倍余量。**这个值本轮不变** —— 动画帧的数量上限
/// 由"录制时长 ÷ 帧间隔"决定，与用户操作强度无关（`MAX_INFLIGHT=1`
/// 保证 in-flight ≤1，节奏不会因为主线程忙而变快）。
const TRACE_CAP_ANIM: usize = 4096;

/// **非动画（静态）paint** 环形缓冲容量（第74轮新增）。
///
/// # 为什么必须另开一个环，而不是往同一个环里塞
///
/// 同一个环、容量几千、满了"丢最旧"，会出现这个后果：
/// **一次窗口 resize 的上百次 `WM_PAINT` 会把正在录的动画帧整体挤掉**。
/// 而"静态重绘的单次绝对成本"恰恰是本轮要量的东西（低配机器卡不卡
/// 取决于单次重绘多久），它和"动画帧间隔分布"是两组互不替代的数据。
/// 挤掉就等于两个都测不准。所以拆成两个独立的环：
///
/// * `TRACE_BUF`（动画）—— 只由后台时钟线程 `trace_post` 追加，
///   容量见上，**静态 paint 一个字节都碰不到它**；
/// * `TRACE_STATIC_BUF`（静态）—— 只由主线程 `trace_paint_enter` 追加，
///   只会被别的静态 paint 挤掉。
///
/// # 16384 这个数怎么来的
///
/// 静态 paint 的产生速率上界 = 重绘频率。窗口拖动/resize 时系统按
/// 屏幕刷新率连续发 `WM_PAINT`，144Hz 屏 ≈ 144 次/秒；一次从最大化到
/// 还原的完整往返外加连续拖边，本轮实测 13 秒录制里静态 paint 的量级
/// 是**几百到一两千**。16384 能覆盖"连续 113 秒不间断 resize"，
/// 对 13 秒的录制来说是 1000 倍余量。
///
/// 代价：`FrameRec` 384 字节（字段见下），16384 × 384 ≈ 6.3MB，
/// 加上动画环 4096 × 384 ≈ 1.6MB，合计约 7.9MB 常驻 —— 对一个资源
/// 管理器可接受，且**只在 `TRACE_ON` 为真并真的发生过 push 时才分配**
/// （`Vec::new()` 不分配；关着 trace 时这块内存根本不存在）。
///
/// # 满了怎么办
///
/// 仍是**覆盖最旧的**（丢最旧而不是拒新），理由与动画环一致：排查要看
/// "最近发生了什么"。但**被丢掉的数量必须记账**（`TRACE_STATIC_DROPPED`，
/// 写进 `.meta`）—— 否则"没录到"和"没发生"在 CSV 里长得一样，
/// 和本轮修的这个缺陷是同一类错误。
const TRACE_CAP_STATIC: usize = 16384;

/// **静态 paint 的 seq 基址**（第74轮）。
///
/// 静态记录与动画记录**写进同一份 CSV**，所以两者的 `seq` 必须能一眼
/// 分开。给静态记录加一个 `1<<48` 的偏移：动画 seq 是录制期内单调递增的
/// 小整数（867 条录制 ⇒ 最大 866），静态 seq 是 281474976710656 + n。
/// 两段数值不可能相交，**即使分析脚本完全不看 `kind` 列也不会混**。
pub const STATIC_SEQ_BASE: u64 = 1u64 << 48;

/// CSV 里的 `kind` 列取值：这是一条**动画帧**记录（`trace_post` 建的）。
pub const KIND_ANIM: u32 = 0;
/// CSV 里的 `kind` 列取值：这是一次**静态 / 非动画** `WM_PAINT` 记录
/// （`trace_paint_enter` 建的，`seq >= STATIC_SEQ_BASE`）。
pub const KIND_PAINT: u32 = 1;

/// CSV `kind` 列里写出来的**文本**（`0` / `1`）。
const KIND_ANIM_TXT: &str = "anim";
const KIND_PAINT_TXT: &str = "paint";

/// `TRACE_ACTIVE_PAINT`：当前这次 paint 的记录在**哪个环**里。
///
/// * `ACTIVE_NONE` —— 没有正在进行的 paint；
/// * `ACTIVE_ANIM` —— 在动画环（`TRACE_BUF`）；
/// * `ACTIVE_PAINT` —— 在静态环（`TRACE_STATIC_BUF`）。
///
/// 第74轮新增。第73版靠"倒着扫最近一条 `t_paint_enter != 0` 的记录"来
/// 定位，而现在两种记录**同时存在于两个环里**，倒扫会认错行（动画帧的
/// 时刻可能被写到一条静态记录上）。改成显式发布"在哪个环"。
const ACTIVE_NONE: u32 = 0;
const ACTIVE_ANIM: u32 = 1;
const ACTIVE_PAINT: u32 = 2;

/// **自检计数器**：本进程一共记过多少条 paint 记录（动画 + 静态）。
///
/// 第74轮：这是"看起来在采但其实全丢"的哨兵。配套的断言见
/// `TRACE_SUB_STEPS_WRITTEN`。
static TRACE_PAINTS_RECORDED: AtomicU64 = AtomicU64::new(0);

/// **自检计数器**：`sub_*` 打点被调用的总次数（begin + end 都算）。
static TRACE_SUB_STEPS_TOTAL: AtomicU64 = AtomicU64::new(0);

/// **自检计数器**：`sub_*` 打点里**真的写进了某条记录**的次数。
///
/// # 为什么必须是这一个，而不是 `sub_steps_total`
///
/// 任务书给的断言是 `paints_recorded * 16 >= sub_steps_total`。这个式子
/// **在本项目的代码里恒不成立**：一次完整的 `paint()` 会打 15 对
/// `trace_sub_begin/end` = **30 次**，恒 > 16，所以只要有一次完整 paint
/// 断言就必然为 0 —— 它测不出"丢数据"，只会恒定误报。
///
/// 真正能区分"采到了"与"全丢了"的量是 `sub_steps_written`：打点被调用
/// **并且成功写进了一条记录**的次数。旧版缺陷下 `paint()` 每次走完 15 个
/// 打点，但 `TRACE_PAINT_OWNED == false` 让它们全部只累加计数器、
/// 一条记录都不写 —— 即 `sub_steps_written == 0` 而
/// `paints_recorded > 0`。这才是哨兵。
///
/// # 判据
///
/// 每次 `paint()` 至少有**一对**完整的 `begin`/`end`
/// （`SUB_BEGIN_ANIM` 是无条件执行的，它不是 `if` 分支），所以
/// `sub_steps_written >= paints_recorded * 2` 是"每次 paint 都落盘了
/// 子步骤"的充要下限。留 2 而不是 1 是为了容忍某一侧边界不完整。
static TRACE_SUB_STEPS_WRITTEN: AtomicU64 = AtomicU64::new(0);

/// 自检是否通过（见 `TRACE_SUB_STEPS_WRITTEN` 的说明）。
#[inline]
pub fn trace_selftest_ok() -> bool {
    let p = TRACE_PAINTS_RECORDED.load(Ordering::Relaxed);
    let w = TRACE_SUB_STEPS_WRITTEN.load(Ordering::Relaxed);
    w >= p * 2
}

/// "没有帧号"哨兵。
const NO_SEQ: u64 = u64::MAX;

/// 一帧的完整时间线。时刻都是 QPC 原始计数（未换算），存整数，
/// 由分析脚本按 `qpc_hz` 换算 —— 换算留在脚本里，程序侧不做浮点。
///
/// 时刻为 0 = 这一帧没走到那个阶段（例如没触发重绘就没有 paint 时刻）。
#[derive(Clone, Copy, Default)]
pub struct FrameRec {
    /// 帧序号，**后台线程侧**单调递增。
    ///
    /// 这是把两侧记录配对的唯一依据：主线程拿不到"post 时刻对应哪个
    /// 帧号"（`PostMessageW` 的 lparam 是 0；改它就等于改了动画驱动
    /// 逻辑，超出"只埋点"的范围），所以配对靠"当前待绘的那一帧"。
    pub seq: u64,
    /// 后台线程 post **之前**的 QPC 时刻。
    pub t_post: u64,
    /// post 之前的 in-flight 值。判 (d) 与区分 (a)/(b) 的关键字段。
    pub inflight_before: u32,
    /// post 之后（fetch_add 之后）的 in-flight 值。
    pub inflight_after: u32,
    /// `frame_done()` 之后的 in-flight 值。判 (d) 直接证据：
    /// 若它没降到 0，说明递减本身失真。
    pub inflight_after_done: u32,
    /// 主线程进入 `WM_APP_ANIM_TICK` 分支的 QPC 时刻。
    ///
    /// `t_handler_enter - t_post` = 投递 + 排队延迟。
    pub t_handler_enter: u64,
    /// `app.tick_anim()` 返回后。
    pub t_tick_end: u64,
    /// `sync_anim_timer()` 返回后。
    pub t_sync_end: u64,
    /// `frame_done()` 调用后。判 (d)。
    pub t_frame_done: u64,
    /// 进入 `paint::paint` 的 QPC 时刻（`WM_PAINT` 分支，BeginPaint 之后）。
    pub t_paint_enter: u64,
    /// `paint::paint` 返回后。
    pub t_paint_exit: u64,
    /// 本次 paint 归属于哪个 seq。
    ///
    /// 动画帧：等于该帧的 `seq`（`>= 0` 且 `< STATIC_SEQ_BASE`）。
    /// 静态 paint：等于它自己那条记录的 `seq`（`>= STATIC_SEQ_BASE`）。
    ///
    /// **第74轮改动**：以前"归不上任何动画帧"的 paint 记 `NO_SEQ` 且整条
    /// 记录被丢弃 —— 那正是本轮修掉的缺陷（1260 次 paint 一行没落盘）。
    /// 现在静态 paint **自己也有一条记录**，`paint_seq` 就是它自己的 seq。
    pub paint_seq: u64,
    /// 记录类别（CSV 的 `kind` 列）：`KIND_ANIM` / `KIND_STATIC`。
    ///
    /// 与 `seq >= STATIC_SEQ_BASE` 恒等，单列出来是为了让分析脚本不必
    /// 记住偏移常量（脚本里 `kind == 1` 就是静态帧）。
    pub kind: u32,
    /// paint 里那句无条件 `InvalidateRect(hwnd_search)` 是否执行了。
    /// 本轮**只记录、不改动**（`paint.rs` 那两句一个字没动）。
    pub invalidated_search: u32,
    /// 同上，地址框。
    pub invalidated_address: u32,

    // ----------------------------------------------------------------
    // 第70轮：`paint()` 内部逐子函数耗时（同一份 `TraceBuf`，同一行 CSV）
    // ----------------------------------------------------------------
    //
    // 设计要点：
    //
    // * **不另起缓冲**。子函数时刻必须和 `t_paint_enter / t_paint_exit`
    //   在**同一条记录**里，否则两份数据要靠 seq 二次关联，join 出错就
    //   悄悄得出错误结论。追加字段是 O(1)、无分配。
    // * 存 **QPC 原始计数**（u64），不存毫秒浮点 —— 换算统一在分析脚本。
    // * `0` = 这一步这一帧**没执行**（例如 `sidebar_visible == false`，
    //   `paint_sidebar` 根本没被调用；分析脚本必须跳过 0，不能当成"耗时 0"
    //   混进统计 —— 那会把中位数系统性拉低）。
    //
    // 步编号与 `SUB_*` 常量一一对应，顺序 = `paint()` 里的实际调用顺序。
    /// 每一步的**开始**时刻（QPC 原始计数）。下标见 `SUB_*`。
    pub sub_begin: [u64; SUB_N],
    /// 每一步的**结束**时刻。`sub_e[i] - sub_begin[i]` = 该步耗时。
    pub sub_end: [u64; SUB_N],

    // ----------------------------------------------------------------
    // 第70轮：区分"快帧 / 慢帧"的判据（每帧一次，在 layout 之后采）
    // ----------------------------------------------------------------
    //
    // 上一轮测到 paint 耗时**双峰且中间 10~24ms 完全为空** —— 同一段代码
    // 一半帧 6ms 以内、另一簇 24~40ms。内容"多了一点"只会让分布变宽，
    // 不会挖出空洞，所以更像是**走了不同的代码路径**。这些字段就是为了
    // 把"不同的路径"量化成可比较的数字。
    //
    // 全部只读、不改状态；`anim_active()` 本身是纯函数（只读字段），
    // 这里额外调它不会引入副作用。
    /// 客户区宽 / 高（像素）。
    pub d_w: u32,
    /// 客户区高（像素）。
    pub d_h: u32,
    /// 分栏数（1 = 不分栏）。
    pub d_panes: u32,
    /// 当前标签的**目录条目总数**（筛选前）。
    pub d_entries: u32,
    /// 当前标签**实际要画出来的行数**（筛选后）。列表越长 paint 越贵？
    pub d_visible: u32,
    /// 视图模式（0=Details 1=List 2=MediumIcons 3=LargeIcons）。
    pub d_viewmode: u32,
    /// `l.view` 面积（像素²）。
    pub d_view_area: u64,
    /// 正在放大动画中的图标数。
    pub d_icon_zoom: u32,
    /// 正在"弹一下"的条目数。
    pub d_icon_bounce: u32,
    /// 标题栏按键悬停动画数。
    pub d_cap_anim: u32,
    /// 状态位图（见 `CTX_*` 常量）。
    pub d_flags: u32,
    /// `app.anim_active()` 的结果（1 = 此刻确实有缓动在跑）。
    pub d_anim_active: u32,
}

// ====================================================================
// 第70轮：paint 内部步编号 / 判据位 / 子窗口类别
// ====================================================================

/// `paint()` 内部逐段计时的步数。顺序 = `paint()` 里的实际调用顺序。
///
/// 编号**只增不改**：CSV 里每一步占两列（`_b` / `_e`），分析脚本按名字
/// 取列；一旦重排，历史 CSV 的语义就变了。
pub const SUB_N: usize = 15;

/// 每一步在 CSV 里的列名前缀（`_b` = begin，`_e` = end）。
///
/// 与 `SUB_*` 编号**严格同序** —— `SUB_NAMES[i]` 就是第 `i` 步。
pub const SUB_NAMES: [&str; SUB_N] = [
    "sub_begin_anim",
    "sub_create_surface",
    "sub_layout",
    "sub_fill",
    "sub_title",
    "sub_toolbar",
    "sub_sidebar",
    "sub_panes",
    "sub_status",
    "sub_drag_overlay",
    "sub_dock_overlay",
    "sub_bitblt",
    "sub_cleanup",
    "sub_invalidate",
    "sub_sync_timer",
];

/// `app.begin_anim_frame()`。
pub const SUB_BEGIN_ANIM: usize = 0;
/// `CreateCompatibleDC` + `CreateCompatibleBitmap` + `SelectObject(mem,bmp)`。
///
/// **重点嫌疑**：每帧新建一张**全窗口**大小的位图。
pub const SUB_CREATE_SURFACE: usize = 1;
/// `layout_sized(app, w, h)`。遍历约 200 处窗格 / 标签 / 列宽计算。
pub const SUB_LAYOUT: usize = 2;
/// 整窗 `FillRect` 清屏。
pub const SUB_FILL: usize = 3;
/// `paint_title_bar`。
pub const SUB_TITLE: usize = 4;
/// `paint_toolbar`。
pub const SUB_TOOLBAR: usize = 5;
/// `paint_sidebar`（仅 `sidebar_visible` 时执行）。
pub const SUB_SIDEBAR: usize = 6;
/// `paint_panes` —— 渲染整个文件列表。**重点嫌疑**。
pub const SUB_PANES: usize = 7;
/// `paint_status`（仅 `statusbar_visible` 时执行）。
pub const SUB_STATUS: usize = 8;
/// `paint_drag_overlay`（仅拖拽中执行）。
pub const SUB_DRAG_OVERLAY: usize = 9;
/// `paint_dock_overlay`（仅贴靠预览中执行）。
pub const SUB_DOCK_OVERLAY: usize = 10;
/// 全窗口 `BitBlt(mem → hdc)`。**重点嫌疑**：全窗口上传。
pub const SUB_BITBLT: usize = 11;
/// `SelectObject` / `DeleteObject(bmp)` / `DeleteDC(mem)` 的清理。
pub const SUB_CLEANUP: usize = 12;
/// 那两句无条件 `InvalidateRect(子EDIT, erase=true)`。**重点嫌疑**。
pub const SUB_INVALIDATE: usize = 13;
/// `app.sync_anim_timer()`（在 `paint()` 末尾，窗口清理之后）。
pub const SUB_SYNC_TIMER: usize = 14;

/// `d_flags` 位：地址栏滑入 / 滑出动画在跑。
pub const CTX_ADDR_ANIM: u32 = 1 << 0;
/// `d_flags` 位：侧栏导航窗格平滑滚动在跑。
pub const CTX_SB_SCROLL: u32 = 1 << 1;
/// `d_flags` 位：侧栏"当前位置"指示条正在缓动。
pub const CTX_SB_ACTIVE: u32 = 1 << 2;
/// `d_flags` 位：侧栏悬停高亮条正在缓动。
pub const CTX_SB_HOVER: u32 = 1 << 3;
/// `d_flags` 位：正在拖拽（框选 / 拖文件）。
pub const CTX_DRAG: u32 = 1 << 4;
/// `d_flags` 位：正在拖窗格分隔线。
pub const CTX_DIV_DRAG: u32 = 1 << 5;
/// `d_flags` 位：侧栏可见（决定 `paint_sidebar` 跑不跑）。
pub const CTX_SIDEBAR_VISIBLE: u32 = 1 << 6;
/// `d_flags` 位：状态栏可见。
pub const CTX_STATUS_VISIBLE: u32 = 1 << 7;
/// `d_flags` 位：当前标签是"主页"虚拟目录。
pub const CTX_HOME: u32 = 1 << 8;
/// `d_flags` 位：当前标签是"回收站"。
pub const CTX_RECYCLE: u32 = 1 << 9;
/// `d_flags` 位：当前标签是"设置"页。
pub const CTX_SETTINGS: u32 = 1 << 10;
/// `d_flags` 位：当前标签正在加载。
pub const CTX_LOADING: u32 = 1 << 11;
/// `d_flags` 位：存在列筛选（画表头筛选箭头 + 行可能被过滤）。
pub const CTX_COL_FILTER: u32 = 1 << 12;
/// `d_flags` 位：分栏（`panes > 1`）。
pub const CTX_SPLIT: u32 = 1 << 13;
/// `d_flags` 位：存在图标缩放目标（本帧至少有一个图标在放大）。
pub const CTX_ICON_ZOOM_TGT: u32 = 1 << 14;

// --------------------------------------------------------------------
// 子窗口分类：主窗口 / 搜索框 / 地址框 / 其余 EDIT（重命名框等）
// --------------------------------------------------------------------

/// 子窗口分类总数（CSV `.child` 里的 `kind`）。
pub const CHILD_KINDS: usize = 4;
/// 主窗口。
pub const CHILD_MAIN: usize = 0;
/// 搜索框（`hwnd_search`，类 `EDIT` + 子类化 `search_proc`）。
pub const CHILD_SEARCH: usize = 1;
/// 地址框（`hwnd_address`，类 `EDIT` + 子类化 `addr_proc`）。
pub const CHILD_ADDR: usize = 2;
/// 其余 `EDIT` 子窗口（重命名框等）—— 单列一栏，避免"漏在统计外"。
pub const CHILD_OTHER: usize = 3;

/// `.child` 摘要里每类子窗口的行名（与 `CHILD_*` 编号同序）。
pub const CHILD_KIND_NAMES: [&str; CHILD_KINDS] = ["main", "search", "addr", "other"];

/// 每类子窗口的 `WM_PAINT` 次数。
static CHILD_PAINT_CNT: [AtomicU64; CHILD_KINDS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
/// 每类子窗口 `WM_ERASEBKGND` 次数。
///
/// `InvalidateRect(hwnd, None, true)` 的 `erase=true` 就是"先发一次
/// `WM_ERASEBKGND` 再发 `WM_PAINT`"，所以这个计数直接回答"每帧额外触发
/// 了几次背景擦除"。
static CHILD_ERASE_CNT: [AtomicU64; CHILD_KINDS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
/// 每类子窗口 `WM_PAINT` 累计耗时（QPC 原始计数之和）。
static CHILD_PAINT_NS: [AtomicU64; CHILD_KINDS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
/// 每类子窗口 `WM_PAINT` 单次最大耗时（QPC 原始计数）。
static CHILD_PAINT_MAX: [AtomicU64; CHILD_KINDS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// 每次子窗口 `WM_PAINT` 的原始耗时样本（QPC 差值）。
///
/// 环形（满了清空重记）—— 只给分位数用，不需要全量。
static CHILD_SAMPLES: Mutex<[Vec<u64>; CHILD_KINDS]> = Mutex::new([
    Vec::new(),
    Vec::new(),
    Vec::new(),
    Vec::new(),
]);

/// 子窗口样本环形容量。
const CHILD_SAMPLE_CAP: usize = 4096;

/// 子窗口 `WM_PAINT` 开始时刻（按类别），`trace_child_paint_exit` 用。
static CHILD_ENTER: [AtomicU64; CHILD_KINDS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// "当前这个 paint 已认领了一个动画帧" —— 决定内部步打点往哪写。
///
/// 必须单独一个标志而不是靠 `TRACE_PENDING`：孤儿 paint（子 EDIT 的
/// `WM_PAINT`、悬停重绘）会把 `PENDING` 换走，若内部步跟着 PENDING 写就会
/// 污染某一帧的时刻。
static TRACE_PAINT_OWNED: AtomicBool = AtomicBool::new(false);

/// 本次 paint 认领到的 seq（内部步打点靠它 O(1) 定位记录）。
static TRACE_CLAIMED_SEQ: AtomicU64 = AtomicU64::new(NO_SEQ);

/// 认不上一帧的 paint 里，内部步打点被调用的次数（数据健全性自检）。
///
/// 正常应为 0：孤儿 paint 不进 `paint()`（子 EDIT 走自己的窗口过程）。
/// 若不为 0，说明有别的路径调了 `paint()`，结论要打问号。
static TRACE_SUB_UNCLAIMED: AtomicU64 = AtomicU64::new(0);

/// 真正的 O(1) 环形缓冲。
///
/// **不用 `Vec` + `remove(0)`**：那是 O(n) 搬移，4096 条 × 56 字节
/// ≈ 220KB 的 memmove **每帧一次** —— 埋点自己就成了它要测量的瓶颈。
///
/// 也不用"固定数组 + AtomicUsize 写指针"的无锁写法：那种写法 dump
/// 时可能读到正被后台线程改写的记录，算出 `t_handler_enter < t_post`
/// 这种物理上不可能的值，污染结论。**分析工具的数值可信度比省下的
/// 几十纳秒重要**。
struct TraceBuf {
    cap: usize,
    /// 最老一条的下标。
    head: usize,
    /// 有效条数。
    len: usize,
    items: Vec<FrameRec>,
    /// 因"丢最旧"被覆盖掉的条数（第74轮新增，静态环记账用）。
    dropped: u64,
}

impl TraceBuf {
    const fn new() -> Self {
        Self { cap: TRACE_CAP_ANIM, head: 0, len: 0, items: Vec::new(), dropped: 0 }
    }

    /// 静态环专用构造：容量 `TRACE_CAP_STATIC`。
    const fn static_new() -> Self {
        Self { cap: TRACE_CAP_STATIC, head: 0, len: 0, items: Vec::new(), dropped: 0 }
    }

    /// 第 `i` 条（`0` = 最老，`len-1` = 最新）。
    #[inline(always)]
    fn at(&self, i: usize) -> &FrameRec {
        &self.items[(self.head + i) % self.cap]
    }

    #[inline(always)]
    fn at_mut(&mut self, i: usize) -> &mut FrameRec {
        let k = (self.head + i) % self.cap;
        &mut self.items[k]
    }

    #[inline(always)]
    fn newest_seq(&self) -> u64 {
        if self.len == 0 {
            NO_SEQ
        } else {
            self.at(self.len - 1).seq
        }
    }

    /// 按帧号找下标。**O(1)**：缓冲里存的是一段**连续**的 seq
    /// （只有 dump 才清空），所以从最新一条往回数就是 seq 往回减。
    #[inline(always)]
    fn find(&self, seq: u64) -> Option<usize> {
        if self.len == 0 || seq == NO_SEQ {
            return None;
        }
        let newest = self.newest_seq();
        if seq > newest {
            return None;
        }
        let back = (newest - seq) as usize;
        if back >= self.len {
            return None;
        }
        Some(self.len - 1 - back)
    }

    #[inline(always)]
    fn push(&mut self, r: FrameRec) {
        if self.len < self.cap {
            // 填充阶段 head 恒为 0，于是 slot == len == items.len()，
            // 直接追加即可（之后一旦绕头就都走 else 分支原地覆盖）。
            if self.items.len() < self.cap {
                self.items.push(r);
            } else {
                let k = (self.head + self.len) % self.cap;
                self.items[k] = r;
            }
            self.len += 1;
        } else {
            self.items[self.head] = r;
            self.head = (self.head + 1) % self.cap;
            self.dropped += 1;
        }
    }

    fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
        self.dropped = 0;
    }

    /// 拿走全部内容（供 dump 在锁外慢慢格式化）。
    fn take_all(&mut self) -> Vec<FrameRec> {
        let items = std::mem::take(&mut self.items);
        self.head = 0;
        self.len = 0;
        self.dropped = 0;
        items
    }

    /// 因"丢最旧"被覆盖掉的条数。**必须在持锁后调用**。
    fn dropped(&self) -> u64 {
        self.dropped
    }
}

static TRACE_BUF: Mutex<TraceBuf> = Mutex::new(TraceBuf::new());

/// 静态 paint 环形缓冲（第74轮）。
///
/// 与 `TRACE_BUF` **完全独立**：满环/清空只影响自己。这样一次 resize
/// 引发的上百次 `WM_PAINT` 不会把正在录的动画帧挤掉。
static TRACE_STATIC_BUF: Mutex<TraceBuf> = Mutex::new(TraceBuf::static_new());

/// 静态 paint 序号（加 `STATIC_SEQ_BASE` 后才是 CSV 里的 `seq`）。
static TRACE_STATIC_SEQ: AtomicU64 = AtomicU64::new(0);

/// 当前这次 paint 的记录在哪个环里（`ACTIVE_*`）。
static TRACE_ACTIVE_PAINT: AtomicU32 = AtomicU32::new(ACTIVE_NONE);

/// 后台线程侧的帧序号。只有开 trace 时才自增。
static TRACE_SEQ: AtomicU64 = AtomicU64::new(0);

/// 主线程正在处理 / 等待重绘的帧号；`NO_SEQ` = 没有。
///
/// `t_handler_enter` 时置为本帧的 seq，`trace_paint_enter` 消费掉它
/// （换成 `NO_SEQ`）。这样"这一帧还没被重绘"和"这一帧已经被别的
/// paint 消费掉了"能区分开 —— 前者说明帧画丢了。
static TRACE_PENDING: AtomicU64 = AtomicU64::new(NO_SEQ);

/// 归不上任何动画帧的 `WM_PAINT` 次数（悬停 / `WM_TIMER` 引起的）。
///
/// 与帧数相除就是"每帧平均几次重绘"。若明显 > 1，说明一帧其实被画了
/// 多次（`paint.rs` 末尾那两句无条件 `InvalidateRect` 会让两个子
/// EDIT 各自再触发一次）—— 这正是假设 (c) 的头号嫌疑。
static TRACE_ORPHAN_PAINTS: AtomicU64 = AtomicU64::new(0);

/// QPC 频率（计数/秒），首次调用时查询并缓存。
static QPC_FREQ: AtomicU64 = AtomicU64::new(0);

/// 读一次 QPC 原始计数。
///
/// `#[inline(always)]`：它被内联进每个埋点位置，关掉 trace 时那一整块
/// 会被优化掉，所以这里**不需要**自己再判 `TRACE_ON`。
#[inline(always)]
pub fn qpc() -> u64 {
    let mut c: i64 = 0;
    // 失败时返回 0：宁可让这一帧时刻为 0（分析脚本能识别并跳过），
    // 也不要 panic —— 埋点绝不能成为崩溃源。
    if unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&mut c) }.is_err() {
        return 0;
    }
    c as u64
}

/// QPC 频率（计数/秒）。
pub fn qpc_hz() -> u64 {
    let cached = QPC_FREQ.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let mut f: i64 = 0;
    if unsafe { windows::Win32::System::Performance::QueryPerformanceFrequency(&mut f) }.is_err()
        || f <= 0
    {
        return 1;
    }
    let f = f as u64;
    QPC_FREQ.store(f, Ordering::Relaxed);
    f
}

/// 按 `TRACE_ENV=1` 初始化开关。进程启动时显式调一次。
///
/// 不在 `static` 初始化器里读环境变量：Rust 的 static 初始化器里不能
/// 安全调 Win32 API，且 release 下可能被去重，行为不可靠。
pub fn trace_init_from_env() -> bool {
    let on = std::env::var(TRACE_ENV).map(|v| v.trim() == "1").unwrap_or(false);
    TRACE_ON.store(on, Ordering::Relaxed);
    if on {
        // 每次开 trace 从空缓冲开始，否则会混进上一次的残留帧。
        if let Ok(mut b) = TRACE_BUF.lock() {
            b.clear();
        }
        if let Ok(mut b) = TRACE_STATIC_BUF.lock() {
            b.clear();
        }
        TRACE_SEQ.store(0, Ordering::Relaxed);
        TRACE_STATIC_SEQ.store(0, Ordering::Relaxed);
        TRACE_PAINTS_RECORDED.store(0, Ordering::Relaxed);
        TRACE_SUB_STEPS_TOTAL.store(0, Ordering::Relaxed);
        TRACE_SUB_STEPS_WRITTEN.store(0, Ordering::Relaxed);
        TRACE_ACTIVE_PAINT.store(ACTIVE_NONE, Ordering::Relaxed);
        TRACE_PENDING.store(NO_SEQ, Ordering::Relaxed);
        TRACE_ORPHAN_PAINTS.store(0, Ordering::Relaxed);
        reset_child_stats();
    }
    on
}

/// 埋点是否开着。
pub fn trace_on() -> bool {
    TRACE_ON.load(Ordering::Relaxed)
}

/// 开 / 关开关。关时顺手清空缓冲，让"一段录制 = 一份缓冲"成立。
pub fn trace_set(on: bool) {
    TRACE_ON.store(on, Ordering::Relaxed);
    if on {
        reset_child_stats();
    } else {
        TRACE_PENDING.store(NO_SEQ, Ordering::Relaxed);
        TRACE_PAINT_OWNED.store(false, Ordering::Relaxed);
        TRACE_CLAIMED_SEQ.store(NO_SEQ, Ordering::Relaxed);
        TRACE_ACTIVE_PAINT.store(ACTIVE_NONE, Ordering::Relaxed);
        if let Ok(mut b) = TRACE_BUF.lock() {
            b.clear();
        }
        if let Ok(mut b) = TRACE_STATIC_BUF.lock() {
            b.clear();
        }
    }
}

/// 归零所有子窗口统计（每次开录时清一遍）。
fn reset_child_stats() {
    TRACE_SUB_UNCLAIMED.store(0, Ordering::Relaxed);
    for k in 0..CHILD_KINDS {
        CHILD_PAINT_CNT[k].store(0, Ordering::Relaxed);
        CHILD_ERASE_CNT[k].store(0, Ordering::Relaxed);
        CHILD_PAINT_NS[k].store(0, Ordering::Relaxed);
        CHILD_PAINT_MAX[k].store(0, Ordering::Relaxed);
        CHILD_ENTER[k].store(0, Ordering::Relaxed);
    }
    if let Ok(mut v) = CHILD_SAMPLES.lock() {
        for b in v.iter_mut() {
            b.clear();
        }
    }
}

/// 环形缓冲里已录到的帧数。
fn trace_buf_len() -> usize {
    TRACE_BUF.lock().map(|b| b.len).unwrap_or(0)
}

/// 已录到的帧数。
pub fn trace_count() -> usize {
    trace_buf_len()
}

/// 归不上动画帧的 paint 次数。
pub fn trace_orphan_paints() -> u64 {
    TRACE_ORPHAN_PAINTS.load(Ordering::Relaxed)
}

// --------------------------------------------------------------------
// 埋点位置（每个都是「关闭时零成本」的那条线）
// --------------------------------------------------------------------

/// **后台线程**：post 之前打点，并建好这一帧的空记录。
///
/// inflight 已满（这一拍被丢弃）时**照样留一条记录**：它的
/// `t_handler_enter` 恒为 0、`inflight_before >= MAX_INFLIGHT`。
/// 不留的话，"这一拍根本没 post"和"post 了但主线程没取"在 CSV 里
/// 都表现为"时间段里缺一条记录"，无法区分。
#[inline(always)]
pub fn trace_post(inflight_before: u32, inflight_after: u32) {
    if TRACE_ON.load(Ordering::Relaxed) {
        let rec = FrameRec {
            seq: TRACE_SEQ.fetch_add(1, Ordering::Relaxed),
            t_post: qpc(),
            inflight_before,
            inflight_after,
            paint_seq: NO_SEQ,
            kind: KIND_ANIM,
            ..Default::default()
        };
        if let Ok(mut b) = TRACE_BUF.lock() {
            b.push(rec);
        }
    }
}

/// 取"当前待绘那一帧"的下标。**只在持锁后调用。**
#[inline(always)]
fn pending_idx(b: &TraceBuf) -> Option<usize> {
    b.find(TRACE_PENDING.load(Ordering::Relaxed))
}

/// **主线程**：进入 `WM_APP_ANIM_TICK` 分支。
#[inline(always)]
pub fn trace_handler_enter() {
    if TRACE_ON.load(Ordering::Relaxed) {
        let t = qpc();
        // seq 在**取锁之前**发布：这样 tick 逻辑跑完（可能几 ms）之后
        // 才来的 paint 也能找到正确的帧；而这里立刻置 PENDING，保证
        // 同一帧不会有两个 paint 各写一次时刻。
        TRACE_PENDING.store(TRACE_SEQ.load(Ordering::Relaxed) - 1, Ordering::Relaxed);
        if let Ok(mut b) = TRACE_BUF.lock() {
            if let Some(i) = pending_idx(&b) {
                b.at_mut(i).t_handler_enter = t;
            }
        }
    }
}

/// 某个阶段结束的通用打点。
///
/// 抽成一个函数而不是写三遍，是为了让"锁内只有一次写"这条纪律只有
/// 一处实现可审。
#[inline(always)]
fn trace_mark(field: u8) {
    if TRACE_ON.load(Ordering::Relaxed) {
        let t = qpc();
        if let Ok(mut b) = TRACE_BUF.lock() {
            if let Some(i) = pending_idx(&b) {
                let r = b.at_mut(i);
                match field {
                    0 => r.t_tick_end = t,
                    1 => r.t_sync_end = t,
                    _ => r.t_frame_done = t,
                }
            }
        }
    }
}

/// `tick_anim()` 返回后。
#[inline(always)]
pub fn trace_tick_end() {
    trace_mark(0);
}

/// `sync_anim_timer()` 返回后。
#[inline(always)]
pub fn trace_sync_end() {
    trace_mark(1);
}

/// `frame_done()` 之后。判 (d)：顺带记下递减后的 in-flight，
/// 若它没降到 0，说明递减本身失真。
#[inline(always)]
pub fn trace_frame_done(inflight_after_done: u32) {
    if TRACE_ON.load(Ordering::Relaxed) {
        let t = qpc();
        if let Ok(mut b) = TRACE_BUF.lock() {
            if let Some(i) = pending_idx(&b) {
                let r = b.at_mut(i);
                r.t_frame_done = t;
                r.inflight_after_done = inflight_after_done;
            }
        }
    }
}

/// **主线程**：进入 `paint::paint` 之前。
///
/// # 第74轮：认领失败不再丢数据，而是**给它自己建一条记录**
///
/// 以前这里是 `if claimed == NO_SEQ { 孤儿计数++; return; }` —— 那次
/// `return` 就是本轮修掉的缺陷：1260 次 paint 一行没落盘，所有静态重绘
/// （悬停 / resize / 切目录 / 子 EDIT）的成本全部丢失，而那恰恰是
/// "低配机器卡不卡"最需要的量（静态帧不需要 60fps，要的是**单次重绘
/// 的绝对成本**）。
///
/// 现在两条路径：
///
/// * 认领到动画 seq → 写进 `TRACE_BUF` 里那一帧（行为与旧版完全一致，
///   动画帧的时间线一个字节没变）；
/// * 认领不到（`NO_SEQ`）→ **当场在 `TRACE_STATIC_BUF` 里 `push` 一条
///   新记录**，`seq = STATIC_SEQ_BASE + n`，把它自己的 seq 发布成
///   `TRACE_CLAIMED_SEQ`，并把 `TRACE_PAINT_OWNED` 置真。
///
/// 于是 `paint()` 里那 15 个 `sub_*` 打点**对任何 paint 都生效**，
/// 与"是不是动画帧"彻底无关。
#[inline(always)]
pub fn trace_paint_enter() {
    if TRACE_ON.load(Ordering::Relaxed) {
        let t = qpc();
        let claimed = TRACE_PENDING.swap(NO_SEQ, Ordering::Relaxed);
        TRACE_PAINTS_RECORDED.fetch_add(1, Ordering::Relaxed);
        if claimed == NO_SEQ {
            // 静态 paint：自己建一条记录。
            //
            // `orphan_paints` 的**新语义**：以前是"认领失败、被丢掉的
            // paint 数"（丢数据）；现在是"**非动画** paint 数"，也就是
            // CSV 里 `kind=paint` 的行数。计数值与旧版同源（都是"没认领
            // 上动画帧的次数"），但从"丢失计数"变成"静态帧计数" ——
            // 它现在**恰好等于**静态记录条数，可以直接用来校验 CSV。
            TRACE_ORPHAN_PAINTS.fetch_add(1, Ordering::Relaxed);
            let seq = STATIC_SEQ_BASE + TRACE_STATIC_SEQ.fetch_add(1, Ordering::Relaxed);
            let mut r = FrameRec {
                seq,
                kind: KIND_PAINT,
                paint_seq: seq,
                ..FrameRec::default()
            };
            r.t_paint_enter = t;
            // 子步骤整段清零：不清的话"这一步这一帧没跑"会读到槽位上
            // 一次被覆盖掉的记录的残留时刻，算出乱七八糟的耗时。
            r.sub_begin = [0; SUB_N];
            r.sub_end = [0; SUB_N];
            if let Ok(mut b) = TRACE_STATIC_BUF.lock() {
                b.push(r);
            } else {
                // 锁被 poison（只可能 panic 传播进来）：退回"无记录"，
                // 记一次未认领，绝不 panic —— 埋点不能成为崩溃源。
                TRACE_SUB_UNCLAIMED.fetch_add(1, Ordering::Relaxed);
                TRACE_PAINT_OWNED.store(false, Ordering::Relaxed);
                TRACE_CLAIMED_SEQ.store(NO_SEQ, Ordering::Relaxed);
                TRACE_ACTIVE_PAINT.store(ACTIVE_NONE, Ordering::Relaxed);
                return;
            }
            TRACE_CLAIMED_SEQ.store(seq, Ordering::Relaxed);
            TRACE_PAINT_OWNED.store(true, Ordering::Relaxed);
            TRACE_ACTIVE_PAINT.store(ACTIVE_PAINT, Ordering::Relaxed);
            return;
        }
        // 动画帧：行为与旧版一字不差。
        TRACE_CLAIMED_SEQ.store(claimed, Ordering::Relaxed);
        TRACE_PAINT_OWNED.store(true, Ordering::Relaxed);
        TRACE_ACTIVE_PAINT.store(ACTIVE_ANIM, Ordering::Relaxed);
        if let Ok(mut b) = TRACE_BUF.lock() {
            if let Some(i) = b.find(claimed) {
                let r = b.at_mut(i);
                r.paint_seq = claimed;
                r.t_paint_enter = t;
                r.t_paint_exit = 0;
                r.invalidated_search = 0;
                r.invalidated_address = 0;
                r.sub_begin = [0; SUB_N];
                r.sub_end = [0; SUB_N];
            }
        }
    }
}

/// **主线程**：`paint::paint` 返回后。
#[inline(always)]
pub fn trace_paint_exit() {
    if TRACE_ON.load(Ordering::Relaxed) {
        let t = qpc();
        // 按"这次 paint 认领/新建的是哪条记录"定位，而不是像旧版那样
        // 倒扫"最近一条未结束的 paint" —— 静态 paint 现在也各自一条
        // 记录，倒扫在两种记录交错时会认错行。
        let seq = TRACE_CLAIMED_SEQ.load(Ordering::Relaxed);
        match TRACE_ACTIVE_PAINT.load(Ordering::Relaxed) {
            ACTIVE_PAINT => {
                if let Ok(mut b) = TRACE_STATIC_BUF.lock() {
                    if let Some(i) = b.find(seq) {
                        b.at_mut(i).t_paint_exit = t;
                    }
                }
            }
            ACTIVE_ANIM => {
                if let Ok(mut b) = TRACE_BUF.lock() {
                    if let Some(i) = b.find(seq) {
                        b.at_mut(i).t_paint_exit = t;
                    }
                }
            }
            _ => {}
        }
        TRACE_PAINT_OWNED.store(false, Ordering::Relaxed);
        TRACE_CLAIMED_SEQ.store(NO_SEQ, Ordering::Relaxed);
        TRACE_ACTIVE_PAINT.store(ACTIVE_NONE, Ordering::Relaxed);
    }
}

/// 找到"最近一个已开始但未结束 paint"的帧下标。
///
/// 必须在持锁后调用。倒着扫，最坏情况（脏数据）也只扫到缓冲末尾。
#[inline(always)]
#[allow(dead_code)]
fn newest_unfinished_paint(b: &TraceBuf) -> Option<usize> {
    let mut i = b.len;
    while i > 0 {
        i -= 1;
        let r = b.at(i);
        if r.t_paint_enter != 0 && r.t_paint_exit == 0 {
            return Some(i);
        }
    }
    None
}

/// 记录"paint 里那句无条件 `InvalidateRect(hwnd_search)` 执行了"。
///
/// 本轮**只是记录**（`paint.rs` 里那两句无条件失效一个字都没改）。
/// 用途：若 `paint` 耗时正常但帧间隔长尾很大，那"每帧都在无条件擦除
/// 两个子 EDIT"就退到嫌疑列表 —— 它们各自会再触发一次子窗口
/// `WM_PAINT`，也就是每帧其实有 3 次重绘而不是 1 次（看
/// `TRACE_ORPHAN_PAINTS` 与帧数的比值）。
#[inline(always)]
pub fn trace_invalidate(kind: u8) {
    if TRACE_ON.load(Ordering::Relaxed) {
        let seq = TRACE_CLAIMED_SEQ.load(Ordering::Relaxed);
        let which = TRACE_ACTIVE_PAINT.load(Ordering::Relaxed);
        if which == ACTIVE_PAINT {
            if let Ok(mut b) = TRACE_STATIC_BUF.lock() {
                if let Some(i) = b.find(seq) {
                    if kind == 0 {
                        b.at_mut(i).invalidated_search = 1;
                    } else {
                        b.at_mut(i).invalidated_address = 1;
                    }
                }
            }
        } else if which == ACTIVE_ANIM {
            if let Ok(mut b) = TRACE_BUF.lock() {
                if let Some(i) = b.find(seq) {
                    if kind == 0 {
                        b.at_mut(i).invalidated_search = 1;
                    } else {
                        b.at_mut(i).invalidated_address = 1;
                    }
                }
            }
        }
    }
}

// --------------------------------------------------------------------
// 第70轮：paint 内部逐段打点
// --------------------------------------------------------------------

/// **主线程**：`paint()` 内某一步的开始。
///
/// 与 `trace_mark` 同样的纪律 —— QPC 在**取锁之前**取，锁内只有两次写。
///
/// # 第74轮：子步骤打点改成**帧无关**
///
/// 旧版这里判 `TRACE_PAINT_OWNED`，false 就直接返回并累加
/// `TRACE_SUB_UNCLAIMED`。那是本轮修掉的缺陷的一半：`paint()` 每次调用
/// 都会走完 15 个打点，但非动画 paint 的 `TRACE_PAINT_OWNED` 是 false，
/// 于是 1260 次 paint × 15 步**全部只累加计数器、一条记录都不写**。
///
/// 现在 `trace_paint_enter` 保证**任何**一次 `paint()` 都有对应记录
/// （动画帧认领、非动画现场新建），所以这里只判"当前有没有正在进行的
/// paint"，然后按 `TRACE_ACTIVE_PAINT` 去对应的环写。
#[inline(always)]
pub fn trace_sub_begin(step: usize) {
    if TRACE_ON.load(Ordering::Relaxed) {
        let t = qpc();
        TRACE_SUB_STEPS_TOTAL.fetch_add(1, Ordering::Relaxed);
        if !TRACE_PAINT_OWNED.load(Ordering::Relaxed) {
            // 理论上到不了这里（`paint()` 必被 `trace_paint_enter` 包住）。
            // 真到了就是有别的路径直接调了 `paint()`，记一次留证据。
            TRACE_SUB_UNCLAIMED.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let seq = TRACE_CLAIMED_SEQ.load(Ordering::Relaxed);
        match TRACE_ACTIVE_PAINT.load(Ordering::Relaxed) {
            ACTIVE_PAINT => {
                if let Ok(mut b) = TRACE_STATIC_BUF.lock() {
                    if let Some(i) = b.find(seq) {
                        b.at_mut(i).sub_begin[step] = t;
                        TRACE_SUB_STEPS_WRITTEN.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            ACTIVE_ANIM => {
                if let Ok(mut b) = TRACE_BUF.lock() {
                    if let Some(i) = b.find(seq) {
                        b.at_mut(i).sub_begin[step] = t;
                        TRACE_SUB_STEPS_WRITTEN.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            _ => {}
        }
    }
}

/// **主线程**：`paint()` 内某一步的结束。
///
/// 锁内只有一次写。注意 `sub_end[i] == 0` 时分析脚本要当作"这一步没跑"，
/// 但**开始时刻为 0** 才是真正的"没跑"信号 —— 因为 `qpc()` 理论上可能
/// 返回 0（只有 QPC 读失败时），实际上把两者都当"没跑"处理即可。
#[inline(always)]
pub fn trace_sub_end(step: usize) {
    if TRACE_ON.load(Ordering::Relaxed) {
        let t = qpc();
        TRACE_SUB_STEPS_TOTAL.fetch_add(1, Ordering::Relaxed);
        if !TRACE_PAINT_OWNED.load(Ordering::Relaxed) {
            TRACE_SUB_UNCLAIMED.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let seq = TRACE_CLAIMED_SEQ.load(Ordering::Relaxed);
        match TRACE_ACTIVE_PAINT.load(Ordering::Relaxed) {
            ACTIVE_PAINT => {
                if let Ok(mut b) = TRACE_STATIC_BUF.lock() {
                    if let Some(i) = b.find(seq) {
                        b.at_mut(i).sub_end[step] = t;
                        TRACE_SUB_STEPS_WRITTEN.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            ACTIVE_ANIM => {
                if let Ok(mut b) = TRACE_BUF.lock() {
                    if let Some(i) = b.find(seq) {
                        b.at_mut(i).sub_end[step] = t;
                        TRACE_SUB_STEPS_WRITTEN.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            _ => {}
        }
    }
}

/// **主线程**：采一次判据快照（`layout` 之后打一次即可 —— 布局里的
/// `l.view` 和 `app` 的视图状态在这一帧内不会再变）。
///
/// 取时刻与写字段在同一次调用里，但**不在锁内取 QPC**（锁内只有字段写）。
///
/// 逐项说明为什么要记它（不是"能记就记"，是每一项都有具体的
/// 双峰解释能力）：
///
/// * `entries` / `visible` —— 列表越长要画的行越多。Details 视图每行
///   一次文本 + 可能一次图标；行数差 10 倍就应该把分布**变宽**，
///   而不是挖出空洞。所以它是"排除项"，不是"解释项"。
/// * `view_mode` —— 大图标视图要画图标（Shell 取图标 + AlphaBlend），
///   单帧成本可能差一个量级。**这是最有区分度的一项。**
/// * `view_area` —— `BitBlt` 与 `FillRect` 的规模。
/// * `icon_zoom` / `icon_bounce` / `cap_anim` —— 正在跑的动画数量。
///   动画跑得多不代表画得多，但每帧要逐个查表算缓动。
/// * `addr_anim` / `sb_scroll_target` / `sb_active_y != target` ——
///   这些是"**切目录**"这一类交互特有的标志。上一轮测到**点击侧栏条目换目录**
///   是最有效的驱动方式，而这类帧恰好也是最慢的那一簇 —— 如果慢帧与
///   这些位强相关，就说明慢帧不是"画的东西多"，而是"**多画了一层东西**"
///   （地址栏滑入 + 侧栏指示条 + 滚动，每一项都可能在同一个位置反复
///   AlphaBlend / 画文本）。
/// * `anim_active()` —— 兜底判据：慢帧是不是"确实有缓动在跑"的帧。
#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub fn trace_ctx(
    width: i32,
    height: i32,
    view: RECT,
    panes: usize,
    entries: usize,
    visible: usize,
    view_mode: u32,
    icon_zoom: usize,
    icon_bounce: usize,
    cap_anim: usize,
    flags: u32,
    anim_active: bool,
) {
    if TRACE_ON.load(Ordering::Relaxed) && TRACE_PAINT_OWNED.load(Ordering::Relaxed) {
        let area = ((view.right - view.left).max(0) as i64) * ((view.bottom - view.top).max(0) as i64);
        let seq = TRACE_CLAIMED_SEQ.load(Ordering::Relaxed);
        // 判据快照在两个环里写法完全一样，抽成一次写。
        macro_rules! put_ctx {
            ($b:expr) => {{
                if let Some(i) = $b.find(seq) {
                    let r = $b.at_mut(i);
                    r.d_w = width.max(0) as u32;
                    r.d_h = height.max(0) as u32;
                    r.d_panes = panes as u32;
                    r.d_entries = entries as u32;
                    r.d_visible = visible as u32;
                    r.d_viewmode = view_mode;
                    r.d_view_area = area.max(0) as u64;
                    r.d_icon_zoom = icon_zoom as u32;
                    r.d_icon_bounce = icon_bounce as u32;
                    r.d_cap_anim = cap_anim as u32;
                    r.d_flags = flags;
                    r.d_anim_active = anim_active as u32;
                }
            }};
        }
        match TRACE_ACTIVE_PAINT.load(Ordering::Relaxed) {
            ACTIVE_PAINT => {
                if let Ok(mut b) = TRACE_STATIC_BUF.lock() {
                    put_ctx!(b);
                }
            }
            ACTIVE_ANIM => {
                if let Ok(mut b) = TRACE_BUF.lock() {
                    put_ctx!(b);
                }
            }
            _ => {}
        }
    }
}

// --------------------------------------------------------------------
// 第70轮：子窗口（两个子 EDIT）每次重绘的代价
// --------------------------------------------------------------------
//
// 分类方式：**由各自的窗口过程写死类别**，不做运行时反查。
// `addr_proc` / `search_proc` / `rename_proc` 各自只可能被对应的那个
// hwnd 调用，所以在那里写 `CHILD_ADDR` / `CHILD_SEARCH` / `CHILD_OTHER`
// 是**恒等映射**，比 `GetDlgCtrlID` 反查更省、更不会错。
// （原先那版按控件 ID 反查的 `child_kind` 已删 —— 它在录制期间会白付
// 一次系统调用，而结果与写死完全相同。）
//
// 另一个关键性质：子 EDIT 的 `WM_PAINT` **归不上任何动画帧**，所以它
// 只累加这里的分类计数，**一个 `FrameRec` 都不碰** —— 既量到了它的
// 代价，又完全不干扰外层时间线。

/// **主线程**：某个窗口的 `WM_PAINT` 开始。
#[inline(always)]
pub fn trace_child_paint_enter(kind: usize) {
    if TRACE_ON.load(Ordering::Relaxed) {
        let t = qpc();
        CHILD_PAINT_CNT[kind].fetch_add(1, Ordering::Relaxed);
        CHILD_ENTER[kind].store(t, Ordering::Relaxed);
    }
}

/// **主线程**：某个窗口的 `WM_PAINT` 结束。
#[inline(always)]
pub fn trace_child_paint_exit(kind: usize) {
    if TRACE_ON.load(Ordering::Relaxed) {
        let t = qpc();
        let e = CHILD_ENTER[kind].load(Ordering::Relaxed);
        if e == 0 || t <= e {
            return;
        }
        let d = t - e;
        CHILD_PAINT_NS[kind].fetch_add(d, Ordering::Relaxed);
        // max 用 CAS 循环；这是全进程唯一一处"锁外原子自旋"，只跑在
        // 录制的几秒里，且原子操作不涉及锁，不会与后台时钟线程互等。
        let mut cur = CHILD_PAINT_MAX[kind].load(Ordering::Relaxed);
        while d > cur {
            match CHILD_PAINT_MAX[kind].compare_exchange_weak(
                cur,
                d,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(c) => cur = c,
            }
        }
        if let Ok(mut v) = CHILD_SAMPLES.lock() {
            let b = &mut v[kind];
            if b.len() < CHILD_SAMPLE_CAP {
                b.push(d);
            } else {
                b.clear();
                b.push(d);
            }
        }
    }
}

/// **主线程**：某个窗口的 `WM_ERASEBKGND`。
///
/// `InvalidateRect(hwnd, None, true)` 的 `erase=true` 让 Windows 先发一次
/// `WM_ERASEBKGND` 再发 `WM_PAINT` —— 所以这个计数就是"每帧额外几次擦除"。
#[inline(always)]
pub fn trace_child_erase(kind: usize) {
    if TRACE_ON.load(Ordering::Relaxed) {
        CHILD_ERASE_CNT[kind].fetch_add(1, Ordering::Relaxed);
    }
}

// --------------------------------------------------------------------
// dump
// --------------------------------------------------------------------

/// 把缓冲写成 CSV。返回写出的帧数。
///
/// **必须在主线程调**（调用点就在消息循环这一侧），并且**先把开关
/// 置 false 再读缓冲** —— 否则后台线程可能正写到一半，dump 出来的
/// 最后一帧是半条记录（`t_handler_enter` 有、`t_post` 空），统计里
/// 就会出现物理上不可能的"负间隔"。
///
/// 顺序：关开关 → 取锁 → 整个缓冲 move 出来（锁内只有一次 move）
/// → **放锁** → 再在锁外做格式化与文件 I/O。写文件要几十毫秒，
/// 绝不能占着锁（后台线程会一直等）。
pub fn dump_trace(path: &str) -> std::io::Result<usize> {
    TRACE_ON.store(false, Ordering::Relaxed);
    TRACE_PENDING.store(NO_SEQ, Ordering::Relaxed);
    TRACE_PAINT_OWNED.store(false, Ordering::Relaxed);
    TRACE_CLAIMED_SEQ.store(NO_SEQ, Ordering::Relaxed);
    TRACE_ACTIVE_PAINT.store(ACTIVE_NONE, Ordering::Relaxed);
    let orphans = TRACE_ORPHAN_PAINTS.load(Ordering::Relaxed);
    let unclaimed = TRACE_SUB_UNCLAIMED.load(Ordering::Relaxed);
    let paints_recorded = TRACE_PAINTS_RECORDED.load(Ordering::Relaxed);
    let sub_steps_total = TRACE_SUB_STEPS_TOTAL.load(Ordering::Relaxed);
    let sub_steps_written = TRACE_SUB_STEPS_WRITTEN.load(Ordering::Relaxed);
    let selftest_ok = trace_selftest_ok();

    // ---- 锁内只有两次 move：两个环各整体搬出 ----
    //
    // 第74轮：动画环与静态环是**两个独立缓冲**，各自 `take_all` 一次。
    // 之后的归并、格式化、文件 I/O 全在**锁外** —— 锁纪律与旧版一致。
    let (recs, statics, anim_dropped, static_dropped) = {
        let mut b = TRACE_BUF
            .lock()
            .map_err(|_| std::io::Error::other("trace buffer poisoned"))?;
        let anim_dropped = b.dropped();
        let r = b.take_all();
        drop(b);
        let mut sb = TRACE_STATIC_BUF
            .lock()
            .map_err(|_| std::io::Error::other("trace static buffer poisoned"))?;
        let sd = sb.dropped();
        let s = sb.take_all();
        (r, s, anim_dropped, sd)
    };

    // 归并成**一条**时间轴：一次 paint = 一行，两类混排。
    //
    // 动画帧里 `t_paint_enter == 0` 的（post 了但这一拍没触发重绘）
    // 排在最前，保持"动画时间线在前"的旧观感。
    let mut all: Vec<FrameRec> = Vec::with_capacity(recs.len() + statics.len());
    all.extend(recs.iter().filter(|r| r.t_paint_enter == 0).copied());
    let mut painted: Vec<&FrameRec> = recs.iter().filter(|r| r.t_paint_enter != 0).collect();
    let mut sp: Vec<&FrameRec> = statics.iter().filter(|r| r.t_paint_enter != 0).collect();
    painted.sort_unstable_by_key(|r| r.t_paint_enter);
    sp.sort_unstable_by_key(|r| r.t_paint_enter);
    all.extend(painted.into_iter().copied());
    all.extend(sp.into_iter().copied());
    drop(recs);
    drop(statics);

    let n_anim = all.iter().filter(|r| r.kind == KIND_ANIM).count();
    let n_paint = all.iter().filter(|r| r.kind == KIND_PAINT).count();
    let hz = qpc_hz();

    // 第70轮：CSV **只追加**子函数与判据列，老列顺序一个字节没动 ——
    // 上一轮的分析脚本按列名/列序取值，追加不影响它继续用。
    let mut s = String::with_capacity(all.len() * 340 + 1024);
    // 老 14 列，**末尾不带换行** —— 第70轮的新列要接在同一行表头后面。
    // （这里若留 `\n`，追加列就会跑到第二行，解析脚本按列名取值全落空。）
    s.push_str(
        "seq,t_post,t_handler_enter,t_tick_end,t_sync_end,t_frame_done,\
         t_paint_enter,t_paint_exit,inflight_before,inflight_after,\
         inflight_after_done,paint_seq,inv_search,inv_address",
    );
    // ---- 第70轮追加列 ----
    use std::fmt::Write as _;
    let mut hdr = String::new();
    for name in SUB_NAMES.iter() {
        let _ = write!(hdr, ",{name}_b,{name}_e");
    }
    hdr.push_str(
        ",d_w,d_h,d_panes,d_entries,d_visible,d_viewmode,d_view_area,\
         d_icon_zoom,d_icon_bounce,d_cap_anim,d_flags,d_anim_active",
    );
    // ---- 第74轮追加列：`kind`，**追加在最后**（不插中间） ----
    //
    // 老分析脚本（`%TEMP%\qa73\an73.py`）按**列名**取值（`r['t_paint_enter']`），
    // 末尾追加对它完全透明；插在中间则会破坏任何按列序取值的脚本。
    hdr.push_str(",kind");
    s.push_str(&hdr);
    s.push('\n');

    for r in &all {
        // 老 14 列 + 第70轮追加列，**拼成一行**再换行 —— 分两次写会让
        // 追加列掉到下一行，行列错位。
        let _ = write!(
            s,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            r.seq,
            r.t_post,
            r.t_handler_enter,
            r.t_tick_end,
            r.t_sync_end,
            r.t_frame_done,
            r.t_paint_enter,
            r.t_paint_exit,
            r.inflight_before,
            r.inflight_after,
            r.inflight_after_done,
            r.paint_seq,
            r.invalidated_search,
            r.invalidated_address
        );
        for i in 0..SUB_N {
            let _ = write!(s, ",{},{}", r.sub_begin[i], r.sub_end[i]);
        }
        let _ = write!(
            s,
            ",{},{},{},{},{},{},{},{},{},{},{},{}",
            r.d_w,
            r.d_h,
            r.d_panes,
            r.d_entries,
            r.d_visible,
            r.d_viewmode,
            r.d_view_area,
            r.d_icon_zoom,
            r.d_icon_bounce,
            r.d_cap_anim,
            r.d_flags,
            r.d_anim_active
        );
        // ---- 第74轮追加列：kind 文本（`anim` / `paint`） ----
        let _ = write!(s, ",{}", if r.kind == KIND_PAINT { KIND_PAINT_TXT } else { KIND_ANIM_TXT });
        s.push('\n');
    }
    std::fs::write(path, s.as_bytes())?;

    // ---- 子窗口统计：另写一份 `.child`，与主 CSV 并排 ----
    //
    // 为什么不塞进主 CSV：子窗口的 `WM_PAINT` 次数与耗时是**聚合计数**，
    // 不是逐帧序列 —— 塞进去就得给每帧加 4×4 列而其中 99% 是 0。
    // 分位数只有这一份统计才需要，所以单独落一份。
    let samples: Vec<Vec<u64>> = CHILD_SAMPLES
        .lock()
        .map(|v| v.iter().cloned().collect())
        .unwrap_or_else(|_| vec![Vec::new(); CHILD_KINDS]);
    let mut c = String::new();
    c.push_str("kind,paint_cnt,erase_cnt,total_ticks,max_ticks,p50_ticks,p95_ticks,mean_ticks\n");
    for k in 0..CHILD_KINDS {
        let n = CHILD_PAINT_CNT[k].load(Ordering::Relaxed);
        let total = CHILD_PAINT_NS[k].load(Ordering::Relaxed);
        let mx = CHILD_PAINT_MAX[k].load(Ordering::Relaxed);
        let mut v: Vec<u64> = samples.get(k).cloned().unwrap_or_default();
        v.sort_unstable();
        let p50 = v.get(v.len() / 2).copied().unwrap_or(0);
        let p95 = v.get(v.len().saturating_sub(1).min((v.len() * 95) / 100)).copied().unwrap_or(0);
        let mean = if n > 0 { total / n } else { 0 };
        let _ = writeln!(
            c,
            "{},{},{},{},{},{},{},{}",
            CHILD_KIND_NAMES[k], n, CHILD_ERASE_CNT[k].load(Ordering::Relaxed), total, mx, p50, p95, mean
        );
    }
    let _ = std::fs::write(format!("{path}.child"), c.as_bytes());

    // 元信息另写一份：QPC 频率、帧数、孤儿 paint 数，免得脚本猜。
    //
    // 第74轮新增自检字段 —— 这是本轮缺陷（"看起来在采但全丢"）的哨兵：
    //
    // * `paints_recorded` = `trace_paint_enter` 被调用总次数，**必须**
    //   等于 CSV 数据行数（不等就是有记录没落盘，或落盘了没记账）。
    // * `sub_steps_total`  = `sub_*` 打点被调用总次数。
    // * `sub_steps_written`= 其中**真的写进记录**的次数。
    // * `selftest_ok`      = `sub_steps_written >= paints_recorded * 2`。
    //   旧版缺陷下 `sub_steps_written == 0` 而 `paints_recorded == 1260`
    //   ⇒ 断言必然为 0，一眼看出"数据全丢"。
    // * `static_dropped` / `anim_dropped` = 两环各自"丢最旧"丢掉的条数。
    //   没有它就无法区分"只发生了 N 次"与"发生了远多于 N 次但只留 N 条"。
    let meta = format!(
        "qpc_hz={} frames={} frame_ms={} orphan_paints={} trace_env={} \
         sub_steps={} sub_unclaimed={}\n\
         paints_recorded={} sub_steps_total={} sub_steps_written={} selftest_ok={} \
         rows_anim={} rows_paint={} anim_dropped={} static_dropped={} \
         static_cap={} anim_cap={} static_seq_base={}\n",
        hz,
        all.len(),
        FRAME_MS,
        orphans,
        TRACE_ENV,
        SUB_N,
        unclaimed,
        paints_recorded,
        sub_steps_total,
        sub_steps_written,
        if selftest_ok { 1 } else { 0 },
        n_anim,
        n_paint,
        anim_dropped,
        static_dropped,
        TRACE_CAP_STATIC,
        TRACE_CAP_ANIM,
        STATIC_SEQ_BASE
    );
    let _ = std::fs::write(format!("{path}.meta"), meta.as_bytes());
    Ok(all.len())
}
