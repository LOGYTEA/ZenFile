//! OLE 拖放（第78轮）：把文件拖出 ZenFile 到别的程序，以及接收别的程序拖进来的文件。
//!
//! ## 为什么单独一个模块
//!
//! 仓库里 `app.rs` / `main.rs` 都已经 6000 / 4000 行。拖放有它自己一整套 COM
//! 契约（`IDataObject` / `IDropSource` / `IDropTarget` + PIDL 与 `STGMEDIUM`
//! 的手工内存管理），塞进任何一个都会让两边一起失控。
//!
//! ## 数据对象为什么用 `SHCreateDataObject`
//!
//! 拖出去这一步的本职是"造一个能和 Shell 说话的数据对象"。手工拼 `CF_HDROP`
//! 的 `STGMEDIUM`（`GlobalAlloc` → 填 `DROPFILES` → 双 `\0` 结尾的路径表）要几十
//! 行，而且**只提供 `CF_HDROP`** —— 拖到资源管理器能落地，拖到只认
//! `CFSTR_SHELLIDLIST` 的目标（以及那些要"给我源文件夹的 `IShellFolder`"的）
//! 就只剩一个空手势。`SHCreateDataObject` 是 Shell 自己那份实现：喂它父目录的
//! 绝对 PIDL + 各子项的相对 PIDL，它一次把 `CF_HDROP`、`CFSTR_SHELLIDLIST`
//! 那些全备好，跨卷 / 长路径 / 特殊目录都跟着 Shell 自己的路径走。
//!
//! ## 移动语义（重要，别"贴心地"补一个删除）
//!
//! 出现 `DROPEFFECT_MOVE` 时，**真正去移动文件的是目标程序**：OLE 只是把
//! "源愿意移动"这件事报给目标，目标拿到路径后自己搬。资源管理器作为源时也
//! 不自己删（跨卷拖放它甚至直接退化成复制）。所以这里**绝不能**看到
//! `DROPEFFECT_MOVE` 就回头去删源文件 —— 那会在目标还在慢慢拷贝时把源文件
//! 抽走，或者目标其实拷贝失败了而我们已经删了（数据丢失）。
//! 保持"源什么都不做"这个语义。

use std::path::{Path, PathBuf};

use windows::core::HSTRING;
use windows::Win32::Foundation::{
    HWND, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, S_OK,
};
use windows::Win32::System::Com::{
    CoTaskMemFree, IDataObject, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, TYMED_HGLOBAL,
};
use windows::Win32::System::Ole::{
    DoDragDrop, IDropSource, IDropSource_Impl, ReleaseStgMedium, CF_HDROP, DROPEFFECT,
    DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState;
use windows::Win32::UI::Shell::{
    DragQueryFileW, IShellFolder, SHCreateDataObject, SHGetDesktopFolder, HDROP,
};

/// 绝对 / 相对 PIDL 的裸指针。`ITEMIDLIST` 是不透明变长结构，只能拿指针传，
/// 长度由它自己内部的终止项决定。
type ItemIdList = windows::Win32::UI::Shell::Common::ITEMIDLIST;

/// 一次拖出操作的结果（既给调用方，也是"真跑过一遍"的凭据）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DragOutResult {
    /// `DoDragDrop` 返回的 HRESULT：`DRAGDROP_S_DROP` = 目标收下了，
    /// `DRAGDROP_S_CANCEL` = 用户按了 Esc / 右键取消了。
    pub hr: i32,
    /// 实际生效的 effect（`DROPEFFECT_COPY` / `DROPEFFECT_MOVE` / `DROPEFFECT_NONE`）。
    pub effect: u32,
}

// --------------------------------------------------------------------
// 数据对象
// --------------------------------------------------------------------

/// 释放 `ParseDisplayName` 出来的 PIDL。Shell 用 CoTaskMem 分配它们，
/// 所以只能用 `CoTaskMemFree` —— `free` / `GlobalFree` 都是堆损坏。
unsafe fn free_pidl(p: *mut ItemIdList) {
    if !p.is_null() {
        CoTaskMemFree(Some(p as *const core::ffi::c_void));
    }
}

/// 释放一批 PIDL。
unsafe fn free_pidls(pidls: &[*mut ItemIdList]) {
    for &p in pidls {
        free_pidl(p);
    }
}

/// 为"同一个父目录下一批子项"造 Shell 数据对象。
///
/// PIDL 的拿法照抄 `menu::build_shell_bridge`：`SHGetDesktopFolder` → 对父目录
/// `ParseDisplayName` 拿绝对 PIDL → `BindToObject` 拿父目录的 `IShellFolder`
/// → 对每个子项只用**文件名** `ParseDisplayName` 拿相对 PIDL。别自己发明。
///
/// 返回的第二个元素是本次分到的全部 PIDL（父 + 子），调用方在 `DoDragDrop`
/// 返回后统一释放。解析不出 PIDL 的项会被跳过 —— 拖不出比崩掉好；
/// 一个都解析不出来时返回 `None`。
fn build_data_object(
    hwnd: HWND,
    parent: &Path,
    files: &[PathBuf],
) -> Option<(IDataObject, Vec<*mut ItemIdList>)> {
    unsafe {
        let desktop = SHGetDesktopFolder().ok()?;
        let mut parent_pidl: *mut ItemIdList = std::ptr::null_mut();
        let eaten = 0u32;
        let mut attrs = 0u32;
        // 父目录给**绝对路径**：`ParseDisplayName` 走的是 shell namespace，
        // "C:\Users\x\Desktop" 它认，相对路径不认。
        let dir = HSTRING::from(parent.to_string_lossy().as_ref());
        desktop
            .ParseDisplayName(hwnd, None, &dir, Some(&eaten), &mut parent_pidl, &mut attrs)
            .ok()?;
        if parent_pidl.is_null() {
            return None;
        }
        //  turbofish 只能给 `T`，`P1`（绑定上下文）交给类型推导：这里不传。
        let folder: IShellFolder = desktop.BindToObject(parent_pidl, None).ok()?;

        let mut child: Vec<*mut ItemIdList> = Vec::with_capacity(files.len());
        for f in files {
            // 只给文件名：这是在**父目录的 IShellFolder** 上解析，要的是相对 PIDL。
            let Some(n) = f.file_name() else { continue };
            let leaf = HSTRING::from(n.to_string_lossy().as_ref());
            let mut pidl: *mut ItemIdList = std::ptr::null_mut();
            let e2 = 0u32;
            let mut a2 = 0u32;
            if folder
                .ParseDisplayName(hwnd, None, &leaf, Some(&e2), &mut pidl, &mut a2)
                .is_err()
                || pidl.is_null()
            {
                continue;
            }
            child.push(pidl);
        }
        if child.is_empty() {
            free_pidl(parent_pidl);
            return None;
        }
        let rel: Vec<*const ItemIdList> = child.iter().map(|p| *p as *const _).collect();
        let obj: IDataObject = SHCreateDataObject(Some(parent_pidl as *const _), Some(&rel), None).ok()?;
        Some((obj, std::iter::once(parent_pidl).chain(child).collect()))
    }
}

/// 从数据对象里把 `CF_HDROP` 取出来，读成路径列表。
///
/// 这是"数据对象内容对不对"的凭据：拖出去到底带的是哪几个文件，看这里就够，
/// 不必真拖到资源管理器落地（那会改动用户的文件）。
///
/// 释放走 `ReleaseStgMedium` 而不是 `GlobalFree`：`STGMEDIUM` 的所有权规则由它
/// 统一管 —— `CF_HDROP` 的 `hGlobal` 未必是我们 `GlobalAlloc` 出来的，直接
/// `GlobalFree` 会double free。`STGMEDIUM` 在 windows-rs 里**没有** `Drop`，
/// 所以每个返回分支都得显式放。
pub fn hdrop_paths(obj: &IDataObject) -> Option<Vec<PathBuf>> {
    unsafe {
        let fmt = FORMATETC {
            cfFormat: CF_HDROP.0,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        let mut medium: STGMEDIUM = obj.GetData(&fmt).ok()?;
        let hglobal = medium.u.hGlobal;
        let mut out = None;
        if !hglobal.is_invalid() {
            let hdrop = HDROP(hglobal.0);
            let n = DragQueryFileW(hdrop, 0xFFFF_FFFF, None);
            let mut v = Vec::with_capacity(n as usize);
            for i in 0..n {
                // 先问长度，再按长度取：一次给够缓冲，省掉"短了再来一遍"的分支。
                let need = DragQueryFileW(hdrop, i, None) as usize;
                if need == 0 {
                    continue;
                }
                let mut buf = vec![0u16; need + 1];
                let got = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
                if got == 0 {
                    continue;
                }
                v.push(PathBuf::from(String::from_utf16_lossy(&buf[..got])));
            }
            out = Some(v);
        }
        ReleaseStgMedium(&mut medium);
        out
    }
}

// --------------------------------------------------------------------
// IDropSource
// --------------------------------------------------------------------

/// 拖出期间的"源"回调。OLE 在自己的模态循环里调它，只问两个问题：
/// "还要继续拖吗" 和 "光标怎么画"。
#[windows_core::implement(IDropSource)]
struct DropSource;

impl IDropSource_Impl for DropSource_Impl {
    /// OLE 在每次鼠标移动 / 按键变化时问："还要继续吗？"
    fn QueryContinueDrag(
        &self,
        fescapepressed: windows_core::BOOL,
        _grfkeystate: MODIFIERKEYS_FLAGS,
    ) -> windows_core::HRESULT {
        // `fescapepressed` 是 OLE 替我们查过的 Esc 状态；自己的 `GetKeyState`
        // 再查一遍，两个口径取或，挡住"回调没带、但实际按了"的那一档。
        // `GetKeyState` 最高位为 1 = 键当前处于按下态（于是返回值 < 0）。
        let esc = fescapepressed.as_bool() || unsafe { GetKeyState(0x1B) } < 0;
        if esc {
            return DRAGDROP_S_CANCEL;
        }
        if unsafe { GetKeyState(0x01) } >= 0 {
            // 左键松开 = 用户就在这一刻投放。
            return DRAGDROP_S_DROP;
        }
        if unsafe { GetKeyState(0x02) } < 0 {
            // 拖着的时候按下右键 = 取消（资源管理器同款）。
            return DRAGDROP_S_CANCEL;
        }
        S_OK
    }

    /// OLE 问："光标怎么画？" 返回 `DRAGDROP_S_USEDEFAULTCURSORS` 把这件事
    /// 交回给系统 —— 系统的复制/移动/禁止光标和用户别处看到的一致，
    /// 自己画还要处理 DPI、拖影、跨显示器，收益为负。
    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> windows_core::HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

// --------------------------------------------------------------------
// 拖出：对外主入口
// --------------------------------------------------------------------

/// 把 `parent` 下的 `files` 拖出去，交给 OLE 的模态循环，返回实际生效的 effect。
///
/// **这是模态调用**：`DoDragDrop` 内部自己泵消息，函数返回时拖拽已经结束
/// （投放完成 / 按 Esc 取消 / 出错）。所以调用方必须在它返回**之后**才清
/// `app.drag` 那类状态 —— 提前清的话，鼠标一动就会看到状态已经没了。
///
/// `copy` 为 true 时只向目标声明 `DROPEFFECT_COPY`（按住 Ctrl 的语义）；
/// 否则同时声明"可复制可移动"，由目标按它那边的默认键状态挑。
pub fn drag_out(hwnd: HWND, parent: &Path, files: &[PathBuf], copy: bool) -> DragOutResult {
    let mut out = DragOutResult::default();
    if files.is_empty() {
        trace(&out, files, &[]);
        return out;
    }
    let Some((obj, pidls)) = build_data_object(hwnd, parent, files) else {
        trace(&out, files, &[]);
        return out;
    };
    // 趁数据对象还在手上，把 `CF_HDROP` 读回来核对一遍：能证明"造出来的
    // 数据对象真的装着那几个文件"。也借它填进 trace 文件里当凭据。
    let carried = hdrop_paths(&obj).unwrap_or_default();
    let hr = unsafe {
        let source: IDropSource = DropSource.into();
        let ok_effects = if copy {
            DROPEFFECT_COPY
        } else {
            DROPEFFECT_COPY | DROPEFFECT_MOVE
        };
        let mut effect = DROPEFFECT_NONE;
        let hr = DoDragDrop(&obj, &source, ok_effects, &mut effect);
        out.hr = hr.0;
        out.effect = effect.0;
        hr
    };
    // 数据对象与 PIDL 到这里才释放：`DoDragDrop` 期间目标随时可能回来取数据，
    // 提前释放就是野指针。
    drop(obj);
    unsafe { free_pidls(&pidls) };
    trace(&out, files, &carried);
    let _ = hr;
    out
}

/// 把一次拖出的结果写进 `ZENFILE_DND_TRACE` 指定的文件。
///
/// GUI 进程没有控制台，`eprintln!` 看不见，而"数据对象里到底装了什么"、
/// "`DoDragDrop` 到底返回了什么"这两件事必须能拿到真凭实据。设了环境变量
/// 就 append 一行，没设就是一个 `std::env::var` 的开销，正常用户零成本。
fn trace(res: &DragOutResult, wanted: &[PathBuf], carried: &[PathBuf]) {
    let Ok(path) = std::env::var("ZENFILE_DND_TRACE") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let mut s = String::new();
    s.push_str(&format!(
        "hr=0x{:08X} effect={} wanted={} carried={}\n",
        res.hr,
        res.effect,
        wanted.len(),
        carried.len()
    ));
    for p in wanted {
        s.push_str(&format!("  wanted : {}\n", p.display()));
    }
    for p in carried {
        s.push_str(&format!("  carried: {}\n", p.display()));
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(s.as_bytes());
    }
}
