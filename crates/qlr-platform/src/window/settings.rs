//! 设置面板 —— 从 window.rs 纯搬家拆出（2026-11-04）。
//!
//! 每块只多一行 `use super::*;`，函数体一字未改。
//!
//! 2026-11-23：新增左「分类侧栏 + 右侧单页」两栏布局（[`paint_settings_two_pane`]），
//! 解决"行数太多、整窗比屏幕还高、底部设置项够不到"的显示问题。

use super::*;

/// 设置页的行高（像素）。比预览页的 24 大一点：设置项要能用鼠标点，热区太小难点。
pub fn settings_row_height() -> i32 {
    dpi::px(28)
}

/// 设置页内容距客户区顶部的留白。
pub fn settings_padding_top() -> i32 {
    dpi::px(18)
}

/// 设置页左右留白。
pub fn settings_padding_x() -> i32 {
    dpi::px(28)
}

/// 左侧分类栏宽度。
pub fn settings_sidebar_width() -> i32 {
    dpi::px(190)
}

/// 左侧分类栏每个条目的高度。
pub fn settings_sidebar_item_height() -> i32 {
    dpi::px(38)
}

/// 左侧分类栏顶部留白。
pub fn settings_sidebar_padding_top() -> i32 {
    dpi::px(16)
}

/// 右侧页面标题区的高度（标题画在这一条里，正文从它下面开始）。
pub fn settings_title_height() -> i32 {
    dpi::px(44)
}

/// 设置页高亮行的底色（比白色背景略深的浅灰，1.0.8 起浅色主题）。
pub const SETTINGS_HIGHLIGHT_RGB: u32 = 0x00E8_E8E8;

/// 左栏背景（比白色略深的浅灰）。
const SIDEBAR_BG_RGB: u32 = 0x00FAF8F7;
/// 左栏选中项底色（浅蓝）。
const SIDEBAR_SEL_BG_RGB: u32 = 0x00FEF0E8;
/// 左栏选中项左侧强调条（品牌蓝 #2F6FEB → COLORREF 0x00EB6F2F）。
const SIDEBAR_ACCENT_RGB: u32 = 0x00EB6F2F;
/// 左右分栏之间的细分隔线。
const SIDEBAR_DIVIDER_RGB: u32 = 0x00E6E4E3;

/// 设置页的一行：左边是名称，右边是值（右对齐）。
///
/// 两列而不是拼成一个字符串：值要能被眼睛快速扫到、也要能单独上色，
/// 拼接后就没法右对齐了。
pub struct SettingsRow<'a> {
    pub left: &'a str,
    pub right: &'a str,
    pub style: TextStyle,
    /// 值那一列用的样式（通常是 `Code`：等宽、淡青，像"键帽"）。
    pub value_style: TextStyle,
    /// 是否高亮（当前选中/正在改键的那一行）。
    pub highlight: bool,
}

/// 左侧分类栏的一项。
pub struct SidebarItem<'a> {
    pub text: &'a str,
    pub selected: bool,
}

/// 画一整页设置内容（单栏旧版，保留给旧测试/兼容）。
///
/// 行从顶部开始、每行 [`settings_row_height()`]，与 `qlr-app` 的命中测试共用同一组
/// 常量，保证"看到的行"和"点到的行"永远是同一行。
pub fn paint_settings(hwnd: HWND, rows: &[SettingsRow<'_>]) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc: HDC = BeginPaint(hwnd, &mut ps);
        if hdc.0.is_null() {
            let _ = EndPaint(hwnd, &ps);
            return;
        }
        SetBkMode(hdc, TRANSPARENT);

        let rc = client_rect(hwnd);
        let background = CreateSolidBrush(COLORREF(TEXT_BACKGROUND_RGB));
        FillRect(hdc, &rc, background);
        let _ = DeleteObject(background.into());

        let mut y = rc.top + settings_padding_top();
        let highlight = CreateSolidBrush(COLORREF(SETTINGS_HIGHLIGHT_RGB));

        FONTS.with(|cell| {
            let mut set = cell.borrow_mut();
            for row in rows {
                if row.highlight && !row.left.is_empty() {
                    let band = RECT {
                        left: rc.left + 8,
                        top: y,
                        right: rc.right - 8,
                        bottom: y + settings_row_height(),
                    };
                    FillRect(hdc, &band, highlight);
                }
                if !row.left.is_empty() {
                    let font = set.get(row.style);
                    let old = SelectObject(hdc, font.into());
                    SetTextColor(hdc, COLORREF(row.style.color()));
                    let mut buf: Vec<u16> = row.left.encode_utf16().collect();
                    let right_edge = if row.right.is_empty() {
                        rc.right - settings_padding_x()
                    } else {
                        rc.right - settings_padding_x() - 180
                    };
                    let mut rect = RECT {
                        left: rc.left + settings_padding_x(),
                        top: y,
                        right: right_edge,
                        bottom: y + settings_row_height(),
                    };
                    DrawTextW(
                        hdc,
                        &mut buf,
                        &raw mut rect,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
                    );
                    SelectObject(hdc, old);
                }
                if !row.right.is_empty() {
                    let font = set.get(row.value_style);
                    let old = SelectObject(hdc, font.into());
                    SetTextColor(hdc, COLORREF(row.value_style.color()));
                    let mut buf: Vec<u16> = row.right.encode_utf16().collect();
                    let mut rect = RECT {
                        left: rc.right - settings_padding_x() - 172,
                        top: y,
                        right: rc.right - settings_padding_x(),
                        bottom: y + settings_row_height(),
                    };
                    DrawTextW(
                        hdc,
                        &mut buf,
                        &raw mut rect,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
                    );
                    SelectObject(hdc, old);
                }
                y += settings_row_height();
            }
        });
        let _ = DeleteObject(highlight.into());

        let _ = EndPaint(hwnd, &ps);
    }
}

/// 画「左侧分类栏 + 右侧单页」两栏设置界面（2026-11-23）。
///
/// 每次只显示**一个分类**的若干行（调用方只传当前分类的行），所以整窗高度不再随
/// 设置项总数线性增长，不会超出屏幕；分类间切换由 `qlr-app` 的命中测试负责。
pub fn paint_settings_two_pane(
    hwnd: HWND,
    sidebar: &[SidebarItem<'_>],
    title: &str,
    rows: &[SettingsRow<'_>],
) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc: HDC = BeginPaint(hwnd, &mut ps);
        if hdc.0.is_null() {
            let _ = EndPaint(hwnd, &ps);
            return;
        }
        SetBkMode(hdc, TRANSPARENT);

        let rc = client_rect(hwnd);
        // 整窗白底
        let background = CreateSolidBrush(COLORREF(TEXT_BACKGROUND_RGB));
        FillRect(hdc, &rc, background);
        let _ = DeleteObject(background.into());

        // 左栏底色 + 分栏竖线
        let sw = settings_sidebar_width();
        let side = RECT {
            left: rc.left,
            top: rc.top,
            right: rc.left + sw,
            bottom: rc.bottom,
        };
        let sidebar_bg = CreateSolidBrush(COLORREF(SIDEBAR_BG_RGB));
        FillRect(hdc, &side, sidebar_bg);
        let _ = DeleteObject(sidebar_bg.into());
        let divider = CreateSolidBrush(COLORREF(SIDEBAR_DIVIDER_RGB));
        let vline = RECT {
            left: rc.left + sw - 1,
            top: rc.top,
            right: rc.left + sw,
            bottom: rc.bottom,
        };
        FillRect(hdc, &vline, divider);
        let _ = DeleteObject(divider.into());

        FONTS.with(|cell| {
            let mut set = cell.borrow_mut();

            // ---- 左栏条目 ----
            let ih = settings_sidebar_item_height();
            let mut y = rc.top + settings_sidebar_padding_top();
            let sel_bg = CreateSolidBrush(COLORREF(SIDEBAR_SEL_BG_RGB));
            let accent = CreateSolidBrush(COLORREF(SIDEBAR_ACCENT_RGB));
            for item in sidebar {
                let band = RECT {
                    left: rc.left,
                    top: y,
                    right: rc.left + sw - 1,
                    bottom: y + ih,
                };
                if item.selected {
                    FillRect(hdc, &band, sel_bg);
                    let bar = RECT {
                        left: rc.left,
                        top: y + dpi::px(7),
                        right: rc.left + dpi::px(3),
                        bottom: y + ih - dpi::px(7),
                    };
                    FillRect(hdc, &bar, accent);
                }
                if !item.text.is_empty() {
                    let font = set.get(TextStyle::Normal);
                    let old = SelectObject(hdc, font.into());
                    let color = if item.selected {
                        0x0020_2020
                    } else {
                        0x005A_5A5A
                    };
                    SetTextColor(hdc, COLORREF(color));
                    let mut buf: Vec<u16> = item.text.encode_utf16().collect();
                    let mut rect = RECT {
                        left: rc.left + settings_padding_x() / 2 + dpi::px(4),
                        top: y,
                        right: rc.left + sw - settings_padding_x() / 2,
                        bottom: y + ih,
                    };
                    DrawTextW(
                        hdc,
                        &mut buf,
                        &raw mut rect,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
                    );
                    SelectObject(hdc, old);
                }
                y += ih;
            }
            let _ = DeleteObject(sel_bg.into());
            let _ = DeleteObject(accent.into());

            // ---- 右侧标题 ----
            let cx = rc.left + sw + settings_padding_x();
            let cy = rc.top + settings_padding_top();
            let right_full = rc.right - settings_padding_x();
            if !title.is_empty() {
                let font = set.get(TextStyle::Heading3);
                let old = SelectObject(hdc, font.into());
                SetTextColor(hdc, COLORREF(TextStyle::Heading3.color()));
                let mut buf: Vec<u16> = title.encode_utf16().collect();
                let mut rect = RECT {
                    left: cx,
                    top: cy,
                    right: right_full,
                    bottom: cy + settings_title_height(),
                };
                DrawTextW(
                    hdc,
                    &mut buf,
                    &raw mut rect,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
                );
                SelectObject(hdc, old);
            }

            // ---- 右侧行 ----
            let mut ry = cy + settings_title_height();
            let highlight = CreateSolidBrush(COLORREF(SETTINGS_HIGHLIGHT_RGB));
            for row in rows {
                if row.highlight && !row.left.is_empty() {
                    let band = RECT {
                        left: cx - dpi::px(8),
                        top: ry,
                        right: right_full,
                        bottom: ry + settings_row_height(),
                    };
                    FillRect(hdc, &band, highlight);
                }
                if !row.left.is_empty() {
                    let font = set.get(row.style);
                    let old = SelectObject(hdc, font.into());
                    SetTextColor(hdc, COLORREF(row.style.color()));
                    let mut buf: Vec<u16> = row.left.encode_utf16().collect();
                    let right_edge = if row.right.is_empty() {
                        right_full
                    } else {
                        right_full - 180
                    };
                    let mut rect = RECT {
                        left: cx,
                        top: ry,
                        right: right_edge,
                        bottom: ry + settings_row_height(),
                    };
                    DrawTextW(
                        hdc,
                        &mut buf,
                        &raw mut rect,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
                    );
                    SelectObject(hdc, old);
                }
                if !row.right.is_empty() {
                    let font = set.get(row.value_style);
                    let old = SelectObject(hdc, font.into());
                    SetTextColor(hdc, COLORREF(row.value_style.color()));
                    let mut buf: Vec<u16> = row.right.encode_utf16().collect();
                    let mut rect = RECT {
                        left: right_full - 172,
                        top: ry,
                        right: right_full,
                        bottom: ry + settings_row_height(),
                    };
                    DrawTextW(
                        hdc,
                        &mut buf,
                        &raw mut rect,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
                    );
                    SelectObject(hdc, old);
                }
                ry += settings_row_height();
            }
            let _ = DeleteObject(highlight.into());
        });

        let _ = EndPaint(hwnd, &ps);
    }
}
