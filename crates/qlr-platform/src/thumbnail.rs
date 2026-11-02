//! Windows 缩略图（`IShellItemImageFactory::GetImage`）——给"大图首帧"当第一段用。
//!
//! 为什么需要它（1.8.3，见 `docs/dev/CONVENTIONS.md` #153）：48MP PNG 的全量解码
//! 实测 197 ms（DEFLATE 88.8 ms + 逐像素累加 ~105 ms），而 PNG 是**单条 zlib 流**，
//! 既不能并行也不能"只解一块"，所以"一次就出全清晰帧"进不了 30 ms。
//! 唯一能立刻出画面的来源是系统缩略图缓存/自有缩略图提供器：
//! 命中缓存时是一次内存拷贝（<5 ms），没命中时它自己做一次缩略解码，
//! 通常也比"我们解全图再缩"便宜（它只要 256~1024 那一档）。
//!
//! 约定：
//! - 调用线程必须已经 `CoInitializeEx(APARTMENTTHREADED)`（`selection::init_com()`）；
//! - 任何失败都返回 `None`：这只是"锦上添花的第一段"，拿不到就退回全量解码；
//! - 返回的像素是 **BGRA、top-down、alpha 全 255**（`Canvas::Bitmap` 要的格式）。
//!   `GetImage` 给的 HBITMAP 往往没有 alpha 通道，这里统一补成不透明。

use std::path::Path;

use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDIBits, HGDIOBJ,
};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_INCACHEONLY,
    SIIGBF_RESIZETOFIT, SIIGBF_THUMBNAILONLY,
};
use windows::core::PCWSTR;

use crate::wide::to_wide;

const SCOPE: &str = "thumbnail";

/// 一张缩略图：BGRA、top-down、`width * height * 4` 字节。
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// 向 Shell 要一张不超过 `max_px` 的缩略图。拿不到（没有处理器、COM 没初始化、
/// 文件不被认作图片……）一律 `None`。
///
/// `max_px` 是**长边**上限：窗口是 1589×924 时传 1589，Shell 会按比例缩到盒子里。
///
/// `cached_only = true` 时加 `SIIGBF_THUMBNAILONLY`：**只接受已有的缩略图缓存**，
/// 没缓存就直接失败（实测 <2 ms），不会替我们当场解码 —— 这才是第一段想要的行为。
/// 为什么要这个开关（1.8.3 实测）：不加它时 Shell 会自己解码 48MP PNG，
/// 冷文件 124–184 ms，比我们自己的全量解码（197 ms）只快一点点，还要多占一次解码时间；
/// 加了之后：有缓存 1–5 ms（用户真正想要的手感），没缓存 0 成本跳过。
pub fn shell_thumbnail(path: &Path, max_px: u32, cached_only: bool) -> Option<Thumbnail> {
    let started = std::time::Instant::now();
    let wide = to_wide(&path.to_string_lossy());
    // SHCreateItemFromParsingName 既要 COM 已经初始化，也允许返回"不支持该文件"的 E_FAIL。
    let factory: IShellItemImageFactory = unsafe {
        SHCreateItemFromParsingName::<_, _, IShellItemImageFactory>(PCWSTR(wide.as_ptr()), None)
    }
    .ok()?;
    let size = SIZE {
        cx: max_px.max(1) as i32,
        cy: max_px.max(1) as i32,
    };
    // BIGGERSIZEOK：允许 Shell 直接给缓存里更大的那张（它自己会缩）。
    // RESIZETOFIT：保持比例塞进盒子，不会被拉变形（形状正确是第一段的唯一要求）。
    let mut flags = SIIGBF_RESIZETOFIT | SIIGBF_BIGGERSIZEOK;
    if cached_only {
        // 两个旗标都要：THUMBNAILONLY 排除"图标当缩略图"，
        // INCACHEONLY 才是"只读已经缓存的、绝不现解"。只加 THUMBNAILONLY 时
        // 系统仍会当场解码（实测 48MP PNG 144 ms），等于白等。
        flags |= SIIGBF_THUMBNAILONLY | SIIGBF_INCACHEONLY;
    }
    let hbmp = unsafe { factory.GetImage(size, flags) }.ok()?;
    let thumb = unsafe { bitmap_to_bgra(hbmp) };
    unsafe {
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
    }
    let elapsed = started.elapsed().as_millis();
    match thumb {
        Some(t) => {
            qlr_core::log_debug!(
                SCOPE,
                "SHELLTHUMB {}x{} elapsed={elapsed}ms cached_only={cached_only}",
                t.width,
                t.height
            );
            Some(t)
        }
        None => {
            qlr_core::log_debug!(SCOPE, "SHELLTHUMB 读像素失败 elapsed={elapsed}ms");
            None
        }
    }
}

/// HBITMAP → 32bpp top-down BGRA。失败返回 `None`（调用方继续走全量解码）。
unsafe fn bitmap_to_bgra(hbmp: windows::Win32::Graphics::Gdi::HBITMAP) -> Option<Thumbnail> {
    // 先问尺寸：GetDIBits 需要知道高度，而 HBITMAP 的尺寸只有 GetObject 能给。
    let mut bm = windows::Win32::Graphics::Gdi::BITMAP::default();
    let got = unsafe {
        windows::Win32::Graphics::Gdi::GetObjectW(
            HGDIOBJ(hbmp.0),
            core::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut core::ffi::c_void),
        )
    };
    if got == 0 {
        return None;
    }
    let (width, height) = (bm.bmWidth, bm.bmHeight);
    if width <= 0 || height <= 0 {
        return None;
    }
    let (w, h) = (width as u32, height as u32);
    let mut bgra = vec![0u8; w as usize * h as usize * 4];

    let dc = unsafe { CreateCompatibleDC(None) };
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // 负数 = top-down：与 `Canvas::Bitmap` 的行序一致，省一次翻转
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let copied = unsafe {
        GetDIBits(
            dc,
            hbmp,
            0,
            h,
            Some(bgra.as_mut_ptr() as *mut core::ffi::c_void),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    unsafe {
        let _ = DeleteDC(dc);
    }
    if copied == 0 {
        return None;
    }
    // Shell 的 HBITMAP 基本都不带 alpha（32bpp 的第 4 字节是 0），直接用会整张透明。
    for px in bgra.chunks_exact_mut(4) {
        px[3] = 255;
    }
    Some(Thumbnail {
        width: w,
        height: h,
        bgra,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实桌面下量一次 48MP PNG 的缩略图耗时（`#[ignore]`：要真 Shell + 真夹具）。
    ///
    /// 跑法：`cargo test -p qlr-platform shell_thumbnail -- --ignored --nocapture`
    /// 判据：命中缩略图缓存时要 <10 ms；冷缓存几十毫秒也算合格（比 197 ms 的全量解码快）。
    #[test]
    #[ignore]
    fn shell_thumbnail_48mp_timing() {
        let Some(dir) = std::env::var_os("QLRUST_FIXTURES")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("USERPROFILE").map(|p| {
                    std::path::PathBuf::from(p)
                        .join("Desktop")
                        .join("qlrust-dev")
                })
            })
        else {
            return;
        };
        let path = dir.join("48mp_8000x6000.png");
        if !path.exists() {
            eprintln!("缺夹具，跳过：{}", path.display());
            return;
        }
        // 同一文件夹里的其它图也量一下：Explorer 看过的文件也许已经进了缩略图缓存。
        for name in ["png_1920x1080.png", "4k_3840x2160.png", "8mp_3840x2160.jpg"] {
            let p = dir.join(name);
            if !p.exists() {
                continue;
            }
            if !crate::selection::init_com() {
                return;
            }
            let t = std::time::Instant::now();
            let got = shell_thumbnail(&p, 256, true);
            eprintln!(
                "{name} cached_only=256 -> {:?} {} ms",
                got.map(|t| (t.width, t.height)),
                t.elapsed().as_millis()
            );
            crate::selection::uninit_com();
        }
        assert!(crate::selection::init_com(), "需要 STA");
        // 把"建 shell item"和"取图"分开量：两者一个是 COM 开销、一个是解码开销。
        {
            let t = std::time::Instant::now();
            let wide = to_wide(&path.to_string_lossy());
            let f: Option<IShellItemImageFactory> = unsafe {
                SHCreateItemFromParsingName::<_, _, IShellItemImageFactory>(
                    PCWSTR(wide.as_ptr()),
                    None,
                )
            }
            .ok();
            eprintln!("SHCreateItemFromParsingName {} us", t.elapsed().as_micros());
            if let Some(f) = f {
                for (label, flags) in [
                    ("THUMBNAILONLY", SIIGBF_THUMBNAILONLY),
                    ("INCACHEONLY", SIIGBF_THUMBNAILONLY | SIIGBF_INCACHEONLY),
                ] {
                    let t = std::time::Instant::now();
                    let got = unsafe { f.GetImage(SIZE { cx: 1589, cy: 1589 }, flags) };
                    eprintln!(
                        "GetImage({label}) {} us ok={}",
                        t.elapsed().as_micros(),
                        got.is_ok()
                    );
                    if let Ok(b) = got {
                        unsafe {
                            let _ = DeleteObject(HGDIOBJ(b.0));
                        }
                    }
                }
            }
        }
        // 冷/热缓存各量一次，另外再看小尺寸（系统缩略图缓存通常只存 ≤256）能不能命中。
        for max_px in [256u32, 1024, 1589] {
            for cached_only in [true, false] {
                let t = std::time::Instant::now();
                let got = shell_thumbnail(&path, max_px, cached_only);
                let ms = t.elapsed().as_millis();
                match got {
                    Some(t) => {
                        assert_eq!(t.bgra.len(), t.width as usize * t.height as usize * 4);
                        assert!(
                            t.bgra.chunks_exact(4).all(|px| px[3] == 255),
                            "alpha 必须补满"
                        );
                        eprintln!(
                            "max_px={max_px} cached_only={cached_only} -> {0}x{1} {ms}ms",
                            t.width, t.height
                        );
                    }
                    None => eprintln!(
                        "max_px={max_px} cached_only={cached_only} 拿不到缩略图（{ms}ms）"
                    ),
                }
            }
        }
        crate::selection::uninit_com();
    }
}
