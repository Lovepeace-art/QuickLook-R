//! 显示编排：show/hide、开窗尺寸与位置记忆、跟随选中项、阴影同步、状态行文本。
//!
//! 几何全部来自 `layout::Layout`，这里不再自己切内容区。

use super::*;

/// 跟随选中项 + 投机预渲染（1.0.10 / 1.1.0），共用一只 400 ms 计时器。
///
/// - 窗口**开着**：跟着资源管理器的选中项换内容（用户原话："预览文件随鼠标选中的文件而变化"）；
/// - 窗口**收着**：把当前选中的那个文件先渲染好（见 `maybe_prewarm`），用户按空格时直接上屏。
///
/// 两条路都先过 `foreground_is_shell()` 这道便宜的门：`IShellWindows` 那套调用要几毫秒，
/// 用户切到浏览器/游戏时继续问会白烧 CPU。
/// 后台轮询得到的"资源管理器当前选中的文件"。
///
/// 为什么放到线程里：`selection::current_selection()` 要枚举 `IShellWindows` 再 `FindWindowSW` 重试，
/// 在用户机器上实测有一次 **1538 ms**（日志 `SLOWSELECT`）。它跑在 UI 线程的 400 ms 计时器里，
/// 那就是"拖动时窗口定住、过一会儿才追上来"的真正原因。
pub(crate) static FOLLOW_SLOT: OnceLock<Mutex<Option<(PathBuf, Instant)>>> = OnceLock::new();
pub(crate) static FOLLOW_STARTED: OnceLock<()> = OnceLock::new();

pub(crate) fn follow_slot() -> &'static Mutex<Option<(PathBuf, Instant)>> {
    FOLLOW_SLOT.get_or_init(|| Mutex::new(None))
}

/// 跟随线程的轮询间隔（1.7.6）。
///
/// 以前是"先睡 300 ms 再查"，再加上 UI 侧 400 ms 的计时器取结果：用户点一下文件到画面换掉
/// 最坏要等 **700 ms**（用户反馈"延迟有点大"）。现在线程一发现变化就 `PostMessage`
/// （`WM_QLR_FOLLOW`），UI 立刻动手；轮询间隔只决定"发现得有多快"。
pub(crate) const FOLLOW_AWAKE_MS: u64 = 90;
/// 窗口收着时的轮询间隔：这时只喂投机预热，没必要跟手。
pub(crate) const FOLLOW_ASLEEP_MS: u64 = 250;

/// 起一个后台线程，每 [`FOLLOW_AWAKE_MS`] 摸一次选中项（COM 初始化在那个线程里做）。
///
/// 发现"UI 显示的不是这个文件"就先把路径放进 [`follow_slot`]，再 `PostMessage(WM_QLR_FOLLOW)`
/// 叫醒 UI —— 这样延迟只剩"一次轮询 + 一次查询"，不用再等 400 ms 计时器。
pub(crate) fn ensure_follow_worker() {
    FOLLOW_STARTED.get_or_init(|| {
        std::thread::Builder::new()
            .name("qlrust-follow".into())
            .spawn(|| {
                let _ = qlr_platform::selection::init_com();
                // 1.8.3：这个线程也会算窗口尺寸（投机预热），而 `dpi::px()` 读的是
                // **线程本地** DPI。不 seed 的话它按 96 算，与 UI 线程算出的 w/h 不一致，
                // 而 `pagecache` 的 key 含 w/h → 预热永远打不中，白烧 CPU 还看不出效果。
                dpi::seed_thread(dpi::system_dpi());
                // 已经通知过、但 UI 没切过去（比如"不支持"被跳过）：别每 90 ms 抖一次消息。
                let mut notified: Option<PathBuf> = None;
                loop {
                    let (visible, has_path) = with_state(|s| (s.visible, s.last_path.is_some()))
                        .unwrap_or((false, false));
                    let awake = visible && has_path;
                    let interval = if awake {
                        FOLLOW_AWAKE_MS
                    } else {
                        FOLLOW_ASLEEP_MS
                    };
                    let busy = with_state(|s| s.drag_from.is_some() || s.resize_edges.is_some())
                        .unwrap_or(false);
                    if !awake || busy || !qlr_platform::hotkey::foreground_is_shell() {
                        std::thread::sleep(std::time::Duration::from_millis(interval));
                        continue;
                    }
                    let started = Instant::now();
                    let picked = qlr_platform::selection::current_selection()
                        .first()
                        .map(|p| p.to_path_buf());
                    let cost = started.elapsed();
                    if cost.as_millis() >= 300 {
                        log_warn!(
                            SCOPE,
                            "SLOWSELECT {}ms（后台线程，不影响界面）",
                            cost.as_millis()
                        );
                    }
                    if let Some(next) = picked {
                        let shown = with_state(|s| s.last_path.clone()).flatten();
                        let fresh = shown.as_deref() != Some(next.as_path())
                            && notified.as_deref() != Some(next.as_path());
                        if fresh && let Ok(mut slot) = follow_slot().lock() {
                            *slot = Some((next.clone(), Instant::now()));
                            notified = Some(next);
                            drop(slot);
                            if let Some(host) = with_state(|s| s.host) {
                                log_debug!(
                                    SCOPE,
                                    "FOLLOW 发现新选中（查询 {}ms）",
                                    cost.as_millis()
                                );
                                window::post(host.raw(), ipc::WM_QLR_FOLLOW);
                            }
                        }
                    }
                    // 睡之前扣掉这次查询的开销：查询本身可能就要几十毫秒。
                    let wait = interval.saturating_sub(cost.as_millis() as u64).max(10);
                    std::thread::sleep(std::time::Duration::from_millis(wait));
                }
            })
            .ok();
    });
}

/// UI 线程收到"选中项变了"：计时器兜底与 `WM_QLR_FOLLOW` 都走这里。
pub(crate) fn on_follow_wake() {
    // 窗口收着就别跟了：线程可能在我们刚收起时就投了消息，照做会把窗口重新弹出来。
    if !with_state(|s| s.visible).unwrap_or(false) {
        return;
    }
    let picked = follow_slot().lock().ok().and_then(|mut slot| slot.take());
    let Some((path, at)) = picked else {
        return;
    };
    log_debug!(SCOPE, "FOLLOW 应用 +{}ms", at.elapsed().as_millis());
    follow_picked(path);
}

/// 拖动复证：整段拖动结束时打一行摘要（次数、最大间隔、最大单次耗时、期间有没有重绘）。
pub(crate) struct DragTrace {
    pub(crate) started: Option<Instant>,
    pub(crate) moves: u32,
    pub(crate) last_msg: Option<Instant>,
    pub(crate) gap_max_ms: u128,
    pub(crate) cost_max_ms: u128,
    pub(crate) paints: u32,
    pub(crate) moved: (i32, i32),
    pub(crate) origin: (i32, i32),
}

pub(crate) static DRAG_TRACE: OnceLock<Mutex<DragTrace>> = OnceLock::new();

pub(crate) fn drag_trace() -> &'static Mutex<DragTrace> {
    DRAG_TRACE.get_or_init(|| {
        Mutex::new(DragTrace {
            started: None,
            moves: 0,
            last_msg: None,
            gap_max_ms: 0,
            cost_max_ms: 0,
            paints: 0,
            moved: (0, 0),
            origin: (0, 0),
        })
    })
}

pub(crate) fn trace_drag_start() {
    if let Ok(mut trace) = drag_trace().lock() {
        *trace = DragTrace {
            started: Some(Instant::now()),
            moves: 0,
            last_msg: Some(Instant::now()),
            gap_max_ms: 0,
            cost_max_ms: 0,
            paints: 0,
            moved: (0, 0),
            origin: qlr_platform::window::cursor_pos().unwrap_or((0, 0)),
        };
    }
}

pub(crate) fn trace_drag_msg() {
    if let Ok(mut trace) = drag_trace().lock() {
        if let Some(last) = trace.last_msg {
            let gap = last.elapsed().as_millis();
            if gap > trace.gap_max_ms {
                trace.gap_max_ms = gap;
            }
        }
        trace.last_msg = Some(Instant::now());
        trace.moves += 1;
    }
}

pub(crate) fn trace_drag_moved(dx: i32, dy: i32) {
    if let Ok(mut trace) = drag_trace().lock() {
        trace.moved.0 += dx;
        trace.moved.1 += dy;
    }
}

pub(crate) fn trace_drag_cost(ms: u128) {
    if let Ok(mut trace) = drag_trace().lock() {
        if ms > trace.cost_max_ms {
            trace.cost_max_ms = ms;
        }
    }
}

pub(crate) fn trace_drag_paint() {
    if let Ok(mut trace) = drag_trace().lock() {
        trace.paints += 1;
    }
}

pub(crate) fn trace_drag_end() {
    let Ok(mut trace) = drag_trace().lock() else {
        return;
    };
    let Some(started) = trace.started.take() else {
        return;
    };
    log_info!(
        SCOPE,
        "DRAGTRACE 鼠标消息={} 次 总时长={}ms 消息最大间隔={}ms 单次最大耗时={}ms 拖动期间重绘={} 次 窗口位移={}x{} 鼠标位移={}x{}",
        trace.moves,
        started.elapsed().as_millis(),
        trace.gap_max_ms,
        trace.cost_max_ms,
        trace.paints,
        trace.moved.0,
        trace.moved.1,
        qlr_platform::window::cursor_pos()
            .map(|p| p.0 - trace.origin.0)
            .unwrap_or(0),
        qlr_platform::window::cursor_pos()
            .map(|p| p.1 - trace.origin.1)
            .unwrap_or(0)
    );
    trace.moves = 0;
    trace.gap_max_ms = 0;
    trace.cost_max_ms = 0;
    trace.paints = 0;
    trace.last_msg = None;
}

/// UI 线程卡顿探测器：跟随计时器每 400 ms 转一次，间隔明显变大就说明界面被别的东西堵住了。
pub(crate) static UI_TICK: OnceLock<Mutex<Instant>> = OnceLock::new();

pub(crate) fn ui_tick() -> &'static Mutex<Instant> {
    UI_TICK.get_or_init(|| Mutex::new(Instant::now()))
}

pub(crate) fn on_follow_timer() {
    if let Ok(mut last) = ui_tick().lock() {
        let gap = last.elapsed().as_millis();
        if gap > 700 {
            log_warn!(SCOPE, "UISTALL {}ms（UI 线程被堵住，计时器迟到了）", gap);
        }
        *last = Instant::now();
    }
    // 兜底收尸：处理器没被卸掉会让它的子窗口留在屏幕上。
    drain_shell_trash();
    // 正在拖动/改尺寸时不要碰 COM —— `current_selection()` 要枚举 IShellWindows，
    // 一次几十到几百毫秒，正好卡在拖动热路径上会表现为"窗口跟不上鼠标"。
    if with_state(|s| s.drag_from.is_some() || s.resize_edges.is_some()).unwrap_or(false) {
        return;
    }
    if !hotkey::foreground_is_shell() {
        return;
    }
    let (visible, has_path) =
        with_state(|s| (s.visible, s.last_path.is_some())).unwrap_or((false, false));
    if visible && has_path {
        ensure_follow_worker();
        // 主通道是线程的 `WM_QLR_FOLLOW`；这里只是兜底（线程恰好没投成/消息丢了）。
        on_follow_wake();
    } else {
        maybe_prewarm();
    }
}

/// 窗口开着：把预览切到资源管理器当前选中的文件。
pub(crate) fn follow_picked(next: PathBuf) {
    let same = with_state(|s| s.last_path.as_deref() == Some(next.as_path())).unwrap_or(true);
    if same {
        return;
    }
    // 不支持的文件直接忽略（不切换、也不响铃）：鼠标扫过一堆无关文件时，
    // 每越过一个就"叮"一声会把用户逼疯；响铃只在**用户主动按空格**时给。
    let kind = format::kind_for_path(&next);
    let supported =
        qlr_render::viewer_for(kind).is_some() || plugins::host().find_for_path(&next).is_some();
    if !supported {
        log_debug!(SCOPE, "跟随：跳过不支持的 {}（{kind}）", next.display());
        return;
    }
    log_info!(SCOPE, "跟随选中项 → {}", next.display());
    // 计时器路径不经过 `apply_request`，延迟起点要自己重置（否则日志里 elapsed 是上次的）。
    begin_request();
    show_preview(Some(next));
}

/// 是否按"图片缩放比例"开窗。
///
/// 1.7.9 起 SVG / 视频也归到这里：它们和位图一样有"原始像素尺寸"，用户设的初始缩放
/// （默认 50%）对它们同样合理；PDF 不跟（PDF 有自己的"适应窗口"口径）。
pub(crate) fn uses_image_zoom(kind: format::Kind) -> bool {
    matches!(
        kind,
        format::Kind::Image | format::Kind::Svg | format::Kind::Video
    )
}

/// 这个文件打开时窗口该多大（宽, 高，**含顶栏**）。
///
/// `show_preview` 与预渲染（1.1.0）共用这一份计算：两边一漂移，预热就永远打不中
/// （白烧 CPU 还看不出效果），所以尺寸规则只写在这里。
pub(crate) fn window_size_for(
    work: RECT,
    kind: format::Kind,
    natural: Option<(u32, u32)>,
    config: &Config,
    remembered: Option<(i32, i32)>,
) -> (i32, i32) {
    // 顶栏（1.0.9）四种格式都有：只有"按内容开窗"的格式才需要额外留一条。
    // 视频再加一条进度条的高度（1.7.9）：不留的话画面会被进度条挤扁一圈。
    let reserve = if natural.is_some() {
        window::chrome_height()
    } else {
        0
    } + if kind == format::Kind::Video {
        window::audio_bar_h()
    } else {
        0
    };
    let base = match natural {
        Some((w, h)) => {
            // 图片的初始缩放走配置（默认 50%，用户可在设置里改）；PDF 保持"适应窗口"。
            let factor = if uses_image_zoom(kind) {
                config.image_zoom_percent as f32 / 100.0
            } else {
                1.0
            };
            let mut b = window::natural_rect_scaled(work, w as i32, h as i32, factor);
            b.bottom = (b.bottom + reserve).min(work.bottom);
            b
        }
        // 文本/Markdown 没有"原始尺寸"（1.0.5）：固定阅读宽度，高度按比例。
        None => window::text_rect(work, config.height_ratio),
    };
    let (mut width, mut height) = (
        (base.right - base.left).max(1),
        (base.bottom - base.top).max(1),
    );
    // 尺寸记忆（1.0.14）：用户自己改过这个格式的窗口大小就照它开。
    if let Some((w, h)) = remembered {
        let (ww, wh) = (work.right - work.left, work.bottom - work.top);
        width = w.clamp(min_window_w(), ww);
        height = h.clamp(min_window_h(), wh);
    }
    // 图片/视频（1.8.1）：窗口比例 = 素材比例，记忆下来的尺寸也要夹回来——
    // 否则上次拖成方形，下次开图又变成"内容在方窗口里留白/变形"。
    if matches!(kind, format::Kind::Image | format::Kind::Video) {
        if let Some((w, h)) = natural {
            if w > 0 && h > 0 {
                let aspect = w as f64 / h as f64;
                (width, height) = media_window_size(work, aspect, width, media_bar(kind));
            }
        }
    }
    (width, height)
}

/// 视频画面下面还有一条进度条（`window::audio_bar_h()`），图片没有。
pub(crate) fn media_bar(kind: format::Kind) -> i32 {
    if kind == format::Kind::Video {
        window::audio_bar_h()
    } else {
        0
    }
}

/// 让内容铺满且不变形的窗口尺寸（1.8.1 图片/视频）。
///
/// `content_w` 是"想开多宽"，高度由比例反算；超出工作区就先压高度再反算宽度，
/// 绝不把内容算出 0。返回的是**整窗**尺寸（内容 + 顶栏 + 进度条）。
pub(crate) fn media_window_size(work: RECT, aspect: f64, content_w: i32, bar: i32) -> (i32, i32) {
    let extra = window::chrome_height() + bar;
    let (ww, wh) = (work.right - work.left, work.bottom - work.top);
    let mut cw = content_w.max(min_window_w());
    let mut ch = (cw as f64 / aspect).round().max(1.0) as i32;
    if ch + extra > wh {
        ch = (wh - extra).max(min_window_h());
        cw = (ch as f64 * aspect).round().max(1.0) as i32;
    }
    if cw > ww {
        cw = ww;
        ch = (cw as f64 / aspect).round().max(1.0) as i32;
    }
    (cw, ch + extra)
}

/// 这个格式记住的窗口尺寸：优先本次会话的（`last_sizes`），其次上次落盘的（1.0.14）。
pub(crate) fn remembered_size(kind_key: &str) -> Option<(i32, i32)> {
    with_state(|s| {
        s.last_sizes
            .get(kind_key)
            .copied()
            .or_else(|| s.config.window_size_for(kind_key))
    })
    .flatten()
}

pub(crate) fn show_preview(path: Option<PathBuf>) {
    let Some((config, preview_hwnd, last)) =
        with_state(|s| (s.config.clone(), s.preview, s.last_path.clone()))
    else {
        return;
    };
    let target = path.or(last);
    // 与 hide_preview 的 GDI/USER 计数配对：两个数字一起看，才知道增长发生在 show 还是 hide。
    let (gdi0, user0) = window::gui_resources();
    log_debug!(SCOPE, "SHOW 起点 GDI={gdi0} USER={user0}");

    // 现场报障："md 有时候第一次点 × 没反应，第二次才关"。如果这次 SHOW 紧跟在一次
    // HIDE 之后，用户看到的就是"点了没反应"——不管是谁（渲染回调、跟随计时器、
    // 热键）把窗口叫回来的，先把这条痕留下，下次复现直接看日志定位。
    if let Some(at) = *last_hide_slot() {
        let gap = at.elapsed().as_millis();
        if gap < 1500 {
            log_info!(SCOPE, "HIDE→SHOW 间隔 {gap}ms（会表现为「点了 × 没反应」）");
        }
    }

    // 换文件就把页缓存清掉（1.3.0）：缓存按路径记，旧文档的几十页继续占内存没意义。
    // 同一个文件翻页不清——那正是缓存要服务的场景。
    if let Some(new) = target.as_deref() {
        let same = with_state(|s| s.last_path.as_deref() == Some(new)).unwrap_or(false);
        if !same {
            pagecache::clear();
        }
    }

    let work = window::work_area(preview_hwnd.raw());
    let kind = target
        .as_deref()
        .map(format::kind_for_path)
        .unwrap_or(format::Kind::Unsupported);
    // 按**原始尺寸**开窗（1.0.1）：图片读文件头像素尺寸、PDF 取首页 point 尺寸，
    // 探测不到（文本/插件/坏文件）才退回配置里的比例。
    let natural = target
        .as_deref()
        .and_then(|p| qlr_render::natural_size(p, kind));
    // 图片/视频（1.8.1）：窗口比例锁成素材比例——之后改大小只做等比缩放、内容始终铺满。
    let aspect = match (kind, natural) {
        (format::Kind::Image | format::Kind::Video, Some((w, h))) if w > 0 && h > 0 => {
            Some(w as f64 / h as f64)
        }
        _ => None,
    };
    with_state(|s| s.media_aspect = aspect);
    // PDF 先问总页数（同一把锁里再开文档，代价只有一次 read；页数拿不到不影响渲染）。
    let pages = if kind == format::Kind::Pdf {
        target.as_deref().and_then(qlr_render::pdf_page_count)
    } else {
        None
    };
    // 尺寸规则（1.0.1 / 1.0.9 / 1.0.14 / 1.1.0）：集中到 `window_size_for`，
    // 预渲染用同一份算，两边才不会漂移。
    let kind_key = kind.name();
    let (content_w, content_h) =
        window_size_for(work, kind, natural, &config, remembered_size(kind_key));
    // 顶栏（1.0.9）：只有按内容开窗的格式才把顶栏算在内容之外。
    // 这条规则与 `window_size_for` 里那个 `reserve` 是同一件事，1.8.3 起由
    // `layout::reserve_for_window` 一处给出。
    let reserve = layout::reserve_for_window(natural);
    // 居中的"底稿"只用来定 left/top，宽高一律以 `window_size_for` 为准。
    let mut rect = match natural {
        Some((w, h)) => {
            let factor = if uses_image_zoom(kind) {
                config.image_zoom_percent as f32 / 100.0
            } else {
                1.0
            };
            window::natural_rect_scaled(work, w as i32, h as i32, factor)
        }
        // 文本/Markdown 没有"原始尺寸"（1.0.5）：固定阅读宽度，高度按比例。
        None => window::text_rect(work, config.height_ratio),
    };
    rect.right = rect.left + content_w;
    rect.bottom = rect.top + content_h;
    // 位置记忆（1.0.13）：
    // - 窗口已经开着（跟随选中项换文件、或连按两次空格换内容）→ 就用它现在的位置，
    //   换文件不该把窗口搬回屏幕中央；
    // - 刚打开 → 用上次记住的位置，没记过才保持居中。
    let wanted = if with_state(|s| s.visible).unwrap_or(false) {
        current_window_pos().or(config.window_pos)
    } else {
        config.window_pos
    };
    place_at(&mut rect, wanted, work);
    let (width, height) = (
        (rect.right - rect.left).max(1) as u32,
        (rect.bottom - rect.top).max(1) as u32,
    );
    if let Some((nw, nh)) = natural {
        log_info!(SCOPE, "开窗尺寸 {}x{}（原始 {}x{}）", width, height, nw, nh);
    }

    // 1) 先定内容与占位：旧帧立刻作废，避免"上一张图"残留在新窗口上。
    // 内置查看器优先，内置没有才问插件（与渲染线程的分发规则一致）。
    let plugin = target
        .as_deref()
        .filter(|_| qlr_render::viewer_for(kind).is_none())
        .and_then(|p| plugins::host().find_for_path(p));
    // 压缩包（1.7.3）：内置列表视图是壳层自己画的，既没有 `Viewer` 也可能没有插件，
    // 所以单独放行一次——否则 `.zip` 会掉进下面"不支持"分支被响铃拒绝。
    let archive = target.as_deref().is_some_and(is_archive);
    let renderable =
        target.is_some() && (qlr_render::viewer_for(kind).is_some() || plugin.is_some() || archive);

    // 不支持的文件（1.0.10）：**一个像素都不画**，只"响一声"然后收起窗口。
    //
    // 用户原话："预览不支持的文件时，不要有任何预览，系统直接响铃报错即可"。
    // 以前会弹一个占位文字窗口（写着插件目录、怎么退出），扫文件时非常吵；
    // 而且那个窗口一出现，用户按空格的第一反应是"我明明按了怎么没反应"。
    if !renderable {
        log_info!(
            SCOPE,
            "SHOW 拒绝 kind={} target={}",
            kind,
            target
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "-".to_string())
        );
        window::beep();
        if with_state(|s| s.visible).unwrap_or(false) {
            hide_preview();
        }
        return;
    }

    // 新文件一律回到 100%：上一个文件留下的缩放/平移套到这张图上会"开着就是裁切过的"
    // （1.0.1 修：之前 show_preview 不重置 zoom）。
    // PDF 有顶部页码条（1.0.8）：窗口高 = 内容高 + HEADER_HEIGHT，所以缩放基准确记"内容区"。
    with_state(|s| {
        s.zoom = 1.0;
        s.pan = (0, 0);
        s.zoom_preview = false;
        // 带上一个文件残留的顶栏状态会串味：悬停按钮、页码输入、滚动条拖拽都清掉。
        s.hover_button = None;
        s.page_edit = None;
        s.scroll_drag = false;
        // "阻止关闭"只对当前这一次预览有效（换文件就重新问一次）。
        s.pinned = false;
        s.resize_edges = None;
        s.resize_from = None;
        // 记住自然尺寸（日志用）与开窗尺寸（zoom 的基准，1.0.2 窗口跟随缩放）。
        s.natural = natural;
        s.base = natural.map(|_| (width, (height as i32 - reserve).max(1) as u32));
    });
    hotkey::set_capture_digits(false);
    let zoom = 1.0_f32;
    with_state(|s| {
        s.page = 0;
        s.pages = pages;
    });

    clear_archive_view();
    if let Some(p) = target.as_deref()
        && is_archive(p)
    {
        if !open_archive_view(p) {
            // 解析失败（不是真 zip / 截断 / 加密 / 空）：按"不支持"处理——响一声、
            // 不画任何东西。别往下面掉：那条路会给压缩包建"正在渲染…"的占位，
            // 再等一次注定失败的渲染，用户看到的是"卡住了一下然后报错"。
            log_warn!(SCOPE, "压缩包解析失败，按不支持处理：{}", p.display());
            window::beep();
            if with_state(|s| s.visible).unwrap_or(false) {
                hide_preview();
            }
            return;
        }
        set_content(None);
        set_lines(Vec::new());
        // 压缩包走内置列表视图，不需要系统预览处理器：上一个文件的处理器必须当场卸掉，
        // 否则它的子窗口会一直盖在列表上面（1.7.5，见 `drop_stale_shell_preview`）。
        drop_stale_shell_preview();
        window::show_at(preview_hwnd.raw(), rect, config.topmost);
        sync_shadow();
        hotkey::set_active(true);
        // 托盘没加上时，第一次"窗口真的可见"就用 toast 说一次（1.8.3）。
        notify_tray_failure_once(preview_hwnd.raw());
        with_state(|s| {
            s.visible = true;
            s.last_path = Some(p.to_path_buf());
            s.last_show_at = Some(Instant::now());
        });
        window::invalidate(preview_hwnd.raw());
        log_info!(SCOPE, "SHOW archive {}", p.display());
        return;
    }
    set_content(None);
    let mut lines = build_lines(target.as_deref());
    // 交给系统预览处理器（Office）时**连文件信息页也一起删掉**（1.8.1 用户要求
    // "把占位页面直接删掉"）：窗口先空着，处理器第一帧到了直接铺满，中间不闪一页文字。
    // 只有走我们自己渲染的格式才留"正在渲染…"那一行。
    let shell = shell_candidate(target.as_deref());
    if shell {
        lines.clear();
    } else if renderable {
        lines.insert(2, "正在渲染…".to_string());
    }
    set_lines(lines);
    // 2) 放锁之后再投任务/动窗口（见 LINES 的注释）。
    //
    // 先看预渲染槽（1.1.0）：命中的话直接上屏，省掉一次 16–60 ms 的渲染。
    // 渲染尺寸要和翻页时算的一致（PDF 扣掉顶栏）：`view_size` 就是这么算的，
    // 两边不一致的话页缓存 key 永远对不上，缓存等于白做（1.3.0 踩过）。
    // 渲染盒要扣掉滚动条：`view_size` 量的就是这个宽度（1.8.3，见 `render_box`）。
    let (render_w, render_h) = render_box(kind, pages, width as i32, height as i32, reserve, zoom);
    // 1.4.0：重开同一个文件（没改过）直接上屏上一帧，连渲染都省掉。
    // 先查这个再看预渲染槽：上一帧是"刚刚真的显示过"的，比预热结果更新。
    let wanted = target
        .as_deref()
        .map(|p| frame_key(p, 0, render_w, render_h, zoom > 1.0));
    let last = wanted
        .as_ref()
        .and_then(take_last_frame)
        .filter(|canvas| frame_matches_size(canvas, render_w, render_h));
    let prepared = if last.is_some() {
        None
    } else {
        // 预热结果是按"当时算出来的尺寸"渲的，窗口/页数一变就对不上了。
        // 尺寸不匹配还拿去上屏，就是一次满屏拉伸（发虚 + 每帧重采样），
        // 还不如就地下场重渲（下一页缓存/预渲染还会命中）。
        //
        // 注意这里用 `fits_render_box` 而不是 `frame_matches_size`：渲染器**从不**按盒子
        // 逐像素出图，它把内容"适配"进盒子（PDF 按页面比例留白、位图按整像素取整），
        // 所以 `bitmap 619x876` 对 `请求 619x894` 是正常结果。用精确相等判定，预热
        // 结果会被当成"尺寸不合"全部丢掉 —— 这就是"预热永远打不中"的第二层原因
        // （第一层是盒子本身差一条 `chrome_height`，见 `render_box`）。
        take_prepared(target.as_deref(), render_w, render_h).and_then(|canvas| {
            if fits_render_box(&canvas, render_w, render_h) {
                Some(canvas)
            } else {
                log_debug!(
                    SCOPE,
                    "PREWARM 帧装不进渲染盒（{}x{} vs 需要 {}x{}），丢弃重渲",
                    canvas_size(&canvas).0,
                    canvas_size(&canvas).1,
                    render_w,
                    render_h
                );
                None
            }
        })
    };
    // 1.4.1：系统装了 Office 预览处理器就优先交给它（真排版）。
    // 这里只做"判断"：真正 DoPreview 必须等窗口 show 出来之后——在隐藏窗口上 DoPreview
    // 会让处理器等一个永远不会到来的激活，UI 线程直接卡死（踩过，表现为"未响应"）。
    let shell_wanted = shell_candidate(target.as_deref());
    // 换到"不需要系统预览处理器"的文件（图片/文本…）时同样要卸掉上一个文件的处理器，
    // 否则它的子窗口会盖住新内容（1.7.5，见 `drop_stale_shell_preview`）。
    if !shell_wanted {
        drop_stale_shell_preview();
    }
    // 命中缓存/预渲染时内容已经是"不透明"的，后面就不能再开亚克力（否则整个窗口变半透明灰）。
    // 1.8.3 两段式首帧的**第一段**：>16 MP 的图片、且没有缓存/预热可上屏时，
    // 先给一张"形状对、但模糊"的占位快照（下一帧全清时原地替换）。
    // 只在这里（UI 线程）做：COM 早就初始化好了，没有缓存时 18 µs 就返回，
    // 有缓存时几毫秒——不会像"渲染线程临时起 COM"那样给进程多挂两条线程。
    // 1.8.3 两段式首帧的**第一段**（>16 MP 的图片、且没有缓存/预热可上屏时）：
    // 先给一张"形状对、但模糊"的占位快照，等全清帧到了原地替换。
    //
    // **默认关闭**，只认 `QLRUST_QUICK_FRAME=1`。为什么默认不开（本机实测，别凭直觉改）：
    // - 本机能拿到的第一段只有"Shell 已有缩略图缓存"这一种（`INCACHEONLY`，没有缓存时
    //   18 µs 失败返回）；而 Shell **现解** 48MP PNG 要 108–171 ms，和我们自己的全量解码
    //   （195 ms）同一量级，插一段只会把清晰帧往后推到 360 ms；
    // - 让 Shell 参与进来本身有代价：第一次探测会把 shell32/thumbcache 拉起来，
    //   进程多 2 条线程、+25 个句柄（一次性，不是泄漏）——本任务的验收里"线程回到 7"
    //   比"冷文件首帧 30 ms"更硬；
    // - 中文注释一句话：没有缩略图缓存时，这条路上限就是"什么都不发生"，
    //   开了也只是多花一次探测——所以默认关，留给"系统确实缓存过缩略图"的机器。
    let quick_enabled = *QUICK_FRAME_ENABLED
        .get_or_init(|| std::env::var("QLRUST_QUICK_FRAME").ok().as_deref() == Some("1"));
    if quick_enabled && !shell_wanted && renderable && last.is_none() && prepared.is_none() {
        quick_first_frame(kind, natural, target.as_deref(), render_w.max(render_h));
    }
    let mut presented = false;
    if !shell_wanted
        && renderable
        && let Some(p) = target.as_deref()
    {
        match (last, prepared) {
            (Some(canvas), _) => {
                log_info!(SCOPE, "LASTFRAME 命中 {}（跳过渲染）", p.display());
                present_canvas(canvas);
                presented = true;
            }
            (None, Some(canvas)) => {
                log_info!(SCOPE, "PREWARM 命中 {}（跳过渲染）", p.display());
                present_canvas(canvas);
                presented = true;
            }
            (None, None) => request_render(p, render_w, render_h, 0),
        }
    }

    // 占位文字阶段才用亚克力；已经有真内容（缓存命中）就必须按"不透明"处理，
    // 否则第二次预览会整窗发灰 —— 顶栏也会跟着变灰（用户 1.4.10 反馈）。
    let _ = presented;

    // 1.7.4 修（用户反馈"顶栏一直是错的文件名"）：**先把文件名记进状态，再显示窗口**。
    //
    // 为什么顺序这么重要：下面 `open_shell_preview` 是跨进程 COM 调用，STA 上会泵消息，
    // 期间到达的 WM_PAINT 会真的跑一遍 `repaint()`；那时若 `last_path` 还是上一个文件，
    // 顶栏就按旧名字画了一帧，而且这之后没有任何 `invalidate` 去纠正它——旧标题就留在
    // 屏幕上了（直到用户滚动/缩放/碰顶栏才被刷掉）。
    // 换文件的"冻帧"（1.7.6）：窗口已经开着、并且这次要交给系统预览处理器时，
    // 内容区先停在上一帧——`open_shell_preview` 里的 `DoPreview` 要几十到几百毫秒，
    // 这期间旧处理器已经卸掉、新的一帧还没来，整窗重画就会露出占位文字页（割裂感）。
    let hold = shell_wanted && with_state(|s| s.visible && s.last_path.is_some()).unwrap_or(false);
    with_state(|s| s.hold_frame = hold);
    with_state(|s| s.last_path = target.clone());
    // 留一条可断言的日志：验证脚本靠"这一行出现在 `SHELL 预览已接管` 之前"证明上面那个顺序。
    if let Some(p) = target.as_deref() {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.display().to_string());
        log_debug!(SCOPE, "标题栏 ← {name}");
    }
    window::show_at(preview_hwnd.raw(), rect, config.topmost);
    // 窗口已经可见，这时才能把内容交给系统预览处理器（顺序反了会卡死，见上面注释）。
    // 处理器失败（没装 Office、注册表指向的 DLL 缺失等）要立刻退回插件渲染，
    // 否则屏幕上只留着"正在渲染…"的占位文字，用户会以为卡住了。
    if shell_wanted && !open_shell_preview(target.as_deref()) && renderable {
        // 处理器没起来：立刻退回插件渲染（否则屏幕上只剩"正在渲染…"的占位文字）。
        if let Some(p) = target.as_deref() {
            request_render(p, render_w, render_h, 0);
        }
    }
    // 处理器没起来（或压根没要处理器）就别冻帧了，让占位/渲染帧正常上屏。
    if with_state(|s| s.hold_frame).unwrap_or(false)
        && with_state(|s| s.shell_preview.is_none()).unwrap_or(true)
    {
        with_state(|s| s.hold_frame = false);
        window::invalidate(preview_hwnd.raw());
    }
    sync_shell_rect();
    // 位置记忆（1.0.13）：窗口刚摆好，把落点记下来（不管来自记忆还是居中）。
    if let Some(pos) = current_window_pos() {
        with_state(|s| s.last_pos = Some(pos));
    }
    // 柔和投影（1.0.10）：显示后立刻同步一次色调层（位置 + alpha 图）。
    sync_shadow();
    // 开着的时候才需要旁听翻页键。
    // 跟随计时器从启动就跑（1.1.0）：窗口收着的时候它负责预渲染，所以这里不再重开。
    hotkey::set_active(true);
    // 托盘没加上时，第一次"窗口真的可见"就用 toast 说一次（1.8.3）。
    notify_tray_failure_once(preview_hwnd.raw());

    // 声音与画面（1.7.9）：窗口已经可见，音频自动开始播、视频自动开始放；
    // 其它格式一律停掉（从 wav 切到图片时不能继续在后台响）。
    match kind {
        format::Kind::Audio => {
            if let Some(p) = target.as_deref() {
                start_audio(p);
            }
            stop_video();
        }
        format::Kind::Video => {
            if let Some(p) = target.as_deref() {
                start_video(p);
            }
            stop_audio();
        }
        _ => {
            stop_audio();
            stop_video();
        }
    }

    let shown_at = with_state(|s| {
        s.visible = true;
        // `last_path` 在显示窗口**之前**就写好了（1.7.4，见上面那段注释）。
        // 自适应预渲染靠"最近按过空格没有"决定要不要提前画（1.1.0）。
        s.last_show_at = Some(Instant::now());
        s.request_started.elapsed().as_millis()
    })
    .unwrap_or_default();
    log_info!(
        SCOPE,
        "SHOW target={} kind={} renderable={} plugin={} size={width}x{height} elapsed={shown_at}ms",
        with_state(|s| s.last_path.as_ref().map(|p| p.display().to_string()))
            .flatten()
            .unwrap_or_else(|| "(无)".to_string()),
        kind,
        renderable,
        plugin.map(|p| p.name()).unwrap_or("-")
    );
}

/// 预览窗当前的左上角屏幕坐标（窗口无效时 `None`）。
pub(crate) fn current_window_pos() -> Option<(i32, i32)> {
    let preview = with_state(|s| s.preview)?;
    let rc = window::window_rect(preview.raw());
    if rc.right <= rc.left && rc.bottom <= rc.top {
        return None;
    }
    Some((rc.left, rc.top))
}

/// 把"预览窗现在在哪"记进内存（1.0.13 位置记忆）。
///
/// 调用的地方 = 所有会改变窗口位置的路径：显示、拖动、拖边改大小、缩放（缩放会按中心改尺寸）。
/// 只更新内存，不落盘；落盘在 `persist_position()`（收起窗口 / 退出时才写，避免拖一次写一次盘）。
pub(crate) fn remember_position() {
    if !with_state(|s| s.visible).unwrap_or(false) {
        return;
    }
    if let Some(pos) = current_window_pos() {
        with_state(|s| s.last_pos = Some(pos));
    }
}

/// 把"预览窗现在多大"记进内存（1.0.14 尺寸记忆），按当前内容的格式分别记。
///
/// 只在**用户自己改过尺寸**的路径上调用（拖边改大小、缩放），不在显示时调用：
/// 显示时按内容算出来的尺寸如果也记，一张 8000×6000 的大图就会把之后所有图片的
/// 初始尺寸都顶到工作区上限，"按原图 50% 开窗"的设定就形同虚设。
pub(crate) fn remember_size() {
    if !with_state(|s| s.visible).unwrap_or(false) {
        return;
    }
    let Some(kind) = with_state(|s| s.last_path.clone())
        .flatten()
        .map(|p| format::kind_for_path(&p))
    else {
        return;
    };
    let Some((w, h)) = current_window_size() else {
        return;
    };
    // 太小的不记（拖到最小尺寸时的一瞬间），否则下次开窗只有一条缝。
    if w < min_window_w() || h < min_window_h() {
        return;
    }
    with_state(|s| {
        s.last_sizes.insert(kind.name().to_string(), (w, h));
    });
    log_debug!(SCOPE, "记住窗口尺寸 {} {}x{}", kind.name(), w, h);
}

/// 预览窗当前的客户区尺寸（窗口无效时 `None`）。
pub(crate) fn current_window_size() -> Option<(i32, i32)> {
    let preview = with_state(|s| s.preview)?;
    let rc = window::window_rect(preview.raw());
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    if w <= 0 || h <= 0 {
        return None;
    }
    Some((w, h))
}

/// 把记住的位置与尺寸写进 `config.json`（1.0.13 / 1.0.14）。
///
/// 先 `remember_position()` 再写，所以调用方不必自己先捕获。写失败只记警告——
/// 记忆丢了不影响这次预览，没必要打断用户。
pub(crate) fn persist_position() {
    remember_position();
    let Some((mut config, pos, sizes)) = with_state(|s| {
        (
            s.config.clone(),
            s.last_pos.or(s.config.window_pos),
            s.last_sizes.clone(),
        )
    }) else {
        return;
    };
    let mut changed = false;
    if let Some(pos) = pos
        && config.window_pos != Some(pos)
    {
        config.window_pos = Some(pos);
        changed = true;
    }
    for (kind, size) in sizes {
        if config.window_size_for(&kind) != Some(size) {
            let (w, h) = size;
            if w >= 200 && h >= 120 {
                config.set_window_size(&kind, size);
                changed = true;
            }
        }
    }
    if !changed {
        return; // 没变就别写盘
    }
    let path = paths::config_path();
    match config.save(&path) {
        Ok(()) => {
            with_state(|s| {
                s.config.window_pos = config.window_pos;
                s.config.window_sizes = config.window_sizes.clone();
            });
            log_debug!(
                SCOPE,
                "记住窗口几何 pos={:?} sizes={}",
                config.window_pos,
                config.window_sizes.len()
            );
        }
        Err(e) => log_warn!(SCOPE, "窗口几何保存失败：{e}"),
    }
}

/// 把矩形（只取尺寸）摆到记住的位置上，夹回工作区（1.0.13）。
///
/// `wanted` 为 `None` 表示没有记忆，保持调用方算好的居中位置。
pub(crate) fn place_at(rect: &mut RECT, wanted: Option<(i32, i32)>, work: RECT) {
    let Some((x, y)) = wanted else {
        return;
    };
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    let mut rc = RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    };
    // 换了显示器/分辨率时，记下的坐标可能在工作区外——夹回来，别让窗口消失。
    window::clamp_into(&mut rc, work);
    *rect = rc;
}

/// 把投影浮层对齐到预览窗（拖动/改大小/显示后都要调；内部会缓存 alpha 图）。
/// 上一次真正同步投影的时刻（拖动热路径上做节流用）。
pub(crate) static LAST_SHADOW_SYNC: OnceLock<Mutex<Instant>> = OnceLock::new();

pub(crate) fn last_shadow_sync() -> &'static Mutex<Instant> {
    LAST_SHADOW_SYNC.get_or_init(|| Mutex::new(Instant::now() - std::time::Duration::from_secs(1)))
}

/// 把投影窗口对齐到预览窗口。
///
/// `throttle` 为真时最多每 8 ms 做一次：投影是**另一个分层窗口**，`SetWindowPos` 一次要走 DWM，
/// 拖动时每帧都同步会让主窗口"追不上鼠标"（用户 1.4.1 反馈）。拖动结束会强制同步一次。
pub(crate) fn sync_shadow_with(throttle: bool) {
    // 拖动期间投影必须一直藏着：`sync_shadow` 会把投影窗口 `ShowWindow` 出来，
    // 只在 on_drag_start 里藏一次会被下一次移动又亮起来（1.4.8 踩过）。
    if with_state(|s| s.drag_from.is_some()).unwrap_or(false) {
        if let Some(shadow) = with_state(|s| s.shadow).flatten() {
            window::suppress_shadow();
            window::hide_shadow(shadow.raw());
        }
        return;
    }
    if throttle {
        let Ok(mut last) = last_shadow_sync().lock() else {
            return;
        };
        if last.elapsed() < std::time::Duration::from_millis(8) {
            return;
        }
        *last = Instant::now();
    } else if let Ok(mut last) = last_shadow_sync().lock() {
        *last = Instant::now();
    }
    let Some((shadow, preview)) = with_state(|s| s.shadow.map(|h| (h, s.preview))).flatten() else {
        return;
    };
    window::sync_shadow(shadow.raw(), preview.raw());
}

pub(crate) fn sync_shadow() {
    sync_shadow_with(false);
}

pub(crate) fn hide_preview() {
    // 压缩包视图只在"当前这次预览"里有效（1.7.3）：收起就放掉，几千条目的树不该常驻。
    clear_archive_view();
    // 音频（1.7.9）：收起必须停声。`AudioPlayer` 的 drop 会 `MCI_CLOSE`，
    // 不关的话 MCI 设备一直被占着——再次预览同一个文件会拿到同一个 alias，
    // 表现成"第二次预览没声音"。
    stop_audio();
    // 视频同理：放掉 MF 的 SourceReader，否则下一次预览同一文件会多占一个解码器。
    stop_video();
    // 系统预览处理器要先卸掉，否则它会把自己的子窗口留在屏幕上（1.4.1）。
    // 顺序很关键：先取走句柄、**等窗口隐藏之后**再真正 Unload（见 SHELL_TRASH 注释）。
    let old_shell = with_state(|s| s.shell_preview.take()).flatten();
    // 换帧期间"盖住"的旧处理器也一起放掉（1.7.6）：收回时不卸，下次显示会带着僵尸子窗口。
    let old_cover = with_state(|s| s.shell_cover.take()).flatten();
    // 位置记忆（1.0.13）：收起之前先记一次——窗口一旦隐藏，`GetWindowRect` 还可能有效，
    // 但"visible"已经是 false，`remember_position` 会直接返回，所以必须抢在前面。
    remember_position();
    // 锁内**只改状态、只取句柄**，`window::hide` 放到锁外。
    //
    // 为什么必须这样（结构评审 #1，与历史 #128 死锁同型）：`window::hide` 内部是
    // `ShowWindow(SW_HIDE)`，它会**同步**把 WM_WINDOWPOSCHANGED / WM_PAINT 派发回
    // 我们自己的 wndproc，而 `repaint` 那条路要再拿一次 STATE 锁。此时本线程正持着
    // 锁 → 重入等待，整窗假死（空格关不掉、托盘退出卡住）。
    let Some((preview, shadow, elapsed)) = with_state(|s| {
        s.visible = false;
        // 收起之后不再跟着选中项跑（否则看不见的窗口还在后台不停重光栅化）。
        s.pinned = false;
        // 1.7.4：**拖边/拖动的中间状态必须一起清掉**。
        // 以前这里不清，若恰好在拖动/改大小的过程中被收起（空格、`--hide`、托盘），
        // capture 与 `resize_from` 会留在状态里：下一次显示后第一条鼠标移动
        // 会拿"上一次的原始矩形 + 这一次的位移"把窗口瞬间跳到旧尺寸（用户看到的
        // 是"窗口自己跳回去了"）。同时 PPT 的翻页计数与提示也要跟本次预览一起失效。
        s.resize_edges = None;
        s.resize_from = None;
        s.media_aspect = None;
        s.drag_from = None;
        s.dragged = false;
        s.slide = None;
        s.toast = None;
        s.shell_input_pending = false;
        s.hold_frame = false;
        // 1.8.3（T03）：页码输入状态必须跟预览一起收起。
        // 以前这里不清：点开页码框输到一半按空格/`--hide` 收起窗口，`page_edit` 留在
        // 状态里、`CAPTURE_DIGITS` 还开着 → 全局钩子继续吞数字/退格/回车，用户在**别的
        // 程序**里打不出数字（这个"谁吞了我的按键"极难自查）。`finish_page_edit(false)`
        // 会同时把 `page_edit` 清掉并 `set_capture_digits(false)`。
        s.page_edit = None;
        (s.preview, s.shadow, s.request_started.elapsed().as_millis())
    }) else {
        return;
    };
    // 锁已经放掉了，这时候才真正隐藏窗口（见上面那段注释）。
    window::hide(preview.raw());
    if let Some(shadow) = shadow {
        window::hide_shadow(shadow.raw());
    }
    // 数字输入模式的开关是**全局**的，所以放在锁外关（与 capture 一起收拾临时状态）。
    // 记一行日志：smoke 脚本可以按稳定字符串断言"收起之后确实释放了"。
    if hotkey::capture_digits() {
        hotkey::set_capture_digits(false);
        log_info!(SCOPE, "收起预览：数字输入模式已释放（capture=false）");
    }
    // capture 也要放掉：它挂在窗口上，隐藏窗口持有 capture 会吞掉后续鼠标消息。
    window::release_capture();
    window::allow_shadow();
    unsafe {
        let _ = KillTimer(Some(preview.raw()), TIMER_TOAST);
        let _ = KillTimer(Some(preview.raw()), TIMER_SHELL_INPUT);
        let _ = KillTimer(Some(preview.raw()), TIMER_SHELL_SWAP);
    }
    // 计时器不在这里停（1.1.0）：窗口收着的时候它转去做投机预渲染。
    // 位置记忆（1.0.13）：收起时把位置落盘，下次打开直接用。
    persist_position();
    retire_shell_preview(old_shell);
    retire_shell_preview(old_cover);
    let _ = preview;
    hotkey::set_wheel_pass_through(false);
    hotkey::set_active(false);
    if let Some(preview) = with_state(|s| s.preview) {
        window::set_activatable(preview.raw(), false);
    }
    // GDI/USER 计数：现场报障「每预览一个文件 GDI +1 且收起不释放」，这两行是二分的地基。
    // 正常应该每次回到同一个值；连续 10 次稳定 +1 就是泄漏。
    let (gdi, user) = window::gui_resources();
    log_info!(SCOPE, "HIDE elapsed={elapsed}ms GDI={gdi} USER={user}");
    *last_hide_slot() = Some(Instant::now());
}

pub(crate) static LAST_HIDE: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

/// 上一次"收起预览"的时刻（见 `LAST_HIDE`）。
pub(crate) fn last_hide_slot() -> MutexGuard<'static, Option<Instant>> {
    LAST_HIDE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn build_lines(target: Option<&Path>) -> Vec<String> {
    let zoom = with_state(|s| s.zoom).unwrap_or(1.0);
    let mut lines = vec![
        version::banner(),
        "内置支持：图片 · PDF · 文本 · Markdown（其余格式以插件形式扩展）".to_string(),
    ];
    match target {
        Some(p) => {
            lines.push(format!("文件：{}", p.display()));
            lines.push(format!(
                "缩放：{:.0}%（+ / - 缩放，上下方向键平移，PageDown 翻页）",
                zoom * 100.0
            ));
            match std::fs::metadata(p) {
                Ok(meta) => lines.push(format!(
                    "大小：{} · 类型：{} ({})",
                    human_size(meta.len()),
                    extension_of(p),
                    format::kind_for_path(p)
                )),
                Err(e) => lines.push(format!("无法读取文件信息：{e}")),
            }
            // 1.0.10：不支持的文件不再画占位（`show_preview` 直接响铃收起），
            // 所以这里只剩"插件接管了"这一种非内置情形。
            if qlr_render::viewer_for(format::kind_for_path(p)).is_none()
                && let Some(plugin) = plugins::host().find_for_path(p)
            {
                lines.push(format!(
                    "插件：{}（接管 .{}）",
                    plugin.name(),
                    extension_of(p).trim_start_matches('.')
                ));
                lines.push(format!("插件文件：{}", plugin.file().display()));
            }
        }
        None => lines.push("没有目标文件（试试 qlrust <文件路径>）".to_string()),
    }
    lines.push(String::new());
    lines.push("托盘右键菜单可退出。".to_string());
    lines
}

pub(crate) fn extension_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_else(|| "（无扩展名）".to_string())
}

pub(crate) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}
