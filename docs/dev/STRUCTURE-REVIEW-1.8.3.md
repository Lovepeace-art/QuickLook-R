# QLRust 结构评审（v1.8.3，release/1.8.3 @ c956000）

> **注（2026-11-04）**：本文是拆分前快照。此后已落地 §6-4 裸 Win32 收口、`window.rs` 纯搬家拆成
> `window/` 子模块（单文件 ≤1600 行，verify-layers §3b 棘轮）、折行量宽、PaintBuffer 复用、
> PreviewBackend 契约 + Surface 路由 + 四态会话状态机。下列"平台层反向膨胀/结构债"条目仅作历史依据。

> 只读审计：未修改 `crates/` 下任何代码。行数以 `Get-Content .Count`（含空行）实测；
> "生产代码"= 各文件 `#[cfg(test)]` 之前的行。证据格式 `file:line`，可在当前 HEAD 直接跳转。

## TL;DR

1. **依赖方向成立，无反向依赖**；但 `qlr-render` 实际是"第二个平台层"（Cargo.toml 带 `windows`，pdf/media/svg 共 67 处 `unsafe`），与 `ARCH.md:21` 的"不依赖 windows"自述相反。
2. **`app.rs` 7705 行 / 247 个顶层 fn / 66 字段的 `State` / 17 个 `OnceLock`**，单文件承担约 20 类职责；`run`(531 行)、`on_hotkey`(438)、`repaint`(410)、`show_preview`(354) 四个函数占了 24%。
3. **"内容区尺寸"有 4 套独立算法**（`content_size_of` / `frame` / `render_box` / `view_size`），一致性靠 `app.rs:3002` 的注释口头维持，历史上已产出 `STRETCH 位图 1044x1477 != 内容区 1020x1479` 这类现场日志。
4. **`hide_preview` 持 `STATE` 锁调 `ShowWindow`**（`app.rs:4851-4855`），这是 #128 自等死锁的复现面，也是唯一还没收口的持锁动窗口点（另有 `app.rs:4643` 持锁整帧贴图）。
5. **panic 面近乎为零**（24 个生产文件 `unwrap/expect/panic!` 全为 0，唯一 `app.rs:1094` 是崩溃测试钩子），这是全仓最硬的强项；真正的风险在 `unsafe`（≈240 处）与**插件在主进程内以 `panic=abort` 运行**。
6. 渲染只有**一个** worker 且**不重建**（spawn 仅 `app.rs:1053` 一次），看门狗只能"请求取消"（协作式），插件/FFI 挂死 = 本次会话预览永久失效。
7. 平台层反向膨胀：`window.rs` ≈3.3k 行 / 140 个 `pub fn` 里塞了顶栏、设置页、目录、音频条、放大镜等**UI 绘制与布局决策**（`paint_chrome:1957`、`chrome_rects:1815`、`paint_settings:1610`）。
8. 优先级判断：**先修稳定性三处（持锁、几何单点、worker 重建），再动拆文件**；拆文件是"为了以后"，前两项是"为了这个版本不再被打回"。

---

## 0. 规模与分布（实测）

| crate | 行数 | 文件 | 关键事实 |
|---|---|---|---|
| qlr-app | 8970 | 5 + build.rs | `app.rs` 7705（生产 7004 / 测试 ~690）、`settings.rs` 682、`pagecache.rs` 255、`cli.rs` 226、`main.rs` 16 |
| qlr-platform | 6838 | 13 | `window.rs` ≈3.3k（48%）、`hotkey.rs` 775、`selection.rs` 562、`audio.rs` 420 |
| qlr-render | 7988 | 17 | `svg.rs` 1552、`html.rs` 948、`media.rs` 786、`markdown.rs` 593、`code.rs` 501、`pdf.rs` 472 |
| qlr-core | 2727 | 10 | `format.rs` 510、`config.rs` 446、`plugins.rs` 390（含 11 处 `unsafe`） |
| plugin-office | 961 | 1 | 18 处 `unsafe`，在主进程内跑 zip/image |
| plugin-api / demo | 202 / 261 | 1 / 1 | ABI 稳定，5 / 10 处 `unsafe` |

`app.rs` 生产 7004 行的职责分布（按连续行段实测，合计恰好 7004）：

| 行段 | 行数 | 职责 |
|---|---|---|
| 1–462 | 462 | 常量 / `State`(66 字段) / 17 个全局槽 / `with_state` |
| 463–1052 | 590 | `run`（531 行单函数）+ 启动 + 退出看门狗 |
| 1053–1532 | 480 | 渲染调度、worker、上一帧缓存 |
| 1533–1919 | 387 | 系统预览处理器 `IPreviewHandler` 编排 |
| 1920–2173 | 254 | 预热 / 预取 / 预热预算 |
| 2174–2302 | 129 | IPC 请求转交 |
| 2303–2724 | 422 | 缩放 / 滚动 / 滚轮 |
| 2725–3084 | 360 | 拖动移动 / 拖边改尺寸 |
| 3085–3454 | 370 | 跟随选中项 + UI 卡顿诊断 |
| 3455–3786 | 332 | 滚轮零头 / 右键缩放 / 放大镜 |
| 3787–3882 | 96 | 窗口尺寸决策 |
| 3883–4236 | 354 | `show_preview` |
| 4237–4427 | 191 | 位置/尺寸记忆 + 阴影同步 |
| 4428–4833 | 406 | 音视频播放 |
| 4834–4928 | 95 | 收起 / 退出 |
| 4929–5089 | 161 | 文本行与标题 |
| 5090–5403 | 314 | 布局 `Frame` / 页码框 / chrome 命中 |
| 5404–5869 | 466 | 压缩包树与列表 |
| 5870–6418 | 549 | `repaint` / 叠层 / `paint_code` |
| 6419–7004 | 586 | 托盘 / 设置 / 热键（`on_hotkey` 438）/ `wndproc` |

---

## 1. 分层是否成立

**依赖方向：成立。** 实测 `Cargo.toml`：`app → {core, platform, render, windows}`、`platform → {core, windows}`、`render → {core, image, jpeg-decoder, png, libloading, cfg(windows) windows}`、`core → {serde, serde_json, libloading, qlr-plugin-api}`。
`rg "^use qlr_"` 全仓无一处从 `core` 指向 `app`/`platform`/`render`，也没有 `render → platform`。**没有反向依赖**。

但"分层"在两个方向上都漏了，清单如下。

### 1.1 该在 platform、却写在 app 的 FFI（越层）

| 位置 | 问题 |
|---|---|
| `crates/qlr-app/src/app.rs:26-30` | app 直接 `use windows::Win32::...`（`HWND/LPARAM/LRESULT/RECT/WPARAM`、`SystemInformation::GetTickCount`、`Threading::{GetCurrentProcess,TerminateProcess}`），违反 AGENTS §2"app 只做编排" |
| `crates/qlr-app/src/app.rs:1004` | `unsafe { TerminateProcess(GetCurrentProcess(), 1) }`，退出看门狗直接杀自己，platform 无对应门面 |
| `crates/qlr-app/src/app.rs:1606, 1658, 1693, 4892, 5045` | `KillTimer` / `SetTimer` 裸调；而 `qlr-platform/src/window.rs:529 start_timer` / `:536 stop_timer` 已存在——**同一件事两套机制**（缺一个按 id 批停的封装就绕过去了） |
| `crates/qlr-app/src/app.rs:4926` | `PostQuitMessage(0)` 裸调（退出流程本该全在 platform 或全在 app 的一处） |
| `crates/qlr-app/src/app.rs:2227, 6643` | `GetTickCount()` 裸调（platform 已有 `Instant`/`now_ms` 语义的时间源） |
| `crates/qlr-app/src/settings.rs:219-220` | 设置窗 `unsafe extern "system" fn wndproc` + `unsafe` 块内 `dpi::enter`（窗口过程在 app 是设计，但 `unsafe` 面本可只留签名） |

代价：`app.rs` 的 `unsafe` 计数 12（含 `wndproc:6674`），与文件头注释"unsafe 只出现在窗口过程签名里"不符。

### 1.2 该"薄"却厚：platform 里的 UI 与业务决策（反向下沉）

`qlr-platform/src/window.rs` 140 个 `pub fn` 里，下列已不是"FFI 薄封装"，而是**绘制通道 + 布局决策**：

| 位置 | 问题 |
|---|---|
| `window.rs:1788 Chrome` / `:1805 ChromeRects` / `:1815 chrome_rects` | 顶栏视图模型与"谁挨着谁"的布局规则住在 platform；CONV #89 明确"只能由 chrome_rects() 一处决定"，但它所在的层不对 |
| `window.rs:1957 paint_chrome`（163 行）、`:1942 paint_chrome_strip` | 顶栏整条绘制（标题/页码/按钮状态） |
| `window.rs:1596 SettingsRow` / `:1610 paint_settings`（95 行） | 设置页行模型 + 绘制（`app/settings.rs` 只给数据） |
| `window.rs:1201 paint_list`（131 行）、`:2367 paint_toc` / `:2393 paint_toc_dc` | 压缩包目录树 / 列表 / 大纲 |
| `window.rs:2228 paint_audio_bar`（102 行）、`:2894 paint_lens` | 音频条 / 放大镜 |

`qlr-platform/src/hotkey.rs`：`:67 SHELL_CLASSES` 白名单、`:171/:197` 命中判定、`:214 foreground_is_shell()`、`:334 set_active()`、`:695/:704 set_capture_digits()/capture_digits()`。**"页码输入模式下吞数字键"是 app 的业务状态**，却以全局布尔住在平台。
`qlr-platform/src/tray.rs:68-72` 定义菜单 id（`SETTINGS:101/ABOUT:102/EXIT:103`）与中文标签（`:123-130` "设置…/关于/退出"），`app` 侧 `on_tray` 再按数字分支——**菜单语义被切成两半**。

### 1.3 该在 core/render、却被困在 app 的纯逻辑

`app.rs:5484 build_archive_tree`、`:5574 cmp_archive_nodes`、`:5581 sort_archive_ids`、`:5585 sort_archive_children`、`:5595 flatten_archive`、`:4971 extension_of`、`:4978 human_size`、`:2445 scroll_clamp`、`:3468 pixels_to_rows`、`:2702 wheel_take`、`:2993 content_size_of`。
这些不需要任何句柄，却只能在 Windows 上被测（`app.rs:7310 content_size_of_subtracts_chrome_height` 里还要调 `window::chrome_height()`）。AGENTS §3 的判断口诀"能在 Linux 上单测吗？能 → core"在这里一条都没落地。

### 1.4 文档与代码的双向漂移（最需要立刻修的一类）

| 文档断言 | 代码事实 |
|---|---|
| `ARCH.md:21`：render"不依赖 windows crate，纯 CPU" | `crates/qlr-render/Cargo.toml` 有 `cfg(windows) windows`；`media.rs:86/89 CoInitializeEx`、`:102 MFStartup`、`svg.rs` 10 处 D2D `unsafe` |
| `ARCH.md:27`：app 只有 `app/cli/main` 三文件 | 实际 5 个 src 文件（`app.rs/settings.rs/pagecache.rs/cli.rs/main.rs`）+ `build.rs`，`pagecache.rs` 是 1.8.x 新加的第 4 个，文档没跟上 |
| `ARCH.md:14`：core 不依赖 windows | 成立（但 core 依赖 `libloading` 并做 DLL 扫描：`plugins.rs` 340 行 + 11 `unsafe`，`lib.rs:8-12` 自述为有意例外） |
| `ARCH.md:16`：platform"只做 FFI，不做业务决策" | 见 1.2 |

结论：**"核心分层（app→platform→core / app→render→core）是成立的，但 render 事实上是第二平台层，platform 事实上是第二 UI 层。"** 与其改代码去迎合文档，不如先把文档改成事实，再加一条脚本断言（见 §6-9）。

---

## 2. `qlr-app` 是否过肥

**是。** 指标：7705 行 / 247 个顶层 `fn`（含 32 个 `#[test]`）/ `State` 66 字段 / 17 个 `OnceLock` / 287 处 `with_state(`。

### 2.1 按职责聚簇（可直接作为拆文件方案）

| 簇 | 行数 | 代表函数 | 建议文件 | 拆分价值 |
|---|---|---|---|---|
| A 全局状态 | 462 | `State:134`、`with_state:407`、17 个 `static` | `app/state.rs` | 必拆（所有簇的公共依赖，先独立才谈其他拆） |
| B 启动/退出 | 590 | `run:463`(531)、`on_render_watchdog:1321` | `app/boot.rs` | 高（`run` 531 行是最大单体） |
| C 渲染调度 | 734 | `spawn_worker:1053`、`request_render`、`maybe_prewarm:3355`、`take_prepared:2121` | `app/render.rs` | 高（+预热天然同族） |
| D 系统处理器 | 387 | `open_shell_preview:1618`、`sync_shadow_with` | `app/shell.rs` | 高（COM 边界集中，最容易接 #145 类回归） |
| E 输入手势 | 1114 | 缩放/滚轮 422、拖动改尺寸 360、零头/放大镜 332 | `app/gesture.rs` | 中（内聚但共享 `State` 多） |
| F 跟随/诊断 | 370 | `on_follow_timer:3301` | `app/follow.rs` | 中 |
| G 布局与绘制 | 1024 | `frame:5102`、`repaint:5906`、`paint_code:6123` | `app/paint.rs` | 高（与 C 的几何真源合并更好） |
| H 压缩包 | 466 | `build_archive_tree:5484`、`archive_row_index:5669` | `app/archive.rs` | **最高**（纯逻辑可下沉 core，零窗口依赖） |
| I 音视频 | 406 | `video_paint:4643` 等 | `app/media.rs` | 中 |
| J 几何记忆 | 287 | `window_size_for:3787`、位置/阴影 | `app/geometry.rs` | 高（几何真源应在此） |
| K IPC | 129 | `forward:2173` | `app/ipc.rs` | 低（行数少） |
| L 热键+托盘+wndproc | 586 | `on_hotkey:6591`(438)、`wndproc:6674` | `app/hotkey.rs` + `app/wndproc.rs` | 高（438 行单函数） |

### 2.2 拆分风险（按严重度）

1. **`State` 是事实上的 god struct（66 字段，`app.rs:134-380`）**，12 个簇全在读写它。直接按簇拆文件只会把"一屏 7000 行"改成"12 个文件共享一个 66 字段结构"，耦合度不变。**必须先按字段切数据所有权**（preview/render、shell、archive、audio、win/geometry、settings），否则拆分是纯搬字。
2. **17 个 `OnceLock` 静态**（`STATE:380`、`LINES:383`、`CONTENT:385`、`JOB_TX:387`、`RESULT:390`、`PENDING:393`、`LAST_FRAMES:1467`、`SHELL_TRASH:1555`、`PREPARED:1936`、`PREWARM_TX:1938`、`FOLLOW_SLOT:3085`、`DRAG_TRACE:3197`、`UI_TICK:3295`、`LAST_SHADOW_SYNC:4371`、`LAST_HIDE:4912`、`ARCHIVE:5434`…）：跨文件访问要么 `pub(crate)` 暴露，要么连同 `with_state` 一起搬进 `state.rs`——**这是拆 A 簇的第一步，也是唯一无风险的**。
3. **`wndproc:6674` 是单点分派**：所有 `WM_*` 都在这一个 `unsafe extern "system"` 里，拆文件时它只能留在 `app/wndproc.rs` 并继续 `use crate::paint::*; use crate::gesture::*;`——一个 200 行的 match 直接连到全仓 12 个模块，等于**把耦合从"同文件"变成"同 crate 跨模块"**，收益要打折。
4. **测试与实现同文件**（`app.rs:7017` 起 ~690 行、32 个 `#[test]`，含 `window::` 常量断言）：拆文件会把测试也撕开，`cargo test` 的模块路径全变，AGENTS §8 的"app 48 个测试"基线要重算。
5. `on_hotkey:6591` 438 行里混着"热键语义 + 前台闸门 + 托盘 + 设置窗开关"，1.8.3 刚动过前台闸门，**建议最后拆它**。

---

## 3. 重复与坏味道

### 3.1 "内容区尺寸"：4 套算法（最贵的重复）

| 位置 | 算什么 |
|---|---|
| `app.rs:2993 content_size_of` | `client − window::chrome_height()` |
| `app.rs:5102 frame` | 绘制/命中真源：`chrome_rects().content − 目录栏 − 滚动条`（`:5118/:5124 content.right -= window::scrollbar_w()`） |
| `app.rs:3017 render_box` | 渲染请求：扣滚动条再 × zoom（`:3024-3031`） |
| `app.rs:2311 view_size` | `frame().content × zoom`（`:2321` 注释："渲染盒必须等于 frame().content"） |

外围还有 `app.rs:3787 window_size_for`、`:3941 show_preview` 内联 `height - reserve`、`:2465 rows_in`、`:5779 archive_rows_per_screen`、`window.rs:1201 paint_list` 与 `app.rs:5669 archive_row_index` 两套行度量。
`app.rs:3002-3005` 注释原文承认三条路"必须给出同一个答案，否则……现场日志 `STRETCH 位图 1044x1477 != 内容区 1020x1479`"。
**这是靠注释+人工纪律维持的一致性，不是靠类型。** 量级：任何一次"扣不扣滚动条/扣不扣目录栏"的修改都要改 3–4 处。

### 3.2 顶栏字面量：4 份 `Chrome {}` 拷贝

`app.rs:5837`（压缩包通道）、`:5921`、`:5954`、`:5995` 各构造一次 `window::Chrome { .. }`，全文件 33 处 `chrome/chrome_rects` 引用。AGENTS §5.3 已把"改字段要全改"写成天条——**天条本身是坏味道的自白**；命中优先级同样写两处。

### 3.3 样式词汇：三套映射并行

`qlr_render::LineStyle → window::TextStyle` 的映射在 `app.rs:4997-5012`（9 个分支）与 `:6260-6268` **各写一遍**；`qlr_render::Token → TextStyle` 在 `:6500-6506` 第三遍；`line_bg` 颜色表 `:5014-5018` 重复。⇒ 新增一个 `LineStyle` 变体要改 3 处，漏一处就是"某种语法不着色"。

### 3.4 DPI：两套换算（1.8.3 有意为之，但已混用）

`dpi.rs:148 px()`（普通 DIP，96 恒等）、`:203 cpx()`（内容缩放，默认 `sqrt(dpi/96)`）、`:86 scaled(hwnd,dip)`（按窗口 DPI）。
混用实例：`app.rs:2866 ring.max(dpi::px(RESIZE_MARGIN_DIP) / 2)` 与 `:2868 dpi::scaled(hwnd, 8).max(dpi::px(RESIZE_MARGIN_DIP))`——**同一表达式里窗口 DPI 与线程 current DPI 混算**。
更隐蔽：`dpi.rs:96 thread_local!` + `:111 seed_thread` + `:116 current()` ⇒ `px()` 读的是**线程本地** DPI；预热线程算窗口尺寸时读到的是别的窗口的 DPI（`app.rs:3111-3115` 注释自承）。另 `app.rs:2306 fn scaled(value, zoom)` 与 `platform::dpi::scaled` 同名不同义。

### 3.5 `unwrap/expect/panic` 与 `unsafe`

生产段实测（脚本按 `mod tests` 切分，24 个文件）：

| 类别 | 结果 |
|---|---|
| `unwrap()` | **全部 0** |
| `expect()` | **全部 0**（除 `build.rs:1`） |
| `panic!` | 仅 `app.rs:1094`（`QLRUST_PANIC_TEST=1` 崩溃测试钩子） |
| `#[cfg(test)]` 内 panic | render 13 处（`text.rs:173/182/227/258/275/291`、`svg.rs:1473`、`media.rs:655/740`、`markdown.rs:645`、`image_view.rs:311`、`html.rs:968/999`） |

⇒ **panic 面≈零，全仓最强项**，值得写进 reviewer 指南保护它（新增 `unwrap` 就该被 clippy/review 拦）。

`unsafe` 分布（生产段）：`window.rs 90`、`pdf.rs 29`、`media.rs 28`、`hotkey.rs 24`、`selection.rs 20`、`plugin-office 18`、`shellpreview 12`、`app.rs 12`、`core/plugins.rs 11`、`svg.rs 10`、`tray.rs 10`、`plugin-demo 10`、`instance 8`、`plugin-api 5`、audio/crash/dpi/win11 各 3、`settings.rs 2`、integrity/code 各 1。

高危点排序：
1. `plugin-office` 18 处 + 第三方 `zip`/`image` 在主进程内解析，release `panic="abort"`（`Cargo.toml [profile.release]`）⇒ **插件一个越界写直接带走宿主**。
2. `render` 的 `pdf.rs/media.rs/svg.rs`（67 处）跑在 worker 线程，`SetCurrentPosition` 曾因 `guidtimeformat=NULL` 崩（#141）。
3. `app.rs 12` 处属越层（§1.1），其中 `TerminateProcess` 不可逆。

### 3.6 超过 100 行的函数

`app.rs`：`run:463`(531)、`on_hotkey:6591`(438)、`repaint:5906`(410)、`show_preview:3883`(354)、`on_render_done:1365`(104)、`open_shell_preview:1618`(104)、`on_chrome_click:5231`(101)、`maybe_prewarm:3355`(100)。
`window.rs`：`paint_chrome:1957`(163)、`paint_doc_with:1345`(151)、`paint_list:1201`(131)、`paint_bitmap_at:2703`(118)、`paint_shadow:205`(103)、`paint_audio_bar_dc:2241`(102)。
（`paint_settings:1610` 95、`paint_bitmap_scaled:2607` 81、`paint_toc_dc:2393` 75、`dispatch:3016` 71 为次一级。）

### 3.7 锁与"内容高度"的写法也各有多套

- **取锁**三套：`with_state`（`:407-411`）、手写 `x.lock().ok()`（`set_lines:413`、`get_lines:421`、`on_render_done:1367`、`take_prepared:2123`）、`.unwrap_or_else(|e| e.into_inner())`（`last_hide_slot:4915`）。加上 `prepared_slot()`/`last_hide_slot()` 这类"槽访问器"各写一遍 ⇒ 新增一个全局槽时写法不统一，poison 行为也不统一。
- **"扣掉多少"**散在三处：`app.rs:3003` 扣 `chrome_height()`、`:3941` 内联 `height - reserve`（还叠 `AUDIO_BAR_H`）、`:3024-3031` 扣滚动条再乘 zoom。`CHROME_HEIGHT`/`AUDIO_BAR_H`/`scrollbar_w()` 三个常量被 4 个函数各自组合，没有一处叫"这就是内容区"。

---

## 4. 状态管理

**形态**：`static STATE: OnceLock<Mutex<State>>`（`app.rs:380`），`with_state` = `STATE.get()?.lock().ok()?`（`:407-411`），另有 `set_lines:413` / `get_lines:421` / `get_content` 直接 `.lock().ok()`。全局共 **17 个 `OnceLock`**，其中 `LINES`/`CONTENT` 两把锁的分工（ARCH §4：先写 LINES 放锁再动窗口）是**正确且必要**的设计，不要动。

**粒度**：一把大锁覆盖 config/host/preview/visible/zoom/scroll/page/shell_preview/toc/video/audio/pinned，287 个 `with_state` 调用点。粒度粗不是问题本身（单线程 UI + 一次拷贝），问题是**粗锁里混进了窗口/COM 调用**。

**持锁做重活/回调的实际清单**（脚本对全部 `with_state(...)` 做括号平衡扫描后仅 3 处）：

| 位置 | 内容 | 判定 |
|---|---|---|
| `app.rs:3718` | `with_state(\|s\| (s.preview, window::client_rect(s.preview.raw()), s.zoom))` | 只 `GetClientRect`，不重入，可接受但应注释 |
| `app.rs:4643` | `video_paint` 持锁调 `window::paint_bitmap_fit_window` | **持锁整帧贴图**（4K 下几十 ms），期间任何 `with_state` 都阻塞 |
| `app.rs:4851-4855` | `hide_preview` 内 `with_state(\|s\| { window::hide(...); window::hide_shadow(...); s.visible=false; })` | **持锁调 `ShowWindow`** ⇒ 同步重入 `wndproc → WM_PAINT`，与 #128/#139 完全同型，**最高危** |

**中毒策略不一致**（同文件两种写法）：`:408 .lock().ok()?` ⇒ 一旦 poison 就**永久静默返回 `None`**（全功能哑火、无日志、无提示）；`:4915-4919 last_hide_slot()` 用 `.unwrap_or_else(|e| e.into_inner())`。两者并存意味着"到底哪儿会哑火"取决于调用点，不可推理。

**计时器与锁的交互**：`hide_preview` 故意不停 `TIMER_FOLLOW`/`TIMER_WATCHDOG`（`:4897` 注释，1.1.0 起的"窗口收着时投机预渲染"），`:4892` 只停 TOAST/SHELL_INPUT/SHELL_SWAP。这是**有意的设计**，但它是 §5.1 里"空闲 CPU 6.5 次唤醒/秒"的来源，两者要一起看。

---

## 5. 围绕三个目标的短板（按投入产出比排序）

### 5.1 高稳定性（最该先做）

| # | 短板 | 证据 | ROI |
|---|---|---|---|
| S1 | `hide_preview` 持锁调 `ShowWindow`，与 #128 同型 | `app.rs:4851-4855` | **极高**：改动 <20 行，消灭一类"点 × 卡死" |
| S2 | 锁中毒后静默哑火，且两种策略并存 | `:408` vs `:4915-4919` | 极高：统一为 `into_inner + log_error!`，<10 行 |
| S3 | 渲染 worker 单例且不重建；看门狗只 `cancel::cancel()`（协作式，FFI 阶段打断不了） | `:1053` 唯一 spawn、`:1321 on_render_watchdog`、`render/src/cancel.rs` 头部自述"单个 FFI 调用中途打断不了" | 高：插件/PDF 挂死 = 本会话预览永久失效，无自动恢复 |
| S4 | 插件在主进程内、`panic=abort`、18 处 `unsafe` + 第三方解析 | `Cargo.toml [profile.release]`、`plugin-office/lib.rs`、`core/plugins.rs` | 中高（人日大）：换进程隔离收益大但成本 8–15 人日 |
| S5 | `video_paint` 持锁整帧贴图 | `app.rs:4643` | 中：拖动/播放期间 UI 可能掉帧 |

### 5.2 高性能

| # | 短板 | 证据 | ROI |
|---|---|---|---|
| P1 | 每帧 WM_PAINT 都真量宽折行（`Measurer` + `push_wrapped`），滚动/缩放时重复排版 | `app.rs:6123 paint_code` → `:6146 Measurer::new` → `:6164 push_wrapped`（`:6288 paint_styled_with`） | 高：折行结果可按 `(width, 行区间)` 缓存，滚动场景直接省掉 |
| P2 | 每帧新建整窗尺寸 `PaintBuffer`（`CreateCompatibleDC` + `CreateCompatibleBitmap`），4K 窗 ≈33 MB/帧 | `window.rs:1137 PaintBuffer::new`，`:1215 paint_list`、`:1361 paint_doc_with` 各一份，Drop 才删 | 中高：改 1–2 个缓存 buffer，3–4 人日 |
| P3 | 顶栏/列表/音频条在 platform 里各自 `paint_*_dc`，与 `paint_chrome` 的条带绘制两套入口 | `window.rs:1942` vs `:1957`、`:2228` vs `:2241`、`:2367` vs `:2393` | 中：合并后少一次整条带走位 |
| P4 | 高倍缩放按"自然尺寸 × zoom"重新解码/重采样（`MAX_RENDER_PIXELS` 只兜底不优化） | `app.rs:1223`、`:3017 render_box` | 中：大图放大已有 #107 逐行边解边缩，但 zoom 变化仍整张重跑 |
| P5 | 文本通道无"按行窗口"渲染（`LINES` 全量持有） | `app.rs:383 LINES`、`:4929` 文本行 | 低：大文件已 `truncated`，暂不痛 |

### 5.3 低资源占用

| # | 短板 | 证据 | ROI |
|---|---|---|---|
| R1 | 隐藏态常驻两个计时器：`FOLLOW` 400ms + `WATCHDOG` 250ms ≈6.5 次/秒，每次都 `GetForegroundWindow`+类名比对、`drain_shell_trash()`、卡顿自检 | `app.rs:872/874`、`:3301 on_follow_timer` | 高：可降到 1–2 次/秒或改 `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` |
| R2 | 内存上限是**加法不是上限**：pagecache 64 MiB + 上一帧 3 张（`LAST_FRAME_CAP=3`）+ 预热 1 张 + 渲染中 1 张，单张最大 128 MB（`MAX_RENDER_PIXELS`） | `pagecache.rs:24`、`app.rs:1463`、`:1223`、`:1945-1951` | 高：缺一个"总预算"闸门，最坏峰值可接近 500 MB |
| R3 | 预热预算按"原始像素/文件大小"分别封顶，四套常量各自为政 | `app.rs:1945-1951` | 中：可合并成一张表 |
| R4 | `take_prepared` 未命中即清槽（对），但命中容差 ±2px 之外无统计，预热命中率不可观测 | `app.rs:2121-2140`（只有 `log_debug`） | 低：加一行计数即可指导预热策略 |
| R5 | GDI 对象每帧重建（同 P2），句柄峰值与窗口尺寸线性相关 | `window.rs:1137/1165` | 中（与 P2 同一改动的两个收益） |

---

## 6. 下一步（6–10 条候选，均不与 1.8.3 已完成项重复）

> 已排除：DPI 全量缩放+内容缩放、热键前台闸门、目录滚轮、滚轮零头、渲染盒扣滚动条、退出看门狗。

| # | 改进 | 目标 | 改动面 | 风险 | 人日 | 优先级 |
|---|---|---|---|---|---|---|
| 1 | **持锁动窗口清零**：`hide_preview`/`video_paint` 改"锁内取句柄 → 放锁 → 动窗口"；`with_state` 中毒统一 `into_inner` + `log_error!` | 稳定性 | `app.rs:4643`、`:4851-4855`、`:407-411`、`:4915-4919`，约 80 行 | 低（有 #128 现成范式；`ShowWindow` 顺序变了要跑 `smoke.ps1` 的"点 × 立刻再开"用例） | 1–2 | **P0** |
| 2 | **几何单点化**：新建 `app/layout.rs`（纯函数）统一 `frame/render_box/view_size/content_size_of/window_size_for`，一处产出 `Layout { client, chrome, content, render_box, rows }`；补 property test（任意 client/zoom 下 `render_box ⊆ content` 且 `view_size == frame.content×zoom`） | 稳定性 + 性能 | `app.rs:2993-3031`、`:5102-5130`、`:2311-2321`、`:3787-3882`、`:2465`、`:5779`、`:5669`，约 250 行 + 新增 ~150 行 | 中（碰所有绘制与命中；但可用"日志断言 STRETCH 不匹配"做回归闸门） | 4–6 | **P0** |
| 3 | **`app.rs` 拆文件（分三步）**：① 抽 `app/state.rs`（462 行，零风险）② 抽 `app/archive.rs`（466 行，纯逻辑，顺手下沉 `build_archive_tree` 到 core 可 Linux 单测）③ 抽 `app/shell.rs`（387 行 COM 边界）与 `app/render.rs`（734 行）。**不碰 `wndproc` 与 `on_hotkey`** | 可维护性（间接稳定性） | `app.rs` 7004 行 → 拆成 6 文件；测试模块跟着搬（32 个 `#[test]`） | 中高（`State` 66 字段跨模块、17 个 `OnceLock`、`pub(crate)` 可见性涟漪；`git blame` 断代） | 8–12 | **P1** |
| 4 | **裸 Win32 收口**：platform 补 `window::{stop_timer_by_ids, post_quit, terminate_self, now_ms}`，`app.rs` 去掉 `use windows::*` 里的 `Threading`/`SystemInformation`/`WindowsAndMessaging`，`unsafe` 12 → ≈2 | 分层整洁 + 稳定性 | `app.rs:26-30,1004,1606,1658,1693,2227,4892,4926,5045,6643`；`window.rs` +25 行 | 低（纯搬运，唯一要小心 `TerminateProcess` 的调用时机） | 1–2 | **P1** |
| 5 | **渲染 worker 可重建**：把 `JOB_TX/RESULT/PENDING` 收进 `struct Renderer`，看门狗超时（3 s）→ `cancel` 后再等 2 s 无响应则丢弃旧句柄、`spawn` 新 worker 并 `RESULT` 置错；UI 侧显示"渲染超时，已重置" | 稳定性 | `app.rs:1053-1223`、`:1321-1360`，约 150 行 | 中（旧线程若卡在 FFI 里会泄漏一个线程 + 占着 COM；需 `log_error!` 留痕，且要避免与 `cancel` 语义冲突） | 4–6 | **P1** |
| 6 | **折行结果缓存**：`paint_code` 把 `push_wrapped` 的 break offsets 按 `(text_id, width, style_rev)` 缓存到 `LINES` 侧，滚动时只做绘制；`Measurer` 句柄复用到一帧结束 | 高性能 | `app.rs:6123-6300`、`qlr_render::wrap`，约 120 行 | 中（缓存失效条件漏一个就是"改窗口宽度后换行错位"；需单测 + `verify-archive/verify-media` 回归） | 4–6 | **P1** |
| 7 | **`PaintBuffer` 复用**：按尺寸 LRU 保留 1–2 个 `PaintBuffer`（键=`(w,h)`），`paint_list`/`paint_doc_with` 共用；顺带统计每帧 GDI 对象数 | 低资源 + 高性能 | `window.rs:1137-1230`、`:1345-1382`，约 120 行 | 中（GDI 生命周期与 `Drop` 语义、`WS_CLIPCHILDREN` 下的重绘正确性） | 3–4 | **P2** |
| 8 | **空闲唤醒降频**：窗口隐藏时 `FOLLOW` 400ms→1500ms、`WATCHDOG` 250ms→1000ms（唤醒由 `hide_preview` 统一改），或用 `SetWinEventHook` 事件驱动替代轮询 | 低资源（CPU 空转） | `app.rs:872-880`、`:3301-3354`、`:4892-4900`，约 60 行 | 低–中（滚动跟随灵敏度会变，需实测 `on_follow_timer` 的响应延迟；1.1.0 的"收着时预渲染"要保留） | 2–3 | **P2** |
| 9 | **内存总预算闸门**：`pagecache 64 MiB + 帧缓存 + 预热` 统一到一个 `Budget`（如 256 MiB），超了先淘 pagecache 再拒预热；`log_info!` 打一行 `MEM used=…MB cap=…MB` | 低资源 | `pagecache.rs:24-60`、`app.rs:1223/1463/1945`，约 100 行 | 中（淘汰顺序错了会让翻页变慢；需 `bench.ps1` 前后对比） | 3–4 | **P2** |
| 10 | **文档-代码对齐 + 分层断言脚本**：改 `ARCH.md:14/16/21/27` 为事实描述；`scripts/verify-layers.ps1` 断言 ①`core` 无 `use windows::` ②`render` 的 `windows` 依赖只出现在 `media.rs/svg.rs` 白名单 ③`app` 不出现 `use windows::Win32`（白名单 `wndproc`） | 长期（防漂移） | `ARCH.md` 4 处、新增脚本 ~60 行、`README` 一行 | 低 | 0.5–1 | **P1（最便宜）** |

建议落地顺序：**1 → 10 → 2 → 8 → 4 → 3 → 6 → 5 → 9 → 7**（先修"会再被用户打回"的，再修"以后每次改动都要付利息"的）。

---

## 7. 如果只能做三件事

1. **修 `hide_preview` 的持锁动窗口 + 统一锁中毒策略**（`app.rs:4851-4855`、`:407-411`、`:4915-4919`）。1–2 人日，消灭与 #128 同型的一类死锁/哑火，是唯一"改动 <100 行、风险最低、对用户可见"的一项。
2. **把"内容区几何"收敛成一份纯函数 `Layout`**（`app.rs:2993/3017/2311/5102/3787` → `app/layout.rs`）。4–6 人日，一次消灭 4 套算法和 `STRETCH … != 内容区 …` 这类漂移 bug 的土壤，并让预热/绘制/渲染三条路从此不可能各算各的。
3. **给渲染 worker 一条可恢复路径**（`app.rs:1053` 单例 + `:1321` 只 `cancel`）。4–6 人日，把"插件或 PDF 卡住 = 整个会话预览失效"变成"超时后自愈 + 一行日志"，这是当前稳定性最大的单点。

> 不做拆文件也不影响上面三条；但拆文件（§6-3）如果做，请**从 `state.rs` 和 `archive.rs` 开始**：前者零风险，后者能把 466 行纯逻辑送到 core 的 Linux 单测覆盖下。

---

## 附录 A：评审方法（可复现命令）

```powershell
# 体积（含空行）
Get-ChildItem -Recurse -Include *.rs crates | % { "{0} {1}" -f $_.FullName, (Get-Content $_.FullName).Count }
# 函数/结构/impl 清单
rg -n "^fn |^pub fn |^struct |^enum |^impl " crates/qlr-app/src/app.rs
# 依赖方向（各 Cargo.toml 的 [dependencies] + 实际 use）
rg -n "^use qlr_|qlr_[a-z]+ = " crates -g "*.rs" -g "Cargo.toml"
# unsafe 计数
rg -c "\bunsafe\b" crates -g "*.rs"
# 生产段 panic/wrap：按 `mod tests` 切分后计数（本报告 §3.5 的数字来源）
#   for each file: cut = 第一个 `mod tests` 行号；只统计 [0..cut] 内的 unwrap()/expect(/panic!/
# 持锁动窗口：对每个 `with_state(` 做括号平衡截取闭包体，再 grep window::/unsafe
# 测试模块边界
rg -n "#\[cfg\(test\)\]" crates/qlr-app/src/app.rs crates/qlr-platform/src/window.rs
```

行数口径说明：`Get-Content .Count` 含空行，比 `Measure-Object -Line`（非空）略大，两者在 `app.rs` 上差 359 行；本报告统一用前者。

## 附录 B：与 CONVENTIONS.md 编号的对应

| 本报告条目 | 相关编号坑 | 关系 |
|---|---|---|
| §4 / §6-1 持锁动窗口 | #128 / #139 / #145 | 同一根因，`hide_preview` 是最新一处 |
| §6-2 几何单点化 | #78（绘制入口显式接内容矩形）、#89（顶栏只能由 `chrome_rects()` 决定） | #89 是本报告方案的"顶栏版"，§6-2 是"内容区版" |
| §3.1 四套内容区算法 | #66（设置页行高绘制与命中共用） | 同型："度量与绘制必须同源" |
| §3.6 `repaint` 410 行 | #78 | 绘制入口太少、参数太多 |
| §6-5 worker 重建 | — | 新增，建议占 #152 |
| §6-9 内存总预算 | #108 / #109 / #110（页缓存 key/LRU/预取） | 目前只有单点上限，缺全局预算 |
| §1.1 裸 `SetTimer` | — | 建议新增："平台已有封装就不许裸调" |
| §3.5 `unwrap` 归零 | — | 建议写成 review 规则，保护这个强项 |

