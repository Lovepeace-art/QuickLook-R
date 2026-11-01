//! 取消信号（1.2.0）：让"用户已经换文件了"这件事能被正在跑的渲染看见。
//!
//! 为什么用全局原子而不是给每处渲染传一个 `&AtomicBool`：
//! 渲染只有**一个**工作线程（`qlr-render`），同一时刻只有一个活在跑，
//! 所以"当前活是否作废"是一个全局事实。传参数会污染
//! `render_file_at_zoom` → `Viewer::render` → 每个渲染器的签名，
//! 换来的只是"未来可能并行渲染"的假想收益。
//!
//! 用法：
//! - 主线程发新请求时 [`cancel`]（旧活自己会在下一个检查点退出）；
//! - 工作线程拿到最新请求后 [`reset`]；
//! - 渲染器在**阶段边界**调 [`checkpoint`]（解码前/解码后/缩放前/每 N 行……）。
//!
//! 必须清楚的边界：单个 FFI 调用（`FPDF_RenderPageBitmap`、`image` 的一次解码）
//! 中途是打断不了的。检查点只能把"多阶段"的活提前结束，把等待从"整份文件"
//! 缩短到"一个阶段"。这条限制写进 `docs/dev/ARCH.md` 的稳定性一节。

use std::sync::atomic::{AtomicBool, Ordering};

use qlr_core::{Error, Result};

static CANCELLED: AtomicBool = AtomicBool::new(false);

/// 请求取消当前正在跑的活（主线程发新请求时调用）。
pub fn cancel() {
    CANCELLED.store(true, Ordering::Relaxed);
}

/// 清掉取消信号（工作线程准备开跑最新活时调用）。
pub fn reset() {
    CANCELLED.store(false, Ordering::Relaxed);
}

/// 当前活是否已被取消。
pub fn is_cancelled() -> bool {
    CANCELLED.load(Ordering::Relaxed)
}

/// 检查点：已取消就返回 [`Error::Cancelled`]，让调用链一路 `?` 退出。
pub fn checkpoint() -> Result<()> {
    if is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_and_reset_round_trip() {
        reset();
        assert!(!is_cancelled());
        assert!(checkpoint().is_ok());

        cancel();
        assert!(is_cancelled());
        let err = checkpoint().expect_err("取消后 checkpoint 必须报错");
        assert!(err.is_cancelled());
        assert!(err.to_string().contains("已取消"));

        // 必须能复位：否则一次取消之后所有渲染都短路。
        reset();
        assert!(checkpoint().is_ok());
    }
}
