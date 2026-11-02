//! 可执行体入口。
//!
//! `windows_subsystem = "windows"`：不弹控制台窗口（双击不闪黑框）。
//! 代价是没有 stdout/stderr，所以所有诊断信息都走
//! `%LOCALAPPDATA%\QLRust\logs\app.log`（便携模式在 `<exe>\UserData\logs\`）。

#![windows_subsystem = "windows"]

mod app;
mod cli;
mod layout;
mod pagecache;
mod settings;

fn main() -> std::process::ExitCode {
    app::run()
}
