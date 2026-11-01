# ADR-0007：渲染层独立成 crate + 单 worker 线程 + 代际号丢弃过期帧

- 状态：已采纳（M2 起头）
- 日期：2026-10-30

## 背景

M2 要接真实的解码/排版。三条路可选：

1. **在 UI 线程里直接渲染**——最省事，但 48MP JPEG 解码 175 ms、26 MB 文本整形十几毫秒，
   全都会直接变成"按空格后窗口卡住"。
2. **`qlr-platform` 里渲染**——会把 `image` 这类纯 CPU 依赖塞进平台层，平台层从此不能
   "只做 FFI"，也破坏了 `qlr-core` 可跨平台单测的分层。
3. **新 crate `qlr-render`（不依赖 `windows`）+ 一个常驻 worker 线程**。

## 决策

选 3，并且定死四条约定：

1. **`qlr-render` 不依赖 `windows`**：只产出 `Canvas::{Bitmap(Bitmap), Text(TextDoc)}`，
   像素格式固定 BGRA8。上屏（`StretchDIBits` / `DrawTextW`）留在 `qlr-platform`。
   好处：渲染逻辑能 `cargo test`（当前 18 条）、能被基准脚本直接调用、天然可换平台。
2. **一个常驻 worker 线程**，不是每次预览 spawn：线程创建本身 ~50 µs 起步，
   而预览是高频操作；常驻还能省掉解码器的重复初始化。
3. **worker 绝不碰 `HWND`**：`HWND` 不是 `Send`。worker 只把结果放进 `RESULT` 槽，
   再 `PostMessageW(host, WM_QLR_RENDER)` 通知 UI 线程来取。窗口操作全在 UI 线程。
4. **代际号（generation）丢弃过期帧**：用户连按空格翻文件时，上一张的解码结果不能盖掉
   当前这张。`State.render_gen` 每次请求自增，结果带回代际号，不匹配就丢弃并记 debug。

## 代价与已知欠账

- 多一次跨线程的像素所有权转移（`Vec<u8>` move，不拷贝；用 `Arc` 共享给 UI）。
- `qlr-app` 里多两个静态槽（`JOB_TX` / `RESULT`）和一把 `CONTENT` 锁。
  **规矩不变：持 `STATE` 锁时不许动窗口**；`WM_PAINT` 只读 `CONTENT`/`LINES`。
- 当前仍是"整图解码再缩放"（朴素路径），48MP 峰值 ~190 MB，未达 A8.1 的 ≤60 MB。
  这是**故意**的：先有基线（`bench-compare.md` 的可比数字），再换成按显示尺寸缩放解码。

## 被否掉的方案

- **线程池**：M2 一次只显示一个文件，池的调度收益为零，复杂度却是实打实的。
- **`std::sync::mpsc` + 阻塞在 UI 线程等结果**：等于没异步。
- **worker 直接 `ShowWindow`/`InvalidateRect`**：跨线程操作窗口在 Win32 上能跑但极易
  与消息泵竞态（尤其 `ShowWindow` 会同步重入 `wndproc`），收益为零、风险极高。
