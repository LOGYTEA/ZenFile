//! 图标：复用 Shell 提供的系统图标，按扩展名缓存。
//!
//! 纪律（技术方案 §12）：
//!   - 只在行可见时才请求图标；
//!   - 缓存键按扩展名/目录归并，数量上界天然受限；
//!   - 图标由 Shell 持有，本进程只缓存 HICON 句柄。
//!
//! 实现选择：取 HICON 而非系统图像列表 index。
//! 早期用 SHGFI_SYSICONINDEX + ImageList_DrawEx，实测图标被画成 14x16 实心
//! 色块（遮罩未生效，即用户反馈的"图标边上的小方块"）。改用 SHGFI_ICON +
//! DrawIconEx，遮罩由 GDI 处理，边角透明。

use std::collections::HashMap;
use std::path::Path;

use windows::core::HSTRING;
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_SMALLICON,
    SHGFI_USEFILEATTRIBUTES,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, DrawIconEx, HICON, DI_NORMAL};

const SHGFI_BASE: u32 = SHGFI_ICON.0 | SHGFI_USEFILEATTRIBUTES.0;

/// 目录/驱动器图标缓存的上界。超过就整表清空（在切换目录时做，不在绘制中做）。
const DIR_CACHE_MAX: usize = 1024;

pub struct IconCache {
    small: HashMap<String, HICON>,
    large: HashMap<String, HICON>,
    /// **具体**目录 / 驱动器根的图标：key = "L:<小写路径>" / "S:<小写路径>"。
    /// 值是 Option：`None` 表示"已经问过 Shell，但没拿到"，避免每帧重绘都对每个
    /// 目录重复发起一次 Shell 查询。
    dir: HashMap<String, Option<HICON>>,
}

impl Default for IconCache {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for IconCache {
    fn drop(&mut self) {
        for (_, h) in self.small.drain() {
            if !h.0.is_null() {
                unsafe {
                    let _ = DestroyIcon(h);
                }
            }
        }
        for (_, h) in self.large.drain() {
            if !h.0.is_null() {
                unsafe {
                    let _ = DestroyIcon(h);
                }
            }
        }
        for (_, h) in self.dir.drain() {
            if let Some(h) = h {
                if !h.0.is_null() {
                    unsafe {
                        let _ = DestroyIcon(h);
                    }
                }
            }
        }
    }
}

impl IconCache {
    pub fn new() -> Self {
        Self {
            small: HashMap::new(),
            large: HashMap::new(),
            dir: HashMap::new(),
        }
    }

    /// 取"按类型归并"的通用图标句柄（目录用 probe 名字、文件按扩展名）。
    ///
    /// 这条路**不带** `SHGFI_USEFILEATTRIBUTES` 之外的信息：它不会访问磁盘，
    /// 所以给的是"这一类东西的默认图标"。要拿某个具体目录的真实图标（含
    /// `desktop.ini` 自定义图标、驱动器图标）必须走 [`Self::dir_icon`]。
    /// 返回的 HICON 由本缓存持有，不要调用方销毁。
    pub fn icon(&mut self, is_dir: bool, extension: &str, large: bool) -> HICON {
        let key = if is_dir {
            "dir".to_string()
        } else {
            extension.to_lowercase()
        };
        let map = if large { &mut self.large } else { &mut self.small };
        if let Some(h) = map.get(&key) {
            return *h;
        }
        let probe = if is_dir {
            "probe_folder".to_string()
        } else {
            format!("probe{key}")
        };
        let attrs = if is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            Default::default()
        };
        let flags = SHGFI_BASE | if large { SHGFI_LARGEICON.0 } else { SHGFI_SMALLICON.0 };
        let mut sfi = SHFILEINFOW::default();
        let hicon = unsafe {
            SHGetFileInfoW(
                &HSTRING::from(probe.as_str()),
                attrs,
                Some(&mut sfi),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                windows::Win32::UI::Shell::SHGFI_FLAGS(flags),
            );
            sfi.hIcon
        };
        if hicon.0.is_null() {
            return hicon;
        }
        map.insert(key, hicon);
        hicon
    }

    /// 目录/驱动器图标入口。
    ///
    /// 关键：**不能用 `SHGFI_USEFILEATTRIBUTES`**。带上那个标志后 Shell 根本不访问
    /// 磁盘路径，只会按属性猜一个图标 —— 于是 `desktop.ini` 指定的自定义图标一律
    /// 被忽略，所有文件夹都变成同一个通用黄文件夹（实测：Pictures/Downloads/Music
    /// 三个带自定义图标的目录，带标志时取到的都是 #FFE896 那个通用图标）。
    ///
    /// 去掉标志、传真实路径，Shell 会自己读 `desktop.ini`（UTF-16 带 BOM 也好、
    /// `IconResource` 负资源 ID 也好、`IconFile`+`IconIndex` 也好），返回的正是
    /// 资源管理器里看到的那个图标；驱动器根还会直接给硬盘图标。
    pub fn dir_icon(&mut self, path: &Path, large: bool) -> HICON {
        let prefix = if large { "L:" } else { "S:" };
        let norm = path.to_string_lossy().to_lowercase();
        let dkey = format!("{prefix}{norm}");
        // 先拷出来再可变借用自己，否则撞 borrow checker
        let cached: Option<Option<HICON>> = self.dir.get(&dkey).copied();
        let got = match cached {
            Some(h) => h,
            None => {
                let h = self.shell_dir_icon(path, large);
                self.dir.insert(dkey, h);
                h
            }
        };
        match got {
            Some(h) => h,
            None => self.icon(true, "", large),
        }
    }

    /// 向 Shell 查询某个真实目录的图标；拿不到返回 None（调用方回退通用图标）。
    fn shell_dir_icon(&mut self, path: &Path, large: bool) -> Option<HICON> {
        let s = path.to_string_lossy();
        if s.is_empty() {
            return None;
        }
        let flags = SHGFI_ICON
            | if large {
                SHGFI_LARGEICON
            } else {
                SHGFI_SMALLICON
            };
        let mut sfi = SHFILEINFOW::default();
        let res = unsafe {
            SHGetFileInfoW(
                &HSTRING::from(s.as_ref()),
                FILE_ATTRIBUTE_DIRECTORY,
                Some(&mut sfi),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                flags,
            )
        };
        if res == 0 || sfi.hIcon.0.is_null() {
            return None;
        }
        Some(sfi.hIcon)
    }

    /// 缓存上界维护。**只能在切换目录时调用** —— 绘制中途销毁 HICON，会把同一帧
    /// 已经取到、还没画出去的句柄一起干掉。
    pub fn trim_dir_cache(&mut self) {
        if self.dir.len() < DIR_CACHE_MAX {
            return;
        }
        for (_, h) in self.dir.drain() {
            if let Some(h) = h {
                if !h.0.is_null() {
                    unsafe {
                        let _ = DestroyIcon(h);
                    }
                }
            }
        }
    }

    pub fn draw(&self, hdc: HDC, icon: HICON, x: i32, y: i32, size: i32) {
        if icon.0.is_null() {
            return;
        }
        unsafe {
            // DI_NORMAL：用图标自带的遮罩绘制，边角保持透明
            let _ = DrawIconEx(hdc, x, y, icon, size, size, 0, None, DI_NORMAL);
        }
    }
}

/// 供 C 字符串与 Win32 互操作的小工具。
pub(crate) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

pub(crate) fn from_wide(wide: &[u16]) -> String {
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end])
}
