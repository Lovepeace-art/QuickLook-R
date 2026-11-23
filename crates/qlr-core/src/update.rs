//! 自动更新：清单解析 + 版本比较 + 检查节流（纯逻辑，无 `windows` 依赖，可跨平台单测）。
//!
//! 安全边界（重要）：
//! - 本模块**只解析和比较**，不联网、不下载、不替换文件。联网由 `qlr_platform::net` 负责，
//!   真正装包由用户点开下载页自己完成。
//! - 刻意**不做静默自替换**：本项目无代码签名、且预览内容是 in-process 的，进程内替换自身
//!   二进制/被占用的 dll 会踩一堆坑（见 `docs/dev/RELEASE.md`）。先做"检查 → 提示 → 打开下载页"。
//!
//! 兼容两种清单来源：
//! 1. 自己托管的 `latest.json`：`{ "version": "1.16.0", "url": "...", "sha256": "...", "notes": "..." }`
//! 2. GitHub `releases/latest` API：`{ "tag_name": "v1.16.0", "html_url": "...", "body": "..." }`
//!    （字段名不同，[`UpdateManifest::parse`] 会归一化）

use serde::Deserialize;

/// 环境变量：显式覆盖更新清单地址（一般用配置里的 `update_manifest_url`）。
pub const ENV_UPDATE_URL: &str = "QLRUST_UPDATE_URL";

/// 默认更新清单地址：发布仓的 GitHub `releases/latest` API。
///
/// 作为 `Config::update_manifest_url` 的默认值，这样开箱即用、不会出现"尚未配置更新源"。
/// 维护者可在 `config.json` 里改成本项目自托管的 `latest.json`，或用环境变量
/// `QLRUST_UPDATE_URL` 临时覆盖。
pub const DEFAULT_UPDATE_URL: &str =
    "https://api.github.com/repos/Lovepeace-art/QuickLook-R/releases/latest";

/// 归一化后的更新清单。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateManifest {
    /// 版本号（可能带前导 `v`，比较时会忽略）。
    pub version: String,
    /// 下载 / 发布页地址。空串 = 没有可用链接。
    pub url: String,
    /// 可选 SHA256（给自己托管的包做校验用；GitHub API 不提供）。
    pub sha256: Option<String>,
    /// 可选更新说明。
    pub notes: Option<String>,
}

/// `parse` 的宽松中间结构：同时接住自托管字段与 GitHub 字段。
#[derive(Debug, Deserialize)]
struct RawManifest {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    tag_name: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

impl UpdateManifest {
    /// 解析清单文本。缺版本号视为错误；其余字段宽松。
    pub fn parse(text: &str) -> Result<Self, String> {
        let raw: RawManifest =
            serde_json::from_str(text).map_err(|e| format!("更新清单格式错误：{e}"))?;
        let version = raw.version.or(raw.tag_name).unwrap_or_default();
        if version.trim().is_empty() {
            return Err("更新清单缺少 version / tag_name 字段".to_string());
        }
        Ok(UpdateManifest {
            version: version.trim().to_string(),
            url: raw.url.or(raw.html_url).unwrap_or_default(),
            sha256: raw.sha256.filter(|s| !s.trim().is_empty()),
            notes: raw.notes.or(raw.body),
        })
    }
}

/// 版本号拆段：忽略前导 `v`/`V` 和 `-pre`/`+build` 后缀，每段取前导数字（缺省 0）。
fn parts(v: &str) -> Vec<u64> {
    let trimmed = v.trim().trim_start_matches(['v', 'V']);
    let core = trimmed.split(['-', '+']).next().unwrap_or(trimmed);
    core.split('.')
        .map(|seg| {
            seg.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
        })
        .map(|digits| digits.parse::<u64>().unwrap_or(0))
        .collect()
}

/// `latest` 是否比 `current` 新（逐段数值比较；长度不同则缺的段按 0）。
pub fn is_newer(current: &str, latest: &str) -> bool {
    let cur = parts(current);
    let new = parts(latest);
    let n = cur.len().max(new.len());
    for i in 0..n {
        let a = *cur.get(i).unwrap_or(&0);
        let b = *new.get(i).unwrap_or(&0);
        if a != b {
            return b > a;
        }
    }
    false
}

/// 距上次检查是否已超过 `interval_hours` 小时。`last` 为 `None`（从没查过）视为该查。
pub fn should_check(last_unix: Option<i64>, now_unix: i64, interval_hours: u64) -> bool {
    match last_unix {
        None => true,
        Some(last) => {
            let interval = (interval_hours as i64).saturating_mul(3600);
            now_unix.saturating_sub(last) >= interval
        }
    }
}

/// 当前 Unix 时间（秒）。取不到就返回 0（视为很久以前，`should_check` 会判该查）。
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_self_hosted_manifest() {
        let m = UpdateManifest::parse(
            r#"{"version":"1.16.0","url":"https://x/y.zip","sha256":"abc","notes":"hi"}"#,
        )
        .unwrap();
        assert_eq!(m.version, "1.16.0");
        assert_eq!(m.url, "https://x/y.zip");
        assert_eq!(m.sha256.as_deref(), Some("abc"));
        assert_eq!(m.notes.as_deref(), Some("hi"));
    }

    #[test]
    fn parses_github_releases_shape() {
        let m = UpdateManifest::parse(
            r#"{"tag_name":"v1.17.2","html_url":"https://github.com/o/r/releases/tag/v1.17.2","body":"notes"}"#,
        )
        .unwrap();
        assert_eq!(m.version, "v1.17.2");
        assert_eq!(m.url, "https://github.com/o/r/releases/tag/v1.17.2");
        assert_eq!(m.notes.as_deref(), Some("notes"));
        assert!(m.sha256.is_none());
    }

    #[test]
    fn missing_version_is_an_error() {
        assert!(UpdateManifest::parse(r#"{"url":"https://x"}"#).is_err());
        assert!(UpdateManifest::parse("not json").is_err());
    }

    #[test]
    fn is_newer_compares_numerically() {
        assert!(is_newer("1.15.4", "1.15.5"));
        assert!(is_newer("1.15.4", "1.16.0"));
        assert!(
            is_newer("1.9.0", "1.15.0"),
            "不能按字符串比（'1.9' > '1.15' 是错的）"
        );
        assert!(is_newer("1.15.4", "v1.16.0"), "容忍前导 v");
        assert!(!is_newer("1.16.0", "1.16.0"));
        assert!(!is_newer("1.16.0", "1.15.9"));
        assert!(!is_newer("2.0.0", "1.99.99"));
        // 预发布后缀不参与比较
        assert!(!is_newer("1.16.0-beta", "1.16.0"));
    }

    #[test]
    fn should_check_respects_interval() {
        let day = 24 * 3600;
        assert!(should_check(None, 1_000_000, 24));
        assert!(!should_check(Some(1_000_000), 1_000_000 + day - 1, 24));
        assert!(should_check(Some(1_000_000), 1_000_000 + day, 24));
        assert!(should_check(Some(1_000_000), 1_000_000 + 2 * day, 24));
    }
}
