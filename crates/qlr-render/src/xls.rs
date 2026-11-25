//! 老式二进制 Excel（`.xls` / BIFF8）解析：`calamine` → `qlr_core::sheet::SheetDoc`。
//!
//! 批 B（2026-11-25）#3：用户要求内置看 `.xls`，不再依赖装 Office。xlsx 一族仍走
//! `sheet.rs` 自己解 OOXML（拿到样式/条件格式/合并等丰富信息）；老式 `.xls` 用纯 Rust
//! 的 `calamine` 解，代价是**样式信息拿不到**（字体/颜色/边框/合并），只保证"文字 + 数值 +
//! 默认列宽行高"能看。这对"临时看一眼内容"足够，也符合"能看但不像 Excel、绝不报错"的口径。
//!
//! 契约与 `sheet.rs` 一致：只出数据模型，不碰 `windows`；失败必须是可解释的 `Error::msg`，
//! 绝不 panic（release 是 `panic = "abort"`）。

use std::path::Path;

use calamine::{Data, Range, Reader, open_workbook_auto};
use qlr_core::Error;
use qlr_core::Result;
use qlr_core::sheet::{Cell, CellStyle, DEFAULT_COL_WIDTH, DEFAULT_ROW_HEIGHT, Sheet, SheetDoc};

const SCOPE: &str = "xls";

/// 单表最多保留多少格（与 `sheet.rs` 口径一致，防 OOM）。
const MAX_CELLS: usize = 400_000;
const MAX_COLS: u32 = 16_384;
const MAX_ROWS: u32 = 131_072;
const MAX_SHEETS: usize = 512;
const MAX_TEXT_CHARS: usize = 512;

/// 解析 `.xls`，返回整本工作簿的 `SheetDoc`（只有选中的那张带几何+单元格，其余空壳）。
pub fn parse(path: &Path, explicit: Option<usize>) -> Result<SheetDoc> {
    let mut wb = open_workbook_auto(path).map_err(|e| {
        Error::msg(format!(
            "打开表格失败（{}）：{e}",
            crate::display_name(path)
        ))
    })?;
    // 先拿走名字（`sheet_names` 借 wb，后面 `worksheet_range` 要 `&mut`）。
    let names: Vec<String> = wb.sheet_names().to_owned();
    if names.is_empty() {
        return Err(Error::msg(format!(
            "工作簿里没有任何工作表（{}）",
            crate::display_name(path)
        )));
    }
    let selected = match explicit {
        Some(i) => i.min(names.len() - 1),
        None => 0,
    };
    let mut sheets: Vec<Sheet> = Vec::with_capacity(names.len().min(MAX_SHEETS));
    for (i, name) in names.iter().enumerate() {
        if i >= MAX_SHEETS {
            qlr_core::log_warn!(
                SCOPE,
                "工作簿有 {} 张表，只取前 {MAX_SHEETS} 张",
                names.len()
            );
            break;
        }
        if i == selected {
            let range = wb
                .worksheet_range(name)
                .map_err(|e| Error::msg(format!("读工作表失败（{name}）：{e}")))?;
            sheets.push(build_sheet(name, &range));
        } else {
            // 空壳：只留名字，壳层靠它显示"第几张/叫什么"，翻到才真正解析。
            sheets.push(Sheet {
                name: name.clone(),
                ..Default::default()
            });
        }
    }
    Ok(SheetDoc {
        sheets,
        active: selected,
    })
}

/// 只读骨架：`(工作表数, active 下标)`。给壳层定翻页范围用。
pub fn layout(path: &Path) -> Option<(usize, usize)> {
    let wb = open_workbook_auto(path).ok()?;
    let n = wb.sheet_names().len();
    if n == 0 {
        None
    } else {
        Some((n.min(MAX_SHEETS), 0))
    }
}

/// 把一个 `Range<Data>` 建成 `Sheet`（默认列宽/行高，无样式、无合并）。
fn build_sheet(name: &str, range: &Range<Data>) -> Sheet {
    let (nrows, ncols) = range.get_size();
    let (sr, sc) = range.start().unwrap_or((0, 0));
    let rows = ((sr as usize) + nrows).min(MAX_ROWS as usize) as u32;
    let cols = ((sc as usize) + ncols).min(MAX_COLS as usize) as u32;
    let mut sheet = Sheet {
        name: name.to_string(),
        cols,
        rows,
        col_widths: vec![DEFAULT_COL_WIDTH; cols as usize],
        row_heights: vec![DEFAULT_ROW_HEIGHT; rows as usize],
        cells: Vec::new(),
        merges: Vec::new(),
        frozen_rows: 0,
        frozen_cols: 0,
    };
    if rows == 0 || cols == 0 {
        return sheet;
    }
    'rows: for (ri, row) in range.rows().enumerate() {
        let r = sr + ri as u32;
        if r >= rows {
            break;
        }
        for (ci, d) in row.iter().enumerate() {
            let c = sc + ci as u32;
            if c >= cols {
                break;
            }
            if let Some((mut text, numeric)) = cell_text(d) {
                if text.chars().count() > MAX_TEXT_CHARS {
                    text = text.chars().take(MAX_TEXT_CHARS).collect();
                }
                sheet.cells.push(Cell {
                    row: r,
                    col: c,
                    text,
                    style: CellStyle::default(),
                    numeric,
                    data_bar: None,
                });
                if sheet.cells.len() >= MAX_CELLS {
                    qlr_core::log_warn!(SCOPE, "单元格超过上限 {MAX_CELLS}，余下截断");
                    break 'rows;
                }
            }
        }
    }
    sheet
}

/// calamine 的 `Data` → `(显示文本, 是否数值)`。`None` = 空格子，不生成 `Cell`。
fn cell_text(d: &Data) -> Option<(String, bool)> {
    match d {
        Data::Empty => None,
        Data::String(s) => Some((s.clone(), false)),
        Data::Int(i) => Some((i.to_string(), true)),
        Data::Float(f) => Some((fmt_float(*f), true)),
        Data::Bool(b) => Some(((if *b { "TRUE" } else { "FALSE" }).to_string(), false)),
        Data::Error(e) => Some((format!("{e:?}"), false)),
        Data::DateTime(dt) => Some((excel_serial_to_string(dt.as_f64()), false)),
        Data::DateTimeIso(s) | Data::DurationIso(s) => Some((s.clone(), false)),
    }
}

/// 浮点显示：整数不带小数点，小数最多 6 位并去掉尾随 0。
fn fmt_float(v: f64) -> String {
    if !v.is_finite() {
        return v.to_string();
    }
    if v == v.trunc() && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let s = format!("{v:.6}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Excel 序列日期（自 1899-12-30 起的天数）→ `YYYY-MM-DD[ HH:MM:SS]`。
fn excel_serial_to_string(v: f64) -> String {
    if !v.is_finite() || v < 1.0 {
        return fmt_float(v);
    }
    let days = v.floor() as i64;
    let frac = v - days as f64;
    // 1899-12-30 → 1970-01-01 相差 25569 天。
    let (y, m, d) = civil_from_days(days - 25_569);
    let secs = (frac * 86_400.0).round() as i64;
    if secs == 0 {
        format!("{y:04}-{m:02}-{d:02}")
    } else {
        let (hh, mm, ss) = (secs / 3600, (secs % 3600) / 60, secs % 60);
        format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
    }
}

/// 以"1970-01-01 起的天数"换算公历年月日（Howard Hinnant 的 civil_from_days）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_formatting_trims_trailing_zeros() {
        assert_eq!(fmt_float(3.0), "3");
        assert_eq!(fmt_float(3.5), "3.5");
        assert_eq!(fmt_float(1.23456789), "1.234568");
        assert_eq!(fmt_float(-0.0), "0");
    }

    #[test]
    fn excel_serial_converts_known_dates() {
        // 45000 = 2023-03-15（Excel 序列日期）；带小数时补时间部分。
        assert_eq!(excel_serial_to_string(45_000.0), "2023-03-15");
        let dt = excel_serial_to_string(45_000.5);
        assert!(dt.starts_with("2023-03-15 12:00:00"), "{dt}");
    }

    #[test]
    fn civil_from_days_matches_epoch_anchors() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1)); // 2024-01-01
    }
}
