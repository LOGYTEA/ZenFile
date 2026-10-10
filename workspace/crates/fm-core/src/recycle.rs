//! 回收站：直接解析 `$Recycle.Bin`，不走 Shell 的 IShellFolder。
//!
//! 为什么绕开 Shell：资源管理器打开回收站要对每一项构建属性存储（取"原位置"、
//! "删除日期"、"大小"三列），几千项时肉眼可见地卡。Windows 的回收站其实只是
//! 每个固定磁盘根目录下的一个 `$Recycle.Bin\<用户SID>` 文件夹，里面成对存放：
//!   - `$I......`：信息头（版本 / 大小 / 删除时间 / 原始路径）
//!   - `$R......`：被删除文件的实体（名字只差首字母）
//! 直接读这些文件：一次目录遍历 + 每个 $I 一次小文件读，比 Shell 那条路快一个
//! 数量级，而且**不需要初始化 COM**。
//!
//! $I 头的两种格式（实测本机 Windows 10 为版本 2）：
//! ```text
//! v2: 0x00 u32 版本 | 0x04 u32 ? | 0x08 u64 大小 | 0x10 FILETIME | 0x18 u32 路径字符数(含结尾 NUL) | 0x1C UTF-16LE 路径
//! v1: 0x00 u32 版本 | 0x04 u32 大小 | 0x08 FILETIME            | 0x10 UTF-16LE 路径（到文件尾）
//! ```

use std::path::{Path, PathBuf};

use windows::core::HSTRING;
use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};

/// 固定磁盘（GetDriveTypeW 的返回值）。没有符号常量，直接按文档取值。
const DRIVE_FIXED: u32 = 3;

/// 回收站里的一项。
#[derive(Clone, Debug)]
pub struct Item {
    /// 实体文件名（`$R......`），还原/彻底删除都靠它配对
    pub id: String,
    /// 删除前的原始路径（含文件名）
    pub original: PathBuf,
    /// 只含文件名的显示名
    pub name: String,
    pub size: u64,
    /// 删除时间（FILETIME：1601 起 100ns）
    pub deleted: i64,
    /// `$R` 实体路径
    pub r_path: PathBuf,
    /// `$I` 信息路径
    pub i_path: PathBuf,
    pub is_dir: bool,
}

/// 当前用户的 SID 字符串（`S-1-5-21-...`）。取不到返回 None，调用方退化为扫全部子目录。
fn current_sid() -> Option<String> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return None;
        }
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        if len == 0 {
            let _ = CloseHandle(token);
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            len,
            &mut len,
        );
        let _ = CloseHandle(token);
        if ok.is_err() {
            return None;
        }
        let tu = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut raw: windows::core::PWSTR = windows::core::PWSTR::null();
        if ConvertSidToStringSidW(tu.User.Sid, &mut raw).is_err() {
            return None;
        }
        let s = raw.to_string().unwrap_or_default();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

/// 所有存在回收站的盘符（只取固定磁盘；U 盘/光驱没有回收站）。
fn bin_roots() -> Vec<PathBuf> {
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = format!("{letter}:\\");
        let hs = HSTRING::from(root.as_str());
        // DRIVE_FIXED = 3：只有固定磁盘才有 $Recycle.Bin
        if unsafe { GetDriveTypeW(&hs) } == DRIVE_FIXED {
            out.push(PathBuf::from(format!("{letter}:\\$Recycle.Bin")));
        }
    }
    out
}

/// 遍历回收站，返回全部条目（按删除时间倒序，最近删除的在前）。
pub fn list() -> Vec<Item> {
    let sid = current_sid();
    let mut items = Vec::new();
    for root in bin_roots() {
        let Ok(rd) = std::fs::read_dir(&root) else { continue };
        for sid_dir in rd.flatten() {
            // 只认本用户的 SID 目录：多用户机器上不能把别人的回收站列出来。
            // 取不到 SID 时退化成扫全部子目录（至少保证功能可用）。
            if let Some(s) = &sid {
                let dirname = sid_dir.file_name().to_string_lossy().to_uppercase();
                if dirname != s.to_uppercase() {
                    continue;
                }
            }
            let Ok(inner) = std::fs::read_dir(sid_dir.path()) else { continue };
            for e in inner.flatten() {
                let fname = e.file_name().to_string_lossy().to_string();
                if !fname.starts_with("$I") {
                    continue;
                }
                let Ok(data) = std::fs::read(e.path()) else { continue };
                if let Some(it) = parse_info(&fname, &e.path(), &data) {
                    items.push(it);
                }
            }
        }
    }
    items.sort_by(|a, b| b.deleted.cmp(&a.deleted));
    items
}

fn parse_info(fname: &str, i_path: &Path, data: &[u8]) -> Option<Item> {
    if data.len() < 24 {
        return None;
    }
    let u32at = |o: usize| -> u32 {
        if o + 4 > data.len() {
            return 0;
        }
        u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]])
    };
    let u64at = |o: usize| -> u64 {
        if o + 8 > data.len() {
            return 0;
        }
        u64::from_le_bytes([
            data[o], data[o + 1], data[o + 2], data[o + 3], data[o + 4], data[o + 5], data[o + 6],
            data[o + 7],
        ])
    };
    let version = u32at(0);
    let (size, deleted, path_off) = match version {
        2 => (u64at(8), u64at(0x10) as i64, 0x1Cusize),
        1 => (u32at(4) as u64, u64at(8) as i64, 0x10usize),
        _ => return None,
    };
    let path_end = if version == 2 {
        let chars = u32at(0x18) as usize;
        (path_off + chars * 2).min(data.len())
    } else {
        data.len()
    };
    if path_end <= path_off || path_end > data.len() {
        return None;
    }
    let mut units: Vec<u16> = data[path_off..path_end]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    // 结尾 NUL 不算路径内容
    while units.last() == Some(&0) {
        units.pop();
    }
    if units.is_empty() {
        return None;
    }
    let original = PathBuf::from(String::from_utf16_lossy(&units));

    let r_name = format!("$R{}", &fname[2..]);
    let r_path = i_path.parent().unwrap_or(Path::new("")).join(&r_name);
    if !r_path.exists() {
        return None;
    }
    let is_dir = std::fs::metadata(&r_path).map(|m| m.is_dir()).unwrap_or(false);
    let name = original
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| r_name.clone());
    Some(Item {
        id: r_name,
        original,
        name,
        size,
        deleted,
        r_path,
        i_path: i_path.to_path_buf(),
        is_dir,
    })
}

/// 还原：把 `$R` 实体搬回原始路径（必要时补建目录）。返回成功条数。
pub fn restore(items: &[Item]) -> usize {
    let mut ok = 0;
    for it in items {
        if let Some(parent) = it.original.parent() {
            if !parent.exists() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let target = unique_target(&it.original);
        let moved = if it.is_dir {
            move_dir(&it.r_path, &target)
        } else {
            std::fs::rename(&it.r_path, &target).is_ok()
        };
        if moved {
            let _ = std::fs::remove_file(&it.i_path);
            ok += 1;
        }
    }
    ok
}

/// 彻底删除：删掉 `$R` 实体与 `$I` 信息文件。返回成功条数。
pub fn remove(items: &[Item]) -> usize {
    let mut ok = 0;
    for it in items {
        let gone = if it.is_dir {
            std::fs::remove_dir_all(&it.r_path).is_ok()
        } else {
            std::fs::remove_file(&it.r_path).is_ok()
        };
        // 实体删不掉就别删信息文件，否则回收站里会留下一条永远还原不了的幽灵项
        if gone {
            let _ = std::fs::remove_file(&it.i_path);
            ok += 1;
        }
    }
    ok
}

/// 清空回收站：优先走系统 API，失败再自己逐项删。
pub fn empty() -> bool {
    use windows::Win32::UI::Shell::SHEmptyRecycleBinW;
    unsafe {
        let r = SHEmptyRecycleBinW(None, windows::core::PCWSTR::null(), 0);
        if r.is_ok() {
            return true;
        }
    }
    let items = list();
    !items.is_empty() && remove(&items) > 0
}

/// 回收站总条目数与总字节数（供侧栏/状态栏显示）。
pub fn summary() -> (usize, u64) {
    let items = list();
    (items.len(), items.iter().map(|i| i.size).sum())
}

/// 目标已存在时自动加后缀，避免还原时覆盖同名的新文件。
fn unique_target(p: &Path) -> PathBuf {
    if !p.exists() {
        return p.to_path_buf();
    }
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let parent = p.parent().unwrap_or(Path::new(""));
    for i in 1..1000 {
        let cand = parent.join(format!("{stem} ({i}){ext}"));
        if !cand.exists() {
            return cand;
        }
    }
    p.to_path_buf()
}

/// 目录的移动：同盘 rename，跨盘则复制后删除。
fn move_dir(from: &Path, to: &Path) -> bool {
    if std::fs::rename(from, to).is_ok() {
        return true;
    }
    if copy_dir_recursive(from, to).is_err() {
        return false;
    }
    let _ = std::fs::remove_dir_all(from);
    true
}

fn copy_dir_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)?.flatten() {
        let target = to.join(e.file_name());
        let meta = e.metadata()?;
        if meta.is_dir() {
            copy_dir_recursive(&e.path(), &target)?;
        } else {
            std::fs::copy(e.path(), &target)?;
        }
    }
    Ok(())
}
