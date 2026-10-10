//! 本机驱动器清单（盘符、类型、卷标、文件系统、容量）。
//!
//! 为什么单独成模块：主页磁贴、导航窗格树、地址栏面包屑三处都要用同一份
//! 判定结果。以前各处各写一遍 `GetLogicalDrives`，于是主页把 U 盘也叫
//! "本地磁盘"，导航窗格又少了光驱 —— 判定必须只有一个出口。
//!
//! 全部走 Win32 直呼，不碰 COM / WMI：
//!   - `GetLogicalDrives` 拿盘符位掩码（一次 syscall）
//!   - `GetDriveTypeW` 判类型（不访问介质，不会弹"请插入磁盘"）
//!   - `GetVolumeInformationW` 拿卷标与文件系统，失败=介质未就绪
//!   - `GetDiskFreeSpaceExW` 拿容量

use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
};

/// `Win32::Storage::FileSystem` 里没有对应的常量符号，按 Win32 头文件原值定义。
const DRIVE_UNKNOWN: u32 = 0;
const DRIVE_NO_ROOT_DIR: u32 = 1;
const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;
const DRIVE_CDROM: u32 = 5;
const DRIVE_RAMDISK: u32 = 6;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DriveKind {
    /// 本地硬盘（含 USB 移动硬盘——现代 Windows 对它们同样报 FIXED）
    Fixed,
    /// U 盘 / 读卡器：DRIVE_REMOVABLE
    Removable,
    /// 光驱
    Optical,
    /// 网络映射驱动器
    Network,
    /// 内存盘
    Ram,
    Unknown,
}

impl DriveKind {
    /// 资源管理器在"无卷标"时用的那个名字。
    pub fn name(&self) -> &'static str {
        match self {
            DriveKind::Fixed => "本地磁盘",
            DriveKind::Removable => "可移动磁盘",
            DriveKind::Optical => "DVD 驱动器",
            DriveKind::Network => "网络驱动器",
            DriveKind::Ram => "内存盘",
            DriveKind::Unknown => "磁盘",
        }
    }

    /// 一句话说明，给状态栏/属性窗口用。
    pub fn description(&self) -> &'static str {
        match self {
            DriveKind::Fixed => "本地硬盘",
            DriveKind::Removable => "可移动存储设备（U 盘 / 存储卡）",
            DriveKind::Optical => "光盘驱动器",
            DriveKind::Network => "映射到网络共享的位置",
            DriveKind::Ram => "内存虚拟磁盘",
            DriveKind::Unknown => "未知类型",
        }
    }

    pub fn from_raw(v: u32) -> Self {
        match v {
            DRIVE_REMOVABLE => DriveKind::Removable,
            DRIVE_FIXED => DriveKind::Fixed,
            DRIVE_REMOTE => DriveKind::Network,
            DRIVE_CDROM => DriveKind::Optical,
            DRIVE_RAMDISK => DriveKind::Ram,
            DRIVE_NO_ROOT_DIR | DRIVE_UNKNOWN => DriveKind::Unknown,
            _ => DriveKind::Unknown,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DriveInfo {
    /// 形如 `C:\`
    pub root: String,
    pub kind: DriveKind,
    /// 盘符单独取出来，避免各处反复解析字符串
    pub letter: char,
    /// 卷标；空串表示没有（资源管理器此时改用类型名）
    pub label: String,
    /// 文件系统名，如 `NTFS`；取不到通常是介质没就绪
    pub fs: String,
    /// 介质是否已就绪（空读卡器、无盘光驱为 false）
    pub ready: bool,
    pub total: Option<u64>,
    pub available: Option<u64>,
}

impl DriveInfo {
    /// 盘符，形如 `C:`
    pub fn letter_str(&self) -> String {
        format!("{}:", self.letter)
    }

    /// 资源管理器显示的那个名字。
    ///
    /// 规则：有卷标就"<卷标> (<盘符>)"，否则"<类型名> (<盘符>)"。
    pub fn display_name(&self) -> String {
        if self.label.is_empty() {
            format!("{} ({})", self.kind.name(), self.letter_str())
        } else {
            format!("{} ({})", self.label, self.letter_str())
        }
    }

    /// 已用字节数。任一项缺失就返回 None。
    pub fn used(&self) -> Option<u64> {
        match (self.total, self.available) {
            (Some(t), Some(a)) => Some(t.saturating_sub(a)),
            _ => None,
        }
    }

    /// 已用占比，0.0 ~ 1.0。
    pub fn used_ratio(&self) -> Option<f64> {
        match (self.total, self.used()) {
            (Some(t), Some(u)) if t > 0 => Some((u as f64 / t as f64).clamp(0.0, 1.0)),
            _ => None,
        }
    }

    /// 剩余空间是否告警级别（资源管理器在低于 10% 时把条画成红色）。
    pub fn critical(&self) -> bool {
        match (self.total, self.available) {
            (Some(t), Some(a)) if t > 0 => (a as f64 / t as f64) < 0.1,
            _ => false,
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(v: &[u16]) -> String {
    let end = v.iter().position(|&c| c == 0).unwrap_or(v.len());
    String::from_utf16_lossy(&v[..end])
}

/// 单个根目录的完整信息。
pub fn info(root: &str) -> DriveInfo {
    let w = wide(root);
    let letter = root.chars().next().unwrap_or('?').to_ascii_uppercase();
    let kind = DriveKind::from_raw(unsafe { GetDriveTypeW(PCWSTR(w.as_ptr())) });

    let mut label_buf = [0u16; 256];
    let mut fs_buf = [0u16; 64];
    let vol = unsafe {
        GetVolumeInformationW(
            PCWSTR(w.as_ptr()),
            Some(&mut label_buf),
            None,
            None,
            None,
            Some(&mut fs_buf),
        )
    };
    let ready = vol.is_ok();
    let label = if ready { from_wide(&label_buf).trim().to_string() } else { String::new() };
    let fs = if ready { from_wide(&fs_buf) } else { String::new() };

    let mut available = 0u64;
    let mut total = 0u64;
    let mut total_free = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(w.as_ptr()),
            Some(&mut available),
            Some(&mut total),
            Some(&mut total_free),
        )
        .is_ok()
    };
    let (total, available) = if ok && total > 0 {
        (Some(total), Some(available))
    } else {
        (None, None)
    };

    DriveInfo {
        root: root.to_string(),
        kind,
        letter,
        label,
        fs,
        // GetVolumeInformation 失败基本等于介质没就绪；但网络盘离线时
        // GetDiskFreeSpaceEx 也会失败，二者取其一就够了。
        ready: ready && total.is_some(),
        total,
        available,
    }
}

/// 全部逻辑驱动器。**已按盘符排序**，可直接用于列表渲染。
pub fn list() -> Vec<DriveInfo> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u32)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| info(&format!("{}:\\", (b'A' + i as u8) as char)))
        .collect()
}

/// 只看固定本地硬盘（建搜索索引时的根集合；U 盘不该被长期索引）。
pub fn fixed_roots() -> Vec<String> {
    list()
        .into_iter()
        .filter(|d| d.kind == DriveKind::Fixed)
        .map(|d| d.root)
        .collect()
}

/// 容量（总、可用）。保留这个函数名：旧调用方还在用。
pub fn space(root: &str) -> Option<(u64, u64)> {
    let d = info(root);
    match (d.total, d.available) {
        (Some(t), Some(a)) => Some((t, a)),
        _ => None,
    }
}
