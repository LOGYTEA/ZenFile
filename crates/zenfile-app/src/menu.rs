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
    BeginPaint, CombineRgn, CreateCompatibleBitmap, CreateRectRgn, CreateRoundRectRgn,
    CreateSolidBrush, DeleteObject, DrawTextW,
    EndPaint, FillRect, GetDC, GetMonitorInfoW, GetPixel, IntersectClipRect, MonitorFromWindow,
    ReleaseDC, RestoreDC, SaveDC, SelectClipRgn, SelectObject, SetBkMode, SetTextColor,
    SetWindowOrgEx,
    DT_CALCRECT, DT_CENTER, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HBRUSH, HDC,
    MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, RGN_AND, SRCCOPY, TRANSPARENT,
};
use windows::Win32::Graphics::Gdi::{BitBlt, CreateCompatibleDC, DeleteDC};
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{IsWindow, PostMessageW};
use windows::Win32::UI::Shell::{
    CMINVOKECOMMANDINFOEX, CMF_EXPLORE, CMF_EXTENDEDVERBS, CMF_ITEMMENU, IShellFolder,
    IContextMenu, IContextMenu3, IShellExtInit, SHCreateDataObject, SHGetDesktopFolder, GCS_VERB,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, GetMenuItemCount,
    GetMenuItemID, GetMenuItemInfoW, GetWindowLongPtrW, RegisterClassExW, SetWindowLongPtrW,
    ShowWindow,
    TrackPopupMenu, GWLP_USERDATA, HMENU, MENUITEMINFOW,
    MFS_DISABLED, MFT_SEPARATOR, MIIM_BITMAP, MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING,
    MIIM_SUBMENU, MFS_CHECKED, SW_SHOWNOACTIVATE, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_DESTROY,
    WM_ERASEBKGND,
    WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT,
    WM_RBUTTONUP, WNDCLASSEXW, WS_BORDER, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    // 菜单窗口自己要定光标：类光标是 NULL，交给 DefWindowProc 的结果就是"菜单
    // 里沿用打开菜单那一刻的形状"（比如从分隔线上右键 → 菜单里一直是 ↔）。
    IDC_ARROW, LoadCursorW, SetCursor, WM_SETCURSOR,
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
/// 子菜单收到"落点在自己外面"的点击：不自己关链，把点击原样上报父菜单
/// （lparam 打包屏幕坐标，同 WM_MOUSEMOVE 的打包方式），由父菜单裁决——
/// 点在父菜单身上→处理（切换/收起子菜单、执行条目）；都不是→关整条链。
/// 第53轮之前这里直接 activated=true 自爆，把整条链带走了 —— 表现为
/// "点『排序方式』菜单整个消失"，子菜单永远点不开。
const WM_MENU_FORWARD_CLICK: u32 = 0x0404;

/// 悬停到带子菜单的条目后**等一会儿**再展开。wparam = 要展开的条目下标。
///
/// 为什么要这个延迟（用户反馈"右键菜单悬停展开的那些会卡展开"）：
/// 展开一个子菜单 = `CreateWindowExW` + 首次 `ShowWindow`，QPC 埋点实测两者
/// 合计约 2.8ms（`ShowWindow` 的首次合成就占 2.4ms），再加销毁上一个子菜单
/// 约 2.7ms —— 一次悬停切换接近 5.5ms 全卡在一条 WM_MOUSEMOVE 里。
/// 原来**一移到那一行就立刻**展开，于是鼠标扫过一列带子菜单的条目
/// （打开方式 / 排序方式 / 查看 / 新建 …）时，每划一行就完整走一遍
/// 建窗 + 首帧 + 建缓存，串起来就是肉眼可见的一顿。
///
/// 资源管理器本身也是延迟展开（`MenuShowDelay`）。这里取 220ms：
/// 足够滤掉"扫过去"，又不至于让"停在某项上想看内容"变得难等。
/// 展开后仍走第53轮的"单击固定"语义（再点一次该条目 / 点空白才收起）。
const WM_MENU_EXPAND_DELAYED: u32 = 0x0405;

/// 悬停展开的延迟时长（毫秒）。
const HOVER_EXPAND_DELAY_MS: u32 = 220;
/// 延迟展开用的单次计时器 id。
const TIMER_EXPAND: usize = 0xE7;

/// 宿主侧自有的命令（自绘菜单的顶部图标行与常规项）。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Command {
    Open,
    /// 「打开方式」：弹出自绘的程序选择窗口
    OpenWith,
    Copy,
    Cut,
    Paste,
    Rename,
    /// 批量重命名。载荷 = 模式序号（对应 `rename::MODE_LABELS`）
    BulkRename(usize),
    Delete,
    DeletePermanent,
    NewFolder,
    NewFile,
    /// 按系统 ShellNew 模板新建。载荷 = 含点的扩展名（".txt" / ".docx" …）
    NewTemplate(String),
    SelectAll,
    Refresh,
    ViewModeCycle,
    ShowNativeMenu,
    Properties,
    // —— 「固定到…」三项 ——
    //
    // Windows **没有公开的 pin API**。微软的立场是"固定到任务栏/开始"属于用户的
    // 决定，程序不该以编程方式去做（Explorer 能做是因为它自己就是 Shell）。所以
    // 这里只剩 Shell 上下文菜单里的 verb 这一条路，而且各版本 Windows 提供的
    // verb 不一样 —— 必须**运行时探测**，不能写死。
    PinTaskbar,
    PinStartMenu,
    PinStartScreen,
    // —— 标题栏「更多」菜单专用 ——
    ViewIcons,
    ViewList,
    ViewDetails,
    SortByName,
    SortByModified,
    SortByType,
    SortBySize,
    /// 只把方向设成升序，不动当前排序列
    SortAscending,
    /// 只把方向设成降序，不动当前排序列
    SortDescending,
    /// 表头右键：按语义列号排序并给定方向（0=名称 1=修改日期 2=类型 3=大小）
    HeaderSort(u8, bool),
    /// 「选择列…」：弹出子菜单，勾掉/勾上某一列（载荷 = 列 id）
    ToggleColumn(String),
    /// 「其他(M)...」：打开「选择详细信息」对话框（勾选 + 上移/下移，第53轮）
    ChooseColumns,
    /// 「将列调整为合适的大小」：按当前内容自动定宽（当前目录生效）
    AutofitColumns,
    /// 「所有列均调整为合适的大小」
    AutofitAllColumns,
    ToggleSidebar,
    ToggleStatusbar,
    /// 「使用 GPU 渲染」：切换 GPU(Direct2D) / CPU 多线程 GDI 两条渲染路径
    ToggleGpuRender,
    /// 导航窗格"跟随展开到当前文件夹"
    ToggleFollowCurrent,
    /// "此电脑"节点是否默认展开到盘符
    ToggleThispcExpanded,
    /// 显示/隐藏"隐藏的项目"（FILE_ATTRIBUTE_HIDDEN）
    ToggleHidden,
    /// 显示/隐藏"受保护的操作系统文件"（FILE_ATTRIBUTE_SYSTEM）
    ToggleSystem,
    /// 显示/隐藏文件扩展名
    ToggleExtensions,
    /// 循环切换"大小"列的默认单位
    CycleSizeUnit,
    /// 设置页下拉：直接选中第 n 档大小单位（不再循环）
    SetSizeUnitIdx(usize),
    /// 设置页下拉：true = 暗色
    SetThemeDark(bool),
    /// 设置页下拉：直接选中第 n 个默认视图（DEFAULT_VIEW_CYCLE 的下标）
    SetDefaultViewIdx(usize),
    /// 表头筛选：按列号 + 桶序号过滤。桶 0 = 全部（等价于清除筛选）
    ColFilter(usize, usize),
    /// 表头筛选：按具体类型名过滤（"类型"列）
    ColFilterKind(String),
    /// 循环切换明暗主题
    CycleTheme,
    /// 循环切换新标签的默认视图模式
    CycleDefaultView,
    /// 以标签页形式打开设置
    OpenSettings,
    /// 打开回收站视图
    OpenRecycle,
    RecycleRestore,
    RecycleDelete,
    RecycleEmpty,
    RecycleSelectAll,
    NewTab,
    About,
    // —— 第 19 轮：多窗口 ——
    /// 在本进程里再开一个主窗口（托盘菜单 / Ctrl+N）
    NewWindow,
    /// 把第 n 个窗口提到前台（托盘菜单里的窗口清单）
    FocusWindow(usize),
    /// 关掉所有窗口并退出
    QuitApp,
}

/// 去掉菜单文本里的 `&` 加速键标记。
///
/// Shell 与系统菜单的字符串惯例是用 `&` 标出助记键（"打开(&O)"、"c&ursor"）。
/// 自绘菜单用 `DT_NOPREFIX` 画字，`&` 会被原样画出来 —— 于是界面上出现
/// 莫名其妙的 `&`，文件夹名也变成"c&rsor"。这里统一按 Win32 的助记键规则
/// 剥掉：单个 `&` 去掉，`&&` 还原成一个 `&`（文件名里真的有 & 的情况）。
pub fn strip_accel(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '&' {
            match chars.next() {
                // `&&` 是转义后的一个真 `&`（文件名里真的有 & 的情况）
                Some('&') => out.push('&'),
                // 只吃掉 `&` 本身，助记字符**要留着**：Windows 把 "打开(&O)"
                // 渲染成 "打开(O)"（O 带下划线），不是 "打开()"。早先这里连 O
                // 一起吞了，于是菜单里每一项都拖着一对空括号 —— "打开()"、
                // "剪切()"、"复制到()"，就是"括号里是空的"那条反馈。
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 菜单文本的**比较键**：剥掉助记键标记，再剥掉它带来的一对括号。
///
/// Shell 的字符串是 "打开(&O)"，我们自己的条目是 "打开"。只剥 `&` 的话前者是
/// "打开(O)"、后者是 "打开"，两者照样不相等 —— Shell 那个「打开」就会混进来，
/// 菜单上一屏出现两个「打开」（用户报过的重复项）。去重必须比这个键。
pub fn menu_key(text: &str) -> String {
    let s = strip_accel(text);
    let t = s.trim_end();
    for (open, close) in [('(', ')'), ('（', '）')] {
        if !t.ends_with(close) {
            continue;
        }
        if let Some(i) = t.rfind(open) {
            let inner = &t[i + open.len_utf8()..t.len() - close.len_utf8()];
            if inner.chars().count() == 1 {
                return t[..i].trim_end().to_lowercase();
            }
        }
    }
    t.to_lowercase()
}

#[derive(Clone, Debug)]
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
    /// "这一项就是当前生效的那个"（查看 → 详细信息、排序方式 → 名称、
    /// 表头菜单里已显示的列 …）。
    ///
    /// 渲染成行首一条**橙色**竖线：蓝色竖线已经用来表示"鼠标正悬停在这行"，
    /// 两者撞色就分不清"我指在这儿"和"现在用的是这个"。
    pub selected: bool,
    pub has_submenu: bool,
    pub submenu: Vec<MenuEntry>,
    pub action: MenuAction,
}

impl MenuEntry {
    /// 一条普通项（其余字段走默认值）。加字段时不必回头改每个构造点 ——
    /// 这个结构有 6 处分散的构造，少补一个就是一次编译错误。
    pub fn item(text: impl Into<String>, cmd: Command) -> Self {
        MenuEntry {
            text: text.into(),
            glyph: None,
            bmp: None,
            separator: false,
            disabled: false,
            selected: false,
            has_submenu: false,
            submenu: Vec::new(),
            action: MenuAction::Own(cmd),
        }
    }

    pub fn with_disabled(mut self, on: bool) -> Self {
        self.disabled = on;
        self
    }

    pub fn with_glyph(mut self, g: &'static str) -> Self {
        self.glyph = Some(g);
        self
    }

    pub fn with_selected(mut self, on: bool) -> Self {
        self.selected = on;
        self
    }

    pub fn sep() -> Self {
        MenuEntry {
            text: String::new(),
            glyph: None,
            bmp: None,
            separator: true,
            disabled: false,
            selected: false,
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

/// 为某个**目录的空白处**构建背景上下文菜单。
///
/// 资源管理器在空白处右键出来的是"对这个文件夹做什么"：BandZip 的压缩项、
/// "在此处打开 PowerShell/终端"、"Git Bash Here"、同步网盘的项……全在这一层。
/// 之前这里直接 `None`（"没有选中文件就没得问 Shell"），于是空白处右键
/// 只有我们自己那几行，第三方扩展一个都看不到。
///
/// 关键差别是菜单来源：**背景菜单**走 `IShellFolder::CreateViewObject`，
/// 而不是对 0 个对象调 `GetUIObjectOf`（那样拿到的是文件夹"作为条目"的菜单，
/// 内容是"打开/剪切/复制"，正是我们本来就有、要被去重掉的那些）。
pub fn build_background_bridge(hwnd: HWND, dir: &Path) -> Option<ShellMenuBridge> {
    unsafe {
        let desktop: IShellFolder = SHGetDesktopFolder().ok()?;
        let mut attrs = 0u32;
        let mut dir_pidl: *mut windows::Win32::UI::Shell::Common::ITEMIDLIST = std::ptr::null_mut();
        desktop
            .ParseDisplayName(
                hwnd,
                None,
                &HSTRING::from(dir.to_string_lossy().as_ref()),
                None,
                &mut dir_pidl,
                &mut attrs,
            )
            .ok()?;
        if dir_pidl.is_null() {
            return None;
        }
        let folder: IShellFolder = desktop.BindToObject(dir_pidl, None).ok()?;
        let ctx: IContextMenu = folder.CreateViewObject(hwnd).ok()?;
        let hmenu = CreatePopupMenu().ok()?;
        // 背景菜单**不能**带 CMF_ITEMMENU：那是"选了东西"的语义，
        // 带上去很多扩展会把自己收起来（"在这里解压"之类的就不出来了）。
        if ctx
            .QueryContextMenu(hmenu, 0, CMD_FIRST, CMD_LAST, CMF_EXPLORE)
            .is_err()
        {
            let _ = DestroyMenu(hmenu);
            return None;
        }
        let bridge = ShellMenuBridge {
            menu3: ctx.cast::<IContextMenu3>().ok(),
            menu: Some(ctx),
            hmenu: Some(hmenu),
            first_id: CMD_FIRST,
        };
        CoTaskMemFree(Some(dir_pidl as *const core::ffi::c_void));
        Some(bridge)
    }
}

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
        let text = strip_accel(&crate::icons::from_wide(&buf));
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
            selected: (mii.fState & MFS_CHECKED) == MFS_CHECKED,
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

// -------------------------------------------------------------- 「固定到…」
//
// 三个 verb 名**全是实测来的**（Win10 IoT LTSC 21H2 / build 19044，用
// `IContextMenu::GetCommandString(GCS_VERB)` 逐条枚举 notepad.exe 与文件夹的菜单）：
//
//   · `PinToStartScreen` —— 真的存在，exe 和文件夹都有，由 appresolver.dll 提供
//   · `taskbarpin`       —— 菜单里**没有**。任务栏固定那个扩展（CLSID
//                           {90AA3A4E-1CBA-4233-B8BB-535773D48449}）确实在
//                           shell32.dll 里、也有 InProcServer32，但它**没有注册成
//                           ContextMenuHandler**，所以不会出现在菜单里。加
//                           CMF_EXTENDEDVERBS 也查不到。要它只能自己 CoCreate +
//                           IShellExtInit 把上下文喂进去。
//   · `startpin`         —— 菜单里没有
//
// 所以这三项一律**运行时探测**，探测不到就把菜单项置成禁用态 —— 宁可显示"用不了"，
// 也不能点了静悄悄什么都不发生（资源管理器里不可用的动作就是淡的、点不动）。
const VERB_PIN_START_SCREEN: &str = "PinToStartScreen";
const VERB_PIN_TASKBAR: &str = "taskbarpin";
const VERB_PIN_START: &str = "startpin";

/// 任务栏固定的 Shell 扩展。见上面注释：它没挂进 ContextMenuHandler，得手动 CoCreate。
const CLSID_TASKBAR_PIN: windows::core::GUID = windows::core::GUID {
    data1: 0x90AA3A4E,
    data2: 0x1CBA,
    data3: 0x4233,
    data4: [0xB8, 0xBB, 0x53, 0x57, 0x73, 0xD4, 0x84, 0x49],
};

/// 在 Shell 菜单里找某个 verb 对应的**命令偏移**（InvokeCommand 用 MAKEINTRESOURCE
/// 形式，所以要的是相对 idCmdFirst 的偏移量，不是菜单 id）。
///
/// `GetCommandString` 的 `idcmd` 必须传偏移量 —— 传菜单 id 会直接返回
/// "参数错误"(0x80070057)。这是实测踩到的，不是从文档推出来的。
fn verb_offset(ctx: &IContextMenu, hmenu: HMENU, first_id: u32, verb: &str) -> Option<u32> {
    unsafe {
        let count = GetMenuItemCount(Some(hmenu));
        let mut buf = [0u16; 128];
        for i in 0..count {
            let id = GetMenuItemID(hmenu, i as i32);
            if id < first_id || id == u32::MAX {
                continue;
            }
            buf[0] = 0;
            // GCS_VERB = GCS_VERBW，缓冲区按 UTF-16 传
            if ctx
                .GetCommandString(
                    (id - first_id) as usize,
                    GCS_VERB,
                    None,
                    windows::core::PSTR(buf.as_mut_ptr().cast::<u8>()),
                    buf.len() as u32,
                )
                .is_ok()
            {
                let s = String::from_utf16_lossy(&buf);
                if s.trim_end_matches('\0').eq_ignore_ascii_case(verb) {
                    return Some(id - first_id);
                }
            }
        }
    }
    None
}

/// 这三个「固定」在当前系统 + 当前条目上分别能不能用。
///
/// 一次建桥探三个 verb，避免为每一项各建一次 Shell 菜单（每次都要 ParseDisplayName
/// + GetUIObjectOf + QueryContextMenu，右键一次建 4 次太浪费）。
pub fn pin_verbs_available(hwnd: HWND, parent: &Path, files: &[PathBuf]) -> [bool; 3] {
    let mut out = [false; 3];
    let Some(bridge) = build_shell_bridge(hwnd, parent, files, true) else {
        return out;
    };
    let (Some(ctx), Some(hmenu)) = (&bridge.menu, bridge.hmenu) else {
        return out;
    };
    let pin_screen = verb_offset(ctx, hmenu, bridge.first_id, VERB_PIN_START_SCREEN).is_some();
    // 「固定到开始菜单」：Win10/11 里"开始菜单"就是"开始"的磁贴区，和开始屏幕
    // 是同一个目标。系统若单独提供 `startpin` 就用它，否则退回 `PinToStartScreen`。
    let pin_start = verb_offset(ctx, hmenu, bridge.first_id, VERB_PIN_START).is_some() || pin_screen;
    out[1] = pin_start;
    out[2] = pin_screen;
    out[0] = verb_offset(ctx, hmenu, bridge.first_id, VERB_PIN_TASKBAR).is_some()
        || taskbar_pin_available(hwnd, parent, files);
    out
}

/// 执行某个「固定」动作。返回 false 表示当前系统做不到。
pub fn pin_invoke(hwnd: HWND, parent: &Path, files: &[PathBuf], which: PinTarget) -> bool {
    // 先走菜单里现成的 verb
    if let Some(bridge) = build_shell_bridge(hwnd, parent, files, true) {
        let (ctx, hmenu, first) = (&bridge.menu, bridge.hmenu, bridge.first_id);
        if let (Some(ctx), Some(hmenu)) = (ctx, hmenu) {
            let verb = match which {
                PinTarget::Taskbar => VERB_PIN_TASKBAR,
                PinTarget::StartMenu => VERB_PIN_START,
                PinTarget::StartScreen => VERB_PIN_START_SCREEN,
            };
            if let Some(off) = verb_offset(ctx, hmenu, first, verb) {
                return invoke(&bridge, hwnd, off);
            }
            // 开始菜单：系统没单独提供 `startpin` 时，退回和开始屏幕同一个 verb
            if which == PinTarget::StartMenu {
                if let Some(off) = verb_offset(ctx, hmenu, first, VERB_PIN_START_SCREEN) {
                    return invoke(&bridge, hwnd, off);
                }
            }
        }
    }
    if which == PinTarget::Taskbar {
        return taskbar_pin_invoke(hwnd, parent, files);
    }
    false
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PinTarget {
    Taskbar,
    StartMenu,
    StartScreen,
}

/// 手动 CoCreate 任务栏固定扩展，拿它的菜单查 `taskbarpin`。
///
/// 它实现 `IContextMenu` + `IShellExtInit`：必须先 `Initialize` 把"在哪个目录、
/// 选中了什么"喂进去，`QueryContextMenu` 才会吐出 `taskbarpin`。
fn taskbar_pin_context(
    hwnd: HWND,
    parent: &Path,
    files: &[PathBuf],
) -> Option<(IContextMenu, HMENU)> {
    unsafe {
        let desktop: IShellFolder = SHGetDesktopFolder().ok()?;
        let mut attrs = 0u32;
        let mut parent_pidl: *mut windows::Win32::UI::Shell::Common::ITEMIDLIST =
            std::ptr::null_mut();
        desktop
            .ParseDisplayName(
                hwnd,
                None,
                &HSTRING::from(parent.to_string_lossy().as_ref()),
                None,
                &mut parent_pidl,
                &mut attrs,
            )
            .ok()?;
        if parent_pidl.is_null() {
            return None;
        }
        let folder: IShellFolder = desktop.BindToObject(parent_pidl, None).ok()?;
        let mut pidls: Vec<*mut windows::Win32::UI::Shell::Common::ITEMIDLIST> = Vec::new();
        for f in files {
            let name = f.file_name()?.to_string_lossy().into_owned();
            let eaten = 0u32;
            let mut a2 = 0u32;
            let mut pidl: *mut windows::Win32::UI::Shell::Common::ITEMIDLIST =
                std::ptr::null_mut();
            if folder
                .ParseDisplayName(
                    hwnd,
                    None,
                    &HSTRING::from(name.as_str()),
                    Some(&eaten),
                    &mut pidl,
                    &mut a2,
                )
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
        // IShellExtInit 要的"选中了什么"是一个 IDataObject
        let data: windows::Win32::System::Com::IDataObject =
            SHCreateDataObject(Some(parent_pidl as *const _), Some(&slice), None).ok()?;

        let ctx: IContextMenu = windows::Win32::System::Com::CoCreateInstance(
            &CLSID_TASKBAR_PIN,
            None,
            windows::Win32::System::Com::CLSCTX_INPROC_SERVER,
        )
        .ok()?;
        let init: IShellExtInit = ctx.cast::<IShellExtInit>().ok()?;
        init.Initialize(Some(parent_pidl as *const _), &data, None).ok()?;

        let hmenu = CreatePopupMenu().ok()?;
        if ctx
            .QueryContextMenu(hmenu, 0, CMD_FIRST, CMD_LAST, CMF_EXPLORE)
            .is_err()
        {
            let _ = DestroyMenu(hmenu);
            for p in pidls {
                CoTaskMemFree(Some(p as *const core::ffi::c_void));
            }
            CoTaskMemFree(Some(parent_pidl as *const core::ffi::c_void));
            return None;
        }
        for p in pidls {
            CoTaskMemFree(Some(p as *const core::ffi::c_void));
        }
        CoTaskMemFree(Some(parent_pidl as *const core::ffi::c_void));
        Some((ctx, hmenu))
    }
}

/// 「固定到任务栏」在当前系统能不能用（只探不执行）。
fn taskbar_pin_available(hwnd: HWND, parent: &Path, files: &[PathBuf]) -> bool {
    match taskbar_pin_context(hwnd, parent, files) {
        Some((ctx, hmenu)) => {
            let ok = verb_offset(&ctx, hmenu, CMD_FIRST, VERB_PIN_TASKBAR).is_some();
            let _ = unsafe { DestroyMenu(hmenu) };
            ok
        }
        None => false,
    }
}

/// 「固定到任务栏」：走 CoCreate 出来的扩展菜单。
fn taskbar_pin_invoke(hwnd: HWND, parent: &Path, files: &[PathBuf]) -> bool {
    let Some((ctx, hmenu)) = taskbar_pin_context(hwnd, parent, files) else {
        return false;
    };
    let Some(off) = verb_offset(&ctx, hmenu, CMD_FIRST, VERB_PIN_TASKBAR) else {
        let _ = unsafe { DestroyMenu(hmenu) };
        return false;
    };
    unsafe {
        let info = CMINVOKECOMMANDINFOEX {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: 0x00004000, // CMIC_MASK_UNICODE
            hwnd,
            lpVerbW: PCWSTR(off as usize as *const u16),
            lpParametersW: PCWSTR::null(),
            lpDirectoryW: PCWSTR::null(),
            nShow: 5,
            ..Default::default()
        };
        let r = ctx
            .InvokeCommand(
                &info as *const _ as *const windows::Win32::UI::Shell::CMINVOKECOMMANDINFO,
            )
            .is_ok();
        let _ = DestroyMenu(hmenu);
        r
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
    /// 绘制原点（设备坐标）。真菜单是 (0,0)；设置页那块实时预览是它那块
    /// 面板的左上角 —— 没有这个偏移的话，菜单会被自己的裁剪框（0,0,w,h）
    /// 整块裁掉，预览区一片空白。
    ox: i32,
    oy: i32,
    parent_hwnd: HWND,
    submenu: Option<HWND>,
    open_submenu_for: Option<usize>,
    /// 这是作为**另一级菜单的子菜单**弹出的；顶层菜单为 None。
    parent_menu: Option<HWND>,
    /// 本窗口是被父菜单的第几项展开的（顶层菜单为 None）。
    opened_by: Option<usize>,
    /// 本窗口弹出那一刻光标的**屏幕坐标**。
    ///
    /// 菜单一 `SetCapture`，系统会按"光标当前所在位置"补发一条 WM_MOUSEMOVE。
    /// 那条消息不是用户操作 —— 光标其实还停在原处没动过（通常在父菜单那一行上，
    /// 或干脆在屏幕别处）。照它处理会有两个后果：
    ///   1. 子菜单认为"用户把鼠标移回父菜单了" → 刚弹出就把自己关掉，
    ///      于是"悬停展开"时灵时不灵（灵不灵取决于物理光标当时在哪）；
    ///   2. 顶层菜单会把完全不相关的某一行点亮。
    /// 因此：位置与弹出时相同、且不在本菜单内的那条消息一律丢弃。
    open_pos: Option<(i32, i32)>,
    /// 本窗口里已经有条目被真正执行（用于决定销毁时是否级联关闭父菜单）。
    activated: bool,
    /// 弹出本窗口的那一次点击是**左键按下**（按钮/下拉那种）。
    ///
    /// 这类菜单是在 `WM_LBUTTONDOWN` 里弹的，随后那一下 `WM_LBUTTONUP` 会被
    /// 本窗口的 capture 接住；照常处理就等于"刚弹出就被点了一下" —— 菜单立刻
    /// 关掉，于是必须**按住不放**才能看见它（"批量重命名按钮要按住才展开"）。
    /// 这里把那一次抬起吞掉（只吞一次，之后的抬起照常算点击）。
    swallow_up: bool,
    /// 整块菜单的离屏位图缓存（见 `MenuBitmap`）。设置页预览不用它（None）。
    cache: Option<MenuBitmap>,
    /// 已经为第几项**排了**延迟展开（`HOVER_EXPAND_DELAY_MS` 之后才真的建窗）。
    ///
    /// 不是"要不要展开"而是"已经排上了没有"：鼠标在同一行上连续抖动时会
    /// 重排计时器，真正展开的时机也就跟着往后推 —— 这正是想要的（抖动不展开）。
    pending_expand: Option<usize>,
    /// 本次 WM_MOUSEMOVE 里悬停行**变了**（见 WM_MOUSEMOVE 里那处赋值）。
    hover_moved: bool,
}

/// 撤掉还没到期的延迟展开。
///
/// 必须**连着杀计时器**：只清 `pending_expand` 而留着计时器，到期时 `wparam`
/// 里那个下标仍会被展开 —— 鼠标早就划走了，菜单却自己弹出来。
fn cancel_pending_expand(state: &mut MenuWindow, hwnd: HWND) {
    if state.pending_expand.take().is_some() {
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::KillTimer(Some(hwnd), TIMER_EXPAND);
        }
    }
}

/// 菜单窗口的离屏位图缓存。
///
/// 悬停在条目之间移动时，**唯一变的是「哪一行高亮」**。而整块菜单重画一次
/// 实测 1.1ms（24 行，每行一次 `DrawTextW` + 一次 `TransparentBlt`）。展开一个
/// 子菜单的路径上会**连着画好几次**：父菜单换高亮 → 子菜单首帧 → 收起旧子菜单
/// 后父菜单再换一次高亮。几次叠起来接近 5ms，鼠标要顿一下子菜单才出现 ——
/// 这就是"悬停展开会卡"。
///
/// 所以整块菜单**只在首次**（以及尺寸/滚动变了时）画进缓存；之后换高亮只
/// 重画脏掉的那一两行，最后把缓存整块 `BitBlt` 贴上去（贴一次 ~120us，
/// 比重画便宜一个数量级）。
///
/// 缓存高度取 `view_h`（可见高度）而不是内容总高：第三方 Shell 扩展能把菜单
/// 堆到几百行，按内容高度分配位图就是十几 MB，纯浪费。代价是滚动时整块重画
/// —— 滚动是用户主动动作，一次 1.1ms 无感。
struct MenuBitmap {
    dc: HDC,
    bmp: windows::Win32::Graphics::Gdi::HBITMAP,
    /// `SelectObject` 换上去之前的那个位图，销毁时必须选回去再删
    old_bmp: windows::Win32::Graphics::Gdi::HGDIOBJ,
    w: i32,
    h: i32,
    /// 缓存里当前画着的高亮行。与 `state.hover` 不同才需要补画那两行。
    hover: Option<usize>,
    /// 缓存对应的滚动偏移。变了必须整块重画（缓存只覆盖可见区）。
    scroll: i32,
}

impl MenuBitmap {
    unsafe fn free(&mut self) {
        if !self.dc.is_invalid() {
            let _ = SelectObject(self.dc, self.old_bmp);
            let _ = DeleteDC(self.dc);
        }
        if !self.bmp.is_invalid() {
            let _ = DeleteObject(self.bmp.into());
        }
        *self = MenuBitmap {
            dc: HDC(std::ptr::null_mut()),
            bmp: windows::Win32::Graphics::Gdi::HBITMAP(std::ptr::null_mut()),
            old_bmp: windows::Win32::Graphics::Gdi::HGDIOBJ(std::ptr::null_mut()),
            w: 0,
            h: 0,
            hover: None,
            scroll: 0,
        };
    }
}

/// 缓存缺失/尺寸对不上时重建一块。
///
/// 尺寸对不上的两种来源：窗口尺寸被改（缩放/DPI 变化），或缓存当初建失败了。
/// 两种都必须整块重画 —— 所以把 `hover`/`scroll` 一起清成"需要全画"的状态。
/// `scroll = -1` 表示"缓存里画的不是当前内容"，`paint_menu_cached` 会走全量路径。
unsafe fn ensure_cache(state: &mut MenuWindow, hwnd: HWND) {
    let w = state.width.max(1);
    let h = state.view_h.max(1);
    let need = match &state.cache {
        Some(c) => c.w != w || c.h != h,
        None => true,
    };
    if !need {
        return;
    }
    if let Some(mut old) = state.cache.take() {
        old.free();
    }
    // 参考 DC 用窗口自己的：位图的设备格式要和贴上去的窗口一致
    //（颜色深度/像素格式跟着 DC 走，用主屏 DC 建的位图贴到别的显示器会偏色）。
    let src = windows::Win32::Graphics::Gdi::GetDC(Some(hwnd));
    if src.is_invalid() {
        return;
    }
    let dc = CreateCompatibleDC(Some(src));
    if dc.is_invalid() {
        let _ = windows::Win32::Graphics::Gdi::ReleaseDC(Some(hwnd), src);
        return;
    }
    let bmp = CreateCompatibleBitmap(src, w, h);
    let _ = windows::Win32::Graphics::Gdi::ReleaseDC(Some(hwnd), src);
    if bmp.is_invalid() {
        let _ = DeleteDC(dc);
        return;
    }
    let old = SelectObject(dc, bmp.into());
    state.cache = Some(MenuBitmap { dc, bmp, old_bmp: old, w, h, hover: None, scroll: -1 });
}

pub fn register_menu_class() {
    let cls = windows::core::HSTRING::from("ZenFileMenu");
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
    // 本菜单是否由一次**左键按下**弹出（按钮/下拉）。见 `MenuWindow::swallow_up`。
    from_left_down: bool,
) -> Option<HWND> {
    let cls = HSTRING::from("ZenFileMenu");
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
        ox: 0,
        oy: 0,
        parent_hwnd,
        submenu: None,
        open_submenu_for: None,
        parent_menu,
        opened_by: None,
        open_pos: None,
        activated: false,
        swallow_up: from_left_down,
        cache: None,
        pending_expand: None,
        hover_moved: false,
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
        // SetCapture 之后系统会按光标当前位置补发一条 WM_MOUSEMOVE —— 记下此刻位置，
        // 好把那条"不是用户操作"的消息认出来（见 MenuWindow::open_pos 的说明）。
        let mut cur = POINT::default();
        if windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut cur).is_ok() {
            (*state).open_pos = Some((cur.x, cur.y));
        }
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

/// 父菜单在给定**屏幕坐标**下命中的条目下标。
///
/// 子菜单要靠它判断"光标是不是移到父菜单的另一项上了"。跨窗口读状态只能走
/// GWLP_USERDATA 拿裸指针，父子同线程，不存在竞争。
unsafe fn parent_entry_at(parent: HWND, screen: POINT) -> Option<usize> {
    let ptr = GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut MenuWindow;
    if ptr.is_null() {
        return None;
    }
    let ps = &*ptr;
    let mut pt = screen;
    let _ = ScreenToClient(parent, &mut pt);
    if pt.x < 0 || pt.x > ps.width || pt.y < 0 || pt.y > ps.view_h {
        return None;
    }
    entry_at(ps, pt.y)
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
        // 悬停停留够了 → 现在才真的建子菜单。
        // 判定必须用**当下**的 hover，而不是计时器 wparam 里那个下标：
        // 计时器排队期间鼠标可能已经移走（虽然 WM_MOUSEMOVE 会 KillTimer，
        // 但"排队 → 移走 → 又移回同一行"这种序列下 wparam 仍可能过期）。
        WM_TIMER if wparam.0 == TIMER_EXPAND => {
            let _ = windows::Win32::UI::WindowsAndMessaging::KillTimer(Some(hwnd), TIMER_EXPAND);
            let Some(i) = state.pending_expand.take() else {
                return LRESULT(0);
            };
            if state.hover != Some(i) || state.open_submenu_for == Some(i) {
                return LRESULT(0);
            }
            // 停在这一项上这么久但**中途进过别的行**：那就重新按当前行排队，
            // 不要在这里补展开（否则"划过去"最后停在某项上时会展开错的那一项）。
            if !state.entries.get(i).map(|e| e.has_submenu && !e.submenu.is_empty()).unwrap_or(false) {
                return LRESULT(0);
            }
            open_submenu_at(state, hwnd, i);
            LRESULT(0)
        }        // 菜单窗口的类光标是 NULL，走 DefWindowProc 的结果就是"光标维持打开菜单
        // 那一刻的形状"（从分隔线上右键，整个菜单里都是 ↔）。一律自己给箭头。
        WM_SETCURSOR => {
            if let Ok(c) = LoadCursorW(None, IDC_ARROW) {
                SetCursor(Some(c));
            }
            return LRESULT(1);
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            // 走离屏位图缓存：整块菜单只在首次/尺寸变/滚动变时重画，
            // 换高亮只补那一两行。展开子菜单这条路径上要连着换好几次高亮，
            // 原来每次都整块重画（实测 ~1.1ms/次），叠起来接近 5ms —— 肉眼就是
            // "鼠标悬停过去，子菜单顿一下才出来"。
            paint_menu_window(hdc, state, hwnd);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let y = (lparam.0 >> 16) as i16 as i32;
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            // 丢弃"弹出瞬间由系统补发"的那条鼠标移动（位置与弹出时完全相同，且不在本菜单内）
            if let Some((ox, oy)) = state.open_pos.take() {
                let mut sp = POINT { x, y };
                let _ = ClientToScreen(hwnd, &mut sp);
                if sp.x == ox && sp.y == oy && !inside_popup(state, x, y) {
                    return LRESULT(0);
                }
            }
            // 子菜单弹出后由**子菜单**持有 capture，本窗口就再也收不到 WM_MOUSEMOVE。
            // 于是鼠标移回父菜单时高亮会一直卡在展开子菜单的那一项上。这里让子菜单在
            // 光标越过自己左边界时把 capture 交还回来，并附带上屏幕坐标重算高亮。
            // 是否该把鼠标交还给父菜单：光标离开本菜单，且落回父菜单里**另一项**上。
            //
            // 这里曾经写成"光标离开本菜单 且 (x<0 或 落在父菜单矩形内)"。问题是子菜单
            // 刚弹出时光标还停在父菜单那一行上，x 必然 < 0 —— 子菜单收到的第一个
            // WM_MOUSEMOVE 就立刻把自己关掉并交还父菜单，于是**悬停永远展开不出来**
            // （脚本里先把光标挪进子菜单再验证，所以测不出来）。
            // 现在必须"落在父菜单的另一个条目上"才交还：停在自己那一行时子菜单保持打开。
            let back_to_parent = state.parent_menu.is_some()
                && !inside_popup(state, x, y)
                && {
                    let p = state.parent_menu.unwrap();
                    let mut pr = RECT::default();
                    let mut sp = POINT { x, y };
                    let _ = ClientToScreen(hwnd, &mut sp);
                    let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(p, &mut pr);
                    let in_parent = sp.x >= pr.left - 2
                        && sp.x <= pr.right + 2
                        && sp.y >= pr.top
                        && sp.y <= pr.bottom;
                    in_parent && match parent_entry_at(p, sp) {
                        Some(i) => Some(i) != state.opened_by,
                        None => false,
                    }
                };
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
                state.hover = idx;
                // `hover_moved`：这一次 WM_MOUSEMOVE 是"移到了新的一行"，而不是
                // "在原来那一行里轻微抖动"。只有前者才配触发延迟展开 —— 否则鼠标
                // 在带子菜单的那行上哪怕只是颤 1px，计时器就被反复推后永远展不开。
                state.hover_moved = true;
                let _ = InvalidateRect(Some(hwnd), None, false);
            } else {
                state.hover_moved = false;
            }
            // 第54轮：悬停到带子菜单的项 → 照资源管理器**自动展开**（用户：
            // "右键那些悬停展示怎么没了"）。第53轮把悬停展开整段删掉后，
            // 鼠标划过「打开方式/排序方式/查看」只剩一根高亮条，看不到里面有什么。
            //
            // **必须先判 `open_submenu_for != Some(i)`**：第53轮给 open_submenu_at
            // 加了"再点一次 = 收起"的开关语义，悬停时重复调用会把刚展开的子菜单
            // 立刻又收回去（现象就是怎么悬停都弹不出来）。
            //
            // 这里只管"展开"，**不负责因为鼠标移开而收起** —— 收起仍走第53轮的
            // "单击固定"语义（再点一次该条目 / 点空白），所以移开时子菜单保持钉住。
            //
            // **延迟展开**（见 `WM_MENU_EXPAND_DELAYED` 的说明）：这里只**排**计时器，
            // 真正建窗放到计时器到期后。原来是同一处直接建窗，而建窗+首帧+建缓存
            // 一次要 2.8ms，扫过一列带子菜单的条目就是连续几次这种顿挫。
            cancel_pending_expand(state, hwnd);
            if let Some(i) = idx {
                let has_sub = {
                    let e = &state.entries[i];
                    e.has_submenu && !e.submenu.is_empty()
                };
                // 只有**从别处移进来**才排计时器：已经展开的这一项不再排，
                // 否则鼠标在这行里轻微移动会不断把计时器推后，永远展不开。
                if has_sub && state.open_submenu_for != Some(i) && state.hover_moved {
                    state.pending_expand = Some(i);
                    unsafe {
                        let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(
                            Some(hwnd),
                            TIMER_EXPAND,
                            HOVER_EXPAND_DELAY_MS,
                            None,
                        );
                    }
                }
            }
            // 第53轮「单击固定展开」：光标本窗口不当把移开当成"要收起"——
            // 收起只由"再点一次该条目 / 点空白"触发，子菜单一直钉着。
            // 鼠标从父菜单直接移进已展开的子菜单时，把 capture 交回去让它接管。
            if idx.is_none() {
                if let Some(sub) = state.submenu {
                    if IsWindow(Some(sub)).as_bool() {
                        let mut sr = RECT::default();
                        let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(sub, &mut sr);
                        let mut sp = POINT { x, y };
                        let _ = ClientToScreen(hwnd, &mut sp);
                        if sp.x >= sr.left && sp.x <= sr.right && sp.y >= sr.top && sp.y <= sr.bottom {
                            let _ = SetCapture(sub);
                            // **把这一次移动补给它**（第54轮）：交还捕获的那条
                            // WM_MOUSEMOVE 是被**本窗口**消费掉的（这时候捕获还在自己
                            // 手里），子菜单永远看不到它；而放鼠标后到下一次移动之间
                            // 没有任何消息，于是"鼠标明明已经进到子菜单里了，里面却
                            // 一行都没亮"。补一条同坐标的 WM_MOUSEMOVE 让它立刻重算。
                            // 真实操作时鼠标是连续移动的，下一帧自然就会亮 —— 但鼠标
                            // 快速甩过去时（尤其配 hover 自动展开）这一帧的落后很明显。
                            // 注意 WM_MOUSEMOVE 的 lparam 是**客户区坐标**
                            // （自定义消息 WM_MENU_RETURN 才用屏幕坐标）——
                            // 直接塞屏幕坐标会让子菜单在错误的位置判命中。
                            let mut cp = sp;
                            let _ = ScreenToClient(sub, &mut cp);
                            let _ = PostMessageW(
                                Some(sub),
                                WM_MOUSEMOVE,
                                WPARAM(0),
                                LPARAM(((cp.y as isize) << 16) | (cp.x as isize & 0xFFFF)),
                            );
                        }
                    }
                }
            }
            LRESULT(0)
        }
        // 子菜单把鼠标交还回来：按真实光标位置重算高亮。
        // 第53轮：子菜单「单击固定展开」—— 光标回到父菜单**不再**关掉子菜单，
        // 只把 capture 接回来刷新父菜单高亮；子菜单继续挂在屏幕上。
        WM_MENU_RETURN => {
            let sx = (lparam.0 & 0xFFFF) as i16 as i32;
            let sy = (lparam.0 >> 16) as i16 as i32;
            let mut pt = POINT { x: sx, y: sy };
            let _ = ScreenToClient(hwnd, &mut pt);
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
            // wparam = 被销毁的那个子菜单句柄（第54轮带上的身份）。
            // **只认"当前这一个"**：过期通知（悬停/点击切到另一个子菜单时，
            // 旧子菜单销毁的那条晚一步到）必须忽略 —— 否则会把刚开好的新子菜单
            // 的状态清掉，并把捕获从它手里抢回来，让新子菜单从此收不到鼠标
            // （点它里面的条目没反应，看着像"菜单悬停展示没了"）。
            let who = HWND(wparam.0 as *mut core::ffi::c_void);
            if state.submenu != Some(who) {
                return LRESULT(0);
            }
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
        // 子菜单上报的点击（落点在子菜单外面）。裁决权在本层：
        //   落在自己身上 → 当成一次点击处理（切换/收起子菜单、执行条目）
        //   自己还有父菜单 → 继续往上传
        //   都不是（点在桌面/别的窗口上）→ 整条链关闭
        WM_MENU_FORWARD_CLICK => {
            let sx = (lparam.0 & 0xFFFF) as i16 as i32;
            let sy = (lparam.0 >> 16) as i16 as i32;
            let mut pt = POINT { x: sx, y: sy };
            let _ = ScreenToClient(hwnd, &mut pt);
            if inside_popup(state, pt.x, pt.y) {
                // capture 收回来：之后光标在本菜单上移动要由本窗口接管
                let _ = SetCapture(hwnd);
                if let Some(idx) = entry_at(state, pt.y) {
                    let entry = state.entries[idx].clone();
                    activate_at(state, hwnd, idx, &entry);
                }
            } else if let Some(p) = state.parent_menu {
                if IsWindow(Some(p)).as_bool() {
                    let _ = PostMessageW(Some(p), WM_MENU_FORWARD_CLICK, WPARAM(0), lparam);
                    return LRESULT(0);
                }
                state.activated = true;
                let _ = DestroyWindow(hwnd);
            } else {
                state.activated = true;
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let y = (lparam.0 >> 16) as i16 as i32;
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            // 任何一次真实点击都作废还没到期的悬停展开：点了就是"我要这一项"，
            // 不该再被计时器补一个子菜单出来。
            cancel_pending_expand(state, hwnd);
            // 弹出本窗口的那一次"按下"的抬起：只吞掉这一次。
            // 不吞的话菜单刚弹出就被自己关掉 —— 用户必须一直按住才看得见它。
            if state.swallow_up {
                state.swallow_up = false;
                // 顺便按真实光标位置把高亮算出来（补发的那条 MOUSEMOVE 可能已被丢弃）
                let idx = if inside_popup(state, x, y) { entry_at(state, y) } else { None };
                if idx != state.hover {
                    state.hover = idx;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                return LRESULT(0);
            }
            // 关键：因为有 SetCapture，屏幕任意位置的点击都会投递到这里。
            // 落在弹出菜单之外时：
            //   * 顶层菜单 → 点空白处 = 关掉**整条**菜单链。只销毁自己会让父菜单
            //     变成一个还挂在屏幕上、却再也收不到鼠标消息的空壳（表现就是
            //     "菜单关不掉"）。
            //   * 子菜单（第53轮）→ **不关链**，把点击原样上报给父菜单裁决
            //     （WM_MENU_FORWARD_CLICK）。之前这里直接 activated=true 自爆，
            //     父菜单跟着销毁 —— 用户点「排序方式」，整条菜单凭空消失，
            //     子菜单就永远"打不开"。
            if !inside_popup(state, x, y) {
                let mut sp = POINT { x, y };
                let _ = ClientToScreen(hwnd, &mut sp);
                let packed = LPARAM(((sp.y as isize) << 16) | (sp.x as isize & 0xFFFF));
                match state.parent_menu {
                    Some(p) if IsWindow(Some(p)).as_bool() => {
                        let _ = PostMessageW(Some(p), WM_MENU_FORWARD_CLICK, WPARAM(0), packed);
                    }
                    _ => {
                        state.activated = true;
                        let _ = DestroyWindow(hwnd);
                    }
                }
                return LRESULT(0);
            }
            if let Some(idx) = entry_at(state, y) {
                let entry = state.entries[idx].clone();
                // 把命中的下标一起传进去：子菜单定位不能依赖 hover
                // （hover 可能被"弹出瞬间那条补发消息"左右，也可能还没更新）。
                activate_at(state, hwnd, idx, &entry);
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
            cancel_pending_expand(state, hwnd);
            if let Some(sub) = state.submenu {
                let _ = DestroyWindow(sub);
            }
            // 离屏位图是本窗口 GDI 对象的持有者：DestroyWindow 不会替我们
            // DeleteDC/DeleteObject（选在里面的位图还会挡住删除）。漏了这一步
            // 就是每次开一次菜单泄漏一块 DC + 一块位图。
            if let Some(mut c) = state.cache.take() {
                c.free();
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
                    // wparam = **自己的句柄**（第54轮）：让父菜单能分辨这条通知是谁发的。
                    // open_submenu_at 里"销毁旧的 → 立刻开新的"是同步发生的，旧的
                    // 那条 SUBCLOSED 会晚一步到达；不带上身份的话父菜单只能无条件
                    // 清状态，结果是把**刚开好的新子菜单**的状态清掉并抢走它的捕获
                    // （捕获被抢 = 新子菜单收不到鼠标，点里面的条目毫无反应）。
                    let _ = PostMessageW(
                        Some(p),
                        if activated { WM_MENU_CLOSE_CHAIN } else { WM_MENU_SUBCLOSED },
                        WPARAM(hwnd.0 as usize),
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
        // 已展开再点一次 = 收起（第53轮"单击固定展开"的开关语义）
        if let Some(sub) = state.submenu.take() {
            unsafe {
                let _ = DestroyWindow(sub);
                // 子菜单销毁会连本窗口的 capture 一起释放，同步抢回来
                // （WM_MENU_SUBCLOSED 里还会补一次，这里先抢避免空窗期丢消息）
                let _ = SetCapture(hwnd);
            }
        }
        state.open_submenu_for = None;
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
        false,
    );
    match sub {
        Some(s) => {
            // 告诉子菜单"你是被第 index 项展开的"：它靠这个判断光标是否移到了
            // 父菜单的**另一项**上，避免刚弹出就被自己关掉。
            let p = unsafe { GetWindowLongPtrW(s, GWLP_USERDATA) as *mut MenuWindow };
            if !p.is_null() {
                unsafe { (*p).opened_by = Some(index) };
            }
            state.submenu = Some(s);
            state.open_submenu_for = Some(index);
        }
        None => state.open_submenu_for = None,
    }
}

/// 执行第 `index` 项。`index` 由调用方从命中判定直接给出，不再依赖 `hover`。
fn activate_at(state: &mut MenuWindow, hwnd: HWND, index: usize, entry: &MenuEntry) {
    if entry.disabled {
        return;
    }
    if entry.has_submenu && !entry.submenu.is_empty() {
        // 点击带子菜单的项：展开/收起（开关语义在 open_submenu_at 里）。
        // （原来这里用 `std::ptr::eq` 在 entries 里找自己，而 entry 是 clone 出来的，
        //   地址永远不相等 → 定位恒失败，子菜单总是弹在菜单顶部第一行。）
        open_submenu_at(state, hwnd, index);
        return;
    }
    activate(state, hwnd, entry)
}

fn activate(state: &mut MenuWindow, hwnd: HWND, entry: &MenuEntry) {
    if entry.disabled {
        return;
    }
    if entry.has_submenu && !entry.submenu.is_empty() {
        // 兜底：拿不到下标时（键盘回车）退回用 hover
        if let Some(i) = state.hover {
            if state.entries[i].has_submenu {
                open_submenu_at(state, hwnd, i);
                return;
            }
        }
        return;
    }
    let app = state.app;
    let action = entry.action.clone();
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
///
/// 透明色按位图句柄缓存：句柄在整条菜单的生命周期里不变，而 `GetPixel` 是 GDI
/// 里最慢的调用之一 —— 每重画一次（鼠标每移动一行就是一次）都为每个 Shell 条目
/// 问一遍，二十来项叠起来正好是"在条目间快速移动时闪那么几毫秒"。
static BMP_KEY: std::sync::Mutex<Vec<(isize, u32)>> = std::sync::Mutex::new(Vec::new());

unsafe fn draw_bmp(hdc: HDC, hbmp: isize, x: i32, y: i32, size: i32) {
    use windows::Win32::Graphics::Gdi::{SelectObject, TransparentBlt, HBITMAP};
    let mem = CreateCompatibleDC(Some(hdc));
    if mem.is_invalid() {
        return;
    }
    let old = SelectObject(mem, HBITMAP(hbmp as *mut core::ffi::c_void).into());
    let key = match BMP_KEY.lock() {
        Ok(mut g) => match g.iter().find(|(h, _)| *h == hbmp).map(|(_, k)| *k) {
            Some(k) => k,
            None => {
                let k = GetPixel(mem, 0, size.saturating_sub(1)).0;
                g.push((hbmp, k));
                k
            }
        },
        Err(_) => GetPixel(mem, 0, size.saturating_sub(1)).0,
    };
    let ok = TransparentBlt(hdc, x, y, size, size, mem, 0, 0, size, size, key).as_bool();
    if !ok {
        let _ = BitBlt(hdc, x, y, size, size, Some(mem), 0, 0, SRCCOPY);
    }
    SelectObject(mem, old);
    let _ = DeleteDC(mem);
}

/// 把一串菜单条目画进 `hdc`（**不弹窗口**）。
///
/// 真菜单窗口和设置页「右键菜单」那一块的实时预览都走 `paint_menu` 同一份渲染：
/// 预览要是另写一份，两边的行高/图标槽/箭头迟早会长歪，那就不是"预览"而是
/// "另一张示意图"。所以这里造一个一次性的 `MenuWindow` 交进去。
pub unsafe fn paint_menu_into(
    hdc: HDC,
    entries: &[MenuEntry],
    width: i32,
    view_h: i32,
    row_h: i32,
    scale: f32,
    theme: &Theme,
    hover: Option<usize>,
    ox: i32,
    oy: i32,
) {
    let st = MenuWindow {
        theme: *theme,
        entries: entries.to_vec(),
        hover,
        app: std::ptr::null_mut(),
        scale,
        row_h,
        width,
        view_h,
        scroll: 0,
        ox,
        oy,
        parent_hwnd: HWND(std::ptr::null_mut()),
        submenu: None,
        open_submenu_for: None,
        parent_menu: None,
        opened_by: None,
        open_pos: None,
        activated: false,
        swallow_up: false,
        cache: None,
        pending_expand: None,
        hover_moved: false,
    };
    paint_menu(hdc, &st);
}

/// 按「外观」页的圆角画一块实心矩形。
///
/// 菜单绘制拿不到 `App`（`MenuWindow` 里只存了主题快照），去全局那份 App 取
/// —— 取不到就退回方角，不能因为拿不到就把菜单画崩。
unsafe fn menu_fill_radius(hdc: HDC, id: &str, r: &RECT, fill: windows::Win32::Foundation::COLORREF) {
    let Some(app) = crate::app_ref_opt() else {
        let br = CreateSolidBrush(fill);
        let _ = FillRect(hdc, r, br);
        let _ = DeleteObject(br.into());
        return;
    };
    crate::paint::fill_radius(&*app, hdc, id, r, fill, None);
}

/// 菜单的板底 + 裁剪 + 字体，在内容坐标系下画好，返回旧字体句柄。
///
/// `hdc` 的窗口原点已经由 `SetWindowOrgEx(-ox, scroll-oy)` 设成内容坐标系，
/// 所以下面所有 y 都是**内容 y**，行高/位置和命中判定用的是同一套。
unsafe fn begin_menu_paint(hdc: HDC, state: &MenuWindow) -> windows::Win32::Graphics::Gdi::HGDIOBJ {
    let t = &state.theme;
    // 客户区高度就是可见高度；内容更长时由滚动偏移决定画哪一段。
    //
    // 这块底板**必须自己带上 ox/oy**：此时窗口原点还没设（下一行才
    // SetWindowOrgEx），画的是设备坐标。真菜单窗口 ox=oy=0 看不出差别，
    // 设置页「右键菜单」里 ox/oy 是面板左上角，少了这个偏移整块底板会
    // 糊到窗口 (0,0) 上，把侧栏和标题栏一起盖掉。
    let r = RECT {
        left: state.ox,
        top: state.oy,
        right: state.ox + state.width,
        bottom: state.oy + state.view_h,
    };
    menu_fill_radius(hdc, "menu.panel", &r, t.surface);

    // 裁剪到可见区，避免超长内容画到窗口外（也会溢出到屏幕上）
    let _ = IntersectClipRect(hdc, r.left, r.top, r.right, r.bottom);
    SetWindowOrgEx(hdc, -state.ox, state.scroll - state.oy, None);

    let font = crate::paint::menu_font(state.scale);
    let old_font = SelectObject(hdc, font.into());
    SetBkMode(hdc, TRANSPARENT);
    old_font
}

/// 第 `index` 项的**内容坐标**顶边。
fn entry_top(state: &MenuWindow, index: usize) -> i32 {
    let mut top = 4;
    for e in state.entries.iter().take(index) {
        top += if e.separator { 6 } else { state.row_h };
    }
    top
}

/// 画第 `index` 项。`hovered` 由调用方给 —— 局部重画时必须能单独指定
/// "这一行现在算不算悬停"，不能读全局 `state.hover`。
unsafe fn paint_entry(hdc: HDC, state: &MenuWindow, index: usize, top: i32, hovered: bool) {
    let t = &state.theme;
    let e = &state.entries[index];
    if e.separator {
        let line = CreateSolidBrush(t.border);
        let lr = RECT { left: (10.0 * state.scale) as i32, top: top + 3, right: state.width - (10.0 * state.scale) as i32, bottom: top + 4 };
        let _ = FillRect(hdc, &lr, line);
        DeleteObject(line.into());
        return;
    }
    let rect = RECT { left: 0, top, right: state.width, bottom: top + state.row_h };
    if hovered && !e.disabled {
        menu_fill_radius(hdc, "menu.hover", &rect, t.hover_bg);
        // 左侧再压一条强调竖线：光靠底色在暗色主题下对比仍然偏弱，
        // 加一条 3px 的重色边，鼠标在哪一行一眼就能看出来。
        let ab = CreateSolidBrush(t.accent);
        let ar = RECT { left: 0, top, right: (3.0 * state.scale) as i32, bottom: top + state.row_h };
        let _ = FillRect(hdc, &ar, ab);
        DeleteObject(ab.into());
    }
    // 「当前生效的那一项」：和悬停同构（行首竖线），但换橙色。
    // 悬停时优先画蓝色（鼠标位置是更即时、更需要看清的信息），
    // 没悬停才显示橙色的选中标记。
    if e.selected && !hovered {
        let ob = CreateSolidBrush(t.orange);
        let or_ = RECT { left: 0, top, right: (3.0 * state.scale) as i32, bottom: top + state.row_h };
        let _ = FillRect(hdc, &or_, ob);
        DeleteObject(ob.into());
    }
    // 禁用态要和 Windows 一致：字淡到"读得出但明显不可用"，并且**不参与
    // 悬停高亮**（上面 hovered 已经排除了）。只改颜色不改布局，
    // 免得菜单宽度随状态跳来跳去。
    SetTextColor(hdc, if e.disabled { t.text_dim } else { t.text });
    let icon_pad = (10.0 * state.scale) as i32;
    if let Some(glyph) = e.glyph {
        // 空字形喂给 DrawTextW 会崩（windows-rs 对空切片解引用）。
        // 自有命令都带 glyph，但设置页预览能造出空串条目，守卫必须留在这里。
        if !glyph.is_empty() {
            let gfont = crate::paint::glyph_font(state.scale);
            let old2 = SelectObject(hdc, gfont.into());
            let mut g: Vec<u16> = glyph.encode_utf16().collect();
            let gr = RECT { left: icon_pad, top, right: icon_pad + (28.0 * state.scale) as i32, bottom: top + state.row_h };
            DrawTextW(hdc, &mut g, &mut gr.clone(), DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            SelectObject(hdc, old2);
        }
    }
    if let Some(hbmp) = e.bmp {
        draw_bmp(hdc, hbmp, icon_pad, top + (state.row_h - 16) / 2, 16);
    }
    let icon_left = icon_pad + (28.0 * state.scale) as i32;
    let mut text: Vec<u16> = e.text.encode_utf16().collect();
    let tr = RECT {
        left: icon_left,
        top,
        right: state.width - (26.0 * state.scale) as i32,
        bottom: top + state.row_h,
    };
    DrawTextW(hdc, &mut text, &mut tr.clone(), DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
    if e.has_submenu {
        // 与侧栏**同一个**实心三角，只是方向朝右（`down = false`）。
        // 之前是拿 ASCII 的 ">" 当箭头：字号小、笔画细、位置还随字体漂，
        // 和侧栏那套树形箭头完全不是一个东西。
        let ar = RECT {
            left: state.width - (22.0 * state.scale) as i32,
            top,
            right: state.width - 6,
            bottom: top + state.row_h,
        };
        crate::paint::draw_tree_chevron(hdc, &ar, t.text_dim, false);
    }
}

/// 顶/底的滚动提示三角（画在设备坐标上，不吃滚动偏移）。
unsafe fn paint_scroll_hint(hdc: HDC, state: &MenuWindow) {
    let t = &state.theme;
    let more_up = state.scroll > 0;
    let more_down = state.scroll < max_scroll(state);
    if !more_up && !more_down {
        return;
    }
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

unsafe fn paint_menu(hdc: HDC, state: &MenuWindow) {
    unsafe {
        let old_font = begin_menu_paint(hdc, state);
        for i in 0..state.entries.len() {
            paint_entry(hdc, state, i, entry_top(state, i), state.hover == Some(i));
        }
        SetWindowOrgEx(hdc, -state.ox, -state.oy, None);
        paint_scroll_hint(hdc, state);
        SelectObject(hdc, old_font);
    }
}

/// 菜单窗口的 `WM_PAINT`：走离屏位图缓存，换高亮只重画脏掉的那一两行。
///
/// 判据是缓存里记录的 `hover`/`scroll` 与当前值是否一致：
///   * 尺寸变了（缩放/DPI）→ 缓存重建 → 整块画
///   * 滚动位置变了 → 缓存只覆盖旧的那一屏 → 整块画
///   * 只有高亮变了 → **只重画旧高亮行和新高亮行**，其余行一个 GDI 调用都不发
unsafe fn paint_menu_window(hdc: HDC, state: &mut MenuWindow, hwnd: HWND) {
    unsafe {
        ensure_cache(state, hwnd);
        let Some((w, h)) = state.cache.as_ref().map(|c| (c.w, c.h)) else {
            // 缓存建不起来（极端情况下 GDI 会失败）：退回一次性直绘。
            paint_menu(hdc, state);
            return;
        };
        let scroll = state.scroll;
        let hover = state.hover;
        let full = match state.cache.as_ref() {
            Some(c) => c.scroll != scroll,
            None => true,
        };
        if full {
            let dc = state.cache.as_ref().unwrap().dc;
            let old_font = begin_menu_paint(dc, state);
            for i in 0..state.entries.len() {
                paint_entry(dc, state, i, entry_top(state, i), hover == Some(i));
            }
            SetWindowOrgEx(dc, -state.ox, -state.oy, None);
            paint_scroll_hint(dc, state);
            SelectObject(dc, old_font);
            let c = state.cache.as_mut().unwrap();
            c.scroll = scroll;
            c.hover = hover;
        } else {
            // 只补画"高亮从 A 变到 B"涉及的至多两行。
            //
            // 必须**先擦掉该行的底色再重画**：上一帧这行画过高亮圆角底，
            // 直接画上去会和旧底色叠在一起（圆角处露出上一帧的弧边）。
            let dc = state.cache.as_ref().unwrap().dc;
            let prev = state.cache.as_ref().unwrap().hover;
            state.cache.as_mut().unwrap().hover = hover;
            if prev != hover {
                for idx in [prev, hover].into_iter().flatten() {
                    if idx >= state.entries.len() {
                        continue;
                    }
                    let ctop = entry_top(state, idx);
                    let row_h = if state.entries[idx].separator { 6 } else { state.row_h };
                    // 这一行在**设备坐标**里的纵向范围（缓存位图是设备坐标）。
                    // 滚出可见区的行本来就不在缓存里，跳过。
                    let y0 = (ctop - scroll).max(0);
                    let y1 = (ctop + row_h - scroll).min(state.view_h);
                    if y1 <= y0 {
                        continue;
                    }
                    let saved = SaveDC(dc);
                    // 擦底区域必须**与面板的圆角求交**：第一行/最后一行擦的是
                    // 整条宽度，直接铺方块会把面板的圆角抹平成直角。
                    let panel = CreateRoundRectRgn(
                        state.ox,
                        state.oy,
                        state.ox + state.width,
                        state.oy + state.view_h,
                        panel_d(state),
                        panel_d(state),
                    );
                    let band = CreateRectRgn(state.ox, state.oy + y0, state.ox + state.width, state.oy + y1);
                    let _ = CombineRgn(Some(panel), Some(panel), Some(band), RGN_AND);
                    let _ = DeleteObject(band.into());
                    let _ = SelectClipRgn(dc, Some(panel));
                    let br = CreateSolidBrush(state.theme.surface);
                    let er = RECT { left: state.ox, top: state.oy + y0, right: state.ox + state.width, bottom: state.oy + y1 };
                    let _ = FillRect(dc, &er, br);
                    DeleteObject(br.into());
                    // 擦干净了，把裁剪恢复到整块面板再画这一行
                    // （paint_entry 里那些圆角高亮底自己也要被裁在面板内）。
                    let full = CreateRoundRectRgn(
                        state.ox,
                        state.oy,
                        state.ox + state.width,
                        state.oy + state.view_h,
                        panel_d(state),
                        panel_d(state),
                    );
                    let _ = SelectClipRgn(dc, Some(full));
                    let font = crate::paint::menu_font(state.scale);
                    let of = SelectObject(dc, font.into());
                    SetBkMode(dc, TRANSPARENT);
                    // 这里画的是**设备坐标**（不设窗口原点），所以传给
                    // paint_entry 的 y 要自己减掉滚动偏移。
                    paint_entry(dc, state, idx, ctop - scroll, hover == Some(idx));
                    SelectObject(dc, of);
                    let _ = RestoreDC(dc, saved);
                    let _ = DeleteObject(full.into());
                }
            }
        }
        let dc = state.cache.as_ref().unwrap().dc;
        let _ = BitBlt(hdc, 0, 0, w.min(state.width), h.min(state.view_h), Some(dc), 0, 0, SRCCOPY);
    }
}

/// 面板圆角的椭圆直径，必须和 `fill_radius` 里那个 `d` 同口径
/// （`d = (rad*2).min(w).min(h)`，传的是**直径**不是半径）。
fn panel_d(state: &MenuWindow) -> i32 {
    let rad = crate::app_ref_opt().map(|a| a.radius_of("menu.panel") as i32).unwrap_or(0);
    if rad <= 0 {
        return 0;
    }
    (rad * 2).min(state.width).min(state.view_h)
}

