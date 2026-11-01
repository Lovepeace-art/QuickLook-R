//! 视频/音频"探针"与视频首帧解码（Media Foundation）。
//!
//! **分层例外（读之前先知道）**：本 crate 其余模块刻意不依赖 `windows`（见 `lib.rs` 顶部），
//! 这样 `cargo test` 与基准脚本能在无窗口环境跑。`media` 是唯一例外——Media Foundation
//! 没有跨平台替身，而首帧解码又必须留在渲染层（壳层只负责把 `Canvas` 贴上屏）。
//! 因此本文件是全 crate 唯一出现 `windows::` 的地方，新增 FFI 请照这个边界办。
//!
//! 实现要点（都是踩过的坑，改之前先读）：
//! - **COM 模式**：Source Reader 官方推荐 MTA。本模块在**调用线程**上
//!   `CoInitializeEx(MTA)`；如果该线程已经是 STA（UI 线程在 `selection::init_com` 里
//!   初始化过），会拿到 `RPC_E_CHANGED_MODE`，**这不是错误**——MF 在 STA 上也能用，
//!   只是别指望异步回调。谁初始化的谁 `CoUninitialize`，用 [`ComScope`] 记账。
//! - **MFStartup 只做一次**（进程级）。`MFShutdown` 故意不调：MF 自己有工作队列线程，
//!   在 DLL 卸载/进程退出路径上调它比不调更容易死锁。
//! - **RGB32 的方向**：`MFVideoFormat_RGB32` 的缓冲是 BGRA，但**行序可能是自下而上**
//!   （`MF_MT_DEFAULT_STRIDE` 为负）。这里按 stride 的符号决定要不要翻行，别假设。
//! - 失败一律 `None`：预览链路是"尽力而为"，探测失败只该退化成"暂不支持"，不能 panic。

use std::path::Path;
use std::sync::OnceLock;

use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaBuffer, IMFMediaType, IMFSample, IMFSourceReader, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_PD_DURATION,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_AUDIO_STREAM,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READER_FLAG, MF_SOURCE_READER_MEDIASOURCE,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_VERSION, MFCreateAttributes, MFCreateMediaType,
    MFCreateSourceReaderFromURL, MFMediaType_Video, MFSTARTUP_LITE, MFStartup, MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Variant::{VT_I8, VT_UI8};
use windows::core::{GUID, Interface, PCWSTR};

use crate::image_view::fit_within;
use crate::{Bitmap, Canvas};

/// 日志作用域（`QLRUST_LOG` 里按它过滤）。
const SCOPE: &str = "media";

/// 一条媒体文件的概要信息。字段都能"拿不到就不给"，下游据此降级。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaInfo {
    /// 视频显示宽（像素）。纯音频文件为 0。
    pub width: u32,
    /// 视频显示高（像素）。纯音频文件为 0。
    pub height: u32,
    /// 时长（毫秒）。容器没写时长时为 0。
    pub duration_ms: u64,
    pub has_video: bool,
    pub has_audio: bool,
    /// 视频帧率 `(分子, 分母)`，例如 `(30000, 1001)`。拿不到为 `None`。
    pub frame_rate: Option<(u32, u32)>,
    /// 视频编码的四字符码（`H264` / `HEVC` / `AVC1` …）。拿不到为 `None`。
    pub video_codec: Option<String>,
}

/// 本线程 COM 的记账。`owned == true` 表示是**我们**初始化的，`Drop` 时要收尾。
struct ComScope {
    owned: bool,
}

impl Drop for ComScope {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: 只在 CoInitializeEx 返回成功（S_OK/S_FALSE）时置 owned，
            // 成功一次就必须配一次 CoUninitialize。
            unsafe { CoUninitialize() };
        }
    }
}

/// 在当前线程把 COM 拉起来（MTA 优先；已是 STA 也放行）。
fn com_scope() -> Option<ComScope> {
    // SAFETY: 参数为空指针表示使用 Win32 默认初始化；返回值只用来判断 HRESULT。
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_ok() {
        // S_OK（首次）与 S_FALSE（本线程已初始化过 MTA）都要配对 CoUninitialize。
        return Some(ComScope { owned: true });
    }
    if hr == windows::Win32::Foundation::RPC_E_CHANGED_MODE {
        // 线程已经是 STA（仓库的 UI 线程就是这样）。MF 在 STA 上可用，且这次不是我们
        // 初始化的，所以不能 CoUninitialize。
        qlr_core::log_debug!(SCOPE, "调用线程已是 STA，按 STA 复用 COM");
        return Some(ComScope { owned: false });
    }
    qlr_core::log_warn!(SCOPE, "CoInitializeEx(MTA) 失败：HRESULT=0x{:08X}", hr.0);
    None
}

/// 进程级 `MFStartup`，只做一次。返回 `false` 表示 MF 这辈子不可用。
fn ensure_mf() -> bool {
    static STARTED: OnceLock<bool> = OnceLock::new();
    *STARTED.get_or_init(|| {
        // LITE：不起网络/SDP 那一套，本地文件预览够用，而且启动更快。
        // SAFETY: MFStartup 是进程级初始化，参数是版本号与标志位。
        match unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) } {
            Ok(()) => true,
            Err(e) => {
                qlr_core::log_warn!(SCOPE, "MFStartup 失败：{e}");
                false
            }
        }
    })
}

/// 把路径转成 NUL 结尾的 UTF-16。路径里含 NUL 说明数据已损坏，直接拒绝。
fn wide_path(path: &Path) -> Option<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return None;
    }
    wide.push(0);
    Some(wide)
}

/// 建一个 Source Reader。失败（格式不认识 / 文件损坏 / 缺解码器）返回 `None`。
///
/// **必须带 `MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING`**（1.7.9）：H.264 解码器原生只出
/// NV12，直接 `SetCurrentMediaType(RGB32)` 会被回 `0xC00D36B4 MF_E_INVALIDMEDIATYPE`。
/// 打开视频处理器（Video Processor MFT，Win8+ 系统组件）后，Reader 才会在解码器后面
/// 插一个 NV12→RGB32 的转换，`request_rgb32` 才能成。这个属性**只能在建 Reader 时给**，
/// 建完再往 Reader 的 IMFAttributes 上写不起作用。
fn open_reader(path: &Path) -> Option<IMFSourceReader> {
    let wide = wide_path(path)?;
    // SAFETY: wide 以 NUL 结尾且活得比调用长。
    let attrs = unsafe {
        let mut out: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut out, 2).ok()?;
        let attrs = out?;
        // SAFETY: 属性值是 BOOL(1)，键是常量 GUID。
        attrs
            .SetUINT32(raw(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING), 1)
            .ok()?;
        attrs
    };
    // SAFETY: 两个参数都是我们持有的有效 COM 对象/指针。
    match unsafe { MFCreateSourceReaderFromURL(PCWSTR(wide.as_ptr()), &attrs) } {
        Ok(reader) => Some(reader),
        Err(e) => {
            qlr_core::log_debug!(SCOPE, "打开失败（{}）：{e}", path.display());
            None
        }
    }
}

/// 取媒体类型上的属性接口（`IMFMediaType` 的方法来自 `IMFAttributes`，要显式转一下）。
fn attrs(mt: &IMFMediaType) -> Option<IMFAttributes> {
    mt.cast().ok()
}

/// `MF_MT_FRAME_SIZE` 打包成 `(宽 << 32) | 高`。
fn frame_size(mt: &IMFMediaType) -> Option<(u32, u32)> {
    let packed = unsafe { attrs(mt)?.GetUINT64(raw(&MF_MT_FRAME_SIZE)) }.ok()?;
    let width = (packed >> 32) as u32;
    let height = packed as u32;
    (width > 0 && height > 0).then_some((width, height))
}

/// `MF_MT_FRAME_RATE` 打包成 `(分子 << 32) | 分母`。
fn frame_rate(mt: &IMFMediaType) -> Option<(u32, u32)> {
    let packed = unsafe { attrs(mt)?.GetUINT64(raw(&MF_MT_FRAME_RATE)) }.ok()?;
    let num = (packed >> 32) as u32;
    let den = packed as u32;
    (num > 0 && den > 0).then_some((num, den))
}

/// 把 GUID 转成 `*const GUID`（`windows` 0.62 的属性键是裸指针）。
const fn raw(guid: &GUID) -> *const GUID {
    guid as *const GUID
}

/// 视频子类型 → 四字符码。
///
/// H.264/HEVC/AV1 这类"fourcc 型" GUID 的布局固定是
/// `{FOURCC-0000-0010-8000-00AA00389B71}`，所以不必逐个比对常量（也不可能穷举）。
fn codec_name(mt: &IMFMediaType) -> Option<String> {
    let guid = unsafe { attrs(mt)?.GetGUID(raw(&MF_MT_SUBTYPE)) }.ok()?;
    // 0x00000016-0000-0010-8000-00AA00389B71 这种形状才是 fourcc。
    const FOURCC_TAIL: [u8; 8] = [0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71];
    if guid.data2 != 0 || guid.data3 != 0x0010 || guid.data4 != FOURCC_TAIL {
        return None;
    }
    let bytes = guid.data1.to_le_bytes();
    if !bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).trim_end().to_owned())
}

/// 时长（毫秒）：`MF_PD_DURATION` 是 100 ns 单位。
fn duration_ms(reader: &IMFSourceReader) -> Option<u64> {
    // SAFETY: 返回的 PROPVARIANT 在下面按 vt 判断后才读 uhVal。
    let var = unsafe {
        reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, raw(&MF_PD_DURATION))
    }
    .ok()?;
    // SAFETY: 联合体读取，前置条件由 vt 检查保证（VT_UI8 才有 uhVal）。
    let (vt, ticks) = unsafe {
        (
            var.Anonymous.Anonymous.vt,
            var.Anonymous.Anonymous.Anonymous.uhVal,
        )
    };
    if vt != VT_UI8 {
        return None;
    }
    Some(ticks / 10_000)
}

/// 探测一个媒体的元信息。不是媒体 / 没有可用流 / MF 初始化失败都返回 `None`。
pub fn probe(path: &Path) -> Option<MediaInfo> {
    if !path.is_file() {
        return None;
    }
    let _com = com_scope()?;
    if !ensure_mf() {
        return None;
    }
    let reader = open_reader(path)?;

    // 用 `GetNativeMediaType`（原始类型）而不是"当前类型"：后者在我们没 SetCurrentMediaType
    // 之前就是解码器默认值，尺寸/帧率可能已被改写。
    let video =
        unsafe { reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0) }.ok();
    let has_audio =
        unsafe { reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32, 0) }
            .is_ok();
    if video.is_none() && !has_audio {
        // 既没视频也没音频：这不是我们能预览的东西（例如把 .txt 改后缀成 .mp4）。
        return None;
    }

    let (width, height) = video.as_ref().and_then(frame_size).unwrap_or((0, 0));
    Some(MediaInfo {
        width,
        height,
        duration_ms: duration_ms(&reader).unwrap_or(0),
        has_video: video.is_some(),
        has_audio,
        frame_rate: video.as_ref().and_then(frame_rate),
        video_codec: video.as_ref().and_then(codec_name),
    })
}

/// 让 Source Reader 直接输出 RGB32（内存里就是 BGRA，省一次通道重排）。
fn request_rgb32(reader: &IMFSourceReader) -> Option<()> {
    // SAFETY: 都是 COM 调用，对象由 windows crate 的包装类型持有。
    let mt: IMFMediaType = match unsafe { MFCreateMediaType() } {
        Ok(mt) => mt,
        Err(e) => {
            qlr_core::log_debug!(SCOPE, "MFCreateMediaType 失败：{e}");
            return None;
        }
    };
    let a = attrs(&mt)?;
    unsafe {
        if let Err(e) = a.SetGUID(raw(&MF_MT_MAJOR_TYPE), raw(&MFMediaType_Video)) {
            qlr_core::log_debug!(SCOPE, "设置主类型失败：{e}");
            return None;
        }
        if let Err(e) = a.SetGUID(raw(&MF_MT_SUBTYPE), raw(&MFVideoFormat_RGB32)) {
            qlr_core::log_debug!(SCOPE, "设置 RGB32 子类型失败：{e}");
            return None;
        }
    }
    // `pdwreserved` 必须为空。
    unsafe {
        if let Err(e) =
            reader.SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, None, &mt)
        {
            // 0xC00D36B4 = MF_E_INVALIDMEDIATYPE：多半是没开视频处理器（见 `open_reader`）。
            qlr_core::log_debug!(SCOPE, "要 RGB32 被拒：0x{:08X} {e}", e.code().0);
            return None;
        }
    }
    Some(())
}

/// 读第一帧样本。跳过 `STREAMTICK`（只有时间戳没有像素），最多试 8 次。
fn read_first_sample(
    reader: &IMFSourceReader,
) -> Option<windows::Win32::Media::MediaFoundation::IMFSample> {
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    for _ in 0..8 {
        let mut actual_stream = 0u32;
        let mut flags = 0u32;
        let mut timestamp = 0i64;
        let mut sample: Option<windows::Win32::Media::MediaFoundation::IMFSample> = None;
        // SAFETY: 六个参数在调用期间都有效；sample 由 MF 填。
        unsafe {
            reader
                .ReadSample(
                    stream,
                    0,
                    Some(&mut actual_stream),
                    Some(&mut flags),
                    Some(&mut timestamp),
                    Some(&mut sample),
                )
                .ok()?;
        }
        let flags = MF_SOURCE_READER_FLAG(flags as i32);
        if flags.contains(MF_SOURCE_READERF_ENDOFSTREAM) {
            return None;
        }
        if let Some(sample) = sample {
            return Some(sample);
        }
    }
    None
}

/// 把样本的像素连同行跨距一起拷出来（跨距由 stride 描述，可能带尾部填充）。
fn copy_pixels(
    sample: &windows::Win32::Media::MediaFoundation::IMFSample,
) -> Option<(Vec<u8>, usize)> {
    // SAFETY: ConvertToContiguousBuffer 返回可直接 Lock 的连续缓冲。
    let buffer: IMFMediaBuffer = unsafe { sample.ConvertToContiguousBuffer() }.ok()?;
    let mut data: *mut u8 = std::ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: 输出参数都是本地变量；Lock 成功后必须 Unlock，下面保证成对。
    unsafe { buffer.Lock(&mut data, None, Some(&mut len)) }.ok()?;
    if data.is_null() || len == 0 {
        // SAFETY: 与上面的 Lock 配对。
        let _ = unsafe { buffer.Unlock() };
        return None;
    }
    // SAFETY: MF 保证 [data, data+len) 可读且在 Unlock 前有效。
    let pixels = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
    // SAFETY: 与上面的 Lock 配对。
    let _ = unsafe { buffer.Unlock() };
    Some((pixels, len as usize))
}

/// 像素缓冲的行跨距（字节）。负值表示**自下而上**存储。
fn current_stride(reader: &IMFSourceReader) -> Option<i32> {
    let mt =
        unsafe { reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32) }.ok()?;
    let stride = unsafe { attrs(&mt)?.GetUINT32(raw(&MF_MT_DEFAULT_STRIDE)) }.ok()?;
    Some(stride as i32)
}

/// 把一帧 RGB32 缓冲整理成"自上而下、无填充"的 BGRA，并把 alpha 补成 255。
///
/// 为什么要自己翻行：`MFVideoFormat_RGB32` 是 DIB 语义，解码器给的行序常常是自下而上
/// （`MF_MT_DEFAULT_STRIDE < 0`）。不翻的话预览窗里上下颠倒，而且看起来"只是有点怪"，
/// 很难联想到方向问题。
fn normalize_bgra(pixels: &[u8], width: u32, height: u32, stride: i32) -> Option<Vec<u8>> {
    let row = width as usize * 4;
    let abs_stride = stride.unsigned_abs() as usize;
    if abs_stride < row {
        return None;
    }
    if pixels.len() < abs_stride * (height as usize - 1) + row {
        return None;
    }
    let mut out = vec![0u8; row * height as usize];
    for y in 0..height as usize {
        let src_row = if stride < 0 {
            height as usize - 1 - y
        } else {
            y
        };
        let src = src_row * abs_stride;
        out[y * row..(y + 1) * row].copy_from_slice(&pixels[src..src + row]);
    }
    // RGB32 的第 4 字节是 "X"（未定义），上屏/合成时会变成全透明，统一钉成 255。
    for px in out.chunks_exact_mut(4) {
        px[3] = 255;
    }
    Some(out)
}

/// 等比缩到显示预算内（只缩不放，和 `image_view` 的默认口径一致）。
fn fit_bitmap(
    bgra: Vec<u8>,
    width: u32,
    height: u32,
    max_width: u32,
    max_height: u32,
) -> Option<Bitmap> {
    let (dw, dh) = fit_within(width, height, max_width, max_height);
    if (dw, dh) == (width, height) {
        return Some(Bitmap::new(width, height, bgra));
    }
    let mut rgba = bgra;
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let img = image::RgbaImage::from_raw(width, height, rgba)?;
    // thumbnail：缩小时等价于盒式区域平均，比 Lanczos 快一个量级且不会在文字边缘振铃。
    let mut out = image::imageops::thumbnail(&img, dw.max(1), dh.max(1)).into_raw();
    for px in out.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Some(Bitmap::new(dw.max(1), dh.max(1), out))
}

/// 取视频**首帧**，等比缩到 `max_width × max_height` 以内（不放大）。
///
/// 拿不到首帧（无视频流 / 解码失败 / MF 不可用）返回 `None`，不 panic。
pub fn first_frame(path: &Path, max_width: u32, max_height: u32) -> Option<Canvas> {
    if !path.is_file() {
        return None;
    }
    let _com = com_scope()?;
    if !ensure_mf() {
        return None;
    }
    let reader = open_reader(path)?;
    request_rgb32(&reader)?;

    // 尺寸以**协商后**的当前类型为准（RGB32 输出可能带对齐填充）。
    let mt =
        unsafe { reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32) }.ok()?;
    let (width, height) = frame_size(&mt)?;

    let sample = read_first_sample(&reader)?;
    let (pixels, len) = copy_pixels(&sample)?;
    // stride 拿不到就按"紧凑、自上而下"赌一把——绝大多数解码器在 RGB32 下就是 w*4。
    let stride = current_stride(&reader).unwrap_or(width as i32 * 4);
    qlr_core::log_debug!(
        SCOPE,
        "首帧 {width}x{height} stride={stride} bytes={len}（{}）",
        path.display()
    );

    let bgra = normalize_bgra(&pixels, width, height, stride)?;
    let bitmap = fit_bitmap(bgra, width, height, max_width, max_height)?;
    Some(Canvas::Bitmap(bitmap))
}

/// 播放时的一帧：自上而下、无填充的 BGRA。
#[derive(Clone)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    /// 这一帧在时间轴上的位置（毫秒，Media Foundation 给的时间戳）。
    pub ts_ms: u64,
}

/// 按顺序解码视频帧的流（1.7.9 的视频播放）。
///
/// 为什么是 `SourceReader` 而不是 `IMFMediaSession` + EVR：预览窗要的是"能把帧贴到我们
/// 自己的位图上"，用 EVR 就得再开一个视频子窗口并处理它自己的激活/暂停语义，和 1.7.9 刚理
/// 顺的"Office 子窗口接管输入"那套会打架。顺序读样本 = 解码在后台线程也没必要，720p 一帧
/// 几毫秒，直接在 `WM_TIMER` 里读下一帧就够。
pub struct FrameStream {
    reader: IMFSourceReader,
    /// 本线程 COM 的一次性记账（Drop 顺序：字段按声明顺序 drop，reader 先于 _com）。
    _com: ComScope,
    width: u32,
    height: u32,
    duration_ms: u64,
    ended: bool,
}

// `IMFSourceReader` 是 COM 接口（内部是 `NonNull<c_void>`），默认不是 `Send`。
// 壳层把它放进 `static Mutex<State>`（窗口状态本来就是全局的），所以这里要显式声明。
//
// SAFETY: 实际使用严格单线程——`FrameStream` 由 UI 线程创建，之后只在同一个线程的
// `WM_TIMER`/`WM_PAINT` 里被调用；没有任何跨线程传递，也没有并发调用。
// 换线程用（例如挪到渲染线程）会违反这条，别这么做。
unsafe impl Send for FrameStream {}

impl FrameStream {
    /// 打开一个视频文件准备逐帧解码。失败（无视频流 / 缺解码器 / MF 不可用）返回 `None`。
    pub fn open(path: &Path) -> Option<Self> {
        if !path.is_file() {
            return None;
        }
        let com = com_scope()?;
        if !ensure_mf() {
            return None;
        }
        let reader = open_reader(path)?;
        request_rgb32(&reader)?;
        let mt =
            unsafe { reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32) }
                .ok()?;
        let (width, height) = frame_size(&mt)?;
        let duration = duration_ms(&reader).unwrap_or(0);
        qlr_core::log_debug!(
            SCOPE,
            "FrameStream 打开 {width}x{height} 时长 {duration}ms（{}）",
            path.display()
        );
        Some(Self {
            reader,
            _com: com,
            width,
            height,
            duration_ms: duration,
            ended: false,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    /// 读下一帧。到末尾返回 `None`（此后一直 `None`，除非 [`Self::seek`]）。
    pub fn next_frame(&mut self) -> Option<RawFrame> {
        if self.ended {
            return None;
        }
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        // 最多试 64 次：音频流的样本也会从 ReadSample 里冒出来（actual_stream 不是视频流），
        // 还有 `STREAMTICK` 这种"只有时间戳没有像素"的通知，都要跳过。
        for _ in 0..64 {
            let mut actual_stream = 0u32;
            let mut flags = 0u32;
            let mut timestamp = 0i64;
            let mut sample: Option<IMFSample> = None;
            // SAFETY: 六个参数在调用期间都有效；sample 由 MF 填。
            unsafe {
                self.reader
                    .ReadSample(
                        stream,
                        0,
                        Some(&mut actual_stream),
                        Some(&mut flags),
                        Some(&mut timestamp),
                        Some(&mut sample),
                    )
                    .ok()?;
            }
            if MF_SOURCE_READER_FLAG(flags as i32).contains(MF_SOURCE_READERF_ENDOFSTREAM) {
                self.ended = true;
                return None;
            }
            let Some(sample) = sample else {
                continue;
            };
            let (pixels, len) = copy_pixels(&sample)?;
            let stride = current_stride(&self.reader).unwrap_or(self.width as i32 * 4);
            let bgra = normalize_bgra(&pixels, self.width, self.height, stride)?;
            // 逐帧不打日志：30fps 下每秒 30 行会把日志刷爆（排查时看 "FrameStream 打开/跳到" 就够）。
            let _ = len;
            return Some(RawFrame {
                width: self.width,
                height: self.height,
                bgra,
                ts_ms: (timestamp / 10_000).max(0) as u64,
            });
        }
        None
    }

    /// 跳到 `position_ms`。之后第一帧就是该位置附近（MF 自己往前滚到关键帧再解）。
    pub fn seek(&mut self, position_ms: u64) -> bool {
        let target = position_ms.min(self.duration_ms.max(position_ms));
        let pv = propvariant_i64((target as i64) * 10_000);
        // `guidtimeformat` 必须是一个**有效的** GUID 指针：MSDN 说传 NULL 表示"100ns 单位"，
        // 但本机 mfreadwrite 会直接解引用它（实测 STATUS_ACCESS_VIOLATION）。
        // 官方样例走的也是 `GUID_NULL`（全零 GUID），效果相同：按 100ns 单位解释。
        let guid_format = windows::core::GUID::zeroed();
        // SAFETY: 两个指针都在调用期间有效；pv 由 COM 复制走。
        let ok = unsafe { self.reader.SetCurrentPosition(&guid_format, &pv).is_ok() };
        if ok {
            self.ended = false;
            qlr_core::log_debug!(SCOPE, "FrameStream 跳到 {target}ms");
        } else {
            qlr_core::log_debug!(SCOPE, "FrameStream 跳转失败 {target}ms");
        }
        ok
    }
}

/// 造一个 "64 位整数" 的 `PROPVARIANT`（`SetCurrentPosition` 只认这个形状）。
///
/// 只能整块构造：这些类型是 `#[repr(C)]` 联合体，字段级赋值要过好几层 `ManuallyDrop`
/// 解引用，写起来比直接构造更易错。
fn propvariant_i64(value: i64) -> PROPVARIANT {
    PROPVARIANT {
        // SAFETY: 目标形状是 `VT_I8` + 64 位整数，联合体里读的就是 `hVal`，与写入一致。
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_I8,
                Anonymous: PROPVARIANT_0_0_0 { hVal: value },
                ..Default::default()
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 夹具目录：优先环境变量 `QLRUST_MEDIA_FIXTURES`，否则按开发机布局推算
    /// （`<桌面>\DSH\quicklook-rs\crates\qlr-render` 往上四层 → `<桌面>\qlrust-dev\compare`）。
    /// 发布仓库不带夹具，缺少时跳过。
    ///
    /// 夹具不在时**跳过**（打印一行原因）而不是失败：媒体单测依赖真实 mp4/wav，
    /// 缺文件是环境问题，不是代码问题。开发机上这两个夹具由
    /// `_bench/make-compare-fixtures.ps1` 生成。
    fn fixture(name: &str) -> Option<PathBuf> {
        let dir = std::env::var_os("QLRUST_MEDIA_FIXTURES")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../../qlrust-dev/compare")
            });
        let path = dir.join(name);
        if path.is_file() {
            Some(path)
        } else {
            eprintln!(
                "跳过：缺少夹具 {}（可设 QLRUST_MEDIA_FIXTURES）",
                path.display()
            );
            None
        }
    }

    #[test]
    fn probe_reads_dimensions_duration_and_streams() {
        let Some(path) = fixture("video-demo.mp4") else {
            return;
        };
        let info = probe(&path).expect("video-demo.mp4 应该能 probe 成功");
        eprintln!("probe(video-demo.mp4) = {info:?}");
        assert_eq!((info.width, info.height), (1280, 720), "{info:?}");
        assert!(info.has_video, "应识别出视频流：{info:?}");
        assert!(info.has_audio, "应识别出音频流（带 AAC）：{info:?}");
        // 夹具是 3 s；demuxer 给的容器时长可能差几十毫秒。
        assert!(
            (2_800..=3_200).contains(&info.duration_ms),
            "时长应约 3000 ms，实际 {}",
            info.duration_ms
        );
    }

    #[test]
    fn first_frame_is_scaled_and_non_empty() {
        let Some(path) = fixture("video-demo.mp4") else {
            return;
        };
        let canvas = first_frame(&path, 640, 320).expect("应能解出首帧");
        let Canvas::Bitmap(bitmap) = canvas else {
            panic!("首帧必须是位图");
        };
        // 1280x720 缩到 640x320 的预算里 → 640x360 会超高，按宽限缩到 568x320。
        eprintln!(
            "first_frame(640x320) = {}x{}, bytes={}",
            bitmap.width,
            bitmap.height,
            bitmap.bytes()
        );
        assert_eq!(bitmap.width, 568, "按高受限应缩到 568x320");
        assert_eq!(bitmap.height, 320);
        assert_eq!(
            bitmap.bgra.len(),
            bitmap.width as usize * bitmap.height as usize * 4
        );
        // 画面必须有内容（testsrc2 是彩条 + 大字，不可能全黑）。
        let lit = bitmap
            .bgra
            .chunks_exact(4)
            .filter(|px| px[0] > 16 || px[1] > 16 || px[2] > 16)
            .count();
        let total = bitmap.bgra.len() / 4;
        eprintln!("非黑像素 {lit}/{total}");
        assert!(lit * 2 > total, "首帧应大部分是亮色，实际 {lit}/{total}");
        // alpha 必须被钉成 255（RGB32 的第 4 字节是未定义的 X）。
        assert!(
            bitmap.bgra.chunks_exact(4).all(|px| px[3] == 255),
            "alpha 应统一为 255"
        );
    }

    #[test]
    fn first_frame_without_budget_keeps_native_size() {
        let Some(path) = fixture("video-demo.mp4") else {
            return;
        };
        // 预算给 0 表示"不限制"（见 image_view::fit_within 的约定）。
        let canvas = first_frame(&path, 0, 0).expect("应能解出首帧");
        assert_eq!(canvas.dimensions(), (1280, 720));
    }

    #[test]
    fn missing_file_returns_none() {
        let ghost = PathBuf::from(r"C:\__qlrust_media_does_not_exist__\nope.mp4");
        assert!(probe(&ghost).is_none());
        assert!(first_frame(&ghost, 320, 240).is_none());
    }

    #[test]
    fn garbage_pretending_to_be_mp4_returns_none_without_panic() {
        let bad = crate::testutil::write_file("media", "broken.mp4", b"definitely not a video");
        assert!(probe(&bad).is_none(), "坏文件必须返回 None");
        assert!(first_frame(&bad, 320, 240).is_none(), "坏文件必须返回 None");
    }

    #[test]
    fn non_media_extension_is_rejected() {
        let txt = crate::testutil::write_file("media", "plain.txt", b"hello");
        assert!(probe(&txt).is_none(), "没有音视频流的文件不算媒体");
    }

    /// 首帧落盘成 PNG + 打 3 次耗时（手动评估/报告用，所以 `#[ignore]`）。
    ///
    /// 跑法：`cargo test -p qlr-render -- --ignored --nocapture dump_first_frame`
    /// 输出路径可用 `QLRUST_FRAME_OUT` 覆盖，默认 `target/test-tmp/media/video-frame.png`。
    #[test]
    #[ignore = "落盘首帧 PNG + 计时（手动评估用）"]
    fn dump_first_frame_png() {
        let Some(path) = fixture("video-demo.mp4") else {
            return;
        };
        let out = std::env::var_os("QLRUST_FRAME_OUT")
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::testutil::dir("media").join("video-frame.png"));

        // 冷/热各测：第 1 次含 MF 初始化与解码器加载，后两次才是热路径量级。
        let mut times = Vec::new();
        let mut last = None;
        for _ in 0..3 {
            let started = std::time::Instant::now();
            let canvas = first_frame(&path, 1280, 720).expect("应能解出首帧");
            times.push(started.elapsed());
            last = Some(canvas);
        }
        let Some(Canvas::Bitmap(bitmap)) = last else {
            panic!("首帧必须是位图");
        };

        // 非黑像素占比（判"是不是一张真的解码出来的画面"）。
        let total = (bitmap.width as usize) * (bitmap.height as usize);
        let lit = bitmap
            .bgra
            .chunks_exact(4)
            .filter(|px| px[0] > 16 || px[1] > 16 || px[2] > 16)
            .count();

        let mut rgba = bitmap.bgra.clone();
        for px in rgba.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        let img =
            image::RgbaImage::from_raw(bitmap.width, bitmap.height, rgba).expect("尺寸应匹配");
        img.save(&out).expect("PNG 落盘失败");

        eprintln!(
            "首帧 {}x{} 非黑像素 {lit}/{total} = {:.1}%",
            bitmap.width,
            bitmap.height,
            lit as f64 * 100.0 / total as f64
        );
        eprintln!(
            "耗时 3 次：{}",
            times
                .iter()
                .map(|t| format!("{:.1} ms", t.as_secs_f64() * 1000.0))
                .collect::<Vec<_>>()
                .join(" / ")
        );
        eprintln!("已落盘：{}", out.display());
        assert!(lit * 2 > total, "画面不该以黑色为主");
    }

    #[test]
    fn frame_stream_walks_forward_and_can_seek_back() {
        let Some(path) = fixture("video-demo.mp4") else {
            return;
        };
        let mut stream = FrameStream::open(&path).expect("应能建帧流");
        assert_eq!((stream.width(), stream.height()), (1280, 720));
        assert!(
            (2_800..=3_200).contains(&stream.duration_ms()),
            "时长应约 3000 ms，实际 {}",
            stream.duration_ms()
        );

        let first = stream.next_frame().expect("应能解出第 1 帧");
        assert_eq!((first.width, first.height), (1280, 720));
        assert_eq!(first.bgra.len(), 1280 * 720 * 4);
        // 每一帧都得是"铺满整张"的，不是残影：全透明像素说明 alpha 没填。
        assert!(
            first.bgra.chunks_exact(4).all(|p| p[3] == 255),
            "BGRA 的 alpha 应统一填 255"
        );

        let second = stream.next_frame().expect("应能解出第 2 帧");
        assert!(
            second.ts_ms > first.ts_ms,
            "第 2 帧时间戳 {} 应大于第 1 帧 {}",
            second.ts_ms,
            first.ts_ms
        );
        assert_ne!(
            first.bgra, second.bgra,
            "夹具是彩条动画，相邻两帧的像素不该完全一样"
        );

        // 往回跳之后还能继续出帧（播放器"快退"就靠这条）。
        assert!(stream.seek(0), "seek(0) 应成功");
        let again = stream.next_frame().expect("seek 后应仍能出帧");
        assert_eq!(again.bgra.len(), 1280 * 720 * 4);
    }

    #[test]
    fn frame_stream_on_non_media_file_is_none() {
        let Some(path) = fixture("text-long.txt") else {
            return;
        };
        assert!(FrameStream::open(&path).is_none(), "文本文件不该建出帧流");
    }

    #[test]
    fn stride_flip_and_padding_are_handled() {
        // 2x2、stride=+12（每行带 4 字节填充）：自上而下，直接裁掉填充。
        let top_down = vec![
            1, 2, 3, 0, 4, 5, 6, 0, 9, 9, 9, 9, // 第 0 行 + 填充
            7, 8, 9, 0, 10, 11, 12, 0, 9, 9, 9, 9, // 第 1 行 + 填充
        ];
        let out = normalize_bgra(&top_down, 2, 2, 12).expect("应能整理");
        assert_eq!(out.len(), 16);
        assert_eq!(&out[0..8], &[1, 2, 3, 255, 4, 5, 6, 255]);
        assert_eq!(&out[8..16], &[7, 8, 9, 255, 10, 11, 12, 255]);

        // 同样的缓冲但 stride = -12：自下而上，行序要翻过来。
        let bottom_up = top_down.clone();
        let out = normalize_bgra(&bottom_up, 2, 2, -12).expect("应能整理");
        assert_eq!(&out[0..8], &[7, 8, 9, 255, 10, 11, 12, 255]);
        assert_eq!(&out[8..16], &[1, 2, 3, 255, 4, 5, 6, 255]);

        // 缓冲比"最后一行像素的末尾"还短 → 只报 None，不越界读。
        // 2 行 × stride 12 时，最后一行像素结束于 12 + 2*4 = 20，所以 19 才是"不够"。
        assert!(normalize_bgra(&top_down[..19], 2, 2, 12).is_none());
    }
}
