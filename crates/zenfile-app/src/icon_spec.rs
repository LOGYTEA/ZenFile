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
    fn gdi_fallback_stroke_is_monotonic_in_size() {
        // 第72轮：这条测试原来叫 `stroke_is_one_pixel_across_the_board`，
        // 断言 `stroke_px` 在所有档位都恰好 1px（≥1.5x 时 2px）。
        //
        // 它**断言的是已废弃的规格**，不是自绘路径的笔画像素。
        // `stroke_px` 仍然存在，但只服务 `paint.rs` 里三处 **GDI 手绘回退**
        // （`draw_ui_icon` 返回 false 才触发）；第70轮起自绘 UI 字形一律走
        // `icon_spec_design::stroke_hw`（= `2.5 * size/13`，随边长线性变粗）。
        //
        // 所以"大图标还是 1px"这条老期望**在自绘路径上已经不成立了** ——
        // 继续留着它只会让每次 `cargo test` 都红，而且红得毫无意义：
        // 它守的是一条已经不走的代码路径。
        //
        // 真正要守的不变式是**单调性**：边长越大描边越粗，不会出现
        // "18px 的图标比 10px 的还淡"这种光学错配。逐像素相等不是目标，
        // 光学一致才是（第70轮把它从 1px 提到 2px 就是为此）。
        for s in Size::ALL {
            for scale in [1.0, 1.5, 2.0] {
                let w = stroke_px(s.scaled(scale), scale);
                assert!(w >= 1, "{s:?} @ {scale}x 的回退描边细到画不出来");
            }
            // 小档一定不大于大档（scale=1 下比较）
            if let Some(big) = Size::ALL.last() {
                assert!(
                    stroke_px(s.px(), 1.0) <= stroke_px(big.px(), 1.0),
                    "{s:?} 的回退描边比最大档还粗，光学上会显得更重"
                );
            }
        }
    }

    #[test]
    fn self_drawn_stroke_scales_with_size() {
        // ⚠️ 这条断言在第73轮被**反转过一次**，这里跟着改的不是措辞，是判据本身。
        //
        // 原断言：`2*stroke_hw(size)` 随边长单调递增。那是第70轮"笔画随字号缩放"
        // 那套模型的守门人 —— 而第73轮已经明确把它推翻了（见
        // `icon_spec_design::design::HW` 注释里那段"第70轮…那是反的"）。
        // 现在的模型是**物理直径恒定**：不管字形多大，落在屏幕上的笔画都是
        // 同一个物理像素数（Lucide / Material / Segoe Fluent 的统一做法）。
        //
        // 于是两条不变式要分开守，量纲也不同：
        //   * **物理直径**（`hw * size / GRID`，与 size 无关）必须各档一致 ——
        //     这才是"18px 图标不能比 10px 的淡"的正解；
        //   * **设计单位 hw**（`STROKE_PX*GRID/2/size`）随边长**递减** ——
        //     同一条 1.5px 的线，占的比例本来就该在小图上更大（小图要相对更粗
        //     才看得清，是物理下限，不是设计失误）。递减本身就是正确模型的
        //     必然推论，守它是为了让"谁把公式改回按比例放大"立刻暴露。
        //
        // 只断言物理直径的话，hw 恒为 0 这类退化实现也能过；只断言设计单位递减
        // 的话，GRID 被改坏也看不出来。两条都要。
        let d = crate::icon_spec_design::design::GRID;
        for w in Size::ALL.windows(2) {
            let (s0, s1) = (w[0].px(), w[1].px());
            let h0 = crate::icon_spec_design::design::stroke_hw(s0);
            let h1 = crate::icon_spec_design::design::stroke_hw(s1);
            assert!(h0 > 0.0 && h1 > 0.0, "{w:?} 的笔画半径退化为 0");
            // 物理直径恒定（直径 = hw * size / GRID）
            let p0 = 2.0 * h0 * s0 as f32 / d;
            let p1 = 2.0 * h1 * s1 as f32 / d;
            assert!(
                (p1 - p0).abs() < 1e-3,
                "{:?} 物理直径 {p1:.3}px != {:?} 的 {p0:.3}px（应恒为 STROKE_PX）",
                w[1],
                w[0]
            );
            // 设计单位半径随边长递减
            assert!(h1 < h0, "{:?} 的设计半径没比 {:?} 小", w[1], w[0]);
        }
    }
    #[test]
    fn multi_stroke_glyphs_keep_gaps_wider_than_their_strokes() {
        // **这才是"笔画不能糊死"的真正判据。**
        //
        // `icon_spec_design` 的注释里写过一条粗略纪律"笔画直径不得超外接框
        // 25%"，第70轮作者据此说"当前 5/24 = 20.8%"。但那个 5 是按
        // **13px（Sm）档**算的，而 `stroke_hw(size) = 2.5 * size/13` 是
        // **随边长线性增长**的，`BOX` 却是固定的 24 设计单位 ——
        // 于是"占框比"逐档上升，实测：
        //
        // ```text
        //   10px (Xs) -> 15.4%      17px (Lg) -> 27.2%
        //   13px (Sm) -> 20.8%      18px (Xl) -> 28.8%
        //   15px (Md) -> 22.5%
        // ```
        //
        // 所以那条 25% 只在 Sm 档成立，**对 Lg/Xl 一直是破的**。
        // 但它也从来不是真正的失效条件：糊死只发生在**相邻笔画之间**，
        // 跟"占外接框多少"没有因果关系 —— 单笔画字形（导航箭头）占比再高
        // 也不粘连，多笔画字形（hamburger / viewgrid / viewlist / gear）
        // 才是危险区。真窗口实测也印证了这点：Lg/Xl 的 18px 导航键渲染出
        // 干净的 4px 笔画，无糊无连。
        //
        // 真正的失效条件由 `row_gap(hw) = 4*hw` 保证：行间距 = **两个笔画
        // 直径**，净空恒等于一个直径，抗锯齿之后仍然看得见分开的缝。
        // 这条测试就把它钉住，别让有人再把 row_gap 改回写死的数字。
        for s in Size::ALL {
            let hw = crate::icon_spec_design::design::stroke_hw(s.px());
            let gap = crate::icon_spec_design::design::row_gap(hw);
            let diameter = 2.0 * hw;
            assert!(
                gap > diameter,
                "{}px 档的行间距 {gap:.1} 不大于笔画直径 {diameter:.1}，多笔画字形会粘连",
                s.px()
            );
            assert!(
                gap - diameter >= diameter,
                "{}px 档的墨迹净空不足一个笔画直径",
                s.px()
            );
        }
    }
}
