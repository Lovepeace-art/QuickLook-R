//! 位图绘制 —— 从 window.rs 纯搬家拆出（2026-11-04）。
//!
//! 每块只多一行 `use super::*;`，函数体一字未改。

use super::*;

/// 把整张位图**等比拉伸**铺进客户区（1.0.5 的缩放预览）。
///
/// 为什么需要它：缩放时窗口尺寸是立刻能改的，但"按新尺寸重新光栅化"是要花时间的
/// （PDF 一页几十毫秒，拖动时每一帧都重渲染就会一顿一顿）。所以拖动期间先把**旧位图
/// 拉伸**铺上新窗口——画面立刻跟着手走，松手后再渲染一张清晰的换上。
/// 拉伸只是画一次，GDI 由显卡搬，不占渲染线程。
/// 把一张 BGRA 位图**等比**铺进内容区（1.7.8，图片专用）。
///
/// 为什么需要：用户要求"图片缩放定死长宽比，只支持等比缩放"——之前窗口一拉宽，
/// 画面就被横向拉伸（人物变胖）。这里按 `min(cw/w, ch/h)` 算缩放比、居中摆放，
/// 四周留我们的底色（图片本来就有留白，观感上是"适应窗口"而不是"黑边"）。
pub fn paint_bitmap_fit(
    hwnd: HWND,
    width: u32,
    height: u32,
    bgra: &[u8],
    chrome: Option<&Chrome<'_>>,
    reserve_bar: bool,
) {
    if width == 0 || height == 0 || bgra.len() < (width as usize * height as usize * 4) {
        return;
    }
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc: HDC = BeginPaint(hwnd, &mut ps);
        if hdc.0.is_null() {
            let _ = EndPaint(hwnd, &ps);
            return;
        }
        let rc = client_rect(hwnd);
        let brush = CreateSolidBrush(COLORREF(BITMAP_BACKGROUND_RGB));
        FillRect(hdc, &rc, brush);
        let _ = DeleteObject(brush.into());

        let content = match chrome {
            Some(spec) => paint_chrome(hdc, rc, spec),
            None => rc,
        };
        // 视频底部要留出进度条：不留的话每一帧都会把那条盖掉（1.7.9 实测）。
        let content = if reserve_bar {
            picture_area(content)
        } else {
            content
        };
        let (cw, ch) = (content.right - content.left, content.bottom - content.top);
        if cw <= 0 || ch <= 0 {
            let _ = EndPaint(hwnd, &ps);
            return;
        }
        paint_bitmap_fit_dc(hdc, content, width, height, bgra);
        let _ = EndPaint(hwnd, &ps);
    }
}

/// 把一张 BGRA 位图等比贴进窗口的内容区（1.7.9：视频逐帧播放用）。
///
/// 和 [`paint_bitmap_fit`] 的区别：这里**不开 `BeginPaint`**，直接 `GetDC` 画完就还。
/// 播放时一秒钟要画二十几帧，走 `BeginPaint/EndPaint` 会把 `WM_PAINT` 的顺序搅乱
/// （画完不 `EndPaint` 就等于没确认更新区）。底色先铺一遍，避免等比缩放留下的
/// 四周空白处残留上一帧的像素。
pub fn paint_bitmap_fit_window(hwnd: HWND, area: RECT, width: u32, height: u32, bgra: &[u8]) {
    if width == 0 || height == 0 || bgra.len() < (width as usize * height as usize * 4) {
        return;
    }
    unsafe {
        let hdc: HDC = GetDC(Some(hwnd));
        if hdc.0.is_null() {
            return;
        }
        let brush = CreateSolidBrush(COLORREF(BITMAP_BACKGROUND_RGB));
        FillRect(hdc, &area, brush);
        let _ = DeleteObject(brush.into());
        paint_bitmap_fit_dc(hdc, area, width, height, bgra);
        ReleaseDC(Some(hwnd), hdc);
    }
}

/// 把一张 BGRA 位图等比铺进 `area`（1.7.9：视频逐帧播放要直接画进内存/窗口 DC）。
///
/// 和 [`paint_bitmap_fit`] 同一个口径（`min(cw/w, ch/h)` + 居中），只是不自取 DC、
/// 不开 `BeginPaint`——播放时每帧只有这一块要更新，从 WM_TIMER 里 GetDC 画完即走。
pub fn paint_bitmap_fit_dc(hdc: HDC, area: RECT, width: u32, height: u32, bgra: &[u8]) {
    if width == 0 || height == 0 || bgra.len() < (width as usize * height as usize * 4) {
        return;
    }
    let (cw, ch) = (area.right - area.left, area.bottom - area.top);
    if cw <= 0 || ch <= 0 {
        return;
    }
    unsafe {
        let scale = (cw as f32 / width as f32).min(ch as f32 / height as f32);
        let (dw, dh) = (
            ((width as f32 * scale).round() as i32).max(1),
            ((height as f32 * scale).round() as i32).max(1),
        );
        let dx = area.left + (cw - dw) / 2;
        let dy = area.top + (ch - dh) / 2;

        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
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
            width as i32,
            height as i32,
            Some(bgra.as_ptr() as *const core::ffi::c_void),
            &bi,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
    }
}

pub fn paint_bitmap_scaled(
    hwnd: HWND,
    width: u32,
    height: u32,
    bgra: &[u8],
    chrome: Option<&Chrome<'_>>,
) {
    if width == 0 || height == 0 || bgra.len() < (width as usize * height as usize * 4) {
        return;
    }
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc: HDC = BeginPaint(hwnd, &mut ps);
        if hdc.0.is_null() {
            let _ = EndPaint(hwnd, &ps);
            return;
        }

        let rc = client_rect(hwnd);
        let brush = CreateSolidBrush(COLORREF(BITMAP_BACKGROUND_RGB));
        FillRect(hdc, &rc, brush);
        let _ = DeleteObject(brush.into());

        let content = match chrome {
            Some(spec) => paint_chrome(hdc, rc, spec),
            None => rc,
        };
        let (cw, ch) = (content.right - content.left, content.bottom - content.top);
        if cw <= 0 || ch <= 0 {
            let _ = EndPaint(hwnd, &ps);
            return;
        }
        // 直接铺满内容区：预览帧本来就是"等清晰帧"的临时替代，宁可差一点比例，
        // 也不能在四周露出一圈底色（那和"黑边"是同一种观感缺陷）。
        let (dw, dh) = (cw, ch);
        let (dx, dy) = (content.left, content.top);

        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        // 1:1 铺满（缩放预览帧尺寸正好等于内容区）就不需要插值：COLORONCOLOR 快得多。
        if dw == width as i32 && dh == height as i32 {
            SetStretchBltMode(hdc, COLORONCOLOR);
        } else {
            SetStretchBltMode(hdc, HALFTONE);
            let _ = SetBrushOrgEx(hdc, 0, 0, None);
        }
        StretchDIBits(
            hdc,
            dx,
            dy,
            dw,
            dh,
            0,
            0,
            width as i32,
            height as i32,
            Some(bgra.as_ptr() as *const core::ffi::c_void),
            &bi,
            DIB_RGB_COLORS,
            SRCCOPY,
        );

        let _ = EndPaint(hwnd, &ps);
    }
}

/// 把一张 BGRA 位图等比居中贴到窗口客户区（不放大）。
///
/// 用 `StretchDIBits` 而不是 D3D11：这是 A8.5 的装载裁剪选择——零额外 dll、零 GPU 依赖，
/// 大图拖动掉帧的问题等 T2（A9.1）再换 flip 模型。`HALFTONE` + `SetBrushOrgEx(0,0)`
/// 是 `StretchDIBits` 的经典搭配，否则缩放会出现条纹。
pub fn paint_bitmap(hwnd: HWND, width: u32, height: u32, bgra: &[u8]) {
    paint_bitmap_at(hwnd, width, height, bgra, 0, 0, None);
}

/// 画位图，但按 offset 裁剪左上角（放大后用方向键平移时用）。
///
/// 语义：
/// - 位图某一维**不大于**客户区 → 该维铺满（和 fit 行为一致，留白是底色）；
/// - 位图某一维**大于**客户区 → 从 offset 处开始裁出客户区大小的一块，
///   offset 会被夹到 [0, 位图尺寸 - 客户区尺寸]，所以平移不会拖出黑边。
///
/// `header` 有值时先在顶部画一条状态条（PDF 显示页码），内容区从状态条下方开始。
///
/// 放大时**不改绘制缩放**，而是让渲染线程按放大后的像素尺寸重新出图（1:1 上屏），
/// 这样放大看到的是重新光栅化的清晰图，不是把位图拉伸。
pub fn paint_bitmap_at(
    hwnd: HWND,
    width: u32,
    height: u32,
    bgra: &[u8],
    offset_x: i32,
    offset_y: i32,
    chrome: Option<&Chrome<'_>>,
) {
    if width == 0 || height == 0 || bgra.len() < (width as usize * height as usize * 4) {
        return;
    }
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc: HDC = BeginPaint(hwnd, &mut ps);
        if hdc.0.is_null() {
            let _ = EndPaint(hwnd, &ps);
            return;
        }

        let rc = client_rect(hwnd);
        // 显示位图时亚克力是关掉的，而窗口类背景刷是空的（为了在文字状态下透出模糊），
        // 所以这里必须自己刷一层实色背景，否则图片四周的留白会露出桌面。
        let brush = CreateSolidBrush(COLORREF(BITMAP_BACKGROUND_RGB));
        FillRect(hdc, &rc, brush);
        let _ = DeleteObject(brush.into());

        let content = match chrome {
            Some(spec) => paint_chrome(hdc, rc, spec),
            None => rc,
        };
        let (cw, ch) = (content.right - content.left, content.bottom - content.top);
        let (bw, bh) = (width as i32, height as i32);

        // 每一维独立决策：比客户区大就裁（可平移），小就**拉伸铺满**。
        //
        // 为什么要铺满而不是居中：渲染器按内容的宽高比取整，出来的位图经常比客户区
        // 少 1 px（实测 `render=734x1040` 却是 `bitmap 734x1039`）。居中绘制会在
        // 上下各留半像素，合成一条深色细线——用户看到的就是"pdf 缩小后有一点黑边"。
        // 差 1 px 的拉伸肉眼看不出来，却能把那条线彻底抹掉。
        //
        // 1.8.3：但"少一大截"是另一回事。PDF 页面有自己的宽高比，比内容区矮几十像素
        // 是**正常留白**；把它拉伸到铺满则是每帧整张重采样（HALFTONE），页面还会
        // 被纵向拉长几毫米。所以只有"差几像素"才铺满，差得多就居中 1:1（不重采样）。
        const FILL_SLACK: i32 = 4;
        let (dw, dx, sx, sw) = if bw > cw {
            let pan = offset_x.clamp(0, bw - cw);
            // 坑：本函数的 DIB 是 top-down（biHeight 取负），但 StretchDIBits 的
            // **源矩形 y 仍然从位图底部量起**。直接把"从顶部算的偏移"塞进去会取到
            // 图像的另一端——实测表现是"放大后窗口里一片纯白"（其实画的是页面底部
            // 的空白），平移几步才偶然露出一点字。所以这里换算成从底部量。
            (cw, content.left, bw - cw - pan, cw)
        } else if cw - bw > FILL_SLACK {
            // 居中 1:1：目的尺寸 = 源尺寸，不插值。
            (bw, content.left + (cw - bw) / 2, 0, bw)
        } else {
            (cw, content.left, 0, bw)
        };
        let (dh, dy, sy, sh) = if bh > ch {
            let pan = offset_y.clamp(0, bh - ch);
            (ch, content.top, bh - ch - pan, ch)
        } else if ch - bh > FILL_SLACK {
            (bh, content.top + (ch - bh) / 2, 0, bh)
        } else {
            (ch, content.top, 0, bh)
        };

        // biHeight 取负 = top-down DIB（第一行就是图像顶行），和我们的 BGRA 缓冲顺序一致。
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        // 1:1 拷贝不需要插值（拖动/平移每帧都走这里，HALFTONE 会明显发卡）。
        // 顺带：位图和内容区只差几个像素（"页面比例所限"的留白填充、按宽高比取整）
        // 也不值得上 HALFTONE——3% 以内的补边肉眼分不出，HALFTONE 却要整张重采样一遍，
        // 每一帧都来一次就是"滚动频闪 + 拖动掉帧"。真正需要插值的是缩放预览/缩放视频，
        // 那种差距远超 3%。
        let near = (dw - sw).abs() * 32 <= cw.abs() && (dh - sh).abs() * 32 <= ch.abs();
        if near {
            SetStretchBltMode(hdc, COLORONCOLOR);
        } else {
            SetStretchBltMode(hdc, HALFTONE);
            let _ = SetBrushOrgEx(hdc, 0, 0, None);
        }
        StretchDIBits(
            hdc,
            dx,
            dy,
            dw,
            dh,
            sx,
            sy,
            sw,
            sh,
            Some(bgra.as_ptr() as *const core::ffi::c_void),
            &bi,
            DIB_RGB_COLORS,
            SRCCOPY,
        );

        let _ = EndPaint(hwnd, &ps);
    }
}
