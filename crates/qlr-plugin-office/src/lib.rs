//! QLRust 插件：Office（docx/xlsx/pptx/odt/ods/odp）与压缩包（zip/cbz/jar…）的轻量预览。
//!
//! 这个插件的意义不只是"多看几种格式"——它是**插件 ABI 的第一个真实用例**：
//! 演示一个第三方能力可以完全不改宿主就接进来（宿主只认三个导出符号）。
//!
//! # 两条路线
//!
//! 1. **缩略图优先**：Office 文件里几乎都带 `docProps/thumbnail.jpeg`（另存时由 Office 写入），
//!    这是作者自己认可的"封面"，比我们猜的任何渲染都准，也最省时间。
//! 2. **文本大纲兜底**：没有缩略图就抽正文里的文字，画一张白底卡片
//!    （标题 = 文件名，正文 = 前若干行）。够用来"确认这是哪个文件"，而不是真的排版。
//!
//! # 为什么不做真正的排版
//!
//! 真渲染 Office 需要一个排版引擎（LibreOffice 那一套几百 MB），与我们"6 个文件 9 MB"的
//! 体积目标冲突。这里刻意只做"看得到内容"的最低限度，其余交给"在其他应用打开"按钮。

use std::io::{Read, Seek};
use std::path::Path;

use qlr_plugin_api::{
    ABI_VERSION, MAX_PIXELS, QlrImage, QlrPluginInfo, QlrPluginRenderFn, QlrPluginTextFn,
    QlrPluginTextFreeFn, RENDER_FAILED, RENDER_OK, RENDER_TOO_BIG,
};

const NAME: &str = "Office / 压缩包（轻量预览）";

/// 接管的扩展名。Office 的 OOXML 与 ODF 都是 zip，压缩包本身也走同一套解析。
const EXTENSIONS: &str = "docx;docm;dotx;dotm;xlsx;xlsm;xltx;xltm;pptx;pptm;potx;potm;\
odt;ods;odp;zip;cbz;jar;war;vsdx";

/// 缩略图的最长边上限：再大也是缩小显示，解全图纯浪费。
const THUMB_MAX_SIDE: u32 = 1600;

/// 文本大纲最多取多少行（卡片里也放不下更多）。
const MAX_LINES: usize = 40;

/// 单行最多多少字符（防止一行 2 MB 的压缩 JSON 把卡片撑爆）。
const MAX_LINE_CHARS: usize = 200;

// ---------------------------------------------------------------- 插件入口

/// # Safety
///
/// 无参数、无副作用，宿主在插件加载后调用即可；可安全重复调用。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qlr_plugin_info() -> QlrPluginInfo {
    QlrPluginInfo {
        abi_version: ABI_VERSION,
        name: NAME.as_ptr(),
        name_len: NAME.len() as u32,
        extensions: EXTENSIONS.as_ptr(),
        extensions_len: EXTENSIONS.len() as u32,
    }
}

/// 文本大纲（1.6.0）：宿主优先用它来渲染，读起来是等宽文本、可滚动，比位图卡片清晰。
///
/// 压缩包输出"目录树 + 大小"，Office 文档输出"逐页文字大纲"。
///
/// # Safety
///
/// - `path` 必须指向 `path_len` 字节的有效 UTF-8 路径。
/// - `out_ptr` / `out_len` 必须可写；成功后 `*out_ptr` 指向插件分配的 UTF-8 缓冲，
///   调用方拷走后必须用 [`qlr_plugin_text_free`] 释放。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qlr_plugin_text(
    path: *const u8,
    path_len: usize,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> i32 {
    if out_ptr.is_null() || out_len.is_null() || path.is_null() || path_len == 0 {
        return RENDER_FAILED;
    }
    let bytes = unsafe { std::slice::from_raw_parts(path, path_len) };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return RENDER_FAILED;
    };
    let path = Path::new(text);
    let Some(lines) = text_outline(path) else {
        return RENDER_FAILED;
    };
    let joined = lines.join("\r\n");
    let mut buffer = joined.into_bytes();
    buffer.shrink_to_fit();
    let len = buffer.len();
    let raw = buffer.as_mut_ptr();
    std::mem::forget(buffer);
    unsafe {
        *out_ptr = raw;
        *out_len = len;
    }
    RENDER_OK
}

/// # Safety
///
/// `ptr` 必须是 [`qlr_plugin_text`] 返回的指针，`len` 必须是它返回的长度；只能释放一次。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qlr_plugin_text_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    unsafe {
        drop(Vec::from_raw_parts(ptr, len, len));
    }
}

const _: QlrPluginTextFn = qlr_plugin_text;
const _: QlrPluginTextFreeFn = qlr_plugin_text_free;
/// # Safety
///
/// - `path` 必须指向 `path_len` 字节的有效 UTF-8 路径，且在本次调用期间保持有效。
/// - `out` 必须指向可写的 [`QlrImage`]；成功时填进去的像素由 [`qlr_plugin_free`] 释放。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qlr_plugin_render(
    path: *const u8,
    path_len: usize,
    max_width: u32,
    max_height: u32,
    out: *mut QlrImage,
) -> i32 {
    if out.is_null() || path.is_null() || path_len == 0 {
        return RENDER_FAILED;
    }
    let bytes = unsafe { std::slice::from_raw_parts(path, path_len) };
    let path: &Path = match std::str::from_utf8(bytes) {
        Ok(text) => Path::new(text),
        Err(_) => return RENDER_FAILED,
    };
    let max_w = max_width.clamp(64, 8192);
    let max_h = max_height.clamp(64, 8192);
    if u64::from(max_w) * u64::from(max_h) > u64::from(MAX_PIXELS) {
        return RENDER_TOO_BIG;
    }

    match render(path, max_w, max_h) {
        Some((w, h, rgba)) => {
            // 交给宿主的堆块：宿主只读，最后由 `qlr_plugin_free` 归还。
            let mut boxed = rgba.into_boxed_slice();
            let ptr = boxed.as_mut_ptr();
            let len = boxed.len();
            std::mem::forget(boxed);
            unsafe {
                *out = QlrImage {
                    width: w,
                    height: h,
                    pixels: ptr,
                    len,
                };
            }
            RENDER_OK
        }
        None => RENDER_FAILED,
    }
}

/// # Safety
///
/// image 必须是 [qlr_plugin_render] 填过、且**只释放一次**的容器（也允许传空指针）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qlr_plugin_free(image: *mut QlrImage) {
    if image.is_null() {
        return;
    }
    let image = unsafe { &mut *image };
    if !image.pixels.is_null() && image.len > 0 {
        // 用当初那个 slice 的容量重建 Box，再正常析构（长度必须原样，不能猜）。
        let slice = std::ptr::slice_from_raw_parts_mut(image.pixels, image.len);
        unsafe { drop(Box::from_raw(slice)) };
    }
    image.pixels = core::ptr::null_mut();
    image.len = 0;
    image.width = 0;
    image.height = 0;
}

// 钉住签名：编译期就能发现导出函数和 ABI 定义不一致。
const _: QlrPluginRenderFn = qlr_plugin_render;

// ---------------------------------------------------------------- 渲染主流程

/// 渲染：缩略图优先，没有就画文本大纲卡片。
fn render(path: &Path, max_w: u32, max_h: u32) -> Option<(u32, u32, Vec<u8>)> {
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;

    if let Some(image) = thumbnail(&mut archive, max_w, max_h) {
        return Some(image);
    }
    let lines = outline(path, &mut archive);
    Some(draw_card(path, &lines, max_w, max_h))
}

/// 找文件里自带的缩略图（Office 会写 `docProps/thumbnail.jpeg`，ODF 写 `Thumbnails/thumbnail.png`）。
fn thumbnail<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    max_w: u32,
    max_h: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    const CANDIDATES: [&str; 4] = [
        "docProps/thumbnail.jpeg",
        "docProps/thumbnail.jpg",
        "docProps/thumbnail.png",
        "Thumbnails/thumbnail.png",
    ];
    let name = CANDIDATES
        .iter()
        .find(|name| archive.by_name(name).is_ok())?;
    // `by_name` 上一次只是为了探测存在性；这里重新取一次拿到可读流。
    let mut entry = archive.by_name(name).ok()?;
    let mut bytes = Vec::new();
    // 缩略图不该超过几 MB；读的时候顺手设个上限，免得被畸形包喂爆内存。
    let budget = 16 * 1024 * 1024usize;
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let read = entry.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        if bytes.len() + read > budget {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    if bytes.is_empty() {
        return None;
    }

    let image = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let (src_w, src_h) = image.dimensions();
    if src_w == 0 || src_h == 0 {
        return None;
    }
    // 缩略图本身的尺寸就很小，直接按"适应窗口"缩一次即可。
    let scale = (max_w as f32 / src_w as f32)
        .min(max_h as f32 / src_h as f32)
        .min((THUMB_MAX_SIDE as f32) / src_w.max(src_h) as f32)
        .min(1.0);
    let (dst_w, dst_h) = if scale >= 1.0 {
        (src_w, src_h)
    } else {
        (
            ((src_w as f32 * scale) as u32).max(1),
            ((src_h as f32 * scale) as u32).max(1),
        )
    };
    let resized = if (dst_w, dst_h) == (src_w, src_h) {
        image
    } else {
        image::imageops::resize(&image, dst_w, dst_h, image::imageops::FilterType::Triangle)
    };
    Some((dst_w, dst_h, resized.into_raw()))
}

/// 文本大纲的公开入口（1.6.0 的 `qlr_plugin_text` 用它）。
///
/// 压缩包 → 目录树 + 统计行；Office → 逐页/逐表文字大纲。
fn text_outline(path: &Path) -> Option<Vec<String>> {
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let kind = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(kind.as_str(), "zip" | "cbz" | "jar" | "war") {
        let items = entry_list(&mut archive);
        let mut lines = Vec::new();
        lines.push(format!("{}", path.file_name()?.to_string_lossy()));
        lines.push(String::new());
        lines.extend(items);
        Some(lines)
    } else {
        Some(outline(path, &mut archive))
    }
}

/// 抽正文文字：按扩展名选解析入口，失败就给个只有文件名的卡片。
fn outline<R: Read + Seek>(path: &Path, archive: &mut zip::ZipArchive<R>) -> Vec<String> {
    let kind = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    let mut lines = match kind.as_str() {
        // Word：正文段落
        "docx" | "docm" | "dotx" | "dotm" => {
            read_text_entry(archive, "word/document.xml", "<w:t", "</w:t>")
                .map(|text| paragraphs(&text))
                .unwrap_or_default()
        }
        // Excel：工作表名 + 单元格文字
        //
        // 注意两种存储方式都要管：Excel 自己存 `xl/sharedStrings.xml`（共享字符串表），
        // 而 openpyxl 等库默认写**行内字符串**（`<c t="inlineStr"><is><t>…</t></is></c>`，
        // 根本没有 sharedStrings.xml）——只读前者会看到一张空表（踩过）。
        "xlsx" | "xlsm" | "xltx" | "xltm" => {
            let mut out = Vec::new();
            if let Some(workbook) = read_raw_entry(archive, "xl/workbook.xml") {
                let names = sheet_names(&workbook);
                if !names.is_empty() {
                    out.push(format!("工作表：{}", names.join("、")));
                }
            }
            // 共享字符串表（Excel 自己写的文件才有；取出来给下面的单元格按索引查）
            let shared: Vec<String> = read_raw_entry(archive, "xl/sharedStrings.xml")
                .map(|xml| shared_strings(&xml))
                .unwrap_or_default();
            for name in worksheet_names(archive) {
                if let Some(xml) = read_raw_entry(archive, &name) {
                    out.extend(sheet_rows(&xml, &shared));
                }
            }
            out
        }
        // PowerPoint：逐页取文字（页与页之间用分隔线）
        "pptx" | "pptm" | "potx" | "potm" => {
            let mut out = Vec::new();
            let names = slide_names(archive);
            for (index, name) in names.iter().enumerate() {
                if index > 0 {
                    out.push("──".to_string());
                }
                out.push(format!("第 {} 页", index + 1));
                if let Some(text) = read_text_entry(archive, name, "<a:t", "</a:t>") {
                    out.extend(paragraphs(&text));
                }
            }
            out
        }
        // OpenDocument：content.xml 里段落文字
        "odt" | "ods" | "odp" => read_text_entry(archive, "content.xml", "<text:", "</text:")
            .map(|text| paragraphs(&text))
            .unwrap_or_default(),
        // 压缩包：条目清单（名字 + 大小）
        _ => entry_list(archive),
    };

    if lines.is_empty() {
        lines.push("（这个文件里没有可显示的文本，也找不到缩略图）".to_string());
    }
    lines.truncate(MAX_LINES);
    lines
}

/// 读一个 zip 条目的整段文字（不解压超过 8 MB，防畸形包）。找不到就 `None`。
fn read_raw_entry<R: Read + Seek>(archive: &mut zip::ZipArchive<R>, name: &str) -> Option<String> {
    let mut entry = archive.by_name(name).ok()?;
    let mut raw = Vec::new();
    entry
        .by_ref()
        .take(8 * 1024 * 1024)
        .read_to_end(&mut raw)
        .ok()?;
    Some(String::from_utf8_lossy(&raw).into_owned())
}

/// 读一个 zip 条目里**标记之间**的文字（其余标签全部丢掉）。
fn read_text_entry<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
    open_marker: &str,
    close_marker: &str,
) -> Option<String> {
    let text = read_raw_entry(archive, name)?;
    // 不为了几行大纲去引一个 XML 解析器：直接按标记抓。
    Some(extract_between(&text, open_marker, close_marker))
}

/// 共享字符串表里的条目（`<si><t>文字</t></si>`）。
fn shared_strings(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in xml.split("<si>").skip(1) {
        out.push(paragraphs(&extract_between(part, "<t", "</t>")).join(" "));
    }
    out
}

/// 一张工作表里的前若干行，每行把单元格用 `│` 连起来。
///
/// 单元格有三种存法，都得管：
/// - `t="s"` → `<v>` 是共享字符串表的下标；
/// - `t="inlineStr"` → `<is><t>文字</t></is>`；
/// - 其余 → `<v>` 就是数字（日期是序列号，这里如实显示）。
fn sheet_rows(xml: &str, shared: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for row in xml.split("</row>") {
        let mut cells = Vec::new();
        // 注意分隔符是 `<c`（不带空格）：`<c r="A1">` 有属性、`<c>` 没有，
        // 只匹配带空格的写法会漏掉整列没有属性的单元格（踩过：数字全丢）。
        for cell in row.split("<c").skip(1) {
            let head = cell.chars().next();
            if !(head == Some('>') || head.is_some_and(char::is_whitespace)) {
                continue; // `<cols>`、`<c t=...>` 之外的同前缀标签
            }
            let Some(gt) = cell.find('>') else { continue };
            let attrs = &cell[..gt];
            let body = &cell[gt + 1..];
            let inline = attrs.contains("t=\"inlineStr\"") || attrs.contains("t='inlineStr'");
            let is_shared = attrs.contains("t=\"s\"") || attrs.contains("t='s'");
            let value = if inline {
                paragraphs(&extract_between(body, "<t", "</t>")).join(" ")
            } else if is_shared {
                let index: usize = paragraphs(&extract_between(body, "<v", "</v>"))
                    .first()
                    .and_then(|text| text.parse().ok())
                    .unwrap_or(usize::MAX);
                shared.get(index).cloned().unwrap_or_default()
            } else {
                paragraphs(&extract_between(body, "<v", "</v>")).join(" ")
            };
            if !value.is_empty() {
                cells.push(value);
            }
        }
        if !cells.is_empty() {
            out.push(clip(&cells.join(" │ ")));
        }
        if out.len() >= MAX_LINES {
            break;
        }
    }
    out
}

/// xlsx 的各个工作表条目名，按 `sheet1.xml`、`sheet2.xml`… 的数字顺序排。
fn worksheet_names<R: Read + Seek>(archive: &mut zip::ZipArchive<R>) -> Vec<String> {
    let mut found: Vec<(u32, String)> = Vec::new();
    for index in 0..archive.len() {
        let Ok(entry) = archive.by_index(index) else {
            continue;
        };
        let name = entry.name().to_string();
        let Some(rest) = name.strip_prefix("xl/worksheets/sheet") else {
            continue;
        };
        let Some(number) = rest
            .strip_suffix(".xml")
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        found.push((number, name));
    }
    found.sort_by_key(|(number, _)| *number);
    found.into_iter().map(|(_, name)| name).collect()
}

/// 把 `<w:t>文字</w:t>` 这类结构里的文字抽出来，段落之间用 `\n` 分开。
fn extract_between(xml: &str, open: &str, close: &str) -> String {
    let mut out = String::new();
    let mut rest = xml;
    while let Some(start) = rest.find(open) {
        rest = &rest[start + open.len()..];
        // `<w:t xml:space="preserve">` 这种要跳过标签自己的属性。
        if let Some(gt) = rest.find('>') {
            rest = &rest[gt + 1..];
        }
        let Some(end) = rest.find(close) else {
            break;
        };
        out.push_str(&unescape(&rest[..end]));
        out.push('\n');
        rest = &rest[end + close.len()..];
    }
    out
}

/// 把抽出来的文字按行整理：去空白行、截断超长行。
fn paragraphs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        out.push(clip(line));
    }
    out
}

fn clip(line: &str) -> String {
    let cleaned: String = line.chars().take(MAX_LINE_CHARS).collect();
    if line.chars().count() > MAX_LINE_CHARS {
        format!("{cleaned}…")
    } else {
        cleaned
    }
}

/// XML 实体还原（只处理最常见的五个 + 数字实体）。
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        let Some(semi) = rest.find(';').filter(|&semi| semi <= 12) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            other => {
                if let Some(hex) = other
                    .strip_prefix("#x")
                    .or_else(|| other.strip_prefix("#X"))
                {
                    u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                } else {
                    other
                        .strip_prefix('#')
                        .and_then(|dec| dec.parse::<u32>().ok())
                        .and_then(char::from_u32)
                }
            }
        };
        match decoded {
            Some(ch) => {
                out.push(ch);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 从 `xl/workbook.xml` 里取工作表名（`<sheet name="Sheet1" …>`）。
fn sheet_names(xml: &str) -> Vec<String> {
    let mut names = Vec::new();
    for part in xml.split("<sheet ").skip(1) {
        if let Some(name) = part.find("name=\"") {
            let rest = &part[name + 6..];
            if let Some(end) = rest.find('"') {
                names.push(unescape(&rest[..end]));
            }
        }
    }
    names
}

/// pptx 的幻灯片条目名，按页号排序（`ppt/slides/slide1.xml`、`slide2.xml`…）。
fn slide_names<R: Read + Seek>(archive: &mut zip::ZipArchive<R>) -> Vec<String> {
    let mut names: Vec<(u32, String)> = Vec::new();
    for index in 0..archive.len() {
        let Ok(entry) = archive.by_index(index) else {
            continue;
        };
        let name = entry.name().to_string();
        let Some(rest) = name.strip_prefix("ppt/slides/slide") else {
            continue;
        };
        let Some(number) = rest
            .strip_suffix(".xml")
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        names.push((number, name));
    }
    names.sort_by_key(|(number, _)| *number);
    names.into_iter().map(|(_, name)| name).collect()
}

/// 压缩包的**目录树**（1.4.1）：按路径分层，文件夹在前、文件带大小。
///
/// 用户要的是"能看出这个包的结构"，而不是一长串完整路径。
fn entry_list<R: Read + Seek>(archive: &mut zip::ZipArchive<R>) -> Vec<String> {
    let mut items: Vec<(String, u64)> = Vec::new();
    let mut packed = 0u64;
    let mut unpacked = 0u64;
    let mut dir_set: Vec<String> = Vec::new();
    for index in 0..archive.len() {
        let Ok(entry) = archive.by_index(index) else {
            continue;
        };
        if !entry.is_dir() {
            unpacked += entry.size();
            packed += entry.compressed_size();
        }
        let name = entry.name().to_string();
        // 目录数按"路径里出现过的目录"算：很多打包工具（python zipfile 等）
        // 根本不写目录条目，只数 `is_dir()` 会永远是 0（踩过）。
        let parts: Vec<&str> = name.split('/').filter(|part| !part.is_empty()).collect();
        for level in 0..parts.len().saturating_sub(1) {
            let prefix = parts[..=level].join("/");
            if !dir_set.iter().any(|seen| seen == &prefix) {
                dir_set.push(prefix);
            }
        }
        items.push((name, entry.size()));
    }
    let dirs = dir_set.len();
    let files = items
        .iter()
        .filter(|(name, _)| !name.ends_with('/'))
        .count();
    let mut out = vec![format!(
        "{dirs} 个文件夹 · {files} 个文件 · 压缩后 {} / 解压后 {}",
        human_size(packed),
        human_size(unpacked)
    )];
    out.extend(tree_lines(&items));
    out
}

/// 把 `a/b/c.txt` 这样的条目铺成一棵缩进树（纯函数，方便单测）。
fn tree_lines(items: &[(String, u64)]) -> Vec<String> {
    // 先排序：同层里按名字升序，看起来和资源管理器一致。
    let mut sorted: Vec<&(String, u64)> = items.iter().collect();
    sorted.sort_by_key(|(name, _)| name.to_lowercase());

    let mut out: Vec<String> = Vec::new();
    let mut seen_dirs: Vec<String> = Vec::new();
    for (path, size) in sorted {
        let mut parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        if parts.is_empty() {
            continue;
        }
        // 目录条目（以 / 结尾）自己就是最后一段；文件条目最后一段是文件名。
        let name = parts.pop().unwrap_or_default();
        let depth = parts.len();
        // 逐层补出目录行：第一次遇到某层才打印。
        for level in 0..depth {
            let prefix = parts[..=level].join("/");
            if seen_dirs.iter().any(|seen| seen == &prefix) {
                continue;
            }
            seen_dirs.push(prefix);
            out.push(clip(&format!(
                "{}▸ {}/",
                "    ".repeat(level),
                parts[level]
            )));
        }
        // 目录条目本身不再单独出一行（上面已经有 `▸ 名字/`）。
        if path.ends_with('/') {
            continue;
        }
        out.push(clip(&format!(
            "{}· {}    {}",
            "    ".repeat(depth),
            name,
            human_size(*size)
        )));
        if out.len() >= MAX_LINES {
            out.push("…（条目太多，只显示开头这些）".to_string());
            break;
        }
    }
    out
}
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// ---------------------------------------------------------------- 文本大纲卡片

/// 画一张"文件大纲"卡片：白底、深灰字、标题一行 + 若干正文行。
///
/// 用 GDI 直接画到 DIB 里（插件跑在宿主进程内，字体是系统的，不必自带字体文件）。
/// 渲染失败（建 DC/建位图失败）时退回"纯白图"，不会返回 None——
/// 一张白图 + 宿主顶栏里的文件名，也好过"插件渲染失败"。
fn draw_card(path: &Path, lines: &[String], max_w: u32, max_h: u32) -> (u32, u32, Vec<u8>) {
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS, CreateCompatibleDC,
        CreateDIBSection, CreateFontW, CreateSolidBrush, DEFAULT_CHARSET, DEFAULT_PITCH,
        DEFAULT_QUALITY, DIB_RGB_COLORS, DeleteDC, DeleteObject, FW_BOLD, FW_NORMAL, GetDC,
        HGDIOBJ, OUT_TT_PRECIS, ReleaseDC, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
        TextOutW,
    };

    let width = max_w.max(64);
    let height = max_h.max(64);
    let mut rgba = vec![255u8; (width as usize) * (height as usize) * 4];

    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        if mem.0.is_null() {
            ReleaseDC(None, screen);
            return (width, height, rgba);
        }
        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = width as i32;
        // 负高度 = top-down DIB，像素顺序与 RGBA 缓冲一致，省一次翻转。
        info.bmiHeader.biHeight = -(height as i32);
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB.0;
        let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
        let Ok(bitmap) = CreateDIBSection(Some(mem), &info, DIB_RGB_COLORS, &mut bits, None, 0)
        else {
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
            return (width, height, rgba);
        };
        if bits.is_null() {
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
            return (width, height, rgba);
        }
        let old = SelectObject(mem, bitmap.into());
        // 白底：整块填白（BRUSH 一次覆盖比逐像素快得多）。
        let brush = CreateSolidBrush(windows::Win32::Foundation::COLORREF(0x00FF_FFFF));
        let client = windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        windows::Win32::Graphics::Gdi::FillRect(mem, &client, brush);
        let _ = DeleteObject(brush.into());

        SetBkMode(mem, TRANSPARENT);
        let face = "Microsoft YaHei";

        // 标题：文件名
        let title: Vec<u16> = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned())
            .encode_utf16()
            .collect();
        let font_title = CreateFontW(
            -22,
            0,
            0,
            0,
            FW_BOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_TT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            DEFAULT_QUALITY,
            DEFAULT_PITCH.0 as u32,
            windows::core::PCWSTR(face.encode_utf16().collect::<Vec<u16>>().as_ptr()),
        );
        let font_body = CreateFontW(
            -16,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_TT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            DEFAULT_QUALITY,
            DEFAULT_PITCH.0 as u32,
            windows::core::PCWSTR(face.encode_utf16().collect::<Vec<u16>>().as_ptr()),
        );

        let mut y = 24i32;
        if !font_title.0.is_null() {
            SelectObject(mem, HGDIOBJ(font_title.0));
            SetTextColor(mem, windows::Win32::Foundation::COLORREF(0x0020_2020));
            let _ = TextOutW(mem, 24, y, &title);
            y += 34;
        }
        if !font_body.0.is_null() {
            SelectObject(mem, HGDIOBJ(font_body.0));
            SetTextColor(mem, windows::Win32::Foundation::COLORREF(0x0040_4040));
            let line_height = 22;
            for line in lines {
                if y + line_height > height as i32 - 16 {
                    break;
                }
                let wide: Vec<u16> = line.encode_utf16().collect();
                let _ = TextOutW(mem, 24, y, &wide);
                y += line_height;
            }
        }

        // 从 DIB 抄出来：BGRA → RGBA，alpha 一律 255（卡片本来就不透明）。
        let pixels =
            std::slice::from_raw_parts(bits as *const u8, (width as usize) * (height as usize) * 4);
        for (dst, src) in rgba.chunks_exact_mut(4).zip(pixels.chunks_exact(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = 255;
        }

        let _ = SelectObject(mem, old);
        let _ = DeleteObject(bitmap.into());
        if !font_title.0.is_null() {
            let _ = DeleteObject(font_title.into());
        }
        if !font_body.0.is_null() {
            let _ = DeleteObject(font_body.into());
        }
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
    }

    (width, height, rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_reports_abi_and_office_extensions() {
        let info = unsafe { qlr_plugin_info() };
        assert_eq!(info.abi_version, ABI_VERSION);
        let name = unsafe { core::slice::from_raw_parts(info.name, info.name_len as usize) };
        assert!(!name.is_empty());
        let exts =
            unsafe { core::slice::from_raw_parts(info.extensions, info.extensions_len as usize) };
        let exts = core::str::from_utf8(exts).expect("扩展名必须是 UTF-8");
        let parsed = qlr_plugin_api::parse_extensions(exts);
        for expected in ["docx", "xlsx", "pptx", "zip"] {
            assert!(parsed.contains(&expected.to_string()), "缺少 {expected}");
        }
    }

    #[test]
    fn unescape_handles_entities_and_numbers() {
        assert_eq!(unescape("a &amp; b &lt;c&gt;"), "a & b <c>");
        assert_eq!(unescape("&#65;&#x42;"), "AB");
        // 不认识的实体保持原样，别把用户的文字吃掉。
        assert_eq!(unescape("100% &unknown; ok"), "100% &unknown; ok");
        assert_eq!(unescape(" lone & ampersand"), " lone & ampersand");
    }

    #[test]
    fn extract_between_takes_text_and_skips_attributes() {
        let xml = r#"<w:p><w:t>第一行</w:t><w:t xml:space="preserve"> 第二行 </w:t></w:p>"#;
        let text = extract_between(xml, "<w:t", "</w:t>");
        let lines = paragraphs(&text);
        assert_eq!(lines, vec!["第一行".to_string(), "第二行".to_string()]);
    }

    #[test]
    fn sheet_names_are_read_from_workbook_xml() {
        let xml =
            r#"<sheets><sheet name="销售" sheetId="1"/><sheet name="库存" sheetId="2"/></sheets>"#;
        assert_eq!(
            sheet_names(xml),
            vec!["销售".to_string(), "库存".to_string()]
        );
    }

    #[test]
    fn long_lines_are_clipped() {
        let long = "x".repeat(MAX_LINE_CHARS + 50);
        let clipped = clip(&long);
        assert_eq!(clipped.chars().count(), MAX_LINE_CHARS + 1);
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn human_size_switches_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn shared_strings_and_cells_are_extracted() {
        let shared = shared_strings("<sst><si><t>姓名</t></si><si><t>年龄</t></si></sst>");
        assert_eq!(shared, vec!["姓名".to_string(), "年龄".to_string()]);
        let sheet = "<row><c t=\"s\"><v>0</v></c><c t=\"s\"><v>1</v></c></row>\
                     <row><c><v>42</v></c></row>\
                     <row><c t=\"inlineStr\"><is><t>行内</t></is></c></row>";
        let rows = sheet_rows(sheet, &shared);
        assert_eq!(rows[0], "姓名 │ 年龄");
        assert_eq!(rows[1], "42");
        assert_eq!(rows[2], "行内");
    }

    #[test]
    fn tree_lines_nest_by_directory() {
        let items = vec![
            ("a/b/c.txt".to_string(), 10u64),
            ("a/b/d.txt".to_string(), 20),
            ("a/e.txt".to_string(), 30),
            ("top.txt".to_string(), 40),
        ];
        let lines = tree_lines(&items);
        assert_eq!(lines[0], "▸ a/");
        assert_eq!(lines[1], "    ▸ b/");
        assert_eq!(lines[2], "        · c.txt    10 B");
        assert_eq!(lines[3], "        · d.txt    20 B");
        assert_eq!(lines[4], "    · e.txt    30 B");
        assert_eq!(lines[5], "· top.txt    40 B");
    }

    #[test]
    fn null_out_is_rejected() {
        assert_eq!(
            unsafe { qlr_plugin_render(b"a.docx".as_ptr(), 6, 100, 100, core::ptr::null_mut()) },
            RENDER_FAILED
        );
    }

    #[test]
    fn missing_file_fails_cleanly() {
        let mut image = QlrImage::empty();
        let code = unsafe {
            qlr_plugin_render(
                b"Z:\\no\\such\\file.docx".as_ptr(),
                20,
                200,
                200,
                &mut image,
            )
        };
        assert_eq!(code, RENDER_FAILED);
        assert!(image.pixels.is_null());
    }

    #[test]
    fn card_is_drawn_white_with_text() {
        let lines = vec!["第一行".to_string(), "第二行".to_string()];
        let (w, h, rgba) = draw_card(Path::new("demo.docx"), &lines, 400, 300);
        assert_eq!((w, h), (400, 300));
        assert_eq!(rgba.len(), (400 * 300 * 4) as usize);
        // 角落必须是白底；正文区域必须出现过非白像素（说明字真画上去了）。
        assert_eq!(&rgba[0..3], &[255, 255, 255]);
        let dark = rgba
            .chunks_exact(4)
            .filter(|px| px[0] < 128 && px[1] < 128 && px[2] < 128)
            .count();
        assert!(dark > 50, "文字像素太少：{dark}");
    }
}
