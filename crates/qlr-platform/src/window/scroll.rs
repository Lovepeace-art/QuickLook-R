//! 滚动条 —— 从 window.rs 纯搬家拆出（2026-11-04）。
//!
//! 每块只多一行 `use super::*;`，函数体一字未改。

use super::*;

/// 右侧竖滚动条宽度（文本 / PDF / Markdown 用）。
pub fn scrollbar_w() -> i32 {
    dpi::px(12)
}

/// 滚动条槽底色与滑块色（`hover` 时用深一点的那个）。
pub const SCROLLBAR_TRACK_RGB: u32 = 0x00F2_F2F2;

pub const SCROLLBAR_THUMB_RGB: u32 = 0x00C0_C0C0;

pub const SCROLLBAR_THUMB_HOVER_RGB: u32 = 0x0090_9090;

/// 滚动条的位置信息，单位都是"内容单位"（文本是行、PDF 是页）。
///
/// 用比率而不是像素：绘制只需要知道"滑块多长、从哪儿开始"，
/// 把单位换算留给壳层，平台层不关心内容是什么。
#[derive(Clone, Copy, Debug)]
pub struct ScrollInfo {
    /// 顶部可见内容在整体里的位置（0.0–1.0）。
    pub start: f32,
    /// 一屏可见内容占整体的比例（0.0–1.0）。
    pub len: f32,
}

impl ScrollInfo {
    /// 全部内容都放得下时返回 `None`（不画滚动条）。
    pub fn new(start_units: f32, visible_units: f32, total_units: f32) -> Option<Self> {
        if total_units <= 0.0 || visible_units >= total_units {
            return None;
        }
        let len = (visible_units / total_units).clamp(0.02, 1.0);
        let travel = (total_units - visible_units).max(1.0);
        let start = (start_units / travel).clamp(0.0, 1.0);
        Some(Self { start, len })
    }
}

/// 滚动条占的那一条（内容区右侧）。
pub fn scrollbar_rect(area: RECT) -> RECT {
    RECT {
        left: area.right - scrollbar_w(),
        top: area.top,
        right: area.right,
        bottom: area.bottom,
    }
}

fn thumb_rect(track: RECT, info: ScrollInfo) -> RECT {
    let height = track.bottom - track.top;
    let thumb_h = ((info.len * height as f32).round() as i32).clamp(24, height.max(24));
    let travel = (height - thumb_h).max(0);
    let top = track.top + (info.start * travel as f32).round() as i32;
    RECT {
        left: track.left,
        top,
        right: track.right,
        bottom: top + thumb_h,
    }
}

/// 画滚动条。用 `GetDC` 而不是 `BeginPaint`：内容是用 `BeginPaint` 画的，
/// 同一块窗口一次 `BeginPaint` 只能有一个（第二次会拿到空 DC）。
pub fn paint_scrollbar(hwnd: HWND, area: RECT, info: ScrollInfo, hover: bool) {
    unsafe {
        let hdc = GetDC(Some(hwnd));
        if hdc.0.is_null() {
            return;
        }
        paint_scrollbar_dc(hdc, area, info, hover);
        ReleaseDC(Some(hwnd), hdc);
    }
}

/// 画到指定 DC（1.5.2：和正文共用内存 DC，避免两次上屏）。
#[allow(unused_unsafe)]
pub fn paint_scrollbar_dc(hdc: HDC, area: RECT, info: ScrollInfo, hover: bool) {
    unsafe {
        let track = scrollbar_rect(area);
        fill_solid(hdc, track, SCROLLBAR_TRACK_RGB);
        let thumb = thumb_rect(track, info);
        let colour = if hover {
            SCROLLBAR_THUMB_HOVER_RGB
        } else {
            SCROLLBAR_THUMB_RGB
        };
        let inner = RECT {
            left: thumb.left + 3,
            top: thumb.top + 2,
            right: thumb.right - 3,
            bottom: thumb.bottom - 2,
        };
        if inner.right > inner.left && inner.bottom > inner.top {
            fill_solid(hdc, inner, colour);
        }
        // DC 由调用方持有/释放（1.5.2）
    }
}

/// 鼠标在轨道上的 y 位置 → 期望的 `start`（0.0–1.0）。拖滑块与点轨道共用。
pub fn scroll_start_at(area: RECT, y: i32, info: ScrollInfo) -> f32 {
    let track = scrollbar_rect(area);
    let height = track.bottom - track.top;
    let thumb_h = ((info.len * height as f32).round() as i32).clamp(24, height.max(24));
    let travel = (height - thumb_h).max(1);
    let offset = (y - track.top - thumb_h / 2) as f32;
    (offset / travel as f32).clamp(0.0, 1.0)
}
