# 对比模式：忠实复刻 QuickLook 的多窗口 + 完整交互（路线 A）

> 目标：把「锁」做成 QuickLook 那样——锁住当前预览窗（冻结它显示的文件、不再跟随选中项），
> 并立刻另开一个**空的新驱动窗**；被锁窗保持**完整交互**（滚动/翻页/缩放/放大镜/表格点选/复制/拖边改大小/顶栏按钮）。
> 窗口阴影本阶段先注释掉。落地前不动代码，先过计划。

> **实现状态（2026-11-18，1.12.0）**：**P0–P5 全部完成**。会话化、锁/新窗、输入路由、渲染按窗口回传、阴影注释、文档、打包均已完成；`cargo test --workspace` 全绿、clippy `-D warnings`/fmt 干净、verify-docx/verify-pptx 未回归；新增 `scripts/verify-multiwin.ps1` 有桌面 GUI 段 **9 PASS / 0 FAIL**（真实双窗：锁 A 后 B 开在新窗口、两窗并存且各自出帧），并新增 `--pin` CLI 入口。

## 0. 语义（对齐 QuickLook 源码）

- **驱动窗（driver）**：全局只有"当前"一个。空格开/关、跟随资源管理器选中项、托盘、命令行都作用在它上面。
- **被锁窗（pinned）**：点顶栏「锁」= 把当前驱动窗"摘下"成被锁窗，并**立刻新建一个新的空驱动窗**（QL 的 `ForgetCurrentWindow()` = `Pinned=true; InitNewViewerWindow()`）。被锁窗可以叠多个。
- **跟随**：只有驱动窗跟随选中项；被锁窗永远显示锁定时那个文件。
- **空格**：永远作用于驱动窗（开关/换文件），不碰被锁窗。
- **关闭**：`×` / `Esc` 关掉"光标所在（无则前台）的预览窗"。关掉被锁窗只关它自己；关掉驱动窗后**自动新建一个空驱动窗**（QL 的 Closed 回调：驱动窗关→`InitNewViewerWindow`；被遗忘的 pinned 窗关→不重建）。
- **阴影**：本阶段注释掉（`shadow=None`），后续再彻底删除。

## 1. 架构：全局单态 → 按窗口会话

现状（`crates/qlr-app/src/app/session.rs`）：`STATE`(:552)、`LINES`(:555)、`CONTENT`(:557)、`RESULT`(:562)、`JOB_TX`(:559)、`RENDER_EPOCH`(:572)、`RENDER_TRACK`(:653) 全是全局单例；一切 `with_state/get_content/set_content`（约 363 处）都隐式指向"那一个窗口"。

改造：引入 `Session` 把"每窗口私有"的东西装进去，用注册表按 HWND 查；**把三个入口改成按"当前消息的窗口"解析会话**，因此 363 个调用点一个字都不用改。

```rust
// session.rs（新增）
pub(crate) struct Session {
    pub(crate) state:   Mutex<State>,                   // 原 STATE
    pub(crate) lines:   Mutex<Vec<String>>,             // 原 LINES
    pub(crate) content: Mutex<Option<Arc<Canvas>>>,     // 原 CONTENT
}
static SESSIONS: OnceLock<Mutex<HashMap<isize, Arc<Session>>>>;  // hwnd → 会话
static DRIVER:   OnceLock<Mutex<Option<isize>>>;                 // 当前驱动窗
thread_local! { static CURRENT: Cell<isize>; }                  // wndproc 顶部写入

pub(crate) fn enter_session(hwnd: isize);          // wndproc 顶部调用
fn resolve() -> Option<Arc<Session>>;              // CURRENT 有会话→用它；否则→DRIVER
// with_state / get_content / set_content / set_lines / get_lines / preview_raw 全改走 resolve()
```

要点：
- `State` 结构**字段不变**（它本来就是"一个预览会话的全部状态"，含 `preview`、`pinned`、`shell_preview`、`audio`、`video`、`wrap_cache` 等每窗口资源）；只是从"全局一个"变成"每会话一个"。
- `resolve()` 取会话时**先 clone 出 `Arc<Session>` 再释放注册表锁**，避免长时间持全局 map 锁（防死锁，沿用现有"锁内只改状态、`window::*` 放锁外"铁律）。
- `RESULT` → `static RESULTS: Mutex<HashMap<isize, RenderOutcome>>`（按目标窗口，见 §4）。
- `JOB_TX / RENDER_EPOCH / RENDER_TRACK` 保持**全局**（单渲染线程不动），任务里只多带 `target`。

生命周期函数（session.rs 新增）：
`new_session(hwnd, host, config) -> Arc<Session>`、`drop_session(hwnd)`、`set_driver(Option<isize>)`、`driver() -> Option<isize>`、`with_session(hwnd, || ...)`（临时改 CURRENT，供输入重定向用）。

## 2. 窗口创建与「锁」

- 复用 `window::create_preview_window(PREVIEW_CLASS, title)`（`window/mod.rs:137`，窗口类已注册一次、共用一个 `wndproc`）再建新窗。
- **启动**（boot.rs）：建 host → 建首个预览窗（= driver）→ `new_session` + `set_driver` → 其余（渲染线程/预加载/`start_timer`/`hotkey::set_preview`）照旧挂在 driver。
- **`show.rs::pin_current()`（新增）**：
  1. `old = driver()`；把 old 会话 `state.pinned = true`；停掉 old 的 `TIMER_FOLLOW`（不再跟随）。
  2. 建新预览窗 `w = create_preview_window(...)`；`new_session(w,...)`；`set_driver(w)`。
  3. 新窗挂 `TIMER_FOLLOW/WATCHDOG/SHEET_KEYS`；更新 `hotkey::set_previews(...)`；先不 Show（等选中项）。
  4. 位置：沿用位置记忆（同 `show_preview` 的自然定位）。
  - `chrome.rs:252` 的 pin 分支从"置 `s.pinned=true`"改为调用 `pin_current()`（保持单向：已 pinned 再点 → beep 无效）。
- **`show.rs::close_window(hwnd)`（新增，区别于 `hide_preview`）**：
  - 是 driver → `hide_preview()` 后**重建一个空 driver 窗**（`InitNewViewerWindow` 等价）。
  - 是被锁窗 → 对该会话 `hide_preview_of(hwnd)`（hide 自己的窗 + `reset_for_hide`，不碰 driver）→ `drop_session(hwnd)` → `window::destroy_window(hwnd)`。
- `hide_preview()` 改为"作用于当前会话"（经 `resolve()`），于是 `Action::Hide`、托盘、`×` 都只影响当前/驱动。
- `boot.rs` 的 `WM_DESTROY`（当前无条件 `window::post_quit()`）改为：**只有 `hwnd == host` 才 post_quit**；预览窗销毁只 `drop_session`。
- 平台新增 `window::destroy_window(hwnd)`（包 `DestroyWindow`），避免 HWND 累积。

## 3. 输入与热键路由（最高风险）

原则：**直接投递到某窗口的消息**（`WM_PAINT / WM_MOUSEWHEEL / WM_LBUTTON… / WM_TIMER / WM_HOTKEY` Ctrl+C）天然落在正确窗口（wndproc 顶部 `enter_session(hwnd)` 已解析）。只有**全局钩子经 host 转发**的消息要重定向：

- `WM_QLR_HOTKEY`（空格/翻页/缩放/放大镜…）：空格 → driver；其余"内容导航键" → **光标下的预览窗**（无则 driver）。
- `WM_QLR_WHEEL`、`WM_QLR_ESCAPE` → 光标下的预览窗，否则 driver（`WM_MOUSEWHEEL` 直投已是正确窗口）。

实现：
- `input.rs::preview_under_cursor() -> Option<isize>`：`window::window_under_cursor()` 取光标处 HWND → 取根窗 → 命中 `SESSIONS` 的键。
- `session::with_session(hwnd, || ...)` 包里跑 `on_wheel / on_hotkey / escape`。
- 平台 `hotkey.rs`：`PREVIEW_HWND` 单值 → `PREVIEWS: Mutex<SmallVec<isize>>` + `set_previews(&[isize])`；`cursor_over_preview()` / `foreground_belongs_to_preview()` 改为遍历集合；app 在开/关窗时同步这张表。
- 复制 Ctrl+C：`register_copy(hwnd)` 本就按窗口（`hotkey.rs:392`），搬到每个新显示窗（`show_preview` 里对 `s.preview` 注册、关闭时 unregister）。

## 4. 渲染结果按窗口回传

- `RenderJob` 加 `target: isize`（= 发起请求会话的 `s.preview`）。
- 单渲染 worker 不变；完成时 `RESULTS.insert(target, (generation, result))` 并 `PostMessage(目标预览窗, WM_QLR_RENDER)`（现在是 post 到 host，`render.rs:417`）。
- `on_render_done()` 在目标窗口 wndproc 跑（CURRENT=该窗），从 `RESULTS.remove(当前 preview)` 取；代际校验用该会话 `s.render_gen`（本来就是每会话字段）。
- 看门狗/租约 `RENDER_TRACK/RENDER_EPOCH` 仍全局（单 worker）；超时日志按请求方 `s.render_since`（每会话）读。
- `pagecache` 全局共享（按文件 key），多窗口命中是加分项，不改。

## 5. 阴影（先注释，零风险第一步）

- `boot.rs:209-221` 的 shadow `register_class`+`create_shadow_window` 整段注释，`shadow = None`。
- `sync_shadow/hide_shadow/allow_shadow/suppress_shadow` 已能容忍 `None`（`sync_shadow_with` 里 `else return`）；调用点保留无害。
- `State.shadow` 字段保留（恒 `None`）；后续清理阶段再连同 `shadow.rs` + 调用点一起删。

## 6. 改动清单（按文件）

| 文件 | 改动 |
|---|---|
| `session.rs` | 新增 `Session`/`SESSIONS`/`CURRENT`/`resolve`/`enter_session`/`new_session`/`drop_session`/`set_driver`/`driver`/`with_session`/`RESULTS`；三个入口改走 `resolve()` |
| `boot.rs` | 启动建 driver+session；`wndproc` 顶部 `enter_session(hwnd)`；`WM_DESTROY` 仅 host 退出；`WM_QLR_HOTKEY/WHEEL/ESCAPE` 重定向；计时器挂 driver；`hotkey::set_previews`；阴影注释 |
| `show.rs` | `show_preview/hide_preview` 作用于当前会话；新增 `pin_current()`/`close_window()`；follow 仅 driver |
| `render.rs` | `RenderJob.target`；`RESULTS`；`on_render_done` 按目标取；worker post 到 target |
| `chrome.rs` | pin 按钮改调 `pin_current()`（其余经 `s.preview` 已自动正确） |
| `input.rs` | `preview_under_cursor`/`with_session`；`on_wheel/on_hotkey/escape` 重定向 |
| 平台 `hotkey.rs` | `PREVIEWS` 集合 + `set_previews`；`cursor_over_preview/foreground` 遍历 |
| 平台 `window/mod.rs` | 新增 `destroy_window`、`window_under_cursor`（若缺） |
| 清理阶段 | 删 `shadow.rs` + `State.shadow` + `sync_shadow` 调用点 |

## 7. 风险与对策

- **输入路由到错窗（最高）**：拿不准一律回退 driver；建手动矩阵逐条验（见 §9）。
- **重入/死锁**：`resolve()` clone `Arc` 后放注册表锁；`window::*` 调用一律放会话锁外（沿用既有铁律）。
- **渲染结果串窗**：`RESULTS` 按 target + 代际校验。
- **计时器/热键泄漏**：关窗务必 `stop_timer` + `unregister_copy` + `drop_session`。
- **单窗零回归**：空格/跟随始终指向 driver，平时只一个窗口，行为与现在一致；P1 先跑全量测试证明这点再进 P2。

## 8. 分阶段（逐步验收）

- **P0** 阴影注释（10 分钟）。
- **P1** 会话化重构（不改行为，单窗照旧；363 调用点不动）：`session.rs` + `boot.rs` 入口 + `wndproc enter_session`；全量测试/verify 通过。
- **P2** 锁→新窗口：`pin_current`/`close_window`、driver 重建、pin 按钮接入；单窗+双窗手动验证。
- **P3** 输入路由：`preview_under_cursor`、hotkey 集合、wheel/esc/nav 重定向。
- **P4** 渲染结果按窗口：`RenderJob.target`/`RESULTS`。
- **P5** 收尾：清理阴影调用点、文档（STATUS/LOG/README/plan）、`verify-multiwin.ps1`、全量 clippy/test、打包。

## 9. 验收

**手动矩阵**：①开 A→点锁→A 冻结显示 A；②选 B 按空格→**新窗口**显示 B，A 仍显示 A；③鼠标悬 A 滚轮缩放/翻页只动 A，B 不动；④A/B 各自拖边改大小、放大镜、表格点选互不影响；⑤关 A 只关 A，B 在；⑥关 B（driver）→ 自动出新空 driver；⑦单窗常规路径与现在完全一致。

**自动**：`cargo test` 全绿（app/core/platform/render/plugin/…）；`verify-sheet/verify-docx/verify-pptx` 全 PASS；新增 `verify-multiwin.ps1`（用 `--show`/IPC 触发 pin + 第二个文件，断言出现两个预览窗 + 各自内容）。

## 10. 工作量

约 **3–4 人日**（含手动矩阵与文档）：P1 会话化 ~1d、P2 锁/窗口 ~0.5d、P3 输入路由 ~1d、P4 渲染回传 ~0.3d、收尾 ~0.5d。

## 已确认的语义（2026-11-18，用户拍板）

1. 「锁」保持**单向不可取消**（QL 行为）：点了就锁、不能取消，关掉该窗即释放。
2. 新驱动窗初始位置**沿用上次窗口位置**（位置记忆），选中文件时原位弹出。
3. `Esc` / 顶栏 `×` 关闭**光标所在的预览窗**（含被锁窗）；驱动窗关闭后自动新建空驱动窗。

## 11. 后续路线（本「对比模式」P5 之后再排期）

用户要求：**Office 相关的优化一律放在对比模式之后**（不在本计划内动手，仅登记）。按性价比排序：

1. **图片懒解码**（收益最大）：`qlr-render/src/docx.rs:3031`、`qlr-render/src/pptx.rs:1199` 的 `decode_image` 现在 parse 阶段对所有图片一次性 `load_from_memory` 全解码；改成按页/可见懒解码 + JPEG 1/2·1/4·1/8 缩放解码 + 去重 + LRU。当前 image-heavy 快照 430–440ms / 36MB 主要就是它。
2. **图表保真**：原生图表无预览图（62MB deck 有 3 个 `ppt/charts/chartN.xml`，仅 `<c:chart r:id>` 引用、无 blip 回退），只能自绘矢量（量少、几乎不吃内存）；OLE 内嵌对象通常自带 EMF/WMF，可用 `PlayEnhMetaFile` 近零内存；占位框仅兜底。
3. **连续滚动模式**（相对翻页的性能/内存代价先做评估再定）。
4. **各格式渲染缺口**：格式覆盖（`.xls/.doc/.ppt` 旧 BIFF、ODF 仍走系统处理器/插件兜底）；xlsx（隐藏行列解析了但未跳过绘制、标签栏不能横滚、表内图片/图表占位、公式引擎、图标集/打印布局）；docx（脚注尾注/批注/修订/多栏/复杂编号/图表浮动图环绕）；pptx（动画/切换/SmartArt、渐变只取首色）。
5. **进程外沙箱隔离**（ADR-0010，暂缓）。

> 这些都在「对比模式」P0–P5 完成并发布后，再单独立项、逐条评估。
