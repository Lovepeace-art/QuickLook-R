# QLRust 插件开发指南（ABI v1）

> 内置只做四类格式（图片 / PDF / 文本 / Markdown）。**其余格式一律由插件接管**：
> 视频、Office、压缩包、字体、数据库、PE/ELF、CHM……你想加什么就写个 DLL 扔进 `plugins\`。
>
> 三层事实（读代码时按这个找）：
> - 契约：`crates/qlr-plugin-api/src/lib.rs`（宿主和插件共用的唯一一份定义）
> - 宿主：`crates/qlr-core/src/plugins.rs`（扫描、校验、派发、回收内存）
> - 示例：`crates/qlr-plugin-demo/src/lib.rs`（视频占位图，可直接复制当模板）

## 1. 十行上手

```rust
use qlr_plugin_api::*;

const NAME: &[u8] = "我的插件".as_bytes();
const EXTS: &[u8] = b"xyz;abc";

#[unsafe(no_mangle)]
pub extern "C" fn qlr_plugin_info() -> QlrPluginInfo {
    QlrPluginInfo {
        abi_version: ABI_VERSION,
        name: NAME.as_ptr(), name_len: NAME.len() as u32,
        extensions: EXTS.as_ptr(), extensions_len: EXTS.len() as u32,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn qlr_plugin_render(
    path: *const u8, path_len: usize, max_w: u32, max_h: u32, out: *mut QlrImage,
) -> i32 {
    let _path = unsafe { std::str::from_utf8(core::slice::from_raw_parts(path, path_len)) };
    let (w, h) = (max_w.min(800).max(1), max_h.min(600).max(1));
    let rgba = vec![0x40u8; (w * h * 4) as usize];          // 你的渲染结果
    let boxed = rgba.into_boxed_slice();
    unsafe { *out = QlrImage { width: w, height: h, pixels: Box::into_raw(boxed) as *mut u8, len: w as usize * h as usize * 4 } };
    RENDER_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn qlr_plugin_free(image: *mut QlrImage) {
    if image.is_null() { return; }
    let (ptr, len) = unsafe { ((*image).pixels, (*image).len) };
    if !ptr.is_null() && len > 0 {
        drop(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(ptr, len)) });
    }
    unsafe { *image = QlrImage::empty() };
}
```

`Cargo.toml`：

```toml
[lib]
crate-type = ["cdylib"]           # 必须是 cdylib，否则导出符号不会有 C ABI

[dependencies]
qlr-plugin-api = { path = "../qlr-plugin-api" }   # 只有这一个依赖，纯 C 布局、零传递依赖
```

编译产物 `<name>.dll` 丢进 `<qlrust.exe 所在目录>\plugins\`，重启 QLRust 即生效。

## 2. 宿主怎么用它

```
按下空格
  └─ 扩展名 → format::Kind
       ├─ 内置四类 → qlr-render（Rust 实现）
       └─ Unsupported → plugins::host().find_for_path() → 插件的 qlr_plugin_render()
                                                        └─ 拿回 RGBA → Canvas::Bitmap → 上屏
```

- **内置优先**：插件**不能**覆盖 `.png`/`.pdf`/`.txt`/`.md`，也不该试图覆盖——
  那是内置实现的地盘，插件只补空缺（见 `docs/adr/0008-plugin-api.md` 的"为什么不做覆盖"）。
- **同名扩展名冲突**：按 DLL 文件名排序，**先注册者生效**，后面的记一条日志丢弃。可预测、可复现。
- **加载失败绝不致命**：坏插件只影响它自己那几种扩展名，宿主照常启动，
  日志里有一行 `插件加载失败 …`，占位文字里也会提示"有 N 个插件加载失败"。

查看当前状态（不用开 GUI）：打开日志 `…\UserData\logs\app.log`，启动时有：

```
插件就绪 dir=D:\qlrust\plugins loaded=1 extensions=8 errors=0
插件 示例插件：视频占位图 接管 [mp4 mkv webm avi mov flv wmv m4v] 文件=D:\qlrust\plugins\qlr_plugin_demo.dll
```

预览插件接管过的文件时：

```
SHOW target=…\clip.mp4 kind=unsupported renderable=true plugin=示例插件：视频占位图 size=1190x748 elapsed=1ms
PLUGIN 示例插件：视频占位图 1190x669 bytes=3184440 elapsed=2ms (请求 1190x748)
```

## 3. 硬约定（宿主会校验，违反了就拒收）

| 项目 | 约定 | 违反后果 |
|---|---|---|
| 像素格式 | RGBA8，逐行连续，`stride = width*4`，左上角原点 | 颜色错乱（不报错，最难查） |
| `len` | 必须 **等于** `width*height*4` | 拒收，报"像素数据不合法" |
| 尺寸 | `width/height` 都 > 0 | 拒收 |
| 像素上限 | `width*height <= 64 MP`（64×1024×1024） | 拒收，报"图太大" |
| 内存归属 | `render` 分配 / `free` 释放，**宿主只读不 free** | 泄漏或崩溃 |
| `free` 一定被调 | 渲染失败、数据非法、宿主退出都会调；失败路径上指针可能是 null | 你的 `free` 必须容忍 null |
| 线程 | `render` 跑在宿主的**渲染线程**，不是 UI 线程 | 阻塞会卡住下一次预览，但不会卡死 UI |
| 字符串 | UTF-8，`(ptr, len)`，**无结尾 NUL**，`info` 的必须 `'static` | 拒收或乱码 |
| ABI 版本 | 必须**严格等于** `ABI_VERSION` | 跳过并记日志（不做兼容层） |

## 4. 性能建议（这些是实测出来的，不是理论）

- **别在 `render` 里读整份文件**：宿主给的 `max_width/max_height` 是"你能塞进窗口的尺寸"，
  解码到比这更大的位图纯属浪费。视频首帧、PDF 首页都是这个道理。
- **尊重 `max_*`**：宿主会按你返回的尺寸直接上屏；给一张 8K 图，用户看到的是 40 ms 的抖动。
- **慢操作自己做缓存**：宿主每次预览都会调 `render`，但它**不会**帮你缓存。
  一个 `HashMap<PathBuf, Arc<Vec<u8>>>` 就能把二次打开的延迟压到接近 0。
- **别开线程池**：宿主已经是"UI 线程 + 1 个渲染线程"，你再加线程池只会抢 CPU。
  需要异步就起**一个**工作线程，`render` 里等结果，别把句柄抛回宿主。
- **panic = 崩溃 = 宿主退出**：Rust 插件里的 `panic!` 会穿过 `extern "C"` 边界变成 abort，
  把宿主一起带走。所有可能失败的地方都要 `Result`，别 `unwrap` 外部数据。

## 5. 调试

1. **先单独跑单元测试**：把渲染逻辑写成 `fn render_image(path: &[u8], w: u32, h: u32) -> (u32, u32, Vec<u8>)`，
   ABI 那三个函数只做"搬运"。示例插件就是这么写的（6 条测试全是纯函数）。
2. **宿主日志级别调到 debug**：`UserData\config.json` 里 `"log": "debug"`，能看到 `收到请求`、`SCALE` 等。
3. **用 `QLRUST_PLUGIN_DIR` 环境变量**指向你的开发目录，不用每次拷 DLL 到 exe 旁边：
   ```powershell
   $env:QLRUST_PLUGIN_DIR = "D:\my-plugin\target\release"
   .\qlrust.exe --show D:\sample.xyz
   ```
4. **插件被跳过了？** 看日志的 `插件加载失败 …`：缺符号 → 忘了 `#[unsafe(no_mangle)]` 或
   `crate-type = ["cdylib"]`；版本不匹配 → 你引用的 `qlr-plugin-api` 和宿主版本不同。

## 6. 发布一个插件

最小分发物就是**一个 DLL**。建议：

- 文件名带平台和版本：`qlrdemo-0.1.0-win64.dll`；
- 附一个 `README.txt` 说明接管的扩展名和依赖（如果链接了 FFmpeg 之类的第三方 DLL，
  要一并放进 `plugins\` 或说明安装位置——宿主**不会**替你找依赖）；
- 不要打包成安装器：用户的 `plugins\` 目录就是唯一安装点，删文件即卸载。

## 7. 想知道能不能做某件事

| 你可能想问 | 现状 |
|---|---|
| 插件能自定义 UI（按钮/进度条）吗 | 不能。v1 只能返回一张位图，交互（缩放/平移/翻页）由宿主统一处理 |
| 插件能接管文本类格式并保留折行/滚动吗 | 不能。返回位图的插件会被当图片处理（可缩放、可平移，无滚动） |
| 插件能读宿主的配置吗 | 不能。需要配置就自己写文件，位置推荐 `%LOCALAPPDATA%\你的插件名\` |
| 插件能进程外运行（崩溃也不影响宿主）吗 | v1 是**进程内**的。进程外方案见于 `docs/adr/0008-plugin-api.md` 的"备选" |
| 能监控到"用户关闭了窗口"吗 | 不能。v1 没有生命周期回调；需要的话下次会加成 `ABI_VERSION = 2` |

---

**改这个 ABI 的规矩**：任何破坏性改动都要
① `ABI_VERSION += 1`、② 更新本文件、③ 在 `docs/dev/LOG.md` 记一条、④ 同步改示例插件。
`crates/qlr-plugin-api/src/lib.rs` 里有一条断言结构体尺寸的测试，它会先跳出来提醒你。

---

## 参考实现二：`crates/qlr-plugin-office`（1.4.0）

如果说 `qlr-plugin-demo` 演示的是"最小能跑"，这个插件演示的是**一个真实格式族**怎么接进来：

| 它做了什么 | 用到的能力 |
|---|---|
| 解析 zip（Office 文件本身就是 zip） | 插件**自带依赖**（`zip` crate），宿主不需要知道 |
| 有 `docProps/thumbnail.jpeg` 就显示缩略图 | 插件自己解码图片（`image` crate） |
| 没有缩略图就抽正文画卡片 | 插件自己用 GDI 画字（同进程，直接用系统字体） |
| 失败时返回非 0 | 宿主显示"插件渲染失败"，不影响其它格式 |

要点：
- **插件可以带自己的第三方依赖**。宿主的体积预算（A8.5）只管 `qlrust.exe`，插件 DLL 各自计。
- **失败要干净**：`render` 返回 `RENDER_FAILED` 且**不要**填 `out`；宿主会兜住。
- **别在插件里做重活**：这个插件的最坏情况（42 ms）是一次 docx 的 XML 抓取 + 画 40 行字；
  真排版不在这里做，留给"在其他应用打开"。
