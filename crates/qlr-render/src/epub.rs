//! EPUB 内置预览（批 B #6，2026-11-25）。
//!
//! EPUB 就是一个 zip：`META-INF/container.xml` 指向 OPF（`package.opf`），OPF 的
//! `manifest` 给出 id→href、`spine` 给出阅读顺序。这里**不做版式还原**，只按 spine 顺序
//! 把每个 XHTML 章节抽成文本行，复用 `html::layout` 的排版（标题/段落/列表/表格都有），
//! 交同一套 `Canvas::Text` 上屏——"能顺着读"即可，符合零依赖路线（ADR-0004）。
//!
//! 路由上归到 `Kind::Html`（同为"文档排版"族）：壳层不需要为 EPUB 加任何绘制特判，
//! `HtmlViewer` 按扩展名分发到本模块。代价是顶栏标签显示"HTML"，可接受。

use std::fs::File;
use std::io::Read;
use std::path::Path;

use qlr_core::{Error, Result};

use crate::canvas::{Canvas, TextDoc};

/// 一次预览最多拼多少行（防超长书把内存/排版拖崩）。
const MAX_TOTAL_LINES: usize = 20_000;
/// 单个 zip part 解压上限（EPUB 单章正常几 KB–几 MB，留足余量）。
const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;

/// 该路径是不是 `.epub`。
pub fn is_epub(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("epub")
    )
}

/// 渲染 EPUB：按 spine 顺序拼各章文本。
pub fn render(path: &Path, head: bool) -> Result<Canvas> {
    let file = File::open(path).map_err(|e| Error::Message(format!("打不开 EPUB：{e}")))?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|e| Error::Message(format!("不是有效的 EPUB：{e}")))?;

    // 1) container.xml → OPF 路径
    let container = read_entry(&mut zip, "META-INF/container.xml")?;
    let opf_path = find_tag_attr(&container, "rootfile", "full-path")
        .ok_or_else(|| Error::Message("EPUB 缺少 container.xml 的 rootfile".to_string()))?;
    let opf_dir = dirname(&opf_path);

    // 2) OPF → manifest(id→href) + spine(idref 顺序)
    let opf = read_entry(&mut zip, &opf_path)?;
    let mut manifest: Vec<(String, String)> = Vec::new();
    for tag in find_tags(&opf, "item") {
        if let (Some(id), Some(href)) = (attr(tag, "id"), attr(tag, "href")) {
            let lower = href.to_ascii_lowercase();
            if lower.ends_with(".xhtml") || lower.ends_with(".html") || lower.ends_with(".htm") {
                manifest.push((id, href));
            }
        }
    }
    let mut spine: Vec<String> = Vec::new();
    for tag in find_tags(&opf, "itemref") {
        if let Some(idref) = attr(tag, "idref") {
            spine.push(idref);
        }
    }
    if spine.is_empty() {
        return Err(Error::Message("EPUB 的 spine 是空的".to_string()));
    }

    // 3) 逐章排版
    let mut lines: Vec<String> = Vec::new();
    let mut styles: Vec<crate::canvas::LineStyle> = Vec::new();
    for idref in &spine {
        let Some((_, href)) = manifest.iter().find(|(id, _)| id == idref) else {
            continue;
        };
        let full = join_path(&opf_dir, href);
        let Ok(content) = read_entry(&mut zip, &full) else {
            // 单章读不出来就跳过，不要整本失败。
            continue;
        };
        let (mut l, mut s) = crate::html::layout(&content);
        if !lines.is_empty() {
            lines.push(String::new());
            styles.push(crate::canvas::LineStyle::Normal);
        }
        lines.append(&mut l);
        styles.append(&mut s);
        if lines.len() >= MAX_TOTAL_LINES {
            break;
        }
    }
    if lines.is_empty() {
        return Err(Error::Message("EPUB 没有可显示的文字".to_string()));
    }

    let truncated = lines.len() > MAX_TOTAL_LINES;
    if truncated {
        if head {
            lines.truncate(MAX_TOTAL_LINES);
            styles.truncate(MAX_TOTAL_LINES);
        } else {
            let start = lines.len() - MAX_TOTAL_LINES;
            lines.drain(..start);
            styles.drain(..start);
        }
    }
    let total_lines = lines.len();
    Ok(Canvas::Text(TextDoc {
        lines,
        styles: Some(styles),
        spans: None,
        runs: None,
        boxes: None,
        total_lines,
        truncated,
        encoding: "utf-8",
    }))
}

/// 读一个 zip part 成字符串（有单 part 大小上限，防 zip 炸弹）。
fn read_entry(zip: &mut zip::ZipArchive<File>, name: &str) -> Result<String> {
    let mut f = zip
        .by_name(name)
        .map_err(|e| Error::Message(format!("EPUB 缺少 {name}：{e}")))?;
    if f.size() > MAX_ENTRY_BYTES {
        return Err(Error::Message(format!("EPUB 里 {name} 过大")));
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)
        .map_err(|e| Error::Message(format!("读 EPUB 的 {name} 失败：{e}")))?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// 收集形如 `<tag ...>` 的起始标签（跳过 `<tagXxx` 这样的更长标签名）。
fn find_tags<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let needle = format!("<{tag}");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = xml[from..].find(&needle) {
        let s = from + rel;
        let after = &xml[s + needle.len()..];
        let boundary = matches!(
            after.chars().next(),
            Some(' ') | Some('\t') | Some('\n') | Some('\r') | Some('>') | Some('/')
        );
        if boundary {
            if let Some(end) = after.find('>') {
                out.push(&xml[s..s + needle.len() + end + 1]);
                from = s + needle.len() + end + 1;
                continue;
            }
        }
        from = s + needle.len();
    }
    out
}

/// 在全部 `<tag>` 里找第一个带 `name="..."` 的属性值。
fn find_tag_attr(xml: &str, tag: &str, name: &str) -> Option<String> {
    find_tags(xml, tag).into_iter().find_map(|t| attr(t, name))
}

/// 从单个起始标签串里取 `name="value"`（也接受单引号）。做简单实体反转义。
fn attr(tag: &str, name: &str) -> Option<String> {
    let mut from = 0;
    while let Some(rel) = tag[from..].find(name) {
        let start = from + rel;
        let before_ok = start == 0 || tag.as_bytes()[start - 1].is_ascii_whitespace();
        let after = &tag[start + name.len()..];
        if before_ok {
            if let Some(rest) = after.trim_start().strip_prefix('=') {
                let rest = rest.trim_start();
                if let Some(q) = rest.chars().next() {
                    if q == '"' || q == '\'' {
                        if let Some(end) = rest[1..].find(q) {
                            return Some(decode_entities(&rest[1..1 + end]));
                        }
                    }
                }
            }
        }
        from = start + name.len();
        if from >= tag.len() {
            break;
        }
    }
    None
}

/// 最常见的几个 XML 实体（够用于路径/文本）。
fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("%20", " ")
}

fn dirname(p: &str) -> String {
    match p.rfind('/') {
        Some(i) => p[..=i].to_string(),
        None => String::new(),
    }
}

/// OPF 目录 + 相对 href → zip 内绝对路径（处理 `..`、`./`、前导 `/`）。
fn join_path(dir: &str, href: &str) -> String {
    let href = href.replace("%20", " ");
    let joined = if let Some(stripped) = href.strip_prefix('/') {
        stripped.to_string()
    } else {
        format!("{dir}{href}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn write_epub(path: &Path) {
        let file = File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opt = SimpleFileOptions::default();
        zip.start_file("mimetype", opt).unwrap();
        zip.write_all(b"application/epub+zip").unwrap();
        zip.start_file("META-INF/container.xml", opt).unwrap();
        zip.write_all(
            br#"<?xml version="1.0"?>
<container><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
        )
        .unwrap();
        zip.start_file("OEBPS/content.opf", opt).unwrap();
        zip.write_all(
            br#"<?xml version="1.0"?>
<package><manifest>
<item id="c1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
<item id="c2" href="texts/ch2.xhtml" media-type="application/xhtml+xml"/>
<item id="css" href="s.css" media-type="text/css"/>
</manifest>
<spine><itemref idref="c1"/><itemref idref="c2"/></spine></package>"#,
        )
        .unwrap();
        zip.start_file("OEBPS/ch1.xhtml", opt).unwrap();
        zip.write_all("<html><body><h1>第一章</h1><p>你好，世界。</p></body></html>".as_bytes())
            .unwrap();
        zip.start_file("OEBPS/texts/ch2.xhtml", opt).unwrap();
        zip.write_all("<html><body><p>第二章的内容。</p></body></html>".as_bytes())
            .unwrap();
        zip.finish().unwrap();
    }

    #[test]
    fn parses_minimal_epub_in_spine_order() {
        let path = crate::testutil::dir("epub").join("mini.epub");
        write_epub(&path);
        let canvas = render(&path, true).expect("应能解析");
        let Canvas::Text(doc) = canvas else {
            panic!("EPUB 应产出 Text");
        };
        let all = doc.lines.join("\n");
        assert!(all.contains("第一章"), "{all}");
        assert!(all.contains("你好，世界"), "{all}");
        assert!(all.contains("第二章的内容"), "{all}");
        // spine 顺序：第一章在第二章之前
        let p1 = all.find("第一章").unwrap();
        let p2 = all.find("第二章").unwrap();
        assert!(p1 < p2, "章节顺序应遵守 spine");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn join_path_normalizes_relative_segments() {
        assert_eq!(join_path("OEBPS/", "ch1.xhtml"), "OEBPS/ch1.xhtml");
        assert_eq!(
            join_path("OEBPS/", "texts/ch2.xhtml"),
            "OEBPS/texts/ch2.xhtml"
        );
        assert_eq!(join_path("OEBPS/", "../a/b.xhtml"), "a/b.xhtml");
        assert_eq!(join_path("OEBPS/", "/root/x.xhtml"), "root/x.xhtml");
    }
}
