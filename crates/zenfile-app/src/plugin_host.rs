//! 插件进程侧（`ZenFile.exe --plugin <id>`）。
//!
//! # 为什么要单独的进程，而不是插件跑在宿主里
//!
//! 用户明确要求过："不要是 hook 这种插件崩了或者有问题，整个软件直接崩溃报错"。
//! 进程隔离是唯一可靠的答案 —— 进程之间没有共享地址空间，插件可以随便
//! `panic!`、可以越界写内存、可以死循环，宿主都碰不到。
//!
//! 但"每个插件一个 exe"会带来分发 / 升级 / 版本兼容一整套麻烦，所以改成
//! **同一个 exe 的另一个进程**：升级一次到位、版本永远一致，`--plugin <id>`
//! 只是告诉这个进程"你扮演谁"。
//!
//! # 插件进程的自我约束
//!
//! 这个进程里**没有窗口、没有托盘、没有单实例互斥**（`main` 里的
//! `--plugin` 分支在那些逻辑之前就 return 了）。它能做的事只有：
//!
//! 1. 按清单里的能力申请，接管对应功能；
//! 2. 周期心跳，让宿主知道它活着；
//! 3. 干活（干活本身由具体插件实现）。
//!
//! 换句话说，插件进程是一个**没有特权的客人**：它拿不到宿主窗口句柄、
//! 拿不到托盘、拿不到别的插件的管道。

use std::time::Duration;

use serde_json::{json, Value};

/// 插件进程主循环返回码。
pub const EXIT_OK: i32 = 0;
/// 清单 / 参数有问题，这个进程连"我是谁"都没说清。
pub const EXIT_BAD_ARGS: i32 = 2;
/// 管道始终不通（宿主没了），主动退出。
pub const EXIT_NO_HOST: i32 = 3;

/// 跑一个插件进程。
///
/// `args` 是**完整的** argv（含 `argv[0]`），只用来取管道名和插件自己的参数。
pub fn run_plugin_process(id: &str, args: &[String]) -> i32 {
    // 插件 id 要能安全地出现在命名管道名里。管道名不允许 `\ / : * ? " < > |`
    // 与空格；带进来的字符一律换成 `_`，免得 CreateNamedPipe 静默失败。
    let safe_id: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if safe_id.is_empty() {
        return EXIT_BAD_ARGS;
    }

    // 管道名由 pid + id 决定：宿主拉起进程后就知道该连哪一个。
    // 用 pid 是为了让"重启同一个插件"天然换一条管道，不会撞上上一条残留的。
    //
    // **必须调 core 里那同一个函数**：早期这里自己过滤了一遍、宿主侧没过滤，
    // 于是 id 带非安全字符时两侧算出两个名字，插件永远连不上。
    let pipe = zenfile_core::plugin::pipe_name(std::process::id(), id);

    let mut pipe = match PipeClient::connect(&pipe) {
        Some(p) => p,
        None => {
            // 连不上不重试：宿主没起来（或已经退了），插件留着也是孤儿。
            // Job Object 会在宿主退出时带走我们，所以这里直接退是安全的。
            return EXIT_NO_HOST;
        }
    };

    // 握手：声明身份 + 申请能力。宿主校验协议版本后才认这个插件。
    let hello = json!({
        "t": "hello",
        "id": safe_id,
        "protocol": zenfile_core::PROTOCOL_VERSION,
        "argv": args.iter().skip(1).collect::<Vec<_>>(),
    });
    if pipe.send(&hello).is_err() {
        return EXIT_NO_HOST;
    }

    // 主循环：等宿主的话，同时周期心跳。
    //
    // **心跳必须自己主动发，不能等宿主来问** —— 宿主判断"插件还活着"靠的
    // 就是这条。只在收到消息时才发心跳的话，宿主一安静下来就会误判卡死。
    let mut next_beat = std::time::Instant::now();
    loop {
        // 心跳（每 HEARTBEAT_INTERVAL_MS 一次）
        if std::time::Instant::now() >= next_beat {
            if pipe.send(&json!({"t":"beat"})).is_err() {
                // 管道断了 = 宿主没了。Job Object 会收我们，但自己退更干净。
                return EXIT_NO_HOST;
            }
            // 间隔取宿主超时的一半：丢一拍也不会被误判卡死。
            next_beat = std::time::Instant::now()
                + Duration::from_millis(zenfile_core::plugin::HEARTBEAT_TIMEOUT_MS / 2);
        }

        // 等宿主的消息。非阻塞轮询：管道没数据就返回 Ok(None)，立刻回到心跳检查。
        //
        // **绝不能在这里阻塞等完整帧** —— 宿主要是只发了个半截帧就卡住了，
        // 插件跟着一起卡，看起来就变成"插件把宿主拖死了"。
        match pipe.poll(Duration::from_millis(50)) {
            Ok(Some(msg)) => match msg.get("t").and_then(|v| v.as_str()) {
                Some("shutdown") => return EXIT_OK,
                Some("call") => {
                    // 宿主发来一次能力调用。**回什么都行，但必须回** ——
                    // 宿主会等这个响应，不回的话它那边挂着一个等不到的请求。
                    let id = msg.get("id").cloned().unwrap_or(Value::Null);
                    let _ = pipe.send(&json!({"t":"result","id":id,"ok":false,"error":"该插件未实现此能力"}));
                }
                _ => {}
            },
            Ok(None) => {}
            Err(_) => return EXIT_NO_HOST,
        }
    }
}

/// 命名管道客户端。
///
/// 名字叫 `PipeClient` 而不是 `Pipe`，因为它**只是客户端**（连别人建好的
/// 管道），宿主那侧才是 server。
struct PipeClient {
    handle: windows::Win32::Foundation::HANDLE,
    /// 已经从管道读出来、但还没凑够一整行的字节。
    ///
    /// **半截 JSON 必须留到下一次**：管道是字节流，一次 `ReadFile` 拿到的
    /// 很可能只是半条消息。直接 parse 会报 "EOF while parsing"，
    /// 而那不是协议错误，只是正常的分片。
    pending: Vec<u8>,
}

impl PipeClient {
    /// 连一根已有的管道，最多等 2 秒。
    ///
    /// 宿主是**先建管道再拉进程**的，但两边并发，进程起来时管道可能刚好还在
    /// `ConnectNamedPipe` 里 —— 所以必须**轮询重试**而不是连一次就放弃。
    ///
    /// windows-rs 0.62 的几个签名坑（都在这里踩过，记下来免得下次再改）：
    /// - `CreateFileW` 的 `dwDesiredAccess` 是**裸 `u32`**，不是
    ///   `GENERIC_ACCESS_RIGHTS`（尽管那个常量存在）→ `GENERIC_READ.0 | GENERIC_WRITE.0`
    /// - `dwShareMode` 是 `FILE_SHARE_MODE`，要 `|` 起来
    /// - `WriteFile` / `ReadFile` 的 `lpNumberOfBytes*` 要 **`Option<*mut u32>`**，
    ///   传 `&mut u32` 编译不过
    /// - `PeekNamedPipe` 在 **`Win32::System::Pipes`**，不在 `Storage::FileSystem`
    fn connect(name: &str) -> Option<Self> {
        use windows::core::HSTRING;
        use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE};
        use windows::Win32::Storage::FileSystem::{
            CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_FLAGS_AND_ATTRIBUTES,
            OPEN_EXISTING,
        };

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            unsafe {
                match CreateFileW(
                    &HSTRING::from(name),
                    // CreateFileW 要裸 u32；这两个常量的类型是 GENERIC_ACCESS_RIGHTS
                    GENERIC_READ.0 | GENERIC_WRITE.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAGS_AND_ATTRIBUTES(0),
                    None,
                ) {
                    Ok(h) if !h.is_invalid() => {
                        return Some(Self { handle: h, pending: Vec::new() });
                    }
                    Ok(h) => {
                        let _ = CloseHandle(h);
                    }
                    Err(_) => {}
                }
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// 发一条 JSON 消息（自动补换行分隔）。
    fn send(&mut self, v: &Value) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(v).unwrap_or_default();
        line.push(b'\n');
        write_all(self.handle, &line)
    }

    /// 等一条消息（最多 `timeout`）。`Ok(None)` = 超时，没消息可读。
    ///
    /// 读不满一行就**留着等下一次**（`pending` 字段），不会把半截 JSON
    /// 丢在半路上 —— 那是把一次正常的网络分片当成协议错误。
    fn poll(&mut self, timeout: Duration) -> std::io::Result<Option<Value>> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            // 先看缓冲区里有没有攒够一行
            if let Some(nl) = self.pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=nl).collect();
                let line = &line[..line.len() - 1];
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_slice::<Value>(line) {
                    Ok(v) => return Ok(Some(v)),
                    // 解析失败就跳过这行，别让它把插件拖进死循环
                    Err(_) => continue,
                }
            }
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            let mut buf = [0u8; 4096];
            let n = read_available(self.handle, &mut buf)?;
            if n > 0 {
                self.pending.extend_from_slice(&buf[..n]);
            }
        }
    }
}

fn write_all(
    handle: windows::Win32::Foundation::HANDLE,
    buf: &[u8],
) -> std::io::Result<()> {
    use windows::Win32::Storage::FileSystem::WriteFile;
    let mut off = 0usize;
    while off < buf.len() {
        let chunk = &buf[off..];
        let mut written = 0u32;
        unsafe {
            // bytesWritten 要 `Option<*mut u32>`（windows-rs 0.62），不是 `&mut u32`
            WriteFile(handle, Some(chunk), Some(&mut written), None)
        }
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        if written == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "宿主不再接收",
            ));
        }
        off += written as usize;
    }
    Ok(())
}

fn read_available(
    handle: windows::Win32::Foundation::HANDLE,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    // `PeekNamedPipe` 在 `Win32::System::Pipes`（不在 Storage::FileSystem）。
    //
    // 先 Peek 拿"现在能读多少"再 ReadFile：`ReadFile` 本身会**阻塞**，
    // 而插件进程的消息循环绝不能卡在这儿等一个沉默的宿主。
    use windows::Win32::Storage::FileSystem::ReadFile;
    use windows::Win32::System::Pipes::PeekNamedPipe;

    let mut avail = 0u32;
    unsafe { PeekNamedPipe(handle, None, 0, None, Some(&mut avail), None) }
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
    if avail == 0 {
        return Ok(0);
    }
    let want = (avail as usize).min(buf.len());
    let mut read = 0u32;
    unsafe { ReadFile(handle, Some(&mut buf[..want]), Some(&mut read), None) }
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
    Ok(read as usize)
}
