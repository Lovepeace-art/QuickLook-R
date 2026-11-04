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
            _ => Surface::Bitmap,
        }
    }
    fn natural_size(&self, path: &Path, kind: format::Kind) -> Option<(u32, u32)> {
        qlr_render::natural_size(path, kind)
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
}

static NATIVE: NativeBackend = NativeBackend;
static ARCHIVE: ArchiveBackend = ArchiveBackend;
static SHELL: ShellBackend = ShellBackend;
static PLUGIN: PluginBackend = PluginBackend;

/// 按优先级排好的后端表（`resolve` 取首个 `can_handle`）。
static BACKENDS: [&'static dyn PreviewBackend; 4] = [&NATIVE, &ARCHIVE, &SHELL, &PLUGIN];

/// 选出接管这个文件的后端；`None` = 不支持（壳层响铃、不画）。
pub(crate) fn resolve(path: &Path, kind: format::Kind) -> Option<&'static dyn PreviewBackend> {
    // 压缩包是**按扩展名**判定的容器：即使内容被嗅探成文本（例如一个假的 `.zip`
    // 里只有纯文本），也仍应走目录列表/解析失败提示，而不是退化成文本预览。
    // 这条必须排在按 `Kind` 判定的后端前面，等价于旧逻辑里 `archive` 先于渲染分支。
    if is_archive(path) {
        return Some(&ARCHIVE);
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
    fn priorities_are_strictly_increasing() {
        let ps: Vec<u8> = BACKENDS.iter().map(|b| b.priority()).collect();
        assert!(ps.windows(2).all(|w| w[0] < w[1]), "{ps:?}");
    }
}
