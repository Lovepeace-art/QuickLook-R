//! 折行结果缓存（1.8.3-fix 批次四，结构评审 §6-6）。
//!
//! **为什么要它**：折行的成本全在"量宽"上——一段 340 字的段落，二分折行要 8 次 GDI
//! `GetTextExtent`，一屏 29 行里只有 4 个长行，但就是这 4 行要 243 次量宽 / 5.1 ms
//! （改成二分之前是 1359 次 / 12.1 ms）。而滚一格只有最后几行是新的，前 20 多行的
//! 折行结果**一模一样**——不缓存就是每滚一格重算一遍同样的东西。
//!
//! **短行也缓存**：一行放得下的行只要 1 次量宽，但滚一格会有 20 多行重复出现，
//! 缓存它们等于把那 20 多次量宽全省掉（实测日志/代码类文档每帧 29 次 → 5 次）。
//!
//! **什么时候整本作废**：换内容（`clear`）、改宽度（`set_width`，拖窗/缩放让所有
//! 折行结果都不再成立）。样式变了但内容没换的情况不存在——样式随内容一起下发。

use std::collections::HashMap;
use std::sync::Arc;

/// 最多缓存多少条原始行。
///
/// 一屏 29 行，滚一格换 5 行 → 512 条足够覆盖"来来回回滚"的工作集，
/// 每条最多 `wrap::MAX_ROWS_PER_LINE`（400）行文字的引用，实际占用是几十 KB 量级。
pub const CAP: usize = 512;

/// 一条折行结果：`(样式下标, 折出来的每一行)`。
type Entry = (u8, Arc<Vec<String>>);

/// 折行缓存。只在 UI 线程上用（`STATE` 里跟着 `State` 走）。
#[derive(Default)]
pub struct WrapCache {
    width: u32,
    entries: HashMap<usize, Entry>,
    hits: u64,
    misses: u64,
    doc: u64,
}

impl WrapCache {
    /// 换内容：所有折行结果都作废。
    pub fn clear(&mut self) {
        if !self.entries.is_empty() {
            self.entries.clear();
        }
        self.doc = self.doc.wrapping_add(1);
    }

    /// 记录当前折行宽度；变了就作废（折行结果只对那个宽度成立）。
    pub fn set_width(&mut self, width: u32) {
        if self.width != width {
            self.width = width;
            self.entries.clear();
        }
    }

    /// 取一条折行结果（`None` = 没缓存过）。
    pub fn get(&mut self, line: usize, style: u8) -> Option<Arc<Vec<String>>> {
        match self.entries.get(&line) {
            Some((s, rows)) if *s == style => {
                self.hits += 1;
                Some(Arc::clone(rows))
            }
            _ => {
                self.misses += 1;
                None
            }
        }
    }

    /// 存一条折行结果（单行的也存：短行的量宽虽只要一次，但滚一格会有 20 多行重复）。
    pub fn put(&mut self, line: usize, style: u8, rows: Vec<String>) {
        if self.entries.len() >= CAP && !self.entries.contains_key(&line) {
            // 简单策略：满了就整本清空。比起 LRU 链表，这里更看重"代码短、没状态"
            // ——缓存本身只值几毫秒，维护顺序的复杂度不值。
            self.entries.clear();
        }
        self.entries.insert(line, (style, Arc::new(rows)));
    }

    /// `(命中, 未命中, 条目数, 内容代际)`：日志用。
    pub fn stats(&self) -> (u64, u64, usize, u64) {
        (self.hits, self.misses, self.entries.len(), self.doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_row_results_are_cached_too() {
        let mut c = WrapCache::default();
        c.put(3, 0, vec!["一行".to_string()]);
        assert_eq!(c.get(3, 0).map(|v| v.len()), Some(1), "单行结果也要缓存");
        assert_eq!(c.stats().2, 1);
    }

    #[test]
    fn cache_hits_by_line_and_style() {
        let mut c = WrapCache::default();
        c.put(3, 1, vec!["a".into(), "b".into()]);
        assert_eq!(c.get(3, 1).map(|v| v.len()), Some(2));
        assert!(c.get(3, 0).is_none(), "样式不同不能命中");
        assert!(c.get(4, 1).is_none(), "行号不同不能命中");
        let (hits, misses, _, _) = c.stats();
        assert_eq!((hits, misses), (1, 2));
    }

    #[test]
    fn width_change_and_clear_invalidate() {
        let mut c = WrapCache::default();
        c.set_width(800);
        c.put(1, 0, vec!["a".into(), "b".into()]);
        assert!(c.get(1, 0).is_some());
        c.set_width(600);
        assert!(c.get(1, 0).is_none(), "改宽度后折行结果不再成立");

        c.put(1, 0, vec!["a".into(), "b".into()]);
        c.clear();
        assert!(c.get(1, 0).is_none(), "换内容后必须作废");
    }

    #[test]
    fn cache_is_bounded() {
        let mut c = WrapCache::default();
        for i in 0..(CAP + 10) {
            c.put(i, 0, vec!["a".into(), "b".into()]);
        }
        assert!(
            c.stats().2 <= CAP,
            "缓存条目 {} 超过上限 {CAP}",
            c.stats().2
        );
    }
}
