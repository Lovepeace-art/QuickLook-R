# QLRust

**用 Rust 原生重写quicklook，单进程、无外部运行时依赖、主程序 2.2 MB。**


## 支持的格式

每行一类，点「截图」看实际效果（截的都是本程序在 Win10 上的真实窗口）：

 [图片](docs/screenshots/preview-image.png) |
 [PDF](docs/screenshots/preview-pdf.png) |
 [文本](docs/screenshots/preview-text.png) |
 [源码](docs/screenshots/preview-code.png) |
 [Markdown](docs/screenshots/preview-markdown.png) |
 Office |
 [zip](docs/screenshots/preview-archive.png) |
 [音频](docs/screenshots/preview-audio.png) |
 [视频](docs/screenshots/preview-video.png) |
 未完待续

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

## 安装

**绿色版（推荐）**：从 [Releases](https://github.com/Lovepeace-art/QuickLook-R/releases) 下载 `qlrust-1.8.2-win64.zip`，解压到任意目录，双击 `qlrust.exe` 即可。目录里有 `portable.lock` 时数据（配置/日志）写在同目录 `UserData\`，不碰注册表。

**安装版**（免管理员，装到 `%LOCALAPPDATA%\Programs\QLRust` 并注册"打开方式"）：

```powershell
pwsh -NoProfile -File scripts\install.ps1              # 安装 + 注册文件关联
pwsh -NoProfile -File scripts\install.ps1 -Autostart   # 附带开机自启
pwsh -NoProfile -File "%LOCALAPPDATA%\Programs\QLRust\uninstall.ps1"   # 卸载（含关联、注册表、开始菜单、数据目录）
```

首次运行会有 SmartScreen 提示（无代码签名证书），选"更多信息 → 仍要运行"即可。

**系统要求**：Windows 10 / Windows 11，x64。Office 三件预览需要本机装了 Microsoft Office（走系统 `IPreviewHandler`）；PDF 需要随包提供的 `pdfium.dll`。

## 致谢

- **[QuickLook](https://github.com/QL-Win/QuickLook)**（Paddy Xu）：这个交互范式（空格预览、跟在资源管理器选中项后面、插件化）与大量行为契约都来自它；本项目的快捷键语义、便携目录布局、插件模型、系统预览处理器接入方式都参考了它。本项目是**独立重写**，代码不拷贝。
- **[PDFium](https://pdfium.googlesource.com/pdfium/)**（BSD-3-Clause）：PDF 光栅化，运行时动态加载，本仓库不分发该 DLL。
- **[windows-rs](https://github.com/microsoft/windows-rs)**（MIT OR Apache-2.0）：Win32 / COM / Direct2D / Media Foundation 绑定。
- **Rust 社区**：本项目零第三方 GUI/运行时依赖，全部靠 std + `windows-rs` 完成，受益于社区里关于 GDI/Direct2D/Media Foundation 的经验帖。
- Windows 平台上那些"没有好文档"的部分（`IPreviewHandler` 的输入同步呼叫限制、`CreateSvgDocument` 的能力边界、MCI 的路径怪癖）都是靠实测 + 踩坑记录磨出来的，清单见 [CONVENTIONS](docs/dev/CONVENTIONS.md)。

## 许可

**GPL-3.0-or-later**，全文见 [LICENSE](LICENSE)，来源与第三方组件说明见 [NOTICE.md](NOTICE.md)。

与上游 QuickLook 同许可：本项目为**独立重写**（代码不从上游拷贝），但行为契约（快捷键、便携目录布局、插件语义、系统预览处理器的接入方式）大量参考上游，按衍生作品对待。
