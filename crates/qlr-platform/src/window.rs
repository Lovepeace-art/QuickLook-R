//! 窗口：类注册、创建、显示/隐藏、绘制、消息泵。
//!
//! 这里有两条必须守住的规矩（都来自 M0 spike 的实测）：
//! 1. **预览窗口绝不能抢焦点**：`WS_EX_NOACTIVATE` + `SetWindowPos(SWP_NOACTIVATE)`，
//!    否则用户按空格弹窗后，焦点从资源管理器跑到预览窗口，再按空格就不是"关闭"了。
//! 2. **`WM_NCCALCSIZE` 返回 0 取代 `WS_THICKFRAME`**：这是去掉系统边框/标题栏
//!    又保留 DWM 阴影与亚克力的标准做法（我们的 M0 spike 就是这么做的）。

use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, BeginPaint,
    BitBlt, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, COLORONCOLOR, CreateCompatibleBitmap,
    CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DEFAULT_CHARSET,
    DEFAULT_GUI_FONT, DEFAULT_PITCH, DIB_RGB_COLORS, DRAW_TEXT_FORMAT, DT_CENTER, DT_END_ELLIPSIS,
    DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject, DrawTextW,
    EndPaint, FW_BOLD, FW_NORMAL, FillRect, FrameRect, GetDC, GetMonitorInfoW, GetStockObject,
    GetTextExtentPoint32W, HALFTONE, HBITMAP, HBRUSH, HDC, HFONT, HGDIOBJ, InvalidateRect,
    MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, OUT_TT_PRECIS, PAINTSTRUCT,
    RDW_ALLCHILDREN, RDW_INVALIDATE, RDW_UPDATENOW, RedrawWindow, ReleaseDC, SRCCOPY, SelectObject,
    SetBkMode, SetBrushOrgEx, SetStretchBltMode, SetTextColor, StretchBlt, StretchDIBits,
    TRANSPARENT,
};
use windows::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_NODEFAULT, SND_SYSTEM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, ReleaseCapture, SetCapture, VK_SHIFT,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumChildWindows,
    GetClientRect, GetCursorPos, GetMessageW, GetSystemMetrics, GetWindowRect, HWND_NOTOPMOST,
    HWND_TOP, HWND_TOPMOST, IDC_ARROW, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE,
    IDI_APPLICATION, IsWindow, IsWindowVisible, KillTimer, LoadCursorW, LoadIconW, MB_ICONWARNING,
    MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MSG, MessageBoxW, PostMessageW, RegisterClassW,
    SM_CXSCREEN, SM_CYSCREEN, SW_SHOW, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SWP_SHOWWINDOW, SendMessageW, SetCursor, SetForegroundWindow, SetTimer,
    SetWindowPos, ShowWindow, TranslateMessage, ULW_ALPHA, UpdateLayeredWindow, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_KEYDOWN, WM_KEYUP, WM_MOUSEWHEEL, WM_SETREDRAW, WNDCLASS_STYLES, WNDCLASSW,
    WNDPROC, WS_CHILD, WS_VISIBLE, WindowFromPoint,
};
use windows::core::{BOOL, PCWSTR};

// `windows` 0.62 没给 `EnableWindow` 生成绑定（只需要这一个，自己声明）。
#[link(name = "user32")]
unsafe extern "system" {
    fn EnableWindow(hwnd: HWND, enabled: i32) -> i32;
}

use crate::wide::to_wide;

// 1.8.3：几何量一律写 DIP，用 `dpi::px()` 换到物理像素。这个模块里用得最密，
// 直接 use 进来，免得每处写全路径。
use crate::dpi;

/// 弹一个原生告警框。
///
/// 只用于"程序还能跑，但用户必须知道某件事"的场景（例如托盘加不上 —— 否则程序
/// 会静默常驻后台、用户找不到入口，以为它没启动）。**不要**用它做正常交互。
///
/// `MB_TOPMOST | MB_SETFOREGROUND` 是为了让它在后台进程里也能冒到前面来
/// （我们的进程没有前台窗口，不加这两个标志可能被压在其他窗口后面）。
pub fn warn_box(title: &str, text: &str) {
    let title_w = to_wide(title);
    let text_w = to_wide(text);
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text_w.as_ptr()),
            PCWSTR(title_w.as_ptr()),
            MB_OK | MB_ICONWARNING | MB_TOPMOST | MB_SETFOREGROUND,
        );
    }
}

/// 位图模式下的窗口底色（`0x00RRGGBB`）。图片按比例缩放后四周的留白用这个颜色。
///
/// 为什么这里必须自己刷背景：窗口类的 `hbrBackground` 是空的（文字状态下要靠这个
/// 透出亚克力的模糊），一旦关掉亚克力改成不透明，留白处就会露出桌面内容。
///
/// 1.0.8 起改成**白色**（用户要求"所有文件默认打开，底色都是白色"）——PDF 页面本身
/// 是白纸，深色留白会显得脏；图片的留白用白色也不抢画面。
pub const BITMAP_BACKGROUND_RGB: u32 = 0x00FF_FFFF;

/// 进程的 `HINSTANCE`（`GetModuleHandleW(NULL)` 就是当前 exe）。
pub fn instance_handle() -> HINSTANCE {
    unsafe {
        GetModuleHandleW(None)
            .map(|m: HMODULE| HINSTANCE(m.0))
            .unwrap_or_default()
    }
}

/// 注册窗口类。同名类重复注册会失败，调用方应只注册一次。
pub fn register_class(name: &str, wndproc: WNDPROC) -> windows::core::Result<()> {
    unsafe {
        let class = to_wide(name);
        let wc = WNDCLASSW {
            // 不要 CS_HREDRAW / CS_VREDRAW：
            // 这两个标志的意思是"窗口一变大小/一移动就整窗重画"。我们的客户区是一张几 MB 的位图，
            // 拖窗口时每 1 px 都整幅重画一次 → 窗口像在慢动作里追鼠标（1.4.1 修）。
            // 需要重画的时机我们自己控制（显示/缩放/松手补渲染时都有显式 invalidate）。
            style: WNDCLASS_STYLES(0),
            lpfnWndProc: wndproc,
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance_handle(),
            hIcon: LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            // 不给背景刷：亚克力要透出 DWM 模糊，客户区不能整片填色。
            hbrBackground: HBRUSH::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR(class.as_ptr()),
        };
        if RegisterClassW(&wc) == 0 {
            return Err(windows::core::Error::from_thread());
        }
        Ok(())
    }
}

/// 创建不可见的宿主窗口（承载托盘回调与 `WM_COPYDATA`；永不显示，所以不出现在任务栏）。
pub fn create_host_window(class: &str, title: &str) -> windows::core::Result<HWND> {
    create_window(
        class,
        title,
        WINDOW_EX_STYLE(0x0000_0080), // WS_EX_TOOLWINDOW
        WINDOW_STYLE(0),
    )
}

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

/// 创建预览窗口（popup + 不激活 + 置顶 + 工具窗口）。
pub fn create_preview_window(class: &str, title: &str) -> windows::core::Result<HWND> {
    const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
    const WS_EX_TOPMOST: u32 = 0x0000_0008;
    const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
    create_window(
        class,
        title,
        WINDOW_EX_STYLE(WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE),
        // `WS_CLIPCHILDREN`（1.7.4）：系统预览处理器的内容是一个**子窗口**，
        // 没有这个样式，我们自己整窗填底色的那一下会把子窗口的像素擦掉 —— 表现是
        // "PPT 翻页后画面还停在第 1 页、只零星重画几块"（子窗口没被 invalidate，
        // 自然也不会重画）。加上之后父窗口的绘制不再碰子窗口区域。
        WINDOW_STYLE(0x8000_0000 | 0x0200_0000), // WS_POPUP | WS_CLIPCHILDREN
    )
}

/// 创建设置窗口（1.0.7）：**普通可激活窗口**（带标题栏、能收键盘）。
///
/// 和预览窗刻意相反：预览窗 `WS_EX_NOACTIVATE` 是为了不抢资源管理器的焦点，
/// 而设置页必须拿焦点才能"点一行、直接按新键"。样式用 `WS_OVERLAPPED | WS_CAPTION |
/// WS_SYSMENU`（0x00C00000 | 0x00080000 | 0x00080000 的常见组合）：
/// 有标题栏和关闭按钮，但不允许改大小（设置项是固定行数的两列表格）。
pub fn create_settings_window(class: &str, title: &str) -> windows::core::Result<HWND> {
    const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
    const WS_CAPTION: u32 = 0x00C0_0000; // WS_BORDER | WS_DLGFRAME
    const WS_SYSMENU: u32 = 0x0008_0000;
    create_window(
        class,
        title,
        WINDOW_EX_STYLE(WS_EX_TOOLWINDOW),
        WINDOW_STYLE(WS_CAPTION | WS_SYSMENU),
    )
}

fn create_window(
    class: &str,
    title: &str,
    ex_style: WINDOW_EX_STYLE,
    style: WINDOW_STYLE,
) -> windows::core::Result<HWND> {
    let class = to_wide(class);
    let title = to_wide(title);
    unsafe {
        CreateWindowExW(
            ex_style,
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            style,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance_handle()),
            None,
        )
    }
}

/// 显示并定位窗口。**不激活**：焦点留在资源管理器。
pub fn show_at(hwnd: HWND, rect: RECT, topmost: bool) {
    let width = (rect.right - rect.left).max(1);
    let height = (rect.bottom - rect.top).max(1);
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(if topmost {
                HWND_TOPMOST
            } else {
                HWND_NOTOPMOST
            }),
            rect.left,
            rect.top,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, false);
    }
}

/// 光标位置（**屏幕坐标**）。
///
/// 拖动必须用它，不能用 `WM_MOUSEMOVE` 的 client 坐标：拖动过程中我们自己在挪窗口，
/// client 坐标会被"窗口也走了"抵消掉——实测鼠标走 90 px、窗口只走 27 px（约 1/3），
/// 因为每挪一次窗口，下一次的 client 坐标就退回原点附近。
pub fn cursor_pos() -> Option<(i32, i32)> {
    let mut p = POINT::default();
    unsafe {
        GetCursorPos(&mut p).ok()?;
    }
    Some((p.x, p.y))
}

/// 把窗口整体挪动 (dx, dy)——左键拖动用的就是它。
///
/// 为什么不挪**内容**（改绘制偏移）：拖动每帧都要重画一整张位图，
/// 大图一帧 10 ms 以上、DWM 会在半成品状态取一帧，用户看到的就是频闪。
/// 挪窗口是由桌面合成器搬现成的表面，几乎零成本，也不产生任何重绘。
pub fn move_by(hwnd: HWND, dx: i32, dy: i32) {
    if dx == 0 && dy == 0 {
        return;
    }
    let mut rc = RECT::default();
    unsafe {
        if GetWindowRect(hwnd, &mut rc).is_err() {
            return;
        }
        let _ = SetWindowPos(
            hwnd,
            None,
            rc.left + dx,
            rc.top + dy,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

pub fn hide(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, windows::Win32::UI::WindowsAndMessaging::SW_HIDE);
    }
}

/// 把窗口改成新的矩形（拖边改大小用）。
///
/// 用 `SetWindowPos` 而不是 `MoveWindow`：前者不碰 z 序、也不激活窗口
/// （预览窗是 `WS_EX_NOACTIVATE`，一动焦点就跑到它身上，空格就不灵了）。
pub fn resize_to(hwnd: HWND, left: i32, top: i32, width: i32, height: i32) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            left,
            top,
            width.max(1),
            height.max(1),
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// 窗口矩形（**屏幕坐标**）。无边框窗口下它与客户区同尺寸。
pub fn window_rect(hwnd: HWND) -> RECT {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut rc);
    }
    rc
}

/// 鼠标指针形状。
///
/// 窗口无边框、也没 `WS_THICKFRAME`，所以"边缘能拖"这件事系统完全不知道，
/// 光标必须自己换——否则用户看不出哪里可以拖。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorKind {
    Arrow,
    SizeNS,
    SizeWE,
    SizeNWSE,
    SizeNESW,
}

/// 立刻换光标（`WM_SETCURSOR` 里调；返回 true 表示已处理）。
pub fn set_cursor(kind: CursorKind) -> bool {
    let id = match kind {
        CursorKind::Arrow => IDC_ARROW,
        CursorKind::SizeNS => IDC_SIZENS,
        CursorKind::SizeWE => IDC_SIZEWE,
        CursorKind::SizeNWSE => IDC_SIZENWSE,
        CursorKind::SizeNESW => IDC_SIZENESW,
    };
    unsafe {
        match LoadCursorW(None, id) {
            Ok(cursor) => {
                SetCursor(Some(cursor));
                true
            }
            Err(_) => false,
        }
    }
}

/// 系统提示音（1.0.10：不支持的文件不再画任何东西，只响一声）。
///
/// 用 `PlaySoundW` + `SND_ALIAS` 播系统"错误提示"音，而不是 `MessageBox`：
/// 用户要的是"响铃拒绝"，弹框会打断资源管理器里的选中流程。
/// `windows` crate 没有导出 `MessageBeep`，这也正是改走 PlaySound 的原因。
pub fn beep() {
    let alias = to_wide("SystemHand");
    unsafe {
        let _ = PlaySoundW(
            PCWSTR(alias.as_ptr()),
            None,
            SND_ALIAS | SND_ASYNC | SND_NODEFAULT | SND_SYSTEM,
        );
    }
}

/// 启动/重启一个定时器（跟随选中项轮询用）。
pub fn start_timer(hwnd: HWND, id: usize, ms: u32) {
    unsafe {
        let _ = SetTimer(Some(hwnd), id, ms, None);
    }
}

/// 停掉定时器。
pub fn stop_timer(hwnd: HWND, id: usize) {
    unsafe {
        let _ = KillTimer(Some(hwnd), id);
    }
}

/// 窗口最外圈的**发丝边**（1.0.10 定稿）。
///
/// 柔和投影由独立的阴影窗口（[`sync_shadow`]）负责；这里只画 1 px 浅灰，
/// 负责"边界清楚"——纯白窗口贴在纯白背景上时，光靠投影还是会糊成一片。
/// 早先试过三圈由深到浅的灰边，看起来像"贴了三条胶带"，用户明确否掉了。
pub fn paint_window_frame(hwnd: HWND) {
    unsafe {
        let hdc = GetDC(Some(hwnd));
        if hdc.0.is_null() {
            return;
        }
        let rc = client_rect(hwnd);
        frame_solid(hdc, rc, FRAME_HAIRLINE_RGB);
        // DC 由调用方持有/释放（1.5.2）
    }
}

/// 显示并激活窗口（设置页专用：要拿键盘焦点才能改键）。
///
/// 和预览窗的 `show_at` 刻意不同：预览窗永远 `SW_SHOWNOACTIVATE`，
/// 因为它不该抢走资源管理器的焦点；设置页相反，必须能收到按键。
pub fn show_foreground(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
}

/// 窗口句柄是否仍然有效（设置页重复打开时要先问一句，避免对着已销毁的句柄发消息）。
pub fn is_window(hwnd: HWND) -> bool {
    unsafe { IsWindow(Some(hwnd)).as_bool() }
}

/// 销毁窗口（设置页关窗用）。
pub fn destroy(hwnd: HWND) {
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

/// 把窗口的**客户区**尺寸设成给定值（自动补上标题栏/边框的厚度）。
///
/// 设置页的行数决定内容高度，只有按客户区算才不会出现"最后一行被裁掉半截"。
/// 返回值是实际设置的窗口矩形（失败时返回零矩形）。
pub fn set_client_size(hwnd: HWND, client_width: i32, client_height: i32) -> RECT {
    let mut win = RECT::default();
    let mut cli = RECT::default();
    unsafe {
        if GetWindowRect(hwnd, &mut win).is_err() || GetClientRect(hwnd, &mut cli).is_err() {
            return RECT::default();
        }
        let extra_w = (win.right - win.left) - (cli.right - cli.left);
        let extra_h = (win.bottom - win.top) - (cli.bottom - cli.top);
        let width = client_width + extra_w;
        let height = client_height + extra_h;
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            width,
            height,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        let _ = GetWindowRect(hwnd, &mut win);
    }
    win
}

/// 取消窗口的最小化/最大化限制没什么可做的；这里只把窗口摆到工作区正中。
pub fn center_on_work_area(hwnd: HWND) -> RECT {
    let work = work_area(hwnd);
    let mut win = RECT::default();
    unsafe {
        if GetWindowRect(hwnd, &mut win).is_err() {
            return RECT::default();
        }
        let width = win.right - win.left;
        let height = win.bottom - win.top;
        let left = work.left + ((work.right - work.left) - width) / 2;
        let top = work.top + ((work.bottom - work.top) - height) / 2;
        let _ = SetWindowPos(
            hwnd,
            None,
            left,
            top,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        let _ = GetWindowRect(hwnd, &mut win);
    }
    win
}

/// 改窗口尺寸，但**保持窗口中心不动**（1.0.5）。
///
/// 用户把窗口拖到屏幕另一角再缩放，1.0.4 之前会按工作区重新居中——窗口"跳回原位"，
/// 手上的操作瞬间失去上下文。现在以窗口自己的中心为锚点放大/缩小，
/// 只有顶到工作区边界才往里挤（`clamp_into`）。
pub fn resize_around_center(hwnd: HWND, width: i32, height: i32) -> RECT {
    let mut cur = RECT::default();
    let work = work_area(hwnd);
    unsafe {
        if GetWindowRect(hwnd, &mut cur).is_err() {
            return cur;
        }
        let (cx, cy) = ((cur.left + cur.right) / 2, (cur.top + cur.bottom) / 2);
        let (width, height) = (width.max(1), height.max(1));
        let mut rc = RECT {
            left: cx - width / 2,
            top: cy - height / 2,
            right: cx - width / 2 + width,
            bottom: cy - height / 2 + height,
        };
        clamp_into(&mut rc, work);
        let _ = SetWindowPos(
            hwnd,
            None,
            rc.left,
            rc.top,
            rc.right - rc.left,
            rc.bottom - rc.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        rc
    }
}

/// 把矩形整体推进工作区（尺寸不变，只挪位置）。
///
/// 1.0.13 起也给"记住上次窗口位置"用：记下的坐标可能来自另一台显示器/另一个分辨率，
/// 直接用会让窗口出现在看不见的地方，所以摆位置前一律过一道这个。
pub fn clamp_into(rc: &mut RECT, work: RECT) {
    let (ww, wh) = (work.right - work.left, work.bottom - work.top);
    if rc.right - rc.left <= ww {
        if rc.left < work.left {
            rc.right += work.left - rc.left;
            rc.left = work.left;
        }
        if rc.right > work.right {
            rc.left -= rc.right - work.right;
            rc.right = work.right;
        }
    }
    if rc.bottom - rc.top <= wh {
        if rc.top < work.top {
            rc.bottom += work.top - rc.top;
            rc.top = work.top;
        }
        if rc.bottom > work.bottom {
            rc.top -= rc.bottom - work.bottom;
            rc.bottom = work.bottom;
        }
    }
}

/// 文本/Markdown 的开窗矩形（1.0.5）：**宽度固定**、高度按比例。
///
/// 用户反馈"文本默认窗口有点大"：以前宽高都跟着屏幕比例走，在 1080p 上是一个
/// 1200×750 的大窗，但一屏正文其实只用到中间一条。文字类内容没有"原始尺寸"，
/// 所以给一个阅读宽度（`text_window_width()`）更合适，剩下的上下留白用户自己看着调。
pub fn text_rect(work: RECT, height_ratio: f32) -> RECT {
    let (ww, wh) = (work.right - work.left, work.bottom - work.top);
    let width = text_window_width().min((ww as f32 * 0.92) as i32).max(320);
    let height = ((wh as f32) * height_ratio).clamp(240.0, wh as f32 * 0.92) as i32;
    let left = work.left + (ww - width) / 2;
    let top = work.top + (wh - height) / 2;
    RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

/// 文本/Markdown 的默认窗口宽度（像素）。880 在 100% 缩放下大约 70–80 个汉字宽，
/// 是长文档比较舒服的阅读宽度；宽度不再随屏幕变大而膨胀。
pub fn text_window_width() -> i32 {
    dpi::px(880)
}

pub fn is_visible(hwnd: HWND) -> bool {
    unsafe { IsWindowVisible(hwnd).as_bool() }
}

pub fn client_rect(hwnd: HWND) -> RECT {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    rc
}

/// 窗口所在显示器的工作区（已排除任务栏）。失败时退回主屏全尺寸。
pub fn work_area(hwnd: HWND) -> RECT {
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            return info.rcWork;
        }
        RECT {
            left: 0,
            top: 0,
            right: GetSystemMetrics(SM_CXSCREEN),
            bottom: GetSystemMetrics(SM_CYSCREEN),
        }
    }
}

/// 按比例在工作区居中的矩形。
pub fn centered_rect(work: RECT, width_ratio: f32, height_ratio: f32) -> RECT {
    let ww = (work.right - work.left).max(1);
    let wh = (work.bottom - work.top).max(1);
    let width = ((ww as f32 * width_ratio) as i32).clamp(240, ww);
    let height = ((wh as f32 * height_ratio) as i32).clamp(160, wh);
    let left = work.left + (ww - width) / 2;
    let top = work.top + (wh - height) / 2;
    RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

/// 内容自适应窗口矩形：**等比**缩放到工作区内并居中。
///
/// 1.0.1 早先的写法是把宽、高**各自**夹到工作区上限，于是 1279×1706 的竖图会得到
/// 1279×956 的横窗口——内容左右各留一大片黑边（用户实测截图）。这里改成先求一个
/// 统一缩放系数，再一次性套到宽高上，**宽高比因此永远不被破坏**。
///
/// - 小图不放大（"按原文件尺寸"），但保证不小于 `MIN_WIDTH × MIN_HEIGHT`（针眼窗口没法看）；
/// - 大图等比缩到工作区，但**留 `NATURAL_FILL` 的余量**（1.0.8 用户反馈"打开的时候屏幕
///   撑得有点满"：1.0.2 曾经用满工作区，机器上就是"贴着屏幕边"的感觉）。
pub fn natural_rect(work: RECT, width: i32, height: i32) -> RECT {
    natural_rect_scaled(work, width, height, 1.0)
}

/// 同 [`natural_rect`]，但先按 `factor` 缩一次（1.0.9：图片初始 50%）。
///
/// 语义是"用户希望一开始看到多大的内容"：0.5 = 半尺寸起步，
/// 仍然受工作区 86% 与最小可读尺寸约束（否则 10% 的缩略图窗口会小到点不着）。
pub fn natural_rect_scaled(work: RECT, width: i32, height: i32, factor: f32) -> RECT {
    const MIN_WIDTH: i32 = 240;
    const MIN_HEIGHT: i32 = 160;
    /// 大图最多占工作区的比例（留 14% 给桌面：看着不挤，也方便拖窗口）。
    const NATURAL_FILL: f32 = 0.86;

    let ww = ((work.right - work.left) as f32 * NATURAL_FILL).max(1.0) as i32;
    let wh = ((work.bottom - work.top) as f32 * NATURAL_FILL).max(1.0) as i32;
    // 1.8.3：入参 `width/height` 是**96 DPI 下的自然像素**（PDF 页 794×1123 = A4@96dpi，
    // 图片 = 图片像素数），先按 DPI 换成物理像素再算缩放。
    // 不换的话：200% 屏上一页 A4 只有 397×561 DIP，用户看到的是"内容小一半"
    // （这是"顶栏小"之外的另一半，光改字号和行高不够）。
    let cw = dpi::px(width.max(1));
    let ch = dpi::px(height.max(1));

    // `fit` = 塞进工作区所需的系数；`min_scale` = 撑到最小可读尺寸所需的系数。
    let fit = (ww as f32 / cw as f32).min(wh as f32 / ch as f32);
    let min_scale = (MIN_WIDTH as f32 / cw as f32).max(MIN_HEIGHT as f32 / ch as f32);
    let factor = factor.clamp(0.05, 4.0);
    let mut scale = (fit.min(1.0) * factor).min(fit);
    if scale < min_scale {
        scale = min_scale.min(fit);
    }

    // 宽高用**同一个** scale；四舍五入后可能差 1 px，属于取整误差，不影响观感。
    let width = ((cw as f32 * scale).round() as i32).clamp(1, ww);
    let height = ((ch as f32 * scale).round() as i32).clamp(1, wh);
    let screen_w = (work.right - work.left).max(1);
    let screen_h = (work.bottom - work.top).max(1);
    let left = work.left + (screen_w - width) / 2;
    let top = work.top + (screen_h - height) / 2;
    RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

/// 缩放后的窗口矩形：窗口尺寸**跟着缩放走**（内容 = 自然尺寸 × zoom），仍然居中。
///
/// 与 `natural_rect` 的两点区别：
/// - 不设最小尺寸（缩到 0.25× 就该变小，而不是卡在 240×160 上不动）；
/// - 超过工作区时顶满工作区（配合"位图比窗口大 + 平移"实现放大后的局部查看）。
pub fn zoomed_rect(work: RECT, natural: (u32, u32), zoom: f32) -> RECT {
    let ww = (work.right - work.left).max(1);
    let wh = (work.bottom - work.top).max(1);
    let cw = ((natural.0.max(1) as f32) * zoom.max(0.01))
        .round()
        .max(1.0);
    let ch = ((natural.1.max(1) as f32) * zoom.max(0.01))
        .round()
        .max(1.0);
    // 1.8.3：与 `natural_rect_scaled` 同一套口径——自然尺寸是 96 DPI 像素，先换到物理像素。
    // 这样"缩放 100%"在 200% 屏上仍然是一页实实在在的 A4，而不是半页。
    let cw = dpi::pxf(cw);
    let ch = dpi::pxf(ch);
    let scale = (ww as f32 / cw).min(wh as f32 / ch).min(1.0);
    let width = ((cw * scale).round() as i32).clamp(1, ww);
    let height = ((ch * scale).round() as i32).clamp(1, wh);
    let left = work.left + (ww - width) / 2;
    let top = work.top + (wh - height) / 2;
    RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

/// 文本客户区的底色（不透明）。文本预览不再走亚克力：亚克力会把字和背景混色，
/// 滚动时新旧字形叠加会更明显（1.0.1 起文本一律刷不透明底）。
pub const TEXT_BACKGROUND_RGB: u32 = BITMAP_BACKGROUND_RGB;

/// 文本从客户区顶部往下留的空白（像素）。
pub fn text_padding_top() -> i32 {
    dpi::cpx(16)
}

/// 文本行高（像素）。**必须与 `qlr-app` 的 `line_height()` 一致**：那边用它算可见行数。
pub fn line_height() -> i32 {
    dpi::cpx(24)
}

/// 行号栏宽度（像素）。行号靠右对齐画在这一栏里，正文从栏右侧开始。
///
/// 为什么是**固定常数**而不是"按最大行号的位数算"：折行时"量的宽度"必须等于
/// "画的宽度"（CONVENTIONS #38）。常数两边都不会算错；动态算就得让渲染折行和
/// GDI 绘制各自量一遍数字宽度，量法一旦不同就会出现"折了行还被省略号吃掉"。
/// 64 px 足够 6 位数行号（文本预算上限 20 万行）。
pub fn text_gutter() -> i32 {
    dpi::cpx(64)
}

/// 上屏用的行样式（像素级：字号/字重/颜色/字体面）。
///
/// 为什么平台层自己定义一份、而不是直接用 `qlr_render::LineStyle`：
/// `qlr-platform` 不依赖 `qlr-render`（渲染层可以换成别家实现，平台层只管画）。
/// 语义 → 像素的映射由壳层做一次转换，两边各自演进不会互相拖。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextStyle {
    Normal,
    Heading1,
    Heading2,
    Heading3,
    Code,
    Quote,
    List,
    Rule,
    /// Markdown 表格行（`|` 已去掉，单元格用 `│` 分隔）。
    Table,
    /// 左侧行号（1.0.5）：只出现在文本/Markdown 的行号栏里，不对应任何内容样式。
    LineNumber,
    /// 代码 token（1.0.8）：关键字（`fn`/`let`/`class`…），VSCode 浅色主题的蓝。
    Keyword,
    /// 代码 token：字符串字面量，VSCode 浅色主题的砖红。
    Str,
    /// 代码 token：注释，VSCode 浅色主题的绿。
    Comment,
    /// 代码 token：数字字面量，VSCode 浅色主题的青绿。
    Number,
}

impl TextStyle {
    const COUNT: usize = 14;

    /// 与 `qlr_render::LineStyle::as_index` 同序（壳层按索引直转）。
    /// 前 9 个与渲染层一一对应；后面 5 个是平台层独有的附加样式。
    pub fn as_index(self) -> usize {
        match self {
            TextStyle::Normal => 0,
            TextStyle::Heading1 => 1,
            TextStyle::Heading2 => 2,
            TextStyle::Heading3 => 3,
            TextStyle::Code => 4,
            TextStyle::Quote => 5,
            TextStyle::List => 6,
            TextStyle::Rule => 7,
            TextStyle::Table => 8,
            TextStyle::LineNumber => 9,
            TextStyle::Keyword => 10,
            TextStyle::Str => 11,
            TextStyle::Comment => 12,
            TextStyle::Number => 13,
        }
    }

    /// 字号（**物理像素**，逻辑高度）。全部 ≤18 DIP，保证塞进 24 DIP 行高不被裁。
    ///
    /// 1.8.3：表里写的是 DIP，实际传给 `CreateFontW` 的是按 DPI 换算后的值。
    /// 200% 下正文 15 → 30，与 30×2=60 的顶栏、48 的行高成比例，
    /// 这就是用户要的"字看起来和 100% 下一模一样大"。
    fn height(self) -> i32 {
        let dip = match self {
            TextStyle::Heading1 => 18,
            TextStyle::Heading2 => 17,
            TextStyle::Heading3 => 16,
            TextStyle::Code => 13,
            TextStyle::Table => 13,
            TextStyle::LineNumber => 12,
            TextStyle::Keyword | TextStyle::Str | TextStyle::Comment | TextStyle::Number => 14,
            _ => 15,
        };
        dpi::cpx(dip)
    }

    fn weight(self) -> i32 {
        match self {
            TextStyle::Heading1 | TextStyle::Heading2 | TextStyle::Heading3 => FW_BOLD.0 as i32,
            _ => FW_NORMAL.0 as i32,
        }
    }

    /// 字体面：代码用等宽，其余用雅黑（Win10 自带、CJK 覆盖好）。
    fn face(self) -> &'static str {
        match self {
            TextStyle::Code
            | TextStyle::Table
            | TextStyle::LineNumber
            | TextStyle::Keyword
            | TextStyle::Str
            | TextStyle::Comment
            | TextStyle::Number => "Consolas",
            _ => "Microsoft YaHei",
        }
    }

    /// 文字颜色。`COLORREF` 是 `0x00BBGGRR`（注意 R 在最低字节）。
    ///
    /// 1.0.8 起是**浅色主题**（白底），配色对齐 VSCode Light：正文接近黑，
    /// 关键字蓝、字符串砖红、注释绿、数字青绿。
    fn color(self) -> u32 {
        match self {
            TextStyle::Normal | TextStyle::List => 0x0020_2020, // #202020 正文
            TextStyle::Heading1 => 0x00B5_6616,                 // #1666B5
            TextStyle::Heading2 => 0x009E_4E1F,                 // #1F4E9E
            TextStyle::Heading3 => 0x003A_3A3A,                 // #3A3A3A
            TextStyle::Code => 0x0022_2222,                     // #222222
            TextStyle::Quote => 0x007D_736A,                    // #6A737D
            TextStyle::Rule => 0x00D4_D4D4,                     // #D4D4D4
            TextStyle::Table => 0x0020_2020,                    // #202020
            TextStyle::LineNumber => 0x00A6_A09A,               // #9AA0A6 中灰：能定位但不抢注意力
            TextStyle::Keyword => 0x00FF_0000,                  // #0000FF
            TextStyle::Str => 0x0015_15A3,                      // #A31515
            TextStyle::Comment => 0x0000_8000,                  // #008000
            TextStyle::Number => 0x0058_8609,                   // #098658
        }
    }

    fn create(self) -> HFONT {
        let face = to_wide(self.face());
        unsafe {
            CreateFontW(
                -self.height(),
                0,
                0,
                0,
                self.weight(),
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_TT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                DEFAULT_PITCH.0 as u32,
                PCWSTR(face.as_ptr()),
            )
        }
    }
}

/// 一行待绘制文本。
pub struct StyledRow<'a> {
    /// 左侧行号（1 基）。`None` = 这一行不显示行号（折行的续行、状态行都是 None）。
    pub number: Option<u32>,
    pub text: &'a str,
    pub style: TextStyle,
    /// 整行底色（Markdown 代码块/引用块），`None` = 跟随窗口底色。
    pub bg: Option<u32>,
}

/// 一段同色文本：一行可以有多个 span（代码着色、Markdown 行内混排）。
pub struct Span<'a> {
    pub text: &'a str,
    pub style: TextStyle,
}

/// 一行待绘制内容：行号 + 若干样式段 + 可选行底色（1.0.8）。
///
/// `paint_styled` 的 `StyledRow` 是它的单段特例；代码预览需要"一行多色"，
/// 所以真正的绘制路径是这里的 `paint_doc`。
pub struct DocRow<'a> {
    /// 左侧行号（1 基）。`None` = 这一行不显示行号（折行续行、状态行）。
    pub number: Option<u32>,
    pub spans: &'a [Span<'a>],
    /// 整行底色（Markdown 代码块 / 引用块的分色条），`None` = 跟随窗口底色。
    pub bg: Option<u32>,
}

thread_local! {
    /// 每线程一套字体，惰性创建（一次重画要画几十行，不能每行 CreateFontW）。
    static FONTS: std::cell::RefCell<FontSet> = std::cell::RefCell::new(FontSet::default());
}

struct FontSet {
    fonts: [Option<HFONT>; TextStyle::COUNT],
}

impl Default for FontSet {
    fn default() -> Self {
        Self {
            fonts: [None; TextStyle::COUNT],
        }
    }
}

impl FontSet {
    /// 取（并按需创建）某个样式的字体。创建失败就退回系统 GUI 字体，绝不 panic。
    fn get(&mut self, style: TextStyle) -> HFONT {
        let idx = style.as_index();
        if let Some(font) = self.fonts[idx] {
            return font;
        }
        let font = style.create();
        if font.0.is_null() {
            qlr_core::log_warn!(
                "window",
                "CreateFontW 失败（{}），该样式退回系统 GUI 字体",
                style.face()
            );
            // 不缓存：stock 对象不能被 DeleteObject
            return HFONT(unsafe { GetStockObject(DEFAULT_GUI_FONT) }.0);
        }
        self.fonts[idx] = Some(font);
        font
    }

    /// 丢掉全部缓存字体（DPI 变化时用）：不丢的话跨屏之后还在用旧像素高度的字体，
    /// 字会"半大不小"，而且每次都新建字体但不释放 → 每跨一次屏漏 14 个 HFONT。
    fn clear(&mut self) {
        for slot in &mut self.fonts {
            if let Some(font) = slot.take() {
                // 只删自己 `CreateFontW` 出来的；`get()` 里失败退回 stock 的那条路径
                // **不缓存**，所以这里不可能拿到 stock 对象（删它会失败但也不致命）。
                unsafe {
                    let _ = DeleteObject(font.into());
                }
            }
        }
    }
}

impl Drop for FontSet {
    /// 线程退出时兜底：`thread_local` 不保证 drop，但主线程退出前会走到这里的情况
    /// （例如设置窗所在的线程）至少不把 HFONT 漏给系统。
    fn drop(&mut self) {
        self.clear();
    }
}

/// 丢掉本线程的字体缓存。**这是 `dpi::set_change_hook` 的回调**：DPI 一变就调，
/// 下次绘制按新字号重建。
pub fn fonts_invalidate() {
    // 正常情况下一个窗口生命周期里只会调 1–2 次（跨屏拖动、改缩放）。
    // 如果在同一次预览里反复看到这行，说明线程 DPI 在**来回跳**——那是设计问题，
    // 不是正常路径（每跳一次就要重建 14 个 HFONT，绘制会明显变慢）。
    qlr_core::log_debug!("window", "字体缓存失效（DPI 变化）");
    FONTS.with(|cell| cell.borrow_mut().clear());
}

/// 装上"DPI 变了 → 重建字体"的钩子。宿主启动时调一次即可。
pub fn install_dpi_hook() {
    dpi::set_change_hook(fonts_invalidate);
}

/// 画一组带样式的文本行（逐行上色，每行可以多段）。
///
/// **每次绘制都先刷一层不透明底色**：窗口类背景刷是空的（为了在位图状态下不闪），
/// `WM_ERASEBKGND` 又返回 1（不擦除），所以如果这里不自己刷底，滚动/换内容时
/// 旧字形会留在客户区上——实测表现是"上下键一按，两屏文字叠在一起发糊"（1.0.1 修）。
///
/// 行从**顶部**开始排（不再垂直居中）：居中会让"行数变化"变成"整体上下平移"，
/// 滚动时每一屏的基线都在动，视觉上像抖动。
///
/// 空行只占高度不调 `DrawTextW`（原因见下方注释：空切片会让 user32 拿 `cchText=0`
/// 去 wcslen 悬垂指针）。
/// 一次 `WM_PAINT` 用的离屏缓冲（双缓冲）。建不起来返回 `None`，调用方直接画窗口 DC。
struct PaintBuffer {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
}

impl PaintBuffer {
    unsafe fn new(window_dc: HDC, rc: RECT) -> Option<Self> {
        let dc = unsafe { CreateCompatibleDC(Some(window_dc)) };
        if dc.0.is_null() {
            return None;
        }
        let bitmap = unsafe {
            CreateCompatibleBitmap(
                window_dc,
                (rc.right - rc.left).max(1),
                (rc.bottom - rc.top).max(1),
            )
        };
        if bitmap.0.is_null() {
            let _ = unsafe { DeleteDC(dc) };
            return None;
        }
        let old = unsafe { SelectObject(dc, bitmap.into()) };
        Some(Self { dc, bitmap, old })
    }
}

impl Drop for PaintBuffer {
    fn drop(&mut self) {
        unsafe {
            let _ = SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}
/// 列表视图的一行（压缩包目录用，1.7.2）。
pub struct ListRow<'a> {
    /// 目录层级（决定缩进）。
    pub depth: usize,
    /// 是否是文件夹。
    pub is_dir: bool,
    /// 文件夹是否展开。
    pub expanded: bool,
    /// 名字（不带路径）。
    pub name: &'a str,
    /// 右列：大小（文件才有，目录留空）。
    pub size: &'a str,
    /// 右列：修改时间。
    pub time: &'a str,
    /// 是否高亮（当前行）。
    pub selected: bool,
}

/// 列表视图的行高。
pub fn list_row_height() -> i32 {
    dpi::cpx(26)
}

/// 画一个文件列表（📁/📄 + 缩进 + 大小 + 时间），带双缓冲。
///
/// 和 `paint_doc` 是两条不同的绘制通道：文本通道讲究"逐行等宽"，列表讲究"列对齐 +
/// 图标 + 缩进"，混在一条通道里会把两边都搞复杂（1.7.2）。
pub fn paint_list(
    hwnd: HWND,
    chrome: Option<&Chrome<'_>>,
    content: Option<RECT>,
    rows: &[ListRow<'_>],
) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let window_dc: HDC = BeginPaint(hwnd, &mut ps);
        if window_dc.0.is_null() {
            let _ = EndPaint(hwnd, &ps);
            return;
        }
        let rc = client_rect(hwnd);
        let buffer = PaintBuffer::new(window_dc, rc);
        let hdc: HDC = buffer.as_ref().map_or(window_dc, |b| b.dc);
        SetBkMode(hdc, TRANSPARENT);
        let brush = CreateSolidBrush(COLORREF(TEXT_BACKGROUND_RGB));
        FillRect(hdc, &rc, brush);
        let _ = DeleteObject(brush.into());
        let area = match chrome {
            Some(spec) => paint_chrome(hdc, rc, spec),
            None => rc,
        };
        let content = content.unwrap_or(area);

        // 三列：左（图标+名字） / 中（大小，右对齐） / 右（时间，右对齐）
        let size_right = content.right - 180;
        let time_right = content.right - 16;
        let size_left = size_right - 110;

        FONTS.with(|cell| {
            let mut set = cell.borrow_mut();
            let mut y = content.top + text_padding_top();
            for row in rows {
                if row.selected {
                    let band = RECT {
                        left: content.left + 4,
                        top: y,
                        right: content.right - 4,
                        bottom: y + list_row_height(),
                    };
                    let hi = CreateSolidBrush(COLORREF(LIST_HIGHLIGHT_RGB));
                    FillRect(hdc, &band, hi);
                    let _ = DeleteObject(hi.into());
                }
                let indent = content.left + 16 + (row.depth as i32) * 16;
                // 折叠标记只用几何符号（▾ / ▸）：GDI 的 `DrawTextW` 不做字体回退，
                // 📁/📄 这类彩色 emoji 会被画成豆腐块（1.7.3 实测截图）。
                // 目录和文件靠"名字末尾的 /"与折叠箭头区分。
                let marker = if row.is_dir {
                    if row.expanded {
                        "\u{25BE} "
                    } else {
                        "\u{25B8} "
                    }
                } else {
                    "  "
                };
                let label = format!("{marker}{}", row.name);
                let font = set.get(TextStyle::Normal);
                let old = SelectObject(hdc, font.into());
                SetTextColor(hdc, COLORREF(TextStyle::Normal.color()));
                let mut rect = RECT {
                    left: indent,
                    right: size_left - 8,
                    top: y,
                    bottom: y + list_row_height(),
                };
                let mut buf: Vec<u16> = label.encode_utf16().collect();
                DrawTextW(
                    hdc,
                    &mut buf,
                    &raw mut rect,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
                );
                if !row.size.is_empty() {
                    SetTextColor(hdc, COLORREF(TextStyle::LineNumber.color()));
                    let mut sr = RECT {
                        left: size_left,
                        right: size_right,
                        top: y,
                        bottom: y + list_row_height(),
                    };
                    let mut sb: Vec<u16> = row.size.encode_utf16().collect();
                    DrawTextW(
                        hdc,
                        &mut sb,
                        &raw mut sr,
                        DT_RIGHT | DT_SINGLELINE | DT_VCENTER,
                    );
                }
                if !row.time.is_empty() {
                    let mut tr = RECT {
                        left: size_right + 8,
                        right: time_right,
                        top: y,
                        bottom: y + list_row_height(),
                    };
                    let mut tb: Vec<u16> = row.time.encode_utf16().collect();
                    DrawTextW(
                        hdc,
                        &mut tb,
                        &raw mut tr,
                        DT_RIGHT | DT_SINGLELINE | DT_VCENTER,
                    );
                }
                SelectObject(hdc, old);
                y += list_row_height();
            }
        });

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
        let _ = EndPaint(hwnd, &ps);
    }
}

/// 列表选中行的底色。
pub const LIST_HIGHLIGHT_RGB: u32 = 0x00E8F0F8;
pub fn paint_doc(
    hwnd: HWND,
    chrome: Option<&Chrome<'_>>,
    content: Option<RECT>,
    rows: &[DocRow<'_>],
) {
    paint_doc_with(hwnd, chrome, content, rows, None);
}

/// `paint_doc` 的实现版本，多一个"叠加层"回调（目录栏、右侧滚动条）。
///
/// 1.5.2：叠加层也画进**同一块**内存 DC，整窗一次性 `BitBlt` 上屏。
/// 之前叠加层是自己 `GetDC` 再画，正文和侧栏是两次上屏，滚动时侧栏会闪。
pub fn paint_doc_with(
    hwnd: HWND,
    chrome: Option<&Chrome<'_>>,
    content: Option<RECT>,
    rows: &[DocRow<'_>],
    overlay: Option<&dyn Fn(HDC, RECT)>,
) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let window_dc: HDC = BeginPaint(hwnd, &mut ps);
        if window_dc.0.is_null() {
            let _ = EndPaint(hwnd, &ps);
            return;
        }
        let rc = client_rect(hwnd);
        // 双缓冲（1.5.1）：整屏先画进内存 DC，最后一次性 BitBlt 上屏。
        let buffer = PaintBuffer::new(window_dc, rc);
        let hdc: HDC = buffer.as_ref().map_or(window_dc, |b| b.dc);
        SetBkMode(hdc, TRANSPARENT);

        let brush = CreateSolidBrush(COLORREF(TEXT_BACKGROUND_RGB));
        FillRect(hdc, &rc, brush);
        let _ = DeleteObject(brush.into());

        // 顶栏由这里一起画（同一块 DC，省一次 BeginPaint）；内容区可以被壳层收窄
        // （Markdown 的目录栏、右侧滚动条都要从内容区里让出位置）。
        let after_chrome = match chrome {
            Some(spec) => paint_chrome(hdc, rc, spec),
            None => rc,
        };
        let content = content.unwrap_or(after_chrome);

        let mut y = content.top + text_padding_top();

        // 有任何一行带行号，就给全体正文空出一条行号栏（否则正文宽度会随滚动位置跳动）。
        let gutter = if rows.iter().any(|row| row.number.is_some()) {
            text_gutter()
        } else {
            0
        };
        let text_left = content.left + 12 + gutter;
        let text_right = content.right - 24;

        FONTS.with(|cell| {
            let mut set = cell.borrow_mut();
            for row in rows {
                // 行底色（Markdown 代码块/引用块）：整行一条色带，不覆盖行号栏。
                if let Some(rgb) = row.bg {
                    let band = RECT {
                        left: text_left - 8,
                        top: y,
                        right: text_right + 8,
                        bottom: y + line_height(),
                    };
                    let brush = CreateSolidBrush(COLORREF(rgb));
                    FillRect(hdc, &band, brush);
                    let _ = DeleteObject(brush.into());
                }
                // 行号：右对齐画在行号栏里，和样式无关（一眼能定位第几行）。
                if let Some(number) = row.number {
                    let font = set.get(TextStyle::LineNumber);
                    let old = SelectObject(hdc, font.into());
                    SetTextColor(hdc, COLORREF(TextStyle::LineNumber.color()));
                    let mut label: Vec<u16> = number.to_string().encode_utf16().collect();
                    let mut rect = RECT {
                        left: content.left + 24,
                        top: y,
                        right: content.left + 24 + gutter - 10,
                        bottom: y + line_height(),
                    };
                    DrawTextW(
                        hdc,
                        &mut label,
                        &raw mut rect,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
                    );
                    SelectObject(hdc, old);
                }
                // 坑：windows-rs 的 `DrawTextW` 用 `lpchtext.len()` 当 cchText，空切片会传
                // cchText=0 并把一个悬垂指针交给 user32 —— user32 随即 wcslen 那段内存，
                // 直接 0xC0000005（实测崩在 user32.dll+0x1f16b，经 DispatchMessageW 进来）。
                // 空行只占高度、不画字。
                let blank = row.spans.iter().all(|span| span.text.is_empty());
                if blank {
                    y += line_height();
                    continue;
                }
                // 逐段画：先量宽度决定下一段的起点，再 DrawTextW（默认会裁到 rect，
                // 所以超长的代码行到右边界自动断掉，不会画到窗口外面）。
                let mut x = text_left;
                for span in row.spans {
                    if span.text.is_empty() {
                        continue;
                    }
                    if x >= text_right {
                        break;
                    }
                    let font = set.get(span.style);
                    let old = SelectObject(hdc, font.into());
                    SetTextColor(hdc, COLORREF(span.style.color()));

                    let mut buf: Vec<u16> = span.text.encode_utf16().collect();
                    let mut size = SIZE::default();
                    let _ = GetTextExtentPoint32W(hdc, &buf, &mut size);
                    let mut rect = RECT {
                        left: x,
                        top: y,
                        right: text_right,
                        bottom: y + line_height(),
                    };
                    DrawTextW(
                        hdc,
                        &mut buf,
                        &raw mut rect,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
                    );
                    SelectObject(hdc, old);
                    x += size.cx.max(0);
                }
                y += line_height();
            }
        });

        // 叠加层（目录栏/滚动条）也画在这块内存 DC 上，然后整窗一次性上屏。
        if let Some(draw) = overlay {
            draw(hdc, content);
        }
        // 双缓冲（1.5.1）：把这块内存 DC 的内容一次性上屏。
        // 以前是"正文用 BeginPaint 画一次、目录/滚动条再用 GetDC 画一次"，两次上屏之间
        // 就是用户看到的频闪；建不起缓冲时这里退化成直画（buffer 为 None）。
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
        let _ = EndPaint(hwnd, &ps);
    }
}

/// 画一组单段样式的文本行（Markdown/文本预览走这里）。
///
/// 只是 `paint_doc` 的薄包装：**全程序只有一条文本绘制路径**，
/// 免得"折行时量的字体"和"绘制时用的字体"哪天又对不上。
pub fn paint_styled(
    hwnd: HWND,
    chrome: Option<&Chrome<'_>>,
    content: Option<RECT>,
    rows: &[StyledRow<'_>],
) {
    paint_styled_with(hwnd, chrome, content, rows, None);
}

/// `paint_styled` 的实现版本，多一个叠加层回调（目录/滚动条画在同一块内存 DC 上）。
pub fn paint_styled_with(
    hwnd: HWND,
    chrome: Option<&Chrome<'_>>,
    content: Option<RECT>,
    rows: &[StyledRow<'_>],
    overlay: Option<&dyn Fn(HDC, RECT)>,
) {
    let mut spans: Vec<Vec<Span<'_>>> = Vec::with_capacity(rows.len());
    for row in rows {
        spans.push(if row.text.is_empty() {
            Vec::new()
        } else {
            vec![Span {
                text: row.text,
                style: row.style,
            }]
        });
    }
    let docs: Vec<DocRow<'_>> = rows
        .iter()
        .zip(spans.iter())
        .map(|(row, spans)| DocRow {
            number: row.number,
            spans: spans.as_slice(),
            bg: row.bg,
        })
        .collect();
    paint_doc_with(hwnd, chrome, content, &docs, overlay);
}

/// 画若干行普通文本（占位内容、纯文本预览都走这里）。
///
/// 只是 `paint_styled` 的薄包装：**全程序只有一条文本绘制路径**，
/// 免得"折行时量的字体"和"绘制时用的字体"哪天又对不上。
pub fn paint_text(hwnd: HWND, chrome: Option<&Chrome<'_>>, content: Option<RECT>, lines: &[&str]) {
    let rows: Vec<StyledRow<'_>> = lines
        .iter()
        .map(|text| StyledRow {
            number: None,
            text,
            style: TextStyle::Normal,
            bg: None,
        })
        .collect();
    paint_styled_with(hwnd, chrome, content, &rows, None);
}

/// `paint_text` 的叠加层版本（占位文字 + 目录/滚动条一次上屏）。
pub fn paint_text_with(
    hwnd: HWND,
    chrome: Option<&Chrome<'_>>,
    content: Option<RECT>,
    lines: &[&str],
    overlay: Option<&dyn Fn(HDC, RECT)>,
) {
    let rows: Vec<StyledRow<'_>> = lines
        .iter()
        .map(|text| StyledRow {
            number: None,
            text,
            style: TextStyle::Normal,
            bg: None,
        })
        .collect();
    paint_styled_with(hwnd, chrome, content, &rows, overlay);
}

// ---------------------------------------------------------------------------
// 设置页（1.0.7）：两列表格 + 行高亮
// ---------------------------------------------------------------------------

/// 设置页的行高（像素）。比预览页的 24 大一点：设置项要能用鼠标点，热区太小难点。
pub fn settings_row_height() -> i32 {
    dpi::px(28)
}
/// 设置页内容距客户区顶部的留白。
pub fn settings_padding_top() -> i32 {
    dpi::px(18)
}
/// 设置页左右留白。
pub fn settings_padding_x() -> i32 {
    dpi::px(28)
}
/// 设置页高亮行的底色（比白色背景略深的浅灰，1.0.8 起浅色主题）。
pub const SETTINGS_HIGHLIGHT_RGB: u32 = 0x00E8_E8E8;

/// 设置页的一行：左边是名称，右边是值（右对齐）。
///
/// 两列而不是拼成一个字符串：值要能被眼睛快速扫到、也要能单独上色，
/// 拼接后就没法右对齐了。
pub struct SettingsRow<'a> {
    pub left: &'a str,
    pub right: &'a str,
    pub style: TextStyle,
    /// 值那一列用的样式（通常是 `Code`：等宽、淡青，像"键帽"）。
    pub value_style: TextStyle,
    /// 是否高亮（当前选中/正在改键的那一行）。
    pub highlight: bool,
}

/// 画一整页设置内容。
///
/// 行从顶部开始、每行 [`settings_row_height()`]，与 `qlr-app` 的命中测试共用同一组
/// 常量（见 `settings::row_at`），保证"看到的行"和"点到的行"永远是同一行。
pub fn paint_settings(hwnd: HWND, rows: &[SettingsRow<'_>]) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc: HDC = BeginPaint(hwnd, &mut ps);
        if hdc.0.is_null() {
            let _ = EndPaint(hwnd, &ps);
            return;
        }
        SetBkMode(hdc, TRANSPARENT);

        let rc = client_rect(hwnd);
        let background = CreateSolidBrush(COLORREF(TEXT_BACKGROUND_RGB));
        FillRect(hdc, &rc, background);
        let _ = DeleteObject(background.into());

        let mut y = rc.top + settings_padding_top();
        let highlight = CreateSolidBrush(COLORREF(SETTINGS_HIGHLIGHT_RGB));

        FONTS.with(|cell| {
            let mut set = cell.borrow_mut();
            for row in rows {
                if row.highlight && !row.left.is_empty() {
                    let band = RECT {
                        left: rc.left + 8,
                        top: y,
                        right: rc.right - 8,
                        bottom: y + settings_row_height(),
                    };
                    FillRect(hdc, &band, highlight);
                }
                // 空行只占高度：`DrawTextW` 收到空切片会传 cchText=0 并 wcslen 悬垂指针。
                if !row.left.is_empty() {
                    let font = set.get(row.style);
                    let old = SelectObject(hdc, font.into());
                    SetTextColor(hdc, COLORREF(row.style.color()));
                    let mut buf: Vec<u16> = row.left.encode_utf16().collect();
                    // 右边没有值（分类标题、说明行、底部提示）就把整行让给左列，
                    // 否则说明文字会被空着的值列挤掉半句（"按住它移动鼠标 = 放大镜；按住它…"）。
                    let right_edge = if row.right.is_empty() {
                        rc.right - settings_padding_x()
                    } else {
                        rc.right - settings_padding_x() - 180
                    };
                    let mut rect = RECT {
                        left: rc.left + settings_padding_x(),
                        top: y,
                        right: right_edge,
                        bottom: y + settings_row_height(),
                    };
                    DrawTextW(
                        hdc,
                        &mut buf,
                        &raw mut rect,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
                    );
                    SelectObject(hdc, old);
                }
                if !row.right.is_empty() {
                    let font = set.get(row.value_style);
                    let old = SelectObject(hdc, font.into());
                    SetTextColor(hdc, COLORREF(row.value_style.color()));
                    let mut buf: Vec<u16> = row.right.encode_utf16().collect();
                    let mut rect = RECT {
                        left: rc.right - settings_padding_x() - 172,
                        top: y,
                        right: rc.right - settings_padding_x(),
                        bottom: y + settings_row_height(),
                    };
                    DrawTextW(
                        hdc,
                        &mut buf,
                        &raw mut rect,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
                    );
                    SelectObject(hdc, old);
                }
                y += settings_row_height();
            }
        });
        let _ = DeleteObject(highlight.into());

        let _ = EndPaint(hwnd, &ps);
    }
}

// ---------------------------------------------------------------------------
// 顶栏（1.0.9）：所有格式统一的黑色工具条
//
// 为什么不像系统窗口那样给每种格式单独的标题栏：预览窗是无边框的（去掉了系统
// 标题栏才能做亚克力/圆角），所以"标题 + 按钮"必须自己画。既然自己画，就顺手让
// 四种格式（图片/PDF/文本/Markdown+代码）长得一模一样——用户不用为每种格式重新
// 学一遍"关闭按钮在哪"。
// ---------------------------------------------------------------------------

/// 顶栏高度（像素）。
pub fn chrome_height() -> i32 {
    dpi::px(30)
}
/// 顶栏底色：黑色。浅色内容区上面压一条黑边，一眼就是"这是预览器的框"。
pub const CHROME_BG_RGB: u32 = 0x0020_2020;
/// 顶栏主文字色。
pub const CHROME_FG_RGB: u32 = 0x00E8_E8E8;
/// 顶栏里"悬停提示"气泡的底色。
pub const CHROME_HINT_RGB: u32 = 0x0028_2828;
/// 顶栏按钮的悬停底色。
pub const CHROME_HOVER_RGB: u32 = 0x003C_3C3C;
/// 页码输入框的底色与边框。
pub const CHROME_BOX_RGB: u32 = 0x0032_3232;
pub const CHROME_BOX_BORDER_RGB: u32 = 0x005C_5C5C;
/// "阻止关闭"按钮开启后的底色（暗琥珀，和悬停灰一眼区分）。
pub const CHROME_PIN_RGB: u32 = 0x0050_4520;

/// 窗口最外圈那三圈"阴影"：由深到浅，让白底窗口和白底程序分得开边界。
/// 发丝边的颜色（1.0.10 定稿：投影交给分层窗口，客户区只留一条 1 px）。
pub const FRAME_HAIRLINE_RGB: u32 = 0x00C8_C8C8;
/// 顶栏右侧每个按钮的宽度。
pub fn chrome_button_w() -> i32 {
    dpi::px(38)
}
/// PDF 页码框宽度。
pub fn page_box_w() -> i32 {
    dpi::px(84)
}

/// 右侧竖滚动条宽度（文本 / PDF / Markdown 用）。
pub fn scrollbar_w() -> i32 {
    dpi::px(12)
}
/// 滚动条槽底色与滑块色（`hover` 时用深一点的那个）。
pub const SCROLLBAR_TRACK_RGB: u32 = 0x00F2_F2F2;
pub const SCROLLBAR_THUMB_RGB: u32 = 0x00C0_C0C0;
pub const SCROLLBAR_THUMB_HOVER_RGB: u32 = 0x0090_9090;

/// Markdown 目录栏宽度与每行高度。
pub fn toc_width() -> i32 {
    dpi::px(200)
}
pub fn toc_row_height() -> i32 {
    dpi::cpx(24)
}
pub const TOC_BG_RGB: u32 = 0x00F7_F7F7;
pub const TOC_SEP_RGB: u32 = 0x00E4_E4E4;
pub const TOC_TITLE_RGB: u32 = 0x00A0_9A94;

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

/// 顶栏上可以点的三个按钮。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChromeButton {
    /// 用系统默认程序打开这个文件（`ShellExecute`）。
    External,
    /// 关闭预览（备用入口：Esc 之外的鼠标路径）。
    Close,
    /// 阻止关闭（1.0.10）：按下后空格/Esc 都关不掉这个窗口，且**不能取消**
    /// （用户原话"不能取消阻止关闭"——防误触，开着就是开着，关窗可以走 × 或托盘）。
    Pin,
}

/// PDF 页码框：正常显示 `current / total`，`editing` 有值时显示正在输入的数字。
pub struct PageBox<'a> {
    pub current: u32,
    pub total: u32,
    pub editing: Option<&'a str>,
}

/// 顶栏内容（四种格式共用同一套结构）。
pub struct Chrome<'a> {
    /// 左侧标题（文件名）。
    pub title: &'a str,
    /// PDF 才有的页码框。
    pub page: Option<PageBox<'a>>,
    /// 鼠标停在哪个按钮上（决定按钮底色）。
    pub hover: Option<ChromeButton>,
    /// 悬停提示文字（画在按钮下方的小气泡里）。
    pub hint: Option<&'a str>,
    /// 一次性提示（1.7.4）：画在顶栏右侧的气泡里，由壳层计时清掉。
    /// 目前用于"已是最后一页/第一页"这类翻页反馈（系统预览处理器自己没有这个概念）。
    pub toast: Option<&'a str>,
    /// "阻止关闭"是否已开启（开启了就给按钮上高亮底色，用户能看出窗口被锁住）。
    pub pinned: bool,
}

/// 顶栏各块的位置。绘制与命中测试都从 [`chrome_rects`] 取，避免两处各算一遍。
pub struct ChromeRects {
    /// 扣掉顶栏后的内容区。
    pub content: RECT,
    pub page: RECT,
    pub external: RECT,
    pub close: RECT,
    pub pin: RECT,
}

/// 由客户区算出顶栏各块的位置（纯函数，便于单测）。
pub fn chrome_rects(rc: RECT) -> ChromeRects {
    let close = RECT {
        left: rc.right - chrome_button_w(),
        top: rc.top,
        right: rc.right,
        bottom: rc.top + chrome_height(),
    };
    let external = RECT {
        left: close.left - chrome_button_w(),
        right: close.left,
        top: rc.top,
        bottom: rc.top + chrome_height(),
    };
    // 页码框在最左、紧跟"阻止关闭"（锁）；两者都离 × 远一点，避免点错。
    let pin = RECT {
        left: external.left - chrome_button_w(),
        top: rc.top,
        right: external.left,
        bottom: rc.top + chrome_height(),
    };
    let page = RECT {
        left: pin.left - 10 - page_box_w(),
        top: rc.top + 5,
        right: pin.left - 10,
        bottom: rc.top + chrome_height() - 5,
    };
    ChromeRects {
        content: RECT {
            left: rc.left,
            top: rc.top + chrome_height(),
            right: rc.right,
            bottom: rc.bottom,
        },
        page,
        external,
        close,
        pin,
    }
}

/// 铺一块纯色（每次用完立刻删刷子，GDI 对象不删会漏）。
fn fill_solid(hdc: HDC, rect: RECT, rgb: u32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(rgb));
        FillRect(hdc, &rect, brush);
        let _ = DeleteObject(brush.into());
    }
}

/// 画一条 1 px 边框（`FrameRect` 用刷子画边框，不填内部）。
fn frame_solid(hdc: HDC, rect: RECT, rgb: u32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(rgb));
        FrameRect(hdc, &rect, brush);
        let _ = DeleteObject(brush.into());
    }
}

/// 用指定样式画一段单行文字。
fn draw_line(
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
fn measure_line(hdc: HDC, text: &str, style: TextStyle) -> i32 {
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

/// 把子窗口提到同级最上层（1.7.6）。
///
/// 用途：换文件时让**旧**的系统预览子窗口继续盖住内容区，等新处理器的第一帧画出来再把它卸掉，
/// 这样中间不会露出空白/占位页（`HWND_TOP` 在子窗口里＝同级最前）。
pub fn raise_child(hwnd: HWND) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// 只重画顶栏那一条，**内容区一个像素都不碰**（1.7.6）。
///
/// 用途：换文件时把内容区"冻"在上一帧，等新内容真的画出来再覆盖。中间那一两百毫秒
/// （Office 预览要跨进程 `DoPreview`）如果把整窗重画一遍，用户会看到"占位文字页"闪一下——
/// 这就是用户说的"Office 切换割裂感最重"。
///
/// 之所以"不碰就冻得住"：窗口类的背景刷是空的，`WM_ERASEBKGND` 又返回 1（不擦除），
/// 所以没画到的像素保持原样（位图通道当年就是靠这个避免闪烁）。
pub fn paint_chrome_strip(hwnd: HWND, chrome: &Chrome<'_>) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        if hdc.0.is_null() {
            let _ = EndPaint(hwnd, &ps);
            return;
        }
        let rc = client_rect(hwnd);
        paint_chrome(hdc, rc, chrome);
        let _ = EndPaint(hwnd, &ps);
    }
}

/// 画顶栏，返回扣掉顶栏后的内容区。
pub fn paint_chrome(hdc: HDC, rc: RECT, chrome: &Chrome<'_>) -> RECT {
    let rects = chrome_rects(rc);
    let band = RECT {
        left: rc.left,
        top: rc.top,
        right: rc.right,
        bottom: rc.top + chrome_height(),
    };
    fill_solid(hdc, band, CHROME_BG_RGB);

    // 标题右边界：有页码框才让位，没有就一路顶到"锁"按钮左边 12px。
    // 1.7.4 修：以前**无条件**按页码框的位置收，非 PDF（Office/图片/文本）白扣
    // 94+10+12 ≈ 116px —— 窄窗里长文件名必被 `DT_END_ELLIPSIS` 截成半截，
    // 240px 宽的窗口甚至算出反向矩形（`right < left`，DrawTextW 什么都不画）。
    let title_right = if chrome.page.is_some() {
        rects.page.left - 12
    } else {
        rects.pin.left - 12
    };
    let title = RECT {
        left: rc.left + 14,
        top: band.top,
        // 再窄也留 10px：反向矩形是"整条标题消失"，比截断难查得多。
        right: title_right.max(rc.left + 24),
        bottom: band.bottom,
    };
    draw_line(
        hdc,
        title,
        chrome.title,
        TextStyle::Normal,
        CHROME_FG_RGB,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
    );

    if let Some(page) = &chrome.page {
        fill_solid(hdc, rects.page, CHROME_BOX_RGB);
        frame_solid(hdc, rects.page, CHROME_BOX_BORDER_RGB);
        let label = match page.editing {
            // 正在输入：光标用 `|` 顶替，用户一眼看出"这里可以打字"。
            Some(text) => format!("{text}|"),
            None => format!("{} / {}", page.current, page.total),
        };
        draw_line(
            hdc,
            rects.page,
            &label,
            TextStyle::Code,
            CHROME_FG_RGB,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
    }

    for (button, rect, glyph) in [
        (ChromeButton::Pin, rects.pin, "锁"),
        (ChromeButton::External, rects.external, "↗"),
        (ChromeButton::Close, rects.close, "×"),
    ] {
        // 锁按钮的底色表示"锁定状态"而不是悬停（开着就一直亮着）。
        if chrome.hover == Some(button) {
            fill_solid(hdc, rect, CHROME_HOVER_RGB);
        } else if button == ChromeButton::Pin && chrome.pinned {
            fill_solid(hdc, rect, CHROME_PIN_RGB);
        }
        draw_line(
            hdc,
            rect,
            glyph,
            TextStyle::Normal,
            CHROME_FG_RGB,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
    }

    // 悬停提示：贴在按钮下方，压在内容区上。自己做是因为原生 tooltip 要额外的
    // TOOLTIPS_CLASS 窗口与消息转发，而我们窗口本来就不激活（tooltip 也不一定弹得出来）。
    if let (Some(button), Some(hint)) = (chrome.hover, chrome.hint) {
        let anchor = match button {
            ChromeButton::External => rects.external,
            ChromeButton::Close => rects.close,
            ChromeButton::Pin => rects.pin,
        };
        let text_w = measure_line(hdc, hint, TextStyle::Normal);
        let width = text_w + 18;
        let left = (anchor.right - width).max(rc.left + 4);
        let bubble = RECT {
            left,
            top: rc.top + chrome_height() + 4,
            right: left + width,
            bottom: rc.top + chrome_height() + 26,
        };
        fill_solid(hdc, bubble, CHROME_HINT_RGB);
        frame_solid(hdc, bubble, CHROME_BOX_BORDER_RGB);
        draw_line(
            hdc,
            bubble,
            hint,
            TextStyle::Normal,
            CHROME_FG_RGB,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
    }

    // 一次性提示（1.7.4）：画在顶栏右侧、贴着"锁"左边的气泡，1.2s 后由壳层清掉。
    // 为什么不复用上面那个悬停气泡：那个贴在按钮下方、会压在内容上；翻页提示
    // 需要"扫一眼就有"，放在顶栏里既不挡内容也不用等鼠标。
    if let Some(toast) = chrome.toast.filter(|t| !t.is_empty()) {
        let text_w = measure_line(hdc, toast, TextStyle::Normal);
        let width = text_w + 18;
        let right = rects.pin.left - 6;
        let bubble = RECT {
            // 允许盖住标题右端：标题本来就是"看一眼就知道是什么文件"，
            // 提示只有 1.2s，压一下不影响；不让盖的话窄窗里会挤成 0 宽（踩过）。
            left: (right - width).max(rc.left + 60),
            top: rc.top + 5,
            right,
            bottom: rc.top + chrome_height() - 5,
        };
        fill_solid(hdc, bubble, CHROME_BOX_RGB);
        frame_solid(hdc, bubble, CHROME_BOX_BORDER_RGB);
        draw_line(
            hdc,
            bubble,
            toast,
            TextStyle::Normal,
            CHROME_FG_RGB,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
    }

    rects.content
}

// ---------------------------------------------------------------------------
// 滚动条与目录栏（1.0.9）
// ---------------------------------------------------------------------------

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
        right: (area.left + toc_width()).min(area.right),
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

/// 用系统默认关联程序打开文件（顶栏的「在其他应用打开」按钮）。
///
/// 用 `ShellExecuteW` 而不是 `CreateProcess`：文件类型千差万别，只有 shell 知道
/// 该交给谁（图片→照片、PDF→阅读器、代码→编辑器）。失败时返回 false（例如没有
/// 关联程序），调用方记日志即可，不要弹框——这是用户主动点的次要操作。
pub fn open_external(path: &str) -> bool {
    unsafe {
        let operation = to_wide("open");
        let file = to_wide(path);
        let result = ShellExecuteW(
            None,
            PCWSTR(operation.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        // 文档：返回值 > 32 才是成功。
        result.0 as isize > 32
    }
}

/// 捕获鼠标：拖拽平移时，鼠标移出窗口后仍能收到 `WM_MOUSEMOVE`。
pub fn set_capture(hwnd: HWND) {
    unsafe {
        SetCapture(hwnd);
    }
}

/// 释放鼠标捕获（抬手时一定要调，否则别的窗口点不动）。
pub fn release_capture() {
    unsafe {
        let _ = ReleaseCapture();
    }
}

/// 某个虚拟键此刻是否按着（`GetAsyncKeyState`）。
///
/// 用轮询而不是自己维护按键状态：放大镜这类"按住"语义只在鼠标移动时查一次，
/// 在全局键盘钩子里跟踪按下/抬起反而更容易漏（焦点可能在别的进程上）。
pub fn key_down(vk: u16) -> bool {
    unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
}

/// 用户此刻是否按着 Shift（放大镜的**默认**开关；1.0.7 起改成可配置，
/// 壳层读 `hotkeys.lens_hold` 后调 [`key_down`]）。
pub fn shift_down() -> bool {
    unsafe { (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0 }
}

/// 修饰键此刻的状态：`(ctrl, alt, shift)`。设置页"按下新键"时用它拼 `Ctrl+X`。
pub fn modifier_state() -> (bool, bool, bool) {
    unsafe {
        let query = |vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY| {
            (GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000) != 0
        };
        (
            query(windows::Win32::UI::Input::KeyboardAndMouse::VK_CONTROL),
            query(windows::Win32::UI::Input::KeyboardAndMouse::VK_MENU),
            query(windows::Win32::UI::Input::KeyboardAndMouse::VK_SHIFT),
        )
    }
}

/// 放大镜的半径（像素）。镜头是 `2 * lens_radius()` 见方。
pub fn lens_radius() -> i32 {
    dpi::px(110)
}

/// 在客户区上叠加一个放大镜（跟随光标的方形镜头）。
///
/// 做法是**放大屏幕像素**，不是重新渲染：先用 `StretchBlt` 把光标周围
/// `2*lens_radius() / zoom` 见方的一块画面放大到内存位图，再 `BitBlt` 贴回窗口，
/// 最后画一圈亮边框。好处是不管文件多大、缩放几倍，代价都是一次 GDI 搬运
/// （<1 ms），也不需要知道当前是第几页、平移到哪里。
///
/// 必须在内容画完之后调用（内部用 `GetDC` 而不是 `BeginPaint`，可以和
/// `paint_bitmap_*` 的 `BeginPaint/EndPaint` 共用同一次 `WM_PAINT`）。
pub fn paint_lens(hwnd: HWND, center: (i32, i32), zoom: f32) {
    let zoom = zoom.clamp(1.5, 12.0);
    let (cx, cy) = center;
    let size = lens_radius() * 2;
    // 镜头里显示多少源像素：放大倍数越大，取的源区域越小。
    let src_half = ((lens_radius() as f32) / zoom).round().max(4.0) as i32;
    unsafe {
        let hdc = GetDC(Some(hwnd));
        if hdc.0.is_null() {
            return;
        }
        let mem = CreateCompatibleDC(Some(hdc));
        if mem.0.is_null() {
            ReleaseDC(Some(hwnd), hdc);
            return;
        }
        let bmp = CreateCompatibleBitmap(hdc, size, size);
        let old = SelectObject(mem, bmp.into());
        // 放大用 HALFTONE（平滑），缩小才需要 COLORONCOLOR；放大镜基本只放大。
        SetStretchBltMode(mem, HALFTONE);
        let _ = SetBrushOrgEx(mem, 0, 0, None);
        let _ = StretchBlt(
            mem,
            0,
            0,
            size,
            size,
            Some(hdc),
            cx - src_half,
            cy - src_half,
            src_half * 2,
            src_half * 2,
            SRCCOPY,
        );
        // 贴回：`COLORONCOLOR` 1:1 搬运，不做二次平滑。
        SetStretchBltMode(hdc, COLORONCOLOR);
        let left = cx - lens_radius();
        let top = cy - lens_radius();
        let _ = BitBlt(hdc, left, top, size, size, Some(mem), 0, 0, SRCCOPY);

        // 一圈亮边框 + 一圈暗边框，任何底色上都看得清镜头边界。
        let border = RECT {
            left,
            top,
            right: left + size - 1,
            bottom: top + size - 1,
        };
        let light = CreateSolidBrush(COLORREF(0x00E0_E0E0));
        FrameRect(hdc, &border, light);
        let _ = DeleteObject(light.into());
        let inner = RECT {
            left: left + 1,
            top: top + 1,
            right: left + size - 2,
            bottom: top + size - 2,
        };
        let dark = CreateSolidBrush(COLORREF(0x0040_4040));
        FrameRect(hdc, &inner, dark);
        let _ = DeleteObject(dark.into());

        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(Some(hwnd), hdc);
    }
}
/// 异步给某个窗口投一条消息（调用方可能在别的线程上）。
pub fn post(hwnd: HWND, msg: u32) {
    unsafe {
        let _ = PostMessageW(
            Some(hwnd),
            msg,
            windows::Win32::Foundation::WPARAM(0),
            windows::Win32::Foundation::LPARAM(0),
        );
    }
}

/// 请求重绘（worker 线程把新帧交过来之后由 UI 线程调用）。
/// 临时冻结窗口重绘（多阶段绘制时用：文字一帧、目录/滚动条另一帧，不冻就会看到"半成品"闪过）。
///
/// 用法：`set_redraw(hwnd, false)` → 画完 → `set_redraw(hwnd, true)`（true 里带一次整窗刷新）。
pub fn set_redraw(hwnd: HWND, allow: bool) {
    unsafe {
        let _ = SendMessageW(
            hwnd,
            WM_SETREDRAW,
            Some(WPARAM(usize::from(allow))),
            Some(LPARAM(0)),
        );
        if allow {
            let _ = RedrawWindow(
                Some(hwnd),
                None,
                None,
                RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW,
            );
        }
    }
}

pub fn invalidate(hwnd: HWND) {
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// 消息泵取一条消息的结果。
pub enum Pump {
    Message(MSG),
    Quit,
    Error,
}
pub fn pump() -> Pump {
    let mut msg = MSG::default();
    match unsafe { GetMessageW(&mut msg, None, 0, 0).0 } {
        0 => Pump::Quit,
        -1 => Pump::Error,
        _ => Pump::Message(msg),
    }
}

pub fn dispatch(msg: &MSG) {
    unsafe {
        let _ = TranslateMessage(msg);
        DispatchMessageW(msg);
    }
}
/// 文本测量器：持有一个 DC 与 GDI 字体，量完自动还原。
///
/// 折行需要知道"这段文字有多宽"，而宽度依赖当前字体与 DPI —— 只能问 GDI。
/// 每次 `measure` 都 `GetDC`/`ReleaseDC` 太贵（一次重画要量几十上百行），
/// 所以这里把 DC 抓一次、用完 Drop 还原。
pub struct Measurer {
    hwnd: HWND,
    hdc: HDC,
    old: HGDIOBJ,
    /// 当前这个 DC 里选中的是哪个样式的字体（换样式才重选，避免每行 SelectObject）。
    current: std::cell::Cell<usize>,
}

impl Measurer {
    /// 拿不到 DC 就返回 `None`（调用方退回"不折行"）。
    pub fn new(hwnd: HWND) -> Option<Self> {
        unsafe {
            let hdc = GetDC(Some(hwnd));
            if hdc.0.is_null() {
                return None;
            }
            // 必须与 paint_styled 用的是同一套字体/方式，否则量的宽度和画的宽度不一致
            let font = FONTS.with(|cell| cell.borrow_mut().get(TextStyle::Normal));
            let old = SelectObject(hdc, font.into());
            Some(Self {
                hwnd,
                hdc,
                old,
                current: std::cell::Cell::new(TextStyle::Normal.as_index()),
            })
        }
    }

    /// 这段文字在普通样式下的像素宽度。
    pub fn width(&self, text: &str) -> u32 {
        self.width_styled(TextStyle::Normal, text)
    }

    /// 这段文字在指定样式下的像素宽度（Markdown 折行必须按各自字体量）。
    pub fn width_styled(&self, style: TextStyle, text: &str) -> u32 {
        if text.is_empty() {
            return 0;
        }
        let idx = style.as_index();
        if self.current.get() != idx {
            let font = FONTS.with(|cell| cell.borrow_mut().get(style));
            unsafe {
                SelectObject(self.hdc, font.into());
            }
            self.current.set(idx);
        }
        let wide = crate::wide::to_wide(text);
        let body = &wide[..wide.len() - 1]; // 去掉结尾的 NUL
        let mut size = SIZE::default();
        unsafe {
            if GetTextExtentPoint32W(self.hdc, body, &mut size).as_bool() {
                size.cx.max(0) as u32
            } else {
                0
            }
        }
    }
}

impl Drop for Measurer {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.hdc, self.old);
            let _ = ReleaseDC(Some(self.hwnd), self.hdc);
        }
    }
}

/// 屏幕坐标下最外层的窗口（1.4.1：系统预览处理器的子窗口要把滚轮转给它）。
pub fn window_from_point(x: i32, y: i32) -> HWND {
    unsafe { WindowFromPoint(windows::Win32::Foundation::POINT { x, y }) }
}

/// 把滚轮事件转给指定窗口（`WM_MOUSEWHEEL` 的 lParam 是**屏幕**坐标）。
pub fn post_wheel(target: HWND, delta: i16, screen: (i32, i32)) -> bool {
    let wparam = WPARAM(((delta as i32 as u32) << 16) as usize);
    let lparam = LPARAM(((((screen.1 as u32) << 16) | (screen.0 as u32 & 0xFFFF)) as i32) as isize);
    unsafe { PostMessageW(Some(target), WM_MOUSEWHEEL, wparam, lparam).is_ok() }
}

/// 给"别人的子窗口"（系统预览处理器）准备一个宿主子窗口（1.4.1）。
///
/// 为什么不能直接把父窗口交给处理器：处理器会按**父窗口客户区**铺满自己
/// （它调 `GetClientRect(parent)`），结果盖住我们的顶栏。
/// 所以先建一个只占内容区的子窗口，处理器再怎么铺也只铺在内容区里。
pub fn create_child_host(parent: HWND, rect: RECT) -> Option<HWND> {
    const CLASS: &str = "QLRust.ChildHost";
    static REGISTERED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    unsafe {
        if REGISTERED.get().is_none() {
            let class = to_wide(CLASS);
            let wc = WNDCLASSW {
                // 同样不要 CS_HREDRAW/CS_VREDRAW：这个窗口只是容器，重画由子窗口自己负责。
                style: WNDCLASS_STYLES(0),
                lpfnWndProc: Some(host_wndproc),
                hInstance: instance_handle(),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: HBRUSH::default(),
                lpszClassName: PCWSTR(class.as_ptr()),
                ..Default::default()
            };
            RegisterClassW(&wc);
            let _ = REGISTERED.set(());
        }
        let class = to_wide(CLASS);
        let title = to_wide("QLRust 内容宿主");
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_CHILD | WS_VISIBLE,
            rect.left,
            rect.top,
            (rect.right - rect.left).max(1),
            (rect.bottom - rect.top).max(1),
            Some(parent),
            None,
            Some(instance_handle()),
            None,
        )
        .ok()?;
        Some(hwnd)
    }
}

/// 移动/改大小那个宿主子窗口。
pub fn move_child_host(hwnd: HWND, rect: RECT) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            rect.left,
            rect.top,
            (rect.right - rect.left).max(1),
            (rect.bottom - rect.top).max(1),
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// 把键盘焦点交给某个子窗口（1.7.8）。返回是否成功。
///
/// 预览窗本身是"不抢焦点"的（Office 预览才临时放开），所以点击时要把焦点**直接**
/// 交付给处理器的内容子窗口，否则滚轮/选区依然不响应。
pub fn focus_child(hwnd: HWND) -> bool {
    // `windows` crate 0.62 没给 `SetFocus` 生成绑定（和 `EnableWindow` 一样），自己声明。
    let previous = set_focus_raw(hwnd);
    !previous.is_null()
}

#[link(name = "user32")]
unsafe extern "system" {
    fn SetFocus(hwnd: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
}

fn set_focus_raw(hwnd: HWND) -> *mut core::ffi::c_void {
    unsafe { SetFocus(hwnd.0) }
}

/// 让预览窗"可以拿到焦点"（1.7.7）。
///
/// 为什么需要：Word/Excel 的预览是 **OLE 就地激活**的对象，只有容器窗口能激活、
/// 焦点能进去，滚轮/滚动条/选区才会响应。实测对照：资源管理器自己的预览窗格里
/// 同一个 xlsx 滚轮滚得很好（`_bench\drag\pane-after.png` 显示滚到了第 25–50 行），
/// 而我们这边同一条流纹丝不动——差别就是我们的窗口是 `WS_EX_NOACTIVATE`。
/// 代价：这类预览被点一下会抢焦点（资源管理器不再是前台，跟随选中项会暂停），
/// 所以只对"不接管输入的 Office 预览"开；图片/文本/压缩包/PPT 保持 NOACTIVATE。
pub fn set_activatable(hwnd: HWND, activatable: bool) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, SetWindowLongPtrW,
    };
    const WS_EX_NOACTIVATE: isize = 0x0800_0000;
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let next = if activatable {
            style & !WS_EX_NOACTIVATE
        } else {
            style | WS_EX_NOACTIVATE
        };
        if next != style {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, next);
        }
    }
}

/// 让宿主子窗口"只显示、不收输入"（1.7.4）。
///
/// 为什么需要：`IPreviewHandler` 的窗口会盖住整块内容区，鼠标按在窗口边上时事件
/// 直接进了它的窗口，我们的 `WM_LBUTTONDOWN` 根本收不到 —— 这就是"预览 Office 文件时
/// 拖边改窗口大小完全失效"的直接原因（用户 1.7.3 反馈）。
/// 禁用后再点，Windows 在命中测试里**跳过**这个子窗口，点击/滚轮回落到预览窗自己身上；
/// 处理器照样画（实测 PowerPoint 被禁用后仍然正常渲染、也还认我们投给它的按键）。
pub fn set_child_input(hwnd: HWND, enabled: bool) {
    unsafe {
        let _ = EnableWindow(hwnd, i32::from(enabled));
    }
}

/// 给"别人的窗口"投一次按键（按下 + 抬起）。`vk` 用 `VK_NEXT` / `VK_PRIOR`。
///
/// 用途：PPT 的翻页由我们接管（1.7.4）——系统预览处理器没有"跳页"接口，
/// 但实测 `PostMessage(WM_KEYDOWN, VK_NEXT)` 到它铺满内容区的那个窗口就能翻一页。
pub fn post_key(target: HWND, vk: u16) -> bool {
    let down = unsafe {
        PostMessageW(
            Some(target),
            WM_KEYDOWN,
            WPARAM(vk as usize),
            LPARAM(0x0001),
        )
        .is_ok()
    };
    unsafe {
        let _ = PostMessageW(
            Some(target),
            WM_KEYUP,
            WPARAM(vk as usize),
            LPARAM(0xC000_0001_u32 as i32 as isize),
        );
    }
    down
}

thread_local! {
    /// `largest_descendant` 的收集缓冲（EnumChildWindows 只能回传一个 LPARAM）。
    static DESCENDANTS: std::cell::RefCell<Vec<HWND>> = const { std::cell::RefCell::new(Vec::new()) };
}

unsafe extern "system" fn collect_descendant(hwnd: HWND, _param: LPARAM) -> BOOL {
    DESCENDANTS.with(|slot| slot.borrow_mut().push(hwnd));
    BOOL(1)
}

/// 找 `parent` 里**面积最大的可见后代**（1.7.4：PPT 翻页键要投给真正画幻灯片的那层）。
///
/// 为什么按面积挑：处理器的窗口层级是 `宿主(我们) → 若干 0×0/100×100 的壳 → 真正的内容窗口`，
/// 只有内容窗口铺满内容区、也只有它认翻页键（实测投给别的层没反应）。
pub fn largest_descendant(parent: HWND) -> Option<HWND> {
    DESCENDANTS.with(|slot| slot.borrow_mut().clear());
    unsafe {
        let _ = EnumChildWindows(Some(parent), Some(collect_descendant), LPARAM(0));
    }
    let mut best: Option<(HWND, i64)> = None;
    DESCENDANTS.with(|slot| {
        for hwnd in slot.borrow().iter().copied() {
            if !is_visible(hwnd) {
                continue;
            }
            let rc = window_rect(hwnd);
            let area = ((rc.right - rc.left) as i64) * ((rc.bottom - rc.top) as i64);
            if area <= 0 {
                continue;
            }
            if best.map(|(_, a)| area > a).unwrap_or(true) {
                best = Some((hwnd, area));
            }
        }
    });
    best.map(|(hwnd, _)| hwnd)
}

/// 窗口是否可见（`IsWindowVisible`：自身与所有祖先都带 WS_VISIBLE 才算）。
/// 内容宿主窗口的窗口过程：只做最基础的默认处理（它自己不画任何东西）。
unsafe extern "system" fn host_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work() -> RECT {
        RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        }
    }

    #[test]
    fn natural_rect_keeps_small_sizes_but_not_tiny() {
        // 400×300 的图片：窗口就按 400×300 开，并且居中。
        let rc = natural_rect(work(), 400, 300);
        assert_eq!(rc.right - rc.left, 400);
        assert_eq!(rc.bottom - rc.top, 300);
        assert_eq!(rc.left, (1920 - 400) / 2);
        assert_eq!(rc.top, (1040 - 300) / 2);

        // 32×32 的小图标：等比放大到"撑满最小可读尺寸"（240/32 = 7.5 倍 → 240×240），
        // 不能像旧版那样把宽高各自夹死（那会破坏宽高比）。
        let tiny = natural_rect(work(), 32, 32);
        assert_eq!(tiny.right - tiny.left, 240);
        assert_eq!(tiny.bottom - tiny.top, 240);
    }

    #[test]
    fn natural_rect_preserves_aspect_ratio_of_portrait_images() {
        // 1.0.1 的真 bug：1279×1706 的竖图曾被开成 1279×956 的横窗口 → 左右大片黑边。
        let rc = natural_rect(work(), 1279, 1706);
        let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
        assert!(w < h, "竖图必须开成竖窗口，实际 {w}x{h}");
        // 高度用满可用高度（工作区 1040 × NATURAL_FILL 0.86 = 894），宽度按比例跟随
        assert_eq!(h, 894);
        assert!((w as f32 - 894.0 * 1279.0 / 1706.0).abs() <= 1.5, "{w}");
    }

    #[test]
    fn natural_rect_clamps_huge_images_to_work_area() {
        // 8000×6000 的图不会顶到屏幕外：等比缩进"工作区 × 0.86"（1.0.8 起留 14% 余量，
        // 用户反馈"打开的时候屏幕撑得有点满"）。
        let rc = natural_rect(work(), 8000, 6000);
        let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
        assert!(w <= 1651, "{w}");
        assert!(h <= 894, "{h}");
        // 4:3 的图受高度限制 → 高度顶满，宽度 = 894 * 4/3
        assert_eq!(h, 894);
        assert!((w as f32 - 1192.0).abs() <= 1.5, "{w}");
        // 仍然居中（左右/上下留白各占一半，误差 1 px 来自整除）
        assert!((rc.left - (1920 - w) / 2).abs() <= 1);
        assert!((rc.top - (1040 - h) / 2).abs() <= 1);
    }

    #[test]
    fn zoomed_rect_follows_the_zoom_both_ways() {
        // 1.0.2：窗口要跟着缩放变大变小（基准 = 开窗尺寸，不是自然尺寸）。
        let base = (780, 1040);
        // 100%：就是这个基准尺寸
        let at100 = zoomed_rect(work(), base, 1.0);
        assert_eq!(
            (at100.right - at100.left, at100.bottom - at100.top),
            (780, 1040)
        );
        // 80%：两张边都等比缩到 0.8（624×832），且保持居中
        let at80 = zoomed_rect(work(), base, 0.8);
        assert_eq!((at80.right - at80.left, at80.bottom - at80.top), (624, 832));
        assert!((at80.left - (1920 - 624) / 2).abs() <= 1);
        assert!((at80.top - (1040 - 832) / 2).abs() <= 1);
        // 50%：宽高始终同比例（不能只缩一边）
        let at50 = zoomed_rect(work(), base, 0.5);
        let (w50, h50) = (at50.right - at50.left, at50.bottom - at50.top);
        assert_eq!((w50, h50), (390, 520));
        // 超过工作区：顶满工作区而不是跑到屏幕外
        let at400 = zoomed_rect(work(), base, 4.0);
        let (w4, h4) = (at400.right - at400.left, at400.bottom - at400.top);
        assert!(w4 <= 1920 && h4 <= 1040, "{w4}x{h4}");
        assert_eq!(h4, 1040, "3:4 的内容受高度限制 → 高度顶满");
    }

    #[test]
    fn text_palette_is_light_enough_for_dark_text() {
        // 1.0.8 起底色是白的（用户要求"所有文件默认打开，底色都是白色"），
        // 所以判据反过来：底色必须够亮，正文色必须够暗，否则白底白字看不见。
        let bg = TEXT_BACKGROUND_RGB;
        let (r, g, b) = (bg & 0xFF, (bg >> 8) & 0xFF, (bg >> 16) & 0xFF);
        assert!(r > 0xE0 && g > 0xE0 && b > 0xE0, "底色太暗：{r},{g},{b}");
        assert_eq!(bg, BITMAP_BACKGROUND_RGB, "文本与图片底色必须一致");
        let fg = TextStyle::Normal.color();
        let (fr, fg_, fb) = (fg & 0xFF, (fg >> 8) & 0xFF, (fg >> 16) & 0xFF);
        assert!(
            fr < 0x60 && fg_ < 0x60 && fb < 0x60,
            "正文色太亮：{fr},{fg_},{fb}"
        );
    }

    #[test]
    fn text_rect_uses_a_fixed_reading_width() {
        // 1.0.5：文本不再按屏幕比例摊开，固定 880 宽（1080p 上以前是 1200 宽，用户嫌大）。
        let rc = text_rect(work(), 0.72);
        assert_eq!(rc.right - rc.left, text_window_width());
        assert_eq!(rc.bottom - rc.top, (1040.0 * 0.72) as i32);
        // 仍然居中
        assert!((rc.left - (1920 - text_window_width()) / 2).abs() <= 1);
        // 窄屏：不能超过工作区 92%
        let narrow = RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 600,
        };
        let rc = text_rect(narrow, 0.72);
        assert!(rc.right - rc.left <= (800.0 * 0.92) as i32, "{rc:?}");
        assert!(rc.right <= narrow.right && rc.bottom <= narrow.bottom);
    }

    #[test]
    fn chrome_rects_place_page_box_left_of_the_pin_button() {
        // 顶栏右侧顺序（1.0.10 起）：页码框 | 锁 | ↗ | ×。
        // 画与命中测试都从 chrome_rects 取，这条测试保证四块互不重叠、顺序没被改回去。
        let rc = RECT {
            left: 0,
            top: 0,
            right: 900,
            bottom: 600,
        };
        let rects = chrome_rects(rc);
        assert_eq!(rects.close.right, rc.right);
        assert_eq!(rects.close.left, rects.external.right);
        assert_eq!(rects.pin.right, rects.external.left);
        assert!(rects.page.right <= rects.pin.left);
        assert!(rects.page.left > rc.left);
        // 内容区从顶栏下沿开始（正文不能画到黑条上）。
        assert_eq!(rects.content.top, rc.top + chrome_height());
        // 每个按钮宽度一致，且都在顶栏带内。
        for button in [rects.pin, rects.external, rects.close] {
            assert_eq!(button.right - button.left, chrome_button_w());
            assert_eq!(button.top, rc.top);
            assert_eq!(button.bottom, rc.top + chrome_height());
        }
        // 页码框比按钮窄一点、上下留 5 px，仍在同一横带里。
        assert_eq!(rects.page.right - rects.page.left, page_box_w());
        assert_eq!(rects.page.top, rc.top + 5);
    }

    #[test]
    fn clamp_into_only_moves_never_resizes() {
        let mut rc = RECT {
            left: -100,
            top: -50,
            right: 500,
            bottom: 250,
        };
        clamp_into(&mut rc, work());
        assert_eq!(
            (rc.left, rc.top),
            (0, 0),
            "越界后必须整体挪进工作区，尺寸不变"
        );
        assert_eq!((rc.right - rc.left, rc.bottom - rc.top), (600, 300));

        let mut rc = RECT {
            left: 1800,
            top: 1000,
            right: 2400,
            bottom: 1400,
        };
        clamp_into(&mut rc, work());
        assert_eq!((rc.right, rc.bottom), (1920, 1040));
        assert_eq!((rc.right - rc.left, rc.bottom - rc.top), (600, 400));

        // 比工作区还大：不动（尺寸不能为了塞进去而缩，缩放由渲染尺寸决定）
        let mut huge = RECT {
            left: 100,
            top: 100,
            right: 3000,
            bottom: 2000,
        };
        clamp_into(&mut huge, work());
        assert_eq!((huge.left, huge.top), (100, 100));
    }
}
