//! PDF 连续纵向滚动的几何（#1 铺垫，2026-11-25）：把"每页按内容宽度 fit、多页竖向堆叠"
//! 的布局算清楚。与 `pptx.rs` 的 `pptx_flow_geom` 同一套思路——模型只给页尺寸与页数，
//! 这一层只产几何；像素由调用方按可见页范围逐页贴（PDF 是光栅页，逐页渲染成本高，
//! 故只提供纯几何，渲染与上屏的接线放在后续）。
//!
//! 坐标口径与 docx/pptx 流式一致：`view_y` 是已滚过的纵向像素；页 `i` 的顶部 =
//! `content.top + pad + i*(ph+gap) - view_y`。

use super::*;
use windows::Win32::Graphics::Gdi::{IntersectClipRect, RestoreDC, SaveDC};

/// PDF 连续滚动的几何参数（每页按内容宽度缩放，竖向堆叠）。
pub struct PdfGeom {
    /// 单页缩放比（内容可用宽 / 原始页宽）。
    pub s: f32,
    /// 四周留白（像素）。
    pub pad: i32,
    /// 单页绘制宽（= 内容可用宽）。
    pub pw: i32,
    /// 单页绘制高（按 `s` 缩放）。
    pub ph: i32,
    /// 页间距（像素）。
    pub gap: i32,
    /// 全部页堆叠总高。
    pub total: i32,
    /// 视口高。
    pub view: i32,
}

fn pdf_flow_geom(page_w: u32, page_h: u32, n: usize, content: RECT) -> PdfGeom {
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
    PdfGeom {
        s,
        pad,
        pw,
        ph,
        gap,
        total,
        view,
    }
}

/// 一帧的滚动态：总高 / 视口高 / 可见页范围 / 顶部页，供壳层算滚动条与决定懒渲染哪些页。
pub struct PdfFlow {
    pub total_h: i32,
    pub view_h: i32,
    pub first_page: usize,
    pub last_page: usize,
    pub top_page: usize,
}

/// 给定当前纵向偏移 `view_y`，算出总高 / 视口高 / 可见页范围 / 顶部页。
pub fn pdf_flow(page_w: u32, page_h: u32, n: usize, content: RECT, view_y: i32) -> PdfFlow {
    let g = pdf_flow_geom(page_w, page_h, n, content);
    let max_view = (g.total - g.view).max(0);
    let vy = view_y.clamp(0, max_view);
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
    let (first_page, last_page) = match first {
        Some(f) => (f, last),
        None => (0, n.saturating_sub(1)),
    };
    PdfFlow {
        total_h: g.total,
        view_h: g.view,
        first_page,
        last_page,
        top_page: first_page,
    }
}

/// 让第 `page`（0 基）页顶对齐视口顶部所需的 `view_y`（页码跳转用）。
pub fn pdf_page_offset(page_w: u32, page_h: u32, n: usize, content: RECT, page: usize) -> i32 {
    let g = pdf_flow_geom(page_w, page_h, n, content);
    let max_view = (g.total - g.view).max(0);
    (g.pad + (page as i32) * (g.ph + g.gap)).clamp(0, max_view)
}

/// 第 `page` 页在给定 `view_y` 下的目标绘制矩形（未裁剪；完全不可见时返回 `None`）。
/// 调用方负责把 `top/bottom` 再裁到 `content`。
pub fn pdf_page_dest(
    page_w: u32,
    page_h: u32,
    n: usize,
    content: RECT,
    page: usize,
    view_y: i32,
) -> Option<RECT> {
    let g = pdf_flow_geom(page_w, page_h, n, content);
    let max_view = (g.total - g.view).max(0);
    let vy = view_y.clamp(0, max_view);
    let top = content.top + g.pad + (page as i32) * (g.ph + g.gap) - vy;
    let bot = top + g.ph;
    if bot <= content.top || top >= content.bottom {
        return None;
    }
    Some(RECT {
        left: content.left + g.pad,
        top,
        right: content.left + g.pad + g.pw,
        bottom: bot,
    })
}

/// 把"多页 PDF 竖向堆叠"按当前 `view_y` 画到窗口：逐页贴已渲染好的位图（缺页由调用方
/// 不传该页，露出浅灰底），chrome 顶栏、滚动条 overlay 与正文同一次 `BeginPaint` 内画完，
/// 避免"正文先上屏、侧栏后补"的频闪。
///
/// `pages` 每项是 `(页号 0 基, 位图宽, 位图高, BGRA 数据)`；调用方只需传**可见页里已有位图**的项。
/// 逐页用 `IntersectClipRect` 裁到内容区，页被上/下边缘截断时源图整张贴、只显示可见部分。
#[allow(clippy::too_many_arguments)]
pub fn paint_pdf_flow(
    hwnd: HWND,
    chrome: Option<&Chrome<'_>>,
    content: RECT,
    page_w: u32,
    page_h: u32,
    total_pages: usize,
    pages: &[(usize, u32, u32, &[u8])],
    view_y: i32,
    overlay: Option<&dyn Fn(HDC, RECT)>,
) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc: HDC = BeginPaint(hwnd, &mut ps);
        if hdc.0.is_null() {
            let _ = EndPaint(hwnd, &ps);
            return;
        }
        let rc = client_rect(hwnd);
        let bg = CreateSolidBrush(COLORREF(BITMAP_BACKGROUND_RGB));
        FillRect(hdc, &rc, bg);
        let _ = DeleteObject(bg.into());

        let content = match chrome {
            Some(spec) => {
                // paint_chrome 只画顶栏、返回"顶栏以下"的整块区域（**不含**目录栏扣减）；
                // 调用方传入的 content 才是扣掉目录栏/滚动条后的真正排版区。两者相交，
                // 才能让页面画在目录栏右侧，而不是钻到面板底下被盖住（否则左侧被裁）。
                let area = paint_chrome(hdc, rc, spec);
                RECT {
                    left: area.left.max(content.left),
                    top: area.top.max(content.top),
                    right: area.right.min(content.right),
                    bottom: area.bottom.min(content.bottom),
                }
            }
            None => content,
        };
        // 页与页之间的缝隙：浅灰（PDF 页本身是白底，浅灰能把"一页的边界"显出来）。
        let gap = CreateSolidBrush(COLORREF(0x00EC_ECEC));
        FillRect(hdc, &content, gap);
        let _ = DeleteObject(gap.into());

        let saved = SaveDC(hdc);
        IntersectClipRect(
            hdc,
            content.left,
            content.top,
            content.right,
            content.bottom,
        );
        for &(page, w, h, bgra) in pages {
            if w == 0 || h == 0 || bgra.len() < (w as usize * h as usize * 4) {
                continue;
            }
            let Some(dest) = pdf_page_dest(page_w, page_h, total_pages, content, page, view_y)
            else {
                continue;
            };
            let (dw, dh) = (dest.right - dest.left, dest.bottom - dest.top);
            if dw <= 0 || dh <= 0 {
                continue;
            }
            let bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w as i32,
                    biHeight: -(h as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            // 页面被缩到内容宽（s<1），必须插值，否则缩小后走样。
            SetStretchBltMode(hdc, HALFTONE);
            let _ = SetBrushOrgEx(hdc, 0, 0, None);
            StretchDIBits(
                hdc,
                dest.left,
                dest.top,
                dw,
                dh,
                0,
                0,
                w as i32,
                h as i32,
                Some(bgra.as_ptr() as *const core::ffi::c_void),
                &bi,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }
        let _ = RestoreDC(hdc, saved);
        if let Some(draw) = overlay {
            draw(hdc, content);
        }
        let _ = EndPaint(hwnd, &ps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_flow_geom_fits_width_and_stacks() {
        let content = RECT {
            left: 100,
            top: 50,
            right: 900,
            bottom: 650,
        };
        // A4 近似 794×1123，3 页竖向堆叠。
        let g = pdf_flow_geom(794, 1123, 3, content);
        // 单页宽 = 内容宽 - 左右各 8px padding。
        assert_eq!(g.pw, 800 - 2 * dpi::px(8));
        assert!(g.s > 0.0 && g.ph > 0);
        // 3 页总高应明显超过一屏 → 可以连续纵向滚动。
        assert!(
            g.total > g.view,
            "3 页总高应超过视口：total={} view={}",
            g.total,
            g.view
        );
    }

    #[test]
    fn pdf_flow_visible_range_tracks_scroll() {
        let content = RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 600,
        };
        let n = 5;
        let f0 = pdf_flow(794, 1123, n, content, 0);
        assert_eq!(f0.first_page, 0);
        assert_eq!(f0.top_page, 0);
        assert!(f0.last_page < n);
        // 滚到底应该能看到最后一页。
        let g = pdf_flow_geom(794, 1123, n, content);
        let max_view = (g.total - g.view).max(0);
        let fend = pdf_flow(794, 1123, n, content, max_view);
        assert_eq!(fend.last_page, n - 1);
    }

    #[test]
    fn pdf_page_offset_monotonic_and_clamped() {
        let content = RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 600,
        };
        let g = pdf_flow_geom(794, 1123, 5, content);
        let max_view = (g.total - g.view).max(0);
        let o0 = pdf_page_offset(794, 1123, 5, content, 0);
        let o1 = pdf_page_offset(794, 1123, 5, content, 1);
        assert_eq!(
            o0,
            g.pad.min(max_view),
            "第 0 页偏移应为顶部留白（clamp 后）"
        );
        assert!(o1 > o0, "第 1 页偏移应大于第 0 页");
        let olast = pdf_page_offset(794, 1123, 5, content, 4);
        assert!(olast <= max_view);
    }

    #[test]
    fn pdf_page_dest_none_when_out_of_view() {
        let content = RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 600,
        };
        // 第 0 页在顶部时可见；把视口滚到接近底部，第 0 页应移出。
        let g = pdf_flow_geom(794, 1123, 5, content);
        let max_view = (g.total - g.view).max(0);
        assert!(pdf_page_dest(794, 1123, 5, content, 0, 0).is_some());
        assert!(pdf_page_dest(794, 1123, 5, content, 0, max_view).is_none());
    }
}
