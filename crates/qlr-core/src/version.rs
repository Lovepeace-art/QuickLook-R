//! 进程身份与版本常量。

/// 程序名（日志、托盘提示、窗口标题用）。
pub const APP_NAME: &str = "QLRust";

/// 版本号，来自 Cargo。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 用户可见的完整标识，例如 `QLRust 0.1.0`。
pub fn banner() -> String {
    format!("{APP_NAME} {VERSION}")
}
