//! 自绘图标主题（两套）。
//!
//! 设置-外观里的图标主题只有两个选项：
//!   - **default = 系统 Shell 图标**（零回归，本模块不接管）；
//!   - **optimized = 本模块参数化自绘的"优化"套**（圆角、柔和、和谐配色）。
//!
//! 实现：SDF（有符号距离场）软渲染到 4× 超采样缓冲，box 降采样出抗锯齿，
//! premultiplied BGRA → `CreateIconIndirect` 出 HICON。所有图形定义在 32 单位
//! 设计网格上，统一圆角语言 + 一套和谐配色。
//!
//! 纪律：
//!   - 图标由本模块渲染，调用方（`IconCache`）负责缓存与销毁；
//!   - default 组合**绝不**落到本模块（active() 的调用方守卫）。

use windows::Win32::Foundation::COLORREF;
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};

/// 一套图标里要画的所有种类。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    Folder,
    Drive,
    ThisPc,
    RecycleEmpty,
    RecycleFull,
    Home,
    Desktop,
    Download,
    Documents,
    Pictures,
    Music,
    Videos,
    Txt,
    ImageFile,
    VideoFile,
    AudioFile,
    Exe,
    Pdf,
    Archive,
    Code,
    Generic,
}

// ---------------------------------------------------------------- 全局主题状态

struct Spec {
    dark: bool,
}

/// 当前生效的主题。`None` = default = 系统图标（默认，本模块不接管）。
///
/// 全局而不是 IconCache 字段：`stock_icon` / `menu_bmp_for` 这两个入口是
/// 自由函数（没有 &mut IconCache，见 icons.rs 里同样原因的 STOCK 静态缓存），
/// 主题也必须能被它们看到。整个应用只有一个主题开关，进程级单例是诚实的建模。
static mut SPEC: Option<Spec> = None;

/// 自绘图标缓存：(Kind, 目标边长) → HICON。句柄由本模块持有并在切主题时销毁。
static mut THEMED: Vec<(Kind, i32, HICON)> = Vec::new();

/// 主题生效时的「已知文件夹路径 → Kind」表（路径小写、反斜杠、无尾分隔符）。
///
/// 一次切主题只解析一轮 `SHGetKnownFolderPath`（6 次 shell 调用），别放在
/// dir_icon 的热路径里每帧问 shell。
static mut KNOWN: Vec<(String, Kind)> = Vec::new();

fn norm_path(p: &str) -> String {
    p.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// 应用主题。`theme` 是配置值（`default`/`optimized`）；`default` 清空状态，
/// 一切回到系统图标。**销毁旧主题的全部 HICON** —— 调用方负责清按句柄值
/// 当 key 的派生缓存（`IconCache::on_theme_changed`）。
///
/// `dark` 记录当前明暗，供优化套里需要随底色切换的细节点使用。
pub fn set_theme(theme: &str, dark: bool) {
    unsafe {
        let next = if theme == "optimized" {
            // 已知文件夹表：切到主题的第一帧就要用，先解析好。
            KNOWN.clear();
            use fm_core::paths::known_folder;
            let mut push = |name: &str, kind: Kind| {
                if let Some(p) = known_folder(name) {
                    KNOWN.push((norm_path(&p), kind));
                }
            };
            push("Desktop", Kind::Desktop);
            push("Downloads", Kind::Download);
            push("Documents", Kind::Documents);
            push("Pictures", Kind::Pictures);
            push("Music", Kind::Music);
            push("Videos", Kind::Videos);
            if let Ok(p) = std::env::var("USERPROFILE") {
                KNOWN.push((norm_path(&p), Kind::Home));
            }
            Some(Spec { dark })
        } else {
            KNOWN.clear();
            None
        };
        let unchanged = match (&SPEC, &next) {
            (None, None) => true,
            (Some(a), Some(b)) => a.dark == b.dark,
            _ => false,
        };
        if unchanged {
            return;
        }
        // 销毁旧主题图标（句柄值会被 GDI 复用 —— 调用方必须同时清派生缓存，
        // 见第39轮审计 #14 同款纪律）。
        for (_, _, h) in THEMED.drain(..) {
            if !h.0.is_null() {
                let _ = windows::Win32::UI::WindowsAndMessaging::DestroyIcon(h);
            }
        }
        for (_, _, _, h) in UI.drain(..) {
            if !h.0.is_null() {
                let _ = windows::Win32::UI::WindowsAndMessaging::DestroyIcon(h);
            }
        }
        SPEC = next;
    }
}

/// 主题是否接管（default 之外）。
pub fn active() -> bool {
    unsafe { SPEC.is_some() }
}

/// 取（或首次渲染）一枚主题图标。`active()` 为 false 时返回 None。
///
/// 由调用方负责缓存语义之外的销毁 —— 句柄归本模块的 THEMED 表持有。
pub fn get(kind: Kind, size: i32) -> Option<HICON> {
    unsafe {
        let spec = SPEC.as_ref()?;
        let size = size.clamp(16, 128);
        for (k, s, h) in THEMED.iter() {
            if *k == kind && *s == size {
                return Some(*h);
            }
        }
        let h = render(kind, spec.dark, size);
        if h.0.is_null() {
            return None;
        }
        THEMED.push((kind, size, h));
        Some(h)
    }
}

/// 系统库存图标 id → 自绘 Kind。不认识的 id 返回 None（调用方退回系统图标）。
pub fn stock_kind(siid: i32) -> Option<Kind> {
    use windows::Win32::UI::Shell::{
        SIID_DESKTOPPC, SIID_DRIVEFIXED, SIID_FOLDER, SIID_RECYCLER, SIID_RECYCLERFULL,
    };
    if siid == SIID_DESKTOPPC.0 {
        Some(Kind::ThisPc)
    } else if siid == SIID_RECYCLER.0 {
        Some(Kind::RecycleEmpty)
    } else if siid == SIID_RECYCLERFULL.0 {
        Some(Kind::RecycleFull)
    } else if siid == SIID_DRIVEFIXED.0 {
        Some(Kind::Drive)
    } else if siid == SIID_FOLDER.0 {
        Some(Kind::Folder)
    } else {
        None
    }
}

/// 目录路径 → 自绘 Kind（主题模式的 dir_icon 用）。
///
/// 顺序：盘符根 → 已知文件夹表（桌面/下载/文档/图片/音乐/视频/用户主目录）
/// → 通用文件夹。已知表在 `set_theme` 里解析一次，这里纯字符串比对。
pub fn dir_kind(path: &std::path::Path) -> Kind {
    let p = norm_path(&path.to_string_lossy());
    // 盘符根：norm_path 会吃掉尾反斜杠，"C:\" 归一化后就是 "c:"（2 字符）
    if p.len() == 2 && p.ends_with(':') {
        return Kind::Drive;
    }
    unsafe {
        for (kp, k) in KNOWN.iter() {
            if *kp == p {
                return *k;
            }
        }
    }
    Kind::Folder
}

// ---------------------------------------------------------------- UI 线形字形

/// 界面按钮字形（第49轮）。标题栏/目录栏/状态栏的按钮图标也归主题管：
/// 与文件图标同一套 SDF 渲染，但画的是**线条**字形，颜色由调用方给
/// （theme.text / text_dim），风格旋钮控制笔帽与圆角。
///
/// 默认（直角黑白）组合下 `ui_icon` 恒返回 None —— 调用方走原来的
/// Segoe MDL2 字体字形 / GDI 手绘，零回归。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Ui {
    /// ←（工具栏返回）
    Back,
    /// →（工具栏前进）
    Forward,
    /// ↑（工具栏上级）
    Up,
    /// 放大镜（搜索框）
    Search,
    /// 铅笔（批量重命名）
    Rename,
    /// ▼（下拉 / 树展开）
    ChevronDown,
    /// ▶（树收起）
    ChevronRight,
    /// 侧栏开关（方框 + 左填栏）
    PaneToggle,
    /// 主菜单（三条杠）
    Hamburger,
    /// 新建标签（十字）
    Plus,
    /// —（最小化）
    Min,
    /// □（最大化）
    Max,
    /// ⧉（还原）
    Restore,
    /// ×（关闭）
    Close,
    /// 状态栏：图标视图（2×2 空心格）
    ViewGrid,
    /// 状态栏：列表视图
    ViewList,
    /// 齿轮（设置标签）
    Gear,
}

/// UI 字形缓存：(字形, 边长, COLORREF) → HICON。句柄由本模块持有并在切主题时销毁。
static mut UI: Vec<(Ui, i32, u32, HICON)> = Vec::new();

/// 取一枚主题 UI 字形。主题未接管或渲染失败返回 None —— 调用方走原路径。
///
/// `size` 是成品边长（像素，含 DPI），内部 4× 超采样。
pub fn ui_icon(kind: Ui, size: i32, color: COLORREF) -> Option<HICON> {
    unsafe {
        if SPEC.is_none() {
            return None;
        }
        let size = size.clamp(10, 96);
        let ckey = color.0;
        for (k, s, c, h) in UI.iter() {
            if *k == kind && *s == size && *c == ckey {
                return Some(*h);
            }
        }
        let rgb = [
            (ckey & 0xFF) as u8,
            ((ckey >> 8) & 0xFF) as u8,
            ((ckey >> 16) & 0xFF) as u8,
        ];
        // 优化套的 UI 字形统一用圆头（圆角语言）。
        let h = render_ui(kind, rgb, size);
        if h.0.is_null() {
            return None;
        }
        if UI.len() > 512 {
            // 防呆上界：字形 × 尺寸 × 颜色 的组合数有限，超了整表清空重来
            for (_, _, _, h) in UI.drain(..) {
                if !h.0.is_null() {
                    let _ = windows::Win32::UI::WindowsAndMessaging::DestroyIcon(h);
                }
            }
        }
        UI.push((kind, size, ckey, h));
        Some(h)
    }
}

fn render_ui(kind: Ui, rgb: [u8; 3], size: i32) -> HICON {
    let mut cv = Cv::new(size);
    draw_ui(kind, &mut cv, rgb);
    bake(&cv, size)
}

/// 线段笔画 SDF（设计单位）。`round`=圆头（胶囊），否则平头（方帽矩形）。
///
/// 这是所有线条字形的唯一笔画原语：风格差异（圆头/平头）集中在这里，
/// 各字形本身不用关心风格。
fn sd_stroke(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32, hw: f32, round: bool) -> f32 {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let len2 = dx * dx + dy * dy;
    if len2 < 1e-6 {
        return ((px - x0).powi(2) + (py - y0).powi(2)).sqrt() - hw;
    }
    let len = len2.sqrt();
    let t = ((px - x0) * dx + (py - y0) * dy) / len2;
    if round {
        let tc = t.clamp(0.0, 1.0);
        let ex = px - (x0 + tc * dx);
        let ey = py - (y0 + tc * dy);
        (ex * ex + ey * ey).sqrt() - hw
    } else {
        // 平头帽：以线段中点为原点的局部 (perp, along) 坐标里做盒 SDF
        let cross = ((px - x0) * dy - (py - y0) * dx) / len;
        let along = t * len - len * 0.5;
        let qx = cross.abs() - hw;
        let qy = along.abs() - len * 0.5;
        let ox = qx.max(0.0);
        let oy = qy.max(0.0);
        (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0)
    }
}

fn bbox_of(pts: &[[f32; 2]], pad: f32) -> [f32; 4] {
    let mut x0 = f32::MAX;
    let mut y0 = f32::MAX;
    let mut x1 = f32::MIN;
    let mut y1 = f32::MIN;
    for p in pts {
        x0 = x0.min(p[0]);
        y0 = y0.min(p[1]);
        x1 = x1.max(p[0]);
        y1 = y1.max(p[1]);
    }
    [x0 - pad, y0 - pad, x1 + pad, y1 + pad]
}

impl Cv {
    /// 画一笔线段（bbox 自动包住笔画）。风格差异全在 `sd_stroke` 里。
    fn stroke(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, hw: f32, round: bool, col: [u8; 3]) {
        let m = hw + 1.0;
        let bbox = [x0.min(x1) - m, y0.min(y1) - m, x0.max(x1) + m, y0.max(y1) + m];
        self.fill(bbox, col, move |x, y| sd_stroke(x, y, x0, y0, x1, y1, hw, round));
    }

    /// 实心多边形（点按顺时针）。`round` 时把凸角倒圆（近似：点向重心收再外扩）。
    fn fill_poly(&mut self, pts: &[[f32; 2]], round: bool, col: [u8; 3]) {
        let r = if round { 2.6 } else { 0.0 };
        let n = pts.len();
        let mut cx = 0.0;
        let mut cy = 0.0;
        for p in pts {
            cx += p[0];
            cy += p[1];
        }
        cx /= n as f32;
        cy /= n as f32;
        let mut inset = [[0.0f32; 2]; 8];
        for (i, p) in pts.iter().enumerate() {
            inset[i] = [
                cx + (p[0] - cx) * (1.0 - r / 6.0),
                cy + (p[1] - cy) * (1.0 - r / 6.0),
            ];
        }
        let shrink = 1.0 - r / 6.0;
        let _ = shrink;
        self.fill(bbox_of(pts, r + 1.0), col, move |x, y| sd_poly(x, y, &inset[..n]) - r);
    }

    /// 圆角矩形**描边**（环）。
    fn ring_rrect(&mut self, x: f32, y: f32, w: f32, h: f32, hw: f32, r: f32, col: [u8; 3]) {
        self.fill([x - 1.0, y - 1.0, x + w + 1.0, y + h + 1.0], col, move |px, py| {
            sd_rrect(px, py, x, y, w, h, r).max(-sd_rrect(px, py, x + hw, y + hw, w - 2.0 * hw, h - 2.0 * hw, (r - hw).max(0.2)))
        });
    }

    /// 圆形描边（环）。
    fn ring_circle(&mut self, cx: f32, cy: f32, rad: f32, hw: f32, col: [u8; 3]) {
        self.fill([cx - rad - 1.0, cy - rad - 1.0, cx + rad + 1.0, cy + rad + 1.0], col, move |x, y| {
            sd_ellipse(x, y, cx, cy, rad, rad).abs() - hw
        });
    }
}

/// 画一枚 UI 字形（32 单位设计网格）。
fn draw_ui(kind: Ui, cv: &mut Cv, col: [u8; 3]) {
    // 优化套 UI 字形统一圆头（圆角语言）。
    let round = true;
    let hw = 2.0;
    let cap = true;
    match kind {
        Ui::Back => {
            cv.stroke(12.0, 16.0, 26.0, 16.0, hw, cap, col);
            cv.fill_poly(&[[6.0, 16.0], [16.0, 9.5], [16.0, 22.5]], round, col);
        }
        Ui::Forward => {
            cv.stroke(6.0, 16.0, 20.0, 16.0, hw, cap, col);
            // 三角必须顺时针（sd_poly 的内部判据），反了整个箭头就只剩杆
            cv.fill_poly(&[[26.0, 16.0], [16.0, 22.5], [16.0, 9.5]], round, col);
        }
        Ui::Up => {
            cv.stroke(16.0, 26.0, 16.0, 14.0, hw, cap, col);
            cv.fill_poly(&[[16.0, 6.0], [22.5, 16.0], [9.5, 16.0]], round, col);
        }
        Ui::Search => {
            // 圆环 + 斜手柄（原 GDI 画法的同一构图）
            cv.ring_circle(13.5, 13.5, 8.0, hw, col);
            cv.stroke(19.5, 19.5, 26.5, 26.5, hw * 1.15, cap, col);
        }
        Ui::Rename => {
            // 铅笔：粗斜杆 + 左下笔尖三角
            cv.stroke(10.5, 21.5, 22.0, 10.0, 2.6, cap, col);
            cv.fill_poly(&[[4.5, 27.5], [8.5, 18.0], [16.5, 24.5]], round, col);
        }
        Ui::ChevronDown => {
            cv.fill_poly(&[[8.0, 12.5], [24.0, 12.5], [16.0, 21.5]], round, col);
        }
        Ui::ChevronRight => {
            cv.fill_poly(&[[12.5, 8.0], [21.5, 16.0], [12.5, 24.0]], round, col);
        }
        Ui::PaneToggle => {
            let r = if round { 4.0 } else { 0.0 };
            cv.ring_rrect(4.0, 8.5, 24.0, 15.0, hw, r, col);
            // 左侧填实的一栏："被收起的那一栏"
            let r2 = if round { 2.5 } else { 0.0 };
            cv.fill([6.0, 10.5, 13.5, 21.5], col, move |x, y| sd_rrect(x, y, 6.5, 11.0, 6.0, 10.0, r2));
        }
        Ui::Hamburger => {
            for y in [10.5f32, 16.0, 21.5] {
                cv.stroke(8.0, y, 24.0, y, hw * 0.85, cap, col);
            }
        }
        Ui::Plus => {
            cv.stroke(16.0, 8.0, 16.0, 24.0, hw, cap, col);
            cv.stroke(8.0, 16.0, 24.0, 16.0, hw, cap, col);
        }
        Ui::Min => {
            // 与原 GDI 版同一位置约定：横线压在几何中心之下一点，不"贴地"
            cv.stroke(9.0, 19.5, 23.0, 19.5, hw, cap, col);
        }
        Ui::Max => {
            let r = if round { 4.0 } else { 0.0 };
            cv.ring_rrect(8.0, 8.0, 16.0, 16.0, hw, r, col);
        }
        Ui::Restore => {
            // 后框只画上边 + 右边（与原 GDI 版同构），前框完整
            let r = if round { 3.5 } else { 0.0 };
            cv.stroke(12.5, 7.0, 26.0, 7.0, hw, cap, col);
            cv.stroke(26.0, 7.0, 26.0, 20.5, hw, cap, col);
            cv.ring_rrect(6.0, 12.0, 14.5, 14.5, hw, r, col);
        }
        Ui::Close => {
            cv.stroke(9.5, 9.5, 22.5, 22.5, hw, cap, col);
            cv.stroke(22.5, 9.5, 9.5, 22.5, hw, cap, col);
        }
        Ui::ViewGrid => {
            // 2×2 空心格（照原 GDI 版：空心比实心更像"图标视图"）
            let r = if round { 3.0 } else { 0.0 };
            let t = hw * 0.85;
            for (gx, gy) in [(5.0f32, 5.0f32), (18.0, 5.0), (5.0, 18.0), (18.0, 18.0)] {
                cv.ring_rrect(gx, gy, 9.0, 9.0, t, r, col);
            }
        }
        Ui::ViewList => {
            // 表头条 + 三行 + 一条列分隔线（照原 GDI 版构图）
            cv.fill([5.0, 6.0, 27.0, 10.0], col, |x, y| {
                sd_rrect(x, y, 5.0, 6.0, 22.0, 4.0, if round { 1.8 } else { 0.3 })
            });
            for y in [14.5f32, 19.5, 24.5] {
                cv.stroke(5.0, y, 27.0, y, hw * 0.75, cap, col);
            }
            cv.stroke(19.0, 13.0, 19.0, 26.0, hw * 0.6, cap, col);
        }
        Ui::Gear => {
            // 齿轮：环 + 6 枚径向齿 + 中心轴
            cv.ring_circle(16.0, 16.0, 9.0, hw * 1.15, col);
            for k in 0..6 {
                let a = (k as f32) * std::f32::consts::PI / 3.0;
                let (s, c) = a.sin_cos();
                cv.stroke(
                    16.0 + 8.5 * c,
                    16.0 + 8.5 * s,
                    16.0 + 12.5 * c,
                    16.0 + 12.5 * s,
                    hw * 1.05,
                    cap,
                    col,
                );
            }
            cv.fill([12.5, 12.5, 19.5, 19.5], col, |x, y| sd_ellipse(x, y, 16.0, 16.0, 3.4, 3.4));
        }
    }
}

/// 按扩展名归类（文件图标入口用）。未识别 → Generic。
pub fn ext_kind(ext: &str) -> Kind {
    let e = ext.trim_start_matches('.').to_ascii_lowercase();
    match e.as_str() {
        "txt" | "log" | "md" | "rtf" | "text" | "ini" => Kind::Txt,
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "ico" | "tif" | "tiff"
        | "heic" | "avif" => Kind::ImageFile,
        "mp4" | "avi" | "mkv" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "ts" => Kind::VideoFile,
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "wma" | "aac" | "ape" => Kind::AudioFile,
        "exe" | "msi" | "appx" => Kind::Exe,
        "pdf" => Kind::Pdf,
        "zip" | "7z" | "rar" | "tar" | "gz" | "bz2" | "xz" | "iso" => Kind::Archive,
        "rs" | "py" | "js" | "ts" | "jsx" | "tsx" | "cpp" | "cc" | "c" | "h" | "hpp" | "java"
        | "go" | "kt" | "swift" | "rb" | "php" | "html" | "css" | "scss" | "vue" | "json"
        | "xml" | "yaml" | "yml" | "toml" | "sh" | "bat" | "ps1" | "sql" | "lua" | "pl" => Kind::Code,
        "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "csv" => Kind::Documents,
        _ => Kind::Generic,
    }
}

// ---------------------------------------------------------------- 调色板

struct Pal {
    /// 主体形状
    body: [u8; 3],
    /// 次级/背板/描边面
    body2: [u8; 3],
    /// 画在主体上的深色细节（线条/字迹）
    ink: [u8; 3],
    /// 更柔的线
    line: [u8; 3],
    /// 强调色（画在深色面上的亮件）
    acc: [u8; 3],
}

fn pal_for(kind: Kind, dark: bool) -> Pal {
    // 优化套统一彩色和谐配色（暗/亮主题通用的扁平色，靠 body2 做层次）。
    // `dark` 预留给未来随底色微调的细节点（当前未使用，避免两套调色板漂移）。
    let _ = dark;
    match kind {
        Kind::Folder => Pal { body: [255, 196, 84], body2: [244, 173, 54], ink: [255, 255, 255], line: [214, 150, 46], acc: [255, 232, 150] },
        Kind::Drive => Pal { body: [126, 140, 156], body2: [98, 112, 128], ink: [70, 82, 96], line: [84, 96, 110], acc: [80, 220, 140] },
        Kind::ThisPc | Kind::Desktop => Pal { body: [78, 90, 106], body2: [98, 112, 130], ink: [255, 255, 255], line: [70, 82, 96], acc: [120, 174, 246] },
        Kind::RecycleEmpty | Kind::RecycleFull => Pal { body: [150, 178, 202], body2: [120, 150, 175], ink: [60, 84, 104], line: [110, 140, 165], acc: [236, 240, 244] },
        Kind::Home => Pal { body: [255, 222, 176], body2: [232, 150, 92], ink: [196, 120, 70], line: [210, 170, 130], acc: [255, 255, 255] },
        Kind::Download => Pal { body: [86, 150, 240], body2: [70, 132, 222], ink: [255, 255, 255], line: [70, 132, 222], acc: [255, 255, 255] },
        Kind::Documents | Kind::Txt => Pal { body: [255, 255, 255], body2: [231, 234, 239], ink: [150, 158, 170], line: [196, 202, 212], acc: [86, 150, 240] },
        Kind::Pictures | Kind::ImageFile => Pal { body: [231, 234, 240], body2: [208, 212, 221], ink: [120, 180, 230], line: [196, 202, 212], acc: [90, 196, 140] },
        Kind::Music => Pal { body: [255, 255, 255], body2: [231, 234, 239], ink: [150, 120, 245], line: [196, 202, 212], acc: [150, 120, 245] },
        Kind::Videos | Kind::VideoFile => Pal { body: [78, 90, 106], body2: [98, 112, 130], ink: [255, 255, 255], line: [70, 82, 96], acc: [236, 238, 242] },
        Kind::AudioFile => Pal { body: [255, 255, 255], body2: [231, 234, 239], ink: [240, 130, 175], line: [196, 202, 212], acc: [240, 130, 175] },
        Kind::Exe => Pal { body: [86, 150, 240], body2: [70, 132, 222], ink: [255, 255, 255], line: [196, 202, 212], acc: [255, 255, 255] },
        Kind::Pdf => Pal { body: [255, 255, 255], body2: [231, 234, 239], ink: [228, 82, 82], line: [196, 202, 212], acc: [255, 255, 255] },
        Kind::Archive => Pal { body: [214, 168, 72], body2: [186, 142, 56], ink: [120, 92, 38], line: [158, 120, 50], acc: [255, 236, 170] },
        Kind::Code => Pal { body: [255, 255, 255], body2: [231, 234, 239], ink: [80, 200, 130], line: [196, 202, 212], acc: [80, 200, 130] },
        Kind::Generic => Pal { body: [255, 255, 255], body2: [231, 234, 239], ink: [150, 158, 170], line: [196, 202, 212], acc: [150, 158, 170] },
    }
}

// ---------------------------------------------------------------- SDF 图元

/// 圆角矩形 SDF。(x, y, w, h) 是**左上角 + 宽高**（不是中心点）。
#[inline]
fn sd_rrect(px: f32, py: f32, x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let r = r.min(w / 2.0).min(h / 2.0);
    let qx = (px - (x + w / 2.0)).abs() - (w / 2.0 - r);
    let qy = (py - (y + h / 2.0)).abs() - (h / 2.0 - r);
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

#[inline]
fn sd_ellipse(px: f32, py: f32, cx: f32, cy: f32, rx: f32, ry: f32) -> f32 {
    let nx = (px - cx) / rx;
    let ny = (py - cy) / ry;
    let k = (nx * nx + ny * ny).sqrt();
    if k < 1e-6 {
        -rx.min(ry)
    } else {
        (k - 1.0) * rx.min(ry)
    }
}

#[inline]
fn sd_seg(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> f32 {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let l2 = dx * dx + dy * dy;
    let t = if l2 < 1e-6 {
        0.0
    } else {
        (((px - x0) * dx + (py - y0) * dy) / l2).clamp(0.0, 1.0)
    };
    let ex = px - (x0 + t * dx);
    let ey = py - (y0 + t * dy);
    (ex * ex + ey * ey).sqrt() - r
}

/// 凸多边形 SDF（半平面最大值）。点按顺时针给。
#[inline]
fn sd_poly(px: f32, py: f32, pts: &[[f32; 2]]) -> f32 {
    let n = pts.len();
    let mut d = f32::MIN;
    for i in 0..n {
        let a = pts[i];
        let b = pts[(i + 1) % n];
        let ex = b[0] - a[0];
        let ey = b[1] - a[1];
        let wx = px - a[0];
        let wy = py - a[1];
        let cross = ex * wy - ey * wx;
        let len = (ex * ex + ey * ey).sqrt().max(1e-6);
        // 顺时针点序下 interior 在 cross < 一侧；取 max 归一化有符号距离
        d = d.max(-cross / len);
    }
    d
}

// ---------------------------------------------------------------- 画布

struct Cv {
    w: i32,
    /// 每设计单位对应的像素数（4× 超采样）
    u: f32,
    /// RGBA straight alpha
    buf: Vec<u8>,
}

impl Cv {
    fn new(size: i32) -> Self {
        let w = size * SS;
        Cv { w, u: w as f32 / 32.0, buf: vec![0u8; (w * w * 4) as usize] }
    }

    /// 用闭包给的 SDF（设计单位）填充一个颜色。bbox 也是设计单位。
    fn fill<F: Fn(f32, f32) -> f32>(&mut self, bbox: [f32; 4], col: [u8; 3], f: F) {
        let u = self.u;
        let x0 = ((bbox[0] * u).floor() as i32).max(0);
        let y0 = ((bbox[1] * u).floor() as i32).max(0);
        let x1 = ((bbox[2] * u).ceil() as i32).min(self.w);
        let y1 = ((bbox[3] * u).ceil() as i32).min(self.w);
        for y in y0..y1 {
            for x in x0..x1 {
                let dx = (x as f32 + 0.5) / u;
                let dy = (y as f32 + 0.5) / u;
                let cov = (0.5 - f(dx, dy)).clamp(0.0, 1.0);
                if cov <= 0.0 {
                    continue;
                }
                let i = ((y * self.w + x) * 4) as usize;
                let sa = cov; // 源不透明，覆盖即 alpha
                let da = self.buf[i + 3] as f32 / 255.0;
                let oa = sa + da * (1.0 - sa);
                if oa <= 0.0 {
                    continue;
                }
                for c in 0..3 {
                    let sc = col[c] as f32 / 255.0 * sa;
                    let dc = self.buf[i + c] as f32 / 255.0 * da;
                    let v = (sc + dc * (1.0 - sa)) / oa;
                    self.buf[i + c] = (v * 255.0).round().clamp(0.0, 255.0) as u8;
                }
                self.buf[i + 3] = (oa * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
    }

    /// 折角文档页：圆角矩形**右上斜切** + 深一档的折边三角。
    fn fill_page(&mut self, p: &Pal, r: f32) {
        let body = p.body;
        let fold = p.body2;
        // 切角三角与折边都按 y-down 顺时针（正 shoelace）给点 —— sd_poly 是
        // 半平面最大值，绕向反了整个形状会翻成"外面"。
        self.fill([6.0, 3.0, 26.0, 29.0], body, move |x, y| {
            sd_rrect(x, y, 6.5, 3.5, 19.0, 25.0, r)
                .max(-sd_poly(x, y, &[[16.0, 3.5], [25.5, 3.5], [25.5, 13.0]]))
        });
        // 折角翻边：贴着斜切线内侧的深色三角
        self.fill([16.0, 3.0, 26.0, 13.0], fold, move |x, y| {
            sd_poly(x, y, &[[17.5, 4.0], [25.0, 11.5], [17.5, 11.5]])
        });
    }
}

const SS: i32 = 4;

// ---------------------------------------------------------------- 各图标绘制

/// 绘制一枚优化的自绘图标（32 单位设计网格）。
///
/// `dark` 预留给未来随底色微调的细节点（当前所有图形对明暗底色都自适应：
/// 主体用中明度彩色、细节用对比色，不依赖背景）。
fn draw(kind: Kind, cv: &mut Cv, dark: bool) {
    let _ = dark;
    let p = pal_for(kind, dark);
    let (b, b2, ink, line, acc) = (p.body, p.body2, p.ink, p.line, p.acc);
    let r = 4.0; // 主圆角
    let r2 = 3.0; // 次级圆角

    match kind {
        Kind::Folder => {
            // 背片（标签）+ 前片主体：经典文件夹剪影
            cv.fill([3.0, 4.5, 13.5, 11.0], b2, |x, y| sd_rrect(x, y, 4.0, 5.0, 11.0, 6.0, 2.5));
            cv.fill([2.5, 9.0, 29.5, 28.0], b, |x, y| sd_rrect(x, y, 3.0, 9.5, 26.5, 18.0, r));
        }
        Kind::Drive => {
            // 硬盘：盘体 + 盘片 + 轴心 + LED + 标签
            cv.fill([2.5, 8.5, 29.5, 23.5], b, |x, y| sd_rrect(x, y, 3.0, 9.0, 26.5, 14.0, r));
            cv.fill([7.0, 11.5, 25.0, 20.5], b2, |x, y| sd_ellipse(x, y, 16.0, 16.0, 8.5, 4.5));
            cv.fill([13.5, 14.0, 18.5, 18.0], line, |x, y| sd_ellipse(x, y, 16.0, 16.0, 2.5, 2.5));
            cv.fill([24.0, 10.5, 27.0, 13.5], acc, |x, y| sd_ellipse(x, y, 25.5, 12.0, 1.4, 1.4));
            cv.fill([21.0, 18.5, 27.0, 21.0], b2, |x, y| sd_rrect(x, y, 21.5, 19.0, 5.0, 2.0, 0.8));
        }
        Kind::ThisPc => {
            // 显示器：屏 + 内屏 + 颈 + 底座
            cv.fill([4.0, 4.0, 28.0, 20.0], b, |x, y| sd_rrect(x, y, 4.5, 4.5, 23.5, 15.0, r));
            cv.fill([6.5, 6.5, 25.5, 17.5], acc, |x, y| sd_rrect(x, y, 7.0, 7.0, 18.5, 10.0, r2));
            cv.fill([14.0, 19.5, 18.0, 23.5], b2, |x, y| sd_rrect(x, y, 14.5, 20.0, 3.0, 3.5, 0.8));
            cv.fill([10.0, 23.5, 22.0, 27.0], b2, |x, y| sd_rrect(x, y, 10.5, 24.0, 11.0, 3.0, 1.6));
        }
        Kind::RecycleEmpty | Kind::RecycleFull => {
            if kind == Kind::RecycleFull {
                cv.fill([9.0, 1.5, 24.0, 9.0], acc, |x, y| {
                    sd_ellipse(x, y, 12.5, 6.5, 2.6, 2.3)
                        .min(sd_ellipse(x, y, 17.0, 4.8, 3.0, 2.6))
                        .min(sd_ellipse(x, y, 21.0, 6.6, 2.3, 2.1))
                });
            }
            cv.fill([12.0, 3.0, 20.0, 6.5], b2, |x, y| sd_rrect(x, y, 12.5, 3.5, 7.0, 2.8, 1.2));
            cv.fill([5.5, 6.0, 26.5, 11.0], b2, |x, y| sd_rrect(x, y, 6.0, 6.5, 19.5, 4.2, r2));
            cv.fill([8.0, 10.0, 24.0, 29.0], b, |x, y| {
                sd_poly(x, y, &[[9.0, 10.5], [23.0, 10.5], [21.0, 28.0], [11.0, 28.0]])
            });
            for gx in [13.0f32, 16.0, 19.0] {
                cv.fill([gx - 1.0, 12.5, gx + 1.0, 27.0], line, move |x, y| {
                    sd_seg(x, y, gx, 13.0, gx, 26.0, 0.5)
                });
            }
        }
        Kind::Home => {
            cv.fill([2.5, 13.5, 29.5, 28.5], b2, move |x, y| {
                sd_poly(x, y, &[[16.0, 3.5], [29.5, 14.5], [2.5, 14.5]])
            });
            cv.fill([5.0, 12.5, 27.0, 28.5], b, |x, y| sd_rrect(x, y, 5.0, 12.5, 22.0, 16.0, r2));
            cv.fill([12.0, 17.5, 20.0, 28.5], ink, |x, y| sd_rrect(x, y, 12.5, 18.0, 7.0, 10.0, 1.4));
        }
        Kind::Desktop => {
            cv.fill([3.5, 5.0, 28.5, 27.0], b, |x, y| sd_rrect(x, y, 4.0, 5.5, 24.5, 21.0, r));
            cv.fill([4.5, 6.0, 27.5, 11.0], b2, |x, y| sd_rrect(x, y, 5.0, 6.5, 22.5, 4.5, r2));
            cv.fill([25.0, 8.0, 27.0, 10.0], acc, |x, y| sd_ellipse(x, y, 26.0, 9.0, 1.1, 1.1));
            cv.fill([8.0, 14.0, 24.0, 16.0], acc, move |x, y| sd_rrect(x, y, 8.5, 14.5, 15.0, 1.4, 0.7));
            cv.fill([8.0, 19.0, 20.0, 21.0], acc, move |x, y| sd_rrect(x, y, 8.5, 19.5, 11.0, 1.4, 0.7));
        }
        Kind::Download => {
            // 向下箭头（白）+ 托盘（蓝）
            cv.fill([9.0, 4.0, 23.0, 13.0], acc, move |x, y| {
                sd_poly(x, y, &[[16.0, 4.0], [22.0, 12.0], [10.0, 12.0]])
            });
            cv.fill([14.0, 11.0, 18.0, 23.0], acc, move |x, y| sd_seg(x, y, 16.0, 11.0, 16.0, 22.0, 2.2));
            cv.fill([3.0, 21.0, 29.0, 28.5], b, |x, y| sd_rrect(x, y, 3.5, 21.5, 25.5, 6.5, r));
        }
        Kind::Documents | Kind::Txt | Kind::Generic => {
            let lines_n = if kind == Kind::Txt { 4 } else if kind == Kind::Documents { 3 } else { 0 };
            cv.fill_page(&p, r);
            for i in 0..lines_n {
                let y = 14.0 + i as f32 * 3.6;
                let x1 = if i == lines_n - 1 { 19.0 } else { 21.5 };
                cv.fill([10.0, y - 1.0, x1 + 1.0, y + 1.0], ink, move |x, yy| {
                    sd_seg(x, yy, 11.0, y, x1, y, 0.62)
                });
            }
        }
        Kind::Pictures | Kind::ImageFile => {
            if kind == Kind::Pictures {
                cv.fill([3.0, 5.0, 29.0, 27.0], b, |x, y| sd_rrect(x, y, 3.5, 5.5, 25.5, 21.0, r));
            } else {
                cv.fill_page(&p, r);
            }
            let (fx, fy, fw, fh) = if kind == Kind::Pictures {
                (5.5f32, 7.5, 21.0, 15.0)
            } else {
                (9.5, 13.0, 13.0, 9.5)
            };
            let sky = [150u8, 200, 235];
            let mount = [90u8, 196, 140];
            let sun = [255u8, 210, 90];
            let (x0, y0, x1, y1) = (fx, fy, fx + fw, fy + fh);
            cv.fill([x0, y0, x1, y1], sky, |x, y| {
                sd_rrect(x, y, x0, y0, x1 - x0, y1 - y0, if kind == Kind::Pictures { 2.5 } else { 1.2 })
            });
            let base = y1 - 0.5;
            let h = (y1 - y0) * 0.6;
            let midx = (x0 + x1) / 2.0;
            cv.fill([x0, y0, x1, y1], mount, move |x, y| {
                sd_poly(x, y, &[[x0 + 1.0, base], [x0 + (x1 - x0) * 0.4, base - h], [x0 + (x1 - x0) * 0.68, base]])
                    .min(sd_poly(x, y, &[[midx + 1.0, base], [midx + (x1 - x0) * 0.24, base - h * 0.7], [x1 - 1.0, base]]))
            });
            let sx = x1 - (x1 - x0) * 0.22;
            let sy = y0 + (y1 - y0) * 0.26;
            let sr = ((x1 - x0) * 0.1).max(1.4);
            cv.fill([sx - sr - 0.5, sy - sr - 0.5, sx + sr + 0.5, sy + sr + 0.5], sun, move |x, y| {
                sd_ellipse(x, y, sx, sy, sr, sr)
            });
        }
        Kind::Music => {
            // 双音符（杆 + 横梁 + 两符头）
            cv.fill([12.5, 5.5, 15.0, 22.0], ink, move |x, y| sd_seg(x, y, 13.8, 6.0, 13.8, 21.5, 1.3));
            cv.fill([20.5, 3.5, 23.0, 20.0], ink, move |x, y| sd_seg(x, y, 21.8, 4.0, 21.8, 19.5, 1.3));
            cv.fill([12.5, 4.5, 23.0, 7.0], ink, move |x, y| sd_seg(x, y, 13.8, 5.5, 21.8, 3.5, 1.5));
            cv.fill([9.5, 20.5, 14.5, 25.5], ink, |x, y| sd_ellipse(x, y, 11.5, 22.5, 3.0, 2.3));
            cv.fill([18.5, 18.5, 23.5, 23.5], ink, |x, y| sd_ellipse(x, y, 20.5, 20.5, 3.0, 2.3));
        }
        Kind::Videos | Kind::VideoFile => {
            if kind == Kind::Videos {
                cv.fill([2.5, 7.0, 29.5, 25.0], b, |x, y| sd_rrect(x, y, 3.0, 7.5, 26.5, 17.0, r));
                // 三角必须顺时针（正 shoelace），反了整个形状会翻成"外面"（第51轮实测）
                cv.fill([11.0, 10.5, 23.0, 21.5], acc, move |x, y| sd_poly(x, y, &[[13.0, 20.5], [13.0, 11.5], [22.0, 16.0]]));
            } else {
                cv.fill_page(&p, r);
                cv.fill([9.5, 12.5, 22.5, 24.5], acc, move |x, y| sd_poly(x, y, &[[12.0, 23.0], [12.0, 14.0], [21.0, 18.5]]));
            }
        }
        Kind::AudioFile => {
            cv.fill_page(&p, r);
            for (bx, by0, by1) in [(11.0f32, 18.0, 24.0), (15.0, 14.0, 24.0), (19.0, 16.0, 24.0)] {
                cv.fill([bx - 1.0, by0, bx + 1.0, by1], acc, move |x, y| sd_rrect(x, y, bx - 1.0, by0, 2.0, by1 - by0, 1.0));
            }
        }
        Kind::Exe => {
            cv.fill([4.0, 4.0, 28.0, 28.0], b, |x, y| sd_rrect(x, y, 4.5, 4.5, 23.5, 23.5, r));
            // 白齿轮
            cv.fill([5.0, 5.0, 27.0, 27.0], acc, |x, y| {
                sd_ellipse(x, y, 16.0, 16.0, 9.5, 9.5).max(-sd_ellipse(x, y, 16.0, 16.0, 6.0, 6.0))
            });
            for k in 0..8 {
                let a = (k as f32) * std::f32::consts::PI / 4.0;
                let (s, c) = a.sin_cos();
                cv.fill([4.0, 4.0, 28.0, 28.0], acc, move |x, y| {
                    sd_seg(x, y, 16.0 + 8.5 * c, 16.0 + 8.5 * s, 16.0 + 12.0 * c, 16.0 + 12.0 * s, 1.5)
                });
            }
            cv.fill([12.5, 12.5, 19.5, 19.5], b, |x, y| sd_ellipse(x, y, 16.0, 16.0, 3.0, 3.0));
        }
        Kind::Pdf => {
            cv.fill_page(&p, r);
            cv.fill([8.5, 17.0, 23.5, 23.5], ink, |x, y| sd_rrect(x, y, 9.0, 17.5, 14.0, 5.5, 1.4));
            cv.fill([11.0, 19.0, 21.0, 20.4], acc, |x, y| sd_rrect(x, y, 11.2, 19.1, 9.6, 1.2, 0.5));
            cv.fill([11.0, 21.0, 17.5, 22.4], acc, |x, y| sd_rrect(x, y, 11.2, 21.1, 6.0, 1.2, 0.5));
        }
        Kind::Archive => {
            cv.fill([4.0, 9.0, 28.0, 27.0], b, |x, y| sd_rrect(x, y, 4.5, 9.5, 23.0, 17.0, r));
            cv.fill([3.0, 6.0, 29.0, 11.5], b2, |x, y| sd_rrect(x, y, 3.5, 6.5, 25.0, 4.5, r2));
            cv.fill([4.5, 16.0, 27.5, 18.5], line, |x, y| sd_rrect(x, y, 5.0, 16.2, 22.0, 2.0, 0.6));
            cv.fill([14.0, 9.5, 18.0, 27.0], line, |x, y| sd_rrect(x, y, 14.5, 9.5, 3.0, 17.0, 0.6));
        }
        Kind::Code => {
            cv.fill_page(&p, r);
            // 绿色 </>
            cv.fill([8.0, 12.0, 24.0, 23.0], ink, |x, y| {
                sd_seg(x, y, 13.0, 14.5, 10.2, 17.5, 1.0)
                    .min(sd_seg(x, y, 10.2, 17.5, 13.0, 20.5, 1.0))
                    .min(sd_seg(x, y, 19.0, 14.5, 21.8, 17.5, 1.0))
                    .min(sd_seg(x, y, 21.8, 17.5, 19.0, 20.5, 1.0))
                    .min(sd_seg(x, y, 15.0, 13.6, 17.6, 21.4, 0.9))
            });
        }
    }
}

// ---------------------------------------------------------------- 渲染出口

/// 渲染一枚图标成 HICON。`size` 是成品边长（16/32…），内部 4× 超采样。
///
/// 返回的 HICON 由调用方负责 `DestroyIcon`（`IconCache` 统一持有）。
pub fn render(kind: Kind, dark: bool, size: i32) -> HICON {
    let size = size.clamp(16, 256);
    let mut cv = Cv::new(size);
    draw(kind, &mut cv, dark);
    bake(&cv, size)
}

/// 画布 → HICON：4× 超采样缓冲 box 降采样成 premultiplied BGRA，
/// 再包一层全 0 掩码的 32bpp 图标。文件图标与 UI 字形共用这一段。
fn bake(cv: &Cv, size: i32) -> HICON {
    let ss = SS as usize;
    let out = size as usize;
    // box 降采样 → premultiplied BGRA
    let mut pmp = vec![0u8; out * out * 4];
    for oy in 0..out {
        for ox in 0..out {
            let (mut ar, mut ag, mut ab, mut aa) = (0f32, 0f32, 0f32, 0f32);
            for sy in 0..ss {
                for sx in 0..ss {
                    let i = (((oy * ss + sy) as i32 * cv.w + (ox * ss + sx) as i32) * 4) as usize;
                    let a = cv.buf[i + 3] as f32 / 255.0;
                    ar += cv.buf[i] as f32 * a;
                    ag += cv.buf[i + 1] as f32 * a;
                    ab += cv.buf[i + 2] as f32 * a;
                    aa += a;
                }
            }
            let n = (ss * ss) as f32;
            let o = (oy * out + ox) * 4;
            pmp[o] = (ar / n).round().clamp(0.0, 255.0) as u8;
            pmp[o + 1] = (ag / n).round().clamp(0.0, 255.0) as u8;
            pmp[o + 2] = (ab / n).round().clamp(0.0, 255.0) as u8;
            pmp[o + 3] = (aa / n * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }

    unsafe {
        // 颜色层：32bpp DIB（BGRA，premultiplied）
        let mut hdr: BITMAPINFO = std::mem::zeroed();
        hdr.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        hdr.bmiHeader.biWidth = size;
        hdr.bmiHeader.biHeight = -size; // 自上而下
        hdr.bmiHeader.biPlanes = 1;
        hdr.bmiHeader.biBitCount = 32;
        hdr.bmiHeader.biCompression = BI_RGB.0;
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(None, &hdr, DIB_RGB_COLORS, &mut bits, None, 0)
            .unwrap_or_default();
        if dib.is_invalid() || bits.is_null() {
            return HICON::default();
        }
        // BGRA 排布
        let dst = std::slice::from_raw_parts_mut(bits as *mut u8, pmp.len());
        for px in 0..out * out {
            let s = px * 4;
            dst[s] = pmp[s + 2];
            dst[s + 1] = pmp[s + 1];
            dst[s + 2] = pmp[s];
            dst[s + 3] = pmp[s + 3];
        }
        // 掩码层：1bpp 全 0（= 全部走颜色层 alpha）
        let stride = ((size + 31) / 32) * 4;
        let zeros = vec![0u8; (stride * size) as usize];
        let mask = CreateBitmap(size, size, 1, 1, Some(zeros.as_ptr() as *const core::ffi::c_void));
        if mask.is_invalid() {
            let _ = DeleteObject(dib.into());
            return HICON::default();
        }
        let info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: dib,
        };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(dib.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}
