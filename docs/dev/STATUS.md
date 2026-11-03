# 进度（STATUS）

> 更新时间：2026-11-03 · **1.8.3-fix**（批次二，未发版）：把"每次重来"的开销收掉——
> **JPEG 按解码档位缓存**（滚一格不再重解码整张图；48MP 四档 367.6 → 78.1 ms、4K 170.0 → 90.5 ms）；
> **PDF 文档单槽缓存**（一次预览原本把文件读 3 遍 + pdfium 解析 3 遍，现在只读一次，
> `render_page` 3.22 → 1.25 ms）；**Markdown 排版按预算早停**（8 MB 首屏 356 → 16 ms）；
> **HTML 隐藏元素一次扫描剥离**（8 MB 预处理 145 → 8.8 ms；HTML 故意不早停，
> 否则状态行的"全文共 N 行"就不准了）；**缩放内循环去掉逐字节边界检查**
> （4K 一档 51.4 → 46.1 ms、48MP 100.6 → 71.2 ms，逐字节基线不变）；
> **新增 `alloc-stats` 特性量帧级分配**（默认不编：冷帧 65.7 MB / 热帧 5.6 MB）；
> **`bench.ps1` 补 PeakPrivate / GDI-USER / 帧级分配三列 + GDI 泄漏判据 + `-Passes` 浸泡**
> （4K 6 次开合 GDI 稳定 20，无泄漏）。
> 回归：`cargo test` **290 passed / 9 ignored**、fmt/clippy 零输出、`cargo build --release` 通过；
> `smoke.ps1` **16 PASS · 1 SKIP · 0 FAIL**、`verify-archive.ps1` **11 PASS · 0 FAIL**、
> `verify-pdf.ps1` **PASS**。明细见 §46；**200% 缩放视觉验收仍待用户真机**。
>
> <details><summary>1.8.3-fix 批次一（评审 7 项：收 5 项 + 自加 1 项，撤 1 项）</summary>
>
> **1.8.3-fix 批次一**：**扩展名不认识时的嗅探改成有界读**（`kind_for_path` 原本 `fs::read`
> 整个文件、只取前 512 B，GB 级 `.iso` 改名成 `.xyz` 就能把内存顶满）；**热键级 2 判据从死代码
> 变成真生效**（`GetFocus` 线程亲和、钩子线程恒 NULL，改用 `GetGUIThreadInfo` 的 `hwndFocus`，
> Win11 桌面 F2 就地改名不再吞空格），并补上钩子 stop 事件句柄的 `CloseHandle`；**压缩包目录前缀
> 只在新值时 `Box::leak`**（原本无条件 leak，连看 N 个压缩包进程内存线性涨）；**`PageCache` key
> 补文件戳**（mtime+len，预览中文件被覆盖后不再显示旧页，对齐 CONVENTIONS #112）；
> **`pack.ps1` 先生成 SHA256SUMS 再压缩**。评审建议的 `ShellPreview::close()` 的
> `std::mem::forget` 方案**没有收**——那个方法零调用点，`forget` 反而会漏掉
> `IPreviewHandler` 的 COM 引用。回归：`cargo test` **287 passed / 4 ignored**。
>
> </details>
>
> <details><summary>1.8.3 200% 缩放与空格偷按键</summary>
>
> **1.8.3**（2026-11-02）：**修好"Win11 200% 缩放下界面小一半"**——根因是窗口坐标是物理像素，而 30/24/15 这些几何量是照 100% 写的（顶栏只剩 15 DIP、正文 7.5 DIP）。17 个几何常量改成 DIP→物理像素的函数（161 处引用），字号表、PDF/图片的自然尺寸、预热线程一并换算；图片默认缩放 50% → 100%（老配置有 schema 1→2 迁移）。**空格热键不再偷按键**：修饰键改成精确匹配（`Ctrl+Space`/`Alt+Space` 不再被吞，`Shift+=` 单独放行），前台在输入文字时（桌面 F2 改名）直接放行；**收起预览时释放数字输入模式**（此前会一直吞数字键）。
> 回归：`cargo test` **264 passed / 3 ignored**、fmt/clippy 零输出、`smoke.ps1` 17 PASS（1 SKIP=管理员会话 UIPI）、**视觉基线 5 张截图与改前逐字节一致**（96 DPI 下必须零变化）。200% 视觉验收待用户真机。
> </details>
>
> <details><summary>1.8.2 图片/视频固定窗口大小</summary>

> **1.8.2**：**图片/视频不再能拖边改窗口大小**——看细节用滚轮缩放就够，窗口只在打开时按素材比例定一次（`resizable_kind()` + `resize_margin()` 返回 0 + `edges_at()` 短路，光标也不再变双向箭头）。最大化/外部改大小这类情况仍按"比例差 >2% 就等比居中"处理，不会把图拉变形。
> 回归：`verify-mediashape.ps1` **13 PASS · 0 FAIL · 0 SKIP**（图片 `640x390`、视频 `640x442` 拖边前后一致）、`verify-office.ps1` 23 PASS（docx/pptx 拖边仍能改大小）、`verify-archive.ps1` 11 PASS、`verify-media.ps1` 15 PASS、`smoke.ps1` 全过、`cargo test` **251 passed**、fmt/clippy 干净。
>
>
> </details>
>
> <details><summary>1.8.1 Excel 点选单元格 / 媒体比例锁定</summary>
> **Excel 预览里左键能点选单元格了**——真因是焦点只给到 Excel 的容器层 `EXCEL9`，画表格的 `EXCEL7` 从来没拿到 UI 激活；改成让处理器自己设焦点（`IPreviewHandler::SetFocus`），并且只能放到"输入同步呼叫退栈后"的定时器里调（`WM_ACTIVATE`/`WM_SETFOCUS` 里会被 `0x8001010D` 拒，`WM_MOUSEACTIVATE` 里会把这一次点击吃掉）。**图片/视频改成锁窗口比例**，**交给系统处理器的类型不再画文字占位页**。
> </details>
>
> <details><summary>1.8.0 改大小不留白边</summary>
> Office 预览改窗口大小不再留白边——把新矩形推到输入同步呼叫退栈后、**锁外**调 `SetRect`（持锁调用会把消息泵回自己的窗口过程 → 自等成 `Application Hang`），并把投影窗口也在 `WM_SIZE` 里同步（修"画框和阴影对不上"）。Office 交互回归：滚轮 / 横向滚动条拖动 / Shift+滚轮 全 True。
> </details>
>
> <details><summary>1.7.9 四类新预览</summary>
> 内置预览 4→8 类：音频（MCI）、视频（Media Foundation 逐帧）、SVG（D2D）、HTML（轻量排版）；图片改等比居中；修三处"点一下卡死/没反应"。`verify-media.ps1` 15 PASS。
> </details>
>
> <details><summary>1.7.8 焦点修复</summary>
> 点一下预览窗就把焦点交给处理器的内容子窗口 → Excel 滚轮/横向滚动条/Shift+滚轮立刻可用。
> </details>
>
> <details><summary>1.7.7 Excel 预览卡死</summary>
> `DoPreview` 之后紧跟的 `SetWindow`（OLE 就地激活对象）会让 Excel 自己等自己，进程
> `Responding=False`、只剩占位页、滚动条全废。改成"矩形在打开前算好 + 打开后不进 COM"，
> 就绪判断从"死等 400 ms"改成轮询"内容占满 50%"（Excel 实测 768 ms → 121 ms 就绪）。
> </details>


## 1. 一句话

**1.0.9 已交付**（用户九轮实测反馈全部修完）：内置四类（图片 / PDF / 文本 / Markdown）全部能预览，其余格式走插件（进程内 C ABI v1 + 示例插件）；
M6 硬化项全部落地——每用户安装器（免管理员、123 个扩展名关联、开机自启）、Win11 圆角/backdrop（Win10 静默降级为亚克力）、
可无人值守的基准脚本与预算门、318 例确定性变异 fuzz + 畸形输入回归、97 个合成样本 + 55 个真实文件全通过的样本矩阵、仓库 README 与四张截图。
`cargo test` 160 passed、`fmt`/`clippy -D warnings` 0、`smoke.ps1` 17 PASS + 1 SKIP、`bench.ps1` 无 FAIL、`sample-matrix.ps1` 100%。仓库处于可安全中断状态。

**M1 壳层 + 全局热键 + 选中项通道全部打通**：冷启动、单实例转发、亚克力无边框窗口、托盘、日志、配置、`WH_KEYBOARD_LL` 全局热键（带心跳自愈）、Shell 选中项读取（资源管理器多标签 + **桌面**）都已实现并实测通过。**唯一仍缺人工确认的是"真实手工按空格"（自动化按不出真空格，注入键被钩子自己过滤掉）。**

## 2. 里程碑

| 里程碑 | 内容 | 状态 |
|---|---|---|
| M0 | 可行性 + 同机对拍 + Rust spike | ✅ 完成（上级目录 `docs/bench-compare.md`、`spike/`） |
| **M1** | 壳层 + 热键 + 选中项（A1.0–A1.10） | 🟡 **代码全绿，差人工按键确认** |
| M2 | 三大高频族（图片/文本/PDF）+ 性能基准复测 | ✅ 完成（0.2.0，A2.0–A2.10） |
| M3 | 归档/代码/字体/CSV/DB 等 | 🟡 部分转内置（1.7.3 压缩包目录树、1.7.9 源码着色）；**字体仍是缺口**，其余走插件 |
| M4 | Office 走 `IPreviewHandler` + 视频 | ✅ 完成（1.7.4 Office 预览处理器 + PPT 翻页接管；1.7.9 视频画面 + 进度条，无声音） |
| M5 | 插件体系 | ✅ 完成（0.5.0，收窄为进程内 C ABI v1 + `plugins\*.dll` 扫描 + 示例插件） |
| M6 | 安装器/Win11 兼容/基准进 CI/安全/文档/样本矩阵 | ✅ **完成（1.0.0，见 §13）** |

## 3. M1 任务级进度（WBS A1.x）

| 编号 | 任务 | 状态 | 备注 |
|---|---|---|---|
| A1.0 | workspace 骨架 + release profile（LTO/panic=abort/strip） | ✅ | release exe 0.33 MB |
| A1.1 | 窗口类与预览窗（无边框/NOACTIVATE/TOOLWINDOW） | ✅ | `QLRust.PreviewWindow` |
| A1.2 | 亚克力材质（Win10 未文档化路径） | ✅ | `SetWindowCompositionAttribute` 动态解析 |
| A1.3 | 单实例 + 请求转发 | ✅ | 互斥体 + `WM_COPYDATA` |
| A1.4 | CLI（裸路径/Toggle/--show/--hide/--quit/--ping） | ✅ | 对齐上游 `App.xaml.cs:148-159` |
| A1.5 | 热键钩子 `WH_KEYBOARD_LL` | ✅ | 独立线程 + 30 s 心跳自愈 + 注入过滤 + 类名白名单；ADR-0005 |
| A1.6 | 选中项通道（Shell COM） | ✅ | 逐条枚举 `IShellWindows` + 同根匹配；**桌面走 `SWC_DESKTOP`（0.1.4）**；ADR-0006 |
| A1.7 | 托盘图标 + 右键菜单 + 气泡 + 双击开合 | ✅ 代码完成 | 自定义图标（0.1.3）；**0.1.5 查清 `0x80070005` 的真因：exe 放在 DSH 工作目录里会被打 Low 完整性标签**（坑 #29），启动日志加 `令牌完整性=`、失败时弹框说明 |

| A1.8 | 自研文件日志 + panic hook + 轮转 | ✅ | |
| A1.9 | 配置读写（JSON/原子写/钳制/便携目录） | ✅ | 新增 `hotkey_enabled` |
| A1.10 | 端到端冒烟脚本 | ✅ | `scripts/smoke.ps1` **15 项**（含渲染断言）+ `scripts/probe-selection.ps1` |
| A1.11 | 设置界面 | ⬜ 未开始 | |
| A1.12 | 自更新 | ⬜ 未开始 | |

## 3.1 M2 任务级进度（WBS A2.x）

| 编号 | 任务 | 状态 | 备注 |
|---|---|---|---|
| A2.0 | 格式注册表（扩展名 → 查看器类别） | ✅ | `crates/qlr-core/src/format.rs`，**308 个扩展名**，25 个 `Kind`，冲突按上游插件 `Priority` 裁决 + 人工 override；6 条单测 |
| A2.1 | `Viewer` trait + 渲染结果类型（Bitmap / Text） | ✅ | 新 crate `qlr-render`（不依赖 windows）；`Canvas::{Bitmap, Text(TextDoc)}`；ADR-0007 |
| A2.2 | 图片渲染器（解码 → 缩放 → BGRA） | ✅ | `image` 0.25（10 种格式）+ `thumbnail` 盒式缩放 + 就地 RGBA→BGRA；**4K 53–54 ms**、48MP 峰值待量 |
| A2.3 | 图片渲染器 A8.1（按显示尺寸缩放解码） | ✅ JPEG | `jpeg-decoder` DCT 域缩放（1/2^n）；48MP RenderMs 171→**66**、峰值 150→**12.8 MB**（已达 ≤60 MB 预算）；其他格式仍走整图解码 |
| A2.4 | 文本渲染器（编码探测 + 行切分 + 上限保护） | ✅ | BOM/UTF-8/UTF-16 探测 + 8 MB 读入上限 + 20 万行上限 + 单行 2000 字符上限；mmap 未做（8 MB 上限下收益很小） |
| A2.5 | 文本：滚动 + 可视区虚拟化 + 折行 + 大文件硬化 | ✅（0.2.0） | 滚轮/PageDown/↑↓ 滚动、只画可视区几十行、**自动折行（只算当前屏）**、PageDown 按折行后吃掉的实际原始行前进；未引入 `cosmic-text`（省 1–2 MB，见 CONVENTIONS #38） | 26 MB 文本 121,425 行，RENDER 21 ms、峰值 21.7 MB；折行夹具 300 行 → 每屏吃 8 行 |
| A2.6 | PDF 渲染（手写 PDFium FFI + 自备 `pdfium.dll`） | ✅ 渲染 / 翻页 / 页数 / 中文错误 | 200 页 PDF 翻页均值 6.3 ms、最慢 9 ms；坏 PDF 不崩 |
| A2.7 | worker 线程 + 代际号（丢弃过期帧） | ✅ | 常驻线程 `qlr-render`；`RESULT` 槽 + `WM_QLR_RENDER` + `render_gen`；ADR-0007 |
| A2.8 | 交互：缩放、平移、翻页、Esc 关闭 | ✅ 滚轮缩放（1.25×/格，最多 4 格）+ 键盘 +/-、拖拽平移 + 方向键、PageDown/PageUp 翻页、Esc 收起、点一下收起；页缩略图待做 | 放大后按目标像素重栅格化（`fit_scaled`）；826×1169 渲染 2–3 ms；滚轮/Esc 走 `WH_MOUSE_LL`/`WH_KEYBOARD_LL` 旁听 |
| A2.9 | 基准复测（对齐 `docs/bench-compare.md` 三项夹具） | ✅ | 见 `docs/dev/bench.md`：4K 67–86 ms / 48MP 190 ms / 文本 34 ms（感知） |
| A2.10 | `scripts/bench.ps1` 自动化 | ✅ | 自动找夹具 + 逐夹具重启常驻 + 采样峰值 RSS + 写 `bench-results.json` |

## 4. 当前实测指标（1.0.0 release，Win10 19045 / i5-12400F）

| 指标 | 值 | 预算 | 采集方式 |
|---|---|---|---|
| 分发体积 | exe **1,740,800 B（1.66 MB）**；dist 目录 **9.3 MB**（含 `pdfium.dll` 7.6 MB + 示例插件 104 KB）；zip **4,758,589 B** | exe ≤5 MB | `scripts/pack.ps1` |
| 壳层就绪（进程内到窗口+托盘+钩子） | **4–5 ms** | ≤20 ms | `scripts/bench.ps1`（日志 `壳层就绪 window_created=`） |
| 端到端冷启动（含进程创建） | **84 ms** | — | `scripts/smoke.ps1` |
| 空闲常驻内存 | **7.6–8.8 MB** working set | ≤10 MB | `scripts/bench.ps1` |
| 4K JPEG（8.3 MP）渲染 / 感知 / 峰值 | **32–35 ms / 50–69 ms / 17.5–18.1 MB** | 感知 ≤100 ms ✅ | 同上 |
| 48 MP JPEG 渲染 / 感知 / 峰值 | **58–70 ms / 73–83 ms / 16.7–16.8 MB** | 峰值 ≤60 MB ✅ | 同上 |
| 26.6 MB 文本（121,425 行）渲染 / 感知 / 峰值 | **18–30 ms / 37–50 ms / 26.2–37.1 MB** | 不 OOM ✅ | 同上 |
| 8 页 PDF 渲染 / 感知 | **7 ms / 36 ms** | — | 同上 |
| 200 页 PDF 翻页 | 均值 6.3 ms（最慢 9 ms） | — | 手工流程（`docs/dev/bench.md` §9） |
| 第二实例转交耗时 | 44 ms（含进程创建） | — | `scripts/smoke.ps1` |
| 样本矩阵通过率 | 合成 **97/97**、真实壁纸 28/28、真实文档 7/7 | ≥98% | `scripts/sample-matrix.ps1` |
| 单测 | **116 通过**（app 18 / core 28 / platform 11 / plugin-api 4 / plugin-demo 7 / render 41 / malformed 7） | 全绿 | `cargo test` |
| clippy / fmt | 0 警告 / 0 差异 | 0 | `cargo clippy --all-targets -- -D warnings`、`cargo fmt --all --check` |

对照 QuickLook 4.5.0：同机冷启动 ~1240 ms、常驻 96 MB、分发包 122.5 MB。

> **提醒（坑 #29）**：以上数字都要在**工作目录之外**的副本上量（`dist\`、桌面、`%LOCALAPPDATA%`）。
> 工作目录内的 exe 被 DSH 打了 Low 完整性标签，托盘与 Shell 选区必然 `0x80070005`，但**预览功能本身不受影响**。

## 5. 已知问题

1. **托盘图标在提权环境下不可用**（`Shell_NotifyIconW` → 0x80070005）。非提权正常；冒烟脚本 SKIP 而不是 FAIL。
2. **`WM_PAINT` 里每次绘制都写一行 debug 日志**（`wm=0x…`）。默认 info 级别不产生 IO；将来密集重绘要记得降级。
3. **没有 `wndproc` 级别的 SEH 兜底**：Rust panic 会被 hook 记进日志，但 FFI 层的访问违例只能靠事件日志排查（`DrawTextW` 空串那次就是这么查出来的）。`[计划]` A8.7 会加 SEH 兜底与 P99 看门狗。
4. **`state.visible` 与真实窗口状态可能不同步**：NOACTIVATE 窗口收不到 Alt+Tab / 任务栏关闭通知，只在 `WM_CLOSE` 时同步。
5. **热键不做"松手收起"**：当前是"按下触发 toggle"，QuickLook 是"按住 750 ms 出窗、松手消失"。短按语义差异待 M2 与产品形态一起定（钩子已能拿到 `WM_KEYUP`，只是没接）。
6. **选中项只在 Shell 系文件管理器与桌面上有效**（Explorer 含多标签、桌面；DOpus/Everything 通道未实现）。桌面依赖 `FindWindowSW(SWC_DESKTOP)`，进程刚起来时 Shell 可能未注册好，已做 3 轮 × 15 ms 重试。
7. **集成测试必须把 exe 复制到沙箱外**（工作目录内 exe 令牌被降 → Shell COM 0x80070005），见 CONVENTIONS 坑 #9。

## 6. 下一步（1.0.0 之后，按收益排序）

> 1.0.0 已交付，用户要求的四项（内置四类 / 插件体系 / M6 / 打磨）全部完成。下面是不在本次范围内、按收益排序的候选：

1. **人工确认 M1 Gate**（任何时候都能做，不阻塞开发）：选中 jpg → **真按空格** → 应出图片。
   自动化按不出真空格（钩子刻意过滤注入键），这是唯一需要人手的验收项。
2. **插件生态**：按 `docs/dev/PLUGINS.md` 把视频缩略图、Office、压缩包、字体、CSV/DB 等逐个做成插件；
   宿主侧只需新增"按扩展名扫描"的分发逻辑，核心不用改（ABI v1 已留好）。
3. **代码签名**：拿到证书后按 `docs/dev/RELEASE.md` §3 走一遍，SmartScreen 立刻转正。
4. **`A1.6` 兜底**：UIA `SelectionPattern`（200 ms 超时）+ 鼠标命中测试，覆盖 DOpus/Everything 之类非 Shell 管理器。
5. **折行质量**：字距调整 / 连字 / 双向文字（要引字体库，会破 5 MB 预算，需先决策）。
6. **PDF 页缩略图 / 文本 mmap**：体验向，非预算项。

## 7. 怎么验收当前状态

```powershell
cargo clippy --all-targets -- -D warnings   # 0 警告
cargo test                                   # 57 passed
cargo build --release
pwsh -NoProfile -File scripts/smoke.ps1 -Exe target\release\qlrust.exe     # 14 PASS + 1 SKIP
pwsh -NoProfile -File scripts/probe-selection.ps1                          # PASS（需要真实桌面会话）
```

期望：冒烟 15 项里 14 PASS + 1 SKIP（托盘，提权环境）；probe-selection PASS。任何 FAIL 都不要推进。

> `probe-selection.ps1` 会自己 `Start-Process explorer.exe "/select,<夹具>"`，需要交互式桌面会话；在纯 CI 环境跑不了（这也是它没进 CI 的原因）。








---

## 8. M2 收口（2026-10-30，0.2.0）—— 按用户要求在此停止

用户指令（m09431）：「做到 m2 后先停止后续进程」。因此 **M2 就是本次交付的终点**，
M3（音视频/归档/Office/3D）、M4、M5、M6 与插件体系**一律不启动**，除非用户明确要求继续。

### 8.1 M2 交付清单（全部实测）
| 能力 | 状态 | 关键数字 |
|---|---|---|
| 图片预览（10 种格式 + JPEG 缩放解码） | ✅ | 4K 渲染 41 ms / 感知 87 ms / 峰值 12.5 MB；48MP 77 ms / 95 ms / 12.3 MB |
| 文本预览（编码探测 + 折行 + 滚动 + 虚拟化） | ✅ | 26.6 MB 文本 RENDER 21 ms / 感知 53 ms / 峰值 21.7 MB；121,425 行 |
| PDF 预览（手写 PDFium FFI + 翻页 + 缩放 + 平移） | ✅ | 200 页 PDF 翻页均值 6.3 ms、最慢 9 ms |
| 交互（空格/Esc/滚轮/拖拽/方向键/PageDown） | ✅ | 滚轮一格 1.25×、拖拽 3 px 阈值、`NAV_KEYS` 只旁听不吞键 |
| 渲染线程 + 代际号 | ✅ | 常驻 `qlr-render` 线程，过期帧按代际号丢弃 |
| 单实例 / 托盘 / 亚克力 / 热键 / 选中项（含桌面） | ✅（M1） | 冷启动 50–85 ms、空闲 7.6–8.8 MB |
| 自动化验证 | ✅ | `smoke.ps1` 17 PASS + 1 SKIP；`cargo test` 75 passed；clippy 0 |

### 8.2 已知遗留（M2 范围内明确不做）
- 折行是朴素实现（字符/空格断行），无字距调整、连字、双向文字；`cosmic-text` 未引入。
- PDF 页缩略图未做。
- 文本 mmap 未做（8 MB 读入上限下收益很小）。
- `scripts/bench.ps1` 在「上一轮宿主还活着」时会把请求转交给老实例，导致 `FAIL(无 RENDER)`
  （2026-10-30 复现并加了 `--quit` 前置，但仍未彻底稳定）；**M2 的数字来自手工流程实测**，
  脚本修复留作 M2 之后的第一件事（见 CONVENTIONS #37）。
- A1.6 第三/四级兜底（UIA `SelectionPattern` / 鼠标命中测试）未做。

### 8.3 恢复工作的正确方式
1. 先读 `docs/dev/HANDOFF.md`（十分钟上手）。
2. 跑三条验证：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test`，
   再跑 `scripts/smoke.ps1 -Exe target\release\qlrust.exe`（需要真实桌面会话）。
3. 读 `docs/dev/CONVENTIONS.md` 的 #1–#39 坑清单，尤其 #9/#29（沙箱外跑）、#30（FFI 借用缓冲）、
   #32（StretchDIBits 原点）、#37（基准脚本）、#38（折行宽度必须等于绘制宽度）、
   #39（工作区 exe 是 Low 完整性，测试夹具别写 `%TEMP%`）。

## 9. 0.3.0 方向变更：内置四类 + 插件扩展（2026-10-31，按用户要求）

### 用户要求（原话）
> m345先停止，去除掉对其他格式的支持，目前先主要支持图片、pdf、文本、md等4个格式，
> 把后边的m6跑完，把整个项目打磨完整，后续其他格式以插件的形式单独开发，支持到项目里

### 落地结果（代码已完成，文档本节记录）
- `crates/qlr-core/src/format.rs` 整文件重写：`Kind` = `Image` / `Pdf` / `Text` / `Markdown` /
  `Unsupported`（5 个）；扩展名表 308 → 约 150；新增 `all_extensions()`（文档与样本矩阵共用真值）；
  新增**内容嗅探兜底**（无扩展名/未知后缀时读文件头 512 B，认 PDF/位图魔数，否则按"无 NUL 且基本可打印"判文本）。
- `crates/qlr-render/src/lib.rs`：`viewer_for` 只留三臂；不支持时错误文案带插件提示。
- `crates/qlr-app/src/app.rs`：`Kind::Unknown` → `Kind::Unsupported`；占位横幅改成四类清单 + 插件提示。
- 验证：`cargo test` 77 passed、clippy 0、fmt 0；实机 `clip.mp4` → `kind=unsupported renderable=false`、
  `note.md` → `kind=markdown renderable=true`。

### 接下来的顺序（M6）
1. ~~Markdown 轻量排版~~ → ✅ **0.4.0 已完成，见 §10**。
2. 插件体系骨架：`qlr-plugin-api`（C ABI v1：`plugin_version` / `can_render` / `render` / `free`）+
   `<exe>\plugins\*.dll` 扫描 + 一个示例插件（演示接管 `.mp4` 之类的占位格式）+ `docs/dev/PLUGINS.md` + ADR-0008。
3. A6.1 安装器（per-user 免管理员）+ 卸载 + 文件关联 + 开机启动（可实测）。
4. A6.3 Win11 兼容（DWM 圆角/backdrop，失败静默降级到 Win10 亚克力）。
5. A6.4 修 `scripts/bench.ps1` 宿主生命周期 + 基准 JSON 进 CI；A6.5 畸形输入安全回归 + 简易 fuzz；
   A6.7 样本矩阵（每类 ≥20 个真实文件）；A6.6 README/截图。
6. 收口 1.0.0：版本三处同步、dist + zip + 校验和、`docs/dev` 全量同步、提交与交付。

### 明确不做（与用户指令一致）
- 音视频（LAVFilters 那条路）、Office（`IPreviewHandler`）、归档、字体、DB、PE/ELF、CHM、缩略图……
  一律不在内置范围；它们以插件形态回归。
- A6.2 代码签名：**没有证书**，只写文档说明（未签名 exe 会被 SmartScreen 提示）。

## 10. 0.4.0 Markdown 排版（2026-10-31）

### 成品
打开 `.md` 不再是"一堆纯文本"：`# 标题` 变大变金、围栏代码块等宽淡蓝且**围栏行不显示**、
`>` 引用变灰并带 `▎` 前缀、列表归一成 `•`、`---` 画成一条 40 字宽的灰横线。
长行也照常折行（按各自样式的字号量宽），滚动/翻页与纯文本完全一致。

### 实现要点（改哪了）
| 层 | 文件 | 改动 |
|---|---|---|
| 渲染 | `crates/qlr-render/src/markdown.rs`（新） | `layout()` 纯逻辑分类 → `(Vec<String>, Vec<LineStyle>)`；`render()` 复用 `text.rs` 的 8 MB/20 万行预算 |
| 渲染 | `crates/qlr-render/src/canvas.rs` | `LineStyle` 8 变体；`TextDoc.styles: Option<Vec<LineStyle>>`（纯文本为 `None`，零分配） |
| 平台 | `crates/qlr-platform/src/window.rs` | `TextStyle` / `StyledRow` / `paint_styled()` / `Measurer::width_styled()`；`paint_text` 退化为薄包装 |
| 壳层 | `crates/qlr-app/src/app.rs` | 带样式时逐原始行折行并记录每行样式；`to_text_style()` 是唯一转换点；`STYLED` 日志 |

### 设计取舍（为什么这么做）
- **两套样式枚举**（渲染 `LineStyle` / 平台 `TextStyle`）：渲染层不能依赖 windows（要跨平台单测），
  平台层不该依赖某个渲染实现。中间一次映射，两边独立演进。
- **行高仍固定 24 px**：`visible_rows()` / 翻页 / 虚拟化都按常量算，变行高会牵一发动全身；
  所以靠"字号 ≤18 + 字重 + 颜色"区分层级，而不是靠行高。
- **只用 `CreateFontW`、不引字体库**：零额外依赖（A8.5 的 exe 体积预算），失败退回系统 GUI 字体。
- **不做行内强调**（`**粗**`/`` `代码` ``/表格对齐/HTML）：那需要行内切片 + 分段绘制，
  收益小、风险大；真需要时由 CommonMark 插件接管（见插件体系）。

### 验证
- 实机夹具 `md-demo.md` → `kind=markdown renderable=true`、`RENDER gen=1 text bytes=639 ink=20`、
  `STYLED 屏幕 20 行（样式分布：h1=1 h2=1 h3=1 code=4 quote=1 list=3 rule=1 normal=8）`，
  与夹具逐项对上；截图 `_bench/md-styled.png`。
- `cargo test` **87 passed**（app 17 / core 22 / platform 8 / render 40）、`fmt` 0、`clippy -D warnings` 0。
- 新增单测：8 条 markdown 分类/截断/端到端；`LineStyle` 索引与声明顺序一致（上屏侧查表依赖它）。
## 11. 0.5.0 插件体系（2026-10-31）

### 成品
内置四类之外的一切格式，都可以由**一个 DLL** 接管。装法就是把 dll 扔进 `plugins\`：
```
qlrust.exe
pdfium.dll
plugins\qlr_plugin_demo.dll     ← 示例：接管视频扩展名，画色条占位图
```
按空格预览 `.mp4` 会走插件（`SHOW … plugin=示例插件：视频占位图`），
内置四类仍然走 Rust 实现（插件抢不走 `.png`/`.pdf`/`.txt`/`.md`）。

### 三层在哪里
| 层 | 文件 | 职责 |
|---|---|---|
| 契约 | `crates/qlr-plugin-api/src/lib.rs` | `QlrPluginInfo`/`QlrImage`、三个函数类型、`ABI_VERSION`、扩展名归一化 |
| 宿主 | `crates/qlr-core/src/plugins.rs` | 扫描/加载/校验/查表/渲染/拷贝/回收 |
| 派发 | `crates/qlr-app/src/app.rs` | `render_with_plugin()`；内置优先，插件补缺 |
| 示例 | `crates/qlr-plugin-demo/src/lib.rs` | 视频占位图（也是 PLUGINS.md 的模板） |

### 关键数字
- 插件 DLL 示例 **101 KB**；**主 exe 仍是 1.73 MB**（`libloading` 本来就在用，PDFium 就是运行时加载的）。
- 插件渲染 1190×669 实测 **2 ms**（含跨界拷贝 3.18 MB）。
- 单测 **104 passed**（新增 plugin-api 4 / plugin-demo 7 / core 6）。

### 设计红线（写死在这里，避免以后被"顺手改掉"）
- **内置优先**：插件只补 `Kind::Unsupported`，不允许覆盖四类内置。理由见 ADR-0008。
- **跨界数据一律拷贝**：插件分配 → 宿主拷贝 → 立刻 `free`。不做共享内存/引用计数。
- **坏插件不致命**：加载失败只记日志 + 占位文字提示，宿主照常启动。
- **严格 ABI 版本**：不等就跳过，不做兼容层。
- **进程内**：崩溃会带走宿主（已写入 PLUGINS.md 的"只装信任的插件"）。

### 顺手修的 bug
`SHOW … elapsed=` 原来用进程启动时刻算延迟，常驻实例上会报 `elapsed=6583ms`。
改成每次请求重置的 `State.request_started`，现在报的是真实的按键到出窗延迟（0–2 ms）。
## 12. 0.6.0 安装器 + Win11 观感（M6：A6.1 / A6.3）

### 成品
| 能力 | 入口 | 验证方式 |
|---|---|---|
| 免管理员安装 | `scripts/install.ps1` | 实跑：装到 `%LOCALAPPDATA%\Programs\QLRust`，`令牌完整性=high`、托盘 rect 正常 |
| 卸载 | `scripts/uninstall.ps1` | 实跑：进程、123 个关联、Capabilities、快捷方式、目录全清，无残留 |
| 文件关联 | 安装器自动注册 | `HKCU:\Software\Classes\QLRust.File` + `.png\OpenWithProgids` + `RegisteredApplications` |
| 开机启动 | `install.ps1 -Autostart` | `HKCU:\...\Run` 值 = `"<exe>" --hide` |
| 扩展名真值导出 | `qlrust.exe --list-extensions <文件>` | 输出 123 行，与 `format::all_extensions()` 一致 |
| Win11 圆角/backdrop | `qlr-platform::win11` | Win10 上静默失败并记日志（本机 `build=19045 圆角=false`） |
| 打包 | `scripts/pack.ps1` | `dist\qlrust-0.5.0-win64\` + zip 4.76 MB + `SHA256SUMS.txt` |

### 为什么安装器是 PowerShell 而不是 WiX/Inno
- 本机没有 Inno/NSIS，也不方便为一次性的 per-user 安装引入构建依赖；
- WBS 要求的是「per-user、免管理员、可卸载、能注册关联」，PowerShell 脚本能全部满足，
  而且**可测**：刚才那些验证就是脚本自己跑出来的；
- 代价：用户要装要敲一行 `pwsh -File scripts\install.ps1`。若以后要单文件 setup.exe，
  用 Inno 包一层即可，脚本可以原样复用（ADR 级别的取舍，暂不单开 ADR）。

### 装到 `%LOCALAPPDATA%` 之后的一个副作用（好的那种）
DSH 工作区里的 exe 会被打上 **Low 完整性**，托盘与 Shell 选区全被 UIPI 拒（坑 #29）。
安装到工作区外的目录后 `令牌完整性=high`、`托盘图标已添加 rect=(1683,1040)-(1707,1080)` ——
**这是本项目第一次在自动化里看到托盘真的注册成功**。以后要验托盘/UIPI 相关的行为，
用安装版而不是工作区里的 release。
## 13. 1.0.0 收口（M6：A6.2 / A6.4 / A6.5 / A6.6 / A6.7）

### 成品清单

| 项 | 落地物 | 实测 |
|---|---|---|
| A6.4 基准 | `scripts/bench.ps1`（重写）、`scripts/make-fixtures.ps1`、`scripts/bench-budget.json`、`scripts/bench-budget-ci.json`、`.github/workflows/bench.yml` | 10 项全 OK、exit 0；冷启动 4–5 ms；4K JPEG 35/69 ms；26.6 MB 文本 18–30/37–50 ms |
| A6.5 安全 | `crates/qlr-render/tests/malformed.rs`（7 条，含 318 例确定性变异 fuzz）、`docs/dev/SECURITY.md` | 全绿；顺手修掉 UTF-16 被误判二进制 |
| A6.6 文档 | 仓库 `README.md`（重写）、`docs/screenshots/preview-{markdown,image,pdf,text}.png`（1190×748）、`packaging/README.txt` | 新用户三步上手 |
| A6.7 矩阵 | `scripts/sample-matrix.ps1` + `scripts/make-fixtures.ps1 -Matrix` | 合成 97/97 = 100%（image 31 / text 24 / markdown 22 / pdf 20）；真实文件 28 张壁纸 100% + 7 个真实文档 100% |
| A6.2 签名 | `docs/dev/RELEASE.md` | 无证书，只做文档：SmartScreen 表现、SHA-256 核对、将来 Azure Trusted Signing / signtool 步骤 |

### 关键数字（release，1.0.0）

- exe **1,740,800 B（1.66 MB）**，`FileVersion 1.0.0.0`；dist 含 `pdfium.dll` 7,586,816 B 与 `plugins\qlr_plugin_demo.dll` 103,936 B；zip **4,758,589 B**。
- `cargo test` **116 passed**（app 18 / core 28 / platform 11 / plugin-api 4 / plugin-demo 7 / render 41 / malformed 7）。
- `smoke.ps1` 17 PASS + 1 SKIP（托盘 UIPI，提权会话固有限制）。
- 安装版实机：`令牌完整性=high`、托盘注册成功、插件 `loaded=1 extensions=8`、
  Markdown `STYLED 屏幕 30 行（h1=1 h2=2 h3=1 code=3 quote=1 list=6 rule=1 normal=15）`、`--quit` 后 `leftover: 0`。

### 为什么 bench 单独一条 workflow

`ci.yml` 只做 fmt/clippy/test/体积预算（分钟级）；`bench.yml` 要起真实 GUI + 生成夹具 + 跑三轮，
慢一个数量级，所以放独立 workflow（push master + 每周一 03:00 UTC + 手动触发），
门限用 `bench-budget-ci.json`（比本机预算再放宽约 2 倍）——它的职责是**拦住"慢一个数量级"的回归**，不是卡 5% 抖动。

### 遗留（都在"非目标"里，不是缺陷）

- 无代码签名证书（A6.2 已文档化）。
- 设置界面 / 自更新未做（WBS 里本就没有）。
- 折行无字距/连字/双向文字；Markdown 不做行内强调。
- M1 的"真实手工按空格"仍需人验（自动化注入键被钩子刻意过滤）。

## 14. 1.0.1 用户实测三缺陷修复

用户在 1.0.0 安装版上实测报了三件事，逐条修掉并发 1.0.1。

| # | 用户原话 | 真身 | 修法 |
|---|---|---|---|
| 1 | "文本按上下的时候会这样，翻页逻辑好像不对"（截图：两屏文字叠成糊） | `WM_ERASEBKGND => 1` + `InvalidateRect(..., bErase=false)` + `SetBkMode(TRANSPARENT)` ⇒ 没人刷底，旧字形留在客户区 | `paint_styled` 每次先 `FillRect` 刷 `TEXT_BACKGROUND_RGB`；行起点由"垂直居中"改 `rc.top + TEXT_PADDING_TOP`（居中会让行数一变整屏平移） |
| 1b | （同上，自动化测不出来） | `WM_QLR_WHEEL` 直接走 `wheel_zoom`，绕过"文本先滚" | 两条入口汇到 `fn on_wheel(delta)`：先 `scroll_text_by`，滚不动才缩放 |
| 1c | "一开就是裁切过的" | `show_preview` 不重置 `zoom`/`pan`，新文件继承上一个文件的缩放 | `with_state(|s| { s.zoom = 1.0; s.pan = (0,0) })` |
| 2 | "会抢文件的打开方式，直接变成预览" | 旧 `install.ps1` 默认就写 ProgID + `Applications\qlrust.exe` + **123 个扩展名 OpenWithProgids**；`-Default` 还写 `HKCU\Software\Classes\.<ext>` 默认值（**HKCU 优先于 HKLM**，实测 ShellExecute 直接拉起预览） | 默认**零关联**；`-Associate` 才登记候选（不写默认值）；`-Default` 必须配 `-Associate` 且先备份；每次安装跑幂等 `Clear-LegacyAssoc` 修复旧机器；装完自检并 `throw` |
| 3 | "我需要按原文件尺寸自适应打开大小" | 开窗尺寸固定按 `width_ratio/height_ratio`（0.62×0.72） | `qlr_render::natural_size(path, kind)`（图片读头部 `dimensions`、PDF 首页 `page_size` × 96/72）+ `window::centered_rect_px(work, w, h)`（夹在 `[240×160, 工作区×0.92]` 居中）；文本/MD 无自然尺寸，仍走比例 |

### 实测（安装版 / 便携版同一套夹具）

- 重影判定用"滚动一圈再滚回来，截图 `Ink` 像素必须精确回到原值"：15100 → 15183 → 15283 → **15100**；
  日志 `SCROLL 0 -> 12` → `12 -> 24` → `24 -> 0`。
- 开窗尺寸：`png_1920x1080.png` → `1766x956（原始 1920x1080）`；`pdf_8page.pdf` → `793x956（原始 793x1123）`；
  `note.md` → 比例 `1190x748`（无自然尺寸）。
- 滚轮 12 次（`wheels=12`）在文本上不再产生 `ZOOM` 日志。
- 关联：卸载摘除 123 条 OpenWithProgids + 2 条 MRU；默认安装后"扩展名默认值命中 0 / UserChoice 92 项前后一致"；
  `-Associate` 后只进候选（ProgID=True、OpenWithProgids=123、默认值命中 0）；双击链路 .jpg→照片、.pdf→Chrome、.md→Obsidian 全部保持原样。
- 关卡：`cargo test` **120 passed**（app 18 / core 28 / platform 14 / plugin-api 4 / plugin-demo 7 / render 42 / malformed 7）、`fmt` 0、`clippy -D warnings` 0。

### 顺手修的两个安装器缺陷（不是用户报的）

- 旧 `uninstall.ps1` 是 **UTF-8 无 BOM**，而 UninstallString 用 `powershell.exe`（PS 5.1 按 ANSI 读）⇒ 解析 5 个错误，
  "控制面板卸载"本就是坏的。两个脚本改为 **UTF-8 with BOM**，UninstallString 带 `-ExecutionPolicy Bypass`。
- 47 个扩展名（`.jpg .png .gif .tif .zip .7z .rar` 等）的 `HKCU\Software\Classes\.<ext>` 默认值是**空字符串**，
  会屏蔽 HKLM 的 `jpegfile`/`pngfile` 等登记。来源是第三方关联工具（`.7z/.rar/.cr2` 等 QLRust 从未支持过），
  **按"别动别人的"原则未清** —— 需要用户点头才动。

---

## 15. 1.0.2 窗口跟随内容与缩放（用户第二轮反馈）

> 用户原话：「左右的黑边还是很多，没有做到窗口自适应，而且窗口最大是有限的」→「窗口跟随图片的缩放而缩放」

### 成品

| 改动 | 位置 | 说明 |
| --- | --- | --- |
| `natural_rect` 取代 `centered_rect_px` | `crates/qlr-platform/src/window.rs` | 一次算统一 scale，保持宽高比；用满工作区（去 8% 余量） |
| `zoomed_rect` 新增 | 同上 | 内容尺寸等比夹进工作区；不设最小尺寸（缩小时窗口要真的变小） |
| `State.base` 新增 | `crates/qlr-app/src/app.rs` | zoom 的基准 = 开窗尺寸，不再是自然尺寸 |
| `set_zoom_to` 重写 | 同上 | 先按 `base × zoom` 改窗口，再按同一尺寸渲染；位图比窗口大时可平移看局部 |
| `clamp_render_size` 新增 | 同上 | 单帧渲染夹到 32 MP（128 MB BGRA），高倍缩放不吃爆内存 |
| `begin_request` 新增 | 同上 | 真实热键/鼠标路径也重置 `request_started`（修 `elapsed=27050ms` 假延迟） |
| `RenderRequest.upscale` 新增 | `crates/qlr-render/src/{lib,image_view}.rs` | 缩放时允许**放大渲染**（`fit_within` 原本 `scale.min(1.0)` 永不放大 → 原来"放大"只是裁剪一份原图） |

### 实测（1920×1080 / 工作区 1920×1040）

| 内容 | 1.00× | 0.80× | 1.25× |
| --- | --- | --- | --- |
| 800×1200 竖图 | 693×1040（零黑边，位图 693×1040） | 554×832 | 窗口夹到 693×1040，位图 866×1300（平移看局部） |
| 1920×1080 横图 | 1849×1040（16:9 精确贴合） | 1479×832 | 夹到 1849×1040 |
| PDF 793×1123pt | 734×1040 | 587×832 | — |
| 文本 / Markdown | 1190×748，不跟缩放（按行滚动，没有"自然尺寸"概念） | 不变 | 不变 |

关卡：`cargo test` 121 passed、`fmt` 0、`clippy -D warnings` 0。
新增坑：#52（夹尺寸必须一次算 scale）、#53（缩放基准 = 开窗尺寸）、#54（用户样本可能含隐私，别截屏演示）。
---

## 16. 1.0.3 鼠标交互重做（用户第三轮反馈）

> 用户原话：「删掉鼠标左键关闭的功能，以后用左键来拖动，来解决文件长的翻页下滑问题。在pdf里，滚轮用来上下滑动」
> 「pdf的缩放可以用鼠标右键拖动来实现」

### 交互契约（这是现在唯一真相，改之前先看 CONVENTIONS #55）

| 输入 | 文本 / Markdown | 图片 | PDF |
| --- | --- | --- | --- |
| 左键单击 | 无动作 | 无动作 | 无动作 |
| 左键拖动 | 按行滚动（攒够 24 px 滚一行） | 平移 | 平移 |
| 右键上下拖 | 忽略 | 缩放（60 px = 1.25×） | 缩放 |
| 滚轮 | 一格 3 行 | 放得下→缩放；放大后→上下滑动 | 上下滑动（1/4 屏/格），到页底继续滚→翻页 |

### 代码落点

| 改动 | 位置 | 说明 |
| --- | --- | --- |
| 删 `on_preview_click` + `Config.hide_on_click` | `crates/qlr-app/src/app.rs`、`crates/qlr-core/src/config.rs` | 左键不再关窗；配置项一并删除（不留"改了没效果"的开关） |
| `drag_residual` / `pixels_to_rows` | `crates/qlr-app/src/app.rs` | 拖拽滚动攒像素零头，慢速拖动也能滚 |
| `zoom_anchor` / `zoom_drag_target` / `on_zoom_drag_*` | 同上 | 右键上下拖 = 缩放，增量式、<2% 变化不重渲染 |
| `on_wheel` 重写 + `scroll_page_by` | 同上 | 返回 `(动了没, 顶到边界没)`，PDF 到边翻页、图片放得下才缩放 |
| `step_page` 里 `pan` 归零 | 同上 | 翻页后从新页顶端看起 |
| `WM_RBUTTONDOWN/UP` 两臂 | 同上 | `WM_MOUSEMOVE` 改走 `on_mouse_move`，右键优先分流 |

### 实测（`_bench/verify-input.ps1`：真鼠标事件注入 + 日志断言）

| 断言 | 结果 |
| --- | --- |
| 左键单击后窗口仍在、日志无 `HIDE` | ✅ |
| 左键拖动 200 行文本 → `SCROLL 0→1→2→3→4→5` | ✅ |
| 滚轮在文本上 → `SCROLL` 且无 `ZOOM` | ✅ |
| 右键上拖 150 px → `ZOOM 1.00→…→1.75`（渲染 725×1087→1211×1817） | ✅ |
| 放大后滚轮 → `VSCROLL pan.y 0 -> 260（内容 1816px / 窗口 1040px）` | ✅ |
| PDF 滚轮 → `PAGE 2/8`；放大后先 `VSCROLL` 再 `PAGE 3/8` | ✅ |

关卡：`cargo test` 124 passed、`fmt` 0、`clippy -D warnings` 0。
新增坑：#55（鼠标语义只有一处真相 + 拖拽必须攒像素零头 + 删开关要删干净）、#56（漏 import 的 `WM_*` 会变成绑定模式 → `unreachable pattern`）。

## 17. 1.0.4 拖动改成挪窗口 + 滚轮方向修正（用户第四轮反馈）

> 用户原话：「鼠标左键拖动对图片来说，应该是整个窗口在桌面的移动，不然只是针对里边的图片的话，会频闪。pdf的右键拖动同理。鼠标滚轮到文本/md的拖动放方向逻辑反了」

### 成品

| 输入 | 文本 / Markdown | 图片 | PDF |
| --- | --- | --- | --- |
| 左键拖动 | 按行滚动（内边距不变） | **整个窗口在桌面上移动** | 同左 |
| 右键上下拖 | 忽略 | 缩放（60 px = 1.25×） | 缩放（修掉抖动） |
| 滚轮 | 一格 3 行，**向上滚 = 看上面的内容** | 放得下→缩放；放大后→上下滑动 | 上下滑动，到边继续滚→翻页 |

### 三处根因

1. **频闪**：1.0.3 的拖动改的是"位图绘制偏移"（`pan_pixels`），每帧重画一整张位图，
   合成器在半成品上取帧 → 闪。改成 `window::move_by()`（`SetWindowPos` 带
   `SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE`），拖动期间零重绘。
2. **拖 90 px 只走 27 px**：`WM_MOUSEMOVE` 的 client 坐标与"窗口自己也在移动"互相抵消。
   改为 `window::cursor_pos()`（`GetCursorPos` 屏幕坐标），位移与鼠标 1:1（实测 90,70 精确）。
3. **滚轮方向反了**：1.0.3 写 `scroll_text_by(notches * 3)`，向上滚反而往下翻。
   改 `wheel_text_rows(notches) = -notches * 3`（纯函数 + 单测 `wheel_up_scrolls_towards_the_top`）。

顺手修的第四个：右键拖动缩放的锚点原来用 client y，缩放会改窗口位置（居中）→ 锚点漂，
表现为 `ZOOM 1.05 -> 0.87 -> 1.17 -> 0.91 …` 来回抖；锚点改屏幕 y 后单调 `1.00 → 1.75`。

### 代码落点

| 改动 | 位置 |
| --- | --- |
| `move_by(hwnd, dx, dy)` / `cursor_pos()` | `crates/qlr-platform/src/window.rs` |
| `drag_origin()`（屏幕坐标优先）/ `move_window_by()` / `wheel_text_rows()` | `crates/qlr-app/src/app.rs` |
| 删 `pan_pixels`（不再有人调） | `crates/qlr-app/src/app.rs` |
| 单测 `wheel_up_scrolls_towards_the_top` | `crates/qlr-app/src/app.rs` |

### 实测（`_bench/verify-input-104.ps1`：真鼠标注入 + 位置/日志断言）

| 断言 | 结果 |
| --- | --- |
| 图片左键拖 90,70 → 窗口位移 `90,70`，期间无 `RENDER`/`PAN`/`ZOOM` | ✅ |
| 文本左键拖动 → 有 `SCROLL`、窗口位置不变 | ✅ |
| 滚轮向上 → 行号 65 → 62；向下 → 62 → 65 | ✅ |
| PDF 左键拖 -80,50 → 窗口位移 `-80,50` | ✅ |
| PDF 右键上拖 150 px → `ZOOM 1.00 → … → 1.75`（单调，无抖动） | ✅ |

关卡：`cargo test` **125 passed**（app 21 / core 28 / platform 16 / render 42 + malformed 7 + plugin-api 4 + plugin-demo 7）、
`cargo fmt` 0、`cargo clippy --all-targets -- -D warnings` 0。
新增坑：#57（拖内容会频闪、拖窗口不会）、#58（自己在挪窗口时 client 坐标不能用，缩放锚点同理）。
## 18. 1.0.5 缩放保位 + PDF 缩放不卡 + 文本窗宽 + Markdown 真排版

> 用户原话：「图片：移动后再缩放，会回到初始打开的位置，要调整到不回到初始打开的位置。pdf：右键缩放的时候卡顿比较强，按逻辑来说应该是缩放窗口大小的，现在是缩放文档，再调整窗口。文本/md：默认窗口有点大，宽度设置成一个合适的数值吧，上下左右再调整就行，另外，md的预览，需要是渲染后的格式，不能是源码格式，每行左边再加一个行号方便定位，鼠标左键也需要一个拖动整个窗口的功能」

### 成品（1.0.5 起的交互）

| 输入 | 文本 / Markdown | 图片 | PDF |
| --- | --- | --- | --- |
| 左键拖动 | **整个窗口移动**（不再滚动） | 整个窗口移动 | 整个窗口移动 |
| 右键上下拖 | **按行滚动**（窗口不动） | 缩放 | 缩放（拖动期间只改窗口，松手才重渲染） |
| 滚轮 | 一格 3 行 | 放得下→缩放；放大后→上下滑动 | 上下滑动，到边继续滚→翻页 |
| 每行左侧 | **行号**（折行续行不带），宽度固定 64 px | — | — |

### 五处根因与改法

1. **缩放跳回中央**：`set_zoom_to` 用 `window::zoomed_rect(work, …)` 按**工作区**重新摆位置。
   改 `window::resize_around_center(hwnd, w, h)`（以窗口自身中心为锚点）+ `clamp_into` 推回工作区。
   实测：拖 120,80 后缩放，中心偏移 `0,0` px。
2. **PDF 缩放卡顿**：拖动每帧都 `request_render`，一页 PDF 几十毫秒 → 卡。
   改为拖动期间只改窗口 + 旧位图**拉伸**（`paint_bitmap_scaled`、`State.zoom_preview`），
   松手 `on_zoom_drag_end` 补一次渲染。实测 12 个 ZOOM 帧 → 1 次 RENDER。
3. **文本窗过大**：宽高都跟着屏幕比例（1080p → 1200×750）。改 `window::text_rect()`：
   宽度固定 `TEXT_WINDOW_WIDTH = 880`、高度仍按比例 → 880×748。
4. **Markdown 是源码**：`markdown.rs` 原来只去块级标记。新增 `inline()` 去掉全部行内标记
   （代码、强调、链接、图片、转义、HTML），新增表格与任务列表，新样式 `LineStyle::Table`。
5. **没有行号**：`StyledRow` 加 `number`，左侧 64 px 行号栏（`TEXT_GUTTER` 常数）。

### 代码落点

| 改动 | 位置 |
| --- | --- |
| `resize_around_center` / `clamp_into` / `text_rect` / `TEXT_WINDOW_WIDTH` / `TEXT_GUTTER` / `paint_bitmap_scaled` / `StyledRow.number` | `crates/qlr-platform/src/window.rs` |
| `State.scroll_anchor` / `State.zoom_preview` / `set_zoom_to(.., render)` / `on_zoom_drag_*` 分流 / 行号与表格上屏 | `crates/qlr-app/src/app.rs` |
| `LineStyle::Table`（COUNT = 9） | `crates/qlr-render/src/canvas.rs` |
| `inline()` / `table_row()` / `task_marker()` / 转义锁定 | `crates/qlr-render/src/markdown.rs` |

### 实测（`_bench/verify-input-105.ps1`）

见 LOG 1.0.5 的六行断言表：缩放保位 `0,0` px、PDF 12 帧 1 次渲染、文本 880 宽、
左键拖窗 `100,60`、右键滚动、Markdown `STYLED 15 行（h1=1 h2=1 code=1 list=2 rule=1 table=2 normal=7）行号栏=64px`。

关卡：`cargo test` **132 passed**、`cargo fmt --check` 0、`cargo clippy --all-targets -- -D warnings` 0。
新增坑：#59（缩放要用窗口中心当锚点）、#60（拖动期间不重渲染，只拉伸预览）、#61（行号栏宽度必须常数）。

## 19. 1.0.6 回退 + 黑边真修 + 放大镜（用户第五轮反馈后续）

用户原话：「我觉得需要回退到上一个版本，刚才的改动都没什么用，体验并没有变好，pdf的黑边依然存在，也别让拉伸窗口了。如果真的需要放大素材中的某部分细节，引入一个放大镜功能算了」。

### 现在的结论

- **上一版（拖边改窗口大小 / 放大上限=工作区）已整体回退**（`git revert f3b0b2e` → `9b7b9d7`）。
  拖边改窗口大小这个方向不再做。
- **黑边其实是 1 px 的深色细线**：PDF 开窗 `734x1040`、位图 `734x1039`（按页面比例取整），
  而居中绘制会丢那一行像素。修法是"位图不大于客户区就铺满客户区"，差 1 px 的拉伸看不出来。
- **放大镜**：按住 Shift 出现 220×220 镜头（放大屏幕像素，不重新渲染），Shift + 滚轮调 1.5×–8×，
  松开消失。图片与 PDF 都支持。

### 交互补充（与 1.0.5 一致的都保留）

| 输入 | 行为 |
| --- | --- |
| 按住 **Shift** + 移动鼠标 | 光标处出现放大镜（默认 3×），跟随移动 |
| **Shift + 滚轮** | 放大镜倍数 ±1（1.5×–8×） |
| 松开 Shift（或鼠标移出窗口） | 放大镜消失 |

其余手势（左键拖动挪窗口、右键拖动缩放/文本滚动、滚轮滚动、PageDown 翻页）与 1.0.5 相同。

### 代码落点

| 改动 | 位置 |
| --- | --- |
| `paint_bitmap_at` 铺满客户区 + 显式源矩形、`paint_bitmap_scaled` 铺满、`shift_down`、`LENS_RADIUS`、`paint_lens` | `crates/qlr-platform/src/window.rs` |
| `State.lens` / `State.lens_zoom` / `LENS_DEFAULT_ZOOM`+上下限 / `update_lens` / `lens_wheel` / `paint_lens_if_any` | `crates/qlr-app/src/app.rs` |

### 实测（`_bench/verify-lens.ps1` 10 PASS / 0 FAIL）

图片与 PDF 的底行/右列底色像素均为 0；`LENS 开 368x521 zoom=3.0×` + 边框 `(224,224,224)`；
Shift+滚轮 `4.0×` → `5.0×`；松开后 `LENS 关` 且边框 0 残留。
关卡：`cargo test` **132 passed**、`fmt` 0、`clippy -D warnings` 0、`smoke.ps1` 17 PASS + 1 SKIP。
新增坑：#63 位图差 1 px 要铺满、#64 放大镜放大屏幕像素而不是重新渲染。
## 20. 1.0.7 设置页 + 快捷键按类别分组 + 托盘菜单瘦身

用户原话：「把设置页完善一下，主要是快捷键相关的，按类别放好，你先做，我等会再调整，另外，右键显示预览那个东西可以删掉，没用」。

### 现在的结论

- **托盘右键菜单**：`显示预览` 已删（它没有明确的"显示什么"，点了常以为坏了）。现在是
  `设置…` / `关于` / 退出。
- **快捷键全部可改**：配置在 `config.json` 的 `hotkeys` 段（9 个字段），
  默认 `toggle=Space`、`close=Esc`、`page_next=PageDown`、`page_prev=PageUp`、
  `scroll_down=Down`、`scroll_up=Up`、`zoom_in==`、`zoom_out=-`、`lens_hold=Shift`。
  支持修饰键写法（`Ctrl+Space`），修饰键要求**完全相等**（`Ctrl+Space` 不会被 `Space` 抢）。
- **设置页**：托盘「设置…」或 `qlrust.exe --settings`。按类别分组：

  | 分组 | 内容 |
  | --- | --- |
  | 全局 | 打开/关闭预览、关闭预览 |
  | 浏览（预览打开时） | 下一页、上一页、向下滚动一行、向上滚动一行、放大一档、缩小一档 |
  | 放大镜 | 放大镜开关（按住） |
  | 鼠标（固定，不能改键） | 左键拖动 = 移动窗口、右键上下拖 = 图片/PDF 缩放、文本按行滚动、滚轮 = 滚动/翻页 |

  操作：点一行 → 直接按新键（`Backspace` 恢复默认）→ `Enter` 保存 / `Esc` 取消；
  保存后**立即生效**（写文件 → `WM_QLR_SETTINGS_SAVED` → 宿主重装钩子绑定），无需重启。

### 代码落点

| 改动 | 位置 |
| --- | --- |
| `keymap`（`Action` 8 个 + `Category` 2 个 + `Binding` + `Hotkeys`，12 条测试） | `crates/qlr-core/src/keymap.rs`（新） |
| `Config.hotkeys` + `sanitized()` + `summary()` + 3 条测试 | `crates/qlr-core/src/config.rs` |
| `Action::Settings`、`WM_QLR_SETTINGS_SAVED` | `crates/qlr-core/src/ipc.rs` |
| 绑定表 `BINDINGS: [AtomicU32; 8]`、`set_bindings`、`log_bindings`、钩子按绑定匹配 | `crates/qlr-platform/src/hotkey.rs` |
| 托盘菜单删 `TOGGLE`、`设置…` 可用 | `crates/qlr-platform/src/tray.rs` |
| `paint_settings` / `SettingsRow` / `SETTINGS_*` 常量 / `create_settings_window` / `set_client_size` / `center_on_work_area` / `show_foreground` / `key_down` / `modifier_state` | `crates/qlr-platform/src/window.rs` |
| 设置窗逻辑（布局、命中测试、改键、冲突检查、保存、`WM_*` 处理，4 条测试） | `crates/qlr-app/src/settings.rs`（新） |
| `open_settings` / `on_settings_saved` / `lens_down` / `startup_shows` 加 `Settings` / `--settings` | `crates/qlr-app/src/app.rs`、`crates/qlr-app/src/cli.rs` |

### 实测（`_bench/verify-settings.ps1` 10 PASS / 0 FAIL）

`--settings` 起窗（636×640、21 行、ink=13058，截图 `_bench/settings-107.png`）→ 点"打开/关闭预览"行 →
按 `F9` → `Enter` 保存：`config.json` 变成 `"toggle": "F9"`，
日志依次 `改键 打开 / 关闭预览 = F9` → `设置已保存 → …\config.json` →
`按键绑定：打开 / 关闭预览=F9 …` → `配置已热更新：… [toggle=F9 …]`，窗口自动关闭。

关卡：`cargo test` **151 passed**（core 26 / app 42 / platform 18 / plugin-api 4 / plugin-demo 7 /
render 47 / malformed 7）、`cargo fmt --check` 0、`cargo clippy --all-targets -- -D warnings` 0、
`scripts/smoke.ps1` 17 PASS + 1 SKIP。

新增坑：#65 新增"启动就该看见效果"的命令要同步 `startup_shows`；#66 设置页行高常量必须绘制与命中
测试共用；#67 钩子回调里禁止写日志（300 ms 预算）；#68 `ipc::Action` 与 `keymap::Action` 同名要写全路径。

## 21. 1.0.8 白色主题 + 代码高亮 + PDF 页码条（用户第八轮反馈）

八条反馈一次做完。核心变化是**主题从深色翻成浅色**，于是"配色"这一层全部重排，
顺带把代码高亮、Markdown 块级底色、PDF 顶部页码条一起接上。

### 交互/观感对照

| 项 | 1.0.7 | 1.0.8 |
|---|---|---|
| 底色 | `#1A1A1A` 深灰 | **白 `#FFFFFF`**（文本/图片/设置页同底） |
| 正文色 | 近白 `#F0F0F0` | 近黑 `#202020`；标题蓝、引用灰、行号浅灰 |
| 图片开窗 | 工作区用满 | **工作区 × 0.86**（1920×1080 工作区上 1920×1080 图 → 1589×894） |
| 图片右键拖动 | 缩放 | **无动作**（不再缩放）；滚轮缩放保留 |
| PDF 缩小 | 目标尺寸直接渲染（糊） | 2 倍超采样后盒缩（清晰），24 MP 预算 |
| PDF 翻页 | 先写占位再渲染（闪） | 旧页留屏，新帧到位才刷 |
| PDF 顶部 | 无 | **`PDF · 第 n / m 页（滚轮或 PageDown 翻页）` 状态条（26 px）** |
| 文本/MD 行数 | 少算 padding → 最后一行被裁 | 29 行正文完整可见 |
| 源码文件 | 纯文本一行一色 | **仿 VSCode**：等宽 + 行号 + 关键字/字符串/注释/数字着色，不折行 |
| Markdown | 无块底色 | 代码块 `#F6F8FA`、引用块 `#F7F7F7` 整行底色 |
| 热键前台判定 | 只认 shell 类 | shell 类 **或** 本进程窗口（修"空格偶失效"） |

### 代码落点

| 文件 | 改动 |
|---|---|
| `crates/qlr-platform/src/window.rs` | `BITMAP_BACKGROUND_RGB=0xFFFFFF`；`TextStyle` 14 变体（新增 Keyword/Str/Comment/Number）+ 浅色配色；`Span`/`DocRow`/`paint_doc`（一行多段）；`paint_styled` 退化为单段包装；`HEADER_HEIGHT=26` + `paint_header`；`paint_bitmap_at/scaled` 增加 `header` 参数；`natural_rect` 加 `NATURAL_FILL=0.86` |
| `crates/qlr-render/src/code.rs`（新） | `CODE_EXTS` / `is_code` / `tokenize_lines` / `tokenize_line`（注释、字符串、数字、关键字，跨行块注释状态）/ `render`，7 条单测 |
| `crates/qlr-render/src/canvas.rs` | `Token`（5 类）+ `Span` + `TextDoc.spans` |
| `crates/qlr-render/src/lib.rs` | `pub mod code`；`TextViewer` 按扩展名分流到 `code::render`；新增 `CodeViewer` |
| `crates/qlr-render/src/pdf.rs` | `SUPERSAMPLE=2.0` + `SUPERSAMPLE_BUDGET=24MP` + `downscale_bgra` |
| `crates/qlr-platform/src/hotkey.rs` | `foreground_is_ours()`；Toggle/Close 分支改 `ours \|\| shell`；白名单补 4 个 shell 类 |
| `crates/qlr-app/src/app.rs` | `visible_rows` 扣 padding；`paint_code()`；`to_token_style()`；`line_bg()`；`view_size`/`set_zoom_to`/`show_preview` 的 PDF 内容区扣 `HEADER_HEIGHT`；`step_page` 不再写占位；`on_zoom_drag_start` 只对 PDF 缩放 |
| `crates/qlr-core/src/keymap.rs` | `+`/`=` 同一物理键，显示统一 `+`（日志不再出现 `zoom==/-`） |

### 验证

`cargo test` **158 passed**、`fmt` 0、`clippy -D warnings` 0、`smoke.ps1` 17 PASS + 1 SKIP。
实机 8 项逐条截图 + 日志确认（详见 `LOG.md` 的 1.0.8 条目与 `_bench/v108-*.png`）。

新增坑：#69 改主题会废掉判色脚本；#70 行数预算要扣 padding；#71 新内容没到别 invalidate；
#72 缩小清晰度靠超采样（要预算）；#73 一行多段要自己量宽推进；#74 热键判定要含本进程窗口。

### 附带修复：设置页在浅色底下的两处露馅

换白底重拍 `docs/screenshots/settings.png` 才看出来（深色底时都不显眼）：

- **底部提示行被裁半截**：`create()` 按 `layout().len()` 算高度，`paint()` 多画 2 行（空行 + 提示）。
  抽出 `EXTRA_ROWS`/`client_height()`，绘制与开窗同源，加单测 `client_height_covers_every_painted_row`（#75）。
- **说明行被空值列挤掉半句**：`paint_settings` 左列固定留 180 px 给值列，说明行没值也照留；
  改成"右值为空就整行给左列"。提示行配色从 `Rule`（浅灰）改 `Quote`（深灰），白底下才看得见。
- 截图改用 `PrintWindow`（`_bench/shot3.ps1`）——`CopyFromScreen` 会截到盖住窗口的别的程序（#76）。


---

## 22. 1.0.9 统一顶栏：目录 / 页码跳转 / 外部打开 / 滚动条

用户第九轮反馈（9 件事，原话见 `LOG.md` 1.0.9 条目）。核心变化是**顶栏从"只有 PDF 有的一条文字"
升级成所有格式共用的底层家具**，并明确"插件只能画在顶栏之下"。

### 交互对照（1.0.9）

| 输入 | 文本/Markdown | 源码 | 图片 | PDF |
|---|---|---|---|---|
| 顶栏黑底 | 有（标题） | 有 | 有 | 有（标题 + `1 / 8` 页码框 + ↗ + ×） |
| 点页码框 | — | — | — | 进入输入态，数字 + 回车跳页，Esc 取消 |
| 点 ↗ | 用系统默认程序打开该文件（悬停提示"在其他应用打开"） | 同 | 同 | 同 |
| 点 × | 关闭预览（与 Esc、再按空格等效） | 同 | 同 | 同 |
| 左侧目录 | Markdown 有（窗口宽 > 520 才显示）；点击跳转，高亮跟滚动 | — | — | — |
| 右侧滚动条 | 有（按行） | 有 | — | 有（按页） |
| 右键拖动 | 按行滚动 | 按行滚动 | 不能缩放（关了） | **不再缩放**（按用户要求） |
| 滚轮 | 逐行滚动 | 逐行滚动 | 缩放 | 页内上下滑 → 到边翻页 |

图片开窗默认 **50%**（`Config::image_zoom_percent`，设置页可改 10–400%），
顶栏/目录/滚动条都不占"内容像素"（`chrome_rects()` 与 `frame()` 统一扣）。

### 代码落点

| 文件 | 新增/改动 |
|------|-----------|
| `crates/qlr-platform/src/window.rs` | `Chrome`/`PageBox`/`ChromeButton`/`ChromeRects`/`chrome_rects`/`paint_chrome`；`ScrollInfo`/`scrollbar_rect`/`paint_scrollbar`/`scroll_start_at`；`TocItem`/`toc_rect`/`toc_item_rect`/`paint_toc`/`toc_item_at`；`natural_rect_scaled`；`open_external`；`paint_doc`/`paint_styled`/`paint_text`/`paint_bitmap_*` 全部接 `Option<&Chrome>` 与 content 矩形 |
| `crates/qlr-render/src/markdown.rs` | `TocEntry` + `outline()`（标题 → 目录项，`line` = 显示行下标） |
| `crates/qlr-platform/src/hotkey.rs` | `set_capture_digits()` + 钩子里的数字输入分支（放在注入过滤之前） |
| `crates/qlr-core/src/config.rs` | `image_zoom_percent`（默认 50，夹 10–400） |
| `crates/qlr-app/src/app.rs` | `State` 加 `hover_button/page_edit/scroll_drag/toc/toc_active`；`frame()`/`file_title()`/`scroll_info()`/`goto_page()`/`open_external()`/`finish_page_edit()`/`page_edit_key()`/`on_chrome_click()`/`apply_scroll_y()`/`update_hover()`/`paint_overlays()`/`toc_following()`；`repaint` 三个分支改走 chrome + content |
| `crates/qlr-app/src/settings.rs` | `Row::ImageZoom`/`Edit::ImageZoom` + `confirm_image_zoom()`（范围提示） |

### 验证

`cargo test` **160 passed**、`fmt` 0、`clippy -D warnings` 0。
实机：`_bench/verify-input-109.ps1` **9/9**、`_bench/verify-109b.ps1` **2/2**
（页码输入跳页、外部打开、目录跟随高亮、滚动条拖到底、图片 50%、PDF 右键不再缩放）。

新增坑：#77 叠层必须后画；#78 绘制入口必须带 content 矩形；#79 数字输入要借钩子且放在注入过滤前；
#80 目录高亮要跟滚动；#81 数字型设置项要给范围提示。

## 23. 1.0.10 不支持只响铃 / 拖边改大小 / 柔和投影 / 跟随选中项 / 顶栏可拖 / 阻止关闭

用户原话见 [LOG.md](LOG.md) 的 1.0.10 条目；六条按顺序落地，第三条（阴影）做过一次返工。

### 交互对照（1.0.10 新增/变更）

| 场景 | 1.0.9 | 1.0.10 |
|------|-------|--------|
| 选中不支持的文件按空格 | 弹窗显示占位文字（"暂无内置查看器"+插件目录） | **不弹窗**，响一声系统提示音；已可见的预览直接收起 |
| 窗口边框 | 无 | 客户区最外 1 px `#C8C8C8` 发丝边 + 窗口外 16 px 柔和投影 |
| 拖窗口边/角 | 无 | 6 px 热区，指针变 `↔ ↕ ⤡`；松手按新客户区重渲染 |
| 拖顶栏空白处 | 不动（只关掉页码输入） | 移动窗口（与拖正文一致） |
| 预览打开时切换选中文件 | 无反应 | 400 ms 内自动换成新选中的文件 |
| 顶栏「锁」按钮 | 无 | 点一次之后空格 / Esc 关不掉；**不能取消**；`×` 与托盘退出仍可关 |

### 代码落点

| 文件 | 新增/改动 |
|------|-----------|
| `crates/qlr-platform/src/window.rs` | `ChromeButton::Pin` + `Chrome.pinned` + `ChromeRects.pin`；`resize_to`/`window_rect`/`CursorKind`/`set_cursor`；`beep()`（`PlaySoundW`）；`start_timer`/`stop_timer`；`create_shadow_window`/`sync_shadow`/`paint_shadow`/`hide_shadow`；`paint_window_frame` 改单圈发丝边；`SHADOW_MARGIN`/`SHADOW_ALPHA`/`FRAME_HAIRLINE_RGB` |
| `crates/qlr-platform/src/hotkey.rs` | `foreground_is_shell()`（跟随轮询先看前台类名再动 COM） |
| `crates/qlr-app/src/app.rs` | `State` 加 `pinned`/`resize_edges`/`resize_from`/`shadow`；`ResizeEdges` + `edges_at`/`begin_resize`/`on_resize_move`/`finish_resize`；`close_blocked()`；`sync_shadow()`；`on_follow_timer()`；`show_preview` 提前拒绝 + 启定时器；`WM_TIMER`/`WM_SETCURSOR` |

### 验证

`cargo test` **163 passed**（app 30 / platform 19）、`fmt` 0、`clippy -D warnings` 0、exe **1,891,840 B**。
实机：`_bench/verify-input-1010.ps1` **13/13**（3 轮）、`_bench/verify-shadow.ps1` **SHADOW OK**
（阴影窗 912×780 对齐 880×748 预览窗，外侧到贴边亮度 255→171 的 16 px 渐变）。
## 24. 1.0.11 顶栏页码框与「锁」换位

顶栏右侧顺序（1.0.11 起）：**`页码框 | 锁 | ↗ | ×`**（1.0.10 是 `锁 | 页码框 | ↗ | ×`）。

- 只改了 `crates/qlr-platform/src/window.rs` 的 `chrome_rects()`：`pin` 贴 `external` 左侧，
  `page` 在 `pin` 左侧留 10 px，标题右边界取 `rects.page.left - 12`。
- 单测改名并加固：`chrome_rects_place_page_box_left_of_the_pin_button`（断言 `page <= pin == external <= close`、
  按钮等宽、页码框宽 `PAGE_BOX_W` 且上下各留 5 px）。
- `_bench/verify-input-1010.ps1` 的"点锁"坐标同步改成 `external.left - 38`，13/13 仍全绿。
## 25. 1.0.12 修"缩小后外面多一圈灰框"（投影没跟缩放）

- 根因：`sync_shadow()` 只在显示/拖动/拖边时调用，**缩放改窗口尺寸这条路漏了**；
  阴影保持老尺寸 ⇒ 中心不变、四边均匀露出灰边。
- 改法：`set_zoom_to()` 末尾补 `sync_shadow()`；`repaint()` 开头兜一次（尺寸不变时只是一次 `SetWindowPos`）。
- 验证：`_bench/verify-zoom-shadow.ps1` **4/4**：`795x477 → 636x388 → 509x316`，
  阴影始终 `= 预览窗 ±16`。

## 26. 1.0.13 窗口位置记忆

- `Config.window_pos: Option<(i32, i32)>`；`remember_position()` 在显示/拖动/拖边/缩放后捕获，
  `persist_position()` 只在收起与退出时写 `config.json`。
- 开窗时：窗口已开 → 用当前位置；否则用存档；再否则居中。落点统一过 `window::clamp_into`（改成 `pub`）。
- 验证 `_bench/verify-pos.ps1` **8/8**（拖到 702,386 → 收起 → 重开仍在 702,386；换文件不跳位）。

## 27. 1.0.14 窗口尺寸记忆（按格式分开）

- `Config.window_sizes: BTreeMap<String, (i32, i32)>`（键 = `format::Kind::name()`，值含顶栏）、
  `window_size_for()` / `set_window_size()`；`sanitized()` 丢掉 0/负数/超大值。
- `remember_size()` 只在**用户改过尺寸**的路径调用（`finish_resize` / `set_zoom_to`）；
  `show_preview` 优先 `last_sizes`（本次会话）→ `config.window_sizes`（上次落盘）→ 按内容算。
- `persist_position()` 现在同时比对位置与尺寸，任一变化才写 `config.json`。
- 验证 `_bench/verify-size.ps1` **9/9**（图片 955×477 记住、PDF 631×924 各自独立、缩小后 764×388 也记住）。

## 28. 1.1.0 投机预渲染（窗口收着时先画好）

### 为什么做
1.0.14 实测：按空格到出图 16–20 ms（4K/8MP 图），其中绝大部分是"解码 + 缩放 + 建位图"。
用户按空格的**前一步**永远是在资源管理器里点中一个文件，那个信号我们看得见
（1.0.10 的跟随选中项已经在用），于是可以提前把那**一张**画好。

### 三档（设置页 → 性能 → 投机预渲染，点一下换档）

| 档 | 行为 | 适用 |
|---|---|---|
| 关 | 完全不预热；计时器只在窗口可见时跟随选中项 | 不想有任何后台开销 |
| **自适应（默认）** | 只在"最近 5 分钟内真的按过空格"时预热 | 日常 |
| 总是 | 只要前台是资源管理器且单选就预热 | 长时间连续看图 / 演示 |

### 六道闸门（按"便宜的先过"排序）
1. 前台是资源管理器（`hotkey::foreground_is_shell()`，省掉 COM 查询）；
2. 单选（多选不猜，框选一堆时"他会看哪个"是随机的）；
3. 两拍稳定（同一候选连续两次 400 ms 轮询才动手，按住方向键扫文件时不抢 CPU）；
4. 按类型预算：图片 ≤ 20 MP、文本 ≤ 8 MB、PDF ≤ 30 MB；
5. 自适应档额外要求"最近 5 分钟按过空格"；
6. 单槽 + **算前算后各查一次**"还要不要"（算完已过时就丢弃）。

### 关键取舍
- **独立线程 `qlr-prewarm`**：预热绝不能堵住用户真要看的那一帧（复用渲染线程的话，一次 48MP 预热会让用户等 60 ms）。
- **只留一个槽**：用户选中的永远是"那一个"，多槽等于把内存花在用不到的文件上。
- **插件渲染一律不预热**：插件是第三方代码，不该在"用户没要求"时跑。
- **`window_size_for()` 让开窗与预热共用同一套尺寸算术**：两边一漂移，预热就永远打不中槽。
- **计时器常驻**（启动即开、`hide_preview` 不再停）：它同时承担"可见时跟随"与"收着时预热"。
- 超预算的日志只在**换文件**时记一行，否则一直选中的大文件会把日志刷满。

### 实测（`_bench/verify-prewarm.ps1` / `_bench/measure-prewarm.ps1`）

| 项 | 结果 |
|---|---|
| 8MP JPEG 首帧 | 无预热 **20 ms** → 命中 **0–1 ms** |
| 预热自身耗时 | 8MP JPEG 20 ms、8 页 PDF 3 ms（都在窗口收着时做） |
| 常驻增量 | Private **+2.3 MB**（795×477 画布 1.5 MB + 行索引） |
| 空闲 CPU | 6 s 增量 0.02–0.05 s（自适应档、前台非资源管理器） |
| 关档对照 | 日志里完全没有 `PREWARM` 行，也无命中 |
| 超预算 | 16 MB 文本、48MP 图片 → 跳过（不预热） |
| 内存口径 | 对比要看 **Private**：`WorkingSet64` 会把用过的 dll 页面算进来（pdfium 7.2 MB） |
## 29. 1.2.0 稳定性硬化（渲染超时/取消 + 崩溃兜底 + 长稳压测 + fuzz）

按计划书三期做，目标是"用户怎么折腾都不会整窗卡死、崩溃也留得下证据"。

### B1 渲染超时与取消

| 机制 | 实现 | 目的 |
|---|---|---|
| 取消信号 | `qlr-render::cancel`（`cancel / reset / checkpoint`，进程级 `AtomicBool`） | 新的请求进来时，让**正在跑**的旧活尽早退出 |
| 检查点 | 图片（入口 + 解码后）、PDF（入口 + 缩放前）、文本 / Markdown / 源码（入口 + 解码前） | 多阶段渲染可以在阶段之间刹车 |
| 看门狗 | 250 ms 轮询的 `WM_TIMER`；超过 **3 s** 记 `RENDER 超时`、打印取消提示、只把**占位文字**换成"超过 3 s，正在请求取消" | UI 线程永不阻塞，窗口始终可关/可换文件 |
| 排队丢弃 | 渲染线程一次只做最新任务（`try_recv` 排空，记 `RENDER 丢弃 N 个排队任务`） | 连按方向键扫文件时不做无用功 |
| 边界（诚实说明） | 单次 FFI 调用（`FPDF_RenderPageBitmap`、`image` 一次解码）中途打断不了 | 检查点只能缩短"多阶段"的等待，做不到"随时中断" |

### B2 崩溃兜底 + minidump

- `qlr-platform::crash`：`install_hook()` 在启动时**第一件事**安装（先把旧的简易 hook 删掉——它会把带 minidump 的那个覆盖掉）。
- 崩溃时写两份证据到 `<data>\crashes\`：`crash-<时间戳>.txt`（banner / 时间 / 线程 / 位置 / 消息 / 回栈）与 `.dmp`（`MiniDumpWriteDump`，Normal + ThreadInfo）；同时把 `PANIC: …（dump=true）` 写进日志。
- 下次启动 `report_previous_crash()` 打一行 `上次运行发生崩溃（crashes 里共 N 份报告）`。
- 插件调用包了 `catch_unwind`（**诚实说明**：release 是 `panic = "abort"`，这条救不回 release 进程；它是给 dev/测试档 + 将来进程外插件铺路）。
- 文件名必须**自己消毒**：`utc_timestamp()` 里有 `:`，Windows 上非法 → 建文件静默失败（第一次验证时 `dump=false` 就是这个）。

### B3 长稳压测（`scripts/stress.ps1`）

| 项 | 结果（本机实测：2000 次开关 + 500 次翻页 + 300 次缩放，总耗时 166.8 s） |
|---|---|
| GDI 对象 | 20 → 20（**+0**，上限 +5） |
| USER 对象 | 16 → 16（**+0**，上限 +5） |
| 句柄 | 153 → 153（**+0**，上限 +20） |
| 私有字节 | 3.9 → 5.2 MB（允许 +4 MB） |
| 常驻内存 | 前段均值 14.1 → 后段 15.7 MB（**堆保留**，允许 +8 MB；峰值 20.3 MB，重启后 11.1 MB） |
| 日志 | 无 `panicked at` / 无崩溃报告 |

判据为什么这么写：Windows 的堆在密集分配/释放后会保留已提交页面，"RSS 涨几 MB"不等于泄漏；
真正的泄漏先表现在 **GDI/USER/句柄**或**私有字节**持续增长上，所以三条一起看，并复核"重启后回到基线"。

### B4 fuzz 扩容（`crates/qlr-render/tests/fuzz.rs`）

- 确定性变异（固定种子 `0x5EED_1234_ABCD_0001`，5 种手法：位翻转 / 字节替换 / 截断 / 插入 / 自复制），
  种子覆盖 png / pdf / txt / md / rs 五种入口。
- 默认 **2000 例**（`cargo test` 里跑，0.3 s）；`QLR_FUZZ_ITERS=10000` 给夜间任务（1.7 s，实测无挂起）。
- 判据：不 panic、不超时；并断言"不能全成功也不能全失败"（防变异失效 / 入口被短路）。

### 验证（`_bench/verify-hardening.ps1`，12/12 PASS）

`C0` 启动前无残留实例 · `C1` 注入 panic 后进程退出 · `C2` 生成崩溃报告 · `C3` 生成 minidump（54.7 KB）·
`C4` 报告含消息与位置 · `C5` 下次启动报告上次崩溃 · `A1` 注入慢渲染生效 · `A2` 3 s 报超时 ·
`A3` 已请求取消 · `A4` 渲染卡着也能关窗口（123 ms）· `A5` 慢帧后线程仍可用 · `B1` 过期排队任务被丢弃。

两个只在测试时用的隐藏环境变量（正常使用不存在）：`QLRUST_SLOW_RENDER_MS`、`QLRUST_PANIC_TEST`。

## 30. 1.2.1 大 PNG 流式降采样（四期）

### 问题

1.2.0 之前，除 JPEG 外的所有图片都走 `image` 的**整图解码**：一张 8000×6000 的 PNG，
解码缓冲就是 8000×6000×4 ≈ 192 MB，而屏幕上最终只要 1200×900 ≈ 4 MB。
中间那 190 MB 纯白花，用户开着别的程序时这一下就是可见的卡顿甚至换页。
JPEG 早就有 DCT 域缩放（`jpeg_decoder::Decoder::scale`），所以这个洞只在 PNG 这类"按行存"的格式上。

### 做法

新增 `crates/qlr-render/src/downscale.rs`：用 `png` crate 的 `Reader::next_row()` **逐行**解码，
每读一行就按面积平均累加进"目标行"，累计完一行就写出去、清空累加器。

- 常驻 = 目标画布 + 一行原始像素 + 每列累加器（`dst_w*4` 个 u32 + 计数），48MP→1200 宽约 **4 MB + 32 KB + 20 KB**；
- 颜色一律摊平成 RGBA（`Transformations::EXPAND | STRIP_16`，调色板/灰度/16 位都由解码器处理）；
- 每 64 行问一次 `cancel::checkpoint()`；解码出错**如实报**，不偷偷回退（回退会再失败一次，还会盖掉真实原因）；
- 只在"确实要缩小 + 源图 ≥ 4 MP"时启用，其它情形（放大、小图、隔行、APNG）返回 `None` 交回整图解码。

### 边界（诚实说明）

- **隔行扫描（Adam7）PNG 放弃**：`next_row` 给出的行不是自上而下的自然顺序，面积映射会错位 → 交回 `image`。
- **APNG 放弃**（多帧语义本来也不支持）。
- 只做 PNG：TIFF/GIF/BMP 走整图解码（量级小），`tiff` 的 strip API 用法琐碎，需要时再补。

### 实测（`_bench/measure-bigpng.ps1`，口径 = 从 Show 到首帧 PAINT 的 `PrivateMemorySize64` 峰值）

| 场景 | 1.2.0（整图解码） | 1.2.1（流式） | 提升 |
|---|---|---|---|
| 48MP PNG 8000×6000 | 峰值 **140.6 MB**（增量 138.6），RENDER **183 ms** | 峰值 **4.8 MB**（增量 2.8），RENDER **191 ms** | 内存 **29×**，耗时基本持平 |
| 8MP PNG 3840×2160 | 峰值 **29.5 MB**（增量 27.5），RENDER **42 ms** | 峰值 **7.1 MB**（增量 5.1），RENDER **29 ms** | 内存 **4×**，耗时 **-31%** |

（8MP 更快是因为省掉了"整图 RGBA + `thumbnail` 复制"两趟大内存搬运；48MP 那 8 ms 差异在噪声内。）

## 31. 1.3.0 PDF 页缓存（五期 5b）+ 为什么 5a/5c 不做

### 做了什么
| 项 | 做法 | 效果 |
|---|---|---|
| 页缓存 | 新 `crates/qlr-app/src/pagecache.rs`：`PageKey{path,page,width,height}` 精确匹配、`VecDeque` LRU、上限 64 MB、单页超限不入缓存 | 翻回去（200 页 Word PDF）**0 ms**，此前 6–9 ms（放大后 28–60 ms） |
| 相邻页预取 | 复用 `qlr-prewarm` 线程，新增 `WarmTask::Page`：当前页算完就把前后各一页丢过去，结果只进缓存 | 顺次翻页**首次**也命中（实测 `PAGECACHE 预取 page=2 elapsed=2ms` → 翻页 `命中`） |
| 换文件清缓存 | `show_preview` 里路径变了就 `pagecache::clear()` | 不会攒着上一本文档的几十页 |
| 尺寸统一 | 开窗的渲染请求改成 `(宽, 高 − reserve)`，与 `view_size()` 对齐 | 缓存 key 才对得上（第 108 条） |

### 实测（`_bench/verify-pagecache.ps1` 8/8 PASS + `_bench/measure-*.ps1`）
- 8 页 PDF：开窗预取 page2 → 翻页命中 → 翻回命中，每次"请求→首帧 0 ms"。
- 200 页 Word PDF：`PAGECACHE 命中 gen=2 page=2 632x894（跳过渲染）`，三次往返全部 0 ms。
- 关档（`prewarm: "off"`）：不再预取，但已算过的页仍命中（E1/E2）。
- 缓存占用：8 页文档实测 2 页/4 MB；上限 64 MB ≈ 28 页。

### 为什么 5a（GPU 呈现）/5c（mipmap）不做
- 5a：实测 `PAINT` 与 `RENDER` 同毫秒完成（631×893 与 993×559 都是 <1 ms），GDI `StretchDIBits` 已不是瓶颈；
  引入 D3D11 交换链还要处理"分层阴影 + `PrintWindow` 截图可能全黑"的兼容问题，收益 0–1 ms 不值得。
  数据与推理写进 `docs/dev/CONVENTIONS.md` 第 111 条，将来做 4K/8K 平滑平移时再回头看。
- 5c：缩小路径已经各自做了高质量降采样（JPEG 走 DCT 域 `Decoder::scale`、PNG 走逐行面积平均、
  PDF 走 2× 超采样后盒式缩小），mipmap 只在"连续几十次缩放"里才可能有差别，暂不做。

### 踩坑
1. 缓存 key 用了"整窗高度"写、用"内容区高度"读 → 命中率恒为 0（第 108 条）；先怀疑缓存实现、再怀疑尺寸不一致，
   用日志里的 `631x924` vs `631x894` 一眼看出差 30（顶栏）。
2. 验证脚本头一版把 B1 写成"翻页必须看到 RENDER"——预取生效后第 2 页本来就**不该**再渲染，
   断言写反了会误报失败；正确断言是"翻页命中缓存且本段没有 `RENDER gen=`"。
3. `pagecache::is_empty()` 一开始只给测试用，被 clippy 判 `dead code`；改成测试直接断言 `len() == 0`。

---

## §32 1.4.0 上一帧缓存 + Office/压缩包插件

**一句话**：重开同一个文件不再重新渲染（22 ms → 2 ms）；Office 与压缩包用**插件**接进来，
宿主一行代码没改——插件 ABI 从"能跑"变成"真有人用"。

### 交互 / 能力变化
| 场景 | 1.3.0 | 1.4.0 |
|---|---|---|
| 重开同一文件（未修改） | 重新渲染 20–60 ms | **命中上一帧，2 ms 上屏** |
| 在 A/B 之间来回看 | 每次都渲染 | 各自命中（LRU 3 帧） |
| 文件被改过 | — | 文件戳变了 → 不命中，照常渲染 |
| .docx/.xlsx/.pptx/.odt/.zip | "暂无内置查看器" | 插件预览（缩略图或文字大纲） |

### 代码落点
| 位置 | 作用 |
|---|---|
| `crates/qlr-app/src/app.rs` `struct FrameKey` | 路径 + mtime + 大小 + 页 + 渲染尺寸 + 放大标志 |
| 同上 `LAST_FRAMES: Mutex<VecDeque<...>>` | 3 帧 LRU（`LAST_FRAME_CAP`） |
| 同上 `take_last_frame` / `remember_last_frame` | 查/存，命中即 `present_canvas` |
| `crates/qlr-plugin-office/src/lib.rs` | 插件：缩略图优先 → 文字大纲卡片（GDI 画字） |
| `crates/qlr-plugin-office/src/lib.rs` `sheet_rows` | xlsx 单元格：共享字符串 + 行内字符串 + 数字 |
| `scripts/pack.ps1` | 打包时一并带上 `qlr_plugin_office.dll` |

### 为什么这样取舍
1. **key 里必须带文件戳**：只比路径会把改过的文件当没改过，用户会看到旧内容（比"慢一点"严重得多）。
2. **只留 3 帧**：3 张窗口位图 ≈ 10 MB，换掉"每次重开都重算"，性价比最高；
   再多就要考虑"内存换速度"是否还符合本项目的定位。
3. **Office 只做大纲不做排版**：真排版 = 排版引擎（几百 MB），与体积目标冲突；
   文字大纲足够回答"这是哪个文件"，要看真容请用顶栏「在其他应用打开」。
4. **插件优先，宿主不内建**：验证插件 ABI 是否够用（结论：够——一个 DLL + 三个符号就能接一类格式）。

### 验证
- `_bench/verify-lastframe.ps1` **7/7 PASS**；五个 Office/压缩包夹具真机截图逐张确认。
- `cargo test` **194 passed**、`fmt` 0、`clippy` 0、`smoke.ps1` 17 PASS + 1 SKIP。

---

## §33 1.4.1 系统预览处理器（Office）+ 压缩包目录树 + 拖动性能

**一句话**：Office 不再"抽文字画卡片"，改成把内容交给**系统预览处理器**（`IPreviewHandler`，
和资源管理器预览窗格、QuickLook 同一条路），于是 Word/Excel/PowerPoint 的**真排版**直接出现在窗口里；
压缩包从"一串文件名"改成**目录树**；顺手修掉 1:1 拷贝还在做插值、以及窗口类带 `CS_HREDRAW|CS_VREDRAW` 的浪费。

### 交互 / 能力变化
| 场景 | 1.4.0 | 1.4.1 |
|---|---|---|
| .docx（装了 Office） | 文字大纲卡片 | **系统预览处理器渲染的真排版**（标题字号/颜色/段落间距） |
| .xlsx / .pptx | 文字大纲卡片 | **真表格 / 真幻灯片**（同一处理器） |
| .zip | 文件名列表 | **目录树**：`4 个文件夹 · 5 个文件 · 压缩后 11.7 KB / 解压后 11.7 MB` + 缩进树 + 每项大小 |
| 没装 Office / 处理器起不来 | — | **立刻退回插件卡片**（不会再停在"正在渲染…"） |
| 处理器窗口里的滚轮 | — | 转给处理器（`WindowFromPoint` + `PostMessage(WM_MOUSEWHEEL)`） |
| 拖动窗口 | 每帧都做 HALFTONE | 1:1 拷贝走 `COLORONCOLOR`，且窗口类不再 `CS_HREDRAW/CS_VREDRAW` |

### 代码落点
| 位置 | 作用 |
|---|---|
| `crates/qlr-platform/src/shellpreview.rs`（新） | 查注册表 CLSID → `CoCreateInstance` → `IInitializeWithFile::Initialize` → `SetWindow` → `DoPreview`；`Drop` 里 `Unload` + 销毁子宿主 |
| `crates/qlr-platform/src/window.rs` `create_child_host` / `move_child_host` | 给处理器一块"内容宿主"子窗口（它按父客户区铺满，直接给主窗口会把顶栏盖掉） |
| 同上 `window_from_point` / `post_wheel` | 把滚轮转发给处理器（它不监听我们的全局钩子） |
| `crates/qlr-app/src/app.rs` `shell_candidate` / `open_shell_preview` / `sync_shell_rect` | 只有"内置没有 + 插件接了 + 系统有处理器"才走这条路 |
| 同上 `State.shell_preview` + `repaint` 早退 | 正文由处理器画，宿主只画顶栏（不画目录/滚动条） |
| `crates/qlr-plugin-office/src/lib.rs` `tree_lines` | 压缩包目录树（目录数**按路径前缀**统计，因为很多打包器不写目录条目） |
| `crates/qlr-platform/src/window.rs` `paint_bitmap_at` / `paint_bitmap_scaled` / `register_class` | 1:1 用 `COLORONCOLOR`；去掉 `CS_HREDRAW\|CS_VREDRAW` |
| `crates/qlr-app/src/app.rs` `move_window_by` | `SLOWMOVE` 埋点（单次 ≥5 ms 才记一行） |

### 为什么这样取舍
1. **借系统处理器而不是自己写排版引擎**：真排版引擎是几百 MB，和"A8.5 体积预算"直接冲突；
   系统处理器是"装了 Office 就已经在你机器上"的现成能力，我们只负责给它一块窗口。
2. **必须有独立子宿主窗口**：处理器 `GetClientRect(父窗口)` 铺满，直接把主窗口交出去会连顶栏一起吃掉
   （第一版踩过：顶栏变白、标题没了）。
3. **`DoPreview` 必须等窗口可见**：隐藏窗口上调用会让处理器等一个永远不会到来的激活，UI 线程卡死
   （表现为"未响应"幽灵窗口，只能杀进程）。
4. **失败要立刻退回插件卡片**：否则屏幕停在"正在渲染…"，用户以为卡住了。
5. **压缩包按目录树而不是平铺**：用户要的是"结构"；顺带把压缩前/后大小放上去，便于判断值不值得解压。

### 验证
- 真机（`<夹具目录>`，逐个格式单独起一次宿主）：
  `plugin-plain.docx`、`plugin-demo.xlsx`、`plugin-demo.pptx` 全部 `SHELL 预览已接管`（截图 `_bench/v141-docx.png`、
  `v141-xlsx.png`、`v141-pptx.png`）；`tree-demo.zip` 走插件 `PLUGIN … elapsed=27ms`，目录树正确（`_bench/v141-zip.png`）。
- 切换文件（docx → xlsx → pptx → zip → docx）连续五步宿主不再崩（早前那次崩溃是"在隐藏窗口上 DoPreview"）。
- `cargo test` **195 passed**、`cargo fmt --all --check` 0、`cargo clippy --all-targets -- -D warnings` 0；
  release exe **2,028,544 B**（`FileVersion 1.4.1.0`）。
- **未知项**：用户报告的"同一文件第二次打开会虚掉、拖动很卡"在本机复现不出来
  （真实鼠标注入 40 步拖窗：`MOVE=39/40`、`PAINT=0`、耗时等于注入基准）。
  已加 `SLOWMOVE` 埋点，等用户开着 debug 日志再拖一次来定位。

---

## §34 1.4.2 收起时卸处理器卡死 + 拖动减负

**一句话**：录屏里"关了之后窗口留在屏幕上、内容变白"不是没关，而是**卸系统预览处理器时把 UI 线程卡住了**；
根因是 `Unload()` 跑在 `WM_COPYDATA` 的同步调用栈里。改成"挪进回收队列 + 发消息稍后卸"，关闭立刻恢复。

| 位置 | 改动 |
|---|---|
| `crates/qlr-app/src/app.rs` `SHELL_TRASH` / `retire_shell_preview` / `drain_shell_trash` | 处理器延迟拆卸；`WM_QLR_INVOKE` 与跟随计时器各收尸一次 |
| 同上 `hide_preview` | 先隐藏窗口，再把句柄交给回收队列（顺序反过来会看到白窗） |
| 同上 `open_shell_preview` | 换文件时旧处理器同样走回收队列 |
| 同上 `on_follow_timer` | 拖动/改尺寸期间直接返回，不碰 COM |
| 同上 `sync_shadow_with(throttle)` | 拖动时投影 8 ms 节流，松手强制对齐 |
| 同上 `follow_selection` | 加 `SLOWSELECT` 埋点（≥40 ms 记 warn） |

**验证**：修复前无 `HIDE`、窗口残留；修复后 `HIDE elapsed=8ms` + `SHELL 预览已卸下 1 个`，窗口消失；
注入 60 步拖动窗口位移精确 `180,120`；`cargo test` 195 passed、clippy 0、exe 2,033,152 B。
## §35 1.7.3 压缩包列表视图（锁顺序 + 目录树 + 点击/滚轮）

**一句话**：黑屏不是绘制坏，是 `paint_archive()` 拿着 `archive_slot` 的锁去调 `archive_rows()`，
后者也要同一把锁 → `Mutex` 不可重入 → UI 线程永久阻塞（窗口一片黑、点哪儿都没反应）。
改法是"锁内只拷字段、放锁再调下一层"，然后才把 `repaint()` 里的 `if false` 打开。

| 位置 | 改动 |
|---|---|
| `crates/qlr-app/src/app.rs` `paint_archive` | 锁内只取 `title/selected/scroll`，`archive_rows()` 与 `frame()` 都在放锁之后 |
| 同上 `repaint` / `has_archive_view` | 打开列表通道；顶栏 hover/pin 与文本通道共用，补 `sync_shadow()` |
| 同上 `ArchiveNode` / `build_archive_tree` | 条目表 → 预建目录树：补齐隐含父目录、目录在前按名排；展平只做一趟 DFS |
| 同上 `archive_click` / `archive_scroll_by` | 点行选中 + 目录翻转展开（并吃掉点击，避免变成拖窗口）；滚轮一格 3 行、到头不冒泡 |
| 同上 `frame` | 压缩包单独算滚动条（内容单位=展平行） |
| 同上 `show_preview` | `renderable` 放行压缩包；坏包响铃、不显示 |
| `crates/qlr-platform/src/window.rs` `paint_list` | 标记只用 ▾/▸（GDI 不做 emoji 回退，📁 是豆腐块） |
| `scripts/verify-archive.ps1` | 新增：11 项端到端（日志 + 三列像素 + 折叠/展开 + 滚轮 + 四扩展名 + 坏包） |

**验证**：`verify-archive.ps1` 11 PASS · 0 FAIL；`cargo test` 194 passed；fmt/clippy 0；
`smoke.ps1` 16 PASS + 1 SKIP（托盘 = 管理员会话 UIPI）。截图见 `_bench/archive/`。

**下一步（按顺序）**：
1. ③ PPT 预览"最后一页"提示 + **顶栏文件名错乱**——先查"为什么 Office 处理器没起来、退回了插件"；
2. ⑤ **拖边改窗口大小失效**——打日志确认 `edges_at()` 是否被 `on_chrome_click()` 抢先 `return`；
3. 列表视图打磨：双击/回车用系统程序打开条目、拖滚动条、按名字过滤。

## §36 1.7.4 Office 预览三件修复（PPT 翻页接管 / 顶栏标题时序 / 拖边改大小）

用户反馈两条：① PPT 看到最后一页是黑屏「幻灯片放映结束，单击鼠标重新放映演示文稿。」；
② 顶栏文件名"一直都是错的"。做这两条时又暴露第三条：在 Office 预览上拖边改窗口大小完全没反应。

### 根因（三条都不是"取值错"，是时序/样式/命中测试）

| 症状 | 根因 |
| --- | --- |
| PPT 最后一页黑屏 | PowerPoint 的"放映"翻过末页自己切黑屏；处理器没有"跳页"接口 |
| 顶栏文件名错乱 | 记 `last_path` 在 `show_at` **之后**；`DoPreview()` 的跨进程 COM 泵消息期间 `WM_PAINT` 按旧标题画一帧，之后无 invalidate |
| Office 上拖边无效 | 处理器子窗口铺满内容区，边缘的鼠标进不到预览窗（`WindowFromPoint` 返回子窗口） |
| 翻页后画面不动 | `create_preview_window` 缺 `WS_CLIPCHILDREN`：我们填底色擦掉子窗口像素 |

### 改了什么

| 文件 | 改动 |
| --- | --- |
| `crates/qlr-platform/src/window.rs` | 预览窗样式加 `WS_CLIPCHILDREN`；`Chrome::toast` + 顶栏气泡；标题宽度按 `chrome.page.is_some()` 让位；`set_child_input`/`post_key`/`largest_descendant` |
| `crates/qlr-platform/src/shellpreview.rs` | `nav`（面积最大的后代窗口）、`input_off`、`take_over_input()`、`step(next)`（VK_NEXT/VK_PRIOR）、`forward_wheel(delta)` |
| `crates/qlr-platform/src/dpi.rs` | `window_dpi(hwnd)` + `scaled(hwnd, dip)` |
| `crates/qlr-app/src/app.rs` | `state.toast/slide/shell_input_pending`；`TIMER_TOAST`/`TIMER_SHELL_INPUT`；`show_toast`/`current_toast`；`last_path` 提前 + 标题日志；`edges_at(.., margin)` + `resize_margin`；`content_size_of`；`finish_resize` 用它；`is_slides`/`slide_count`/`step_slide`；所有 Office 预览都接管输入；`forward_wheel_to_shell` 改投 `nav` |
| `scripts/verify-office.ps1` | 新增：15 项端到端（标题时序 / 处理器接管 / 内容非空白 / 数页 / 滚轮翻页 / 末页不黑屏 / 两格式拖边 / 内容高度=客户区−30 / 边缘命中自己） |

### 验证

`verify-office.ps1` **15 PASS · 0 FAIL**；`cargo test` **202 passed**；fmt/clippy 0；
`smoke.ps1` 16 PASS + 1 SKIP（托盘 = 管理员会话 UIPI）；`verify-archive.ps1` 11 PASS。
截图见 `_bench/office/`（`04-ppt-last.png` = 第 3 页 + 顶栏「已是最后一页」）。

### 下一步（按顺序）

1. 列表视图打磨：双击/回车用系统程序打开条目、拖滚动条、按名字过滤；
2. PPT/Word 预览的**键盘**：目前只接了 PageDown/PageUp/滚轮，`Home/End`、方向键还没接；
3. 已知遗留：`toast` 只服务 PPT 边界，其余"一闪而过的提示"（复制路径成功等）还没走同一条通道。

## §37 1.7.5 换文件没卸处理器（旧子窗口盖住新内容）

### 症状

用户截图：预览 `tree-demo.zip` 的列表视图，右侧一大片竖条纹，配文"预览窗口拖动会这样"。
本地复现同一根因的另一副面孔：顶栏是 `plugin-demo.zip`，正文还是上一张 Excel 表格。

### 根因

`open_shell_preview()` 只在"这次也走系统预览处理器"时才会 retire 上一个 `ShellPreview`。
切到**不需要处理器**的文件（压缩包 / 图片 / 文本）时它不被调用 → Office 处理器的子窗口
（`QLRust.ChildHost` → PowerPoint/Excel 那层）留在预览窗里继续盖着内容区；我们的列表画在它下面。
拖动/改大小时那片残留由别人的进程按旧尺寸重绘 → 竖条纹。
压缩包分支还**提前 return**，根本走不到收尸代码。

### 改了什么

| 文件 | 改动 |
| --- | --- |
| `crates/qlr-app/src/app.rs` | 新增 `drop_stale_shell_preview()`（retire + 清 `slide`/`shell_input_pending` + KillTimer + 调试日志）；调用点：`show_preview` 压缩包分支 + 主路径 `!shell_wanted` |
| `scripts/verify-office.ps1` | 15 → **21 项**：新增 6 项换文件断言（处理器子窗口存在→切换后为 0、日志 `换文件：卸掉…`、切换后画面是新内容、图片主路径同类断言） |

### 验证

`verify-office.ps1` **21 PASS · 0 FAIL**（切换前子窗口 4 个 → 切换后 0 个）；`cargo test` 202 passed；
fmt/clippy 0；`smoke.ps1` 全过；`verify-archive.ps1` 11 PASS。

### 下一步

1. 列表视图打磨：双击/回车用系统程序打开条目、拖滚动条、按名字过滤；
2. Office 预览键盘只有 PageUp/PageDown + 滚轮，`Home/End`、方向键还没接；
3. `toast` 通道目前只服务 PPT 边界提示。

## §38 1.7.6 跟随延迟与 Office 切换的割裂感

### 症状（用户）

1. 预览窗开着，点别的文件 → 内容换得慢；
2. Office 预览切换时"偶尔还跳回占位页"，割裂感最重。

### 延迟：两处白等

| 位置 | 旧 | 新 |
| --- | --- | --- |
| 跟随线程轮询 | 每 300 ms 查一次（且先睡后查） | 窗口开着 **90 ms**、收着 250 ms，睡眠扣查询开销 |
| UI 拿结果 | 400 ms 计时器轮询槽位 | 线程 `PostMessage(WM_QLR_FOLLOW)` → 立刻切；计时器变兜底 |
| 查询本身 | 每次新建 `IShellWindows` + 完整枚举 | 线程级缓存 + "上次命中的 browser" 快路径 |

实测（`_bench/follow-latency.ps1`，真 `SendKeys{Down}` 改选中项）：**41 ms 平均 / 72 ms 最差**；
旧二进制对照 **419 ms / 440 ms**。

### 割裂感：`DoPreview` 那 130–260 ms 里露出的是我们的占位页

- `hold_frame` + `window::paint_chrome_strip`：换文件时只重画顶栏，内容区像素冻住；
- `shell_cover` + `raise()`：旧处理器顶到最上层继续盖住，`TIMER_SHELL_SWAP`(260 ms) 再卸。

抓帧对比：切换后 137 ms 是空白 Word 窗口（Office 自己的首帧），199 ms 已是正文；
我们的文字占位页不再出现。

### 验证

`cargo test` 202 passed；fmt/clippy 0；`verify-office.ps1` 21 PASS；`smoke.ps1` 全过；
`verify-archive.ps1` 11 PASS。

### 下一步

1. Office→Office 仍剩 ~60 ms 白底（Office 会拆掉旧处理器窗口）：可试"新宿主先隐藏、
   在后台画好再显示"，需先验证隐藏窗口下 `IPreviewHandler` 是否照常绘制；
2. 列表视图打磨：双击/回车打开条目、拖滚动条、按名字过滤；
3. Office 预览键盘只有 PageUp/PageDown + 滚轮，`Home/End`、方向键还没接。

## §39 1.7.7 Excel 预览卡死（占位页 + 滚动条不可用）

### 症状（用户）

预览 xlsx 时"还有那个占位页"，滚动条完全点不动；用户还提到 WPS 与 Office 预览不一样。

### 根因：`DoPreview` 之后的 `SetWindow` 死锁

`open_shell_preview` 返回后紧跟 `sync_shell_rect()` → `IPreviewHandler::SetWindow`。
Excel 的预览器是 **OLE 就地激活**对象，`DoPreview()` 返回时激活还在进行，
此时同步回调容器就是**自等自**：进程 `Responding=False`，日志停在 takeover 那行，
屏幕上是我们的占位页，鼠标全部无响应——"滚动条不可用"其实是**整个窗口没响应**。

### 修复

| 改动 | 说明 |
| --- | --- |
| 矩形提前定 | `shell_content_rect()` 在 `open()` 之前算好（内缩 10 DIP 留给拖边热区），打开后不再调 `SetWindow` |
| 就绪轮询 | `TIMER_SHELL_INPUT` 每 120 ms 查 `visible_area ≥ 50%`，最多 3 s；Excel 实测 121 ms 就绪（之前死等 400 ms 会先露出占位页） |
| 允许激活 | `window::set_activatable()`：仅 Office 预览清掉 `WS_EX_NOACTIVATE`，滚动条可拖 |
| 滚轮放行 | `hotkey::set_wheel_pass_through()`：Word/Excel 预览时不吞滚轮 |

### 验证

`Responding=True`、`SHELL 就绪：内容占 93%（轮询 1 次）`；拖动滚动条实测生效；
`cargo test` 202 passed、fmt/clippy 0、`verify-office.ps1` 21 PASS、`smoke.ps1` 全过、
`verify-archive.ps1` 11 PASS。

### 仍未解决

Excel 预览的**滚轮**（钩子放行 / `WM_MOUSEWHEEL` / `WM_VSCROLL` / `VK_NEXT` 均无效），
而资源管理器预览窗格里同一文件可滚 —— 宿主激活/焦点差异，下一步试"把焦点交给 Excel 子窗口"
或"UIA 读滚动条 range 后自己设值"。见 CONVENTIONS #138。

## §40 1.7.8–1.7.9 四类新预览（音频/视频/SVG/HTML）+ 三处卡死/吞点击

### 交付

| 能力 | 实现 | 实测证据 |
| --- | --- | --- |
| SVG | `qlr-render/src/svg.rs`（D2D 光栅化 + 纯 Rust 兜底；渐变/圆角/路径） | `_bench\media\01-svg.png`，彩色像素 37366 |
| HTML | `qlr-render/src/html.rs`（标题/段落/列表/代码块/实体还原/`<pre>` 保留缩进） | `_bench\media\02-html.png`，左侧 30 px 无行号栏 |
| 音频 | `qlr-platform/src/audio.rs`（MCI）+ 底部 52 px 进度条 + `P`/`←`/`→`/滚轮 | `_bench\media\03-audio.png`，日志 `AUDIO 播放结束（2000ms）` |
| 视频 | `qlr-render/src/media.rs`（MF：`probe`/`first_frame`/`FrameStream` 逐帧） | `04-video-a.png` vs `04-video-b.png` 差异 412 采样点 |

- 格式路由 4 → 8 类（`format::KINDS`），`TEXT_EXTS` 去掉 html/htm/xhtml。
- 图片等比居中：`window::paint_bitmap_fit`，`Image|Svg|Video` 适用。

### 修掉的三处卡死/吞点击（都比新功能更值钱）

1. **持锁调 `SetFocus` 自锁死** → Word 预览点一下即 `Application Hang`（CONVENTIONS #139）。
   句柄锁内取、`SetFocus` 锁外调 + 重入闸门。
2. **`WM_MOUSEACTIVATE` 返回 2（`MA_ACTIVATEANDEAT`）** → 第一次点击被吞，拖边只有第二次生效
   （CONVENTIONS #140）。改返回 1。
3. **`SetCurrentPosition` 传 NULL GUID** → `STATUS_ACCESS_VIOLATION`（CONVENTIONS #141）。
   改传 `GUID::zeroed()`。

顺带：视频整块铺底擦掉进度条（`picture_area` + `reserve_bar`，CONVENTIONS #143）；
`AUDIO_FILL_RGB` 颜色写反（COLORREF 是 `0x00BBGGRR`，CONVENTIONS #142）。

### 回归数字（2026-11-01 夜）

- `cargo test` **244 passed / 0 failed**；`cargo fmt --all -- --check` 0；
  `cargo clippy --all-targets -- -D warnings` 0。
- `scripts/verify-media.ps1` **15 PASS · 0 FAIL**（新增）。
- `scripts/verify-office.ps1` **22 PASS · 0 FAIL**。
- `scripts/verify-archive.ps1` **11 PASS · 0 FAIL**。
- `scripts/smoke.ps1` 全部通过（托盘 SKIP 仍是管理员会话 UIPI）。

### 已知缺口

- 视频**无声**（MCI 播不了 mp4，见 CONVENTIONS #144）；要出声需 `IMFMediaSession` + EVR / WASAPI。
- Excel 滚轮依赖"先点一下"（OLE 预览固有行为，与用户要求一致）。
- 发布阻断项（dist 678 MB、根目录 LICENSE、15 处绝对路径、README/rc 版本、`.gitattributes`）
  仍未动，按用户定的"最后一步"处理。

## §41 1.8.0 Office 预览改大小留白边 / 画框与阴影不一致

### 用户症状（原话）

「窗口伸缩会引起白边这个问题，之前是没有的哦」、「1.78那会好像也没有问题，之前只是滚动有问题，
窗口拉伸没问题」；随后又发截图「你看这个画面，阴影和画框不一致」（预览窗旁边多一块半透明白块）。

### 定位（先证明"不是 1.7.9 引入的"）

用 `git worktree add ..\_old178 7a72f30` 单独编 1.7.8，同一探针量 `big-sheet.xlsx` /
`office-demo.docx` 三档尺寸，右缘 40 px 纯白占比两版都是 **100%** → 与 1.7.9 无关；
根因是"Office 走系统预览处理器"这条路（1.7.6/1.7.7 起）本身：处理器自建的子窗口
（`EXCEL9 → EXCEL7`）不随我们的窗口长大。1.7.2 时代 Office 走插件位图渲染，所以那时候没问题。

### 修法（CONVENTIONS #145 / #146）

1. `WM_SIZE` 里只 `window::post(ipc::WM_QLR_SHELLFIT)` —— 在输入同步呼叫里直接调 COM 会被
   `0x8001010D`（`RPC_E_CANTCALLOUT_ININPUTSYNCCALL`）拒。
2. 消息到点时**把处理器句柄从 `State` 里 `take()` 出来、锁外调 `SetRect`、再放回去**；
   持锁调会自等（`SetRect` 把消息泵回我们自己的窗口过程）→ 之前几次 `Application Hang`（1002）就是这条。
3. `WM_SIZE` 里同步投影窗口（`sync_shadow_with(true)`，拖动中限流 8 ms），
   `finish_resize` 再精确对齐一次。

**不重开预览**（用户明确否掉"缩放重新渲染"：慢、费资源）——只是让处理器重新排版，
不 `DoPreview`、不丢页码、不闪。

### 验证数字（2026-11-01 夜，release）

- 白边：`_bench\shell-resize-probe.ps1` xlsx 右缘纯白 **78.5%（未改尺寸基线）/ 78.8% / 79.7%**；
  docx 正文铺满整宽（`_bench\shell-resize\final2-docx-b-1040x520.png`）。
- 稳定：进程 `Responding=True`，事件日志无新增 1002。
- Excel 交互：滚轮 True / 横向滚动条 True / Shift+滚轮 True / 拖条 True。
- 阴影对齐：1040x660 窗口配 `ShadowWindow 1072x692`（+32 / -16）。
- `verify-office.ps1` **23 PASS · 0 FAIL**（新增"拖边后处理器重新对齐尺寸"）、
  `verify-archive.ps1` 11 PASS、`verify-media.ps1` 15 PASS、`smoke.ps1` 全过、
  `cargo test` 244 passed、fmt/clippy 干净。

### 放弃的路（别再走）

1. `WM_SIZE` 里同步 `SetWindow/SetRect` —— 被 `0x8001010D` 拒。
2. 投递消息 + 持锁调 `SetRect` + 手工撑 `EXCEL9` + `fill_child` 兜底 —— 卡死（1002）。
3. 改完大小**重开预览** —— 能用但用户否掉。
4. 进程外 `prevhost` 承载（`CoGetClassObject` + `IClassFactory` 缓存 + `LockServer`）——
   白边同样消失，但 **Excel 滚动条拖不动**（拖条 False），退回进程内。

## §42 1.8.1 Excel 点选单元格 / 图片视频锁窗口比例 / 去掉占位页

### 用户诉求

1. 「Excel焦点聚焦到预览窗口的时候，鼠标左键无法选中目标单元格。」
2. 「图片、视频类都不需要拉伸缩放窗口，设计和图片一样的」+ 更早一条：「图片类型取消窗口的拉伸缩放，且窗口比例固定预览图片比例，同时注意不能破坏图像撑满窗口的设计」。
3. 「Excel预览窗口打开的时候我还是看到了一个占位页面，需要把占位页面直接删掉」。

### 根因与修法

| 症状 | 根因 | 修法 |
| --- | --- | --- |
| Excel "看得见、点不动" | 焦点只到容器层 `EXCEL9`，`EXCEL7` 从未 UI 激活；投合成鼠标消息无效 | `IPreviewHandler::SetFocus()`（`ShellPreview::set_com_focus`），只能在 `TIMER_SHELL_INPUT`（退栈后）调；`WM_MOUSEACTIVATE` 只返回 `MA_ACTIVATE` |
| Excel 输入被整个禁掉 | `take_over_input()`（`EnableWindow(FALSE)`）连子窗口一起禁 | 只给 PPT 用；Excel/Word 按 `shell_owns_input` 分流 |
| 图片被拉伸/留白 | 1.7.8 的 `paint_bitmap_fit` 方向错了（让图片适配窗口） | 反过来：**窗口锁素材比例**（`media_aspect` + `media_window_size` + `clamp_media_resize`），图片走铺满 |
| 视频比例偏 0.05+ | 内容高没扣底部进度条 | `bar = window::AUDIO_BAR_H`（52），只在视频上扣 |
| 打开时闪"文件信息页" | 系统处理器冷启动要几百 ms，我们画了占位页 | `shell_candidate` 命中就 `lines.clear()`（空内容区等首帧） |

### 验证（`scripts/verify-mediashape.ps1`，新增，13 PASS · 0 FAIL · 0 SKIP）

- Excel 左键点一下就选中单元格：点击前后画面差 **0.67%**（阈值 0.3%）；
  截图 `_bench/shape/excel-before-click.png` / `excel-after-click.png`。
- Excel 改大小后右缘无白边：右缘 40px 纯白占比 **41.4%**（1.8.0 的修复没退化）。
- 图片：`1.778 → 1.777 → 1.778`；视频：`1.778 → 1.777 → 1.778`（已扣 52px）。
- 回归：`verify-office.ps1` 23 PASS、`verify-archive.ps1` 11 PASS、`verify-media.ps1` 15 PASS · 0 SKIP、
  `smoke.ps1` 全过（托盘 SKIP=管理员会话 UIPI）、`cargo test` **249 passed**、fmt/clippy 干净。

### 踩坑

1. `IPreviewHandler::SetFocus` 在 `WM_ACTIVATE`/`WM_SETFOCUS` 里必被拒（`0x8001010D`，输入同步呼叫）。
2. 在 `WM_MOUSEACTIVATE` 里调能成功，但 COM 泵消息会把这一次点击吃掉（用户要连点两下）。
3. `smoke.ps1` 的 `Get-Content -Raw` 偶发返回数组 → `Check` 报 "无法将值 System.Object[] 转换为类型 System.Boolean"；改用 `-join "`n"`。

## §43 1.8.2 图片/视频去掉拖边改大小

用户 1.8.2 要求：「把图片和视频的窗口伸缩功能去掉，滚轮对窗口进行缩放就足够了。」

| 点 | 做法 |
| --- | --- |
| 拖边不响应 | `resizable_kind()`：`Image \| Video` → false；`resize_margin()` 返回 0 |
| 0 热区别留缝 | `edges_at()` 在 `margin <= 0` 时直接 `ResizeEdges::default()`（否则正好压线还能抓到 0 宽的边） |
| 光标 | `WM_SETCURSOR` 与 `WM_LBUTTONDOWN` 共用 `resize_margin`，所以不会变成双向箭头 |
| 开窗大小 | 仍按素材比例定一次（`media_window_size`，视频扣 52px 进度条） |
| 外部改大小 | 新增 `window_aspect_differs()`：窗口比例和图片差 >2%（最大化等）就退回 `paint_bitmap_fit` 等比居中 |
| 兜底 | `clamp_media_resize` 与 `on_resize_move` 里那条比例分支保留 |

验证：

- `_bench\noresize-probe.ps1`：图片开窗 `707x428`，拖右/拖下后仍 `707x428`；滚轮前后画面差 **56.48%**（能缩放）。
  视频 `1109x706` 拖边不变。
- `scripts\verify-mediashape.ps1` **13 PASS · 0 FAIL · 0 SKIP**（新增两条"拖边不改窗口大小"：
  图片 `640x390 → 640x390`、视频 `640x442 → 640x442`）；Excel 点选/白边/占位页仍全 PASS。
- 非媒体类型没受影响：`verify-office.ps1` **23 PASS**（含 docx/pptx 拖边改大小）。
- `cargo test` **251 passed**、fmt/clippy 干净。

备注：视频滚轮仍是快进/快退（一格 5 秒，1.7.9 既定行为），图片滚轮才是缩放。

## §44 开源发布准备（1.8.2 之后）

**一句话**：功能面收尾，转入"能公开、能上 GitHub"。历史上第一次有 `LICENSE` / `NOTICE.md` / `.gitattributes`，
`dist/` 不再入库，仓库里不再有开发机绝对路径，并补了一份给 AI 的完整协作手册 `AGENTS.md`。

| 点 | 结果 |
|---|---|
| 许可 | 根 `LICENSE`（GPL-3.0 全文）+ `NOTICE.md`（独立重写声明、上游 QuickLook 关系、第三方组件） |
| 体积 | `.gitignore` 挡掉 `dist/`、`*.zip`；原 `size-pack` 71.14 MiB / 跟踪 473 文件 |
| 路径卫生 | 22 处文档 + 3 个脚本 + 2 处单测回退路径洗白；`git grep 'Users.Administrator'` **0 命中** |
| 脚本可移植 | 夹具目录三级回退：显式参数 > `$env:QLRUST_FIXTURES` > 开发机默认；截图默认落 `%TEMP%` |
| AI 手册 | `AGENTS.md` 重写（14 节：地图 / crate / 分层 / 类型矩阵 / 硬约定 / 改码 / 日志 / 构建测试 / 提交 / 发布 / 文档义务 / 禁区 / 坑速查 / 协作） |
| 版本 | 1.8.2（`Cargo.toml` / `qlrust.rc` / `README.md` 三处一致） |

验证：`cargo test` **251 passed / 3 ignored**、fmt/clippy 干净；顺手修掉发布准备自己引入的一条回归
（MCI 不认带 `..` 的路径，CONVENTIONS **#151**）。

发布：`git checkout --orphan publish-1.8.2` → 单条初始提交 → `gh repo create QuickLook-R --public --push`；
详见 `docs/dev/HANDOFF.md`「发布准备记录」。

## §45 1.8.3-fix 批次一（评审 7 项：收 5 项 + 自加 1 项，撤 1 项）

**一句话**：一轮全仓评审之后挑"小改动、高确定性"的 6 项落地，不改变行为契约、不碰渲染热路径；
评审里那条 `mem::forget` 的"修复"是拿真泄漏换假收益，没要。分支 `fix/1.8.3-batch1`（基于 `71999be`，7 个提交，快进关系）。

| # | 点 | 做法 | 为什么 |
| --- | --- | --- | --- |
| 1 | 嗅探全量读 | `crates/qlr-core/src/format.rs::kind_for_path` 改 `File::open` + `read(&mut [0u8; 512])` | 只取前 512 B 却 `fs::read` 整个文件，12 个调用点 |
| 2 | 热键级 2 是死代码 | `GetFocus` → 同一次 `GetGUIThreadInfo` 输出里的 `gti.hwndFocus` | `GetFocus` 线程亲和，钩子线程恒 NULL；1.8.3 声称的"前台打字时不吞空格"从未生效 |
| 2 | 钩子句柄泄漏 | `HotkeyGuard::stop()` 在 join 之后 `CloseHandle(stop)` 并归零；`start()` 的 spawn 失败路径补关 | 每进程泄漏 1 个内核事件 |
| 3 | 压缩包前缀泄漏 | `crates/qlr-render/src/archive.rs` 两处插入点先 `contains` 再 `leak` | 重复值被 `BTreeSet` 丢弃但 `leak` 已经发生 → 确定性泄漏 |
| 4 | `PageCache` 没有文件戳 | `PageKey` 加 `mtime`/`len`，`new()` 加 `stamp: (u64,u64)`，4 个调用点统一用 `file_stamp()` | 预览中文件被覆盖 → 翻页/翻回命中旧像素（与 CONVENTIONS #112 同坑） |
| 5 | 预取重复查戳 | `prefetch_neighbor_pages()` 直接沿用 `key.mtime` / `key.len` | 省一次 metadata，且避免与触发它的 key 版本不一致 |
| 6 | zip 里没有校验和 | `scripts/pack.ps1` 先生成 `SHA256SUMS.txt` 再 `Compress-Archive` | 清单原本在压缩之后才写 |
| — | `ShellPreview::close()` | **不收**。`close()` 零调用点，实际收尸走 `app/shell.rs` 的 `drop(pending)` | `std::mem::forget(self)` 会跳过 `handler: IPreviewHandler` 的 Drop → 每次漏一个 COM 引用 |

验证：

- `cargo fmt --all -- --check` / `cargo clippy --all-targets -- -D warnings`：零输出
  （本批自带一处 fmt 违规——`render.rs:522` 单行超长、`hotkey.rs:34` import 换行——已单独 `style(fmt)` 提交修掉）。
- `cargo test`：**287 passed / 4 ignored**（1.8.3 基线 286 + `pagecache::tests::stale_stamp_does_not_hit`）；
  `cargo build --release` 通过。
- 端到端（release 构建，本机交互式会话）：`smoke.ps1` **16 PASS · 1 SKIP · 0 FAIL**（SKIP = 管理员会话
  UIPI 拒托盘图标，非回归）、`verify-archive.ps1` **11 PASS · 0 FAIL**、`verify-pdf.ps1` **PASS**。
  `verify-office.ps1` 未跑——本批不碰 Office 路径。
- **必须人工确认的两条**：① 几 GB 的 `.iso` 改名成 `.xyz` 后按空格，任务管理器内存不飙升；
  ② Win11 桌面 F2 就地改名之后按空格能打出空格，`app.log` 里 `text_focus_level=2`、`passed_text_focus=` 大于 0。

范围外（评审的批次二，未开工，也是真正影响体感的那部分）：JPEG 解码不进 `source_cache`、
PDF 每次预览把全文件读 3 遍、md/html 全文排版、锁中毒三套策略。

## §46 1.8.3-fix 批次二（把"每次重来"的开销收掉，7 个提交）

评审报告 🔴 分组里**用户真能感觉到**的那部分。每项都先建 `#[ignore]` 基准、拿基线、再改。

| # | 提交 | 改了什么 | 实测（release） |
|---|------|----------|------------------|
| 1 | `4352c0f` | JPEG 按**解码档位**缓存：`jpeg-decoder` 的 `scale()` 返回的实际解码尺寸当档位标签，`source_cache` 加 `Tag`；命中就只缩放不解码 | 4K 四档合计 170.0 → 90.5 ms（−47%）；48MP 367.6 → 78.1 ms（−79%） |
| 2 | `a2a6439` | PDF **文档单槽缓存**：一次预览原本读盘 3 遍 + 解析 3 遍，现在只读一次（>256 MiB 不常驻） | `render_page` 3.22 → 1.25 ms；冷加载 0.54 ms / 命中槽 0.01 ms |
| 3 | `d38506a` | Markdown **排版按预算早停**（`layout_capped`），`total_lines` 仍报源文件行数 | 8 MB / 29.7 万行，`head` 400 行 356.1 → 15.7 ms |
| 4 | `c0af9f5` | HTML 隐藏元素**一次扫描剥离**（原来 13 个标签各扫全文件）+ `find_ci` 首字节粗筛 | 8 MB 预处理 145.0 → 8.8 ms；`head` 260.0 → 120.7 ms |
| 5 | `f76e1ec` | 缩放内循环：整行切片 + `chunks_exact(4)` + 局部累加器（去掉逐字节边界检查） | 4K → 3178 51.4 → 46.1 ms；48MP → 3178 100.6 → 71.2 ms |
| 6 | `58c53d2` | 新增 `alloc-stats` 特性：记账分配器 + `RENDERALLOC gen=N bytes=… live=…`（**默认不编**，不动 `RENDER` 行字段顺序） | 冷帧 65.7 MB / 热帧 5.6 MB（≈ 位图 5.4 MB）/ live 稳定 19.2 MB |
| 7 | `57fdcb3` | `bench.ps1` 补 `PeakPrivateMB` / `GdiAfterHide` / `UserAfterHide` / `AllocKB` / `LiveKB`，加 GDI 泄漏判据与 `-Passes` 浸泡 | 4K 6 次开合 GDI 16 → 20 → 20…**无泄漏** |

**故意没做的两件事**：

- **HTML 不早停**：状态行的"全文共 N 行"是排版后的显示行数，排一半算不出准确值
  （Markdown 报的是源文件行数，才不受影响）。几十毫秒不值得让界面开始撒谎。
- **`RENDER` 行不加字段**：那行的字段顺序被 `bench.ps1` 的正则和现场 grep 依赖着，
  帧级分配另起一行 `RENDERALLOC`。

**留到下一批**：HTML 的 tokenize（还是整篇 33 ms，要流式词法）／帧缓存与内存预算闸门
（STRUCTURE-REVIEW §6-7、§6-9）／持锁动窗口（§7-1）／几何单点化（§7-2）／PDF 页缩略图／
文本 mmap／A1.6 第三四级兜底。

验证：

- `cargo fmt --all -- --check`、`cargo clippy --all-targets -- -D warnings`：零输出
  （`--features alloc-stats` 下 clippy 也零）。
- `cargo test`：**290 passed / 9 ignored / 0 failed**（app 62、core 54、platform 33+3、
  plugin-api 4、plugin-demo 7、plugin-office 11、render 109+6、fuzz 1、malformed 9）。
  新增 5 个 `#[ignore]` 基准（`bench_jpeg_zoom_steps` / `bench_pdf_costs` / `bench_md_phases` /
  `bench_html_phases` / `bench_downscale`）+ 1 个分配计数自测。
- 端到端（release）：`smoke.ps1` **16 PASS · 1 SKIP · 0 FAIL**、`verify-archive.ps1` **11 PASS**、
  `verify-pdf.ps1` **PASS**（SHOW 8ms / RENDER 2ms）。`verify-office.ps1` 未跑（本批不碰 Office）。
- **必须人工确认**：200% 缩放下看 4K 图滚轮缩放、PDF 翻页、md/html 长文档首屏，
  视觉与上一版一致（本批没动几何/DPI 逻辑，理论上不受影响，但没有真机视觉基线）。
