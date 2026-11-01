//! QLRust 插件 ABI v1。
//!
//! 这个 crate 只有一份职责：定义**宿主和插件之间的唯一契约**。
//! 它是纯 C 布局 + 三个导出函数指针，**不依赖 serde、不依赖 windows、不依赖任何本项目类型**——
//! 插件可以用 Rust 写，也可以用 C/C++/Zig/Delphi 写，只要能导出下面三个符号。
//!
//! # 插件长什么样
//!
//! 一个插件就是一个 DLL（`<exe 目录>\plugins\*.dll`），导出三个 `extern "C"` 符号：
//!
//! | 符号 | 类型 | 职责 |
//! |---|---|---|
//! | `qlr_plugin_info` | [`QlrPluginInfoFn`] | 报 ABI 版本、名字、接管哪些扩展名 |
//! | `qlr_plugin_render` | [`QlrPluginRenderFn`] | 把文件渲染成一张 RGBA 位图 |
//! | `qlr_plugin_free` | [`QlrPluginFreeFn`] | 释放上一行返回的像素内存 |
//!
//! # 约定（宿主会按这些约定校验）
//!
//! - **像素格式**：RGBA8，逐行连续，`stride = width * 4`，左上角为原点（行优先、自上而下）。
//! - **内存归属**：`render` 里由**插件**分配，`free` 里由插件释放；宿主只读、绝不 `free`。
//! - **尺寸**：插件应尽量把结果放进 `max_width × max_height`（等比缩放），
//!   但宿主不强制——超出的部分会按"适应窗口"缩下去。像素总量上限 [`MAX_PIXELS`]，
//!   超了宿主直接拒收（防止一个插件把宿主吃成 4 GB）。
//! - **失败**：返回非 0 即可，宿主会把占位文字显示成"插件渲染失败"；此时**不要**填 `out`。
//! - **线程**：`render` 在宿主的渲染线程上调用（**不是** UI 线程），插件可以阻塞，但别死循环。
//! - **字符串**：一律用 `(ptr, len)` 的 UTF-8 字节对，**不带结尾 NUL**，生命周期为 `'static`（info）
//!   或调用期间（路径）。
//!
//! # 版本策略
//!
//! [`ABI_VERSION`] 只在**破坏性变更**时递增（改结构体字段、改调用约定、改像素格式）。
//! 宿主只加载版本完全相等的插件，其余记为"版本不匹配"并跳过——不做兼容层，
//! 因为插件是独立分发的，宁可显式失败也不要悄悄跑错。
//!
//! # 示例
//!
//! 仓库里有一个可运行的示例插件：`crates/qlr-plugin-demo`（接管 `.mp4` 等视频扩展名，
//! 画一张色条占位图）。它的 `src/lib.rs` 是照着抄的最佳起点。

/// 当前 ABI 版本。破坏性变更时才 +1。
pub const ABI_VERSION: u32 = 1;

/// 单张位图的像素总量上限（64 MP，约 256 MB RGBA）。
///
/// 宿主拿到 `width * height > MAX_PIXELS` 的结果一律拒收：
/// 插件是第三方代码，不能让它靠一个 `u32::MAX` 把宿主进程撑爆。
pub const MAX_PIXELS: u32 = 64 * 1024 * 1024;

/// 渲染成功。
pub const RENDER_OK: i32 = 0;
/// 插件自己知道渲染不了（格式不对、文件损坏等）。
pub const RENDER_FAILED: i32 = 1;
/// 文件太大 / 超出宿主给的预算，插件主动放弃。
pub const RENDER_TOO_BIG: i32 = 2;

/// 宿主 → 插件的位图容器。
///
/// `pixels` 指向 `width * height * 4` 字节的 RGBA8 数据，由插件分配、插件释放
/// （见 [`QlrPluginFreeFn`]）。
#[repr(C)]
#[derive(Debug)]
pub struct QlrImage {
    /// 宽（像素），必须 > 0。
    pub width: u32,
    /// 高（像素），必须 > 0。
    pub height: u32,
    /// RGBA8 像素，长度必须是 `width * height * 4`。
    pub pixels: *mut u8,
    /// `pixels` 的字节长度（宿主会拿它和 `width * height * 4` 对账）。
    pub len: usize,
}

impl QlrImage {
    /// 空图（插件在失败路径上应该直接不填 `out`，这只是给 `Default` 用）。
    pub const fn empty() -> Self {
        Self {
            width: 0,
            height: 0,
            pixels: core::ptr::null_mut(),
            len: 0,
        }
    }
}

/// 插件自述。
#[repr(C)]
#[derive(Debug)]
pub struct QlrPluginInfo {
    /// 必须等于 [`ABI_VERSION`]，否则宿主跳过这个插件。
    pub abi_version: u32,
    /// 插件名（UTF-8，无结尾 NUL），宿主写进日志和占位文字。
    pub name: *const u8,
    /// `name` 的字节长度。
    pub name_len: u32,
    /// 接管的扩展名，分号分隔、小写、不带点（例如 `"mp4;mkv;webm"`）。
    pub extensions: *const u8,
    /// `extensions` 的字节长度。
    pub extensions_len: u32,
}

/// `qlr_plugin_info` 的签名。
pub type QlrPluginInfoFn = unsafe extern "C" fn() -> QlrPluginInfo;

/// `qlr_plugin_render` 的签名。
///
/// `path` / `path_len`：待预览文件的完整路径（UTF-8，无结尾 NUL，仅在调用期间有效）。
/// `max_width` / `max_height`：宿主建议的最大尺寸，插件应等比缩到里面。
/// `out`：成功时写入位图；失败时**不要**碰它。
pub type QlrPluginRenderFn = unsafe extern "C" fn(
    path: *const u8,
    path_len: usize,
    max_width: u32,
    max_height: u32,
    out: *mut QlrImage,
) -> i32;

/// `qlr_plugin_free` 的签名。
///
/// 宿主用完之后回调它，插件负责把 `QlrImage` 里自己的像素内存放掉（`width/height/len`
/// 会被宿主清零，指针可能为 null —— 插件要能容忍）。
pub type QlrPluginFreeFn = unsafe extern "C" fn(image: *mut QlrImage);

/// `qlr_plugin_info` 的符号名（含结尾 NUL，`GetProcAddress` / `libloading::Library::get` 都要）。
pub const SYM_INFO: &[u8] = b"qlr_plugin_info\0";
/// `qlr_plugin_render` 的符号名。
pub const SYM_RENDER: &[u8] = b"qlr_plugin_render\0";
/// `qlr_plugin_free` 的符号名。
pub const SYM_FREE: &[u8] = b"qlr_plugin_free\0";
/// 可选的"文本输出"符号名（1.6.0）。
///
/// 插件如果导出它，宿主会**优先**用文本渲染（等宽、可滚动、清晰），位图那条路仍然保留做兜底。
/// 约定：返回一段 UTF-8 文本（NUL 结尾不在 buf 里也算），宿主拷走后调用 `qlr_plugin_text_free`。
pub const SYM_TEXT: &[u8] = b"qlr_plugin_text\0";
/// 释放 `qlr_plugin_text` 返回的缓冲区。
pub const SYM_TEXT_FREE: &[u8] = b"qlr_plugin_text_free\0";

/// `qlr_plugin_text(path, path_len, out_ptr, out_len) -> i32`（0 = 成功）。
///
/// `out_ptr` 指向一块插件自己分配的 UTF-8 字节；`out_len` 是字节数。
pub type QlrPluginTextFn = unsafe extern "C" fn(
    path: *const u8,
    path_len: usize,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> i32;
/// `qlr_plugin_text_free(ptr, len)`：把上面那块内存还回去。
pub type QlrPluginTextFreeFn = unsafe extern "C" fn(ptr: *mut u8, len: usize);

/// 解析插件自述里的扩展名串。
///
/// 规则（宽松，宁可救回来也不丢插件）：按 `;`/`,`/空白切分，去点、转小写、去空、去重。
/// 真正的"能不能用"由宿主按扩展名查表决定，这里只做归一化。
pub fn parse_extensions(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for piece in raw.split([';', ',', ' ', '\t', '\r', '\n']) {
        let ext = piece.trim().trim_start_matches('.').to_ascii_lowercase();
        if ext.is_empty() {
            continue;
        }
        if !out.iter().any(|e| e == &ext) {
            out.push(ext);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_are_normalized() {
        assert_eq!(
            parse_extensions(" MP4; .MKV ,webm\n avi;;mp4 "),
            vec!["mp4", "mkv", "webm", "avi"]
        );
    }

    #[test]
    fn empty_extensions_are_dropped() {
        assert!(parse_extensions("").is_empty());
        assert!(parse_extensions(" ; , . ").is_empty());
    }

    #[test]
    fn image_struct_is_c_layout_sized() {
        // ABI 一旦变形，插件的二进制就不再兼容；用尺寸把这件事钉死在测试里。
        //
        // QlrImage：u32 + u32 + 指针(8) + usize(8) = 24（无填充）。
        // QlrPluginInfo：u32 + 4 字节填充 + 指针(8) + u32 + 4 字节填充 + 指针(8) + u32 + 4 字节填充 = 40。
        // 这两个数字是 x64 上的；换平台（理论上没有 32 位版本）会立刻失败——正是想要的效果。
        assert_eq!(core::mem::size_of::<QlrImage>(), 24);
        assert_eq!(core::mem::size_of::<QlrPluginInfo>(), 40);
    }

    #[test]
    fn symbol_names_are_nul_terminated() {
        for sym in [SYM_INFO, SYM_RENDER, SYM_FREE] {
            assert_eq!(sym.last(), Some(&0));
        }
    }
}
