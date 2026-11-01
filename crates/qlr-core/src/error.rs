//! 统一错误类型。
//!
//! 约定：平台层把 `windows::core::Error` 用 [`Error::platform`] 包成字符串再往上抛，
//! 这样 core 不需要知道 `windows` crate。格式化里保留 HRESULT 码与系统消息，方便查日志。

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Json(serde_json::Error),
    /// Windows API 失败（`windows::core::Error` 的文本形式，含 HRESULT）。
    Platform(String),
    /// 配置非法（缺字段、取值越界等）。
    Config(String),
    /// 其它业务错误。
    Message(String),
    /// 有更新的请求把这个活取消了（1.2.0）。**不是失败**：调用方应当静默丢弃，
    /// 不要弹错误、也不要把占位文字画到窗口上。
    Cancelled,
}

impl Error {
    pub fn platform(msg: impl Into<String>) -> Self {
        Self::Platform(msg.into())
    }

    pub fn config(msg: impl Into<String>) -> Self {
        Self::Config(msg.into())
    }

    pub fn msg(msg: impl Into<String>) -> Self {
        Self::Message(msg.into())
    }

    /// 这个错误是不是"被更新的请求取消"——UI 层据此静默丢弃，不当失败处理。
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Json(e) => write!(f, "json: {e}"),
            Self::Platform(e) => write!(f, "platform: {e}"),
            Self::Config(e) => write!(f, "config: {e}"),
            Self::Message(e) => write!(f, "{e}"),
            Self::Cancelled => write!(f, "已取消（有更新的请求）"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}
