//! QLRust 平台层。
//!
//! 职责边界（见 `docs/dev/ARCH.md`）：
//! - **只做 FFI 薄封装**，不写业务决策（业务在 `qlr-app`）；
//! - 每个函数失败都返回 `windows::core::Error` 或 `Option`，由调用方决定是降级还是致命；
//! - 命名统一：`xxx_yyy`，不做类型再包装（直接用 `windows` crate 的句柄类型，减少转换层）。

pub mod audio;
pub mod crash;
pub mod dpi;
pub mod hotkey;
pub mod instance;
pub mod integrity;
pub mod selection;
pub mod shellpreview;
pub mod tray;
pub mod wide;
pub mod win11;
pub mod window;

use qlr_core::error::Error;

/// 把 `windows::core::Error` 转成 core 的统一错误（保留 HRESULT，便于查日志）。
pub fn to_core(e: windows::core::Error) -> Error {
    Error::platform(format!("{e} (HRESULT=0x{:08X})", e.code().0))
}
