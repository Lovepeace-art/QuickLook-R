//! 渲染投递与生命周期：工作线程、租约与自愈看门狗、结果落地、上一帧缓存、预热。
//!
//! 解码一律在 worker 线程里做（`repaint` 只合成），这里只管"把活派出去、把结果接回来"。

use super::*;

/// 拉起常驻渲染线程。返回任务发送端。
///
/// 线程只拿 `host` 的裸句柄用于 `PostMessageW`（`HWND` 不是 `Send`，所以只能传 `isize`），
/// 绝不碰窗口的其他属性——窗口只能在 UI 线程上操作。
/// 测试钩子：`QLRUST_SLOW_RENDER_MS=5000` 会让每个渲染任务先睡这么久。
///
/// 存在的唯一目的是验证 1.2.0 的"渲染超时/取消"链路——真做出一个要跑 5 s 的
/// 合法文件很难（那才是问题），而"注入延迟"能稳定复现看门狗的行为。
/// 生产环境不会设置这个变量，`std::env::var` 只查一次。
pub(crate) fn fake_slow_ms() -> Option<u64> {
    use std::sync::OnceLock;
    static CACHED: OnceLock<Option<u64>> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("QLRUST_SLOW_RENDER_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|ms| *ms > 0 && *ms <= 60_000)
    })
}

/// 测试钩子（1.8.3）：`QLRUST_RENDER_HANG_MS=30000` 让渲染线程在处理**第一个**任务前
/// 先睡这么久，模拟"卡在第三方 FFI（PDFium / MediaFoundation / 插件 cdylib）里回不来"，
/// 用来验证租约判挂 + 线程自愈这条链路。
///
/// **仅开发/自动化验证用**，和 `QLRUST_DPI_FAKE` / `QLRUST_DPI_FIX` 是同一类开关：
/// 不设置这个变量时行为与改动前完全一致（`std::env::var` 只查一次，热路径零开销）。
///
/// 为什么只注入到**第一条**线程：要验证的是"打开文件 → 约 10 s 后自愈 ERROR →
/// 再打开另一个文件能正常出图"。重建出来的线程如果也睡 30 s，就永远验证不到"恢复"。
pub(crate) fn fake_hang_ms() -> Option<u64> {
    use std::sync::OnceLock;
    static CACHED: OnceLock<Option<u64>> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("QLRUST_RENDER_HANG_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|ms| *ms > 0 && *ms <= 600_000)
    })
}

/// 派发任务前登记租约（UI 线程）。
///
/// 顺序是"先登记、再 `send`"：反过来的话，一个几十微秒就完成的渲染可能先注销，
/// 然后被我们写成一条再也不会被清掉的空租约 —— 10 s 后看门狗就会对一条不存在的活判挂。
///
/// 这里**不**预先生成任务描述：那要在每一帧（包括 3 ms 的文本帧）上多一次 `format!` 和
/// 一次堆分配，而描述只在"真的判挂"那一次才用得上（`mark_hung` 里现算）。
pub(crate) fn begin_lease(job: &RenderJob) {
    let lease = Lease {
        epoch: RENDER_EPOCH.load(Ordering::Acquire),
        generation: job.generation,
        started: Instant::now(),
        job: job.clone(),
    };
    if let Ok(mut guard) = render_track().lock() {
        guard.lease = Some(lease);
    }
}

/// 任务描述（写日志用）：文件名 + 类型 + 目标尺寸 + 页码，**不含完整用户路径**。
pub(crate) fn describe_job(job: &RenderJob) -> String {
    let name = job
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("<无名>");
    let kind = format::kind_for_path(&job.path);
    if kind == format::Kind::Pdf {
        format!(
            "{name} {kind} {}x{} 第{}页",
            job.max_width,
            job.max_height,
            job.page + 1
        )
    } else {
        format!("{name} {kind} {}x{}", job.max_width, job.max_height)
    }
}

/// 投递失败（线程已退出）：撤掉刚登记的租约，否则看门狗会为一条根本不存在的活重建线程。
pub(crate) fn abort_lease(generation: u64) {
    if let Ok(mut guard) = render_track().lock()
        && let Some(lease) = guard.lease.as_ref()
        && lease.generation == generation
    {
        guard.lease = None;
    }
}

/// 渲染线程交活：只有"同一条线程 + 同一代"的租约才有资格注销。
///
/// 两个条件各挡一种错：`epoch` 挡"自愈前那条旧线程迟到返回"（它手里那张租约早属于新世界
/// 了）；`generation` 挡"同一线程里 A 交活时把 B 的租约清掉"（UI 在 A 还在跑的时候就已经
/// 派了 B，单线程 worker 手里是 A，租约却已经是 B 的）。
pub(crate) fn end_lease(epoch: u64, generation: u64) {
    let Ok(mut guard) = render_track().lock() else {
        return;
    };
    if let Some(lease) = guard.lease.as_ref()
        && lease.epoch == epoch
        && lease.generation == generation
    {
        guard.lease = None;
        // 有帧真的跑完了 = 线程是活的：连续自愈计数清零，退避不会累积到下一次偶发挂死。
        guard.consecutive = 0;
    }
}

/// 判挂阈值：连续自愈时翻倍退避（10 s → 20 s → 40 s 封顶，理由见 `RENDER_HANG_BACKOFF_MAX`）。
pub(crate) fn hang_threshold(consecutive: u32) -> Duration {
    Duration::from_millis(RENDER_HANG_MS << consecutive.min(RENDER_HANG_BACKOFF_MAX))
}

/// 判挂（UI 线程，250 ms 一拍）：租约超时、且没有重建在飞时占住 `healing`。
///
/// 返回 `(任务描述, 已卡毫秒, 原任务, 连续第几次, 本会话第几次)`。
pub(crate) fn mark_hung() -> Option<(String, u128, RenderJob, u32, u32)> {
    let mut guard = render_track().lock().ok()?;
    if guard.healing {
        return None;
    }
    let (desc, ms, job) = {
        let lease = guard.lease.as_ref()?;
        let ms = lease.started.elapsed();
        if ms < hang_threshold(guard.consecutive) {
            return None;
        }
        (describe_job(&lease.job), ms.as_millis(), lease.job.clone())
    };
    guard.healing = true;
    Some((
        desc,
        ms,
        job,
        guard.consecutive.saturating_add(1),
        guard.total.saturating_add(1),
    ))
}

/// 重建失败（线程起不来 / 发送端锁被毒化）：退避一次，别每 250 ms 刷一行 ERROR。
pub(crate) fn heal_failed() {
    if let Ok(mut guard) = render_track().lock() {
        guard.healing = false;
        guard.consecutive = guard.consecutive.saturating_add(1);
        // 时钟重启：下一次判挂要再等一个（已经翻倍过的）阈值。
        if let Some(lease) = guard.lease.as_mut() {
            lease.started = Instant::now();
        }
    }
}

/// 重建成功：记一次，并把卡住的那一帧重新登记成新租约（`Some` 表示要重发）。
///
/// **必须在"把这一帧投给新线程"之前调用**：新线程可能几微秒就算完（文本类），
/// 要是它先交活、我们再登记租约，就会留下一条永远没人清的空租约 —— 下一个阈值到点
/// 时看门狗会为一条根本不存在的活再判一次挂死。
pub(crate) fn heal_succeeded(requeue: Option<&RenderJob>) {
    if let Ok(mut guard) = render_track().lock() {
        guard.healing = false;
        guard.lease = None;
        guard.consecutive = guard.consecutive.saturating_add(1);
        guard.total = guard.total.saturating_add(1);
    }
    if let Some(job) = requeue {
        begin_lease(job);
    }
}

/// 渲染线程挂死 → 换一条新线程（1.8.3）。
///
/// 旧线程**不 join、不 kill**：Windows 上 `TerminateThread` 会在它正持有锁（堆锁、加载器锁、
/// 第三方库内部锁）时把进程状态撕坏 —— 那比"烂着一条线程"危险得多。旧线程的 `Sender`
/// 在这里就地换成新的、旧的被 drop，所以它一旦从 FFI 里爬出来，`rx.recv()` 立刻报错、
/// 自己安静退出（线程自然死亡，不占任何资源）。
///
/// 保证任何时刻最多一个重建在飞：`healing` 从判挂起一直占到发送端换完；重建本身是同步的
/// （只 spawn + 换 Sender，不 join 任何东西），UI 线程不会被它拖住。
pub(crate) fn check_render_hang() {
    let Some((desc, ms, job, consecutive, total)) = mark_hung() else {
        return;
    };
    let Some(host) = with_state(|s| s.host) else {
        heal_failed();
        return;
    };
    // 用户还停在这一帧上（代际没变 + 窗口开着）才值得重发，否则没必要白占一条新线程。
    let requeue = with_state(|s| s.visible && s.render_gen == job.generation).unwrap_or(false);
    let (tx, epoch) = match spawn_render_worker(host.raw()) {
        Ok(pair) => pair,
        Err(e) => {
            log_error!(SCOPE, "RENDER 挂死 {ms}ms（{desc}），重建渲染线程失败：{e}");
            heal_failed();
            return;
        }
    };
    // 换发送端 + 让新代号生效 + 登记新租约 + 重发，四件事在同一把锁里按这个顺序做完：
    // - 代号必须先于投递：否则新线程可能在我们写代号之前就算完，结果被当成"旧线程"丢弃；
    // - 租约必须先于投递：否则"几微秒就算完"的那一帧交活时找不到租约（见 `heal_succeeded`）。
    // 锁序与 `request_render` 一致（JOB_TX → RENDER_TRACK），不会与渲染线程那把单锁成环。
    let swapped = match JOB_TX.get().map(|slot| slot.lock()) {
        Some(Ok(mut guard)) => {
            *guard = tx;
            RENDER_EPOCH.store(epoch, Ordering::Release);
            heal_succeeded(requeue.then_some(&job));
            if requeue {
                let _ = guard.send(job.clone());
            }
            true
        }
        _ => false,
    };
    if !swapped {
        // 只有锁被毒化才可能走到这里：新通道随 `tx` 一起丢弃，新线程自己退出。
        log_error!(SCOPE, "RENDER 挂死 {ms}ms（{desc}），重建后没换上发送端");
        heal_failed();
        return;
    }
    if requeue {
        // 重发也算"这一帧重新开始"：让 3 s 的取消告警重新计时。
        with_state(|s| {
            s.render_since = Some(Instant::now());
            s.render_warned = false;
        });
    }
    log_error!(
        SCOPE,
        "RENDER 挂死 {ms}ms（{desc}，代际 {}），已重建渲染线程 epoch={}（连续第 {consecutive} 次 / 本会话第 {total} 次）{}",
        job.generation,
        epoch,
        if requeue {
            "，这一帧已重发给新线程"
        } else {
            ""
        }
    );
}

/// 拉起一条渲染线程。返回 `(任务发送端, 线程代号)`。
///
/// 线程只拿 `host` 的裸句柄用于 `PostMessageW`（`HWND` 不是 `Send`，所以只能传 `isize`），
/// 绝不碰窗口的其他属性——窗口只能在 UI 线程上操作。
///
/// 代号**由调用方**在把发送端换进 `JOB_TX` 之后才写进 `RENDER_EPOCH`（见
/// `check_render_hang`）：换不上通道时新线程根本收不到活，代号也就不该生效，否则旧线程
/// 会立刻变成"已退役"，它下一次交活的结果被白白丢掉。
///
/// 测试钩子：`QLRUST_SLOW_RENDER_MS=5000` 让每个渲染任务先睡这么久（验证 1.2.0 的
/// 取消链路——真造一个要跑 5 s 的合法文件很难，"注入延迟"才能稳定复现看门狗的行为）；
/// `QLRUST_RENDER_HANG_MS` 让第一条线程的第一个任务睡（验证 1.8.3 的挂死自愈）。
/// 两者都只读一次环境变量，生产环境不设置时零开销。
pub(crate) fn spawn_render_worker(host: HWND) -> Result<(Sender<RenderJob>, u64), qlr_core::Error> {
    let (tx, rx) = mpsc::channel::<RenderJob>();
    let host_raw = host.0 as isize;
    let epoch = RENDER_EPOCH.load(Ordering::Acquire) + 1;
    // 挂死只注入到第一条线程：要验证的是"自愈之后能恢复"，新线程必须是好的。
    let hang_ms = if epoch == 1 { fake_hang_ms() } else { None };
    std::thread::Builder::new()
        .name("qlr-render".to_string())
        .spawn(move || {
            let mut hang_simulated = false;
            while let Ok(mut job) = rx.recv() {
                // 1.2.0：队列里堆着好几个过期的活（用户连按方向键）时，只做最新的那个。
                // 工作线程是单线程，不这么做"过期帧"会一个个渲染完再被丢弃——纯浪费。
                let mut dropped = 0u32;
                while let Ok(newer) = rx.try_recv() {
                    job = newer;
                    dropped += 1;
                }
                if dropped > 0 {
                    log_debug!(
                        SCOPE,
                        "RENDER 丢弃 {dropped} 个排队任务，只做最新的 gen={}",
                        job.generation
                    );
                }
                // 新活开跑：清掉上一条活留下的取消信号（否则这一帧一进来就自杀）。
                qlr_render::cancel::reset();
                let started = Instant::now();
                // 测试钩子（1.2.0）：注入一段假的慢渲染，用来验证看门狗与取消链路。
                // 只读一次环境变量，正常使用时不产生任何开销。
                if let Some(ms) = fake_slow_ms() {
                    log_warn!(SCOPE, "RENDER 注入慢渲染 {ms}ms（测试钩子）");
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                }
                // 测试钩子（1.8.3）：模拟"卡在第三方 FFI 里不返回"——不查取消标志、
                // 不响应任何协作式退出，只能靠 UI 线程换一条线程来恢复。
                if let Some(ms) = hang_ms.filter(|_| !hang_simulated) {
                    hang_simulated = true;
                    log_warn!(
                        SCOPE,
                        "RENDER 注入挂死 {ms}ms（测试钩子 QLRUST_RENDER_HANG_MS，仅开发用）"
                    );
                    std::thread::sleep(Duration::from_millis(ms));
                }
                // 测试钩子（1.2.0）：验证崩溃兜底（panic → 报告 + minidump）。
                // release 是 `panic = "abort"`，所以这里**一定会**把进程带走——
                // 这正是要验证的路径：进程没了，但证据必须留下。
                // 注意 `cmd /c "set X=1 && ..."` 里 `set` 会把 `&&` 前的空格算进值里
                // （值变成 `"1 "`），所以这里必须 trim 再比，否则测试钩子静默失效。
                if std::env::var("QLRUST_PANIC_TEST")
                    .ok()
                    .map(|v| v.trim().to_string())
                    .as_deref()
                    == Some("1")
                {
                    panic!("QLRust 崩溃测试（QLRUST_PANIC_TEST=1）");
                }
                // 内置查看器优先；内置不接的格式才问插件（"插件只补空缺，不抢内置"）。
                let kind = format::kind_for_path(&job.path);
                let result = if qlr_render::viewer_for(kind).is_some() {
                    qlr_render::render_file_at_zoom(
                        &job.path,
                        job.max_width,
                        job.max_height,
                        job.page,
                        job.upscale,
                    )
                } else {
                    render_with_plugin(&job.path, kind, job.max_width, job.max_height)
                };
                let elapsed = started.elapsed().as_millis();
                match &result {
                    Ok(canvas) => log_info!(
                        SCOPE,
                        "RENDER gen={} {} {}x{} bytes={} ink={} elapsed={}ms (请求 {}x{})",
                        job.generation,
                        canvas.kind_name(),
                        canvas.dimensions().0,
                        canvas.dimensions().1,
                        canvas.approx_bytes(),
                        canvas.ink_pixels(),
                        elapsed,
                        job.max_width,
                        job.max_height
                    ),
                    Err(e) => log_warn!(
                        SCOPE,
                        "RENDER gen={} 失败 elapsed={}ms：{e}",
                        job.generation,
                        elapsed
                    ),
                }
                // 1.8.3：自愈换线程之后，旧线程迟早会从第三方代码里爬出来交活。
                // 它的结果绝不能进结果槽、也不能通知 UI —— 那会顶掉新线程刚算好的那一帧
                // （UI 只看代际号，一代就对得上），或者把用户早翻过去的旧图糊上来。
                if RENDER_EPOCH.load(Ordering::Acquire) != epoch {
                    log_debug!(
                        SCOPE,
                        "RENDER gen={} 来自已退役的渲染线程（epoch={epoch}），丢弃",
                        job.generation
                    );
                    continue;
                }
                end_lease(epoch, job.generation);
                if let Some(slot) = RESULT.get()
                    && let Ok(mut guard) = slot.lock()
                {
                    *guard = Some((job.generation, result));
                }
                window::post(HWND(host_raw as *mut core::ffi::c_void), ipc::WM_QLR_RENDER);
            }
            log_debug!(SCOPE, "渲染线程退出");
        })
        .map_err(|e| qlr_core::Error::Message(format!("创建渲染线程失败：{e}")))?;
    Ok((tx, epoch))
}

/// 走插件渲染（只在渲染线程调用）。
///
/// 失败原因要能直接写给用户看：插件没装、插件报错、插件给了非法数据，三种情况文案不同。
pub(crate) fn render_with_plugin(
    path: &Path,
    kind: format::Kind,
    max_w: u32,
    max_h: u32,
) -> qlr_core::Result<Canvas> {
    let host = plugins::host();
    let Some(plugin) = host.find_for_path(path) else {
        return Err(qlr_core::Error::Message(format!(
            "{kind} 暂无内置查看器，也没有插件接管这个扩展名：{}",
            path.display()
        )));
    };
    let started = Instant::now();
    // 1.6.0：插件能给出"文本大纲"时优先用文本渲染（等宽、可滚动、缩放不发虚），
    // 位图那条路仍然保留给只画图的插件。
    if let Some(text) = plugin.text(path) {
        // 去掉成串的空行：插件的排版留白在文本视图里会变成"隔一行一个"，很浪费屏幕。
        let mut lines: Vec<String> = Vec::new();
        for raw in text.split('\n') {
            let line = raw.trim_end_matches('\r').to_string();
            if line.is_empty() && lines.last().is_some_and(String::is_empty) {
                continue;
            }
            lines.push(line);
        }
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        if !lines.is_empty() {
            log_info!(
                SCOPE,
                "PLUGIN-TEXT {} {} 行 elapsed={}ms",
                plugin.name(),
                lines.len(),
                started.elapsed().as_millis()
            );
            let total = lines.len();
            // 1.7.0：走**普通文本**通道（spans = None），不带行号栏 —— 插件给的是
            // "目录树/大纲"这类结构性内容，行号是给源码用的，加上反而像日志。
            return Ok(Canvas::Text(qlr_render::TextDoc {
                lines,
                styles: None,
                spans: None,
                total_lines: total,
                truncated: false,
                encoding: "utf-8",
            }));
        }
    }
    let image = plugin.render(path, max_w, max_h).map_err(|e| {
        log_warn!(SCOPE, "插件失败 {}", e);
        qlr_core::Error::Message(e)
    })?;
    log_info!(
        SCOPE,
        "PLUGIN {} {}x{} bytes={} elapsed={}ms (请求 {}x{})",
        plugin.name(),
        image.width,
        image.height,
        image.bytes(),
        started.elapsed().as_millis(),
        max_w,
        max_h
    );
    Ok(Canvas::Bitmap(qlr_render::Bitmap::from_rgba(
        image.width,
        image.height,
        image.rgba,
    )))
}

/// 单帧渲染的像素上限（32 MP ≈ 128 MB BGRA）。
///
/// 高倍缩放时"位图 = 自然尺寸 × zoom"会线性膨胀（8× 的 12 MP 图 = 768 MP），
/// 这里统一兜住：超了就等比缩回去，宁可放大后略微发虚，也不能把内存吃爆。
pub(crate) const MAX_RENDER_PIXELS: u64 = 32_000_000;

/// 把渲染目标夹进 `MAX_RENDER_PIXELS` 内（等比，保持宽高比）。
pub(crate) fn clamp_render_size(w: u32, h: u32) -> (u32, u32) {
    let (w, h) = (w.max(1) as u64, h.max(1) as u64);
    let pixels = w.saturating_mul(h);
    if pixels <= MAX_RENDER_PIXELS {
        return (w as u32, h as u32);
    }
    let scale = (MAX_RENDER_PIXELS as f64 / pixels as f64).sqrt();
    (
        ((w as f64 * scale) as u32).max(1),
        ((h as f64 * scale) as u32).max(1),
    )
}

/// 投一个渲染任务，返回本次的代际号。
pub(crate) fn request_render(path: &Path, max_w: u32, max_h: u32, page: usize) {
    let (max_w, max_h) = clamp_render_size(max_w, max_h);
    let is_pdf = with_state(|s| s.pages.is_some()).unwrap_or(false);
    // 1.3.0：PDF 页缓存。翻回去看一眼是 PDF 最常见的动作，命中就别再麻烦渲染线程。
    // 注意代际也要 +1：否则先前那一帧算完回来会把刚上屏的这一页顶掉。
    if is_pdf {
        let key = pagecache::PageKey::new(path, page, max_w, max_h, file_stamp(path));
        if let Some(canvas) = pagecache::get(&key) {
            let generation = with_state(|s| {
                s.render_gen = s.render_gen.wrapping_add(1);
                s.render_gen
            })
            .unwrap_or(0);
            log_info!(
                SCOPE,
                "PAGECACHE 命中 gen={generation} page={} {}x{}（跳过渲染，缓存 {} 页/{} MB）",
                page + 1,
                max_w,
                max_h,
                pagecache::stats().0,
                pagecache::stats().1 / (1024 * 1024)
            );
            with_state(|s| {
                s.render_since = None;
                s.render_warned = false;
                s.pending_key = None;
            });
            present_canvas(canvas);
            return;
        }
    }
    let generation = with_state(|s| {
        s.render_gen = s.render_gen.wrapping_add(1);
        s.render_gen
    })
    .unwrap_or(0);
    let Some(tx_slot) = JOB_TX.get() else {
        log_warn!(SCOPE, "渲染线程未就绪，跳过渲染");
        return;
    };
    let Ok(tx) = tx_slot.lock() else { return };
    // 1.2.0：新请求发出去之前，先让**正在跑**的那个活自己退出（阶段边界检查点会看到），
    // 否则用户连按方向键时，工作线程要先把前面几个没人要的文件渲染完。
    qlr_render::cancel::cancel();
    let is_pdf = with_state(|s| s.pages.is_some()).unwrap_or(false);
    // 注意：`with_state` 是普通 `Mutex`，**嵌套调用会自己把自己锁死**（这里踩过：
    // 在闭包里又调 `with_state(|s| s.zoom > 1.0)`，进程卡在开窗那一步，日志只到"开窗尺寸"）。
    // 所以先把要用的值取出来，再进闭包。
    let upscale = with_state(|s| s.zoom > 1.0).unwrap_or(false);
    let frame = frame_key(path, page, max_w, max_h, upscale);
    with_state(|s| {
        s.render_since = Some(Instant::now());
        s.render_warned = false;
        // PDF 这一帧算完回来要进页缓存，先把 key 记下来（非 PDF 不记）。
        s.pending_key = if is_pdf {
            Some(pagecache::PageKey::new(
                path,
                page,
                max_w,
                max_h,
                file_stamp(path),
            ))
        } else {
            None
        };
        // 任何格式都记"上一帧"的 key：重开同一个文件时可以直接上屏（1.4.0）。
        s.pending_frame = Some(frame);
    });
    let job = RenderJob {
        generation,
        path: path.to_path_buf(),
        max_width: max_w,
        max_height: max_h,
        page,
        upscale: with_state(|s| s.zoom > 1.0).unwrap_or(false),
    };
    // 1.8.3：登记租约（先登记再投递，顺序理由见 `begin_lease`）。
    begin_lease(&job);
    if tx.send(job).is_err() {
        abort_lease(generation);
        log_warn!(SCOPE, "渲染线程已退出，丢弃本次任务");
    }
}

/// 渲染看门狗（1.2.0）：每 250 ms 看一次"这一帧跑了多久"。
///
/// 只做两件事：**记一条 warn**（带毫秒数，便于事后判断是解析器卡住还是慢盘）、
/// **请求取消**（协作式：渲染器在下一个阶段边界退出；单个 FFI 调用打断不了，
/// 这条限制写在 `qlr_render::cancel` 的模块文档里）。UI 线程绝不因此阻塞——
/// 用户随时还能按空格换文件、按 × 关窗口。
///
/// 1.8.3 起多一件事：**判挂换线程**（`check_render_hang`）。取消只在渲染器肯查标志时
/// 有用；卡死在第三方 FFI 里的线程连标志都摸不到，只能换一条新的顶上去。
pub(crate) fn on_render_watchdog() {
    check_render_hang();
    let slow = with_state(|s| {
        let since = s.render_since?;
        let ms = since.elapsed().as_millis();
        if ms < RENDER_TIMEOUT_MS {
            return None;
        }
        let first = !s.render_warned;
        s.render_warned = true;
        Some((ms, first))
    })
    .flatten();
    let Some((ms, first)) = slow else {
        return;
    };
    if !first {
        return;
    }
    log_warn!(
        SCOPE,
        "RENDER 超时 {ms}ms —— 已请求取消这一帧（窗口仍可关闭/换文件）"
    );
    qlr_render::cancel::cancel();
    // 只有当窗口现在显示的是"占位文字"时才改文案；已经有内容（缩放重渲染）就别动，
    // 否则用户正看着的图会被一行提示顶掉。
    if get_content().is_none() {
        set_lines(vec![
            version::banner(),
            format!(
                "这一帧渲染超过 {} s，正在请求取消…",
                RENDER_TIMEOUT_MS / 1000
            ),
            String::new(),
            "换一个文件或按 × 关闭即可（渲染线程会自己接着做下一帧）".to_string(),
        ]);
        if let Some(preview) = with_state(|s| s.preview)
            && with_state(|s| s.visible).unwrap_or(false)
        {
            window::invalidate(preview.raw());
        }
    }
}

/// UI 线程收到 `WM_QLR_RENDER`：取结果、校验代际、上屏。
pub(crate) fn on_render_done() {
    let taken = RESULT
        .get()
        .and_then(|slot| slot.lock().ok().and_then(|mut guard| guard.take()));
    let Some((generation, result)) = taken else {
        return;
    };
    // 有结果（哪怕是失败/取消）就算这一帧结束，看门狗从零开始计。
    with_state(|s| {
        s.render_since = None;
        s.render_warned = false;
    });
    let Some((current, visible, preview)) = with_state(|s| (s.render_gen, s.visible, s.preview))
    else {
        return;
    };
    if generation != current {
        // 用户已经翻到下一个文件了，这一帧作废。
        log_debug!(SCOPE, "丢弃过期帧 gen={generation} current={current}");
        return;
    }

    // 被更新的请求取消掉的帧：不是失败，静默丢弃（屏幕上继续显示上一帧/占位）。
    if let Err(e) = &result
        && e.is_cancelled()
    {
        log_debug!(SCOPE, "RENDER gen={generation} 已取消（有更新的请求）");
        return;
    }

    let is_bitmap = matches!(&result, Ok(Canvas::Bitmap(_)));
    let rendered_text = matches!(&result, Ok(Canvas::Text(_)));
    // 新的一帧到位了，缩放预览态结束（之后的绘制按 1:1 裁剪/居中）。
    with_state(|s| s.zoom_preview = false);
    match result {
        Ok(canvas) => {
            let canvas = Arc::new(canvas);
            // 1.3.0：PDF 这一帧进页缓存（翻回去就不用再算），并顺手预热相邻页。
            if let Some(key) = with_state(|s| s.pending_key.take()).flatten()
                && matches!(canvas.as_ref(), Canvas::Bitmap(_))
            {
                pagecache::put(key.clone(), canvas.clone());
                prefetch_neighbor_pages(&key);
            }
            // 1.4.0：把这一帧记成"上一帧"，重开同一个文件时直接上屏。
            if let Some(key) = with_state(|s| s.pending_frame.take()).flatten() {
                remember_last_frame(key, canvas.clone());
            }
            set_content(Some(canvas));
            // 两段式首帧收尾（1.8.3）：全清帧把占位帧原地替换掉，两个时间打一条日志。
            if let Some((first_ms, shown_at)) = take_quick_shown() {
                log_debug!(
                    SCOPE,
                    "QUICK gen={generation} 首帧 {first_ms}ms → 全清 {}ms（同尺寸替换）",
                    first_ms + shown_at.elapsed().as_millis()
                );
            }
            // 内容不透明（1.5.0 起不再有亚克力材质），只保证 DWM backdrop 关掉。
            if is_bitmap || rendered_text {
                win11::clear_backdrop(preview.raw());
            }
            if visible {
                window::invalidate(preview.raw());
            }
        }
        Err(e) => {
            set_content(None);
            set_lines(vec![
                version::banner(),
                format!("渲染失败：{e}"),
                String::new(),
                "点击窗口关闭 · 托盘右键菜单退出".to_string(),
            ]);

            if visible {
                window::invalidate(preview.raw());
            }
        }
    }
}

/// 一帧内容到位：上屏 + 挑材质 + 重画。
///
/// 正常渲染（`on_render_done`）与预渲染命中（`show_preview`）共用这条路，
/// 免得其中一条忘了关亚克力 → 图片被壁纸洗白（1.0.1 踩过的坑）。
/// 上一帧的 key（1.4.0）：路径 + 文件戳 + 页号 + 渲染尺寸 + 是否放大。
///
/// 为什么要文件戳：同一个路径的文件可能被改过（编辑器保存、下载覆盖），
/// 只比路径的话会把旧内容当成新的显示出来。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FrameKey {
    pub(crate) path: PathBuf,
    pub(crate) mtime: u64,
    pub(crate) size: u64,
    pub(crate) page: usize,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) upscale: bool,
}

/// 上一帧本身：只留一帧（和 `PREPARED` 一个思路）。
///
/// 内存代价接近 0：这一帧同时被 `CONTENT` 和这里各持一个 `Arc`，是同一块内存；
/// 真正多出来的只是"换文件之后旧那一帧还活着"的那几 MB。
/// 上一帧缓存最多留几帧：3 帧足够覆盖"在 A 和 B 之间来回看"的常见用法。
pub(crate) const LAST_FRAME_CAP: usize = 3;

/// 上一帧缓存的总内存上限（字节）。1.8.3：从"只数帧数"改成"帧数 + 字节"双限。
///
/// 为什么必须按字节兜一道：一帧位图的体积 = 宽 × 高 × 4，跟文件格式无关——
/// 1080p 一帧 5.7 MB，放大预览（8×，按 32 MP 上限截断）一帧能到 128 MB。
/// 只数帧数的话"3 帧"最坏是 ~260 MB，而用户换文件时对此毫无感知，
/// 收盘后内存一直挂着（实测每预览一个文件 Private 就 +5.7 MB 且收起不回落）。
///
/// 24 MB 这个数的取舍：
/// - 1080p（5.7 MB/帧）能留 4 帧，"A/B 来回看"这个主要收益还在；
/// - 换过 4 个 1080p 文件后常驻约 23 MB，而不是"一人一帧"线性涨到几十 MB；
/// - 单帧就超过 24 MB 时（大窗口 / 放大预览）靠 `trim_last_frames` 里的
///   `len() > 1` 仍然保留最近 1 帧——"同一文件重开秒开"这条最大收益不能整条扔掉。
pub(crate) const LAST_FRAME_MAX_BYTES: usize = 24 * 1024 * 1024;

/// 上一帧们的 LRU：头是最久没用到，尾是刚用过。
pub(crate) type LastFrames = std::collections::VecDeque<(FrameKey, Arc<Canvas>)>;
pub(crate) static LAST_FRAMES: OnceLock<Mutex<LastFrames>> = OnceLock::new();

pub(crate) fn last_frame_slot() -> &'static Mutex<LastFrames> {
    LAST_FRAMES.get_or_init(|| Mutex::new(std::collections::VecDeque::new()))
}

/// 取文件戳（mtime 秒 + 字节数）。拿不到就是 (0, 0)，此时缓存按"永不过期"处理。
pub(crate) fn file_stamp(path: &Path) -> (u64, u64) {
    match std::fs::metadata(path) {
        Ok(meta) => {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (mtime, meta.len())
        }
        Err(_) => (0, 0),
    }
}

/// 组装一帧的 key。
pub(crate) fn frame_key(
    path: &Path,
    page: usize,
    width: u32,
    height: u32,
    upscale: bool,
) -> FrameKey {
    let (mtime, size) = file_stamp(path);
    FrameKey {
        path: path.to_path_buf(),
        mtime,
        size,
        page,
        width,
        height,
        upscale,
    }
}

/// 上一帧还在吗（且就是这个 key）？命中就把整帧交出去，调用方直接上屏、不用再渲染。
pub(crate) fn take_last_frame(key: &FrameKey) -> Option<Arc<Canvas>> {
    let mut guard = last_frame_slot().lock().ok()?;
    let pos = guard.iter().position(|(stored, _)| stored == key)?;
    // 命中就挪到队尾（最近使用），这样"在 A/B 之间来回看"不会把 A 挤掉。
    let entry = guard.remove(pos)?;
    let canvas = entry.1.clone();
    guard.push_back(entry);
    Some(canvas)
}

pub(crate) fn remember_last_frame(key: FrameKey, canvas: Arc<Canvas>) {
    let Ok(mut guard) = last_frame_slot().lock() else {
        return;
    };
    // 同一个 key 再来一次就替换（内容以新的为准），并挪到队尾。
    if let Some(pos) = guard.iter().position(|(stored, _)| stored == &key) {
        guard.remove(pos);
    }
    guard.push_back((key, canvas));
    trim_last_frames(&mut guard);
}

/// 一帧占多少字节（只用于"上一帧缓存"的预算，不做精确统计）。
pub(crate) fn canvas_bytes(canvas: &Canvas) -> usize {
    match canvas {
        Canvas::Bitmap(b) => b.bgra.len(),
        // 文本帧是重排结果，量级只有几十 KB：按行估一个偏大的上限即可。
        Canvas::Text(t) => t.lines.len().saturating_mul(256),
    }
}

/// 淘汰最旧的帧，直到同时满足"帧数 ≤ [`LAST_FRAME_CAP`]"和"总字节 ≤ [`LAST_FRAME_MAX_BYTES`]"。
///
/// 唯一的例外是**至少保留最近 1 帧**：单帧就可能超预算（放大预览一帧 128 MB），
/// 这时限额给"同一文件重开秒开"让路——宁可超一点，也不要每次重开都重新渲染。
pub(crate) fn trim_last_frames(frames: &mut LastFrames) {
    while frames.len() > LAST_FRAME_CAP {
        frames.pop_front();
    }
    let mut total: usize = frames.iter().map(|(_, canvas)| canvas_bytes(canvas)).sum();
    while total > LAST_FRAME_MAX_BYTES && frames.len() > 1 {
        let Some((_, evicted)) = frames.pop_front() else {
            break;
        };
        total = total.saturating_sub(canvas_bytes(&evicted));
    }
}

/// 只有超过这个像素数的图片才给"两段式首帧"（与 `source_cache::CACHE_MAX_PIXELS` 对齐）。
///
/// 阈值以下（4K = 8.3 MP）全量解码本来就在 60–80 ms 量级，再插一段占位只是多一次跳变。
pub(crate) const QUICK_MIN_PIXELS: u64 = qlr_render::source_cache::CACHE_MAX_PIXELS;

/// 两段式首帧是否启用（默认关，见 `show_preview` 里的注释与实测数字）。
pub(crate) static QUICK_FRAME_ENABLED: OnceLock<bool> = OnceLock::new();

/// 第一段已经上屏：`(第一段耗时 ms, 上屏时刻)`，等全清帧替换时打一条"两段式"日志。
pub(crate) static QUICK_SHOWN: OnceLock<Mutex<Option<(u128, Instant)>>> = OnceLock::new();

pub(crate) fn take_quick_shown() -> Option<(u128, Instant)> {
    let slot = QUICK_SHOWN.get_or_init(|| Mutex::new(None));
    slot.lock().ok().and_then(|mut guard| guard.take())
}

/// 两段式首帧的第一段（1.8.3）：给 >16 MP 的图片先塞一张"形状对、但模糊"的占位快照。
///
/// 只认**已经有缩略图缓存**的那一种（`SIIGBF_INCACHEONLY`）。实测（本机 48MP PNG，
/// `cargo test -p qlr-platform shell_thumbnail -- --ignored --nocapture`）：
/// - 让 Shell 现解：`THUMBNAILONLY` 171 ms、普通 `RESIZETOFIT` 108–149 ms —— 和我们
///   自己的全量解码（197 ms）同一量级，插一段只会把"清晰帧"往后推，纯亏；
/// - 读**已有缓存**：缓存命中时个位数毫秒；没有缓存时 **18 µs** 就失败返回，零成本。
///
/// 另一条常见设想"先解 1/4 再放大"在这里也不成立：PNG 是**单条 zlib 流**，
/// 降采样照样要完整 inflate（DEFLATE 本身 ~89 ms），一分钱省不下来。
///
/// 硬约束：这一帧**绝不**写进 `pagecache` / 上一帧缓存——它只走 `set_content`，
/// 全清帧到了原地上屏替换；重开文件命中的仍是全清帧，像素与"直接全量解码"完全一致。
pub(crate) fn quick_first_frame(
    kind: format::Kind,
    natural: Option<(u32, u32)>,
    path: Option<&Path>,
    long_side: u32,
) {
    if kind != format::Kind::Image {
        return;
    }
    let (Some((src_w, src_h)), Some(path)) = (natural, path) else {
        return;
    };
    if u64::from(src_w) * u64::from(src_h) <= QUICK_MIN_PIXELS {
        return;
    }
    let started = Instant::now();
    let Some(thumb) = thumbnail::shell_thumbnail(path, long_side.max(1), true) else {
        // 没有缩略图缓存：什么都不做，保持"一次到位"的老行为（不跳变、不变慢）。
        return;
    };
    let elapsed = started.elapsed().as_millis();
    let (w, h) = (thumb.width, thumb.height);
    set_content(Some(Arc::new(Canvas::Bitmap(qlr_render::Bitmap::new(
        w, h, thumb.bgra,
    )))));
    log_debug!(
        SCOPE,
        "QUICK 首帧 {w}x{h} elapsed={elapsed}ms（Shell 缩略图缓存，源 {src_w}x{src_h}）"
    );
    let slot = QUICK_SHOWN.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = slot.lock() {
        *guard = Some((elapsed, Instant::now()));
    }
}

/// 缓存里的这一帧是不是按"这次要用的尺寸"渲出来的。
///
/// 尺寸不一致还拿来用的话，上屏那一侧会走拉伸路径：画面发虚、每帧重采样（拖动掉帧、滚动频闪）。
pub(crate) fn frame_matches_size(canvas: &Canvas, width: u32, height: u32) -> bool {
    match canvas {
        Canvas::Bitmap(bitmap) => bitmap.width == width && bitmap.height == height,
        // 文本是无损重排的，尺寸无关（重新排版即可）。
        Canvas::Text(_) => true,
    }
}

pub(crate) fn canvas_size(canvas: &Canvas) -> (u32, u32) {
    match canvas {
        Canvas::Bitmap(bitmap) => (bitmap.width, bitmap.height),
        Canvas::Text(_) => (0, 0),
    }
}

/// 预渲染帧能不能直接上屏（"装得进这个渲染盒"）。
///
/// 为什么不能用 [`frame_matches_size`] 的精确相等：渲染器把内容**适配**进盒子，
/// 出来的位图**本来就比盒子小一点**——
/// - PDF 按页面比例留白：`bitmap 619x876` 对 `请求 619x894`（差 18 px 是页脚留白）；
/// - 位图按整像素取整：`bitmap 1588x893` 对 `请求 1589x894`（差 1 px）。
///
/// 精确相等会把这两种**正常**结果全部判成"尺寸不合"，预热/上一帧就永远用不上。
/// 所以判据换成两条同时成立：
/// 1. **不超出盒子**（任一边更大 → 那是别的窗口尺寸渲的，上屏会被裁）；
/// 2. **至少贴住盒子的一条边**（±2 px）→ 说明它就是"这个盒子"的适配结果；
///    更小的旧窗口渲出来的帧两条边都差得远，照样会被拒。
pub(crate) fn fits_render_box(canvas: &Canvas, width: u32, height: u32) -> bool {
    match canvas {
        Canvas::Text(_) => true,
        Canvas::Bitmap(bitmap) => {
            let inside = bitmap.width <= width && bitmap.height <= height;
            let touches = bitmap.width.abs_diff(width) <= 2 || bitmap.height.abs_diff(height) <= 2;
            inside && touches
        }
    }
}

pub(crate) fn present_canvas(canvas: Arc<Canvas>) {
    let Some(preview) = with_state(|s| s.preview) else {
        return;
    };
    let visible = with_state(|s| s.visible).unwrap_or(false);
    let opaque = matches!(canvas.as_ref(), Canvas::Bitmap(_) | Canvas::Text(_));
    with_state(|s| s.zoom_preview = false);
    set_content(Some(canvas));
    let _ = opaque;
    win11::clear_backdrop(preview.raw());
    if visible {
        window::invalidate(preview.raw());
    }
}

// ---------------------------------------------------------------------------
// 投机预渲染（1.1.0）
// ---------------------------------------------------------------------------

/// 预渲染留下的那一帧：只留一格，尺寸/路径都对得上才用，否则直接丢（省内存）。
pub(crate) struct Prepared {
    pub(crate) path: PathBuf,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) canvas: Arc<Canvas>,
}

/// 预渲染线程收到的任务（1.3.0 起有两种）：塞进单槽给"按空格"用，还是只写页缓存。
pub(crate) enum WarmTask {
    /// 给"用户选中了这个文件、可能要按空格"准备的（1.1.0 的投机预渲染）。
    Prepared(RenderJob),
    /// 给"用户正在看这个 PDF 的相邻页"准备的，结果只进页缓存（1.3.0）。
    Page(RenderJob),
}

/// 预渲染结果的槽（和 `RESULT` 分开：它不进 UI 队列，也不占代际号）。
pub(crate) static PREPARED: OnceLock<Mutex<Option<Prepared>>> = OnceLock::new();
/// 预渲染线程的投递口。
pub(crate) static PREWARM_TX: OnceLock<Mutex<Option<Sender<WarmTask>>>> = OnceLock::new();

pub(crate) fn prepared_slot() -> &'static Mutex<Option<Prepared>> {
    PREPARED.get_or_init(|| Mutex::new(None))
}

/// 预渲染的图像上限（像素）。20 MP ≈ 80 MB BGRA，超过就不值得为一个"可能不按"的文件预花。
pub(crate) const PREWARM_MAX_PIXELS: u64 = 20_000_000;
/// 文本类预热上限（字节）：和查看器的 8 MB 读入上限一致，再大预热也只是一部分。
pub(crate) const PREWARM_MAX_TEXT_BYTES: u64 = 8 * 1024 * 1024;
/// PDF 预热上限（字节）：30 MB 之外就别预读了（200 页的扫描件预渲染一次要几百毫秒）。
pub(crate) const PREWARM_MAX_PDF_BYTES: u64 = 30 * 1024 * 1024;
/// 视频预热上限（字节）：首帧解码要过 Media Foundation，100 MB 之外不预。
pub(crate) const PREWARM_MAX_VIDEO_BYTES: u64 = 100 * 1024 * 1024;

/// 开一条独立的预渲染线程。
///
/// 为什么不复用 `qlr-render` 那条：预热是"抢在用户按空格之前先算"，一旦用户真按了，
/// 真正要看的这一帧不能排在预热后面等——两条独立线程，最坏情况只是多占几毫秒 CPU。
pub(crate) fn spawn_prewarm_worker() -> qlr_core::Result<()> {
    let (tx, rx) = mpsc::channel::<WarmTask>();
    std::thread::Builder::new()
        .name("qlr-prewarm".to_string())
        .spawn(move || {
            while let Ok(task) = rx.recv() {
                match task {
                    WarmTask::Page(job) => warm_page(&job),
                    WarmTask::Prepared(job) => warm_prepared(&job),
                }
            }
            log_debug!(SCOPE, "预渲染线程退出");
        })
        .map_err(|e| qlr_core::Error::Message(format!("创建预渲染线程失败：{e}")))?;
    if let Ok(mut guard) = PREWARM_TX.get_or_init(|| Mutex::new(None)).lock() {
        *guard = Some(tx);
    }
    Ok(())
}

/// 投机预渲染：算好一帧塞进单槽，等用户按空格。
pub(crate) fn warm_prepared(job: &RenderJob) {
    // 排队期间用户可能已经换了选中项：先看一眼还值不值得算。
    if !prewarm_still_wanted(&job.path) {
        log_debug!(SCOPE, "PREWARM 跳过（已过时）：{}", job.path.display());
        return;
    }
    let kind = format::kind_for_path(&job.path);
    if qlr_render::viewer_for(kind).is_none() {
        // 插件渲染一律不预热：插件是第三方代码，不该在"用户没要求"时跑。
        return;
    }
    let started = Instant::now();
    let result =
        qlr_render::render_file_at_zoom(&job.path, job.max_width, job.max_height, job.page, false);
    let elapsed = started.elapsed().as_millis();
    match result {
        Ok(canvas) => {
            // 算完再确认一次：这中间用户可能已经选到别的文件了，别把过期帧塞进槽。
            if !prewarm_still_wanted(&job.path) {
                log_debug!(
                    SCOPE,
                    "PREWARM 丢弃（算完已过时）{} elapsed={}ms",
                    job.path.display(),
                    elapsed
                );
                return;
            }
            if let Ok(mut guard) = prepared_slot().lock() {
                *guard = Some(Prepared {
                    path: job.path.clone(),
                    width: job.max_width,
                    height: job.max_height,
                    canvas: Arc::new(canvas),
                });
            }
            log_info!(
                SCOPE,
                "PREWARM 就绪 {} {}x{} elapsed={}ms",
                job.path.display(),
                job.max_width,
                job.max_height,
                elapsed
            );
        }
        Err(e) => log_debug!(
            SCOPE,
            "PREWARM 失败 {} elapsed={}ms：{e}",
            job.path.display(),
            elapsed
        ),
    }
}

/// 相邻页预取（1.3.0）：只把结果写进页缓存，不上屏、不碰单槽。
pub(crate) fn warm_page(job: &RenderJob) {
    let key = pagecache::PageKey::new(
        &job.path,
        job.page,
        job.max_width,
        job.max_height,
        file_stamp(&job.path),
    );
    if pagecache::has(&key) {
        return;
    }
    let started = Instant::now();
    match qlr_render::render_file_at_zoom(&job.path, job.max_width, job.max_height, job.page, false)
    {
        Ok(canvas) => {
            let elapsed = started.elapsed().as_millis();
            let canvas = Arc::new(canvas);
            pagecache::put(key, canvas);
            let (pages, mb) = pagecache::stats();
            log_debug!(
                SCOPE,
                "PAGECACHE 预取 page={} elapsed={}ms（缓存 {} 页/{} MB）",
                job.page + 1,
                elapsed,
                pages,
                mb / (1024 * 1024)
            );
        }
        Err(e) => log_debug!(SCOPE, "PAGECACHE 预取失败 page={}：{e}", job.page + 1),
    }
}

/// 当前这一页算完/上屏之后，顺手把前后各一页丢给预渲染线程。
///
/// 只做相邻一页（不是整本）：用户翻页基本是循序的，一页够用；
/// 整本预取既慢又占内存，还会跟"用户真正要看的那一帧"抢 CPU。
pub(crate) fn prefetch_neighbor_pages(key: &pagecache::PageKey) {
    let (pages, prefetch) =
        with_state(|s| (s.pages, s.config.prewarm.is_on())).unwrap_or((None, false));
    let Some(total) = pages else { return };
    if !prefetch {
        return;
    }
    let Some(slot) = PREWARM_TX.get() else {
        return;
    };
    let Ok(guard) = slot.lock() else { return };
    let Some(tx) = guard.as_ref() else { return };
    for delta in [1_i64, -1] {
        let target = key.page as i64 + delta;
        if target < 0 || target as usize >= total {
            continue;
        }
        // 文件戳直接沿用当前这一页的 key：同一帧里前后页必然同属一个版本的
        // 文件，再查一次 metadata 只是白花一次系统调用（还可能与 key 不一致）。
        let neighbor = pagecache::PageKey::new(
            &key.path,
            target as usize,
            key.width,
            key.height,
            (key.mtime, key.len),
        );
        if pagecache::has(&neighbor) {
            continue;
        }
        let _ = tx.send(WarmTask::Page(RenderJob {
            generation: 0,
            path: neighbor.path.clone(),
            max_width: neighbor.width,
            max_height: neighbor.height,
            page: neighbor.page,
            upscale: false,
        }));
    }
}

/// 这个路径还是当前"准备预热"的候选吗？（用户已经换选中项就作废）
pub(crate) fn prewarm_still_wanted(path: &Path) -> bool {
    with_state(|s| s.prewarm_candidate.as_deref() == Some(path)).unwrap_or(false)
}

/// 投一个预渲染任务。
pub(crate) fn request_prewarm(path: &Path, width: u32, height: u32) {
    let (max_width, max_height) = clamp_render_size(width, height);
    let Some(slot) = PREWARM_TX.get() else {
        return;
    };
    let Ok(guard) = slot.lock() else { return };
    let Some(tx) = guard.as_ref() else { return };
    let job = RenderJob {
        generation: 0,
        path: path.to_path_buf(),
        max_width,
        max_height,
        page: 0,
        upscale: false,
    };
    if tx.send(WarmTask::Prepared(job)).is_err() {
        log_warn!(SCOPE, "预渲染线程已退出，丢弃本次预热");
    }
}

/// 取出与 `path` + 目标尺寸匹配的预渲染帧（对不上就把槽清空，别占着几 MB 内存）。
pub(crate) fn take_prepared(path: Option<&Path>, width: u32, height: u32) -> Option<Arc<Canvas>> {
    let path = path?;
    let mut guard = prepared_slot().lock().ok()?;
    let prepared = guard.take()?;
    // 尺寸允许 ±2 px 误差：两次计算中间可能被 DPI/工作区微调过一次，差两三像素看不出来。
    let close = |a: u32, b: u32| a.abs_diff(b) <= 2;
    if prepared.path != path || !close(prepared.width, width) || !close(prepared.height, height) {
        log_debug!(
            SCOPE,
            "PREWARM 未命中（{} {}x{} vs 需要 {}x{}）",
            prepared.path.display(),
            prepared.width,
            prepared.height,
            width,
            height
        );
        return None;
    }
    Some(prepared.canvas)
}

/// 预热预算闸门：按类型封顶，超过就不预热。
///
/// 预热是"用户没要求时花 CPU"，所以宁可少预几个，也不要在 200 页扫描件上卡一下。
pub(crate) fn prewarm_within_budget(path: &Path, kind: format::Kind) -> bool {
    match kind {
        format::Kind::Image => match qlr_render::natural_size(path, kind) {
            Some((w, h)) => (w as u64) * (h as u64) <= PREWARM_MAX_PIXELS,
            None => false,
        },
        // SVG / 视频也按"原始像素"算预算：8000×6000 的矢量图跟同尺寸位图一样贵。
        format::Kind::Svg => match qlr_render::natural_size(path, kind) {
            Some((w, h)) => (w as u64) * (h as u64) <= PREWARM_MAX_PIXELS,
            None => false,
        },
        // 视频要过一遍 Media Foundation 解码首帧（几十毫秒级），只在文件不大时预。
        format::Kind::Video => std::fs::metadata(path)
            .map(|m| m.len() <= PREWARM_MAX_VIDEO_BYTES)
            .unwrap_or(false),
        format::Kind::Pdf => std::fs::metadata(path)
            .map(|m| m.len() <= PREWARM_MAX_PDF_BYTES)
            .unwrap_or(false),
        // HTML 跟文本同量级（读 + 排版），共用文本预算。
        format::Kind::Text | format::Kind::Markdown | format::Kind::Html => std::fs::metadata(path)
            .map(|m| m.len() <= PREWARM_MAX_TEXT_BYTES)
            .unwrap_or(false),
        // 音频：卡片内容只靠元信息，渲染本身极便宜；但"预渲染"对听声音没意义，不预。
        format::Kind::Audio => false,
        format::Kind::Unsupported => false,
    }
}

/// 窗口收着：把"当前选中的这一个文件"先渲染好，等用户按空格。
///
/// 用户原话（m16283）："二期默认自适应吧，我试试效果，后续我觉得没用的话会放弃掉这项改进"。
/// 所以每一道闸门都往"少烧 CPU"那边偏：多花的那点 CPU 必须换来看得见的即时感。
pub(crate) fn maybe_prewarm() {
    let Some(config) = with_state(|s| s.config.clone()) else {
        return;
    };
    if !config.prewarm.is_on() {
        return;
    }
    // 自适应档：只有"最近真的按过空格"才预热。一个只把 QLRust 当托盘程序挂着的人
    // 不应该被我们每 400 ms 摸一次资源管理器。
    if config.prewarm == qlr_core::config::Prewarm::Adaptive {
        let recent = with_state(|s| {
            s.last_show_at
                .map(|t| t.elapsed() < qlr_core::config::Prewarm::RECENT)
                .unwrap_or(false)
        })
        .unwrap_or(false);
        if !recent {
            return;
        }
    }
    let selection = selection::current_selection();
    // 多选不猜：用户框选一堆文件时"他会看哪个"是纯随机。
    if selection.paths.len() != 1 {
        return;
    }
    let Some(path) = selection.first().map(|p| p.to_path_buf()) else {
        return;
    };
    let kind = format::kind_for_path(&path);
    // 不支持/只有插件接的格式不预热：插件是第三方代码，不该在"用户没要求"时跑。
    if qlr_render::viewer_for(kind).is_none() {
        return;
    }
    if !prewarm_within_budget(&path, kind) {
        // 超预算的文件会一直被选中，每 400 ms 记一行会把日志刷满；只在"换了个文件"时记一次。
        let first = with_state(|s| {
            let fresh = s.prewarm_candidate.as_deref() != Some(path.as_path());
            s.prewarm_candidate = Some(path.clone());
            fresh
        })
        .unwrap_or(false);
        if first {
            log_debug!(SCOPE, "PREWARM 超预算跳过 {}（{kind}）", path.display());
        }
        return;
    }
    // 两拍稳定：同一路径连续两次轮询才动手，用户按住方向键扫文件时不会跟着抢 CPU。
    let stable = with_state(|s| {
        let same = s.prewarm_candidate.as_deref() == Some(path.as_path());
        s.prewarm_candidate = Some(path.clone());
        same
    })
    .unwrap_or(false);
    if !stable {
        log_debug!(SCOPE, "PREWARM 候选 {}（{kind}）", path.display());
        return;
    }
    // 已经有这文件的结果了就别重算。
    if prepared_slot()
        .lock()
        .map(|g| g.as_ref().is_some_and(|p| p.path == path))
        .unwrap_or(false)
    {
        return;
    }
    let Some(preview) = with_state(|s| s.preview) else {
        return;
    };
    let work = window::work_area(preview.raw());
    let natural = qlr_render::natural_size(&path, kind);
    let remembered = with_state(|s| {
        s.last_sizes
            .get(kind.name())
            .copied()
            .or_else(|| s.config.window_size_for(kind.name()))
    })
    .flatten();
    let (width, height) = window_size_for(work, kind, natural, &config, remembered);
    // 预热必须用和上屏**同一个**渲染尺寸，否则结果永远打不中缓存键：
    // 上屏那条（`show_preview`）传的是整窗尺寸 + `layout::reserve_for_window`，
    // 这里是同一份 `window_size_for` + 同一个 reserve，所以两个渲染盒逐字相同。
    // （曾经这里漏了 reserve，差一条 `chrome_height`；`take_prepared` 容差只有 ±2 px，
    //  于是预热结果永远对不上 —— 白烧 CPU 却一点加速都没有。）
    let pages = if kind == format::Kind::Pdf {
        qlr_render::pdf_page_count(&path)
    } else {
        None
    };
    let reserve = layout::reserve_for_window(natural);
    let (width, height) = render_box(kind, pages, width, height, reserve, 1.0);
    log_debug!(
        SCOPE,
        "PREWARM 开始 {} {width}x{height}（自然 {:?}，reserve={reserve}）",
        path.display(),
        natural
    );
    request_prewarm(&path, width.max(1), height.max(1));
}
