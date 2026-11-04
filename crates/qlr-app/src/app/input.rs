//! 输入：热键、鼠标、滚轮、拖动与改尺寸、页码输入、缩放与平移。
//!
//! 钩子回调只往这里投消息，真正的判定都在 UI 线程这一层做。

use super::*;

/// 翻页键 → 方向：PageDown/右 = +1，PageUp/左 = -1。
pub(crate) fn page_delta_for(vk: usize) -> Option<isize> {
    match vk as u32 {
        0x22 | 0x27 => Some(1),
        0x21 | 0x25 => Some(-1),
        _ => None,
    }
}

/// 缩放范围：0.25×–8×。再大 pdfium 会把内存吃爆，再小没意义。
pub(crate) const ZOOM_MIN: f32 = 0.25;
pub(crate) const ZOOM_MAX: f32 = 8.0;

pub(crate) fn scaled(value: u32, zoom: f32) -> u32 {
    layout::scaled(value, zoom)
}

/// 当前该按多大像素渲染：客户区尺寸（扣掉顶部状态条）× 缩放。
pub(crate) fn view_size(preview: HWND) -> (u32, u32) {
    // 渲染盒必须**等于绘制/命中用的内容区**（`frame().content`）。
    //
    // 以前这里用整窗客户区减顶栏，比 `frame().content` 多出一条滚动条的宽度：
    // 位图按全宽渲染、上屏却只铺在"扣掉滚动条"的内容区里，于是
    // ①页面右边那条被滚动条压住（96 DPI 下 12 px，200% 下 24 px）；
    // ②宽高都和内容区差几像素，`StretchDIBits` 只能拉伸 → 每帧重采样。
    // 现场日志就是这条：`STRETCH 位图 1044x1477 != 内容区 1020x1479`。
    //
    // 顶栏（1.0.9 起四种格式统一）也已经在 `chrome_rects().content` 里扣过了。
    // 1.8.3 起四套"内容区尺寸"统一由 `layout::Layout` 产出，这里直接取 `render_box`：
    // 它和 `frame().content × zoom` 是同一份算术，不会再各算一遍。
    frame(preview).render_box
}

/// 复位到"适应窗口"（100%）。
pub(crate) fn zoom_reset() {
    let current = with_state(|s| s.zoom).unwrap_or(1.0);
    set_zoom_to(1.0, current, true);
}

/// 按倍数缩放。
pub(crate) fn set_zoom(factor: f32) {
    let current = with_state(|s| s.zoom).unwrap_or(1.0);
    set_zoom_to((current * factor).clamp(ZOOM_MIN, ZOOM_MAX), current, true);
}

/// 改缩放并（可选）重渲染当前文件。
///
/// 1.0.2 起：**窗口跟着缩放走**（有自然尺寸的内容）。以前只在固定窗口里改渲染分辨率，
/// 放大后画面被裁掉、缩小后四周一圈黑边，用户反馈"窗口应该跟着缩放"。
/// 1.0.5 两处改动：
/// - 窗口尺寸变了以后**保持窗口中心不动**（`resize_around_center`），不再按工作区重新居中
///   ——用户把窗口拖到别处再缩放，画面"跳回屏幕中间"是很难受的；
/// - `render = false` 时**不重渲染**，只把旧位图拉伸铺上新窗口（`zoom_preview`）。
///   右键拖动缩放时每一帧重渲染一页 PDF 是卡顿的根源，松手（`on_zoom_drag_end`）才出高清图。
/// - 文本/Markdown 没有自然尺寸（滚动是按行走的），保持原行为：只改渲染分辨率。
pub(crate) fn set_zoom_to(new: f32, old: f32, render: bool) {
    let Some((preview, path)) = with_state(|s| (s.preview, s.last_path.clone())) else {
        return;
    };
    let (new, base) = with_state(|s| {
        s.zoom = new;
        // 缩放后原来的平移量不再有意义（渲染尺寸变了），直接回中。
        s.pan = (0, 0);
        (s.zoom, s.base)
    })
    .unwrap_or((new, None));
    if (old - new).abs() < f32::EPSILON {
        log_debug!(SCOPE, "缩放已在边界 {new:.2}×，忽略");
        return;
    }
    // 有开窗基准尺寸：窗口跟着缩放走（1.0.2），位图按"基准 × zoom"渲染。
    // 窗口塞不进工作区时会被夹住，此时位图比窗口大 → 用户可以平移看局部。
    let (width, height) = match base {
        Some((bw, bh)) => {
            let cw = ((bw as f32) * new).round().max(1.0) as u32;
            let ch = ((bh as f32) * new).round().max(1.0) as u32;
            // base 记的是**内容区**尺寸：PDF 的窗口还要多出一条页码栏的高度。
            let reserve = window::chrome_height();
            window::resize_around_center(preview.raw(), cw as i32, (ch as i32) + reserve);
            (cw, ch)
        }
        None => view_size(preview.raw()),
    };
    // 从"改完窗口"到"新位图到位"这段时间，画面只能是旧位图 —— 标记成预览态，
    // 让 `repaint` 用拉伸方式铺满窗口，而不是按 1:1 裁剪（那会露出半屏底色）。
    with_state(|s| s.zoom_preview = true);
    if render {
        if let Some(path) = path {
            let page = with_state(|s| s.page).unwrap_or(0);
            request_render(&path, width, height, page);
        }
    }
    let mut lines = get_lines();
    for line in lines.iter_mut() {
        if line.starts_with("缩放：") {
            *line = format!("缩放：{new:.0}%");
            break;
        }
    }
    set_lines(lines);
    // 缩放会改窗口尺寸，投影浮层必须跟着重算——不跟就会留下一个比窗口大的灰框
    // （用户在缩小图片时看到"外圈多了一层阴影框"，就是这个）。
    sync_shadow();
    // 位置记忆（1.0.13）：缩放按窗口中心改尺寸，左上角会跟着变，记下新位置。
    remember_position();
    // 尺寸记忆（1.0.14）：缩放也是用户对尺寸的决定（放大看细节后会保持这个大小）。
    remember_size();
    window::invalidate(preview.raw());
    // 用户可见的缩放（滚轮/快捷键）打 info，拖动预览帧打 debug：
    // 一次拖动几十帧，全打 info 会把日志刷爆；但冒烟脚本要能看到"滚轮真的缩放了"。
    if render {
        log_info!(SCOPE, "ZOOM {old:.2} -> {new:.2} render={width}x{height}");
    } else {
        log_debug!(
            SCOPE,
            "ZOOM {old:.2} -> {new:.2} render={width}x{height}（预览，松手再渲染）"
        );
    }
}

/// 文本滚动：当前内容不是文本时返回 false（调用方据此退回缩放/翻页/平移）。
pub(crate) fn scroll_text_by(lines: isize) -> bool {
    let Some(preview) = with_state(|s| s.preview) else {
        return false;
    };
    if !matches!(get_content().as_deref(), Some(Canvas::Text(_))) {
        return false;
    }
    let rows = visible_rows(preview.raw());
    let total = text_total_lines();
    // 内容比窗口还短：什么都不用滚，但也算"已处理"，别再退回去缩放
    if total <= rows {
        return true;
    }
    let current = with_state(|s| s.scroll).unwrap_or(0);
    let next = scroll_clamp(current, lines, total, rows);
    if next != current {
        with_state(|s| s.scroll = next);
        log_debug!(
            SCOPE,
            "SCROLL {current} -> {next}（共 {total} 行，可见 {rows} 行）"
        );
        window::invalidate(preview.raw());
    }
    true
}

/// 滚动位置钳制（纯函数，便于单测）。
pub(crate) fn scroll_clamp(current: usize, delta: isize, total: usize, rows: usize) -> usize {
    let max = total.saturating_sub(rows);
    let raw = current as i64 + delta as i64;
    raw.clamp(0, max as i64) as usize
}

/// 状态行占一行，正文能用的行数要减 1。
/// 一屏能放几行正文（不含顶部留白与状态行）。
///
/// 1.0.8 修：以前按 `h / window::line_height() - 1` 算，**没扣顶部留白**，748 高的窗口算出
/// 30 行正文，加上状态行就是 `16 + 31*24 = 760 > 748` —— 最后一行被窗口裁掉半截，
/// 用户看到的就是"md 和文本最后一行显示不全"。
pub(crate) fn visible_rows(hwnd: HWND) -> usize {
    rows_in(window::client_rect(hwnd))
}

/// 给定一块内容区，能放几行正文（不含顶部留白与状态行）。
///
/// 1.0.9 起顶栏、目录栏、滚动条都会吃掉内容区，所以行数必须按**内容区**算，
/// 否则文本最后一行会跑到顶栏下面去（或反过来多留一条空白）。
pub(crate) fn rows_in(content: RECT) -> usize {
    let usable = (content.bottom - content.top) - window::text_padding_top();
    let rows = (usable / window::line_height()).max(1) as usize;
    // 再让出一行给状态行（状态行画在正文之上，两行一起必须塞得下）。
    rows.saturating_sub(1).max(1)
}

/// 翻页：文本按"上次绘制实际吃掉多少原始行"滚动，其它内容（PDF/图片）换页或换张。
pub(crate) fn turn_page(delta: isize) {
    // PPT（1.7.4）：翻页由我们接管（数页、到头停住并提示）。放在最前面，
    // 因为这类内容既没有文本行也没有 `pages`，落到下面只会什么都不做。
    if step_slide(delta) {
        return;
    }
    let step = with_state(|s| s.preview)
        .map(|p| page_lines(p.raw()) as isize)
        .unwrap_or(10)
        .max(1);
    if !scroll_text_by(delta * step) {
        step_page(delta);
    }
}

/// 翻页步长：优先用上次绘制实际吃掉的**原始行数**（折行后一屏原始行更少），
/// 还没画过就退回屏幕行数。
pub(crate) fn page_lines(hwnd: HWND) -> usize {
    let consumed = with_state(|s| s.wrapped_consumed).unwrap_or(0);
    if consumed > 0 {
        consumed
    } else {
        visible_rows(hwnd)
    }
}

pub(crate) fn text_total_lines() -> usize {
    match get_content().as_deref() {
        Some(Canvas::Text(t)) => t.lines.len(),
        _ => 0,
    }
}

/// 滚轮总入口：文本先滚，其余（图片/PDF）才滑动/缩放。
///
/// **钩子投递的滚轮和 CLI 的 `--wheel` 必须走同一条路**（1.0.1 修）：
/// 之前 `WM_QLR_WHEEL` 直接调 `wheel_zoom`，导致鼠标滚轮在文本上不是滚动而是
/// "缩放重新渲染"——用户看到的是屏幕内容突然变形/错位（以为是翻页逻辑坏了）。
///
/// 1.0.3 起的约定（按用户要求）：
/// - **文本**：一格 3 行（折行后的显示行，滚到底自然停）。向上滚 = 看上面的内容。
/// - **PDF**：页内上下滑动，一格 1/4 屏；滑到页顶/页底还继续滚 = 翻到上一页/下一页。
/// - **图片**：放得下就保持老习惯（滚轮 = 缩放）；放大到超出窗口后可上下滑动。
/// - **缩放**：图片/PDF 都改用**右键上下拖**（`on_zoom_drag_*`），滚轮不再改缩放。
pub(crate) fn on_wheel(delta: i32) {
    // 档数走**带零头累积**的入口（1.8.3）。这里有一个真实事故：
    // 高分辨率滚轮/触控板每次只发 30–60，`delta / 120` 永远是 0，
    // 于是"滚轮完全没反应"——图片不缩放、正文不滚、目录不滚，日志里也没有任何记录。
    let notches = take_wheel_notches(delta);
    // 这个函数有 8 条分支，出问题（"滚轮没反应"）时必须能一眼看出停在哪一支。
    log_debug!(
        SCOPE,
        "WHEEL 进入 delta={delta} 本次档数={notches} 累计零头={}",
        with_state(|s| s.wheel_acc).unwrap_or(0)
    );
    // 音频/视频（1.7.9）：滚轮 = 快进/快退（一格 5 秒）。放在最前面——它们没有可滚内容，
    // 落到下面会去缩放/翻页，用户听到的是"声音还在放、画面莫名其妙变了"。
    if playback_active() {
        if notches != 0 {
            // 往上滚 = 往回（与"翻到前面"同一方向约定）。
            playback_seek(-(notches.signum() as i64) * AUDIO_SEEK_STEP_MS);
        }
        return;
    }
    // PPT（1.7.4）：翻页由我们接管，滚轮 = 翻页（和 PDF 一致：往下滚 = 下一页）。
    // 必须放在 `forward_wheel_to_shell` 前面：那个函数会把滚轮转给处理器的子窗口，
    // 而它的翻页计数只有我们自己在维护，转出去就失准了。
    if with_state(|s| s.slide).flatten().is_some() {
        if notches != 0 {
            step_slide(if notches > 0 { -1 } else { 1 });
        }
        return;
    }
    // 零头还没攒够一整档：什么都不做，等下一发继续攒（**不要**在这里调 wheel_zoom，
    // 否则高分辨率滚轮会拿 delta=30 去缩放，一格变成十几次缩放）。
    if notches == 0 {
        return;
    }
    // 系统预览处理器接管时，滚轮归它（它自己有滚动条/翻页），我们不抢。
    if forward_wheel_to_shell(delta as i16) {
        return;
    }
    // Shift + 滚轮：调放大镜倍数（按住 Shift 时用户在看细节，不该拿去滚动/缩放内容）。
    if lens_wheel(notches) {
        return;
    }
    // 压缩包列表（1.7.3）：按行滚。放在文本/位图前面——列表视图的内容不在 `CONTENT` 里，
    // 继续往下走只会误伤（比如被当成"图片放得下"去缩放）。
    if archive_scroll_by(wheel_text_rows(notches)) {
        return;
    }
    // 目录栏（1.8.3）：鼠标悬停在目录上时，滚轮滚**目录**而不是正文。
    // 目录比窗口高的时候以前根本翻不到后面几条（没有滚动条，也没有滚轮），
    // 用户的原话："目录页可以不要滚动栏，支持鼠标滚动即可"。
    if with_state(|s| s.hover_toc).unwrap_or(false) && scroll_toc_by(notches) {
        return;
    }
    // 文本：一格 3 行。滚得动就不缩放。
    if scroll_text_by(wheel_text_rows(notches)) {
        return;
    }
    let (moved, at_edge) = scroll_page_by(notches);
    if moved && !at_edge {
        return;
    }
    // PDF：一页看完继续滚 = 翻页（滚轮向下 = 下一页，和"往下读"一致）。
    if with_state(|s| s.pages).flatten().is_some() {
        step_page(if notches > 0 { -1 } else { 1 });
        return;
    }
    // 图片：本来就没得滚（整张放得下）→ 保留"滚轮缩放"的老习惯。
    if !moved {
        wheel_zoom(notches);
    }
}

/// 滚动目录栏（1.8.3）。返回是否处理了这次滚轮。
///
/// 目录一屏放不下时，滚轮在目录上就滚目录；放得下就返回 false，让给正文
/// （否则"目录只有 5 条"的文件滚轮会失效，看起来像坏了）。
pub(crate) fn scroll_toc_by(notches: isize) -> bool {
    let Some(preview) = with_state(|s| s.preview) else {
        return false;
    };
    let layout = frame(preview.raw());
    let Some(toc) = layout.toc else {
        return false;
    };
    let visible = window::toc_visible_rows(layout.below_chrome).max(1);
    let total = with_state(|s| s.toc.len()).unwrap_or(0);
    let max = total.saturating_sub(visible);
    if max == 0 {
        return false;
    }
    let cur = with_state(|s| s.toc_scroll).unwrap_or(0);
    let step = wheel_text_rows(notches); // 一格 3 条，与正文手感一致
    let next = (cur as i64 + step as i64).clamp(0, max as i64) as usize;
    if next != cur {
        with_state(|s| s.toc_scroll = next);
        window::invalidate(preview.raw());
        log_debug!(
            SCOPE,
            "TOCSCROLL {cur} -> {next}（共 {total} 条，可见 {visible} 条）"
        );
    }
    let _ = toc;
    true
}

/// 滚轮档数 → 文本滚动行数（纯函数，便于单测）。
///
/// 1.0.4 修方向：**往上滚必须看上面的内容**。1.0.3 写成了 `notches * 3`，
/// 结果向上滚反而往下翻（和拖拽、PageUp 全反着），用户一眼就看出来了。
pub(crate) fn wheel_text_rows(notches: isize) -> isize {
    -notches * 3
}

/// 页内上下滑动（位图比窗口高才有意义）。
///
/// 返回 `(这次是否真的动了, 是否已经顶到边界)`：PDF 用第二个值判断"该翻页了"。
pub(crate) fn scroll_page_by(notches: isize) -> (bool, bool) {
    let Some((preview, content_h)) = with_state(|s| s.preview).zip(bitmap_height()) else {
        return (false, false);
    };
    let rc = window::client_rect(preview.raw());
    let ch = (rc.bottom - rc.top).max(1);
    let max = (content_h - ch).max(0);
    if max == 0 {
        return (false, false);
    }
    // 一格 = 1/4 屏；滚轮向下（notches < 0）→ 看更下面的内容（pan.1 增大）。
    let step = (ch / 4).max(48) * -(notches as i32);
    let (cur, next) = with_state(|s| {
        let cur = s.pan.1;
        let next = (cur + step).clamp(0, max);
        s.pan.1 = next;
        (cur, next)
    })
    .unwrap_or((0, 0));
    let at_edge = if notches > 0 { next == 0 } else { next == max };
    if next != cur {
        window::invalidate(preview.raw());
        log_debug!(
            SCOPE,
            "VSCROLL pan.y {cur} -> {next}（内容 {content_h}px / 窗口 {ch}px）"
        );
    }
    (next != cur, at_edge)
}

/// 当前位图的高度（像素）；文本/占位内容返回 `None`。
pub(crate) fn bitmap_height() -> Option<i32> {
    match get_content().as_deref() {
        Some(Canvas::Bitmap(b)) => Some(b.height as i32),
        _ => None,
    }
}

/// 滚轮缩放：一格（`WHEEL_DELTA`=120）放大 1.25 倍，负向缩小。
///
/// 入参是**档数**（已经过 `take_wheel_notches` 的零头累积），不是原始 delta：
/// 高分辨率滚轮单发只有 30，再在这里除一次 120 就又变回 0 —— 1.8.3 前就是这个问题。
pub(crate) fn wheel_zoom(notches: isize) {
    if notches == 0 {
        return;
    }
    let factor = if notches > 0 {
        1.25_f32
    } else {
        1.0 / 1.25_f32
    };
    for _ in 0..notches.abs() {
        set_zoom(factor);
    }
    log_debug!(SCOPE, "ZOOM 档数={notches}");
}

/// 滚轮增量 → 档数（一格 `WHEEL_DELTA`=120，一次最多 4 档）。
pub(crate) fn wheel_notches(delta: i32) -> i32 {
    (delta / 120).clamp(-4, 4)
}

/// 把滚轮增量攒成"档"（1.8.3）：**零头不再扔掉**。
///
/// 一格滚轮 = 120，但高分辨率滚轮 / 精密触控板每次只发 30–60。
/// 1.8.2 直接 `delta / 120`，零头被丢 → 这类设备上每一发都算出 0 档 →
/// 图片不缩放、正文不滚、目录不滚，**而且日志里一条都不留**（看起来像"功能消失了"）。
///
/// 纯函数便于单测：`acc` 是跨事件累积的零头，本次拿走整档后把余数写回。
pub(crate) fn wheel_take(acc: &mut i32, delta: i32) -> i32 {
    *acc = acc.saturating_add(delta);
    let notches = (*acc / 120).clamp(-4, 4);
    if notches != 0 {
        if notches.abs() == 4 {
            // 触控板一次甩出几百，别把余数留成巨款（否则停止后还会继续滚）。
            *acc = 0;
        } else {
            *acc -= notches * 120;
        }
    }
    // 长时间不滚时零头可能慢慢攒；夹一下防止异常设备把它撑爆。
    *acc = (*acc).clamp(-119, 119);
    notches
}

/// 取本次滚轮对应的档数（带零头累积，见 [`wheel_take`]）。
pub(crate) fn take_wheel_notches(delta: i32) -> isize {
    with_state(|s| wheel_take(&mut s.wheel_acc, delta)).unwrap_or_else(|| wheel_notches(delta))
        as isize
}

/// 客户区坐标（`lparam` 的低/高 16 位是**有符号**的，别直接用 `& 0xFFFF`）。
pub(crate) fn client_xy(lparam: LPARAM) -> (i32, i32) {
    let x = (lparam.0 as i32) as i16 as i32;
    let y = ((lparam.0 >> 16) as i32) as i16 as i32;
    (x, y)
}

/// 按下：记录起点，开始拖拽（内容比窗口大才有意义，但捕获总是设上，
/// 否则鼠标移出窗口后 `WM_MOUSEMOVE` 就断了）。
pub(crate) fn on_drag_start(hwnd: HWND, lparam: LPARAM) {
    trace_drag_start();
    // 拖动期间先收起投影：投影是独立的分层窗口，只按 8 ms 节流跟随，
    // 窗口以 144 Hz 走、投影以 ~60 Hz 走，肉眼就是"窗口在抖、边缘在拖影"（1.4.8）。
    if let Some(shadow) = with_state(|s| s.shadow).flatten() {
        window::hide_shadow(shadow.raw());
    }
    let (x, y) = drag_origin(lparam);
    with_state(|s| {
        s.drag_from = Some((x, y));
        s.dragged = false;
        s.drag_residual = 0;
    });
    window::set_capture(hwnd);
}

/// 拖动用的坐标：**屏幕坐标优先**。
///
/// 1.0.4 踩的坑：拖动时我们一边挪窗口、一边用 client 坐标算位移，两者互相抵消——
/// 鼠标走 90 px，窗口只走 27 px（约 1/3），手感是"拖不动"。改用 `GetCursorPos`
/// 的屏幕坐标后，位移与鼠标 1:1。
pub(crate) fn drag_origin(lparam: LPARAM) -> (i32, i32) {
    window::cursor_pos().unwrap_or_else(|| client_xy(lparam))
}

/// 移动：累积位移超过 3 px 才算"拖动"，之后每帧把增量交给平移 / 滚动。
pub(crate) fn on_drag_move(lparam: LPARAM) {
    let (x, y) = drag_origin(lparam);
    let mut step: Option<(i32, i32)> = None;
    with_state(|s| {
        if let Some((fx, fy)) = s.drag_from {
            let (dx, dy) = (x - fx, y - fy);
            if !s.dragged && (dx.abs() + dy.abs()) >= 3 {
                s.dragged = true;
            }
            if s.dragged {
                s.drag_from = Some((x, y));
                step = Some((dx, dy));
            }
        }
    });
    if let Some((dx, dy)) = step {
        // 1.0.5：左键拖动在**所有内容**上都是"移动窗口"（用户要求文本也能整窗拖动）。
        //
        // 1.0.3 语义：挪的是位图的绘制偏移，实测会频闪——每帧重画一整张位图，
        // 4K 一帧十几毫秒，桌面合成器在半成品上取帧就是闪烁。挪窗口由合成器搬
        // 现成的表面，零重绘，手感也更像"拖一张纸"。
        // 文本的滚动改由滚轮/右键拖动/PageUp·PageDown 承担（见 `on_zoom_drag_*`）。
        move_window_by(dx, dy);
    }
}

/// 拖动窗口：位移直接跟随鼠标（不重绘任何内容）。
pub(crate) fn move_window_by(dx: i32, dy: i32) {
    if dx == 0 && dy == 0 {
        return;
    }
    let Some(preview) = with_state(|s| s.preview) else {
        return;
    };
    // 拖动是"每次鼠标移动都调一次"的热路径：任何一步慢都会直接表现为"窗口追不上鼠标"。
    // 这里量每一步的耗时，超过 5 ms 就记一条 warn（平时不打，免得日志爆炸）。
    let started = Instant::now();
    window::move_by(preview.raw(), dx, dy);
    // 投影是另一个窗口，挪了主窗口就得跟着挪（不跟会拖下一条"鬼影"）；
    // 拖动热路径上节流到 8 ms 一次，松手时 `on_drag_end` 会强制对齐。
    sync_shadow_with(true);
    // 位置记忆（1.0.13）：拖完立刻记，收起时统一落盘。
    remember_position();
    let ms = started.elapsed().as_millis();
    trace_drag_moved(dx, dy);
    trace_drag_cost(ms);
    trace_drag_msg();
    if ms >= 5 {
        log_warn!(
            SCOPE,
            "SLOWMOVE ({dx}, {dy}) {ms}ms —— 拖动跟不上鼠标就查这里"
        );
    }
    log_debug!(SCOPE, "MOVE ({dx}, {dy})");
}

/// 鼠标落在窗口**客户区**的哪几条边（`margin` 像素以内算命中）。纯函数，便于单测。
///
/// 只算客户区：预览窗无边框、`WM_NCCALCSIZE` 返回 0 把整块窗口都当客户区，
/// 所以两者等价，不必再去问 `GetWindowRect`（拖动过程中它每帧都在变）。
///
/// `margin` 由调用方按 DPI 算（1.7.4）：PMv2 下坐标是物理像素，固定 6 px 在
/// 125%/150% 屏上只有 4~5 DIP，"贴不准就拖不动"。
pub(crate) fn edges_at(rc: RECT, x: i32, y: i32, margin: i32) -> ResizeEdges {
    // 1.8.2：图片/视频不允许拖边改大小（`resize_margin` 会返回 0），这里直接短路，
    // 免得 `x <= left && x >= left` 在正好压线时还能抓到一个 0 宽的边。
    if margin <= 0 {
        return ResizeEdges::default();
    }
    let inside = x >= rc.left - margin
        && x <= rc.right + margin
        && y >= rc.top - margin
        && y <= rc.bottom + margin;
    if !inside {
        return ResizeEdges::default();
    }
    // 顶栏也算窗口的一部分：用户要求"顶栏也能拖动改大小"以外的位置本来就该能拖。
    ResizeEdges {
        left: x <= rc.left + margin,
        right: x >= rc.right - margin,
        top: y <= rc.top + margin,
        bottom: y >= rc.bottom - margin,
    }
}

/// 这类预览**不允许拖边改大小**（1.8.2，用户要求）：
/// 「把图片和视频的窗口伸缩功能去掉，滚轮对窗口进行缩放就足够了」——
/// 图片/视频靠滚轮缩放看细节，窗口本身只在打开时按素材比例定一次大小。
pub(crate) fn resizable_kind(kind: Option<format::Kind>) -> bool {
    !matches!(kind, Some(format::Kind::Image) | Some(format::Kind::Video))
}

/// 这台机器/这个窗口上的边缘热区宽度（DIP 8 → 物理像素，最少 6）。
///
/// Excel/Word 预览（1.8.1）：处理器收到鼠标事件后，它的子窗口会盖住内容区，只留下
/// 我们 `SHELL_RING_DIP` 那一圈。这时热区不能还是 8 DIP——超出的部分落到子窗口上，
/// 用户就"抓不到边"。所以把热区收进那一圈里（同时保证 ≥ 正常最小值的 1/2）。
///
/// 图片/视频（1.8.2）：返回 0 = 四条边都不响应，窗口大小固定（缩放交给滚轮）。
pub(crate) fn resize_margin(hwnd: HWND) -> i32 {
    if !resizable_kind(current_kind()) {
        return 0;
    }
    let ring = dpi::scaled(hwnd, SHELL_RING_DIP);
    let live_shell =
        with_state(|s| s.shell_preview.is_some() && !s.shell_owns_input).unwrap_or(false);
    if live_shell {
        return ring.max(dpi::px(RESIZE_MARGIN_DIP) / 2);
    }
    dpi::scaled(hwnd, 8).max(dpi::px(RESIZE_MARGIN_DIP))
}

/// 按下：命中边缘就进入"改大小"模式（否则交给移动窗口）。
pub(crate) fn begin_resize(hwnd: HWND, edges: ResizeEdges, lparam: LPARAM) {
    window::suppress_shadow();
    let (x, y) = drag_origin(lparam);
    let rc = window::window_rect(hwnd);
    with_state(|s| {
        s.resize_edges = Some(edges);
        s.resize_from = Some((x, y, rc));
        // 改大小期间不移动窗口，别让 `on_drag_move` 也跟着跑。
        s.drag_from = None;
        s.dragged = false;
    });
    window::set_capture(hwnd);
    window::set_cursor(edges.cursor());
    log_debug!(SCOPE, "RESIZE 开始（{}）", edges.describe());
}

/// 图片/视频预览改大小（1.8.1）：把"想改到的矩形"夹成**素材的比例**。
///
/// 用户口径：「图片、视频类都不需要拉伸缩放窗口，设计和图片一样的」——即窗口本身按素材
/// 比例长，内容永远铺满，既不拉伸也不留白。`bar` 是视频底部进度条的高度（图片传 0）。
///
/// 哪一维在拖就由哪一维驱动（只拖上下 → 高度驱动；其余 → 宽度驱动），
/// 对面那条边（或中线）保持不动，手感才自然。
///
/// **1.8.2 起这条路径在 UI 上已经走不到了**：图片/视频的 `resize_margin` 返回 0，
/// 拖边不再进入改大小（用户：「把图片和视频的窗口伸缩功能去掉，滚轮缩放就足够了」）。
/// 保留为兜底（别的路径真的改大小时也不会变形），单测照旧覆盖。
pub(crate) fn clamp_media_resize(rc: RECT, edges: ResizeEdges, aspect: f64, bar: i32) -> RECT {
    if !(aspect.is_finite() && aspect > 0.0) {
        return rc;
    }
    let extra = window::chrome_height() + bar;
    let mut out = rc;
    let vertical_only = (edges.top || edges.bottom) && !(edges.left || edges.right);
    let (content_w, content_h) = if vertical_only {
        let ch = ((out.bottom - out.top) - extra).max(min_window_h());
        let cw = (ch as f64 * aspect).round().max(min_window_w() as f64) as i32;
        (cw, ch)
    } else {
        let cw = (out.right - out.left).max(min_window_w());
        let ch = (cw as f64 / aspect).round().max(min_window_h() as f64) as i32;
        (cw, ch)
    };
    // 水平：拖左边 → 右边不动；拖右边 → 左边不动（两种都拖 = 右边不动）；
    // 只拖上下 → 保持水平居中。
    if !(edges.left || edges.right) {
        let center = (out.left + out.right) / 2;
        out.left = center - content_w / 2;
    } else if edges.left {
        out.left = out.right - content_w;
    }
    out.right = out.left + content_w;
    // 垂直：同理（上下两条边的坐标含顶栏与进度条，所以按"内容高 + 附加高"算总高）。
    let total_h = content_h + extra;
    if !(edges.top || edges.bottom) {
        let center = (out.top + out.bottom) / 2;
        out.top = center - total_h / 2;
    } else if edges.top {
        out.top = out.bottom - total_h;
    }
    out.bottom = out.top + total_h;
    out
}

/// 拖动：按"按下时的原始矩形 + 累计位移"重算，而不是每帧累加增量。
///
/// 为什么：一旦某一维被最小尺寸夹住，增量就丢了；之后往回拖，窗口跟不上鼠标，
/// 手感是"卡住一截"。用原始矩形算，怎么夹都不会漂。
pub(crate) fn on_resize_move(lparam: LPARAM) {
    let (x, y) = drag_origin(lparam);
    let (Some((x0, y0, original)), Some(edges)) = (
        with_state(|s| s.resize_from).flatten(),
        with_state(|s| s.resize_edges).flatten(),
    ) else {
        return;
    };
    let (dx, dy) = (x - x0, y - y0);
    let mut rc = original;
    if edges.left {
        rc.left = (original.left + dx).min(original.right - min_window_w());
    }
    if edges.right {
        rc.right = (original.right + dx).max(original.left + min_window_w());
    }
    if edges.top {
        rc.top = (original.top + dy).min(original.bottom - min_window_h());
    }
    if edges.bottom {
        rc.bottom = (original.bottom + dy).max(original.top + min_window_h());
    }
    // 图片/视频（1.8.1）：窗口比例锁死素材比例——无论拖哪条边，先夹回等比再落地。
    // （1.8.2 起拖边这条路对图片/视频已关闭，这里保留兜底：万一别处真的在改大小，
    //  也不会把素材拉变形。）
    if let Some(aspect) = with_state(|s| s.media_aspect).flatten() {
        let bar = current_kind().map(media_bar).unwrap_or(0);
        rc = clamp_media_resize(rc, edges, aspect, bar);
    }
    window::resize_to(
        with_state(|s| s.preview)
            .map(|h| h.raw())
            .unwrap_or_default(),
        rc.left,
        rc.top,
        rc.right - rc.left,
        rc.bottom - rc.top,
    );
    sync_shadow();
    log_debug!(
        SCOPE,
        "RESIZE {}x{}",
        rc.right - rc.left,
        rc.bottom - rc.top
    );
}

/// 窗口客户区尺寸 → 内容区尺寸（扣掉顶栏，下限 1）。
///
/// 全局约定：`request_render` 与 `s.base` 都用**内容区**尺寸（见 `view_size()`、
/// `show_preview` 里 `layout::reserve_for_window`、`set_zoom_to` 里 `+ reserve`）。
/// 拖边结束时以前把整窗高度当内容高度传下去，于是位图比内容区高 30px →
/// 上屏走拉伸（图片/PDF 拖完发虚），`base` 也高 30px → 之后每缩放一次高度再漂 30px。
pub(crate) fn content_size_of(client: RECT) -> (u32, u32) {
    layout::content_size_of(client)
}

/// 内容区尺寸 → 渲染盒（已扣掉滚动条、已按缩放放大）。
///
/// 跟 `view_size()` 同一个口径：`view_size` 是"窗口已经有了，直接量绘制用的内容区"，
/// 这里是"窗口还没开，按算出来的尺寸推"。两边算出来的必须是同一个数，
/// 否则页缓存 key 对不上、预热也永远打不中（1.3.0 的坑）。
///
/// 1.8.3 起两边都由 `layout::Layout` 产出：这里只是"没有窗口时"的入口。
///
/// `reserve` 是"整窗高里有多少不属于内容"（按素材尺寸开窗的格式是顶栏，
/// 文本/Markdown 是 0）。**上屏（`show_preview`）与预热（`maybe_prewarm`）必须传同一个值**，
/// 否则两个渲染盒差一条 `chrome_height`，预热结果永远对不上缓存键（±2 px 容差），
/// 就变成"白烧 CPU 却没有任何加速"——1.8.3 修过一次（见 `docs/dev/LOG.md`）。
pub(crate) fn render_box(
    kind: format::Kind,
    pages: Option<usize>,
    width: i32,
    height: i32,
    reserve: i32,
    zoom: f32,
) -> (u32, u32) {
    layout::from_window_size(
        width,
        height,
        reserve,
        layout::Opts {
            kind,
            pages,
            zoom,
            toc: false,
            bar: layout::Bar::Auto,
        },
    )
    .render_box
}

/// 抬起：结束改大小，并按新尺寸重画（位图内容需要重新光栅化到新尺寸）。
pub(crate) fn finish_resize(edges: ResizeEdges) {
    window::allow_shadow();
    window::release_capture();
    let Some((preview, path, page)) = with_state(|s| (s.preview, s.last_path.clone(), s.page))
    else {
        return;
    };
    let size = window::client_rect(preview.raw());
    let (width, height) = content_size_of(size);
    log_info!(
        SCOPE,
        "RESIZE 结束 {}x{}（{}）",
        width,
        height,
        edges.describe()
    );
    // 窗口尺寸变了，原来的"zoom 基准"就不成立了：把新尺寸当成新的 1.0×。
    with_state(|s| {
        s.base = Some((width, height));
        s.zoom = 1.0;
        s.pan = (0, 0);
        s.zoom_preview = false;
    });
    if let Some(path) = path {
        request_render(&path, width, height, page);
    }
    sync_shadow();
    // 位置记忆（1.0.13）：拖左边/上边会同时移动位置，一并记下。
    remember_position();
    // 尺寸记忆（1.0.14）：拖边改出来的大小下次照用。
    remember_size();
    // 拖边过程中 `WM_SIZE` 故意不同步内容（每像素重开一次太慢），松手补一次；
    // 同样只能投递（`WM_LBUTTONUP` 也在输入同步呼叫里）。
    window::post(preview.raw(), ipc::WM_QLR_SHELLFIT);
    window::invalidate(preview.raw());
}

/// 文本拖拽滚动：把像素位移折算成"行"。
///
/// 攒零头是必须的：`window::line_height()` = 24 px，鼠标一次只动 3–5 px，
/// 每帧取整会让慢速拖动完全不动（体验上像"拖不动"）。
pub(crate) fn scroll_text_by_pixels(pixels: i32) -> bool {
    if !matches!(get_content().as_deref(), Some(Canvas::Text(_))) {
        return false;
    }
    let rows = with_state(|s| pixels_to_rows(&mut s.drag_residual, pixels)).unwrap_or(0);
    if rows != 0 {
        scroll_text_by(rows);
    }
    // 是文本内容就一律"已处理"，别退回平移（文本没有可平移的位图）。
    true
}

/// 像素位移 + 零头 → 要滚几行（负数 = 向上）。纯函数，便于单测。
pub(crate) fn pixels_to_rows(residual: &mut i32, pixels: i32) -> isize {
    *residual += pixels;
    let rows = *residual / window::line_height();
    *residual -= rows * window::line_height();
    rows as isize
}

/// 抬起：**只结束拖拽**。1.0.3 起左键不再关闭窗口（用户要求：左键专用于拖动，
/// 长文件靠拖拽下滑；关窗用 Esc / 再次按空格 / 托盘菜单）。
pub(crate) fn on_drag_end() {
    let dragged = with_state(|s| {
        let d = s.dragged;
        s.drag_from = None;
        s.dragged = false;
        s.drag_residual = 0;
        d
    })
    .unwrap_or(false);
    window::release_capture();
    window::allow_shadow();
    trace_drag_end();
    if dragged {
        // 抖动结束：把拖动期间被节流掉的投影对齐补回来。
        sync_shadow_with(false);
        log_debug!(SCOPE, "拖拽结束");
    }
}

/// 右键按下（1.0.5，1.0.8 收窄）：
/// - **PDF**：记住缩放锚点，上下拖 = 缩放；
/// - **文本/Markdown**：记住滚动锚点，上下拖 = 滚动（左键专职挪窗口）；
/// - **图片**：什么都不做——用户反馈"图片的右键缩放可以取消掉"（图片用滚轮就够，
///   右键拖动留着空，免得误触把窗口尺寸改了）。
///
/// 锚点用**屏幕 y**：缩放会改窗口位置，client y 会跟着漂，
/// 1.0.4 之前用 client y 时右拖缩放会来回抖（1.05 → 0.87 → 1.17 …）。
pub(crate) fn on_zoom_drag_start(hwnd: HWND, lparam: LPARAM) {
    let (_, y) = drag_origin(lparam);
    window::set_capture(hwnd);
    if matches!(get_content().as_deref(), Some(Canvas::Text(_))) {
        with_state(|s| {
            s.scroll_anchor = Some(y);
            s.drag_residual = 0;
        });
        return;
    }
    // 1.0.9：PDF 的右键缩放先整体关掉（用户原话"pdf 先关闭右键缩放的逻辑"）。
    // 缩放改由顶栏/快捷键做，右键只保留文本滚动；图片的右键缩放 1.0.8 已撤。
    window::release_capture();
}

/// 右键拖动：图像往上是放大（每 60 px 一档 1.25×）；文本往上是往上滚。
pub(crate) fn on_zoom_drag_move(lparam: LPARAM) {
    let (_, y) = drag_origin(lparam);
    // 文本：右键上下拖 = 按行滚动。
    if let Some(anchor_y) = with_state(|s| s.scroll_anchor).flatten() {
        let dy = y - anchor_y;
        if dy != 0 {
            with_state(|s| s.scroll_anchor = Some(y));
            scroll_text_by_pixels(-dy);
        }
        return;
    }
    let Some((anchor_y, anchor_zoom)) = with_state(|s| s.zoom_anchor).flatten() else {
        return;
    };
    let dy = (anchor_y - y) as f32; // 往上拖为正
    let target = zoom_drag_target(anchor_zoom, dy);
    let current = with_state(|s| s.zoom).unwrap_or(1.0);
    // 变化不到 2% 就别动：省下无意义的窗口重排。
    if (target / current - 1.0).abs() < 0.02 {
        return;
    }
    // 拖动期间只改窗口、不重渲染（1.0.5）：每一帧重渲染一页 PDF 就是"卡顿"的来源。
    set_zoom_to(target, current, false);
    // 缩放后重新锚定：增量式拖动，松手回拖不会"跳"。
    with_state(|s| s.zoom_anchor = Some((y, target)));
}

/// 右键拖动 → 目标缩放（纯函数，便于单测）。
///
/// 往上拖（`dy > 0`）放大、往下拖缩小，每 60 px 一档 1.25×——和老滚轮手感一致。
pub(crate) fn zoom_drag_target(anchor_zoom: f32, dy: f32) -> f32 {
    (anchor_zoom * 1.25_f32.powf(dy / 60.0)).clamp(ZOOM_MIN, ZOOM_MAX)
}

/// 右键抬起：结束缩放/滚动，**缩放这时候才真正渲染**（1.0.5）。
///
/// 拖动期间只改窗口 + 拉伸旧位图，松手才按最终尺寸出一张高清图：一次渲染，
/// 而不是每一帧一次。
pub(crate) fn on_zoom_drag_end() {
    let (zooming, scrolling) = with_state(|s| {
        let zooming = s.zoom_anchor.take().is_some();
        let scrolling = s.scroll_anchor.take().is_some();
        s.drag_residual = 0;
        (zooming, scrolling)
    })
    .unwrap_or((false, false));
    window::release_capture();
    if scrolling && !zooming {
        log_debug!(
            SCOPE,
            "右键滚动结束 scroll={}",
            with_state(|s| s.scroll).unwrap_or(0)
        );
        return;
    }
    if !zooming {
        return;
    }
    let zoom = with_state(|s| s.zoom).unwrap_or(1.0);
    // 预览态还没被新帧顶掉 → 补一次真正的渲染。
    let pending = with_state(|s| s.zoom_preview).unwrap_or(false);
    if pending && let Some((preview, Some(path))) = with_state(|s| (s.preview, s.last_path.clone()))
    {
        let (width, height) = view_size(preview.raw());
        let page = with_state(|s| s.page).unwrap_or(0);
        request_render(&path, width, height, page);
        log_debug!(SCOPE, "ZOOM-DRAG 松手补渲染 {width}x{height}");
    }
    log_info!(SCOPE, "ZOOM-DRAG 结束 zoom={zoom:.2}");
}

/// `WM_MOUSEMOVE` 分流：放大镜（Shift）优先，其次右键（缩放/滚动），否则左键拖动（挪窗口）。
pub(crate) fn on_mouse_move(lparam: LPARAM) {
    // 放大镜：按住 Shift 就在光标处开镜头，松开即关。它只是叠一层放大，不影响其他手势，
    // 所以先处理、并且不 return —— 用户可能一边按 Shift 一边拖动。
    update_lens(lparam);
    // 正在拖边改大小（1.0.10）：这时只改窗口矩形，不挪窗口、不平移画面。
    if with_state(|s| s.resize_edges.is_some()).unwrap_or(false) {
        on_resize_move(lparam);
        return;
    }
    // 正在拖滚动条（1.0.9）：这时只改滚动位置，不挪窗口、不平移画面。
    if with_state(|s| s.scroll_drag).unwrap_or(false) {
        let (_, y) = client_xy(lparam);
        if let Some(preview) = with_state(|s| s.preview) {
            apply_scroll_y(preview.raw(), y);
        }
        return;
    }
    let (zooming, scrolling) = with_state(|s| (s.zoom_anchor.is_some(), s.scroll_anchor.is_some()))
        .unwrap_or((false, false));
    if zooming || scrolling {
        on_zoom_drag_move(lparam);
    } else {
        on_drag_move(lparam);
    }
}

/// 按 Shift 状态更新放大镜位置；有变化才重绘。
///
/// 只有位图（图片 / PDF）才需要放大——文本本来就是矢量字，放大只会变模糊。
pub(crate) fn update_lens(lparam: LPARAM) {
    let Some(preview) = with_state(|s| s.preview) else {
        return;
    };
    if !with_state(|s| s.visible).unwrap_or(false) {
        return;
    }
    let want = lens_down() && matches!(get_content().as_deref(), Some(Canvas::Bitmap(_)));
    if !want {
        let changed = with_state(|s| {
            if s.lens.take().is_some() {
                log_debug!(SCOPE, "LENS 关");
                true
            } else {
                false
            }
        })
        .unwrap_or(false);
        if changed {
            window::invalidate(preview.raw());
        }
        return;
    }
    let (x, y) = client_xy(lparam);
    let changed = with_state(|s| {
        if s.lens == Some((x, y)) {
            false
        } else {
            let first = s.lens.is_none();
            s.lens = Some((x, y));
            if first {
                log_debug!(SCOPE, "LENS 开 {x}x{y} zoom={:.1}×", s.lens_zoom);
            }
            true
        }
    })
    .unwrap_or(false);
    if changed {
        window::invalidate(preview.raw());
    }
}

/// 放大镜的"按住"键此刻是否按着（1.0.7 起可配置，默认 Shift）。
///
/// 不用 `window::shift_down()`：设置页允许把放大镜换成 Ctrl/Alt，
/// 而"哪个键"的真值只在 `config.hotkeys.lens_hold` 里。
pub(crate) fn lens_down() -> bool {
    let Some(binding) = with_state(|s| s.config.hotkeys.lens_binding()) else {
        return window::shift_down();
    };
    window::key_down(binding.vk)
}

/// 放大镜键 + 滚轮 = 调放大镜倍数；返回 true 表示这次滚轮已被放大镜吃掉。
pub(crate) fn lens_wheel(notches: isize) -> bool {
    if !lens_down() {
        return false;
    }
    let Some(preview) = with_state(|s| s.preview) else {
        return false;
    };
    if !matches!(get_content().as_deref(), Some(Canvas::Bitmap(_))) {
        return false;
    }
    let zoom = with_state(|s| {
        s.lens_zoom = (s.lens_zoom + notches as f32).clamp(LENS_MIN_ZOOM, LENS_MAX_ZOOM);
        s.lens_zoom
    })
    .unwrap_or(LENS_DEFAULT_ZOOM);
    log_debug!(SCOPE, "LENS 倍数 {zoom:.1}×");
    window::invalidate(preview.raw());
    true
}

/// 平移（像素）。只有位图大于客户区时才有视觉效果。
pub(crate) fn pan_by(dx: i32, dy: i32) {
    let Some((preview, rc, zoom)) =
        with_state(|s| (s.preview, window::client_rect(s.preview.raw()), s.zoom))
    else {
        return;
    };
    let (cw, ch) = ((rc.right - rc.left).max(1), (rc.bottom - rc.top).max(1));
    // 每一步挪 1/12 屏，和缩放倍数无关（用户按的是"屏"的直觉，不是像素）。
    let (step_x, step_y) = ((cw / 12).max(8), (ch / 12).max(8));
    with_state(|s| {
        let (bw, bh) = (
            scaled(cw as u32, zoom) as i32,
            scaled(ch as u32, zoom) as i32,
        );
        s.pan.0 = (s.pan.0 + dx * step_x).clamp(0, (bw - cw).max(0));
        s.pan.1 = (s.pan.1 + dy * step_y).clamp(0, (bh - ch).max(0));
    });
    window::invalidate(preview.raw());
    log_info!(
        SCOPE,
        "PAN {:?} zoom={zoom:.2}",
        with_state(|s| s.pan).unwrap_or((0, 0))
    );
}

/// 在当前文档里翻 delta 页（目前只有 PDF 有页的概念）。
pub(crate) fn step_page(delta: isize) {
    let Some((preview, path, pages, page)) =
        with_state(|s| (s.preview, s.last_path.clone(), s.pages, s.page))
    else {
        return;
    };
    let Some(pages) = pages.filter(|n| *n > 1) else {
        return;
    };
    let Some(path) = path else { return };
    let target = (page as isize + delta).clamp(0, pages as isize - 1) as usize;
    if target == page {
        log_debug!(SCOPE, "已经在第 {} 页，忽略翻页", page + 1);
        return;
    }
    with_state(|s| {
        s.page = target;
        // 换页从页顶看起（滚轮滚到页底再往下滚就翻页，1.0.3）。
        s.pan = (0, 0);
    });

    let (width, height) = view_size(preview.raw());
    request_render(&path, width, height, target);

    // 1.0.8 修"翻页频闪"：以前这里立刻把内容改成占位文字再 invalidate，
    // 新位图回来之前窗口就闪一下白底+文字。现在**什么都不动**——旧的一页继续留在
    // 屏幕上，等 `on_render_done` 拿到新页再整帧换掉（顶部页码也一起更新）。
    log_info!(SCOPE, "PAGE {}/{}", target + 1, pages);
}

/// 跳到第 `page` 页（1 基，越界会被夹住）。
pub(crate) fn goto_page(page: usize) {
    let (preview, path, pages) = match with_state(|s| (s.preview, s.last_path.clone(), s.pages)) {
        Some(triple) => triple,
        None => return,
    };
    let (Some(path), Some(total)) = (path, pages) else {
        return;
    };
    let target = page.clamp(1, total);
    let changing = with_state(|s| {
        let changed = s.page + 1 != target;
        s.page = target - 1;
        s.pan = (0, 0);
        s.zoom_preview = false;
        changed
    })
    .unwrap_or(false);
    if !changing {
        return;
    }
    let (width, height) = view_size(preview.raw());
    request_render(&path, width, height, target - 1);
    window::invalidate(preview.raw());
    log_info!(SCOPE, "PAGE {}/{}（跳转）", target, total);
}

/// 退出页码编辑模式（`submit` 为真时先按输入内容跳页）。
pub(crate) fn finish_page_edit(submit: bool) {
    let text = with_state(|s| s.page_edit.take()).flatten();
    hotkey::set_capture_digits(false);
    let Some(text) = text else { return };
    if submit && let Ok(page) = text.trim().parse::<usize>() {
        goto_page(page);
    }
    if let Some(preview) = with_state(|s| s.preview) {
        window::invalidate(preview.raw());
    }
}

/// 页码输入模式下的按键（数字/退格/回车/Esc）。返回 `true` 表示这个键被吃掉了。
///
/// 为什么要单独处理：预览窗是 `WS_EX_NOACTIVATE`，永远拿不到键盘焦点，
/// 数字键是 `WH_KEYBOARD_LL` 钩子转进来的（`hotkey::set_capture_digits`）。
pub(crate) fn page_edit_key(vk: usize) -> bool {
    let digit = match vk as u32 {
        0x30..=0x39 => Some((vk as u32 - 0x30) as u8),
        0x60..=0x69 => Some((vk as u32 - 0x60) as u8),
        _ => None,
    };
    if let Some(digit) = digit {
        with_state(|s| {
            if let Some(text) = s.page_edit.as_mut()
                && text.len() < 6
            {
                text.push(char::from(b'0' + digit));
            }
        });
    } else {
        match vk as u32 {
            0x08 => {
                with_state(|s| {
                    if let Some(text) = s.page_edit.as_mut() {
                        text.pop();
                    }
                });
            }
            0x0D => {
                finish_page_edit(true);
                return true;
            }
            0x1B => {
                finish_page_edit(false);
                return true;
            }
            _ => return false,
        }
    }
    if let Some(preview) = with_state(|s| s.preview) {
        window::invalidate(preview.raw());
    }
    true
}

/// 空格热键命中（由钩子线程 `PostMessage` 过来，跑在 UI 线程上，所以能安全调 COM）。
///
/// `vk` = 虚拟键码，`pressed_at` 是钩子按下时记的 tick（`GetTickCount`），
/// 用来量"钩子 → 到手"这段延迟——它应该是个位数毫秒，涨上去说明 UI 线程被什么阻塞了。
pub(crate) fn on_hotkey(vk: usize, pressed_at: u32) {
    // 真实空格/翻页键从钩子线程过来，不经过 `apply_request`，延迟起点要在这里重置。
    begin_request();
    // 页码输入（1.0.9）：预览窗没有键盘焦点，数字键是钩子转发过来的。
    if with_state(|s| s.page_edit.is_some()).unwrap_or(false) && page_edit_key(vk) {
        return;
    }
    // 音频（1.7.9）：P 播放/暂停，左右键快退/快进 5 秒。
    // 必须放在 `page_delta_for` 之前：左右键在那条路上会被当成"上一页/下一页"，
    // 而音频没有页——最坏情况是翻页逻辑把窗口内容换成占位文字。
    if playback_active() {
        match vk as u32 {
            0x50 => {
                playback_toggle();
                return;
            }
            0x25 => {
                playback_seek(-AUDIO_SEEK_STEP_MS);
                return;
            }
            0x27 => {
                playback_seek(AUDIO_SEEK_STEP_MS);
                return;
            }
            _ => {}
        }
    }
    // 翻页键（PageDown/PageUp/右/左）：钩子只旁听不吞，走到这里就直接翻页。
    if let Some(delta) = page_delta_for(vk) {
        turn_page(delta);
        return;
    }
    match vk as u32 {
        // + / = 放大，- 缩小，0 复位；上下方向键纵向平移（放大后才有意义）。
        0xBB | 0x6B => return set_zoom(1.25),
        0xBD | 0x6D => return set_zoom(0.8),
        0x30 | 0x60 => return zoom_reset(),
        0x28 => {
            if !scroll_text_by(1) {
                pan_by(0, 1);
            }
            return;
        }
        0x26 => {
            if !scroll_text_by(-1) {
                pan_by(0, -1);
            }
            return;
        }
        _ => {}
    }

    let now = window::now_ms();
    let hook_to_handler = now.wrapping_sub(pressed_at);
    let found = selection::current_selection();
    log_info!(
        SCOPE,
        "HOTKEY vk=0x{:02X} fg={} hook_to_handler={}ms {}",
        vk,
        hotkey::foreground_class().unwrap_or_else(|| "(无)".to_string()),
        hook_to_handler,
        found.summary()
    );
    match found.first().cloned() {
        Some(path) => toggle(Some(path)),
        None => {
            // 没有选中项：窗口开着就当"再按一次空格收起"；否则**把空格还回去**。
            //
            // 钩子为了拿选中项必须先吞键（回调里做不了 COM），如果这里什么也不做，
            // 用户就会觉得"空格按了没反应、键盘坏了"——所以必须重放。
            if with_state(|s| s.visible).unwrap_or(false) {
                // 1.0.10：开了"阻止关闭"之后这条路径也要挡住（它不经过 `toggle`）。
                if !close_blocked("空格") {
                    hide_preview();
                }
            } else {
                log_debug!(SCOPE, "热键命中但没有选中项，重放按键让系统继续处理");
                hotkey::replay(vk as u32);
            }
        }
    }
}
