# 安全模型（QLRust 1.0.0）

这份文档说明「QLRust 信任什么、不信什么、被喂坏文件时会怎样」。

## 1. 输入面

只有两类外部输入：**被预览的文件** 和 **插件 DLL**。没有网络功能，不解析远程内容，不执行被预览文件里的任何脚本（Markdown 不渲染 HTML，PDF 不跑 JavaScript）。

| 输入 | 处理者 | 防护 |
|---|---|---|
| 图片（png/jpeg/gif/bmp/tiff/webp/ico） | `image` crate | 先读文件头算尺寸，超过像素预算直接拒绝，不分配内存 |
| PDF | PDFium（可选 `pdfium.dll`） | 每次渲染前把页尺寸限制在预算内；`FPDF_LoadMemDocument64` 的缓冲区由宿主持有到文档关闭 |
| 文本/Markdown | 自己写的解码器 | 只读前 8 MB；单行最多 2000 字符；总行数上限 20 万 |
| 插件 | 进程内 `libloading` | ABI 版本严格相等、名字与扩展名长度上限 4096 B、返回位图 `len` 必须等于 `w*h*4`、像素数上限 64 MP |

## 2. 威胁模型

- **不受信任的文件**：必须不崩溃、不越界、不无限吃内存。见下节 fuzz 回归。
- **插件被完全信任**：插件是原生 DLL，跑在宿主进程内，与宿主同权限，能读写任意文件、任意内存。这是刻意的取舍（进程内零 IPC 开销，见 ADR-0008）。**只安装来源可信的插件**；宿主不做沙箱、不做签名校验。
- **不涉及**：提权（不请求 UAC，全部 per-user，装在 `%LOCALAPPDATA%`）、驱动、内核、服务。

## 3. 已做的回归

`crates/qlr-render/tests/malformed.rs`（7 条，随 `cargo test` 跑）：

| 测试 | 覆盖 |
|---|---|
| `empty_files_do_not_panic` | 0 字节 png/jpg/pdf 必须报错；0 字节 txt/md 合法（返回空画布） |
| `truncated_images_report_errors` | 截半 PNG、只有魔数的 JPEG、头里声称 60000×60000 的超预算 PNG |
| `broken_pdf_reports_error_without_crashing` | 纯垃圾字节、只有头的 PDF、0 页 PDF |
| `binary_and_odd_text_are_handled` | 含 NUL 报错、非法 UTF-8 有损解码、10 MB 单行被截断 |
| `pathological_markdown_is_bounded` | 未闭合代码围栏、2000 行病态标记；断言 `styles.len() == lines.len()` |
| `fuzz_mutated_samples_never_panic` | xorshift64 确定性变异，3 个种子 × 106 例 = **318 例**，零 panic |
| `unsupported_kind_never_reaches_a_parser` | `.mp4` 之类绝不进解析器，只出插件提示 |

变异 fuzz 固定种子，因此可复现、可在 CI 里重复跑；它不是替代真正 fuzz（LibFuzzer/AFL 跑数十小时）的方案，而是**每次提交流水线上的守门员**。

## 4. 资源上限（硬编码，不给文件决定权）

| 上限 | 值 | 位置 |
|---|---|---|
| 文本读取 | 8 MB | `qlr-render/src/text.rs` `READ_LIMIT` |
| 文本行数 | 200,000 | `text.rs` `MAX_LINES` |
| 单行字符 | 2,000 | `text.rs` `MAX_LINE_CHARS` |
| 折行后每源行最多 | 400 行 | `qlr-render/src/wrap.rs` `MAX_ROWS_PER_LINE` |
| 位图像素 | 由窗口尺寸与屏幕 DPI 推导 | `qlr-app/src/app.rs` 渲染请求 |
| 插件位图 | 64 MP | `qlr-plugin-api` `MAX_PIXELS` |
| 插件字符串 | 4,096 B | `qlr-core/src/plugins.rs` `read_str` |

## 5. 路径与进程

- 路径只在本地使用：预览目标来自资源管理器选中项或命令行参数，转交常驻实例前会**绝对化**（否则相对路径会按常驻实例的工作目录解析，历史上出过一次「打不开文件」）。
- 没有路径遍历面（不解压压缩包、不按文件名拼输出路径）。
- 单实例通过 `WM_COPYDATA` 转发请求；处理该消息期间**禁止调用 COM**（会触发 `RPC_E_CANTCALLOUT_ININPUTSYNCCALL`），改为排队后自投消息再处理。
- 低完整性（Low IL）环境下运行会打警告并禁用托盘等特权操作——这是给沙箱化调用方准备的降级路径，不是提权路径。

## 6. 报告问题

发现崩溃或可疑行为，请附：被预览文件（或能复现的最小样本）、`%LOCALAPPDATA%\QLRust\logs\app.log`、Windows 版本。日志里没有文件内容本身，只有路径、尺寸与耗时。
