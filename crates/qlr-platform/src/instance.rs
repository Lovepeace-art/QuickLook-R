//! 单实例 + 请求转交。
//!
//! 机制（`docs/adr/0002-single-instance-and-ipc.md`）：
//! 1. 命名互斥体 `Local\QLRust.Instance` 判重（`Local\` = 当前登录会话）；
//! 2. 后启动的进程用 `FindWindowW` 找到常驻实例的隐藏宿主窗口；
//! 3. 用 `WM_COPYDATA` 把 JSON 请求送过去，内核负责跨进程封送；
//! 4. 送完即退出。
//!
//! 这样"第二次启动 = 一次 IPC"，既省内存又天然实现了"预览同一个文件不重复开窗"。

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, WPARAM,
};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, SendMessageW, WM_COPYDATA};
use windows::core::PCWSTR;

use crate::wide::to_wide;

/// 持有互斥体句柄；`Drop` 时释放，进程退出/崩溃都会自动释放（内核保证）。
pub struct InstanceGuard {
    handle: HANDLE,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// 尝试成为唯一实例。
///
/// - `Ok(Some(guard))`：本进程是主实例，`guard` 必须活到进程结束；
/// - `Ok(None)`：已有主实例在跑，调用方应转交请求后退出。
pub fn acquire(mutex_name: &str) -> windows::core::Result<Option<InstanceGuard>> {
    let name = to_wide(mutex_name);
    let handle = unsafe { CreateMutexW(None, true, PCWSTR(name.as_ptr())) }?;
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Ok(None);
    }
    Ok(Some(InstanceGuard { handle }))
}

/// 找到常驻实例的宿主窗口（类名固定，`FindWindowW` 能搜到隐藏窗口）。
pub fn find_host(class: &str) -> Option<HWND> {
    let name = to_wide(class);
    unsafe { FindWindowW(PCWSTR(name.as_ptr()), PCWSTR::null()) }.ok()
}

/// 把请求载荷发给宿主窗口。
///
/// `WM_COPYDATA` 是同步消息：本函数返回时对方已经处理完（除非对方卡住）。
/// 发送方必须保持 `payload` 在其处理期间有效——这里由 `data` 的借用作用域保证。
pub fn send_request(hwnd: HWND, payload: &[u8]) -> windows::core::Result<()> {
    let mut data = payload.to_vec();
    let cds = COPYDATASTRUCT {
        dwData: qlr_core::ipc::COPYDATA_MAGIC,
        cbData: data.len() as u32,
        lpData: data.as_mut_ptr().cast(),
    };
    unsafe {
        SendMessageW(
            hwnd,
            WM_COPYDATA,
            Some(WPARAM(0)),
            Some(LPARAM((&raw const cds) as isize)),
        );
    }
    Ok(())
}

/// 从 `WM_COPYDATA` 的 `lParam` 里取出载荷；不是我们的消息则返回 `None`。
///
/// 安全性：指针由系统在消息处理期间保证有效，本函数只在处理 `WM_COPYDATA` 时调用。
pub fn copy_data_payload(lparam: LPARAM) -> Option<Vec<u8>> {
    if lparam.0 == 0 {
        return None;
    }
    let cds = unsafe { &*(lparam.0 as *const COPYDATASTRUCT) };
    if cds.dwData != qlr_core::ipc::COPYDATA_MAGIC || cds.lpData.is_null() || cds.cbData == 0 {
        return None;
    }
    if cds.cbData as usize > qlr_core::ipc::MAX_PAYLOAD {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize) };
    Some(bytes.to_vec())
}
