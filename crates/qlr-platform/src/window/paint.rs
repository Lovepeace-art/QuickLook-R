//! 基础绘制原语 —— 从 window.rs 纯搬家拆出（2026-11-04）。
//!
//! 每块只多一行 `use super::*;`，函数体一字未改。

use super::*;

/// 铺一块纯色（每次用完立刻删刷子，GDI 对象不删会漏）。
pub(crate) fn fill_solid(hdc: HDC, rect: RECT, rgb: u32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(rgb));
        FillRect(hdc, &rect, brush);
        let _ = DeleteObject(brush.into());
    }
}

/// 画一条 1 px 边框（`FrameRect` 用刷子画边框，不填内部）。
pub(crate) fn frame_solid(hdc: HDC, rect: RECT, rgb: u32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(rgb));
        FrameRect(hdc, &rect, brush);
        let _ = DeleteObject(brush.into());
    }
}

/// 用指定样式画一段单行文字。
pub(crate) fn draw_line(
    hdc: HDC,
    rect: RECT,
    text: &str,
    style: TextStyle,
    rgb: u32,
    format: DRAW_TEXT_FORMAT,
) {
    if text.is_empty() {
        return;
    }
    unsafe {
        FONTS.with(|cell| {
            let mut set = cell.borrow_mut();
            let font = set.get(style);
            let old = SelectObject(hdc, font.into());
            SetTextColor(hdc, COLORREF(rgb));
            SetBkMode(hdc, TRANSPARENT);
            let mut buf: Vec<u16> = text.encode_utf16().collect();
            let mut area = rect;
            DrawTextW(hdc, &mut buf, &raw mut area, format);
            SelectObject(hdc, old);
        });
    }
}

/// 量一段文字的像素宽度（悬停提示要按文字宽度画气泡）。
pub(crate) fn measure_line(hdc: HDC, text: &str, style: TextStyle) -> i32 {
    unsafe {
        FONTS.with(|cell| {
            let mut set = cell.borrow_mut();
            let font = set.get(style);
            let old = SelectObject(hdc, font.into());
            let buf: Vec<u16> = text.encode_utf16().collect();
            let mut size = SIZE::default();
            let _ = GetTextExtentPoint32W(hdc, &buf, &mut size);
            SelectObject(hdc, old);
            size.cx.max(0)
        })
    }
}
