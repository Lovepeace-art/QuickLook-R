//! 进程内"可丢缓存"的**总**预算。
//!
//! ## 为什么需要这一层
//!
//! 1.8.3 之前每处缓存都只看自己那一份上限：源图缓存 64 MB、PDF 页缓存 64 MB、
//! 上一帧 24 MB、PDF 文档槽 256 MB（1.8.4 批次二加的）。**上限是加法，不是上限**——
//! 四个都吃满就是 400 MB 以上，再叠一帧 32 MP 的位图（128 MB）能摸到 500 MB。
//! 用户看到的不是"某个缓存超了"，而是"预览器怎么占着几百兆内存不还"。
//!
//! 这一层只做一件事：给所有可丢缓存**一把共同的尺子**。谁要常驻 N 字节就先
//! [`try_reserve`]，拿到 [`Permit`] 才算数；`Permit` 一 drop（条目被淘汰、文档被关、
//! 换文件清缓存）就自动归还。**没有中心调度器、没有回收回调**：拿不到额度的那一方
//! 自己先淘汰自家的旧条目再试，还不行就不缓存——缓存只影响快慢，不影响对错，
//! 所以"这回不缓存"永远是可接受的答案。
//!
//! ## 为什么放在 `qlr-render`
//!
//! 三处缓存里两处（源图、PDF 文档槽）就在本 crate，第三处（壳层的 PDF 页缓存）存的
//! 也是本 crate 产出的 `Canvas`。尺子放在"产出这些东西"的一侧，比放在壳层更不容易漂。
//!
//! ## 谁**不**参与
//!
//! - 正在渲染的那一帧：必要的临时内存，不是缓存，拦它等于让预览失败；
//! - 上一帧（`qlr-app` 的 `LAST_FRAME_*`，最多 3 帧 / 24 MB）：它失败的模式是"重开同一个
//!   文件要重渲染"，而 24 MB 相对 256 MB 只是零头，接进来只多一层记账；
//! - 插件（`qlr-plugin-*`）：跑在自己的 dll 里，宿主看不到它的分配；
//! - `PaintBuffer` 等壳层合成缓冲：每帧复用、体积固定，量级远小于上面这些。

use std::sync::atomic::{AtomicUsize, Ordering};

/// 所有可丢缓存的合计上限：256 MB。
///
/// 取这个数的理由（对着 1.8.4 实测的四笔账）：
/// - 一张 4K 截图解码成 RGBA 是 33 MB，源图缓存 64 MB 装得下两张；
/// - PDF 一份 20 MB 的扫描件常驻 20 MB、100 MB 的常驻 100 MB（换来每次打开省
///   250 ms —— 实测冷加载 264.63 ms vs 命中 0.02 ms）；
/// - PDF 页缓存 64 MB ≈ 28 页 631×893；
/// - 上一帧 24 MB（不参与本预算）。
///
/// 105 MB 的文档 + 两个 64 MB 缓存 = 233 MB，正好在这个数内；再大的文档就只留
/// "本次打开用得上"，不再常驻（省 250 ms 不值得常驻 200 MB）。
///
/// 这条线跟 `qlr-app` 里的 `MAX_RENDER_PIXELS`（32 MP ≈ 128 MB）无关：
/// 那一帧是渲染中的临时内存，不在本预算的账上。
pub const LIMIT_BYTES: usize = 256 * 1024 * 1024;

/// 一本账：当前占了多少字节 + 上限多少。
///
/// 生产代码只用下面那个 [`GLOBAL`]；单测用 `Budget::new(小上限)` 自己开一本、互不干扰。
/// （全局账是进程共享的，单测去改它会让并行跑的别的用例莫名其妙拿不到额度。）
pub struct Budget {
    used: AtomicUsize,
    limit: usize,
}

impl Budget {
    /// 建一本上限为 `limit` 字节的账。
    pub const fn new(limit: usize) -> Self {
        Self {
            used: AtomicUsize::new(0),
            limit,
        }
    }

    /// 申请 `bytes` 字节额度；装不下就 `None`（调用方自己淘汰旧条目后再来一次）。
    ///
    /// `bytes == 0` 永远成功——空条目占不到内存，没必要让它因为预算耗尽而进不来。
    pub fn try_reserve(&self, bytes: usize) -> Option<Permit<'_>> {
        if bytes == 0 {
            return Some(Permit {
                budget: self,
                bytes: 0,
            });
        }
        let mut used = self.used.load(Ordering::Relaxed);
        loop {
            let next = used.checked_add(bytes)?;
            if next > self.limit {
                return None;
            }
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => {
                    return Some(Permit {
                        budget: self,
                        bytes,
                    });
                }
                Err(actual) => used = actual,
            }
        }
    }

    /// 已经占掉多少字节。
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    /// 上限。
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// 还剩多少字节。
    pub fn available(&self) -> usize {
        self.limit.saturating_sub(self.used())
    }

    /// 一行给人看的摘要（日志用）：`used=217MB cap=256MB`。
    pub fn summary(&self) -> String {
        format!(
            "used={}MB cap={}MB",
            self.used() / (1024 * 1024),
            self.limit() / (1024 * 1024)
        )
    }
}

/// 进程唯一的全局账。
static GLOBAL: Budget = Budget::new(LIMIT_BYTES);

/// 一次成功的额度申请。**drop 即归还**，不要 `mem::forget`。
pub struct Permit<'a> {
    budget: &'a Budget,
    bytes: usize,
}

impl Permit<'_> {
    /// 这份额度占了多少字节。
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// 向全局账申请 `bytes` 字节额度；装不下就 `None`。
pub fn try_reserve(bytes: usize) -> Option<Permit<'static>> {
    GLOBAL.try_reserve(bytes)
}

/// 全局账已经占掉多少字节。
pub fn used() -> usize {
    GLOBAL.used()
}

/// 全局账的总上限。
pub fn limit() -> usize {
    GLOBAL.limit()
}

/// 全局账还剩多少字节。
pub fn available() -> usize {
    GLOBAL.available()
}

/// 全局账的一行摘要。
pub fn summary() -> String {
    GLOBAL.summary()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: usize = 1024 * 1024;

    #[test]
    fn reserve_then_drop_returns_everything() {
        let budget = Budget::new(1024);
        let permit = budget.try_reserve(1000).expect("装得下");
        assert_eq!(budget.used(), 1000);
        assert_eq!(permit.bytes(), 1000);
        drop(permit);
        assert_eq!(budget.used(), 0, "drop 必须把额度还回去");
    }

    #[test]
    fn over_limit_is_refused_and_nothing_is_charged() {
        let budget = Budget::new(1024);
        assert!(budget.try_reserve(1025).is_none());
        assert_eq!(budget.used(), 0, "被拒的申请不许记账");
        // 正好等于上限可以拿。
        let all = budget.try_reserve(1024).expect("刚好装满");
        assert_eq!(budget.available(), 0);
        assert!(budget.try_reserve(1).is_none(), "满了再要 1 字节也得拒");
        drop(all);
        assert_eq!(budget.available(), 1024);
    }

    #[test]
    fn zero_bytes_always_succeeds() {
        let budget = Budget::new(64);
        let _all = budget.try_reserve(64).expect("装满");
        let zero = budget.try_reserve(0).expect("0 字节不受预算限制");
        assert_eq!(zero.bytes(), 0);
        assert_eq!(budget.used(), 64);
    }

    #[test]
    fn several_permits_add_up() {
        let budget = Budget::new(256 * MB);
        let a = budget.try_reserve(10 * MB).expect("第一份");
        let b = budget.try_reserve(20 * MB).expect("第二份");
        assert_eq!(budget.used(), 30 * MB);
        assert_eq!(budget.available(), 226 * MB);
        drop(b);
        assert_eq!(budget.used(), 10 * MB);
        drop(a);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn summary_is_human_readable() {
        let budget = Budget::new(256 * MB);
        let _p = budget.try_reserve(64 * MB).expect("64 MB");
        assert_eq!(budget.summary(), "used=64MB cap=256MB");
    }

    /// 这条用例把"上限是加法"这件事钉死：三笔账加起来 233 MB 进得来，
    /// 换一份 200 MB 的文档就进不来——这正是上一版会吃满 400 MB 的路径。
    #[test]
    fn the_three_caches_fit_or_the_big_document_stays_out() {
        let budget = Budget::new(LIMIT_BYTES);
        let doc = budget.try_reserve(105 * MB).expect("105 MB 文档");
        let source = budget.try_reserve(64 * MB).expect("源图缓存");
        let pages = budget.try_reserve(64 * MB).expect("页缓存");
        assert_eq!(budget.used(), 233 * MB);
        drop((doc, source, pages));
        assert_eq!(budget.used(), 0);

        let big = budget.try_reserve(200 * MB).expect("200 MB 文档");
        assert!(budget.try_reserve(64 * MB).is_none(), "挤掉源图缓存的位置");
        drop(big);
        assert!(budget.try_reserve(64 * MB).is_some(), "文档放掉就又装得下");
    }

    /// 全局账只用只读方式碰一下（不 reserve，不影响并行跑的别的用例）。
    #[test]
    fn global_budget_has_the_documented_limit() {
        assert_eq!(limit(), 256 * MB);
        assert!(used() <= limit());
        assert_eq!(available(), limit() - used());
        assert!(summary().starts_with("used="), "{}", summary());
    }
}
