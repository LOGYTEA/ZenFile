//! 自绘右键菜单（技术方案 §7）。
//!
//! 三个已被核实的现实约束：
//!   1. Win11 上通过 IContextMenu 拿到的是传统菜单（等价"显示更多选项"层），
//!      现代一级菜单的 XAML 原生项无法获取；
//!   2. 只能加载与程序位数一致的 Shell 扩展（64 位程序不加载 32 位扩展）；
//!   3. 拿到内容后菜单需自绘，键盘导航、高 DPI、无障碍要自己补。
//!
//! 因此本模块的做法是：
//!   QueryContextMenu → 遍历 HMENU 抽取全部条目 → 用自绘窗口渲染 → InvokeCommand 执行，
//!   并提供"显示更多选项"兜底（Shift+右键直接调 Explorer 原生菜单）。

use std::path::{Path, PathBuf};

use windows::core::{HSTRING, PCWSTR};
use windows::core::Interface as _;
use windows::Win32::Foundation::{LPARAM, LRESULT, RECT, WPARAM, HWND};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, GetMonitorInfoW,
    IntersectClipRect, MonitorFromWindow, SelectObject, SetBkMode, SetTextColor, SetWindowOrgEx,
    DT_CALCRECT, DT_CENTER, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, HBRUSH, HDC,
    HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{IsWindow, PostMessageW};
use windows::Win32::UI::Shell::{
    CMINVOKECOMMANDINFOEX, CMF_EXPLORE, CMF_EXTENDEDVERBS, CMF_ITEMMENU, IShellFolder,
    IContextMenu, IContextMenu3, SHGetDesktopFolder,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, GetMenuItemCount,
    GetMenuItemInfoW, GetWindowLongPtrW, RegisterClassExW, SetWindowLongPtrW, ShowWindow,
    TrackPopupMenu, GWLP_USERDATA, HMENU, MENUITEMINFOW,
    MFS_DISABLED, MFT_SEPARATOR, MIIM_BITMAP, MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING,
    MIIM_SUBMENU, SW_SHOWNOACTIVATE, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_DESTROY, WM_ERASEBKGND,
    WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT,
    WM_RBUTTONUP, WNDCLASSEXW, WS_BORDER, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::theme::Theme;

// 自绘菜单的几条私有消息（WM_USER 起，不会和系统消息冲突）。
// 子菜单是一个独立窗口且各自 SetCapture，父子之间必须靠消息协调，
// 否则鼠标移回父菜单时父菜单收不到 WM_MOUSEMOVE，高亮和子菜单就都卡住不动了。
/// 子菜单把鼠标交还给父菜单（lparam 打包屏幕坐标，同 WM_MOUSEMOVE 的打包方式）
const WM_MENU_RETURN: u32 = 0x0401;
/// 子菜单窗口已销毁，父菜单只需清掉自己持有的句柄
const WM_MENU_SUBCLOSED: u32 = 0x0402;
/// 子菜单里的条目被真正执行，整条菜单链都要关掉（不能只关最里层）
const WM_MENU_CLOSE_CHAIN: u32 = 0x0403;

/// 宿主侧自有的命令（自绘菜单的顶部图标行与常规项）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    Open,
    Copy,
    Cut,
    Paste,
    Rename,
    Delete,
    DeletePermanent,
    NewFolder,
    NewFile,
    SelectAll,
    Refresh,
    ViewModeCycle,
    ShowNativeMenu,
    Properties,
    // —— 标题栏「更多」菜单专用 ——
    ViewIcons,
    ViewList,
    ViewDetails,
    SortByName,
    SortByModified,
    SortByType,
    SortBySize,
    ToggleSidebar,
    ToggleStatusbar,
    /// 显示/隐藏"隐藏的项目"（FILE_ATTRIBUTE_HIDDEN）
    ToggleHidden,
    /// 显示/隐藏"受保护的操作系统文件"（FILE_ATTRIBUTE_SYSTEM）
    ToggleSystem,
    NewTab,
    About,
}

#[derive(Clone, Copy, Debug)]
pub enum MenuAction {
    Own(Command),
    /// Shell 命令偏移（InvokeCommand 用 MAKEINTRESOURCE 形式传递）
    Shell(u32),
    None,
}

#[derive(Clone)]
pub struct MenuEntry {
    pub text: String,
    /// Segoe MDL2 Assets 图标字符（自有命令使用）
    pub glyph: Option<&'static str>,
    /// Shell 提供的位图（第三方扩展条目）
    pub bmp: Option<isize>,
    pub separator: bool,
    pub disabled: bool,
    pub has_submenu: bool,
    pub submenu: Vec<MenuEntry>,
    pub action: MenuAction,
}

impl MenuEntry {
    pub fn sep() -> Self {
        MenuEntry {
            text: String::new(),
            glyph: None,
            bmp: None,
            separator: true,
            disabled: false,
            has_submenu: false,
            submenu: Vec::new(),
            action: MenuAction::None,
        }
    }
}

/// Shell 菜单桥：持有 HMENU 与 IContextMenu，生命周期必须覆盖菜单显示全过程。
pub struct ShellMenuBridge {
    menu: Option<IContextMenu>,
    menu3: Option<IContextMenu3>,
    hmenu: Option<HMENU>,
    first_id: u32,
}

impl Drop for ShellMenuBridge {
    fn drop(&mut self) {
        if let Some(h) = self.hmenu {
            let _ = unsafe { DestroyMenu(h) };
        }
    }
}

const CMD_FIRST: u32 = 0x6000;
const CMD_LAST: u32 = 0xF000;

/// 为一组文件构建 Shell 上下文菜单（必须全部同父目录）。
/// 返回 None 表示 Shell 拒绝（例如跨目录多选），此时退化为纯自有菜单。
pub fn build_shell_bridge(hwnd: HWND, parent: &Path, files: &[PathBuf], extended: bool) -> Option<ShellMenuBridge> {
    unsafe {
        let desktop: IShellFolder = SHGetDesktopFolder().ok()?;
        let mut attrs = 0u32;
        let mut parent_pidl: *mut windows::Win32::UI::Shell::Common::ITEMIDLIST = std::ptr::null_mut();
        desktop
            .ParseDisplayName(hwnd, None, &HSTRING::from(parent.to_string_lossy().as_ref()), None, &mut parent_pidl, &mut attrs)
            .ok()?;
        if parent_pidl.is_null() {
            return None;
        }
        let folder: IShellFolder = desktop.BindToObject(parent_pidl, None).ok()?;
        let mut pidls: Vec<*mut windows::Win32::UI::Shell::Common::ITEMIDLIST> = Vec::new();
        for f in files {
            let name = f.file_name()?.to_string_lossy().into_owned();
            let eaten = 0u32;
            let mut attrs2 = 0u32;
            let mut pidl: *mut windows::Win32::UI::Shell::Common::ITEMIDLIST = std::ptr::null_mut();
            if folder
                .ParseDisplayName(hwnd, None, &HSTRING::from(name.as_str()), Some(&eaten), &mut pidl, &mut attrs2)
                .is_err()
                || pidl.is_null()
            {
                continue;
            }
            pidls.push(pidl);
        }
        if pidls.is_empty() {
            CoTaskMemFree(Some(parent_pidl as *const core::ffi::c_void));
            return None;
        }
        let slice: Vec<*const windows::Win32::UI::Shell::Common::ITEMIDLIST> =
            pidls.iter().map(|p| *p as *const _).collect();
        let ctx: IContextMenu = folder.GetUIObjectOf(hwnd, &slice, None).ok()?;

        let hmenu = CreatePopupMenu().ok()?;
        let mut flags = CMF_EXPLORE | CMF_ITEMMENU;
        if extended {
            flags |= CMF_EXTENDEDVERBS;
        }
        if ctx.QueryContextMenu(hmenu, 0, CMD_FIRST, CMD_LAST, flags).is_err() {
            let _ = DestroyMenu(hmenu);
            return None;
        }

        let bridge = ShellMenuBridge {
            menu3: ctx.cast::<IContextMenu3>().ok(),
            menu: Some(ctx),
            hmenu: Some(hmenu),
            first_id: CMD_FIRST,
        };
        // PIDL 与 parent_pidl 在 bridge 存活期间由绑定持有语义，菜单显示结束后释放。
        for p in pidls {
            CoTaskMemFree(Some(p as *const core::ffi::c_void));
        }
        CoTaskMemFree(Some(parent_pidl as *const core::ffi::c_void));
        Some(bridge)
    }
}

/// 把 HMENU 转成结构化的自绘条目。递归深度限制为 2 层，超出用"显示更多选项"承接。
pub fn entries_from_bridge(bridge: &ShellMenuBridge) -> Vec<MenuEntry> {
    let mut out = Vec::new();
    let Some(hmenu) = bridge.hmenu else { return out };
    unsafe {
        let count = GetMenuItemCount(Some(hmenu));
        for i in 0..count {
            if let Some(e) = read_entry(hmenu, i as u32, bridge.first_id, 0) {
                // 折叠连续的重复分隔符，观感更接近原生
                if e.separator && out.last().map(|l: &MenuEntry| l.separator).unwrap_or(false) {
                    continue;
                }
                out.push(e);
            }
        }
    }
    out
}

fn read_entry(hmenu: HMENU, pos: u32, first_id: u32, depth: usize) -> Option<MenuEntry> {
    unsafe {
        let mut buf = vec![0u16; 512];
        let mut mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_ID | MIIM_STRING | MIIM_STATE | MIIM_SUBMENU | MIIM_FTYPE | MIIM_BITMAP,
            dwTypeData: windows::core::PWSTR(buf.as_mut_ptr()),
            cch: buf.len() as u32,
            ..Default::default()
        };
        if GetMenuItemInfoW(hmenu, pos, true, &mut mii).is_err() {
            return None;
        }
        let text = crate::icons::from_wide(&buf);
        if (mii.fType & MFT_SEPARATOR) == MFT_SEPARATOR {
            return Some(MenuEntry::sep());
        }
        let disabled = (mii.fState & MFS_DISABLED) == MFS_DISABLED;
        let has_submenu = !mii.hSubMenu.is_invalid() && !mii.hSubMenu.0.is_null();
        let mut submenu = Vec::new();
        if has_submenu && depth < 2 {
            let count = GetMenuItemCount(Some(mii.hSubMenu));
            for j in 0..count {
                if let Some(child) = read_entry(mii.hSubMenu, j as u32, first_id, depth + 1) {
                    submenu.push(child);
                }
            }
        }
        let action = if mii.wID >= first_id {
            MenuAction::Shell(mii.wID - first_id)
        } else if has_submenu {
            MenuAction::None
        } else {
            MenuAction::None
        };
        let bmp = if isize::BITS > 0 && !mii.hbmpItem.is_invalid() && !mii.hbmpItem.0.is_null() {
            Some(mii.hbmpItem.0 as isize)
        } else {
            None
        };
        Some(MenuEntry {
            text: if text.is_empty() { "(无法读取的菜单项)".into() } else { text },
            glyph: None,
            bmp,
            separator: false,
            disabled,
            has_submenu: has_submenu && !submenu.is_empty(),
            submenu,
            action,
        })
    }
}

/// 执行 Shell 菜单项。`id_offset` 取自 MenuAction::Shell。
pub fn invoke(bridge: &ShellMenuBridge, hwnd: HWND, id_offset: u32) -> bool {
    let Some(ctx) = &bridge.menu else { return false };
    unsafe {
        let info = CMINVOKECOMMANDINFOEX {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: 0x00004000, // CMIC_MASK_UNICODE
            hwnd,
            lpVerbW: PCWSTR(id_offset as usize as *const u16),
            lpParametersW: PCWSTR::null(),
            lpDirectoryW: PCWSTR::null(),
            nShow: 5,
            ..Default::default()
        };
        if bridge.first_id.checked_add(id_offset).is_none() {
            return false;
        }
        ctx.InvokeCommand(&info as *const _ as *const windows::Win32::UI::Shell::CMINVOKECOMMANDINFO)
            .is_ok()
    }
}

/// 兜底：直接调起 Explorer 原生菜单（Shift+右键 或"显示更多选项"）。
pub fn show_native(hwnd: HWND, parent: &Path, files: &[PathBuf]) -> bool {
    let Some(bridge) = build_shell_bridge(hwnd, parent, files, true) else {
        return false;
    };
    let Some(hmenu) = bridge.hmenu else { return false };
    let mut pt = windows::Win32::Foundation::POINT::default();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);
        let cmd = TrackPopupMenu(
            hmenu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            Some(0),
            hwnd,
            None,
        );
        if cmd.0 != 0 {
            return invoke(&bridge, hwnd, (cmd.0 as u32).saturating_sub(bridge.first_id));
        }
    }
    false
}

// ---------------------------------------------------------------------------
// 自绘菜单窗口
// ---------------------------------------------------------------------------

pub const MENU_ROW: i32 = 28;
pub const MENU_ICON_W: i32 = 30;

/// 菜单窗口自身的状态。App 只保存 hwnd 与 bridge。
struct MenuWindow {
    theme: Theme,
    entries: Vec<MenuEntry>,
    hover: Option<usize>,
    app: *mut crate::app::App,
    scale: f32,
    row_h: i32,
    width: i32,
    /// 菜单可见高度（可能小于内容高度，此时可滚动）
    view_h: i32,
    /// 内容滚动偏移
    scroll: i32,
    parent_hwnd: HWND,
    submenu: Option<HWND>,
    open_submenu_for: Option<usize>,
    /// 这是作为**另一级菜单的子菜单**弹出的；顶层菜单为 None。
    parent_menu: Option<HWND>,
    /// 本窗口里已经有条目被真正执行（用于决定销毁时是否级联关闭父菜单）。
    activated: bool,
}

pub fn register_menu_class() {
    let cls = windows::core::HSTRING::from("NexFileMenu");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(menu_proc),
        lpszClassName: windows::core::PCWSTR(cls.as_ptr()),
        hbrBackground: HBRUSH(std::ptr::null_mut()),
        ..Default::default()
    };
    unsafe {
        RegisterClassExW(&wc);
    }
}

/// 弹出一级菜单。
///
/// `parent_menu`：本窗口作为子菜单弹出时，传父菜单的 HWND；顶层菜单传 None。
/// 子菜单靠它把鼠标交还回去，并在自己销毁时通知父菜单清理句柄。
pub fn open_menu(
    app: *mut crate::app::App,
    parent_hwnd: HWND,
    entries: Vec<MenuEntry>,
    x: i32,
    y: i32,
    scale: f32,
    theme: Theme,
    parent_menu: Option<HWND>,
) -> Option<HWND> {
    let cls = HSTRING::from("NexFileMenu");
    if entries.is_empty() {
        return None;
    }
    let row_h = (MENU_ROW as f32 * scale).round() as i32;
    let content_h: i32 = entries.iter().map(|e| if e.separator { 6 } else { row_h }).sum::<i32>() + 8;

    let measure_hdc = unsafe { windows::Win32::Graphics::Gdi::GetDC(Some(parent_hwnd)) };
    let width = {
        let mut max_text = 8i32;
        for e in &entries {
            let mut len = e.text.chars().count() as i32;
            if e.has_submenu {
                len += 3;
            }
            max_text = max_text.max(len);
        }
        let font = crate::paint::menu_font(scale);
        let old = unsafe { SelectObject(measure_hdc, font.into()) };
        let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // 不带结尾 NUL：DrawTextW 的 cchText 取切片长度，多算一个 U+0000
        // 会让测宽偏大（并多画一个 .notdef 方框）。
        let mut probe: Vec<u16> = "平均宽度平均宽度".encode_utf16().collect();
        unsafe {
            DrawTextW(measure_hdc, &mut probe, &mut rect, DT_CALCRECT | DT_NOPREFIX);
        }
        let per_char = (rect.right - rect.left).max(1) / 8;
        let _ = unsafe { SelectObject(measure_hdc, old) };
        let _ = unsafe { windows::Win32::Graphics::Gdi::ReleaseDC(Some(parent_hwnd), measure_hdc) };
        (MENU_ICON_W as f32 * scale) as i32 + per_char * max_text + (48.0 * scale) as i32
    };

    // 菜单可能比屏幕还高（第三方 Shell 扩展会把条目堆到几百行）。若不限制高度，
    // 超出屏幕的条目虽然看不见，却仍然参与命中判定 —— 点击与显示就对不上了。
    // 这里按光标所在显示器的工作区限高，并把窗口整体上移保证底部可见。
    let (final_x, final_y, view_h) = fit_to_screen(parent_hwnd, x, y, width, content_h);

    let state = Box::into_raw(Box::new(MenuWindow {
        theme,
        entries,
        hover: None,
        app,
        scale,
        row_h,
        width: width.max((160.0 * scale) as i32),
        view_h,
        scroll: 0,
        parent_hwnd,
        submenu: None,
        open_submenu_for: None,
        parent_menu,
        activated: false,
    }));

    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP | WS_BORDER,
            final_x,
            final_y,
            width.max((160.0 * scale) as i32),
            view_h,
            Some(parent_hwnd),
            None,
            None,
            Some(state as *const core::ffi::c_void),
        )
    }
    .ok()?;
    unsafe {
        let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = SetCapture(hwnd);
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
    Some(hwnd)
}

/// 把弹出菜单限制在光标所在显示器的工作区内。
///
/// 返回 (窗口左上 x, 窗口左上 y, 可见高度)。内容比可见高度长时由 `scroll` 负责滚动，
/// 保证"看得见的条目"和"点得到的条目"始终是同一批。
fn fit_to_screen(parent: HWND, x: i32, y: i32, width: i32, content_h: i32) -> (i32, i32, i32) {
    type Rect = RECT;
    let mut wa: Rect = Rect::default();
    unsafe {
        let mon = MonitorFromWindow(parent, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            wa = mi.rcWork;
        }
    }
    if wa.right <= wa.left || wa.bottom <= wa.top {
        // 取不到工作区就退化为不限制
        return (x, y, content_h);
    }
    let margin = 8;
    let max_h = (wa.bottom - wa.top - margin * 2).max(120);
    let view_h = content_h.min(max_h);

    // 纵向：优先向下，放不下就整体上移；仍放不下则保证顶部不低于工作区顶端
    let mut top = y.max(wa.top + margin);
    if top + view_h > wa.bottom - margin {
        top = (wa.bottom - margin - view_h).max(wa.top + margin);
    }
    // 横向同理，避免菜单右侧被裁掉
    let mut left = x;
    if left + width > wa.right - margin {
        left = (wa.right - margin - width).max(wa.left + margin);
    }
    if left < wa.left + margin {
        left = wa.left + margin;
    }
    (left, top, view_h)
}

/// 菜单内容区的总高度（含上下 4px 内边距）。
fn content_height(state: &MenuWindow) -> i32 {
    let mut top = 4;
    for e in &state.entries {
        top += if e.separator { 6 } else { state.row_h };
    }
    top + 4
}

/// 最大可滚动距离。
fn max_scroll(state: &MenuWindow) -> i32 {
    (content_height(state) - state.view_h).max(0)
}

fn inside_popup(state: &MenuWindow, x: i32, y: i32) -> bool {
    x >= 0 && x <= state.width && y >= 0 && y <= state.view_h
}

fn entry_at(state: &MenuWindow, y: i32) -> Option<usize> {
    let y = y + state.scroll; // 命中判定换算回内容坐标
    let mut top = 4;
    for (i, e) in state.entries.iter().enumerate() {
        let h = if e.separator { 6 } else { state.row_h };
        if y >= top && y < top + h {
            if e.separator {
                return None;
            }
            return Some(i);
        }
        top += h;
    }
    None
}

fn row_rect(state: &MenuWindow, index: usize) -> RECT {
    let mut top = 4;
    for e in state.entries.iter().take(index) {
        top += if e.separator { 6 } else { state.row_h };
    }
    let pad = (4.0 * state.scale) as i32;
    RECT { left: pad, top, right: state.width - pad, bottom: top + state.row_h }
}

unsafe extern "system" fn menu_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut MenuWindow;
    if state_ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let state = &mut *state_ptr;

    match msg {
        WM_MOUSEACTIVATE => return LRESULT(3), // MA_NOACTIVATE：不抢宿主焦点
        WM_ERASEBKGND => return LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            paint_menu(hdc, state);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let y = (lparam.0 >> 16) as i16 as i32;
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            // 子菜单弹出后由**子菜单**持有 capture，本窗口就再也收不到 WM_MOUSEMOVE。
            // 于是鼠标移回父菜单时高亮会一直卡在展开子菜单的那一项上。这里让子菜单在
            // 光标越过自己左边界时把 capture 交还回来，并附带上屏幕坐标重算高亮。
            // 是否该把鼠标交还给父菜单：光标离开本菜单，且落回父菜单的矩形内。
            // 只判 x<0 是不够的 —— 子菜单被屏幕边界挤到父菜单**左侧**时，
            // 光标往右移回父菜单，x 是大于宽度的，条件不成立，父子就此失联。
            let back_to_parent = state.parent_menu.is_some()
                && !inside_popup(state, x, y)
                && (x < 0 || {
                    let p = state.parent_menu.unwrap();
                    let mut pr = RECT::default();
                    let mut sp = POINT { x, y };
                    let _ = ClientToScreen(hwnd, &mut sp);
                    let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(p, &mut pr);
                    sp.x >= pr.left - 2 && sp.x <= pr.right + 2 && sp.y >= pr.top && sp.y <= pr.bottom
                });
            if back_to_parent {
                let mut pt = POINT { x, y };
                let _ = ClientToScreen(hwnd, &mut pt);
                if let Some(p) = state.parent_menu {
                    let _ = SetCapture(p);
                    let _ = PostMessageW(
                        Some(p),
                        WM_MENU_RETURN,
                        WPARAM(0),
                        LPARAM(((pt.y as isize) << 16) | (pt.x as isize & 0xFFFF)),
                    );
                }
                return LRESULT(0);
            }
            // 菜单持有鼠标 capture，屏幕任何位置都会走到这里 —— 必须先判定是否真的在弹出菜单内
            let idx = if inside_popup(state, x, y) {
                entry_at(state, y)
            } else {
                None
            };
            if idx != state.hover {
                match idx {
                    // 悬停到带子菜单的项 → 直接展开（用户要的就是"悬停能展开"）
                    Some(i)
                        if state.entries[i].has_submenu && !state.entries[i].submenu.is_empty() =>
                    {
                        open_submenu_at(state, hwnd, i);
                    }
                    _ => {
                        if let Some(sub) = state.submenu.take() {
                            let _ = DestroyWindow(sub);
                            // 子菜单销毁时自己调了 ReleaseCapture —— 那一次释放掉的
                            // 正是**本窗口**的 capture。不立刻抢回来的话，本窗口从此
                            // 收不到任何鼠标消息：悬停不再响应、点击外面也关不掉菜单。
                            let _ = SetCapture(hwnd);
                        }
                        state.open_submenu_for = None;
                    }
                }
                state.hover = idx;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        // 子菜单把鼠标交还回来：清掉子菜单句柄，按真实光标位置重算高亮
        WM_MENU_RETURN => {
            let sx = (lparam.0 & 0xFFFF) as i16 as i32;
            let sy = (lparam.0 >> 16) as i16 as i32;
            let mut pt = POINT { x: sx, y: sy };
            let _ = ScreenToClient(hwnd, &mut pt);
            if let Some(sub) = state.submenu.take() {
                let _ = DestroyWindow(sub);
            }
            state.open_submenu_for = None;
            // 同理：销毁子菜单会连本窗口的 capture 一起释放掉，必须抢回来
            let _ = SetCapture(hwnd);
            let idx = if inside_popup(state, pt.x, pt.y) {
                entry_at(state, pt.y)
            } else {
                None
            };
            if idx != state.hover {
                state.hover = idx;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_MENU_SUBCLOSED => {
            state.submenu = None;
            state.open_submenu_for = None;
            // 子菜单销毁时释放了 capture，父菜单必须自己抢回来，
            // 否则从此收不到鼠标消息 —— 菜单就"关不掉"了。
            let _ = SetCapture(hwnd);
            LRESULT(0)
        }
        WM_MENU_CLOSE_CHAIN => {
            // 子菜单里的条目被真正执行了：整条菜单链一起关，
            // 只关最里层会留下一个还开着的空壳父菜单。
            state.submenu = None;
            state.open_submenu_for = None;
            state.activated = true;
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let y = (lparam.0 >> 16) as i16 as i32;
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            // 关键：因为有 SetCapture，屏幕任意位置的点击都会投递到这里。
            // 落在弹出菜单之外时只能关闭菜单，绝不能触发任何条目 ——
            // 之前缺这个判断，导致每条选项的点击热区在水平方向上等同于整块屏幕。
            if !inside_popup(state, x, y) {
                // 点空白处 = 关掉**整条**菜单链。只销毁自己会让父菜单变成
                // 一个还挂在屏幕上、却再也收不到鼠标消息的空壳（表现就是"菜单关不掉"）。
                state.activated = true;
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            if let Some(idx) = entry_at(state, y) {
                let entry = state.entries[idx].clone();
                activate(state, hwnd, &entry);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            // 菜单开着时再点右键：资源管理器是整条菜单收起
            state.activated = true;
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            // 内容超过可见高度时用滚轮滚动
            let delta = (wparam.0 >> 16) as i16 as i32;
            let limit = max_scroll(state);
            state.scroll = (state.scroll - delta / 4).clamp(0, limit);
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let key = wparam.0 as i32;
            if key == 0x1B {
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            if key == 0x28 || key == 0x26 {
                let cur = state.hover.unwrap_or(usize::MAX);
                let next = if key == 0x28 {
                    state.entries.iter().enumerate().find(|(i, e)| *i > cur && !e.separator).map(|(i, _)| i)
                        .or_else(|| state.entries.iter().position(|e| !e.separator))
                } else {
                    state.entries.iter().enumerate().rev().find(|(i, e)| *i < cur && !e.separator).map(|(i, _)| i)
                        .or_else(|| state.entries.iter().rposition(|e| !e.separator))
                };
                state.hover = next;
                let _ = InvalidateRect(Some(hwnd), None, false);
                return LRESULT(0);
            }
            if key == 0x0D && state.hover.is_some() {
                let entry = state.entries[state.hover.unwrap()].clone();
                activate(state, hwnd, &entry);
            }
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            if let Some(sub) = state.submenu {
                let _ = DestroyWindow(sub);
            }
            let _ = ReleaseCapture();
            let app = state.app;
            let parent_hwnd = state.parent_hwnd;
            let parent_menu = state.parent_menu;
            let activated = state.activated;
            drop(Box::from_raw(state_ptr));
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            // 只有**顶层**菜单才通知宿主；子菜单被销毁时要告诉父菜单清句柄，
            // 否则父菜单会一直持有并试图销毁一个已经不存在的窗口。
            if let Some(p) = parent_menu {
                if IsWindow(Some(p)).as_bool() {
                    let _ = PostMessageW(
                        Some(p),
                        if activated { WM_MENU_CLOSE_CHAIN } else { WM_MENU_SUBCLOSED },
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
            if parent_menu.is_none() && !app.is_null() {
                let _ = InvalidateRect(Some(parent_hwnd), None, true);
                (*app).notify_menu_closed(hwnd);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 在第 `index` 项右侧展开子菜单。
///
/// 子菜单是一级独立窗口、有自己的 SetCapture，所以展开后父菜单收不到鼠标消息 ——
/// 由父菜单的 WM_MENU_RETURN / 子菜单越左边界交还 capture 来协调。
fn open_submenu_at(state: &mut MenuWindow, hwnd: HWND, index: usize) {
    if state.open_submenu_for == Some(index) {
        return;
    }
    if let Some(sub) = state.submenu.take() {
        unsafe {
            let _ = DestroyWindow(sub);
        }
    }
    let mut win = RECT::default();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut win);
    }
    // 子菜单顶端与这一项对齐：累加它前面所有条目的高度（含滚动偏移）
    let mut top = 4 - state.scroll;
    for e in state.entries.iter().take(index) {
        top += if e.separator { 6 } else { state.row_h };
    }
    let entries = state.entries[index].submenu.clone();
    let sub = open_menu(
        state.app,
        hwnd,
        entries,
        win.right - 2,
        win.top + top,
        state.scale,
        state.theme,
        Some(hwnd),
    );
    match sub {
        Some(s) => {
            state.submenu = Some(s);
            state.open_submenu_for = Some(index);
        }
        None => state.open_submenu_for = None,
    }
}

fn activate(state: &mut MenuWindow, hwnd: HWND, entry: &MenuEntry) {
    if entry.disabled {
        return;
    }
    if entry.has_submenu && !entry.submenu.is_empty() {
        // 点击带子菜单的项：沿用与悬停展开完全相同的定位逻辑。
        // （原来这里用 `std::ptr::eq` 在 entries 里找自己，而 entry 是 clone 出来的，
        //   地址永远不相等 → 定位恒失败，子菜单总是弹在菜单顶部第一行。）
        if let Some(i) = state.hover {
            if state.entries[i].has_submenu {
                open_submenu_at(state, hwnd, i);
                return;
            }
        }
        return;
    }
    let app = state.app;
    let action = entry.action;
    state.activated = true;
    unsafe {
        let _ = DestroyWindow(hwnd);
        if !app.is_null() {
            (*app).on_menu_activation(action);
        }
    }
}

/// 把 Shell 扩展提供的 HBITMAP 贴到菜单行。
/// 绘制 Shell 菜单项自带的 HBITMAP。
///
/// 直接 BitBlt 会把位图的背景一起贴出来 —— 条目图标边上出现一个方块。
/// 菜单位图的惯例是左下角像素即背景色，以此作为透明色走 TransparentBlt。
unsafe fn draw_bmp(hdc: HDC, hbmp: isize, x: i32, y: i32, size: i32) {
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleDC, DeleteDC, GetPixel, SelectObject, TransparentBlt, HBITMAP,
        SRCCOPY,
    };
    let mem = CreateCompatibleDC(Some(hdc));
    if mem.is_invalid() {
        return;
    }
    let old = SelectObject(mem, HBITMAP(hbmp as *mut core::ffi::c_void).into());
    let key = GetPixel(mem, 0, size.saturating_sub(1));
    let ok = TransparentBlt(hdc, x, y, size, size, mem, 0, 0, size, size, key.0).as_bool();
    if !ok {
        let _ = BitBlt(hdc, x, y, size, size, Some(mem), 0, 0, SRCCOPY);
    }
    SelectObject(mem, old);
    let _ = DeleteDC(mem);
}

unsafe fn paint_menu(hdc: HDC, state: &MenuWindow) {
    let t = &state.theme;
    let bg = CreateSolidBrush(t.surface);
    let mut r = RECT::default();
    unsafe {
        // 客户区高度就是可见高度；内容更长时由滚动偏移决定画哪一段
        r.right = state.width;
        r.bottom = state.view_h;
        let _ = FillRect(hdc, &r, bg);
        DeleteObject(bg.into());

        // 裁剪到可见区，避免超长内容画到窗口外（也会溢出到屏幕上）
        let clip = RECT { left: 0, top: 0, right: state.width, bottom: state.view_h };
        let _ = IntersectClipRect(hdc, clip.left, clip.top, clip.right, clip.bottom);
        SetWindowOrgEx(hdc, 0, state.scroll, None);

        let font = crate::paint::menu_font(state.scale);
        let old_font = SelectObject(hdc, font.into());
        SetBkMode(hdc, TRANSPARENT);

        let mut top = 4;
        for (i, e) in state.entries.iter().enumerate() {
            if e.separator {
                let line = CreateSolidBrush(t.border);
                let lr = RECT { left: (10.0 * state.scale) as i32, top: top + 3, right: state.width - (10.0 * state.scale) as i32, bottom: top + 4 };
                let _ = FillRect(hdc, &lr, line);
                DeleteObject(line.into());
                top += 6;
                continue;
            }
            let rect = RECT { left: 0, top, right: state.width, bottom: top + state.row_h };
            if state.hover == Some(i) && !e.disabled {
                let hb = CreateSolidBrush(t.hover_bg);
                let _ = FillRect(hdc, &rect, hb);
                DeleteObject(hb.into());
                // 左侧再压一条强调竖线：光靠底色在暗色主题下对比仍然偏弱，
                // 加一条 3px 的重色边，鼠标在哪一行一眼就能看出来。
                let ab = CreateSolidBrush(t.accent);
                let ar = RECT { left: 0, top, right: (3.0 * state.scale) as i32, bottom: top + state.row_h };
                let _ = FillRect(hdc, &ar, ab);
                DeleteObject(ab.into());
            }
            SetTextColor(hdc, if e.disabled { t.text_dim } else { t.text });
            let icon_pad = (10.0 * state.scale) as i32;
            if let Some(glyph) = e.glyph {
                let gfont = crate::paint::glyph_font(state.scale);
                let old2 = SelectObject(hdc, gfont.into());
                let mut g: Vec<u16> = glyph.encode_utf16().collect();
                let gr = RECT { left: icon_pad, top, right: icon_pad + (28.0 * state.scale) as i32, bottom: top + state.row_h };
                DrawTextW(hdc, &mut g, &mut gr.clone(), DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
                SelectObject(hdc, old2);
            }
            if let Some(hbmp) = e.bmp {
                draw_bmp(hdc, hbmp, icon_pad, top + (state.row_h - 16) / 2, 16);
            }
            let mut text: Vec<u16> = e.text.encode_utf16().collect();
            let tr = RECT {
                left: icon_pad + (28.0 * state.scale) as i32,
                top,
                right: state.width - (26.0 * state.scale) as i32,
                bottom: top + state.row_h,
            };
            DrawTextW(hdc, &mut text, &mut tr.clone(), DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            if e.has_submenu {
                let mut arrow: Vec<u16> = ">".encode_utf16().collect();
                let ar = RECT { left: state.width - (22.0 * state.scale) as i32, top, right: state.width - 6, bottom: top + state.row_h };
                DrawTextW(hdc, &mut arrow, &mut ar.clone(), DT_RIGHT | DT_VCENTER | DT_SINGLELINE);
            }
            top += state.row_h;
        }

        // 滚动指示：内容未显示完时，在顶/底画一条提示三角（画完要还原原点）
        SetWindowOrgEx(hdc, 0, 0, None);
        let more_up = state.scroll > 0;
        let more_down = state.scroll < max_scroll(state);
        if more_up || more_down {
            let c = CreateSolidBrush(t.text_dim);
            let gfont = crate::paint::glyph_font(state.scale);
            let old2 = SelectObject(hdc, gfont.into());
            SetTextColor(hdc, t.text);
            if more_up {
                let mut a: Vec<u16> = "\u{E70E}".encode_utf16().collect(); // ChevronUp
                let ar = RECT { left: state.width - 22, top: 0, right: state.width - 4, bottom: 14 };
                DrawTextW(hdc, &mut a, &mut ar.clone(), DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            }
            if more_down {
                let mut a: Vec<u16> = "\u{E70D}".encode_utf16().collect(); // ChevronDown
                let ar = RECT { left: state.width - 22, top: state.view_h - 14, right: state.width - 4, bottom: state.view_h };
                DrawTextW(hdc, &mut a, &mut ar.clone(), DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            }
            SelectObject(hdc, old2);
            DeleteObject(c.into());
        }

        SelectObject(hdc, old_font);
    }
}

