//! 托盘图标与右键菜单。
//!
//! 用 `Shell_NotifyIconW` + 宿主的隐藏窗口做回调（`WM_APP+1` → `WM_RBUTTONUP` 弹菜单）。
//!
//! 图标是编译期嵌进 exe 的 32×32 PNG（`assets/qlrust-32.png`），运行时用
//! `CreateIconFromResourceEx` 转成 `HICON`。**为什么不用 `IDI_APPLICATION`**：
//! 那是一张和别的程序完全一样的通用图标，用户实测在托盘里找不到它、
//! 也认不出哪个是自己的程序（见 `docs/dev/LOG.md` 2026-10-30 条目）。
//! PNG 走 `dwVer = 0x00030000` 这条 Vista+ 路径，已在真机上验证可用。
//!
//! 设计说明：这里刻意**不用 RAII 结构体**，而是"句柄进、动作出"的自由函数。
//! 原因是 `HWND` 不是 `Send`，一旦藏进结构体就会污染上层所有共享状态；
//! 由调用方（单线程 UI）自己持有句柄最省事。

use std::sync::OnceLock;

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER, Shell_NotifyIconGetRect,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, DestroyMenu, GetCursorPos, HICON,
    IDI_APPLICATION, IMAGE_FLAGS, LoadIconW, MF_SEPARATOR, MF_STRING, SetForegroundWindow,
    TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
};
use windows::core::PCWSTR;

use crate::wide::{fill_buffer, to_wide};

/// 编译期嵌入的托盘图标（32×32 PNG）。
const ICON_PNG: &[u8] = include_bytes!("../../../assets/qlrust-32.png");

/// PNG 图标的资源版本号（Vista 起支持 PNG 压缩的 RT_ICON）。
const PNG_ICON_VERSION: u32 = 0x0003_0000;

/// 进程内只创建一次图标句柄（`HICON` 不是 `Send`，用 `isize` 存裸值）。
static ICON: OnceLock<isize> = OnceLock::new();

/// 取托盘图标句柄：优先用内嵌自定义图标，失败退回系统通用图标。
///
/// 返回值只作 `NOTIFYICONDATAW.hIcon` 用，故意不销毁（进程生命周期内复用）。
fn icon_handle() -> Option<isize> {
    let raw = *ICON.get_or_init(create_icon);
    (raw != 0).then_some(raw)
}

fn create_icon() -> isize {
    // PNG 图标：dwVer 必须是 0x00030000，cx/cy 传 0 表示"用图标自身尺寸"。
    if let Ok(h) =
        unsafe { CreateIconFromResourceEx(ICON_PNG, true, PNG_ICON_VERSION, 0, 0, IMAGE_FLAGS(0)) }
    {
        if !h.is_invalid() {
            return h.0 as isize;
        }
    }
    match unsafe { LoadIconW(None, IDI_APPLICATION) } {
        Ok(h) if !h.is_invalid() => h.0 as isize,
        _ => 0,
    }
}

/// 菜单项 id（与 `qlr-app` 里的处理分支一一对应）。
///
/// 1.0.7 起删掉了"显示预览"（`TOGGLE`）：它没有明确的"显示什么"——
/// 有文件在用就重开、没有就什么都不做，用户点了常以为坏了。开预览一律靠空格。
pub mod menu {
    pub const SETTINGS: usize = 101;
    pub const ABOUT: usize = 102;
    pub const EXIT: usize = 103;
}

/// 添加托盘图标。`callback_msg` 建议传 `qlr_core::ipc::WM_TRAY_CALLBACK`。
pub fn add(hwnd: HWND, id: u32, tooltip: &str, callback_msg: u32) -> windows::core::Result<()> {
    let mut data = make_data(hwnd, id, callback_msg);
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    fill_buffer(&mut data.szTip, tooltip);
    if unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
        Ok(())
    } else {
        Err(windows::core::Error::from_thread())
    }
}

/// 弹一个气泡提示（`NIF_INFO`）。
///
/// 用途：程序启动后是纯后台状态（没有窗口也没有任务栏按钮），
/// 用户很容易不知道"到底起来了没有"。启动时弹一次气泡告诉他怎么用、怎么退出。
/// 注意：Windows 10 会把气泡显示在通知中心，且"专注助手"开启时可能被静音——这只是提示，失败不致命。
pub fn notify(hwnd: HWND, id: u32, title: &str, message: &str) -> bool {
    let mut data = make_data(hwnd, id, qlr_core::ipc::WM_TRAY_CALLBACK);
    data.uFlags = NIF_INFO;
    data.dwInfoFlags = NIIF_INFO;
    fill_buffer(&mut data.szInfoTitle, title);
    fill_buffer(&mut data.szInfo, message);
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool()
}

/// 只看提示文字变更时调用（`NIM_MODIFY`）。
pub fn set_tooltip(hwnd: HWND, id: u32, tooltip: &str) -> bool {
    let mut data = make_data(hwnd, id, qlr_core::ipc::WM_TRAY_CALLBACK);
    data.uFlags = NIF_TIP;
    fill_buffer(&mut data.szTip, tooltip);
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool()
}

/// 移除图标。退出前必须调用，否则托盘里会留下"僵尸图标"直到鼠标划过。
pub fn remove(hwnd: HWND, id: u32) -> bool {
    let data = make_data(hwnd, id, 0);
    unsafe { Shell_NotifyIconW(NIM_DELETE, &data) }.as_bool()
}

/// 弹出右键菜单并返回被选中的 id（0 = 用户取消）。
///
/// 注意 `TrackPopupMenu` 前必须 `SetForegroundWindow`，否则点菜单外不会消失
/// （Win32 托盘菜单的经典坑）。
pub fn show_menu(hwnd: HWND) -> usize {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return 0;
        };
        let settings = to_wide("设置…");
        let about = to_wide("关于");
        let exit = to_wide("退出");

        let _ = AppendMenuW(menu, MF_STRING, menu::SETTINGS, PCWSTR(settings.as_ptr()));
        let _ = AppendMenuW(menu, MF_STRING, menu::ABOUT, PCWSTR(about.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, menu::EXIT, PCWSTR(exit.as_ptr()));

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);

        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            Some(0),
            hwnd,
            None,
        )
        .0;

        let _ = DestroyMenu(menu);
        cmd.max(0) as usize
    }
}

/// 查询图标当前在屏幕上的矩形。
///
/// 用途：`Shell_NotifyIconW(NIM_ADD)` 返回成功**不等于**用户能看见图标
/// （可能被塞进 `^` 溢出区，或被"隐藏的图标"设置关掉了）。
/// 这是唯一能问 Shell "我的图标现在落在哪儿"的接口。
///
/// **实测结论（2026-10-30 二次验证）：返回值是准的**。图标加完它报
/// `(1683,1040)-(1707,1080)`，截屏找到的新图标确实在 `x=1687..1702`，
/// 完全吻合。
///
/// 曾经误判"它偏了 100 px"，真相是**上一条进程被 `Stop-Process` 杀掉后，
/// 它的图标还赖在托盘上（僵尸图标，鼠标划过托盘区才会消失）**，
/// 而我把那个僵尸（在 `x=1787`）当成了新图标。教训：核对托盘位置前，
/// 必须先把所有旧实例杀干净并让鼠标扫过托盘区，再重新截屏。
/// 已知偏差只有一处：高度报 40（含图标上下的内边距），不是 24。
pub fn rect(hwnd: HWND, id: u32) -> Option<RECT> {
    let identifier = NOTIFYICONIDENTIFIER {
        cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: hwnd,
        uID: id,
        guidItem: Default::default(),
    };
    unsafe { Shell_NotifyIconGetRect(&identifier) }.ok()
}

/// 切换通知协议版本。
///
/// **目前故意不调用**：`NOTIFYICON_VERSION_4` 会把回调语义改掉
/// （右键从 `WM_RBUTTONUP` 变成 `WM_CONTEXTMENU`，左键单击变 `NIN_SELECT`，
/// 坐标搬进 `wParam`），而 `qlr-app` 的 `on_tray` 是按 v0/v3 语义写的。
/// 想启用得同时改 `on_tray`，等真的需要"单击托盘"再一起动。
pub fn set_version(hwnd: HWND, id: u32, version: u32) -> bool {
    let mut data = make_data(hwnd, id, qlr_core::ipc::WM_TRAY_CALLBACK);
    data.uFlags = NIF_MESSAGE;
    data.Anonymous.uVersion = version;
    unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) }.as_bool()
}

fn make_data(hwnd: HWND, id: u32, callback_msg: u32) -> NOTIFYICONDATAW {
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = id;
    data.uFlags = NIF_MESSAGE;
    data.uCallbackMessage = callback_msg;
    if let Some(raw) = icon_handle() {
        data.hIcon = HICON(raw as *mut core::ffi::c_void);
    }
    data
}
