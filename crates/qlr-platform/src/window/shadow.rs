//! 阴影窗口 —— 从 window.rs 纯搬家拆出（2026-11-04）。
//!
//! 每块只多一行 `use super::*;`，函数体一字未改。

use super::*;

/// 创建阴影窗口（1.0.10）：`WS_EX_LAYERED` 的透明浮层，永远贴在预览窗**下面**。
///
/// 为什么要单独一个窗口：`CS_DROPSHADOW` 只有一条很硬的 1 px 边、深色桌面上还看不见；
/// 而我们的预览窗是 `WS_POPUP` + `WM_NCCALCSIZE` 去边框，DWM 不会给它投阴影。
/// 分层窗口（`UpdateLayeredWindow` + 逐像素 alpha）能画出和网页 `box-shadow`
/// 一样的柔和外发光，且 `WS_EX_TRANSPARENT` 保证它不吃鼠标点击。
pub fn create_shadow_window(class: &str, title: &str) -> windows::core::Result<HWND> {
    const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
    const WS_EX_TOPMOST: u32 = 0x0000_0008;
    const WS_EX_TRANSPARENT: u32 = 0x0000_0020;
    const WS_EX_LAYERED: u32 = 0x0008_0000;
    const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
    create_window(
        class,
        title,
        WINDOW_EX_STYLE(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_TRANSPARENT | WS_EX_LAYERED | WS_EX_NOACTIVATE,
        ),
        WINDOW_STYLE(0x8000_0000), // WS_POPUP
    )
}

/// 投影的外扩宽度（像素）：阴影画在窗口外面这么宽的一圈里。
pub fn shadow_margin() -> i32 {
    dpi::px(16)
}

/// 投影最深处的 alpha（0–255）。太黑会像"贴了张黑纸"，80–110 附近最像 CSS 阴影。
pub const SHADOW_ALPHA: f32 = 96.0;

/// 让阴影窗口跟随目标窗口：位置/尺寸同步，尺寸变了才重画那张 alpha 图。
///
/// 调用点：显示预览后、拖动窗口后、拖边改大小过程中（每帧只做 `SetWindowPos`）。
pub fn sync_shadow(shadow: HWND, target: HWND) {
    // 拖动/改尺寸期间投影必须一直藏着（1.4.10）：以前只在 app 层判断，
    // 但 resize / chromeless 拖拽路径会绕过它，于是"藏一次、亮一次"。
    if SHADOW_SUPPRESSED.load(std::sync::atomic::Ordering::Relaxed) {
        unsafe {
            let _ = ShowWindow(shadow, windows::Win32::UI::WindowsAndMessaging::SW_HIDE);
        }
        return;
    }
    let mut rc = RECT::default();
    unsafe {
        if GetWindowRect(target, &mut rc).is_err() {
            return;
        }
    }
    let width = (rc.right - rc.left) + shadow_margin() * 2;
    let height = (rc.bottom - rc.top) + shadow_margin() * 2;
    if width <= 0 || height <= 0 {
        return;
    }
    unsafe {
        // 先摆位置（同时把它压到 target 下面：hWndInsertAfter = target）。
        let _ = SetWindowPos(
            shadow,
            Some(target),
            rc.left - shadow_margin(),
            rc.top - shadow_margin(),
            width,
            height,
            SWP_NOACTIVATE,
        );
    }
    paint_shadow(shadow, width, height);
    unsafe {
        let _ = ShowWindow(shadow, SW_SHOWNOACTIVATE);
    }
}

/// 把阴影画进分层窗口（缓存：尺寸没变就不重建位图，避免拖动时每帧算几万像素）。
fn paint_shadow(shadow: HWND, width: i32, height: i32) {
    thread_local! {
        static LAST: std::cell::Cell<(i32, i32)> = const { std::cell::Cell::new((0, 0)) };
    }
    let rebuild = LAST.with(|last| {
        let cached = last.get();
        if cached == (width, height) {
            false
        } else {
            last.set((width, height));
            true
        }
    });
    if !rebuild {
        return;
    }

    // 逐像素预乘 alpha 的 32 位位图：中间挖空（被预览窗盖住）+ 向内 16 px 柔和衰减。
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    let m = shadow_margin() as f32;
    for y in 0..height {
        for x in 0..width {
            // 到"内框"（= 预览窗）的带符号距离：框内为负，框外为正式像素数。
            let dx = (m - x as f32).max(x as f32 - (width as f32 - m));
            let dy = (m - y as f32).max(y as f32 - (height as f32 - m));
            let d = dx.max(dy);
            if d < 0.0 {
                continue; // 框架内部：完全透明（被预览窗挡住，画了也浪费）
            }
            // 平滑衰减：贴边最浓，往外 16 px 线性降到 0（平方让边缘更柔）。
            let t = (1.0 - d / m).clamp(0.0, 1.0);
            let a = (SHADOW_ALPHA * t * t) as u8;
            if a == 0 {
                continue;
            }
            let i = ((y * width + x) * 4) as usize;
            // 预乘：B,G,R = 0（纯黑阴影），A = a。
            pixels[i + 3] = a;
        }
    }

    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = width;
        info.bmiHeader.biHeight = -height; // 负数 = top-down，行序与我们的数组一致
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB.0;
        let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
        let Ok(bitmap) = CreateDIBSection(Some(mem), &info, DIB_RGB_COLORS, &mut bits, None, 0)
        else {
            let _ = DeleteDC(mem);
            let _ = ReleaseDC(None, screen);
            return;
        };
        if bits.is_null() {
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(mem);
            let _ = ReleaseDC(None, screen);
            return;
        }
        core::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u8, pixels.len());
        let old = SelectObject(mem, bitmap.into());
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let size = SIZE {
            cx: width,
            cy: height,
        };
        let src = POINT::default();
        let _ = UpdateLayeredWindow(
            shadow,
            Some(screen),
            // pptDst 传 None：位置已经由 `SetWindowPos` 摆好了。
            // 传 `Some(&POINT::default())` 会把窗口挪到 (0,0)——踩过，
            // 表现是"阴影跑到屏幕左上角、窗口下面光秃秃"。
            None,
            Some(&size),
            Some(mem),
            Some(&src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
        SelectObject(mem, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(mem);
        let _ = ReleaseDC(None, screen);
    }
}

/// 隐藏阴影（预览窗收起时必须跟着收，否则桌面上留一块灰）。
/// 投影是否被临时禁用（拖动/改尺寸期间）。`sync_shadow` 会读它，禁用时只隐藏、不显示。
static SHADOW_SUPPRESSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 拖动/改尺寸开始：`sync_shadow` 从此只隐藏、不再把投影亮起来。
pub fn suppress_shadow() {
    SHADOW_SUPPRESSED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// 拖动/改尺寸结束：允许投影重新出现（调用方随后应 `sync_shadow` 一次对齐）。
pub fn allow_shadow() {
    SHADOW_SUPPRESSED.store(false, std::sync::atomic::Ordering::Relaxed);
}

pub fn hide_shadow(shadow: HWND) {
    unsafe {
        let _ = ShowWindow(shadow, windows::Win32::UI::WindowsAndMessaging::SW_HIDE);
    }
}
