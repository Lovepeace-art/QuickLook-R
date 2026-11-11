//! xlsx（OOXML 电子表格）解析：zip 容器 → `qlr_core::sheet::SheetDoc`。
//!
//! **契约**：本模块只做"文件 → 纯数据模型"，不碰 `windows`、不画图、不排版。
//! 几何（列宽累加、合并命中）由 `qlr_core::sheet::Sheet` 负责，绘制由
//! `qlr_platform::window::sheet` 负责。这样这三段可以分别单测。
//!
//! 失败必须是**可解释的错误**（`Error::msg`），不能 panic：release 构建是
//! `panic = "abort"`，进程内解析崩一次就把整个预览进程带走（见 CONVENTIONS #128 同类）。
//!
//! # 解析到什么程度
//!
//! 支持（1.9.0 的验收面）：
//! - `xl/workbook.xml` 的 sheet 列表 + `activeTab`，sheet 名 → `xl/_rels/workbook.xml.rels`
//!   的 rId → 真实 part 路径（**不假设 `sheetN.xml` 的 N 等于序号**：WPS/Excel 导出会乱序，
//!   `big-20k.xlsx` 里 Target 甚至是绝对路径 `/xl/worksheets/sheet1.xml`）；
//! - `xl/sharedStrings.xml`（含 `<r><t>` 富文本拼接、跳过注音 `<rPh>`）；
//! - `xl/styles.xml`：numFmts / fonts / fills / borders / cellXfs；主题色读
//!   `xl/theme/theme1.xml` 的 `clrScheme`（带 tint 与 Excel 的 dk1/lt1 对调）；
//! - `xl/worksheets/sheet*.xml`：`<cols>` 列宽、`<row ht>` 行高、`<c r s t>` 的
//!   `<v>`/`<is>`、`<mergeCells>`、`<sheetView><pane>` 冻结；
//! - **条件格式**（1.9.3）：色阶（2/3 色标线性插值）、数据条比例（`Cell::data_bar`）、
//!   `cellIs` + `<dxfs>` 差分样式（红字/底色）。实现见下文「条件格式」一节；
//! - 数字格式：内建 numFmtId 0–49 的常见项 + 自定义格式串的**最小可用子集**
//!   （整数 / 固定小数 / 千分位 / 百分比 / 会计括号 / 科学计数 / 日期时间）。
//!
//! 明确不支持（都退化成"能看但不像 Excel"，不会报错）：
//! - 分数格式（`# ?/?`）→ 按 General 输出小数；
//! - `[h]`/`[m]` 这类**累计**时间 → 当普通时分秒（36 小时会显示成 12:00:00）；
//! - 日期里的中文/本地化月名（`mmm` 一律英文缩写 Jan/Mar…）、佛历/民国历 `[$-404]`；
//! - 条件格式里的 `iconSet`/`expression`/`top10`/`duplicateValues`/`containsText` 等
//!   （见「条件格式」一节的 type 分派表），以及图标集、单元格内图片/批注、超链接、
//!   公式（只取 `<v>` 缓存值）；
//! - 富文本**段内**样式（一个单元格里红黑混排）→ 取该格的 `cellXfs` 字体，忽略 `<r>` 上的字体；
//! - 旋转文字、缩进、文本方向、自定义 `indexedColors` 调色板（用内建 56 色调色板）。
//!
//! # 上限（防 OOM / 防秒挂）
//!
//! 见下面 `MAX_*` 常量。所有上限都是"截断 + `log_warn!`"，只有 zip 结构异常才 `Err`。
//! 实测 16 万格（8.8 MB 解压后）在 100 ms 量级，滚动时不会重解析（壳层缓存 `SheetDoc`）。

use std::fs::File;
use std::io::Read;
use std::path::Path;

use qlr_core::Error;
use qlr_core::Result;
use qlr_core::sheet::{
    Border, Cell, CellStyle, Colors, DEFAULT_COL_WIDTH, DEFAULT_ROW_HEIGHT, HAlign, Merge, Sheet,
    SheetDoc, VAlign, line,
};

/// 日志 scope（AGENTS §7：不要动态拼 scope）。
const SCOPE: &str = "sheet";

/// 一个工作簿最多认多少张表（超出截断；Excel 的旧限制是 255 个可见表）。
const MAX_SHEETS: usize = 512;
/// 单表最多保留多少单元格。40 万格 ≈ 25 MB 模型内存，够 40 屏滚动。
const MAX_CELLS: usize = 400_000;
/// 列上限（Excel 自己的上限就是 16384）。
const MAX_COLS: u32 = 16_384;
/// 行上限。**比 Excel 的 1048576 小得多**：`row_heights` 是稠密数组，照抄 Excel 上限
/// 光行高级数组就要 4 MB；13 万行（512 KB）对预览足够，多出来的行直接裁掉。
const MAX_ROWS: u32 = 131_072;
/// 单元格文本截断字符数（长文本在预览里没有意义，只会拖慢绘制）。
const MAX_TEXT_CHARS: usize = 512;
/// zip **单个 part** 解压后的上限（big-20k 的 sheet1.xml 是 8.8 MB，留足余量）。
const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
/// 一次解析累计解压上限（zip 炸弹防护：40 个 64 MB 的 part 也才到这个量级）。
const MAX_TOTAL_BYTES: u64 = 192 * 1024 * 1024;
/// 共享字符串表最多多少条 / 解码后最多多少字节。
const MAX_SHARED_STRINGS: usize = 500_000;
const MAX_SHARED_BYTES: usize = 32 * 1024 * 1024;
/// styles.xml 里各数组的条数上限（正常文件都是个位/两位数，这里是畸形文件兜底）。
const MAX_XFS: usize = 4_096;
const MAX_FONTS: usize = 1_024;
const MAX_FILLS: usize = 1_024;
const MAX_BORDERS: usize = 1_024;
const MAX_NUM_FMTS: usize = 512;
/// `<cols>` 的条数上限 / 合并区域条数上限。
const MAX_COL_BANDS: usize = 4_096;
const MAX_MERGES: usize = 4_096;
/// 条件格式的兜底上限：单表最多认多少条 `<cfRule>`、一个 `sqref` 最多认多少个区域、
/// 单个区域最多扫多少格（正常文件都在个位数；畸形文件不许把解析拖进秒级）。
const MAX_CF_RULES: usize = 256;
const MAX_CF_RANGES: usize = 512;
const MAX_CF_CELLS: usize = 100_000;

/// 解析一个 xlsx/xlsm/xltx/xltm 文件（取工作簿里 `activeTab` 那张表）。
pub fn parse_sheet(path: &Path) -> qlr_core::Result<SheetDoc> {
    parse_sheet_at(path, 0)
}

/// 同上，但指定工作表序号（`0` = 工作簿自己的 `activeTab`，`n` = 第 n 张）。
///
/// 表格预览没有"页"的概念，壳层的翻页键复用它来切 sheet。
///
/// **返回的 `SheetDoc` 里 `sheets` 有全部工作表的条目（名字都在），但只有被选中的那张
/// 有几何与单元格数据**，其余是空壳（`cols = rows = 0`）。这样壳层不用第二次读文件就能
/// 知道"一共几张表、第几张叫什么"（`doc.len()` / `doc.at(i).name`），而每翻一页只需要
/// 解一张 sheet 的 XML。
///
/// `doc.active` 是最终选中的下标（越界会夹到最后一张）。
pub fn parse_sheet_at(path: &Path, sheet_index: usize) -> qlr_core::Result<SheetDoc> {
    // `0` 表示跟着工作簿的 activeTab 走；`n` 表示第 n 张。
    parse_sheet_impl(path, (sheet_index != 0).then_some(sheet_index))
}

/// 同上，但 `index` 是**显式下标**（`0` 就是第 0 张表，不看 activeTab）。
///
/// 壳层用"翻页键 ≡ 切工作表"时走这条：它先把 `page` 初始化成 `activeTab`
/// （见 `sheet_layout`），之后每翻一次就传绝对下标，语义不跟 activeTab 纠缠。
pub fn parse_sheet_index(path: &Path, index: usize) -> qlr_core::Result<SheetDoc> {
    parse_sheet_impl(path, Some(index))
}

/// 只读工作簿骨架：返回 `(工作表数, activeTab 下标)`，不解样式、不读单元格。
///
/// 给壳层定翻页范围用（`s.pages` / `s.page`），代价是一次 workbook.xml 读取。
pub fn sheet_layout(path: &Path) -> Option<(usize, usize)> {
    let file = File::open(path).ok()?;
    let zip = zip::ZipArchive::new(file).ok()?;
    let mut pkg = Package { zip, used: 0 };
    let workbook = pkg.read_optional("xl/workbook.xml").ok()??;
    let (metas, active) = parse_workbook(&workbook);
    if metas.is_empty() {
        return None;
    }
    let count = metas.len().min(MAX_SHEETS);
    Some((count, active.min(count - 1)))
}

fn parse_sheet_impl(path: &Path, explicit: Option<usize>) -> qlr_core::Result<SheetDoc> {
    let name = crate::display_name(path);
    let file =
        File::open(path).map_err(|e| Error::msg(format!("打开 xlsx 失败（{name}）：{e}")))?;
    let zip = zip::ZipArchive::new(file)
        .map_err(|e| Error::msg(format!("不是有效的 xlsx（{name} 的 zip 容器打不开）：{e}")))?;
    let mut pkg = Package { zip, used: 0 };

    // ---- 工作簿骨架：sheet 名 + activeTab + rId→part ----
    let workbook = pkg.read_required("xl/workbook.xml")?;
    let (mut metas, active) = parse_workbook(&workbook);
    if metas.is_empty() {
        return Err(Error::msg(format!("工作簿里没有任何工作表（{name}）")));
    }
    if metas.len() > MAX_SHEETS {
        qlr_core::log_warn!(
            SCOPE,
            "工作簿有 {} 张表，只取前 {MAX_SHEETS} 张",
            metas.len()
        );
        metas.truncate(MAX_SHEETS);
    }
    let rels = match pkg.read_optional("xl/_rels/workbook.xml.rels")? {
        Some(xml) => parse_rels(&xml),
        None => Vec::new(),
    };

    // 显式下标（`Some`）就按它走；`None` 表示跟着工作簿的 activeTab。
    let selected = match explicit {
        Some(index) => index.min(metas.len() - 1),
        None => active.min(metas.len() - 1),
    };

    // ---- 主题色 / 样式 / 共享字符串 ----
    let theme = match pkg.read_optional(&part_by_hint(&rels, "theme", "xl/theme/theme1.xml"))? {
        Some(xml) => parse_theme(&xml),
        None => Theme::default(),
    };
    let styles = match pkg.read_optional(&part_by_hint(&rels, "styles", "xl/styles.xml"))? {
        Some(xml) => parse_styles(&xml, &theme),
        None => StyleSheet::default(),
    };
    let shared = match pkg.read_optional(&part_by_hint(
        &rels,
        "sharedStrings",
        "xl/sharedStrings.xml",
    ))? {
        Some(xml) => parse_shared_strings(&xml),
        None => Vec::new(),
    };

    // ---- 选中的那张表 ----
    let target = sheet_target(&metas, &rels, selected);
    let mut resolved = pkg.find_name(&target);
    if resolved.is_none() {
        // 兜底：rels 缺失/写错时按 `sheet{n+1}.xml` 猜，再不行按目录里的顺序数。
        resolved = pkg.find_name(&format!("xl/worksheets/sheet{}.xml", selected + 1));
    }
    let resolved = resolved
        .or_else(|| pkg.nth_worksheet(selected))
        .ok_or_else(|| Error::msg(format!("工作表 XML 缺失（{name} 里没有 {target}）")))?;
    let sheet_xml = pkg
        .read_optional(&resolved)?
        .ok_or_else(|| Error::msg(format!("工作表 XML 读不出（{name} 的 {resolved}）")))?;
    let mut sheet = parse_worksheet(&sheet_xml, &shared, &styles);
    sheet.name = metas
        .get(selected)
        .map(|m| m.name.clone())
        .unwrap_or_default();

    // ---- 组装：所有表都留名字，只有选中的那张有数据 ----
    let mut sheets: Vec<Sheet> = metas
        .iter()
        .map(|m| Sheet {
            name: m.name.clone(),
            ..Sheet::default()
        })
        .collect();
    if let Some(slot) = sheets.get_mut(selected) {
        *slot = sheet;
    }
    Ok(SheetDoc {
        sheets,
        active: selected,
    })
}

// ---------------------------------------------------------------------------
// zip 容器
// ---------------------------------------------------------------------------

/// 一层薄封装：按 part 名取解压文本，带单条目/总量上限。
struct Package {
    zip: zip::ZipArchive<File>,
    /// 已经解压出来的累计字节数（zip 炸弹防护）。
    used: u64,
}

impl Package {
    /// 在包里找 part：先精确匹配，再大小写不敏感（WPS / 某些 Java 库写法不一）。
    fn find_name(&mut self, want: &str) -> Option<String> {
        let want = normalize_part(want);
        if self.zip.file_names().any(|n| n == want) {
            return Some(want);
        }
        self.zip
            .file_names()
            .find(|n| normalize_part(n).eq_ignore_ascii_case(&want))
            .map(str::to_string)
    }

    /// 按目录里 `xl/worksheets/*.xml` 的字典序取第 `index` 个（rels 彻底不可用时的兜底）。
    fn nth_worksheet(&mut self, index: usize) -> Option<String> {
        let mut names: Vec<String> = self
            .zip
            .file_names()
            .filter(|n| {
                let lower = n.to_ascii_lowercase();
                lower.starts_with("xl/worksheets/") && lower.ends_with(".xml")
            })
            .map(str::to_string)
            .collect();
        names.sort();
        names.get(index).cloned()
    }

    fn read_optional(&mut self, entry: &str) -> Result<Option<String>> {
        let Some(name) = self.find_name(entry) else {
            return Ok(None);
        };
        self.read_part(&name).map(Some)
    }

    fn read_required(&mut self, entry: &str) -> Result<String> {
        self.read_optional(entry)?
            .ok_or_else(|| Error::msg(format!("不是有效的 xlsx：缺少 {entry}")))
    }

    fn read_part(&mut self, name: &str) -> Result<String> {
        let entry = self
            .zip
            .by_name(name)
            .map_err(|e| Error::msg(format!("读取 {name} 失败：{e}")))?;
        // 声明尺寸只用来预分配；真正的读取用 `take` 夹死，防"头部谎报尺寸"。
        let declared = entry.size();
        if declared > MAX_ENTRY_BYTES {
            return Err(Error::msg(format!(
                "{name} 解压后有 {declared} 字节，超过单条目上限 {MAX_ENTRY_BYTES}"
            )));
        }
        if self.used.saturating_add(declared) > MAX_TOTAL_BYTES {
            qlr_core::log_warn!(
                SCOPE,
                "xlsx 累计解压超过 {MAX_TOTAL_BYTES} 字节，放弃读取 {name}"
            );
            return Err(Error::msg("xlsx 解压总量超过上限（疑似压缩炸弹）"));
        }
        let mut buf: Vec<u8> = Vec::with_capacity((declared as usize).min(4 * 1024 * 1024));
        let read = entry
            .take(MAX_ENTRY_BYTES + 1)
            .read_to_end(&mut buf)
            .map_err(|e| Error::msg(format!("{name} 解压失败：{e}")))?;
        if read as u64 > MAX_ENTRY_BYTES {
            return Err(Error::msg(format!("{name} 解压超过单条目上限")));
        }
        self.used = self.used.saturating_add(read as u64);
        Ok(decode_xml(&buf))
    }
}

/// part 名规范化：去前导 `/` 与 `./`，反斜杠换正斜杠。
fn normalize_part(name: &str) -> String {
    let mut s = name.replace('\\', "/");
    loop {
        let trimmed = s.trim_start_matches('/');
        let trimmed = trimmed.strip_prefix("./").unwrap_or(trimmed);
        if trimmed.len() == s.len() {
            return s;
        }
        s = trimmed.to_string();
    }
}

/// XML 字节 → String：去 BOM，非法字节用替换字符（坏文件不该让整次预览失败）。
fn decode_xml(bytes: &[u8]) -> String {
    let body = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        bytes.get(3..).unwrap_or(&[])
    } else {
        bytes
    };
    String::from_utf8_lossy(body).into_owned()
}

/// rels 里按 part 名（大小写不敏感的尾巴）找真实路径；找不到就用默认名。
fn part_by_hint(rels: &[Rel], hint: &str, fallback: &str) -> String {
    let lower = hint.to_ascii_lowercase();
    rels.iter()
        .find(|r| r.target.to_ascii_lowercase().contains(&lower))
        .map(|r| r.target.clone())
        .unwrap_or_else(|| fallback.to_string())
}

// ---------------------------------------------------------------------------
// 极简 XML 扫描
// ---------------------------------------------------------------------------
//
// 为什么手写：仓库一贯不引 XML 解析库（见 AGENTS §12 的零依赖倾向），而 xlsx 的标签形状
// 极其固定——只有 `<tag attr="v">text</tag>`，属性永远带引号。这里只做"够用的扫描"：
// 不做实体递归、不做命名空间、不做 DTD；遇到不认识的东西一律跳过，绝不为容错写状态机。

/// `<tag` 的边界判定：`<tag>`、`<tag />`、`<tag ...>` 算，`<tags>` 不算。
fn tag_matches(s: &str, at: usize, name: &str) -> bool {
    let Some(rest) = s.get(at..) else {
        return false;
    };
    let Some(after) = rest.strip_prefix('<') else {
        return false;
    };
    let Some(tail) = after.strip_prefix(name) else {
        return false;
    };
    matches!(
        tail.as_bytes().first(),
        Some(b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>')
    )
}

/// 从 `from` 起找下一个 `<name ...>` 的起点（`<` 的下标）。
fn find_tag(s: &str, from: usize, name: &str) -> Option<usize> {
    let mut from = from;
    loop {
        let rel = s.get(from..)?.find('<')?;
        let at = from + rel;
        if tag_matches(s, at, name) {
            return Some(at);
        }
        from = at + 1;
    }
}

/// 标签名（`<` 之后到空白 / `/` / `>` 之前）。闭合标签会带前导 `/`。
fn tag_name(s: &str, at: usize) -> &str {
    let Some(rest) = s.get(at + 1..) else {
        return "";
    };
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .unwrap_or(rest.len());
    rest.get(..end).unwrap_or("")
}

/// 解析一个起始标签：返回（属性串，是否自闭合，`>` 之后的下标）。
fn open_tag(s: &str, at: usize) -> Option<(&str, bool, usize)> {
    let name_len = tag_name(s, at).len();
    let inner_start = at + 1 + name_len;
    let close = s.get(inner_start..)?.find('>')?;
    let inner = s.get(inner_start..inner_start + close)?;
    Some((
        inner,
        inner.trim_end().ends_with('/'),
        inner_start + close + 1,
    ))
}

/// 读一段纯文本到下一个 `<`（`<v>`/`<t>` 里不可能出现裸 `<`，实体都已转义）。
fn text_until_tag(s: &str, from: usize) -> (&str, usize) {
    match s.get(from..).and_then(|r| r.find('<')) {
        Some(rel) => (s.get(from..from + rel).unwrap_or(""), from + rel),
        None => (s.get(from..).unwrap_or(""), s.len()),
    }
}

/// 在一个起始标签的属性串里取值。
///
/// 只有"前面的字符是空白 + 后面紧跟 `=`"才算命中，避免把 `t="inlineStr"` 里的 `s` 当成属性名。
fn attr<'a>(inner: &'a str, key: &str) -> Option<&'a str> {
    let mut rest = inner;
    loop {
        let pos = rest.find(key)?;
        let before_ok = pos == 0
            || rest
                .as_bytes()
                .get(pos - 1)
                .is_some_and(|b| b.is_ascii_whitespace());
        let after = rest.get(pos + key.len()..).unwrap_or("");
        let after_trim = after.trim_start();
        if before_ok && after_trim.starts_with('=') {
            let value = after_trim.get(1..).unwrap_or("").trim_start();
            let quote = value.as_bytes().first().copied();
            if quote == Some(b'"') || quote == Some(b'\'') {
                let quote = quote.unwrap_or(b'"');
                let body = value.get(1..).unwrap_or("");
                let end = body.find(quote as char).unwrap_or(body.len());
                return body.get(..end);
            }
            return Some(
                value
                    .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
                    .next()
                    .unwrap_or(""),
            );
        }
        rest = rest.get(pos + key.len()..)?;
    }
}

/// XML 实体解码（`&amp;` / `&#x4E2D;` / `_x000D_`）。
fn decode_entities(raw: &str) -> String {
    if !raw.contains('&') && !raw.contains("_x0") {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(pos) = rest.find('&') {
        out.push_str(rest.get(..pos).unwrap_or(""));
        let tail = rest.get(pos..).unwrap_or("");
        let end = match tail.find(';') {
            Some(e) if e <= 10 => e,
            _ => {
                out.push('&');
                rest = rest.get(pos + 1..).unwrap_or("");
                continue;
            }
        };
        let ent = tail.get(1..end).unwrap_or("");
        match ent {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => {
                let code = if let Some(hex) = ent.strip_prefix("#x").or(ent.strip_prefix("#X")) {
                    u32::from_str_radix(hex, 16).ok()
                } else if let Some(dec) = ent.strip_prefix('#') {
                    dec.parse::<u32>().ok()
                } else {
                    None
                };
                match code.and_then(char::from_u32) {
                    Some(ch) => out.push(ch),
                    // 不认识的实体原样保留，别丢内容
                    None => out.push_str(tail.get(..=end).unwrap_or("")),
                }
            }
        }
        rest = tail.get(end + 1..).unwrap_or("");
    }
    out.push_str(rest);
    if out.contains("_x0") {
        // Excel 把控制字符写成 `_xHHHH_`（`_x000D_` 是回车）。预览里回车一律去掉。
        out = out.replace("_x000D_", "").replace("_x000A_", " ");
    }
    out
}

// ---------------------------------------------------------------------------
// 数值小工具（解析路径上不许 panic：全部 saturating / checked / Option）
// ---------------------------------------------------------------------------

fn parse_u32(s: &str) -> Option<u32> {
    s.trim().parse::<u32>().ok()
}

fn parse_f64(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

/// `"1"` / `"true"` 视为真（OOXML 的布尔属性两种都出现过）。
fn is_true(v: &str) -> bool {
    v == "1" || v.eq_ignore_ascii_case("true")
}

/// `"B3"` → 列下标 1（0 起）。`$` 前缀照吃。
fn col_of_ref(r: &str) -> Option<u32> {
    let mut col: u32 = 0;
    let mut seen = false;
    for byte in r.bytes() {
        if byte == b'$' {
            continue;
        }
        let up = byte.to_ascii_uppercase();
        if up.is_ascii_uppercase() {
            col = col.checked_mul(26)?.checked_add(u32::from(up - b'A' + 1))?;
            seen = true;
        } else {
            break;
        }
    }
    if seen { col.checked_sub(1) } else { None }
}

/// `"B3"` → (行下标 2, 列下标 1)，都从 0 起。
fn parse_ref(r: &str) -> Option<(u32, u32)> {
    let col = col_of_ref(r)?;
    let mut row: u32 = 0;
    let mut seen = false;
    for byte in r.bytes() {
        if byte.is_ascii_digit() {
            row = row
                .saturating_mul(10)
                .saturating_add(u32::from(byte - b'0'));
            seen = true;
        } else if seen {
            break;
        }
    }
    if !seen || row == 0 {
        return None;
    }
    Some((row - 1, col))
}

/// Excel 列宽（"字符个数"）→ 像素。
///
/// Excel 的换算是 `像素 = round(字符数 × maxDigitWidth + 5)`，`maxDigitWidth` 是正文字体
/// 里最宽数字的像素宽（Calibri 11 @96 DPI = 7）。默认列宽 8.43 字符 → 64 像素，正好等于
/// `DEFAULT_COL_WIDTH`，两个口径能对上。
///
/// 抽成具名函数是为了以后想按字体/DPI 精确算 `maxDigitWidth`（或换成 Excel 的
/// "MDW + padding 分段"算法）时只改这一处，而不是让 `×7+5` 散落在解析代码里。
fn col_width_px(chars: f64) -> u32 {
    const MAX_DIGIT_WIDTH: f64 = 7.0;
    const PADDING: f64 = 5.0;
    if !chars.is_finite() || chars <= 0.0 {
        return 0;
    }
    let px = (chars * MAX_DIGIT_WIDTH + PADDING).round();
    px.clamp(0.0, f64::from(u32::MAX)) as u32
}

/// Excel 行高（磅 pt）→ 像素：96/72 = 4/3。
fn row_height_px(pt: f64) -> u32 {
    if !pt.is_finite() || pt <= 0.0 {
        return 0;
    }
    let px = (pt * 96.0 / 72.0).round();
    px.clamp(0.0, f64::from(u32::MAX)) as u32
}

// ---------------------------------------------------------------------------
// workbook.xml / rels
// ---------------------------------------------------------------------------

/// 工作簿里的一张表（只到"名字 + rId"这一层）。
struct SheetMeta {
    name: String,
    rid: String,
}

/// 一条 relationship：`Id` → 包内绝对路径。
struct Rel {
    id: String,
    target: String,
}

fn parse_workbook(xml: &str) -> (Vec<SheetMeta>, usize) {
    let mut sheets = Vec::new();
    let mut i = 0;
    while let Some(at) = find_tag(xml, i, "sheet") {
        let Some((inner, _, after)) = open_tag(xml, at) else {
            break;
        };
        i = after.max(at + 1);
        if sheets.len() >= MAX_SHEETS {
            break;
        }
        let name = attr(inner, "name").map(decode_entities).unwrap_or_default();
        let rid = attr(inner, "r:id")
            .or_else(|| attr(inner, "id"))
            .unwrap_or("")
            .to_string();
        sheets.push(SheetMeta { name, rid });
    }
    let active = find_tag(xml, 0, "workbookView")
        .and_then(|at| open_tag(xml, at))
        .and_then(|(inner, _, _)| attr(inner, "activeTab").and_then(parse_u32))
        .map(|v| v as usize)
        .unwrap_or(0);
    (sheets, active)
}

fn parse_rels(xml: &str) -> Vec<Rel> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(at) = find_tag(xml, i, "Relationship") {
        let Some((inner, _, after)) = open_tag(xml, at) else {
            break;
        };
        i = after.max(at + 1);
        if out.len() >= MAX_SHEETS * 4 {
            break;
        }
        if attr(inner, "TargetMode").is_some_and(|m| m.eq_ignore_ascii_case("External")) {
            continue;
        }
        let (Some(id), Some(target)) = (attr(inner, "Id"), attr(inner, "Target")) else {
            continue;
        };
        out.push(Rel {
            id: id.to_string(),
            target: resolve_target(target),
        });
    }
    out
}

/// relationship 的 Target 是**相对 rels 所在目录**（这里是 `xl/`）的路径；
/// 但有的写入器直接给包内绝对路径（`big-20k.xlsx` 就是 `/xl/worksheets/sheet1.xml`）。
fn resolve_target(target: &str) -> String {
    let normalized = normalize_part(target);
    if normalized.is_empty() {
        return normalized;
    }
    let lower = normalized.to_ascii_lowercase();
    if lower.starts_with("xl/") {
        return normalized;
    }
    format!("xl/{normalized}")
}

/// 选中的表在包里叫什么（先 rId → rels，再按 `sheet{n+1}.xml` 猜）。
fn sheet_target(metas: &[SheetMeta], rels: &[Rel], selected: usize) -> String {
    metas
        .get(selected)
        .and_then(|m| rels.iter().find(|r| r.id == m.rid))
        .map(|r| r.target.clone())
        .unwrap_or_else(|| format!("xl/worksheets/sheet{}.xml", selected + 1))
}

// ---------------------------------------------------------------------------
// 主题色（theme1.xml）
// ---------------------------------------------------------------------------

/// `clrScheme` 的 12 个颜色槽，顺序**按 OOXML 原文**：dk1, lt1, dk2, lt2, accent1..6,
/// hlink, folHlink。
#[derive(Clone, Debug)]
struct Theme {
    colors: [u32; 12],
}

impl Default for Theme {
    /// Office 2013+ 默认主题（文件里没有 theme part 时用）。
    fn default() -> Self {
        Self {
            colors: [
                0x00_00_00, // dk1
                0xFF_FF_FF, // lt1
                0x44_54_6A, // dk2
                0xE7_E6_E6, // lt2
                0x44_72_C4, // accent1
                0xED_7D_31, // accent2
                0xA5_A5_A5, // accent3
                0xFF_C0_00, // accent4
                0x5B_9B_D5, // accent5
                0x70_AD_47, // accent6
                0x05_63_C1, // hlink
                0x95_4F_72, // folHlink
            ],
        }
    }
}

/// `theme` 属性下标 → `clrScheme` 下标。
///
/// Excel 的颜色选择器顺序是「背景 1 / 文本 1 / 背景 2 / 文本 2 / 强调 1..6 / …」，
/// 也就是把 `clrScheme` 的 (dk1, lt1) 与 (dk2, lt2) 两两对调：`theme="1"` 是**文本色**
/// （一般近黑），`theme="0"` 才是背景色（一般白）。
///
/// 这个对调**实测过**：`real-monthly.xlsx` 有 5 550 格用 `theme="1"` 且 fillId=0（无底色），
/// 若按 clrScheme 原序把 theme=1 当成 lt1=白，这些格子会变成白底白字。
const THEME_ORDER: [usize; 12] = [1, 0, 3, 2, 4, 5, 6, 7, 8, 9, 10, 11];

impl Theme {
    fn color(&self, idx: usize, tint: Option<f64>) -> Option<u32> {
        let base = *self.colors.get(*THEME_ORDER.get(idx)?)?;
        Some(match tint {
            Some(t) => apply_tint(base, t),
            None => base,
        })
    }
}

fn parse_theme(xml: &str) -> Theme {
    const NAMES: [&str; 12] = [
        "dk1", "lt1", "dk2", "lt2", "accent1", "accent2", "accent3", "accent4", "accent5",
        "accent6", "hlink", "folHlink",
    ];
    let mut theme = Theme::default();
    for (slot, name) in NAMES.iter().enumerate() {
        let open = format!("<a:{name}");
        let Some(start) = xml.find(&open) else {
            continue;
        };
        let Some(gt) = xml.get(start..).and_then(|r| r.find('>')) else {
            continue;
        };
        let body_start = start + gt + 1;
        let close = format!("</a:{name}>");
        let end = xml
            .get(body_start..)
            .and_then(|r| r.find(&close))
            .map(|p| body_start + p)
            .unwrap_or(body_start);
        let body = xml.get(body_start..end).unwrap_or("");
        // `<a:sysClr val="windowText" lastClr="000000"/>` 或 `<a:srgbClr val="4472C4"/>`
        let rgb = attr(body, "lastClr")
            .and_then(parse_hex_color)
            .or_else(|| attr(body, "val").and_then(parse_hex_color));
        if let (Some(rgb), Some(slot_ref)) = (rgb, theme.colors.get_mut(slot)) {
            *slot_ref = rgb;
        }
    }
    theme
}

/// `RRGGBB` / `AARRGGBB` → `0xRRGGBB`（透明通道直接丢，预览不画半透明）。
fn parse_hex_color(raw: &str) -> Option<u32> {
    let s = raw.trim();
    if s.len() != 6 && s.len() != 8 {
        return None;
    }
    if !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let tail = s.get(s.len() - 6..)?;
    u32::from_str_radix(tail, 16).ok()
}

/// 主题色的 tint：>0 向白提亮，<0 向黑压暗（与 POI `applyTint` 同一个近似式）。
fn apply_tint(rgb: u32, tint: f64) -> u32 {
    let t = tint.clamp(-1.0, 1.0);
    let channel = |shift: u32| -> u32 {
        let c = f64::from((rgb >> shift) & 0xFF);
        let v = if t < 0.0 {
            c * (1.0 + t)
        } else {
            c * (1.0 - t) + 255.0 * t
        };
        v.round().clamp(0.0, 255.0) as u32
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

/// 内建（旧 Excel 调色板）indexed 颜色，索引 0–63。`indexedColors` 自定义调色板不支持。
const INDEXED_COLORS: [u32; 64] = [
    0x00_00_00, 0xFF_FF_FF, 0xFF_00_00, 0x00_FF_00, 0x00_00_FF, 0xFF_FF_00, 0xFF_00_FF, 0x00_FF_FF,
    0x00_00_00, 0xFF_FF_FF, 0xFF_00_00, 0x00_FF_00, 0x00_00_FF, 0xFF_FF_00, 0xFF_00_FF, 0x00_FF_FF,
    0x80_00_00, 0x00_80_00, 0x00_00_80, 0x80_80_00, 0x80_00_80, 0x00_80_80, 0xC0_C0_C0, 0x80_80_80,
    0x99_99_FF, 0x99_33_66, 0xFF_FF_CC, 0xCC_FF_FF, 0x66_00_66, 0xFF_80_80, 0x00_66_CC, 0xCC_CC_FF,
    0x00_00_80, 0xFF_00_FF, 0xFF_FF_00, 0x00_FF_FF, 0x80_00_80, 0x80_00_00, 0x00_80_80, 0x00_00_FF,
    0x00_CC_FF, 0xCC_FF_FF, 0xCC_FF_CC, 0xFF_FF_99, 0x99_CC_FF, 0xFF_99_CC, 0xCC_99_FF, 0xFF_CC_99,
    0x33_66_FF, 0x33_CC_CC, 0x99_CC_00, 0xFF_CC_00, 0xFF_99_00, 0xFF_66_00, 0x66_66_99, 0x96_96_96,
    0x00_33_66, 0x33_99_66, 0x00_33_00, 0x33_33_00, 0x99_33_00, 0x99_33_66, 0x33_33_99, 0x33_33_33,
];

/// `<color …/>` 的四种写法 → `0xRRGGBB`。`auto`/缺失返回 `None`（= 用默认色）。
fn resolve_color(inner: &str, theme: &Theme) -> Option<u32> {
    if let Some(rgb) = attr(inner, "rgb").and_then(parse_hex_color) {
        return Some(rgb);
    }
    if let Some(idx) = attr(inner, "theme").and_then(parse_u32) {
        let tint = attr(inner, "tint").and_then(parse_f64);
        return theme.color(idx as usize, tint);
    }
    if let Some(idx) = attr(inner, "indexed").and_then(parse_u32) {
        return INDEXED_COLORS.get(idx as usize).copied();
    }
    None
}

// ---------------------------------------------------------------------------
// styles.xml
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Font {
    bold: bool,
    italic: bool,
    size_dpt: u16,
    color: Option<u32>,
}

impl Default for Font {
    fn default() -> Self {
        Self {
            bold: false,
            italic: false,
            size_dpt: CellStyle::default().size_dpt,
            color: None,
        }
    }
}

/// 一个 `cellXfs` 条目**预先算好**的东西（每格只用复制 `CellStyle` + 借用 `NumFormat`，
/// 不在 hot loop 里做格式化决策）。
struct XfCell {
    style: CellStyle,
    fmt: NumFormat,
    /// 水平对齐是 "general"（没显式指定）：数值按 Excel 习惯右对齐、文本左对齐。
    auto_align: bool,
}

impl Default for XfCell {
    fn default() -> Self {
        Self {
            style: CellStyle::default(),
            fmt: NumFormat::general(),
            auto_align: true,
        }
    }
}

struct StyleSheet {
    xfs: Vec<XfCell>,
    fallback: XfCell,
    /// 条件格式的差分样式（`<dxfs><dxf>…</dxf></dxfs>`），`<cfRule dxfId="…">` 索引到这里。
    ///
    /// 和 `cellXfs` 是**两套东西**：`xf` 是"整格完整样式"，`dxf` 只描述"相对格子原有样式
    /// 的差异"，条件格式命中时才叠上去。绝大多数文件没有 `<dxfs>`，所以是空 `Vec`。
    dxfs: Vec<Dxf>,
}

impl Default for StyleSheet {
    fn default() -> Self {
        Self {
            xfs: vec![XfCell::default()],
            fallback: XfCell::default(),
            dxfs: Vec::new(),
        }
    }
}

impl StyleSheet {
    fn get(&self, idx: u32) -> &XfCell {
        self.xfs.get(idx as usize).unwrap_or(&self.fallback)
    }
}

fn parse_styles(xml: &str, theme: &Theme) -> StyleSheet {
    let fonts = parse_fonts(xml, theme);
    let fills = parse_fills(xml, theme);
    let borders = parse_borders(xml);
    let num_fmts = parse_num_fmts(xml);

    let mut xfs: Vec<XfCell> = Vec::new();
    if let Some(at) = find_tag(xml, 0, "cellXfs") {
        if let Some((_, _, start)) = open_tag(xml, at) {
            let body = child_region(xml, start, "</cellXfs>");
            let mut i = 0;
            while let Some(x_at) = find_tag(body, i, "xf") {
                let Some((inner, self_closing, after)) = open_tag(body, x_at) else {
                    break;
                };
                i = after.max(x_at + 1);
                if xfs.len() >= MAX_XFS {
                    break;
                }
                let (content, next) = if self_closing {
                    ("", after)
                } else {
                    let end = body
                        .get(after..)
                        .and_then(|r| r.find("</xf>"))
                        .map(|p| after + p)
                        .unwrap_or(body.len());
                    (body.get(after..end).unwrap_or(""), end.saturating_add(4))
                };
                xfs.push(build_xf(
                    inner, content, &fonts, &fills, &borders, &num_fmts,
                ));
                i = next.min(body.len()).max(i);
            }
        }
    }
    if xfs.is_empty() {
        xfs.push(XfCell::default());
    }
    StyleSheet {
        xfs,
        fallback: XfCell::default(),
        dxfs: parse_dxfs(xml, theme),
    }
}

/// 从 `start` 起到 `closer` 之前的子串（closer 不存在则到结尾）。
fn child_region<'a>(xml: &'a str, start: usize, closer: &str) -> &'a str {
    let end = xml
        .get(start..)
        .and_then(|r| r.find(closer))
        .map(|p| start + p)
        .unwrap_or(xml.len());
    xml.get(start..end).unwrap_or("")
}

fn build_xf(
    inner: &str,
    content: &str,
    fonts: &[Font],
    fills: &[Option<u32>],
    borders: &[Border],
    num_fmts: &[(u32, String)],
) -> XfCell {
    let font = attr(inner, "fontId")
        .and_then(parse_u32)
        .and_then(|i| fonts.get(i as usize))
        .copied()
        .unwrap_or_default();
    let bg = attr(inner, "fillId")
        .and_then(parse_u32)
        .and_then(|i| fills.get(i as usize))
        .copied()
        .flatten();
    let border = attr(inner, "borderId")
        .and_then(parse_u32)
        .and_then(|i| borders.get(i as usize))
        .copied()
        .unwrap_or_default();

    let mut h_align = HAlign::Left;
    let mut v_align = VAlign::Bottom;
    let mut auto_align = true;
    let mut wrap = false;
    if let Some(a_at) = find_tag(content, 0, "alignment") {
        if let Some((a_inner, _, _)) = open_tag(content, a_at) {
            if let Some(h) = attr(a_inner, "horizontal") {
                match h {
                    "center" | "centerContinuous" | "distributed" => {
                        h_align = HAlign::Center;
                        auto_align = false;
                    }
                    "right" => {
                        h_align = HAlign::Right;
                        auto_align = false;
                    }
                    "left" | "justify" | "fill" => {
                        h_align = HAlign::Left;
                        auto_align = false;
                    }
                    // "general"：交给解析阶段按内容判断
                    _ => {}
                }
            }
            if let Some(v) = attr(a_inner, "vertical") {
                v_align = match v {
                    "top" => VAlign::Top,
                    "center" | "justify" | "distributed" => VAlign::Center,
                    _ => VAlign::Bottom,
                };
            }
            wrap = attr(a_inner, "wrapText").is_some_and(is_true);
        }
    }

    XfCell {
        style: CellStyle {
            bold: font.bold,
            italic: font.italic,
            size_dpt: font.size_dpt,
            colors: Colors {
                fg: font.color.unwrap_or(0x00_00_00),
                bg,
            },
            wrap,
            h_align,
            v_align,
            border,
        },
        fmt: num_format(
            attr(inner, "numFmtId").and_then(parse_u32).unwrap_or(0),
            num_fmts,
        ),
        auto_align,
    }
}

/// 取第一个 `<name …/>` 的属性串（`content` 内）。
fn first_tag_attrs<'a>(content: &'a str, name: &str) -> Option<&'a str> {
    let at = find_tag(content, 0, name)?;
    open_tag(content, at).map(|(inner, _, _)| inner)
}

/// 布尔型子元素（`<b/>`、`<i/>`），`val="0"` 视为假。
fn child_flag(content: &str, name: &str) -> bool {
    let Some(inner) = first_tag_attrs(content, name) else {
        return false;
    };
    match attr(inner, "val") {
        Some(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        None => true,
    }
}

/// 迭代 `<container>…</container>` 里每个 `child` 标签的（属性串, 内容）。
fn each_child<'a>(
    xml: &'a str,
    container: &str,
    child: &str,
    mut visit: impl FnMut(&'a str, &'a str),
) {
    let Some(at) = find_tag(xml, 0, container) else {
        return;
    };
    let Some((_, _, start)) = open_tag(xml, at) else {
        return;
    };
    let body = child_region(xml, start, &format!("</{container}>"));
    let mut i = 0;
    let mut count = 0usize;
    while let Some(c_at) = find_tag(body, i, child) {
        let Some((inner, self_closing, after)) = open_tag(body, c_at) else {
            break;
        };
        i = after.max(c_at + 1);
        let content = if self_closing {
            ""
        } else {
            let end = body
                .get(after..)
                .and_then(|r| r.find(&format!("</{child}>")))
                .map(|p| after + p)
                .unwrap_or(body.len());
            i = end.max(i);
            body.get(after..end).unwrap_or("")
        };
        visit(inner, content);
        count += 1;
        if count >= MAX_FONTS.max(MAX_FILLS) {
            break;
        }
    }
}

fn parse_fonts(xml: &str, theme: &Theme) -> Vec<Font> {
    let mut out = Vec::new();
    each_child(xml, "fonts", "font", |inner, content| {
        if out.len() >= MAX_FONTS {
            return;
        }
        let size_dpt = first_tag_attrs(content, "sz")
            .and_then(|a| attr(a, "val"))
            .and_then(parse_f64)
            .map(|pt| (pt * 10.0).round().clamp(10.0, 800.0) as u16)
            .unwrap_or_else(|| CellStyle::default().size_dpt);
        let color = first_tag_attrs(content, "color").and_then(|a| resolve_color(a, theme));
        let _ = inner;
        out.push(Font {
            bold: child_flag(content, "b"),
            italic: child_flag(content, "i"),
            size_dpt,
            color,
        });
    });
    out
}

fn parse_fills(xml: &str, theme: &Theme) -> Vec<Option<u32>> {
    let mut out = Vec::new();
    each_child(xml, "fills", "fill", |_inner, content| {
        if out.len() >= MAX_FILLS {
            return;
        }
        let Some(pattern) = first_tag_attrs(content, "patternFill") else {
            // gradientFill 之类的渐变底色：预览不画渐变，当无底色。
            out.push(None);
            return;
        };
        let kind = attr(pattern, "patternType").unwrap_or("none");
        if !kind.eq_ignore_ascii_case("solid") {
            out.push(None);
            return;
        }
        // solid 时 fgColor 才是"看得见的那块颜色"，bgColor 是图案色。
        let fill = first_tag_attrs(content, "fgColor")
            .and_then(|a| resolve_color(a, theme))
            .or_else(|| first_tag_attrs(content, "bgColor").and_then(|a| resolve_color(a, theme)));
        out.push(fill);
    });
    out
}

/// 条件格式用的差分样式（`<dxfs>` 里的一个 `<dxf>`）。
#[derive(Clone, Copy, Debug, Default)]
struct Dxf {
    /// `<dxf><font><color rgb="FF9C0006"/></font>` → 字色。
    font_rgb: Option<u32>,
    /// `<dxf><fill><patternFill><bgColor rgb="FFFFC7CE"/></patternFill></fill>` → 底色。
    fill_rgb: Option<u32>,
}

/// `<dxfs count="N"><dxf>…</dxf>…</dxfs>`：条件格式专用差分样式表。
///
/// **索引用处**：`<cfRule dxfId="2">` 指的是这里第 2 个 `<dxf>`，不是 `cellXfs`。
/// 所以自闭合/空 `<dxf/>` 也必须占位（否则后面所有下标都错位）。
fn parse_dxfs(xml: &str, theme: &Theme) -> Vec<Dxf> {
    let mut out: Vec<Dxf> = Vec::new();
    let Some(at) = find_tag(xml, 0, "dxfs") else {
        return out;
    };
    let Some((_, _, start)) = open_tag(xml, at) else {
        return out;
    };
    let body = child_region(xml, start, "</dxfs>");
    let mut i = 0;
    while let Some(d_at) = find_tag(body, i, "dxf") {
        let Some((_inner, self_closing, after)) = open_tag(body, d_at) else {
            break;
        };
        i = after.max(d_at + 1);
        if out.len() >= MAX_XFS {
            qlr_core::log_warn!(SCOPE, "条件格式 dxfs 超过 {MAX_XFS} 条，已截断");
            break;
        }
        let content = if self_closing {
            ""
        } else {
            let end = body
                .get(after..)
                .and_then(|r| r.find("</dxf>"))
                .map(|p| after + p)
                .unwrap_or(body.len());
            i = end.max(i);
            body.get(after..end).unwrap_or("")
        };
        out.push(parse_dxf(content, theme));
    }
    out
}

/// 一个 `<dxf>`：只取 `font/color` 与 `fill/patternFill/{bgColor|fgColor}` 两样。
///
/// Excel 写条件格式底色时用的是 `bgColor`（和普通 `<fill>` 的 `fgColor` 相反），
/// 但别的写文件工具两种都有，所以两个都认，`bgColor` 优先。
fn parse_dxf(content: &str, theme: &Theme) -> Dxf {
    let mut dxf = Dxf::default();
    if let Some(at) = find_tag(content, 0, "font") {
        if let Some((_, _, start)) = open_tag(content, at) {
            let font = child_region(content, start, "</font>");
            dxf.font_rgb = first_tag_attrs(font, "color").and_then(|a| resolve_color(a, theme));
        }
    }
    if let Some(at) = find_tag(content, 0, "fill") {
        if let Some((_, _, start)) = open_tag(content, at) {
            let fill = child_region(content, start, "</fill>");
            dxf.fill_rgb = first_tag_attrs(fill, "bgColor")
                .and_then(|a| resolve_color(a, theme))
                .or_else(|| first_tag_attrs(fill, "fgColor").and_then(|a| resolve_color(a, theme)));
        }
    }
    dxf
}

fn parse_borders(xml: &str) -> Vec<Border> {
    let mut out = Vec::new();
    each_child(xml, "borders", "border", |_inner, content| {
        if out.len() >= MAX_BORDERS {
            return;
        }
        out.push(Border {
            top: border_side(content, "top"),
            right: border_side(content, "right"),
            bottom: border_side(content, "bottom"),
            left: border_side(content, "left"),
        });
    });
    out
}

fn border_side(content: &str, name: &str) -> u8 {
    let Some(inner) = first_tag_attrs(content, name) else {
        return line::NONE;
    };
    match attr(inner, "style") {
        Some(style) => border_weight(style),
        None => line::NONE,
    }
}

/// OOXML 的 `style` → 模型里的粗细档位（0 无 / 1 细线 / 2 粗线 / 3 双线）。
///
/// 虚线/点线在预览里画成 1px 实线（模型只有"粗细"这一维，见 `qlr_core::sheet::line`）。
fn border_weight(style: &str) -> u8 {
    match style {
        "double" => line::DOUBLE,
        "medium" | "mediumDashed" | "mediumDashDot" | "mediumDashDotDot" | "slantDashDot"
        | "thick" => line::MEDIUM,
        "thin" | "hair" | "dotted" | "dashed" | "dashDot" | "dashDotDot" => line::THIN,
        _ => line::NONE,
    }
}

fn parse_num_fmts(xml: &str) -> Vec<(u32, String)> {
    let mut out: Vec<(u32, String)> = Vec::new();
    each_child(xml, "numFmts", "numFmt", |inner, _| {
        if out.len() >= MAX_NUM_FMTS {
            return;
        }
        let (Some(id), Some(code)) = (
            attr(inner, "numFmtId").and_then(parse_u32),
            attr(inner, "formatCode"),
        ) else {
            return;
        };
        out.push((id, decode_entities(code)));
    });
    out
}

// ---------------------------------------------------------------------------
// 数字格式
// ---------------------------------------------------------------------------

/// 内建 numFmtId → 格式串。没列出来的（地区/特殊格式，27–36 / 50–58）退回 General。
fn builtin_code(id: u32) -> Option<&'static str> {
    match id {
        0 => Some("General"),
        1 => Some("0"),
        2 => Some("0.00"),
        3 => Some("#,##0"),
        4 => Some("#,##0.00"),
        5 => Some("\"$\"#,##0_);(\"$\"#,##0)"),
        6 => Some("\"$\"#,##0_);[Red](\"$\"#,##0)"),
        7 => Some("\"$\"#,##0.00_);(\"$\"#,##0.00)"),
        8 => Some("\"$\"#,##0.00_);[Red](\"$\"#,##0.00)"),
        9 => Some("0%"),
        10 => Some("0.00%"),
        11 => Some("0.00E+00"),
        12 => Some("# ?/?"),
        13 => Some("# ??/??"),
        14 => Some("m/d/yyyy"),
        15 => Some("d-mmm-yy"),
        16 => Some("d-mmm"),
        17 => Some("mmm-yy"),
        18 => Some("h:mm AM/PM"),
        19 => Some("h:mm:ss AM/PM"),
        20 => Some("h:mm"),
        21 => Some("h:mm:ss"),
        22 => Some("m/d/yyyy h:mm"),
        37 => Some("_(* #,##0_);_(* \\(#,##0\\);_(* \"-\"_);_(@_)"),
        38 => Some("_(\"$\"* #,##0_);_(\"$\"* \\(#,##0\\);_(\"$\"* \"-\"_);_(@_)"),
        39 => Some("_(* #,##0.00_);_(* \\(#,##0.00\\);_(* \"-\"??_);_(@_)"),
        40 => Some("_(\"$\"* #,##0.00_);_(\"$\"* \\(#,##0.00\\);_(\"$\"* \"-\"??_);_(@_)"),
        45 => Some("mm:ss"),
        46 => Some("[h]:mm:ss"),
        47 => Some("mm:ss.0"),
        48 => Some("##0.0E+0"),
        49 => Some("@"),
        _ => None,
    }
}

fn num_format(id: u32, custom: &[(u32, String)]) -> NumFormat {
    if let Some((_, code)) = custom.iter().find(|(cid, _)| *cid == id) {
        return NumFormat::parse(code);
    }
    match builtin_code(id) {
        Some(code) => NumFormat::parse(code),
        None => NumFormat::general(),
    }
}

/// 日期/时间格式的一个片段。
#[derive(Clone, Copy, Debug, PartialEq)]
enum DateTok {
    /// 年份，`1`/`2` 位（2 = `yy`，否则四位）
    Year(u8),
    /// 月份数字（1/2 位）
    Month(u8),
    /// `mmm`
    MonthShort,
    /// `mmmm`
    MonthLong,
    /// 日（1/2 位）
    Day(u8),
    WeekdayShort,
    WeekdayLong,
    /// 12 小时制的小时（1/2 位）
    Hour12(u8),
    /// 24 小时制的小时（1/2 位）
    Hour24(u8),
    Minute(u8),
    Second(u8),
    /// `ss.0` 的十分之一秒
    Tenths,
    AmPm,
    /// 还没判定是"月"还是"分"的 `m` 段
    MonthOrMinute(u8),
    Lit(char),
}

/// 数字格式里"数字部分"的最小描述。
#[derive(Clone, Debug, Default, PartialEq)]
struct NumberSpec {
    /// 小数位数（`0.00` → 2）
    decimals: u8,
    /// 千分位（`#,##0`）
    thousands: bool,
    /// `%` 个数（每个 ×100）
    percent: u8,
    scientific: bool,
    /// 数字前面的字面量（含货币符号、会计左括号）
    prefix: String,
    /// 数字后面的字面量（含 `%`、会计右括号）
    suffix: String,
}

/// 格式串的一个分区（`;` 分隔）。
#[derive(Clone, Debug, PartialEq)]
enum Section {
    General,
    Text,
    /// 整段没有数字占位符（比如会计格式的零值区 `"-"`）→ 直接输出字面量
    Literal(String),
    Number(NumberSpec),
    Date(Vec<DateTok>),
}

/// 一条完整的数字格式：正/负/零三个分区 + 文本区。
#[derive(Clone, Debug)]
struct NumFormat {
    pos: Section,
    neg: Section,
    zero: Section,
    /// 负数分区是格式串里自己写的（自带了括号或负号），不要再补 `-`
    neg_from_code: bool,
    /// 整条就是 General：数值可以直接拿原文（省一次格式化成 String）
    plain_general: bool,
}

impl NumFormat {
    fn general() -> Self {
        Self {
            pos: Section::General,
            neg: Section::General,
            zero: Section::General,
            neg_from_code: false,
            plain_general: true,
        }
    }

    fn parse(code: &str) -> Self {
        if code.trim().is_empty() {
            return Self::general();
        }
        let sections = split_sections(code);
        let first = sections.first().map(|s| parse_section(s));
        let pos = first.unwrap_or(Section::General);
        let neg_sec = sections.get(1).map(|s| parse_section(s));
        let zero_sec = sections.get(2).map(|s| parse_section(s));
        let neg_from_code = neg_sec.is_some();
        let neg = neg_sec.unwrap_or_else(|| pos.clone());
        let zero = zero_sec.unwrap_or_else(|| pos.clone());
        let plain_general = sections.len() == 1 && matches!(pos, Section::General);
        Self {
            pos,
            neg,
            zero,
            neg_from_code,
            plain_general,
        }
    }

    /// 数值 → 显示文本。
    fn render(&self, v: f64) -> String {
        let negative = v.is_sign_negative() && v != 0.0;
        let section = if v == 0.0 {
            &self.zero
        } else if negative {
            &self.neg
        } else {
            &self.pos
        };
        match section {
            Section::General | Section::Text => general_number(v),
            Section::Literal(lit) => lit.clone(),
            Section::Date(toks) => render_date(v, toks),
            Section::Number(spec) => {
                let mut out = spec.prefix.clone();
                if negative && !self.neg_from_code {
                    out.push('-');
                }
                out.push_str(&render_numeric_body(v, spec));
                out.push_str(&spec.suffix);
                out
            }
        }
    }
}

/// 按 `;` 切分区，但跳过引号里与 `[...]` 里的 `;`。
fn split_sections(code: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut in_bracket = false;
    let mut chars = code.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                in_quote = !in_quote;
                cur.push(ch);
            }
            '\\' => {
                cur.push(ch);
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            '[' if !in_quote => {
                in_bracket = true;
                cur.push(ch);
            }
            ']' if !in_quote => {
                in_bracket = false;
                cur.push(ch);
            }
            ';' if !in_quote && !in_bracket => out.push(std::mem::take(&mut cur)),
            _ => cur.push(ch),
        }
    }
    out.push(cur);
    out
}

fn parse_section(code: &str) -> Section {
    let code = code.trim();
    if code.is_empty() {
        return Section::General;
    }
    if code.eq_ignore_ascii_case("general") {
        return Section::General;
    }
    if code == "@" {
        return Section::Text;
    }
    if looks_like_date(code) {
        return Section::Date(parse_date_tokens(code));
    }
    // 分数（`# ?/?`）不实现：按 General 输出小数，比输出 "0 /" 这种怪串强。
    if has_placeholder_slash(code) {
        return Section::General;
    }
    parse_number_section(code)
}

/// 判断格式串是不是日期/时间：**引号与 `[...]` 里的字母不算**。
///
/// 规则：出现 y/d/h/s 就是日期；只有 m 时要再确认"没有数字占位符"（`mmmm` 是月份名，
/// `#,##0` 不是月份）。
fn looks_like_date(code: &str) -> bool {
    let mut has_ydhs = false;
    let mut has_m = false;
    let mut has_placeholder = false;
    let bytes = code.as_bytes();
    let mut i = 0;
    let mut in_quote = false;
    while i < bytes.len() {
        let c = bytes.get(i).copied().unwrap_or(0);
        if c == b'"' {
            in_quote = !in_quote;
            i += 1;
            continue;
        }
        if in_quote {
            i += 1;
            continue;
        }
        match c {
            b'\\' | b'_' | b'*' => i += 2,
            b'[' => {
                i += 1;
                while i < bytes.len() && bytes.get(i) != Some(&b']') {
                    i += 1;
                }
                i += 1;
            }
            b'0' | b'#' | b'?' => {
                has_placeholder = true;
                i += 1;
            }
            _ => {
                match c.to_ascii_lowercase() {
                    b'y' | b'd' | b'h' | b's' => has_ydhs = true,
                    b'm' => has_m = true,
                    _ => {}
                }
                i += 1;
            }
        }
    }
    has_ydhs || (has_m && !has_placeholder)
}

/// 格式串里有没有"占位符之间的 `/`"（分数格式的特征）。
fn has_placeholder_slash(code: &str) -> bool {
    let mut seen_placeholder = false;
    let mut in_quote = false;
    let bytes = code.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes.get(i).copied().unwrap_or(0);
        if c == b'"' {
            in_quote = !in_quote;
            i += 1;
            continue;
        }
        if in_quote {
            i += 1;
            continue;
        }
        match c {
            b'\\' | b'_' | b'*' => i += 2,
            b'0' | b'#' | b'?' => {
                seen_placeholder = true;
                i += 1;
            }
            b'/' if seen_placeholder => return true,
            _ => i += 1,
        }
    }
    false
}

/// 解析"数字类"分区：把字面量拆到 prefix/suffix，把 `0`/`#`/`,`/`.`/`%`/`E+` 记成参数。
fn parse_number_section(code: &str) -> Section {
    let mut spec = NumberSpec::default();
    let mut seen_placeholder = false;
    let mut in_decimals = false;
    let chars: Vec<char> = code.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars.get(i).copied().unwrap_or(' ');
        match c {
            '"' => {
                i += 1;
                while i < chars.len() && chars.get(i) != Some(&'"') {
                    push_literal(
                        &mut spec,
                        seen_placeholder,
                        chars.get(i).copied().unwrap_or(' '),
                    );
                    i += 1;
                }
                i += 1;
            }
            '\\' => {
                if let Some(lit) = chars.get(i + 1).copied() {
                    push_literal(&mut spec, seen_placeholder, lit);
                }
                i += 2;
            }
            // `_x` = 留出一个字符的宽度；`*x` = 用 x 填充。两者都没有可见字面量。
            '_' | '*' => i += 2,
            '[' => {
                i += 1;
                let mut body = String::new();
                while i < chars.len() && chars.get(i) != Some(&']') {
                    body.push(chars.get(i).copied().unwrap_or(' '));
                    i += 1;
                }
                i += 1;
                // `[$$-409]` / `[$￥-804]` → 货币符号；`[Red]` / `[>=100]` / `[DBNum1]` → 丢
                for sym in currency_symbol(&body).chars() {
                    push_literal(&mut spec, seen_placeholder, sym);
                }
            }
            '0' | '#' | '?' => {
                seen_placeholder = true;
                if in_decimals {
                    spec.decimals = spec.decimals.saturating_add(1);
                }
                i += 1;
            }
            '.' => {
                if seen_placeholder {
                    in_decimals = true;
                } else {
                    push_literal(&mut spec, seen_placeholder, '.');
                }
                i += 1;
            }
            ',' => {
                if seen_placeholder && !in_decimals {
                    spec.thousands = true;
                } else if !seen_placeholder {
                    push_literal(&mut spec, seen_placeholder, ',');
                }
                i += 1;
            }
            '%' => {
                spec.percent = spec.percent.saturating_add(1);
                push_literal(&mut spec, seen_placeholder, '%');
                i += 1;
            }
            'E' | 'e' if matches!(chars.get(i + 1), Some('+' | '-')) => {
                spec.scientific = true;
                i += 2;
                while i < chars.len() && chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
                    i += 1;
                }
            }
            _ => {
                push_literal(&mut spec, seen_placeholder, c);
                i += 1;
            }
        }
    }
    if !seen_placeholder {
        // 整段没有数字占位符：纯字面量（会计格式的 `"-"` 零值区就是这种）。
        let lit = format!("{}{}", spec.prefix, spec.suffix);
        return Section::Literal(lit);
    }
    Section::Number(spec)
}

fn push_literal(spec: &mut NumberSpec, after_number: bool, ch: char) {
    if after_number {
        spec.suffix.push(ch);
    } else {
        spec.prefix.push(ch);
    }
}

/// `[$$-409]` → `$`；`[$￥-804]` → `￥`；`[$-409]` → 空；`[Red]` → 空。
fn currency_symbol(body: &str) -> String {
    let Some(rest) = body.strip_prefix('$') else {
        return String::new();
    };
    match rest.find('-') {
        Some(pos) => rest.get(..pos).unwrap_or("").to_string(),
        None => rest.to_string(),
    }
}

/// 解析日期/时间格式串 → 片段序列。
fn parse_date_tokens(code: &str) -> Vec<DateTok> {
    let upper = code.to_ascii_uppercase();
    let has_ampm = upper.contains("AM/PM") || upper.contains("A/P");
    let chars: Vec<char> = code.chars().collect();
    let mut toks: Vec<DateTok> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars.get(i).copied().unwrap_or(' ');
        match c {
            '"' => {
                i += 1;
                while i < chars.len() && chars.get(i) != Some(&'"') {
                    toks.push(DateTok::Lit(chars.get(i).copied().unwrap_or(' ')));
                    i += 1;
                }
                i += 1;
            }
            '\\' => {
                if let Some(lit) = chars.get(i + 1).copied() {
                    toks.push(DateTok::Lit(lit));
                }
                i += 2;
            }
            '_' | '*' => i += 2,
            '[' => {
                i += 1;
                let mut body = String::new();
                while i < chars.len() && chars.get(i) != Some(&']') {
                    body.push(chars.get(i).copied().unwrap_or(' '));
                    i += 1;
                }
                i += 1;
                let low = body.to_ascii_lowercase();
                match low.as_str() {
                    "h" | "hh" => toks.push(DateTok::Hour24(low.len().min(2) as u8)),
                    "m" | "mm" => toks.push(DateTok::Minute(low.len().min(2) as u8)),
                    "s" | "ss" => toks.push(DateTok::Second(low.len().min(2) as u8)),
                    // 颜色 / 条件 / 币种 / 历法：一律丢
                    _ => {}
                }
            }
            'y' | 'Y' => {
                let run = run_len(&chars, i, |c| c.eq_ignore_ascii_case(&'y'));
                toks.push(DateTok::Year(if run == 2 { 2 } else { 4 }));
                i += run;
            }
            'm' | 'M' => {
                let run = run_len(&chars, i, |c| c.eq_ignore_ascii_case(&'m'));
                toks.push(DateTok::MonthOrMinute(run.min(4) as u8));
                i += run;
            }
            'd' | 'D' => {
                let run = run_len(&chars, i, |c| c.eq_ignore_ascii_case(&'d'));
                toks.push(match run {
                    1 | 2 => DateTok::Day(run as u8),
                    3 => DateTok::WeekdayShort,
                    _ => DateTok::WeekdayLong,
                });
                i += run;
            }
            'h' | 'H' => {
                let run = run_len(&chars, i, |c| c.eq_ignore_ascii_case(&'h'));
                toks.push(if has_ampm {
                    DateTok::Hour12(run.min(2) as u8)
                } else {
                    DateTok::Hour24(run.min(2) as u8)
                });
                i += run;
            }
            's' | 'S' => {
                let run = run_len(&chars, i, |c| c.eq_ignore_ascii_case(&'s'));
                toks.push(DateTok::Second(run.min(2) as u8));
                i += run;
            }
            'a' | 'A' => {
                let rest: String = chars
                    .iter()
                    .skip(i)
                    .take(5)
                    .collect::<String>()
                    .to_ascii_uppercase();
                if rest.starts_with("AM/PM") {
                    toks.push(DateTok::AmPm);
                    i += 5;
                } else if rest.starts_with("A/P") {
                    toks.push(DateTok::AmPm);
                    i += 3;
                } else {
                    toks.push(DateTok::Lit(c));
                    i += 1;
                }
            }
            '.' => {
                let next_zero = chars.get(i + 1) == Some(&'0');
                if next_zero && matches!(toks.last(), Some(DateTok::Second(_))) {
                    toks.push(DateTok::Tenths);
                    i += 2;
                } else {
                    toks.push(DateTok::Lit('.'));
                    i += 1;
                }
            }
            _ => {
                toks.push(DateTok::Lit(c));
                i += 1;
            }
        }
    }
    // 把 m 段定成"月"还是"分"：看**最近的非字面量邻居**——`hh:mm:ss` 里的 `:` 是字面量，
    // 只盯紧邻元素会把 `mm` 判成月份。
    for idx in 0..toks.len() {
        if let DateTok::MonthOrMinute(run) = toks.get(idx).copied().unwrap_or(DateTok::Lit(' ')) {
            let prev = toks
                .get(..idx)
                .and_then(|before| before.iter().rev().find(|t| !matches!(t, DateTok::Lit(_))))
                .copied();
            let next = toks
                .get(idx + 1..)
                .and_then(|after| after.iter().find(|t| !matches!(t, DateTok::Lit(_))))
                .copied();
            let is_minute = matches!(
                prev,
                Some(DateTok::Hour12(_) | DateTok::Hour24(_) | DateTok::AmPm)
            ) || matches!(next, Some(DateTok::Second(_) | DateTok::Tenths));
            let tok = if is_minute {
                DateTok::Minute(run.min(2))
            } else {
                match run {
                    1 | 2 => DateTok::Month(run),
                    3 => DateTok::MonthShort,
                    _ => DateTok::MonthLong,
                }
            };
            if let Some(slot) = toks.get_mut(idx) {
                *slot = tok;
            }
        }
    }
    toks
}

fn run_len(chars: &[char], from: usize, pred: impl Fn(char) -> bool) -> usize {
    let mut n = 0;
    while let Some(c) = chars.get(from + n) {
        if !pred(*c) {
            break;
        }
        n += 1;
    }
    n.max(1)
}

// ----- 数值/日期渲染 -----

fn render_numeric_body(v: f64, spec: &NumberSpec) -> String {
    let mut x = v.abs();
    for _ in 0..spec.percent {
        x *= 100.0;
    }
    if !x.is_finite() {
        return general_number(x);
    }
    if spec.scientific {
        return scientific(x, spec.decimals);
    }
    let rounded = round_to(x, spec.decimals);
    let mut out = format!("{:.*}", usize::from(spec.decimals), rounded);
    if spec.thousands {
        out = group_thousands(&out);
    }
    out
}

/// Excel 的舍入是"四舍五入、远离零"（`(x*m).round()` 对正数就是这个行为）。
fn round_to(x: f64, decimals: u8) -> f64 {
    let scale = 10f64.powi(i32::from(decimals));
    let scaled = x * scale;
    if !scaled.is_finite() {
        return x;
    }
    scaled.round() / scale
}

fn group_thousands(s: &str) -> String {
    let (int_part, frac) = match s.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    };
    let (sign, digits) = match int_part.strip_prefix('-') {
        Some(d) => ("-", d),
        None => ("", int_part),
    };
    let mut out = String::with_capacity(s.len() + digits.len() / 3 + 1);
    out.push_str(sign);
    let n = digits.len();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (n - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if let Some(f) = frac {
        out.push('.');
        out.push_str(f);
    }
    out
}

fn scientific(v: f64, decimals: u8) -> String {
    if v == 0.0 {
        return format!("{:.*}E+00", usize::from(decimals), 0.0);
    }
    let mut exp = v.abs().log10().floor() as i32;
    let mut mant = v / 10f64.powi(exp);
    if mant.abs() >= 10.0 {
        mant /= 10.0;
        exp += 1;
    }
    let mut digits = format!("{:.*}", usize::from(decimals), mant.abs());
    trim_trailing_zeros(&mut digits);
    format!(
        "{}{digits}E{}{:02}",
        if v < 0.0 { "-" } else { "" },
        if exp < 0 { '-' } else { '+' },
        exp.abs()
    )
}

/// Excel 的 General 格式：最多 11 位有效数字，整数不带小数点，超大/超小走科学计数。
fn general_number(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 { "∞" } else { "-∞" }.to_string();
    }
    if v == 0.0 {
        return "0".to_string();
    }
    let abs = v.abs();
    if abs >= 1e11 || abs < 1e-9 {
        return scientific(v, 5);
    }
    let mut s = if abs >= 1.0 {
        let int_digits = abs.log10().floor() as i32 + 1;
        let decimals = (11 - int_digits).clamp(0, 10) as usize;
        format!("{v:.decimals$}")
    } else {
        let exp = abs.log10().floor() as i32;
        let decimals = (10 - exp).clamp(0, 15) as usize;
        format!("{v:.decimals$}")
    };
    trim_trailing_zeros(&mut s);
    s
}

fn trim_trailing_zeros(s: &mut String) {
    if !s.contains('.') {
        return;
    }
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
}

/// Excel 序列号（1900 日期系统）→ (年, 月, 日, 星期)。
///
/// **1900 闰年补偿**：Excel 认为 1900 年是闰年（为了兼容 Lotus 1-2-3），所以序列号 60
/// 对应一个根本不存在的 1900-02-29。于是 1–59 用 `1899-12-31` 起算，60 特判，≥61 用
/// `1899-12-30` 起算（这样序列号 25569 正好是 1970-01-01）。
fn serial_to_ymd(days: i64) -> (i32, u32, u32, usize) {
    let unix_days = if days < 61 {
        days - 25_568
    } else {
        days - 25_569
    };
    let (y, m, d) = civil_from_days(unix_days);
    // 1970-01-01 是周四：0 = 周日
    let weekday = ((unix_days + 4).rem_euclid(7)) as usize;
    if days == 60 {
        (1900, 2, 29, weekday)
    } else {
        (y as i32, m, d, weekday)
    }
}

/// Howard Hinnant 的 civil_from_days（与 `qlr_core::log` 里同一算法；那边是 private，
/// 这里不能引 `qlr-core` 的私有函数，所以复制一份 15 行纯算术）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

const MONTHS_SHORT: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const MONTHS_LONG: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const WEEKDAYS_SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const WEEKDAYS_LONG: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

fn render_date(v: f64, toks: &[DateTok]) -> String {
    if !v.is_finite() || v < 0.0 {
        // 负日期 / 非数：Excel 显示 `#####`，这里退回数字本身，信息量更大。
        return general_number(v);
    }
    let days = v.trunc();
    let mut day_index = days as i64;
    let mut secs = ((v - days) * 86_400.0).round() as i64;
    if secs >= 86_400 {
        secs -= 86_400;
        day_index += 1;
    }
    let (year, month, day, weekday) = serial_to_ymd(day_index);
    let hour = secs / 3600;
    let minute = (secs % 3600) / 60;
    let second = secs % 60;
    let tenths = (((v - days) * 864_000.0).round() as i64).rem_euclid(10);

    let mut out = String::new();
    for tok in toks {
        match *tok {
            DateTok::Year(digits) => {
                if digits == 2 {
                    out.push_str(&format!("{:02}", year.rem_euclid(100)));
                } else {
                    out.push_str(&format!("{year:04}"));
                }
            }
            DateTok::Month(digits) => {
                if digits >= 2 {
                    out.push_str(&format!("{month:02}"));
                } else {
                    out.push_str(&format!("{month}"));
                }
            }
            DateTok::MonthShort => out.push_str(
                MONTHS_SHORT
                    .get(month.saturating_sub(1) as usize)
                    .copied()
                    .unwrap_or(""),
            ),
            DateTok::MonthLong => out.push_str(
                MONTHS_LONG
                    .get(month.saturating_sub(1) as usize)
                    .copied()
                    .unwrap_or(""),
            ),
            DateTok::Day(digits) => {
                if digits >= 2 {
                    out.push_str(&format!("{day:02}"));
                } else {
                    out.push_str(&format!("{day}"));
                }
            }
            DateTok::WeekdayShort => {
                out.push_str(WEEKDAYS_SHORT.get(weekday).copied().unwrap_or(""))
            }
            DateTok::WeekdayLong => out.push_str(WEEKDAYS_LONG.get(weekday).copied().unwrap_or("")),
            DateTok::Hour12(digits) => {
                let h = if hour % 12 == 0 { 12 } else { hour % 12 };
                if digits >= 2 {
                    out.push_str(&format!("{h:02}"));
                } else {
                    out.push_str(&format!("{h}"));
                }
            }
            DateTok::Hour24(digits) => {
                if digits >= 2 {
                    out.push_str(&format!("{hour:02}"));
                } else {
                    out.push_str(&format!("{hour}"));
                }
            }
            DateTok::Minute(digits) => {
                if digits >= 2 {
                    out.push_str(&format!("{minute:02}"));
                } else {
                    out.push_str(&format!("{minute}"));
                }
            }
            DateTok::Second(digits) => {
                if digits >= 2 {
                    out.push_str(&format!("{second:02}"));
                } else {
                    out.push_str(&format!("{second}"));
                }
            }
            DateTok::Tenths => out.push_str(&format!("{tenths}")),
            DateTok::AmPm => out.push_str(if hour < 12 { "AM" } else { "PM" }),
            DateTok::MonthOrMinute(_) => {}
            DateTok::Lit(ch) => out.push(ch),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// sharedStrings.xml
// ---------------------------------------------------------------------------

fn parse_shared_strings(xml: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    let mut i = 0;
    while let Some(at) = find_tag(xml, i, "si") {
        let Some((_, self_closing, after)) = open_tag(xml, at) else {
            break;
        };
        i = after.max(at + 1);
        if out.len() >= MAX_SHARED_STRINGS || bytes > MAX_SHARED_BYTES {
            qlr_core::log_warn!(
                SCOPE,
                "共享字符串超过上限（{} 条 / {} 字节），后面的按空串处理",
                out.len(),
                bytes
            );
            break;
        }
        if self_closing {
            out.push(String::new());
            continue;
        }
        let end = xml
            .get(after..)
            .and_then(|r| r.find("</si>"))
            .map(|p| after + p)
            .unwrap_or(xml.len());
        let text = collect_text(xml, after, end);
        bytes = bytes.saturating_add(text.len());
        out.push(text);
        i = end.saturating_add(5);
    }
    out
}

/// 拼接 `<si>`/`<is>` 里的所有 `<t>`（富文本多段），**跳过注音 `<rPh>`**（否则文本会重复）。
fn collect_text(xml: &str, mut i: usize, end: usize) -> String {
    let mut out = String::new();
    let mut phonetic = false;
    while i < end {
        let Some(at) = xml.get(i..end).and_then(|r| r.find('<')).map(|p| i + p) else {
            break;
        };
        let name = tag_name(xml, at);
        let Some((_, self_closing, after)) = open_tag(xml, at) else {
            break;
        };
        i = after.max(at + 1);
        match name {
            "rPh" => phonetic = true,
            "/rPh" => phonetic = false,
            "t" if !phonetic && !self_closing => {
                let region = xml.get(i..end).unwrap_or("");
                let (raw, next) = text_until_tag(region, 0);
                out.push_str(&decode_entities(raw));
                i = i.saturating_add(next).min(end);
            }
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// worksheets/sheetN.xml
// ---------------------------------------------------------------------------

fn parse_worksheet(xml: &str, shared: &[String], styles: &StyleSheet) -> Sheet {
    let (default_col_w, default_row_h) = sheet_defaults(xml);
    let (frozen_rows, frozen_cols) = parse_frozen(xml);
    let bands = parse_col_bands(xml, default_col_w);
    let merges = parse_merges(xml);

    let mut cells: Vec<Cell> = Vec::new();
    let mut row_heights: Vec<u32> = Vec::new();
    let mut used_cols: u32 = 0;
    let mut used_rows: u32 = 0;
    let mut truncated = false;
    let mut disordered = false;
    let mut prev_row: i64 = -1;

    if let Some(sd_at) = find_tag(xml, 0, "sheetData") {
        let start = open_tag(xml, sd_at).map(|(_, _, after)| after).unwrap_or(0);
        let body = child_region(xml, start, "</sheetData>");
        let mut i = 0;
        while let Some(r_at) = find_tag(body, i, "row") {
            let Some((r_inner, r_self_closing, r_after)) = open_tag(body, r_at) else {
                break;
            };
            i = r_after.max(r_at + 1);
            let row_index: i64 = match attr(r_inner, "r").and_then(parse_u32) {
                Some(n) => i64::from(n.saturating_sub(1)),
                None => prev_row + 1,
            };
            if row_index < 0 {
                continue;
            }
            prev_row = row_index;
            let row = row_index.min(i64::from(u32::MAX)) as u32;
            if row < MAX_ROWS {
                let idx = row as usize;
                if row_heights.len() <= idx {
                    row_heights.resize(idx + 1, default_row_h);
                }
                let hidden = attr(r_inner, "hidden").is_some_and(is_true);
                let height = attr(r_inner, "ht").and_then(parse_f64).map(row_height_px);
                if let Some(slot) = row_heights.get_mut(idx) {
                    if hidden {
                        *slot = 0;
                    } else if let Some(px) = height {
                        *slot = px;
                    }
                }
                used_rows = used_rows.max(row.saturating_add(1));
            }
            if r_self_closing {
                continue;
            }

            let row_end = body
                .get(r_after..)
                .and_then(|r| r.find("</row>"))
                .map(|p| r_after + p)
                .unwrap_or(body.len());
            let row_src = body.get(r_after..row_end).unwrap_or("");
            prev_col_reset(
                &mut disordered,
                &mut cells,
                row,
                row_src,
                shared,
                styles,
                &mut used_cols,
                &mut truncated,
            );
            i = row_end.saturating_add(6).min(body.len());
        }
    }

    if disordered {
        cells.sort_unstable_by_key(|c| (c.row, c.col));
    }
    if truncated {
        qlr_core::log_warn!(SCOPE, "工作表单元格超过 {MAX_CELLS} 个，已截断");
    }

    // 条件格式：必须等 cells 排好序（色阶/数据条要先按区域收齐数值算 min/max，
    // `cellIs` 要改命中格的样式）。绝大多数文件根本没有这个块，所以先做一次廉价子串
    // 判断再进解析——没有条件格式的路径（含 real-monthly 的 5 张表）开销是 μs 级。
    if xml.contains("conditionalFormatting") {
        apply_conditional_formatting(xml, &mut cells, styles);
    }

    // 列数：以**实际用到的**列为准。Excel 的滚动范围也是"用到的范围"，不是整张表的
    // 16384 列；而 `<cols>` 里 `<col min="98" max="16384" width="…"/>` 这种"剩余整表"
    // band 只该用来定宽，不能拿来撑列数——实测 `real-monthly.xlsx` 的「销售数据分天」
    // 就有这么一条，撑开会把 131 列的表变成 16384 列的横向荒漠。
    // 只有"一个单元格都没有"的纯格式表才退回 band 的最右列。
    let mut cols = used_cols;
    if cols == 0 {
        cols = bands
            .iter()
            .map(|(_, max, _)| max.saturating_add(1))
            .max()
            .unwrap_or(0);
    }
    cols = cols.min(MAX_COLS);
    let rows = (row_heights.len() as u32).max(used_rows).min(MAX_ROWS);

    let mut col_widths = vec![default_col_w; cols as usize];
    for (min, max, px) in &bands {
        for col in *min..=*max {
            if let Some(slot) = col_widths.get_mut(col as usize) {
                *slot = *px;
            }
        }
    }
    row_heights.resize(rows as usize, default_row_h);

    Sheet {
        name: String::new(),
        cols,
        rows,
        col_widths,
        row_heights,
        cells,
        merges,
        frozen_rows,
        frozen_cols,
    }
}

/// 扫一行里的 `<c>`。抽出来只是为了别让 `parse_worksheet` 太长（参数都是就地借用）。
#[allow(clippy::too_many_arguments)]
fn prev_col_reset(
    disordered: &mut bool,
    cells: &mut Vec<Cell>,
    row: u32,
    row_src: &str,
    shared: &[String],
    styles: &StyleSheet,
    used_cols: &mut u32,
    truncated: &mut bool,
) {
    let mut i = 0;
    let mut prev_col: i64 = -1;
    while let Some(c_at) = find_tag(row_src, i, "c") {
        let Some((c_inner, c_self_closing, c_after)) = open_tag(row_src, c_at) else {
            break;
        };
        i = c_after.max(c_at + 1);
        let col_index: i64 = match attr(c_inner, "r").and_then(col_of_ref) {
            Some(c) => i64::from(c),
            None => prev_col + 1,
        };
        if col_index < 0 {
            continue;
        }
        prev_col = col_index;
        let col = col_index.min(i64::from(u32::MAX)) as u32;
        *used_cols = (*used_cols).max(col.saturating_add(1));

        let kind = attr(c_inner, "t").unwrap_or("n");
        let xf = styles.get(attr(c_inner, "s").and_then(parse_u32).unwrap_or(0));
        let c_end = if c_self_closing {
            c_after
        } else {
            row_src
                .get(c_after..)
                .and_then(|r| r.find("</c>"))
                .map(|p| c_after + p)
                .unwrap_or(row_src.len())
        };

        let mut text: Option<String> = None;
        let mut numeric = false;
        if !c_self_closing {
            // **一定要先把搜索范围夹到本单元格**：`find_tag` 是"从这里往后扫到结尾"，
            // 不夹的话每个格子都要把整行再扫一遍（97 列 × 40 KB 行 = 每行 4 MB），
            // 实测把 353×97 的表从 20 ms 拖到 110 ms。
            let cell_src = row_src.get(c_after..c_end).unwrap_or("");
            if kind == "inlineStr" {
                if let Some((_, _, start)) =
                    find_tag(cell_src, 0, "is").and_then(|p| open_tag(cell_src, p))
                {
                    text = Some(finish_text(collect_text(cell_src, start, cell_src.len())));
                }
            } else if let Some((_, _, start)) =
                find_tag(cell_src, 0, "v").and_then(|p| open_tag(cell_src, p))
            {
                let region = cell_src.get(start..).unwrap_or("");
                let (raw, _) = text_until_tag(region, 0);
                text = match kind {
                    "s" => parse_u32(raw.trim())
                        .and_then(|idx| shared.get(idx as usize))
                        .map(|s| finish_text(s.clone())),
                    // 公式字符串结果 / ISO 日期：原样显示
                    "str" | "d" => Some(finish_text(decode_entities(raw))),
                    "b" => Some(if raw.trim() == "1" { "TRUE" } else { "FALSE" }.to_string()),
                    // 错误值（#DIV/0! / #N/A …）原样显示
                    "e" => Some(finish_text(decode_entities(raw))),
                    _ => {
                        numeric = true;
                        Some(finish_text(number_cell_text(raw, &xf.fmt)))
                    }
                };
            }
        }

        // 空文本 + 没有任何可见样式（底色/边框）→ 当空格子，不占模型内存。
        let mut style = xf.style;
        let has_visual = style.colors.bg.is_some() || !style.border.is_empty();
        let has_text = text.as_deref().is_some_and(|t| !t.is_empty());
        if !has_text && !has_visual {
            continue;
        }
        if col >= MAX_COLS || row >= MAX_ROWS {
            continue;
        }
        if numeric && xf.auto_align {
            style.h_align = HAlign::Right;
        }
        if cells.len() >= MAX_CELLS {
            *truncated = true;
            continue;
        }
        if let Some(last) = cells.last() {
            if (row, col) < (last.row, last.col) {
                *disordered = true;
            }
        }
        cells.push(Cell {
            row,
            col,
            text: text.unwrap_or_default(),
            style,
            numeric,
            data_bar: None,
        });
    }
}

/// 数值单元格 → 文本（General 且原文干净时直通，省一次格式化）。
fn number_cell_text(raw: &str, fmt: &NumFormat) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    if fmt.plain_general && is_plain_decimal(raw) {
        return raw.to_string();
    }
    match raw.parse::<f64>() {
        Ok(v) => fmt.render(v),
        Err(_) => decode_entities(raw),
    }
}

/// 原文本身就已经是 General 的规范写法（`10.5` / `-0.1` / `0`），可以直通。
fn is_plain_decimal(s: &str) -> bool {
    if s.is_empty() || s.len() > 12 {
        return false;
    }
    let mut dots = 0;
    for (idx, byte) in s.bytes().enumerate() {
        match byte {
            b'-' if idx == 0 => {}
            b'.' => {
                dots += 1;
                if dots > 1 {
                    return false;
                }
            }
            b'0'..=b'9' => {}
            _ => return false,
        }
    }
    !s.ends_with('.')
}

/// 文本收尾：清控制字符 + 截断到 `MAX_TEXT_CHARS` 字符。
fn finish_text(mut s: String) -> String {
    if s.bytes().any(|b| b < 0x20 && b != b'\n') || s.contains('\u{7f}') {
        s = s
            .chars()
            .filter(|c| *c == '\n' || (*c >= ' ' && *c != '\u{7f}'))
            .collect();
    }
    if s.len() > MAX_TEXT_CHARS && s.chars().count() > MAX_TEXT_CHARS {
        let cut = s
            .char_indices()
            .nth(MAX_TEXT_CHARS)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        s.truncate(cut);
    }
    s
}

/// `sheetFormatPr` 的默认列宽/行高（缺省回落到模型常量）。
fn sheet_defaults(xml: &str) -> (u32, u32) {
    let Some(inner) = first_tag_attrs(xml, "sheetFormatPr") else {
        return (DEFAULT_COL_WIDTH, DEFAULT_ROW_HEIGHT);
    };
    let col = attr(inner, "defaultColWidth")
        .and_then(parse_f64)
        .map(col_width_px)
        .unwrap_or(DEFAULT_COL_WIDTH);
    let row = attr(inner, "defaultRowHeight")
        .and_then(parse_f64)
        .map(row_height_px)
        .unwrap_or(DEFAULT_ROW_HEIGHT);
    (col, row)
}

/// `<pane xSplit ySplit state="frozen">` → (冻结行数, 冻结列数)。
fn parse_frozen(xml: &str) -> (u32, u32) {
    let Some(inner) = first_tag_attrs(xml, "pane") else {
        return (0, 0);
    };
    let state = attr(inner, "state").unwrap_or("");
    if state != "frozen" && state != "frozenSplit" {
        return (0, 0);
    }
    let split = |key: &str, cap: u32| -> u32 {
        attr(inner, key)
            .and_then(parse_f64)
            .map(|v| v.round().clamp(0.0, f64::from(cap)) as u32)
            .unwrap_or(0)
    };
    (split("ySplit", MAX_ROWS), split("xSplit", MAX_COLS))
}

/// `<cols><col min max width customWidth hidden/></cols>` → (起始列, 结束列, 像素宽)。
fn parse_col_bands(xml: &str, default_w: u32) -> Vec<(u32, u32, u32)> {
    let mut out: Vec<(u32, u32, u32)> = Vec::new();
    let Some(at) = find_tag(xml, 0, "cols") else {
        return out;
    };
    let Some((_, _, start)) = open_tag(xml, at) else {
        return out;
    };
    let body = child_region(xml, start, "</cols>");
    let mut i = 0;
    while let Some(c_at) = find_tag(body, i, "col") {
        let Some((inner, _, after)) = open_tag(body, c_at) else {
            break;
        };
        i = after.max(c_at + 1);
        if out.len() >= MAX_COL_BANDS {
            break;
        }
        let min = attr(inner, "min").and_then(parse_u32).unwrap_or(1).max(1) - 1;
        let max = attr(inner, "max")
            .and_then(parse_u32)
            .unwrap_or(min.saturating_add(2))
            .max(1)
            .saturating_sub(1);
        let hidden = attr(inner, "hidden").is_some_and(is_true);
        let px = if hidden {
            0
        } else {
            attr(inner, "width")
                .and_then(parse_f64)
                .map(col_width_px)
                .unwrap_or(default_w)
        };
        let last = MAX_COLS.saturating_sub(1);
        out.push((min.min(last), max.min(last), px));
    }
    out
}

/// `<mergeCells><mergeCell ref="A1:C3"/></mergeCells>`。
fn parse_merges(xml: &str) -> Vec<Merge> {
    let mut out: Vec<Merge> = Vec::new();
    let Some(at) = find_tag(xml, 0, "mergeCells") else {
        return out;
    };
    let Some((_, _, start)) = open_tag(xml, at) else {
        return out;
    };
    let body = child_region(xml, start, "</mergeCells>");
    let mut i = 0;
    while let Some(m_at) = find_tag(body, i, "mergeCell") {
        let Some((inner, _, after)) = open_tag(body, m_at) else {
            break;
        };
        i = after.max(m_at + 1);
        if out.len() >= MAX_MERGES {
            break;
        }
        let Some((from, to)) = attr(inner, "ref").and_then(|r| r.split_once(':')) else {
            continue;
        };
        let (Some((r1, c1)), Some((r2, c2))) = (parse_ref(from), parse_ref(to)) else {
            continue;
        };
        if r2 < r1 || c2 < c1 || r1 >= MAX_ROWS || c1 >= MAX_COLS {
            continue;
        }
        out.push(Merge {
            row: r1,
            col: c1,
            rows: (r2.min(MAX_ROWS - 1) - r1).saturating_add(1),
            cols: (c2.min(MAX_COLS - 1) - c1).saturating_add(1),
        });
    }
    out
}

// ---------------------------------------------------------------------------
// 条件格式（`<conditionalFormatting sqref="…"><cfRule …>`）
// ---------------------------------------------------------------------------
//
// 支持到什么程度（1.9.3 的验收面）：
//
// | `cfRule@type` | 做什么 |
// |---|---|
// | `colorScale` | 2/3 色标按数值线性插值，写 `cell.style.colors.bg` |
// | `dataBar`    | 算比例写 `cell.data_bar`（颜色不画，绘制端只要比例） |
// | `cellIs`     | 比较数值，命中后把 `dxfId` 指向的字色/底色叠上去 |
// | 其它（`expression` / `iconSet` / `top10` / `duplicateValues` / `containsText` / …） | 跳过 |
//
// 跳过是有意的：图标集画不出来、表达式需要公式求值器、top10 要整列排序——"猜一个
// 近似"只会把格子涂成错色，比不涂更误导。
//
// 反过来的近似（都在函数注释里标了）：
// - `percent`/`percentile` 一律按"在 min..max 上线性插值"算（Excel 的 percentile 是
//   排序后取分位点，我们不做排序）；
// - `<color theme="…"/>` 拿不到 theme（这一层只有 styles），整条规则跳过；
// - 条件格式与格子自身底色的优先级：**条件格式赢**（写 `colors.bg` 覆盖 `fillId` 的底色），
//   因为 Excel 里条件格式就是画在填充之上的那一层。

/// 色阶/数据条的一个取值端点（`<cfvo>`）。
#[derive(Clone, Copy, Debug)]
enum CfValue {
    /// 目标区域数值的最小值（`autoMin` 同义）。
    Min,
    /// 目标区域数值的最大值（`autoMax` 同义）。
    Max,
    /// 字面量（`type="num"`）。
    Num(f64),
    /// `type="percent"`：`val` 是 0–100，按 `min + (max - min) × val/100` 取。
    Percent(f64),
    /// `type="percentile"`：**近似**成 `Percent`（见上方小节说明，不做排序）。
    Percentile(f64),
}

impl CfValue {
    /// 端点 → 具体数值。`lo`/`hi` 是目标区域的数值 min/max。
    fn resolve(self, lo: f64, hi: f64) -> f64 {
        match self {
            Self::Min => lo,
            Self::Max => hi,
            Self::Num(v) => v,
            Self::Percent(p) | Self::Percentile(p) => lo + (hi - lo) * (p / 100.0),
        }
    }
}

/// 把本表的条件格式叠到已经解析好的单元格上。
///
/// **调用点**：`parse_worksheet` 里 cells 已排序之后、构造 `Sheet` 之前——
/// 色阶/数据条必须先把整片区域的数值收齐才能算 min/max（一边遍历一边算会算错）。
fn apply_conditional_formatting(xml: &str, cells: &mut [Cell], styles: &StyleSheet) {
    let mut i = 0;
    let mut rules = 0usize;
    let mut overflow = false;
    while let Some(block_at) = find_tag(xml, i, "conditionalFormatting") {
        let Some((block_inner, block_self_closing, block_after)) = open_tag(xml, block_at) else {
            break;
        };
        i = block_after.max(block_at + 1);
        if block_self_closing {
            continue;
        }
        let body = child_region(xml, block_after, "</conditionalFormatting>");
        i = block_after.saturating_add(body.len()).max(i);
        let Some(sqref) = attr(block_inner, "sqref") else {
            continue;
        };
        let ranges = parse_sqref(sqref);
        if ranges.is_empty() {
            continue;
        }
        let mut j = 0;
        while let Some(rule_at) = find_tag(body, j, "cfRule") {
            let Some((rule_inner, rule_self_closing, rule_after)) = open_tag(body, rule_at) else {
                break;
            };
            j = rule_after.max(rule_at + 1);
            if rules >= MAX_CF_RULES {
                overflow = true;
                break;
            }
            let content = if rule_self_closing {
                ""
            } else {
                let end = body
                    .get(rule_after..)
                    .and_then(|r| r.find("</cfRule>"))
                    .map(|p| rule_after + p)
                    .unwrap_or(body.len());
                j = end.max(j);
                body.get(rule_after..end).unwrap_or("")
            };
            rules += 1;
            apply_cf_rule(rule_inner, content, &ranges, cells, styles);
        }
        if overflow {
            break;
        }
    }
    if overflow {
        qlr_core::log_warn!(SCOPE, "条件格式规则超过 {MAX_CF_RULES} 条，已截断");
    }
}

/// 一条 `<cfRule>`：按 `type` 分派，只做三种，其它一律跳过（不 panic、不猜）。
fn apply_cf_rule(
    rule_inner: &str,
    content: &str,
    ranges: &[(u32, u32, u32, u32)],
    cells: &mut [Cell],
    styles: &StyleSheet,
) {
    match attr(rule_inner, "type").unwrap_or("") {
        "colorScale" => apply_color_scale(content, ranges, cells),
        "dataBar" => apply_data_bar(content, ranges, cells),
        "cellIs" => apply_cell_is(rule_inner, content, ranges, cells, styles),
        _ => {}
    }
}

/// 色阶：按区域 min/max 与色标数值插值，结果写 `colors.bg`（不动原字色）。
fn apply_color_scale(content: &str, ranges: &[(u32, u32, u32, u32)], cells: &mut [Cell]) {
    let (Some(points), Some(colors)) = (parse_cfvo_list(content), parse_cf_colors(content)) else {
        return;
    };
    // 色标数与颜色数不一致（坏文件）时按少的那个截断；不足两个色标就不是色阶。
    let stops: Vec<(CfValue, u32)> = points.iter().copied().zip(colors).collect();
    if stops.len() < 2 {
        return;
    }
    for &range in ranges {
        let hits = collect_numbers(cells, range);
        let Some((lo, hi)) = min_max(&hits) else {
            continue;
        };
        let resolved: Vec<(f64, u32)> = stops
            .iter()
            .map(|(point, rgb)| (point.resolve(lo, hi), *rgb))
            .collect();
        for (idx, value) in hits {
            let rgb = gradient_color(&resolved, value);
            if let Some(cell) = cells.get_mut(idx) {
                cell.style.colors.bg = Some(rgb);
            }
        }
    }
}

/// 数据条：只算比例（`0.0`–`1.0`）写 `data_bar`，颜色这次不画。
fn apply_data_bar(content: &str, ranges: &[(u32, u32, u32, u32)], cells: &mut [Cell]) {
    let Some(points) = parse_cfvo_list(content) else {
        return;
    };
    let (Some(first), Some(last)) = (points.first().copied(), points.last().copied()) else {
        return;
    };
    if points.len() < 2 {
        return;
    }
    for &range in ranges {
        let hits = collect_numbers(cells, range);
        let Some((lo, hi)) = min_max(&hits) else {
            continue;
        };
        let bar_lo = first.resolve(lo, hi);
        let bar_hi = last.resolve(lo, hi);
        for (idx, value) in hits {
            let ratio = if bar_hi > bar_lo {
                ((value - bar_lo) / (bar_hi - bar_lo)).clamp(0.0, 1.0)
            } else {
                // 整片区域同一个值：Excel 画满条，这里给满条（不与色阶的"中间色"混淆）。
                1.0
            };
            if let Some(cell) = cells.get_mut(idx) {
                cell.data_bar = Some(ratio as f32);
            }
        }
    }
}

/// `cellIs`：比数值，命中就把 `dxf` 的字色/底色叠上去。
///
/// 只认 `<formula>` 里的**数字字面量**：`$D$2`、`TODAY()`、`"文本"` 都丢掉整条规则
/// （没有求值器，猜只会误标）。
fn apply_cell_is(
    rule_inner: &str,
    content: &str,
    ranges: &[(u32, u32, u32, u32)],
    cells: &mut [Cell],
    styles: &StyleSheet,
) {
    let Some(dxf) = attr(rule_inner, "dxfId")
        .and_then(parse_u32)
        .and_then(|idx| styles.dxfs.get(idx as usize))
        .copied()
    else {
        return;
    };
    if dxf.font_rgb.is_none() && dxf.fill_rgb.is_none() {
        return;
    }
    let operator = attr(rule_inner, "operator").unwrap_or("");
    let args = parse_cf_formulas(content);
    if args.is_empty() {
        return;
    }
    for &range in ranges {
        // 先只读地记下命中格下标，再统一改样式：cells 不能同时被两种借用。
        let mut hits: Vec<usize> = Vec::new();
        each_in_range(cells, range, |idx, cell| {
            if let Some(value) = cell_value(cell) {
                if cell_is_hit(operator, value, &args) {
                    hits.push(idx);
                }
            }
        });
        for idx in hits {
            if let Some(cell) = cells.get_mut(idx) {
                if let Some(fg) = dxf.font_rgb {
                    cell.style.colors.fg = fg;
                }
                if let Some(bg) = dxf.fill_rgb {
                    cell.style.colors.bg = Some(bg);
                }
            }
        }
    }
}

/// `cellIs` 的比较语义。`args` 是 `<formula>` 里的数字（1 个或 2 个）。
fn cell_is_hit(operator: &str, value: f64, args: &[f64]) -> bool {
    let a = args.first().copied();
    let b = args.get(1).copied();
    // `text` 是格式化后的显示串，`equal` 用个小容差比较（`0.1+0.2` 那种毛刺不误判）。
    let close = |target: f64| (value - target).abs() <= target.abs().max(1.0) * 1e-9;
    match operator {
        "lessThan" => a.is_some_and(|a| value < a),
        "lessThanOrEqual" => a.is_some_and(|a| value <= a),
        "greaterThan" => a.is_some_and(|a| value > a),
        "greaterThanOrEqual" => a.is_some_and(|a| value >= a),
        "equal" => a.is_some_and(close),
        "notEqual" => a.is_some_and(|a| !close(a)),
        "between" => match (a, b) {
            (Some(a), Some(b)) => value >= a.min(b) && value <= a.max(b),
            _ => false,
        },
        "notBetween" => match (a, b) {
            (Some(a), Some(b)) => value < a.min(b) || value > a.max(b),
            _ => false,
        },
        _ => false,
    }
}

/// `<cfvo type="…" val="…"/>` 列表（最多 3 个）。
///
/// **认不出的 type 直接整体返回 `None`**：端点序列错一位，色阶就会整体错色，
/// 宁可不画。
fn parse_cfvo_list(content: &str) -> Option<Vec<CfValue>> {
    let mut out: Vec<CfValue> = Vec::new();
    let mut i = 0;
    while let Some(at) = find_tag(content, i, "cfvo") {
        let (inner, _, after) = open_tag(content, at)?;
        i = after.max(at + 1);
        if out.len() >= 3 {
            break;
        }
        let val = attr(inner, "val").and_then(parse_f64).unwrap_or(0.0);
        out.push(match attr(inner, "type").unwrap_or("") {
            "min" | "autoMin" => CfValue::Min,
            "max" | "autoMax" => CfValue::Max,
            "num" => CfValue::Num(val),
            "percent" => CfValue::Percent(val),
            "percentile" => CfValue::Percentile(val),
            _ => return None,
        });
    }
    Some(out)
}

/// 色阶里的 `<color rgb="…"/>` 列表（最多 3 个）。
///
/// 只认 `rgb`/`indexed`：`theme=` 这一层拿不到主题色表，按需求整条规则跳过（返回 `None`）。
fn parse_cf_colors(content: &str) -> Option<Vec<u32>> {
    let mut out: Vec<u32> = Vec::new();
    let mut i = 0;
    while let Some(at) = find_tag(content, i, "color") {
        let (inner, _, after) = open_tag(content, at)?;
        i = after.max(at + 1);
        if out.len() >= 3 {
            break;
        }
        out.push(color_attr(inner)?);
    }
    Some(out)
}

/// `<color …>` → `0xRRGGBB`（只要 `rgb` / `indexed`，`theme` 不认）。
fn color_attr(inner: &str) -> Option<u32> {
    if let Some(rgb) = attr(inner, "rgb").and_then(parse_hex_color) {
        return Some(rgb);
    }
    let idx = attr(inner, "indexed").and_then(parse_u32)?;
    INDEXED_COLORS.get(idx as usize).copied()
}

/// `<formula>10</formula>` → 数字（最多 2 个）。
fn parse_cf_formulas(content: &str) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::new();
    let mut i = 0;
    while let Some(at) = find_tag(content, i, "formula") {
        let Some((_, self_closing, after)) = open_tag(content, at) else {
            break;
        };
        i = after.max(at + 1);
        if out.len() >= 2 {
            break;
        }
        if self_closing {
            continue;
        }
        let (raw, next) = text_until_tag(content, after);
        i = next.max(i);
        if let Some(v) = literal_number(raw) {
            out.push(v);
        }
    }
    out
}

/// 纯数字字面量（`10` / `-1.5` / `1E3`）。`$D$2`、`"文本"`、`TODAY()` 都不算。
fn literal_number(raw: &str) -> Option<f64> {
    let v = raw.trim().parse::<f64>().ok()?;
    v.is_finite().then_some(v)
}

/// 条件格式用的数值：**只有数值格参与**（`numeric == true`）。
///
/// 文本格一律不参与——`10052953469532` 这种 SKU 也是"看起来像数字的文本"，
/// 让它进色阶就会把整列 SKU 涂上颜色（1.9.0 的 `####` 是同一个坑）。
///
/// `text` 已经是格式化后的显示串，所以两种写法要能认：
/// 千分位 `1,234.5` 去掉逗号；百分号 `12.3%` 按 **12.3** 取（不除 100）——
/// 同一列的口径一致，色阶只关心相对大小。
fn cell_value(cell: &Cell) -> Option<f64> {
    if !cell.numeric {
        return None;
    }
    if let Ok(v) = cell.text.parse::<f64>() {
        return v.is_finite().then_some(v);
    }
    let mut cleaned = String::with_capacity(cell.text.len());
    for ch in cell.text.chars() {
        if ch != ',' && ch != '%' && ch != '\u{a0}' && !ch.is_whitespace() {
            cleaned.push(ch);
        }
    }
    let v = cleaned.parse::<f64>().ok()?;
    v.is_finite().then_some(v)
}

/// 区域命中的（cells 下标，数值）。文本格、解析不出数值的格子不进来。
fn collect_numbers(cells: &[Cell], range: (u32, u32, u32, u32)) -> Vec<(usize, f64)> {
    let mut out: Vec<(usize, f64)> = Vec::new();
    each_in_range(cells, range, |idx, cell| {
        if let Some(v) = cell_value(cell) {
            out.push((idx, v));
        }
    });
    out
}

/// 遍历矩形区域里的格子（cells 按 `(row, col)` 升序，见 `parse_worksheet`）。
///
/// 注意**不能**用"起点二分 + 一段连续切片"来近似：行优先排序下，`C2:C5` 这种单列
/// 区域是不连续的（每行后面都跟着 D/E 列），切出来会把隔壁列也算进去。所以按行窗口
/// 二分定位起点，再逐格判列。命中数上限 `MAX_CF_CELLS`。
fn each_in_range(
    cells: &[Cell],
    (row0, col0, row1, col1): (u32, u32, u32, u32),
    mut visit: impl FnMut(usize, &Cell),
) {
    let start = cells.partition_point(|c| c.row < row0);
    let mut hits = 0usize;
    for (offset, cell) in cells.get(start..).unwrap_or(&[]).iter().enumerate() {
        if cell.row > row1 {
            break;
        }
        if cell.col < col0 || cell.col > col1 {
            continue;
        }
        hits += 1;
        if hits > MAX_CF_CELLS {
            qlr_core::log_warn!(SCOPE, "条件格式单个区域超过 {MAX_CF_CELLS} 格，已截断");
            break;
        }
        visit(start + offset, cell);
    }
}

/// 一堆 `(下标, 数值)` 的 min/max；空集合返回 `None`。
fn min_max(values: &[(usize, f64)]) -> Option<(f64, f64)> {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for (_, v) in values {
        lo = lo.min(*v);
        hi = hi.max(*v);
    }
    (lo <= hi).then_some((lo, hi))
}

/// 在色标之间按数值线性插值出 `0xRRGGBB`。
///
/// `stops` 已按声明顺序给出（2 或 3 个），第 0 项是 `CfValue` 换算后的数值：
/// - 落在第一个色标左侧 → 第一个颜色；落在最后一个右侧 → 最后一个颜色；
/// - 中间 → 找它落在哪两个相邻色标之间，按该段内的比例插值（这就是 Excel 的观感）；
/// - 所有色标数值相同（`hi == lo`）→ 整体取中间色（3 色标取中间那支，2 色标各半）。
fn gradient_color(stops: &[(f64, u32)], value: f64) -> u32 {
    let Some(&(first_value, first_rgb)) = stops.first() else {
        return 0;
    };
    let Some(&(last_value, last_rgb)) = stops.last() else {
        return first_rgb;
    };
    if stops.len() < 2 || !value.is_finite() {
        return first_rgb;
    }
    if last_value <= first_value {
        let mixed = lerp_rgb(first_rgb, last_rgb, 0.5);
        return if stops.len() >= 3 {
            stops.get(stops.len() / 2).map_or(mixed, |s| s.1)
        } else {
            mixed
        };
    }
    if value <= first_value {
        return first_rgb;
    }
    if value >= last_value {
        return last_rgb;
    }
    for pair in stops.windows(2) {
        let (Some(&(lo, lo_rgb)), Some(&(hi, hi_rgb))) = (pair.first(), pair.get(1)) else {
            continue;
        };
        if value < lo || value > hi {
            continue;
        }
        if hi <= lo {
            return hi_rgb;
        }
        let t = ((value - lo) / (hi - lo)).clamp(0.0, 1.0);
        return lerp_rgb(lo_rgb, hi_rgb, t);
    }
    last_rgb
}

/// 两色按 `t`（0–1）线性插值（每通道四舍五入）。
fn lerp_rgb(from: u32, to: u32, t: f64) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |shift: u32| -> u32 {
        let a = f64::from((from >> shift) & 0xFF);
        let b = f64::from((to >> shift) & 0xFF);
        (a + (b - a) * t).round().clamp(0.0, 255.0) as u32
    };
    (mix(16) << 16) | (mix(8) << 8) | mix(0)
}

/// `sqref`（空格分隔的区域列表）→ 0 基、含两端的 `(行起, 列起, 行止, 列止)`。
///
/// 语法只有 `A1` 与 `A1:C10` 两种；`$` 前缀、大小写、反序区域都容忍，认不出的片段
/// 直接丢（`sqref="!!!"` 就是空列表 = 这条规则不生效，不是错误）。区域数上限
/// `MAX_CF_RANGES`，超了截断并 `log_warn!`。
fn parse_sqref(sqref: &str) -> Vec<(u32, u32, u32, u32)> {
    let mut out: Vec<(u32, u32, u32, u32)> = Vec::new();
    for part in sqref.split_whitespace() {
        if out.len() >= MAX_CF_RANGES {
            qlr_core::log_warn!(SCOPE, "条件格式 sqref 区域超过 {MAX_CF_RANGES} 个，已截断");
            break;
        }
        let Some((from, to)) = split_ref_range(part) else {
            continue;
        };
        out.push(normalize_range(from, to));
    }
    out
}

/// `"A1:C10"` → `((0,0),(9,2))`；`"A1"` → 同一个点。
fn split_ref_range(part: &str) -> Option<((u32, u32), (u32, u32))> {
    match part.split_once(':') {
        Some((from, to)) => Some((parse_ref(from)?, parse_ref(to)?)),
        None => parse_ref(part).map(|p| (p, p)),
    }
}

/// 反序/越界的区域夹成合法矩形（行 < `MAX_ROWS`、列 < `MAX_COLS`）。
fn normalize_range((r1, c1): (u32, u32), (r2, c2): (u32, u32)) -> (u32, u32, u32, u32) {
    (
        r1.min(r2).min(MAX_ROWS - 1),
        c1.min(c2).min(MAX_COLS - 1),
        r1.max(r2).min(MAX_ROWS - 1),
        c1.max(c2).min(MAX_COLS - 1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// 造一个 xlsx（zip 条目名 → 内容），落在 `target/test-tmp/sheet/`。
    fn write_xlsx(file: &str, entries: &[(&str, &str)]) -> std::path::PathBuf {
        let path = crate::testutil::dir("sheet").join(file);
        let handle = std::fs::File::create(&path).expect("建 zip 失败");
        let mut writer = zip::ZipWriter::new(handle);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in entries {
            writer.start_file(*name, opts).expect("写 zip 条目失败");
            writer.write_all(body.as_bytes()).expect("写 zip 内容失败");
        }
        writer.finish().expect("收尾 zip 失败");
        path
    }

    const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/></Types>"#;

    const WORKBOOK: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><bookViews><workbookView activeTab="0"/></bookViews><sheets><sheet name="销售" sheetId="1" r:id="rId1"/><sheet name="明细 &amp; 备注" sheetId="7" r:id="rId2"/></sheets></workbook>"#;

    /// 同一份工作簿，但 `activeTab="1"`：验证 `sheet_index == 0` 跟着工作簿走。
    const WORKBOOK_ACTIVE1: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><bookViews><workbookView activeTab="1"/></bookViews><sheets><sheet name="销售" sheetId="1" r:id="rId1"/><sheet name="明细 &amp; 备注" sheetId="7" r:id="rId2"/></sheets></workbook>"#;

    // 故意让 rId1 → sheet9.xml（文件名不等于序号），验证走 rels 而不是猜名字。
    const RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId9" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/><Relationship Id="rId8" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheetB.xml"/><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet9.xml"/></Relationships>"#;

    const SHARED: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="3" uniqueCount="3"><si><t>金额</t></si><si><r><rPr><b/></rPr><t>富</t></r><r><t>文本</t></r></si><si><t xml:space="preserve">A&amp;B</t><rPh sb="0" eb="1"><t>注音</t></rPh></si></sst>"#;

    const STYLES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><numFmts count="2"><numFmt numFmtId="164" formatCode="0.0%"/><numFmt numFmtId="165" formatCode="yyyy-mm-dd"/></numFmts><fonts count="3"><font><sz val="11"/><name val="Calibri"/></font><font><b/><i/><sz val="14"/><color rgb="FFFF0000"/></font><font><sz val="10"/><color theme="1" tint="0.4"/></font></fonts><fills count="3"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill><fill><patternFill patternType="solid"><fgColor rgb="FFFFFF00"/><bgColor indexed="64"/></patternFill></fill></fills><borders count="2"><border><left/><right/><top/><bottom/></border><border><left style="thin"/><right style="medium"/><top style="double"/><bottom/></border></borders><cellXfs count="5"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/><xf numFmtId="0" fontId="1" fillId="2" borderId="1" xfId="0" applyFont="1"><alignment horizontal="center" vertical="center" wrapText="1"/></xf><xf numFmtId="164" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"/><xf numFmtId="165" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"><alignment horizontal="center"/></xf><xf numFmtId="4" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"/></cellXfs></styleSheet>"#;

    const SHEET_SALES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:C4"/><sheetViews><sheetView tabSelected="1" workbookViewId="0"><pane xSplit="1" ySplit="1" topLeftCell="B2" activePane="bottomRight" state="frozen"/></sheetView></sheetViews><sheetFormatPr defaultRowHeight="15"/><cols><col min="1" max="1" width="20" customWidth="1"/><col min="3" max="3" width="12.5" customWidth="1"/></cols><sheetData><row r="1" ht="30" customHeight="1"><c r="A1" s="1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row><row r="2"><c r="A2" s="2"><v>0.256</v></c><c r="B2" s="3"><v>45000</v></c><c r="C2" s="4"><v>1234.5</v></c></row><row r="3"><c r="A3" t="inlineStr"><is><t>行内</t></is></c><c r="B3" t="b"><v>1</v></c><c r="C3" t="e"><v>#DIV/0!</v></c></row><row r="4"><c r="A4" t="s"><v>2</v></c><c r="B4"><f>1+1</f><v>2</v></c></row></sheetData><mergeCells count="1"><mergeCell ref="A4:B4"/></mergeCells></worksheet>"#;

    const SHEET_DETAIL: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><v>7.5</v></c></row><row r="2"><c r="B2" t="inlineStr"><is><t>明细行</t></is></c></row></sheetData></worksheet>"#;

    /// 造一份最小工作簿。**文件名必须每个测试各不相同**：`#[test]` 默认并行跑，
    /// 同名文件会出现"一个测试在读、另一个正在写"的偶发失败（本地已复现过一次）。
    fn sample_xlsx(file: &str) -> std::path::PathBuf {
        write_xlsx(
            file,
            &[
                ("[Content_Types].xml", CONTENT_TYPES),
                ("xl/workbook.xml", WORKBOOK),
                ("xl/_rels/workbook.xml.rels", RELS),
                ("xl/sharedStrings.xml", SHARED),
                ("xl/styles.xml", STYLES),
                ("xl/worksheets/sheet9.xml", SHEET_SALES),
                ("xl/worksheets/sheetB.xml", SHEET_DETAIL),
            ],
        )
    }

    #[test]
    fn parses_workbook_sheet_list_and_follows_active_tab() {
        // activeTab = 1 → `parse_sheet` 默认显示第 2 张
        let path = write_xlsx(
            "active1.xlsx",
            &[
                ("[Content_Types].xml", CONTENT_TYPES),
                ("xl/workbook.xml", WORKBOOK_ACTIVE1),
                ("xl/_rels/workbook.xml.rels", RELS),
                ("xl/sharedStrings.xml", SHARED),
                ("xl/styles.xml", STYLES),
                ("xl/worksheets/sheet9.xml", SHEET_SALES),
                ("xl/worksheets/sheetB.xml", SHEET_DETAIL),
            ],
        );
        let doc = parse_sheet(&path).expect("解析最小 xlsx 失败");
        assert_eq!(doc.len(), 2);
        assert_eq!(doc.at(0).map(|s| s.name.as_str()), Some("销售"));
        // 实体解码：`明细 &amp; 备注`
        assert_eq!(doc.at(1).map(|s| s.name.as_str()), Some("明细 & 备注"));
        assert_eq!(doc.active, 1);
        assert_eq!(doc.sheet().map(|s| s.cells.len()), Some(2));
        // 没被选中的表只有名字，没有几何
        assert_eq!(
            doc.at(0).map(|s| (s.cols, s.rows, s.cells.len())),
            Some((0, 0, 0))
        );

        // activeTab = 0 的那份默认显示第一张
        let doc0 = parse_sheet(&sample_xlsx("active0.xlsx")).expect("解析失败");
        assert_eq!(doc0.active, 0);
        assert_eq!(doc0.sheet().map(|s| s.name.as_str()), Some("销售"));
    }

    #[test]
    fn sheet_index_pages_through_sheets_via_rels() {
        let path = sample_xlsx("paging.xlsx");
        // n = 1 → 下标 1（第 2 张）
        let second = parse_sheet_at(&path, 1).expect("切表失败");
        assert_eq!(second.active, 1);
        assert_eq!(second.sheet().map(|s| s.name.as_str()), Some("明细 & 备注"));
        assert_eq!(second.sheet().map(|s| s.cells.len()), Some(2));

        // n = 0 → 跟着 activeTab（这里是 0）；rId1 指向 worksheets/sheet9.xml，
        // 说明走的是 rels 而不是"文件名 = 序号"的猜测。
        let first = parse_sheet_at(&path, 0).expect("解析第一张失败");
        assert_eq!(first.active, 0);
        assert_eq!(first.sheet().map(|s| s.name.as_str()), Some("销售"));
        assert_eq!(first.sheet().map(|s| (s.cols, s.rows)), Some((3, 4)));

        // 越界夹到最后一张，不报错（壳层翻页到边界时的行为）
        let over = parse_sheet_at(&path, 99).expect("越界应夹取而非报错");
        assert_eq!(over.active, 1);
    }

    #[test]
    fn parses_geometry_styles_dates_and_merges() {
        let path = write_xlsx(
            "geometry.xlsx",
            &[
                ("xl/workbook.xml", WORKBOOK),
                ("xl/_rels/workbook.xml.rels", RELS),
                ("xl/sharedStrings.xml", SHARED),
                ("xl/styles.xml", STYLES),
                ("xl/worksheets/sheet9.xml", SHEET_SALES),
                ("xl/worksheets/sheetB.xml", SHEET_DETAIL),
            ],
        );
        let doc = parse_sheet_at(&path, 0).expect("解析失败");
        assert_eq!(doc.active, 0);
        let s = doc.sheet().expect("没有当前表");

        assert_eq!((s.cols, s.rows), (3, 4));
        // 列宽：20 字符 → 20*7+5；12.5 → 92.5 四舍五入；未声明的列用默认 64
        assert_eq!(s.col_widths, vec![145, DEFAULT_COL_WIDTH, 93]);
        // 行高：row1 ht=30pt → 40px；其余 sheetFormatPr defaultRowHeight=15pt → 20px
        assert_eq!(s.row_heights, vec![40, 20, 20, 20]);

        // 共享字符串 + 富文本拼接
        assert_eq!(s.cell(0, 0).map(|c| c.text.as_str()), Some("金额"));
        assert_eq!(s.cell(0, 1).map(|c| c.text.as_str()), Some("富文本"));
        // 注音被跳过
        assert_eq!(s.cell(3, 0).map(|c| c.text.as_str()), Some("A&B"));

        // 自定义百分比格式 + general 对齐 → 右对齐
        let pct = s.cell(1, 0).expect("A2");
        assert_eq!(pct.text, "25.6%");
        assert_eq!(pct.style.h_align, HAlign::Right);

        // 日期（序列号 45000 → 2023-03-15）
        assert_eq!(s.cell(1, 1).map(|c| c.text.as_str()), Some("2023-03-15"));
        // 显式 horizontal=center 不被改
        assert_eq!(s.cell(1, 1).map(|c| c.style.h_align), Some(HAlign::Center));

        // 内建 4 = #,##0.00
        assert_eq!(s.cell(1, 2).map(|c| c.text.as_str()), Some("1,234.50"));

        // inlineStr / 布尔 / 错误值 / 公式缓存值
        assert_eq!(s.cell(2, 0).map(|c| c.text.as_str()), Some("行内"));
        assert_eq!(s.cell(2, 1).map(|c| c.text.as_str()), Some("TRUE"));
        assert_eq!(s.cell(2, 2).map(|c| c.text.as_str()), Some("#DIV/0!"));
        assert_eq!(s.cell(3, 1).map(|c| c.text.as_str()), Some("2"));

        // 样式：粗斜体 / 14pt / 红字 / 黄底 / 边框权重 / 换行 / 居中对齐
        let head = s.cell(0, 0).expect("A1");
        assert!(head.style.bold && head.style.italic);
        assert_eq!(head.style.size_dpt, 140);
        assert_eq!(head.style.colors.fg, 0x00_FF_00_00);
        assert_eq!(head.style.colors.bg, Some(0x00_FF_FF_00));
        assert!(head.style.wrap);
        assert_eq!(head.style.h_align, HAlign::Center);
        assert_eq!(head.style.v_align, VAlign::Center);
        assert_eq!(head.style.border.left, line::THIN);
        assert_eq!(head.style.border.right, line::MEDIUM);
        assert_eq!(head.style.border.top, line::DOUBLE);
        assert_eq!(head.style.border.bottom, line::NONE);

        // 合并 + 冻结
        assert_eq!(
            s.merges,
            vec![Merge {
                row: 3,
                col: 0,
                rows: 1,
                cols: 2
            }]
        );
        assert_eq!((s.frozen_rows, s.frozen_cols), (1, 1));
        assert!(s.covered_by_merge(3, 1));
        assert!(!s.covered_by_merge(3, 0));
    }

    #[test]
    fn cells_are_sorted_for_binary_search() {
        let s = parse_sheet(&sample_xlsx("sorted.xlsx")).expect("解析失败");
        let sheet = s.sheet().expect("没有当前表");
        let mut sorted = sheet.cells.clone();
        sorted.sort_unstable_by_key(|c| (c.row, c.col));
        assert_eq!(sheet.cells, sorted);
        // 二分查找必须能命中（模型契约）
        assert!(sheet.cell(0, 0).is_some());
    }

    #[test]
    fn theme_colors_resolve_with_tint() {
        let theme = Theme::default();
        // theme=1 → 文本色（dk1，黑）：Excel 的 dk1/lt1 与 clrScheme 顺序对调
        assert_eq!(theme.color(1, None), Some(0x00_00_00));
        // theme=0 → 背景色（lt1，白）
        assert_eq!(theme.color(0, None), Some(0xFF_FF_FF));
        // tint>0 提亮、tint<0 压暗
        let lifted = theme.color(1, Some(0.4)).unwrap_or(0);
        assert!(lifted > 0x00_00_00);
        let darkened = theme.color(0, Some(-0.5)).unwrap_or(0);
        assert!(darkened < 0xFF_FF_FF);
        // 越界不 panic
        assert_eq!(theme.color(99, None), None);
    }

    #[test]
    fn general_format_trims_and_switches_to_scientific() {
        let f = NumFormat::general();
        assert_eq!(f.render(0.0), "0");
        assert_eq!(f.render(10.5), "10.5");
        assert_eq!(f.render(-0.1), "-0.1");
        assert_eq!(f.render(1_234.5), "1234.5");
        // 浮点毛刺被抹掉（Excel 也显示 0.3）
        assert_eq!(f.render(0.300_000_000_000_000_04), "0.3");
        assert_eq!(f.render(1e12), "1E+12");
        assert!(f.render(1e-12).contains("E-12"));
        assert_eq!(f.render(f64::NAN), "NaN");
    }

    #[test]
    fn builtin_number_formats_render_like_excel() {
        assert_eq!(num_format(1, &[]).render(2.6), "3");
        assert_eq!(num_format(2, &[]).render(2.567), "2.57");
        assert_eq!(num_format(3, &[]).render(1_234_567.0), "1,234,567");
        assert_eq!(num_format(4, &[]).render(-1_234.567), "-1,234.57");
        assert_eq!(num_format(9, &[]).render(0.256), "26%");
        assert_eq!(num_format(10, &[]).render(0.256), "25.60%");
        assert_eq!(num_format(11, &[]).render(12_345.0), "1.23E+04");
        assert_eq!(num_format(49, &[]).render(1.0), "1");
    }

    #[test]
    fn accounting_format_uses_parens_for_negative() {
        // 内建 38 = `"$"#,##0`，负数走括号分区
        let f = num_format(38, &[]);
        assert_eq!(f.render(1_234.0), "$1,234");
        assert_eq!(f.render(-1_234.0), "$(1,234)");
    }

    #[test]
    fn date_formats_apply_1900_leap_year_compensation() {
        let iso = NumFormat::parse("yyyy-mm-dd");
        // 1900-02-29 是 Excel 虚构的那一天（序列号 60）
        assert_eq!(iso.render(59.0), "1900-02-28");
        assert_eq!(iso.render(60.0), "1900-02-29");
        assert_eq!(iso.render(61.0), "1900-03-01");
        assert_eq!(iso.render(1.0), "1900-01-01");
        assert_eq!(iso.render(25_569.0), "1970-01-01");
        assert_eq!(NumFormat::parse("m/d/yyyy").render(45_000.0), "3/15/2023");
        assert_eq!(
            NumFormat::parse("yyyy-mm-dd hh:mm:ss").render(45_000.5),
            "2023-03-15 12:00:00"
        );
        assert_eq!(NumFormat::parse("d-mmm-yy").render(45_000.0), "15-Mar-23");
    }

    #[test]
    fn time_formats_and_ampm() {
        assert_eq!(NumFormat::parse("hh:mm").render(0.5), "12:00");
        assert_eq!(NumFormat::parse("h:mm AM/PM").render(0.75), "6:00 PM");
        assert_eq!(NumFormat::parse("h:mm AM/PM").render(0.0), "12:00 AM");
        assert_eq!(NumFormat::parse("mm:ss").render(45.0 / 86400.0), "00:45");
        assert_eq!(NumFormat::parse("hh:mm:ss").render(0.25), "06:00:00");
    }

    #[test]
    fn literal_section_drives_zero_display() {
        // 会计格式的零值区 `"-"` → 显示"-"
        let f = NumFormat::parse("#,##0_);(#,##0);\"-\"");
        assert_eq!(f.render(0.0), "-");
        assert_eq!(f.render(-1.5), "(2)");
        assert_eq!(f.render(1.5), "2");
    }

    #[test]
    fn custom_format_with_currency_symbol() {
        let f = NumFormat::parse("[$￥-804]#,##0.00");
        assert_eq!(f.render(1234.5), "￥1,234.50");
    }

    #[test]
    fn text_fraction_and_malformed_formats_degrade_gracefully() {
        // 分数格式不支持 → 退化成 General 小数
        assert_eq!(NumFormat::parse("# ?/?").render(0.5), "0.5");
        // 只有文本区
        assert_eq!(NumFormat::parse("@").render(42.0), "42");
        // 空格式 / General
        assert_eq!(NumFormat::parse("").render(1.5), "1.5");
        assert_eq!(NumFormat::parse("General").render(1.5), "1.5");
        // 引号里的字母不能被当成日期
        assert_eq!(NumFormat::parse("0\" 天\"").render(3.0), "3 天");
    }

    #[test]
    fn col_width_conversion_is_named_and_matches_default() {
        assert_eq!(col_width_px(8.43), DEFAULT_COL_WIDTH);
        assert_eq!(col_width_px(20.0), 145);
        assert_eq!(col_width_px(0.0), 0);
        assert_eq!(col_width_px(-5.0), 0);
        assert_eq!(col_width_px(f64::NAN), 0);
        // 行高 15pt ↔ 20px（模型默认值）
        assert_eq!(row_height_px(15.0), DEFAULT_ROW_HEIGHT);
    }

    #[test]
    fn cell_ref_parsing_handles_absolute_and_out_of_range() {
        assert_eq!(col_of_ref("A1"), Some(0));
        assert_eq!(col_of_ref("B3"), Some(1));
        assert_eq!(col_of_ref("$AA$10"), Some(26));
        assert_eq!(col_of_ref("1"), None);
        assert_eq!(parse_ref("C7"), Some((6, 2)));
        assert_eq!(parse_ref("0"), None);
        assert_eq!(parse_ref(""), None);
    }

    #[test]
    fn non_zip_file_reports_error_without_panicking() {
        let path =
            crate::testutil::write_file("sheet", "not-a-zip.xlsx", "这不是 zip 容器".as_bytes());
        let err = parse_sheet(&path).expect_err("非 zip 必须报错");
        let msg = format!("{err}");
        assert!(msg.contains("xlsx"), "{msg}");
        assert!(!msg.is_empty());
    }

    #[test]
    fn truncated_zip_reports_error_without_panicking() {
        let good = sample_xlsx("trunc-src.xlsx");
        let bytes = std::fs::read(&good).expect("读回样本失败");
        let half = bytes.get(..bytes.len() / 2).unwrap_or(&[]);
        let path = crate::testutil::write_file("sheet", "truncated.xlsx", half);
        assert!(parse_sheet(&path).is_err());
    }

    #[test]
    fn missing_sheet_part_reports_error_without_panicking() {
        let path = write_xlsx(
            "no-sheet.xlsx",
            &[
                ("xl/workbook.xml", WORKBOOK),
                ("xl/_rels/workbook.xml.rels", RELS),
            ],
        );
        let err = parse_sheet(&path).expect_err("缺 sheet 必须报错");
        assert!(format!("{err}").contains("工作表"), "{err}");
    }

    #[test]
    fn workbook_without_sheets_reports_error() {
        let empty = r#"<workbook xmlns="x"><sheets/></workbook>"#;
        let path = write_xlsx(
            "empty.xlsx",
            &[
                ("xl/workbook.xml", empty),
                ("xl/worksheets/sheet1.xml", SHEET_DETAIL),
            ],
        );
        assert!(parse_sheet(&path).is_err());
    }

    #[test]
    fn row_limit_truncates_instead_of_allocating() {
        // 一行在 Excel 允许但超过我们行上限的位置：不能 panic，也不能把数组撑爆
        let xml = r#"<worksheet><sheetData><row r="1048576"><c r="A1048576"><v>1</v></c></row><row r="1"><c r="A1"><v>2</v></c></row></sheetData></worksheet>"#;
        let styles = StyleSheet::default();
        let sheet = parse_worksheet(xml, &[], &styles);
        assert!(sheet.rows <= MAX_ROWS);
        assert!(sheet.row_heights.len() as u32 <= MAX_ROWS);
        assert!(sheet.cells.iter().all(|c| c.row < MAX_ROWS));
    }

    #[test]
    fn oversized_text_is_truncated_to_limit() {
        let long = "长".repeat(MAX_TEXT_CHARS + 100);
        let xml = format!(
            r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>{long}</t></is></c></row></sheetData></worksheet>"#
        );
        let sheet = parse_worksheet(&xml, &[], &StyleSheet::default());
        let text = sheet.cells.first().map(|c| c.text.len()).unwrap_or(0);
        assert_eq!(text, MAX_TEXT_CHARS * "长".len());
    }

    #[test]
    fn whole_sheet_column_band_does_not_inflate_column_count() {
        // `<col min="2" max="16384">` 是"剩余整表"格式，不能把表撑成 16384 列。
        let xml = r#"<worksheet><cols><col min="1" max="1" width="20"/><col min="2" max="16384" width="9"/></cols><sheetData><row r="1"><c r="A1"><v>1</v></c><c r="C1"><v>2</v></c></row></sheetData></worksheet>"#;
        let sheet = parse_worksheet(xml, &[], &StyleSheet::default());
        assert_eq!(sheet.cols, 3);
        assert_eq!(sheet.col_widths.first().copied(), Some(col_width_px(20.0)));
        assert_eq!(sheet.col_widths.get(1).copied(), Some(col_width_px(9.0)));

        // 一个单元格都没有的纯格式表：退回 band 的最右列（别退化成 0 列）
        let empty =
            r#"<worksheet><cols><col min="1" max="4" width="12"/></cols><sheetData/></worksheet>"#;
        let sheet = parse_worksheet(empty, &[], &StyleSheet::default());
        assert_eq!(sheet.cols, 4);
        assert_eq!(sheet.rows, 0);
    }

    // ---- 条件格式（1.9.3） -------------------------------------------------

    /// 带一个配色条件格式 `dxf` 的最小 styles.xml（`dxfId="0"` 指向红字 + 粉底）。
    const CF_STYLES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><fonts count="1"><font><sz val="11"/></font></fonts><fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills><borders count="1"><border><left/><right/><top/><bottom/></border></borders><cellXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/></cellXfs><dxfs count="2"><dxf><font><color rgb="FF9C0006"/></font><fill><patternFill><bgColor rgb="FFFFC7CE"/></patternFill></fill></dxf><dxf/></dxfs></styleSheet>"#;

    #[test]
    fn sqref_expands_multi_area_and_cross_column_ranges() {
        // 空格分隔的多区域 + 跨列区域，都要展开成 0 基含两端的矩形
        assert_eq!(
            parse_sqref("A1:C10 D5 E7:E9"),
            vec![(0, 0, 9, 2), (4, 3, 4, 3), (6, 4, 8, 4)]
        );
        // 绝对引用 / 大小写 / 反序区域都容忍
        assert_eq!(parse_sqref("$B$2:$AA$3"), vec![(1, 1, 2, 26)]);
        assert_eq!(parse_sqref("c5:a1"), vec![(0, 0, 4, 2)]);
        // 列号是 0 基：D2:D5 → 行 1..=4、列 3（不是 2、也不是 4）——这条钉住
        // "色阶落到隔壁列"那类差一错误
        assert_eq!(parse_sqref("D2:D5"), vec![(1, 3, 4, 3)]);
        assert_eq!(parse_sqref("C2:C5"), vec![(1, 2, 4, 2)]);
        // 坏输入：认不出的片段直接丢（不报错、不 panic）
        assert!(parse_sqref("!!!").is_empty());
        assert!(parse_sqref("").is_empty());
        assert!(parse_sqref("A1::B2").is_empty());
        assert_eq!(parse_sqref("A1 !!! B2"), vec![(0, 0, 0, 0), (1, 1, 1, 1)]);
    }

    #[test]
    fn color_scale_interpolates_two_and_three_stops() {
        // 2 色标（min 红 → max 黄）：0 / 50 / 100 → 两端原色、中间各半；文本格不参与
        let two = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>0</v></c><c r="B1" t="inlineStr"><is><t>50</t></is></c></row><row r="2"><c r="A2"><v>50</v></c></row><row r="3"><c r="A3"><v>100</v></c></row></sheetData><conditionalFormatting sqref="A1:B3"><cfRule type="colorScale" priority="1"><colorScale><cfvo type="min"/><cfvo type="max"/><color rgb="FFFF0000"/><color rgb="FFFFFF00"/></colorScale></cfRule></conditionalFormatting></worksheet>"#;
        let sheet = parse_worksheet(two, &[], &StyleSheet::default());
        let bg = |row: u32, col: u32| sheet.cell(row, col).and_then(|c| c.style.colors.bg);
        assert_eq!(bg(0, 0), Some(0x00_FF_00_00));
        assert_eq!(bg(1, 0), Some(0x00_FF_80_00));
        assert_eq!(bg(2, 0), Some(0x00_FF_FF_00));
        // 字色不被色阶动
        assert_eq!(
            sheet.cell(1, 0).map(|c| c.style.colors.fg),
            Some(0x00_00_00)
        );
        // 文本格（B1 = "50"）不涂色
        assert_eq!(bg(0, 1), None);

        // 3 色标（0 红 / 50 黄 / 100 绿）：中间格正好是中间色，两段内部再插值
        let three = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>0</v></c></row><row r="2"><c r="A2"><v>25</v></c></row><row r="3"><c r="A3"><v>50</v></c></row><row r="4"><c r="A4"><v>75</v></c></row><row r="5"><c r="A5"><v>100</v></c></row></sheetData><conditionalFormatting sqref="A1:A5"><cfRule type="colorScale" priority="1"><colorScale><cfvo type="num" val="0"/><cfvo type="num" val="50"/><cfvo type="num" val="100"/><color rgb="FFFF0000"/><color rgb="FFFFFF00"/><color rgb="FF00FF00"/></colorScale></cfRule></conditionalFormatting></worksheet>"#;
        let sheet = parse_worksheet(three, &[], &StyleSheet::default());
        let bg = |row: u32| sheet.cell(row, 0).and_then(|c| c.style.colors.bg);
        assert_eq!(bg(0), Some(0x00_FF_00_00));
        assert_eq!(bg(1), Some(0x00_FF_80_00)); // 红→黄 中点：G 0→255 走一半
        assert_eq!(bg(2), Some(0x00_FF_FF_00));
        assert_eq!(bg(3), Some(0x00_80_FF_00)); // 黄→绿 中点：R 255→0 走一半
        assert_eq!(bg(4), Some(0x00_00_FF_00));

        // 所有格同值（hi == lo）：整体取中间色，不能除零
        let flat = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>7</v></c></row><row r="2"><c r="A2"><v>7</v></c></row></sheetData><conditionalFormatting sqref="A1:A2"><cfRule type="colorScale" priority="1"><colorScale><cfvo type="min"/><cfvo type="max"/><color rgb="FFFF0000"/><color rgb="FFFFFF00"/></colorScale></cfRule></conditionalFormatting></worksheet>"#;
        let sheet = parse_worksheet(flat, &[], &StyleSheet::default());
        assert_eq!(
            sheet.cell(0, 0).and_then(|c| c.style.colors.bg),
            Some(0x00_FF_80_00)
        );
    }

    #[test]
    fn data_bar_ratio_spans_zero_to_one() {
        let xml = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>-10</v></c></row><row r="2"><c r="A2"><v>10</v></c></row><row r="3"><c r="A3"><v>30</v></c></row><row r="4"><c r="A4" t="inlineStr"><is><t>30</t></is></c></row><row r="5"><c r="A5" t="s"><v>0</v></c></row></sheetData><conditionalFormatting sqref="A1:A5"><cfRule type="dataBar" priority="1"><dataBar><cfvo type="min"/><cfvo type="max"/><color rgb="FF638EC6"/></dataBar></cfRule></conditionalFormatting></worksheet>"#;
        let shared = vec!["文本".to_string()];
        let sheet = parse_worksheet(xml, &shared, &StyleSheet::default());
        let bar = |row: u32| sheet.cell(row, 0).and_then(|c| c.data_bar);
        // min = -10、max = 30 → -10→0.0、10→0.5、30→1.0
        assert_eq!(bar(0), Some(0.0));
        assert!((bar(1).unwrap_or(f32::NAN) - 0.5).abs() < 0.02);
        assert_eq!(bar(2), Some(1.0));
        // 文本格（一个 inlineStr、一个共享字符串）不参与
        assert!(sheet.cell(3, 0).is_some());
        assert_eq!(bar(3), None);
        assert!(sheet.cell(4, 0).is_some());
        assert_eq!(bar(4), None);
        // 数据条不动底色
        assert_eq!(sheet.cell(2, 0).and_then(|c| c.style.colors.bg), None);

        // 回归：单列区域不能把隔壁列吃进来。行优先排序下 `A1:A3` 在 cells 里是
        // 不连续的（每行后面都跟着 B 列），按"连续切片"扫会把 1000/2000/3000 也算进
        // min/max，比例就全错了。
        let neighbor = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>0</v></c><c r="B1"><v>1000</v></c></row><row r="2"><c r="A2"><v>50</v></c><c r="B2"><v>2000</v></c></row><row r="3"><c r="A3"><v>100</v></c><c r="B3"><v>3000</v></c></row></sheetData><conditionalFormatting sqref="A1:A3"><cfRule type="dataBar" priority="1"><dataBar><cfvo type="min"/><cfvo type="max"/><color rgb="FF638EC6"/></dataBar></cfRule></conditionalFormatting></worksheet>"#;
        let sheet = parse_worksheet(neighbor, &[], &StyleSheet::default());
        assert_eq!(sheet.cell(0, 0).and_then(|c| c.data_bar), Some(0.0));
        let mid_bar = sheet
            .cell(1, 0)
            .and_then(|c| c.data_bar)
            .unwrap_or(f32::NAN);
        assert!((mid_bar - 0.5).abs() < 0.02);
        assert_eq!(sheet.cell(2, 0).and_then(|c| c.data_bar), Some(1.0));
        // 隔壁列一格都不许被标
        assert!((0..3).all(|row| sheet.cell(row, 1).and_then(|c| c.data_bar).is_none()));
    }

    #[test]
    fn cell_is_less_than_colors_only_matching_numeric_cells() {
        let styles = parse_styles(CF_STYLES, &Theme::default());
        assert_eq!(styles.dxfs.len(), 2);
        assert_eq!(
            styles.dxfs.first().and_then(|d| d.font_rgb),
            Some(0x00_9C_00_06)
        );
        assert_eq!(
            styles.dxfs.first().and_then(|d| d.fill_rgb),
            Some(0x00_FF_C7_CE)
        );
        // 空 `<dxf/>` 也占位，别把后面的下标挤错
        assert_eq!(styles.dxfs.get(1).and_then(|d| d.font_rgb), None);

        let xml = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>5</v></c><c r="B1"><v>50</v></c><c r="C1" t="inlineStr"><is><t>5</t></is></c></row><row r="2"><c r="A2"><v>500</v></c><c r="B2" t="s"><v>0</v></c></row></sheetData><conditionalFormatting sqref="A1:C2"><cfRule type="cellIs" dxfId="0" priority="1" operator="lessThan"><formula>10</formula></cfRule></conditionalFormatting></worksheet>"#;
        let shared = vec!["5".to_string()];
        let sheet = parse_worksheet(xml, &shared, &styles);
        // A1 = 5 < 10 → 命中：红字 + 粉底
        let hit = sheet.cell(0, 0).expect("A1");
        assert_eq!(hit.style.colors.fg, 0x00_9C_00_06);
        assert_eq!(hit.style.colors.bg, Some(0x00_FF_C7_CE));
        // B1 = 50 不命中：样式原封不动
        let miss = sheet.cell(0, 1).expect("B1");
        assert_eq!(miss.style.colors.fg, 0x00_00_00);
        assert_eq!(miss.style.colors.bg, None);
        // C1 是文本 "5"：看着像数字也不参与比较
        let text = sheet.cell(0, 2).expect("C1");
        assert_eq!(text.style.colors.fg, 0x00_00_00);
        assert_eq!(text.style.colors.bg, None);
        // A2 = 500 不命中；B2 是共享字符串文本，同样不命中
        assert_eq!(
            sheet.cell(1, 0).map(|c| c.style.colors.fg),
            Some(0x00_00_00)
        );
        assert_eq!(
            sheet.cell(1, 1).map(|c| c.style.colors.fg),
            Some(0x00_00_00)
        );
        // 文本/数值口径不许被条件格式改动
        assert_eq!(sheet.cell(0, 2).map(|c| c.numeric), Some(false));
        assert_eq!(sheet.cell(0, 0).map(|c| c.numeric), Some(true));

        // 其余 operator：between（只有 15 落在 10..20 里）
        let range_xml = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>5</v></c></row><row r="2"><c r="A2"><v>15</v></c></row><row r="3"><c r="A3"><v>25</v></c></row></sheetData><conditionalFormatting sqref="A1:A3"><cfRule type="cellIs" dxfId="0" priority="1" operator="between"><formula>10</formula><formula>20</formula></cfRule></conditionalFormatting></worksheet>"#;
        let sheet = parse_worksheet(range_xml, &[], &styles);
        assert_eq!(
            sheet.cell(0, 0).map(|c| c.style.colors.fg),
            Some(0x00_00_00)
        );
        assert_eq!(
            sheet.cell(1, 0).map(|c| c.style.colors.fg),
            Some(0x00_9C_00_06)
        );
        assert_eq!(
            sheet.cell(2, 0).map(|c| c.style.colors.fg),
            Some(0x00_00_00)
        );
    }

    #[test]
    fn malformed_conditional_formatting_is_skipped_without_panic() {
        let styles = parse_styles(CF_STYLES, &Theme::default());
        let cases = [
            // sqref 是垃圾
            r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData><conditionalFormatting sqref="!!!"><cfRule type="cellIs" dxfId="0" operator="lessThan"><formula>10</formula></cfRule></conditionalFormatting></worksheet>"#,
            // 缺子元素 / 缺 type / dxfId 越界 / 未知 type / 空规则
            r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData><conditionalFormatting sqref="A1"><cfRule type="colorScale"/><cfRule type="dataBar"/><cfRule type="cellIs" dxfId="99" operator="lessThan"><formula>10</formula></cfRule><cfRule type="expression"><formula>A1&gt;1</formula></cfRule><cfRule type="iconSet" dxfId="0"/><cfRule/></conditionalFormatting></worksheet>"#,
            // 色标只有 1 个 / 颜色只有 theme（拿不到 → 整条跳过）
            r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData><conditionalFormatting sqref="A1"><cfRule type="colorScale"><colorScale><cfvo type="min"/><color rgb="FFFF0000"/></colorScale></cfRule><cfRule type="colorScale" priority="2"><colorScale><cfvo type="min"/><cfvo type="max"/><color theme="4"/><color theme="5"/></colorScale></cfRule></conditionalFormatting></worksheet>"#,
            // 认不出的 cfvo type / 没有 cfvo / 没有 formula / formula 不是字面量
            r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData><conditionalFormatting sqref="A1"><cfRule type="colorScale"><colorScale><cfvo type="formula" val="A1"/><cfvo type="max"/><color rgb="FFFF0000"/><color rgb="FF00FF00"/></colorScale></cfRule><cfRule type="dataBar"><dataBar><color rgb="FF638EC6"/></dataBar></cfRule><cfRule type="cellIs" dxfId="0" operator="lessThan"/><cfRule type="cellIs" dxfId="0" operator="lessThan"><formula>$D$2</formula></cfRule></conditionalFormatting></worksheet>"#,
            // 块没闭合 / 标签截断
            r#"<worksheet><conditionalFormatting sqref="A1:A2"><cfRule type="colorScale"><colorScale><cfvo type="min"/>"#,
            r#"<worksheet><conditionalFormatting"#,
            // 区域越界（XFD1048577 超过我们自己的行上限）
            r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData><conditionalFormatting sqref="A1:XFD1048577"><cfRule type="dataBar" priority="1"><dataBar><cfvo type="min"/><cfvo type="max"/><color rgb="FF638EC6"/></dataBar></cfRule></conditionalFormatting></worksheet>"#,
        ];
        for (index, xml) in cases.iter().enumerate() {
            let sheet = parse_worksheet(xml, &[], &styles);
            // 坏输入 = "没有条件格式"：一个格子都不许被涂色
            assert!(
                sheet.cells.iter().all(|c| c.style.colors.bg.is_none()),
                "case {index} 不该涂底色"
            );
            assert!(
                sheet.cells.iter().all(|c| c.style.colors.fg == 0x00_00_00),
                "case {index} 不该改字色"
            );
        }
        // 越界那条规则仍然合法（区域被夹进上限），数据条算得出来
        let last = parse_worksheet(cases[6], &[], &styles);
        assert_eq!(last.cell(0, 0).and_then(|c| c.data_bar), Some(1.0));

        // dxfs 的数量上限复用 MAX_XFS：超了截断，不能把整份文件撑进内存
        let mut huge = String::from("<styleSheet><dxfs count=\"9999\">");
        for _ in 0..(MAX_XFS + 16) {
            huge.push_str("<dxf><font><color rgb=\"FF9C0006\"/></font></dxf>");
        }
        huge.push_str("</dxfs></styleSheet>");
        assert_eq!(parse_dxfs(&huge, &Theme::default()).len(), MAX_XFS);
    }

    /// 条件格式夹具（默认不跑，夹具在仓外，见 `_bench/condfmt/`）：
    ///
    /// 夹具版式（`_bench/condfmt/make_condfmt.py` 产出，5,824 字节）：
    /// - `C2:C5` dataBar（min/max）——C2:C5 = 120 / 98 / 143 / 361
    /// - `C2:C5` cellIs `lessThan 100` + `dxfId="0"`（红字 `0x9C0006` / 粉底 `0xFFC7CE`）
    /// - `D2:D5` colorScale min/percentile50/max，颜色按声明顺序 `F8696B → FFEB84 → 63BE7B`
    ///   （即 min=红、max=绿，和 Excel 存的顺序一致：**第 i 个 `<color>` 对应第 i 个 `<cfvo>`**）
    ///
    /// ```powershell
    /// $env:QLR_COND_SAMPLE = "<夹具>\condfmt-demo.xlsx"
    /// $env:PATH = "D:\Ruanjian\Rust\cargo\bin;$env:PATH"
    /// cargo test -p qlr-render --offline sheet::tests::conditional_formatting_fixture -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn conditional_formatting_fixture() {
        let Ok(sample) = std::env::var("QLR_COND_SAMPLE") else {
            println!("跳过：未设置 QLR_COND_SAMPLE");
            return;
        };
        let doc = parse_sheet(&std::path::PathBuf::from(&sample)).expect("解析夹具失败");
        let sheet = doc.sheet().expect("夹具里没有工作表");
        // 先把 C/D 两列原样打出来，肉眼核对"谁被涂了"
        for cell in sheet.cells.iter().filter(|c| c.col == 2 || c.col == 3) {
            println!(
                "  ({},{}) text={:?} numeric={} bar={:?} fg={:#08X} bg={:?}",
                cell.row,
                cell.col,
                cell.text,
                cell.numeric,
                cell.data_bar,
                cell.style.colors.fg,
                cell.style.colors.bg.map(|v| format!("{v:#08X}"))
            );
        }

        let bar = |row: u32| sheet.cell(row, 2).and_then(|c| c.data_bar);
        // 数据条只落在 C 列：98(min)→0.0、120→0.0837、143→0.1711、361(max)→1.0
        assert!(
            (bar(1).unwrap_or(f32::NAN) - 0.0837).abs() < 0.02,
            "C2 数据条"
        );
        assert!((bar(2).unwrap_or(f32::NAN) - 0.0).abs() < 0.02, "C3 数据条");
        assert!(
            (bar(3).unwrap_or(f32::NAN) - 0.1711).abs() < 0.02,
            "C4 数据条"
        );
        assert!((bar(4).unwrap_or(f32::NAN) - 1.0).abs() < 0.02, "C5 数据条");

        // cellIs：只有 C3(98 < 100) 命中，红字 + 粉底；其它 C 格不许有底色
        let hit = sheet.cell(2, 2).expect("C3");
        assert_eq!(hit.style.colors.fg, 0x00_9C_00_06, "C3 字色");
        assert_eq!(hit.style.colors.bg, Some(0x00_FF_C7_CE), "C3 底色");
        for row in [1u32, 3, 4] {
            let cell = sheet.cell(row, 2).expect("C 列");
            assert_eq!(cell.style.colors.fg, 0x00_00_00, "C{row}^ 不该被 cellIs 标");
            assert_eq!(cell.style.colors.bg, None, "C{row}^ 不该有底色");
        }

        // 色阶只在 D 列，且顺序为"小 → F8696B(第一个色标)、大 → 63BE7B(最后一个色标)"。
        // 注意 min 在 **D3**(2760)，不是 D2：D2:D5 = 3480.5 / 2760 / 4102.25 / 10342.75。
        let bg = |row: u32| sheet.cell(row, 3).and_then(|c| c.style.colors.bg);
        let d_bgs: Vec<Option<u32>> = (1..=4).map(bg).collect();
        assert!(d_bgs.iter().all(Option::is_some), "D2:D5 都该有色阶底色");
        assert_eq!(bg(2), Some(0x00_F8_69_6B), "D3(2760, min) 应取第一个色标");
        assert_eq!(
            bg(4),
            Some(0x00_63_BE_7B),
            "D5(10342.75, max) 应取最后一个色标"
        );
        assert_ne!(bg(1), bg(2), "D2 与最小格应落在渐变的不同位置");
        assert_ne!(bg(3), bg(4), "D4 与最大格应落在渐变的不同位置");
        // D 列不参与数据条/cellIs（那两条规则的 sqref 是 C2:C5）
        for row in 1..=4 {
            let cell = sheet.cell(row, 3).expect("D 列");
            assert_eq!(cell.data_bar, None, "D{row}^ 不该有数据条");
            assert_eq!(cell.style.colors.fg, 0x00_00_00, "D{row}^ 不该被 cellIs 标");
        }
        println!("condfmt fixture ok: cells={}", sheet.cells.len());
    }

    ///
    /// ```powershell
    /// $env:QLR_SHEET_SAMPLE = "<夹具>\real-monthly.xlsx"
    /// $env:PATH = "D:\Ruanjian\Rust\cargo\bin;$env:PATH"
    /// cargo test -p qlr-render --offline sheet::tests::perf -- --ignored --nocapture
    /// ```
    ///
    /// 这里用 `println!` 是**测试专用**：GUI 子系统没有 stdout，生产路径一律走 `log_*!`。
    #[test]
    #[ignore]
    fn perf_parse_real_sample() {
        let Ok(sample) = std::env::var("QLR_SHEET_SAMPLE") else {
            println!("跳过：未设置 QLR_SHEET_SAMPLE");
            return;
        };
        let path = std::path::PathBuf::from(&sample);
        let started = std::time::Instant::now();
        let doc = parse_sheet(&path).expect("解析样本失败");
        let parse_ms = started.elapsed().as_secs_f64() * 1000.0;
        let sheet = doc.sheet().expect("样本里没有工作表");
        let styled_empty = sheet.cells.iter().filter(|c| c.text.is_empty()).count();
        println!(
            "parse_ms={parse_ms:.1} sheets={} active={} name={} cells={} (empty-styled={styled_empty}) rows={} cols={}",
            doc.len(),
            doc.active,
            sheet.name,
            sheet.cells.len(),
            sheet.rows,
            sheet.cols
        );
        // 抽几格文本出来肉眼对一遍"像不像 Excel"（尤其日期/百分比/千分位）
        let peek: Vec<String> = sheet
            .cells
            .iter()
            .take(8)
            .map(|c| format!("({},{})=\"{}\"", c.row, c.col, c.text))
            .collect();
        println!("  first-cells: {}", peek.join(" "));
        if let Some(last) = sheet.cells.last() {
            println!("  last-cell: ({},{})=\"{}\"", last.row, last.col, last.text);
        }

        let mut per_sheet = Vec::new();
        for index in 1..doc.len() {
            let t0 = std::time::Instant::now();
            if let Ok(other) = parse_sheet_at(&path, index) {
                if let Some(s) = other.sheet() {
                    per_sheet.push(format!(
                        "#{index} {} {:.1}ms {}x{} cells={}",
                        s.name,
                        t0.elapsed().as_secs_f64() * 1000.0,
                        s.rows,
                        s.cols,
                        s.cells.len()
                    ));
                }
            }
        }
        for line in per_sheet {
            println!("  sheet {line}");
        }
    }
}
