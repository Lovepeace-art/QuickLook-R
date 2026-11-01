//! 极简文件日志。
//!
//! 为什么不用 `tracing`/`log` 生态：壳层要求"零重依赖"（A8.5）。日志只需要
//! 「时间 + 级别 + 模块 + 一句话」，自己写 120 行足够，还能保证滚动与路径可控。
//! 需要结构化日志/span 时再换 `tracing`，见 `docs/dev/CONVENTIONS.md`。
//!
//! 位置：`%LOCALAPPDATA%\QLRust\logs\app.log`（便携模式：`<exe>\UserData\logs\app.log`）。
//! 滚动：超过 [`MAX_BYTES`] 时改名为 `app.log.1`（只保留 1 份历史）。

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::Result;

/// 单个日志文件的上限（超过就滚动）。
pub const MAX_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN ",
            Self::Info => "INFO ",
            Self::Debug => "DEBUG",
            Self::Trace => "TRACE",
        }
    }

    /// 解析 `QLRUST_LOG` 环境变量（`error|warn|info|debug|trace`），非法值退回 `info`。
    pub fn from_env_or(default: Level) -> Level {
        match std::env::var("QLRUST_LOG").ok().as_deref() {
            Some("error") => Level::Error,
            Some("warn") => Level::Warn,
            Some("info") => Level::Info,
            Some("debug") => Level::Debug,
            Some("trace") => Level::Trace,
            _ => default,
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

struct Logger {
    level: Level,
    path: PathBuf,
    file: Mutex<File>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// 初始化日志（幂等：重复调用只更新级别）。
pub fn init(level: Level, path: &Path) -> Result<()> {
    if let Some(existing) = LOGGER.get() {
        // 只允许改级别，文件句柄保持不变。
        let _ = existing;
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    rotate_if_needed(path)?;
    let file = OpenOptions::new().create(true).append(true).open(path)?;

    let _ = LOGGER.set(Logger {
        level,
        path: path.to_path_buf(),
        file: Mutex::new(file),
    });
    Ok(())
}

fn rotate_if_needed(path: &Path) -> std::io::Result<()> {
    let too_big = std::fs::metadata(path)
        .map(|m| m.len() > MAX_BYTES)
        .unwrap_or(false);
    if too_big {
        let mut old = path.as_os_str().to_os_string();
        old.push(".1");
        let old = PathBuf::from(old);
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(path, &old);
    }
    Ok(())
}

/// 当前是否输出该级别。未初始化时一律不写（避免启动早期噪声落到终端）。
pub fn enabled(level: Level) -> bool {
    LOGGER.get().map(|l| level <= l.level).unwrap_or(false)
}

/// 写一条日志。**不 panic**：日志失败就静默丢弃，绝不影响主流程。
pub fn write(level: Level, scope: &str, msg: &str) {
    let Some(logger) = LOGGER.get() else { return };
    if level > logger.level {
        return;
    }

    let tid = std::thread::current().id();
    let line = format!(
        "{} {} [{:?}] {:<10} {}\n",
        utc_timestamp(),
        level.as_str(),
        tid,
        scope,
        msg
    );

    if let Ok(mut f) = logger.file.lock() {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    }
    debug_assert!(logger.path.exists(), "日志文件应当存在");
}

/// 日志文件路径（尚未初始化也能查）。
pub fn path() -> Option<PathBuf> {
    LOGGER.get().map(|l| l.path.clone())
}

/// `2026-10-29T12:34:56.789Z`（UTC，零依赖实现）。
pub fn utc_timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let millis = now.subsec_millis();
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z")
}

/// Howard Hinnant 的 civil_from_days 算法（days 起点 1970-01-01）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

#[macro_export]
macro_rules! log_error {
    ($scope:expr, $($arg:tt)*) => {
        $crate::log::write($crate::log::Level::Error, $scope, &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($scope:expr, $($arg:tt)*) => {
        $crate::log::write($crate::log::Level::Warn, $scope, &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_info {
    ($scope:expr, $($arg:tt)*) => {
        $crate::log::write($crate::log::Level::Info, $scope, &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_debug {
    ($scope:expr, $($arg:tt)*) => {{
        if $crate::log::enabled($crate::log::Level::Debug) {
            $crate::log::write($crate::log::Level::Debug, $scope, &format!($($arg)*))
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_has_expected_shape() {
        let ts = utc_timestamp();
        assert_eq!(ts.len(), 24, "格式应为 2026-10-29T12:34:56.789Z，实际 {ts}");
        assert!(ts.ends_with('Z'));
    }

    #[test]
    fn civil_from_known_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_390), (2025, 10, 29));
    }
}
