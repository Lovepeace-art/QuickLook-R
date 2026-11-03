//! 纯文本折行：把"一行太长"的文本按可用像素宽度切成多行。
//!
//! 为什么放在 `qlr-render`（不碰 windows）：折行规则本身是纯逻辑，
//! 只要注入一个 `measure`（给一段文字、返回它有多宽）就能测；
//! 真实的测量由 `qlr-platform::window::measure_text` 用 GDI 完成。
//!
//! 为什么只做"窗口"而不是整篇：26 MB 的日志有 12 万行，
//! 每次重画都折行全文会卡；只折"当前看得见的那几十行"。

/// 一行最多折成多少行（防止一行 2 MB 的压缩 JSON 把内存吃光）。
pub const MAX_ROWS_PER_LINE: usize = 400;

/// 一次折行的结果窗口。
#[derive(Debug, Default, Clone)]
pub struct WrapWindow {
    /// 折好的显示行（可以直接交给 `DrawTextW`）。
    pub rows: Vec<String>,
    /// 这次窗口吃掉了多少**原始行**（翻页时用它前进）。
    pub consumed_raw: usize,
}

/// 从原始第 `start` 行开始，折出能填满 `max_rows` 行的窗口。
///
/// `measure`：给定一段文字返回像素宽度（由平台层提供，测试里注入定宽实现）。
pub fn wrap_window(
    lines: &[String],
    start: usize,
    max_rows: usize,
    width: u32,
    measure: &dyn Fn(&str) -> u32,
) -> WrapWindow {
    let mut out = WrapWindow::default();
    if max_rows == 0 || width == 0 {
        return out;
    }
    let start = start.min(lines.len());
    for line in &lines[start..] {
        if out.rows.len() >= max_rows {
            break;
        }
        let before = out.rows.len();
        push_wrapped(line, width, measure, &mut out.rows, max_rows);
        // 这一原始行至少贡献一行（空行也算），计数才不会卡在同一个位置
        if out.rows.len() > before {
            out.consumed_raw += 1;
        }
    }
    out
}

/// 把单行折进 `out`，最多再加 `limit` 行。
pub fn push_wrapped(
    line: &str,
    width: u32,
    measure: &dyn Fn(&str) -> u32,
    out: &mut Vec<String>,
    limit: usize,
) {
    let budget = limit.saturating_sub(out.len());
    if budget == 0 {
        return;
    }
    // 空行照原样保留（它占的是一行高度）
    if line.is_empty() {
        out.push(String::new());
        return;
    }
    // 整行放得下就别折腾
    if measure(line) <= width {
        out.push(line.to_owned());
        return;
    }

    let mut rest = line;
    let mut produced = 0usize;
    while !rest.is_empty() && produced < budget && produced < MAX_ROWS_PER_LINE {
        let chunk = fit_prefix(rest, width, measure);
        // fit_prefix 返回字节数，保证至少前进一个字符，否则这里会死循环 —— 加一道保险
        let advance = chunk.max(char_len(rest));
        let (head, tail) = rest.split_at(advance.min(rest.len()));
        out.push(head.trim_end().to_owned());
        rest = tail.trim_start();
        produced += 1;
    }
    // 还有剩余（行太长被 MAX_ROWS_PER_LINE 截住）：留个提示，别让人以为文件就这些内容
    if !rest.is_empty() {
        if out.len() < limit {
            let mut hint = rest.chars().take(80).collect::<String>();
            hint.push('…');
            out.push(hint);
        } else if let Some(last) = out.last_mut() {
            last.push('…');
        }
    }
}

/// 返回 `s` 里"按宽度能放下"的最长前缀（以字节长度表示），并尽量在空白/汉字边界断开。
///
/// **量宽次数是这里的成本命门**：旧实现从行首逐字符 `measure` 到超宽为止，
/// 一段 340 字的段落要量 340 次 GDI；实测一屏 29 行 = 1359 次量宽 / 12 ms，
/// 每滚一格都付一遍。宽度对前缀长度单调，所以改成**二分**（≈log₂n 次），
/// 再在一次横扫里找最后一个可断点（纯字符扫描，不需要量宽）。
fn fit_prefix(s: &str, width: u32, measure: &dyn Fn(&str) -> u32) -> usize {
    if measure(s) <= width {
        return s.len();
    }
    // `lo`：已知能放下的最长字符边界（初始 0，即"一个都放不下"也合法）；
    // `hi`：已知放不下的边界（初始整串，上面刚量过确实放不下）。
    let mut lo = 0usize;
    let mut hi = s.len();
    while hi - lo > 1 {
        let mut mid = lo + (hi - lo) / 2;
        // 对齐到字符边界（往前退，保证 `s[..mid]` 合法）
        while mid > lo && !s.is_char_boundary(mid) {
            mid -= 1;
        }
        if mid == lo {
            break;
        }
        if measure(&s[..mid]) <= width {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    // 在放得下的前缀里找最后一个可断点（空白之后、或中日韩字符之后）
    let mut last_break = 0usize;
    for (idx, ch) in s[..lo].char_indices() {
        if ch == ' ' || ch == '\t' || is_wide(ch) {
            last_break = idx + ch.len_utf8();
        }
    }
    if last_break > 0 {
        return last_break;
    }
    if lo == 0 {
        // 一个字符都放不下：硬切，至少放一个字符（否则调用方会原地打转）
        return char_len(s);
    }
    // 一个可断点都没有（比如一长串 ASCII 无空格）：硬切，连那个超宽的字符一起交出，
    // 与旧实现 `acc_end` 的口径一致（调用方会再 trim）。
    lo + s[lo..].chars().next().map(char::len_utf8).unwrap_or(0)
}

fn char_len(s: &str) -> usize {
    s.chars().next().map(char::len_utf8).unwrap_or(0)
}

/// 宽字符（中日韩、全角标点）：它们之间天然可以断行。
fn is_wide(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x115F      // 韩文字母
        | 0x2E80..=0x303E    // 部首、康熙、CJK 标点
        | 0x3041..=0x33FF    // 平假名/片假名/注音/CJK 兼容
        | 0x3400..=0x4DBF    // CJK 扩展 A
        | 0x4E00..=0x9FFF    // CJK 基本区
        | 0xA000..=0xA4CF    // 彝文
        | 0xAC00..=0xD7A3    // 韩文音节
        | 0xF900..=0xFAFF    // CJK 兼容汉字
        | 0xFE30..=0xFE4F    // CJK 兼容形式
        | 0xFF00..=0xFF60    // 全角字符
        | 0xFFE0..=0xFFE6)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用测量器：ASCII 一个字符 8 px，宽字符 16 px。
    fn mono(s: &str) -> u32 {
        s.chars().map(|c| if is_wide(c) { 16 } else { 8 }).sum()
    }

    fn wrap(line: &str, width: u32) -> Vec<String> {
        let mut out = Vec::new();
        push_wrapped(line, width, &mono, &mut out, MAX_ROWS_PER_LINE);
        out
    }

    #[test]
    fn short_line_is_untouched() {
        let rows = wrap("hello", 800);
        assert_eq!(rows, vec!["hello"]);
    }

    #[test]
    fn breaks_at_spaces() {
        // 80 px 放得下 10 个 ASCII 字符
        let rows = wrap("aaaa bbbb cccc dddd", 80);
        assert_eq!(rows, vec!["aaaa bbbb", "cccc dddd"]);
        // 拼回去（含被吃掉的空格）应该等于原文
        assert_eq!(rows.join(" "), "aaaa bbbb cccc dddd");
    }

    #[test]
    fn breaks_cjk_every_char() {
        // 每个汉字 16 px，64 px 一行放 4 个
        let rows = wrap("中文折行测试一下", 64);
        assert_eq!(rows, vec!["中文折行", "测试一下"]);
        assert_eq!(rows.concat(), "中文折行测试一下");
    }

    #[test]
    fn hard_breaks_long_ascii_run() {
        let s = "x".repeat(25);
        let rows = wrap(&s, 80); // 每行 10 个
        assert_eq!(rows.len(), 3);
        assert_eq!(rows.concat(), s);
    }

    #[test]
    fn empty_line_stays_empty() {
        assert_eq!(wrap("", 80), vec![""]);
    }

    #[test]
    fn window_counts_consumed_raw_lines() {
        let lines: Vec<String> = (0..100).map(|i| format!("line{i}")).collect();
        let all_short = |_: &str| 10u32;
        let w = wrap_window(&lines, 0, 10, 800, &all_short);
        assert_eq!(w.rows.len(), 10);
        assert_eq!(w.consumed_raw, 10);

        let w2 = wrap_window(&lines, 95, 10, 800, &all_short);
        assert_eq!(w2.rows.len(), 5); // 只剩 5 行
        assert_eq!(w2.consumed_raw, 5);
    }

    #[test]
    fn window_advances_by_fewer_lines_when_wrapping() {
        // 每个原始行 20 个 ASCII 字符（160 px），宽度 80 px → 每行折成 2 行，
        // 所以 10 行的窗口只吃得下 5 个原始行
        let lines: Vec<String> = (0..20).map(|_| "a".repeat(20)).collect();
        let w = wrap_window(&lines, 0, 10, 80, &mono);
        assert_eq!(w.rows.len(), 10);
        assert_eq!(w.consumed_raw, 5);
    }

    #[test]
    fn pathological_single_line_is_capped() {
        let s = "a".repeat(MAX_ROWS_PER_LINE * 20);
        let rows = wrap(&s, 8); // 每行 1 个字符
        assert_eq!(rows.len(), MAX_ROWS_PER_LINE);
        assert!(rows.last().unwrap().ends_with('…'));
    }

    /// 旧的"逐字符量宽"实现，留在测试里当基准：二分版必须与它切出完全相同的行。
    fn fit_prefix_linear(s: &str, width: u32, measure: &dyn Fn(&str) -> u32) -> usize {
        if measure(s) <= width {
            return s.len();
        }
        let mut last_break = 0usize;
        let mut acc_end = 0usize;
        for (idx, ch) in s.char_indices() {
            acc_end = idx + ch.len_utf8();
            if measure(&s[..acc_end]) > width {
                break;
            }
            if ch == ' ' || ch == '\t' || is_wide(ch) {
                last_break = acc_end;
            }
        }
        if last_break > 0 {
            last_break
        } else {
            acc_end.max(char_len(s))
        }
    }

    #[test]
    fn fit_prefix_matches_the_linear_reference() {
        // 二分是"宽度对前缀长度单调"的假设换来的，所以必须逐字节对齐旧实现。
        let no_break = "no-break-汉字混在一起的一长串没有空格的文字".repeat(4);
        let ascii_run = "x".repeat(300);
        let cases = [
            "aaaa bbbb cccc dddd eeee",
            "中文折行测试一下中文折行测试一下",
            "mixed 混排 text with 空格 and CJK 汉字 more words here",
            ascii_run.as_str(),
            no_break.as_str(),
            "emoji 🚀 也算宽字符吗，试试看 🎉🎉 一二三四五六七八九十",
        ];
        for case in cases {
            for width in [8u32, 40, 64, 120, 240, 756, 4000] {
                let want = fit_prefix_linear(case, width, &mono);
                let got = fit_prefix(case, width, &mono);
                assert_eq!(
                    got,
                    want,
                    "width={width} case={:?}（got={got} want={want}）",
                    &case[..case.len().min(20)]
                );
            }
        }
    }

    #[test]
    fn fit_prefix_measures_logarithmically_not_per_char() {
        // 340 字的段落：旧实现每行量 300+ 次，二分版应该只有几十次。
        use std::cell::Cell;
        let calls = Cell::new(0usize);
        let counting = |s: &str| {
            calls.set(calls.get() + 1);
            mono(s)
        };
        let line = "这是一段很长的段落文字，没有换行，用来逼折行算法真的去切分。".repeat(8);
        let mut out = Vec::new();
        push_wrapped(&line, 756, &counting, &mut out, 400);
        assert!(out.len() >= 4, "应折成多行，实际 {}", out.len());
        let per_row = calls.get() as f64 / out.len() as f64;
        assert!(
            per_row <= 24.0,
            "每行量宽 {per_row:.1} 次太多（二分应 ≈ log₂ 字符数）"
        );
    }
}
