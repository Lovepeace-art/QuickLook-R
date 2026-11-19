//! 目录 —— 从 window.rs 纯搬家拆出（2026-11-04）。
//!
//! 每块只多一行 `use super::*;`，函数体一字未改。

use super::*;

/// Markdown 目录栏宽度与每行高度。
pub fn toc_width() -> i32 {
    dpi::px(200)
}

/// 目录栏目标宽度（随窗口宽度微调）：比历史 200px 略宽、能放全常见标题；
/// 上限也收窄，避免宽窗口下目录栏出现大片空白。
///
/// 说明：上一版 30%/360 太宽（用户反馈"空白太大"），且绘图层把已算好的矩形又
/// 套了一次 `toc_rect` 导致二次收缩、与 `layout` 预留宽度不一致（那道空白的主因）。
/// 绘制已改为直接按 `below_chrome` 求解一次，这里再取一个紧凑区间即可：
/// 约 22% 窗口宽，夹在 196..228px（`dpi::px` 换算）。
pub fn toc_width_for(area_width: i32) -> i32 {
    let floor = dpi::px(196);
    let cap = dpi::px(228);
    (area_width * 22 / 100).clamp(floor.min(area_width.max(1)), cap)
}

pub fn toc_row_height() -> i32 {
    dpi::cpx(24)
}

pub const TOC_BG_RGB: u32 = 0x00F7_F7F7;

pub const TOC_SEP_RGB: u32 = 0x00E4_E4E4;

pub const TOC_TITLE_RGB: u32 = 0x00A0_9A94;

/// 目录栏里的一行。
pub struct TocItem<'a> {
    /// 标题级别（1–3，决定缩进）。
    pub level: u8,
    pub text: &'a str,
}

/// 目录栏占的那一整块（内容区左侧）。
pub fn toc_rect(area: RECT) -> RECT {
    RECT {
        left: area.left,
        top: area.top,
        right: (area.left + toc_width_for(area.right - area.left)).min(area.right),
        bottom: area.bottom,
    }
}

/// 目录第 `index` 行的位置（命中测试用）。
pub fn toc_item_rect(area: RECT, index: usize) -> RECT {
    let panel = toc_rect(area);
    let top = panel.top + dpi::px(8) + index as i32 * toc_row_height();
    RECT {
        left: panel.left,
        top,
        right: panel.right,
        bottom: top + toc_row_height(),
    }
}

/// 画 Markdown 目录栏（左侧竖排标题列表）。用 `GetDC`，理由同 [`paint_scrollbar`]。
///
/// `first` = 从第几条开始画（目录本身可以滚动，1.8.3）。
pub fn paint_toc(
    hwnd: HWND,
    area: RECT,
    items: &[TocItem<'_>],
    selected: Option<usize>,
    first: usize,
) {
    unsafe {
        let hdc = GetDC(Some(hwnd));
        if hdc.0.is_null() {
            return;
        }
        paint_toc_dc(hdc, area, items, selected, first);
        ReleaseDC(Some(hwnd), hdc);
    }
}

/// 目录一屏能放下几行（不含"目录"标题行）。滚动上限要用它算。
pub fn toc_visible_rows(area: RECT) -> usize {
    let panel = toc_rect(area);
    let usable = (panel.bottom - panel.top) - dpi::px(8) - toc_row_height();
    (usable.max(0) / toc_row_height().max(1)) as usize
}

#[allow(unused_unsafe)]
/// 画到指定 DC（同 `paint_scrollbar_dc`）。`first` = 首行下标（滚动位置）。
pub fn paint_toc_dc(
    hdc: HDC,
    area: RECT,
    items: &[TocItem<'_>],
    selected: Option<usize>,
    first: usize,
) {
    let panel = toc_rect(area);
    unsafe {
        fill_solid(hdc, panel, TOC_BG_RGB);
        // 右侧分隔线
        let sep = RECT {
            left: panel.right - 1,
            top: panel.top,
            right: panel.right,
            bottom: panel.bottom,
        };
        fill_solid(hdc, sep, TOC_SEP_RGB);

        draw_line(
            hdc,
            RECT {
                left: panel.left + 12,
                top: panel.top + 8,
                right: panel.right - 8,
                bottom: panel.top + dpi::px(8) + toc_row_height(),
            },
            "目录",
            TextStyle::LineNumber,
            TOC_TITLE_RGB,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );

        for (index, item) in items.iter().enumerate().skip(first) {
            // 行号按"屏幕上的第几行"算（`index - first`），不是按文档里的序号，否则滚动后整列会跑出去。
            let row = toc_item_rect(area, index - first + 1);
            if row.top >= panel.bottom {
                break;
            }
            if selected == Some(index) {
                let band = RECT {
                    left: row.left + 4,
                    top: row.top,
                    right: row.right - 4,
                    bottom: row.bottom,
                };
                fill_solid(hdc, band, 0x00EC_ECEC);
            }
            let indent = dpi::px(12 + (item.level.saturating_sub(1) as i32) * 12);
            draw_line(
                hdc,
                RECT {
                    left: panel.left + indent,
                    top: row.top,
                    right: panel.right - 8,
                    bottom: row.bottom,
                },
                item.text,
                if item.level <= 1 {
                    TextStyle::Normal
                } else {
                    TextStyle::Quote
                },
                TextStyle::Normal.color(),
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
        }
        // DC 由调用方持有/释放（1.5.2）
    }
}

/// 目录里第几行被点到（`y` 是客户区坐标）。返回的是 `items` 的下标。
///
/// `first` 必须和绘制时用的一致（目录可滚动，1.8.3）——两边不一致就会出现
/// "看到的行"和"点到的行"错开。
pub fn toc_item_at(area: RECT, y: i32, first: usize) -> Option<usize> {
    let panel = toc_rect(area);
    if y < panel.top + dpi::px(8) + toc_row_height() {
        return None; // "目录"标题那一行
    }
    let row = ((y - panel.top - dpi::px(8)) / toc_row_height() - 1).max(0) as usize;
    Some(first + row)
}
