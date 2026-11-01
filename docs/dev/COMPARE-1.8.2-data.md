# QLRust 1.8.2 与 QuickLook 4.5.0 实测数据

本文件只记录测量结果与测量口径。所有数字都是本机一次实测的原始值（含每轮样本），不含推断与评价。

## 环境与口径

- 机器：i5-12400F / 16 GB / Windows 10 22H2 19045 / 单屏 1920×1080 / 管理员会话。
- QLRust 1.8.2：`C:\Users\Administrator\Desktop\qlrust-try\qlrust.exe`（便携目录，`plugins\` + `pdfium.dll`）；FileVersion 1.8.2.0。
- QuickLook 4.5.0：`D:\Xin\QuickLook\QuickLook.exe`，单进程常驻；本机未安装 `OfficeViewer-Native` 插件。
- 夹具：`qlrust-dev\compare\` 下 18 个文件（图片/PDF/文本/源码/Markdown/CSV/Office/压缩包/音频/视频/SVG/HTML/字体）。
- 触发方式（两边都不注入按键）：QLRust 走 CLI/IPC `qlrust.exe --show <path>`；QuickLook 走命名管道 `\\.\pipe\QuickLook.App.Pipe.<SID>`，消息 `QuickLook.App.PipeMessages.Invoke|<path>|`（切换用 `Switch`）。
- 判定方式：`EnumWindows` 按窗口类找预览窗（QLRust `QLRust.PreviewWindow`；QuickLook `HwndWrapper[QuickLook.exe…]` 且标题以 `QuickLook - ` 开头）。画面稳定＝`PrintWindow(PW_RENDERFULLCONTENT)` 或屏幕区域哈希连续两次相同。内存＝该工具全部进程 `WorkingSet64` 之和。
- 冷启动＝测量前把进程杀干净并等待 0.9 s（QuickLook 1.2 s）；热切换＝进程已常驻且预览窗已开着另一个文件。
- 采样脚本：`_bench\coldstart-perfixture.ps1`（冷启动逐夹具 + 资源）、`_bench\switch-latency.ps1`（热切换）、`_bench\coldstart-3x.ps1`（冷启动 3 轮）、`_bench\compare-run.ps1`（18 夹具逐项）。
- 采样粒度：窗口轮询 2–5 ms；画面哈希 6–10 ms；每格数字为 2 轮（冷启动逐夹具）或 3 轮（单夹具冷启动）中位。

## 一、冷启动到首帧（进程未运行 → 窗口出现 → 画面稳定）

2 轮中位，单位 ms。

| 文件 | QLRust 出窗 | QLRust 稳定 | QuickLook 出窗 | QuickLook 就绪 |
| --- | ---: | ---: | ---: | ---: |
| img-photo.jpg（1280×720） | 63 | 130 | 1161 | 1200 |
| img-huge.png（8000×6000） | 30 | 104 | 1122 | 1160 |
| doc-demo.pdf（8 页） | 30 | 99 | 1081 | 1159 |
| text-long.txt（500 行） | 31 | 97 | 1066 | 1410 |
| markdown-demo.md | 31 | 110 | 1068 | 1146 |
| archive-demo.zip | 30 | 110 | 1072 | 1102 |
| office-demo.docx | 32 | 112 | 1052 | —（无 Office 插件，窗口无内容） |

单夹具 3 轮（img-photo.jpg，`_bench\coldstart-3x.ps1`）：

| 指标 | QLRust 1.8.2 | QuickLook 4.5.0 |
| --- | ---: | ---: |
| 窗口出现（3 轮原始值） | 62 / 45 / 32 ms | 1134 / 1174 / 1190 ms |
| 画面稳定 / 内容就绪（3 轮原始值） | 128 / 104 / 98 ms | 1184 / 1222 / 1225 ms |
| 看该文件时内存 | 13.6 / 13.7 / 13.6 MB | 163.5 / 163.8 / 163.7 MB |

QLRust 内部里程碑（同一轮日志）：进程启动 → 窗口创建 8 ms，托盘/热键就绪 11 ms，首帧渲染 2–30 ms。
外部测到的约 30–60 ms 里含 WMI 启动开销约 15–25 ms 与 2–5 ms 轮询粒度。

## 二、热切换（进程已常驻、预览窗已开着 → 切到另一个文件）

2 轮中位，单位 ms。「开始变」＝窗口像素首次与切换前不同；「稳定」＝新画面连续两帧相同。

| 切换 | QLRust 开始变 | QLRust 稳定 | QuickLook 开始变 | QuickLook 稳定 |
| --- | ---: | ---: | ---: | ---: |
| 图片 → PDF | 41 | 67 | 67 | 159 |
| PDF → 文本 | 39 | 64 | 60 | 未稳定（画面持续变化，8 s 内无连续两帧相同） |
| 文本 → Markdown | 40 | 66 | 13 | 360 |
| zip → 大图 | 31 | 70 | 41 | 274 |

## 三、内存与资源

| 场景 | QLRust 1.8.2 | QuickLook 4.5.0 |
| --- | ---: | ---: |
| 空闲 WorkingSet / Private | 10.0 MB / 2.3 MB | 95.7 MB / 65.7 MB |
| 空闲 句柄 / 线程 | 195 / 7 | 500 / 16 |
| 空闲 GDI / USER 对象 | 16 / 15 | 11 / 18 |
| 看 img-photo.jpg（1280×720）：WorkingSet | 13.7 MB | 96.1 MB |
| 看 img-photo.jpg：GDI / USER / 句柄 / 线程 | 21 / 16 / 199 / 7 | 11 / 18 / 500 / 16 |
| 看 img-huge.png（8000×6000） | 34.4 MB | 1172.7 MB |
| 看文件期间内存（18 夹具均值） | 34.5 MB | 1007.1 MB |
| 看文件期间内存（18 夹具最小–最大） | 13.8–97.2 MB | 858.0–1228.5 MB |

## 四、体积与 GUI 资源

| | QLRust 1.8.2 | QuickLook 4.5.0 |
| --- | ---: | ---: |
| 分发包 | 5.43 MB（`qlrust-1.8.2-win64.zip`） | 约 278 MB（安装目录 1510 个文件） |
| 主程序 | 2.23 MB（单 exe） | —（托管程序集 + 插件目录） |
| 依赖 | `pdfium.dll`（PDF）、系统 Office（Office 三件） | .NET 运行时 + WPF + 插件 |

## 五、格式与功能覆盖

| 维度 | QLRust 1.8.2 | QuickLook 4.5.0 |
| --- | --- | --- |
| 扩展名数量 | 123（11 类内置查看器 + 插件） | 416（29 种语言） |
| 内置查看器类型 | 图片、PDF、文本、源码、Markdown、CSV、Office、压缩包、音频、视频、SVG、HTML | 由插件提供，随安装包可选 |
| 字体预览（ttf/otf） | 不支持 | 支持（DirectWrite 画样本页） |
| 视频音频输出 | 画面 + 进度条；音频有播放条；视频无声音 | 画面 + 声音 |
| Office 三件（本机实测） | 走系统 `IPreviewHandler`；PPT 翻页由本程序接管、Excel 可点选单元格与拖滚动条、docx/xlsx/pptx 均能出窗 | 需安装 `OfficeViewer-Native` 插件；本机未装，docx/pptx/xlsx 均不出窗 |
| 压缩包 | 内置目录树（名称/大小/时间三列，可折叠），出窗 129 ms / 稳定 366 ms | 插件列表视图，出窗 488 ms / 稳定 677 ms |
| 插件模型 | 进程内 C ABI，3 个导出符号，DLL 放入 `plugins\` | .NET 程序集，`.qlplugin` 双击安装 |
| 便携运行 | 支持（`portable.lock`，数据写 `UserData\`） | 以安装包为主 |
| 自动更新 | 无 | 有 |
| 界面语言 | 中文 | 29 种 |

## 附录：18 夹具逐项（热切换口径）

`_bench\compare-run.ps1` 结果，单位 ms / MB；`-1` 表示该工具未出窗。

| 夹具 | QLRust 出窗 | QLRust 稳定 | QLRust 内存 | QuickLook 出窗 | QuickLook 稳定 | QuickLook 内存 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| archive-demo.zip | 129 | 366 | 13.8 | 488 | 677 | 858.0 |
| audio-demo.wav | 62 | 250 | 21.6 | 197 | 2648 | 936.3 |
| big-sheet.xlsx | 60 | 377 | 28.4 | -1 | -1 | 911.4 |
| code-demo.py | 46 | 198 | 28.1 | 178 | 347 | 904.7 |
| doc-demo.pdf | 47 | 199 | 34.1 | 240 | 410 | 913.1 |
| font-demo.ttf | -1 | -1 | 32.2 | 197 | 778 | 1203.2 |
| img-alpha.png | 62 | 199 | 33.2 | 196 | 431 | 909.7 |
| img-huge.png | 46 | 183 | 34.4 | 188 | 562 | 1172.7 |
| img-photo.jpg | 46 | 175 | 33.0 | 200 | 358 | 1159.0 |
| markdown-demo.md | 45 | 289 | 33.5 | 198 | 353 | 1228.5 |
| office-demo.docx | 59 | 397 | 33.8 | -1 | -1 | 927.5 |
| office-demo.pptx | 60 | 233 | 33.8 | -1 | -1 | 907.7 |
| office-demo.xlsx | 60 | 442 | 33.9 | -1 | -1 | 904.9 |
| page-demo.html | 57 | 204 | 32.3 | 197 | 595 | 1195.9 |
| table-demo.csv | 61 | 239 | 30.9 | 198 | 1032 | 908.3 |
| text-long.txt | 60 | 213 | 31.0 | 194 | 1359 | 911.8 |
| vector.svg | 61 | 195 | 36.2 | 195 | 338 | 1154.8 |
| video-demo.mp4 | 45 | 3490 | 97.2 | 230 | 3958 | 1020.9 |
| 均值 | 59 | 450（不含视频 260） | 34.5 | 221 | 989（不含视频 603） | 1007.1 |
| 中位 | 60 | — | — | 197 | — | — |
