//! 电子表格（xlsx）的**纯数据模型**：解析出来的内容 + 几何计算。
//!
//! 为什么模型放 `qlr-core` 而不是直接放渲染层：
//! - `qlr-render` 负责"文件 → 模型"（解 zip、读 XML、算数字格式），不能碰 `windows`；
//! - `qlr-platform` 负责"模型 → 像素"（GDI 画网格与文字），它只依赖 `qlr-core`；
//! - 两边共用同一份几何（列宽累加、合并单元格命中），否则"量的位置"和"画的位置"
//!   一旦不一致，就会出现"文字画进隔壁单元格"这种只有截图才看得见的 bug（CONVENTIONS #38
//!   的同类问题）。
//!
//! 这一层**不做任何 IO、不依赖 `windows`**，可以在 Linux 上单测。

/// Excel 默认列宽（8.43 字符）在 96 DPI 下的像素值，与 Excel 自身的观感对齐。
pub const DEFAULT_COL_WIDTH: u32 = 64;

/// Excel 默认行高（15 pt）在 96 DPI 下的像素值。
pub const DEFAULT_ROW_HEIGHT: u32 = 20;

/// 单元格水平对齐。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// 单元格垂直对齐。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VAlign {
    Top,
    Center,
    #[default]
    Bottom,
}

/// 边框线型。数值即"粗细档位"，画的时候按档位换算像素（1 → 1px，2 → 2px，3 → 双线）。
pub mod line {
    pub const NONE: u8 = 0;
    pub const THIN: u8 = 1;
    pub const MEDIUM: u8 = 2;
    pub const DOUBLE: u8 = 3;
}

/// 四边边框（只用粗细档位，不细分颜色：预览里格线一律浅灰/深灰）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Border {
    pub top: u8,
    pub right: u8,
    pub bottom: u8,
    pub left: u8,
}

impl Border {
    pub fn is_empty(&self) -> bool {
        self.top == line::NONE
            && self.right == line::NONE
            && self.bottom == line::NONE
            && self.left == line::NONE
    }

    pub fn max_weight(&self) -> u8 {
        self.top.max(self.right).max(self.bottom).max(self.left)
    }
}

/// 单元格配色：前景（字色）与可选底色，`0xRRGGBB`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Colors {
    pub fg: u32,
    pub bg: Option<u32>,
}

// 默认色写成显式实现（而不是 derive）是为了让"默认字色黑、默认无底色"这两个**语义**
// 在代码里看得见；clippy 认为可 derive，这里显式保留。
#[allow(clippy::derivable_impls)]
impl Default for Colors {
    fn default() -> Self {
        Self {
            fg: 0x00_00_00,
            bg: None,
        }
    }
}

/// 单元格样式（只保留预览真正会画的那些属性）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellStyle {
    /// 粗体（`<b/>` 或 font 的 weight ≥ 700）。
    pub bold: bool,
    /// 斜体：GDI 能画，但探针阶段先记录、绘制端可只对粗体做差异化。
    pub italic: bool,
    /// 字号，单位 0.1 pt（Excel 的 `sz` 是半磅，这里放大一级避免浮点）。
    pub size_dpt: u16,
    pub colors: Colors,
    /// `wrapText`：true 时按列宽折行，false 时溢出/裁剪。
    pub wrap: bool,
    pub h_align: HAlign,
    pub v_align: VAlign,
    pub border: Border,
}

impl Default for CellStyle {
    fn default() -> Self {
        Self {
            bold: false,
            italic: false,
            // 11 pt = 110 dpt，Excel 默认正文字号
            size_dpt: 110,
            colors: Colors::default(),
            wrap: false,
            h_align: HAlign::Left,
            v_align: VAlign::Bottom,
            border: Border::default(),
        }
    }
}

impl CellStyle {
    /// 字号（pt，浮点），给绘制端算像素高度用。
    pub fn size_pt(&self) -> f32 {
        f32::from(self.size_dpt) / 10.0
    }
}

/// 一个非空单元格。`text` 是**已经算好数字格式的显示字符串**（日期、百分比、千分位都在
/// 解析阶段定稿）——绘制端不再考虑数字格式，避免"渲染层也要懂 Excel 格式码"。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub row: u32,
    pub col: u32,
    pub text: String,
    pub style: CellStyle,
}

/// 合并区域：`(row, col)` 是左上角，`rows`/`cols` 是跨度（≥1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Merge {
    pub row: u32,
    pub col: u32,
    pub rows: u32,
    pub cols: u32,
}

impl Merge {
    pub fn contains(&self, row: u32, col: u32) -> bool {
        row >= self.row
            && row < self.row.saturating_add(self.rows)
            && col >= self.col
            && col < self.col.saturating_add(self.cols)
    }

    /// 是不是"被合并吃掉的非左上角格子"（这些格子不单独画）。
    pub fn covers_other(&self, row: u32, col: u32) -> bool {
        self.contains(row, col) && !(row == self.row && col == self.col)
    }
}

/// 一张工作表。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sheet {
    pub name: String,
    /// 列数 / 行数（`col_widths` / `row_heights` 的长度就是它们）。
    pub cols: u32,
    pub rows: u32,
    /// 每列宽度（像素，已含 Excel 的字符宽换算）。
    pub col_widths: Vec<u32>,
    /// 每行高度（像素）。
    pub row_heights: Vec<u32>,
    /// 非空单元格，按 `(row, col)` 升序。
    pub cells: Vec<Cell>,
    /// 合并区域。
    pub merges: Vec<Merge>,
    /// 冻结窗格（`pane` 的 `xSplit`/`ySplit`），暂只记录不实现悬停。
    pub frozen_rows: u32,
    pub frozen_cols: u32,
}

impl Sheet {
    /// 第 `col` 列左边界的 x 坐标（像素）。
    pub fn col_x(&self, col: u32) -> i64 {
        let n = (col as usize).min(self.col_widths.len());
        self.col_widths[..n].iter().map(|w| i64::from(*w)).sum()
    }

    /// 第 `row` 行上边界的 y 坐标（像素）。
    pub fn row_y(&self, row: u32) -> i64 {
        let n = (row as usize).min(self.row_heights.len());
        self.row_heights[..n].iter().map(|h| i64::from(*h)).sum()
    }

    /// 整表宽度（像素）。
    pub fn width(&self) -> i64 {
        self.col_widths.iter().map(|w| i64::from(*w)).sum()
    }

    /// 整表高度（像素）。
    pub fn height(&self) -> i64 {
        self.row_heights.iter().map(|h| i64::from(*h)).sum()
    }

    /// 某个 y 坐标落在第几行（越界夹到最后一行）。
    pub fn row_at(&self, y: i64) -> u32 {
        let mut acc = 0i64;
        for (i, h) in self.row_heights.iter().enumerate() {
            acc += i64::from(*h);
            if y < acc {
                return i as u32;
            }
        }
        self.rows.saturating_sub(1)
    }

    /// 某个 x 坐标落在第几列（越界夹到最后一列）。
    pub fn col_at(&self, x: i64) -> u32 {
        let mut acc = 0i64;
        for (i, w) in self.col_widths.iter().enumerate() {
            acc += i64::from(*w);
            if x < acc {
                return i as u32;
            }
        }
        self.cols.saturating_sub(1)
    }

    /// 按 `(row, col)` 取单元格（二分；`cells` 必须已排序）。
    pub fn cell(&self, row: u32, col: u32) -> Option<&Cell> {
        let key = (row, col);
        self.cells
            .binary_search_by(|c| (c.row, c.col).cmp(&key))
            .ok()
            .map(|i| &self.cells[i])
    }

    /// 这个格子是不是被别的合并区域吃掉了（真正的左上角不算）。
    pub fn covered_by_merge(&self, row: u32, col: u32) -> bool {
        self.merges.iter().any(|m| m.covers_other(row, col))
    }

    /// 包含这个格子的合并区域（含左上角自己）。
    pub fn merge_at(&self, row: u32, col: u32) -> Option<&Merge> {
        self.merges.iter().find(|m| m.contains(row, col))
    }

    /// 合并区域在像素坐标里的矩形 `(x, y, w, h)`。
    pub fn merge_rect(&self, m: &Merge) -> (i64, i64, i64, i64) {
        let x = self.col_x(m.col);
        let y = self.row_y(m.row);
        let w = self.col_x(m.col.saturating_add(m.cols)) - x;
        let h = self.row_y(m.row.saturating_add(m.rows)) - y;
        (x, y, w, h)
    }

    /// 单格在像素坐标里的矩形 `(x, y, w, h)`。
    pub fn cell_rect(&self, row: u32, col: u32) -> (i64, i64, i64, i64) {
        let x = self.col_x(col);
        let y = self.row_y(row);
        let w = i64::from(*self.col_widths.get(col as usize).unwrap_or(&0));
        let h = i64::from(*self.row_heights.get(row as usize).unwrap_or(&0));
        (x, y, w, h)
    }
}

/// 一个工作簿：多张表 + 当前表。
///
/// 预览只画**当前一张**（`active`），切换由壳层改索引后重画——不在这里做跨表联动。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SheetDoc {
    pub sheets: Vec<Sheet>,
    /// 初始显示哪一张（一般是 workbook 里的第一个可见表）。
    pub active: usize,
}

impl SheetDoc {
    /// 当前表（空工作簿返回 `None`）。越界时夹回第一张。
    pub fn sheet(&self) -> Option<&Sheet> {
        if self.sheets.is_empty() {
            return None;
        }
        self.sheets.get(self.active.min(self.sheets.len() - 1))
    }

    /// 第 `index` 张表（越界返回 `None`）。
    pub fn at(&self, index: usize) -> Option<&Sheet> {
        self.sheets.get(index)
    }

    pub fn len(&self) -> usize {
        self.sheets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sheets.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sheet() -> Sheet {
        Sheet {
            name: "测试".to_string(),
            cols: 3,
            rows: 2,
            col_widths: vec![100, 50, 70],
            row_heights: vec![20, 40],
            cells: vec![
                Cell {
                    row: 0,
                    col: 0,
                    text: "A1".into(),
                    style: CellStyle::default(),
                },
                Cell {
                    row: 1,
                    col: 2,
                    text: "C2".into(),
                    style: CellStyle::default(),
                },
            ],
            merges: vec![Merge {
                row: 0,
                col: 1,
                rows: 2,
                cols: 2,
            }],
            frozen_rows: 1,
            frozen_cols: 0,
        }
    }

    #[test]
    fn geometry_accumulates_widths_and_heights() {
        let s = sheet();
        assert_eq!(s.col_x(0), 0);
        assert_eq!(s.col_x(1), 100);
        assert_eq!(s.col_x(2), 150);
        assert_eq!(s.col_x(3), 220);
        // 越界不 panic，回到总数
        assert_eq!(s.col_x(99), 220);
        assert_eq!(s.row_y(1), 20);
        assert_eq!(s.row_y(2), 60);
        assert_eq!(s.width(), 220);
        assert_eq!(s.height(), 60);
    }

    #[test]
    fn hit_testing_maps_pixels_back_to_cells() {
        let s = sheet();
        assert_eq!(s.col_at(0), 0);
        assert_eq!(s.col_at(99), 0);
        assert_eq!(s.col_at(100), 1);
        assert_eq!(s.col_at(149), 1);
        assert_eq!(s.col_at(150), 2);
        assert_eq!(s.col_at(9_999), 2);
        assert_eq!(s.row_at(0), 0);
        assert_eq!(s.row_at(19), 0);
        assert_eq!(s.row_at(20), 1);
        assert_eq!(s.row_at(9_999), 1);
    }

    #[test]
    fn cell_lookup_is_binary_search_by_coordinates() {
        let s = sheet();
        assert_eq!(s.cell(0, 0).map(|c| c.text.as_str()), Some("A1"));
        assert_eq!(s.cell(1, 2).map(|c| c.text.as_str()), Some("C2"));
        assert!(s.cell(1, 1).is_none());
        assert!(s.cell(9, 9).is_none());
    }

    #[test]
    fn merge_hit_testing_separates_origin_from_covered_cells() {
        let s = sheet();
        let m = s.merge_at(1, 1).expect("(1,1) 在合并区域内");
        assert_eq!((m.row, m.col, m.rows, m.cols), (0, 1, 2, 2));
        // 左上角要自己画
        assert!(s.merge_at(0, 1).is_some());
        assert!(!s.covered_by_merge(0, 1));
        // 被吃掉的格子
        assert!(s.covered_by_merge(0, 2));
        assert!(s.covered_by_merge(1, 1));
        assert!(s.covered_by_merge(1, 2));
        // 合并区域之外
        assert!(!s.covered_by_merge(0, 0));
        assert!(s.merge_at(0, 0).is_none());
    }

    #[test]
    fn merge_rect_uses_accumulated_geometry() {
        let s = sheet();
        let m = s.merge_at(0, 1).unwrap();
        // x = 第 1 列左边界(100)，宽 = 50+70，y = 0，高 = 20+40
        assert_eq!(s.merge_rect(m), (100, 0, 120, 60));
        assert_eq!(s.cell_rect(1, 2), (150, 20, 70, 40));
    }

    #[test]
    fn doc_without_sheets_degrades_to_none() {
        let empty = SheetDoc::default();
        assert!(empty.is_empty());
        assert!(empty.sheet().is_none());
        assert!(empty.at(0).is_none());
    }

    #[test]
    fn active_index_is_clamped_to_available_sheets() {
        let doc = SheetDoc {
            sheets: vec![Sheet::default()],
            active: 7,
        };
        assert_eq!(doc.len(), 1);
        assert!(doc.sheet().is_some());
    }

    #[test]
    fn style_size_pt_round_trips_from_tenths() {
        let mut st = CellStyle::default();
        assert_eq!(st.size_pt(), 11.0);
        st.size_dpt = 145;
        assert!((st.size_pt() - 14.5).abs() < f32::EPSILON);
    }
}
