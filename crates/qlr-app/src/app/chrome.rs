//! 顶栏 / 目录 / 滚动条 / 提示气泡 / 放大镜的绘制与命中，以及 `repaint` 这条总绘制通道。
//!
//! 像素一律走 `qlr_platform::window` 的原语，这里只决定"画在哪、画什么"。

use super::*;

/// 内容画完之后再叠一层放大镜（必须在同一次 `WM_PAINT` 内、内容之后调用）。
pub(crate) fn paint_lens_if_any(hwnd: HWND) {
    let Some(center) = with_state(|s| s.lens).flatten() else {
        return;
    };
    let zoom = with_state(|s| s.lens_zoom).unwrap_or(LENS_DEFAULT_ZOOM);
    // 镜头整体留在窗口里：靠边时把镜头往里推，而不是让 GDI 裁掉一半。
    let rc = window::client_rect(hwnd);
    let (r, cw, ch) = (
        window::lens_radius(),
        (rc.right - rc.left).max(1),
        (rc.bottom - rc.top).max(1),
    );
    let (x, y) = (
        center.0.clamp(r, (cw - r).max(r)),
        center.1.clamp(r, (ch - r).max(r)),
    );
    window::paint_lens(hwnd, (x, y), zoom);
}

/// 渲染层的语义样式 → 平台层的像素样式。
///
/// 两边故意各有一份枚举：渲染层不依赖 windows，平台层不依赖渲染实现。
/// 中间这条转换是唯一耦合点，加新样式时**两边都要改**，编译器会提醒（match 是穷尽的）。
pub(crate) fn to_text_style(style: qlr_render::LineStyle) -> window::TextStyle {
    match style {
        qlr_render::LineStyle::Normal => window::TextStyle::Normal,
        qlr_render::LineStyle::Heading1 => window::TextStyle::Heading1,
        qlr_render::LineStyle::Heading2 => window::TextStyle::Heading2,
        qlr_render::LineStyle::Heading3 => window::TextStyle::Heading3,
        qlr_render::LineStyle::Code => window::TextStyle::Code,
        qlr_render::LineStyle::Quote => window::TextStyle::Quote,
        qlr_render::LineStyle::List => window::TextStyle::List,
        qlr_render::LineStyle::Rule => window::TextStyle::Rule,
        qlr_render::LineStyle::Table => window::TextStyle::Table,
    }
}

/// Markdown 块级底色（1.0.8）：代码块 / 引用块给一条浅灰底，视觉上和正文分开。
///
/// `COLORREF` 是 `0x00BBGGRR`（CONVENTIONS #41）：`#F6F8FA` → `0x00FA_F8F6`。
pub(crate) fn line_bg(style: qlr_render::LineStyle) -> Option<u32> {
    match style {
        qlr_render::LineStyle::Code => Some(0x00FA_F8F6),
        qlr_render::LineStyle::Quote => Some(0x00F7_F7F7),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 顶栏 / 内容区布局（1.0.9：四种格式统一）
// ---------------------------------------------------------------------------

/// 顶栏标题：文件名（不带路径）。
pub(crate) fn file_title() -> String {
    with_state(|s| {
        s.last_path
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().to_string())
    })
    .flatten()
    .unwrap_or_else(|| "QLRust".to_string())
}

/// 顶栏一次性提示（1.7.4）：写进状态并起一个 1.2s 的定时器擦掉。
///
/// 用在"翻页到边界"这种"用户按了但内容不会再动"的场合——不给反馈的话，
/// 用户会以为按键没生效（PPT 以前就是按到底直接切到 PowerPoint 自己的黑屏结束页）。
pub(crate) fn show_toast(hwnd: HWND, text: impl Into<String>) {
    show_toast_ms(hwnd, text, 1200);
}

/// 带时长的提示：[`show_toast`] 的通用版本。
///
/// 翻页到边界那种"按了但内容不再动"的瞬时反馈 1.2 s 够；而"托盘图标没加上、
/// 程序还在后台跑"这种需要读两遍的信息给 5 s，否则用户只看到一闪。
pub(crate) fn show_toast_ms(hwnd: HWND, text: impl Into<String>, ms: u64) {
    let until = Instant::now() + std::time::Duration::from_millis(ms);
    with_state(|s| s.toast = Some((text.into(), until)));
    unsafe {
        let _ = SetTimer(Some(hwnd), TIMER_TOAST, ms as u32, None);
    }
    window::invalidate(hwnd);
}

/// 托盘注册失败后的第一次"可见提示"（1.8.3）。
///
/// 必须在**窗口已经显示出来之后**调用：toast 画在顶栏上，窗口没显示时写进去
/// 谁也看不见，而且定时器一过就被擦掉了。所以 `show_preview` 在每次成功
/// `show_at` 之后都问一次这里，标记被取走之后就不再打扰。
pub(crate) fn notify_tray_failure_once(hwnd: HWND) {
    if !TRAY_FAILURE_NOTICE.swap(false, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    log_warn!(
        SCOPE,
        "托盘图标没加上（见启动日志）：已在预览窗顶栏提示一次，退出用 qlrust.exe --quit"
    );
    show_toast_ms(
        hwnd,
        "托盘图标没加上：后台常驻，qlrust.exe --quit 可退出",
        5000,
    );
}

/// 当前还该显示的那条提示（过期的不返回）。
pub(crate) fn current_toast() -> Option<String> {
    with_state(|s| {
        s.toast
            .as_ref()
            .filter(|(_, until)| Instant::now() < *until)
            .map(|(text, _)| text.clone())
    })
    .flatten()
}

/// 当前内容的滚动位置（0.0–1.0）。文本按行、PDF 按页；放得下就返回 `None`。
///
/// 注意锁的顺序：先取内容（`CONTENT`）、再取状态（`STATE`），**不要**在持有
/// `STATE` 的时候去碰 `CONTENT`——`repaint` 是在 `WM_PAINT` 里跑的，那条路径
/// 已经证明过会重入。
pub(crate) fn scroll_info() -> Option<window::ScrollInfo> {
    let content = get_content()?;
    let (page, pages, scroll, consumed) =
        with_state(|s| (s.page, s.pages, s.scroll, s.wrapped_consumed)).unwrap_or((0, None, 0, 1));
    // PDF：滚动条只看**页数**，和"现在上屏的是占位文字还是真页面"无关。
    // 这一条必须放在 match 之前：`view_size()` 要靠它算出"内容区宽不宽"，
    // 而算渲染尺寸时上屏的往往还是那句"正在渲染…"——按占位文字算出来的
    // "没有滚动条"会让位图按全宽渲染，右边正好被滚动条压掉一条。
    if let Some(total) = pages {
        return window::ScrollInfo::new(page as f32, 1.0, total as f32);
    }
    match &*content {
        Canvas::Text(doc) => {
            let total = doc.lines.len() as f32;
            window::ScrollInfo::new(scroll as f32, consumed.max(1) as f32, total)
        }
        Canvas::Bitmap(_) => {
            let total = pages? as f32;
            window::ScrollInfo::new(page as f32, 1.0, total)
        }
    }
}

/// 内容区布局。绘制与命中测试都从这里取，保证"看到的位置"和"点到的位置"一致。
///
/// 1.8.3 起它就是 `layout::Layout`：几何只在 `layout.rs` 里算一次，这里只负责
/// **把状态翻译成输入**（有没有目录、这一帧画不画滚动条），不再自己切矩形。
pub(crate) type Frame = layout::Layout;

pub(crate) fn frame(hwnd: HWND) -> Frame {
    let client = window::client_rect(hwnd);
    // 问滚动条信息要一块"顶栏以下"的区域（纯函数，代价可忽略）；几何本身交给
    // `layout::from_client` 统一算，这里不切矩形。
    let below = window::chrome_rects(client).content;
    // 目录、缩放、页数一次锁里取完（布局的输入必须是"同一瞬间"的状态）。
    let (has_toc, zoom, pages) =
        with_state(|s| (!s.toc.is_empty(), s.zoom, s.pages)).unwrap_or((false, 1.0, None));
    // 压缩包列表（1.7.3）：内容单位是"展平后的行"，和文本/PDF 的 `scroll_info()` 不是一套，
    // 所以这里单独问一次；命中测试和绘制都读同一个 `Frame`，两边才不会错位。
    let visible_bar = if has_archive_view() {
        archive_scroll_info(below).is_some()
    } else {
        scroll_info().is_some()
    };
    layout::from_client(
        client,
        layout::Opts {
            kind: current_kind().unwrap_or(format::Kind::Unsupported),
            pages,
            zoom,
            toc: has_toc,
            bar: layout::Bar::from_visible(visible_bar),
        },
    )
}

/// 用系统默认程序打开当前文件（顶栏的「在其他应用打开」）。
pub(crate) fn open_external() {
    let path = with_state(|s| s.last_path.clone()).flatten();
    let Some(path) = path else { return };
    let shown = path.to_string_lossy().to_string();
    let ok = window::open_external(&shown);
    log_info!(SCOPE, "在其他应用打开：{shown} ok={ok}");
}

/// 顶栏/目录/滚动条的点击。返回 `true` 表示这次点击已被消费（不要再去拖窗口）。
pub(crate) fn on_chrome_click(hwnd: HWND, lparam: LPARAM) -> bool {
    let (x, y) = client_xy(lparam);
    let client = window::client_rect(hwnd);
    let rects = window::chrome_rects(client);

    if y < rects.content.top {
        // ---- 顶栏 ----
        if point_in(x, y, rects.close) {
            // 现场报障："md 有时候第一次点 × 没反应"。这一行留在 info：
            // 有它就说明点击**确实到了**关闭按钮（窗口却还在 = 收起后又被谁 SHOW 回来，
            // 见 `show_preview` 里那条 HIDE→SHOW 追踪）；没有它就要往别处查
            // （命中区不对 / 被非客户区当成改大小吞掉）。
            log_info!(SCOPE, "顶栏：关闭（{x},{y}）");
            hide_preview();
            return true;
        }
        // 挨着 × 却没点中：记一条，专门用来抓"差几像素"的命中问题。
        // 顶栏 + 内容区都没命中时才会走到，不会刷屏（只有真的贴边点才触发）。
        let near_close = x >= rects.close.left - 6
            && x < rects.close.right + 6
            && y >= rects.close.top - 6
            && y < rects.close.bottom + 6;
        if near_close {
            log_info!(
                SCOPE,
                "顶栏：点击 ({x},{y}) 贴着关闭按钮但没命中，close=({},{})-({},{})",
                rects.close.left,
                rects.close.top,
                rects.close.right,
                rects.close.bottom
            );
        }
        if point_in(x, y, rects.external) {
            open_external();
            return true;
        }
        if point_in(x, y, rects.pin) {
            // 单向锁：点一次就开，**再点也不关**（用户原话"不能取消阻止关闭"）。
            // 防的就是"手忙脚乱时随手又点一下把锁关了"。
            let already = with_state(|s| s.pinned).unwrap_or(false);
            if already {
                log_debug!(SCOPE, "顶栏：阻止关闭已开启（不能取消）");
                window::beep();
            } else {
                with_state(|s| s.pinned = true);
                log_info!(SCOPE, "已开启「阻止关闭」：空格/Esc 关不掉这个窗口");
            }
            window::invalidate(hwnd);
            return true;
        }
        let has_page = with_state(|s| s.pages.is_some()).unwrap_or(false);
        if has_page && point_in(x, y, rects.page) {
            // 点页码框 = 开始输入（窗口不激活，输入靠键盘钩子转发数字键）。
            with_state(|s| s.page_edit = Some(String::new()));
            hotkey::set_capture_digits(true);
            log_debug!(SCOPE, "页码输入：开始");
            window::invalidate(hwnd);
            return true;
        }
        // 点顶栏空白处（1.0.10）：结束页码输入，并且**像正文一样拖窗口**。
        // 用户要求"鼠标左键拖拽窗口的作用域增加到顶栏上"——以前顶栏是"死"的，
        // 想挪窗口只能抓内容区，而图片/PDF 撑满时内容区反而不好抓。
        finish_page_edit(false);
        on_drag_start(hwnd, lparam);
        return true;
    }

    let layout = frame(hwnd);
    if let Some(bar) = layout.bar
        && point_in(x, y, bar)
    {
        with_state(|s| s.scroll_drag = true);
        window::set_capture(hwnd);
        apply_scroll_y(hwnd, y);
        return true;
    }
    if let Some(toc) = layout.toc
        && point_in(x, y, toc)
    {
        let index = window::toc_item_at(
            layout.below_chrome,
            y,
            with_state(|s| s.toc_scroll).unwrap_or(0),
        );
        if let Some(index) = index {
            let target = with_state(|s| s.toc.get(index).map(|item| item.2)).flatten();
            if let Some(line) = target {
                with_state(|s| {
                    s.scroll = line;
                    s.toc_active = Some(index);
                });
                window::invalidate(hwnd);
                log_debug!(SCOPE, "目录跳转：第 {} 行", line + 1);
            }
        }
        return true;
    }
    false
}

/// 点/拖滚动条：把鼠标 y 换成"滚到哪儿"。
pub(crate) fn apply_scroll_y(hwnd: HWND, y: i32) {
    let layout = frame(hwnd);
    let (Some(_bar), Some(info)) = (layout.bar, scroll_info()) else {
        return;
    };
    let ratio = window::scroll_start_at(layout.below_chrome, y, info);
    let is_pdf = with_state(|s| s.pages.is_some()).unwrap_or(false);
    if is_pdf {
        let total = with_state(|s| s.pages).flatten().unwrap_or(1);
        let page = (ratio * (total.saturating_sub(1)) as f32).round() as usize + 1;
        goto_page(page);
    } else {
        let total = text_total_lines();
        let visible = with_state(|s| s.wrapped_consumed).unwrap_or(1).max(1);
        let max = total.saturating_sub(visible);
        let scroll = (ratio * max as f32).round() as usize;
        with_state(|s| s.scroll = scroll.min(max));
        window::invalidate(hwnd);
    }
}

/// 鼠标是否落在某个矩形里。
pub(crate) fn point_in(x: i32, y: i32, rect: RECT) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

/// 鼠标移动时更新"悬停在哪"（顶栏按钮 / 滚动条 / 目录），变化了才重绘。
pub(crate) fn update_hover(hwnd: HWND, lparam: LPARAM) -> bool {
    let (x, y) = client_xy(lparam);
    let client = window::client_rect(hwnd);
    let rects = window::chrome_rects(client);
    let button = if y < rects.content.top {
        if point_in(x, y, rects.close) {
            Some(window::ChromeButton::Close)
        } else if point_in(x, y, rects.external) {
            Some(window::ChromeButton::External)
        } else if point_in(x, y, rects.pin) {
            Some(window::ChromeButton::Pin)
        } else {
            None
        }
    } else {
        None
    };
    let layout = frame(hwnd);
    let toc_active = layout.toc.filter(|toc| point_in(x, y, *toc)).and_then(|_| {
        window::toc_item_at(
            layout.below_chrome,
            y,
            with_state(|s| s.toc_scroll).unwrap_or(0),
        )
    });
    let hover_toc = layout.toc.is_some_and(|toc| point_in(x, y, toc));
    let changed = with_state(|s| {
        let changed =
            s.hover_button != button || s.toc_active != toc_active || s.hover_toc != hover_toc;
        s.hover_button = button;
        s.toc_active = toc_active;
        // 滚轮落点：悬停在目录栏上就滚目录，否则滚正文（1.8.3）。
        s.hover_toc = hover_toc;
        changed
    })
    .unwrap_or(false);
    if changed {
        window::invalidate(hwnd);
    }
    changed
}

// ---------------------------------------------------------------- 压缩包列表视图（1.7.3）

pub(crate) fn repaint(hwnd: HWND) {
    if with_state(|s| s.drag_from.is_some()).unwrap_or(false) {
        trace_drag_paint();
    }
    // 换文件时的"冻帧"（1.7.6）：只更新顶栏（标题必须立刻对），内容区保持上一帧。
    // 这样 Office 预览那几十到几百毫秒的 `DoPreview` 期间不会闪出占位文字页。
    if with_state(|s| s.hold_frame).unwrap_or(false) {
        let title = file_title();
        let hover = with_state(|s| s.hover_button).unwrap_or(None);
        let hint = hover.map(|button| match button {
            window::ChromeButton::External => "在其他应用打开",
            window::ChromeButton::Close => "关闭预览",
            window::ChromeButton::Pin => "阻止关闭",
        });
        let toast = current_toast();
        let chrome = window::Chrome {
            title: &title,
            page: None,
            hover,
            hint,
            toast: toast.as_deref(),
            pinned: with_state(|s| s.pinned).unwrap_or(false),
        };
        window::paint_chrome_strip(hwnd, &chrome);
        return;
    }
    // 压缩包（1.7.3）：内置列表视图，不走文本/位图通道。
    if has_archive_view() {
        paint_archive(hwnd);
        return;
    }
    // 系统预览处理器接管内容时（1.4.1）：正文由它的子窗口自己画，我们只画顶栏。
    if with_state(|s| s.shell_preview.is_some()).unwrap_or(false) {
        let title = file_title();
        let (page_now, page_total, editing) =
            with_state(|s| (s.page, s.pages, s.page_edit.clone())).unwrap_or((0, None, None));
        let page = page_total.map(|total| window::PageBox {
            current: (page_now + 1) as u32,
            total: total as u32,
            editing: editing.as_deref(),
        });
        let hover = with_state(|s| s.hover_button).unwrap_or(None);
        let hint = hover.map(|button| match button {
            window::ChromeButton::External => "在其他应用打开",
            window::ChromeButton::Close => "关闭预览",
            window::ChromeButton::Pin => "阻止关闭",
        });
        let toast = current_toast();
        let chrome = window::Chrome {
            title: &title,
            page,
            hover,
            hint,
            toast: toast.as_deref(),
            pinned: with_state(|s| s.pinned).unwrap_or(false),
        };
        // 传空 rows：只把底色和顶栏画出来，内容区留给子窗口。
        window::paint_doc(hwnd, Some(&chrome), None, &[]);
        return;
    }
    // 绘制前兜一次投影：窗口尺寸/位置只要变了（缩放、拖边、外部改尺寸），
    // 阴影浮层就必须跟上，否则会留下一个比窗口大的灰框。
    // `sync_shadow` 内部按尺寸缓存 alpha 图，尺寸没变时只是一次 `SetWindowPos`，可以随便调。
    sync_shadow();
    // 顶栏（1.0.9）：四种格式共用一套。标题=文件名、PDF 多一个页码框、
    // 右侧三个按钮（阻止关闭 / 在其他应用打开 / 关闭）。
    let title = file_title();
    let (page_now, page_total, editing) =
        with_state(|s| (s.page + 1, s.pages, s.page_edit.clone())).unwrap_or((1, None, None));
    let hover = with_state(|s| s.hover_button).flatten();
    let pinned = with_state(|s| s.pinned).unwrap_or(false);
    let hint = hover.map(|button| match button {
        window::ChromeButton::External => "在其他应用打开",
        window::ChromeButton::Close => "关闭预览",
        // 锁是单向的：开着就告诉用户"已经锁了、关不掉"，没开就说清点它会怎样。
        window::ChromeButton::Pin => {
            if pinned {
                "已阻止关闭（Esc/空格关不掉）"
            } else {
                "阻止关闭（开了就不能取消）"
            }
        }
    });
    let page = page_total.map(|total| window::PageBox {
        current: page_now as u32,
        total: total as u32,
        editing: editing.as_deref(),
    });
    let toast = current_toast();
    let chrome = window::Chrome {
        title: &title,
        page,
        hover,
        hint,
        toast: toast.as_deref(),
        pinned,
    };
    let layout = frame(hwnd);

    // 优先画渲染好的内容；还没到位就画占位文字。
    if let Some(canvas) = get_content() {
        match &*canvas {
            Canvas::Bitmap(b) => {
                let pan = with_state(|s| s.pan).unwrap_or((0, 0));
                // 缩放后新位图还没回来 → 把旧位图拉伸铺满窗口（1.0.5），
                // 画面立刻跟着手走；按 1:1 裁剪的话窗口一变就是大半屏底色。
                let preview = with_state(|s| s.zoom_preview).unwrap_or(false);
                let rc = window::client_rect(hwnd);
                log_debug!(
                    SCOPE,
                    "PAINT bitmap {}x{} pan={:?} client={}x{} ink={}{}",
                    b.width,
                    b.height,
                    pan,
                    rc.right - rc.left,
                    rc.bottom - rc.top,
                    canvas.ink_pixels(),
                    if preview {
                        "（缩放预览，拉伸）"
                    } else {
                        ""
                    }
                );
                // SVG / 视频首帧：**只等比缩放**（1.7.8 用户要求"定死长宽比"）。
                // 图片（1.8.1）改由"窗口比例锁死图片比例"来保证不变形，所以这里铺满。
                // PDF/文本/Office 是"页面"语义，继续铺满内容区（比例由渲染尺寸保证）。
                let fit_bitmap = with_state(|s| s.last_path.clone())
                    .flatten()
                    .map(|p| match format::kind_for_path(&p) {
                        // SVG / 视频：窗口比例本身是自由的，等比居中留白。
                        format::Kind::Svg | format::Kind::Video => true,
                        // 图片（1.8.1）：窗口比例已在开窗时锁成图片比例，铺满即等比。
                        // 1.8.2 起拖边改大小对图片关掉了，但**最大化/系统改大小**仍可能
                        // 把窗口变成别的比例——这时退回等比居中，别把图拉变形。
                        format::Kind::Image => window_aspect_differs(hwnd, b.width, b.height),
                        _ => false,
                    })
                    .unwrap_or(false);
                if fit_bitmap {
                    // 视频（1.7.9）：播放时 `s.video` 里有"当前这一帧"，它比渲染出来的首帧新。
                    // 用帧画（而不是首帧位图），窗口改大小/被别的窗口盖住后重画才不会"倒带"。
                    let painted_frame =
                        with_state(|s| s.video.as_ref().and_then(|v| v.last.clone())).flatten();
                    // 视频底部有进度条：贴图时要把那条留出来，否则这一帧会把进度条盖掉。
                    let reserve_bar = current_kind() == Some(format::Kind::Video);
                    match painted_frame {
                        Some(frame) => window::paint_bitmap_fit(
                            hwnd,
                            frame.width,
                            frame.height,
                            &frame.bgra,
                            Some(&chrome),
                            reserve_bar,
                        ),
                        None => window::paint_bitmap_fit(
                            hwnd,
                            b.width,
                            b.height,
                            &b.bgra,
                            Some(&chrome),
                            reserve_bar,
                        ),
                    }
                    paint_overlays(hwnd, &layout);
                    paint_lens_if_any(hwnd);
                    return;
                }
                if preview {
                    window::paint_bitmap_scaled(hwnd, b.width, b.height, &b.bgra, Some(&chrome));
                } else {
                    // 1.4.3：位图尺寸和内容区尺寸不一致时 `paint_bitmap_at` 会**拉伸**，
                    // 每一帧都要重采样整张图 —— 表现就是"画面发虚 + 拖动跟不上 + 滚动频闪"。
                    // 这里量一次并记 warn，同时把尺寸对不上的事实写进日志（便于定位）。
                    let content = layout.content;
                    let (cw, ch) = (
                        (content.right - content.left).max(1),
                        (content.bottom - content.top).max(1),
                    );
                    // 只统计"真的差很多"的情况：位图按宽高比取整，和内容区常常差 1–2 px，
                    // 那种拉伸肉眼看不出来、代价也可以忽略，不该刷日志。
                    //
                    // 判据只认**宽度**：宽度对不上说明"渲染口径"和"绘制口径"又不一致了
                    // （1.8.3 修过一次：渲染盒多了滚动条宽，右边被压住 24 px）。
                    // 高度对不上是正常的——PDF 页面有固定宽高比，比内容区矮的那一截
                    // 就是留白，不该被当成 bug 报警（现场日志里那 3 行 STRETCH 就是这么来的）。
                    let off_w = (b.width as i32 - cw).abs();
                    if off_w > 16 && !with_state(|s| s.zoom_preview).unwrap_or(false) {
                        log_warn!(
                            SCOPE,
                            "STRETCH 位图宽 {} != 内容区 {cx}（渲染口径不一致，上屏要拉伸）",
                            b.width,
                            cx = cw
                        );
                    } else if (b.height as i32 - ch).abs() > 16 {
                        log_debug!(
                            SCOPE,
                            "LETTERBOX 位图高 {} < 内容区 {cy}（页面比例所限，正常留白）",
                            b.height,
                            cy = ch
                        );
                    }
                    window::paint_bitmap_at(
                        hwnd,
                        b.width,
                        b.height,
                        &b.bgra,
                        pan.0,
                        pan.1,
                        Some(&chrome),
                    );
                }
                paint_overlays(hwnd, &layout);
                paint_lens_if_any(hwnd);
                return;
            }
            Canvas::Text(t) => {
                // 代码预览（1.0.8）：不折行、一行一源行，走 paint_doc 的"一行多色"通道。
                if let Some(spans) = &t.spans {
                    paint_code(hwnd, &chrome, &layout, t, spans);
                    paint_overlays(hwnd, &layout);
                    return;
                }
                // 可视区虚拟化：只把当前窗口该显示的那几十行交给 GDI。
                // 上游 QuickLook 在 64 KB 文本上滚屏吃掉 6.6 GB，就是因为把整篇都塞给了控件。
                let rows = visible_rows(hwnd);
                let scroll = with_state(|s| s.scroll).unwrap_or(0).min(t.lines.len());
                // paint_styled 左边留 24 px 边距 + 一条行号栏，右边留 24 px。
                // 折行可用宽度必须把这几块减掉（CONVENTIONS #38：量宽 != 画宽就会"折了行还被吃字"）。
                // 排版宽度必须等于**绘制用的**内容区宽度（CONVENTIONS #38）：
                // 内容区已经扣掉了顶栏、目录栏、滚动条，这里照着它算。
                let rc = layout.content;
                // HTML（1.7.9）是"排版出来的文档"，不是源码：行号栏和"第 X–Y 行"都是噪音，
                // 排出来的效果应该像一篇网页而不是带行号的文本。音频卡片同理（就几行元信息）。
                let plain_doc = matches!(
                    current_kind(),
                    Some(format::Kind::Html) | Some(format::Kind::Audio)
                );
                let gutter = if plain_doc { 0 } else { window::text_gutter() };
                let width = ((rc.right - rc.left) - 48 - gutter).max(0) as u32;
                // 折行需要"这段文字有多宽"，只能问 GDI：抓一次 DC 量一屏，
                // 量不到（拿 DC 失败）就退回不折行 —— 宁可不折行也不能画错。
                let measurer = window::Measurer::new(hwnd);
                // 每行记 `(文本, 样式, 行号)`：折行产生的续行不带行号，只有源行首行带。
                let mut body: Vec<(String, qlr_render::LineStyle, Option<u32>)> = Vec::new();
                let mut consumed = 0usize;
                if let Some(m) = &measurer {
                    // 逐原始行量、逐行记样式（Markdown 每行样式不同：标题字号更大、代码等宽）。
                    for (i, line) in t.lines.iter().enumerate().skip(scroll) {
                        if body.len() >= rows {
                            break;
                        }
                        let style = t
                            .styles
                            .as_ref()
                            .and_then(|all| all.get(i).copied())
                            .unwrap_or_default();
                        let budget = rows - body.len();
                        let mut texts: Vec<String> = Vec::new();
                        let measure = |s: &str| m.width_styled(to_text_style(style), s);
                        qlr_render::wrap::push_wrapped(line, width, &measure, &mut texts, budget);
                        if texts.is_empty() {
                            continue;
                        }
                        consumed += 1;
                        for (k, text) in texts.into_iter().enumerate() {
                            let number = if plain_doc {
                                None
                            } else if k == 0 {
                                Some((i + 1) as u32)
                            } else {
                                None
                            };
                            body.push((text, style, number));
                        }
                    }
                } else {
                    let end = (scroll + rows).min(t.lines.len());
                    for i in scroll..end {
                        let style = t
                            .styles
                            .as_ref()
                            .and_then(|all| all.get(i).copied())
                            .unwrap_or_default();
                        body.push((
                            t.lines[i].clone(),
                            style,
                            if plain_doc {
                                None
                            } else {
                                Some((i + 1) as u32)
                            },
                        ));
                    }
                    consumed = end - scroll;
                }
                with_state(|s| s.wrapped_consumed = consumed);
                log_debug!(
                    SCOPE,
                    "WRAP 原文 {} 行 从 {} 起，屏幕 {} 行，宽 {}px -> 折成 {} 行（吃掉 {} 原始行）{}",
                    t.lines.len(),
                    scroll,
                    rows,
                    width,
                    body.len(),
                    consumed,
                    if t.styles.is_some() {
                        "（带样式）"
                    } else {
                        ""
                    }
                );
                let last = scroll + consumed;
                let status = format!(
                    "── 第 {}–{} 行 / 共 {} 行{} ──",
                    scroll + 1,
                    last.max(scroll + 1),
                    t.lines.len(),
                    if t.truncated {
                        "，文件被截断"
                    } else {
                        ""
                    }
                );
                let mut styled: Vec<window::StyledRow<'_>> = Vec::with_capacity(body.len() + 1);
                if !plain_doc {
                    styled.push(window::StyledRow {
                        number: None,
                        text: &status,
                        style: window::TextStyle::Normal,
                        bg: None,
                    });
                }
                for (text, style, number) in &body {
                    styled.push(window::StyledRow {
                        number: *number,
                        text,
                        style: to_text_style(*style),
                        bg: line_bg(*style),
                    });
                }
                // 每份内容打一次样式分布（INFO 级）：渲染样式是"看不见的"东西，
                // 出问题时只有这条日志能证明"确实是按样式画的、各占几行"。
                let first_time = with_state(|s| {
                    let first = !s.styled_logged;
                    s.styled_logged = true;
                    first
                })
                .unwrap_or(false);
                if first_time && !body.is_empty() {
                    let mut tally = [0usize; qlr_render::LineStyle::COUNT];
                    for (_, style, _) in &body {
                        tally[style.as_index()] += 1;
                    }
                    let mut parts: Vec<String> = Vec::new();
                    for style in [
                        qlr_render::LineStyle::Heading1,
                        qlr_render::LineStyle::Heading2,
                        qlr_render::LineStyle::Heading3,
                        qlr_render::LineStyle::Code,
                        qlr_render::LineStyle::Quote,
                        qlr_render::LineStyle::List,
                        qlr_render::LineStyle::Rule,
                        qlr_render::LineStyle::Table,
                        qlr_render::LineStyle::Normal,
                    ] {
                        let n = tally[style.as_index()];
                        if n > 0 {
                            parts.push(format!("{}={n}", style.name()));
                        }
                    }
                    log_info!(
                        SCOPE,
                        "STYLED 屏幕 {} 行（样式分布：{}）行号栏={}px",
                        styled.len() - 1,
                        parts.join(" "),
                        window::text_gutter()
                    );
                }
                let _ = t.encoding;
                // 量一帧的绘制耗时：滚动"频闪/卡"时这条日志能直接指认是绘制太慢。
                let paint_started = Instant::now();
                // 正文和叠加层画进同一块内存 DC，整窗只上屏一次（1.5.2，消掉侧栏闪）。
                let overlay = |hdc: HDC, content: RECT| paint_overlays_dc(hdc, &layout, content);
                window::paint_styled_with(
                    hwnd,
                    Some(&chrome),
                    Some(layout.content),
                    &styled,
                    Some(&overlay),
                );
                let ms = paint_started.elapsed().as_millis();
                if ms >= 8 {
                    log_warn!(SCOPE, "SLOWPAINT text {ms}ms（{} 行）", styled.len());
                }
                return;
            }
        }
    }
    let lines = get_lines();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let overlay = |hdc: HDC, content: RECT| paint_overlays_dc(hdc, &layout, content);
    window::paint_text_with(
        hwnd,
        Some(&chrome),
        Some(layout.content),
        &refs,
        Some(&overlay),
    );
}

/// 目录里"当前读到哪一节"：取最后一个起始行 ≤ 滚动位置的小节。
pub(crate) fn toc_following(items: &[(u8, String, usize)], scroll: usize) -> Option<usize> {
    let mut found = None;
    for (index, (_, _, line)) in items.iter().enumerate() {
        if *line <= scroll {
            found = Some(index);
        }
    }
    found
}

/// 在内容之上叠加目录栏与滚动条（1.0.9）。
///
/// 为什么单独画：这两块用 `GetDC` 直接往窗口上画（一次 `BeginPaint` 只能有一个），
/// 所以必须在正文绘制**之后**调用，否则会被正文的 `FillRect` 盖掉。
/// 把目录栏与滚动条画到**给定 DC** 上（1.5.2：和正文共用内存 DC，整窗一次上屏）。
pub(crate) fn paint_overlays_dc(hdc: HDC, layout: &Frame, content: RECT) {
    if let Some(toc) = layout.toc {
        let (items, hovered, scroll) = with_state(|s| (s.toc.clone(), s.toc_active, s.scroll))
            .unwrap_or_else(|| (Vec::new(), None, 0));
        let refs: Vec<window::TocItem<'_>> = items
            .iter()
            .map(|(level, text, _)| window::TocItem {
                level: *level,
                text: text.as_str(),
            })
            .collect();
        let selected = hovered.or_else(|| toc_following(&items, scroll));
        let first_toc = with_state(|s| s.toc_scroll).unwrap_or(0);
        window::paint_toc_dc(hdc, toc, &refs, selected, first_toc);
    }
    if layout.bar.is_some()
        && let Some(info) = scroll_info()
    {
        window::paint_scrollbar_dc(hdc, layout.below_chrome, info, false);
    }
    // 音频/视频进度条（1.7.9）：整窗重画时也要画一次，否则拖动窗口/改大小之后那条就没了
    // （`TIMER_AUDIO`/`TIMER_VIDEO` 只在播放时刷，暂停状态下不会有第二次机会补上）。
    if let Some((pos, dur)) = playback_progress() {
        let playing = playback_playing();
        window::paint_audio_bar_dc(hdc, content, pos, dur, playing);
    }
    let _ = content;
}

pub(crate) fn paint_overlays(hwnd: HWND, layout: &Frame) {
    if let Some(toc) = layout.toc {
        let (items, hovered, scroll) =
            with_state(|s| (s.toc.clone(), s.toc_active, s.scroll)).unwrap_or_default();
        // 悬停优先，否则跟着当前滚动位置高亮（像阅读器那样，滚到哪里目录指到哪里）。
        let selected = hovered.or_else(|| toc_following(&items, scroll));
        let first_toc = with_state(|s| s.toc_scroll).unwrap_or(0);
        let refs: Vec<window::TocItem<'_>> = items
            .iter()
            .map(|(level, text, _)| window::TocItem {
                level: *level,
                text: text.as_str(),
            })
            .collect();
        window::paint_toc(hwnd, toc, &refs, selected, first_toc);
    }
    if layout.bar.is_some()
        && let Some(info) = scroll_info()
    {
        window::paint_scrollbar(hwnd, layout.below_chrome, info, false);
    }
    // 音频/视频进度条（1.7.9）：位图通道（图片/视频/SVG）走的是这里而不是 `_dc` 版本，
    // 而贴图会把整块内容区先铺一遍底色——不补这一笔，进度条就会在重画时消失。
    if let Some((pos, dur)) = playback_progress() {
        let playing = playback_playing();
        window::paint_audio_bar(hwnd, layout.below_chrome, pos, dur, playing);
    }
    // 最外圈那三条灰边（1.0.10）：必须**最后**画，否则会被正文/位图的整块填充盖掉。
    // 用户要求"窗口周围新增一圈灰色阴影，方便边边和其他程序的白色区分"。
    window::paint_window_frame(hwnd);
}

/// `WM_COPYDATA` 是 `SendMessage` 送来的，处理它时本线程处于"输入同步呼叫"状态。
///
/// 这个状态下调用 COM（`CoCreateInstance(ShellWindows)`）会被系统直接拒成
/// `RPC_E_CANTCALLOUT_ININPUTSYNCCALL (0x8001010D)` —— 实测踩到过：
/// 第二个实例转交 `--simulate-hotkey` 时，主实例拿不到选中项。
/// 所以这里**只排队**，用 `PostMessage` 转成一条普通队列消息再处理，
/// 那时输入同步状态已经退出，COM 与渲染都能正常跑。
pub(crate) fn on_copy_data(hwnd: HWND, lparam: LPARAM) {
    let Some(bytes) = instance::copy_data_payload(lparam) else {
        log_warn!(SCOPE, "收到无法识别的 WM_COPYDATA，已忽略");
        return;
    };
    let Some(request) = Request::from_bytes(&bytes) else {
        log_warn!(SCOPE, "WM_COPYDATA 载荷解析失败（{} 字节）", bytes.len());
        return;
    };
    if let Ok(mut pending) = pending_requests().lock() {
        pending.push(request);
    }
    window::post(hwnd, ipc::WM_QLR_INVOKE);
}

/// 代码预览上屏（1.0.8）：不折行、一行一个源行，按 token 分段着色。
///
/// 为什么不折行：VSCode 默认也不折行；折行会让"一行代码变两行"，行号对不上，
/// 也会打乱缩进语义。超长行由 `paint_doc` 在右边界截断。
pub(crate) fn paint_code(
    hwnd: HWND,
    chrome: &window::Chrome<'_>,
    layout: &Frame,
    t: &qlr_render::TextDoc,
    spans: &[Vec<qlr_render::Span>],
) {
    let rows = rows_in(layout.content);
    let scroll = with_state(|s| s.scroll).unwrap_or(0).min(t.lines.len());
    let end = (scroll + rows).min(t.lines.len()).min(spans.len());

    let status = format!(
        "── 第 {}–{} 行 / 共 {} 行{} · 源码 ──",
        scroll + 1,
        end.max(scroll + 1),
        t.lines.len(),
        if t.truncated {
            "，文件被截断"
        } else {
            ""
        }
    );
    // 先把 token 段映射成平台层样式（`paint_doc` 只认 `&[window::Span]`）。
    let mut lane: Vec<Vec<window::Span<'_>>> = Vec::with_capacity(end - scroll + 1);
    lane.push(vec![window::Span {
        text: &status,
        style: window::TextStyle::Normal,
    }]);
    for row in &spans[scroll..end] {
        lane.push(
            row.iter()
                .map(|s| window::Span {
                    text: s.text.as_str(),
                    style: to_token_style(s.token),
                })
                .collect(),
        );
    }
    let mut doc: Vec<window::DocRow<'_>> = Vec::with_capacity(lane.len());
    doc.push(window::DocRow {
        number: None,
        spans: &lane[0],
        bg: None,
    });
    for (k, spans_for_row) in lane.iter().enumerate().skip(1) {
        doc.push(window::DocRow {
            number: Some((scroll + k) as u32),
            spans: spans_for_row,
            bg: None,
        });
    }
    with_state(|s| s.wrapped_consumed = end - scroll);
    // 着色是"看不见的"东西，出问题时只有这条日志能证明各 token 各占几段。
    let first_time = with_state(|s| {
        let first = !s.styled_logged;
        s.styled_logged = true;
        first
    })
    .unwrap_or(false);
    if first_time {
        let mut tally = [0usize; qlr_render::Token::COUNT];
        for row in &spans[scroll..end] {
            for s in row {
                tally[s.token.as_index()] += 1;
            }
        }
        log_info!(
            SCOPE,
            "CODE 屏幕 {} 行 token 段：plain={} keyword={} str={} comment={} number={} 行号栏={}px",
            end - scroll,
            tally[0],
            tally[1],
            tally[2],
            tally[3],
            tally[4],
            window::text_gutter()
        );
    }
    window::paint_doc(hwnd, Some(chrome), Some(layout.content), &doc);
}

/// 渲染层 token → 平台层字体/颜色。
pub(crate) fn to_token_style(token: qlr_render::Token) -> window::TextStyle {
    match token {
        qlr_render::Token::Plain => window::TextStyle::Code,
        qlr_render::Token::Keyword => window::TextStyle::Keyword,
        qlr_render::Token::Str => window::TextStyle::Str,
        qlr_render::Token::Comment => window::TextStyle::Comment,
        qlr_render::Token::Number => window::TextStyle::Number,
    }
}
