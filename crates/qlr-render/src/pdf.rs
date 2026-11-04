//! PDF 渲染：手写 pdfium 动态绑定。
//!
//! 为什么不用 `pdfium-render` crate：它为了"一个 API 兼顾桌面/浏览器/异步"
//! 拉进了 `tokio` + `wasm-bindgen` + `web-sys`，编译进 exe 要多几百 KB 到 MB 级，
//! 与 A8.5「装载裁剪、exe ≤5 MB」直接冲突；而我们要用的只有 12 个 C 函数。
//! 这里用 `libloading` 手工绑定，pdfium.dll 缺失时退化成"可读的错误提示"，
//! 主程序照常启动（不做硬依赖）。
//!
//! 关键决策：
//! - **用 `FPDF_LoadMemDocument64` 而不是 `FPDF_LoadDocument`**：后者要 ANSI 路径，
//!   中文用户名/中文路径必然乱码；前者由我们自己读文件字节，路径随便。
//! - **`FPDFBitmap_CreateEx` + 自己的缓冲区**：避免再拷一次 BGRA（pdfium 直接写进
//!   我们的 `Vec<u8>`，stride = width*4）。
//! - pdfium 的库初始化和调用都串行化（一个 `Mutex`）：pdfium 自身不是线程安全的，
//!   我们只有一个渲染线程，但别给以后留坑。
//! - dll 只在第一次真正渲染 PDF 时才加载（启动路径不碰它）。

use std::ffi::{c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use libloading::Library;

use crate::budget;
use crate::canvas::{Bitmap, Canvas};
use qlr_core::error::{Error, Result};

/// pdfium 的库文件名（与 exe 同目录）。
pub const LIBRARY_NAME: &str = "pdfium.dll";
/// 可用环境变量覆盖 dll 路径（排障用）。
pub const ENV_LIBRARY: &str = "QLRUST_PDFIUM";
/// 最多渲染的页数上限（防御超长文档，先只渲染请求的那一页）。
pub const MAX_PAGES: usize = 10_000;
/// 单页渲染像素上限（防异常页尺寸 / 超大画布把内存吃穿）。
pub const MAX_PAGE_PIXELS: u64 = 50_000_000;
/// 单边像素上限。
pub const MAX_PAGE_EDGE: u32 = 20_000;

// pdfium C API 常量（见 fpdfview.h）
const FPDF_ERR_SUCCESS: u32 = 0;
const FPDF_BITMAP_BGRA: c_int = 4;
// FPDF_ANNOT：把注释/表单外观也画出来，否则签名、批注会消失。
const RENDER_FLAGS: c_int = 0x01;

// ---------------------------------------------------------------------------
// 动态符号表
// ---------------------------------------------------------------------------

type FnInitLibrary = unsafe extern "C" fn();
type FnGetLastError = unsafe extern "C" fn() -> u32;
type FnLoadMemDocument = unsafe extern "C" fn(*const c_void, usize, *const c_char) -> *mut c_void;
type FnCloseDocument = unsafe extern "C" fn(*mut c_void);
type FnGetPageCount = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnLoadPage = unsafe extern "C" fn(*mut c_void, c_int) -> *mut c_void;
type FnClosePage = unsafe extern "C" fn(*mut c_void);
type FnGetPageWidthF = unsafe extern "C" fn(*mut c_void) -> f32;
type FnGetPageHeightF = unsafe extern "C" fn(*mut c_void) -> f32;
type FnBitmapCreateEx =
    unsafe extern "C" fn(c_int, c_int, c_int, *mut c_void, c_int) -> *mut c_void;
type FnBitmapFillRect = unsafe extern "C" fn(*mut c_void, c_int, c_int, c_int, c_int, u32);
type FnBitmapDestroy = unsafe extern "C" fn(*mut c_void);
type FnRenderPageBitmap =
    unsafe extern "C" fn(*mut c_void, *mut c_void, c_int, c_int, c_int, c_int, c_int, c_int);

struct Api {
    // Library 必须活到最后：符号是从它里面取的。
    _lib: Library,
    init: FnInitLibrary,
    last_error: FnGetLastError,
    load_mem: FnLoadMemDocument,
    close_document: FnCloseDocument,
    page_count: FnGetPageCount,
    load_page: FnLoadPage,
    close_page: FnClosePage,
    page_width: FnGetPageWidthF,
    page_height: FnGetPageHeightF,
    bitmap_create_ex: FnBitmapCreateEx,
    bitmap_fill_rect: FnBitmapFillRect,
    bitmap_destroy: FnBitmapDestroy,
    render_page: FnRenderPageBitmap,
}

// 全部调用都经 `api()` 里那把锁串行化；pdfium 的指针只在这个模块内出现。
unsafe impl Send for Api {}

fn library_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = std::env::var_os(ENV_LIBRARY) {
        out.push(PathBuf::from(p));
    }
    let exe_dir = qlr_core::paths::exe_dir();
    out.push(exe_dir.join(LIBRARY_NAME));
    // 开发期（cargo run / 直接跑 target\release）dll 常在 target 目录或仓库根
    out.push(exe_dir.join("..").join(LIBRARY_NAME));
    out.push(PathBuf::from(LIBRARY_NAME));
    out
}

fn load_api() -> std::result::Result<Api, String> {
    let mut tried = Vec::new();
    for candidate in library_candidates() {
        if !candidate.exists() {
            tried.push(format!("{}（不存在）", candidate.display()));
            continue;
        }
        // SAFETY: 只加载我们自己指定的 dll；符号名与 pdfium 公开 C API 一致。
        let lib = match unsafe { Library::new(&candidate) } {
            Ok(l) => l,
            Err(e) => {
                tried.push(format!("{}（加载失败：{e}）", candidate.display()));
                continue;
            }
        };
        unsafe {
            macro_rules! sym {
                ($name:literal, $ty:ty) => {
                    match lib.get::<$ty>($name.as_bytes()) {
                        Ok(s) => *s,
                        Err(e) => {
                            return Err(format!(
                                "pdfium.dll 缺少符号 {}（{}）：{e}",
                                $name,
                                candidate.display()
                            ));
                        }
                    }
                };
            }
            let api = Api {
                init: sym!("FPDF_InitLibrary", FnInitLibrary),
                last_error: sym!("FPDF_GetLastError", FnGetLastError),
                load_mem: sym!("FPDF_LoadMemDocument64", FnLoadMemDocument),
                close_document: sym!("FPDF_CloseDocument", FnCloseDocument),
                page_count: sym!("FPDF_GetPageCount", FnGetPageCount),
                load_page: sym!("FPDF_LoadPage", FnLoadPage),
                close_page: sym!("FPDF_ClosePage", FnClosePage),
                page_width: sym!("FPDF_GetPageWidthF", FnGetPageWidthF),
                page_height: sym!("FPDF_GetPageHeightF", FnGetPageHeightF),
                bitmap_create_ex: sym!("FPDFBitmap_CreateEx", FnBitmapCreateEx),
                bitmap_fill_rect: sym!("FPDFBitmap_FillRect", FnBitmapFillRect),
                bitmap_destroy: sym!("FPDFBitmap_Destroy", FnBitmapDestroy),
                render_page: sym!("FPDF_RenderPageBitmap", FnRenderPageBitmap),
                _lib: lib,
            };
            (api.init)();
            return Ok(api);
        }
    }
    Err(format!("找不到 {LIBRARY_NAME}：{}", tried.join("；")))
}

static API: OnceLock<std::result::Result<Mutex<Api>, String>> = OnceLock::new();

/// 取（必要时加载）pdfium。第一次调用会 `FPDF_InitLibrary`；失败原因会被缓存。
fn api() -> Result<&'static Mutex<Api>> {
    match API.get_or_init(|| load_api().map(Mutex::new)) {
        Ok(api) => Ok(api),
        Err(e) => Err(Error::Message(format!(
            "{e}\nPDF 预览需要 {LIBRARY_NAME} 放在 qlrust.exe 同目录；\
             也可用环境变量 {ENV_LIBRARY} 指定完整路径。"
        ))),
    }
}

/// pdfium 是否可用（不加载文档，只做一次库探测；结果缓存）。
pub fn is_available() -> bool {
    api().is_ok()
}

/// dll 加载失败的详细原因（成功时返回 None）。
pub fn load_error() -> Option<String> {
    API.get_or_init(|| load_api().map(Mutex::new))
        .as_ref()
        .err()
        .cloned()
}

fn pdf_error_message(code: u32) -> &'static str {
    match code {
        FPDF_ERR_SUCCESS => "成功",
        1 => "文件损坏或格式不对（FPDF_ERR_FILE）",
        2 => "需要密码（FPDF_ERR_PASSWORD）",
        3 => "PDF 格式不受支持（FPDF_ERR_FORMAT）",
        4 => "不支持的加密方式（FPDF_ERR_SECURITY）",
        5 => "页面内容错误（FPDF_ERR_PAGE）",
        _ => "未知错误",
    }
}

/// 文档槽：同一时间只常驻**一份**已加载的 PDF。
///
/// 为什么要有它：预览一个 PDF 会连着走三步——`natural_size`（取第 1 页尺寸开窗）、
/// `pdf_page_count`（顶栏"第 x / N 页"）、`render_page`（出图），翻页再各来一遍。
/// 1.8.3 的每一步都是"把整个文件读进内存 + pdfium 解析一遍"，而三步用的是同一份文档。
/// 缓存之后一次预览只读一次、只解析一次，翻页连磁盘都不碰。
///
/// 只留一份的理由：预览是一张一张看的，多槽位除了多占内存没有收益。文件变了
/// （修改时间或长度变了）直接换掉，绝不拿旧文档糊弄用户；特大文档不常驻
/// （见 [`DOC_SLOT_MAX_BYTES`]）。
struct DocSlot {
    path: PathBuf,
    mtime: SystemTime,
    len: u64,
    raw: *mut c_void,
    /// **必须持有**：FPDF_LoadMemDocument64 不拷贝字节，文档在生命周期内
    /// 一直引用调用方的缓冲区。丢了它 pdfium 会读到已释放内存 —— 表现是
    /// 「某些页偶发 LoadPage 返回 NULL」，极难查（2026-10-30 实际踩到）。
    _data: Vec<u8>,
    /// 关闭函数跟着槽走：槽是静态的，`Drop` 里没有 `Api` 可用。
    close: FnCloseDocument,
    pages: usize,
    /// 常驻期间在进程级总预算上占的额度（不常驻时为 `None`）。
    ///
    /// 悬在槽里而不是调用方手里：额度跟着"文档还在内存里"这件事走，槽一清空
    /// （换文件、超限、程序退出）`Drop` 就把它还回 [`crate::budget`]。
    _permit: Option<budget::Permit<'static>>,
}

impl Drop for DocSlot {
    fn drop(&mut self) {
        unsafe { (self.close)(self.raw) }
    }
}

// SAFETY: 裸句柄只在这把锁里被摸，且取槽之前必须先拿 `api()` 的锁（见 `with_document`），
// 所以任何时刻最多一个线程在引用这个 `FPDF_DOCUMENT`。
unsafe impl Send for DocSlot {}

/// 文档槽常驻上限：超过就不缓存，用完立刻关掉。
///
/// 取 256 MB：常见扫描件几十 MB、几百页的文字文档也就一两百 MB；再往上
/// "钉住整份文件 + pdfium 的解析结构"比"多读一次磁盘"更伤内存。
const DOC_SLOT_MAX_BYTES: usize = 256 * 1024 * 1024;

/// 文件身份；读不到就没有身份，也就不复用。
fn identity(path: &Path) -> Option<(SystemTime, u64)> {
    let md = std::fs::metadata(path).ok()?;
    Some((md.modified().ok()?, md.len()))
}

/// 文档槽的锁。**取它之前必须先持有 `api()` 的锁**（顺序固定：api → 槽），
/// 否则两个线程会同时摸同一个 `FPDF_DOCUMENT`，而 pdfium 不是线程安全的。
static SLOT: OnceLock<Mutex<Option<DocSlot>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<DocSlot>> {
    SLOT.get_or_init(|| Mutex::new(None))
}

/// 读字节 + `FPDF_LoadMemDocument64` + 数页。
fn load_document(api: &Api, path: &Path, stamp: Option<(SystemTime, u64)>) -> Result<DocSlot> {
    // 自己读字节：路径里可以有中文，也不受 pdfium 的 ANSI 路径限制。
    let data = std::fs::read(path)
        .map_err(|e| Error::Message(format!("打不开 PDF {}：{e}", path.display())))?;
    if data.is_empty() {
        return Err(Error::Message(format!("PDF 是空文件：{}", path.display())));
    }
    // SAFETY: data 由返回的 DocSlot 持有（见字段注释），生命周期覆盖整个文档。
    let raw = unsafe {
        (api.load_mem)(
            data.as_ptr() as *const c_void,
            data.len(),
            c"".as_ptr() as *const c_char,
        )
    };
    if raw.is_null() {
        let code = unsafe { (api.last_error)() };
        return Err(Error::Message(format!(
            "PDF 打不开（{}）：{}",
            pdf_error_message(code),
            path.display()
        )));
    }
    let count = unsafe { (api.page_count)(raw) };
    let (mtime, len) = stamp.unwrap_or((SystemTime::UNIX_EPOCH, data.len() as u64));
    let doc = DocSlot {
        path: path.to_path_buf(),
        mtime,
        len,
        raw,
        _data: data,
        close: api.close_document,
        pages: count.max(0) as usize,
        _permit: None,
    };
    // 没有可渲染页面：`doc` 在这里 drop，句柄随之关闭，不会漏。
    if doc.pages == 0 {
        return Err(Error::Message(format!(
            "PDF 没有可渲染的页面：{}",
            path.display()
        )));
    }
    Ok(doc)
}

/// 复用（或加载）文档后执行 `f`：`f` 拿到文档句柄与页数。
///
/// 调用方必须已持有 `api()` 的锁；`f` 执行期间文档槽锁也在这条路径上，
/// 所以句柄不会被别人抢先关掉。
fn with_document<T>(
    api: &Api,
    path: &Path,
    f: impl FnOnce(&Api, *mut c_void, usize) -> Result<T>,
) -> Result<T> {
    let mut held = slot()
        .lock()
        .map_err(|_| Error::Message("PDF 文档槽已中毒".into()))?;
    let stamp = identity(path);
    let reuse = match (&*held, stamp) {
        (Some(s), Some((mtime, len))) => s.path == path && s.mtime == mtime && s.len == len,
        _ => false,
    };
    if !reuse {
        // 旧文档先放掉：换文件时它本来就要关，额度也该先还回去再申请
        // （不然"换一份同样大的文档"会自己把自己挡在预算外）。
        *held = None;
        let mut fresh = load_document(api, path, stamp)?;
        if fresh._data.len() > DOC_SLOT_MAX_BYTES {
            // 特大文档不常驻：`fresh` 在 `f` 返回后 drop，句柄随之关闭。
            return f(api, fresh.raw, fresh.pages);
        }
        match budget::try_reserve(fresh._data.len()) {
            Some(permit) => {
                fresh._permit = Some(permit);
                *held = Some(fresh); // 同一时刻只留一份，旧文档在这里 drop → 关闭
            }
            // 总预算被别的缓存占着：这次不常驻，本次照样能用（多读一次盘而已）。
            None => return f(api, fresh.raw, fresh.pages),
        }
    }
    let (raw, pages) = held
        .as_ref()
        .map(|s| (s.raw, s.pages))
        .ok_or_else(|| Error::Message("PDF 文档槽意外为空".to_string()))?;
    f(api, raw, pages)
}

struct PageHandle<'a> {
    api: &'a Api,
    raw: *mut c_void,
}

impl Drop for PageHandle<'_> {
    fn drop(&mut self) {
        unsafe { (self.api.close_page)(self.raw) }
    }
}

/// 页数（用于状态行显示 "第 1 / N 页"）。
pub fn page_count(path: &Path) -> Result<usize> {
    let api = api()?;
    let guard = api
        .lock()
        .map_err(|_| Error::Message("pdfium 状态锁已中毒".into()))?;
    // 走文档槽：同一份 PDF 在被预览期间只读一次、只解析一次。
    with_document(&guard, path, |_, _, pages| Ok(pages.min(MAX_PAGES)))
}

/// 指定页的物理尺寸，单位为 PDF point（1 pt = 1/72 英寸）。
///
/// 只为了"开窗时按原始尺寸定大小"，不渲染像素；越界页自动夹到最后一页。
pub fn page_size(path: &Path, page_index: usize) -> Result<(f64, f64)> {
    let api = api()?;
    let guard = api
        .lock()
        .map_err(|_| Error::Message("pdfium 状态锁已中毒".into()))?;
    with_document(&guard, path, |guard, raw, pages| {
        let index = page_index.min(pages.saturating_sub(1));
        let raw_page = unsafe { (guard.load_page)(raw, index as c_int) };
        if raw_page.is_null() {
            let code = unsafe { (guard.last_error)() };
            return Err(Error::Message(format!(
                "第 {} 页打不开（{}）：{}",
                index + 1,
                pdf_error_message(code),
                path.display()
            )));
        }
        let page = PageHandle {
            api: guard,
            raw: raw_page,
        };
        let (pw, ph) = unsafe {
            (
                (guard.page_width)(page.raw) as f64,
                (guard.page_height)(page.raw) as f64,
            )
        };
        if !(pw.is_finite() && ph.is_finite()) || pw <= 0.0 || ph <= 0.0 {
            return Err(Error::Message(format!(
                "PDF 页面尺寸异常（{pw}x{ph}）：{}",
                path.display()
            )));
        }
        Ok((pw, ph))
    })
}

/// 把指定页渲染成位图（等比缩放到 max_width × max_height 内）。
///
/// `page_index` 从 0 开始；越界自动夹到最后一页。
pub fn render_page(
    path: &Path,
    page_index: usize,
    max_width: u32,
    max_height: u32,
) -> Result<Canvas> {
    crate::cancel::checkpoint()?;
    let api = api()?;
    let guard = api
        .lock()
        .map_err(|_| Error::Message("pdfium 状态锁已中毒".into()))?;
    with_document(&guard, path, |guard, raw, pages| {
        let index = page_index.min(pages.saturating_sub(1));

        let raw_page = unsafe { (guard.load_page)(raw, index as c_int) };
        if raw_page.is_null() {
            let code = unsafe { (guard.last_error)() };
            return Err(Error::Message(format!(
                "第 {} 页打不开（{}）：{}",
                index + 1,
                pdf_error_message(code),
                path.display()
            )));
        }
        let page = PageHandle {
            api: guard,
            raw: raw_page,
        };

        let (pw, ph) = unsafe {
            (
                (guard.page_width)(page.raw) as f64,
                (guard.page_height)(page.raw) as f64,
            )
        };
        if !(pw.is_finite() && ph.is_finite()) || pw <= 0.0 || ph <= 0.0 {
            return Err(Error::Message(format!(
                "PDF 页面尺寸异常（{}x{}）：{}",
                pw,
                ph,
                path.display()
            )));
        }

        // 打开文档 + 取页高这几步在大 PDF 上也要几十毫秒；画之前先问一句"还要不要"，
        // 免得用户已经换页/换文件了还把整页光栅化做完（1.2.0）。
        crate::cancel::checkpoint()?;
        let (bw, bh) = fit_scaled(pw, ph, max_width.max(1) as f64, max_height.max(1) as f64);
        // 缩小（目标像素小于"1:1 = 96 dpi"的页面尺寸）时**超采样 2 倍再盒式缩小**（1.0.8）。
        //
        // 为什么：pdfium 直接往小画布上画矢量文字，笔画只有 1 px 宽还要抗锯齿，
        // 屏幕上就是"糊"。多渲一倍像素再缩，等价于给文字做了一次 4×4 的盒式平均，
        // 小字号反而更结实（这也是各家阅读器缩小时的常规做法）。放大方向不需要。
        const SUPERSAMPLE: f64 = 2.0;
        /// 超采样后的像素预算：一张 A4 缩到 470 px 宽也要 940×1330≈1.25 MP，够用；
        /// 上限防止大页面（A0 图纸）把内存吃穿。
        const SUPERSAMPLE_BUDGET: f64 = 24_000_000.0;
        let natural = 96.0 / 72.0;
        let target_scale = (bw as f64 / pw).min(bh as f64 / ph);
        let mut scale_up = 1.0_f64;
        if target_scale * SUPERSAMPLE < natural {
            let pixels = (bw as f64 * SUPERSAMPLE) * (bh as f64 * SUPERSAMPLE);
            if pixels <= SUPERSAMPLE_BUDGET {
                scale_up = SUPERSAMPLE;
            }
        }
        let rw = ((bw as f64) * scale_up).round().max(1.0) as u32;
        let rh = ((bh as f64) * scale_up).round().max(1.0) as u32;
        // 多渲染 1 倍再缩到目标尺寸没必要（pdfium 自带抗锯齿），
        // 但页面在屏幕上是"缩放后"呈现，所以直接按目标像素渲染即可。
        let width = rw.max(1);
        let height = rh.max(1);
        if u64::from(width) * u64::from(height) > MAX_PAGE_PIXELS
            || width > MAX_PAGE_EDGE
            || height > MAX_PAGE_EDGE
        {
            return Err(Error::Message(format!(
                "PDF 页面渲染尺寸过大（{width}x{height}）：{}",
                path.display()
            )));
        }
        let stride = width as usize * 4;
        let mut buffer = vec![0u8; stride * height as usize];

        // SAFETY: buffer 的容量 = stride * height，正好是 pdfium 要写的范围；
        // bitmap 在 page 关闭前销毁。
        let bitmap = unsafe {
            (guard.bitmap_create_ex)(
                width as c_int,
                height as c_int,
                FPDF_BITMAP_BGRA,
                buffer.as_mut_ptr() as *mut c_void,
                stride as c_int,
            )
        };
        if bitmap.is_null() {
            return Err(Error::Message(format!(
                "pdfium 分配位图失败（{width}x{height}）"
            )));
        }
        // 白底：PDF 页面本身没有背景，不填会得到透明黑（看着像"黑屏"）。
        unsafe {
            (guard.bitmap_fill_rect)(bitmap, 0, 0, width as c_int, height as c_int, 0xFFFF_FFFF);
            (guard.render_page)(
                bitmap,
                page.raw,
                0,
                0,
                width as c_int,
                height as c_int,
                0,
                RENDER_FLAGS,
            );
            (guard.bitmap_destroy)(bitmap);
        }

        let canvas = if scale_up > 1.0 {
            downscale_bgra(&buffer, width, height, bw.max(1), bh.max(1))
        } else {
            Bitmap::new(width, height, buffer)
        };
        Ok(Canvas::Bitmap(canvas))
    })
}

/// 把 BGRA 位图盒式缩到 `dw × dh`（超采样收尾用）。
///
/// 用 `image::imageops::thumbnail`（盒式采样）：缩小时它等价于区域平均，
/// 比 Lanczos 快一个量级，而且不会在文字边缘造出振铃。
fn downscale_bgra(bgra: &[u8], w: u32, h: u32, dw: u32, dh: u32) -> Bitmap {
    let mut rgba = bgra.to_vec();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let Some(img) = image::RgbaImage::from_raw(w, h, rgba) else {
        // 尺寸对不上（理论上不会发生）：原样返回，宁可不缩也不 panic。
        return Bitmap::new(w, h, bgra.to_vec());
    };
    let small = image::imageops::thumbnail(&img, dw.max(1), dh.max(1));
    let mut out = small.into_raw();
    for px in out.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Bitmap::new(dw.max(1), dh.max(1), out)
}

/// dll 自检信息（写启动日志用）。
pub fn describe() -> String {
    match api() {
        Ok(_) => format!("pdfium 可用（{LIBRARY_NAME}）"),
        Err(e) => e.to_string(),
    }
}

/// 把 pw×ph 按比例塞进 max_w×max_h。
///
/// 与 image_view::fit_within 的区别：**这里允许放大**。图片预览不该把
/// 100×100 的图放大成糊状，但 PDF 是矢量——放大后重新栅格化反而更清晰，
/// --zoom-in 正是靠这一点（实测曾因复用 it_within 导致放大后位图尺寸
/// 恒等于页面原始点数 595×842，看起来"缩放没生效"）。
fn fit_scaled(pw: f64, ph: f64, max_w: f64, max_h: f64) -> (u32, u32) {
    let scale = (max_w / pw).min(max_h / ph);
    let w = (pw * scale).round().max(1.0) as u32;
    let h = (ph * scale).round().max(1.0) as u32;
    (w, h)
}
#[test]
fn fit_scaled_can_upscale() {
    // 页面 595×842 塞进 1190×748：受高度限制，等比缩到 ~529×748
    let (w, h) = fit_scaled(595.0, 842.0, 1190.0, 748.0);
    assert_eq!(h, 748);
    assert!((528..=529).contains(&w), "实际 {w}x{h}");
    // 塞进 1488×935：高仍是瓶颈 → 高 935，宽比上面大（允许放大）
    let (w2, h2) = fit_scaled(595.0, 842.0, 1488.0, 935.0);
    assert_eq!(h2, 935);
    assert!(w2 > w && h2 > h, "放大后应更大，实际 {w2}x{h2}");
    assert!((660..=661).contains(&w2), "实际 {w2}x{h2}");
    // 塞进 1859×1169
    let (w3, h3) = fit_scaled(595.0, 842.0, 1859.0, 1169.0);
    assert_eq!(h3, 1169);
    assert!((826..=827).contains(&w3), "实际 {w3}x{h3}");
    // 极端小尺寸不会变 0
    let (w4, h4) = fit_scaled(1.0, 1.0, 1.0, 1.0);
    assert!(w4 >= 1 && h4 >= 1);
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_library_reports_actionable_error() {
        // 不论 dll 在不在，都不能 panic；不可用时错误里必须写清怎么办。
        match api() {
            Ok(_) => assert!(is_available()),
            Err(e) => {
                let msg = e.to_string();
                assert!(msg.contains(LIBRARY_NAME), "错误里要点名 dll：{msg}");
                assert!(
                    msg.contains(ENV_LIBRARY) || msg.contains("同目录"),
                    "错误里要给办法：{msg}"
                );
            }
        }
    }

    #[test]
    fn error_codes_are_human_readable() {
        assert_eq!(pdf_error_message(FPDF_ERR_SUCCESS), "成功");
        assert!(pdf_error_message(2).contains("密码"));
        assert!(!pdf_error_message(999).is_empty());
    }

    /// 手测用（CI 不跑，需要真 pdfium）：
    ///
    /// ```text
    /// $env:QLRUST_PDFIUM="<repo>\target\release\pdfium.dll"
    /// $env:QLR_BENCH_PDF="<repo>\target\real-samples\test-200page.pdf"
    /// cargo test --release -p qlr-render --lib bench_pdf_costs -- --ignored --nocapture
    /// ```
    ///
    /// 分开称"读文件 / 冷加载 / 热复用 / 整页渲染"四段，用来判断优化该往哪儿使劲
    /// （2026-11-03 就是靠它确认：重读全文件只占零点几毫秒，真开销在光栅化）。
    #[test]
    #[ignore]
    fn bench_pdf_costs() {
        use std::time::Instant;
        let Some(path) = std::env::var_os("QLR_BENCH_PDF").map(std::path::PathBuf::from) else {
            println!("未设置 QLR_BENCH_PDF，跳过");
            return;
        };
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        println!("--- {}（{size} B）---", path.display());

        let t = Instant::now();
        for _ in 0..5 {
            let mut data = std::fs::read(&path).expect("读文件");
            std::hint::black_box(&mut data);
        }
        println!("fs::read 一次 {:.2} ms", ms_per(t, 5));

        let api = api().expect("pdfium 不可用");
        {
            let guard = api.lock().expect("锁");
            // 冷加载：直接调 load_document，绕过槽（等价于"文件第一次被预览"）。
            let t = Instant::now();
            for _ in 0..5 {
                let doc = load_document(&guard, &path, identity(&path)).expect("打开文档");
                std::hint::black_box((doc.raw, doc.pages));
            }
            println!(
                "冷加载（读 + 解析 + 数页，未命中槽）一次 {:.2} ms",
                ms_per(t, 5)
            );
        }

        // 热复用：先灌一次槽，再量 page_count（锁 + identity 比对 + 复用）。
        let _ = page_count(&path).expect("数页");
        let t = Instant::now();
        for _ in 0..5 {
            std::hint::black_box(page_count(&path).expect("数页"));
        }
        println!("热复用（命中文档槽）一次 {:.2} ms", ms_per(t, 5));

        // 整页渲染：第一次含冷加载，之后都命中槽 —— 就是用户"滚一格/翻一页"的路径。
        let t = Instant::now();
        for i in 0..5 {
            let canvas = render_page(&path, i * 20, 1589, 894).expect("渲染");
            std::hint::black_box(canvas);
        }
        println!("render_page（首个含冷加载）一次 {:.2} ms", ms_per(t, 5));
    }

    fn ms_per(t: std::time::Instant, times: u32) -> f64 {
        t.elapsed().as_secs_f64() * 1000.0 / f64::from(times)
    }
}
