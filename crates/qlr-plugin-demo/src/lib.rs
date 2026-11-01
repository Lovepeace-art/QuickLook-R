//! QLRust 示例插件：**视频占位图**。
//!
//! 这个插件的目的是把插件 ABI 的最小实现摆在你面前，而不是真的去解码视频——
//! 那需要 FFmpeg/LAVFilters 之类的重依赖，属于"以后单独开发"的部分。
//! 它接管 `.mp4/.mkv/.webm/.avi/.mov/.flv/.wmv`，画一张**按文件路径哈希着色**的色条图：
//! 同一个文件每次颜色一样、不同文件颜色不同，方便一眼看出"插件真的按文件渲染了"。
//!
//! 想写自己的插件：复制这个文件的骨架，改 `NAME` / `EXTS` / `render_image` 三处即可。
//! 详细契约见 `crates/qlr-plugin-api/src/lib.rs` 与 `docs/dev/PLUGINS.md`。

use qlr_plugin_api::{
    ABI_VERSION, MAX_PIXELS, QlrImage, QlrPluginFreeFn, QlrPluginInfo, QlrPluginInfoFn,
    QlrPluginRenderFn, RENDER_FAILED, RENDER_OK,
};

const NAME: &[u8] = "示例插件：视频占位图".as_bytes();
const EXTS: &[u8] = b"mp4;mkv;webm;avi;mov;flv;wmv;m4v";

/// 一张图的配色：主色 + 三种辅色，全部由路径哈希决定。
fn palette(seed: u32) -> [[u8; 3]; 4] {
    let hue = (seed % 360) as f32;
    let mut out = [[0u8; 3]; 4];
    for (i, slot) in out.iter_mut().enumerate() {
        let h = (hue + i as f32 * 24.0) % 360.0;
        *slot = hsv_to_rgb(h, 0.55, 0.86);
    }
    out
}

/// HSV → RGB（`h` 在 0..360，`s`/`v` 在 0..1）。自己写是为了零依赖。
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [u8; 3] {
    let c = v * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let to = |f: f32| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    [to(r), to(g), to(b)]
}

/// 路径哈希（FNV-1a 32 位）。只要稳定、够散，不需要抗碰撞。
fn hash_path(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in bytes {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// 画图：等宽色条 + 四周一圈白边 + 中间一条深色横带（"这是占位图"的视觉暗示）。
fn render_image(path: &[u8], max_w: u32, max_h: u32) -> (u32, u32, Vec<u8>) {
    let seed = hash_path(path);
    let colors = palette(seed);

    // 16:9 等比缩到宿主给的范围里；范围异常小时退回一个能看的最小尺寸。
    let (max_w, max_h) = (max_w.max(64), max_h.max(64));
    let mut w = max_w;
    let mut h = (w * 9 / 16).max(1);
    if h > max_h {
        h = max_h;
        w = (h * 16 / 9).max(1);
    }
    let (w, h) = (w.min(4096), h.min(4096));

    let mut rgba = vec![0u8; (w * h * 4) as usize];
    let bars = colors.len() as u32;
    let border = (w.min(h) / 32).max(2);
    for y in 0..h {
        for x in 0..w {
            let idx = ((y * w + x) * 4) as usize;
            let mut color =
                colors[((x as u64 * bars as u64) / w as u64).min(bars as u64 - 1) as usize];
            // 四周白边
            if x < border || y < border || x + border >= w || y + border >= h {
                color = [245, 245, 245];
            }
            // 中间深色横带（高度的 1/5）
            let band_lo = h / 2 - h / 10;
            let band_hi = h / 2 + h / 10;
            if y >= band_lo && y < band_hi {
                color = [24, 24, 28];
            }
            rgba[idx] = color[0];
            rgba[idx + 1] = color[1];
            rgba[idx + 2] = color[2];
            rgba[idx + 3] = 255;
        }
    }
    (w, h, rgba)
}

#[unsafe(no_mangle)]
pub extern "C" fn qlr_plugin_info() -> QlrPluginInfo {
    QlrPluginInfo {
        abi_version: ABI_VERSION,
        name: NAME.as_ptr(),
        name_len: NAME.len() as u32,
        extensions: EXTS.as_ptr(),
        extensions_len: EXTS.len() as u32,
    }
}

/// 渲染入口。
///
/// # Safety
///
/// 宿主保证：`path` 指向 `path_len` 字节的合法 UTF-8，`out` 指向可写的 `QlrImage`。
/// 本函数只做空指针检查，无法验证"指针是否真的可读"——那是宿主的责任。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qlr_plugin_render(
    path: *const u8,
    path_len: usize,
    max_width: u32,
    max_height: u32,
    out: *mut QlrImage,
) -> i32 {
    if out.is_null() || path.is_null() || path_len == 0 {
        return RENDER_FAILED;
    }
    // 宿主承诺 path 只在调用期间有效，所以这里只读、不留引用。
    let bytes = unsafe { core::slice::from_raw_parts(path, path_len) };
    let (w, h, rgba) = render_image(bytes, max_width, max_height);
    if w == 0 || h == 0 || w.saturating_mul(h) > MAX_PIXELS {
        return RENDER_FAILED;
    }
    let len = rgba.len();
    let boxed = rgba.into_boxed_slice();
    let ptr = Box::into_raw(boxed) as *mut u8;
    unsafe {
        *out = QlrImage {
            width: w,
            height: h,
            pixels: ptr,
            len,
        };
    }
    RENDER_OK
}

/// 释放入口。
///
/// # Safety
///
/// `image` 必须是 `qlr_plugin_render` 填过的那一份（或空指针）；宿主只会原样交回。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qlr_plugin_free(image: *mut QlrImage) {
    if image.is_null() {
        return;
    }
    // 宿主调用前会把指针/长度原样交回，这里按 Box<[u8]> 收尾；
    // 空指针（失败路径）直接跳过，绝不 free 一个野指针。
    let (ptr, len) = unsafe { ((*image).pixels, (*image).len) };
    if !ptr.is_null() && len > 0 {
        let slice = core::ptr::slice_from_raw_parts_mut(ptr, len);
        drop(unsafe { Box::from_raw(slice) });
    }
    unsafe {
        *image = QlrImage::empty();
    }
}

/// 让 rustc 帮我们把签名钉住：任何一个参数写错都会在这里报错。
const _: QlrPluginInfoFn = qlr_plugin_info;
const _: QlrPluginRenderFn = qlr_plugin_render;
const _: QlrPluginFreeFn = qlr_plugin_free;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_fits_inside_budget_keeping_aspect() {
        let (w, h, rgba) = render_image(b"a.mp4", 800, 600);
        assert!(w <= 800 && h <= 600);
        // 16:9 应该贴着宽边
        assert_eq!(w, 800);
        assert_eq!(h, 450);
        assert_eq!(rgba.len(), (w * h * 4) as usize);
    }

    #[test]
    fn tall_budget_uses_full_width() {
        let (w, _h, _) = render_image(b"a.mp4", 320, 1000);
        assert_eq!(w, 320);
    }

    #[test]
    fn same_path_same_colors_different_path_different_colors() {
        let a = render_image(b"/videos/a.mp4", 128, 128).2;
        let b = render_image(b"/videos/a.mp4", 128, 128).2;
        let c = render_image(b"/videos/b.mp4", 128, 128).2;
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn alpha_is_opaque_everywhere() {
        let (w, h, rgba) = render_image(b"x.mkv", 64, 64);
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        for px in rgba.chunks_exact(4) {
            assert_eq!(px[3], 255);
        }
    }

    #[test]
    fn info_reports_abi_and_extensions() {
        let info = qlr_plugin_info();
        assert_eq!(info.abi_version, ABI_VERSION);
        let exts = unsafe {
            core::str::from_utf8(core::slice::from_raw_parts(
                info.extensions,
                info.extensions_len as usize,
            ))
        }
        .expect("扩展名必须是 UTF-8");
        assert!(qlr_plugin_api::parse_extensions(exts).contains(&"mp4".to_string()));
    }

    #[test]
    fn null_out_is_rejected() {
        assert_eq!(
            unsafe { qlr_plugin_render(b"a.mp4".as_ptr(), 5, 100, 100, core::ptr::null_mut()) },
            RENDER_FAILED
        );
    }

    #[test]
    fn render_then_free_round_trip() {
        // 走一遍真实调用顺序（填图 → 读出来 → 交回去），验证内存归属没写反。
        let mut image = QlrImage::empty();
        let code = unsafe { qlr_plugin_render(b"/v/a.mp4".as_ptr(), 8, 160, 120, &mut image) };
        assert_eq!(code, RENDER_OK);
        assert_eq!(image.width, 160);
        assert_eq!(image.height, 90);
        assert_eq!(image.len, (160 * 90 * 4) as usize);
        let first = unsafe { *image.pixels };
        unsafe { qlr_plugin_free(&mut image) };
        assert!(image.pixels.is_null(), "free 之后必须清空指针");
        assert_eq!(image.len, 0);
        // 再渲染一次：free 之后内存不应该被重复释放 / 崩溃。
        let mut again = QlrImage::empty();
        assert_eq!(
            unsafe { qlr_plugin_render(b"/v/a.mp4".as_ptr(), 8, 160, 120, &mut again) },
            RENDER_OK
        );
        assert_eq!(
            unsafe { *again.pixels },
            first,
            "同路径同尺寸应得到同样像素"
        );
        unsafe { qlr_plugin_free(&mut again) };
    }
}
