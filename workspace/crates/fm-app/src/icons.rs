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

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use windows::core::HSTRING;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{
    AlphaBlend, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC,
    DeleteObject, FillRect, GetObjectW, SelectObject, SetBrushOrgEx, SetStretchBltMode,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAP, BLENDFUNCTION, HBITMAP, HDC, HALFTONE,
};
use windows::Win32::Foundation::COLORREF;
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
};
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHFILEINFOW, SHGFI_DISPLAYNAME, SHGFI_ICON, SHGFI_LARGEICON,
    SHGFI_SMALLICON, SHGFI_USEFILEATTRIBUTES,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, DrawIconEx, GetIconInfo, HICON, DI_NORMAL};

const SHGFI_BASE: u32 = SHGFI_ICON.0 | SHGFI_USEFILEATTRIBUTES.0;

/// 按路径缓存的图标上界。超过就整表清空（在切换目录时做，不在绘制中做）。
const DIR_CACHE_MAX: usize = 1024;
/// 缩略图上界。每张占 `size²×4` 字节，比 HICON 费得多，上界给小一点。
const THUMB_CACHE_MAX: usize = 256;
/// 放大位图上界（96²×4 ≈ 36KB/张，384 张 ≈ 14MB）。满了整表清空重来。
const UPSCALED_CACHE_MAX: usize = 384;
/// 缩略图重采样缓存上界（thumb 原图大、重采样结果 96²×4，256 张 ≈ 9MB）。
const THUMB_SCALED_MAX: usize = 256;

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
    /// 缩略图缓存：值 = (位图, 询问时刻)。
    /// `None` 位图只记 **2 秒**（thumb() 按 elapsed 判定）—— 旧的"失败永久
    /// 缓存"让一次偶发失败（目录刚创建 / Shell 忙 / 慢盘首问）把该图标
    /// 整个会话都钉在 32px HICON 拉 96 的糊锯齿上（第30轮实锤）。
    thumbs: HashMap<String, (Option<HBITMAP>, std::time::Instant)>,
    /// **具体**目录 / 驱动器根 / 需逐文件取图标的文件的图标：
    /// key = "L:<小写路径>" / "S:<小写路径>"。
    /// 值是 Option：`None` 表示"已经问过 Shell，但没拿到"，避免每帧重绘都对每个
    /// 路径重复发起一次 Shell 查询。
    dir: HashMap<String, Option<HICON>>,
    /// **放大后的图标位图**（premultiplied 双线性插值结果，见 `upscaled_bmp`）：
    /// key = (HICON 句柄值, 目标尺寸)。HICON 由上面几张表长期持有、指针即身份；
    /// 指针失效（Drop）时整表一起清。`RefCell` 是为了 draw/&self 签名不动 ——
    /// 调用点遍布 paint 里十几处 `&App` 函数，改成 `&mut` 要传染整条链。
    upscaled: RefCell<HashMap<(usize, i32), HBITMAP>>,
    /// **缩略图重采样缓存**：key = (HBITMAP 句柄值, dw, dh)。
    /// 缩略图（IShellItemImageFactory，256px 起）放大/缩小到格子尺寸不再走
    /// GDI StretchBlt —— 它**只插值 RGB、不插值 alpha 通道**，边缘 alpha
    /// 最近邻采样就是文件夹图标那圈硬锯齿阶梯。手工重采样后按位图缓存。
    scaled_thumbs: RefCell<HashMap<(usize, i32, i32), HBITMAP>>,
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
        // 放大位图缓存（HBITMAP）：和 HICON 分开销毁，别混进上面的 DestroyIcon
        for (_, b) in self.upscaled.get_mut().drain() {
            if !b.is_invalid() {
                unsafe {
                    let _ = DeleteObject(b.into());
                }
            }
        }
        for (_, b) in self.scaled_thumbs.get_mut().drain() {
            if !b.is_invalid() {
                unsafe {
                    let _ = DeleteObject(b.into());
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
            upscaled: RefCell::new(HashMap::new()),
            scaled_thumbs: RefCell::new(HashMap::new()),
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
        // 归一化进 key：`E:/x` 与 `E:\x` 是同一个目录，别各存一份 HICON
        let norm = path.to_string_lossy().replace('/', "\\").to_lowercase();
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
        // 正斜杠入口（地址栏/启动参数）会让 SHGetFileInfoW 返回 0，
        // 与 shell_thumb 同一道归一化（第30轮日志实锤）。
        let s = path.to_string_lossy().replace('/', "\\");
        if s.is_empty() {
            return None;
        }
        let mut sfi = SHFILEINFOW::default();
        let res = unsafe {
            SHGetFileInfoW(
                &HSTRING::from(s.as_str()),
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
    /// 系统 JUMBO 图像列表（256px）里该路径的原生图标。
    ///
    /// `SHDefExtractIcon` 对文件夹恒 E_FAIL、`SHGFI_LARGEICON` 只有 32px
    /// （放大到 96 = 糊锯齿）。JUMBO 列表就是资源管理器画大图标用的那张表：
    /// `SHGFI_SYSICONINDEX` 拿系统图像列表索引 → 同一索引到 JUMBO 表取原生
    /// 256px，然后走现有的"缩小 box 平均"管线降到 96 —— 源够大，边缘就锐。
    /// 任何一步失败返回 None，自然落到 SHGFI 32px 兜底。
    fn shell_jumbo_icon(s: &str) -> Option<HICON> {
        use windows::Win32::UI::Controls::{IImageList, ILD_IMAGE, ILD_TRANSPARENT};
        use windows::Win32::UI::Shell::{SHGetImageList, SHGFI_SYSICONINDEX, SHIL_JUMBO};
        if s.is_empty() {
            return None;
        }
        unsafe {
            let mut sfi = SHFILEINFOW::default();
            let res = SHGetFileInfoW(
                &HSTRING::from(s),
                FILE_ATTRIBUTE_DIRECTORY,
                Some(&mut sfi),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_SYSICONINDEX,
            );
            if res == 0 || sfi.iIcon < 0 {
                return None;
            }
            let list: IImageList = match SHGetImageList(SHIL_JUMBO as i32) {
                Ok(l) => l,
                Err(e) => {
                    return None;
                }
            };
            let h = match list.GetIcon(sfi.iIcon, ILD_TRANSPARENT.0 | ILD_IMAGE.0) {
                Ok(h) => h,
                Err(e) => {
                    return None;
                }
            };
            if h.0.is_null() {
                None
            } else {
                Some(h)
            }
        }
    }

    fn shell_path_icon(&mut self, path: &Path, large: bool) -> Option<HICON> {
        // 正斜杠归一化（地址栏 / 启动参数 / 单实例转发都可能传 `E:/x`）：
        // SHGetFileInfoW 对正斜杠路径 res=0 直接失败，图标链会整体退到
        // 32px 通用款再拉 96 —— 就是用户看到的糊锯齿（第30轮日志实锤：
        // `jumbo(E:/download): shgfi fail res=0`，而 thumb 链第28轮就修了）。
        let s = path.to_string_lossy().replace('/', "\\");
        if s.is_empty() {
            return None;
        }
        // 大图标走 `SHDefExtractIcon`：与资源管理器**同源** —— 会按
        // desktop.ini 的 IconResource / 文件夹类型默认取图标（Win10 黄色
        // 默认款），且给的是放大尺寸。SHGFI_LARGEICON 只有 32px，放大发虚；
        // 而 IShellItemImageFactory 那条 thumb 路给文件夹的是另一套浅蓝
        // "通用文件夹"图标，颜色和 Explorer 对不上（用户反馈）。
        if large {
            unsafe {
                use windows::core::PCWSTR;
                use windows::Win32::UI::Shell::SHDefExtractIconW;
                let w = wide(&s);
                let mut hl = HICON::default();
                let mut hs = HICON::default();
                let got = SHDefExtractIconW(
                    PCWSTR(w.as_ptr()),
                    0,
                    0,
                    Some(&mut hl as *mut _),
                    Some(&mut hs as *mut _),
                    96,
                );
                if got.is_ok() && !hl.0.is_null() {
                    if !hs.0.is_null() {
                        let _ = DestroyIcon(hs);
                    }
                    return Some(hl);
                }
                // 失败的半成品句柄都要收掉
                if !hl.0.is_null() {
                    let _ = DestroyIcon(hl);
                }
                if !hs.0.is_null() {
                    let _ = DestroyIcon(hs);
                }
                // SHDefExtractIcon 对**文件夹**恒 E_FAIL（第28轮实锤）；
                // 退而求其次走系统 JUMBO 图像列表（256px，资源管理器画
                // 大图标用的同一张表）—— 再不行才落 SHGFI 的 32px，
                // 因为 32 拉到 96 就是用户截图里那种糊锯齿（第30轮复现实锤）。
                if let Some(h) = Self::shell_jumbo_icon(&s) {
                    return Some(h);
                }
            }
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
                &HSTRING::from(s.as_str()),
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
        // 诊断开关：ZENFILE_NO_THUMB=1 强制全部走 HICON 回退路径，
        // 用于复现"thumb 失败时"的图标质量（第30轮锯齿诊断，定位后删）。
        if std::env::var_os("ZENFILE_NO_THUMB").is_some() {
            return None;
        }
        let key = format!("{size}:{}", path.to_string_lossy().to_lowercase());
        // 命中：成功位图直接用；失败条目 2 秒内当"还在失败"，过期则删掉重问
        // （否则一次偶发失败会把该图标整个会话钉在 32px 拉 96 的糊上）。
        let expired = match self.thumbs.get(&key) {
            Some((Some(b), _)) => return Some(*b),
            Some((None, t)) if t.elapsed() < std::time::Duration::from_secs(2) => return None,
            Some((None, _)) => true,
            None => false,
        };
        if expired {
            self.thumbs.remove(&key);
        }
        let v = shell_thumb(path, size);
        // 缩略图一张就是 size×size×4 字节，别让它无限长：过界整表重建。
        if self.thumbs.len() >= THUMB_CACHE_MAX {
            self.clear_thumbs();
        }
        self.thumbs.insert(key, (v, std::time::Instant::now()));
        v
    }

    /// 画一张缩略图到 `size × size` 的格子里（等比缩放并居中）。
    /// 失败返回 false，调用方应退回 HICON 那条路。
    ///
    /// **不走 GDI StretchBlt**（老路 HALFTONE）：它只插值 RGB 三通道、
    /// **不插值 alpha** —— 边缘 alpha 最近邻采样，出来的就是文件夹图标
    /// 那圈硬锯齿阶梯（用户截图反馈的"粗糙"）。这里改成：
    /// 读出 32bpp 像素 → 判定 alpha 空间 → 缩小用 box 面积平均 / 放大用
    /// premultiplied 双线性 → 按 (bmp, dw, dh) 缓存 → AlphaBlend 贴出。
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
            // 等比缩放居中：缩略图不一定是正方形（宽图/竖图都是），
            // 硬拉成正方会把人压扁。
            let (dw, dh) = if w >= h {
                (size, ((size as i64 * h as i64) / w as i64).max(1) as i32)
            } else {
                (((size as i64 * w as i64) / h as i64).max(1) as i32, size)
            };
            let dx = (size - dw) / 2;
            let dy = (size - dh) / 2;

            // 缓存命中
            let key = (bmp.0 as usize, dw, dh);
            if let Some(sb) = self.scaled_thumbs.borrow().get(&key) {
                let sb = *sb;
                return blend_bmp(hdc, sb, x + dx, y + dy, dw, dh, 255);
            }

            // **没有"1:1 直贴"捷径**（第31轮）：Shell 交回来的位图即便尺寸正好，
            // 边缘也常见 2~3px 一级的硬台阶和青绿杂边（那是 Shell 自己放大糊出来
            // 的成品）。直贴等于把糙图原样照搬 —— 看起来"优化了一堆代码但画面
            // 一点没变"。统一走 premultiplied 重采样（已是 pmp 时重采样是恒等+
            // 抗锯齿），代价被 scaled_thumbs 缓存吃掉。
            // 读像素（thumb 此刻没有被选进任何 DC，可直接 GetDIBits）
            let Some(data) = read_bmp_rgba(bmp, w, h) else {
                return false;
            };
            // alpha 空间判定：WIC 出来的一般是 premultiplied；个别路径是
            // straight。转成 pmp 后再插值（对 pmp 再乘一次 alpha 会二次变暗）。
            let pmp = if looks_straight(&data) { to_pmp(&data) } else { data };
            let out = resample_pmp(&pmp, w, h, dw, dh);
            if out.is_empty() {
                return false;
            }
            let Some(sbmp) = pmp_to_bmp(&out, dw, dh) else {
                return false;
            };
            {
                let mut c = self.scaled_thumbs.borrow_mut();
                if c.len() >= THUMB_SCALED_MAX {
                    for (_, b) in c.drain() {
                        if !b.is_invalid() {
                            let _ = DeleteObject(b.into());
                        }
                    }
                }
                c.insert(key, sbmp);
            }
            blend_bmp(hdc, sbmp, x + dx, y + dy, dw, dh, 255)
        }
    }

    /// 清掉缩略图缓存。与 `clear_path_icons` 同一时机调用。
    fn clear_thumbs(&mut self) {
        for (_, (b, _)) in self.thumbs.drain() {
            if let Some(b) = b {
                if !b.is_invalid() {
                    unsafe {
                        let _ = DeleteObject(b.into());
                    }
                }
            }
        }
        // **派生表必须一起清**（第39轮审计 #14）：scaled_thumbs 的 key 是
        // HBITMAP 句柄值，源位图一删 GDI 就会复用句柄 —— 新缩略图拿到刚
        // 释放的号就命中旧条目，返回上一张图的重采样结果，表现为
        // "文件 A 显示成文件 B 的图"，且永不自愈（重启才好）。
        // 上面 thumbs 过界驱逐（clear_thumbs 的唯一其它触发点）同理。
        self.clear_derived();
    }

    /// 清两张**按句柄值当 key** 的派生位图表。
    ///
    /// 谁销毁了源句柄（HBITMAP/HICON），谁就得来清一次 —— 句柄值会被
    /// GDI/Shell 高度复用，不清理等于给未来的同名 key 预埋一个错缓存
    /// （第39轮审计 #14）。
    fn clear_derived(&mut self) {
        for (_, b) in self.upscaled.get_mut().drain() {
            if !b.is_invalid() {
                unsafe {
                    let _ = DeleteObject(b.into());
                }
            }
        }
        for (_, b) in self.scaled_thumbs.get_mut().drain() {
            if !b.is_invalid() {
                unsafe {
                    let _ = DeleteObject(b.into());
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
        // dir 里的 HICON 销毁了，按句柄派生的 upscaled 一起作废
        //（第39轮审计 #14：句柄复用会撞出"错图标"缓存）
        self.clear_derived();
    }

    pub fn draw(&self, hdc: HDC, icon: HICON, x: i32, y: i32, size: i32) {
        if icon.0.is_null() || size <= 0 {
            return;
        }
        unsafe {
            // 放大（大图标视图 48/96px）走高质量插值路径：DrawIconEx 直接拉伸
            // 是 mask/最近邻那套，边缘硬切还带 alpha halo —— 图标外圈一圈
            // 白/绿杂边、锯齿很重（用户截图反馈的就是它）。
            if size > 32 {
                if let Some(bmp) = self.upscaled_bmp(icon, size) {
                    if blend_bmp(hdc, bmp, x, y, size, size, 255) {
                        return;
                    }
                }
                // 取像素失败（老式 mask-only 图标之类）→ 退回老路
                let _ = SetStretchBltMode(hdc, HALFTONE);
                let _ = SetBrushOrgEx(hdc, 0, 0, None);
                let _ = DrawIconEx(hdc, x, y, icon, size, size, 0, None, DI_NORMAL);
                return;
            }
            // ≤32 是原尺寸（或缩到行图标）：DrawIconEx 本身就够
            let _ = DrawIconEx(hdc, x, y, icon, size, size, 0, None, DI_NORMAL);
        }
    }

    /// 取（或生成）放大后的 premultiplied 位图。
    fn upscaled_bmp(&self, icon: HICON, size: i32) -> Option<HBITMAP> {
        let key = (icon.0 as usize, size);
        if let Some(b) = self.upscaled.borrow().get(&key) {
            return Some(*b);
        }
        let bmp = render_upscaled(icon, size)?;
        let mut c = self.upscaled.borrow_mut();
        if c.len() >= UPSCALED_CACHE_MAX {
            for (_, b) in c.drain() {
                if !b.is_invalid() {
                    unsafe {
                        let _ = DeleteObject(b.into());
                    }
                }
            }
        }
        c.insert(key, bmp);
        Some(bmp)
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
        // 放大 + 淡出（隐藏的大图标）：同样先走高质量插值位图，再带
        // SourceConstantAlpha 贴出 —— 老路径里 DrawIconEx 拉伸的锯齿照样在。
        if size > 32 {
            if let Some(bmp) = self.upscaled_bmp(icon, size) {
                if unsafe { blend_bmp(hdc, bmp, x, y, size, size, alpha) } {
                    return;
                }
            }
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
            // 32×32 HICON 拉到 96px：默认 BLACKONWHITE 边缘会糊。
            // DrawIconEx 走 StretchBlt，stretch mode 设了才生效。
            let _ = SetStretchBltMode(mem, HALFTONE);
            let _ = SetBrushOrgEx(mem, 0, 0, None);
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
    // **路径归一化成反斜杠**：SHCreateItemFromParsingName 对正斜杠盘符路径
    // （"E:/download"，从启动参数/面包屑传进来时很常见）直接失败 → thumb
    // 返回 None → 文件夹的内容图案（download 的 ISO 光盘、games 的齿轮…）
    // 整个丢失，只剩纯蓝图标，和资源管理器对不上（用户反馈"颜色完全对不上"）。
    let s = path.to_string_lossy().replace('/', "\\");
    if s.is_empty() || size <= 0 {
        return None;
    }
    unsafe {
        let factory: IShellItemImageFactory =
            match SHCreateItemFromParsingName(&HSTRING::from(s.as_str()), None) {
                Ok(f) => f,
                Err(_) => return None,
            };
        // **向 Shell 要 2 倍分辨率的源图，再由我们自己 box 缩小到目标尺寸。**
        //
        // 这才是治锯齿的关键（第31轮实锤）：直接按目标尺寸要图时，Shell 交回来的
        // 常常就是一份"把小图放大糊出来的"成品 —— 边缘带 2~3px 一级的硬台阶，
        // 而且 alpha 插值留下青绿杂边。我们拿到 96 就 1:1 直贴，等于把这份糙图
        // 原样照搬到屏幕上，看起来"优化了一堆代码但画面一点没变"。
        //
        // 要 192 让 box 缩小真正做超采样（面积平均）：台阶被抹平，杂边也被平均掉。
        // 拿不到 2 倍源时（老图标/特殊类型）再退回目标尺寸，并让 draw_thumb
        // 走它自己的重采样路径。
        let want = SIZE { cx: size * 2, cy: size * 2 };
        // 先要真缩略图；Shell 说"这个类型没有缩略图"时再退成大号图标。
        // 两条路都不加 SIIGBF_SCALEUP：宁可低分辨率清晰，也不要把小图放大糊掉。
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
        // 2 倍源要不到（部分自定义图标只登记了固定尺寸）：退回目标尺寸。
        let want = SIZE { cx: size, cy: size };
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

// ---------------------------------------------------------------- 高质量放大

/// 把 HICON 以**原生尺寸**画进 32bpp DIB 并读出像素（top-down BGRA）。
///
/// 透明底先清零：DrawIconEx 只写有颜色的像素，底不清零会留下一次的脏像素。
/// alpha 通道**全 0**（老式 mask-only 图标，GDI 不写 alpha）时返回 None ——
/// 这种图没有可用的 alpha，双线性插值会把它糊成一块黑，调用方退回
/// DrawIconEx 老路。
fn icon_native_rgba(icon: HICON, ow: i32, oh: i32) -> Option<Vec<u8>> {
    if ow <= 0 || oh <= 0 || icon.0.is_null() {
        return None;
    }
    unsafe {
        let src = windows::Win32::Graphics::Gdi::GetDC(None);
        let mem = CreateCompatibleDC(Some(src));
        let _ = windows::Win32::Graphics::Gdi::ReleaseDC(None, src);
        if mem.is_invalid() {
            return None;
        }
        let mut hdr: windows::Win32::Graphics::Gdi::BITMAPINFO = std::mem::zeroed();
        hdr.bmiHeader.biSize =
            std::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32;
        hdr.bmiHeader.biWidth = ow;
        hdr.bmiHeader.biHeight = -oh; // 自上而下
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
        if bmp.is_invalid() || bits.is_null() {
            let _ = DeleteDC(mem);
            return None;
        }
        std::ptr::write_bytes(bits as *mut u8, 0, (ow * oh * 4) as usize);
        let old = SelectObject(mem, bmp.into());
        // cxDesired/cyDesired = 0 → 按图标原生尺寸画，不缩放（无损取样）
        let ok = DrawIconEx(mem, 0, 0, icon, 0, 0, 0, None, DI_NORMAL).is_ok();
        SelectObject(mem, old);
        let n = (ow * oh * 4) as usize;
        let data = if ok {
            std::slice::from_raw_parts(bits as *const u8, n).to_vec()
        } else {
            Vec::new()
        };
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        if data.is_empty() {
            return None;
        }
        if !data.iter().skip(3).step_by(4).any(|&a| a > 0) {
            return None;
        }
        Some(data)
    }
}

/// 从 32bpp HBITMAP 读出 top-down BGRA 像素（原样字节，不做颜色空间转换）。
fn read_bmp_rgba(bmp: HBITMAP, w: i32, h: i32) -> Option<Vec<u8>> {
    unsafe {
        let src = windows::Win32::Graphics::Gdi::GetDC(None);
        if src.0.is_null() {
            return None;
        }
        let mut hdr: windows::Win32::Graphics::Gdi::BITMAPINFO = std::mem::zeroed();
        hdr.bmiHeader.biSize =
            std::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32;
        hdr.bmiHeader.biWidth = w;
        hdr.bmiHeader.biHeight = -h; // 自上而下
        hdr.bmiHeader.biPlanes = 1;
        hdr.bmiHeader.biBitCount = 32;
        hdr.bmiHeader.biCompression = windows::Win32::Graphics::Gdi::BI_RGB.0;
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let n = windows::Win32::Graphics::Gdi::GetDIBits(
            src,
            bmp,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            &mut hdr,
            windows::Win32::Graphics::Gdi::DIB_RGB_COLORS,
        );
        let _ = windows::Win32::Graphics::Gdi::ReleaseDC(None, src);
        if n == 0 {
            None
        } else {
            Some(buf)
        }
    }
}

/// 启发式判断缓冲是 **straight alpha**（存在半透明且 rgb 分量超过 alpha）。
/// WIC/IShellItemImageFactory 出来的一般已是 premultiplied；个别路径是 straight。
fn looks_straight(d: &[u8]) -> bool {
    let mut i = 0;
    let mut checked = 0usize;
    while i + 3 < d.len() && checked < 4096 {
        let a = d[i + 3];
        if a > 8 && a < 250 {
            checked += 1;
            if d[i] > a || d[i + 1] > a || d[i + 2] > a {
                return true;
            }
        }
        i += 28; // 每 7 像素采一个，够统计判断
    }
    let _ = checked;
    false
}

/// straight → premultiplied。
fn to_pmp(d: &[u8]) -> Vec<u8> {
    let mut o = d.to_vec();
    for px in o.chunks_exact_mut(4) {
        let a = px[3] as u32;
        px[0] = ((px[0] as u32 * a + 127) / 255) as u8;
        px[1] = ((px[1] as u32 * a + 127) / 255) as u8;
        px[2] = ((px[2] as u32 * a + 127) / 255) as u8;
    }
    o
}

/// 重采样 **premultiplied** 像素（dw×dh 可非方形）。
///
/// - 放大：双线性（各通道直接加权 —— 源已是 pmp，不能再乘 alpha）
/// - 缩小：box 面积平均。对 alpha 边缘锯齿最有效 —— GDI StretchBlt
///   不插值 alpha 通道，边缘呈硬阶梯（用户截图文件夹图标的粗糙感）。
fn resample_pmp(src: &[u8], sw: i32, sh: i32, dw: i32, dh: i32) -> Vec<u8> {
    if sw <= 0 || sh <= 0 || dw <= 0 || dh <= 0 || src.len() < (sw * sh * 4) as usize {
        return Vec::new();
    }
    let mut out = vec![0u8; (dw * dh * 4) as usize];
    if dw >= sw && dh >= sh {
        for dy in 0..dh {
            let syf = ((dy as f32 + 0.5) * sh as f32 / dh as f32 - 0.5).clamp(0.0, (sh - 1) as f32);
            let y0 = syf as i32;
            let y1 = (y0 + 1).min(sh - 1);
            let fy = syf - y0 as f32;
            for dx in 0..dw {
                let sxf = ((dx as f32 + 0.5) * sw as f32 / dw as f32 - 0.5).clamp(0.0, (sw - 1) as f32);
                let x0 = sxf as i32;
                let x1 = (x0 + 1).min(sw - 1);
                let fx = sxf - x0 as f32;
                let corners = [
                    (x0, y0, (1.0 - fx) * (1.0 - fy)),
                    (x1, y0, fx * (1.0 - fy)),
                    (x0, y1, (1.0 - fx) * fy),
                    (x1, y1, fx * fy),
                ];
                let mut acc = [0f32; 4];
                for (cx, cy, w) in corners {
                    let o = ((cy * sw + cx) * 4) as usize;
                    acc[0] += src[o] as f32 * w;
                    acc[1] += src[o + 1] as f32 * w;
                    acc[2] += src[o + 2] as f32 * w;
                    acc[3] += src[o + 3] as f32 * w;
                }
                let o = ((dy * dw + dx) * 4) as usize;
                out[o] = acc[0].round().clamp(0.0, 255.0) as u8;
                out[o + 1] = acc[1].round().clamp(0.0, 255.0) as u8;
                out[o + 2] = acc[2].round().clamp(0.0, 255.0) as u8;
                out[o + 3] = acc[3].round().clamp(0.0, 255.0) as u8;
            }
        }
    } else {
        for dy in 0..dh {
            let y0 = (dy as i64 * sh as i64 / dh as i64) as i32;
            let y1 = ((((dy + 1) as i64 * sh as i64 / dh as i64) as i32).max(y0 + 1)).min(sh);
            for dx in 0..dw {
                let x0 = (dx as i64 * sw as i64 / dw as i64) as i32;
                let x1 = ((((dx + 1) as i64 * sw as i64 / dw as i64) as i32).max(x0 + 1)).min(sw);
                let n = ((x1 - x0) * (y1 - y0)).max(1) as u32;
                let mut sb = 0u32;
                let mut sg = 0u32;
                let mut sr = 0u32;
                let mut sa = 0u32;
                for yy in y0..y1 {
                    for xx in x0..x1 {
                        let o = ((yy * sw + xx) * 4) as usize;
                        sb += src[o] as u32;
                        sg += src[o + 1] as u32;
                        sr += src[o + 2] as u32;
                        sa += src[o + 3] as u32;
                    }
                }
                let o = ((dy * dw + dx) * 4) as usize;
                out[o] = (sb / n) as u8;
                out[o + 1] = (sg / n) as u8;
                out[o + 2] = (sr / n) as u8;
                out[o + 3] = (sa / n) as u8;
            }
        }
    }
    out
}

/// 把 premultiplied 像素包成 32bpp DIB（供 AlphaBlend 的 AC_SRC_ALPHA 用）。
fn pmp_to_bmp(data: &[u8], dw: i32, dh: i32) -> Option<HBITMAP> {
    if dw <= 0 || dh <= 0 {
        return None;
    }
    unsafe {
        let src = windows::Win32::Graphics::Gdi::GetDC(None);
        let mem = CreateCompatibleDC(Some(src));
        let _ = windows::Win32::Graphics::Gdi::ReleaseDC(None, src);
        if mem.is_invalid() {
            return None;
        }
        let mut hdr: windows::Win32::Graphics::Gdi::BITMAPINFO = std::mem::zeroed();
        hdr.bmiHeader.biSize =
            std::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32;
        hdr.bmiHeader.biWidth = dw;
        hdr.bmiHeader.biHeight = -dh;
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
        let _ = DeleteDC(mem);
        if bmp.is_invalid() || bits.is_null() {
            if !bmp.is_invalid() {
                let _ = DeleteObject(bmp.into());
            }
            return None;
        }
        let n = (dw * dh * 4) as usize;
        if data.len() < n {
            let _ = DeleteObject(bmp.into());
            return None;
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), bits as *mut u8, n);
        Some(bmp)
    }
}

/// 原生取像素 → premultiplied 双线性放大 → DIB 位图。失败返回 None（调用方退老路）。
fn render_upscaled(icon: HICON, size: i32) -> Option<HBITMAP> {
    // 1) 原生尺寸（GetIconInfo → hbmColor 的宽高；拿不到就按 32）
    let (mut ow, mut oh) = (32i32, 32i32);
    unsafe {
        let mut ii: windows::Win32::UI::WindowsAndMessaging::ICONINFO = std::mem::zeroed();
        if GetIconInfo(icon, &mut ii).is_ok() {
            if !ii.hbmColor.is_invalid() {
                let mut bm: BITMAP = std::mem::zeroed();
                let got = GetObjectW(
                    windows::Win32::Graphics::Gdi::HGDIOBJ(ii.hbmColor.0),
                    std::mem::size_of::<BITMAP>() as i32,
                    Some(&mut bm as *mut _ as *mut core::ffi::c_void),
                );
                if got > 0 && bm.bmWidth > 0 && bm.bmHeight > 0 {
                    ow = bm.bmWidth;
                    oh = bm.bmHeight;
                }
            }
            if !ii.hbmColor.is_invalid() {
                let _ = DeleteObject(ii.hbmColor.into());
            }
            if !ii.hbmMask.is_invalid() {
                let _ = DeleteObject(ii.hbmMask.into());
            }
        }
    }
    // 2) 无损取样 → straight 转 pmp → 重采样 → 打包
    let data = icon_native_rgba(icon, ow, oh)?;
    let pmp = to_pmp(&data); // DrawIconEx 到 DIB 出来的是 straight alpha
    let out = resample_pmp(&pmp, ow, oh, size, size);
    if out.is_empty() {
        return None;
    }
    pmp_to_bmp(&out, size, size)
}

/// 把缓存位图贴到目标 DC。`alpha` = 整体不透明度（隐藏项淡出用）。
///
/// 源是 **premultiplied** 32bpp（`upscaled_bmp` 保证），所以 AlphaFormat
/// 必须是 AC_SRC_ALPHA —— 直接 AC_SRC_OVER 贴 straight alpha 会出暗边。
unsafe fn blend_bmp(hdc: HDC, bmp: HBITMAP, x: i32, y: i32, dw: i32, dh: i32, alpha: u8) -> bool {
    if dw <= 0 || dh <= 0 {
        return false;
    }
    let mem = CreateCompatibleDC(Some(hdc));
    if mem.is_invalid() {
        return false;
    }
    let old = SelectObject(mem, bmp.into());
    let mut bf: BLENDFUNCTION = std::mem::zeroed();
    bf.BlendOp = AC_SRC_OVER as u8;
    bf.BlendFlags = 0;
    bf.SourceConstantAlpha = alpha;
    bf.AlphaFormat = AC_SRC_ALPHA as u8;
    let ok = AlphaBlend(hdc, x, y, dw, dh, mem, 0, 0, dw, dh, bf);
    SelectObject(mem, old);
    let _ = DeleteDC(mem);
    ok.as_bool()
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
