//! 图片渲染：解码 → 等比缩放 → BGRA。
//!
//! 两条路径：
//! - **JPEG 走缩放解码**（A2.3）：`jpeg-decoder` 的 DCT 域缩放（1/1、1/2、1/4、1/8）
//!   直接解到显示尺寸附近，再补一次盒式缩放。48MP 图因此不会先产生 192 MB 的整图缓冲。
//! - 其余格式走 `image` 的朴素路径（整图解码后缩放），后续按同样思路逐个替换。
//!
//! 为什么 JPEG 先做：它是最高频、也最容易一次解出巨量内存的格式，且 DCT 缩放是标准能力。
//!
//! 1.8.3-fix 起两条路都带缓存：非 JPEG 走 `source_cache` 的原始分辨率缓存，JPEG 因为
//! "只解到需要的档"而不能存原图，改成**按解码档位缓存**（见 `render_jpeg_scaled`）。

use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use image::imageops::FilterType;
use qlr_core::{Error, Result};

use crate::canvas::{Bitmap, Canvas};
use crate::source_cache;

/// 解码并缩放到 `max_width × max_height` 以内（保持宽高比，不放大）。
///
/// `max_*` 传 0 表示该方向不限制。
pub fn render(path: &Path, max_width: u32, max_height: u32) -> Result<Canvas> {
    render_scaled(path, max_width, max_height, false)
}

/// 同上，但可以要求"放大渲染"（`upscale = true` 时不设 1.0 上限）。
///
/// 用户放大图片时，位图必须真的变大才有意义；默认路径不放大小图（预览一张 32×32 图标
/// 不该被糊成整屏）。
pub fn render_scaled(
    path: &Path,
    max_width: u32,
    max_height: u32,
    upscale: bool,
) -> Result<Canvas> {
    crate::cancel::checkpoint()?;
    if is_jpeg(path) {
        match render_jpeg_scaled(path, max_width, max_height, upscale) {
            Ok(canvas) => return Ok(canvas),
            // 缩放路径解不了的（CMYK、渐进式异常等）退回 `image`，不要因为优化而少功能。
            Err(e) => crate::trace_jpeg_fallback(path, &e),
        }
    }
    if is_png(path) {
        if let Some(canvas) = render_png_boxed(path, max_width, max_height, upscale)? {
            return Ok(canvas);
        }
    }
    render_via_image(path, max_width, max_height, upscale)
}

/// 格式判断：只看扩展名（与 JPEG 那条一致；猜格式的活留给 `image`）。
fn is_png(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("png")
    )
}

/// PNG 专用：先读头拿尺寸 → 只需要缩小且像素够多时走**逐行流式降采样**。
///
/// 为什么值得单开一条路：`image` 只能整图解码，8000×6000 的 PNG 解码缓冲 ≈192 MB，
/// 而我们最终只要屏幕那么大。走流式后峰值降到"目标画布 + 一行原始像素"。
/// 返回 `Ok(None)` 表示"这次不走流式"（放大、隔行、APNG、小图），由调用方回退。
fn render_png_boxed(
    path: &Path,
    max_width: u32,
    max_height: u32,
    upscale: bool,
) -> Result<Option<Canvas>> {
    let Some((src_w, src_h)) = dimensions(path) else {
        return Ok(None);
    };
    let (dst_w, dst_h) = fit_within_opt(src_w, src_h, max_width, max_height, upscale);
    // 不缩小就别折腾：逐行解码比整解慢，`thumbnail` 也够快。
    if (dst_w, dst_h) == (src_w, src_h) {
        return Ok(None);
    }
    if u64::from(src_w) * u64::from(src_h) < crate::downscale::STREAM_MIN_PIXELS {
        return Ok(None);
    }
    let started = std::time::Instant::now();
    match crate::downscale::decode_png_boxed(path, dst_w, dst_h) {
        Ok(Some(bitmap)) => {
            qlr_core::log_debug!(
                "render",
                "PNG 流式降采样 {src_w}x{src_h} -> {}x{} elapsed={}ms",
                bitmap.width,
                bitmap.height,
                started.elapsed().as_millis()
            );
            Ok(Some(Canvas::Bitmap(bitmap)))
        }
        // 隔行 / APNG / 未知色型：交回整图解码。
        Ok(None) => Ok(None),
        // 解码出错就如实报，不要偷偷回退（回退会再失败一次，还会把真实原因盖掉）。
        Err(e) => Err(e),
    }
}

fn is_jpeg(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("jpg" | "jpeg" | "jfif")
    )
}

/// JPEG 专用：先读头拿尺寸 → 算目标尺寸 → 让解码器按 1/8…1/1 缩小 → 补一次盒式缩放。
fn render_jpeg_scaled(
    path: &Path,
    max_width: u32,
    max_height: u32,
    upscale: bool,
) -> Result<Canvas> {
    let file = std::fs::File::open(path).map_err(|e| Error::Message(format!("打不开图片：{e}")))?;
    let mut decoder = jpeg_decoder::Decoder::new(BufReader::new(file));
    decoder
        .read_info()
        .map_err(|e| Error::Message(format!("读 JPEG 头失败：{e}")))?;

    let info = decoder
        .info()
        .ok_or_else(|| Error::Message("JPEG 没有尺寸信息".to_string()))?;
    let (src_w, src_h) = (info.width as u32, info.height as u32);
    let (want_w, want_h) =
        fit_within_opt(src_w.max(1), src_h.max(1), max_width, max_height, upscale);

    // `scale` 只会挑"不小于请求尺寸"的 1/2^n 档，并把**实际解出的尺寸**还给我们：
    // 这个尺寸既是缓存的分档标签，也是下一步盒式缩放的源尺寸。
    let (dw, dh) = decoder
        .scale(want_w as u16, want_h as u16)
        .map_err(|e| Error::Message(format!("设置 JPEG 缩放失败：{e}")))?;
    let (dw, dh) = (u32::from(dw), u32::from(dh));

    // 这一档已经解过：省掉整段"读熵编码 + IDCT + 转 RGBA"（4K 图 30–40 ms、48MP 66–69 ms）。
    if let Some(src) = source_cache::lookup_tagged(path, (dw, dh)) {
        qlr_core::log_debug!(
            "render",
            "JPEGCACHE 命中 {dw}x{dh} -> {want_w}x{want_h}（{}）",
            crate::display_name(path)
        );
        return Ok(Canvas::Bitmap(finish_rgba(
            &src.rgba, src.width, src.height, want_w, want_h,
        )?));
    }

    let pixels = decoder
        .decode()
        .map_err(|e| Error::Message(format!("解码失败：{e}")))?;
    let scaled_info = decoder
        .info()
        .ok_or_else(|| Error::Message("解码后丢失尺寸信息".to_string()))?;

    let (dw, dh) = (u32::from(scaled_info.width), u32::from(scaled_info.height));
    let rgba = jpeg_pixels_to_rgba(pixels, dw, dh, scaled_info.pixel_format)?;

    // 值得缓存的（这一档的像素数在上限内）：先按档位存一份，再用**同一个函数**出帧。
    // 48MP 原图存不下，但它 1/4、1/8 档的解码结果只有 12 MB / 3 MB——正是"每档重解码
    // 66–69 ms"最疼的那几档。
    if source_cache::cacheable(u64::from(dw) * u64::from(dh)) {
        let src = Arc::new(source_cache::SourceImage {
            width: dw,
            height: dh,
            rgba,
        });
        let bitmap = finish_rgba(&src.rgba, dw, dh, want_w, want_h)?;
        source_cache::store_tagged(path, (dw, dh), Arc::clone(&src));
        return Ok(Canvas::Bitmap(bitmap));
    }
    Ok(Canvas::Bitmap(finish_rgba(&rgba, dw, dh, want_w, want_h)?))
}

/// JPEG 解码器的输出 → RGBA8（预览一律按 RGBA 存，出帧时再转 BGRA）。
///
/// 缩放路径不支持 CMYK/4 通道（`jpeg-decoder` 会给 `Cmyk32`），这时返回 Err，
/// 由 `render_scaled` 回退到 `image` 那条朴素路径，功能不能因为优化而少。
fn jpeg_pixels_to_rgba(
    pixels: Vec<u8>,
    dw: u32,
    dh: u32,
    format: jpeg_decoder::PixelFormat,
) -> Result<Vec<u8>> {
    let n = (dw as usize) * (dh as usize);
    let rgba = match format {
        jpeg_decoder::PixelFormat::RGB24 if pixels.len() == n * 3 => {
            let mut out = Vec::with_capacity(n * 4);
            for px in pixels.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            out
        }
        jpeg_decoder::PixelFormat::L8 if pixels.len() == n => {
            let mut out = Vec::with_capacity(n * 4);
            for &g in &pixels {
                out.extend_from_slice(&[g, g, g, 255]);
            }
            out
        }
        other => {
            return Err(Error::Message(format!(
                "缩放路径不支持该 JPEG 像素格式：{other:?}"
            )));
        }
    };
    Ok(rgba)
}

/// 把某一档解码结果缩到出帧尺寸。
///
/// 缩小走 `scale::downscale_exact`——它与 `image::imageops::thumbnail` **逐字节一致**
/// （`scale.rs` 的 `downscale_matches_image_crate` 钉着，1920→1920 的恒等档也在里面）。
/// **两条路径（刚解码 / 命中缓存）必须调用同一个函数**，否则同一张图"第一次"和
/// "第二次"会差一级色，用户来回滚一眼就能看出来。
/// 唯一退路是"一维放一维缩"的极端长条：那时 `scale_rgba_to_bgra` 返回 `None`，退回 `thumbnail`。
fn finish_rgba(rgba: &[u8], dw: u32, dh: u32, want_w: u32, want_h: u32) -> Result<Bitmap> {
    if let Some(bitmap) = crate::scale::scale_rgba_to_bgra(rgba, dw, dh, want_w, want_h) {
        return Ok(bitmap);
    }
    let img = image::RgbaImage::from_raw(dw, dh, rgba.to_vec())
        .ok_or_else(|| Error::Message("解码缓冲与尺寸不匹配".to_string()))?;
    let scaled = image::imageops::thumbnail(&img, want_w, want_h);
    Ok(Bitmap::from_rgba(
        scaled.width(),
        scaled.height(),
        scaled.into_raw(),
    ))
}

/// 只读文件头拿图片原始像素尺寸（**不解码像素**）。
///
/// 开窗时用它在"原始尺寸"下决定窗口大小；读失败返回 `None`（调用方退回按比例开窗）。
pub fn dimensions(path: &Path) -> Option<(u32, u32)> {
    let reader = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?;
    let (w, h) = reader.into_dimensions().ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}

/// 通用路径：整图解码 → 盒式缩放。给所有非 JPEG 格式兜底。
///
/// 1.8.3：解码结果会先落进 `source_cache`，**同一张图再缩放时不再解码**。
/// 缩放本身走 `scale` 那条快路径（缩小与 `thumbnail` 逐字节一致，放大用定点双线性）；
/// 只有"一维放一维缩"这种极端长条才退回 `thumbnail`，保证历史口径不漂。
fn render_via_image(path: &Path, max_width: u32, max_height: u32, upscale: bool) -> Result<Canvas> {
    // 命中缓存：跳过整个"打开 + 解码 + 转 RGBA"（1920 PNG 省 4 ms，4K 截图省 30 ms 以上）。
    if let Some(src) = source_cache::lookup(path) {
        let (dst_w, dst_h) =
            thumbnail_dims_for(src.width, src.height, max_width, max_height, upscale);
        if let Some(bitmap) =
            crate::scale::scale_rgba_to_bgra(&src.rgba, src.width, src.height, dst_w, dst_h)
        {
            qlr_core::log_debug!(
                "render",
                "IMGCACHE 命中 {}x{} -> {dst_w}x{dst_h}（{}）",
                src.width,
                src.height,
                crate::display_name(path)
            );
            return Ok(Canvas::Bitmap(bitmap));
        }
    }

    let reader = image::ImageReader::open(path)
        .map_err(|e| Error::Message(format!("打不开图片：{e}")))?
        .with_guessed_format()
        .map_err(|e| Error::Message(format!("猜不出图片格式：{e}")))?;

    let img = reader
        .decode()
        .map_err(|e| Error::Message(format!("解码失败：{e}")))?;
    // 解码是最贵的一段（大图几十~几百毫秒），解完先问一句"还要不要"，
    // 免得用户已经换文件了还在做缩放和转 RGBA。
    crate::cancel::checkpoint()?;

    let (src_w, src_h) = (img.width().max(1), img.height().max(1));
    let (dst_w, dst_h) = fit_within_opt(src_w, src_h, max_width, max_height, upscale);

    // 值得缓存的（像素数在上限内）：先转成 RGBA 存起来，再**用同一套缩放**出帧。
    // 命中与未命中必须走同一个缩放函数，否则同一档缩放在"第一次"和"第二次"会差 1 级色，
    // 用户滚回去看一眼就能发现画面抖了一下。
    if source_cache::cacheable(u64::from(src_w) * u64::from(src_h)) {
        let (exact_w, exact_h) = thumbnail_dims_for(src_w, src_h, max_width, max_height, upscale);
        let rgba = img.to_rgba8().into_raw();
        let src = std::sync::Arc::new(source_cache::SourceImage {
            width: src_w,
            height: src_h,
            rgba,
        });
        if let Some(bitmap) =
            crate::scale::scale_rgba_to_bgra(&src.rgba, src_w, src_h, exact_w, exact_h)
        {
            source_cache::store(path, src);
            return Ok(Canvas::Bitmap(bitmap));
        }
    }

    // `thumbnail` 是盒式采样，比 `resize(CatmullRom)` 快约一个量级；预览场景看不出差别。
    let scaled = if (dst_w, dst_h) == (src_w, src_h) {
        img
    } else {
        img.thumbnail(dst_w, dst_h)
    };

    let rgba = scaled.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    Ok(Canvas::Bitmap(Bitmap::from_rgba(w, h, rgba.into_raw())))
}

/// 等比缩放到不超过 `(max_w, max_h)`，且绝不放大。
///
/// 任意一维传 0 视为"该维不限制"。返回至少 1×1，避免下游除零。
pub fn fit_within(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    fit_within_opt(src_w, src_h, max_w, max_h, false)
}

/// 出帧的**最终**尺寸：`fit_within_opt` 算出盒子，再按 `image` 的口径等比缩进盒子。
///
/// 为什么要单独一步：老路径是 `img.thumbnail(dst_w, dst_h)`，而 `DynamicImage::thumbnail`
/// 内部先做 `resize_dimensions`（等比、**四舍五入**、至少 1）再精确缩到那个尺寸。
/// 我们若直接按"填满盒子"的尺寸缩，结果会差 1 px：1920×1080 进 1589×893 的盒子，
/// `thumbnail` 给 1588×893。差这 1 px 等于把画面纵向拉伸 0.06%，
/// 逐像素的视觉基线立刻不成立（实测 7.3 万个像素不同）。所以这一步必须照抄。
fn thumbnail_dims_for(src_w: u32, src_h: u32, max_w: u32, max_h: u32, upscale: bool) -> (u32, u32) {
    let (box_w, box_h) = fit_within_opt(src_w, src_h, max_w, max_h, upscale);
    let (src_w, src_h) = (src_w.max(1), src_h.max(1));
    if (box_w, box_h) == (src_w, src_h) {
        return (src_w, src_h);
    }
    let ratio = (f64::from(box_w) / f64::from(src_w)).min(f64::from(box_h) / f64::from(src_h));
    let w = ((f64::from(src_w) * ratio).round() as u32).max(1);
    let h = ((f64::from(src_h) * ratio).round() as u32).max(1);
    (w, h)
}

/// `fit_within` 的可放大版本（`upscale = true` 时允许超过原始尺寸）。
fn fit_within_opt(src_w: u32, src_h: u32, max_w: u32, max_h: u32, upscale: bool) -> (u32, u32) {
    let src_w = src_w.max(1);
    let src_h = src_h.max(1);
    if max_w == 0 && max_h == 0 {
        return (src_w, src_h);
    }
    // 用 f64 算比例再截断：先乘后除，避免整数除法把小数吃掉。
    let by_w = if max_w == 0 {
        f64::INFINITY
    } else {
        max_w as f64 / src_w as f64
    };
    let by_h = if max_h == 0 {
        f64::INFINITY
    } else {
        max_h as f64 / src_h as f64
    };
    let scale = if upscale {
        by_w.min(by_h)
    } else {
        by_w.min(by_h).min(1.0)
    };
    if scale.is_infinite() {
        return (src_w, src_h);
    }
    if (scale - 1.0).abs() < f64::EPSILON {
        return (src_w, src_h);
    }
    let w = ((src_w as f64 * scale).floor() as u32).max(1);
    let h = ((src_h as f64 * scale).floor() as u32).max(1);
    (w, h)
}

/// 解码滤波器（留给 A8.1 切换用；现在固定盒式）。
pub const FILTER: FilterType = FilterType::Triangle;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_aspect_ratio() {
        // 4000x3000 塞进 1000x1000 → 1000x750
        assert_eq!(fit_within(4000, 3000, 1000, 1000), (1000, 750));
        // 窗口比图大时绝不放大
        assert_eq!(fit_within(800, 600, 4000, 3000), (800, 600));
        // 只限宽度
        assert_eq!(fit_within(4000, 3000, 1000, 0), (1000, 750));
        // 只限高度
        assert_eq!(fit_within(4000, 3000, 0, 600), (800, 600));
        // 不限制
        assert_eq!(fit_within(4000, 3000, 0, 0), (4000, 3000));
    }

    #[test]
    fn fit_never_returns_zero() {
        // 极端长条形：3000x1 塞进 100x100，高度不能被截成 0
        let (w, h) = fit_within(3000, 1, 100, 100);
        assert!(w >= 1 && h >= 1, "得到 {w}x{h}");
    }

    #[test]
    fn decodes_a_generated_png() {
        // 现场生成一张 8x4 的红色 PNG，验证解码路径与 BGRA 转换都对。
        let path = crate::testutil::dir("image").join("red.png");

        let img = image::RgbaImage::from_pixel(8, 4, image::Rgba([255, 0, 0, 255]));
        img.save(&path).unwrap();

        let canvas = render(&path, 4, 4).expect("应该能解码");
        match canvas {
            Canvas::Bitmap(b) => {
                assert_eq!((b.width, b.height), (4, 2), "8x4 缩到 4x4 应为 4x2");
                // 红色像素在 BGRA 里是 B=0,G=0,R=255,A=255
                assert_eq!(&b.bgra[0..4], &[0, 0, 255, 255]);
            }
            other => panic!("期望 Bitmap，得到 {}", other.kind_name()),
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_is_a_clean_error() {
        let err = render(Path::new(r"C:\definitely\not\here.png"), 100, 100).unwrap_err();
        assert!(format!("{err}").contains("打不开图片"), "{err}");
    }

    /// 同一个请求"第一次（未命中）"与"第二次（命中缓存）"必须给出**逐字节相同**的位图。
    ///
    /// 这两次走的是两条不同的前置路径（前者解码 + 缓存 + 缩放，后者纯缩放），
    /// 只要有一处算错（尺寸、取整、通道顺序），用户来回滚一下就能看到画面抖。
    #[test]
    fn cache_hit_and_miss_produce_identical_bitmaps() {
        let path = crate::testutil::dir("image").join("cache-same.png");
        let mut img = image::RgbaImage::new(320, 180);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgba([(x % 251) as u8, (y % 241) as u8, ((x + y) % 233) as u8, 255]);
        }
        img.save(&path).unwrap();

        for (w, h) in [(160u32, 90u32), (200, 120), (400, 240)] {
            let cold = match render(&path, w, h).unwrap() {
                Canvas::Bitmap(b) => b,
                other => panic!("期望 Bitmap，得到 {}", other.kind_name()),
            };
            let warm = match render(&path, w, h).unwrap() {
                Canvas::Bitmap(b) => b,
                other => panic!("期望 Bitmap，得到 {}", other.kind_name()),
            };
            assert_eq!((cold.width, cold.height), (warm.width, warm.height));
            assert_eq!(cold.bgra, warm.bgra, "{w}x{h} 命中缓存后像素变了");
        }
        let _ = std::fs::remove_file(&path);
    }

    fn expect_bitmap(canvas: Canvas) -> Bitmap {
        match canvas {
            Canvas::Bitmap(b) => b,
            other => panic!("期望 Bitmap，得到 {}", other.kind_name()),
        }
    }

    /// 造一张 640×480 的有纹理 JPEG（纯色会掩盖取样/换算错误），返回路径。
    fn textured_jpeg(name: &str) -> std::path::PathBuf {
        let path = crate::testutil::dir("image").join(name);
        let mut img = image::RgbImage::new(640, 480);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgb([(x % 251) as u8, (y % 241) as u8, ((x + y) % 233) as u8]);
        }
        img.save(&path).expect("写 JPEG 夹具");
        path
    }

    /// JPEG 走"按解码档位缓存"之后，同一档的第二次出帧必须与第一次**逐字节相同**。
    ///
    /// 这是 1.8.3-fix 引入缓存时唯一的硬约束：命中路径（纯缩放）与未命中路径
    /// （解码 + 缩放）必须收敛到同一个结果，否则用户滚回去看一眼就能发现画面抖了。
    /// 第二遍、第三遍（倒序）都要过一遍，后者专门验证"两个档位同时在缓存里"不互相污染。
    #[test]
    fn jpeg_cache_hit_produces_identical_bitmaps() {
        let path = textured_jpeg("jpeg-cache-1.8.3f.jpg");
        let boxes = [(320u32, 240u32), (900, 675), (640, 480), (160, 120)];

        let mut first = Vec::new();
        for &(w, h) in &boxes {
            let b = expect_bitmap(render(&path, w, h).expect("冷渲染"));
            first.push((b.width, b.height, b.bgra));
        }

        for (i, &(w, h)) in boxes.iter().enumerate() {
            let b = expect_bitmap(render(&path, w, h).expect("热渲染"));
            assert_eq!(
                (b.width, b.height),
                (first[i].0, first[i].1),
                "第 {i} 档尺寸变了"
            );
            assert_eq!(b.bgra, first[i].2, "第 {i} 档 {w}x{h} 命中缓存后像素变了");
        }
        // 倒着再来一遍：档位切换顺序变了，结果不许变。
        for (i, &(w, h)) in boxes.iter().enumerate().rev() {
            let b = expect_bitmap(render(&path, w, h).expect("倒序热渲染"));
            assert_eq!(b.bgra, first[i].2, "倒序第 {i} 档 {w}x{h} 像素变了");
        }

        let tags = crate::source_cache::tags_for(&path);
        assert!(
            tags.len() >= 2,
            "不同缩放档应分别入缓存（实际标签 {tags:?}）"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 不缩放的请求（窗口比图大）也要能缓存：这时档位是 1/1，缓存里就是整张图。
    #[test]
    fn jpeg_full_size_level_is_cached() {
        let path = textured_jpeg("jpeg-cache-full-1.8.3f.jpg");
        let b1 = expect_bitmap(render(&path, 4000, 4000).expect("冷渲染"));
        assert_eq!((b1.width, b1.height), (640, 480), "默认不放大");
        let b2 = expect_bitmap(render(&path, 4000, 4000).expect("热渲染"));
        assert_eq!(b1.bgra, b2.bgra, "1/1 档命中缓存后像素变了");
        assert_eq!(crate::source_cache::tags_for(&path), vec![(640, 480)]);
        let _ = std::fs::remove_file(&path);
    }

    /// 手测用（CI 不跑）：`cargo test -p qlr-render --lib bench_jpeg_zoom_steps -- --ignored --nocapture`
    ///
    /// 打印"每个缩放档第一次（= 1.8.3 老路径的代价）与第二次（= 命中档位缓存）"的毫秒数。
    /// 数字要写进 `docs/dev/LOG.md`，所以留在仓库里可复现，而不是跑完就扔。
    #[test]
    #[ignore]
    fn bench_jpeg_zoom_steps() {
        use std::time::Instant;
        for (w, h, name) in [(3840u32, 2160u32, "4k"), (8000, 6000, "48mp")] {
            let path = crate::testutil::dir("image").join(format!("bench-zoom-{name}.jpg"));
            let mut img = image::RgbImage::new(w, h);
            for (x, y, px) in img.enumerate_pixels_mut() {
                *px = image::Rgb([
                    (x % 251) as u8,
                    (y % 241) as u8,
                    ((x / 7 + y / 5) % 233) as u8,
                ]);
            }
            img.save(&path).expect("写 JPEG 夹具");
            let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            println!("--- {name}: {w}x{h}, {} KB ---", bytes / 1024);

            // 模拟"适应窗口 → 1.25 倍 → 2 倍 → 回适应窗口"：滚一格再滚回去。
            let boxes = [(1589u32, 894u32), (1986, 1118), (3178, 1788), (1589, 894)];
            let mut cold_ms = Vec::new();
            let mut hot_ms = Vec::new();
            for &(bw, bh) in &boxes {
                let t0 = Instant::now();
                let a = expect_bitmap(render(&path, bw, bh).expect("冷渲染"));
                let cold = t0.elapsed().as_secs_f64() * 1000.0;
                let t1 = Instant::now();
                let b = expect_bitmap(render(&path, bw, bh).expect("热渲染"));
                let hot = t1.elapsed().as_secs_f64() * 1000.0;
                assert_eq!(a.bgra, b.bgra, "命中缓存不许改变像素");
                println!(
                    "窗口 {bw}x{bh} -> {}x{}：第一次 {cold:.1} ms，第二次 {hot:.1} ms",
                    a.width, a.height
                );
                cold_ms.push(cold);
                hot_ms.push(hot);
            }
            let sum = |v: &[f64]| v.iter().sum::<f64>();
            println!(
                "四档合计：第一次 {:.1} ms，第二次 {:.1} ms（省 {:.0}%）",
                sum(&cold_ms),
                sum(&hot_ms),
                (1.0 - sum(&hot_ms) / sum(&cold_ms)) * 100.0
            );
            let _ = std::fs::remove_file(&path);
        }
    }

    /// 出帧尺寸必须与 `image` 自己挑的尺寸一致（它是历史像素口径）。
    ///
    /// 这条测试是"换了缩放实现但基线一个像素都不动"的凭据：
    /// 尺寸差 1 px 就等于把画面拉伸千分之几，140 万像素的截图里会有几万个像素不同。
    #[test]
    fn frame_dims_match_image_crate_thumbnail() {
        let cases = [
            (1920u32, 1080u32, 1589u32, 894u32, false),
            (1920, 1080, 1588, 894, false),
            (1920, 1080, 880, 748, false),
            (4000, 3000, 1000, 1000, false),
            (641, 479, 1589, 894, true),
            (1920, 1080, 1986, 1118, true),
            (1920, 1080, 3879, 2183, true),
            (97, 61, 13, 7, false),
            (1920, 1080, 0, 893, false),
        ];
        for (sw, sh, bw, bh, up) in cases {
            // 老路径先 `fit_within_opt` 得到盒子，再 `img.thumbnail(盒子)`；
            // 我们只是把这两步的结果摊平成一个函数，尺寸必须一模一样。
            let (box_w, box_h) = fit_within_opt(sw, sh, bw, bh, up);
            let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(sw, sh));
            let want = img.thumbnail(box_w, box_h);
            let got = thumbnail_dims_for(sw, sh, bw, bh, up);
            assert_eq!(
                got,
                (want.width(), want.height()),
                "{sw}x{sh} 放进 {bw}x{bh}（upscale={up}）尺寸不一致"
            );
        }
    }
}
