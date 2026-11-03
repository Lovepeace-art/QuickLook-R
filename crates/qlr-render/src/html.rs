//! HTML 预览：把 HTML 当成**文档**排版，输出与 Markdown 同一套 `Canvas::Text`。
//!
//! 为什么不用 WebView2：ADR-0004 的零依赖路线不允许拖一个 WebView2Loader.dll +
//! Evergreen Runtime 依赖进来（体积、分发、卸载残留三笔账都不划算），而预览窗口
//! 只要"能读"，不要"像浏览器"。所以这里自写一个极简解析器：
//!
//! - 块级：`h1`–`h6`、`p`、`ul/ol/li`（含嵌套缩进）、`pre`、`blockquote`、`hr`、
//!   `table/tr/th/td`、`div` 等容器（容器本身不产出字符，只用来断行）；
//! - 行内：`b/strong`、`i/em`、`code`、`a`、`span`、`sup/sub` 等标记**只取文字**，
//!   不保留标记符号（行内样式交给 `qlr-platform` 按 `LineStyle` 上色）；
//! - 直接丢弃：`script`/`style`/`noscript`/`template`/`iframe`/`svg`/`canvas`/
//!   `object`/`embed`/`video` 的内容，以及 `<!-- 注释 -->`、`<head>` 整段。
//!
//! 排版约定与 `markdown.rs` 完全一致（同一套 `LineStyle`，同一套前缀符号）：
//! 列表 `• `（有序 `1. `）、引用 `▎ `、分隔线 `─`×40、表格 `a │ b`。
//! 这样壳层（`qlr-platform`）不需要为新格式加任何特判——`MarkdownViewer` 怎么画，
//! `HtmlViewer` 就怎么画。
//!
//! 已知取舍：不解析 CSS（`display:none` 的内容会照样显示）、不执行脚本、
//! 不处理 `&nbsp;` 的"不许断行"语义（直接当普通空格）、多栏/网格布局退化成一列。
//! 这些都是"读得懂就行"的可接受损失，换来的是零新依赖 + 可跨平台单测。

use std::path::Path;

use qlr_core::{Error, Result};

use crate::canvas::{Canvas, LineStyle, TextDoc};
use crate::text::{self, MAX_LINES, READ_LIMIT};

/// 分隔线画成多长（字符数，与 `markdown::RULE_CHARS` 一致）。
const RULE_CHARS: usize = 40;

/// 每层列表缩进两个空格（GDI 用等宽/比例字体都不至于挤在一起）。
const LIST_INDENT: &str = "  ";

/// 有序列表最多数到多少项就放弃编号（防止一份 10 万项的 `<li>` 拖慢排版）。
const MAX_ORDERED: usize = 99_999;

/// 渲染 HTML 文件（`head` 为 true 时超预算保留开头，否则保留结尾）。
pub fn render(path: &Path, head: bool) -> Result<Canvas> {
    render_with(path, head, READ_LIMIT, MAX_LINES)
}

/// 带预算的版本（测试用）。
///
/// 说明：**没有 `theme` 参数**。主题（深/浅色、字号）在 `qlr-platform` 按
/// `LineStyle` 决定，渲染层只交"文本 + 样式"，这是既有架构（`markdown.rs` 同样如此）。
/// 想调字号/颜色请改壳层，不要在渲染层塞主题。
pub fn render_with(path: &Path, head: bool, read_limit: usize, max_lines: usize) -> Result<Canvas> {
    crate::cancel::checkpoint()?;
    let (bytes, truncated_file) = text::read_capped(path, read_limit)?;
    if text::looks_binary(&bytes) {
        return Err(Error::Message(
            "看起来是二进制文件（含 NUL 字节），不走 HTML 渲染".to_string(),
        ));
    }
    let (src, encoding) = text::decode(&bytes);

    let (mut lines, mut styles) = layout(&src);
    // 这里**不能**学 Markdown 那样"排到预算就停"：状态行要显示"全文共 N 行"，
    // 而 HTML 的 N 是排版后的显示行数，早停就再也算不出准确值（Markdown 报的是
    // 源文件行数，不受影响）。省下的那几十毫秒不值得在界面上撒谎。
    let total_lines = lines.len();

    // 截断策略与纯文本/Markdown 一致：超预算就砍，砍哪一头看 `head`。
    let truncated_lines = lines.len() > max_lines;
    if truncated_lines {
        if head {
            lines.truncate(max_lines);
            styles.truncate(max_lines);
        } else {
            let start = lines.len() - max_lines;
            lines.drain(..start);
            styles.drain(..start);
        }
    }

    Ok(Canvas::Text(TextDoc {
        lines,
        styles: Some(styles),
        spans: None,
        total_lines,
        truncated: truncated_file || truncated_lines,
        encoding,
    }))
}

/// HTML 源码 → 「(显示文本, 样式)」。纯函数，测试直接打这个。
pub fn layout(src: &str) -> (Vec<String>, Vec<LineStyle>) {
    let stripped = strip_comments(&strip_hidden_all(src));
    let mut builder = Builder::default();
    for_each_token(&stripped, |token| match token {
        Tok::Text(t) => builder.push_text(t),
        Tok::Start(name, attrs) => builder.on_start(name, attrs),
        Tok::End(name) => builder.on_end(name),
    });
    builder.flush();
    (builder.lines, builder.styles)
}

/// 纯文本化（不排版、不带样式）：给"复制全文"或搜索用。
pub fn plain_text(src: &str) -> String {
    layout(src).0.join("\n")
}

// ---------------------------------------------------------------------------
// 预处理：剥掉不该显示的内容
// ---------------------------------------------------------------------------

/// 去掉 `<script>…</script>`、`<style>…</style>` 这类元素的**整段内容**。
///
/// `tag` 传空串时表示"把所有隐藏元素都剥一遍"。整段剥掉（而不是解析后挑着走）
/// 是因为脚本/CSS 里天然带 `<`、`>`、`//`，让 tokenizer 去硬扛只会自找苦吃。
/// 内容不显示的这些元素（连内容一起丢），`head` 也在里面。
const HIDDEN_TAGS: &[&str] = &[
    "head", "script", "style", "noscript", "template", "iframe", "svg", "canvas", "object",
    "embed", "video", "audio", "select",
];

/// 一次扫描剥掉所有隐藏元素（含内容）。
///
/// 为什么是"一次"：1.8.3 是每个标签各扫一遍全文件 —— 13 个标签 × 8 MB 的
/// 大小写不敏感查找，实测 8 MB 的 HTML 光这一步就要 145 ms，而屏幕上一屏
/// 只显示开头几十行。现在只走一遍：遇 `<` 读标签名，命中 [`HIDDEN_TAGS`]
/// 就整段跳到对应闭标签之后。
///
/// 语义与"逐标签剥"一致：先出现的外层元素先把整段吃掉，所以 `<svg>` 里的
/// `<script>` 不必单独处理。只有开标签没有闭标签时，后面的内容全算该元素
/// 内部（浏览器也是这个行为）。
fn strip_hidden_all(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut copied = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        let Some(off) = src[i..].find('<').map(|k| i + k) else {
            break;
        };
        // 读标签名（`<` 后面紧跟的字母数字），判断是不是隐藏元素。
        let name_start = off + 1;
        let mut j = name_start;
        while j < bytes.len() && bytes[j].is_ascii_alphanumeric() {
            j += 1;
        }
        if j > name_start {
            let name = &src[name_start..j];
            if let Some(tag) = HIDDEN_TAGS.iter().find(|t| name.eq_ignore_ascii_case(t)) {
                // 标签名必须有边界：`<scriptx>` 不是 `<script>`
                let bounded = bytes
                    .get(j)
                    .is_none_or(|b| b.is_ascii_whitespace() || *b == b'>' || *b == b'/');
                if bounded {
                    let open_end = find_ci(&src[j..], ">")
                        .map(|k| j + k + 1)
                        .unwrap_or(bytes.len());
                    // 自闭合 `<script/>`：整段就是一个标签，内容本来就没有
                    if !src[off..open_end].trim_end().ends_with("/>") {
                        let close = format!("</{tag}");
                        let Some(k) = find_ci(&src[open_end..], &close) else {
                            // 没有闭标签：后面的内容都算元素内部
                            out.push_str(&src[copied..off]);
                            return out;
                        };
                        let close_at = open_end + k;
                        let end = find_ci(&src[close_at..], ">")
                            .map(|m| close_at + m + 1)
                            .unwrap_or(bytes.len());
                        out.push_str(&src[copied..off]);
                        copied = end;
                        i = end;
                        continue;
                    }
                }
            }
        }
        i = off + 1;
    }

    out.push_str(&src[copied..]);
    out
}

/// 去掉 `<!-- … -->`。
fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    loop {
        let Some(i) = rest.find("<!--") else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..i]);
        match rest[i..].find("-->") {
            None => return out,
            Some(j) => rest = &rest[i + j + 3..],
        }
    }
}

/// 大小写不敏感的子串查找（返回**字节**偏移；针只能是 ASCII，故切点安全）。
fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    // 先按首字节粗筛，再比对整串：8 MB 的文件里绝大多数位置第一个字节就对不上，
    // 原来每个位置都调一次 eq_ignore_ascii_case，白花了十几倍的时间。
    let first = n[0];
    let first_upper = first.to_ascii_uppercase();
    let last = h.len() - n.len();
    for i in 0..=last {
        let b = h[i];
        if (b == first || b == first_upper) && h[i..i + n.len()].eq_ignore_ascii_case(n) {
            return Some(i);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 词法：标签 / 文本
// ---------------------------------------------------------------------------

enum Tok<'a> {
    Text(&'a str),
    /// 标签名（已小写）+ 属性正文
    Start(&'a str, &'a str),
    End(&'a str),
}

/// 流式词法：边扫边把 token 交给 `f`，不再先攒一个 `Vec<Tok>`。
///
/// 为什么改成流式（#158）：8 MB 的 HTML 有 96 万个 token，攒成 `Vec` 光结构体就
/// 23 MB，每个标签名还要各分配一个 `String`（近百万次小分配）。实测这段 38 ms、
/// 峰值多占四五十 MB。现在标签名复用同一个 `String` 缓冲，一扫到底。
fn for_each_token<F>(src: &str, mut f: F)
where
    F: for<'a> FnMut(Tok<'a>),
{
    let mut name = String::with_capacity(24);
    let mut rest = src;
    loop {
        let Some(lt) = rest.find('<') else {
            if !rest.is_empty() {
                f(Tok::Text(rest));
            }
            return;
        };
        if lt > 0 {
            f(Tok::Text(&rest[..lt]));
        }
        let after = &rest[lt + 1..];
        // `<!DOCTYPE>` / `<![CDATA[` / `<?xml`：整段丢弃
        if after.starts_with('!') || after.starts_with('?') {
            match after.find('>') {
                Some(end) => {
                    rest = &after[end + 1..];
                    continue;
                }
                None => return,
            }
        }
        let Some(end) = tag_end(after) else {
            // 掐断的标签：把剩下的当正文，别丢字
            f(Tok::Text(&rest[lt..]));
            return;
        };
        let body = &after[..end];
        rest = &after[end + 1..];
        let closing = body.starts_with('/');
        let name_body = if closing { &body[1..] } else { body };
        let name_len = name_body
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(name_body.len());
        let raw = &name_body[..name_len];
        if raw.is_empty() {
            continue;
        }
        name.clear();
        if raw.is_ascii() {
            for &b in raw.as_bytes() {
                name.push(b.to_ascii_lowercase() as char);
            }
        } else {
            // 非 ASCII 标签名（现实中几乎不存在）：退回整串小写，别把 UTF-8 拆坏
            name.push_str(&raw.to_ascii_lowercase());
        }
        let attrs = &name_body[name_len..];
        if closing {
            f(Tok::End(&name));
        } else {
            f(Tok::Start(&name, attrs));
        }
    }
}

/// 找标签结尾的 `>`（跳过引号里的 `>`，`title="a>b"` 不会把标签切断）。
fn tag_end(src: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in src.char_indices() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '>' => return Some(i),
                _ => {}
            },
        }
    }
    None
}

/// 取某个属性的值（只给 `img alt`、`a href` 这类少数用途）。
fn attr_value<'a>(attrs: &'a str, key: &str) -> Option<&'a str> {
    let bytes = attrs.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        let name_start = i;
        while i < bytes.len() && bytes[i] != b'=' && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let name = &attrs[name_start..i];
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = "";
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                value = &attrs[start..i];
                i += 1;
            } else {
                let start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                value = &attrs[start..i];
            }
        }
        if !name.is_empty() && name.eq_ignore_ascii_case(key) {
            return Some(value);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 排版
// ---------------------------------------------------------------------------

struct List {
    ordered: bool,
    next: usize,
}

#[derive(Default)]
struct Builder {
    lines: Vec<String>,
    styles: Vec<LineStyle>,
    /// 当前块：`(样式, 行前缀)`。前缀是列表符号/引用竖条，空串表示普通块。
    block: Option<(LineStyle, String)>,
    buf: String,
    /// 待补空格：HTML 会把连续空白压成一个空格，跨 token 也必须记住"刚才有个空白"。
    space: bool,
    in_pre: bool,
    lists: Vec<List>,
    /// 表格：当前行的单元格 / 是否表头行
    in_table: bool,
    row: Vec<String>,
    header_row: bool,
    /// `<title>` 之类要跳过文字的标签
    skip: Option<String>,
    /// 总字符预算（防止压缩成一行的 8 MB HTML 把排版拖死）
    chars: usize,
}

impl Builder {
    fn on_start(&mut self, name: &str, attrs: &str) {
        if let Some(skip) = &self.skip {
            // 只关心它的结束标签；嵌套同名标签不该出现在这里，出现也只当一次
            let _ = skip;
            return;
        }
        if name == "title" || name == "head" {
            self.skip = Some(name.to_string());
            return;
        }
        if self.in_table
            && !matches!(
                name,
                "table" | "tr" | "td" | "th" | "thead" | "tbody" | "tfoot"
            )
        {
            // 单元格里的行内标记：只取文字
            if name == "img"
                && let Some(alt) = attr_value(attrs, "alt")
                && !alt.trim().is_empty()
            {
                self.push_text(&format!("[{}]", alt.trim()));
            }
            if name == "br" {
                self.space = true;
            }
            return;
        }
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = match name {
                    "h1" => LineStyle::Heading1,
                    "h2" => LineStyle::Heading2,
                    _ => LineStyle::Heading3,
                };
                self.begin(level, String::new());
            }
            "p" | "div" | "section" | "article" | "header" | "footer" | "main" | "nav"
            | "aside" | "figure" | "figcaption" | "form" | "body" | "html" | "dl" | "dt" | "dd" => {
                self.begin(LineStyle::Normal, String::new());
            }
            "blockquote" => self.begin(LineStyle::Quote, format!("▎ {}", String::new())),
            "pre" => {
                self.begin(LineStyle::Code, String::new());
                self.in_pre = true;
            }
            "ul" | "ol" => {
                self.flush();
                self.lists.push(List {
                    ordered: name == "ol",
                    next: 1,
                });
            }
            "li" => {
                self.flush();
                let depth = self.lists.len().saturating_sub(1);
                let (ordered, number) = match self.lists.last_mut() {
                    Some(list) if list.ordered => {
                        let n = list.next.min(MAX_ORDERED);
                        list.next += 1;
                        (true, n)
                    }
                    _ => (false, 0),
                };
                let prefix = if ordered {
                    format!("{}{}. ", LIST_INDENT.repeat(depth), number)
                } else {
                    format!("{}• ", LIST_INDENT.repeat(depth))
                };
                self.begin(LineStyle::List, prefix);
            }
            "table" => {
                self.flush();
                self.in_table = true;
                self.row.clear();
            }
            "tr" => {
                self.flush();
                self.row.clear();
                self.header_row = false;
            }
            "th" | "td" => {
                self.flush();
                if name == "th" {
                    self.header_row = true;
                }
            }
            "hr" => {
                self.flush();
                if !self.lines.last().is_some_and(|l| l.is_empty()) {
                    self.lines.push(String::new());
                    self.styles.push(LineStyle::Normal);
                }
                self.lines.push("─".repeat(RULE_CHARS));
                self.styles.push(LineStyle::Rule);
            }
            "br" => self.hard_break(),
            "img" => {
                if let Some(alt) = attr_value(attrs, "alt")
                    && !alt.trim().is_empty()
                {
                    self.push_text(&format!("[{}]", alt.trim()));
                }
            }
            _ => {}
        }
    }

    fn on_end(&mut self, name: &str) {
        if let Some(skip) = self.skip.take_if(|s| s == name) {
            let _ = skip;
            return;
        }
        if self.skip.is_some() {
            // 还在被跳过的元素里（比如 </style> 里的怪东西），不理
            if name != "title" && name != "head" {
                return;
            }
            return;
        }
        if self.in_table {
            match name {
                "td" | "th" => {
                    let cell = self.take_buf().trim().to_string();
                    self.row.push(cell);
                }
                "tr" => {
                    self.emit_row();
                }
                "table" => {
                    self.flush();
                    self.in_table = false;
                    self.row.clear();
                }
                _ => {}
            }
            return;
        }
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "p" | "div" | "section" | "article"
            | "header" | "footer" | "main" | "nav" | "aside" | "figure" | "figcaption" | "form"
            | "body" | "html" | "blockquote" | "dl" | "dt" | "dd" | "li" => self.flush(),
            "pre" => {
                self.flush();
                self.in_pre = false;
            }
            "ul" | "ol" => {
                self.flush();
                self.lists.pop();
            }
            _ => {}
        }
    }

    fn push_text(&mut self, s: &str) {
        if self.skip.is_some() {
            return;
        }
        // 实体在这里统一还原（`&lt;` → `<`，`&#20013;` → `中`）。
        // 放在**取文字**这一步而不是 tokenizer：tokenizer 要拿原始的 `&lt;` 才能不把它当成
        // 标签开头，而排版需要的是还原后的字符。`<pre>` 也得还原——`&lt;!--` 在代码块里
        // 就是字面的注释开头，不还原就丢了可读性。
        let decoded;
        let s = if s.contains('&') {
            decoded = decode_entities(s);
            decoded.as_str()
        } else {
            s
        };
        self.chars += s.len();
        if self.chars > 8 * 1024 * 1024 {
            // 极端兜底：超过 8 MB 文字不再排版（文件本身已被 read_capped 限过，这里只是保险）
            return;
        }
        if self.in_pre {
            for (i, part) in s.split('\n').enumerate() {
                if i > 0 {
                    self.emit_line(LineStyle::Code, String::new(), true);
                }
                self.buf.push_str(part);
            }
            return;
        }
        // 按"非空白段"整段拷进 `buf`，而不是逐字符 `push`：8 MB 正文光这一层就要
        // 三四十毫秒，切成段 `push_str` 之后是 memcpy（#158）。
        // ASCII 空白直接按字节判；非 ASCII 字节才解一个 `char` 问 `is_whitespace()`
        // （U+3000 这类全角空格也算空白，不能只认 ASCII）。
        let bytes = s.as_bytes();
        let mut i = 0usize;
        let mut run: Option<usize> = None;
        while i < bytes.len() {
            let byte = bytes[i];
            let (whitespace, step) = if byte < 0x80 {
                (byte.is_ascii_whitespace(), 1)
            } else {
                // `i` 永远落在字符边界上，解一个 `char` 不会越界
                let ch = s[i..].chars().next().unwrap_or(' ');
                (ch.is_whitespace(), ch.len_utf8())
            };
            if whitespace {
                if let Some(start) = run.take() {
                    self.push_run(&s[start..i]);
                }
                self.space = true;
            } else if run.is_none() {
                run = Some(i);
            }
            i += step;
        }
        if let Some(start) = run {
            self.push_run(&s[start..]);
        }
    }

    /// 把一段不含空白的文字接进当前行：需要时先补那个被压缩掉的空格。
    fn push_run(&mut self, text: &str) {
        if self.space && !self.buf.is_empty() {
            self.buf.push(' ');
        }
        self.space = false;
        self.buf.push_str(text);
    }

    /// 起一个新块：先把上一块吐出去，再按需插一个空行（段落之间要留白）。
    fn begin(&mut self, style: LineStyle, prefix: String) {
        self.flush();
        self.separate(style);
        self.block = Some((style, prefix));
        self.space = false;
    }

    /// `<br>` / `pre` 里的硬换行：同一块继续，但先把当前内容落成一行。
    fn hard_break(&mut self) {
        let Some((style, prefix)) = self.block.clone() else {
            return;
        };
        self.emit_line(style, prefix, false);
    }

    /// 落一行（`raw` = true 表示代码块：保留前后空格）。
    fn emit_line(&mut self, style: LineStyle, prefix: String, raw: bool) {
        let body = self.take_buf();
        let text = if raw { body } else { body.trim().to_string() };
        let line = if text.is_empty() {
            String::new()
        } else {
            format!("{prefix}{text}")
        };
        if line.is_empty() && style != LineStyle::Code {
            return;
        }
        self.lines.push(clip(&line));
        self.styles.push(style);
        self.space = false;
    }

    fn flush(&mut self) {
        let Some((style, prefix)) = self.block.take() else {
            return;
        };
        let body = self.take_buf();
        // 代码块（`<pre>`）只去尾部空白：行首缩进是内容的一部分（Python 缩进、`&lt;!--` 的
        // 缩进都靠它），trim() 会把 `\t`/空格一起吃掉，读起来就像代码被"左对齐"了。
        let text = if style == LineStyle::Code {
            body.trim_end()
        } else {
            body.trim()
        };
        if !text.is_empty() {
            self.lines.push(clip(&format!("{prefix}{text}")));
            self.styles.push(style);
        }
        self.space = false;
    }

    fn take_buf(&mut self) -> String {
        std::mem::take(&mut self.buf)
    }

    /// 需要时补一个空行，让"块"之间有呼吸感（列表项之间不补，否则一份 50 项清单会翻倍）。
    fn separate(&mut self, style: LineStyle) {
        let Some(last) = self.styles.last().copied() else {
            return;
        };
        if self.lines.last().is_some_and(|l| l.is_empty()) {
            return;
        }
        let adjacent_compact = matches!(style, LineStyle::List | LineStyle::Quote)
            && matches!(last, LineStyle::List | LineStyle::Quote);
        if adjacent_compact {
            return;
        }
        self.lines.push(String::new());
        self.styles.push(LineStyle::Normal);
    }

    /// 表格一行：单元格用 `│` 连起来（与 Markdown 表格同一套画法）。
    fn emit_row(&mut self) {
        self.buf.clear();
        if self.row.is_empty() {
            return;
        }
        let joined = self
            .row
            .iter()
            .map(|c| c.trim())
            .collect::<Vec<_>>()
            .join(" │ ");
        self.row.clear();
        let header = self.header_row;
        self.header_row = false;
        if joined.trim().is_empty() {
            return;
        }
        self.separate(LineStyle::Table);
        self.lines.push(clip(&joined));
        self.styles.push(LineStyle::Table);
        if header {
            self.lines.push("─".repeat(RULE_CHARS));
            self.styles.push(LineStyle::Rule);
        }
    }
}

/// 单行长度上限（与 Markdown 一致，防止一行几万字把 GDI 撑爆）。
fn clip(s: &str) -> String {
    text::truncate_chars(s, text::MAX_LINE_CHARS)
}

/// 还原 HTML 字符实体：数字（`&#20013;` / `&#x6587;`）与常见命名实体。
///
/// 为什么不引 crate：命名实体的完整表有两千多项（`html5ever` 那份），而预览只要
/// "读得懂"——ASCII 标点 + 排版符号 + 几种货币/度量单位就够。认不出来的实体**原样保留**
/// （显示成 `&foo;`），既不猜也不会把内容吃掉。
fn decode_entities(s: &str) -> String {
    const NAMED: [(&str, char); 29] = [
        ("amp", '&'),
        ("lt", '<'),
        ("gt", '>'),
        ("quot", '"'),
        ("apos", '\''),
        ("nbsp", '\u{a0}'),
        ("ensp", '\u{2002}'),
        ("emsp", '\u{2003}'),
        ("thinsp", '\u{2009}'),
        ("copy", '©'),
        ("reg", '®'),
        ("trade", '™'),
        ("deg", '°'),
        ("plusmn", '±'),
        ("times", '×'),
        ("divide", '÷'),
        ("middot", '·'),
        ("bull", '•'),
        ("hellip", '…'),
        ("mdash", '—'),
        ("ndash", '–'),
        ("lsquo", '‘'),
        ("rsquo", '’'),
        ("ldquo", '“'),
        ("rdquo", '”'),
        ("laquo", '«'),
        ("raquo", '»'),
        ("larr", '←'),
        ("rarr", '→'),
    ];

    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 1..];
        // 实体最长 `&CounterClockwiseContourIntegral;` 那类也就 30 多个字符，扫到 `;` 或
        // 32 字符还没结束就当"不是实体"，把 `&` 原样输出（避免在长文本里反复扫描）。
        let end = after
            .char_indices()
            .take(33)
            .find(|(_, c)| *c == ';')
            .map(|(i, _)| i);
        let Some(end) = end else {
            out.push('&');
            rest = after;
            continue;
        };
        let body = &after[..end];
        let decoded = if let Some(hex) = body.strip_prefix("#x").or_else(|| body.strip_prefix("#X"))
        {
            u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
        } else if let Some(dec) = body.strip_prefix('#') {
            dec.parse::<u32>().ok().and_then(char::from_u32)
        } else {
            NAMED
                .iter()
                .find(|(name, _)| *name == body)
                .map(|(_, c)| *c)
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
  <title>页面标题</title>
  <style>
    body { color: #333; font-family: sans-serif; }
    .card:hover { transform: translateY(-2px); }
  </style>
  <script>console.log("这段不该出现"); alert(1);</script>
</head>
<body>
  <h1>QLRust <span>预览文档</span></h1>
  <p class="lead">这是一段<b>加粗</b>的正文，里面有 <code>inline()</code> 代码与&nbsp;空格。</p>
  <h2>卡片区</h2>
  <ul>
    <li>第一项</li>
    <li>第二项
      <ul>
        <li>子项 A</li>
      </ul>
    </li>
  </ul>
  <ol>
    <li>步骤一</li>
    <li>步骤二</li>
  </ol>
  <pre><code>&lt;!-- 中文注释 --&gt;
  code line</code></pre>
  <table>
    <tr><th>名称</th><th>数量</th></tr>
    <tr><td>苹果</td><td>3</td></tr>
  </table>
  <blockquote>引用一句</blockquote>
  <hr>
  <footer><p>&#20013;文实体 &amp; 收尾</p></footer>
</body>
</html>"#;

    fn texts() -> Vec<String> {
        layout(SAMPLE).0
    }

    fn styled() -> Vec<(String, LineStyle)> {
        let (lines, styles) = layout(SAMPLE);
        lines.into_iter().zip(styles).collect()
    }

    fn find(prefix: &str) -> Option<(String, LineStyle)> {
        styled().into_iter().find(|(l, _)| l.contains(prefix))
    }

    #[test]
    fn heading_paragraph_and_list_are_laid_out() {
        let (lines, styles) = layout(SAMPLE);
        assert_eq!(lines.len(), styles.len(), "文本与样式必须一一对应");

        // h1：样式必须是 Heading1，且标题里的 <span> 只留文字
        let h1 = find("QLRust").expect("应有 h1");
        assert_eq!(h1.1, LineStyle::Heading1);
        assert_eq!(h1.0.trim(), "QLRust 预览文档");

        // h2 → Heading2
        assert_eq!(find("卡片区").expect("应有 h2").1, LineStyle::Heading2);

        // 正文：加粗/代码标记都只留文字，不残留 <b> 之类的符号
        let p = find("这是一段加粗的正文").expect("应有正文行");
        assert_eq!(p.1, LineStyle::Normal);
        assert!(p.0.contains("inline()"), "行内代码文字应保留：{}", p.0);
        assert!(!p.0.contains('<'), "不应残留标签：{}", p.0);

        // 一级列表项：前缀 `• `
        assert_eq!(find("第一项").expect("应有列表项").0, "• 第一项");
        // 嵌套列表项：缩进两个空格（层级必须看得出来）
        let nested = find("子项 A").expect("应有嵌套列表项");
        assert_eq!(nested.0, "  • 子项 A");
        assert_eq!(nested.1, LineStyle::List);

        // 有序列表：编号 + 同样的前缀位置
        assert_eq!(find("步骤一").expect("应有有序项").0, "1. 步骤一");
        assert_eq!(find("步骤二").expect("应有有序项").0, "2. 步骤二");
    }

    #[test]
    fn blocks_are_separated_by_blank_lines() {
        let (lines, _) = layout(SAMPLE);
        let h1 = lines.iter().position(|l| l.contains("QLRust")).unwrap();
        assert!(
            lines.get(h1 + 1).is_some_and(|l| l.is_empty()),
            "标题后应有空行，实际：{:?}",
            lines.get(h1 + 1)
        );
    }

    #[test]
    fn script_and_style_content_is_dropped() {
        let joined = texts().join("\n");
        for forbidden in ["console", "alert", "color: #333", "translateY", "页面标题"] {
            assert!(
                !joined.contains(forbidden),
                "script/style/head 内容不该出现：{forbidden}"
            );
        }
    }

    #[test]
    fn entities_and_protected_characters_are_decoded() {
        let (lines, styles) = layout(
            r#"<p>a &lt; b &amp;&amp; c &gt; d &#20013; &#x6587; &quot;q&quot; &mdash; &hellip;</p>"#,
        );
        // 源码里两个数字实体之间**有空格**，按 HTML 语义空格要保留（浏览器也一样）。
        assert_eq!(lines, vec!["a < b && c > d 中 文 \"q\" — …".to_string()]);
        assert_eq!(styles, vec![LineStyle::Normal]);

        // 相邻实体不插空格；认不出来的实体原样保留（不猜、不吞内容）
        assert_eq!(
            layout("<p>&#20013;&#x6587;</p>").0,
            vec!["中文".to_string()]
        );
        assert_eq!(
            layout("<p>AT&T &foo; &#xZZ;</p>").0,
            vec!["AT&T &foo; &#xZZ;".to_string()]
        );

        // 代码块里的 `&lt;!--` 必须还原成 `<!--`，不能被当注释剥掉
        let (lines, styles) = layout("<pre>&lt;!-- 注释 --&gt;\nx</pre>");
        assert_eq!(lines[0], "<!-- 注释 -->");
        assert_eq!(lines[0..2], [lines[0].clone(), "x".to_string()][..]);
        assert!(styles.iter().all(|s| *s == LineStyle::Code));
    }

    #[test]
    fn table_rows_join_cells_and_header_gets_rule() {
        let styled = styled();
        let row = styled
            .iter()
            .find(|(l, _)| l.contains("苹果"))
            .expect("应有表格行");
        assert_eq!(row.0, "苹果 │ 3");
        assert_eq!(row.1, LineStyle::Table);

        let header = styled
            .iter()
            .position(|(l, _)| l.starts_with("名称 │ 数量"))
            .expect("应有表头行");
        assert_eq!(styled[header].1, LineStyle::Table);
        // 表头后面紧跟分隔线（和 Markdown 表格观感一致）
        assert_eq!(styled[header + 1].1, LineStyle::Rule);
        assert_eq!(styled[header + 1].0, "─".repeat(RULE_CHARS));
    }

    #[test]
    fn pre_block_keeps_leading_spaces_and_line_breaks() {
        let (lines, styles) = layout("<pre>  two spaces\n\tTab\tend</pre>");
        assert_eq!(lines[0], "  two spaces");
        assert_eq!(lines[1], "\tTab\tend");
        assert!(styles.iter().all(|s| *s == LineStyle::Code));
    }

    #[test]
    fn blockquote_and_rule_use_markdown_prefixes() {
        let styled = styled();
        let quote = styled
            .iter()
            .find(|(l, _)| l.contains("引用一句"))
            .expect("应有引用");
        assert_eq!(quote.0, "▎ 引用一句");
        assert_eq!(quote.1, LineStyle::Quote);
        assert!(
            styled
                .iter()
                .any(|(l, s)| *s == LineStyle::Rule && l.chars().all(|c| c == '─')),
            "应有分隔线"
        );
    }

    #[test]
    fn broken_html_does_not_panic() {
        for bad in [
            "",
            "纯文本没有标签",
            "<",
            "<h1",
            "<h1>没闭合的标题",
            "</p></div>孤立的结束标签",
            "<script>没闭合的脚本",
            "<table><tr><td>半截表格",
            "<p>a<br>b</p>",
            "<ul><li>没有结束的列表",
            "<svg><rect/></svg><p>后面还有正文</p>",
        ] {
            let (lines, styles) = layout(bad);
            assert_eq!(
                lines.len(),
                styles.len(),
                "坏输入也不能让文本与样式错位：{bad:?}"
            );
        }
        // 「没有结束的列表」的正文不能丢
        assert!(layout("<ul><li>没有结束的列表").0[0].contains("没有结束的列表"));
        // 被剥掉的 svg 之后的内容要保留
        assert!(
            layout("<svg><rect/></svg><p>后面还有正文</p>")
                .0
                .iter()
                .any(|l| l.contains("后面还有正文"))
        );
    }

    #[test]
    fn tag_names_are_matched_case_insensitively() {
        // 流式词法复用同一个标签名缓冲：大小写归一，且后一个标签不会串上一个的名字。
        let (lines, styles) =
            layout("<DIV><H2>标题</H2><P>正文 <B>粗</B></P><UL><LI>A</LI></UL></DIV>");
        assert_eq!(
            lines,
            vec![
                "标题".to_string(),
                String::new(),
                "正文 粗".to_string(),
                String::new(),
                "• A".to_string(),
            ]
        );
        assert_eq!(styles[0], LineStyle::Heading2);
        assert_eq!(styles[2], LineStyle::Normal);
        assert_eq!(styles[4], LineStyle::List);
    }

    #[test]
    fn image_alt_becomes_placeholder_text() {
        let (lines, _) = layout(r#"<p><img src="a.png" alt="示意图"> 说明</p>"#);
        assert!(lines[0].contains("[示意图]"), "{}", lines[0]);
    }

    #[test]
    fn renders_file_to_text_canvas() {
        let path = crate::testutil::write_file("html", "sample.html", SAMPLE.as_bytes());
        let canvas = render(&path, true).expect("HTML 文件应渲染成功");
        let doc = match canvas {
            Canvas::Text(doc) => doc,
            other => panic!("HTML 应产出文本画布，得到 {}", other.kind_name()),
        };
        assert!(doc.lines.len() > 10, "行数太少：{}", doc.lines.len());
        assert_eq!(doc.styles.as_ref().map(Vec::len), Some(doc.lines.len()));
        assert!(!doc.truncated);
        assert_eq!(doc.total_lines, doc.lines.len());
        // 排版结果里不该有 HTML 标签残留
        assert!(
            doc.lines
                .iter()
                .all(|l| !l.contains("<p>") && !l.contains("<h1>"))
        );
    }

    #[test]
    fn binary_file_is_rejected() {
        let path = crate::testutil::write_file("html", "bin.html", b"<p>\x00\x01binary</p>");
        let err = render(&path, true).unwrap_err();
        assert!(format!("{err}").contains("二进制"), "{err}");
    }

    #[test]
    fn long_document_is_truncated_by_budget() {
        let mut html = String::from("<ul>");
        for i in 0..50 {
            html.push_str(&format!("<li>item {i}</li>"));
        }
        html.push_str("</ul>");
        let path = crate::testutil::write_file("html", "long.html", html.as_bytes());
        let doc = match render_with(&path, true, READ_LIMIT, 10).expect("应成功") {
            Canvas::Text(doc) => doc,
            other => panic!("应产出文本画布，得到 {}", other.kind_name()),
        };
        assert_eq!(doc.lines.len(), 10, "应按预算截断");
        assert!(doc.truncated);
        assert_eq!(doc.total_lines, 50);
        assert_eq!(doc.lines[0], "• item 0");
    }

    #[test]
    fn plain_text_flattens_the_document() {
        let text = plain_text(SAMPLE);
        assert!(text.contains("QLRust 预览文档"));
        assert!(text.contains("• 第一项"));
        assert!(!text.contains("console"));
    }

    /// 手测用（CI 不跑）：8 MB 的 HTML 各阶段耗时，用来判断"排版早停"值不值得做。
    ///
    /// ```text
    /// cargo test --release -p qlr-render --lib bench_html_phases -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn bench_html_phases() {
        use std::time::Instant;
        // 不用 temp_dir()：本机 TEMP 在受限会话里可能不可写（实测拒绝访问）。
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("bench-text");
        std::fs::create_dir_all(&dir).expect("建目录");
        let path = dir.join("big.html");
        if !path.exists() {
            let mut s = String::with_capacity(8 * 1024 * 1024 + 4096);
            let mut n = 0usize;
            while s.len() < 8 * 1024 * 1024 {
                s.push_str(&format!(
                    "<h2>第 {n} 节</h2>\n<p>普通段落，混一点中文与 English，长度接近真实文档的一行，带 <b>粗体</b> 与 <i>斜体</i> 与 <a href=\"https://example.com/{n}\">链接</a>。</p>\n<ul><li>列表项一</li><li>列表项二</li></ul>\n"
                ));
                n += 1;
            }
            std::fs::write(&path, &s).expect("写夹具");
        }
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;

        let t = Instant::now();
        let bytes = std::fs::read(&path).expect("读夹具");
        println!(
            "文件 {} KB，fs::read {:.1} ms",
            bytes.len() / 1024,
            ms(t.elapsed())
        );

        let t = Instant::now();
        let (src, _encoding) = crate::text::decode(&bytes);
        println!("decode {:.1} ms", ms(t.elapsed()));

        // 各阶段都跑 3 遍取最好成绩：单次测量在这台机器上抖动能到 ±6 ms。
        let stripped = strip_comments(&strip_hidden_all(&src));

        let mut strip_best = f64::MAX;
        for _ in 0..3 {
            let t = Instant::now();
            let pre = strip_comments(&strip_hidden_all(&src));
            std::hint::black_box(&pre);
            strip_best = strip_best.min(ms(t.elapsed()));
        }
        println!("预处理（head/script/style/注释）{strip_best:.1} ms");

        let mut tok_best = f64::MAX;
        let mut tok_n = 0usize;
        for _ in 0..3 {
            let t = Instant::now();
            let mut n = 0usize;
            for_each_token(&stripped, |_tok| n += 1);
            tok_best = tok_best.min(ms(t.elapsed()));
            tok_n = n;
        }
        println!("词法（流式，不攒 Vec）{tok_n} 个 token，{tok_best:.1} ms");

        let mut layout_best = f64::MAX;
        let mut layout_lines = 0usize;
        for _ in 0..3 {
            let t = Instant::now();
            let (l, _styles) = layout(&src);
            layout_best = layout_best.min(ms(t.elapsed()));
            layout_lines = l.len();
        }
        println!("layout 全量 {layout_best:.1} ms → {layout_lines} 显示行");

        for limit in [MAX_LINES, 400] {
            let mut best = f64::MAX;
            let mut n = 0usize;
            for _ in 0..3 {
                let t = Instant::now();
                let canvas = render_with(&path, true, READ_LIMIT, limit).expect("渲染");
                best = best.min(ms(t.elapsed()));
                n = match canvas {
                    Canvas::Text(doc) => doc.lines.len(),
                    _ => 0,
                };
            }
            println!("render_with(head=true, {limit} 行) {best:.1} ms → {n} 行");
        }
    }
}
