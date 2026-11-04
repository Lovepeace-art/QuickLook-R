//! 系统预览处理器（`IPreviewHandler`，Office 那一类）的编排：
//! 创建 / 退役 / 换尺寸 / 焦点 / 翻页，以及"这个文件交给谁来画"的判定。

use super::*;

/// 试一把系统预览处理器（1.4.1）：只对"内置四类之外、且有插件接管"的文件尝试，
/// 因为这类文件我们自己画得最简陋，系统处理器画得最好。
///
/// 成功返回 true（调用方跳过插件渲染），失败返回 false（照常走插件卡片）。
/// 只判断"要不要试系统预览处理器"，不做任何 COM 调用（COM 部分要等窗口显示后再做）。
pub(crate) fn shell_candidate(path: Option<&Path>) -> bool {
    let Some(path) = path else {
        return false;
    };
    let kind = format::kind_for_path(path);
    if qlr_render::viewer_for(kind).is_some() {
        // 图片/PDF/文本/Markdown 由我们自己画，且我们的交互（缩放/滚动/目录）比处理器好。
        return false;
    }
    if qlr_core::plugins::host().find_for_path(path).is_none() {
        // 没插件 = 本来就不支持（预览窗根本不会开），不用试。
        return false;
    }
    shellpreview::has_handler(path)
}

/// 系统预览处理器的"延迟拆卸"队列。
///
/// 为什么不能就地 `Drop`：`ShellPreview::drop` 要 `Unload()`，而收起窗口的调用链常常发生在
/// **`WM_COPYDATA` 处理期间**（另一个进程用 `SendMessage` 同步等我们）。在这种调用栈里进 COM
/// 会死锁（表现为窗口留在屏幕上、日志断在这里、进程"还活着"但不再响应）。
/// 所以：把句柄挪进这个队列，`PostMessage(WM_QLR_INVOKE)` 回到自己的消息循环里再卸。
pub(crate) static SHELL_TRASH: OnceLock<Mutex<Vec<shellpreview::ShellPreview>>> = OnceLock::new();

pub(crate) fn shell_trash() -> &'static Mutex<Vec<shellpreview::ShellPreview>> {
    SHELL_TRASH.get_or_init(|| Mutex::new(Vec::new()))
}

pub(crate) fn retire_shell_preview(handle: Option<shellpreview::ShellPreview>) {
    let Some(handle) = handle else {
        return;
    };
    if let Ok(mut slot) = shell_trash().lock() {
        slot.push(handle);
    }
    // 提醒消息循环来收尸（posted，不是 send —— 这里绝不能是同步调用）。
    if let Some(host) = with_state(|s| s.host) {
        window::post(host.raw(), ipc::WM_QLR_INVOKE);
    }
}

/// 在自己的消息循环里真正卸掉处理器（此时已脱离 `WM_COPYDATA` 调用栈）。
pub(crate) fn drain_shell_trash() {
    let pending: Vec<shellpreview::ShellPreview> = match shell_trash().lock() {
        Ok(mut slot) => std::mem::take(&mut *slot),
        Err(_) => return,
    };
    if pending.is_empty() {
        return;
    }
    let count = pending.len();
    drop(pending);
    log_debug!(SCOPE, "SHELL 预览已卸下 {count} 个");
}

/// 换到"不需要系统预览处理器"的文件时，把上一个文件的处理器当场卸掉（1.7.5）。
///
/// `open_shell_preview` 只在"这次也要用处理器"时才会顺带收尸，于是从 docx/pptx/xlsx
/// 切到 zip/图片/文本时，处理器的子窗口会留在预览窗里继续盖着内容——用户看到的是
/// 「顶栏是新的文件名、正文还是上一个文件」；拖动/改大小时那片残留子窗口由别人按旧尺寸
/// 重绘，还会出现竖条纹。调用点：`show_preview` 的压缩包分支 + 非 `shell_wanted` 的主路径。
pub(crate) fn drop_stale_shell_preview() {
    // 注意：不能就地 drop（会把 COM 带进 `WM_COPYDATA` 的同步调用栈里，见 SHELL_TRASH 注释）。
    let stale = with_state(|s| s.shell_preview.take()).flatten();
    if stale.is_none() {
        return;
    }
    retire_shell_preview(stale);
    with_state(|s| {
        s.slide = None;
        s.shell_input_pending = false;
    });
    if let Some(preview) = with_state(|s| s.preview) {
        window::stop_timer(preview.raw(), TIMER_SHELL_INPUT);
    }
    // 处理器卸掉了：滚轮回到我们自己手里（缩放/滚文本），窗口也回到"不抢焦点"。
    if let Some(preview) = with_state(|s| s.preview) {
        window::set_activatable(preview.raw(), false);
    }
    hotkey::set_wheel_pass_through(false);
    log_debug!(SCOPE, "换文件：卸掉上一个文件的系统预览处理器");
}

pub(crate) fn open_shell_preview(path: Option<&Path>) -> bool {
    // 上一个"遮盖用"的旧处理器先卸掉（换两次文件可能都没走到换帧定时器）。
    let cover = with_state(|s| s.shell_cover.take()).flatten();
    retire_shell_preview(cover);
    // 换文件时（`hold_frame` 有效）**先不卸旧的**：让它继续盖着内容区，等新的处理器画出
    // 第一帧再换（`TIMER_SHELL_SWAP`）。否则中间几十到几百毫秒露出空白/占位页。
    let hold = with_state(|s| s.hold_frame).unwrap_or(false);
    let mut old = with_state(|s| s.shell_preview.take()).flatten();
    if !hold {
        // 注意：不能就地 drop（会把 COM 带进 `WM_COPYDATA` 的同步调用栈里，见 SHELL_TRASH 注释）。
        retire_shell_preview(old.take());
    }
    with_state(|s| s.slide = None);
    let Some(path) = path else {
        retire_shell_preview(old.take());
        return false;
    };
    let Some(preview) = with_state(|s| s.preview) else {
        retire_shell_preview(old.take());
        return false;
    };
    // 子窗口的矩形要**在这里就定下来**（不接管输入时内缩一圈）。
    //
    // 为什么不能等打开后再 `sync_shell_rect()` 去调：`set_rect` 会走
    // `IPreviewHandler::SetWindow`，而 Excel 的预览（OLE 就地激活）在刚 `DoPreview`
    // 之后是**不能立刻再进 COM** 的——实测直接卡死（`Responding=False`，日志停在
    // "SHELL 预览已接管"，用户看到的就是占位页不动 + 滚动条全废）。
    let owns_input = is_slides(path);
    with_state(|s| s.shell_owns_input = owns_input);
    // Office（Word/Excel）的预览是 OLE 就地激活对象：容器窗口必须先"可激活"，
    // 否则滚轮/滚动条/选区全都不响应（对比：资源管理器预览窗格里同一个 xlsx 可以滚）。
    window::set_activatable(preview.raw(), !owns_input);
    let content = shell_content_rect(preview.raw(), owns_input);
    match shellpreview::ShellPreview::open(path, preview.raw(), content) {
        Some(handle) => {
            with_state(|s| s.shell_preview = Some(handle));
            // 旧内容顶到最前面盖住新的（新子窗口建出来就在最上层，会被它挡住）。
            if let Some(previous) = old.take() {
                previous.raise();
                with_state(|s| s.shell_cover = Some(previous));
                window::start_timer(preview.raw(), TIMER_SHELL_SWAP, SHELL_SWAP_DELAY_MS);
                log_debug!(SCOPE, "换帧：旧内容继续盖住，等新处理器出首帧");
            }
            log_info!(
                SCOPE,
                "SHELL 预览已接管 {}（内容由系统预览处理器绘制）",
                path.display()
            );
            // 输入接管（1.7.4 起，1.7.7 收窄到"只有 PPT"）。
            //
            // 历史：1.7.4 为了让 Office 预览也能拖边改大小，对所有处理器都
            // `EnableWindow(宿主, FALSE)`。代价是 **Excel/Word 的滚动条、工作表标签、选区全都点不动**
            // （用户反馈"滚动栏都不可用"）。1.7.7 改成：
            //   - PPT：整块接管（翻页由我们计数，点一下就翻页会让计数失准）；
            //   - 其余：**不接管**，只把子窗口内缩一圈（`sync_shell_rect`），窗口四条边照样能拖。
            //
            // 另外不再按固定 400 ms 猜：Excel 的 `DoPreview` 实测要 768 ms，
            // 猜早了就露出占位页（用户反馈"Excel 还有那个占位页"）。改成轮询
            // `ShellPreview::visible_area`，画出来才收尾（`TIMER_SHELL_INPUT` 每次 120 ms）。
            let owns_input = is_slides(path);
            // 不接管输入的（Word/Excel）把滚轮**放行**给内容子窗口，让处理器自己滚；
            // 接管了的（PPT）继续由我们吞滚轮做翻页。
            hotkey::set_wheel_pass_through(!owns_input);
            with_state(|s| {
                s.shell_input_pending = owns_input;
                s.shell_poll = 0;
            });
            if let Some(preview) = with_state(|s| s.preview) {
                window::start_timer(preview.raw(), TIMER_SHELL_INPUT, SHELL_POLL_MS);
            }
            // PPT：翻页也由我们接管。为什么必须接管：处理器的"放映"没有"跳页"接口，
            // 我们只能给它投 `VK_NEXT`/`VK_PRIOR`；而它自己**翻过最后一页就切到黑屏
            // "放映结束"**（用户截图反馈的"PPT 最后一页"）。接管后由我们数页、到头停住并提示。
            if let Some(total) = slide_count(path) {
                with_state(|s| s.slide = Some((1, total)));
                log_info!(
                    SCOPE,
                    "SHELL 接管翻页：共 {total} 页（PageDown/PageUp/滚轮）"
                );
            }
            true
        }
        None => {
            // 新处理器起不来：旧的还得卸掉（否则它会一直盖着插件渲染出来的内容）。
            retire_shell_preview(old.take());
            log_info!(SCOPE, "SHELL 预览不可用 {}（退回插件卡片）", path.display());
            false
        }
    }
}

/// 这个文件是不是"一页一页放"的演示文稿（PPT 家族）。
///
/// 只认 pptx 家族：它们内部是 zip，页数能从目录里数出来；`.ppt`（旧二进制格式）数不出来，
/// 交回给系统处理器自己放（不接管就不会误判页码）。
pub(crate) fn is_slides(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("pptx" | "pptm" | "potx" | "potm" | "ppsx" | "ppsm")
    )
}

/// 数 pptx 里有多少页（`ppt/slides/slideN.xml` 的条目数）。数不出来返回 `None`。
pub(crate) fn slide_count(path: &Path) -> Option<usize> {
    if !is_slides(path) {
        return None;
    }
    let archive = qlr_render::archive::open(path).ok()?;
    let total = archive
        .entries
        .iter()
        .filter(|entry| {
            !entry.is_dir
                && entry.path.starts_with("ppt/slides/slide")
                && entry.path.ends_with(".xml")
        })
        .count();
    (total > 0).then_some(total)
}

/// 翻一页 PPT（我们自己接管的通道）。到头就停住并给提示。
///
/// 返回 true = 这次翻页请求已经被"幻灯片通道"吃掉了，调用方不要再走文本/PDF 那套。
pub(crate) fn step_slide(delta: isize) -> bool {
    let Some((current, total)) = with_state(|s| s.slide).flatten() else {
        return false;
    };
    let Some(hwnd) = with_state(|s| s.preview).map(|p| p.raw()) else {
        return false;
    };
    let next = (current as isize + delta).clamp(1, total as isize) as usize;
    if next == current {
        // 到头了：**不再投给处理器**（它翻过最后一页会切到黑屏"放映结束"），
        // 只在顶栏给一句提示——这就是用户要的"到最后一页就停住并提示"。
        let text = if delta > 0 {
            "已是最后一页"
        } else {
            "已是第一页"
        };
        show_toast(hwnd, text);
        log_debug!(SCOPE, "SHELL 翻页：{text}（{current}/{total}）");
        return true;
    }
    let went = with_state(|s| {
        s.shell_preview
            .as_ref()
            .map(|handle| handle.step(delta > 0))
            .unwrap_or(false)
    })
    .unwrap_or(false);
    if !went {
        // 处理器不认按键（换了别的预览器）：别给用户"按了没反应"的错觉。
        show_toast(hwnd, "这个演示文稿不支持翻页");
        return true;
    }
    with_state(|s| s.slide = Some((next, total)));
    window::invalidate(hwnd);
    log_debug!(SCOPE, "SHELL 翻页：{next}/{total}");
    true
}

/// 窗口尺寸稳下来之后：让系统预览处理器的内容跟上新尺寸（1.7.9）。
///
/// 背景（用户反馈「窗口伸缩会引起白边」）：Word/Excel 的预览是 OLE 就地激活对象，
/// 它自己建的子窗口**不会**跟着我们的窗口变——把预览窗拉宽，右边就多出一条空白。
/// （1.7.2 时代 Office 走插件位图渲染，位图会随窗口重画，所以那时候没这个问题。）
///
/// **试过但不可行的路子**（都实机验证过，别再走一遍）：
/// - 在 `WM_SIZE` 里直接 `SetWindow/SetRect`：被系统拒（`0x8001010D`
///   `RPC_E_CANTCALLOUT_ININPUTSYNCCALL`，那时还在输入同步呼叫里）。
/// - 投递一条消息、等退栈后再 `SetWindow/SetRect` + 手工撑 `EXCEL9` + 兜底 `fill_child`：
///   进程**卡死**（事件日志 `Application Hang` 1002）。Excel 就地激活对象在改大小路径上
///   进 COM，很容易自等（同一类坑见 CONVENTIONS #139）。
/// - 照原版 QuickLook `PreviewHandlerHost.OnResize` 那样只调 `SetRect`（它靠"下一个
///   resize 事件再试一次"兜住 `0x8001010D`）：我们也卡死——原版宿主在 WinForms 里、
///   处理器常驻 `prevhost`，跨进程调用不会把 UI 线程拖住；我们是同进程调用。
///
/// 所以唯一可行的是：**等输入同步呼叫退栈后**（投递消息里）再调 `SetRect`，**并且全程不持锁**
/// （`SetRect` 会把消息泵回我们自己的窗口过程，持锁调用必然自等——CONVENTIONS #139 那一族；
/// 之前的卡死就是这一条，不是 COM 本身不行）。
/// 这样改大小只是"让处理器重新排版"，不重开预览、不重新 `DoPreview`、不丢页码。
pub(crate) fn fit_shell_to_window() {
    if with_state(|s| s.resize_edges.is_some()).unwrap_or(false) {
        // 还在拖：拖完 `finish_resize` 会再投一次。
        return;
    }
    sync_shell_rect();
}

/// 内容区变了（拖边改大小、缩放、换页）：把新矩形告诉系统预览处理器。
///
/// 1.7.7：不接管输入时（Word/Excel…）子窗口**内缩一圈**——那一圈归我们，拖边改大小照旧，
/// 而内容区里的滚动条、工作表标签、选区都还是处理器自己的（能点、能拖）。
pub(crate) fn sync_shell_rect() {
    let Some(preview) = with_state(|s| s.preview) else {
        return;
    };
    let owns = with_state(|s| s.shell_owns_input).unwrap_or(false);
    let content = shell_content_rect(preview.raw(), owns);
    // **锁外**调 COM：`SetRect` 会把消息泵回我们自己的窗口过程，持锁调用必然自等（#139 那一族）。
    // 所以先把句柄从 `State` 里取出来，调用完再放回去；这段窗口里别的路径看到 `None`，
    // 只会跳过"跟处理器有关"的操作，不会崩。
    let Some(mut handle) = with_state(|s| s.shell_preview.take()).flatten() else {
        return;
    };
    log_debug!(SCOPE, "处理器重新对齐到 {content:?}");
    handle.set_rect(content);
    with_state(|s| {
        s.shell_preview = Some(handle);
    });
}

/// 系统预览处理器该占的那块矩形（1.7.7）。
///
/// 接管输入的（PPT）铺满内容区；其余内缩一圈，把窗口四条边留给"拖边改大小"。
pub(crate) fn shell_content_rect(preview: HWND, owns_input: bool) -> RECT {
    let content = window::chrome_rects(window::client_rect(preview)).content;
    if owns_input {
        return content;
    }
    let ring = dpi::scaled(preview, SHELL_RING_DIP);
    shell_inset(content, ring)
}

/// 内容区四边各收 `ring` 像素（下限 80×60，窗口很小时不至于把子窗口压没）。
pub(crate) fn shell_inset(content: RECT, ring: i32) -> RECT {
    let ring = ring.max(0);
    let left = content.left + ring;
    let top = content.top + ring;
    let right = (content.right - ring).max(left + 80);
    let bottom = (content.bottom - ring).max(top + 60);
    RECT {
        left,
        top,
        right,
        bottom,
    }
}

/// 滚轮转给系统预览处理器（它自己不监听我们的全局钩子）。
///
/// 返回 true 表示"已经转出去了"，调用方就不要再自己滚/缩放。
///
/// 1.7.7 起只有"接管输入"的处理器（目前只有 PPT）才需要转发：输入没被接管时
/// （Word/Excel）鼠标本来就在子窗口上，滚轮由它自己处理，我们再投一次就成了双倍滚动。
pub(crate) fn forward_wheel_to_shell(delta: i16) -> bool {
    let owns = with_state(|s| s.shell_owns_input).unwrap_or(false);
    if !owns {
        return with_state(|s| s.shell_preview.is_some()).unwrap_or(false);
    }
    with_state(|s| {
        s.shell_preview
            .as_ref()
            .map(|handle| handle.forward_wheel(delta))
    })
    .flatten()
    .unwrap_or(false)
}

thread_local! {
    /// 「正在把焦点交给预览处理器」的重入闸门（1.7.9）。见 `focus_shell_content`。
    static IN_FOCUS_TRANSFER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 把焦点交给系统预览处理器的内容子窗口（1.7.8）。
///
/// 用户实测：把鼠标点进预览窗之后，Excel 的滚轮立刻就能用——所以点窗口时必须把焦点
/// **直接**塞给处理器的内容层，而不是只把容器窗口激活。
///
/// 1.7.9 修自锁死：`SetFocus` 会把激活/焦点消息**泵回我们自己的消息循环**
/// （实测 Word 预览点一下就卡死，最后被 Windows 判为 Application Hang 杀掉）。
/// 所以句柄只在锁内取出，`SetFocus` 在锁放开之后调；再加一个重入闸门，防止
/// 「激活 → 转交焦点 → 又激活」来回套娃。
pub(crate) fn focus_shell_content() {
    if IN_FOCUS_TRANSFER.get() {
        return;
    }
    // 1.8.1：交给处理器**自己**设焦点（`IPreviewHandler::SetFocus`）——Excel 的预览是
    // OLE 就地激活对象，只把焦点塞给它的容器层（`EXCEL9`）时表格"看得见、点不动"，
    // 单元格选不中。COM 调用会把消息泵回我们，所以必须**锁外**调（CONVENTIONS #145）。
    let Some(mut handle) = with_state(|s| s.shell_preview.take()).flatten() else {
        return;
    };
    let target = handle.nav_target();
    IN_FOCUS_TRANSFER.set(true);
    let com_focused = handle.set_com_focus();
    let focused = com_focused || target.map(window::focus_child).unwrap_or(false);
    IN_FOCUS_TRANSFER.set(false);
    with_state(|s| s.shell_preview = Some(handle));
    if focused {
        log_debug!(SCOPE, "焦点已交给预览处理器（滚轮/滚动条可用）");
    }
}

/// 窗口内容区的比例和位图比例差得多不多（>2% 算"不一样"）。
///
/// 1.8.2：图片/视频不再拖边改大小，但最大化、`SetWindowPos`、系统 DPI 变化仍可能让窗口
/// 变成别的比例；那种时候铺满会把图拉变形，所以退回"等比居中"。
pub(crate) fn window_aspect_differs(hwnd: HWND, w: u32, h: u32) -> bool {
    if w == 0 || h == 0 {
        return false;
    }
    let content = window::chrome_rects(window::client_rect(hwnd)).content;
    let cw = (content.right - content.left).max(1) as f64;
    let ch = (content.bottom - content.top).max(1) as f64;
    let bitmap = w as f64 / h as f64;
    ((cw / ch) - bitmap).abs() / bitmap > 0.02
}
