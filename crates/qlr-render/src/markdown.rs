//! Markdown 轻量排版：把源文件切成「(文本, 样式)」的行序列。
//!
//! 刻意**不做完整 CommonMark**——目标是"打开 README 一眼能看懂结构"，不是渲染器。
//! 支持的块级元素：ATX 标题（`#`…`######`）、围栏代码块（``` / ~~~）、引用（`>`）、
//! 无序/有序列表（含任务列表 `- [ ]`）、分隔线（`---` / `***` / `___`）、表格（`| a | b |`）。
//! 支持的**行内**元素（1.0.5）：行内代码、粗体/斜体/删除线标记、链接与图片、反斜杠转义、
//! 简单 HTML 标签——一律**去掉标记、保留文字**。用户的原话是
//! "md 的预览需要是渲染后的格式，不能是源码格式"：满屏 `**`、`` ` ``、`[]()` 就是源码感。
//! 因为一行只有一个字号/字重，行内不加粗（要粗体得做字符级排版，留给插件）。
//! **不做**的：表格对齐、HTML 渲染、脚注、参考式链接的语义。
//!
//! 这一层只产出**语义样式**（`LineStyle`），字号/颜色/字体面由 `qlr-platform` 决定：
//! 渲染层不依赖 `windows`，才能跨平台单测。

use std::path::Path;

use qlr_core::{Error, Result};

use crate::canvas::{Canvas, LineStyle, TextDoc};
use crate::text::{self, MAX_LINE_CHARS, MAX_LINES, READ_LIMIT};

/// 分隔线画成多长的一根横线（字符数，不是像素）。
const RULE_CHARS: usize = 40;

/// 渲染 Markdown 文件。`head` 为 true 时超出预算保留开头，否则保留结尾。
pub fn render(path: &Path, head: bool) -> Result<Canvas> {
    render_with(path, head, READ_LIMIT, MAX_LINES)
}

/// 带预算的版本（测试用）。
pub fn render_with(path: &Path, head: bool, read_limit: usize, max_lines: usize) -> Result<Canvas> {
    crate::cancel::checkpoint()?;
    let (bytes, truncated_file) = text::read_capped(path, read_limit)?;
    if text::looks_binary(&bytes) {
        return Err(Error::Message(
            "看起来是二进制文件（含 NUL 字节），不走 Markdown 渲染".to_string(),
        ));
    }
    let (src, encoding) = text::decode(&bytes);

    let mut raw: Vec<&str> = if src.is_empty() {
        Vec::new()
    } else {
        src.split('\n').map(|l| l.trim_end_matches('\r')).collect()
    };
    // 以换行结尾的文件不该多出一个空行。
    if src.ends_with('\n') {
        raw.pop();
    }
    let total_lines = raw.len();

    let (mut lines, mut styles) = layout(&raw);

    // 截断策略与纯文本一致：超预算就砍，砍哪一头看 `head`。
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

/// 目录项：一个标题，以及它在**显示行**里的下标。
pub struct TocEntry {
    /// 1–3（4–6 级标题在排版时就并成了 3）。
    pub level: u8,
    pub text: String,
    /// 显示行下标（与 `TextDoc::lines` 同一套坐标）。
    pub line: usize,
}

/// 从已排版好的显示行里抽目录（Markdown 的 h1–h3）。
///
/// 为什么基于"显示行"而不是源文件：壳层滚动用的是显示行坐标，给它同一套坐标
/// 才能"点目录 → 直接滚到那一行"，不用再做一层映射（映射错位正是行号对不上的经典来源）。
pub fn outline(lines: &[String], styles: &[LineStyle]) -> Vec<TocEntry> {
    let mut out = Vec::new();
    for (index, (text, style)) in lines.iter().zip(styles.iter()).enumerate() {
        let level = match style {
            LineStyle::Heading1 => 1,
            LineStyle::Heading2 => 2,
            LineStyle::Heading3 => 3,
            _ => continue,
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push(TocEntry {
            level,
            text: trimmed.to_string(),
            line: index,
        });
    }
    out
}

/// 把源行映射成「(显示文本, 样式)」。
///
/// 围栏标记行本身**不显示**（否则满屏 ``` 很难看），只负责切换 `Code` 状态。
/// 代码块内部保留原样（`**` 在代码里就是字面上的两个星号）；
/// 其余行都过一遍 `inline()`，去掉行内标记。
pub fn layout(lines: &[&str]) -> (Vec<String>, Vec<LineStyle>) {
    let mut out_text = Vec::with_capacity(lines.len());
    let mut out_style = Vec::with_capacity(lines.len());
    let mut in_fence = false;

    for raw in lines {
        let line = raw.trim_end();

        if is_fence(line) {
            in_fence = !in_fence;
            continue;
        }

        let (text, style) = if in_fence {
            (clip(line), LineStyle::Code)
        } else if let Some((text, level)) = heading(line) {
            let style = match level {
                1 => LineStyle::Heading1,
                2 => LineStyle::Heading2,
                _ => LineStyle::Heading3,
            };
            (text, style)
        } else if let Some((text, is_separator)) = table_row(line) {
            if is_separator {
                ("─".repeat(RULE_CHARS), LineStyle::Rule)
            } else {
                (text, LineStyle::Table)
            }
        } else if let Some(rest) = quote(line) {
            (clip(&format!("▎ {}", inline(&rest))), LineStyle::Quote)
        } else if is_rule(line) {
            ("─".repeat(RULE_CHARS), LineStyle::Rule)
        } else if let Some(rest) = list_item(line) {
            (clip(&format!("• {}", task_marker(&rest))), LineStyle::List)
        } else {
            (clip(&inline(line)), LineStyle::Normal)
        };

        out_text.push(text);
        out_style.push(style);
    }

    (out_text, out_style)
}

/// 列表项里的任务框：`[ ] 待办` → `☐ 待办`、`[x] 已完成` → `☑ 已完成`。
/// 其它内容原样（返回带行内标记清理后的文本）。
fn task_marker(rest: &str) -> String {
    let trimmed = rest.trim_start();
    for (prefix, box_) in [("[ ] ", "☐ "), ("[x] ", "☑ "), ("[X] ", "☑ ")] {
        if let Some(after) = trimmed.strip_prefix(prefix) {
            return format!("{box_}{}", inline(after));
        }
    }
    inline(rest)
}

/// 去掉行内 Markdown 标记，只留文字（1.0.5）。
///
/// 顺序有讲究：先把"成对的分隔符"按**从长到短**处理（`**` 在 `*` 之前），
/// 否则 `**粗**` 会先被单星号拆成 `*粗*` 的残渣。链接必须在强调之前——
/// `[**a**](u)` 这种写法要先把整体识别成链接，再做内部强调。
fn inline(s: &str) -> String {
    // 转义必须先"锁"起来：`\*星号\*` 里的星号是字面量，不能让下面的去标记逻辑吃掉。
    // 做法是把 `\x` 换成私有区的同码点字符（U+E000 + x），最后一律还原。
    let mut t = protect_escapes(s);
    t = strip_code_spans(&t);
    t = strip_links(&t);
    for marker in ["**", "__", "~~", "*", "_"] {
        t = strip_pairs(&t, marker);
    }
    t = strip_html_tags(&t);
    restore_escapes(&t)
}

/// 私有区起点：`\*` → `U+E02A`。ASCII 标点都在 0x20–0x7E，加 0xE000 不会撞车。
const ESCAPE_BASE: u32 = 0xE000;

fn protect_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(&next) = chars.peek()
            && next.is_ascii_punctuation()
        {
            chars.next();
            let masked = char::from_u32(ESCAPE_BASE + next as u32).unwrap_or(next);
            out.push(masked);
            continue;
        }
        out.push(c);
    }
    out
}

fn restore_escapes(s: &str) -> String {
    s.chars()
        .map(|c| {
            let code = c as u32;
            if (ESCAPE_BASE..ESCAPE_BASE + 0x7F).contains(&code) {
                char::from_u32(code - ESCAPE_BASE).unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

/// 行内代码：`` `x` `` → `x`（没配上对的反引号原样留着）。
fn strip_code_spans(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('`') else {
            break;
        };
        out.push_str(&rest[..start]);
        out.push_str(&after[..end]);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// 成对分隔符：`**粗**` → `粗`。
///
/// 三条否决条件（否则原样保留标记），都是为了不让"看起来像标记的普通文本"丢字符：
///
/// - 中间是空的或两头有空格 → `2 * 3 * 4` 不是斜体；
/// - 开头紧贴 ASCII 字母数字 → `snake_case_name` 里的下划线不是斜体；
/// - 结尾紧贴 ASCII 字母数字 → `foo_bar` 同理。
///
/// 中文两侧不设限，所以 `这是**粗**体` 能正常去掉标记。
fn strip_pairs(s: &str, marker: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let m: Vec<char> = marker.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if !starts_with_at(&chars, i, &m) {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let after = i + m.len();
        if let Some(close) = find_seq(&chars, after, &m) {
            let inner: String = chars[after..close].iter().collect();
            let before_ok = i == 0 || !is_word(chars[i - 1]);
            let after_ok = !chars.get(close + m.len()).is_some_and(|c| is_word(*c));
            let inner_ok = !inner.is_empty()
                && !inner.starts_with(char::is_whitespace)
                && !inner.ends_with(char::is_whitespace);
            if before_ok && after_ok && inner_ok {
                out.push_str(&inner);
                i = close + m.len();
                continue;
            }
        }
        out.extend(m.iter());
        i = after;
    }
    out
}

fn starts_with_at(chars: &[char], at: usize, needle: &[char]) -> bool {
    chars.len() >= at + needle.len() && chars[at..at + needle.len()] == *needle
}

fn find_seq(chars: &[char], from: usize, needle: &[char]) -> Option<usize> {
    (from..=chars.len().saturating_sub(needle.len())).find(|&i| starts_with_at(chars, i, needle))
}

/// 只把 ASCII 字母数字算作"词内"：CJK 字符在 Rust 里也是 alphanumeric，
/// 若一并算进来，`这是**粗**体` 永远去不掉标记。
fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

/// 链接与图片：`[文字](地址)` → `文字`，`![描述](地址)` → `[图] 描述`。
/// 参考式（`[文字][id]`）只保留文字。
fn strip_links(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let is_image = chars[i] == '!' && chars.get(i + 1) == Some(&'[');
        if chars[i] == '[' || is_image {
            let open = if is_image { i + 1 } else { i };
            if let Some(close) = find_char(&chars, open + 1, ']') {
                let label: String = chars[open + 1..close].iter().collect();
                let mut next = close + 1;
                let mut url = String::new();
                if chars.get(next) == Some(&'(')
                    && let Some(paren) = find_char(&chars, next + 1, ')')
                {
                    url = chars[next + 1..paren].iter().collect();
                    next = paren + 1;
                } else if chars.get(next) == Some(&'[')
                    && let Some(bracket) = find_char(&chars, next + 1, ']')
                {
                    next = bracket + 1;
                }
                let label = label.trim();
                if is_image {
                    out.push_str("[图] ");
                    out.push_str(if label.is_empty() { &url } else { label });
                } else {
                    out.push_str(if label.is_empty() { &url } else { label });
                }
                i = next;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn find_char(chars: &[char], from: usize, target: char) -> Option<usize> {
    chars
        .iter()
        .enumerate()
        .skip(from)
        .find(|(_, c)| **c == target)
        .map(|(i, _)| i)
}

/// 简单 HTML 标签（`<br>`、`</p>`、`<span class="x">`）直接去掉；`<` 后不是字母/`/` 的保持原样
/// （`a < b` 这种普通文本不能被吃掉）。
fn strip_html_tags(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<'
            && let Some(len) = tag_len(&chars, i)
        {
            i += len;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// `<` 开头、像标签就返回标签长度（含 `<>`），否则 `None`。
///
/// 判据比"有个 `<` 有个 `>`"严：标签名必须是 `[A-Za-z][A-Za-z0-9-]*`，后面紧跟 `>` 或空白
/// （带属性）。这样 `#include <stdio.h>` 里的 `<stdio.h>` 不会被当标签吃掉——
/// 那行是代码，去掉后半截最容易让人以为预览"丢字"。
fn tag_len(chars: &[char], start: usize) -> Option<usize> {
    let mut i = start + 1;
    if chars.get(i) == Some(&'/') {
        i += 1;
    }
    if !chars.get(i)?.is_ascii_alphabetic() {
        return None;
    }
    while chars
        .get(i)
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '-')
    {
        i += 1;
    }
    match chars.get(i) {
        Some('>') => return Some(i - start + 1),
        Some(c) if c.is_whitespace() => {}
        _ => return None,
    }
    let end = find_char(chars, i, '>')?;
    let len = end - start + 1;
    (len <= 80).then_some(len)
}

/// 表格行：`| a | b |` → `a │ b`；分隔行（`|---|:--:|`）返回 `(空, true)`。
fn table_row(line: &str) -> Option<(String, bool)> {
    let t = line.trim();
    if !t.starts_with('|') || !t.ends_with('|') || t.len() < 2 {
        return None;
    }
    let cells: Vec<&str> = t[1..t.len() - 1].split('|').map(|c| c.trim()).collect();
    let separator = !cells.is_empty()
        && cells
            .iter()
            .all(|c| !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':'));
    if separator {
        return Some((String::new(), true));
    }
    let joined = cells
        .iter()
        .map(|c| inline(c))
        .collect::<Vec<_>>()
        .join(" │ ");
    Some((clip(&joined), false))
}

/// 单行长度上限，防止压缩成一行的 Markdown 把 GDI 撑爆。
fn clip(s: &str) -> String {
    text::truncate_chars(s, MAX_LINE_CHARS)
}

fn is_fence(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

/// ATX 标题：`#`…`######` + 空格。返回 (去掉标记的文本, 级别)。
fn heading(line: &str) -> Option<(String, usize)> {
    let t = line.trim_start();
    let hashes = t.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &t[hashes..];
    // "##没有空格" 不是标题（GitHub 也不认），避免把 #include 之类误判。
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    let text = rest.trim().trim_end_matches('#').trim_end();
    Some((clip(text), hashes))
}

/// 引用：`>` 开头，去掉一个 `>` 与前导空格。
fn quote(line: &str) -> Option<String> {
    let t = line.trim_start();
    let rest = t.strip_prefix('>')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest).to_string())
}

/// 分隔线：同一字符 `-` / `*` / `_` 至少 3 个（允许中间夹空格）。
fn is_rule(line: &str) -> bool {
    let t = line.trim();
    if t.len() < 3 {
        return false;
    }
    let Some(first) = t.chars().next() else {
        return false;
    };
    if !matches!(first, '-' | '*' | '_') {
        return false;
    }
    t.chars().all(|c| c == first || c == ' ')
}

/// 列表项：`- ` / `* ` / `+ ` / `1. ` / `1) `。返回去掉标记的正文。
fn list_item(line: &str) -> Option<String> {
    let t = line.trim_start();
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(marker) {
            return Some(rest.to_string());
        }
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        let rest = &t[digits..];
        if let Some(r) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return Some(r.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styles_of(src: &[&str]) -> Vec<LineStyle> {
        layout(src).1
    }

    fn texts_of(src: &[&str]) -> Vec<String> {
        layout(src).0
    }

    #[test]
    fn headings_get_their_level() {
        let t = texts_of(&["# 一级", "## 二级", "### 三级", "#### 四级"]);
        assert_eq!(t[0], "一级");
        assert_eq!(t[1], "二级");
        assert_eq!(t[2], "三级");
        assert_eq!(t[3], "四级"); // 4–6 级统一按三级样式
        assert_eq!(
            styles_of(&["# 一级", "## 二级", "### 三级", "#### 四级"]),
            vec![
                LineStyle::Heading1,
                LineStyle::Heading2,
                LineStyle::Heading3,
                LineStyle::Heading3
            ]
        );
    }

    #[test]
    fn hash_without_space_is_not_a_heading() {
        // 代码里常见 `#include <stdio.h>`，绝不能当标题
        assert_eq!(styles_of(&["#include <stdio.h>"]), vec![LineStyle::Normal]);
        assert_eq!(texts_of(&["#include <stdio.h>"])[0], "#include <stdio.h>");
    }

    #[test]
    fn fence_toggles_code_and_hides_markers() {
        let src = ["前", "```rust", "fn main() {}", "# 不是标题", "```", "后"];
        let (texts, styles) = layout(&src);
        assert_eq!(texts, vec!["前", "fn main() {}", "# 不是标题", "后"]);
        assert_eq!(
            styles,
            vec![
                LineStyle::Normal,
                LineStyle::Code,
                LineStyle::Code,
                LineStyle::Normal
            ]
        );
    }

    #[test]
    fn quote_list_and_rule() {
        let (texts, styles) = layout(&["> 引用", "- 第一项", "1. 有序项", "---"]);
        assert_eq!(texts[0], "▎ 引用");
        assert_eq!(texts[1], "• 第一项");
        assert_eq!(texts[2], "• 有序项");
        assert_eq!(texts[3].chars().count(), RULE_CHARS);
        assert_eq!(
            styles,
            vec![
                LineStyle::Quote,
                LineStyle::List,
                LineStyle::List,
                LineStyle::Rule
            ]
        );
    }

    #[test]
    fn plain_text_passes_through_unchanged() {
        let src = ["普通一行", "", "  缩进也算普通"];
        assert_eq!(texts_of(&src), vec!["普通一行", "", "  缩进也算普通"]);
        assert!(styles_of(&src).iter().all(|s| *s == LineStyle::Normal));
    }

    #[test]
    fn long_line_is_clipped() {
        let long = "x".repeat(MAX_LINE_CHARS + 500);
        let texts = texts_of(&[long.as_str()]);
        // truncate_chars 保留 MAX_LINE_CHARS 个字符再加一个省略号
        assert_eq!(texts[0].chars().count(), MAX_LINE_CHARS + 1);
        assert!(texts[0].ends_with('…'));
    }

    #[test]
    fn line_count_matches_style_count() {
        let src = ["# t", "", "```", "code", "```", "- a", "> b", "end"];
        let (texts, styles) = layout(&src);
        assert_eq!(texts.len(), styles.len());
        assert_eq!(texts.len(), 6); // 8 行源文本里两个围栏行被吃掉
    }

    #[test]
    fn inline_markup_is_stripped_but_text_survives() {
        // 用户要的是"渲染后的格式"，不是满屏 `**` 和反引号
        assert_eq!(inline("这是 **粗体** 和 *斜体*"), "这是 粗体 和 斜体");
        assert_eq!(inline("用 `cargo test` 跑"), "用 cargo test 跑");
        assert_eq!(inline("[文档](https://x.dev) 在这"), "文档 在这");
        assert_eq!(inline("![架构图](a.png)"), "[图] 架构图");
        assert_eq!(inline("~~删掉~~ 保留"), "删掉 保留");
        assert_eq!(inline("转义 \\*星号\\* 不处理"), "转义 *星号* 不处理");
        assert_eq!(inline("<b>粗</b> 和 <br>换行"), "粗 和 换行");
    }

    #[test]
    fn inline_keeps_lonely_markers_and_math() {
        // 落单的分隔符不能吃：`2 * 3 * 4` 不是强调，`a < b` 不是标签
        assert_eq!(inline("2 * 3 * 4"), "2 * 3 * 4");
        assert_eq!(
            inline("文件 snake_case_name 别动"),
            "文件 snake_case_name 别动"
        );
        assert_eq!(inline("a < b 且 c > d"), "a < b 且 c > d");
        assert_eq!(inline("没配对的 `反引号"), "没配对的 `反引号");
    }

    #[test]
    fn task_list_items_get_checkbox() {
        let t = texts_of(&["- [ ] 待办", "- [x] 已完成", "- 普通项"]);
        assert_eq!(t, vec!["• ☐ 待办", "• ☑ 已完成", "• 普通项"]);
    }

    #[test]
    fn table_rows_are_reflowed_without_pipes() {
        let (texts, styles) = layout(&["| 名字 | 值 |", "|:---|---:|", "| a | 1 |"]);
        assert_eq!(texts[0], "名字 │ 值");
        assert_eq!(texts[1].chars().count(), RULE_CHARS); // 分隔行画成分隔线
        assert_eq!(texts[2], "a │ 1");
        assert_eq!(
            styles,
            vec![LineStyle::Table, LineStyle::Rule, LineStyle::Table]
        );
    }

    #[test]
    fn code_block_keeps_markup_verbatim() {
        // 代码里的 `**` 就是两个星号，不能被当成强调去掉
        let (texts, _) = layout(&["```", "let x = **y;", "```"]);
        assert_eq!(texts, vec!["let x = **y;"]);
    }

    #[test]
    fn renders_real_file_end_to_end() {
        let path = crate::testutil::write_file(
            "markdown",
            "readme.md",
            "# 标题\n\n- item\n\n```\ncode\n```\n".as_bytes(),
        );
        let canvas = render(&path, true).expect("markdown 渲染应成功");
        match canvas {
            Canvas::Text(doc) => {
                // 围栏行不显示，其余原样（含空行）
                assert_eq!(doc.lines, vec!["标题", "", "• item", "", "code"]);
                let styles = doc.styles.expect("Markdown 必须带样式");
                assert_eq!(styles[0], LineStyle::Heading1);
                assert_eq!(styles[2], LineStyle::List);
                assert_eq!(styles[4], LineStyle::Code);
                assert_eq!(doc.total_lines, 7); // 统计的是源文件行数（尾换行不多算一行）
            }
            _ => panic!("markdown 应该产出 Text"),
        }
    }
}
