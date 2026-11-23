//! 自动更新检查的编排（纯流程）。
//!
//! 分工：版本/清单逻辑在 `qlr_core::update`（可单测），网络与打开浏览器在
//! `qlr_platform::net`，本模块只负责「什么时候查、查到后怎么提示」。
//!
//! 设计原则：**绝不静默下载/替换**。检查到新版本 → 托盘气泡 + 日志；用户点托盘
//! 「检查更新」→ 若已有可用版本就打开下载页，否则现场查一次。见 `qlr_core::update` 注释。

use super::*;

use qlr_core::update as upd;

/// 每天最多自动查一次（用户手动点不受此限）。
const AUTO_INTERVAL_HOURS: u64 = 24;
/// 拉取清单的超时。
const FETCH_TIMEOUT_MS: u32 = 8000;

/// 进程内缓存「已发现的可用版本」，供 UI / 托盘菜单二次点开下载页。
static AVAILABLE: Mutex<Option<upd::UpdateManifest>> = Mutex::new(None);

/// 已有可用新版本吗？（供将来的顶栏/设置页读取）
pub(crate) fn available() -> Option<upd::UpdateManifest> {
    AVAILABLE.lock().ok().and_then(|g| g.clone())
}

/// 启动时调用：开启且到期才在后台静默查一次。
pub(crate) fn startup_check(host: HWND, config: Config) {
    if !config.update_check {
        return;
    }
    if !upd::should_check(
        config.last_update_check,
        upd::now_unix(),
        AUTO_INTERVAL_HOURS,
    ) {
        log_debug!(
            SCOPE,
            "自动更新：距上次检查不足 {AUTO_INTERVAL_HOURS} 小时，跳过"
        );
        return;
    }
    spawn_check(host, config, false);
}

/// 托盘「检查更新」调用：无论是否到期都查一次，并给用户反馈。
pub(crate) fn manual_check(host: HWND, config: Config) {
    spawn_check(host, config, true);
}

/// 托盘「检查更新」的完整语义：已有可用版本 → 打开下载页；否则现场查一次。
pub(crate) fn check_or_open(host: HWND, config: Config) {
    if let Some(m) = available()
        && !m.url.is_empty()
        && qlr_platform::net::open_url(&m.url)
    {
        log_info!(SCOPE, "已打开下载页：{}", m.url);
        return;
    }
    manual_check(host, config);
}

fn spawn_check(host: HWND, config: Config, interactive: bool) {
    // HWND 不是 Send：只搬裸值进线程，在线程里重建（tray 也需要它来弹气泡）。
    let host_raw = host.0 as isize;
    std::thread::spawn(move || {
        let host = HWND(host_raw as *mut core::ffi::c_void);

        // 优先级：环境变量 > 配置 > 内置默认源（发布仓 releases/latest）。
        let url = std::env::var(upd::ENV_UPDATE_URL)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                let s = config.update_manifest_url.trim().to_string();
                if s.is_empty() { None } else { Some(s) }
            })
            .unwrap_or_else(|| upd::DEFAULT_UPDATE_URL.to_string());
        let url = url.trim().to_string();

        match qlr_platform::net::get_string(&url, FETCH_TIMEOUT_MS) {
            Ok(text) => match upd::UpdateManifest::parse(&text) {
                Ok(m) => {
                    mark_checked();
                    let newer = upd::is_newer(version::VERSION, &m.version);
                    let skipped = config.skipped_version.as_deref() == Some(m.version.as_str());
                    if newer && !skipped {
                        if let Ok(mut g) = AVAILABLE.lock() {
                            *g = Some(m.clone());
                        }
                        log_info!(
                            SCOPE,
                            "自动更新：发现新版本 {}（当前 {}），下载页 {}",
                            m.version,
                            version::VERSION,
                            if m.url.is_empty() {
                                "（清单未给 url）"
                            } else {
                                &m.url
                            }
                        );
                        tray::notify(
                            host,
                            TRAY_ID,
                            "QLRust 有新版本",
                            &format!("v{} 可用，右键托盘 → 检查更新 打开下载页", m.version),
                        );
                    } else if interactive {
                        let msg = if newer && skipped {
                            format!("v{} 已被你跳过", m.version)
                        } else {
                            format!("已是最新版本 v{}", version::VERSION)
                        };
                        log_info!(SCOPE, "手动检查更新：{msg}");
                        tray::notify(host, TRAY_ID, "检查更新", &msg);
                    }
                }
                Err(e) => {
                    log_warn!(SCOPE, "更新清单解析失败：{e}");
                    if interactive {
                        tray::notify(host, TRAY_ID, "检查更新", &format!("清单格式不对：{e}"));
                    }
                }
            },
            Err(e) => {
                log_warn!(SCOPE, "拉取更新清单失败：{e}");
                if interactive {
                    tray::notify(host, TRAY_ID, "检查更新", &format!("联网检查失败：{e}"));
                }
            }
        }
    });
}

/// 记录本次检查时间（后台线程直接读写磁盘上的配置，避免碰会话状态）。
fn mark_checked() {
    let path = paths::config_path();
    let (mut config, _err) = Config::load(&path);
    config.last_update_check = Some(upd::now_unix());
    if let Err(e) = config.save(&path) {
        log_warn!(SCOPE, "记录更新检查时间失败：{e}");
    }
}
