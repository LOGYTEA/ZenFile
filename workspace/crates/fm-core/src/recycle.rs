//! 回收站：直接解析 `$Recycle.Bin`，不走 Shell 的 IShellFolder。
//!
//! 为什么绕开 Shell：资源管理器打开回收站要对每一项构建属性存储（取"原位置"、
//! "删除日期"、"大小"三列），几千项时肉眼可见地卡。Windows 的回收站其实只是
//! 每个固定磁盘根目录下的一个 `$Recycle.Bin\<用户SID>` 文件夹，里面成对存放：
//!   - `$I......`：信息头（版本 / 大小 / 删除时间 / 原始路径）
//!   - `$R......`：被删除文件的实体（名字只差首字母）
//! 直接读这些文件：一次目录遍历 + 每个 $I 一次小文件读，比 Shell 那条路快一个
//! 数量级，而且**不需要初始化 COM**。
//!
//! $I 头的两种格式（实测本机 Windows 10 为版本 2）：
//! ```text
//! v2: 0x00 u32 版本 | 0x04 u32 ? | 0x08 u64 大小 | 0x10 FILETIME | 0x18 u32 路径字符数(含结尾 NUL) | 0x1C UTF-16LE 路径
//! v1: 0x00 u32 版本 | 0x04 u32 大小 | 0x08 FILETIME            | 0x10 UTF-16LE 路径（到文件尾）
//! ```

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use windows::core::HSTRING;
use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};

/// 固定磁盘（GetDriveTypeW 的返回值）。没有符号常量，直接按文档取值。
const DRIVE_FIXED: u32 = 3;

/// 回收站里的一项。
#[derive(Clone, Debug)]
pub struct Item {
    /// 实体文件名（`$R......`），还原/彻底删除都靠它配对
    pub id: String,
    /// 删除前的原始路径（含文件名）
    pub original: PathBuf,
    /// 只含文件名的显示名
    pub name: String,
    pub size: u64,
    /// 删除时间（FILETIME：1601 起 100ns）
    pub deleted: i64,
    /// `$R` 实体路径
    pub r_path: PathBuf,
    /// `$I` 信息路径
    pub i_path: PathBuf,
    pub is_dir: bool,
}

/// 当前用户的 SID 字符串（`S-1-5-21-...`）。取不到返回 None，调用方退化为扫全部子目录。
fn current_sid() -> Option<String> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return None;
        }
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        if len == 0 {
            let _ = CloseHandle(token);
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            len,
            &mut len,
        );
        let _ = CloseHandle(token);
        if ok.is_err() {
            return None;
        }
        let tu = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut raw: windows::core::PWSTR = windows::core::PWSTR::null();
        if ConvertSidToStringSidW(tu.User.Sid, &mut raw).is_err() {
            return None;
        }
        let s = raw.to_string().unwrap_or_default();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

/// 所有存在回收站的盘符（只取固定磁盘；U 盘/光驱没有回收站）。
fn bin_roots() -> Vec<PathBuf> {
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = format!("{letter}:\\");
        let hs = HSTRING::from(root.as_str());
        // DRIVE_FIXED = 3：只有固定磁盘才有 $Recycle.Bin
        if unsafe { GetDriveTypeW(&hs) } == DRIVE_FIXED {
            out.push(PathBuf::from(format!("{letter}:\\$Recycle.Bin")));
        }
    }
    out
}

/// 遍历回收站，返回全部条目（按删除时间倒序，最近删除的在前）。
pub fn list() -> Vec<Item> {
    let mut all = Vec::new();
    list_streaming(|batch| all.extend(batch));
    all.sort_by(|a, b| b.deleted.cmp(&a.deleted));
    all
}

/// 分批扫描，边扫边回吐。
///
/// # 为什么必须缓存
///
/// 一项要读一次 `$I` 小文件。实测本机**任何一次** `File::open` 都要 202 µs
/// （在 %TEMP% 里新建 2000 个普通小文件作对照，也是 202 µs），而且
/// 2 线程 / 32 线程都是 8 秒左右 —— 加线程完全摊不掉，是杀软的文件过滤驱动
/// 在逐次扫描。所以 4.6 万项 = 9 秒，光靠"优化"是压不下去的。
///
/// 唯一的出路是**别打开那么多文件**：`$I` 写进去之后就不会再改，所以解析结果
/// 可以长期缓存。第二次打开时只解析"缓存里没有的新增项"（一般就几条），
/// 列表瞬间就满。
///
/// # 首屏
///
/// 冷启动仍然要几秒，所以按 512 条一块解析、一块回吐：界面在两三百毫秒内
/// 就有内容，不再是一片空白。
pub fn list_streaming(mut on_batch: impl FnMut(Vec<Item>)) {
    let sid = current_sid();
    let mut cache = load_cache();
    // 本趟真实存在的 $I 路径。收尾时用它把"已经被还原/清空"的旧缓存条目剔掉。
    let mut seen: HashSet<String> = HashSet::new();
    let mut pending: Vec<Item> = Vec::new();
    let mut fresh_n = 0usize;
    const CHUNK: usize = 512;
    const FLUSH: usize = 400;
    for root in bin_roots() {
        let (i_files, r_dirs) = enumerate_bin(&root, &sid);
        if i_files.is_empty() {
            continue;
        }
        for start in (0..i_files.len()).step_by(CHUNK) {
            let end = (start + CHUNK).min(i_files.len());
            let (items, fresh) = parse_range(&i_files[start..end], &r_dirs, &cache);
            for it in &items {
                seen.insert(it.i_path.to_string_lossy().to_string());
            }
            for it in fresh {
                fresh_n += 1;
                cache.insert(it.i_path.to_string_lossy().to_string(), it);
            }
            pending.extend(items);
            if pending.len() >= FLUSH {
                on_batch(std::mem::take(&mut pending));
            }
        }
        if !pending.is_empty() {
            on_batch(std::mem::take(&mut pending));
        }
    }
    // 已经不在回收站里的（还原了 / 清空了）从缓存里删掉，否则会永远显示
    let n_inserted = cache.len();
    cache.retain(|k, _| seen.contains(k));
    // 缓存没动过就别重写那 10MB：刷新一次列表就写一遍纯属浪费
    if fresh_n > 0 || cache.len() != n_inserted {
        save_cache(&cache);
    }
}

/// 枚举一个盘的回收站。
///
/// 一次 `read_dir` 同时收两样东西：
///   * 所有 `$I` 的路径（要读内容的）
///   * 所有 `$R` 的"是否目录" —— **`DirEntry::file_type()` 在 Windows 上直接读
///     WIN32_FIND_DATA，不再走一次系统调用**。老代码给每条 `$R` 单独 stat 一次，
///     4.6 万条就是 4.6 万次额外的 CreateFile。
///
/// 实测这一段只要 110 ms（4.6 万项），跟后面读 `$I` 的 8 秒比可以忽略。
fn enumerate_bin(
    root: &Path,
    sid: &Option<String>,
) -> (Vec<(Vec<u16>, PathBuf, PathBuf)>, HashMap<Vec<u16>, bool>) {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let mut i_files: Vec<(Vec<u16>, PathBuf, PathBuf)> = Vec::new();
    let mut r_dirs: HashMap<Vec<u16>, bool> = HashMap::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return (i_files, r_dirs);
    };
    for sid_dir in rd.flatten() {
        // 只认本用户的 SID 目录：多用户机器上不能把别人的回收站列出来。
        // 取不到 SID 时退化成扫全部子目录（至少保证功能可用）。
        if let Some(s) = sid {
            let dirname = sid_dir.file_name().to_string_lossy().to_uppercase();
            if dirname != s.to_uppercase() {
                continue;
            }
        }
        let dir = sid_dir.path();
        let Ok(inner) = std::fs::read_dir(&dir) else { continue };
        for e in inner.flatten() {
            let fname = e.file_name();
            // 不看 `to_string_lossy()`：4 万次转码 + 分配纯属浪费。
            // 直接看前两个 UTF-16 码元是不是 "$I" / "$R"。
            let mut wide: Vec<u16> = fname.encode_wide().collect();
            if wide.len() < 3 || wide[0] != '$' as u16 {
                continue;
            }
            let key = wide[2..].to_vec();
            if wide[1] == 'I' as u16 {
                wide[1] = 'R' as u16; // 同一项的实体文件，名字只差第二个字符
                i_files.push((key, dir.join(OsString::from_wide(&wide)), e.path()));
            } else if wide[1] == 'R' as u16 {
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                r_dirs.insert(key, is_dir);
            }
        }
    }
    (i_files, r_dirs)
}

/// 解析 `files` 这一段。返回 `(全部条目, 本次新解析出来的)`。
///
/// 缓存里有的直接取，一次文件都不开；只有缓存里没有的才真的去读 `$I`。
fn parse_range(
    files: &[(Vec<u16>, PathBuf, PathBuf)],
    rmap: &HashMap<Vec<u16>, bool>,
    cache: &HashMap<String, Item>,
) -> (Vec<Item>, Vec<Item>) {
    // 过滤驱动把每次 open 串行化了，多线程只能摊掉一点点（实测 ~20%），
    // 不值得无限加线程。
    let threads = if files.len() >= 64 { 4 } else { 1 };
    let chunk = (files.len() / threads).max(1);
    let f = files;
    let parts: Vec<(Vec<Item>, Vec<Item>)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..files.len())
            .step_by(chunk)
            .map(|start| {
                let end = (start + chunk).min(files.len());
                s.spawn(move || {
                    let mut all = Vec::new();
                    let mut fresh = Vec::new();
                    // $I 头 = 32 字节固定头 + UTF-16 原始路径，一般 200~600 字节，
                    // 一次 read 就够。别用 std::fs::read：它会先 stat 一次来定缓冲
                    // 大小，等于每条多一次系统调用。
                    let mut buf = [0u8; 4096];
                    for i in start..end {
                        let (key, r_path, i_path) = &f[i];
                        // $R 不在 → 这一对是残的（$I 留下了、实体已被清掉）
                        let Some(&is_dir) = rmap.get(key) else { continue };
                        let ck = i_path.to_string_lossy().to_string();
                        if let Some(c) = cache.get(&ck) {
                            let mut it = c.clone();
                            it.is_dir = is_dir; // 目录属性每趟免费重取一次，别信陈的
                            all.push(it);
                            continue;
                        }
                        let Ok(mut fh) = std::fs::File::open(i_path) else { continue };
                        use std::io::Read;
                        let Ok(n) = fh.read(&mut buf) else { continue };
                        if let Some(it) = parse_info(r_path, i_path, &buf[..n], is_dir) {
                            fresh.push(it.clone());
                            all.push(it);
                        }
                    }
                    (all, fresh)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_default())
            .collect()
    });
    let mut all = Vec::with_capacity(files.len());
    let mut fresh = Vec::new();
    for (a, f2) in parts {
        all.extend(a);
        fresh.extend(f2);
    }
    (all, fresh)
}

/// 解析一个 `$I` 头。`r_path` 与 `is_dir` 由调用方一并给出 —— 配对与类型是在
/// 目录枚举那一遍就顺手拿到的，这里不再碰磁盘。
fn parse_info(r_path: &Path, i_path: &Path, data: &[u8], is_dir: bool) -> Option<Item> {
    if data.len() < 24 {
        return None;
    }
    let u32at = |o: usize| -> u32 {
        if o + 4 > data.len() {
            return 0;
        }
        u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]])
    };
    let u64at = |o: usize| -> u64 {
        if o + 8 > data.len() {
            return 0;
        }
        u64::from_le_bytes([
            data[o], data[o + 1], data[o + 2], data[o + 3], data[o + 4], data[o + 5], data[o + 6],
            data[o + 7],
        ])
    };
    let version = u32at(0);
    let (size, deleted, path_off) = match version {
        2 => (u64at(8), u64at(0x10) as i64, 0x1Cusize),
        1 => (u32at(4) as u64, u64at(8) as i64, 0x10usize),
        _ => return None,
    };
    let path_end = if version == 2 {
        let chars = u32at(0x18) as usize;
        (path_off + chars * 2).min(data.len())
    } else {
        data.len()
    };
    if path_end <= path_off || path_end > data.len() {
        return None;
    }
    let mut units: Vec<u16> = data[path_off..path_end]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    // 结尾 NUL 不算路径内容
    while units.last() == Some(&0) {
        units.pop();
    }
    if units.is_empty() {
        return None;
    }
    let original = PathBuf::from(String::from_utf16_lossy(&units));

    let r_name = r_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let name = original
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| r_name.clone());
    Some(Item {
        id: r_name,
        original,
        name,
        size,
        deleted,
        r_path: r_path.to_path_buf(),
        i_path: i_path.to_path_buf(),
        is_dir,
    })
}

// ---------------------------------------------------------------- 解析结果缓存
//
// `$I` 文件写进去之后就不会再改，所以解析出来的（原路径 / 大小 / 删除时间）
// 是长期有效的。缓存一份之后，第二次打开回收站就只解析新增的那几条。
// 缓存以 `$I` 的完整路径为键；收尾时把"本趟没出现"的键剔掉，
// 这样还原掉的、清空掉的都会自动消失。

fn cache_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    Some(base.join("ZenFile").join("recycle.bin"))
}

fn load_cache() -> HashMap<String, Item> {
    let mut out = HashMap::new();
    let Some(p) = cache_path() else { return out };
    let Ok(b) = std::fs::read(&p) else { return out };
    if b.len() < 12 || &b[..8] != b"NFRCY01\0" {
        return out;
    }
    let mut o = 8usize;
    let n = u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as usize;
    o += 4;
    out.reserve(n.min(2_000_000));
    for _ in 0..n {
        let Some((i_path, no)) = rd_str(&b, o) else { break };
        o = no;
        let Some((r_path, no)) = rd_str(&b, o) else { break };
        o = no;
        let Some((id, no)) = rd_str(&b, o) else { break };
        o = no;
        let Some((original, no)) = rd_str(&b, o) else { break };
        o = no;
        let Some((name, no)) = rd_str(&b, o) else { break };
        o = no;
        if o + 17 > b.len() {
            break;
        }
        let size = u64::from_le_bytes([
            b[o], b[o + 1], b[o + 2], b[o + 3], b[o + 4], b[o + 5], b[o + 6], b[o + 7],
        ]);
        o += 8;
        let deleted = i64::from_le_bytes([
            b[o], b[o + 1], b[o + 2], b[o + 3], b[o + 4], b[o + 5], b[o + 6], b[o + 7],
        ]);
        o += 8;
        let is_dir = b[o] != 0;
        o += 1;
        out.insert(
            i_path.clone(),
            Item {
                id,
                original: PathBuf::from(original),
                name,
                size,
                deleted,
                r_path: PathBuf::from(r_path),
                i_path: PathBuf::from(i_path),
                is_dir,
            },
        );
    }
    out
}

fn rd_str(b: &[u8], o: usize) -> Option<(String, usize)> {
    if o + 4 > b.len() {
        return None;
    }
    let n = u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as usize;
    let s = o + 4;
    if s + n > b.len() {
        return None;
    }
    Some((String::from_utf8_lossy(&b[s..s + n]).into_owned(), s + n))
}

fn wr_str(b: &mut Vec<u8>, s: &str) {
    let x = s.as_bytes();
    b.extend_from_slice(&(x.len() as u32).to_le_bytes());
    b.extend_from_slice(x);
}

fn save_cache(map: &HashMap<String, Item>) {
    let Some(p) = cache_path() else { return };
    let mut b: Vec<u8> = Vec::with_capacity(map.len() * 160 + 64);
    b.extend_from_slice(b"NFRCY01\0");
    b.extend_from_slice(&(map.len() as u32).to_le_bytes());
    for it in map.values() {
        wr_str(&mut b, &it.i_path.to_string_lossy());
        wr_str(&mut b, &it.r_path.to_string_lossy());
        wr_str(&mut b, &it.id);
        wr_str(&mut b, &it.original.to_string_lossy());
        wr_str(&mut b, &it.name);
        b.extend_from_slice(&it.size.to_le_bytes());
        b.extend_from_slice(&it.deleted.to_le_bytes());
        b.push(if it.is_dir { 1 } else { 0 });
    }
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // 先写临时文件再改名：中途崩了不会留下半个坏缓存
    let tmp = p.with_extension("bin.tmp");
    if std::fs::write(&tmp, &b).is_err() {
        return;
    }
    if std::fs::rename(&tmp, &p).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// 丢掉解析结果缓存（清空回收站之后必须调，否则下一趟枚举是 0，
/// 缓存会被自然清空；但显式清一次更保险）。
pub fn clear_cache() {
    if let Some(p) = cache_path() {
        let _ = std::fs::remove_file(p);
    }
}

/// 还原：把 `$R` 实体搬回原始路径（必要时补建目录）。返回成功条数。
pub fn restore(items: &[Item]) -> usize {
    let mut ok = 0;
    for it in items {
        if let Some(parent) = it.original.parent() {
            if !parent.exists() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let target = unique_target(&it.original);
        let moved = if it.is_dir {
            move_dir(&it.r_path, &target)
        } else {
            std::fs::rename(&it.r_path, &target).is_ok()
        };
        if moved {
            let _ = std::fs::remove_file(&it.i_path);
            ok += 1;
        }
    }
    ok
}

/// 彻底删除：删掉 `$R` 实体与 `$I` 信息文件。返回成功条数。
pub fn remove(items: &[Item]) -> usize {
    let mut ok = 0;
    for it in items {
        let gone = if it.is_dir {
            std::fs::remove_dir_all(&it.r_path).is_ok()
        } else {
            std::fs::remove_file(&it.r_path).is_ok()
        };
        // 实体删不掉就别删信息文件，否则回收站里会留下一条永远还原不了的幽灵项
        if gone {
            let _ = std::fs::remove_file(&it.i_path);
            ok += 1;
        }
    }
    ok
}

/// 清空回收站：优先走系统 API，失败再自己逐项删。
pub fn empty() -> bool {
    use windows::Win32::UI::Shell::SHEmptyRecycleBinW;
    unsafe {
        let r = SHEmptyRecycleBinW(None, windows::core::PCWSTR::null(), 0);
        if r.is_ok() {
            clear_cache();
            return true;
        }
    }
    let items = list();
    let ok = !items.is_empty() && remove(&items) > 0;
    if ok {
        clear_cache();
    }
    ok
}

/// 回收站总条目数与总字节数（供侧栏/状态栏显示）。
pub fn summary() -> (usize, u64) {
    let items = list();
    (items.len(), items.iter().map(|i| i.size).sum())
}

/// 目标已存在时自动加后缀，避免还原时覆盖同名的新文件。
fn unique_target(p: &Path) -> PathBuf {
    if !p.exists() {
        return p.to_path_buf();
    }
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let parent = p.parent().unwrap_or(Path::new(""));
    for i in 1..1000 {
        let cand = parent.join(format!("{stem} ({i}){ext}"));
        if !cand.exists() {
            return cand;
        }
    }
    p.to_path_buf()
}

/// 目录的移动：同盘 rename，跨盘则复制后删除。
fn move_dir(from: &Path, to: &Path) -> bool {
    if std::fs::rename(from, to).is_ok() {
        return true;
    }
    if copy_dir_recursive(from, to).is_err() {
        return false;
    }
    let _ = std::fs::remove_dir_all(from);
    true
}

fn copy_dir_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)?.flatten() {
        let target = to.join(e.file_name());
        let meta = e.metadata()?;
        if meta.is_dir() {
            copy_dir_recursive(&e.path(), &target)?;
        } else {
            std::fs::copy(e.path(), &target)?;
        }
    }
    Ok(())
}
