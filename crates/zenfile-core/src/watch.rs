//! 目录变更增量层（技术方案 §5）。
//!
//! 只为"当前可见目录"开一个句柄，严禁递归监听整棵树（§12 内存纪律）。
//! 收到通知后的策略是"触发重新枚举"，而不是增量合并 —— 前者简单且结果永远正确。

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
    FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::{CancelIoEx, OVERLAPPED};
use windows::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, INFINITE,
};

use crate::error::{Error, Result};

pub enum WatchMsg {
    Changed(PathBuf),
}

pub struct DirWatcher {
    stop_event: HANDLE,
    dir_handle: HANDLE,
    thread: Option<std::thread::JoinHandle<()>>,
}

// safety: 句柄仅在 stop 时被 drop，线程同步 join，不存在跨线程数据竞争。
unsafe impl Send for DirWatcher {}

impl DirWatcher {
    pub fn start(path: PathBuf, tx: Sender<WatchMsg>) -> Result<Self> {
        let hs = HSTRING::from(path.to_string_lossy().as_ref());
        let dir_handle = unsafe {
            CreateFileW(
                &hs,
                FILE_LIST_DIRECTORY.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .map_err(|_| Error::Other(format!("无法打开目录句柄: {}", path.display())))?;

        let stop_event = unsafe { CreateEventW(None, true, false, None) }
            .map_err(|_| Error::Other("无法创建停止事件".into()))?;

        // 句柄是裸指针（!Send），先转成整数再搬到 worker，由 worker 负责关闭。
        let dir_handle_val = dir_handle.0 as isize;
        let stop_event_val = stop_event.0 as isize;
        let thread = std::thread::spawn(move || unsafe {
            let dir_handle = HANDLE(dir_handle_val as *mut core::ffi::c_void);
            let stop_event = HANDLE(stop_event_val as *mut core::ffi::c_void);
            let mut ov = OVERLAPPED::default();
            ov.hEvent = match CreateEventW(None, true, false, None) {
                Ok(e) => e,
                Err(_) => return,
            };
            let mut buffer = vec![0u8; 64 * 1024].into_boxed_slice();
            let filter = FILE_NOTIFY_CHANGE_FILE_NAME
                | FILE_NOTIFY_CHANGE_DIR_NAME
                | FILE_NOTIFY_CHANGE_ATTRIBUTES
                | FILE_NOTIFY_CHANGE_SIZE;

            loop {
                let ok = windows::Win32::Storage::FileSystem::ReadDirectoryChangesW(
                    dir_handle,
                    buffer.as_mut_ptr() as *mut core::ffi::c_void,
                    buffer.len() as u32,
                    false,
                    filter,
                    None,
                    Some(&mut ov),
                    None,
                );
                if ok.is_err() {
                    break;
                }
                let wait = WaitForMultipleObjects(&[stop_event, ov.hEvent], false, INFINITE);
                let signaled = wait.0.saturating_sub(WAIT_OBJECT_0.0);
                if signaled == 0 {
                    let _ = CancelIoEx(dir_handle, None);
                    break;
                }
                let _ = ResetEvent(ov.hEvent);
                if tx.send(WatchMsg::Changed(path.clone())).is_err() {
                    break;
                }
            }
            let _ = CloseHandle(ov.hEvent);
            let _ = CloseHandle(dir_handle);
            let _ = CloseHandle(stop_event);
        });

        Ok(Self { stop_event, dir_handle, thread: Some(thread) })
    }

    pub fn stop(mut self) {
        let _ = unsafe { SetEvent(self.stop_event) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        } else {
            let _ = unsafe { CloseHandle(self.dir_handle) };
        }
    }
}
