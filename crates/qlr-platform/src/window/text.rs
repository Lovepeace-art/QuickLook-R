//! 文本/文档绘制与测量 —— 从 window.rs 纯搬家拆出（2026-11-04）。
//!
//! 每块只多一行 `use super::*;`，函数体一字未改。

use super::*;

/// 文本客户区的底色（不透明）。文本预览不再走亚克力：亚克力会把字和背景混色，
/// 滚动时新旧字形叠加会更明显（1.0.1 起文本一律刷不透明底）。
pub const TEXT_BACKGROUND_RGB: u32 = BITMAP_BACKGROUND_RGB;

/// 文本从客户区顶部往下留的空白（像素）。
pub fn text_padding_top() -> i32 {
    dpi::cpx(16)
}

/// 文本行高（像素）。**必须与 `qlr-app` 的 `line_height()` 一致**：那边用它算可见行数。
pub fn line_height() -> i32 {
    dpi::cpx(24)
}

/// 行号栏宽度（像素）。行号靠右对齐画在这一栏里，正文从栏右侧开始。
///
/// 为什么是**固定常数**而不是"按最大行号的位数算"：折行时"量的宽度"必须等于
/// "画的宽度"（CONVENTIONS #38）。常数两边都不会算错；动态算就得让渲染折行和
/// GDI 绘制各自量一遍数字宽度，量法一旦不同就会出现"折了行还被省略号吃掉"。
/// 64 px 足够 6 位数行号（文本预算上限 20 万行）。
pub fn text_gutter() -> i32 {
    dpi::cpx(64)
}

/// 上屏用的行样式（像素级：字号/字重/颜色/字体面）。
///
/// 为什么平台层自己定义一份、而不是直接用 `qlr_render::LineStyle`：
/// `qlr-platform` 不依赖 `qlr-render`（渲染层可以换成别家实现，平台层只管画）。
/// 语义 → 像素的映射由壳层做一次转换，两边各自演进不会互相拖。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextStyle {
    Normal,
    Heading1,
    Heading2,
    Heading3,
    Code,
    Quote,
    List,
    Rule,
    /// Markdown 表格行（`|` 已去掉，单元格用 `│` 分隔）。
    Table,
    /// 左侧行号（1.0.5）：只出现在文本/Markdown 的行号栏里，不对应任何内容样式。
    LineNumber,
    /// 代码 token（1.0.8）：关键字（`fn`/`let`/`class`…），VSCode 浅色主题的蓝。
    Keyword,
    /// 代码 token：字符串字面量，VSCode 浅色主题的砖红。
    Str,
    /// 代码 token：注释，VSCode 浅色主题的绿。
    Comment,
    /// 代码 token：数字字面量，VSCode 浅色主题的青绿。
    Number,
}

impl TextStyle {
    const COUNT: usize = 14;

    /// 与 `qlr_render::LineStyle::as_index` 同序（壳层按索引直转）。
    /// 前 9 个与渲染层一一对应；后面 5 个是平台层独有的附加样式。
    pub fn as_index(self) -> usize {
        match self {
            TextStyle::Normal => 0,
            TextStyle::Heading1 => 1,
            TextStyle::Heading2 => 2,
            TextStyle::Heading3 => 3,
            TextStyle::Code => 4,
            TextStyle::Quote => 5,
            TextStyle::List => 6,
            TextStyle::Rule => 7,
            TextStyle::Table => 8,
            TextStyle::LineNumber => 9,
            TextStyle::Keyword => 10,
            TextStyle::Str => 11,
            TextStyle::Comment => 12,
            TextStyle::Number => 13,
        }
    }

    /// 字号（**物理像素**，逻辑高度）。全部 ≤18 DIP，保证塞进 24 DIP 行高不被裁。
    ///
    /// 1.8.3：表里写的是 DIP，实际传给 `CreateFontW` 的是按 DPI 换算后的值。
    /// 200% 下正文 15 → 30，与 30×2=60 的顶栏、48 的行高成比例，
    /// 这就是用户要的"字看起来和 100% 下一模一样大"。
    fn height(self) -> i32 {
        let dip = match self {
            TextStyle::Heading1 => 18,
            TextStyle::Heading2 => 17,
            TextStyle::Heading3 => 16,
            TextStyle::Code => 13,
            TextStyle::Table => 13,
            TextStyle::LineNumber => 12,
            TextStyle::Keyword | TextStyle::Str | TextStyle::Comment | TextStyle::Number => 14,
            _ => 15,
        };
        dpi::cpx(dip)
    }

    pub(crate) fn weight(self) -> i32 {
        match self {
            TextStyle::Heading1 | TextStyle::Heading2 | TextStyle::Heading3 => FW_BOLD.0 as i32,
            _ => FW_NORMAL.0 as i32,
        }
    }

    /// 字体面：代码用等宽，其余用雅黑（Win10 自带、CJK 覆盖好）。
    fn face(self) -> &'static str {
        match self {
            TextStyle::Code
            | TextStyle::Table
            | TextStyle::LineNumber
            | TextStyle::Keyword
            | TextStyle::Str
            | TextStyle::Comment
            | TextStyle::Number => "Consolas",
            _ => "Microsoft YaHei",
        }
    }

    /// 文字颜色。`COLORREF` 是 `0x00BBGGRR`（注意 R 在最低字节）。
    ///
    /// 1.0.8 起是**浅色主题**（白底），配色对齐 VSCode Light：正文接近黑，
    /// 关键字蓝、字符串砖红、注释绿、数字青绿。
    pub(crate) fn color(self) -> u32 {
        match self {
            TextStyle::Normal | TextStyle::List => 0x0020_2020, // #202020 正文
            TextStyle::Heading1 => 0x00B5_6616,                 // #1666B5
            TextStyle::Heading2 => 0x009E_4E1F,                 // #1F4E9E
            TextStyle::Heading3 => 0x003A_3A3A,                 // #3A3A3A
            TextStyle::Code => 0x0022_2222,                     // #222222
            TextStyle::Quote => 0x007D_736A,                    // #6A737D
            TextStyle::Rule => 0x00D4_D4D4,                     // #D4D4D4
            TextStyle::Table => 0x0020_2020,                    // #202020
            TextStyle::LineNumber => 0x00A6_A09A,               // #9AA0A6 中灰：能定位但不抢注意力
            TextStyle::Keyword => 0x00FF_0000,                  // #0000FF
            TextStyle::Str => 0x0015_15A3,                      // #A31515
            TextStyle::Comment => 0x0000_8000,                  // #008000
            TextStyle::Number => 0x0058_8609,                   // #098658
        }
    }

    fn create(self) -> HFONT {
        let face = to_wide(self.face());
        unsafe {
            CreateFontW(
                -self.height(),
                0,
                0,
                0,
                self.weight(),
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_TT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                DEFAULT_PITCH.0 as u32,
                PCWSTR(face.as_ptr()),
            )
        }
    }
}

/// 一行待绘制文本。
pub struct StyledRow<'a> {
    /// 左侧行号（1 基）。`None` = 这一行不显示行号（折行的续行、状态行都是 None）。
    pub number: Option<u32>,
    pub text: &'a str,
    pub style: TextStyle,
    /// 整行底色（Markdown 代码块/引用块），`None` = 跟随窗口底色。
    pub bg: Option<u32>,
}

/// 一段同色文本：一行可以有多个 span（代码着色、Markdown 行内混排）。
pub struct Span<'a> {
    pub text: &'a str,
    pub style: TextStyle,
}

/// 一行待绘制内容：行号 + 若干样式段 + 可选行底色（1.0.8）。
///
/// `paint_styled` 的 `StyledRow` 是它的单段特例；代码预览需要"一行多色"，
/// 所以真正的绘制路径是这里的 `paint_doc`。
pub struct DocRow<'a> {
    /// 左侧行号（1 基）。`None` = 这一行不显示行号（折行续行、状态行）。
    pub number: Option<u32>,
    pub spans: &'a [Span<'a>],
    /// 整行底色（Markdown 代码块 / 引用块的分色条），`None` = 跟随窗口底色。
    pub bg: Option<u32>,
}

thread_local! {
    /// 每线程一套字体，惰性创建（一次重画要画几十行，不能每行 CreateFontW）。
    pub(crate) static FONTS: std::cell::RefCell<FontSet> = std::cell::RefCell::new(FontSet::default());
}

pub(crate) struct FontSet {
    fonts: [Option<HFONT>; TextStyle::COUNT],
}

impl Default for FontSet {
    fn default() -> Self {
        Self {
            fonts: [None; TextStyle::COUNT],
        }
    }
}

impl FontSet {
    /// 取（并按需创建）某个样式的字体。创建失败就退回系统 GUI 字体，绝不 panic。
    pub(crate) fn get(&mut self, style: TextStyle) -> HFONT {
        let idx = style.as_index();
        if let Some(font) = self.fonts[idx] {
            return font;
        }
        let font = style.create();
        if font.0.is_null() {
            qlr_core::log_warn!(
                "window",
                "CreateFontW 失败（{}），该样式退回系统 GUI 字体",
                style.face()
            );
            // 不缓存：stock 对象不能被 DeleteObject
            return HFONT(unsafe { GetStockObject(DEFAULT_GUI_FONT) }.0);
        }
        self.fonts[idx] = Some(font);
        font
    }

    /// 丢掉全部缓存字体（DPI 变化时用）：不丢的话跨屏之后还在用旧像素高度的字体，
    /// 字会"半大不小"，而且每次都新建字体但不释放 → 每跨一次屏漏 14 个 HFONT。
    fn clear(&mut self) {
        for slot in &mut self.fonts {
            if let Some(font) = slot.take() {
                // 只删自己 `CreateFontW` 出来的；`get()` 里失败退回 stock 的那条路径
                // **不缓存**，所以这里不可能拿到 stock 对象（删它会失败但也不致命）。
                unsafe {
                    let _ = DeleteObject(font.into());
                }
            }
        }
    }
}

impl Drop for FontSet {
    /// 线程退出时兜底：`thread_local` 不保证 drop，但主线程退出前会走到这里的情况
    /// （例如设置窗所在的线程）至少不把 HFONT 漏给系统。
    fn drop(&mut self) {
        self.clear();
    }
}

/// 丢掉本线程的字体缓存。**这是 `dpi::set_change_hook` 的回调**：DPI 一变就调，
/// 下次绘制按新字号重建。
pub fn fonts_invalidate() {
    // 正常情况下一个窗口生命周期里只会调 1–2 次（跨屏拖动、改缩放）。
    // 如果在同一次预览里反复看到这行，说明线程 DPI 在**来回跳**——那是设计问题，
    // 不是正常路径（每跳一次就要重建 14 个 HFONT，绘制会明显变慢）。
    qlr_core::log_debug!("window", "字体缓存失效（DPI 变化）");
    FONTS.with(|cell| cell.borrow_mut().clear());
}

/// 装上"DPI 变了 → 重建字体"的钩子。宿主启动时调一次即可。
pub fn install_dpi_hook() {
    dpi::set_change_hook(fonts_invalidate);
}

/// 画一组带样式的文本行（逐行上色，每行可以多段）。
///
/// **每次绘制都先刷一层不透明底色**：窗口类背景刷是空的（为了在位图状态下不闪），
/// `WM_ERASEBKGND` 又返回 1（不擦除），所以如果这里不自己刷底，滚动/换内容时
/// 旧字形会留在客户区上——实测表现是"上下键一按，两屏文字叠在一起发糊"（1.0.1 修）。
///
/// 行从**顶部**开始排（不再垂直居中）：居中会让"行数变化"变成"整体上下平移"，
/// 滚动时每一屏的基线都在动，视觉上像抖动。
///
/// 空行只占高度不调 `DrawTextW`（原因见下方注释：空切片会让 user32 拿 `cchText=0`
/// 去 wcslen 悬垂指针）。
/// 一次 `WM_PAINT` 用的离屏缓冲（双缓冲）。建不起来返回 `None`，调用方直接画窗口 DC。
struct PaintBuffer {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    /// 位图尺寸（物理像素）。§6-7：用于判断能否在下一帧复用同一块缓冲。
    w: i32,
    h: i32,
}

impl PaintBuffer {
    unsafe fn new(window_dc: HDC, rc: RECT) -> Option<Self> {
        let dc = unsafe { CreateCompatibleDC(Some(window_dc)) };
        if dc.0.is_null() {
            return None;
        }
        let w = (rc.right - rc.left).max(1);
        let h = (rc.bottom - rc.top).max(1);
        let bitmap = unsafe { CreateCompatibleBitmap(window_dc, w, h) };
        if bitmap.0.is_null() {
            let _ = unsafe { DeleteDC(dc) };
            return None;
        }
        let old = unsafe { SelectObject(dc, bitmap.into()) };
        Some(Self {
            dc,
            bitmap,
            old,
            w,
            h,
        })
    }
}

impl Drop for PaintBuffer {
    fn drop(&mut self) {
        unsafe {
            let _ = SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

// §6-7：整窗离屏缓冲按尺寸复用，避免每帧都 CreateCompatibleBitmap + CreateCompatibleDC
// 再销毁（4K 窗口下每帧 churn 一块 ~33MB 的 GDI 位图）。paint 只在 UI 线程的 `WM_PAINT`
// 里跑，所以用 `thread_local` 就够，不需要锁。
thread_local! {
    static PAINT_BUFFER: std::cell::RefCell<Option<PaintBuffer>> =
        const { std::cell::RefCell::new(None) };
}

/// §6-7：设 `QLRUST_NO_PAINT_REUSE=1` 可关闭"整窗缓冲复用"，退化为每帧新建（仅用于 A/B 实测）。
fn paint_reuse_disabled() -> bool {
    std::env::var_os("QLRUST_NO_PAINT_REUSE").is_some()
}

/// 取一块可复用的整窗离屏缓冲。尺寸变了就重建；建不起来返回 `None`，调用方直画窗口 DC。
fn acquire_paint_buffer(window_dc: HDC, rc: RECT) -> Option<PaintBuffer> {
    let w = (rc.right - rc.left).max(1);
    let h = (rc.bottom - rc.top).max(1);
    if paint_reuse_disabled() {
        return unsafe { PaintBuffer::new(window_dc, rc) };
    }
    PAINT_BUFFER.with(|slot| {
        let mut slot = slot.borrow_mut();
        match slot.take() {
            Some(buf) if buf.w == w && buf.h == h => Some(buf),
            // 尺寸不符（或首次）：旧缓冲在 `take` 出来的值被丢弃时已 `Drop` 释放。
            _ => {
                let nb = unsafe { PaintBuffer::new(window_dc, rc) };
                // §6-7：只在"新建"时打一行，便于 bench/日志确认多帧只建一次（复用生效）。
                if nb.is_some() && qlr_core::log::enabled(qlr_core::log::Level::Debug) {
                    qlr_core::log_debug!("window", "PAINTBUF new {w}x{h}");
                }
                nb
            }
        }
    })
}

/// 把缓冲放回缓存供下一次 `WM_PAINT` 复用（必须在 `EndPaint` 之前调）。
fn release_paint_buffer(buf: Option<PaintBuffer>) {
    if paint_reuse_disabled() {
        return;
    }
    if let Some(buf) = buf {
        PAINT_BUFFER.with(|slot| *slot.borrow_mut() = Some(buf));
    }
}

/// 丢掉复用的离屏缓冲（收起预览 / 切换内容时调用，避免藏起来后仍占一块整窗位图）。
pub fn drop_paint_buffer() {
    PAINT_BUFFER.with(|slot| {
        let _ = slot.borrow_mut().take();
    });
}

/// §6-7：每帧 GDI/USER 对象计数 + 本帧绘制耗时（仅 debug 级打，用于 bench 确认无泄漏/无回退）。
fn log_paint_gdi(tag: &str, ms: f64) {
    if qlr_core::log::enabled(qlr_core::log::Level::Debug) {
        let (g, u) = gui_resources();
        qlr_core::log_debug!("window", "PAINT {tag} gdi={g} user={u} {ms:.2}ms");
    }
}

/// 列表视图的一行（压缩包目录用，1.7.2）。
pub struct ListRow<'a> {
    /// 目录层级（决定缩进）。
    pub depth: usize,
    /// 是否是文件夹。
    pub is_dir: bool,
    /// 文件夹是否展开。
    pub expanded: bool,
    /// 名字（不带路径）。
    pub name: &'a str,
    /// 右列：大小（文件才有，目录留空）。
    pub size: &'a str,
    /// 右列：修改时间。
    pub time: &'a str,
    /// 是否高亮（当前行）。
    pub selected: bool,
}

/// 列表视图的行高。
pub fn list_row_height() -> i32 {
    dpi::cpx(26)
}

/// 画一个文件列表（📁/📄 + 缩进 + 大小 + 时间），带双缓冲。
///
/// 和 `paint_doc` 是两条不同的绘制通道：文本通道讲究"逐行等宽"，列表讲究"列对齐 +
/// 图标 + 缩进"，混在一条通道里会把两边都搞复杂（1.7.2）。
pub fn paint_list(
    hwnd: HWND,
    chrome: Option<&Chrome<'_>>,
    content: Option<RECT>,
    rows: &[ListRow<'_>],
) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let window_dc: HDC = BeginPaint(hwnd, &mut ps);
        if window_dc.0.is_null() {
            let _ = EndPaint(hwnd, &ps);
            return;
        }
        let rc = client_rect(hwnd);
        let pt0 = std::time::Instant::now();
        let mut buffer = acquire_paint_buffer(window_dc, rc);
        let hdc: HDC = buffer.as_ref().map_or(window_dc, |b| b.dc);
        SetBkMode(hdc, TRANSPARENT);
        let brush = CreateSolidBrush(COLORREF(TEXT_BACKGROUND_RGB));
        FillRect(hdc, &rc, brush);
        let _ = DeleteObject(brush.into());
        let area = match chrome {
            Some(spec) => paint_chrome(hdc, rc, spec),
            None => rc,
        };
        let content = content.unwrap_or(area);

        // 三列：左（图标+名字） / 中（大小，右对齐） / 右（时间，右对齐）
        let size_right = content.right - 180;
        let time_right = content.right - 16;
        let size_left = size_right - 110;

        FONTS.with(|cell| {
            let mut set = cell.borrow_mut();
            let mut y = content.top + text_padding_top();
            for row in rows {
                if row.selected {
                    let band = RECT {
                        left: content.left + 4,
                        top: y,
                        right: content.right - 4,
                        bottom: y + list_row_height(),
                    };
                    let hi = CreateSolidBrush(COLORREF(LIST_HIGHLIGHT_RGB));
                    FillRect(hdc, &band, hi);
                    let _ = DeleteObject(hi.into());
                }
                let indent = content.left + 16 + (row.depth as i32) * 16;
                // 折叠标记只用几何符号（▾ / ▸）：GDI 的 `DrawTextW` 不做字体回退，
                // 📁/📄 这类彩色 emoji 会被画成豆腐块（1.7.3 实测截图）。
                // 目录和文件靠"名字末尾的 /"与折叠箭头区分。
                let marker = if row.is_dir {
                    if row.expanded {
                        "\u{25BE} "
                    } else {
                        "\u{25B8} "
                    }
                } else {
                    "  "
                };
                let label = format!("{marker}{}", row.name);
                let font = set.get(TextStyle::Normal);
                let old = SelectObject(hdc, font.into());
                SetTextColor(hdc, COLORREF(TextStyle::Normal.color()));
                let mut rect = RECT {
                    left: indent,
                    right: size_left - 8,
                    top: y,
                    bottom: y + list_row_height(),
                };
                let mut buf: Vec<u16> = label.encode_utf16().collect();
                DrawTextW(
                    hdc,
                    &mut buf,
                    &raw mut rect,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
                );
                if !row.size.is_empty() {
                    SetTextColor(hdc, COLORREF(TextStyle::LineNumber.color()));
                    let mut sr = RECT {
                        left: size_left,
                        right: size_right,
                        top: y,
                        bottom: y + list_row_height(),
                    };
                    let mut sb: Vec<u16> = row.size.encode_utf16().collect();
                    DrawTextW(
                        hdc,
                        &mut sb,
                        &raw mut sr,
                        DT_RIGHT | DT_SINGLELINE | DT_VCENTER,
                    );
                }
                if !row.time.is_empty() {
                    let mut tr = RECT {
                        left: size_right + 8,
                        right: time_right,
                        top: y,
                        bottom: y + list_row_height(),
                    };
                    let mut tb: Vec<u16> = row.time.encode_utf16().collect();
                    DrawTextW(
                        hdc,
                        &mut tb,
                        &raw mut tr,
                        DT_RIGHT | DT_SINGLELINE | DT_VCENTER,
                    );
                }
                SelectObject(hdc, old);
                y += list_row_height();
            }
        });

        if let Some(buffer) = &buffer {
            let _ = BitBlt(
                window_dc,
                rc.left,
                rc.top,
                rc.right - rc.left,
                rc.bottom - rc.top,
                Some(buffer.dc),
                0,
                0,
                SRCCOPY,
            );
        }
        log_paint_gdi("list", pt0.elapsed().as_secs_f64() * 1000.0);
        release_paint_buffer(buffer.take());
        let _ = EndPaint(hwnd, &ps);
    }
}

/// 列表选中行的底色。
pub const LIST_HIGHLIGHT_RGB: u32 = 0x00E8F0F8;

pub fn paint_doc(
    hwnd: HWND,
    chrome: Option<&Chrome<'_>>,
    content: Option<RECT>,
    rows: &[DocRow<'_>],
) {
    paint_doc_with(hwnd, chrome, content, rows, None);
}

/// `paint_doc` 的实现版本，多一个"叠加层"回调（目录栏、右侧滚动条）。
///
/// 1.5.2：叠加层也画进**同一块**内存 DC，整窗一次性 `BitBlt` 上屏。
/// 之前叠加层是自己 `GetDC` 再画，正文和侧栏是两次上屏，滚动时侧栏会闪。
pub fn paint_doc_with(
    hwnd: HWND,
    chrome: Option<&Chrome<'_>>,
    content: Option<RECT>,
    rows: &[DocRow<'_>],
    overlay: Option<&dyn Fn(HDC, RECT)>,
) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let window_dc: HDC = BeginPaint(hwnd, &mut ps);
        if window_dc.0.is_null() {
            let _ = EndPaint(hwnd, &ps);
            return;
        }
        let rc = client_rect(hwnd);
        let pt0 = std::time::Instant::now();
        // 双缓冲（1.5.1）：整屏先画进内存 DC，最后一次性 BitBlt 上屏。
        let mut buffer = acquire_paint_buffer(window_dc, rc);
        let hdc: HDC = buffer.as_ref().map_or(window_dc, |b| b.dc);
        SetBkMode(hdc, TRANSPARENT);

        let brush = CreateSolidBrush(COLORREF(TEXT_BACKGROUND_RGB));
        FillRect(hdc, &rc, brush);
        let _ = DeleteObject(brush.into());

        // 顶栏由这里一起画（同一块 DC，省一次 BeginPaint）；内容区可以被壳层收窄
        // （Markdown 的目录栏、右侧滚动条都要从内容区里让出位置）。
        let after_chrome = match chrome {
            Some(spec) => paint_chrome(hdc, rc, spec),
            None => rc,
        };
        let content = content.unwrap_or(after_chrome);

        let mut y = content.top + text_padding_top();

        // 有任何一行带行号，就给全体正文空出一条行号栏（否则正文宽度会随滚动位置跳动）。
        let gutter = if rows.iter().any(|row| row.number.is_some()) {
            text_gutter()
        } else {
            0
        };
        let text_left = content.left + 12 + gutter;
        let text_right = content.right - 24;

        FONTS.with(|cell| {
            let mut set = cell.borrow_mut();
            for row in rows {
                // 行底色（Markdown 代码块/引用块）：整行一条色带，不覆盖行号栏。
                if let Some(rgb) = row.bg {
                    let band = RECT {
                        left: text_left - 8,
                        top: y,
                        right: text_right + 8,
                        bottom: y + line_height(),
                    };
                    let brush = CreateSolidBrush(COLORREF(rgb));
                    FillRect(hdc, &band, brush);
                    let _ = DeleteObject(brush.into());
                }
                // 行号：右对齐画在行号栏里，和样式无关（一眼能定位第几行）。
                if let Some(number) = row.number {
                    let font = set.get(TextStyle::LineNumber);
                    let old = SelectObject(hdc, font.into());
                    SetTextColor(hdc, COLORREF(TextStyle::LineNumber.color()));
                    let mut label: Vec<u16> = number.to_string().encode_utf16().collect();
                    let mut rect = RECT {
                        left: content.left + 24,
                        top: y,
                        right: content.left + 24 + gutter - 10,
                        bottom: y + line_height(),
                    };
                    DrawTextW(
                        hdc,
                        &mut label,
                        &raw mut rect,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
                    );
                    SelectObject(hdc, old);
                }
                // 坑：windows-rs 的 `DrawTextW` 用 `lpchtext.len()` 当 cchText，空切片会传
                // cchText=0 并把一个悬垂指针交给 user32 —— user32 随即 wcslen 那段内存，
                // 直接 0xC0000005（实测崩在 user32.dll+0x1f16b，经 DispatchMessageW 进来）。
                // 空行只占高度、不画字。
                let blank = row.spans.iter().all(|span| span.text.is_empty());
                if blank {
                    y += line_height();
                    continue;
                }
                // 逐段画：先量宽度决定下一段的起点，再 DrawTextW（默认会裁到 rect，
                // 所以超长的代码行到右边界自动断掉，不会画到窗口外面）。
                let mut x = text_left;
                for span in row.spans {
                    if span.text.is_empty() {
                        continue;
                    }
                    if x >= text_right {
                        break;
                    }
                    let font = set.get(span.style);
                    let old = SelectObject(hdc, font.into());
                    SetTextColor(hdc, COLORREF(span.style.color()));

                    let mut buf: Vec<u16> = span.text.encode_utf16().collect();
                    let mut size = SIZE::default();
                    let _ = GetTextExtentPoint32W(hdc, &buf, &mut size);
                    let mut rect = RECT {
                        left: x,
                        top: y,
                        right: text_right,
                        bottom: y + line_height(),
                    };
                    DrawTextW(
                        hdc,
                        &mut buf,
                        &raw mut rect,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
                    );
                    SelectObject(hdc, old);
                    x += size.cx.max(0);
                }
                y += line_height();
            }
        });

        // 叠加层（目录栏/滚动条）也画在这块内存 DC 上，然后整窗一次性上屏。
        if let Some(draw) = overlay {
            draw(hdc, content);
        }
        // 双缓冲（1.5.1）：把这块内存 DC 的内容一次性上屏。
        // 以前是"正文用 BeginPaint 画一次、目录/滚动条再用 GetDC 画一次"，两次上屏之间
        // 就是用户看到的频闪；建不起缓冲时这里退化成直画（buffer 为 None）。
        if let Some(buffer) = &buffer {
            let _ = BitBlt(
                window_dc,
                rc.left,
                rc.top,
                rc.right - rc.left,
                rc.bottom - rc.top,
                Some(buffer.dc),
                0,
                0,
                SRCCOPY,
            );
        }
        log_paint_gdi("doc", pt0.elapsed().as_secs_f64() * 1000.0);
        release_paint_buffer(buffer.take());
        let _ = EndPaint(hwnd, &ps);
    }
}

/// 画一组单段样式的文本行（Markdown/文本预览走这里）。
///
/// 只是 `paint_doc` 的薄包装：**全程序只有一条文本绘制路径**，
/// 免得"折行时量的字体"和"绘制时用的字体"哪天又对不上。
pub fn paint_styled(
    hwnd: HWND,
    chrome: Option<&Chrome<'_>>,
    content: Option<RECT>,
    rows: &[StyledRow<'_>],
) {
    paint_styled_with(hwnd, chrome, content, rows, None);
}

/// `paint_styled` 的实现版本，多一个叠加层回调（目录/滚动条画在同一块内存 DC 上）。
pub fn paint_styled_with(
    hwnd: HWND,
    chrome: Option<&Chrome<'_>>,
    content: Option<RECT>,
    rows: &[StyledRow<'_>],
    overlay: Option<&dyn Fn(HDC, RECT)>,
) {
    let mut spans: Vec<Vec<Span<'_>>> = Vec::with_capacity(rows.len());
    for row in rows {
        spans.push(if row.text.is_empty() {
            Vec::new()
        } else {
            vec![Span {
                text: row.text,
                style: row.style,
            }]
        });
    }
    let docs: Vec<DocRow<'_>> = rows
        .iter()
        .zip(spans.iter())
        .map(|(row, spans)| DocRow {
            number: row.number,
            spans: spans.as_slice(),
            bg: row.bg,
        })
        .collect();
    paint_doc_with(hwnd, chrome, content, &docs, overlay);
}

/// 画若干行普通文本（占位内容、纯文本预览都走这里）。
///
/// 只是 `paint_styled` 的薄包装：**全程序只有一条文本绘制路径**，
/// 免得"折行时量的字体"和"绘制时用的字体"哪天又对不上。
pub fn paint_text(hwnd: HWND, chrome: Option<&Chrome<'_>>, content: Option<RECT>, lines: &[&str]) {
    let rows: Vec<StyledRow<'_>> = lines
        .iter()
        .map(|text| StyledRow {
            number: None,
            text,
            style: TextStyle::Normal,
            bg: None,
        })
        .collect();
    paint_styled_with(hwnd, chrome, content, &rows, None);
}

/// `paint_text` 的叠加层版本（占位文字 + 目录/滚动条一次上屏）。
pub fn paint_text_with(
    hwnd: HWND,
    chrome: Option<&Chrome<'_>>,
    content: Option<RECT>,
    lines: &[&str],
    overlay: Option<&dyn Fn(HDC, RECT)>,
) {
    let rows: Vec<StyledRow<'_>> = lines
        .iter()
        .map(|text| StyledRow {
            number: None,
            text,
            style: TextStyle::Normal,
            bg: None,
        })
        .collect();
    paint_styled_with(hwnd, chrome, content, &rows, overlay);
}

/// 文本测量器：持有一个 DC 与 GDI 字体，量完自动还原。
///
/// 折行需要知道"这段文字有多宽"，而宽度依赖当前字体与 DPI —— 只能问 GDI。
/// 每次 `measure` 都 `GetDC`/`ReleaseDC` 太贵（一次重画要量几十上百行），
/// 所以这里把 DC 抓一次、用完 Drop 还原。
pub struct Measurer {
    hwnd: HWND,
    hdc: HDC,
    old: HGDIOBJ,
    /// 当前这个 DC 里选中的是哪个样式的字体（换样式才重选，避免每行 SelectObject）。
    current: std::cell::Cell<usize>,
}

impl Measurer {
    /// 拿不到 DC 就返回 `None`（调用方退回"不折行"）。
    pub fn new(hwnd: HWND) -> Option<Self> {
        unsafe {
            let hdc = GetDC(Some(hwnd));
            if hdc.0.is_null() {
                return None;
            }
            // 必须与 paint_styled 用的是同一套字体/方式，否则量的宽度和画的宽度不一致
            let font = FONTS.with(|cell| cell.borrow_mut().get(TextStyle::Normal));
            let old = SelectObject(hdc, font.into());
            Some(Self {
                hwnd,
                hdc,
                old,
                current: std::cell::Cell::new(TextStyle::Normal.as_index()),
            })
        }
    }

    /// 这段文字在普通样式下的像素宽度。
    pub fn width(&self, text: &str) -> u32 {
        self.width_styled(TextStyle::Normal, text)
    }

    /// 这段文字在指定样式下的像素宽度（Markdown 折行必须按各自字体量）。
    pub fn width_styled(&self, style: TextStyle, text: &str) -> u32 {
        if text.is_empty() {
            return 0;
        }
        self.select_style(style);
        let wide = crate::wide::to_wide(text);
        let body = &wide[..wide.len() - 1]; // 去掉结尾的 NUL
        let mut size = SIZE::default();
        unsafe {
            if GetTextExtentPoint32W(self.hdc, body, &mut size).as_bool() {
                size.cx.max(0) as u32
            } else {
                0
            }
        }
    }

    /// 一次问 GDI："这段文字在 `width` 内能放下多少字节"（返回**最长可放前缀的 UTF-8 字节长度**）。
    ///
    /// `GetTextExtentExPointW` 一次调用就把能放下的字符数放进 `lpnFit`，省掉折行二分里
    /// 那十几次 `GetTextExtentPoint32W`。口径与 [`Self::width_styled`] 完全一致
    /// （同一个 HDC、同一套字体、同样的 `<=` 边界），否则折行会差一个字符。
    pub fn fit_width_styled(&self, style: TextStyle, text: &str, width: u32) -> u32 {
        if text.is_empty() {
            return 0;
        }
        self.select_style(style);
        let wide = crate::wide::to_wide(text);
        let body = &wide[..wide.len() - 1]; // 去掉结尾的 NUL
        let mut fit: i32 = 0;
        let mut size = SIZE::default();
        unsafe {
            // `lpndx = NULL`：不要每字符宽度数组，只要"能放几个"。
            if GetTextExtentExPointW(
                self.hdc,
                PCWSTR(body.as_ptr()),
                body.len() as i32,
                width as i32,
                Some(&mut fit),
                None,
                &mut size,
            )
            .as_bool()
            {
                // `fit` 是 UTF-16 码元数，换算成 UTF-8 字节长度（emoji 是 2 码元 / 4 字节）。
                let target = fit.max(0) as usize;
                let mut units = 0usize;
                for (idx, ch) in text.char_indices() {
                    if units >= target {
                        return idx as u32;
                    }
                    units += ch.len_utf16();
                }
                text.len() as u32
            } else {
                0
            }
        }
    }

    /// 把 DC 里选中的字体切成 `style`（换样式才 `SelectObject`，避免每行重选）。
    fn select_style(&self, style: TextStyle) {
        let idx = style.as_index();
        if self.current.get() != idx {
            let font = FONTS.with(|cell| cell.borrow_mut().get(style));
            unsafe {
                SelectObject(self.hdc, font.into());
            }
            self.current.set(idx);
        }
    }
}

impl Drop for Measurer {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.hdc, self.old);
            let _ = ReleaseDC(Some(self.hwnd), self.hdc);
        }
    }
}
