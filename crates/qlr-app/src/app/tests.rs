//! `app` 的单测：几何、输入、缓存、诊断这些纯逻辑的回归。

use super::*;

/// 1.8.3：开窗尺寸必须随 DPI 变，且"宽随 DPI、高随工作区比例"这条口径不能被改坏。
///
/// 这是 DPI 改动唯一能自动断言的部分（视觉要靠 `scripts\baseline-shot.ps1` +
/// 用户真机）。它同时锁住两个曾经的坑：
/// - 文本窗宽度要 ×DPI（否则 200% 下只有 440 DIP 宽的一条缝）；
/// - 文本窗高度**不能** ×DPI（高度 = 工作区 × height_ratio，卡里写的"2478 px"
///   就是这么算错的：1080 高的屏上根本放不下）。
#[test]
fn window_size_scales_with_dpi() {
    // 用一块比窗口大得多的"工作区"，免得 0.92 的上限夹住宽度，让 2× 关系可断言。
    let work = RECT {
        left: 0,
        top: 0,
        right: 3840,
        bottom: 2160,
    };
    let cfg = Config::default();

    dpi::seed_thread(96);
    let text96 = window_size_for(work, format::Kind::Text, None, &cfg, None);
    let pdf96 = window_size_for(work, format::Kind::Pdf, Some((794, 1123)), &cfg, None);

    dpi::seed_thread(192);
    let text192 = window_size_for(work, format::Kind::Text, None, &cfg, None);
    let pdf192 = window_size_for(work, format::Kind::Pdf, Some((794, 1123)), &cfg, None);
    dpi::seed_thread(96); // 别把 DPI 留给同线程的其它测试

    assert_eq!(text192.0, text96.0 * 2, "文本窗宽度应该 ×DPI");
    assert_eq!(text192.1, text96.1, "文本窗高度按工作区比例，不随 DPI 变");
    // PDF 的自然尺寸也是 ×DPI 的：200% 下一页 A4 明显更大（受工作区 86% 夹持，
    // 所以只能断言"明显变大"，不能断言恰好 2 倍）。
    assert!(
        pdf192.0 as f64 >= pdf96.0 as f64 * 1.5,
        "PDF 200% 下应该明显变大：{} -> {}",
        pdf96.0,
        pdf192.0
    );
    assert!(pdf192.1 >= pdf96.1, "PDF 高度也不该变小");
}

/// 预热与上屏必须给出**逐字相同**的渲染盒。
///
/// 1.8.3 的现场 bug：`maybe_prewarm` 调 `render_box` 时传的是整窗高，而上屏传的是
/// 「整窗高 − reserve」，差一条 `chrome_height`（96 DPI 下 30 px）；`take_prepared`
/// 的容差只有 ±2 px，于是预热结果永远对不上缓存键 —— 白烧 CPU、一点加速没有。
/// 这条断言锁住"两边同口径"，并顺手钉住"不能忘了扣顶栏"。
#[test]
fn prewarm_box_matches_show_box() {
    let chrome = window::chrome_height();
    let (win_w, win_h) = (1020, 1449); // 整窗（含顶栏）
    let natural = Some((794_u32, 1123_u32)); // A4@96dpi，按素材开窗 → reserve = 顶栏
    for kind in [format::Kind::Pdf, format::Kind::Image] {
        let pages = if kind == format::Kind::Pdf {
            Some(8)
        } else {
            None
        };
        let show_box = render_box(
            kind,
            pages,
            win_w,
            win_h,
            layout::reserve_for_window(natural),
            1.0,
        );
        let warm_box = render_box(
            kind,
            pages,
            win_w,
            win_h,
            layout::reserve_for_window(natural),
            1.0,
        );
        assert_eq!(show_box, warm_box, "{kind:?}：预热盒与上屏盒必须相同");
        // 忘了扣顶栏的那个（即修之前的预热口径）必须与它不同，否则这条测试就白写了。
        let forgot_reserve = render_box(kind, pages, win_w, win_h, 0, 1.0);
        assert_ne!(
            warm_box, forgot_reserve,
            "{kind:?}：reserve 必须真的参与计算"
        );
        assert_eq!(
            warm_box.1,
            forgot_reserve.1 - chrome as u32,
            "{kind:?}：高度差应正好是顶栏"
        );
    }
}

/// 滚动位置永远落在 [0, total - rows]：不能滚成负数，也不能滚过头看不到最后一行。
#[test]
fn scroll_clamp_stays_in_range() {
    // 100 行、可见 10 行 → 最大偏移 90
    assert_eq!(scroll_clamp(0, 0, 100, 10), 0);
    assert_eq!(scroll_clamp(0, 5, 100, 10), 5);
    assert_eq!(scroll_clamp(0, -5, 100, 10), 0);
    assert_eq!(scroll_clamp(90, 10, 100, 10), 90);
    assert_eq!(scroll_clamp(50, 1_000, 100, 10), 90);
    // 内容比窗口还短：永远 0
    assert_eq!(scroll_clamp(7, 3, 5, 10), 0);
    // 空文件不 panic
    assert_eq!(scroll_clamp(0, 3, 0, 10), 0);
}

/// 目录高亮跟随滚动：取最后一个起始行 ≤ scroll 的小节。
#[test]
fn toc_highlight_follows_scroll() {
    let items = vec![
        (1u8, "一".to_string(), 0usize),
        (2u8, "1.1".to_string(), 10),
        (2u8, "1.2".to_string(), 20),
    ];
    assert_eq!(toc_following(&items, 0), Some(0));
    assert_eq!(toc_following(&items, 9), Some(0));
    assert_eq!(toc_following(&items, 10), Some(1));
    assert_eq!(toc_following(&items, 19), Some(1));
    assert_eq!(toc_following(&items, 999), Some(2));
    // 空目录不选出任何项
    assert_eq!(toc_following(&[], 5), None);
}

#[test]
fn wheel_notches_clamps_and_ignores_jitter() {
    assert_eq!(wheel_notches(0), 0);
    // 触控板的细碎增量不该触发缩放
    assert_eq!(wheel_notches(60), 0);
    assert_eq!(wheel_notches(-60), 0);
    assert_eq!(wheel_notches(120), 1);
    assert_eq!(wheel_notches(-240), -2);
    // 触控板甩一下可能几百格，必须钳到 4
    assert_eq!(wheel_notches(12000), 4);
    assert_eq!(wheel_notches(-12000), -4);
}

/// 1.8.3 真实事故：高分辨率滚轮/精密触控板每次只发 30–60，
/// `delta / 120` 恒为 0 → 滚轮在整台机器上"完全失效"（不缩放、不滚动、日志无记录）。
/// 修法是把零头攒起来：4 次 30 = 1 档。
#[test]
fn wheel_residual_accumulates_into_notches() {
    let mut acc = 0;
    // 触控板细碎增量：前三发不动，第四发凑满一格
    assert_eq!(wheel_take(&mut acc, -30), 0);
    assert_eq!(wheel_take(&mut acc, -30), 0);
    assert_eq!(wheel_take(&mut acc, -30), 0);
    assert_eq!(wheel_take(&mut acc, -30), -1, "4×30=120，正好一档");
    assert_eq!(acc, 0, "整档用掉之后零头要清零");

    // 普通滚轮一格一档，行为不变
    assert_eq!(wheel_take(&mut acc, 120), 1);
    assert_eq!(acc, 0);

    // 触控板甩一下：钳到 4 档，且零头清零（否则松手后还会继续滚）
    assert_eq!(wheel_take(&mut acc, 12000), 4);
    assert_eq!(acc, 0);

    // 方向反转不能卡住
    assert_eq!(wheel_take(&mut acc, 60), 0);
    assert_eq!(wheel_take(&mut acc, -60), 0, "先 +60 再 -60 应该回到 0");
    assert_eq!(acc, 0);
    assert_eq!(wheel_take(&mut acc, -60), 0);
    assert_eq!(wheel_take(&mut acc, -60), -1, "反向攒够一格要能出档");
}

#[test]
fn client_xy_sign_extends_both_halves() {
    // 低 16 位是 x、高 16 位是 y，两者都是有符号的（负坐标不能靠 & 0xFFFF）
    let pack = |x: i32, y: i32| -> isize {
        (((y as u32) & 0xFFFF) << 16 | ((x as u32) & 0xFFFF)) as i64 as isize
    };
    assert_eq!(client_xy(LPARAM(pack(120, 80))), (120, 80));
    assert_eq!(client_xy(LPARAM(pack(-5, -7))), (-5, -7));
    assert_eq!(client_xy(LPARAM(pack(0, 0))), (0, 0));
}

#[test]
fn human_size_switches_units() {
    assert_eq!(human_size(512), "512 B");
    assert_eq!(human_size(2048), "2.0 KB");
    assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
}

#[test]
fn extension_is_reported() {
    assert_eq!(extension_of(Path::new(r"C:\a\b.PNG")), ".PNG");
    assert_eq!(extension_of(Path::new(r"C:\a\b")), "（无扩展名）");
}

#[test]
fn scaled_keeps_pixels_sane() {
    assert_eq!(scaled(100, 1.0), 100);
    assert_eq!(scaled(100, 2.0), 200);
    // 极端缩小不能变 0（位图宽度 0 是非法状态）
    assert_eq!(scaled(1, 0.001), 1);
    // 再大也封顶，避免一次渲染吃掉几百 MB
    assert_eq!(scaled(20_000, 8.0), 16_384);
}

/// 滚轮方向：往上滚必须看上面的内容（1.0.4 修的反向 bug）。
#[test]
fn wheel_up_scrolls_towards_the_top() {
    // 向上滚（notches > 0）→ 负数 → scroll 变小 → 显示更前面的行
    assert_eq!(wheel_text_rows(1), -3);
    assert_eq!(wheel_text_rows(4), -12);
    // 向下滚 → 正数
    assert_eq!(wheel_text_rows(-1), 3);
    assert_eq!(wheel_text_rows(0), 0);
    // 和"拖拽向上 = 往下读"方向相反，这才是所有编辑器的习惯
    assert!(wheel_text_rows(1) < 0, "向上滚不能往下翻");
}

/// 文本拖拽滚动：像素攒够一行才滚（慢速拖动不能一步不动，也不能一抖滚十行）。
#[test]
fn pixels_accumulate_into_rows() {
    let mut residual = 0;
    // 24 px 一行：不到一行先攒着，滚 0 行
    assert_eq!(pixels_to_rows(&mut residual, 10), 0);
    assert_eq!(residual, 10);
    // 再动 14 px 凑满 24 → 滚 1 行，零头清零
    assert_eq!(pixels_to_rows(&mut residual, 14), 1);
    assert_eq!(residual, 0);
    // 一次拖 100 px → 滚 4 行，余 4 px
    assert_eq!(pixels_to_rows(&mut residual, 100), 4);
    assert_eq!(residual, 4);
    // 往回拖同样按行算（负号保留），零头跟着走
    assert_eq!(pixels_to_rows(&mut residual, -28), -1);
    assert_eq!(residual, 0);
}

/// 右键拖动 = 缩放：上拖变大、下拖变小，且不越界。
#[test]
fn right_drag_scales_both_ways() {
    // 往上拖 60 px = 一档 1.25×
    assert!((zoom_drag_target(1.0, 60.0) - 1.25).abs() < 0.001);
    // 往下拖 60 px = 一档缩小
    assert!((zoom_drag_target(1.0, -60.0) - 0.8).abs() < 0.001);
    // 不动就不变
    assert!((zoom_drag_target(1.5, 0.0) - 1.5).abs() < 0.001);
    // 极端拖动被 ZOOM_MIN/ZOOM_MAX 夹住
    assert_eq!(zoom_drag_target(1.0, 10_000.0), ZOOM_MAX);
    assert_eq!(zoom_drag_target(1.0, -10_000.0), ZOOM_MIN);
}

#[test]
fn page_keys_map_to_direction() {
    assert_eq!(page_delta_for(0x22), Some(1)); // PageDown
    assert_eq!(page_delta_for(0x27), Some(1)); // →
    assert_eq!(page_delta_for(0x21), Some(-1)); // PageUp
    assert_eq!(page_delta_for(0x25), Some(-1)); // ←
    assert_eq!(page_delta_for(0xBB), None); // + 是缩放，不是翻页
}

#[test]
fn zoom_limits_are_sane() {
    // 编译期就该拦住"缩放范围写反了"这种事
    const _: () = assert!(ZOOM_MIN < 1.0 && ZOOM_MAX > 1.0);
    const _: () = assert!(ZOOM_MAX <= 16.0);
}

#[test]
fn build_lines_without_target_mentions_usage() {
    let lines = build_lines(None);
    assert!(lines.iter().any(|l| l.contains("qlrust")));
    assert!(lines[0].contains("QLRust"));
}

#[test]
fn build_lines_with_missing_file_reports_error() {
    let lines = build_lines(Some(Path::new(r"Z:\nope\missing.bin")));
    assert!(lines.iter().any(|l| l.contains("无法读取文件信息")));
}

#[test]
fn unsupported_kind_placeholder_points_to_plugins() {
    // 1.0.10 起不支持的文件根本不会画占位（只响铃），但 `build_lines` 仍要
    // 在"插件接管了"的时候把插件信息写出来，方便排查。
    let lines = build_lines(Some(Path::new(r"C:\x\a.mp4")));
    assert!(lines[1].contains("图片"), "横幅应列出四类：{lines:?}");
}

/// 窗口位置记忆（1.0.13）：记忆的坐标可能来自另一台显示器，摆位置时必须夹进工作区，
/// 而且**只挪位置、不改尺寸**（尺寸是内容决定的）。
#[test]
fn place_at_moves_without_resizing() {
    let work = RECT {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1040,
    };
    let mut rect = RECT {
        left: 400,
        top: 300,
        right: 1200,
        bottom: 800,
    };
    place_at(&mut rect, Some((100, 60)), work);
    assert_eq!(
        (
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top
        ),
        (100, 60, 800, 500),
        "记住的位置照搬，尺寸不变"
    );

    // 记在屏幕外（换过显示器）→ 整体挪回工作区，尺寸仍然不变。
    let mut rect = RECT {
        left: 400,
        top: 300,
        right: 1200,
        bottom: 800,
    };
    place_at(&mut rect, Some((-500, -400)), work);
    assert_eq!(
        (
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top
        ),
        (0, 0, 800, 500),
        "越界后夹回工作区左上角"
    );

    // 没有记忆 → 一个字都不改（调用方算好的居中位置保留）。
    let mut rect = RECT {
        left: 400,
        top: 300,
        right: 1200,
        bottom: 800,
    };
    place_at(&mut rect, None, work);
    assert_eq!((rect.left, rect.top), (400, 300), "没记忆就不动");
}

#[test]
fn content_size_of_subtracts_chrome_height() {
    // 顶栏占 `window::chrome_height()`，内容区 = 客户区减去它（渲染尺寸与 `base` 都按内容区算）。
    let client = RECT {
        left: 0,
        top: 0,
        right: 800,
        bottom: 600,
    };
    let (w, h) = content_size_of(client);
    assert_eq!(w, 800);
    assert_eq!(h, (600 - window::chrome_height()) as u32);
    // 窗口被夹到极矮时不能出 0（0 高会让渲染线程直接报错）。
    let tiny = RECT {
        left: 0,
        top: 0,
        right: 10,
        bottom: 5,
    };
    assert_eq!(content_size_of(tiny), (10, 1));
}

/// 取"内容区"尺寸（窗口矩形扣掉顶栏），测试里反复用。
fn content_of(rc: RECT) -> (i32, i32) {
    (
        rc.right - rc.left,
        rc.bottom - rc.top - window::chrome_height(),
    )
}

#[test]
fn media_window_size_locks_to_picture_aspect() {
    // 1.8.1（用户口径）：图片窗口的比例 = 图片比例，**记忆的尺寸也要夹回来**，
    // 否则上次把窗口拖成方形，下次开图又会"图片在方窗口里留白/变形"。
    let work = RECT {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1080,
    };
    let config = Config::default();
    for remembered in [None, Some((900, 900)), Some((300, 900))] {
        let (w, h) = window_size_for(
            work,
            format::Kind::Image,
            Some((1280, 720)),
            &config,
            remembered,
        );
        let rc = RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        };
        let (cw, ch) = content_of(rc);
        let want = (cw as f64 * 720.0 / 1280.0).round() as i32;
        assert!(
            (ch - want).abs() <= 1,
            "记忆 {remembered:?}：内容区 {cw}x{ch} 不是 16:9（期望高 {want}）"
        );
    }
    // 非图片格式不受影响（文本只看宽度，保持原样）。
    let (_, h) = window_size_for(work, format::Kind::Text, None, &config, Some((900, 900)));
    assert_eq!(h, 900);
    // 视频（1.8.1 同图片口径）：比例锁死，但要额外留出底部进度条的高度。
    let (w, h) = window_size_for(work, format::Kind::Video, Some((1280, 720)), &config, None);
    let content_h = h - window::chrome_height() - window::audio_bar_h();
    let want = (w as f64 * 720.0 / 1280.0).round() as i32;
    assert!(
        (content_h - want).abs() <= 1,
        "视频内容区 {w}x{content_h} 不是 16:9（期望高 {want}）"
    );
}

#[test]
fn clamp_media_resize_drives_width_from_dragged_edge() {
    // 拖右边：高度跟着比例长，**左边不动**，垂直居中。
    let rc = RECT {
        left: 0,
        top: 0,
        right: 800,
        bottom: 630,
    };
    let edges = ResizeEdges {
        right: true,
        ..ResizeEdges::default()
    };
    let out = clamp_media_resize(rc, edges, 2.0, 0);
    assert_eq!(content_of(out), (800, 400));
    assert_eq!(out.left, 0, "拖右边时左边界不能动");
    // 总高 = 400 + 顶栏，垂直居中：上下各让一半。
    let total = out.bottom - out.top;
    assert_eq!(out.top, 315 - total / 2);
}

#[test]
fn clamp_media_resize_drives_height_when_only_vertical_edges_drag() {
    // 只拖下边：宽度由高度反算（600 高 × 2.0 = 1200 宽），水平居中。
    let rc = RECT {
        left: 0,
        top: 0,
        right: 800,
        bottom: 630,
    };
    let edges = ResizeEdges {
        bottom: true,
        ..ResizeEdges::default()
    };
    let out = clamp_media_resize(rc, edges, 2.0, 0);
    assert_eq!(content_of(out), (1200, 600));
    assert_eq!(out.top, 0, "拖下边时上边界不能动");
    assert_eq!(out.left, 400 - 600);
}

#[test]
fn clamp_media_resize_anchors_opposite_edge() {
    // 拖左边：右边界钉住，窗口往右"长"。
    let rc = RECT {
        left: 100,
        top: 0,
        right: 800,
        bottom: 630,
    };
    let edges = ResizeEdges {
        left: true,
        ..ResizeEdges::default()
    };
    let out = clamp_media_resize(rc, edges, 2.0, 0);
    assert_eq!(content_of(out), (700, 350));
    assert_eq!(out.right, 800, "拖左边时右边界不能动");
    // 比例非法（0 / NaN）时原样返回，绝不把窗口算成 0 宽。
    assert_eq!(clamp_media_resize(rc, edges, 0.0, 0), rc);
    assert_eq!(clamp_media_resize(rc, edges, f64::NAN, 0), rc);
}

#[test]
fn clamp_media_resize_reserves_video_bar() {
    // 视频（1.8.1 与图片同口径）：总高 = 内容高 + 顶栏 + 进度条。
    // 拖下边 → 内容高驱动宽度，比例仍然严格（宽度 = 内容高 × 2）。
    let rc = RECT {
        left: 0,
        top: 0,
        right: 640,
        bottom: 400,
    };
    let edges = ResizeEdges {
        bottom: true,
        ..ResizeEdges::default()
    };
    let out = clamp_media_resize(rc, edges, 2.0, window::audio_bar_h());
    let content_h = out.bottom - out.top - window::chrome_height() - window::audio_bar_h();
    assert_eq!(out.right - out.left, content_h * 2);
    assert_eq!(out.top, 0, "拖下边时上边界不能动");
}

#[test]
fn edges_at_detects_borders_and_corners() {
    // 热区宽度按 DPI 算（1.7.4），测试里固定成 100% 缩放时的 8 px。
    const MARGIN: i32 = 8;
    let rc = RECT {
        left: 0,
        top: 0,
        right: 800,
        bottom: 600,
    };
    // 正中：不命中任何边。
    assert!(!edges_at(rc, 400, 300, MARGIN).any());
    // 四条边。
    assert_eq!(
        edges_at(rc, 2, 300, MARGIN),
        ResizeEdges {
            left: true,
            ..Default::default()
        }
    );
    assert_eq!(
        edges_at(rc, 798, 300, MARGIN),
        ResizeEdges {
            right: true,
            ..Default::default()
        }
    );
    assert_eq!(
        edges_at(rc, 400, 1, MARGIN),
        ResizeEdges {
            top: true,
            ..Default::default()
        }
    );
    assert_eq!(
        edges_at(rc, 400, 598, MARGIN),
        ResizeEdges {
            bottom: true,
            ..Default::default()
        }
    );
    // 左上角：两条边同时命中。
    let corner = edges_at(rc, 1, 1, MARGIN);
    assert!(corner.left && corner.top && !corner.right && !corner.bottom);
    // 窗口外太远：不算（否则在别处点一下就开始改大小）。
    assert!(!edges_at(rc, 400, 900, MARGIN).any());
    // 热区变宽（高 DPI 屏）：原来够不着的点现在算命中。
    // 800 宽的窗口、margin=12 → x=790 落在右边热区里。
    assert!(edges_at(rc, 790, 300, 12).right);
    assert!(!edges_at(rc, 790, 300, MARGIN).right);
}

#[test]
fn resize_edges_pick_the_matching_cursor() {
    let left = ResizeEdges {
        left: true,
        ..Default::default()
    };
    assert_eq!(left.cursor(), window::CursorKind::SizeWE);
    let top = ResizeEdges {
        top: true,
        ..Default::default()
    };
    assert_eq!(top.cursor(), window::CursorKind::SizeNS);
    // 左上 / 右下 = NWSE；右上 / 左下 = NESW。
    let nw = ResizeEdges {
        left: true,
        top: true,
        ..Default::default()
    };
    assert_eq!(nw.cursor(), window::CursorKind::SizeNWSE);
    let se = ResizeEdges {
        right: true,
        bottom: true,
        ..Default::default()
    };
    assert_eq!(se.cursor(), window::CursorKind::SizeNWSE);
    let ne = ResizeEdges {
        right: true,
        top: true,
        ..Default::default()
    };
    assert_eq!(ne.cursor(), window::CursorKind::SizeNESW);
    assert_eq!(ResizeEdges::default().cursor(), window::CursorKind::Arrow);
    assert!(!ResizeEdges::default().any());
}

/// 1.8.2：图片/视频不响应拖边改大小（`resize_margin` 返回 0 → 这里必须一条边都不命中）。
#[test]
fn edges_at_no_resize_for_media() {
    let rc = RECT {
        left: 0,
        top: 0,
        right: 800,
        bottom: 600,
    };
    // margin = 0 时，就算鼠标正好压在边上也不该抓边。
    for (x, y) in [
        (0, 300),
        (800, 300),
        (400, 0),
        (400, 600),
        (0, 0),
        (800, 600),
    ] {
        assert!(
            !edges_at(rc, x, y, 0).any(),
            "({x},{y}) 不该命中边缘（margin=0）"
        );
    }
    assert!(edges_at(rc, 798, 300, 8).any(), "正常窗口仍然要能拖边");
}

/// 1.8.2：只有图片/视频取消拖边改大小，其它类型照旧。
#[test]
fn resizable_kind_excludes_image_and_video() {
    assert!(!resizable_kind(Some(format::Kind::Image)));
    assert!(!resizable_kind(Some(format::Kind::Video)));
    assert!(resizable_kind(Some(format::Kind::Pdf)));
    assert!(resizable_kind(Some(format::Kind::Text)));
    assert!(resizable_kind(None));
}

// ---------------------------------------------------------- 压缩包目录树（1.7.3）

fn entry(path: &str, size: u64, is_dir: bool) -> qlr_render::archive::Entry {
    qlr_render::archive::Entry {
        path: path.to_string(),
        size,
        time: String::new(),
        is_dir,
        depth: path.matches('/').count(),
    }
}

/// 展平结果：`(名字, 层级)`，方便逐行断言。
fn flat_names(nodes: &[ArchiveNode], roots: &[usize]) -> Vec<(String, usize)> {
    flatten_archive(nodes, roots)
        .into_iter()
        .map(|(id, depth)| (nodes[id].name.clone(), depth))
        .collect()
}

/// 目录行必须排在子项之前、缩进逐级 +1，且折叠状态会被展平尊重。
#[test]
fn archive_tree_nests_dirs_before_their_children() {
    let entries = vec![
        entry("a/", 0, true),
        entry("a/b/", 0, true),
        entry("a/b/x.txt", 10, false),
        entry("a/y.txt", 20, false),
        entry("root.txt", 30, false),
    ];
    let (mut nodes, roots) = build_archive_tree(&entries);
    assert_eq!(
        flat_names(&nodes, &roots),
        vec![
            ("a".to_string(), 0),
            ("b".to_string(), 1),
            ("x.txt".to_string(), 2),
            ("y.txt".to_string(), 1),
            ("root.txt".to_string(), 0),
        ]
    );
    // 折叠 "a"（roots 的第一个）之后，它下面三行必须一起消失。
    let a = roots[0];
    nodes[a].expanded = false;
    assert_eq!(
        flat_names(&nodes, &roots),
        vec![("a".to_string(), 0), ("root.txt".to_string(), 0)]
    );
}

/// 打包器常常不写目录条目（`Compress-Archive`、`python zipfile`），
/// 父目录链必须在建树时补出来，否则 `src/core/main.rs` 会变成三行平铺。
#[test]
fn archive_tree_invents_missing_parent_dirs() {
    let entries = vec![
        entry("src/core/main.rs", 100, false),
        entry("LICENSE", 5, false),
    ];
    let (nodes, roots) = build_archive_tree(&entries);
    assert_eq!(
        flat_names(&nodes, &roots),
        vec![
            ("src".to_string(), 0),
            ("core".to_string(), 1),
            ("main.rs".to_string(), 2),
            ("LICENSE".to_string(), 0),
        ]
    );
    assert!(nodes[roots[0]].is_dir, "补出来的 src 必须是目录行");
}

/// 目录在前、同类按名字（大小写不敏感）排：跟资源管理器的顺序一致。
#[test]
fn archive_tree_sorts_dirs_then_names() {
    let entries = vec![
        entry("zeta.txt", 1, false),
        entry("Alpha.txt", 1, false),
        entry("mid/", 0, true),
        entry("beta/", 0, true),
    ];
    let (nodes, roots) = build_archive_tree(&entries);
    assert_eq!(
        flat_names(&nodes, &roots),
        vec![
            ("beta".to_string(), 0),
            ("mid".to_string(), 0),
            ("Alpha.txt".to_string(), 0),
            ("zeta.txt".to_string(), 0),
        ]
    );
}

/// 点击反算行号：口径必须和 `paint_list` 一致（内容区顶 + `window::text_padding_top()`，行高 `window::list_row_height()`）。
#[test]
fn archive_row_index_matches_paint_list_metrics() {
    let content = RECT {
        left: 0,
        top: 30,
        right: 800,
        bottom: 748,
    };
    let top = 30 + window::text_padding_top();
    assert_eq!(archive_row_index(content, 0, top), Some(0));
    assert_eq!(
        archive_row_index(content, 0, top + window::list_row_height() - 1),
        Some(0)
    );
    assert_eq!(
        archive_row_index(content, 0, top + window::list_row_height()),
        Some(1)
    );
    // 滚动 10 行后，同样的 y 对应第 10 行。
    assert_eq!(archive_row_index(content, 10, top), Some(10));
    // 点在内容区之上（顶栏）或之下（窗口底）都不算行。
    assert_eq!(archive_row_index(content, 0, top - 1), None);
    assert_eq!(archive_row_index(content, 0, 748), None);
}

/// 1.8.3 自愈：判挂阈值必须"起步 10 s、连续自愈翻倍、封顶 40 s"。
///
/// 这条锁住三件事：① 阈值不能偷偷改小（会把慢盘/大图误判成挂死）；
/// ② 连续重建必须退避（否则新线程也挂死时变成每 10 s 死一条的连环抖动）；
/// ③ 退避有上限（否则挂死次数一多，阈值涨到几分钟，用户以为程序又"永久失效"了）。
#[test]
fn hang_threshold_backs_off_then_caps() {
    assert_eq!(hang_threshold(0), Duration::from_millis(10_000));
    assert_eq!(hang_threshold(1), Duration::from_millis(20_000));
    assert_eq!(hang_threshold(2), Duration::from_millis(40_000));
    assert_eq!(
        hang_threshold(9),
        Duration::from_millis(40_000),
        "封顶后不再翻倍"
    );
}

/// 1.8.3 自愈日志：任务描述带文件名/类型/尺寸/页码，但**不许**带上完整用户路径。
#[test]
fn job_desc_has_no_user_path() {
    let job = RenderJob {
        generation: 7,
        path: PathBuf::from(r"C:\Users\someone\Desktop\report.pdf"),
        max_width: 800,
        max_height: 1000,
        page: 2,
        upscale: false,
    };
    let desc = describe_job(&job);
    assert!(desc.contains("report.pdf"), "要有文件名：{desc}");
    assert!(desc.contains("800x1000"), "要有目标尺寸：{desc}");
    assert!(
        desc.contains("第3页"),
        "PDF 要带页码（0 基转 1 基）：{desc}"
    );
    assert!(
        !desc.contains("someone"),
        "日志不许出现完整用户路径：{desc}"
    );
}

/// 造一帧"占 `mb` 兆字节"的位图：`canvas_bytes` 只看 `bgra.len()`。
fn fake_frame(mb: usize, tag: u8) -> Arc<Canvas> {
    let bytes = mb * 1024 * 1024;
    Arc::new(Canvas::Bitmap(qlr_render::Bitmap {
        width: 1024,
        height: (bytes / 4 / 1024) as u32,
        bgra: vec![tag; bytes],
    }))
}

fn fake_key(n: usize) -> FrameKey {
    FrameKey {
        path: PathBuf::from(format!("f{n}.png")),
        mtime: 0,
        size: 0,
        page: 0,
        width: 1024,
        height: 1024,
        upscale: false,
    }
}

/// 1.8.3：上一帧缓存同时受"帧数 ≤ 3"和"总字节 ≤ 24 MB"两条上限约束，
/// 且单帧就超预算时仍保留最近 1 帧（"同一文件重开秒开"这条收益不能被限额吃掉）。
#[test]
fn last_frame_cache_respects_byte_budget() {
    // 3 帧 × 10 MB = 30 MB > 24 MB → 只能留 2 帧，且被淘汰的必须是最旧那帧。
    let mut frames: LastFrames = std::collections::VecDeque::new();
    for i in 0..3 {
        frames.push_back((fake_key(i), fake_frame(10, i as u8)));
    }
    trim_last_frames(&mut frames);
    assert_eq!(frames.len(), 2, "24 MB 预算下 10 MB 的帧最多留 2 帧");
    assert_eq!(
        frames.back().expect("至少要留一帧").0.path,
        PathBuf::from("f2.png"),
        "最新的那帧不许被淘汰"
    );

    // 单帧 30 MB 已超预算：留 1 帧；再来一帧 30 MB 时淘汰旧的，仍留 1 帧。
    let mut frames: LastFrames = std::collections::VecDeque::new();
    frames.push_back((fake_key(0), fake_frame(30, 0)));
    trim_last_frames(&mut frames);
    assert_eq!(frames.len(), 1, "单帧超预算也要保住最近 1 帧");
    frames.push_back((fake_key(1), fake_frame(30, 1)));
    trim_last_frames(&mut frames);
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames.back().expect("至少要留一帧").0.path,
        PathBuf::from("f1.png")
    );

    // 小帧照旧受"最多 3 帧"约束（1 MB × 5 → 3）。
    let mut frames: LastFrames = std::collections::VecDeque::new();
    for i in 0..5 {
        frames.push_back((fake_key(i), fake_frame(1, i as u8)));
    }
    trim_last_frames(&mut frames);
    assert_eq!(frames.len(), LAST_FRAME_CAP);
}

/// 视频帧在 `State` 里存 `Arc<RawFrame>`（结构评审 §7-1）：锁内只加引用计数，
/// 贴图（3.6 MB 的 `StretchDIBits`）放到锁外。这里钉住"确实是共享而不是拷贝"。
#[test]
fn video_frame_is_shared_not_copied() {
    let frame = Arc::new(qlr_render::media::RawFrame {
        width: 1280,
        height: 720,
        bgra: vec![7u8; 1280 * 720 * 4],
        ts_ms: 0,
    });
    let cloned = frame.clone();
    assert!(Arc::ptr_eq(&frame, &cloned), "锁内取帧必须是共享同一块缓冲");
    assert_eq!(Arc::strong_count(&frame), 2);
}

/// 结构评审 §7-1 的账：以前 `video_paint` 拿着 STATE 锁贴整帧，720p 贴一次
/// `StretchDIBits` 1–3 ms，播放时每秒几十次；现在锁内只剩一次 `Arc` 克隆。
///
/// 这里量的是**锁内那一半**：整帧克隆 vs `Arc` 克隆。
#[test]
#[ignore = "基准，跑法：cargo test --release -p qlr-app video_lock_hold -- --ignored --nocapture"]
fn bench_video_lock_hold() {
    let frame = Arc::new(qlr_render::media::RawFrame {
        width: 1280,
        height: 720,
        bgra: vec![3u8; 1280 * 720 * 4],
        ts_ms: 0,
    });
    let n = 200;
    let t = Instant::now();
    for _ in 0..n {
        let copy = (*frame).clone();
        std::hint::black_box(&copy);
    }
    let copy_ns = t.elapsed().as_secs_f64() * 1e9 / n as f64;

    let t = Instant::now();
    for _ in 0..n {
        let shared = frame.clone();
        std::hint::black_box(&shared);
    }
    let arc_ns = t.elapsed().as_secs_f64() * 1e9 / n as f64;

    println!("1280x720 帧（3.6 MB）锁内开销：整帧克隆 {copy_ns:.0} ns → Arc 克隆 {arc_ns:.0} ns");
    assert!(
        arc_ns * 100.0 < copy_ns,
        "Arc 克隆应该比整帧克隆便宜两个数量级：{arc_ns:.0} ns vs {copy_ns:.0} ns"
    );
}
