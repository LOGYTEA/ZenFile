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

pub mod acl;
pub mod bulk;
pub mod clipboard;
pub mod config;
pub mod folder_ini;
pub mod drives;
pub mod engine;
pub mod error;
pub mod hotkey;
pub mod index;
pub mod ipc;
pub mod natural;
pub mod ops;
pub mod paths;
pub mod process;
pub mod recycle;
pub mod selftest;
pub mod types;
pub mod watch;

pub use error::{Error, Result};
pub use types::{
    DirBatch, FileEntry, FileKind, Generation, SortKey, SortSpec, ViewMode, MAX_BATCH,
};

/// IPC 协议版本：宿主与网关内插件协商用（§10.2）。
pub const PROTOCOL_VERSION: u32 = 1;

/// 大小列用什么单位显示。
///
/// 七个档位：自动 / B / KB / MB / GB / TB / PB。
/// `Auto` 的自适应档位：B / KB / MB / GB / TB / PB —— 与资源管理器一致。
/// 注意最底下那一档是 **B**，不是"字节"（用户明确要求不要出现"字节"二字）。
/// 之后每上一级换一个单位。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SizeUnit {
    #[default]
    Auto,
    Bytes,
    KB,
    MB,
    GB,
    TB,
    PB,
}

impl SizeUnit {
    /// 设置页里循环切换的顺序。自动在最前（也是默认）。
    pub const ALL: [SizeUnit; 7] = [
        SizeUnit::Auto,
        SizeUnit::Bytes,
        SizeUnit::KB,
        SizeUnit::MB,
        SizeUnit::GB,
        SizeUnit::TB,
        SizeUnit::PB,
    ];

    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "bytes" | "b" => SizeUnit::Bytes,
            "kb" | "k" => SizeUnit::KB,
            "mb" | "m" => SizeUnit::MB,
            "gb" | "g" => SizeUnit::GB,
            "tb" | "t" => SizeUnit::TB,
            "pb" | "p" => SizeUnit::PB,
            _ => SizeUnit::Auto,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            SizeUnit::Auto => "auto",
            SizeUnit::Bytes => "bytes",
            SizeUnit::KB => "kb",
            SizeUnit::MB => "mb",
            SizeUnit::GB => "gb",
            SizeUnit::TB => "tb",
            SizeUnit::PB => "pb",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            SizeUnit::Auto => "自动（B/KB/MB/GB/TB/PB）",
            SizeUnit::Bytes => "B",
            SizeUnit::KB => "KB",
            SizeUnit::MB => "MB",
            SizeUnit::GB => "GB",
            SizeUnit::TB => "TB",
            SizeUnit::PB => "PB",
        }
    }

    /// 固定单位下的格式化。`Auto` 走自适应，其余一律换算到该单位（不足 1 也显示 0.x）。
    pub fn format(&self, bytes: u64) -> String {
        let v = bytes as f64;
        match self {
            SizeUnit::Bytes => format!("{} B", bytes),
            SizeUnit::KB => format!("{} KB", trim(v / KB_F)),
            SizeUnit::MB => format!("{} MB", trim(v / MB_F)),
            SizeUnit::GB => format!("{} GB", trim(v / GB_F)),
            SizeUnit::TB => format!("{} TB", trim(v / TB_F)),
            SizeUnit::PB => format!("{} PB", trim(v / PB_F)),
            SizeUnit::Auto => {
                // 不足 1 KB 就停在 B 这一档
                if v < KB_F {
                    format!("{} B", bytes)
                } else if v < MB_F {
                    format!("{} KB", trim(v / KB_F))
                } else if v < GB_F {
                    format!("{} MB", trim(v / MB_F))
                } else if v < TB_F {
                    format!("{} GB", trim(v / GB_F))
                } else if v < PB_F {
                    format!("{} TB", trim(v / TB_F))
                } else {
                    format!("{} PB", trim(v / PB_F))
                }
            }
        }
    }
}

pub const KB_F: f64 = 1024.0;
pub const MB_F: f64 = KB_F * 1024.0;
pub const GB_F: f64 = MB_F * 1024.0;
pub const TB_F: f64 = GB_F * 1024.0;
pub const PB_F: f64 = TB_F * 1024.0;

/// 小数位：小于 10 保留 1 位、小于 100 保留 1 位、再大就取整 —— 与资源管理器一致，
/// 避免"1.00 KB"这种多余的零。
fn trim(v: f64) -> String {
    if v < 10.0 {
        let s = format!("{v:.2}");
        let s = s.trim_end_matches('0').trim_end_matches('.');
        if s.is_empty() { "0".to_string() } else { s.to_string() }
    } else if v < 100.0 {
        format!("{v:.1}")
    } else {
        format!("{}", v.round() as u64)
    }
}
