//! 音频与视频播放：MCI 播放、Media Foundation 取帧、进度条、播放 / 暂停 / 跳转。

use super::*;

/// 打开音频并**自动播放**（1.7.9）。
///
/// 打不开就打不开：只在顶栏给一句提示，卡片（时长/格式）照常显示，用户至少知道文件是什么。
/// 自动播放是有意的——预览工具"点到一个声音文件就该出声"（原版 QuickLook 也是这个行为）；
/// 想静音按 P 暂停，收起窗口自动停（[`stop_audio`]）。
pub(crate) fn start_audio(path: &Path) {
    stop_audio();
    let hwnd = preview_raw();
    let Some(player) = qlr_platform::audio::AudioPlayer::open(path) else {
        log_warn!(
            SCOPE,
            "AUDIO 打不开（缺解码器或不支持的容器）：{}",
            path.display()
        );
        if let Some(hwnd) = hwnd {
            show_toast(hwnd, "这个音频打不开（缺解码器）");
        }
        return;
    };
    let duration = player.duration_ms();
    let played = player.play();
    log_info!(
        SCOPE,
        "AUDIO 打开 {} · 时长 {}ms · 播放={}",
        path.display(),
        duration,
        played
    );
    with_state(|s| {
        s.audio = Some(player);
        s.audio_ticking = played;
    });
    match hwnd {
        Some(hwnd) if played => {
            window::start_timer(hwnd, TIMER_AUDIO, AUDIO_TICK_MS);
            paint_audio_now(hwnd);
        }
        Some(hwnd) => show_toast(hwnd, "这个音频播不了（缺解码器）"),
        None => {}
    }
}

/// 停声并释放 MCI 设备（收起窗口 / 换文件 / 退出都走这里）。
///
/// 必须真的 drop `AudioPlayer`：它的 `Drop` 会 `MCI_CLOSE`。不关的话设备被占住，
/// 再预览同一个文件会拿到同一个 alias，表现成"第二次预览没声音"。
pub(crate) fn stop_audio() {
    let (had, ticking, hwnd) =
        with_state(|s| (s.audio.take().is_some(), s.audio_ticking, s.preview.raw())).unwrap_or((
            false,
            false,
            HWND::default(),
        ));
    with_state(|s| s.audio_ticking = false);
    if ticking && !hwnd.0.is_null() {
        window::stop_timer(hwnd, TIMER_AUDIO);
    }
    if had {
        log_debug!(SCOPE, "AUDIO 停止并释放设备");
    }
}

/// 当前音频进度：`(已播 ms, 总 ms)`。没有音频预览时 `None`。
pub(crate) fn audio_progress() -> Option<(u64, u64)> {
    with_state(|s| s.audio.as_ref().map(|p| (p.position_ms(), p.duration_ms()))).flatten()
}

/// 播放 / 暂停（P 键）。返回是否真的切了（没有音频预览时 `false`）。
///
/// 用 `audio_ticking`（刷新计时器是否开着）当"正在播"的判据：计时器只在播放时开，
/// 暂停就杀掉——比每帧去问 MCI 状态便宜，也不会因为 `status` 报错而误判。
pub(crate) fn audio_toggle() -> bool {
    let Some(playing_now) = with_state(|s| s.audio.as_ref().map(|_| s.audio_ticking)).flatten()
    else {
        return false;
    };
    let ok = with_state(|s| {
        s.audio
            .as_ref()
            .map(|p| if playing_now { p.pause() } else { p.resume() })
    })
    .flatten()
    .unwrap_or(false);
    if !ok {
        return false;
    }
    let playing = !playing_now;
    with_state(|s| s.audio_ticking = playing);
    if let Some(hwnd) = preview_raw() {
        if playing {
            window::start_timer(hwnd, TIMER_AUDIO, AUDIO_TICK_MS);
        } else {
            window::stop_timer(hwnd, TIMER_AUDIO);
        }
        show_toast(hwnd, if playing { "播放" } else { "暂停" });
        paint_audio_now(hwnd);
    }
    log_info!(SCOPE, "AUDIO {}", if playing { "继续播放" } else { "暂停" });
    true
}

/// 快进 / 快退 `delta_ms`（左右键、滚轮）。到两头就夹住，不报错。
pub(crate) fn audio_seek(delta_ms: i64) -> bool {
    let Some((pos, dur)) = audio_progress() else {
        return false;
    };
    let target = (pos as i64 + delta_ms).clamp(0, dur.max(1) as i64) as u64;
    let ok = with_state(|s| s.audio.as_ref().map(|p| p.seek(target)))
        .flatten()
        .unwrap_or(false);
    if ok {
        log_info!(SCOPE, "AUDIO 定位 {}ms / {}ms", target, dur);
        if let Some(hwnd) = preview_raw() {
            paint_audio_now(hwnd);
        }
    }
    ok
}

/// 立刻重画进度条那一条（只刷这一块，不整窗重画——音频每 250ms 刷一次，整窗重画太贵）。
pub(crate) fn paint_audio_now(hwnd: HWND) {
    let Some((pos, dur)) = playback_progress() else {
        return;
    };
    let playing = playback_playing();
    let content = window::chrome_rects(window::client_rect(hwnd)).content;
    window::paint_audio_bar(hwnd, content, pos, dur, playing);
}

/// 打开视频并开始逐帧播放（1.7.9）。
///
/// 打不开就只给一句提示（和音频一致）：卡片/首帧照旧显示，用户至少知道文件是什么。
pub(crate) fn start_video(path: &Path) {
    stop_video();
    let hwnd = preview_raw();
    let Some(mut stream) = qlr_render::media::FrameStream::open(path) else {
        log_warn!(
            SCOPE,
            "VIDEO 打不开（缺解码器或不支持的容器）：{}",
            path.display()
        );
        if let Some(hwnd) = hwnd {
            show_toast(hwnd, "这个视频打不开（缺解码器）");
        }
        return;
    };
    let duration = stream.duration_ms();
    let Some(first) = stream.next_frame() else {
        if let Some(hwnd) = hwnd {
            show_toast(hwnd, "这个视频解不出画面");
        }
        return;
    };
    log_info!(
        SCOPE,
        "VIDEO 播放 {} · {}x{} · 时长 {}ms",
        path.display(),
        first.width,
        first.height,
        duration
    );
    with_state(|s| {
        s.video = Some(VideoPlay {
            stream,
            last: Some(first),
            anchor_pos_ms: 0,
            anchor_at: std::time::Instant::now(),
            playing: true,
            duration_ms: duration,
        });
    });
    if let Some(hwnd) = hwnd {
        video_paint(hwnd);
        window::start_timer(hwnd, TIMER_VIDEO, VIDEO_TICK_MS);
        paint_audio_now(hwnd);
    }
}

/// 停视频并放掉解码器（收起窗口 / 换文件 / 退出都走这里）。
pub(crate) fn stop_video() {
    let (had, hwnd) = with_state(|s| (s.video.take().is_some(), s.preview.raw()))
        .unwrap_or((false, HWND::default()));
    if had && !hwnd.0.is_null() {
        window::stop_timer(hwnd, TIMER_VIDEO);
    }
    if had {
        log_debug!(SCOPE, "VIDEO 停止并释放解码器");
    }
}

/// 画面播放到哪一毫秒了（暂停时就是暂停的那个点）。
pub(crate) fn video_position_ms() -> Option<u64> {
    with_state(|s| {
        s.video.as_ref().map(|v| {
            if v.playing {
                let elapsed = v.anchor_at.elapsed().as_millis() as u64;
                let pos = v.anchor_pos_ms.saturating_add(elapsed);
                // 别越过总时长（否则进度条会画到 100% 之外、"到头"判断也会抖）。
                if v.duration_ms > 0 {
                    pos.min(v.duration_ms)
                } else {
                    pos
                }
            } else {
                v.anchor_pos_ms
            }
        })
    })
    .flatten()
}

/// 把当前这一帧贴到内容区（等比居中）。
///
/// 直接在锁内画：`paint_bitmap_fit_window` 只做 GDI（GetDC/FillRect/StretchDIBits），
/// 不会回调进 state；反过来先把 720p 帧拷一份（3.6MB）每帧一次反而更贵。
pub(crate) fn video_paint(hwnd: HWND) {
    let content = window::chrome_rects(window::client_rect(hwnd)).content;
    // 只贴"画面区"：底部那条进度条不归帧管，否则每帧都会把它擦掉。
    let area = window::picture_area(content);
    let _ = with_state(|s| {
        if let Some(v) = s.video.as_ref()
            && let Some(f) = v.last.as_ref()
        {
            window::paint_bitmap_fit_window(hwnd, area, f.width, f.height, &f.bgra);
        }
    });
}

/// 取帧定时器：追上"此刻应该显示的那一帧"，落后就跳帧（宁可丢帧也不放慢）。
pub(crate) fn video_tick(hwnd: HWND) {
    let Some(target) = video_position_ms() else {
        window::stop_timer(hwnd, TIMER_VIDEO);
        return;
    };
    let mut ended = false;
    let mut painted = false;
    let _ = with_state(|s| {
        let Some(v) = s.video.as_mut() else {
            return;
        };
        if !v.playing {
            return;
        }
        loop {
            let Some(frame) = v.stream.next_frame() else {
                ended = true;
                break;
            };
            let ts = frame.ts_ms;
            v.last = Some(frame);
            painted = true;
            // 解到的帧已经追上目标时刻就不再往下解（剩下的时间留给上屏）。
            if ts + VIDEO_TICK_MS as u64 >= target {
                break;
            }
        }
    });
    if ended {
        let dur = with_state(|s| s.video.as_ref().map(|v| v.duration_ms))
            .flatten()
            .unwrap_or(0);
        with_state(|s| {
            if let Some(v) = s.video.as_mut() {
                v.playing = false;
                // 停在末尾：进度条显示满格，再按 P 就从头重放（`video_toggle` 里判断）。
                v.anchor_pos_ms = dur;
            }
        });
        window::stop_timer(hwnd, TIMER_VIDEO);
        let content = window::chrome_rects(window::client_rect(hwnd)).content;
        window::paint_audio_bar(hwnd, content, dur, dur, false);
        show_toast(hwnd, "播放结束");
        log_debug!(SCOPE, "VIDEO 播放结束（{}ms）", dur);
        return;
    }
    if painted {
        video_paint(hwnd);
        paint_audio_now(hwnd);
    }
}

/// 视频播放 / 暂停（P 键）。停在末尾时再按 = 从头重放。
pub(crate) fn video_toggle() -> bool {
    let hwnd = preview_raw();
    let Some(now_playing) = with_state(|s| s.video.as_ref().map(|v| v.playing)).flatten() else {
        return false;
    };
    let pos = video_position_ms().unwrap_or(0);
    let ok = with_state(|s| {
        let Some(v) = s.video.as_mut() else {
            return false;
        };
        if !now_playing && v.duration_ms > 0 && pos + 200 >= v.duration_ms {
            // 已经到末尾了：再按 P 就是"重放"。
            let _ = v.stream.seek(0);
            v.last = v.stream.next_frame();
            v.anchor_pos_ms = 0;
        } else {
            v.anchor_pos_ms = pos;
        }
        v.anchor_at = std::time::Instant::now();
        v.playing = !now_playing;
        true
    })
    .unwrap_or(false);
    if !ok {
        return false;
    }
    if let Some(hwnd) = hwnd {
        if now_playing {
            window::stop_timer(hwnd, TIMER_VIDEO);
        } else {
            window::start_timer(hwnd, TIMER_VIDEO, VIDEO_TICK_MS);
            video_paint(hwnd);
        }
        show_toast(hwnd, if now_playing { "暂停" } else { "播放" });
        paint_audio_now(hwnd);
    }
    log_info!(
        SCOPE,
        "VIDEO {}",
        if now_playing {
            "暂停"
        } else {
            "继续播放"
        }
    );
    true
}

/// 视频快进 / 快退 `delta_ms`（左右键、滚轮）。到两头就夹住。
pub(crate) fn video_seek(delta_ms: i64) -> bool {
    let base = video_position_ms();
    let (Some(base), Some(dur)) = (
        base,
        with_state(|s| s.video.as_ref().map(|v| v.duration_ms)).flatten(),
    ) else {
        return false;
    };
    let target = (base as i64 + delta_ms).clamp(0, dur.max(1) as i64) as u64;
    let ok = with_state(|s| {
        let Some(v) = s.video.as_mut() else {
            return false;
        };
        if !v.stream.seek(target) {
            return false;
        }
        // 跳完立刻解一帧：不然画面还停在原处，看起来像"没生效"。
        v.last = v.stream.next_frame();
        v.anchor_pos_ms = target;
        v.anchor_at = std::time::Instant::now();
        true
    })
    .unwrap_or(false);
    if ok {
        log_info!(SCOPE, "VIDEO 定位 {}ms / {}ms", target, dur);
        if let Some(hwnd) = preview_raw() {
            video_paint(hwnd);
            paint_audio_now(hwnd);
        }
    }
    ok
}

/// 当前有没有"会动"的预览（音频或视频）：滚轮/左右键的语义就跟着它变。
///
/// 注意判据是"有没有这种东西"，不是"正在不在播"——暂停中的视频也归它管，
/// 否则暂停时滚轮会掉进下面的缩放/翻页分支。
pub(crate) fn playback_active() -> bool {
    with_state(|s| s.visible && (s.audio.is_some() || s.video.is_some())).unwrap_or(false)
}

/// 播放/暂停（P 键），音频与视频共用。
pub(crate) fn playback_toggle() -> bool {
    if with_state(|s| s.audio.is_some()).unwrap_or(false) {
        audio_toggle()
    } else {
        video_toggle()
    }
}

/// 快进/快退（左右键、滚轮），音频与视频共用。
pub(crate) fn playback_seek(delta_ms: i64) -> bool {
    if with_state(|s| s.audio.is_some()).unwrap_or(false) {
        audio_seek(delta_ms)
    } else {
        video_seek(delta_ms)
    }
}

/// 进度条要显示的 `(已播 ms, 总 ms)`：音频优先，其次视频。
pub(crate) fn playback_progress() -> Option<(u64, u64)> {
    if let Some(p) = audio_progress() {
        return Some(p);
    }
    let pos = video_position_ms()?;
    let dur = with_state(|s| s.video.as_ref().map(|v| v.duration_ms)).flatten()?;
    Some((pos, dur))
}

/// 进度条上该显示"播放中"还是"已暂停"。
pub(crate) fn playback_playing() -> bool {
    if with_state(|s| s.audio.is_some()).unwrap_or(false) {
        return with_state(|s| s.audio_ticking).unwrap_or(false);
    }
    with_state(|s| s.video.as_ref().map(|v| v.playing))
        .flatten()
        .unwrap_or(false)
}
