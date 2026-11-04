//! 进程入口与消息分派：`run`、窗口过程、托盘/设置动作、IPC 转交、退出与诊断。
//!
//! 这里是"所有事情的入口"，所以只做**分派**——真正的动作都在别的模块里。

use super::*;

/// 程序入口。返回进程退出码。
pub fn run() -> ExitCode {
    dpi::enable_per_monitor_v2();
    // 1.8.3：主线程先按"系统 DPI"起步。开窗尺寸是在**窗口建出来之前**算的
    // （`window_size_for`），那时没有 hwnd 可查，只能用系统 DPI；单屏用户这就是对的。
    // 窗口建好之后由窗口过程里的 `dpi::enter(hwnd)` 按窗口实际 DPI 细化。
    dpi::seed_thread(dpi::system_dpi());
    // 装"DPI 变了 → 重建字体"的钩子：字体是按像素高度 CreateFontW 出来的，
    // 不重建就会一直是旧尺寸。
    window::install_dpi_hook();
    let _ = paths::ensure_dirs();

    let (config, config_err) = Config::load(&paths::config_path());
    // 1.8.3：内容缩放（正文/目录/列表）与外壳分开——见 `Config::content_scale_percent`。
    // 必须在读配置之后设（默认 0 = 自动，200% 下按 1.41 倍而不是 2 倍）。
    dpi::set_content_percent(config.content_scale_percent);
    let level = Level::from_env_or(level_from_str(&config.log_level));
    let _ = log::init(level, &paths::log_file());

    log_info!(SCOPE, "=== 启动 {} ===", version::banner());
    // 崩溃兜底（1.2.0）：先装 hook（越早越好，后面任何 panic 都能留下 dmp），
    // 再报告"上次是不是崩过"。
    crash::install_hook();
    crash::report_previous_crash();
    log_info!(
        SCOPE,
        "exe_dir={:?} portable={} data_dir={:?}",
        paths::exe_dir(),
        paths::is_portable(),
        paths::data_dir()
    );
    log_info!(SCOPE, "config: {}", config.summary());
    let hotkey_enabled = config.hotkey_enabled;
    if let Some(err) = config_err {
        log_warn!(SCOPE, "{err}");
    }
    log_info!(SCOPE, "log_file={:?}", log::path());

    // Shell 选中项要 COM（STA）。必须在 UI 线程上初始化，且要在任何 shell 调用之前。
    let com_ok = selection::init_com();
    log_info!(SCOPE, "COM 初始化={com_ok}");
    // 完整性级别决定"能不能跟 explorer 说话"：低于 medium 时托盘图标与 Shell 选区
    // 都会被 UIPI 拒成 0x80070005（坑 #29）。写进启动日志，排障第一眼就有答案。
    log_info!(SCOPE, "令牌完整性={}", integrity::describe());

    // GUI 子系统没有 stderr：panic 必须同时写进日志 + 落盘报告。
    // 这一步已经由启动时的 `crash::install_hook()` 做完（删掉这里旧的一条，
    // 否则后装的 hook 会把前面那个带 minidump 的覆盖掉——1.2.0 踩过）。

    let started = Instant::now();
    let command = cli::parse(std::env::args().skip(1));

    // 诊断命令：查一次选中项就退出（早于单实例判定，可以和常驻实例共存）。
    let probe_target = match &command {
        Command::ProbeSelection => Some(None),
        Command::ProbeSelectionOf(hwnd) => Some(Some(*hwnd)),
        _ => None,
    };
    if let Some(target) = probe_target {
        let found = match target {
            Some(raw) => {
                log_info!(SCOPE, "PROBE-SELECTION frame=0x{raw:X}");
                selection::selection_of_hwnd(HWND(raw as *mut core::ffi::c_void))
            }
            None => selection::current_selection(),
        };
        for (index, path) in found.paths.iter().enumerate() {
            log_info!(SCOPE, "PROBE-SELECTION [{index}] {}", path.display());
        }
        log_info!(SCOPE, "PROBE-SELECTION {}", found.summary());
        if com_ok {
            selection::uninit_com();
        }
        return ExitCode::SUCCESS;
    }

    // 安装器要的文件关联扩展名清单：由 `format::all_extensions()`（唯一真值）写出，
    // 不建窗口、不进单实例，写完就退出。
    if let Command::ListExtensions(out) = &command {
        let mut text = String::new();
        for ext in format::all_extensions() {
            text.push_str(ext);
            text.push('\n');
        }
        return match std::fs::write(out, text.as_bytes()) {
            Ok(()) => {
                log_info!(
                    SCOPE,
                    "LIST-EXTENSIONS {} 个 -> {:?}",
                    format::all_extensions().len(),
                    out
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                log_error!(SCOPE, "LIST-EXTENSIONS 写 {:?} 失败：{e}", out);
                ExitCode::from(4)
            }
        };
    }

    // 无窗口渲染（A2.10 基准 / A2.8 PDF 诊断）：不建窗口、不进单实例，
    // 渲染一次把结果写日志就退出 —— 这样基准脚本不需要桌面会话也能跑。
    if let Command::Render { path, page } = &command {
        let target = std::path::Path::new(path);
        let t0 = Instant::now();
        let pages = qlr_render::pdf_page_count(target);
        match qlr_render::render_file_at(target, 1190, 748, *page) {
            Ok(canvas) => {
                log_info!(
                    SCOPE,
                    "RENDER-CLI ok kind={} bytes={} pages={:?} page={} elapsed={}ms",
                    canvas.kind_name(),
                    canvas.approx_bytes(),
                    pages,
                    page,
                    t0.elapsed().as_millis()
                );
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                log_error!(
                    SCOPE,
                    "RENDER-CLI FAIL page={} elapsed={}ms err={e}",
                    page,
                    t0.elapsed().as_millis()
                );
                return ExitCode::from(3);
            }
        }
    }

    let request = match command {
        Command::Help => {
            log_info!(SCOPE, "帮助：\n{}", cli::usage());
            return ExitCode::SUCCESS;
        }
        Command::Version => {
            log_info!(SCOPE, "{}", version::banner());
            return ExitCode::SUCCESS;
        }
        // 上面已经处理并 return 了。
        Command::ProbeSelection
        | Command::ProbeSelectionOf(_)
        | Command::Render { .. }
        | Command::ListExtensions(_) => {
            unreachable!()
        }
        Command::Invoke(req) => req,
    };

    // 已有实例在跑 → 把请求转交过去，本进程立刻退出。
    if let Some(host) = instance::find_host(ipc::HOST_WINDOW_CLASS) {
        return forward(host, &request, started);
    }

    // 抢单实例。Ok(None) 说明刚好有人抢先，再试一次转交。
    let _guard = match instance::acquire(ipc::INSTANCE_MUTEX) {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            return match instance::find_host(ipc::HOST_WINDOW_CLASS) {
                Some(host) => forward(host, &request, started),
                None => {
                    log_error!(SCOPE, "互斥体被占用但找不到宿主窗口，放弃启动");
                    ExitCode::from(2)
                }
            };
        }
        Err(e) => {
            log_error!(SCOPE, "创建单实例互斥体失败：{}", qlr_platform::to_core(e));
            return ExitCode::from(2);
        }
    };

    // 注册窗口类（宿主 + 预览，共用一个窗口过程）。
    for class in [ipc::HOST_WINDOW_CLASS, PREVIEW_CLASS] {
        if let Err(e) = window::register_class(class, Some(wndproc)) {
            log_error!(
                SCOPE,
                "注册窗口类 {class} 失败：{}",
                qlr_platform::to_core(e)
            );
            return ExitCode::from(4);
        }
    }

    let host = match window::create_host_window(ipc::HOST_WINDOW_CLASS, &version::banner()) {
        Ok(h) => h,
        Err(e) => {
            log_error!(SCOPE, "创建宿主窗口失败：{}", qlr_platform::to_core(e));
            return ExitCode::from(5);
        }
    };
    let preview = match window::create_preview_window(PREVIEW_CLASS, "QLRust 预览") {
        Ok(h) => h,
        Err(e) => {
            log_error!(SCOPE, "创建预览窗口失败：{}", qlr_platform::to_core(e));
            return ExitCode::from(5);
        }
    };
    // 柔和投影（1.0.10）：一个独立的分层窗口贴在预览窗下面。
    // 拿不到就当作没有阴影（不影响预览本身），所以失败只记日志。
    let shadow = match window::register_class(SHADOW_CLASS, Some(wndproc))
        .and_then(|()| window::create_shadow_window(SHADOW_CLASS, "QLRust 阴影"))
    {
        Ok(h) => Some(Hwnd::from_raw(h)),
        Err(e) => {
            log_warn!(
                SCOPE,
                "创建阴影窗口失败（不显示投影）：{}",
                qlr_platform::to_core(e)
            );
            None
        }
    };
    // Win11 才有 DWM 圆角/backdrop（Win10 上这两个属性不存在，调用静默失败）。
    // 圆角/backdrop 都走 DWM（Win11 才有 backdrop，Win10 自动降级为无效果）。
    // Win11 走官方 DWM 属性。两者都取不到时退回纯色背景。
    let effects = win11::apply(preview);
    log_info!(
        SCOPE,
        "系统 build={} 圆角={} backdrop={}",
        win11::build_number(),
        effects.rounded,
        effects.backdrop
    );

    let tray_ready = match tray::add(
        host,
        TRAY_ID,
        &format!("{}（右键菜单）", version::banner()),
        ipc::WM_TRAY_CALLBACK,
    ) {
        Ok(()) => {
            // 图标加成功 ≠ 用户能看见：可能被塞进 `^` 溢出区，或被"隐藏的图标"设置关掉。
            // 用 Shell_NotifyIconGetRect 问一次 Shell 它在哪儿，写进日志便于排查。
            match tray::rect(host, TRAY_ID) {
                Some(r) => log_info!(
                    SCOPE,
                    "托盘图标已添加 rect=({},{})-({},{}) 尺寸={}x{}",
                    r.left,
                    r.top,
                    r.right,
                    r.bottom,
                    r.right - r.left,
                    r.bottom - r.top
                ),
                None => log_warn!(
                    SCOPE,
                    "托盘图标已添加但 Shell 报不出位置（可能被折叠进溢出区，或图标未真正注册）"
                ),
            }
            true
        }
        Err(e) => {
            let code = qlr_platform::to_core(e);
            log_error!(SCOPE, "托盘图标创建失败（不影响预览）：{}", code);
            if let Some(level) = integrity::current().filter(|l| l.below_medium()) {
                log_error!(
                    SCOPE,
                    "根因疑似令牌完整性={}：低完整性进程不能向 explorer 加托盘图标（UIPI）。\
                     通常是因为 exe 放在受限目录里（例如 DSH 工作目录：文件被打了 \
                     Low Mandatory Level 标签）。把 qlrust.exe 复制到普通目录再运行。",
                    level.as_str()
                );
            }
            // 托盘失败时程序会静默常驻后台、用户完全找不到入口（也没法退出），
            // 所以必须显式告知 —— 否则用户只会看到"程序好像没起来"。
            //
            // 1.8.3 改：不再单独起线程弹模态 MessageBox。
            // 老弹框的三个副作用（实测都成立）：
            // 1. 那条 `std::thread::spawn` 让进程形态变样（线程 7 → 9），
            //    任何"常驻资源"对比都会被它带偏（上一版 +9 线程 / +46% WS 就是这么来的）；
            // 2. 模态框会盖住屏幕、只能点"确定"，自动化脚本会被它卡住；
            // 3. 框关掉之后用户仍然不知道程序还在跑——信息只活在那一次弹窗里。
            //
            // 现在改成"日志 + 第一次显示窗口时的顶栏 toast"（见 `notify_tray_failure_once`）：
            // 无窗口时也能在日志里查到根因和手工退出方式，第一次预览时再用非模态提示说一次。
            // 仍然只在"用户双击启动"（没有路径、动作是裸 Toggle）时标记，
            // 命令行/自动化调用不打扰（这条约束从 1.0.x 就有，别去掉）。
            let bare_launch = request.path.is_none() && matches!(request.action, Action::Toggle);
            if bare_launch {
                TRAY_FAILURE_NOTICE.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            false
        }
    };

    // 配置马上要被移进 State，热键摘要与绑定表先留一份出来（下面装钩子要用）。
    let hotkeys = config.hotkeys.clone();

    let _ = STATE.set(Mutex::new(State {
        config,
        host: Hwnd::from_raw(host),
        preview: Hwnd::from_raw(preview),
        shadow,
        visible: false,
        last_path: None,
        surface: None,
        request_started: started,
        render_gen: 0,
        page: 0,
        pages: None,
        zoom: 1.0,
        scroll: 0,
        wrapped_consumed: 0,
        natural: None,
        base: None,
        drag_from: None,
        dragged: false,
        drag_residual: 0,
        zoom_anchor: None,
        scroll_anchor: None,
        zoom_preview: false,
        pan: (0, 0),
        styled_logged: false,
        lens: None,
        lens_zoom: LENS_DEFAULT_ZOOM,
        hover_button: None,
        page_edit: None,
        wheel_acc: 0,
        toc_scroll: 0,
        hover_toc: false,
        scroll_drag: false,
        toc: Vec::new(),
        toc_active: None,
        pinned: false,
        resize_edges: None,
        resize_from: None,
        media_aspect: None,
        last_pos: None,
        last_sizes: std::collections::BTreeMap::new(),
        last_show_at: None,
        prewarm_candidate: None,
        render_since: None,
        render_warned: false,
        pending_key: None,
        pending_frame: None,
        shell_preview: None,
        toast: None,
        slide: None,
        shell_input_pending: false,
        hold_frame: false,
        shell_cover: None,
        shell_owns_input: false,
        shell_poll: 0,
        audio: None,
        audio_ticking: false,
        video: None,
        wrap_cache: wrapcache::WrapCache::default(),
    }));
    let _ = LINES.set(Mutex::new(vec!["QLRust".to_string()]));
    let _ = CONTENT.set(Mutex::new(None));
    let _ = RESULT.set(Mutex::new(None));

    // 渲染线程（M2）：一次拉起、长期存活。它拿不到 HWND，只能 PostMessage 通知 UI。
    match spawn_render_worker(host) {
        Ok((tx, epoch)) => {
            let _ = JOB_TX.set(Mutex::new(tx));
            // 通道就位之后再让代号生效（理由见 `spawn_render_worker` 的注释）。
            RENDER_EPOCH.store(epoch, Ordering::Release);
        }
        Err(e) => log_error!(SCOPE, "渲染线程启动失败（只能出占位文字）：{e}"),
    }

    // 预渲染线程（1.1.0）：只做"窗口收着时先把下一个文件画好"，与渲染线程分开。
    match spawn_prewarm_worker() {
        Ok(()) => log_info!(SCOPE, "预渲染线程就绪（档位={}）", {
            with_state(|s| s.config.prewarm.label()).unwrap_or("-")
        }),
        Err(e) => log_warn!(SCOPE, "预渲染线程启动失败（预热不可用）：{e}"),
    }

    // 插件（M6）：懒加载其实也行，但启动时扫一次能把"加载失败"提前暴露在日志里。
    let host_plugins = plugins::host();
    log_info!(
        SCOPE,
        "插件就绪 dir={} loaded={} extensions={} errors={}",
        host_plugins.dir().display(),
        host_plugins.plugins().len(),
        host_plugins.extension_count(),
        host_plugins.errors().len()
    );
    for plugin in host_plugins.plugins() {
        log_info!(
            SCOPE,
            "插件 {} 接管 [{}] 文件={}",
            plugin.name(),
            plugin.extensions().join(" "),
            plugin.file().display()
        );
    }
    for error in host_plugins.errors() {
        log_warn!(SCOPE, "插件加载失败 {error}");
    }

    log_info!(
        SCOPE,
        "壳层就绪 window_created={}ms tray={}",
        started.elapsed().as_millis(),
        tray_ready
    );

    // 跟随 + 预渲染共用同一只 400 ms 计时器（1.1.0 起常驻）：
    // - 窗口开着 → 跟着资源管理器的选中项换内容；
    // - 窗口收着 → 悄悄把当前选中的那个文件先画好，等用户按空格。
    window::start_timer(preview, TIMER_FOLLOW, FOLLOW_INTERVAL_MS);
    // 渲染看门狗（1.2.0）：和跟随计时器分开，因为它要 250 ms 一拍才够灵敏。
    window::start_timer(preview, TIMER_WATCHDOG, WATCHDOG_INTERVAL_MS);

    // 全局空格热键（A1.5）。失败不致命：还可以用 CLI / 托盘触发。
    let mut hotkey_guard = if hotkey_enabled {
        // 滚轮缩放与 Esc 收起都靠钩子线程旁听（预览窗是 WS_EX_NOACTIVATE，永远
        // 拿不到焦点，系统的 WM_MOUSEWHEEL/WM_KEYDOWN 根本不会投给它）。
        hotkey::set_preview(preview);
        hotkey::set_extra_messages(ipc::WM_QLR_ESCAPE, ipc::WM_QLR_WHEEL);
        // 1.0.7：按键全部来自配置（设置页可改），钩子按绑定表匹配。
        hotkey::set_bindings(hotkeys.packed());
        match hotkey::start(host, ipc::WM_QLR_HOTKEY) {
            Ok(guard) => {
                log_info!(
                    SCOPE,
                    "热键就绪 [{}] installed={} elapsed={}ms",
                    hotkeys.summary(),
                    guard.installed(),
                    started.elapsed().as_millis()
                );
                Some(guard)
            }
            Err(e) => {
                log_error!(
                    SCOPE,
                    "热键安装失败（降级为仅 CLI/托盘触发）：{}",
                    qlr_platform::to_core(e)
                );
                None
            }
        }
    } else {
        log_warn!(SCOPE, "配置关闭了热键（hotkey_enabled=false）");
        None
    };

    // 首个请求。
    //
    // 启动语义（用户实测反馈）：**双击启动就应该是纯后台**，不要弹一个空窗口占着前台。
    // 所以只有"明确要看某个文件"（带路径）或显式 `--show` 才在启动时出窗；
    // 裸启动 = 安静驻留后台，靠托盘图标 + 空格热键工作。
    // 1.0.7：`--settings` 也算"启动就要做一件看得见的事"，否则设置页永远打不开。
    let startup_shows =
        request.path.is_some() || matches!(request.action, Action::Show | Action::Settings);
    if startup_shows {
        apply_request(&request);
    } else {
        log_info!(
            SCOPE,
            "启动后保持后台：不显示窗口（选中文件按空格预览，托盘图标右键退出）"
        );
    }

    // 后台状态没有窗口、也没有任务栏按钮，用户很容易"不知道它起没起来"。
    // 起一次气泡提示（失败不致命：提权会话下托盘本身就可能被 UIPI 拒）。
    if tray_ready
        && !startup_shows
        && tray::notify(
            host,
            TRAY_ID,
            &version::banner(),
            "已在后台运行：选中文件按空格预览；右键托盘图标可退出。",
        )
    {
        log_info!(SCOPE, "已弹出后台运行提示（托盘气泡）");
    }

    loop {
        match window::pump() {
            window::Pump::Message(msg) => window::dispatch(&msg),
            window::Pump::Quit => {
                log_info!(SCOPE, "消息泵收到 WM_QUIT");
                break;
            }
            window::Pump::Error => {
                log_error!(SCOPE, "消息泵 GetMessageW 返回错误");
                break;
            }
        }
    }

    // 退出兜底（1.8.3）：消息泵已经退出，下面全是"尽力而为"的清理。清理有可能被
    // 第三方 DLL 吊住——插件是用 LoadLibrary 进来的，进程退出时 Windows 会进它的
    // `DllMain(DLL_PROCESS_DETACH)`；插件在 DllMain 里等锁/等 COM/跑静态析构，
    // 进程就停在"窗口没了、任务管理器里还在"的半死状态（现场报障就是这个现象）。
    // 托盘图标已经摘掉、配置也已经落盘，所以超时直接强制结束自己最干脆。
    arm_exit_watchdog(Duration::from_secs(5));
    if tray_ready {
        tray::remove(host, TRAY_ID);
    }
    // 先摘钩子再退进程：钩子线程还在跑时退进程，Windows 会给调试器记一条"钩子未摘除"。
    if let Some(mut guard) = hotkey_guard.take() {
        let stats = hotkey::stats();
        log_info!(
            SCOPE,
            "热键统计 callbacks={} swallowed={} reinstall={} passed={} wheels={} \
             passed_text_focus={} text_focus_level={}",
            stats.callbacks,
            stats.swallowed,
            stats.reinstalls,
            stats.passed_other_class,
            stats.wheels,
            stats.passed_text_focus,
            stats.text_focus_level
        );
        guard.stop();
    }
    if com_ok {
        selection::uninit_com();
    }
    log_info!(SCOPE, "退出：总运行 {}ms", started.elapsed().as_millis());
    ExitCode::SUCCESS
}

/// 退出看门狗：清理阶段卡住就强制结束进程（调用点见 `run()`）。
///
/// 为什么需要它：`quit()` 只负责把消息泵停掉，之后还要摘热键钩子、卸托盘、
/// 反初始化 COM。这几步都会进第三方代码（钩子线程、插件 DLL 的 DllMain），
/// 一旦对方卡住，进程就永远停在"没有窗口、任务管理器里还在"的状态——用户唯一的
/// 出路是手动结束进程。这里给清理一个硬上限，到点自己走 `TerminateProcess`：
/// 不触发 DLL detach 流程（卡住我们的正是它），也不等任何线程收尾。
pub(crate) fn arm_exit_watchdog(limit: Duration) {
    let spawned = std::thread::Builder::new()
        .name("qlrust-exit-watchdog".into())
        .spawn(move || {
            std::thread::sleep(limit);
            log_error!(
                SCOPE,
                "退出清理超过 {}s 仍未结束（第三方 DLL/钩子线程卡住），强制结束进程",
                limit.as_secs()
            );
            window::terminate_self(1);
            // TerminateProcess 正常情况下当场生效；万一被拒（受保护句柄），
            // 再用 std 的保底退出，至少不会把控制权交给卡住的清理代码。
            std::process::exit(1);
        });
    if spawned.is_err() {
        log_error!(
            SCOPE,
            "退出看门狗线程起不来（内存不足？），清理阶段没有超时保护"
        );
    }
}

pub(crate) fn level_from_str(s: &str) -> Level {
    match s {
        "error" => Level::Error,
        "warn" => Level::Warn,
        "debug" => Level::Debug,
        "trace" => Level::Trace,
        _ => Level::Info,
    }
}

// ---------------------------------------------------------------------------
// 渲染线程（M2）
// ---------------------------------------------------------------------------

/// 把请求转交给已在运行的实例。
pub(crate) fn forward(host: HWND, request: &Request, started: Instant) -> ExitCode {
    match instance::send_request(host, &request.to_bytes()) {
        Ok(()) => {
            log_info!(
                SCOPE,
                "请求已转交：action={:?} path={:?}，耗时 {}ms",
                request.action,
                request.path,
                started.elapsed().as_millis()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            log_error!(SCOPE, "转交请求失败：{}", qlr_platform::to_core(e));
            ExitCode::from(3)
        }
    }
}

///
/// 每次请求进来先 `begin_request()`：SHOW/HIDE 报的延迟是"这一步花了多久"，
/// 而不是"进程活了多久"。`on_hotkey` 走的是另一条入口（真实空格），那里也要调一次，
/// 否则从托盘/热键进来的预览会报出几十秒的假延迟。
pub(crate) fn begin_request() {
    with_state(|s| s.request_started = Instant::now());
}

pub(crate) fn apply_request(request: &Request) {
    log_debug!(
        SCOPE,
        "收到请求 action={:?} path={:?}",
        request.action,
        request.path
    );
    begin_request();
    match request.action {
        Action::Toggle => toggle(request.path.as_deref().map(PathBuf::from)),
        Action::Show => show_preview(request.path.as_deref().map(PathBuf::from)),
        Action::Hide => {
            hide_preview();
        }
        Action::Quit => quit(),
        Action::Ping => log_debug!(SCOPE, "收到 Ping"),
        // 模拟热键：和真实钩子走同一个处理函数，延迟按 0 记（没有经过钩子）。
        // 键码取配置里"打开/关闭预览"的绑定，改键后冒烟测试仍能对准。
        Action::HotkeyProbe => {
            let vk = with_state(|s| {
                s.config
                    .hotkeys
                    .binding(qlr_core::keymap::Action::Toggle)
                    .vk
            })
            .unwrap_or(0x20);
            on_hotkey(vk as usize, window::now_ms())
        }
        Action::Settings => open_settings(),
        Action::PageNext => turn_page(1),
        Action::PagePrev => turn_page(-1),
        Action::ZoomIn => set_zoom(1.25),
        Action::ZoomOut => set_zoom(0.8),
        Action::ZoomReset => zoom_reset(),
        Action::Pan { dx, dy } => pan_by(dx, dy),
        Action::Wheel { delta } => on_wheel(delta),
        // 只给自动化用：钩子会过滤合成按键，脚本按不出真的方向键。
        Action::Scroll { delta } => {
            scroll_text_by(delta as isize);
        }
    }
}

pub(crate) fn toggle(path: Option<PathBuf>) {
    let (visible, last) = with_state(|s| (s.visible, s.last_path.clone())).unwrap_or((false, None));
    // Toggle 的语义（对齐 QuickLook 的空格键行为）：
    // - 带路径的调用 = "我要看这个文件"：同一个文件再按一次才收起，换了文件就直接换成新的；
    // - 不带路径 = 纯开/关。
    // 1.0.10：开了"阻止关闭"之后，空格不能再把它收起来（换成别的文件仍然允许）。
    match path {
        Some(new_path) => {
            if visible && last.as_deref() == Some(new_path.as_path()) {
                if close_blocked("空格") {
                    return;
                }
                hide_preview();
            } else {
                show_preview(Some(new_path));
            }
        }
        None => {
            // 不带路径 = 纯开关，但**只允许"收起"**。
            //
            // 用户实测反馈："打开程序的时候，只应该出现在后台，前台不要留窗口"。
            // 早期版本在裸启动时弹一个空窗口（还会把上次的内容重新翻出来），
            // 双击一下就被窗口占住前台 —— 所以没有路径时一律不新建窗口。
            // 想看说明/占位内容请用托盘菜单或 `--show`，那是显式意图。
            if visible {
                if close_blocked("空格") {
                    return;
                }
                hide_preview();
            } else {
                log_debug!(SCOPE, "裸 Toggle 且窗口未显示：忽略（保持后台）");
            }
        }
    }
}

/// "阻止关闭"开着时拒绝收起窗口，返回 `true` 表示这次收起被挡下了。
///
/// 只挡**含糊意图**（空格、Esc 这类"顺手关掉"）；顶栏的 × 和托盘退出是显式点击，
/// 照样能关——否则用户开了锁就再也关不掉，只能杀进程。
pub(crate) fn close_blocked(how: &str) -> bool {
    let pinned = with_state(|s| s.pinned && s.visible).unwrap_or(false);
    if pinned {
        log_info!(SCOPE, "已开启「阻止关闭」，忽略{how}");
        window::beep();
    }
    pinned
}

pub(crate) fn quit() {
    log_info!(SCOPE, "收到退出请求");
    // 位置记忆（1.0.13）：托盘退出时窗口可能还开着，退出前补一次落盘。
    persist_position();
    window::post_quit();
}

pub(crate) fn pending_requests() -> &'static Mutex<Vec<Request>> {
    PENDING.get_or_init(|| Mutex::new(Vec::new()))
}
/// 处理排队的请求（从消息循环里调用，此时已不在输入同步呼叫里）。
pub(crate) fn drain_pending() {
    loop {
        let next = pending_requests().lock().ok().and_then(|mut q| {
            if q.is_empty() {
                None
            } else {
                Some(q.remove(0))
            }
        });
        match next {
            Some(request) => apply_request(&request),
            None => return,
        }
    }
}

pub(crate) fn on_tray(lparam: LPARAM) {
    // 低 16 位是鼠标消息，高 16 位是图标 id（这里只有一个图标，忽略 id）。
    let mouse_message = (lparam.0 as u32) & 0xFFFF;
    // 双击托盘图标 = 打开说明/占位窗口。
    // 程序常驻后台后没有任何可见入口（tool window 不进任务栏），
    // 双击是最符合直觉的"我要看看这东西"的动作。
    if mouse_message == WM_LBUTTONDBLCLK {
        if with_state(|s| s.visible).unwrap_or(false) {
            hide_preview();
        } else {
            show_preview(None);
        }
        return;
    }
    if mouse_message != WM_RBUTTONUP {
        return;
    }
    let Some(host) = with_state(|s| s.host) else {
        return;
    };
    match tray::show_menu(host.raw()) {
        tray::menu::SETTINGS => open_settings(),
        tray::menu::ABOUT => log_info!(
            SCOPE,
            "{} — GPL-3.0-or-later，配置目录 {:?}",
            version::banner(),
            paths::data_dir()
        ),
        tray::menu::EXIT => quit(),
        _ => {}
    }
}

/// 打开设置窗口（托盘菜单 + `--settings` 共用）。
pub(crate) fn open_settings() {
    let Some((host, config)) = with_state(|s| (s.host, s.config.clone())) else {
        return;
    };
    settings::open(host.raw(), &config);
    log_info!(SCOPE, "设置窗口：已请求打开");
}

/// 设置窗保存完毕：重读配置 → 热更新钩子绑定。
///
/// 为什么不直接让设置窗改钩子：钩子的绑定表是平台层的**进程级全局量**，
/// 而"谁读配置"应该只有一处。设置窗写文件、宿主读文件，两边职责不交叉。
pub(crate) fn on_settings_saved() {
    let path = paths::config_path();
    let (config, err) = Config::load(&path);
    if let Some(e) = err {
        log_warn!(SCOPE, "设置保存后重读配置有问题：{e}");
    }
    hotkey::set_bindings(config.hotkeys.packed());
    with_state(|s| s.config = config.clone());
    log_info!(SCOPE, "配置已热更新：{}", config.summary());
}

pub(crate) unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        // 1.8.3：每条消息先对齐到本窗口的 DPI。放在最前面有两个作用：
        // 1) 绘制前 `dpi::px()` 拿到的是本窗口 DPI（预热线程、设置窗、跨屏拖动都靠它）；
        // 2) `WM_DPICHANGED` 到达时 DPI 已经变了，`enter` 会发现变化并触发字体重建钩子。
        dpi::enter(hwnd);
        log_debug!(SCOPE, "wm=0x{msg:04X}");
        match msg {
            // 去掉系统边框/标题栏，同时保留 DWM 阴影与亚克力。
            WM_NCCALCSIZE if wparam.0 != 0 => LRESULT(0),
            // 返回 1 表示"背景我自己管"，避免擦除时把亚克力刷成实色。
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                repaint(hwnd);
                LRESULT(0)
            }
            WM_COPYDATA => {
                on_copy_data(hwnd, lparam);
                LRESULT(1)
            }
            ipc::WM_QLR_INVOKE => {
                // 先收掉待卸的系统预览处理器（此时已经脱离了 WM_COPYDATA 的同步调用栈）。
                drain_shell_trash();
                drain_pending();
                LRESULT(0)
            }
            ipc::WM_TRAY_CALLBACK => {
                on_tray(lparam);
                LRESULT(0)
            }
            ipc::WM_QLR_HOTKEY => {
                on_hotkey(wparam.0, lparam.0 as u32);
                LRESULT(0)
            }
            ipc::WM_QLR_ESCAPE => {
                // 1.0.10：开了"阻止关闭"之后 Esc 也关不掉（锁是单向的）。
                if close_blocked("Esc") {
                    return LRESULT(0);
                }
                log_debug!(SCOPE, "ESC 收起预览");
                hide_preview();
                LRESULT(0)
            }
            ipc::WM_QLR_WHEEL => {
                // wparam 里是 i16 的滚轮增量（一格 ±120）
                let delta = (wparam.0 as i32) as i16 as i32;
                // 走总入口：文本滚、图片/PDF 缩（1.0.1 前这里直连 wheel_zoom 是错的）。
                on_wheel(delta);
                LRESULT(0)
            }
            ipc::WM_QLR_RENDER => {
                on_render_done();
                LRESULT(0)
            }
            ipc::WM_QLR_SETTINGS_SAVED => {
                on_settings_saved();
                LRESULT(0)
            }
            ipc::WM_QLR_FOLLOW => {
                // 跟随线程发现选中项变了（1.7.6）：立刻切，不等计时器。
                on_follow_wake();
                LRESULT(0)
            }
            ipc::WM_QLR_SHELLFIT => {
                // 窗口尺寸稳下来了：把系统预览处理器的内容子窗口对齐到新尺寸（1.7.9）。
                fit_shell_to_window();
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                // 1) 先看是不是按在窗口边缘（拖边改大小，1.0.10 加回来）。
                let (x, y) = client_xy(lparam);
                let hit_rect = window::client_rect(hwnd);
                let edges = edges_at(hit_rect, x, y, resize_margin(hwnd));
                // 1.7.4 起埋点：`按下 (x, y) 客户区 WxH 边=?`——"拖边不生效"这类反馈
                // 全看这一条就能分清是"点没到边缘"还是"点根本没进来"。
                log_debug!(
                    SCOPE,
                    "按下 ({}, {}) 客户区 {}x{} 边={}",
                    x,
                    y,
                    hit_rect.right - hit_rect.left,
                    hit_rect.bottom - hit_rect.top,
                    if edges.any() {
                        edges.describe()
                    } else {
                        "无".to_string()
                    }
                );
                if edges.any() {
                    begin_resize(hwnd, edges, lparam);
                    return LRESULT(0);
                }
                // 2) 顶栏 / 目录 / 滚动条（1.0.9）：这些是"窗口的家具"。
                if on_chrome_click(hwnd, lparam) {
                    LRESULT(0)
                } else if archive_click(hwnd, lparam) {
                    // 3) 压缩包行（1.7.3）：选中 + 目录折叠，不吃掉的话会变成拖窗口。
                    LRESULT(0)
                } else {
                    on_drag_start(hwnd, lparam);
                    LRESULT(0)
                }
            }
            WM_MOUSEMOVE => {
                // 悬停只影响顶栏按钮与目录高亮，先更新再走手势分派。
                update_hover(hwnd, lparam);
                on_mouse_move(lparam);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                if with_state(|s| std::mem::replace(&mut s.scroll_drag, false)).unwrap_or(false) {
                    window::release_capture();
                }
                // 拖边结束要按新尺寸重画（位图得重新光栅化）。
                if let Some(edges) = with_state(|s| s.resize_edges.take()).flatten() {
                    with_state(|s| s.resize_from = None);
                    finish_resize(edges);
                    return LRESULT(0);
                }
                on_drag_end();
                LRESULT(0)
            }
            WM_RBUTTONDOWN => {
                on_zoom_drag_start(hwnd, lparam);
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                on_zoom_drag_end();
                LRESULT(0)
            }
            WM_CLOSE => {
                hide_preview();
                LRESULT(0)
            }
            // 跟随选中项（1.0.10）：只有我们自己的定时器才处理。
            WM_TIMER if wparam.0 == TIMER_FOLLOW => {
                on_follow_timer();
                LRESULT(0)
            }
            // 渲染看门狗（1.2.0）：只记日志 + 请求取消，绝不阻塞 UI。
            WM_TIMER if wparam.0 == TIMER_WATCHDOG => {
                on_render_watchdog();
                LRESULT(0)
            }
            // 音频进度条（1.7.9）：只重画进度条那一条，整窗不动。
            WM_TIMER if wparam.0 == TIMER_AUDIO => {
                let Some((pos, dur)) = audio_progress() else {
                    window::stop_timer(hwnd, TIMER_AUDIO);
                    with_state(|s| s.audio_ticking = false);
                    return LRESULT(0);
                };
                // 播完了（MCI 停在末尾不再前进）：收掉计时器、给一句提示、进度条复位到 0。
                if dur > 0 && pos + AUDIO_TICK_MS as u64 >= dur {
                    window::stop_timer(hwnd, TIMER_AUDIO);
                    with_state(|s| s.audio_ticking = false);
                    let _ = with_state(|s| s.audio.as_ref().map(|p| p.seek(0)));
                    let content = window::chrome_rects(window::client_rect(hwnd)).content;
                    window::paint_audio_bar(hwnd, content, 0, dur, false);
                    show_toast(hwnd, "播放结束");
                    log_debug!(SCOPE, "AUDIO 播放结束（{}ms）", dur);
                    return LRESULT(0);
                }
                paint_audio_now(hwnd);
                LRESULT(0)
            }
            // 视频逐帧播放（1.7.9）：解一帧贴一帧，落后就跳帧。
            WM_TIMER if wparam.0 == TIMER_VIDEO => {
                video_tick(hwnd);
                LRESULT(0)
            }
            // 顶栏提示到期（1.7.4）：擦掉那一句话，并把顶栏重画一次。
            WM_TIMER if wparam.0 == TIMER_TOAST => {
                with_state(|s| s.toast = None);
                window::stop_timer(hwnd, TIMER_TOAST);
                window::invalidate(hwnd);
                LRESULT(0)
            }
            // 接管处理器输入（1.7.4）：等首帧画完再关，给它留出 DoPreview 的时间。
            WM_TIMER if wparam.0 == TIMER_SHELL_INPUT => {
                window::stop_timer(hwnd, TIMER_SHELL_INPUT);
                // 轮询"内容真的画出来了吗"（1.7.7）。判据 = 内容区里最大那块可见后代窗口
                // 占到 50% 以上；到点没收尾就再等 120 ms，最多 3 s（`SHELL_POLL_MAX`）。
                let content = window::chrome_rects(window::client_rect(hwnd)).content;
                let (ratio, has_preview) = with_state(|s| {
                    (
                        s.shell_preview
                            .as_ref()
                            .map(|handle| handle.visible_area(content))
                            .unwrap_or(0),
                        s.shell_preview.is_some(),
                    )
                })
                .unwrap_or((0, false));
                let poll = with_state(|s| {
                    s.shell_poll = s.shell_poll.saturating_add(1);
                    s.shell_poll
                })
                .unwrap_or(0);
                let ready = ratio >= 50 || !has_preview || poll >= SHELL_POLL_MAX;
                if !ready {
                    window::start_timer(hwnd, TIMER_SHELL_INPUT, SHELL_POLL_MS);
                    return LRESULT(0);
                }
                log_debug!(SCOPE, "SHELL 就绪：内容占 {ratio}%（轮询 {poll} 次）");
                // 只有 PPT 接管输入（翻页由我们计数）；Excel/Word 必须让处理器自己收鼠标——
                // 禁用宿主 `EnableWindow(FALSE)` 会连子窗口一起禁掉，用户点不到单元格、
                // 拖不了滚动条（1.8.1 用户反馈"Excel 焦点进去后左键选不中单元格"）。
                if with_state(|s| s.shell_input_pending).unwrap_or(false) {
                    let owns = with_state(|s| s.shell_owns_input).unwrap_or(false);
                    if owns {
                        with_state(|s| {
                            if let Some(handle) = s.shell_preview.as_mut() {
                                handle.take_over_input();
                            }
                        });
                    }
                    with_state(|s| s.shell_input_pending = false);
                }
                // 一打开就把"UI 激活"做完（1.8.1）：`IPreviewHandler::SetFocus` 在
                // `WM_ACTIVATE`/`WM_SETFOCUS` 里调会被拒（`0x8001010D` 输入同步呼叫），
                // 只有从定时器这种"退栈后"的地方才调得通。做完这一步，用户**第一次**点
                // 单元格就能选中，不必先点一下"激活"。只有非 PPT 走这条（PPT 由我们接管翻页）。
                if !with_state(|s| s.shell_owns_input).unwrap_or(false) {
                    focus_shell_content();
                }
                // 冻帧解除（1.7.6）：内容已经画出来，可以整窗重画。
                if with_state(|s| s.hold_frame).unwrap_or(false) {
                    with_state(|s| s.hold_frame = false);
                    window::invalidate(hwnd);
                }
                LRESULT(0)
            }
            // 换帧（1.7.6）：到点把"盖住"的旧处理器卸掉，露出新处理器的首帧。
            WM_TIMER if wparam.0 == TIMER_SHELL_SWAP => {
                window::stop_timer(hwnd, TIMER_SHELL_SWAP);
                // 换帧不再"死等 260 ms"（1.7.8）：Excel 的 `DoPreview` 要 768 ms，
                // 掀开得太早会露出还没画完的白/黑底（用户说的"Office 之间切换有黑边"）。
                // 判据同 `TIMER_SHELL_INPUT`：新处理器内容占到 50% 才掀开，最多等 1.8 s。
                let content = window::chrome_rects(window::client_rect(hwnd)).content;
                let (ratio, has_preview) = with_state(|s| {
                    (
                        s.shell_preview
                            .as_ref()
                            .map(|handle| handle.visible_area(content))
                            .unwrap_or(0),
                        s.shell_preview.is_some(),
                    )
                })
                .unwrap_or((0, false));
                let waited = with_state(|s| {
                    s.shell_poll = s.shell_poll.saturating_add(1);
                    s.shell_poll
                })
                .unwrap_or(0);
                if has_preview && ratio < 50 && waited < 15 {
                    window::start_timer(hwnd, TIMER_SHELL_SWAP, SHELL_POLL_MS);
                    return LRESULT(0);
                }
                let cover = with_state(|s| s.shell_cover.take()).flatten();
                if cover.is_some() {
                    retire_shell_preview(cover);
                    with_state(|s| s.hold_frame = false);
                    window::invalidate(hwnd);
                    log_debug!(SCOPE, "换帧：卸掉遮盖的旧内容（新内容占 {ratio}%）");
                }
                LRESULT(0)
            }
            // 光标形状（1.0.10）：无边框窗口 + 边缘拖拽，系统完全不知道"这里能拖"，
            // 必须自己换指针，否则用户看不出边可以抓。
            WM_SETCURSOR => {
                let over = window::cursor_pos()
                    .map(|(cx, cy)| {
                        let rc = window::window_rect(hwnd);
                        edges_at(rc, cx - rc.left, cy - rc.top, resize_margin(hwnd))
                    })
                    .unwrap_or_default();
                let dragging = with_state(|s| s.resize_edges).flatten().unwrap_or_default();
                let kind = if dragging.any() {
                    dragging.cursor()
                } else {
                    over.cursor()
                };
                if window::set_cursor(kind) {
                    LRESULT(1)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            WM_DESTROY => {
                window::post_quit();
                LRESULT(0)
            }
            // 窗口尺寸变了：**投递**一条消息，等这条同步呼叫退栈后再对齐系统预览处理器（1.7.9）。
            //
            // 症状：把 Office 预览窗拉宽，右边/下边留一条白（用户反馈「窗口伸缩会引起白边」）。
            // 原因：内容其实是处理器自己建的子窗口，不会跟着我们的窗口变（窗口树
            // `QLRust.ChildHost → EXCEL9 → EXCEL7`，宿主撑大了，`EXCEL7` 还是旧尺寸）。
            // 为什么不能在这里直接干：`SetWindowPos` 派发的 `WM_SIZE` 在输入同步呼叫里，
            // 那时进 COM 会被系统拒（0x8001010D），见 `ipc::WM_QLR_SHELLFIT` 的注释。
            // 拖边过程中不同步：每像素一次重开太慢，`finish_resize` 里再投一次。
            WM_SIZE => {
                if with_state(|s| s.resize_edges.is_none()).unwrap_or(true) {
                    window::post(hwnd, ipc::WM_QLR_SHELLFIT);
                }
                // 投影是另一个分层窗口，尺寸变了必须自己跟一次：不然改完大小投影还停在上一个
                // 尺寸上（看起来像"画框和阴影对不上"，用户 1.8.0 报）。拖动中限流到 8 ms。
                sync_shadow_with(true);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            // 点窗口就是要"激活它"（1.7.8）。
            //
            // 背景：Word/Excel 的预览是 OLE 就地激活对象，**容器不活、它就不收滚轮**
            // （用户实测：点一下预览窗再滚就能滚，且要求"点一下就立刻把焦点放进去"）。
            // 这里返回 MA_ACTIVATE(1) 让窗口正常激活，并把焦点交给处理器的内容子窗口，
            // 这样滚轮/滚动条/选区立刻可用，不用用户再点一次。
            //
            // 1.7.9 修：原来返回的是 2 = `MA_ACTIVATEANDEAT`（激活**并吞掉**这次点击）——
            // 于是"未激活的窗口上的第一次点击"永远到不了 `WM_LBUTTONDOWN`，拖边改大小
            // 只有第二次才生效（用户报的「拖边失效」）。
            WM_MOUSEACTIVATE => {
                // 这里**不**把焦点转交处理器（1.8.1）：`IPreviewHandler::SetFocus` 是 COM 调用，
                // 会把消息泵回我们自己，正好把"这次点击"吃掉——用户要连点两下才能选中单元格。
                // 焦点改在后面的 `WM_ACTIVATE` / `WM_SETFOCUS` 里给（那两条一定在点击落地前到）。
                LRESULT(1) // MA_ACTIVATE
            }
            // 点预览窗会先激活它、再给它焦点（1.7.8）：这两处都要把焦点转交给处理器的
            // 内容子窗口，否则 Excel/Word 的滚轮仍然不动（实测：`WM_MOUSEACTIVATE` 有时压根不来，
            // 但 `WM_ACTIVATE` / `WM_SETFOCUS` 一定会来）。
            WM_ACTIVATE => {
                if (wparam.0 & 0xFFFF) != 0 {
                    focus_shell_content();
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_SETFOCUS => {
                focus_shell_content();
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
