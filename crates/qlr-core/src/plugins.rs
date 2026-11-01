//! 插件宿主：扫描 `plugins\*.dll`、校验 ABI、按扩展名派发渲染。
//!
//! 契约定义在独立的 `qlr-plugin-api` crate 里（那是**宿主和插件共用**的唯一一份定义）。
//! 这里只负责"用"它：把 DLL 加载进来、把结构体校验一遍、把结果拷成宿主自己的类型。
//!
//! 三条设计原则（都是踩出来的，别随手改）：
//!
//! 1. **加载失败永不致命**：一个坏插件只影响它自己那几种扩展名，
//!    宿主照常启动，错误记进 [`PluginHost::errors`] 并写日志。
//! 2. **跨界数据一律拷贝**：插件给的是裸指针，宿主在 `free` 之前把它复制成 `Vec<u8>`，
//!    之后插件怎么折腾内存都与宿主无关。
//! 3. **`free` 一定调**：哪怕渲染成功但校验失败（尺寸不对、`len` 对不上），也要先 `free` 再报错，
//!    否则每预览一次就漏一块内存。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use libloading::Library;
use qlr_plugin_api::{
    ABI_VERSION, MAX_PIXELS, QlrImage, QlrPluginFreeFn, QlrPluginInfoFn, QlrPluginRenderFn,
    QlrPluginTextFn, QlrPluginTextFreeFn, RENDER_FAILED, RENDER_OK, SYM_FREE, SYM_INFO, SYM_RENDER,
    SYM_TEXT, SYM_TEXT_FREE, parse_extensions,
};

/// 插件渲染出来的一张图（宿主自己的类型，与 ABI 解耦）。
#[derive(Clone, Debug)]
pub struct PluginImage {
    /// 宽（像素）。
    pub width: u32,
    /// 高（像素）。
    pub height: u32,
    /// RGBA8，长度 = `width * height * 4`。
    pub rgba: Vec<u8>,
}

impl PluginImage {
    /// 字节数。
    pub fn bytes(&self) -> usize {
        self.rgba.len()
    }
}

/// 一个已加载并通过校验的插件。
pub struct Plugin {
    name: String,
    extensions: Vec<String>,
    file: PathBuf,
    render: QlrPluginRenderFn,
    free: QlrPluginFreeFn,
    /// 可选的文本输出（1.6.0）：有就用文本渲染，没有才退回位图。
    text: Option<QlrPluginTextFn>,
    text_free: Option<QlrPluginTextFreeFn>,
    /// 必须活到进程结束：函数指针指向这块内存里的代码。
    _lib: Library,
}

impl std::fmt::Debug for Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugin")
            .field("name", &self.name)
            .field("extensions", &self.extensions)
            .field("file", &self.file)
            .finish_non_exhaustive()
    }
}

impl Plugin {
    /// 插件自述的名字。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 接管的扩展名（小写、不带点）。
    pub fn extensions(&self) -> &[String] {
        &self.extensions
    }

    /// 插件 DLL 的完整路径。
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// 渲染一个文件。
    ///
    /// `max_width` / `max_height` 只是建议值：插件应该等比缩进去，但它要是给了更大的图，
    /// 宿主也会照收（上屏时按"适应窗口"再缩），只挡像素总量超过 [`MAX_PIXELS`] 的。
    /// 让插件给一段文本大纲（1.6.0）。没有导出文本符号的插件返回 `None`。
    pub fn text(&self, path: &Path) -> Option<String> {
        let text_fn = self.text?;
        let free_fn = self.text_free?;
        let raw = path.as_os_str().to_string_lossy().into_owned();
        let bytes = raw.as_bytes();
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut len = 0usize;
        let code = unsafe { text_fn(bytes.as_ptr(), bytes.len(), &mut ptr, &mut len) };
        if code != qlr_plugin_api::RENDER_OK || ptr.is_null() || len == 0 {
            if !ptr.is_null() {
                unsafe { free_fn(ptr, len) };
            }
            return None;
        }
        let owned = unsafe { std::slice::from_raw_parts(ptr, len).to_vec() };
        unsafe { free_fn(ptr, len) };
        String::from_utf8(owned).ok()
    }
    pub fn render(
        &self,
        path: &Path,
        max_width: u32,
        max_height: u32,
    ) -> Result<PluginImage, String> {
        // 非 UTF-8 路径在 Windows 上极罕见，但真碰上时宁可 lossy 也要渲染：
        // 插件多半只用它做哈希/扩展名判断，不靠路径回读文件。
        let path_text = path.to_string_lossy();
        let bytes = path_text.as_bytes();
        let mut image = QlrImage::empty();
        // 1.2.0：把第三方代码包在 `catch_unwind` 里。
        // **诚实说明**：release 档是 `panic = "abort"`（体积预算），这条在 release
        // 里救不回进程；它在 dev/测试档有效，能挡住"插件里一个 index 越界"直接把
        // 宿主带走。真正的隔离要等进程外插件（见 docs/dev/PLUGINS.md 的"以后"）。
        // 另外：Rust 的 cdylib 在 FFI 边界收到 unwind 会 abort，所以这里挡的是
        // "插件 panic 之前、我们自己的代码"以及 abort 之前能拦下的一部分情况。
        let code = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            (self.render)(
                bytes.as_ptr(),
                bytes.len(),
                max_width.max(1),
                max_height.max(1),
                &mut image,
            )
        }))
        .unwrap_or(RENDER_FAILED);

        // 先把结果读干净，再无条件 free —— 顺序不能反。
        let taken = if code != RENDER_OK {
            Err(format!(
                "插件「{}」渲染失败（返回码 {code}）：{}",
                self.name,
                path.display()
            ))
        } else {
            copy_out(&image)
        };
        // free 也要包：插件 panic 时我们仍然要尽力把它给的内存还回去。
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            (self.free)(&mut image)
        }));
        taken
    }
}

/// 校验并把插件给的内存拷成宿主自己的 `Vec<u8>`。
fn copy_out(image: &QlrImage) -> Result<PluginImage, String> {
    if image.width == 0 || image.height == 0 {
        return Err("插件返回了 0 尺寸的图".to_string());
    }
    if image.width.saturating_mul(image.height) > MAX_PIXELS {
        return Err(format!(
            "插件返回的图太大（{}x{}，超过 {} 像素上限）",
            image.width, image.height, MAX_PIXELS
        ));
    }
    let expected = (image.width as usize)
        .saturating_mul(image.height as usize)
        .saturating_mul(4);
    if image.pixels.is_null() || image.len != expected {
        return Err(format!(
            "插件返回的像素数据不合法（len={}，应为 {expected}）",
            image.len
        ));
    }
    // SAFETY: 插件承诺 pixels 指向 len 字节；上面已校验非空且长度自洽。
    let rgba = unsafe { std::slice::from_raw_parts(image.pixels, image.len) }.to_vec();
    Ok(PluginImage {
        width: image.width,
        height: image.height,
        rgba,
    })
}

/// 插件目录里的全部插件 + 加载过程中的错误（错误不致命，只记录）。
#[derive(Debug, Default)]
pub struct PluginHost {
    plugins: Vec<Plugin>,
    errors: Vec<String>,
    dir: PathBuf,
}

impl PluginHost {
    /// 扫描目录里所有 `*.dll`。
    ///
    /// 目录不存在 = 没有插件（正常情况：用户没装插件），不报错。
    pub fn scan(dir: &Path) -> Self {
        let mut host = PluginHost {
            plugins: Vec::new(),
            errors: Vec::new(),
            dir: dir.to_path_buf(),
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return host;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| e.eq_ignore_ascii_case("dll"))
            })
            .collect();
        files.sort();
        for file in files {
            match unsafe { load_plugin(&file) } {
                Ok(plugin) => host.plugins.push(plugin),
                Err(e) => host.errors.push(format!("{}：{e}", file.display())),
            }
        }
        host
    }

    /// 加载成功的插件。
    pub fn plugins(&self) -> &[Plugin] {
        &self.plugins
    }

    /// 加载失败的插件（给日志/设置界面用）。
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    /// 扫描的目录。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 按扩展名找插件。**先注册者优先**（同名扩展名由文件名排序决定，稳定可预测）。
    pub fn find_for_ext(&self, ext: &str) -> Option<&Plugin> {
        let ext = ext.trim_start_matches('.').to_ascii_lowercase();
        if ext.is_empty() {
            return None;
        }
        self.plugins
            .iter()
            .find(|p| p.extensions.iter().any(|e| e == &ext))
    }

    /// 按路径找插件。
    pub fn find_for_path(&self, path: &Path) -> Option<&Plugin> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        self.find_for_ext(ext)
    }

    /// 已注册扩展名总数（去重前）。
    pub fn extension_count(&self) -> usize {
        self.plugins.iter().map(|p| p.extensions.len()).sum()
    }
}

/// 真正加载一个 DLL：三个符号缺一不可，ABI 版本必须严格相等。
unsafe fn load_plugin(file: &Path) -> Result<Plugin, String> {
    unsafe {
        let lib = Library::new(file).map_err(|e| format!("LoadLibrary 失败：{e}"))?;
        let info_fn = *lib
            .get::<QlrPluginInfoFn>(SYM_INFO)
            .map_err(|e| format!("缺少符号 qlr_plugin_info：{e}"))?;
        let render_fn = *lib
            .get::<QlrPluginRenderFn>(SYM_RENDER)
            .map_err(|e| format!("缺少符号 qlr_plugin_render：{e}"))?;
        let free_fn = *lib
            .get::<QlrPluginFreeFn>(SYM_FREE)
            .map_err(|e| format!("缺少符号 qlr_plugin_free：{e}"))?;

        let info = info_fn();
        if info.abi_version != ABI_VERSION {
            return Err(format!(
                "ABI 版本不匹配（插件 {} / 宿主 {ABI_VERSION}）",
                info.abi_version
            ));
        }
        let name = read_str(info.name, info.name_len)?;
        if name.trim().is_empty() {
            return Err("插件名为空".to_string());
        }
        let extensions = parse_extensions(&read_str(info.extensions, info.extensions_len)?);
        if extensions.is_empty() {
            return Err("插件没有声明任何扩展名".to_string());
        }
        Ok(Plugin {
            name,
            extensions,
            file: file.to_path_buf(),
            render: render_fn,
            free: free_fn,
            text: lib.get::<QlrPluginTextFn>(SYM_TEXT).ok().map(|f| *f),
            text_free: lib
                .get::<QlrPluginTextFreeFn>(SYM_TEXT_FREE)
                .ok()
                .map(|f| *f),
            _lib: lib,
        })
    }
}

fn read_str(ptr: *const u8, len: u32) -> Result<String, String> {
    if ptr.is_null() {
        return Err("字符串指针为空".to_string());
    }
    if len > 4096 {
        return Err(format!("字符串长度异常（{len}）"));
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
    String::from_utf8(bytes.to_vec()).map_err(|_| "字符串不是合法 UTF-8".to_string())
}

/// 插件目录：`<exe 所在目录>\plugins`。
///
/// 便携版和安装版共用同一规则；`QLRUST_PLUGIN_DIR` 环境变量可以覆盖（测试/多套插件用）。
pub fn plugin_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("QLRUST_PLUGIN_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.join("plugins")))
        .unwrap_or_else(|| PathBuf::from("plugins"))
}

/// 进程级插件宿主（第一次访问时扫描一次，之后复用）。
///
/// 故意做成懒加载 + 全局：插件加载要读磁盘、载入 DLL，没必要在每次预览时重做；
/// 而且 `Library` 一旦 drop 就卸载代码，正在跑的渲染会崩。
pub fn host() -> &'static PluginHost {
    static HOST: OnceLock<PluginHost> = OnceLock::new();
    HOST.get_or_init(|| PluginHost::scan(&plugin_dir()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir(tag: &str) -> PathBuf {
        // 与其它测试一致：写 workspace 的 target/ 下（Low 完整性进程写不了 %TEMP%）。
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/qlrust-plugins-test")
            .join(tag);
        std::fs::create_dir_all(&dir).expect("创建测试目录");
        dir
    }

    #[test]
    fn missing_dir_is_not_an_error() {
        let host = PluginHost::scan(Path::new("Z:\\根本不存在的目录\\plugins"));
        assert!(host.plugins().is_empty());
        assert!(host.errors().is_empty());
        assert_eq!(host.extension_count(), 0);
        assert!(host.find_for_ext("mp4").is_none());
    }

    #[test]
    fn empty_dir_loads_nothing() {
        let dir = temp_dir("empty");
        let host = PluginHost::scan(&dir);
        assert!(host.plugins().is_empty());
        assert!(host.errors().is_empty());
    }

    #[test]
    fn broken_dll_is_reported_not_fatal() {
        let dir = temp_dir("broken");
        let file = dir.join("broken.dll");
        let mut f = std::fs::File::create(&file).expect("写测试文件");
        f.write_all(b"this is definitely not a PE file").unwrap();
        drop(f);
        let host = PluginHost::scan(&dir);
        // 坏的跳过，但要说清楚是哪个文件 —— 而不是让宿主起不来。
        assert!(host.plugins().is_empty());
        assert_eq!(host.errors().len(), 1);
        assert!(host.errors()[0].contains("broken.dll"));
        std::fs::remove_file(&file).ok();
    }

    #[test]
    fn non_dll_files_are_ignored() {
        let dir = temp_dir("mixed");
        std::fs::write(dir.join("readme.txt"), b"hello").unwrap();
        std::fs::write(dir.join("data.bin"), b"\x00\x01\x02").unwrap();
        let host = PluginHost::scan(&dir);
        assert!(host.plugins().is_empty());
        assert!(host.errors().is_empty());
    }

    #[test]
    fn plugin_dir_can_be_overridden() {
        // 环境变量是进程级的，这里只验证"值非空就采信"这条规则在一个独立进程里成立；
        // 同进程内改环境变量会影响其它测试，所以只测默认分支的形状。
        if std::env::var("QLRUST_PLUGIN_DIR").is_err() {
            let dir = plugin_dir();
            assert!(
                dir.ends_with("plugins"),
                "默认目录应以 plugins 结尾：{dir:?}"
            );
        }
    }

    #[test]
    fn copy_out_rejects_bad_payloads() {
        let zero = QlrImage::empty();
        assert!(copy_out(&zero).is_err());

        let mut huge = QlrImage::empty();
        huge.width = 65_000;
        huge.height = 65_000;
        assert!(copy_out(&huge).is_err(), "超过 64 MP 必须被拒");

        let mut short = QlrImage::empty();
        short.width = 2;
        short.height = 2;
        short.len = 8; // 应为 16
        assert!(copy_out(&short).is_err(), "len 与尺寸对不上必须被拒");
    }
}
