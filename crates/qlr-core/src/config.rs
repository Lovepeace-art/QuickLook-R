//! 用户配置。
//!
//! 格式：JSON（不是 TOML）。理由见 `docs/adr/0004-config-and-logging.md`：
//! 只需要 `serde_json` 一个依赖，且与 IPC 协议共用同一套序列化，省一次选型成本。
//!
//! 兼容策略：`#[serde(default)]` + 忽略未知字段，所以**新增字段是向后兼容的**；
//! 破坏性改动必须提升 [`Config::schema_version`] 并在 `migrate` 里写迁移。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::keymap::Hotkeys;

pub const CURRENT_SCHEMA: u32 = 2;

/// 投机预渲染档位（1.1.0）。
///
/// 干什么：窗口**没开着**时，悄悄把资源管理器里"当前选中的那一个文件"先渲染好，
/// 用户按空格时直接把现成的帧贴上去。
///
/// 代价（实测）：4K 图 ≈18 ms、48 MP ≈60 ms 的一次性 CPU，加上常驻 2–6 MB。
/// 所以默认给 [`Prewarm::Adaptive`]：只有"用户最近真的用空格预览过"才动手，
/// 免得给一个只把 QLRust 当托盘程序挂着的人白烧 CPU。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Prewarm {
    /// 关。完全不轮询、不渲染。
    Off,
    /// 自适应（默认）：最近 [`Prewarm::RECENT`] 内真的预览过才预热。
    #[default]
    Adaptive,
    /// 总是：只要前台是资源管理器、且选中了单个文件就预热。
    Always,
}

impl Prewarm {
    /// "最近用过"的判定窗口：5 分钟。按空格是连续动作（看一批图/一批日志），
    /// 5 分钟足够覆盖一段连续工作，又不至于让闲置两小时的人继续被轮询。
    pub const RECENT: std::time::Duration = std::time::Duration::from_secs(300);

    /// 配置里的字符串形式（写进 `config.json`）。
    pub fn name(self) -> &'static str {
        match self {
            Prewarm::Off => "off",
            Prewarm::Adaptive => "adaptive",
            Prewarm::Always => "always",
        }
    }

    /// 给设置页/日志看的短标签。
    pub fn label(self) -> &'static str {
        match self {
            Prewarm::Off => "关",
            Prewarm::Adaptive => "自适应",
            Prewarm::Always => "总是",
        }
    }

    /// 设置页里点一下换下一档（关 → 自适应 → 总是 → 关）。
    pub fn cycle(self) -> Self {
        match self {
            Prewarm::Off => Prewarm::Adaptive,
            Prewarm::Adaptive => Prewarm::Always,
            Prewarm::Always => Prewarm::Off,
        }
    }

    /// 不管最近有没有预览过，都要预热吗？
    pub fn is_on(self) -> bool {
        self != Prewarm::Off
    }
}

/// 宽容地读 [`Prewarm`]：未知/缺失/类型不对一律当自适应。
///
/// 为什么不直接用 `#[derive(Deserialize)]`：枚举遇到不认识的字符串会**整个配置解析失败**，
/// 于是用户手改一个拼错的档位，连日志级别、窗口尺寸、快捷键全被打回默认——
/// 一个小字段的笔误不该有这种后果。
fn de_prewarm<'de, D>(deserializer: D) -> std::result::Result<Prewarm, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    Ok(match raw.as_deref() {
        Some("off") => Prewarm::Off,
        Some("always") => Prewarm::Always,
        _ => Prewarm::Adaptive,
    })
}

/// 某个格式类别用「内置渲染」还是「保真」(系统 IPreviewHandler) 承载。
///
/// 默认 [`BackendPref::Native`]：QLRust 自己画（快、省内存、观感一致）。选 `Shell`
/// 时交给系统预览处理器（真排版，但要装 Office/对应程序，慢、占上百 MB）。系统没有
/// 对应处理器时后端解析会自动回退内置，不会出现空白窗——见 `app::backend::resolve`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendPref {
    /// 内置渲染（默认，与历史行为一致）。
    #[default]
    Native,
    /// 保真：交给系统预览处理器（IPreviewHandler）。
    Shell,
}

impl BackendPref {
    /// 写进 `config.json` 的字符串形式。
    pub fn name(self) -> &'static str {
        match self {
            BackendPref::Native => "native",
            BackendPref::Shell => "shell",
        }
    }

    /// 设置页/日志用的短标签。
    pub fn label(self) -> &'static str {
        match self {
            BackendPref::Native => "内置",
            BackendPref::Shell => "保真",
        }
    }

    /// 设置页点一下换一档（内置 ↔ 保真）。
    pub fn cycle(self) -> Self {
        match self {
            BackendPref::Native => BackendPref::Shell,
            BackendPref::Shell => BackendPref::Native,
        }
    }
}

/// 设置页里可按格式切换后端的固定清单：`(format::Kind::name(), 显示名)`。
///
/// 只收「有内置渲染、值得做保真对照」的族；长尾（无内置）统一由
/// `shell_for_unsupported` 一个开关控制，避免每加一种格式就往设置页塞一行。
pub const BACKEND_KINDS: &[(&str, &str)] = &[
    ("document", "Word 文档（docx）"),
    ("presentation", "演示文稿（pptx）"),
    ("sheet", "电子表格（xlsx）"),
    ("pdf", "PDF"),
];

/// 宽容地读「每格式后端偏好」：未知 kind 或写坏的档位一律跳过，**绝不让它炸掉整份配置**。
///
/// 与 [`de_prewarm`] 同一个理由：一个手改错的小字段不该把快捷键/窗口尺寸全打回默认。
fn de_backend_prefs<'de, D>(
    deserializer: D,
) -> std::result::Result<std::collections::BTreeMap<String, BackendPref>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<std::collections::BTreeMap<String, String>>::deserialize(deserializer)?;
    let mut out = std::collections::BTreeMap::new();
    if let Some(map) = raw {
        for (kind, value) in map {
            match value.to_ascii_lowercase().as_str() {
                "native" => {
                    out.insert(kind, BackendPref::Native);
                }
                "shell" => {
                    out.insert(kind, BackendPref::Shell);
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 配置结构版本；用于将来迁移，不要手改。
    pub schema_version: u32,

    /// 是否启用亚克力背景（Win10 走未文档化 API，失败会自动降级）。
    /// 预览窗口是否置顶。
    pub topmost: bool,

    /// 预览窗口宽 / 工作区宽。
    pub width_ratio: f32,
    /// 预览窗口高 / 工作区高。
    pub height_ratio: f32,

    /// 长按判定（毫秒）。0 = 按下即出。对应 QuickLook 的 `HOLD_TO_PREVIEW_DURATION=750`。
    /// 见 `docs/route-a-plan.md` §2.2「产品层延迟」。
    pub hold_key_ms: u32,

    /// 是否启用全局空格热键（关掉后只能用 CLI / 托盘触发）。
    /// 关闭场景：与别的按键增强工具冲突、或管理员进程间 UIPI 收不到按键时。
    pub hotkey_enabled: bool,

    /// 预览的文件被外部程序改动时是否自动重新加载（默认 false）。
    ///
    /// 只在窗口可见时由 `TIMER_RELOAD`（800 ms 轮询文件 mtime）触发；改了文件再存一次，
    /// 预览会自己刷新，不用重新按空格。默认关：轮询虽轻，但多数人文件不会被改。
    pub auto_reload: bool,

    /// 快捷键绑定（1.0.7 起可配置，见 `qlr_core::keymap`）。
    ///
    /// 整段缺失时用默认值（`#[serde(default)]` 在这个字段上由 `Hotkeys::default()` 兜底），
    /// 所以老配置文件升级后不需要手改。
    pub hotkeys: Hotkeys,

    /// 图片初始缩放（百分比，1.0.9 起可配置）。
    ///
    /// 1.0.9 的默认是 50%（用户当时的原话"默认 50%"），理由是"大图按原尺寸开窗会顶到
    /// 工作区 86%，反而看不出全貌"。**1.8.3 改成 100%**：200% 缩放的机器上 50% 叠上
    /// 未做 DPI 换算，图片只占屏幕一小块（用户报障"内容也小很多"）；而且窗口尺寸有记忆
    /// （`window_sizes`），觉得大的人拖一次就记住了，比"每次都太小"好救。
    ///
    /// 老配置的迁移见 [`Config::migrate`]（schema 1 → 2）。
    pub image_zoom_percent: u32,

    /// 预览窗口**内容**的缩放（百分比；`0` = 自动）。1.8.3 新增。
    ///
    /// 为什么要把"内容"和"外壳"分开：外壳（顶栏 / 页码框 / 滚动条 / 设置窗）必须跟着
    /// DPI 走，否则 200% 下按钮小得点不着；但正文也跟着翻倍就变成"字很大、一屏没几行"。
    /// `0` = 自动（按 `sqrt(dpi/96)` 压缩，200% 下 ≈1.41 倍而不是 2 倍）；
    /// 填具体百分比就按它；填 `100` 等于"完全跟随 DPI"。
    pub content_scale_percent: u32,

    /// 上次预览窗口的位置（屏幕坐标，左上角；1.0.13 起记忆）。
    ///
    /// 用户要求："记录上次预览窗口打开的位置，新开预览窗口的时候直接在上次位置打开"。
    /// 位置是用户的习惯；尺寸见 `window_sizes`。
    ///
    /// 存的是屏幕坐标，所以换显示器/改分辨率后可能落在工作区外——使用时会夹回工作区
    /// （见 `window::clamp_into`），不需要在这里做校验（这里也不知道是哪个显示器）。
    pub window_pos: Option<(i32, i32)>,

    /// 每种格式记住的窗口尺寸（1.0.14 起）：键是 `format::Kind::name()`
    /// （`image` / `pdf` / `text` / `markdown`），值是窗口宽高（**含顶栏**）。
    ///
    /// 为什么按格式分开记：文本是 880 px 的阅读版式、图片按原图 50%、PDF 按页宽比例，
    /// 三者能差一个数量级。混成一份记忆就会出现"打开 PDF 得到一条 880 宽的窄缝"
    /// 或"打开小图周围一大片空白"——那看起来是 bug，不是记忆。
    ///
    /// 只在用户**自己改过**尺寸（拖边 / 缩放）时写；首次按内容算出来的尺寸不记，
    /// 否则一张 8000×6000 的大图会把之后所有图片的初始尺寸都顶到工作区上限。
    pub window_sizes: std::collections::BTreeMap<String, (i32, i32)>,

    /// 投机预渲染（1.1.0 起，默认自适应，可在设置里关）。
    ///
    /// 用户原话："二期默认自适应吧，我试试效果，后续我觉得没用的话会放弃掉这项改进"。
    /// 所以这里必须**能一键关掉**，而且默认档位是"只在确实有预览习惯时才烧 CPU"。
    ///
    /// 反序列化用 [`de_prewarm`]：老配置没有这个字段 → 自适应；手改成了乱七八糟的字符串
    /// → 也退回自适应，**不让一个档位写错就把整份配置打回出厂**。
    #[serde(default, deserialize_with = "de_prewarm")]
    pub prewarm: Prewarm,

    /// 每个格式类别用「内置渲染」还是「保真」(系统 IPreviewHandler)。
    ///
    /// 键是 `format::Kind::name()`（`document`/`presentation`/`sheet`/`pdf`）；缺省（空表）
    /// = 全部内置，与历史行为一致。缺省内置是刻意的：保真会退回"上百 MB / 数百 ms"的
    /// 资源画像，跟 32 ms / 30 MB 的卖点相反，只应作为用户按格式显式 opt-in 的出口。
    /// 反序列化用 [`de_backend_prefs`] 做宽容解析。
    #[serde(default, deserialize_with = "de_backend_prefs")]
    pub backend_prefs: std::collections::BTreeMap<String, BackendPref>,

    /// 未知格式（`Kind::Unsupported`，没有内置查看器）是否也尝试系统预览处理器。
    ///
    /// 默认 `false` = 现状（不支持就响铃、不画）；`true` 时只要系统真注册了处理器就交给它，
    /// 对应"长尾格式先套用 QuickLook 原版"的需求。没有处理器时仍按不支持处理。
    pub shell_for_unsupported: bool,

    /// 启动时自动检查更新（默认开）。
    ///
    /// 检查只做"拉一份几 KB 的清单 + 版本比较"，**不自动下载、不替换自身**；发现新版本时
    /// 只弹托盘提示并让用户点开下载页。理由见 [`qlr_core::update`] 与 `docs/dev/RELEASE.md`
    /// （无代码签名 + in-process 内容，静默自替换坑多）。清单地址见 `update_manifest_url`。
    pub update_check: bool,

    /// 更新清单地址（自托管 `latest.json` 或 GitHub `releases/latest` API 均可）。
    ///
    /// 默认指向发布仓的 GitHub `releases/latest`（见 [`crate::update::DEFAULT_UPDATE_URL`]），
    /// 开箱即可检查更新。维护者可改成自托管源；也可用环境变量 `QLRUST_UPDATE_URL` 覆盖，
    /// 便于本地/内测指向测试源。`parse` 能兼容两种 JSON 形状，见 [`crate::update`]。
    pub update_manifest_url: String,

    /// 上次检查更新完成的时间（Unix 秒；`None` = 从没查过）。用来做「一天最多查一次」节流。
    pub last_update_check: Option<i64>,

    /// 用户选择"跳过此版本"的版本号；等于清单版本时不再提示（直到更新的版本出现）。
    pub skipped_version: Option<String>,

    /// 语言：`auto` 或具体 locale（如 `zh-CN`）。
    pub language: String,

    /// 日志级别：`error|warn|info|debug|trace`；`QLRUST_LOG` 环境变量优先。
    pub log_level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA,
            topmost: true,
            width_ratio: 0.62,
            height_ratio: 0.72,
            hold_key_ms: 0,
            hotkey_enabled: true,
            auto_reload: false,
            hotkeys: Hotkeys::default(),
            image_zoom_percent: 100,
            content_scale_percent: 0,
            window_pos: None,
            window_sizes: std::collections::BTreeMap::new(),
            prewarm: Prewarm::default(),
            backend_prefs: std::collections::BTreeMap::new(),
            shell_for_unsupported: false,
            update_check: true,
            update_manifest_url: crate::update::DEFAULT_UPDATE_URL.to_string(),
            last_update_check: None,
            skipped_version: None,
            language: "auto".to_string(),
            log_level: "info".to_string(),
        }
    }
}

impl Config {
    /// 读取配置。文件不存在 → 默认值；解析失败 → 默认值 + 返回错误（调用方记日志但继续跑）。
    pub fn load(path: &Path) -> (Self, Option<crate::error::Error>) {
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<Config>(&bytes) {
                Ok(cfg) => (cfg.migrate().sanitized(), None),
                Err(e) => (
                    Config::default(),
                    Some(crate::error::Error::config(format!(
                        "配置解析失败（已退回默认值）：{e}"
                    ))),
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Config::default(), None),
            Err(e) => (
                Config::default(),
                Some(crate::error::Error::config(format!(
                    "配置读取失败（已退回默认值）：{e}"
                ))),
            ),
        }
    }

    /// 老配置迁移（1.8.3 起**真有了这个函数**：此前 `migrate` 只写在文档里，并不存在）。
    ///
    /// 原则：只做确定安全的改动，并且**认得出用户自己改过的值就别碰**。
    ///
    /// schema 1 → 2：`image_zoom_percent` 50 → 100。
    /// 老配置里那个 50 是 1.0.9 落盘的默认值；不迁移的话，200% 缩放的机器上升级完
    /// "图片还是小一半"，等于白改。所以**恰好等于 50 才改**：用户自己设成 60/80/120
    /// 的一律不动（他有明确偏好，而 50 只是历史默认）。
    fn migrate(mut self) -> Self {
        if self.schema_version < 2 {
            if self.image_zoom_percent == 50 {
                self.image_zoom_percent = 100;
            }
            self.schema_version = 2;
        }
        self
    }

    /// 原子保存：先写 `.tmp` 再 rename，避免掉电/崩溃留下半个文件。
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// 把越界值拉回合理范围（配置文件是用户可编辑的，不能信任）。
    fn sanitized(mut self) -> Self {
        self.width_ratio = self.width_ratio.clamp(0.1, 1.0);
        self.height_ratio = self.height_ratio.clamp(0.1, 1.0);
        if self.schema_version == 0 {
            self.schema_version = CURRENT_SCHEMA;
        }
        self.hotkeys = self.hotkeys.sanitized();
        // 1% 太小、400% 太大（4 倍已经要跨屏拖了），夹在中间。
        self.image_zoom_percent = self.image_zoom_percent.clamp(10, 400);
        // 内容缩放：0 = 自动；手填的离谱值夹到 50–400（太小看不清，太大一屏放不下一行）。
        self.content_scale_percent = if self.content_scale_percent == 0 {
            0
        } else {
            self.content_scale_percent.clamp(50, 400)
        };
        // 记住的窗口尺寸是用户可手改的 JSON：丢掉明显不合理的（负数、0、比任何显示器都大）。
        self.window_sizes
            .retain(|_, (w, h)| (200..=20000).contains(w) && (120..=20000).contains(h));
        self
    }

    /// 取某种格式记住的窗口尺寸（1.0.14）。
    pub fn window_size_for(&self, kind: &str) -> Option<(i32, i32)> {
        self.window_sizes.get(kind).copied()
    }

    /// 记下某种格式的窗口尺寸（1.0.14）。
    pub fn set_window_size(&mut self, kind: &str, size: (i32, i32)) {
        self.window_sizes.insert(kind.to_string(), size);
    }

    /// 取某格式类别希望的后端（缺省 = 内置）。
    pub fn backend_pref(&self, kind: &str) -> BackendPref {
        self.backend_prefs.get(kind).copied().unwrap_or_default()
    }

    /// 设置某格式类别的后端偏好。
    pub fn set_backend_pref(&mut self, kind: &str, pref: BackendPref) {
        self.backend_prefs.insert(kind.to_string(), pref);
    }

    /// 后端的日志摘要：`native` 表示全内置，否则列出被设为保真的格式。
    fn backend_summary(&self) -> String {
        if self.backend_prefs.is_empty() && !self.shell_for_unsupported {
            return "native".to_string();
        }
        let mut parts: Vec<String> = self
            .backend_prefs
            .iter()
            .filter(|(_, v)| **v == BackendPref::Shell)
            .map(|(k, _)| format!("{k}=shell"))
            .collect();
        if self.shell_for_unsupported {
            parts.push("unsupported=shell".to_string());
        }
        if parts.is_empty() {
            "native".to_string()
        } else {
            parts.join(",")
        }
    }

    /// 给 UI/日志看的摘要。
    pub fn summary(&self) -> String {
        format!(
            "topmost={} size={:.2}x{:.2} hold={}ms hotkey={} [{}] img={}% pos={:?} sizes={} prewarm={} backend={} lang={} log={}",
            self.topmost,
            self.width_ratio,
            self.height_ratio,
            self.hold_key_ms,
            self.hotkey_enabled,
            self.hotkeys.summary(),
            self.image_zoom_percent,
            self.window_pos,
            self.window_sizes.len(),
            self.prewarm.name(),
            self.backend_summary(),
            self.language,
            self.log_level
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid() {
        let c = Config::default();
        assert!((0.1..=1.0).contains(&c.width_ratio));
        assert_eq!(c.schema_version, CURRENT_SCHEMA);
    }

    #[test]
    fn roundtrip_via_json() {
        let c = Config {
            topmost: false,
            width_ratio: 0.5,
            ..Config::default()
        };
        let s = serde_json::to_string(&c).unwrap();
        let back: Config = serde_json::from_str(&s).unwrap();
        assert!(!back.topmost);
        assert!((back.width_ratio - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let s = r#"{"topmost":false,"future_option":123}"#;
        let c: Config = serde_json::from_str(s).unwrap();
        assert!(!c.topmost);
    }

    #[test]
    fn missing_hotkey_flag_defaults_to_enabled() {
        // 旧配置文件里没有 hotkey_enabled，应该自动补 true（向后兼容）。
        let s = r#"{"topmost":false}"#;
        let c: Config = serde_json::from_str(s).unwrap();
        assert!(c.hotkey_enabled);
        assert!(c.summary().contains("hotkey=true"));
    }

    #[test]
    fn out_of_range_ratios_are_clamped() {
        let s = r#"{"width_ratio":5.0,"height_ratio":0.0}"#;
        let c: Config = serde_json::from_str::<Config>(s).unwrap().sanitized();
        assert!((c.width_ratio - 1.0).abs() < f32::EPSILON);
        assert!((c.height_ratio - 0.1).abs() < f32::EPSILON);
    }

    #[test]
    fn missing_file_yields_default_without_error() {
        let (c, err) = Config::load(Path::new("Z:\\definitely\\not\\here\\config.json"));
        assert!(err.is_none());
        assert_eq!(c.schema_version, CURRENT_SCHEMA);
    }

    #[test]
    fn old_config_without_hotkeys_gets_defaults() {
        // 1.0.6 及更早的配置文件里没有 hotkeys 段，升级后必须自动补上默认绑定。
        let s = r#"{"topmost":false}"#;
        let c: Config = serde_json::from_str(s).unwrap();
        assert_eq!(c.hotkeys.toggle, "Space");
        assert_eq!(c.hotkeys.close, "Esc");
        assert_eq!(c.hotkeys.lens_hold, "Shift");
    }

    #[test]
    fn broken_hotkeys_are_repaired_on_load() {
        let s = r#"{"hotkeys":{"toggle":"不是键名","page_next":"F5"}}"#;
        let c: Config = serde_json::from_str::<Config>(s).unwrap().sanitized();
        // 错的修回默认值，对的保留用户的选择。
        assert_eq!(c.hotkeys.toggle, "Space");
        assert_eq!(c.hotkeys.page_next, "F5");
    }

    #[test]
    fn summary_includes_hotkeys() {
        let c = Config::default();
        assert!(c.summary().contains("toggle=Space"));
    }

    /// 预渲染档位（1.1.0）：缺失 → 自适应；写坏的字符串不该炸掉整份配置。
    #[test]
    fn prewarm_defaults_to_adaptive_and_tolerates_garbage() {
        assert_eq!(Prewarm::default(), Prewarm::Adaptive);
        assert_eq!(Config::default().prewarm, Prewarm::Adaptive);

        // 老配置：连字段都没有。
        let c: Config = serde_json::from_str(r#"{"topmost":false}"#).unwrap();
        assert_eq!(c.prewarm, Prewarm::Adaptive);

        // 手改成了错的值：整份配置仍然读得出来，只有这一档退回自适应。
        let c: Config = serde_json::from_str(r#"{"prewarm":"瞎写","log_level":"debug"}"#).unwrap();
        assert_eq!(c.prewarm, Prewarm::Adaptive);
        assert_eq!(c.log_level, "debug", "别的字段不能被牵连");
    }

    /// 每格式后端偏好：缺失 → 全内置；写坏的档位/未知 kind 被跳过而不炸整份配置。
    #[test]
    fn backend_prefs_default_native_and_tolerate_garbage() {
        assert_eq!(BackendPref::default(), BackendPref::Native);
        assert_eq!(
            Config::default().backend_pref("document"),
            BackendPref::Native
        );
        assert_eq!(Config::default().backend_summary(), "native");

        let c: Config = serde_json::from_str(r#"{"topmost":false}"#).unwrap();
        assert!(c.backend_prefs.is_empty());
        assert!(!c.shell_for_unsupported);

        // 正常读到保真。
        let c: Config = serde_json::from_str(r#"{"backend_prefs":{"document":"shell"}}"#).unwrap();
        assert_eq!(c.backend_pref("document"), BackendPref::Shell);
        assert_eq!(c.backend_pref("presentation"), BackendPref::Native);
        assert_eq!(c.backend_summary(), "document=shell");

        // 写坏的档位字符串被跳过，其它字段不受牵连。
        let c: Config = serde_json::from_str(
            r#"{"backend_prefs":{"pdf":"瞎写","sheet":"shell"},"log_level":"debug"}"#,
        )
        .unwrap();
        assert_eq!(c.backend_pref("pdf"), BackendPref::Native);
        assert_eq!(c.backend_pref("sheet"), BackendPref::Shell);
        assert_eq!(c.log_level, "debug");
    }

    #[test]
    fn backend_pref_cycles_between_two_modes() {
        assert_eq!(BackendPref::Native.cycle(), BackendPref::Shell);
        assert_eq!(BackendPref::Shell.cycle(), BackendPref::Native);
    }

    /// 更新检查相关字段：默认开启、地址为空、从未检查、未跳过；老配置缺这些字段也要能读。
    #[test]
    fn auto_reload_defaults_false_and_survives_old_configs() {
        let c: Config = serde_json::from_str(r#"{"topmost":false}"#).unwrap();
        assert!(!c.auto_reload, "旧配置没有 auto_reload 时应默认 false");
        let c2: Config = serde_json::from_str(r#"{"auto_reload":true}"#).unwrap();
        assert!(c2.auto_reload);
    }

    #[test]
    fn update_fields_default_and_survive_old_configs() {
        let c = Config::default();
        assert!(c.update_check);
        assert_eq!(c.update_manifest_url, crate::update::DEFAULT_UPDATE_URL);
        assert_eq!(c.last_update_check, None);
        assert_eq!(c.skipped_version, None);

        // 老配置里没有这些字段 → 用默认值，且不影响其它字段。
        let old: Config = serde_json::from_str(r#"{"topmost":false}"#).unwrap();
        assert!(old.update_check);
        assert_eq!(old.update_manifest_url, crate::update::DEFAULT_UPDATE_URL);

        // 新字段能正常往返。
        let s = r#"{"update_check":false,"update_manifest_url":"https://x/latest.json","last_update_check":1700000000,"skipped_version":"1.16.0"}"#;
        let c: Config = serde_json::from_str(s).unwrap();
        assert!(!c.update_check);
        assert_eq!(c.update_manifest_url, "https://x/latest.json");
        assert_eq!(c.last_update_check, Some(1_700_000_000));
        assert_eq!(c.skipped_version.as_deref(), Some("1.16.0"));
    }

    /// 设置页点一下换一档，转一圈回到原点。
    #[test]
    fn prewarm_cycles_through_all_three_modes() {
        let mut m = Prewarm::Off;
        let mut seen = Vec::new();
        for _ in 0..3 {
            seen.push(m);
            m = m.cycle();
        }
        assert_eq!(seen, vec![Prewarm::Off, Prewarm::Adaptive, Prewarm::Always]);
        assert_eq!(m, Prewarm::Off, "转三下要回到原点");
        assert!(!Prewarm::Off.is_on());
        assert!(Prewarm::Adaptive.is_on() && Prewarm::Always.is_on());
    }

    /// 窗口位置记忆（1.0.13）：老配置文件里没有这个字段也要能读，
    /// 写出去再读回来必须一模一样（否则每次启动位置都丢）。
    #[test]
    fn window_position_survives_a_save_and_load() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/qlrust-cfg-test");
        std::fs::create_dir_all(&dir).expect("建测试目录");
        let path = dir.join("pos.json");

        // 老配置：完全没有 window_pos 字段。
        std::fs::write(&path, br#"{"topmost":false}"#).expect("写老配置");
        let (old, err) = Config::load(&path);
        assert!(err.is_none(), "老配置不该报错");
        assert_eq!(old.window_pos, None, "没记过位置就是 None");

        // 记一个位置 → 存 → 读回来还是它。
        let mut c = old;
        c.window_pos = Some((1234, 567));
        c.save(&path).expect("保存配置");
        let (back, err) = Config::load(&path);
        assert!(err.is_none());
        assert_eq!(back.window_pos, Some((1234, 567)));
        assert!(back.summary().contains("1234"));
    }

    /// 窗口尺寸记忆（1.0.14）：按格式各记一份，而且手改坏的尺寸要被丢掉。
    #[test]
    fn window_sizes_are_per_kind_and_sanitized() {
        let mut c = Config::default();
        assert_eq!(c.window_size_for("image"), None, "没记过就是空的");
        c.set_window_size("image", (1200, 900));
        c.set_window_size("pdf", (700, 1000));
        assert_eq!(c.window_size_for("image"), Some((1200, 900)));
        assert_eq!(c.window_size_for("pdf"), Some((700, 1000)));
        assert_eq!(c.window_size_for("markdown"), None, "各格式互不影响");

        // 手改 JSON 写出负数/离谱值 → 读回来被丢掉，不会把窗口开成负尺寸。
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/qlrust-cfg-test");
        std::fs::create_dir_all(&dir).expect("建测试目录");
        let path = dir.join("sizes.json");
        std::fs::write(
            &path,
            br#"{"window_sizes":{"image":[0,0],"pdf":[700,1000],"text":[999999,50]}}"#,
        )
        .expect("写配置");
        let (loaded, err) = Config::load(&path);
        assert!(err.is_none());
        assert_eq!(loaded.window_size_for("image"), None, "0 尺寸要丢掉");
        assert_eq!(loaded.window_size_for("pdf"), Some((700, 1000)));
        assert_eq!(loaded.window_size_for("text"), None, "离谱尺寸要丢掉");
    }

    /// 1.8.3：图片默认缩放 50% → 100%，老配置要跟着迁。
    #[test]
    fn image_zoom_default_is_one_hundred() {
        assert_eq!(Config::default().image_zoom_percent, 100);
    }

    /// schema 1 的老配置（值是 1.0.9 落盘的 50）→ 迁移成 100，并把版本抬到 2。
    #[test]
    fn schema1_migrates_legacy_fifty_percent_to_one_hundred() {
        let s = r#"{"schema_version":1,"image_zoom_percent":50}"#;
        let c: Config = serde_json::from_str::<Config>(s).unwrap().migrate();
        assert_eq!(c.image_zoom_percent, 100);
        assert_eq!(c.schema_version, 2);
    }

    /// 用户自己设过的值（不是老默认的 50）不许被迁移改掉。
    #[test]
    fn migration_leaves_custom_zoom_alone() {
        for kept in [60u32, 80, 120, 400] {
            let s = format!(r#"{{"schema_version":1,"image_zoom_percent":{kept}}}"#);
            let c: Config = serde_json::from_str::<Config>(&s).unwrap().migrate();
            assert_eq!(c.image_zoom_percent, kept, "自定义 {kept}% 不该被改");
        }
    }

    /// 迁移必须幂等：跑两遍结果一样（load() 每次启动都会跑一遍）。
    #[test]
    fn migration_is_idempotent() {
        let s = r#"{"schema_version":1,"image_zoom_percent":50}"#;
        let once: Config = serde_json::from_str::<Config>(s).unwrap().migrate();
        let twice = once.clone().migrate();
        assert_eq!(once.image_zoom_percent, twice.image_zoom_percent);
        assert_eq!(twice.schema_version, 2);
    }
}
