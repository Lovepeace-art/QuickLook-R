//! 预览后端契约（§7 第一刀，无 sidecar）。
//!
//! 「这个文件由谁来画」以前散在 `show.rs` / `render.rs` / `shell.rs` 三处，各自重新枚举
//! `Kind`（`viewer_for`、`find_for_path`、`is_archive`、`has_handler`）。每加一个后端
//! （office 轻型、html 改进、插件、未来的 sidecar）都要在四五个文件里各补一个分支 ——
//! 这正是"加格式 = 改平台"的来源。
//!
//! 这一刀只收口"**选择 / 表面 / 自然尺寸**"三件事，不动渲染实现、不动窗口尺寸算式、
//! 不动音视频启停；因此与改造前**行为等价**。优先级与旧规则一致：
//! 原生自研(`Viewer`) > 压缩包列表 > 系统预览处理器(`IPreviewHandler`) > 插件。

use super::*;

/// 内容在屏幕上的承载方式（`repaint` 的通道由此驱动；本刀先只做分类，不改绘制）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Surface {
    /// 贴位图（`StretchDIBits`）。
    Bitmap,
    /// 文本/文档排版（`DrawTextW` 那一套）。
    Text,
    /// 系统预览处理器的子窗口自己画（我们只画顶栏）。
    ChildWindow,
    /// 电子表格网格（内核算出可见行列，逐格 GDI 画，可滚动/可翻 sheet）。
    Sheet,
    /// 分页文档（docx）：模型带每页的块/行坐标，逐页 GDI 画，翻页只改页码。
    Document,
    /// 幻灯片（pptx）：模型带每张幻灯片的形状/文字坐标，逐张 GDI 画，翻页只改页码。
    Presentation,
    /// 壳层自绘列表（压缩包目录树）。
    List,
}

impl Surface {
    /// 用"真正到手的 `Canvas`"校准表面（两者 `Bitmap`/`Text` 一一对应）。
    ///
    /// `show_preview` 是按 `Kind` **预算**表面（`backend.surface(kind)`），但插件返回的内容
    /// 可能与预算不同（例如插件给的是文本而非位图）。内容抵达时以实际 `Canvas` 为准，
    /// 保证 `State.surface` 与 `get_content()` 讲的是同一件事，`repaint` 才不会走错通道。
    pub(crate) fn from_canvas(canvas: &qlr_render::Canvas) -> Surface {
        match canvas {
            qlr_render::Canvas::Bitmap(_) => Surface::Bitmap,
            qlr_render::Canvas::Text(_) => Surface::Text,
            qlr_render::Canvas::Sheet(_) => Surface::Sheet,
            qlr_render::Canvas::Document(_) => Surface::Document,
            qlr_render::Canvas::Presentation(_) => Surface::Presentation,
        }
    }
}

/// 后端跑在哪个进程里——**契约元数据，不是行为**。
///
/// 把它显式化，是为了让"这个后端将来要不要隔离"在选后端时就能被读到、写进日志，
/// 而不是散落在注释里。当前只有系统预览处理器（Office）天然在独立进程；其余都在本进程。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Isolation {
    /// 与本进程同生共死（原生自研、压缩包列表、进程内 C ABI 插件）。
    InProc,
    /// 由系统在独立代理进程里承载（`IPreviewHandler` → `prevhost.exe`）。
    SystemProcess,
    /// 需要自建 sidecar 才安全（不可信的二进制解码器 / 易挂起）。**当前无后端使用**，
    /// 作为契约预留：真正要隔离 `pdfium` 一类时，在这里声明而不是散改调用点。
    #[allow(dead_code)]
    NeedsSidecar,
}

impl Isolation {
    /// 稳定标签（进日志、可被 grep）。
    pub(crate) fn label(self) -> &'static str {
        match self {
            Isolation::InProc => "inproc",
            Isolation::SystemProcess => "system",
            Isolation::NeedsSidecar => "sidecar",
        }
    }
}

/// 谁能接管一个文件的预览。
///
/// 实现者应是**无状态零大小**的：`resolve` 每次按需返回静态引用，不分配。
pub(crate) trait PreviewBackend: Sync {
    /// 稳定标识（进日志、被断言用，不本地化）。
    fn id(&self) -> &'static str;
    /// 优先级，越小越先（数组顺序也按它排，`resolve` 取首个命中）。
    fn priority(&self) -> u8;
    /// 这个后端是否接管 `path`（`kind` 是 `kind_for_path` 的结果，避免重复探测）。
    fn can_handle(&self, path: &Path, kind: format::Kind) -> bool;
    /// 内容表面（`kind` 只对原生后端有意义：文本类给 `Text`，位图类给 `Bitmap`）。
    fn surface(&self, kind: format::Kind) -> Surface;
    /// 按原始尺寸开窗用的像素尺寸（无自然尺寸的后端返回 `None`）。
    fn natural_size(&self, path: &Path, kind: format::Kind) -> Option<(u32, u32)>;
    /// 跑在哪个进程里（见 [`Isolation`]）。
    fn isolation(&self) -> Isolation;
    /// 是否需要把输入事件转发给它（滚动/翻页/Office 点选=是；v1 插件静态位图=否）。
    fn interactive(&self) -> bool;
}

/// 原生自研渲染（`qlr_render::Viewer`）。最高优先级。
struct NativeBackend;

impl PreviewBackend for NativeBackend {
    fn id(&self) -> &'static str {
        "native"
    }
    fn priority(&self) -> u8 {
        0
    }
    fn can_handle(&self, _path: &Path, kind: format::Kind) -> bool {
        qlr_render::viewer_for(kind).is_some()
    }
    fn surface(&self, kind: format::Kind) -> Surface {
        match kind {
            format::Kind::Text
            | format::Kind::Markdown
            | format::Kind::Html
            | format::Kind::Audio => Surface::Text,
            format::Kind::Sheet => Surface::Sheet,
            format::Kind::Document => Surface::Document,
            format::Kind::Presentation => Surface::Presentation,
            _ => Surface::Bitmap,
        }
    }
    fn natural_size(&self, path: &Path, kind: format::Kind) -> Option<(u32, u32)> {
        qlr_render::natural_size(path, kind)
    }
    fn isolation(&self) -> Isolation {
        Isolation::InProc
    }
    fn interactive(&self) -> bool {
        true
    }
}

/// 压缩包目录树（壳层自绘列表，1.7.3）。
struct ArchiveBackend;

impl PreviewBackend for ArchiveBackend {
    fn id(&self) -> &'static str {
        "archive"
    }
    fn priority(&self) -> u8 {
        1
    }
    fn can_handle(&self, path: &Path, _kind: format::Kind) -> bool {
        is_archive(path)
    }
    fn surface(&self, _kind: format::Kind) -> Surface {
        Surface::List
    }
    fn natural_size(&self, _path: &Path, _kind: format::Kind) -> Option<(u32, u32)> {
        None
    }
    fn isolation(&self) -> Isolation {
        Isolation::InProc
    }
    fn interactive(&self) -> bool {
        true
    }
}

/// 系统预览处理器（Office 那一类）。规则与旧 `shell_candidate` 完全一致：
/// 内置不接、且有插件接管（说明这格式我们确实不自己画）、且系统真有处理器。
struct ShellBackend;

impl PreviewBackend for ShellBackend {
    fn id(&self) -> &'static str {
        "shell"
    }
    fn priority(&self) -> u8 {
        2
    }
    fn can_handle(&self, path: &Path, kind: format::Kind) -> bool {
        if qlr_render::viewer_for(kind).is_some() {
            // 图片/PDF/文本/Markdown 由我们自己画，交互也比处理器好。
            return false;
        }
        if plugins::host().find_for_path(path).is_none() {
            // 没插件 = 本来就不支持（预览窗根本不会开），不用试。
            return false;
        }
        shellpreview::has_handler(path)
    }
    fn surface(&self, _kind: format::Kind) -> Surface {
        Surface::ChildWindow
    }
    fn natural_size(&self, _path: &Path, _kind: format::Kind) -> Option<(u32, u32)> {
        None
    }
    fn isolation(&self) -> Isolation {
        Isolation::SystemProcess
    }
    fn interactive(&self) -> bool {
        true
    }
}

/// 插件兜底（`qlr-plugin-api` 的进程内 C ABI）。默认出位图（插件也能给文本大纲，
/// 那是渲染期的 `Canvas::Text`；此处只登记默认表面）。
struct PluginBackend;

impl PreviewBackend for PluginBackend {
    fn id(&self) -> &'static str {
        "plugin"
    }
    fn priority(&self) -> u8 {
        3
    }
    fn can_handle(&self, path: &Path, kind: format::Kind) -> bool {
        // 内置优先：内置接的格式，插件不抢。
        if qlr_render::viewer_for(kind).is_some() {
            return false;
        }
        plugins::host().find_for_path(path).is_some()
    }
    fn surface(&self, _kind: format::Kind) -> Surface {
        Surface::Bitmap
    }
    fn natural_size(&self, _path: &Path, _kind: format::Kind) -> Option<(u32, u32)> {
        None
    }
    fn isolation(&self) -> Isolation {
        Isolation::InProc
    }
    fn interactive(&self) -> bool {
        false
    }
}

static NATIVE: NativeBackend = NativeBackend;
static ARCHIVE: ArchiveBackend = ArchiveBackend;
static SHELL: ShellBackend = ShellBackend;
static PLUGIN: PluginBackend = PluginBackend;

/// 按优先级排好的后端表（`resolve` 取首个 `can_handle`）。
static BACKENDS: [&'static dyn PreviewBackend; 4] = [&NATIVE, &ARCHIVE, &SHELL, &PLUGIN];

/// 进程级的后端偏好缓存（与 `Config` 同步，由宿主在每次读到配置后灌一次）。
///
/// 为什么不用 `with_state(|s| s.config...)`：`resolve` 被渲染线程、预热线程到处调用，
/// 若在里面再拿会话锁，既可能和已持有的 `State` 锁冲突（std `Mutex` 不可重入），
/// 又要处理"后台线程没有会话"。偏好是进程级、只读、极小，单独放一把短锁最干净。
#[derive(Default, Clone)]
struct RuntimePrefs {
    /// 被设为「保真」的格式 kind 名（`format::Kind::name()`）。
    shell_kinds: std::collections::HashSet<String>,
    /// 未知格式也尝试系统处理器。
    shell_for_unsupported: bool,
}

static PREFS: std::sync::OnceLock<std::sync::Mutex<RuntimePrefs>> = std::sync::OnceLock::new();

fn prefs() -> &'static std::sync::Mutex<RuntimePrefs> {
    PREFS.get_or_init(|| std::sync::Mutex::new(RuntimePrefs::default()))
}

/// 宿主在每次读到配置后调用：把「保真」偏好灌进进程级缓存。
///
/// 调用点：`boot.rs` 启动读配置、`on_settings_saved` 热更新读配置各一次。
pub(crate) fn apply_config(config: &qlr_core::config::Config) {
    let want = RuntimePrefs::from_config(config);
    *prefs().lock().unwrap_or_else(|e| e.into_inner()) = want;
}

impl RuntimePrefs {
    /// 从配置算出进程级偏好快照（纯函数，便于单测，不碰全局）。
    fn from_config(config: &qlr_core::config::Config) -> Self {
        let mut shell_kinds = std::collections::HashSet::new();
        for (kind, pref) in &config.backend_prefs {
            if *pref == qlr_core::config::BackendPref::Shell {
                shell_kinds.insert(kind.clone());
            }
        }
        Self {
            shell_kinds,
            shell_for_unsupported: config.shell_for_unsupported,
        }
    }
}

/// 该格式是否被用户设为「保真」。
fn prefers_shell(kind: format::Kind) -> bool {
    prefs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .shell_kinds
        .contains(kind.name())
}

/// 未知格式是否也交给系统处理器。
fn shell_for_unsupported() -> bool {
    prefs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .shell_for_unsupported
}

/// 选出接管这个文件的后端；`None` = 不支持（壳层响铃、不画）。
pub(crate) fn resolve(path: &Path, kind: format::Kind) -> Option<&'static dyn PreviewBackend> {
    // 压缩包是**按扩展名**判定的容器：即使内容被嗅探成文本（例如一个假的 `.zip`
    // 里只有纯文本），也仍应走目录列表/解析失败提示，而不是退化成文本预览。
    // 这条必须排在按 `Kind` 判定的后端前面，等价于旧逻辑里 `archive` 先于渲染分支。
    if is_archive(path) {
        return Some(&ARCHIVE);
    }
    // 「保真」偏好：用户按格式显式要求走系统处理器时，让 Shell 抢在 Native 前。
    // 只有系统确实注册了处理器才让路，否则回落到正常优先级（内置兜底，不留空白窗）。
    let want_shell = (prefers_shell(kind)
        || (kind == format::Kind::Unsupported && shell_for_unsupported()))
        && shellpreview::has_handler(path);
    if want_shell {
        return Some(&SHELL);
    }
    // 表顺序必须与 `priority` 一致；改表忘了改优先级会在 debug/test 里立刻炸出来。
    debug_assert!(
        BACKENDS
            .windows(2)
            .all(|w| w[0].priority() < w[1].priority())
    );
    BACKENDS.iter().copied().find(|b| b.can_handle(path, kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_kinds_resolve_to_native() {
        for kind in [
            format::Kind::Image,
            format::Kind::Pdf,
            format::Kind::Text,
            format::Kind::Markdown,
            format::Kind::Svg,
            format::Kind::Html,
            format::Kind::Audio,
            format::Kind::Video,
            format::Kind::Sheet,
        ] {
            let p = PathBuf::from(r"C:\x\a");
            let b = resolve(&p, kind).expect("内置类必须有后端");
            assert_eq!(b.id(), "native", "{kind} 应交给原生后端");
        }
    }

    #[test]
    fn native_surface_splits_text_and_bitmap() {
        assert_eq!(NATIVE.surface(format::Kind::Text), Surface::Text);
        assert_eq!(NATIVE.surface(format::Kind::Markdown), Surface::Text);
        assert_eq!(NATIVE.surface(format::Kind::Html), Surface::Text);
        assert_eq!(NATIVE.surface(format::Kind::Audio), Surface::Text);
        assert_eq!(NATIVE.surface(format::Kind::Image), Surface::Bitmap);
        assert_eq!(NATIVE.surface(format::Kind::Pdf), Surface::Bitmap);
        assert_eq!(NATIVE.surface(format::Kind::Video), Surface::Bitmap);
        // 表格是第三种表面：数据到壳层，逐格 GDI 画（可滚动/翻 sheet）。
        assert_eq!(NATIVE.surface(format::Kind::Sheet), Surface::Sheet);
    }

    #[test]
    fn archive_resolves_list_and_has_no_natural_size() {
        let p = PathBuf::from(r"C:\x\a.zip");
        let b = resolve(&p, format::kind_for_path(&p)).expect("zip 应由压缩包后端接管");
        assert_eq!(b.id(), "archive");
        assert_eq!(b.surface(format::Kind::Unsupported), Surface::List);
        assert_eq!(b.natural_size(&p, format::Kind::Unsupported), None);
    }

    #[test]
    fn unknown_extension_without_plugin_is_unsupported() {
        // `.zzz` 既不是内置、也不是压缩包、也没插件注册 → 应当没有后端。
        let p = PathBuf::from(r"C:\x\a.zzz");
        assert!(resolve(&p, format::kind_for_path(&p)).is_none());
    }

    #[test]
    fn runtime_prefs_from_config_collects_shell_kinds() {
        // 纯函数：默认配置 = 全内置、未知格式不试处理器。
        let empty = RuntimePrefs::from_config(&qlr_core::config::Config::default());
        assert!(empty.shell_kinds.is_empty());
        assert!(!empty.shell_for_unsupported);

        let mut cfg = qlr_core::config::Config::default();
        cfg.set_backend_pref("document", qlr_core::config::BackendPref::Shell);
        cfg.shell_for_unsupported = true;
        let p = RuntimePrefs::from_config(&cfg);
        assert!(p.shell_kinds.contains("document"));
        assert!(!p.shell_kinds.contains("sheet"));
        assert!(p.shell_for_unsupported);
    }

    #[test]
    fn priorities_are_strictly_increasing() {
        let ps: Vec<u8> = BACKENDS.iter().map(|b| b.priority()).collect();
        assert!(ps.windows(2).all(|w| w[0] < w[1]), "{ps:?}");
    }

    #[test]
    fn isolation_and_interactive_contract() {
        assert_eq!(NATIVE.isolation(), Isolation::InProc);
        assert!(NATIVE.interactive());
        assert_eq!(ARCHIVE.isolation(), Isolation::InProc);
        assert!(ARCHIVE.interactive());
        assert_eq!(SHELL.isolation(), Isolation::SystemProcess);
        assert!(SHELL.interactive(), "Office 类需要把点选/翻页转发给它");
        assert_eq!(PLUGIN.isolation(), Isolation::InProc);
        assert!(!PLUGIN.interactive(), "v1 插件只出静态位图，不接输入");
        // 标签进日志，必须稳定可 grep。
        assert_eq!(Isolation::InProc.label(), "inproc");
        assert_eq!(Isolation::SystemProcess.label(), "system");
        assert_eq!(Isolation::NeedsSidecar.label(), "sidecar");
    }
}
