# 架构（ARCH）

> 本文只写**当前真实存在**的结构。计划中的东西一律标 `[计划]`，不要在这里描述没写的代码。

## 1. 分层与依赖方向

```
qlr-app  ──►  qlr-platform  ──►  qlr-core
   │  │                              ▲
   │  └──►  qlr-render  ─────────────┘
   └──────────────────────────────────┘
```

- `qlr-core`：**不依赖 `windows` crate**，纯逻辑，可跨平台单测。
  `config`（JSON 配置）· `paths`（便携/非便携数据目录）· `log`（自研文件日志）· `ipc`（协议常量与 `Request` 编解码）· `error` · `version`。
- `qlr-platform`：Win32 薄封装，只做 FFI，不做业务决策。
  `window`（窗口类/创建/显示/绘制/消息泵/滚动条/贴图）· `hotkey`（`WH_KEYBOARD_LL` + `WH_MOUSE_LL` 钩子）·
  `selection`（资源管理器选中项）· `shellpreview`（系统预览处理器宿主 `IPreviewHandler`）·
  `audio`（MCI 播放条）· `tray`（托盘图标与右键菜单）· `instance`（单实例互斥体 + `WM_COPYDATA` 转发）·
  `win11`（圆角 + 系统 backdrop）· `dpi` · `integrity`（进程令牌完整性）· `crash`（崩溃报告）· `wide`（字符串转换）。
- `qlr-render`：**不依赖 `windows` crate**，纯 CPU 渲染，可跨平台单测。
  `canvas`（`Bitmap`/`TextDoc`/`Canvas`）· `image_view`（解码+缩放+BGRA）· `pdf`（PDFium 动态绑定）·
  `text`（编码探测+行切分+上限）· `code`（源码着色）· `markdown` · `html`（自写极简解析器）·
  `svg`（Direct2D `CreateSvgDocument` + 降级光栅化器）· `archive`（zip 目录树解析）· `wrap`（折行·测量）·
  `downscale`（大图预算）· `cancel`（渲染取消）· `lib`（`Viewer` trait + 按格式路由）。
  唯一边界例外：`media`（Media Foundation 探针 + 视频首帧），见该文件顶部说明。
- `qlr-app`：宿主进程。`app`（状态机 + `wndproc` + 渲染线程调度 + 压缩包列表 + PPT 翻页接管）· `cli`（参数解析）· `main`。

规则：**业务判断不许下沉到 platform，FFI 细节不许上浮到 app**。

## 2. 进程与线程模型

| 项 | 现状（M2） | 目标 |
|---|---|---|
| 进程数 | 1 个常驻进程；再次调用时空转一次就退出 | A9 才考虑子进程池 |
| 线程 | **UI 线程**（消息泵）+ **钩子线程**（`WH_KEYBOARD_LL`）+ **渲染线程**（`qlr-render`） | 交互（A2.8）后可能再加一条预取线程 |
| 生命周期 | 冷启动 → 常驻 → `--quit` 退出 | 同 |

渲染线程的数据流（ADR-0007）：

```
UI:  show_preview → request_render(gen += 1) ──mpsc──► worker: qlr_render::render_file
UI:  （窗口立刻显示占位文字）                              │
UI:  on_render_done ◄──PostMessage(WM_QLR_RENDER)────────┘ 结果写进 RESULT 槽
UI:  gen 匹配 → CONTENT = Arc<Canvas> → InvalidateRect → WM_PAINT 用 paint_bitmap/paint_text
UI:  gen 不匹配 → 丢弃（翻文件时旧帧不能盖新帧）
```

**没有 async 运行时**，也不要加（见 AGENTS.md §5）。

线程间的唯一通信方式是 `PostMessage`：

```
钩子线程 ──(PostMessage WM_QLR_HOTKEY)──> UI 线程 wndproc ──> on_hotkey()
                                             └─> selection::current_selection()  // STA 才能调 COM
```

**为什么钩子要独立线程**：低级钩子的回调在安装它的线程上被调用，而那个线程必须有消息循环。
放 UI 线程上会被渲染/IO 拖累，一旦回调超时 `LowLevelHooksTimeout`（默认 300 ms）累计 10 次，
Windows 会把进程从钩子链上静默摘掉（无通知）。细节与心跳自愈见 ADR-0005。


## 3. 窗口

注册两个窗口类，共用一个 `wndproc`：

| 类名 | 用途 | 样式 |
|---|---|---|
| `QLRust.HostWindow` | 隐藏的"宿主"窗口：接收 `WM_COPYDATA`（第二实例转交的请求）和托盘回调 | `WS_EX_TOOLWINDOW`，创建后不显示 |
| `QLRust.PreviewWindow` | 预览窗，画内容的地方 | `WS_POPUP` + `WS_EX_TOOLWINDOW \| WS_EX_TOPMOST \| WS_EX_NOACTIVATE` |

要点：

- **不抢焦点**：`WS_EX_NOACTIVATE` + `ShowWindow(SW_SHOWNOACTIVATE)` + `SetWindowPos(SWP_NOACTIVATE)`。空格预览的关键前提是"用户还在资源管理器里"。
- **无边框**：`WM_NCCALCSIZE`（`wParam != 0`）直接返回 0，不调 `DefWindowProc`。保留 DWM 阴影与圆角。
- **背景自己管**：`WM_ERASEBKGND` 返回 1，避免擦除把亚克力刷成实色；窗口类注册时 `hbrBackground` 给空刷子。

## 4. 状态与锁

```
STATE: OnceLock<Mutex<State>>      // config, host, preview, visible, last_path, started
LINES: OnceLock<Mutex<Vec<String>>> // 待绘制的文本行
```

**为什么是两个锁**：`ShowWindow` 会同步重入 `wndproc` → `WM_PAINT` → 读 `LINES`。如果绘制内容存在 `STATE` 里并在持锁时动窗口，就是这个顺序：持 `STATE` → 等 `LINES` … 而写 `LINES` 的路径又可能先拿 `STATE` → 死锁。

**固定顺序**：先写 `LINES` 并**放锁**，再动窗口。

## 5. 单实例与 IPC

- 互斥体：`Local\QLRust.Instance`（`CreateMutexW` + `ERROR_ALREADY_EXISTS`），句柄由 `InstanceGuard` 的 `Drop` 关闭。用 `Local\` 前缀，避免跨会话互相顶。
- 找宿主：`FindWindowW("QLRust.HostWindow")`。
- 投递：`SendMessageW(WM_COPYDATA)`，`COPYDATASTRUCT{dwData: 0x514C_5253, cbData, lpData}`。
- 载荷：`qlr_core::ipc::Request` 的 JSON，上限 `MAX_PAYLOAD = 64 KiB`。
- 为什么不用命名管道：当前只需要"投一个请求然后退出"，`WM_COPYDATA` 自带同步 + 零额外线程；等 M3 出现"长连接 + 流式响应"需求再换（候选 `interprocess 2.4.7`）。

动作语义（`Request.action`）：

| action | 触发 | 行为 |
|---|---|---|
| `Toggle` | 裸路径 / 空格键 | 同一文件再按=收起；换文件=换内容显示；无路径=纯开关 |
| `Show` / `Hide` | `--show` / `--hide` | 强制显示 / 隐藏 |
| `Quit` | `--quit` | `PostQuitMessage` |
| `Ping` | `--ping` | 只写日志，用于探活 |
| `HotkeyProbe` | `--simulate-hotkey`（**仅测试用**） | 走一遍 `on_hotkey` 的处理链；因为钩子会过滤 `SendInput` 注入键，自动化按不出真空格 |

## 5.2 全局热键（A1.5，ADR-0005）

独立线程装 `WH_KEYBOARD_LL`，回调里只做三件廉价的事：判注入标记、读前台窗口类名、`PostMessageW`。

| 行为 | 实现 |
|---|---|
| 只认 Shell 宿主 | 前台类名 ∈ `SHELL_CLASSES`（`CabinetWClass`/`ExploreWClass`/`ShellTabWindowClass`/`Progman`/`WorkerW`/`dopus.lister`/`Everything`/`FilePilot`/`DeskBox`/`IDM`）；**刻意不含 `#32770`** |
| 过滤注入键 | `KBDLLHOOKSTRUCT.flags & (LLKHF_INJECTED 0x10 \| LLKHF_LOWER_IL_INJECTED 0x02)` |
| 过滤自动重复 | `KEY_DOWN.swap(true)`，`WM_KEYUP` 才清 |
| 自愈 | 每 30 s `Unhook` + `SetWindowsHookEx`（系统摘钩无通知，只能主动重装） |
| 吞键 | 命中时 `return LRESULT(1)`；任何一条不满足就 `CallNextHookEx` |

统计量（`hotkey::stats()`：callbacks/swallowed/reinstalls/passed_other_class）在退出时写日志，是排查"钩子是不是死了"的唯一手段。

## 5.3 选中项通道（A1.6，ADR-0006）

```
前台/目标窗口 -> GetAncestor(.., GA_ROOT) -> 类名
  ├─ Progman / WorkerW（桌面）
  │    FindWindowSW(&VARIANT::default(), &VARIANT::default(), SWC_DESKTOP, &mut resolved, SWFO_NEEDDISPATCH)
  │    （3 轮 × 2 种 flags 重试，间隔 15 ms —— 会间歇性返回 S_OK + 空指针）
  │
  └─ 其他（资源管理器窗口）
       CoCreateInstance(CLSID_ShellWindows) -> IShellWindows
         for i in 0..Count(): Item(i) -> IServiceProvider -> QueryService(SID_SShellBrowser)
             -> IOleWindow::GetWindow()            // 得到的是 ShellTabWindowClass，不是 frame
             -> QueryActiveShellView() -> IFolderView2::GetSelection(false) -> 各项 GetDisplayName
       匹配：先精确（目标本身就是 tab），再同根（GetAncestor(.., GA_ROOT) 相等，可见优先）

两条路最终都汇到 paths_from_browser()：
  GetDisplayName(SIGDN_FILESYSPATH) 失败时，退回
  GetDisplayName(SIGDN_PARENTRELATIVEPARSING) + folder.GetFolder::<IShellItem>() join，
  且拼出来的路径必须 exists() 才认（回收站这类虚拟项会被自然跳过）
```

- **不能用 `FindWindowSW` 定位资源管理器窗口**：实测传任何 frame HWND 都解析到桌面那条（ADR-0006）。
  它只适合配 `SWC_DESKTOP` 取桌面。
- **桌面不在 `IShellWindows` 枚举里**（实测 `count=3` 全是文件夹窗口），必须单独走 `SWC_DESKTOP`；
  `Source::Desktop` → 日志里的 `source=desktop`。
- 空选区（`S_FALSE` / `0x80070490`）是正常状态，返回 `source=none`，不阻塞按键。
- 必须在 STA 线程调用（UI 线程）；进程令牌完整性级别不足时 `CoCreateInstance` 会 `0x80070005`（CONVENTIONS 坑 #9）。
- **不能在 `WM_COPYDATA` 处理期间调用**：那时线程处于输入同步呼叫状态，COM 激活会被拒成
  `0x8001010D`。所以第二实例转交的请求一律先排队（`PENDING`），再 `PostMessage(WM_QLR_INVOKE)` 延后处理。
- 调试入口：`--probe-selection`（走前台窗口）、`--probe-selection-hwnd <HWND>`（钉死窗口），两者都只写日志不建窗口。

## 6. 材质（Win11 圆角 + 系统 backdrop）

现状（1.8.2）：预览窗**不画半透明材质**，只有两块系统增强，全在 `crates/qlr-platform/src/win11.rs`：

- Win11（≥ 22000）：`DWMWA_WINDOW_CORNER_PREFERENCE`（圆角）+ `DWMWA_SYSTEMBACKDROP_TYPE`（Mica 家族 backdrop），
  由 DWM 合成，跟随系统"透明效果"开关。
- Win10（≤ 19045）：这两个属性不存在（`E_INVALIDARG`），**退回纯色背景**。

历史：0.6.0–1.4.x 曾有一条 Win10 亚克力通道（未文档化 `user32!SetWindowCompositionAttribute` +
`ACCENT_ENABLE_ACRYLICBLURBEHIND`，参数 `GradientColor` 是 `0xAABBGGRR`），**1.5.0 删除**——位图内容本来就
必须关掉半透明层（否则"图片像被提亮"），留着的代码路径只增加失败面。

失败时只 WARN、退回普通背景，不阻止预览。

## 7. 绘制

`WM_PAINT` 只看两个来源，**优先画面、其次占位文字**：

1. `CONTENT: Option<Arc<Canvas>>` 有值：
   - `Canvas::Bitmap` → `paint_bitmap`：`StretchDIBits`，`BITMAPINFO` 用负 `biHeight`（top-down，
     和 BGRA 缓冲顺序一致），配 `HALFTONE` + `SetBrushOrgEx(0,0)`（否则缩放出条纹）。
   - `Canvas::Text` → `paint_text`：按行高 `LINE_HEIGHT=24` 只画放得下的前 N 行（可视区虚拟化 `[计划]` A2.8）。
2. 否则画 `LINES` 里的占位文字（`DrawTextW`，空行只占高度不画 —— 见 CONVENTIONS 坑 #1）。

渲染本身在 worker 线程，数据流见 §2。**UI 线程从不做解码**。
`[计划]` A9.1 换 D3D11 flip 模型（`softbuffer`/`wgpu`）解决大图拖动掉帧，必须同批写 GDI 回退。

## 8. 日志

自研（`qlr-core/src/log.rs`），不用 `tracing`：

- 格式：`2026-10-29T12:36:42.607Z LEVEL [ThreadId(1)] scope  message`（UTC 时间，手写 civil 换算，不引 chrono）。
- 级别由 `QLRUST_LOG` 环境变量覆盖，否则用配置里的 `log_level`（默认 `info`）。
- 单文件，超 4 MB 轮转成 `.1`；写失败静默（不能在日志里再崩一次）。
- `panics`：`run()` 里装了 `std::panic::set_hook`，panic 写 `PANIC: ...` 到日志 —— GUI 子系统没有 stderr，不装就彻底丢现场。

## 9. 数据目录

- 便携：`<exe 目录>\portable.lock` 存在 → 数据写 `<exe>\UserData\`（对齐上游 QuickLook 的约定）。
- 否则：`%LOCALAPPDATA%\QLRust\`。
- 布局：`config.json`、`logs\app.log`(+`.1`)。

## 10. 退出路径

- `WM_CLOSE` → **只隐藏**，不退出（托盘/下次空格还要用）。
- `WM_DESTROY` → `PostQuitMessage`。
- `--quit` → 转交给常驻实例 → `quit()` → `PostQuitMessage`。
- 正常退出会先移除托盘图标，再写 `退出：总运行 {n}ms`。

## 11. `[计划]` 还没写的东西

PDF 渲染（A2.6）· 交互：滚轮缩放/拖动平移/Esc（A2.8）· 文本 mmap 与可视区虚拟化 · 按显示尺寸缩放解码（A8.1/A2.3）· 选中项第三/四级兜底（UIA `SelectionPattern`、鼠标命中）· 热键"按住出窗/松手收起"语义（钩子已能拿到 `WM_KEYUP`，未接）· 插件体系 · 设置界面 · 自更新。
细节与风险见仓库上一级 `docs/route-a-plan.md` §5、`docs/route-a-wbs.md` §7。

### PDF 渲染（0.1.6）

`qlr-render/src/pdf.rs` 直接绑 PDFium C API（12 个函数，`libloading` 运行时加载
`pdfium.dll`，**不做链接期依赖**，缺库时其他格式照常工作）：

```
渲染线程 → pdf::render_page(path, page, max_w, max_h)
          ├─ std::fs::read             （自己读字节，绕开 pdfium 的 ANSI 路径限制）
          ├─ FPDF_LoadMemDocument64    （缓冲区由 DocHandle 持有，见 CONVENTIONS #30）
          ├─ FPDF_GetPageCount / FPDF_LoadPage
          ├─ FPDFBitmap_CreateEx       （写进我们自己的 Vec<u8>，stride = w*4，BGRA）
          ├─ FPDFBitmap_FillRect(白)   （PDF 页面无背景，不填是透明黑）
          └─ FPDF_RenderPageBitmap(…, FPDF_ANNOT) → Canvas::Bitmap
```

- 只按显示尺寸渲染一次，不做"先大图再缩"。
- 页索引越界自动夹到最后一页；`FPDF_GetLastError` 翻成中文原因。
- 翻页：钩子在**预览窗开着**时旁听 PageDown/PageUp/←/→（**不吞键**），
  UI 线程 `step_page(±1)` 改页 → 重新投渲染任务 → 代际号保证旧帧不覆盖新帧。

### 缩放 / 平移（0.1.7）

- 状态：State.zoom: f32（0.25–8.0）、State.pan: (i32, i32)（像素，绘制时钳制）。
- 数据流：--zoom-in / 方向键 → set_zoom_to() → 用"客户区 × zoom"作为渲染请求尺寸 → 投给渲染线程 → 新位图 → epaint 用 paint_bitmap_at(..., pan)。
- **缩放 = 按目标像素重新栅格化**，不是把已有位图拉大：PDF 走 pdf::fit_scaled()（允许放大，矢量重新渲染更清晰）；图片走 image_view::fit_within()（保持"不放大"，避免把小图糊成马赛克）。
- 平移只改 pan，不重新渲染；paint_bitmap_at 里位图比客户区大的那一维按偏移裁剪、小的那一维居中。
- 键位：+/- 与方向键/PageUp/PageDown 在 hotkey::NAV_KEYS 里，**只在预览窗打开时旁听、一律不吞键**；0（复位）故意不旁听，避免干扰打字。

### 缩放 / 平移 / 翻页（0.1.8 完整版）

三条输入通道，各自的"为什么要绕路"：

| 操作 | 消息从哪来 | 为什么不直接收 |
|---|---|---|
| 空格（开合预览） | `WH_KEYBOARD_LL` 钩子 | 要在资源管理器里就吞掉，普通按键消息到不了我们 |
| Esc（收起）/ 滚轮 | 同上 + `WH_MOUSE_LL` 钩子 | 预览窗是 `WS_EX_NOACTIVATE`，永远没焦点，收不到 `WM_KEYDOWN`/`WM_MOUSEWHEEL` |
| 左键拖动（平移/滚动）、右键拖动（缩放） | 普通 `WM_LBUTTONDOWN`/`WM_RBUTTONDOWN`/`WM_MOUSEMOVE`/`WM_*BUTTONUP` | 鼠标命中不依赖焦点，正常收 |

钩子回调只做三件事：判断该不该管 → `PostMessage` 给 UI 线程 → 决定吞不吞键。
所有状态变更（缩放、平移、重渲染）都在 UI 线程的 `State` 里做。

**1.0.4 起的鼠标契约**（按用户要求定的，改动前先看 `CONVENTIONS` #55 / #57 / #58）：

| 输入 | 文本 | 图片 | PDF |
|---|---|---|---|
| 左键拖动 | 按行滚动（像素攒够 `LINE_HEIGHT` 才滚一行） | **整个窗口在桌面上移动** | 同左 |
| 右键上下拖 | 忽略 | 缩放（60 px = 1.25×） | 缩放 |
| 滚轮 | 一格 3 行，向上滚=看上面的内容 | 放得下→缩放；放大后→上下滑动 | 上下滑动，到页底/页顶继续滚→翻页 |
| 左键单击 | **什么都不做**（1.0.3 删掉了"点一下关闭"） | 同左 | 同左 |

拖动为什么挪窗口而不是挪内容（1.0.4）：挪内容要每帧重画一整张位图（4K 十几毫秒），
桌面合成器会在半成品上取帧 → 用户看到的是**频闪**。挪窗口是把现成的表面交给合成器搬，
零重绘。所有拖动的坐标一律用 `GetCursorPos()` 的**屏幕坐标**，用完再 `WM_MOUSEMOVE` 的
client 坐标会与"窗口自己也在移动"互相抵消（实测鼠标走 90 px、窗口只走 27 px）。

缩放语义：**按目标像素重栅格化**，不是把旧位图拉伸。`zoom` ∈ [0.25, 8]，
请求尺寸 = 客户区 × zoom，PDF 走 `fit_scaled`（允许放大），图片走 `fit_within`
（`upscale` 为 false 时不放大——放大图片没有信息增量，1.0.2 起缩放会传 `upscale = zoom > 1`）。
放大后位图比客户区大，`paint_bitmap_at` 按 `pan` 裁剪显示。

`NAV_KEYS`（PageDown/PageUp/方向键/`+`/`-`）只在预览窗开着时**旁听、不吞键**；
`0`（复位缩放）刻意不旁听 —— 它是最普通的输入字符，吞了会干扰打字，复位只走托盘/命令行。

### 文本渲染（0.1.9）

管线：`qlr-render::text::render` → `Canvas::Text(TextDoc)` → UI 线程按需切片 → `paint_text`。

三条预算：
- `READ_LIMIT = 8 MB`：超过只读前 8 MB，`TextDoc.truncated = true`。
- `MAX_LINES = 200_000`：行数硬上限（8 MB 若全是 1 字符行会有数百万行）。
- **只画可视区**：`repaint` 取 `scroll..scroll + visible_rows`，第一行是状态行。
  `visible_rows = 客户区高 / LINE_HEIGHT(24) - 1`，减掉的 1 行给状态行。

滚动：
- 滚轮（文本上）= 一格 3 行；PageDown/PageUp = 一页；↑↓ = 一行；左键拖动 = 按像素折算行。
- `scroll_clamp(current, delta, total, rows)` 把位置夹在 `[0, total-rows]`，
  纯函数、有单测；内容比窗口短时恒为 0。
- 换内容（`set_content`）时 `scroll` 归零。

为什么图片/PDF 走缩放而文本走滚动：缩放对位图有意义（重栅格化），对文本没有
（文字是 GDI 画的，放大只是拉伸字形）。所以 `scroll_text_by` 返回 `false` 让调用方
退回原来的缩放/翻页/平移路径。

## 文本折行（0.2.0）

分成两层，测量能力用依赖注入，所以 `qlr-render` 仍然不依赖 windows：

```
qlr-app::repaint  ──取宽度闭包──▶  qlr_render::wrap::wrap_window
       │                                   │
       └── window::Measurer (GDI) ─────────┘
```

- `qlr-render/src/wrap.rs`：`pub fn wrap_window(lines, start, max_rows, width, measure) -> WrapWindow`。
  `WrapWindow { rows, consumed_raw }` —— `consumed_raw` 是"这一屏吃掉了几个**原始行**"，
  翻页必须按它前进（折行后一屏 30 行可能只对应 8 行原文）。
  只折当前屏；`MAX_ROWS_PER_LINE = 400` 限制单行最多产出多少行。
- `qlr-platform/src/window.rs`：`pub struct Measurer`（一个 DC + `DEFAULT_GUI_FONT`，`Drop` 还原）
  `pub fn width(&self, text: &str) -> u32`。**必须和 `paint_text` 用同一字体**。
- 分流：`turn_page(delta)` 先 `scroll_text_by(delta * page_lines(hwnd))`，不是文本才 `step_page(delta)`；
  `page_lines()` 优先返回上次绘制写回的 `State.wrapped_consumed`，没画过就退回屏幕行数。

## 格式注册表（0.3.0：收窄为四类 + 内容嗅探）

```
路径 ──▶ qlr_core::format::kind_for_path
              │ ① 扩展名命中四张表（IMAGE / PDF / MARKDOWN / TEXT）→ 立即返回，不读文件
              │ ② 扩展名不认识 / 没有扩展名 → 读文件头 512 B 嗅探
              │      %PDF-                 → Pdf
              │      PNG/JPEG/GIF/BMP/TIFF/ICO/CUR/RIFF+WEBP 魔数 → Image
              │      无 NUL 且控制字符 ≤5% → Text
              │      其它（含读不到文件）  → Unsupported
              ▼
       Kind ──▶ qlr_render::viewer_for(kind)
                    Image            → ImageViewer
                    Text | Markdown  → TextViewer（Markdown 之后叠行样式）
                    Pdf              → PdfViewer（pdfium.dll）
                    Unsupported      → None → 壳层出占位文字 + kind.hint() 指向插件
```

- **只有五个变体**：`Image` / `Pdf` / `Text` / `Markdown` / `Unsupported`。新增格式族请写插件
  （`crates/qlr-plugin-api` + `docs/dev/PLUGINS.md`），不要再往 `Kind` 里加。
- 扩展名真值只有一份：`format::extensions(kind)` / `format::all_extensions()`；
  文档、样本矩阵脚本、文件关联都从这两个函数取，避免三处各写一份再对不上。
- **为什么补了内容嗅探**（原计划"注册表只按名字判定"）：Windows 上大量文本文件没有扩展名
  （`README`、`Makefile`），而不认识的后缀可能是任意二进制。只按名字判会两头都判错。
  嗅探的判据刻意保守 —— 认不出的宁可说"不支持"，也不要 renderer 里抛一屏乱码。
- 判定顺序按"代价从小到大"：扩展名是纯字符串比较；嗅探要 open+read 512 B，
  只在扩展名失配时才付这个代价。
### 插件体系（0.5.0：进程内 C ABI）

```
按空格 → format::kind_for_path(path)
           ├─ Image / Pdf / Text / Markdown → qlr-render 的内置 Viewer（Rust）
           └─ Unsupported → plugins::host().find_for_path(path)
                              ├─ 有插件 → 插件 qlr_plugin_render() → RGBA → Canvas::Bitmap
                              └─ 没插件 → 占位文字（含插件目录/已加载数量/失败数）
```

- **契约 crate 零依赖**：`qlr-plugin-api` 只有 `#[repr(C)]` 结构和三个 `extern "C"` 类型别名。
  宿主与插件引同一份，避免两边各写一遍。`ABI_VERSION` 严格相等才加载。
- **宿主在 `qlr-core`**（不是 `qlr-platform`）：`libloading` 三平台同一套 API，
  放 core 才能保持在"没有 windows 依赖"的边界内，也让插件扫描可跨平台单测。
- **一次性数据流**：插件分配 RGBA → 宿主校验（尺寸/`len`/64 MP 上限）→ 拷贝成 `Vec<u8>` →
  **无条件调 `free`**（哪怕校验失败）→ `Bitmap::from_rgba` 交给既有的位图上屏路径。
  所以插件渲染出来的东西和内置图片走完全相同的缩放/平移/材质逻辑，没有第二条绘制路径。
- **扫描时机**：启动时一次（`OnceLock`），失败项进 `PluginHost::errors` 并写日志；
  `QLRUST_PLUGIN_DIR` 可覆盖目录（开发插件时免拷贝）。
- **为什么不用进程外 JSON-RPC**：见 `docs/adr/0008-plugin-api.md`（一句话：位图过 pipe 的成本
  与"用户自己写几个插件"的规模不匹配；进程外留作 v2）。
### 材质通道（1.8.2：只剩一条系统通道）

| 系统 | 通道 | 代码 | 失败时 |
|---|---|---|---|
| Win11 ≥ 22000 | 官方 `DWMWA_WINDOW_CORNER_PREFERENCE` + `DWMWA_SYSTEMBACKDROP_TYPE` | `crates/qlr-platform/src/win11.rs` | 返回 `false` → 纯色背景 |
| Win10 ≤ 19045 | 没有可用通道（Win10 亚克力通道 1.5.0 已删除） | — | 纯色背景 |

- **版本判定看 `ntdll!RtlGetVersion`**，不用 `GetVersionEx`：后者会被 exe 清单里的
  `<compatibility>` 段骗（我们的清单就声明了 Win10），`RtlGetVersion` 是内核真实版本。
  `OsVersionInfoW` 的布局有单测钉住（5×DWORD + 128×WCHAR），错一字节就返回 `STATUS_BUFFER_TOO_SMALL`。
- **显示位图时必须关掉一切"半透明层"**：Win11 backdrop 会让整窗内容与壁纸混色，
  症状是"图片像被提亮"（0.1.1 那个 bug）。所以有位图内容时 `apply_material()` 调
  `win11::clear_backdrop`（**圆角保留**，圆角不影响像素）。
## 测量与守门（1.0.0）

三套脚本，各自守一个面，互不替代：

```text
scripts/make-fixtures.ps1      生成夹具（-Big 基准用 / -Matrix 样本矩阵用）
        │
        ├── scripts/bench.ps1 ──────────> 三张表 + bench-results.json
        │     · 冷启动（日志"壳层就绪 window_created=Nms"）
        │     · 首帧（每轮两次投递：pass0 冷 / pass1 热，每次前先 --hide）
        │     · 内存（PeakRSS / RSS / Private）
        │     · -Budget 比预算，超了 exit 1；CI 用 bench-budget-ci.json（本机预算 ×2）
        │
        ├── scripts/sample-matrix.ps1 ──> 按类通过率，-MinRate 0.98
        │     · 逐文件 `--render`，靠日志增量读 RENDER-CLI ok/FAIL 判定
        │     · 每类 <20 个样本给 ⚠（合成矩阵：image 31 / text 24 / md 22 / pdf 20）
        │
        └── cargo test（含 tests/malformed.rs）──> 畸形输入 + 318 例确定性变异 fuzz
```

**为什么不用退出码判渲染结果**：`--render` 走的是"投递给常驻实例"这条路，退出码只代表"投递成功"，
真正的渲染结果在宿主日志里。脚本因此用 `FileShare.ReadWrite|Delete` 共享读日志，
按调用前的文件长度做增量匹配（坑：普通 `Get-Content` 持有读锁会挡住写端）。

**为什么 bench 单独一条 workflow**：起真实 GUI 比 `cargo check` 慢一个数量级，
混进 `ci.yml` 会让每次 push 都等几分钟；`bench.yml` 用 push master + 每周一次 + 手动触发，
职责是拦"慢一个数量级"的回归，不是卡 5% 抖动。

## 快捷键与设置页（1.0.7：配置驱动）

```text
config.json  hotkeys{9 个字段}
        │
        ├─ keymap::Hotkeys ──parse──> Binding{vk,ctrl,alt,shift}
        │        │                         │
        │        │                         ├─ packed() [u32; 8] ──> hotkey::set_bindings()
        │        │                         │        │
        │        │                         │        └─ WH_KEYBOARD_LL 回调按表匹配
        │        │                         │             · Toggle：前台是 shell 类 → 吞键 + WM_QLR_HOTKEY
        │        │                         │             · Close ：前台是 shell 类 → 吞键 + WM_QLR_ESCAPE
        │        │                         │             · 其余 6 个：旁听，投 WM_QLR_HOTKEY(canonical_vk)
        │        │                         │
        │        └─ lens_binding() ────────┴─> app::lens_down()（GetAsyncKeyState 现查）
        │
        └─ settings 窗口（独立线程/独立窗口类 QLRust.SettingsWindow）
                 · 自绘：paint_settings 按 SETTINGS_ROW_HEIGHT 逐行画
                 · 点一行 → editing = Some(Edit) → 下一个按键直接当绑定
                 · Enter 保存：写 config.json → WM_QLR_SETTINGS_SAVED → 宿主重装绑定 + 更新 State.config
```

三条设计取舍：

- **为什么设置窗是普通可激活窗口，而预览窗是 `WS_EX_NOACTIVATE`**：预览窗的全部意义是"不抢焦点"
  （焦点留在资源管理器，空格继续可用）；设置窗必须收 `WM_KEYDOWN` 才能"按一下新键"，所以反过来。
- **为什么按键动作与进程间命令分成两个 `Action` 枚举**：`ipc::Action` 是可远程投递的命令
  （`--show`、`--quit`、`--settings`），`keymap::Action` 是"用户可改的按键语义"。混在一起会让
  "设置页里能改的项"与"命令行能投的命令"耦合，改 ABI 时就分不清哪些是契约。见 CONVENTIONS #68。
- **为什么改完要热更新而不是提示重启**：钩子绑定本来就是 `AtomicU32` 数组，重装一次只是 8 次 store，
  比重启进程便宜得多；用户改完立即按一下新键验证，反馈闭环最短。
