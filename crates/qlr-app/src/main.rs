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

// 帧级分配计数：只有 `--features alloc-stats` 时才编进来（默认零开销）。
// 开了之后全局分配器换成记账版本，`RENDER` 旁边会多一行 `RENDERALLOC`。
#[cfg(feature = "alloc-stats")]
mod alloc_stats;

#[cfg(feature = "alloc-stats")]
#[global_allocator]
static ALLOC: alloc_stats::Counting = alloc_stats::Counting;

fn main() -> std::process::ExitCode {
    app::run()
}
