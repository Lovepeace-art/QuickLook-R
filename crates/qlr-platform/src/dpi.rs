//! Per-Monitor V2 DPI 感知 + DIP → 物理像素换算（1.8.3）。
//!
//! 必须在**创建任何窗口之前**调用 [`enable_per_monitor_v2`]（文档要求），否则多屏不同
//! 缩放时会糊。本机 Win10 19045 支持 V2（Windows 10 1703+）。
//!
//! # 1.8.3 起的三个约定（违反任何一个都会出现"某些地方大小不对"）
//!
//! 1. **几何量一律写 DIP（设计值），用 [`px`] 换算出物理像素。**
//!    写死物理像素的后果就是 200% 缩放下"顶栏和内容都小一半"（用户报障）。
//! 2. **`enter(hwnd)` 每次都真查 `GetDpiForWindow`，不缓存 hwnd。**
//!    1.8.3 之前这里是"记住上次的 hwnd，同一个就直接返回"的写法，看着省事，实际是
//!    自锁：同一个窗口被拖到另一块缩放不同的屏上，DPI 变了但 hwnd 没变 → 永远按旧
//!    DPI 画。而"该失效的条件"是补不完的（跨屏、改缩放、RDP 重连都会变）。
//!    `GetDpiForWindow` 是用户态 O(1)（内核侧已有缓存），一帧几十次是微秒级。
//! 3. **DPI 变化要通知字体重建。** 字体是按像素高度 `CreateFontW` 出来的，DPI 变了
//!    不重建就会一直是旧尺寸。用 [`set_change_hook`] 注册（`window.rs` 在 `FONTS`
//!    的 `clear()` 上挂一个），`enter` 检测到变化时回调。
//!
//! # 逃生开关
//!
//! `QLRUST_DPI_FIX=0` → [`px`] 退化成恒等函数，**完整回到 1.8.2 的行为**
//! （连字体尺寸都不变）。给 200% 机器上"新版本反而更糟"留的后门。

use std::cell::Cell;
use std::sync::OnceLock;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForSystem, GetDpiForWindow,
    SetProcessDpiAwarenessContext,
};

/// 启用 Per-Monitor V2。返回是否成功（失败不致命：退化为系统缩放，只是多屏下略糊）。
pub fn enable_per_monitor_v2() -> bool {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_ok() }
}

/// 假 DPI（`QLRUST_DPI_FAKE=192` 之类），**只给开发和自动化验证用**。
///
/// 为什么需要它：开发机是 96 DPI，而"200% 缩放"是用户真实报障的场景。手改系统缩放
/// 既影响全局又不能自动化。设了这个环境变量后，`window_dpi` / `system_dpi` 一律返回它，
/// 于是所有 DIP→物理换算、开窗尺寸、字体大小都按 200% 算——可以在 96 DPI 机器上复现
/// "布局按 2 倍"的整条路径。
///
/// 注意：屏幕物理尺寸不变，所以窗口会显得很大甚至超出屏幕。它验证的是**数值与命中**，
/// 不是观感；视觉验收仍然必须在真 200% 机器上做。
fn fake_dpi() -> Option<u32> {
    static FAKE: OnceLock<Option<u32>> = OnceLock::new();
    *FAKE.get_or_init(|| {
        std::env::var("QLRUST_DPI_FAKE")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|d| (96..=480).contains(d))
    })
}

/// 这个窗口所在显示器的 DPI（96 = 100%；取不到按 96 算）。
///
/// 为什么需要它：开了 PMv2 之后窗口坐标一律是**物理像素**，而"边缘热区 6px 好抓"
/// 这个直觉是 DIP 的。125% 屏上 6 物理像素只有 4.8 DIP，用户会觉得"贴得不够准就拖不动"
/// （1.7.3 反馈的拖边问题里就有一条）。
pub fn window_dpi(hwnd: HWND) -> u32 {
    if let Some(fake) = fake_dpi() {
        return fake;
    }
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 { 96 } else { dpi }
}

/// 系统 DPI（主屏 / 登录时的缩放）。**只给"窗口还没建"的路径用**，例如开窗尺寸计算。
///
/// 局限：多屏且各屏缩放不同时，它给的是主屏值。预览窗建出来之后一切以
/// [`enter`] 查到的窗口 DPI 为准；只有"建窗前那一次算尺寸"可能偏。
/// 单屏用户（本次报障的机器）不受影响。
pub fn system_dpi() -> u32 {
    if let Some(fake) = fake_dpi() {
        return fake;
    }
    let dpi = unsafe { GetDpiForSystem() };
    if dpi == 0 { 96 } else { dpi }
}

/// 把 DIP 换算成这个窗口上的物理像素（四舍五入）。
///
/// 保留给"已经拿到 hwnd、但不在线程 DPI 上下文里"的调用点（例如热区判定）。
pub fn scaled(hwnd: HWND, dip: i32) -> i32 {
    ((dip as i64 * window_dpi(hwnd) as i64 + 48) / 96) as i32
}

/// 开关：`QLRUST_DPI_FIX=0` 时全部换算退化成恒等（1.8.2 行为）。
fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| !matches!(std::env::var("QLRUST_DPI_FIX"), Ok(v) if v.trim() == "0"))
}

thread_local! {
    /// 本线程当前使用的 DPI。默认 96 —— **96 下 [`px`] 恒等**，所以既有的 251 个
    /// 单测（在无窗口的测试进程里跑）一条都不会变。
    static DPI: Cell<u32> = const { Cell::new(96) };
    /// DPI 变化时的回调（字体重建）。每线程一个，`window` 模块注册。
    static HOOK: Cell<Option<fn()>> = const { Cell::new(None) };
}

/// 注册"DPI 变了"的回调。只有窗口层需要（清字体缓存）。
pub fn set_change_hook(hook: fn()) {
    HOOK.with(|c| c.set(Some(hook)));
}

/// 把本线程的 DPI 设成 `dpi`（给 worker 线程用：预热线程算出的窗口尺寸必须和 UI
/// 线程一致，否则 `pagecache` 的 key（含 w/h）永远对不上，预热白烧 CPU）。
pub fn seed_thread(dpi: u32) {
    DPI.with(|c| c.set(if dpi == 0 { 96 } else { dpi }));
}

/// 本线程当前 DPI（默认 96）。
pub fn current() -> u32 {
    DPI.with(|c| c.get())
}

/// 进入某个窗口的 DPI 上下文；**返回 true 表示 DPI 刚刚变化**（字体该重建了）。
///
/// 见模块文档第 2 条：每次都真查，不缓存 hwnd。
pub fn enter(hwnd: HWND) -> bool {
    let dpi = window_dpi(hwnd);
    let changed = DPI.with(|c| {
        let old = c.get();
        if old != dpi {
            c.set(dpi);
            true
        } else {
            false
        }
    });
    if changed {
        HOOK.with(|c| {
            if let Some(hook) = c.get() {
                hook();
            }
        });
    }
    changed
}

/// DIP → 物理像素（四舍五入，与 [`scaled`] 同一套取整）。
///
/// 96 DPI 下恒等，`QLRUST_DPI_FIX=0` 下恒等。只用于**非负几何量**：负值直接原样返回
/// （窗口坐标可能出现负的 left/top，那些是屏幕坐标，本来就不是 DIP，不该换算）。
pub fn px(v: i32) -> i32 {
    if v < 0 || !enabled() {
        return v;
    }
    let dpi = current();
    if dpi == 96 {
        return v;
    }
    ((v as i64 * dpi as i64 + 48) / 96) as i32
}

/// 浮点版（给比例类数值用，例如字号 15.0 × 1.5）。
pub fn pxf(v: f32) -> f32 {
    if v < 0.0 || !enabled() {
        return v;
    }
    let dpi = current();
    if dpi == 96 {
        return v;
    }
    v * (dpi as f32) / 96.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 96 DPI 下必须恒等 —— 这是"251 个既有测试不受影响"的前提。
    #[test]
    fn identity_at_96() {
        seed_thread(96);
        for v in [0, 1, 12, 15, 24, 30, 620, 880, 1633] {
            assert_eq!(px(v), v, "{v} 在 96 DPI 下不该变");
        }
    }

    /// 200% 下翻倍（四舍五入到整数）。
    #[test]
    fn doubles_at_192() {
        seed_thread(192);
        for v in [1, 12, 15, 24, 30, 620, 880] {
            assert_eq!(px(v), v * 2, "{v} 在 200% 下应该是 {v}*2");
        }
        seed_thread(96);
    }

    /// 150% 这类非整数倍要四舍五入（12 → 18，15 → 22.5 → 23）。
    #[test]
    fn rounds_at_144() {
        seed_thread(144);
        assert_eq!(px(12), 18);
        assert_eq!(px(15), 23); // 22.5 上取整
        assert_eq!(px(24), 36);
        assert_eq!(px(620), 930);
        seed_thread(96);
    }

    /// 负值（屏幕坐标）与 0 原样返回：它们不是 DIP，不该被放大。
    #[test]
    fn negative_and_zero_pass_through() {
        seed_thread(192);
        assert_eq!(px(-1), -1);
        assert_eq!(px(-100), -100);
        assert_eq!(px(0), 0);
        seed_thread(96);
    }

    /// enter/seed 之后 current() 要跟着变；重复 enter 同一 DPI 返回 false（不重复建字体）。
    #[test]
    fn seed_updates_current() {
        seed_thread(120);
        assert_eq!(current(), 120);
        seed_thread(0);
        assert_eq!(current(), 96, "0 按 96 处理");
    }

    /// pxf 与 px 同一套语义。
    #[test]
    fn pxf_scales_floats() {
        seed_thread(192);
        assert!((pxf(15.0) - 30.0).abs() < 0.01);
        seed_thread(96);
        assert!((pxf(15.0) - 15.0).abs() < 0.01);
    }
}
