//! 设置页「右键菜单」右侧那块**实时预览**（第56轮）。
//!
//! 铺的是"各种格式的样本文件"：图片 / 视频 / 音频 / 压缩包 / 文档（ppt、word、
//! pdf…）/ 代码 / 其它，**按字母序分组**、**大图标网格**排列，超出窗口就滚。
//!
//! 排布规则（用户原话）：
//!   "同一行一放一个字母的，比如 a 开头，如果有 5 个，那你一排显示不下，
//!    那就第二排继续显示，没了直接换行。"
//! 也就是：**每个字母都从新的一行开头起排**，排满了往下续一行，这一组完了
//! 立刻换行给下一个字母 —— 一个字母绝不会和另一个字母挤在同一行。
//!
//! 交互：
//!   · 在某一格上右键 → 弹**这个格式真实的右键菜单**（`own_menu_rows` 现算）；
//!   · 在下面的空白处右键 → 弹**目录右键**（背景菜单，items 为空那套）；
//!   · 菜单里的命令**不会真的执行**（`App::preview_menu`），只用来看长相。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use windows::core::HSTRING;
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES,
};
use windows::Win32::UI::WindowsAndMessaging::HICON;

/// 一枚格式样本。
pub struct Sample {
    /// 显示名 = 扩展名大写（"PNG" / "DOCX"），**排序与分字母都按它**。
    pub name: String,
    /// 扩展名（不带点，小写）—— 取图标用。
    pub ext: String,
    /// 真实落盘的样本文件路径（0 字节；只为"这一格背后真有个文件"）。
    pub path: PathBuf,
}

/// 所有格式（图片 / 视频 / 音频 / 压缩包 / 办公文档 / 代码 / 其它）。
///
/// 顺序随便写，构建时会按 name 排一遍；重复项自动去掉。
const EXTS: &[&str] = &[
    // —— 图片 ——
    "png", "jpg", "jpeg", "jfif", "gif", "bmp", "webp", "tif", "tiff", "ico", "svg", "heic",
    "heif", "avif", "tga", "psd", "ai", "eps", "emf", "wmf", "dds", "cr2", "nef", "arw",
    // —— 视频 ——
    "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "mpg", "mpeg", "m2ts", "mts", "rmvb",
    "rm", "3gp", "vob", "ogv", "asf", "m4v", "ts",
    // —— 音频 ——
    "mp3", "wav", "flac", "aac", "ogg", "oga", "opus", "m4a", "wma", "ape", "aiff", "mid",
    "midi", "amr", "ac3",
    // —— 压缩包 / 镜像 ——
    "zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz", "iso", "cab", "arj", "lzh",
    // —— 文档 / 办公（ppt、word、excel、pdf…）——
    "doc", "docx", "dot", "dotx", "xls", "xlsx", "xlsm", "xlsb", "ppt", "pptx", "pps", "ppsx",
    "pptm", "pdf", "rtf", "txt", "md", "odt", "ods", "odp", "csv", "epub", "mobi", "wps", "et",
    "dps", "xps", "chm", "djvu",
    // —— 代码 / 配置 ——
    "c", "cpp", "h", "hpp", "cs", "java", "js", "ts", "json", "xml", "yml", "yaml", "toml",
    "ini", "log", "sql", "sh", "bat", "ps1", "py", "rb", "go", "rs", "php", "html", "htm",
    "css", "vue",
    // —— 其它 ——
    "exe", "msi", "dll", "lnk", "url", "apk", "ttf", "otf", "db",
];

/// 样本文件落在哪：%TEMP%\ZenFilePreview\
fn sample_dir() -> Option<PathBuf> {
    let base = std::env::var("TEMP").ok().or_else(|| std::env::var("TMP").ok())?;
    if base.trim().is_empty() {
        return None;
    }
    Some(PathBuf::from(base).join("ZenFilePreview"))
}

fn build() -> Vec<Sample> {
    let mut v: Vec<Sample> = Vec::with_capacity(EXTS.len());
    for e in EXTS {
        if v.iter().any(|s| s.ext == *e) {
            continue;
        }
        v.push(Sample {
            name: e.to_uppercase(),
            ext: (*e).to_string(),
            path: PathBuf::new(),
        });
    }
    // 按字母序（大写名直接比就是字典序）
    v.sort_by(|a, b| a.name.cmp(&b.name));

    // 落盘：0 字节，只为"这一格背后真有个文件"。建一次，之后每次进来直接复用。
    if let Some(dir) = sample_dir() {
        if std::fs::create_dir_all(&dir).is_ok() {
            for s in v.iter_mut() {
                let p = dir.join(format!("{}.{}", s.name, s.ext));
                if !p.exists() {
                    let _ = std::fs::write(&p, b"");
                }
                s.path = p;
            }
        }
    }
    v
}

/// 全部样本（进程内只建一次）。
pub fn samples() -> &'static Vec<Sample> {
    static S: OnceLock<Vec<Sample>> = OnceLock::new();
    S.get_or_init(build)
}

/// 分字母的组头字符（非字母归到 `#`）。
pub fn letter_of(name: &str) -> char {
    let c = name.chars().next().unwrap_or('#').to_ascii_uppercase();
    if c.is_ascii_uppercase() {
        c
    } else {
        '#'
    }
}

/// 按首字母切成一组一组，返回 `(字母, 该组样本下标)`。
///
/// 依赖 `samples()` 已按 name 排好序 —— 同一个字母的必然连在一起。
pub fn groups() -> Vec<(char, Vec<usize>)> {
    let s = samples();
    let mut out: Vec<(char, Vec<usize>)> = Vec::new();
    for (i, smp) in s.iter().enumerate() {
        let c = letter_of(&smp.name);
        match out.last_mut() {
            Some((lc, v)) if *lc == c => v.push(i),
            _ => out.push((c, vec![i])),
        }
    }
    out
}

/// 某个扩展名的**图标句柄值**（进程内缓存，图标由本进程持有到退出）。
///
/// 存 `isize` 而不是 `HICON`：`HICON` 是裸指针、不是 `Sync`，放不进 `static`
/// 缓存（与 `appfind::FoundApp::icon` 同一个坑）。
pub fn icon(ext: &str) -> isize {
    static C: OnceLock<Mutex<HashMap<String, isize>>> = OnceLock::new();
    let key = ext.trim_start_matches('.').to_ascii_lowercase();
    let map = C.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(m) = map.lock() {
        if let Some(h) = m.get(&key) {
            return *h;
        }
    }
    // 图标主题接管时按类型给自绘那一套（与文件列表保持同一观感）
    let h: isize = if crate::icon_theme::active() {
        crate::icon_theme::get(crate::icon_theme::ext_kind(&key), 32)
            .map(|i| i.0 as isize)
            .unwrap_or(0)
    } else {
        0
    };
    let h = if h != 0 {
        h
    } else {
        // 探测名**必须带点**：SHGFI_USEFILEATTRIBUTES 让 Shell 完全不碰磁盘，
        // 它只按 pszPath 的"扩展名"查关联图标（写成 "probetxt" 会全部塌成通用图标）。
        let probe = if key.is_empty() {
            "probe".to_string()
        } else {
            format!("probe.{key}")
        };
        let mut sfi = SHFILEINFOW::default();
        let hicon = unsafe {
            SHGetFileInfoW(
                &HSTRING::from(probe.as_str()),
                FILE_ATTRIBUTE_NORMAL,
                Some(&mut sfi),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                windows::Win32::UI::Shell::SHGFI_FLAGS(
                    SHGFI_ICON.0 | SHGFI_USEFILEATTRIBUTES.0 | SHGFI_LARGEICON.0,
                ),
            );
            sfi.hIcon
        };
        hicon.0 as isize
    };
    if let Ok(mut m) = map.lock() {
        m.insert(key, h);
    }
    h
}

/// 句柄值 → HICON（绘制时用）。
pub fn hicon(v: isize) -> HICON {
    HICON(v as *mut core::ffi::c_void)
}
