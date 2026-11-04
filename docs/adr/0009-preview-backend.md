# ADR-0009：预览后端契约（PreviewBackend）—— 选择 / 表面 / 自然尺寸收口

- 状态：已采纳
- 日期：2026-11-04
- 相关：`crates/qlr-app/src/app/backend.rs`、`docs/dev/ARCH.md`、`docs/dev/PLUGINS.md`

## 背景

「这个文件由谁来画」以前散在 `show.rs` / `render.rs` / `shell.rs` 三处，各自重新枚举
`Kind`（`qlr_render::viewer_for`、`plugins::host().find_for_path`、`is_archive`、
`shellpreview::has_handler`）。每加一个后端（office 轻型、html 改进、插件、未来的 sidecar）
都要在四五个文件里各补一个分支 —— 这正是"加格式 = 改平台"的来源（见 ARCH §1 的总纲）。

§7 的后端契约第一刀**只收口三件事**：**选择 / 表面 / 自然尺寸**。不引入 sidecar、不改渲染实现、
不改窗口尺寸算式、不动音视频启停，因此与改造前**行为等价**。

## 决策

在 `qlr-app` 内新增 `crates/qlr-app/src/app/backend.rs`：

- `trait PreviewBackend`：`id` / `priority` / `can_handle(path, kind)` / `surface(kind)` / `natural_size(path, kind)`。
- 四个零大小实现：`NativeBackend`（自研 `qlr_render::Viewer`）· `ArchiveBackend`（压缩包目录树）·
  `ShellBackend`（系统 `IPreviewHandler`，规则等同旧 `shell_candidate`）· `PluginBackend`（进程内 C ABI 插件）。
- `resolve(path, kind) -> Option<&'static dyn PreviewBackend>`：**唯一入口**，取首个 `can_handle`。
- `show.rs` / `render.rs` / `shell.rs` 一律走 `resolve`，不再各自枚举 `Kind`；`shell.rs::shell_candidate`
  删除，规则并入 `ShellBackend`。
- **压缩包按扩展名优先**：`resolve` 第一步 `if is_archive(path)` 直接返回 `ArchiveBackend`，即使内容被
  嗅探成文本（例如一个假的 `.zip` 里只有纯文本），也仍走目录列表 / "解析失败"提示，不退化成文本预览。
  这与旧逻辑里 `archive` 分支先于渲染分支一致；其余顺序为 native > shell > plugin。

## 后果

- 新增后端（office 轻型、html 改进、sidecar）只改 `backend.rs` 一处，`show`/`render`/`shell` 不动。
- `scripts/verify-layers.ps1` 新增 §2c「app 只在 backend.rs 里枚举 `viewer_for`」，现 **13 PASS · 0 FAIL**。
- `Surface`（Bitmap / Text / ChildWindow / List）目前只做分类；`repaint` 的绘制通道**暂未**据它重构（后续刀）。
- 无行为变化：`cargo test` **312 passed / 0 failed**（含 backend 5 个单测）；`smoke.ps1` **16 PASS · 1 SKIP**；
  `verify-archive` 11 PASS、`verify-media` 15 PASS、`verify-office` 23 PASS、`verify-mediashape` 13 PASS。

## 备选与否决

- **只在 `show.rs` 加一层 `match`（不抽 trait）**：不解决 `render.rs` / `shell.rs` 的重复枚举，否决。
- **一步到位引入进程外 sidecar 宿主**：§7 明确本刀不做（PDF 留待后续单客户 sidecar），否决。
