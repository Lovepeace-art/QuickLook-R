//! 位图缩放：给"只缩"和"只放"两种情形各走一条又快又稳的路。
//!
//! ## 为什么不用 `image::imageops::thumbnail`
//!
//! 它是**逐输出像素**走泛型 `GenericImageView` 取样的：放大方向每个输出像素要 4 次
//! trait 调用 + 4 次 f32 乘加，1920×1080 放大到 3878×2181 实测 **168 ms**
//! （缩小方向 1589×894 也要 20 ms）。而缩放在预览里是"每滚一格就要做一次"的操作，
//! 这条开销比解码本身（同一张 PNG 解码只有 3–5 ms）大一到两个数量级。
//!
//! ## 两条路的口径
//!
//! - **缩小（两条边都不放大）**：与 `thumbnail` 的 `thumbnail_sample_block` 分支
//!   **逐字节一致**——同样的 `ceil` 边界、同样的 `(sum + n/2) / n` 取整。
//!   有单测 `downscale_matches_image_crate` 钉住，所以历史视觉基线不会变。
//! - **放大（两条边都不缩小）**：8 位定点双线性（视觉与 `thumbnail` 的分支等价，
//!   误差 ≤1 级）。放大原本没有"逐字节基线"要求，但要求"不糊不乱"，
//!   双线性是预览放大最稳的选择。
//! - **一条边放一条边缩**（极端长条）：返回 `None`，调用方退回 `thumbnail`。
//!   这种情形少见，不值得为它复制第三套边界算术。

use crate::canvas::Bitmap;

/// 定点权重分母：8 位小数（0..=256），`(a*(256-w) + b*w) >> 8` 的误差 < 1/256 级。
const FIX_ONE: u32 = 256;

/// 把 RGBA8 源图缩/放到 `(dw, dh)`，输出 BGRA（上屏顺序）。
///
/// `src` 的长度必须等于 `sw * sh * 4`；不满足时返回 `None`（调用方走老路径）。
pub fn scale_rgba_to_bgra(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Option<Bitmap> {
    let need = sw as usize * sh as usize * 4;
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 || src.len() != need {
        return None;
    }
    if dw <= sw && dh <= sh {
        return Some(downscale_exact(src, sw, sh, dw, dh));
    }
    if dw >= sw && dh >= sh {
        return Some(upscale_bilinear(src, sw, sh, dw, dh));
    }
    // 一维放一维缩：交给 `image::imageops::thumbnail`（见模块头注释）。
    None
}

/// 输出维度到源维度的覆盖区间 `[lo, hi)`——**逐字照抄** `image` 的
/// `thumbnail`：`lo = clamp(ceil(i*ratio), 0, n-1)`、`hi = clamp(ceil((i+1)*ratio), lo, n)`。
///
/// 之所以照抄而不是"自己写个更合理的"：缩小方向的历史像素口径就是这套算术，
/// 换了基线截图就不再逐字节一致。`ratio >= 1`（缩小）时 `hi > lo` 恒成立，区间是源列的一个划分。
fn cover_ranges(dst_n: u32, src_n: u32) -> Vec<(u32, u32)> {
    let ratio = src_n as f32 / dst_n as f32;
    (0..dst_n)
        .map(|i| {
            let lo_f = i as f32 * ratio;
            let hi_f = lo_f + ratio;
            let lo = (lo_f.ceil() as u32).clamp(0, src_n - 1);
            let hi = (hi_f.ceil() as u32).clamp(lo, src_n);
            (lo, hi)
        })
        .collect()
}

/// 盒式缩小（面积平均），与 `image::imageops::thumbnail` 逐字节一致。
///
/// 快的来源：区间一次算好、按"源像素只遍历一遍"累加、整数取整；
/// 老实现是每个输出像素内部再遍历一遍源矩形 + 泛型取样。
fn downscale_exact(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Bitmap {
    let cols = cover_ranges(dw, sw);
    let rows = cover_ranges(dh, sh);
    let sw = sw as usize;
    let dw = dw as usize;
    let dh = dh as usize;

    let mut out = vec![0u8; dw * dh * 4];
    let mut sums = vec![0u32; dw * 4];

    for (dy, &(lo, hi)) in rows.iter().enumerate() {
        sums.fill(0);
        for y in lo as usize..hi as usize {
            let line = &src[y * sw * 4..(y + 1) * sw * 4];
            // 逐源像素累加到它所属的输出列：列区间是划分，所以每个源像素只加一次。
            //
            // 累加写成"整行切片 + chunks_exact(4) + 行内局部累加器"而不是按下标取
            // 4 个字节：下标版本每读一个通道都要过一次边界检查，8 MP 的图上就是
            // 几千万次。实测（release）：
            //   4K → 3178   51.4 → 46.1 ms      4K → 1986   28.3 → 21.6 ms
            //   1080p → 1589 12.7 → 11.5 ms     48MP → 3178  100.6 → 71.2 ms
            // 纯标量，没有手写 SIMD；取整口径没动，逐字节基线仍由
            // `downscale_matches_image_crate` 钉住。
            let mut out = sums.chunks_exact_mut(4);
            for &(cl, cr) in &cols {
                let band = &line[cl as usize * 4..cr as usize * 4];
                let mut acc = [0u32; 4];
                for px in band.chunks_exact(4) {
                    acc[0] += px[0] as u32;
                    acc[1] += px[1] as u32;
                    acc[2] += px[2] as u32;
                    acc[3] += px[3] as u32;
                }
                let sum = out.next().expect("输出列数与 sums 等长");
                sum[0] += acc[0];
                sum[1] += acc[1];
                sum[2] += acc[2];
                sum[3] += acc[3];
            }
        }
        // 与 `thumbnail_sample_block` 同一套取整：n = 列数 × 行数，round = n/2。
        let rows_n = hi - lo;
        let base = dy * dw * 4;
        for (dx, &(cl, cr)) in cols.iter().enumerate() {
            let n = (cr - cl) * rows_n;
            let round = n / 2;
            let s = dx * 4;
            let o = base + dx * 4;
            let px = [
                (sums[s] + round) / n,
                (sums[s + 1] + round) / n,
                (sums[s + 2] + round) / n,
                (sums[s + 3] + round) / n,
            ];
            // RGBA → BGRA：交换 R/B（alpha 不动）。
            out[o] = px[2].min(255) as u8;
            out[o + 1] = px[1].min(255) as u8;
            out[o + 2] = px[0].min(255) as u8;
            out[o + 3] = px[3].min(255) as u8;
        }
    }
    Bitmap::new(dw as u32, dh as u32, out)
}

/// 源坐标 → `(x0, x1, 权重)`：中心对齐的双线性（像素中心落在 `(i+0.5)*src/dst - 0.5`）。
fn axis_taps(dst_n: u32, src_n: u32) -> Vec<(usize, usize, u32)> {
    let ratio = src_n as f32 / dst_n as f32;
    (0..dst_n)
        .map(|i| {
            let u = (i as f32 + 0.5) * ratio - 0.5;
            let u = u.clamp(0.0, (src_n - 1) as f32);
            let x0 = u.floor() as usize;
            let x1 = (x0 + 1).min(src_n as usize - 1);
            let frac = ((u - x0 as f32) * FIX_ONE as f32).round() as u32;
            (x0, x1, frac.min(FIX_ONE))
        })
        .collect()
}

/// 双线性放大（只放大，不缩小）。
///
/// 两段定点的理由：水平方向的结果按源行缓存一次，同一对源行通常服务 1–2 个输出行，
/// 均摊下来每个输出像素只剩 4 次乘法（垂直混合），而不是 12 次。
fn upscale_bilinear(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Bitmap {
    let xs = axis_taps(dw, sw);
    let ys = axis_taps(dh, sh);
    let dw = dw as usize;
    let dh = dh as usize;
    let sw = sw as usize;

    let mut out = vec![0u8; dw * dh * 4];
    // 水平混合结果（0..=65280，u16 装得下）：每行 dw*4 个通道。
    let mut h0 = vec![0u16; dw * 4];
    let mut h1 = vec![0u16; dw * 4];
    let mut cached_y0 = usize::MAX;

    for (oy, &(y0, y1, fy)) in ys.iter().enumerate() {
        if cached_y0 != y0 {
            blend_row(&src[y0 * sw * 4..(y0 + 1) * sw * 4], &xs, &mut h0);
            blend_row(&src[y1 * sw * 4..(y1 + 1) * sw * 4], &xs, &mut h1);
            cached_y0 = y0;
        }
        let base = oy * dw * 4;
        let inv = FIX_ONE - fy;
        for i in 0..dw * 4 {
            let v = (h0[i] as u32 * inv + h1[i] as u32 * fy + 32768) >> 16;
            out[base + i] = v.min(255) as u8;
        }
        // 上一步写的是 RGBA 顺序，这里就地换成 BGRA（一次交换，和 `Bitmap::from_rgba` 等价）。
        for px in out[base..base + dw * 4].chunks_exact_mut(4) {
            px.swap(0, 2);
        }
    }
    Bitmap::new(dw as u32, dh as u32, out)
}

/// 一行源像素 → 水平混合成 `dw*4` 个 `u16`。
fn blend_row(line: &[u8], xs: &[(usize, usize, u32)], dst: &mut [u16]) {
    for (i, &(x0, x1, w)) in xs.iter().enumerate() {
        let inv = FIX_ONE - w;
        let (a, b) = (x0 * 4, x1 * 4);
        let o = i * 4;
        for c in 0..4 {
            dst[o + c] = (line[a + c] as u32 * inv + line[b + c] as u32 * w) as u16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一张有纹理的 RGBA 图（避免纯色掩盖取样错误）。
    fn sample(sw: u32, sh: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((sw * sh * 4) as usize);
        for y in 0..sh {
            for x in 0..sw {
                v.push(((x * 7 + y * 3) & 0xFF) as u8);
                v.push(((x * 11 + y * 5) & 0xFF) as u8);
                v.push(((x * 13 + y * 17) & 0xFF) as u8);
                v.push(255);
            }
        }
        v
    }

    /// 缩小方向必须与 `image::imageops::thumbnail` **逐字节一致**：
    /// 这是"换了实现但视觉基线不动"的唯一凭据。
    #[test]
    fn downscale_matches_image_crate() {
        let cases = [
            (1920u32, 1080u32, 1589u32, 894u32),
            (1920, 1080, 960, 540),
            (1000, 700, 333, 233),
            (641, 479, 320, 240),
            (97, 61, 13, 7),
            (1920, 1080, 1920, 1080),
        ];
        for (sw, sh, dw, dh) in cases {
            let src = sample(sw, sh);
            let img = image::RgbaImage::from_raw(sw, sh, src.clone()).expect("源图");
            let want = image::imageops::thumbnail(&img, dw, dh);
            let got = scale_rgba_to_bgra(&src, sw, sh, dw, dh).expect("缩小应走快路径");
            assert_eq!((got.width, got.height), (dw, dh));
            for (i, px) in want.pixels().enumerate() {
                let [r, g, b, a] = px.0;
                let o = i * 4;
                assert_eq!(
                    &got.bgra[o..o + 4],
                    &[b, g, r, a],
                    "{sw}x{sh} -> {dw}x{dh} 第 {i} 个像素不一致"
                );
            }
        }
    }

    /// 放大：纯色必须保持纯色，且两条边都翻倍时尺寸对。
    #[test]
    fn upscale_keeps_solid_colour() {
        let src = [10u8, 20, 30, 255].repeat(4 * 3); // 4x3 纯色
        let out = scale_rgba_to_bgra(&src, 4, 3, 16, 12).expect("放大应走快路径");
        assert_eq!((out.width, out.height), (16, 12));
        for px in out.bgra.chunks_exact(4) {
            // 纯色在双线性下不允许出现任何偏移（四邻点相同 → 加权和就是原值）。
            assert_eq!(
                px,
                &[30, 20, 10, 255],
                "RGBA(10,20,30) 应变成 BGRA(30,20,10)"
            );
        }
    }

    /// 一维放一维缩、以及非法缓冲：返回 None，由调用方退回老路径。
    #[test]
    fn mixed_axis_and_bad_buffer_fall_back() {
        let src = sample(100, 50);
        assert!(scale_rgba_to_bgra(&src, 100, 50, 200, 25).is_none());
        assert!(scale_rgba_to_bgra(&src, 100, 50, 50, 100).is_none());
        // 缓冲长度与尺寸对不上（解码器给了半张图）也不能读越界。
        assert!(scale_rgba_to_bgra(&src[..12], 100, 50, 10, 10).is_none());
        assert!(scale_rgba_to_bgra(&[], 0, 0, 10, 10).is_none());
    }

    /// 手测用（CI 不跑）：缩放本身的耗时——预览里"每滚一格"都要付这笔钱，
    /// JPEG 解码进缓存之后它就是剩余开销的大头。
    ///
    /// ```text
    /// cargo test --release -p qlr-render --lib bench_downscale -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn bench_downscale() {
        use std::time::Instant;
        for (label, sw, sh, dw, dh) in [
            (
                "4K → 1.25 倍档（最坏情况，几乎不缩）",
                3840u32,
                2160u32,
                3178u32,
                1788u32,
            ),
            ("4K → 适应窗口", 3840, 2160, 1986, 1118),
            ("1080p → 适应窗口", 1920, 1080, 1589, 894),
            ("48MP → 1.25 倍档", 8000, 6000, 3178, 1788),
        ] {
            let src = sample(sw, sh);
            // 跑两遍取第二遍：第一遍要把源缓冲读进缓存。
            let mut best = f64::MAX;
            for _ in 0..2 {
                let t = Instant::now();
                let out = scale_rgba_to_bgra(&src, sw, sh, dw, dh).expect("缩小路");
                best = best.min(t.elapsed().as_secs_f64() * 1000.0);
                std::hint::black_box(out);
            }
            println!("{label}: {sw}x{sh} → {dw}x{dh}  {best:.1} ms");
        }
    }
}
