//! 右键「新建」的子菜单：系统与资源管理器**同一份**模板清单。
//!
//! Explorer 的「新建」不是硬编码的，它扫 `HKEY_CLASSES_ROOT\<扩展名>\ShellNew`：
//! 哪个扩展名挂了 ShellNew，哪个就出现在菜单里，显示名取 Shell 的"文件类型名"
//! （于是 .txt 叫"文本文档"、.docx 叫"Microsoft Word 文档"，而不是自己翻译的）。
//! 之前这里只写了"文件夹 / 文本文档"两项，用户装了 Office/WPS/画图之后，
//! 系统里能新建的东西一大堆，菜单里却只有两个 —— 跟系统不一致。

use std::path::{Path, PathBuf};

use windows::core::HSTRING;
use windows::Win32::Foundation::WIN32_ERROR;
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::Win32::System::Registry::{RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CLASSES_ROOT, KEY_READ, REG_BINARY, REG_EXPAND_SZ, REG_SZ};
use windows::Win32::UI::Shell::{SHGetFileInfoW, SHLoadIndirectString, SHFILEINFOW, SHGFI_TYPENAME, SHGFI_USEFILEATTRIBUTES};

/// 一个可新建的模板。
#[derive(Clone)]
pub struct NewTemplate {
    /// 含点的扩展名，如 ".txt"
    pub ext: String,
    /// 菜单上显示的名（Shell 的类型名，如"文本文档"）
    pub label: String,
}

/// 扫出本机全部 ShellNew 模板。HKCR 下带 ShellNew 子键的扩展名通常只有十几个，
/// 每次展开「新建」时扫一遍足够，不做缓存（装了新软件立刻就能看见）。
pub fn templates() -> Vec<NewTemplate> {
    let mut out: Vec<NewTemplate> = Vec::new();
    unsafe {
        let mut root = HKEY::default();
        // 打开 HKCR 根：子键枚举从它开始
        if RegOpenKeyExW(HKEY_CLASSES_ROOT, None, None, KEY_READ, &mut root) != WIN32_ERROR(0) {
            return out;
        }
        let mut i = 0u32;
        loop {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            if RegEnumKeyExW(
                root,
                i,
                Some(windows::core::PWSTR(name.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            ) != WIN32_ERROR(0)
            {
                break;
            }
            i += 1;
            if len == 0 || len as usize > name.len() {
                continue;
            }
            let key = String::from_utf16_lossy(&name[..len as usize]);
            if !key.starts_with('.') || key.len() < 2 {
                continue;
            }
            // 没挂 ShellNew 的扩展名不出现在「新建」里
            let sub_path = HSTRING::from(format!("{key}\\ShellNew").as_str());
            let mut sub = HKEY::default();
            let opened = RegOpenKeyExW(HKEY_CLASSES_ROOT, &sub_path, None, KEY_READ, &mut sub);
            if opened != WIN32_ERROR(0) {
                continue;
            }
            let _ = RegCloseKey(sub);
            // `Command` 是"跑一个外部程序去建"（WPS 的 pdfwpsshellnew 就是这种），
            // 我们建不出真文件，列出来点了没反应更糟，跳过。
            // `ItemName` 不是这回事 —— 它只是**显示名**（本地化资源串），
            // .txt / .bmp 都靠它显示"文本文档"/"BMP 图像"，文件照样用 NullFile 建得出来。
            // 早年把它一起跳过了，结果「新建」里只剩 .rtf 一项，跟系统完全对不上。
            if has_value(&sub_path, "Command") {
                continue;
            }
            // 菜单上显示的是**类型名**（"文本文档"），不是 ItemName —— ItemName 是
            // notepad.exe 里那条 "新建文本文档"，资源管理器拿它当新建文件的名字用，
            // 放到菜单里就成了"新建文本文档"，跟系统对不上。
            let label = type_name(&key);
            out.push(NewTemplate { ext: key, label });
        }
        let _ = RegCloseKey(root);
    }
    // 按类型名排序：注册表顺序是安装顺序，完全不可预期（今天装个 WPS 明天就变）。
    out.sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()));
    out
}

fn has_value(key: &HSTRING, name: &str) -> bool {
    let v = HSTRING::from(name);
    let mut ty = Default::default();
    let mut size = 0u32;
    unsafe {
        RegQueryValueExW(
            {
                let mut h = HKEY::default();
                if RegOpenKeyExW(HKEY_CLASSES_ROOT, key, None, KEY_READ, &mut h) != WIN32_ERROR(0) {
                    return false;
                }
                h
            },
            &v,
            None,
            Some(&mut ty),
            None,
            Some(&mut size),
        ) == WIN32_ERROR(0)
    }
}

/// `ShellNew\ItemName`：新建出来的**文件叫什么**（如 notepad 的 "新建文本文档"）。
///
/// 值多半是 `@%SystemRoot%\system32\notepad.exe,-470` 这种"间接字符串"，
/// 得交给 Shell 去解（自己解析等于重造多语言资源加载，还没法跟着系统语言变）。
/// 解不出来就退化成 "新建{类型名}"。
fn item_name(ext: &str) -> Option<String> {
    let (ty, b) = read_value(ext, "ItemName")?;
    if ty != REG_SZ.0 && ty != REG_EXPAND_SZ.0 {
        return None;
    }
    let s = wide_from_bytes(&b);
    if s.trim().is_empty() {
        return None;
    }
    Some(resolve_indirect(&s).unwrap_or(s))
}

/// 解 `@dll,-id` 形式的间接字符串；不是这种形式就原样返回 None。
fn resolve_indirect(s: &str) -> Option<String> {
    if !s.starts_with('@') {
        return None;
    }
    let src = HSTRING::from(s);
    let mut buf = [0u16; 256];
    unsafe {
        if SHLoadIndirectString(&src, &mut buf, None).is_ok() {
            let v: Vec<u16> = buf.iter().take_while(|&&c| c != 0).copied().collect();
            let out = String::from_utf16_lossy(&v);
            if !out.trim().is_empty() {
                return Some(out);
            }
        }
    }
    None
}

/// Shell 给这个文件类型起的名（"文本文档" / "RTF 格式" / "JPEG 图像"）。
fn type_name(ext: &str) -> String {
    let probe = HSTRING::from(format!("probe{ext}").as_str());
    let mut sfi = SHFILEINFOW::default();
    unsafe {
        let r = SHGetFileInfoW(
            &probe,
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut sfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_TYPENAME | SHGFI_USEFILEATTRIBUTES,
        );
        if r == 0 {
            // 拿不到类型名就用扩展名本身，总比显示空白强
            return ext.trim_start_matches('.').to_uppercase();
        }
    }
    let v: Vec<u16> = sfi.szTypeName.iter().take_while(|&&c| c != 0).copied().collect();
    let s = String::from_utf16_lossy(&v);
    if s.trim().is_empty() {
        ext.trim_start_matches('.').to_uppercase()
    } else {
        s
    }
}

/// 读 `ShellNew` 下的一个值。
fn read_value(ext: &str, name: &str) -> Option<(u32, Vec<u8>)> {
    let path = HSTRING::from(format!("{ext}\\ShellNew").as_str());
    let mut h = HKEY::default();
    unsafe {
        if RegOpenKeyExW(HKEY_CLASSES_ROOT, &path, None, KEY_READ, &mut h) != WIN32_ERROR(0) {
            return None;
        }
    }
    let vname = HSTRING::from(name);
    let mut ty = Default::default();
    let mut size = 0u32;
    unsafe {
        if RegQueryValueExW(h, &vname, None, Some(&mut ty), None, Some(&mut size)) != WIN32_ERROR(0) {
            let _ = RegCloseKey(h);
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        if RegQueryValueExW(h, &vname, None, Some(&mut ty), Some(buf.as_mut_ptr()), Some(&mut size))
            != WIN32_ERROR(0)
        {
            let _ = RegCloseKey(h);
            return None;
        }
        buf.truncate(size as usize);
        let _ = RegCloseKey(h);
        Some((ty.0, buf))
    }
}

fn wide_from_bytes(b: &[u8]) -> String {
    let u: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| u16::from_ne_bytes([c[0], c[1]]))
        .take_while(|&c| c != 0)
        .collect();
    String::from_utf16_lossy(&u)
}

/// 按模板在 `dir` 里建一个新文件，返回建出来的路径。
///
/// 三种 ShellNew 形态：
///   * `NullFile`  → 空文件（txt、rtf 多数是这种）
///   * `Data`      → 把注册表里那段二进制原样写进去（bmp 之类靠它给个空位图）
///   * `FileName`  → 把系统模板文件拷过来（Office/WPS 的空白文档走这条路）
pub fn create(dir: &Path, ext: &str) -> zenfile_core::Result<PathBuf> {
    // 跟资源管理器一样命名：ItemName 给什么就叫什么（"新建文本文档.txt"）
    let stem = item_name(ext).unwrap_or_else(|| format!("新建{}", type_name(ext)));
    let path = zenfile_core::paths::unique_name(dir, &stem, ext);
    let bytes: Vec<u8> = if read_value(ext, "NullFile").is_some() {
        Vec::new()
    } else if let Some((_, b)) = read_value(ext, "Data") {
        b
    } else if let Some((ty, b)) = read_value(ext, "FileName") {
        // 值可能是 REG_SZ / REG_EXPAND_SZ 的相对名（相对于系统 ShellNew 目录）
        if ty == REG_SZ.0 || ty == REG_EXPAND_SZ.0 {
            let name = wide_from_bytes(&b);
            match template_bytes(&name) {
                Some(v) => v,
                None => Vec::new(),
            }
        } else {
            Vec::new()
        }
    } else if let Some((ty, b)) = read_value(ext, "Data") {
        let _ = ty;
        b
    } else {
        Vec::new()
    };
    std::fs::write(&path, &bytes).map_err(zenfile_core::Error::Io)?;
    Ok(path)
}

/// `FileName` 指向的模板文件：可能是绝对路径，也可能是系统 ShellNew 目录里的名字。
fn template_bytes(name: &str) -> Option<Vec<u8>> {
    if name.starts_with('@') {
        // "@shell32.dll,-123" 这种是资源引用，得 Shell 去解，我们解不了
        return None;
    }
    let cands: Vec<PathBuf> = if name.contains(':') || name.starts_with('\\') {
        vec![PathBuf::from(name)]
    } else {
        let mut v = Vec::new();
        if let Ok(w) = std::env::var("SystemRoot") {
            v.push(PathBuf::from(w).join("ShellNew").join(name));
        }
        v.push(PathBuf::from("C:\\Windows\\ShellNew").join(name));
        v.push(PathBuf::from("C:\\WINNT\\ShellNew").join(name));
        v
    };
    for c in cands {
        if let Ok(b) = std::fs::read(&c) {
            return Some(b);
        }
    }
    None
}

/// 注册表里 `REG_BINARY` 常量的类型值（Data 用），这里只为文档留个引用点。
#[allow(dead_code)]
const _DATA_TYPE: u32 = REG_BINARY.0;
