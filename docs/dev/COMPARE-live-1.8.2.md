# 实机对比（1.8.2 vs 原版 QuickLook 4.5.0）

> 目的：上 GitHub 之前的最后一次「本地全面对比」——同一台机器、同一批夹具、同一个测量口径，
> 逐项量 QLRust 1.8.2 与原版 QuickLook 4.5.0 的**触发 → 窗口出现 → 画面稳定**、内存，以及能看哪些格式。
>
> 结论：18 个夹具里 QLRust 出窗 **17 个**（缺字体）、原版 **14 个**（缺 Office 三件与 400 行表格）；
> 窗口出现均值 **59 ms vs 221 ms**；首帧稳定均值（不含视频）**260 ms vs 603 ms**；
> 预览期内存均值 **34.5 MB vs 1007.1 MB**。

## 环境与口径

- 机器：Windows 10 19045、单屏 1920x1080、管理员会话。
- QLRust：`C:\Users\Administrator\Desktop\qlrust-try\qlrust.exe`（1.8.2.0，便携目录，装 `plugins\` + `pdfium.dll`）。
- 原版：`D:\Xin\QuickLook\QuickLook.exe` 4.5.0（单实例，pid 22660；本机**没装** OfficeViewer-Native，所以 Office 打不开）。
- 触发：两边都走 IPC（QLRust `--show <path>` / 原版命名管道 `QuickLook.App.Pipe…Invoke|<path>|`），不注入按键。
- 判定：`EnumWindows` 按进程 + 类名找预览窗（原版是 WPF 窗，标题不是裸 `QuickLook` 才算加载完）；
  截图 `PrintWindow(PW_RENDERFULLCONTENT)`，连续 3 次采样无差异算「画面稳定」；
  内存取该工具所有进程 `WorkingSet64` 之和。
- 脚本：`_bench\compare-run.ps1 -Tool qlrust|quicklook`（结果 CSV：`_bench\compare\qlrust.csv` / `quicklook.csv`，
  截图：`_bench\compare\<tool>\<夹具>.png`）。

## 逐夹具结果

| 夹具 | QLRust 窗口 | QLRust 稳定 | QLRust 内存 | 原版 窗口 | 原版 稳定 | 原版 内存 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| archive-demo.zip | 129 ms | 366 ms | 13.8 MB | 488 ms | 677 ms | 858.0 MB |
| audio-demo.wav | 62 ms | 250 ms | 21.6 MB | 197 ms | 2648 ms | 936.3 MB |
| big-sheet.xlsx | 60 ms | 377 ms | 28.4 MB | ✗ | ✗ | 911.4 MB |
| code-demo.py | 46 ms | 198 ms | 28.1 MB | 178 ms | 347 ms | 904.7 MB |
| doc-demo.pdf | 47 ms | 199 ms | 34.1 MB | 240 ms | 410 ms | 913.1 MB |
| font-demo.ttf | ✗ | ✗ | 32.2 MB | 197 ms | 778 ms | 1203.2 MB |
| img-alpha.png | 62 ms | 199 ms | 33.2 MB | 196 ms | 431 ms | 909.7 MB |
| img-huge.png（8000x6000） | 46 ms | 183 ms | 34.4 MB | 188 ms | 562 ms | 1172.7 MB |
| img-photo.jpg | 46 ms | 175 ms | 33.0 MB | 200 ms | 358 ms | 1159.0 MB |
| markdown-demo.md | 45 ms | 289 ms | 33.5 MB | 198 ms | 353 ms | 1228.5 MB |
| office-demo.docx | 59 ms | 397 ms | 33.8 MB | ✗ | ✗ | 927.5 MB |
| office-demo.pptx | 60 ms | 233 ms | 33.8 MB | ✗ | ✗ | 907.7 MB |
| office-demo.xlsx | 60 ms | 442 ms | 33.9 MB | ✗ | ✗ | 904.9 MB |
| page-demo.html | 57 ms | 204 ms | 32.3 MB | 197 ms | 595 ms | 1195.9 MB |
| table-demo.csv | 61 ms | 239 ms | 30.9 MB | 198 ms | 1032 ms | 908.3 MB |
| text-long.txt | 60 ms | 213 ms | 31.0 MB | 194 ms | 1359 ms | 911.8 MB |
| vector.svg | 61 ms | 195 ms | 36.2 MB | 195 ms | 338 ms | 1154.8 MB |
| video-demo.mp4 | 45 ms | 3490 ms | 97.2 MB | 230 ms | 3958 ms | 1020.9 MB |

## 汇总

| 指标 | QLRust 1.8.2 | 原版 QuickLook 4.5.0 |
| --- | ---: | ---: |
| 出窗夹具数 | **17 / 18** | 14 / 18 |
| 窗口出现（均值 / 中位） | **59 ms / 60 ms** | 221 ms / 197 ms |
| 首帧稳定（均值，全部） | **450 ms** | 989 ms |
| 首帧稳定（均值，不含视频） | **260 ms** | 603 ms |
| 预览时内存（均值） | **34.5 MB** | 1007.1 MB |
| 空闲内存（实测） | ~23 MB | 253.8 MB |

「内存」是**看文件期间**的 `WorkingSet64`：原版每个预览都会拉起 WPF 视觉树，切几个文件后就停在 0.9–1.2 GB，
空闲回落到 253.8 MB；QLRust 全程 13.8–97.2 MB，最大的是视频（Media Foundation 解码缓冲）。

## 差异清单

- **QLRust 有、原版本机没有**：Office 三件（docx/pptx/xlsx，走系统预览处理器）、400 行的 `big-sheet.xlsx`。
  原版缺的是 `OfficeViewer-Native` 插件（安装包里选装），不是它做不到。
- **原版有、QLRust 没有**：字体（`font-demo.ttf`）。原版用 DirectWrite 画字体样本页；我们目前把 ttf 归到「不支持」。
  这是已知缺口；要补的话，在 `qlr-render` 里加一个字体预览通道即可（DirectWrite 本机可用）。
- **两边都有、QLRust 明显更快**：压缩包列表（129/366 vs 488/677）、文本/代码/CSV（45–61 / 198–289 vs 178–198 / 347–1359）、
  PDF（47/199 vs 240/410）、图片（46–62 / 175–199 vs 188–200 / 358–562）、Markdown、SVG、HTML。
- **视频**：两边「稳定」都是 3.5–4 s（画面一直在动，本来就不会稳定）；QLRust 内存 97 MB，原版 1021 MB。
  QLRust 只有画面 + 进度条（MCI 播不了 mp4，音频走 Media Foundation 但不发声），原版能出声。
- **音频**：QLRust 62 ms 出窗、250 ms 稳定；原版 197 ms / 2648 ms（加载了整套 WPF 波形控件）。

## 冷启动复测（2026-11-02，两工具同口径，各 3 轮）

起因：README 一开始只放了「常驻后窗口出现 59 vs 221 ms（3.7×）」，读起来像"优势缩水"；
而 1.3.0 那份对比里是「冷启动到首帧 10–14×」——**两者口径不同**，不是性能回退。
所以按同一口径重测一遍（进程不存在 → 预览窗出现 → 画面稳定，各 3 轮取中位）：

| 指标 | QLRust 1.8.2 | QuickLook 4.5.0 | 差距 |
| --- | ---: | ---: | ---: |
| 冷启动 → 窗口出现 | 45 ms（32 / 45 / 62） | 1174 ms（1134 / 1174 / 1190） | **26×** |
| 冷启动 → 画面稳定 | 104 ms（98 / 104 / 128） | 1222 ms（1184 / 1222 / 1225） | 12× |
| 看一张 1280×720 图的内存 | 13.6 MB | 163.6 MB | 12× |
| 空闲内存 / 句柄 | 10.1 MB / 195（线程 7） | 95.2 MB / 517（刚启动；久驻 253.8 MB / 1739） | 9–25× |
| 分发包 / 主程序 | 5.43 MB / 2.23 MB exe | 约 278 MB / 数十个程序集 | 51× |

- 夹具：`img-photo.jpg`（1280×720）。QLRust 用 `--show`，原版用命名管道 `Invoke|path|`，触发前都把进程杀干净并等 0.9–1.5 s。
- 脚本：`_bench\coldstart-3x.ps1`（两工具各 3 轮，打全部样本与中位）。
- 为什么"常驻后"只有 3.7×：原版把上一次的 WPF 视觉树留在内存里，第二次打开只是"显示"；
  QLRust 每次按当前窗口尺寸重渲染（换来常驻 10–14 MB 而不是 95–254 MB）。这是设计取舍，不是缺陷。
- 为什么原版内存会从 95 MB 涨到 253.8 MB：它每开一个预览就再拉一套 WPF 控件，看几个文件后停在 0.9–1.2 GB，
  关掉预览也回不到刚启动的水平；QLRust 关掉预览就回到 10–14 MB。

## 拍板上 GitHub 的结论

- 功能覆盖：18 类夹具里 17 类能预览；唯一缺口是**字体预览**，对「快速看文件」这个主场景权重很低，可以先发。
- 性能与内存：全项领先，尤其「看几个文件后的内存」（34.5 MB vs 1007 MB）——这是原版用户最常抱怨的点。
- 没有回归：`verify-office` 23 PASS、`verify-mediashape` 13 PASS、`verify-archive` 11 PASS、
  `verify-media` 15 PASS、`smoke` 全过、`cargo test` 251 passed。
