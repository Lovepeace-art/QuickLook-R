//! 底部工作表标签栏（1.9.2）。
//!
//! Excel 用户靠底部标签切工作表，预览里没有它就等于"只能靠翻页键猜第几张"——
//! 所以这一条是有意为之的**家具**，不是装饰：内容区会因此让出一条高度。
//!
//! 只在 `Sheet` 预览下画；标签名来自 `SheetDoc` 的全部工作表（几何数据只有当前那张，
//! 切换由壳层重新解析，见 `qlr-render::sheet::parse_sheet_index`）。
//!
//! 摆放与命中用**同一个** [`tab_rects`]：宽度按名字估（中文按全角、ASCII 按半角），
//! 不依赖 HDC，所以命中测试不需要先拿 DC——绘制与点击才不会错位。

use qlr_core::sheet::SheetDoc;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    DT_CENTER, DT_END_ELLIPSIS, DT_SINGLELINE, DT_VCENTER, GetDC, ReleaseDC,
};

use super::dpi;
use super::paint::{draw_line, fill_solid};
use super::text::TextStyle;

/// 标签栏高度（DIP）。
const TABS_H: i32 = 24;
/// 单个标签的最小/最大宽度（DIP）：名字太长就省略号，别把整条撑爆。
const TAB_MIN_W: i32 = 56;
const TAB_MAX_W: i32 = 200;
/// 标签左右内边距（DIP，两侧各一）。
const TAB_PAD_X: i32 = 10;

/// 标签栏底色（比内容白底略灰，和 Excel 一致）。
pub(crate) const TABS_BG_RGB: u32 = 0x00F0F0F0;
/// 活动标签底色（纯白，看起来"抬起来"）。
const TAB_ACTIVE_RGB: u32 = 0x00FF_FFFF;
/// 分隔线与活动标签下的强调色。
const TAB_LINE_RGB: u32 = 0x00D0D0D0;
const TAB_ACCENT_RGB: u32 = 0x00_2A7D4F;
/// 文字颜色。
const TAB_TEXT_RGB: u32 = 0x0050_5050;
const TAB_TEXT_ACTIVE_RGB: u32 = 0x0010_1010;

/// 标签栏高度（像素，随 DPI）。
pub fn sheet_tabs_height() -> i32 {
    dpi::cpx(TABS_H).max(1)
}

/// 一个字符大概多宽（DIP）：中文/全角按 15，ASCII 按 8。
fn char_w(ch: char) -> i32 {
    if (ch as u32) < 0x2E80 { 8 } else { 15 }
}

/// 按名字估标签宽度（DIP）。
fn tab_width(name: &str) -> i32 {
    let text: i32 = name.chars().map(char_w).sum();
    (text + TAB_PAD_X * 2).clamp(TAB_MIN_W, TAB_MAX_W)
}

/// 每个标签的矩形（从左往右；超出右边界的不再排，返回已排下的）。
///
/// 刻意**不做**横向滚动：预览窗口宽度有限，工作表数量通常个位数；真的放不下时
/// 宁可少画几个（还能用翻页键），也不要搞出一套标签滚动条。
fn tab_rects(doc: &SheetDoc, area: RECT) -> Vec<(usize, RECT)> {
    let h = sheet_tabs_height();
    let top = (area.bottom - h).max(area.top);
    let mut out = Vec::new();
    let mut x = area.left;
    for (index, sheet) in doc.sheets.iter().enumerate() {
        let w = dpi::cpx(tab_width(sheet.name.as_str())).max(1);
        if x + w > area.right {
            break;
        }
        out.push((
            index,
            RECT {
                left: x,
                top,
                right: x + w,
                bottom: area.bottom,
            },
        ));
        x += w;
    }
    out
}

/// 画标签栏。`active` 是当前工作表下标（越界时不高亮任何标签）。
pub fn paint_sheet_tabs(hwnd: HWND, area: RECT, doc: &SheetDoc, active: usize) {
    if area.right <= area.left || area.bottom <= area.top {
        return;
    }
    let h = sheet_tabs_height();
    let Some(strip) = rect_of(area.left, area.bottom - h, area.right, area.bottom) else {
        return;
    };
    let dc = unsafe { GetDC(Some(hwnd)) };
    if dc.is_invalid() {
        return;
    }
    fill_solid(dc, strip, TABS_BG_RGB);
    for (index, rect) in tab_rects(doc, area) {
        let is_active = index == active;
        if is_active {
            fill_solid(dc, rect, TAB_ACTIVE_RGB);
            // 活动标签顶部一条强调线 + 顶部主线抹掉，看起来"与内容连成一片"。
            fill_solid(
                dc,
                rect_of(
                    rect.left,
                    rect.top,
                    rect.right,
                    rect.top + dpi::cpx(2).max(1),
                )
                .unwrap_or(rect),
                TAB_ACCENT_RGB,
            );
        }
        // 右侧分隔线（活动标签也画，避免相邻标签糊在一起）。
        fill_solid(
            dc,
            rect_of(
                rect.right - dpi::cpx(1).max(1),
                rect.top,
                rect.right,
                rect.bottom,
            )
            .unwrap_or(rect),
            TAB_LINE_RGB,
        );
        let name = &doc.sheets[index].name;
        let inset = dpi::cpx(TAB_PAD_X).max(0);
        let text_rect = RECT {
            left: rect.left + inset,
            top: rect.top + dpi::cpx(2).max(0),
            right: rect.right - inset,
            bottom: rect.bottom,
        };
        draw_line(
            dc,
            text_rect,
            name,
            TextStyle::Normal,
            if is_active {
                TAB_TEXT_ACTIVE_RGB
            } else {
                TAB_TEXT_RGB
            },
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
    }
    // 标签栏上沿一条主线。
    fill_solid(
        dc,
        rect_of(
            strip.left,
            strip.top,
            strip.right,
            strip.top + dpi::cpx(1).max(1),
        )
        .unwrap_or(strip),
        TAB_LINE_RGB,
    );
    unsafe {
        ReleaseDC(Some(hwnd), dc);
    }
}

/// 命中测试：`(x, y)` 落在哪个标签上（客户区坐标）。点在标签栏空白处返回 `None`。
pub fn sheet_tab_at(doc: &SheetDoc, area: RECT, x: i32, y: i32) -> Option<usize> {
    if y < area.bottom - sheet_tabs_height() || y >= area.bottom {
        return None;
    }
    tab_rects(doc, area)
        .into_iter()
        .find(|(_, rect)| x >= rect.left && x < rect.right)
        .map(|(index, _)| index)
}

fn rect_of(left: i32, top: i32, right: i32, bottom: i32) -> Option<RECT> {
    if right > left && bottom > top {
        Some(RECT {
            left,
            top,
            right,
            bottom,
        })
    } else {
        None
    }
}
