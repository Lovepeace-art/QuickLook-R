//! 设置面板 —— 从 window.rs 纯搬家拆出（2026-11-04）。
//!
//! 每块只多一行 `use super::*;`，函数体一字未改。

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

/// 设置页高亮行的底色（比白色背景略深的浅灰，1.0.8 起浅色主题）。
pub const SETTINGS_HIGHLIGHT_RGB: u32 = 0x00E8_E8E8;

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

/// 画一整页设置内容。
///
/// 行从顶部开始、每行 [`settings_row_height()`]，与 `qlr-app` 的命中测试共用同一组
/// 常量（见 `settings::row_at`），保证"看到的行"和"点到的行"永远是同一行。
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
                // 空行只占高度：`DrawTextW` 收到空切片会传 cchText=0 并 wcslen 悬垂指针。
                if !row.left.is_empty() {
                    let font = set.get(row.style);
                    let old = SelectObject(hdc, font.into());
                    SetTextColor(hdc, COLORREF(row.style.color()));
                    let mut buf: Vec<u16> = row.left.encode_utf16().collect();
                    // 右边没有值（分类标题、说明行、底部提示）就把整行让给左列，
                    // 否则说明文字会被空着的值列挤掉半句（"按住它移动鼠标 = 放大镜；按住它…"）。
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
