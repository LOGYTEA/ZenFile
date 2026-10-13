//! 自绘图标主题（第48轮）。
//!
//! 用户要求设置-外观里可切换 4 套图标 = 风格(直角/圆角) × 颜色(黑白/彩色)：
//!   - **直角黑白 = 现在这套系统 Shell 图标**（默认，零回归 —— 本模块不接管）；
//!   - 其余三套（直角彩色 / 圆角黑白 / 圆角彩色）由本模块参数化自绘。
//!
//! 实现：SDF（有符号距离场）软渲染到 4× 超采样缓冲，box 降采样出抗锯齿，
//! premultiplied BGRA → `CreateIconIndirect` 出 HICON。所有图形定义在 32 单位
//! 设计网格上，风格只改圆角半径、配色只换调色板 —— 一套几何画出三套。
//!
//! 纪律：
//!   - 图标由本模块渲染，调用方（`IconCache`）负责缓存与销毁；
//!   - 直角黑白组合**绝不能**落到本模块（is_default_combo 的调用方守卫）。

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

/// 直角黑白 = 现在这套（系统图标），本模块不接管。
pub fn is_default_combo(style: &str, tone: &str) -> bool {
    style != "round" && tone != "color"
}

// ---------------------------------------------------------------- 全局主题状态

struct Spec {
    round: bool,
    color: bool,
    dark: bool,
}

/// 当前生效的主题。`None` = 直角黑白 = 系统图标（默认）。
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

/// 应用主题。`style`/`tone` 是配置值；combo 为默认（直角黑白）时清空状态，
/// 一切回到系统图标。**销毁旧主题的全部 HICON** —— 调用方负责清按句柄值
/// 当 key 的派生缓存（`IconCache::on_theme_changed`）。
///
/// `dark` 只影响黑白套（暗色主题配浅墨、亮色主题配深墨），彩色套通用。
pub fn set_theme(style: &str, tone: &str, dark: bool) {
    unsafe {
        // 已知文件夹表：任何主题都要（系统图标模式下 dir_icon 不走这里，
        // 但切到主题的第一帧就要用，先解析好）。
        KNOWN.clear();
        if !is_default_combo(style, tone) {
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
        }

        let next =
            if is_default_combo(style, tone) { None } else { Some(Spec { round: style == "round", color: tone == "color", dark }) };
        let unchanged = match (&SPEC, &next) {
            (None, None) => true,
            (Some(a), Some(b)) => a.round == b.round && a.color == b.color && a.dark == b.dark,
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
        SPEC = next;
    }
}

/// 主题是否接管（非默认 combo）。
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
        let h = render(kind, spec.round, spec.color, spec.dark, size);
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

fn pal_for(kind: Kind, color: bool, dark: bool) -> Pal {
    if !color {
        // 黑白：暗色主题画浅形深线，亮色主题反之 —— 与系统图标一样自适应底色。
        return if dark {
            Pal {
                body: [235, 235, 235],
                body2: [150, 150, 150],
                ink: [40, 40, 40],
                line: [110, 110, 110],
                acc: [255, 255, 255],
            }
        } else {
            Pal {
                body: [88, 88, 88],
                body2: [148, 148, 148],
                ink: [245, 245, 245],
                line: [210, 210, 210],
                acc: [60, 60, 60],
            }
        };
    }
    // 彩色（暗亮主题通用的扁平配色）
    match kind {
        Kind::Folder => Pal { body: [247, 200, 67], body2: [217, 164, 40], ink: [146, 106, 20], line: [180, 138, 34], acc: [255, 224, 120] },
        Kind::Drive => Pal { body: [154, 166, 178], body2: [116, 128, 142], ink: [66, 76, 88], line: [88, 98, 110], acc: [61, 220, 132] },
        Kind::ThisPc | Kind::Desktop => Pal { body: [46, 58, 71], body2: [107, 119, 133], ink: [30, 40, 52], line: [70, 82, 96], acc: [76, 141, 255] },
        Kind::RecycleEmpty | Kind::RecycleFull => Pal { body: [143, 176, 201], body2: [108, 140, 167], ink: [58, 82, 102], line: [88, 118, 142], acc: [236, 240, 244] },
        Kind::Home => Pal { body: [242, 232, 220], body2: [211, 106, 74], ink: [139, 94, 60], line: [176, 150, 124], acc: [255, 255, 255] },
        Kind::Download => Pal { body: [107, 119, 133], body2: [76, 88, 102], ink: [40, 50, 62], line: [70, 82, 96], acc: [76, 141, 255] },
        Kind::Documents | Kind::Txt => Pal { body: [245, 245, 247], body2: [214, 214, 218], ink: [140, 150, 164], line: [176, 184, 196], acc: [76, 141, 255] },
        Kind::Pictures | Kind::ImageFile => Pal { body: [245, 245, 247], body2: [214, 214, 218], ink: [126, 200, 227], line: [176, 184, 196], acc: [76, 175, 125] },
        Kind::Music => Pal { body: [245, 245, 247], body2: [214, 214, 218], ink: [167, 139, 250], line: [176, 184, 196], acc: [167, 139, 250] },
        Kind::Videos | Kind::VideoFile => Pal { body: [46, 58, 71], body2: [107, 119, 133], ink: [224, 92, 92], line: [70, 82, 96], acc: [236, 238, 242] },
        Kind::AudioFile => Pal { body: [245, 245, 247], body2: [214, 214, 218], ink: [244, 143, 177], line: [176, 184, 196], acc: [244, 143, 177] },
        Kind::Exe => Pal { body: [236, 238, 242], body2: [76, 141, 255], ink: [46, 58, 71], line: [176, 184, 196], acc: [255, 255, 255] },
        Kind::Pdf => Pal { body: [245, 245, 247], body2: [214, 214, 218], ink: [224, 82, 82], line: [176, 184, 196], acc: [255, 255, 255] },
        Kind::Archive => Pal { body: [212, 168, 67], body2: [180, 138, 48], ink: [120, 94, 36], line: [150, 118, 44], acc: [255, 236, 170] },
        Kind::Code => Pal { body: [245, 245, 247], body2: [214, 214, 218], ink: [61, 220, 132], line: [176, 184, 196], acc: [61, 220, 132] },
        Kind::Generic => Pal { body: [245, 245, 247], body2: [214, 214, 218], ink: [140, 150, 164], line: [176, 184, 196], acc: [140, 150, 164] },
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

fn draw(kind: Kind, cv: &mut Cv, round: bool, color: bool, dark: bool) {
    let p = pal_for(kind, color, dark);
    // 直角=几乎不倒角；圆角=大倒角。所有 shape 共用这一个旋钮。
    let r = if round { 3.2 } else { 0.55 };
    let r2 = if round { 2.2 } else { 0.4 };
    let (b, b2, ink, line, acc) = (p.body, p.body2, p.ink, p.line, p.acc);

    match kind {
        Kind::Folder => {
            // 后片（标签）
            cv.fill([2.0, 5.0, 17.0, 13.0], b2, |x, y| sd_rrect(x, y, 2.5, 5.5, 13.5, 7.5, r2));
            // 前片
            cv.fill([2.0, 9.0, 30.0, 27.0], b, |x, y| sd_rrect(x, y, 2.5, 9.5, 27.0, 17.0, r));
        }
        Kind::Drive => {
            cv.fill([2.0, 10.0, 30.0, 23.0], b, |x, y| sd_rrect(x, y, 2.5, 10.5, 27.0, 12.0, r));
            cv.fill([3.0, 17.0, 29.0, 19.0], b2, |x, y| sd_seg(x, y, 4.0, 18.0, 28.0, 18.0, 0.7));
            cv.fill([23.0, 19.0, 27.5, 22.5], acc, |x, y| {
                sd_ellipse(x, y, 25.2, 20.7, 1.5, 1.5)
            });
        }
        Kind::ThisPc => {
            cv.fill([3.0, 4.0, 29.0, 21.0], b, |x, y| sd_rrect(x, y, 3.5, 4.5, 25.0, 16.0, r2));
            cv.fill([5.0, 6.0, 27.0, 18.0], acc, |x, y| sd_rrect(x, y, 5.5, 6.5, 21.0, 11.0, r2 * 0.7));
            cv.fill([10.0, 20.0, 22.0, 27.0], b2, |x, y| {
                sd_poly(x, y, &[[13.5, 20.5], [18.5, 20.5], [21.0, 25.5], [11.0, 25.5]])
                    .min(sd_rrect(x, y, 8.5, 25.0, 15.0, 2.6, 1.0))
            });
        }
        Kind::RecycleEmpty | Kind::RecycleFull => {
            if kind == Kind::RecycleFull {
                // 溢出的纸团
                cv.fill([9.0, 2.0, 24.0, 10.0], acc, |x, y| {
                    sd_ellipse(x, y, 12.8, 6.8, 2.6, 2.4)
                        .min(sd_ellipse(x, y, 17.0, 5.2, 3.0, 2.8))
                        .min(sd_ellipse(x, y, 21.0, 7.0, 2.4, 2.2))
                });
            }
            // 提手
            cv.fill([12.0, 3.5, 20.0, 7.5], b2, |x, y| sd_rrect(x, y, 12.5, 4.0, 7.0, 3.0, 1.2));
            // 桶盖
            cv.fill([6.0, 6.0, 26.0, 11.5], b2, |x, y| sd_rrect(x, y, 6.5, 6.5, 19.0, 4.5, r2));
            // 桶身（上宽下窄）
            cv.fill([8.0, 10.0, 24.0, 29.0], b, |x, y| {
                sd_poly(x, y, &[[9.0, 10.5], [23.0, 10.5], [21.0, 28.0], [11.0, 28.0]])
            });
            // 竖纹
            for gx in [13.0f32, 16.0, 19.0] {
                cv.fill([gx - 1.2, 12.0, gx + 1.2, 27.0], line, move |x, y| {
                    sd_seg(x, y, gx, 13.0, gx, 26.0, 0.55)
                });
            }
        }
        Kind::Home => {
            cv.fill([1.0, 3.0, 31.0, 15.0], b2, |x, y| {
                sd_poly(x, y, &[[16.0, 4.0], [29.0, 14.0], [3.0, 14.0]])
            });
            cv.fill([5.0, 12.0, 27.0, 29.0], b, |x, y| sd_rrect(x, y, 5.5, 12.5, 21.0, 16.0, r2));
            cv.fill([12.0, 19.0, 20.0, 29.0], ink, |x, y| sd_rrect(x, y, 12.5, 19.5, 7.0, 9.0, r2));
        }
        Kind::Desktop => {
            cv.fill([2.0, 5.0, 30.0, 22.0], b, |x, y| sd_rrect(x, y, 2.5, 5.5, 27.0, 16.5, r2));
            cv.fill([4.0, 7.0, 28.0, 19.0], acc, |x, y| sd_rrect(x, y, 4.5, 7.5, 23.0, 11.0, r2 * 0.7));
            cv.fill([4.0, 16.0, 28.0, 19.0], b2, |x, y| sd_rrect(x, y, 4.5, 16.2, 23.0, 2.3, 0.6));
            cv.fill([10.0, 21.0, 22.0, 28.0], b2, |x, y| {
                sd_poly(x, y, &[[13.5, 21.5], [18.5, 21.5], [21.0, 26.0], [11.0, 26.0]])
                    .min(sd_rrect(x, y, 8.5, 25.5, 15.0, 2.5, 1.0))
            });
        }
        Kind::Download => {
            cv.fill([8.0, 3.0, 24.0, 21.0], acc, |x, y| {
                sd_poly(x, y, &[
                    [16.0, 4.0], [22.0, 12.0], [18.4, 12.0], [18.4, 20.0],
                    [13.6, 20.0], [13.6, 12.0], [10.0, 12.0],
                ])
            });
            cv.fill([4.0, 20.0, 28.0, 28.0], b, |x, y| sd_rrect(x, y, 4.5, 20.5, 23.0, 7.0, r));
        }
        Kind::Documents | Kind::Txt | Kind::Generic => {
            let lines_n = if kind == Kind::Txt { 4 } else if kind == Kind::Documents { 3 } else { 0 };
            cv.fill_page(&p, r);
            for i in 0..lines_n {
                let y = 13.5 + i as f32 * 3.6;
                let x1 = if i == lines_n - 1 { 19.0 } else { 21.5 };
                cv.fill([10.0, y - 1.0, x1 + 1.0, y + 1.0], ink, move |x, yy| {
                    sd_seg(x, yy, 11.0, y, x1, y, 0.62)
                });
            }
        }
        Kind::Pictures | Kind::ImageFile => {
            // 相框 = 大图；文档页里的小图 = 图片文件。两者共用一套"天+山+太阳"。
            if kind == Kind::Pictures {
                cv.fill([3.0, 6.0, 29.0, 26.0], b2, |x, y| sd_rrect(x, y, 3.5, 6.5, 25.0, 19.0, r));
            } else {
                cv.fill_page(&p, r);
            }
            // 画框内区（Pictures）/ 页内小图（ImageFile）
            let (fx, fy, fw, fh) = if kind == Kind::Pictures {
                (5.5f32, 8.5f32, 21.0f32, 15.0f32)
            } else {
                (9.5, 13.0, 13.0, 9.5)
            };
            // 天
            let sky = ink;
            let (x0, y0, x1, y1) = (fx, fy, fx + fw, fy + fh);
            cv.fill([x0, y0, x1, y1], sky, move |x, y| {
                sd_rrect(x, y, x0, y0, x1 - x0, y1 - y0, if kind == Kind::Pictures { r2 * 0.7 } else { 1.2 })
            });
            // 山（双峰）
            let mount = acc;
            let base = y1 - 0.5;
            let midx = (x0 + x1) / 2.0;
            cv.fill([x0, y0, x1, y1], mount, move |x, y| {
                let h = (y1 - y0) * 0.62;
                sd_poly(x, y, &[
                    [x0 + 1.0, base],
                    [x0 + (x1 - x0) * 0.38, base - h],
                    [x0 + (x1 - x0) * 0.66, base],
                ])
                .min(sd_poly(x, y, &[
                    [midx + 1.0, base],
                    [midx + (x1 - x0) * 0.22, base - h * 0.68],
                    [x1 - 1.0, base],
                ]))
            });
            // 太阳：彩色 = 暖黄；黑白 = 跟 acc 走（灰阶），别漏出一抹黄破坏整套灰度
            let sun = if color { [255u8, 206, 92] } else { acc };
            let sx = x1 - (x1 - x0) * 0.22;
            let sy = y0 + (y1 - y0) * 0.28;
            let sr = ((x1 - x0) * 0.10).max(1.4);
            cv.fill([sx - sr - 0.5, sy - sr - 0.5, sx + sr + 0.5, sy + sr + 0.5], sun, move |x, y| {
                sd_ellipse(x, y, sx, sy, sr, sr)
            });
        }
        Kind::Music => {
            cv.fill([8.0, 4.0, 27.0, 27.0], ink, |x, y| {
                sd_seg(x, y, 15.0, 22.5, 15.0, 7.0, 1.2)
                    .min(sd_seg(x, y, 23.5, 20.5, 23.5, 5.0, 1.2))
                    .min(sd_seg(x, y, 15.0, 7.0, 23.5, 5.0, 2.2))
                    .min(sd_ellipse(x, y, 12.2, 23.0, 3.3, 2.5))
                    .min(sd_ellipse(x, y, 20.7, 21.0, 3.3, 2.5))
            });
        }
        Kind::Videos | Kind::VideoFile => {
            if kind == Kind::Videos {
                cv.fill([2.0, 7.0, 30.0, 25.0], b, |x, y| sd_rrect(x, y, 2.5, 7.5, 27.0, 17.0, r));
                for py in [10.5f32, 16.0, 21.5] {
                    cv.fill([4.0, py - 1.3, 7.5, py + 1.3], acc, move |x, y| {
                        sd_rrect(x, y, 4.2, py - 1.1, 2.6, 2.2, 0.5)
                    });
                }
                cv.fill([10.0, 11.0, 24.0, 21.0], ink, |x, y| {
                    sd_poly(x, y, &[[13.0, 12.0], [22.5, 16.0], [13.0, 20.0]])
                });
            } else {
                cv.fill_page(&p, r);
                cv.fill([10.0, 13.0, 23.0, 24.0], ink, |x, y| {
                    sd_poly(x, y, &[[12.5, 14.5], [21.0, 18.8], [12.5, 23.0]])
                });
            }
        }
        Kind::AudioFile => {
            cv.fill_page(&p, r);
            cv.fill([10.0, 11.0, 23.0, 25.0], ink, |x, y| {
                sd_seg(x, y, 15.2, 21.5, 15.2, 12.5, 1.0)
                    .min(sd_seg(x, y, 15.2, 12.8, 20.0, 14.6, 1.6))
                    .min(sd_ellipse(x, y, 13.0, 22.2, 2.4, 1.9))
            });
        }
        Kind::Exe => {
            cv.fill([4.0, 5.0, 28.0, 27.0], b, |x, y| sd_rrect(x, y, 4.5, 5.5, 23.0, 21.0, r));
            cv.fill([4.0, 5.0, 28.0, 11.0], b2, |x, y| sd_rrect(x, y, 4.5, 5.5, 23.0, 5.0, r));
            for gx in [9.0f32, 12.0, 15.0] {
                cv.fill([gx - 1.1, 6.8, gx + 1.1, 9.0], acc, move |x, y| {
                    sd_ellipse(x, y, gx, 7.9, 0.85, 0.85)
                });
            }
            cv.fill([10.0, 13.0, 22.0, 25.0], b2, |x, y| sd_ellipse(x, y, 16.0, 18.5, 4.6, 4.6));
            cv.fill([13.5, 16.0, 18.5, 21.0], b, |x, y| sd_ellipse(x, y, 16.0, 18.5, 2.0, 2.0));
        }
        Kind::Pdf => {
            cv.fill_page(&p, r);
            cv.fill([9.0, 16.0, 23.0, 23.0], ink, |x, y| sd_rrect(x, y, 9.5, 16.5, 13.0, 6.0, 1.4));
            cv.fill([11.0, 18.0, 20.5, 19.6], acc, |x, y| sd_rrect(x, y, 11.2, 18.1, 8.6, 1.4, 0.5));
            cv.fill([11.0, 20.3, 17.5, 21.9], acc, |x, y| sd_rrect(x, y, 11.2, 20.4, 6.0, 1.4, 0.5));
        }
        Kind::Archive => {
            cv.fill([4.0, 9.0, 28.0, 29.0], b, |x, y| sd_rrect(x, y, 4.5, 9.5, 23.0, 19.0, r));
            cv.fill([5.0, 14.0, 27.0, 16.0], b2, |x, y| sd_seg(x, y, 5.5, 15.0, 26.5, 15.0, 0.8));
            cv.fill([14.0, 14.0, 18.0, 29.0], b2, |x, y| sd_seg(x, y, 16.0, 15.0, 16.0, 28.0, 1.3));
            cv.fill([13.0, 17.0, 19.0, 21.5], acc, |x, y| sd_rrect(x, y, 13.5, 17.5, 5.0, 3.5, 1.0));
        }
        Kind::Code => {
            cv.fill_page(&p, r);
            cv.fill([8.0, 12.0, 24.0, 23.0], ink, |x, y| {
                sd_seg(x, y, 13.0, 14.5, 10.2, 17.5, 1.0)
                    .min(sd_seg(x, y, 10.2, 17.5, 13.0, 20.5, 1.0))
                    .min(sd_seg(x, y, 19.0, 14.5, 21.8, 17.5, 1.0))
                    .min(sd_seg(x, y, 21.8, 17.5, 19.0, 20.5, 1.0))
                    .min(sd_seg(x, y, 17.6, 13.6, 15.0, 21.4, 0.9))
            });
        }
    }
}

// ---------------------------------------------------------------- 渲染出口

/// 渲染一枚图标成 HICON。`size` 是成品边长（16/32…），内部 4× 超采样。
///
/// 返回的 HICON 由调用方负责 `DestroyIcon`（`IconCache` 统一持有）。
pub fn render(kind: Kind, round: bool, color: bool, dark: bool, size: i32) -> HICON {
    let size = size.clamp(16, 256);
    let mut cv = Cv::new(size);
    draw(kind, &mut cv, round, color, dark);

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
