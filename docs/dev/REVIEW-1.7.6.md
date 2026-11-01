# QLRust 1.7.6 结构 / 对标 / 发布审计

> 2026-11-01 · 面向"是否上传 GitHub"的一次全面体检。
> 三份独立审计（结构健康度、与原版 QuickLook 对标、发布就绪度）+ Lead 复核的关键数字。
> 所有结论都带 `文件:行号` 或命令证据；未确认的会标"未确认"。

---

## 0. 一句话结论

**工程底子是好的**（分层干净、零死代码、CI 齐全、可观测性强、性能对原版是数量级领先），
**但仓库现在不能直接推公开仓**：`dist/` 把 678 MB 历史产物提交进了 git、根目录没有 `LICENSE`
（而 `Cargo.toml` 已声明 GPL-3.0-or-later）、15 个文件写死了本机绝对路径、README 版本号还停在
1.4.2。这些是"发布卫生"问题，不是产品问题——**清一遍大约半天**。
功能的真实差距只剩"表示层广度"（视频/字体/SVG/多语言/插件安装），不是骨架问题。

---

## 1. 项目结构

### 1.1 健康的部分（不要动）

| 项 | 证据 |
| --- | --- |
| 7 个 crate 全部 `version.workspace` | `Cargo.toml:13` |
| `qlr-core` 确实零 `windows` 依赖（唯一可跨平台测试的层） | `crates/qlr-core/Cargo.toml:11-18` |
| `qlr-platform` 只做 FFI（30 个 `pub fn`，无业务判断） | `crates/qlr-platform/src/*.rs` |
| 全仓死代码近乎为零：0 个 `TODO/FIXME`、0 个 `#[allow(dead_code)]`、0 个 `if false` | grep 全仓 |
| 插件三 crate 不合并是对的（契约零依赖 `qlr-plugin-api` 185 行 `repr(C)`、demo 是 ABI 最小样例、office 独立 `cdylib`） | `crates/qlr-plugin-api/src/lib.rs` |
| 绘制与命中测试同源（`ChromeRects` 一处算，两处用） | `crates/qlr-platform/src/window.rs:1706` |

`qlr-core` 依赖 `qlr-plugin-api` 看着像"多一条箭头"，但有明文理由（`libloading` 放 core），
**不要为了图好看再加一层抽象**。

### 1.2 必须修的结构债

1. **`crates/qlr-app/src/app.rs` 单文件 5799 行**，`repaint()`（`app.rs:4778`）一个函数里塞了 4 条互斥绘制通道，
   `window::Chrome` 字面量被复制 4 份（`app.rs:4759`、`4793`、`4826`、`4867`），
   hover 提示 `match` 也复制 4 份（`app.rs:4727-4737`、`4787-4791`、`4820-4824`、`4849-4860`）。
   → 抽 `State::chrome(&self) -> window::Chrome` + `fn chrome_hint(button, pinned) -> &str`，四处合一。
   这是**下一次改顶栏/材质一定会踩**的坑。
2. **顶栏按钮命中优先级写了两遍**（`app.rs:4189-4212` 与 `app.rs:4292-4301`），且这块零测试。
   → 提纯成 `window::ChromeButton::hit(rects, x, y)` + 单测。
3. **`log + beep + hide_preview` 抄两遍**（`app.rs:3446-3460`、`app.rs:3493-3502`）→ `fn reject_preview(reason)`。
4. **图片/PDF 通道没有双缓冲**：`paint_bitmap_scaled`（`window.rs:2227`）、`paint_bitmap_at`（`window.rs:2325`）
   直接画窗口 DC，而 `paint_list`（`window.rs:1149`）/`paint_doc_with`（`window.rs:1295`）走 `PaintBuffer`。
   这是"拖动发虚/频闪"的同类隐患，把 `PaintBuffer` 收进统一绘制入口。

### 1.3 文档已经与代码脱节（会误导下一个人）

| 文档 | 问题 |
| --- | --- |
| `docs/dev/ARCH.md:347`、`crates/qlr-platform/src/win11.rs:6,11` | 仍指向 `crates/qlr-platform/src/acrylic.rs` —— **该文件 1.5.0 已删**（`win11.rs:4` 自述） |
| `docs/dev/STATUS.md:45` | 写"A1.11 设置界面 ⬜ 未开始"，与同文件 `:559-586`（已交付、实测 10 PASS）和 `crates/qlr-app/src/settings.rs`（632 行）直接矛盾 |
| `docs/adr/0008-plugin-api.md:19-21` | 只列 3 个导出符号，代码已是 5 个（`crates/qlr-plugin-api/src/lib.rs:133-147`）；`docs/dev/PLUGINS.md:146` 还答"插件不能接管文本" |
| `docs/adr/0001-…:23` | 写"进程外协议"，但 ADR-0008 已定"进程内 C ABI"，需要标注"已被取代" |
| `docs/dev/compare-1.3.0-vs-quicklook.md` | 停在 1.3.0（Office/压缩包都还写"无"），已被 1.4.1/1.7.3 追平 |

### 1.4 值得做但不急

- `app.rs:4323-4776`（压缩包列表，约 450 行）边界最干净、无反向依赖 → 可整块搬成 `archive_view.rs`。
- `app.rs:3352`（`show_preview` 301 行）先抽纯函数 `plan_window()`，其余留原处。
- 三处"该纯化但没纯化"的逻辑：`apply_scroll_y`（`app.rs:4261`）、`page_edit_key`（`app.rs:4143`）、
  `step_slide`（`app.rs:1585`）——对照已有单测的 `content_size_of`(2568)/`edges_at`(2479)/
  `archive_row_index`(4591)，这三处是"白捡"的测试点。
- `crates/qlr-render/Cargo.toml:10` 的 `libloading = "0.8"` 是字面量，其余全 `workspace`。
- `wndproc`（`app.rs:5439`，20 个分支）与 `run()`（`app.rs:375-870`）零测试——不比拆文件优先，
  但要清楚这块没有回归网。

---

## 2. 与原版 QuickLook 的对比（1.7.6 vs 原版 4.5.0 / 本地 master）

原版：本地检出 `_src/QuickLook`（不入库；C#/WPF，`net462`），
线上最新正式版 **4.5.0**（2026-04-22）；本地这份是 `latest` 标签、`CHANGELOG.md:5` 里 4.6.0 尚未发布。

| 能力项 | 原版 QuickLook | QLRust 1.7.6 | 方向 |
| --- | --- | --- | --- |
| 冷启动首帧 | 1.0–1.5 s | 外测 ~100 ms、壳层 4–5 ms | **领先 10×+** |
| 常驻内存 | 空壳 95 MB，48MP 峰值 739 MB | 7.6–24 MB | **领先** |
| 分发体积 | 278 MB / 122 MB | 整包 ~6 MB，exe 1.93 MB | **领先 20–30×** |
| 格式广度 | 25 个插件工程 / 416 个扩展名 / 29 种语言 | 4 类内置（约 123 个扩展名）+ 1 个 office 插件 / 仅中文 | **落后** |
| 视频/音频 | VideoViewer（LAVFilters + WPFMediaKit + MediaInfo） | 只有占位色卡 demo | **落后（感知最强）** |
| Office | OfficeViewer + 系统预览处理器 + Native | 系统 `IPreviewHandler`；没装 Office 时退插件大纲卡片 | 略落后（有条件持平） |
| 压缩包 | 目录树 + 信息面板 | 列表视图（折叠、三列、滚轮、11 项端到端） | 基本持平 |
| PDF | 翻页 + 缩放 | 页码框跳转、页缓存、预取、超采样、放大镜 | **领先** |
| Markdown/文本 | 浏览器内核 + 高亮主题 | 自绘目录 + 行号 + 仿 VSCode 高亮 + 折行 | **领先** |
| 设置 | **无 GUI**，29 个 XML 开关 | 有 GUI 设置页，但只有 4 组开关 | 界面领先、项数落后 |
| 字体/SVG/邮件/HTML/CHM/PE-ELF/DB/3D/证书 | 全有 | 全无（定位为"交插件"） | 落后 |
| 稳定性/可观测性 | 无公开证据 | 长稳压测、3 s 超时取消、minidump、万例 fuzz | **领先** |
| 分发/更新 | Store / Installer / Scoop / 自动更新 | `install.ps1` + zip，无自更新 | 落后 |
| 插件安装 | `.qlplugin` 双击安装 | 手动拷 DLL 到 `plugins\` | 落后 |

### 2.1 值得补的（按用户感知度排序）

1. **视频/音频首帧**（现在完全不可用，原版靠 DirectShow）——唯一"打开就觉得没法用"的品类。
2. **英文 UI + 英文 README**——上 GitHub 的第一道门槛（29:1 的语言比）。
3. **字体 `.ttf/.otf`、SVG**——日常最容易撞上的两个扩展名，成本低。
4. **设置项补齐**（关窗失焦、自动重载、F5、字体字号、主题、扩展名黑白名单）——原版 `OPTIONS.md` 有现成语义。
5. **自动更新 + winget/Scoop**——现在升级靠重装。
6. **插件"下载即装"**——现在是手拷 DLL。

### 2.2 不该照搬

- WPF 可视树常驻（换热 16–31 ms，代价 170–740 MB）；
- `.NET` 插件 ABI（必须拖 CLR/WPF）；
- LAVFilters + DirectShow 视频链（GPL 组合 + 40 MB 原生依赖 + COM 注册）；
- HelixToolkit 3D（绑 WPF 渲染栈）。

### 2.3 许可与命名（发布前必须定）

- 原版是 **GPL-3.0**（上游 `LICENSE-GPL.txt:2` "Version 3, 29 June 2007"）。
- QLRust 已在 `Cargo.toml:17`、`crates/qlr-app/resources/qlrust.rc:25`、`README.md:146`
  声明 **GPL-3.0-or-later**，方向正确且与上游兼容（GPLv3 §14），**缺的只是 `LICENSE` 正文**。
- 定性：**干净重写但有行为参考**。证据：仓库里 0 个 `.cs`；图标是自绘（`assets/qlrust.ico`
  与上游 `QLPlugin.ico` 哈希不同）；但注释直接引用上游实现作为行为依据
  （`crates/qlr-app/src/cli.rs:3` → `App.xaml.cs:148-159`、`crates/qlr-core/src/paths.rs:3` →
  `SettingHelper.cs:63-69`），并沿用了上游常量（`crates/qlr-platform/src/hotkey.rs:202` 的 `QLTR`、
  `crates/qlr-core/src/config.rs:108` 的 750 ms）。按衍生作品处理最稳。
- 建议：**继续 GPL-3.0-or-later**；README 首屏加"非官方、与 QL-Win / QuickLook 无关联、未获背书"；
  不要用 "QuickLook for Rust / QuickLook-RS" 这类易混名（`QLRust` 可保留）。

---

## 3. 发布到 GitHub：阻断项与建议

### 3.1 P0 阻断（必须做，否则不要公开）

1. **`dist/` 整个目录进了 git**：`git ls-files` 里 354 个文件、**678 MB**（52 个版本目录、
   46 份 7.24 MB 的 `pdfium.dll`、50 个 zip）。仓库 `.git` 已 141 MB。
   → `git rm -r --cached dist` + `.gitignore` 加 `dist/`；**历史必须用 `git filter-repo --path dist --invert-paths` 清掉**，
   产物改放 GitHub Releases。
2. **没有 `LICENSE` 文件**：`Cargo.toml:17` 已声明 GPL-3.0-or-later，但仓库里找不到正文
   （违反 GPL-3.0 §4/§5 的分发义务，GitHub 也不会显示 license 徽章）。
   → 复制上游 `LICENSE-GPL.txt` 为根目录 `LICENSE`。
3. **15 个文件写死本机绝对路径**（形如 `C:\Users\<用户名>\…`、`D:\<工具链目录>\…`）：
   `scripts/stress.ps1:12-13`、`scripts/verify-office.ps1:20,26`、`scripts/verify-pdf.ps1:9`、
   `scripts/bench-results.json:3,5`、`scripts/sample-matrix.json:3-4`、
   `docs/dev/HANDOFF.md:14,21,261`、`docs/dev/CONVENTIONS.md:275,408`、`docs/dev/LOG.md`（11 处）、`docs/dev/STATUS.md:990`。
   → 脚本默认值改 `$PSScriptRoot`/相对路径；两个 json 是生成物，删掉并 ignore。
4. **版本号对不上**：`Cargo.toml:14`=1.7.6 ✅、`Cargo.lock` 七个 crate 一致 ✅、`STATUS.md:3`=1.7.6 ✅；
   但 `README.md:5` 与 `:130` 是 **1.4.2**，`crates/qlr-app/resources/qlrust.rc:12-13,23,28` 是 **1.7.2.0**
   （这个会写进 exe 属性）。
5. **没有 `.gitattributes`**：`git ls-files --eol` 显示 45 个 `w/mixed`、52 个 `w/crlf`、93 个 `w/lf`。
   → `* text=auto`、`*.ps1 *.md *.txt eol=crlf`、`*.rs *.toml eol=lf`、`*.dll *.exe *.zip *.png binary`。

### 3.2 P1 建议

- 补 `NOTICE`/`ATTRIBUTION`：上游行为参考 + `pdfium`（BSD-3-Clause，需要附全文 `LICENSES/pdfium-BSD-3-Clause.txt`）+ Rust 依赖清单。
- 根目录 `SECURITY.md`（现在的 `docs/dev/SECURITY.md` 是对内安全模型，没有联系渠道与披露时限）。
- `CONTRIBUTING.md`、`CODE_OF_CONDUCT.md`、`.github/ISSUE_TEMPLATE/{bug,feature}.yml`、PR 模板。
- `.github/workflows/release.yml`：`on: push: tags: ['v*']` → windows-latest → build → zip → 上传 Release + SHA256SUMS。
- README 面向陌生人：补构建步骤（`cargo build --release`，现在只有 fmt/clippy/test）、系统要求段、
  4–6 张截图（`docs/screenshots/` 已有 10 张 558 KB）、致谢；`README.md:96` 引用的
  `docs/bench-compare.md` 是失效链接。
- `.gitignore` 只有 5 行，补 `dist/`、`crashes/`、`.vs/`、`*.user`、`__pycache__`、`*.zip`、`*.tmp`。
- 版本号写进 rc 文件的那处建议由 `build.rs` 从 `CARGO_PKG_VERSION` 生成，避免以后再漂。

### 3.3 已经做得好的（公开仓不丢人）

- CI 齐：`.github/workflows/ci.yml` 里 `cargo fmt --all -- --check`、`clippy --all-targets -D warnings`、
  `cargo test --all`、`cargo build --release` + 5 MB 体积预算；另有 `bench.yml`、`fuzz.yml`（windows-latest）。
- 无凭据泄漏：邮箱 0 命中、`AKIA` 0 命中、`BEGIN PRIVATE KEY` 0 命中（`token` 命中全是 `OpenProcessToken` 之类误报）。
- 依赖许可健康：serde/serde_json/image/png/jpeg-decoder/libloading/windows 全是 MIT / Apache-2.0 / ISC。
- 文档体量惊人：8 篇 ADR、ARCH/LOG/STATUS/CONVENTIONS/HANDOFF/PLUGINS/RELEASE/SECURITY + 10 张截图。

---

## 4. 建议的下一步（两条路，选一条）

**路线 A：先发布（推荐）**
1. 清 `dist/`（含历史重写）；2. 补 `LICENSE` + `NOTICE` + `.gitattributes`；3. 修 15 处绝对路径；
4. 版本对齐 1.7.6（README、rc）；5. README 补构建/系统要求/截图/非官方声明；6. 加 release workflow；
7. 建仓 push、`git tag v1.7.6`、发 Release。
→ 工作量约半天，之后所有功能补强都能以"公开项目"的身份推进。

**路线 B：先补功能再发布**
把"视频/音频"或"字体/SVG"补上再公开，第一印象更强；但仓库卫生问题会一直悬着，
而且历史里那 678 MB 越拖越难清。

---

## 5. 附：本次审计用的三份独立报告

- 结构健康度：`app.rs` 绘制通道/命中/拒绝逻辑重复、文档与代码脱节、图片通道缺双缓冲（§1）
- 对标分析：只有"表示层广度"落后，性能与稳定性数量级领先（§2）
- 发布就绪度：`dist/` 678 MB / 46 份 pdfium、无 LICENSE、15 处绝对路径、版本漂移（§3）


