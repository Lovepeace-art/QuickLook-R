//! 音频播放条 —— 从 window.rs 纯搬家拆出（2026-11-04）。
//!
//! 每块只多一行 `use super::*;`，函数体一字未改。

use super::*;

/// 音频进度条（1.7.9）：贴在内容区底部的一条。
///
/// 高度 52：上行放"0:05 / 0:15 · 播放中"，下行放轨道。再矮就挤成一团，
/// 再高就明显吃掉卡片正文的位置。
pub fn audio_bar_h() -> i32 {
    dpi::px(52)
}

pub const AUDIO_BAR_BG_RGB: u32 = 0x00F7_F7F7;

pub const AUDIO_TRACK_RGB: u32 = 0x00DC_DCDC;

// COLORREF 是 0x00BBGGRR（低字节是红）——这两个不对称的常量要按这个顺序写。
pub const AUDIO_FILL_RGB: u32 = 0x00D9_904A; // = #4A90D9 的蓝

pub const AUDIO_TEXT_RGB: u32 = 0x0040_4040;

pub const AUDIO_HINT_RGB: u32 = 0x0090_9090;

/// 音频进度条占的那一块（内容区底部一条；内容区比它矮时就占满整个内容区）。
pub fn audio_bar_rect(area: RECT) -> RECT {
    let height = (area.bottom - area.top).max(0);
    let h = audio_bar_h().min(height);
    RECT {
        left: area.left,
        top: area.bottom - h,
        right: area.right,
        bottom: area.bottom,
    }
}

/// 内容区里"画面"该占的那一块 = 内容区去掉底部进度条。
///
/// 视频每帧都往窗口上贴像素，不留出这条就会被盖住（1.7.9 实测：播放结束后进度条消失）。
/// 内容区比进度条还矮时保留 1 px，避免出现空矩形。
pub fn picture_area(area: RECT) -> RECT {
    let bar = audio_bar_rect(area);
    RECT {
        left: area.left,
        top: area.top,
        right: area.right,
        bottom: bar.top.max(area.top + 1),
    }
}

/// 毫秒 → `m:ss`（音频用；超过一小时给 `h:mm:ss`）。
fn fmt_time(ms: u64) -> String {
    let total = ms / 1000;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// 画音频进度条（1.7.9）。直接画到窗口，供 `TIMER_AUDIO` 局部刷新（整窗不重画）。
pub fn paint_audio_bar(hwnd: HWND, area: RECT, pos_ms: u64, dur_ms: u64, playing: bool) {
    unsafe {
        let hdc = GetDC(Some(hwnd));
        if hdc.0.is_null() {
            return;
        }
        paint_audio_bar_dc(hdc, area, pos_ms, dur_ms, playing);
        ReleaseDC(Some(hwnd), hdc);
    }
}

/// 画到指定 DC（整窗重画时与正文共用内存 DC，避免两次上屏）。
#[allow(unused_unsafe)]
pub fn paint_audio_bar_dc(hdc: HDC, area: RECT, pos_ms: u64, dur_ms: u64, playing: bool) {
    unsafe {
        let bar = audio_bar_rect(area);
        if bar.right <= bar.left || bar.bottom <= bar.top {
            return;
        }
        fill_solid(hdc, bar, AUDIO_BAR_BG_RGB);
        // 顶边一条分隔线：让"进度条"和上面的卡片正文有明显分界。
        fill_solid(
            hdc,
            RECT {
                left: bar.left,
                top: bar.top,
                right: bar.right,
                bottom: bar.top + 1,
            },
            TOC_SEP_RGB,
        );
        let pad = 24;
        let inner_left = bar.left + pad;
        let inner_right = bar.right - pad;
        if inner_right <= inner_left {
            return;
        }
        // 上行：时间与状态（左）+ 键位提示（右）。
        let text_top = bar.top + 5;
        let time = format!(
            "{} / {}  ·  {}",
            fmt_time(pos_ms),
            fmt_time(dur_ms),
            if playing { "播放中" } else { "已暂停" }
        );
        draw_line(
            hdc,
            RECT {
                left: inner_left,
                top: text_top,
                right: inner_right,
                bottom: text_top + 20,
            },
            &time,
            TextStyle::Normal,
            AUDIO_TEXT_RGB,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
        draw_line(
            hdc,
            RECT {
                left: inner_left,
                top: text_top,
                right: inner_right,
                bottom: text_top + 20,
            },
            "P 播放/暂停 · ←/→ 快退/快进 · 滚轮 定位",
            TextStyle::LineNumber,
            AUDIO_HINT_RGB,
            DT_RIGHT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
        // 下行：轨道 + 已播部分（圆角不做，纯色块在浅色底上已经够看）。
        let track = RECT {
            left: inner_left,
            top: bar.top + 30,
            right: inner_right,
            bottom: bar.top + 36,
        };
        fill_solid(hdc, track, AUDIO_TRACK_RGB);
        let total = dur_ms.max(1) as f64;
        let ratio = (pos_ms as f64 / total).clamp(0.0, 1.0);
        let filled = track.left + ((track.right - track.left) as f64 * ratio).round() as i32;
        if filled > track.left {
            fill_solid(
                hdc,
                RECT {
                    left: track.left,
                    top: track.top,
                    right: filled.min(track.right),
                    bottom: track.bottom,
                },
                AUDIO_FILL_RGB,
            );
        }
        // 游标：6 px 宽的一小竖，位置超出轨道时夹在两端。
        let cursor = RECT {
            left: (filled - 3).clamp(track.left, (track.right - 6).max(track.left)),
            top: track.top - 2,
            right: (filled + 3).clamp(track.left + 6, track.right),
            bottom: track.bottom + 2,
        };
        if cursor.right > cursor.left {
            fill_solid(hdc, cursor, AUDIO_FILL_RGB);
        }
    }
}
