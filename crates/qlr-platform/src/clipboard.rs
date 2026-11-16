//! 剪贴板（1.9.4）：目前只有"写一段文本"这一个需求。
//!
//! 为什么放平台层：`OpenClipboard/SetClipboardData` 是 Win32，`qlr-core` 不许依赖 `windows`。
//! 为什么不做读取：预览器是只读的，读剪贴板既没必要也多一份隐私面。
//!
//! 实现要点（踩过的坑都写在注释里）：
//! - `OpenClipboard` 会失败（别的进程正占着剪贴板），所以要重试几次，不能一次失败就放弃；
//! - `GlobalAlloc` 出来的块所有权在 `SetClipboardData` 成功后归系统，**不能再自己 `GlobalFree`**；
//! - 文本用 `CF_UNICODETEXT`，必须带结尾 0，长度按字节算。

use windows::Win32::Foundation::{GlobalFree, HANDLE};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;

/// 把 `text` 写进剪贴板。成功返回 `true`。
///
/// 失败是**非致命**的：调用方只记一条 warn，不打断预览。
pub fn set_text(text: &str) -> bool {
    // 空文本也允许（相当于清空），但 `CF_UNICODETEXT` 要给一个只含结尾 0 的块。
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    let bytes = wide.len() * std::mem::size_of::<u16>();

    // 打开剪贴板最多试 5 次（每次间隔 10ms）：别的程序短暂占用是常态。
    let mut opened = false;
    for _ in 0..5 {
        if unsafe { OpenClipboard(None) }.is_ok() {
            opened = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if !opened {
        return false;
    }

    let ok = (|| -> bool {
        unsafe {
            if EmptyClipboard().is_err() {
                return false;
            }
            let handle = match GlobalAlloc(GMEM_MOVEABLE, bytes) {
                Ok(h) => h,
                Err(_) => return false,
            };
            if handle.0.is_null() {
                return false;
            }
            let ptr = GlobalLock(handle);
            if ptr.is_null() {
                // 还没交给系统，得自己释放（否则泄漏）。
                let _ = GlobalFree(Some(handle));
                return false;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr as *mut u16, wide.len());
            let _ = GlobalUnlock(handle);
            // 成功之后所有权归系统，不能再 GlobalFree。
            if SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(handle.0))).is_err() {
                let _ = GlobalFree(Some(handle));
                return false;
            }
            true
        }
    })();

    unsafe {
        let _ = CloseClipboard();
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::GetClipboardData;

    /// 真机冒烟：写进去再读回来（要碰系统剪贴板，默认 `#[ignore]`，
    /// 手动用 `cargo test -p qlr-platform --lib --offline clipboard -- --ignored` 跑）。
    #[test]
    #[ignore]
    fn write_then_read_back_round_trips() {
        let text = "表格复制 ✓ 1,234.5";
        assert!(set_text(text), "写剪贴板失败（被别的程序占着？）");
        unsafe {
            assert!(OpenClipboard(None).is_ok(), "打开剪贴板失败");
            let handle = GetClipboardData(CF_UNICODETEXT.0 as u32).expect("读不到 CF_UNICODETEXT");
            let hg = HGLOBAL(handle.0);
            let ptr = GlobalLock(hg) as *const u16;
            assert!(!ptr.is_null());
            let mut len = 0usize;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            let back = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
            let _ = GlobalUnlock(hg);
            let _ = CloseClipboard();
            assert_eq!(back, text);
        }
    }
}
