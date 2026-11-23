//! 极简 HTTP(S) GET 与「用默认浏览器打开网址」。
//!
//! 为什么手写 `winhttp.dll` / `shell32.dll` 的扁平 C 导出，而不是打开 windows crate 的
//! WinHttp 绑定：这里只需要做一件小事——拉一份几 KB 的更新清单 JSON。为一个 GET 去开
//! 整片 `Win32_Networking_WinHttp` feature 不划算；而这些是稳定的扁平 C 导出（和 pdfium
//! 那类一样，手写函数指针风险可控）。属于本 crate（FFI 薄封装）的职责。
//!
//! 安全边界：**只用于更新清单**，不是通用下载器。响应体上限 1 MB，超时默认 8 秒。

use std::ffi::c_void;

use crate::wide::to_wide;

#[link(name = "winhttp")]
unsafe extern "system" {
    fn WinHttpOpen(
        psz_agent_w: *const u16,
        dw_access_type: u32,
        psz_proxy_w: *const u16,
        psz_proxy_bypass_w: *const u16,
        dw_flags: u32,
    ) -> *mut c_void;
    fn WinHttpConnect(
        h_session: *mut c_void,
        pswz_server_name: *const u16,
        n_server_port: u16,
        dw_reserved: u32,
    ) -> *mut c_void;
    fn WinHttpOpenRequest(
        h_connect: *mut c_void,
        psz_verb: *const u16,
        psz_object_name: *const u16,
        psz_version: *const u16,
        psz_referrer: *const u16,
        ppwsz_accept_types: *const *const u16,
        dw_flags: u32,
    ) -> *mut c_void;
    fn WinHttpSetTimeouts(
        h_internet: *mut c_void,
        resolve_timeout: i32,
        connect_timeout: i32,
        send_timeout: i32,
        receive_timeout: i32,
    ) -> i32;
    fn WinHttpSendRequest(
        h_request: *mut c_void,
        lpsz_headers: *const u16,
        dw_headers_length: u32,
        lp_optional: *mut c_void,
        dw_optional_length: u32,
        dw_total_length: u32,
        dw_context: usize,
    ) -> i32;
    fn WinHttpReceiveResponse(h_request: *mut c_void, lp_reserved: *mut c_void) -> i32;
    fn WinHttpQueryHeaders(
        h_request: *mut c_void,
        dw_info_level: u32,
        pwsz_name: *const u16,
        lp_buffer: *mut c_void,
        lpdw_buffer_length: *mut u32,
        lpdw_index: *mut u32,
    ) -> i32;
    fn WinHttpReadData(
        h_request: *mut c_void,
        lp_buffer: *mut c_void,
        dw_number_of_bytes_to_read: u32,
        lpdw_number_of_bytes_read: *mut u32,
    ) -> i32;
    fn WinHttpCloseHandle(h_internet: *mut c_void) -> i32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn ShellExecuteW(
        hwnd: *mut c_void,
        lp_operation: *const u16,
        lp_file: *const u16,
        lp_parameters: *const u16,
        lp_directory: *const u16,
        n_show_cmd: i32,
    ) -> *mut c_void;
}

const WINHTTP_ACCESS_TYPE_DEFAULT_PROXY: u32 = 0;
const WINHTTP_FLAG_SECURE: u32 = 0x0080_0000;
const WINHTTP_QUERY_STATUS_CODE: u32 = 19;
const WINHTTP_QUERY_FLAG_NUMBER: u32 = 0x2000_0000;
const SW_SHOWNORMAL: i32 = 1;
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// 拆解后的 URL。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Url {
    host: String,
    port: u16,
    path: String,
    secure: bool,
}

fn split_url(url: &str) -> Result<Url, String> {
    let (secure, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(format!("URL 必须以 http:// 或 https:// 开头：{url}"));
    };
    // 主体到第一个 '/' 之前是 host[:port]，之后是 path（含 query）。
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(format!("URL 缺少主机名：{url}"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (
            h.to_string(),
            p.parse::<u16>().map_err(|_| "端口非法".to_string())?,
        ),
        _ => (authority.to_string(), if secure { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(format!("URL 缺少主机名：{url}"));
    }
    Ok(Url {
        host,
        port,
        path: path.to_string(),
        secure,
    })
}

/// GET 一个 URL，返回响应体文本（UTF-8）。非 2xx 或网络失败返回 `Err`。
pub fn get_string(url: &str, timeout_ms: u32) -> Result<String, String> {
    let u = split_url(url)?;
    let agent = to_wide("QLRust-Update/1");
    let host = to_wide(&u.host);
    let path = to_wide(&u.path);
    let timeout = timeout_ms.min(i32::MAX as u32) as i32;

    unsafe {
        let session = WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        );
        if session.is_null() {
            return Err("WinHttpOpen 失败".to_string());
        }

        let result = (|| -> Result<String, String> {
            let _ = WinHttpSetTimeouts(session, timeout, timeout, timeout, timeout);
            let conn = WinHttpConnect(session, host.as_ptr(), u.port, 0);
            if conn.is_null() {
                return Err("WinHttpConnect 失败".to_string());
            }
            let flags = if u.secure { WINHTTP_FLAG_SECURE } else { 0 };
            let req = WinHttpOpenRequest(
                conn,
                std::ptr::null(), // 默认 GET
                path.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                flags,
            );
            if req.is_null() {
                let _ = WinHttpCloseHandle(conn);
                return Err("WinHttpOpenRequest 失败".to_string());
            }

            let outcome = (|| -> Result<String, String> {
                if WinHttpSendRequest(req, std::ptr::null(), 0, std::ptr::null_mut(), 0, 0, 0) == 0
                {
                    return Err("WinHttpSendRequest 失败".to_string());
                }
                if WinHttpReceiveResponse(req, std::ptr::null_mut()) == 0 {
                    return Err("WinHttpReceiveResponse 失败".to_string());
                }
                // 查 HTTP 状态码（数字形式）。
                let mut status: u32 = 0;
                let mut len: u32 = std::mem::size_of::<u32>() as u32;
                let _ = WinHttpQueryHeaders(
                    req,
                    WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                    std::ptr::null(),
                    &mut status as *mut u32 as *mut c_void,
                    &mut len,
                    std::ptr::null_mut(),
                );
                if !(200..300).contains(&status) {
                    return Err(format!("HTTP 状态码 {status}"));
                }
                // 读响应体（分块，上限 1 MB）。
                let mut body: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let mut read: u32 = 0;
                    let ok = WinHttpReadData(
                        req,
                        chunk.as_mut_ptr() as *mut c_void,
                        chunk.len() as u32,
                        &mut read,
                    );
                    if ok == 0 || read == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..read as usize]);
                    if body.len() >= MAX_BODY_BYTES {
                        break;
                    }
                }
                String::from_utf8(body).map_err(|e| format!("响应不是 UTF-8：{e}"))
            })();

            let _ = WinHttpCloseHandle(req);
            let _ = WinHttpCloseHandle(conn);
            outcome
        })();

        let _ = WinHttpCloseHandle(session);
        result
    }
}

/// 用系统默认浏览器打开 URL。成功返回 `true`。
pub fn open_url(url: &str) -> bool {
    if url.trim().is_empty() {
        return false;
    }
    let file = to_wide(url);
    let op = to_wide("open");
    unsafe {
        let h = ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
        (h as isize) > 32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_https_default_port_and_root() {
        let u = split_url("https://example.com").unwrap();
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, 443);
        assert_eq!(u.path, "/");
        assert!(u.secure);
    }

    #[test]
    fn split_with_path_query_and_explicit_port() {
        let u = split_url("http://example.com:8080/a/b?x=1").unwrap();
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, 8080);
        assert_eq!(u.path, "/a/b?x=1");
        assert!(!u.secure);
    }

    #[test]
    fn split_rejects_missing_scheme() {
        assert!(split_url("example.com/x").is_err());
        assert!(split_url("ftp://example.com").is_err());
    }
}
