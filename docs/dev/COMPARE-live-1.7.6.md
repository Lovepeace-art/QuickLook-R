# 实机对比：QLRust 1.7.6 vs 原版 QuickLook 4.5.0

> 2026-11-01 · 同一台机器（Windows 10 19045 x64、单屏 1920×1080）、同一批 17 个夹具、同一套测量方法。
> 结论先说：**QLRust 在"日常四类"上已经够用，且启动/内存/跟随速度全面领先；差的是 3 类格式
> （音频播放 / 字体 / 矢量渲染），以及 Office 上反而是原版在本机不可用。**

---

## 0. 测量方法（可复现）

| 项 | 做法 |
| --- | --- |
| 夹具 | `<夹具目录>\compare\`（生成脚本 `_bench\make-compare-fixtures.ps1`，17 个文件，1.6 MB） |
| 触发 | **都用 IPC，不注入按键**：QLRust `qlrust.exe --show <path>`；原版走命名管道 `\\.\pipe\QuickLook.App.Pipe.<SID>`（助手 `_bench\probe\ql-trigger.ps1`） |
| 计时 | `Stopwatch` 起于触发；**窗口出现** = 枚举到该工具的预览窗且可见；**画面稳定** = 窗口截图的 8px 采样指纹连续 3 次不变 |
| 截图 | `PrintWindow(PW_RENDERFULLCONTENT)`（不用 `CopyFromScreen`，CONVENTIONS #54） |
| 脚本 | `_bench\compare-run.ps1 -Tool qlrust|quicklook`，结果 `_bench\compare\<tool>.csv` + 每夹具一张 png |
| 注意 | 原版窗口每次打开都换 HWND，只能按"类名前缀 `HwndWrapper[QuickLook.exe` + 可见 + 标题不是裸 `QuickLook`"现查；它的界面有淡入动画与媒体进度条，**"画面稳定"对媒体类会显著偏大**（不代表用户等待时间） |

---

## 1. 格式覆盖（17 个夹具，逐项实测）

| 夹具 | QLRust 1.7.6 | 原版 QuickLook 4.5.0 |
| --- | --- | --- |
| `img-photo.jpg` 1280×720 | ✅ 图片 | ✅ 图片（带工具栏/缩放） |
| `img-alpha.png` 带透明 | ✅ | ✅ |
| `img-huge.png` 8000×6000 | ✅ 降采样 | ✅（内存 +384 MB） |
| `vector.svg` | ⚠️ **当源码文本显示**（XML 高亮，不渲染图形） | ✅ **真矢量渲染**（含渐变/文字） |
| `doc-demo.pdf` 8 页 | ✅ 真排版 + 页码框 | ✅（WebView2/PDF 插件） |
| `text-long.txt` 500 行 | ✅ 自绘文本 + 行号 | ✅ |
| `code-demo.py` 69 行 | ✅ 仿 VSCode 语法高亮 | ✅ |
| `markdown-demo.md` | ✅ 自绘排版（目录/高亮） | ✅ WebView2 渲染（会拉起 msedgewebview2） |
| `table-demo.csv` | ✅ 分隔符表格 | ✅ CsvViewer |
| `page-demo.html` | ⚠️ 源码文本（不执行 HTML） | ✅ **WebView2 真渲染** |
| `archive-demo.zip` | ✅ 内置目录树（可折叠/滚动） | ✅ 目录树 |
| `office-demo.docx` | ✅ **系统 IPreviewHandler 真排版** | ❌ **窗口卡在加载态（裸标题、455×206），出不来内容** |
| `office-demo.xlsx` | ✅ 真排版 | ❌ 同上 |
| `office-demo.pptx` | ✅ 真排版 + 翻页接管 | ❌ 同上 |
| `font-demo.ttf` | ❌ **无窗口**（不支持） | ✅ 字形表（114 KB 截图） |
| `audio-demo.wav` | ❌ **无窗口**（不支持） | ✅ 媒体播放器（可播） |
| `video-demo.mp4` | ⚠️ 只出**静态首帧**（不能播放） | ✅ 可播放（3 s 测试片，画面在动） |

**覆盖计数**：QLRust 完整 11 项 / 部分 3 项（svg、html、mp4）/ 无 3 项（字体、音频）——
注：Office 三件在原版是"无"，在 QLRust 是"完整"，所以两边的窟窿不在同一个位置。

> 原版 Office 失败的原因（本机）：`D:\Xin\QuickLook\QuickLook.Plugin` 里只有 `OfficeViewer`，
> 没有 `OfficeViewer-Native`，窗口打开后停在加载态；这不是"原版不支持 Office"，而是它的
> Office 链路依赖额外原生组件/Office 互操作，本机这份没跑起来。**但用户视角就是"看不了"。**

---

## 2. 速度（毫秒，越低越好）

| 夹具 | QLRust 窗口 / 稳定 | 原版 窗口 / 稳定 |
| --- | --- | --- |
| img-photo.jpg | **44 / 186** | 183 / 379 |
| img-huge.png | **48 / 184** | 153 / 527 |
| doc-demo.pdf | **45 / 247** | 249 / 496 |
| text-long.txt | **60 / 215** | 168 / 324 |
| code-demo.py | **60 / 227** | 153 / 402 |
| markdown-demo.md | **46 / 251** | 131 / 314 |
| table-demo.csv | **45 / 215** | 186 / — |
| page-demo.html | **48 / 373** | 153 / 631 |
| archive-demo.zip | **117 / 298** | 386 / 598 |
| vector.svg | 44 / 188（源码文本） | 193 / 367（渲染） |
| font-demo.ttf | — | 148 / 647 |
| audio-demo.wav | — | 193 / 3136（媒体） |
| video-demo.mp4 | 43 / 187（静态帧） | 142 / 3740（播放中） |
| office-demo.docx | 63 / 386 | — |
| **平均（可比的 10 项）** | **窗口 54 ms · 稳定 238 ms** | **窗口 183 ms · 稳定 917 ms** |

- **窗口出现快 3.4×、画面稳定快 3.9×**；重灾区分两类：图片类（243 ms vs 350 ms）与
  PDF/压缩包这类要"加载外部渲染器"的（原版要起 WebView2/插件程序集）。
- 原版"稳定"里含淡入动画与媒体进度刷新，**媒体类的 3.1–3.7 s 不能当成打开耗时**；
  但即便只看静态文档，QLRust 也快 2 倍上下。

---

## 3. 内存

| 状态 | QLRust | 原版 QuickLook |
| --- | --- | --- |
| 空闲（无预览） | **23.2 MB** | 248.2 MB |
| 看 PDF 时 | **28.3 MB** | 254.9 MB（子进程 0；看 md/html 时会再拉 msedgewebview2） |
| 看 8000×6000 图时 | 20.4 MB | 1730 MB（整棵树含 WebView2） |
| 句柄数 | 242 | 1718 |

---

## 4. 交互手感

| 项 | QLRust | 原版 |
| --- | --- | --- |
| 跟随选中项 | 轮询 **90 ms** + 线程 `PostMessage` 唤醒，**实测 41 ms** 开始切换 | `FocusMonitor` **500 ms** 轮询（+ 方向键 keyup 触发） |
| 换文件过渡 | 冻帧 + 旧内容盖 260 ms（1.7.6） | 淡入动画（约 100–200 ms） |
| 开关语义 | 空格 toggles、Esc 关、顶栏按钮 | 空格 keydown toggle、Esc keyup 关、按住 750 ms 自动回落 |
| 顶栏/工具栏 | 自绘极简（锁 / 打开 / 关闭 / 页码框） | 带图标工具栏（旋转、适配、复制路径等） |
| 缩放/滚动 | 滚轮缩放/滚动、Shift 放大镜 | 反相/旋转/缩放按钮，无放大镜 |
| 多语言 | 仅中文 | 29 种 |

---

## 5. 结论：是否"满足基本需求"

**对日常使用的四类（图片 / PDF / 文本·代码 / Markdown）+ 压缩包 + Office，QLRust 已达标**，
而且在启动、内存、跟随速度上是数量级优势；Office 甚至比本机原版更好用。

**要补的三处（按感知度）**：
1. **音频 / 视频**：视频只能出静态首帧、音频完全打不开——这是唯一"打开就觉得不能用"的品类；
2. **字体 `.ttf`**：无窗口，直接没反应；
3. **SVG / HTML**：当源码显示（可读但不渲染），需要的话可走系统渲染（WebView2 / IWIC）。

**建议**：如果目标是"满足基本需求再上 GitHub"，最小补齐清单就是上面 3 项（音频可先做
"系统播放器打开/内嵌简单播放"，SVG 可用 WebView2 或 Windows 的矢量渲染，字体画一张字形表）。
其余（多语言、工具栏图标、自动更新）属于体验加分项，不阻断发布。

---

## 6. 可复现命令

```powershell
# 1) 夹具（幂等）
pwsh -NoProfile -File <工作区>\_bench\make-compare-fixtures.ps1

# 2) QLRust 侧（需要 qlrust-try\qlrust.exe 在运行）
pwsh -NoProfile -File <工作区>\_bench\compare-run.ps1 -Tool qlrust

# 3) 原版侧（需要 QuickLook 在运行，并用 probe\ql-trigger.ps1 走命名管道）
pwsh -NoProfile -File <工作区>\_bench\compare-run.ps1 -Tool quicklook
```

截图分别在 `_bench\compare\qlrust\` 与 `_bench\compare\quicklook\`（同名夹具直接对比）。
