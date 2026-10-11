//! 主题与度量（技术方案 §14.4）。
//!
//! 颜色全部集中于此，便于将来替换为主题 JSON 文件做热重载。
//! dark 为默认：SDR 下保证正文对比度 ≥ 7:1，次要文字 ≥ 4.5:1。

use windows::Win32::Foundation::COLORREF;

/// 把 0xRRGGBB 转成 GDI 的 0x00BBGGRR。
pub const fn rgb(hex: u32) -> COLORREF {
    let r = (hex >> 16) & 0xFF;
    let g = (hex >> 8) & 0xFF;
    let b = hex & 0xFF;
    COLORREF((b << 16) | (g << 8) | r)
}

#[derive(Clone, Copy)]
pub struct Theme {
    pub window_bg: COLORREF,
    pub chrome_bg: COLORREF,
    /// 标题栏底色。资源管理器暗色主题下标题栏是纯黑，与命令栏/导航窗格不同色。
    pub title_bg: COLORREF,
    /// 状态栏底色，比内容区亮一档。
    pub status_bg: COLORREF,
    pub surface: COLORREF,
    pub surface_alt: COLORREF,
    pub text: COLORREF,
    pub text_dim: COLORREF,
    pub accent: COLORREF,
    pub selection_bg: COLORREF,
    pub selection_text: COLORREF,
    pub hover_bg: COLORREF,
    pub border: COLORREF,
    pub danger: COLORREF,
    pub tab_active: COLORREF,
    /// 菜单里"当前生效的那一项"的标记色。与 hover 同构（同样是行首一条竖线），
    /// 只是换成橙色：蓝色竖条已经被"鼠标正悬停在哪一行"占用了，两者撞色就分不清
    /// "我指在这儿"和"现在用的是这个"。
    pub orange: COLORREF,
}

/// 暗色配色取自本机 Windows 10 资源管理器的实测像素（CabinetWClass，DPI 96）：
///   标题栏 #000000 / 命令栏与导航窗格 #191919 / 列表内容区 #202020 / 状态栏 #333333
///   分隔线 #535353 / 正文 #DEDEDE / 强调蓝 #4375CE
/// 逐项对齐，避免"看起来像另一个软件"。
pub fn dark() -> Theme {
    Theme {
        window_bg: rgb(0x202020),
        chrome_bg: rgb(0x191919),
        title_bg: rgb(0x000000),
        status_bg: rgb(0x333333),
        surface: rgb(0x2B2B2B),
        surface_alt: rgb(0x2B2B2B),
        text: rgb(0xDEDEDE),
        text_dim: rgb(0x8F8F8F),
        accent: rgb(0x4375CE),
        selection_bg: rgb(0x474747),
        selection_text: rgb(0xFFFFFF),
        // 悬停色必须**明显亮于**它所处的底色，否则等于没有悬停反馈。
        // 之前这里是 #2A2A2A，而自绘菜单的底色 surface 是 #2B2B2B —— 悬停后反而
        // 暗了 1 个色阶，肉眼完全分辨不出来，表现就是"鼠标移上去毫无反应"。
        // 现在按"内容区 #202020 < 悬停 #3D3D3D < 选中 #474747"拉开层次：
        // 列表里差 29 级、菜单里差 18 级，都足够看清。
        hover_bg: rgb(0x3D3D3D),
        border: rgb(0x535353),
        danger: rgb(0xE4585A),
        tab_active: rgb(0x2B2B2B),
        orange: rgb(0xE8A33D),
    }
}

pub fn light() -> Theme {
    Theme {
        window_bg: rgb(0xFFFFFF),
        chrome_bg: rgb(0xF3F3F3),
        title_bg: rgb(0xF3F3F3),
        status_bg: rgb(0xF3F3F3),
        surface: rgb(0xFFFFFF),
        surface_alt: rgb(0xEDEDED),
        text: rgb(0x1F2124),
        text_dim: rgb(0x5C6166),
        accent: rgb(0x0A62D0),
        selection_bg: rgb(0xCBE3FF),
        selection_text: rgb(0x101418),
        hover_bg: rgb(0xE5E5E5),
        border: rgb(0xC8C8C8),
        danger: rgb(0xC42B2B),
        tab_active: rgb(0xFFFFFF),
        orange: rgb(0xC2700A),
    }
}

/// 每处尺寸都随 DPI 缩放，避免自绘界面在高分屏上错位。
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub scale: f32,
    pub title_h: i32,
    pub toolbar_h: i32,
    pub status_h: i32,
    pub sidebar_w: i32,
    pub row_h: i32,
    pub pad: i32,
}

impl Metrics {
    pub fn new(dpi: u32) -> Self {
        let scale = dpi as f32 / 96.0;
        Metrics {
            scale,
            title_h: (34.0 * scale).round() as i32,
            toolbar_h: (40.0 * scale).round() as i32,
            status_h: (23.0 * scale).round() as i32,
            // 导航窗格宽度与资源管理器一致（实测内容区从 x=157 开始）
            sidebar_w: (157.0 * scale).round() as i32,
            row_h: (21.0 * scale).round() as i32,
            pad: (8.0 * scale).round() as i32,
        }
    }

    pub fn row_height_for(&self, mode: fm_core::ViewMode) -> i32 {
        ((match mode {
            // 21px 是资源管理器详细信息/列表视图的实测行距
            fm_core::ViewMode::Details | fm_core::ViewMode::List => 21.0,
            fm_core::ViewMode::MediumIcons => 88.0,
            fm_core::ViewMode::LargeIcons => 128.0,
        }) * self.scale)
            .round() as i32
    }
}
