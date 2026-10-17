//! 管道 server 端：宿主这一侧。
//!
//! 插件进程侧（`plugin_host.rs` 的 `PipeClient`）是**客户端**，连的是这里
//! `CreateNamedPipe` 建出来的实例。两侧的方向在实现里很容易搞反，这里先钉死：
//!
//! ```text
//! 宿主（server）                        插件（client）
//! CreateNamedPipe  ──命名──▶  CreateFileW(OPEN_EXISTING)
//! ConnectNamedPipe（等待）        连上之后双向可读可写
//! ```
//!
//! # 为什么不把管道名当命令行参数传过去
//!
//! 管道名里有**插件自己的 pid**（`\\.\pipe\ZenFile.<pid>.<id>`），而宿主拿
//! 到 pid 之前管道名还不存在。所以顺序只能是「先 spawn 拿到 pid → 用 pid
//! 拼出管道名 → 建管道 → ConnectNamedPipe 等待」。
//!
//! 竞态在于：spawn 里 `ResumeThread` 之后插件立刻就会 `CreateFileW`，
//! 而管道此刻可能还没建好。**不靠时序，靠插件侧的轮询重试**（`PipeClient::connect`
//! 每 20ms 重试、最多 2 秒）。宿主这一侧不用 sleep 等进程，反而更稳。
//!
//! # 读线程为什么不碰 UI
//!
//! 读线程只做一件事：把管道里的字节攒成整行 JSON，然后把 `last_heartbeat` 的
//! 新值**攒在内存里**。宿主的状态在别的线程，所以这里用 `Arc<AtomicU64>`
//! 交出去，绝不跨线程碰 `App` 里的 `PluginSlot`。
//!
//! 这一点是硬纪律：**worker 绝不碰 UI 状态**（同 `network.rs` 的做法）。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::Value;

/// 一根管道 + 它的后台读线程。
///
/// 宿主侧一个插件一根；插件重启后 pid 变了，管道名跟着变，自然换一根新的，
/// 不会撞上上一条的残留。
pub struct PipeServer {
    name: String,
    /// 管道实例句柄。`start()` 之后有效。
    handle: windows::Win32::Foundation::HANDLE,
    /// 最近一次收到心跳的时刻（`now_ms()` 的值）。读线程写、主循环读。
    last_beat: Arc<AtomicU64>,
    /// 读线程已停止（管道断了 / 主动关闭）。主循环据此认定「插件不再响应」。
    dead: Arc<AtomicBool>,
    /// 收到过 hello。主循环据此把 Starting 推进到 Running。
    greeted: Arc<AtomicBool>,
    /// 读线程句柄，Drop 时 join。
    reader: Option<std::thread::JoinHandle<()>>,
}

impl PipeServer {
    /// 建一根管道并把实例句柄备好。**还没有连接**（不调 ConnectNamedPipe）。
    ///
    /// `CREATE` 失败（重名 / 句柄耗尽）返回 None —— 调用方降级成「这个插件
    /// 没管道」，不该让整个宿主起不来。
    pub fn create(name: &str, now_ms: u64) -> Option<Self> {
        use windows::core::HSTRING;
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
        };
        use windows::Win32::System::Pipes::{CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_WAIT};

        // 4K 缓冲：心跳是一行几十字节，批量读时也不会把多条消息挤在一起，
        // 但足以让「一次写一条」不必拆包。
        const N_IN: u32 = 4096;
        const N_OUT: u32 = 4096;

        // 两个参数的**类型不同**，不能混在一个 `|` 里：
        //   dwOpenMode 是 FILE_FLAGS_AND_ATTRIBUTES（开模式 + 首实例标志）
        //   dwPipeMode 是 NAMED_PIPE_MODE（字节流 + 阻塞语义）
        // FILE_FLAG_FIRST_PIPE_INSTANCE：保证我们是这根管道的唯一实例。
        // 别人（包括同名插件的残留进程）抢不到，插件侧连不上会立刻暴露问题，
        // 而不是两个客户端接到同一根管头上。
        let open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE;
        let pipe_mode = PIPE_TYPE_BYTE | PIPE_WAIT;

        // `CreateNamedPipeW` 返回**裸 HANDLE**（不是 Result）——
        // 失败时是 INVALID_HANDLE_VALUE。所以判 `is_invalid()`，别去 match Err。
        let h = unsafe {
            CreateNamedPipeW(
                &HSTRING::from(name),
                open_mode,
                pipe_mode,
                1,      // max instances = 1：一根管道只服务一个插件
                N_OUT,
                N_IN,
                0,      // 默认超时
                None,
            )
        };
        if h.is_invalid() {
            return None;
        }
        Some(Self {
            name: name.to_string(),
            handle: h,
            last_beat: Arc::new(AtomicU64::new(now_ms)),
            dead: Arc::new(AtomicBool::new(false)),
            greeted: Arc::new(AtomicBool::new(false)),
            reader: None,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// 收读线程 + 等插件连上来。返回 false = 管道建好了但线程起不来（极少）。
    pub fn start(&mut self) -> bool {
        if self.reader.is_some() {
            return true;
        }
        let handle = SendHandle(self.handle);
        let last_beat = self.last_beat.clone();
        let dead = self.dead.clone();
        let greeted = self.greeted.clone();
        self.reader = Some(std::thread::spawn(move || {
            // 时钟用 zenfile_core::now_ms（GetTickCount64，单调）。
            // 绝不用 SystemTime::now：NTP 校时会让它往回跳，心跳判定会错。
            read_loop(handle, last_beat, dead, greeted, zenfile_core::now_ms);
        }));
        true
    }

    /// 最近一次心跳时刻。
    pub fn last_heartbeat(&self) -> u64 {
        self.last_beat.load(Ordering::Relaxed)
    }

    /// 是否收到过 hello（= 握手完成）。
    pub fn greeted(&self) -> bool {
        self.greeted.load(Ordering::Relaxed)
    }

    /// 读线程是否已结束（管道断了）。
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }

    /// 往管道写一行 JSON（宿主 → 插件）。用于 `shutdown` / `call`。
    pub fn send(&self, v: &Value) -> bool {
        write_all(self.handle, &line_of(v))
    }
}

impl Drop for PipeServer {
    fn drop(&mut self) {
        use windows::Win32::System::Pipes::DisconnectNamedPipe;
        // 先置 dead 再断开：不置的话读线程会以为宿主还活着。
        self.dead.store(true, Ordering::Relaxed);
        // DisconnectNamedPipe 让客户端立刻看到「管道没了」（插件侧会收到
        // 读失败而自行退出）。**不用 CancelSynchronousIo**：它要的是线程
        // 句柄而不是管道句柄，传错会静默失败；而读线程本来就是 50ms 轮询，
        // 断开后它自然醒来看到 dead 就退出，join 得回来。
        unsafe {
            let _ = DisconnectNamedPipe(self.handle);
        }
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

/// 管道句柄的 `Send` 包装。
///
/// `HANDLE` 是 `*mut c_void`，**默认 !Send**，所以不能直接 `move` 进
/// `thread::spawn`。这根管道由 `PipeServer` 独占拥有、句柄生命周期由它管
/// （Drop 里 join + Close），跨线程共享是安全的 —— 但编译器不知道，
/// 所以这里显式声明一次。
struct SendHandle(windows::Win32::Foundation::HANDLE);

// SAFETY：句柄由 PipeServer 独占拥有，Drop 时先置 dead 再 Disconnect 再
// CloseHandle，读线程在 CloseHandle 之前一定已经 join 完毕。
unsafe impl Send for SendHandle {}

/// 读线程主循环：Peek → Read → 按 `\n` 切行 → parse。
///
/// 心跳的处理只有一件事：把 `last_beat` 更新为现在。
/// hello 把 `greeted` 置起来（表示这个进程确实是我们要找的那个）。
/// 其余消息类型忽略——`call` 的响应能力留给后续演进。
fn read_loop(
    handle: SendHandle,
    last_beat: Arc<AtomicU64>,
    dead: Arc<AtomicBool>,
    greeted: Arc<AtomicBool>,
    now_ms: fn() -> u64,
) {
    let handle = handle.0;
    let mut pending: Vec<u8> = Vec::new();

    // **必须 ConnectNamedPipe，否则数据永远不流动。**
    //
    // `CreateNamedPipeW` 只是造出一个"待连接"的实例；客户端 `CreateFileW`
    // 挂上来的那一刻连接还在排队状态，要 `ConnectNamedPipe` 才真正接通。
    // 漏掉这一步的现象极具欺骗性：管道看起来存在、插件进程也活着、
    // 但宿主一个字节都读不到 —— 心跳永远停在初始值，最后 5 秒后被判卡死。
    //
    // 客户端可能在 ConnectNamedPipe 之前就连了（两边并发），此时本调用
    // 返回 ERROR_PIPE_CONNECTED —— 那是**成功**，不是错误。
    {
        use windows::Win32::System::Pipes::ConnectNamedPipe;
        let r = unsafe { ConnectNamedPipe(handle, None) };
        if let Err(e) = r {
            let code = e.code().0 as u32;
            // ERROR_PIPE_CONNECTED(535) = 客户端抢先连上了，照样往下走。
            if code != 535 {
                dead.store(true, Ordering::Relaxed);
                return;
            }
        }
    }

    loop {
        if dead.load(Ordering::Relaxed) {
            return;
        }
        // 先看有没有能读的字节，有才 ReadFile。ReadFile 本身会**阻塞**，
        // 而这里是后台线程，等一等没关系；但我们想尽快看到「宿主要关管了」，
        // 所以轮询间隔保持短。
        match read_available(handle, &mut pending) {
            Ok(0) => {
                // 没有数据：睡一下再看。50ms 足够灵敏，也够省 CPU。
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
            Ok(_) => {}
            Err(_) => {
                dead.store(true, Ordering::Relaxed);
                return;
            }
        }
        // 读到了字节，处理完整行。
        while let Some(nl) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=nl).collect();
            let line = &line[..line.len() - 1];
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_slice::<Value>(line) else {
                // 单行解析失败就跳过这行，别让它把读线程拖进死循环。
                continue;
            };
            match v.get("t").and_then(|x| x.as_str()) {
                Some("hello") => {
                    greeted.store(true, Ordering::Relaxed);
                    // hello 之后按「活着」记一次时间，避免刚握手就因为
                    // last_beat 是拉起时刻而被判超时。
                    last_beat.store(now_ms(), Ordering::Relaxed);
                }
                Some("beat") => {
                    last_beat.store(now_ms(), Ordering::Relaxed);
                }
                _ => {}
            }
        }
    }
}

/// Peek 再 Read：先问「现在能读多少」再读，绝不用会阻塞的 ReadFile 空读。
fn read_available(handle: windows::Win32::Foundation::HANDLE, out: &mut Vec<u8>) -> std::io::Result<usize> {
    use windows::Win32::Storage::FileSystem::ReadFile;
    use windows::Win32::System::Pipes::PeekNamedPipe;

    let mut avail = 0u32;
    unsafe { PeekNamedPipe(handle, None, 0, None, Some(&mut avail), None) }
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    if avail == 0 {
        return Ok(0);
    }
    let mut buf = [0u8; 4096];
    let want = (avail as usize).min(buf.len());
    let mut read = 0u32;
    unsafe { ReadFile(handle, Some(&mut buf[..want]), Some(&mut read), None) }
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    out.extend_from_slice(&buf[..read as usize]);
    Ok(read as usize)
}

fn write_all(handle: windows::Win32::Foundation::HANDLE, buf: &[u8]) -> bool {
    use windows::Win32::Storage::FileSystem::WriteFile;
    let mut off = 0usize;
    while off < buf.len() {
        let chunk = &buf[off..];
        let mut written = 0u32;
        let written_ok = unsafe { WriteFile(handle, Some(chunk), Some(&mut written), None).is_ok() };
        if !written_ok || written == 0 {
            return false;
        }
        off += written as usize;
    }
    true
}

fn line_of(v: &Value) -> Vec<u8> {
    let mut b = serde_json::to_vec(v).unwrap_or_default();
    b.push(b'\n');
    b
}
