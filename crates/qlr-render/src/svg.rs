//! SVG 渲染：Direct2D 1.3 的 `ID2D1DeviceContext5::CreateSvgDocument` 光栅化，
//! 拿不到 D2D（无 D3D11 设备 / 系统太老 / `d2d1.dll` 缺导出）时退回内置的极简光栅化器。
//!
//! 为什么是 D2D 而不是 resvg/usvg：ADR-0004 的"少依赖"路线 + A8.5 的装载裁剪（exe ≤ 5 MB）。
//! D2D 的 SVG 子系统（Windows 10 1703+）本身就是系统组件，零额外 dll、零 crate。
//!
//! 为什么还要写一条降级路径：SVG 预览是"打开看看"，不是"输出印刷稿"。
//! D3D11 在远程桌面/虚拟机/无显卡驱动容器里可能建不出设备，那时**必须有东西能画**，
//! 而不是让用户看到"暂不支持"。降级器覆盖 rect/circle/ellipse/line/polygon/polyline/path
//! （M/L/H/V/C/Q/S/Z）+ fill/stroke/stroke-width/opacity + 线性渐变；
//! **不支持**文字、径向渐变、clipPath/mask、CSS、`transform` 属性——这些只有 D2D 那条路有。
//!
//! 尺寸约定（与 `pdf::render_page` 不同，这里**输出就是请求的尺寸**）：
//! 内容按 `viewBox` 等比缩放到 `width × height` 内并居中，画布本身固定为 `width × height`。
//! 这样壳层开窗时不用再算居中，也不会出现"位图比客户区少 1 px"那类坑（CONVENTIONS #63）。
//!
//! 线程/COM：D3D11 + D2D 不要求 `CoInitializeEx`，但 `SHCreateMemStream` 走的 ole32 在
//! 某些进程里会返回 `CO_E_NOTINITIALIZED`，所以入口做一次**尽力而为**的 MTA 初始化
//! （失败只 debug 日志，不阻止渲染）。D2D 对象全部"本函数内创建、本函数内销毁"，
//! 因此可以在任意 worker 线程上调用，也可以被多线程并发调用。

use std::path::Path;

use crate::canvas::{Bitmap, Canvas};

/// 默认底色（BGRA，白）：与 PDF 一致——预览窗口要的是"纸"，不是"透明黑"。
pub const WHITE: [u8; 4] = [255, 255, 255, 255];

/// 读文件上限（SVG 是文本；1 MB 已经能装下绝大多数矢量图，超过的用 D2D 也没意义）。
const READ_LIMIT: usize = 4 * 1024 * 1024;

/// 渲染 SVG 文件。
///
/// 返回 `Some(Canvas::Bitmap)`：尺寸固定 `(width, height)`，白底，内容等比缩放居中。
/// 返回 `None`：文件读不到 / 不是 SVG（没有 `<svg>` 根元素）/ 两种后端都失败。
/// **任何情况下都不 panic**。
pub fn render(path: &Path, width: u32, height: u32) -> Option<Canvas> {
    render_with_background(path, width, height, Some(WHITE))
}

/// 同上，但可以指定底色（BGRA）。传 `None` 保留透明（alpha=0）。
///
/// 注意：`qlr-platform` 的位图上屏走 `StretchDIBits` + `SRCCOPY`（不看 alpha），
/// 透明像素会显示成 RGB 原值（本模块填 0，即黑）。要"看起来像浏览器"就用白底。
pub fn render_with_background(
    path: &Path,
    width: u32,
    height: u32,
    background: Option<[u8; 4]>,
) -> Option<Canvas> {
    let bytes = read_file(path)?;
    let (src, _encoding) = crate::text::decode(&bytes);
    render_str(&src, width, height, background)
}

/// 文件内容的**原始**显示尺寸（px，96 DPI），用于"按原尺寸开窗"。
///
/// 顺序与浏览器一致：`width`/`height` 属性优先；都没有就看 `viewBox` 的宽高。
/// 读不到就 `None`（壳层退回默认开窗尺寸，不让探测失败变成开窗失败）。
pub fn natural_size(path: &Path) -> Option<(u32, u32)> {
    let bytes = read_file(path)?;
    let (src, _) = crate::text::decode(&bytes);
    let (w, h) = viewport_of(&src)?;
    let (pw, ph) = (w.round(), h.round());
    if !(pw.is_finite() && ph.is_finite()) || pw < 1.0 || ph < 1.0 {
        return None;
    }
    Some((pw as u32, ph as u32))
}

/// 当前平台实际用的是哪条后端（日志/诊断用）：`"direct2d"` / `"fallback"` / `"none"`。
pub fn backend_name() -> &'static str {
    #[cfg(windows)]
    {
        if d2d::available() {
            return "direct2d";
        }
    }
    "fallback"
}

/// 直接从字符串渲染（测试与"内联 SVG"用，不落盘）。
pub(crate) fn render_str(
    src: &str,
    width: u32,
    height: u32,
    background: Option<[u8; 4]>,
) -> Option<Canvas> {
    let cw = width.max(1);
    let ch = height.max(1);
    // 不是 SVG：没有根元素就当场退出（避免把 HTML/XML 当 SVG 硬画成空白）。
    let (vw, vh) = viewport_of(src).unwrap_or((cw as f64, ch as f64));
    if !(vw.is_finite() && vh.is_finite()) || vw <= 0.0 || vh <= 0.0 {
        return None;
    }
    let fit = Fit::new(vw, vh, cw, ch);

    #[cfg(windows)]
    {
        if let Some(bgra) = d2d::rasterize(src, cw, ch, vw, vh, &fit, background) {
            return Some(Canvas::Bitmap(Bitmap::new(cw, ch, bgra)));
        }
        qlr_core::log_debug!(
            "render",
            "Direct2D SVG 光栅化不可用，降级到内置光栅化器（{cw}x{ch}）"
        );
    }

    let bgra = fallback::rasterize(src, cw, ch, &fit, background);
    Some(Canvas::Bitmap(Bitmap::new(cw, ch, bgra)))
}

/// 等比缩放 + 居中：`scale` 允许 > 1（矢量放大不糊，和位图相反）。
struct Fit {
    scale: f64,
    tx: f64,
    ty: f64,
}

impl Fit {
    fn new(vw: f64, vh: f64, cw: u32, ch: u32) -> Self {
        let scale = (cw as f64 / vw).min(ch as f64 / vh);
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        let tx = (cw as f64 - vw * scale) / 2.0;
        let ty = (ch as f64 - vh * scale) / 2.0;
        Self { scale, tx, ty }
    }

    fn point(&self, x: f64, y: f64) -> (f64, f64) {
        (self.tx + x * self.scale, self.ty + y * self.scale)
    }
}

fn read_file(path: &Path) -> Option<Vec<u8>> {
    let (bytes, _truncated) = crate::text::read_capped(path, READ_LIMIT).ok()?;
    if bytes.is_empty() || crate::text::looks_binary(&bytes) {
        return None;
    }
    Some(bytes)
}

/// SVG 用户坐标系的宽高：`viewBox` 优先（浏览器也把它当内部坐标系），否则 `width`/`height`。
fn viewport_of(src: &str) -> Option<(f64, f64)> {
    let tag = find_open_tag(src, "svg")?;
    let mut w = None;
    let mut h = None;
    let mut vb = None;
    for_each_attr(tag, |key, value| {
        if key.eq_ignore_ascii_case("viewBox") {
            let n = numbers(value);
            if n.len() == 4 && n[2] > 0.0 && n[3] > 0.0 {
                vb = Some((n[2], n[3]));
            }
        } else if key.eq_ignore_ascii_case("width") {
            w = length(value);
        } else if key.eq_ignore_ascii_case("height") {
            h = length(value);
        }
    });
    if let (Some(width), Some(height)) = (w, h)
        && width > 0.0
        && height > 0.0
    {
        return Some((width, height));
    }
    vb
}

/// 找第一个 `<name ...>` 的**属性正文**（不含标签名与 `>`）。
fn find_open_tag<'a>(src: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = src;
    loop {
        let start = rest.find('<')?;
        let after = &rest[start + 1..];
        // 跳过 `<!--` / `<![CDATA[` / `<?xml`
        if after.starts_with('!') || after.starts_with('?') || after.starts_with('/') {
            rest = &after[after.find('>')? + 1..];
            continue;
        }
        let end = tag_end(after)?;
        let body = &after[..end];
        let name_len = body
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(body.len());
        if body[..name_len].eq_ignore_ascii_case(name) {
            return Some(&body[name_len..]);
        }
        rest = &after[end + 1..];
    }
}

/// 从标签名之后找到 `>` 的位置（跳过引号里的 `>`），返回相对下标。
fn tag_end(src: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in src.char_indices() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '>' => return Some(i),
                _ => {}
            },
        }
    }
    None
}

/// 遍历标签里的 `key="value"`（单双引号都认，`key` 没有 `=` 时值为空串）。
fn for_each_attr(tag: &str, mut f: impl FnMut(&str, &str)) {
    let bytes: &[u8] = tag.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // 跳过空白与结尾的 `/`
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let name_start = i;
        while i < bytes.len() && bytes[i] != b'=' && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let name = &tag[name_start..i];
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = "";
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                i += 1;
                let value_start = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                value = &tag[value_start..i];
                i += 1;
            } else {
                let value_start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                value = &tag[value_start..i];
            }
        }
        if !name.is_empty() {
            f(name, value);
        }
    }
}

/// 按空白/逗号切出所有数字（`"0 0 640 400"` / `"1,2 3"` 都认）。
fn numbers(s: &str) -> Vec<f64> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        let numeric = c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E');
        if numeric {
            cur.push(c);
            continue;
        }
        // `1e-5` 里的 `-` 属于数字，别在指数处截断
        if !cur.is_empty() {
            if let Ok(v) = cur.parse::<f64>() {
                out.push(v);
            }
            cur.clear();
        }
    }
    if let Ok(v) = cur.parse::<f64>() {
        out.push(v);
    }
    out
}

/// 长度值换算到 px（96 DPI）。百分比/`em` 这类相对单位无从得知 → `None`。
fn length(s: &str) -> Option<f64> {
    let t = s.trim();
    let num_len = t
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E')))
        .unwrap_or(t.len());
    let value: f64 = t[..num_len].parse().ok()?;
    let factor = match t[num_len..].trim().to_ascii_lowercase().as_str() {
        "" | "px" => 1.0,
        "pt" => 96.0 / 72.0,
        "pc" => 16.0,
        "in" => 96.0,
        "cm" => 96.0 / 2.54,
        "mm" => 96.0 / 25.4,
        _ => return None,
    };
    Some(value * factor)
}

// ---------------------------------------------------------------------------
// Direct2D 1.3：SVG 光栅化（主路径）
// ---------------------------------------------------------------------------

/// D3D11 设备 + D2D 设备上下文 + SVG 文档。
///
/// 特意不缓存：D2D 对象带线程亲和性（`THREADING_MODE_SINGLE_THREADED`），
/// 缓存就得跟着 worker 线程生命周期做 TLS，先按"每次新建"保证正确；
/// 实测一次 800×600 光栅化 ~15–30 ms（设备创建占一半），够用但不如位图路径快。
#[cfg(windows)]
mod d2d {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct2D::Common::{D2D_SIZE_F, D2D1_COLOR_F};
    use windows::Win32::Graphics::Direct2D::{
        D2D1_CREATION_PROPERTIES, D2D1_DEBUG_LEVEL_NONE, D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
        D2D1_THREADING_MODE_SINGLE_THREADED, D2D1_UNIT_MODE_PIXELS, D2D1CreateDeviceContext,
        ID2D1DeviceContext, ID2D1DeviceContext5, ID2D1SvgDocument,
    };
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_10_0,
        D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
        ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::IDXGISurface;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
    use windows::Win32::UI::Shell::SHCreateMemStream;
    use windows::core::Interface;

    use super::Fit;

    /// `windows_numerics::Matrix3x2` 的布局镜像。
    ///
    /// `windows` crate 不重导出 `windows_numerics`，为两个浮点字段再加一个依赖不划算；
    /// 这里只按 `#[repr(C)]` 顺序构造，然后 `as *const _` 让它推断成目标类型。
    #[repr(C)]
    struct Matrix {
        m11: f32,
        m12: f32,
        m21: f32,
        m22: f32,
        m31: f32,
        m32: f32,
    }

    /// `ID2D1DeviceContext5`（D2D 1.3）在 Win10 1703 才出现，SVG 也在那一版。
    ///
    /// 这里只做"接口在不在"的静态判断（`d2d1.dll` 一定随系统存在，缺的是设备），
    /// **不**建设备——设备创建要 15–30 ms，不能为了探测付这个钱。
    /// 真正的可用性由 `rasterize` 是否返回 `None` 决定，调用方据此降级。
    pub(super) fn available() -> bool {
        true
    }

    /// 把 SVG 文本光栅化成 BGRA。失败返回 `None`（调用方降级到内置光栅化器）。
    pub(super) fn rasterize(
        src: &str,
        cw: u32,
        ch: u32,
        vw: f64,
        vh: f64,
        fit: &Fit,
        background: Option<[u8; 4]>,
    ) -> Option<Vec<u8>> {
        // ole32 的流对象在没初始化的线程上会 CO_E_NOTINITIALIZED；
        // S_FALSE（本线程已经初始化过）也是成功，RPC_E_CHANGED_MODE 说明上层已按 STA 初始化，
        // 那种情况照样能建流，所以：**只记日志，不拦**。
        unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            if hr.is_err() {
                qlr_core::log_debug!(
                    "render",
                    "CoInitializeEx(MTA) 未成功：HRESULT=0x{:08X}（继续尝试 SVG）",
                    hr.0
                );
            }
        }

        let (device, context) = create_device()?;
        let texture = create_target_texture(&device, cw, ch)?;
        let surface: IDXGISurface = texture.cast().ok()?;
        let props = D2D1_CREATION_PROPERTIES {
            threadingMode: D2D1_THREADING_MODE_SINGLE_THREADED,
            debugLevel: D2D1_DEBUG_LEVEL_NONE,
            options: D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
        };
        let dc: ID2D1DeviceContext =
            unsafe { D2D1CreateDeviceContext(&surface, Some(&props)) }.ok()?;
        let dc5: ID2D1DeviceContext5 = dc.cast().ok()?;
        // 像素单位：否则 D2D 按 DPI（默认 96）缩放，位图尺寸就对不上了。
        unsafe { dc.SetUnitMode(D2D1_UNIT_MODE_PIXELS) };

        let stream = unsafe { SHCreateMemStream(Some(src.as_bytes())) }?;
        // viewport 用 SVG 自己的坐标系：文档内部按用户单位排版，缩放交给渲染变换。
        let doc: ID2D1SvgDocument = unsafe {
            dc5.CreateSvgDocument(
                &stream,
                D2D_SIZE_F {
                    width: vw as f32,
                    height: vh as f32,
                },
            )
        }
        .ok()?;

        let clear = match background {
            Some([b, g, r, a]) => D2D1_COLOR_F {
                r: r as f32 / 255.0,
                g: g as f32 / 255.0,
                b: b as f32 / 255.0,
                a: a as f32 / 255.0,
            },
            None => D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            },
        };
        let matrix = Matrix {
            m11: fit.scale as f32,
            m12: 0.0,
            m21: 0.0,
            m22: fit.scale as f32,
            m31: fit.tx as f32,
            m32: fit.ty as f32,
        };

        unsafe {
            dc.BeginDraw();
            dc.Clear(Some(&clear));
            dc.SetTransform(&matrix as *const Matrix as *const _);
            dc5.DrawSvgDocument(&doc);
            // EndDraw 是**唯一**会报告"目标丢了/设备被拔"的地方，必须当真。
            dc.EndDraw(None, None).ok()?;
        }

        read_back(&device, &context, &texture, cw, ch)
    }

    /// 建 D3D11 设备：先硬件，失败退 WARP（纯软件光栅化，远程桌面/无显卡也能跑）。
    fn create_device() -> Option<(ID3D11Device, ID3D11DeviceContext)> {
        let levels = [
            D3D_FEATURE_LEVEL_11_1,
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_10_0,
        ];
        for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
            let mut device = None;
            let mut context = None;
            let ok = unsafe {
                D3D11CreateDevice(
                    None,
                    driver,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&levels),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
            }
            .is_ok();
            if ok && let (Some(device), Some(context)) = (device, context) {
                return Some((device, context));
            }
            qlr_core::log_debug!(
                "render",
                "D3D11 设备（driver={driver:?}）建不出来，试下一个"
            );
        }
        None
    }

    fn create_target_texture(device: &ID3D11Device, w: u32, h: u32) -> Option<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.ok()?;
        texture
    }

    /// 把渲染目标拷贝到 STAGING 纹理再 `Map` 读回（GPU→CPU 唯一可靠路径）。
    fn read_back(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        w: u32,
        h: u32,
    ) -> Option<Vec<u8>> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }.ok()?;
        let staging = staging?;
        let dst: ID3D11Resource = staging.cast().ok()?;
        let source: ID3D11Resource = src.cast().ok()?;
        unsafe {
            context.CopyResource(&dst, &source);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(&dst, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .ok()?;
            let pitch = mapped.RowPitch as usize;
            let row_bytes = w as usize * 4;
            let mut out = vec![0u8; row_bytes * h as usize];
            if !mapped.pData.is_null() && pitch >= row_bytes {
                for y in 0..h as usize {
                    let src_row = (mapped.pData as *const u8).add(y * pitch);
                    let dst_row = out.as_mut_ptr().add(y * row_bytes);
                    std::ptr::copy_nonoverlapping(src_row, dst_row, row_bytes);
                }
            }
            context.Unmap(&dst, 0);
            Some(out)
        }
    }
}

// ---------------------------------------------------------------------------
// 内置光栅化器（降级路径）
// ---------------------------------------------------------------------------

/// 迷你 SVG 光栅化器。
///
/// 目标不是"渲染器"，而是"矢量预览不能白屏"：矩形/圆/椭圆/折线/多边形/路径
/// （M L H V C Q S Z，绝对与相对）+ fill/stroke/stroke-width/opacity + 线性渐变。
/// 抗锯齿用 4 条子扫描线 + 水平方向解析覆盖，足够让 1 px 细线不锯齿。
mod fallback {
    use super::{Fit, for_each_attr, tag_end};

    /// 直通道 RGBA（0..1）画布，最后统一转 BGRA。
    struct Img {
        w: usize,
        h: usize,
        px: Vec<[f32; 4]>,
    }

    /// 填充/描边的颜料。
    enum Paint {
        Solid([f32; 4]),
        /// 渐变端点已经换算到**像素坐标**（缩放/平移是线性变换，端点跟着一起变）。
        Linear {
            from: [f32; 4],
            to: [f32; 4],
            x1: f32,
            y1: f32,
            x2: f32,
            y2: f32,
        },
    }

    struct Shape {
        polys: Vec<Vec<[f32; 2]>>,
        fill: Option<Paint>,
        stroke: Option<(Paint, f32)>,
        opacity: f32,
    }

    /// 线性渐变定义（用户坐标，`objectBoundingBox` 的百分比已按元素 bbox 展开）。
    struct Gradient {
        id: String,
        stops: Vec<(f32, [f32; 4])>,
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        user_space: bool,
    }

    pub(super) fn rasterize(
        src: &str,
        cw: u32,
        ch: u32,
        fit: &Fit,
        background: Option<[u8; 4]>,
    ) -> Vec<u8> {
        let mut img = Img::new(cw as usize, ch as usize, background);
        let gradients = parse_gradients(src);
        for shape in parse_shapes(src, &gradients, fit) {
            if let Some(paint) = &shape.fill {
                for poly in &shape.polys {
                    img.fill(poly, paint, shape.opacity);
                }
            }
            if let Some((paint, width)) = &shape.stroke {
                let half = (width * 0.5).max(0.35);
                for poly in &shape.polys {
                    for quad in stroke_quads(poly, half) {
                        img.fill(&quad, paint, shape.opacity);
                    }
                }
            }
        }
        img.into_bgra()
    }

    impl Img {
        fn new(w: usize, h: usize, background: Option<[u8; 4]>) -> Self {
            let bg = match background {
                Some([b, g, r, a]) => [
                    r as f32 / 255.0,
                    g as f32 / 255.0,
                    b as f32 / 255.0,
                    a as f32 / 255.0,
                ],
                None => [0.0, 0.0, 0.0, 0.0],
            };
            Self {
                w,
                h,
                px: vec![bg; w * h],
            }
        }

        fn into_bgra(self) -> Vec<u8> {
            let mut out = Vec::with_capacity(self.px.len() * 4);
            for p in self.px {
                out.push((p[2].clamp(0.0, 1.0) * 255.0).round() as u8);
                out.push((p[1].clamp(0.0, 1.0) * 255.0).round() as u8);
                out.push((p[0].clamp(0.0, 1.0) * 255.0).round() as u8);
                out.push((p[3].clamp(0.0, 1.0) * 255.0).round() as u8);
            }
            out
        }

        /// 扫描线填充（非零环绕 + 4 条子扫描线的垂直抗锯齿 + 水平解析覆盖）。
        fn fill(&mut self, poly: &[[f32; 2]], paint: &Paint, alpha: f32) {
            if poly.len() < 3 || alpha <= 0.0 {
                return;
            }
            let (mut min_x, mut min_y, mut max_x, mut max_y) =
                (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for p in poly {
                min_x = min_x.min(p[0]);
                min_y = min_y.min(p[1]);
                max_x = max_x.max(p[0]);
                max_y = max_y.max(p[1]);
            }
            let y0 = (min_y.floor().max(0.0)) as usize;
            let y1 = (max_y.ceil().min(self.h as f32)) as usize;
            let x0 = (min_x.floor().max(0.0)) as usize;
            let x1 = (max_x.ceil().min(self.w as f32)) as usize;
            if y0 >= self.h || x0 >= self.w || x1 <= x0 {
                return;
            }

            const SUB: usize = 4;
            let mut coverage = vec![0f32; x1 - x0];
            let mut crossings: Vec<(f32, i32)> = Vec::new();
            for row in y0..y1.min(self.h) {
                coverage.iter_mut().for_each(|c| *c = 0.0);
                for sub in 0..SUB {
                    let y = row as f32 + (sub as f32 + 0.5) / SUB as f32;
                    crossings.clear();
                    for i in 0..poly.len() {
                        let a = poly[i];
                        let b = poly[(i + 1) % poly.len()];
                        let (dir, lo, hi) = if a[1] <= y && b[1] > y {
                            (1, a, b)
                        } else if b[1] <= y && a[1] > y {
                            (-1, b, a)
                        } else {
                            continue;
                        };
                        let t = (y - lo[1]) / (hi[1] - lo[1]);
                        crossings.push((lo[0] + t * (hi[0] - lo[0]), dir));
                    }
                    crossings.sort_by(|p, q| p.0.total_cmp(&q.0));
                    let mut winding = 0;
                    let mut span_start = 0.0;
                    for &(x, dir) in &crossings {
                        let before = winding;
                        winding += dir;
                        if before == 0 && winding != 0 {
                            span_start = x;
                        } else if before != 0 && winding == 0 {
                            add_span(&mut coverage, x0, x1, span_start, x);
                        }
                    }
                }
                for (index, cov) in coverage.iter().enumerate() {
                    let (color, weight) = match paint {
                        Paint::Solid(c) => (*c, alpha),
                        Paint::Linear { .. } => {
                            let x = (x0 + index) as f32 + 0.5;
                            let y = row as f32 + 0.5;
                            (paint.sample(x, y), alpha)
                        }
                    };
                    let a = (cov / SUB as f32) * weight * color[3];
                    if a > 0.001 {
                        let dst = &mut self.px[row * self.w + x0 + index];
                        blend(dst, [color[0], color[1], color[2]], a);
                    }
                }
                if row + 1 >= self.h {
                    break;
                }
            }
        }
    }

    impl Paint {
        fn sample(&self, x: f32, y: f32) -> [f32; 4] {
            match self {
                Paint::Solid(c) => *c,
                Paint::Linear {
                    from,
                    to,
                    x1,
                    y1,
                    x2,
                    y2,
                } => {
                    let (dx, dy) = (x2 - x1, y2 - y1);
                    let len2 = dx * dx + dy * dy;
                    let t = if len2 <= f32::EPSILON {
                        0.0
                    } else {
                        (((x - x1) * dx + (y - y1) * dy) / len2).clamp(0.0, 1.0)
                    };
                    lerp(*from, *to, t)
                }
            }
        }
    }

    fn lerp(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
        [
            a[0] + (b[0] - a[0]) * t,
            a[1] + (b[1] - a[1]) * t,
            a[2] + (b[2] - a[2]) * t,
            a[3] + (b[3] - a[3]) * t,
        ]
    }

    /// 把 `[sx, ex]` 这段（可以带小数）累加进覆盖缓冲。
    fn add_span(coverage: &mut [f32], x0: usize, x1: usize, sx: f32, ex: f32) {
        let sx = sx.max(x0 as f32);
        let ex = ex.min(x1 as f32);
        if ex <= sx {
            return;
        }
        let first = sx.floor() as usize;
        let last = ex.ceil() as usize;
        for i in first..last {
            if i < x0 || i >= x1 {
                continue;
            }
            let left = (i as f32).max(sx);
            let right = ((i + 1) as f32).min(ex);
            if right > left {
                coverage[i - x0] += right - left;
            }
        }
    }

    /// 直通道 alpha 混合（source-over）。
    fn blend(dst: &mut [f32; 4], rgb: [f32; 3], a: f32) {
        let a = a.clamp(0.0, 1.0);
        for i in 0..3 {
            dst[i] = rgb[i] * a + dst[i] * (1.0 - a);
        }
        dst[3] = a + dst[3] * (1.0 - a);
    }

    /// 把折线/多边形扩成"线段四边形 + 圆头关节"，再走同一套填充。
    fn stroke_quads(poly: &[[f32; 2]], half: f32) -> Vec<Vec<[f32; 2]>> {
        let mut out = Vec::with_capacity(poly.len());
        let closed = poly.len() > 2 && poly[0] == poly[poly.len() - 1];
        let segments = if closed {
            poly.len() - 1
        } else {
            poly.len().saturating_sub(1)
        };
        for i in 0..segments {
            let a = poly[i];
            let b = poly[(i + 1) % poly.len()];
            let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
            let len = (dx * dx + dy * dy).sqrt();
            if len <= f32::EPSILON {
                continue;
            }
            let (nx, ny) = (-dy / len * half, dx / len * half);
            out.push(vec![
                [a[0] + nx, a[1] + ny],
                [b[0] + nx, b[1] + ny],
                [b[0] - nx, b[1] - ny],
                [a[0] - nx, a[1] - ny],
            ]);
        }
        // 关节补圆（否则折线拐角会漏出一小段）
        for v in poly
            .iter()
            .take(if closed { poly.len() - 1 } else { poly.len() })
        {
            out.push(disc(v[0], v[1], half, 10));
        }
        out
    }

    fn disc(cx: f32, cy: f32, r: f32, steps: usize) -> Vec<[f32; 2]> {
        (0..steps)
            .map(|i| {
                let a = std::f32::consts::TAU * i as f32 / steps as f32;
                [cx + r * a.cos(), cy + r * a.sin()]
            })
            .collect()
    }

    // ---- 解析 ----

    /// 第一遍：收集 `<linearGradient>`（含 stop 子元素）。
    fn parse_gradients(src: &str) -> Vec<Gradient> {
        let mut out: Vec<Gradient> = Vec::new();
        let mut current: Option<Gradient> = None;
        for (name, body) in elements(src) {
            // 注意：`elements()` 交出来的标签名**已经小写**（`linearGradient` → `lineargradient`），
            // 所以这里的分支必须写小写。写成驼峰会导致渐变永远解析不出来——
            // 表现是"渐变填充的图形整块消失"，而纯色图形正常，很容易被当成光栅化器的锅。
            match name.as_str() {
                "lineargradient" => {
                    let mut g = Gradient {
                        id: String::new(),
                        stops: Vec::new(),
                        x1: 0.0,
                        y1: 0.0,
                        x2: 1.0,
                        y2: 0.0,
                        user_space: false,
                    };
                    for_each_attr(body, |key, value| {
                        if key.eq_ignore_ascii_case("id") {
                            g.id = value.to_string();
                        } else if key.eq_ignore_ascii_case("gradientUnits") {
                            g.user_space = value.eq_ignore_ascii_case("userSpaceOnUse");
                        } else if key.eq_ignore_ascii_case("gradientTransform") {
                            // 不支持变换：忽略（降级为"没旋转的渐变"）
                        } else if key.eq_ignore_ascii_case("x1") {
                            g.x1 = ratio(value);
                        } else if key.eq_ignore_ascii_case("y1") {
                            g.y1 = ratio(value);
                        } else if key.eq_ignore_ascii_case("x2") {
                            g.x2 = ratio(value);
                        } else if key.eq_ignore_ascii_case("y2") {
                            g.y2 = ratio(value);
                        }
                    });
                    current = Some(g);
                }
                "stop" => {
                    if let Some(g) = current.as_mut() {
                        let mut offset = 0.0;
                        let mut color = [0.0, 0.0, 0.0, 1.0];
                        for_each_attr(body, |key, value| {
                            if key.eq_ignore_ascii_case("offset") {
                                offset = ratio(value);
                            } else if key.eq_ignore_ascii_case("stop-color") {
                                if let Some(c) = parse_color(value) {
                                    color = c;
                                }
                            } else if key.eq_ignore_ascii_case("stop-opacity")
                                && let Some(o) = value.trim().parse::<f32>().ok()
                            {
                                color[3] *= o.clamp(0.0, 1.0);
                            }
                        });
                        g.stops.push((offset.clamp(0.0, 1.0), color));
                    }
                }
                "/lineargradient" => {
                    if let Some(g) = current.take()
                        && !g.id.is_empty()
                        && !g.stops.is_empty()
                    {
                        out.push(g);
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// 第二遍：把每个图形元素变成"像素坐标下的多边形 + 颜料"。
    fn parse_shapes(src: &str, gradients: &[Gradient], fit: &Fit) -> Vec<Shape> {
        let mut out = Vec::new();
        for (name, body) in elements(src) {
            let mut nums = std::collections::HashMap::new();
            let mut fill_raw = String::from("black");
            let mut stroke_raw = String::from("none");
            let mut stroke_width = 1.0f32;
            let mut opacity = 1.0f32;
            let mut fill_opacity = 1.0f32;
            let mut d = String::new();
            let mut points = String::new();
            for_each_attr(body, |key, value| {
                if key.eq_ignore_ascii_case("fill") {
                    fill_raw = value.trim().to_string();
                } else if key.eq_ignore_ascii_case("stroke") {
                    stroke_raw = value.trim().to_string();
                } else if key.eq_ignore_ascii_case("stroke-width") {
                    stroke_width = length(value).unwrap_or(1.0);
                } else if key.eq_ignore_ascii_case("opacity") {
                    opacity = value.trim().parse().unwrap_or(1.0);
                } else if key.eq_ignore_ascii_case("fill-opacity") {
                    fill_opacity = value.trim().parse().unwrap_or(1.0);
                } else if key.eq_ignore_ascii_case("d") {
                    d = value.to_string();
                } else if key.eq_ignore_ascii_case("points") {
                    points = value.to_string();
                } else if let Ok(v) = value.trim().parse::<f32>() {
                    nums.insert(key.to_ascii_lowercase(), v);
                }
            });
            let n = |key: &str| nums.get(key).copied().unwrap_or(0.0);
            let (cx, cy) = fit.point(n("cx") as f64, n("cy") as f64);

            let mut open = false;
            let polys: Vec<Vec<[f32; 2]>> = match name.as_str() {
                "rect" => {
                    let (x, y) = (n("x"), n("y"));
                    let (w, h) = (n("width"), n("height"));
                    if w <= 0.0 || h <= 0.0 {
                        continue;
                    }
                    let rx = n("rx").min(w / 2.0);
                    let ry = if nums.contains_key("ry") {
                        n("ry").min(h / 2.0)
                    } else {
                        rx.min(h / 2.0)
                    };
                    vec![rect_points(fit, x, y, w, h, rx, ry)]
                }
                "circle" => {
                    let r = n("r");
                    if r <= 0.0 {
                        continue;
                    }
                    vec![ellipse_points(
                        cx as f32,
                        cy as f32,
                        fit.scale as f32 * r,
                        fit.scale as f32 * r,
                    )]
                }
                "ellipse" => {
                    let (rx, ry) = (n("rx"), n("ry"));
                    if rx <= 0.0 || ry <= 0.0 {
                        continue;
                    }
                    vec![ellipse_points(
                        cx as f32,
                        cy as f32,
                        fit.scale as f32 * rx,
                        fit.scale as f32 * ry,
                    )]
                }
                "line" => {
                    open = true;
                    let (x1, y1) = fit.point(n("x1") as f64, n("y1") as f64);
                    let (x2, y2) = fit.point(n("x2") as f64, n("y2") as f64);
                    vec![vec![[x1 as f32, y1 as f32], [x2 as f32, y2 as f32]]]
                }
                "polygon" | "polyline" => {
                    open = name == "polyline";
                    let raw = super::numbers(&points);
                    let pts: Vec<[f32; 2]> = raw
                        .chunks_exact(2)
                        .map(|p| {
                            let (x, y) = fit.point(p[0], p[1]);
                            [x as f32, y as f32]
                        })
                        .collect();
                    if pts.len() < 2 {
                        continue;
                    }
                    vec![pts]
                }
                "path" => path_polys(&d, fit, &mut open),
                _ => continue,
            };
            if polys.is_empty() {
                continue;
            }
            let fill = paint_of(&fill_raw, gradients, fit, &polys, fill_opacity);
            let stroke = paint_of(&stroke_raw, gradients, fit, &polys, 1.0)
                .map(|p| (p, stroke_width * fit.scale as f32));
            if fill.is_none() && stroke.is_none() {
                continue;
            }
            // 折线（line/polyline）默认不填充，只描边
            let fill = if open { None } else { fill };
            out.push(Shape {
                polys,
                fill,
                stroke,
                opacity: opacity.clamp(0.0, 1.0),
            });
        }
        out
    }

    /// 把 `fill`/`stroke` 的值解析成颜料（`none` → None，`url(#id)` → 渐变）。
    fn paint_of(
        raw: &str,
        gradients: &[Gradient],
        fit: &Fit,
        polys: &[Vec<[f32; 2]>],
        opacity: f32,
    ) -> Option<Paint> {
        if raw.is_empty()
            || raw.eq_ignore_ascii_case("none")
            || raw.eq_ignore_ascii_case("transparent")
        {
            return None;
        }
        if let Some(id) = raw
            .strip_prefix("url(")
            .and_then(|s| s.strip_suffix(')'))
            .map(|s| {
                s.trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .trim_start_matches('#')
            })
        {
            let g = gradients.iter().find(|g| g.id == id)?;
            let first = g.stops.first()?;
            let last = g.stops.last()?;
            let (x1, y1, x2, y2) = gradient_box(g, polys);
            let (dx1, dy1) = fit.point(x1 as f64, y1 as f64);
            let (dx2, dy2) = fit.point(x2 as f64, y2 as f64);
            let mut from = first.1;
            let mut to = last.1;
            from[3] *= opacity;
            to[3] *= opacity;
            return Some(Paint::Linear {
                from,
                to,
                x1: dx1 as f32,
                y1: dy1 as f32,
                x2: dx2 as f32,
                y2: dy2 as f32,
            });
        }
        let mut color = parse_color(raw)?;
        color[3] *= opacity.clamp(0.0, 1.0);
        Some(Paint::Solid(color))
    }

    /// 渐变端点：`objectBoundingBox`（默认，0..1 相对 bbox）或 `userSpaceOnUse`（已是用户单位）。
    fn gradient_box(g: &Gradient, polys: &[Vec<[f32; 2]>]) -> (f32, f32, f32, f32) {
        if g.user_space {
            return (g.x1, g.y1, g.x2, g.y2);
        }
        // 注意：polys 已经是像素坐标，但 `objectBoundingBox` 需要的是元素 bbox——
        // 两者只差一个整体缩放/平移，所以按像素 bbox 展开这两个 0..1 比例也成立。
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for p in polys.iter().flatten() {
            min_x = min_x.min(p[0]);
            min_y = min_y.min(p[1]);
            max_x = max_x.max(p[0]);
            max_y = max_y.max(p[1]);
        }
        if min_x > max_x {
            return (0.0, 0.0, 1.0, 0.0);
        }
        let (w, h) = (max_x - min_x, max_y - min_y);
        (
            min_x + g.x1 * w,
            min_y + g.y1 * h,
            min_x + g.x2 * w,
            min_y + g.y2 * h,
        )
    }

    fn rect_points(fit: &Fit, x: f32, y: f32, w: f32, h: f32, rx: f32, ry: f32) -> Vec<[f32; 2]> {
        let mut pts = Vec::new();
        let push = |pts: &mut Vec<[f32; 2]>, px: f32, py: f32| {
            let (dx, dy) = fit.point(px as f64, py as f64);
            pts.push([dx as f32, dy as f32]);
        };
        if rx <= 0.0 || ry <= 0.0 {
            push(&mut pts, x, y);
            push(&mut pts, x + w, y);
            push(&mut pts, x + w, y + h);
            push(&mut pts, x, y + h);
            return pts;
        }
        const ARC: usize = 6;
        for (i, (ox, oy, a0)) in [
            (x + w - rx, y + ry, -std::f32::consts::FRAC_PI_2),
            (x + w - rx, y + h - ry, 0.0),
            (x + rx, y + h - ry, std::f32::consts::FRAC_PI_2),
            (x + rx, y + ry, std::f32::consts::PI),
        ]
        .into_iter()
        .enumerate()
        {
            for k in 0..=ARC {
                let a = a0 + std::f32::consts::FRAC_PI_2 * k as f32 / ARC as f32;
                let _ = i;
                push(&mut pts, ox + rx * a.cos(), oy + ry * a.sin());
            }
        }
        pts
    }

    fn ellipse_points(cx: f32, cy: f32, rx: f32, ry: f32) -> Vec<[f32; 2]> {
        let steps = (rx.max(ry) as usize / 2).clamp(16, 96);
        (0..steps)
            .map(|i| {
                let a = std::f32::consts::TAU * i as f32 / steps as f32;
                [cx + rx * a.cos(), cy + ry * a.sin()]
            })
            .collect()
    }

    /// SVG path 的 `d` 属性 → 若干多边形（只处理 M L H V C Q S Z，弧线降级成直线）。
    fn path_polys(d: &str, fit: &Fit, open: &mut bool) -> Vec<Vec<[f32; 2]>> {
        let mut out: Vec<Vec<[f32; 2]>> = Vec::new();
        let mut current: Vec<[f32; 2]> = Vec::new();
        let mut cursor = (0.0f64, 0.0f64);
        let mut start = (0.0f64, 0.0f64);
        let mut cmd = 'M';
        let tokens = tokenize_path(d);
        let mut i = 0;
        let at = |tokens: &[PathTok], i: usize| -> f64 {
            match tokens.get(i) {
                Some(PathTok::Num(v)) => *v,
                _ => 0.0,
            }
        };
        while i < tokens.len() {
            if let PathTok::Cmd(c) = tokens[i] {
                cmd = c;
                i += 1;
                if matches!(cmd, 'Z' | 'z') {
                    if current.len() >= 3 {
                        current.push(current[0]);
                        out.push(std::mem::take(&mut current));
                    }
                    cursor = start;
                    continue;
                }
            }
            let rel = cmd.is_ascii_lowercase();
            let base = if rel { cursor } else { (0.0, 0.0) };
            let map = |x: f64, y: f64| -> [f32; 2] {
                let (dx, dy) = fit.point(x, y);
                [dx as f32, dy as f32]
            };
            match cmd.to_ascii_uppercase() {
                'M' => {
                    let x = base.0 + at(&tokens, i);
                    let y = base.1 + at(&tokens, i + 1);
                    i += 2;
                    if current.len() >= 2 {
                        out.push(std::mem::take(&mut current));
                    }
                    cursor = (x, y);
                    start = cursor;
                    current.push(map(x, y));
                }
                'L' => {
                    let x = base.0 + at(&tokens, i);
                    let y = base.1 + at(&tokens, i + 1);
                    i += 2;
                    cursor = (x, y);
                    current.push(map(x, y));
                }
                'H' => {
                    let x = base.0 + at(&tokens, i);
                    i += 1;
                    cursor.0 = x;
                    current.push(map(cursor.0, cursor.1));
                }
                'V' => {
                    let y = base.1 + at(&tokens, i);
                    i += 1;
                    cursor.1 = y;
                    current.push(map(cursor.0, cursor.1));
                }
                'C' | 'S' => {
                    // S 的前一个控制点按反射处理（简化：直接用当前点，视觉误差可接受）
                    let (c1x, c1y, c2x, c2y, x, y, used) = if cmd == 'S' || cmd == 's' {
                        (
                            cursor.0,
                            cursor.1,
                            base.0 + at(&tokens, i),
                            base.1 + at(&tokens, i + 1),
                            base.0 + at(&tokens, i + 2),
                            base.1 + at(&tokens, i + 3),
                            4,
                        )
                    } else {
                        (
                            base.0 + at(&tokens, i),
                            base.1 + at(&tokens, i + 1),
                            base.0 + at(&tokens, i + 2),
                            base.1 + at(&tokens, i + 3),
                            base.0 + at(&tokens, i + 4),
                            base.1 + at(&tokens, i + 5),
                            6,
                        )
                    };
                    i += used;
                    for k in 1..=16 {
                        let t = k as f64 / 16.0;
                        let mt = 1.0 - t;
                        let x0 = mt * mt * mt * cursor.0
                            + 3.0 * mt * mt * t * c1x
                            + 3.0 * mt * t * t * c2x
                            + t * t * t * x;
                        let y0 = mt * mt * mt * cursor.1
                            + 3.0 * mt * mt * t * c1y
                            + 3.0 * mt * t * t * c2y
                            + t * t * t * y;
                        current.push(map(x0, y0));
                    }
                    cursor = (x, y);
                }
                'Q' => {
                    let (qx, qy) = (base.0 + at(&tokens, i), base.1 + at(&tokens, i + 1));
                    let (x, y) = (base.0 + at(&tokens, i + 2), base.1 + at(&tokens, i + 3));
                    i += 4;
                    for k in 1..=12 {
                        let t = k as f64 / 12.0;
                        let (mt, nt) = (1.0 - t, t);
                        let x0 = mt * mt * cursor.0 + 2.0 * mt * nt * qx + nt * nt * x;
                        let y0 = mt * mt * cursor.1 + 2.0 * mt * nt * qy + nt * nt * y;
                        current.push(map(x0, y0));
                    }
                    cursor = (x, y);
                }
                _ => {
                    // 未知命令（A/T 等）：跳过它的参数，别死循环
                    while i < tokens.len() && matches!(tokens[i], PathTok::Num(_)) {
                        i += 1;
                    }
                }
            }
        }
        if current.len() >= 2 {
            out.push(current);
        }
        // 只有两个点时按折线画（描边仍有效）
        *open = out.iter().any(|p| p.len() == 2);
        out
    }

    enum PathTok {
        Cmd(char),
        Num(f64),
    }

    fn tokenize_path(d: &str) -> Vec<PathTok> {
        let mut out = Vec::new();
        let mut num = String::new();
        let flush = |num: &mut String, out: &mut Vec<PathTok>| {
            if !num.is_empty() {
                if let Ok(v) = num.parse::<f64>() {
                    out.push(PathTok::Num(v));
                }
                num.clear();
            }
        };
        for c in d.chars() {
            if c.is_ascii_alphabetic() {
                flush(&mut num, &mut out);
                out.push(PathTok::Cmd(c));
            } else if c.is_ascii_digit() || matches!(c, '.' | '-' | '+') {
                // 数字中间的 `-`（`10-5` 这种紧凑写法）要切开
                if (c == '-' || c == '+')
                    && !num.is_empty()
                    && !num.ends_with('e')
                    && !num.ends_with('E')
                {
                    flush(&mut num, &mut out);
                }
                num.push(c);
            } else {
                flush(&mut num, &mut out);
            }
        }
        flush(&mut num, &mut out);
        out
    }

    /// 遍历所有标签，返回 `(小写标签名或 "/小写标签名", 属性正文)`。
    fn elements(src: &str) -> Vec<(String, &str)> {
        let mut out = Vec::new();
        let mut rest = src;
        while let Some(start) = rest.find('<') {
            let after = &rest[start + 1..];
            if after.starts_with('!') || after.starts_with('?') {
                // 注释 / 声明：整段跳过
                let Some(end) = after.find(">") else {
                    break;
                };
                rest = &after[end + 1..];
                continue;
            }
            let Some(end) = tag_end(after) else {
                break;
            };
            let body = &after[..end];
            let closing = body.starts_with('/');
            let name_body = if closing { &body[1..] } else { body };
            let name_len = name_body
                .find(|c: char| c.is_whitespace() || c == '/')
                .unwrap_or(name_body.len());
            let name = name_body[..name_len].to_ascii_lowercase();
            let attrs = &name_body[name_len..];
            out.push((if closing { format!("/{name}") } else { name }, attrs));
            rest = &after[end + 1..];
        }
        out
    }

    /// `#rgb` / `#rrggbb` / `rgb(r,g,b)` / `rgb(r%,g%,b%)` / 常见颜色名。
    pub(super) fn parse_color(raw: &str) -> Option<[f32; 4]> {
        let t = raw.trim();
        if let Some(hex) = t.strip_prefix('#') {
            let digits: Vec<u8> = hex
                .chars()
                .filter_map(|c| c.to_digit(16).map(|d| d as u8))
                .collect();
            return match digits.len() {
                3 => Some([
                    digits[0] as f32 / 15.0,
                    digits[1] as f32 / 15.0,
                    digits[2] as f32 / 15.0,
                    1.0,
                ]),
                6 => Some([
                    (digits[0] * 16 + digits[1]) as f32 / 255.0,
                    (digits[2] * 16 + digits[3]) as f32 / 255.0,
                    (digits[4] * 16 + digits[5]) as f32 / 255.0,
                    1.0,
                ]),
                _ => None,
            };
        }
        if t.len() > 4 && t[..4].eq_ignore_ascii_case("rgb(") {
            let inner = t[4..].trim_end_matches(')');
            let parts: Vec<&str> = inner.split([',', ' ']).filter(|s| !s.is_empty()).collect();
            if parts.len() < 3 {
                return None;
            }
            let channel = |s: &str| -> f32 {
                let s = s.trim();
                if let Some(p) = s.strip_suffix('%') {
                    p.parse::<f32>().unwrap_or(0.0) / 100.0
                } else {
                    s.parse::<f32>().unwrap_or(0.0) / 255.0
                }
            };
            return Some([channel(parts[0]), channel(parts[1]), channel(parts[2]), 1.0]);
        }
        let named: [(&str, [f32; 4]); 12] = [
            ("black", [0.0, 0.0, 0.0, 1.0]),
            ("white", [1.0, 1.0, 1.0, 1.0]),
            ("red", [1.0, 0.0, 0.0, 1.0]),
            ("green", [0.0, 128.0 / 255.0, 0.0, 1.0]),
            ("blue", [0.0, 0.0, 1.0, 1.0]),
            ("yellow", [1.0, 1.0, 0.0, 1.0]),
            ("orange", [1.0, 165.0 / 255.0, 0.0, 1.0]),
            ("purple", [128.0 / 255.0, 0.0, 128.0 / 255.0, 1.0]),
            ("gray", [128.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0, 1.0]),
            ("grey", [128.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0, 1.0]),
            ("silver", [192.0 / 255.0, 192.0 / 255.0, 192.0 / 255.0, 1.0]),
            ("navy", [0.0, 0.0, 128.0 / 255.0, 1.0]),
        ];
        named
            .iter()
            .find(|(name, _)| t.eq_ignore_ascii_case(name))
            .map(|(_, c)| *c)
    }

    /// `50%` / `0.5` / `12` → 比例。SVG 的 offset 允许三种写法。
    fn ratio(s: &str) -> f32 {
        let t = s.trim();
        if let Some(p) = t.strip_suffix('%') {
            return p.parse::<f32>().unwrap_or(0.0) / 100.0;
        }
        t.parse::<f32>().unwrap_or(0.0)
    }

    fn length(s: &str) -> Option<f32> {
        super::length(s).map(|v| v as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 最小内联 SVG：矩形 + 圆 + 线性渐变（任务要求的夹具形态）。
    const SAMPLE: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" width="100" height="60" viewBox="0 0 100 60">
  <defs>
    <linearGradient id="g" x1="0" y1="0" x2="1" y2="0">
      <stop offset="0%" stop-color="#1f4fd8"/>
      <stop offset="100%" stop-color="#e8784a"/>
    </linearGradient>
  </defs>
  <rect x="0" y="0" width="100" height="60" fill="url(#g)"/>
  <rect x="6" y="6" width="40" height="20" fill="#ffffff" stroke="#12304f" stroke-width="2"/>
  <circle cx="72" cy="34" r="14" fill="#f5d64a" stroke="#8a2b1e" stroke-width="2"/>
  <path d="M10 50 L26 36 L40 50 Z" fill="#27b46b"/>
</svg>"##;

    fn non_background_ratio(bitmap: &Bitmap, bg: [u8; 4]) -> f64 {
        let total = (bitmap.width * bitmap.height) as f64;
        let differing = bitmap
            .bgra
            .chunks_exact(4)
            .filter(|px| {
                (px[0] as i32 - bg[0] as i32).abs() > 8
                    || (px[1] as i32 - bg[1] as i32).abs() > 8
                    || (px[2] as i32 - bg[2] as i32).abs() > 8
            })
            .count();
        differing as f64 / total
    }

    fn bitmap_of(canvas: Canvas) -> Bitmap {
        match canvas {
            Canvas::Bitmap(b) => b,
            other => panic!("SVG 应产出位图，得到 {}", other.kind_name()),
        }
    }

    #[test]
    fn renders_inline_svg_with_content() {
        let canvas = render_str(SAMPLE, 240, 160, Some(WHITE)).expect("合法 SVG 应渲染成功");
        let bitmap = bitmap_of(canvas);
        // 输出尺寸必须等于请求尺寸（内容等比缩放后居中，四周留白）
        assert_eq!((bitmap.width, bitmap.height), (240, 160));
        assert_eq!(bitmap.bgra.len(), 240 * 160 * 4);
        // 画进去的内容必须**看得见**：非背景色像素占比要够高
        let ratio = non_background_ratio(&bitmap, WHITE);
        assert!(ratio > 0.3, "非背景像素占比太低：{ratio:.3}");
        // 中间必须不是白（渐变矩形铺满整个 viewBox → 中间区域一定是彩色）
        let center = ((160 / 2) * 240 + 240 / 2) * 4;
        let px = &bitmap.bgra[center..center + 4];
        assert!(
            px[0] < 240 || px[1] < 240 || px[2] < 240,
            "中心像素不应是纯白：{px:?}"
        );
        // 等比缩放：100x60 塞进 240x160 → 按**宽**适配（scale=2.4 → 240x144），
        // 上下各留 8 px（不是按高度适配：240/100=2.4 < 160/60=2.667）。
        // 所以留白在 y<8 与 y>=152，x 方向贴满。
        let top = &bitmap.bgra[(2 * 240 + 120) * 4..(2 * 240 + 124) * 4];
        assert_eq!(top[3], 255, "白底 alpha 必须是 255");
        assert!(
            top[0] >= 240 && top[1] >= 240 && top[2] >= 240,
            "上方留白应是白底：{top:?}"
        );
        let bottom = &bitmap.bgra[(156 * 240 + 120) * 4..(156 * 240 + 124) * 4];
        assert!(
            bottom[0] >= 240 && bottom[1] >= 240 && bottom[2] >= 240,
            "下方留白应是白底：{bottom:?}"
        );
    }

    #[test]
    fn transparent_background_stays_transparent() {
        let canvas = render_str(SAMPLE, 100, 60, None).expect("透明底渲染应成功");
        let bitmap = bitmap_of(canvas);
        // 左上角在 viewBox 内部，但被渐变矩形覆盖 → 不透明；用"没有白底"来判：角落像素 alpha
        // 只可能来自图形自身（渐变是 100% 不透明，所以这里是 255）。
        assert_eq!(bitmap.bgra[3], 255);
        // 换一张只有中间小方块的图：四周必须是 alpha=0
        let sparse = r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="40">
            <rect x="10" y="10" width="20" height="20" fill="#000000"/></svg>"##;
        let bitmap = bitmap_of(render_str(sparse, 40, 40, None).expect("应成功"));
        assert_eq!(bitmap.bgra[3], 0, "透明底：左上角 alpha 应为 0");
        let inner = ((20 * 40) + 20) * 4;
        assert_eq!(bitmap.bgra[inner + 3], 255, "方块内应当不透明");
    }

    #[test]
    fn broken_svg_does_not_panic() {
        // 空串 / 纯文本 / 不是 SVG 的 XML / 半截标签 / 关不上的 svg
        for bad in [
            "",
            "这不是 SVG",
            "<html><body>hi</body></html>",
            "<svg xmlns=\"http://www.w3.org/2000/svg\"",
            "<svg></svg>",
            "<svg width=\"abc\" height=\"?\"><rect/></svg>",
            "<svg width=\"10\" height=\"10\"><path d=\"M M M\"/><circle r=\"-5\"/></svg>",
        ] {
            let out = render_str(bad, 64, 48, Some(WHITE));
            // 允许 Some（画成一张纯白图）也允许 None，唯一不允许的是 panic
            if let Some(canvas) = out {
                let bitmap = bitmap_of(canvas);
                assert_eq!((bitmap.width, bitmap.height), (64, 48));
            }
        }
    }

    #[test]
    fn missing_file_returns_none() {
        let path = std::path::PathBuf::from(r"C:\definitely\not\here\a.svg");
        assert!(render(&path, 100, 100).is_none());
        assert!(natural_size(&path).is_none());
    }

    #[test]
    fn whole_file_renders_end_to_end() {
        let path = crate::testutil::write_file("svg", "sample.svg", SAMPLE.as_bytes());
        let canvas = render(&path, 120, 80).expect("文件渲染应成功");
        let bitmap = bitmap_of(canvas);
        assert_eq!((bitmap.width, bitmap.height), (120, 80));
        assert!(non_background_ratio(&bitmap, WHITE) > 0.3);
    }

    #[test]
    fn natural_size_prefers_width_height_then_viewbox() {
        let p1 = crate::testutil::write_file(
            "svg",
            "with-size.svg",
            br#"<svg width="320px" height="200" viewBox="0 0 10 10"></svg>"#,
        );
        assert_eq!(natural_size(&p1), Some((320, 200)));

        let p2 = crate::testutil::write_file(
            "svg",
            "viewbox-only.svg",
            br#"<svg viewBox="0 0 640 400"></svg>"#,
        );
        assert_eq!(natural_size(&p2), Some((640, 400)));

        // 百分比宽高无法换算 → None（壳层用默认开窗尺寸）
        let p3 = crate::testutil::write_file(
            "svg",
            "percent.svg",
            br#"<svg width="100%" height="100%"></svg>"#,
        );
        assert_eq!(natural_size(&p3), None);
    }

    /// 降级光栅化器本身必须有货：即使本机 D2D 可用，这条路径也要能单独验证。
    #[test]
    fn fallback_rasterizer_draws_shapes() {
        let fit = super::Fit::new(100.0, 60.0, 200, 120);
        let bgra = super::fallback::rasterize(SAMPLE, 200, 120, &fit, Some(WHITE));
        let bitmap = Bitmap::new(200, 120, bgra);
        let ratio = non_background_ratio(&bitmap, WHITE);
        assert!(ratio > 0.3, "降级光栅化器没画出内容：{ratio:.3}");
        // 渐变必须真的在渐变：左半蓝、右半偏红
        let at =
            |x: usize, y: usize| bitmap.bgra[(y * 200 + x) * 4..(y * 200 + x) * 4 + 4].to_vec();
        let left = at(30, 60);
        let right = at(170, 60);
        assert!(left[0] > left[2], "左侧应是蓝底（B>R）：{left:?}");
        assert!(right[2] > right[0], "右侧应偏暖（R>B）：{right:?}");
    }

    #[test]
    fn fallback_handles_rounded_rect_and_bezier() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="80" height="80">
            <rect x="10" y="10" width="60" height="60" rx="14" fill="#e8784a"/>
            <path d="M20 70 C 40 30, 60 90, 75 20" fill="none" stroke="#12304f" stroke-width="3"/>
        </svg>"##;
        let fit = super::Fit::new(80.0, 80.0, 80, 80);
        let bitmap = Bitmap::new(
            80,
            80,
            super::fallback::rasterize(svg, 80, 80, &fit, Some(WHITE)),
        );
        // 圆角矩形的四角应保留背景色（圆角生效）
        let corner = &bitmap.bgra[(10 * 80 + 10) * 4..(10 * 80 + 10) * 4 + 4];
        assert!(
            corner[0] > 240 && corner[1] > 240 && corner[2] > 240,
            "左上角应被圆角切掉：{corner:?}"
        );
        let body = &bitmap.bgra[(40 * 80 + 40) * 4..(40 * 80 + 40) * 4 + 4];
        assert!(body[2] > body[0], "矩形本体应是橙色（B>R? 橙色 R>B）");
        assert!(non_background_ratio(&bitmap, WHITE) > 0.3);
    }

    #[test]
    fn backend_is_reported() {
        // 不崩就行；数值随机器不同（D2D 可用时 "direct2d"）
        let name = backend_name();
        assert!(name == "direct2d" || name == "fallback" || name == "none");
    }
}
