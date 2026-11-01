# QLRust

**Windows 上的「按空格瞬间预览」工具：选中任意文件敲一下空格就能看，用 Rust 原生重写，单进程、无外部运行时依赖、主程序 2.2 MB。**

![Markdown 预览](docs/screenshots/preview-markdown.png)

> 当前版本 **1.8.2**（Windows 10 / 11 x64）· 内置 **11 类**查看器 · 其余格式走插件 · GPL-3.0-or-later（见 [LICENSE](LICENSE) / [NOTICE.md](NOTICE.md)）
>
> 下载：[Releases](https://github.com/Lovepeace-art/QuickLook-R/releases) · 想接手开发先读 [AGENTS.md](AGENTS.md) 与 [docs/dev/HANDOFF.md](docs/dev/HANDOFF.md)

<details>
<summary><b>用法与快捷键</b>（点开）</summary>

| 操作 | 效果 |
|---|---|
| 资源管理器/桌面选中文件 → `空格` | 打开/关闭预览（再按一次关） |
| `Esc` | 关闭预览 |
| **左键拖动** | 所有格式：预览窗口跟着鼠标移动（不重绘、不频闪） |
| **右键上下拖动** | 文本/Markdown/源码按行滚动 |
| 滚轮 | 文本/Markdown/源码/HTML 滚动；PDF 滑动（到页底继续滚 = 翻页）；图片缩放；压缩包逐行滚动；PPT 翻页（到首/末页停住并提示）；音频拖进度；视频快进/快退（一格 5 秒） |
| **按 `Shift` 移动鼠标** | 放大镜：光标处 220×220 镜头（默认 3×），松开消失 |
| **`Shift` + 滚轮** | 放大镜倍数 1.5×–8× |
| `PageDown` / `PageUp` / `↑` `↓` | 文本整屏/单行滚动、PDF 翻页 |
| **顶栏** | 左侧文件名；PDF 中间可点页码框（输入 + `Enter` 跳转）；右侧 `锁` / `↗`（用系统程序打开）/ `×`；拖顶栏空白处也能移动窗口 |
| **拖窗口边/角** | 指针变 `↔ ↕ ⤡`，松手按新尺寸重渲染。**图片与视频例外**：窗口锁死素材长宽比、不允许拖边（缩放交给滚轮） |
| **跟随选中项** | 预览开着时在资源管理器换选文件，窗口自动换内容 |
| **记住位置与尺寸** | 按格式分别记；换显示器自动夹回工作区 |
| 不支持的格式 | 不弹窗、不画占位文字，只响一声提示音 |

上面这些按键都能改：托盘右键 →「设置…」或 `qlrust.exe --settings`。

命令行（第二实例会把请求转交给常驻实例）：

```powershell
qlrust.exe                 # 常驻后台，注册全局热键
qlrust.exe .\a.jpg         # 请求预览（常驻实例未跑则自己顶上）
qlrust.exe --settings      # 打开设置窗口
qlrust.exe --hide          # 收起预览窗
qlrust.exe --quit          # 退出常驻实例
qlrust.exe --render a.pdf  # 无窗口渲染一帧（脚本/CI 用）
qlrust.exe --list-extensions out.txt   # 导出全部识别扩展名
```

</details>

## 支持的格式

每行一类，点「截图」看实际效果（截的都是本程序在 Win10 上的真实窗口）：

| 类型 | 扩展名 | 能力 | 截图 |
|---|---|---|---|
| 图片 | png jpg jpeg gif bmp webp tif tiff ico cur | 按原图尺寸 50% 开窗（设置页可改 10–400%）、滚轮缩放、左键拖动、大图按预算降采样 | [截图](docs/screenshots/preview-image.png) |
| PDF | pdf | 逐页渲染、顶栏页码框（可输入跳转）、页缓存（翻回去 0 ms，预取前后各一页）、滚轮滑动/到边翻页、缩小态超采样 | [截图](docs/screenshots/preview-pdf.png) |
| 文本 | txt log ini toml json xml csv…（100+ 扩展名） | 编码自动识别、自动折行、行号、只画可视区、默认 880 px 阅读宽度 | [截图](docs/screenshots/preview-text.png) |
| 源码 | rs go py js ts c cpp java cs php rb lua sh ps1 sql…（70+） | 仿 VSCode：等宽 + 行号 + 关键字/字符串/注释着色，不折行 | [截图](docs/screenshots/preview-code.png) |
| Markdown | md markdown mdown mkd | 标题分级、代码块、引用、列表、任务框、表格；左侧目录点击跳转 | [截图](docs/screenshots/preview-markdown.png) |
| Office | docx xlsx pptx pptm potx ppsx odt ods odp | 走**系统预览处理器**渲染真排版（装了 Office 才生效）：PPT 翻页由本程序接管（到首/末页停住提示，不掉黑屏），Excel/Word 点一下就能拖滚动条、点选单元格 | [docx](docs/screenshots/preview-office.png) · [xlsx](docs/screenshots/preview-office-xlsx.png) |
| 压缩包 | zip cbz jar war | 内置**可展开目录树**：目录在前按名排序，带大小/时间三列，点行折叠展开、滚轮滚动 | [截图](docs/screenshots/preview-archive.png) |
| 音频 | wav mp3 m4a aac flac ogg wma | 内置播放条：时长、进度、播放/暂停，滚轮拖进度（MCI） | [截图](docs/screenshots/preview-audio.png) |
| 视频 | mp4 m4v mov mkv wmv avi webm | 内置解码出画面 + 进度条，滚轮快进/快退（Media Foundation）；**有画面、暂不出声** | [截图](docs/screenshots/preview-video.png) |
| SVG | svg | Direct2D `CreateSvgDocument` 矢量光栅化，随窗口重绘不糊；拿不到 D2D 时退回内置极简光栅化器 | [截图](docs/screenshots/preview-svg.png) |
| HTML | html htm xhtml | 当**文档**排版（自写极简解析器：标题/段落/列表/引用/表格/代码块），不拉 WebView2、不执行脚本 | [截图](docs/screenshots/preview-html.png) |
| 放大镜 | 任意格式 | `Shift` + 移动鼠标：光标处 220×220 镜头，`Shift`+滚轮调倍数 | [截图](docs/screenshots/preview-lens.png) |
| 其它 | 任意扩展名 | 有插件则交给插件；没有插件只响一声提示音（插件怎么写见 [docs/dev/PLUGINS.md](docs/dev/PLUGINS.md)） | — |

没写扩展名的文件会**嗅探文件头**（PDF/位图魔数/无 NUL 文本）再决定用哪个查看器。

## 与 QuickLook 原版的对比

上游 [QuickLook](https://github.com/QL-Win/QuickLook)（C# / WPF，4.5.0）是这类工具的事实标准。下面全部是**同一台机器、同一批夹具、同一个测量口径**的实测（18 个夹具，脚本 `_bench/compare-run.ps1`，明细见 [docs/dev/COMPARE-live-1.8.2.md](docs/dev/COMPARE-live-1.8.2.md)）。

### 一、速度与内存：QLRust 全面领先

| 指标（18 个夹具） | QLRust 1.8.2 | QuickLook 4.5.0 |
|---|---|---|
| 窗口出现（均值 / 中位） | **59 ms / 60 ms** | 221 ms / 197 ms |
| 首帧稳定（均值，不含视频） | **260 ms** | 603 ms |
| 看文件期间内存（均值） | **34.5 MB** | 1007.1 MB |
| 空闲内存 | **约 23 MB** | 253.8 MB |
| 冷启动 | **约 100 ms** | 1.0–1.5 s |
| 分发包体积 | **5.4 MB** | 约 278 MB |
| 主程序 | 2.2 MB 单 exe | 数十个托管程序集 |

差距是结构性的：WPF 视觉树 + 二十多个托管插件程序集 vs 原生窗口 + 按需加载。原版每开一个预览就拉起一套 WPF 控件，切几个文件后停到 0.9–1.2 GB；我们全程 13.8–97.2 MB（最大是视频解码缓冲）。

### 二、QLRust 比原版强的地方

| 维度 | QLRust | 原版 QuickLook 4.5.0 |
|---|---|---|
| Office 三件 | 内置接管系统预览处理器：**PPT 翻页由我们控制**（到末页停住给提示，不掉进"放映结束"黑屏）、**Excel 点一下就选中单元格**、滚动条/滚动轮可用 | 靠可选装的 `OfficeViewer-Native` 插件；本机未装，docx/pptx/xlsx 与 400 行表格都打不开 |
| 压缩包 | 内置目录树（可折叠、三列、滚轮），开图 **129 ms / 366 ms** | 走插件列表，**488 ms / 677 ms** |
| 音频 | 内置播放条，**62 ms / 250 ms** | WPF 波形控件，197 ms / 2648 ms |
| 视频 | 内置 MF 解码出画面 + 进度条，97 MB | 能播能出声，1021 MB |
| 文本/代码/CSV | 45–61 ms 出窗，198–289 ms 稳定 | 178–198 ms 出窗，347–1359 ms 稳定 |
| 插件模型 | 进程内 C ABI，三个导出符号，一个 DLL 丢进 `plugins\` 即可 | .NET 程序集 + `.qlplugin` 双击安装 |
| 便携性 | 解压即用（`portable.lock`），数据留 `UserData\` | 安装包为主 |
| 体积 | 5.4 MB | 278 MB |

### 三、原版比 QLRust 强的地方（不藏着）

| 维度 | 原版 QuickLook | QLRust 现状 |
|---|---|---|
| 格式广度 | **416 个扩展名 / 29 种语言**：字体、CHM、PE/ELF、SQLite、3D 模型、Office 全套、归档、邮件等 | **123 个扩展名**，11 类内置查看器 + 插件 |
| 字体预览 | 用 DirectWrite 画字体样本页 | **不支持**（ttf/otf 归到「不支持」，这是已知最大缺口） |
| 视频声音 | 音视频同步播放 | 视频**只有画面**（MCI 播不了 mp4，需接 EVR/WASAPI） |
| 界面上限 | 插件生态成熟、可换主题、自动更新、`.qlplugin` 双击安装 | 无自动更新、界面固定（托盘设置页可调热键/渲染比例/预渲染） |
| 多语言 | 界面 29 种语言 | 仅中文 |
| 生态 | GitHub 4k+ star、多年打磨 | 1.8.2 首个公开版本 |

### 四、一句话结论

**同样一件事（敲空格看一眼），QLRust 快 3–4 倍、省 30 倍内存、体积小 50 倍；原版支持的格式更多、生态更成熟。**我们优先做的是「最常看的 11 类」，冷门格式与字体预览留作后续。

## 安装

**绿色版（推荐）**：从 [Releases](https://github.com/Lovepeace-art/QuickLook-R/releases) 下载 `qlrust-1.8.2-win64.zip`，解压到任意目录，双击 `qlrust.exe` 即可。目录里有 `portable.lock` 时数据（配置/日志）写在同目录 `UserData\`，不碰注册表。

**安装版**（免管理员，装到 `%LOCALAPPDATA%\Programs\QLRust` 并注册"打开方式"）：

```powershell
pwsh -NoProfile -File scripts\install.ps1              # 安装 + 注册文件关联
pwsh -NoProfile -File scripts\install.ps1 -Autostart   # 附带开机自启
pwsh -NoProfile -File "%LOCALAPPDATA%\Programs\QLRust\uninstall.ps1"   # 卸载（含关联、注册表、开始菜单、数据目录）
```

首次运行会有 SmartScreen 提示（无代码签名证书），选"更多信息 → 仍要运行"即可。

**系统要求**：Windows 10 1809+ / Windows 11，x64。Office 三件预览需要本机装了 Microsoft Office（走系统 `IPreviewHandler`）；PDF 需要随包提供的 `pdfium.dll`。

## 文档说明

| 文档 | 内容 |
|---|---|
| [AGENTS.md](AGENTS.md) | 给 AI / 人类协作者的完整约定：仓库地图、crate 索引、分层决策、硬约定、改码规则、日志、构建测试、提交纪律、坑速查表 |
| [docs/dev/HANDOFF.md](docs/dev/HANDOFF.md) | 接力说明：当前状态、回归命令、下一步（新人从这读起） |
| [docs/dev/STATUS.md](docs/dev/STATUS.md) | 做到哪了：需求矩阵、里程碑、每版小节 |
| [docs/dev/ARCH.md](docs/dev/ARCH.md) | 架构：进程/线程/窗口/四类绘制通道/模块清单 |
| [docs/dev/LOG.md](docs/dev/LOG.md) | 每版改了什么、为什么、怎么验证（带实测数字） |
| [docs/dev/CONVENTIONS.md](docs/dev/CONVENTIONS.md) | 坑清单 #1–#151：症状 → 根因 → 修法 → 数字 |
| [docs/dev/PLUGINS.md](docs/dev/PLUGINS.md) | 插件 ABI 与十分钟上手模板 |
| [docs/dev/COMPARE-live-1.8.2.md](docs/dev/COMPARE-live-1.8.2.md) | 与本机原版 QuickLook 的逐夹具对比数据 |
| [docs/adr/](docs/adr/) | 架构决策记录 0001–0008（分层、单实例、零依赖、热键、选区、渲染管线、插件 ABI） |
| [docs/dev/SECURITY.md](docs/dev/SECURITY.md) / [RELEASE.md](docs/dev/RELEASE.md) | 安全边界（无网络、插件同权限、日志不含内容）与发布流程 |

## 开发说明

环境：Rust 1.75+、Windows 10/11 x64（构建不需要 MSVC 之外的额外依赖）。

```powershell
cargo build --release
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test                                                              # 251 passed / 3 ignored
```

端到端验证（每个脚本会自建便携目录、用 WMI 起真进程、`PrintWindow` 截图断言，最后打印 PASS/SKIP 汇总）：

| 脚本 | 期望 | 覆盖 |
|---|---|---|
| `scripts/smoke.ps1` | 全过（托盘 1 条 SKIP：管理员会话 UIPI） | 单实例、热键、窗口、翻页、缩放、退出 |
| `scripts/verify-office.ps1` | 23 PASS | Office 处理器接管、标题、翻页边界、拖边改大小、改完重新对齐 |
| `scripts/verify-archive.ps1` | 11 PASS | 压缩包目录树、折叠、滚动 |
| `scripts/verify-media.ps1` | 15 PASS | 音频/视频/SVG/HTML |
| `scripts/verify-mediashape.ps1` | 13 PASS | Excel 点选单元格、右缘无白边、图片/视频锁比例 |
| `scripts/verify-pdf.ps1` / `probe-selection.ps1` | 全过 | PDF 渲染翻页、选中项跟随 |
| `scripts/bench.ps1` / `stress.ps1` | 预算内 | 性能与长稳（2000 次开关句柄零增长） |

验证脚本用 `QLRUST_FIXTURES` 指向夹具目录（缺夹具的检查会 SKIP 而不是 FAIL；夹具可用 `scripts/make-fixtures.ps1` 现造）。

代码结构：

```
crates/
  qlr-app         编排：消息泵、热键、状态机、CLI、渲染线程、压缩包列表
  qlr-platform    纯 Win32 FFI：窗口/托盘/钩子/选区/DWM/DPI/音频/系统预览处理器宿主
  qlr-render      解码与排版：image / pdf / text / code / markdown / svg / html / 折行 / 画布
  qlr-core        纯逻辑：配置、路径、日志、IPC、格式表、插件清单
  qlr-plugin-api  插件 ABI（C，零依赖）
  qlr-plugin-demo 示例插件（演示 ABI）
  qlr-plugin-office Office/压缩包的插件实现（内置已有同类能力，留作 ABI 参考）
scripts/          install / uninstall / pack / bench / stress / smoke / verify-*
docs/             dev（LOG/STATUS/ARCH/HANDOFF/CONVENTIONS…）+ adr（架构决策）
```

几条硬约束（为了体积与延迟）：不引 `pdfium-render`（会拖 tokio）而是手写 12 个 C 绑定动态加载；不引 `cosmic-text` 而是自己折行 + GDI 测量；上屏用 `StretchDIBits` 而不是 D3D11；`qlr-core` 与 `qlr-render` 不许出现 `windows::`（唯一例外是视频解码那道边界，见 `crates/qlr-render/src/media.rs` 顶部说明）。

CI：`.github/workflows/ci.yml`（Windows runner：fmt / clippy / test / release 构建 + 体积预算）、`fuzz.yml`、`bench.yml`（每周定时跑真实 GUI 基准）。

## 致谢

- **[QuickLook](https://github.com/QL-Win/QuickLook)**（Paddy Xu）：这个交互范式（空格预览、跟在资源管理器选中项后面、插件化）与大量行为契约都来自它；本项目的快捷键语义、便携目录布局、插件模型、系统预览处理器接入方式都参考了它。本项目是**独立重写**，代码不拷贝。
- **[PDFium](https://pdfium.googlesource.com/pdfium/)**（BSD-3-Clause）：PDF 光栅化，运行时动态加载，本仓库不分发该 DLL。
- **[windows-rs](https://github.com/microsoft/windows-rs)**（MIT OR Apache-2.0）：Win32 / COM / Direct2D / Media Foundation 绑定。
- **Rust 社区**：本项目零第三方 GUI/运行时依赖，全部靠 std + `windows-rs` 完成，受益于社区里关于 GDI/Direct2D/Media Foundation 的经验帖。
- Windows 平台上那些"没有好文档"的部分（`IPreviewHandler` 的输入同步呼叫限制、`CreateSvgDocument` 的能力边界、MCI 的路径怪癖）都是靠实测 + 踩坑记录磨出来的，清单见 [CONVENTIONS](docs/dev/CONVENTIONS.md)。

## 许可

**GPL-3.0-or-later**，全文见 [LICENSE](LICENSE)，来源与第三方组件说明见 [NOTICE.md](NOTICE.md)。

与上游 QuickLook 同许可：本项目为**独立重写**（代码不从上游拷贝），但行为契约（快捷键、便携目录布局、插件语义、系统预览处理器的接入方式）大量参考上游，按衍生作品对待。
