//! 内容区几何：**一处算，三处用**。
//!
//! 1.8.3 之前，"内容区到底多大"分散在四个函数里各算一遍，靠注释约定"必须给出同一个
//! 答案"：`content_size_of`（客户区扣顶栏）、`render_box`（渲染盒扣滚动条再乘 zoom）、
//! `view_size`（量已有窗口的 `frame().content`）、`frame`（绘制/命中）。任何一处少扣
//! 一条滚动条，就变成"位图比内容区宽 12 px → 上屏走 `StretchDIBits` → 每帧重采样 +
//! 右边被压住"。现场日志就是这么来的：`STRETCH 位图 1044x1477 != 内容区 1020x1479`。
//!
//! 收敛后只有两条规则：
//! 1. **内容区 = 顶栏以下 − 目录栏（够宽才有）− 滚动条（有才扣）**；
//! 2. **渲染盒 = 内容区 × zoom**（四舍五入，夹在 1..=16384）。
//!
//! 三条路都走 [`Layout`]：绘制/命中 `from_client`、渲染请求 `from_below`（只给宽高）、
//! 开窗前推尺寸 `from_window_size`。状态（有没有目录、要不要滚动条）由调用方从 `State`
//! 取好再传进来 —— 本模块**不碰任何全局**，所以能在普通单测里跑（含模拟 192 DPI）。
//!
//! ## 有意没并进来的几处（都带"为什么"）
//!
//! - **`window_size_for`（开窗尺寸）**：它算的是"窗口该多大"，输入是工作区、配置里的
//!   高度比例、图片缩放百分比、尺寸记忆、素材宽高比，输出**含顶栏的整窗矩形**。它不是
//!   "内容区几何"，硬并进来会把 5 个无关输入塞进 `Layout`。这里只提供它下游要用的
//!   [`reserve_for_window`]（整窗 = 内容 + reserve），语义不变。
//! - **`set_zoom_to` 里的 `ch + window::chrome_height()`**：同一条"内容 → 整窗"的加法，
//!   但基准尺寸 `base` 只对"按内容开窗"的格式存在，那里拿不到 `natural`；改成调
//!   [`reserve_for_window`] 需要多问一次状态，收益为零、风险不为零，留原样。
//! - **`rows_in` / `window::paint_list` 的行度量**：那是"一行多高、一屏几行"，属于文字
//!   排版参数，不是内容区矩形。两处共用的是 `window::line_height()`，已经是一份真源。
//! - **预热的渲染盒**：它和上屏走**同一套输入**（`window_size_for` 的整窗尺寸 +
//!   [`reserve_for_window`] + `Bar::Auto`），差别只在"窗口还没开"。1.8.3 之前预热
//!   漏传了 `reserve`（差一条 `chrome_height`），导致缓存键永远对不上、预热白做；
//!   现在两边都经 `app::render_box(…, reserve, …)` → 本模块，逐字一致。

use qlr_core::format;
use qlr_platform::window;
use windows::Win32::Foundation::RECT;

/// 目录栏要求的最小宽度（像素，历史阈值）。
///
/// 窗口窄于它就不显示目录：正文被挤到 300 px 还不如没有目录。这个 520 是**物理像素**
/// 字面量（1.8.2 之前的老值，没有走 DPI 换算），保持原样以免改变视觉。
const TOC_MIN_WIDTH: i32 = 520;

/// 这一类内容在内容区里会不会占掉一条竖向滚动条。
///
/// 只有 PDF 是"位图 + 滚动条"同时存在的：图片/SVG/视频没有条，文本没有位图。
/// 单页 PDF 放得下，不画条（`window::ScrollInfo::new` 的规则：`total > visible` 才有），
/// 这也是唯一需要看页数的地方。
pub fn reserves_scrollbar(kind: format::Kind, pages: Option<usize>) -> bool {
    kind == format::Kind::Pdf && pages.is_none_or(|n| n > 1)
}

/// 滚动条这条信息**从哪来**。
///
/// - `Auto`：按 [`reserves_scrollbar`]（kind + 页数）推 —— 窗口还没开、只能靠规则的时候用
///   （开窗前算渲染盒、预热）。
/// - `Yes` / `No`：调用方已经知道答案（窗口开着，直接看 `scroll_info()` 有没有）——
///   绘制与命中走这条，因为文本的滚动条取决于"行数放不放得下"，不是 kind 能推的。
///
/// 两种来源必须给出同一个答案，否则就是"位图和内容区差一条滚动条"；`Auto` 存在的意义
/// 就是让没有窗口的那条路也能算出和绘制一致的宽度。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bar {
    Auto,
    Yes,
    No,
}

impl Bar {
    /// 这一帧到底画不画滚动条。
    fn shown(self, kind: format::Kind, pages: Option<usize>) -> bool {
        match self {
            Bar::Auto => reserves_scrollbar(kind, pages),
            Bar::Yes => true,
            Bar::No => false,
        }
    }

    /// 由"有没有滚动条信息"翻译成显式答案（绘制/命中的入口用）。
    pub fn from_visible(visible: bool) -> Self {
        if visible { Bar::Yes } else { Bar::No }
    }
}

/// 一次布局的输入。**只有数据，没有全局状态**：状态由调用方取好再传进来。
#[derive(Clone, Copy, Debug)]
pub struct Opts {
    /// 内容类型（只影响 `Bar::Auto` 的推导）。
    pub kind: format::Kind,
    /// 总页数（PDF 才有；`None` 表示没问到）。
    pub pages: Option<usize>,
    /// 缩放倍数（0.25–8）。
    pub zoom: f32,
    /// 这次有没有目录栏（Markdown 且解析出了标题）。
    pub toc: bool,
    /// 滚动条从哪来。
    pub bar: Bar,
    /// 底部工作表标签栏（1.9.2，只有表格预览才有）：内容区要为它让出一条高度。
    pub tabs: bool,
}

/// 算好的几何。绘制、命中、渲染请求、开窗前推尺寸都只读它。
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    /// 扣掉顶栏后的区域（目录栏 / 滚动条都从它里面切）。
    pub below_chrome: RECT,
    /// 真正排版/渲染内容的区域（已扣目录栏、已扣滚动条）。
    pub content: RECT,
    /// 目录栏（Markdown 且窗口够宽才有）。
    pub toc: Option<RECT>,
    /// 滚动条（内容放不下才有）。
    pub bar: Option<RECT>,
    /// 底部工作表标签栏（1.9.2，表格预览才有）。
    pub tabs: Option<RECT>,
    /// 渲染盒：`content × zoom`。上屏时位图尺寸必须等于它，否则每帧拉伸。
    pub render_box: (u32, u32),
}

/// 窗口已经开着：从客户区算出全部几何（绘制 / 命中 / `view_size`）。
pub fn from_client(client: RECT, opts: Opts) -> Layout {
    from_below(window::chrome_rects(client).content, opts)
}

/// 窗口还没开：只知道整窗宽高与"顶栏留多少"（`show_preview`、预热）。
pub fn from_window_size(width: i32, height: i32, reserve: i32, opts: Opts) -> Layout {
    from_below(
        RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: (height - reserve).max(1),
        },
        opts,
    )
}

/// 核心：给定"顶栏以下的整块区域"，切出目录栏、滚动条，算出渲染盒。
///
/// 切法（顺序有意义，和历史一致）：先切目录栏（从左边），再切滚动条（从右边）。
pub fn from_below(below: RECT, opts: Opts) -> Layout {
    let mut content = below;
    // 窗口太窄就不显示目录：正文被挤到 300 px 还不如没有目录。
    let want_toc = opts.toc && (below.right - below.left) > TOC_MIN_WIDTH;
    if want_toc {
        content.left += window::toc_width();
    }
    let toc = want_toc.then(|| window::toc_rect(below));
    // 底部标签栏（1.9.2）：从下边切一条，再切目录/滚动条——顺序与"可见顺序"一致。
    let tabs = if opts.tabs {
        let h = window::sheet_tabs_height();
        let top = (content.bottom - h).max(content.top);
        content.bottom = top;
        Some(RECT {
            left: below.left,
            top,
            right: below.right,
            bottom: below.bottom,
        })
    } else {
        None
    };
    let bar = if opts.bar.shown(opts.kind, opts.pages) {
        content.right -= window::scrollbar_w();
        Some(window::scrollbar_rect(below))
    } else {
        None
    };
    let (w, h) = size_of(content);
    Layout {
        below_chrome: below,
        content,
        toc,
        bar,
        tabs,
        render_box: (scaled(w, opts.zoom), scaled(h, opts.zoom)),
    }
}

/// 客户区 → 内容区尺寸（扣顶栏，下限 1）。
///
/// 拖边结束后的 `base`、`request_render` 的口径都来自这里（1.0.9）。
/// 注意：这是"内容区"，**没有**扣目录栏/滚动条；要渲染盒请用 [`Layout::render_box`]。
pub fn content_size_of(client: RECT) -> (u32, u32) {
    (
        (client.right - client.left).max(1) as u32,
        ((client.bottom - client.top) - window::chrome_height()).max(1) as u32,
    )
}

/// 整窗 = 内容 + `reserve`。只有"按内容开窗"的格式才把顶栏算在内容之外。
///
/// 历史原因（1.0.9）：文本/Markdown 没有原始尺寸，窗口按阅读宽度开，顶栏算在内容里面，
/// 所以它们的 `reserve` 是 0；图片/PDF/视频/Office 按素材开窗，顶栏必须额外留出来。
pub fn reserve_for_window(natural: Option<(u32, u32)>) -> i32 {
    if natural.is_some() {
        window::chrome_height()
    } else {
        0
    }
}

/// 内容尺寸 × zoom（四舍五入，夹在 1..=16384）。
///
/// 16 384 是硬上限：8× 的 2048 图就顶到了，再大 pdfium / 图片解码会把内存吃爆。
pub fn scaled(value: u32, zoom: f32) -> u32 {
    ((value as f32 * zoom).round() as i64).clamp(1, 16_384) as u32
}

fn size_of(rect: RECT) -> (u32, u32) {
    (
        (rect.right - rect.left).max(1) as u32,
        (rect.bottom - rect.top).max(1) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rc(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    fn opts(kind: format::Kind, pages: Option<usize>, zoom: f32, toc: bool, bar: Bar) -> Opts {
        Opts {
            kind,
            pages,
            zoom,
            toc,
            bar,
            tabs: false,
        }
    }

    /// 1.8.3 的现场 bug：位图按全宽渲染、上屏却只铺在"扣掉滚动条"的内容区里。
    ///
    /// 这是本次重构真正要守住的不变量 —— `Content` 与 `render_box` 必须同宽，
    /// 这样"上屏走 `StretchDIBits`"就只发生在 zoom ≠ 1 的预览态。
    #[test]
    fn scrollbar_keeps_render_box_as_wide_as_content() {
        let client = rc(0, 0, 1020, 1479 + window::chrome_height());
        // 文本（行数放不下 → 有滚动条）与多页 PDF（按规则推导 → 有滚动条）两条路。
        for o in [
            opts(
                format::Kind::Text,
                None,
                1.0,
                false,
                Bar::from_visible(true),
            ),
            opts(format::Kind::Pdf, Some(8), 1.0, false, Bar::Auto),
        ] {
            let l = from_client(client, o);
            assert!(l.bar.is_some(), "{:?}：应该有滚动条", o.kind);
            assert_eq!(
                l.render_box.0,
                size_of(l.content).0,
                "{:?}：渲染盒宽 {} 与内容区宽 {} 不一致（会每帧拉伸）",
                o.kind,
                l.render_box.0,
                size_of(l.content).0
            );
            assert_eq!(
                l.render_box,
                size_of(l.content),
                "{:?}：zoom=1 时渲染盒应等于内容区",
                o.kind
            );
        }
    }

    /// 单页 PDF 放得下 → 不画滚动条 → 内容区就是"顶栏以下"的全部。
    #[test]
    fn single_page_pdf_fills_below_chrome() {
        let client = rc(0, 0, 800, 900);
        let l = from_client(
            client,
            opts(format::Kind::Pdf, Some(1), 1.0, false, Bar::Auto),
        );
        assert!(l.bar.is_none(), "单页 PDF 不该有滚动条");
        assert_eq!(
            (
                l.content.left,
                l.content.top,
                l.content.right,
                l.content.bottom
            ),
            (
                l.below_chrome.left,
                l.below_chrome.top,
                l.below_chrome.right,
                l.below_chrome.bottom
            ),
            "没有目录也没有滚动条时，内容区 == 顶栏以下"
        );
        assert!(l.toc.is_none());
        // 页数没问到（None）时按"多页"处理：宁可多扣一条，也别让页面被压住。
        assert!(reserves_scrollbar(format::Kind::Pdf, None));
        assert!(!reserves_scrollbar(format::Kind::Text, Some(80)));
    }

    /// Markdown 有目录 → 正文从目录右边开始，目录栏本身贴左。
    #[test]
    fn toc_shifts_content_right_of_the_panel() {
        let client = rc(0, 0, 1000, 700);
        let l = from_client(
            client,
            opts(format::Kind::Markdown, None, 1.0, true, Bar::Yes),
        );
        let toc = l.toc.expect("够宽就该有目录");
        assert_eq!(l.content.left, l.below_chrome.left + window::toc_width());
        assert_eq!(toc.right, l.below_chrome.left + window::toc_width());
        assert_eq!(toc.right, l.content.left);
        // 滚动条在最右边，且不压目录。
        assert_eq!(
            l.content.right,
            l.below_chrome.right - window::scrollbar_w()
        );
        assert_eq!(l.bar.expect("有滚动条").left, l.content.right);
    }

    /// 窗口太窄（< 520 px）时不显示目录：正文优先。
    #[test]
    fn narrow_window_drops_the_toc() {
        let l = from_client(
            rc(0, 0, TOC_MIN_WIDTH, 700),
            opts(format::Kind::Markdown, None, 1.0, true, Bar::No),
        );
        assert!(l.toc.is_none(), "520 宽不放目录");
        assert_eq!(l.content.left, l.below_chrome.left);
    }

    /// zoom > 1 时渲染盒 = 内容区 × zoom，取整规则与历史一致（四舍五入 + 1..=16384）。
    #[test]
    fn render_box_scales_content_by_zoom() {
        let client = rc(0, 0, 1000, 800);
        let o = opts(format::Kind::Image, None, 1.0, false, Bar::No);
        let base = from_client(client, o);
        let (w, h) = size_of(base.content);
        for zoom in [0.25_f32, 1.0, 1.5, 2.0, 3.7, 8.0] {
            let l = from_client(client, Opts { zoom, ..o });
            assert_eq!(
                l.render_box,
                (
                    (w as f32 * zoom).round() as u32,
                    (h as f32 * zoom).round() as u32
                ),
                "zoom={zoom}"
            );
        }
        // 1.5× 的奇数值：1001 × 1.5 = 1501.5 → 四舍五入 1502（不是截断 1501）。
        assert_eq!(scaled(1001, 1.5), 1502);
        assert_eq!(scaled(1, 0.001), 1, "下限 1");
        assert_eq!(scaled(20_000, 8.0), 16_384, "上限 16 384");
    }

    /// 96 DPI 与 192 DPI 下几何按比例走：顶栏/滚动条/目录栏都是 DIP × DPI。
    ///
    /// 这一条守的是 1.8.3 的"DPI 全量缩放"：内容区被扣掉的那几条边必须跟着 DPI 变，
    /// 否则 200% 下滚动条只有一半宽、目录栏只占一半。
    #[test]
    fn geometry_follows_dpi() {
        use qlr_platform::dpi;
        let client = rc(0, 0, 1000, 800);
        let o = opts(format::Kind::Pdf, Some(8), 1.0, false, Bar::Auto);

        dpi::seed_thread(96);
        let l96 = from_client(client, o);
        let (chrome96, bar96) = (window::chrome_height(), window::scrollbar_w());

        dpi::seed_thread(192);
        let l192 = from_client(client, o);
        let (chrome192, bar192) = (window::chrome_height(), window::scrollbar_w());
        dpi::seed_thread(96); // 别把 192 留给同线程后面的其它测试

        assert_eq!(chrome192, chrome96 * 2, "顶栏高度要按 DPI 翻倍");
        assert_eq!(bar192, bar96 * 2, "滚动条宽度要按 DPI 翻倍");
        assert_eq!(
            l96.below_chrome.bottom - l96.below_chrome.top,
            800 - chrome96
        );
        assert_eq!(
            l192.below_chrome.bottom - l192.below_chrome.top,
            800 - chrome192
        );
        assert_eq!(l96.content.right, 1000 - bar96);
        assert_eq!(l192.content.right, 1000 - bar192);
        // 内容区高度不受滚动条影响（只扣顶栏）；宽度也不受顶栏影响。
        assert_eq!(
            size_of(l96.content).1,
            size_of(l192.content).1 + chrome96 as u32
        );
    }

    /// 整窗尺寸入口：`from_window_size(w, h, reserve)` 与 `from_client` 在同一份数据上
    /// 必须给出同一个渲染盒（上屏路径与预热路径的唯一差别就是它俩）。
    #[test]
    fn window_size_entry_matches_client_entry() {
        let chrome = window::chrome_height();
        let (w, h) = (1020, 1449);
        let client = rc(0, 0, w, h + chrome);
        let o = opts(format::Kind::Pdf, Some(8), 1.0, false, Bar::Auto);
        assert_eq!(
            from_window_size(w, h + chrome, chrome, o).render_box,
            from_client(client, o).render_box
        );
        // `reserve_for_window`：按内容开窗的格式才额外留顶栏。
        assert_eq!(reserve_for_window(Some((800, 600))), chrome);
        assert_eq!(reserve_for_window(None), 0);
    }
}
