# docx 原生预览——计划与可行性（评审稿）

> 状态：**评审稿，未开工**。目标是与 1.9.x 表格（xlsx）优化同样的形态：纯 Rust、进程内、
> 不依赖 Office，**性能与内存双降**。本文只做计划与可行性，等拍板后再落代码。
> 参考：[ADR-0011 原生表格预览](../adr/0011-native-sheet-preview.md)、
> [SHEET-1.9.x 收口 §6](../dev/SHEET-1.9.x.md)、LOG 2026-11-10 探针条目。

---

## 0. 一句话结论

docx 原生预览**可行且便宜**：前期探针实测「真实中文合同 解析+排版+绘制 **15.1 ms / 13.7 MB**、
103 页文档 **45 ms**、可读性 100%、"像 Word" 90%」。做法与表格完全同构——
`qlr-core` 放数据模型、`qlr-render` 解析并排版、`qlr-platform::window` 只画**当前页**、
`qlr-app` 只做壳层编排。目标是在**不装 Office**、内存只涨十几 MB 的前提下，把 docx 冷启动从
**718 ms 降到 20 ms 量级**、常驻从 **Office 的 130–300 MB 降到宿主的 +十几 MB**。

---

## 1. 背景与目标

### 1.1 现状（docx 走系统 `IPreviewHandler`）

| 维度 | 现状（ShellBackend / 系统处理器） | 来源 |
|---|---|---|
| 依赖 | **必须装 MS Office / Office 查看器**，否则不出内容 | HANDOFF 1.9.4 |
| 冷启动 | 真实合同 `real-contract.docx` **718 ms** | ADR-0011 §背景实测 |
| 热启动 | 同文件二次 **82 ms**（靠 Word 常驻） | ADR-0011 |
| 常驻内存 | Office 进程 **130–300 MB** | ADR-0011 |
| 输入/交互 | 子窗口自己收输入，改大小要 `SetRect` 重新对齐（一堆 `#145/#147` 坑） | AGENTS §5.2 |

对比：同仓 xlsx 走原生后 **解析 17.6 ms、首帧 5+18 ms、宿主 +6 MB / 表**，且不装 Office
（ADR-0011）。差距就是这次要补齐的。

### 1.2 目标（对齐表格的验收口径）

1. **性能**：解析+排版一次进入 **20 ms 量级**；翻页/重绘**只画当前页**、每帧 **10 ms 量级**。
2. **内存**：不驻留整篇位图；宿主相对空闲只涨 **十几 MB 量级**（探针 13.7 MB）；无 Office 进程。
3. **零重依赖**：只用仓库已有的 `zip`（表格已用）与 `image`（图片/插件已用），**不引** PDFium、
   LibreOffice、office2pdf（后者 71.9 MB / 2 万行 7.2 GB，已在 ADR-0011 否决）。
4. **可回退**：解析失败不 panic、不空窗——退系统处理器 / 插件大纲卡片（与表格同一套兜底）。

---

## 2. 可行性证据（探针已做，不重复造轮子）

### 2.1 实测数字（`_bench/office2pdf/spike/`，LOG 2026-11-10）

| 夹具 | 解析+排版+绘制 | 内存 | 观感 |
|---|---|---|---|
| 真实中文合同 `real-contract.docx` | **15.1 ms** | **13.7 MB** | 可读性 100%、"像 Word" 90% |
| 103 页文档 | **45 ms** | — | 分页正确 |
| （对照）系统处理器冷启动 | 718 ms | Office 130–300 MB | — |

### 2.2 探针暴露的缺口（决定范围分级，见 §4）

页眉页脚、分页、浮动图、编号语义（多级列表）、中西文间距——这几项是"像不像 Word"的差距来源，
但都**不影响"打开就能看懂"**，可分级补。

### 2.3 仓库里现成可复用的轮子（这是"便宜"的关键）

| 能力 | 现成实现 | 复用方式 |
|---|---|---|
| zip 解容器 | `qlr-render/src/sheet.rs` 已用 `zip` crate | 直接复用，同样"单条目/总解压"上限防护 |
| 手写 OOXML 解析（**不引 XML 库**） | `qlr-render/src/sheet.rs` 的标记扫描套路 | 同风格写 `docx.rs` |
| 文本折行（注入 `measure`） | `qlr-render/src/wrap.rs`：纯逻辑 + 注入测量函数 | 段落折行直接复用它 |
| 真实字体度量 | `qlr-platform::window::measure_text`（GDI） | 排版时注入给渲染层 |
| 字体缓存（14 档，禁 `CreateFontW` 热路径） | `qlr-platform/src/window/text.rs` 的 `FONTS` | 段内 run 落到最接近档位 |
| 逐格/可见区 GDI 绘制、滚动、标签栏 | `qlr-platform/src/window/sheet.rs` | 表格/分页绘制照抄分层与裁剪思路 |
| 页码框 + 翻页 | PDF 通道（`s.page`/`s.pages`/`PageUp/Down`） | docx 分页复用同一套壳层机制 |
| "视图内覆盖不写回文件" | `sheet::SheetSizes` | 未来 docx 缩放/页宽调整沿用同一思想 |

---

## 3. 架构设计

### 3.1 为什么必须是"数据模型 + 绘制层分离"（不能像图片那样出一个大位图）

`qlr-render` **不依赖 `windows`、没有字体光栅化能力**——它只会产出 `Canvas::Text`（语义行）
或 `Canvas::Bitmap`（解码完的位图）。docx 要画的是**带精确位置的多字号文本 + 表格 + 内嵌图**，
只有 `qlr-platform` 的 GDI 能把字变成像素。同时"整篇渲染成一张大位图"会直接爆内存
（103 页 × 整页像素 = 数百 MB）。所以结构只能是：

```
解析(纯) + 排版(纯, 注入measure)  →  页模型(DocDoc)   →  按当前页 GDI 绘制(platform)
```

这与表格 `SheetDoc → paint_sheet`（只画可见格）**完全同构**，也是表格拿到 +6MB/表 的原因。

### 3.2 分层落点（严格遵守 AGENTS §3）

| 层 | 新增 | 职责 | 硬约束 |
|---|---|---|---|
| `qlr-core/src/docx.rs`（新） | `DocDoc` 页模型 + 几何 | 段落/行/run/表格/图片的**纯数据**；页内坐标计算 | **零 `windows`**，可 Linux 单测 |
| `qlr-render/src/docx.rs`（新） | `parse_docx()` + `layout_docx()` | zip→OOXML→模型；折行/分页（`measure` 注入） | 不碰窗口；失败返回 `Error`，不 panic |
| `qlr-platform/src/window/docx.rs`（新） | `paint_docx_page()` | 白页/页边距/段落/run/表格/内嵌图/页眉页脚 | 只在 platform；禁热路径 `CreateFontW` |
| `qlr-app`（多点小改） | `Kind::Document`、`Canvas::Document`、通道/翻页/开窗/兜底 | 编排 | `app.rs` 只许 `edit` 精确改 |

### 3.3 与表格方案的逐项对照（证明"和 Excel 一样"）

| 表格（已落地） | docx（本计划） |
|---|---|
| `Kind::Sheet` | `Kind::Document` |
| `Canvas::Sheet(Box<SheetDoc>)`（只带数据） | `Canvas::Document(Box<DocDoc>)`（只带数据） |
| `qlr-core::sheet::SheetDoc` | `qlr-core::docx::DocDoc` |
| `qlr-render::sheet::parse_sheet*` | `qlr-render::docx::parse_docx*` |
| `qlr-platform::window::sheet::paint_sheet`（只画可见格） | `qlr-platform::window::docx::paint_docx_page`（只画当前页） |
| 二维像素滚动 `s.sheet_scroll` | 页内滚动 / `PageUp·Down` 翻页（复用 PDF 的 `s.page`） |
| 顶栏页码框切 sheet（`sheet_layout` 只读骨架） | 顶栏页码框翻页（页数在首次排版后拿到，见 §3.6） |
| `SheetSizes` 视图内覆盖不写回 | 缩放/页宽覆盖不写回 |
| 解析失败 → 系统处理器/插件兜底 | 同左（`.doc` 老格式同样落兜底） |

### 3.4 数据模型草案（`qlr-core/src/docx.rs`，只列骨架）

```rust
pub struct DocDoc { pub pages: Vec<Page>, pub page_w: u32, pub page_h: u32, pub title: Option<String> }
pub struct Page { pub blocks: Vec<Block>, pub header: Option<Vec<Line>>, pub footer: Option<Vec<Line>> }
pub enum Block {
    Para(Para),
    Table(Table),                 // 行×列，单元格内含 Vec<Line>
    Image { rect: Rect, bytes_id: u32 }, // 内嵌图：按 rels 取 media 字节，绘制期解码
    Rule,                          // 分隔线 / 段落边框
}
pub struct Para { pub rect: Rect, pub align: HAlign, pub lines: Vec<Line> }
pub struct Line { pub rect: Rect, pub runs: Vec<Run> }
pub struct Run {
    pub text: String,
    pub bold: bool, pub italic: bool, pub underline: bool, pub strike: bool,
    pub size_dpt: u16,          // 0.1pt，和 sheet::CellStyle 同口径
    pub color: u32,             // 0xRRGGBB
    pub highlight: Option<u32>,
    pub font_family: u8,        // 落到 14 档字体缓存的下标（不存字体名）
}
```

几何（页内 x/y、行高）在 `qlr-core` 里算，绘制端只照坐标画——和表格"量位置=画位置"
（CONVENTIONS #38 同类）保持一致。

### 3.5 排版在哪做、`measure` 怎么注入

折行需要真实字宽，而 `qlr-render` 不能碰 GDI。解法与 `wrap.rs` 一致：**排版函数接收一个
`measure: &dyn Fn(&str, size_dpt, bold) -> u32`**，单测里注入"定宽实现"，生产里由
`qlr-platform::window::measure_text` 提供。于是：

- `qlr-render::docx::layout_docx(bytes, content_w, measure) -> DocDoc` 是纯函数，可跨平台测；
- 分页（累计行高超过页高就断页）也在这一层，一次性排版出全部页；
- 已知折行结果依赖 GDI 度量，故 **同一份 `measure` 必须贯穿排版与绘制**，避免"量的时候宽度
  和画的时候不一致导致文字压线"。

### 3.6 页数怎么来（开窗与翻页的时序）

PDF 能从 pdfium 廉价拿到页数；docx 的页数要排版后才知道。策略（与表格"按需"一致）：

1. **开窗尺寸**：只需解析 `document.xml` 的 `w:sectPr/w:pgSz`（twips），`natural_size` 换算成像素
   给窗口锁**页面比例**（A4/Letter 自动对角），代价 ~1 ms，类似 `sheet_layout` 只读骨架。
2. **总页数**：首次 `Canvas::Document` 抵达时用 `doc.pages.len()` 回填 `s.pages`（`set_content`
   里已有"按实际 Canvas 校准"的钩子，加一条分支即可）。首屏前先显示"第 1 页"，拿到后排页。

### 3.7 注册新 Kind 的 6 步（AGENTS §4 清单逐条落地）

1. `qlr-core/src/format.rs`：`Kind::Document` + `name/label/hint/extensions/kind_for_path`
   全同步 + 单测；`DOC_EXTS = ["docx","docm","dotx","dotm"]`（`.doc`/`.dot` 老 BIFF **不进**，
   落兜底，和 `.xls` 一个处理）。
2. `qlr-render`：`natural_size()` 加 `Kind::Document` → 读页尺寸；`has_natural_size` 置真。
3. `qlr-app`：`Canvas::Document` 分支、`Surface::Document`、`repaint()` 通道、
   `width_for/window_size_for`、`on_wheel`/翻页键。
4. `shell_candidate()`：docx 命中原生后**不再画占位页**（#149 同类）。
5. `scripts/verify-docx.ps1`：新增可自动断言的检查（见 §8）。
6. `README.md` 支持表 + `docs/dev/STATUS.md` 格式矩阵。

### 3.8 `Canvas::Document` 的 fan-out 成本（真实工作量，先摊开）

`Canvas` 是全 match 的（表格新增 `Sheet` 时扫过一圈）。加 `Document` 要同步改：

- `qlr-render/src/canvas.rs`：`ink_pixels/dimensions/kind_name/approx_bytes`；
- `qlr-app/src/app/backend.rs`：`Surface`（加 `Document`）、`from_canvas`、`NativeBackend::surface`；
- `qlr-app/src/app/render.rs`：`canvas_bytes/canvas_size/frame_matches_size/fits_render_box/present_canvas`；
- `qlr-app/src/app/chrome.rs`：`scroll_info`、`repaint` 的 `Canvas::Document` 绘制分支；
- `qlr-app/src/app/input.rs`：滚轮/翻页/（可选）点选文本。
预计半天，且编译器会**穷举报错**兜住漏改——比"静默走错分支"安全。

---

## 4. 范围分级（先给可验收的最小面，再按冻结节奏补）

### P0 —— 覆盖 80% 日常规整文档（本计划的主交付，建议先做）

- 段落 + run：粗体/斜体/下划线/删除线、字号、字色、高亮；
- 对齐（左/中/右/两端）、段前段后间距、行距、首行缩进/悬挂缩进；
- `styles.xml` 的 `docDefaults` + 命名样式继承（标题靠样式字号落地）；
- 页面尺寸/页边距（`sectPr`）、分页；
- 表格：网格 + 单元格文字 + 边框（基本）；
- 内嵌图片（`word/media/*`，按 `image` crate 解码）；
- 翻页（`PageUp/Down` + 顶栏页码框）、页内滚动、窗口锁页面比例；
- 解析失败/`.doc` → 系统处理器/插件兜底，不 panic、不空窗。

### P1 —— "更像 Word"（P0 稳定后再排）

页眉页脚、多级编号语义（`numbering.xml`）、中西文自动间距、超链接文字着色、分节/多节、
脚注尾注、表格单元格合并、锚定浮动图（按锚点近似内联）。

### P2 —— 明确**不做**或长期

域代码（页码/TOC 自动生成）、文本框、图表、SmartArt、批注/修订痕迹、多栏、竖排文字、
OMML 公式、图片环绕方式全语义、精确制表位。

> 依据：探针显示 P0 就能到"可读性 100%、像 Word 90%"；P1 补到"日常办公够用"；
> P2 投入产出比差（与表格"公式引擎/图表不建议做"同判断）。

---

## 5. 性能与内存目标 + 测量方法

| 指标 | 现状（系统处理器） | 目标（原生） | 怎么测 |
|---|---|---|---|
| 冷启动（真实合同） | 718 ms | **≤ 25 ms** | `scripts/bench` 记 parse+layout 耗时；`app.log` 时间戳 |
| 翻页/重绘一帧 | — | **≤ 15 ms** | `PrintWindow` 前后计时 / GDI 计数 |
| 103 页文档首次排版 | — | **≤ 60 ms** | 同探针口径 |
| 宿主常驻（相对空闲） | Office +130–300 MB | **+10–20 MB** | `Get-Process` Private/WS，看完 docx 后采样 |
| 依赖 Office | 是 | **否** | 干净机（无 Office）跑通预览 |
| GDI/USER 句柄 | 子窗口一套 | 不随翻页增长 | `GetGuiResources` 前后比 |

验收标准：冷 <25 ms、翻页 <15 ms、看完一个大 docx 再 `--hide` 后常驻回落到 +20 MB 以内、
无 Office 机器能预览（与表格同一套 PERF 脚本口径）。

---

## 6. 工作拆解与工期估算（人天）

| 模块 | 内容 | 估算 |
|---|---|---|
| `qlr-core::docx` | 模型 + 几何 + 单测（不碰系统） | 1.5 |
| `qlr-render::docx` 解析 | document.xml（段落/run/表格/图 rels）、styles.xml、numbering.xml、sectPr、硬上限防护 | 4.0 |
| 排版引擎 | 折行（复用 `wrap`+注入 measure）、分页、表格列宽、间距/缩进 | 2.5 |
| `qlr-platform::window::docx` 绘制 | 段落/run 落字体档位、表格网格、内嵌图、页眉页脚、（可选）页边框 | 3.0 |
| `qlr-app` 接线 | Kind/Canvas/Surface/通道/翻页/开窗/兜底/state | 1.5 |
| 验证与文档 | `verify-docx.ps1` + 夹具 + LOG/STATUS/README | 1.5 |
| **P0 合计** | | **≈ 14 人天**（区间 11–17，与探针估算一致） |
| P1 追加 | 页眉页脚/编号语义/间距/合并等 | +6–9 |
| P2 | 域/图/环绕等 | 长期，不在本轮承诺 |

---

## 7. 风险与回退

| 风险 | 影响 | 缓解 |
|---|---|---|
| 折行/GDI 度量不一致 → 文字压线/越界 | 观感 | 排版与绘制用**同一 `measure`**；单测注入定宽；截图回归 |
| 进程内解析 + `panic=abort` | 坏文件带走预览进程 | 沿用表格硬上限（单条目 64 MB / 总解压 192 MB / 文本截断）+ `broken.docx`/`fake.docx` fuzz（夹具已在 `_bench`） |
| 字体只有 14 档、无字族 | 字号/字族不完全精确 | 已知取舍，先落最接近档位；P1 再评是否给 `text.rs` 加动态字体缓存 |
| `Canvas::Document` fan-out 漏改 | 走错通道 | 编译器穷举 + `verify-docx` 断言 + `verify-layers` 静态扫描 |
| 页数要排版后才知道 | 首屏页码暂缺 | §3.6：开窗用页尺寸、页数在首次 Canvas 回填 |
| 长文档（数百页）排版耗时 | 首帧慢 | 实测 103 页 45 ms 可接受；超长文档设页数上限 + `log_warn` 截断 |

**回退**：解析失败或超上限 → 保留原 `ShellBackend`（装 Office 时出真排版）+ `qlr-plugin-office`
大纲卡片，与表格"原生优先、兜底系统处理器/插件"完全一致。

---

## 8. 验证计划

- **单测（无窗口，`cargo test`）**：`qlr-core::docx` 几何/分页；`qlr-render::docx` 解析（注入定宽
  measure 的折行、表格、内嵌图 rels）；坏文件不 panic（沿用 `tests/malformed.rs` 对 `doc.docx` 的用例扩展）。
- **`scripts/verify-docx.ps1`（端到端真进程）**：打开夹具 docx → 断言 `TITLE` 已设、`DOC 原生接管`
  日志、页数/页码正确、`PageUp/Down` 换页、拖边改大小后内容高度=客户区−`CHROME_HEIGHT`、
  截图亮像素占比 > 阈值（`PrintWindow`，禁 `CopyFromScreen`）。
- **夹具**：直接复用 `_bench/office2pdf/fixtures/`：`cn-report.docx`、`real-contract.docx`、
  `big-report.docx`、`broken.docx`、`fake.docx`（通过 `QLRUST_FIXTURES` 指向）。
- **回归基线**：开工前先跑绿（当前 `cargo test` **363 passed / 0 failed / 13 ignored**、
  `verify-sheet` 13 PASS、`verify-office` 23 PASS、`verify-layers` 14 PASS、smoke 16 PASS/1 SKIP）。
- **文档**：LOG 版本条目（含实测数字）、STATUS 矩阵、README 支持表、CONVENTIONS 新坑、HANDOFF 接力说明。

---

## 9. 需要你拍板的点

1. **目标定级**：先只做 **P0**（≈14 人天、覆盖 80%）上线冻结，还是一次做到 P1？—— 建议先 P0，
   按表格"先能用再冻结"的节奏。
2. **翻页 vs 连续滚动**：先做「一页一屏 + `PageUp/Down`/页码框翻页」（复用 PDF 机制、最省），
   还是直接做「连续滚动 + 页间分隔线」（更像 Word、但排版与壳层更重）？—— 建议先翻页式。
3. **与系统处理器并存策略**：装 Office 的机器上，**原生优先**（与 Excel 一致）还是系统处理器优先？
   —— 建议原生优先，解析失败再兜底处理器。
4. **字体保真度**：沿用 14 档字体缓存（快、省内存、字号/字族近似）还是给 `text.rs` 加动态字体缓存
   按 pt 精确（更像 Word、但 GDI 句柄与内存上升）？—— 建议先沿用档位，P1 再评估。
5. **是否本轮连 pptx 一起做**？—— 建议按既定顺序先 docx，pptx 另开窗口。

---

## 10. 建议的执行顺序（拍板后）

1. 跑绿基线（§8）。
2. `qlr-core::docx` 模型 + 单测（纯逻辑，风险最低）。
3. `qlr-render::docx` 解析 + 排版 + 无窗口单测（大头）。
4. `qlr-app` 接线 + `qlr-platform::window::docx` 最小绘制 → 能出第一页。
5. `verify-docx.ps1` + 夹具 → 端到端 PASS。
6. 补 P1 缺口，冻结，写文档。
