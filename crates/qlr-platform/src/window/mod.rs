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
    GetTextExtentExPointW, GetTextExtentPoint32W, HALFTONE, HBITMAP, HBRUSH, HDC, HFONT, HGDIOBJ,
    InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, OUT_TT_PRECIS,
    PAINTSTRUCT, RDW_ALLCHILDREN, RDW_INVALIDATE, RDW_UPDATENOW, RedrawWindow, ReleaseDC, SRCCOPY,
    SelectObject, SetBkMode, SetBrushOrgEx, SetStretchBltMode, SetTextColor, StretchBlt,
    StretchDIBits, TRANSPARENT,
};
use windows::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_NODEFAULT, SND_SYSTEM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
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
    MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MSG, MessageBoxW, PostMessageW, PostQuitMessage,
    RegisterClassW, SM_CXSCREEN, SM_CYSCREEN, SW_SHOW, SW_SHOWNOACTIVATE, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SendMessageW, SetCursor,
    SetForegroundWindow, SetTimer, SetWindowPos, ShowWindow, TranslateMessage, ULW_ALPHA,
    UpdateLayeredWindow, WINDOW_EX_STYLE, WINDOW_STYLE, WM_KEYDOWN, WM_KEYUP, WM_MOUSEWHEEL,
    WM_SETREDRAW, WNDCLASS_STYLES, WNDCLASSW, WNDPROC, WS_CHILD, WS_VISIBLE, WindowFromPoint,
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

/// 一次停掉多个定时器（`KillTimer` 循环包装）。
///
/// app 层不直接碰 Win32 计时器：批量停用这个，单停用 [`stop_timer`]。
pub fn stop_timers(hwnd: HWND, ids: &[usize]) {
    for &id in ids {
        stop_timer(hwnd, id);
    }
}

/// 请求消息泵退出（`PostQuitMessage(0)`）。只应在 UI 线程调用。
pub fn post_quit() {
    unsafe {
        PostQuitMessage(0);
    }
}

/// 强制结束当前进程（退出看门狗用）。
///
/// 用 `TerminateProcess` 而不是 `std::process::exit`：**不触发 DLL detach**
/// （卡住清理的正是它），也不等任何线程收尾。
pub fn terminate_self(code: u32) {
    unsafe {
        let _ = TerminateProcess(GetCurrentProcess(), code);
    }
}

/// 单调毫秒计数（`GetTickCount`，约 49.7 天回绕一次）。给钩子时间戳用。
pub fn now_ms() -> u32 {
    unsafe { GetTickCount() }
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
        // 1.8.3 修 GDI 泄漏：这里以前只有 `GetDC` 没有 `ReleaseDC`，注释还写着
        // "DC 由调用方持有/释放"——但函数既不返回 DC，唯一调用点
        // （`app.rs::paint_overlays`）也拿不到它，于是每画一帧就漏 1 个 GDI 对象。
        // 表现为"每预览一个文件 GDI +1、收起不回收"：每次 show 至少跑一次
        // `paint_overlays`，而 `ReleaseDC` 一次都没有。现在配对释放，
        // 和同文件的 `paint_scrollbar` / `paint_toc` 保持一致。
        ReleaseDC(Some(hwnd), hdc);
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

/// 当前进程的 GDI / USER 对象数（诊断用）。
///
/// 为什么需要它：现场报障"每预览一个文件，GDI 对象就 +1 且收起不释放"。
/// GDI 对象是进程级配额（默认 1 万），一天几百次预览就会累积到图形界面开始掉字。
/// 这个函数只读计数，成本是一次系统调用，只在 `show`/`hide` 各打一行日志时用。
/// 进程句柄失效等异常情况返回 `(0, 0)`——诊断信息不该影响主流程。
pub fn gui_resources() -> (u32, u32) {
    use windows::Win32::System::Threading::{
        GR_GDIOBJECTS, GR_USEROBJECTS, GetCurrentProcess, GetGuiResources,
    };
    unsafe {
        (
            GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS),
            GetGuiResources(GetCurrentProcess(), GR_USEROBJECTS),
        )
    }
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

// ---------------------------------------------------------------------------
// 设置页（1.0.7）：两列表格 + 行高亮
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 顶栏（1.0.9）：所有格式统一的黑色工具条
//
// 为什么不像系统窗口那样给每种格式单独的标题栏：预览窗是无边框的（去掉了系统
// 标题栏才能做亚克力/圆角），所以"标题 + 按钮"必须自己画。既然自己画，就顺手让
// 四种格式（图片/PDF/文本/Markdown+代码）长得一模一样——用户不用为每种格式重新
// 学一遍"关闭按钮在哪"。
// ---------------------------------------------------------------------------

/// 窗口最外圈那三圈"阴影"：由深到浅，让白底窗口和白底程序分得开边界。
/// 发丝边的颜色（1.0.10 定稿：投影交给分层窗口，客户区只留一条 1 px）。
pub const FRAME_HAIRLINE_RGB: u32 = 0x00C8_C8C8;

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

// ---------------------------------------------------------------------------
// 滚动条与目录栏（1.0.9）
// ---------------------------------------------------------------------------

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

// —— 子模块（2026-11-04 纯搬家；每块只多一行 use super::*）——
mod audio;
mod bitmap;
mod chrome;
mod paint;
mod scroll;
mod settings;
mod shadow;
mod sheet;
mod sheet_tabs;
mod text;
mod toc;

pub use audio::*;
pub use bitmap::*;
pub use chrome::*;
pub(crate) use paint::*;
pub use scroll::*;
pub use settings::*;
pub use shadow::*;
pub use sheet::*;
pub use sheet_tabs::*;
pub use text::*;
pub use toc::*;
