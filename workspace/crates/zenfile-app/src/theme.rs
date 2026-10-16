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

// 第75轮：加 `PartialEq`，让 `App::child_edit_key` 能把"整份配色"当成一个
// 可比较的快照（子 EDIT 的底色/文字色都从这里来，配色一变就必须重画它们）。
// 只加派生、不改任何字段与取值。
#[derive(Clone, Copy, PartialEq)]
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
    /// 列表行"鼠标悬停"的底色。**必须比选中态弱一档**：悬停是"我会点到这"，
    /// 选中是"已经点了这"，两者一样重就分不清状态。
    /// 层次：内容区 < row_hover（悬停） < hover_bg（焦点行） < selection_bg（选中）。
    pub row_hover: COLORREF,
    // ── 第80轮·改动3：补齐三个此前完全缺失的交互维度 ──────────────────────
    //
    // 【本项目既有色阶不变量，改这三个字段时必须继续成立】
    //     window_bg < row_hover < hover_bg < press_bg < selection_bg
    // （深色为递增，浅色同序递减 —— 浅色下"越深＝越强"，方向相反但强弱次序一致。）
    // 理由：悬停 < 按下 < 选中，是一条**单向递进**的"意图强度"轴。
    // 一旦 press 比 hover 弱，按下按钮反而会变淡，用户得到的是"我按下去了，
    // 但它好像松开了一点"的反向反馈 —— 这是比"没有按下反馈"更糟的失败。
    //
    // ⚠️ 本轮（2026-02）取值时踩过的坑，留档：
    //     规格单里给的是深色 #383838，但 #383838 比 hover_bg #3D3D3D **更暗**，
    //     直接写进来就把上面这条不变量打断了（按下态比悬停态弱一档）。
    //     故深色改取 #424242：比 hover_bg 亮 5 级、比 selection_bg 暗 5 级，
    //     正好落在两者的几何中点，"按下"在视觉上明确重于"悬停"、轻于"选中"。
    //     浅色 #DCDCDC 原规格值本身合规，未作改动。

    /// 按下 / 激活态底色。此前这个维度**整个不存在**：按钮按下时只能沿用
    /// hover_bg，于是"按下"和"悬停"在界面上一模一样，点下去没有任何确认反馈。
    /// 取值受上条不变量约束：必须夹在 hover_bg 与 selection_bg 之间。
    pub press_bg: COLORREF,
    /// 禁用态前景色。此前靠 text_dim 硬凑，但两者语义不同：text_dim 是
    /// "次要信息"（仍然可读、仍然有意义），disabled 是"此处不可操作"
    /// （不可读、传达的是能力缺失）。混用会让整屏次要文字看起来像坏了。
    pub disabled: COLORREF,
    /// 键盘焦点环颜色。此前**完全没有焦点环** —— 纯键盘导航时当前控件无处指示，
    /// Tab 到哪全靠猜。这是无障碍必需项，不是装饰。
    pub focus_ring: COLORREF,
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
        // 悬停行：比内容区 #202020 亮 14 级，比焦点行 #3D3D3D 暗 15 级 ——
        // 一眼能看出"这里可以点"，又不至于和"已选中"抢注意力。
        row_hover: rgb(0x2E2E2E),
        // 不变量：window_bg #202020 < row_hover #2E2E2E < hover_bg #3D3D3D
        //       < press_bg #424242 < selection_bg #474747
        // 规格单原给 #383838，比 hover_bg 还暗，违反上述次序，已改为几何中点 #424242。
        press_bg: rgb(0x424242),
        // 比 text_dim #8F8F8F 更暗一档：禁用态要"退到背景里去"，
        // 不能仍保留次要文字那种"我读得到你"的清晰度。
        disabled: rgb(0x6A6A6A),
        // 比 accent #4375CE 亮：焦点环画在控件边框外侧，与控件自身底色相邻，
        // 用同一个蓝会和"链接/强调"语义混在一起。
        focus_ring: rgb(0x7BA7E8),
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
        // 亮色下反过来：悬停比焦点行浅一档（#F2F2F2 > #E5E5E5）
        row_hover: rgb(0xF2F2F2),
        // 不变量（浅色为递减）：window_bg #FFFFFF > row_hover #F2F2F2 > hover_bg #E5E5E5
        //       > press_bg #DCDCDC > selection_bg #CBE3FF
        press_bg: rgb(0xDCDCDC),
        // 比 text_dim #5C6166 更浅：与深色同理，禁用态要"退进背景"而不是"变淡但仍清晰"。
        disabled: rgb(0xA8ADB2),
        focus_ring: rgb(0x4A8FE0),
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
            // 标题栏 34 → 38：正文 13 → 14px 后，34px 高度下上下留白各只剩 10px，
            // 中文 14px 字面高约 19px，行高 1.5（见 LINE_H_RATIO）需要 21px，
            // 34 - 21 = 13px 呼吸区偏挤；38px 才是能真正"装下新字号"的最小值。
            title_h: (38.0 * scale).round() as i32,
            // 工具栏 40 → 45：Tauri 模板按钮实测高 45px，这是本轮最硬的外部锚点
            //（Tauri 侧为实测值而非推测），其余常量都是围绕它对齐的。
            toolbar_h: (45.0 * scale).round() as i32,
            // 状态栏 23 → 28：同上加大的字号与点击目标高度需要。
            status_h: (28.0 * scale).round() as i32,
            // 导航窗格宽度与资源管理器一致（实测内容区从 x=157 开始）
            sidebar_w: (157.0 * scale).round() as i32,
            // 行高 21 → 22：**这是权衡结果，不是照抄 Tauri/shadcn 的 20。**
            //
            // 参照值确实是 20px（shadcn 默认行高、多数 Web 列表的取值），但本项目
            // 正文要从 13px 提到 14px，而**中文字形的笔画密度高于拉丁字母**：
            // 同样 14px 下，中文汉字的字面高与视觉重心都比拉丁字母更高更满，
            // 20px 行高配 14px 中文会显得上下贴边、行与行之间"挤在一起"，
            // 长串中文件名（.docx/.rar/.zip 之类）尤其明显。
            // 多付的 2px 换来的是可读性，一屏少看约 0 行（22 vs 20，一屏约少 1 行）
            // 相对字号提升带来的信息量增益可以接受，故取 22。
            // 详见 Metrics::row_height_for 里的同步取值 —— 两处必须一起改，否则
            // 点击命中区（用 row_h）和实际绘制行高（用 row_height_for）会错位。
            row_h: (22.0 * scale).round() as i32,
            pad: (8.0 * scale).round() as i32,
        }
    }

    pub fn row_height_for(&self, mode: zenfile_core::ViewMode) -> i32 {
        ((match mode {
            // 22px 与 Metrics::new 里 row_h 同源（详见那里的"为什么是 22 不是 20"）。
            // 这两个值必须保持一致：绘制用 row_height_for、鼠标命中用 row_h，
            // 一旦不等，点的行和显示的行就会差一行 —— 这是最难自查的偶发 bug。
            zenfile_core::ViewMode::Details | zenfile_core::ViewMode::List => 22.0,
            zenfile_core::ViewMode::MediumIcons => 88.0,
            zenfile_core::ViewMode::LargeIcons => 128.0,
        }) * self.scale)
            .round() as i32
    }
}

// ── 第80轮·改动3：字体与行高规格（本轮**只定义、不接线**）────────────────────
//
// ⚠️ 下面这些常量本轮**没有任何调用点**，是留给后续绘制任务（paint.rs）的。
// 出现 dead_code 警告是预期的，**不要**为了让警告消失而删掉它们或加
// #[allow(dead_code)] 掩盖 —— 它们是已确认的规格，等绘制任务接线。
//
// 字体族**刻意不跟 Tauri**：Tauri 用 Inter 是因为它是 Web 端默认无衬线体，
// 属于技术选型而非设计优势。本项目是原生 GDI 自绘，字体族保持
// `Microsoft YaHei UI`（中文正文）/ `Segoe MDL2 Assets`（图标）不变 ——
// Inter 没有中文字形，中文会掉字回退到系统字体，反而比原来更差。
// 本轮调整的只有字号与字重。

/// UI 正文字号（逻辑像素）。Tauri 模板为 16px，本项目取 14px：
/// 文件列表是密度敏感区，16px 配现有行高会让一屏少看约 8 行，
/// 而文件管理器的主要价值就是"一屏看全"。14px 是可读性与密度的折中。
pub const UI_FONT_PX: i32 = 14;

/// 表头字号。比正文小 1px —— 表头是分区标签，与具体文件名争抢注意力没有意义。
pub const HEADER_FONT_PX: i32 = 13;

/// 字重。Tauri 模板与 shadcn 都用 500，但中文笔画密，全 500 会糊成一团，
/// 故正文 400、标题 500：只在需要强调层级的地方加重，正文保持纤细。
pub const FONT_WEIGHT_NORMAL: i32 = 400;
pub const FONT_WEIGHT_MEDIUM: i32 = 500;

/// 行高倍数。Tauri 模板的 line-height 是 1.5。
///
/// 用途（下一轮接线）：单行文本绘制时，按 `字号 × LINE_H_RATIO` 计算行盒高度，
/// 并把文字在该行盒内**垂直居中**，而不是贴着行顶画。
/// 目前绘制代码用的是固定偏移量，改成按本常数居中后，多行文本的基线才会稳定。
///
/// 注意：14px × 1.5 = 21px，行盒比 Metrics::row_h(22) 略小 —— 这是有意的，
/// 行盒管"文字怎么排"，行高管"行与行之间隔多少"，两者不必相等；
/// 相等反而会让文字顶满整行、失去行间的呼吸感。
pub const LINE_H_RATIO: f32 = 1.5;
