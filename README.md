# QLRust

**用 Rust 原生重写quicklook，单进程、无外部运行时依赖、主程序 2.3 MB。**


## 支持的格式

 [图片](docs/screenshots/preview-image.png) |
 [PDF](docs/screenshots/preview-pdf.png) |
 [文本](docs/screenshots/preview-text.png) |
 [源码](docs/screenshots/preview-code.png) |
 [Markdown](docs/screenshots/preview-markdown.png) |
 [表格（xlsx）](docs/screenshots/preview-sheet.png) |
 Office（docx / pptx） |
 [zip](docs/screenshots/preview-archive.png) |
 [音频](docs/screenshots/preview-audio.png) |
 [视频](docs/screenshots/preview-video.png) |
 未完待续

## 与 QuickLook 原版的对比

上游 [QuickLook](https://github.com/QL-Win/QuickLook)（C# / WPF，4.5.0）。下表是同一台机器（i5-12400F / 16 GB / Win10 22H2）、同一批 18 个夹具、同一测量口径的实测：

| 指标 | QLRust 1.8.3-fix | QuickLook 4.5.0 |
| --- | ---: | ---: |
| 冷启动 · 进程 → 窗口出现（3 轮中位） | **31 ms**（94 / 31 / 30） | 1085 ms（1085 / 1091 / 1063） |
| 冷启动 · 进程 → 画面稳定/就绪 | **90 ms**（160 / 90 / 75） | 1110 ms（1131 / 1110 / 1082） |
| 预览窗口出现（18 夹具均值 / 中位） | **58 ms / 60 ms** | 190 ms / 147 ms |
| 首帧稳定（收敛样本均值） | **241 ms（16 / 18 收敛）** | 358 ms（仅 4 / 18 收敛，文本类带光标闪烁不可比） |
| 热切换 · 换文件 → 画面开始变 | 31–41 ms¹ | 13–67 ms¹ |
| 热切换 · 换文件 → 新画面稳定 | **64–70 ms**¹ | 159–360 ms¹ |
| 看一张 1280×720 图的内存 | **22.2 MB** | 163.1 MB |
| 看文件期间内存（18 夹具均值） | **37.5 MB（13.6–103.6）** | 722.4 MB（553–990） |
| 空闲内存 / 句柄 / 线程 | **9.95 MB / 208 / 7**¹ | 95.7 MB / 500 / 16¹ |
| 分发包 / 主程序 | **5.46 MB / 2.29 MB 单 exe** | 约 278 MB / 托管程序集 |
| 能出窗的夹具 | **17 / 18** | 14 / 18 |
| 支持的扩展名 | 123 | **416（29 种语言）** |
| 字体预览 | 不支持 | **支持（DirectWrite 样本页）** |
| 视频声音 | 无 | **有** |
| 表格 xlsx（本机实测） | **内置原生渲染，不需要装 Office**（23,167 格的真实表：解析 17.6 ms、首帧 23 ms、宿主 +6 MB） | 未装 `OfficeViewer-Native` → 打不开 |
| Office 文档 docx / pptx | **内置接管系统预览处理器**（需本机装 Office） | 未装 `OfficeViewer-Native` → 打不开 |
| 压缩包 | **内置目录树，129 ms** | 插件列表，488 ms |

完整数据、逐夹具明细与测量口径：[docs/dev/COMPARE-live-1.8.3-fix.md](docs/dev/COMPARE-live-1.8.3-fix.md)（上一版：[COMPARE-live-1.8.2.md](docs/dev/COMPARE-live-1.8.2.md)）。

> 2026-11-04 复测：冷启动、预览窗口出现、首帧稳定、看文件内存、1280×720 内存均为 1.8.3-fix 实测（同一台机器、18 夹具、8 s 超时）。
> 标 ¹ 的「热切换 / 空闲资源」沿用 1.8.2–1.8.3 口径，未随本次复测；「18 夹具内存均值」比 1.8.3 略高（34.5→37.5 MB）是因为本轮把 Office 三件也走系统预览处理器一起量了，不是回退。
> 原版的「首帧稳定」对文本/PDF/HTML/Office 类不可比（WPF 光标闪烁、WebView2 动画导致 8 s 内拿不到 3 帧一致），故主口径用「窗口出现 + 内存 + 覆盖」。

## 安装

**绿色版（推荐）**：从 [Releases](https://github.com/Lovepeace-art/QuickLook-R/releases) 下载 `qlrust-1.8.3-win64.zip`，解压到任意目录，双击 `qlrust.exe` 即可。目录里有 `portable.lock` 时数据（配置/日志）写在同目录 `UserData\`，不碰注册表。

**安装版**（免管理员，装到 `%LOCALAPPDATA%\Programs\QLRust` 并注册"打开方式"）：

```powershell
pwsh -NoProfile -File scripts\install.ps1              # 安装 + 注册文件关联
pwsh -NoProfile -File scripts\install.ps1 -Autostart   # 附带开机自启
pwsh -NoProfile -File "%LOCALAPPDATA%\Programs\QLRust\uninstall.ps1"   # 卸载（含关联、注册表、开始菜单、数据目录）
```

首次运行会有 SmartScreen 提示（无代码签名证书），选"更多信息 → 仍要运行"即可。

**系统要求**：Windows 10 / Windows 11，x64。表格（xlsx/xlsm/xltx/xltm）**不需要装 Office**（内置原生渲染，见 [ADR-0011](docs/adr/0011-native-sheet-preview.md)）；docx / pptx 预览仍走系统 `IPreviewHandler`，需要本机装了 Microsoft Office；PDF 需要随包提供的 `pdfium.dll`。

## 致谢

- **[QuickLook](https://github.com/QL-Win/QuickLook)**（Paddy Xu）：这个交互范式（空格预览、跟在资源管理器选中项后面、插件化）与大量行为契约都来自它；本项目的快捷键语义、便携目录布局、插件模型、系统预览处理器接入方式都参考了它。本项目是**独立重写**，代码不拷贝。
- **[PDFium](https://pdfium.googlesource.com/pdfium/)**（BSD-3-Clause）：PDF 光栅化，运行时动态加载，本仓库不分发该 DLL。
- **[windows-rs](https://github.com/microsoft/windows-rs)**（MIT OR Apache-2.0）：Win32 / COM / Direct2D / Media Foundation 绑定。
- **Rust 社区**：本项目零第三方 GUI/运行时依赖，全部靠 std + `windows-rs` 完成，受益于社区里关于 GDI/Direct2D/Media Foundation 的经验帖。
- Windows 平台上那些"没有好文档"的部分（`IPreviewHandler` 的输入同步呼叫限制、`CreateSvgDocument` 的能力边界、MCI 的路径怪癖）都是靠实测 + 踩坑记录磨出来的，清单见 [CONVENTIONS](docs/dev/CONVENTIONS.md)。

## 许可

**GPL-3.0-or-later**，全文见 [LICENSE](LICENSE)，来源与第三方组件说明见 [NOTICE.md](NOTICE.md)。

与上游 QuickLook 同许可：本项目为**独立重写**（代码不从上游拷贝），但行为契约（快捷键、便携目录布局、插件语义、系统预览处理器的接入方式）大量参考上游，按衍生作品对待。
