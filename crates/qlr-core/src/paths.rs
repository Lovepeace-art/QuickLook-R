//! 路径策略：便携模式与安装模式。
//!
//! 与 QuickLook 保持一致的判定方式（`QuickLook.Common/Helpers/SettingHelper.cs:63-69`）：
//! **exe 同目录存在 `portable.lock` 即便携模式**，数据写 `<exe目录>\UserData\`；
//! 否则写 `%LOCALAPPDATA%\QLRust\`。
//!
//! 便携模式对我们特别重要：M0 对拍时必须在 `D:` 上跑，且整个程序只该有一个 exe + 一个数据目录。

use std::path::PathBuf;

use crate::error::Result;

/// exe 所在目录（取不到时退化到当前目录，避免 panic）。
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 是否便携模式（exe 同目录有 `portable.lock`）。
pub fn is_portable() -> bool {
    exe_dir().join("portable.lock").exists()
}

/// 数据根目录。
pub fn data_dir() -> PathBuf {
    if is_portable() {
        return exe_dir().join("UserData");
    }
    match std::env::var_os("LOCALAPPDATA") {
        Some(v) => PathBuf::from(v).join("QLRust"),
        // 极端环境（无 LOCALAPPDATA）退化到便携目录，保证程序仍能跑。
        None => exe_dir().join("UserData"),
    }
}

pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

pub fn log_file() -> PathBuf {
    logs_dir().join("app.log")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

/// 建好必要目录（幂等）。
pub fn ensure_dirs() -> Result<()> {
    std::fs::create_dir_all(data_dir())?;
    std::fs::create_dir_all(logs_dir())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_is_absolute_or_exe_relative() {
        let d = data_dir();
        assert!(
            d.is_absolute() || d.starts_with("."),
            "数据目录应当是绝对路径（当前 {d:?}）"
        );
    }

    #[test]
    fn log_file_sits_under_logs_dir() {
        assert!(log_file().starts_with(logs_dir()));
    }
}
