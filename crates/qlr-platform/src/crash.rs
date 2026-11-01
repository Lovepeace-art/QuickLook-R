//! 崩溃兜底（1.2.0）：让"进程没了"这件事至少留下证据。
//!
//! 先说清楚**做不到什么**：release 档是 `panic = "abort"`（A8.5 体积预算：
//! 去掉 unwind 表 + fat LTO，exe 才 1.9 MB），所以 panic **不能**被 `catch_unwind`
//! 救回来——进程会直接结束。因此这一层不承诺"插件 panic 不死宿主"，只承诺
//! **事后可诊断**：
//!
//! - panic hook 写一份文本报告（消息 + 位置 + 线程 + backtrace）；
//! - 同时用 `MiniDumpWriteDump` 落一个 `.dmp`，可以用 WinDbg / Visual Studio
//!   直接打开看崩溃时的调用栈；
//! - 下次启动时 `report_previous_crash()` 把最近一份报告的位置写进日志，
//!   让用户/我们能一眼看到"上次是崩了，不是被杀了"。
//!
//! 报告落在 `<data_dir>\crashes\`（便携模式是 `UserData\crashes\`）。

use std::io::Write;
use std::path::{Path, PathBuf};

use qlr_core::{log_warn, paths};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Diagnostics::Debug::{
    MINIDUMP_TYPE, MiniDumpNormal, MiniDumpWithThreadInfo, MiniDumpWriteDump,
};
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId};

/// 崩溃报告目录：`<data_dir>\crashes`。
pub fn crash_dir() -> PathBuf {
    paths::data_dir().join("crashes")
}

/// 安装 panic hook。重复调用是幂等的（后装的覆盖前装的）。
pub fn install_hook() {
    std::panic::set_hook(Box::new(|info| {
        // 注意：这段代码在意料之外的时刻运行，任何 IO 失败都必须吞掉，
        // 绝不能在"正在崩溃"的时候再 panic 一次。
        // 文件名必须自己消毒：`utc_timestamp()` 里有 `:`（Windows 文件名非法），
        // 直接拼会**静默**创建失败——1.2.0 第一次验证时 dump=false 就是这么来的。
        let stamp = qlr_core::log::utc_timestamp()
            .chars()
            .map(|c| match c {
                '0'..='9' | 'A'..='Z' | 'a'..='z' | '-' | '_' => c,
                _ => '-',
            })
            .collect::<String>();
        let dir = crash_dir();
        let _ = std::fs::create_dir_all(&dir);
        let base = dir.join(format!("crash-{stamp}"));

        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(非字符串 payload)".to_string());
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "(未知位置)".to_string());
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("(未命名线程)").to_string();
        // `force_capture` 不受 RUST_BACKTRACE 影响；release 里 stripped 只有地址，
        // 但配合 dmp 足够定位。
        let backtrace = std::backtrace::Backtrace::force_capture();

        let text = format!(
            "QLRust {} 崩溃报告\n时间：{stamp}\n线程：{thread_name}\n位置：{location}\n消息：{message}\n\n{backtrace}\n",
            qlr_core::version::banner()
        );
        if let Ok(mut f) = std::fs::File::create(base.with_extension("txt")) {
            let _ = f.write_all(text.as_bytes());
            let _ = f.flush();
        }
        let dump = write_minidump(&base.with_extension("dmp"));
        // 日志里保留原来的 "PANIC: ..." 文案（既有脚本/排障习惯都指着它）。
        qlr_core::log::write(
            qlr_core::log::Level::Error,
            "crash",
            &format!("PANIC: {message} @ {location}（dump={dump}）"),
        );
        // 尽量把结果吐到控制台/调试器（GUI 程序通常看不到，但调试器会看到）。
        eprintln!("[qlrust] 崩溃：{message} @ {location}（dump={dump}）");
    }));
}

/// 写一份 minidump 到 `path`，成功返回 `true`。
///
/// 用标准库的 `File` 拿句柄，避免再加一层 `CreateFileW` 的宽字符/权限参数。
pub fn write_minidump(path: &Path) -> bool {
    let Ok(file) = std::fs::File::create(path) else {
        return false;
    };
    use std::os::windows::io::AsRawHandle;
    let handle = HANDLE(file.as_raw_handle());
    // `MiniDumpWithThreadInfo` 会带上每个线程的信息（谁在跑、在等什么），
    // 体积比全内存 dump 小两个数量级，对"卡死/崩溃"够用。
    let dump_type: MINIDUMP_TYPE = MINIDUMP_TYPE(MiniDumpNormal.0 | MiniDumpWithThreadInfo.0);
    let process: HANDLE = unsafe { GetCurrentProcess() };
    let pid = unsafe { GetCurrentProcessId() };
    let result = unsafe { MiniDumpWriteDump(process, pid, handle, dump_type, None, None, None) };
    // `File` 在这里 drop 会关句柄；dump 已经写完，顺序无关。
    drop(file);
    // 顺手清掉一个"空壳"：写失败时可能留下 0 字节文件，别让它骗人。
    if result.is_err() {
        let _ = std::fs::remove_file(path);
        return false;
    }
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    size > 0
}

/// 启动时调用：如果上次崩过，把最近一份报告写进日志（一条 warn）。
pub fn report_previous_crash() {
    let dir = crash_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    let mut count = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("txt") {
            continue;
        }
        count += 1;
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(time) = meta.modified() else { continue };
        if newest.as_ref().is_none_or(|(t, _)| time > *t) {
            newest = Some((time, path));
        }
    }
    if let Some((_, path)) = newest {
        log_warn!(
            "crash",
            "上次运行发生崩溃（crashes 里共 {count} 份报告）：{}",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_dir_sits_under_data_dir() {
        let dir = crash_dir();
        assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some("crashes"));
        assert_eq!(dir.parent(), Some(paths::data_dir().as_path()));
    }

    #[test]
    fn minidump_of_ourselves_is_written() {
        // 写自己进程的快照：能拿到非空文件就说明 dbghelp 链路是通的。
        // 夹具放 `target/`（工作区 exe 是 Low 完整性，写不了 %TEMP%）。
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/qlrust-crash-test");
        std::fs::create_dir_all(&dir).expect("建夹具目录");
        let path = dir.join("self.dmp");
        let ok = write_minidump(&path);
        assert!(ok, "MiniDumpWriteDump 应当成功");
        let size = std::fs::metadata(&path).expect("dump 文件存在").len();
        assert!(size > 1024, "dump 太小（{size} 字节），多半没写成功");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_failure_is_reported_not_panicking() {
        // 目标目录不存在 → 必须返回 false 而不是 panic（崩溃路径上不能二次崩溃）。
        let bad = PathBuf::from("Z:\\不存在的盘\\x.dmp");
        assert!(!write_minidump(&bad));
    }
}
