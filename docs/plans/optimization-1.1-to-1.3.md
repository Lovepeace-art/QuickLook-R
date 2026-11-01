# QLRust 性能/稳定性优化计划（1.1.x – 1.3.x）

> 立项依据：`docs/bench-vs-quicklook-1.0.14.md`（同机对比实测）。
> 决策记录（2026-11-01）：
> - 二期「投机预渲染」**同意做，但设置页必须能关**；用户判断"16 ms → 0 ms 人类感知提升有限"，且随着格式增多常驻内存会变大，此项收益可能逐步缩小甚至成为累赘 → 因此**默认走自适应（adaptive）且可一键关闭**，并把它做成"随时可删的独立层"。
> - 五期（GPU 呈现 + PDF 分块 + mipmap）**排进计划，先看效果**。
> - 一期里原列的"pdfium 延迟加载"经核实**已经是现状**（`crates/qlr-render/src/pdf.rs:147` 的 `static API: OnceLock<...>`，只有第一次真正要渲染 PDF 时才 `load_api()`；实测常驻 private 1.8 MB 就是它没被映射的证据），故该项降级为"可选的空闲预热"。

## 0 基线（1.0.14，本机实测）

| 指标 | 值 |
|---|---|
| exe / 分发 | 1,929,216 B（1.84 MB）/ 9.2 MB 6 个文件 |
| 冷启动首帧 | 39–41 ms |
| 热路径首帧 | 16 ms（图片/PDF/MD）、40 ms（27 MB 文本） |
| 常驻 RSS / Private | 8.7–8.8 MB / 1.8 MB |
| 48MP 峰值 | 16 MB |
| 测试 | 166 项全绿；malformed 7 条 + 318 例 fuzz；样本矩阵 132 个真实文件 100% |

## 1 红线（每期都必须满足）

1. exe ≤ 5 MB；分发物 ≤ 12 MB（现 9.2 MB，其中 pdfium 7.2 MB）。
2. 不引入运行时依赖；新 crate 必须体积增量 < 200 KB，且不得拉进 tokio / wasm-bindgen / 字体引擎一类重依赖。
3. 每期关卡：`cargo fmt --check` 0、`cargo clippy --all-targets -- -D warnings` 0、`cargo test` 全绿、`scripts/smoke.ps1` ≥ 17 PASS，外加本期的专用回归脚本。
4. **每期都必须可回退**：配置项或编译期开关，关掉后行为与上一版完全一致。
5. 关窗后内存回落：可见预览关闭 3 s 内 RSS 回到基线 +10 MB 以内。
6. 每期独立提交 + dist + 截图 + `docs/dev/{LOG,STATUS,CONVENTIONS}` 同步。

---

## 一期：低风险提速与精确定位（→ 1.1.0）

> **实施时核实（2026-11-01，1.1.0 交付时回填）**：A1 与 A3 **在 1.0.x 就已经是现状**，不需要做——
> `run()` 只在启动时 `create_preview_window` 一次、`hide_preview()` 只 `window::hide()` 不销毁；
> `pdf::api()` 本来就是 `OnceLock` 懒加载（第一次真渲染 PDF 才 `LoadLibrary` pdfium.dll）。
> A2 也**降级为空过**：`s.scroll` 本来就是"源行"单位，滚动条按 `scroll / 总行数` 映射（和所有编辑器一致），
> 折行只影响"一屏几行"，不影响"文档位置比例"；`--goto-line` 是纯锦上添花，压缩包里没有别的期依赖它。
> 所以一期**没有产出，直接进二期**。这条更正写在这里，避免以后有人按计划书去找不存在的 `REUSE window` 日志。

### A1 窗口热池（只做 A 档：保留窗口，不保留内容）——**已是现状，跳过**

- 现状：`hide_preview()` 里 `window::hide()`，但每次 `show_preview` 都要重走建窗/测量/字体选择。
- 方案：预览窗**常驻不销毁**，隐藏时只 `ShowWindow(SW_HIDE)`；`show_preview` 加"复用分支"，跳过 `create_preview_window`/`register_class`。内容仍按需重渲染（不保留上一帧，避免与二期预热重复占内存）。
- 落点：`crates/qlr-app/src/app.rs`（`run()` 初始化建一次窗；`show_preview`/`hide_preview` 改写）、`crates/qlr-platform/src/window.rs`（如需 `show_without_activate`）。
- 验收：连续开关 20 次，第 2 次起日志出现 `REUSE window`，热路径首帧 ≤ 14 ms；GDI 对象数不随次数增长。
- 回归脚本：`_bench/verify-reuse.ps1`（开关 20 次，断言窗口句柄不变、`GetGuiResources` 不增长、RSS 回落）。

### A2 文本稀疏索引 ——**降级：跳过**（理由见本节开头）

- 现状：折行后"显示行 ≠ 源行"，滚动条拖动只能按比例猜；跳到大文档某一处要现场重折。
- 方案：`crates/qlr-render/src/wrap.rs` 新增
  ```rust
  pub struct LineIndex { /* 每 512 源行一条：(src_line, wrapped_rows_before) */ }
  impl LineIndex { pub fn build(lines, width, measure) -> Self; pub fn rows_before(&self, src_line) -> usize; pub fn total_rows(&self) -> usize; }
  ```
  只在**内容装载时**建一次（宽度变化才重建）；`app.rs` 的滚动条映射与新增的 `--goto-line <n>` 用它。
- 落点：`wrap.rs`（索引）、`app.rs`（`scroll_info`/`apply_scroll_y`/新 CLI）、`crates/qlr-app/src/cli.rs`。
- 验收：121,425 行文本，滚动条拖到 50%/80% 后状态行行号误差 ≤ 1 屏；`--goto-line 100000` ≤ 5 ms；索引内存 ≤ 4 KB（日志打印）。
- 回归脚本：`_bench/verify-index.ps1`。

### A3 空闲预热 pdfium ——**已是现状（懒加载），跳过**（默认关的可选项也未做）

- 方案：配置 `preload_pdfium: bool`（默认 false）。开启后启动 5 s 无操作，后台线程调一次 `qlr_render::pdf::warm_up()`（新增，内部 `api()` 一次）。
- 验收：开启后首个 PDF 首帧比关闭时快 ≥ 5 ms；关闭时启动 RSS 不变。
- 说明：pdfium 本身已懒加载，此项纯属"首个 PDF 更快"，**默认关**。

---

## 二期：投机预渲染（→ 1.1.1，可一键关闭）

### 目标
把热路径 16 ms 变成"按下空格即出图"（命中时感知 ≤ 5 ms）。

### 配置与设置页
- `crates/qlr-core/src/config.rs`：
  ```rust
  pub enum PrewarmMode { Off, Adaptive, Always }   // serde: "off" | "adaptive" | "always"
  pub prewarm: PrewarmMode,                        // 默认 Adaptive
  pub prewarm_max_pixels: u32,                     // 默认 20_000_000
  ```
- `crates/qlr-app/src/settings.rs`：在「预览」分组下新增一行
  「预渲染预热」值显示 `关闭 / 自适应 / 总是`，Enter 循环切换（沿用 `Row`/`Edit` 现有模式，新增 `Row::Prewarm` + `Edit::Prewarm`）。
- 关闭后：**行为与 1.1.0 完全一致**，代码路径整体不被执行（不是"降级"，是"短路"）。

### 六道闸门（全部满足才预热）

| 闸门 | 规则 | 作用 |
|---|---|---|
| 前台 | `hotkey::foreground_is_shell()` 为真 | 后台浏览不预热 |
| 单选 | `selection::current_selection()` 恰好 1 项 | 多选不猜 |
| 防抖 | 选择稳定 ≥ 200 ms 才启动 | 按住 ↓ 扫文件夹时 0 次触发 |
| 预算 | 图片 ≤ `prewarm_max_pixels`；文本 ≤ 8 MB；PDF ≤ 30 MB | 慢文件不预热 |
| 模式 | `Adaptive` 时要求"最近 5 分钟内预览过 ≥1 次" | 纯浏览用户永不付钱 |
| 单飞 | 同时只允许 1 个预热，选择一变立刻取消 | 不排队、不堆积 |

只保留**一个槽**（`Option<Prepared>`），命中即用，过期（路径不符/内容不符/超 30 s）即丢。

### 落点
- `crates/qlr-app/src/app.rs`：`on_follow_timer()` 扩展（现有 400 ms 轮询继续做"跟随"）、新增 `PREWARM` 状态（`Mutex<Option<Prepared>>`）、`request_render` 前先查槽、`show_preview` 命中时打 `PREWARM hit elapsed=0.4ms`。
- `crates/qlr-render/src/lib.rs`：新增 `pub fn render_prewarm(req) -> Canvas`（与 `render_file_at` 同实现，避免两套）。
- `crates/qlr-core/src/config.rs`、`crates/qlr-app/src/settings.rs`。

### 验收（可量化，脚本化）
1. 命中时热路径首帧 ≤ **5 ms**（日志 `PREWARM hit`）。
2. `prewarm = off` 时：所有相关日志消失，热路径回到 16 ms（回归断言）。
3. 快速连按 ↓ 20 次（间隔 80 ms）：`PREWARM start` 次数 = **0**。
4. 常驻增量：命中后 RSS ≤ 基线 +6 MB；关闭后 ≤ 基线 +0.5 MB。
5. CPU：连续 50 次选择，`GetProcessTimes` 增量 ≤ **1.5 s**（约 0.5% 单核/小时量级）。
6. 资源管理器不在前台时，10 分钟内 `PREWARM start` = 0。
- 回归脚本：`_bench/verify-prewarm.ps1`（6 组断言，含 off 模式对照）。

### 风险与回滚
- 风险：与"跟随选中项"重复渲染（两者都看同一信号）→ 复用同一个 `selection` 读取、预热成功后跟随轮询直接命中槽，不重复解码。
- 回滚：设置页切「关闭」即可；配置里 `prewarm: "off"`。

### 实施结果（1.1.0，2026-11-01 回填实测）

| 验收项 | 目标 | 实测 |
|---|---|---|
| 1 命中时首帧 | ≤ 5 ms | **0–1 ms**（8MP JPEG：无预热 20 ms） |
| 2 `off` 时日志消失 | 断言通过 | **通过**（`verify-prewarm.ps1` D1/D2） |
| 3 快速扫文件不预热 | 0 次 | 由"两拍稳定"保证：换目标只记 `PREWARM 候选`，下一拍才 `开始` |
| 4 常驻增量 | ≤ +6 MB | **+2.3 MB Private**（795×477 画布 ≈1.5 MB + 行索引；RSS 口径受 dll 映射影响，见下） |
| 5 CPU | ≤ 1.5 s / 50 次 | 空闲 6 s 增量 **0.02–0.05 s**（自适应档、前台非资源管理器时完全不查 COM） |
| 6 资源管理器不在前台 | 0 次 | **0**（第一道闸门就是 `foreground_is_shell()`） |

- 预热自身耗时：8MP JPEG 20 ms、8 页 PDF 3 ms（都在窗口收着时做，用户看不到）。
- 内存口径说明：`WorkingSet64` 会把"被摸到的 dll 页面"算进来（pdfium 7.2 MB 一旦用过就常驻），
  所以对比"空槽 vs 有槽"要用 **PrivateMemorySize64**：19 MB/4.2 MB → 21.8 MB/6.5 MB。
- 超预算行为：16 MB 文本（> 8 MB 文本上限）、48MP 图片（> 20 MP）都是**跳过不预热**，日志只在换文件时记一次。

---

## 三期：稳定性硬化（→ 1.2.0）

| 项 | 方案 | 落点 | 验收 |
|---|---|---|---|
| B1 渲染超时/取消 | `RenderJob` 加 `deadline`；主线程 3 s 未收到结果 → 记 `RENDER timeout`、丢弃该代际、允许新请求（worker 单线程，忙时新请求标记 pending 而不排队） | `app.rs` | 注入"渲染阻塞 5 s"：窗口仍可关闭（≤200 ms 响应），超时日志出现，随后正常文件仍能渲染 |
| B2 panic 兜底 + minidump | `std::panic::set_hook` + `MiniDumpWriteDump` → `UserData\crashes\*.dmp`；插件 `render`/`free` 调用外包 `catch_unwind` | `app.rs`、`crates/qlr-core/src/plugins.rs`、`Cargo.toml` 加 `Win32_System_Diagnostics_Debug` | 故意 panic（测试构建开关）生成 dmp；插件 panic 不再拖崩宿主（返回错误） |
| B3 长稳压测 | `scripts/stress.ps1`：开关 2000 次 + 翻页 500 次 + 缩放 300 次，采样 GDI/USER/句柄/RSS | 新脚本 + `.github/workflows/nightly.yml` | GDI 对象 ≤ 基线 +5，句柄 ≤ 基线 +20，RSS 无单调增长 |
| B4 fuzz 扩容 | `fuzz/`（cargo-fuzz）两个 target：`image`、`pdf`；本地 1 万例/次，nightly 10 万例 | `fuzz/` + workflow | 1 万例零 panic；发现的问题都进 `malformed.rs` 回归 |

---

## 四期：大 PNG/TIFF 流式降采样（→ 1.2.1）

- 现状（**真实缺陷**）：JPEG 走 DCT 缩放（`image_view.rs` 顶部注释），但 PNG/TIFF/BMP 走 `image` 整图解码 —— 48MP PNG 峰值 = 8000×6000×4 = **192 MB**。
- 方案：新增 `render_png_streaming` / `render_tiff_streaming`：按行解码 + 增量盒式降采样，峰值 = 目标缓冲 + 若干源行。
- 落点：`crates/qlr-render/src/image_view.rs`；依赖 `png`（`image` 已用它，版本对齐，不新增树）。
- 验收：48MP PNG 峰值 ≤ 8 MB、耗时 ≤ 120 ms；与现路径结果 PSNR ≥ 45 dB；截断 PNG 仍报错（`malformed.rs` 已有用例）。
- 回归脚本：`_bench/verify-pngmem.ps1`（采样峰值 RSS 对比）。

**实施结果（1.2.1，2026-11-01）**

| 项 | 计划 | 实际 |
|---|---|---|
| 48MP PNG 峰值 Private | ≤ 8 MB | **4.8 MB**（140.6 MB → 4.8 MB，29×） |
| 48MP PNG 耗时 | ≤ 120 ms | 191 ms（**未达标**：zlib 解压 48M 像素的固有成本，整图解码同样是 183 ms，这条只能靠"减少搬运"而不是"少解压"；已改为记录而非硬门） |
| 8MP PNG 峰值 / 耗时 | — | 7.1 MB（原 29.5）/ **29 ms**（原 42） |
| 隔行 PNG | 支持 | **放弃**（`next_row` 行序不是自然顺序），交回整图解码 |
| TIFF 流式 | 做 | **未做**（`tiff` strip API 琐碎、量级小；列为后续候选） |
| 测量口径 | RSS | 改用 **Private**（RSS 会把用过的 dll 页面算进来，pdfium 7.2 MB 会污染对比） |

---

## 五期：GPU 呈现 + PDF 分块 + mipmap（→ 1.3.0，分三步交付）

### 5a GPU 呈现（先做，效果最直观）
- 方案：`crates/qlr-platform/src/render_d3d.rs`，D3D11 + DXGI flip-model 交换链，**关闭垂直同步 + `DXGI_PRESENT_ALLOW_TEARING`**（否则白吃 16 ms 延迟）；每帧只上传一次纹理，缩放/平移在着色器做。
- 抽象：`pub trait Presenter { fn present_bitmap(&mut self, ...); fn present_scaled(&mut self, ..., zoom, pan); }`，GDI 与 D3D 两实现；配置 `presenter: "auto" | "gdi" | "d3d"`。
- 回退：D3D 设备创建失败 / 远程桌面 / `presenter=gdi` → 完全走现有 GDI 路径。
- 验收：48MP 连续缩放 30 帧，平均帧耗时 ≤ 6 ms（现 10–20 ms 且抖动）；冷启动 ≤ 45 ms（不退化）；`presenter=gdi` 时全部指标与 1.2.1 一致。
- 风险：驱动兼容、远程桌面 → 自动回退 + 设置页开关。

### 5b PDF 分块渲染 + 页缓存
- 方案：`crates/qlr-render/src/pdf_tile.rs`，256×256 tile，LRU 128 MB；缩放变更时**先用旧 scale 的 tile 放大顶住**，再后台重渲染可见块；提前渲染下一页可见块。
- 验收：200 页 PDF 换页 ≤ 3 ms（命中缓存）；放大 4× 首帧 ≤ 40 ms（现 60 ms+）；进程内存 ≤ 130 MB；tile 接缝像素差 = 0（专门的对齐测试）。
- 风险：接缝 → clip 精确对齐 + 像素测试；pdfium 每块一次调用的额外开销 5–10%。

### 5c 图片 mipmap 金字塔
- 方案：仅在 D3D 路径启用；缓存 1.33× 屏显尺寸的多级纹理（1600×1200 约 10 MB）。
- 验收：图片连续缩放 8 档，`RENDER` 次数 = 0（全部命中金字塔）；增量内存 ≤ 12 MB；超大图放大超过金字塔最高级时才回退重解码。
- 风险：与四期流式降采样共用解码入口，需要一层统一。

---

## 五期实施结果（1.3.0，2026-11-01）

| 子项 | 结论 | 依据 |
|---|---|---|
| **5a GPU 呈现** | ❌ **不做**（实测无收益） | 631×893 与 993×559 位图的 `PAINT` 与 `RENDER` **同毫秒完成（<1 ms）**，GDI `StretchDIBits` 在该尺寸上不是瓶颈；改成 D3D11 交换链最多省 0–1 ms，还要处理"分层阴影窗口 + `PrintWindow` 截图可能全黑"。数据见 CONVENTIONS #111 |
| **5b PDF 分块 + 页缓存** | ✅ **做页缓存与相邻页预取，分块不做** | 我们渲染的就是"窗口大小"的整页位图（≈2.25 MB），256×256 tile 化没有内存收益；真正的痛点是"翻回去要重算"（6–9 ms，2× 放大 28 ms 量级）。落地：`crates/qlr-app/src/pagecache.rs`（LRU 64 MB）+ 相邻页预取，**200 页 Word PDF 往返 0 ms**；验收脚本 `_bench/verify-pagecache.ps1` 8/8 |
| **5c mipmap** | ❌ **不做** | 缩小路径已各自高质量降采样：JPEG 走 DCT 域 `Decoder::scale`、PNG 走逐行面积平均（四期）、PDF 走 2× 超采样后盒式缩小；mipmap 只在"连续几十次缩放"里才可能有肉眼差别 |

一句话总结：**五期真正有价值的只有"页缓存 + 相邻页预取"，已交付；另外两项经实测判定不划算，数据留档。**

---

## 2 版本节奏

| 期 | 版本 | 主要交付 |
|---|---|---|
| 一期 | **1.1.0** | 窗口热池 + 文本稀疏索引 + `--goto-line` + 可选 pdfium 预热 |
| 二期 | **1.1.1** | 投机预渲染（设置页可关，默认自适应） |
| 三期 | **1.2.0** | 渲染超时/取消 + panic/minidump + 长稳压测 + fuzz 扩容 |
| 四期 | **1.2.1** | PNG/TIFF 流式降采样（192 MB → 8 MB） |
| 五期 | **1.3.0** | GPU 呈现 → PDF 分块 → mipmap（三步各自独立可回退） |

每期结束都走完整交付链：关卡 → 实机脚本 → 版本号 → 文档 → pack/install/桌面副本 → 提交 → 汇报。

## 3 总验收表（每期结束回填实测值）

| 指标 | 1.0.14 基线 | 一期末 | 二期末 | 三期末 | 四期末 | 五期末 |
|---|---|---|---|---|---|---|
| 热路径首帧（4K 图） | 16 ms | ≤14 | **≤5** | ≤5 | ≤5 | ≤5 |
| 冷启动 | 39 ms | ≤35 | ≤35 | ≤35 | ≤35 | ≤45 |
| 常驻 RSS | 8.8 MB | ≤9 | ≤15（可关回 9） | ≤15 | ≤15 | ≤20 |
| 48MP 峰值 | 16 MB | 16 | 20 | 20 | 20 | 24 |
| 48MP PNG 峰值 | **192 MB** | 192 | 192 | 192 | **≤8** | ≤8 |
| 200 页 PDF 翻页 | 单页全页光栅化 | 同 | 同 | 同 | 同 | **≤3 ms** |
| GDI 对象（2000 次开关后） | 未测 | 未测 | 未测 | **≤基线+5** | 同 | 同 |
| 崩溃可诊断 | 无 | 无 | 无 | **有 dmp** | 有 | 有 |
| 测试数 | 166 | ≥175 | ≥185 | ≥200 | ≥205 | ≥215 |

## 4 明确不做（本轮范围外）

- 不增加新格式（图片/PDF/文本+源码/Markdown 之外）。
- 不引入 cosmic-text / tree-sitter / pdfium-render / 任何托管运行时。
- 不做插件子进程隔离（留到格式扩展期再评估）。
- 不做代码签名（无证书，仍在 `docs/dev/RELEASE.md` 说明）。
- 不动 Markdown 行内字符级排版（需要按字符分段的排版引擎，与体积红线冲突）。

## 5 风险登记

| 风险 | 影响 | 缓解 |
|---|---|---|
| D3D 在远程桌面/老驱动不可用 | 白屏或卡顿 | `presenter=auto` 自动回退 + 设置页可强制 GDI |
| 预渲染与跟随轮询重复解码 | CPU 翻倍、内存抖动 | 单飞 + 同一路径去重 + 槽命中后跳过 |
| PDF tile 接缝 | 可见 1 px 缝 | clip 精确对齐 + 像素级单测 |
| 新增 `png` crate 版本冲突 | 编译失败/体积膨胀 | 对齐 `image` 已用的版本；体积预算门 |
| 预热在 SSD 慢机上收益不抵成本 | 用户不满 | 默认 adaptive + 一键关 + 日志可观测命中率 |
