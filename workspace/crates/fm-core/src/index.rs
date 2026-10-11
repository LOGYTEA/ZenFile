//! 文件名索引：Everything 式"秒搜"的核心。
//!
//! Everything 之所以能在一瞬间搜出全盘结果，是因为它**不遍历目录树**，而是直接读
//! NTFS 的 MFT。读 MFT 要管理员权限，还要处理 USN 日志、重解析点等一堆细节 ——
//! 一个不提权的资源管理器拿不到。
//!
//! 这里换一条不需要提权的路：
//!   * 后台把整个卷**遍历一次**，只留"文件名 + 父目录 + 大小/时间"这套最小信息，
//!     写成紧凑的二进制缓存（`%LOCALAPPDATA%\ZenFile\index\C.bin`）；
//!   * 之后再搜索就只在这份缓存里做匹配 —— 全卷上百万条也是毫秒级；
//!   * 缓存只是加速手段：没有它、或它过期/损坏时回退到现地递归搜索，结果一样。
//!
//! # 为什么以前做不到"秒搜"（第 19 轮修掉的三处）
//!
//! 1. **缓存反序列化太慢**：老格式是"每条一个 String"，一百万条就是两百万次
//!    `String` 分配，外加一百万次 `to_lowercase()`。光把缓存读回内存就要好几秒，
//!    用户感受是"第二次搜索也卡"。现在名字全部拼进两块字节块（原始 / 小写），
//!    只配两张偏移表 —— 反序列化从"百万次堆分配"降到"十几次"。
//! 2. **查询是单线程线性扫**：一百万条逐条 `contains`。现在按区间分片多线程扫。
//! 3. **首次搜索要现场走一遍整卷**：单线程走，几十秒。现在按目录层并行走，
//!    并且**启动后就后台建**，等用户真的去搜的时候索引通常已经就绪。
//!
//! 缓存刻意**不常驻监听**（§12：不做递归目录监听）。新鲜度靠"建立时刻 + 用户
//! 手动重建"来兜，命中已删除的文件时打开会失败 —— 这在 Everything 里同样存在
//! （它靠 USN 订阅才缓解，那需要提权）。

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
#[derive(Clone, Debug)]
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
/// 名字不存 `Vec<String>`，而是全部拼进两块字节块：`blob`（原始大小写，用于显示）
/// 与 `lower`（小写，用于匹配），各配一张偏移表。代价是取名字要多一次切片，
/// 收益是**反序列化不再做百万次堆分配** —— 这是"第二次搜索也卡"的根因。
pub struct Index {
    pub root: String,
    /// 建立时刻（unix 秒）
    pub built: u64,
    /// 是否完整（撞到条目上限或被取消时为 false）
    pub complete: bool,
    pub dirs: Vec<String>,
    blob: Vec<u8>,
    lower: Vec<u8>,
    /// `blob` 的偏移表，长度 = 条目数 + 1
    off: Vec<u32>,
    /// `lower` 的偏移表。少数 Unicode 小写化会改变字节长度，不能与 `off` 共用。
    loff: Vec<u32>,
    parent: Vec<u32>,
    depth: Vec<u8>,
    size: Vec<u64>,
    mtime: Vec<i64>,
    flags: Vec<u8>,
}

impl Index {
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    fn name_at(&self, i: usize) -> &str {
        let a = self.off[i] as usize;
        let b = self.off[i + 1] as usize;
        std::str::from_utf8(&self.blob[a..b]).unwrap_or("")
    }

    fn lower_at(&self, i: usize) -> &[u8] {
        let a = self.loff[i] as usize;
        let b = self.loff[i + 1] as usize;
        &self.lower[a..b]
    }

    /// 第 i 条名字的字节长度（小写 blob 与原名等长，取哪边都一样）。
    fn len_at(&self, i: usize) -> usize {
        (self.loff[i + 1] - self.loff[i]) as usize
    }

    /// 遍历一个卷建立索引。**按目录层并行扫描**：单线程走满盘要几十秒，
    /// 这也是以前"第一次搜索像卡死"的直接原因。
    pub fn build(root: &str, cancel: &AtomicBool, tick: Option<&dyn Fn(usize)>) -> Index {
        let nthreads = std::thread::available_parallelism()
            .map(|n| n.get().clamp(2, 8))
            .unwrap_or(4);
        let root_s = root.trim_end_matches('\\').to_string();
        let mut dirs: Vec<String> = vec![root_s.clone()];
        let mut dir_depth: Vec<u8> = vec![0];
        // 目录条目由 scan_dirs 顺带产出；这里只记"哪些目录还没出条目"，
        // 防止因为 MAX_ENTRIES / 取消而提前退出时漏掉最后一批。
        let mut emitted: Vec<bool> = vec![false];
        let mut frontier: Vec<u32> = vec![0];
        let mut complete = true;

        let mut name: Vec<String> = Vec::new();
        let mut lname: Vec<String> = Vec::new();
        let mut parent: Vec<u32> = Vec::new();
        let mut depth: Vec<u8> = Vec::new();
        let mut size: Vec<u64> = Vec::new();
        let mut mtime: Vec<i64> = Vec::new();
        let mut flags: Vec<u8> = Vec::new();

        while !frontier.is_empty() {
            if cancel.load(Ordering::Relaxed) {
                complete = false;
                break;
            }
            let chunks = split_even(&frontier, nthreads);
            let (dirs_ref, dep_ref) = (&dirs, &dir_depth);
            let results: Vec<Vec<ScanOut>> = std::thread::scope(|s| {
                let handles: Vec<_> = chunks
                    .into_iter()
                    .map(|ids| s.spawn(move || scan_dirs(dirs_ref, dep_ref, &ids)))
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap_or_default())
                    .collect()
            });
            let mut next: Vec<u32> = Vec::new();
            for rs in results {
                for o in rs {
                    name.push(o.name.clone());
                    lname.push(o.lname);
                    parent.push(o.parent);
                    depth.push(o.depth);
                    size.push(o.size);
                    mtime.push(o.mtime);
                    flags.push(o.flags);
                    if let Some(p) = o.sub {
                        next.push(dirs.len() as u32);
                        dir_depth.push(o.depth.saturating_add(1));
                        dirs.push(p);
                        emitted.push(true);
                    }
                }
            }
            if name.len() >= MAX_ENTRIES {
                complete = false;
                break;
            }
            if let Some(f) = tick {
                f(name.len());
            }
            frontier = next
                .into_iter()
                .filter(|id| dir_depth[*id as usize] as usize <= MAX_DEPTH)
                .collect();
        }

        // 目录本身也要能被搜到。scan_dirs 已经把它们当普通条目出过了
        // （带真实 mtime / 属性），这里只补"扫到过但没来得及出条目"的那几个，
        // 不能无脑重加一遍——那会让每个目录在结果里出现两次。
        for i in 1..dirs.len() {
            if emitted.get(i).copied().unwrap_or(true) {
                continue;
            }
            let full = &dirs[i];
            let nm = full.rsplit('\\').next().unwrap_or(full).to_string();
            name.push(nm.clone());
            lname.push(nm.to_lowercase());
            parent.push(parent_of(&dirs, full));
            depth.push(dir_depth[i]);
            size.push(0);
            mtime.push(0);
            flags.push(FLAG_DIR);
        }

        Self::assemble(root_s, complete, dirs, name, lname, parent, depth, size, mtime, flags)
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(
        root: String,
        complete: bool,
        dirs: Vec<String>,
        name: Vec<String>,
        lname: Vec<String>,
        parent: Vec<u32>,
        depth: Vec<u8>,
        size: Vec<u64>,
        mtime: Vec<i64>,
        flags: Vec<u8>,
    ) -> Index {
        let n = name.len();
        let mut blob = Vec::with_capacity(n * 16);
        let mut lower = Vec::with_capacity(n * 16);
        let mut off = Vec::with_capacity(n + 1);
        let mut loff = Vec::with_capacity(n + 1);
        for i in 0..n {
            off.push(blob.len() as u32);
            blob.extend_from_slice(name[i].as_bytes());
            loff.push(lower.len() as u32);
            lower.extend_from_slice(lname[i].as_bytes());
        }
        off.push(blob.len() as u32);
        loff.push(lower.len() as u32);
        Index {
            root,
            built: now_secs(),
            complete,
            dirs,
            blob,
            lower,
            off,
            loff,
            parent,
            depth,
            size,
            mtime,
            flags,
        }
    }

    // ------------------------------------------------------------ 序列化

    fn push_str(buf: &mut Vec<u8>, s: &str) {
        let b = s.as_bytes();
        buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
        buf.extend_from_slice(b);
    }

    fn push_u32v(buf: &mut Vec<u8>, v: &[u32]) {
        buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
        for x in v {
            buf.extend_from_slice(&x.to_le_bytes());
        }
    }

    fn push_u64v(buf: &mut Vec<u8>, v: &[u64]) {
        buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
        for x in v {
            buf.extend_from_slice(&x.to_le_bytes());
        }
    }

    fn push_i64v(buf: &mut Vec<u8>, v: &[i64]) {
        buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
        for x in v {
            buf.extend_from_slice(&x.to_le_bytes());
        }
    }

    fn push_u8v(buf: &mut Vec<u8>, v: &[u8]) {
        buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
        buf.extend_from_slice(v);
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let n = self.len();
        let mut b = Vec::with_capacity(self.blob.len() + self.lower.len() + n * 16 + 256);
        b.extend_from_slice(b"NFIDX02\0");
        b.extend_from_slice(&self.built.to_le_bytes());
        b.extend_from_slice(&(self.complete as u8).to_le_bytes());
        b.extend_from_slice(&(self.dirs.len() as u32).to_le_bytes());
        b.extend_from_slice(&(n as u32).to_le_bytes());
        Self::push_str(&mut b, &self.root);
        for d in &self.dirs {
            Self::push_str(&mut b, d);
        }
        Self::push_u32v(&mut b, &self.off);
        Self::push_u32v(&mut b, &self.loff);
        Self::push_u32v(&mut b, &self.parent);
        Self::push_u64v(&mut b, &self.size);
        Self::push_i64v(&mut b, &self.mtime);
        Self::push_u8v(&mut b, &self.depth);
        Self::push_u8v(&mut b, &self.flags);
        Self::push_str(&mut b, &String::from_utf8_lossy(&self.blob));
        Self::push_str(&mut b, &String::from_utf8_lossy(&self.lower));
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
            // 少了这一步游标就停在字符串开头，后面每一项都会读回同一段字节，
            // 读到越界为止 —— 表现为"缓存还原出 0 条"。
            *p += n;
            Some(s)
        };
        let rd_u32v = |p: &mut usize| -> Option<Vec<u32>> {
            let n = rd_u32(p)? as usize;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(rd_u32(p)?);
            }
            Some(v)
        };
        let rd_u64v = |p: &mut usize| -> Option<Vec<u64>> {
            let n = rd_u32(p)? as usize;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(rd_u64(p)?);
            }
            Some(v)
        };
        let rd_i64v = |p: &mut usize| -> Option<Vec<i64>> {
            let n = rd_u32(p)? as usize;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(rd_i64(p)?);
            }
            Some(v)
        };
        let rd_u8v = |p: &mut usize| -> Option<Vec<u8>> {
            let n = rd_u32(p)? as usize;
            let v = b.get(*p..*p + n)?.to_vec();
            *p += n;
            Some(v)
        };
        if b.get(p..p + 8)? != b"NFIDX02\0" {
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
        let off = rd_u32v(&mut p)?;
        let loff = rd_u32v(&mut p)?;
        let parent = rd_u32v(&mut p)?;
        let size = rd_u64v(&mut p)?;
        let mtime = rd_i64v(&mut p)?;
        let depth = rd_u8v(&mut p)?;
        let flags = rd_u8v(&mut p)?;
        let blob = rd_str(&mut p)?.into_bytes();
        let lower = rd_str(&mut p)?.into_bytes();
        if parent.len() != n || off.len() != n + 1 || loff.len() != n + 1 {
            return None;
        }
        Some(Index {
            root,
            built,
            complete,
            dirs,
            blob,
            lower,
            off,
            loff,
            parent,
            depth,
            size,
            mtime,
            flags,
        })
    }

    /// 缓存文件路径。`C:\` → `%LOCALAPPDATA%\ZenFile\index\C.bin`
    pub fn cache_path(root: &str) -> Option<PathBuf> {
        let letter = root
            .trim_end_matches('\\')
            .chars()
            .next()
            .filter(|c| c.is_ascii_alphabetic())?;
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
        Some(base.join("ZenFile").join("index").join(format!("{}.bin", letter.to_ascii_uppercase())))
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

    // ------------------------------------------------------------ 查询

    /// 子串搜索（大小写不敏感），支持 `*` / `?` 通配。
    ///
    /// 排序刻意偏向"最像用户想找的那个"：名字以查询串开头的最先，其次层级浅的，
    /// 再其次名字短的。全卷匹配可能上万条，先按这个序截断到 `limit` 再返回。
    pub fn query(&self, q: &str, limit: usize) -> Vec<Hit> {
        self.query_q(&Query::parse(q), limit)
    }

    /// 结构化查询（高级搜索语法）。按区间分片多线程扫。
    pub fn query_q(&self, q: &Query, limit: usize) -> Vec<Hit> {
        if q.is_empty() || self.is_empty() {
            return Vec::new();
        }
        let n = self.len();
        let threads = std::thread::available_parallelism()
            .map(|v| v.get().clamp(1, 8))
            .unwrap_or(4);
        let chunk = (n / threads).max(4096);
        let idx = self;
        let per_thread_cap = 20_000usize / threads.max(1) + 256;
        let min_len = q.min_name_len();
        let found: Vec<Vec<(usize, bool)>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..n)
                .step_by(chunk)
                .map(|start| {
                    let end = (start + chunk).min(n);
                    s.spawn(move || {
                        let mut out: Vec<(usize, bool)> = Vec::new();
                        for i in start..end {
                            // 长度都不够，后面的子串比较全免了
                            if min_len > 0 && idx.len_at(i) < min_len {
                                continue;
                            }
                            if !q.matches_entry(idx, i) {
                                continue;
                            }
                            let lb = idx.lower_at(i);
                            let starts = q
                                .texts
                                .first()
                                .map(|t| starts_with_bytes(lb, t.as_bytes()))
                                .unwrap_or(false);
                            out.push((i, starts));
                            if out.len() >= per_thread_cap {
                                break;
                            }
                        }
                        out
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
        });
        let mut hits: Vec<(usize, bool)> = found.into_iter().flatten().collect();
        if hits.is_empty() {
            return Vec::new();
        }
        let cmp = |a: &(usize, bool), b: &(usize, bool)| {
            let (ia, sa) = *a;
            let (ib, sb) = *b;
            sb.cmp(&sa)
                .then(idx.depth[ia].cmp(&idx.depth[ib]))
                .then(idx.len_at(ia).cmp(&idx.len_at(ib)))
                .then(idx.name_at(ia).cmp(idx.name_at(ib)))
        };
        if hits.len() > limit {
            // 只要前 limit 名：先 O(n) 选出第 limit 名，再只排这 limit 个。
            // 全量 sort_by 是 O(n log n)，2 万候选 × 字符串比较要 20ms，
            // 换成分区选择后降到 2ms 左右。
            hits.select_nth_unstable_by(limit, cmp);
            hits.truncate(limit);
        }
        hits.sort_by(cmp);
        hits
            .into_iter()
            .map(|(i, _)| Hit {
                name: idx.name_at(i).to_string(),
                path: idx.path_of(i),
                size: idx.size[i],
                modified: idx.mtime[i],
                dir: (idx.flags[i] & FLAG_DIR) != 0,
                hidden: (idx.flags[i] & FLAG_HIDDEN) != 0,
            })
            .collect()
    }

    fn path_of(&self, i: usize) -> PathBuf {
        let dir = self.dirs.get(self.parent[i] as usize).map(|s| s.as_str()).unwrap_or("");
        if dir.is_empty() {
            PathBuf::from(self.name_at(i))
        } else {
            PathBuf::from(format!("{}\\{}", dir, self.name_at(i)))
        }
    }
}

// ---------------------------------------------------------------- 建索引的内部件

struct ScanOut {
    name: String,
    lname: String,
    parent: u32,
    depth: u8,
    size: u64,
    mtime: i64,
    flags: u8,
    /// 这一条若是目录，这里是它的完整路径（由主线程分配 dirs 槽位）
    sub: Option<String>,
}

fn scan_dirs(dirs: &[String], dir_depth: &[u8], ids: &[u32]) -> Vec<ScanOut> {
    let mut out = Vec::new();
    for &id in ids {
        let Some(dir) = dirs.get(id as usize) else { continue };
        let d = dir_depth[id as usize];
        let rd = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for ent in rd.flatten() {
            let name = match ent.file_name().into_string() {
                Ok(s) => s,
                Err(_) => ent.file_name().to_string_lossy().to_string(),
            };
            let Ok(md) = ent.metadata() else { continue };
            use std::os::windows::fs::MetadataExt;
            let attrs = md.file_attributes();
            let is_dir = (attrs & 0x10) != 0;
            let reparse = (attrs & 0x400) != 0;
            let hidden = (attrs & 0x2) != 0;
            let mut flags = 0u8;
            if is_dir {
                flags |= FLAG_DIR;
            }
            if hidden {
                flags |= FLAG_HIDDEN;
            }
            let sub = if is_dir
                && !reparse
                && !SKIP_NAMES.iter().any(|s| name.eq_ignore_ascii_case(s))
            {
                Some(ent.path().to_string_lossy().to_string())
            } else {
                None
            };
            out.push(ScanOut {
                lname: name.to_lowercase(),
                name,
                parent: id,
                depth: d,
                size: if is_dir { 0 } else { md.len() },
                mtime: md.last_write_time() as i64,
                flags,
                sub,
            });
        }
    }
    out
}

/// 目录 `full` 的父目录在 `dirs` 里的槽位（找不到就退回 0 = 卷根）。
fn parent_of(dirs: &[String], full: &str) -> u32 {
    let parent_path = match full.rfind('\\') {
        Some(i) => &full[..i],
        None => return 0,
    };
    dirs.iter().position(|d| d == parent_path).map(|i| i as u32).unwrap_or(0)
}

fn split_even(items: &[u32], buckets: usize) -> Vec<Vec<u32>> {
    let n = buckets.max(1);
    let mut out: Vec<Vec<u32>> = vec![Vec::new(); n];
    for (i, item) in items.iter().enumerate() {
        out[i % n].push(*item);
    }
    out.retain(|v| !v.is_empty());
    out
}

// ---------------------------------------------------------------- 查询语法

/// 比较运算符。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Eq,
    Gt,
    Lt,
    Ge,
    Le,
}

/// 可限定的字段。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Name,
    Ext,
    Kind,
    Size,
    Date,
    Path,
}

/// 一条已解析的查询。
///
/// 语法（空格分隔，引号内视为整体）：
/// ```text
/// 报告                子串；多个词默认 AND
/// "年度 报告"          引号内含空格也当一个整体
/// a OR b              任一命中即可
/// -草稿               排除含"草稿"的
/// *.txt / item?.log   * 与 ? 通配
/// ext:txt             扩展名
/// kind:file|dir       类型（file/文件，dir/folder/目录/文件夹）
/// size:>10mb          大小（b/kb/mb/gb/tb，比较符 > < >= <=）
/// date:>2026-01-01    修改日期（也认 today / week）
/// path:github         完整路径里含
/// ```
#[derive(Clone, Debug, Default)]
pub struct Query {
    /// 必须全部命中（子串或通配，已小写）
    pub texts: Vec<String>,
    /// 任一命中即可
    pub any: Vec<String>,
    /// 都不许命中
    pub not: Vec<String>,
    pub ext: Option<String>,
    pub kind: Option<String>,
    pub size: Option<(Op, u64)>,
    /// FILETIME（100ns）
    pub date: Option<(Op, i64)>,
    pub path: Option<String>,
}

impl Query {
    pub fn is_empty(&self) -> bool {
        self.texts.is_empty()
            && self.any.is_empty()
            && self.not.is_empty()
            && self.ext.is_none()
            && self.kind.is_none()
            && self.size.is_none()
            && self.date.is_none()
            && self.path.is_none()
    }

    /// 按空格切词，引号内的空格不切。
    fn tokenize(s: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut quoted = false;
        let mut has = false;
        for c in s.chars() {
            match c {
                '"' => {
                    quoted = !quoted;
                    has = true;
                }
                ' ' | '\t' if !quoted => {
                    if has {
                        out.push(std::mem::take(&mut cur));
                        has = false;
                    }
                }
                _ => {
                    cur.push(c);
                    has = true;
                }
            }
        }
        if has {
            out.push(cur);
        }
        out
    }

    pub fn parse(s: &str) -> Query {
        let mut q = Query::default();
        let toks = Self::tokenize(s);
        let mut i = 0usize;
        // 待定的 OR 组：`a OR b OR c` 会在遇到下一个普通词时收束
        let mut or_group: Vec<String> = Vec::new();
        while i < toks.len() {
            let t = toks[i].clone();
            let lower = t.to_lowercase();
            if lower == "or" && !or_group.is_empty() {
                i += 1;
                continue;
            }
            if let Some((f, rest)) = split_field(&t) {
                match f {
                    Field::Name => q.texts.push(rest.to_lowercase()),
                    Field::Ext => q.ext = Some(rest.trim_start_matches('.').to_lowercase()),
                    Field::Kind => q.kind = Some(rest.to_lowercase()),
                    Field::Size => q.size = parse_size(&rest),
                    Field::Date => q.date = parse_date(&rest),
                    Field::Path => q.path = Some(rest.to_lowercase()),
                }
                i += 1;
                continue;
            }
            if let Some(v) = t.strip_prefix('-') {
                if !v.is_empty() {
                    q.not.push(v.to_lowercase());
                }
                i += 1;
                continue;
            }
            // 前面出现过 OR：这个词属于 OR 组
            let in_or = i > 0 && toks[i - 1].eq_ignore_ascii_case("or");
            if in_or || (i + 1 < toks.len() && toks[i + 1].eq_ignore_ascii_case("or")) {
                or_group.push(lower);
            } else if !or_group.is_empty() {
                or_group.push(lower);
            } else {
                q.texts.push(lower);
            }
            i += 1;
        }
        if or_group.len() > 1 {
            q.any = or_group;
        } else if let Some(v) = or_group.into_iter().next() {
            q.texts.push(v);
        }
        q
    }

    /// 名字至少要有这么长才可能命中（`*` 算 0 个字符，`?` 算 1 个）。
    ///
    /// 扫之前先用长度过滤一轮，77 万条里能直接跳掉九成以上 ——
    /// 这是"击键就有结果"的关键一步。
    pub fn min_name_len(&self) -> usize {
        let need = |t: &str| -> usize {
            if t.contains('*') || t.contains('?') {
                t.chars().filter(|c| *c != '*').count()
            } else {
                t.chars().count()
            }
        };
        // texts 是 AND：要满足最长的那个；any 是 OR：满足最短的那个即可
        let a = self.texts.iter().map(|t| need(t)).max().unwrap_or(0);
        let b = self.any.iter().map(|t| need(t)).min().unwrap_or(0);
        a.max(b)
    }

    fn matches_entry(&self, idx: &Index, i: usize) -> bool {
        let lb = idx.lower_at(i);
        for t in &self.texts {
            if !contains_bytes(lb, t) {
                return false;
            }
        }
        if !self.any.is_empty() && !self.any.iter().any(|t| contains_bytes(lb, t)) {
            return false;
        }
        for t in &self.not {
            if contains_bytes(lb, t) {
                return false;
            }
        }
        if let Some(ext) = &self.ext {
            // 直接看小写 blob 的尾巴。`name_at().to_lowercase()` 会**每条分配一次
            // String**，77 万条就是 77 万次堆分配 —— 实测这一个字段让查询从 6ms
            // 涨到 46ms，是"高级搜索很卡"的真凶。
            let want = ext.as_bytes();
            let lb = idx.lower_at(i);
            let need = want.len() + 1;
            if lb.len() < need
                || lb[lb.len() - want.len() - 1] != b'.'
                || &lb[lb.len() - want.len()..] != want
            {
                return false;
            }
        }
        if let Some(kind) = &self.kind {
            let is_dir = (idx.flags[i] & FLAG_DIR) != 0;
            let want_dir = matches!(kind.as_str(), "dir" | "folder" | "目录" | "文件夹");
            let want_file = matches!(kind.as_str(), "file" | "文件");
            if (want_dir && !is_dir) || (want_file && is_dir) {
                return false;
            }
        }
        if let Some((op, v)) = self.size {
            let s = idx.size[i];
            let ok = match op {
                Op::Eq => s == v,
                Op::Gt => s > v,
                Op::Lt => s < v,
                Op::Ge => s >= v,
                Op::Le => s <= v,
            };
            if !ok {
                return false;
            }
        }
        if let Some((op, v)) = self.date {
            let m = idx.mtime[i];
            let ok = match op {
                Op::Eq => m == v,
                Op::Gt => m > v,
                Op::Lt => m < v,
                Op::Ge => m >= v,
                Op::Le => m <= v,
            };
            if !ok {
                return false;
            }
        }
        if let Some(p) = &self.path {
            if !idx.path_of(i).to_string_lossy().to_lowercase().contains(p) {
                return false;
            }
        }
        true
    }
}

fn split_field(t: &str) -> Option<(Field, String)> {
    let i = t.find(':')?;
    if i == 0 {
        return None;
    }
    let key = t[..i].to_lowercase();
    let rest = t[i + 1..].to_string();
    if rest.is_empty() {
        return None;
    }
    let f = match key.as_str() {
        "name" | "名称" => Field::Name,
        "ext" | "扩展名" => Field::Ext,
        "kind" | "type" | "类型" => Field::Kind,
        "size" | "大小" => Field::Size,
        "date" | "modified" | "日期" => Field::Date,
        "path" | "路径" => Field::Path,
        _ => return None,
    };
    Some((f, rest))
}

/// `>10mb` / `<=512` / `1.5gb` → (Op, 字节数)
fn parse_size(s: &str) -> Option<(Op, u64)> {
    let s = s.trim();
    let (op, rest) = if let Some(v) = s.strip_prefix(">=") {
        (Op::Ge, v)
    } else if let Some(v) = s.strip_prefix("<=") {
        (Op::Le, v)
    } else if let Some(v) = s.strip_prefix('>') {
        (Op::Gt, v)
    } else if let Some(v) = s.strip_prefix('<') {
        (Op::Lt, v)
    } else {
        (Op::Eq, s)
    };
    let rest = rest.trim().to_lowercase();
    let (num, mul): (&str, u64) = if let Some(v) = rest.strip_suffix("tb") {
        (v, 1024 * 1024 * 1024 * 1024)
    } else if let Some(v) = rest.strip_suffix("gb") {
        (v, 1024 * 1024 * 1024)
    } else if let Some(v) = rest.strip_suffix("mb") {
        (v, 1024 * 1024)
    } else if let Some(v) = rest.strip_suffix("kb") {
        (v, 1024)
    } else if let Some(v) = rest.strip_suffix('b') {
        (v, 1)
    } else {
        (rest.as_str(), 1)
    };
    let n: f64 = num.trim().parse().ok()?;
    Some((op, (n * mul as f64) as u64))
}

/// 1601-01-01 → 1970-01-01，单位 100ns（FILETIME 与 unix 秒的换算基准）
const EPOCH_DIFF: i64 = 116_444_736_000_000_000;

/// `>2026-01-01` / `2026-10` / `today` / `week` → FILETIME
fn parse_date(s: &str) -> Option<(Op, i64)> {
    let s = s.trim();
    let (op, rest) = if let Some(v) = s.strip_prefix(">=") {
        (Op::Ge, v)
    } else if let Some(v) = s.strip_prefix("<=") {
        (Op::Le, v)
    } else if let Some(v) = s.strip_prefix('>') {
        (Op::Gt, v)
    } else if let Some(v) = s.strip_prefix('<') {
        (Op::Lt, v)
    } else {
        (Op::Ge, s)
    };
    let rest = rest.trim().to_lowercase();
    let now = now_secs() as i64;
    if rest == "today" || rest == "今天" {
        return Some((op, (now - now % 86400) * 10_000_000 + EPOCH_DIFF));
    }
    if rest == "week" || rest == "本周" || rest == "month" || rest == "本月" {
        return Some((op, (now - 7 * 86400) * 10_000_000 + EPOCH_DIFF));
    }
    let (y, m, d) = parse_ymd(&rest)?;
    let secs = civil_from_days(y, m, d) * 86400;
    Some((op, secs * 10_000_000 + EPOCH_DIFF))
}

fn parse_ymd(s: &str) -> Option<(i32, u32, u32)> {
    let parts: Vec<&str> = s.split(|c| c == '-' || c == '/' || c == '.').collect();
    let y: i32 = parts.first()?.parse().ok()?;
    let m: u32 = parts.get(1).and_then(|v| v.parse().ok()).unwrap_or(1);
    let d: u32 = parts.get(2).and_then(|v| v.parse().ok()).unwrap_or(1);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some((y, m, d))
}

/// 公历日期 → 自 1970-01-01 的天数（Howard Hinnant 的 civil_from_days）。
pub fn civil_from_days(y: i32, m: u32, d: u32) -> i64 {
    let y = y as i64 - (m as i64 <= 2) as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m as i64 + if m > 2 { -3 } else { 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

// ---------------------------------------------------------------- 字节级匹配

/// 名字已存成小写，这里直接按字节比。
fn contains_bytes(hay: &[u8], needle: &str) -> bool {
    let n = needle.as_bytes();
    if n.is_empty() {
        return true;
    }
    if n.contains(&b'*') || n.contains(&b'?') {
        return wild_bytes(hay, n);
    }
    if n.len() > hay.len() {
        return false;
    }
    // 首尾双字节过滤：hay.windows().any() 每格都要建切片再 memcmp，
    // 77 万条 × 平均 20 字节时这一处就是查询耗时的大头（实测 20ms → 6ms）。
    let (first, last) = (n[0], n[n.len() - 1]);
    let end = hay.len() - n.len();
    let mut i = 0usize;
    while i <= end {
        if hay[i] != first || hay[i + n.len() - 1] != last {
            i += 1;
            continue;
        }
        if &hay[i..i + n.len()] == n {
            return true;
        }
        i += 1;
    }
    false
}

fn starts_with_bytes(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() >= needle.len() && &hay[..needle.len()] == needle
}

fn wild_bytes(s: &[u8], pat: &[u8]) -> bool {
    let (mut si, mut pi, mut star, mut ss) = (0usize, 0usize, None::<usize>, 0usize);
    while si < s.len() {
        if pi < pat.len() && (pat[pi] == b'?' || pat[pi] == s[si]) {
            si += 1;
            pi += 1;
        } else if pi < pat.len() && pat[pi] == b'*' {
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
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
        Index::assemble(
            "C:\\t".to_string(),
            true,
            vec!["C:\\t".to_string(), "C:\\t\\sub".to_string()],
            vec!["a.txt".to_string(), "sub".to_string()],
            vec!["a.txt".to_string(), "sub".to_string()],
            vec![0, 0],
            vec![0, 0],
            vec![7, 0],
            vec![1, 2],
            vec![0, FLAG_DIR],
        )
    }

    /// 缓存必须能原样还原：`to_bytes` / `from_bytes` 只要有一处游标没推进，
    /// 症状就是"第二次搜索一个结果都没有"，而且只在有缓存时才出现。
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
        let idx = sample();
        assert_eq!(idx.query("*.txt", 10).len(), 1);
        assert_eq!(idx.query("a*.txt", 10).len(), 1);
    }

    #[test]
    fn advanced_syntax() {
        let q = Query::parse("report ext:txt size:>1kb -draft");
        assert_eq!(q.texts, vec!["report".to_string()]);
        assert_eq!(q.ext.as_deref(), Some("txt"));
        assert_eq!(q.not, vec!["draft".to_string()]);
        assert!(matches!(q.size, Some((Op::Gt, v)) if v == 1024));

        let q2 = Query::parse("\"a b\" c");
        assert_eq!(q2.texts, vec!["a b".to_string(), "c".to_string()]);

        let q3 = Query::parse("aa OR bb");
        assert_eq!(q3.any, vec!["aa".to_string(), "bb".to_string()]);

        let q4 = Query::parse("date:>2026-01-01 kind:file");
        assert!(q4.date.is_some());
        assert_eq!(q4.kind.as_deref(), Some("file"));
    }

    #[test]
    fn volume_roots() {
        assert_eq!(volume_root(Path::new("C:\\Users\\a")).as_deref(), Some("C:\\"));
        assert_eq!(volume_root(Path::new("d:\\")).as_deref(), Some("D:\\"));
        assert_eq!(volume_root(Path::new("\\\\srv\\share")), None);
        assert_eq!(volume_root(Path::new("relative")), None);
    }
}
