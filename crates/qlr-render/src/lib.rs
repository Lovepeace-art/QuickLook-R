//! QLRust 渲染层：把"文件"变成"能上屏的东西"。
//!
//! 分层约定（重要）：
//! - **主要路径不依赖 `windows`**，纯 Rust + `image`（+ 动态加载的 pdfium.dll）。
//!   可以 `cargo test` 跑、可以被基准脚本直接调。
//! - 上屏（`StretchDIBits` / `DrawTextW`）在 `qlr-platform`，本 crate 只交 `Canvas`。
//! - 格式路由查 `qlr_core::format`（内置 8 类 `Kind`：Image / Pdf / Text / Markdown /
//!   Svg / Html / Audio / Video）。
//!
//! 已实现：`Image`（位图，JPEG 走缩放解码）、`Text`（纯文本管线）、
//! `Markdown`（在文本管线之上叠轻量排版）、`Pdf`（pdfium.dll，动态加载）、
//! `Svg`（Direct2D 1.3 优先 + 内置光栅化器兜底）、`Html`（当文档排版，不跑脚本）、
//! `Video`（Media Foundation 解首帧）、`Audio`（元信息卡片，出声在壳层 `audio::AudioPlayer`）。
//!
//! `windows` 只出现在两条 target 依赖里（`svg` 的 D2D、`media` 的 Media Foundation），
//! SVG 有纯 Rust 兜底，所以 Windows 上缺 GPU 也能出图。
//! 八类之外返回 `Error::Message`，壳层据此提示"后续以插件形式提供"。

pub mod archive;
pub mod budget;
pub mod cancel;
pub mod canvas;
pub mod code;
pub mod downscale;
pub mod html;
pub mod image_view;
pub mod markdown;
pub mod media;
pub mod pdf;
pub mod scale;
pub mod sheet;
pub mod source_cache;
pub mod svg;
pub mod text;
pub mod wrap;

#[cfg(test)]
mod testutil;

pub use canvas::{Bitmap, Canvas, LineStyle, Span, TextDoc, Token};
pub use media::MediaInfo;

use std::path::Path;

use qlr_core::{Error, Kind, Result, format};

/// 一次渲染请求。尺寸是**显示预算**（窗口像素），不是原图尺寸。
#[derive(Debug, Clone)]
pub struct RenderRequest<'a> {
    pub path: &'a Path,
    pub max_width: u32,
    pub max_height: u32,
    /// 文本类取开头还是结尾（日志常看尾部）。
    pub head: bool,
    /// PDF 要看第几页（0 起）。其他渲染器忽略这个字段。
    pub page: usize,
    /// 允许"放大渲染"（超过原始像素也要更大的位图）。
    ///
    /// 默认 `false`：预览一张小图不该把它糊成整屏。用户缩放（明确要放大）时才置 `true`，
    /// 否则 `zoom > 1` 只能裁剪、看不到任何细节增加。
    pub upscale: bool,
}

impl<'a> RenderRequest<'a> {
    pub fn new(path: &'a Path, max_width: u32, max_height: u32) -> Self {
        Self {
            path,
            max_width,
            max_height,
            head: true,
            page: 0,
            upscale: false,
        }
    }

    /// 指定 PDF 页（其余字段沿用默认）。
    pub fn with_page(mut self, page: usize) -> Self {
        self.page = page;
        self
    }

    /// 允许放大渲染（用户缩放时要更多像素，而不是裁剪）。
    pub fn with_upscale(mut self, upscale: bool) -> Self {
        self.upscale = upscale;
        self
    }
}

/// 渲染器统一契约。实现者必须无状态可复用（壳层会用长驻 worker 线程反复调用）。
pub trait Viewer: Send + Sync {
    fn kind(&self) -> Kind;
    fn render(&self, req: &RenderRequest<'_>) -> Result<Canvas>;
}

/// 位图渲染器。
pub struct ImageViewer;

impl Viewer for ImageViewer {
    fn kind(&self) -> Kind {
        Kind::Image
    }

    fn render(&self, req: &RenderRequest<'_>) -> Result<Canvas> {
        image_view::render_scaled(req.path, req.max_width, req.max_height, req.upscale)
    }
}

/// 纯文本渲染器。
pub struct TextViewer;

impl Viewer for TextViewer {
    fn kind(&self) -> Kind {
        Kind::Text
    }

    fn render(&self, req: &RenderRequest<'_>) -> Result<Canvas> {
        // 1.0.8：源码文件走代码着色（等宽 + 行号 + 关键字/字符串/注释/数字），
        // 其余纯文本仍走朴素排版。两者共用同一套读取/解码/截断预算。
        if code::is_code(req.path) {
            code::render(req.path, req.head)
        } else {
            text::render(req.path, req.head)
        }
    }
}

/// 代码预览（等宽 + 行号 + 词法着色）。
pub struct CodeViewer;

impl Viewer for CodeViewer {
    fn kind(&self) -> Kind {
        Kind::Text
    }

    fn render(&self, req: &RenderRequest<'_>) -> Result<Canvas> {
        code::render(req.path, req.head)
    }
}

/// Markdown 渲染器：文本管线 + 轻量块级排版（标题/代码/引用/列表/分隔线）。
pub struct MarkdownViewer;

impl Viewer for MarkdownViewer {
    fn kind(&self) -> Kind {
        Kind::Markdown
    }

    fn render(&self, req: &RenderRequest<'_>) -> Result<Canvas> {
        markdown::render(req.path, req.head)
    }
}

/// PDF 渲染器（pdfium.dll；单页渲染，翻页由壳层改 `req.page`）。
pub struct PdfViewer;

impl Viewer for PdfViewer {
    fn kind(&self) -> Kind {
        Kind::Pdf
    }

    fn render(&self, req: &RenderRequest<'_>) -> Result<Canvas> {
        pdf::render_page(req.path, req.page, req.max_width, req.max_height)
    }
}

static IMAGE_VIEWER: ImageViewer = ImageViewer;
static TEXT_VIEWER: TextViewer = TextViewer;
static MARKDOWN_VIEWER: MarkdownViewer = MarkdownViewer;
static PDF_VIEWER: PdfViewer = PdfViewer;

/// SVG 渲染器（1.7.9）：输出尺寸就是请求尺寸（内容等比居中，白底）。
pub struct SvgViewer;

impl Viewer for SvgViewer {
    fn kind(&self) -> Kind {
        Kind::Svg
    }

    fn render(&self, req: &RenderRequest<'_>) -> Result<Canvas> {
        // `svg::render` 用 `Option`（两种后端都失败才是 None），这里翻译成带路径的错误，
        // 好让壳层的日志能说清是哪个文件、而不是一句"渲染失败"。
        svg::render(req.path, req.max_width, req.max_height).ok_or_else(|| {
            Error::Message(format!(
                "SVG 渲染失败（文件损坏或不支持的写法）：{}",
                req.path.display()
            ))
        })
    }
}

/// HTML 渲染器（1.7.9）：当**文档**排版——不执行脚本、不取网络资源、只认一小撮标签。
pub struct HtmlViewer;

impl Viewer for HtmlViewer {
    fn kind(&self) -> Kind {
        Kind::Html
    }

    fn render(&self, req: &RenderRequest<'_>) -> Result<Canvas> {
        html::render(req.path, req.head)
    }
}

/// 视频渲染器（1.7.9）：Media Foundation 解**首帧**上屏（不做播放）。
///
/// 解不出画面时不让预览窗空着：退回一张"元信息卡片"，用户至少能看到时长/分辨率/编码，
/// 并知道这是"没法解码"而不是"文件坏了"。
pub struct VideoViewer;

impl Viewer for VideoViewer {
    fn kind(&self) -> Kind {
        Kind::Video
    }

    fn render(&self, req: &RenderRequest<'_>) -> Result<Canvas> {
        let info = media::probe(req.path);
        if let Some(canvas) = media::first_frame(req.path, req.max_width, req.max_height) {
            return Ok(canvas);
        }
        let lines = media_card(
            req.path,
            info.as_ref(),
            "无法解码首帧（缺少解码器或文件损坏）",
        );
        Ok(Canvas::Text(TextDoc {
            total_lines: lines.len(),
            lines,
            ..TextDoc::default()
        }))
    }
}

/// 音频渲染器（1.7.9）：只出**元信息卡片**，声音由壳层 `audio::AudioPlayer` 放。
///
/// 之所以放在渲染层：预览窗要显示时长/格式/采样这些"看起来像文件"的东西，
/// 而不是一句"暂不支持"；音频本身没有画面，卡片就是它的画面。
pub struct AudioViewer;

impl Viewer for AudioViewer {
    fn kind(&self) -> Kind {
        Kind::Audio
    }

    fn render(&self, req: &RenderRequest<'_>) -> Result<Canvas> {
        let info = media::probe(req.path);
        let mut lines = media_card(req.path, info.as_ref(), "");
        lines.push(String::new());
        // 键位必须和壳层实现一致：P 播放/暂停、←/→ 快退/快进 5 秒、滚轮定位
        // （`app.rs` 的 `on_hotkey` 0x50/0x25/0x27 与 `on_wheel`）。
        lines.push("P：播放 / 暂停      ← / →：快退 / 快进 5 秒      滚轮：定位".to_string());
        Ok(Canvas::Text(TextDoc {
            total_lines: lines.len(),
            lines,
            ..TextDoc::default()
        }))
    }
}

/// 电子表格渲染器（1.9.0）：**只解析，不出像素**。
///
/// 与其它预览不同，表格要能滚动、要能换 sheet，所以这一层返回的是
/// `Canvas::Sheet(SheetDoc)` 这种纯数据；真正画到窗口（列号行号表头、网格线、
/// 合并单元格、数字右对齐）在 `qlr_platform::window::sheet`。
///
/// 这么切的两个理由：
/// 1. `qlr-render` 不许依赖 `windows`，解析器因此能在 `cargo test` 里无窗口单测；
/// 2. 滚动/缩放只是重画，不该每次重解析（20 万格要 130ms，而重画一屏只要 13ms）。
pub struct SheetViewer;

impl Viewer for SheetViewer {
    fn kind(&self) -> Kind {
        Kind::Sheet
    }

    fn render(&self, req: &RenderRequest<'_>) -> Result<Canvas> {
        // 工作簿主战场是"看当前 sheet 的第一屏"，`page` 复用来选第几个工作表
        // （表格本来就没有"页"，壳层翻页键就是切 sheet）。
        //
        // 这里用**显式下标**（`parse_sheet_index`）：壳层已经把 `page` 初始化成
        // 工作簿的 `activeTab`（见 `sheet::sheet_layout`），所以 `page = 0` 该是
        // "第 0 张"，不能又解释成"跟 activeTab 走"——否则用户在 Excel 里停在
        // 第 3 张，预览按 PageUp 会从第 3 张跳回第 3 张（看起来像按键失灵）。
        Ok(Canvas::Sheet(Box::new(sheet::parse_sheet_index(
            req.path, req.page,
        )?)))
    }
}

static SVG_VIEWER: SvgViewer = SvgViewer;
static HTML_VIEWER: HtmlViewer = HtmlViewer;
static VIDEO_VIEWER: VideoViewer = VideoViewer;
static AUDIO_VIEWER: AudioViewer = AudioViewer;
static SHEET_VIEWER: SheetViewer = SheetViewer;

/// 拼一张媒体"元信息卡片"的文字行（音频卡片与视频降级共用）。
///
/// `note` 非空时插在第一行（用于说明"为什么只有卡片没有画面"）。
fn media_card(path: &Path, info: Option<&MediaInfo>, note: &str) -> Vec<String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let mut lines: Vec<String> = Vec::new();
    if !note.is_empty() {
        lines.push(note.to_string());
        lines.push(String::new());
    }
    lines.push(name);
    lines.push(String::new());
    match info {
        Some(i) => {
            if i.has_video {
                let fps = match i.frame_rate {
                    Some((n, d)) if d > 0 => format!(" · {:.0} fps", f64::from(n) / f64::from(d)),
                    _ => String::new(),
                };
                lines.push(format!("分辨率：{} × {}{}", i.width, i.height, fps));
            }
            if let Some(codec) = &i.video_codec {
                lines.push(format!("视频编码：{codec}"));
            }
            lines.push(format!("时长：{}", fmt_ms(i.duration_ms)));
            lines.push(format!("音轨：{}", if i.has_audio { "有" } else { "无" }));
        }
        None => lines.push("元信息：读不到（文件损坏或缺解码器）".to_string()),
    }
    if let Ok(meta) = std::fs::metadata(path) {
        lines.push(format!("大小：{}", fmt_bytes(meta.len())));
    }
    lines.push(format!(
        "类型：{}",
        path.extension()
            .map(|e| e.to_string_lossy().to_uppercase())
            .unwrap_or_else(|| "未知".to_string())
    ));
    lines
}

/// 毫秒 → `m:ss`（超过一小时给 `h:mm:ss`）。
fn fmt_ms(ms: u64) -> String {
    let total = ms / 1000;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// 字节数 → 人类可读（与壳层日志口径一致：1024 进制）。
fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// 按类别取渲染器。返回 `None` 表示没有内置查看器（八类之外，走插件/系统处理器）。
pub fn viewer_for(kind: Kind) -> Option<&'static dyn Viewer> {
    match kind {
        Kind::Image => Some(&IMAGE_VIEWER),
        Kind::Text => Some(&TEXT_VIEWER),
        Kind::Markdown => Some(&MARKDOWN_VIEWER),
        Kind::Pdf => Some(&PDF_VIEWER),
        Kind::Svg => Some(&SVG_VIEWER),
        Kind::Html => Some(&HTML_VIEWER),
        Kind::Audio => Some(&AUDIO_VIEWER),
        Kind::Video => Some(&VIDEO_VIEWER),
        Kind::Sheet => Some(&SHEET_VIEWER),
        Kind::Unsupported => None,
    }
}

/// 该格式是否"有渲染器"（不代表一定能渲染成功：PDF 缺 dll 会在 render 时报错）。
pub fn is_supported(kind: Kind) -> bool {
    viewer_for(kind).is_some()
}

/// 按扩展名路由并渲染。这是壳层唯一需要调用的入口。
pub fn render_file(path: &Path, max_width: u32, max_height: u32) -> Result<Canvas> {
    render_file_at(path, max_width, max_height, 0)
}

/// 同上，但指定 PDF 页码（其他格式忽略 `page`）。
pub fn render_file_at(path: &Path, max_width: u32, max_height: u32, page: usize) -> Result<Canvas> {
    render_file_at_zoom(path, max_width, max_height, page, false)
}

/// 同上，但允许放大渲染（用户缩放时要更多像素，而不是裁剪）。
pub fn render_file_at_zoom(
    path: &Path,
    max_width: u32,
    max_height: u32,
    page: usize,
    upscale: bool,
) -> Result<Canvas> {
    let kind = format::kind_for_path(path);
    let Some(viewer) = viewer_for(kind) else {
        return Err(Error::Message(format!(
            "{kind} 暂无内置查看器（{}）：{}",
            kind.hint(),
            path.display()
        )));
    };
    viewer.render(
        &RenderRequest::new(path, max_width, max_height)
            .with_page(page)
            .with_upscale(upscale),
    )
}

/// PDF 页数（非 PDF 返回 None）。壳层用它显示 "第 x / N 页"。
pub fn pdf_page_count(path: &Path) -> Option<usize> {
    if format::kind_for_path(path) != Kind::Pdf {
        return None;
    }
    pdf::page_count(path).ok()
}

/// 文件**原始**显示尺寸（像素），用于"按原尺寸开窗"。
///
/// - 图片：读文件头拿像素尺寸，不解码；
/// - SVG：读 `width`/`height`（缺失时退回 `viewBox`）；
/// - 视频：Media Foundation 读首帧尺寸（不解码整帧）；
/// - PDF：取第 1 页的 point 尺寸，按 96 DPI（1 pt = 96/72 px）换算成像素；
/// - 文本 / Markdown / HTML / 音频：没有"原始尺寸"概念，返回 `None`。
///
/// 失败一律 `None`：开窗是交互路径，不能因为探测失败就不开窗。
pub fn natural_size(path: &Path, kind: Kind) -> Option<(u32, u32)> {
    match kind {
        Kind::Image => image_view::dimensions(path),
        Kind::Svg => svg::natural_size(path),
        Kind::Video => media::probe(path)
            .filter(|i| i.has_video && i.width > 0 && i.height > 0)
            .map(|i| (i.width, i.height)),
        Kind::Pdf => pdf::page_size(path, 0).ok().and_then(|(w, h)| {
            let (pw, ph) = (w * 96.0 / 72.0, h * 96.0 / 72.0);
            if !(pw.is_finite() && ph.is_finite()) || pw < 1.0 || ph < 1.0 {
                return None;
            }
            Some((pw.round() as u32, ph.round() as u32))
        }),
        Kind::Text
        | Kind::Markdown
        | Kind::Html
        | Kind::Audio
        | Kind::Sheet
        | Kind::Unsupported => None,
    }
}

/// 媒体文件的概要信息（音频/视频的时长、分辨率、编码）。
///
/// 壳层用它做"日志 + 顶栏提示"；读不到返回 `None`（不是错误：老容器/缺解码器都这样）。
pub fn media_info(path: &Path) -> Option<MediaInfo> {
    media::probe(path)
}

/// 只给日志用的文件名（**不含目录**：日志里禁止出现用户的完整路径）。
pub(crate) fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "（无文件名）".to_string())
}

/// JPEG 缩放解码失败时退回通用路径的痕迹（debug 级，不打扰用户）。
pub(crate) fn trace_jpeg_fallback(path: &Path, err: &Error) {
    qlr_core::log_debug!(
        "render",
        "JPEG 缩放解码失败，退回通用路径：{err}（{}）",
        path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn known_kinds_route_to_viewer() {
        assert!(viewer_for(Kind::Image).is_some());
        assert!(viewer_for(Kind::Text).is_some());
        assert!(viewer_for(Kind::Markdown).is_some());
        assert!(viewer_for(Kind::Pdf).is_some());
        assert!(viewer_for(Kind::Svg).is_some());
        assert!(viewer_for(Kind::Html).is_some());
        assert!(viewer_for(Kind::Audio).is_some());
        assert!(viewer_for(Kind::Video).is_some());
        // 八类之外必须返回 None（壳层据此出"暂未支持"提示）
        assert!(viewer_for(Kind::Unsupported).is_none());
        // `format::KINDS` 里的每一类都必须有渲染器（否则壳层会开窗然后报"暂不支持"）
        for k in format::KINDS {
            assert!(is_supported(*k), "{k} 没有渲染器但被登记为内置格式");
        }
    }

    #[test]
    fn unimplemented_kind_reports_plugin_hint() {
        let err = render_file(&PathBuf::from(r"C:\x\a.docx"), 800, 600).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unsupported"), "{msg}");
        assert!(msg.contains("插件"), "应提示插件路线：{msg}");
    }

    #[test]
    fn unknown_kind_is_rejected_not_panicking() {
        let err = render_file(&PathBuf::from(r"C:\x\a.zzz"), 800, 600).unwrap_err();
        assert!(format!("{err}").contains("unsupported"), "{err}");
    }

    #[test]
    fn viewer_kind_matches_expected() {
        assert_eq!(IMAGE_VIEWER.kind(), Kind::Image);
        assert_eq!(TEXT_VIEWER.kind(), Kind::Text);
        assert_eq!(MARKDOWN_VIEWER.kind(), Kind::Markdown);
        assert_eq!(PDF_VIEWER.kind(), Kind::Pdf);
        assert_eq!(SVG_VIEWER.kind(), Kind::Svg);
        assert_eq!(HTML_VIEWER.kind(), Kind::Html);
        assert_eq!(AUDIO_VIEWER.kind(), Kind::Audio);
        assert_eq!(VIDEO_VIEWER.kind(), Kind::Video);
    }

    #[test]
    fn media_card_shows_duration_and_falls_back_without_info() {
        let p = PathBuf::from(r"C:\x\audio-demo.wav");
        let card = media_card(&p, None, "读不到");
        assert!(
            card.iter().any(|l| l.contains("audio-demo.wav")),
            "{card:?}"
        );
        assert!(card.iter().any(|l| l.contains("读不到")), "{card:?}");
        assert!(card.iter().any(|l| l == "类型：WAV"), "{card:?}");

        let info = MediaInfo {
            width: 1280,
            height: 720,
            duration_ms: 3_000,
            has_video: true,
            has_audio: true,
            frame_rate: Some((30, 1)),
            video_codec: Some("H264".to_string()),
        };
        let card = media_card(&p, Some(&info), "");
        assert!(
            card.iter().any(|l| l == "分辨率：1280 × 720 · 30 fps"),
            "{card:?}"
        );
        assert!(card.iter().any(|l| l == "时长：0:03"), "{card:?}");
        assert!(card.iter().any(|l| l == "视频编码：H264"), "{card:?}");
        assert!(card.iter().any(|l| l == "音轨：有"), "{card:?}");
    }

    #[test]
    fn duration_formatting_switches_to_hours() {
        assert_eq!(fmt_ms(0), "0:00");
        assert_eq!(fmt_ms(3_000), "0:03");
        assert_eq!(fmt_ms(65_000), "1:05");
        assert_eq!(fmt_ms(3_723_000), "1:02:03");
    }

    #[test]
    fn byte_formatting_is_binary() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(2_048), "2.0 KB");
        assert_eq!(fmt_bytes(7 * 1024 * 1024), "7.0 MB");
    }

    #[test]
    fn pdf_page_count_is_none_for_other_kinds() {
        assert!(pdf_page_count(&PathBuf::from(r"C:\x\a.txt")).is_none());
    }

    #[test]
    fn natural_size_reads_image_header_only() {
        // 最小合法 PNG（1×1，透明像素），只为验证"只读文件头就能拿到尺寸"。
        const PNG: [u8; 67] = [
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, // 签名
            0x00, 0x00, 0x00, 0x0d, b'I', b'H', b'D', b'R', // IHDR 长度 + 类型
            0x00, 0x00, 0x00, 0x01, // 宽 1
            0x00, 0x00, 0x00, 0x01, // 高 1
            0x08, 0x06, 0x00, 0x00, 0x00, // 位深 8 / 真彩+α / 压缩 / 滤波 / 隔行
            0x1f, 0x15, 0xc4, 0x89, // CRC
            0x00, 0x00, 0x00, 0x0a, b'I', b'D', b'A', b'T', // IDAT
            0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, //
            0x0d, 0x0a, 0x2d, 0xb4, // CRC
            0x00, 0x00, 0x00, 0x00, b'I', b'E', b'N', b'D', // IEND
            0xae, 0x42, 0x60, 0x82, // CRC
        ];
        let png = crate::testutil::write_file("natural", "one.png", &PNG);
        assert_eq!(natural_size(&png, Kind::Image), Some((1, 1)));

        // 文本 / HTML / 插件格式没有"原始尺寸"
        let txt = crate::testutil::write_file("natural", "a.txt", b"hello");
        assert_eq!(natural_size(&txt, Kind::Text), None);
        assert_eq!(natural_size(&txt, Kind::Markdown), None);
        assert_eq!(natural_size(&txt, Kind::Html), None);
        assert_eq!(natural_size(&txt, Kind::Unsupported), None);

        // 坏文件不能 panic，只能返回 None
        let bad = crate::testutil::write_file("natural", "broken.png", b"not a png");
        assert_eq!(natural_size(&bad, Kind::Image), None);
    }
}
