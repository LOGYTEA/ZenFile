//! 内核统一错误模型。刻意保持扁平，便于跨 IPC 边界转成错误码。

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// Win32 GetLastError
    Win32(u32),
    /// COM HRESULT
    Com(i32),
    Io(std::io::Error),
    Json(String),
    Config(String),
    /// 操作被用户或代际作废取消
    Canceled,
    /// 目标已存在时的冲突（由上层决定覆盖/跳过/重命名）
    Conflict(String),
    Other(String),
}

impl Error {
    pub fn from_win32() -> Self {
        Error::Win32(unsafe { windows::Win32::Foundation::GetLastError().0 })
    }

    pub fn msg<T: Into<String>>(m: T) -> Self {
        Error::Other(m.into())
    }

    /// 跨 IPC 边界传输用的稳定错误码。
    pub fn code(&self) -> i32 {
        match self {
            Error::Win32(e) => *e as i32,
            Error::Com(h) => *h,
            Error::Io(_) => -1,
            Error::Json(_) => -2,
            Error::Config(_) => -3,
            Error::Canceled => -4,
            Error::Conflict(_) => -5,
            Error::Other(_) => -6,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Win32(e) => write!(f, "Win32 错误 0x{e:08X}"),
            Error::Com(h) => write!(f, "HRESULT 0x{h:08X}"),
            Error::Io(e) => write!(f, "IO 错误: {e}"),
            Error::Json(e) => write!(f, "JSON 错误: {e}"),
            Error::Config(e) => write!(f, "配置错误: {e}"),
            Error::Canceled => write!(f, "操作已取消"),
            Error::Conflict(p) => write!(f, "目标已存在: {p}"),
            Error::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Self {
        Error::Com(e.code().0)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
