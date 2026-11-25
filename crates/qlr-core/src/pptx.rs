//! 演示文稿（pptx）的**纯数据模型**：每张幻灯片上的绝对定位图形 + 已排版文字。
//!
//! 与 `docx.rs`/`sheet.rs` 同一套分层理由：
//! - `qlr-render::pptx` 负责"文件 → 模型"（解 zip、扫 OOXML、颜色/占位符几何、折行）；
//! - `qlr-platform::window::pptx` 负责"模型 → 像素"（GDI 画白/彩色幻灯片、文字、图片、表格）；
//! - 两边共用同一份几何（EMU→像素、shape 矩形、行坐标），否则"量的位置"和"画的位置"
//!   一旦不一致就会看到"文字跑到框外"这种只有截图才看得见的 bug（CONVENTIONS #38）。
//!
//! 这一层**不做任何 IO、不依赖 `windows`、不做排版计算**，可以在 Linux 上单测。
//! 折行/定位在 `qlr-render::pptx`（需要注入字体度量），绘制在 `qlr-platform::window::pptx`。
//!
//! 与 docx 的差别：幻灯片是**绝对坐标**（每个图形自带 xfrm），没有"分页/流式"排版；
//! 所以模型里每个 shape 直接带像素矩形，文字也已经折好行（`Vec<Para>`，line 的 rect 是绝对坐标）。

use std::path::PathBuf;

use crate::docx::{ImageSlot, Line, Para, Rect, Run};

/// 一个文本框 / 带文字的自动图形。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextBox {
    /// 整块矩形（像素，幻灯片坐标系）；平台按这个裁剪（不溢出到隔壁元素）。
    pub rect: Rect,
    /// 段落列表（每段若干行，行带绝对坐标与对齐）。
    pub paras: Vec<Para>,
    /// 块底色（`spPr` 的 `solidFill`），`None` = 透明。
    pub fill: Option<u32>,
    /// 边框色（`spPr` 的 `ln`），`None` = 无边框。
    pub line: Option<u32>,
}

/// 一张图片（`p:pic`）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Picture {
    pub rect: Rect,
    /// `PptxDoc::images` 的下标。
    pub image: u32,
    /// 替代文字（解码失败时显示）。
    pub alt: String,
}

/// 无文字形状的几何类别（`a:prstGeom` 的 `prst`，仅取常见几种）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RectGeom {
    /// 普通矩形（默认；也作为未知 `prst` 的兜底）。
    #[default]
    Rect,
    /// 圆角矩形（`roundRect`）。
    RoundRect,
    /// 椭圆 / 圆（`ellipse` / `oval`）。
    Ellipse,
}

/// 一个纯色块 / 无文字的形状（`p:sp` 无文字但有填充，或 `p:cxnSp` 直线）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FillRect {
    pub rect: Rect,
    pub fill: Option<u32>,
    pub line: Option<u32>,
    /// 形状几何（默认矩形）。
    pub geom: RectGeom,
    /// 整形状旋转角（度，整数；0 = 不旋转）。
    pub rot_deg: i32,
}

/// 表格单元格（文字已折行，`rect` 是绝对坐标）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableCell {
    pub rect: Rect,
    pub paras: Vec<Para>,
    pub fill: Option<u32>,
}

/// 表格一行。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableRow {
    pub rect: Rect,
    pub cells: Vec<TableCell>,
}

/// 表格（`p:graphicFrame` 里的 `a:tbl`）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableShape {
    pub rect: Rect,
    /// 列 x 边界偏移（相对 `rect.x`），长度 = 列数+1。
    pub col_x: Vec<i32>,
    pub rows: Vec<TableRow>,
    pub borders: bool,
}

/// 图表类型（渲染用；够画常见柱/折线/饼即可）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChartKind {
    /// 普通柱状图（含分组/单系列）。
    Bar,
    /// 堆叠柱。
    BarStacked,
    /// 百分比堆叠柱。
    BarPercent,
    /// 折线图。
    Line,
    /// 饼图。
    Pie,
}

/// 一个图表系列：名称 + 数值 + 主色。
#[derive(Clone, Debug, PartialEq)]
pub struct ChartSeries {
    pub name: String,
    pub values: Vec<f64>,
    /// 系列主色 `0xRRGGBB`（取自 `spPr` 首个颜色；缺省用内置调色板）。
    pub color: u32,
}

/// 一个原生图表里"预览真正会画"的数据（标题、类目、系列值）。
#[derive(Clone, Debug, PartialEq)]
pub struct Chart {
    pub kind: ChartKind,
    pub title: String,
    pub categories: Vec<String>,
    pub series: Vec<ChartSeries>,
    /// 数值格式码（如 `0%`），用于纵轴/数据标签显示。
    pub number_format: String,
}

/// 图表占位形状（`p:graphicFrame` 里引用 `c:chart` 的 part）。
#[derive(Clone, Debug, PartialEq)]
pub struct ChartShape {
    pub rect: Rect,
    pub chart: Chart,
}

/// 幻灯片上的一个元素。顺序即 z 序（先画的在下）。
#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    Text(TextBox),
    Picture(Picture),
    Rect(FillRect),
    Table(TableShape),
    Chart(ChartShape),
}

impl Shape {
    pub fn rect(&self) -> Rect {
        match self {
            Shape::Text(t) => t.rect,
            Shape::Picture(p) => p.rect,
            Shape::Rect(r) => r.rect,
            Shape::Table(t) => t.rect,
            Shape::Chart(c) => c.rect,
        }
    }
}

/// 一张幻灯片。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Slide {
    pub shapes: Vec<Shape>,
    /// 背景色 `0xRRGGBB`；`None` = 白底。
    pub bg: Option<u32>,
    /// 第几页（1 起，界面页码框用）。
    pub number: u32,
}

/// 整份演示文稿模型。
#[derive(Clone, Debug, Default)]
pub struct PptxDoc {
    pub slides: Vec<Slide>,
    /// 幻灯片像素尺寸（`p:sldSz` 换算；常见 1280×720 或 960×720）。
    pub width: u32,
    pub height: u32,
    /// 共享图片表（`Picture.image` 是这里面的下标）。
    pub images: Vec<ImageSlot>,
    /// 源文件路径（惰性解码时重开此 zip 读取图片字节；None 表示无法回读，图片将不显示）。
    pub source: Option<PathBuf>,
    /// 标题（`docProps/core.xml` 的 `dc:title`）。
    pub title: Option<String>,
}

impl PptxDoc {
    /// 第 `index` 张（越界返回 `None`）。
    pub fn slide(&self, index: usize) -> Option<&Slide> {
        self.slides.get(index)
    }

    pub fn len(&self) -> usize {
        self.slides.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slides.is_empty()
    }

    /// 粗略驻留字节数（日志/基准用，不是精确分配量）。
    pub fn approx_bytes(&self) -> usize {
        fn para_bytes(paras: &[Para]) -> usize {
            paras
                .iter()
                .map(|p| {
                    p.lines
                        .iter()
                        .map(|l: &Line| {
                            l.runs
                                .iter()
                                .map(|r: &Run| r.text.len() + 32)
                                .sum::<usize>()
                                + 32
                        })
                        .sum::<usize>()
                        + 32
                })
                .sum()
        }
        let shapes: usize = self
            .slides
            .iter()
            .map(|s| {
                s.shapes
                    .iter()
                    .map(|sh| match sh {
                        Shape::Text(t) => para_bytes(&t.paras) + 48,
                        Shape::Picture(_) => 48,
                        Shape::Rect(_) => 16,
                        Shape::Table(t) => {
                            let mut n = t.col_x.len() * 4 + 48;
                            for row in &t.rows {
                                for cell in &row.cells {
                                    n += para_bytes(&cell.paras) + 48;
                                }
                            }
                            n
                        }
                        Shape::Chart(c) => {
                            c.chart.title.len()
                                + c.chart
                                    .categories
                                    .iter()
                                    .map(|s| s.len() + 8)
                                    .sum::<usize>()
                                + c.chart
                                    .series
                                    .iter()
                                    .map(|s| s.name.len() + 8 + s.values.len() * 8)
                                    .sum::<usize>()
                                + 64
                        }
                    })
                    .sum::<usize>()
                    + 32
            })
            .sum();
        let imgs: usize = self.images.iter().map(|i| i.approx_bytes()).sum();
        shapes + imgs
    }
}

/// 幻灯片尺寸（解析 `p:sldSz` 得到，给 `natural_size` 与开窗用）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SlideSize {
    pub width: u32,
    pub height: u32,
}

impl SlideSize {
    /// 16:9 默认（1280×720，96 DPI）。
    pub fn widescreen() -> Self {
        Self {
            width: 1280,
            height: 720,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_doc_reports_no_slides() {
        let d = PptxDoc::default();
        assert!(d.is_empty());
        assert_eq!(d.len(), 0);
        assert!(d.slide(0).is_none());
    }

    #[test]
    fn slide_access_is_bounded() {
        let d = PptxDoc {
            slides: vec![Slide::default(), Slide::default()],
            width: 1280,
            height: 720,
            ..PptxDoc::default()
        };
        assert_eq!(d.len(), 2);
        assert!(!d.is_empty());
        assert!(d.slide(1).is_some());
        assert!(d.slide(2).is_none());
    }

    #[test]
    fn shape_rect_matches_variant() {
        let r = Rect::new(1, 2, 3, 4);
        assert_eq!(
            Shape::Rect(FillRect {
                rect: r,
                fill: None,
                line: None,
                ..Default::default()
            })
            .rect(),
            r
        );
        assert_eq!(
            Shape::Picture(Picture {
                rect: r,
                image: 0,
                alt: String::new()
            })
            .rect(),
            r
        );
    }

    #[test]
    fn approx_bytes_counts_text_and_images() {
        let d = PptxDoc {
            slides: vec![Slide {
                shapes: vec![Shape::Text(TextBox {
                    rect: Rect::default(),
                    paras: vec![Para {
                        lines: vec![Line {
                            runs: vec![Run {
                                text: "abcdefghij".into(),
                                ..Run::default()
                            }],
                            ..Line::default()
                        }],
                        ..Para::default()
                    }],
                    ..TextBox::default()
                })],
                ..Slide::default()
            }],
            images: vec![ImageSlot::new(2, 2, "0123456789abcdef")],
            ..PptxDoc::default()
        };
        assert!(d.approx_bytes() >= 10 + 16);
    }

    #[test]
    fn widescreen_is_16_by_9() {
        let s = SlideSize::widescreen();
        assert_eq!(s.width, 1280);
        assert_eq!(s.height, 720);
    }
}
