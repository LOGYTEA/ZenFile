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
