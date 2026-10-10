//! 读文件/文件夹的 ACL（"属性 → 安全"页要用）。
//!
//! 只做**读**：改权限要写安全描述符、处理继承与规范顺序，一步出错就把文件
//! 变成谁都打不开。资源管理器那一页也是把编辑交给另一个"高级安全设置"窗口，
//! 这里同样只呈现，不提供改写。

use std::path::Path;

use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    AclSizeInformation, DACL_SECURITY_INFORMATION, GetAce, GetAclInformation,
    GROUP_SECURITY_INFORMATION, IsValidSid, LookupAccountSidW, OWNER_SECURITY_INFORMATION,
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, PSID, SID_NAME_USE,
};

/// 一条 ACE（访问控制项）。
#[derive(Clone)]
pub struct Ace {
    /// `域\账户`，查不到就回落到 SID 字符串
    pub account: String,
    pub sid: String,
    /// true = 允许，false = 拒绝
    pub allow: bool,
    /// ACE 是从父项继承下来的（资源管理器里这一行是灰的）
    pub inherited: bool,
    /// 资源管理器那张表里的权限名：完全控制 / 修改 / 读取和执行 / 读取 / 写入 / 特别
    pub right: String,
    pub mask: u32,
}

pub struct Security {
    pub owner: String,
    pub group: String,
    pub aces: Vec<Ace>,
    /// 拿不到（非 NTFS / 没权限 / 路径不存在）时的原因
    pub error: Option<String>,
}

impl Security {
    pub fn is_empty(&self) -> bool {
        self.aces.is_empty() && self.error.is_none()
    }
}

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_DENIED_ACE_TYPE: u8 = 1;
const INHERITED_ACE: u8 = 0x10;

/// 读一个路径的安全描述符（属主 / 属组 / DACL）。
///
/// 一次 `GetNamedSecurityInfoW` 拿到整张表，比逐项问快得多，也是资源管理器的做法。
pub fn read(path: &Path) -> Security {
    let wide: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let mut owner: PSID = PSID::default();
        let mut group: PSID = PSID::default();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd = windows::Win32::Security::PSECURITY_DESCRIPTOR::default();
        let rc = GetNamedSecurityInfoW(
            windows::core::PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            Some(&mut group),
            Some(&mut dacl),
            None,
            &mut sd,
        );
        if rc.0 != 0 {
            return Security {
                owner: String::new(),
                group: String::new(),
                aces: Vec::new(),
                error: Some(format!("GetNamedSecurityInfoW 失败 0x{:08X}", rc.0)),
            };
        }
        let out = Security {
            owner: name_of(owner),
            group: name_of(group),
            aces: aces_of(dacl),
            error: None,
        };
        // 安全描述符是 GetNamedSecurityInfoW 自己分配的，必须还回去
        if !sd.0.is_null() {
            let _ = LocalFree(Some(HLOCAL(sd.0 as *mut _)));
        }
        out
    }
}

unsafe fn name_of(sid: PSID) -> String {
    if sid.0.is_null() || !IsValidSid(sid).as_bool() {
        return String::new();
    }
    let mut name = [0u16; 256];
    let mut name_len = name.len() as u32;
    let mut dom = [0u16; 256];
    let mut dom_len = dom.len() as u32;
    let mut use_ = SID_NAME_USE::default();
    let ok = LookupAccountSidW(
        None,
        sid,
        Some(windows::core::PWSTR(name.as_mut_ptr())),
        &mut name_len,
        Some(windows::core::PWSTR(dom.as_mut_ptr())),
        &mut dom_len,
        &mut use_,
    )
    .is_ok();
    if ok && name_len > 0 {
        let n = String::from_utf16_lossy(&name[..(name_len as usize).min(name.len())]);
        let d = String::from_utf16_lossy(&dom[..(dom_len as usize).min(dom.len())]);
        if d.is_empty() {
            return n;
        }
        return format!("{d}\\{n}");
    }
    sid_string(sid)
}

unsafe fn sid_string(sid: PSID) -> String {
    let mut ptr = windows::core::PWSTR::null();
    if ConvertSidToStringSidW(sid, &mut ptr).is_ok() && !ptr.is_null() {
        let s = ptr.to_string().unwrap_or_default();
        // ConvertSidToStringSidW 的返回要由调用方 LocalFree
        let _ = LocalFree(Some(HLOCAL(ptr.0 as *mut _)));
        return s;
    }
    String::new()
}

unsafe fn aces_of(dacl: *mut ACL) -> Vec<Ace> {
    let mut out = Vec::new();
    if dacl.is_null() {
        return out;
    }
    let mut info = ACL_SIZE_INFORMATION::default();
    if GetAclInformation(
        dacl,
        &mut info as *mut _ as *mut core::ffi::c_void,
        std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
        AclSizeInformation,
    )
    .is_err()
    {
        return out;
    }
    for i in 0..info.AceCount {
        let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
        if GetAce(dacl, i, &mut raw).is_err() || raw.is_null() {
            continue;
        }
        let header = &*(raw as *const ACE_HEADER);
        let allow = match header.AceType {
            ACCESS_ALLOWED_ACE_TYPE => true,
            ACCESS_DENIED_ACE_TYPE => false,
            _ => continue, // 审计项等不呈现
        };
        let ace = &*(raw as *const ACCESS_ALLOWED_ACE);
        // SID 紧跟在 ACCESS_ALLOWED_ACE 的 SidStart 字段处
        let sid = PSID(&ace.SidStart as *const u32 as *mut core::ffi::c_void);
        if !IsValidSid(sid).as_bool() {
            continue;
        }
        out.push(Ace {
            account: name_of(sid),
            sid: sid_string(sid),
            allow,
            inherited: header.AceFlags & INHERITED_ACE != 0,
            right: right_of(ace.Mask),
            mask: ace.Mask,
        });
    }
    out
}

// 资源管理器"安全"页那张表用的就是这几个固定组合，挨个对一遍；
// 对不上就报"特别" —— 说"完全控制"而实际不是，比报"特别"危险得多。
const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
const FILE_MODIFY: u32 = 0x0013_01BF;
const FILE_READ_EXECUTE: u32 = 0x0012_00A9;
const FILE_READ: u32 = 0x0012_0089;
const FILE_WRITE: u32 = 0x0010_0116;

fn right_of(mask: u32) -> String {
    match mask {
        FILE_ALL_ACCESS => "完全控制".to_string(),
        FILE_MODIFY => "修改".to_string(),
        FILE_READ_EXECUTE => "读取和执行".to_string(),
        FILE_READ => "读取".to_string(),
        FILE_WRITE => "写入".to_string(),
        _ => "特别".to_string(),
    }
}
