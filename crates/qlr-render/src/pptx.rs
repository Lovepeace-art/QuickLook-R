//! 演示文稿（pptx）解析 + 排版：`文件 → qlr_core::pptx::PptxDoc`。
//!
//! 只做"看得懂幻灯片"的最低限度，够用来确认这是哪份 PPT、每页大概长什么样，
//! 而不追求和 PowerPoint 像素级一致（动画/切换/SmartArt/图表先不做，见文档"已知限制"）。
//!
//! 结构：
//! - `Package`：解 zip、按预算读条目（对齐 `sheet.rs`/`docx.rs` 的只手写 OOXML 路线）；
//! - `parse_xml`：把一段 XML 建成轻量树（`Node`），比事件流好写；
//! - 主题色（`clrScheme`）、占位符几何（从 slideLayout/slideMaster 继承）、
//!   组合变换（`p:grpSp` 的 chOff/chExt → off/ext 线性映射）；
//! - 文字：每段落按注入的 `measure` 折行，行矩形是**绝对**幻灯片坐标（绘制端照着画）。
//!
//! 这一层不依赖 `windows`，可在 Linux 上单测。

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use image::GenericImageView;

use qlr_core::docx::{DocImage, HAlign, ImageSlot, Line, Para, Rect, Run, dpt_to_px, emu_to_px};
use qlr_core::pptx::{
    Chart, ChartKind, ChartSeries, ChartShape, FillRect, Picture, PptxDoc, RectGeom, Shape, Slide,
    SlideSize, TableCell, TableRow, TableShape, TextBox,
};
use qlr_core::{Error, Result};

use crate::docx::{Measure, approx_measure};

/// 单条目解压上限（防止畸形包把内存撑爆）。
const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
/// 整包累计解压上限。
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
/// 幻灯片数上限。
const MAX_SLIDES: usize = 2048;
/// 每页元素数上限。
const MAX_SHAPES_PER_SLIDE: usize = 4096;
/// 图片数上限。
const MAX_IMAGES: usize = 1024;
/// 单张图片解码后的像素上限（超出等比缩小）。
const MAX_IMAGE_PIXELS: u64 = 40_000_000;
/// 单张图片显示尺寸上限（长边像素）。
const MAX_IMAGE_SIDE: u32 = 4000;

// ---------------------------------------------------------------- 包读取

struct Package {
    zip: zip::ZipArchive<std::fs::File>,
    used: u64,
}

impl Package {
    fn open(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)
            .map_err(|e| Error::msg(format!("打不开 pptx（{e}）：{}", short(path))))?;
        let zip = zip::ZipArchive::new(file).map_err(|_e| {
            Error::msg(format!(
                "不是有效的 pptx（…zip 容器打不开）：{}",
                short(path)
            ))
        })?;
        Ok(Self { zip, used: 0 })
    }

    fn read_raw(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
        let Some(real) = self.find_name(name) else {
            return Ok(None);
        };
        let mut entry = self
            .zip
            .by_name(&real)
            .map_err(|e| Error::msg(format!("读 {name} 失败：{e}")))?;
        let declared = entry.size();
        if declared > MAX_ENTRY_BYTES {
            return Err(Error::msg(format!(
                "pptx 条目过大（{declared} 字节 > 上限）：{name}"
            )));
        }
        self.used = self.used.saturating_add(declared);
        if self.used > MAX_TOTAL_BYTES {
            return Err(Error::msg("pptx 解压总量超上限".to_string()));
        }
        let cap = (declared as usize).min(4 * 1024 * 1024);
        let mut buf = Vec::with_capacity(cap);
        entry
            .by_ref()
            .take(MAX_ENTRY_BYTES + 1)
            .read_to_end(&mut buf)
            .map_err(|e| Error::msg(format!("读 {name} 失败：{e}")))?;
        Ok(Some(buf))
    }

    fn read_optional(&mut self, name: &str) -> Result<Option<String>> {
        Ok(self.read_raw(name)?.map(|b| decode_xml(&b)))
    }

    fn read_required(&mut self, name: &str) -> Result<String> {
        self.read_optional(name)?
            .ok_or_else(|| Error::msg(format!("pptx 缺少必需条目：{name}")))
    }

    /// 精确名或大小写不敏感地找条目（OOXML 里大小写偶有不一致）。
    fn find_name(&mut self, name: &str) -> Option<String> {
        if self.zip.index_for_name(name).is_some() {
            return Some(name.to_string());
        }
        let want = name.to_ascii_lowercase();
        for i in 0..self.zip.len() {
            if let Ok(entry) = self.zip.by_index_raw(i) {
                if entry.name().to_ascii_lowercase() == want {
                    return Some(entry.name().to_string());
                }
            }
        }
        None
    }
}

fn decode_xml(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

fn short(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "（无文件名）".to_string())
}

// ---------------------------------------------------------------- 轻量 XML 树

#[derive(Debug, Default)]
struct Node {
    name: String,
    attrs: String,
    kids: Vec<Node>,
    text: String,
}

impl Node {
    /// 去掉命名空间前缀后的本地名（`p:sp` → `sp`）。
    fn local(&self) -> &str {
        match self.name.rfind(':') {
            Some(i) => &self.name[i + 1..],
            None => &self.name,
        }
    }

    fn attr(&self, key: &str) -> Option<&str> {
        attr_lookup(&self.attrs, key)
    }

    fn attr_i64(&self, key: &str) -> Option<i64> {
        self.attr(key).and_then(|s| s.trim().parse().ok())
    }

    fn child(&self, local: &str) -> Option<&Node> {
        self.kids.iter().find(|k| k.local() == local)
    }

    fn children<'a>(&'a self, local: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.kids.iter().filter(move |k| k.local() == local)
    }
}

fn parse_xml(s: &str) -> Node {
    let b = s.as_bytes();
    let n = b.len();
    let mut stack: Vec<Node> = vec![Node {
        name: "#root".to_string(),
        ..Node::default()
    }];
    let mut i = 0usize;
    while i < n {
        if b[i] == b'<' {
            if s[i..].starts_with("<!--") {
                match s[i + 4..].find("-->") {
                    Some(j) => {
                        i = i + 4 + j + 3;
                        continue;
                    }
                    None => break,
                }
            }
            if s[i..].starts_with("<![CDATA[") {
                match s[i + 9..].find("]]>") {
                    Some(j) => {
                        if let Some(top) = stack.last_mut() {
                            top.text.push_str(&s[i + 9..i + 9 + j]);
                        }
                        i = i + 9 + j + 3;
                        continue;
                    }
                    None => break,
                }
            }
            if s[i..].starts_with("<?") {
                match s[i + 2..].find("?>") {
                    Some(j) => {
                        i = i + 2 + j + 2;
                        continue;
                    }
                    None => break,
                }
            }
            if s[i..].starts_with("<!") {
                match s[i + 2..].find('>') {
                    Some(j) => {
                        i = i + 2 + j + 1;
                        continue;
                    }
                    None => break,
                }
            }
            if s[i..].starts_with("</") {
                match s[i + 2..].find('>') {
                    Some(j) => {
                        i = i + 2 + j + 1;
                        if stack.len() > 1 {
                            let node = stack.pop().unwrap();
                            stack.last_mut().unwrap().kids.push(node);
                        }
                        continue;
                    }
                    None => break,
                }
            }
            // 开始标签：引号感知地找到 '>'
            let mut j = i + 1;
            let mut quote = 0u8;
            while j < n {
                let c = b[j];
                if quote != 0 {
                    if c == quote {
                        quote = 0;
                    }
                } else if c == b'"' || c == b'\'' {
                    quote = c;
                } else if c == b'>' {
                    break;
                }
                j += 1;
            }
            if j >= n {
                break;
            }
            let inner = &s[i + 1..j];
            let empty = inner.trim_end().ends_with('/');
            let inner2 = if empty {
                inner.trim_end().trim_end_matches('/')
            } else {
                inner
            };
            let (name, attrs) = match inner2.find(|c: char| c.is_whitespace()) {
                Some(k) => (inner2[..k].trim(), inner2[k..].trim().to_string()),
                None => (inner2.trim(), String::new()),
            };
            let node = Node {
                name: name.to_string(),
                attrs,
                kids: Vec::new(),
                text: String::new(),
            };
            i = j + 1;
            if empty {
                stack.last_mut().unwrap().kids.push(node);
            } else {
                stack.push(node);
            }
            continue;
        }
        let start = i;
        while i < n && b[i] != b'<' {
            i += 1;
        }
        if let Some(top) = stack.last_mut() {
            top.text.push_str(&decode_entities(&s[start..i]));
        }
    }
    while stack.len() > 1 {
        let node = stack.pop().unwrap();
        stack.last_mut().unwrap().kids.push(node);
    }
    stack.pop().unwrap()
}

fn attr_lookup<'a>(attrs: &'a str, key: &str) -> Option<&'a str> {
    let b = attrs.as_bytes();
    let n = b.len();
    let mut i = 0usize;
    // 精确名优先；同名（去命名空间前缀）作为兜底，且只在没有精确命中时采用。
    let mut local_hit: Option<&'a str> = None;
    while i < n {
        while i < n && (b[i] as char).is_whitespace() {
            i += 1;
        }
        let ks = i;
        while i < n && b[i] != b'=' && !(b[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        let name = &attrs[ks..i];
        while i < n && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i < n && b[i] == b'=' {
            i += 1;
        } else {
            continue;
        }
        while i < n && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        let q = b[i];
        let val;
        if q == b'"' || q == b'\'' {
            i += 1;
            let vs = i;
            while i < n && b[i] != q {
                i += 1;
            }
            val = &attrs[vs..i];
            i += 1;
        } else {
            let vs = i;
            while i < n && !(b[i] as char).is_whitespace() && b[i] != b'/' {
                i += 1;
            }
            val = &attrs[vs..i];
        }
        if name == key {
            return Some(val);
        }
        if local_hit.is_none() && local_of(name) == local_of(key) {
            local_hit = Some(val);
        }
    }
    local_hit
}

/// 去掉命名空间前缀（`r:id` → `id`）。
fn local_of(name: &str) -> &str {
    match name.rfind(':') {
        Some(i) => &name[i + 1..],
        None => name,
    }
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
        let Some(semi) = rest.find(';').filter(|&x| x <= 12) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..semi];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            other => other
                .strip_prefix("#x")
                .or_else(|| other.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| other.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
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

fn find_desc<'a>(node: &'a Node, local: &str) -> Option<&'a Node> {
    if node.local() == local {
        return Some(node);
    }
    for k in &node.kids {
        if let Some(found) = find_desc(k, local) {
            return Some(found);
        }
    }
    None
}

// ---------------------------------------------------------------- 关系 / 主题

struct Rel {
    ty: String,
    target: String,
}

fn parse_rels(xml: &str, base: &str) -> HashMap<String, Rel> {
    let mut map = HashMap::new();
    let root = parse_xml(xml);
    // parse_xml 返回的是合成 `#root`，真正的 `<Relationships>` 是它的子节点；
    // 直接在 #root 上找 `Relationship` 会落空（踩过：rels 全空 → 图片/占位符全丢）。
    let holder = root.child("Relationships").unwrap_or(&root);
    for r in holder.children("Relationship") {
        let (Some(id), Some(ty), Some(target)) = (r.attr("Id"), r.attr("Type"), r.attr("Target"))
        else {
            continue;
        };
        map.insert(
            id.to_string(),
            Rel {
                ty: ty.to_string(),
                target: join_part(base, target),
            },
        );
    }
    map
}

/// 把 rels 里的相对 `target` 解析成包内绝对路径（处理 `..`）。
fn join_part(base: &str, target: &str) -> String {
    if let Some(stripped) = target.strip_prefix('/') {
        return stripped.to_string();
    }
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

type Colors = HashMap<String, u32>;

fn parse_theme(node: &Node) -> Colors {
    let mut map = Colors::new();
    if let Some(scheme) = find_desc(node, "clrScheme") {
        for item in &scheme.kids {
            let key = item.local().to_string();
            let val = if let Some(s) = item.child("srgbClr") {
                s.attr("val").and_then(parse_hex)
            } else if let Some(s) = item.child("sysClr") {
                s.attr("lastClr").and_then(parse_hex)
            } else {
                None
            };
            if let Some(v) = val {
                map.insert(key, v);
            }
        }
    }
    map
}

fn parse_hex(s: &str) -> Option<u32> {
    let t = s.trim();
    if t.len() == 6 {
        u32::from_str_radix(t, 16).ok()
    } else if t.len() == 8 {
        // AARRGGBB：丢掉 alpha
        u32::from_str_radix(&t[2..], 16).ok()
    } else {
        None
    }
}

/// 解析一个颜色元素（`srgbClr` / `schemeClr` / `sysClr`）。
fn resolve_color(el: &Node, theme: &Colors) -> Option<u32> {
    match el.local() {
        "srgbClr" | "sysClr" => el.attr("val").or(el.attr("lastClr")).and_then(parse_hex),
        "schemeClr" => {
            let name = el.attr("val")?;
            let base = *theme.get(name)?;
            Some(apply_modifiers(base, el))
        }
        _ => None,
    }
}

/// 处理 `lumMod`/`lumOff`/`tint`/`shade` 这几个常见颜色修饰（近似）。
fn apply_modifiers(base: u32, el: &Node) -> u32 {
    let mut r = ((base >> 16) & 0xFF) as f32;
    let mut g = ((base >> 8) & 0xFF) as f32;
    let mut b = (base & 0xFF) as f32;
    let chan = |n: &Node| n.attr("val").and_then(|v| v.trim().parse::<i32>().ok());
    for ch in &el.kids {
        match ch.local() {
            "lumMod" => {
                if let Some(v) = chan(ch) {
                    let m = v as f32 / 100_000.0;
                    r *= m;
                    g *= m;
                    b *= m;
                }
            }
            "lumOff" => {
                if let Some(v) = chan(ch) {
                    let o = v as f32 / 100_000.0 * 255.0;
                    r += o;
                    g += o;
                    b += o;
                }
            }
            "tint" => {
                if let Some(v) = chan(ch) {
                    let t = v as f32 / 100_000.0;
                    r = r + (255.0 - r) * (1.0 - t);
                    g = g + (255.0 - g) * (1.0 - t);
                    b = b + (255.0 - b) * (1.0 - t);
                }
            }
            "shade" => {
                if let Some(v) = chan(ch) {
                    let t = v as f32 / 100_000.0;
                    r *= t;
                    g *= t;
                    b *= t;
                }
            }
            _ => {}
        }
    }
    let c = |v: f32| -> u32 { (v.round().clamp(0.0, 255.0)) as u32 };
    (c(r) << 16) | (c(g) << 8) | c(b)
}

/// 从容器（如 `spPr`）的 `solidFill`/`gradFill` 里取一个代表色。
fn fill_color(container: &Node, theme: &Colors) -> Option<u32> {
    if let Some(sf) = container.child("solidFill") {
        if let Some(color_el) = sf.kids.first() {
            return resolve_color(color_el, theme);
        }
    }
    if let Some(gf) = container.child("gradFill") {
        if let Some(first) = find_desc(gf, "gs") {
            if let Some(color_el) = first.kids.first() {
                return resolve_color(color_el, theme);
            }
        }
    }
    None
}

// ---------------------------------------------------------------- 几何变换

/// 仿射（仅缩放+平移，逐轴独立）：abs = a + e * k。
/// 用于把组合（`p:grpSp`）里子元素的 EMU 坐标映射到绝对 EMU。
#[derive(Clone, Copy)]
struct Xf {
    ax: f64,
    ay: f64,
    kx: f64,
    ky: f64,
}

impl Xf {
    fn identity() -> Self {
        Self {
            ax: 0.0,
            ay: 0.0,
            kx: 1.0,
            ky: 1.0,
        }
    }
    fn x(&self, e: f64) -> f64 {
        self.ax + e * self.kx
    }
    fn y(&self, e: f64) -> f64 {
        self.ay + e * self.ky
    }
}

fn read_off_ext(xfrm: &Node) -> Option<(i64, i64, i64, i64)> {
    let off = xfrm.child("off")?;
    let ext = xfrm.child("ext")?;
    Some((
        off.attr_i64("x")?,
        off.attr_i64("y")?,
        ext.attr_i64("cx")?,
        ext.attr_i64("cy")?,
    ))
}

/// 由元素里可能存在的 `xfrm`（`a:xfrm` / `p:xfrm`）计算像素矩形（可带组合变换）。
fn rect_from_xfrm(container: &Node, map: &Xf) -> Option<Rect> {
    let xf = container.child("xfrm")?;
    let (ox, oy, cx, cy) = read_off_ext(xf)?;
    Some(rect_from_off_ext(map, ox, oy, cx, cy))
}

fn rect_from_off_ext(map: &Xf, ox: i64, oy: i64, cx: i64, cy: i64) -> Rect {
    let x0 = emu_to_px(map.x(ox as f64) as i64);
    let y0 = emu_to_px(map.y(oy as f64) as i64);
    let x1 = emu_to_px(map.x((ox + cx) as f64) as i64);
    let y1 = emu_to_px(map.y((oy + cy) as f64) as i64);
    Rect::new(x0, y0, (x1 - x0).max(0), (y1 - y0).max(1))
}

/// 组合节点的子坐标变换（基于 `grpSpPr` 的 `xfrm` 的 chOff/chExt → off/ext）。
fn child_map(parent: &Node, map: &Xf) -> Xf {
    let Some(grp) = parent.child("grpSpPr") else {
        return *map;
    };
    let Some(xf) = grp.child("xfrm") else {
        return *map;
    };
    let Some((ox, oy, cx, cy)) = read_off_ext(xf) else {
        return *map;
    };
    let (chox, choy, chcx, chcy) = (
        xf.child("chOff").and_then(|n| n.attr_i64("x")),
        xf.child("chOff").and_then(|n| n.attr_i64("y")),
        xf.child("chExt").and_then(|n| n.attr_i64("cx")),
        xf.child("chExt").and_then(|n| n.attr_i64("cy")),
    );
    match (chox, choy, chcx, chcy) {
        (Some(chox), Some(choy), Some(chcx), Some(chcy)) if chcx > 0 && chcy > 0 => {
            let ratx = cx as f64 / chcx as f64;
            let raty = cy as f64 / chcy as f64;
            Xf {
                ax: map.ax + (ox as f64 - chox as f64 * ratx) * map.kx,
                ay: map.ay + (oy as f64 - choy as f64 * raty) * map.ky,
                kx: map.kx * ratx,
                ky: map.ky * raty,
            }
        }
        _ => *map,
    }
}

// ---------------------------------------------------------------- 占位符

/// 占位符几何表：`i:<idx>` 与 `t:<type>` 两种键。
struct Placeholders {
    by_idx: HashMap<String, Rect>,
    by_type: HashMap<String, Rect>,
}

impl Placeholders {
    fn empty() -> Self {
        Self {
            by_idx: HashMap::new(),
            by_type: HashMap::new(),
        }
    }

    fn get(&self, idx: Option<&str>, ty: Option<&str>) -> Option<Rect> {
        if let Some(i) = idx {
            if let Some(r) = self.by_idx.get(i) {
                return Some(*r);
            }
        }
        if let Some(t) = ty {
            if let Some(r) = self.by_type.get(t) {
                return Some(*r);
            }
        }
        None
    }

    fn absorb(&mut self, other: Placeholders) {
        // 后吸收的（layout）覆盖先吸收的（master）。
        for (k, v) in other.by_idx {
            self.by_idx.insert(k, v);
        }
        for (k, v) in other.by_type {
            self.by_type.insert(k, v);
        }
    }
}

/// 从 slideLayout / slideMaster 的 XML 里抽占位符矩形（顶层 sp，无组合变换），
/// 同时抽出**非占位符的装饰图形**（垫在幻灯片内容之下的母版装饰），以及背景色。
fn parse_placeholders(xml: &str, theme: &Colors) -> (Placeholders, Option<u32>, Vec<Shape>) {
    let mut ph = Placeholders::empty();
    let root = parse_xml(xml);
    let bg = parse_bg(&root, theme);
    let mut decor: Vec<Shape> = Vec::new();
    let Some(tree) = find_desc(&root, "spTree") else {
        return (ph, bg, decor);
    };
    let map = Xf::identity();
    let empty_rels: HashMap<String, Rel> = HashMap::new();
    let empty_ph = Placeholders::empty();
    let ctx = SlideCtx {
        slide_rels: &empty_rels,
        theme,
        ph: &empty_ph,
    };
    for sp in tree.children("sp") {
        match find_desc(sp, "ph") {
            Some(ph_node) => {
                let Some(rect) = rect_from_xfrm(sp.child("spPr").unwrap_or(sp), &map) else {
                    continue;
                };
                if let Some(idx) = ph_node.attr("idx") {
                    ph.by_idx.insert(idx.to_string(), rect);
                }
                if let Some(ty) = ph_node.attr("type") {
                    ph.by_type.insert(ty.to_string(), rect);
                }
            }
            None => {
                // 非占位符：母版/版式上的装饰图形，垫在幻灯片内容之下。
                if decor.len() < MAX_SHAPES_PER_SLIDE {
                    if let Some(s) = parse_sp(sp, &ctx, &map) {
                        decor.push(s);
                    }
                }
            }
        }
    }
    (ph, bg, decor)
}

/// 取 `p:bg` 的背景色：`bgPr` 的 solidFill，或 gradFill 各色标的平均色（近似为纯色）；`bgRef` 交给外层再试。
fn parse_bg(root: &Node, theme: &Colors) -> Option<u32> {
    let bg = find_desc(root, "bg")?;
    if let Some(bg_pr) = bg.child("bgPr") {
        if let Some(sf) = bg_pr.child("solidFill") {
            if let Some(el) = sf.kids.first() {
                return resolve_color(el, theme);
            }
        }
        // 渐变背景：多色标平均成一种代表色垫底（真正的多段渐变后续再做）。
        if let Some(gf) = bg_pr.child("gradFill") {
            if let Some(lst) = find_desc(gf, "gsLst") {
                let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
                for gs in lst.children("gs") {
                    if let Some(el) = gs.kids.first() {
                        if let Some(c) = resolve_color(el, theme) {
                            r += u64::from((c >> 16) & 0xFF);
                            g += u64::from((c >> 8) & 0xFF);
                            b += u64::from(c & 0xFF);
                            n += 1;
                        }
                    }
                }
                if let (Some(rr), Some(gg), Some(bb)) =
                    (r.checked_div(n), g.checked_div(n), b.checked_div(n))
                {
                    return Some(((rr as u32) << 16) | ((gg as u32) << 8) | (bb as u32));
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------- 文字排版

/// 默认内边距（EMU，≈9.6px）。
const DEFAULT_INSET_EMU: i64 = 91_440;
/// 默认正文字号（0.1pt）；18pt = 180。注意 OOXML 的 `sz` 是「百分之一磅」，
/// 18pt 存的是 1800，换算成本模型的 0.1pt 单位是 sz/10=180，不能直接拿 1800。
const DEFAULT_SIZE_DPT: u16 = 180;

fn is_cjk(ch: char) -> bool {
    let c = ch as u32;
    (0x2E80..=0x9FFF).contains(&c)
        || (0xF900..=0xFAFF).contains(&c)
        || (0xFF00..=0xFFEF).contains(&c)
        || (0x3000..=0x303F).contains(&c)
}

/// 把 `txBody` 里的段落折行成 `Vec<Para>`（行矩形绝对坐标）。
fn layout_text(
    tx_body: &Node,
    rect: Rect,
    anchor: Option<&str>,
    theme: &Colors,
    measure: &Measure,
) -> Vec<Para> {
    let body_pr = tx_body.child("bodyPr");
    let inset = |k: &str| {
        body_pr
            .and_then(|b| b.attr(k))
            .and_then(|v| v.parse::<i64>().ok())
            .map(emu_to_px)
            .unwrap_or(emu_to_px(DEFAULT_INSET_EMU))
    };
    let l_ins = inset("lIns");
    let t_ins = inset("tIns");
    let r_ins = inset("rIns");
    let b_ins = inset("bIns");
    let avail = (rect.w - l_ins - r_ins).max(1);

    let mut paras: Vec<Para> = Vec::new();
    let mut y = rect.y + t_ins;
    for p in tx_body.children("p") {
        let align = p
            .child("pPr")
            .and_then(|pp| pp.attr("algn"))
            .map(map_align)
            .unwrap_or(HAlign::Left);
        let mut runs: Vec<Run> = Vec::new();
        for r in p
            .kids
            .iter()
            .filter(|k| k.local() == "r" || k.local() == "fld")
        {
            let rpr = r.child("rPr");
            let text: String = r
                .kids
                .iter()
                .filter(|k| k.local() == "t")
                .map(|t| t.text.clone())
                .collect();
            if text.is_empty() {
                continue;
            }
            runs.push(build_run(&text, rpr, theme));
        }
        // 行高：本段最大字号 * 1.2，至少 12px。
        let line_h = runs
            .iter()
            .map(|r| dpt_to_px(r.size_dpt))
            .max()
            .unwrap_or(dpt_to_px(DEFAULT_SIZE_DPT))
            .max(8);
        let line_h = (line_h as f32 * 1.2).round() as i32;
        let mut lines = wrap_runs(&runs, avail, line_h, measure);
        if lines.is_empty() {
            lines.push(Vec::new());
        }
        let mut para_lines = Vec::new();
        for line_runs in lines {
            para_lines.push(Line {
                rect: Rect::new(rect.x + l_ins, y, avail, line_h),
                runs: merge_runs(line_runs),
            });
            y += line_h;
        }
        let para_rect = Rect::new(
            rect.x + l_ins,
            para_lines.first().map(|l| l.rect.y).unwrap_or(y),
            avail,
            para_lines.iter().map(|l| l.rect.h).sum(),
        );
        paras.push(Para {
            rect: para_rect,
            align,
            lines: para_lines,
            space_before: 0,
            space_after: 0,
        });
    }

    // 垂直锚点：ctr 居中，b 贴底。
    let content_h = (rect.h - t_ins - b_ins).max(1);
    let total: i32 = paras.iter().map(|p| p.rect.h).sum();
    let shift = match anchor {
        Some("ctr") => (content_h - total) / 2,
        Some("b") => content_h - total,
        _ => 0,
    };
    if shift > 0 {
        for para in &mut paras {
            para.rect.y += shift;
            for line in &mut para.lines {
                line.rect.y += shift;
            }
        }
    }
    paras
}

fn map_align(algn: &str) -> HAlign {
    match algn.trim() {
        "ctr" => HAlign::Center,
        "r" => HAlign::Right,
        "just" | "dist" => HAlign::Justify,
        _ => HAlign::Left,
    }
}

fn build_run(text: &str, rpr: Option<&Node>, theme: &Colors) -> Run {
    let mut run = Run {
        text: text.to_string(),
        ..Run::default()
    };
    if let Some(rpr) = rpr {
        if let Some(sz) = rpr.attr_i64("sz") {
            run.size_dpt = (sz / 10).clamp(1, 1600) as u16;
        } else {
            run.size_dpt = DEFAULT_SIZE_DPT;
        }
        run.bold = truthy(rpr.attr("b"));
        run.italic = truthy(rpr.attr("i"));
        run.underline = rpr.attr("u").is_some_and(|u| u != "none");
        run.strike = truthy(rpr.attr("strike"));
        if let Some(color) = fill_color(rpr, theme) {
            run.color = color;
        }
    } else {
        run.size_dpt = DEFAULT_SIZE_DPT;
    }
    run
}

fn truthy(v: Option<&str>) -> bool {
    matches!(v, Some("1") | Some("true") | Some("on"))
}

/// 段落内按词/CJK 字符折行。返回每行的 run 列表（未合并）。
fn wrap_runs(runs: &[Run], avail: i32, line_h: i32, measure: &Measure) -> Vec<Vec<Run>> {
    // 先切成 token：CJK 单字、英文单词、空格。
    struct Tok {
        text: String,
        run: Run,
    }
    let mut toks: Vec<Tok> = Vec::new();
    for run in runs {
        let mut word = String::new();
        let flush = |word: &mut String, toks: &mut Vec<Tok>| {
            if !word.is_empty() {
                toks.push(Tok {
                    text: std::mem::take(word),
                    run: run.clone(),
                });
            }
        };
        for ch in run.text.chars() {
            if ch == ' ' {
                flush(&mut word, &mut toks);
                toks.push(Tok {
                    text: " ".to_string(),
                    run: run.clone(),
                });
            } else if is_cjk(ch) {
                flush(&mut word, &mut toks);
                toks.push(Tok {
                    text: ch.to_string(),
                    run: run.clone(),
                });
            } else {
                word.push(ch);
            }
        }
        flush(&mut word, &mut toks);
    }

    let mut lines: Vec<Vec<Run>> = Vec::new();
    let mut cur: Vec<Run> = Vec::new();
    let mut cur_w = 0i32;
    for tok in toks {
        let w = measure(&tok.text, tok.run.size_dpt, tok.run.bold).max(1);
        let is_space = tok.text == " ";
        if cur_w + w > avail && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
            if is_space {
                continue; // 换行后丢掉行首空格
            }
        }
        cur.push(Run {
            text: tok.text,
            ..tok.run
        });
        cur_w += w;
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    // 空段落仍给一行（有 line_h 的空白）。
    let _ = line_h;
    lines
}

fn merge_runs(runs: Vec<Run>) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    for r in runs {
        if let Some(last) = out.last_mut() {
            if same_style(last, &r) {
                last.text.push_str(&r.text);
                continue;
            }
        }
        out.push(r);
    }
    out
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

// ---------------------------------------------------------------- 元素解析

struct SlideCtx<'a> {
    slide_rels: &'a HashMap<String, Rel>,
    theme: &'a Colors,
    ph: &'a Placeholders,
}

fn parse_sp_tree(
    tree: &Node,
    ctx: &SlideCtx,
    map: &Xf,
    images: &mut Vec<ImageSlot>,
    cache: &mut HashMap<String, u32>,
    pkg: &mut Package,
) -> Vec<Shape> {
    let mut shapes = Vec::new();
    for child in &tree.kids {
        if shapes.len() >= MAX_SHAPES_PER_SLIDE {
            break;
        }
        match child.local() {
            "sp" => {
                if let Some(s) = parse_sp(child, ctx, map) {
                    shapes.push(s);
                }
            }
            "pic" => {
                if let Some(s) = parse_pic(child, ctx, map, images, cache, pkg) {
                    shapes.push(s);
                }
            }
            "graphicFrame" => {
                if let Some(s) = parse_graphic_frame(child, ctx, map, pkg) {
                    shapes.push(s);
                }
            }
            "grpSp" => {
                let cmap = child_map(child, map);
                shapes.extend(parse_sp_tree(child, ctx, &cmap, images, cache, pkg));
            }
            "cxnSp" => {
                if let Some(s) = parse_cxn(child, ctx, map) {
                    shapes.push(s);
                }
            }
            _ => {}
        }
    }
    shapes
}

/// 从 `spPr` 的 `a:prstGeom@prst` 判断形状几何；未知一律当矩形。
fn rect_geom(sp_pr: &Node) -> RectGeom {
    let prst = find_desc(sp_pr, "prstGeom")
        .and_then(|n| n.attr("prst"))
        .unwrap_or("");
    match prst {
        "roundRect" => RectGeom::RoundRect,
        "ellipse" | "oval" => RectGeom::Ellipse,
        _ => RectGeom::Rect,
    }
}

/// 从 `spPr/a:xfrm@rot`（1/60000 度）取整数旋转角。
fn xfrm_rot(sp_pr: &Node) -> i32 {
    sp_pr
        .child("xfrm")
        .and_then(|x| x.attr_i64("rot"))
        .map(|r| (r as f64 / 60_000.0).round() as i32)
        .unwrap_or(0)
}

fn parse_sp(node: &Node, ctx: &SlideCtx, map: &Xf) -> Option<Shape> {
    let sp_pr = node.child("spPr");
    let ph_node = find_desc(node, "ph");
    let idx = ph_node.and_then(|p| p.attr("idx"));
    let ty = ph_node.and_then(|p| p.attr("type"));
    let rect = sp_pr
        .and_then(|p| rect_from_xfrm(p, map))
        .or_else(|| ctx.ph.get(idx, ty))
        .unwrap_or_else(|| Rect::new(0, 0, 1, 1));

    let fill = sp_pr.and_then(|p| fill_color(p, ctx.theme));
    let line = sp_pr
        .and_then(|p| p.child("ln"))
        .and_then(|ln| fill_color(ln, ctx.theme));

    if let Some(tx) = node.child("txBody") {
        let anchor = tx.child("bodyPr").and_then(|b| b.attr("anchor"));
        let paras = layout_text(tx, rect, anchor, ctx.theme, &approx_measure);
        let has_text = paras.iter().any(|p| p.lines.iter().any(|l| !l.is_empty()));
        if has_text {
            return Some(Shape::Text(TextBox {
                rect,
                paras,
                fill,
                line,
            }));
        }
    }
    if fill.is_some() || line.is_some() {
        let geom = sp_pr.map(rect_geom).unwrap_or_default();
        let rot_deg = sp_pr.map(xfrm_rot).unwrap_or(0);
        return Some(Shape::Rect(FillRect {
            rect,
            fill,
            line,
            geom,
            rot_deg,
        }));
    }
    None
}

fn parse_cxn(node: &Node, ctx: &SlideCtx, map: &Xf) -> Option<Shape> {
    let sp_pr = node.child("spPr")?;
    let mut rect = rect_from_xfrm(sp_pr, map)?;
    if rect.h < 2 {
        rect.h = 2;
    }
    let line = sp_pr.child("ln").and_then(|ln| fill_color(ln, ctx.theme));
    Some(Shape::Rect(FillRect {
        rect,
        fill: None,
        line,
        geom: rect_geom(sp_pr),
        rot_deg: xfrm_rot(sp_pr),
    }))
}

fn parse_pic(
    node: &Node,
    ctx: &SlideCtx,
    map: &Xf,
    images: &mut Vec<ImageSlot>,
    cache: &mut HashMap<String, u32>,
    _pkg: &mut Package,
) -> Option<Shape> {
    let sp_pr = node.child("spPr")?;
    // 少数没有 xfrm 的图片就没有几何，直接跳过（不猜位置，避免画到页外）。
    let rect = rect_from_xfrm(sp_pr, map)?;
    let blip = find_desc(node, "blip")?;
    let rid = blip.attr("embed").or_else(|| blip.attr("link"))?;
    let rel = ctx.slide_rels.get(rid)?;
    let alt = find_desc(node, "cNvPr")
        .and_then(|n| n.attr("descr"))
        .unwrap_or("")
        .to_string();

    let index = if let Some(&i) = cache.get(&rel.target) {
        i
    } else {
        if images.len() >= MAX_IMAGES {
            return None;
        }
        let i = images.len() as u32;
        // 只记条目名 + 显示尺寸；读字节/解码推迟到该张首次绘制。
        images.push(ImageSlot::new(
            rect.w.max(1) as u32,
            rect.h.max(1) as u32,
            rel.target.clone(),
        ));
        cache.insert(rel.target.clone(), i);
        i
    };
    Some(Shape::Picture(Picture {
        rect,
        image: index,
        alt,
    }))
}

fn parse_graphic_frame(node: &Node, ctx: &SlideCtx, map: &Xf, pkg: &mut Package) -> Option<Shape> {
    // graphicFrame 的 xfrm 可能是 `p:xfrm` / `a:xfrm`，直接子节点名为 xfrm。
    let rect = node
        .child("xfrm")
        .and_then(read_off_ext)
        .map(|(ox, oy, cx, cy)| rect_from_off_ext(map, ox, oy, cx, cy))?;
    // 不是表格时看看是不是图表：graphicData 里引用 `c:chart r:id`。
    if find_desc(node, "tbl").is_none() {
        if let Some(chart_el) = find_desc(node, "chart") {
            let chart = chart_el
                .attr("r:id")
                .and_then(|rid| ctx.slide_rels.get(rid))
                .and_then(|rel| pkg.read_optional(&rel.target).ok().flatten())
                .and_then(|xml| parse_chart(&xml, ctx.theme));
            return chart.map(|chart| Shape::Chart(ChartShape { rect, chart }));
        }
    }
    let tbl = find_desc(node, "tbl")?;

    let mut col_emu: Vec<i64> = Vec::new();
    if let Some(grid) = tbl.child("tblGrid") {
        for gc in grid.children("gridCol") {
            col_emu.push(gc.attr_i64("w").unwrap_or(0));
        }
    }
    if col_emu.is_empty() {
        col_emu.push(rect.w as i64);
    }
    let mut col_x: Vec<i32> = Vec::with_capacity(col_emu.len() + 1);
    let mut acc = 0i32;
    col_x.push(0);
    for w in &col_emu {
        acc += emu_to_px(*w).max(1);
        col_x.push(acc);
    }
    let total_w: i32 = col_x.last().copied().unwrap_or(rect.w);
    let col_scale = rect.w as f64 / total_w.max(1) as f64;
    for x in col_x.iter_mut() {
        *x = (*x as f64 * col_scale).round() as i32;
    }

    let trs: Vec<&Node> = tbl.children("tr").collect();
    let row_h = if trs.is_empty() {
        0
    } else {
        rect.h / trs.len() as i32
    };
    let mut rows = Vec::with_capacity(trs.len());
    let mut y = rect.y;
    for tr in trs {
        let mut cells = Vec::new();
        let mut col = 0usize;
        for tc in tr.children("tc") {
            let span = tc
                .child("tcPr")
                .and_then(|p| p.attr_i64("gridSpan"))
                .unwrap_or(1)
                .max(1) as usize;
            let x0 = col_x.get(col).copied().unwrap_or(0);
            let x1 = col_x
                .get((col + span).min(col_x.len() - 1))
                .copied()
                .unwrap_or(total_w);
            let cell_rect = Rect::new(rect.x + x0, y, (x1 - x0).max(1), row_h.max(1));
            let fill = tc.child("tcPr").and_then(|p| fill_color(p, ctx.theme));
            let paras = match tc.child("txBody") {
                Some(tx) => layout_text(tx, cell_rect, Some("t"), ctx.theme, &approx_measure),
                None => Vec::new(),
            };
            cells.push(TableCell {
                rect: cell_rect,
                paras,
                fill,
            });
            col += span;
        }
        rows.push(TableRow {
            rect: Rect::new(rect.x, y, rect.w, row_h.max(1)),
            cells,
        });
        y += row_h.max(1);
    }
    Some(Shape::Table(TableShape {
        rect,
        col_x,
        rows,
        borders: true,
    }))
}

// ---------------------------------------------------------------- 图表解析

/// 图表系列主色：`spPr` 里第一个 `srgbClr`/`schemeClr`（用主题色表解析）。
fn chart_series_color(node: &Node, theme: &Colors) -> Option<u32> {
    fn walk(n: &Node, theme: &Colors) -> Option<u32> {
        match n.local() {
            "srgbClr" => {
                if let Some(c) = n.attr("val").and_then(parse_hex) {
                    return Some(c & 0x00FF_FFFF);
                }
            }
            "schemeClr" => {
                if let Some(c) = n.attr("val").and_then(|v| theme.get(v)) {
                    return Some(*c & 0x00FF_FFFF);
                }
            }
            _ => {}
        }
        for k in &n.kids {
            if let Some(c) = walk(k, theme) {
                return Some(c);
            }
        }
        None
    }
    walk(node, theme)
}

/// 内置系列调色板（主题里取不到时的兜底）。
fn chart_palette(i: usize) -> u32 {
    const PAL: [u32; 8] = [
        0x4472C4, 0xED7D31, 0xA5A5A5, 0xFFC000, 0x5B9BD5, 0x70AD47, 0x264478, 0x9E480E,
    ];
    PAL[i % PAL.len()]
}

/// 收集一个 `c:pt` 容器的全部 `<c:pt idx=..><c:v>..` 并按 idx 排序。
fn read_pts(node: &Node) -> Vec<(i64, String)> {
    let cache = find_desc(node, "strCache")
        .or_else(|| find_desc(node, "numCache"))
        .or_else(|| find_desc(node, "strLit"))
        .or_else(|| find_desc(node, "numLit"))
        .unwrap_or(node);
    let mut out: Vec<(i64, String)> = Vec::new();
    collect_pts(cache, &mut out);
    out.sort_by_key(|(i, _)| *i);
    out
}

fn collect_pts(node: &Node, out: &mut Vec<(i64, String)>) {
    for k in &node.kids {
        if k.local() == "pt" {
            let idx = k.attr_i64("idx").unwrap_or(out.len() as i64);
            let v = k
                .child("v")
                .map(|v| v.text.trim().to_string())
                .unwrap_or_default();
            out.push((idx, v));
        } else {
            collect_pts(k, out);
        }
    }
}

fn chart_title(root: &Node) -> String {
    let Some(title) = find_desc(root, "title") else {
        return String::new();
    };
    let mut out = String::new();
    if let Some(rich) = find_desc(title, "rich") {
        collect_all_text(rich, &mut out);
    }
    if out.trim().is_empty() {
        if let Some(tx) = title.child("tx") {
            if let Some(v) = find_desc(tx, "v") {
                out.push_str(v.text.trim());
            }
        }
    }
    out.trim().to_string()
}

fn collect_all_text(node: &Node, out: &mut String) {
    for k in &node.kids {
        if k.local() == "t" {
            out.push_str(&k.text);
        } else {
            collect_all_text(k, out);
        }
    }
}

/// 把 `ppt/charts/chartN.xml` 解析成可绘制的 [`Chart`]（只支持柱/折线/饼）。
fn parse_chart(xml: &str, theme: &Colors) -> Option<Chart> {
    let root = parse_xml(xml);
    let (kind, plot) = if let Some(b) = find_desc(&root, "barChart") {
        let grouping = b
            .child("grouping")
            .and_then(|g| g.attr("val"))
            .unwrap_or("clustered");
        let kind = match grouping {
            "stacked" => ChartKind::BarStacked,
            "percentStacked" => ChartKind::BarPercent,
            _ => ChartKind::Bar,
        };
        (kind, b)
    } else if let Some(l) = find_desc(&root, "lineChart") {
        (ChartKind::Line, l)
    } else {
        let p = find_desc(&root, "pieChart")?;
        (ChartKind::Pie, p)
    };

    let title = chart_title(&root);
    let mut number_format = String::new();
    let mut categories: Vec<String> = Vec::new();
    let mut series: Vec<ChartSeries> = Vec::new();
    for (i, ser) in plot.children("ser").enumerate() {
        if series.len() >= 8 {
            break;
        }
        let name = find_desc(ser, "tx")
            .and_then(|t| find_desc(t, "v"))
            .map(|v| v.text.trim().to_string())
            .unwrap_or_default();
        if categories.is_empty() {
            if let Some(cat) = ser.child("cat") {
                categories = read_pts(cat).into_iter().map(|(_, v)| v).collect();
            }
        }
        let val_node = ser.child("val");
        let values: Vec<f64> = val_node
            .map(read_pts)
            .map(|pts| {
                pts.into_iter()
                    .filter_map(|(_, v)| v.parse::<f64>().ok())
                    .collect()
            })
            .unwrap_or_default();
        if number_format.is_empty() {
            if let Some(val) = val_node {
                number_format = find_desc(val, "formatCode")
                    .map(|f| f.text.trim().to_string())
                    .unwrap_or_default();
            }
        }
        if values.is_empty() {
            continue;
        }
        let color = ser
            .child("spPr")
            .and_then(|sp| chart_series_color(sp, theme))
            .unwrap_or_else(|| chart_palette(i));
        series.push(ChartSeries {
            name,
            values,
            color,
        });
    }
    if series.is_empty() {
        return None;
    }
    let max_len = series.iter().map(|s| s.values.len()).max().unwrap_or(0);
    while categories.len() < max_len {
        categories.push((categories.len() + 1).to_string());
    }
    Some(Chart {
        kind,
        title,
        categories,
        series,
        number_format,
    })
}

// ---------------------------------------------------------------- 图片解码

fn decode_image(bytes: &[u8], want_w: u32, want_h: u32) -> Option<DocImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let (sw, sh) = img.dimensions();
    if sw == 0 || sh == 0 {
        return None;
    }
    let mut dw = want_w.clamp(1, MAX_IMAGE_SIDE);
    let mut dh = want_h.clamp(1, MAX_IMAGE_SIDE);
    while u64::from(dw) * u64::from(dh) > MAX_IMAGE_PIXELS {
        dw = (dw / 2).max(1);
        dh = (dh / 2).max(1);
    }
    let rgba = if (dw, dh) == (sw, sh) {
        img.to_rgba8()
    } else {
        image::imageops::resize(
            &img.to_rgba8(),
            dw,
            dh,
            image::imageops::FilterType::Triangle,
        )
    };
    Some(DocImage {
        width: dw,
        height: dh,
        rgba: rgba.into_raw(),
    })
}

/// 惰性解码单张图片（按需从源 pptx 重开 zip 读取该条目并解码，缓存回槽）。
pub fn decode_slot(doc: &PptxDoc, index: usize) -> Option<()> {
    let slot = doc.images.get(index)?;
    if slot.is_decoded() {
        return Some(());
    }
    let src = doc.source.as_deref()?;
    let mut pkg = Package::open(src).ok()?;
    let bytes = pkg.read_raw(&slot.part).ok().flatten()?;
    let img = decode_image(&bytes, slot.disp_w, slot.disp_h)?;
    let _ = slot.set_decoded(img);
    Some(())
}

/// 确保某一幻灯片用到的图片都已解码（绘制该张前由应用调用；一张只开一次 zip）。
pub fn ensure_slide_images(doc: &PptxDoc, slide: usize) {
    let Some(s) = doc.slides.get(slide) else {
        return;
    };
    let mut idxs: Vec<usize> = Vec::new();
    for shape in &s.shapes {
        if let Shape::Picture(pic) = shape {
            idxs.push(pic.image as usize);
        }
    }
    if idxs.is_empty() {
        return;
    }
    let mut pkg = doc.source.as_deref().and_then(|p| Package::open(p).ok());
    for index in idxs {
        let Some(slot) = doc.images.get(index) else {
            continue;
        };
        if slot.is_decoded() {
            continue;
        }
        if let Some(pkg) = pkg.as_mut() {
            if let Ok(Some(bytes)) = pkg.read_raw(&slot.part) {
                if let Some(img) = decode_image(&bytes, slot.disp_w, slot.disp_h) {
                    let _ = slot.set_decoded(img);
                }
            }
        }
    }
}

// ---------------------------------------------------------------- 顶层入口

/// 解析整份演示文稿。
pub fn parse_pptx(path: &Path) -> Result<PptxDoc> {
    let mut pkg = Package::open(path)?;
    let pres_xml = pkg.read_required("ppt/presentation.xml")?;
    let pres = parse_xml(&pres_xml);

    let (width, height) = presentation_size(&pres);

    let prels = pkg
        .read_optional("ppt/_rels/presentation.xml.rels")?
        .map(|x| parse_rels(&x, "ppt"))
        .unwrap_or_default();

    let theme = prels
        .values()
        .find(|r| r.ty.ends_with("/theme"))
        .and_then(|r| pkg.read_optional(&r.target).ok().flatten())
        .map(|xml| parse_theme(&parse_xml(&xml)))
        .unwrap_or_default();

    // 幻灯片顺序：sldIdLst 里的 r:id 顺序即放映顺序。
    let mut slide_parts: Vec<String> = Vec::new();
    if let Some(lst) = find_desc(&pres, "sldIdLst") {
        for sid in lst.children("sldId") {
            if let Some(rid) = sid.attr("r:id") {
                if let Some(rel) = prels.get(rid) {
                    slide_parts.push(rel.target.clone());
                }
            }
        }
    }
    if slide_parts.is_empty() {
        // 兜底：按 ppt/slides/slideN.xml 数字顺序。
        for i in 1..=MAX_SLIDES {
            let name = format!("ppt/slides/slide{i}.xml");
            if pkg.find_name(&name).is_some() {
                slide_parts.push(name);
            } else {
                break;
            }
        }
    }

    let mut images: Vec<ImageSlot> = Vec::new();
    let mut img_cache: HashMap<String, u32> = HashMap::new();
    let mut slides: Vec<Slide> = Vec::new();
    for (index, part) in slide_parts.iter().enumerate().take(MAX_SLIDES) {
        let Some(xml) = pkg.read_optional(part)? else {
            continue;
        };
        let root = parse_xml(&xml);
        let slide_rels = read_slide_rels(&mut pkg, part)?;
        let (ph, bg, decor) = resolve_placeholders(&mut pkg, &slide_rels, &theme);

        // 母版/版式装饰先入队（垫底），幻灯片自身图形叠在其上（z 顺序=数组顺序）。
        let mut shapes: Vec<Shape> = decor;
        if let Some(tree) = find_desc(&root, "spTree") {
            let ctx = SlideCtx {
                slide_rels: &slide_rels,
                theme: &theme,
                ph: &ph,
            };
            let map = Xf::identity();
            shapes.extend(parse_sp_tree(
                tree,
                &ctx,
                &map,
                &mut images,
                &mut img_cache,
                &mut pkg,
            ));
        }
        // 背景：slide 自身优先，其次 layout/master（resolve_placeholders 已回调 bg）。
        let slide_bg = parse_bg(&root, &theme).or(bg);
        slides.push(Slide {
            shapes,
            bg: slide_bg,
            number: (index + 1) as u32,
        });
    }

    let title = pkg
        .read_optional("docProps/core.xml")?
        .map(|xml| extract_core_title(&xml))
        .filter(|t| !t.is_empty());

    Ok(PptxDoc {
        slides,
        width,
        height,
        images,
        source: Some(path.to_path_buf()),
        title,
    })
}

fn presentation_size(pres: &Node) -> (u32, u32) {
    if let Some(sz) = find_desc(pres, "sldSz") {
        let cx = sz.attr_i64("cx").unwrap_or(0);
        let cy = sz.attr_i64("cy").unwrap_or(0);
        if cx > 0 && cy > 0 {
            return (emu_to_px(cx) as u32, emu_to_px(cy) as u32);
        }
    }
    (1280, 720)
}

fn read_slide_rels(pkg: &mut Package, slide_part: &str) -> Result<HashMap<String, Rel>> {
    // ppt/slides/slideN.xml → ppt/slides/_rels/slideN.xml.rels
    let (dir, file) = match slide_part.rsplit_once('/') {
        Some((d, f)) => (d, f),
        None => ("ppt/slides", slide_part),
    };
    let rels_name = format!("{dir}/_rels/{file}.rels");
    let base = dir.to_string();
    Ok(pkg
        .read_optional(&rels_name)?
        .map(|x| parse_rels(&x, &base))
        .unwrap_or_default())
}

/// 沿 slideLayout → slideMaster 收集占位符几何、背景色与装饰图形。
fn resolve_placeholders(
    pkg: &mut Package,
    slide_rels: &HashMap<String, Rel>,
    theme: &Colors,
) -> (Placeholders, Option<u32>, Vec<Shape>) {
    let mut ph = Placeholders::empty();
    let mut bg = None;
    let mut decor: Vec<Shape> = Vec::new();

    let layout = slide_rels
        .values()
        .find(|r| r.ty.ends_with("/slideLayout"))
        .map(|r| r.target.clone());

    if let Some(layout_part) = layout {
        if let Ok(Some(lxml)) = pkg.read_optional(&layout_part) {
            let (lph, lbg, ldecor) = parse_placeholders(&lxml, theme);
            // master 先吸收（低优先），layout 后吸收（覆盖）
            if let Some(master_part) = master_of(pkg, &layout_part) {
                if let Ok(Some(mxml)) = pkg.read_optional(&master_part) {
                    let (mph, mbg, mdecor) = parse_placeholders(&mxml, theme);
                    ph.absorb(mph);
                    bg = bg.or(mbg);
                    decor.extend(mdecor);
                }
            }
            ph.absorb(lph);
            bg = bg.or(lbg);
            decor.extend(ldecor);
        }
    }
    (ph, bg, decor)
}

fn master_of(pkg: &mut Package, layout_part: &str) -> Option<String> {
    let (dir, file) = layout_part.rsplit_once('/')?;
    let rels_name = format!("{dir}/_rels/{file}.rels");
    let xml = pkg.read_optional(&rels_name).ok().flatten()?;
    let map = parse_rels(&xml, dir);
    map.values()
        .find(|r| r.ty.ends_with("/slideMaster"))
        .map(|r| r.target.clone())
}

fn extract_core_title(xml: &str) -> String {
    let root = parse_xml(xml);
    find_desc(&root, "title")
        .map(|n| n.text.trim().to_string())
        .unwrap_or_default()
}

/// 只读 `presentation.xml` 的幻灯片尺寸（给开窗用，便宜）。
pub fn presentation_layout(path: &Path) -> Option<SlideSize> {
    let mut pkg = Package::open(path).ok()?;
    let xml = pkg.read_optional("ppt/presentation.xml").ok()??;
    let pres = parse_xml(&xml);
    let (w, h) = presentation_size(&pres);
    if w == 0 || h == 0 {
        return None;
    }
    Some(SlideSize {
        width: w,
        height: h,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attr_lookup_handles_prefixes_and_quotes() {
        let attrs = r#" r:id="rId4" id="260" x="123" y='7' cx="99""#;
        assert_eq!(attr_lookup(attrs, "r:id"), Some("rId4"));
        assert_eq!(attr_lookup(attrs, "id"), Some("260"));
        assert_eq!(attr_lookup(attrs, "x"), Some("123"));
        assert_eq!(attr_lookup(attrs, "y"), Some("7"));
        assert_eq!(attr_lookup(attrs, "cx"), Some("99"));
        assert_eq!(attr_lookup(attrs, "missing"), None);
    }

    #[test]
    fn xml_tree_nests_and_decodes() {
        let xml = r#"<a:root a="1"><a:child>hi &amp; bye</a:child><a:empty/></a:root>"#;
        let root = parse_xml(xml);
        let r = root.child("root").expect("root");
        let child = r.child("child").expect("child");
        assert_eq!(child.text, "hi & bye");
        assert!(r.child("empty").is_some());
    }

    #[test]
    fn parse_hex_accepts_rgb_and_argb() {
        assert_eq!(parse_hex("FF8800"), Some(0xFF8800));
        assert_eq!(parse_hex("FFFF8800"), Some(0xFF8800));
        assert_eq!(parse_hex("xy"), None);
    }

    #[test]
    fn scheme_color_uses_theme_and_modifiers() {
        let mut theme = Colors::new();
        theme.insert("accent1".to_string(), 0x808080);
        let xml = r#"<schemeClr val="accent1"><lumMod val="50000"/></schemeClr>"#;
        let el = parse_xml(xml);
        let c = resolve_color(el.child("schemeClr").unwrap(), &theme).unwrap();
        // 0x80 * 0.5 = 0x40
        assert_eq!(c, 0x404040);
    }

    #[test]
    fn shape_geometry_and_rotation_parsed() {
        let xml = r#"<p:spPr><a:xfrm rot="5400000"><a:off x="0" y="0"/><a:ext cx="1000000" cy="500000"/></a:xfrm><a:prstGeom prst="roundRect"><a:avLst/></a:prstGeom></p:spPr>"#;
        let root = parse_xml(xml);
        let sp_pr = root.child("spPr").unwrap();
        assert_eq!(rect_geom(sp_pr), RectGeom::RoundRect);
        assert_eq!(xfrm_rot(sp_pr), 90);

        let root2 =
            parse_xml(r#"<p:spPr><a:prstGeom prst="ellipse"><a:avLst/></a:prstGeom></p:spPr>"#);
        assert_eq!(rect_geom(root2.child("spPr").unwrap()), RectGeom::Ellipse);

        let root3 =
            parse_xml(r#"<p:spPr><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr>"#);
        assert_eq!(rect_geom(root3.child("spPr").unwrap()), RectGeom::Rect);
    }

    #[test]
    fn master_decor_shapes_extracted_but_placeholders_kept() {
        let master = r#"<p:sldMaster><p:cSld><p:spTree>
            <p:sp><p:nvSpPr><p:cNvPr id="2" name="decor"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
              <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="12192000" cy="1000000"/></a:xfrm>
                <a:solidFill><a:srgbClr val="FF0000"/></a:solidFill></p:spPr></p:sp>
            <p:sp><p:nvSpPr><p:cNvPr id="3" name="title"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
              <p:spPr><a:xfrm><a:off x="914400" y="914400"/><a:ext cx="8000000" cy="1000000"/></a:xfrm></p:spPr></p:sp>
          </p:spTree></p:cSld></p:sldMaster>"#;
        let (ph, _bg, decor) = parse_placeholders(master, &Colors::new());
        // 装饰图形（无 ph）被抽出，占位符不混进装饰。
        assert_eq!(decor.len(), 1, "应抽出 1 个装饰图形，实际 {}", decor.len());
        match &decor[0] {
            Shape::Rect(r) => assert_eq!(r.fill, Some(0x00FF_0000)),
            other => panic!("装饰应为 Rect，实际 {other:?}"),
        }
        // 占位符仍按 type 记录几何。
        assert!(
            ph.get(None, Some("title")).is_some(),
            "title 占位符几何应保留"
        );
    }

    #[test]
    fn gradient_background_averages_color_stops() {
        let master = r#"<p:sldMaster><p:cSld>
            <p:bg><p:bgPr><a:gradFill><a:gsLst>
              <a:gs pos="0"><a:srgbClr val="000000"/></a:gs>
              <a:gs pos="100000"><a:srgbClr val="FFFFFF"/></a:gs>
            </a:gsLst></a:gradFill></p:bgPr></p:bg>
            <p:spTree/></p:cSld></p:sldMaster>"#;
        let (_ph, bg, _decor) = parse_placeholders(master, &Colors::new());
        assert_eq!(bg, Some(0x007F_7F7F), "黑白渐变应平均成中灰");
    }

    #[test]
    fn join_part_resolves_parent_segments() {
        assert_eq!(
            join_part("ppt/slides", "../media/image1.png"),
            "ppt/media/image1.png"
        );
        assert_eq!(
            join_part("ppt", "slides/slide1.xml"),
            "ppt/slides/slide1.xml"
        );
        assert_eq!(join_part("ppt", "theme/theme1.xml"), "ppt/theme/theme1.xml");
    }

    #[test]
    fn presentation_size_reads_sld_sz() {
        let xml = r#"<p:presentation><p:sldSz cx="12192000" cy="6858000"/></p:presentation>"#;
        let pres = parse_xml(xml);
        assert_eq!(presentation_size(&pres), (1280, 720));
    }

    #[test]
    fn wrap_breaks_cjk_per_character() {
        let run = Run {
            text: "一二三四五六七八".to_string(),
            size_dpt: 320,
            ..Run::default()
        };
        // 每个汉字约 32px 宽（approx_measure 的 CJK 全角），宽度 3 字 → 每行 3 个左右。
        let lines = wrap_runs(&[run], 100, 40, &approx_measure);
        assert!(lines.len() >= 2, "应折成多行：{}", lines.len());
    }

    #[test]
    fn truthy_reads_boolean_attrs() {
        assert!(truthy(Some("1")));
        assert!(truthy(Some("true")));
        assert!(!truthy(Some("0")));
        assert!(!truthy(None));
    }

    /// 回归：parse_xml 返回合成 `#root`，早先直接在 #root 上找 `Relationship`
    /// 会得到空 map（导致图片/占位符全丢、幻灯片全空白）。必须下钻到 `<Relationships>`。
    #[test]
    fn parse_rels_descends_relationships_root() {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
            <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
            <Relationship Id=\"rId1\" \
            Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout\" \
            Target=\"../slideLayouts/slideLayout2.xml\"/>\
            </Relationships>";
        let map = parse_rels(xml, "ppt/slides");
        assert_eq!(map.len(), 1, "rels 应解析出 1 条");
        let r = map.get("rId1").expect("rId1 存在");
        assert!(r.ty.ends_with("/slideLayout"));
        assert_eq!(r.target, "ppt/slideLayouts/slideLayout2.xml");
    }

    /// 回归：从最小 chart XML 解析出柱状图标题/类目/系列。
    #[test]
    fn parse_chart_reads_bar_series() {
        let xml = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><c:chart><c:title><c:tx><c:rich><a:p><a:r><a:t>销量对比</a:t></a:r></a:p></c:rich></c:tx></c:title><c:plotArea><c:barChart><c:barDir val="col"/><c:grouping val="clustered"/><c:ser><c:idx val="0"/><c:tx><c:strRef><c:strCache><c:pt idx="0"><c:v>A</c:v></c:pt></c:strCache></c:strRef></c:tx><c:spPr><a:solidFill><a:srgbClr val="4472C4"/></a:solidFill></c:spPr><c:cat><c:strRef><c:strCache><c:pt idx="0"><c:v>1月</c:v></c:pt><c:pt idx="1"><c:v>2月</c:v></c:pt></c:strCache></c:strRef></c:cat><c:val><c:numRef><c:numCache><c:formatCode>0</c:formatCode><c:pt idx="0"><c:v>3</c:v></c:pt><c:pt idx="1"><c:v>5</c:v></c:pt></c:numCache></c:numRef></c:val></c:ser></c:barChart></c:plotArea></c:chart></c:chartSpace>"#;
        let theme = Colors::new();
        let chart = parse_chart(xml, &theme).expect("应解析出图表");
        assert_eq!(chart.kind, ChartKind::Bar);
        assert_eq!(chart.title, "销量对比");
        assert_eq!(chart.categories, vec!["1月".to_string(), "2月".to_string()]);
        assert_eq!(chart.series.len(), 1);
        assert_eq!(chart.series[0].name, "A");
        assert_eq!(chart.series[0].values, vec![3.0, 5.0]);
        assert_eq!(chart.series[0].color, 0x0044_72C4);
    }
}
