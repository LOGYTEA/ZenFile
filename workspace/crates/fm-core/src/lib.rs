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
/// `Auto` 的规则是"以能表达这个大小的最小单位为准"：不足 1 MB 就一律用 KB
/// （不再出现"512 字节"这种和 KB 混排的写法），不足 1 GB 用 MB，以此类推。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SizeUnit {
    #[default]
    Auto,
    Bytes,
    KB,
    MB,
    GB,
}

impl SizeUnit {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "bytes" | "b" => SizeUnit::Bytes,
            "kb" | "k" => SizeUnit::KB,
            "mb" | "m" => SizeUnit::MB,
            "gb" | "g" => SizeUnit::GB,
            _ => SizeUnit::Auto,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            SizeUnit::Auto => "自动（最小单位）",
            SizeUnit::Bytes => "字节",
            SizeUnit::KB => "KB",
            SizeUnit::MB => "MB",
            SizeUnit::GB => "GB",
        }
    }

    /// 固定单位下的格式化。`Auto` 走自适应，其余一律换算到该单位（不足 1 也显示 0.x）。
    pub fn format(&self, bytes: u64) -> String {
        const KB: f64 = 1024.0;
        const MB: f64 = KB * 1024.0;
        const GB: f64 = MB * 1024.0;
        let v = bytes as f64;
        match self {
            SizeUnit::Bytes => format!("{} 字节", bytes),
            SizeUnit::KB => format!("{} KB", trim(v / KB)),
            SizeUnit::MB => format!("{} MB", trim(v / MB)),
            SizeUnit::GB => format!("{} GB", trim(v / GB)),
            SizeUnit::Auto => {
                if v < MB {
                    format!("{} KB", trim(v / KB))
                } else if v < GB {
                    format!("{} MB", trim(v / MB))
                } else {
                    format!("{} GB", trim(v / GB))
                }
            }
        }
    }
}

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
