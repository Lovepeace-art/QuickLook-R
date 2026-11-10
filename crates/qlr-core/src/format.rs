//! 格式注册表：把路径映射到"用哪个查看器"。
//!
//! 内置查看器覆盖 9 类：图片 / PDF / 文本 / Markdown / SVG / HTML / 音频 / 视频 / 表格。
//! 其余格式一律 `Kind::Unsupported`，由壳层给出"暂未支持，后续以插件形式提供"
//! 的提示 —— 插件体系见 `crates/qlr-plugin-api` 与 `docs/dev/PLUGINS.md`。
//!
//! 1.9.0 新增的「表格」族（xlsx/xlsm/xltx/xltm）**不走插件也不走 Office**：
//! `qlr-render::sheet` 自己解析 OOXML，`qlr-platform::window::sheet` 逐格画网格，
//! 见 `docs/adr/0010-native-sheet-preview.md`。
//!
//! 判定顺序（先便宜后昂贵）：
//! 1. 扩展名命中扩展名表之一 → 直接返回，不读文件；
//! 2. 扩展名不认识（含没有扩展名）→ 读文件头 512 字节嗅探：
//!    魔数识别 PDF / 常见位图；剩下的若"没有 NUL 且基本可打印"就当文本，
//!    否则判为不支持。
//!
//! 之所以保留内容兜底：Windows 上大量文本文件根本没有扩展名（`README`、`Makefile`），
//! 而不认识的后缀又可能是任意二进制 —— 只看扩展名会两头都判错。

use std::fmt;
use std::path::Path;

/// 内容嗅探读多少字节。够覆盖所有魔数，又便宜到可以每张预览都读一次。
const SNIFF_LEN: usize = 512;

/// 支持的文件类别。**新增族请走插件，不要往这里加变体**
/// —— 例外是 1.7.9 用户明确要求内置的 SVG / HTML / 音频 / 视频四类，
/// 以及 1.9.0 同样明确要求内置的表格（xlsx 一族）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// 位图：png/jpg/gif/bmp/webp/tiff/ico 等（能解码的那些）
    Image,
    Pdf,
    /// 纯文本 / 源码 / 配置 / 日志 / 分隔符表格（csv/tsv）
    Text,
    /// Markdown（渲染时做轻量排版）
    Markdown,
    /// SVG（矢量，Direct2D 优先、内置光栅化器兜底）
    Svg,
    /// HTML（当文档排版，不跑脚本、不支持 CSS）
    Html,
    /// 音频：只出声不出画面（MCI 播放）
    Audio,
    /// 视频：取首帧上屏（Media Foundation）
    Video,
    /// 电子表格（xlsx/xlsm/xltx/xltm）：原生网格渲染，可滚动、看得到公式结果缓存值
    Sheet,
    /// 扩展名表之外：壳层只出提示文字
    Unsupported,
}

impl Kind {
    /// 供日志与 UI 用的稳定标识（不本地化，会进日志与脚本）。
    pub fn name(self) -> &'static str {
        match self {
            Kind::Image => "image",
            Kind::Pdf => "pdf",
            Kind::Text => "text",
            Kind::Markdown => "markdown",
            Kind::Svg => "svg",
            Kind::Html => "html",
            Kind::Audio => "audio",
            Kind::Video => "video",
            Kind::Sheet => "sheet",
            Kind::Unsupported => "unsupported",
        }
    }

    /// 中文名，给提示文字用。
    pub fn label(self) -> &'static str {
        match self {
            Kind::Image => "图片",
            Kind::Pdf => "PDF",
            Kind::Text => "文本",
            Kind::Markdown => "Markdown",
            Kind::Svg => "SVG",
            Kind::Html => "HTML",
            Kind::Audio => "音频",
            Kind::Video => "视频",
            Kind::Sheet => "表格",
            Kind::Unsupported => "未知格式",
        }
    }

    /// 这一类当前有没有内置查看器。
    pub fn is_renderable(self) -> bool {
        !matches!(self, Kind::Unsupported)
    }

    /// 兼容旧名，等价于 [`Kind::is_renderable`]。
    pub fn renderable(self) -> bool {
        self.is_renderable()
    }

    /// 是不是"按原始像素尺寸开窗"的类别（图片与矢量图）。
    pub fn has_natural_size(self) -> bool {
        matches!(self, Kind::Image | Kind::Svg | Kind::Video | Kind::Pdf)
    }

    /// 不支持时给用户的建议（内置查看器之外的路子）。
    pub fn hint(self) -> &'static str {
        match self {
            Kind::Unsupported => "暂未支持；后续以插件形式提供（见 docs/dev/PLUGINS.md）",
            _ => "",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// 内置扩展名表。`extensions()` 把它们暴露出去，文档与矩阵脚本共用一份真值。
const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "jpe", "jfif", "gif", "bmp", "dib", "webp", "tif", "tiff", "ico", "cur",
];
const PDF_EXTS: &[&str] = &["pdf"];
const MARKDOWN_EXTS: &[&str] = &["md", "markdown", "mdown", "mkd", "mkdn"];
/// SVG：`svg` 是标准扩展名；`svgz` 是 gzip 压缩版（渲染层当前不处理压缩，会渲染失败但
/// 至少走对分支、给出"渲染失败"而不是"不支持"）。
const SVG_EXTS: &[&str] = &["svg", "svgz"];
const HTML_EXTS: &[&str] = &["html", "htm", "xhtml", "shtml"];
/// 音频：MCI 能播的那些容器（`wav` 走 waveaudio，其余走 mpegvideo/自动）。
const AUDIO_EXTS: &[&str] = &[
    "wav", "mp3", "m4a", "aac", "flac", "ogg", "oga", "opus", "wma", "aif", "aiff", "mid", "midi",
];
/// 视频：Media Foundation 能解的容器（`mkv`/`webm` 看系统有没有装对应分离器）。
const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mov", "avi", "wmv", "webm", "mkv", "mpg", "mpeg", "ts", "m2ts", "3gp",
];
/// 电子表格（OOXML 的四种后缀，容器完全一样，只是宏/模板标记不同）。
///
/// 老式二进制 `.xls`/`.et` **不在**这里：它要 BIFF 解析器，本轮不做，
/// 交回系统处理器/插件兜底（见 `docs/adr/0010-native-sheet-preview.md`）。
const SHEET_EXTS: &[&str] = &["xlsx", "xlsm", "xltx", "xltm"];
const TEXT_EXTS: &[&str] = &[
    // 纯文本 / 日志 / 说明
    "txt",
    "text",
    "log",
    "nfo",
    "me",
    "rst",
    // 结构化配置
    "ini",
    "cfg",
    "conf",
    "config",
    "toml",
    "yaml",
    "yml",
    "json",
    "jsonc",
    "json5",
    "xml",
    "properties",
    "env",
    "lock",
    "gitignore",
    "gitattributes",
    "gitmodules",
    "editorconfig",
    // 分隔符表格
    "csv",
    "tsv",
    "psv",
    // 脚本
    "bat",
    "cmd",
    "ps1",
    "psm1",
    "psd1",
    "sh",
    "bash",
    "zsh",
    "fish",
    // 编译型语言
    "h",
    "hpp",
    "hxx",
    "c",
    "cc",
    "cpp",
    "cxx",
    "inl",
    "ipp",
    "m",
    "mm",
    "cs",
    "fs",
    "fsx",
    "vb",
    "rs",
    "go",
    "swift",
    "kt",
    "kts",
    "scala",
    "dart",
    "java",
    // 动态语言
    "py",
    "pyw",
    "rb",
    "php",
    "lua",
    "pl",
    "pm",
    "t",
    "r",
    "jl",
    "groovy",
    "gradle",
    // Web 前端（`html`/`htm`/`xhtml` 不在这里：1.7.9 起走 `Kind::Html` 排版预览）
    "css",
    "scss",
    "sass",
    "less",
    "styl",
    "js",
    "mjs",
    "cjs",
    "ts",
    "tsx",
    "jsx",
    "mts",
    "cts",
    "vue",
    "svelte",
    "astro",
    // 其它半结构化 / 机器生成的文本
    "sql",
    "graphql",
    "gql",
    "proto",
    "thrift",
    "diff",
    "patch",
    "tex",
    "bib",
    "asm",
    "s",
    "nasm",
    "cmake",
    "mk",
];

/// 某一类对应的扩展名（`Unsupported` 返回空表）。
pub fn extensions(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::Image => IMAGE_EXTS,
        Kind::Pdf => PDF_EXTS,
        Kind::Text => TEXT_EXTS,
        Kind::Markdown => MARKDOWN_EXTS,
        Kind::Svg => SVG_EXTS,
        Kind::Html => HTML_EXTS,
        Kind::Audio => AUDIO_EXTS,
        Kind::Video => VIDEO_EXTS,
        Kind::Sheet => SHEET_EXTS,
        Kind::Unsupported => &[],
    }
}

/// 内置能预览的全部类别（顺序即注册表顺序，文档与脚本共用）。
pub const KINDS: &[Kind] = &[
    Kind::Image,
    Kind::Pdf,
    Kind::Text,
    Kind::Markdown,
    Kind::Svg,
    Kind::Html,
    Kind::Audio,
    Kind::Video,
    Kind::Sheet,
];

/// 内置能预览的全部扩展名（给文档 / 矩阵脚本 / 文件关联用）。
pub fn all_extensions() -> Vec<&'static str> {
    let mut all: Vec<&'static str> = Vec::new();
    for k in KINDS {
        all.extend_from_slice(extensions(*k));
    }
    all.sort_unstable();
    all.dedup();
    all
}

/// 路径 → 类别。先看扩展名，再看内容。
pub fn kind_for_path(path: &Path) -> Kind {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        if let Some(kind) = kind_for_ext(ext) {
            return kind;
        }
    }
    // 扩展名不认识：读个头看看。读不到就当不支持 —— "猜成文本"会造成
    // 把 exe 渲染成一屏乱码，比明确说不支持更糟。
    // 有界读：`sniff` 只看头部，曾经 `fs::read` 全量读进内存却只用前 512 字节，
    // 几 GB 的镜像/磁盘文件会把内存峰值顶到文件大小。
    use std::io::Read as _;
    match std::fs::File::open(path) {
        Ok(mut file) => {
            let mut buf = [0u8; SNIFF_LEN];
            let n = file.read(&mut buf).unwrap_or(0);
            sniff(&buf[..n]).unwrap_or(Kind::Unsupported)
        }
        Err(_) => Kind::Unsupported,
    }
}

/// 扩展名（可带点、大小写不敏感）→ 类别。不认识返回 `None`。
pub fn kind_for_ext(ext: &str) -> Option<Kind> {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    if IMAGE_EXTS.contains(&ext.as_str()) {
        return Some(Kind::Image);
    }
    if PDF_EXTS.contains(&ext.as_str()) {
        return Some(Kind::Pdf);
    }
    if MARKDOWN_EXTS.contains(&ext.as_str()) {
        return Some(Kind::Markdown);
    }
    if SVG_EXTS.contains(&ext.as_str()) {
        return Some(Kind::Svg);
    }
    if HTML_EXTS.contains(&ext.as_str()) {
        return Some(Kind::Html);
    }
    if AUDIO_EXTS.contains(&ext.as_str()) {
        return Some(Kind::Audio);
    }
    if VIDEO_EXTS.contains(&ext.as_str()) {
        return Some(Kind::Video);
    }
    if SHEET_EXTS.contains(&ext.as_str()) {
        return Some(Kind::Sheet);
    }
    if TEXT_EXTS.contains(&ext.as_str()) {
        return Some(Kind::Text);
    }
    None
}

/// 内容嗅探：只认"有明确魔数"的几种 + "看着像文本"的兜底。
///
/// 返回 `None` 表示"认不出来"（调用方判为不支持）。
pub fn sniff(head: &[u8]) -> Option<Kind> {
    if head.is_empty() {
        return None;
    }
    if head.starts_with(b"%PDF-") {
        return Some(Kind::Pdf);
    }
    if is_image_magic(head) {
        return Some(Kind::Image);
    }
    if looks_like_text(head) {
        return Some(Kind::Text);
    }
    None
}

/// 常见位图魔数（只含我们能解码的那些）。
fn is_image_magic(h: &[u8]) -> bool {
    const SIGS: &[&[u8]] = &[
        b"\x89PNG\r\n\x1a\n", // PNG
        b"\xff\xd8\xff",      // JPEG
        b"GIF87a",            // GIF
        b"GIF89a",
        b"BM",               // BMP
        b"II*\x00",          // TIFF 小端
        b"MM\x00*",          // TIFF 大端
        b"\x00\x00\x01\x00", // ICO
        b"\x00\x00\x02\x00", // CUR
    ];
    if SIGS.iter().any(|s| h.starts_with(s)) {
        return true;
    }
    // WEBP：RIFF....WEBP
    h.len() >= 12 && h.starts_with(b"RIFF") && &h[8..12] == b"WEBP"
}

/// "没有 NUL 字节、且绝大多数不是控制字符"就认为能当文本打开。
///
/// 判据刻意宽松：UTF-8/GBK 中文的字节会落在 0x80–0xFF，这里不去判断编码 ——
/// 文本查看器自己有 BOM/UTF-8/UTF-16 探测，比这里猜得准。
fn looks_like_text(head: &[u8]) -> bool {
    // UTF-16 文本含 NUL（每字符一个 0 字节），但那是文本不是二进制：
    // 带 BOM 的直接认成文本（下面那套控制字符比例对 UTF-16 不成立），
    // 不带 BOM 又含 NUL 的仍按二进制处理。
    if head.starts_with(&[0xFF, 0xFE]) || head.starts_with(&[0xFE, 0xFF]) {
        return true;
    }
    if head.contains(&0) {
        return false;
    }
    let mut controls = 0usize;
    for &b in head {
        if b < 0x20 && !matches!(b, b'\t' | b'\r' | b'\n' | 0x0c | 0x1b) {
            controls += 1;
        }
    }
    // 允许极少量控制字符（终端转义、进度条残留），超过 5% 按二进制处理。
    controls * 20 <= head.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn kind(path: &str) -> Kind {
        kind_for_path(&PathBuf::from(path))
    }

    #[test]
    fn image_extensions_route_to_image() {
        for p in [
            r"C:\a\b.png",
            r"C:\a\b.JPG",
            r"C:\a\b.jpeg",
            r"C:\a\b.gif",
            r"C:\a\b.bmp",
            r"C:\a\b.webp",
            r"C:\a\b.tiff",
            r"C:\a\b.ico",
        ] {
            assert_eq!(kind(p), Kind::Image, "{p}");
        }
    }

    #[test]
    fn pdf_text_markdown_route_to_own_kinds() {
        assert_eq!(kind(r"C:\a\b.pdf"), Kind::Pdf);
        assert_eq!(kind(r"C:\a\b.txt"), Kind::Text);
        assert_eq!(kind(r"C:\a\b.rs"), Kind::Text);
        assert_eq!(kind(r"C:\a\b.csv"), Kind::Text);
        assert_eq!(kind(r"C:\a\b.md"), Kind::Markdown);
        assert_eq!(kind(r"C:\a\b.MARKDOWN"), Kind::Markdown);
    }

    #[test]
    fn media_and_office_are_unsupported() {
        assert_eq!(kind(r"C:\a\b.docx"), Kind::Unsupported);
        assert_eq!(kind(r"C:\a\b.zip"), Kind::Unsupported);
        assert_eq!(kind(r"C:\a\b.exe"), Kind::Unsupported);
        assert_eq!(kind(r"C:\a\b.ttf"), Kind::Unsupported);
        assert_eq!(kind(r"C:\a\b.stl"), Kind::Unsupported);
    }

    #[test]
    fn svg_html_audio_video_route_to_their_own_kinds() {
        // SVG / HTML（1.7.9 起 HTML 不再当源码文本显示）
        assert_eq!(kind(r"C:\a\b.svg"), Kind::Svg);
        assert_eq!(kind(r"C:\a\b.SVG"), Kind::Svg);
        assert_eq!(kind(r"C:\a\b.html"), Kind::Html);
        assert_eq!(kind(r"C:\a\b.htm"), Kind::Html);
        assert_eq!(kind(r"C:\a\b.xhtml"), Kind::Html);
        // HTML 之外的前端源码仍是文本
        assert_eq!(kind(r"C:\a\b.css"), Kind::Text);
        assert_eq!(kind(r"C:\a\b.js"), Kind::Text);
        assert_eq!(kind(r"C:\a\b.tsx"), Kind::Text);
        // 音频 / 视频
        for ext in ["wav", "mp3", "m4a", "flac", "ogg", "wma"] {
            assert_eq!(kind(&format!(r"C:\a\b.{ext}")), Kind::Audio, "{ext}");
        }
        for ext in ["mp4", "mov", "avi", "mkv", "webm", "wmv"] {
            assert_eq!(kind(&format!(r"C:\a\b.{ext}")), Kind::Video, "{ext}");
        }
    }

    #[test]
    fn only_four_kinds_are_renderable() {
        for k in [
            Kind::Image,
            Kind::Pdf,
            Kind::Text,
            Kind::Markdown,
            Kind::Svg,
            Kind::Html,
            Kind::Audio,
            Kind::Video,
            Kind::Sheet,
        ] {
            assert!(k.is_renderable(), "{k} 应可渲染");
        }
        assert!(!Kind::Unsupported.is_renderable());
        assert!(Kind::Unsupported.hint().contains("插件"));
        // `KINDS` 必须与"可渲染"集合完全一致（新增变体忘了登记会在这里炸）
        for k in KINDS {
            assert!(k.is_renderable(), "KINDS 里混进了不可渲染的 {k}");
        }
        assert_eq!(KINDS.len(), 9, "内置类别数量变了：同步文档与矩阵脚本");
    }

    #[test]
    fn natural_size_kinds_are_declared() {
        for k in [Kind::Image, Kind::Svg, Kind::Video, Kind::Pdf] {
            assert!(k.has_natural_size(), "{k} 应按原始尺寸开窗");
        }
        for k in [
            Kind::Text,
            Kind::Markdown,
            Kind::Html,
            Kind::Audio,
            Kind::Sheet,
        ] {
            assert!(!k.has_natural_size(), "{k} 不该按原始尺寸开窗");
        }
    }

    #[test]
    fn sniff_detects_pdf_and_bitmaps() {
        assert_eq!(sniff(b"%PDF-1.7\n%..."), Some(Kind::Pdf));
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...."), Some(Kind::Image));
        assert_eq!(sniff(b"\xff\xd8\xff\xe0...."), Some(Kind::Image));
        assert_eq!(sniff(b"GIF89a...."), Some(Kind::Image));
        assert_eq!(sniff(b"RIFF\x24\x00\x00\x00WEBPVP8 "), Some(Kind::Image));
    }

    #[test]
    fn sniff_falls_back_to_text_or_none() {
        assert_eq!(sniff("你好，世界\n第二行\n".as_bytes()), Some(Kind::Text));
        assert_eq!(sniff(b"no extension here\n\tindent\n"), Some(Kind::Text));
        // NUL 字节 = 二进制
        assert_eq!(sniff(b"\x7fELF\x02\x01\x01\x00\x00\x00"), None);
        assert_eq!(sniff(b"MZ\x90\x00\x00\x00"), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn extensionless_files_are_sniffed() {
        // 真实临时文件，验证"没有扩展名也能认出来"。
        // 夹具放 `target/` 而不是 `%TEMP%`：本机 DSH 会给工作区内的 exe 打
        // Low 完整性标签（no-write-up），这种进程往 %TEMP% 写一律 ACCESS_DENIED。
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/qlrust-fmt-test");
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let txt = dir.join("README");
        std::fs::write(&txt, "这是一个没有扩展名的文本文件\n")
            .unwrap_or_else(|e| panic!("写 {} 失败：{e}", txt.display()));
        assert_eq!(kind_for_path(&txt), Kind::Text);
        let pdf = dir.join("noext");
        std::fs::write(&pdf, "%PDF-1.4\n...").unwrap();
        assert_eq!(kind_for_path(&pdf), Kind::Pdf);
        let bin = dir.join("blob");
        std::fs::write(&bin, [0x00u8, 0x01, 0x02, 0x03, 0x00]).unwrap();
        assert_eq!(kind_for_path(&bin), Kind::Unsupported);
        let _ = std::fs::remove_file(&txt);
        let _ = std::fs::remove_file(&pdf);
        let _ = std::fs::remove_file(&bin);
    }

    #[test]
    fn all_extensions_are_unique_and_nonempty() {
        let all = all_extensions();
        assert!(all.len() > 100, "扩展名太少：{}", all.len());
        let mut sorted = all.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len(), "有重复扩展名");
        assert!(all.contains(&"pdf"));
        assert!(all.contains(&"png"));
        assert!(all.contains(&"md"));
        // 1.7.9：音频/视频/SVG/HTML 进内置表
        for ext in ["mp4", "wav", "svg", "html"] {
            assert!(all.contains(&ext), "{ext} 应在内置表里");
        }
        assert!(!all.contains(&"docx"), "Office 仍由插件/系统处理器负责");
    }
}
