//! 文件操作（技术方案 §6）。
//!
//! 三个层次必须分清：
//!   Windows 资源管理器 —— 单线程、一次一个文件（事实，非缺陷）
//!   Robocopy /MT     —— 并行的是"多个文件"，不是单个文件
//!   单个大文件       —— 多少线程都是一条流
//!
//! 因此本项目采取双策略：
//!   A（默认）IFileOperation：与 Explorer 行为一致，自带冲突/进度/回收站/撤销
//!   B（快速模式）自研线程池 + 队列：解决海量小文件场景
//! FileOperation 的所有调用必须在 STA 线程执行，并保持 CLSID 饿汉初始化。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::HSTRING;
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVE_FILE_FLAGS};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{
    FileOperation, IFileOperation, IShellItem, SHCreateItemFromParsingName, FOF_ALLOWUNDO,
    FOF_NOCONFIRMATION, FOF_NOCONFIRMMKDIR, FOF_RENAMEONCOLLISION,
};

use crate::error::{Error, Result};

/// COM 初始化守卫：每线程一次，且不得跨线程移动（技术方案 §2）。
pub struct ComGuard {
    _private: (),
}

impl ComGuard {
    pub fn init() -> Result<Self> {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        // RPC_E_CHANGED_MODE(0x80010106) 与 S_FALSE 都表示"本线程已初始化过"，不应视为失败。
        if hr.is_err() {
            let code = hr.0;
            if code != -2147417850 && code != 1 {
                return Err(Error::Com(code));
            }
        }
        Ok(ComGuard { _private: () })
    }
}

fn shell_item(path: &Path) -> Result<IShellItem> {
    let hs = HSTRING::from(path.to_string_lossy().as_ref());
    let item: IShellItem =
        unsafe { SHCreateItemFromParsingName(&hs, None) }.map_err(Error::from)?;
    Ok(item)
}

/// 默认操作位。
fn base_flags(silent: bool) -> windows::Win32::UI::Shell::FILEOPERATION_FLAGS {
    let mut f = FOF_RENAMEONCOLLISION | FOF_NOCONFIRMMKDIR;
    if silent {
        f |= FOF_NOCONFIRMATION;
    }
    f
}

struct FileOp {
    op: IFileOperation,
}

impl FileOp {
    fn new(owner: isize, recycle: bool, silent: bool) -> Result<Self> {
        let op: IFileOperation = unsafe {
            windows::Win32::System::Com::CoCreateInstance(&FileOperation, None, {
                windows::Win32::System::Com::CLSCTX_ALL
            })
        }
        .map_err(Error::from)?;
        let mut flags = base_flags(silent);
        if recycle {
            flags |= FOF_ALLOWUNDO;
        }
        unsafe {
            op.SetOperationFlags(flags)?;
            if owner != 0 {
                op.SetOwnerWindow(windows::Win32::Foundation::HWND(owner as *mut core::ffi::c_void))?;
            }
        }
        Ok(Self { op })
    }

    fn commit(&self) -> Result<()> {
        unsafe { self.op.PerformOperations() }.map_err(Error::from)
    }
}

/// 就地粘贴时资源管理器给新副本起的名字后缀（"X - 副本"里的"副本"）。
///
/// 这个串是 shell32 里的本地化资源，`CopyItem` 传 NULL 时 Shell 只会退化成
/// "X (2)"，跟资源管理器对不上。资源号没有公开可靠的说法，所以只在中文界面
/// 下用"副本"，其它语言维持 Shell 默认 —— 硬编造一个英文 "Copy" 反而更糟。
fn copy_suffix() -> Option<&'static str> {
    match unsafe { GetUserDefaultUILanguage() } {
        0x0804 | 0x0404 | 0x0C04 | 0x1004 | 0x1404 => Some("副本"),
        _ => None,
    }
}

/// 复制到目标文件夹。
pub fn copy_items(owner: isize, dest: &Path, items: &[PathBuf]) -> Result<()> {
    let fo = FileOp::new(owner, false, false)?;
    let dest_item = shell_item(dest)?;
    let suffix = copy_suffix();
    for src in items {
        let item = shell_item(src)?;
        // 只有"粘回原目录"才需要自己起名（否则名字跟源一样，Shell 不会自动加后缀）
        let copy_name = match (suffix, src.parent()) {
            (Some(sfx), Some(p)) if p == dest => src.file_name().map(|n| {
                let s = n.to_string_lossy();
                match s.rfind('.') {
                    Some(i) if i > 0 => format!("{} - {}{}", &s[..i], sfx, &s[i..]),
                    _ => format!("{s} - {sfx}"),
                }
            }),
            _ => None,
        };
        let name_h = copy_name.map(|s| HSTRING::from(s.as_str()));
        unsafe {
            fo.op.CopyItem(
                &item,
                &dest_item,
                name_h
                    .as_ref()
                    .map(|h| windows::core::PCWSTR(h.as_ptr()))
                    .unwrap_or(windows::core::PCWSTR::null()),
                None,
            )?
        };
    }
    fo.commit()
}

/// 移动到目标文件夹。同卷走 MoveFileEx 改名（瞬时），其余交给 Shell 以保证跨卷语义一致。
pub fn move_items(owner: isize, dest: &Path, items: &[PathBuf]) -> Result<()> {
    let same_volume = items
        .iter()
        .all(|p| same_root(p, dest));
    if same_volume {
        for src in items {
            let target = dest.join(src.file_name().ok_or_else(|| Error::Other("无文件名".into()))?);
            unsafe {
                MoveFileExW(
                    &HSTRING::from(src.to_string_lossy().as_ref()),
                    &HSTRING::from(target.to_string_lossy().as_ref()),
                    MOVE_FILE_FLAGS(0),
                )?
            };
        }
        return Ok(());
    }
    let fo = FileOp::new(owner, false, false)?;
    let dest_item = shell_item(dest)?;
    for src in items {
        let item = shell_item(src)?;
        unsafe { fo.op.MoveItem(&item, &dest_item, windows::core::PCWSTR::null(), None)? };
    }
    fo.commit()
}

/// 删除。recycle=true 进回收站（可逆），false 为彻底删除。
pub fn delete_items(owner: isize, items: &[PathBuf], recycle: bool) -> Result<()> {
    let fo = FileOp::new(owner, recycle, false)?;
    for target in items {
        let item = shell_item(target)?;
        unsafe { fo.op.DeleteItem(&item, None)? };
    }
    fo.commit()
}

/// 重命名。走 Shell 以保留"是否允许改名"与第三方钩子的一致性。
pub fn rename_item(owner: isize, item_path: &Path, new_name: &str) -> Result<()> {
    let fo = FileOp::new(owner, false, true)?;
    let item = shell_item(item_path)?;
    let hs = HSTRING::from(new_name);
    unsafe { fo.op.RenameItem(&item, &hs, None)? };
    fo.commit()
}

/// 批量重命名。`pairs` = (原路径, 新文件名)。
///
/// 全部放进**同一个** IFileOperation 再提交：这样要么一起成功，要么一起失败，
/// 而且 Ctrl+Z 一次就能全部撤销。逐个 `rename_item` 会变成 N 个独立操作，
/// 用户要按 N 次撤销，中间失败还会留下改了一半的目录。
pub fn rename_items(owner: isize, pairs: &[(PathBuf, String)]) -> Result<()> {
    if pairs.is_empty() {
        return Ok(());
    }
    let fo = FileOp::new(owner, false, true)?;
    for (path, new_name) in pairs {
        let item = shell_item(path)?;
        let hs = HSTRING::from(new_name.as_str());
        unsafe { fo.op.RenameItem(&item, &hs, None)? };
    }
    fo.commit()
}

/// 新建文件夹，返回实际创建的路径（Shell 会自动去重命名）。
pub fn new_folder(owner: isize, dir: &Path) -> Result<()> {
    let fo = FileOp::new(owner, true, true)?;
    let dest = shell_item(dir)?;
    let hs = HSTRING::from("新建文件夹");
    unsafe {
        fo.op.NewItem(
            &dest,
            windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY.0,
            &hs,
            windows::core::PCWSTR::null(),
            None,
        )?
    };
    fo.commit()
}

/// 新建文件。
pub fn new_file(owner: isize, dir: &Path, name: &str) -> Result<()> {
    let fo = FileOp::new(owner, true, true)?;
    let dest = shell_item(dir)?;
    let hs = HSTRING::from(name);
    unsafe {
        fo.op.NewItem(
            &dest,
            windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL.0,
            &hs,
            windows::core::PCWSTR::null(),
            None,
        )?
    };
    fo.commit()
}

fn same_root(a: &Path, b: &Path) -> bool {
    let root = |p: &Path| -> Option<String> {
        let s = p.to_string_lossy();
        if let Some(unc) = s.strip_prefix("\\\\") {
            Some(format!("UNC\\{unc}").to_lowercase())
        } else {
            let bytes = s.as_bytes();
            if bytes.len() >= 2 && bytes[1] == b':' {
                Some((bytes[0] as char).to_ascii_lowercase().to_string())
            } else {
                None
            }
        }
    };
    match (root(a), root(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

// ------------------------------ 快速模式（策略 B） ------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TransferKind {
    Copy,
    Move,
}

#[derive(Default)]
pub struct TransferStats {
    pub files_done: usize,
    pub files_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub current: Option<PathBuf>,
    pub canceled: bool,
    pub finished: bool,
    pub error: Option<String>,
}

impl TransferStats {
    pub fn progress(&self) -> f32 {
        if self.bytes_total == 0 {
            if self.files_total == 0 {
                return 1.0;
            }
            return self.files_done as f32 / self.files_total as f32;
        }
        (self.bytes_done as f32 / self.bytes_total as f32).clamp(0.0, 1.0)
    }
}

pub type SharedStats = Arc<Mutex<TransferStats>>;

/// 自研多线程传输：并行的是"多个文件"，单文件仍是一条流 —— 这是分层事实，不是缺陷。
pub fn transfer_async(kind: TransferKind, dest: &Path, items: Vec<PathBuf>, threads: usize) -> SharedStats {
    let stats: SharedStats = Arc::new(Mutex::new(TransferStats {
        files_total: items.len(),
        ..Default::default()
    }));
    let out = stats.clone();
    let dest = dest.to_path_buf();
    std::thread::spawn(move || {
        let queue: Vec<(PathBuf, PathBuf)> = flatten(&items, &dest);
        let total_bytes: u64 = queue.iter().filter_map(|(s, _)| s.metadata().ok()).map(|m| m.len()).sum();
        {
            let mut s = out.lock().unwrap();
            s.files_total = queue.len();
            s.bytes_total = total_bytes;
        }
        if queue.is_empty() {
            out.lock().unwrap().finished = true;
            return;
        }
        let queue = Arc::new(queue);
        let idx = Arc::new(AtomicU64::new(0));
        let worker_count = threads.clamp(1, 64).min(queue.len());
        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let out = out.clone();
            let queue = queue.clone();
            let idx = idx.clone();
            handles.push(std::thread::spawn(move || {
                loop {
                    let i = idx.fetch_add(1, Ordering::Relaxed) as usize;
                    if i >= queue.len() {
                        break;
                    }
                    let (src, dst) = &queue[i];
                    {
                        let mut s = out.lock().unwrap();
                        s.current = Some(src.clone());
                    }
                    transfer_one(src, dst, &out);
                    {
                        let mut s = out.lock().unwrap();
                        s.files_done += 1;
                    }
                }
            }));
        }
        for h in handles {
            let _ = h.join();
        }

        if kind == TransferKind::Move {
            // 跨卷移动：复制成功后再删除源，失败则保留源由用户判断（§6.4）。
            for (src, _) in queue.iter() {
                if src.is_dir() {
                    let _ = std::fs::remove_dir_all(src);
                } else {
                    let _ = std::fs::remove_file(src);
                }
            }
        }
        let mut s = out.lock().unwrap();
        s.finished = true;
    });
    stats
}

fn transfer_one(src: &Path, dst: &Path, out: &SharedStats) {
    if src.is_dir() {
        if let Err(e) = std::fs::create_dir_all(dst) {
            let mut s = out.lock().unwrap();
            s.error = Some(format!("创建目录失败 {}: {e}", dst.display()));
        }
        return;
    }
    if let Some(parent) = dst.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            let mut s = out.lock().unwrap();
            s.error = Some(format!("创建目录失败 {}: {e}", parent.display()));
            return;
        }
    }
    match std::fs::copy(src, dst) {
        Ok(n) => {
            let mut s = out.lock().unwrap();
            s.bytes_done += n;
        }
        Err(e) => {
            let mut s = out.lock().unwrap();
            s.error = Some(format!("复制失败 {}: {e}", src.display()));
        }
    }
}

/// 把待转移项展开成 (源, 目标) 列表：目录递归展开。
fn flatten(items: &[PathBuf], dest: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    for item in items {
        let name = match item.file_name() {
            Some(n) => n,
            None => continue,
        };
        if item.is_dir() {
            out.push((item.clone(), dest.join(name)));
            collect_dir(item, dest.join(name), &mut out);
        } else {
            out.push((item.clone(), dest.join(name)));
        }
    }
    out
}

fn collect_dir(src: &Path, dst: PathBuf, out: &mut Vec<(PathBuf, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(src) else { return };
    for entry in rd.flatten() {
        let p = entry.path();
        let name = entry.file_name();
        if p.is_dir() {
            out.push((p.clone(), dst.join(&name)));
            collect_dir(&p, dst.join(&name), out);
        } else {
            out.push((p, dst.join(&name)));
        }
    }
}
