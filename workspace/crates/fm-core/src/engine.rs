//! 目录引擎（技术方案 §5）。
//!
//! 分层：
//!   主路径  FindFirstFileExW + FIND_FIRST_EX_LARGE_FETCH（所有普通目录）
//!   递归    并行顶层扇出 worker（仅搜索场景）
//!   增量    watch.rs 的 ReadDirectoryChangesW
//!
//! 任何一层失败都必须回退主路径 —— 本项目尚未接入 USN / MFT / Everything，
//! 它们是可选加速点，缺失不影响正确性。

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use windows::core::HSTRING;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    FindClose, FindFirstFileExW, FindNextFileW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_REPARSE_POINT, FINDEX_INFO_LEVELS,
    FINDEX_SEARCH_OPS, WIN32_FIND_DATAW,
};

use crate::types::{DirBatch, FileEntry, FileKind, Generation, SortSpec, MAX_BATCH};

/// 引擎向 UI 发送的事件。UI 端用 try_recv 排空，不做阻塞等待。
pub enum EngineMsg {
    Batch(DirBatch),
    Error(Generation, crate::Error),
}

/// 把 \\?\ 前缀应用于长路径/UNC，规避 MAX_PATH 与路径规范化陷阱。
fn search_pattern(dir: &Path) -> String {
    let base = dir.to_string_lossy().into_owned();
    let trimmed = base.trim_end_matches(|c| c == '\\' || c == '/');
    let native = trimmed.replace('/', "\\");
    if native.starts_with("\\\\") {
        format!("\\\\?\\UNC\\{}\\*", native.trim_start_matches("\\\\"))
    } else {
        format!("\\\\?\\{}\\*", native)
    }
}

fn filetime_to_i64(high: u32, low: u32) -> i64 {
    (((high as u64) << 32) | low as u64) as i64
}

fn to_entry(dir: &Path, data: &WIN32_FIND_DATAW) -> Option<FileEntry> {
    let name = wide_to_string(&data.cFileName);
    if name == "." || name == ".." {
        return None;
    }
    let attrs = data.dwFileAttributes;
    let kind = if attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
        if attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            FileKind::ReparsePoint
        } else {
            FileKind::Directory
        }
    } else {
        FileKind::File
    };
    let size = (((data.nFileSizeHigh as u64) << 32) | data.nFileSizeLow as u64) as u64;
    Some(FileEntry {
        path: dir.join(&name),
        name,
        kind,
        size,
        modified: filetime_to_i64(data.ftLastWriteTime.dwHighDateTime, data.ftLastWriteTime.dwLowDateTime),
        created: filetime_to_i64(data.ftCreationTime.dwHighDateTime, data.ftCreationTime.dwLowDateTime),
        accessed: filetime_to_i64(data.ftLastAccessTime.dwHighDateTime, data.ftLastAccessTime.dwLowDateTime),
        attributes: attrs,
        hidden: attrs & FILE_ATTRIBUTE_HIDDEN.0 != 0,
    })
}

pub(crate) fn wide_to_string(wide: &[u16]) -> String {
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end])
}

/// 单次目录枚举：边遍历边分批提交，首批数据几乎立即可见。
///
/// 该函数的调用方通常是后台线程；UI 侧通过比较 generation 丢弃过期批次。
pub fn enumerate_dir(
    dir: &Path,
    generation: Generation,
    sort: SortSpec,
    tx: &Sender<EngineMsg>,
) -> std::result::Result<(), ()> {
    let pattern = HSTRING::from(&search_pattern(dir)[..]);
    let mut data = WIN32_FIND_DATAW::default();
    let handle = unsafe {
        FindFirstFileExW(
            &pattern,
            FINDEX_INFO_LEVELS(1), // FindExInfoBasic
            &mut data as *mut _ as *mut c_void,
            FINDEX_SEARCH_OPS(0), // FindExSearchNameMatch
            None,
            windows::Win32::Storage::FileSystem::FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    let handle: HANDLE = match handle {
        Ok(h) => h,
        Err(e) => {
            let _ = tx.send(EngineMsg::Error(generation, e.into()));
            return Err(());
        }
    };

    let mut pending: Vec<FileEntry> = Vec::with_capacity(MAX_BATCH);
    let sentinel_outer = true;
    let mut sentinel = sentinel_outer;
    let mut end_of_scan = false;
    loop {
        let entry_opt = if sentinel {
            sentinel = false;
            to_entry(dir, &data)
        } else {
            match unsafe { FindNextFileW(handle, &mut data) } {
                Ok(()) => to_entry(dir, &data),
                Err(_) => {
                    end_of_scan = true;
                    None
                }
            }
        };

        // 注意：通配符枚举会把 "." 与 ".." 作为前两条返回，to_entry 已过滤，
        // 因此这里绝不能把 None 当作结束信号，否则空目录枚举结果全为空。
        if let Some(entry) = entry_opt {
            pending.push(entry);
            if pending.len() >= MAX_BATCH {
                flush(generation, false, &mut pending, sort, tx);
            }
        }
        if end_of_scan {
            break;
        }
    }
    // 注意：最后一批需要排序，因此先把总 fading 送出。为避免打乱顺序，
    // 这里采用简单可靠的做法：直接一次性排序残余集合再提交。
    flush(generation, true, &mut pending, sort, tx);
    let _ = unsafe { FindClose(handle) };
    Ok(())
}

fn flush(
    generation: Generation,
    done: bool,
    pending: &mut Vec<FileEntry>,
    sort: SortSpec,
    tx: &Sender<EngineMsg>,
) {
    if pending.is_empty() && !done {
        return;
    }
    sort.sort(pending);
    let batch = DirBatch {
        generation,
        entries: std::mem::take(pending),
        done,
    };
    if tx.send(EngineMsg::Batch(batch)).is_err() {
        // UI 已退出，线程自然结束
    }
}

/// 搜索请求：递归遍历 + 即时筛选 + 结果上限（内存纪律）。
pub struct SearchRequest {
    pub root: PathBuf,
    pub query: String,
    pub generation: Generation,
    /// 结果硬上限，防止一次性把 10 万条塞进内存
    pub limit: usize,
    pub threads: usize,
}

/// 递归并行搜索。采用 worker 共享栈（VecDeque<PathBuf>）而非 jwalk，
/// 以免引入额外依赖；默认跳过巨型工程目录。
const SKIP_DIRS: [&str; 5] = ["node_modules", ".git", "Intermediate", "DerivedDataCache", "$RECYCLE.BIN"];

pub fn run_search(req: SearchRequest, tx: Sender<EngineMsg>) {
    let threads = req.threads.clamp(1, 16);
    let query = req.query.to_lowercase();
    let limit = req.limit;
    let generation = req.generation;
    let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let produced = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let (batch_tx, batch_rx) = std::sync::mpsc::channel::<FileEntry>();
    let out_tx = tx.clone();
    let stop_flag = stopped.clone();
    let produced_flag = produced.clone();
    // 收集线程：把命中项按代际分批转发给 UI，并处理结果上限
    let collector = std::thread::spawn(move || {
        let mut pending: Vec<FileEntry> = Vec::with_capacity(MAX_BATCH);
        loop {
            match batch_rx.recv() {
                Ok(e) => {
                    pending.push(e);
                    if pending.len() >= MAX_BATCH {
                        if out_tx
                            .send(EngineMsg::Batch(DirBatch {
                                generation,
                                entries: std::mem::take(&mut pending),
                                done: false,
                            }))
                            .is_err()
                        {
                            break;
                        }
                    }
                    if produced_flag.load(std::sync::atomic::Ordering::Relaxed) >= limit {
                        stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                Err(_) => break, // 所有 worker 已结束
            }
        }
        let _ = out_tx.send(EngineMsg::Batch(DirBatch { generation, entries: pending, done: true }));
    });

    // 顶层扇出：root 的直接子目录各由一个 worker 深挖，root 自身的文件由主线程处理。
    // 每个 worker 拥有自己的栈，因此不存在共享队列的空闲竞态（这是上一版活锁的根因）。
    let mut children: Vec<FileEntry> = Vec::new();
    collect_children(&req.root, &mut children);
    let mut workers = Vec::new();
    let mut top_dirs: Vec<PathBuf> = children
        .iter()
        .filter(|e| e.is_dir())
        .map(|e| e.path.clone())
        .collect();

    let chunks: Vec<Vec<PathBuf>> = split_evenly(&mut top_dirs, threads);
    for chunk in chunks {
        if chunk.is_empty() {
            continue;
        }
        let tx = batch_tx.clone();
        let query = query.clone();
        let stopped = stopped.clone();
        let produced = produced.clone();
        workers.push(std::thread::spawn(move || {
            let mut stack: Vec<PathBuf> = chunk;
            let mut buf: Vec<FileEntry> = Vec::new();
            while !stopped.load(std::sync::atomic::Ordering::Relaxed) {
                let Some(dir) = stack.pop() else { break };
                buf.clear();
                collect_children(&dir, &mut buf);
                for e in &buf {
                    if e.is_dir() {
                        let skip = SKIP_DIRS.iter().any(|k| e.name.eq_ignore_ascii_case(k));
                        if !skip {
                            stack.push(e.path.clone());
                        }
                    }
                    if e.name.to_lowercase().contains(&query) {
                        if produced.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= limit {
                            stopped.store(true, std::sync::atomic::Ordering::Relaxed);
                            return;
                        }
                        if tx.send(e.clone()).is_err() {
                            return;
                        }
                    }
                }
            }
        }));
    }

    // root 顶层命中项（经同一条批通道，保证顺序一致）
    {
        let root_tx = batch_tx.clone();
        for e in &children {
            if e.name.to_lowercase().contains(&query) && root_tx.send(e.clone()).is_ok() {
                let _ = produced.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    drop(batch_tx);
    for w in workers {
        let _ = w.join();
    }
    let _ = collector.join();
}

fn split_evenly(items: &mut Vec<PathBuf>, buckets: usize) -> Vec<Vec<PathBuf>> {
    let n = buckets.max(1);
    let mut out: Vec<Vec<PathBuf>> = vec![Vec::new(); n];
    for (i, item) in items.drain(..).enumerate() {
        out[i % n].push(item);
    }
    out
}

/// 单次目录快照（不排序、不分批），供搜索 worker 使用。
fn collect_children(dir: &Path, out: &mut Vec<FileEntry>) {
    let pattern = HSTRING::from(&search_pattern(dir)[..]);
    let mut data = WIN32_FIND_DATAW::default();
    let handle = unsafe {
        FindFirstFileExW(
            &pattern,
            FINDEX_INFO_LEVELS(1),
            &mut data as *mut _ as *mut c_void,
            FINDEX_SEARCH_OPS(0),
            None,
            windows::Win32::Storage::FileSystem::FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    let Ok(handle) = handle else { return };
    let mut sentinel = true;
    loop {
        let mut reached_end = false;
        let entry = if sentinel {
            sentinel = false;
            to_entry(dir, &data)
        } else {
            match unsafe { FindNextFileW(handle, &mut data) } {
                Ok(()) => to_entry(dir, &data),
                Err(_) => {
                    reached_end = true;
                    None
                }
            }
        };
        if let Some(e) = entry {
            out.push(e);
        } else if reached_end {
            break;
        }
    }
    let _ = unsafe { FindClose(handle) };
}
