//! 应用编排（1.8.3 按职责拆成 `app/` 子模块）。
//!
//! 拆分原则：**纯搬家**。每个子模块只多一行 `use super::*;`，函数体一行没改；
//! 跨模块访问靠 `pub(crate)`（这些项原来在同一个 `mod` 里是私有的）。
//! 本文件只剩：prelude（原 `app.rs` 顶部那组 `use`）+ `mod` 声明 + 再导出——
//! 这样 `crate::app::with_state` 之类的老路径一个都不用动。

pub(crate) use crate::cli::{self, Command};
pub(crate) use crate::layout;
pub(crate) use crate::pagecache;
pub(crate) use crate::settings;
pub(crate) use crate::wrapcache;
pub(crate) use qlr_core::ipc::{self, Action, Request};
pub(crate) use qlr_core::{
    Config, Level, format, log, log_debug, log_error, log_info, log_warn, paths, plugins, version,
};
pub(crate) use qlr_platform::{
    crash, dpi, hotkey, instance, integrity, selection, shellpreview, thumbnail, tray, win11,
    window,
};
pub(crate) use qlr_render::Canvas;
pub(crate) use std::path::{Path, PathBuf};
pub(crate) use std::process::ExitCode;
pub(crate) use std::sync::atomic::{AtomicU64, Ordering};
pub(crate) use std::sync::mpsc::{self, Sender};
pub(crate) use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
pub(crate) use std::time::{Duration, Instant};
pub(crate) use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
pub(crate) use windows::Win32::Graphics::Gdi::HDC;
// 计时器 / 退出 / 时钟原语一律走 `qlr_platform::window` 的薄封装（§6-4 收口），
// 这里**不再**再导出 `GetTickCount / GetCurrentProcess / TerminateProcess /
// KillTimer / PostQuitMessage / SetTimer`——`verify-layers.ps1` 会断言 app 不许直接调。
pub(crate) use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, WM_ACTIVATE, WM_CLOSE, WM_COPYDATA, WM_DESTROY, WM_ERASEBKGND,
    WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NCCALCSIZE,
    WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SETFOCUS, WM_SIZE, WM_TIMER,
};

mod archive;
pub(crate) mod backend;
mod boot;
mod chrome;
mod input;
mod media;
mod render;
mod session;
mod shell;
mod show;

#[cfg(test)]
mod tests;

pub(crate) use archive::*;
pub(crate) use boot::*;
pub(crate) use chrome::*;
pub(crate) use input::*;
pub(crate) use media::*;
pub(crate) use render::*;
pub(crate) use session::*;
pub(crate) use shell::*;
pub(crate) use show::*;
