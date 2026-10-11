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
    DeleteObject, FillRect, GetObjectW, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION,
    BITMAP, HBITMAP, HDC,
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
/// 缩略图上界。每张占 `size²×4` 字节，比 HICON 费得多，上界给小一点。
const THUMB_CACHE_MAX: usize = 256;

/// 这几类文件的图标**只属于它自己**，不能按扩展名归并：
/// 每个 exe/dll/ico 都嵌着自己的图标，.lnk 显示的是目标图标，.url 显示网站图标。
/// 其余类型（txt/cpp/png…）全系统共用同一个图标，按扩展名取即可，没必要逐文件问 Shell。
const PER_FILE_ICON_EXT: &[&str] = &[
    "exe", "dll", "ico", "lnk", "url", "scr", "msi", "cpl", "msc", "drv", "ocx", "sys", "appref-ms",
];

pub struct IconCache {
    small: HashMap<String, HICON>,
    large: HashMap<String, HICON>,
    /// 大图标视图用的**真实缩略图**（HBITMAP，来自 IShellItemImageFactory）。
    ///
    /// key = "尺寸:小写路径"。值同样是 Option：`None` = 问过但没拿到，别每帧重试。
    thumbs: HashMap<String, Option<HBITMAP>>,
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
            thumbs: HashMap::new(),
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

    /// 大图标视图要的缩略图。
    ///
    /// `SHGetFileInfoW` 的 `SHGFI_LARGEICON` 只给 32×32；把它 `DrawIconEx` 拉到
    /// 64/96 就是把 32 个像素插值成 96 个 —— 糊成一团还带锯齿，尺寸也和资源管理
    /// 器对不上。资源管理器走的其实是 `IShellItemImageFactory`（图片/视频给真缩略
    /// 图，其余给 256×256 的图标），这里跟同一条路。
    pub fn thumb(&mut self, path: &Path, size: i32) -> Option<HBITMAP> {
        let key = format!("{size}:{}", path.to_string_lossy().to_lowercase());
        if let Some(v) = self.thumbs.get(&key) {
            return *v;
        }
        let v = shell_thumb(path, size);
        // 缩略图一张就是 size×size×4 字节，别让它无限长：过界整表重建。
        if self.thumbs.len() >= THUMB_CACHE_MAX {
            self.clear_thumbs();
        }
        self.thumbs.insert(key, v);
        v
    }

    /// 画一张缩略图到 `size × size` 的格子里（等比缩放并居中）。
    /// 失败返回 false，调用方应退回 HICON 那条路。
    pub fn draw_thumb(&self, hdc: HDC, bmp: HBITMAP, x: i32, y: i32, size: i32) -> bool {
        if bmp.is_invalid() || size <= 0 {
            return false;
        }
        unsafe {
            let mut bm: BITMAP = std::mem::zeroed();
            if GetObjectW(
                windows::Win32::Graphics::Gdi::HGDIOBJ(bmp.0),
                std::mem::size_of::<BITMAP>() as i32,
                Some(&mut bm as *mut _ as *mut core::ffi::c_void),
            ) == 0
            {
                return false;
            }
            let (w, h) = (bm.bmWidth, bm.bmHeight);
            if w <= 0 || h <= 0 {
                return false;
            }
            let mem = CreateCompatibleDC(Some(hdc));
            if mem.is_invalid() {
                return false;
            }
            let old = SelectObject(mem, bmp.into());
            let mut bf: BLENDFUNCTION = std::mem::zeroed();
            bf.BlendOp = AC_SRC_OVER as u8;
            bf.SourceConstantAlpha = 255;
            bf.AlphaFormat = AC_SRC_ALPHA as u8;
            // 等比缩放居中：缩略图不一定是正方形（宽图/竖图都是），
            // 硬拉成正方会把人压扁。
            let dw = if w >= h { size } else { (size * w) / h };
            let dh = if w >= h { (size * h) / w } else { size };
            let dx = (size - dw) / 2;
            let dy = (size - dh) / 2;
            let ok = AlphaBlend(hdc, x + dx, y + dy, dw, dh, mem, 0, 0, w, h, bf);
            SelectObject(mem, old);
            let _ = DeleteDC(mem);
            ok.as_bool()
        }
    }

    /// 清掉缩略图缓存。与 `clear_path_icons` 同一时机调用。
    fn clear_thumbs(&mut self) {
        for (_, b) in self.thumbs.drain() {
            if let Some(b) = b {
                if !b.is_invalid() {
                    unsafe {
                        let _ = DeleteObject(b.into());
                    }
                }
            }
        }
    }

    /// 文件落地之后把**路径相关**的图标缓存作废。
    ///
    /// `dir` 缓存存的是 `Option`：一次查询失败会记成 `None`，之后永远不再问。
    /// 于是"刚拷进来的文件夹（此时还没有 desktop.ini）"一旦被记成 `None`，
    /// 就算之后图标真的有了也不会再刷新 —— 表现就是"复制过来的东西没有图标，
    /// 得重启软件才有"。复制/移动/删除完成后调一次即可。
    pub fn clear_path_icons(&mut self) {
        for (_, h) in self.dir.drain() {
            if let Some(h) = h {
                if !h.0.is_null() {
                    unsafe {
                        let _ = DestroyIcon(h);
                    }
                }
            }
        }
        self.clear_thumbs();
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

/// 向 Shell 要一张 `size × size` 的缩略图（图片/视频/文档封面是真内容，
/// 其余给 256×256 的大图标）。拿不到返回 None。
fn shell_thumb(path: &Path, size: i32) -> Option<HBITMAP> {
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::UI::Shell::{
        IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_ICONONLY,
        SIIGBF_RESIZETOFIT, SIIGBF_THUMBNAILONLY,
    };
    let s = path.to_string_lossy();
    if s.is_empty() || size <= 0 {
        return None;
    }
    unsafe {
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(&HSTRING::from(s.as_ref()), None).ok()?;
        let want = SIZE { cx: size, cy: size };
        // 先要真缩略图；Shell 说"这个类型没有缩略图"时再退成大号图标。
        // 两条路都不加 SIIGBF_SCALEUP：宁可 32×32 清晰，也不要把小图放大糊掉。
        for flags in [
            SIIGBF_RESIZETOFIT.0 | SIIGBF_THUMBNAILONLY.0,
            SIIGBF_RESIZETOFIT.0 | SIIGBF_ICONONLY.0,
        ] {
            if let Ok(hbmp) = factory.GetImage(want, windows::Win32::UI::Shell::SIIGBF(flags)) {
                if !hbmp.is_invalid() {
                    return Some(hbmp);
                }
            }
        }
        None
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

/// 菜单里条目图标的位图缓存（`HBITMAP` 句柄，长期持有）。
///
/// 自绘菜单的 `MenuEntry.bmp` 要的是 HBITMAP 而不是 HICON：菜单每次弹出都会重建
/// 条目，不缓存的话一次右键就漏十几张位图。数量受"系统里带 ShellNew 的扩展名"
/// 限制（十几个），常驻没问题。
static mut MENU_BMP: Option<Vec<(String, HBITMAP)>> = None;

/// 把某个扩展名的系统图标烘成一张 16×16 的 32bpp 位图，供自绘菜单使用。
pub fn menu_bmp_for(ext: &str) -> Option<isize> {
    let key = ext.trim_start_matches('.').to_lowercase();
    unsafe {
        let cache = MENU_BMP.get_or_insert_with(Vec::new);
        if let Some((_, b)) = cache.iter().find(|(k, _)| *k == key) {
            return Some(b.0 as isize);
        }
        let icon = icon_for_menu(&key);
        if icon.0.is_null() {
            return None;
        }
        let b = icon_to_bmp(icon, 16)?;
        cache.push((key, b));
        Some(b.0 as isize)
    }
}

/// 「新建 → 文件夹」那一项的图标（通用文件夹，不按扩展名取）。
pub fn folder_menu_bmp() -> Option<isize> {
    menu_bmp_for("\u{0001}folder")
}

/// 菜单位图要的 HICON：这里**不能**走 IconCache（它是 &mut 且属于 App，
/// 菜单行是在 &App 下构建的），所以单独用 SHGetFileInfo 取一次并缓存住。
fn icon_for_menu(key: &str) -> HICON {
    let probe = if key.starts_with('\u{0001}') {
        "probe_folder".to_string()
    } else if key.is_empty() {
        "probe".to_string()
    } else {
        format!("probe.{key}")
    };
    let attrs = if key.starts_with('\u{0001}') {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    let flags = SHGFI_BASE | SHGFI_SMALLICON.0;
    let mut sfi = SHFILEINFOW::default();
    unsafe {
        SHGetFileInfoW(
            &HSTRING::from(probe.as_str()),
            attrs,
            Some(&mut sfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            windows::Win32::UI::Shell::SHGFI_FLAGS(flags),
        );
        sfi.hIcon
    }
}

/// 把一个 HICON 画成 32bpp 位图（带 alpha），供菜单 `draw_bmp` 使用。
fn icon_to_bmp(icon: HICON, size: i32) -> Option<HBITMAP> {
    if icon.0.is_null() {
        return None;
    }
    unsafe {
        let src = windows::Win32::Graphics::Gdi::GetDC(None);
        let mem = CreateCompatibleDC(Some(src));
        let _ = windows::Win32::Graphics::Gdi::ReleaseDC(None, src);
        if mem.is_invalid() {
            return None;
        }
        // 必须是 32bpp 且 alpha 全 0 的底：DrawIconEx 只写有颜色的像素，
        // 底不清零的话图标四周会留下一圈上一次的脏像素（就是那个"小方块"）。
        let mut hdr: windows::Win32::Graphics::Gdi::BITMAPINFO = std::mem::zeroed();
        hdr.bmiHeader.biSize = std::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32;
        hdr.bmiHeader.biWidth = size;
        hdr.bmiHeader.biHeight = -size; // 自上而下
        hdr.bmiHeader.biPlanes = 1;
        hdr.bmiHeader.biBitCount = 32;
        hdr.bmiHeader.biCompression = windows::Win32::Graphics::Gdi::BI_RGB.0;
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bmp = windows::Win32::Graphics::Gdi::CreateDIBSection(
            Some(mem),
            &hdr,
            windows::Win32::Graphics::Gdi::DIB_RGB_COLORS,
            &mut bits,
            None,
            0,
        )
        .unwrap_or_default();
        if bmp.is_invalid() {
            let _ = DeleteDC(mem);
            return None;
        }
        let old = SelectObject(mem, bmp.into());
        let _ = DrawIconEx(mem, 0, 0, icon, size, size, 0, None, DI_NORMAL);
        SelectObject(mem, old);
        let _ = DeleteDC(mem);
        Some(bmp)
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
