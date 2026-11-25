//! HTML 样式表（CSS）的极简解析与层叠，零依赖纯逻辑。
//!
//! 只做预览「读得懂 + 有颜色/层次」需要的那一小撮：颜色、背景、粗细、斜体、
//! 下划线、字号、对齐、margin/padding、border、display:none。选择器只支持
//! `tag` / `.class` / `#id` 以及它们的简单组合（后代/伪类/`>` 一律忽略）。
//!
//! 这里不碰像素，只把声明算成 [`InlineStyle`]（逐段上色用）与 [`BlockProps`]
//! （块盒子近似用），由 `html.rs` 排版、`qlr-platform` 绘制。

use crate::canvas::InlineStyle;

/// 解析 CSS 颜色：`#rgb` / `#rrggbb` / `rgb(r,g,b)` / 少量命名色。
pub fn parse_color(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        let hex = hex.trim();
        return match hex.len() {
            3 => {
                let r = u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()? as u32;
                let g = u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()? as u32;
                let b = u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()? as u32;
                Some((r << 16) | (g << 8) | b)
            }
            6 => u32::from_str_radix(hex, 16).ok(),
            _ => None,
        };
    }
    if let Some(rest) = s.strip_prefix("rgb(").or_else(|| s.strip_prefix("rgba(")) {
        let inner = rest.trim_end_matches(')');
        let mut it = inner.split(',').map(|x| x.trim());
        let r: u32 = it.next()?.parse().ok()?;
        let g: u32 = it.next()?.parse().ok()?;
        let b: u32 = it.next()?.parse().ok()?;
        return Some(((r.min(255)) << 16) | ((g.min(255)) << 8) | b.min(255));
    }
    let named = match s.to_ascii_lowercase().as_str() {
        "black" => 0x000000,
        "white" => 0xFFFFFF,
        "red" => 0xFF0000,
        "green" => 0x008000,
        "blue" => 0x0000FF,
        "yellow" => 0xFFFF00,
        "orange" => 0xFFA500,
        "gray" | "grey" => 0x808080,
        "silver" => 0xC0C0C0,
        "purple" => 0x800080,
        "navy" => 0x000080,
        "teal" => 0x008080,
        "maroon" => 0x800000,
        "olive" => 0x808000,
        "lime" => 0x00FF00,
        "aqua" | "cyan" => 0x00FFFF,
        "fuchsia" | "magenta" => 0xFF00FF,
        _ => return None,
    };
    Some(named)
}

/// 解析长度（px / em / pt / 无单位）为 DIP 近似值；百分比返回 None。
fn parse_len(s: &str) -> Option<i32> {
    let s = s.trim();
    if s == "0" {
        return Some(0);
    }
    let (num, unit) = split_num_unit(s)?;
    let v = match unit {
        "px" | "" => num,
        "em" => num * 16.0,
        "rem" => num * 16.0,
        "pt" => num * 96.0 / 72.0,
        _ => return None,
    };
    Some(v.round() as i32)
}

fn split_num_unit(s: &str) -> Option<(f32, &str)> {
    let idx = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .unwrap_or(s.len());
    let (n, u) = s.split_at(idx);
    let num: f32 = n.parse().ok()?;
    Some((num, u.trim()))
}

/// 一条声明列表（解析自 `style="..."` 或 `<style>` 规则体）。
#[derive(Clone, Debug, Default)]
pub struct Decls {
    items: Vec<(String, String)>,
}

impl Decls {
    pub fn parse(s: &str) -> Decls {
        let mut items = Vec::new();
        for part in s.split(';') {
            let Some((k, v)) = part.split_once(':') else {
                continue;
            };
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim().to_string();
            if !k.is_empty() && !v.is_empty() {
                items.push((k, v));
            }
        }
        Decls { items }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.items
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn merge_over(&mut self, other: &Decls) {
        for (k, v) in &other.items {
            if let Some(slot) = self.items.iter_mut().find(|(kk, _)| kk == k) {
                slot.1 = v.clone();
            } else {
                self.items.push((k.clone(), v.clone()));
            }
        }
    }
}

/// 一个简单选择器：`tag` / `.class` / `#id` 的组合。
#[derive(Clone, Debug, Default)]
pub struct Selector {
    pub tag: Option<String>,
    pub id: Option<String>,
    pub classes: Vec<String>,
}

impl Selector {
    pub fn parse(s: &str) -> Option<Selector> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        // 只取最后一段（忽略 `a b`、`a > b` 的后代/子代关系）
        let last = s
            .split(|c: char| c.is_whitespace() || c == '>' || c == '+' || c == '~')
            .next_back()
            .unwrap_or(s);
        // 去掉伪类
        let last = last.split(':').next().unwrap_or(last);
        if last.is_empty() || last == "*" {
            return None;
        }
        let mut sel = Selector::default();
        let mut tag_buf = String::new();
        let mut chars = last.chars().peekable();
        while let Some(&c) = chars.peek() {
            match c {
                '.' => {
                    chars.next();
                    let mut cl = String::new();
                    while let Some(&d) = chars.peek() {
                        if d == '.' || d == '#' {
                            break;
                        }
                        cl.push(d);
                        chars.next();
                    }
                    if !cl.is_empty() {
                        sel.classes.push(cl);
                    }
                }
                '#' => {
                    chars.next();
                    let mut id = String::new();
                    while let Some(&d) = chars.peek() {
                        if d == '.' || d == '#' {
                            break;
                        }
                        id.push(d);
                        chars.next();
                    }
                    if !id.is_empty() {
                        sel.id = Some(id);
                    }
                }
                _ => {
                    tag_buf.push(c);
                    chars.next();
                }
            }
        }
        if !tag_buf.is_empty() {
            sel.tag = Some(tag_buf.to_ascii_lowercase());
        }
        Some(sel)
    }

    pub fn specificity(&self) -> u32 {
        let mut s = 0;
        if self.id.is_some() {
            s += 100;
        }
        s += 10 * self.classes.len() as u32;
        if self.tag.is_some() {
            s += 1;
        }
        s
    }

    pub fn matches(&self, tag: &str, id: Option<&str>, classes: &[&str]) -> bool {
        if let Some(t) = &self.tag
            && !t.eq_ignore_ascii_case(tag)
        {
            return false;
        }
        if let Some(want) = &self.id
            && Some(want.as_str()) != id
        {
            return false;
        }
        for c in &self.classes {
            if !classes.iter().any(|x| x.eq_ignore_ascii_case(c)) {
                return false;
            }
        }
        true
    }
}

#[derive(Clone, Debug)]
pub struct CssRule {
    pub selector: Selector,
    pub decls: Decls,
}

/// 整张样式表。
#[derive(Clone, Debug, Default)]
pub struct Stylesheet {
    pub rules: Vec<CssRule>,
}

impl Stylesheet {
    pub fn parse(css: &str) -> Stylesheet {
        let mut rules = Vec::new();
        let bytes = css.as_bytes();
        let mut i = 0usize;
        // 逐条 `selector { decls }`，跳过 `@media` 之类的外层块（取里面的规则）。
        while i < bytes.len() {
            let Some(open) = css[i..].find('{').map(|k| i + k) else {
                break;
            };
            let sel_text = &css[i..open];
            let Some(close) = css[open + 1..].find('}').map(|k| open + 1 + k) else {
                break;
            };
            let body = &css[open + 1..close];
            // 选择器可能是逗号分隔的组合；`@` 开头的（@media/@font-face）跳过其选择器部分。
            let sel_text = sel_text.trim();
            if !sel_text.starts_with('@') {
                let decls = Decls::parse(body);
                for one in sel_text.split(',') {
                    if let Some(selector) = Selector::parse(one) {
                        rules.push(CssRule {
                            selector,
                            decls: decls.clone(),
                        });
                    }
                }
            }
            i = close + 1;
        }
        Stylesheet { rules }
    }

    /// 收集匹配该元素的所有声明，按特异性从低到高排序（后者覆盖前者）。
    pub fn matching(&self, tag: &str, id: Option<&str>, classes: &[&str]) -> Vec<Decls> {
        let mut hits: Vec<(u32, usize, &Decls)> = Vec::new();
        for (idx, r) in self.rules.iter().enumerate() {
            if r.selector.matches(tag, id, classes) {
                hits.push((r.selector.specificity(), idx, &r.decls));
            }
        }
        // 特异性升序；同特异性按出现顺序（靠后的覆盖靠前的）。
        hits.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        hits.into_iter().map(|(_, _, d)| d.clone()).collect()
    }
}

/// 块元素的盒模型近似（margin/padding/border/背景/对齐/是否隐藏）。
#[derive(Clone, Copy, Debug, Default)]
pub struct BlockProps {
    /// 0=左 1=居中 2=右。
    pub align: u8,
    pub margin_top: i32,
    pub margin_bottom: i32,
    pub padding_left: i32,
    pub padding_right: i32,
    pub padding_top: i32,
    pub padding_bottom: i32,
    pub bg: Option<u32>,
    pub border: Option<(u32, i32)>,
    pub display_none: bool,
}

impl BlockProps {
    pub fn is_default(&self) -> bool {
        self.align == 0
            && self.margin_top == 0
            && self.margin_bottom == 0
            && self.padding_left == 0
            && self.padding_right == 0
            && self.padding_top == 0
            && self.padding_bottom == 0
            && self.bg.is_none()
            && self.border.is_none()
            && !self.display_none
    }
}

/// 把「父内联样式 + 样式表匹配规则 + 元素自身 `style=`」叠加，算出该元素的
/// 内联样式与块属性。
pub fn compute(
    parent: InlineStyle,
    sheet: Option<&Stylesheet>,
    tag: &str,
    id: Option<&str>,
    classes: &[&str],
    inline_style: Option<&str>,
) -> (InlineStyle, BlockProps) {
    let mut merged = Decls::default();
    if let Some(sheet) = sheet {
        for d in sheet.matching(tag, id, classes) {
            merged.merge_over(&d);
        }
    }
    if let Some(inline) = inline_style {
        merged.merge_over(&Decls::parse(inline));
    }

    // 内联继承：颜色/粗细/斜体/下划线/字号默认从父级继承。
    let mut inline = InlineStyle {
        font_pct: parent.pct(),
        ..parent
    };

    if let Some(v) = merged.get("color") {
        if let Some(c) = parse_color(v) {
            inline.fg = Some(c);
        }
    }
    if let Some(v) = merged
        .get("background-color")
        .or_else(|| merged.get("background"))
    {
        if let Some(c) = parse_color(v) {
            inline.bg = Some(c);
        }
    }
    if let Some(v) = merged.get("font-weight") {
        let v = v.trim().to_ascii_lowercase();
        if v == "bold" || v == "bolder" || v.parse::<u32>().map(|n| n >= 600).unwrap_or(false) {
            inline.bold = true;
        } else if v == "normal" || v == "lighter" || v == "400" {
            inline.bold = false;
        }
    }
    if let Some(v) = merged.get("font-style") {
        inline.italic = v.trim().eq_ignore_ascii_case("italic");
    }
    if let Some(v) = merged
        .get("text-decoration-line")
        .or_else(|| merged.get("text-decoration"))
    {
        if v.to_ascii_lowercase().contains("underline") {
            inline.underline = true;
        }
    }
    if let Some(v) = merged.get("font-size") {
        inline.font_pct = font_size_pct(v, parent.pct());
    }

    // 块属性。
    let mut block = BlockProps::default();
    if let Some(v) = merged.get("text-align") {
        block.align = match v.trim().to_ascii_lowercase().as_str() {
            "center" => 1,
            "right" | "end" => 2,
            _ => 0,
        };
    }
    if let Some(v) = merged.get("display") {
        if v.trim().eq_ignore_ascii_case("none") {
            block.display_none = true;
        }
    }
    if let Some(c) = merged
        .get("background-color")
        .or_else(|| merged.get("background"))
        .and_then(parse_color)
    {
        block.bg = Some(c);
    }
    apply_box(&merged, &mut block);
    (inline, block)
}

fn font_size_pct(v: &str, parent_pct: u16) -> u16 {
    let v = v.trim().to_ascii_lowercase();
    let base = parent_pct.max(1) as f32 / 100.0;
    match v.as_str() {
        "small" => return (base * 0.85 * 100.0).round() as u16,
        "large" => return (base * 1.2 * 100.0).round() as u16,
        "x-large" => return (base * 1.5 * 100.0).round() as u16,
        "xx-large" => return (base * 2.0 * 100.0).round() as u16,
        "medium" | "initial" | "inherit" => return parent_pct,
        _ => {}
    }
    if let Some(p) = v.strip_suffix('%') {
        if let Ok(n) = p.trim().parse::<f32>() {
            return (base * n / 100.0 * 100.0).round() as u16;
        }
    }
    if let Some((n, unit)) = split_num_unit(&v) {
        let px = match unit {
            "px" | "" => n,
            "em" => n * 16.0 * base,
            "rem" => n * 16.0,
            "pt" => n * 96.0 / 72.0,
            _ => n,
        };
        return ((px / 16.0) * 100.0).round() as u16;
    }
    parent_pct
}

fn apply_box(d: &Decls, block: &mut BlockProps) {
    if let Some(v) = d.get("padding") {
        let sides = parse_sides(v);
        block.padding_top = sides[0];
        block.padding_right = sides[1];
        block.padding_bottom = sides[2];
        block.padding_left = sides[3];
    }
    if let Some(v) = d.get("padding-left").and_then(parse_len) {
        block.padding_left = v;
    }
    if let Some(v) = d.get("padding-top").and_then(parse_len) {
        block.padding_top = v;
    }
    if let Some(v) = d.get("padding-bottom").and_then(parse_len) {
        block.padding_bottom = v;
    }
    if let Some(v) = d.get("margin") {
        let sides = parse_sides(v);
        block.margin_top = sides[0];
        block.margin_bottom = sides[2];
    }
    if let Some(v) = d.get("margin-top").and_then(parse_len) {
        block.margin_top = v;
    }
    if let Some(v) = d.get("margin-bottom").and_then(parse_len) {
        block.margin_bottom = v;
    }
    // border / border-left 等：取颜色与宽度（只画一条近似边框线）。
    for key in [
        "border",
        "border-left",
        "border-top",
        "border-right",
        "border-bottom",
    ] {
        if let Some(v) = d.get(key) {
            let mut color = None;
            let mut width = 1;
            for tok in v.split_whitespace() {
                if let Some(c) = parse_color(tok) {
                    color = Some(c);
                } else if let Some(w) = parse_len(tok) {
                    if w >= 0 {
                        width = w.max(1);
                    }
                }
            }
            if let Some(c) = color {
                block.border = Some((c, width));
            } else if v.to_ascii_lowercase().contains("solid") {
                block.border = Some((0x000000, width));
            }
        }
    }
}

/// `padding`/`margin` 简写 → [top,right,bottom,left]。
fn parse_sides(v: &str) -> [i32; 4] {
    let parts: Vec<i32> = v.split_whitespace().filter_map(parse_len).collect();
    match parts.as_slice() {
        [] => [0, 0, 0, 0],
        [a] => [*a, *a, *a, *a],
        [a, b] => [*a, *b, *a, *b],
        [a, b, c] => [*a, *b, *c, *b],
        [a, b, c, dd, ..] => [*a, *b, *c, *dd],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_colors() {
        assert_eq!(parse_color("#f00"), Some(0xFF0000));
        assert_eq!(parse_color("#00ff00"), Some(0x00FF00));
        assert_eq!(parse_color("rgb(1,2,3)"), Some(0x010203));
        assert_eq!(parse_color("white"), Some(0xFFFFFF));
        assert_eq!(parse_color("notacolor"), None);
    }

    #[test]
    fn stylesheet_cascade_by_specificity() {
        let sheet = Stylesheet::parse(
            "p { color: #111111 } .hi { color: #222222 } #top { color: #333333 }",
        );
        // p 命中 tag(1) 与 #top(100)；#top 更特异。
        let (style, _) = compute(
            InlineStyle::default(),
            Some(&sheet),
            "p",
            Some("top"),
            &[],
            None,
        );
        assert_eq!(style.fg, Some(0x333333));
        // 只有 class
        let (style, _) = compute(
            InlineStyle::default(),
            Some(&sheet),
            "p",
            None,
            &["hi"],
            None,
        );
        assert_eq!(style.fg, Some(0x222222));
    }

    #[test]
    fn inline_style_wins_over_stylesheet() {
        let sheet = Stylesheet::parse("p { color: red; font-weight: bold }");
        let (style, _) = compute(
            InlineStyle::default(),
            Some(&sheet),
            "p",
            None,
            &[],
            Some("color: #0000ff"),
        );
        assert_eq!(style.fg, Some(0x0000FF));
        assert!(style.bold);
    }

    #[test]
    fn block_box_parsed() {
        let (_, block) = compute(
            InlineStyle::default(),
            None,
            "div",
            None,
            &[],
            Some(
                "padding: 4px 8px; margin: 10px; border: 2px solid #123456; text-align: center; background: #eeeeee",
            ),
        );
        assert_eq!(block.padding_top, 4);
        assert_eq!(block.padding_left, 8);
        assert_eq!(block.margin_top, 10);
        assert_eq!(block.border, Some((0x123456, 2)));
        assert_eq!(block.align, 1);
        assert_eq!(block.bg, Some(0xEEEEEE));
    }

    #[test]
    fn display_none_flagged() {
        let (_, block) = compute(
            InlineStyle::default(),
            None,
            "div",
            None,
            &[],
            Some("display:none"),
        );
        assert!(block.display_none);
    }

    #[test]
    fn font_size_percent_and_px() {
        let (s, _) = compute(
            InlineStyle::default(),
            None,
            "span",
            None,
            &[],
            Some("font-size: 24px"),
        );
        assert_eq!(s.pct(), 150);
        let (s, _) = compute(
            InlineStyle::default(),
            None,
            "span",
            None,
            &[],
            Some("font-size: 50%"),
        );
        assert_eq!(s.pct(), 50);
    }
}
