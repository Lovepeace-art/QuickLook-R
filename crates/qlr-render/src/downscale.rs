//! 大 PNG 的**流式降采样**：解码器逐行吐像素，我们边解边缩，峰值内存只与目标尺寸有关。
//!
//! ## 为什么单独一条路径
//!
//! `image` 的解码器只能"整图解码"：一张 8000×6000 的 PNG，解码缓冲就是
//! 8000×6000×4 ≈ **192 MB**（实测峰值，见 docs/dev/STATUS.md §30），
//! 而我们最终只需要屏幕那么大（比如 1200×900 ≈ 4 MB）。
//! 中间那 190 MB 纯属白花，用户开着别的程序时这一下就是可见的卡顿甚至换页。
//!
//! 流式路径的做法：读一行 → 按面积平均累加到目标行 → 丢掉这一行。
//! 常驻 = 目标画布 + 一行原始像素 + 每列累加器（`dst_w * 4` 个 u32 + 计数），
//! 8000 宽降到 1200 宽时约 **4 MB + 32 KB + 20 KB**。
//!
//! ## 为什么只做 PNG
//!
//! - JPEG 早就有 DCT 域缩放（`jpeg_decoder::Decoder::scale`），根本不需要这条路径；
//! - PNG 是唯一"按行存、按行解、又常被用来存巨图"的格式（截图、地图、扫描件）；
//! - TIFF / GIF / BMP 走 `image` 整图解码，量级小得多（且 `tiff` 的 strip API 用法琐碎），
//!   留给需要时再补。
//!
//! ## 边界（诚实的部分）
//!
//! - **隔行扫描（Adam7）PNG 直接放弃**：`next_row` 出来的行不是从上到下的自然顺序，
//!   面积映射会错位。交给 `image` 走老路径，慢一点但正确。
//! - **APNG 放弃**：多帧语义我们本来也不支持。
//! - 逐行解码中途只看 `cancel::checkpoint()`（每 64 行一次），单个 zlib 块内部打断不了。

use std::io::BufReader;
use std::path::Path;

use qlr_core::{Error, Result};

use crate::canvas::Bitmap;

/// 低于这个像素量就不值得折腾流式（小图整解比逐行还快）。
pub const STREAM_MIN_PIXELS: u64 = 4_000_000;

/// 每解这么多行问一次"还要不要"。
const CANCEL_EVERY_ROWS: u32 = 64;

/// 把 PNG 直接解成 `dst_w × dst_h`（面积平均），中途内存不随源图尺寸增长。
///
/// 返回值：
/// - `Ok(Some(bitmap))`：成功；
/// - `Ok(None)`：这个 PNG 不适合走流式（隔行 / APNG / 未知色型），调用方回退整图解码；
/// - `Err(..)`：文件坏了或读不了。
pub fn decode_png_boxed(path: &Path, dst_w: u32, dst_h: u32) -> Result<Option<Bitmap>> {
    let dst_w = dst_w.max(1) as usize;
    let dst_h = dst_h.max(1) as usize;

    let file = std::fs::File::open(path).map_err(|e| Error::Message(format!("打不开图片：{e}")))?;
    let mut decoder = png::Decoder::new(BufReader::new(file));
    // 调色板/低灰度展开成 RGB(A)，16 位降到 8 位：这样后面只有 3 或 4 字节/像素两种情形。
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);

    let mut reader = decoder
        .read_info()
        .map_err(|e| Error::Message(format!("读 PNG 头失败：{e}")))?;

    let (src_w, src_h, channels) = {
        let info = reader.info();
        if info.interlaced || info.animation_control().is_some() {
            return Ok(None);
        }
        // 注意看**输出**色型而不是原始色型：`EXPAND` 已经把索引色/低位深展开，
        // `info.color_type` 仍是文件里写的那个（比如 Indexed），照它算字节数会错位。
        let (out_color, _out_depth) = reader.output_color_type();
        let channels = match out_color {
            png::ColorType::Rgb => 3,
            png::ColorType::Rgba => 4,
            png::ColorType::Grayscale => 1,
            png::ColorType::GrayscaleAlpha => 2,
            // EXPAND 之后不该再出现索引色/16 位；出现就只能回退。
            _ => return Ok(None),
        };
        (info.width, info.height, channels)
    };
    if src_w == 0 || src_h == 0 {
        return Ok(None);
    }
    // 不需要缩（或要放大）时流式没有意义：整图解码 + `thumbnail` 更快。
    if dst_w as u32 >= src_w && dst_h as u32 >= src_h {
        return Ok(None);
    }

    // 源列 → 目标列（一次算好，避免每行再做除法）。
    let map_x = column_map(src_w as usize, dst_w);

    let mut sums = vec![0u32; dst_w * 4];
    let mut counts = vec![0u32; dst_w];
    let mut out = vec![0u8; dst_w * dst_h * 4];

    let mut y: u32 = 0;
    let mut cur_dst_row: Option<usize> = None;
    // png 0.18 的 `next_row()` 直接借出一行的切片（不用自己备缓冲）。
    while let Some(row) = reader
        .next_row()
        .map_err(|e| Error::Message(format!("解码 PNG 失败（第 {y} 行）：{e}")))?
    {
        let line = row.data();
        if line.len() < src_w as usize * channels {
            return Err(Error::Message(format!(
                "PNG 第 {y} 行长度异常：{} < {}",
                line.len(),
                src_w as usize * channels
            )));
        }
        let dst_row = ((y as u64 * dst_h as u64 / src_h as u64) as usize).min(dst_h - 1);
        if cur_dst_row != Some(dst_row) {
            if let Some(prev) = cur_dst_row {
                flush_row(&mut out, prev, &sums, &counts, dst_w);
                sums.fill(0);
                counts.fill(0);
            }
            cur_dst_row = Some(dst_row);
        }
        accumulate_line(line, channels, &map_x, &mut sums, &mut counts);
        y += 1;
        if y % CANCEL_EVERY_ROWS == 0 {
            crate::cancel::checkpoint()?;
        }
    }
    if let Some(prev) = cur_dst_row {
        flush_row(&mut out, prev, &sums, &counts, dst_w);
    }
    crate::cancel::checkpoint()?;
    Ok(Some(Bitmap::from_rgba(dst_w as u32, dst_h as u32, out)))
}

/// 源列 → 目标列：`x * dst / src`，一次算好。
fn column_map(src_w: usize, dst_w: usize) -> Vec<u32> {
    (0..src_w)
        .map(|x| ((x as u64 * dst_w as u64 / src_w as u64) as u32).min(dst_w as u32 - 1))
        .collect()
}

/// 把一行源像素按列映射累加进目标行。
fn accumulate_line(
    line: &[u8],
    channels: usize,
    map_x: &[u32],
    sums: &mut [u32],
    counts: &mut [u32],
) {
    for (x, &tx) in map_x.iter().enumerate() {
        let base = x * channels;
        let (r, g, b, a) = match channels {
            1 => (line[base], line[base], line[base], 255),
            2 => (line[base], line[base], line[base], line[base + 1]),
            3 => (line[base], line[base + 1], line[base + 2], 255),
            _ => (line[base], line[base + 1], line[base + 2], line[base + 3]),
        };
        let t = tx as usize * 4;
        sums[t] += r as u32;
        sums[t + 1] += g as u32;
        sums[t + 2] += b as u32;
        sums[t + 3] += a as u32;
        counts[tx as usize] += 1;
    }
}

/// 把累加器写成一行 RGBA（按计数取平均），没被覆盖到的列填不透明黑。
fn flush_row(out: &mut [u8], dst_row: usize, sums: &[u32], counts: &[u32], dst_w: usize) {
    let base = dst_row * dst_w * 4;
    for (tx, &n) in counts.iter().enumerate().take(dst_w) {
        let o = base + tx * 4;
        if n == 0 {
            out[o] = 0;
            out[o + 1] = 0;
            out[o + 2] = 0;
            out[o + 3] = 255;
            continue;
        }
        let s = tx * 4;
        for c in 0..4 {
            out[o + c] = ((sums[s + c] + n / 2) / n) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 生成一张 `w × h` 的 RGBA PNG，每行颜色不同（便于验证"缩完还是每行不同"）。
    fn write_png(name: &str, w: u32, h: u32) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/qlrust-png-test");
        std::fs::create_dir_all(&dir).expect("建夹");
        let path = dir.join(name);
        let file = std::fs::File::create(&path).expect("建文件");
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);

        let mut writer = enc.write_header().expect("写头");
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for _x in 0..w {
                // 每行一个灰度值；alpha 固定 255。
                let v = (y * 255 / h.max(1)) as u8;
                data.extend_from_slice(&[v, v, v, 255]);
            }
        }
        writer.write_image_data(&data).expect("写像素");
        drop(writer);
        path
    }

    #[test]
    fn downscales_to_the_requested_size() {
        let path = write_png("boxed-basic.png", 64, 48);
        let bitmap = decode_png_boxed(&path, 16, 12)
            .expect("解码")
            .expect("走流式");
        assert_eq!((bitmap.width, bitmap.height), (16, 12));
        assert_eq!(bitmap.bgra.len(), 16 * 12 * 4);
    }

    #[test]
    fn rows_stay_in_order_and_alpha_is_opaque() {
        let path = write_png("boxed-rows.png", 32, 64);
        let bitmap = decode_png_boxed(&path, 8, 8)
            .expect("解码")
            .expect("走流式");
        // 源图自上而下由黑到白，缩完第一行必须比最后一行暗。
        let first = bitmap.bgra[0] as u32 + bitmap.bgra[1] as u32 + bitmap.bgra[2] as u32;
        let last_off = (7 * 8) * 4;
        let last = bitmap.bgra[last_off] as u32
            + bitmap.bgra[last_off + 1] as u32
            + bitmap.bgra[last_off + 2] as u32;
        assert!(first < last, "第一行 {first} 应比最后一行 {last} 暗");
        // 全部不透明（BGRA 里 alpha 在 +3）。
        assert!(bitmap.bgra.chunks(4).all(|px| px[3] == 255));
    }

    #[test]
    fn upscale_requests_fall_back_to_the_generic_path() {
        let path = write_png("boxed-bigger.png", 16, 16);
        // 目标比源还大 → 不走流式（返回 None 让调用方整图解码）。
        assert!(decode_png_boxed(&path, 64, 64).expect("解码").is_none());
    }

    /// 调色板 PNG 走 `EXPAND` 会变成 RGB（3 字节/像素），单独钉一下这条色型分支。
    #[test]
    fn palette_png_expands_to_rgb() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/qlrust-png-test");
        std::fs::create_dir_all(&dir).expect("建夹");
        let path = dir.join("boxed-palette.png");
        let file = std::fs::File::create(&path).expect("建文件");
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), 32, 32);
        enc.set_color(png::ColorType::Indexed);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_palette(vec![0, 0, 0, 255, 255, 255]);
        let mut writer = enc.write_header().expect("写头");
        let mut data = Vec::with_capacity(32 * 32);
        for y in 0..32u8 {
            for _ in 0..32 {
                data.push(y / 16); // 上半黑、下半白
            }
        }
        writer.write_image_data(&data).expect("写像素");
        drop(writer);

        let bitmap = decode_png_boxed(&path, 8, 8)
            .expect("解码")
            .expect("走流式");
        assert_eq!((bitmap.width, bitmap.height), (8, 8));
        // 上半应接近黑、下半接近白。
        let top = bitmap.bgra[0] as u32;
        let bottom_off = (7 * 8) * 4;
        let bottom = bitmap.bgra[bottom_off] as u32;
        assert!(top < 64, "上半应接近黑，实际 {top}");
        assert!(bottom > 191, "下半应接近白，实际 {bottom}");
    }

    #[test]
    fn broken_png_reports_an_error() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/qlrust-png-test");
        std::fs::create_dir_all(&dir).expect("建夹");
        let path = dir.join("boxed-broken.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\nnot really a png").expect("写");
        assert!(decode_png_boxed(&path, 8, 8).is_err());
    }

    #[test]
    fn column_map_stays_in_range() {
        let map = column_map(1000, 37);
        assert_eq!(map.len(), 1000);
        assert!(map.iter().all(|&t| (t as usize) < 37));
        // 单调不减：缩小时列序不能乱。
        assert!(map.windows(2).all(|w| w[0] <= w[1]));
    }
}
