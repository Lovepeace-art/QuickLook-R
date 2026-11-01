//! PDF 页缓存（1.3.0）：把"已经渲染过的页"留在内存里，翻回去时直接上屏。
//!
//! 为什么需要它：PDF 的一页要经过 `FPDF_RenderPageBitmap` 整页光栅化，
//! 631×893 的页面约 7 ms、放大到 2× 约 28 ms、4× 约 60 ms。
//! 用户翻页最常见的动作是"翻过去看一眼、又翻回来"，这一来一回没必要重算。
//!
//! 三个约束：
//! 1. **按 (路径, 页号, 宽, 高) 精确匹配**——尺寸不同就是不同的位图，不能拿来凑。
//! 2. **LRU + 字节上限**：一页 631×893 是 2.25 MB，64 MB 上限约 28 页；
//!    超了就从最久没用的开始丢（宁可重算一次，也不要悄悄吃满内存）。
//! 3. 只缓存 **PDF**：图片/文本的渲染本来就便宜，缓存它们只会让内存变脏。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use qlr_render::Canvas;

/// 缓存上限：64 MB。一页 631×893 约 2.25 MB，够放 ~28 页。
pub const CACHE_LIMIT_BYTES: usize = 64 * 1024 * 1024;

/// 一次命中的 key：路径 + 页号 + 目标尺寸。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageKey {
    pub path: PathBuf,
    pub page: usize,
    pub width: u32,
    pub height: u32,
}

impl PageKey {
    pub fn new(path: &Path, page: usize, width: u32, height: u32) -> Self {
        Self {
            path: path.to_path_buf(),
            page,
            width,
            height,
        }
    }
}

struct Entry {
    key: PageKey,
    canvas: Arc<Canvas>,
    bytes: usize,
}

/// 简单的 LRU：`VecDeque` 头是"最久没用到"，尾是"刚用过"。
pub struct PageCache {
    entries: VecDeque<Entry>,
    bytes: usize,
    limit: usize,
}

impl PageCache {
    pub fn new(limit: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            limit: limit.max(1),
        }
    }

    /// 取一页（命中就把它挪到队尾＝最近使用）。
    pub fn get(&mut self, key: &PageKey) -> Option<Arc<Canvas>> {
        let pos = self.entries.iter().position(|e| &e.key == key)?;
        let entry = self.entries.remove(pos)?;
        let canvas = entry.canvas.clone();
        self.entries.push_back(entry);
        Some(canvas)
    }

    /// 存一页。同一个 key 再来一次就替换（尺寸/内容以新的为准）。
    pub fn put(&mut self, key: PageKey, canvas: Arc<Canvas>) {
        if let Some(pos) = self.entries.iter().position(|e| e.key == key) {
            if let Some(old) = self.entries.remove(pos) {
                self.bytes = self.bytes.saturating_sub(old.bytes);
            }
        }
        let bytes = estimate_bytes(canvas.as_ref());
        // 单页就超上限：不缓存，免得把别的页全挤掉。
        if bytes >= self.limit {
            return;
        }
        self.entries.push_back(Entry { key, canvas, bytes });
        self.bytes += bytes;
        while self.bytes > self.limit {
            let Some(old) = self.entries.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(old.bytes);
        }
    }

    pub fn contains(&self, key: &PageKey) -> bool {
        self.entries.iter().any(|e| &e.key == key)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

/// 位图按 4 字节/像素算，文本按字符数粗估（文本不进缓存，但别让它算成 0）。
fn estimate_bytes(canvas: &Canvas) -> usize {
    canvas.approx_bytes()
}

static CACHE: OnceLock<Mutex<PageCache>> = OnceLock::new();

fn cache() -> &'static Mutex<PageCache> {
    CACHE.get_or_init(|| Mutex::new(PageCache::new(CACHE_LIMIT_BYTES)))
}

/// 查缓存。
pub fn get(key: &PageKey) -> Option<Arc<Canvas>> {
    cache().lock().ok().and_then(|mut guard| guard.get(key))
}

/// 存一页。
pub fn put(key: PageKey, canvas: Arc<Canvas>) {
    if let Ok(mut guard) = cache().lock() {
        guard.put(key, canvas);
    }
}

/// 已经有了吗（用来避免重复预渲染）。
pub fn has(key: &PageKey) -> bool {
    cache()
        .lock()
        .map(|guard| guard.contains(key))
        .unwrap_or(false)
}

/// 统计（日志/测试用）。
pub fn stats() -> (usize, usize) {
    cache()
        .lock()
        .map(|guard| (guard.len(), guard.bytes()))
        .unwrap_or((0, 0))
}

/// 清空（换文件时用：几百页的文档占着缓存没有意义）。
pub fn clear() {
    if let Ok(mut guard) = cache().lock() {
        guard.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qlr_render::Bitmap;

    fn fake_bitmap(w: u32, h: u32) -> Arc<Canvas> {
        Arc::new(Canvas::Bitmap(Bitmap::from_rgba(
            w,
            h,
            vec![0u8; (w * h * 4) as usize],
        )))
    }

    #[test]
    fn hits_only_on_exact_key() {
        let mut cache = PageCache::new(16 * 1024 * 1024);
        let key = PageKey::new(Path::new("a.pdf"), 0, 100, 100);
        cache.put(key.clone(), fake_bitmap(100, 100));
        assert!(cache.get(&key).is_some());
        // 页号不同、尺寸不同都不算命中（宁可重算也不要拿错位图）。
        assert!(
            cache
                .get(&PageKey::new(Path::new("a.pdf"), 1, 100, 100))
                .is_none()
        );
        assert!(
            cache
                .get(&PageKey::new(Path::new("a.pdf"), 0, 101, 100))
                .is_none()
        );
        assert!(
            cache
                .get(&PageKey::new(Path::new("b.pdf"), 0, 100, 100))
                .is_none()
        );
    }

    #[test]
    fn evicts_oldest_when_over_limit() {
        // 每页 100×100×4 = 40 KB；上限 100 KB → 最多 2 页。
        let mut cache = PageCache::new(100 * 1024);
        for page in 0..3 {
            cache.put(
                PageKey::new(Path::new("a.pdf"), page, 100, 100),
                fake_bitmap(100, 100),
            );
        }
        assert!(cache.len() <= 2, "实际 {}", cache.len());
        // 最早那页被挤掉。
        assert!(
            cache
                .get(&PageKey::new(Path::new("a.pdf"), 0, 100, 100))
                .is_none()
        );
        assert!(
            cache
                .get(&PageKey::new(Path::new("a.pdf"), 2, 100, 100))
                .is_some()
        );
    }

    #[test]
    fn get_refreshes_recency() {
        let mut cache = PageCache::new(100 * 1024);
        let a = PageKey::new(Path::new("a.pdf"), 0, 100, 100);
        let b = PageKey::new(Path::new("a.pdf"), 1, 100, 100);
        let c = PageKey::new(Path::new("a.pdf"), 2, 100, 100);
        cache.put(a.clone(), fake_bitmap(100, 100));
        cache.put(b.clone(), fake_bitmap(100, 100));
        // 摸一下 a，让它变成"最近使用"，再塞 c 时该挤掉 b。
        assert!(cache.get(&a).is_some());
        cache.put(c.clone(), fake_bitmap(100, 100));
        assert!(cache.get(&a).is_some(), "a 刚用过，不该被挤掉");
        assert!(cache.get(&b).is_none(), "b 最久没用，应被挤掉");
        assert!(cache.get(&c).is_some());
    }

    #[test]
    fn oversized_page_is_not_cached() {
        let mut cache = PageCache::new(10 * 1024);
        let key = PageKey::new(Path::new("a.pdf"), 0, 200, 200); // 160 KB
        cache.put(key.clone(), fake_bitmap(200, 200));
        assert_eq!(cache.len(), 0, "单页超过上限就不该进缓存");
    }

    #[test]
    fn clear_empties_everything() {
        let mut cache = PageCache::new(16 * 1024 * 1024);
        cache.put(
            PageKey::new(Path::new("a.pdf"), 0, 100, 100),
            fake_bitmap(100, 100),
        );
        cache.clear();
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.bytes(), 0);
    }
}
