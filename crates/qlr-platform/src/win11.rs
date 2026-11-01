//! Win11 观感增强：DWM 圆角 + 系统 backdrop（Mica 家族）。
//!
//! **本模块只管 Win11 的圆角与 backdrop**（历史：以前还有一个 Win10 亚克力模块，已于 1.5.0 删除）。
//! - Win10（≤ 19045）：`DWMWA_WINDOW_CORNER_PREFERENCE` / `DWMWA_SYSTEMBACKDROP_TYPE`
//!   两个属性都**不存在**，调用会返回 `E_INVALIDARG`（0x80070057）。所以 Win10 上
//!   **不做任何材质**，退回纯色背景。
//! - Win11（≥ 22000）：官方属性可用，圆角和 backdrop 都由 DWM 合成，效果比自绘/亚克力
//!   更贴合系统，且跟随"透明效果"开关。
//!
//! 本模块的全部函数都返回 `bool`/结构体，**取不到就不做**，绝不 panic、绝不弹窗
//! （约定见 `docs/dev/ARCH.md`「材质通道」）。
//!
//! 版本号来源是 `ntdll!RtlGetVersion`，**不用** `GetVersionEx`：
//! 后者会被 exe 清单里的 `<compatibility>` 段骗（我们自己的清单就声明了 Win10），
//! `RtlGetVersion` 拿的是内核真实版本。

use std::sync::OnceLock;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{DWMWINDOWATTRIBUTE, DwmSetWindowAttribute};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::core::PCSTR;

use crate::wide::to_wide;

/// `DWMWA_WINDOW_CORNER_PREFERENCE`
const DWMWA_WINDOW_CORNER_PREFERENCE: DWMWINDOWATTRIBUTE = DWMWINDOWATTRIBUTE(33);
/// `DWMWA_SYSTEMBACKDROP_TYPE`
const DWMWA_SYSTEMBACKDROP_TYPE: DWMWINDOWATTRIBUTE = DWMWINDOWATTRIBUTE(38);
/// `DWM_WINDOW_CORNER_PREFERENCE::DWMWCP_ROUND`
const DWMWCP_ROUND: i32 = 2;
/// `DWM_SYSTEMBACKDROP_TYPE::DWMSBT_TRANSIENTWINDOW`（Win11 22H2 的"亚克力"）
const DWMSBT_TRANSIENTWINDOW: i32 = 3;

/// Win11 首个正式版本的 build 号。
const WIN11_BUILD: u32 = 22000;

/// `RTL_OSVERSIONINFOW`（只需要 `dwBuildNumber`，但字段顺序必须完全对齐）。
#[repr(C)]
struct OsVersionInfoW {
    os_version_info_size: u32,
    major_version: u32,
    minor_version: u32,
    build_number: u32,
    platform_id: u32,
    csd_version: [u16; 128],
}

// `[u16; 128]` 没有 `Default` 实现（只有 ≤32 才有），所以手写。
impl Default for OsVersionInfoW {
    fn default() -> Self {
        Self {
            os_version_info_size: 0,
            major_version: 0,
            minor_version: 0,
            build_number: 0,
            platform_id: 0,
            csd_version: [0; 128],
        }
    }
}

type RtlGetVersion = unsafe extern "system" fn(*mut OsVersionInfoW) -> i32;

/// 本机 build 号（取不到返回 0）。
pub fn build_number() -> u32 {
    static CACHE: OnceLock<u32> = OnceLock::new();
    *CACHE.get_or_init(|| unsafe {
        let ntdll = to_wide("ntdll.dll");
        let Ok(module) = GetModuleHandleW(windows::core::PCWSTR(ntdll.as_ptr())) else {
            return 0;
        };
        let Some(proc) = GetProcAddress(module, PCSTR(c"RtlGetVersion".as_ptr().cast())) else {
            return 0;
        };
        let func: RtlGetVersion = std::mem::transmute(proc);
        let mut info = OsVersionInfoW {
            os_version_info_size: std::mem::size_of::<OsVersionInfoW>() as u32,
            ..Default::default()
        };
        if func(&mut info) == 0 {
            info.build_number
        } else {
            0
        }
    })
}

/// 是否跑在 Win11（含 21H2）或更新的系统上。
pub fn is_windows11() -> bool {
    build_number() >= WIN11_BUILD
}

/// 一次性应用的全部效果（字段为 `true` 表示**这一项真的设上了**）。
///
/// 调用方在日志里打一遍即可；不需要在别处再判断版本。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effects {
    pub windows11: bool,
    pub rounded: bool,
    pub backdrop: bool,
}

impl Effects {
    /// 有没有任何一项生效。
    pub fn any(&self) -> bool {
        self.rounded || self.backdrop
    }
}

/// 给窗口加上圆角 + 系统 backdrop。Win10 上返回全 `false`（静默降级）。
///
/// 可以重复调用：窗口每次重新显示都会重新套一次，成本是两条 `DwmSetWindowAttribute`。
pub fn apply(hwnd: HWND) -> Effects {
    let windows11 = is_windows11();
    if !windows11 {
        return Effects::default();
    }
    let rounded = set_int(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND);
    let backdrop = set_int(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, DWMSBT_TRANSIENTWINDOW);
    Effects {
        windows11,
        rounded,
        backdrop,
    }
}

/// 只取消 backdrop（显示位图时必须关掉，否则整窗内容被混色：
/// 这就是 0.1.1 "图片被提亮" 那个 bug 的同一个坑，见 `CONVENTIONS.md`）。
///
/// 圆角保留——位图上加圆角是纯粹的视觉收益，不影响像素。
pub fn clear_backdrop(hwnd: HWND) -> bool {
    is_windows11() && set_int(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, 1) // DWMSBT_NONE
}

fn set_int(hwnd: HWND, attribute: DWMWINDOWATTRIBUTE, value: i32) -> bool {
    unsafe { DwmSetWindowAttribute(hwnd, attribute, (&raw const value).cast(), 4).is_ok() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_version_struct_layout_matches_win32() {
        // RTL_OSVERSIONINFOW：5 个 DWORD + 128 个 WCHAR。
        assert_eq!(
            std::mem::size_of::<OsVersionInfoW>(),
            5 * 4 + 128 * 2,
            "RtlGetVersion 会按这个尺寸校验，错一个字节就返回 STATUS_BUFFER_TOO_SMALL"
        );
    }

    #[test]
    fn effects_report_no_effect_on_default() {
        let e = Effects::default();
        assert!(!e.any());
        assert!(!e.windows11);
    }

    #[test]
    fn build_number_is_plausible_on_this_machine() {
        // 取到就应该是 Win10+ 的 build（本机 19045）；取不到是 0（NTDLL 取符号失败）。
        let build = build_number();
        assert!(
            build == 0 || build >= 7600,
            "build={build} 看起来不像 Windows 版本号"
        );
    }
}
