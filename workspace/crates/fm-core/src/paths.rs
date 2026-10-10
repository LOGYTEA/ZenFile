//! 路径与目标解析 —— 地址栏语义层（技术方案 §13）。
//!
//! | 输入                  | 结果                     |
//! |-----------------------|--------------------------|
//! | `C:\Users`            | Navigate(PathBuf)        |
//! | `cmd`/`powershell`/`wsl` | ShellCommand(name)    |
//! | `shell:Downloads`     | Navigate(KnownFolder)    |
//! | `%APPDATA%`           | 环境变量展开后导航        |
//! | `\\server\share`      | Navigate(UNC)            |
//! | `..`                  | 上级目录                  |

use std::path::{Path, PathBuf};

use windows::core::HSTRING;
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;

use crate::error::Result;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// 导航到某个目录
    Navigate(PathBuf),
    /// 在当前目录启动 shell（映射到终端标签页）
    ShellCommand(String),
}

/// 地址栏能识别的 shell 别名。
const SHELLS: [&str; 4] = ["cmd", "powershell", "pwsh", "wsl"];

/// 常见 `shell:` 命名空间别名 → 已知文件夹（无法解析时再回落 KNOWNFOLDER）。
const SHELL_ALIAS: [(&str, &str); 8] = [
    ("downloads", "%USERPROFILE%\\Downloads"),
    ("desktop", "%USERPROFILE%\\Desktop"),
    ("documents", "%USERPROFILE%\\Documents"),
    ("pictures", "%USERPROFILE%\\Pictures"),
    ("music", "%USERPROFILE%\\Music"),
    ("videos", "%USERPROFILE%\\Videos"),
    ("desktop" /* alias dup 保护 */, "%USERPROFILE%\\Desktop"),
    ("profile", "%USERPROFILE%"),
];

/// 展开 `%VAR%` 环境变量。
pub fn expand_env(input: &str) -> String {
    if !input.contains('%') {
        return input.to_string();
    }
    unsafe {
        let hs = HSTRING::from(input);
        let mut buf = vec![0u16; 4096];
        let need = ExpandEnvironmentStringsW(&hs, Some(&mut buf));
        if need == 0 {
            return input.to_string();
        }
        let len = (need as usize).saturating_sub(1).min(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }
}

/// 把地址栏输入解析为导航目标。`cwd` 作为相对路径与 shell 的工作目录基准。
pub fn resolve(input: &str, cwd: &Path) -> Result<Target> {
    let raw = input.trim();
    if raw.is_empty() {
        return Ok(Target::Navigate(cwd.to_path_buf()));
    }

    let lower = raw.to_lowercase();
    // 主页：与 Explorer 首页对齐的虚拟目录，不是真实路径
    if lower == "主页" || lower == "home" || lower == "shell:home" {
        return Ok(Target::Navigate(PathBuf::from("shell:Home")));
    }
    if let Some(shell) = SHELLS.iter().find(|s| **s == lower) {
        return Ok(Target::ShellCommand(shell.to_string()));
    }
    if lower == "terminal" {
        return Ok(Target::ShellCommand("cmd".to_string()));
    }

    if let Some(rest) = lower.strip_prefix("shell:") {
        if let Some((_, tmpl)) = SHELL_ALIAS.iter().find(|(k, _)| *k == rest) {
            return Ok(Target::Navigate(PathBuf::from(expand_env(tmpl))));
        }
    }

    let expanded = expand_env(raw);

    // 盘符形式 "C:" → "C:\"
    let path = if expanded.len() == 2 && expanded.as_bytes()[1] == b':' {
        PathBuf::from(format!("{expanded}\\"))
    } else {
        PathBuf::from(expanded)
    };
    let path = if path.is_absolute() { path } else { cwd.join(path) };
    Ok(Target::Navigate(normalize(&path)))
}

/// 归一化：去掉尾部多余分隔符，并把 "." / ".." 段做词法折叠。
pub fn normalize(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy().replace('/', "\\");
    let (prefix, rest) = if let Some(unc) = raw.strip_prefix("\\\\") {
        (format!("\\\\\\\\{}", unc.split('\\').next().unwrap_or("")), &raw[2..])
    } else if raw.len() >= 2 && raw.as_bytes()[1] == b':' {
        (raw[..2].to_string(), &raw[2..])
    } else {
        (String::new(), raw.as_str())
    };
    let mut stack: Vec<&str> = Vec::new();
    for seg in rest.split('\\') {
        match seg {
            "" | "." => {}
            ".." => {
                if !stack.is_empty() {
                    stack.pop();
                }
            }
            other => stack.push(other),
        }
    }
    let mut out = prefix;
    for seg in stack {
        out.push('\\');
        out.push_str(seg);
    }
    if out.is_empty() {
        out.push('\\');
    }
    // 盘符根必须带反斜杠（"C:" 与 "C://" 语义不同）
    if out.len() == 2 && out.as_bytes()[1] == b':' {
        out.push('\\');
    }
    PathBuf::from(out)
}

/// 上级目录（含盘符根 → 返回"\此电脑"语义的 None）。
pub fn parent_of(path: &Path) -> Option<PathBuf> {
    path.parent().map(|p| p.to_path_buf()).filter(|p| !p.as_os_str().is_empty())
}

/// 在目录中生成不冲突的名字："新建文件夹" / "新建文件夹 (2)" ...
pub fn unique_name(dir: &Path, base: &str, ext: &str) -> PathBuf {
    let mut candidate = dir.join(format!("{base}{ext}"));
    let mut n = 2;
    while candidate.exists() {
        candidate = dir.join(format!("{base} ({n}){ext}"));
        n += 1;
    }
    candidate
}

/// 分列出 hostname 为字母的盘符列表（用于侧栏"此电脑"）。
/// 逻辑磁盘的容量信息，单位字节。返回 `(总容量, 可用)`。
///
/// 取不到时返回 `None`：光驱无盘、网络位置离线、权限不足都会失败，此时调用方
/// 应当只显示盘符而不画容量条。
pub fn drive_space(root: &str) -> Option<(u64, u64)> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
    let mut available: u64 = 0;
    let mut total: u64 = 0;
    let mut total_free: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            Some(&mut available),
            Some(&mut total),
            Some(&mut total_free),
        )
        .is_ok()
    };
    if ok && total > 0 {
        Some((total, available))
    } else {
        None
    }
}

/// 该路径是否就是某个盘符的根（形如 `C:\`）。
pub fn is_drive_root(p: &Path) -> bool {
    let s = p.to_string_lossy();
    let bytes = s.as_bytes();
    bytes.len() == 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\'
}

/// 去掉 Win32 的 verbatim 前缀（`\\?\`、`\\?\UNC\`）。
///
/// `std::fs::canonicalize` 一定会加这个前缀。它是给 Win32 API 用的**内部约定**：
/// 带前缀的路径不做任何规范化。一旦把它混进我们自己的逻辑（拼子路径、交给
/// 目录枚举、显示给用户），行为就变了 —— 实测带 `\\?\` 前缀去枚举一个目录，
/// 结果永远是空列表（普通路径同样目录能列出 30 项）。
///
/// 所以凡是 canonicalize 的产出，在离开这一点之前必须先过这个函数。
pub fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p.to_path_buf()
    }
}

pub fn logical_drives() -> Vec<String> {
    let mask = unsafe { windows::Win32::Storage::FileSystem::GetLogicalDrives() };
    (0..26u32)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| format!("{}:\\", (b'A' + i as u8) as char))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_command_detected() {
        let cwd = PathBuf::from("C:\\");
        assert_eq!(resolve("cmd", &cwd).unwrap(), Target::ShellCommand("cmd".into()));
        assert_eq!(resolve("CMD", &cwd).unwrap(), Target::ShellCommand("cmd".into()));
    }

    #[test]
    fn parent_up() {
        let cwd = PathBuf::from("C:\\Users");
        assert_eq!(resolve("..", &cwd).unwrap(), Target::Navigate(PathBuf::from("C:\\")));
    }
}
