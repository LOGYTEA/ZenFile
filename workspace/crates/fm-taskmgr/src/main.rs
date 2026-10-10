//! 模块子进程示例：不注册托盘、不注册自启、不显示主窗口。
//!
//! 它唯一的作用是验证 §3 的进程树语义：由 fm-app.exe 用 CreateProcess 拉起后，
//! 任务管理器中应折叠在宿主的唯一顶层条目之下。

// 子进程同样必须是 Windows 子系统：控制台子系统会让它自己再开一个控制台窗口。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use fm_core::ipc::{self, Capability, Envelope};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version") {
        // Windows 子系统下无控制台，这行在 release 里不产生可见输出；debug 构建可看到
        println!("fm-taskmgr 0.1.0");
        return;
    }
    // 握手：声明协议版本与所需能力（宿主的 IPC 白名单据此授权）
    let _hello = Envelope::Hello {
        module: "taskmgr".into(),
        protocol_version: fm_core::PROTOCOL_VERSION,
        capabilities: vec![Capability::TaskSnapshot, Capability::LogWrite],
    };
    let _frame = ipc::encode(&_hello).unwrap();

    // 模块保持存活直到宿主退出或作业被关闭。
    loop {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
}
