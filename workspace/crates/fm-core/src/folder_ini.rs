//! 文件夹的 `desktop.ini`：自定义图标 / 文件夹类型 / 文件夹图片。
//!
//! ## 为什么必须原样保序地改
//!
//! 这个文件里可能还有 Shell 写进去的其它键（`LocalizedResourceName`、
//! `InfoTip`、`[ViewState]` 的 `Mode`/`Vid`…）。整文件重写会把它们冲掉，
//! 表现就是"改了个图标，文件夹的显示名也变回英文了"。所以这里做的是
//! **按节定位、按键替换**，不认识的节和键一律原样留着。
//!
//! ## 编码
//!
//! 资源管理器写出来的是 **UTF-16LE 带 BOM**（本机 `%USERPROFILE%\Pictures`
//! 那份就是）。读的时候两种都认：有 BOM 按 UTF-16LE 解，没有就按 UTF-8 容错解。
//! 写的时候一律 UTF-16LE 带 BOM，与系统保持一致。

use std::path::{Path, PathBuf};

/// 资源管理器"自定义"页能改的那几项。
#[derive(Clone, Default)]
pub struct FolderIni {
    /// `[.ShellClassInfo] IconResource=` 形如 `%SystemRoot%\system32\imageres.dll,-113`
    pub icon_resource: Option<String>,
    /// `[.ShellClassInfo] IconFile=`
    pub icon_file: Option<String>,
    /// `[.ShellClassInfo] IconIndex=`
    pub icon_index: Option<i32>,
    /// `[ViewState] FolderType=`（Pictures / Music / Videos / Documents / Generic …）
    pub folder_type: Option<String>,
    /// `[.ShellClassInfo] InfoTip=`
    pub info_tip: Option<String>,
    /// `[{BE098140-…}] IconArea_Image=` 文件夹图片
    pub area_image: Option<String>,
    /// 文件里还有别的内容（说明不是我们造的、也不是空的）
    pub has_other: bool,
}

/// 文件夹图片的节名：Shell 的固定 CLSID，照抄即可。
const VIEW_CLSID: &str = "{BE098140-A513-11D0-A3A4-00C04FD706EC}";

/// 五种文件夹模板 -> `IconResource`。
///
/// 这几个索引不是猜的：从本机 `%USERPROFILE%\{Pictures,Music,Videos,Documents}`
/// 的 `desktop.ini` 里读出来的，与资源管理器写的一致。
pub const FOLDER_TYPES: [(&str, &str, Option<&str>); 5] = [
    ("常规项", "Generic", None),
    ("文档", "Documents", Some("%SystemRoot%\\system32\\imageres.dll,-112")),
    ("图片", "Pictures", Some("%SystemRoot%\\system32\\imageres.dll,-113")),
    ("音乐", "Music", Some("%SystemRoot%\\system32\\imageres.dll,-108")),
    ("视频", "Videos", Some("%SystemRoot%\\system32\\imageres.dll,-189")),
];

impl FolderIni {
    pub fn is_blank(&self) -> bool {
        self.icon_resource.is_none()
            && self.icon_file.is_none()
            && self.icon_index.is_none()
            && self.folder_type.is_none()
            && self.info_tip.is_none()
            && self.area_image.is_none()
            && !self.has_other
    }

    /// 当前模板在 `FOLDER_TYPES` 里的下标（认不出来按"常规项"算）。
    pub fn type_index(&self) -> usize {
        FOLDER_TYPES
            .iter()
            .position(|(_, key, _)| self.folder_type.as_deref() == Some(*key))
            .unwrap_or(0)
    }
}

// ------------------------------------------------------------------ 读

pub fn path_of(dir: &Path) -> PathBuf {
    dir.join("desktop.ini")
}

pub fn read(dir: &Path) -> FolderIni {
    let mut out = FolderIni::default();
    let Ok(raw) = std::fs::read(path_of(dir)) else {
        return out;
    };
    let text = decode(&raw);
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
        {
            section = name.trim().to_string();
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            out.has_other = true;
            continue;
        };
        let k = k.trim();
        let v = v.trim();
        let known = match (&section[..], k) {
            (".ShellClassInfo", "IconResource") => {
                out.icon_resource = Some(v.to_string());
                true
            }
            (".ShellClassInfo", "IconFile") => {
                out.icon_file = Some(v.to_string());
                true
            }
            (".ShellClassInfo", "IconIndex") => {
                out.icon_index = v.parse::<i32>().ok();
                true
            }
            (".ShellClassInfo", "InfoTip") => {
                out.info_tip = Some(v.to_string());
                true
            }
            ("ViewState", "FolderType") => {
                out.folder_type = Some(v.to_string());
                true
            }
            (s, "IconArea_Image") if s.eq_ignore_ascii_case(VIEW_CLSID) => {
                out.area_image = Some(v.to_string());
                true
            }
            _ => false,
        };
        if !known {
            out.has_other = true;
        }
    }
    out
}

fn decode(raw: &[u8]) -> String {
    if raw.len() >= 2 && raw[0] == 0xFF && raw[1] == 0xFE {
        let u16s: Vec<u16> = raw[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&u16s);
    }
    String::from_utf8_lossy(raw).to_string()
}

// ------------------------------------------------------------------ 写

/// 把改动落回 `desktop.ini`。
///
/// * 只动认识的键，其余行原样保留；
/// * 值为 None 表示该键删掉；
/// * 结果为空就删掉这个文件，不留一个空壳（空 desktop.ini 会让 Shell 认为
///   这个文件夹"被自定义过"，从而不再读它的 LocalizedResourceName 之类）。
///
/// 返回是否真的写了盘。
pub fn write(dir: &Path, next: &FolderIni) -> std::io::Result<bool> {
    let file = path_of(dir);
    let existing = std::fs::read(&file).unwrap_or_default();
    let text = if existing.is_empty() {
        String::new()
    } else {
        decode(&existing)
    };

    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    set_key(&mut lines, ".ShellClassInfo", "IconResource", next.icon_resource.as_deref());
    set_key(&mut lines, ".ShellClassInfo", "IconFile", next.icon_file.as_deref());
    set_key(
        &mut lines,
        ".ShellClassInfo",
        "IconIndex",
        next.icon_index.map(|i| i.to_string()).as_deref(),
    );
    set_key(&mut lines, ".ShellClassInfo", "InfoTip", next.info_tip.as_deref());
    set_key(&mut lines, "ViewState", "FolderType", next.folder_type.as_deref());
    set_key(&mut lines, VIEW_CLSID, "IconArea_Image", next.area_image.as_deref());

    // 只留下还能起作用的内容：空行压掉，多余的空节也压掉
    let body = tidy(lines);
    if body.trim().is_empty() {
        if file.exists() {
            // 先把系统/隐藏属性摘掉再删，否则 RemoveFile 会拒绝
            let _ = clear_hidden(&file);
            std::fs::remove_file(&file)?;
            return Ok(false);
        }
        return Ok(false);
    }
    // 同样是为了能覆盖写：desktop.ini 带 system/hidden，直接 File::create 会失败
    let _ = clear_hidden(&file);
    std::fs::write(&file, encode(&body))?;
    let _ = set_hidden(&file);
    Ok(true)
}

/// 改属性位。
///
/// `desktop.ini` 自带 HIDDEN|SYSTEM，带这两个位时 `File::create` / `remove_file`
/// 会被拒（"拒绝访问"），所以写之前必须先摘、写完再戴回去。只读位同理。
fn set_attrs(p: &Path, attrs: u32) -> std::io::Result<()> {
    use windows::Win32::Storage::FileSystem::{FILE_FLAGS_AND_ATTRIBUTES, SetFileAttributesW};
    let wide = windows::core::HSTRING::from(p.to_string_lossy().as_ref());
    unsafe { SetFileAttributesW(&wide, FILE_FLAGS_AND_ATTRIBUTES(attrs)) }
        .map_err(|e| std::io::Error::other(format!("{e}")))
}

fn clear_hidden(p: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    let attrs = std::fs::metadata(p)?.file_attributes();
    // 去掉 HIDDEN(0x2) | SYSTEM(0x4) | READONLY(0x1)
    set_attrs(p, attrs & !(0x1 | 0x2 | 0x4))
}

fn set_hidden(p: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    let attrs = std::fs::metadata(p)?.file_attributes();
    set_attrs(p, attrs | 0x2 | 0x4)
}

/// 在指定节里设置/替换/删除一个键。节不存在就整节追加到末尾。
fn set_key(lines: &mut Vec<String>, section: &str, key: &str, value: Option<&str>) {
    let mut sec_at: Option<usize> = None;
    let mut key_at: Option<usize> = None;
    let mut i = 0usize;
    while i < lines.len() {
        let t = lines[i].trim();
        if let Some(name) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            let name = name.trim();
            if sec_at.is_some() && key_at.is_none() && name.eq_ignore_ascii_case(section) {
                // 同名节出现了第二次（一般不会有），也当作目标
            }
            if name.eq_ignore_ascii_case(section) {
                if let Some(at) = key_at {
                    apply(lines, at, key, value);
                    return;
                }
                sec_at = Some(i);
            } else if sec_at.is_some() {
                // 进了下一节：目标节里没这个键
                if let Some(v) = value {
                    lines.insert(i, format!("{key}={v}"));
                }
                return;
            }
        } else if sec_at.is_some() && key_at.is_none() {
            if let Some((k, _)) = t.split_once('=') {
                if k.trim().eq_ignore_ascii_case(key) {
                    key_at = Some(i);
                }
            }
        }
        i += 1;
    }
    if let Some(at) = key_at {
        apply(lines, at, key, value);
        return;
    }
    if let Some(v) = value {
        if sec_at.is_none() {
            lines.push(format!("[{section}]"));
        }
        lines.push(format!("{key}={v}"));
    }
}

fn apply(lines: &mut Vec<String>, at: usize, key: &str, value: Option<&str>) {
    match value {
        Some(v) => lines[at] = format!("{key}={v}"),
        None => {
            lines.remove(at);
        }
    }
}

/// 压掉空行与空节（键被删光了的节）。
fn tidy(lines: Vec<String>) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut pending_section: Option<String> = None;
    for l in lines {
        let t = l.trim();
        if t.is_empty() {
            continue;
        }
        if t.starts_with('[') {
            pending_section = Some(t.to_string());
            continue;
        }
        if let Some(s) = pending_section.take() {
            out.push(s);
        }
        out.push(t.to_string());
    }
    out.join("\r\n")
}

fn encode(text: &str) -> Vec<u8> {
    let mut b = vec![0xFFu8, 0xFE];
    for u in text.encode_utf16() {
        b.extend_from_slice(&u.to_le_bytes());
    }
    b
}
