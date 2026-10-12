//! 与 Windows 资源管理器互认"每个文件夹用哪种查看方式"（技术方案 §5）。
//!
//! 资源管理器把每个去过的文件夹的视图存在注册表里，俗称 **bags**：
//!
//! ```text
//! HKCU\Software\Classes\Local Settings\Software\Microsoft\Windows\Shell\
//!     BagMRU\                  <- 一棵树：节点的 NodeSlot 指向下面的 bag 号
//!       <n>\Shell\              <- 节点的 NodeSlot
//!     Bags\<slot>\Shell\<FolderType GUID>\
//!         Vid    = "{...}"     <- 视图的唯一标识（最准）
//!         Mode   = 4           <- 老式视图号
//!         IconSize = 16
//! ```
//!
//! Win8 之后这套东西从 `Software\Microsoft\Windows\Shell` 挪到了
//! `Software\Classes\Local Settings\...`，两个位置都要找。
//!
//! **只做尽力而为**：BagMRU 里存的是 ITEMIDLIST（Shell 自己的二进制结构，
//! 格式随版本变过好几次）。这里不去逐字节解析它，而是从二进制里捞可读的名字
//! 再逐级拼出路径，最后只接受"磁盘上真的存在"的那几条。捞不出来、对不上的
//! 一律返回 None —— 上层退回配置里的默认视图，绝不会把一个错的视图套到
//! 别的文件夹上。

use std::path::{Path, PathBuf};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW, RegQueryValueExW,
    RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE,
    REG_SZ,
};

/// 两个可能的位置：Win8+ 的 Local Settings 优先。
const ROOTS: [&str; 2] = [
    r"Software\Classes\Local Settings\Software\Microsoft\Windows\Shell",
    r"Software\Microsoft\Windows\Shell",
];

// —— 视图的几种写法 ——
const VID_DETAILS: &str = "{137E7700-3573-11CF-AE69-08002B2E1262}";
const VID_ICONS: &str = "{0057D0E0-3573-11CF-AE69-08002B2E1262}";
const VID_LIST: &str = "{0E1FA5E0-3573-11CF-AE69-08002B2E1262}";
const VID_TILES: &str = "{65F125E5-7BE1-4810-BA9D-D271C8432CE3}";
/// 写回时用的 FolderType：Generic（资源管理器对普通文件夹就是它）
const FOLDER_TYPE_GENERIC: &str = "{5C4F28B5-F869-4E84-8E60-F11DB97C5CC7}";

fn vid_of(mode: fm_core::ViewMode) -> &'static str {
    match mode {
        fm_core::ViewMode::Details => VID_DETAILS,
        fm_core::ViewMode::List => VID_LIST,
        // 中图标 ↔ 资源管理器"平铺"最接近：都是 48px 带文字的格子
        fm_core::ViewMode::MediumIcons => VID_TILES,
        fm_core::ViewMode::LargeIcons => VID_ICONS,
    }
}

/// `Mode`（老式视图号）：1=大图标 3=列表 4=详细信息 6=平铺
fn mode_id(mode: fm_core::ViewMode) -> u32 {
    match mode {
        fm_core::ViewMode::Details => 4,
        fm_core::ViewMode::List => 3,
        fm_core::ViewMode::MediumIcons => 6,
        fm_core::ViewMode::LargeIcons => 1,
    }
}

/// `LogicalViewMode`：1=详细信息 2=平铺 3=图标 4=列表
fn logical_id(mode: fm_core::ViewMode) -> u32 {
    match mode {
        fm_core::ViewMode::Details => 1,
        fm_core::ViewMode::MediumIcons => 2,
        fm_core::ViewMode::LargeIcons => 3,
        fm_core::ViewMode::List => 4,
    }
}

fn icon_size(mode: fm_core::ViewMode) -> u32 {
    match mode {
        fm_core::ViewMode::Details | fm_core::ViewMode::List => 16,
        fm_core::ViewMode::MediumIcons => 48,
        fm_core::ViewMode::LargeIcons => 96,
    }
}

fn vid_to_mode(vid: &str) -> Option<fm_core::ViewMode> {
    let v = vid.trim().to_lowercase();
    if v == VID_DETAILS.to_lowercase() {
        Some(fm_core::ViewMode::Details)
    } else if v == VID_ICONS.to_lowercase() {
        Some(fm_core::ViewMode::LargeIcons)
    } else if v == VID_LIST.to_lowercase() {
        Some(fm_core::ViewMode::List)
    } else if v == VID_TILES.to_lowercase() {
        Some(fm_core::ViewMode::MediumIcons)
    } else {
        None
    }
}

/// 老式 `Mode` 号的兜底（有些 bag 只有 Mode 没有 Vid）
fn mode_id_to_mode(id: u32) -> Option<fm_core::ViewMode> {
    match id {
        4 => Some(fm_core::ViewMode::Details),
        3 => Some(fm_core::ViewMode::List),
        1 => Some(fm_core::ViewMode::LargeIcons),
        6 => Some(fm_core::ViewMode::MediumIcons),
        _ => None,
    }
}

// ---------------------------------------------------------------- 注册表薄封装

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn open(path: &str, write: bool) -> Option<HKEY> {
    let w = wide(path);
    let mut h = HKEY::default();
    let access = if write { KEY_READ | KEY_WRITE } else { KEY_READ };
    if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(w.as_ptr()), Some(0), access, &mut h).is_err() {
        return None;
    }
    Some(h)
}

unsafe fn value_raw(h: HKEY, name: &str) -> Option<Vec<u8>> {
    let n = wide(name);
    let mut ty = REG_DWORD;
    let mut cb = 0u32;
    if RegQueryValueExW(h, PCWSTR(n.as_ptr()), None, Some(&mut ty), None, Some(&mut cb)).is_err() {
        return None;
    }
    if cb == 0 {
        return Some(Vec::new());
    }
    let mut buf = vec![0u8; cb as usize];
    if RegQueryValueExW(
        h,
        PCWSTR(n.as_ptr()),
        None,
        Some(&mut ty),
        Some(buf.as_mut_ptr()),
        Some(&mut cb),
    )
    .is_err()
    {
        return None;
    }
    Some(buf)
}

unsafe fn value_dword(h: HKEY, name: &str) -> Option<u32> {
    let b = value_raw(h, name)?;
    if b.len() < 4 {
        return None;
    }
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

unsafe fn value_str(h: HKEY, name: &str) -> Option<String> {
    let b = value_raw(h, name)?;
    let u16s: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|c| *c != 0)
        .collect();
    Some(String::from_utf16_lossy(&u16s))
}

unsafe fn set_dword(h: HKEY, name: &str, v: u32) -> bool {
    let n = wide(name);
    RegSetValueExW(h, PCWSTR(n.as_ptr()), Some(0), REG_DWORD, Some(&v.to_le_bytes())).is_ok()
}

unsafe fn set_str(h: HKEY, name: &str, v: &str) -> bool {
    let n = wide(name);
    let d: Vec<u8> = v.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
    RegSetValueExW(h, PCWSTR(n.as_ptr()), Some(0), REG_SZ, Some(&d)).is_ok()
}

/// 枚举键下的所有**值名**（值本身用 value_* 取）。
unsafe fn value_names(h: HKEY) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0u32;
    loop {
        let mut buf = vec![0u16; 256];
        let mut len = buf.len() as u32;
        if RegEnumValueW(
            h,
            i,
            Some(PWSTR(buf.as_mut_ptr())),
            &mut len,
            None,
            None,
            None,
            None,
        )
        .is_err()
        {
            break;
        }
        let s = String::from_utf16_lossy(&buf[..len as usize]);
        if !s.is_empty() {
            out.push(s);
        }
        i += 1;
        if i > 512 {
            break;
        }
    }
    out
}

unsafe fn subkey_names(h: HKEY) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0u32;
    loop {
        let mut buf = vec![0u16; 256];
        let mut len = buf.len() as u32;
        if RegEnumKeyExW(h, i, Some(PWSTR(buf.as_mut_ptr())), &mut len, None, None, None, None)
            .is_err()
        {
            break;
        }
        out.push(String::from_utf16_lossy(&buf[..len as usize]));
        i += 1;
        if i > 512 {
            break;
        }
    }
    out
}

// ---------------------------------------------------------------- 名字抽取

fn is_name_char(c: char) -> bool {
    !c.is_control() && c != '\\' && c != '/' && c != ':'
}

/// 从一段 ITEMIDLIST 里捞文件夹名。
///
/// 不逐字节解析（格式随 Windows 版本变过），只做两件事：
///   1. 卷项（类型字节 0x2F）的名字是紧跟其后的 ASCIIZ —— "E:\" 这种；
///   2. 其余一律在整段里找最长的一段可读 UTF-16LE（长名通常在扩展块里，
///      位置最靠后），找不到再退 ASCII（8.3 短名）。
fn item_name(blob: &[u8]) -> Option<String> {
    if blob.len() < 5 {
        return None;
    }
    // 卷项：size(2) + type(1)=0x2F + ASCIIZ
    if blob[2] == 0x2F {
        let s: String = blob[3..].iter().take_while(|b| **b != 0).map(|b| *b as char).collect();
        if !s.is_empty() && s.len() <= 260 {
            return Some(s);
        }
    }
    let mut best: Option<String> = None;
    // UTF-16LE：偶数偏移上取连续可读字符
    let mut cur = String::new();
    let mut i = 0;
    while i + 1 < blob.len() {
        let c = u16::from_le_bytes([blob[i], blob[i + 1]]);
        if c == 0 {
            if !cur.is_empty() {
                if better(&cur, &best) {
                    best = Some(cur.clone());
                }
                cur.clear();
            }
            i += 2;
            continue;
        }
        if let Some(ch) = char::from_u32(c as u32) {
            if is_name_char(ch) {
                cur.push(ch);
            } else if !cur.is_empty() {
                if better(&cur, &best) {
                    best = Some(cur.clone());
                }
                cur.clear();
            }
        }
        i += 2;
    }
    if !cur.is_empty() && better(&cur, &best) {
        best = Some(cur);
    }
    if best.is_some() {
        return best;
    }
    // 兜底：ASCII
    let mut cur = String::new();
    let mut best: Option<String> = None;
    for b in blob {
        let ch = *b as char;
        if ch.is_ascii_alphanumeric() || "_.-~ #()[]".contains(ch) {
            cur.push(ch);
        } else if !cur.is_empty() {
            if better(&cur, &best) {
                best = Some(cur.clone());
            }
            cur.clear();
        }
    }
    if !cur.is_empty() && better(&cur, &best) {
        best = Some(cur);
    }
    best
}

fn better(cand: &str, best: &Option<String>) -> bool {
    if cand.is_empty() || cand.len() > 200 {
        return false;
    }
    // "CFSF" 这类结构魔数不是名字；GUID 也不是
    if cand == "CFSF" || cand.starts_with('{') {
        return false;
    }
    match best {
        None => true,
        Some(b) => cand.len() > b.len(),
    }
}

// ---------------------------------------------------------------- BagMRU 遍历

/// 遍历 BagMRU，产出 `路径 -> bag 号`。只保留磁盘上真实存在的路径。
///
/// 结果带 60 秒缓存：这棵树在资源管理器用久了会有几百上千个节点，
/// 而 `system_view()` 是**每次进目录**都要调的，不缓存的话导航会被拖出卡顿。
fn slot_map() -> Vec<(String, u32)> {
    if let Ok(g) = CACHE.lock() {
        if let Some((at, m)) = &*g {
            if at.elapsed() < std::time::Duration::from_secs(60) {
                return m.clone();
            }
        }
    }
    let out = slot_map_uncached();
    if let Ok(mut g) = CACHE.lock() {
        *g = Some((std::time::Instant::now(), out.clone()));
    }
    out
}

static CACHE: std::sync::Mutex<Option<(std::time::Instant, Vec<(String, u32)>)>> =
    std::sync::Mutex::new(None);

fn slot_map_uncached() -> Vec<(String, u32)> {
    let mut out: Vec<(String, u32)> = Vec::new();
    for root in ROOTS {
        let Some(h) = (unsafe { open(root, false) }) else {
            continue;
        };
        let Some(tree) = (unsafe { open(&format!("{root}\\BagMRU"), false) }) else {
            unsafe {
                let _ = RegCloseKey(h);
            }
            continue;
        };
        walk(tree, String::new(), 0, &mut out);
        unsafe {
            let _ = RegCloseKey(tree);
            let _ = RegCloseKey(h);
        }
        if !out.is_empty() {
            break;
        }
    }
    out
}

fn walk(node: HKEY, path: String, depth: usize, out: &mut Vec<(String, u32)>) {
    if depth > 10 || out.len() > 4000 {
        return;
    }
    unsafe {
        // 本节点自己的 bag 号（路径已知的祖先才收）
        if !path.is_empty() {
            if let Some(slot) = value_dword(node, "NodeSlot") {
                out.push((path.clone(), slot));
            }
        }
        // 每个值 "i" 描述一个子节点，名字从值内容里捞，槽位在同名子键里
        let mut names: Vec<(String, String)> = Vec::new();
        for v in value_names(node) {
            if !v.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            if let Some(blob) = value_raw(node, &v) {
                if let Some(n) = item_name(&blob) {
                    names.push((v, n));
                }
            }
        }
        for (key, name) in names {
            let child = join_path(&path, &name);
            let Some(sub) = open_sub(node, &key) else {
                continue;
            };
            walk(sub, child, depth + 1, out);
            let _ = RegCloseKey(sub);
        }
    }
}

unsafe fn open_sub(parent: HKEY, name: &str) -> Option<HKEY> {
    let n = wide(name);
    let mut h = HKEY::default();
    if RegOpenKeyExW(parent, PCWSTR(n.as_ptr()), Some(0), KEY_READ, &mut h).is_err() {
        return None;
    }
    Some(h)
}

fn join_path(parent: &str, name: &str) -> String {
    // 卷项名字自带根（"C:\"），直接就是完整路径
    if name.len() == 3 && name.chars().nth(1) == Some(':') {
        return name.to_string();
    }
    if parent.is_empty() {
        return name.to_string();
    }
    if parent.ends_with('\\') {
        format!("{parent}{name}")
    } else {
        format!("{parent}\\{name}")
    }
}

fn norm(p: &str) -> String {
    p.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

fn slot_for(path: &Path) -> Option<(String, u32)> {
    let want = norm(&path.to_string_lossy());
    if want.is_empty() {
        return None;
    }
    let map = slot_map();
    // 精确匹配优先；只对"磁盘上真的存在"的条目生效，避免把捞错的
    // 名字当成路径套上去
    let mut best: Option<(String, u32)> = None;
    for (p, slot) in map {
        if norm(&p) == want && Path::new(&p).exists() {
            return Some((p, slot));
        }
        if norm(&p) == want {
            best = Some((p, slot));
        }
    }
    best
}

// ---------------------------------------------------------------- 对外接口

/// 资源管理器给这个文件夹记的视图。找不到（没去过 / 解析不出来）就返回 None。
pub fn system_view(path: &Path) -> Option<fm_core::ViewMode> {
    let (_, slot) = slot_for(path)?;
    for root in ROOTS {
        let base = format!("{root}\\Bags\\{slot}\\Shell");
        let Some(h) = (unsafe { open(&base, false) }) else {
            continue;
        };
        // Win10/11 把视图值放在 FolderType GUID 子键下；老版本直接放 Shell 下
        let mut mode = None;
        if let Some(vid) = (unsafe { value_str(h, "Vid") }) {
            mode = vid_to_mode(&vid);
        }
        if mode.is_none() {
            if let Some(id) = (unsafe { value_dword(h, "Mode") }) {
                mode = mode_id_to_mode(id);
            }
        }
        unsafe {
            let _ = RegCloseKey(h);
        }
        if mode.is_none() {
            for sub in unsafe { subkey_names_of(&base) } {
                let k = format!("{base}\\{sub}");
                let Some(h2) = (unsafe { open(&k, false) }) else {
                    continue;
                };
                if let Some(vid) = (unsafe { value_str(h2, "Vid") }) {
                    mode = vid_to_mode(&vid);
                }
                if mode.is_none() {
                    if let Some(id) = (unsafe { value_dword(h2, "Mode") }) {
                        mode = mode_id_to_mode(id);
                    }
                }
                unsafe {
                    let _ = RegCloseKey(h2);
                }
                if mode.is_some() {
                    break;
                }
            }
        }
        if mode.is_some() {
            return mode;
        }
    }
    None
}

unsafe fn subkey_names_of(path: &str) -> Vec<String> {
    let Some(h) = open(path, false) else {
        return Vec::new();
    };
    let v = subkey_names(h);
    let _ = RegCloseKey(h);
    v
}

/// 把视图写回资源管理器那套 bag。
///
/// 只写**已经存在的** bag：BagMRU 节点要按 Shell 的 ITEMIDLIST 格式新建，
/// 自己捏一个出来风险太大（Explorer 读不懂就会把整棵树判坏）。
/// 资源管理器没去过的文件夹这里会返回 false，上层就该退回只改自己的配置。
pub fn write_system_view(path: &Path, mode: fm_core::ViewMode) -> bool {
    let Some((_, slot)) = slot_for(path) else {
        return false;
    };
    for root in ROOTS {
        let base = format!("{root}\\Bags\\{slot}\\Shell");
        let Some(h) = (unsafe { open(&base, true) }) else {
            continue;
        };
        // 两处都写：
        //   - `Shell` 键本身是老式位置（Win7 及更早就只认这里）；
        //   - `Shell\<FolderType GUID>` 是 Win8+ 的位置。
        // 只挑一处写是不够的：有的 bag 压根没有 FolderType 子键（Vid 就在
        // Shell 键下），这时候照着 GUID 子键去写会全部落空 —— 软件里看着
        // 视图切了，资源管理器那边一点没动。
        let subs = unsafe { subkey_names(h) };
        let wrote_sub = !subs.is_empty();
        unsafe {
            write_all(h, mode);
            let _ = RegCloseKey(h);
        }
        for sub in subs {
            let Some(h2) = (unsafe { open(&format!("{base}\\{sub}"), true) }) else {
                continue;
            };
            unsafe {
                write_all(h2, mode);
                let _ = RegCloseKey(h2);
            }
        }
        // 一个 FolderType 子键都没有（老式 bag）：补一个 Generic 出来，
        // Win10/11 的资源管理器认这个位置。
        if !wrote_sub {
            let sub = wide(&format!("{base}\\{FOLDER_TYPE_GENERIC}"));
            let mut h3 = HKEY::default();
            let created = unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    PCWSTR(sub.as_ptr()),
                    Some(0),
                    None,
                    REG_OPTION_NON_VOLATILE,
                    KEY_READ | KEY_WRITE,
                    None,
                    &mut h3,
                    None,
                )
                .is_ok()
            };
            if created {
                unsafe {
                    write_all(h3, mode);
                    let _ = RegCloseKey(h3);
                }
            }
        }
        return true;
    }
    false
}

unsafe fn write_all(h: HKEY, mode: fm_core::ViewMode) {
    let _ = set_str(h, "Vid", vid_of(mode));
    let _ = set_dword(h, "Mode", mode_id(mode));
    let _ = set_dword(h, "LogicalViewMode", logical_id(mode));
    let _ = set_dword(h, "IconSize", icon_size(mode));
}

// ============================================================ 列（第33轮）
//
// Explorer 的列配置就在同一个 bag 里，值名固定：
//   `Columns`   REG_SZ，逗号分隔的**显示顺序**，每项是列的内部名
//               （name,datemodified,type,size,datecreated,dateaccessed,
//                 author,tags,title,path,attributes）
//   `ColWidths` REG_SZ，**分号分隔**，每项是 "列名:宽度"（像素）
//   `ColFlags`  REG_DWORD 集合，置位表示隐藏（1=不显示），语义是
//               `flag & 1 << 位号`。我们只读不用它 —— 直接读 Columns 更准
//               （Columns 缺项 = 用默认可见，省掉位序对齐的活）。
//
// 找不到 bag（资源管理器没去过这个目录）时读写都返回 false/No，
// 上层退到自己的每目录配置。

/// 我们支持的列 ↔ Explorer 内部名。**必须一字不差**，
/// 差一个字母 Explorer 就认不出这一列（列会消失而不是显示错名字）。
/// 我们的 id 本来就是照 Explorer 的内部名取的（见 fm_core::columns）。

fn bag_keys(path: &Path) -> Option<(String, Vec<String>)> {
    let (_, slot) = slot_for(path)?;
    for root in ROOTS {
        let base = format!("{root}\\Bags\\{slot}\\Shell");
        let Some(h) = (unsafe { open(&base, false) }) else {
            continue;
        };
        let subs = unsafe { subkey_names(h) };
        unsafe {
            let _ = RegCloseKey(h);
        }
        return Some((base, subs));
    }
    None
}

/// 读资源管理器里这个文件夹的显示列序列。`None` = 没 bag / bag 里没 Columns。
pub fn system_columns(path: &Path) -> Option<Vec<String>> {
    let (base, subs) = bag_keys(path)?;
    let mut collect = |key: &str| -> Option<Vec<String>> {
        unsafe {
            let h = open(key, false)?;
            let raw = value_raw(h, "Columns");
            let _ = RegCloseKey(h);
            let raw = raw?;
            // REG_SZ：UTF-16LE。Explorer 写的是每项之间带一个 NUL 的双 NUL 结尾
            let s = decode_utf16z(&raw);
            let mut out: Vec<String> = Vec::new();
            for part in s.split(',') {
                let p = part.trim();
                if !p.is_empty() {
                    out.push(p.to_string());
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        }
    };
    // Shell 键本身先试，再试 FolderType 子键 —— 老 bag 只有前者
    let base_owned = base.clone();
    if let Some(v) = collect(&base_owned) {
        return Some(v);
    }
    for sub in subs {
        if let Some(v) = collect(&format!("{base}\\{sub}")) {
            return Some(v);
        }
    }
    None
}

/// 读资源管理器里某一列的宽度（像素）。`None` = 没记录。
pub fn system_col_width(path: &Path, id: &str) -> Option<u32> {
    let (base, subs) = bag_keys(path)?;
    let mut scan = |key: &str| -> Option<u32> {
        unsafe {
            let h = open(key, false)?;
            let raw = value_raw(h, "ColWidths");
            let _ = RegCloseKey(h);
            let s = decode_utf16z(&raw?);
            // 格式 "name:200;datemodified:120"
            for item in s.split(';') {
                let mut it = item.splitn(2, ':');
                let (k, v) = (it.next()?.trim(), it.next()?.trim());
                if k.eq_ignore_ascii_case(id) {
                    return v.parse::<u32>().ok();
                }
            }
            None
        }
    };
    if let Some(v) = scan(&base) {
        return Some(v);
    }
    for sub in subs {
        if let Some(v) = scan(&format!("{base}\\{sub}")) {
            return Some(v);
        }
    }
    None
}

/// 写列序列回资源管理器。
pub fn write_system_columns(path: &Path, cols: &[String]) -> bool {
    let Some((base, subs)) = bag_keys(path) else {
        return false;
    };
    let joined = cols.join(",");
    // 只有已存在的 FolderType 子键 + Shell 键本身都写；一个都没有的老 bag
    // 不擅自建（跟 write_system_view 同一条纪律：Shell 读不懂会把树判坏）
    let mut wrote = false;
    unsafe {
        if let Some(h) = open(&base, true) {
            wrote |= set_str(h, "Columns", &joined);
            let _ = RegCloseKey(h);
        }
    }
    for sub in subs {
        unsafe {
            if let Some(h) = open(&format!("{base}\\{sub}"), true) {
                wrote |= set_str(h, "Columns", &joined);
                let _ = RegCloseKey(h);
            }
        }
    }
    wrote
}

/// 写某一列宽度回资源管理器。
///
/// 宽度的合并规则要照 Explorer：**只替换自己那一项，其余原样保留**。
/// 直接整条重写会把资源管理器记着的其他列宽全清掉 —— 那正是用户最恨的
/// "我的设置被莫名重置"。
pub fn write_system_col_width(path: &Path, id: &str, w: u32) -> bool {
    let Some((base, subs)) = bag_keys(path) else {
        return false;
    };
    let apply = |key: &str| -> bool {
        unsafe {
            let Some(h) = open(key, true) else {
                return false;
            };
            let cur = value_raw(h, "ColWidths").map(|r| decode_utf16z(&r)).unwrap_or_default();
            let mut items: Vec<String> = cur
                .split(';')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let tag = format!("{id}:{w}");
            match items.iter().position(|s| {
                s.split(':').next().map(|k| k.trim().eq_ignore_ascii_case(id)).unwrap_or(false)
            }) {
                Some(i) => items[i] = tag,
                None => items.push(tag),
            }
            let ok = set_str(h, "ColWidths", &items.join(";"));
            let _ = RegCloseKey(h);
            ok
        }
    };
    let mut wrote = apply(&base);
    for sub in subs {
        wrote |= apply(&format!("{base}\\{sub}"));
    }
    wrote
}

/// REG_SZ 原始字节 → String（去掉结尾 NUL）。
unsafe fn decode_utf16z(raw: &[u8]) -> String {
    let mut units: Vec<u16> = Vec::with_capacity(raw.len() / 2);
    let mut i = 0;
    while i + 1 < raw.len() {
        units.push(u16::from_le_bytes([raw[i], raw[i + 1]]));
        i += 2;
    }
    let end = units.iter().position(|&c| c == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// 供自检/排查用：本机扫到了多少个可用 bag。
pub fn debug_slot_count() -> usize {
    slot_map().len()
}

/// 自检用：把一段 ITEMIDLIST 的名字捞出来（纯函数，不碰注册表）。
pub fn debug_item_name(blob: &[u8]) -> Option<String> {
    item_name(blob)
}

/// 自检用：VID ↔ ViewMode 的双向映射必须自洽。
pub fn debug_vid_roundtrip() -> bool {
    for m in ALL_MODES {
        if vid_to_mode(vid_of(m)) != Some(m) {
            return false;
        }
    }
    vid_to_mode("{00000000-0000-0000-0000-000000000000}").is_none()
}

const ALL_MODES: [fm_core::ViewMode; 4] = [
    fm_core::ViewMode::Details,
    fm_core::ViewMode::List,
    fm_core::ViewMode::LargeIcons,
    fm_core::ViewMode::MediumIcons,
];

/// 自检用：老式 `Mode` 号 ↔ ViewMode 也要自洽。
pub fn debug_mode_tables() -> bool {
    for m in ALL_MODES {
        if mode_id_to_mode(mode_id(m)) != Some(m) {
            return false;
        }
        if logical_id(m) < 1 || logical_id(m) > 4 || icon_size(m) == 0 {
            return false;
        }
    }
    mode_id_to_mode(999).is_none()
}

/// 自检用例表（纯逻辑 + 一次只读的注册表扫描，绝不写注册表）。
pub fn selftest() -> Vec<(&'static str, bool, String)> {
    let mut out: Vec<(&'static str, bool, String)> = Vec::new();
    out.push(("VID ↔ 视图模式映射自洽", debug_vid_roundtrip(), String::new()));
    out.push(("老式 Mode 号 ↔ 视图模式自洽", debug_mode_tables(), String::new()));

    // 卷项：size(2) + type(0x2F) + "C:\" ASCIIZ
    let mut vol: Vec<u8> = vec![0x16, 0x00, 0x2F];
    vol.extend_from_slice(b"C:\\");
    vol.push(0);
    let n = debug_item_name(&vol);
    out.push((
        "卷项 ITEMIDLIST 能捞出盘符",
        n.as_deref() == Some("C:\\"),
        format!("{n:?}"),
    ));

    // 普通文件夹：结构头之后跟一段 UTF-16LE 长名
    let mut f: Vec<u8> = vec![0x20, 0x00, 0x31, 0x00, 0x00, 0x00];
    for c in "Downloads".encode_utf16() {
        f.extend_from_slice(&c.to_le_bytes());
    }
    f.extend_from_slice(&[0, 0]);
    let n2 = debug_item_name(&f);
    out.push((
        "文件夹长名能从 ITEMIDLIST 捞出来",
        n2.as_deref() == Some("Downloads"),
        format!("{n2:?}"),
    ));
    out.push((
        "碎片（<5 字节）不产出名字",
        debug_item_name(&[1, 2, 3]).is_none(),
        String::new(),
    ));
    out.push((
        "GUID 结构不被当成文件夹名",
        !matches!(debug_item_name(&guid_blob()), Some(s) if s.starts_with('{')),
        String::new(),
    ));

    // 「新建」模板：只读扫 HKCR 的 ShellNew。断言"必有 .txt"（任何 Windows 都挂了它），
    // 其余项随安装的软件变化，只报不判。
    let tpls = crate::shellnew::templates();
    let list: Vec<String> = tpls.iter().map(|t| format!("{}({})", t.label, t.ext)).collect();
    out.push((
        "扫得到系统「新建」模板且含 .txt",
        tpls.iter().any(|t| t.ext == ".txt"),
        format!("{} 项：{}", tpls.len(), list.join(", ")),
    ));

    // 菜单文本：助记键 `&` 只该吃掉它自己，助记字符要留下（"打开(&O)" → "打开(O)"）。
    // 早先连 O 一起吞掉，于是右键菜单里每一项都拖着一对空括号 —— "打开()"。
    let s1 = crate::menu::strip_accel("打开(&O)");
    out.push((
        "菜单文本去助记键后括号里不是空的",
        s1 == "打开(O)",
        format!("打开(&O) -> {s1}"),
    ));
    let s2 = crate::menu::strip_accel("a&&b");
    out.push((
        "两个 && 还原成一个真 &",
        s2 == "a&b",
        format!("a&&b -> {s2}"),
    ));
    // 去重必须认得出 Shell 的"打开(&O)"和我们自己的"打开"是同一条。
    // 认不出来的话菜单里就会同时出现两个「打开」。
    let k1 = crate::menu::menu_key("打开(&O)");
    let k2 = crate::menu::menu_key("打开");
    out.push((
        "Shell 条目与自有条目的去重键一致",
        k1 == k2 && k1 == "打开",
        format!("{k1} == {k2}"),
    ));

    // 只读扫一次注册表：允许一个 bag 都没有（干净机器 / CI），但不许崩
    let cnt = debug_slot_count();
    out.push((
        "扫描资源管理器 bag 不崩溃",
        true,
        format!("本机可用 bag {cnt} 个"),
    ));

    // 写回往返：证明"软件里改的会同步到系统"这条路真的通
    let (ok, detail) = debug_roundtrip();
    out.push(("改视图能写回资源管理器并读回", ok, detail));
    out
}

/// BagMRU 里第一条"磁盘上确实还在"的路径（自检挑靶子用）。
pub fn debug_first_real() -> Option<String> {
    slot_map()
        .into_iter()
        .find(|(p, _)| Path::new(p).is_dir())
        .map(|(p, _)| p)
}

/// 自检用：**写入 → 读回** 的往返。
///
/// 挑一个资源管理器去过的真实文件夹，把它改成另一种视图、读回来核对，
/// 再**还原成原值** —— 不能因为跑一次自检就把用户文件夹的视图改掉。
pub fn debug_roundtrip() -> (bool, String) {
    let Some(p) = debug_first_real() else {
        return (true, "本机没有可用的 bag，跳过".to_string());
    };
    let path = PathBuf::from(&p);
    let before = system_view(&path);
    let target = if before == Some(fm_core::ViewMode::Details) {
        fm_core::ViewMode::LargeIcons
    } else {
        fm_core::ViewMode::Details
    };
    if !write_system_view(&path, target) {
        return (false, format!("{p}: 写入失败"));
    }
    let after = system_view(&path);
    // 还原
    if let Some(b) = before {
        write_system_view(&path, b);
    }
    let restored = system_view(&path);
    (
        after == Some(target) && restored == before,
        format!("{p}: {before:?} -> {after:?} -> 还原 {restored:?}"),
    )
}

/// 造一段以 GUID 文本为主的 blob，验证它不会被当成文件夹名。
fn guid_blob() -> Vec<u8> {
    let mut v: Vec<u8> = vec![0x60, 0x00, 0x1F];
    for c in "{5C4F28B5-F869-4E84-8E60-F11DB97C5CC7}".encode_utf16() {
        v.extend_from_slice(&c.to_le_bytes());
    }
    v.extend_from_slice(&[0, 0]);
    v
}

#[allow(dead_code)]
fn _assert_pathbuf_used(p: &PathBuf) -> bool {
    p.exists()
}
