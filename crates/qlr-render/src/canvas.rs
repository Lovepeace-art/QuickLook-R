//! 渲染结果类型。**不依赖 `windows`**：这一层只产出像素或文本行，
//! 上屏由 `qlr-platform` 负责（`StretchDIBits` / `DrawTextW`）。
//!
//! 这么切的理由：渲染是唯一需要跨平台单测、也是最需要被基准脚本直接调用的部分；
//! 把它和窗口解耦之后，`cargo test` 和 `scripts/bench` 都能在无窗口环境跑。

use qlr_core::sheet::SheetDoc;

/// 一张已就绪、可直接贴到窗口上的位图。像素格式固定 `BGRA8`（Windows 原生顺序，
/// 省掉上屏前的一次 swizzle）。
#[derive(Clone)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    /// 长度必须等于 `width * height * 4`。
    pub bgra: Vec<u8>,
}

// 手写 Debug：`bgra` 是几百 MB 级别的缓冲，派生 Debug 会在断言失败时把内存全打印出来。
impl std::fmt::Debug for Bitmap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bitmap")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.bgra.len())
            .finish()
    }
}

impl Bitmap {
    pub fn new(width: u32, height: u32, bgra: Vec<u8>) -> Self {
        debug_assert_eq!(
            bgra.len(),
            width as usize * height as usize * 4,
            "BGRA 缓冲长度必须等于 w*h*4"
        );
        Self {
            width,
            height,
            bgra,
        }
    }

    pub fn bytes(&self) -> usize {
        self.bgra.len()
    }

    /// 把 RGBA8 就地转成 BGRA8（就地交换 R/B，零分配）。
    pub fn from_rgba(width: u32, height: u32, mut rgba: Vec<u8>) -> Self {
        for px in rgba.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        Self::new(width, height, rgba)
    }
}

/// 一行的排版样式。
///
/// 这一层只表达**语义**（这是标题、这是代码），不表达像素（字号/颜色/字体面）。
/// 像素映射在 `qlr-platform` 里做：渲染层不依赖 `windows`，才能跨平台单测。
/// 加这个枚举是为了 Markdown：同一份文本里不同行要有不同观感。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineStyle {
    #[default]
    Normal,
    Heading1,
    Heading2,
    Heading3,
    Code,
    Quote,
    List,
    Rule,
    /// 表格行（1.0.5）：`|` 已去掉、单元格用 `│` 分隔，上屏用等宽字体。
    Table,
}

impl LineStyle {
    /// 调试/日志用的短名。
    pub fn name(self) -> &'static str {
        match self {
            LineStyle::Normal => "normal",
            LineStyle::Heading1 => "h1",
            LineStyle::Heading2 => "h2",
            LineStyle::Heading3 => "h3",
            LineStyle::Code => "code",
            LineStyle::Quote => "quote",
            LineStyle::List => "list",
            LineStyle::Rule => "rule",
            LineStyle::Table => "table",
        }
    }

    /// 转成小整数，供上屏侧做查表（顺序与枚举声明一致）。
    pub fn as_index(self) -> usize {
        match self {
            LineStyle::Normal => 0,
            LineStyle::Heading1 => 1,
            LineStyle::Heading2 => 2,
            LineStyle::Heading3 => 3,
            LineStyle::Code => 4,
            LineStyle::Quote => 5,
            LineStyle::List => 6,
            LineStyle::Rule => 7,
            LineStyle::Table => 8,
        }
    }

    /// 样式种类数（上屏侧按这个长度建字体表）。
    pub const COUNT: usize = 9;
}

/// 代码着色里的词法类别（1.0.8）。只有这几类：够像 VSCode，又不用引 tree-sitter。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Token {
    /// 普通字符（标识符、括号、运算符、空白……）。
    #[default]
    Plain,
    /// 语言关键字（`fn` / `let` / `if` / `class` …）与 C 系 `#include`。
    Keyword,
    /// 字符串 / 字符字面量（含未闭合的）。
    Str,
    /// 注释（行注释与块注释）。
    Comment,
    /// 数字字面量（十/十六进制、小数）。
    Number,
}

impl Token {
    /// 调试/日志用的短名。
    pub fn name(self) -> &'static str {
        match self {
            Token::Plain => "plain",
            Token::Keyword => "keyword",
            Token::Str => "str",
            Token::Comment => "comment",
            Token::Number => "number",
        }
    }

    /// 转成小整数（与上屏侧 `TextStyle` 的 token 段对齐用）。
    pub fn as_index(self) -> usize {
        match self {
            Token::Plain => 0,
            Token::Keyword => 1,
            Token::Str => 2,
            Token::Comment => 3,
            Token::Number => 4,
        }
    }

    pub const COUNT: usize = 5;
}

/// 行内一段带词法类别的文字。切分保证 `spans` 拼起来等于原来那一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub token: Token,
}

/// 文本类内容的排版结果。只保留**要显示的行**，不是整个文件。
#[derive(Clone, Debug, Default)]
pub struct TextDoc {
    pub lines: Vec<String>,
    /// 每行的样式，与 `lines` 等长；`None` 表示整篇同一套样式（普通文本）。
    /// 用 `Option` 而不是恒定的 `Vec`，是为了让纯文本路径零额外分配。
    pub styles: Option<Vec<LineStyle>>,
    /// 代码着色结果（1.0.8）：外层与 `lines` 等长，内层是这一行的 token 段。
    /// `None` 表示不是代码，整行同一套样式。与 `styles` 互斥（代码不折行、无行级样式）。
    pub spans: Option<Vec<Vec<Span>>>,
    /// 文件里一共有多少行（可能远大于 `lines.len()`）。
    pub total_lines: usize,
    /// 是否因为超出显示预算而只截取了开头一段。
    pub truncated: bool,
    /// 解码时用了哪种编码（日志/UI 提示用，例如 `utf-8` / `utf-8 (lossy)`）。
    pub encoding: &'static str,
}

/// 一次渲染的产物。
#[derive(Clone, Debug)]
pub enum Canvas {
    Bitmap(Bitmap),
    Text(TextDoc),
    /// 电子表格：只带数据，不带像素（绘制在 qlr-platform::window::sheet，可滚动/可翻 sheet）。
    Sheet(Box<SheetDoc>),
}

impl Canvas {
    /// 统计"明显不是白底"的像素数量（每 4 像素取 1 个，够看出内容在不在）。
    ///
    /// 用途：日志里能一眼区分"渲染出来就是空白"和"渲染有内容但没画到屏幕上"。
    pub fn ink_pixels(&self) -> usize {
        match self {
            Canvas::Bitmap(b) => b
                .bgra
                .chunks_exact(16)
                .filter(|px| px[0] < 200 || px[1] < 200 || px[2] < 200)
                .count(),
            Canvas::Text(t) => t.lines.len(),
            Canvas::Sheet(d) => d.sheet().map(|s| s.cells.len()).unwrap_or(0),
        }
    }

    /// 像素尺寸（文本/表格返回 0×0，日志里用）。
    pub fn dimensions(&self) -> (u32, u32) {
        match self {
            Canvas::Bitmap(b) => (b.width, b.height),
            Canvas::Text(_) | Canvas::Sheet(_) => (0, 0),
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Canvas::Bitmap(_) => "bitmap",
            Canvas::Text(_) => "text",
            Canvas::Sheet(_) => "sheet",
        }
    }

    /// 粗略的驻留字节数，写日志与基准用（不是精确分配量）。
    pub fn approx_bytes(&self) -> usize {
        match self {
            Canvas::Bitmap(b) => b.bytes(),
            Canvas::Text(t) => t.lines.iter().map(|l| l.len()).sum(),
            Canvas::Sheet(d) => d
                .sheets
                .iter()
                .map(|s| {
                    s.cells.iter().map(|c| c.text.len() + 48).sum::<usize>()
                        + s.col_widths.len() * 4
                        + s.row_heights.len() * 4
                        + s.merges.len() * 16
                })
                .sum(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_to_bgra_swaps_channels_in_place() {
        // 一个 2x1 的图：像素 (R,G,B,A) = (10,20,30,255) 与 (1,2,3,4)
        let rgba = vec![10, 20, 30, 255, 1, 2, 3, 4];
        let bmp = Bitmap::from_rgba(2, 1, rgba);
        assert_eq!(bmp.width, 2);
        assert_eq!(bmp.height, 1);
        assert_eq!(bmp.bgra, vec![30, 20, 10, 255, 3, 2, 1, 4]);
        assert_eq!(bmp.bytes(), 8);
    }

    #[test]
    fn canvas_reports_kind_and_size() {
        let bmp = Bitmap::new(1, 1, vec![0, 0, 0, 0]);
        let c = Canvas::Bitmap(bmp);
        assert_eq!(c.kind_name(), "bitmap");
        assert_eq!(c.approx_bytes(), 4);

        let t = Canvas::Text(TextDoc {
            lines: vec!["abc".into(), "de".into()],
            styles: None,
            spans: None,
            total_lines: 2,
            truncated: false,
            encoding: "utf-8",
        });
        assert_eq!(t.kind_name(), "text");
        assert_eq!(t.approx_bytes(), 5);
    }

    #[test]
    fn line_style_index_is_dense_and_unique() {
        let all = [
            LineStyle::Normal,
            LineStyle::Heading1,
            LineStyle::Heading2,
            LineStyle::Heading3,
            LineStyle::Code,
            LineStyle::Quote,
            LineStyle::List,
            LineStyle::Rule,
            LineStyle::Table,
        ];
        assert_eq!(all.len(), LineStyle::COUNT);
        for (i, s) in all.iter().enumerate() {
            assert_eq!(
                s.as_index(),
                i,
                "索引必须与声明顺序一致（上屏侧查表依赖它）"
            );
        }
        assert_eq!(LineStyle::default(), LineStyle::Normal);
    }
}
