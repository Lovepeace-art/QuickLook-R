//! 代码预览（1.0.8）：仿 VSCode 的等宽 + 行号 + 少量语法着色。
//!
//! 定位是"**看得清结构**"，不是完整的语言服务：一套通用的 token 切分（注释、
//! 字符串、数字、关键字），覆盖 C 系 / Rust / Python / shell / JS / Go / Java
//! 等常见写法，不认识的语言退化成"只有注释和字符串上色"，也不会出错。
//!
//! 两条刻意的取舍：
//! - **不折行**：VSCode 默认也不折行；折行会让"一行代码变两行"，行号对不上，
//!   而且代码缩进语义会被打乱。超长行到右边界直接截断。
//! - **不解析语法结构**：只做词法。要真高亮得引 tree-sitter（几 MB），
//!   与 A8.5 的体积预算冲突，这里够用就行。

use std::path::Path;

use qlr_core::{Error, Result};

use crate::canvas::{Canvas, Span, TextDoc, Token};
use crate::text::{self, MAX_LINE_CHARS, MAX_LINES, READ_LIMIT};

/// 这些扩展名走代码着色（其余纯文本走 `text::render`）。
///
/// 和 `format::TEXT_EXTS` 的关系：这里是它的**子集**——凡是能算"编程语言源码"的
/// 才进来，`log`/`csv`/`json` 之类仍然按纯文本显示（json 其实也上色，见下）。
pub const CODE_EXTS: &[&str] = &[
    // C 系
    "c", "h", "cc", "cpp", "cxx", "hpp", "hxx", "inl", "ipp", "m", "mm", "cs", "java", "kt", "kts",
    "scala", "dart", "swift", "go", "rs", "fs", "fsx", "vb", "groovy", "gradle",
    // 脚本
    "py", "pyw", "rb", "php", "lua", "pl", "pm", "t", "r", "jl", "sh", "bash", "zsh", "fish", "ps1",
    "psm1", "psd1", "bat", "cmd", // Web
    "js", "mjs", "cjs", "jsx", "ts", "tsx", "mts", "cts", "vue", "svelte", "astro", "css", "scss",
    "sass", "less", "html", "htm", "xhtml", "xml", "svg",
    // 数据/配置（也按"键值 + 字符串"上色，看起来比纯文本好读）
    "json", "jsonc", "json5", "toml", "yaml", "yml", "ini", "cfg", "conf", "sql", "graphql", "gql",
    "proto", "thrift", "cmake", "mk",
];

/// 这个文件该不该走代码着色（按扩展名，大小写不敏感）。
pub fn is_code(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| CODE_EXTS.contains(&e.as_str()))
}

/// `#` 开头是注释的语言（C 系里 `#` 是预处理指令，另作处理）。
fn hash_comment_lang(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "py" | "pyw"
            | "rb"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "pl"
            | "pm"
            | "r"
            | "jl"
            | "toml"
            | "yaml"
            | "yml"
            | "ini"
            | "cfg"
            | "conf"
            | "cmake"
            | "mk"
            | "ps1"
            | "psm1"
            | "psd1"
    )
}

/// `--` 开头是注释的语言。
fn dash_comment_lang(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| e == "sql" || e == "lua")
}

/// 关键字表（跨语言并集，小写比较）。排序后二分查找，避免每帧都是线性扫描。
const KEYWORDS: &[&str] = &[
    "abstract",
    "and",
    "as",
    "async",
    "await",
    "auto",
    "become",
    "bool",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "crate",
    "debugger",
    "def",
    "default",
    "defer",
    "delete",
    "do",
    "double",
    "dyn",
    "elif",
    "else",
    "end",
    "enum",
    "except",
    "explicit",
    "export",
    "extends",
    "extern",
    "false",
    "final",
    "finally",
    "float",
    "fn",
    "for",
    "foreach",
    "friend",
    "from",
    "func",
    "function",
    "global",
    "go",
    "goto",
    "if",
    "impl",
    "implements",
    "import",
    "in",
    "inline",
    "instanceof",
    "int",
    "interface",
    "is",
    "lambda",
    "let",
    "long",
    "loop",
    "match",
    "mod",
    "move",
    "mut",
    "namespace",
    "new",
    "nil",
    "none",
    "not",
    "null",
    "nullptr",
    "of",
    "operator",
    "or",
    "override",
    "package",
    "pass",
    "private",
    "protected",
    "pub",
    "public",
    "raise",
    "ref",
    "return",
    "self",
    "short",
    "signed",
    "sizeof",
    "static",
    "string",
    "struct",
    "super",
    "switch",
    "template",
    "then",
    "this",
    "throw",
    "trait",
    "true",
    "try",
    "type",
    "typedef",
    "typename",
    "typeof",
    "union",
    "unsafe",
    "unsigned",
    "use",
    "using",
    "var",
    "virtual",
    "void",
    "volatile",
    "where",
    "while",
    "with",
    "yield",
];

fn is_keyword(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    // Rust 字面量 `None` / Python `None` 都是大写开头，统一按小写比较。
    KEYWORDS.binary_search(&lower.as_str()).is_ok()
}

/// 渲染代码文件。
pub fn render(path: &Path, head: bool) -> Result<Canvas> {
    render_with(path, head, READ_LIMIT, MAX_LINES)
}

/// 同 `render`，但可指定读取预算与行数上限（测试用）。
pub fn render_with(path: &Path, head: bool, read_limit: usize, max_lines: usize) -> Result<Canvas> {
    crate::cancel::checkpoint()?;
    let (bytes, truncated_bytes) = text::read_capped(path, read_limit)?;
    if text::looks_binary(&bytes) {
        return Err(Error::Message(
            "看起来是二进制文件（含 NUL 字节），不走代码高亮".into(),
        ));
    }
    let (raw, encoding) = text::decode(&bytes);
    let mut lines: Vec<String> = raw.lines().map(|l| l.to_string()).collect();
    // 文件末尾是换行时 `lines()` 会少一行，和纯文本预览保持一致就不补了。
    let total_lines = lines.len();
    let truncated_lines = total_lines > max_lines;
    if truncated_lines {
        if head {
            lines.truncate(max_lines);
        } else {
            lines = lines.split_off(total_lines - max_lines);
        }
    }
    for line in &mut lines {
        if line.len() > MAX_LINE_CHARS {
            *line = text::truncate_chars(line, MAX_LINE_CHARS);
        }
    }

    let spans = tokenize_lines(path, &lines, truncated_lines && !head);
    Ok(Canvas::Text(TextDoc {
        lines,
        styles: None,
        spans: Some(spans),
        total_lines,
        truncated: truncated_bytes || truncated_lines,
        encoding,
    }))
}

/// 逐行切 token。`skip_first` = 这份内容是从文件中段截来的（块注释状态未知，直接从未注释开始）。
pub fn tokenize_lines(path: &Path, lines: &[String], skip_first: bool) -> Vec<Vec<Span>> {
    let hash = hash_comment_lang(path);
    let dash = dash_comment_lang(path);
    let mut out = Vec::with_capacity(lines.len());
    let mut in_block = false;
    for line in lines {
        let (spans, next) = tokenize_line(
            line,
            hash,
            dash,
            in_block && !(skip_first && out.is_empty()),
        );
        in_block = next;
        out.push(spans);
    }
    out
}

/// 单行切 token，返回 `(spans, 行末是否仍处在块注释里)`。
///
/// 纯函数（除了返回块注释状态），便于单测。
fn tokenize_line(
    line: &str,
    hash_comment: bool,
    dash_comment: bool,
    in_block: bool,
) -> (Vec<Span>, bool) {
    let chars: Vec<char> = line.chars().collect();
    let mut spans: Vec<Span> = Vec::new();
    let mut buf = String::new();
    let mut i = 0usize;
    let mut block = in_block;

    // 把缓冲区作为 Plain 段收尾（相邻同色段合并，少画几次 DrawTextW）。
    macro_rules! flush {
        () => {
            if !buf.is_empty() {
                spans.push(Span {
                    text: std::mem::take(&mut buf),
                    token: Token::Plain,
                });
            }
        };
    }
    macro_rules! push {
        ($text:expr, $token:expr) => {{
            flush!();
            spans.push(Span {
                text: $text,
                token: $token,
            });
        }};
    }

    while i < chars.len() {
        let ch = chars[i];

        // 块注释内部：一直吃到 `*/`
        if block {
            let mut end = chars.len();
            let mut j = i;
            while j + 1 < chars.len() {
                if chars[j] == '*' && chars[j + 1] == '/' {
                    end = j + 2;
                    break;
                }
                j += 1;
            }
            let text: String = chars[i..end].iter().collect();
            push!(text, Token::Comment);
            if end == chars.len() && !(chars.len() >= 2 && chars[chars.len() - 2..] == ['*', '/']) {
                // 没找到结束标记：整行都在注释里，状态延续到下一行。
                return (spans, true);
            }
            block = false;
            i = end;
            continue;
        }

        // 行注释
        if ch == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            let text: String = chars[i..].iter().collect();
            push!(text, Token::Comment);
            return (spans, false);
        }
        if hash_comment && ch == '#' {
            let text: String = chars[i..].iter().collect();
            push!(text, Token::Comment);
            return (spans, false);
        }
        if dash_comment && ch == '-' && i + 1 < chars.len() && chars[i + 1] == '-' {
            let text: String = chars[i..].iter().collect();
            push!(text, Token::Comment);
            return (spans, false);
        }

        // 块注释开始
        if ch == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            flush!();
            let mut j = i + 2;
            let mut closed = None;
            while j + 1 < chars.len() {
                if chars[j] == '*' && chars[j + 1] == '/' {
                    closed = Some(j + 2);
                    break;
                }
                j += 1;
            }
            match closed {
                Some(end) => {
                    let text: String = chars[i..end].iter().collect();
                    spans.push(Span {
                        text,
                        token: Token::Comment,
                    });
                    i = end;
                }
                None => {
                    let text: String = chars[i..].iter().collect();
                    spans.push(Span {
                        text,
                        token: Token::Comment,
                    });
                    return (spans, true);
                }
            }
            continue;
        }

        // 预处理指令（C 系）：`#include` / `#define` 等整行按"关键字 + 其余"
        if ch == '#' && !hash_comment && buf.is_empty() && spans.is_empty() {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_alphabetic() {
                j += 1;
            }
            if j > i + 1 {
                let text: String = chars[i..j].iter().collect();
                push!(text, Token::Keyword);
                i = j;
                continue;
            }
        }

        // 字符串（" ' ` 三种；`'` 在 C 系里是字符字面量，同样按字符串上色）
        if ch == '"' || ch == '\'' || ch == '`' {
            let quote = ch;
            let mut j = i + 1;
            let mut escaped = false;
            while j < chars.len() {
                let c = chars[j];
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == quote {
                    j += 1;
                    break;
                }
                j += 1;
            }
            let text: String = chars[i..j.min(chars.len())].iter().collect();
            push!(text, Token::Str);
            i = j;
            continue;
        }

        // 数字
        if ch.is_ascii_digit() {
            let mut j = i;
            while j < chars.len()
                && (chars[j].is_ascii_alphanumeric() || chars[j] == '.' || chars[j] == '_')
            {
                j += 1;
            }
            let text: String = chars[i..j].iter().collect();
            push!(text, Token::Number);
            i = j;
            continue;
        }

        // 标识符 / 关键字
        if ch.is_alphanumeric() || ch == '_' {
            let mut j = i;
            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            let word: String = chars[i..j].iter().collect();
            if is_keyword(&word) {
                push!(word, Token::Keyword);
            } else {
                buf.push_str(&word);
            }
            i = j;
            continue;
        }

        buf.push(ch);
        i += 1;
    }

    flush!();
    (spans, block)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(line: &str) -> Vec<(String, Token)> {
        let (spans, _) = tokenize_line(line, false, false, false);
        spans.into_iter().map(|s| (s.text, s.token)).collect()
    }

    fn has(list: &[(String, Token)], text: &str, token: Token) -> bool {
        list.iter().any(|(t, k)| t == text && *k == token)
    }

    #[test]
    fn keywords_strings_and_comments_are_tagged() {
        let list = tokens("fn main() { let s = \"hi\"; } // 注释");
        assert!(has(&list, "fn", Token::Keyword));
        assert!(has(&list, "let", Token::Keyword));
        assert!(has(&list, "\"hi\"", Token::Str));
        assert!(has(&list, "// 注释", Token::Comment));
    }

    #[test]
    fn numbers_are_tagged() {
        let list = tokens("let x = 0x1F + 3.14 + 42;");
        assert!(has(&list, "0x1F", Token::Number));
        assert!(has(&list, "3.14", Token::Number));
        assert!(has(&list, "42", Token::Number));
    }

    #[test]
    fn block_comment_spans_lines() {
        let (first, open) = tokenize_line("/* 开始", false, false, false);
        assert!(open);
        assert!(matches!(first[0].token, Token::Comment));
        let (second, open2) = tokenize_line("还在注释里 */ code();", false, false, open);
        assert!(!open2);
        assert!(second.iter().any(|s| s.text == "还在注释里 */"));
    }

    #[test]
    fn hash_comments_only_for_hash_languages() {
        let (spans, _) = tokenize_line("# 注释", true, false, false);
        assert_eq!(spans[0].token, Token::Comment);
        // C 系的 `#` 不是注释：`#include <stdio.h>` 里 include 是关键字
        let list = tokens("#include <stdio.h>");
        assert!(has(&list, "#include", Token::Keyword));
    }

    #[test]
    fn unterminated_string_does_not_hang() {
        let list = tokens("let s = \"没闭合");
        assert!(has(&list, "\"没闭合", Token::Str));
    }

    #[test]
    fn span_text_rebuilds_the_line_exactly() {
        let line = "impl Foo { fn bar(&self) -> u32 { 42 } } // done";
        let (spans, _) = tokenize_line(line, false, false, false);
        let rebuilt: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(rebuilt, line);
    }

    #[test]
    fn code_extensions_are_detected() {
        assert!(is_code(Path::new("a.rs")));
        assert!(is_code(Path::new("b.PY")));
        assert!(is_code(Path::new("c.json")));
        assert!(!is_code(Path::new("d.txt")));
        assert!(!is_code(Path::new("noext")));
    }
}
