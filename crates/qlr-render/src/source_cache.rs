//! 解码后的**源位图**缓存（只给静态图片用）。
//!
//! ## 为什么要有它
//!
//! 预览里"滚一格滚轮"= 一次新的渲染请求。老路径每次都要
//! `打开文件 → 读完整个 PNG → 尺寸规划 → 缩放 → 转 BGRA`，其中"读文件 + 解码"这一段
//! 与缩放无关，纯属重复劳动：同一张 1920×1080 PNG 解一遍 3–5 ms、
//! 一张 4K 截图 30–40 ms，连滚 5 档就白付 5 次。
//!
//! 缓存的是**原始分辨率**的 RGBA（不是某一档缩放结果），所以任意缩放档都能直接复用，
//! 也不会出现"缓存了几个档、每个档 4 MB"这种按档膨胀的问题。
//!
//! ## 上限与代价
//!
//! - 总预算 [`CACHE_LIMIT_BYTES`] = 64 MB，超出按 LRU 淘汰；
//! - 单张超过 [`CACHE_MAX_PIXELS`] 像素的**不进缓存**：48MP 的 RGBA 是 192 MB 一张，
//!   放进来会把预算一次性吃穿，而这类图本来就走流式降采样（`downscale`），也不需要缓存；
//! - 命中判定用 `路径 + 修改时间 + 文件长度`：文件被覆盖（截图工具、编辑器另存）后
//!   修改时间会变，不会拿旧位图糊弄用户；
//! - 锁只在这一层的 `lookup` / `store` 之间持有，**渲染线程不会拿它去碰 UI 状态**，
//!   所以不会和窗口线程互相等。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

/// 缓存总预算：64 MB。
///
/// 取这个数的理由：一张 4K 截图（3840×2160 RGBA）是 33 MB，装得下；
/// 一张 48MP 是 192 MB，装不下也不该装（那种图走流式降采样，本来就不产生整图缓冲）。
/// 64 MB 是"常见截图能装一张、内存曲线又不至于让用户觉得预览器很胖"的折中，
/// 与 1.8.3 给"最后一帧"定的字节上限是同一个量级。
pub const CACHE_LIMIT_BYTES: usize = 64 * 1024 * 1024;

/// 单张进缓存的最大像素数（16 MP ≈ 64 MB 的 RGBA）。
///
/// 与总预算同量级：一张 16 MP 的图刚好占满预算，再大就不划算（见模块头注释）。
pub const CACHE_MAX_PIXELS: u64 = 16_000_000;

/// 缓存里的一张源图：**原始分辨率**的 RGBA8。
pub struct SourceImage {
    pub width: u32,
    pub height: u32,
    /// 长度 == `width * height * 4`。
    pub rgba: Vec<u8>,
}

impl SourceImage {
    pub fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

struct Entry {
    path: PathBuf,
    mtime: SystemTime,
    len: u64,
    img: Arc<SourceImage>,
    /// 最近一次使用时的全局计数（越小越旧）。
    used: u64,
}

struct Cache {
    entries: Vec<Entry>,
    bytes: usize,
    tick: u64,
}

static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

fn cache() -> &'static Mutex<Cache> {
    CACHE.get_or_init(|| {
        Mutex::new(Cache {
            entries: Vec::new(),
            bytes: 0,
            tick: 0,
        })
    })
}

/// 文件的 (修改时间, 长度)；读不到就没有身份，也就不缓存。
fn identity(path: &Path) -> Option<(SystemTime, u64)> {
    let md = std::fs::metadata(path).ok()?;
    Some((md.modified().ok()?, md.len()))
}

/// 这个像素量值不值得进缓存（48MP 那种一张 192 MB 的不值得）。
pub fn cacheable(px: u64) -> bool {
    px > 0 && px <= CACHE_MAX_PIXELS
}

/// 查缓存：命中返回 `Arc`（渲染期间即使被淘汰，手头这份也还有效）。
pub fn lookup(path: &Path) -> Option<Arc<SourceImage>> {
    let (mtime, len) = identity(path)?;
    let mut c = cache().lock().ok()?;
    c.tick += 1;
    let tick = c.tick;
    let hit = c
        .entries
        .iter_mut()
        .find(|e| e.path == path && e.mtime == mtime && e.len == len)?;
    hit.used = tick;
    Some(Arc::clone(&hit.img))
}

/// 存一张源图；超预算按 LRU 淘汰，单张超预算直接不存。
pub fn store(path: &Path, img: Arc<SourceImage>) {
    let Some((mtime, len)) = identity(path) else {
        return;
    };
    let size = img.rgba.len();
    if size == 0 || size > CACHE_LIMIT_BYTES || !cacheable(img.pixels()) {
        return;
    }
    let Ok(mut c) = cache().lock() else {
        return;
    };
    c.tick += 1;
    let tick = c.tick;
    // 同一路径的旧版本（文件改过了）先丢掉，避免同一张图留两份。
    if let Some(pos) = c
        .entries
        .iter()
        .position(|e| e.path == path && e.mtime == mtime && e.len == len)
    {
        let old = c.entries.remove(pos);
        c.bytes -= old.img.rgba.len();
    }
    while c.bytes + size > CACHE_LIMIT_BYTES && !c.entries.is_empty() {
        let oldest = c
            .entries
            .iter()
            .enumerate()
            .min_by_key(|(_, e)| e.used)
            .map(|(i, _)| i)
            .unwrap_or(0);
        let gone = c.entries.remove(oldest);
        c.bytes -= gone.img.rgba.len();
    }
    c.bytes += size;
    c.entries.push(Entry {
        path: path.to_path_buf(),
        mtime,
        len,
        img,
        used: tick,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str, bytes: &[u8]) -> PathBuf {
        let dir = crate::testutil::dir("imgcache");
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("写夹具");
        path
    }

    fn source(w: u32, h: u32) -> Arc<SourceImage> {
        Arc::new(SourceImage {
            width: w,
            height: h,
            rgba: vec![7u8; (w * h * 4) as usize],
        })
    }

    #[test]
    fn hit_then_miss_after_file_changes() {
        let path = tmp("hit.png", b"first");
        let img = source(4, 4);
        store(&path, Arc::clone(&img));
        let got = lookup(&path).expect("刚存的应命中");
        assert_eq!(got.width, 4);

        // 内容与长度都变了 → 必须当未命中（不能拿旧像素）。
        std::fs::write(&path, b"second version").expect("改写");
        assert!(lookup(&path).is_none(), "文件变了就不许命中");
    }

    #[test]
    fn oversize_is_never_stored() {
        let path = tmp("huge.png", b"x");
        // 20 MP > 16 MP 上限：存了也不会命中。
        store(&path, source(5000, 4000));
        assert!(lookup(&path).is_none());
        assert!(cacheable(CACHE_MAX_PIXELS));
        assert!(!cacheable(CACHE_MAX_PIXELS + 1));
    }

    #[test]
    fn lru_evicts_until_within_budget() {
        // 16 MP 一张（64 MB）刚好占满预算；再塞第二张必须把第一张挤掉。
        let a = tmp("a.bin", b"a");
        let b = tmp("b.bin", b"b");
        store(&a, source(4000, 4000));
        store(&b, source(4000, 4000));
        assert!(lookup(&b).is_some(), "后存的应命中");
        assert!(lookup(&a).is_none(), "预算只够一张，旧的应被淘汰");
        let bytes = cache().lock().unwrap().bytes;
        assert!(bytes <= CACHE_LIMIT_BYTES, "缓存字节 {bytes} 超预算");
    }
}
