//! 自研文件传输引擎：多线程 + **单文件分块并行** + 实时进度。
//!
//! 为什么不再用 `IFileOperation`：
//!   * 它是**模态**的，`PerformOperations` 会自己泵消息 → 重入 WndProc →
//!     与调用方手里的 `&mut App` 撞成别名 UB（表现就是"一松手卡死"）；
//!   * 进度对话框由 Shell 提供，样式跟软件不一致，也拿不到"已用时间 / 预计剩余"；
//!   * 它按"文件"并行，一个 20GB 的大文件永远只有一条流在跑。
//!
//! 这里把工作拆到**块**的粒度：
//!   * 小文件（< `SPLIT_MIN`）整份丢给一个工作线程；
//!   * 大文件先预占目标文件的长度，再切成若干块，块之间并行；每块用
//!     `read_at` / `write_at`（Windows 上底层是带 OVERLAPPED 的 ReadFile/WriteFile，
//!     显式偏移，多线程各写各的区间不会互相干扰）。
//! 于是"复制一个 20GB 文件"也能吃满多个核。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use windows::Win32::Storage::FileSystem::{
    LPPROGRESS_ROUTINE_CALLBACK_REASON, PROGRESS_CANCEL, PROGRESS_CONTINUE,
};

/// 超过这个大小就把单个文件拆成多块并行传。
///
/// 拆分的收益要盖过"预占长度 + 多次 open"的开销，实测 8 MB 是个合适的门槛：
/// 再小的话单个文件本来几十毫秒就传完了，拆了反而慢。
pub const SPLIT_MIN: u64 = 8 * 1024 * 1024;
/// 单块大小。8 MB：NVMe 的 QD 深度靠**并发块数**就能填满，单块再大只会
/// 让每块的内存拷贝变久（见 `copy_range` 的说明）。
pub const CHUNK: u64 = 8 * 1024 * 1024;
/// 单块的**字节数上限**。任何一个块都不会超过它。
const MAX_CHUNK_BYTES: u64 = 64 * 1024 * 1024;
/// 旧的 `MAX_CHUNKS_PER_FILE = 64` 才是真凶：`ceil(size/CHUNK)` 被 clamp 到 64
/// 之后，**块的实际长度 = size/n 远大于 CHUNK** —— 4.7GB 文件 → 64 块 → 每块
/// 74MB（注释里写的却是 4MB）。每块都要 74MB 的分配 + 归零，几十个线程同时
/// 持有就是 GB 级工作集，全程颠 cache —— 这才是"大文件 CPU 跑满却不涨吞吐"的
/// 根源（实测 2.71 CPU-s/GiB，Explorer 只要 0.38）。
///
/// 现在块长**恒为 CHUNK**（末块吃余数），块数自然等于 size/CHUNK。
/// 曾经担心的"几 TB 文件把队列撑爆"不成立：`Job` 约 24 字节，
/// 1 TiB 文件 = 131072 块 = 3MB 队列，只有文件大小的 0.3%。
const _: () = assert!(CHUNK <= MAX_CHUNK_BYTES);

/// 工作线程的拷贝缓冲。**每个线程一块，跨所有块复用**（见 `Porter`）：
/// 8 MB × 32 线程 = 256MB 常驻。
const WORKER_BUF: usize = 8 * 1024 * 1024;
const _: () = assert!(WORKER_BUF as u64 >= CHUNK);

/// 打开文件时一律带上这个共享模式：`FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE`。
///
/// **不能省**。Rust 默认只给读打开带共享，写打开不带 `FILE_SHARE_WRITE` ——
/// 于是"多个块并行写同一个文件"时，第一个线程拿到句柄之后其余线程再 `open`
/// 就是 `ERROR_ACCESS_DENIED (os error 5)`。实测 5 块只能成功 1 块。
const SHARE_ALL: u32 = 0x0000_0001 | 0x0000_0002 | 0x0000_0004;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Copy,
    Move,
}

/// 目标已存在时的处理。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Conflict {
    /// 自动改名成"X - 副本"（与资源管理器中文界面一致）。`IFileOperation` 的
    /// `CopyItem` 传 NULL 只会得到 "X (2)"，所以这里自己拼。
    Rename,
    /// 直接覆盖
    Replace,
    /// 跳过
    Skip,
}

#[derive(Default, Clone)]
pub struct Stats {
    pub files_done: usize,
    pub files_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// 正在传的文件名（只是名字，不是全路径 —— 弹窗里放不下全路径）
    pub current: String,
    /// 已经跑了多少毫秒
    pub elapsed_ms: f64,
    /// 按当前速度估算还要多少毫秒；速度为 0 时是 f64::INFINITY
    pub eta_ms: f64,
    /// 平滑后的速度（字节/秒）
    pub speed: f64,
    pub renamed: usize,
    pub skipped: usize,
    pub failed: usize,
    pub canceled: bool,
    pub finished: bool,
    pub error: Option<String>,
}

impl Stats {
    /// 剩余文件数。
    pub fn files_left(&self) -> usize {
        self.files_total.saturating_sub(self.files_done)
    }
    /// 0.0~1.0。总量为 0（全是空文件/空目录）时按文件数算。
    ///
    /// **只有 `finished` 才允许返回 1.0**。规划阶段（还没统计出总量）本来就是
    /// 0/0，若按"分母为 0 就算完成"处理，弹窗第一帧会先画一条**满格**的进度条
    /// 再跳回 0 —— 看着像"刚点粘贴就已经传完了"。
    pub fn progress(&self) -> f64 {
        if self.finished {
            return 1.0;
        }
        if self.bytes_total == 0 {
            if self.files_total == 0 {
                return 0.0;
            }
            return (self.files_done as f64 / self.files_total as f64).clamp(0.0, 1.0);
        }
        (self.bytes_done as f64 / self.bytes_total as f64).clamp(0.0, 1.0)
    }
}

pub type Shared = Arc<Mutex<Stats>>;

/// 一份工作：要么建目录，要么传一段字节。
enum Job {
    Dir(PathBuf),
    /// 小文件：整份传
    Whole { file: usize },
    /// 大文件的一块
    Chunk { file: usize, offset: u64, len: u64 },
    /// **超大单文件**：整份交给内核 `CopyFileEx`（带进度回调 + 取消）。
    ///
    /// 为什么不用分块：用户态的「读进缓冲 → 写出」是**双拷贝**，还要额外付
    /// 每次 `seek_read`/`seek_write` 的系统调用与 Cc 页管理。实测同一份
    /// 4.7GiB ISO（E:→D:，无缓存读回口径，交错 A/B ×2）：
    ///
    /// | 路径 | 落盘耗时 | 吞吐 | CPU/GiB |
    /// |---|---:|---:|---:|
    /// | `std::fs::copy` 单线程（内核态） | **7.72 / 8.09 s** | **623 / 594 MB/s** | **0.42 / 0.41** |
    /// | 自研分块 32 线程 | 11.15 / 11.84 s | 432 / 406 MB/s | 1.42 / 1.37 |
    ///
    /// 内核路径**快 45%、CPU 少 3.4 倍**，0.41 的 CPU/GiB 也基本就是
    /// Explorer 的 0.38。所以并行分块对单个大文件是**负优化** ——
    /// 真正的瓶颈从来不是线程数或块长，而是「用户态中转」这件事本身。
    ///
    /// 注意内核态 ≠ 丢功能：`CopyFileExW` 自带 `LPPROGRESS_ROUTINE`
    /// （进度）与 `pbcancel`（取消），正好是引擎相对 `fs::copy` 的价值所在。
    Big { file: usize },
}

/// 超过这个大小就交给内核整份复制（见 `Job::Big`）。
///
/// 取 256 MB：实测 32 线程分块在 4.7GiB 上比内核慢 45%，但分块在**多文件**
/// 场景仍有价值（每个文件一段 IO 上下文，多文件并发才能填满队列深度）。
/// 单文件越大，分块越无意义，所以只在单文件足够大时才切内核路径。
pub const KERNEL_MIN: u64 = 256 * 1024 * 1024;

/// 一个待传文件。
struct FTask {
    src: PathBuf,
    dst: PathBuf,
    size: u64,
    /// 还没传完的块数；归零才算"这个文件做完了"
    left: Arc<AtomicUsize>,
    /// 这个文件有没有失败过。Move 删源必须看它 —— 只看 `left==0` 会把
    /// 复制失败的源也删掉（失败路径同样 fetch_sub，left 一样归零）。
    ok: Arc<AtomicBool>,
}

/// 传输任务句柄：拿着它能看进度、能取消。
pub struct Transfer {
    pub stats: Shared,
    pub cancel: Arc<AtomicBool>,
}

/// 把 `items` 搬/复制到 `dest`，立刻返回句柄。
///
/// 冲突默认按 `Conflict::Rename`（与资源管理器一致：就地粘贴会生成"X - 副本"）。
pub fn start(kind: Kind, dest: &Path, items: Vec<PathBuf>, threads: usize) -> Transfer {
    start_with(kind, dest, items, threads, Conflict::Rename)
}

pub fn start_with(
    kind: Kind,
    dest: &Path,
    items: Vec<PathBuf>,
    threads: usize,
    on_conflict: Conflict,
) -> Transfer {
    let stats: Shared = Arc::new(Mutex::new(Stats::default()));
    let cancel = Arc::new(AtomicBool::new(false));
    let handle = Transfer {
        stats: stats.clone(),
        cancel: cancel.clone(),
    };
    let dest = dest.to_path_buf();
    std::thread::spawn(move || {
        run(kind, &dest, items, threads, on_conflict, stats, cancel);
    });
    handle
}

/// 规划一个 `size` 字节的文件该拆成哪些块。返回 `[(offset, len)]`。
///
/// **保证**（有单测钉住）：
///   * 段首尾相接、覆盖 `[0, size)`，无缝隙无重叠（否则丢数据）；
///   * 每块恒为 `CHUNK`，只有末块吃余数 —— **没有任何块超过 `MAX_CHUNK_BYTES`**。
///
/// **不要在这里按"块数"向下 clamp。**旧版是 `ceil(size/CHUNK).clamp(2, 64)`，
/// 4.7GB 文件因此被压成 64 块、每块 74MB，一块就要分配+归零 74MB 缓冲，
/// 几十个线程并发 = GB 级工作集颠 cache，CPU 跑满却涨不动吞吐
/// （实测 2.71 CPU-s/GiB，Explorer 只要 0.38）。并行度由 worker 线程数给足，
/// 不需要靠"块少而大"来实现。
fn plan_chunks(size: u64) -> Vec<(u64, u64)> {
    if size == 0 {
        return Vec::new();
    }
    // 块长恒为 CHUNK，只有末块吃余数 —— 于是块数就是向上取整的块数，
    // 天然等于"最自然的切法"。旧版为了让块数落在 [2,64] 才引入均分，
    // 均分正是块长失控的来源。
    let n = (size + CHUNK - 1) / CHUNK;
    let mut out = Vec::with_capacity(n as usize);
    let mut off = 0u64;
    for _ in 0..n.saturating_sub(1) {
        out.push((off, CHUNK));
        off += CHUNK;
    }
    // 末块吃掉全部余数。如果余数大到超过 MAX_CHUNK_BYTES（>1 TiB 的文件才会
    // 走到这），就地把它切成若干个不超过上限的块，不去突破上限。
    let mut rest = size - off;
    while rest > MAX_CHUNK_BYTES {
        out.push((off, MAX_CHUNK_BYTES));
        off += MAX_CHUNK_BYTES;
        rest -= MAX_CHUNK_BYTES;
    }
    out.push((off, rest));
    debug_assert_eq!(off + rest, size, "块必须正好覆盖整个文件");
    debug_assert!(
        out.iter().all(|(_, l)| *l <= MAX_CHUNK_BYTES),
        "不允许任何块超过上限"
    );
    out
}

/// 只跑"规划"阶段，返回 `(目录数, 文件数, 总字节)`。
///
/// 这是"复制一个 N 万条目的目录"里**唯一会卡住界面**的那一段。
/// 独立出来是为了让基准能直接调用生产这一条路 ——
/// **基准必须打的就是生产实现**，否则候选方案和落地代码会各跑各的，
/// 测出 86 倍却发现产品还是 3.25s。
pub fn plan_only(
    items: &[PathBuf],
    dest: &Path,
    on_conflict: Conflict,
) -> (usize, usize, u64) {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut bytes = 0u64;
    let mut renamed = 0usize;
    let mut skipped = 0usize;
    let mut taken: HashSet<PathBuf> = HashSet::new();
    let mut cache = ExistCache::new();
    for src in items {
        collect(
            src,
            dest,
            &mut dirs,
            &mut files,
            &mut bytes,
            on_conflict,
            &mut renamed,
            &mut skipped,
            &mut taken,
            &mut cache,
        );
    }
    (dirs.len(), files.len(), bytes)
}

fn run(
    kind: Kind,
    dest: &Path,
    items: Vec<PathBuf>,
    threads: usize,
    on_conflict: Conflict,
    stats: Shared,
    cancel: Arc<AtomicBool>,
) {
    let t0 = Instant::now();
    // 0) **同卷移动走 rename**：整棵子树一次改目录项就位，几 GB 也是毫秒级，
    //    跟资源管理器一个观感。跨卷才需要真的搬字节。
    //    rename 失败的（目标被别的进程占用、跨了连接点）退回慢路径，不会丢东西。
    let mut items = items;
    let mut renamed = 0usize;
    let mut skipped = 0usize;
    let mut fast_bytes = 0u64;
    let mut fast_files = 0usize;
    // **本批已分配的目标路径**：同批两个 `photo.jpg` 会被 resolve 成同一个
    // 目标，而磁盘 exists() 查不到这个还没落盘的名字 —— Move 时两份源先
    // 互相覆盖、再双双删源，两个都没了只剩一份（第39轮审计 #3）。
    let mut taken: HashSet<PathBuf> = HashSet::new();
    if kind == Kind::Move {
        let mut slow: Vec<PathBuf> = Vec::with_capacity(items.len());
        for src in &items {
            if !same_volume(src, dest) {
                slow.push(src.clone());
                continue;
            }
            let Some(name) = src.file_name().map(|n| n.to_string_lossy().to_string()) else {
                continue;
            };
            let Some(target) =
                resolve_conflict(
                    &dest.join(&name),
                    on_conflict,
                    &mut renamed,
                    &mut skipped,
                    &mut taken,
                )
            else {
                continue;
            };
            // 先量尺寸：rename 之后源就没了，量不到
            let (sz, nf) = subtree_stats(src);
            match std::fs::rename(src, &target) {
                Ok(()) => {
                    fast_bytes += sz;
                    fast_files += nf.max(1);
                }
                // 最常见的是"目标被占用"或"跨卷连接点"：退回复制+删源
                Err(_) => slow.push(src.clone()),
            }
        }
        items = slow;
    }
    // 1) 展开成"要建的目录 + 要传的文件"
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut files: Vec<FTask> = Vec::new();
    let mut bytes_total = 0u64;
    let mut cache = ExistCache::new();
    for src in &items {
        collect(
            src,
            dest,
            &mut dirs,
            &mut files,
            &mut bytes_total,
            on_conflict,
            &mut renamed,
            &mut skipped,
            &mut taken,
            &mut cache,
        );
    }
    {
        let mut s = stats.lock().unwrap();
        s.files_total = files.len() + fast_files;
        s.bytes_total = bytes_total + fast_bytes;
        // 快路径那部分在树还没铺开时就做完了，这里直接记进已完成
        s.files_done = fast_files;
        s.bytes_done = fast_bytes;
        s.renamed = renamed;
        s.skipped = skipped;
    }
    if files.is_empty() && dirs.is_empty() {
        let mut s = stats.lock().unwrap();
        s.finished = true;
        s.elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        return;
    }

    // 2) 把文件拆成块，铺成一个扁平队列
    let mut jobs: Vec<Job> = Vec::with_capacity(dirs.len() + files.len() * 2);
    for d in dirs {
        jobs.push(Job::Dir(d));
    }
    for (fi, f) in files.iter().enumerate() {
        if f.size >= KERNEL_MIN {
            // 超大单文件：内核整份复制（Job::Big 的注释有实测数据）
            f.left.store(1, Ordering::Relaxed);
            jobs.push(Job::Big { file: fi });
        } else if f.size >= SPLIT_MIN {
            let plan = plan_chunks(f.size);
            f.left.store(plan.len(), Ordering::Relaxed);
            for (offset, len) in plan {
                jobs.push(Job::Chunk { file: fi, offset, len });
            }
        } else {
            f.left.store(1, Ordering::Relaxed);
            jobs.push(Job::Whole { file: fi });
        }
    }

    // 2.5) 要拆块的文件：**规划阶段**就把目标文件建好并预占到最终长度。
    // 不能让"第一个到达的块"负责创建 —— 那样 N 个块会并发去 create/set_len，
    // 既有可能互相抢，也让"谁先到"变成不确定的行为。
    let mut failed_pre = 0usize;
    for f in files.iter() {
        // **只预建分块的文件**。`Job::Big` 走内核 CopyFileEx，它自己会建目标；
        // 我们要是抢先 create+truncate 一个同长度的空壳，内核会把它当成
        // "目标已存在"而失败。
        if f.size < SPLIT_MIN || f.size >= KERNEL_MIN {
            continue;
        }
        if let Some(p) = f.dst.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        use std::os::windows::fs::OpenOptionsExt;
        let r = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(SHARE_ALL)
            .open(&f.dst)
            .and_then(|h| h.set_len(f.size));
        if let Err(e) = r {
            let mut s = stats.lock().unwrap();
            if s.error.is_none() {
                s.error = Some(format!("预建 {} 失败：{e}", f.dst.display()));
            }
            failed_pre += 1;
        }
    }
    if failed_pre > 0 {
        // 预建没成 → 这些文件退回"整份传"，别把文件写成半截
        let big: Vec<usize> = files
            .iter()
            .enumerate()
            .filter(|(_, f)| f.size >= SPLIT_MIN && f.size < KERNEL_MIN)
            .map(|(i, _)| i)
            .collect();
        jobs.retain(|j| !matches!(j, Job::Chunk { .. }));
        for i in big {
            files[i].left.store(1, Ordering::Relaxed);
            jobs.push(Job::Whole { file: i });
        }
    }

    // 3) 工作线程拉队列
    let jobs = Arc::new(jobs);
    let files = Arc::new(files);
    let cursor = Arc::new(AtomicU64::new(0));
    let done_bytes = Arc::new(AtomicU64::new(fast_bytes));
    let done_files = Arc::new(AtomicUsize::new(fast_files));
    let failed = Arc::new(AtomicUsize::new(0));
    let cur_name = Arc::new(Mutex::new(String::new()));
    let nthreads = threads.clamp(1, 64).min(jobs.len().max(1));
    let mut handles = Vec::with_capacity(nthreads);
    for _ in 0..nthreads {
        let jobs = jobs.clone();
        let files = files.clone();
        let cursor = cursor.clone();
        let stats = stats.clone();
        let cancel = cancel.clone();
        let done_bytes = done_bytes.clone();
        let done_files = done_files.clone();
        let failed = failed.clone();
        let cur_name = cur_name.clone();
        handles.push(std::thread::spawn(move || {
            let mut porter = Porter::new();
            while !cancel.load(Ordering::Relaxed) {
                let i = cursor.fetch_add(1, Ordering::Relaxed) as usize;
                if i >= jobs.len() {
                    break;
                }
                match &jobs[i] {
                    Job::Dir(d) => {
                        if let Err(e) = std::fs::create_dir_all(d) {
                            failed.fetch_add(1, Ordering::Relaxed);
                            let mut s = stats.lock().unwrap();
                            if s.error.is_none() {
                                s.error = Some(format!("创建目录失败 {}: {e}", d.display()));
                            }
                        }
                    }
                    Job::Whole { file } => {
                        let f = &files[*file];
                        set_cur(&cur_name, &stats, &f.src);
                        match std::fs::copy(&f.src, &f.dst) {
                            Ok(n) => {
                                bump(&stats, &done_bytes, n, &f.left, &done_files, &cur_name);
                            }
                            Err(e) => {
                                failed.fetch_add(1, Ordering::Relaxed);
                                file_failed(&stats, &f.src, &e, &f.left, &f.ok, &done_files);
                            }
                        }
                    }
                    Job::Big { file } => {
                        let f = &files[*file];
                        set_cur(&cur_name, &stats, &f.src);
                        if let Some(p) = f.dst.parent() {
                            let _ = std::fs::create_dir_all(p);
                        }
                        match copy_kernel(&f.src, &f.dst, &done_bytes, &cancel) {
                            Ok(n) => {
                                // **绝不能走 `bump`**：`bump` 会
                                // `done_bytes.fetch_add(n)`，而 progress_cb
                                // 已经把增量逐段记进同一个 done_bytes 了。
                                // 这里再整体加一遍 = 进度按 2 倍速跑，
                                // 统计字节数直接翻倍（GUI 因为 clamp 到 1.0
                                // 看不出来，内部 stats 是错的）。
                                // 只收尾文件计数：`left` 归零才算这个文件做完。
                                finish_file(&stats, &f.left, &done_files);
                            }
                            Err(e) => {
                                failed.fetch_add(1, Ordering::Relaxed);
                                file_failed(&stats, &f.src, &e, &f.left, &f.ok, &done_files);
                            }
                        }
                    }
                    Job::Chunk { file, offset, len } => {
                        let f = &files[*file];
                        set_cur(&cur_name, &stats, &f.src);
                        match porter.copy_range(&f.src, &f.dst, *offset, *len, f.size) {
                            Ok(n) => {
                                bump(&stats, &done_bytes, n, &f.left, &done_files, &cur_name);
                            }
                            Err(e) => {
                                failed.fetch_add(1, Ordering::Relaxed);
                                file_failed(&stats, &f.src, &e, &f.left, &f.ok, &done_files);
                            }
                        }
                    }
                }
            }
        }));
    }
    // 进度（时间 / 速度 / 剩余）在这个线程里按固定周期刷新，
    // 让工作线程只管搬字节，不去抢 stats 的锁。
    let mut last_bytes = 0u64;
    let mut speed = 0.0f64;
    let mut last_t = t0;
    loop {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let alive = handles.iter().any(|_| true) && !cursor_done(&cursor, jobs.len());
        let now = Instant::now();
        let b = done_bytes.load(Ordering::Relaxed);
        let dt = (now - last_t).as_secs_f64();
        if dt > 0.0 {
            let inst = (b.saturating_sub(last_bytes)) as f64 / dt;
            // 指数平滑：瞬时速度抖得厉害，ETA 跟着跳会很难看
            speed = if speed <= 0.0 { inst } else { speed * 0.7 + inst * 0.3 };
            last_bytes = b;
            last_t = now;
        }
        {
            let mut s = stats.lock().unwrap();
            s.bytes_done = b;
            s.files_done = done_files.load(Ordering::Relaxed);
            s.failed = failed.load(Ordering::Relaxed);
            s.elapsed_ms = (now - t0).as_secs_f64() * 1000.0;
            s.speed = speed;
            s.eta_ms = if speed > 1.0 {
                (s.bytes_total.saturating_sub(b)) as f64 / speed * 1000.0
            } else {
                f64::INFINITY
            };
            s.current = cur_name.lock().unwrap().clone();
        }
        if !alive {
            break;
        }
        if cancel.load(Ordering::Relaxed) {
            break;
        }
    }
    for h in handles {
        let _ = h.join();
    }

    // 4) 移动：源与目标是同一卷时本来就是 rename，跨卷才需要"复制完删源"。
    //    只删**确实搬成功了**的那些：失败的文件留在原处让用户自己判断。
    if kind == Kind::Move && !cancel.load(Ordering::Relaxed) {
        for f in files.iter() {
            // 三重条件缺一不可：块走完（left==0）**且**没失败过（ok）**且**
            // 目标真的在。只看 left 的旧版会把复制失败的源删掉 —— 失败路径
            // 同样 fetch_sub，left 一样归零，源消失、目标是半截文件。
            if f.left.load(Ordering::Relaxed) == 0
                && f.ok.load(Ordering::Relaxed)
                && f.dst.is_file()
            {
                let _ = std::fs::remove_file(&f.src);
            }
        }
        // 顶层目录：文件删完之后把空壳收掉。但只要有任何一个文件/预建失败，
        // 源树里就还留着没搬走的东西 —— 这时整棵 remove_dir_all 会把用户
        // 唯一的数据一起删掉，必须先确认零失败。
        if failed.load(Ordering::Relaxed) == 0 && failed_pre == 0 {
            for src in &items {
                if src.is_dir() && !same_volume(src, dest) {
                    let _ = std::fs::remove_dir_all(src);
                }
            }
        }
    }
    let mut s = stats.lock().unwrap();
    s.bytes_done = done_bytes.load(Ordering::Relaxed);
    s.files_done = done_files.load(Ordering::Relaxed);
    s.failed = failed.load(Ordering::Relaxed);
    s.elapsed_ms = (Instant::now() - t0).as_secs_f64() * 1000.0;
    s.canceled = cancel.load(Ordering::Relaxed);
    s.finished = true;
}

fn cursor_done(cursor: &AtomicU64, n: usize) -> bool {
    cursor.load(Ordering::Relaxed) as usize >= n
}

fn set_cur(cur: &Arc<Mutex<String>>, stats: &Shared, src: &Path) {
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut c = cur.lock().unwrap();
    if *c != name {
        *c = name.clone();
        if let Ok(mut s) = stats.lock() {
            s.current = name;
        }
    }
}

fn bump(
    stats: &Shared,
    done_bytes: &Arc<AtomicU64>,
    n: u64,
    left: &Arc<AtomicUsize>,
    done_files: &Arc<AtomicUsize>,
    _cur: &Arc<Mutex<String>>,
) {
    done_bytes.fetch_add(n, Ordering::Relaxed);
    finish_file(stats, left, done_files);
}

/// 只做"这个文件收尾"的计数，**不动** `done_bytes`。
///
/// 分两条路是因为字节入账的时机不同：分块路径由调用方在搬完后一次性报增量；
/// 内核 `CopyFileExW` 路径的进度回调**已经**把增量记进 `done_bytes`，
/// 再经 `bump` 加一遍就是重复统计。
fn finish_file(stats: &Shared, left: &Arc<AtomicUsize>, done_files: &Arc<AtomicUsize>) {
    if left.fetch_sub(1, Ordering::Relaxed) == 1 {
        done_files.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut s) = stats.lock() {
            s.files_done = done_files.load(Ordering::Relaxed);
        }
    }
}

fn file_failed(
    stats: &Shared,
    src: &Path,
    e: &std::io::Error,
    left: &Arc<AtomicUsize>,
    ok: &Arc<AtomicBool>,
    done_files: &Arc<AtomicUsize>,
) {
    if left.fetch_sub(1, Ordering::Relaxed) == 1 {
        done_files.fetch_add(1, Ordering::Relaxed);
    }
    // 标记这个文件失败了：Move 的删源阶段只看 left 会把它也当成"搬完了"
    ok.store(false, Ordering::Relaxed);
    if let Ok(mut s) = stats.lock() {
        if s.error.is_none() {
            s.error = Some(format!("{}: {e}", src.display()));
        }
    }
}

/// 通过**回调**把进度写进 `done_bytes`，并让取消能立刻中断内核复制。
///
/// 回调拿到的是裸指针，所以这里用 `catch_unwind` 兜住 panic ——
///  unwind 穿过 extern "system" 边界在 Windows 上是未定义行为。
/// `panic = "abort"` 的 release 构建下 catch_unwind 不会拦住，
/// 但那也意味着不会跨 FFI unwind，两种配置都安全。
unsafe extern "system" fn progress_cb(
    _total_size: i64,
    total_transferred: i64,
    _stream_size: i64,
    _stream_transferred: i64,
    _stream_num: u32,
    _reason: LPPROGRESS_ROUTINE_CALLBACK_REASON,
    _h_src: windows::Win32::Foundation::HANDLE,
    _h_dst: windows::Win32::Foundation::HANDLE,
    data: *const core::ffi::c_void,
) -> windows::Win32::Storage::FileSystem::COPYPROGRESSROUTINE_PROGRESS {
    let ctx = unsafe { &*(data as *const KernelCtx) };
    let n = total_transferred.max(0) as u64;
    let delta = {
        let _g = ctx.mutex.lock();
        // done 累加增量：内核给的是累计值，引擎的 done_bytes 也是累计值
        let d = n.saturating_sub(ctx.last.get());
        ctx.last.set(n);
        d
    };
    ctx.done.fetch_add(delta, Ordering::Relaxed);
    // 取消：内核看到 `PROGRESS_CANCEL`(1) / `PROGRESS_STOP`(2) 就中止复制。
    // ⚠️ **常量方向很容易写反**：CONTINUE=0、CANCEL=1、STOP=2、QUIET=3。
    // 写反的后果特别隐蔽 —— 返回 1 会让内核在第一次回调就中止，
    // 表现为"复制 0.1 秒就 ERROR_REQUEST_ABORTED，文件根本没建"，
    // 看起来像引擎崩了，其实是回调语义反了。用命名常量，别用裸数字。
    if ctx.cancel.load(Ordering::Relaxed) {
        PROGRESS_CANCEL
    } else {
        PROGRESS_CONTINUE
    }
}

/// 进度回调的上下文。**必须 `Send + Sync`**：它以裸指针跨 FFI 传递。
struct KernelCtx {
    done: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
    /// 上次回调时的累计字节，用来把内核的累计值换算成增量。
    /// 只在持有 `mutex` 时读写。
    last: std::cell::Cell<u64>,
    /// 内核回调可能来自不同线程，别让两个回调同时改 `last`。
    /// 用 `Cell`（非原子）是为了让"读—改—写"成为一个临界区，
    /// 单靠 AtomicU64 的 load/store 挡不住交错。
    mutex: Mutex<()>,
}

/// 整份交给内核 `CopyFileExW`（见 `Job::Big` 的实测数据）。
///
/// 进度通过 `LPPROGRESS_ROUTINE` 回调累加进 `done_bytes`，
/// 取消通过同一个回调返回 0 中止 —— 两者都不需要额外线程轮询。
fn copy_kernel(
    src: &Path,
    dst: &Path,
    done_bytes: &Arc<AtomicU64>,
    cancel: &Arc<AtomicBool>,
) -> std::io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{CopyFileExW, COPY_FILE_FAIL_IF_EXISTS};

    let mut s: Vec<u16> = src.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut d: Vec<u16> = dst.as_os_str().encode_wide().chain(Some(0)).collect();
    let ctx = KernelCtx {
        done: done_bytes.clone(),
        cancel: cancel.clone(),
        last: std::cell::Cell::new(0),
        mutex: Mutex::new(()),
    };
    // CopyFileExW 要求 lpData 原样回传给我们自己的回调，
    // 这里借用 ctx 的地址；CopyFileExW 是同步的，返回前ctx 一定还活着。
    let lpdata = (&ctx as *const KernelCtx) as *const core::ffi::c_void;
    let r = unsafe {
        CopyFileExW(
            PCWSTR(s.as_ptr()),
            PCWSTR(d.as_ptr()),
            // LPPROGRESS_ROUTINE 是函数**类型**别名，不是值 —— 直接传函数名
            Some(progress_cb),
            Some(lpdata),
            None,
            COPY_FILE_FAIL_IF_EXISTS,
        )
    };
    // **先判取消再传播错误**：回调返回 0 时 CopyFileEx 自己也会失败，
    // 要是先用 `?` 抛出去，一个用户主动的"停止"就会被记成硬失败
    // （Move 模式下会把源标成 ok=false —— 方向是对的，但错误文案会变成
    //  "某个 IO 错误"而不是"已取消"，且 failed 计数虚高）。
    if cancel.load(Ordering::Relaxed) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "已取消",
        ));
    }
    r.map_err(|e| std::io::Error::from_raw_os_error(e.code().0))?;
    // 回调可能没跑满（极小文件某些情况不回调），补齐差额
    let want = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
    let got = ctx.last.get();
    if want > got {
        done_bytes.fetch_add(want - got, Ordering::Relaxed);
        return Ok(want);
    }
    Ok(got)
}

/// 一个工作线程的私有状态：**缓冲只分配一次**，跨所有块复用。
///
/// 旧实现每个块都 `vec![0u8; len]` —— 4.7GB 文件分 600 块就是反复
/// 分配+归零 4.8GB 内存，分配器碎片 + memset 白烧 CPU。复用之后
/// 每线程只归零一次 `WORKER_BUF`，之后再不碰分配器。
struct Porter {
    buf: Vec<u8>,
}

impl Porter {
    fn new() -> Self {
        Porter {
            // 一次性归零，之后整块复用
            buf: vec![0u8; WORKER_BUF],
        }
    }

    /// 传 `[offset, offset+len)` 这一段。目标文件在规划阶段已预占到 `total` 长度。
    ///
    /// `len` 可以大于缓冲（`plan_chunks` 只保证 ≤ `MAX_CHUNK_BYTES`）：
    /// 这种情况按缓冲大小切片流式过，不会去分配更大的内存。
    fn copy_range(
        &mut self,
        src: &Path,
        dst: &Path,
        offset: u64,
        len: u64,
        total: u64,
    ) -> std::io::Result<u64> {
        // Windows 上这套叫 seek_read / seek_write（Unix 才叫 read_at / write_at）。
        // 底层是带 OVERLAPPED 的 ReadFile/WriteFile，走显式偏移，不动文件指针，
        // 所以多个块可以并行写同一个文件的不同区间。
        use std::os::windows::fs::{FileExt, OpenOptionsExt};
        // **必须显式开共享模式**：Rust 默认的写打开不带 FILE_SHARE_WRITE，
        // 一个线程占着目标文件之后其余块的线程再 open 就是 ERROR_ACCESS_DENIED。
        let sf = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(SHARE_ALL)
            .open(src)?;
        let df = std::fs::OpenOptions::new()
            .write(true)
            .share_mode(SHARE_ALL)
            .open(dst)?;
        // 兜底：万一预建那一步被跳过（比如目标已存在但比源短），这里补一次
        // （`File::set_len` 收&self，所以句柄本身不用 mut）
        if df.metadata()?.len() < total {
            df.set_len(total)?;
        }
        let mut done = 0u64;
        while done < len {
            let want = (len - done).min(self.buf.len() as u64) as usize;
            // seek_read 允许短读，循环补齐这一段
            let mut got = 0usize;
            let mut roff = offset + done;
            while got < want {
                let n = sf.seek_read(&mut self.buf[got..want], roff)?;
                if n == 0 {
                    break;
                }
                got += n;
                roff += n as u64;
            }
            if got == 0 {
                // 源已到头（文件被截短了）：如实返回已搬的量，交给上层记失败
                break;
            }
            let mut w = 0usize;
            while w < got {
                w += df.seek_write(&self.buf[w..got], offset + done + w as u64)?;
            }
            done += got as u64;
        }
        Ok(done)
    }
}

/// 冲突决议时用来判"目标已存在"的目录快照缓存。
///
/// **为什么需要它**：`resolve_conflict` 原本对每个条目调一次 `Path::exists()`。
/// 那是一次完整的文件系统查询（`GetFileAttributesW`，还要过一遍杀软过滤驱动），
/// 10 万条目就是 10 万次。实测这一项独占规划阶段 0.94s 里的 ~0.9s，
/// 而纯枚举只要 0.03s —— 也就是说"扫描慢"根本不是枚举慢，是**存在性查询慢**。
///
/// **为什么缓存是安全的**：一次规划期间我们只往目标目录里**新增**文件，
/// 不删不改别人的东西；而 `taken` 已经负责处理"本批内部撞名"。
/// 唯一会读到陈旧信息的场景是"扫描过程中有别的进程在目标目录建了同名文件" ——
/// 那本来就是个竞态，真发生了也只能靠后续 `create` 失败兜底，
/// 而这里预判出来只影响**名字选得不同**，不会丢数据。
///
/// 按目录缓存（不是全局一个 set）：源树的每个子目录对应目标树的一个子目录，
/// 各扫一次即可，每个目录只付一次 `read_dir` 而不是每条目一次 `exists`。
struct ExistCache {
    /// 目录路径（小写，用于 Windows 上大小写不敏感）→ 该目录下的文件名集合
    dirs: HashMap<String, HashSet<String>>,
}

impl ExistCache {
    fn new() -> Self {
        Self { dirs: HashMap::new() }
    }

    /// `name` 在 `parent` 下是否已存在（大小写不敏感，与 NTFS 一致）。
    fn contains(&mut self, parent: &Path, name: &str) -> bool {
        let key = parent.to_string_lossy().to_lowercase();
        let set = self.dirs.entry(key).or_insert_with(|| {
            std::fs::read_dir(parent)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.file_name().to_string_lossy().to_lowercase())
                        .collect()
                })
                .unwrap_or_default()
        });
        set.contains(&name.to_lowercase())
    }
}

/// 展开一个源路径：目录递归成"目录清单 + 文件清单"。
#[allow(clippy::too_many_arguments)]
fn collect(
    src: &Path,
    dest: &Path,
    dirs: &mut Vec<PathBuf>,
    files: &mut Vec<FTask>,
    bytes_total: &mut u64,
    on_conflict: Conflict,
    renamed: &mut usize,
    skipped: &mut usize,
    taken: &mut HashSet<PathBuf>,
    cache: &mut ExistCache,
) {
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if name.is_empty() {
        return;
    }
    let want = dest.join(&name);
    let exists = cache.contains(want.parent().unwrap_or(dest), &name);
    let target = resolve_conflict_with(
        &want,
        exists,
        on_conflict,
        renamed,
        skipped,
        taken,
    );
    let Some(target) = target else { return };

    let Ok(md) = src.symlink_metadata() else { return };
    if md.is_dir() {
        dirs.push(target.clone());
        let Ok(rd) = std::fs::read_dir(src) else { return };
        // **不要**在这里对每个条目再 `entry.path().symlink_metadata()`。
        // `read_dir` 在 Windows 上走 FindFirstFile/FindNextFile，返回的
        // `DirEntry` 已经从 WIN32_FIND_DATA 里拿到了 FILE_ATTRIBUTE_DIRECTORY
        // 和 nFileSizeHigh/Low，也就是目录/文件、大小全都齐了。
        // 走 `path().symlink_metadata()` 等于**每个条目重开一次文件**
        // （CreateFileW + FILE_FLAG_BACKUP_SEMANTICS），把枚举本身的成本
        // 放大一个数量级 —— 实测 10 万条目目录树上 3.33s vs 0.037s，90 倍。
        // 这一段是"复制一个目录"的前置扫描，扫不掉就直接体现在按下粘贴的延迟上。
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if !ft.is_dir() && !ft.is_file() {
                // 符号链接/联接点：不跟着走，也当不了复制源
                continue;
            }
            // 递归里仍然要处理子节点自身（顶层那个 symlink_metadata 是它
            // 在父目录里被需要的，递归这一层可以从 ft 直接进）
            collect_entry(
                &e.path(),
                ft.is_dir(),
                entry_size(&e, ft.is_dir()),
                &target,
                dirs,
                files,
                bytes_total,
                on_conflict,
                renamed,
                skipped,
                taken,
                cache,
            );
        }
        return;
    }
    if !md.is_file() {
        return;
    }
    push_file(src, target, md.len(), files, bytes_total);
}

/// 已知该条目是目录还是文件时的递归体。
///
/// 与 [`collect`] 的差别只有一处：省掉入口那次 `symlink_metadata()`，
/// 因为 `DirEntry::file_type()` 已经把答案给全了。这是上面那条 90 倍的落点。
///
/// `size` 由调用方用 [`entry_size`] 从 `DirEntry` 上取好（目录传 `None`）——
/// **不能**在这里 `src.symlink_metadata()`，那会把刚省掉的那次
/// `CreateFileW` 又加回来，优化就白做了。
#[allow(clippy::too_many_arguments)]
fn collect_entry(
    src: &Path,
    is_dir: bool,
    size: Option<u64>,
    dest: &Path,
    dirs: &mut Vec<PathBuf>,
    files: &mut Vec<FTask>,
    bytes_total: &mut u64,
    on_conflict: Conflict,
    renamed: &mut usize,
    skipped: &mut usize,
    taken: &mut HashSet<PathBuf>,
    cache: &mut ExistCache,
) {
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if name.is_empty() {
        return;
    }
    let want = dest.join(&name);
    let exists = cache.contains(want.parent().unwrap_or(dest), &name);
    let target =
        resolve_conflict_with(&want, exists, on_conflict, renamed, skipped, taken);
    let Some(target) = target else { return };
    if is_dir {
        dirs.push(target.clone());
        let Ok(rd) = std::fs::read_dir(src) else { return };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if !ft.is_dir() && !ft.is_file() {
                continue;
            }
            collect_entry(
                &e.path(),
                ft.is_dir(),
                entry_size(&e, ft.is_dir()),
                &target,
                dirs,
                files,
                bytes_total,
                on_conflict,
                renamed,
                skipped,
                taken,
                cache,
            );
        }
        return;
    }
    // 大小已经在调用方从 DirEntry 上取到了；拿不到（条目刚被删/无权限）
    // 就跳过，与旧的 `symlink_metadata()` 失败分支同语义。
    let Some(size) = size else { return };
    push_file(src, target, size, files, bytes_total);
}

fn push_file(
    src: &Path,
    target: PathBuf,
    size: u64,
    files: &mut Vec<FTask>,
    bytes_total: &mut u64,
) {
    *bytes_total += size;
    files.push(FTask {
        src: src.to_path_buf(),
        dst: target,
        size,
        left: Arc::new(AtomicUsize::new(0)),
        ok: Arc::new(AtomicBool::new(true)),
    });
}

/// 从 `DirEntry` 上取大小；目录不需要，传 `None`。
///
/// **这一行就是那 90 倍的全部。**
/// `DirEntry::metadata()` 在 Unix 上直接从 `dirent.d_type` 起步、在 Windows 上
/// 复用 `FindNextFileW` 顺带回来的 `WIN32_FIND_DATA`（里面已经有
/// `nFileSizeHigh/Low`），**不重新打开路径**；而 `path().symlink_metadata()`
/// 每个条目都要 `CreateFileW + FILE_FLAG_BACKUP_SEMANTICS` 再查一次，
/// 把纯枚举的成本放大一个数量级（实测 32.5µs/条目 vs 0.38µs/条目）。
///
/// 目录传 `None` 而不是也查一次：目录的大小在 NTFS 上还要额外查，
/// 而且复制目录本来就靠 `read_dir`，目录条目再查一次纯属白付。
fn entry_size(e: &std::fs::DirEntry, is_dir: bool) -> Option<u64> {
    if is_dir {
        return None;
    }
    // `metadata()` 不跟随符号链接，与旧的 `symlink_metadata()` 语义一致。
    // 顺带再确认一次是文件：枚举到、查询时之间被换成目录的话要丢掉。
    match e.metadata() {
        Ok(m) if m.is_file() => Some(m.len()),
        _ => None,
    }
}

/// 量一棵子树的字节数与文件数。只走 `read_dir`/`metadata`（不 `CreateFile`），
/// 所以不受杀软过滤驱动那个 202µs/次的影响。空目录按 1 个算。
fn subtree_stats(p: &Path) -> (u64, usize) {
    let Ok(md) = p.symlink_metadata() else { return (0, 0) };
    if !md.is_dir() {
        return (md.len(), 1);
    }
    let (mut sz, mut n) = (0u64, 0usize);
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            // metadata() 在目录上不会跟着进符号链接，不会绕出去
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                stack.push(e.path());
            } else {
                sz += m.len();
                n += 1;
            }
        }
    }
    (sz, n.max(1))
}

/// 目标已存在时怎么办。返回 None = 跳过。
///
/// `taken` 是**本批已分配的目标**（第39轮）：它解决的是 `exists()` 查不到的
/// 那类撞名 —— 同批两个源叫同一个名字时，目标都还没落盘，磁盘检查一路
/// 绿灯，两个任务最终写同一个路径。
fn resolve_conflict(
    want: &Path,
    on_conflict: Conflict,
    renamed: &mut usize,
    skipped: &mut usize,
    taken: &mut HashSet<PathBuf>,
) -> Option<PathBuf> {
    let exists = want.exists();
    resolve_conflict_with(want, exists, on_conflict, renamed, skipped, taken)
}

/// [`resolve_conflict`] 的核心，`exists` 由调用方给。
///
/// 拆开是因为存在性判定有两种来源：现场 `exists()`（单文件路径，代价可忽略）
/// 和 [`ExistCache`] 里的目录快照（成万条目时每条目一次 `exists()` 就是瓶颈）。
/// 拆成两个函数而不是给 `resolve_conflict` 加个 `Option<&mut cache>` 参数，
/// 是为了让**"判定结果"和"怎么判定"彻底分开** —— 判定错了要能一眼看出是哪边。
fn resolve_conflict_with(
    want: &Path,
    exists: bool,
    on_conflict: Conflict,
    renamed: &mut usize,
    skipped: &mut usize,
    taken: &mut HashSet<PathBuf>,
) -> Option<PathBuf> {
    let batch = taken.contains(want);
    if !exists && !batch {
        taken.insert(want.to_path_buf());
        return Some(want.to_path_buf());
    }
    match on_conflict {
        // 批内 Replace 不能照办：两个源写同一目标 = 后写的覆盖先写的，
        // 然后 Move 把两份源都删掉（净丢一份）。改名保住两份。
        Conflict::Replace if !batch => {
            taken.insert(want.to_path_buf());
            Some(want.to_path_buf())
        }
        Conflict::Replace => {
            let p = unique_copy_name(want, taken);
            taken.insert(p.clone());
            *renamed += 1;
            Some(p)
        }
        Conflict::Skip => {
            *skipped += 1;
            None
        }
        Conflict::Rename => {
            let p = unique_copy_name(want, taken);
            taken.insert(p.clone());
            *renamed += 1;
            Some(p)
        }
    }
}

/// 资源管理器中文界面就地粘贴的命名：`X - 副本.ext`、`X - 副本 (2).ext`…
///
/// 除了磁盘上已有的，还要避开 `taken`（同批已分配但还没写出去的名字）——
/// 否则批内第 2、3 个同名项会拿到同一个"副本"名，等于没解决（第39轮）。
fn unique_copy_name(want: &Path, taken: &HashSet<PathBuf>) -> PathBuf {
    let stem = want
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = want
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    let parent = want.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let first = parent.join(format!("{stem} - 副本{ext}"));
    if !first.exists() && !taken.contains(&first) {
        return first;
    }
    for i in 2..1000 {
        let p = parent.join(format!("{stem} - 副本 ({i}){ext}"));
        if !p.exists() && !taken.contains(&p) {
            return p;
        }
    }
    parent.join(format!("{stem} - 副本 ({}){ext}", std::process::id()))
}

/// 两个路径是不是在同一卷（同一盘符就算 —— 挂载点/连接点先不管）。
fn same_volume(a: &Path, b: &Path) -> bool {
    fn root(p: &Path) -> Option<String> {
        let s = p.to_string_lossy();
        let b = s.as_bytes();
        if b.len() >= 2 && b[1] == b':' {
            return Some(s[..2].to_ascii_uppercase());
        }
        None
    }
    match (root(a), root(b)) {
        (Some(x), Some(y)) => x == y,
        _ => true,
    }
}

/// 把一个时长格式化成 `1:23` / `1:02:03`。
pub fn fmt_duration(ms: f64) -> String {
    if !ms.is_finite() || ms < 0.0 {
        return "--:--".to_string();
    }
    let s = (ms / 1000.0).round() as u64;
    let (h, m, sec) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

/// 把剩余毫秒说成人话：`约 3 分钟` / `约 12 秒` / `不到 1 秒`。
pub fn fmt_eta(ms: f64) -> String {
    if !ms.is_finite() {
        return "正在计算…".to_string();
    }
    if ms < 1000.0 {
        return "不到 1 秒".to_string();
    }
    let s = (ms / 1000.0).round() as u64;
    if s < 60 {
        return format!("约 {s} 秒");
    }
    let m = s / 60;
    let rs = s % 60;
    if m < 60 {
        return if rs == 0 {
            format!("约 {m} 分钟")
        } else {
            format!("约 {m} 分 {rs} 秒")
        };
    }
    let h = m / 60;
    format!("约 {h} 小时 {} 分", m % 60)
}


#[cfg(test)]
mod tests {
    use super::*;

    /// 同批两个同名源 + Rename：必须拿到**两个不同**的目标。
    /// 第39轮审计 #3 的核心用例 —— 旧实现两个都返回 want，
    /// Move 时互相覆盖后双双删源，净丢一份。
    #[test]
    fn batch_rename_two_same_names_distinct() {
        let want = PathBuf::from(r"C:\__zen_t39\a\photo.jpg");
        let mut taken = HashSet::new();
        let (mut rn, mut sk) = (0usize, 0usize);
        let a = resolve_conflict(&want, Conflict::Rename, &mut rn, &mut sk, &mut taken);
        let b = resolve_conflict(&want, Conflict::Rename, &mut rn, &mut sk, &mut taken);
        let (a, b) = (a.expect("first"), b.expect("second"));
        assert_ne!(a, b, "同批同名必须分成两个目标");
        assert_eq!(rn, 1, "只有第二个走改名");
        assert!(taken.contains(&a) && taken.contains(&b));
    }

    /// Replace 也不能让批内两个源指向同一目标（覆盖 = 丢一份）。
    #[test]
    fn batch_replace_never_shares_target() {
        let want = PathBuf::from(r"C:\__zen_t39\b\data.bin");
        let mut taken = HashSet::new();
        let (mut rn, mut sk) = (0usize, 0usize);
        let a = resolve_conflict(&want, Conflict::Replace, &mut rn, &mut sk, &mut taken)
            .expect("first");
        let b = resolve_conflict(&want, Conflict::Replace, &mut rn, &mut sk, &mut taken)
            .expect("second");
        assert_ne!(a, b, "批内 Replace 必须改名，否则两份源写同一目标");
    }

    /// Skip 语义保留：批内第二个照旧跳过。
    #[test]
    fn batch_skip_drops_second() {
        let want = PathBuf::from(r"C:\__zen_t39\c\note.txt");
        let mut taken = HashSet::new();
        let (mut rn, mut sk) = (0usize, 0usize);
        assert!(resolve_conflict(&want, Conflict::Skip, &mut rn, &mut sk, &mut taken).is_some());
        assert!(resolve_conflict(&want, Conflict::Skip, &mut rn, &mut sk, &mut taken).is_none());
        assert_eq!(sk, 1);
    }

    /// 第三个同名项要拿到第三个名字（不能与前两个撞）。
    #[test]
    fn third_duplicate_gets_third_name() {
        let want = PathBuf::from(r"C:\__zen_t39\d\x.txt");
        let mut taken = HashSet::new();
        let (mut rn, mut sk) = (0usize, 0usize);
        let names: Vec<PathBuf> = (0..3)
            .map(|_| {
                resolve_conflict(&want, Conflict::Rename, &mut rn, &mut sk, &mut taken)
                    .expect("name")
            })
            .collect();
        let uniq: HashSet<&PathBuf> = names.iter().collect();
        assert_eq!(uniq.len(), 3, "三个同名源必须三个不同目标: {names:?}");
    }

    /// unique_copy_name 不能返回 taken 里已占用的名字（否则改名等于没改）。
    #[test]
    fn unique_copy_name_avoids_taken() {
        let want = PathBuf::from(r"C:\__zen_t39\e\y.txt");
        let mut taken = HashSet::new();
        let first = unique_copy_name(&want, &taken);
        taken.insert(first.clone());
        let second = unique_copy_name(&want, &taken);
        assert_ne!(first, second);
    }

    /// 块规划的三条不变量，跨一组有代表性的尺寸全跑一遍。
    /// 这三条正是旧 bug 的三个侧面：块长失控（74MB/块）、覆盖有缝、末块算错。
    fn assert_plan_invariants(size: u64) {
        let plan = plan_chunks(size);
        assert!(!plan.is_empty(), "size={size} 必须至少一块");
        // 1) 首尾相接、覆盖 [0,size)、无缝隙无重叠
        let mut expect_off = 0u64;
        for (off, len) in &plan {
            assert_eq!(*off, expect_off, "size={size} 块必须首尾相接");
            assert!(*len > 0, "size={size} 不允许 0 长块");
            // 2) 任何块都不超过 MAX_CHUNK_BYTES
            assert!(
                *len <= MAX_CHUNK_BYTES,
                "size={size} 块长 {} 超过上限 {MAX_CHUNK_BYTES}",
                *len
            );
            expect_off += *len;
        }
        assert_eq!(expect_off, size, "size={size} 块必须正好覆盖整个文件");
    }

    /// 核心回归：4.7GB 的真实 Windows ISO 尺寸。
    /// 旧实现在这里被 clamp 成 64 块 × 74MB —— 断言现在不是这个形状。
    #[test]
    fn big_iso_not_squashed_to_few_huge_chunks() {
        let gib = 1024u64 * 1024 * 1024;
        let plan = plan_chunks(4 * gib + 700 * 1024 * 1024);
        assert_eq!(
            plan.len(),
            ((4 * gib + 700 * 1024 * 1024 + CHUNK - 1) / CHUNK) as usize,
            "4.7GiB 必须按 CHUNK 正常切，不能被 clamp 成几十个巨块"
        );
        assert!(
            plan.iter().all(|(_, l)| *l <= CHUNK),
            "每块都该 <= CHUNK = {CHUNK}"
        );
        // 旧行为的形状：64 块、平均 74MB —— 明确不能出现
        assert!(
            plan.len() != 64,
            "又回到旧的 clamp(2,64) 行为（每块 {:} 字节）",
            plan[0].1
        );
    }

    #[test]
    fn chunk_plan_invariants_hold_across_sizes() {
        for &size in &[
            1,
            CHUNK - 1,
            CHUNK,
            CHUNK + 1,
            SPLIT_MIN,
            16 * 1024 * 1024,
            100 * 1024 * 1024,
            1024u64 * 1024 * 1024,
            4 * 1024 * 1024 * 1024 + 700 * 1024 * 1024,
            40 * 1024 * 1024 * 1024, // 远大于 KERNEL_MIN，走内核路径
            1024u64 * 1024 * 1024 * 1024, // 1 TiB
        ] {
            assert_plan_invariants(size);
        }
    }

    /// 三档必须互不重叠且单调递增。搞反了会让"大文件"退化回分块，
    /// 或者让"小文件"去占内核的复制队列。
    #[test]
    fn size_gates_are_ordered() {
        assert!(SPLIT_MIN < KERNEL_MIN, "分块门槛必须小于内核门槛");
        assert!(SPLIT_MIN <= CHUNK * 4, "小文件不该被拆得太碎");
    }

    /// 预建只覆盖"分块"这一档。内核路径（`Job::Big`）必须**不**预建 ——
    /// CopyFileEx 看到"目标已存在"会直接失败。
    /// 这条不变量挂在真实路径上：预建循环的跳过条件与 worker 的选路条件
    /// 必须用同一组阈值。
    #[test]
    fn kernel_files_are_never_preallocated() {
        // 预建循环：size >= SPLIT_MIN && size < KERNEL_MIN
        let preallocated = |size: u64| size >= SPLIT_MIN && size < KERNEL_MIN;
        // worker 选路：size >= KERNEL_MIN → Job::Big
        let is_kernel = |size: u64| size >= KERNEL_MIN;
        for size in [
            0,
            1024,
            SPLIT_MIN,
            100 * 1024 * 1024,
            KERNEL_MIN - 1,
            KERNEL_MIN,
            KERNEL_MIN + 1,
            4 * 1024 * 1024 * 1024,
        ] {
            assert!(
                !(preallocated(size) && is_kernel(size)),
                "size={size} 既被预建又走内核 → CopyFileEx 会因目标已存在而失败"
            );
        }
    }

    #[test]
    fn empty_file_has_no_chunks() {
        assert!(plan_chunks(0).is_empty());
    }

    /// 目录扫描的**功能**判据，不只是"能跑"：一棵混合树扫出来的
    /// 文件数、字节数、目标路径结构必须与逐个 `symlink_metadata` 的老写法一致。
    ///
    /// 性能改造（复用 `DirEntry` 缓存、省掉每条目一次 `CreateFileW`）最典型的
    /// 失败方式不是编译不过，是**悄悄少算或算错** —— 少递归一层、少收一个文件、
    /// 大小取成 0。这类 bug 在界面上就是"复制完少了一堆文件"。
    /// 所以这里逐项对拍，而不是只断言 `!files.is_empty()`。
    #[test]
    fn collect_matches_naive_recursion_on_a_mixed_tree() {
        let tmp = std::env::temp_dir().join("zf-collect-parity");
        let _ = std::fs::remove_dir_all(&tmp);
        let src = tmp.join("src");
        let dst = tmp.join("dst");
        // 3 层 + 空目录 + 0 字节文件 + 各种大小
        for (rel, len) in [
            ("a.bin", 0u64),
            ("b.bin", 1),
            ("sub/c.bin", 4096),
            ("sub/deep/d.bin", 1024 * 1024),
            ("sub/deep/deeper/e.bin", 7),
            ("sub/empty2/keep", 0),
            ("other.bin", 123456),
        ] {
            let p = src.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, vec![0u8; len as usize]).unwrap();
        }

        let mut dirs = Vec::new();
        let mut files = Vec::new();
        let mut total = 0u64;
        let mut renamed = 0;
        let mut skipped = 0;
        let mut taken = HashSet::new();
        let mut cache = ExistCache::new();
        collect(
            &src,
            &dst,
            &mut dirs,
            &mut files,
            &mut total,
            Conflict::Skip,
            &mut renamed,
            &mut skipped,
            &mut taken,
            &mut cache,
        );

        // 参照答案：老写法，每个条目重开一次
        let mut want_files = 0usize;
        let mut want_bytes = 0u64;
        let mut want_names = Vec::new();
        fn naive(dir: &std::path::Path, names: &mut Vec<String>, n: &mut usize, b: &mut u64) {
            let Ok(rd) = std::fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                let Ok(md) = p.symlink_metadata() else { continue };
                if md.is_dir() {
                    naive(&p, names, n, b);
                } else if md.is_file() {
                    names.push(p.file_name().unwrap().to_string_lossy().to_string());
                    *n += 1;
                    *b += md.len();
                }
            }
        }
        naive(&src, &mut want_names, &mut want_files, &mut want_bytes);

        assert_eq!(files.len(), want_files, "扫到的文件数与逐条重开不一致");
        assert_eq!(total, want_bytes, "总字节数与逐条重开不一致");
        let mut got_names: Vec<String> =
            files.iter().map(|f| f.src.file_name().unwrap().to_string_lossy().to_string()).collect();
        got_names.sort();
        want_names.sort();
        assert_eq!(got_names, want_names, "扫到的文件集合不一致");

        // 目标路径：整棵树重定位到 `dst/<顶层源名>/` 下。
        // 注意顶层**也**要带上自己的目录名 —— `collect` 的语义是
        // "把这个源搬进这个目标目录"，不是"把这个源的内容摊进目标目录"。
        // 这条也钉住：少拼一层目录名的话，复制整棵树的落点会整体错一层。
        let root = dst.join("src");
        for f in &files {
            let rel = f.src.strip_prefix(&src).unwrap();
            assert_eq!(f.dst, root.join(rel), "目标路径结构不一致");
            assert_eq!(f.size, std::fs::metadata(&f.src).unwrap().len());
        }
        assert!(dirs.iter().all(|d| d.starts_with(&root) && src.join(d.strip_prefix(&root).unwrap()).is_dir()));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 末块必须吃完余数：块数 × base + 余数 == size，且最后一块非空。
    #[test]
    fn last_chunk_takes_remainder() {
        let size = CHUNK * 3 + 12345;
        let plan = plan_chunks(size);
        assert_eq!(plan.len(), 4);
        let last = plan[plan.len() - 1];
        assert_eq!(last.1, 12345);
        assert_eq!(last.0 + last.1, size);
    }

    /// 目录快照缓存必须和逐条 `exists()` 给出**同一个答案**。
    ///
    /// 这个缓存替代的是每个条目一次 `exists()`（10 万次文件系统查询）。
    /// 它的失败模式是"悄悄判错"：判成"不存在"就覆盖别人的文件，
    /// 判成"存在"就无谓地改名叫 `X - 副本`。两条都不报错，只在结果里体现，
    /// 所以必须逐项对着真的 `exists()` 钉死，尤其是大小写。
    #[test]
    fn exist_cache_agrees_with_path_exists_case_insensitively() {
        let tmp = std::env::temp_dir().join(format!(
            "zf_exist_cache_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        for n in ["Photo.jpg", "report.PDF", "Sub Dir"] {
            let p = tmp.join(n);
            if n == "Sub Dir" {
                std::fs::create_dir_all(&p).unwrap();
            } else {
                std::fs::write(&p, b"x").unwrap();
            }
        }
        // 目标目录此刻**不存在** —— 真实场景就是这样（复制到一个新地方）。
        let fresh = tmp.join("does_not_exist");

        let mut cache = ExistCache::new();
        for name in [
            "Photo.jpg",
            "photo.jpg",
            "PHOTO.JPG",
            "report.PDF",
            "Report.pdf",
            "Sub Dir",
            "sub dir",
            "never_seen.txt",
        ] {
            let p = tmp.join(name);
            let real = p.exists();
            assert_eq!(
                cache.contains(&tmp, name),
                real,
                "缓存与 exists() 对 `{name}` 判断不一致"
            );
        }
        // 空目录也必须能查（否则每个不存在的目录都会退化成"全不存在"）
        assert!(!cache.contains(&fresh, "任何名字"));
        std::fs::create_dir_all(&fresh).unwrap();
        let mut c2 = ExistCache::new();
        assert!(!c2.contains(&fresh, "任何名字"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 用快照缓存规划出来的结果，必须和"每个目标都实打实 exists() 一次"完全一致。
    ///
    /// 单测缓存自身不够：真正会出错的是**接上之后**的规划结果 ——
    /// 目标目录里已经有一个同名文件时，必须被改名而不是覆盖。
    #[test]
    fn planning_a_colliding_file_picks_a_copy_name_instead_of_overwriting() {
        let tmp = std::env::temp_dir().join(format!(
            "zf_exist_plan_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let src = tmp.join("src");
        let dst = tmp.join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("a.txt"), b"new").unwrap();
        std::fs::write(src.join("b.txt"), b"new").unwrap();
        // 目标里已有 a.txt 的**旧内容** —— 不许被覆盖
        std::fs::write(dst.join("a.txt"), b"OLD").unwrap();

        // 直接复制**单个文件**。别用整棵目录：目录顶层撞名会先被改成
        // `src - 副本`，里头的文件落到一个全新的空目录，压根撞不上 ——
        // 断言"必须改名"就变成在测一个永远不发生的路径（第一版就是这么写错的，
        // 一跑就 FAIL，逼我把场景换成真正的撞名）。
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        let mut bytes = 0u64;
        let mut renamed = 0usize;
        let mut skipped = 0usize;
        let mut taken = HashSet::new();
        let mut cache = ExistCache::new();
        for f in ["a.txt", "b.txt"] {
            collect(
                &src.join(f),
                &dst,
                &mut dirs,
                &mut files,
                &mut bytes,
                Conflict::Rename,
                &mut renamed,
                &mut skipped,
                &mut taken,
                &mut cache,
            );
        }
        assert_eq!(files.len(), 2, "两个源文件都该被规划");
        assert_eq!(dirs.len(), 0, "复制单个文件不该产生目录");
        assert_eq!(renamed, 1, "只有撞名的那个该被改名");
        // a.txt 必须改名；b.txt 不该改
        let names: Vec<String> = files
            .iter()
            .map(|f| f.dst.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"a - 副本.txt".to_string()), "撞名没改名：{names:?}");
        assert!(names.contains(&"b.txt".to_string()), "不该改的也改了：{names:?}");
        // 目标里的旧内容必须原样还在
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"OLD");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 同批两个同名源也必须各自拿到不同的目标名（批内撞名）。
    ///
    /// `taken` 与目录快照是**两套独立机制**，各管一半：
    /// 快照管"盘上已经有"，`taken` 管"本批已分配但还没落盘"。
    /// 只测其中一套，另一套的 bug 就漏了。
    ///
    /// 两个源目录名要**不同**（`one/same.txt` 与 `two/same.txt`）——
    /// 那样 `want` 才落在同一个父目录下，`taken` 才真的被考验到。
    /// 用两个同名目录的话 `want` 是 `dst/one` 和 `dst/two`，本来就不同名，
    /// 一眼看去"文件同名"很吓人，实际上压根没冲突，断言会**假通过**。
    #[test]
    fn duplicate_sources_in_one_batch_get_distinct_targets() {
        let tmp = std::env::temp_dir().join(format!(
            "zf_exist_dup_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let d1 = tmp.join("one");
        let d2 = tmp.join("two");
        let inner = tmp.join("inner");
        let dst = tmp.join("dst");
        for d in [&d1, &d2] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("same.txt"), b"x").unwrap();
        }
        // 第三个源目录叫 `one2`，里面的文件也叫 `same.txt` ——
        // 三份源的**文件名**全一样，落点父目录也全一样
        let d3 = tmp.join("one2");
        std::fs::create_dir_all(&d3).unwrap();
        std::fs::write(d3.join("same.txt"), b"x").unwrap();
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("same.txt"), b"x").unwrap();
        std::fs::create_dir_all(&dst).unwrap();

        // 只把 `one/same.txt` 和 `one2/same.txt` 复制进 dst —— 两个 want 完全相同
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        let mut bytes = 0u64;
        let mut renamed = 0usize;
        let mut skipped = 0usize;
        let mut taken = HashSet::new();
        let mut cache = ExistCache::new();
        for f in [d1.join("same.txt"), d3.join("same.txt")] {
            collect(
                &f,
                &dst,
                &mut dirs,
                &mut files,
                &mut bytes,
                Conflict::Rename,
                &mut renamed,
                &mut skipped,
                &mut taken,
                &mut cache,
            );
        }
        let names: Vec<String> = files
            .iter()
            .map(|f| f.dst.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names.len(), 2);
        assert_eq!(renamed, 1, "第二个应该被改名");
        assert_ne!(names[0], names[1], "两个源拿到了同一个目标名：{names:?}");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
