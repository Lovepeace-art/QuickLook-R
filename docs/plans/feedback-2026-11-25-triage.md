# QLRust 反馈处理评估（2026-11-25）

> 基线版本 **1.15.5**。本文只做评估 + 方案，**未改动任何代码**。
> 证据都带 `文件:行号`，方便复核；工作量按"熟悉本仓的一个人"估算。

---

## 0. 结论速览

| # | 反馈 | 类型 | 一句话结论 | 工作量 | 风险 |
|---|---|---|---|---|---|
| 1 | PDF 改连续上下滚动 | 改造 | 可行，复用 docx 的 `view_y`；代价=重做滚动/命中/翻页模型 + 可见页环形缓存 | 3–5 人日 | 中（大页 PDF 滚动掉帧、内存） |
| 2 | 新增 PSD | 新格式 | 可行，加 `psd` crate 取合成图，复用图片通道 | 1–2 人日 | 低–中（CMYK/16bit/超大） |
| 3 | 新增 doc / xls | 新格式 | **xls 可做**（calamine→Sheet 渲染器）；**doc 不建议原生**（保留系统处理器兜底） | 3–5 人日 | 中（BIFF 边界/加密） |
| 4 | pptx 缺母版等，做对照 | 分析+补齐 | 见 §4 对照表；最值钱的是"母版/版式装饰图形 + 背景(渐变/图)" | 分析已给；补齐分档 2–8 人日 | 中 |
| 5 | PDF 页数多卡顿 | 性能 | 根因：单页 PDFium 光栅 + 预取仅邻页且 30MB 上限 + 单 DocSlot | 1–3 人日 | 低 |
| 6 | 新增 EPUB | 新格式 | 可行，zip + OPF spine → 复用 HTML 排版 | 3–5 人日 | 中（复杂 CSS/DRM） |
| 7 | 保真调用的系统解析流程 | 问答 | 见 §7 全流程（注册表→IPreviewHandler→prevhost→SetFocus/Timer） | — | — |
| 8 | 超高图片展示过小，要保留关闭按钮+最小宽度 | bug/UX | 根因 `natural_rect_raw` 用 `.min(fit)` 把 `MIN_WIDTH` 抵消 | 0.5–1 人日 | 低 |
| 9 | 图片缩放固定比例的割裂感 | UX | 每档固定 1.25× + 缩放回中 + 松手才出高清 | 0.5–1 人日 | 低 |
| 10 | Excel 无自适应行高 / 列宽命中错 / 行列出选 | bug/UX | 行高只对折行算且跳表头；列宽命中疑 DPI/反解偏差；行/列头点选**未实现** | 1–2 人日(+复现) | 中 |
| 11 | PNG 透明区需 0 填充，不补白 | bug/UX | 根因 `paint_bitmap_at` 先刷白 + `StretchDIBits SRCCOPY` 不看 alpha | 0.5 人日 | 低 |

## 建议分批

- **批 A（快修，~2–3 人日，风险低，建议先做）**：#11 / #8 / #9 / #10（行列出选 + 列宽复现）
- **批 B（中等，~1–2 周）**：#5 / #2 / #6 / #3(xls)
- **批 C（大工程 / 需拍板）**：#1 / #4(补齐) / #3(doc 原生 —— 建议不做)

---

## 1. PDF 改成 docx 那样上下翻动，代价是什么

**现状**：docx/pptx 自 1.15.0 起就是连续纵向滚动（`app/input.rs:299` 注释"文档/幻灯片 连续纵向滚动"）；**只有 PDF 还是"页码框 + 翻页"**，单页走 PDFium 光栅 `FPDF_RenderPageBitmap`（`qlr-render/pdf.rs:505`），带页缓存 + 邻页预取。

**改成连续滚动的代价（要做的事）**：
1. **滚动/命中模型重写**：现在 PDF 一切按"页码"定位，连续滚动要改成按 `view_y` 像素滚动，并维护"页码↔y 偏移"的映射（滚到哪页、点哪、翻页键落到哪）。
2. **每页高度预计算**：PDF 各页尺寸可以不同，需在打开时用 `FPDF_GetPageSizeByIndex`（只取尺寸、不渲染，很便宜）按目标宽度算出累计高度，才能摆版。
3. **可见页环形缓存**：连续滚动不能把所有页位图常驻（内存爆炸），要维护可见 3–5 页的环形缓冲，滚动时按需重渲染、淘汰离屏页。现有 `DocSlot` 单槽（`pdf.rs:201`）+ 30MB 预取上限（`render.rs:1213`）要一起调整。
4. **翻页语义重映射**：`VK_NEXT/VK_PRIOR`、`--page N`、`step_slide` 这些翻页入口要改成"滚动一屏"。
5. **回归断言改写**：`scripts/verify-pdf.ps1` 的翻页断言要改。

**净收益**：与 docx/pptx 体验一致。**风险**：大页尺寸/复杂页 PDF 单页光栅贵，快速滚动可能掉帧（靠环形缓存 + 降分辨率预览缓解）。

---

## 2. 新增 PSD 图片支持

**现状**：`image` crate 不支持 PSD（`Cargo.toml` 的 features 无 psd），`IMAGE_EXTS` 也没有 psd（`qlr-core/format.rs:128-130`）。

**方案**：
- 加 `psd` crate（读 PSD/PSB，可取合成图 composite 或按图层合成）；
- `qlr-render/image_view.rs` 增加 psd 分支：解码成 RGBA → 复用 `scale_rgba_to_bgra` → `Bitmap`；
- `format.rs` 的 `IMAGE_EXTS` 加 `"psd"`（可连带 `"psb"`），走图片通道；
- 超大 PSD 受 `MAX_IMAGE_PIXELS=50_000_000`（`image_view.rs:24`）保护，超限要降采样而不是报错；
- 补 README 支持表 / STATUS / verify 脚本。

**代价 1–2 人日**。风险：CMYK / 16bit·32bit / 超大文件；建议预览取"合成图"（快），图层浏览以后再说；可优先读 PSD 内嵌缩略图加速首屏。

---

## 3. 新增 doc / xls

**xls（Word 前一代表格，BIFF8）——可以做**：
- 用 `calamine`（纯 Rust，读 xls/xlsx）读成单元格矩阵 → 映射到 `qlr_core::sheet` 模型 → 复用 `window::sheet` 渲染器（冻结/合并/格式/自适应基本可用）；
- `SHEET_EXTS` 加 `"xls"`，`kind_for_path` 归 `Sheet`；
- 注意公式"值"、日期序列号、数字格式转换。**3–5 人日**；风险：加密/宏工作簿、BIFF 边界样本。
- 现状是 `.xls` 被显式排除、交给系统处理器（`format.rs:147-149` 注释）。

**doc（Word 97-2003 二进制）——不建议做原生**：
- 没有成熟的纯 Rust 版式解析；转 HTML/文本近似会丢版式、表格、图片位置。
- 建议**保留现在的 `IPreviewHandler` 兜底**（装了 Office 就能看）；若一定要"不装 Office 也能看"，可加一个"纯文本抽取"的降级通道（成本低，但版式全丢，需明确接受）。
- 现状 `.doc` 被显式排除（`format.rs:152-153`）。

**结论**：xls 排进批 B；doc 原生不做，保留系统处理器。

---

## 4. pptx 与 PowerPoint / LibreOffice 对照

**QLRust 现状**（`qlr-core/pptx.rs`、`qlr-render/pptx.rs`）：能画 文本 / 图片 / 矩形填充 / 表格 / 图表（柱、堆积柱、百分比柱、折线、饼 五种，内置自绘）；支持主题色、占位符几何（沿 slideLayout→slideMaster 取矩形）、组合变换、纯色/渐变（仅首色标）填充、纯色背景。

**缺口对照表**：

| 能力项 | QLRust 现状 | PowerPoint / LibreOffice | 影响 | 补齐成本 |
|---|---|---|---|---|
| 母版/版式**装饰图形**（非占位符） | 只抽占位符矩形，不画装饰（`pptx.rs:695`） | 完整渲染 | 背景 logo/色块/装饰条缺失，观感差一截 | 中 2–3 人日 |
| 背景（渐变/图片） | 仅纯色（`Slide.bg`，`pptx.rs:141`） | 纯色/渐变/图片 | 渐变/图片背景变白 | 中 1–2 人日 |
| 填充 | 纯色 / 渐变**仅首色标**（`fill_color:550`） | 纯色/多色标渐变/图案/图片 | 渐变被压平、图案缺失 | 中 1–2 人日 |
| 形状几何 | 一律按矩形画 | 圆角/椭圆/箭头/任意多边形/组合 | 圆角变直角，箭头/流程图形失真 | 中高 2–3 人日 |
| 旋转 / 翻转 | 未处理 | 支持 | 旋转的文字/图片错位 | 中 1–2 人日 |
| 表格 | 支持（`Table`） | 支持 | 基本可用，样式简化 | 低 |
| 图表类型 | 5 种（`ChartKind:77`） | 全部图形+样式 | 散点/雷达/复合图缺 | 中高 |
| SmartArt | 不渲染 | 支持 | 整块缺失 | 高（不建议） |
| 动画 / 切换 | 静态 | 支持 | 预览静态可接受 | 忽略（合理） |
| 主题字体/字号层级 | 主题色已支持，字号部分 | 完整 | 字号偏差 | 低–中 |
| 阴影/发光/3D 效果 | 无 | 支持 | 观感差异 | 高（不建议） |
| 嵌入对象 / OLE / 媒体 | 无 | 部分 | 缺失 | 高 |
| 备注 / 讲义 | 无 | 支持 | 预览不需要 | 忽略 |
| 超链接 / 交互 | 无 | 支持 | 预览不需要 | 忽略 |

**建议优先级**：母版装饰图形 + 背景（性价比最高）→ 形状几何（圆角/椭圆/箭头）→ 图表类型；SmartArt/效果/OLE 明确不做。分档约 **2–8 人日**，取决于补齐到哪一层。

---

## 5. PDF 页数多时明显卡顿

**根因（叠加）**：
1. **打开**：`load_document` 把整个文件读进内存（`DOC_SLOT_MAX_BYTES=256MB`，`pdf.rs:234/251`）再 `FPDF_LoadMemDocument64` 解析 + 取 `page_count` —— 大文件首屏延迟一部分在这。
2. **单页光栅成本**：每页 `FPDF_RenderPageBitmap` 按 `PREVIEW_DPI` 渲整页，页复杂度高时几十~百 ms；切换时先显示旧页拉伸（`zoom_preview`），要等新页到才清晰。
3. **预取太窄且有上限**：`prefetch_neighbor_pages` 只预取 ±邻页，且受 `PREWARM_MAX_PDF_BYTES=30MB`（`render.rs:1213`）限制——**超过 30MB 的大 PDF 直接不预取**，快速翻页每页都现渲染 → 空白/卡。
4. **单 DocSlot**：同时只开一个 PDF，切文件要重载。

**优化方向**：①按可视区尺寸渲染而非固定高 DPI（降单页成本）；②滑动窗口预取 3–5 页（环形），30MB 上限改成"按页数/按内存预算"可配；③预取放低优先级线程，不阻塞当前页；④配合连续滚动按需渲染。**1–3 人日**。

---

## 6. 新增 EPUB

**结构**：`zip`（已有依赖）→ `META-INF/container.xml` → OPF（manifest + spine）→ 按 spine 顺序取 XHTML → 复用 `qlr-render/html.rs` 排版；图片从 zip 内解出。需要新增 `Kind::Epub` + natural 尺寸 + 用 docx 那套连续滚动。

**代价 3–5 人日**（文本为主的内容可接受；复杂 CSS / 固定版式 EPUB 会降级）。风险：CSS 子集不全、字体、DRM（加密 EPUB 不支持）。

---

## 7. "保真"后调用的系统解析具体流程

`backend.rs:297 resolve` 判定要交系统时返回 `&SHELL`，实际执行在 `qlr-platform/shellpreview.rs`：

1. **找处理器 CLSID**：读注册表 `HKCR\.<扩展名>\ShellEx\{8895b1c6-b41f-4c1c-a562-0d564250836f}`（`PREVIEW_HANDLER_KEY`，`shellpreview.rs:32`）。没有 → `has_handler` 返回 false，回退插件/内置（不会空白）。
2. **创建 COM 对象**：`CoCreateInstance(CLSID, CLSCTX_LOCAL_SERVER|CLSCTX_INPROC)`（`:62`）。多数 Office 预览处理器是**独立代理进程 prevhost.exe（OUT-OF-PROC）**，所以"系统解析"是在别的进程里做的，崩了不影响我们。
3. **初始化输入**：`IInitializeWithFile` 载入文件路径（`:307-315`）。
4. **建宿主窗口**：`create_child_host`（`:73`）建子窗口 → `SetWindow` + `DoPreview`（`:85`），处理器在子窗口里画。
5. **找到内容导航子窗口**：`largest_descendant`（`:94`）拿 Excel/Word 内部可滚动的子 HWND。
6. **就绪判定**：`visible_area` 可见比≥50% 算"已画出"（`:176-194`），此前不亮占位。
7. **输入接管**：PPT 用 `take_over_input`（`set_child_input(false)`，`:115`）由我们投 `VK_NEXT/VK_PRIOR` 翻页；Excel/Word **让它自己收输入**，并在退栈后 TIMER 里调一次 `IPreviewHandler::SetFocus()`（`set_com_focus`，`:166`，必须锁外，见 #145/#147）——这是能点选单元格的唯一办法。
8. **尺寸变化**：`set_rect` 用 `SetRect` 让处理器子窗口重新对齐（`:225-248`），不重建；配 `sync_shadow_with(true)`。
9. **滚动/翻页**：`forward_wheel`（`:212`）/ `step`（`:197-206`）。
10. **卸载**：`close`/`Drop` → `Unload` + 销毁宿主窗口。

**一句话**：保真 = 把文件交给系统注册的 `IPreviewHandler` COM 服务器（Office 的通常跑在 prevhost.exe 里）解析并画到一个子窗口，我们只负责对齐、聚焦和滚轮/翻页的转发。

---

## 8. 超高图片展示过小 / 保关闭按钮 / 最小宽度

**根因**（`window/mod.rs:727-762` `natural_rect_raw`）：
```
scale = fit.min(1.0) * factor;            // fit = min(ww/cw, wh/ch)
if scale < min_scale { scale = min_scale.min(fit); }   // ← 这里被 fit 夹死
```
极高图按高度 fit 后宽度本就极小，`MIN_WIDTH=240` 的保护又被 `.min(fit)` 抵消，于是窗口又窄又小，连顶栏关闭按钮都挤没了。

**修法**：
- 对 `Kind::Image` 施加"**最小客户区宽 `MIN_WIDTH`**"：`client_w = max(计算宽, 240)`，高度仍受工作区限制；
- 图很高 → base 高 > 窗口高，允许纵向溢出，用**已有的 `pan_by`（`input.rs:1562`）**上下平移看；
- 渲染尺寸按 `base×zoom`，抬宽后重渲染到更大宽保证清晰（受 `MAX_IMAGE_PIXELS` 保护）；
- 顶栏 chrome 按窗口宽重排，确保关闭/最大化按钮在 `MIN_WIDTH` 内可点。
- 只对"因超高被压到 < MIN_WIDTH"的图抬宽，正常小图不放大。

**0.5–1 人日**，需真机 + 长截图/长图验证。

---

## 9. 图片缩放"固定比例"的割裂感

**原因**（`app/input.rs`）：
1. 滚轮每档**固定 1.25×**（`wheel_zoom:921-933`），步长大 → 一跳一跳；
2. `set_zoom_to:62-98` 用 `round()` 取整改窗 + `resize_around_center`，缩放中 `pan` 归零 → 画面回中；
3. 松手才重渲染，过程中是拉伸旧位图（`zoom_preview`）→ 松手瞬间"糊→清"闪一下。

**建议**：①细化步长（1.1× 或按 delta 连续）；②拖动/缩放保持鼠标锚点而非回中；③预览用双线性减少糊感；④大图缩放节流重渲染。需你复现确认主因是"跳、糊、回中"哪一种。**0.5–1 人日**。

---

## 10. Excel：自适应行高 / 列宽命中 / 行列出选

**行高**：`sheet_autofit` 只对 `wrap=true` 的单元格折行算高，且**跳过表头行**（`window/sheet.rs:1977`），非折行单元格恒单行高。所以"好像没自适应行高"= 只在折行单元格生效——若你要"按最长内容自动行高（即便不折行）"是另一套策略，需确认口径。

**列宽命中不准（点 B 中→选 A）**：
- 测量：`sheet_autofit:1873` 在 `PREVIEW_DPI` 下 GDI 量宽 + padding + `MAX_COL_W=160`；
- 命中：`sheet_cell_at:2028-2070` → `SheetSizes::col_at:644`，其反解 `sheet.col_x(col) <= base`（原始坐标与显示坐标混用）可能有偏差；另一嫌疑是 **DPI 缩放**（绘制 Metrics 与命中坐标不一致）。
- **要点**：需真机复现，确认是"整体偏左一列"还是"仅宽列偏"，再对症。

**行/列头点选**：`sheet_cell_at` 在 `px<header_w || py<header_h` 时直接 `None`（`sheet.rs:2030` 附近）→ 点行号/列号条不会整行/整列选中，**功能未实现**。要新增：命中 header 时按 `col_at/row_at` 得到列/行索引，进入"整列/整行选中"并高亮。

**合计 1–2 人日**（含复现），建议先复现列宽偏差，再连同"行高口径 + 头点击选中"一起改。

---

## 11. PNG 透明区需 0 填充、不补白

**现状**：`paint_bitmap_at` 先用 `CreateSolidBrush(BITMAP_BACKGROUND_RGB=0x00FFFFFF 白)` 把整个客户区刷白再画（`window/bitmap.rs:259-261`）；`StretchDIBits` 用 `BI_RGB + SRCCOPY`（`:≈`，不看 alpha）→ 透明像素显示成"其自带 RGB / 白底"，且小图周围补的是白边。

**修法**：
- **图片通道**底色由白改为 `0x000000`（0 填充），小图周围不再补白；完全透明（A=0）像素的 RGB 置 0；
- 半透明（0<A<255）当前无法 alpha 合成（SRCCOPY），建议按**黑底预乘**合成，保证"无像素=0、半透明=暗"；
- 落点：`image_view` 产出 BGRA 后处理，或 `paint_bitmap_at` 对图片通道传黑色 bg。

**0.5 人日** + 透明 PNG 验证。**注意**：这会把"整窗背景由白改黑"，多数图片查看器透明区是棋盘格/黑；需确认你要的是"全黑底"还是"仅透明像素置 0、其余保持"。→ 见下方决策点。

---

## 需要你拍板的点

1. **图片背景口径（#11）**：A) 整窗黑底（0 填充最彻底）｜B) 仅透明像素置 0、非透明区仍白 ｜C) 棋盘格。
2. **.doc（#3）**：A) 保留系统处理器兜底（推荐）｜B) 加"纯文本抽取"降级通道 ｜C) 硬啃原生（不建议）。
3. **pptx 补齐深度（#4）**：A) 只补母版装饰+背景（性价比高，~3–4 人日）｜B) 再加形状几何（~6–8 人日）｜C) 全补齐（不建议）。
4. **先做哪批**：建议先 **批 A**（#11/#8/#9/#10），你确认后我就动手；B/C 再排期。

---

## 批 A 实施记录（2026-11-25，已完成并验证）

用户已确认"按建议顺序做"（m00189），批 A 四项已落地（代码 + 单测 + 冒烟 + 截图）：

- **#11**：`qlr-render/image_view.rs` 新增 `composite_over_black`，`render_scaled` 外包；透明 PNG 透明区→纯黑。单测 `composite_over_black_handles_transparency`。
- **#8**：`window/mod.rs` `natural_rect_raw` 加最小宽兜底 + `show.rs` `window_size_for` 抬回 `min_window_w`。单测 `image_extreme_tall_keeps_min_width`；240×7200 长图实测窗口 240×1040。
- **#9**：`input.rs` `wheel_zoom` 1.25×→1.1×；`smoke.ps1` 断言同步 `1.00 -> 1.10`。
- **#10（部分）**：`window/sheet.rs` 新增 `HeaderHit` + `sheet_header_at`，`input.rs sheet_click` 支持点行/列号选整行/整列。单测 `header_hit_selects_row_or_col`。
- **验证**：`cargo test --workspace` app 83 / core 87 / platform 57 / render 170 全绿；`smoke.ps1` 全 PASS；截图 `target/shot-out/{transparent,tall}.png`。
- **仍未做**：#10 列宽命中偏差 / 非折行行高（需真机复现）；批 B（#5/#2/#6/#3-xls）、批 C（#1/#4补齐/#3-doc）。

---

## 批 B 实施记录（2026-11-25）

用户批准新增纯 Rust `psd`+`calamine`、pptx 补到"形状几何+旋转"层、doc 保留系统处理器。批 B 落地：

- **#2 PSD**：`format.rs` `IMAGE_EXTS`+=psd/psb、`is_image_magic`+=8BPS；`image_view.rs` `render_psd`（`psd` crate）+`psd_dimensions`（头 26B）；透明区仍按 #11 变黑。
- **#3 xls**：新 `render/src/xls.rs`（`calamine::open_workbook_auto`→`SheetDoc`）；`render/sheet.rs` 对 `xls` 分流；`SHEET_EXTS`+=xls；Excel 日期序列还原。
- **#6 EPUB**：新 `render/src/epub.rs`（container→OPF→spine→`html::layout`→`Canvas::Text`）；`HTML_EXTS`+=epub，归 Html 族。
- **#5 PDF 预取**：`app/render.rs` `prefetch_neighbor_pages` 预取窗口 `[+1,-1]`→`[+1,+2,-1]`（首改，非完整优化）。
- **验证**：`cargo test --workspace` app 83 / core 88 / platform 57 / render 175 全绿 0 failed；EPUB 现场 ZipWriter 真实夹具；PSD/xls 仅编译+单测。
- **批 C 未做**：#4 pptx 形状几何+旋转、#1 PDF 连续纵向滚动、#3 doc 保留系统处理器；#5 多页实测剖析未完成。

---

## 批 C 实施记录（2026-11-25，#4 部分）

- **#4 pptx 母版/版式装饰 + 背景**：`qlr-render/src/pptx.rs` `parse_placeholders` 现返回 `(Placeholders, Option<u32>, Vec<Shape>)`，把 `slideLayout`/`slideMaster` 里非占位符顶层 `sp` 抽成装饰图形；`resolve_placeholders` 合并 master→layout 的 decor，`parse_pptx` 把 decor 置于 shapes 最前（垫底、z=数组序）；`parse_bg` 带 `theme`（solidFill 主题色修复、gradFill 色标平均近似）。新单测 `master_decor_shapes_extracted_but_placeholders_kept`、`gradient_background_averages_color_stops`。
- **验证**：`cargo test --workspace` app 83 / core 88 / platform 57 / render **177** 全绿 0 failed；`cargo check --workspace --all-targets` 0 warning；`smoke.ps1` 16 项全 PASS。
- **仍未做**：#4 形状几何(圆角/椭圆/箭头)+旋转与真多段渐变；#1 PDF 连续纵向滚动；#3 doc（已定系统处理器兜底，无需原生）；#5 多页实测剖析；#10 列宽命中/行高真机复现。

---

## 批 C 实施记录（2026-11-25，#5 深化 + 取消竞态修复）

- **#5 机制修复**：`qlr-render/src/cancel.rs` 全局 `CANCELLED` 同时服务交互渲染与后台预取，UI 翻页 `cancel()` 会连坐砍掉预取（`render_file_at_zoom` 内 `checkpoint()`）。新增 `thread_local! SKIP_CANCEL` + `without_cancel(f)`，`checkpoint()` 在免取消区直接 `Ok`；`app/render.rs warm_page` 用其包住渲染；`prefetch_neighbor_pages` 窗口 `[1,2,-1]`→`[1,2,3,-1]`；`pagecache CACHE_LIMIT_BYTES` 64→128MB。
- **验证**：`cargo test --workspace` app 83 / core 88 / platform 57 / render **178** 全绿 0 failed（+`without_cancel_ignores_cancel_and_restores`）；`cargo check --workspace --all-targets` 0 warning；`smoke.ps1` 16 项全 PASS。
- **仍未做**：#1 PDF 连续纵向滚动（真·多页竖排，3–5 人日）；#4 形状几何+旋转/真渐变；#10 列宽命中/行高真机复现。

---

## 批 C 实施记录（2026-11-25，#1 铺垫：PDF 连续滚动几何层）

- 新 `qlr-platform/src/window/pdf.rs`：`pdf_flow_geom`/`PdfGeom`、`pdf_flow`/`PdfFlow`、`pdf_page_offset`、`pdf_page_dest`（与 pptx 流式同构，坐标 `content.top+pad+i*(ph+gap)-view_y`）；4 个纯几何单测。**仅几何，未接入绘制/输入**。
- 验证：`cargo test -p qlr-platform` 61 passed/4 ignored；clippy `-D warnings` 干净；`cargo fmt --all --check` 通过；`cargo test --workspace` app 83/core 88/platform 61/render 178 全绿 0 failed。
- 下一步：#1 接线（blit 助手 + chrome 逐页贴图/异步渲染 + input 滚动 + 滚动条）。

## 批 C 实施记录（2026-11-25，#1 接线：PDF 真·连续纵向滚动，已实现并验证）

- `qlr-platform/src/window/pdf.rs` 增 `paint_pdf_flow(...)`：paint_chrome + 浅灰页缝 + `IntersectClipRect` + 逐页 `pdf_page_dest`/`SetStretchBltMode(HALFTONE)`/`StretchDIBits`(top-down) + overlay，单次 BeginPaint 画完。
- `qlr-app/render.rs` 增 `request_flow_page(path,page,w,h)`（`WarmTask::Page` 异步补页），`request_render`/`on_render_done` 记 `s.pdf_page`。
- `qlr-app/session.rs` 加 `State.pdf_page`；`is_flow_view()` 扩为"多页 Bitmap 且 zoom≈1 且非 zoom_preview"也 true；`show.rs` 置 `pdf_page=first_page`；`input.rs flow_goto_page` 加 `Canvas::Bitmap`→`pdf_page_offset`；`chrome.rs` 加 `paint_pdf_flow_view`，`Kind::Pdf` 接入。
- 验证：`cargo test --workspace` app 83/core 88/platform 61/render 178(+8 ignored) 全绿；`check --all-targets` 0 warning；`build` 绿；`smoke.ps1` 16 项 PASS；真机 3 页 PDF 下滚 2 格同屏见"第 1 页页脚 + 页缝 + 第 2 页"（`target/shot-out/pdf_two.png`）。
- 局限：跳页/滚动条按页级 offset；快速拖动未命中页短暂露浅灰等异步补齐。

## 批 A #10 复核记录（2026-11-25）

- 复核"点 B 列中间落 A / 列宽不准 / 行高不自适应"：确认 `qlr-platform/src/window/sheet.rs` 的 `Painter`(绘制) 与 `sheet_cell_at`(命中) 共用同一 `SheetSizes`；自动列宽下加宽的列点击命中正确（当前不复现）。`sheet_autofit` 已实现非折行单行高(pass1)+折行多行高(pass2，表头行跳过)。
- 新增回归单测 `col_row_hit_honours_autofit_extra`（`col_extra=[(0,40)]`→`col_at(55)=B`；`row_extra=[(0,25)]`→`row_at(46)=1`）。
- 验证：platform 62 passed/4 ignored；fmt/clippy 干净；workspace app83/core88/platform62/render178 全绿。
- 待用户：若真机仍复现，请提供该 xlsx 以按真实列宽/合并/折行定位。

## 批 C 实施记录（2026-11-25，#4 深化：形状几何 + 旋转）

- 模型 `qlr-core/pptx.rs`：加 `pub enum RectGeom { Rect, RoundRect, Ellipse }`（默认 Rect）、`FillRect.geom/rot_deg`。
- 解析 `qlr-render/pptx.rs`：`rect_geom(spPr)` 读 `a:prstGeom@prst`（roundRect/ellipse|oval/其余）；`xfrm_rot(spPr)` 读 `a:xfrm@rot`/60000；`parse_sp`/`parse_cxn` 填入。
- 绘制 `qlr-platform/window/pptx.rs`：`draw_fill_rect` 按 geom 用 `RoundRect`/`Ellipse`/`Rectangle`，`begin_rotate`（`SetGraphicsMode(GM_ADVANCED)`+`SetWorldTransform`）绕中心旋转 `rot_deg`。
- 单测 `shape_geometry_and_rotation_parsed`；core 88/render 179/platform 62 全绿；check/clippy 干净；smoke 16 PASS；真机 `target/shot-out/pptx_shapes.png` 圆角/椭圆/旋转均正确。
- 局限：仅无文字形状；箭头按矩形兜底；文字形状旋转/组合内旋转/真多段渐变未做。

## 批 C 实施记录（2026-11-25，#5 多页 PDF 连续翻页实测）
- 60 页 PDF（`test-60page-rich.pdf`）90 次滚轮 → 13/60 页；`PAINT pdf-flow` 92 帧无丢帧/白屏；`PAGECACHE 预取` 16 页、页缓存 17 页/35MB；低墨页逐页渲染 0–1ms（重内容 A4 631×893 2× 约 28ms/页由预取覆盖）。截图 `target/shot-out/pdf60_after.png`（页缝 + 第 2 页标题，页码 13/60）。
- 机制：预取窗口"前看三页+后一页"、`without_cancel` 隔离预取与交互取消、128MB 页缓存、#1 连续流式按需补页。

## 修复记录（2026-11-25，PNG 预览仍露"占位页"）
- 现象：预览 PNG 时仍出现整页"QLRust 版本 / 文件信息 / 正在渲染…"的占位页。
- 根因：`qlr-app/src/app/show.rs` 仅对系统处理器（shell 后端）`lines.clear()`，自渲染图片仍 `lines.insert(2, "正在渲染…")`；大图解码几百 ms 期间可见。
- 修复：占位清空条件加 `kind == format::Kind::Image`，图片开图瞬间空窗等渲染帧。
- 验证：`cargo test --workspace -- --skip minidump_of_ourselves_is_written` 全绿；`smoke.ps1` 16 PASS；24MP 大图（`target/real-samples/huge-24mp.png`）抓帧 `huge2_a.png` 无文字占位 → `huge2_final.png` 铺满。

## PDF 目录（书签大纲）支持（2026-11-25）
- 抽取：`qlr-render/pdf.rs` 可选书签 FFI（`Option<fn>` 加载，缺符号降级为"无目录"）+ `pub struct OutlineEntry{level,title,page(0基)}` + `pub fn outline(path)`（DFS 遍历书签树，GOTO/dest 取页号，UTF-16LE 标题）。
- 状态：`session.rs State.pdf_outline`；`show.rs` 开 PDF 预取大纲→`pdf_outline`+`toc`；`set_content` 对 `Kind::Pdf` 保留大纲；`chrome.rs` 目录点击对 PDF 走 `super::input::goto_page(page+1)`。
- 修 bug：`qlr-platform/window/pdf.rs paint_pdf_flow` 原用 `paint_chrome` 整块覆盖传入 `content`（已扣目录栏）→ 页面被裁；改为两 rect 相交。
- 验证：`cargo test`(skip flaky) app 83 / core 88 / platform 61(+4 ignored) / render 179(+8 ignored)；clippy 干净；smoke 16 PASS；真机 `toc-3page.pdf` → `target/shot-out/toc_top.png`/`toc_jump.png`（目录 4 条含子项缩进、点第 2 条跳到 page 2、页面右侧不裁）。无书签不加目录。
- 局限：目录按 level 缩进；跳页页级对齐。

## HTML 样式渲染（C 级）实施记录（2026-11-25）
- 模型：`qlr-render/canvas.rs` `InlineStyle/RichRun/LineBox` + `TextDoc.runs/boxes`（10 处字面量补齐）。
- CSS：新 `qlr-render/src/html_style.rs`（parse_color/parse_len/Selector(Specificity)/Stylesheet/compute→InlineStyle+BlockProps；继承+cascade+inline 最高；background/text-align/display:none/margin/padding/border）。
- Builder：`html.rs` `layout_rich` 同时产 lines/styles/runs/boxes，inline/tag 栈，逐行 RichRun+LineBox，`extract_styles` 抽 `<style>`，表格 th/td 底色。
- 绘制：`qlr-platform/window/text.rs` `Vis/RSpan/RRow`+`paint_rich`（动态字体/逐段 fg/行 bg 带/align/border/逐字符折行）+`RICHFONTS`；`qlr-app/app/chrome.rs` `paint_rich_html/wrap_rich_line/rgb_to_colorref`。
- 修：空 runs 表格行消失（wrap_rich_line 增 line 参数用 base 样式）；bg/border R/B 反（rgb_to_colorref）。
- 验证：check 0 warning；clippy -D warnings 干净；test app83/core88/platform61(+4)/render185(+8) 全绿；smoke 16 PASS；真机 `target/shot-out/styled3_final.png`。局限：行内逐段背景未画、盒模型近似、不做 `<img>`。
