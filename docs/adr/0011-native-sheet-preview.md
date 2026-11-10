# ADR-0011：表格预览改为内置原生渲染（自己做 xlsx 网格），不走 Office / LibreOffice

- 状态：已采纳
- 日期：2026-11-10
- 相关：ADR-0001（路线 A 全量重写）、ADR-0009（预览后端契约）、ADR-0010（不做全面多进程）、`crates/qlr-core/src/sheet.rs`、`crates/qlr-render/src/sheet.rs`、`crates/qlr-platform/src/window/sheet.rs`

## 背景

Office 三件（docx / xlsx / pptx）此前走系统 `IPreviewHandler`（`ShellBackend`）。这条路有两个硬伤：

1. **冷启慢**：本机实测（同一台机器、真实业务文件、`SHOW elapsed` 口径）

   | 文件 | 冷启（Office 未运行） | 收起后立刻再开 |
   |---|---|---|
   | 真实合同 `real-contract.docx` | 718 ms | 82 ms |
   | 月复盘 `real-monthly.xlsx`（384 KB / 5 sheet） | **1083 ms** | 812 ms |
   | 双十一 deck `real-deck-9mb.pptx` | 761 ms | 723 ms |
   | 62 MB deck | 1017 ms | 997 ms |

   只有 docx 能靠 Word 常驻变快；xlsx/pptx 即使热了也要 0.7–1.0 s。

2. **内存与依赖**：系统处理器会拉起 Office 进程，常驻 130–300 MB，收起预览后不退出、不回落；
   且要求本机装了 Microsoft Office（`plugins` 之外的隐式依赖）。

用户据此提出两个目标：**更优性能** + **摆脱 Office 安装依赖**，并明确「qlr 是闲置进程回收的，
闲置一段时间后除图片/文本外都会释放内存」——所以不能靠「把 Office 常驻保热」来优化冷启。

## 决策

1. **表格（xlsx / xlsm / xltx / xltm）改为内置原生渲染**：新增 `Kind::Sheet`，由
   `SheetViewer` 解析 OOXML、`paint_sheet` 用 GDI 逐格画网格。不调 Office、不调
   LibreOffice、不起外部进程、不引第三方 Office 库。
2. **数据与绘制分离**：`Canvas::Sheet(Box<SheetDoc>)` 只带**数据**（单元格文本 + 样式 + 几何），
   像素由绘制端按当前视口现算。这样"滚一行"只需重画可见的几十行，不需要把整张表铺成位图
   （2 万行的表按像素铺开是几十万像素宽，位图路线走不通）。
3. **后端不改结构**：`NativeBackend::can_handle` 本来就是 `viewer_for(kind).is_some()`，
   `SheetViewer` 上线后表格自动归 native（priority 0），比插一个新后端少一圈改动。
4. **系统处理器保留为兜底**：`.xls`/`.et`（老式 BIFF）本轮不做，仍落回 `ShellBackend` /
   插件卡片；坏文件解析失败时也退回提示文字，不 panic。

## 为什么不是别的方案

| 方案 | 否决理由（实测） |
|---|---|
| **office2pdf**（Rust + Typst，71.9 MB 单 exe） | 保真度尚可（真实 deck 17/17、55/55 页一致），但内存与耗时不可接受：2 万行 xlsx **24.4 s / 7.2 GB**、真实月复盘 xlsx **7.7 s / 2.9 GB**；120 张 20 行表 4.5 s / 1.6 GB；且只认 `.docx/.xlsx/.pptx`（docm/xlsm/odt 一律 `unsupported file format`），宏与老格式全覆盖不了。`--streaming` 更差（59.9 s / 7.8 GB）。 |
| **LibreOffice 外挂**（`soffice --convert-to pdf`） | 依赖 350–700 MB、冷启 1.5–4 s、常驻内存 200–400 MB，在用户两个指标上**都更差**；且等于把一个更大的"Office 依赖"重新装回来。（本机未取得实测数据：管理安装解压版 `soffice.exe` 直接 exit 77「无法启动应用程序」。） |
| **只吃文档内嵌缩略图** | 实测 xlsx **0/111 全无**、docx 17%、pptx 43%，尺寸 256×192–576×324，放大到 900 px 必糊。 |
| **预览时拉起 Excel 窗口塞进来** | 又回到 Office 依赖 + 130–300 MB 常驻，且窗口嵌入要处理焦点/输入同步（CONVENTIONS 里 `IPreviewHandler` 的坑一箩筐）。 |

## 后果

**收益（本机实测，release / lto=fat）**

- 首帧：真实月复盘 xlsx `SHOW … elapsed=5ms` + `RENDER gen=1 sheet … elapsed=18ms`；
  2 万行 16 万格的 `big-20k.xlsx` 解析 **104.5 ms**、出帧 ~108 ms（对比 shell 路径 1083 ms 冷启）。
- 内存：一张 2.3 万格的表，预览态宿主 **16.0 MB**（空载 10.0 MB，即 +6 MB）；
  Office 路线同时是 130–300 MB。
- 滚动重画成本只跟**屏幕上那一屏**有关（23 k 格与 16 万格的表都是那个量级），与表大小无关。
- 独立可测：解析层 `qlr-render/src/sheet.rs` 单测 22 个（含 `#[ignore]` 性能用例），
  绘制层 `qlr-platform/src/window/sheet.rs` 单测 9 个，全部不碰 Office。

**代价 / 已知缺口（写进 README 支持表的"不支持"列）**

- 公式不求值，只显示 `<v>` 缓存（无缓存则空白）；条件格式 / 图表 / 图片 / 批注全丢。
- 字体名不落地（模型无 family 字段）：`FONTS` 只有 14 个固定档位，粗体按字号落 Heading1/2/3、
  非粗体一律 Normal、**斜体不画**；要按 pt 精确字号需给 `text.rs` 加动态字体缓存。
- 日期/时间串放不下时给"部分文本"而不是 `####`（预览只读、拉不宽列，故意与 Excel 不同）。
- 老式 `.xls`/`.et`、宏语义、1904 日期系统、隐藏行列、sheet 级缩放不做。
- 解析在进程内（release `panic="abort"`），所以做了硬上限（单表 40 万格、单条目 64 MB、
  总解压 192 MB、文本 512 字符…）+ 坏文件 fuzz 用例；**远期**若接入不可信来源，考虑按 ADR-0010
  的"可选进程外后端"形态做 sidecar。

**后续（未排期）**：P2 docx 分页（探针实测真实合同 15 ms / 13.7 MB、103 页文档 45 ms）、
P3 pptx 幻灯片（探针实测 9.8 MB deck 46 ms / 14.7 MB，62 MB deck 60 ms），均已验证可行，
见 LOG 2026-11-10 条目。
