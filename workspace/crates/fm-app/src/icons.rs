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
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{
    AlphaBlend, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC,
    DeleteObject, FillRect, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION, HDC,
};
use windows::Win32::Foundation::COLORREF;
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
};
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHFILEINFOW, SHGFI_DISPLAYNAME, SHGFI_ICON, SHGFI_LARGEICON,
    SHGFI_SMALLICON, SHGFI_USEFILEATTRIBUTES,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, DrawIconEx, HICON, DI_NORMAL};

const SHGFI_BASE: u32 = SHGFI_ICON.0 | SHGFI_USEFILEATTRIBUTES.0;

/// 按路径缓存的图标上界。超过就整表清空（在切换目录时做，不在绘制中做）。
const DIR_CACHE_MAX: usize = 1024;

/// 这几类文件的图标**只属于它自己**，不能按扩展名归并：
/// 每个 exe/dll/ico 都嵌着自己的图标，.lnk 显示的是目标图标，.url 显示网站图标。
/// 其余类型（txt/cpp/png…）全系统共用同一个图标，按扩展名取即可，没必要逐文件问 Shell。
const PER_FILE_ICON_EXT: &[&str] = &[
    "exe", "dll", "ico", "lnk", "url", "scr", "msi", "cpl", "msc", "drv", "ocx", "sys", "appref-ms",
];

pub struct IconCache {
    small: HashMap<String, HICON>,
    large: HashMap<String, HICON>,
    /// **具体**目录 / 驱动器根 / 需逐文件取图标的文件的图标：
    /// key = "L:<小写路径>" / "S:<小写路径>"。
    /// 值是 Option：`None` 表示"已经问过 Shell，但没拿到"，避免每帧重绘都对每个
    /// 路径重复发起一次 Shell 查询。
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
            extension.trim_start_matches('.').to_lowercase()
        };
        let map = if large { &mut self.large } else { &mut self.small };
        if let Some(h) = map.get(&key) {
            return *h;
        }
        // 探测名**必须带点**：`SHGFI_USEFILEATTRIBUTES` 让 Shell 完全不碰磁盘，
        // 它只按 pszPath 的"扩展名"去查关联图标。早先这里拼成 `probe` + key
        // （即 "probetxt"），Shell 看到的是一个没有扩展名的文件 —— 于是 txt /
        // docx / png / 未知类型……全部塌成同一个"通用文件"图标。
        let probe = if is_dir {
            "probe_folder".to_string()
        } else if key.is_empty() {
            "probe".to_string()
        } else {
            format!("probe.{key}")
        };
        // 文件那一侧必须显式给 FILE_ATTRIBUTE_NORMAL：传 0 时 Shell 的分支
        // 判定不稳定，取到的不一定是"普通文件"的图标。
        let attrs = if is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_NORMAL
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
        self.path_icon(path, large)
            .unwrap_or_else(|| self.icon(true, "", large))
    }

    /// 文件图标入口。
    ///
    /// exe/dll/ico/lnk/url 这类"图标只属于它自己"的类型走真实路径；
    /// 其余按扩展名拿通用图标（上千个 .txt 逐个问 Shell 没意义，纯属浪费）。
    pub fn file_icon(&mut self, path: &Path, extension: &str, large: bool) -> HICON {
        let ext = extension.trim_start_matches('.').to_ascii_lowercase();
        if !PER_FILE_ICON_EXT.contains(&ext.as_str()) {
            return self.icon(false, &ext, large);
        }
        self.path_icon(path, large)
            .unwrap_or_else(|| self.icon(false, &ext, large))
    }

    /// 按真实路径向 Shell 取图标（目录、驱动器根、exe/lnk 等共用这一条路）。
    fn path_icon(&mut self, path: &Path, large: bool) -> Option<HICON> {
        let prefix = if large { "L:" } else { "S:" };
        let norm = path.to_string_lossy().to_lowercase();
        let dkey = format!("{prefix}{norm}");
        // 先拷出来再可变借用自己，否则撞 borrow checker
        let cached: Option<Option<HICON>> = self.dir.get(&dkey).copied();
        match cached {
            Some(h) => h,
            None => {
                let h = self.shell_path_icon(path, large);
                self.dir.insert(dkey, h);
                h
            }
        }
    }

    /// 目录的本地化显示名（Shell 的 `SHGFI_DISPLAYNAME`）。
    ///
    /// 资源管理器里 `C:\Users` 显示成"用户"、用户目录下的 `Desktop` 显示成"桌面"，
    /// 靠的是 Shell 读 `desktop.ini` 里的 `LocalizedResourceName`。这里走同一条路，
    /// 不用自己解析 ini（系统那份是 UTF-16LE 带 BOM，自己解必错）。
    pub fn display_name(path: &Path) -> Option<String> {
        let s = path.to_string_lossy();
        if s.is_empty() {
            return None;
        }
        let mut sfi = SHFILEINFOW::default();
        let res = unsafe {
            SHGetFileInfoW(
                &HSTRING::from(s.as_ref()),
                FILE_ATTRIBUTE_DIRECTORY,
                Some(&mut sfi),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_DISPLAYNAME,
            )
        };
        if res == 0 {
            return None;
        }
        let v: Vec<u16> = sfi.szDisplayName.iter().take_while(|&&c| c != 0).copied().collect();
        let name = from_wide(&v);
        if name.is_empty() {
            None
        } else {
            Some(name)
        }
    }

    /// 向 Shell 查询某个真实目录的图标；拿不到返回 None（调用方回退通用图标）。
    fn shell_path_icon(&mut self, path: &Path, large: bool) -> Option<HICON> {
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

    /// 淡化绘制（隐藏项 / 受保护的系统文件用）。
    ///
    /// 资源管理器对隐藏项的做法就是把图标整体调淡，而不是换个图标 ——
    /// 这里先把图标画进一张 32bpp 位图，再用 AlphaBlend 以指定不透明度贴出去。
    pub fn draw_faded(&self, hdc: HDC, icon: HICON, x: i32, y: i32, size: i32, alpha: u8) {
        if icon.0.is_null() || alpha >= 255 {
            self.draw(hdc, icon, x, y, size);
            return;
        }
        unsafe {
            let src = windows::Win32::Graphics::Gdi::GetDC(None);
            let mem = CreateCompatibleDC(Some(src));
            let _ = windows::Win32::Graphics::Gdi::ReleaseDC(None, src);
            if mem.is_invalid() {
                self.draw(hdc, icon, x, y, size);
                return;
            }
            let bmp = CreateCompatibleBitmap(hdc, size, size);
            let old = SelectObject(mem, bmp.into());
            // 先铺满透明底：图标是带 alpha 的，直接画上去边角会残留上一次的内容
            let b = CreateSolidBrush(COLORREF(0));
            let r = RECT { left: 0, top: 0, right: size, bottom: size };
            FillRect(mem, &r, b);
            let _ = DeleteObject(b.into());
            let _ = DrawIconEx(mem, 0, 0, icon, size, size, 0, None, DI_NORMAL);

            let mut bf: BLENDFUNCTION = std::mem::zeroed();
            bf.BlendOp = AC_SRC_OVER as u8;
            bf.BlendFlags = 0;
            bf.SourceConstantAlpha = alpha;
            bf.AlphaFormat = AC_SRC_ALPHA as u8;
            let _ = AlphaBlend(hdc, x, y, size, size, mem, 0, 0, size, size, bf);

            SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
        }
    }
}

/// 系统库存图标（`SHGetStockIconInfo`），按 SIID 缓存。
///
/// 回收站、控制面板这类目标**没有真实路径**，问不到 Shell 关联图标，只能走
/// 库存图标。句柄由本进程持有，缓存常驻，不销毁（数量个位数，可忽略）。
static mut STOCK: Option<Vec<(i32, bool, HICON)>> = None;

pub fn stock_icon(siid: i32, large: bool) -> Option<HICON> {
    unsafe {
        let cache = STOCK.get_or_insert_with(Vec::new);
        if let Some((_, _, h)) = cache.iter().find(|(s, l, _)| *s == siid && *l == large) {
            return Some(*h);
        }
        use windows::Win32::UI::Shell::{
            SHGetStockIconInfo, SHGSI_ICON, SHGSI_LARGEICON, SHGSI_SMALLICON, SHSTOCKICONID,
            SHSTOCKICONINFO,
        };
        let mut sii: SHSTOCKICONINFO = std::mem::zeroed();
        sii.cbSize = std::mem::size_of::<SHSTOCKICONINFO>() as u32;
        let flags = SHGSI_ICON | if large { SHGSI_LARGEICON } else { SHGSI_SMALLICON };
        let ok = SHGetStockIconInfo(SHSTOCKICONID(siid), flags, &mut sii);
        if ok.is_err() || sii.hIcon.0.is_null() {
            return None;
        }
        cache.push((siid, large, sii.hIcon));
        Some(sii.hIcon)
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
