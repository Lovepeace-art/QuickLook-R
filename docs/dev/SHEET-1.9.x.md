# 表格（xlsx）预览 1.9.x 阶段状态记录

> 更新时间：2026-11-17 · 状态：**已冻结（feature freeze）** · 最新提交 `8971ef4`（origin/main 已同步）

用户决定（m07794 / m07798）：表格已经"暂时能用"，**先冻结**，不再往里加功能；
接下来开新窗口做 docx / pptx。本文把 1.9.0 → 1.9.4-fix2 的改动与结论收在一处，
后续接手直接看这一份，不必翻 2500 行 LOG。

---

## 一、结论（一句话）

xlsx/xlsm/xltx/xltm 改成**纯 Rust 内置原生渲染**，零 Office / 零 COM / 零 WebView：

| 指标 | 原生渲染 | 原系统处理器（IPreviewHandler）路线 |
|---|---|---|
| 2.3 万格真实表（`real-monthly.xlsx`） | 解析 **17.6ms**、首帧 **5ms + 18ms** | 冷启 **1083ms** / 热启 812ms |
| 16 万格大表（`big-20k.xlsx`） | 解析 **104.5ms**、出帧 ~120ms | 更慢，且随行数线性恶化 |
| 宿主常驻内存 | **16.0MB**（空载 10.0MB，+6MB/表） | Office 常驻 **130–300MB**，且收起后不退 |
| 是否需要本机装 Office | **不需要** | 必须 |

决策与备选方案对比写在 `docs/adr/0011-native-sheet-preview.md`（含 office2pdf 被否决的实测：
2 万行 xlsx 转 PDF 24.4s / 7.2GB 峰值；LibreOffice 350–700MB 且本机未装）。

---

## 二、已完成清单（按提交顺序）

| 提交 | 版本 | 内容 |
|---|---|---|
| `1aba1a7` | 1.9.0 | xlsx 原生渲染主体：`Kind::Sheet` + `qlr-core::sheet` 数据模型 + `qlr-render::sheet` 手写 OOXML 解析（workbook/sharedStrings/styles/theme/数字格式/合并/冻结）+ `qlr-platform::window::sheet` 绘制（只画可见行列、`####` 与溢出规则、冻结区） |
| `3b0ae3b` | 1.9.0 续 | 翻页键切工作表 + 顶栏页码框复用（`sheet_layout` 读 activeTab，`pages`=工作表数） |
| `5a3160b` | 1.9.1 | 拖表头调列宽/行高（`SheetSizes` 贯穿全部几何与命中测试，视图内生效不写回文件）；`Cell.numeric` 修正"文本被当数字画 `####`"；竖直滚动条 |
| `33cb9ca` | 1.9.2 | 底部工作表标签栏（`sheet_tabs.rs`，点击切表） |
| `3768dd3` | 1.9.3 | 条件格式：色阶（2/3 色插值）、数据条、cellIs + dxf 条件红字；`<dxfs>` 解析 |
| `3781153` | 1.9.4 | 单元格选中 / 拖拽多选 + `Ctrl+C` 复制（TSV）、滚轮 `WM_MOUSEWHEEL` 直投、`clipboard.rs`（CF_UNICODETEXT） |
| `83fa14f` | 1.9.4-fix | 复制改走系统 `RegisterHotKey`（不抢焦点也能收 Ctrl+C）；裸启动可见反馈（后被用户否掉） |
| `8971ef4` | 1.9.4-fix2 | 删掉启动提示卡片（零弹窗）；**托盘 `0x80070005` 真因修复**：exe 的 Medium 完整性标签把进程 IL 压低、被 High Explorer 的 UIPI 拒绝 → 部署改 `High` |

配套：`scripts/verify-sheet.ps1`（13 项 PASS：内核接管 / 出帧 / ink / 大表预算 / 坏文件不 panic / `.xls` 不误判 / 条件格式）、
`docs/screenshots/preview-sheet.png`、README 支持表与系统要求、CONVENTIONS #162–#172。

---

## 三、已验证的回归基线

- `cargo test --workspace --offline`：**363 passed / 0 failed / 13 ignored**（1.8.3 基线 318/10）
- `cargo clippy --workspace --all-targets -- -D warnings`：零输出；`cargo fmt --all -- --check`：干净
- `scripts/verify-sheet.ps1`：**13 PASS / 0 FAIL / 0 SKIP**
- `scripts/verify-office.ps1`：**23 PASS**（docx/pptx 仍走系统处理器，未受影响）
- `scripts/smoke.ps1`：16 PASS · 1 SKIP（SKIP=托盘图标，管理员会话下 UIPI 拒绝，非回归）
- 常驻资源（`real-monthly.xlsx`，23,167 格）：WS 16.0MB / Private 5.1MB / 句柄 201 / 线程 7 / GDI 7

---

## 四、已知限制（冻结时接受，未做）

**支持范围**
1. 公式**不求值**，只显示文件里的 `<v>` 缓存（无缓存则空白，`t="e"` 的 `#REF!` 原样显示）。
2. 条件格式只做色阶 / 数据条 / cellIs；**图标集、top10、文本包含、expression 跳过**。
3. 高级数字格式退化为近似：`[Red]`、科学计数法 `E+`、分数、`[$-409]` 多段式。
4. 工作表内**图片 / 图表 / SmartArt / 形状**不画（`drawing` 关系未解析）。
5. 无打印区域 / 分页符 / 页面缩放的模拟。

**交互**
6. 标签栏放不下时**只少画、不能横向滚动**（表多时靠 PageUp/Down 或顶栏页码框切）。
7. 冻结窗格按"冻结行列不滚动"实现，复杂多区冻结（如只冻结 C3 右下）未做。
8. **隐藏行列**：解析器已读 `hidden` 标记，但绘制端尚未跳过（打开带隐藏列的表会看到本应隐藏的列）。
9. 单元格**可选中/可复制，但不可编辑**（预览器定位，不需要编辑）。

**工程约束（部署必读）**
10. 部署时 `qlrust.exe` 与插件 dll 的完整性标签必须是 **`High`**（或至少 ≥ 任务栏 Explorer 的级别）。
    `Medium` 会把进程 IL 压到 Medium，托盘 `NIM_ADD` 被 UIPI 拒（`0x80070005`）。见 CONVENTIONS #172。
11. 清单保持 `asInvoker`：改 `highestAvailable` 在 UAC 开启的机器上会弹提权框。

---

## 五、如果要解冻，建议的下一步（按性价比排序）

1. **隐藏行列**（半天）：解析侧已有 `hidden`，绘制端跳过即可。
2. **标签栏横向滚动**（半天）：表多的工作簿才能点到最后几张。
3. 工作表内图片/图表先做"占位框 + 标题"（1–2 天），完整图表不划算。
4. 公式引擎、图标集、打印布局：**不建议做**（投入产出比差，且不影响"打开就能看清"）。

---

## 六、与 docx / pptx 的关系（新窗口的起点）

- 本次只动了 xlsx；**docx / pptx 仍走系统 `IPreviewHandler`**，行为未变（verify-office 23 PASS 佐证）。
- 前期探针结论（`_bench/office2pdf/spike/`）：
  - **docx 原生可行且便宜**：真实中文合同解析+排版+绘制 **15.1ms / 13.7MB**，可读性 100%、"像 Word" 90%；
    缺口集中在页眉页脚/分页/浮动图/编号语义/中西文间距，**日常规整文档约 11–17 人天**（覆盖 80% 场景），全套 26–39 人天。
  - **pptx 原生可行**：静态幻灯片 1280×720 版式/形状/图片/文本可画，主要缺口是字体按 run 的 `a:latin`/`a:ea` 分流、
    动画/母版继承/图表；建议先补"形状层"，性价比最高。
- 建议路线：先 docx（用户日常合同/报告最多），pptx 次之；两者都可复用本次的"数据模型 + 绘制层分离"结构
  （`qlr-core` 模型 / `qlr-render` 解析 / `qlr-platform::window` 绘制 / `qlr-app` 壳层），以及 `SheetSizes` 那种"视图内覆盖不写回文件"的思路。
