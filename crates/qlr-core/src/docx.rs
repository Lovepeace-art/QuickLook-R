//! 文字处理文档（docx）的**纯数据模型**：解析/排版出来的分页内容 + 几何换算。
//!
//! 为什么模型放 `qlr-core` 而不是直接放渲染层（与 `sheet.rs` 同一套理由）：
//! - `qlr-render` 负责"文件 → 模型"（解 zip、扫 OOXML、折行、分页），不能碰 `windows`；
//! - `qlr-platform` 负责"模型 → 像素"（GDI 画白页、段落 run、表格、内嵌图），只依赖 `qlr-core`；
//! - 两边共用同一份几何（页尺寸、边距、行/块坐标），否则"量的位置"和"画的位置"
//!   一旦不一致，就会出现"文字画到页边距外面"这种只有截图才看得见的 bug（CONVENTIONS #38）。
//!
//! 这一层**不做任何 IO、不依赖 `windows`、不做排版计算**，可以在 Linux 上单测。
//! 排版（折行、分页）在 `qlr-render::docx`，因为它需要注入的字体度量（`measure`）；
//! 绘制在 `qlr-platform::window::docx`。

use std::path::PathBuf;
use std::sync::OnceLock;

/// EMU（English Metric Unit，OOXML 内部长度单位）→ 像素（96 DPI）。
///
/// 1 英寸 = 914400 EMU = 96 px，所以 1 px = 9525 EMU。用于图片 `wp:extent` 等。
pub const EMU_PER_PX: i64 = 9_525;

/// twips（1/20 pt）→ 像素。1 pt = 20 twips = 96/72 px，故 1 px = 15 twips。
///
/// 用于页尺寸 `w:pgSz`、页边距 `w:pgMar`、缩进 `w:ind`、表格行高 `w:trHeight` 等。
pub const TWIPS_PER_PX: i64 = 15;

/// EMU → 像素（四舍五入，负数/极小值夹到 0）。
pub fn emu_to_px(emu: i64) -> i32 {
    if emu <= 0 {
        return 0;
    }
    ((emu + EMU_PER_PX / 2) / EMU_PER_PX) as i32
}

/// twips → 像素（四舍五入；负缩进保留符号）。
pub fn twips_to_px(twips: i64) -> i32 {
    let half = TWIPS_PER_PX / 2;
    let v = if twips >= 0 {
        (twips + half) / TWIPS_PER_PX
    } else {
        -((-twips + half) / TWIPS_PER_PX)
    };
    v as i32
}

/// 字号（0.1 pt 单位）→ 物理像素高度（96 DPI，四舍五入，至少 1）。
///
/// `dpt` 全称 "deci-point"：11 pt 存成 `110`，与 `sheet::CellStyle::size_dpt` 同口径。
/// px = dpt/10 * 96/72 = dpt * 96/720 = dpt * 2/15。
pub fn dpt_to_px(size_dpt: u16) -> i32 {
    let px = (i64::from(size_dpt) * 96 + 360) / 720; // +360 = 半个 720，四舍五入
    (px as i32).max(1)
}

/// 段落水平对齐。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HAlign {
    #[default]
    Left,
    Center,
    Right,
    /// 两端对齐（Word 的 `both`；预览里按普通左对齐处理，不强求拉满）。
    Justify,
}

/// 单元格垂直对齐。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VAlign {
    #[default]
    Top,
    Center,
    Bottom,
}

/// 矩形（像素，页面坐标系，左上角为原点；已含页边距偏移）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> i32 {
        self.x.saturating_add(self.w)
    }

    pub fn bottom(&self) -> i32 {
        self.y.saturating_add(self.h)
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }
}

/// 行内一段文字 + 它的字符格式。一段 = Word 的一个 `w:r`（一次格式切换）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// 删除线（`w:strike`）。
    pub strike: bool,
    /// 字号，0.1 pt 单位（见 [`dpt_to_px`]）。
    pub size_dpt: u16,
    /// 文字颜色 `0xRRGGBB`。
    pub color: u32,
    /// 高亮底色 `0xRRGGBB`（`w:highlight`），`None` = 无。
    pub highlight: Option<u32>,
    /// 等宽字体（代码/等宽段）。选中 Consolas，否则用雅黑。
    pub mono: bool,
}

impl Default for Run {
    fn default() -> Self {
        Self {
            text: String::new(),
            bold: false,
            italic: false,
            underline: false,
            strike: false,
            // Word 中文默认正文 10.5 pt（五号）-> 105 dpt
            size_dpt: 105,
            color: 0x00_00_00,
            highlight: None,
            mono: false,
        }
    }
}

impl Run {
    /// 这一 run 的字号像素高度。
    pub fn size_px(&self) -> i32 {
        dpt_to_px(self.size_dpt)
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// 页面上排好版的一行：一串 run + 它在页面里的矩形。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    pub rect: Rect,
    pub runs: Vec<Run>,
}

impl Line {
    /// 这一行的纯文本（日志/命中用，不含格式）。
    pub fn plain(&self) -> String {
        let mut s = String::new();
        for r in &self.runs {
            s.push_str(&r.text);
        }
        s
    }

    pub fn is_empty(&self) -> bool {
        self.runs.iter().all(|r| r.text.is_empty())
    }
}

/// 一个段落：若干行 + 对齐方式 + 段前段后间距（像素）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Para {
    pub rect: Rect,
    pub align: HAlign,
    pub lines: Vec<Line>,
    pub space_before: i32,
    pub space_after: i32,
}

/// 表格单元格：矩形 + 内含块（单元格里可以有段落/嵌套表）+ 合并跨度 + 底色。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableCell {
    pub rect: Rect,
    pub blocks: Vec<Block>,
    /// 横向合并跨度（`w:gridSpan`，≥1）。
    pub colspan: u32,
    /// 纵向合并（`w:vMerge` 的 restart；`continue` 的格子由 restart 那一格覆盖，不单独画）。
    pub rowspan: u32,
    /// 是否被上方的 `vMerge` 吃掉（这些格子不画自己的边框/内容）。
    pub v_merged: bool,
    /// 底色 `0xRRGGBB`（`w:shd w:fill`）。
    pub fill: Option<u32>,
}

/// 表格一行。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableRow {
    pub rect: Rect,
    pub cells: Vec<TableCell>,
}

/// 表格：行集合 + 列边界（相对表格左上角的 x 偏移，长度 = 列数+1）+ 有无边框。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    pub rect: Rect,
    /// 列 x 边界偏移（相对 `rect.x`），`col_x[i]` 是第 i 条竖线的偏移。
    pub col_x: Vec<i32>,
    pub rows: Vec<TableRow>,
    /// 是否画外/内框线（`w:tblBorders` 有则画）。
    pub borders: bool,
}

/// 内嵌图片块：矩形 + `DocDoc::images` 的下标 + 替代文字（解码失败时显示）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageBlock {
    pub rect: Rect,
    pub image: u32,
    pub alt: String,
}

/// 一页里的一个块。顺序即自上而下。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    Para(Para),
    Table(Table),
    /// 分隔线 / 段落边框（`w:pBdr`）。
    Rule(Rect),
    Image(ImageBlock),
}

impl Block {
    pub fn rect(&self) -> Rect {
        match self {
            Block::Para(p) => p.rect,
            Block::Table(t) => t.rect,
            Block::Rule(r) => *r,
            Block::Image(i) => i.rect,
        }
    }
}

/// 一张已解码、并按"页面上的显示尺寸"缩好的位图（RGBA8）。
///
/// 解析阶段就按显示尺寸缩放，避免把一张 4000×3000 的原图整块留在内存里。
#[derive(Clone, Default)]
pub struct DocImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for DocImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.rgba.len())
            .finish()
    }
}

impl PartialEq for DocImage {
    fn eq(&self, other: &Self) -> bool {
        self.width == other.width && self.height == other.height && self.rgba == other.rgba
    }
}

impl Eq for DocImage {}

/// 一张图片的**惰性槽**：解析阶段只记录该图在包内的**条目名** + 显示尺寸；读字节与解码成
/// RGBA 都推迟到该图所在页（docx）/ 幻灯片（pptx）**首次绘制**时
/// （见 `qlr-render::docx::ensure_page_images` / `qlr-render::pptx::ensure_slide_images`）。
///
/// 为什么放 qlr-core：平台层（qlr-platform）只依赖 qlr-core，绘制时需读 `DocImage`；
/// 而读 zip/解码要用 `zip`+`image`（只在 qlr-render）。所以模型只存条目名，读+解函数放
/// qlr-render，由应用在绘制前按页触发，解码结果经 `OnceLock` 缓存回本槽。
/// 这样打开文件时连图片字节都不读（≈零图像内存/耗时），未翻到的页永不读、不解。
pub struct ImageSlot {
    /// 页面上的显示尺寸（EMU 换算后的像素；0 表示未知，解码时按原图尺寸）。
    pub disp_w: u32,
    pub disp_h: u32,
    /// 图片在包（zip）内的**条目名**，如 `word/media/image1.png` / `ppt/media/image1.png`。
    /// 首次绘制时按它重开源文件读取并解码。
    pub part: String,
    /// 惰性解码结果（首次绘制时填充）。
    decoded: OnceLock<DocImage>,
}

impl ImageSlot {
    pub fn new(disp_w: u32, disp_h: u32, part: impl Into<String>) -> Self {
        Self {
            disp_w,
            disp_h,
            part: part.into(),
            decoded: OnceLock::new(),
        }
    }

    pub fn is_decoded(&self) -> bool {
        self.decoded.get().is_some()
    }

    pub fn decoded(&self) -> Option<&DocImage> {
        self.decoded.get()
    }

    /// 只接受第一次设置；已解码则忽略并返回 `false`。
    pub fn set_decoded(&self, img: DocImage) -> bool {
        self.decoded.set(img).is_ok()
    }

    /// 粗略驻留字节：条目名 +（若已解码）RGBA。
    pub fn approx_bytes(&self) -> usize {
        self.part.len() + self.decoded.get().map(|d| d.rgba.len()).unwrap_or(0) + 32
    }
}

impl Default for ImageSlot {
    fn default() -> Self {
        Self {
            disp_w: 0,
            disp_h: 0,
            part: String::new(),
            decoded: OnceLock::new(),
        }
    }
}

impl Clone for ImageSlot {
    fn clone(&self) -> Self {
        let decoded = OnceLock::new();
        if let Some(d) = self.decoded.get() {
            let _ = decoded.set(d.clone());
        }
        Self {
            disp_w: self.disp_w,
            disp_h: self.disp_h,
            part: self.part.clone(),
            decoded,
        }
    }
}

impl std::fmt::Debug for ImageSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageSlot")
            .field("disp", &(self.disp_w, self.disp_h))
            .field("part", &self.part)
            .field("decoded", &self.decoded.get().map(|d| (d.width, d.height)))
            .finish()
    }
}

/// 一页：正文块 + 页眉 + 页脚。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Page {
    pub blocks: Vec<Block>,
    /// 页眉行（Y 坐标是绝对页面坐标，位于上边距之上）。
    pub header: Vec<Line>,
    /// 页脚行。
    pub footer: Vec<Line>,
}

/// 整篇文档模型。
#[derive(Clone, Debug, Default)]
pub struct DocDoc {
    pub pages: Vec<Page>,
    /// 页面宽高（像素，含页边距）。
    pub page_w: u32,
    pub page_h: u32,
    /// 页边距（像素）。
    pub margin_left: i32,
    pub margin_top: i32,
    pub margin_right: i32,
    pub margin_bottom: i32,
    /// 共享图片表（`Block::Image.image` 是这里面的下标）。每张是**惰性槽**：
    /// 解析阶段只存包内条目名 + 显示尺寸，读取与解码推迟到该页首次绘制。
    pub images: Vec<ImageSlot>,
    /// 源文件路径（惰性解码时重开此 zip 读取图片字节；None 表示无法回读，图片将不显示）。
    pub source: Option<PathBuf>,
    /// 文档标题（`core.xml` 的 `dc:title`）。
    pub title: Option<String>,
}

impl DocDoc {
    /// 当前页（越界返回 `None`）。
    pub fn page(&self, index: usize) -> Option<&Page> {
        self.pages.get(index)
    }

    pub fn len(&self) -> usize {
        self.pages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// 正文可用宽度（页宽 − 左右边距，≥1）。
    pub fn content_width(&self) -> i32 {
        (self.page_w as i32 - self.margin_left - self.margin_right).max(1)
    }

    /// 正文可用高度（页高 − 上下边距，≥1）。
    pub fn content_height(&self) -> i32 {
        (self.page_h as i32 - self.margin_top - self.margin_bottom).max(1)
    }

    /// 粗略驻留字节数（日志/基准用，不是精确分配量）。
    pub fn approx_bytes(&self) -> usize {
        fn runs_bytes(runs: &[Run]) -> usize {
            runs.iter().map(|r| r.text.len() + 32).sum()
        }
        fn line_bytes(lines: &[Line]) -> usize {
            lines
                .iter()
                .map(|l| runs_bytes(&l.runs) + 32)
                .sum::<usize>()
        }
        fn block_bytes(b: &Block) -> usize {
            match b {
                Block::Para(p) => line_bytes(&p.lines) + 48,
                Block::Rule(_) => 16,
                Block::Image(_) => 48,
                Block::Table(t) => {
                    let mut n = t.col_x.len() * 4 + 48;
                    for row in &t.rows {
                        for cell in &row.cells {
                            n += cell.blocks.iter().map(block_bytes).sum::<usize>() + 48;
                        }
                    }
                    n
                }
            }
        }
        let body: usize = self
            .pages
            .iter()
            .map(|p| {
                p.blocks.iter().map(block_bytes).sum::<usize>()
                    + line_bytes(&p.header)
                    + line_bytes(&p.footer)
                    + 32
            })
            .sum();
        let imgs: usize = self.images.iter().map(|i| i.approx_bytes()).sum();
        body + imgs
    }
}

/// 页面尺寸/边距（解析 `sectPr` 得到，给 `natural_size` 与开窗用；不含内容）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PageSetup {
    pub page_w: u32,
    pub page_h: u32,
    pub margin_left: i32,
    pub margin_top: i32,
    pub margin_right: i32,
    pub margin_bottom: i32,
}

impl PageSetup {
    /// A4 竖向的默认值（210×297 mm @96dpi ≈ 794×1123 px，四边 1 英寸 = 96 px 边距）。
    /// 文档没写 `sectPr` 时用它兜底。
    pub fn a4() -> Self {
        Self {
            page_w: 794,
            page_h: 1123,
            margin_left: 96,
            margin_top: 96,
            margin_right: 96,
            margin_bottom: 96,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twips_and_emu_convert_to_pixels() {
        // A4 宽 11906 twips ≈ 794 px（Word 的 A4）
        assert_eq!(twips_to_px(11_906), 794);
        // 1 英寸 = 1440 twips = 96 px
        assert_eq!(twips_to_px(1440), 96);
        // 负缩进保留符号
        assert_eq!(twips_to_px(-210), -14);
        // 1 英寸 = 914400 EMU = 96 px
        assert_eq!(emu_to_px(914_400), 96);
        assert_eq!(emu_to_px(0), 0);
        assert_eq!(emu_to_px(-100), 0);
    }

    #[test]
    fn dpt_converts_to_pixel_height() {
        // 10.5 pt（105 dpt）= 14 px
        assert_eq!(dpt_to_px(105), 14);
        // 12 pt（120 dpt）= 16 px
        assert_eq!(dpt_to_px(120), 16);
        // 0 也至少 1 px，不返回 0
        assert_eq!(dpt_to_px(0), 1);
    }

    #[test]
    fn run_default_is_body_text() {
        let r = Run::default();
        assert_eq!(r.size_dpt, 105);
        assert_eq!(r.size_px(), 14);
        assert_eq!(r.color, 0);
        assert!(!r.bold);
        assert!(r.is_empty());
    }

    #[test]
    fn rect_geometry_and_hit_test() {
        let r = Rect::new(10, 20, 100, 40);
        assert_eq!(r.right(), 110);
        assert_eq!(r.bottom(), 60);
        assert!(!r.is_empty());
        assert!(r.contains(10, 20));
        assert!(r.contains(109, 59));
        assert!(!r.contains(110, 20));
        assert!(!r.contains(10, 60));
        assert!(Rect::new(0, 0, 0, 10).is_empty());
    }

    #[test]
    fn line_plain_concatenates_runs() {
        let line = Line {
            rect: Rect::default(),
            runs: vec![
                Run {
                    text: "你好".into(),
                    ..Run::default()
                },
                Run {
                    text: "世界".into(),
                    ..Run::default()
                },
            ],
        };
        assert_eq!(line.plain(), "你好世界");
        assert!(!line.is_empty());
        assert!(Line::default().is_empty());
    }

    #[test]
    fn doc_reports_page_count_and_content_box() {
        let doc = DocDoc {
            pages: vec![Page::default(), Page::default()],
            page_w: 794,
            page_h: 1123,
            margin_left: 96,
            margin_top: 96,
            margin_right: 96,
            margin_bottom: 96,
            ..DocDoc::default()
        };
        assert_eq!(doc.len(), 2);
        assert!(!doc.is_empty());
        assert!(doc.page(1).is_some());
        assert!(doc.page(2).is_none());
        assert_eq!(doc.content_width(), 602);
        assert_eq!(doc.content_height(), 931);
    }

    #[test]
    fn a4_setup_is_sane() {
        let s = PageSetup::a4();
        assert!((s.page_w as i32) > s.margin_left + s.margin_right);
        assert!((s.page_h as i32) > s.margin_top + s.margin_bottom);
    }

    #[test]
    fn approx_bytes_accounts_for_text_and_images() {
        let doc = DocDoc {
            pages: vec![Page {
                blocks: vec![Block::Para(Para {
                    lines: vec![Line {
                        runs: vec![Run {
                            text: "abcdefghij".into(),
                            ..Run::default()
                        }],
                        ..Line::default()
                    }],
                    ..Para::default()
                })],
                ..Page::default()
            }],
            images: vec![ImageSlot::new(2, 2, "0123456789abcdef")],
            ..DocDoc::default()
        };
        // 至少包含文本长度与图片条目名
        assert!(doc.approx_bytes() >= 10 + 16);
    }

    #[test]
    fn image_slot_decodes_lazily() {
        let slot = ImageSlot::new(2, 2, "abc");
        assert!(!slot.is_decoded());
        assert!(slot.decoded().is_none());
        assert_eq!(slot.approx_bytes(), 3 + 32);
        // 第一次设置成功
        assert!(slot.set_decoded(DocImage {
            width: 1,
            height: 1,
            rgba: vec![0; 4],
        }));
        assert!(slot.is_decoded());
        assert_eq!(slot.decoded().unwrap().width, 1);
        // 第二次忽略
        assert!(!slot.set_decoded(DocImage::default()));
        assert!(slot.approx_bytes() >= 3 + 4);
    }
}
