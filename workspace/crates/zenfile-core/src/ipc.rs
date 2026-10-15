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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
    /// 能力名（**点号小写**，写进 `plugin.json` 的就是这个）。
    ///
    /// 序列化也用它，而不是默认的 PascalCase —— 两套命名会让插件作者
    /// 照文档写了 `"clipboard.history"` 却报
    /// `unknown variant 'clipboard.history', expected one of 'ClipboardHistory'`
    /// （第66轮实测踩到：示例清单就是这么写错的）。
    pub const ALL: [Capability; 6] = [
        Capability::ClipboardHistory,
        Capability::ScreenshotCapture,
        Capability::TaskSnapshot,
        Capability::TerminalOpen,
        Capability::PreviewOpen,
        Capability::LogWrite,
    ];

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

    /// 按能力名解析。**点号小写与 PascalCase 都认**
    /// （`"clipboard.history"` 与 `"ClipboardHistory"`）。
    ///
    /// 两种都收是有意为之：清单是用户手写的文件，让两种都合法比让某个
    /// 版本的用户清单集体失效要好。
    pub fn from_name(s: &str) -> Option<Self> {
        // 点号小写是正规写法
        if let Some(c) = Self::ALL.into_iter().find(|c| c.as_str() == s) {
            return Some(c);
        }
        // 兼容 PascalCase（serde 默认，早先实现用过）
        Self::ALL.into_iter().find(|c| {
            let pascal = match c {
                Capability::ClipboardHistory => "ClipboardHistory",
                Capability::ScreenshotCapture => "ScreenshotCapture",
                Capability::TaskSnapshot => "TaskSnapshot",
                Capability::TerminalOpen => "TerminalOpen",
                Capability::PreviewOpen => "PreviewOpen",
                Capability::LogWrite => "LogWrite",
            };
            pascal == s
        })
    }
}

/// 序列化成点号小写（`"clipboard.history"`）。
///
/// 反序列化**两种写法都收**：点号小写（文档里写的）与 PascalCase
/// （serde 默认，早先的实现用过的）。都认是有意为之 —— 清单是用户手写的
/// 文件，让两种都合法比让某个版本的用户清单集体失效要好。
impl Serialize for Capability {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Capability {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Capability::from_name(&s).ok_or_else(|| {
            let names: Vec<&str> = Capability::ALL.iter().map(|c| c.as_str()).collect();
            serde::de::Error::custom(format!("未知能力 {s:?}，可用：{}", names.join(" / ")))
        })
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

    #[test]
    fn capability_names_roundtrip_in_plugin_json() {
        // 插件作者照文档写 `"clipboard.history"`，必须能解析（第66轮踩过：
        // serde 默认序列化成 `ClipboardHistory`，与文档里的点号写法对不上，
        // 于是清单被判"损坏"，插件在面板上显示"启动失败"）。
        for c in Capability::ALL {
            let s = c.as_str();
            assert_eq!(Capability::from_name(s), Some(c), "{s} 解析不回来");
            // 序列化成 as_str 的样子
            assert_eq!(serde_json::to_string(&c).unwrap(), format!("\"{s}\""));
            // 反序列化回来
            let back: Capability = serde_json::from_str(&format!("\"{s}\"")).unwrap();
            assert_eq!(back, c);
        }
    }

    #[test]
    fn capability_accepts_pascal_case_too() {
        // 早先的实现用的是 serde 默认的 PascalCase，别让那些清单集体失效。
        let c: Capability = serde_json::from_str("\"ClipboardHistory\"").unwrap();
        assert_eq!(c, Capability::ClipboardHistory);
    }

    #[test]
    fn unknown_capability_error_lists_valid_names() {
        // 报错要说清能用什么，否则用户只看到 unknown variant
        let e = serde_json::from_str::<Capability>("\"no.such.cap\"").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("clipboard.history"), "错误信息没列出可用能力：{msg}");
    }
}
