//! 顶栏与按钮 —— 从 window.rs 纯搬家拆出（2026-11-04）。
//!
//! 每块只多一行 `use super::*;`，函数体一字未改。

use super::*;

/// 顶栏高度（像素）。
pub fn chrome_height() -> i32 {
    dpi::px(30)
}

/// 顶栏底色：黑色。浅色内容区上面压一条黑边，一眼就是"这是预览器的框"。
pub const CHROME_BG_RGB: u32 = 0x0020_2020;

/// 顶栏主文字色。
pub const CHROME_FG_RGB: u32 = 0x00E8_E8E8;

/// 顶栏里"悬停提示"气泡的底色。
pub const CHROME_HINT_RGB: u32 = 0x0028_2828;

/// 顶栏按钮的悬停底色。
pub const CHROME_HOVER_RGB: u32 = 0x003C_3C3C;

/// 页码输入框的底色与边框。
pub const CHROME_BOX_RGB: u32 = 0x0032_3232;

pub const CHROME_BOX_BORDER_RGB: u32 = 0x005C_5C5C;

/// "阻止关闭"按钮开启后的底色（暗琥珀，和悬停灰一眼区分）。
pub const CHROME_PIN_RGB: u32 = 0x0050_4520;

/// 顶栏右侧每个按钮的宽度。
pub fn chrome_button_w() -> i32 {
    dpi::px(38)
}

/// PDF 页码框宽度。
pub fn page_box_w() -> i32 {
    dpi::px(84)
}

/// 顶栏上可以点的三个按钮。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChromeButton {
    /// 用系统默认程序打开这个文件（`ShellExecute`）。
    External,
    /// 关闭预览（备用入口：Esc 之外的鼠标路径）。
    Close,
    /// 阻止关闭（1.0.10）：按下后空格/Esc 都关不掉这个窗口，且**不能取消**
    /// （用户原话"不能取消阻止关闭"——防误触，开着就是开着，关窗可以走 × 或托盘）。
    Pin,
}

/// PDF 页码框：正常显示 `current / total`，`editing` 有值时显示正在输入的数字。
pub struct PageBox<'a> {
    pub current: u32,
    pub total: u32,
    pub editing: Option<&'a str>,
}

/// 顶栏内容（四种格式共用同一套结构）。
pub struct Chrome<'a> {
    /// 左侧标题（文件名）。
    pub title: &'a str,
    /// PDF 才有的页码框。
    pub page: Option<PageBox<'a>>,
    /// 鼠标停在哪个按钮上（决定按钮底色）。
    pub hover: Option<ChromeButton>,
    /// 悬停提示文字（画在按钮下方的小气泡里）。
    pub hint: Option<&'a str>,
    /// 一次性提示（1.7.4）：画在顶栏右侧的气泡里，由壳层计时清掉。
    /// 目前用于"已是最后一页/第一页"这类翻页反馈（系统预览处理器自己没有这个概念）。
    pub toast: Option<&'a str>,
    /// "阻止关闭"是否已开启（开启了就给按钮上高亮底色，用户能看出窗口被锁住）。
    pub pinned: bool,
}

/// 顶栏各块的位置。绘制与命中测试都从 [`chrome_rects`] 取，避免两处各算一遍。
pub struct ChromeRects {
    /// 扣掉顶栏后的内容区。
    pub content: RECT,
    pub page: RECT,
    pub external: RECT,
    pub close: RECT,
    pub pin: RECT,
}

/// 由客户区算出顶栏各块的位置（纯函数，便于单测）。
pub fn chrome_rects(rc: RECT) -> ChromeRects {
    let close = RECT {
        left: rc.right - chrome_button_w(),
        top: rc.top,
        right: rc.right,
        bottom: rc.top + chrome_height(),
    };
    let external = RECT {
        left: close.left - chrome_button_w(),
        right: close.left,
        top: rc.top,
        bottom: rc.top + chrome_height(),
    };
    // 页码框在最左、紧跟"阻止关闭"（锁）；两者都离 × 远一点，避免点错。
    let pin = RECT {
        left: external.left - chrome_button_w(),
        top: rc.top,
        right: external.left,
        bottom: rc.top + chrome_height(),
    };
    let page = RECT {
        left: pin.left - 10 - page_box_w(),
        top: rc.top + 5,
        right: pin.left - 10,
        bottom: rc.top + chrome_height() - 5,
    };
    ChromeRects {
        content: RECT {
            left: rc.left,
            top: rc.top + chrome_height(),
            right: rc.right,
            bottom: rc.bottom,
        },
        page,
        external,
        close,
        pin,
    }
}

/// 只重画顶栏那一条，**内容区一个像素都不碰**（1.7.6）。
///
/// 用途：换文件时把内容区"冻"在上一帧，等新内容真的画出来再覆盖。中间那一两百毫秒
/// （Office 预览要跨进程 `DoPreview`）如果把整窗重画一遍，用户会看到"占位文字页"闪一下——
/// 这就是用户说的"Office 切换割裂感最重"。
///
/// 之所以"不碰就冻得住"：窗口类的背景刷是空的，`WM_ERASEBKGND` 又返回 1（不擦除），
/// 所以没画到的像素保持原样（位图通道当年就是靠这个避免闪烁）。
pub fn paint_chrome_strip(hwnd: HWND, chrome: &Chrome<'_>) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        if hdc.0.is_null() {
            let _ = EndPaint(hwnd, &ps);
            return;
        }
        let rc = client_rect(hwnd);
        paint_chrome(hdc, rc, chrome);
        let _ = EndPaint(hwnd, &ps);
    }
}

/// 画顶栏，返回扣掉顶栏后的内容区。
pub fn paint_chrome(hdc: HDC, rc: RECT, chrome: &Chrome<'_>) -> RECT {
    let rects = chrome_rects(rc);
    let band = RECT {
        left: rc.left,
        top: rc.top,
        right: rc.right,
        bottom: rc.top + chrome_height(),
    };
    fill_solid(hdc, band, CHROME_BG_RGB);

    // 标题右边界：有页码框才让位，没有就一路顶到"锁"按钮左边 12px。
    // 1.7.4 修：以前**无条件**按页码框的位置收，非 PDF（Office/图片/文本）白扣
    // 94+10+12 ≈ 116px —— 窄窗里长文件名必被 `DT_END_ELLIPSIS` 截成半截，
    // 240px 宽的窗口甚至算出反向矩形（`right < left`，DrawTextW 什么都不画）。
    let title_right = if chrome.page.is_some() {
        rects.page.left - 12
    } else {
        rects.pin.left - 12
    };
    let title = RECT {
        left: rc.left + 14,
        top: band.top,
        // 再窄也留 10px：反向矩形是"整条标题消失"，比截断难查得多。
        right: title_right.max(rc.left + 24),
        bottom: band.bottom,
    };
    draw_line(
        hdc,
        title,
        chrome.title,
        TextStyle::Normal,
        CHROME_FG_RGB,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
    );

    if let Some(page) = &chrome.page {
        fill_solid(hdc, rects.page, CHROME_BOX_RGB);
        frame_solid(hdc, rects.page, CHROME_BOX_BORDER_RGB);
        let label = match page.editing {
            // 正在输入：光标用 `|` 顶替，用户一眼看出"这里可以打字"。
            Some(text) => format!("{text}|"),
            None => format!("{} / {}", page.current, page.total),
        };
        draw_line(
            hdc,
            rects.page,
            &label,
            TextStyle::Code,
            CHROME_FG_RGB,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
    }

    for (button, rect, glyph) in [
        (ChromeButton::Pin, rects.pin, "锁"),
        (ChromeButton::External, rects.external, "↗"),
        (ChromeButton::Close, rects.close, "×"),
    ] {
        // 锁按钮的底色表示"锁定状态"而不是悬停（开着就一直亮着）。
        if chrome.hover == Some(button) {
            fill_solid(hdc, rect, CHROME_HOVER_RGB);
        } else if button == ChromeButton::Pin && chrome.pinned {
            fill_solid(hdc, rect, CHROME_PIN_RGB);
        }
        draw_line(
            hdc,
            rect,
            glyph,
            TextStyle::Normal,
            CHROME_FG_RGB,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
    }

    // 悬停提示：贴在按钮下方，压在内容区上。自己做是因为原生 tooltip 要额外的
    // TOOLTIPS_CLASS 窗口与消息转发，而我们窗口本来就不激活（tooltip 也不一定弹得出来）。
    if let (Some(button), Some(hint)) = (chrome.hover, chrome.hint) {
        let anchor = match button {
            ChromeButton::External => rects.external,
            ChromeButton::Close => rects.close,
            ChromeButton::Pin => rects.pin,
        };
        let text_w = measure_line(hdc, hint, TextStyle::Normal);
        let width = text_w + 18;
        let left = (anchor.right - width).max(rc.left + 4);
        let bubble = RECT {
            left,
            top: rc.top + chrome_height() + 4,
            right: left + width,
            bottom: rc.top + chrome_height() + 26,
        };
        fill_solid(hdc, bubble, CHROME_HINT_RGB);
        frame_solid(hdc, bubble, CHROME_BOX_BORDER_RGB);
        draw_line(
            hdc,
            bubble,
            hint,
            TextStyle::Normal,
            CHROME_FG_RGB,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
    }

    // 一次性提示（1.7.4）：画在顶栏右侧、贴着"锁"左边的气泡，1.2s 后由壳层清掉。
    // 为什么不复用上面那个悬停气泡：那个贴在按钮下方、会压在内容上；翻页提示
    // 需要"扫一眼就有"，放在顶栏里既不挡内容也不用等鼠标。
    if let Some(toast) = chrome.toast.filter(|t| !t.is_empty()) {
        let text_w = measure_line(hdc, toast, TextStyle::Normal);
        let width = text_w + 18;
        let right = rects.pin.left - 6;
        let bubble = RECT {
            // 允许盖住标题右端：标题本来就是"看一眼就知道是什么文件"，
            // 提示只有 1.2s，压一下不影响；不让盖的话窄窗里会挤成 0 宽（踩过）。
            left: (right - width).max(rc.left + 60),
            top: rc.top + 5,
            right,
            bottom: rc.top + chrome_height() - 5,
        };
        fill_solid(hdc, bubble, CHROME_BOX_RGB);
        frame_solid(hdc, bubble, CHROME_BOX_BORDER_RGB);
        draw_line(
            hdc,
            bubble,
            toast,
            TextStyle::Normal,
            CHROME_FG_RGB,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
    }

    rects.content
}
