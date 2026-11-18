//! pptx 幻灯片绘制（1.11.0）：把 `qlr_core::pptx::PptxDoc` 的**当前一页**画到窗口里。
//!
//! 与 `docx.rs` / `sheet.rs` 同一个思路：**模型只带数据，像素每帧现画**。
//! - 解析与文字折行已在 `qlr-render::pptx` 做完，每个段落/每行都有幻灯片坐标；
//! - 这一层只做"把幻灯片坐标系按 `s` 倍放大、居中到内容区"，然后逐形状画；
//! - 翻页只改 `page`，不重新解析（一帧几毫秒）。
//!
//! 折行用的是 `qlr-render::docx::approx_measure`（保守估计），绘制用真实 GDI 字体宽度；
//! 二者可能差几个百分点，所以**每行都裁到自己的矩形右边界**，超出的字被裁掉而不会溢出形状外。

use super::*;
use qlr_core::docx::{DocImage, HAlign, Line, Para, Run};
use qlr_core::pptx::{
    Chart, ChartKind, ChartShape, FillRect, Picture, PptxDoc, Shape, TableShape, TextBox,
};
use std::cell::RefCell;
use std::collections::HashMap;
use windows::Win32::Graphics::Gdi::{
    CreatePen, IntersectClipRect, PS_SOLID, Pie, Polyline, RestoreDC, SaveDC,
};

const SCOPE: &str = "pptx";

/// 幻灯片边框颜色（浅灰）。
const SLIDE_BORDER_RGB: u32 = 0x00D0_D0D0;
/// 表格网格线颜色。
const TABLE_BORDER_RGB: u32 = 0x00C8_C8C8;

// ---------------------------------------------------------------- 字体缓存

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct FontKey {
    h: i32,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    mono: bool,
}

const FONT_LIMIT: usize = 256;

type FontMap = HashMap<FontKey, HFONT>;

thread_local! {
    static FONTS: RefCell<FontMap> = RefCell::new(HashMap::new());
}

fn create_font(key: FontKey) -> HFONT {
    let face = if key.mono {
        "Consolas"
    } else {
        "Microsoft YaHei"
    };
    let face_w = to_wide(face);
    let weight = if key.bold {
        FW_BOLD.0 as i32
    } else {
        FW_NORMAL.0 as i32
    };
    unsafe {
        CreateFontW(
            -key.h.max(1),
            0,
            0,
            0,
            weight,
            if key.italic { 1 } else { 0 },
            if key.underline { 1 } else { 0 },
            if key.strike { 1 } else { 0 },
            DEFAULT_CHARSET,
            OUT_TT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            DEFAULT_PITCH.0 as u32,
            PCWSTR(face_w.as_ptr()),
        )
    }
}

fn get_font(set: &mut FontMap, key: FontKey) -> HFONT {
    if let Some(font) = set.get(&key) {
        return *font;
    }
    if set.len() < FONT_LIMIT {
        let font = create_font(key);
        if !font.0.is_null() {
            set.insert(key, font);
            return font;
        }
    }
    let mut reduced = key;
    reduced.underline = false;
    reduced.strike = false;
    if reduced != key {
        if let Some(font) = set.get(&reduced) {
            return *font;
        }
        let font = create_font(reduced);
        if !font.0.is_null() {
            set.insert(reduced, font);
            return font;
        }
    }
    reduced.italic = false;
    if let Some(font) = set.get(&reduced) {
        return *font;
    }
    let base = FontKey {
        h: key.h.max(1),
        bold: key.bold,
        italic: false,
        underline: false,
        strike: false,
        mono: false,
    };
    if let Some(font) = set.get(&base) {
        return *font;
    }
    let font = create_font(base);
    if !font.0.is_null() {
        set.insert(base, font);
        return font;
    }
    HFONT(unsafe { GetStockObject(DEFAULT_GUI_FONT) }.0)
}

/// DPI 变化时由 `text::fonts_invalidate` 调用，丢掉本线程的 pptx 字体缓存。
pub(crate) fn clear_font_cache() {
    FONTS.with(|cell| {
        let mut set = cell.borrow_mut();
        for (_, font) in set.drain() {
            unsafe {
                let _ = DeleteObject(font.into());
            }
        }
    });
}

// ---------------------------------------------------------------- 离屏缓冲

struct PptxBuffer {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    w: i32,
    h: i32,
}

impl PptxBuffer {
    unsafe fn new(window_dc: HDC, rc: RECT) -> Option<Self> {
        let dc = unsafe { CreateCompatibleDC(Some(window_dc)) };
        if dc.0.is_null() {
            return None;
        }
        let w = (rc.right - rc.left).max(1);
        let h = (rc.bottom - rc.top).max(1);
        let bitmap = unsafe { CreateCompatibleBitmap(window_dc, w, h) };
        if bitmap.0.is_null() {
            let _ = unsafe { DeleteDC(dc) };
            return None;
        }
        let old = unsafe { SelectObject(dc, bitmap.into()) };
        Some(Self {
            dc,
            bitmap,
            old,
            w,
            h,
        })
    }
}

impl Drop for PptxBuffer {
    fn drop(&mut self) {
        unsafe {
            let _ = SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

thread_local! {
    static BUFFER: RefCell<Option<PptxBuffer>> = const { RefCell::new(None) };
}

fn acquire_buffer(window_dc: HDC, rc: RECT) -> Option<PptxBuffer> {
    let w = (rc.right - rc.left).max(1);
    let h = (rc.bottom - rc.top).max(1);
    BUFFER.with(|slot| {
        let mut slot = slot.borrow_mut();
        match slot.take() {
            Some(buf) if buf.w == w && buf.h == h => Some(buf),
            _ => unsafe { PptxBuffer::new(window_dc, rc) },
        }
    })
}

fn release_buffer(buf: Option<PptxBuffer>) {
    if let Some(buf) = buf {
        BUFFER.with(|slot| *slot.borrow_mut() = Some(buf));
    }
}

/// 收起预览/切换内容时丢掉复用的离屏缓冲。
pub fn drop_pptx_buffer() {
    BUFFER.with(|slot| {
        let _ = slot.borrow_mut().take();
    });
}

// ---------------------------------------------------------------- 颜色/几何工具

fn rgb_to_colorref(rgb: u32) -> COLORREF {
    let r = (rgb >> 16) & 0xFF;
    let g = (rgb >> 8) & 0xFF;
    let b = rgb & 0xFF;
    COLORREF(r | (g << 8) | (b << 16))
}

/// 幻灯片在内容区里的变换：`(缩放 s, 原点 x, 原点 y, 幻灯宽, 幻灯高)`（屏幕像素）。
/// 保持长宽比整页塞入内容区并居中，`s` 含 DPI 内容缩放因子。
fn slide_transform(content: RECT, slide_w: u32, slide_h: u32) -> (f32, i32, i32, i32, i32) {
    let dpi_scale = (dpi::cpx(96) as f32 / 96.0).max(0.1);
    let cw = (content.right - content.left).max(1) as f32;
    let ch = (content.bottom - content.top).max(1) as f32;
    let pw_log = slide_w.max(1) as f32;
    let ph_log = slide_h.max(1) as f32;
    let fit = (cw / (pw_log * dpi_scale)).min(ch / (ph_log * dpi_scale));
    let fit = if fit.is_finite() && fit > 0.0 {
        fit.min(4.0)
    } else {
        1.0
    };
    let s = (dpi_scale * fit).max(0.02);
    let pw = ((pw_log * s).round() as i32).max(1);
    let ph = ((ph_log * s).round() as i32).max(1);
    let ox = content.left + ((content.right - content.left) - pw) / 2;
    let oy = content.top + ((content.bottom - content.top) - ph) / 2;
    (s, ox, oy, pw, ph)
}

#[inline]
fn scale_px(base: i32, origin: i32, s: f32) -> i32 {
    origin + (base as f32 * s).round() as i32
}

fn scaled_rect(r: &qlr_core::docx::Rect, ox: i32, oy: i32, s: f32) -> RECT {
    RECT {
        left: scale_px(r.x, ox, s),
        top: scale_px(r.y, oy, s),
        right: scale_px(r.right(), ox, s),
        bottom: scale_px(r.bottom(), oy, s),
    }
}

fn fill_color_rect(hdc: HDC, rect: &RECT, rgb: u32) {
    if rect.right <= rect.left || rect.bottom <= rect.top {
        return;
    }
    let brush = unsafe { CreateSolidBrush(rgb_to_colorref(rgb)) };
    unsafe {
        FillRect(hdc, rect, brush);
        let _ = DeleteObject(brush.into());
    }
}

fn frame_rect(hdc: HDC, rect: &RECT, rgb: u32) {
    if rect.right <= rect.left || rect.bottom <= rect.top {
        return;
    }
    let brush = unsafe { CreateSolidBrush(rgb_to_colorref(rgb)) };
    unsafe {
        let _ = FrameRect(hdc, rect, brush);
        let _ = DeleteObject(brush.into());
    }
}

fn run_font_key(run: &Run, s: f32) -> FontKey {
    FontKey {
        h: ((run.size_px() as f32) * s).round().max(1.0) as i32,
        bold: run.bold,
        italic: run.italic,
        underline: run.underline,
        strike: run.strike,
        mono: run.mono,
    }
}

// ---------------------------------------------------------------- 画一行

fn draw_line(hdc: HDC, set: &mut FontMap, line: &Line, align: HAlign, ox: i32, oy: i32, s: f32) {
    if line.runs.is_empty() || line.is_empty() {
        return;
    }
    let lx = scale_px(line.rect.x, ox, s);
    let ly = scale_px(line.rect.y, oy, s);
    let lw = ((line.rect.w as f32) * s).round().max(1.0) as i32;
    let lh = ((line.rect.h as f32) * s).round().max(1.0) as i32;
    let right_limit = lx + lw;

    let mut widths: Vec<i32> = Vec::with_capacity(line.runs.len());
    let mut total = 0i32;
    for run in &line.runs {
        if run.text.is_empty() {
            widths.push(0);
            continue;
        }
        let font = get_font(set, run_font_key(run, s));
        let old = unsafe { SelectObject(hdc, font.into()) };
        let buf: Vec<u16> = run.text.encode_utf16().collect();
        let mut size = SIZE::default();
        unsafe {
            let _ = GetTextExtentPoint32W(hdc, &buf, &mut size);
            SelectObject(hdc, old);
        }
        let w = size.cx.max(0);
        widths.push(w);
        total += w;
    }

    let start_off = match align {
        HAlign::Center => ((lw - total) / 2).max(0),
        HAlign::Right => (lw - total).max(0),
        HAlign::Left | HAlign::Justify => 0,
    };
    let mut x = lx + start_off;

    for (run, w) in line.runs.iter().zip(widths) {
        if run.text.is_empty() {
            continue;
        }
        if x >= right_limit {
            break;
        }
        let font = get_font(set, run_font_key(run, s));
        let old = unsafe { SelectObject(hdc, font.into()) };
        if let Some(hl) = run.highlight {
            let band = RECT {
                left: x,
                top: ly,
                right: (x + w).min(right_limit),
                bottom: ly + lh,
            };
            fill_color_rect(hdc, &band, hl);
        }
        unsafe {
            SetTextColor(hdc, rgb_to_colorref(run.color));
        }
        let mut buf: Vec<u16> = run.text.encode_utf16().collect();
        let mut rect = RECT {
            left: x,
            top: ly,
            right: right_limit,
            bottom: ly + lh,
        };
        unsafe {
            DrawTextW(
                hdc,
                &mut buf,
                &raw mut rect,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
            );
            SelectObject(hdc, old);
        }
        x += w;
    }
}

fn draw_paras(hdc: HDC, set: &mut FontMap, paras: &[Para], ox: i32, oy: i32, s: f32) {
    for para in paras {
        for line in &para.lines {
            draw_line(hdc, set, line, para.align, ox, oy, s);
        }
    }
}

// ---------------------------------------------------------------- 画形状

fn draw_shape(hdc: HDC, set: &mut FontMap, shape: &Shape, doc: &PptxDoc, ox: i32, oy: i32, s: f32) {
    match shape {
        Shape::Text(tb) => draw_text_box(hdc, set, tb, ox, oy, s),
        Shape::Picture(p) => draw_picture(hdc, p, doc, ox, oy, s),
        Shape::Rect(fr) => draw_fill_rect(hdc, fr, ox, oy, s),
        Shape::Table(t) => draw_table(hdc, set, t, ox, oy, s),
        Shape::Chart(c) => draw_chart(hdc, set, c, ox, oy, s),
    }
}

// ---------------------------------------------------------------- 画图表

fn chart_color(i: usize) -> u32 {
    const PAL: [u32; 8] = [
        0x4472C4, 0xED7D31, 0xA5A5A5, 0xFFC000, 0x5B9BD5, 0x70AD47, 0x264478, 0x9E480E,
    ];
    PAL[i % PAL.len()]
}

/// 用指定字号画一行左对齐文字（标题/标签/图例共用）。
#[allow(clippy::too_many_arguments)]
fn draw_text_simple(
    hdc: HDC,
    set: &mut FontMap,
    text: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    rgb: u32,
    bold: bool,
) {
    if text.is_empty() || w <= 0 || h <= 0 {
        return;
    }
    let fs = ((h as f32) * 0.72).max(8.0) as i32;
    let key = FontKey {
        h: fs,
        bold,
        italic: false,
        underline: false,
        strike: false,
        mono: false,
    };
    let font = get_font(set, key);
    let old = unsafe { SelectObject(hdc, font.into()) };
    unsafe {
        SetTextColor(hdc, rgb_to_colorref(rgb));
    }
    let mut buf: Vec<u16> = text.encode_utf16().collect();
    let mut r = RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    };
    unsafe {
        DrawTextW(
            hdc,
            &mut buf,
            &raw mut r,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        SelectObject(hdc, old);
    }
}

fn format_value(v: f64, fmt: &str) -> String {
    if fmt.contains('%') {
        format!("{:.0}%", v * 100.0)
    } else if v.fract().abs() < 1e-9 {
        format!("{}", v as i64)
    } else {
        format!("{:.2}", v)
    }
}

fn chart_bounds(chart: &Chart) -> (f64, f64) {
    let (lo, mut hi) = match chart.kind {
        ChartKind::BarPercent => (0.0f64, 1.0f64),
        ChartKind::BarStacked => {
            let n = chart.categories.len().max(1);
            let mut pos = vec![0.0f64; n];
            let mut neg = vec![0.0f64; n];
            for s in &chart.series {
                for (i, v) in s.values.iter().enumerate() {
                    if i < n {
                        if *v >= 0.0 {
                            pos[i] += *v;
                        } else {
                            neg[i] += *v;
                        }
                    }
                }
            }
            let hi = pos.iter().cloned().fold(0.0f64, f64::max);
            let lo = neg.iter().cloned().fold(0.0f64, f64::min);
            (lo, hi)
        }
        _ => {
            let mut lo = 0.0f64;
            let mut hi = 0.0f64;
            for s in &chart.series {
                for v in &s.values {
                    if v.is_finite() {
                        hi = hi.max(*v);
                        lo = lo.min(*v);
                    }
                }
            }
            (lo, hi)
        }
    };
    if (hi - lo).abs() < 1e-9 {
        hi = lo + 1.0;
    }
    (lo, hi)
}

fn draw_chart(hdc: HDC, set: &mut FontMap, cs: &ChartShape, ox: i32, oy: i32, s: f32) {
    let chart = &cs.chart;
    let rect = scaled_rect(&cs.rect, ox, oy, s);
    if rect.right <= rect.left || rect.bottom <= rect.top {
        return;
    }
    fill_color_rect(hdc, &rect, 0x00FF_FFFF);
    frame_rect(hdc, &rect, 0x00D9_D9D9);
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    if w < 32 || h < 28 {
        return;
    }
    let title_h = if chart.title.is_empty() {
        0
    } else {
        (h / 9).clamp(12, 36)
    };
    if !chart.title.is_empty() {
        draw_text_simple(
            hdc,
            set,
            &chart.title,
            rect.left + 6,
            rect.top + 2,
            w - 12,
            title_h,
            0x0030_3030,
            true,
        );
    }
    let legend_h = if chart.series.len() > 1 {
        (h / 10).clamp(10, 26)
    } else {
        0
    };
    let lm = (w / 7).clamp(24, 64);
    let plot = RECT {
        left: rect.left + lm,
        top: rect.top + title_h + 4,
        right: rect.right - 8,
        bottom: rect.bottom - legend_h - 4,
    };
    if plot.right <= plot.left || plot.bottom <= plot.top {
        return;
    }
    match chart.kind {
        ChartKind::Pie => draw_pie(hdc, chart, &plot),
        ChartKind::Line => draw_line_chart(hdc, set, chart, &plot, s),
        _ => draw_bar_chart(hdc, set, chart, &plot),
    }
    if legend_h > 0 {
        draw_legend(
            hdc,
            set,
            chart,
            rect.left + 6,
            rect.bottom - legend_h,
            w - 12,
            legend_h,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_grid_and_axis(
    hdc: HDC,
    set: &mut FontMap,
    chart: &Chart,
    plot: &RECT,
    lo: f64,
    span: f64,
    plot_h: i32,
    y_of: &dyn Fn(f64) -> i32,
) {
    for g in 0..=4 {
        let frac = g as f64 / 4.0;
        let val = lo + span * frac;
        let y = plot.bottom - (frac * plot_h as f64).round() as i32;
        fill_color_rect(
            hdc,
            &RECT {
                left: plot.left,
                top: y,
                right: plot.right,
                bottom: y + 1,
            },
            0x00EC_ECEC,
        );
        let label = format_value(val, &chart.number_format);
        draw_text_simple(
            hdc,
            set,
            &label,
            plot.left - 30,
            y - 6,
            28,
            12,
            0x0060_6060,
            false,
        );
    }
    if lo < 0.0 {
        let y0 = y_of(0.0);
        if y0 >= plot.top && y0 <= plot.bottom {
            fill_color_rect(
                hdc,
                &RECT {
                    left: plot.left,
                    top: y0,
                    right: plot.right,
                    bottom: y0 + 1,
                },
                0x00B0_B0B0,
            );
        }
    }
}

fn draw_category_labels(hdc: HDC, set: &mut FontMap, chart: &Chart, plot: &RECT, slot: f64) {
    let n = chart.categories.len();
    if n == 0 || n > 16 {
        return;
    }
    let y = plot.bottom + 2;
    for i in 0..n {
        let cx = plot.left as f64 + slot * (i as f64 + 0.5);
        let lw = (slot * 0.95).max(10.0) as i32;
        draw_text_simple(
            hdc,
            set,
            &chart.categories[i],
            (cx - lw as f64 / 2.0) as i32,
            y,
            lw,
            12,
            0x0060_6060,
            false,
        );
    }
}

fn draw_bar_chart(hdc: HDC, set: &mut FontMap, chart: &Chart, plot: &RECT) {
    let (lo, hi) = chart_bounds(chart);
    let span = (hi - lo).max(1e-9);
    let plot_w = (plot.right - plot.left).max(1);
    let plot_h = (plot.bottom - plot.top).max(1);
    let n = chart.categories.len().max(1);
    let m = chart.series.len().max(1);
    let y_of = |v: f64| -> i32 { plot.bottom - (((v - lo) / span) * plot_h as f64).round() as i32 };
    draw_grid_and_axis(hdc, set, chart, plot, lo, span, plot_h, &y_of);

    let slot = plot_w as f64 / n as f64;
    match chart.kind {
        ChartKind::BarStacked | ChartKind::BarPercent => {
            let bw = (slot * 0.6).max(1.0);
            for i in 0..n {
                let cx = plot.left as f64 + slot * (i as f64 + 0.5);
                let mut pos = 0.0f64;
                let mut neg = 0.0f64;
                let total_pos: f64 = chart
                    .series
                    .iter()
                    .map(|s| s.values.get(i).copied().unwrap_or(0.0).max(0.0))
                    .sum();
                for ssp in &chart.series {
                    let raw = ssp.values.get(i).copied().unwrap_or(0.0);
                    let v = if chart.kind == ChartKind::BarPercent {
                        if total_pos > 0.0 {
                            raw.max(0.0) / total_pos
                        } else {
                            0.0
                        }
                    } else {
                        raw
                    };
                    let base = if v >= 0.0 { pos } else { neg };
                    let top_v = base + v;
                    let y1 = y_of(base.max(top_v));
                    let y2 = y_of(base.min(top_v));
                    let top = y1.min(y2);
                    let bot = y1.max(y2).max(top + 1);
                    let x0 = (cx - bw / 2.0) as i32;
                    fill_color_rect(
                        hdc,
                        &RECT {
                            left: x0,
                            top,
                            right: x0 + bw as i32,
                            bottom: bot,
                        },
                        ssp.color,
                    );
                    if v >= 0.0 {
                        pos = top_v;
                    } else {
                        neg = top_v;
                    }
                }
            }
        }
        _ => {
            let gw = slot * 0.8;
            let bw = (gw / m as f64).max(1.0);
            for i in 0..n {
                let cx = plot.left as f64 + slot * (i as f64 + 0.5);
                let gx0 = cx - gw / 2.0;
                for (si, ssp) in chart.series.iter().enumerate() {
                    let v = ssp.values.get(i).copied().unwrap_or(0.0);
                    let x0 = (gx0 + bw * si as f64) as i32;
                    let ytop = y_of(v.max(0.0));
                    let ybot = y_of(v.min(0.0));
                    let top = ytop.min(ybot);
                    let bot = ytop.max(ybot).max(top + 1);
                    fill_color_rect(
                        hdc,
                        &RECT {
                            left: x0,
                            top,
                            right: x0 + bw.max(1.0) as i32,
                            bottom: bot,
                        },
                        ssp.color,
                    );
                }
            }
        }
    }
    draw_category_labels(hdc, set, chart, plot, slot);
}

fn draw_line_chart(hdc: HDC, set: &mut FontMap, chart: &Chart, plot: &RECT, s: f32) {
    let (lo, hi) = chart_bounds(chart);
    let span = (hi - lo).max(1e-9);
    let plot_w = (plot.right - plot.left).max(1);
    let plot_h = (plot.bottom - plot.top).max(1);
    let n = chart.categories.len().max(1);
    let y_of = |v: f64| -> i32 { plot.bottom - (((v - lo) / span) * plot_h as f64).round() as i32 };
    draw_grid_and_axis(hdc, set, chart, plot, lo, span, plot_h, &y_of);

    let x_of = |i: usize| -> i32 {
        plot.left + ((plot_w as f64) * (i as f64 + 0.5) / n as f64).round() as i32
    };
    let thick = ((3.0 * s).round() as i32).max(1);
    for ssp in &chart.series {
        let pen = unsafe { CreatePen(PS_SOLID, thick, rgb_to_colorref(ssp.color)) };
        let old = unsafe { SelectObject(hdc, pen.into()) };
        let mut pts: Vec<POINT> = Vec::new();
        for (i, v) in ssp.values.iter().enumerate() {
            if i < n && v.is_finite() {
                pts.push(POINT {
                    x: x_of(i),
                    y: y_of(*v),
                });
            }
        }
        if pts.len() >= 2 {
            unsafe {
                let _ = Polyline(hdc, &pts);
            }
        }
        unsafe {
            SelectObject(hdc, old);
            let _ = DeleteObject(pen.into());
        }
        for p in &pts {
            fill_color_rect(
                hdc,
                &RECT {
                    left: p.x - 2,
                    top: p.y - 2,
                    right: p.x + 2,
                    bottom: p.y + 2,
                },
                ssp.color,
            );
        }
    }
    draw_category_labels(hdc, set, chart, plot, plot_w as f64 / n as f64);
}

fn draw_pie(hdc: HDC, chart: &Chart, plot: &RECT) {
    if chart.series.is_empty() {
        return;
    }
    let ssp = &chart.series[0];
    let total: f64 = ssp.values.iter().filter(|v| **v > 0.0).sum();
    if total <= 0.0 {
        return;
    }
    let cx = (plot.left + plot.right) / 2;
    let cy = (plot.top + plot.bottom) / 2;
    let r = (((plot.right - plot.left).min(plot.bottom - plot.top)) / 2).max(2);
    let (l, t, rr, b) = (cx - r, cy - r, cx + r, cy + r);
    let mut start = 0.0f64;
    let mut k = 0usize;
    for v in &ssp.values {
        let vv = v.max(0.0);
        if vv <= 0.0 {
            continue;
        }
        let ang = vv / total * 360.0;
        let a0 = start.to_radians();
        let a1 = (start + ang).to_radians();
        let x0 = cx + (r as f64 * a0.cos()) as i32;
        let y0 = cy + (r as f64 * a0.sin()) as i32;
        let x1 = cx + (r as f64 * a1.cos()) as i32;
        let y1 = cy + (r as f64 * a1.sin()) as i32;
        let color = if k == 0 { ssp.color } else { chart_color(k) };
        let brush = unsafe { CreateSolidBrush(rgb_to_colorref(color)) };
        let old = unsafe { SelectObject(hdc, brush.into()) };
        unsafe {
            let _ = Pie(hdc, l, t, rr, b, x0, y0, x1, y1);
            SelectObject(hdc, old);
            let _ = DeleteObject(brush.into());
        }
        start += ang;
        k += 1;
    }
}

fn draw_legend(hdc: HDC, set: &mut FontMap, chart: &Chart, x: i32, y: i32, w: i32, h: i32) {
    let m = chart.series.len().max(1);
    let item_w = (w / m as i32).max(20);
    for (i, ssp) in chart.series.iter().enumerate() {
        let ix = x + item_w * i as i32;
        fill_color_rect(
            hdc,
            &RECT {
                left: ix,
                top: y + (h / 2 - 5),
                right: ix + 10,
                bottom: y + (h / 2 + 5),
            },
            ssp.color,
        );
        draw_text_simple(
            hdc,
            set,
            &ssp.name,
            ix + 14,
            y,
            item_w - 16,
            h,
            0x0040_4040,
            false,
        );
    }
}

fn draw_text_box(hdc: HDC, set: &mut FontMap, tb: &TextBox, ox: i32, oy: i32, s: f32) {
    let rect = scaled_rect(&tb.rect, ox, oy, s);
    if let Some(fill) = tb.fill {
        fill_color_rect(hdc, &rect, fill);
    }
    if let Some(line) = tb.line {
        frame_rect(hdc, &rect, line);
    }
    // 文字裁到形状矩形内，避免超出形状。
    unsafe {
        let saved = SaveDC(hdc);
        IntersectClipRect(hdc, rect.left, rect.top, rect.right, rect.bottom);
        draw_paras(hdc, set, &tb.paras, ox, oy, s);
        if saved != 0 {
            let _ = RestoreDC(hdc, saved);
        }
    }
}

fn draw_fill_rect(hdc: HDC, fr: &FillRect, ox: i32, oy: i32, s: f32) {
    let rect = scaled_rect(&fr.rect, ox, oy, s);
    if let Some(fill) = fr.fill {
        fill_color_rect(hdc, &rect, fill);
    }
    if let Some(line) = fr.line {
        frame_rect(hdc, &rect, line);
    }
}

fn draw_table(hdc: HDC, set: &mut FontMap, table: &TableShape, ox: i32, oy: i32, s: f32) {
    for row in &table.rows {
        for cell in &row.cells {
            let crect = scaled_rect(&cell.rect, ox, oy, s);
            if crect.right <= crect.left || crect.bottom <= crect.top {
                continue;
            }
            if let Some(fill) = cell.fill {
                fill_color_rect(hdc, &crect, fill);
            }
            unsafe {
                let saved = SaveDC(hdc);
                IntersectClipRect(hdc, crect.left, crect.top, crect.right, crect.bottom);
                draw_paras(hdc, set, &cell.paras, ox, oy, s);
                if saved != 0 {
                    let _ = RestoreDC(hdc, saved);
                }
            }
            if table.borders {
                frame_rect(hdc, &crect, TABLE_BORDER_RGB);
            }
        }
    }
}

fn draw_picture(hdc: HDC, pic: &Picture, doc: &PptxDoc, ox: i32, oy: i32, s: f32) {
    let Some(slot) = doc.images.get(pic.image as usize) else {
        return;
    };
    let Some(img) = slot.decoded() else {
        return;
    };
    draw_image(hdc, img, &pic.rect, ox, oy, s);
}

fn draw_image(hdc: HDC, img: &DocImage, rect: &qlr_core::docx::Rect, ox: i32, oy: i32, s: f32) {
    if img.width == 0
        || img.height == 0
        || img.rgba.len() < (img.width as usize * img.height as usize * 4)
    {
        return;
    }
    let dx = scale_px(rect.x, ox, s);
    let dy = scale_px(rect.y, oy, s);
    let dw = ((rect.w as f32) * s).round().max(1.0) as i32;
    let dh = ((rect.h as f32) * s).round().max(1.0) as i32;

    let mut bgra = Vec::with_capacity(img.rgba.len());
    for px in img.rgba.chunks_exact(4) {
        bgra.push(px[2]);
        bgra.push(px[1]);
        bgra.push(px[0]);
        bgra.push(255);
    }
    let bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: img.width as i32,
            biHeight: -(img.height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    unsafe {
        SetStretchBltMode(hdc, HALFTONE);
        let _ = SetBrushOrgEx(hdc, 0, 0, None);
        StretchDIBits(
            hdc,
            dx,
            dy,
            dw,
            dh,
            0,
            0,
            img.width as i32,
            img.height as i32,
            Some(bgra.as_ptr() as *const core::ffi::c_void),
            &bi,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
    }
}

// ---------------------------------------------------------------- 主入口

/// 画 pptx 的**当前一页（张）**。翻页由壳层改 `page` 后调用本函数重画。
/// 把一页幻灯片画到任意 HDC 的 `rc` 区域（不依赖 BeginPaint，便于离屏单测复用）。
fn draw_slide_page(
    hdc: HDC,
    rc: RECT,
    chrome: Option<&Chrome<'_>>,
    content: Option<RECT>,
    doc: &PptxDoc,
    page: usize,
) {
    unsafe {
        let bg = CreateSolidBrush(COLORREF(TEXT_BACKGROUND_RGB));
        FillRect(hdc, &rc, bg);
        let _ = DeleteObject(bg.into());

        let after_chrome = match chrome {
            Some(spec) => paint_chrome(hdc, rc, spec),
            None => rc,
        };
        let content = content.unwrap_or(after_chrome);
        let content = RECT {
            left: content.left.max(rc.left),
            top: content.top.max(rc.top),
            right: content.right.min(rc.right),
            bottom: content.bottom.min(rc.bottom),
        };
        if content.right <= content.left || content.bottom <= content.top {
            return;
        }
        let Some(slide) = doc.slide(page) else {
            return;
        };
        let (s, ox, oy, pw, ph) = slide_transform(content, doc.width, doc.height);
        let slide_rect = RECT {
            left: ox,
            top: oy,
            right: ox + pw,
            bottom: oy + ph,
        };
        // 幻灯片底色：自带背景色优先，否则白底。
        let bg_rgb = slide.bg.unwrap_or(0x00FF_FFFF);
        fill_color_rect(hdc, &slide_rect, bg_rgb);

        let saved = SaveDC(hdc);
        IntersectClipRect(
            hdc,
            slide_rect.left,
            slide_rect.top,
            slide_rect.right,
            slide_rect.bottom,
        );
        FONTS.with(|cell| {
            let mut set = cell.borrow_mut();
            for shape in &slide.shapes {
                draw_shape(hdc, &mut set, shape, doc, ox, oy, s);
            }
        });
        if saved != 0 {
            let _ = RestoreDC(hdc, saved);
        }
        frame_rect(hdc, &slide_rect, SLIDE_BORDER_RGB);
    }
}

pub fn paint_slide(
    hwnd: HWND,
    chrome: Option<&Chrome<'_>>,
    content: Option<RECT>,
    doc: &PptxDoc,
    page: usize,
) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let window_dc: HDC = BeginPaint(hwnd, &mut ps);
        if window_dc.0.is_null() {
            let _ = EndPaint(hwnd, &ps);
            return;
        }
        let rc = client_rect(hwnd);
        let started = std::time::Instant::now();

        let mut buffer = acquire_buffer(window_dc, rc);
        let hdc: HDC = buffer.as_ref().map_or(window_dc, |b| b.dc);
        SetBkMode(hdc, TRANSPARENT);

        draw_slide_page(hdc, rc, chrome, content, doc, page);

        if let Some(buffer) = &buffer {
            let _ = BitBlt(
                window_dc,
                rc.left,
                rc.top,
                rc.right - rc.left,
                rc.bottom - rc.top,
                Some(buffer.dc),
                0,
                0,
                SRCCOPY,
            );
        }
        if qlr_core::log::enabled(qlr_core::log::Level::Debug) {
            let (g, u) = gui_resources();
            qlr_core::log_debug!(
                SCOPE,
                "PAINT slide={}/{} shapes={} gdi={g} user={u} {:.2}ms",
                page + 1,
                doc.len(),
                doc.slide(page).map(|s| s.shapes.len()).unwrap_or(0),
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
        release_buffer(buffer.take());
        let _ = EndPaint(hwnd, &ps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_to_colorref_swaps_red_and_blue() {
        assert_eq!(rgb_to_colorref(0x0011_2233).0, 0x0033_2211);
        assert_eq!(rgb_to_colorref(0x00FF_0000).0, 0x0000_00FF);
    }

    #[test]
    fn slide_transform_fits_and_centers() {
        let content = RECT {
            left: 100,
            top: 50,
            right: 900,
            bottom: 650,
        };
        // 16:9 幻灯片（1280×720）。
        let (s, ox, oy, pw, ph) = slide_transform(content, 1280, 720);
        assert!(s > 0.0);
        assert!(pw <= 800, "幻灯宽 {pw} 超过内容宽 800");
        assert!(ph <= 600, "幻灯高 {ph} 超过内容高 600");
        assert!(ox >= content.left && oy >= content.top);
        let left_gap = ox - content.left;
        let right_gap = content.right - (ox + pw);
        assert!((left_gap - right_gap).abs() <= 2);
    }

    #[test]
    fn scale_px_maps_origin_and_scales() {
        assert_eq!(scale_px(0, 100, 1.0), 100);
        assert_eq!(scale_px(50, 100, 2.0), 200);
        assert_eq!(scale_px(10, 0, 0.5), 5);
    }

    /// 端到端像素回归：把一页含文字的幻灯片画到离屏 DIB，必须出现非白像素。
    /// 防的是"模型几何对了但 paint 仍是空白"（用户实测 plugin-demo.pptx 全白）。
    #[test]
    fn draw_slide_page_renders_non_blank() {
        let doc = PptxDoc {
            slides: vec![qlr_core::pptx::Slide {
                shapes: vec![Shape::Text(TextBox {
                    rect: qlr_core::docx::Rect::new(20, 20, 320, 80),
                    paras: vec![Para {
                        rect: qlr_core::docx::Rect::new(20, 20, 320, 80),
                        align: HAlign::Left,
                        lines: vec![Line {
                            rect: qlr_core::docx::Rect::new(30, 30, 300, 30),
                            runs: vec![Run {
                                text: "测试SlideABC".to_string(),
                                size_dpt: 320,
                                ..Run::default()
                            }],
                        }],
                        space_before: 0,
                        space_after: 0,
                    }],
                    fill: None,
                    line: None,
                })],
                bg: Some(0x00FF_FFFF),
                number: 1,
            }],
            width: 640,
            height: 360,
            images: vec![],
            source: None,
            title: None,
        };

        unsafe {
            let screen = GetDC(None);
            let mem = CreateCompatibleDC(Some(screen));
            let mut info = BITMAPINFO::default();
            info.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = 640;
            info.bmiHeader.biHeight = -360;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB.0;
            let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
            let bmp = CreateDIBSection(Some(mem), &info, DIB_RGB_COLORS, &mut bits, None, 0)
                .expect("CreateDIBSection 失败");
            let old = SelectObject(mem, bmp.into());
            let rc = RECT {
                left: 0,
                top: 0,
                right: 640,
                bottom: 360,
            };
            draw_slide_page(mem, rc, None, None, &doc, 0);
            let px = std::slice::from_raw_parts(bits as *const u8, 640 * 360 * 4);
            let nonwhite = px
                .chunks_exact(4)
                .filter(|p| !(p[0] > 240 && p[1] > 240 && p[2] > 240))
                .count();
            assert!(nonwhite > 200, "幻灯片几乎全白，未画出内容：{nonwhite}");
            SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
        }
    }

    /// 离屏像素回归：图表必须真的画出柱/网格（非全白）。
    #[test]
    fn draw_chart_renders_non_blank() {
        let doc = PptxDoc {
            slides: vec![qlr_core::pptx::Slide {
                shapes: vec![Shape::Chart(ChartShape {
                    rect: qlr_core::docx::Rect::new(20, 20, 600, 320),
                    chart: Chart {
                        kind: ChartKind::Bar,
                        title: "销量对比".to_string(),
                        categories: vec!["1月".into(), "2月".into(), "3月".into()],
                        series: vec![
                            qlr_core::pptx::ChartSeries {
                                name: "A".into(),
                                values: vec![1.0, 2.0, 3.0],
                                color: 0x0044_72C4,
                            },
                            qlr_core::pptx::ChartSeries {
                                name: "B".into(),
                                values: vec![2.0, 1.0, 2.5],
                                color: 0x00ED_7D31,
                            },
                        ],
                        number_format: String::new(),
                    },
                })],
                bg: Some(0x00FF_FFFF),
                number: 1,
            }],
            width: 640,
            height: 360,
            images: vec![],
            source: None,
            title: None,
        };
        unsafe {
            let screen = GetDC(None);
            let mem = CreateCompatibleDC(Some(screen));
            let mut info = BITMAPINFO::default();
            info.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = 640;
            info.bmiHeader.biHeight = -360;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB.0;
            let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
            let bmp = CreateDIBSection(Some(mem), &info, DIB_RGB_COLORS, &mut bits, None, 0)
                .expect("CreateDIBSection 失败");
            let old = SelectObject(mem, bmp.into());
            let rc = RECT {
                left: 0,
                top: 0,
                right: 640,
                bottom: 360,
            };
            draw_slide_page(mem, rc, None, None, &doc, 0);
            let px = std::slice::from_raw_parts(bits as *const u8, 640 * 360 * 4);
            let nonwhite = px
                .chunks_exact(4)
                .filter(|p| !(p[0] > 240 && p[1] > 240 && p[2] > 240))
                .count();
            assert!(nonwhite > 500, "图表几乎没画出内容：{nonwhite}");
            SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
        }
    }
}
