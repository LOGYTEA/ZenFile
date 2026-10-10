//! fm-core —— 与 UI 框架无关的原生资源管理内核。
//!
//! 分层职责：
//!   types     : 与视图无关的领域数据结构（§ Thoughts 借鉴 EdenExplorer 的状态边界）
//!   engine    : 目录枚举 + 代际作废 + 分批提交（技术方案 §5）
//!   watch     : ReadDirectoryChangesW 增量层（仅当前可见目录）
//!   ops       : IFileOperation 默认策略 + 多线程快速模式（技术方案 §6）
//!   clipboard : CF_HDROP 与 Explorer 双向互通（技术方案 §6.4）
//!   config    : 三层配置与迁移（技术方案 §14）
//!   ipc       : 长度前缀帧协议 + 能力白名单（技术方案 §10.2）
//!   process   : 子进程启动器 + Job Object 树（技术方案 §3）
//!   hotkey    : 键位解析（技术方案 §8）
//!
//! 该 crate 不依赖任何 UI 框架，因此 GPUI 迁移时无需改动。

pub mod clipboard;
pub mod config;
pub mod engine;
pub mod error;
pub mod hotkey;
pub mod ipc;
pub mod natural;
pub mod ops;
pub mod paths;
pub mod process;
pub mod selftest;
pub mod types;
pub mod watch;

pub use error::{Error, Result};
pub use types::{
    DirBatch, FileEntry, FileKind, Generation, SortKey, SortSpec, ViewMode, MAX_BATCH,
};

/// IPC 协议版本：宿主与网关内插件协商用（§10.2）。
pub const PROTOCOL_VERSION: u32 = 1;
