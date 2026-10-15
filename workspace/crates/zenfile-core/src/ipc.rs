//! 宿主与模块之间的 IPC 帧协议（技术方案 §10.2）。
//!
//! 传输选型：
//!   - 命名管道：控制、日志、短命令
//!   - 文件句柄 / 共享内存 key：大纹理与临时文件（不复制字节流）
//! 帧格式：4 字节 little-endian 长度前缀 + 载荷（JSON；可平滑替换为 Cap'n Proto）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};

pub const FRAME_HEADER: usize = 4;
/// 单帧载荷上限，防止畸形包撑爆内存。
pub const MAX_FRAME: usize = 8 * 1024 * 1024;

/// 能力白名单：模块不得直接调用任意 Win32，只能申请这些能力。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Capability {
    /// 读取剪贴板历史
    ClipboardHistory,
    /// 触发一次截图
    ScreenshotCapture,
    /// 拉取进程/性能快照
    TaskSnapshot,
    /// 在当前目录开终端会话
    TerminalOpen,
    /// 请求预览某个文件
    PreviewOpen,
    /// 写入一行日志
    LogWrite,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::ClipboardHistory => "clipboard.history",
            Capability::ScreenshotCapture => "screenshot.capture",
            Capability::TaskSnapshot => "taskmgr.snapshot",
            Capability::TerminalOpen => "terminal.open",
            Capability::PreviewOpen => "preview.open",
            Capability::LogWrite => "core.log",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Envelope {
    Request { id: u64, capability: Capability, params: Value },
    Response { id: u64, ok: bool, payload: Value },
    /// 单向事件：宿主推送（如隐藏/显示、配置变更）
    Event { name: String, payload: Value },
    /// 模块握手：声明 protocol_version 与所需能力
    Hello { module: String, protocol_version: u32, capabilities: Vec<Capability> },
}

impl Envelope {
    pub fn request(id: u64, capability: Capability, params: Value) -> Self {
        Envelope::Request { id, capability, params }
    }
}

/// 编码为长度前缀帧。
pub fn encode(envelope: &Envelope) -> Result<Vec<u8>> {
    let body = serde_json::to_vec(envelope)?;
    if body.len() > MAX_FRAME {
        return Err(Error::Other("帧超过上限".into()));
    }
    let mut frame = Vec::with_capacity(FRAME_HEADER + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// 从缓冲区中取出一整帧；返回 None 表示字节还不够。
pub fn decode(buf: &[u8]) -> Result<Option<(Envelope, usize)>> {
    if buf.len() < FRAME_HEADER {
        return Ok(None);
    }
    let mut len_bytes = [0u8; 4];
    len_bytes.copy_from_slice(&buf[..FRAME_HEADER]);
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > MAX_FRAME {
        return Err(Error::Other("帧超过上限".into()));
    }
    if buf.len() < FRAME_HEADER + len {
        return Ok(None);
    }
    let envelope: Envelope = serde_json::from_slice(&buf[FRAME_HEADER..FRAME_HEADER + len])?;
    Ok(Some((envelope, FRAME_HEADER + len)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let e = Envelope::Hello {
            module: "taskmgr".into(),
            protocol_version: crate::PROTOCOL_VERSION,
            capabilities: vec![Capability::TaskSnapshot],
        };
        let bytes = encode(&e).unwrap();
        let (parsed, consumed) = decode(&bytes).unwrap().unwrap();
        assert_eq!(consumed, bytes.len());
        match parsed {
            Envelope::Hello { module, .. } => assert_eq!(module, "taskmgr"),
            _ => panic!("类型不符"),
        }
    }

    #[test]
    fn partial_frame() {
        let bytes = encode(&Envelope::request(1, Capability::LogWrite, Value::Null)).unwrap();
        assert!(decode(&bytes[..bytes.len() - 1]).unwrap().is_none());
    }
}
