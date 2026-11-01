//! UTF-16 工具：Win32 一律要 NUL 结尾的宽字符串，用这里统一转换，避免各处手写。

/// 转成 NUL 结尾的 UTF-16，供 `PCWSTR` 使用。
///
/// 返回的 `Vec` 必须活得比 `PCWSTR` 长（调用点通常写成
/// `let name = to_wide(s); ... PCWSTR(name.as_ptr())`）。
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 把定长宽字符数组（如 `NOTIFYICONDATAW::szTip`）填上文本，自动截断并保证结尾 NUL。
pub fn fill_buffer(dst: &mut [u16], src: &str) {
    if dst.is_empty() {
        return;
    }
    let mut n = 0usize;
    for (i, unit) in src.encode_utf16().enumerate() {
        if i + 1 >= dst.len() {
            break;
        }
        dst[i] = unit;
        n = i + 1;
    }
    dst[n] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_wide_is_nul_terminated() {
        let w = to_wide("ab");
        assert_eq!(w, vec![0x61, 0x62, 0]);
    }

    #[test]
    fn fill_buffer_truncates_and_terminates() {
        let mut buf = [0u16; 4];
        fill_buffer(&mut buf, "abcdef");
        assert_eq!(buf, [0x61, 0x62, 0x63, 0]);
    }
}
