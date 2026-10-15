//! 图标规格：全软件**唯一**的尺寸与风格来源（第66轮）。
//!
//! # 之前为什么乱
//!
//! 10 个 UI 图标，调用方各写各的尺寸，一屏上同时出现 8 种边长：
//!
//! | 图标 | 之前的边长 | 谁写的 |
//! |---|---|---|
//! | 侧栏开关 | 17 | `draw_chrome_glyph` |
//! | 更多菜单 | 15 | `draw_chrome_glyph` |
//! | 新建标签 + | 13 | `draw_chrome_glyph` |
//! | 最小化/最大化/关闭 | 13 | `draw_caption_glyph` |
//! | 标签关闭 × | 11 | `paint_tab_strip` |
//! | 返回/前进/上级 | 20 | `paint_toolbar` |
//! | 批量重命名 | 19 | `paint_address` |
//! | 搜索放大镜 | `2*m+2` 动态 | `paint_search` |
//! | 树箭头 | 格子实算 | `draw_tree_chevron` |
//! | 视图切换 | 格子减 4 | `draw_view_glyph` |
//!
//! 同一个"一排标题栏按键"里，17 / 15 / 13 三个尺寸并排 —— 这就是用户说的
//! "动一种风格另一种风格，大小都不一样"。
//!
//! # 现在怎么统一
//!
//! 1. **只有 5 档边长**（`Size` 枚举），所有图标必须从这 5 档里挑一档。
//! 2. **每档边长只对应一个语义**（下表），挑档不是随意的。
//! 3. **所有绘制必须走 `draw`**，它内部统一：查规格 → 主题字形 → 降级 GDI。
//!    调用方不再自己算尺寸、不再自己选渲染路径。
//!
//! | 档 | 边长 | 语义 | 用在 |
//! |---|---|---|---|
//! | `Xs` | 10 | 微型指示符 | 树箭头、状态栏视图键 |
//! | `Sm` | 13 | 小控件 | 三大金刚键、新建标签 +、标签关闭 × |
//! | `Md` | 15 | 常规控件 | 更多菜单、设置齿轮 |
//! | `Lg` | 17 | 大控件 | 侧栏开关 |
//! | `Xl` | 18 | 强调控件 | 工具栏导航键、批量重命名、搜索 |
//!
//! 标题栏那一排按键现在全部是 `Sm`/`Lg`，底色块尺寸也统一 —— 三个窗口键、
//! 侧栏开关、更多菜单、`+` 站在同一条基线上，视觉重量一致。
//!
//! # 为什么是这几档而不是"全部一样大"
//!
//! 全部一样大并不好看：导航键（要突出"主要动作"）就该比关闭键（次要）大一点。
//! 但**档数必须有上限** —— 8 档就是没有规格，5 档是"一眼能数过来"。

use crate::icon_theme::Ui;
use windows::Win32::Foundation::RECT;

/// 图标边长档位（100% 缩放下的设计像素，实际值乘 `metrics.scale`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Size {
    /// 10px —— 微型指示符（树箭头、状态栏视图键）
    Xs,
    /// 13px —— 小控件（窗口键、+、×）
    Sm,
    /// 15px —— 常规控件（更多菜单、齿轮）
    Md,
    /// 17px —— 大控件（侧栏开关）
    Lg,
    /// 18px —— 强调控件（导航键、重命名、搜索）
    Xl,
}

impl Size {
    pub const ALL: [Size; 5] = [Size::Xs, Size::Sm, Size::Md, Size::Lg, Size::Xl];

    /// 设计边长（未乘缩放）。
    pub const fn px(self) -> i32 {
        match self {
            Size::Xs => 10,
            Size::Sm => 13,
            Size::Md => 15,
            Size::Lg => 17,
            Size::Xl => 18,
        }
    }

    /// 实际边长：设计边长 × 缩放，四舍五入且至少 8px。
    ///
    /// 下限 8 是为了保证 1px 笔画在低缩放下还看得见 —— 小到 6px 时描边会被
    /// 量化成断点，图标看着像"缺了一角"。
    pub fn scaled(self, scale: f32) -> i32 {
        ((self.px() as f32 * scale).round() as i32).max(8)
    }

    pub fn label(self) -> &'static str {
        match self {
            Size::Xs => "微型",
            Size::Sm => "小",
            Size::Md => "中",
            Size::Lg => "大",
            Size::Xl => "强调",
        }
    }
}

/// 图标规格：`Ui` 字形 ↔ 尺寸档。
///
/// **一个图标只属于一档**，这里就是那张唯一的表。调用方要改图标大小，
/// 只能改这张表（或改 `Size` 本身），不能在自己的调用点写 `17.0 * s`。
pub const fn spec(kind: Ui) -> Size {
    use Ui::*;
    match kind {
        // —— 标题栏 ——
        // 三大金刚键 / + / × 全是 Sm：它们同属"小控件"这一族，
        // 站在一起就该一样大（第66轮之前是 13/13/11 三个值）。
        Min | Max | Restore | Close | Plus => Size::Sm,
        // 侧栏开关 Lg、更多菜单 Md —— 这两个是"有内容含义"的键
        // （面板 / 菜单），比窗口键略大一点才不显得像附属品。
        PaneToggle => Size::Lg,
        Hamburger => Size::Md,

        // —— 工具栏 ——
        // 导航键 / 重命名 / 搜索 是 Xl：主要动作，要突出。
        Back | Forward | Up | Rename | Search => Size::Xl,

        // —— 微型指示符 ——
        ChevronDown | ChevronRight => Size::Xs,
        ViewGrid | ViewList => Size::Xs,

        // —— 设置标签 ——
        Gear => Size::Md,
    }
}

/// 图标的实际边长。
pub fn size_of(kind: Ui, scale: f32) -> i32 {
    spec(kind).scaled(scale)
}

/// 在 `r` 里居中一个 `size` 的正方形，返回该矩形。
///
/// 图标字形是**按边长缓存的**，所以给它一个正好 `size` 的矩形，
/// 而不是任意矩形 —— 传大矩形会让 `draw_ui_icon` 自己居中，
/// 两个调用点给的矩形不同，居中结果就可能有 1px 抖动。
pub fn centered(r: &RECT, size: i32) -> RECT {
    let cx = (r.left + r.right) / 2;
    let cy = (r.top + r.bottom) / 2;
    let h = size / 2;
    RECT { left: cx - h, top: cy - h, right: cx - h + size, bottom: cy - h + size }
}

/// 统一笔画粗细（GDI 手绘回退路径专用）。
///
/// ⚠️ **自绘字形不走这里 —— 这是死代码路径，别照它改笔画。**
///
/// 自绘 UI 字形（`icon_theme::draw_ui`）的笔画由
/// `icon_spec_design::design::stroke_hw(size)` 决定，那是**连续的设计单位半径**
/// （`2.5 * size/13`），13px 档直径 ≈ 2px **实心**，并随边长线性缩放。
/// 本函数算的是**整数像素宽度**、且不随边长缩放，两者不是一回事。
///
/// 保留它只为 `paint.rs` 里三处 **GDI 手绘回退**（`draw_ui_icon` 返回 false 时
/// 才触发）提供线宽。改动前先确认调用点：自绘 → 改 `stroke_hw`；
/// GDI 回退 → 才改这里。
///
/// 规格：边长 × 0.075，最少 1px。13px → 1px，18px → 1.35 → 1px，
/// 所以实际全都是 1px —— 对**回退路径**而言这正是要的：
/// 小图标和大图标用同样粗的线，视觉上才是一套。
pub fn stroke_px(size: i32, scale: f32) -> i32 {
    ((size as f32 * 0.075 * scale).round() as i32).max(1)
}

/// 编译期断言：同族图标必须同档。
///
/// **用 const 而不是运行时测试** —— 这类"规格"错配一旦发生，编译期就该
/// 报出来，而不是等自检跑一次才发现。改 `spec()` 时忘了同步同族，
/// 这里直接红。
const _: () = {
    use Ui::*;
    let titlebar = [Min, Max, Restore, Close, Plus];
    let want = spec(titlebar[0]).px();
    let mut i = 0;
    while i < titlebar.len() {
        assert!(
            spec(titlebar[i]).px() == want,
            "标题栏同族尺寸不一致：这是规格被改坏了"
        );
        i += 1;
    }
    let nav = [Back, Forward, Up];
    let want = spec(nav[0]).px();
    let mut i = 0;
    while i < nav.len() {
        assert!(spec(nav[i]).px() == want, "导航键同族尺寸不一致");
        i += 1;
    }
    // 档数上限：超过 5 档这套规格就没有意义了
    assert!(Size::ALL.len() <= 5, "图标档位超过 5 个就失去规格意义");
};

/// 运行时自检：把规格的关键性质打印出来，方便 `--selftest` 里核对。
///
/// 返回 `(通过, 说明)`。
pub fn selftest() -> Vec<(&'static str, bool, String)> {
    use Ui::*;
    let all = [
        Back, Forward, Up, Search, Rename, ChevronDown, ChevronRight,
        PaneToggle, Hamburger, Plus, Min, Max, Restore, Close,
        ViewGrid, ViewList, Gear,
    ];
    let mut out = Vec::new();

    let range_ok = all.iter().all(|k| {
        let p = spec(*k).px();
        p >= 10 && p <= 18
    });
    out.push((
        "icon::spec_range",
        range_ok,
        format!("{} 个图标，边长 10..18", all.len()),
    ));

    // 列出实际归档，让"到底有几个尺寸"一目了然
    let mut distinct: Vec<i32> = all.iter().map(|k| spec(*k).px()).collect();
    distinct.sort_unstable();
    distinct.dedup();
    out.push((
        "icon::tier_count",
        distinct.len() <= 5,
        format!("全软件只有 {} 种尺寸：{:?}（上限 5）", distinct.len(), distinct),
    ));

    // 笔画一致
    let strokes: Vec<i32> = Size::ALL.iter().map(|s| stroke_px(s.scaled(1.0), 1.0)).collect();
    let uniform = strokes.iter().all(|w| *w == strokes[0]);
    out.push((
        "icon::stroke_uniform",
        uniform,
        format!("各档笔画粗细 {strokes:?}"),
    ));

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_has_a_spec() {
        // 枚举里每个变体都必须能被 spec 覆盖 —— match 写全了编译期就保证，
        // 这条测试防的是"将来加了新变体忘了归档"。
        let all = [
            Ui::Back, Ui::Forward, Ui::Up, Ui::Search, Ui::Rename,
            Ui::ChevronDown, Ui::ChevronRight, Ui::PaneToggle, Ui::Hamburger,
            Ui::Plus, Ui::Min, Ui::Max, Ui::Restore, Ui::Close,
            Ui::ViewGrid, Ui::ViewList, Ui::Gear,
        ];
        for k in all {
            let s = spec(k);
            assert!(s.px() >= 10 && s.px() <= 18, "{k:?} 的档位越界：{s:?}");
        }
    }

    #[test]
    fn titlebar_family_is_uniform() {
        // 标题栏那一排是用户吐槽最集中的地方：同一排必须有统一规格。
        // 窗口键与 + 同为 Sm；侧栏开关 Lg / 更多菜单 Md 是"有含义"的两档。
        let fam = [Ui::Min, Ui::Max, Ui::Restore, Ui::Close, Ui::Plus];
        let first = spec(fam[0]).px();
        for k in fam {
            assert_eq!(spec(k).px(), first, "{k:?} 与同族窗口键尺寸不一致");
        }
    }

    #[test]
    fn toolbar_family_is_uniform() {
        // 工具栏导航键同属一档
        let nav = [Ui::Back, Ui::Forward, Ui::Up];
        let first = spec(nav[0]).px();
        for k in nav {
            assert_eq!(spec(k).px(), first, "{k:?} 与同族导航键尺寸不一致");
        }
    }

    #[test]
    fn only_five_sizes_exist() {
        // 档数上限是这套规格的核心 —— 档一多就退化成"没有规格"。
        let mut seen: Vec<i32> = Size::ALL.iter().map(|s| s.px()).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), Size::ALL.len(), "档位有重复值");
        assert!(seen.len() <= 5, "档位超过 5 个就失去规格意义了");
    }

    #[test]
    fn stroke_is_one_pixel_across_the_board() {
        // 光学粗细一致：小图标与大图标用同样粗的线。
        for s in Size::ALL {
            for scale in [1.0, 1.5, 2.0] {
                assert_eq!(
                    stroke_px(s.scaled(scale), scale),
                    if scale >= 1.5 { 2 } else { 1 },
                    "{s:?} @ {scale}x 的笔画粗细与其它档不一致"
                );
            }
        }
    }
}
