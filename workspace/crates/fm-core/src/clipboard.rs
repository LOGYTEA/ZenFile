//! 剪贴板互通（技术方案 §6.4）。
//!
//! 必须使用标准 CF_HDROP，并且要与 Explorer 双向互通：
//!   写入侧额外附带 Preferred DropEffect（剪切时才有），
//!   读取侧据此判断粘贴应走复制还是移动。

use std::path::PathBuf;

use windows::core::HSTRING;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW,
    SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalUnlock, GLOBAL_ALLOC_FLAGS,
};

use crate::error::{Error, Result};

/// CF_HDROP = 15（Windows 标准剪贴板格式，hardcode 以避免不同 crate 版本导出差异）
const CF_HDROP_FMT: u32 = 15;
/// GMEM_MOVEABLE | GMEM_ZEROINIT
const GHND: u32 = 0x0042;

const CFSTR_PREFERREDDROPEFFECT: &str = "Preferred DropEffect";

/// CF_UNICODETEXT = 13：用于"复制路径"这类纯文本场景。
const CF_UNICODETEXT_FMT: u32 = 13;

/// 把一段 UTF-16 文本放进剪贴板（Ctrl+Shift+C 复制路径用）。
pub fn set_text(text: &str) -> Result<()> {
    let mut units: Vec<u16> = text.encode_utf16().collect();
    units.push(0);
    let bytes = (units.len() * 2) as usize;
    unsafe {
        OpenClipboard(None)?;
        EmptyClipboard()?;
        let alloc = GlobalAlloc(GLOBAL_ALLOC_FLAGS(GHND), bytes);
        let res = (|| -> Result<()> {
            let h = match alloc {
                Ok(v) if !v.0.is_null() => v,
                _ => return Err(Error::Other("GlobalAlloc 失败".into())),
            };
            let p = GlobalLock(h) as *mut u16;
            if p.is_null() {
                return Err(Error::Other("GlobalLock 失败".into()));
            }
            std::ptr::copy_nonoverlapping(units.as_ptr(), p, units.len());
            let _ = GlobalUnlock(h);
            SetClipboardData(CF_UNICODETEXT_FMT, Some(HANDLE(h.0)))?;
            Ok(())
        })();
        let _ = CloseClipboard();
        res
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClipMode {
    Copy,
    Move,
}

#[repr(C)]
struct DropFiles {
    p_files: u32,
    pt_x: i32,
    pt_y: i32,
    f_nc: i32,
    f_wide: i32,
}

/// 把一组文件放进剪贴板。`mode` 决定 Explorer 粘贴时的语义（剪切/复制）。
pub fn set_files(files: &[PathBuf], mode: ClipMode) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    // double-null-terminated UTF-16 路径列表
    let mut blob: Vec<u16> = Vec::new();
    for f in files {
        blob.extend(f.to_string_lossy().as_ref().encode_utf16());
        blob.push(0);
    }

    unsafe {
        OpenClipboard(Some(windows::Win32::Foundation::HWND(std::ptr::null_mut())))?;
        EmptyClipboard()?;

        let header_size = std::mem::size_of::<DropFiles>();
        let total = header_size + blob.len() * 2;
        let hg = GlobalAlloc(GLOBAL_ALLOC_FLAGS(GHND), total + 2)?;
        let gh = HGLOBAL(hg.0);
        let ptr = GlobalLock(gh) as *mut u8;
        if ptr.is_null() {
            GlobalFree(Some(hg))?;
            CloseClipboard()?;
            return Err(Error::Other("GlobalLock 失败".into()));
        }
        let header = ptr as *mut DropFiles;
        (*header).p_files = header_size as u32;
        (*header).pt_x = 0;
        (*header).pt_y = 0;
        (*header).f_nc = 0;
        (*header).f_wide = 1;
        let tail = ptr.add(header_size) as *mut u16;
        std::ptr::copy_nonoverlapping(blob.as_ptr(), tail, blob.len());
        GlobalUnlock(gh)?;

        if SetClipboardData(CF_HDROP_FMT, Some(HANDLE(hg.0))).is_err() {
            GlobalFree(Some(hg))?;
            CloseClipboard()?;
            return Err(Error::Other("SetClipboardData(CF_HDROP) 失败".into()));
        }

        // Preferred DropEffect：4 字节 little-endian DROPEFFECT
        let effect_format = RegisterClipboardFormatW(&HSTRING::from(CFSTR_PREFERREDDROPEFFECT));
        let hg2 = GlobalAlloc(GLOBAL_ALLOC_FLAGS(GHND), 4)?;
        let p2 = GlobalLock(hg2) as *mut u32;
        if !p2.is_null() {
            *p2 = match mode {
                ClipMode::Copy => 1, // DROPEFFECT_COPY
                ClipMode::Move => 2, // DROPEFFECT_MOVE
            };
            GlobalUnlock(hg2)?;
            let _ = SetClipboardData(effect_format, Some(HANDLE(hg2.0)));
        }

        CloseClipboard()?;
    }
    Ok(())
}

/// 读取剪贴板中的 CF_HDROP。返回 None 表示当前剪贴板不含文件。
pub fn get_files() -> Result<Option<(Vec<PathBuf>, ClipMode)>> {
    unsafe {
        OpenClipboard(Some(windows::Win32::Foundation::HWND(std::ptr::null_mut())))?;
        let handle = match GetClipboardData(CF_HDROP_FMT) {
            Ok(h) if !h.is_invalid() => HANDLE(h.0),
            _ => {
                CloseClipboard()?;
                return Ok(None);
            }
        };
        let gh = HGLOBAL(handle.0);
        let ptr = GlobalLock(gh) as *const DropFiles;
        if ptr.is_null() {
            CloseClipboard()?;
            return Ok(None);
        }
        let offset = (*ptr).p_files as usize;
        let wide = (ptr as *const u8).add(offset) as *const u16;
        let mut files = Vec::new();
        let mut cursor = 0usize;
        loop {
            let ch = *wide.add(cursor);
            if ch == 0 {
                // 连续两个 0 表示列表结束
                if *wide.add(cursor + 1) == 0 || *wide.add(cursor) == 0 {
                    if cursor == 0 {
                        break;
                    }
                    let next = *wide.add(cursor + 1);
                    if next == 0 {
                        break;
                    }
                }
            }
            // 取一段到下一个 0
            let mut len = 0usize;
            while *wide.add(cursor + len) != 0 {
                len += 1;
            }
            if len == 0 {
                break;
            }
            let slice = std::slice::from_raw_parts(wide.add(cursor), len);
            files.push(PathBuf::from(String::from_utf16_lossy(slice)));
            cursor += len + 1;
            if *wide.add(cursor) == 0 {
                break;
            }
        }
        GlobalUnlock(gh)?;

        let effect_format = RegisterClipboardFormatW(&HSTRING::from(CFSTR_PREFERREDDROPEFFECT));
        let mut mode = ClipMode::Copy;
        if let Ok(h2) = GetClipboardData(effect_format) {
            let p = GlobalLock(HGLOBAL(h2.0)) as *const u32;
            if !p.is_null() {
                match *p {
                    2 => mode = ClipMode::Move,
                    1 => mode = ClipMode::Copy,
                    _ => {}
                }
                GlobalUnlock(HGLOBAL(h2.0))?;
            }
        }
        CloseClipboard()?;
        if files.is_empty() {
            Ok(None)
        } else {
            Ok(Some((files, mode)))
        }
    }
}

/// 判断剪贴板是否为空（"粘贴是否可用"的粗粒度判断）。
pub fn has_files() -> bool {
    matches!(get_files(), Ok(Some(_)))
}
