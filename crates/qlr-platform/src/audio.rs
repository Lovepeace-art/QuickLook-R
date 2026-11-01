//! 音频播放控制（MCI / winmm）。
//!
//! 只做"打开—播放—暂停—继续—定位—关闭"这几件事，**不做解码**：MCI 的
//! `waveaudio` / `mpegvideo` 设备自己管解码与音频输出，我们只发命令字符串。
//!
//! 为什么是 MCI 而不是 Media Foundation / WASAPI：
//! - 预览窗口要的是"点一下能出声、能暂停、能拖进度"，MCI 一条 `play/pause/seek` 就够，
//!   不需要自己建绘图/音频会话；
//! - MF 的音频链路要自己做时钟与缓冲管理，量级是几百行，收益为 0；
//! - 代价要写清楚：MCI 是**同步命令 + 全局设备表**，一次只能有一个"当前命令"在跑，
//!   而且设备别名是**进程级**的，所以别名必须唯一（见 [`AudioPlayer::open`]）。
//!
//! 约定：所有方法失败都返回 `false`/`0` 并记 WARN，不 panic——预览链路上
//! 播放失败只该让"播放条"灰掉，不该影响窗口本身。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::Media::Multimedia::{mciGetErrorStringW, mciSendStringW};
use windows::core::PCWSTR;

const SCOPE: &str = "audio";

/// MCI 命令串的口径上限（含 `open`/`alias` 等关键字，不只是路径）。
///
/// 文档给的是 128 字符；超长时 MCI 会**静默截断**并把路径当成坏文件，症状是
/// "明明存在的文件打不开"。宁可在门口就拒绝（长路径确实存在，见 CONVENTIONS 的坑清单）。
const MCI_MAX_COMMAND: usize = 128;

/// 别名序号：进程内单调递增，保证多次 `open` 不会撞车。
static NEXT_ALIAS: AtomicU32 = AtomicU32::new(1);

/// 一个已经打开 MCI 设备的播放器。
///
/// 生命周期即 MCI 会话：`Drop` 里 `stop` + `close`，别名随之回收。
pub struct AudioPlayer {
    alias: String,
    path: PathBuf,
    /// 设备是否还开着（`close` 之后置 `false`，避免 Drop 重复发命令）。
    alive: bool,
}

impl std::fmt::Debug for AudioPlayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioPlayer")
            .field("alias", &self.alias)
            .field("path", &self.path)
            .field("alive", &self.alive)
            .finish()
    }
}

impl AudioPlayer {
    /// 打开一个音频文件。失败（不存在 / 不是能播的音频 / 命令串超长）返回 `None`。
    ///
    /// 设备类型按后缀选：`.wav` → `waveaudio`，其余（`.mp3` / `.mp4` / `.avi` …）先试
    /// `mpegvideo`（走 mciqtz32 / DirectShow），再退回"让 MCI 自己认"。
    pub fn open(path: &Path) -> Option<Self> {
        if !path.is_file() {
            qlr_core::log_debug!(SCOPE, "不是文件，跳过：{}", path.display());
            return None;
        }
        let quoted = quote_path(path)?;
        let alias = format!(
            "qlra{}x{}",
            std::process::id(),
            NEXT_ALIAS.fetch_add(1, Ordering::Relaxed)
        );

        let candidates: &[Option<&str>] = if has_ext(path, "wav") {
            &[Some("waveaudio"), None]
        } else {
            &[Some("mpegvideo"), None]
        };
        for kind in candidates {
            let command = match kind {
                Some(kind) => format!("open {quoted} type {kind} alias {alias}"),
                None => format!("open {quoted} alias {alias}"),
            };
            if command.encode_utf16().count() >= MCI_MAX_COMMAND {
                qlr_core::log_warn!(
                    SCOPE,
                    "MCI 命令串超长（{} 字符），拒绝打开：{}",
                    command.encode_utf16().count(),
                    path.display()
                );
                return None;
            }
            match send(&command) {
                Ok(()) => {
                    // 时间单位统一成毫秒（有些设备默认是"帧"或"秒"）。
                    // 设不上不算失败：`waveaudio` 本来就是毫秒。
                    if let Err(code) = send(&format!("set {alias} time format milliseconds")) {
                        qlr_core::log_debug!(SCOPE, "时间格式设置失败：{}", mci_error(code));
                    }
                    qlr_core::log_debug!(
                        SCOPE,
                        "已打开（kind={}）：{}",
                        kind.unwrap_or("auto"),
                        path.display()
                    );
                    return Some(Self {
                        alias,
                        path: path.to_path_buf(),
                        alive: true,
                    });
                }
                Err(code) => {
                    qlr_core::log_debug!(
                        SCOPE,
                        "打开失败（kind={}）：{} — {}",
                        kind.unwrap_or("auto"),
                        path.display(),
                        mci_error(code)
                    );
                }
            }
        }
        qlr_core::log_warn!(SCOPE, "MCI 打不开：{}", path.display());
        None
    }

    /// 设备别名（诊断用；MCI 命令里靠它定位会话）。
    pub fn alias(&self) -> &str {
        &self.alias
    }

    /// 会话是否还开着。
    pub fn is_open(&self) -> bool {
        self.alive
    }

    /// 开始播放（异步：命令一返回就开始出声，不阻塞 UI 线程）。
    pub fn play(&self) -> bool {
        self.run("play", &format!("play {}", self.alias))
    }

    /// 暂停（保留位置）。
    pub fn pause(&self) -> bool {
        self.run("pause", &format!("pause {}", self.alias))
    }

    /// 从暂停处继续。
    pub fn resume(&self) -> bool {
        self.run("resume", &format!("resume {}", self.alias))
    }

    /// 定位到 `ms` 毫秒处（播放中定位会继续播）。
    pub fn seek(&self, ms: u64) -> bool {
        self.run("seek", &format!("seek {} to {ms}", self.alias))
    }

    /// 当前播放位置（毫秒）。拿不到（未播放/设备不支持）返回 0。
    pub fn position_ms(&self) -> u64 {
        self.query_u64(&format!("status {} position", self.alias))
    }

    /// 总时长（毫秒）。拿不到返回 0。
    pub fn duration_ms(&self) -> u64 {
        self.query_u64(&format!("status {} length", self.alias))
    }

    /// 停止播放并把位置归零。
    pub fn stop(&self) {
        // 停止失败没有补救手段，也不该打断调用方。
        if let Err(code) = send(&format!("stop {}", self.alias)) {
            qlr_core::log_debug!(SCOPE, "stop 失败：{}", mci_error(code));
        }
        let _ = send(&format!("seek {} to 0", self.alias));
    }

    fn run(&self, what: &str, command: &str) -> bool {
        match send(command) {
            Ok(()) => true,
            Err(code) => {
                qlr_core::log_warn!(SCOPE, "{what} 失败（{}）：{}", self.alias, mci_error(code));
                false
            }
        }
    }

    fn query_u64(&self, command: &str) -> u64 {
        query(command)
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        if !self.alive {
            return;
        }
        self.alive = false;
        // 顺序不能反：先 stop 再 close，否则有些设备会把 `close` 当成"停止并等待"，
        // 在 UI 线程上阻塞几十毫秒。别名一定要 close，不然进程内的设备表会越攒越多，
        // 之后同名（不会有，但设备槽位会耗尽）设备打不开。
        let _ = send(&format!("stop {}", self.alias));
        match send(&format!("close {}", self.alias)) {
            Ok(()) => qlr_core::log_debug!(SCOPE, "已关闭（{}）", self.alias),
            Err(code) => {
                qlr_core::log_warn!(SCOPE, "close 失败（{}）：{}", self.alias, mci_error(code))
            }
        }
    }
}

/// 把路径包成 MCI 认的双引号形式。路径里带 `"` 是非法文件名，直接拒绝。
fn quote_path(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    if text.contains('"') {
        return None;
    }
    Some(format!("\"{text}\""))
}

fn has_ext(path: &Path, ext: &str) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

/// 发一条 MCI 命令。`Ok(())` 表示返回码 0。
fn send(command: &str) -> Result<(), u32> {
    let wide: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: wide 以 NUL 结尾且活到调用结束；不要返回值、不要回调窗口。
    let code = unsafe { mciSendStringW(PCWSTR(wide.as_ptr()), None, None) };
    if code == 0 { Ok(()) } else { Err(code) }
}

/// 发一条要回值的 MCI 命令（`status ... position` 之类）。
fn query(command: &str) -> Option<String> {
    let wide: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buffer = [0u16; 64];
    // SAFETY: buffer 是本地数组，长度由 windows crate 自己算；返回码 0 才读内容。
    let code = unsafe { mciSendStringW(PCWSTR(wide.as_ptr()), Some(&mut buffer), None) };
    if code != 0 {
        return None;
    }
    let len = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..len]))
}

/// MCI 错误码 → 可读文本（日志用）。
fn mci_error(code: u32) -> String {
    let mut buffer = [0u16; 128];
    // SAFETY: buffer 是本地数组；失败时保留原始错误码。
    let ok = unsafe { mciGetErrorStringW(code, &mut buffer) };
    let text = if ok.as_bool() {
        let len = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..len])
    } else {
        String::new()
    };
    if text.is_empty() {
        format!("MCIERROR={code}")
    } else {
        format!("{text} (MCIERROR={code})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// 夹具目录：优先 `QLRUST_MEDIA_FIXTURES`，否则按开发机布局推算
    /// （`<桌面>\DSH\quicklook-rs\crates\qlr-platform` 往上四层 → `<桌面>\qlrust-dev\compare`）。
    /// 发布仓库不带夹具，缺了就跳过。
    /// 文件不在就跳过（打印原因），不把环境问题算成代码失败。
    fn fixture(name: &str) -> Option<PathBuf> {
        let dir = std::env::var_os("QLRUST_MEDIA_FIXTURES")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../../qlrust-dev/compare")
            });
        let path = dir.join(name);
        if path.is_file() {
            // MCI 对带 `..` 的路径不认（`mciSendString` 的 open 命令会直接失败），
            // 所以这里先解析成绝对路径；物理路径算不出来时退回原路径。
            Some(std::fs::canonicalize(&path).unwrap_or(path))
        } else {
            eprintln!(
                "跳过：缺少夹具 {}（可设 QLRUST_MEDIA_FIXTURES）",
                path.display()
            );
            None
        }
    }

    #[test]
    fn open_reads_duration_of_wav_fixture() {
        let Some(path) = fixture("audio-demo.wav") else {
            return;
        };
        let player = AudioPlayer::open(&path).expect("audio-demo.wav 应该能打开");
        let duration = player.duration_ms();
        eprintln!("wav duration = {duration} ms, alias = {}", player.alias());
        assert!(
            (1_900..=2_100).contains(&duration),
            "夹具是 2 s，实际 {duration} ms"
        );
        // 还没播放，位置应为 0。
        assert_eq!(player.position_ms(), 0);
        player.stop();
    }

    #[test]
    fn aliases_are_unique_so_two_players_coexist() {
        let Some(path) = fixture("audio-demo.wav") else {
            return;
        };
        let a = AudioPlayer::open(&path).expect("第一个会话应能打开");
        let b = AudioPlayer::open(&path).expect("第二个会话应能打开（别名不撞车）");
        assert_ne!(a.alias(), b.alias(), "别名必须唯一");
        // 两个会话都还能正常回值，说明互不干扰（别名撞车时第二条命令会打到同一设备）。
        assert!(a.duration_ms() > 0);
        assert!(b.duration_ms() > 0);
        drop(a);
        // 关掉第一个之后第二个仍然可用。
        assert!(b.is_open());
        assert!(b.duration_ms() > 0);
    }

    #[test]
    fn fails_gracefully_for_non_audio_input() {
        assert!(AudioPlayer::open(Path::new(r"C:\__qlrust_no_such_audio__.wav")).is_none());

        // 有文件、但不是音频：MCI 解析头部失败 → None，不许 panic。
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-tmp/audio");
        std::fs::create_dir_all(&dir).expect("建测试目录失败");
        let bogus = dir.join("not-audio.wav");
        std::fs::write(&bogus, b"this is definitely not a RIFF wave file").expect("写测试文件失败");
        assert!(AudioPlayer::open(&bogus).is_none(), "坏 wav 应该打不开");
    }

    #[test]
    fn seek_and_position_are_milliseconds() {
        let Some(path) = fixture("audio-demo.wav") else {
            return;
        };
        let player = AudioPlayer::open(&path).expect("应能打开");
        assert!(player.seek(500), "seek 到 500 ms 应成功");
        let position = player.position_ms();
        eprintln!("seek(500) → position = {position} ms");
        assert!(
            (400..=700).contains(&position),
            "定位后位置应≈500 ms，实际 {position}"
        );
    }

    /// 真实出声的播放链路（需要音频设备，所以默认 `#[ignore]`）。
    ///
    /// 跑法：`cargo test -p qlr-platform -- --ignored --nocapture playback`
    #[test]
    #[ignore = "需要真实音频设备"]
    fn playback_advances_then_pause_freezes() {
        let Some(path) = fixture("audio-demo.wav") else {
            return;
        };
        let player = AudioPlayer::open(&path).expect("应能打开");
        assert!(player.play(), "play 应成功");

        std::thread::sleep(Duration::from_millis(300));
        let after_play = player.position_ms();
        eprintln!("play 300 ms 后 position = {after_play}");
        assert!(
            after_play >= 150,
            "播放 300 ms 后位置应明显前进，实际 {after_play}"
        );

        assert!(player.pause(), "pause 应成功");
        let paused_at = player.position_ms();
        std::thread::sleep(Duration::from_millis(250));
        let still = player.position_ms();
        eprintln!("pause 后 {paused_at} → 250 ms 后 {still}");
        assert!(
            still.abs_diff(paused_at) <= 20,
            "暂停后位置不该继续涨：{paused_at} → {still}"
        );

        assert!(player.resume(), "resume 应成功");
        std::thread::sleep(Duration::from_millis(200));
        let resumed = player.position_ms();
        eprintln!("resume 200 ms 后 position = {resumed}");
        assert!(resumed > still, "继续播放后位置应前进：{still} → {resumed}");

        player.stop();
    }

    /// 任务 C 的探针：MCI 能不能播 mp4（走 mpegvideo / mciqtz32 → DirectShow）。
    ///
    /// `#[ignore]`：会真的尝试打开视频设备（可能弹窗），只在手动评估时跑。
    /// 结论（2026-11-01 本机实测）写在这个测试的 eprintln 里，报告里同步。
    #[test]
    #[ignore = "手动评估：MCI 对 mp4 的支持面"]
    fn probe_mci_video_support() {
        let Some(path) = fixture("video-demo.mp4") else {
            return;
        };
        let Some(quoted) = quote_path(&path) else {
            return;
        };
        let started = Instant::now();
        // 逐个设备类型试，把 MCI 的原始错误码打出来——"打不开"和"设备类型不认识"
        // 是两件完全不同的事，报告里要能分辨。
        for (label, command) in [
            (
                "type mpegvideo",
                format!("open {quoted} type mpegvideo alias mcipv"),
            ),
            (
                "type MPEGVideo",
                format!("open {quoted} type MPEGVideo alias mcipv"),
            ),
            (
                "type avivideo",
                format!("open {quoted} type avivideo alias mcipv"),
            ),
            ("(auto)", format!("open {quoted} alias mcipv")),
        ] {
            let result = send(&command);
            match result {
                Ok(()) => {
                    let duration = query("status mcipv length");
                    let played = send("play mcipv").is_ok();
                    std::thread::sleep(Duration::from_millis(400));
                    let position = query("status mcipv position");
                    eprintln!(
                        "  {label}: open=OK duration={duration:?} play={played} position={position:?}（{:.1} ms）",
                        started.elapsed().as_secs_f64() * 1000.0
                    );
                    let _ = send("stop mcipv");
                    let _ = send("close mcipv");
                }
                Err(code) => eprintln!("  {label}: open 失败 → {}", mci_error(code)),
            }
        }
        // 反证：同一个 type 名对 wav 是否"认得"。认得的类型名 + 对 mp4 失败
        // ⇒ 结论是"这个设备类型不支持该容器/编码"，而不是"类型名写错了"。
        if let Some(wav) = fixture("audio-demo.wav") {
            if let Some(qwav) = quote_path(&wav) {
                let code = send(&format!("open {qwav} type MPEGVideo alias mcipw"));
                eprintln!(
                    "  反证 type MPEGVideo + wav: {}",
                    match code {
                        Ok(()) => "open=OK（说明类型名有效）".to_string(),
                        Err(code) => format!("open 失败 → {}", mci_error(code)),
                    }
                );
                let _ = send("close mcipw");
            }
        }
        eprintln!(
            "  总耗时 {:.1} ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
}
