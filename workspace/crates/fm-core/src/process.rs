//! 子进程模型与关机/启动管理（技术方案 §3 / §11）。
//!
//! 任务管理器的树状折叠依赖父子进程关系，因此：
//!   1. 所有模块只能由 app.exe 用 CreateProcess 启动；
//!   2. app.exe 常驻，否则子进程会升为顶层孤儿且再也折不回去；
//!   3. 子进程不得注册托盘 / 自启 / 显示主窗口。

use std::ffi::{c_void, OsStr};
use std::path::{Path, PathBuf};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECTINFOCLASS,
};
use windows::Win32::System::Threading::{
    CreateProcessW, ResumeThread, STARTUPINFOW, PROCESS_INFORMATION,
    CREATE_SUSPENDED, PROCESS_CREATION_FLAGS,
};

use crate::error::{Error, Result};

/// Job Object 生命周期锚点：句柄关闭时，作业内全部进程被内核终结。
/// 这解决了"任务管理器里杀一个、其余变僵尸"的问题（§3.2）。
pub struct ProcessTree {
    job: HANDLE,
    children: Vec<ChildProcess>,
}

unsafe impl Send for ProcessTree {}

pub struct ChildProcess {
    pub pid: u32,
    process: HANDLE,
    thread: HANDLE,
}

impl ProcessTree {
    pub fn new() -> Result<Self> {
        let job = unsafe { CreateJobObjectW(None, None) }.map_err(Error::from)?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
        unsafe {
            SetInformationJobObject(
                job,
                JOBOBJECTINFOCLASS(9), // JobObjectExtendedLimitInformation
                &info as *const _ as *const c_void,
                size,
            )
        }
        .map_err(Error::from)?;
        Ok(Self { job, children: Vec::new() })
    }

    /// 启动子进程。
    ///
    /// 必须 CREATE_SUSPENDED → AssignProcessToJobObject → ResumeThread：
    /// 若子进程启动时已被别的 job 收容（安装器/调试器拉起），直接 Assign 会失败，
    /// 而挂起状态下分配可以保证加入时机（§3.2 已知坑）。
    pub fn spawn<S: AsRef<OsStr>>(&mut self, exe: &Path, args: &[S]) -> Result<u32> {
        let mut cmdline = to_cmd_line(exe, args);
        cmdline.push(0);
        let mut si: STARTUPINFOW = STARTUPINFOW::default();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi = PROCESS_INFORMATION::default();

        unsafe {
            CreateProcessW(
                PCWSTR::null(),
                Some(PWSTR(cmdline.as_mut_ptr())),
                None,
                None,
                false,
                PROCESS_CREATION_FLAGS(CREATE_SUSPENDED.0),
                None,
                PCWSTR::null(),
                &si,
                &mut pi,
            )
        }
        .map_err(Error::from)?;

        // job 分配失败不应让整机模块瘫痪 —— 降级为手动管理生命周期。
        let assigned = unsafe { AssignProcessToJobObject(self.job, pi.hProcess) };
        if assigned.is_err() {
            eprintln!("[warn] AssignProcessToJobObject 失败，降级为手动退出通知");
        }
        unsafe { ResumeThread(pi.hThread) };

        let pid = pi.dwProcessId;
        self.children.push(ChildProcess { pid, process: pi.hProcess, thread: pi.hThread });
        Ok(pid)
    }

    /// 退出顺序（§3.6 写死，绝不能让 app.exe 先退）：
    ///   1. 友好通知 shutdown（本实现通过 Job 反 Providing）
    pub fn shutdown_all(&mut self) {
        for child in self.children.drain(..) {
            let _ = unsafe { windows::Win32::System::Threading::TerminateProcess(child.process, 0) };
            let _ = unsafe { CloseHandle(child.thread) };
            let _ = unsafe { CloseHandle(child.process) };
        }
    }

    pub fn child_count(&self) -> usize {
        self.children.len()
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.shutdown_all();
        let _ = unsafe { CloseHandle(self.job) };
    }
}

fn to_cmd_line<S: AsRef<OsStr>>(exe: &Path, args: &[S]) -> Vec<u16> {
    let mut line = String::from("\"");
    line.push_str(&exe.to_string_lossy());
    line.push('"');
    for a in args {
        line.push(' ');
        line.push('"');
        line.push_str(&a.as_ref().to_string_lossy());
        line.push('"');
    }
    line.encode_utf16().collect()
}

/// 开机自启：计划任务而非 HKCU\Run（§11）。
/// 理由：无 UAC 提示、支持延迟启动、支持 CPU 空闲条件。
pub mod startup {
    use super::*;

    const TASK_NAME: &str = "NexFile.TrayAutostart";

    pub fn install(exe: &PathBuf, delay_seconds: u32) -> Result<()> {
        let tr = format!("\"{}\" --tray", exe.display());
        let delay = format!("{:04}:{:02}", delay_seconds / 60, delay_seconds % 60);
        let script = format!(
            "schtasks.exe /Create /F /TN \"{}\" /TR \"{}\" /SC ONLOGON /DELAY {} /RL LIMITED\n",
            TASK_NAME, tr, delay
        );
        run_hidden_shell(&format!("cmd.exe /c {}", script.trim_end()))
    }

    pub fn remove() -> Result<()> {
        run_hidden_shell(&format!("schtasks.exe /Delete /F /TN \"{}\"", TASK_NAME))
    }

    pub fn is_installed() -> bool {
        let out = std::process::Command::new("schtasks.exe")
            .args(["/Query", "/TN", TASK_NAME])
            .output();
        matches!(out, Ok(o) if o.status.success())
    }

    fn run_hidden_shell(command: &str) -> Result<()> {
        let wide: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
        let mut cmdline = wide.clone();
        let mut si: STARTUPINFOW = STARTUPINFOW::default();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.dwFlags.0 = 0x0000_0001; // STARTF_USESHOWWINDOW
        si.wShowWindow = 0; // SW_HIDE
        let mut pi = PROCESS_INFORMATION::default();
        unsafe {
            CreateProcessW(
                PCWSTR::null(),
                Some(PWSTR(cmdline.as_mut_ptr())),
                None,
                None,
                false,
                PROCESS_CREATION_FLAGS(0),
                None,
                PCWSTR::null(),
                &si,
                &mut pi,
            )
        }
        .map_err(Error::from)?;
        unsafe {
            let _ = windows::Win32::System::Threading::WaitForSingleObject(pi.hProcess, 15000);
            let _ = CloseHandle(pi.hThread);
            let _ = CloseHandle(pi.hProcess);
        }
        Ok(())
    }
}
