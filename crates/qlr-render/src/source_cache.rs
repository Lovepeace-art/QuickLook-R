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
//! - 总预算 [`CACHE_LIMIT_BYTES`] = 64 MB，超出按 LRU 淘汰；**另受 [`crate::budget`]
//!   的进程级总预算约束**（拿不到额度就先淘汰自家条目，还不行就不缓存——别把别人的
//!   额度顶掉，PDF 文档槽那 100 MB 是换来 250 ms 的，比一张图的档位值钱）；
//! - 单张超过 [`CACHE_MAX_PIXELS`] 像素的**不进缓存**：48MP 的 RGBA 是 192 MB 一张，
//!   放进来会把预算一次性吃穿，而这类图本来就走流式降采样（`downscale`），也不需要缓存；
//! - 命中判定用 `路径 + 修改时间 + 文件长度`：文件被覆盖（截图工具、编辑器另存）后
//!   修改时间会变，不会拿旧位图糊弄用户；
//! - 锁只在这一层的 `lookup` / `store` 之间持有，**渲染线程不会拿它去碰 UI 状态**，
//!   所以不会和窗口线程互相等。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use crate::budget;

/// 条目的"档位"标签。
///
/// 图片的通用路径存的是**原始分辨率**，用 [`TAG_FULL`]；JPEG 的 DCT 缩放路径
/// （`image_view::render_jpeg_scaled`）存的是**某一档解码结果**，同一条路径的不同档
/// 必须分行存放——否则"1/2 档解出的 1920×1080"会被下一个人当成原图 3840×2160
/// 拿去放大，画面直接糊掉。标签取解码尺寸本身（`(宽, 高)`），不需要另外发明编号。
pub type Tag = (u32, u32);

/// 原始分辨率条目的标签（PNG/GIF/BMP… 那条通用路径）。
pub const TAG_FULL: Tag = (0, 0);

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
    tag: Tag,
    img: Arc<SourceImage>,
    /// 最近一次使用时的全局计数（越小越旧）。
    used: u64,
    /// 这份条目在进程级总预算上占的额度；条目被淘汰时随 `Drop` 自动归还。
    _permit: budget::Permit<'static>,
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

/// 查原始分辨率缓存（PNG/GIF/BMP… 那条通用路径）。
pub fn lookup(path: &Path) -> Option<Arc<SourceImage>> {
    lookup_tagged(path, TAG_FULL)
}

/// 查指定档位（JPEG 的 DCT 缩放路径用解码尺寸当标签）。
///
/// 命中返回 `Arc`（渲染期间即使被淘汰，手头这份也还有效）。
pub fn lookup_tagged(path: &Path, tag: Tag) -> Option<Arc<SourceImage>> {
    let (mtime, len) = identity(path)?;
    let mut c = cache().lock().ok()?;
    c.tick += 1;
    let tick = c.tick;
    let hit = c
        .entries
        .iter_mut()
        .find(|e| e.path == path && e.mtime == mtime && e.len == len && e.tag == tag)?;
    hit.used = tick;
    Some(Arc::clone(&hit.img))
}

/// 存一张原始分辨率的源图。
pub fn store(path: &Path, img: Arc<SourceImage>) {
    store_tagged(path, TAG_FULL, img)
}

/// 存一张指定档位的源图；超预算按 LRU 淘汰，单张超预算直接不存。
pub fn store_tagged(path: &Path, tag: Tag, img: Arc<SourceImage>) {
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
    // 同一路径 + 同一档的旧版本（文件改过了）先丢掉，避免同一张图留两份。
    if let Some(pos) = c
        .entries
        .iter()
        .position(|e| e.path == path && e.mtime == mtime && e.len == len && e.tag == tag)
    {
        let old = c.entries.remove(pos);
        c.bytes -= old.img.rgba.len();
    }
    // 第一道：本缓存自己的 64 MB 上限。
    while c.bytes + size > CACHE_LIMIT_BYTES && evict_oldest(&mut c) {}
    // 第二道：进程级总预算（`crate::budget`）。拿不到额度就继续淘汰自家条目，
    // 自家空了还拿不到就不缓存——占着额度的是别的缓存（多半是 PDF 文档槽那份
    // 换来 250 ms 的整文件），这里动不了、也不该动。
    let permit = loop {
        if let Some(permit) = budget::try_reserve(size) {
            break permit;
        }
        if !evict_oldest(&mut c) {
            return;
        }
    };
    c.bytes += size;
    c.entries.push(Entry {
        path: path.to_path_buf(),
        mtime,
        len,
        tag,
        img,
        used: tick,
        _permit: permit,
    });
}

/// 丢掉最久没用到的那一份；缓存已经空了就返回 `false`。
fn evict_oldest(c: &mut Cache) -> bool {
    let Some(oldest) = c
        .entries
        .iter()
        .enumerate()
        .min_by_key(|(_, e)| e.used)
        .map(|(i, _)| i)
    else {
        return false;
    };
    let gone = c.entries.remove(oldest);
    c.bytes -= gone.img.rgba.len();
    true
}

#[cfg(test)]
pub fn tags_for(path: &Path) -> Vec<Tag> {
    let Ok(c) = cache().lock() else {
        return Vec::new();
    };
    c.entries
        .iter()
        .filter(|e| e.path == path)
        .map(|e| e.tag)
        .collect()
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

    /// 同一条路径的不同档必须分行存放、互不遮蔽：JPEG 从"适应窗口"放大一档时，
    /// 1/2 档与 1/1 档会同时在缓存里；若共用一个槽，来回滚就会无限重解码。
    #[test]
    fn tags_of_one_file_do_not_shadow_each_other() {
        let path = tmp("levels.jpg", b"same file");
        store_tagged(&path, (4, 4), source(4, 4));
        store_tagged(&path, (2, 2), source(2, 2));
        assert_eq!(lookup_tagged(&path, (4, 4)).expect("1/1 档").width, 4);
        assert_eq!(lookup_tagged(&path, (2, 2)).expect("1/2 档").width, 2);
        // 通用路径的标签是 TAG_FULL，不该误命中某个档位条目。
        assert!(lookup(&path).is_none(), "TAG_FULL 不该命中档位条目");
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
