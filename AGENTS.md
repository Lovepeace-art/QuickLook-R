# AGENTS.md — QLRust 协作约定

> 给在本仓工作的 AI / 人类协作者。Rust 核心在 `crates/`（一个 cargo workspace，7 个 crate），
> 产物只有一个 exe + 若干插件 dll，无外部运行时依赖。
>
> **这份文档写"必须怎么做"和"为什么"。** 遇到症状先查第 13 节速查表，
> 再查 `docs/dev/CONVENTIONS.md`（47 条编号坑，下文用 `#NNN` 引用）。

## 0. 十分钟上手

按顺序读，别跳：

1. 本文档（规则 + 地图）
2. `docs/dev/HANDOFF.md` 末尾「接力说明」——当前状态、回归命令、下一步
3. `docs/dev/ARCH.md`——进程/线程/窗口/渲染通道模型
4. `docs/dev/CONVENTIONS.md`——Windows FFI 与绘制坑清单（每次踩坑都往里加）
5. `docs/dev/STATUS.md` §最新小节 + `docs/dev/LOG.md` 最近 2–3 条——做到哪了、上个版本改了什么
6. 需要"为什么这么设计"时查 `docs/adr/`

改代码前先跑一遍第 8 节的回归命令，**确认基线是绿的**再动手。

## 1. 仓库地图

| 位置 | 内容 | 文档 |
|---|---|---|
| `crates/qlr-app/` | 可执行程序：窗口过程、状态机、四类绘制通道、CLI、托盘、单实例 | 本文档 + `docs/dev/ARCH.md` |
| `crates/qlr-core/` | 纯逻辑：格式注册表、配置、日志、IPC 常量、路径、错误、插件清单 | 本文档 |
| `crates/qlr-platform/` | 所有 Win32 依赖：窗口/绘制、热键、选中项、系统预览处理器、音频、托盘、DPI、完整性、崩溃处理 | 本文档 |
| `crates/qlr-render/` | 解码与排版：位图、PDF（PDFium）、文本/代码、Markdown、压缩包、视频帧、SVG、HTML | 本文档 |
| `crates/qlr-plugin-api/` | 插件 ABI（纯 C 结构 + 4 个导出符号，`ABI_VERSION = 1`） | `crates/qlr-plugin-api/src/lib.rs` 顶部注释 |
| `crates/qlr-plugin-demo/` | 示例插件（同时是 ABI 的活体文档） | 同上 |
| `crates/qlr-plugin-office/` | Office 渲染插件（本机路径的另一条腿；内置处理器不可用时的兜底） | `docs/adr/0008-*.md` |
| `scripts/` | 验证/打包/安装脚本（见第 10 节） | 每个脚本头部注释 |
| `docs/dev/` | LOG / STATUS / HANDOFF / CONVENTIONS / ARCH / COMPARE-* | 第 13 节 |
| `docs/adr/` | 架构决策记录（改架构才写） | — |
| `_bench`（仓外） | 一次性探针脚本，**不进仓库** | — |

## 2. Crate 索引与硬约束

| Crate | 规模 | 职责 | 硬约束 |
|---|---|---|---|
| `qlr-app` | 6 文件 / ~8.1k 行（`app.rs` 单文件 ~7.0k） | 编排：`show_preview` / `repaint` / 窗口过程 / CLI | 只做编排，不直接写 Win32 细节（走 `qlr_platform::window`） |
| `qlr-core` | 10 文件 / ~2.4k 行 | 纯逻辑 + 纯数据 | **零 `windows` 依赖**，必须能跨平台编译与单测 |
| `qlr-platform` | 13 文件 / ~5.9k 行 | 全部 Win32 / COM / MCI / MF / D2D | 新 FFI 只能落在这里 |
| `qlr-render` | 17 文件 / ~7.4k 行 | 解码 + 排版 + 产出 BGRA | 不碰窗口句柄；输入 path/bytes，输出像素与 `RenderInfo` |
| `qlr-plugin-api` | 1 文件 / 185 行 | 插件契约 | 改动即破坏 ABI，必须升 `ABI_VERSION` 并同步 demo + office |
| `qlr-plugin-demo` | 1 文件 / 241 行 | 示例 | 保持最小可读 |
| `qlr-plugin-office` | 1 文件 / 901 行 | Office 位图渲染 | 与内置 `IPreviewHandler` 路线共存，不许抢接管顺序 |

## 3. 分层决策——新代码放哪

写 `cfg(windows)` / `extern "system"` / 平台 crate 依赖之前，先查这张表：

| 你要加的东西 | 归属 | 先例 |
|---|---|---|
| 新的 Win32 / COM / MCI / MF 调用 | `qlr-platform`（薄函数，`pub` 出去） | `window.rs::set_child_input`（`EnableWindow` 手写 FFI） |
| 新的文件格式解码或排版 | `qlr-render`（`pub fn render_*(...) -> Result<...>`） | `svg.rs` / `html.rs` / `media.rs` |
| 新的纯逻辑（路径、格式判定、配置合并、字符串处理） | `qlr-core`（**不许 `use windows::`**） | `format.rs::kind_for_path` |
| 新窗口 / 新绘制通道 / 新消息处理 | `qlr-app::app`，绘制原语仍走 `qlr_platform::window` | `paint_archive` |
| 插件能做的事（用户自定义格式） | 插件 API，不进内置 | `qlr-plugin-demo` |

**判断口诀**：能在 Linux 上单测吗？能 → `qlr-core`；要出像素但不要窗口 → `qlr-render`；要句柄 → `qlr-platform`；要串起来 → `qlr-app`。

## 4. 内置预览类型矩阵

`qlr_core::format::Kind` 是唯一真源（`format.rs:25`）：

| Kind | 触发扩展名（示例） | 渲染方 | 关键实现 |
|---|---|---|---|
| `Image` | png/jpg/gif/bmp/webp/tiff/ico | `qlr-render::image_view` → 位图通道 | 窗口比例锁素材比例（#148） |
| `Pdf` | pdf | `qlr-render::pdf`（PDFium 动态加载） | 页码框 + 翻页 |
| `Text` | txt/log/ini/csv/tsv/源码 | `qlr-render::text` + `code` | 语法着色、自动换行 |
| `Markdown` | md/markdown | `qlr-render::markdown` | 轻量排版 |
| `Svg` | svg | `qlr-render::svg`（D2D `CreateSvgDocument`，失败降级内置光栅化器） | 等比居中 |
| `Html` | html/htm | `qlr-render::html`（自写解析器，不拉 WebView2） | 当文档排版 |
| `Audio` | wav/mp3/… | `qlr-platform::audio`（MCI）+ 播放条 | 滚轮=进度 |
| `Video` | mp4/mkv/… | `qlr-render::media`（Media Foundation 取帧） | 首帧 + 进度条，无声音；窗口锁比例 |
| （系统处理器） | docx/xlsx/pptx… | `qlr-platform::shellpreview`（`IPreviewHandler`） | PPT 翻页由我们接管（`step_slide`） |
| （内置列表） | zip/cbz/jar/war | `qlr-app::archive_rows` + `window::paint_list` | 目录树 + 滚动 |
| `Unsupported` | 其余 | 插件 → 提示文字 | — |

**注册一个新 Kind 的检查清单**（漏一条就会出现"能识别但画不出/画了但窗口尺寸不对"）：

1. `crates/qlr-core/src/format.rs`：`enum Kind` 加变体 → `name()` / `label()` / `hint()` / `extensions()` / `kind_for_path()` 全同步 → 单测（扩展名映射）。
2. `natural_size()`（`qlr-render`）：需要按素材比例开窗的格式必须给宽高。
3. `qlr-app/src/app.rs`：`repaint()` 的 `Canvas::*` 分支、`width_for` / `window_size_for`、`on_wheel` 的滚轮语义。
4. `shell_candidate()`：交给系统处理器的类型**不许再画文字占位页**（#149）。
5. `scripts/verify-*.ps1`：加一条能自动断言的检查。
6. `README.md` 支持表 + `docs/dev/STATUS.md` 格式矩阵。

## 5. 硬约定（违反即复现历史 bug）

### 5.1 锁与消息泵

- **持锁不许碰窗口或 COM。** `ShowWindow` / `SetWindowPos` / `SetRect` / `SetFocus` 都会把消息泵回自己的窗口过程，重入再拿同一把 `Mutex` 就是自等死锁（#128 / #139 / #145）。
  规矩：**锁内只拷数据 → 放锁 → 再动窗口/COM**。需要跨锁用句柄时先 `take()` 出来，用完放回：
  ```rust
  let Some(mut handle) = with_state(|s| s.shell_preview.take()).flatten() else { return; };
  handle.set_rect(content);                       // 锁外调用
  with_state(|s| { s.shell_preview = Some(handle); });
  ```

### 5.2 系统预览处理器（`IPreviewHandler`）

- **不能在输入同步呼叫里调 COM**（`WM_SIZE` / `WM_LBUTTONDOWN` / `WM_MOUSEACTIVATE` 里直接调会拿到 `0x8001010D` `RPC_E_CANTCALLOUT_ININPUTSYNCCALL`）。要 `window::post(hwnd, ipc::WM_QLR_SHELLFIT)` 退栈后再调（#145）。
- `WM_MOUSEACTIVATE` 返回 **`MA_ACTIVATE`(1)**，不要返回 2：返回 2 会吃掉这次点击，用户要连点两下（#140）。
- 让 Excel/Word 能点选单元格/拖滚动条的**唯一**办法是 `IPreviewHandler::SetFocus()`，且只能在「处理器就绪后、已退栈」的 `TIMER_SHELL_INPUT` 里调一次；放在 `WM_MOUSEACTIVATE` 里会把点击吞掉（#147）。
- `EnableWindow(宿主, FALSE)` 接管输入**只给 PPT**（PPT 用我们投的 `VK_NEXT`/`VK_PRIOR` 翻页）；Excel/Word 必须让它自己收输入（#132）。
- 子窗口内容会盖住我们的绘制 → 预览窗必须带 `WS_CLIPCHILDREN`，否则翻页/重绘出现"只重画几块"（#131）。
- 改窗口大小走 **`SetRect` 重新对齐**，不要重建处理器（重建要 0.1–0.8 s，用户明确否掉）；改完必须 `sync_shadow_with(true)` 让阴影跟上（#145 / #146）。
- 交给系统处理器的类型**不要画"文件信息/正在渲染…"占位页**（#149）。

### 5.3 绘制

- 每一帧的所有绘制都走 `qlr_platform::window` 的原语（`paint_*`），**不要直接 `GetDC` 画**；离屏合成用 `PaintBuffer`。
- `DrawTextW` 不做字体回退：**不要放 emoji**（📁/📄 会变成豆腐块），标记用 `▾`/`▸` 或文字（#130）。
- 顶栏 `Chrome` 字面量在 `repaint()` 里有多处（文本/位图/列表/压缩包通道各一份），**改字段要全改**；顶栏按钮命中优先级同样写了两处。
- 裸窗口尺寸 ≠ 内容尺寸：内容高 = 客户区高 − `window::CHROME_HEIGHT`；视频再扣 `window::AUDIO_BAR_H`。算错就会走拉伸/每次缩放漂移。
- 图片/视频：**窗口比例锁素材比例**，图片铺满内容区（不是"等比居中留白"）；不允许拖边改大小，只允许滚轮缩放（#148 / #150）。

### 5.4 钩子与热键

- `WH_KEYBOARD_LL` / `WH_MOUSE_LL` 回调里**禁止** COM / 文件 IO / 加锁 / `format!` / 耗时查找；只允许判注入、读前台类名、`PostMessageW`。钩子超时 10 次后 Windows 会**静默**摘链（ADR-0005）。
- 预览窗是 `WS_EX_NOACTIVATE`，收不到物理滚轮：真滚轮由 `WH_MOUSE_LL` 旁听 → `PostMessage(ipc::WM_QLR_WHEEL)` → `on_wheel`。自动化等价入口是 CLI `--wheel N`。

### 5.5 其它天条

- `crates/qlr-core` **不许** `use windows::`。
- 日志一律 `qlr_core::log_error!/log_warn!/log_info!/log_debug!`，**不要 `println!`**（GUI 子系统无 stdout）。
- 不给 `DrawTextW` 传空切片/空字符串指针（`#1`，直接 `0xC0000005`）。
- MCI（音频）**不认带 `..` 的相对路径**，传进去前先 `std::fs::canonicalize`。
- `IMediaSourceReader::SetCurrentPosition` 的 `guidtimeformat` **不许传 NULL**（#141，`STATUS_ACCESS_VIOLATION`）。
- 门面函数（托盘/DPI/热键/选中项/亚克力）失败只允许 `log_warn!`，**不许阻止预览**；只有"注册窗口类失败"才致命。

## 6. 改代码的具体规则

- **`crates/qlr-app/src/app.rs` 只用 `edit` 工具做精确锚点替换。** 历史事故：用 PowerShell 按行号整段改写，把 `\n` 写成字面量、把注释吃掉（#127 姊妹教训）。
- 中文脚本/中文提交信息**先写文件再执行**：`pwsh -NoProfile -File x.ps1`、`git commit -F $env:TEMP\commit-msg.txt`。直接在 `-m` 里塞中文引号会被 PowerShell 拆坏。
- 改 `.ps1` 脚本时注意 PowerShell 别名冲突：函数名 `Measure` / `Cls` 会撞 `Measure-Object` / `Clear-Host`；函数里 `Write-Output "str"` 又 `return 数值` 会让调用方拿到数组。
- 新增/修改 `verify-*.ps1`：断言优先读**日志中的稳定字符串**（如 `处理器重新对齐到`、`SHELL 就绪`），其次才是像素；截图统一用 `PrintWindow`（`PW_RENDERFULLCONTENT`），**不要 `CopyFromScreen`**（#54）。
- 单测放各 crate 的 `tests` 模块或 `tests/` 目录；**真实桌面 / 真实音频设备**的用例打 `#[ignore]` 并写清跑法。
- 改窗口/绘制/输入/热键/退出路径后，必须重跑 `scripts/smoke.ps1`（端到端真进程）。

## 7. 日志规范

- 宏：`log_error!`（不可恢复）、`log_warn!`（可恢复异常）、`log_info!`（生命周期事件，**默认级别**）、`log_debug!`（诊断，开发时开）。
- 第一个参数是 **SCOPE**（模块级 `const SCOPE: &str`），不要动态拼 scope。
- 开 debug：`UserData/config.json` 里 `"log_level": "debug"`，或环境变量 `QLRUST_LOG=debug`（还有 `trace`）。
- 日志路径：便携模式 `<exe目录>\UserData\logs\app.log`，安装模式 `%LOCALAPPDATA%\QLrust\logs\app.log`；`log.path()` 可取。
- **日志里不许出现**用户文件内容、选区文本、完整用户路径（写文件名即可）；文件 4 MB 滚动。

## 8. 构建 / 测试

```powershell
$env:PATH = "D:\Ruanjian\Rust\cargo\bin;$env:PATH"   # 本机 cargo 不一定是 path 里的
cargo build --release
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

**当前基线（1.8.2）**：`cargo test` → **251 passed / 3 ignored**（app 48、core 48、platform 25+2、plugin-api 4、plugin-demo 7、plugin-office 11、render 98+1、fuzz 1、malformed 9）；fmt / clippy 零输出。

端到端脚本（都会自己造便携目录、WMI 启动真进程、PrintWindow 截图、打印 PASS/SKIP 汇总）：

| 脚本 | 期望 | 覆盖 |
|---|---|---|
| `scripts/smoke.ps1` | 全过（托盘 1 条 SKIP：管理员会话 UIPI） | 单实例、热键、窗口、翻页、缩放、退出 |
| `scripts/verify-office.ps1` | **23 PASS** | Office 处理器接管、标题、翻页边界、拖边改大小、重新对齐 |
| `scripts/verify-archive.ps1` | **11 PASS** | 压缩包目录树、折叠、滚动 |
| `scripts/verify-media.ps1` | **15 PASS** | 音频/视频/SVG/HTML 四类预览 |
| `scripts/verify-mediashape.ps1` | **13 PASS** | Excel 点选单元格、右缘无白边、图片/视频锁比例 |
| `scripts/verify-pdf.ps1` | 全过 | PDF 渲染与翻页 |
| `scripts/probe-selection.ps1` | 全过 | 选中项跟随（需要真实桌面会话） |
| `scripts/bench.ps1` / `stress.ps1` | 预算内 | 性能与压力 |

夹具：`QLRUST_FIXTURES` 环境变量指向夹具目录（默认按开发机布局回退到 `<桌面>\qlrust-dev`）；
仓库**不带**夹具，缺文件时相关检查 SKIP 而不是 FAIL。造夹具用 `scripts/make-fixtures.ps1`。

**实机联调的铁律**：

- 被测 exe **不能放在工作目录里**：那里进程令牌完整性被降级，托盘/Shell COM 会 `0x80070005`（#29）。复制到 `<桌面>\qlrust-try`。
- 用 **WMI** 启动（`Invoke-CimMethod -ClassName Win32_Process -MethodName Create`）；`Start-Process` 在 agent harness 里会挂住。
- 复制新 exe 前先 `Stop-Process` 并**轮询等进程真退出**，否则 `Copy-Item` 报占用或跑成旧二进制。
- 窗口必须先 `SetWindowPos` 挪回主屏再截图：半屏外的窗口只画可见部分，`PrintWindow` 会抓到"新一半 + 陈旧一半"的混合帧（#129）。
- 看崩溃先看事件日志：`Get-WinEvent -LogName Application | ? { $_.Message -match 'qlrust' }`；`Id 1002`（Application Hang）= **卡死被系统杀**，不是 panic，配合 `app.log` 最后一条 `wm=` 定位。

## 9. 提交纪律

- 消息格式 `type(scope): 摘要`（`feat/fix/docs/refactor/test/chore`），正文写"为什么 + 怎么验证（数字）"。例：`feat(1.8.1): Excel 点选单元格 + 图片/视频锁窗口比例 + 去掉占位页`。
- **一次提交一件事**：fmt 单独提交（`style(fmt): ...`），不要和逻辑改动混在一起。
- 提交信息**不用** `-m "多行中文"`，用 `-F $env:TEMP\commit-msg.txt`；不要加 `Co-Authored-By` / `Generated with` 之类 trailer。
- 提交前 `git status` 确认暂存区只有你改的文件；**不要 `git add -A`**（会带上探针脚本和临时产物）。
- 每个功能包完成时**必须**同时更新（否则下个接手者会踩同一个坑）：
  - `docs/dev/LOG.md`：版本条目——改了什么、为什么、怎么验证的（含实测数字）；
  - `docs/dev/STATUS.md`：顶部横幅 + 新 §小节；
  - `docs/dev/CONVENTIONS.md`：新踩的坑，编号递增；
  - `docs/dev/HANDOFF.md`：末尾「接力说明」（状态 + 回归命令 + 下一步）。

## 10. 版本与发布

- 版本号**三处同步**：`Cargo.toml`（workspace `version`）、`crates/qlr-app/resources/qlrust.rc`（`FILEVERSION` / `PRODUCTVERSION` / 字符串四份）、`README.md`。
- 产物：`cargo build --release` → `target\release\qlrust.exe`（~2.2 MB）+ `qlr_plugin_*.dll`；打包 `scripts/pack.ps1 -Version 1.8.2`；安装/卸载 `scripts/install.ps1` / `uninstall.ps1`（有 `portable.lock` 时为便携模式）。
- 分发形态跟着原版走：便携目录 + `portable.lock` + `plugins\` + `pdfium.dll`。
- GitHub 发布：仓库 `QuickLook-R`（GPL-3.0-or-later，见 `LICENSE` / `NOTICE.md`）；发布流程与已完成项见 `docs/dev/HANDOFF.md`「发布准备记录」。

## 11. 文档地图与更新义务

| 文档 | 什么时候必须改 |
|---|---|
| `docs/dev/LOG.md` | 每个版本（追加条目，含实测数字） |
| `docs/dev/STATUS.md` | 每个版本（横幅 + §小节）；里程碑表状态变化时 |
| `docs/dev/HANDOFF.md` | 每个版本末尾「接力说明」；发布/环境变化 |
| `docs/dev/CONVENTIONS.md` | 每踩一个新坑（编号递增，写"症状 → 根因 → 修法 → 数字"） |
| `docs/dev/ARCH.md` | 改进程/线程/窗口/绘制通道/模块清单时 |
| `docs/adr/NNNN-*.md` | 架构级取舍（换依赖、改进程模型、改插件契约） |
| `docs/dev/COMPARE-live-*.md` | 跑完与原版的对比测试后 |
| `README.md` | 支持表、性能表、构建/验证命令、版本号变化 |

文档里的路径**不许写死开发机绝对路径**（`C:\Users\...`），用 `<桌面>` / `<仓库>` / `<夹具目录>` 之类占位符——CI 与其他人会读到它。

## 12. 不要做的事

- 不要引入 async 运行时（`tokio` 等）：消息泵是单线程 Win32 循环，异步只会添乱。
- 不要用 `anyhow` / `thiserror` 重写错误层：`qlr_core::Error` 是手写枚举，为了零依赖 + 可跨平台。
- 不要把渲染/解码放进 UI 线程（`repaint` 只负责合成，解码在 worker 线程，见 ARCH）。
- 不要用第三方 GUI 框架（egui/iced/tao 等）替换原生窗口：预览窗必须与系统合成器、`IPreviewHandler`、托盘、钩子共存。
- 不要为了"顺手"改 ABI：`qlr-plugin-api` 的 4 个符号与 `QlrImage` 布局是外部契约。
- 不要把探针脚本/一次性实验产物放进仓库（`_bench\` 在仓外）。

## 13. 常见坑速查表

| 症状 | 根因 | 见 |
|---|---|---|
| 点一下预览窗就卡死（事件日志 Id 1002） | 持锁调窗口/COM 自等 | #128 #139 #145 |
| Excel 看得见但点不中单元格、拖不动滚动条 | 焦点只到容器 `EXCEL9`，没调 `IPreviewHandler::SetFocus` | #147 |
| Excel/Word 改大小后右边一条白 | 处理器子窗口不跟宿主，`SetRect` 没在退栈后调 | #145 |
| 改大小后阴影比窗口大一圈 | `WM_SIZE` 没 `sync_shadow_with(true)` | #146 |
| 第一次点边缘不改大小、第二次才行 | `WM_MOUSEACTIVATE` 返回 2 吃掉了点击 | #140 |
| 翻页/重绘只重画几块 | 预览窗缺 `WS_CLIPCHILDREN` | #131 |
| 图片被拉伸变形 / 窗口比例不对 | 走的是"等比居中"而不是"窗口锁比例 + 铺满" | #148 |
| 截图有"新一半 + 旧一半"混合帧 | 窗口一半在屏幕外，`PrintWindow` 只拿到可见部分 | #129 |
| 文字里出现豆腐块方框 | GDI `DrawTextW` 不做字体回退（emoji） | #130 |
| 音频打不开（MCI 失败） | 路径里带 `..` | 第 5.5 节 |
| 视频 seek 崩 `STATUS_ACCESS_VIOLATION` | `SetCurrentPosition` 的 `guidtimeformat` 传了 NULL | #141 |
| 交给系统处理器的类型还显示"正在渲染…" | `shell_candidate` 命中后没清占位 | #149 |
| 覆盖层颜色和设计稿不一样 | `COLORREF` 是 `0x00BBGGRR` 不是 RGB | #142 |
| 视频画面盖住进度条 | 走位图通道没 `reserve_bar` | #143 |
| 插件装着但预览走内置 | 接管顺序：内置优先，插件兜底 | `plugins.rs` |

## 14. 协作（多 agent / 多会话）

- 多个 agent 同仓工作时：**先读再写**，改动前用 `git status` 确认工作树是干净的；每人只管自己那一份文件，别动别人的未提交改动。
- 分工按 crate 切最省心（`qlr-render` / `qlr-platform` / `qlr-app` 互不重叠）；共享文件（`Cargo.toml`、文档）改动要显式说明。
- 结论要带数字：跑过什么命令、多少 PASS、多少 ms、多少 MB。没有数字的"验证过了"不算验证。
- 卡住超过两轮就停下来汇报：已确定的事实、正在试的方案、需要用户决策的选项（A/B/C 各带代价）。
