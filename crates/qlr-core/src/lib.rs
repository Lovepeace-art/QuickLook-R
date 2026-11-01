//! QLRust 核心库。
//!
//! 设计约束（重要，见 `docs/adr/0003-deps-and-loading-trim.md`）：
//! **本 crate 不依赖 `windows`**，只放与平台无关的东西（错误、路径、配置、日志、IPC 协议）。
//! 目的是让纯逻辑可以跨平台单测，也避免壳层被迫链接平台重依赖。
//!
//! 唯一的"非纯逻辑"是 [`plugins`]：它用 `libloading` 加载第三方 DLL。这一步仍然是
//! 平台无关的（`libloading` 在三个平台上都是同一套 API），所以放在这里而不是平台层。

pub mod config;
pub mod error;
pub mod format;
pub mod ipc;
pub mod keymap;
pub mod log;
pub mod paths;
pub mod plugins;
pub mod version;

pub use config::Config;
pub use error::{Error, Result};
pub use format::Kind;
pub use log::Level;
