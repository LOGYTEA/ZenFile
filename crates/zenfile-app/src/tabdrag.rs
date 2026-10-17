//! 标签的跨窗口转移（第83轮）：把一个标签从一个 ZenFile 窗口拖到**另一个**
//! ZenFile 窗口（HWND），像浏览器标签那样。
//!
//! ## 为什么用 `WM_COPYDATA` 而不是 OLE 数据对象
//!
//! 同进程两个窗口之间传一条"路径 + 屏幕落点"，用自定义消息是几十行；
//! 走 OLE 要额外造一个 `IDataObject`、注册私有剪贴板格式、注册
//! `IDropTarget`、处理四个回调 —— 几百行，而且**多出来一堆只在拖拽期间
//! 有意义的生命周期**（`HOOVER` 静态、注册/撤销）。收益为零。
//!
//! 外部程序拖入（资源管理器把文件夹拖进 ZenFile）走 `DragAcceptFiles` +
//! `WM_DROPFILES`，那是 OLE 之外的另一条老路，**没有 COM 契约**，
//! 十几行就够，也不用注册任何东西。两条路各走各自最短的那条。
//!
//! ## 落点判定必须由**目标窗口**做
//!
//! 源窗口拿不到对方窗口的标签条 / 内容区几何。所以只把**屏幕坐标**递过去，
//! 目标窗口收到后转成自己的客户区坐标，再走**自己**那份
//! `paint::dock_zone_in`（与绘制共用同一个函数）。协议里不带任何几何。
//!
//! ## 单进程多窗口
//!
//! `SendMessageW` 在同进程里是**直接调用目标窗口过程**，不排队，所以不存在
//! 两个窗口的消息竞争；跨进程时系统按线程挂起本线程、目标处理完再返回，
//! 语义一样。载荷在目标返回前一直有效（`COPYDATASTRUCT` 指向调用栈上的
//! 缓冲），这正是 `SendMessage` 与 `PostMessage` 的关键差别 —— 不能改成
//! `PostMessageW`，那样缓冲在目标读之前就没了。

use std::path::PathBuf;

use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetClassNameW, WindowFromPoint, GA_ROOT,
};
use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};

use crate::app::DockZone;

/// `COPYDATASTRUCT.dwData` 的取值。**1 已被托盘宿主占用**（"开一个新窗口"，
/// `main.rs` 里 `tray_proc` 那段），这里从 2 起，且只发给主窗口。
///
/// **2 = 松手落地**（递路径 + 落点）。
/// **3 = 悬停预览**（只递落点，不带路径 —— 目标窗口画高亮只需要坐标）。
const DW_TAB_XFER: usize = 2;
const DW_TAB_HOVER: usize = 3;
/// 悬停离开：告诉目标窗口把高亮擦掉。
const DW_TAB_HOVER_END: usize = 4;
/// 载荷最小时也要装得下 4 个 i32 坐标（8 字节）。
const MIN_CB: u32 = 8;

/// 载荷：`i32 屏幕x` + `i32 屏幕y` +（仅落地时）路径的 UTF-16（含结尾 NUL）。
///
/// 用定长头 + 变长尾，不引序列化格式：这个协议的另一端永远是本进程自己。
fn encode_xy(screen: (i32, i32)) -> Vec<u16> {
    vec![screen.0 as u16, (screen.0 >> 16) as u16, screen.1 as u16, (screen.1 >> 16) as u16]
}

fn encode(screen: (i32, i32), path: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let mut v = encode_xy(screen);
    v.extend(path.as_os_str().encode_wide());
    v.push(0);
    v
}

fn decode_xy(buf: &[u16]) -> Option<(i32, i32)> {
    if buf.len() < 4 {
        return None;
    }
    let g16 = |a: u16, b: u16| i32::from(a as i16) | (i32::from(b) << 16);
    Some((g16(buf[0], buf[1]), g16(buf[2], buf[3])))
}

fn decode(buf: &[u16]) -> Option<((i32, i32), PathBuf)> {
    let (screen, rest) = (decode_xy(buf)?, &buf[4..]);
    let s: String = String::from_utf16_lossy(rest);
    let s = s.trim_end_matches('\0');
    if s.is_empty() {
        return None;
    }
    Some((screen, PathBuf::from(s)))
}

/// 光标当前压在哪个**顶层窗口**上。标签条/内容区命中要在目标窗口里做，
/// 这里只负责"是不是我家窗口"。
pub fn window_under_cursor() -> Option<HWND> {
    unsafe {
        let mut pt = POINT::default();
        if windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt).is_err() {
            return None;
        }
        let h = WindowFromPoint(pt);
        if h.0.is_null() {
            return None;
        }
        let root = GetAncestor(h, GA_ROOT);
        if root.0.is_null() {
            None
        } else {
            Some(root)
        }
    }
}

/// 这个 HWND 是不是 ZenFile 主窗口（**排除自己**）。
///
/// 排除自己是必须的：拖着自己窗口里的标签时，光标当然还在自己窗口上，
/// 不排除就会把每次移动都判成"拖到别的窗口去"。
pub fn other_zenfile_window(h: HWND, me: HWND) -> bool {
    if h == me || h.0.is_null() {
        return false;
    }
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(h, &mut buf) };
    if n <= 0 {
        return false;
    }
    let name = String::from_utf16_lossy(&buf[..n as usize]);
    name == "ZenFileMainWindow"
}

/// 让本窗口接收外部文件拖入（`WM_DROPFILES`）。窗口创建后调一次。
pub fn accept_files_from(hwnd: HWND) {
    unsafe { DragAcceptFiles(hwnd, true) }
}

/// 把一个标签送到目标窗口的屏幕落点。**目标窗口收到后自己开标签/分屏**。
pub fn send_tab(hwnd: HWND, screen: (i32, i32), path: &std::path::Path) {
    send_raw(hwnd, DW_TAB_XFER, &encode(screen, path));
}

/// 悬停预览：只递坐标，让目标窗口在**它自己**的坐标系里画高亮。
///
/// 不能带路径 —— 预览阶段还没决定要不要落，路径在源窗口手上，但高亮
/// 只需要知道"落在哪"。
pub fn send_hover(hwnd: HWND, screen: (i32, i32)) {
    send_raw(hwnd, DW_TAB_HOVER, &encode_xy(screen));
}

/// 告诉目标窗口擦掉高亮（光标已经不在它身上）。
pub fn clear_hover(hwnd: HWND) {
    send_raw(hwnd, DW_TAB_HOVER_END, &encode_xy((0, 0)));
}

fn send_raw(hwnd: HWND, tag: usize, words: &[u16]) {
    let mut cds = COPYDATASTRUCT {
        dwData: tag,
        cbData: (words.len() * 2) as u32,
        lpData: words.as_ptr() as *mut core::ffi::c_void,
    };
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::SendMessageW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::WM_COPYDATA,
            Some(WPARAM(0)),
            Some(LPARAM(&mut cds as *mut COPYDATASTRUCT as isize)),
        );
    }
}

// --------------------------------------------------------------------
// 外部程序拖入（`WM_DROPFILES`）
// --------------------------------------------------------------------

/// 收一条 `WM_DROPFILES` 的 `HDROP`，读成路径清单。返回 `None` 表示不是
/// 拖放消息或包是空的。
///
/// 走 `WM_DROPFILES` 而不是 OLE：这条老路没有 COM 契约（不需要
/// `IDropTarget`、不用注册私有格式、回调在**我们自己的** WndProc 里），
/// 十几行就够。代价是不支持"拖进内容区的某一半"这类精细落点 —— 外部拖入
/// 一律开成新标签，正好也是我们想要的行为。
pub fn read_wmdropfiles(hdrop: HDROP) -> Option<Vec<PathBuf>> {
    unsafe {
        let n = DragQueryFileW(hdrop, 0xFFFF_FFFF, None);
        if n == 0 {
            return None;
        }
        let mut v = Vec::with_capacity(n as usize);
        for i in 0..n {
            // 先问长度再取：省掉"给短了再取一遍"的分支（UTF-16 长度按字符数
            // 返回，不含结尾 NUL，所以缓冲给 +1）。
            let need = DragQueryFileW(hdrop, i, None) as usize;
            if need == 0 {
                continue;
            }
            let mut buf = vec![0u16; need + 1];
            if DragQueryFileW(hdrop, i, Some(&mut buf)) == 0 {
                continue;
            }
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            v.push(PathBuf::from(String::from_utf16_lossy(&buf[..end])));
        }
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    }
}

/// 把外部拖进来的一批路径开成标签。**只收文件夹**：拖进来的是文件的话
/// 开成标签就是"打开那个文件所在目录"，路径会指到文件自己，`open_tab` 拿它
/// 当目录去枚举必然失败。
pub fn accept_external(app: &mut crate::app::App, paths: Vec<PathBuf>) -> bool {
    let mut any = false;
    for p in paths {
        let is_dir = p.is_dir();
        let dir = if is_dir { Some(p.clone()) } else { p.parent().map(|d| d.to_path_buf()) };
        let Some(d) = dir else { continue };
        app.open_tab(d);
        any = true;
    }
    any
}

/// 一条 `WM_COPYDATA` 的解包结果。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Packet {
    /// 松手落地：路径 + 屏幕落点。
    Drop { screen: (i32, i32), path: PathBuf },
    /// 悬停预览：只有屏幕落点。
    Hover { screen: (i32, i32) },
    /// 离开：擦掉高亮。
    HoverEnd,
    /// 不是本协议的包（调用方继续走自己的分发）。
    Foreign,
}

/// 解一条 `WM_COPYDATA` 载荷。**不碰 `App`**：落地动作由 `main.rs` 的
/// WndProc 在已经拿到 `&mut App` 的那条消息里做。
pub fn decode_packet(lp: LPARAM) -> Packet {
    let cds = unsafe { &*(lp.0 as *const COPYDATASTRUCT) };
    if cds.dwData != DW_TAB_XFER
        && cds.dwData != DW_TAB_HOVER
        && cds.dwData != DW_TAB_HOVER_END
    {
        return Packet::Foreign;
    }
    if cds.lpData.is_null() || cds.cbData < MIN_CB {
        return Packet::Foreign;
    }
    let words = (cds.cbData as usize) / 2;
    let buf = unsafe { std::slice::from_raw_parts(cds.lpData as *const u16, words) };
    match cds.dwData {
        DW_TAB_HOVER_END => Packet::HoverEnd,
        DW_TAB_HOVER => match decode_xy(buf) {
            Some(screen) => Packet::Hover { screen },
            None => Packet::Foreign,
        },
        _ => match decode(buf) {
            Some((screen, path)) => Packet::Drop { screen, path },
            None => Packet::Foreign,
        },
    }
}

/// 落点判定。**纯函数** —— 命中规则写错时（"预览在这、落在那"）能单测。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DropAction {
    None,
    NewTab,
    Split { zone: DockZone },
}

/// 屏幕落点（客户区坐标）在目标窗口里意味着什么。
pub fn classify(tabs: &windows::Win32::Foundation::RECT, view: windows::Win32::Foundation::RECT, x: i32, y: i32) -> DropAction {
    // 标签条上 → 追加成新标签。判据用**纵向**范围 + 落在内容区左缘之内：
    // 标签条是横向一条，横坐标不设限（标签多了会滚动，右侧空处也该接）。
    if y >= tabs.top && y < tabs.bottom {
        return DropAction::NewTab;
    }
    if x < view.left || x > view.right || y < view.top || y > view.bottom {
        return DropAction::None;
    }
    DropAction::Split { zone: crate::paint::dock_zone_in(view, x, y) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::RECT;

    fn r(l: i32, t: i32, r: i32, b: i32) -> RECT {
        RECT { left: l, top: t, right: r, bottom: b }
    }

    #[test]
    fn 屏幕落点解码回原值() {
        let p = std::path::Path::new("C:\\Users\\幻羽\\下载");
        let buf = encode((1234, -567), p);
        let ((x, y), back) = decode(&buf).unwrap();
        assert_eq!((x, y), (1234, -567));
        assert_eq!(back, p);
    }

    #[test]
    fn 负坐标不被截断() {
        // 副屏在主屏左边/上边时屏幕坐标是负的，高 16 位有符号搬移必须对
        let ((x, y), _) = decode(&encode((-1920, -300), std::path::Path::new("D:\\t"))).unwrap();
        assert_eq!((x, y), (-1920, -300));
    }

    #[test]
    fn 标签条上收成新标签() {
        assert_eq!(classify(&r(0, 40, 1180, 66), r(0, 66, 1180, 700), 500, 50), DropAction::NewTab);
    }

    #[test]
    fn 内容区按八向贴靠() {
        let view = r(200, 66, 1180, 700);
        assert!(matches!(classify(&r(0, 40, 1180, 66), view, 210, 400), DropAction::Split { zone: DockZone::Left }));
        assert!(matches!(classify(&r(0, 40, 1180, 66), view, 1170, 400), DropAction::Split { zone: DockZone::Right }));
        assert!(matches!(classify(&r(0, 40, 1180, 66), view, 700, 70), DropAction::Split { zone: DockZone::Top }));
        assert!(matches!(classify(&r(0, 40, 1180, 66), view, 700, 690), DropAction::Split { zone: DockZone::Bottom }));
    }

    #[test]
    fn 工具栏与侧栏都不收() {
        let view = r(200, 66, 1180, 700);
        // 工具栏（标签条上方）
        assert_eq!(classify(&r(0, 40, 1180, 66), view, 500, 20), DropAction::None);
        // 侧栏（内容区左侧）
        assert_eq!(classify(&r(0, 40, 1180, 66), view, 100, 400), DropAction::None);
        // 窗口外
        assert_eq!(classify(&r(0, 40, 1180, 66), view, 1300, 400), DropAction::None);
    }

    #[test]
    fn 悬停包只带坐标不带路径() {
        let buf = encode_xy((300, 240));
        // 只有 4 个坐标字，没有路径尾部
        assert_eq!(buf.len(), 4);
        assert_eq!(decode_xy(&buf), Some((300, 240)));
        // 悬停包不能被当成落地包解（否则会拿空路径去 open_tab）
        assert_eq!(decode(&buf), None);
    }

    #[test]
    fn 包类型不能张冠李戴() {
        // 三个 tag 各走各的解包分支，任一混淆都会让"预览"变成"落地"。
        let p = std::path::Path::new("C:\\t");
        assert_eq!(decode(&encode((1, 2), p)), Some(((1, 2), p.to_path_buf())));
        // 落地包的坐标头能单独解出来（悬停只需要它）
        assert_eq!(decode_xy(&encode((1, 2), p)), Some((1, 2)));
    }
}
