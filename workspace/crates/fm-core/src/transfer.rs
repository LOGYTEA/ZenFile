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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// 超过这个大小就把单个文件拆成多块并行传。
///
/// 拆分的收益要盖过"预占长度 + 多次 open"的开销，实测 8 MB 是个合适的门槛：
/// 再小的话单个文件本来几十毫秒就传完了，拆了反而慢。
pub const SPLIT_MIN: u64 = 8 * 1024 * 1024;
/// 单块大小。4 MB：对机械盘足够顺序，对固态盘又不至于块数太多。
pub const CHUNK: u64 = 4 * 1024 * 1024;
/// 一个文件最多拆多少块。超大文件也别把队列撑爆。
pub const MAX_CHUNKS_PER_FILE: usize = 64;

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
}

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
        if f.size >= SPLIT_MIN {
            let n = ((f.size + CHUNK - 1) / CHUNK)
                .clamp(2, MAX_CHUNKS_PER_FILE as u64) as usize;
            // 块数定下来之后长度均分：最后一块吃余数
            let base = f.size / n as u64;
            f.left.store(n, Ordering::Relaxed);
            let mut off = 0u64;
            for k in 0..n {
                let len = if k + 1 == n {
                    f.size - off
                } else {
                    base
                };
                jobs.push(Job::Chunk { file: fi, offset: off, len });
                off += len;
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
        if f.size < SPLIT_MIN {
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
            .filter(|(_, f)| f.size >= SPLIT_MIN)
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
                    Job::Chunk { file, offset, len } => {
                        let f = &files[*file];
                        set_cur(&cur_name, &stats, &f.src);
                        match copy_range(&f.src, &f.dst, *offset, *len, f.size) {
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

/// 传 `[offset, offset+len)` 这一段。目标文件在规划阶段已经预占到 `total` 长度。
fn copy_range(src: &Path, dst: &Path, offset: u64, len: u64, total: u64) -> std::io::Result<u64> {
    // Windows 上这套叫 seek_read / seek_write（Unix 才叫 read_at / write_at）。
    // 底层是带 OVERLAPPED 的 ReadFile/WriteFile，走显式偏移，不动文件指针，
    // 所以多个块可以并行写同一个文件的不同区间。
    use std::os::windows::fs::{FileExt, OpenOptionsExt};
    // **必须显式开共享模式**：Rust 默认的写打开不带 FILE_SHARE_WRITE，于是
    // 一个线程占着目标文件之后，其余块的线程再 open 就是 ERROR_ACCESS_DENIED
    // （实测 5 块里只能成功 1 块，其余全是 os error 5）。
    let sf = std::fs::OpenOptions::new().read(true).share_mode(SHARE_ALL).open(src)?;
    let mut df = std::fs::OpenOptions::new()
        .write(true)
        .share_mode(SHARE_ALL)
        .open(dst)?;
    // 兜底：万一预建那一步被跳过（比如目标已存在但比源短），这里补一次
    if df.metadata()?.len() < total {
        df.set_len(total)?;
    }
    let mut buf = vec![0u8; len as usize];
    let mut off = offset;
    let mut got = 0usize;
    // seek_read 允许短读，循环补齐
    while got < buf.len() {
        let n = sf.seek_read(&mut buf[got..], off)?;
        if n == 0 {
            break;
        }
        got += n;
        off += n as u64;
    }
    if got == 0 {
        return Ok(0);
    }
    let mut w = 0usize;
    while w < got {
        w += df.seek_write(&buf[w..got], offset + w as u64)?;
    }
    Ok(got as u64)
}

/// 展开一个源路径：目录递归成"目录清单 + 文件清单"。
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
) {
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if name.is_empty() {
        return;
    }
    let target = resolve_conflict(&dest.join(&name), on_conflict, renamed, skipped, taken);
    let Some(target) = target else { return };

    let Ok(md) = src.symlink_metadata() else { return };
    if md.is_dir() {
        dirs.push(target.clone());
        let Ok(rd) = std::fs::read_dir(src) else { return };
        for e in rd.flatten() {
            collect(
                &e.path(),
                &target,
                dirs,
                files,
                bytes_total,
                on_conflict,
                renamed,
                skipped,
                taken,
            );
        }
        return;
    }
    if !md.is_file() {
        return;
    }
    let size = md.len();
    *bytes_total += size;
    files.push(FTask {
        src: src.to_path_buf(),
        dst: target,
        size,
        left: Arc::new(AtomicUsize::new(0)),
        ok: Arc::new(AtomicBool::new(true)),
    });
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
    let batch = taken.contains(want);
    if !want.exists() && !batch {
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
}
