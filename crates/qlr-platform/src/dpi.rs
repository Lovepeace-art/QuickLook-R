//! Per-Monitor V2 DPI 感知。
//!
//! 必须在**创建任何窗口之前**调用（文档要求），否则多屏不同缩放时会糊。
//! 本机 Win10 19045 支持 V2（Windows 10 1703+）。

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};

/// 启用 Per-Monitor V2。返回是否成功（失败不致命：退化为系统缩放，只是多屏下略糊）。
pub fn enable_per_monitor_v2() -> bool {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_ok() }
}

/// 这个窗口所在显示器的 DPI（96 = 100%；取不到按 96 算）。
///
/// 为什么需要它：开了 PMv2 之后窗口坐标一律是**物理像素**，而"边缘热区 6px 好抓"
/// 这个直觉是 DIP 的。125% 屏上 6 物理像素只有 4.8 DIP，用户会觉得"贴得不够准就拖不动"
/// （1.7.3 反馈的拖边问题里就有这一条）。
pub fn window_dpi(hwnd: HWND) -> u32 {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 { 96 } else { dpi }
}

/// 把 DIP 换算成这个窗口上的物理像素（四舍五入）。
pub fn scaled(hwnd: HWND, dip: i32) -> i32 {
    ((dip as i64 * window_dpi(hwnd) as i64 + 48) / 96) as i32
}
