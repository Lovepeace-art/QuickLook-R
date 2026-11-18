//! docx 页面绘制（1.10.0）：把 `qlr_core::docx::DocDoc` 的**当前一页**画到窗口里。
//!
//! 与 `sheet.rs` 同一个思路：**模型只带数据，像素每帧现画**。
//! - 排版（折行、分页）已经在 `qlr-render::docx` 做完了，模型里的每个 run 都有页面坐标；
//! - 这一层只做"把页面坐标系按 `s` 倍放大、居中到内容区"，然后逐行/逐块画；
//! - 翻页只改 `page_index`，不重新解析、不重新排版（一帧几毫秒）。
//!
//! # 为什么每帧都重画
//!
//! 与表格相同：一张 A4 页按 100% 铺开是 794×1123，但用户可以放到 200%，
//! 把每页都烤成位图既费内存又不够清晰。现画的代价是每帧几百个 DrawTextW，属于毫秒级。
//!
//! # 缩放的权威在哪
//!
//! 折行用的是 `qlr-render::docx::approx_measure`（保守估计，宁可早换行），
//! 绘制用的是真实 GDI 字体宽度。二者可能差几个百分点，所以**每行都裁到自己的
//! 矩形右边界**（`right_limit = line.rect.right * s`），超出的字被裁掉而不会溢出到页边距外。

use super::*;
use qlr_core::docx::{Block, DocDoc, HAlign, ImageBlock, Line, Run, Table};
use std::cell::RefCell;
use std::collections::HashMap;
use windows::Win32::Graphics::Gdi::{IntersectClipRect, RestoreDC, SaveDC};

const SCOPE: &str = "docx";

/// 一页四周留白之上的页码/页边框颜色（浅灰）。
const PAGE_BORDER_RGB: u32 = 0x00D0_D0D0;
/// 表格网格线颜色（比页边框略深一点，看得清）。
const TABLE_BORDER_RGB: u32 = 0x00C8_C8C8;

// ---------------------------------------------------------------- 字体缓存

/// 一种"物理字体"的键：像素高度 + 五个字形开关 + 是否等宽。
///
/// 和 `text::TextStyle` 的 14 档缓存不同：docx 的字号来自文件（Word 里可以有
/// 9/10.5/12/14/16/22… 各种字号），不能只有 14 档，否则全篇都是一个字号。
/// 这里按"实际用量"惰性建字体，并用上限兜底（见 [`get_font`]）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct FontKey {
    h: i32,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    mono: bool,
}

/// 字体缓存上限。正常文档去重后远小于此值（几十个）；真超了就走 [`get_font`] 的降级链，
/// 绝不每帧 `CreateFontW` 一堆又不释放（那会泄漏 GDI 句柄）。
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

/// 取（并按需创建）一个物理字体。超过上限时逐级"丢装饰"退到一个一定存在的近似字体：
/// 下划线/删除线 → 斜体 → 等宽 → 正文，最后回落到系统 GUI 字体，**绝不返回空、绝不泄漏**。
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
    // 降级链：每一次都写回缓存，保证下次不用再建。
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

/// 丢掉本线程的 docx 字体缓存。**由 `text::fonts_invalidate` 在 DPI 变化时调用**：
/// 不丢的话跨屏之后还在用旧像素高度的字体（且每次新建都漏一个 HFONT）。
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

/// 与 text.rs 的 `PaintBuffer` 同构（那边是模块私有，这里复制一份，和 sheet.rs 一样的处理）。
struct DocxBuffer {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    w: i32,
    h: i32,
}

impl DocxBuffer {
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

impl Drop for DocxBuffer {
    fn drop(&mut self) {
        unsafe {
            let _ = SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

thread_local! {
    static BUFFER: RefCell<Option<DocxBuffer>> = const { RefCell::new(None) };
}

fn acquire_docx_buffer(window_dc: HDC, rc: RECT) -> Option<DocxBuffer> {
    let w = (rc.right - rc.left).max(1);
    let h = (rc.bottom - rc.top).max(1);
    BUFFER.with(|slot| {
        let mut slot = slot.borrow_mut();
        match slot.take() {
            Some(buf) if buf.w == w && buf.h == h => Some(buf),
            _ => unsafe { DocxBuffer::new(window_dc, rc) },
        }
    })
}

fn release_docx_buffer(buf: Option<DocxBuffer>) {
    if let Some(buf) = buf {
        BUFFER.with(|slot| *slot.borrow_mut() = Some(buf));
    }
}

/// 丢掉复用的离屏缓冲（收起预览 / 切换内容时调用）。
pub fn drop_docx_buffer() {
    BUFFER.with(|slot| {
        let _ = slot.borrow_mut().take();
    });
}

// ---------------------------------------------------------------- 颜色/几何工具

/// `0xRRGGBB` → `COLORREF`（`0x00BBGGRR`，R 在最低字节）。
fn rgb_to_colorref(rgb: u32) -> COLORREF {
    let r = (rgb >> 16) & 0xFF;
    let g = (rgb >> 8) & 0xFF;
    let b = rgb & 0xFF;
    COLORREF(r | (g << 8) | (b << 16))
}

/// 连续纵向滚动的几何：fit-to-width（按内容宽度定单页缩放），多页纵向堆叠。
struct FlowGeom {
    s: f32,
    pad: i32,
    pw: i32,
    ph: i32,
    gap: i32,
    total: i32,
    view: i32,
}

/// 按"适应内容宽度"算单页缩放，把 `n` 页纵向堆叠；页面左右各留 `pad`，页间留 `gap`。
fn docx_flow_geom(page_w: u32, page_h: u32, n: usize, content: RECT) -> FlowGeom {
    let pad = dpi::px(8).max(0);
    let avail_w = ((content.right - content.left) - 2 * pad).max(1);
    let s = (avail_w as f32 / page_w.max(1) as f32).max(0.02);
    let pw = avail_w;
    let ph = ((page_h.max(1) as f32) * s).round().max(1.0) as i32;
    let gap = dpi::px(16).max(1);
    let n = n as i32;
    let total = if n <= 0 {
        0
    } else {
        2 * pad + n * ph + (n - 1) * gap
    };
    let view = (content.bottom - content.top).max(1);
    FlowGeom {
        s,
        pad,
        pw,
        ph,
        gap,
        total,
        view,
    }
}

/// 一帧的滚动态（给壳层算滚动条 + 决定要懒解码哪几页）。
pub struct DocxFlow {
    pub total_h: i32,
    pub view_h: i32,
    pub first_page: usize,
    pub last_page: usize,
    pub top_page: usize,
}

/// 给定当前纵向偏移 `view_y`，算出总高 / 视口高 / 可见页范围 / 顶部页。
pub fn docx_flow(doc: &DocDoc, content: RECT, view_y: i32) -> DocxFlow {
    let g = docx_flow_geom(doc.page_w, doc.page_h, doc.len(), content);
    let max_view = (g.total - g.view).max(0);
    let vy = view_y.clamp(0, max_view);
    let n = doc.len();
    let mut first: Option<usize> = None;
    let mut last = 0usize;
    for i in 0..n {
        let top = content.top + g.pad + (i as i32) * (g.ph + g.gap) - vy;
        let bot = top + g.ph;
        if bot > content.top && top < content.bottom {
            if first.is_none() {
                first = Some(i);
            }
            last = i;
        }
    }
    let top_page = first.unwrap_or(0);
    let (first_page, last_page) = match first {
        Some(f) => (f, last),
        None => (0, n.saturating_sub(1)),
    };
    DocxFlow {
        total_h: g.total,
        view_h: g.view,
        first_page,
        last_page,
        top_page,
    }
}

/// 让第 `page`（0 基）页顶对齐视口顶部所需的 `view_y`（页码框跳转用）。
pub fn docx_page_offset(doc: &DocDoc, content: RECT, page: usize) -> i32 {
    let g = docx_flow_geom(doc.page_w, doc.page_h, doc.len(), content);
    let max_view = (g.total - g.view).max(0);
    (g.pad + (page as i32) * (g.ph + g.gap)).clamp(0, max_view)
}

#[inline]
fn scale_px(base: i32, origin: i32, s: f32) -> i32 {
    origin + (base as f32 * s).round() as i32
}

/// 一个 run 在当前缩放下的物理字体键。
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

// ---------------------------------------------------------------- 绘制一行

/// 画一行（若干 run）：先量出每个 run 的宽度以算出对齐起点，再逐段 DrawTextW。
///
/// `align` / `avail_w` 来自外层 `Para`：模型里 `line.rect.x` 是**左边距起点**、`line.rect.w`
/// 是整行可用宽度，水平和垂直对齐要在这里按实际文字宽度补偏移。
fn draw_line(hdc: HDC, set: &mut FontMap, line: &Line, align: HAlign, ox: i32, oy: i32, s: f32) {
    if line.runs.is_empty() || line.is_empty() {
        return;
    }
    let lx = scale_px(line.rect.x, ox, s);
    let ly = scale_px(line.rect.y, oy, s);
    let lw = ((line.rect.w as f32) * s).round().max(1.0) as i32;
    let lh = ((line.rect.h as f32) * s).round().max(1.0) as i32;
    let right_limit = lx + lw;

    // 第一遍：量宽（用绘制时的同一字体，量的宽 == 画的宽，CONVENTIONS #38）。
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
        // 两端对齐这里按左对齐处理（不强求拉满，和 `qlr-render` 的注释一致）。
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
        // 高亮底色先铺，再画字（字是透明背景）。
        if let Some(hl) = run.highlight {
            let brush = unsafe { CreateSolidBrush(rgb_to_colorref(hl)) };
            let band = RECT {
                left: x,
                top: ly,
                right: (x + w).min(right_limit),
                bottom: ly + lh,
            };
            unsafe {
                FillRect(hdc, &band, brush);
                let _ = DeleteObject(brush.into());
            }
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

/// 画一组独立的行（页眉/页脚）：没有对齐信息，按左对齐、各自 `rect` 画。
fn draw_lines_free(hdc: HDC, set: &mut FontMap, lines: &[Line], ox: i32, oy: i32, s: f32) {
    for line in lines {
        draw_line(hdc, set, line, HAlign::Left, ox, oy, s);
    }
}

// ---------------------------------------------------------------- 绘制块

fn draw_block(hdc: HDC, set: &mut FontMap, block: &Block, doc: &DocDoc, ox: i32, oy: i32, s: f32) {
    match block {
        Block::Para(p) => {
            for line in &p.lines {
                draw_line(hdc, set, line, p.align, ox, oy, s);
            }
        }
        Block::Rule(r) => {
            let x1 = scale_px(r.x, ox, s);
            let x2 = scale_px(r.right(), ox, s);
            let y = scale_px(r.y, oy, s);
            let band = RECT {
                left: x1,
                top: y,
                right: x2.max(x1 + 1),
                bottom: y + (1i32).max((s).round() as i32),
            };
            let brush = unsafe { CreateSolidBrush(rgb_to_colorref(PAGE_BORDER_RGB)) };
            unsafe {
                FillRect(hdc, &band, brush);
                let _ = DeleteObject(brush.into());
            }
        }
        Block::Image(img) => draw_image(hdc, img, doc, ox, oy, s),
        Block::Table(t) => draw_table(hdc, set, t, doc, ox, oy, s),
    }
}

fn draw_table(hdc: HDC, set: &mut FontMap, table: &Table, doc: &DocDoc, ox: i32, oy: i32, s: f32) {
    for row in &table.rows {
        for cell in &row.cells {
            if cell.v_merged {
                continue;
            }
            let crect = RECT {
                left: scale_px(cell.rect.x, ox, s),
                top: scale_px(cell.rect.y, oy, s),
                right: scale_px(cell.rect.right(), ox, s),
                bottom: scale_px(cell.rect.bottom(), oy, s),
            };
            if crect.right <= crect.left || crect.bottom <= crect.top {
                continue;
            }
            // 底色
            if let Some(fill) = cell.fill {
                let brush = unsafe { CreateSolidBrush(rgb_to_colorref(fill)) };
                unsafe {
                    FillRect(hdc, &crect, brush);
                    let _ = DeleteObject(brush.into());
                }
            }
            // 内容（裁到单元格内，绝不画进邻格）
            unsafe {
                let saved = SaveDC(hdc);
                IntersectClipRect(hdc, crect.left, crect.top, crect.right, crect.bottom);
                for child in &cell.blocks {
                    draw_block(hdc, set, child, doc, ox, oy, s);
                }
                if saved != 0 {
                    let _ = RestoreDC(hdc, saved);
                }
            }
            // 网格线
            if table.borders {
                let brush = unsafe { CreateSolidBrush(rgb_to_colorref(TABLE_BORDER_RGB)) };
                unsafe {
                    let _ = FrameRect(hdc, &crect, brush);
                    let _ = DeleteObject(brush.into());
                }
            }
        }
    }
}

fn draw_image(hdc: HDC, block: &ImageBlock, doc: &DocDoc, ox: i32, oy: i32, s: f32) {
    let Some(slot) = doc.images.get(block.image as usize) else {
        return;
    };
    // 惰性槽：只有本页被应用触发解码后才有像素；没有就跳过（不画空框）。
    let Some(img) = slot.decoded() else {
        return;
    };
    if img.width == 0
        || img.height == 0
        || img.rgba.len() < (img.width as usize * img.height as usize * 4)
    {
        return;
    }
    let dx = scale_px(block.rect.x, ox, s);
    let dy = scale_px(block.rect.y, oy, s);
    let dw = ((block.rect.w as f32) * s).round().max(1.0) as i32;
    let dh = ((block.rect.h as f32) * s).round().max(1.0) as i32;

    // RGBA → BGRA（StretchDIBits 用 32 位 BI_RGB 时是小端 BGRA）。
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

/// 画 docx 的**当前纵向视口**（连续纵向滚动，多页堆叠）。
///
/// 与 `paint_sheet` 一样是"整窗双缓冲 + BeginPaint/EndPaint"的入口。多页按 fit-to-width
/// 纵向堆叠，`view_y` 是相对页面栈顶的像素偏移；只画与内容区相交的那几页。
/// 翻页/滚动只改壳层的 `view_y` 后重跑本函数，**不重新解析/排版**。
pub fn paint_page(
    hwnd: HWND,
    chrome: Option<&Chrome<'_>>,
    content: Option<RECT>,
    doc: &DocDoc,
    view_y: i32,
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

        let mut buffer = acquire_docx_buffer(window_dc, rc);
        let hdc: HDC = buffer.as_ref().map_or(window_dc, |b| b.dc);
        SetBkMode(hdc, TRANSPARENT);

        // 窗口底色（页面之外的部分）。
        let bg = CreateSolidBrush(COLORREF(TEXT_BACKGROUND_RGB));
        FillRect(hdc, &rc, bg);
        let _ = DeleteObject(bg.into());

        let after_chrome = match chrome {
            Some(spec) => paint_chrome(hdc, rc, spec),
            None => rc,
        };
        let content = content.unwrap_or(after_chrome);
        // 内容区收进客户区。
        let content = RECT {
            left: content.left.max(rc.left),
            top: content.top.max(rc.top),
            right: content.right.min(rc.right),
            bottom: content.bottom.min(rc.bottom),
        };

        let mut painted = 0usize;
        if content.right > content.left && content.bottom > content.top {
            let g = docx_flow_geom(doc.page_w, doc.page_h, doc.len(), content);
            let max_view = (g.total - g.view).max(0);
            let vy = view_y.clamp(0, max_view);
            let page_left = content.left + g.pad;
            // 页面栈整体裁到内容区，页面滚出视口的部分不会被画到顶栏/滚动条上。
            let saved = SaveDC(hdc);
            IntersectClipRect(
                hdc,
                content.left,
                content.top,
                content.right,
                content.bottom,
            );
            let white = CreateSolidBrush(COLORREF(0x00FF_FFFF));
            FONTS.with(|cell| {
                let mut set = cell.borrow_mut();
                for i in 0..doc.len() {
                    let Some(page_ref) = doc.page(i) else {
                        continue;
                    };
                    let top = content.top + g.pad + (i as i32) * (g.ph + g.gap) - vy;
                    let bot = top + g.ph;
                    if bot <= content.top || top >= content.bottom {
                        continue;
                    }
                    painted += 1;
                    let page_rect = RECT {
                        left: page_left,
                        top,
                        right: page_left + g.pw,
                        bottom: bot,
                    };
                    // 白纸
                    FillRect(hdc, &page_rect, white);
                    // 页面内容裁到页面矩形内（页眉/页脚/图片都不会越出纸面）。
                    let s2 = SaveDC(hdc);
                    IntersectClipRect(
                        hdc,
                        page_rect.left,
                        page_rect.top,
                        page_rect.right,
                        page_rect.bottom,
                    );
                    draw_lines_free(hdc, &mut set, &page_ref.header, page_left, top, g.s);
                    for block in &page_ref.blocks {
                        draw_block(hdc, &mut set, block, doc, page_left, top, g.s);
                    }
                    draw_lines_free(hdc, &mut set, &page_ref.footer, page_left, top, g.s);
                    if s2 != 0 {
                        let _ = RestoreDC(hdc, s2);
                    }
                    // 纸张边框（1px 灰线）。
                    let border = CreateSolidBrush(COLORREF(PAGE_BORDER_RGB));
                    let _ = FrameRect(hdc, &page_rect, border);
                    let _ = DeleteObject(border.into());
                }
            });
            let _ = DeleteObject(white.into());
            if saved != 0 {
                let _ = RestoreDC(hdc, saved);
            }
        }

        // 悬停提示（hint）/一次性提示（toast）是顶栏"下方、压在内容区上"的浮层，
        // 由 `paint_chrome` 画在内容之前；连续滚动的白纸会把它们盖住（用户实测"浮层沉底被挡"）。
        // 这里在内容画完之后把顶栏浮层补画一遍（标题/页码/按钮重复画一次无副作用）。
        if let Some(spec) = chrome {
            let _ = paint_chrome(hdc, rc, spec);
        }

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
                "PAINT docx view_y={view_y} painted={painted}/{} gdi={g} user={u} {:.2}ms",
                doc.len(),
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
        release_docx_buffer(buffer.take());
        let _ = EndPaint(hwnd, &ps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_to_colorref_swaps_red_and_blue() {
        // #112233 → R=0x11 G=0x22 B=0x33 → COLORREF 0x00332211
        assert_eq!(rgb_to_colorref(0x0011_2233).0, 0x0033_2211);
        assert_eq!(rgb_to_colorref(0x00FF_0000).0, 0x0000_00FF);
        assert_eq!(rgb_to_colorref(0x0000_00FF).0, 0x00FF_0000);
    }

    #[test]
    fn docx_flow_geom_fits_width_and_stacks() {
        let content = RECT {
            left: 100,
            top: 50,
            right: 800,
            bottom: 950,
        };
        // A4 页，3 页纵向堆叠。
        let g = docx_flow_geom(794, 1123, 3, content);
        // 单页宽度 = 内容宽 - 左右各 8px padding。
        assert_eq!(g.pw, 700 - 2 * dpi::px(8));
        assert!(g.s > 0.0 && g.ph > 0);
        // 3 页总高明显超过一屏 → 可以连续纵向滚动。
        assert!(
            g.total > g.view,
            "3 页总高应超过视口：total={} view={}",
            g.total,
            g.view
        );
    }

    #[test]
    fn scale_px_maps_origin_and_scales() {
        assert_eq!(scale_px(0, 100, 1.0), 100);
        assert_eq!(scale_px(50, 100, 2.0), 200);
        assert_eq!(scale_px(10, 0, 0.5), 5);
    }

    #[test]
    fn docx_flow_tracks_visible_pages() {
        use qlr_core::docx::{Block, DocDoc, HAlign, Line, Page, Para, Rect, Run};
        let mk = |t: &str| Page {
            blocks: vec![Block::Para(Para {
                rect: Rect::new(64, 64, 666, 40),
                align: HAlign::Left,
                lines: vec![Line {
                    rect: Rect::new(64, 64, 666, 40),
                    runs: vec![Run {
                        text: t.to_string(),
                        size_dpt: 200,
                        ..Run::default()
                    }],
                }],
                space_before: 0,
                space_after: 0,
            })],
            header: vec![],
            footer: vec![],
        };
        let doc = DocDoc {
            pages: vec![mk("page1"), mk("page2"), mk("page3")],
            page_w: 794,
            page_h: 1123,
            ..Default::default()
        };
        let content = RECT {
            left: 100,
            top: 50,
            right: 800,
            bottom: 450,
        };
        let f0 = docx_flow(&doc, content, 0);
        assert_eq!(f0.first_page, 0);
        assert_eq!(f0.top_page, 0);
        assert!(f0.total_h > f0.view_h, "3 页应超过一屏");
        let off1 = docx_page_offset(&doc, content, 1);
        let off2 = docx_page_offset(&doc, content, 2);
        assert!(off1 > 0 && off2 > off1, "页偏移应递增：{off1} {off2}");
        let f1 = docx_flow(&doc, content, off1);
        assert_eq!(f1.top_page, 1, "滚到第二页顶部时 top_page 应为 1");
        assert!(f1.first_page >= 1, "第一页应已离开视口");
    }
}
