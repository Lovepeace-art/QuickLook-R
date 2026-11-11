//! 电子表格网格绘制：`qlr_core::sheet::Sheet` → 屏幕上的一张"能看懂的表格"。
//!
//! 为什么单独一个文件：表格绘制要画的东西（行列号表头、网格线、合并单元格、
//! 数字右对齐、超宽文本的溢出/裁剪规则）和文本/PDF 完全不同，塞进 `text.rs`
//! 只会让两边都变难读。
//!
//! 约束：
//! - 只画**可见区域**（滚动位置 + 内容矩形求交），20 万格的表也只画屏幕上那几百格；
//! - 字号/字体走 `TextStyle` 的字体缓存思路，不要每次 `CreateFontW`（一次重画几百格，
//!   一次创建就是几百个 GDI 对象，句柄会飙）；
//! - `DrawTextW` 不许传空切片（CONVENTIONS #1 会 0xC0000005）、不做字体回退（#130）。
//!
//! # 绘制顺序（1.9.0）
//!
//! ```text
//! 白底 → 顶栏 → 行列号表头
//!   → 四个区域依次：[普通区] → [冻结行] → [冻结列] → [冻结交叉]
//!       每个区域内：单元格底色 → 网格线 → 合并区擦白/底色 → 边框 → 文字
//! ```
//!
//! 冻结区**后画**就是为了处理"普通区滚到冻结区底下"：Excel 的冻结窗格语义是
//! 冻结部分永远压在上面，滚动内容从它底下穿过去（用区域裁剪实现，不靠算法绕）。
//!
//! # 字体：为什么不是"每个 `CellStyle` 一个 HFONT"
//!
//! 仓库的字体缓存（`text::FONTS`）按 `TextStyle` 索引，只有 14 个固定档位，且绘制
//! 路径上**禁止 `CreateFontW`**（几百格 = 几百个 GDI 句柄）。所以这里做一次
//! "语义 → 最接近档位"的近似：粗体按字号落 `Heading1/2/3`，非粗体一律 `Normal`
//! （斜体不画）。代价是非粗体的 14pt/9pt 看起来和 11pt 一样大——这是已知取舍，
//! 换字体档位之前不动 `text.rs` 的缓存结构。
//!
//! # 溢出 / `####` 规则（照 Excel）
//!
//! - 放得下：按 `h_align`/`v_align` 正常画；
//! - 放不下且"看起来像数字"：填 `####`（Excel 的语义是"这一格显示不了这个数"）；
//! - 放不下且左对齐：向右**溢出**，但只溢进"空格子"，遇到第一个有内容的格子
//!   （含合并区）就停；到 content 右边界由裁剪切断；
//! - 其余情况（居中/右对齐的非数字）：硬裁，**不加省略号**（Excel 也不加）。

use qlr_core::sheet::{Border, Cell, CellStyle, HAlign, Merge, Sheet, VAlign, line};
use windows::Win32::Graphics::Gdi::{
    DT_BOTTOM, DT_CALCRECT, DT_TOP, DT_WORDBREAK, IntersectClipRect, RestoreDC, SaveDC,
};

use super::*;

/// 日志 scope（AGENTS.md §7：scope 必须是模块级常量）。
const SCOPE: &str = "sheet";

/// 表头（行号列 / 列号行）的固定宽高，与 Excel 的观感一致（DIP，绘制时按 DPI 换算）。
pub const HEADER_W: i32 = 44;
pub const HEADER_H: i32 = 20;
/// 拖列宽/行高时，鼠标距分隔线多少像素内算"按在线上"（1.9.1）。
pub const DIVIDER_GRAB: i32 = 4;

/// 表头宽高（随 DPI 缩放）。给壳层的诊断日志用：拖列宽/行高"点了没反应"时，
/// 先看这个值，再看命中点是不是落进了表头条里。
pub fn sheet_header_size() -> (i32, i32) {
    let m = Metrics::current();
    (m.header_w, m.header_h)
}

/// 单元格内边距（DIP）。Excel 是 2–3 px，取 3。
const CELL_PADDING: i32 = 3;

// —— 配色 ——
//
// 注意 `COLORREF` 是 `0x00BBGGRR`（新坑 #142），所以这里的浅灰都刻意取
// 三通道相同的值（0x00D9D9D9 之类），换算写错也不会变色；只有单元格自带的前景/
// 底色才走 `rgb_to_colorref()` 显式转一次。

/// 表头底色（浅灰）。
const HEADER_BG_RGB: u32 = 0x00F2_F2F2;
/// 左上角"全选"格底色（比表头略深，Excel 也是这么区分的）。
const HEADER_CORNER_RGB: u32 = 0x00E4_E4E4;
/// 表头文字色。
const HEADER_FG_RGB: u32 = 0x0055_5555;
/// 表头单元格之间的分隔线 / 表头与内容之间的分界线。
const HEADER_SEP_RGB: u32 = 0x00B4_B4B4;
/// 网格线（浅灰 1 px）。
const GRID_RGB: u32 = 0x00D9_D9D9;
/// 单元格边框（深灰）：与网格线拉开层次，否则 MEDIUM 边框看起来和网格一样。
const BORDER_RGB: u32 = 0x0040_4040;

/// 超出屏幕的列宽/行高列（极端坏数据）最多算到 64 列/64 行：
/// 溢出和网格只需要覆盖一屏，多算只是白烧 CPU。
const MAX_SPAN: u32 = 64;

// ---------------------------------------------------------------------------
// DPI 与几何（纯逻辑，单测覆盖）
// ---------------------------------------------------------------------------

/// `i64` → `i32`（饱和，不 panic）。几何量来自 `Sheet`（`i64`），窗口坐标是 `i32`，
/// 坏数据（列宽写成 10 亿）不能在这里炸掉 release 的 `panic="abort"`。
fn px_i32(v: i64) -> i32 {
    if v > i64::from(i32::MAX) {
        i32::MAX
    } else if v < i64::from(i32::MIN) {
        i32::MIN
    } else {
        v as i32
    }
}

/// 按窗口 DPI 换算的表头/内边距/线宽。
#[derive(Clone, Copy)]
struct Metrics {
    header_w: i32,
    header_h: i32,
    pad: i32,
    /// 1 px 线宽（THIN / 网格）
    thin: i32,
    /// 2 px 线宽（MEDIUM）
    medium: i32,
}

impl Metrics {
    /// 当前线程 DPI 下的一套尺寸。`paint_sheet` 里 `dpi::enter()` 已经设好线程 DPI，
    /// 本文件所有几何换算都从这里取，**不要**各算各的（否则表头会和内容错位）。
    fn current() -> Self {
        Self {
            header_w: dpi::cpx(HEADER_W).max(1),
            header_h: dpi::cpx(HEADER_H).max(1),
            pad: dpi::cpx(CELL_PADDING).max(0),
            thin: dpi::cpx(1).max(1),
            medium: dpi::cpx(2).max(1),
        }
    }

    fn body_left(&self, content: RECT) -> i32 {
        content.left.saturating_add(self.header_w)
    }

    fn body_top(&self, content: RECT) -> i32 {
        content.top.saturating_add(self.header_h)
    }
}

/// 列号 → Excel 的字母名：0 → `A`、25 → `Z`、26 → `AA`、701 → `ZZ`、702 → `AAA`。
///
/// 为什么是 base-26 的**双射**而不是普通进制：Excel 没有"0 位"，`A` 既代表 1 也代表
/// 进位后的 0（`AA` = 26×1 + 1）。算法是"取余 → 减 1 → 除 26"，从低位往上拼。
fn column_label(col: u32) -> String {
    let mut letters: Vec<char> = Vec::with_capacity(3);
    let mut idx = col;
    loop {
        let rem = (idx % 26) as u8;
        letters.push(char::from(b'A' + rem));
        if idx < 26 {
            break;
        }
        idx = idx / 26 - 1;
    }
    letters.reverse();
    letters.into_iter().collect()
}

/// 可见列范围（含两端）。`skip` = 冻结列数：普通区不该重画冻结列。
fn visible_cols(
    sheet: &Sheet,
    sizes: &SheetSizes<'_>,
    scroll_x: i32,
    view_w: i32,
    skip: u32,
) -> (u32, u32) {
    if sheet.cols == 0 {
        return (0, 0);
    }
    let last_possible = sheet.cols.saturating_sub(1);
    let left = i64::from(scroll_x.max(0));
    let width = i64::from(view_w.max(0));
    let first = sizes.col_at(sheet, left).max(skip).min(last_possible);
    let right_edge = left.saturating_add(width).saturating_sub(1).max(left);
    let last = sizes.col_at(sheet, right_edge).clamp(first, last_possible);
    (first, last)
}

/// 可见行范围（含两端）。语义同 [`visible_cols`]。
fn visible_rows(
    sheet: &Sheet,
    sizes: &SheetSizes<'_>,
    scroll_y: i32,
    view_h: i32,
    skip: u32,
) -> (u32, u32) {
    if sheet.rows == 0 {
        return (0, 0);
    }
    let last_possible = sheet.rows.saturating_sub(1);
    let top = i64::from(scroll_y.max(0));
    let height = i64::from(view_h.max(0));
    let first = sizes.row_at(sheet, top).max(skip).min(last_possible);
    let bottom_edge = top.saturating_add(height).saturating_sub(1).max(top);
    let last = sizes.row_at(sheet, bottom_edge).clamp(first, last_possible);
    (first, last)
}

/// 覆盖了这一格的合并区（含左上角自己）。
fn merge_covering(merges: &[Merge], row: u32, col: u32) -> Option<&Merge> {
    merges.iter().find(|m| m.contains(row, col))
}

/// 左对齐的文本往右能溢出到哪一列（含）。
///
/// 只溢进"空格子"：遇到第一个有内容的格子、或者被合并区占住的格子就停。上限
/// [`MAX_SPAN`] 列——一屏也放不下更多，再多算就是白烧 CPU。
fn overflow_last_col(sheet: &Sheet, row: u32, col: u32) -> u32 {
    let mut last = col;
    let mut next = col.saturating_add(1);
    while next < sheet.cols && next.saturating_sub(col) <= MAX_SPAN {
        if sheet.cell(row, next).is_some() || sheet.merge_at(row, next).is_some() {
            break;
        }
        last = next;
        next = next.saturating_add(1);
    }
    last
}

/// "看起来像数字"（决定放不下时画 `####` 还是硬裁）。
///
/// 只看文本形状，不看数字格式——数字格式化在解析层已经定稿（`Cell::text` 是显示串），
/// 这里再猜规则码只会和解析层打架。允许：数字、千分位、小数、前导正负号、百分号、
/// 货币前缀、科学计数。
///
/// **日期/时间不算数字**（1.9.0 修）：预览窗是只读的，用户没法拉宽列，所以
/// `2026-10-01` 这种放不下时给"部分文本"比给 `####` 有用；而 Excel 里日期是数字
/// （`####`）——两者在此处故意不同，别当 bug 改回去。判据：`-` 只许出现在首位
/// （正负号），出现 `:`/`/` 一律当文本。
fn looks_numeric(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    let total = trimmed.chars().count();
    let mut digits = 0usize;
    for (i, ch) in trimmed.chars().enumerate() {
        match ch {
            '0'..='9' => digits += 1,
            // 货币符号只允许出现在第一个字符
            '$' | '¥' | '￥' | '€' | '£' => {
                if i != 0 {
                    return false;
                }
            }
            // 百分号只允许出现在最后一个字符
            '%' => {
                if i + 1 != total {
                    return false;
                }
            }
            // 负号只允许打头（`-3.2` 是数字，`2026-10-01` 是日期）
            '-' => {
                if i != 0 {
                    return false;
                }
            }
            ',' | '.' | '+' | ' ' | 'e' | 'E' => {}
            _ => return false,
        }
    }
    digits > 0
}

/// `0xRRGGBB` → `COLORREF`（`0x00BBGGRR`，新坑 #142）。
fn rgb_to_colorref(rgb: u32) -> u32 {
    let r = (rgb >> 16) & 0xFF;
    let g = (rgb >> 8) & 0xFF;
    let b = rgb & 0xFF;
    (b << 16) | (g << 8) | r
}

/// 两个矩形是否相交（退化矩形一律视为不相交）。
fn intersects(a: RECT, b: RECT) -> bool {
    a.right > a.left
        && a.bottom > a.top
        && b.right > b.left
        && b.bottom > b.top
        && a.left < b.right
        && b.left < a.right
        && a.top < b.bottom
        && b.top < a.bottom
}

/// 取交集（用于把壳层给的 content 夹进客户区，绝不允许画到窗口外）。
fn intersect_rect(a: RECT, b: RECT) -> RECT {
    RECT {
        left: a.left.max(b.left),
        top: a.top.max(b.top),
        right: a.right.min(b.right),
        bottom: a.bottom.min(b.bottom),
    }
}

/// 某个 `CellStyle` 落到字体缓存的哪一档（见模块文档"字体"一节）。
fn text_style_for(style: &CellStyle) -> TextStyle {
    if style.bold {
        let pt = style.size_pt();
        if pt >= 17.5 {
            TextStyle::Heading1
        } else if pt >= 14.5 {
            TextStyle::Heading2
        } else {
            TextStyle::Heading3
        }
    } else {
        TextStyle::Normal
    }
}

/// 单元格四条边（画边框时按权重取像素）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Top,
    Right,
    Bottom,
    Left,
}

/// 一条边的 1–2 个填充矩形：THIN 1 条、MEDIUM 1 条（更粗）、DOUBLE 2 条（中间留 1 px 缝）。
fn border_rects(
    box_: RECT,
    side: Side,
    weight: u8,
    thin: i32,
    medium: i32,
) -> (Option<RECT>, Option<RECT>) {
    if weight == line::NONE {
        return (None, None);
    }
    let t = thin.max(1);
    let thick = if weight >= line::MEDIUM {
        medium.max(t)
    } else {
        t
    };
    let gap = t;
    let a = match side {
        Side::Top => RECT {
            left: box_.left,
            top: box_.top,
            right: box_.right,
            bottom: box_.top.saturating_add(thick),
        },
        Side::Bottom => RECT {
            left: box_.left,
            top: box_.bottom.saturating_sub(thick),
            right: box_.right,
            bottom: box_.bottom,
        },
        Side::Left => RECT {
            left: box_.left,
            top: box_.top,
            right: box_.left.saturating_add(thick),
            bottom: box_.bottom,
        },
        Side::Right => RECT {
            left: box_.right.saturating_sub(thick),
            top: box_.top,
            right: box_.right,
            bottom: box_.bottom,
        },
    };
    if weight == line::DOUBLE {
        let b = match side {
            Side::Top => RECT {
                left: box_.left,
                top: box_.top.saturating_add(thick).saturating_add(gap),
                right: box_.right,
                bottom: box_
                    .top
                    .saturating_add(thick)
                    .saturating_add(gap)
                    .saturating_add(thick),
            },
            Side::Bottom => RECT {
                left: box_.left,
                top: box_
                    .bottom
                    .saturating_sub(thick)
                    .saturating_sub(gap)
                    .saturating_sub(thick),
                right: box_.right,
                bottom: box_.bottom.saturating_sub(thick).saturating_sub(gap),
            },
            Side::Left => RECT {
                left: box_.left.saturating_add(thick).saturating_add(gap),
                top: box_.top,
                right: box_
                    .left
                    .saturating_add(thick)
                    .saturating_add(gap)
                    .saturating_add(thick),
                bottom: box_.bottom,
            },
            Side::Right => RECT {
                left: box_
                    .right
                    .saturating_sub(thick)
                    .saturating_sub(gap)
                    .saturating_sub(thick),
                top: box_.top,
                right: box_.right.saturating_sub(thick).saturating_sub(gap),
                bottom: box_.bottom,
            },
        };
        (Some(a), Some(b))
    } else {
        (Some(a), None)
    }
}

// ---------------------------------------------------------------------------
// 离屏缓冲（双缓冲）
// ---------------------------------------------------------------------------
//
// `text.rs` 里已有一份 `PaintBuffer` + 线程内复用，但它是**模块私有**的；为了不改
// 别人的文件（本次写范围只有 `sheet.rs`），这里独立一份同样结构的缓冲。两边不会
// 同时开工：一个窗口一帧只走一条绘制通道，所以各持一块同尺寸位图不会互相打架。

struct SheetBuffer {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    w: i32,
    h: i32,
}

impl SheetBuffer {
    fn new(window_dc: HDC, rc: RECT) -> Option<Self> {
        unsafe {
            let dc = CreateCompatibleDC(Some(window_dc));
            if dc.0.is_null() {
                return None;
            }
            let w = (rc.right - rc.left).max(1);
            let h = (rc.bottom - rc.top).max(1);
            let bitmap = CreateCompatibleBitmap(window_dc, w, h);
            if bitmap.0.is_null() {
                let _ = DeleteDC(dc);
                return None;
            }
            let old = SelectObject(dc, bitmap.into());
            Some(Self {
                dc,
                bitmap,
                old,
                w,
                h,
            })
        }
    }
}

impl Drop for SheetBuffer {
    fn drop(&mut self) {
        unsafe {
            let _ = SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

thread_local! {
    /// 按尺寸复用的整窗离屏缓冲（滚动重画每秒几十次，不能每帧建/删一块整窗位图）。
    static SHEET_BUFFER: std::cell::RefCell<Option<SheetBuffer>> =
        const { std::cell::RefCell::new(None) };
}

/// 取一块可复用的离屏缓冲；建不起来返回 `None`（调用方退化成直画窗口 DC）。
fn acquire_sheet_buffer(window_dc: HDC, rc: RECT) -> Option<SheetBuffer> {
    let w = (rc.right - rc.left).max(1);
    let h = (rc.bottom - rc.top).max(1);
    SHEET_BUFFER.with(|slot| {
        let mut slot = slot.borrow_mut();
        match slot.take() {
            // 尺寸没变：直接复用（`take` 出来的值在被丢弃时已 `Drop` 释放旧缓冲）
            Some(buf) if buf.w == w && buf.h == h => Some(buf),
            _ => SheetBuffer::new(window_dc, rc),
        }
    })
}

/// 把缓冲放回缓存供下一次 `WM_PAINT` 复用（必须在 `EndPaint` 之前调）。
fn release_sheet_buffer(buf: Option<SheetBuffer>) {
    if let Some(buf) = buf {
        SHEET_BUFFER.with(|slot| *slot.borrow_mut() = Some(buf));
    }
}

// ---------------------------------------------------------------------------
// 异常数据检查（每张表只做一次）
// ---------------------------------------------------------------------------

thread_local! {
    /// 上一次已经检查过的形状 `(cols, rows, cells.len(), merges.len())`。
    /// 滚动重画每帧都调 `paint_sheet`，不缓存就会把日志刷满。
    static CHECKED_SHAPE: std::cell::Cell<(u32, u32, usize, usize)> =
        const { std::cell::Cell::new((u32::MAX, u32::MAX, usize::MAX, usize::MAX)) };
}

/// 对可疑数据告警一次（越界合并、行列尺寸数目不一致、`cells` 没排序）。
///
/// `cells` 没排序值得单独查：`Sheet::cell()` 是二分查找，乱序不会崩，但会**静默**
/// 查不到格子——现象是"整个预览是空的"，没有日志会很难查。
fn check_sheet_health(sheet: &Sheet) {
    let shape = (
        sheet.cols,
        sheet.rows,
        sheet.cells.len(),
        sheet.merges.len(),
    );
    let mut first_time = false;
    CHECKED_SHAPE.with(|slot| {
        if slot.get() != shape {
            slot.set(shape);
            first_time = true;
        }
    });
    if !first_time {
        return;
    }
    if sheet.col_widths.len() != sheet.cols as usize
        || sheet.row_heights.len() != sheet.rows as usize
    {
        qlr_core::log_warn!(
            SCOPE,
            "工作表几何不一致：{}/{} 列宽、{}/{} 行高，按较小者绘制",
            sheet.col_widths.len(),
            sheet.cols,
            sheet.row_heights.len(),
            sheet.rows
        );
    }
    let mut bad_merges = 0usize;
    for m in &sheet.merges {
        if m.rows == 0
            || m.cols == 0
            || m.row.saturating_add(m.rows) > sheet.rows
            || m.col.saturating_add(m.cols) > sheet.cols
        {
            bad_merges += 1;
        }
    }
    if bad_merges > 0 {
        qlr_core::log_warn!(SCOPE, "{bad_merges} 个越界/空合并区，按裁剪后的矩形绘制");
    }
    let mut sorted = true;
    let mut prev: Option<(u32, u32)> = None;
    for cell in &sheet.cells {
        if let Some(pair) = prev {
            if (cell.row, cell.col) < pair {
                sorted = false;
                break;
            }
        }
        prev = Some((cell.row, cell.col));
    }
    if !sorted {
        qlr_core::log_warn!(
            SCOPE,
            "单元格没有按 (row,col) 升序，二分查找会漏格（预览可能空白）"
        );
    }
}

// ---------------------------------------------------------------------------
// 可见窗口 → 绘制区域
// ---------------------------------------------------------------------------

/// 一个绘制区域：画哪些行列、用多少滚动偏移、裁剪到哪个矩形。
///
/// 为什么要拆区域而不是"画一遍再补冻结区"：冻结行列把内容切成 4 块，每块的
/// 滚动偏移不同（冻结方向偏移为 0），拆开之后"裁剪"这件事交给 GDI（`IntersectClipRect`），
/// 代码里不用写任何"这一格是否被冻结区盖住"的判断。
/// 视图内的行列尺寸调整（1.9.1）：拖表头分隔线把某列拉宽 / 某行拉高。
///
/// **只影响这次预览**，不写回文件（预览器不改用户文件是硬规矩）。存的是
/// **额外像素**而不是"新宽度"，这样文件自己改过的列宽仍然是基准，用户拖出来的
/// 只是在这个基准上加的偏移——换工作表/换文件时清空即可回到文件原样。
#[derive(Clone, Copy, Default)]
pub struct SheetSizes<'a> {
    pub col_extra: &'a [(u32, i32)],
    pub row_extra: &'a [(u32, i32)],
}

impl<'a> SheetSizes<'a> {
    /// 没有任何调整（常态）。
    pub const EMPTY: SheetSizes<'static> = SheetSizes {
        col_extra: &[],
        row_extra: &[],
    };

    fn extra(list: &[(u32, i32)], index: u32) -> i32 {
        list.iter()
            .find(|(i, _)| *i == index)
            .map(|(_, d)| *d)
            .unwrap_or(0)
    }

    /// 索引 `index` **之前**所有调整的累计（把"额外宽度"换成"坐标偏移"）。
    fn before(list: &[(u32, i32)], index: u32) -> i32 {
        list.iter()
            .filter(|(i, _)| *i < index)
            .map(|(_, d)| *d)
            .sum()
    }

    pub fn col_x(&self, sheet: &Sheet, col: u32) -> i64 {
        sheet
            .col_x(col)
            .saturating_add(i64::from(Self::before(self.col_extra, col)))
    }

    pub fn row_y(&self, sheet: &Sheet, row: u32) -> i64 {
        sheet
            .row_y(row)
            .saturating_add(i64::from(Self::before(self.row_extra, row)))
    }

    pub fn col_width(&self, sheet: &Sheet, col: u32) -> i32 {
        let base = sheet.col_x(col.saturating_add(1)) - sheet.col_x(col);
        px_i32(base)
            .saturating_add(Self::extra(self.col_extra, col))
            .max(0)
    }

    pub fn row_height(&self, sheet: &Sheet, row: u32) -> i32 {
        let base = sheet.row_y(row.saturating_add(1)) - sheet.row_y(row);
        px_i32(base)
            .saturating_add(Self::extra(self.row_extra, row))
            .max(0)
    }

    /// 内容坐标 → 列号（考虑调整；越界给最后一列）。
    pub fn col_at(&self, sheet: &Sheet, x: i64) -> u32 {
        // 先把 x 折回"文件坐标"：减去落在它左边的所有额外宽度，再二分。
        let mut base = x;
        for (col, delta) in self.col_extra {
            if sheet.col_x(*col) <= base {
                base = base.saturating_sub(i64::from(*delta));
            } else {
                break;
            }
        }
        sheet.col_at(base)
    }

    /// 内容坐标 → 行号（考虑调整；越界给最后一行）。
    pub fn row_at(&self, sheet: &Sheet, y: i64) -> u32 {
        let mut base = y;
        for (row, delta) in self.row_extra {
            if sheet.row_y(*row) <= base {
                base = base.saturating_sub(i64::from(*delta));
            } else {
                break;
            }
        }
        sheet.row_at(base)
    }

    /// 总宽/总高（含所有调整，**不含**表头）。
    pub fn content_size(&self, sheet: &Sheet) -> (i32, i32) {
        let w = px_i32(sheet.width()).saturating_add(
            self.col_extra
                .iter()
                .filter(|(i, _)| *i < sheet.cols)
                .map(|(_, d)| *d)
                .sum::<i32>(),
        );
        let h = px_i32(sheet.height()).saturating_add(
            self.row_extra
                .iter()
                .filter(|(i, _)| *i < sheet.rows)
                .map(|(_, d)| *d)
                .sum::<i32>(),
        );
        (w.max(0), h.max(0))
    }
}

/// 拖拽命中的表头分隔线：`is_col` 为真表示拖的是第 `index` 列的**右边线**。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SheetDivider {
    pub is_col: bool,
    pub index: u32,
}

struct Region {
    clip: RECT,
    /// 可见行范围（含两端）
    rows: (u32, u32),
    /// 可见列范围（含两端）
    cols: (u32, u32),
    offset_x: i32,
    offset_y: i32,
}

// ---------------------------------------------------------------------------
// 绘制器
// ---------------------------------------------------------------------------

struct Painter<'a> {
    hdc: HDC,
    sheet: &'a Sheet,
    /// 视图内的行列尺寸调整（用户拖过分隔线才有内容）。
    sizes: SheetSizes<'a>,
    m: Metrics,
    content: RECT,
    scroll_x: i32,
    scroll_y: i32,
    /// 可能出现在屏幕上的合并区（进 paint 前筛一次，逐格再查就只扫这一小撮）。
    merges: Vec<Merge>,
}

impl<'a> Painter<'a> {
    fn new(
        hdc: HDC,
        sheet: &'a Sheet,
        sizes: SheetSizes<'a>,
        content: RECT,
        scroll_x: i32,
        scroll_y: i32,
    ) -> Self {
        let m = Metrics::current();
        let merges = visible_merges(sheet, &sizes, &m, content, scroll_x, scroll_y);
        Self {
            hdc,
            sheet,
            sizes,
            m,
            content,
            scroll_x,
            scroll_y,
            merges,
        }
    }

    // —— 冻结区尺寸 ——

    fn frozen_cols(&self) -> u32 {
        self.sheet.frozen_cols.min(self.sheet.cols)
    }

    fn frozen_rows(&self) -> u32 {
        self.sheet.frozen_rows.min(self.sheet.rows)
    }

    fn frozen_w(&self) -> i32 {
        px_i32(self.sizes.col_x(self.sheet, self.frozen_cols())).max(0)
    }

    fn frozen_h(&self) -> i32 {
        px_i32(self.sizes.row_y(self.sheet, self.frozen_rows())).max(0)
    }

    /// 内容区（扣掉左上表头）。
    fn body_rect(&self) -> RECT {
        RECT {
            left: self.m.body_left(self.content),
            top: self.m.body_top(self.content),
            right: self.content.right,
            bottom: self.content.bottom,
        }
    }

    // —— 坐标换算：内容坐标 → 屏幕坐标 ——

    fn col_left_screen_at(&self, body_left: i32, offset_x: i32, col: u32) -> i32 {
        body_left
            .saturating_add(px_i32(self.sizes.col_x(self.sheet, col)))
            .saturating_sub(offset_x)
    }

    fn col_left_screen(&self, region: &Region, col: u32) -> i32 {
        self.col_left_screen_at(self.m.body_left(self.content), region.offset_x, col)
    }

    fn col_width_of(&self, col: u32) -> i32 {
        self.sizes.col_width(self.sheet, col)
    }

    fn col_right_screen(&self, region: &Region, col: u32) -> i32 {
        self.col_left_screen(region, col)
            .saturating_add(self.col_width_of(col))
    }

    fn row_top_screen_at(&self, body_top: i32, offset_y: i32, row: u32) -> i32 {
        body_top
            .saturating_add(px_i32(self.sizes.row_y(self.sheet, row)))
            .saturating_sub(offset_y)
    }

    fn row_top_screen(&self, region: &Region, row: u32) -> i32 {
        self.row_top_screen_at(self.m.body_top(self.content), region.offset_y, row)
    }

    fn row_height_of(&self, row: u32) -> i32 {
        self.sizes.row_height(self.sheet, row)
    }

    fn row_bottom_screen(&self, region: &Region, row: u32) -> i32 {
        self.row_top_screen(region, row)
            .saturating_add(self.row_height_of(row))
    }

    fn cell_box(&self, region: &Region, row: u32, col: u32) -> RECT {
        RECT {
            left: self.col_left_screen(region, col),
            top: self.row_top_screen(region, row),
            right: self.col_right_screen(region, col),
            bottom: self.row_bottom_screen(region, row),
        }
    }

    fn merge_box(&self, region: &Region, merge: &Merge) -> RECT {
        // 合并区自己按尺寸换算（不能直接用 `Sheet::merge_rect`：它算的是文件坐标，
        // 用户拖过列宽之后右/下边界要跟着走）。
        let left = self
            .m
            .body_left(self.content)
            .saturating_add(px_i32(self.sizes.col_x(self.sheet, merge.col)))
            .saturating_sub(region.offset_x);
        let top = self
            .m
            .body_top(self.content)
            .saturating_add(px_i32(self.sizes.row_y(self.sheet, merge.row)))
            .saturating_sub(region.offset_y);
        let mut w = 0i32;
        for col in merge.col..merge.col.saturating_add(merge.cols) {
            w = w.saturating_add(self.sizes.col_width(self.sheet, col));
        }
        let mut h = 0i32;
        for row in merge.row..merge.row.saturating_add(merge.rows) {
            h = h.saturating_add(self.sizes.row_height(self.sheet, row));
        }
        RECT {
            left,
            top,
            right: left.saturating_add(w.max(0)),
            bottom: top.saturating_add(h.max(0)),
        }
    }

    /// 文字可用的内框（去掉内边距）。
    fn text_box(&self, box_: RECT) -> RECT {
        let pad = self.m.pad;
        let left = box_.left.saturating_add(pad);
        let top = box_.top.saturating_add(pad);
        let right = box_.right.saturating_sub(pad).max(left);
        let bottom = box_.bottom.saturating_sub(pad).max(top);
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    // —— 区域划分 ——

    fn regions(&self) -> Vec<Region> {
        let mut out = Vec::with_capacity(4);
        let body = self.body_rect();
        if body.right <= body.left || body.bottom <= body.top {
            return out;
        }
        let rows_total = self.sheet.rows;
        let cols_total = self.sheet.cols;
        if rows_total == 0 || cols_total == 0 {
            return out;
        }
        let fw = self.frozen_w().min(body.right - body.left).max(0);
        let fh = self.frozen_h().min(body.bottom - body.top).max(0);
        let frozen_rows = self.frozen_rows();
        let frozen_cols = self.frozen_cols();

        // 1) 普通区（最先画；冻结区之后覆盖进来）
        if body.right > body.left.saturating_add(fw) && body.bottom > body.top.saturating_add(fh) {
            let clip = RECT {
                left: body.left.saturating_add(fw),
                top: body.top.saturating_add(fh),
                right: body.right,
                bottom: body.bottom,
            };
            out.push(Region {
                clip,
                rows: visible_rows(
                    self.sheet,
                    &self.sizes,
                    self.scroll_y,
                    clip.bottom - clip.top,
                    frozen_rows,
                ),
                cols: visible_cols(
                    self.sheet,
                    &self.sizes,
                    self.scroll_x,
                    clip.right - clip.left,
                    frozen_cols,
                ),
                offset_x: self.scroll_x,
                offset_y: self.scroll_y,
            });
        }
        // 2) 冻结行 × 普通列（水平滚动仍然跟随）
        if fh > 0 && body.right > body.left.saturating_add(fw) && frozen_rows > 0 {
            let clip = RECT {
                left: body.left.saturating_add(fw),
                top: body.top,
                right: body.right,
                bottom: body.top.saturating_add(fh),
            };
            out.push(Region {
                clip,
                rows: (0, frozen_rows.saturating_sub(1)),
                cols: visible_cols(
                    self.sheet,
                    &self.sizes,
                    self.scroll_x,
                    clip.right - clip.left,
                    frozen_cols,
                ),
                offset_x: self.scroll_x,
                offset_y: 0,
            });
        }
        // 3) 冻结列 × 普通行（垂直滚动仍然跟随）
        if fw > 0 && body.bottom > body.top.saturating_add(fh) && frozen_cols > 0 {
            let clip = RECT {
                left: body.left,
                top: body.top.saturating_add(fh),
                right: body.left.saturating_add(fw),
                bottom: body.bottom,
            };
            out.push(Region {
                clip,
                rows: visible_rows(
                    self.sheet,
                    &self.sizes,
                    self.scroll_y,
                    clip.bottom - clip.top,
                    frozen_rows,
                ),
                cols: (0, frozen_cols.saturating_sub(1)),
                offset_x: 0,
                offset_y: self.scroll_y,
            });
        }
        // 4) 冻结交叉区（最后画，永远压在最上面）
        if fw > 0 && fh > 0 && frozen_rows > 0 && frozen_cols > 0 {
            out.push(Region {
                clip: RECT {
                    left: body.left,
                    top: body.top,
                    right: body.left.saturating_add(fw),
                    bottom: body.top.saturating_add(fh),
                },
                rows: (0, frozen_rows.saturating_sub(1)),
                cols: (0, frozen_cols.saturating_sub(1)),
                offset_x: 0,
                offset_y: 0,
            });
        }
        out
    }

    // —— 顶层 ——

    fn paint(&self) {
        fill_solid(self.hdc, self.content, TEXT_BACKGROUND_RGB);
        // 整体裁剪：任何绘制都不许跑到 content（标题栏/边框）上
        let saved = unsafe { SaveDC(self.hdc) };
        unsafe {
            let _ = IntersectClipRect(
                self.hdc,
                self.content.left,
                self.content.top,
                self.content.right,
                self.content.bottom,
            );
        }
        self.paint_headers();
        for region in self.regions() {
            self.paint_region(&region);
        }
        if saved != 0 {
            unsafe {
                let _ = RestoreDC(self.hdc, saved);
            }
        }
    }

    fn paint_headers(&self) {
        let content = self.content;
        let body_left = self.m.body_left(content);
        let body_top = self.m.body_top(content);
        let top_strip = RECT {
            left: content.left,
            top: content.top,
            right: content.right,
            bottom: body_top.min(content.bottom),
        };
        let left_strip = RECT {
            left: content.left,
            top: body_top.min(content.bottom),
            right: body_left.min(content.right),
            bottom: content.bottom,
        };
        fill_solid(self.hdc, top_strip, HEADER_BG_RGB);
        fill_solid(self.hdc, left_strip, HEADER_BG_RGB);
        fill_solid(
            self.hdc,
            RECT {
                left: content.left,
                top: content.top,
                right: body_left.min(content.right),
                bottom: body_top.min(content.bottom),
            },
            HEADER_CORNER_RGB,
        );
        self.paint_column_header(top_strip, body_left);
        self.paint_row_header(left_strip, body_top);

        // 表头与内容之间的分界线（压在表头内侧，不越过 content）
        let t = self.m.thin;
        if body_top > content.top {
            fill_solid(
                self.hdc,
                RECT {
                    left: content.left,
                    top: body_top.saturating_sub(t).max(content.top),
                    right: content.right,
                    bottom: body_top,
                },
                HEADER_SEP_RGB,
            );
        }
        if body_left > content.left {
            fill_solid(
                self.hdc,
                RECT {
                    left: body_left.saturating_sub(t).max(content.left),
                    top: content.top,
                    right: body_left,
                    bottom: content.bottom,
                },
                HEADER_SEP_RGB,
            );
        }
    }

    fn paint_column_header(&self, strip: RECT, body_left: i32) {
        if self.sheet.cols == 0 || strip.right <= strip.left || strip.bottom <= strip.top {
            return;
        }
        let frozen = self.frozen_cols();
        let fw = self
            .frozen_w()
            .min(strip.right.saturating_sub(body_left))
            .max(0);
        // 普通列（随 scroll_x），先画
        if strip.right > body_left.saturating_add(fw) {
            let clip = RECT {
                left: body_left.saturating_add(fw),
                top: strip.top,
                right: strip.right,
                bottom: strip.bottom,
            };
            let cols = visible_cols(
                self.sheet,
                &self.sizes,
                self.scroll_x,
                clip.right - clip.left,
                frozen,
            );
            self.draw_column_labels(cols, self.scroll_x, clip, body_left);
        }
        // 冻结列（不动），后画
        if fw > 0 && frozen > 0 {
            let clip = RECT {
                left: body_left,
                top: strip.top,
                right: body_left.saturating_add(fw),
                bottom: strip.bottom,
            };
            self.draw_column_labels((0, frozen.saturating_sub(1)), 0, clip, body_left);
        }
    }

    fn paint_row_header(&self, strip: RECT, body_top: i32) {
        if self.sheet.rows == 0 || strip.bottom <= strip.top || strip.right <= strip.left {
            return;
        }
        let frozen = self.frozen_rows();
        let fh = self
            .frozen_h()
            .min(strip.bottom.saturating_sub(body_top))
            .max(0);
        if strip.bottom > body_top.saturating_add(fh) {
            let clip = RECT {
                left: strip.left,
                top: body_top.saturating_add(fh),
                right: strip.right,
                bottom: strip.bottom,
            };
            let rows = visible_rows(
                self.sheet,
                &self.sizes,
                self.scroll_y,
                clip.bottom - clip.top,
                frozen,
            );
            self.draw_row_labels(rows, self.scroll_y, clip, body_top);
        }
        if fh > 0 && frozen > 0 {
            let clip = RECT {
                left: strip.left,
                top: body_top,
                right: strip.right,
                bottom: body_top.saturating_add(fh),
            };
            self.draw_row_labels((0, frozen.saturating_sub(1)), 0, clip, body_top);
        }
    }

    fn draw_column_labels(&self, cols: (u32, u32), offset_x: i32, clip: RECT, body_left: i32) {
        if cols.0 > cols.1 || clip.right <= clip.left || clip.bottom <= clip.top {
            return;
        }
        let saved = unsafe { SaveDC(self.hdc) };
        unsafe {
            let _ = IntersectClipRect(self.hdc, clip.left, clip.top, clip.right, clip.bottom);
        }
        let t = self.m.thin;
        let mut col = cols.0;
        loop {
            let left = self.col_left_screen_at(body_left, offset_x, col);
            let right = left.saturating_add(self.col_width_of(col));
            if right > clip.left && left < clip.right && right > left {
                let label = column_label(col);
                let rect = RECT {
                    left,
                    top: clip.top,
                    right,
                    bottom: clip.bottom,
                };
                draw_line(
                    self.hdc,
                    rect,
                    &label,
                    TextStyle::LineNumber,
                    HEADER_FG_RGB,
                    DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
                );
                fill_solid(
                    self.hdc,
                    RECT {
                        left: right.saturating_sub(t),
                        top: clip.top,
                        right,
                        bottom: clip.bottom,
                    },
                    HEADER_SEP_RGB,
                );
            }
            if col == cols.1 {
                break;
            }
            col += 1;
        }
        if saved != 0 {
            unsafe {
                let _ = RestoreDC(self.hdc, saved);
            }
        }
    }

    fn draw_row_labels(&self, rows: (u32, u32), offset_y: i32, clip: RECT, body_top: i32) {
        if rows.0 > rows.1 || clip.right <= clip.left || clip.bottom <= clip.top {
            return;
        }
        let saved = unsafe { SaveDC(self.hdc) };
        unsafe {
            let _ = IntersectClipRect(self.hdc, clip.left, clip.top, clip.right, clip.bottom);
        }
        let t = self.m.thin;
        let mut row = rows.0;
        loop {
            let top = self.row_top_screen_at(body_top, offset_y, row);
            let bottom = top.saturating_add(self.row_height_of(row));
            if bottom > clip.top && top < clip.bottom && bottom > top {
                let label = row.saturating_add(1).to_string();
                let rect = RECT {
                    left: clip.left,
                    top,
                    right: clip.right,
                    bottom,
                };
                draw_line(
                    self.hdc,
                    rect,
                    &label,
                    TextStyle::LineNumber,
                    HEADER_FG_RGB,
                    DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
                );
                fill_solid(
                    self.hdc,
                    RECT {
                        left: clip.left,
                        top: bottom.saturating_sub(t),
                        right: clip.right,
                        bottom,
                    },
                    HEADER_SEP_RGB,
                );
            }
            if row == rows.1 {
                break;
            }
            row += 1;
        }
        if saved != 0 {
            unsafe {
                let _ = RestoreDC(self.hdc, saved);
            }
        }
    }

    // —— 一个区域 ——

    fn paint_region(&self, region: &Region) {
        if region.clip.right <= region.clip.left || region.clip.bottom <= region.clip.top {
            return;
        }
        if region.rows.0 > region.rows.1 || region.cols.0 > region.cols.1 {
            return;
        }
        let saved = unsafe { SaveDC(self.hdc) };
        unsafe {
            let _ = IntersectClipRect(
                self.hdc,
                region.clip.left,
                region.clip.top,
                region.clip.right,
                region.clip.bottom,
            );
        }
        self.paint_backgrounds(region);
        self.paint_grid(region);
        self.paint_merge_fills(region);
        self.paint_borders(region);
        self.paint_texts(region);
        if saved != 0 {
            unsafe {
                let _ = RestoreDC(self.hdc, saved);
            }
        }
    }

    /// 单元格底色（在网格线**下面**画，网格仍然可见——Excel 就是这个层次）。
    fn paint_backgrounds(&self, region: &Region) {
        for row in region.rows.0..=region.rows.1 {
            for col in region.cols.0..=region.cols.1 {
                if merge_covering(&self.merges, row, col).is_some() {
                    continue;
                }
                let Some(cell) = self.sheet.cell(row, col) else {
                    continue;
                };
                let Some(bg) = cell.style.colors.bg else {
                    continue;
                };
                let box_ = self.cell_box(region, row, col);
                if !intersects(box_, region.clip) {
                    continue;
                }
                fill_solid(self.hdc, box_, rgb_to_colorref(bg));
            }
        }
    }

    /// 网格线：整条竖线/横线一次画完（逐格画会在每帧建几百个刷子）。
    fn paint_grid(&self, region: &Region) {
        let (rows, cols) = (region.rows, region.cols);
        let x0 = self.col_left_screen(region, cols.0);
        let x1 = self.col_right_screen(region, cols.1);
        let y0 = self.row_top_screen(region, rows.0);
        let y1 = self.row_bottom_screen(region, rows.1);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let t = self.m.thin;
        unsafe {
            let brush = CreateSolidBrush(COLORREF(GRID_RGB));
            if brush.0.is_null() {
                return;
            }
            let mut col = cols.0;
            loop {
                let x = self.col_left_screen(region, col);
                fill_rect(
                    self.hdc,
                    RECT {
                        left: x,
                        top: y0,
                        right: x.saturating_add(t),
                        bottom: y1,
                    },
                    brush,
                );
                if col == cols.1 {
                    break;
                }
                col += 1;
            }
            // 最右/最下一条：让表格"封口"
            fill_rect(
                self.hdc,
                RECT {
                    left: x1.saturating_sub(t),
                    top: y0,
                    right: x1,
                    bottom: y1,
                },
                brush,
            );
            let mut row = rows.0;
            loop {
                let y = self.row_top_screen(region, row);
                fill_rect(
                    self.hdc,
                    RECT {
                        left: x0,
                        top: y,
                        right: x1,
                        bottom: y.saturating_add(t),
                    },
                    brush,
                );
                if row == rows.1 {
                    break;
                }
                row += 1;
            }
            fill_rect(
                self.hdc,
                RECT {
                    left: x0,
                    top: y1.saturating_sub(t),
                    right: x1,
                    bottom: y1,
                },
                brush,
            );
            let _ = DeleteObject(brush.into());
        }
    }

    /// 合并区：先把"内部网格线"擦成底色（或白），再按左上格的边框画外框。
    ///
    /// 为什么用"擦"而不是"逐条判断这条网格线在不在合并区里"：擦是合并区数量级
    /// （几个），判断是"网格线 × 合并区"数量级，滚动时后者会明显更贵。
    fn paint_merge_fills(&self, region: &Region) {
        if self.merges.is_empty() {
            return;
        }
        let t = self.m.thin;
        for merge in &self.merges {
            let box_ = self.merge_box(region, merge);
            if !intersects(box_, region.clip) {
                continue;
            }
            let bg = self
                .sheet
                .cell(merge.row, merge.col)
                .and_then(|cell| cell.style.colors.bg)
                .map_or(TEXT_BACKGROUND_RGB, rgb_to_colorref);
            let inner = RECT {
                left: box_.left.saturating_add(t),
                top: box_.top.saturating_add(t),
                right: box_.right,
                bottom: box_.bottom,
            };
            // 用 `fill_solid`（它建完刷子立刻删）：GDI 对象漏一个就是漏一个句柄
            fill_solid(self.hdc, inner, bg);
        }
    }

    fn paint_borders(&self, region: &Region) {
        unsafe {
            let brush = CreateSolidBrush(COLORREF(BORDER_RGB));
            if brush.0.is_null() {
                return;
            }
            for row in region.rows.0..=region.rows.1 {
                for col in region.cols.0..=region.cols.1 {
                    if merge_covering(&self.merges, row, col).is_some() {
                        continue;
                    }
                    let Some(cell) = self.sheet.cell(row, col) else {
                        continue;
                    };
                    if cell.style.border.is_empty() {
                        continue;
                    }
                    let box_ = self.cell_box(region, row, col);
                    if !intersects(box_, region.clip) {
                        continue;
                    }
                    self.stroke_borders(brush, box_, &cell.style.border);
                }
            }
            for merge in &self.merges {
                let box_ = self.merge_box(region, merge);
                if !intersects(box_, region.clip) {
                    continue;
                }
                let Some(cell) = self.sheet.cell(merge.row, merge.col) else {
                    continue;
                };
                if cell.style.border.is_empty() {
                    continue;
                }
                self.stroke_borders(brush, box_, &cell.style.border);
            }
            let _ = DeleteObject(brush.into());
        }
    }

    fn stroke_borders(&self, brush: HBRUSH, box_: RECT, border: &Border) {
        let sides = [
            (Side::Top, border.top),
            (Side::Right, border.right),
            (Side::Bottom, border.bottom),
            (Side::Left, border.left),
        ];
        for (side, weight) in sides {
            let (first, second) = border_rects(box_, side, weight, self.m.thin, self.m.medium);
            if let Some(rect) = first {
                fill_rect(self.hdc, rect, brush);
            }
            if let Some(rect) = second {
                fill_rect(self.hdc, rect, brush);
            }
        }
    }

    // —— 文字 ——

    fn paint_texts(&self, region: &Region) {
        FONTS.with(|slot| {
            let mut fonts = slot.borrow_mut();
            // 合并区的文字：按合并后的整块排版（Excel 也是这么画的）
            for merge in &self.merges {
                let box_ = self.merge_box(region, merge);
                if !intersects(box_, region.clip) {
                    continue;
                }
                let Some(cell) = self.sheet.cell(merge.row, merge.col) else {
                    continue;
                };
                self.draw_cell_text(&mut fonts, region, box_, cell, false);
            }
            // 普通格：被合并区吃掉的格子不单独画字
            for row in region.rows.0..=region.rows.1 {
                for col in region.cols.0..=region.cols.1 {
                    if merge_covering(&self.merges, row, col).is_some() {
                        continue;
                    }
                    let Some(cell) = self.sheet.cell(row, col) else {
                        continue;
                    };
                    let box_ = self.cell_box(region, row, col);
                    if !intersects(box_, region.clip) {
                        continue;
                    }
                    self.draw_cell_text(&mut fonts, region, box_, cell, true);
                }
            }
        });
    }

    fn draw_cell_text(
        &self,
        fonts: &mut super::text::FontSet,
        region: &Region,
        box_: RECT,
        cell: &Cell,
        allow_overflow: bool,
    ) {
        let text = cell.text.as_str();
        if text.is_empty() {
            return;
        }
        let area = self.text_box(box_);
        if area.right <= area.left || area.bottom <= area.top {
            return;
        }
        let style = cell.style;
        let font = fonts.get(text_style_for(&style));
        let rgb = rgb_to_colorref(style.colors.fg);
        let halign = match style.h_align {
            HAlign::Left => DT_LEFT,
            HAlign::Center => DT_CENTER,
            HAlign::Right => DT_RIGHT,
        };
        let valign = match style.v_align {
            VAlign::Top => DT_TOP,
            VAlign::Center => DT_VCENTER,
            VAlign::Bottom => DT_BOTTOM,
        };
        if style.wrap {
            // 折行：GDI 自己断行（DT_WORDBREAK），先量总高度再按 v_align 落位
            draw_wrapped(self.hdc, font, area, text, rgb, halign, style.v_align);
            return;
        }
        let base = DT_SINGLELINE | DT_NOPREFIX | halign | valign;
        let (text_w, _) = measure_text(self.hdc, font, text);
        let avail = area.right - area.left;
        if text_w <= avail {
            draw_text_font(self.hdc, font, area, text, rgb, base);
            return;
        }
        // 放不下：**数值**格才给 `####`（Excel 的语义是"这一列太窄，装不下这个数"）。
        //
        // 只按文字内容猜是不够的：SKU 那种 `10052953469532` 在文件里是**文本**
        // （`t="s"`），Excel 对文本从不画 `####`（右侧空就溢出、右侧有内容就硬裁）。
        // 1.9.0 实测：整列 SKU 被判成数字后全屏 `####`，用户等于看不到数据。
        if cell.numeric && looks_numeric(text) {
            let fill = hash_fill(self.hdc, font, avail);
            draw_text_font(self.hdc, font, area, &fill, rgb, base);
            return;
        }
        // 左对齐的非数字 → 向右溢出，遇到第一个有内容的格子就停（Excel 规则）
        if allow_overflow && style.h_align == HAlign::Left {
            let last = overflow_last_col(self.sheet, cell.row, cell.col);
            if last > cell.col {
                let right = self
                    .col_right_screen(region, last)
                    .saturating_sub(self.m.pad);
                if right > area.left {
                    let wide = RECT {
                        left: area.left,
                        top: area.top,
                        right,
                        bottom: area.bottom,
                    };
                    draw_text_font(
                        self.hdc,
                        font,
                        wide,
                        text,
                        rgb,
                        DT_SINGLELINE | DT_NOPREFIX | DT_LEFT | valign,
                    );
                    return;
                }
            }
        }
        // 其余：硬裁，不加省略号
        draw_text_font(self.hdc, font, area, text, rgb, base);
    }
}

/// 只有真正可能出现在屏幕上的合并区（进 `Painter` 前筛一遍）。
fn visible_merges(
    sheet: &Sheet,
    sizes: &SheetSizes<'_>,
    m: &Metrics,
    content: RECT,
    scroll_x: i32,
    scroll_y: i32,
) -> Vec<Merge> {
    if sheet.merges.is_empty() {
        return Vec::new();
    }
    let body_w = (content.right - content.left - m.header_w).max(0);
    let body_h = (content.bottom - content.top - m.header_h).max(0);
    let x_max = i64::from(scroll_x.max(0)).saturating_add(i64::from(body_w));
    let y_max = i64::from(scroll_y.max(0)).saturating_add(i64::from(body_h));
    sheet
        .merges
        .iter()
        .copied()
        .filter(|merge| {
            let x = sizes.col_x(sheet, merge.col);
            let y = sizes.row_y(sheet, merge.row);
            let mut w = 0i32;
            for col in merge.col..merge.col.saturating_add(merge.cols) {
                w = w.saturating_add(sizes.col_width(sheet, col));
            }
            let mut h = 0i32;
            for row in merge.row..merge.row.saturating_add(merge.rows) {
                h = h.saturating_add(sizes.row_height(sheet, row));
            }
            let w = i64::from(w);
            let h = i64::from(h);
            w > 0 && h > 0 && x < x_max && x + w > 0 && y < y_max && y + h > 0
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 底层绘制小工具
// ---------------------------------------------------------------------------

/// 用已有的刷子填一块（退化矩形直接跳过；`FillRect` 会按 DC 裁剪区裁）。
fn fill_rect(hdc: HDC, rect: RECT, brush: HBRUSH) {
    if rect.right > rect.left && rect.bottom > rect.top {
        unsafe {
            FillRect(hdc, &rect, brush);
        }
    }
}

/// 量一段文字的像素宽高（用的就是绘制时那个字体）。
fn measure_text(hdc: HDC, font: HFONT, text: &str) -> (i32, i32) {
    if text.is_empty() {
        return (0, 0);
    }
    unsafe {
        let old = SelectObject(hdc, font.into());
        let buf: Vec<u16> = text.encode_utf16().collect();
        let mut size = SIZE::default();
        let _ = GetTextExtentPoint32W(hdc, &buf, &mut size);
        SelectObject(hdc, old);
        (size.cx.max(0), size.cy.max(0))
    }
}

/// 指定字体/颜色画一段单行或多行文字（空串不调 `DrawTextW`，见 `#1`）。
fn draw_text_font(
    hdc: HDC,
    font: HFONT,
    rect: RECT,
    text: &str,
    rgb: u32,
    format: DRAW_TEXT_FORMAT,
) {
    if text.is_empty() || rect.right <= rect.left || rect.bottom <= rect.top {
        return;
    }
    unsafe {
        let old = SelectObject(hdc, font.into());
        SetTextColor(hdc, COLORREF(rgb));
        let mut buf: Vec<u16> = text.encode_utf16().collect();
        let mut area = rect;
        DrawTextW(hdc, &mut buf, &raw mut area, format);
        SelectObject(hdc, old);
    }
}

/// `wrap` 的格子：先用 `DT_CALCRECT` 量总高，再按 `v_align` 落位后正式画。
fn draw_wrapped(
    hdc: HDC,
    font: HFONT,
    area: RECT,
    text: &str,
    rgb: u32,
    halign: DRAW_TEXT_FORMAT,
    valign: VAlign,
) {
    if text.is_empty() || area.right <= area.left || area.bottom <= area.top {
        return;
    }
    unsafe {
        let old = SelectObject(hdc, font.into());
        SetTextColor(hdc, COLORREF(rgb));
        let mut buf: Vec<u16> = text.encode_utf16().collect();
        let width = area.right - area.left;
        let mut calc = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: 0,
        };
        DrawTextW(
            hdc,
            &mut buf,
            &raw mut calc,
            DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX,
        );
        let text_h = (calc.bottom - calc.top).max(0);
        let box_h = area.bottom - area.top;
        let top = match valign {
            VAlign::Top => area.top,
            VAlign::Center => area.top.saturating_add((box_h - text_h).max(0) / 2),
            VAlign::Bottom => area.bottom.saturating_sub(text_h),
        };
        let mut dest = RECT {
            left: area.left,
            top,
            right: area.right,
            bottom: area.bottom,
        };
        DrawTextW(
            hdc,
            &mut buf,
            &raw mut dest,
            DT_WORDBREAK | DT_NOPREFIX | halign,
        );
        SelectObject(hdc, old);
    }
}

/// 数字放不下时铺满的 `#` 串（按可用宽度算能放几个）。
fn hash_fill(hdc: HDC, font: HFONT, avail: i32) -> String {
    let (unit, _) = measure_text(hdc, font, "#");
    if unit <= 0 || avail <= 0 {
        return "#".repeat(4);
    }
    let count = (avail / unit).clamp(1, MAX_SPAN as i32) as usize;
    "#".repeat(count)
}

// ---------------------------------------------------------------------------
// 公开 API
// ---------------------------------------------------------------------------

/// 内容总尺寸（含表头），壳层用它算滚动上限。
///
/// 返回值**不小于视口**：表比窗口小时没有可滚动的量（`extent - view <= 0`），
/// 壳层可以直接拿差值当滚动上限。与 [`sheet_cell_at`] 用同一套
/// [`Metrics`]，两者不会出现"滚动到最底还差几像素"的错位。
pub fn sheet_extent(sheet: &Sheet, sizes: &SheetSizes<'_>, view_w: i32, view_h: i32) -> (i32, i32) {
    let m = Metrics::current();
    let (content_w, content_h) = sizes.content_size(sheet);
    let w = i64::from(m.header_w).saturating_add(i64::from(content_w));
    let h = i64::from(m.header_h).saturating_add(i64::from(content_h));
    (px_i32(w).max(view_w.max(0)), px_i32(h).max(view_h.max(0)))
}

/// 屏幕坐标（相对 content 左上角）命中哪个格子。
///
/// - 落在行列号表头上：`None`（表头不是内容，点它不该选中格子）；
/// - 冻结行列：屏幕坐标先按"冻结区不滚动"换算，再交给 `Sheet::col_at/row_at`；
/// - 合并区：返回**左上角那个格子的坐标**（和 Excel 的选区语义一致）；
///   调用方要整块矩形可以用 `Sheet::merge_at` / `merge_rect`。
pub fn sheet_cell_at(
    sheet: &Sheet,
    sizes: &SheetSizes<'_>,
    scroll_x: i32,
    scroll_y: i32,
    px: i32,
    py: i32,
) -> Option<(usize, usize)> {
    if sheet.rows == 0 || sheet.cols == 0 {
        return None;
    }
    let m = Metrics::current();
    if px < m.header_w || py < m.header_h {
        return None;
    }
    let body_x = i64::from(px).saturating_sub(i64::from(m.header_w));
    let body_y = i64::from(py).saturating_sub(i64::from(m.header_h));
    let frozen_w = sizes.col_x(sheet, sheet.frozen_cols.min(sheet.cols));
    let frozen_h = sizes.row_y(sheet, sheet.frozen_rows.min(sheet.rows));
    let content_x = if body_x < frozen_w {
        body_x
    } else {
        body_x.saturating_add(i64::from(scroll_x.max(0)))
    };
    let content_y = if body_y < frozen_h {
        body_y
    } else {
        body_y.saturating_add(i64::from(scroll_y.max(0)))
    };
    let col = sizes.col_at(sheet, content_x);
    let row = sizes.row_at(sheet, content_y);
    if row >= sheet.rows || col >= sheet.cols {
        return None;
    }
    // 合并区：换算到左上角（选区语义）
    if let Some(merge) = sheet.merge_at(row, col) {
        let (origin_row, origin_col) = (merge.row, merge.col);
        if origin_row < sheet.rows && origin_col < sheet.cols {
            return Some((origin_row as usize, origin_col as usize));
        }
    }
    Some((row as usize, col as usize))
}

/// 表头分隔线命中测试（1.9.1）：用户拖列宽/行高时，先问这里"按在哪条线上"。
///
/// - 只有按在**列号条**（顶部 `header_h` 高）或**行号条**（左侧 `header_w` 宽）上才算——
///   内容区里按下去是选格子，不该改尺寸；
/// - 容差 `DIVIDER_GRAB` 像素（线两侧各算），太窄了鼠标根本对不准；
/// - `px`/`py` 是相对 content 左上角的坐标，与 [`sheet_cell_at`] 同一套口径；
/// - 返回的 `index` 是"第 index 列的右边线"（`is_col = true`）或"第 index 行的下边线"。
pub fn sheet_divider_at(
    sheet: &Sheet,
    sizes: &SheetSizes<'_>,
    scroll_x: i32,
    scroll_y: i32,
    px: i32,
    py: i32,
) -> Option<SheetDivider> {
    if sheet.cols == 0 || sheet.rows == 0 || px < 0 || py < 0 {
        return None;
    }
    let m = Metrics::current();
    let grab = DIVIDER_GRAB;

    // 列号条：找最近的列右边界
    if py < m.header_h && px >= m.header_w {
        let body_x = i64::from(px - m.header_w).saturating_add(i64::from(scroll_x.max(0)));
        let near = sizes.col_at(sheet, body_x);
        for col in near.saturating_sub(1)..=near.min(sheet.cols.saturating_sub(1)) {
            let edge = sizes.col_x(sheet, col.saturating_add(1));
            if (body_x - edge).unsigned_abs() as i32 <= grab {
                return Some(SheetDivider {
                    is_col: true,
                    index: col,
                });
            }
        }
        return None;
    }

    // 行号条：找最近的行下边界
    if px < m.header_w && py >= m.header_h {
        let body_y = i64::from(py - m.header_h).saturating_add(i64::from(scroll_y.max(0)));
        let near = sizes.row_at(sheet, body_y);
        for row in near.saturating_sub(1)..=near.min(sheet.rows.saturating_sub(1)) {
            let edge = sizes.row_y(sheet, row.saturating_add(1));
            if (body_y - edge).unsigned_abs() as i32 <= grab {
                return Some(SheetDivider {
                    is_col: false,
                    index: row,
                });
            }
        }
        return None;
    }

    None
}

/// 画一张工作表。
///
/// `scroll_x`/`scroll_y` 是"内容已经滚走的像素"，由壳层维护；`content` 是扣除边框和
/// 标题之后真正可以画表格的矩形（会先被表头切掉左上一块）。
///
/// 空表（`rows == 0 || cols == 0`）画成一张"空白表 + 表头"，不要画成纯白。
pub fn paint_sheet(
    hwnd: HWND,
    chrome: Option<&Chrome<'_>>,
    content: Option<RECT>,
    sheet: &Sheet,
    sizes: &SheetSizes<'_>,
    scroll_x: i32,
    scroll_y: i32,
) {
    let mut ps = PAINTSTRUCT::default();
    let window_dc: HDC = unsafe { BeginPaint(hwnd, &mut ps) };
    if window_dc.0.is_null() {
        unsafe {
            let _ = EndPaint(hwnd, &ps);
        }
        return;
    }
    let rc = client_rect(hwnd);
    let started = std::time::Instant::now();
    // 双缓冲：整屏先画进离屏 DC，最后一次性 BitBlt 上屏（滚动时不频闪）
    let mut buffer = acquire_sheet_buffer(window_dc, rc);
    let hdc: HDC = buffer.as_ref().map_or(window_dc, |b| b.dc);
    unsafe {
        SetBkMode(hdc, TRANSPARENT);
    }
    fill_solid(hdc, rc, TEXT_BACKGROUND_RGB);

    // 顶栏也画在这块 DC 上（和 paint_text/paint_list 一样，省一次 BeginPaint）
    let after_chrome = match chrome {
        Some(spec) => paint_chrome(hdc, rc, spec),
        None => rc,
    };
    let content = intersect_rect(content.unwrap_or(after_chrome), rc);
    if content.right > content.left && content.bottom > content.top {
        check_sheet_health(sheet);
        Painter::new(hdc, sheet, *sizes, content, scroll_x, scroll_y).paint();
    }

    if let Some(buf) = &buffer {
        unsafe {
            let _ = BitBlt(
                window_dc,
                rc.left,
                rc.top,
                rc.right - rc.left,
                rc.bottom - rc.top,
                Some(buf.dc),
                0,
                0,
                SRCCOPY,
            );
        }
    }
    if qlr_core::log::enabled(qlr_core::log::Level::Debug) {
        let (g, u) = gui_resources();
        qlr_core::log_debug!(
            "window",
            "PAINT sheet gdi={g} user={u} {:.2}ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
    release_sheet_buffer(buffer.take());
    unsafe {
        let _ = EndPaint(hwnd, &ps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qlr_core::sheet::{Cell, CellStyle, Colors, Merge};

    fn cell(row: u32, col: u32, text: &str) -> Cell {
        Cell {
            row,
            col,
            text: text.to_string(),
            style: CellStyle::default(),
            // 测试里的格子当"文本"用：真实解析器按 `t` 属性给这个位，
            // 猜内容会出错（SKU 是长数字但属于文本）。
            numeric: false,
        }
    }

    fn sample() -> Sheet {
        Sheet {
            name: "测试".to_string(),
            cols: 3,
            rows: 2,
            col_widths: vec![100, 50, 70],
            row_heights: vec![20, 40],
            cells: vec![cell(0, 0, "A1"), cell(1, 2, "C2")],
            merges: vec![Merge {
                row: 0,
                col: 1,
                rows: 2,
                cols: 2,
            }],
            frozen_rows: 0,
            frozen_cols: 0,
        }
    }

    fn wide_sheet() -> Sheet {
        Sheet {
            cols: 100,
            rows: 1000,
            col_widths: vec![50; 100],
            row_heights: vec![20; 1000],
            ..Default::default()
        }
    }

    #[test]
    fn column_labels_follow_excel_base26() {
        assert_eq!(column_label(0), "A");
        assert_eq!(column_label(1), "B");
        assert_eq!(column_label(25), "Z");
        assert_eq!(column_label(26), "AA");
        assert_eq!(column_label(27), "AB");
        assert_eq!(column_label(51), "AZ");
        assert_eq!(column_label(52), "BA");
        assert_eq!(column_label(701), "ZZ");
        assert_eq!(column_label(702), "AAA");
        // 极端值不 panic、不空
        assert!(!column_label(u32::MAX).is_empty());
    }

    #[test]
    fn visible_window_tracks_scroll_and_frozen_skip() {
        let s = wide_sheet();
        // 300 px 宽 / 50 px 一列 → 第 0..=5 列（第 5 列右边界正好 300）
        assert_eq!(visible_cols(&s, &SheetSizes::EMPTY, 0, 300, 0), (0, 5));
        // 滚走 125 px：从第 2 列开始；125+299=424 → 第 8 列
        assert_eq!(visible_cols(&s, &SheetSizes::EMPTY, 125, 300, 0), (2, 8));
        // 冻结列不重复画
        assert_eq!(visible_cols(&s, &SheetSizes::EMPTY, 0, 300, 3), (3, 5));
        // 100 px 高 / 20 px 一行 → 第 0..=4 行
        assert_eq!(visible_rows(&s, &SheetSizes::EMPTY, 0, 100, 0), (0, 4));
        assert_eq!(visible_rows(&s, &SheetSizes::EMPTY, 0, 100, 2), (2, 4));
        // 滚到底：不越界（1000 行 × 20 px = 20000，滚到上限 19900 时正好露出最后 5 行）
        let (first, last) = visible_rows(&s, &SheetSizes::EMPTY, 19_900, 100, 0);
        assert_eq!((first, last), (995, 999));
        // 滚过头（壳层算大）：仍然夹在最后一行，不越界
        let (first, last) = visible_rows(&s, &SheetSizes::EMPTY, 99_000, 100, 0);
        assert_eq!((first, last), (999, 999));
        // 空表不 panic
        let empty = Sheet::default();
        assert_eq!(visible_cols(&empty, &SheetSizes::EMPTY, 0, 300, 0), (0, 0));
        assert_eq!(visible_rows(&empty, &SheetSizes::EMPTY, 0, 100, 0), (0, 0));
    }

    #[test]
    fn numeric_heuristic_matches_excel_intent() {
        for ok in [
            "123", "1,234.50", "-3.2", "+5", "12%", "$1,200", "￥88", "1e5",
        ] {
            assert!(looks_numeric(ok), "{ok} 应该判成数字");
        }
        // 日期/时间**不算**数字（1.9.0 决定）：预览只读、没法拉宽列，
        // 放不下时给"部分文本"比给 `####` 有用。Excel 里日期是数字，这里故意不同。
        for bad in [
            "",
            "  ",
            "abc",
            "A1",
            "12abc",
            "3 件",
            "-",
            "%",
            "总价 12",
            "2024-01-05",
            "12:30:45",
            "2026/10/01",
        ] {
            assert!(!looks_numeric(bad), "{bad} 不该判成数字");
        }
    }

    #[test]
    fn overflow_stops_at_first_non_empty_neighbour() {
        let mut s = Sheet {
            cols: 5,
            rows: 2,
            col_widths: vec![20; 5],
            row_heights: vec![20; 2],
            cells: vec![cell(0, 0, "很长的标题"), cell(0, 2, "x"), cell(1, 3, "y")],
            ..Default::default()
        };
        // 第 0 行：col0 往右，col1 空 → 可到 col1；col2 有内容 → 停
        assert_eq!(overflow_last_col(&s, 0, 0), 1);
        // 第 1 行：col0 空到 col2，col3 有内容 → 停在 col2
        assert_eq!(overflow_last_col(&s, 1, 0), 2);
        // 右边全是空的：一直到最后可写的一列（受 MAX_SPAN 限制）
        assert_eq!(overflow_last_col(&s, 0, 2), 4);
        // 被合并区占住也算"有内容"
        s.merges = vec![Merge {
            row: 0,
            col: 3,
            rows: 1,
            cols: 2,
        }];
        assert_eq!(overflow_last_col(&s, 1, 0), 2);
        // 最后一列没有右邻
        assert_eq!(overflow_last_col(&s, 1, 4), 4);
    }

    #[test]
    fn extent_and_hit_test_round_trip() {
        let s = sample();
        let m = Metrics::current();
        // extent = 表头 + 内容，且不小于视口
        assert_eq!(
            sheet_extent(&s, &SheetSizes::EMPTY, 0, 0),
            (
                m.header_w + px_i32(s.width()),
                m.header_h + px_i32(s.height())
            )
        );
        assert_eq!(
            sheet_extent(&s, &SheetSizes::EMPTY, 5_000, 5_000),
            (5_000, 5_000)
        );
        // 每格中心都要能命中回自己
        for row in 0..s.rows {
            for col in 0..s.cols {
                let px = m.header_w + px_i32(s.col_x(col)) + 1;
                let py = m.header_h + px_i32(s.row_y(row)) + 1;
                let hit = sheet_cell_at(&s, &SheetSizes::EMPTY, 0, 0, px, py);
                // 合并区会被换算到左上角（0,1）
                let expected = if s.covered_by_merge(row, col) || (row, col) == (0, 1) {
                    (0usize, 1usize)
                } else {
                    (row as usize, col as usize)
                };
                assert_eq!(hit, Some(expected), "({row},{col}) 命中错了");
            }
        }
        // 表头区域不命中
        assert_eq!(
            sheet_cell_at(&s, &SheetSizes::EMPTY, 0, 0, m.header_w - 1, m.header_h + 3),
            None
        );
        assert_eq!(
            sheet_cell_at(&s, &SheetSizes::EMPTY, 0, 0, m.header_w + 3, m.header_h - 1),
            None
        );
        // 滚动之后同一格仍然能命中（要按"冻结区不滚动"的规则反推坐标）
        let scroll = 37;
        let px = m.header_w + px_i32(s.col_x(2)) - scroll + 1;
        // (0,2) 被合并区 (0,1,2×2) 盖住 → 命中换算成左上角 (0,1)
        assert_eq!(
            sheet_cell_at(&s, &SheetSizes::EMPTY, scroll, 0, px, m.header_h + 1),
            Some((0, 1))
        );
        // 滚到最后一格：不越界（2×2 的合并区把整表右下角都吃掉了 → 仍回到左上角）
        assert_eq!(
            sheet_cell_at(
                &s,
                &SheetSizes::EMPTY,
                10_000,
                10_000,
                m.header_w + 1,
                m.header_h + 1
            ),
            Some((0, 1))
        );
    }

    #[test]
    fn merge_interior_is_suppressed_from_grid() {
        let s = sample();
        let merges = s.merges.clone();
        // 左上角与内部都算"归这个合并区管"（内部网格线因此不画）
        assert!(merge_covering(&merges, 0, 1).is_some());
        assert!(merge_covering(&merges, 1, 2).is_some());
        // 区域外不归它管
        assert!(merge_covering(&merges, 0, 0).is_none());
        assert!(merge_covering(&merges, 2, 1).is_none());
        // 越界合并区的矩形被夹住，不会 panic、不会给出负宽高
        let bad = Sheet {
            cols: 1,
            rows: 1,
            col_widths: vec![10],
            row_heights: vec![10],
            merges: vec![Merge {
                row: 0,
                col: 0,
                rows: 9,
                cols: 9,
            }],
            ..Default::default()
        };
        let (_, _, w, h) = bad.merge_rect(&bad.merges[0]);
        assert!(w >= 0 && h >= 0);
        // 空合并区（rows/cols = 0）也算异常数据，绘制时直接跳过
        let zero = Merge {
            row: 0,
            col: 0,
            rows: 0,
            cols: 0,
        };
        assert!(!zero.contains(0, 0));
    }

    #[test]
    fn degenerate_sheets_degrade_instead_of_panicking() {
        let empty = Sheet::default();
        let m = Metrics::current();
        assert_eq!(
            sheet_extent(&empty, &SheetSizes::EMPTY, 0, 0),
            (m.header_w, m.header_h)
        );
        assert_eq!(
            sheet_cell_at(&empty, &SheetSizes::EMPTY, 0, 0, 100, 100),
            None
        );
        assert_eq!(
            sheet_cell_at(&empty, &SheetSizes::EMPTY, 0, 0, -5, -5),
            None
        );
        // 冻结数与行列数不一致（坏解析）：不会 panic
        let mut odd = Sheet {
            cols: 2,
            rows: 2,
            col_widths: vec![10],
            row_heights: vec![],
            frozen_rows: 9,
            frozen_cols: 9,
            ..Default::default()
        };
        odd.cells = vec![cell(99, 99, "越界")];
        assert!(sheet_extent(&odd, &SheetSizes::EMPTY, 0, 0).0 >= 0);
        // 落在表头里 → None（表头不是内容）
        assert_eq!(sheet_cell_at(&odd, &SheetSizes::EMPTY, 0, 0, 3, 3), None);
        // 内容区里点第一格：行高表是空的 → 所有行都是 0 高，`row_at` 只能夹到最后一行
        // （数据模型的既定行为；绘制不 panic，`check_sheet_health` 会告警"行高数目不一致"）
        assert_eq!(
            sheet_cell_at(
                &odd,
                &SheetSizes::EMPTY,
                0,
                0,
                m.header_w + 1,
                m.header_h + 1
            ),
            Some((1, 0))
        );
        // 极端滚动偏移（壳层算错）也只是不命中，不能 panic
        assert!(sheet_cell_at(&odd, &SheetSizes::EMPTY, i32::MAX, i32::MAX, 100, 100).is_some());
        assert_eq!(
            sheet_cell_at(&odd, &SheetSizes::EMPTY, i32::MIN, i32::MIN, 0, 0),
            None
        );
    }

    #[test]
    fn colours_and_borders_convert_as_documented() {
        // 0xRRGGBB → 0x00BBGGRR
        assert_eq!(rgb_to_colorref(0x00_11_22_33), 0x00_33_22_11);
        // 三通道相同的灰色怎么转都不变（本文件的浅灰都故意取这种值）
        assert_eq!(rgb_to_colorref(0x00_D9_D9_D9), 0x00_D9_D9_D9);
        let box_ = RECT {
            left: 0,
            top: 0,
            right: 100,
            bottom: 40,
        };
        // 无边框：什么都不画
        assert_eq!(
            border_rects(box_, Side::Right, line::NONE, 1, 2),
            (None, None)
        );
        // THIN：1 px，贴着内边界
        let (a, b) = border_rects(box_, Side::Right, line::THIN, 1, 2);
        assert_eq!(
            a,
            Some(RECT {
                left: 99,
                top: 0,
                right: 100,
                bottom: 40
            })
        );
        assert!(b.is_none());
        // MEDIUM：2 px
        let (a, _) = border_rects(box_, Side::Bottom, line::MEDIUM, 1, 2);
        assert_eq!(
            a,
            Some(RECT {
                left: 0,
                top: 38,
                right: 100,
                bottom: 40
            })
        );
        // DOUBLE：两条，中间留缝
        let (a, b) = border_rects(box_, Side::Left, line::DOUBLE, 1, 2);
        assert!(a.is_some() && b.is_some());
        assert!(a.unwrap_or_default().left < b.unwrap_or_default().left);
    }

    #[test]
    fn bold_styles_map_onto_the_font_cache() {
        let mut style = CellStyle::default();
        assert_eq!(text_style_for(&style), TextStyle::Normal);
        style.bold = true;
        assert_eq!(text_style_for(&style), TextStyle::Heading3);
        style.size_dpt = 150;
        assert_eq!(text_style_for(&style), TextStyle::Heading2);
        style.size_dpt = 200;
        assert_eq!(text_style_for(&style), TextStyle::Heading1);
        // 非粗体的大字号也只能落到 Normal（字体缓存没有更多档位）
        style.bold = false;
        assert_eq!(text_style_for(&style), TextStyle::Normal);
        // 带底色的格子不会影响字体选择
        style.colors = Colors {
            fg: 0x00FF_0000,
            bg: Some(0x00FF_FF00),
        };
        assert_eq!(text_style_for(&style), TextStyle::Normal);
    }
}
