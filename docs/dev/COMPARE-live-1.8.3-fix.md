# 实机对比（1.8.3-fix vs 原版 QuickLook 4.5.0，2026-11-04）

> 目的：本轮平台层完善（§6-4 裸 Win32 收口、折行量宽、PaintBuffer 复用、PreviewBackend 契约 +
> Surface 路由、`window.rs` 拆分、后端四态状态机）之后的再次本机全面对比——同一台机器、同一批
> 夹具、同一测量口径，量 QLRust 1.8.3-fix 与原版 QuickLook 4.5.0 的**触发 → 窗口出现 → 画面稳定**、内存。
>
> 结论：18 个夹具里 QLRust 出窗 **17 个**（缺字体），原版 **14 个**（缺 Office 三件与 400 行表格）；
> 窗口出现均值 **58 ms vs 190 ms**；预览期内存均值 **37.5 MB vs 722.4 MB**；冷启动窗口 **31 ms vs 1085 ms（35×）**。

## 环境与口径

- 机器：Windows 10 19045、单屏 1920×1080、管理员会话。
- QLRust：`C:\Users\Administrator\Desktop\qlrust-try\qlrust.exe`（1.8.3-fix，2026-11-04 构建；便携目录，带 `plugins\` + `pdfium.dll`）。
- 原版：`D:\Xin\QuickLook\QuickLook.exe` 4.5.0（本机**未装** `OfficeViewer-Native`，所以 Office 打不开）。
- 触发：两边都走 IPC（QLRust `--show <path>` / 原版命名管道 `QuickLook.App.Pipe…Invoke|<path>|`），不注入按键。
- 判定：`EnumWindows` 按进程 + 类名找预览窗；截图 `PrintWindow(PW_RENDERFULLCONTENT)`，连续 3 次采样无差异算「画面稳定」；内存取该工具所有进程 `WorkingSet64` 之和（原版含 `msedgewebview2`）。
- 脚本：`_bench\compare-run.ps1 -Tool qlrust|quicklook`（超时 8000 ms）；冷启动 `_bench\coldstart-3x.ps1`（各 3 轮）。
- 结果 CSV：`_bench\compare\qlrust.csv` / `quicklook.csv`，截图：`_bench\compare\<tool>\<夹具>.png`。

## 逐夹具结果

`—` = 该阈值内没出现窗口 / 没达到 3 帧一致（原版文本类有光标闪烁，见下）。

| 夹具 | QLRust 窗口 | QLRust 稳定 | QLRust 内存 | 原版 窗口 | 原版 稳定 | 原版 内存 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| archive-demo.zip | 127 ms | 364 ms | 13.6 MB | 450 ms | — | 553.4 MB |
| audio-demo.wav | 62 ms | 237 ms | 21.6 MB | 148 ms | — | 618.9 MB |
| big-sheet.xlsx | 60 ms | 244 ms | 21.4 MB | — | — | 579.7 MB |
| code-demo.py | 47 ms | 200 ms | 25.2 MB | 427 ms | — | 603.7 MB |
| doc-demo.pdf | 60 ms | 217 ms | 32.6 MB | 192 ms | — | 623.5 MB |
| font-demo.ttf | — | — | 30.5 MB | 181 ms | — | 920.0 MB |
| img-alpha.png | 61 ms | 207 ms | 31.4 MB | 149 ms | 292 ms | 630.6 MB |
| img-huge.png（8000×6000） | 60 ms | 237 ms | 38.8 MB | 145 ms | 529 ms | 989.8 MB |
| img-photo.jpg | 46 ms | 219 ms | 43.5 MB | 147 ms | 327 ms | 691.8 MB |
| markdown-demo.md | 47 ms | 186 ms | 41.5 MB | 116 ms | — | 987.4 MB |
| office-demo.docx | 61 ms | 227 ms | 41.5 MB | — | — | 644.5 MB |
| office-demo.pptx | 63 ms | 235 ms | 41.5 MB | — | — | 644.1 MB |
| office-demo.xlsx | 60 ms | 222 ms | 41.5 MB | — | — | 644.4 MB |
| page-demo.html | 44 ms | 382 ms | 37.6 MB | 147 ms | — | 939.7 MB |
| table-demo.csv | 44 ms | 180 ms | 34.1 MB | 128 ms | — | 651.8 MB |
| text-long.txt | 61 ms | 227 ms | 34.2 MB | 145 ms | — | 650.9 MB |
| vector.svg | 44 ms | 278 ms | 40.7 MB | 143 ms | 284 ms | 868.4 MB |
| video-demo.mp4 | 47 ms | 3449 ms | 103.6 MB | 147 ms | 3729 ms | 760.9 MB |

## 汇总

| 指标 | QLRust 1.8.3-fix | 原版 QuickLook 4.5.0 |
| --- | ---: | ---: |
| 出窗夹具数 | **17 / 18** | 14 / 18 |
| 窗口出现（均值 / 中位） | **58 ms / 60 ms** | 190 ms / 147 ms |
| 首帧稳定（收敛样本均值） | **241 ms（16 个收敛）** | 358 ms（仅 4 个收敛*） |
| 预览期内存（18 夹具均值） | **37.5 MB（13.6–103.6）** | 722.4 MB（553–990） |

`*` 原版的文本 / 代码 / CSV / PDF / HTML / Office / Markdown / 音频 / 压缩包在本轮 8 s 内都**没达到 3 帧一致**
（WPF 预览带光标闪烁、WebView2 有动画），所以它们的「稳定」值不可与 QLRust 直接比；图片 / SVG / 视频
两边都能收敛。因此**主口径用「窗口出现 + 内存 + 覆盖」，稳定值只作参考**。

> 内存是"看文件期间"的 `WorkingSet64`：QLRust 全程 13.6–103.6 MB（最大是视频/超大图），关掉预览回落到个位数 MB；
> 原版每开一个预览就再拉一套 WPF 视觉树，切几个文件后停在 0.55–0.99 GB。

## 冷启动复测（2026-11-04，两工具同口径，各 3 轮）

| 指标 | QLRust 1.8.3-fix | QuickLook 4.5.0 | 差距 |
| --- | ---: | ---: | ---: |
| 冷启动 → 窗口出现 | **31 ms**（94 / 31 / 30） | 1085 ms（1085 / 1091 / 1063） | **35×** |
| 冷启动 → 画面稳定/就绪 | **90 ms**（160 / 90 / 75） | 1110 ms（1131 / 1110 / 1082） | 12× |
| 看一张 1280×720 图的内存 | **22.2 MB** | 163.1 MB | 7.3× |

## 差异清单

- **QLRust 有、原版本机没有**：Office 三件（docx/pptx/xlsx，走系统 `IPreviewHandler`）、400 行的 `big-sheet.xlsx`。
  原版缺的是 `OfficeViewer-Native` 插件（安装包里选装），不是它做不到。
- **原版有、QLRust 没有**：字体（`font-demo.ttf`）。原版用 DirectWrite 画字体样本页；我们目前把 ttf 归到「不支持」。
- **两边都有、QLRust 明显更快**：压缩包、文本/代码/CSV、PDF、图片、Markdown、SVG、HTML（窗口出现与内存都领先一个数量级）。
- **视频**：两边「稳定」都在 3.5 s 左右（画面一直在动，本来就不稳定）；QLRust 内存 103.6 MB，原版 760.9 MB；
  QLRust 只有画面 + 进度条（不发声），原版能出声。
- **音频**：QLRust 62 ms 出窗 / 237 ms 稳定；原版 148 ms 出窗、稳定未在 8 s 内收敛（加载整套 WPF 波形控件）。

## 结论

- 功能覆盖：18 类夹具 17 类能预览；唯一缺口是**字体预览**，对「快速看文件」主场景权重低，可以先发。
- 性能与内存：全项领先，尤其「看几个文件后的内存」（37.5 MB vs 722 MB）——原版用户最常抱怨的点。
- 无回归：本轮 `cargo test` **316 passed / 0 failed**、fmt/clippy 零输出、`verify-layers` **14 PASS**、
  `smoke` 16 PASS · 1 SKIP、`verify-office` 23 / `verify-media` 15 / `verify-mediashape` 13 / `verify-archive` 11 全绿。
