//! 帧级分配计数（只在 `alloc-stats` 特性下编进二进制）。
//!
//! 为什么需要它：`RENDER` 行只报"位图多大"（`bytes=`），报不出"这一帧一共
//! 分配了多少字节"。一帧里除了位图，还有缩放中间缓冲、文本布局字符串、
//! `Vec` 扩容、日志格式化 —— 这些加起来常常比位图本身还大，而 malloc/free
//! 的成本直接体现成掉帧与内存抖动。评审报告 R5「GDI 对象每帧重建」指的就是
//! 这一类"看着没多大、每帧都重来"的开销，量出来才能谈优化。
//!
//! 默认**不编**：`cargo build --release` 的产物里没有这个分配器，热路径上
//! 不会多一条指令。要看数字时：
//!
//! ```text
//! cargo build --release -p qlr-app --target-dir target\alloc-stats --features alloc-stats
//! ```
//!
//! 然后 `RENDER` 行旁边会多一行 `RENDERALLOC gen=N bytes=…`，`scripts/bench.ps1`
//! 会自动把它并进 `AllocKB` 列。

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static FREED: AtomicUsize = AtomicUsize::new(0);

/// 包一层系统分配器，只记账。
///
/// `Relaxed` 足够：这里只要"量级对得上"的数字，不需要跨线程的严格一致性；
/// 用 `SeqCst` 反而会让每条分配路径多一次内存栅栏。
pub struct Counting;

// SAFETY: 所有方法都直接转给 `System`，只在转交前后各加一次原子计数；
// 计数不改变指针语义，也不改变内存生命周期。
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        FREED.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // realloc 语义上是"释放旧块 + 分配新块"，记账也照这个口径。
        ALLOCATED.fetch_add(new_size, Ordering::Relaxed);
        FREED.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// 进程启动以来累计分配了多少字节（不减去释放）。
pub fn total_allocated() -> usize {
    ALLOCATED.load(Ordering::Relaxed)
}

/// 当前"活着"的分配字节数（累计分配 − 累计释放，可能因为计数口径略偏）。
pub fn live_bytes() -> usize {
    ALLOCATED
        .load(Ordering::Relaxed)
        .saturating_sub(FREED.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_growth() {
        // 分配一块 1 MB 的向量，累计计数应该至少涨 1 MB。
        let before = total_allocated();
        let v = vec![0u8; 1024 * 1024];
        let after = total_allocated();
        assert!(
            after.saturating_sub(before) >= 1024 * 1024,
            "涨了 {} B",
            after - before
        );
        drop(v);
    }
}
