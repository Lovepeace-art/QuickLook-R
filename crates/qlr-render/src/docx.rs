//! docx（WordprocessingML）解析 + 排版：zip 容器 → `qlr_core::docx::DocDoc`。
//!
//! **契约**（与 `sheet.rs` 同一套）：本模块只做"文件 → 纯数据模型"，不碰 `windows`、
//! 不画图。几何/像素由 `qlr_platform::window::docx` 负责。折行需要字体度量，这里用**注入**
//! 的 `measure`（纯函数，CJK≈全角、ASCII≈半角），绘制端按同一 measure 的估算宽度顺序画、
//! 并对每行做裁剪，任何低估都不会溢出页边（CONVENTIONS #38 的意图）。
//!
//! # 支持范围（P1）
//! - 段落：`w:p`，对齐/段前后间距/左右与首行缩进/行距（倍数或固定值）、分页符
//!   （`w:br w:type="page"` 与 `w:pageBreakBefore`）；
//! - 文字 run：`w:r` 的粗体/斜体/下划线/删除线/字号/颜色/高亮/等宽字体；`w:br`（换行）、
//!   `w:tab`；
//! - 样式：`styles.xml` 的 `docDefaults` + 命名样式 `w:style`（含 `basedOn` 链继承）；
//! - 编号：`numbering.xml` 的 bullet / decimal / 字母 / 罗马数字（近似）；
//! - 表格：`w:tbl` 网格列宽、单元格段落、`gridSpan` 横合并、`vMerge` 纵合并（continuation
//!   留空）、底色 `w:shd`、边框是否存在；
//! - 图片：`wp:inline` / `wp:anchor` 的 `wp:extent` + `a:blip r:embed`；
//! - 页眉/页脚：`sectPr` 的 `headerReference`/`footerReference` 指向的 part；
//! - 页面：`w:pgSz` / `w:pgMar`（twips → px）。
//!
//! # 明确不支持（退化成"能看但不像 Word"，不报错）
//! 脚注/尾注、批注、域代码计算结果（页码域留空）、浮动图片的环绕、制表位精确对齐、
//! 旋转/竖排文字、艺术字、SmartArt、图表、页眉首页/奇偶不同、多节不同页边距。
//!
//! # 上限（防 OOM）
//! 见 `MAX_*`。所有上限都是"截断 + log_warn"，只有 zip 结构异常才 `Err`；绝不 panic
//! （release 是 `panic = "abort"`，进程内解析崩一次会把整个预览进程带走）。

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use qlr_core::docx::{
    Block, DocDoc, DocImage, HAlign, ImageBlock, Line, Page, PageSetup, Para, Rect, Run, Table,
    TableCell, TableRow, dpt_to_px, emu_to_px, twips_to_px,
};
use qlr_core::{Error, Result};

const SCOPE: &str = "docx";

/// zip 单条目解压上限（document.xml 正常几 MB，留足余量）。
const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
/// 一次解析累计解压上限（zip 炸弹防护）。
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
/// 最多保留多少页（超出截断）。
const MAX_PAGES: usize = 5_000;
/// 单个段落最多保留多少 run。
const MAX_RUNS_PER_PARA: usize = 20_000;
/// 最多解码多少张图片（远超正常文档）。
const MAX_IMAGES: usize = 512;
/// 单张图片解码后的像素上限（防超大图撑爆内存）。
const MAX_IMAGE_PIXELS: u64 = 40_000_000;
/// 单元格内边距（像素）。
const CELL_PAD: i32 = 4;

// ---------------------------------------------------------------------------
// 公开入口
// ---------------------------------------------------------------------------

/// 解析整篇 docx，返回完全排好版的分页模型（一次解析拿到全部页，翻页只需换下标）。
pub fn parse_docx(path: &Path) -> Result<DocDoc> {
    let name = crate::display_name(path);
    let file =
        File::open(path).map_err(|e| Error::msg(format!("打开 docx 失败（{name}）：{e}")))?;
    let zip = zip::ZipArchive::new(file)
        .map_err(|e| Error::msg(format!("不是有效的 docx（{name} 的 zip 容器打不开）：{e}")))?;
    let mut pkg = Package { zip, used: 0 };

    let document = pkg
        .read_optional("word/document.xml")?
        .ok_or_else(|| Error::msg(format!("不是有效的 docx：缺少 word/document.xml（{name}）")))?;

    let rels = match pkg.read_optional("word/_rels/document.xml.rels")? {
        Some(xml) => parse_rels(&xml),
        None => HashMap::new(),
    };
    let styles_xml = pkg.read_optional("word/styles.xml")?;
    let numbering_xml = pkg.read_optional("word/numbering.xml")?;
    let title = pkg
        .read_optional("docProps/core.xml")?
        .and_then(|xml| extract_title(&xml));

    let (default_run, default_ppr, defs) = match &styles_xml {
        Some(xml) => parse_styles(xml),
        None => (RunStyle::default(), PProps::default(), HashMap::new()),
    };
    let resolved = resolve_all(&defs, &default_run, &default_ppr);
    let numbering = match &numbering_xml {
        Some(xml) => parse_numbering(xml),
        None => HashMap::new(),
    };

    let mut ctx = Ctx {
        default_run,
        default_ppr,
        resolved,
        numbering,
        rels: rels.clone(),
        setup: PageSetup::a4(),
        header_rid: None,
        footer_rid: None,
    };

    let (flows, sect) = parse_flows(&document, &ctx);
    if let Some(sect) = sect {
        ctx.setup = sect.setup;
        ctx.header_rid = sect.header_rid;
        ctx.footer_rid = sect.footer_rid;
    }

    // ---- 图片：把 body 里引用到的 rId 逐张解码成显示尺寸位图 ----
    let mut images: Vec<DocImage> = Vec::new();
    let mut image_map: HashMap<String, usize> = HashMap::new();
    for flow in &flows {
        collect_image_rids(flow, &mut image_map);
    }
    // 保证插入顺序稳定：按出现顺序处理（image_map 现在只是占位）。
    image_map.clear();
    let mut pending: Vec<(String, i32, i32)> = Vec::new();
    for flow in &flows {
        collect_pending(flow, &mut pending);
    }
    for (rid, w, h) in pending {
        if image_map.contains_key(&rid) {
            continue;
        }
        if images.len() >= MAX_IMAGES {
            break;
        }
        let Some(part) = ctx.rels.get(&rid).cloned() else {
            continue;
        };
        let Ok(bytes) = pkg.read_part_bytes(&part) else {
            continue;
        };
        if let Some(img) = decode_image(&bytes, w, h) {
            let index = images.len();
            images.push(img);
            image_map.insert(rid, index);
        }
    }

    // ---- 页眉/页脚 ----
    let header_flows = load_overlay(&mut pkg, ctx.header_rid.as_deref(), &ctx.rels, &ctx);
    let footer_flows = load_overlay(&mut pkg, ctx.footer_rid.as_deref(), &ctx.rels, &ctx);

    // ---- 排版 ----
    let mut doc = layout_document(
        &flows,
        ctx.setup,
        &image_map,
        images,
        title,
        &ctx,
        &approx_measure,
    );
    attach_overlay(&mut doc, &header_flows, &footer_flows, &approx_measure);
    Ok(doc)
}

/// 只读页面骨架（`w:pgSz`/`w:pgMar`），不解正文。给开窗定尺寸用（便宜）。
pub fn document_layout(path: &Path) -> Option<PageSetup> {
    let file = File::open(path).ok()?;
    let zip = zip::ZipArchive::new(file).ok()?;
    let mut pkg = Package { zip, used: 0 };
    let document = pkg.read_optional("word/document.xml").ok()??;
    let mut setup = PageSetup::a4();
    if let Some(attrs) = find_tag_attrs(&document, "pgSz") {
        if let Some(w) = attrs_get(attrs, "w:w").and_then(|v| v.parse::<i64>().ok()) {
            let w = twips_to_px(w).max(1) as u32;
            if w > 100 {
                setup.page_w = w;
            }
        }
        if let Some(h) = attrs_get(attrs, "w:h").and_then(|v| v.parse::<i64>().ok()) {
            let h = twips_to_px(h).max(1) as u32;
            if h > 100 {
                setup.page_h = h;
            }
        }
    }
    if let Some(attrs) = find_tag_attrs(&document, "pgMar") {
        if let Some(v) = attrs_get(attrs, "w:left").and_then(|v| v.parse::<i64>().ok()) {
            setup.margin_left = twips_to_px(v);
        }
        if let Some(v) = attrs_get(attrs, "w:right").and_then(|v| v.parse::<i64>().ok()) {
            setup.margin_right = twips_to_px(v);
        }
        if let Some(v) = attrs_get(attrs, "w:top").and_then(|v| v.parse::<i64>().ok()) {
            setup.margin_top = twips_to_px(v);
        }
        if let Some(v) = attrs_get(attrs, "w:bottom").and_then(|v| v.parse::<i64>().ok()) {
            setup.margin_bottom = twips_to_px(v);
        }
    }
    Some(setup)
}

// ---------------------------------------------------------------------------
// 字体度量（纯近似）
// ---------------------------------------------------------------------------

/// 注入给排版器的度量函数签名：`(文本, 字号 dpt, 粗体) -> 像素宽度`。
pub type Measure<'a> = dyn Fn(&str, u16, bool) -> i32 + 'a;

/// 纯近似度量：CJK 每字 = 1 em（全角），ASCII 字母/数字 ≈ 0.5 em，空格 ≈ 0.28 em。
///
/// 宁可**略微高估**（提前折行）也不要低估（画到页边外）：绘制端会按行矩形裁剪，
/// 提前折行最多多一点留白，绝不会出现文字压边/串行。
pub fn approx_measure(text: &str, size_dpt: u16, bold: bool) -> i32 {
    let em = dpt_to_px(size_dpt) as f32;
    let mut w = 0.0f32;
    for ch in text.chars() {
        w += char_advance(ch, em);
    }
    if bold {
        w *= 1.03;
    }
    (w.ceil() as i32).max(0)
}

fn char_advance(ch: char, em: f32) -> f32 {
    match ch {
        '\t' => return em * 3.0,
        ' ' => return em * 0.28,
        _ => {}
    }
    let c = ch as u32;
    if c < 0x20 {
        return 0.0;
    }
    if is_wide(c) {
        return em;
    }
    match ch {
        'i' | 'l' | 'j' | 't' | 'f' | 'r' | '.' | ',' | ':' | ';' | '\'' | '!' | '|' | 'I' => {
            em * 0.28
        }
        'm' | 'w' | 'M' | 'W' => em * 0.82,
        _ => em * 0.5,
    }
}

fn is_wide(c: u32) -> bool {
    matches!(
        c,
        0x1100..=0x115F
            | 0x2E80..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x20000..=0x3FFFD
    )
}

// ---------------------------------------------------------------------------
// 中间模型（解析产物，未排版）
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct RunStyle {
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    size_dpt: u16,
    color: u32,
    highlight: Option<u32>,
    mono: bool,
}

impl RunStyle {
    fn base() -> Self {
        Self {
            bold: false,
            italic: false,
            underline: false,
            strike: false,
            size_dpt: 105,
            color: 0x000000,
            highlight: None,
            mono: false,
        }
    }
    #[allow(dead_code)]
    fn size_px(&self) -> i32 {
        dpt_to_px(self.size_dpt)
    }
}

#[derive(Clone, Default)]
struct RunPatch {
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    strike: Option<bool>,
    size_dpt: Option<u16>,
    color: Option<u32>,
    highlight: Option<u32>,
    mono: Option<bool>,
}

impl RunPatch {
    fn apply(&self, s: &mut RunStyle) {
        if let Some(v) = self.bold {
            s.bold = v;
        }
        if let Some(v) = self.italic {
            s.italic = v;
        }
        if let Some(v) = self.underline {
            s.underline = v;
        }
        if let Some(v) = self.strike {
            s.strike = v;
        }
        if let Some(v) = self.size_dpt {
            s.size_dpt = v;
        }
        if let Some(v) = self.color {
            s.color = v;
        }
        if let Some(v) = self.highlight {
            s.highlight = Some(v);
        }
        if let Some(v) = self.mono {
            s.mono = v;
        }
    }
}

#[derive(Clone)]
struct PProps {
    align: HAlign,
    space_before: i32,
    space_after: i32,
    indent_left: i32,
    indent_right: i32,
    indent_first: i32,
    line_mult: f32,
    line_fixed: Option<i32>,
    page_break_before: bool,
}

impl Default for PProps {
    fn default() -> Self {
        Self {
            align: HAlign::Left,
            space_before: 0,
            space_after: 0,
            indent_left: 0,
            indent_right: 0,
            indent_first: 0,
            line_mult: 1.15,
            line_fixed: None,
            page_break_before: false,
        }
    }
}

#[derive(Clone, Default)]
struct PPrPatch {
    align: Option<HAlign>,
    space_before: Option<i32>,
    space_after: Option<i32>,
    indent_left: Option<i32>,
    indent_right: Option<i32>,
    indent_first: Option<i32>,
    line_mult: Option<f32>,
    line_fixed: Option<i32>,
    page_break_before: Option<bool>,
}

impl PPrPatch {
    fn apply(&self, p: &mut PProps) {
        if let Some(v) = self.align {
            p.align = v;
        }
        if let Some(v) = self.space_before {
            p.space_before = v;
        }
        if let Some(v) = self.space_after {
            p.space_after = v;
        }
        if let Some(v) = self.indent_left {
            p.indent_left = v;
        }
        if let Some(v) = self.indent_right {
            p.indent_right = v;
        }
        if let Some(v) = self.indent_first {
            p.indent_first = v;
        }
        if let Some(v) = self.line_mult {
            p.line_mult = v;
        }
        if let Some(v) = self.line_fixed {
            p.line_fixed = Some(v);
        }
        if let Some(v) = self.page_break_before {
            p.page_break_before = v;
        }
    }
}

#[derive(Clone, Default)]
struct PPrRaw {
    style_id: Option<String>,
    patch: PPrPatch,
    num: Option<(u32, u32)>,
}

#[derive(Clone, Default)]
struct StyleDef {
    based_on: Option<String>,
    run_patch: RunPatch,
    ppr_patch: PPrPatch,
}

#[derive(Clone)]
struct NumberingLevel {
    fmt: String,
    start: i32,
    text: String,
}

type Numbering = HashMap<u32, Vec<NumberingLevel>>;

#[derive(Clone, Default)]
struct ParaFlow {
    runs: Vec<Run>,
    align: HAlign,
    space_before: i32,
    space_after: i32,
    indent_left: i32,
    indent_right: i32,
    line_mult: f32,
    line_fixed: Option<i32>,
    page_break_before: bool,
    num: Option<(u32, u32)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VMerge {
    None,
    Restart,
    Continue,
}

#[derive(Clone)]
struct PendingImage {
    rid: String,
    w: i32,
    h: i32,
    alt: String,
}

#[derive(Clone)]
struct CellFlow {
    colspan: u32,
    vmerge: VMerge,
    fill: Option<u32>,
    blocks: Vec<Flow>,
}

#[derive(Clone)]
struct RowFlow {
    cells: Vec<CellFlow>,
    min_h: i32,
}

#[derive(Clone, Default)]
struct TableFlow {
    col_widths: Vec<i32>,
    rows: Vec<RowFlow>,
    borders: bool,
}

#[derive(Clone)]
enum Flow {
    Para(ParaFlow),
    Table(TableFlow),
    Image(PendingImage),
}

struct SectInfo {
    setup: PageSetup,
    header_rid: Option<String>,
    footer_rid: Option<String>,
}

struct Ctx {
    default_run: RunStyle,
    default_ppr: PProps,
    resolved: HashMap<String, (RunStyle, PProps)>,
    numbering: Numbering,
    rels: HashMap<String, String>,
    setup: PageSetup,
    header_rid: Option<String>,
    footer_rid: Option<String>,
}

impl Ctx {
    #[allow(dead_code)]
    fn empty() -> Self {
        Self {
            default_run: RunStyle::base(),
            default_ppr: PProps::default(),
            resolved: HashMap::new(),
            numbering: HashMap::new(),
            rels: HashMap::new(),
            setup: PageSetup::a4(),
            header_rid: None,
            footer_rid: None,
        }
    }

    fn style_base(&self, id: Option<&str>) -> (RunStyle, PProps) {
        match id.and_then(|id| self.resolved.get(id)) {
            Some((r, p)) => (r.clone(), p.clone()),
            None => (self.default_run.clone(), self.default_ppr.clone()),
        }
    }
}

// ---------------------------------------------------------------------------
// 极简 XML 拉取解析
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Ev<'a> {
    Start {
        name: &'a str,
        attrs: &'a str,
        empty: bool,
    },
    End(&'a str),
    Text(&'a str),
    Eof,
}

fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

struct XmlScan<'a> {
    s: &'a str,
    pos: usize,
}

impl<'a> XmlScan<'a> {
    fn new(s: &'a str) -> Self {
        Self { s, pos: 0 }
    }

    fn next(&mut self) -> Ev<'a> {
        let s = self.s;
        let b = s.as_bytes();
        let len = b.len();
        loop {
            if self.pos >= len {
                return Ev::Eof;
            }
            if b[self.pos] != b'<' {
                let start = self.pos;
                while self.pos < len && b[self.pos] != b'<' {
                    self.pos += 1;
                }
                return Ev::Text(&s[start..self.pos]);
            }
            // 注释
            if s[self.pos..].starts_with("<!--") {
                if let Some(p) = s[self.pos..].find("-->") {
                    self.pos += p + 3;
                    continue;
                }
                self.pos = len;
                return Ev::Eof;
            }
            // CDATA
            if s[self.pos..].starts_with("<![CDATA[") {
                let inner_start = self.pos + 9;
                if let Some(p) = s[inner_start..].find("]]>") {
                    let text = &s[inner_start..inner_start + p];
                    self.pos = inner_start + p + 3;
                    return Ev::Text(text);
                }
                self.pos = len;
                return Ev::Eof;
            }
            // 处理指令 / DOCTYPE
            if s[self.pos..].starts_with("<?") {
                if let Some(p) = s[self.pos..].find("?>") {
                    self.pos += p + 2;
                    continue;
                }
                self.pos = len;
                return Ev::Eof;
            }
            if s[self.pos..].starts_with("<!") {
                if let Some(p) = s[self.pos..].find('>') {
                    self.pos += p + 1;
                    continue;
                }
                self.pos = len;
                return Ev::Eof;
            }
            // 普通元素：找 '>'，跳过引号内的 '>'
            let mut i = self.pos + 1;
            let mut q = 0u8;
            while i < len {
                let c = b[i];
                if q != 0 {
                    if c == q {
                        q = 0;
                    }
                } else if c == b'"' || c == b'\'' {
                    q = c;
                } else if c == b'>' {
                    break;
                }
                i += 1;
            }
            if i >= len {
                self.pos = len;
                return Ev::Eof;
            }
            let inner = &s[self.pos + 1..i];
            self.pos = i + 1;
            let empty = inner.ends_with('/');
            let inner2 = if empty {
                &inner[..inner.len() - 1]
            } else {
                inner
            };
            if let Some(rest) = inner2.strip_prefix('/') {
                return Ev::End(rest.trim());
            }
            let name_end = inner2
                .find(|c: char| c.is_whitespace())
                .unwrap_or(inner2.len());
            let name = &inner2[..name_end];
            let attrs = &inner2[name_end..];
            return Ev::Start { name, attrs, empty };
        }
    }
}

struct Reader<'a> {
    scan: XmlScan<'a>,
    peeked: Option<Ev<'a>>,
}

impl<'a> Reader<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            scan: XmlScan::new(s),
            peeked: None,
        }
    }
    fn next(&mut self) -> Ev<'a> {
        if let Some(e) = self.peeked.take() {
            e
        } else {
            self.scan.next()
        }
    }
    #[allow(dead_code)]
    fn peek(&mut self) -> Ev<'a> {
        if self.peeked.is_none() {
            self.peeked = Some(self.scan.next());
        }
        self.peeked.unwrap()
    }
}

/// 跳过"刚刚消费了 Start 的那个非空元素"的剩余子树（嵌套同名用深度计数）。
fn skip_element(r: &mut Reader) {
    let mut depth = 0usize;
    loop {
        match r.next() {
            Ev::Start { empty: false, .. } => depth += 1,
            Ev::Start { empty: true, .. } => {}
            Ev::End(_) => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            Ev::Eof => break,
            _ => {}
        }
    }
}

fn attrs_get<'a>(attrs: &'a str, key: &str) -> Option<&'a str> {
    let target = local(key);
    let b = attrs.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let ns = i;
        while i < b.len() && b[i] != b'=' && !(b[i] as char).is_whitespace() {
            i += 1;
        }
        let name = &attrs[ns..i];
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            continue;
        }
        i += 1;
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let q = b[i];
        if q == b'"' || q == b'\'' {
            i += 1;
            let vs = i;
            while i < b.len() && b[i] != q {
                i += 1;
            }
            let val = &attrs[vs..i];
            if i < b.len() {
                i += 1;
            }
            if local(name) == target {
                return Some(val);
            }
        } else {
            let vs = i;
            while i < b.len() && !(b[i] as char).is_whitespace() {
                i += 1;
            }
            let val = &attrs[vs..i];
            if local(name) == target {
                return Some(val);
            }
        }
    }
    None
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        let Some(semi) = rest.find(';').filter(|&semi| semi <= 12) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            other => {
                if let Some(hex) = other
                    .strip_prefix("#x")
                    .or_else(|| other.strip_prefix("#X"))
                {
                    u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                } else {
                    other
                        .strip_prefix('#')
                        .and_then(|d| d.parse::<u32>().ok())
                        .and_then(char::from_u32)
                }
            }
        };
        match decoded {
            Some(ch) => {
                out.push(ch);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 读 `w:t` / 任意元素的直接文本直到对应 End。返回解码后的字符串。
fn read_text_node(r: &mut Reader) -> String {
    let mut out = String::new();
    loop {
        match r.next() {
            Ev::Text(t) => out.push_str(&decode_entities(t)),
            Ev::Start { empty: false, .. } => skip_element(r),
            Ev::Start { empty: true, .. } => {}
            Ev::End(_) => break,
            Ev::Eof => break,
        }
    }
    out
}

/// 找某个标签（如 `w:pgSz`）的属性片段（第一个匹配 `<tag ...>`）。
fn find_tag_attrs<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let needle = format!("<{tag}");
    let mut from = 0;
    while let Some(pos) = xml[from..].find(&needle) {
        let abs = from + pos;
        let after = abs + needle.len();
        let b = xml.as_bytes();
        let boundary =
            after >= b.len() || matches!(b[after], b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>');
        if boundary {
            let mut i = after;
            let mut q = 0u8;
            while i < b.len() {
                let c = b[i];
                if q != 0 {
                    if c == q {
                        q = 0;
                    }
                } else if c == b'"' || c == b'\'' {
                    q = c;
                } else if c == b'>' {
                    break;
                }
                i += 1;
            }
            return Some(&xml[after..i]);
        }
        from = after;
    }
    None
}

fn extract_title(xml: &str) -> Option<String> {
    let start = xml.find("<dc:title")?;
    let after = xml[start..].find('>')? + start + 1;
    let end = xml[after..].find("</dc:title")? + after;
    let title = decode_entities(xml[after..end].trim());
    if title.is_empty() { None } else { Some(title) }
}

// ---------------------------------------------------------------------------
// zip 容器
// ---------------------------------------------------------------------------

struct Package {
    zip: zip::ZipArchive<File>,
    used: u64,
}

impl Package {
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

    fn read_optional(&mut self, entry: &str) -> Result<Option<String>> {
        let Some(name) = self.find_name(entry) else {
            return Ok(None);
        };
        self.read_part_bytes(&name).map(|b| Some(decode_xml(&b)))
    }

    fn read_part_bytes(&mut self, name: &str) -> Result<Vec<u8>> {
        let entry = self
            .zip
            .by_name(name)
            .map_err(|e| Error::msg(format!("读取 {name} 失败：{e}")))?;
        let declared = entry.size();
        if declared > MAX_ENTRY_BYTES {
            return Err(Error::msg(format!(
                "{name} 解压后有 {declared} 字节，超过单条目上限 {MAX_ENTRY_BYTES}"
            )));
        }
        if self.used.saturating_add(declared) > MAX_TOTAL_BYTES {
            qlr_core::log_warn!(
                SCOPE,
                "docx 累计解压超过 {MAX_TOTAL_BYTES} 字节，放弃读取 {name}"
            );
            return Err(Error::msg("docx 解压总量超过上限（疑似压缩炸弹）"));
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
        Ok(buf)
    }
}

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

fn decode_xml(bytes: &[u8]) -> String {
    let body = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        bytes.get(3..).unwrap_or(&[])
    } else {
        bytes
    };
    String::from_utf8_lossy(body).into_owned()
}

fn resolve_target(base_dir: &str, target: &str) -> String {
    let t = target.trim();
    if let Some(stripped) = t.strip_prefix('/') {
        return stripped.to_string();
    }
    if base_dir.is_empty() {
        t.to_string()
    } else {
        format!("{base_dir}/{}", t.trim_start_matches("./"))
    }
}

/// document.xml.rels：`Id → 解析后的 part 路径`（相对 zip 根，如 `word/media/image1.png`）。
fn parse_rels(xml: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut r = Reader::new(xml);
    loop {
        match r.next() {
            Ev::Start {
                name,
                attrs,
                empty: _,
            } => {
                if local(name) == "Relationship" {
                    let id = attrs_get(attrs, "Id").unwrap_or("");
                    let target = attrs_get(attrs, "Target").unwrap_or("");
                    if !id.is_empty() && !target.is_empty() {
                        map.insert(id.to_string(), resolve_target("word", target));
                    }
                }
                // 根元素 <Relationships>：向下走，不能整段跳过（踩过：rels 全空→图片全丢）。
            }
            Ev::Eof => break,
            _ => {}
        }
    }
    map
}

// ---------------------------------------------------------------------------
// styles.xml
// ---------------------------------------------------------------------------

fn parse_styles(xml: &str) -> (RunStyle, PProps, HashMap<String, StyleDef>) {
    let mut default_run = RunStyle::base();
    let mut default_ppr = PProps::default();
    let mut defs: HashMap<String, StyleDef> = HashMap::new();
    let mut r = Reader::new(xml);
    loop {
        match r.next() {
            Ev::Start { name, attrs, empty } => match local(name) {
                "styles" => {}
                "docDefaults" => {
                    if !empty {
                        parse_doc_defaults(&mut r, &mut default_run, &mut default_ppr);
                    }
                }
                "style" => {
                    if !empty {
                        let id = attrs_get(attrs, "w:styleId").map(str::to_string);
                        let def = parse_style_def(&mut r);
                        if let Some(id) = id {
                            defs.insert(id, def);
                        }
                    }
                }
                _ => {
                    if !empty {
                        skip_element(&mut r);
                    }
                }
            },
            Ev::Eof => break,
            _ => {}
        }
    }
    (default_run, default_ppr, defs)
}

fn parse_doc_defaults(r: &mut Reader, run: &mut RunStyle, ppr: &mut PProps) {
    let mut run_patch = RunPatch::default();
    let mut ppr_raw = PPrRaw::default();
    loop {
        match r.next() {
            Ev::Start { name, empty, .. } => match local(name) {
                "rPr" => {
                    if !empty {
                        parse_rpr(r, &mut run_patch);
                    }
                }
                "pPr" => {
                    if !empty {
                        ppr_raw = parse_ppr(r);
                    }
                }
                _ => {
                    if !empty {
                        skip_element(r);
                    }
                }
            },
            Ev::End(_) => break,
            Ev::Eof => break,
            _ => {}
        }
    }
    run_patch.apply(run);
    ppr_raw.patch.apply(ppr);
}

fn parse_style_def(r: &mut Reader) -> StyleDef {
    let mut def = StyleDef::default();
    loop {
        match r.next() {
            Ev::Start { name, attrs, empty } => match local(name) {
                "basedOn" => {
                    def.based_on = attrs_get(attrs, "w:val").map(str::to_string);
                }
                "rPr" => {
                    if !empty {
                        parse_rpr(r, &mut def.run_patch);
                    }
                }
                "pPr" => {
                    if !empty {
                        let raw = parse_ppr(r);
                        def.ppr_patch = raw.patch;
                    }
                }
                _ => {
                    if !empty {
                        skip_element(r);
                    }
                }
            },
            Ev::End(_) => break,
            Ev::Eof => break,
            _ => {}
        }
    }
    def
}

fn resolve_all(
    defs: &HashMap<String, StyleDef>,
    default_run: &RunStyle,
    default_ppr: &PProps,
) -> HashMap<String, (RunStyle, PProps)> {
    let mut out = HashMap::new();
    for id in defs.keys() {
        let v = resolve_one(id, defs, default_run, default_ppr, 0);
        out.insert(id.clone(), v);
    }
    out
}

fn resolve_one(
    id: &str,
    defs: &HashMap<String, StyleDef>,
    default_run: &RunStyle,
    default_ppr: &PProps,
    depth: usize,
) -> (RunStyle, PProps) {
    let mut run = default_run.clone();
    let mut ppr = default_ppr.clone();
    if let Some(def) = defs.get(id) {
        if let Some(base) = &def.based_on {
            if depth < 16 {
                let (r, p) = resolve_one(base, defs, default_run, default_ppr, depth + 1);
                run = r;
                ppr = p;
            }
        }
        def.run_patch.apply(&mut run);
        def.ppr_patch.apply(&mut ppr);
    }
    (run, ppr)
}

// ---------------------------------------------------------------------------
// numbering.xml
// ---------------------------------------------------------------------------

fn parse_numbering(xml: &str) -> Numbering {
    let mut abstracts: HashMap<u32, Vec<NumberingLevel>> = HashMap::new();
    let mut nums: HashMap<u32, u32> = HashMap::new();
    let mut r = Reader::new(xml);
    loop {
        match r.next() {
            Ev::Start { name, attrs, empty } => match local(name) {
                "abstractNum" => {
                    if !empty {
                        let id =
                            attrs_get(attrs, "w:abstractNumId").and_then(|v| v.parse::<u32>().ok());
                        let levels = parse_abstract_num(&mut r);
                        if let Some(id) = id {
                            abstracts.insert(id, levels);
                        }
                    }
                }
                "num" if !empty => {
                    let id = attrs_get(attrs, "w:numId").and_then(|v| v.parse::<u32>().ok());
                    let abs = parse_num(&mut r);
                    if let (Some(id), Some(abs)) = (id, abs) {
                        nums.insert(id, abs);
                    }
                }
                _ => {
                    // 根元素 <w:numbering> 及不关心的包装层：**向下走**而不是整段跳过，
                    // 否则第一个 Start 就会把整篇吞掉（踩过：numbering 全空）。
                }
            },
            Ev::Eof => break,
            _ => {}
        }
    }
    let mut out = HashMap::new();
    for (num_id, abs_id) in nums {
        if let Some(levels) = abstracts.get(&abs_id) {
            out.insert(num_id, levels.clone());
        }
    }
    out
}

fn parse_abstract_num(r: &mut Reader) -> Vec<NumberingLevel> {
    let mut levels: Vec<NumberingLevel> = Vec::new();
    loop {
        match r.next() {
            Ev::Start { name, attrs, empty } => match local(name) {
                "lvl" => {
                    if !empty {
                        let ilvl = attrs_get(attrs, "w:ilvl")
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(levels.len());
                        let level = parse_lvl(r);
                        while levels.len() < ilvl {
                            levels.push(NumberingLevel {
                                fmt: "decimal".into(),
                                start: 1,
                                text: String::new(),
                            });
                        }
                        if levels.len() == ilvl {
                            levels.push(level);
                        } else {
                            levels[ilvl] = level;
                        }
                    }
                }
                _ => {
                    if !empty {
                        skip_element(r);
                    }
                }
            },
            Ev::End(_) => break,
            Ev::Eof => break,
            _ => {}
        }
    }
    levels
}

fn parse_lvl(r: &mut Reader) -> NumberingLevel {
    let mut level = NumberingLevel {
        fmt: "decimal".into(),
        start: 1,
        text: String::new(),
    };
    loop {
        match r.next() {
            Ev::Start { name, attrs, empty } => match local(name) {
                "start" => {
                    level.start = attrs_get(attrs, "w:val")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(1);
                }
                "numFmt" => {
                    level.fmt = attrs_get(attrs, "w:val").unwrap_or("decimal").to_string();
                }
                "lvlText" => {
                    level.text = decode_entities(attrs_get(attrs, "w:val").unwrap_or(""));
                }
                _ => {
                    if !empty {
                        skip_element(r);
                    }
                }
            },
            Ev::End(_) => break,
            Ev::Eof => break,
            _ => {}
        }
    }
    level
}

fn parse_num(r: &mut Reader) -> Option<u32> {
    let mut abs = None;
    loop {
        match r.next() {
            Ev::Start { name, attrs, empty } => {
                if local(name) == "abstractNumId" {
                    abs = attrs_get(attrs, "w:val").and_then(|v| v.parse::<u32>().ok());
                }
                if !empty {
                    skip_element(r);
                }
            }
            Ev::End(_) => break,
            Ev::Eof => break,
            _ => {}
        }
    }
    abs
}

// ---------------------------------------------------------------------------
// 正文解析：document.xml / header / footer
// ---------------------------------------------------------------------------

fn parse_flows(xml: &str, ctx: &Ctx) -> (Vec<Flow>, Option<SectInfo>) {
    let mut r = Reader::new(xml);
    // 找到 body / hdr / ftr 起点
    loop {
        match r.next() {
            Ev::Start { name, .. } => {
                let n = local(name);
                if n == "body" || n == "hdr" || n == "ftr" {
                    break;
                }
            }
            Ev::Eof => return (Vec::new(), None),
            _ => {}
        }
    }
    let mut out = Vec::new();
    let mut sect: Option<SectInfo> = None;
    loop {
        match r.next() {
            Ev::Start { name, empty, .. } => match local(name) {
                "p" => {
                    if !empty {
                        parse_paragraph(&mut r, ctx, &mut out);
                    }
                }
                "tbl" => {
                    if !empty {
                        out.push(Flow::Table(parse_table(&mut r, ctx)));
                    }
                }
                "sectPr" => {
                    if !empty {
                        sect = Some(parse_sect(&mut r));
                    }
                }
                _ => {
                    if !empty {
                        skip_element(&mut r);
                    }
                }
            },
            Ev::End(name) => {
                let n = local(name);
                if n == "body" || n == "hdr" || n == "ftr" {
                    break;
                }
            }
            Ev::Eof => break,
            _ => {}
        }
    }
    (out, sect)
}

fn parse_sect(r: &mut Reader) -> SectInfo {
    let mut setup = PageSetup::a4();
    let mut header_rid = None;
    let mut footer_rid = None;
    loop {
        match r.next() {
            Ev::Start { name, attrs, empty } => match local(name) {
                "pgSz" => {
                    if let Some(v) = attrs_get(attrs, "w:w").and_then(|v| v.parse::<i64>().ok()) {
                        let w = twips_to_px(v).max(1) as u32;
                        if w > 100 {
                            setup.page_w = w;
                        }
                    }
                    if let Some(v) = attrs_get(attrs, "w:h").and_then(|v| v.parse::<i64>().ok()) {
                        let h = twips_to_px(v).max(1) as u32;
                        if h > 100 {
                            setup.page_h = h;
                        }
                    }
                }
                "pgMar" => {
                    if let Some(v) = attrs_get(attrs, "w:left").and_then(|v| v.parse::<i64>().ok())
                    {
                        setup.margin_left = twips_to_px(v);
                    }
                    if let Some(v) = attrs_get(attrs, "w:right").and_then(|v| v.parse::<i64>().ok())
                    {
                        setup.margin_right = twips_to_px(v);
                    }
                    if let Some(v) = attrs_get(attrs, "w:top").and_then(|v| v.parse::<i64>().ok()) {
                        setup.margin_top = twips_to_px(v);
                    }
                    if let Some(v) =
                        attrs_get(attrs, "w:bottom").and_then(|v| v.parse::<i64>().ok())
                    {
                        setup.margin_bottom = twips_to_px(v);
                    }
                }
                "headerReference" => {
                    let ty = attrs_get(attrs, "w:type").unwrap_or("default");
                    if (ty == "default" || ty == "first")
                        && (header_rid.is_none() || ty == "default")
                    {
                        header_rid = attrs_get(attrs, "r:id").map(str::to_string);
                    }
                }
                "footerReference" => {
                    let ty = attrs_get(attrs, "w:type").unwrap_or("default");
                    if (ty == "default" || ty == "first")
                        && (footer_rid.is_none() || ty == "default")
                    {
                        footer_rid = attrs_get(attrs, "r:id").map(str::to_string);
                    }
                }
                _ => {
                    if !empty {
                        skip_element(r);
                    }
                }
            },
            Ev::End(_) => break,
            Ev::Eof => break,
            _ => {}
        }
    }
    SectInfo {
        setup,
        header_rid,
        footer_rid,
    }
}

fn parse_paragraph(r: &mut Reader, ctx: &Ctx, out: &mut Vec<Flow>) {
    let mut ppr = PPrRaw::default();
    let mut specs: Vec<(String, RunPatch)> = Vec::new();
    let mut images: Vec<PendingImage> = Vec::new();
    loop {
        match r.next() {
            Ev::Start { name, empty, .. } => match local(name) {
                "pPr" => {
                    if !empty {
                        ppr = parse_ppr(r);
                    }
                }
                "r" => {
                    if !empty {
                        parse_run(r, &mut specs, &mut images);
                    }
                }
                "hyperlink" | "smartTag" | "ins" | "del" => {
                    if !empty {
                        parse_paragraph_container(r, &mut specs, &mut images);
                    }
                }
                "bookmarkStart"
                | "bookmarkEnd"
                | "proofErr"
                | "commentRangeStart"
                | "commentRangeEnd"
                | "lastRenderedPageBreak"
                | "commentReference"
                | "footnoteReference"
                | "endnoteReference" => {
                    if !empty {
                        skip_element(r);
                    }
                }
                _ => {
                    if !empty {
                        skip_element(r);
                    }
                }
            },
            Ev::End(name) => {
                if local(name) == "p" {
                    break;
                }
            }
            Ev::Eof => break,
            _ => {}
        }
    }

    // ---- 解析样式，组装最终 run ----
    let (base_run, base_ppr) = ctx.style_base(ppr.style_id.as_deref());
    let mut props = base_ppr;
    ppr.patch.apply(&mut props);
    let mut runs = Vec::new();
    for (text, patch) in specs {
        if text.is_empty() {
            continue;
        }
        if runs.len() >= MAX_RUNS_PER_PARA {
            break;
        }
        let mut st = base_run.clone();
        patch.apply(&mut st);
        runs.push(Run {
            text,
            bold: st.bold,
            italic: st.italic,
            underline: st.underline,
            strike: st.strike,
            size_dpt: st.size_dpt,
            color: st.color,
            highlight: st.highlight,
            mono: st.mono,
        });
    }

    out.push(Flow::Para(ParaFlow {
        runs,
        align: props.align,
        space_before: props.space_before,
        space_after: props.space_after,
        indent_left: props.indent_left,
        indent_right: props.indent_right,
        line_mult: props.line_mult,
        line_fixed: props.line_fixed,
        page_break_before: props.page_break_before,
        num: ppr.num,
    }));
    for img in images {
        out.push(Flow::Image(img));
    }
}

/// 处理 `w:hyperlink`/`w:smartTag` 等"透明容器"里的 run（只取文字，忽略链接语义）。
fn parse_paragraph_container(
    r: &mut Reader,
    specs: &mut Vec<(String, RunPatch)>,
    images: &mut Vec<PendingImage>,
) {
    loop {
        match r.next() {
            Ev::Start { name, empty, .. } => match local(name) {
                "r" => {
                    if !empty {
                        parse_run(r, specs, images);
                    }
                }
                "pPr" => {
                    if !empty {
                        skip_element(r);
                    }
                }
                _ => {
                    if !empty {
                        skip_element(r);
                    }
                }
            },
            Ev::End(_) => break,
            Ev::Eof => break,
            _ => {}
        }
    }
}

fn parse_ppr(r: &mut Reader) -> PPrRaw {
    let mut raw = PPrRaw::default();
    loop {
        match r.next() {
            Ev::Start { name, attrs, empty } => match local(name) {
                "pStyle" => {
                    raw.style_id = Some(attrs_get(attrs, "w:val").unwrap_or("").to_string());
                }
                "jc" => {
                    raw.patch.align = Some(parse_align(attrs_get(attrs, "w:val")));
                }
                "spacing" => apply_spacing(attrs, &mut raw.patch),
                "ind" => apply_ind(attrs, &mut raw.patch),
                "pageBreakBefore" => raw.patch.page_break_before = Some(true),
                "numPr" => {
                    if !empty {
                        raw.num = parse_numpr(r);
                    }
                }
                _ => {
                    if !empty {
                        skip_element(r);
                    }
                }
            },
            Ev::End(name) => {
                if local(name) == "pPr" {
                    break;
                }
            }
            Ev::Eof => break,
            _ => {}
        }
    }
    raw
}

fn apply_spacing(attrs: &str, patch: &mut PPrPatch) {
    if let Some(v) = attrs_get(attrs, "w:before").and_then(|v| v.parse::<i64>().ok()) {
        patch.space_before = Some(twips_to_px(v));
    }
    if let Some(v) = attrs_get(attrs, "w:after").and_then(|v| v.parse::<i64>().ok()) {
        patch.space_after = Some(twips_to_px(v));
    }
    let rule = attrs_get(attrs, "w:lineRule").unwrap_or("auto");
    if let Some(line) = attrs_get(attrs, "w:line").and_then(|v| v.parse::<i64>().ok()) {
        if line > 0 {
            if rule == "auto" {
                patch.line_mult = Some((line as f32) / 240.0);
            } else {
                patch.line_mult = Some(1.0);
                patch.line_fixed = Some(twips_to_px(line).max(1));
            }
        }
    }
}

fn apply_ind(attrs: &str, patch: &mut PPrPatch) {
    if let Some(v) = attrs_get(attrs, "w:left")
        .or_else(|| attrs_get(attrs, "w:start"))
        .and_then(|v| v.parse::<i64>().ok())
    {
        patch.indent_left = Some(twips_to_px(v));
    }
    if let Some(v) = attrs_get(attrs, "w:right")
        .or_else(|| attrs_get(attrs, "w:end"))
        .and_then(|v| v.parse::<i64>().ok())
    {
        patch.indent_right = Some(twips_to_px(v));
    }
    if let Some(v) = attrs_get(attrs, "w:firstLine").and_then(|v| v.parse::<i64>().ok()) {
        patch.indent_first = Some(twips_to_px(v));
    } else if let Some(v) = attrs_get(attrs, "w:hanging").and_then(|v| v.parse::<i64>().ok()) {
        patch.indent_first = Some(-twips_to_px(v));
    }
}

fn parse_numpr(r: &mut Reader) -> Option<(u32, u32)> {
    let mut ilvl = 0u32;
    let mut num_id = None;
    loop {
        match r.next() {
            Ev::Start { name, attrs, empty } => {
                if local(name) == "ilvl" {
                    ilvl = attrs_get(attrs, "w:val")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                } else if local(name) == "numId" {
                    num_id = attrs_get(attrs, "w:val").and_then(|v| v.parse::<u32>().ok());
                }
                if !empty {
                    skip_element(r);
                }
            }
            Ev::End(_) => break,
            Ev::Eof => break,
            _ => {}
        }
    }
    num_id.filter(|&n| n != 0).map(|n| (n, ilvl))
}

fn parse_align(val: Option<&str>) -> HAlign {
    match val.unwrap_or("left") {
        "center" => HAlign::Center,
        "right" | "end" => HAlign::Right,
        "both" | "distribute" | "thaiDistribute" | "justify" => HAlign::Justify,
        _ => HAlign::Left,
    }
}

fn parse_run(r: &mut Reader, specs: &mut Vec<(String, RunPatch)>, images: &mut Vec<PendingImage>) {
    let mut patch = RunPatch::default();
    let mut text = String::new();
    loop {
        match r.next() {
            Ev::Start { name, attrs, empty } => match local(name) {
                "rPr" => {
                    if !empty {
                        parse_rpr(r, &mut patch);
                    }
                }
                "t" => {
                    if !empty {
                        text.push_str(&read_text_node(r));
                    }
                }
                "delText" => {
                    if !empty {
                        skip_element(r);
                    }
                }
                "br" => {
                    let ty = attrs_get(attrs, "w:type").unwrap_or("");
                    if ty == "page" || ty == "column" {
                        text.push('\u{0C}');
                    } else {
                        text.push('\n');
                    }
                }
                "cr" => text.push('\n'),
                "tab" => text.push('\t'),
                "drawing" => {
                    if !empty {
                        if let Some(img) = parse_drawing(r) {
                            images.push(img);
                        }
                    }
                }
                "pict" => {
                    if !empty {
                        if let Some(img) = parse_vml_pict(r) {
                            images.push(img);
                        }
                    }
                }
                _ => {
                    if !empty {
                        skip_element(r);
                    }
                }
            },
            Ev::End(name) => {
                if local(name) == "r" {
                    break;
                }
            }
            Ev::Eof => break,
            _ => {}
        }
    }
    if !text.is_empty() {
        specs.push((text, patch));
    }
}

fn parse_rpr(r: &mut Reader, patch: &mut RunPatch) {
    loop {
        match r.next() {
            Ev::Start { name, attrs, empty } => match local(name) {
                "b" => patch.bold = Some(toggle_on(attrs)),
                "i" => patch.italic = Some(toggle_on(attrs)),
                "u" => {
                    let v = attrs_get(attrs, "w:val").unwrap_or("single");
                    patch.underline = Some(v != "none");
                }
                "strike" => patch.strike = Some(toggle_on(attrs)),
                "sz" => {
                    if let Some(v) = attrs_get(attrs, "w:val").and_then(|v| v.parse::<u16>().ok()) {
                        if v > 0 {
                            patch.size_dpt = Some((v as u32 * 5).min(u32::from(u16::MAX)) as u16);
                        }
                    }
                }
                "color" => {
                    if let Some(v) = attrs_get(attrs, "w:val") {
                        if v != "auto" {
                            if let Some(c) = parse_hex_color(v) {
                                patch.color = Some(c);
                            }
                        }
                    }
                }
                "highlight" => {
                    if let Some(v) = attrs_get(attrs, "w:val") {
                        if let Some(c) = highlight_color(v) {
                            patch.highlight = Some(c);
                        }
                    }
                }
                "rFonts" => {
                    let face = attrs_get(attrs, "w:ascii")
                        .or_else(|| attrs_get(attrs, "w:hAnsi"))
                        .or_else(|| attrs_get(attrs, "w:eastAsia"))
                        .unwrap_or("");
                    if face.contains("onsolas")
                        || face.contains("ourier")
                        || face.contains("Mono")
                        || face.contains("mono")
                    {
                        patch.mono = Some(true);
                    }
                }
                _ => {
                    if !empty {
                        skip_element(r);
                    }
                }
            },
            Ev::End(name) => {
                if local(name) == "rPr" {
                    break;
                }
            }
            Ev::Eof => break,
            _ => {}
        }
    }
}

fn toggle_on(attrs: &str) -> bool {
    !matches!(
        attrs_get(attrs, "w:val"),
        Some("0") | Some("false") | Some("off")
    )
}

fn parse_hex_color(v: &str) -> Option<u32> {
    let hex = v.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

fn highlight_color(name: &str) -> Option<u32> {
    Some(match name {
        "yellow" => 0xFFFF00,
        "green" => 0x00FF00,
        "cyan" => 0x00FFFF,
        "magenta" => 0xFF00FF,
        "blue" => 0x0000FF,
        "red" => 0xFF0000,
        "darkBlue" => 0x000080,
        "darkCyan" => 0x008080,
        "darkGreen" => 0x008000,
        "darkMagenta" => 0x800080,
        "darkRed" => 0x800000,
        "darkYellow" => 0x808000,
        "darkGray" => 0x808080,
        "lightGray" => 0xC0C0C0,
        "black" => 0x000000,
        "white" => 0xFFFFFF,
        _ => return None,
    })
}

/// 解析 `w:drawing` 里的 `wp:extent`/`a:ext`（显示尺寸，EMU）与 `a:blip r:embed`。
fn parse_drawing(r: &mut Reader) -> Option<PendingImage> {
    let mut cx = 0i64;
    let mut cy = 0i64;
    let mut embed: Option<String> = None;
    let mut alt = String::new();
    let mut depth = 0usize;
    loop {
        match r.next() {
            Ev::Start { name, attrs, empty } => {
                match local(name) {
                    "extent" | "ext" => {
                        if cx == 0 {
                            if let Some(v) =
                                attrs_get(attrs, "cx").and_then(|v| v.parse::<i64>().ok())
                            {
                                cx = v;
                            }
                            if let Some(v) =
                                attrs_get(attrs, "cy").and_then(|v| v.parse::<i64>().ok())
                            {
                                cy = v;
                            }
                        }
                    }
                    "blip" => {
                        if embed.is_none() {
                            embed = attrs_get(attrs, "r:embed")
                                .or_else(|| attrs_get(attrs, "r:link"))
                                .map(str::to_string);
                        }
                    }
                    "docPr" => {
                        alt = attrs_get(attrs, "descr")
                            .or_else(|| attrs_get(attrs, "name"))
                            .unwrap_or("")
                            .to_string();
                    }
                    _ => {}
                }
                if !empty {
                    depth += 1;
                }
            }
            Ev::End(name) => {
                if depth == 0 {
                    if local(name) == "drawing" {
                        break;
                    }
                } else {
                    depth -= 1;
                }
            }
            Ev::Eof => break,
            _ => {}
        }
    }
    let rid = embed?;
    let w = emu_to_px(cx);
    let h = emu_to_px(cy);
    Some(PendingImage { rid, w, h, alt })
}

/// VML 旧式图片（`w:pict` → `v:imagedata r:id`），尽力解析；失败返回 None。
fn parse_vml_pict(r: &mut Reader) -> Option<PendingImage> {
    let mut rid: Option<String> = None;
    let mut width = 0i32;
    let mut height = 0i32;
    let mut depth = 0usize;
    loop {
        match r.next() {
            Ev::Start { name, attrs, empty } => {
                match local(name) {
                    "imagedata" => {
                        rid = attrs_get(attrs, "r:id")
                            .or_else(|| attrs_get(attrs, "r:embed"))
                            .map(str::to_string);
                    }
                    "shape" => {
                        if let Some(style) = attrs_get(attrs, "style") {
                            width = style_px(style, "width");
                            height = style_px(style, "height");
                        }
                    }
                    _ => {}
                }
                if !empty {
                    depth += 1;
                }
            }
            Ev::End(name) => {
                if depth == 0 {
                    if local(name) == "pict" {
                        break;
                    }
                } else {
                    depth -= 1;
                }
            }
            Ev::Eof => break,
            _ => {}
        }
    }
    let rid = rid?;
    Some(PendingImage {
        rid,
        w: width,
        h: height,
        alt: String::new(),
    })
}

/// 从 VML style 串里取 `width:123pt` / `width:100px` 的像素值。
fn style_px(style: &str, key: &str) -> i32 {
    let needle = format!("{key}:");
    let Some(pos) = style.find(&needle) else {
        return 0;
    };
    let rest = style[pos + needle.len()..].trim_start();
    let mut num = String::new();
    let mut unit = String::new();
    for ch in rest.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            if unit.is_empty() {
                num.push(ch);
            } else {
                break;
            }
        } else if ch == 'p' || ch == 'x' || ch == 't' || ch == 'e' || ch == 'm' || ch == 'i' {
            unit.push(ch);
        } else {
            break;
        }
    }
    let value: f64 = num.parse().unwrap_or(0.0);
    match unit.as_str() {
        "pt" => (value * 96.0 / 72.0).round() as i32,
        "in" => (value * 96.0).round() as i32,
        "cm" => (value * 96.0 / 2.54).round() as i32,
        "mm" => (value * 96.0 / 25.4).round() as i32,
        _ => value.round() as i32,
    }
}

// ---------------------------------------------------------------------------
// 表格解析
// ---------------------------------------------------------------------------

fn parse_table(r: &mut Reader, ctx: &Ctx) -> TableFlow {
    let mut tf = TableFlow::default();
    loop {
        match r.next() {
            Ev::Start { name, empty, .. } => match local(name) {
                "tblPr" => {
                    if !empty {
                        parse_tblpr(r, &mut tf);
                    }
                }
                "tblGrid" => {
                    if !empty {
                        parse_tblgrid(r, &mut tf);
                    }
                }
                "tr" => {
                    if !empty {
                        tf.rows.push(parse_row(r, ctx));
                    }
                }
                _ => {
                    if !empty {
                        skip_element(r);
                    }
                }
            },
            Ev::End(name) => {
                if local(name) == "tbl" {
                    break;
                }
            }
            Ev::Eof => break,
            _ => {}
        }
    }
    tf
}

fn parse_tblpr(r: &mut Reader, tf: &mut TableFlow) {
    loop {
        match r.next() {
            Ev::Start { name, empty, .. } => {
                if local(name) == "tblBorders" {
                    tf.borders = true;
                    if !empty {
                        skip_element(r);
                    }
                } else if !empty {
                    skip_element(r);
                }
            }
            Ev::End(_) => break,
            Ev::Eof => break,
            _ => {}
        }
    }
}

fn parse_tblgrid(r: &mut Reader, tf: &mut TableFlow) {
    loop {
        match r.next() {
            Ev::Start { name, attrs, empty } => {
                if local(name) == "gridCol" {
                    if let Some(v) = attrs_get(attrs, "w:w").and_then(|v| v.parse::<i64>().ok()) {
                        tf.col_widths.push(twips_to_px(v).max(8));
                    }
                }
                if !empty {
                    skip_element(r);
                }
            }
            Ev::End(_) => break,
            Ev::Eof => break,
            _ => {}
        }
    }
}

fn parse_row(r: &mut Reader, ctx: &Ctx) -> RowFlow {
    let mut row = RowFlow {
        cells: Vec::new(),
        min_h: 0,
    };
    loop {
        match r.next() {
            Ev::Start { name, empty, .. } => match local(name) {
                "trPr" => {
                    if !empty {
                        parse_trpr(r, &mut row);
                    }
                }
                "tc" => {
                    if !empty {
                        row.cells.push(parse_cell(r, ctx));
                    }
                }
                _ => {
                    if !empty {
                        skip_element(r);
                    }
                }
            },
            Ev::End(name) => {
                if local(name) == "tr" {
                    break;
                }
            }
            Ev::Eof => break,
            _ => {}
        }
    }
    row
}

fn parse_trpr(r: &mut Reader, row: &mut RowFlow) {
    loop {
        match r.next() {
            Ev::Start { name, attrs, empty } => {
                if local(name) == "trHeight" {
                    if let Some(v) = attrs_get(attrs, "w:val").and_then(|v| v.parse::<i64>().ok()) {
                        row.min_h = twips_to_px(v).max(0);
                    }
                }
                if !empty {
                    skip_element(r);
                }
            }
            Ev::End(_) => break,
            Ev::Eof => break,
            _ => {}
        }
    }
}

fn parse_cell(r: &mut Reader, ctx: &Ctx) -> CellFlow {
    let mut cell = CellFlow {
        colspan: 1,
        vmerge: VMerge::None,
        fill: None,
        blocks: Vec::new(),
    };
    loop {
        match r.next() {
            Ev::Start { name, empty, .. } => match local(name) {
                "tcPr" => {
                    if !empty {
                        parse_tcpr(r, &mut cell);
                    }
                }
                "p" => {
                    if !empty {
                        parse_paragraph(r, ctx, &mut cell.blocks);
                    }
                }
                "tbl" => {
                    if !empty {
                        cell.blocks.push(Flow::Table(parse_table(r, ctx)));
                    }
                }
                _ => {
                    if !empty {
                        skip_element(r);
                    }
                }
            },
            Ev::End(name) => {
                if local(name) == "tc" {
                    break;
                }
            }
            Ev::Eof => break,
            _ => {}
        }
    }
    cell
}

fn parse_tcpr(r: &mut Reader, cell: &mut CellFlow) {
    loop {
        match r.next() {
            Ev::Start { name, attrs, empty } => match local(name) {
                "gridSpan" => {
                    if let Some(v) = attrs_get(attrs, "w:val").and_then(|v| v.parse::<u32>().ok()) {
                        cell.colspan = v.max(1);
                    }
                }
                "vMerge" => {
                    let v = attrs_get(attrs, "w:val").unwrap_or("continue");
                    cell.vmerge = if v == "restart" {
                        VMerge::Restart
                    } else {
                        VMerge::Continue
                    };
                }
                "shd" => {
                    if let Some(fill) = attrs_get(attrs, "w:fill") {
                        if fill != "auto" && fill != "none" {
                            if let Some(c) = parse_hex_color(fill) {
                                cell.fill = Some(c);
                            }
                        }
                    }
                }
                _ => {
                    if !empty {
                        skip_element(r);
                    }
                }
            },
            Ev::End(_) => break,
            Ev::Eof => break,
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// 排版
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct WrappedLine {
    height: i32,
    runs: Vec<Run>,
}

struct WrappedPara {
    /// 每个分组 = 一次"分页符"隔开的连续行；`page_break_before` 表示这组之前要换页。
    groups: Vec<(bool, Vec<WrappedLine>)>,
}

fn same_style(a: &Run, b: &Run) -> bool {
    a.bold == b.bold
        && a.italic == b.italic
        && a.underline == b.underline
        && a.strike == b.strike
        && a.size_dpt == b.size_dpt
        && a.color == b.color
        && a.highlight == b.highlight
        && a.mono == b.mono
}

fn line_height(max_px: i32, base_px: i32, line_mult: f32, line_fixed: Option<i32>) -> i32 {
    let px = max_px.max(base_px).max(1);
    let mut h = (px as f32 * line_mult.max(0.5)).round() as i32;
    if let Some(f) = line_fixed {
        h = h.max(f);
    }
    h.max(px)
}

/// 把一段的 run 序列按 `avail` 折行。返回按分页符切分的分组。
fn wrap_para(
    runs: &[Run],
    avail: i32,
    line_mult: f32,
    line_fixed: Option<i32>,
    base_px: i32,
    measure: &Measure,
) -> WrappedPara {
    // 1) 把 run 拍平成 atom（文字原子 / 换行 / 分页）
    enum Kind {
        Text,
        LineBreak,
        PageBreak,
    }
    struct Atom<'a> {
        text: &'a str,
        style: &'a Run,
        kind: Kind,
    }
    let mut atoms: Vec<Atom> = Vec::new();
    for run in runs {
        let t = &run.text;
        let mut i = 0usize;
        while i < t.len() {
            let ch = t[i..].chars().next().unwrap();
            match ch {
                '\u{0C}' => {
                    atoms.push(Atom {
                        text: "",
                        style: run,
                        kind: Kind::PageBreak,
                    });
                    i += ch.len_utf8();
                }
                '\n' => {
                    atoms.push(Atom {
                        text: "",
                        style: run,
                        kind: Kind::LineBreak,
                    });
                    i += ch.len_utf8();
                }
                '\t' => {
                    atoms.push(Atom {
                        text: &t[i..i + ch.len_utf8()],
                        style: run,
                        kind: Kind::Text,
                    });
                    i += ch.len_utf8();
                }
                _ => {
                    if is_wide(ch as u32) || ch == ' ' {
                        atoms.push(Atom {
                            text: &t[i..i + ch.len_utf8()],
                            style: run,
                            kind: Kind::Text,
                        });
                        i += ch.len_utf8();
                    } else {
                        let start = i;
                        while i < t.len() {
                            let c = t[i..].chars().next().unwrap();
                            if c == '\n'
                                || c == '\u{0C}'
                                || c == '\t'
                                || c == ' '
                                || is_wide(c as u32)
                            {
                                break;
                            }
                            i += c.len_utf8();
                        }
                        atoms.push(Atom {
                            text: &t[start..i],
                            style: run,
                            kind: Kind::Text,
                        });
                    }
                }
            }
        }
    }

    // 2) 按分页符切分成段
    let mut segments: Vec<Vec<usize>> = vec![Vec::new()];
    for (idx, atom) in atoms.iter().enumerate() {
        if let Kind::PageBreak = atom.kind {
            segments.push(Vec::new());
        } else {
            segments.last_mut().unwrap().push(idx);
        }
    }

    // 3) 逐段折行
    let mut groups: Vec<(bool, Vec<WrappedLine>)> = Vec::new();
    let seg_count = segments.len();
    for (si, seg) in segments.iter().enumerate() {
        let mut lines: Vec<WrappedLine> = Vec::new();
        let mut cur_runs: Vec<Run> = Vec::new();
        let mut cur_w = 0i32;
        let mut cur_maxpx = 0i32;
        let flush =
            |runs: &mut Vec<Run>, w: &mut i32, maxpx: &mut i32, lines: &mut Vec<WrappedLine>| {
                let h = line_height(*maxpx, base_px, line_mult, line_fixed);
                lines.push(WrappedLine {
                    height: h,
                    runs: std::mem::take(runs),
                });
                *w = 0;
                *maxpx = 0;
            };
        for &ai in seg {
            let atom = &atoms[ai];
            match atom.kind {
                Kind::LineBreak => {
                    flush(&mut cur_runs, &mut cur_w, &mut cur_maxpx, &mut lines);
                }
                Kind::Text => {
                    let w = measure(atom.text, atom.style.size_dpt, atom.style.bold);
                    if cur_w + w > avail && cur_w > 0 {
                        flush(&mut cur_runs, &mut cur_w, &mut cur_maxpx, &mut lines);
                        if atom.text == " " {
                            continue;
                        }
                    }
                    // 追加到当前行
                    let mut merged = false;
                    if let Some(last) = cur_runs.last_mut() {
                        if same_style(last, atom.style) {
                            last.text.push_str(atom.text);
                            merged = true;
                        }
                    }
                    if !merged {
                        let mut r = atom.style.clone();
                        r.text = atom.text.to_string();
                        cur_runs.push(r);
                    }
                    cur_w += w;
                    cur_maxpx = cur_maxpx.max(atom.style.size_px());
                }
                Kind::PageBreak => {}
            }
        }
        // 末行：有内容或整段为空，都要产出一行（空段落 = 空行）
        if !cur_runs.is_empty() || lines.is_empty() {
            flush(&mut cur_runs, &mut cur_w, &mut cur_maxpx, &mut lines);
        }
        // 空段（无 atom）时 lines 至少 1 行
        if lines.is_empty() {
            let h = line_height(base_px, base_px, line_mult, line_fixed);
            lines.push(WrappedLine {
                height: h,
                runs: Vec::new(),
            });
        }
        // 末尾多余的空分页段（文档以分页符结尾）丢弃
        if si + 1 == seg_count
            && lines.iter().all(|l| l.runs.is_empty())
            && groups.iter().any(|g| g.0 || !g.1.is_empty())
        {
            // 仅当它不是唯一内容时才丢弃
            let only_empty_group = groups.is_empty() && lines.len() == 1;
            if !only_empty_group {
                continue;
            }
        }
        groups.push((si > 0, lines));
    }
    if groups.is_empty() {
        groups.push((
            false,
            vec![WrappedLine {
                height: line_height(base_px, base_px, line_mult, line_fixed),
                runs: Vec::new(),
            }],
        ));
    }
    WrappedPara { groups }
}

fn numbering_marker(
    num_id: u32,
    ilvl: u32,
    ctx: &Ctx,
    counters: &mut HashMap<(u32, u32), i32>,
) -> Option<String> {
    let levels = ctx.numbering.get(&num_id)?;
    let level = levels.get(ilvl as usize)?;
    if level.fmt == "bullet" {
        let bullet = if level.text.is_empty() {
            "•"
        } else {
            &level.text
        };
        return Some(format!("{bullet} "));
    }
    let counter = counters.entry((num_id, ilvl)).or_insert(level.start - 1);
    *counter += 1;
    let n = *counter;
    let num = match level.fmt.as_str() {
        "lowerLetter" => letter(n, false),
        "upperLetter" => letter(n, true),
        "lowerRoman" => roman(n, false),
        "upperRoman" => roman(n, true),
        _ => n.to_string(),
    };
    // 从 lvlText 里取分隔后缀（如 "%1." 的 "."、"%1)" 的 ")"）
    let suffix = if let Some(pos) = level.text.find('%') {
        level.text[pos + 1..].trim_start_matches(|c: char| c.is_ascii_digit())
    } else {
        "."
    };
    let suffix = if suffix.is_empty() { "." } else { suffix };
    Some(format!("{num}{suffix} "))
}

fn letter(mut n: i32, upper: bool) -> String {
    if n <= 0 {
        n = 1;
    }
    let base = if upper { b'A' } else { b'a' };
    // 只处理 1–26，超出用数字兜底
    if n <= 26 {
        ((base + (n as u8 - 1)) as char).to_string()
    } else {
        n.to_string()
    }
}

fn roman(mut n: i32, upper: bool) -> String {
    if n <= 0 {
        n = 1;
    }
    const VALS: [(i32, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut out = String::new();
    let mut left = n;
    for (v, s) in VALS {
        while left >= v {
            out.push_str(s);
            left -= v;
        }
    }
    if upper { out.to_uppercase() } else { out }
}

struct Builder {
    pages: Vec<Page>,
    cur: Page,
    y: i32,
    top: i32,
    bottom: i32,
    left: i32,
    content_w: i32,
}

impl Builder {
    fn new(top: i32, bottom: i32, left: i32, content_w: i32) -> Self {
        Self {
            pages: Vec::new(),
            cur: Page::default(),
            y: top,
            top,
            bottom,
            left,
            content_w,
        }
    }
    fn is_empty(&self) -> bool {
        self.cur.blocks.is_empty()
    }
    fn new_page(&mut self) {
        if !self.cur.blocks.is_empty() {
            self.pages.push(std::mem::take(&mut self.cur));
        } else {
            self.cur = Page::default();
        }
        self.y = self.top;
    }
    fn finish(mut self) -> Vec<Page> {
        if !self.cur.blocks.is_empty() || self.pages.is_empty() {
            self.pages.push(self.cur);
        }
        self.pages
    }
}

fn push_para_segment(b: &mut Builder, lines: Vec<Line>, align: HAlign, x: i32, w: i32, top: i32) {
    if lines.is_empty() {
        return;
    }
    let bottom = lines.last().map(|l| l.rect.bottom()).unwrap_or(top);
    b.cur.blocks.push(Block::Para(Para {
        rect: Rect::new(x, top, w, bottom - top),
        align,
        lines,
        space_before: 0,
        space_after: 0,
    }));
}

fn place_para(
    b: &mut Builder,
    pf: &ParaFlow,
    ctx: &Ctx,
    counters: &mut HashMap<(u32, u32), i32>,
    measure: &Measure,
    force_break: bool,
) {
    let mut runs = pf.runs.clone();
    if let Some((num_id, ilvl)) = pf.num {
        if let Some(marker) = numbering_marker(num_id, ilvl, ctx, counters) {
            let mut r = if let Some(first) = pf.runs.first() {
                first.clone()
            } else {
                Run::default()
            };
            r.text = marker;
            runs.insert(0, r);
        }
    }
    let base_px = pf
        .runs
        .first()
        .map(|r| r.size_px())
        .unwrap_or_else(|| dpt_to_px(105));
    let avail = (b.content_w - pf.indent_left - pf.indent_right).max(24);
    let wrapped = wrap_para(&runs, avail, pf.line_mult, pf.line_fixed, base_px, measure);
    let x = b.left + pf.indent_left;

    if (pf.page_break_before || force_break) && !b.is_empty() {
        b.new_page();
    }

    let space_before = pf.space_before;
    let mut first_group = true;
    for (pbb, lines) in wrapped.groups {
        if pbb && !b.is_empty() {
            b.new_page();
        }
        if first_group && !b.is_empty() && space_before > 0 {
            b.y += space_before;
        }
        first_group = false;

        let mut seg: Vec<Line> = Vec::new();
        let mut seg_top = b.y;
        for wl in lines {
            let h = wl.height;
            if b.y + h > b.bottom && !(b.is_empty() && seg.is_empty()) {
                if !seg.is_empty() {
                    push_para_segment(b, std::mem::take(&mut seg), pf.align, x, avail, seg_top);
                }
                b.new_page();
                seg_top = b.y;
            }
            let line = Line {
                rect: Rect::new(x, b.y, avail, h),
                runs: wl.runs,
            };
            b.y += h;
            seg.push(line);
        }
        if !seg.is_empty() {
            push_para_segment(b, seg, pf.align, x, avail, seg_top);
        }
    }
    b.y += pf.space_after;
}

fn shift_block(block: &mut Block, dx: i32, dy: i32) {
    match block {
        Block::Para(p) => {
            p.rect.x += dx;
            p.rect.y += dy;
            for line in &mut p.lines {
                line.rect.x += dx;
                line.rect.y += dy;
            }
        }
        Block::Table(t) => {
            t.rect.x += dx;
            t.rect.y += dy;
            for row in &mut t.rows {
                row.rect.x += dx;
                row.rect.y += dy;
                for cell in &mut row.cells {
                    cell.rect.x += dx;
                    cell.rect.y += dy;
                    for blk in &mut cell.blocks {
                        shift_block(blk, dx, dy);
                    }
                }
            }
        }
        Block::Rule(r) => {
            r.x += dx;
            r.y += dy;
        }
        Block::Image(i) => {
            i.rect.x += dx;
            i.rect.y += dy;
        }
    }
}

fn layout_table(
    tf: &TableFlow,
    avail: i32,
    measure: &Measure,
    left: i32,
    top: i32,
) -> (Table, i32) {
    let mut cols: Vec<i32> = if tf.col_widths.is_empty() {
        vec![avail.max(40)]
    } else {
        tf.col_widths.clone()
    };
    let sum: i32 = cols.iter().sum();
    if sum > avail && sum > 0 {
        let f = avail as f32 / sum as f32;
        for c in &mut cols {
            *c = ((*c as f32 * f) as i32).max(12);
        }
    }
    let ncol = cols.len();
    let mut col_x = vec![0i32; ncol + 1];
    for i in 0..ncol {
        col_x[i + 1] = col_x[i] + cols[i];
    }
    let tw = *col_x.last().unwrap_or(&0);

    // 逐行排版单元格，求出行高
    struct LaidCell {
        blocks: Vec<Block>,
        colspan: usize,
        vmerge: VMerge,
        fill: Option<u32>,
    }
    let mut row_heights: Vec<i32> = Vec::new();
    let mut laid_rows: Vec<Vec<LaidCell>> = Vec::new();
    for row in &tf.rows {
        let mut col = 0usize;
        let mut laid: Vec<LaidCell> = Vec::new();
        let mut rh = row.min_h.max(0);
        for cell in &row.cells {
            let span = (cell.colspan.max(1) as usize).min(ncol.saturating_sub(col).max(1));
            let sw = if col + span <= ncol {
                col_x[col + span] - col_x[col]
            } else {
                (tw - col_x[col.min(ncol)]).max(10)
            };
            let avail_c = (sw - 2 * CELL_PAD).max(10);
            let (blocks, h) = if cell.vmerge == VMerge::Continue {
                (Vec::new(), 0)
            } else {
                stack_flows(&cell.blocks, avail_c, measure, 0, 0)
            };
            rh = rh.max(h + 2 * CELL_PAD);
            laid.push(LaidCell {
                blocks,
                colspan: span,
                vmerge: cell.vmerge,
                fill: cell.fill,
            });
            col += span;
            if col >= ncol {
                break;
            }
        }
        rh = rh.max(18);
        row_heights.push(rh);
        laid_rows.push(laid);
    }
    let table_h: i32 = row_heights.iter().sum();

    let mut trows: Vec<TableRow> = Vec::new();
    let mut ry = top;
    for (ri, laid) in laid_rows.into_iter().enumerate() {
        let rh = row_heights[ri];
        let mut col = 0usize;
        let mut cells: Vec<TableCell> = Vec::new();
        for lc in laid {
            let cx = left + col_x[col];
            let span = lc.colspan;
            let cw = col_x[(col + span).min(ncol)] - col_x[col];
            let mut blocks = lc.blocks;
            for blk in &mut blocks {
                shift_block(blk, cx + CELL_PAD, ry + CELL_PAD);
            }
            cells.push(TableCell {
                rect: Rect::new(cx, ry, cw, rh),
                blocks,
                colspan: span as u32,
                rowspan: 1,
                v_merged: lc.vmerge == VMerge::Continue,
                fill: lc.fill,
            });
            col += span;
        }
        trows.push(TableRow {
            rect: Rect::new(left, ry, tw, rh),
            cells,
        });
        ry += rh;
    }

    (
        Table {
            rect: Rect::new(left, top, tw, table_h),
            col_x,
            rows: trows,
            borders: tf.borders,
        },
        top + table_h,
    )
}

/// 无分页地顺序堆叠一组 flow（供单元格使用）。返回 (块, 结束 y)。
fn stack_flows(
    flows: &[Flow],
    avail: i32,
    measure: &Measure,
    left: i32,
    top: i32,
) -> (Vec<Block>, i32) {
    let mut out: Vec<Block> = Vec::new();
    let mut y = top;
    for flow in flows {
        match flow {
            Flow::Para(pf) => {
                let base_px = pf
                    .runs
                    .first()
                    .map(|r| r.size_px())
                    .unwrap_or_else(|| dpt_to_px(105));
                let para_avail = (avail - pf.indent_left - pf.indent_right).max(10);
                let wrapped = wrap_para(
                    &pf.runs,
                    para_avail,
                    pf.line_mult,
                    pf.line_fixed,
                    base_px,
                    measure,
                );
                let x = left + pf.indent_left;
                let seg_top = y;
                let mut lines: Vec<Line> = Vec::new();
                for (_pbb, group_lines) in wrapped.groups {
                    for wl in group_lines {
                        let h = wl.height;
                        lines.push(Line {
                            rect: Rect::new(x, y, para_avail, h),
                            runs: wl.runs,
                        });
                        y += h;
                    }
                }
                if !lines.is_empty() {
                    let bottom = lines.last().map(|l| l.rect.bottom()).unwrap_or(y);
                    out.push(Block::Para(Para {
                        rect: Rect::new(x, seg_top, para_avail, bottom - seg_top),
                        align: pf.align,
                        lines,
                        space_before: 0,
                        space_after: 0,
                    }));
                }
                y += pf.space_after;
            }
            Flow::Table(tf) => {
                let (table, ny) = layout_table(tf, avail, measure, left, y);
                out.push(Block::Table(table));
                y = ny;
            }
            Flow::Image(img) => {
                // 单元格里的图片：暂不贴（保留高度），避免依赖 image_map
                let (w, h) = fit_image(img.w, img.h, avail);
                if w > 0 && h > 0 {
                    out.push(Block::Image(ImageBlock {
                        rect: Rect::new(left, y, w, h),
                        image: u32::MAX,
                        alt: img.alt.clone(),
                    }));
                    y += h;
                }
            }
        }
    }
    (out, y)
}

fn fit_image(w: i32, h: i32, avail: i32) -> (i32, i32) {
    if w <= 0 || h <= 0 {
        return (0, 0);
    }
    if w <= avail {
        (w, h)
    } else {
        let nh = (h as i64 * avail as i64 / w as i64) as i32;
        (avail, nh.max(1))
    }
}

fn place_table(b: &mut Builder, tf: &TableFlow, measure: &Measure) {
    // 先干排一次拿高度（表格不跨页拆分）
    let (_, dry_end) = layout_table(tf, b.content_w, measure, b.left, 0);
    let table_h = dry_end;
    if b.y + table_h > b.bottom && !b.is_empty() {
        b.new_page();
    }
    let (table, end_y) = layout_table(tf, b.content_w, measure, b.left, b.y);
    b.cur.blocks.push(Block::Table(table));
    b.y = end_y;
}

fn place_image(b: &mut Builder, img: &PendingImage, image_map: &HashMap<String, usize>) {
    let Some(&index) = image_map.get(&img.rid) else {
        return;
    };
    let (w, h) = fit_image(img.w, img.h, b.content_w);
    if w <= 0 || h <= 0 {
        return;
    }
    if b.y + h > b.bottom && !b.is_empty() {
        b.new_page();
    }
    b.cur.blocks.push(Block::Image(ImageBlock {
        rect: Rect::new(b.left, b.y, w, h),
        image: index as u32,
        alt: img.alt.clone(),
    }));
    b.y += h;
}

/// 段落是否"只有一个分页符、没有可见文字"（python-docx 的 `add_page_break()` 就是这种独立段落）。
fn para_is_page_break_only(pf: &ParaFlow) -> bool {
    let mut has_break = false;
    for r in &pf.runs {
        for ch in r.text.chars() {
            match ch {
                '\u{0C}' => has_break = true,
                '\n' | '\r' | '\t' | ' ' => {}
                _ => return false,
            }
        }
    }
    has_break
}

fn layout_document(
    flows: &[Flow],
    setup: PageSetup,
    image_map: &HashMap<String, usize>,
    images: Vec<DocImage>,
    title: Option<String>,
    ctx: &Ctx,
    measure: &Measure,
) -> DocDoc {
    let page_w = setup.page_w;
    let page_h = setup.page_h;
    let ml = setup.margin_left;
    let mt = setup.margin_top;
    let mr = setup.margin_right;
    let mb = setup.margin_bottom;
    let content_w = (page_w as i32 - ml - mr).max(1);
    let bottom = page_h as i32 - mb;

    let mut b = Builder::new(mt, bottom, ml, content_w);
    let mut counters: HashMap<(u32, u32), i32> = HashMap::new();
    // 独立分页符段落（`<w:br w:type="page"/>` 单独成段）要先"存下来"，作用到下一个块。
    let mut pending_break = false;

    for flow in flows {
        if b.pages.len() >= MAX_PAGES {
            qlr_core::log_warn!(SCOPE, "页数超过 {MAX_PAGES}，截断");
            break;
        }
        match flow {
            Flow::Para(pf) => {
                if para_is_page_break_only(pf) {
                    pending_break = true;
                    continue;
                }
                place_para(&mut b, pf, ctx, &mut counters, measure, pending_break);
                pending_break = false;
            }
            Flow::Table(tf) => {
                if pending_break && !b.is_empty() {
                    b.new_page();
                }
                pending_break = false;
                place_table(&mut b, tf, measure);
            }
            Flow::Image(img) => {
                if pending_break && !b.is_empty() {
                    b.new_page();
                }
                pending_break = false;
                place_image(&mut b, img, image_map);
            }
        }
    }

    let pages = b.finish();
    DocDoc {
        pages,
        page_w,
        page_h,
        margin_left: ml,
        margin_top: mt,
        margin_right: mr,
        margin_bottom: mb,
        images,
        title,
    }
}

// ---------------------------------------------------------------------------
// 页眉/页脚
// ---------------------------------------------------------------------------

fn load_overlay(
    pkg: &mut Package,
    rid: Option<&str>,
    rels: &HashMap<String, String>,
    ctx: &Ctx,
) -> Vec<Flow> {
    let Some(rid) = rid else {
        return Vec::new();
    };
    let Some(part) = rels.get(rid) else {
        return Vec::new();
    };
    let Ok(Some(xml)) = pkg.read_optional(part) else {
        return Vec::new();
    };
    let (flows, _) = parse_flows(&xml, ctx);
    flows
}

fn attach_overlay(
    doc: &mut DocDoc,
    header_flows: &[Flow],
    footer_flows: &[Flow],
    measure: &Measure,
) {
    let content_w = doc.content_width();
    let mt = doc.margin_top;
    let mb = doc.margin_bottom;
    let page_h = doc.page_h as i32;

    let header_lines = build_overlay_lines(header_flows, content_w, doc.margin_left, 0, measure);
    let header_h = header_lines.last().map(|l| l.rect.bottom()).unwrap_or(0);
    let header_top = (mt - header_h - 2).max(2);
    let mut header_lines = header_lines;
    for line in &mut header_lines {
        line.rect.y += header_top;
    }

    let footer_lines = build_overlay_lines(footer_flows, content_w, doc.margin_left, 0, measure);
    let footer_h = footer_lines.last().map(|l| l.rect.bottom()).unwrap_or(0);
    let footer_top = {
        let base = page_h - mb + ((mb - footer_h) / 2).max(2);
        base.min(page_h - footer_h - 2).max(page_h - mb + 1)
    };
    let mut footer_lines = footer_lines;
    for line in &mut footer_lines {
        line.rect.y += footer_top;
    }

    for page in &mut doc.pages {
        page.header = header_lines.clone();
        page.footer = footer_lines.clone();
    }
}

fn build_overlay_lines(
    flows: &[Flow],
    avail: i32,
    left: i32,
    top: i32,
    measure: &Measure,
) -> Vec<Line> {
    let mut lines: Vec<Line> = Vec::new();
    let mut y = top;
    for flow in flows {
        if let Flow::Para(pf) = flow {
            let base_px = pf
                .runs
                .first()
                .map(|r| r.size_px())
                .unwrap_or_else(|| dpt_to_px(105));
            let wrapped = wrap_para(
                &pf.runs,
                avail,
                pf.line_mult,
                pf.line_fixed,
                base_px,
                measure,
            );
            for (_pbb, group_lines) in wrapped.groups {
                for wl in group_lines {
                    let h = wl.height;
                    lines.push(Line {
                        rect: Rect::new(left, y, avail, h),
                        runs: wl.runs,
                    });
                    y += h;
                }
            }
            y += pf.space_after;
        }
    }
    lines
}

// ---------------------------------------------------------------------------
// 图片解码
// ---------------------------------------------------------------------------

fn decode_image(bytes: &[u8], target_w: i32, target_h: i32) -> Option<DocImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.to_rgba8();
    let (sw, sh) = rgba.dimensions();
    if sw == 0 || sh == 0 {
        return None;
    }
    let (tw, th) = if target_w > 0 && target_h > 0 {
        (target_w as u32, target_h as u32)
    } else {
        (sw, sh)
    };
    if (tw as u64) * (th as u64) > MAX_IMAGE_PIXELS {
        return None;
    }
    let resized = if (tw, th) == (sw, sh) {
        rgba
    } else {
        image::imageops::resize(
            &rgba,
            tw.max(1),
            th.max(1),
            image::imageops::FilterType::Triangle,
        )
    };
    Some(DocImage {
        width: resized.width(),
        height: resized.height(),
        rgba: resized.into_raw(),
    })
}

fn collect_image_rids(flow: &Flow, out: &mut HashMap<String, usize>) {
    match flow {
        Flow::Image(img) => {
            out.entry(img.rid.clone()).or_insert(0);
        }
        Flow::Table(tf) => {
            for row in &tf.rows {
                for cell in &row.cells {
                    for b in &cell.blocks {
                        collect_image_rids(b, out);
                    }
                }
            }
        }
        Flow::Para(_) => {}
    }
}

fn collect_pending(flow: &Flow, out: &mut Vec<(String, i32, i32)>) {
    match flow {
        Flow::Image(img) => out.push((img.rid.clone(), img.w, img.h)),
        Flow::Table(tf) => {
            for row in &tf.rows {
                for cell in &row.cells {
                    for b in &cell.blocks {
                        collect_pending(b, out);
                    }
                }
            }
        }
        Flow::Para(_) => {}
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approx_measure_cjk_wider_than_ascii() {
        let cjk = approx_measure("中文测试", 105, false);
        let ascii = approx_measure("abcd", 105, false);
        assert!(cjk > ascii, "CJK {cjk} 应比 ASCII {ascii} 宽");
        // 4 个汉字约 4 个字号宽（14px * 4 = 56 上下）
        assert!((48..=64).contains(&cjk), "cjk={cjk}");
    }

    #[test]
    fn attrs_get_handles_prefixes_and_quotes() {
        let attrs = r#" w:val="both" w:left="720" r:id='rId3'"#;
        assert_eq!(attrs_get(attrs, "w:val"), Some("both"));
        assert_eq!(attrs_get(attrs, "val"), Some("both"));
        assert_eq!(attrs_get(attrs, "w:left"), Some("720"));
        assert_eq!(attrs_get(attrs, "r:id"), Some("rId3"));
        assert_eq!(attrs_get(attrs, "missing"), None);
    }

    #[test]
    fn xml_scan_yields_start_end_text() {
        let mut r = Reader::new("<w:p><w:t>你好</w:t></w:p>");
        match r.next() {
            Ev::Start { name, .. } => assert_eq!(local(name), "p"),
            _ => panic!("期望 start"),
        }
        match r.next() {
            Ev::Start { name, .. } => assert_eq!(local(name), "t"),
            _ => panic!("期望 start t"),
        }
        match r.next() {
            Ev::Text(t) => assert_eq!(t, "你好"),
            other => panic!("期望文本，得到 {:?}", matches!(other, Ev::Eof)),
        }
    }

    #[test]
    fn parse_simple_paragraph_then_layout() {
        let document = r#"<w:document xmlns:w="x"><w:body>
            <w:p><w:pPr><w:jc w:val="center"/></w:pPr>
              <w:r><w:rPr><w:b/></w:rPr><w:t>标题文字</w:t></w:r></w:p>
            <w:p><w:r><w:t>正文第一段。</w:t></w:r></w:p>
            <w:sectPr><w:pgSz w:w="11906" w:h="16838"/>
              <w:pgMar w:left="1440" w:right="1440" w:top="1440" w:bottom="1440"/></w:sectPr>
          </w:body></w:document>"#;
        let ctx = Ctx::empty();
        let (flows, sect) = parse_flows(document, &ctx);
        assert_eq!(flows.len(), 2, "应有两个段落");
        let sect = sect.expect("应有 sectPr");
        assert!(sect.setup.page_w > 700, "A4 宽 {}", sect.setup.page_w);
        let doc = layout_document(
            &flows,
            sect.setup,
            &HashMap::new(),
            Vec::new(),
            None,
            &Ctx::empty(),
            &approx_measure,
        );
        assert_eq!(doc.len(), 1, "单页内容");
        let page = doc.page(0).unwrap();
        assert!(!page.blocks.is_empty());
        // 第一个段落居中
        match &page.blocks[0] {
            Block::Para(p) => {
                assert_eq!(p.align, HAlign::Center);
                assert_eq!(p.lines[0].plain(), "标题文字");
                assert!(p.lines[0].runs[0].bold);
            }
            _ => panic!("期望段落"),
        }
    }

    #[test]
    fn long_text_paginates() {
        // 生成很多段落，强制跨页
        let mut body = String::from("<w:document><w:body>");
        for i in 0..200 {
            body.push_str(&format!(
                "<w:p><w:r><w:t>第{i}行内容，这里放一些中文让它占满一行宽度。</w:t></w:r></w:p>"
            ));
        }
        body.push_str("<w:sectPr><w:pgSz w:w=\"11906\" w:h=\"16838\"/><w:pgMar w:left=\"1440\" w:right=\"1440\" w:top=\"1440\" w:bottom=\"1440\"/></w:sectPr></w:body></w:document>");
        let ctx = Ctx::empty();
        let (flows, sect) = parse_flows(&body, &ctx);
        let doc = layout_document(
            &flows,
            sect.unwrap().setup,
            &HashMap::new(),
            Vec::new(),
            None,
            &Ctx::empty(),
            &approx_measure,
        );
        assert!(doc.len() > 1, "200 段应跨页，实际 {} 页", doc.len());
    }

    #[test]
    fn table_parses_and_lays_out() {
        let document = r#"<w:document><w:body>
            <w:tbl><w:tblPr><w:tblBorders/></w:tblPr>
              <w:tblGrid><w:gridCol w:w="2000"/><w:gridCol w:w="2000"/></w:tblGrid>
              <w:tr>
                <w:tc><w:p><w:r><w:t>A1</w:t></w:r></w:p></w:tc>
                <w:tc><w:p><w:r><w:t>B1</w:t></w:r></w:p></w:tc>
              </w:tr>
              <w:tr>
                <w:tc><w:p><w:r><w:t>A2</w:t></w:r></w:p></w:tc>
                <w:tc><w:p><w:r><w:t>B2</w:t></w:r></w:p></w:tc>
              </w:tr>
            </w:tbl>
            <w:sectPr><w:pgSz w:w="11906" w:h="16838"/></w:sectPr>
          </w:body></w:document>"#;
        let ctx = Ctx::empty();
        let (flows, sect) = parse_flows(document, &ctx);
        assert_eq!(flows.len(), 1);
        let doc = layout_document(
            &flows,
            sect.unwrap().setup,
            &HashMap::new(),
            Vec::new(),
            None,
            &Ctx::empty(),
            &approx_measure,
        );
        let page = doc.page(0).unwrap();
        match &page.blocks[0] {
            Block::Table(t) => {
                assert_eq!(t.rows.len(), 2);
                assert_eq!(t.rows[0].cells.len(), 2);
                assert!(t.borders);
                assert!(t.col_x.len() >= 3);
            }
            _ => panic!("期望表格"),
        }
    }

    #[test]
    fn styles_based_on_chain_resolves_size() {
        let styles = r#"<w:styles>
            <w:docDefaults><w:rPrDefault><w:rPr><w:sz w:val="21"/></w:rPr></w:rPrDefault></w:docDefaults>
            <w:style w:type="paragraph" w:styleId="Base"><w:rPr><w:sz w:val="24"/></w:rPr></w:style>
            <w:style w:type="paragraph" w:styleId="Child"><w:basedOn w:val="Base"/><w:rPr><w:b/></w:rPr></w:style>
          </w:styles>"#;
        let (default_run, default_ppr, defs) = parse_styles(styles);
        assert_eq!(default_run.size_dpt, 105); // 21 半磅 = 10.5pt
        let resolved = resolve_all(&defs, &default_run, &default_ppr);
        let (child_run, _) = resolved.get("Child").unwrap();
        assert_eq!(child_run.size_dpt, 120); // 继承 Base 的 24 半磅 = 12pt
        assert!(child_run.bold);
    }

    #[test]
    fn numbering_bullet_and_decimal() {
        let xml = r#"<w:numbering>
            <w:abstractNum w:abstractNumId="0">
              <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl>
            </w:abstractNum>
            <w:abstractNum w:abstractNumId="1">
              <w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/><w:lvlText w:val="•"/></w:lvl>
            </w:abstractNum>
            <w:num w:numId="10"><w:abstractNumId w:val="0"/></w:num>
            <w:num w:numId="11"><w:abstractNumId w:val="1"/></w:num>
          </w:numbering>"#;
        let numbering = parse_numbering(xml);
        let mut ctx = Ctx::empty();
        ctx.numbering = numbering;
        let mut counters = HashMap::new();
        assert_eq!(
            numbering_marker(10, 0, &ctx, &mut counters).as_deref(),
            Some("1. ")
        );
        assert_eq!(
            numbering_marker(10, 0, &ctx, &mut counters).as_deref(),
            Some("2. ")
        );
        assert_eq!(
            numbering_marker(11, 0, &ctx, &mut counters).as_deref(),
            Some("• ")
        );
    }

    #[test]
    fn emu_and_twips_helpers_via_document_setup() {
        assert_eq!(emu_to_px(914_400), 96);
        assert_eq!(twips_to_px(1440), 96);
    }
}
