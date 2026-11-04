# ADR-0010：不做全面多进程，采用「可选进程外后端」；首个且仅 pdfium 试点

- 状态：已采纳
- 日期：2026-11-04
- 相关：ADR-0001（路线 A 全量重写）、ADR-0008（插件进程内 C ABI）、`docs/dev/ARCH.md`、`crates/qlr-app/src/app/backend.rs`

## 背景

一份对平台层的评审把 QLR 概括为「核心强、外围弱」：窗口/绘制/钩子/DPI/无资源泄漏这些**底层**
扎实（速度护城河在此），但缺一层「宿主级」的管理——后端契约、会话生命周期、IPC/单实例、
可诊断的回收状态机；并提出把平台从「单进程渲染器」升级为「能管理多渲染后端（含隔离的第三
方进程）的宿主骨架」。用户据此问：**我们需不需要做多进程？**

先对齐仓库现状（评审部分基于 1.8.3-fix 之前的快照）：

- 评审点名的正确性缺陷**已修**：`GetFocus` 死代码 → 已改 `GetGUIThreadInfo.hwndFocus`（1.8.3-fix
  批次一，Win11 桌面 F2 改名不再吞空格）；锁中毒静默 → 已改 289 处 `with_state` 统一取回（批次三）。
- 单实例/IPC 不是缺失：`crates/qlr-platform/src/instance.rs`（`Local\QLRust.Instance` 互斥体 +
  `WM_COPYDATA`）；托盘 `tray.rs`；选中项 `selection.rs`；插件 `plugins::host` + `qlr-plugin-api`。
- `window.rs` 3506 行的结构债**已拆**（2026-11-04）：`window/{mod,shadow,text,settings,chrome,paint,
  scroll,audio,toc,bitmap}.rs`，`mod.rs` 1381 行，`verify-layers` §3b 有「单文件 ≤1600 行」棘轮。
- 已有**线程级租约 + 看门狗自愈**：`crates/qlr-app/src/app/render.rs` 的 `begin_lease/abort_lease/
  end_lease/mark_hung/check_render_hang` + `session.rs::Lease`/`RenderTrack`。它是「后端级租约」的雏形。

## 决策

1. **不做全面多进程。** 不把整个预览管线拆成「主进程 + 每格式子进程」。
2. **采用「可选进程外后端」作为一种后端形态**（由 `Isolation` 表达），而不是平台默认。
3. **进程外只留给不可信的二进制解码器**；当前唯一明确候选是进程内的 `pdfium.dll`
   （`crates/qlr-render/src/pdf.rs`）。是否真做 pdfium sidecar，作为**独立试点**另行批准，
   本 ADR 不授权实现。
4. **现在先补「后端能力契约」**：`PreviewBackend` 增加 `isolation()` / `interactive()` 两个元数据，
   把「这个后端在哪个进程、要不要转发输入」显式化并写进日志；线程级租约的判挂日志带上后端 id，
   向「后端级可诊断」靠一步。

## 理由

- **Office 已经进程外**：走系统 `IPreviewHandler`，由 `prevhost.exe` 承载（`crates/qlr-platform/src/
  shellpreview.rs` 注释）。给它自造 sidecar 是重复建设。
- **WebView2 自带多进程**：若将来 HTML 换 WebView2，Chromium 自己隔离 `msedgewebview2.exe`；现在的
  HTML 是自写解析器（`crates/qlr-render/src/html.rs`），更没有该问题。
- **速度护城河来自进程内**：冷启动 18–24 ms vs 原版 1052–1161 ms、1280×720 预览内存 18.6 vs 96.1 MB、
  包体 2.29 vs 278 MB。每预览拉子进程会让冷启动优势归零（除非养 warm pool，那又要求重构内存/租约，
  等于把原版「管理层厚重」搬回来）。高频自研（图片/文本/MD/PDF-展示）**必须**在进程内。
- **多进程的真实成本在管理层**：单实例防多 sidecar、主↔sidecar IPC、RGBA 零拷贝共享内存、
  会话租约/空闲回收状态机、崩溃看门狗、可诊断日志。一条不做，多进程只会把「静默失能」变成
  更难查的「静默崩溃」。这正是我们当前最薄、也最该先补的一层。

## 后果

- `PreviewBackend` 契约扩展（本 ADR 即落地）：`isolation() -> Isolation{InProc|SystemProcess|NeedsSidecar}`
  与 `interactive() -> bool`；`show_preview` 的 `BACKEND` 日志打印两者。
- `describe_job`（看门狗判挂日志）带上后端 id，便于定位「卡在哪个后端」。
- `Isolation::NeedsSidecar` 作为**契约预留**存在，当前无后端使用；将来隔离 pdfium 时在此声明，
  而不是散改四五个调用点。
- 进程外后端（若实现）必须复用现有 `PreviewBackend` 接口，**不改插件 ABI**（ADR-0008 不变），
  以「进程外宿主」再包一层。

## 何时才值得把某个后端搬进程外（判断表）

| 信号 | 进程内 | 进程外 |
|---|---|---|
| 纯 Rust / 无 unsafe 高危解析 | ✅ | — |
| 高频、进热路径（图片/文本/MD/自研 PDF 展示） | ✅ 必须 | — |
| 第三方 C/C++ 二进制解码器、有 CVE 史（pdfium） | ⚠️ 风险 | ✅ 候选 |
| 会挂起/无响应（CAD、字体整形、重型视频容器） | — | ✅ 候选 |
| 系统已隔离（IPreviewHandler=prevhost、WebView2） | 直接用 | 无需自造 |

## 备选与否决理由

- **全面多进程**：吃掉冷启动/内存/包体收益，管理层成本最高，且当前无对应的强需求 → 否决。
- **自研 Office sidecar**：系统已用 `prevhost.exe` 隔离，重复建设 → 否决。
- **只做格式子集、不搭后端骨架**：无法替代现有工具、迁移意愿低（ADR-0001） → 否决。

## 后续

1. 本 ADR 落地后，下一步做「后端会话契约 + 租约状态机」的**进程内**部分：把现有线程级 `Lease`
   推广出「后端会话」维度（活跃/空闲/回收/崩溃四态 + 日志），不动热路径。
2. 若批准，做 **pdfium 单客户 sidecar 试点**（一个常驻子进程、一条 IPC、共享内存零拷贝、看门狗），
   验证「畸形 PDF 挂死/崩溃不带走主进程」后再谈推广。
3. `office 轻型 / html 改进` 优先走「进程内只读渲染」或系统隔离，不轻易开新进程。

## 2026-11-04 补充：已评估并暂缓 pdfium sidecar（崩溃探测）

按「后续 2」做了崩溃探测，结论是**暂缓 sidecar**（决策更新，覆盖「后续 2」的试点设想）：

- **探针**：临时 `examples/pdf_probe.rs`（跑完删除、不入库）对每个 PDF 单独起进程，走当前
  **进程内** pdfium 路径（`pdf_page_count` + 渲染第 0/1 页）；原生访问违例（AV）只会让该子进程
  以非 0 退出，外层按退出码判定。
- **语料**：4 个种子（8 页 / 3 页 / 1 页 / 200 页 Word）× 6 类变异（随机截断 5–95%、字节翻转≤300、
  随机窗口清零、`startxref`→巨大值、`/Length`→巨大值、头部前插 128B 垃圾）。
- **结果**：第一轮 164 文件 → 有帧 124 / 优雅 Err 1 / 取不到页数 39 / **崩溃 0**；
  扩大轮 1004 文件 → 719 / 2 / 283 / **崩溃 0**。两轮均 **0 AV、0 非 0 退出**，全部优雅返回。
- **结论**：现有语料下未证实进程内 pdfium 会崩，而 sidecar 的成本（活跃 +10–15 MB、首帧 +30–75 ms、
  新页 +0.1–1.5 ms）是实打实的 → **暂缓 pdfium sidecar**。保留本 ADR 的 `Isolation::NeedsSidecar`
  契约预留与 `QLRUST_PDF_INPROC` 回退位；四态会话状态机（`afc67b4`）作为后端级可诊断底座保留。
- **边界**：0/1004 ≠ 证明安全（随机变异覆盖不到加密/签名/object-stream/字体子集等定向攻击面）。
  若将来面向大规模不可信 PDF，优先低成本护栏（`SetUnhandledExceptionFilter`/vectored handler 只做
  优雅失败），或只对 MOTW/超大/加密 PDF 按需起子进程，再评估是否重开试点。
