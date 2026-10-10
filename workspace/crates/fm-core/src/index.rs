//! 文件名索引：Everything 式"秒搜"的核心。
//!
//! Everything 之所以能在一瞬间搜出全盘结果，是因为它**不遍历目录树**，而是直接读
//! NTFS 的 MFT。读 MFT 要管理员权限，还要处理 USN 日志、重解析点等一堆细节 ——
//! 一个不提权的资源管理器拿不到。
//!
//! 这里换一条不需要提权的路，效果接近：
//!   * 首次搜索时后台把整个卷**遍历一次**，把"文件名 + 父目录 + 大小/时间"写进一个
//!     紧凑的二进制缓存（`%LOCALAPPDATA%\NexFile\index\C.bin`）；
//!   * 之后再搜索就直接在这个缓存里做子串匹配 —— 全卷上百万条也是毫秒级；
//!   * 缓存只是加速手段：没有它、或它过期/损坏时回退到现地递归搜索，结果一样。
//!
//! 缓存刻意**不常驻监听**（§12：不做递归目录监听）。它的新鲜度靠"建立时刻 + 用户
//! 手动重建"来兜，搜索结果里命中已删除的文件时打开会失败，这在 Everything 里同样
//! 存在（它靠 USN 订阅才缓解，那需要提权）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// 单卷索引的条目上限。超过就停下并标记 `complete = false`，
/// 免得把几百兆内存吃在一个索引上。
pub const MAX_ENTRIES: usize = 2_000_000;
/// 目录深度上限：防重解析点成环，也防病态深层目录。
const MAX_DEPTH: usize = 32;
/// 遍历时直接跳过的目录名（读不了 / 没意义 / 会让索引爆炸）。
const SKIP_NAMES: [&str; 6] = [
    "$RECYCLE.BIN",
    "System Volume Information",
    "node_modules",
    ".git",
    "$WINDOWS.~BT",
    "WindowsApps",
];

const FLAG_DIR: u8 = 1;
const FLAG_HIDDEN: u8 = 2;

/// 一条搜索命中。
pub struct Hit {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub modified: i64,
    pub dir: bool,
    pub hidden: bool,
}

/// 一个卷的文件名索引。
///
/// 用"并列数组"而不是 `Vec<Entry>`：上百万条时，每条少两个 `String` 的胖指针开销，
/// 内存差出几十兆，遍历时也全在连续内存里走。
pub struct Index {
    pub root: String,
    /// 建立时刻（unix 秒）
    pub built: u64,
    /// 是否完整（撞到条目上限时为 false）
    pub complete: bool,
    dirs: Vec<String>,
    name: Vec<String>,
    lower: Vec<String>,
    parent: Vec<u32>,
    depth: Vec<u8>,
    size: Vec<u64>,
    mtime: Vec<i64>,
    flags: Vec<u8>,
}

impl Index {
    pub fn len(&self) -> usize {
        self.name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.name.is_empty()
    }

    /// 遍历一个卷，建立索引。
    ///
    /// * `cancel` 置 true 时尽快停下（返回的仍是已扫到的部分，用于换卷时收工）；
    /// * `tick` 是进度回报，参数是"已扫到的条目数"。遍历一个满盘 C: 要几十秒，
    ///   没有任何进度反馈的话界面看着就像卡死了。
    pub fn build(root: &str, cancel: &AtomicBool, tick: Option<&dyn Fn(usize)>) -> Index {
        let mut out = Index {
            root: root.to_string(),
            built: now_secs(),
            complete: true,
            dirs: Vec::new(),
            name: Vec::new(),
            lower: Vec::new(),
            parent: Vec::new(),
            depth: Vec::new(),
            size: Vec::new(),
            mtime: Vec::new(),
            flags: Vec::new(),
        };
        out.dirs.push(root.trim_end_matches('\\').to_string());
        // 显式栈（递归写起来短，但目录深了会爆栈）
        let mut stack: Vec<(u32, PathBuf, usize)> = vec![(0, PathBuf::from(root), 0)];
        while let Some((parent, dir, depth)) = stack.pop() {
            if cancel.load(Ordering::Relaxed) || out.name.len() >= MAX_ENTRIES {
                out.complete = false;
                break;
            }
            if depth > MAX_DEPTH {
                continue;
            }
            let rd = match std::fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(_) => continue, // 没权限 / 已删除：跳过，不打断整卷
            };
            let mut sub: Vec<(u32, PathBuf, usize)> = Vec::new();
            for ent in rd.flatten() {
                let name = ent.file_name().to_string_lossy().to_string();
                let Ok(md) = ent.metadata() else { continue };
                use std::os::windows::fs::MetadataExt;
                let attrs = md.file_attributes();
                let is_dir = (attrs & 0x10) != 0; // FILE_ATTRIBUTE_DIRECTORY
                let reparse = (attrs & 0x400) != 0; // FILE_ATTRIBUTE_REPARSE_POINT
                let hidden = (attrs & 0x2) != 0;
                let mut flags = 0u8;
                if is_dir {
                    flags |= FLAG_DIR;
                }
                if hidden {
                    flags |= FLAG_HIDDEN;
                }
                let size = if is_dir { 0 } else { md.len() };
                let mtime = filetime_to_i64(md.last_write_time());
                out.name.push(name.clone());
                out.lower.push(name.to_lowercase());
                out.parent.push(parent);
                out.depth.push(depth as u8);
                out.size.push(size);
                out.mtime.push(mtime);
                out.flags.push(flags);
                if is_dir && !reparse && !SKIP_NAMES.iter().any(|s| name.eq_ignore_ascii_case(s)) {
                    // 目录本身也占一个 dirs 槽位：子项的 parent 指向它
                    let idx = out.dirs.len() as u32;
                    out.dirs.push(ent.path().to_string_lossy().to_string());
                    sub.push((idx, ent.path(), depth + 1));
                }
                if out.name.len() >= MAX_ENTRIES {
                    out.complete = false;
                    break;
                }
            }
            if let Some(f) = tick {
                f(out.name.len());
            }
            // 后进先出 = 深度优先，和"一层一层展开"的直觉一致
            sub.reverse();
            stack.extend(sub);
        }
        out
    }

    fn push_str(buf: &mut Vec<u8>, s: &str) {
        let b = s.as_bytes();
        buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
        buf.extend_from_slice(b);
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(self.name.len() * 40 + 64);
        b.extend_from_slice(b"NFIDX01\0");
        b.extend_from_slice(&self.built.to_le_bytes());
        b.extend_from_slice(&(self.complete as u8).to_le_bytes());
        b.extend_from_slice(&(self.dirs.len() as u32).to_le_bytes());
        b.extend_from_slice(&(self.name.len() as u32).to_le_bytes());
        Self::push_str(&mut b, &self.root);
        for d in &self.dirs {
            Self::push_str(&mut b, d);
        }
        for i in 0..self.name.len() {
            b.extend_from_slice(&self.parent[i].to_le_bytes());
            b.extend_from_slice(&[self.depth[i], self.flags[i]]);
            b.extend_from_slice(&self.size[i].to_le_bytes());
            b.extend_from_slice(&self.mtime[i].to_le_bytes());
            Self::push_str(&mut b, &self.name[i]);
        }
        b
    }

    pub fn from_bytes(b: &[u8]) -> Option<Index> {
        let mut p = 0usize;
        let rd_u32 = |p: &mut usize| -> Option<u32> {
            let v = u32::from_le_bytes(b.get(*p..*p + 4)?.try_into().ok()?);
            *p += 4;
            Some(v)
        };
        let rd_u64 = |p: &mut usize| -> Option<u64> {
            let v = u64::from_le_bytes(b.get(*p..*p + 8)?.try_into().ok()?);
            *p += 8;
            Some(v)
        };
        let rd_i64 = |p: &mut usize| -> Option<i64> {
            let v = i64::from_le_bytes(b.get(*p..*p + 8)?.try_into().ok()?);
            *p += 8;
            Some(v)
        };
        let rd_u8 = |p: &mut usize| -> Option<u8> {
            let v = *b.get(*p)?;
            *p += 1;
            Some(v)
        };
        let rd_str = |p: &mut usize| -> Option<String> {
            let n = u32::from_le_bytes(b.get(*p..*p + 4)?.try_into().ok()?) as usize;
            *p += 4;
            let s = String::from_utf8_lossy(b.get(*p..*p + n)?).to_string();
            // 少了这一步，游标就停在字符串开头，后面每一项都会读回同一段字节，
            // 读到越界为止 —— 表现为"缓存还原出 0 条"。
            *p += n;
            Some(s)
        };
        if b.get(p..p + 8)? != b"NFIDX01\0" {
            return None;
        }
        p += 8;
        let built = rd_u64(&mut p)?;
        let complete = rd_u8(&mut p)? != 0;
        let n_dirs = rd_u32(&mut p)? as usize;
        let n = rd_u32(&mut p)? as usize;
        let root = rd_str(&mut p)?;
        let mut dirs = Vec::with_capacity(n_dirs);
        for _ in 0..n_dirs {
            dirs.push(rd_str(&mut p)?);
        }
        let mut name = Vec::with_capacity(n);
        let mut lower = Vec::with_capacity(n);
        let mut parent = Vec::with_capacity(n);
        let mut depth = Vec::with_capacity(n);
        let mut size = Vec::with_capacity(n);
        let mut mtime = Vec::with_capacity(n);
        let mut flags = Vec::with_capacity(n);
        for _ in 0..n {
            parent.push(rd_u32(&mut p)?);
            depth.push(rd_u8(&mut p)?);
            flags.push(rd_u8(&mut p)?);
            size.push(rd_u64(&mut p)?);
            mtime.push(rd_i64(&mut p)?);
            let s = rd_str(&mut p)?;
            lower.push(s.to_lowercase());
            name.push(s);
        }
        Some(Index {
            root,
            built,
            complete,
            dirs,
            name,
            lower,
            parent,
            depth,
            size,
            mtime,
            flags,
        })
    }

    /// 缓存文件路径。`C:\` → `%LOCALAPPDATA%\NexFile\index\C.bin`
    pub fn cache_path(root: &str) -> Option<PathBuf> {
        let letter = root
            .trim_end_matches('\\')
            .chars()
            .next()
            .filter(|c| c.is_ascii_alphabetic())?;
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
        Some(base.join("NexFile").join("index").join(format!("{}.bin", letter.to_ascii_uppercase())))
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(p) = Self::cache_path(&self.root) else {
            return Ok(());
        };
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // 先写临时文件再改名：中途崩了不会留下半个坏缓存
        let tmp = p.with_extension("bin.tmp");
        std::fs::write(&tmp, self.to_bytes())?;
        match std::fs::rename(&tmp, &p) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    pub fn load(root: &str) -> Option<Index> {
        let p = Self::cache_path(root)?;
        let b = std::fs::read(&p).ok()?;
        Self::from_bytes(&b)
    }

    /// 子串搜索（大小写不敏感），支持 `*` / `?` 通配。
    ///
    /// 排序刻意偏向"最像用户想找的那个"：名字以查询串开头的最先，其次层级浅的，
    /// 再其次名字短的。全卷匹配可能上万条，先按这个序截断到 `limit` 再返回。
    pub fn query(&self, q: &str, limit: usize) -> Vec<Hit> {
        let q = q.trim();
        if q.is_empty() {
            return Vec::new();
        }
        let ql = q.to_lowercase();
        let wild = ql.contains('*') || ql.contains('?');
        let mut hits: Vec<(usize, bool)> = Vec::new();
        for i in 0..self.lower.len() {
            let ok = if wild {
                wild_match(&self.lower[i], &ql)
            } else {
                self.lower[i].contains(&ql)
            };
            if ok {
                let starts = self.lower[i].starts_with(&ql);
                hits.push((i, starts));
                if hits.len() >= 20_000 {
                    break;
                }
            }
        }
        // 稳定排序：先"前缀命中"，再浅层，再短名
        hits.sort_by(|a, b| {
            let (ia, sa) = *a;
            let (ib, sb) = *b;
            sb.cmp(&sa)
                .then(self.depth[ia].cmp(&self.depth[ib]))
                .then(self.name[ia].len().cmp(&self.name[ib].len()))
                .then(self.name[ia].cmp(&self.name[ib]))
        });
        hits.truncate(limit);
        hits.into_iter()
            .map(|(i, _)| Hit {
                name: self.name[i].clone(),
                path: self.path_of(i),
                size: self.size[i],
                modified: self.mtime[i],
                dir: (self.flags[i] & FLAG_DIR) != 0,
                hidden: (self.flags[i] & FLAG_HIDDEN) != 0,
            })
            .collect()
    }

    fn path_of(&self, i: usize) -> PathBuf {
        let dir = self.dirs.get(self.parent[i] as usize).map(|s| s.as_str()).unwrap_or("");
        if dir.is_empty() {
            PathBuf::from(&self.name[i])
        } else {
            PathBuf::from(format!("{dir}\\{}", self.name[i]))
        }
    }
}

/// `*`（任意长）与 `?`（单个字符）的最简匹配：逐段吃 `*`，其余逐字符比对。
fn wild_match(s: &str, pat: &str) -> bool {
    let s: Vec<char> = s.chars().collect();
    let p: Vec<char> = pat.chars().collect();
    let (mut si, mut pi, mut star, mut ss) = (0usize, 0usize, None::<usize>, 0usize);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            si += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            ss = si;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            ss += 1;
            si = ss;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `std::fs::Metadata::last_write_time()`（Windows 扩展）→ 与 `FileEntry::modified`
/// 同一量纲的 i64（FILETIME 的 100ns 步进值）。
fn filetime_to_i64(ft: u64) -> i64 {
    ft as i64
}

/// 取路径所在卷的根（`C:\Users\a` → `C:\`）。UNC 返回 None（不给 UNC 建索引）。
pub fn volume_root(path: &Path) -> Option<String> {
    let s = path.to_string_lossy();
    if s.starts_with("\\\\") {
        return None;
    }
    let b = s.as_bytes();
    if b.len() >= 2 && b[1] == b':' {
        return Some(format!("{}\\", s[..2].to_ascii_uppercase()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Index {
        Index {
            root: "C:\\t".to_string(),
            built: 42,
            complete: true,
            dirs: vec!["C:\\t".to_string(), "C:\\t\\sub".to_string()],
            name: vec!["a.txt".to_string(), "sub".to_string()],
            lower: vec!["a.txt".to_string(), "sub".to_string()],
            parent: vec![0, 0],
            depth: vec![0, 0],
            size: vec![7, 0],
            mtime: vec![1, 2],
            flags: vec![0, FLAG_DIR],
        }
    }

    /// 缓存必须能原样还原：`to_bytes` / `from_bytes` 只要有一处游标没推进，
    /// 症状就是"第二次搜索一个结果都没有"，而且只在有缓存时才出现 —— 很难靠手测发现。
    #[test]
    fn roundtrip() {
        let idx = sample();
        let bytes = idx.to_bytes();
        let back = Index::from_bytes(&bytes).expect("from_bytes 不应返回 None");
        assert_eq!(back.len(), idx.len());
        assert_eq!(back.root, "C:\\t");
        assert_eq!(back.dirs.len(), 2);
        assert_eq!(back.query("a", 10).len(), 1);
        assert_eq!(back.query("sub", 10).len(), 1);
        assert!(back.query("sub", 10)[0].dir);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut b = sample().to_bytes();
        b[0] = b'X';
        assert!(Index::from_bytes(&b).is_none());
        assert!(Index::from_bytes(&[]).is_none());
        assert!(Index::from_bytes(&b[..20]).is_none(), "截断必须被拒绝而非 panic");
    }

    #[test]
    fn wildcard() {
        assert!(wild_match("item1.txt", "item*.txt"));
        assert!(wild_match("item12.txt", "item*.txt"));
        assert!(wild_match("a.txt", "?.txt"));
        assert!(!wild_match("ab.txt", "?.txt"));
        assert!(!wild_match("a.txt", "*.md"));
    }

    #[test]
    fn volume_roots() {
        assert_eq!(volume_root(Path::new("C:\\Users\\a")).as_deref(), Some("C:\\"));
        assert_eq!(volume_root(Path::new("d:\\")).as_deref(), Some("D:\\"));
        assert_eq!(volume_root(Path::new("\\\\srv\\share")), None);
        assert_eq!(volume_root(Path::new("relative")), None);
    }
}
