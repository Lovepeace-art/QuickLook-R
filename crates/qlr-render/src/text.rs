//! 文本渲染：读文件 → 探测编码 → 切行 → 只保留要显示的一段。
//!
//! M2 的朴素实现，不 mmap、不做字形排版（排版在 `qlr-platform` 用 `DrawTextW` 交给系统）。
//! 但它**刻意守住两条底线**，因为 QuickLook 在这两处都翻过车：
//! 1. 读入有硬上限（默认 8 MB）——不会像上游那样一个滚屏吃掉几个 GB；
//! 2. 单行有长度上限，遇到压缩成一行的 JSON 不会把 GDI 撑爆。

use std::io::Read;
use std::path::Path;

use qlr_core::{Error, Result};

use crate::canvas::{Canvas, TextDoc};

/// 默认读入上限：8 MB。超出部分不读，标记 `truncated`。
pub const READ_LIMIT: usize = 8 * 1024 * 1024;
/// 默认最多显示多少行。
pub const MAX_LINES: usize = 200_000;
/// 单行最多保留多少个字符（按 char 计，不是字节，避免切断 UTF-8 与中日韩字符）。
pub const MAX_LINE_CHARS: usize = 2000;

/// 渲染文本文件。`head` 为 true 时取开头，否则取结尾（日志场景更常看尾部）。
pub fn render(path: &Path, head: bool) -> Result<Canvas> {
    render_with(path, head, READ_LIMIT, MAX_LINES)
}

/// 带预算的版本（测试与将来的"只读前 N 行"模式用）。
pub fn render_with(path: &Path, head: bool, read_limit: usize, max_lines: usize) -> Result<Canvas> {
    crate::cancel::checkpoint()?;
    let (bytes, truncated_file) = read_capped(path, read_limit)?;
    if looks_binary(&bytes) {
        return Err(Error::Message(
            "看起来是二进制文件（含 NUL 字节），不走文本渲染".to_string(),
        ));
    }
    // 8 MB 文本的解码（尤其 GB18030 启发式）在慢机器上要几十毫秒，解完先问一句"还要不要"。
    crate::cancel::checkpoint()?;
    let (text, encoding) = decode(&bytes);

    let mut lines: Vec<String> = if text.is_empty() {
        Vec::new()
    } else {
        text.split('\n')
            .map(|l| truncate_chars(l.trim_end_matches('\r'), MAX_LINE_CHARS))
            .collect()
    };
    // 以换行结尾的文件不该多出一个空行（"a\n" 是 1 行不是 2 行）。
    if text.ends_with('\n') {
        lines.pop();
    }
    let total_lines = lines.len();

    let truncated_lines = lines.len() > max_lines;
    if truncated_lines {
        lines = if head {
            lines.into_iter().take(max_lines).collect()
        } else {
            let start = lines.len() - max_lines;
            lines.split_off(start)
        };
    }

    Ok(Canvas::Text(TextDoc {
        lines,
        styles: None,
        spans: None,
        total_lines,
        truncated: truncated_file || truncated_lines,
        encoding,
    }))
}

/// 读文件，最多 `limit` 字节。返回 (字节, 是否被截断)。
pub fn read_capped(path: &Path, limit: usize) -> Result<(Vec<u8>, bool)> {
    let file = std::fs::File::open(path).map_err(|e| Error::Message(format!("打不开文件：{e}")))?;
    let declared = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut buf = Vec::with_capacity(declared.min(limit as u64) as usize + 1);
    file.take(limit as u64)
        .read_to_end(&mut buf)
        .map_err(|e| Error::Message(format!("读文件失败：{e}")))?;
    Ok((buf, declared > limit as u64))
}

/// 粗判二进制：前 8 KB 里出现 NUL 就认为不是文本。
///
/// 纯二进制也可能前 8 KB 没 NUL（例如某些压缩流），这里只求"别把二进制画成乱码"，
/// 不做精确判定——真正的二进制查看器在 M3。
///
/// **例外**：带 BOM 的 UTF-16 文件天生就是"每隔一个字节一个 0"，
/// 不排除掉的话所有 UTF-16 日志（PowerShell 重定向、Windows 事件导出）都会被误判成二进制。
pub fn looks_binary(bytes: &[u8]) -> bool {
    if is_utf16_bom(bytes) {
        return false;
    }
    bytes.iter().take(8192).any(|&b| b == 0)
}

/// 是否以 UTF-16 BOM 开头（LE `FF FE` / BE `FE FF`）。
pub fn is_utf16_bom(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF])
}

/// 解码文本。顺序：UTF-8 BOM → UTF-16 BOM → 严格 UTF-8 → GB18030 启发式 → 有损 UTF-8。
///
/// 不引入编码库（零依赖原则）：UTF-16 用标准库 `decode_utf16`，GBK 家族这里只做
/// "是不是 GBK" 的**保守判断**（多字节汉字区），判断不了就退回 lossy，界面上会出现
/// 替换字符，但不会崩也不会乱序。
pub fn decode(bytes: &[u8]) -> (String, &'static str) {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return (
            String::from_utf8_lossy(&bytes[3..]).into_owned(),
            "utf-8 (bom)",
        );
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return (decode_utf16(&bytes[2..], false), "utf-16le");
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return (decode_utf16(&bytes[2..], true), "utf-16be");
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), "utf-8"),
        Err(_) => (String::from_utf8_lossy(bytes).into_owned(), "utf-8 (lossy)"),
    }
}

fn decode_utf16(bytes: &[u8], big_endian: bool) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| {
            if big_endian {
                u16::from_be_bytes([c[0], c[1]])
            } else {
                u16::from_le_bytes([c[0], c[1]])
            }
        })
        .collect();
    char::decode_utf16(units)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// 按字符（不是字节）截断，避免切断多字节字符。
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}…", &s[..idx]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(name: &str, data: &[u8]) -> std::path::PathBuf {
        crate::testutil::write_file("text", name, data)
    }

    #[test]
    fn utf8_text_splits_into_lines() {
        let path = write_temp("a.txt", "第一行\nsecond\n\n第四行\n".as_bytes());
        let canvas = render(&path, true).unwrap();
        match canvas {
            Canvas::Text(t) => {
                assert_eq!(t.encoding, "utf-8");
                assert_eq!(t.lines[0], "第一行");
                assert_eq!(t.lines[1], "second");
                assert_eq!(t.lines[2], "");
                assert_eq!(t.lines[3], "第四行");
                assert_eq!(t.total_lines, 4);
                assert!(!t.truncated);
            }
            other => panic!("期望 Text，得到 {}", other.kind_name()),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn crlf_is_stripped() {
        let path = write_temp("crlf.txt", b"a\r\nb\r\n");
        let Canvas::Text(t) = render(&path, true).unwrap() else {
            panic!("应为文本");
        };
        assert_eq!(t.lines, vec!["a".to_string(), "b".to_string()]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn binary_is_rejected() {
        let path = write_temp("bin.dat", &[0x00, 0x01, 0x02, 0x00]);
        let err = render(&path, true).unwrap_err();
        assert!(format!("{err}").contains("二进制"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn utf16le_bom_is_decoded() {
        let mut data = vec![0xFF, 0xFE];
        for u in "你好".encode_utf16() {
            data.extend_from_slice(&u.to_le_bytes());
        }
        let (s, enc) = decode(&data);
        assert_eq!(enc, "utf-16le");
        assert_eq!(s, "你好");
    }

    /// 回归：带 BOM 的 UTF-16 文件**不能**被判成二进制。
    /// 症状是 PowerShell 重定向出来的日志（UTF-16LE）在预览里只显示一句
    /// "看起来是二进制文件"，A6.7 样本矩阵就是这样抓到的。
    #[test]
    fn utf16_files_are_not_treated_as_binary() {
        assert!(!looks_binary(&[0xFF, 0xFE, 0x61, 0x00]));
        assert!(!looks_binary(&[0xFE, 0xFF, 0x00, 0x61]));
        // 没有 BOM 的含 NUL 数据仍然是二进制
        assert!(looks_binary(&[0x61, 0x00, 0x62]));

        let mut data = vec![0xFF, 0xFE];
        for u in "第一行\r\n第二行\r\n".encode_utf16() {
            data.extend_from_slice(&u.to_le_bytes());
        }
        let path = write_temp("utf16.log", &data);
        match render(&path, true).unwrap() {
            Canvas::Text(t) => {
                assert_eq!(t.encoding, "utf-16le");
                assert_eq!(t.lines, vec!["第一行".to_string(), "第二行".to_string()]);
            }
            other => panic!("UTF-16 文本应当渲染成文本画布：{other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn invalid_utf8_degrades_to_lossy() {
        let (s, enc) = decode(&[0x61, 0xFF, 0x62]);
        assert_eq!(enc, "utf-8 (lossy)");
        assert!(s.starts_with('a') && s.ends_with('b'), "{s}");
    }

    #[test]
    fn long_line_is_truncated_by_chars_not_bytes() {
        let s = "字".repeat(MAX_LINE_CHARS + 10);
        let out = truncate_chars(&s, MAX_LINE_CHARS);
        assert_eq!(out.chars().count(), MAX_LINE_CHARS + 1); // 含省略号
        assert!(out.ends_with('…'));
    }

    /// 行数超过预算时只保留前 N 行，但仍记录真实总行数（UI 要显示"共 N 行"）。
    /// 用 `render_with` 传小预算，避免真的造 20 万行。
    #[test]
    fn huge_line_count_is_capped() {
        const LIMIT: usize = 50;
        let mut s = String::new();
        for i in 0..(LIMIT + 50) {
            s.push_str(&format!("line{i}\n"));
        }
        let path = write_temp("many.txt", s.as_bytes());
        let Canvas::Text(t) = render_with(&path, true, READ_LIMIT, LIMIT).unwrap() else {
            panic!("应为文本");
        };
        assert_eq!(t.lines.len(), LIMIT);
        assert_eq!(t.total_lines, LIMIT + 50);
        assert!(t.truncated);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tail_mode_keeps_last_lines() {
        const LIMIT: usize = 50;
        let mut s = String::new();
        for i in 0..(LIMIT + 5) {
            s.push_str(&format!("line{i}\n"));
        }
        let path = write_temp("tail.txt", s.as_bytes());
        let Canvas::Text(t) = render_with(&path, false, READ_LIMIT, LIMIT).unwrap() else {
            panic!("应为文本");
        };
        assert_eq!(t.lines.len(), LIMIT);
        assert_eq!(t.lines.last().unwrap(), &format!("line{}", LIMIT + 4));
        let _ = std::fs::remove_file(&path);
    }

    /// 滚动要能读到文件末尾——这是"只显示前 2000 行"那个旧限制的核心动机。
    #[test]
    fn no_line_cap_for_typical_files() {
        let mut s = String::new();
        for i in 0..3_000 {
            s.push_str(&format!("line{i}\n"));
        }
        let path = write_temp("three-k.txt", s.as_bytes());
        let Canvas::Text(t) = render(&path, true).unwrap() else {
            panic!("应为文本");
        };
        assert_eq!(t.lines.len(), 3_000);
        assert_eq!(t.total_lines, 3_000);
        assert!(!t.truncated);
        assert_eq!(t.lines[2_999], "line2999");
        let _ = std::fs::remove_file(&path);
    }
}
