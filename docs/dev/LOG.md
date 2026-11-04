# 开发日志（LOG）

> 顺序排列（最新在文件末尾）。每条记：**做了什么 / 为什么 / 怎么验证的（含实测数字）**。
> 架构级取舍另外写进 `docs/adr/`，这里只留索引。

## 2026-10-29 · W1–W2：M1 壳层跑通

### 做了什么

新建 workspace `quicklook-rs`，三个 crate 分工（见 ADR-0002）：

- `qlr-core`（不依赖 windows）：`config`（JSON + 原子写 + 钳制）、`paths`（便携 `portable.lock` → `<exe>\UserData`，否则 `%LOCALAPPDATA%\QLRust`）、`log`（自研文件日志 + 4 MB 轮转 + 手写 UTC 换算）、`ipc`（`Request{action,path}` 编解码 + 窗口类名/互斥体/魔数常量）、`error`、`version`。
- `qlr-platform`：`window`（注册/创建/显示/绘制/消息泵）、`acrylic`（`SetWindowCompositionAttribute` 动态解析）、`tray`（托盘 + 右键菜单，自由函数不用 RAII）、`instance`（互斥体 + `WM_COPYDATA`）、`dpi`、`wide`。
- `qlr-app`：CLI 解析、`wndproc`、状态机（`STATE` + `LINES` 双锁）、托盘菜单动作。

`scripts/smoke.ps1`：端到端冒烟 8 项（冷启动 / 单实例转发 / 转发被处理 / 预览窗可见 / 托盘 / 干净退出）。

### 为什么这么切

- core 不碰 windows → 唯一能跨平台跑、能 `cargo test` 的层；FFI 全在 platform，业务判断全在 app。这条线定下来后，后面加解码器/渲染器时"该放哪"基本不用讨论。
- 双锁（`STATE` / `LINES`）不是设计洁癖，是被 `WM_PAINT` 的同步重入逼出来的（详见 ARCH §4）。

### 踩到的坑（最重要的产出）

**进程启动 6 ms 后无声崩溃**。日志停在 `SHOW target=… elapsed=6ms`，没有 `退出：总运行`，Rust 的 panic hook 也**没有**输出。

定位过程：

1. 加 `std::panic::set_hook` 写日志 → 仍然无输出 ⇒ 不是 Rust panic。
2. 查 Windows 事件日志：
   ```
   错误模块名称: user32.dll  异常代码: 0xc0000005 / 0xc000041d  错误偏移量: 0x1f16b
   ```
   ⇒ 访问违例发生在**我们的窗口过程**里（经 `DispatchMessageW` 进入）。
3. `wndproc` 开头加 `log_debug!("wm=0x{msg:04X}")` → 最后一条是 `wm=0x000F`(WM_PAINT)。
4. 在 `paint_text` 里逐步打点 → 崩在第 4 次 `DrawTextW`，那一行是**空字符串**。

根因：`windows` crate 的 `DrawTextW(hdc, &mut [u16], …)` 用 `slice.len()` 当 `cchText`。空切片 → `cchText = 0`，而 user32 用 `cchText == 0` 表示"自己去 `wcslen`"，于是它去读空 `Vec` 的悬垂指针 → 0xC0000005。

修法：`if buf.is_empty() { y += line_height; continue; }`（空行占高度、不画字）。已写进 `CONVENTIONS.md` 坑清单 #1，并标注同类 API（`ExtTextOutW`/`TextOutW`…）都有这个语义差。

**顺带修的两个问题**：

- 单实例"看起来没生效"：实际是首实例已经崩了、互斥体和宿主窗口都释放了，后面每个进程都成了主实例。修掉崩溃后自然正常。
- `scripts/smoke.ps1` 用 `Start-Process` 启动被测进程会让调用方挂住（子进程继承 stdio 管道），改成 WMI `Win32_Process::Create` 后正常。
- `Toggle` 语义修正：带路径的调用应当"换内容显示"而不是"再按一次就收起"，只有同一文件才收起（对齐上游空格键行为）。

### 验证（release，Win10 19045 / i5-12400F）

```
cargo clippy --all-targets -- -D warnings   → 0 警告
cargo test                                   → 25 passed（core 13 / platform 3 / app 9）
target\release\qlrust.exe                    → 282,624 B = 0.27 MB
scripts\smoke.ps1（8 项）                    → 7 PASS + 1 SKIP（托盘，提权环境 UIPI）
冷启动到壳层就绪                              → 56 ms
空闲常驻内存                                  → 7.6 MB working set / 1.6 MB private
```

对照 QuickLook 4.5.0 同机数据：冷启动 ~1240 ms、常驻 96 MB。

### 提交

`bfa4f99` feat: M1 壳层跑通（W1-W2）—— 36 个文件，含 `docs/dev/` 五件套与 4 份 ADR。

### 下一步

A1.5 热键钩子（`WH_KEYBOARD_LL`，独立线程 + 注入过滤 + 30 s watchdog）→ A1.6 选中项通道（Shell COM 四段 fallback）→ M1 Gate（选文件按空格出窗）。详见 `STATUS.md` §6。

---

## 2026-10-29 · A1.5 + A1.6：热键钩子与选中项通道

### 做了什么

1. **`crates/qlr-platform/src/hotkey.rs`（新，377 行）**：`WH_KEYBOARD_LL` 装在独立线程，
   `MsgWaitForMultipleObjects(stop_event, 30_000, QS_ALLINPUT)` + `PeekMessageW` 泵；
   回调只判注入 / 判前台类名 / `PostMessageW`；命中即吞键。30 s 心跳 `Unhook`+`SetWindowsHookEx`
   自愈（系统摘钩无通知，这是唯一办法）。统计量 + `HotkeyGuard::drop` 保证退出必卸载。
2. **`crates/qlr-platform/src/selection.rs`（新，约 400 行）**：`CoCreateInstance(CLSID_ShellWindows)`
   → 逐条 `Item(i) → IServiceProvider → QueryService(SID_SShellBrowser) → IOleWindow::GetWindow()`
   → `QueryActiveShellView → IFolderView2::GetSelection`；两级匹配（精确 / 同根 + 可见优先）。
3. **接线**：`ipc::WM_QLR_HOTKEY`、`Action::HotkeyProbe`、`Command::ProbeSelection(Hwnd)`、
   `config.hotkey_enabled`、`app::on_hotkey()`、启动时 `CoInitializeEx` / 退出时 `CoUninitialize`。
4. **测试**：`scripts/smoke.ps1` 加 4 项（热键安装 / 热键处理链 / 选中项探测 / 钩子卸载 + 统计）；
   新建 `scripts/probe-selection.ps1`（真实 explorer 选区验证）。
5. **文档**：ADR-0005（热键）、ADR-0006（选中项）；`ARCH.md` §2 线程模型 + §5.2/§5.3；
   `CONVENTIONS.md` 坑 #9–#12；`STATUS.md` 全量刷新。

### 为什么这么切

热键和选中项是"选文件按空格出窗"这条主链的两半，分开做只能各自对着假数据测。
一起做才能验证真实的 `空格 → 前台类名白名单命中 → 查选区 → 拿路径 → toggle` 全链。

### 踩到的坑（本轮最重要的产出）

1. **工作目录里的 exe 拿不到 Shell COM**（`0x80070005`）。用最小复现程序比对进程令牌才定位：
   DSH 沙箱给工作目录内的可执行文件降了完整性级别（`il_rid=4096` Medium vs `12288` High），
   跨进程 COM 激活被拒。**同一个 exe 复制到 `D:\` 就好**。这不是代码问题，是环境。
2. **`FindWindowSW` 完全不可用**：传任何 frame HWND 都解析成桌面那条（`resolved=0xC08D8`），
   拿回桌面选区后 `GetDisplayName` 报 `E_INVALIDARG`。必须逐条枚举 + `GetAncestor(GA_ROOT)` 同根匹配。
   —— 这是"选中项永远为空"的真正原因，之前一直以为是权限问题而走错方向。
3. **注册项的 HWND 是 `ShellTabWindowClass` 不是 frame**。`CabinetWClass` 是它的 root。
4. **`--probe-selection` 用前台窗口，而自动化里前台是宿主终端** → 必然 `source=none`。
   要么先 `SetForegroundWindow` 并回读校验，要么用 `--probe-selection-hwnd` 钉死窗口。
5. **忘了把新构建复制到 `D:\qlrust-bin`**，拿旧二进制测新代码，症状是"某些 debug 行就是不出现"，
   白折腾一轮，误判成 `QLRUST_LOG` 不生效（其实一直是好的）。教训进了 CONVENTIONS 坑 #10。
6. **windows 0.62 语义**：`WH_KEYBOARD_LL`/`QS_ALLINPUT` 本身就是目标类型，不要再包一层；
   `GetTickCount` 在 `System::SystemInformation`；`IShellItemArray::GetCount()` 返回 `u32` 不是 `i32`；
   `IFolderView` 没有 `GetSelection`，在 `IFolderView2` 上。

### 验证（release，Win10 19045 / i5-12400F）

```
cargo clippy --all-targets -- -D warnings  → 0 警告
cargo test                                  → 32 passed（core 14 / platform 8 / app 10）
cargo build --release                       → 345,600 B = 0.33 MB
scripts/smoke.ps1                           → 12 PASS + 1 SKIP（托盘 UIPI）
  冷启动 87–91 ms / 第二实例退出 48 ms / 热键已安装 / 热键处理链可用 /
  选中项探测可运行 / 退出 41 ms / 钩子已卸载 / 热键统计已记录
scripts/probe-selection.ps1                 → PASS
  PROBE-SELECTION source=tab count=1 first=...\spike\fixtures\8mp_3840x2160.jpg
空闲 8.2 MB working set / 1.7 MB private
```

### 提交

`5b02d9b` feat(hotkey,selection): 全局热键 + Shell 选中项通道（A1.5 / A1.6）
（19 个文件：2 个新模块 + 2 个 ADR + 1 个验证脚本 + 13 份改动）

### 下一步

人工确认 M1 Gate（选文件按空格出窗 —— 自动化按不出真空格，钩子会过滤 `SendInput`），
然后进 M2 格式管线。详见 `STATUS.md` §6。


## 2026-10-29 · M2 起头：格式注册表（A2.0），随后按用户要求暂停

### 做了什么

`crates/qlr-core/src/format.rs`（369 行，新增）：把上游 25 个插件 `CanHandle` 里的扩展名字面量
全量抽出（370 条），按各插件声明的 `Priority` 裁决冲突，再对少量"扩展名重叠但直觉明确"的
做人工 override，得到 **308 个扩展名 → 25 个 `Kind`** 的静态表。

- `Kind`：Image / Media / Pdf / Text / Markdown / Csv / Html / Office / Archive / PE / Elf / Dump /
  Cert / Font / Database / Mesh / Mail / Binary / Chm / Prefetch / Thumbnail / Package /
  PluginInstaller / Clsid / Unknown。每个带 `as_str()`（日志匹配用，改动会破坏脚本）、
  `is_rendered()`（M2 已能真渲染的四类）、`roadmap()`（占位时期告诉用户哪个里程碑做）。
- 查找：`kind_for_ext` / `kind_for_path`，小写化用栈上 `[u8; 16]` 避免分配；不做内容嗅探
  （QuickLook 的视频识别靠 MediaInfo 读容器、图片靠解码回退，那属于渲染器的活，M4 再补）。
- 6 条单测：常见类型、大小写/未知/超长、路径、**同一扩展名不得登记两次**、总数下限、M2 类别断言。

### 为什么先做这个

后续渲染器要按类别路由，而"哪些扩展名归谁"一旦散落在渲染器里就会变成 25 处 if-else。
把它做成一张纯数据表 + 一条"不得重复登记"的单测，等价于把上游的 `Priority` 语义固化下来，
以后加格式只改一列数据。

### 验证

```
cargo fmt --check        → 通过
cargo clippy -D warnings → 0 警告
cargo test               → 38 passed（core 20 / platform 8 / app 10）
```

### 状态

按用户要求**暂停**。仓库可安全中断：未完成部分只在新文件里，主路径没有半截改动；
`STATUS.md` §3.1 已列出 M2 的 A2.1–A2.10 任务与验收线，恢复时从 A2.1 接着做即可。

## 2026-10-30 · M2 渲染管线接通：图片真的上屏了（A2.1/A2.2/A2.4/A2.5/A2.7）

### 做了什么

新建 crate **`qlr-render`**（不依赖 `windows`，可跨平台单测），把"文件 → 能上屏的东西"这一层
从平台层剥出来：

- `canvas.rs`：`Bitmap{width,height,bgra}`（手写 `Debug`，避免断言失败时打印几百 MB 像素）、
  `TextDoc{lines,total_lines,truncated,encoding}`、`Canvas::{Bitmap,Text}`。
- `image_view.rs`：`image` 0.25（bmp/gif/ico/jpeg/png/pnm/qoi/tga/tiff/webp）+ `thumbnail` 盒式缩放
  + `Bitmap::from_rgba` 就地交换 R/B 得到 BGRA；`fit_within` 保证等比且不放大。
- `text.rs`：BOM/UTF-8/UTF-16 探测、8 MB 读入上限、2000 行上限、单行 2000 字符上限、
  NUL 字节判二进制。**两条红线是有意的**：QuickLook 在这两处都翻过车（滚屏吃 6.6 GB、
  单行 JSON 撑爆 GDI）。
- `lib.rs`：`Viewer` trait（`kind` / `render`）+ `render_file(path,w,h)` 按格式注册表路由；
  已实现的族是 Image / Text / Markdown / Csv，其余返回带里程碑的 `Error::Message`。

平台层加两件上屏能力（`crates/qlr-platform/src/window.rs`）：
`paint_bitmap`（`StretchDIBits` + top-down `BITMAPINFO`（`biHeight` 取负）+ `HALFTONE` +
`SetBrushOrgEx(0,0)`，否则缩放出条纹）、`invalidate`、`post`。

应用层（`crates/qlr-app/src/app.rs`）加**常驻渲染线程**（ADR-0007）：
`RenderJob{generation,path,max_width,max_height}` 经 `mpsc` 投给 worker；
worker 渲染完把 `(generation, Result<Canvas>)` 放进 `RESULT` 槽，再
`PostMessageW(host, WM_QLR_RENDER)`；UI 线程在 `on_render_done` 里校验代际号，
不匹配就丢弃（连按空格翻文件时上一张的结果不能盖掉当前这张）。
`repaint` 优先画 `CONTENT`，没有才回落到 `LINES` 占位文字。

### 为什么这么切

UI 线程直接解码会把 48MP 的 175 ms 直接变成"按空格后卡住"。而把渲染放 `qlr-platform`
又会让平台层依赖 `image`，破坏"平台层只做 FFI"的分层。详见 ADR-0007。

### 踩到的坑（都已进 CONVENTIONS）

- `InvalidateRect` 在 `Win32::Graphics::Gdi`，**不在** `WindowsAndMessaging`（编译期发现）。
- `std::env::temp_dir()` 在本机**不可写**（`Os { code: 5, PermissionDenied }`）——
  单元测试里写临时文件一律走 `crates/qlr-render/src/testutil.rs`（落到 `target/test-tmp/`）。
- `smoke.ps1` 的夹具路径原来只找仓库内 `spike\fixtures\`，本仓库夹具在**上级目录**，
  于是退化成"用脚本自己当夹具"→ 渲染断言永远失败。已改成"仓库内 → 上级 spike → 现场生成"。
- 行尾换行会把行数算多一行（`"a\n"` 应是 1 行），已按"结尾换行则 pop 掉空尾行"修正。

### 验证（release，Win10 19045 / i5-12400F）

```
cargo fmt --check                              通过
cargo clippy --all-targets -- -D warnings     0 警告
cargo test                                     57 passed（core 20 / platform 9 / render 18 / app 10）
scripts/smoke.ps1                              14 PASS + 1 SKIP（托盘 UIPI）
  ├ 冷启动 71 ms
  ├ 图片渲染完成  RENDER gen=1 bitmap bytes=3181764 elapsed=54ms
  └ 渲染无失败
scripts/probe-selection.ps1                    PASS（source=tab count=1）
```

指标变化：

| 指标 | M1 | 现在 | 备注 |
|---|---|---|---|
| 分发体积 | 0.33 MB | **1.39 MB** | image 解码器 +1.1 MB，仍在 5 MB 预算内 |
| 空闲内存 | 8.2 MB | 8.3 MB | |
| 4K 预览后内存 | — | **11.8 MB** | 含 3 MB 位图 |
| 4K 渲染耗时 | — | **53–54 ms** | 解码+缩放到 1190×748 |
| 单测 | 38 | **57** | |

对照 QuickLook 4.5.0：4K 热路径 421 ms / 峰值 225 MB → 现在 53 ms / 11.8 MB。

### 下一步

先补基准（A2.9/A2.10），再动 A2.3 缩放解码（48MP 现在峰值约 192 MB，是当前最大风险），
然后 PDF（A2.6）与交互（A2.8）。详见 `STATUS.md` §6。

## 2026-10-30 · M2 基准落地（A2.9/A2.10）：第一次拿到三项真实数字

### 做了什么

`scripts/bench.ps1`（新增，约 190 行）：对 4K / 48MP / 26.6 MB 文本三个夹具，逐夹具重启常驻实例，
投递预览请求，**边等 RENDER 边采样内存**（解码峰值只存在于渲染过程中，事后读 `Get-Process`
只能看到稳态），输出表格并写 `scripts/bench-results.json`。
`docs/dev/bench.md`（新增）：口径说明 + 当前数字 + 与 QuickLook 4.5.0 的同机对拍 + "还没量到的"。

### 数字（release，Win10 19045 / i5-12400F）

| 场景 | RenderMs | 感知延迟 | 峰值 RSS | 稳态 RSS |
|---|---|---|---|---|
| 4K JPEG | 57–62 | **67–86 ms** | 34.2 MB | 11.8 MB |
| 48MP JPEG | 171–175 | **190–192 ms** | **148–150 MB** | 12.3 MB |
| 26.6 MB 文本 | 20–23 | **34–35 ms** | 11.6 MB | 11.6 MB |

对 QuickLook 4.5.0（同机）：4K 421 ms → 快 5–6×、峰值 225 MB → 少 6.6×；
48MP 418 ms → 快 2.2×、峰值 404 MB → 少 2.7×；文本 923 ms → 快 27×、峰值 224 MB → 少 19×。

### 结论

- 高频路径（4K、普通文本）已经进到 <100 ms 的"感觉不到"区间，继续压毫秒没意义。
- **48MP 是唯一没达标的**：感知 190 ms 可接受，但峰值 150 MB 远超 A8.1 的 ≤60 MB。
  根因明确：`image` 先解整张 RGBA（8000×6000×4≈192 MB）再缩放 → **A2.3 缩放解码是下一个必做项**。
- 文本的 27× 不是我们多快，而是上游在文本上做了很蠢的事（整文件读入 + 全文排版），
  这正好解释了它当年为什么会吃掉 6.6 GB。

### 踩到的坑

- 第一版脚本只记稳态 RSS，48MP 显示 12.3 MB——**看起来很好，实际是错的**。
  峰值必须自己在渲染过程中采样，否则会得出"内存没问题"的错误结论。

### 下一步

A2.3：JPEG 走 DCT 域缩放（1/2、1/4、1/8）直接解到显示尺寸，目标峰值 ≤60 MB、首屏 ≤25 ms；
改完复跑 `bench.ps1` 贴前后对比。

## 2026-10-30 · A2.3 缩放解码：48MP 峰值 150 MB → 12.8 MB

### 做了什么

`crates/qlr-render/src/image_view.rs` 拆成两条路径：

- **JPEG → 缩放解码**：`jpeg-decoder` 先 `read_info()` 拿原始尺寸，算出目标尺寸，
  再 `decoder.scale(w as u16, h as u16)` 让解码器按 1/2^n 档直接解到目标附近，
  最后补一次盒式缩放。8000×6000 请 1190×748 → 走 1/8 档 → 直接解出 1000×750（3 MB），
  **中间从不产生 192 MB 的整图缓冲**。
- **其他格式 → 通用路径**：`image` 整图解码 + `thumbnail`，行为不变。
- JPEG 缩放路径遇到不支持的像素格式（CMYK32 等）返回 Err，外层退回通用路径，
  只记一条 debug。**优化不能变成少功能。**

依赖：`jpeg-decoder 0.3`，**关掉默认特性**（默认会拉 `rayon` 起线程池，既没必要也增大体积）。

### 数字（同一脚本、同一机器，前后对比）

| 场景 | RenderMs | 感知延迟 | 峰值 RSS |
|---|---|---|---|
| 4K JPEG | 57–62 → **32–34**（−45%） | 86 → **50–57** | 34.2 → **11.7 MB** |
| 48MP JPEG | 171–175 → **66–69**（−61%） | 190 → **81–98** | 148–150 → **12.8 MB**（**−92%**） |
| 26.6 MB 文本 | 22（不变） | 34–35 | 11.6 MB |

48MP 的 148→12.8 MB 是这一轮最大收益，**已经低于 A8.1 的 ≤60 MB 预算**。

对 QuickLook 4.5.0（同机）：4K 421 ms/225 MB、48MP 418 ms/404 MB、文本 923 ms/224 MB
→ 现在是 50–57/81–98/34 ms，峰值 11.7–24.8/12.8/11.6 MB。

exe 1.39 → **1.60 MB**（jpeg-decoder 约 +200 KB），仍在 5 MB 预算内。

### 验证

```
cargo fmt / clippy -D warnings / test 57 passed   全绿
scripts/bench.ps1                                  4K 50–57 ms / 48MP 81–98 ms / 文本 34 ms
```

### 下一步

A2.6 PDF（pdfium，dll 只在 worker 里延迟加载）→ A2.8 交互 → 其他格式的缩放解码。

## 2026-10-30 · 修用户实测反馈的两个 bug（0.1.1）

### 1. 图片被"提亮"——根因是亚克力，不是颜色转换

用户反馈："成功的图片，不是原来的了，亮度提的很高"。

隔离过程：造一张纯灰(128,128,128) + 红蓝标记块的 PNG，预览后屏幕取色：

| 采样点 | 期望 | 亚克力开 | 亚克力关 |
|---|---|---|---|
| 灰 | 128,128,128 | **194,185,177** | **128,128,128** |
| 红块 | 255,0,0 | 255,37,36 | **255,0,0** |
| 蓝块 | 0,0,255 | 35,34,255 | **0,0,255** |

结论：Win10 的 `SetWindowCompositionAttribute`（`ACCENT_ENABLE_ACRYLICBLURBEHIND`）
会让**整窗内容**变半透明，我们画的位图和窗口后面的壁纸混色 —— 所以"提亮"。
`Bitmap::from_rgba` 的 RGBA→BGRA 转换、`StretchDIBits` 的 top-down DIB 都是对的，
一条都不用改（这点很关键：别被表象带去改解码）。

修法：
- `app::apply_material(hwnd, acrylic_enabled, has_bitmap)`：**有位图就关亚克力**，
  只有文字/占位状态才开（模糊背景正是那个状态想要的）。
- `window::paint_bitmap` 先 `FillRect` 一层 `BITMAP_BACKGROUND_RGB = 0x001A1A1A`。
  窗口类背景刷本来是空的（为了让文字状态透出模糊），关掉亚克力后不自己刷底
  就会露出桌面 —— 实测留白处是壁纸颜色（138,95,66），刷完是 (26,26,26)。

修复后逐像素校验：灰 128,128,128 / 红 255,0,0 / 蓝 0,0,255 / 留白 26,26,26。

### 2. 没有选中项时按空格"完全没反应"

日志（用户机器）：`HOTKEY vk=0x20 source=none count=0` 连打二十来次，
中间夹着一次 `source=tab count=1 first=...Pictures\xxx.jpg`（那次成功了）。
说明钩子在工作、键也被吞了，只是选区解析不出来。

修法：
- **重放按键**：`hotkey::replay(vk)` 用 `SendInput`（带 `REPLAY_MARKER=0x514C_5452`）
  把空格还给系统。不自激的原因：钩子对注入键（`LLKHF_INJECTED`）一律 `CallNextHookEx`。
  规矩从此固定：**吞了键就必须有交代**，什么都没显示就还回去。
- 选区解析两处加固：① 桌面特殊 —— 前台可能是 `Progman` 而 shell window 挂在 `WorkerW`
  上，改为"两边都是桌面类就算同根"；② `SIGDN_FILESYSPATH` 取不到时用
  `IFolderView::GetFolder` 的路径 + `SIGDN_PARENTRELATIVEPARSING` 拼，拼出来
  还要 `path.exists()` 才认。
- `HOTKEY` 日志加上 `fg=<前台窗口类名>`，下次用户报"没反应"能一眼看出是哪个窗口。

### 验证

```
cargo fmt / clippy --all-targets -D warnings     0 警告
cargo test                                       57 passed（app 10 / core 20 / platform 9 / render 18）
scripts/smoke.ps1                                15 PASS + 1 SKIP（托盘 UIPI）
release exe                                      1,603,072 B（1.60 MB，预算 5 MB）
```

### 教训（已进 CONVENTIONS 待补条目）

- 表现是"颜色不对"时，先分清**解码错**还是**合成错**：拿一张程序生成的纯色图，
  量屏幕像素，比读代码快十倍。
- 未文档化的合成开关（亚克力）会影响**全部**内容，别假设"它只影响背景"。


## 2026-10-30 · 0.1.2 启动语义修正：后台静默 + 可见入口

### 用户反馈

> "首先，打开程序的时候，只应该出现在后台，前台不要留窗口，
> 其次，现在打开软件，任务栏已经不会显示了"

两件事：① 双击启动弹了个空窗口并占前台 —— 这是 bug
（用户日志里是 `SHOW target=(无) kind=unknown renderable=false`）；② 窗口不进任务栏
（`WS_EX_TOOLWINDOW` 的设计）导致"它起没起来 / 怎么退"找不到入口。

### 改了什么

- **裸启动不再出窗口**（`crates/qlr-app/src/app.rs`）：
  `let startup_shows = request.path.is_some() || matches!(request.action, Action::Show);`
  只有"用户明确要内容"才显示；否则只记一行
  `启动后保持后台：不显示窗口（选中文件按空格预览，托盘图标右键退出）`。
- **裸 Toggle 只允许收起**：`toggle()` 的 `None` 分支改成
  `if visible { hide_preview(); } else { log_debug!("忽略（保持后台）") }`。
  原实现"没有内容就显示空窗"正是双击启动抢前台的来源。
- **可见入口**（`crates/qlr-platform/src/tray.rs` 新增 `notify(hwnd,id,title,msg)->bool`）：
  启动后弹托盘气泡"已在后台运行…"（`NIF_INFO` + `NIIF_INFO` + `NIM_MODIFY`，
  复用 `NOTIFYICONDATAW` 的 `szInfoTitle[64]`/`szInfo[256]`）。
- **托盘双击 = 开合说明窗口**（`on_tray` 新增 `WM_LBUTTONDBLCLK` 分支；
  注意 `WM_LBUTTONDBLCLK` 不在已有的 import 列表里，加上后 clippy 才过）；
  右键菜单"预览"从 `toggle(None)` 改成显式 `if visible { hide } else { show }`
  —— 托盘点击是**显式意图**，允许出窗口，与裸启动不同。

### 为什么不用"进任务栏"来解决第 2 点

预览窗是 `WS_EX_TOOLWINDOW`：不进任务栏、不进 Alt-Tab，这是预览类工具的正确行为
（QuickLook 同样如此）。为"找得到"把它塞进任务栏，等于让一个不该被当作应用窗口的
浮层混进 Alt-Tab，切窗口时会被它打断。正确解法是托盘 + 气泡；Windows 10 默认把新图标
收进 `^` 溢出区，README 写明了怎么拖出来常驻。

### 验证

```
cargo fmt / clippy --all-targets -D warnings     0 警告
cargo test                                       57 passed
scripts/smoke.ps1                                15 PASS + 1 SKIP（托盘 UIPI）
release exe                                      1,604,608 B（1.60 MB，预算 5 MB）
裸启动（WMI 启动 D:\qlrust-try）                 PreviewWindow visible=False
                                                 HostWindow  visible=False  WS=7.6 MB
                                                 日志末尾 = "启动后保持后台…" + "已弹出后台运行提示"
```

### 版本

workspace version 0.1.0 → **0.1.2**（根 `Cargo.toml`），产物 `dist/qlrust-0.1.2-win64/`
（`qlrust.exe` + `portable.lock` + `README.txt`）与同名 zip。



## 2026-10-30 · 0.1.3 托盘图标认不出 + exe 三无身份

### 用户反馈

> 说错了，是托盘图标。里边找不到，运行的时候显示没有签名，托盘没图标，我以为没成功

拆成三件事：① 托盘里找不到 QLRust 的图标；② 运行时有"没有签名"的提示；③ 因此以为程序没起来。

### 做了什么

1. **托盘图标换成自定义图标**（`assets/qlrust-32.png`，32×32 深蓝圆角底 + 白放大镜）。
   原先用 `LoadIconW(None, IDI_APPLICATION)` —— 那是 Windows 通用程序图标，
   和几十个别的程序长得一模一样，用户即使在托盘里看见也认不出是自己的程序。
   图片以 `include_bytes!` 嵌进 exe，运行时用 `CreateIconFromResourceEx(..., 0x0003_0000)`
   转成 `HICON`（Vista+ 支持 PNG 图标资源），失败才退回 `IDI_APPLICATION`。

2. **给 exe 补身份**：新增 `crates/qlr-app/build.rs` + `resources/qlrust.rc` +
   `resources/qlrust.manifest`，把图标、`VERSIONINFO`（"QLRust - 按空格预览文件"、0.1.3.0、
   GPL-3.0-or-later）和 manifest 一起嵌进去。build.rs 去 `C:\Program Files (x86)\Windows Kits\10\bin`
   下找版本号最大的 x64 `rc.exe` 编 `.res`，再用 `cargo:rustc-link-arg-bins` 交给链接器；
   找不到就 `cargo:warning` 跳过（资源不是功能，缺了也能跑）。
   manifest 声明 `asInvoker`（**不请求提权**，否则 `Shell_NotifyIconW` 会被 UIPI 拒 0x80070005）、
   `PerMonitorV2`、Win10 `supportedOS`。

3. **启动日志打印托盘图标坐标**：`tray::rect()` 走 `Shell_NotifyIconGetRect`，
   成功记 `托盘图标已添加 rect=(l,t)-(r,b) 尺寸=WxH`，失败记
   `托盘图标已添加但 Shell 报不出位置（可能被折叠进溢出区，或图标未真正注册）`。

4. README 重写了两节：「托盘里找不到？九成是这两个原因」（溢出区 `^` / 提权启动）、
   「运行前提示"没有签名 / 未知发布者"」（没买代码签名证书，右键属性→解除锁定，或"更多信息→仍要运行"）。

### 关键教训：托盘坐标"偏了 100 px"是误判

第一次对比时看到 `GetRect` 报 `(1683,1040)` 而截屏里蓝色图标在 `x=1787`，据此断定为
"GetRect 不可信"。**这个结论是错的。** 真相是：

- `Stop-Process` 杀掉进程后，**它的托盘图标还留在任务栏上**（僵尸图标，鼠标划过托盘区才消失）；
- 我把上一轮进程的僵尸图标（在 `x=1787`）当成了新图标。

补了干净的对照实验（先杀干净 + `SetCursorPos` 来回扫托盘清僵尸 + 再截屏）：

```
全杀后（只剩僵尸）：  蓝色像素=213  x=1787..1802   ← 僵尸
再启动一个实例：      蓝色像素=413  x=1687..1702（新图标） + 1787..1802（僵尸）
```

新图标 `1687..1702` 与 `GetRect` 报的 `1683..1707` **完全吻合** ⇒ `Shell_NotifyIconGetRect`
是准的，已把 `tray.rs` 里那条错误注释改掉并写明误判原因。

配套的验证纪律：**验证托盘图标必须先"单实例 + 清僵尸"再截屏**，否则数字会骗人。

### 验证

```
cargo fmt --check / clippy --all-targets -D warnings   0 警告
cargo test                                            57 passed
scripts/smoke.ps1                                     15 PASS + 1 SKIP（托盘 UIPI）
release exe                                           1,612,800 B（1.61 MB，预算 5 MB）
                                                       比 0.1.2 +8,192 B（图标 3 KB + 版本信息 + manifest）
VERSIONINFO                                           FileDescription="QLRust - 按空格预览文件（Rust 版 QuickLook 重写）"
                                                       ProductName=QLRust  FileVersion=0.1.3.0
manifest                                              asInvoker / PerMonitorV2 / supportedOS(Win10) 均已嵌入
explorer 代启（medium IL）                            托盘图标已添加 rect=(1683,1040)-(1707,1080) 尺寸=24x40
                                                       壳层就绪 window_created=7ms tray=true
```

### 版本

workspace version 0.1.2 → **0.1.3**，产物 `dist/qlrust-0.1.3-win64/`
（`qlrust.exe` 1,612,800 B + `portable.lock` + `README.txt` 5,232 B）与 zip（805,835 B）。

### 下一步

1. 把 0.1.3 交给用户重测：托盘能不能找到（蓝色放大镜，可能在 `^` 里）、
   "没有签名"提示是否可按 README 步骤绕过。
2. 仍未验证：真实"桌面图标选中 → 空格"场景（该机桌面 0 张图片，需用户实际选中）。
3. M2 继续：A2.6 PDF（pdfium）→ A2.8 滚轮/翻页 → 其他格式的缩放解码。

## 2026-10-30 · 0.1.4 桌面也能按空格预览了（A1.6 收口）

### 用户反馈

> 目前桌面的图片按空格还是没有反应

### 怎么定位的

日志里一行就把范围缩死了：

```
HOTKEY vk=0x20 fg=WorkerW hook_to_handler=0ms source=none count=0   ← 连续 14 次
HOTKEY vk=0x20 fg=CabinetWClass ... source=tab count=1 first=…\Pictures\…jpg   ← 成功案例
```

钩子是好的（`hook_to_handler=0ms`，键被吞了），问题在 `source=none` —— **选不出来**。
区别只在 `fg=` 是 `WorkerW`/`Progman`（桌面）还是 `CabinetWClass`（资源管理器窗口）。

用独立诊断程序（`comtest`，产物必须复制出工作目录才能跑，见坑 #9）把两条路径都打了表：

- 资源管理器窗口：`IShellWindows` 里有它的 `ShellTabWindowClass` 注册项，能查到；
- 桌面：`IShellWindows count=3`，**三条全是已打开的文件夹窗口，根本没有"桌面"那一条**
  ⇒ 按 HWND 匹配的 `query_by_hwnd` 永远命中不了桌面，之前写的
  `is_desktop_class` 同根特判也无从生效（没有候选可选）。

### 做了什么

1. `selection.rs` 加桌面分支：`query_shell()` 先取 `GetAncestor(hwnd, GA_ROOT)` 的类名，
   是 `Progman`/`WorkerW` 就走新的 `query_desktop()`，否则仍走 `query_by_hwnd()`。
2. `query_desktop()` 用 `IShellWindows::FindWindowSW(&VARIANT::default(), &VARIANT::default(),
   SWC_DESKTOP, &mut resolved, SWFO_NEEDDISPATCH)` 取桌面 browser，再复用同一条
   `IServiceProvider → QueryService(SID_S_SHELL_BROWSER) → QueryActiveShellView → IFolderView2 → GetSelection`
   链路。`Source` 新增 `Desktop` 变体（`source=desktop`）。
3. **带重试**：`FindWindowSW` 会间歇性返回 S_OK + 空指针（`windows` crate 里表现为 `Err(0x0)`），
   实测同一状态下 `comtest` 能成、`qlrust` 报错。现在 3 轮 × 2 种 flags，每轮间隔 15 ms。
4. 顺带修一个更隐蔽的问题：**`WM_COPYDATA` 处理期间不能调 COM**。
   第二实例转交请求（`--simulate-hotkey`、命令行传文件）时，主实例线程处于"输入同步呼叫"状态，
   `CoCreateInstance(CLSID_ShellWindows)` 直接被拒成 `0x8001010D`
   （"因为应用程序正在发送一个输入同步呼叫，所以无法执行传出的呼叫"）。
   改成只排队 + `PostMessage(WM_QLR_INVOKE)`，等普通队列消息取出时再处理。

### 验证

```
cargo fmt --check / clippy --all-targets -D warnings   0 警告
cargo test                                            57 passed
scripts/smoke.ps1                                     15 PASS + 1 SKIP（托盘 UIPI）
release exe                                           1,616,896 B（1.62 MB，预算 5 MB）

桌面全选（点桌面空白 + Ctrl+A）：
  --probe-selection-hwnd <Progman 0x25061A>
  → 目标是桌面（Progman），走 SWC_DESKTOP
  → GetSelection 命中 24 项 → 命中桌面 browser hwnd=0x25061A flags=1 attempt=0
  → PROBE-SELECTION source=desktop count=23 first=…\Desktop\DSH   （回收站被正确跳过）

桌面单选（点第一列图标 + 连按 Down 逐项）：
  source=desktop count=1 first=…\Desktop\微信图片_20261027221745.jpg

端到端（常驻实例 + --simulate-hotkey，前台 = Progman）：
  HOTKEY vk=0x20 fg=Progman source=desktop count=1 first=…\微信图片_20261027221745.jpg
  → SHOW … kind=image renderable=true size=1190x748
  → RENDER gen=1 bitmap bytes=1675520 elapsed=15ms
  → 预览窗 0x29A096E vis=True rect=365,146,1555,894（1190×748）
  → 截屏逐点比对：源 223,213,214 / 屏 215,206,207；源 237,241,242 / 屏 239,241,240；
     角落留白 26,26,26（0x1A1A1A）—— 缩放采样误差范围内，颜色正确、无洗白
```

### 版本与产物

workspace version 0.1.3 → **0.1.4**；`dist/qlrust-0.1.4-win64/`
（`qlrust.exe` 1,616,896 B + `portable.lock` + `README.txt` 6,521 B）与 zip（808,428 B）。
`qlrust.rc` 的 FILEVERSION/PRODUCTVERSION 同步改成 0,1,4,0（手工，见 HANDOFF §4.6）。

### 教训

- **"按了没反应"先看 `fg=`**：前台窗口类名一栏就能区分"钩子没生效"和"选区取不到"。
- **不要假设 Shell 的枚举是全集**：桌面不在 `IShellWindows` 里，是这类
  "看起来该有却没有"的典型；定位手段是写个独立诊断程序把每个注册项反查成 HWND 打印出来。
- **自动化"选中桌面图标"不能用 `LVM_SETITEMSTATE`**：跨进程指针无效。
  可用的是鼠标点图标 + 方向键 / `Ctrl+A`（见坑 #28）。

### 下一步

1. 把 0.1.4 交给用户重测桌面场景。
2. M2 继续：A2.6 PDF（pdfium）→ A2.8 滚轮/翻页 → 其他格式的缩放解码。
3. A1.6 第三/四级兜底（UIA `SelectionPattern` 200 ms 超时、鼠标命中测试）仍未做。

## 2026-10-30 · 0.1.5 查清"托盘又没了"：不是程序问题，是 exe 放在哪儿

### 用户原话
> 托盘又没了

### 定位链（这次没有再往程序逻辑里找）
1. 现场：用户在 `...\DSH\quicklook-rs\dist\qlrust-0.1.4-win64\` 里双击运行，
   日志在 `...\dist\qlrust-0.1.4-win64\UserData\logs\app.log`：
   `WARN 托盘图标创建失败（不影响预览）：拒绝访问。 (0x80070005)` → `壳层就绪 … tray=false`。
2. 同一份 exe 在 `D:\qlrust-try\` 里跑托盘是正常的（历史记录 15 PASS）。
3. `icacls` 一比就出来了：
   - 工作目录内：`Mandatory Label\Low Mandatory Level:(I)(NW)`
   - `D:\qlrust-try\qlrust.exe`：**没有任何 Mandatory Label 行**
   ⇒ DSH 沙箱给工作目录内的可执行文件打了 Low 完整性标签，进程启动后继承低完整性，
   低完整性不能与 Medium 的 explorer 通信（UIPI）→ 托盘 `Shell_NotifyIconW` 与
   Shell COM `CoCreateInstance(CLSID_ShellWindows)` **一起** `0x80070005`。
   这也解释了更早那次"`--probe-selection` 恒 source=none"的记忆。

### 改了什么
1. 新增 `crates/qlr-platform/src/integrity.rs`：
   `Integrity{Untrusted,Low,Medium,MediumPlus,High,System,Protected,Other}` +
   `current()`（`OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY)` →
   `GetTokenInformation(TokenIntegrityLevel)` → 从 `TOKEN_MANDATORY_LABEL.Label.Sid`
   偏移 8 读 `SubAuthority[0]`）+ `describe()` + `below_medium()`。
2. 启动日志第一屏加 `令牌完整性=…`（`crates/qlr-app/src/app.rs`）。
3. 托盘失败从 WARN 升级 ERROR，并在 `below_medium()` 时追加一条直指
   "DSH 工作目录 / Low Mandatory Level / 复制到普通目录"的诊断行。
4. 托盘失败时**弹一个说明框**（`qlr_platform::window::warn_box`，`MessageBoxW` +
   `MB_TOPMOST|MB_SETFOREGROUND`）：只在"双击启动"（无路径 + 裸 Toggle）时弹，
   且放独立线程——自动化不带参数的命令行调用不会弹、UI 线程也不会被模态框卡住。
5. 版本 0.1.4 → 0.1.5（`Cargo.toml` + `resources/qlrust.rc` 两处同步）。

### 验证
- `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` = 0；`cargo test` **57 passed**。
- `scripts/smoke.ps1` **15 PASS + 1 SKIP**（托盘 UIPI；冷启 75 ms、`RENDER gen=1 … elapsed=31ms`）。
- **正常目录**（交付副本 `<桌面>\qlrust-0.1.5\`，无标签）：
  `令牌完整性=high` → `托盘图标已添加 rect=(1683,1040)-(1707,1080) 尺寸=24x40` →
  `壳层就绪 window_created=13ms tray=true` → `热键就绪 vk=Space installed=true` → 气泡。
- **工作目录内**（dist 副本，带 Low 标签）：`令牌完整性=low（低于 medium…）` →
  `ERROR 托盘图标创建失败…0x80070005` → `ERROR 根因疑似令牌完整性=low…复制到普通目录再运行`，
  但 `SHOW … kind=image` 与 `RENDER gen=1 … elapsed=29ms` 仍正常 ⇒ **降权只影响与 shell 的通信，不影响预览**。
- release exe **1,621,504 B（1.62 MB）**，VERSIONINFO `0.1.5.0`。
- 交付物：`dist/qlrust-0.1.5-win64/`（exe + portable.lock + README 7,927 B）+ zip 811,323 B；
  另复制到**工作目录之外**的 `<桌面>\qlrust-0.1.5\` 供用户直接双击。

### 教训
- "某个 API 返回 0x80070005" 先问**进程令牌**，再怀疑 API 用法。以前把这条记成 #9（工作目录 exe 拿不到 COM），
  这次才把机制说清：**Low 完整性标签是 DSH 沙箱贴的**，`icacls` 一行就能验。
- 交付给用户的东西**必须放在工作目录之外**，否则用户双击的正是被降权的那份
  —— 这已经是第二次踩（第一次是 #10 副本变旧）。
- 程序不能改自己的令牌，但**必须把不可自救的原因说出来**：日志 + 弹框，否则用户只会以为"没起来"。

### 下一步
1. M2 继续：A2.6 PDF（pdfium）→ A2.8 滚轮/翻页。
2. A1.6 第三/四级兜底（UIA `SelectionPattern` 200 ms 超时、鼠标命中测试）。
3. 等用户确认 0.1.5：从桌面副本双击 → 托盘出现（若在 `^` 里，拖出来）→ 桌面选中图片按空格。

---

## 2026-10-30 · 0.1.6 PDF 预览（A2.8 第一期）+ 一个隐蔽的 UAF

### 做了什么
- **PDF 渲染层** `crates/qlr-render/src/pdf.rs`（新，约 390 行）：不用 `pdfium-render`
  （`cargo add --no-default-features` 仍拉进 tokio/wasm-bindgen，和 A8.5 装载裁剪冲突），
  改手写 12 个 PDFium C 函数的动态绑定（`libloading 0.8.9`）。
  - 库查找顺序：`QLRUST_PDFIUM` 环境变量 → `exe_dir/pdfium.dll` → `exe_dir/../pdfium.dll` → 裸名。
  - 缺库给可操作提示；错误码翻中文（1 损坏 / 2 需要密码 / 3 格式不支持 / 4 加密方式不支持 / 5 页面内容错误）。
  - **自己 `std::fs::read` 再 `FPDF_LoadMemDocument64`**：绕开 pdfium 的 ANSI 路径限制，中文路径可用。
  - 直接按显示尺寸渲染（`FPDFBitmap_CreateEx` 写进我们自己的 `Vec<u8>`，零额外拷贝），
    位图先 `FPDFBitmap_FillRect` 填白（PDF 页面没有背景，不填是透明黑）。
- **渲染器注册**：`RenderRequest` 加 `page`；新增 `PdfViewer`；`viewer_for(Kind::Pdf)`；
  `render_file_at(path, w, h, page)`（`render_file` 变成 page=0 包装）；`pdf_page_count(path)`。
- **翻页**：`hotkey` 加"旁听"模式——预览窗开着时（`set_active(true)`）
  只对 PageDown/PageUp/右/左 投递通知、**不吞键**（用户在资源管理器里还要用方向键）。
  空格仍然是"命中即吞"的那一个。
- **无窗口渲染入口** `--render <文件> [--page N]`：不建窗口、不进单实例，渲染一次写日志就退出，
  给基准脚本和故障诊断用。
- **自动化入口** `--next-page` / `--prev-page`（`Action::PageNext/PagePrev`）。
- 新增 `scripts/verify-pdf.ps1`：起常驻 → 投 PDF → `PrintWindow` 截图 → 采像素断言"有白底、有深色内容" → 退出。

### 修掉的 bug（本轮最重要）
`FPDF_LoadMemDocument64` **不拷贝**调用方的字节缓冲区，文档在生命周期内一直引用它。
我第一版把 `Vec<u8>` 留在 `open_document` 的栈上，函数一返回就释放 → pdfium 读到已释放内存。
表现：**某些页偶发 `LoadPage` 返回 NULL**（同一文件 page 0 成功、page 50 失败、page 199 又成功），
而且 `FPDF_GetLastError` 返回 0（"成功"），错误信息像是胡说。
修法：把 `Vec<u8>` 移进 `DocHandle`（`_data` 字段）持有。

### 验证
- `cargo fmt --check` 0 / `cargo clippy --all-targets -- -D warnings` 0 / `cargo test` **60 passed**
  （app 10 / core 20 / platform 9 / render 21）。
- `scripts/smoke.ps1`：**15 PASS + 1 SKIP**（托盘 UIPI，本会话提权）。
- PDF 基准（`test-200page-word.pdf`，Word 导出 200 页 228 KB，窗口 1190×748）：
  page 0/1/50/100/150/199 全部成功，**平均 6.3 ms、最慢 9 ms**（WBS 验收线是翻页 ≤80 ms）。
- 端到端翻页：`SHOW … kind=pdf` → `RENDER gen=1 … 40ms` → `PAGE 2/3` → `RENDER gen=2 2ms`
  → `PAGE 3/3` → 再翻被钳住（无 PAGE 行）→ `PAGE 2/3` → `RENDER gen=4 2ms`。
- 像素校验（`verify-pdf.ps1`）：深色 1122 px / 纯白 39877 px / 角落留白 26,26,26 → `PASS`。
- release exe **1,646,592 B（1.65 MB）**；dist 目录含 `pdfium.dll`（7,586,816 B）→ 合计 8.8 MB。
  **注意 A8.5 的"exe ≤5 MB"仍达标（1.65 MB），但整个分发目录从 1.6 MB 涨到 8.8 MB**，
  这是 PDFium 的代价，记在这里以免以后误以为体积失控。

### 教训
- **FFI 的内存所有权必须在类型上写清楚**：`LoadMemDocument` 这类"借用调用方缓冲区"的 API，
  要把 buffer 放进句柄结构体里，光靠注释"调用期间有效"会骗过自己。
- **错误码 0 不等于成功**：pdfium 的 `GetLastError` 只反映它自己认为的解析错误，
  指针为空但 code=0 基本就是"我们自己搞坏了内存"。
- 手写 PDF 生成器（`_bench/pdfium/makepdf.ps1`）不靠谱时，改用 Word 导出真 PDF 做基准
  —— 手写版有个 `/Length` 差 1 字节的坑，会让页面渲染成全白。

### 下一步
1. A2.8 第二期：缩放（Ctrl+滚轮 / 快捷键）、连续滚屏、页缩略图。
2. A2.6 文本 shaping（`cosmic-text`）与 A2.9 状态机、A2.10 `scripts/bench.ps1` 扩到 PDF。
3. A1.6 第三/四级兜底（UIA `SelectionPattern` 200 ms 超时、鼠标命中测试）。

## 2026-10-30 · 0.1.7 PDF 缩放 / 平移（A2.8 收尾）

**做了什么**
- crates/qlr-app/src/app.rs：State 增加 zoom: f32（默认 1.0）与 pan: (i32, i32)；ZOOM_MIN = 0.25、ZOOM_MAX = 8.0；scaled()（钳到 1..16384，避免 0 宽位图）、iew_size()、set_zoom()/zoom_reset()/set_zoom_to()、pan_by()（一步 = 1/12 屏）；epaint 改走 paint_bitmap_at(..., pan)；界面加"缩放：N%"一行。
- crates/qlr-platform/src/window.rs：新增 paint_bitmap_at(hwnd, w, h, bgra, offset_x, offset_y)——位图比客户区大的那一维按 offset 裁剪，小的那一维居中；paint_bitmap 变成 paint_bitmap_at(..., 0, 0)；顺带删掉不再被使用的私有 it_within。
- crates/qlr-platform/src/hotkey.rs：NAV_KEYS（PageDown/PageUp/四个方向键/+/-）只在预览窗打开时旁听且**不吞键**；0 故意不旁听（会干扰打字），复位走托盘/命令行。
- crates/qlr-core/src/ipc.rs + cli.rs：Action 加 ZoomIn/ZoomOut/ZoomReset/Pan{dx,dy}，命令行加 --zoom-in/--zoom-out/--zoom-reset/--pan <dx> <dy>（自动化用）。
- crates/qlr-render/src/pdf.rs：**新增 it_scaled() 允许放大**。之前 PDF 复用 image_view::fit_within（那条规则是"图片不放大"），导致 --zoom-in 之后位图尺寸恒等于页面原始点数 595×842，看起来"缩放没生效"。

**踩到并修掉的坑（重要）**
- **放大后整窗纯白**：StretchDIBits 的**源矩形 y 是从位图底部量起的**，即使 DIB 是 top-down（iHeight 取负）。把"从顶部算的平移偏移"直接塞进 sy 会取到图像另一头的空白区——表现为 zoom 1.56 时窗口一片白、平移几步才偶然露出文字。改成 sy = bh - ch - pan_y（横向同理）后正常。已写进 CONVENTIONS #32。
- 为定位它加了两个可长期保留的观测点：RENDER gen=… {}x{} bytes=… ink=…（ink = 位图里"非白"像素数，用来区分"渲染出来就是空白"和"渲染有内容但没画到屏幕上"）；PAINT bitmap {}x{} pan=… client=…（debug 级）。

**验证（全部实测）**
- cargo fmt --check 0 / cargo clippy --all-targets -- -D warnings 0 / cargo test **63 passed**（app 13 / core 20 / platform 8 / render 22）。
- scripts/smoke.ps1：**15 PASS + 1 SKIP**（托盘 UIPI），冷启 58 ms、第二实例 48 ms、RENDER gen=1 bitmap 1190x669 bytes=3184440 ink=198560 elapsed=31ms。
- scripts/verify-pdf.ps1 新增 -Zoom / -Pan 参数并三种工况全 PASS：
  - 原尺寸：itmap 529x748 ink=3075，屏幕上 深色=212；
  - 放大两档：itmap 826x1169 ink=7387，深色=471（内容确实更大更清楚）；
  - 放大后平移：深色降到 139/0（文字被滚出可视区，符合预期）。
- 端到端（工作目录外的 <桌面>\qlrust-0.1.7\）：令牌完整性=high、托盘 ect=(1683,1040)-(1707,1080)、ZOOM 1.00 -> 1.25 -> 1.56、RENDER gen=3 bitmap 826x1169 elapsed=2ms、PAN (0, 62)、--quit 后进程 0。
- release exe **1,662,464 B（1.66 MB）**，VERSIONINFO 0.1.7.0。

**交付物**：dist/qlrust-0.1.7-win64/{qlrust.exe, pdfium.dll, portable.lock, README.txt}；README 新增「0.1.7 改了什么」（缩放/平移、纯白修复、导航键不吞键）。

**下一步**：A2.5 文本 shaping（cosmic-text）、A2.9 状态机收口、A2.10 把 scripts/bench.ps1 扩到 PDF、A1.6 三四级兜底（UIA / 鼠标命中）。

---

## 2026-10-30 · 0.1.8 滚轮缩放 / Esc 关闭 / 拖拽平移（A2.8 收口）

**用户原话**：无（本轮由目标轮次自动推进；上一轮用户只说"继续做吧"）

### 做了什么
1. **滚轮缩放**。预览窗是 `WS_EX_NOACTIVATE`，永远拿不到焦点，系统的 `WM_MOUSEWHEEL`
   会投给前台窗口（资源管理器），我们收不到 —— 这是 A2.8 一开始就标了"要先验"的点。
   验完的结论：**必须旁听低级鼠标钩子**（`WH_MOUSE_LL`），和空格键同一条路子。
   - `hotkey.rs` 加装 `WH_MOUSE_LL`：回调里依次过滤 `code<0`、`!ACTIVE`、
     `WM_MOUSEWHEEL`、`LLMHF_INJECTED`、预览窗未创建/不可见、`pt` 不在预览窗矩形内，
     全通过才 `PostMessage(WHEEL)` 并吞掉。装不上只 WARN，不影响键盘钩子。
   - 一次最多吃 4 格（`wheel_notches`）：触控板会一口气甩出几十格，全给会瞬间冲到 800%。
     一格 = 1.25×。
2. **Esc 收起**。同样靠键盘钩子旁听，但**只在"预览窗开着 + 前台是 shell 类 + 非注入键"**
   时才吞；其余一律放行 —— 否则别的程序里的 Esc（取消对话框、退出全屏）会被我们吃掉。
3. **拖拽平移**。`WM_LBUTTONDOWN` 记录起点 + `SetCapture`（不设捕获的话鼠标移出窗口
   就收不到 `WM_MOUSEMOVE`），累计位移 ≥3 px 才算"拖动"，之后每帧增量交给
   `pan_pixels(-dx, -dy)`；`WM_LBUTTONUP` 时没拖动过才算"点一下 → 关闭"。
   - 坐标要按**有符号** i16 解析（`lparam` 低/高 16 位），`& 0xFFFF` 会把负坐标读成 6 万多。
4. CLI 加 `--wheel <delta>` / `--simulate-hotkey` 之外的诊断口径；`hotkey::stats()` 加 `wheels`
   计数，退出日志多一项。
5. 单测 +2：`wheel_notches_clamps_and_ignores_jitter`、`client_xy_sign_extends_both_halves`。

### 为什么这么切
滚轮/Esc 都走同一条"钩子线程旁听 + PostMessage 回 UI 线程"的路，复用了 A1.5 已经压过的
心跳重建、注入过滤、白名单逻辑，没有引入新的常驻线程；拖拽则完全在 UI 线程内处理，
不需要任何钩子。

### 验证
- `cargo fmt --check` 0 / `cargo clippy --all-targets -- -D warnings` 0 /
  `cargo test` **65 passed**（app 15 / core 20 / platform 8 / render 22）。
- `scripts/smoke.ps1`：**17 PASS + 1 SKIP**（托盘 UIPI）。新增两项检查：
  「鼠标钩子已安装=true（滚轮缩放可用）」「滚轮缩放已生效 1.00 -> 1.25」。
- 真滚轮/真 Esc **没法自动化**：钩子会过滤 `SendInput`/`mouse_event` 合成出来的事件
  （`LLKHF_INJECTED`/`LLMHF_INJECTED`），这是刻意的防重放设计。所以：
  - 缩放链路用 `--wheel` 直投 `Action::Wheel` 验（走的是同一段 `wheel_zoom`）；
  - 钩子本身验"装上了"（日志 `鼠标钩子已安装=true`）；
  - 拖拽用真实 `mouse_event` 拖动验（鼠标消息不过钩子），日志实测
    `PAN bitmap 827x1169 pan=(0, 12) → (0, 24) → … → (0, 72)`，逐步累加正确。
- 工作目录外整套复验（`<桌面>\qlrust-0.1.8\`）：
  `令牌完整性=high`、`托盘图标已添加 rect=(1683,1040)-(1707,1080)`、
  `鼠标钩子已安装=true`、`ZOOM 1.00 -> 1.25 render=1488x935`、
  `RENDER gen=2 bitmap 661x935 bytes=2472140 elapsed=2ms`、`--quit` 后进程归零。
- release exe **1,669,120 B（1.67 MB）**，VERSIONINFO `0.1.8.0`；
  dist 目录 9.2 MB（含 `pdfium.dll` 7.6 MB）。

### 踩到的坑
- `SetCapture`/`ReleaseCapture` 在 `windows::Win32::UI::Input::KeyboardAndMouse`，
  **不在** `WindowsAndMessaging`，也不在 Gdi（我一开始按 Gdi import，报 E0425）。
- PowerShell 单引号字符串里的 `\n` 是字面量，不能当换行写进 Rust 源码
  （把 import 行写成了 `... WM_PAINT,\n WM_RBUTTONUP,`，编译报 "unknown start of token: \"）。
- `Stats` 加字段后，两处 `log_info!` 的占位符与实参数量要一起改（漏一处就是
  "5 positional arguments in format string, but there are 4 arguments"）。

### 下一步
A2.5 文本 shaping（`cosmic-text`，现在文本是逐行 `DrawTextW`，没有换行折叠与富文本）、
A2.9 状态机收口、A2.10 把 `scripts/bench.ps1` 扩到 PDF 与缩放态、A1.6 三四级兜底
（UIA `SelectionPattern` / 鼠标命中测试）。

### 追加（同轮顺手修）：转发路径没绝对化

`verify-pdf.ps1` 复验时发现 `RENDER gen=1 失败：打不开 PDF ..\_bench\...（os error 3）`。
根因不在渲染器：**第二个实例是在"用户的当前目录"里启动的，但它把相对路径原样通过
`WM_COPYDATA` 转给常驻实例，而常驻实例的 CWD 是它自己启动时的目录**，于是相对路径失效。
QuickLook 上游是在新进程里做 `Path.GetFullPath`，我们这边改成**在转发方就绝对化**
（`crates/qlr-app/src/cli.rs` 新增 `fn absolutize`：`is_absolute` 直接返回，否则
`std::path::absolute()`，失败才原样传）。这样常驻实例永远只看到绝对路径。

顺带修 `scripts/verify-pdf.ps1` 两处：`-Pdf`/`-ExeDir` 先 `Resolve-Path` 绝对化
（WMI `Win32_Process::Create` 的工作目录是 `C:\Windows\System32`，相对路径会静默失败，
表现为"日志一行都没有 + 找不到窗口"）；视觉判据仍用 `test-3page.pdf` —— 200 页那份 Word
PDF 的封面本来就几乎空白（`ink=111`），拿它做"有没有渲染出来"的判据会误报。

---

## 2026-10-30 · 0.1.9 文本可滚动 + 可视区虚拟化（A2.5 的"能读完整篇"部分）

**背景**：文本预览此前只能看前 2000 行、且**完全没有滚动**——超过一屏的内容根本看不到。
上游 QuickLook 在这个位置翻过车（64 KB 的 txt 滚屏吃掉 6.6 GB），所以这次改动同时要
把"内存不炸"的底线守住。

### 做了什么
1. `qlr-render::text`：`MAX_LINES` 从 `2_000` 提到 `200_000`（读入上限仍是 8 MB，
   两个预算一起把最坏情况钉死）；新增 `pub fn render_with(path, head, read_limit, max_lines)`，
   `render()` 变成它的默认预算包装——这样测试能传小预算，不必真造 20 万行。
2. `qlr-app`：`State` 加 `scroll: usize`；新增
   `scroll_text_by(lines) -> bool`（不是文本就返回 false，让调用方退回缩放/翻页/平移）、
   `scroll_clamp(current, delta, total, rows) -> usize`（纯函数，7 条断言）、
   `visible_rows(hwnd)`（客户区/行高 − 1 行状态行）、`text_total_lines()`。
3. 输入分发按内容类型分流：滚轮在文本上 → 一格 3 行；PageDown/PageUp → 一页；
   ↑↓ → 一行。图片/PDF 保持原来的缩放/平移/翻页。
4. `repaint` 的 `Canvas::Text` 分支改成**只切出当前窗口该显示的那几十行**再交给 GDI，
   第一行是状态行：`── 第 a–b 行 / 共 N 行（滚轮或 PageDown 滚动）──`，被截断时另加提示。
5. `set_content` 里把 `scroll` 归零，否则打开新文件还停在上一个文件的中段。

### 验证
- `cargo fmt --check` 0、`clippy --all-targets -- -D warnings` 0、
  `cargo test` **67 passed**（app 16 / core 20 / platform 8 / render 23）。
- 夹具 `spike/fixtures/big_100mb.txt`（26.6 MB，读入被 8 MB 截断成 **121,425 行**）：
  `SHOW ... kind=text renderable=true` → `RENDER gen=1 text 0x0 bytes=8145760 ink=121425 elapsed=22ms`
  → 滚轮三次 `SCROLL 0 -> 6 -> 12`（每次 2 格 = 6 行），内存没涨（8 MB 量级）。
- `scripts/smoke.ps1`：**17 PASS + 1 SKIP**（托盘 UIPI）。
- 工作目录外复验（`<桌面>\qlrust-0.1.9\`）：`令牌完整性=high`、
  托盘 `rect=(1683,1040)-(1707,1080)`、文本滚动 `SCROLL 0 -> 6 -> 12`。
- release exe **1,673,728 B（1.67 MB）**，VERSIONINFO `0.1.9.0`。

### 踩到的坑
- 一次 PowerShell `Replace` 把注释里的 `0` 写成了**NUL 字节**，`edit` 工具随后判
  "binary file" 拒绝编辑。诊断：`[System.IO.File]::ReadAllBytes($p) | Where-Object { $_ -eq 0 }`；
  修法：`GetString` → `Replace([string][char]0, "0")` → `WriteAllText(UTF8 无 BOM)`。
- `.NET` 的当前目录**不跟随** PowerShell 的 `Set-Location`：`[System.IO.File]::ReadAllText("crates\...")`
  会去 `workspace\crates\...` 找。.NET 文件 API 一律用绝对路径（这条已进 CONVENTIONS）。
- 多行锚点用 PowerShell here-string 做 `Replace` 时，CRLF/LF 不一致会**静默失配**；
  改代码块优先用 `edit` 工具，PowerShell 只做单行替换。

### 下一步
A2.6 文本 shaping（`cosmic-text`，现在仍是逐行 `DrawTextW`：没有自动换行、没有长行折行、
没有富文本）、A2.10 把 `scripts/bench.ps1` 扩到 PDF/缩放态/文本滚动、
A1.6 三四级兜底（UIA `SelectionPattern` / 鼠标命中测试）。

---

## 2026-10-30 · 0.2.0 文本折行（M2 收口）

**用户指令**：`做到 m2 后先停止后续进程`（m09431）→ 本轮把 M2 最后一块（文本折行）做完，
然后停手，不再往 M3 推进。

### 做了什么
1. **折行是纯逻辑，测量用依赖注入** —— `crates/qlr-render/src/wrap.rs`（新）：
   `wrap_window(lines, start, max_rows, width, measure) -> WrapWindow { rows, consumed_raw }`。
   不引入 `cosmic-text`（会拉 fontdb/swash，约 1–2 MB，与 A8.5「exe ≤5 MB」冲突），
   宽度测量交给平台层用 GDI 做。只折**当前这一屏**：26 MB 文本 12 万行，全量折行会卡死。
   `MAX_ROWS_PER_LINE = 400` 防一行 2 MB 的压缩 JSON 吃光内存；
   `fit_prefix` 英文在空格/制表符断、中文在字符边界断、硬切兜底保证"绝不超宽"。
2. **平台层加 `window::Measurer`** —— 持有一个 DC + `DEFAULT_GUI_FONT`（**必须与 `paint_text`
   用的是同一套字体**，否则量出来的宽度和画出来的宽度不一致），`width(&self, text) -> u32`
   走 `GetTextExtentPoint32W`，`Drop` 里还原并 `ReleaseDC`。
3. **`repaint` 的文本分支改成"折行 + 虚拟化"** —— 宽度 = 客户区宽 − 48（`paint_text` 左右各留
   24 px）；拿不到 DC 就退回不折行；折行结果与状态行一起交给 `paint_text`。
4. **翻页语义修正** —— 新增 `State.wrapped_consumed`，`turn_page()` 按"上一屏实际吃掉多少
   **原始行**"前进（折行后一屏原始行远少于屏幕行数，按屏幕行数翻会跳内容），
   `--next-page` / `--prev-page` 与 PageDown/PageUp 走同一条路径。

### 验证（真机 Win10 22H2 / i5-12400F / release 0.2.0）
- 折行夹具 `_bench/tmp/wrap-long.txt`（300 行 × 400 字符）：`WRAP 原文 300 行 从 0 起，屏幕 30 行，宽 1142px -> 折成 30 行（吃掉 8 原始行）`，
  连按三次 `--next-page` 得到 `SCROLL 0 → 8 → 16 → 24`，再 `--prev-page` 回到 16 —— **不漏行、不跳行**。
- 中文折行夹具 `_bench/tmp/wrap-test.txt`（3000 字符 ASCII 长行 + 570 字中文长行）：`折成 30 行（吃掉 6 原始行）`。
- 三个基准夹具（手工流程，宿主在工作目录外）：
  | 场景 | 渲染 | 按键到出图 | 峰值 |
  |---|---|---|---|
  | 3840×2160 JPEG | 41 ms | 87 ms | 12.5 MB |
  | 8000×6000 JPEG | 77 ms | 95 ms | 12.3 MB |
  | 26.6 MB 文本 | 21 ms | 53 ms | 21.7 MB |
- 关卡：`cargo fmt --check` 0；`cargo clippy --all-targets -- -D warnings` 0；
  `cargo test` **75 passed**（app 16 / core 20 / platform 8 / render 31）；
  `scripts/smoke.ps1` **17 PASS + 1 SKIP**（托盘 UIPI）。
- release exe **1,679,360 B（1.60 MB）**，VERSIONINFO `0.2.0.0`；dist 含 `pdfium.dll` 共 8.8 MB。

### 踩的坑（已进 CONVENTIONS）
- **#37** `scripts/bench.ps1` 每轮删日志 + 起新实例时，若上一轮宿主还活着，新实例只会把请求
  **转交**给老宿主，而老宿主握着的是已删除的旧日志文件句柄 → 每轮 `FAIL(无 RENDER)`，
  日志里只有"请求已转交"。基准前必须 `--quit` 掉老实例（本轮给脚本补了前置 `--quit`，
  仍未彻底稳定，故 M2 数字改用手工流程）。
- **#38** 折行宽度必须等于绘制宽度：`paint_text` 每行左右各留 24 px 且用 `DEFAULT_GUI_FONT`，
  所以测量器必须选同一个字体、可用宽度必须减 48 px，否则会出现"折了行还是被省略号吃掉"。

### 下一步（**用户已要求停在 M2**）
- 不启动 M3/M4/M5/M6 与插件体系。
- 若之后要继续，第一件事是修 `scripts/bench.ps1` 的宿主生命周期问题，再进 M3（音视频/归档）。

## 2026-10-31 · 0.3.0 格式收窄为四类，其余转插件（按用户要求）

### 用户要求（原话）
> m345先停止，去除掉对其他格式的支持，目前先主要支持图片、pdf、文本、md等4个格式，
> 把后边的m6跑完，把整个项目打磨完整，后续其他格式以插件的形式单独开发，支持到项目里

⇒ 方向确定：**内置只保留 图片 / PDF / 文本 / Markdown**；M3/M4/M5 的格式族（音视频、Office、
归档、字体、DB、PE/ELF…）**不再内置**，改为插件形态；接着把 M6 硬化与发布跑完，收口 1.0.0。

### 做了什么（`crates/qlr-core/src/format.rs` 整文件重写，524 → 427 行）
- `Kind` 从 25 个变体砍到 **5 个**：`Image` / `Pdf` / `Text` / `Markdown` / `Unsupported`；
  去掉 `Media`/`Office`/`Archive`/`Font`/`Database`/`PortableExecutable`/`Elf`/`Csv`/`Html`/… 与
  `Unknown`（`Unknown` 语义并入 `Unsupported`）。
- 方法收敛为 `name()`（稳定标识，进日志/脚本）、`label()`（中文名）、`is_renderable()`、
  `hint()`（不支持时的建议文案，指向 `docs/dev/PLUGINS.md`）；删掉 `roadmap()`（里程碑字符串已无意义）。
- 扩展名表 308 → **约 150**，按族分组并带注释：`IMAGE_EXTS`(13) / `PDF_EXTS`(1) /
  `MARKDOWN_EXTS`(5) / `TEXT_EXTS`(约 130：纯文本+配置+CSV+脚本+各语言源码+前端+SQL/TeX 等)。
- 新增 `extensions(kind)` / `all_extensions()`：文档、样本矩阵脚本、文件关联都从这里取真值，不再各写一份。
- **新增内容嗅探兜底**（原设计明确不做，这次补上）：扩展名不认识（含无扩展名）时读文件头
  512 B：`%PDF-` → Pdf；PNG/JPEG/GIF/BMP/TIFF/ICO/CUR/RIFF+WEBP 魔数 → Image；
  没有 NUL 且控制字符 ≤5% → Text；否则 `Unsupported`。读不到文件也判 `Unsupported`
  （把 exe 当文本渲染出一屏乱码，比明确说不支持更糟）。
- 连带改动：`qlr-render/src/lib.rs` 的 `viewer_for` 只留三臂（Markdown 复用文本查看器）、
  报错文案改为 `"{kind} 暂无内置查看器（{hint}）：{path}"`；`qlr-app/src/app.rs` 的 `Kind::Unknown` →
  `Kind::Unsupported`、占位横幅改为「内置支持：图片 · PDF · 文本 · Markdown（其余格式以插件形式扩展）」、
  不支持时多打一行插件提示。

### 怎么验证的
- 单测：`cargo test` **77 passed**（app 17 / core 22 / platform 8 / render 31）。新增 8 条：
  四类各自路由、media/office/zip/exe/ttf/stl 一律 `Unsupported`、`sniff` 魔数、
  `sniff` 文本/二进制兜底、**无扩展名真实文件**（`README`→Text、`noext`(内容是 `%PDF`)→Pdf、
  `blob`(含 NUL)→Unsupported）、`all_extensions` 唯一且 >100 且不含 `mp4`、
  `build_lines` 的不支持占位必须含"插件"。
- 实机（`<夹具目录>`，便携模式）：
  - `clip.mp4` → `SHOW target=…\clip.mp4 kind=unsupported renderable=false`，窗口出占位文字；
  - `note.md` → `kind=markdown renderable=true`、`RENDER gen=1 text 0x0 bytes=141 ink=12`；
  - 宿主已在跑时再发一次 → `请求已转交：action=Toggle`（单实例转发未受影响）。
- 关卡：`cargo fmt --check` 0、`cargo clippy --all-targets -- -D warnings` 0、release 构建通过。

### 踩的坑（已进 CONVENTIONS）
- **#39** 工作区内的 exe 带 `Mandatory Label\Low Mandatory Level:(I)(NW)`（no-write-up），
  Low 进程写 Medium 对象一律 `ACCESS_DENIED(5)`。于是"写临时文件"的测试用例一定挂，
  且 `create_dir_all(%TEMP%)` 被拒后 `write` 报的是 `NotFound(3)`；同样代码用 rustc 编到
  `%TEMP%` 再跑却正常，极易误判。**测试夹具一律写 `<workspace>/target/…`。**
- PowerShell 多行 here-string 做 `ReadAllText`/`Replace`/`WriteAllText` 把
  `crates/qlr-render/src/lib.rs` **写成了 0 字节**（`$t` 为 null 时 `WriteAllText` 照样执行），
  靠 `git checkout --` 救回。**改源码一律用 `edit` 工具，不用 PowerShell 拼接替换。**
- 顺手清了 4 个历史遗留 **NUL 字节**（`docs/dev/LOG.md`×2、`docs/dev/ARCH.md`×1、
  `crates/qlr-platform/src/hotkey.rs`×1，都是早前 PowerShell 写文件时把 `0` 写成 NUL），
  它们会让 `edit` 工具判 "binary file" 拒改。

### 下一步
- Markdown 轻量排版（标题/代码块/引用/列表着色，复用折行 + 行样式）。
- 插件体系骨架（`qlr-plugin-api` C ABI v1 + `plugins\*.dll` 扫描 + 示例插件 + PLUGINS.md/ADR）。
- 然后 M6：安装器/文件关联/开机启动、Win11 圆角/backdrop、bench 进 CI、安全回归、README、样本矩阵。
---

## 0.4.0 —— Markdown 轻量排版（2026-10-31）

**做了什么**：Markdown 从"按纯文本显示"升级为"按块级元素排版"——标题变大变金、
代码块等宽淡蓝、引用灰色带 `▎` 前缀、列表圆点统一、`---` 画成一条灰横线。

- `crates/qlr-render/src/markdown.rs`（新增，约 300 行）：
  `render(path, head)` / `render_with(...)` 复用 `text.rs` 的读入/解码/截断预算，
  `layout(&[&str]) -> (Vec<String>, Vec<LineStyle>)` 做纯逻辑分类（可单测）：
  ATX 标题（`#`…`######`，`####` 以上统一按三级样式）、围栏代码块（``` / ~~~，
  **围栏行本身不显示**）、引用（去 `>` 加 `▎`）、列表（`-`/`*`/`+`/`1.` 归一成 `•`）、
  分隔线（`---`/`***`/`___` → 40 个 `─`）。
  **不做**行内强调/表格对齐/HTML/链接预览——那些留给插件。
- `crates/qlr-render/src/canvas.rs`：`LineStyle { Normal, Heading1..3, Code, Quote, List, Rule }`
  （`as_index()/name()/COUNT`），`TextDoc` 增 `styles: Option<Vec<LineStyle>>`
  （`None` = 纯文本，零额外分配）。
- `crates/qlr-platform/src/window.rs`：`TextStyle`（像素级：字号/字重/字体面/颜色）、
  `StyledRow<'a> { text, style }`、`paint_styled()`、`Measurer::width_styled()`；
  `paint_text()` 变成 `paint_styled()` 的薄包装——**全程序只剩一条文本绘制路径**，
  折行时量的字体与绘制时用的字体不可能再对不上。字体用 `CreateFontW`
  （雅黑正文 15 px / 标题 18·17·16 px 粗体 / 代码 13 px Consolas），
  **行高统一 24 px**（`LINE_HEIGHT` 常量导出），创建失败退回系统 GUI 字体。
- `crates/qlr-app/src/app.rs`：`repaint` 的文本分支按 `t.styles` 分两条路——
  纯文本仍用 `wrap_window` 整屏折行；带样式时**逐原始行**调 `push_wrapped`，
  每行用自己样式的测量器量宽 + 记录该行样式（`body_styles`）。
  `to_text_style()` 是渲染层语义 → 平台层像素的唯一转换点。
  另加 `styled_logged` 与 INFO 级 `STYLED` 日志（每份内容打一次样式分布）。

**验证（实机，`<夹具目录>`）**：
夹具 `md-demo.md`（标题/列表/长引用/rust 代码块/分隔线/三级标题）→
`kind=markdown renderable=true`、`RENDER gen=1 text bytes=639 ink=20`、
`STYLED 屏幕 20 行（样式分布：h1=1 h2=1 h3=1 code=4 quote=1 list=3 rule=1 normal=8）`
—— 与夹具逐项对得上。截图存 `_bench/md-styled.png`（按行带颜色统计：标题色只在顶部行出现、
代码色在中部、引用色在下部，说明不是整屏一个颜色）。
关卡：`cargo test` **87 passed**（app 17 / core 22 / platform 8 / render 40）、
`fmt` 0、`clippy -D warnings` 0。

**踩的坑（已进 CONVENTIONS）**：
- **#40** windows-rs 0.62：`SelectObject` 要 `HGDIOBJ`（`HFONT` 得 `.into()`）、
  `FW_BOLD`/`FW_NORMAL` 是 `FONT_WEIGHT` newtype 要 `.0`、`DEFAULT_PITCH` 是 `u8` 要 `as u32`。
- **#41** `COLORREF` 是 `0x00BBGGRR`（R 在最低字节），写颜色时最容易把红蓝写反；
  且 ClearType 会把纯色文字混成采样不到的颜色，**截图判色不可靠**，要看日志。

**下一步**：插件体系骨架（`qlr-plugin-api` C ABI v1 + `plugins\*.dll` 扫描 + 示例插件）。
---

## 0.5.0 —— 插件体系（M5 收窄版，2026-10-31）

**做了什么**：给"其余格式"开了一条真正的接入通道。内置只做四类，剩下的由插件补——
装上插件、重启、按空格，视频/Office/压缩包就能像内置格式一样出画面。

- 新 crate **`qlr-plugin-api`**（零依赖、纯 `#[repr(C)]`）：三个导出符号
  `qlr_plugin_info` / `qlr_plugin_render` / `qlr_plugin_free`，
  位图用 RGBA8 + `(ptr, len)`，`ABI_VERSION = 1`，像素上限 64 MP。
  契约里带一条 `size_of` 断言（`QlrPluginInfo` 40 / `QlrImage` 24），改结构体立刻红。
- 新 crate **`qlr-plugin-demo`**：视频占位图插件（接管 mp4/mkv/webm/avi/mov/flv/wmv/m4v），
  按路径哈希着色、16:9 适配宿主要求的尺寸；它同时是 PLUGINS.md 的模板来源。
  编译成 `cdylib` 只有 **101 KB**。
- `crates/qlr-core/src/plugins.rs`：宿主。扫 `<exe>\plugins\*.dll`（`QLRUST_PLUGIN_DIR` 可覆盖）、
  校验 ABI/符号/名字/扩展名、按扩展名查（同名冲突先注册者胜）、
  **跨界数据一律拷贝后再调 `free`**、坏插件只记错误不影响启动。
- `crates/qlr-app/src/app.rs`：渲染线程按"内置优先、插件补缺"派发；
  新增 `render_with_plugin()`、`PLUGIN …` 日志、`SHOW … plugin=` 字段、
  占位文字里的"插件目录 N 个插件 M 个扩展名"提示。
- **顺手修掉一个老 bug**：`SHOW … elapsed=` 原来用**进程启动时刻**算，
  常驻实例上会报出 `elapsed=6583ms` 这种荒谬数字；改为 `State.request_started`
  （每次 `apply_request` 重置），现在报的是真实的"这一下按下去到出窗"。

**验证（实机，`<夹具目录>`）**：
```
插件就绪 dir=…\plugins loaded=1 extensions=8 errors=0
插件 示例插件：视频占位图 接管 [mp4 mkv webm avi mov flv wmv m4v] 文件=…\plugins\qlr_plugin_demo.dll
SHOW target=…\clip.mp4 kind=unsupported renderable=true plugin=示例插件：视频占位图 size=1190x748 elapsed=1ms
PLUGIN 示例插件：视频占位图 1190x669 bytes=3184440 elapsed=2ms (请求 1190x748)
SHOW target=…\tool.exe kind=unsupported renderable=false plugin=- size=1190x748 elapsed=0ms
```
（`.exe` 没插件接管 → 走占位文字，里面写明插件目录与已加载数量。）
关卡：`cargo test` **104 passed**（app 17 / core 28 / platform 8 / render 40 / plugin-api 4 / plugin-demo 7）、
`fmt` 0、`clippy -D warnings` 0（示例插件的导出必须标 `unsafe extern "C"`，否则
`not_unsafe_ptr_arg_deref` 会挡下）。exe **1,732,096 B**——**插件体系让主程序一个字节都没涨**。

**文档**：`docs/dev/PLUGINS.md`（给插件作者的唯一入口：十行上手、宿主怎么用、硬约定表、
性能建议、调试、发布）、`docs/adr/0008-plugin-api.md`（为什么进程内 C ABI 而不是进程外 JSON-RPC）。

**下一步**：M6 剩余项——安装器/文件关联/开机启动、Win11 圆角/backdrop、bench 进 CI、
安全回归（畸形语料）、样本矩阵、README/截图，然后收口 1.0.0。
---

## 0.6.0 —— 安装器 + Win11 观感（M6 的 A6.1 / A6.3，2026-10-31）

**做了什么**

1. **每用户安装器**（A6.1，免管理员）。`scripts/install.ps1`：
   - 装到 `%LOCALAPPDATA%\Programs\QLRust`，复制 exe / pdfium.dll / plugins\ / README；
   - 注册文件关联（ProgID + `Applications\qlrust.exe` + 每个扩展名的 `OpenWithProgids` +
     `Capabilities` + `RegisteredApplications`），所以它会出现在「设置 → 默认应用」里；
   - 开始菜单快捷方式；`-Autostart` 写 HKCU 的 Run 项；注册「应用和功能」里的卸载项；
   - `scripts/uninstall.ps1` 对称回滚（只摘自己写的值，不动同键里别的程序的条目）。
2. **扩展名清单只有一份真值**：新增 `qlrust.exe --list-extensions <文件>`，
   由 `format::all_extensions()` 导出（123 个），安装器读它来注册关联。
   *为什么不打 stdout*：GUI 子系统进程的 stdout 句柄经常无效，重定向会拿到空文件，
   那会让安装器"成功"地注册 0 个扩展名。
3. **Win11 观感**（A6.3）：新增 `crates/qlr-platform/src/win11.rs`，
   用官方 `DWMWA_WINDOW_CORNER_PREFERENCE`（圆角）与 `DWMWA_SYSTEMBACKDROP_TYPE`
   （Mica 家族 backdrop）给预览窗口加效果；**Win10 上这两个属性不存在，调用静默失败**，
   继续走原来的未文档化亚克力。显示位图时 backdrop 必须清掉
   （和 0.1.1"图片被提亮"是同一个坑）。
4. **打包脚本** `scripts/pack.ps1`：dist 目录 + zip + `SHA256SUMS.txt`；
   新增 `packaging/README.txt`（用户拿到手看到的那份说明，含快捷操作表、插件说明、
   排障清单、实测性能表）。

**验证（都是本机实跑）**
```
install.ps1 -Source target\stage        → 已复制 3 个文件 / 关联扩展 123 个 / 开始菜单 ✓
uninstall.ps1（此时实例在跑）            → 进程已退出 / 摘除 123 个 / 目录与注册表全清
install.ps1（默认从 dist 找源）          → 源目录 dist\qlrust-0.5.0-win64
装好后跑起来（这是关键一条）：
  exe_dir=…\AppData\Local\Programs\QLRust portable=false data_dir=…\AppData\Local\QLRust
  令牌完整性=high            ← 安装目录不在 DSH 工作区里，头一次看到托盘正常
  托盘图标已添加 rect=(1683,1040)-(1707,1080) 尺寸=24x40
  插件就绪 dir=…\plugins loaded=1 extensions=8 errors=0
  SHOW … kind=markdown renderable=true size=1190x748 elapsed=1ms
Win10 上的 Win11 分支：系统 build=19045 圆角=false backdrop=false（预期，静默降级）
```
关卡：`cargo test` **107 passed**（app 18 / core 28 / platform 11 / render 40 /
plugin-api 4 / plugin-demo 7）、`fmt` 0、`clippy -D warnings` 0。
exe **1,740,800 B**（+1.5 KB：Dwm 绑定）。

**踩坑**（已进 CONVENTIONS）
- **#42** `& $exe --quit` 在没有常驻实例时会把脚本永久挂住（GUI 程序自己变宿主进消息循环），
  安装/卸载脚本必须用 `Invoke-CimMethod Win32_Process::Create`；
  另外 `$_.WaitForExit()` 返回 bool，不加 `[void]` 会往输出里漏 `True`。
- **#43** PowerShell 变量名大小写不敏感：`$AppKey` 被后来的 `$appKey` 覆盖，
  结果 Capabilities 写到了 `Applications\qlrust.exe\Capabilities`，而每一步都"成功"。
- **#44** 注册表默认值必须 `Set-Item -Path $k -Value x`；
  `New-ItemProperty -Name '(Default)'` 造出来的是名字字面叫 `(Default)` 的值，资源管理器读不到。

**下一步**：A6.4（bench.ps1 修宿主生命周期 + 基准进 CI）、A6.5（畸形输入安全回归）、
A6.7（样本矩阵）、A6.6（仓库 README/截图），然后收口 1.0.0。
---

## 1.0.0（2026-10-31）— M6 收口：硬化、样本矩阵、文档、打包

> 用户要求："把后边的 m6 跑完，把整个项目打磨完整，后续其他格式以插件的形式单独开发"
> 起点 0.6.0（安装器 + Win11 观感），本次把 M6 剩余五项（A6.2/A6.4/A6.5/A6.6/A6.7）全部做完并收口 1.0.0。

### 做了什么

1. **A6.4 基准可无人值守**（`scripts/bench.ps1` 重写，177 → 约 270 行）
   - 修掉两个致命 bug：RENDER 正则漏 `ink=` 字段（每轮必然 FAIL）、同文件二次投递是 Toggle 需先 `--hide`（坑 #45）。
   - 新增 `-Rounds`（冷/热两遍）、`-Only`、`-Budget <json>`、`-JsonOut`；输出三张表（冷启动 / 首帧 / 内存）。
   - `Stop-Host` 强制收尾链（`--quit` → 4 s → `Stop-Process -Force` → 删日志 50 次重试 → 断言无残留）。
   - 新增 `scripts/make-fixtures.ps1`（`-Big` / `-Matrix`）、`scripts/bench-budget.json`、`scripts/bench-budget-ci.json`、
     `.github/workflows/bench.yml`（push master + 每周一 03:00 UTC + 手动，超预算即失败）。
2. **A6.5 安全回归**（`crates/qlr-render/tests/malformed.rs`，7 条）
   - 空文件、截断图片、坏 PDF、二进制/超长行、病态 Markdown、`.mp4` 不进解析器；
   - `fuzz_mutated_samples_never_panic`：xorshift64 确定性变异 3 种子 × 106 例 = **318 例**零 panic；
   - **抓到真实缺陷**：带 BOM 的 UTF-16 被判成二进制（坑 #48），两处（`text.rs` / `format.rs`）同修并加单测。
   - 新增 `docs/dev/SECURITY.md`（输入面 / 威胁模型 / 资源上限表 / 路径与进程约束）。
3. **A6.7 样本矩阵**（`scripts/sample-matrix.ps1`）
   - 逐文件 `--render` + 日志增量读判定，按类统计，`-MinRate 0.98` 门限，每类 <20 个给 ⚠。
   - 合成矩阵 97 个 **100%**（image 31 / text 24 / markdown 22 / pdf 20，最慢 77 ms）；
     真实文件 `C:\Windows\Web` 28 张壁纸 100%、`target\real-samples` 7 个真实文档（3 个真 PDF + hosts/services 等）100%。
4. **A6.6 文档**：仓库 `README.md` 全量重写（1.0.0 版：支持格式表、安装、用法、性能表、插件、结构、验证命令、限制、许可），
   新增 `docs/screenshots/preview-{markdown,image,pdf,text}.png`（1190×748，走安装版 exe 截图）。
5. **A6.2 签名**：无证书，写 `docs/dev/RELEASE.md`（发布步骤、三处版本号、SmartScreen 表现与用户核对方式、将来 Azure Trusted Signing / signtool 步骤、发布检查清单、回滚）。
6. **收口 1.0.0**：版本三处同步（`Cargo.toml` 1.0.0、`qlrust.rc` `1,0,0,0` / `"1.0.0.0"`）、
   `scripts/pack.ps1 -Version 1.0.0` 产出 `dist\qlrust-1.0.0-win64`（含 `plugins\qlr_plugin_demo.dll`、`SHA256SUMS.txt`）+ zip **4,758,589 B**。

### 验证

- `cargo test` **116 passed**（app 18 / core 28 / platform 11 / plugin-api 4 / plugin-demo 7 / render 41 / malformed 7）、`fmt` 0、`clippy -D warnings` 0。
- `scripts/smoke.ps1` **17 PASS + 1 SKIP**（托盘 UIPI，提权会话固有限制）。
- `scripts/bench.ps1 -Rounds 1` 10/10 OK、exit 0：冷启动 4–5 ms；4K 35/69 ms；48MP 70/83 ms；26.6 MB 文本 18–30/37–50 ms。
- 样本矩阵 97/97 + 28/28 + 7/7 全 100%。
- 打包产物在**工作区外**实跑（`<桌面>\qlrust-1.0.0`）：
  `令牌完整性=high`、`托盘图标已添加 rect=(1683,1040)-(1707,1080)`、`插件就绪 loaded=1 extensions=8`、
  `SHOW kind=markdown renderable=true`、`STYLED 屏幕 30 行（h1=1 h2=2 h3=1 code=3 quote=1 list=6 rule=1 normal=15）`、`--quit` 后 `leftover: 0`。

### 踩坑（已进 CONVENTIONS #45–#48）

- `bench.ps1` 的 RENDER 正则漏 `ink=`；同文件二次投递是 Toggle（#45）。
- PowerShell 参数名与变量名大小写不敏感会撞车（#46）；测试夹具不能用 `nul.txt` 这类设备名（#47）。
- 带 BOM 的 UTF-16 不是二进制（#48，A6.7 抓到）。

### 状态

1.0.0 交付，工作树干净。M1–M6 全部收口（M3/M4 按用户要求改为插件形态，M5 收窄为 C ABI 骨架）。
后续方向见 `docs/dev/STATUS.md` §6（插件生态、代码签名、UIA 兜底、折行质量、人工确认 M1 Gate）。

---

## 1.0.1 — 用户实测三缺陷修复（2026-10-31）

用户装上 1.0.0 后实测报了三件事，逐条定位到真身并修掉。

### 用户原话

1. 「文本按上下的时候会这样，翻页逻辑好像不对」+ 截图（两屏文字叠成一片糊）
2. 「会抢文件的打开方式，直接变成预览，正确的逻辑不应该抢文件的打开方式，只在调用的时候按空格预览」
3. 「我需要按原文件尺寸自适应打开大小」

### 1. 文本滚动重影（+ 两个连带 bug）

**真身**：`wndproc` 的 `WM_ERASEBKGND => LRESULT(1)` 表示"背景我处理"，`window::invalidate()` 走
`InvalidateRect(Some(hwnd), None, false)`（`bErase=false`）表示"别擦"，而 `paint_styled` 里
`SetBkMode(TRANSPARENT)` 只画字不刷底 ⇒ **没人刷底**，滚动后旧字形留在客户区叠成糊。
另外行起点是"内容垂直居中"，行数一变整屏上下平移，看起来像错位。

- `crates/qlr-platform/src/window.rs`：`paint_styled` 每次先
  `FillRect(hdc, &rc, CreateSolidBrush(COLORREF(TEXT_BACKGROUND_RGB)))` 再画；行起点改 `rc.top + TEXT_PADDING_TOP`
  （新增 `TEXT_BACKGROUND_RGB = BITMAP_BACKGROUND_RGB`、`TEXT_PADDING_TOP = 16`）。
- **连带 bug A**：`WM_QLR_WHEEL` 直接 `wheel_zoom(delta)`，绕过"文本先滚" ⇒ 鼠标滚轮在文本上变成缩放重渲染。
  新增 `fn on_wheel(delta)`（`wheel_notches` → `scroll_text_by` → 滚不动才 `wheel_zoom`），钩子与 CLI 两条入口统一。
- **连带 bug B**：`show_preview` 不重置 `zoom`/`pan` ⇒ 新文件继承上一个文件的裁剪。改为开头 `s.zoom = 1.0; s.pan = (0,0);`。
- 新增 CLI `--scroll <n>`（`Action::Scroll { delta }`）以便脚本化验证滚动。

### 2. 安装器不再抢打开方式（`scripts/install.ps1` / `uninstall.ps1` 重写）

**根因**（注册表实证）：`HKCU\Software\Classes\.<ext>` 默认值**优先于 HKLM**（实测 `HKCU\.md=Markdown` 盖住
`HKLM\.md=Nutstore.LightApp.md`），旧 `install.ps1` 的 `-Default` 分支正往那里写 `QLRust.File`；
而默认安装就无条件写 ProgID + `Applications\qlrust.exe` + **123 个扩展名的 `OpenWithProgids`**。
写 `.qlrtest` 默认值后 ShellExecute 一键复现：qlrust 直接起来渲染预览（日志 `SHOW … kind=text`）。

- 默认安装 **零关联**：只复制文件、开始菜单 lnk、卸载项、可选 `-Autostart`。
- `-Associate` 才登记候选（ProgID + 123 个 `OpenWithProgids` + Capabilities/RegisteredApplications，**不写默认值**）。
- `-Default` 必须配合 `-Associate`，先备份原 ProgID 到 `HKCU\Software\QLRust\DefaultBackup` 再覆盖。
- 每次安装先跑幂等 `Clear-LegacyAssoc` ⇒ **重跑安装器就能修好被旧版抢过的机器**；装完自检，命中就 `throw`（不假成功）。
- 卸载：只删属于自己那条 `OpenWithProgids`，默认值按备份恢复/无备份则 `DeleteValue('', $false)` 删除，
  结尾提示 UserChoice 有哈希保护、需用户去"设置→默认应用"改回。
- 顺手修：旧 `uninstall.ps1` 是 UTF-8 **无 BOM**，被 `powershell.exe`（PS 5.1 按 ANSI 读）解析报 5 个错，
  控制面板"卸载"本就坏的 ⇒ 两个脚本改 **UTF-8 with BOM** + UninstallString 带 `-ExecutionPolicy Bypass`。

### 3. 按原文件尺寸开窗

- `qlr-render/src/image_view.rs` 新增 `pub fn dimensions(path) -> Option<(u32,u32)>`（只读头部）；
  `qlr-render/src/pdf.rs` 新增 `pub fn page_size(path, page_index) -> Result<(f64,f64)>`（point，×96/72 换 px）；
  `qlr-render/src/lib.rs` 新增 `pub fn natural_size(path, kind) -> Option<(u32,u32)>`。
- `qlr-platform/src/window.rs` 新增 `pub fn centered_rect_px(work: RECT, width: i32, height: i32) -> RECT`
  （夹在 `[240×160, 工作区×0.92]`，居中）。
- `app.rs::show_preview`：有自然尺寸按它开窗，没有（文本/Markdown/插件占位）退回比例；日志 `开窗尺寸 WxH（原始 WxH）`。
- `apply_material` 第三参改名 `opaque_content`，文本也算不透明 ⇒ 不再无谓开亚克力（亚克力会让文字发灰）。

### 验证

- **重影判据**：滚动一圈再滚回来，截图 `Ink` 像素精确回到原值 —— 15100 → 15183 → 15283 → **15100**；
  日志 `SCROLL 0 -> 12` → `12 -> 24` → `24 -> 0`（40 行源文件折行后 80 行，可见 30 行）。若旧字形残留，`Ink` 会单调累加。
- 开窗：`png_1920x1080.png` → `1766x956（原始 1920x1080）`；`pdf_8page.pdf` → `793x956（原始 793x1123）`；
  `8mp_3840x2160.jpg` → `1766x956`；`note.md` → 比例 `1190x748`。缩放仍正常（`ZOOM 1.00 -> 1.25 render=991x1195`）。
- 滚轮 12 次（`wheels=12`）在文本上不再出现 `ZOOM`。
- 关联：卸载摘 123 条 `OpenWithProgids` + 2 条 MRU、注册表/目录全清；默认安装"默认值命中 0 / UserChoice 92 项前后一致"；
  `-Associate` 只进候选；恢复机制三种残留（有备份→恢复、无备份→删、别人的→原样保留）全对；
  双击链路 .jpg→照片、.png→PhotoViewer、.pdf→Chrome、.md→Obsidian、.txt→记事本。
- 关卡：`cargo test` **120 passed**（app 18 / core 28 / platform 14 / plugin-api 4 / plugin-demo 7 / render 42 / malformed 7）、`fmt` 0、`clippy -D warnings` 0。

### 踩坑（已进 CONVENTIONS #49–#51）

- `WM_ERASEBKGND => 1` + `InvalidateRect(bErase=false)` = 谁也不刷底，文字滚动必重影（#49）。
- 同一个 UI 动作有两条入口（钩子直投 vs CLI）时，逻辑必须汇到一个函数，否则自动化测不到真路（#50）。
- `HKCU` 扩展名默认值优先于 `HKLM`；"只登记候选"也算抢；PS 5.1 按 ANSI 读脚本，非 BOM 就坏（#51）。

### 遗留（待用户点头）

- 47 个扩展名的 `HKCU\Software\Classes\.<ext>` 默认值是**空字符串**（第三方关联工具留下的，QLRust 从未支持 `.7z/.rar/.cr2` 等），
  会屏蔽 HKLM 的 `jpegfile`/`pngfile` 登记。按"别动别人的"原则**未清**。
- M1 的"真实手工按空格 / 托盘图标可见性"仍待人工确认（自动化注入键被钩子刻意过滤）。

---

## 1.0.2 — 窗口跟着内容与缩放走（用户反馈第二轮）

用户两条反馈（原文）：

> 左右的黑边还是很多，没有做到窗口自适应，而且窗口最大是有限的

> 窗口跟随图片的缩放而缩放

### 修了什么

| 问题 | 根因 | 修法 |
| --- | --- | --- |
| 竖图开成横窗口、左右大片黑边 | `centered_rect_px` 把宽、高**各自独立**夹到 `工作区 × 0.92`，不保持宽高比：1279×1706 → 1279×956，位图却是等比适配的 717×956 | 删掉 `centered_rect_px`，新增 `window::natural_rect`：统一 scale 一次套到宽高，比工作区大才缩，**用满工作区**（去掉 8% 余量） |
| "窗口最大是有限的" | 同上，0.92 的硬上限 | 上项已去掉；窗口现在可以顶到工作区边缘 |
| 缩放时窗口不动，只有画面被裁 | `set_zoom_to` 只改渲染分辨率，窗口尺寸不变 | 新增 `window::zoomed_rect` + `State.base`（开窗尺寸即 zoom 基准）：窗口跟着缩放等比变大变小，塞不进工作区时才夹住（此时位图比窗口大，可平移看局部） |
| 高倍缩放内存会爆 | 位图 = 基准 × zoom 线性增长（8× 的 12 MP 图 = 768 MP） | `request_render` 统一夹 `MAX_RENDER_PIXELS = 32_000_000`（超了等比缩回），宁可放大后略发虚也不能吃爆内存 |
| 真实空格/鼠标路径上报 `elapsed=27050ms` | `request_started` 只在命令行请求里重置 | 新增 `begin_request()`，`apply_request` 与 `on_hotkey` 各调一次 |

### 关键设计：`zoom = 1.0` 是"适应窗口"，不是"100% 原始像素"

`State.base` 存**开窗尺寸**而不是自然尺寸。拿自然尺寸当基准的话，`0.8×` 对 1279×1706 的图仍然是 1023×1365（超过工作区高度）→ 窗口不变、位图变大 → 用户看到"缩小反而裁掉一圈"。改成基准 × zoom 之后：0.8× 就是开窗尺寸的 0.8 倍，缩小一定是在缩小窗口。

### 实测（Win10 19045 / 1920×1080 / 工作区 1920×1040）

| 内容 | 1.00× | 0.80× | 1.25× |
| --- | --- | --- | --- |
| 800×1200 竖图 | 窗口 693×1040，位图 693×1040（零黑边） | 窗口 554×832，位图 554×832 | 窗口夹到 693×1040，位图 866×1300（可平移看局部） |
| 1920×1080 横图 | 1849×1040 | 1479×832 | 夹到 1849×1040 |
| PDF（793×1123 pt） | 734×1040 | 587×832 | — |
| 文本 / Markdown | 1190×748（无自然尺寸，不跟缩放：文本滚动按行走） | 不变 | 不变 |

关卡：`cargo test` **121 passed**（platform 16）、`fmt` 0、`clippy -D warnings` 0。

### 踩坑

- **演示截图别用用户给的文件**：这次用户发来的图是身份证照片，脚本一截图就把敏感信息落到了 `_bench/tmp`。已当场删除，之后一律用自己生成的合成夹具（`portrait-800x1200.png`）做截图与演示。
- `Add-Type` 定义的类型**不跨 pwsh 进程**：测量窗口尺寸的辅助类必须在同一个调用里定义+使用，否则报 `找不到类型 [W]`。
- 覆盖 `qlrust-dev\qlrust.exe` 前必须 `Get-Process qlrust | % { $_.Kill() }`，否则 `being used by another process`，然后你会**对着旧 exe 测新行为**（这次差点把旧行为当成新行为写进文档）。
### 补记：缩放要"真的变大"，不能只是裁剪

第一版实现完后实测发现 `zoom > 1` 只是把画面裁掉一块——因为 `image_view` 的 `fit_within` 里
`scale.min(1.0)` **永不放大**，请求 866×1300 却只拿回 800×1200 的原图。
于是补了 `RenderRequest.upscale` 这个开关：

- 默认 `false`（预览一张 32×32 图标不该被糊成整屏）；
- `State.zoom > 1` 时 `RenderJob.upscale = true`，走 `render_file_at_zoom(...)` → `image_view::render_scaled(..., upscale)` → `fit_within_opt(..., upscale)`；
- 实测放大链路：位图 693×1039 → 866×1299 → 1083×1624 → 1353×2030（对应 1.00 / 1.25 / 1.56 / 1.95×），
  窗口始终夹在 693×1040，平移 `--pan 0 4` → `PAN (0, 344)` → `PAN (0, 688)`，截图确认看到的是画面另一段而不是重复内容；
- 内存由 `MAX_RENDER_PIXELS = 32 MP` 兜住（1.95× 时 1353×2030 ≈ 11 MB）。
## 1.0.3 — 鼠标交互重做：左键拖动、右键缩放、滚轮滚动

用户原话（两条，连着给的）：
> 「删掉鼠标左键关闭的功能，以后用左键来拖动，来解决文件长的翻页下滑问题。在pdf里，滚轮用来上下滑动」
> 「pdf的缩放可以用鼠标右键拖动来实现」

### 做了什么

| 输入 | 之前 | 现在 |
|---|---|---|
| 左键单击 | 关闭预览 | **什么都不做**（`hide_on_click` 配置项一并删除） |
| 左键拖动 | 图片/PDF 平移；文本无效果 | 图片/PDF 平移；**文本按行滚动**（像素攒够一行才滚） |
| 右键上下拖 | 无 | **缩放**：往上拖放大、往下拖缩小，60 px 一档 1.25×，变化 <2% 不动 |
| 滚轮 | 文本 3 行；其它一律缩放 | 文本 3 行；**PDF 上下滑动**（一格 1/4 屏，到边继续滚 = 翻页）；图片放得下缩放、放大后滑动 |

### 代码落点

- `crates/qlr-app/src/app.rs`
  - `State` 加 `drag_residual: i32`（拖拽滚动攒的零头像素）、`zoom_anchor: Option<(i32, f32)>`（右键缩放锚点）；
  - 删 `on_preview_click`；`on_drag_end()` 只收尾（并去掉 `hwnd` 参数）；
  - 新增 `scroll_text_by_pixels` / `pixels_to_rows`（纯函数，可单测）、`on_zoom_drag_start/_move/_end`、`zoom_drag_target`（纯函数）、`on_mouse_move`（右键优先分流）；
  - `on_wheel` 重写 + 新增 `scroll_page_by`（返回 `(动了没, 顶到边界没)`）与 `bitmap_height`；
  - `step_page` 换页时 `pan` 归零（滚到页底翻页后从新页顶端看起）。
- `crates/qlr-core/src/config.rs`：删 `hide_on_click` 字段、默认值与 `summary()` 里的输出。
- `wndproc`：补 `WM_RBUTTONDOWN`/`WM_RBUTTONUP` 两臂，`WM_MOUSEMOVE` 改走 `on_mouse_move`。

### 验证（`_bench/verify-input.ps1`，真鼠标事件注入 + 日志断言）

| 断言 | 结果 |
|---|---|
| 左键单击后窗口仍在、日志无 `HIDE` | ✅ |
| 左键拖动 200 行文本 → `SCROLL 0→1→2→3→4→5` | ✅ |
| 滚轮在文本上 → `SCROLL` 且**无** `ZOOM` | ✅ |
| 右键上拖 150 px → `ZOOM 1.00→…→1.75`、渲染 725×1087→1211×1817 | ✅ |
| 放大后滚轮 → `VSCROLL pan.y 0 -> 260（内容 1816px / 窗口 1040px）` | ✅ |
| PDF 滚轮 → `PAGE 2/8` | ✅ |
| PDF 放大后滚轮 → 先页内 `VSCROLL`，到底继续滚 → `PAGE 3/8` | ✅ |

关卡：`cargo test` **124 passed**（app 20 / core 28 / platform 16 / render 42 + 7 malformed + plugin-api 4 + plugin-demo 7）、
`cargo fmt` 0、`cargo clippy --all-targets -- -D warnings` 0。

### 踩到的坑

- 漏 import `WM_RBUTTONDOWN` → match 里变成绑定模式，`unreachable pattern` 警告 + 右键完全不响应（CONVENTIONS #56）。
- 拖拽滚动必须攒像素零头，否则慢速拖动一行都不滚（CONVENTIONS #55）。
- 删配置字段要连 `summary()` 的格式化参数一起删，否则 `5 positional arguments but 4 arguments`。

## 1.0.4（2026-10-31）拖动改成挪窗口、滚轮方向修正

用户原话：「鼠标左键拖动对图片来说，应该是整个窗口在桌面的移动，不然只是针对里边的图片的话，会频闪。pdf的右键拖动同理。鼠标滚轮到文本/md的拖动放方向逻辑反了」

### 做了什么

| 改动 | 说明 |
| --- | --- |
| 左键拖动 = 挪窗口 | `crates/qlr-platform/src/window.rs` 新增 `move_by(hwnd, dx, dy)`（`SetWindowPos` 带 `SWP_NOSIZE/SWP_NOZORDER/SWP_NOACTIVATE`）；图片、PDF 拖动不再改 `pan`，拖动期间**零重绘**（没有 `RENDER`/`PAINT`/`PAN`，只有 `MOVE`） |
| 拖动坐标改屏幕坐标 | 新增 `window::cursor_pos()`（`GetCursorPos`）+ `app::drag_origin()`；`on_drag_start`/`on_drag_move`/`on_zoom_drag_*` 全部改用它。原来用 `WM_MOUSEMOVE` 的 client 坐标，鼠标走 90 px 窗口只走 27 px（自己挪窗口把坐标抵消了） |
| 滚轮方向修正 | `on_wheel` 改用纯函数 `wheel_text_rows(notches) = -notches * 3`；原来向上滚反而往下翻 |
| 右键拖动缩放不再抖 | 缩放锚点从 client y 改成屏幕 y；原来会 `ZOOM 1.05 -> 0.87 -> 1.17 -> 0.91 …` 来回跳 |
| 删 `pan_pixels` | 挪窗口后没人调用（clippy 会报 dead_code，直接删干净） |

### 验证（`_bench/verify-input-104.ps1`，真鼠标事件注入 + 位置/日志断言）

| 断言 | 结果 |
| --- | --- |
| 图片左键拖 90,70 → 窗口位移 `90,70`，无重绘 | ✅ |
| 文本左键拖动 → `SCROLL`、窗口位置不变 | ✅ |
| 滚轮向上 65→62、向下 62→65 | ✅ |
| PDF 左键拖 -80,50 → 窗口位移 `-80,50` | ✅ |
| PDF 右键上拖 150 px → `ZOOM 1.00 → … → 1.75`（单调） | ✅ |

关卡：`cargo test` 125 passed、`cargo fmt` 0、`cargo clippy --all-targets -- -D warnings` 0。
文档：`STATUS.md` §17、`CONVENTIONS.md` #57/#58、`ARCH.md` 鼠标契约表改成 1.0.4、`README.md` 与 `packaging/README.txt` 操作表同步。

### 踩到的坑

- 拖动时用 client 坐标 → 与窗口自身位移抵消，只走 1/3（#58）。
- 缩放锚点同理，用 client y 会抖成来回跳（#58）。
- 挪内容 = 每帧重画整张位图 = 频闪；挪窗口 = 零重绘（#57）。
## 1.0.5（2026-10-31）缩放保位、PDF 缩放不卡、文本窗宽固定、Markdown 真排版 + 行号

用户原话：「图片：移动后再缩放，会回到初始打开的位置，要调整到不回到初始打开的位置。pdf：右键缩放的时候卡顿比较强，按逻辑来说应该是缩放窗口大小的，现在是缩放文档，再调整窗口。文本/md：默认窗口有点大，宽度设置成一个合适的数值吧，上下左右再调整就行，另外，md的预览，需要是渲染后的格式，不能是源码格式，每行左边再加一个行号方便定位，鼠标左键也需要一个拖动整个窗口的功能」

### 做了什么

| 改动 | 说明 |
| --- | --- |
| 缩放不再跳回中央 | `crates/qlr-platform/src/window.rs` 新增 `resize_around_center(hwnd, w, h)`（以窗口自身中心摆新尺寸，再 `clamp_into` 推回工作区）；`set_zoom_to` 不再用按工作区居中的 `zoomed_rect` |
| PDF 缩放不卡 | 拖动期间只改窗口 + 旧位图**拉伸预览**（`window::paint_bitmap_scaled` + `State.zoom_preview`），松手（`on_zoom_drag_end`）补**一次**真渲染。实测一次拖动 12 个 ZOOM 帧只有 1 次 RENDER |
| 文本窗宽固定 | 新增 `window::text_rect(work, height_ratio)` + `TEXT_WINDOW_WIDTH = 880`：宽度固定、高度仍按比例；1080p 上从 1200×750 缩到 880×748 |
| Markdown 真排版 | `crates/qlr-render/src/markdown.rs` 新增 `inline()`：行内代码、`**`/`__`/`~~`/`*`/`_`、`[文字](地址)`/`![描述](地址)`、`\*` 转义、简单 HTML 标签一律去标记留文字；新增表格行（`| a | b |` → `a │ b`，分隔行画成横线，新样式 `LineStyle::Table`）与任务列表（`- [ ]` → `☐`、`- [x]` → `☑`） |
| 行号栏 | `window::StyledRow` 加 `number: Option<u32>`，`paint_styled` 在左侧画右对齐行号（`TEXT_GUTTER = 64` 固定宽）；折行续行不带行号 |
| 文本左键拖动 = 挪窗口 | `on_drag_move` 删掉文本滚动分支，统一 `move_window_by`；文本滚动改由滚轮 / **右键上下拖动** / PageUp·PageDown |
| 右键拖动在文本上 = 滚动 | `on_zoom_drag_start/move/end` 按内容类型分流（`State.scroll_anchor` vs `zoom_anchor`） |

### 验证（`_bench/verify-input-105.ps1`，真鼠标事件注入 + 窗口位置/日志断言）

| 断言 | 结果 |
| --- | --- |
| 图片先拖 120,80 再 `--zoom-in`：中心偏移 `0,0` px，窗口 1849×1040 → 2311×1300 | ✅ 不跳回中央 |
| PDF 右键上拖 120 px：12 个 `ZOOM` 帧、`（预览，松手再渲染）`、`RENDER` 仅 1 次、`ZOOM-DRAG 松手补渲染` 1 次 | ✅ 不卡 |
| 文本开窗尺寸 `880x748` | ✅ 固定宽度 |
| 文本左键拖 100,60 → 窗口位移 `100,60` 且**无** `SCROLL` | ✅ 拖窗口 |
| 文本右键拖 -140 px → 有 `SCROLL`、窗口位置不变 | ✅ 右键滚动 |
| Markdown：`STYLED 屏幕 15 行（h1=1 h2=1 code=1 list=2 rule=1 table=2 normal=7）`、`行号栏=64px` | ✅ 行号 + 表格 |

关卡：`cargo test` **132 passed**（app 21 / core 28 / platform 18 / render 47 + malformed 7 + plugin-api 4 / plugin-demo 7）、
`cargo fmt --check` 0、`cargo clippy --all-targets -- -D warnings` 0。

### 踩到的坑

- `strip_pairs` 朴素替换会把 `2 * 3 * 4` 当斜体、把 `snake_case_name` 当强调 → 加三条否决条件（中间不能是以空格开头的空内容、两侧不能紧贴 ASCII 字母数字）；CJK 不算"词内"（Rust 的 `is_alphanumeric()` 对汉字为真，直接用它会让 `这是**粗**体` 永远去不掉）。
- 转义必须在去标记**之前**锁住（`\*星号\*` → 私有区 `U+E02A`），否则转义符被当成成对分隔符吃掉。
- HTML 标签判定不能只看"有 `<` 有 `>`"：`#include <stdio.h>` 会被吃掉后半截 → 要求标签名是 `[A-Za-z][A-Za-z0-9-]*` 且紧跟 `>` 或空白。
- 行号栏宽度必须是**常数**（`TEXT_GUTTER = 64`）：动态按位数算会让"折行量宽度"和"GDI 实际绘制"各量一遍数字宽度，两者不一致就出现"折了行还被省略号吃掉"（CONVENTIONS #38 的同类坑）。

## 1.0.6（2026-10-31）回退上一版改动 + 真正消掉黑边 + 放大镜

用户原话：

> 我觉得需要回退到上一个版本，刚才的改动都没什么用，体验并没有变好，pdf的黑边依然存在，也别让拉伸窗口了。如果真的需要放大素材中的某部分细节，引入一个放大镜功能算了

### 先回退

`git revert f3b0b2e`（提交 `9b7b9d7`）：拖边改窗口大小、放大上限=`capped_size`、
`SNAP` 顶满、`COLORONCOLOR` 预览、`DRAG-FRAME` 日志全部回退，代码回到 1.0.5。

回退后重新量了一次"黑边"，终于看清它是什么：PDF 页开窗 `734x1040`，但渲染回来的位图是
`734x1039`（按页面宽高比取整），而 `paint_bitmap_at` 在"位图不大于客户区"时**居中**绘制
（`(cw - bw) / 2` 整数除法），于是**最后一行**留下 `#1A1A1A` 底色。截图实测：
`(5,1039) = 26,26,26`、`(5,1038) = 255,255,255` —— 就是一条 1 px 的深色细线。

### 修法与新增

| 改动 | 说明 |
| --- | --- |
| 黑边（真修） | `paint_bitmap_at` 在"位图不大于客户区"时把目标矩形设成**整个客户区**（`dw = cw`、`dh = ch`，源矩形仍是整张位图），差 1 px 的拉伸肉眼不可见；`paint_bitmap_scaled`（缩放预览）同样一律铺满。`StretchDIBits` 的源宽高改为显式 `sw/sh` |
| 放大镜 | 按住 **Shift** 就在光标处出现 220×220 的镜头（`StretchBlt` 放大屏幕像素 + `BitBlt` 贴回 + 两圈边框），松开即消失；**Shift + 滚轮** 调倍数 1.5×–8×（默认 3×）。只有位图内容（图片/PDF）才开——文本本来就是矢量字 |
| 平台层新增 | `window::shift_down()`（`GetAsyncKeyState(VK_SHIFT)`）、`window::LENS_RADIUS = 110`、`window::paint_lens(hwnd, center, zoom)` |

### 验证（`_bench/verify-lens.ps1`，10 PASS / 0 FAIL）

| 断言 | 结果 |
| --- | --- |
| A 8MP 图片窗口：底行/右列底色像素 | ✅ 各 0 个（回退前底行必有一段 `26,26,26`） |
| B 按住 Shift + 移动：`LENS 开 368x521 zoom=3.0×`，镜头上边框 `(224,224,224)` 命中 51 点 | ✅ 镜头出现 |
| C Shift + 滚轮两次：`LENS 倍数 4.0×` → `5.0×` | ✅ 倍数可调 |
| D 松开 Shift：`LENS 关`，边框残留 0 点 | ✅ 消失干净 |
| E PDF 开窗：底行底色 0 个；Shift 也能开镜头 | ✅ PDF 同样受益 |

截图：`docs/screenshots/preview-lens.png`（PDF 页 + 3× 镜头，734×1040）。

关卡：`cargo test` **132 passed**、`cargo fmt --check` 0、`cargo clippy --all-targets -- -D warnings` 0、
`scripts/smoke.ps1` 17 PASS + 1 SKIP。

### 踩到的坑

- 黑边**不是**"位图比窗口小很多"，就是 1 px：之前一直按"比例不符/留白"的方向找，找错了。
  正确的取证方式是**数边缘那一行/一列的像素值**，不要靠肉眼或按比例猜。
- 放大镜必须在内容之后、同一次 `WM_PAINT` 里画，且用 `GetDC`/`ReleaseDC`（第二次 `BeginPaint`
  会打乱更新区）。
- Shift 状态不能自己维护：预览窗是 `WS_EX_NOACTIVATE`，焦点可能在资源管理器上，键盘消息根本
  收不到；`GetAsyncKeyState` 才准。
## 2026-10-31 · 1.0.7 设置页 + 快捷键按类别分组 + 托盘菜单瘦身

### 用户原话

> 「把设置页完善一下，主要是快捷键相关的，按类别放好，你先做，我等会再调整，另外，右键显示预览那个东西可以删掉，没用」

### 做了什么

1. **删托盘「显示预览」项**。托盘右键菜单现在是：`设置…` / `关于` / `退出`。
   理由（也写进了 `tray.rs` 注释）：「显示预览」没有明确的"显示什么"（是上一个文件？选区？），
   点了常以为坏了。
2. **快捷键全部改成配置驱动**：新模块 `crates/qlr-core/src/keymap.rs`
   - `Action`（8 个：打开/关闭预览、关闭预览、下一页、上一页、向下/向上滚动一行、放大一档、缩小一档）
     + `Category`（全局 / 浏览），每个 Action 带 `label()` / `key()`（配置字段名）/ `default_binding()` / `canonical_vk()`。
   - `Binding { vk, ctrl, alt, shift }`：`parse("Ctrl+Space")` / `text()` / `pack()` / `unpack()` /
     `matches(vk, ctrl, alt, shift)`（**修饰键要求完全相等**，不是包含）。
   - `Hotkeys`（9 个字段：8 个 Action + `lens_hold` 放大镜按住键）进 `Config.hotkeys`，
     `#[serde(default)]` 让老配置自动补默认，`sanitized()` 把坏值修回默认。
3. **键盘钩子按绑定表匹配**（`crates/qlr-platform/src/hotkey.rs`）：`TRIGGER_VK` / `NAV_KEYS` 两张硬编码表
   换成 `static BINDINGS: [AtomicU32; Action::COUNT]`，`set_bindings()` 可热更新；
   `start()` 不再接 `trigger_vk` 参数。
4. **设置窗口**（新文件 `crates/qlr-app/src/settings.rs`，约 520 行）：
   普通可激活窗口（`WS_CAPTION | WS_SYSMENU`，与预览窗的 `WS_EX_NOACTIVATE` 相反），
   行布局按类别分组：全局 / 浏览（预览打开时）/ 放大镜 / 鼠标（固定，不能改键）。
   点一行 → 直接按新键（`Backspace` 恢复默认）→ `Enter` 保存 / `Esc` 取消；
   冲突组合会提示"这个组合已给「XX」了"。
5. **保存即热更新**：设置窗自己写 `config.json` → `PostMessage(host, WM_QLR_SETTINGS_SAVED)` →
   宿主 `Config::load` → `hotkey::set_bindings` + 更新 `State.config`，不用重启。
6. 新增 CLI `--settings`（等同托盘「设置…」），放大镜按住键也读配置（默认仍是 Shift）。

### 怎么验证的

关卡：`cargo test` **151 passed**（core 26 / app 42 / platform 18 / plugin-api 4 / plugin-demo 7 / render 47 / malformed 7）、
`cargo fmt --check` 0、`cargo clippy --all-targets -- -D warnings` 0、`scripts/smoke.ps1` 17 PASS + 1 SKIP。

端到端 `_bench/verify-settings.ps1` **10 PASS / 0 FAIL**（截图 `_bench/settings-107.png`，636×640）：
`--settings` 起窗 → 点"打开/关闭预览"那行 → 按 `F9` → `Enter` 保存 →
`config.json` 里 `"toggle":"F9"`、日志 `改键 打开 / 关闭预览 = F9` / `设置已保存` /
`按键绑定：打开 / 关闭预览=F9` / `配置已热更新`，设置窗自动关闭。

### 踩到的坑

- **`--settings` 一开始静默无效**：`run()` 里的"启动是否立刻出窗"只认 `path.is_some()` 与
  `Action::Show`，其它请求当作"纯后台启动"丢掉。加 `Action::Settings` 才通。
  → 教训：**新增一个"启动时就想看见效果"的命令，必须同步 startup_shows 判定**。
- 设置窗布局常量（行高 28 / 顶部 18 / 左右 28）必须渲染层与命中测试**共用同一份**，
  否则点一行会改到上一行/下一行的按键。`SETTINGS_ROW_HEIGHT` 等常量放在
  `qlr-platform/src/window.rs`，`settings.rs` 的 `layout()` 与 `on_click()` 都引用它。
- 钩子回调里**不要写日志**：`WH_KEYBOARD_LL` 回调有 300 ms 硬预算，累计超时会被系统摘钩。
  所以绑定表变化时在 `set_bindings()` 里打一次全量 `按键绑定：…`。
- `Action` 这个名字在 `qlr-app` 里有两份（`qlr_core::ipc::Action` 与 `qlr_core::keymap::Action`），
  混用报 `expected keymap::Action, found ipc::Action` → 一律写全路径。
- PowerShell 里 `Add-Type` 的 enum delegate 不能可靠捕获 `$script:` 变量：找窗口改成
  C# 静态方法 `FindVisibleByClass`（`EnumWindows` + `GetClassName`），一次就对。


## 1.0.8 — 白色主题 + 代码高亮 + PDF 页码条（用户 8 条反馈）

用户原话（2026-10-31）：

> 1、打开图片的时候，默认的窗口大小是怎么来的？感觉打开的时候屏幕撑的有点满了。
> 2、图片的右键缩放可以取消掉
> 3、pdf缩小的时候，画质会糊掉，且翻页有频闪，偶有空格失效的情况（无法针对到预览窗口）
> 4、md和文本最后一行显示不全
> 5、pdf上方像md和文本一样，新增一个显示页数的东西
> 6、代码文件预览的时候，仿照vscode进行显示优化
> 7、md文件的预览可以再优化一下
> 8、所有文件默认打开，底色都是白色，其他部分做相应变动

### 做了什么

| # | 问题 | 根因 / 做法 |
|---|---|---|
| 1 | 图片开窗撑满屏幕 | `natural_rect` 原来用满工作区（1.0.2 的"用满"）。加 `NATURAL_FILL = 0.86`，可用区 = 工作区 × 0.86，等比缩放后居中；1920×1080 工作区上 1920×1080 的图开成 **1589×894** |
| 2 | 图片右键缩放 | `on_zoom_drag_start` 只对 PDF 生效（`s.pages.is_some()`），其余格式直接 `release_capture()` 返回。图片放大仍可用滚轮 |
| 3a | PDF 缩小糊 | `pdf.rs` 缩小态**超采样 2 倍再盒式缩小**（`SUPERSAMPLE=2.0`、预算 `24 MP`），`render_page` 末尾 `downscale_bgra` + `image::imageops::thumbnail` |
| 3b | 翻页频闪 | `step_page` 原来立刻 `set_lines(占位) + invalidate`，新位图回来前先把窗口换成文字。改成只 `request_render`，旧页留屏直到新帧 |
| 3c | 空格偶失效 | 钩子的 Toggle/Close 分支只认 shell 类前台。新增 `foreground_is_ours()`（`GetWindowThreadProcessId` == `GetCurrentProcessId`），并把 `Shell_TrayWnd`/`MultitaskingViewFrame` 等补进白名单 |
| 4 | 文本/MD 最后一行被裁 | `visible_rows` 忘了扣 `TEXT_PADDING_TOP`：748 高窗口 = 16 + 31×24 = 760 > 748。改成 `((h - 16) / 24) - 1`，现在 29 行正文 + 1 行状态行，留 12 px 余量 |
| 5 | PDF 顶部页码 | 平台层新增 `HEADER_HEIGHT = 26` + `paint_header()`（白底 + 1 px 分隔线），`paint_bitmap_at/scaled` 多一个 `header` 参数；文字 `PDF · 第 n / m 页（滚轮或 PageDown 翻页）`。`view_size`/`set_zoom_to`/`show_preview` 的"内容高"都扣掉这一条 |
| 6 | 代码预览仿 VSCode | 新增 `qlr-render/src/code.rs`（等宽 + 行号 + 词法着色：关键字/字符串/注释/数字；`//`、`#`、`--`、`/* */` 按语言判定；**不折行**）；平台层新增 `paint_doc` + `DocRow`/`Span`（一行多段样式、逐段量宽推进 x）；渲染层 `canvas::Token`/`Span` + `TextDoc.spans` |
| 7 | Markdown 再优化 | 代码块 `#F6F8FA`、引用块 `#F7F7F7` 整行底色（`StyledRow.bg`），标题/表格/行号全套浅色主题配色 |
| 8 | 全局白底 | `BITMAP_BACKGROUND_RGB` 改成 `0x00FF_FFFF`（`TEXT_BACKGROUND_RGB` 跟随），`TextStyle` 的 14 种颜色全部重排成浅色主题（COLORREF = `0x00BBGGRR`），设置页高亮改 `0x00E8E8E8` |

顺带：`keymap` 的 `+`/`=` 是同一个物理键（`VK_OEM_PLUS`），显示统一成 `+`，日志里不再出现 `zoom==/-` 这种怪样。

### 怎么验证的

- 关卡：`cargo test` **158 passed**（core 42 / app 42 / platform 18 / plugin-api 4 / plugin-demo 7 /
  render 54 / malformed 7）、`cargo fmt` 0、`cargo clippy --all-targets -- -D warnings` 0、
  `scripts/smoke.ps1` 17 PASS + 1 SKIP。
- 实机（`<夹具目录>`）：
  - 代码 `demo.rs`（本仓 `code.rs`）→ `CODE 屏幕 29 行 token 段：plain=54 keyword=9 str=40 comment=17`，
    截图确认白底 + 行号 + 蓝色关键字 + 绿色注释 + 棕色字符串（`_bench/v108-code.png`）；
  - 数字着色 `demo-num.rs` → `number=4`；
  - PDF → `开窗尺寸 631x920（原始 793x1123）`、`RENDER bitmap 631x893`、顶部 `PDF · 第 1 / 8 页`；
    `--next-page` 两次 → `PAGE 4/8` → `PAGE 5/8`，翻页期间日志里**没有**占位文字写入；
  - 缩小到 0.64 → `RENDER bitmap 404x572`（内部 808×1144 超采样后缩），截图文字仍清晰；
  - 图片 `png_1920x1080.png` → `开窗尺寸 1589x894（原始 1920x1080）`；
  - `lines-200.txt` → 第 29 行完整可见（截图 `_bench/v108-text.png`）；
  - 图片右键拖动 120 px → 窗口尺寸不变、日志零 `ZOOM`（`_bench/verify-rclick.ps1` PASS）；
  - `demo.md` → `STYLED 屏幕 29 行（h1=1 h2=2 code=4 quote=1 list=4 rule=1 table=2 normal=14）`，
    代码块与引用块有底色（`_bench/v108-md.png`）。

### 踩到的坑

- **浅色主题把老的判色脚本全废了**：`_bench/shot.ps1` 统计"偏离 `0x1A1A1A` 的像素数"，白底下整屏都算"墨"，
  指标直接失去意义。判色类断言要么改成"取背景像素再比"，要么改用日志（CONVENTIONS #69）。
- **`visible_rows` 少扣一次 padding**：行数多算 1 行，绘制越界被窗口裁掉——不报错、不崩溃，
  只是"最后一行看不见"，属于最容易被忽略的一类（CONVENTIONS #70）。
- **翻页写占位 = 自找频闪**：任何"新内容还没到就先 `invalidate`"都会闪一下（CONVENTIONS #71）。
- 代码一行多段：`DrawTextW` 只能画一段，必须自己 `GetTextExtentPoint32W` 量宽推 x，
  且要**沿用同一份字体选择**，否则段与段之间会出现 1–2 px 的缝（CONVENTIONS #73）。
- 热键判定要看"是不是自己进程的前台窗口"，否则设置窗一开，空格就不响应（CONVENTIONS #74）。

### 补记：设置页（1.0.7 遗留）在白色主题下的两处修正

换白底后重拍设置页截图，暴露两个 1.0.7 就存在、但深色底不明显的问题：

- **底部提示行被裁掉半截**：`create()` 用 `layout().len()` 算客户区高度，而 `paint()` 实际会多画
  一个空行 + 一行提示（共 `+2` 行）。现在抽出 `EXTRA_ROWS = 2` 与 `client_height()`，
  窗口高度和绘制行数同源，并补单测 `client_height_covers_every_painted_row` 钉住这条不变量
  （CONVENTIONS #75）。
- **说明文字被空着的值列挤掉半句**：`paint_settings` 左列右边界固定留 180 px 给值列，
  分类标题/说明行没有值却照留。改成"右值为空就把整行让给左列"，`按住它移动鼠标 = 放大镜；按住它滚轮 = 调整倍数（1.5×–8×）`
  现在能整句显示。
- 提示行原来用 `TextStyle::Rule`（浅灰分隔线色，白底下几乎看不见），改用 `Quote`（深灰）。

截图也换了抓法：`_bench/shot3.ps1` 用 `PrintWindow(PW_RENDERFULLCONTENT)` 直接抓窗口内容——
预览窗/设置窗随时可能被别的程序盖住，`CopyFromScreen` 会把**别人屏幕上的内容**截进来
（第一次重拍就截到一段视频），既是错误截图也是隐私事故（CONVENTIONS #76）。

---

## 1.0.9 — 统一顶栏 + Markdown 目录 + 页内跳转 + 滚动条（用户反馈第九轮）

用户原话（含 9 件事）：

> pdf鼠标右键放大的逻辑还是不太对，pdf先关闭右键缩放的逻辑。然后md文件，左边加个目录页,如图。
> 现在预览上方已经有顶栏了，pdf在顶栏页面那里修改成可修改形式，可手动输入跳转页面。
> 顶栏统一设置成黑色底色方便识别，页数那里精简下，1/100,1那里可输入即可，快捷提醒可去掉。
> 顶栏再新增一个在其他应用打开的按钮，效果是跳转到其他页面打开，鼠标放上去提示在其他应用打开。
> 再新增一个关闭的按钮在最右边，作为备用。图片初始预览给一个设置的选项，默认50%。
> 顶栏行为各格式统一。提供预览文件类别支持的插件作用域只在窗口内，顶栏属于底层设置。
> 另外文本和pdf窗口右侧设置一个滚动栏，如果预览页比较长，可以用这个滚动栏滚动快速查找所需内容

### 做了什么

| # | 需求 | 落点 |
|---|------|------|
| 1 | 关掉 PDF 右键缩放 | `app.rs::on_zoom_drag_start` 非文本分支一律 `release_capture()` 返回（`zoom_anchor` 链路保留但不可达） |
| 2 | Markdown 左侧目录 | `qlr_render::markdown::outline()` 产 `TocEntry{level,text,line}`；`window::paint_toc`/`toc_item_at`；点一下滚到那行，**高亮跟着滚动走**（`toc_following()`） |
| 3 | 顶栏统一黑底 | `window::Chrome`/`ChromeButton`/`paint_chrome`，四种格式走同一条绘制路径 |
| 4 | 页码可点可输入 | `window::PageBox{current,total,editing}`；点页码框进输入态，`hotkey::set_capture_digits(true)` 借钩子收数字（预览窗 `WS_NOACTIVATE` 拿不到焦点）；显示精简成 `1 / 8`，去掉"滚轮或 PageDown 翻页"提示 |
| 5 | 在其他应用打开 | `window::open_external()`（`ShellExecuteW`）；悬停提示同名文字 |
| 6 | 最右关闭按钮 | `ChromeButton::Close` → `hide_preview()` |
| 7 | 图片初始 50% | `Config::image_zoom_percent`（默认 50，范围 10–400）+ `window::natural_rect_scaled()`；设置页新增"图片初始缩放"行 |
| 8 | 插件只在顶栏之下 | 所有位图绘制入口（`paint_bitmap_at`/`paint_bitmap_scaled`）都接 `Option<&Chrome>`，内容区自带扣掉 30 px |
| 9 | 右侧滚动条 | `window::ScrollInfo`/`scrollbar_rect`/`paint_scrollbar`/`scroll_start_at`；文本按行、PDF 按页；支持点击轨道与拖动滑块 |

### 设计要点

- 顶栏是"底层家具"：`paint_doc`/`paint_styled`/`paint_text`/位图入口都先画顶栏、再画内容，
  内容矩形由 `chrome_rects()` 统一算，**任何格式都不能自己另起一套**。
- 目录栏与滚动条用 `GetDC` 单独画，必须在正文之后（一次 `BeginPaint` 只能有一个）。
- 目录坐标用"显示行"（`TextDoc::lines` 的下标），点目录→设 `scroll` 一步到位，不需要二次映射。

### 验证

- 新脚本 `_bench/verify-input-109.ps1` → **PASS=9 FAIL=0**：PDF 打开 / 点页码框进输入 / 输入 5 回车
  `PAGE 5/8（跳转）` / 点顶栏 × 关窗 / Markdown 目录生成 / 点目录跳转 / 源码着色 / 图片 50% 开窗 795×477 /
  PDF 右键不再缩放。
- `_bench/verify-109b.ps1` → **PASS=2 FAIL=0**：滚动后目录高亮跟随（截图 `md-mid.png` 高亮落在"2. 进程与线程模型"）；
  点 ↗ → 日志 `在其他应用打开：… note.md ok=true`（记事本被拉起，随后关掉）。
- 滚动条：拖到底后内容滚到 `── 第 362–383 行 / 共 392 行 ──`，像素扫描确认滑块 y 694–729。
- 关卡：`cargo test` **160 passed**（app 28 / core 42 / platform 18 / plugin-api 4 / plugin-demo 7 / render 54 / malformed 7）、
  `cargo fmt --check` 0、`cargo clippy --all-targets -- -D warnings` 0、release exe **1,880,064 B**。

### 踩到的坑（已进 CONVENTIONS #77–#81）

- 目录栏画在内容之上，而 `paint_styled` 当时把 content 写死成 `None` ⇒ 正文从 `x=0` 起画，**左半被目录盖住**。
- 滚动条/目录不能用 `BeginPaint`（一个窗口一次 paint 只能有一个），必须 `GetDC`，因此必须"先正文、后叠层"。
- 页码输入只能借键盘钩子（`WS_EX_NOACTIVATE`）；钩子里的输入分支必须放在"过滤注入键"**之前**，
  否则自动化测不进输入态（真人能进，测试脚本永远进不去）。
- 目录高亮如果只靠鼠标悬停/点击，滚动时"目录指着别处"，读起来别扭 ⇒ 高亮要跟滚动位置走。
- 数字输入型设置项（10–400%）要有范围提示，否则用户敲 1000 只会"看不见反应"。

## 1.0.10 — 不支持的文件只响铃、拖边改大小回来了、柔和投影、跟随选中项、顶栏可拖、阻止关闭

用户原话（顺序反馈）：

> 1、预览不支持的文件时，不要有任何预览，系统直接响铃报错即可，当前预览不支持的文件这个底稿这些可以删掉了
> 2、把窗口伸缩那块加回来吧
> 3、窗口周围新增一圈灰色阴影，方便边边和其他程序的白色区分
> 4、预览窗口打开的时候，预览文件随鼠标选中的文件而变化
> 5、鼠标左键拖拽窗口的作用域增加到顶栏上，达成拖顶栏也能变动位置的目的
> 6、顶栏新增一个阻止关闭的按钮，逻辑是选中后当前预览窗口不能被空格关闭，并且不能取消阻止关闭

随后对第 3 条追加视觉要求：「是类似这样的边框阴影，你的边框看起来怪怪的」（附网页 `box-shadow` 截图）
⇒ 第一版做的"三圈灰边"被否，改成**独立分层窗口画的柔和投影 + 1 px 发丝边**。

### 六条分别怎么落地

| 需求 | 做法 | 关键点 |
|---|---|---|
| 1 不支持的文件不画任何东西 | `show_preview` 里 `renderable == false` 就 `beep()` + `log_info!("SHOW 拒绝 …")` + （若已可见）`hide_preview()` 并 `return` | 占位文案（`build_lines` 的 unsupported 分支、插件目录行）整段删掉；`MessageBeep` 不在 windows crate，用 `PlaySoundW("SystemHand")` |
| 2 拖边改窗口大小 | 平台层补 `resize_to` / `window_rect` / `CursorKind` + `set_cursor`；app 层 `ResizeEdges` + `edges_at`（6 px 热区）+ `begin_resize` / `on_resize_move` / `finish_resize` + `WM_SETCURSOR` | 位移用"按下时原始矩形 + 累计位移"，不用增量累加；松手后按新客户区重渲染 |
| 3 窗口外一圈阴影 | 独立 `QLRust.ShadowWindow`（`WS_EX_LAYERED|TRANSPARENT|NOACTIVATE|TOOLWINDOW`），`paint_shadow` 逐像素算落差写预乘 BGRA，`UpdateLayeredWindow` 上屏；跟随窗口 `sync_shadow()` | `UpdateLayeredWindow` 的 `pptDst` 必须传 `None`，传 `Some(&POINT::default())` 会把阴影挪到 (0,0)；客户区最外圈另画 1 px `#C8C8C8` 发丝边 |
| 4 预览跟随选中项 | `SetTimer(hwnd, TIMER_FOLLOW, 400ms)`；`WM_TIMER` → `foreground_is_shell()` → `selection::current_selection().first()` → 与 `last_path` 比较 | 先看前台类名再调 COM，省掉大部分轮询开销；跟随中发现不支持的文件**只静默跳过**，不响铃（否则扫文件时连响） |
| 5 顶栏也能拖 | `on_chrome_click` 的"顶栏空白处"分支从 `return true` 改成 `on_drag_start(...)` | 按钮与页码框仍优先判定 |
| 6 阻止关闭（单向） | `ChromeButton::Pin`（"锁"）+ `Chrome.pinned` + `State.pinned`；`close_blocked(how)` 统一拦空格/Esc | 开了不能取消（再点只响铃）；`×` 与托盘退出仍可关（显式意图）；**空格收起有两条路径（`toggle` 与 `on_hotkey` 的"无选中项"分支），两条都要拦** |

### 验证

- `cargo test` **163 passed**（app 30 / core 42 / platform 19 / plugin-api 4 / plugin-demo 7 / render 54 / malformed 7）、
  `cargo fmt --check` 0、`cargo clippy --all-targets -- -D warnings` 0、release exe **1,891,840 B**。
- `_bench/verify-input-1010.ps1` → **PASS=13 FAIL=0**（连续 3 轮）：
  不支持的文件不弹窗（`hwnd=0`）、日志有 `SHOW 拒绝`、拖顶栏 `dx=60 dy=40` 尺寸不变、
  拖右边 880 → 1000、有 `RESIZE 结束`、点锁后空格关不掉、`×` 仍可关、跟随选中项切到图片。
- `_bench/verify-shadow.ps1` → 阴影窗口 `912x780` 与预览窗 `880x748` 对齐（外扩 16 px），
  外侧→窗口亮度 `255,255,…,218,210,201,192,182,171` → **16 px 渐变贴边最浓**。

### 踩到的坑（已进 CONVENTIONS #82–#88）

- `MessageBeep` 不在 `windows` crate 的 `Win32::UI::WindowsAndMessaging` 里，编译期直接找不到。
- `UpdateLayeredWindow` 的 `pptDst` 传 `Some(&POINT::default())` 会把分层窗口搬去 (0,0)。
- `CS_DROPSHADOW` 类风格在当前环境下**完全看不见**（无边框 + `WS_POPUP`），只能自己做分层投影。
- "三圈 `FrameRect`"在白色背景上像塑料卡纸，用户一眼否掉 ⇒ 柔和投影才是对的方向。
- 空格的收起路径有两条，只拦 `toggle` 会漏掉热键回调那条（断言 6b/6c 当场挂）。
- 跟随轮询若每个 tick 都调 `selection::current_selection()`，会把 COM 调用打满；先 `foreground_is_shell()` 短路。
## 1.0.11 — 顶栏页码框与「锁」交换位置

用户一句话需求：「把页码和锁按钮交换下位置」。

- 顶栏右侧顺序从 `锁 | 页码框 | ↗ | ×` 改成 **`页码框 | 锁 | ↗ | ×`**。
- 改动只在 `crates/qlr-platform/src/window.rs` 的 `chrome_rects()`：`pin` 紧贴 `external` 左边，
  `page` 在 `pin` 左边留 10 px；标题右边界改为 `rects.page.left - 12`。
  绘制（`paint_chrome`）与命中测试（`on_chrome_click` / `update_hover`）都从 `chrome_rects` 取，**不用改第二处**。
- 测试 `chrome_rects_place_pin_left_of_the_page_box` 改名 `chrome_rects_place_page_box_left_of_the_pin_button`，
  断言顺序 `page <= pin == external <= close`，并补上页码框尺寸（`PAGE_BOX_W`、上下各留 5 px）。

### 验证

- `cargo test` **163 passed**、`fmt` 0、`clippy -D warnings` 0。
- `_bench/verify-input-1010.ps1` **13/13**（脚本里点"锁"的坐标跟着改成 `external.left - CHROME_BUTTON_W`）。
- 截图 `docs/screenshots/preview-pdf.png` 重拍：`pdf_8page.pdf  1 / 8  锁  ↗  ×`。
## 1.0.12 — 修"缩小的时候外面多一圈灰色阴影框"

用户反馈（附截图）：缩小图片后，窗口外多出一圈比窗口大的灰框。

### 根因

投影是**另一个分层窗口**（`QLRust.ShadowWindow`），它的位置与尺寸靠 `sync_shadow()` 跟着预览窗走。
1.0.10 只在"显示 / 拖动 / 拖边改大小 / 松手补渲染"时同步，**缩放（`set_zoom_to` 改窗口尺寸）这条路漏了**：

- `resize_around_center` 让窗口以中心收缩（比如 795×477 → 509×316）；
- 阴影还停在 827×509 的老尺寸上（中心相同，所以四边均匀露出约 95 px）——
  正是用户截图里那圈"比窗口大、四边等宽"的灰框。

### 改法

1. `set_zoom_to()` 末尾补 `sync_shadow()`（缩放改窗口尺寸后立刻重算阴影）；
2. `repaint()` 开头再兜一次 `sync_shadow()`——任何来源的窗口尺寸变化（缩放、拖边、外部改尺寸、
   以后新增的路径）都会在下一帧自愈；`sync_shadow` 内部按尺寸缓存 alpha 图，
   尺寸不变时只是一次 `SetWindowPos`，可以放心每帧调。

### 验证

- `_bench/verify-zoom-shadow.ps1`（新）：打开 1920×1080 图片（50%）→ 缩小两档 → 恢复一档，
  每步比对两个窗口的矩形 → **4/4 PASS**：
  `795x477 → 636x388 → 509x316`，阴影始终 `= 预览窗 ±16`（`827x509 → 668x420 → 541x348`）；
- `cargo test` **163 passed**、`fmt` 0、`clippy -D warnings` 0。

## 1.0.13 — 窗口位置记忆

用户反馈：换文件时预览窗口"会回到默认位置"。其实不是换文件的 bug，而是**根本没有记忆**——
每次 `show_preview` 都按内容重新居中（图片按原图尺寸、文本按 880 阅读宽度），
用户量好的位置每次都被覆盖掉。

### 做法

- `Config` 新增 `window_pos: Option<(i32, i32)>`（屏幕坐标，左上角）；
- **只记位置、不记尺寸**：尺寸由内容决定（原图 / 880 阅读宽度 / 上次拖边结果），
  位置才是用户的习惯；
- 所有会改变窗口位置的路径都记一次（`remember_position()`）：显示、拖动、拖边改大小、缩放；
- **落盘时机**：收起（`hide_preview`）与退出（`quit`）——拖一次写一次盘没必要；
- 开窗时：**窗口已经开着就用它现在的位置**（跟随选中项换文件、连按空格换内容都不该跳），
  刚打开才用 `config.window_pos`，没记过才居中；
- 记下的坐标一律过 `window::clamp_into`（换显示器/改分辨率后坐标可能落在工作区外，
  否则窗口会出现在看不见的地方）。

### 验证

`_bench/verify-pos.ps1`（真实鼠标拖动 + 显隐往返）→ **8/8 PASS**：

- 开窗 `562,296` → 拖到 `702,386` → 收起 → 重开 **仍在 `702,386`**；
- `config.json` 里读得到 `"window_pos": [702, 386]`；
- 开着时换文件（1920×1080 的 PNG → 3840×2160 的 JPG）位置不动；
- 再拖一次（`622,426`）→ 收起 → 重开仍在那；换成尺寸完全不同的文件也在同一点。

`cargo test` **165 passed**（core 43 / app 31 / platform 19 / plugin-api 4 / plugin-demo 7 / render 54 / malformed 7）。

## 1.0.14 — 窗口尺寸也记（按格式分开）

用户接着 1.0.13 说：「尺寸也记一下」。

### 为什么必须**按格式**记

如果只记一份尺寸，三种内容会互相踩：
文本是 880 px 的阅读版式、图片按原图 50%、PDF 按页宽比例，差一个数量级。
把文本拖到 1200 宽之后打开 PDF，就会得到一条 1200 宽的窗口里嵌着 631 宽的页面（两侧硬留白）；
反过来把 8000×6000 大图撑满工作区，再打开小图就是一圈白边——那看起来就是 bug。

所以 `Config.window_sizes` 是一个 `BTreeMap`，键是 `format::Kind::name()`
（`image` / `pdf` / `text` / `markdown`），值是该格式的窗口宽高（**含顶栏**）。

### 什么时候记

只有**用户自己改过尺寸**才记：

- 拖边/拖角改大小（`finish_resize`）；
- 缩放（`set_zoom_to`，放大看细节后保持这个大小是符合直觉的）。

首次打开时按内容算出来的尺寸**不记**——否则一张大图就会把之后所有图片的初始尺寸
都顶到工作区上限，"按原图 50% 开窗"的设定等于废掉。

### 什么时候用

`show_preview` 里优先用本次会话的记录（`last_sizes`），其次用落盘的（`config.window_sizes`）：
"拖大窗口 → 换文件"保持拖大的结果，"重开程序"也能延续上次的决定。
套尺寸时一律夹进工作区（`clamp` 到工作区宽高），再走 1.0.13 的位置记忆 `place_at`。

### 验证

`_bench/verify-size.ps1` → **9/9 PASS**：

| 断言 | 实测 |
|---|---|
| A1 图片开窗 | 795×477 |
| A2 拖右边变宽 | 795 → 955 |
| A3 落盘 | `"image": [955, 477]` |
| A4 重开尺寸不变 | 955×477 |
| A5 位置同时还在 | 562,296 |
| B1 PDF 用自己的尺寸 | pdf 631×924（图片是 955×477） |
| C1 PDF 尺寸单独记 | 751×924 → 重开 751×924 |
| C2 图片没被 PDF 覆盖 | 仍 955×477 |
| D1 缩小后的尺寸也记住 | 764×388 → 重开 764×388 |

`cargo test` **166 passed**（core 44 / app 31 / platform 19 / plugin-api 4 / plugin-demo 7 / render 54 / malformed 7）。

## 1.1.0 — 投机预渲染（窗口收着时先把选中的那一张画好）

用户先问"和 QuickLook 原版差多少、除了加格式还能优化什么"（本机同口径实测见 LOG 上一段），
我给了一份五期计划 `docs/plans/optimization-1.1-to-1.3.md`；用户拍板：
> 「二期可以做，但是设置页要可以关闭……5期可以做进来，我们看看效果」
> 「二期默认自适应吧，我试试效果，后续我觉得没用的话会放弃掉这项改进。可以按顺序开始了」

### 一期的核实结论（省掉一整期的工）
计划书里的三项在动手前先核了代码，结果两项是**现状**、一项**收益太小**：
- A1 窗口热池：`run()` 里 `create_preview_window` 只在启动时跑一次，`hide_preview()` 只 `window::hide()` 不销毁 → 本来就是热池。
- A3 pdfium 懒加载：`crates/qlr-render/src/pdf.rs` 的 `static API: OnceLock<...>` + `fn api()`，第一次真渲染 PDF 才 `LoadLibrary` → 本来就是懒加载。
- A2 文本稀疏索引：`s.scroll` 本来就是"源行"单位，滚动条按 `scroll / 总行数` 映射（和所有编辑器一致），折行只影响"一屏几行" → 收益远小于计划书描述。
⇒ 一期**空过，直接做二期**。这条更正已回填进计划书，避免以后有人去找不存在的 `REUSE window` 日志。

### 做了什么
- `crates/qlr-core/src/config.rs`：`pub enum Prewarm { Off, Adaptive(默认), Always }` + `RECENT = 300s`、`name/label/cycle/is_on`；
  自定义 `de_prewarm` 反序列化（**一个档位拼错不能让整份配置解析失败**，否则日志级别/窗口尺寸/快捷键全被打回默认）。
- `crates/qlr-app/src/app.rs`：独立线程 `qlr-prewarm` + 单槽 `PREPARED`、`spawn_prewarm_worker()`、`maybe_prewarm()`（六道闸门）、
  `take_prepared()`（尺寸 ±2 px 宽容；不匹配就清槽并记 `PREWARM 未命中`）、`present_canvas()`（渲染完成与预热命中共用上屏逻辑）、
  `window_size_for()`（开窗与预热共用尺寸算术）；`show_preview` 末尾先试槽、命中就跳过 `request_render`；
  计时器改常驻（`run()` 里 `start_timer`，`hide_preview` 不再 `stop_timer`），`on_follow_timer` 拆成"可见→跟随 / 收着→预热"。
- `crates/qlr-app/src/settings.rs`：新增 **性能 → 投机预渲染** 一行（点一下换档，不用打字）+ 两条说明。

### 验证
| 断言 | 结果 |
|---|---|
| A1 首帧能打开 | PASS |
| B1/B2/B3 记录候选 → 开始 → 就绪 | PASS（预热 20 ms） |
| C1 再按空格命中、跳过渲染 | PASS |
| D1 关档后完全没有 `PREWARM` | PASS |
| D2 关档后也不存在命中 | PASS |
| 8MP JPEG 首帧 | 20 ms → **0–1 ms** |
| 常驻增量（Private） | **+2.3 MB** |

`_bench/verify-prewarm.ps1` **7/7 PASS**、`_bench/measure-prewarm.ps1` 出数字；`cargo test` **168 passed**；
`fmt` / `clippy -D warnings` 干净；exe **1,949,184 B**（1.0.14 是 1,929,216 B，+20 KB）。

### 踩坑
1. **别在宿主运行中删 `app.log`**：宿主持着已删除文件的句柄，后面所有日志写进黑洞，脚本只读到空文件 → 三次测量全 `-1`。
   正确做法是记行号、只看新增行（和 CONVENTIONS #37 是同一类坑）。
2. PowerShell 的 `-match` **不区分大小写**：配置行里的 `prewarm=off` 会被 `-match 'PREWARM'` 命中，
   于是"关档后没有预热日志"这条断言假失败 → 要用 `-cnotmatch`。
3. 超预算日志一开始每 400 ms 刷一行（一直选中 16 MB 文本时），改成"换文件才记"。
4. `Add-Type -TypeDefinition ... -Name X -Namespace Y` 这套参数组合在 pwsh 7 里**不允许**（命名参数集冲突）→ 只用 `-TypeDefinition`。
5. 失败的那次测量还有一层原因：`--hide` 之后如果资源管理器没在前台，六道闸门第一道就挡住了，
   日志里只有 `wm=0x0113`（计时器）而**没有任何 `selection` 行** —— 排查时先看有没有 `selection` 行，就知道闸门停在哪一道。
## 1.2.0 稳定性硬化（2026-11-01）

计划书三期落地：渲染超时/取消、崩溃兜底 + minidump、长稳压测、fuzz 扩容。

### 做了什么

| 项 | 内容 |
|---|---|
| B1 取消信号 | 新模块 `qlr-render::cancel`（`cancel/reset/checkpoint`）；图片（入口 + 解码后）、PDF（入口 + 缩放前）、文本/Markdown/源码（入口 + 解码前）各插检查点 |
| B1 看门狗 | `TIMER_WATCHDOG`（250 ms）：渲染超 3 s 记 `RENDER 超时`、请求取消、把占位换成"正在请求取消"；UI 线程从不阻塞 |
| B1 排队丢弃 | 渲染线程一次只做最新任务，排空队列并记 `RENDER 丢弃 N 个` |
| B2 崩溃报告 | 新模块 `qlr-platform::crash`：`crashes\crash-<stamp>.txt`（含回栈）+ `.dmp`（`MiniDumpWriteDump`）+ 日志 `PANIC: …（dump=…）`；下次启动提醒 |
| B2 单一 hook | 删掉 `run()` 里旧的简易 panic hook（它会覆盖带 minidump 的那个） |
| B2 插件隔离 | 插件 `render`/`free` 包 `catch_unwind`（release 是 abort，救不回进程，只作铺路） |
| B3 长稳压测 | 新脚本 `scripts/stress.ps1`：开关 2000 次 + 翻页 500 次 + 缩放 300 次，盯 GDI/USER/句柄/私有字节 |
| B4 fuzz 扩容 | 新测试 `crates/qlr-render/tests/fuzz.rs`：确定性变异 2000 例（`QLR_FUZZ_ITERS` 可扩到 1 万） |

### 验证

- `_bench/verify-hardening.ps1` **12/12 PASS**（`C0`–`C5` 崩溃链路、`A1`–`A5` 超时/取消链路、`B1` 排队丢弃）。
  注入方式：`QLRUST_SLOW_RENDER_MS` / `QLRUST_PANIC_TEST` 两个隐藏环境变量（经 `.cmd` 批处理设，因为 WMI 不继承 pwsh 的环境）。
- `scripts/stress.ps1`（全量 **2000/500/300**，166.8 s）**7/7 PASS**：
  GDI 20→20、USER 16→16、句柄 153→153、私有字节 3.9→5.2 MB、RSS 前段 14.1→后段 15.7 MB（峰值 20.3，重启后 11.1）、日志无 panic。
- `cargo test` **173 passed**（core 46 / app 31 / platform 22 / plugin-api 4 / plugin-demo 7 / render 55 / fuzz 1 / malformed 7）、`fmt` 0、`clippy -D warnings` 0。
- exe **1,972,224 B**（1.1.0 是 1,949,184 B，+23 KB：取消检查点 + 崩溃报告 + minidump 绑定）。

### 踩坑（都进了 CONVENTIONS #97–#104）

1. `MessageBeep` 不在 windows crate → 用 `PlaySoundW("SystemHand")`（要 `Win32_Media_Audio`）。
2. `MiniDumpWriteDump` 的 cfg 门是三个 feature 一起（`Storage_FileSystem` + `System_Kernel` + `System_Memory`），只加 Debug 会"configured out"。
3. 崩溃文件名里不能有 `:`（`utc_timestamp()` 带冒号）→ 白名单消毒，否则 `dump=false` 且悄无声息。
4. 只能有一个 panic hook：旧的简易 hook 把带 minidump 的覆盖掉了。
5. `panic = "abort"` 会带走日志缓冲 → 崩溃证据必须落独立文件。
6. WMI 起进程不继承 pwsh 环境；`cmd /c "set X=1 && ..."` 的 `set` 会把空格算进值里（要 `trim()`）；
   `cmd /c "脚本.cmd" "arg"` 偶发什么都不做，稳定写法是 `cmd /c ""脚本.cmd" "a" "b""`，更稳的是写 `.cmd` 批处理。
7. 自动化测"崩溃"前必须保证没有残留实例——否则新进程只是转发请求然后以 0 退出，看起来像崩溃了但什么都没测；
   `Stop-Qlr` 要轮询到进程真的消失，观察窗口要 ≤ 50 ms（abort 只活几十毫秒）。
8. 长稳的内存判据要看私有字节 + GDI/USER/句柄，RSS 涨几 MB 是堆保留（重启回基线）。

## 1.2.1 大 PNG 流式降采样（2026-11-01，计划书四期）

### 做了什么

| 项 | 内容 |
|---|---|
| 新模块 | `crates/qlr-render/src/downscale.rs`：`decode_png_boxed(path, dst_w, dst_h) -> Result<Option<Bitmap>>` |
| 原理 | `png::Reader::next_row()` 逐行解码 → 按面积平均累加到"当前目标行" → 目标行变化时 `flush` + 清零 |
| 接线 | `image_view::render_scaled` 在 JPEG 分支之后加 PNG 分支（`is_png` + 需要缩小 + ≥4 MP 才走） |
| 放弃的情形 | 隔行（Adam7）、APNG、放大、小图、非 8 位输出 → 返回 `None` 交回整图解码 |
| 取消 | 每 64 行一次 `cancel::checkpoint()` |

### 验证

| 场景 | 1.2.0 | 1.2.1 |
|---|---|---|
| 48MP PNG 峰值 Private | 140.6 MB | **4.8 MB** |
| 48MP PNG RENDER | 183 ms | 191 ms |
| 8MP PNG 峰值 Private | 29.5 MB | **7.1 MB** |
| 8MP PNG RENDER | 42 ms | **29 ms** |

`cargo test` **179 passed**（render 61，新增 downscale 6 条）、`fmt` 0、`clippy -D warnings` 0；exe **1,978,880 B**（+6.6 KB，png 本来就在依赖树里，只是改成直连）。

### 踩坑

1. png 0.18 的 `Reader::next_row()` **不收缓冲参数**（借出一行切片），`output_buffer_size()` 返回 `Option<usize>` ——
   照 0.17 的写法编译直接挂在签名上。
2. `info.color_type` 是文件里的色型（调色板图仍是 `Indexed`），设了 `EXPAND` 也不会变 →
   要用 `output_color_type()` 算通道数，否则整幅图错位。
3. png 0.18 的编码器**不能生成隔行 PNG**（无 `set_interlaced`），"隔行拒绝"分支单测覆盖不到，
   只能靠真实文件验；单测改成钉"调色板 → RGB 展开"这条色型分支。
4. clippy 两条：`needless_range_loop`（`for tx in 0..dst_w` 用 `counts[tx]`）→ 改 `counts.iter().enumerate()`；
   `unnecessary_cast`（`(y / 16) as u8`，y 已是 u8）。

## 1.3.0 五期：PDF 页缓存 + 相邻页预取（5a/5c 经实测放弃）

**用户要求**：m17151「继续」→ 按 `docs/plans/optimization-1.1-to-1.3.md` 做五期 1.3.0。

### 做了什么
1. **页缓存**（`crates/qlr-app/src/pagecache.rs`，新文件）：
   `PageKey { path, page, width, height }` 精确匹配；`VecDeque` 做 LRU；上限 `CACHE_LIMIT_BYTES = 64 MB`；
   单页超过上限就不入缓存（否则一页挤掉所有别的页）；`get()` 命中后把条目挪到队尾；
   换文件时 `clear()`。5 条单测（精确匹配 / 溢出淘汰 / 命中刷新 / 超大不缓存 / 清空）。
2. **接线**（`crates/qlr-app/src/app.rs`）：
   - `request_render` 开头查缓存，命中就 `present_canvas`（代际照样 +1，免得旧帧回来把新页顶掉）；
   - `State.pending_key` 记住"这帧算完该写进哪个 key"，`on_render_done` 的 Ok 分支写缓存并 `prefetch_neighbor_pages`；
   - `WarmTask { Prepared(RenderJob), Page(RenderJob) }`：预渲染线程现在两种活，`warm_prepared` 是原逻辑，
     `warm_page` 只写缓存不上屏；
   - `prefetch_neighbor_pages` 只在预渲染档位开着时投递，前后各一页、已缓存就跳过。
3. **尺寸统一**：开窗时的渲染请求高度改成 `height − reserve`，与翻页用的 `view_size()` 一致
   （否则缓存 key 差 30 px，命中率恒为 0）。

### 验证
- `cargo test` **184 passed**（app 36 / core 46 / platform 22 / plugin-api 4 / plugin-demo 7 / render 61 / fuzz 1 / malformed 7）。
- `_bench/verify-pagecache.ps1` → **8/8 PASS**：开窗预取相邻页、翻页走预取、翻回命中、预取过的页也命中、
  关档后不预取但缓存仍命中。
- 200 页 Word PDF：三次翻页往返全部 **请求→首帧 0 ms**（此前 6–9 ms，2× 放大 28 ms 量级）。
- 缓存占用：8 页文档实测 2 页 / 4 MB；64 MB 上限 ≈ 28 页。
- 5a（D3D11 呈现）实测不划算：`PAINT` 与 `RENDER` 同毫秒完成（<1 ms），GDI 不是瓶颈；
  5c（mipmap）三条降采样路径已经覆盖，均记入 CONVENTIONS 第 111 条与 STATUS §31。

### 踩的坑
1. **缓存 key 与渲染请求尺寸不一致** → 命中率恒为 0；日志 `631x924`（写）vs `631x894`（读）差 30 = 顶栏。
   教训进 CONVENTIONS #108：同一份"要渲染多大"只能有一个算法。
2. 验证脚本 B1 断言写反（预取生效后第 2 页本就不该再渲染），改成"命中且本段无 `RENDER gen=`"。
3. `pagecache::is_empty()` 只给测试用 → clippy `dead code`；测试改用 `len() == 0`。
4. 又一次 `edit` 的 old_string 末尾换行被吃掉，把 `prewarm_candidate: None,` 和 `render_since: None,` 粘成一行；
   读回发现后按完整行修好（老坑，见 CONVENTIONS 里"改完要读回"那条）。

---

## 1.4.0 上一帧缓存 + Office/压缩包插件（2026-11-01）

### 用户原话
> 1可以做，2和3可以一起做，做3,然后用2做插件实验，4好像没必要

（1 = 缓存上一帧补热启动，2 = 插件生态，3 = Office/压缩包轻量预览，4 = 资源管理器集成。）

### 做了什么

| 条目 | 做法 | 关键数字 |
|---|---|---|
| 上一帧缓存 | `FrameKey{path,mtime,size,page,w,h,upscale}` + 3 帧 LRU；`show_preview` 先查、`on_render_done` 后存 | 重开同一文件 **22 ms → 2 ms**，不再渲染 |
| Office/压缩包插件 | 新 crate `qlr-plugin-office`（cdylib）：缩略图优先，没有就抽文字画卡片 | docx/xlsx/pptx/zip **1–42 ms**，DLL 1.2 MB |
| 插件 ABI 验证 | 宿主一行代码没改，只把 DLL 放进 `plugins\` | `插件就绪 loaded=2 extensions=28 errors=0` |

### 上一帧缓存（为什么不是"只缓存内存里的那一帧"）
- 之前每次开窗都重新渲染（4K 图 20 ms、48MP 60 ms），**热启动比 QuickLook 慢 2–4×**（见 `compare-1.3.0-vs-quicklook.md`）。
- `FrameKey` 必须带 **mtime + 文件大小**：只比路径的话，编辑器改过的文件会拿旧内容冒充新的。
- 只留 3 帧（LRU）：覆盖"在 A/B 两个文件之间来回看"，内存上限约 3 张窗口位图（≈10 MB），仍然远小于 QuickLook 的常驻。

### Office/压缩包插件（插件体系的第一个真实用例）
- 两条路线：**文件自带的缩略图**（`docProps/thumbnail.jpeg` / `Thumbnails/thumbnail.png`）优先，
  没有就从正文抽文字、用 GDI 画一张"大纲卡片"（标题 = 文件名，正文 = 前 40 行）。
- 不支持真排版（那要一个排版引擎，几百 MB），与"6 个文件 9 MB"的体积目标冲突；
  真要看排版走顶栏的「在其他应用打开」。
- 抽取规则：docx 取 `<w:t>`、pptx 逐页取 `<a:t>`、odt/ods/odp 取 `<text:*>`、zip 列条目（名字 + 大小）；
  xlsx 要同时管**共享字符串表**和**行内字符串**（openpyxl 写的是后者，只读前者会得到一张空表）。

### 验证
- `_bench/verify-lastframe.ps1` → **7/7 PASS**（首次不命中 / 重开命中且不渲染 / 更快 / 中间开过别的文件仍命中 / 文件改过不命中 / 改过就走渲染）。
- 五个夹具真机预览：`plugin-plain.docx`（880×748 卡片，中文正文抽全）、`plugin-demo.xlsx`（工作表名 + 四行数据带数字）、
  `plugin-plain.pptx`（3 页文字带分隔线）、`plugin-demo.zip`（4 个条目 + 大小）、`plugin-thumb.docx`（走缩略图，256×192）。
- `cargo test` **194 passed**（app 36 / core 46 / platform 22 / render 61 / fuzz 1 / malformed 7 / plugin-api 4 / plugin-demo 7 / plugin-office 10）、
  `fmt` 0、`clippy -D warnings` 0；release exe **2,016,768 B**。

### 踩坑
1. **`with_state` 嵌套自锁**：在 `with_state` 闭包里又调 `with_state` → 普通 `Mutex` 非重入 → 进程卡在开窗那一步，
   日志只到"开窗尺寸"，连 `SHOW` 都没有。做法改成"进闭包前先算好所有要用的值"。
2. `extract_between` 的 open 标记**不能带 `>`**：它会把"标记后第一个 `>`"当成标签结尾跳过去；
   写成 `"<v>"` 就会把 `120</v>` 整段吞掉（表现为 xlsx 里的数字全丢）。
3. xlsx 单元格分隔符要按 `<c`（不带空格）切：`<c>` 没有属性、`<c r="A1">` 有，只认带空格那种会漏掉整列。
4. 插件里 `DeleteDC` 返回 `BOOL` 且标了 `#[must_use]`；`unsafe fn` 导出还要写 `# Safety` 段，否则 clippy `-D warnings` 不过。
5. 造夹具用 python-docx/python-pptx 生成的 docx/pptx **自带 `docProps/thumbnail.jpeg`**（模板里就有），
   想验证"文字大纲"路线必须先把缩略图从 zip 里剥掉。

---

## 1.4.1 — 2026-11-01 · Office 走系统预览处理器（真排版）+ 压缩包目录树 + 拖动性能

### 用户原话
> 同一个文件，第二次预览的时候，会虚掉，并且拖动非常不流畅，这个bug要修掉。另外压缩包的预览，
> 我希望是列出来压缩包的结构。最后就是office的预览，我希望借鉴ql原版，当前实现的太简陋了，
> 只有文字，并且是图片显示，缩放的时候会有问题

补充：
> 图片和简单的文本，第二次打开的时候，我左键拖动一段距离，预览窗口会慢慢移动，过了很久才移动到目标位置

关于"借鉴原版"的边界：
> 借鉴原版的意思是，想要他的近似或更优实现形式，但因为我们的工具不一样，所以不可能完全一样，看你自己发挥取舍了

### 做了什么
| 问题 | 做法 |
|---|---|
| Office 只有文字、还是位图 | 新模块 `crates/qlr-platform/src/shellpreview.rs`：走系统 `IPreviewHandler`（注册表 `ShellEx\{8895b1c6-…}` → CLSID → `CoCreateInstance` → `IInitializeWithFile` → `SetWindow` → `DoPreview`），内容由 Word/Excel/PowerPoint 自己画在宿主子窗口里 |
| 处理器会盖掉顶栏 | `window::create_child_host` 给一块"内容宿主"子窗口（处理器按父**客户区**铺满） |
| 处理器吞滚轮 | `window::window_from_point` + `post_wheel` 把 `WM_MOUSEWHEEL` 转给它（lParam 用屏幕坐标） |
| 处理器起不来就卡在"正在渲染…" | `show_preview` 里 `if shell_wanted && !open_shell_preview(..) && renderable { request_render(..) }` 立刻退回插件卡片 |
| 压缩包只有文件名 | 插件里新增 `tree_lines()`：缩进目录树 + `N 个文件夹 · M 个文件 · 压缩后/解压后` |
| 拖动/二次打开"虚、卡" | ① `paint_bitmap_at`/`paint_bitmap_scaled` 在 1:1 拷贝时改用 `COLORONCOLOR`（原来无条件 `HALFTONE` 纯烧 CPU）② `register_class` 去掉 `CS_HREDRAW\|CS_VREDRAW` ③ `move_window_by` 加 `SLOWMOVE` 埋点（≥5 ms 才记） |

### 验证
- 真机逐格式：docx / xlsx / pptx 全部 `SHELL 预览已接管`，截图 `_bench/v141-docx.png`（Word 真排版）、
  `v141-xlsx.png`（真表格）、`v141-pptx.png`（真幻灯片）；`tree-demo.zip` → `PLUGIN … elapsed=27ms` + 目录树（`_bench/v141-zip.png`）。
- 连续切换 5 个文件（docx→xlsx→pptx→zip→docx）宿主存活。
- `cargo test` **195 passed**、`fmt --check` 0、`clippy -D warnings` 0、release exe **2,028,544 B**。
- **未能复现**：用户报告的拖动发卡——真实鼠标注入 40 步拖窗，`MOVE=39/40`、`PAINT=0`、耗时等于注入基准
  （图片/文本/markdown 三种都一样）。留 `SLOWMOVE` 埋点等用户复现数据。

### 踩坑
1. **`DoPreview` 必须等窗口可见**：在隐藏窗口上调用会让处理器等一个不会到来的激活，UI 线程卡死，
   任务管理器里是"未响应"幽灵窗口（第一版就这样，只能杀进程）。
2. **`IInitializeWithFile` 不在 `Win32::UI::Shell` 里**，在 `Win32::UI::Shell_PropertiesSystem`；
   features 还要开 `Win32_System_Registry`（查 CLSID 用）。
3. **zip 的目录条目可能根本不存在**（python `zipfile` 只写文件条目）→ 目录数要按路径前缀统计，
   否则界面显示"0 个文件夹"（已修）。
4. 调试期把 `let mut shell_ok` 写成了"赋值后没读"→ clippy `-D warnings` 直接拦；改成
   `if shell_wanted && !open_shell_preview(..) && renderable { … }` 更短也没有未读变量。

### 下一步
- 等用户开着 debug 日志拖一次，看 `SLOWMOVE` 是否出现，以定位"挪窗口慢"还是"重绘慢"。
- 中文/复杂排版的 Office 文档由系统处理器负责，插件那条文字抽取降级为"没装 Office 时的兜底"。

---

## 1.4.2 — 2026-11-01 · 收起窗口时卸处理器会把 UI 线程卡死 + 拖动热路径减负

### 用户反馈
> 这是我录的屏，能反映我的问题，而且预览后，关闭逻辑也出了问题

（录屏 27.8 s / 1234×818：`QLRust` 预览 xlsx → Excel 真排版正常；随后按下"关"之后窗口**留在屏幕上、内容变白**，进程还在但不再响应。）

### 根因
`hide_preview()` 第一件事就是 `s.shell_preview = None`，触发 `ShellPreview::drop → Unload()`。
而"收起窗口"这条链**经常发生在 `WM_COPYDATA` 处理期间**（另一个进程 `--show/--hide` 用 `SendMessage` 同步等我们），
在这个调用栈里进 COM 会卡住 → 后面的 `window::hide()` 根本没执行到 → 屏幕留下一个只有顶栏、正文全白的窗口，
日志断在 `收到请求 action=Show`，进程看着活着但不再响应。这和录屏里那一帧完全对得上。

### 改法
1. **延迟拆卸**：新增 `SHELL_TRASH` 队列 + `retire_shell_preview()` / `drain_shell_trash()`。
   收起时只把句柄挪进队列并 `PostMessage(WM_QLR_INVOKE)`；真正 `Unload` 发生在自己的消息循环里（已脱离同步调用栈）。
2. **跟随轮询让路**：拖动/改尺寸期间不调 `selection::current_selection()`（它要枚举 `IShellWindows` + `FindWindowSW`，
   几十毫秒起步，正好卡在拖动热路径上就是"窗口追不上鼠标"）。
3. **投影同步节流**：拖动时投影窗口最多 8 ms 同步一次，松手强制对齐（投影是另一个分层窗口，`SetWindowPos` 要走 DWM）。
4. **埋点**：`SLOWSELECT {ms}ms`（跟随轮询）、`SHELL 预览已卸下 N 个`、既有 `SLOWMOVE`。

### 验证
- 修复前：`--show xlsx` 再触发收起 → 无 `HIDE` 日志、窗口留在屏幕上（与录屏一致）。
- 修复后：`HIDE elapsed=8ms` → `SHELL 预览已卸下 1 个` → 窗口消失；文本/图片同样正常。
- 真实鼠标注入拖动 60 步（180×120 px）：`MOVE=60`、窗口位移精确 `180,120`、无 `SLOWMOVE`/`SLOWSELECT`。
- `cargo test` **195 passed**、`fmt` 0、`clippy -D warnings` 0、exe **2,033,152 B**。

### 踩坑
- `with_state(|s| s.shell_preview.take())` 返回的是 `Option<Option<ShellPreview>>`，要 `.flatten()` 才是 `Option<ShellPreview>`。
- 在 pwsh 里把带中文注释的脚本塞进 `-Command` 会被代码页搞坏（`ParserError` + 乱码），脚本一律写文件再 `pwsh -File`。
---

## 1.4.3 — 2026-11-01 · 缓存帧尺寸校验 + 拉花/卡顿埋点

### 用户反馈
> 拖手的问题还存在，第一次预览的时候没问题，第二次预览的时候拖手又出现了，并且当前预览滚动的时候，会有频闪

### 这次做了什么
1. **缓存帧尺寸校验**：`frame_matches_size()` —— `LASTFRAME` 命中前先比对"缓存位图分辨率 == 本次要渲染的尺寸"。
   不一致就不用缓存、重新渲染。原因是上屏那一侧遇到尺寸不一致会走**拉伸**（每帧重采样），
   表现正是"发虚 + 拖动跟不上 + 滚动频闪"三个症状同时出现。
2. **`STRETCH` 埋点**：位图与内容区差超过 16 px 时记一条 warn（差 1–2 px 属于宽高比取整，属正常，不记）。
3. 保留 1.4.2 的 `SLOWMOVE` / `SLOWSELECT` / `SHELL 预览已卸下` 埋点。

### 关键发现（本机实测）
同一张 1920×1080 图片：`开窗尺寸 407x259`，但渲染回来是 **405x228**（宽高比取整 + 顶栏 30 px），
内容区是 407x229 —— 每帧都在拉伸 2 px。量级很小、肉眼不可见，但说明**"位图尺寸 == 内容区尺寸"这件事从来没被保证过**，
一旦窗口尺寸记忆把窗口放大（用户拖过边），差值就会变成几十上百像素，那时就是"发虚 + 掉帧"。
所以加了尺寸校验与埋点，等用户机器上的日志确认是否命中这条路径。

### 关卡
`cargo test` 195 passed、`fmt` 0、`clippy -D warnings` 0、exe **2,033,664 B**、zip 5,522,527 B。---

## 1.7.3 · 2026-11-01 · 压缩包列表视图真正跑通（锁顺序 + 目录树重写 + 点击/滚轮）

### 从哪接着做
`HANDOFF.md` 末尾的接力说明：1.7.2 把压缩包维持插本文本通道，列表视图的代码在仓库里但 `repaint` 里用
`if false` 关着，已定位的真因是 `paint_archive()` 持 `archive_slot` 锁时又调 `archive_rows()`（同锁）→ 自锁死。

### 这次做了什么
1. **打开列表通道**：`repaint()` 去掉 `if false`，改成 `has_archive_view()`；顺手清掉一次 PowerShell
   行级改写留下的字面量 `\n` 注释（那次改坏的锚点）。`paint_archive()` 里锁内只拷 `title/selected/scroll`，
   放锁后再展平、再 `frame()`；顶栏的 hover/pin 与文本通道共用，并补 `sync_shadow()`。
2. **重写 `archive_rows()`**：原来那份"先挑文件→再插目录→最后整体按名字排序"的实现**只画得出目录**
   （文件列表 `rows` 被 `let _ = rows;` 丢掉），层级也串（`src/core` 会挂到 `assets` 下）。
   现在改成打开时预建**目录树**（`ArchiveNode` + `roots`）：补齐打包器不写的隐含父目录、目录在前按名小写排序、
   展平时才按 `expanded` 过滤。滚动/折叠只做一次整型 DFS，几千条目也只遍历一趟。
3. **交互**：`archive_click()`（点行选中 + 目录翻转 `expanded`，并**吃掉**这次点击——不然会落到
   `on_drag_start` 把窗口拖走）、`archive_scroll_by()`（滚轮一格 3 行，到头就停且不冒泡去缩放/翻页）。
   `frame()` 里给压缩包单独算滚动条（它的"内容单位"是展平行，和 `scroll_info()` 不是一套）。
4. **内置接管 `.zip/.cbz/.jar/.war`**：`show_preview` 的 `renderable` 增加"是压缩包"分支，否则 `.zip`
   会掉进"不支持"分支被响铃拒绝（`kind` 仍是 `Unsupported`——不为它新增 `Kind` 变体，
   免得污染格式注册表、预渲染与插件路由）。解析失败（坏包/加密/截断）→ 响一声、不显示、不残留占位。
   `hide_preview()` 顺手 `clear_archive_view()`，别让几千条目的树常驻。
5. **列表标记去掉 emoji**：GDI 的 `DrawTextW` 不做字体回退，📁/📄 全是豆腐块（CONVENTIONS #130）。
6. **新增**：`app.rs` 4 条单测（目录树嵌套/补目录/排序、点击反算行号）+
   `scripts/verify-archive.ps1`（11 项端到端：日志 + 三列像素 + 点击折叠/展开 + 滚轮 + 四扩展名 + 坏包）。

### 为什么
用户要的是"按空格看 zip 能像资源管理器那样展开目录、点开合、滚轮滚"。插本文本通道只能把目录名当文字列出来，
没有层级也没有交互。

### 怎么验证的（实测）
- `scripts/verify-archive.ps1` → **11 PASS · 0 FAIL**，截图落在 `_bench/archive/`
  （`01-expanded.png` 三列对齐、`02-collapsed.png` 目录收起、`03/04-big-*.png` 65 条目滚动 + 滚动条）。
- 日志证据：`ARCHIVE 5 个文件夹 · 6 个文件 · 解压后 17.7 KB（目录树 11 个节点）` →
  `SHOW archive …`；点击 `ARCHIVE 点击 assets/ 展开=false（共 10 行，第 0 行，scroll=0）`；
  滚轮 `ARCHIVE 滚动 12/38（一屏 27 行，共 65 行）`；坏包 `压缩包解析失败，按不支持处理`。
- 插件共存：铺上 `plugins\qlr_plugin_office.dll`（它也接管 zip/cbz/jar/war）后仍是内置列表视图 →
  内置优先，符合"内置接管"的要求。
- `cargo test` **194 passed**、`cargo fmt --check` 0、`cargo clippy --all-targets -D warnings` 0、
  `smoke.ps1` **16 PASS + 1 SKIP**（托盘 SKIP = 管理员会话 UIPI，非代码问题）。

### 踩坑（两条，已进 CONVENTIONS）
- **截图里"名字是新的、大小/时间还是旧的"**：不是绘制 bug。窗口有一半在屏幕外（1920 屏、窗口
  rect 1510..2390），Windows 只画可见部分，`PrintWindow` 抓回来的是混合帧（CONVENTIONS #129）。
  顺带说明这半屏外位置是**旧代码的点击把窗口拖走**留下的（列表点击当时还没吃掉，会落到拖动）。
  现在 `archive_click` 吃掉点击，这条路径没了。
- 外部注入点击（`mouse_event` down/up 间隔 80 ms）时看到一次点击产生两条 `ARCHIVE 点击`（系统又补了一条
  down）；间隔拉到 600 ms 就只有一条。这是注入时序问题，真鼠标不会这样。

### 还没做（下一步，按优先级）
1. ③ PPT 预览"最后一页"提示 / 顶栏文件名错乱（要查"为什么 Office 处理器没起来、退回了插件"）；
2. ⑤ 拖边改窗口大小失效（先打日志确认 `edges_at` 是否被 `on_chrome_click` 抢先 `return`）；
3. 列表视图可继续打磨：双击/回车用系统程序打开条目、拖滚动条、按名字过滤。

## 2026-11-01 · 1.7.4：PPT 翻页接管 + 顶栏标题时序 + 拖边改大小（Office 预览）

### 做了什么

**③ PPT"最后一页"**：系统预览处理器（PowerPoint 的"放映"）翻过最后一页会切到黑屏
「幻灯片放映结束，单击鼠标重新放映演示文稿。」——用户截图反馈的就是这个。
处理器没有"跳页/到第几页"的接口，只能给它投 `VK_NEXT`/`VK_PRIOR`，所以改成**我们数页**：

- `is_slides()`（pptx/pptm/potx/potm/ppsx/ppsm）+ `slide_count()`（用 `qlr_render::archive`
  数 `ppt/slides/slide*.xml`）+ `step_slide(delta)`：`clamp(1..total)`，**到头不投键**，
  只在顶栏气泡给「已是最后一页」/「已是第一页」；投失败给「这个演示文稿不支持翻页」。
- `turn_page` 与 `on_wheel`（放在 `forward_wheel_to_shell` 之前）都先走 `step_slide`。
- 顶栏气泡：`window::Chrome` 加 `toast: Option<&str>`，`paint_chrome` 画在页码/锁/最大化左侧；
  由 `TIMER_TOAST` 定时擦除。左界必须 `(right - width).max(rc.left + 60)`——
  用 `title.right + 6` 会在窄窗挤出 0 宽气泡（踩过）。

**③ 顶栏文件名一直是错的**：根因是**时序**不是取值。`DoPreview()` 是跨进程 COM，STA 上会泵
消息，期间 `WM_PAINT` 会按**旧** `last_path` 画一帧，之后再没有 `invalidate` 纠正它。
修法：把 `with_state(|s| s.last_path = target.clone())` 提到 `window::show_at` **之前**。
同时修标题宽度：页码框只在 `chrome.page.is_some()` 时让位（以前无条件扣 220px，
长文件名被 `DT_END_ELLIPSIS` 截半，240px 窄窗算出反向矩形 → 标题整条消失）。

**⑤ 拖边改窗口大小在 Office 预览里失效**：处理器的子窗口铺满内容区，边缘的鼠标进不到我们。
四处修：

- 预览窗样式补 `WS_CLIPCHILDREN`（缺它时我们填底色会擦掉子窗口像素，表现为"翻页后画面不动"）；
- `ShellPreview::take_over_input()`：`DoPreview` 后 400ms（`TIMER_SHELL_INPUT`）再
  `EnableWindow(宿主, FALSE)`，此后点击回落到预览窗；立即禁用会让内容区一直白（实测）；
- 热区按 DPI 缩放：`window_dpi()`（`GetDpiForWindow`）+ `edges_at(rc, x, y, margin)`，
  `resize_margin = scaled(8).max(6)`；
- `finish_resize` 改用 `content_size_of(client)`（高 = 客户区 − 顶栏 30px）：以前把整窗高当
  内容高，位图高 30px 走拉伸、`base` 也高 30px，每缩放一次漂 30px（文本只看宽度所以没露馅）。
- 滚轮转发重写：子窗口被禁用后 `WindowFromPoint` 永远返回预览窗，改成直接投给处理器里
  **面积最大的后代窗口**（`largest_descendant`），Word/Excel 照样能滚。

`hide_preview` 一并清 `resize_edges/resize_from/drag_from/dragged/slide/toast/shell_input_pending`
+ `release_capture()` + `allow_shadow()` + 两个 `KillTimer`（否则下次预览带着上次的拖拽/气泡状态）。

### 怎么验证的

新增 `scripts/verify-office.ps1`（便携目录放 `%TEMP%\qlrust-office-verify`、铺 `plugins\*.dll`、
debug 级日志、`Normalize-Position` 挪窗回主屏、真鼠标注入拖右下角）：**15 PASS · 0 FAIL**

1. `docx 由系统处理器绘制`、`标题在接管之前就设好`（日志里 `标题栏 ← X` 的偏移 < `SHELL 预览已接管`）；
2. `docx 内容画出来了`（亮像素占比 0.986）；
3. `docx 上拖边能改大小` 880x748 → 980x818，`改大小后内容高度 = 客户区−30`（980x788）；
4. `pptx 数出 3 页`、`滚轮到第 2 页`、`第 1/2 页画面不同`、`到头停住并给提示`、
   `最后一页不是黑屏`（亮像素 0.901，截图 `_bench/office/04-ppt-last.png` 显示第 3 页 + 顶栏「已是最后一页」）；
5. `pptx 上拖边能改大小` 980x818 → 1100x898、`边缘命中预览窗自己（输入已接管）`。

回归：`cargo test` **202 passed**、`cargo fmt --check` 0、`cargo clippy --all-targets -- -D warnings` 0、
`smoke.ps1` 16 PASS + 1 SKIP、`verify-archive.ps1` 11 PASS。

### 记进 CONVENTIONS

- **#131** `WS_CLIPCHILDREN`：不写它，父窗口的底色会擦掉子窗口像素（"翻页后画面不动"的假象）；
- **#132** `EnableWindow(FALSE)` 吃掉处理器区域的鼠标——要等首帧落地，且禁用后
  `WindowFromPoint` 不再可用（滚轮改为直投最大后代窗口）；
- **#133** 顶栏标题错乱是**时序**问题：记文件名必须在 `show_at` 之前。

## 2026-11-01 · 1.7.5：换文件时没卸处理器，旧子窗口盖住新内容（拖动出竖条纹）

### 现象与根因

用户发来截图：预览 `tree-demo.zip` 的列表视图，**右侧一大片竖条纹**，并说"预览窗口拖动会这样"。
本地复现时先撞见同一根因的另一副面孔：顶栏是 `plugin-demo.zip`，正文却还是上一张 Excel 表格。

根因：`open_shell_preview()` 只在"这次也走系统预览处理器"时才会把上一个文件
`ShellPreview` retire 掉。切到**不需要处理器**的文件（压缩包 / 图片 / 文本）时它压根不被调用，
于是 Office 处理器的子窗口（`QLRust.ChildHost` → PowerPoint/Excel 那层）留在预览窗里继续
盖着内容区；我们画的列表在它下面，用户只能看见"顶栏 + 半个旧画面"，
拖动时那片残留由别的进程按旧尺寸重绘 → 竖条纹。

压缩包分支还额外错在**提前 return**：列表视图路径根本走不到后面那段收尸代码。

### 改了什么

- `app.rs` 新增 `drop_stale_shell_preview()`：取走 `shell_preview` → `retire_shell_preview()`
  （不能就地 drop，会把 COM 带进 `WM_COPYDATA` 调用栈）→ 清 `slide`/`shell_input_pending`
  → `KillTimer(TIMER_SHELL_INPUT)` → 调试日志 `换文件：卸掉上一个文件的系统预览处理器`。
- 两个调用点：`show_preview` 的压缩包分支（`window::show_at` 之前）+ 主路径 `!shell_wanted` 处。

### 验证

`scripts/verify-office.ps1` 扩到 **21 项**（新增：pptx 有处理器子窗口 / 换文件时卸掉处理器 /
切到压缩包后没有残留子窗口 / 切过来画的是新内容 / 切到图片时也卸掉处理器 / 切到图片后无残留），
实跑 **21 PASS · 0 FAIL**。关键数字：切换前子窗口 4 个 → 切换后 0 个；截图 `_bench/office/05-after-switch.png`
是干净的 zip 列表。回归：`cargo test` 202 passed、fmt/clippy 0、`smoke.ps1` 全过、`verify-archive.ps1` 11 PASS。

### 踩到的坑（写脚本时）

- 对**同一个文件**再 `--show` 一次等于开关切换，窗口会被收起 —— 换文件测试必须"先换成别的文件"，
  否则等不到预览窗（第一次跑就卡在这里，白排查了一轮）。
- `EnumChildWindows(0)` 会枚举全桌面的子窗口（26 个）：句柄无效时必须先判零，否则
  "残留子窗口"永远 FAIL。
- `--show` 是异步转交（第二个进程发 `WM_COPYDATA`），'等日志出现某行'可能命中**上一次**的记录：
  要按"日志长度之后的新增部分"匹配，或者用时间戳。

## 2026-11-01 · 1.7.6：跟随选中项的延迟（419 ms → 41 ms）+ Office 切换的占位页闪烁

### 用户反馈

1. "预览窗口开着，点击其他文件导致预览内容变更时，延迟有点大"；
2. "Office 预览的时候，偶尔还会跳之前那个占位的页面，切换是最不流畅的，割裂感比较重"。

### 改了什么

**延迟**（`app.rs` / `selection.rs` / `ipc.rs`）

- 跟随线程：窗口开着时轮询 300 ms → **90 ms**（收着时 250 ms），睡眠扣掉查询开销；
- 线程发现变化就 `PostMessage(WM_QLR_FOLLOW)`，UI 立刻切，不再等 400 ms 计时器
  （计时器退化为兜底 + 投机预热）；
- 只对"和当前显示不同、且没通知过"的路径投消息，避免"不支持"文件造成消息风暴；
- `selection`：`IShellWindows` 按线程缓存 + "上次命中的 browser" 快路径（同窗口换文件时
  只剩一次 `GetSelection`），失效才完整枚举。

**割裂感**（`app.rs` / `window.rs` / `shellpreview.rs`）

- `hold_frame`：换文件时只重画顶栏（新增 `window::paint_chrome_strip`），内容区像素冻住，
  `TIMER_SHELL_INPUT` 到点解除；
- `shell_cover`：旧处理器先不卸，`raise()`（`SetWindowPos(HWND_TOP)`）继续盖住，
  `TIMER_SHELL_SWAP`（260 ms）到点再卸；
- 收尾清理：`hide_preview` 里一并 KillTimer(TIMER_SHELL_SWAP) 并 retire `shell_cover`。

### 怎么验证的（全部实测数字）

- 新增 `_bench/follow-latency.ps1`：开资源管理器 + 预览窗，`SendKeys("{DOWN}")` 改选中项，
  用日志时间戳算"按键 → 开始切换"。**新：平均 41 ms / 最差 72 ms**（4 轮）；
  **旧二进制（git stash 编出来对照）：平均 419 ms / 最差 440 ms**。
- `_bench/switch-frames.ps1`：pptx → docx 切换后每 40 ms 抓一帧。137 ms 那帧是空白 Word 窗口
  （Office 自己的首帧延迟），199 ms 就是正文；我们的"占位文字页"不再出现。
- 回归：`cargo test` 202 passed、`cargo fmt --check` 0、`cargo clippy --all-targets -- -D warnings` 0、
  `verify-office.ps1` 21 PASS、`smoke.ps1` 全过、`verify-archive.ps1` 11 PASS。

### 已知遗留

- Office → Office 切换仍有约 60 ms 的白底：Office 在开始第二次 `DoPreview` 时会把旧处理器的
  窗口拆掉，`shell_cover` 盖不住。要彻底消掉得让新宿主窗口"先在后台画好再显示"，
  需要确认 `IPreviewHandler` 在隐藏窗口下会不会照常绘制。

## 2026-11-01 · 1.7.7：Excel 预览卡死（占位页不动 + 滚动条全废）的根因与修复

### 用户反馈

"Excel 还有那个占位页，并且滚动栏都不可用"；并提到"WPS 的预览和 Office 的预览好像不一样"。

### 根因（实测：进程 `Responding=False`）

`open_shell_preview` 之后紧跟一次 `sync_shell_rect()`，它会走
`IPreviewHandler::SetWindow`。Excel 的预览是 **OLE 就地激活**对象，`DoPreview()` 刚返回时
还在激活过程中，这时同步回调容器（我们）就会**自等自死锁**：日志停在 "SHELL 预览已接管"，
窗口只剩占位页、鼠标全无响应 —— 滚动条当然"不可用"。

### 改了什么（1.7.7）

| 文件 | 改动 |
| --- | --- |
| `crates/qlr-app/src/app.rs` | 子窗口矩形提到 `open()` 之前算（新增 `shell_content_rect`），打开后不再立刻 `SetWindow` |
| 同上 | 不接管输入的 Office 预览**内缩 10 DIP**（`shell_inset`），把窗口四条边留给拖边改大小，内容区仍归处理器 |
| 同上 | `TIMER_SHELL_INPUT` 改成**轮询**（120 ms × 最多 25 次）等 `visible_area ≥ 50%`，不再死等 400 ms（Excel 要 768 ms） |
| `crates/qlr-platform/src/shellpreview.rs` | 新增 `visible_area(content)`：最大可见后代窗口占内容区的百分比 |
| `crates/qlr-platform/src/hotkey.rs` | 新增 `set_wheel_pass_through()`：Word/Excel 预览时**放行**滚轮（以前钩子一律吞掉） |
| `crates/qlr-platform/src/window.rs` | 新增 `set_activatable()`：Office 预览允许激活（滚动条才响应），其余保持 NOACTIVATE |

### 验证

- Excel 不再卡死：`Responding=True`，日志 `SHELL 就绪：内容占 93%（轮询 1 次）`（121 ms）；
- 占位页不再出现：切到 xlsx 时先冻住上一屏，等它就绪（`_bench\drag\177-after.png`）；
- **拖动滚动条生效**（`_bench\test-excel-interaction.ps1`：`拖条生效: True`）；
- 回归：`cargo test` 202 passed、fmt/clippy 0、`verify-office.ps1` 21 PASS、
  `smoke.ps1` 全过、`verify-archive.ps1` 11 PASS。

### 仍未解决

Excel 预览的**滚轮**：钩子放行、`PostMessage(WM_MOUSEWHEEL)`、`WM_VSCROLL`、`VK_NEXT`
全部无效；但资源管理器自己的预览窗格里同一个 xlsx 能滚（`_bench\drag\pane-after.png`
滚到第 25–50 行）——说明是宿主激活/焦点差异，下一步试"把焦点交给 Excel 子窗口"或
"读 UIA 滚动条 range 后自己设值"。已记进 CONVENTIONS #138。

## 2026-11-01 · 1.7.8：点一下预览窗就把焦点交给处理器（Excel 滚轮/滚动条立刻可用）

### 用户要求（原话）

"Excel滚动是有效的，只是焦点有点问题，修改成鼠标单击预览窗口后立马把焦点放在预览窗口上即可，
保持资源占用。"

### 根因

Excel/Word 的预览是 **OLE 就地激活**对象：容器窗口不激活、焦点不在内容子窗口上，
它就不收滚轮，滚动条也只"看得见点不动"。1.7.7 让窗口**可激活**了，但点击后焦点仍停在
容器窗（我们）身上，没有进到处理器的内容层——用户必须再点一次。

### 改了什么

| 文件 | 改动 |
| --- | --- |
| `crates/qlr-app/src/app.rs` | `WM_MOUSEACTIVATE` / `WM_ACTIVATE` / `WM_SETFOCUS` 三处都调 `focus_shell_content()`（实测 `WM_MOUSEACTIVATE` 有时压根不来，后两个一定来） |
| `crates/qlr-platform/src/shellpreview.rs` | `focus_content()`：把焦点塞给 `nav`（`DoPreview` 后按面积挑出的最大可见后代窗口） |
| `crates/qlr-platform/src/window.rs` | `focus_child()`（手写 `SetFocus` FFI，windows 0.62 没生成绑定） |

### 验证

- `_bench/test-excel-hscroll.ps1`：点一下预览窗后 **横向滚动条拖动 True**、**Shift+滚轮 True**；
- `_bench/test-excel-interaction.ps1`（先点再滚）：**滚轮 True**、**拖条 True**；
- 机理记进 CONVENTIONS #138（本机 `HKCU\Control Panel\Desktop\MouseWheelRouting = 2`，悬停即滚）。

## 2026-11-01 · 1.7.9：补齐音频/视频/SVG/HTML 四类预览 + 三处"点一下就卡死/没反应"

### 用户要求（原话）

"功能缺口只用补音频、视频、svg、html 即可。这几块的实现比如视频可以参考原版"；
"图片的缩放，定死长宽比，只支持等比例缩放，文本等其他类型可以放开"；
"再检查下不同文件的切换，有时候切换不太流畅。office之间的切换因为滚动栏的原因
有时候会有黑边的渲染，延迟有点高。"

### 做了什么

1. **格式路由从 4 类扩到 8 类**（`crates/qlr-core/src/format.rs`）：新增 `Svg` / `Html` /
   `Audio` / `Video`，去掉 `TEXT_EXTS` 里的 html/htm/xhtml（css/js 仍是文本）。
   `KINDS` 常量 + `has_natural_size()` 供渲染层/尺寸计算统一取。
2. **四个新 Viewer**（`crates/qlr-render/src/`）：
   - `svg.rs`：D2D 光栅化 + 纯 Rust 兜底（渐变、圆角矩形、路径填充/描边）；
   - `html.rs`：轻量排版（标题/段落/列表/代码块/实体还原/`<pre>` 保留行首缩进）；
   - `media.rs`：Media Foundation 解码（`probe` 元信息 + `first_frame` 首帧 +
     `FrameStream` 逐帧流），H.264 靠 `MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING`
     让 Video Processor MFT 做 NV12→RGB32；
   - `audio.rs`（`qlr-platform`）：MCI 播放器（`open/play/pause/resume/seek/position/duration`）。
3. **播放 UI**（`app.rs` + `window.rs`）：底部 52 px 进度条（`paint_audio_bar`）、
   `P` 播放/暂停、`←/→` 快退/快进 5 s、滚轮定位；视频 40 ms 定时追帧（落后丢帧）、
   播完停表并提示；`hide_preview` 一律停播（不留后台解码器）。
4. **图片等比**（用户要求"定死长宽比"）：`window::paint_bitmap_fit`（`scale = min`、居中、
   HALFTONE），只对 `Image|Svg|Video` 生效；PDF/文本/Office 仍铺满。

### 三处"点一下就卡死/没反应"（本轮真正的 bug）

| 症状 | 根因 | 修法 |
| --- | --- | --- |
| 点一下 Word 预览窗，整个进程**卡死**（Windows 记 `Application Hang` 后杀掉） | `WM_ACTIVATE` 里**持锁**调 `SetFocus`；焦点切换把激活消息泵回自己的窗口过程，重入的处理器再拿同一把 `Mutex` → 自锁死（CONVENTIONS #128 的老坑换了个入口） | 句柄只在锁内取出，`SetFocus` 在锁外调；再加 `IN_FOCUS_TRANSFER` 重入闸门 |
| 拖边改大小在 Office 预览上"第一次点没用、第二次才行" | `WM_MOUSEACTIVATE` 返回 **2 = `MA_ACTIVATEANDEAT`**（激活**并吞掉**这次点击），未激活窗口上的第一次按下永远到不了 `WM_LBUTTONDOWN` | 返回 **1 = `MA_ACTIVATE`** |
| 视频拖动进度（`seek`）直接 `STATUS_ACCESS_VIOLATION` | `IMediaSourceReader::SetCurrentPosition` 的 `guidtimeformat` 传 NULL：MSDN 说表示 100 ns 单位，本机 mfreadwrite **直接解引用** | 传 `GUID::zeroed()`（等价 `GUID_NULL`） |

另外两处：视频每帧整块铺底会把进度条擦掉（新增 `window::picture_area` + `reserve_bar`）；
`AUDIO_FILL_RGB` 写成 `0x004A90D9`，而 COLORREF 是 `0x00BBGGRR`，实际画出来是橙色
（改成 `0x00D9904A`）。

### 验证（全部实机，release 二进制）

- `cargo test` **244 passed**（app 41 / core 48 / platform 25(+2 ignored) / plugin-api 4 /
  plugin-demo 7 / plugin-office 11 / render 98(+1 ignored) / fuzz 1 / malformed 9）；
  `cargo fmt --all -- --check` 0；`cargo clippy --all-targets -- -D warnings` 0。
- **新增 `scripts/verify-media.ps1`：15 PASS · 0 FAIL**（svg 彩色像素 37366；html 正文墨迹
  5119 且左侧 30 px 无行号栏；wav `AUDIO 打开`→进度条→`播放结束`；mp4 两帧差异 412 个采样点、
  底部浅色占比 83%）。截图 `_bench\media\01-svg.png` … `04-video-b.png`。
- `scripts/verify-office.ps1`：**22 PASS · 0 FAIL**（新增"pptx 预览窗口可见"；拖边 880x748 →
  980x818、改大小后内容高度 = 客户区−30；pptx 翻页 2/3、到最后一页给提示且不黑屏）。
- `scripts/verify-archive.ps1` **11 PASS**、`scripts/smoke.ps1` **全部通过**。
- Excel 焦点回归（1.7.8 的要求）：点一下之后 滚轮 True / 横向滚动条 True / Shift+滚轮 True。

### 仍未做

- 视频**没有声音**（MCI 在本机播不了 mp4：`type mpegvideo` → MCIERROR=277），
  要出声得再上 `IMFMediaSession` + EVR 或 WASAPI 混流；
- Excel 的滚轮依赖"先点一下"（OLE 预览的固有行为，与用户要求一致）。

## 2026-11-01 · 1.8.0：Office 预览改窗口大小的白边 / 画框与阴影不一致

**用户反馈**（原话）：「窗口伸缩会引起白边这个问题，之前是没有的哦」、
「1.78那会好像也没有问题，之前只是滚动有问题，窗口拉伸没问题」。

**先量清楚"到底是不是 1.7.9 引入的"**：用 `git worktree add ..\_old178 7a72f30` 单独编 1.7.8，
同一个探针对 `big-sheet.xlsx` / `office-demo.docx` 改三档尺寸，右缘 40 px 纯白占比
两版都是 **100%** —— 不是 1.7.9 引入的，而是"Office 走系统预览处理器"这条路线（1.7.6/1.7.7 起）
本来就有的：处理器自己建的子窗口（`EXCEL9 → EXCEL7`）不跟着我们的窗口长大。
1.7.2 时代 Office 走插件位图渲染，位图跟着重画，所以那时候"没问题"。

**修法**（不用重开预览，见 CONVENTIONS #145）：`WM_SIZE` 里 `window::post(ipc::WM_QLR_SHELLFIT)`
（发起同步呼叫里调 COM 会被 `0x8001010D` 拒），消息到了再把新矩形告诉处理器 —— 关键是
**锁外调** `SetRect`（持锁调用会把消息泵回自己窗口过程 → 自等 → Application Hang 1002）。
顺手补 #146：`WM_SIZE` 里同步投影窗口，否则"画框和阴影对不上"。

**验证**（release，`<便携测试目录>`）：
- `_bench\shell-resize-probe.ps1`：xlsx 右缘纯白 78.5%（改尺寸基线）/ 78.8% / 79.7% → 白边消失；
  docx 正文铺满整宽（截图 `_bench\shell-resize\final2-docx-b-1040x520.png`）；进程 `Responding=True`，
  事件日志无新增 1002。
- Excel 交互回归：`_bench\test-excel-hscroll.ps1` → 横向滚动条 True / Shift+滚轮 True；
  `_bench\test-excel-interaction.ps1` → 滚轮 True / 拖条 True。
- `scripts\verify-office.ps1` **23 PASS · 0 FAIL**（新增一条"拖边后处理器重新对齐尺寸"）、
  `verify-archive.ps1` 11 PASS、`verify-media.ps1` 15 PASS、`smoke.ps1` 全过、
  `cargo test` 244 passed、fmt/clippy -D warnings 干净。
- 版本 1.7.9 → 1.8.0（`Cargo.toml` + `crates/qlr-app/resources/qlrust.rc`）。

**试过并放弃的路**（别再走）：① `WM_SIZE` 里同步 `SetWindow/SetRect`（被拒）；② 投递消息但
**持锁**调 `SetRect` + 手工撑 `EXCEL9` + `fill_child` 兜底（卡死）；③ "改完大小重开预览"
（能用，但用户否掉：慢、费资源）；④ 换成进程外 `prevhost` 承载（`CoGetClassObject` +
`IClassFactory` 缓存）：白边同样消失、更隔离，但**Excel 滚动条拖不动了**（实测拖条 False），
所以又退回进程内。

## 2026-11-01 · 1.8.1：Excel 能点选单元格 / 图片视频锁窗口比例 / 去掉占位页

### 用户诉求（三条，原话）

1. 「Excel焦点聚焦到预览窗口的时候，鼠标左键无法选中目标单元格。」
2. 「图片、视频类都不需要拉伸缩放窗口，设计和图片一样的」；
   更早一条把口径说清了：「图片类型取消窗口的拉伸缩放，且窗口比例固定预览图片比例，
   同时注意不能破坏图像撑满窗口的设计」。
3. 「Excel预览窗口打开的时候我还是看到了一个占位页面，需要把占位页面直接删掉」。

### 做了什么

- **Excel 点选单元格**（`crates/qlr-platform/src/shellpreview.rs` / `crates/qlr-app/src/app.rs`）：
  `ShellPreview::set_com_focus()` → `IPreviewHandler::SetFocus()`；`focus_shell_content()` 改成
  锁外调 COM（先 `take()` 出句柄、`IN_FOCUS_TRANSFER` 防重入、失败退回 `window::focus_child`）；
  `WM_MOUSEACTIVATE` 只返回 `MA_ACTIVATE` 不抢焦点；处理器就绪（`TIMER_SHELL_INPUT` 轮询到
  `visible_area >= 50`）后再调一次。`take_over_input()` 只给 PPT，Excel/Word 不再被 `EnableWindow(FALSE)` 禁输入。
- **图片/视频锁窗口比例**：`State.media_aspect`（宽/高）在 `show_preview` 记、`hide_preview` 清；
  `window_size_for` → `media_window_size(work, aspect, width, bar)`；`on_resize_move` →
  `clamp_media_resize(rc, edges, aspect, bar)`；`repaint` 的 `Canvas::Bitmap` 只对 `Svg | Video`
  走 `paint_bitmap_fit`，图片**铺满**（窗口已经锁成素材比例，铺满即等比）。
  `bar = window::AUDIO_BAR_H(52)`，只有视频扣。
- **去掉占位页**：`show_preview` 里 `if shell_candidate(target) { lines.clear(); }`。
- `resize_margin(hwnd)`：有"活的"非 PPT 处理器时热区收进 `SHELL_RING_DIP`（4 DIP）那一圈，
  否则退回 8 DIP。版本 1.8.0 → **1.8.1**（`Cargo.toml` + `crates/qlr-app/resources/qlrust.rc`）。

### 怎么验证的（实测数字）

- 新增 `scripts/verify-mediashape.ps1`：**13 PASS · 0 FAIL · 0 SKIP**
  - Excel 左键点一下就选中单元格：点击前后画面差 **0.67%**；
  - Excel 改大小后右缘纯白占比 **41.4%**（<85 视为铺满，1.8.0 修的白边没退化）；
  - 图片 `1.778 → 1.777 → 1.778`、视频 `1.778 → 1.777 → 1.778`（已扣 52px 进度条）。
- 回归：`verify-office.ps1` **23 PASS**、`verify-archive.ps1` **11 PASS**、
  `verify-media.ps1` **15 PASS · 0 SKIP**、`smoke.ps1` 全过（托盘 SKIP = 管理员会话 UIPI）；
  `cargo test` **249 passed**、`cargo fmt --all -- --check` 干净、`cargo clippy --all-targets -D warnings` 干净。

### 踩坑（都写进 CONVENTIONS #147–#149）

- 焦点只到 `EXCEL9` 容器 → 表格"看得见、点不动"；往 `EXCEL7` 投合成鼠标消息没用。
- `IPreviewHandler::SetFocus` 在 `WM_ACTIVATE`/`WM_SETFOCUS` 里被拒（`0x8001010D`）；
  在 `WM_MOUSEACTIVATE` 里能把"这一次点击"吃掉 → 只能放到退栈后的定时器里。
- `smoke.ps1` 里 `Get-Content -Raw` 偶发返回数组，导致 `Check` 的布尔参数报
  "无法将值 System.Object[] 转换为类型 System.Boolean"；改成 `((Get-Content …) -join "`n")`。

## 2026-11-01 · 1.8.2：图片/视频去掉拖边改大小（缩放交给滚轮）

### 用户原话

「把图片和视频的窗口伸缩功能去掉，滚轮对窗口进行缩放就足够了。」

### 做了什么

- `crates/qlr-app/src/app.rs`：新增 `resizable_kind()`（`Image | Video` → false）；
  `resize_margin()` 对它们返回 **0**，`edges_at()` 在 `margin <= 0` 时短路（顺带避免
  "正好压线抓到一个 0 宽的边"）。`WM_SETCURSOR` / `WM_LBUTTONDOWN` 共用同一个 `resize_margin`，
  所以光标也不会变成双向箭头、点边也不会开始拖。
- 窗口大小只在打开时按素材比例定一次（`media_window_size` 不变）；`clamp_media_resize` 与
  `on_resize_move` 里那条比例分支保留作兜底。
- 新增 `window_aspect_differs(hwnd, w, h)`：图片在**外部改大小**（最大化 / `SetWindowPos` /
  DPI 变化）导致窗口比例和图片差 >2% 时退回 `paint_bitmap_fit` 等比居中，别把图拉变形。
- 版本 1.8.1 → **1.8.2**（`Cargo.toml` + `crates/qlr-app/resources/qlrust.rc`）。
- `scripts/verify-mediashape.ps1`：把"拖右边/拖下边后比例不变"换成更强的
  "拖右边/拖下边**不改窗口大小**"（新增 `Win-Size` helper），仍是 13 项。

### 怎么验证的（实测数字）

- `_bench\noresize-probe.ps1`（新增，真鼠标拖边 + `--wheel` 缩放）：
  - 图片：开窗 `707x428` → 拖右边 `707x428` → 拖下边 `707x428`；滚轮前后画面差 **56.48%**。
  - 视频：开窗 `1109x706` → 拖右边/拖下边都还是 `1109x706`。
- `scripts\verify-mediashape.ps1` **13 PASS · 0 FAIL · 0 SKIP**
  （图片 `640x390` 拖前拖后一致、视频 `640x442` 一致；Excel 点选/白边/占位页仍全 PASS）。
- 单测：`cargo test -p qlr-app` **48 passed**（新增 `edges_at_no_resize_for_media`、
  `resizable_kind_excludes_image_and_video`）。

### 备注

- 视频的滚轮仍是**快进/快退**（一格 5 秒，1.7.9 既定行为），不是缩放；图片的滚轮才是缩放。
- Windows 没有"拖边改大小"以外还能挡住最大化的干净办法，所以最大化后按等比居中处理。

---

## 1.8.2 之后：开源发布准备（2026-11-02）

### 用户原话

> 现在在本地全面测一遍ql原版和我们的版本的对比，没问题的话，我们开始改文件上传github
>
> 注意写一份给ai看的agent.md文档，告诉他所有的东西，开发规范等等，结构可参考
> https://github.com/huanfeng/WindInput

三个决策（问了才做）：**历史压成一个初始提交**、**`docs/dev` 全部保留只洗掉绝对路径**、**仓库名 `QuickLook-R`**。

### 做了什么

| 条目 | 做法 |
|---|---|
| 对比测试 | 18 夹具跑 `_bench\compare-run.ps1`：窗口出现 **59 ms（中位 60）vs 原版 221 ms（中位 197）**；首帧稳定 **450 ms vs 989 ms**（去视频 260 vs 603）；预览期内存 **34.5 MB vs 1007.1 MB**；原版空闲 253.8 MB / 句柄 1739。报告落 `docs/dev/COMPARE-live-1.8.2.md` |
| 不再跟踪 `dist/` | `.gitignore` 加 `/dist/`、`*.zip`、`_bench_tmp/`（原 `size-pack` 71.14 MiB，跟踪 473 文件） |
| 许可证 | 根 `LICENSE`（GPL-3.0 全文 674 行）+ `NOTICE.md`（来源、上游关系、第三方组件） |
| 行尾 | `.gitattributes`：`* text=auto`，源码/文档 LF，`.ps1`/`.rc` CRLF，二进制标 binary |
| 去绝对路径 | 22 处文档 + 3 个脚本 + 2 处单测夹具回退 = 27 处；`git grep 'Users.Administrator'` **0 命中** |
| 脚本参数化 | `verify-office.ps1` / `stress.ps1` / `verify-pdf.ps1`：显式参数 > `$env:QLRUST_FIXTURES` > 开发机默认 |
| AGENTS.md | 重写成 14 节 AI 协作手册（地图 / crate 索引 / 分层决策 / 类型矩阵 / 硬约定 / 改码规则 / 日志 / 构建测试 / 提交纪律 / 版本发布 / 文档义务 / 禁区 / 坑速查 / 协作） |
| README | 版本与支持表、性能对比表、验证命令、已知限制全部按现状重写 |
| 文档漂移 | `ARCH.md` 删掉已不存在的 `acrylic.rs` 描述；`STATUS.md` 里程碑 M3/M4 改成实际状态 |

### 怎么验证的

- `cargo test` **251 passed / 3 ignored**；`cargo fmt --all -- --check` 与 `cargo clippy --all-targets -D warnings` 干净。
- 修了一条发布准备自己引入的回归：单测夹具路径改成 `CARGO_MANIFEST_DIR` 推算后带 `..`，
  **MCI 不认带 `..` 的路径**，3 条 audio 测试在全量跑时失败；`canonicalize` 后恢复
  （`qlr-platform` 25 passed）。详见 CONVENTIONS **#151**。
- 五个端到端脚本的数字沿用 1.8.2（`verify-office` 23 / `verify-archive` 11 / `verify-media` 15 / `verify-mediashape` 13 / `smoke` 全过），本次只改脚本参数化与文档。

### 备注

- 发布走 `git checkout --orphan publish-1.8.2` → `git add -A` → 单条提交 → `gh repo create QuickLook-R --public --source . --push`；老历史留在本地。
- 之后每次发版仍按 AGENTS.md §10 的三处版本号同步（`Cargo.toml` / `qlrust.rc` / `README.md`）。

## README 对比表收敛 + 冷启动同口径复测（2026-11-02）

- 起因：README 的对比表第一行是「常驻后窗口出现 59 vs 221 ms（3.7×）」，读起来像"优势比早期缩水"；
  而 1.3.0 那份文档写的是「冷启动到首帧 10–14×」。两者**口径不同**（进程冷启动 vs 常驻进程再开第二个文件），不是回退。
- 复测（_bench\coldstart-3x.ps1，两工具各 3 轮取中位，夹具 img-photo.jpg 1280x720）：
  QLRust 出窗 **45 ms**（32/45/62）、稳定 **104 ms**（98/104/128）、内存 **13.6 MB**；
  原版出窗 **1174 ms**（1134/1174/1190）、就绪 **1222 ms**（1184/1222/1225）、内存 **163.6 MB**。
  空闲：QLRust 10.1 MB / 195 句柄 / 7 线程；原版刚启动 95.2 MB / 517 句柄，久驻 253.8 MB / 1739 句柄。
- README 把三张表（速度 / 我们更强 / 原版更强）合成**一张**：指标 | QLRust | 原版 | 差距，
  优劣同行对比（扩展名、字体、视频声音、生态标原版强；Office 三件、压缩包标我们强）。
- docs/dev/COMPARE-live-1.8.2.md 追加「冷启动复测」小节；GitHub Release 说明同步换成同一批数字。


## 2026-11-02 · 1.8.3：200% 缩放下的"界面小一半" + 空格偷按键

### 做了什么

**一、DPI（T01）**——17 个几何常量从"物理像素字面量"改成 `pub fn chrome_height() -> i32 { dpi::px(30) }`，
全部引用点（实测 161 处）换成函数调用；`TextStyle::height()` 的字号表改成 DIP 再换算；
`natural_rect_scaled` / `zoomed_rect` 的入参按"96 DPI 自然像素"处理（PDF 页 794×1123 = A4@96dpi）；
`dpi.rs` 加 `enter/px/seed_thread/system_dpi`，`enter` **每次真查** `GetDpiForWindow` 不缓存 hwnd；
补 `FontSet::clear()` + `Drop` + `fonts_invalidate()`，由 `dpi::set_change_hook` 挂在 DPI 变化上。

**二、热键（T02/T04）**——`action_hits` 改成无条件查 ctrl/alt/shift 三个修饰键（此前
"没声明的当 false"，导致 `Ctrl+Space`、`Alt+Space` 被吞）；给 `ZoomIn/ZoomOut` 加
`Action::ignores_shift()` 保住 `Shift+=`。钩子里新增 `foreground_is_text_entry()`：
`GetGUIThreadInfo(0)` 的 caret + 焦点窗口类名两级判据，前台在打字时不吞空格。

**三、输入（T03）**——`hide_preview` 补上 `page_edit = None` 与 `set_capture_digits(false)`；
删掉 `hook_proc` 里重复两遍的 `CAPTURE_DIGITS` 分支。

**四、图片默认缩放 50% → 100%**（schema 1→2 迁移，只有值恰好是 50 才迁，用户自设的不动）；
补上 `Config::migrate()`——此前它只写在文档里、函数根本不存在。

### 为什么

200% 缩放下窗口坐标是物理像素，而 30/24/15 这些常量是照 100% 写的 → 顶栏只有 15 DIP、
正文 7.5 DIP，用户看到"顶栏和内容都小很多"。图片再叠上默认 50%，几乎只剩一小块。

### 怎么验证的（数字）

- `cargo test` **264 passed / 3 ignored**（251 基线 + 13 新增：dpi 换算 6、config 迁移 4、
  keymap 匹配 2、开窗尺寸随 DPI 1）；`cargo fmt --check` / `clippy -D warnings` 零输出。
- **新脚本 `scripts/baseline-shot.ps1` + `baseline-diff.ps1`**：`PrintWindow` 截 5 类界面，
  逐像素对比。96 DPI 下三次改动的中间态**全部与改前逐字节一致**（`tolerance=0`）——
  这是"只改了缩放、没改别的"的唯一硬证据。
- `scripts/smoke.ps1`：17 PASS / 1 SKIP（管理员会话 UIPI，非回归）。
- 200% 视觉验收待用户真机（`scripts/collect-diag.ps1` 收集日志 + DPI + 7 条口述答案）。

## 2026-11-03 · 1.8.3-fix：批次一（评审 7 项里收 5 项 + 1 项自加）

来源：对 1.8.3 全仓 7 个 crate 的一轮代码评审（报告 `QuickLook-R-review-1.8.3.md`，包外文件、不入库）。
本批只做"小改动、高确定性"那一档：不碰渲染热路径，不动架构，不改变现有行为契约。

### 做了什么

1. **嗅探改有界读**（`qlr-core/src/format.rs::kind_for_path`）：扩展名不认识时的兜底嗅探原本用
   `std::fs::read` 把**整个文件**读进内存、却只取前 512 字节（`SNIFF_LEN`）。改成 `File::open` +
   `read(&mut [0u8; SNIFF_LEN])`，嗅探语义不变；12 个调用点全部受益（预览 .iso/.vhdx/无扩展名镜像
   时内存峰值从"文件大小"降到 512 B）。
2. **热键级 2 判据修死代码 + 钩子 stop 句柄泄漏**（`qlr-platform/src/hotkey.rs`）：
   `foreground_is_text_entry()` 的级 2 用 `GetFocus()` 取焦点窗口——`GetFocus` 是**线程亲和**的，
   钩子线程没有任何窗口，恒返回 NULL，所以级 2 自加进来就是死代码（上一节 1.8.3 条目里
   "前台在打字时不吞空格"这句其实没有生效）。改用同一次 `GetGUIThreadInfo` 输出里的
   `gti.hwndFocus`（跨线程有效），判据仍只认 `Edit` / `RichEdit20W` / `RichEdit50W` /
   `RichEditD2DPT` 四个类名，不放大吞键面；`GetGUIThreadInfo` 失败时两级都判否（与原行为一致）。
   顺带补 `HotkeyGuard::stop()` 的 `CloseHandle(stop)`（join 之后关、`self.stop` 归零防重关，
   顺序必须是 SetEvent → join → CloseHandle）与 `start()` spawn 失败路径的漏关。
3. **压缩包目录前缀只在新值时 leak**（`qlr-render/src/archive.rs`）：原本
   `dirs.insert(prefix.clone().leak())` 无条件先 leak 再 insert——`BTreeSet` 对重复值会丢弃，
   但泄漏已经发生。5000 个文件挤在同几层目录 = 上万次分配只有几条进集合，其余永久泄漏。
   两处插入点（目录条目 + 每级前缀）都先 `contains` 判重，`dirs` 计数不变。
4. **`PageCache` key 补文件戳**（`qlr-app/src/pagecache.rs` + `app/render.rs` 4 个调用点）：
   `PageKey` 只有 路径+页号+宽高，预览中文件被编辑器保存/下载覆盖后翻页/翻回会直接命中**旧像素**
   上屏——与 CONVENTIONS **#112**（`FrameKey` 就带了 mtime+len）是同一个坑。`PageKey` 增加
   `mtime` / `len` 字段，`new()` 增加 `stamp: (u64, u64)` 参数，调用点统一用现成的 `file_stamp()`；
   新增单测 `stale_stamp_does_not_hit` 钉住"mtime 或 len 任一变化都不命中"。
5. **相邻页预取复用 key 里的文件戳**（同上，本批自加）：`prefetch_neighbor_pages()` 原先对同一个
   文件再查一次 metadata，且查到的戳可能与触发它的 key 不一致（文件在这中间被覆盖时前后页会拿到
   不同版本）；同一帧里前后页必然同属一个文件版本，直接沿用 `key.mtime` / `key.len`，省一次系统调用。
6. **`pack.ps1` 先生成 SHA256SUMS 再压缩**：原本 `Compress-Archive` 在写清单之前，zip 里根本没有
   校验和文件。现在先算清单、写文件，再压缩（清单本身不在清单里，属正常）。

**没入库的两项**：

- `ShellPreview::close()` 的"双重 Unload/DestroyWindow"经核**只存在于死代码**：`close()` 在整个
  `crates/` 里零调用点（实际收尸走 `app/shell.rs` 的 `drop(pending)`，`Drop` 本就只跑一次）。
  评审建议的 `std::mem::forget(self)` 会连 `handler: IPreviewHandler` 的 `Drop` 一起跳过
  （`IUnknown::drop` 调 `Release`）——用无害的死代码换一个真 COM 引用泄漏，划不来，本批不收。
  真要收拾就写成 `pub fn close(self) { drop(self) }`，或直接删掉这个方法。
- 热键那项原本拆成两个提交（后一个只补前一个漏传的 `GetClassNameW` 实参、否则编译不过），
  本批合并成一个逻辑改动，保证**每个提交都能编译**。

### 为什么

这几项全是"平时看不见、踩到就难看"的坑：GB 级未知文件顶满内存、Win11 桌面 F2 改名吞空格、
常驻进程连看 N 个压缩包内存线性涨、PDF 被覆盖后显示旧页、发布 zip 缺校验和。
它们都不改变任何行为契约，所以可以一次性合入，不需要视觉基线。

### 怎么验证的（数字）

- `cargo fmt --all -- --check`：零输出（本批自带一处 fmt 违规——`render.rs:522` 单行超长、
  `hotkey.rs:34` import 换行——已单独 `style(fmt)` 提交修掉）。
- `cargo clippy --all-targets -- -D warnings`：零输出。
- `cargo test`：**287 passed / 4 ignored / 0 failed**（app 62、core 54、platform 33+3、
  plugin-api 4、plugin-demo 7、plugin-office 11、render 106+1、fuzz 1、malformed 9）；
  较 1.8.3 基线 **+1** 个新单测（`pagecache::tests::stale_stamp_does_not_hit`，静态计数 290 → 291）。
  上一节 1.8.3 条目记的"264 passed"是发布当时的数字，实测基线已到 286，别再照抄 264。
- `cargo build --release`：通过。
- `scripts/smoke.ps1`（release 构建）：**16 PASS · 1 SKIP · 0 FAIL**，覆盖 热键安装/热键处理链/
  滚轮缩放/图片渲染 `elapsed=74ms`/干净退出 与"退出时卸载钩子"；SKIP 那条是托盘图标——
  当前会话以管理员身份运行、explorer 非提权，`Shell_NotifyIconW` 被 UIPI 拒（0x80070005），非回归。
  热键路径按 AGENTS §8 属于"改了就必须跑 smoke"的范围，已跑。
- `scripts/verify-archive.ps1`：**11 PASS · 0 FAIL**——压缩包前缀改动在这里（目录树解析、
  点目录行折叠/展开、滚轮按行滚动、`.cbz/.jar/.war` 接管、坏包不显示也不走渲染通道）。
- `scripts/verify-pdf.ps1 -Pdf target\bench-fixtures\pdf_8page.pdf`：**PASS**（页缓存改动在 PDF
  翻页路径上；`SHOW 22ms` / `RENDER 5ms 619x876`、放大 0 残留）。
- `verify-office.ps1` **未跑**：本批不碰 Office 路径，留到下次动 `IPreviewHandler` 时做防回归。


## 2026-11-03 · 1.8.3-fix 批次二：把"每次重来"的开销收掉（7 个提交）

### 做了什么

评审报告的 🔴 分组里"真正影响体感"的那部分。每项都先量后改，数字都来自 release 构建。

1. **JPEG 按解码档位缓存**（`4352c0f`）：`render_jpeg_scaled` 原本"窗口一变就重解码"——
   JPEG 分支在 `render_via_image`（唯一带 `source_cache` 的路径）之前就 `return` 了。
   现在用 `jpeg-decoder` 的 `scale()` 返回的**实际解码尺寸**当档位标签（`source_cache` 加
   `Tag`，`lookup_tagged` / `store_tagged`；`lookup` / `store` 退化成 `TAG_FULL` 包装），
   命中就只缩放、不碰解码器。是否可缓存按**解码后**像素数判定，所以 48MP 的 1/4、1/8 档也进得来。
   命中与未命中走同一条 `finish_rgba` → 不会出现"缓存命中反而差一级色"。
2. **PDF 文档单槽缓存**（`a2a6439`）：一次预览原来把整个文件读 3 遍、pdfium 解析 3 遍
   （`natural_size` 取第 1 页尺寸开窗 + `page_count` 显示"第 x / N 页" + `render_page` 出图）。
   现在只有一格文档槽，按路径 + mtime + 长度复用；文件变了整份换掉；>256 MiB 的特大文档不常驻
   （用完即关，钉住整份文件比多读一次盘更伤内存）。
3. **Markdown 排版按预算早停**（`d38506a`）：`head` 模式下原来排完全文再截到 `max_lines`。
   现在 `layout_capped(lines, max_out)` 排满就停手（一行源文本最多产出 1 行显示文本，
   所以"排到够"与"排完再截"结果完全一致）；`total_lines` 仍是源文件行数，状态行不受影响。
4. **HTML 隐藏元素一次扫描剥离**（`c0af9f5`）：原来是 13 个标签各扫一遍全文件（`head` 单独一遍 +
   script/style/noscript/… 各一遍），每次还复制一份 8 MB 新字符串。改成 `strip_hidden_all` 单遍：
   遇 `<` 读标签名、命中就跳到闭标签之后（自闭合跳过、无闭标签时后面全算元素内部）。
   顺带给 `find_ci` 加首字节粗筛——原来每个位置都调一次 `eq_ignore_ascii_case`。
   **HTML 故意不早停**：状态行的"全文共 N 行"是排版后的显示行数，排一半就算不出准确值
   （Markdown 报的是源文件行数，才不受影响）。
5. **缩放内循环去掉逐字节边界检查**（`f76e1ec`）：JPEG 进缓存之后，剩余开销的大头变成缩放本身。
   内层从"按下标逐通道取值"改成"整行切片 + `chunks_exact(4)` + 行内局部累加器"，
   取整口径一个字没动（`downscale_matches_image_crate` 仍逐字节对拍 `image::imageops::thumbnail`）。
6. **帧级分配计数**（`58c53d2`）：新增 `alloc-stats` 特性（默认不编、正式产物零开销），
   开时换记账分配器并多打一行 `RENDERALLOC gen=N bytes=… live=…`。**不塞进 `RENDER` 行**——
   那行的字段顺序被 `bench.ps1` 与现场 grep 依赖着。
7. **`bench.ps1` 补三列 + GDI 泄漏判据 + `-Passes` 浸泡**（`57fdcb3`）：PeakPrivateMB、
   GdiAfterHide/UserAfterHide（解析 app 的 `HIDE … GDI=n USER=n`）、AllocKB/LiveKB；
   同一进程里连续开合，第 2 次起收起后对象数不得再涨（> 基线 + 2 判红）。

### 为什么

批次一收的是"平时看不见、踩到就难看"的坑；这一批才是用户能感觉到的：滚一格要重解码整张
4K/48MP 图、翻一页 PDF 要重读整个文件、打开 8 MB 的 md/html 要排版全文。它们全在渲染热路径上，
所以**每一项都先建 `#[ignore]` 基准、拿基线、再改**，改完再把新数字写进提交说明——
不许拿评审报告的估算当实测。

### 怎么验证的（数字）

`cargo fmt --all -- --check` / `cargo clippy --all-targets -- -D warnings`：零输出
（`--features alloc-stats` 下 clippy 也是零）。`cargo test`：**290 passed / 9 ignored / 0 failed**
（app 62、core 54、platform 33+3、plugin-api 4、plugin-demo 7、plugin-office 11、render 109+6、
fuzz 1、malformed 9；新增 5 个 `#[ignore]` 基准与 1 个分配计数自测）。

性能（release，每项都是同机同夹具的前后对比）：

- **JPEG 缩放四档**（`bench_jpeg_zoom_steps`，1589 / 1986 / 3178 / 回到 1589）：
  4K 3840x2160 合计 170.0 → 90.5 ms（−47%）；48MP 8000x6000 合计 367.6 → 78.1 ms（−79%）。
  分档看，冷档 42–47 ms（4K）、147–197 ms（48MP）掉到 10–12 ms；几乎不缩的 3178 档仍有
  46 ms（4K）——那部分是缩放本身，见下面的 `bench_downscale`。
- **PDF 200 页合成夹具**（`bench_pdf_costs`）：`render_page` 3.22 → 1.25 ms（word 版 228 KB）、
  2.77 → 1.38 ms（80 KB 版）；冷加载 0.54 ms、命中槽 0.01 ms。
  合成夹具体量太小（最大的才 228 KB），真实几十 MB 扫描件的收益更大但仓库里没有这种夹具，未实测。
- **Markdown 8 MB / 29.7 万行**（`bench_md_phases`）：`head` 400 行 356.1 → 15.7 ms（22.7×）、
  `MAX_LINES` 353.1 → 240.2 ms；分阶段 fs::read 2.9 / decode 5.6 / split 5.7 / layout 全量 338 ms。
- **HTML 8 MB / 23.4 万显示行**（`bench_html_phases`）：预处理 145.0 → 8.8 ms（16×）、
  `layout` 全量 245.1 → 122.7 ms、`head` 400 行 260.0 → 120.7 ms；剩余开销是 tokenize 33 ms。
- **缩放**（`bench_downscale`）：4K → 3178 51.4 → 46.1 ms、4K → 1986 28.3 → 21.6 ms、
  1080p → 1589 12.7 → 11.5 ms、48MP → 3178 100.6 → 71.2 ms。
- **帧级分配**（alloc-stats 构建，4K JPEG → 1986x1117 位图 5.4 MB）：冷帧分配 65.7 MB、
  热帧 5.6 MB（≈ 位图本身），live 稳定 19.2 MB 不随帧数增长。
- **GDI/USER 浸泡**（4K，6 次开合）：GDI 16 → 20 → 20 → 20 → 20 → 20，USER 15 → 16 → 16…
  **没有 GDI 泄漏**——也反证批次一撤掉 `mem::forget` 是对的（那个泄漏本来只存在于死代码里）。

端到端（release 构建，本机交互式会话）：`smoke.ps1` **16 PASS · 1 SKIP · 0 FAIL**
（SKIP = 管理员会话 UIPI 拒托盘图标）、`verify-archive.ps1` **11 PASS · 0 FAIL**、
`verify-pdf.ps1` **PASS**（SHOW 8ms / RENDER 2ms）。

### 留给下一批

- HTML 的 tokenize 仍是整篇 33 ms（要再快得改成流式词法）；
- 帧缓存/内存预算闸门（STRUCTURE-REVIEW §6-7、§6-9）、持锁动窗口（§7-1）、几何单点化（§7-2）；
- PDF 页缩略图、文本 mmap、A1.6 第三/四级兜底；
- **200% 缩放的视觉验收仍待用户真机**（本批没动任何几何/DPI 逻辑，理论上不受影响）。

## 2026-11-03 · 1.8.3-fix 批次三：拿掉"持锁贴帧"、给分层与缓存总量装上断言

> 承接批次二（见上一节）。批次三只做 `docs/dev/STRUCTURE-REVIEW-1.8.3.md` §6/§7 里
> **先量过、确认还成立**的几项；量完发现不成立的，把结论写下来而不是硬改。

### 做了什么

**① 视频帧改 `Arc` 共享，贴图移出 `STATE` 锁；锁中毒不再静默哑火（`8dd38dc`）**

- `VideoPlay::last` 从 `Option<RawFrame>` 改成 `Option<Arc<RawFrame>>`；`video_paint` / 顶栏
  进度条都改成"锁内只克隆 `Arc`、锁外贴图"，锁的持有时间从"整帧拷贝 + blit"降到"指针 +1"。
- `with_state` 原来 `STATE.get()?.lock().ok()?`——锁一旦中毒（持锁 panic 过），**之后每一次
  `with_state` 都静默返回 `None`**，整个程序变成"窗口还在、什么都不响应"，日志里一个字都没有。
  改成 match：`Err(poisoned)` 时打 `log_error!` 再 `into_inner()` 取回继续（与 #128 的范式一致）。
  289 处调用点因此统一。

**② 新增 `scripts/verify-layers.ps1`：把 ARCH 的分层描述变成可执行断言（`3a0d720`）**

- 11 条断言：6 条依赖方向（core 无 `windows`、无上层；platform 不含 render/app 且必须有 `windows`；
  render 不含 platform/app；app 三向齐全）+ core 源码零 `use windows::` + render/app 的 `use windows::`
  白名单 + platform 不许 `use qlr_render::`/`qlr_app::` + `main.rs` 零 `use windows`。
- `ARCH.md` 的分层表按代码事实重写：render **不是**"不依赖 windows"，只有 `media.rs`（Media Foundation）
  与 `svg.rs`（D2D/D3D/DXGI）两处边界例外；app 的存量是 `layout.rs` 的 `RECT`、`settings.rs` 与
  `app/mod.rs` 的 prelude，脚本用**计数棘轮**（`layout.rs` ≤1、`settings.rs` ≤2）保证只减不增。

**③ 三处缓存共用 256 MB 总预算，上限从"加法"变成真上限（`915ef49`）**

- 新模块 `crates/qlr-render/src/budget.rs`：一本全局账（`AtomicUsize` + CAS 循环），
  `try_reserve(n) -> Option<Permit>`，`Permit` **drop 即归还**；没有中心调度器、没有回收回调——
  拿不到额度的一方自己先淘汰自家旧条目，还不行就不缓存（缓存只影响快慢，不影响对错）。
- 接入三处：`source_cache::store_tagged`、`pdf::with_document`、`pagecache::put`；
  换文件时先放掉旧文档槽再申请额度（否则"换一份同样大的文档"会自己把自己挡在预算外）。
- 渲染线程每帧多打一行 `MEM used=..MB cap=256MB`（单独一行，不动 `RENDER` 的字段顺序）。

### 为什么

- **持锁碰窗口/COM 是这个项目的历史主坑**（#128/#139/#145）。`hide_preview` 早就改成"锁内只改
  状态、`window::hide` 放锁外"，但视频通道仍是"锁内贴整帧"：720p 一帧 3.6 MB，每帧在锁里多拷
  一次就是几百微秒的窗口期，UI 线程一卡，钩子回调就可能超时（ADR-0005：超时 10 次钩子被 Windows 静默摘链）。
- **锁中毒静默失败**比崩溃更难查：现场只会说"预览器不动了"，日志里没有一行错误。
- **ARCH 的分层描述已经在撒谎**：写着"render 不依赖 windows"，实际 `Cargo.toml` 里就有。
  文档骗人比没文档更糟——接手的人会照它做决策。断言脚本是唯一能防漂的东西。
- **上限是加法不是上限**：源图 64 MB + 页缓存 64 MB + 上一帧 24 MB + 文档槽 256 MB ≈ 408 MB，
  再加渲染中一帧 32 MP（128 MB）能摸到 500 MB。文档槽对大文件是真收益（100 MB 扫描件冷加载
  264.63 ms、命中槽 0.02 ms），但不能靠"每处各自守上限"来控制总量。

### 怎么验证的（数字）

- `cargo test` **298 passed / 10 ignored**（app 63+1、core 54、platform 33+3、plugin-api 4、
  plugin-demo 7、plugin-office 11、render 116+6、fuzz 1、malformed 9）。新增：`budget` 7 条单测
  （其中 `the_three_caches_fit_or_the_big_document_stays_out` 把"233 MB 进得来 / 200 MB 文档 +
  64 MB 缓存进不来"钉死）、`video_frame_is_shared_not_copied`（`Arc::ptr_eq` + `strong_count==2`）。
- `cargo fmt --all -- --check` 与 `cargo clippy --all-targets -- -D warnings`（含 `--features alloc-stats`）零输出。
- `#[ignore] bench_video_lock_hold`（720p 3.6 MB）：整帧克隆 **579262 ns** vs `Arc` 克隆 **12 ns**。
- 端到端：`smoke.ps1` **16 PASS · 1 SKIP · 0 FAIL**（SKIP=管理员会话 UIPI 拒托盘 `0x80070005`）；
  `verify-media.ps1 -Video <720p mp4>` **15 PASS · 0 FAIL · 0 SKIP**（视频自动播放、两帧不同、
  进度条留白、播完收尾）；`verify-pdf.ps1` 用 100 MB 填充件 **PASS**；`verify-layers.ps1` **11 PASS**，
  反向测试（塞一个 `use windows::` 进 render）→ **10 PASS · 1 FAIL**、退出码 1。
- 预算实测（`app.log` 里的 `MEM` 行）：100 MB 填充 PDF → `MEM used=100MB cap=256MB`，
  `SHOW ... elapsed=268ms`（=冷加载 264 ms + 开窗）、`RENDER gen=1 ... elapsed=0ms`（页码走已常驻
  文档，不再读盘）；8 MP JPEG（1986 宽）→ `MEM used=39MB`（一个解码档位），小图档位 7 MB。

### 顺手核掉的过期项（评审 §6 十项现况）

| 评审项 | 现况 |
|---|---|
| §6-1 持锁动窗口 | **已过期**：`hide_preview` 早已"锁内改状态、`window::hide` 锁外"（`app/show.rs` 里有注释直引 #1/#128）；本批把视频通道也改掉了 |
| §6-2 几何单点化 | **已过期**：`crates/qlr-app/src/layout.rs` 已有 `Layout`/`Opts`/`Bar` 与四个构造入口，模块头写明 1.8.3 前是四处各算一遍 |
| §6-3 `app.rs` 拆分 | 已完成（`app/` 下 10 个文件，最大 `render.rs` 1315 行） |
| §6-4 裸 Win32 收口 | 未做（`platform` 已过半，剩 `stop_timer_by_ids`/`post_quit`/`now_ms` 这类批量搬迁，纯重构、有回归风险） |
| §6-5 渲染 worker 可重建 | **已过期**：`RENDER_EPOCH` 换代线程 + `QLRUST_RENDER_HANG_MS` 钩子 + `check_render_hang()`，1.8.3 已实现 |
| §6-6 折行结果缓存 | 未做（大文件滚动时的下一个候选，需先量 `paint_code` 的 wrap 占比） |
| §6-7 `PaintBuffer` 复用 / 每帧 GDI 计数 | 部分做了：`bench.ps1` 已有 `GdiAfterHide`/`UserAfterHide` + 泄漏判据；`PaintBuffer` 复用未动 |
| §6-8 空闲唤醒降频 | **实测不值得做**：预览窗开着的 30 s 内 CPU 只有 **15.6 ms（单核 0.05%）**，且这还是在 debug 日志开着（30 s 记了 331 条 `wm=0x0113`）的情况下量的。把 `FOLLOW` 400→1500 ms、`WATCHDOG` 250→1000 ms 只能省下零头，却会增加"选中项跟随延迟"与"挂死发现延迟"，不做。 |
| §6-9 内存总预算 | **本批已做**（`budget.rs`，256 MB） |
| §6-10 文档-代码对齐 + 分层断言 | **本批已做**（`verify-layers.ps1` 11 PASS） |

### 下一步（未开工）

- §6-6 折行结果缓存：先给 `text`/`code` 的 wrap 阶段加一条 `#[ignore]` 基准（大文件逐行 wrap 的
  占比），再决定要不要按 `(text_id, width, style_rev)` 缓存 break offsets。
- HTML 流式词法（8 MB 预处理已降到 8.8 ms，但 tokenize 32.8 ms + layout 122.7 ms 还在）。
- `AGENTS.md` §8 的"当前基线（1.8.2 / 251 passed）"早已过期，应与本节数字同步（本次未改，
  因为基线还会随批次变化）。

## 2026-11-03 · 1.8.3-fix 批次三（续）：HTML 排版改成流式词法

### 做了什么

`crates/qlr-render/src/html.rs` 两处热路径（提交 `eca8801`）：

- `tokenize(src) -> Vec<Tok>` 换成 `for_each_token(src, |tok| …)`，标签名复用一个 `String` 缓冲；
  `Tok` 的变体从 `String` 改成 `&str`（HRTB `for<'a> FnMut(Tok<'a>)`）。
- `Builder::push_text` 不再逐字符 `push`，改成按"非空白段"整段 `push_str`
  （ASCII 空白按字节判；非 ASCII 字节才解 `char` 问 `is_whitespace()`）。
- `bench_html_phases` 改成每个阶段跑 3 遍取最好成绩，并新增一条词法计时
  （原来只有"tokenize 总数 + layout 全量"两个数，看不出排版侧占多少）。

### 为什么

8 MB 的 HTML 有 96 万个 token：先攒 `Vec<Tok>` 再消费，光结构体就约 46 MB，
每个标签名还要各分配一个 `String`。而排版是严格顺序消费、从不回看的，
中间态完全是白付的。这是"读大文件首屏"这条链上最后一处结构性的浪费
（前面的单遍剥离已经把预处理从 145 ms 压到 8.8 ms）。

### 怎么验证的（数字）

release，8 MB HTML → 233638 显示行，各阶段取 3 遍最好：

- 词法：38.1 → **10.2 ms**（3.7 倍）
- `layout` 全量：122.7 → **74.8 ms**（−39%）
- `render_with(head=true, 400 行)`：260.0（1.8.3 基线）→ 120.7（批次二单遍剥离后）→ **87.1 ms**
- 峰值少一份 `Vec<Tok>` ≈ 46 MB，另加近百万个标签名小分配

`cargo test` **299 passed / 10 ignored / 0 failed**（render 117+6），fmt/clippy 零输出；
`scripts/verify-media.ps1` **15 PASS · 0 FAIL · 0 SKIP**（HTML 走排版预览、正文有内容、无行号栏）。

### 顺手记下的

`push_text` 那条单独测时落在噪声里（本机单次抖动 ±6 ms），提交说明里只报"两项合起来"的数字，
并把基准改成 3 遍取最好——见 CONVENTIONS #158。

### 下一步（未开工）

- 每行落盘的分配（`emit_line`/`flush` 里 `trim().to_string()` + `format!` + `clip` 三次分配，
  233638 行 ≈ 56 ms）：要么改成"一个可复用缓冲拼好再 push"，要么让 `clip` 就地截断。
- Markdown 侧同样有 `layout` 全量 240 ms 的余量（早停只解决首屏）。
- §6-6 折行结果缓存、§6-7 `PaintBuffer` 复用（先各加一条基准再决定做不做）。

## 2026-11-03 · 1.8.3-fix 批次四：滚一下卡在哪（折行量宽 + HTML 每行分配）

### 做了什么

两项（提交 `820994f`、`69b9d1a`）：

- `crates/qlr-render/src/html.rs`：`emit_line`/`flush` 每行 3–4 次分配改成一次——字段级借用
  分离替代 `mem::take` + `trim().to_string()`，新增 `clip_line(prefix, text)` 一次
  `with_capacity` 拼装（截断语义与 `text::truncate_chars` 逐字符对齐，单测对拍），
  `emit_row` 的 `join(" │ ")` 改成循环 `push_str`。
- 折行两层：`crates/qlr-render/src/wrap.rs` 的 `fit_prefix` 从"逐字符累加宽度"改成**二分**；
  新增 `crates/qlr-app/src/wrapcache.rs`（按 `(行号, 样式)` 缓存折行结果，512 条，
  换宽度/换文件即清空），`chrome.rs` 绘制循环先查缓存再折。
- `crates/qlr-app/src/app/chrome.rs`：绘制循环补折行计时与量宽计数，`WRAP` 行追加
  `量宽 N 次 / x.x ms · 缓存命中 h/m（存 k 条）`，≥4 ms 额外打 `SLOWWRAP`。

### 为什么

上一批（#158/#159）把 HTML 首屏压到 87 ms 后，用户侧的"滚一下卡"却没有数字可查：
`SLOWPAINT` 只覆盖真正的 GDI 绘制，不含折行。补上度量后立刻看到两处真问题：

1. 量宽是逐字符累加的——宽度对前缀长度单调，本该二分。长段落文档每滚一格要量 1359 次
   `GetTextExtentPoint32W`（单次约 21 µs），合计 12.1 ms。
2. 滚一格时屏上 24/29 行没变，每帧却把它们重新量了一遍。

### 怎么验证的（数字）

release，真窗口（WMI 起进程 + `--scroll 5` ×6，读 `app.log` 的 `WRAP`/`SLOWWRAP` 行）：

- 长段落文档（新夹具 `target/bench-text/longlines.txt`，25,240,000 B、6648 行 × 约 340 字）：
  每滚一格 **1359 次量宽 / 12.1 ms → 243 次 / 5.1 ms**（−58%）。
  该文档折行缓存命中率 **0/20**（一行折 7 行、一屏 4 个原始行、滚一格整屏换）→ 如实记，不吹。
- 短行文档（`spike\fixtures\big_100mb.txt`，121,425 行）：**29 → 5 次量宽 / 0.2 → 0.1 ms**
  （缓存命中 144/203）。
- HTML（`crates/qlr-render` 的 `bench_html_phases`，8 MB）：`layout` 全量 74.8 → **51.2 ms**，
  `render_with(head, 400)` 87.1 → **62.9 ms**。

`cargo test` **306 passed / 10 ignored / 0 failed**（app 67+1、core 54、platform 33+3、plugin-api 4、
plugin-demo 7、plugin-office 11、render 120+6、fuzz 1、malformed 9），fmt/clippy 零输出。

### 走过的弯路

缓存第一版只存"折成多行"的结果，短行文档一次都命中不了（`命中 0/203`、`存 0 条`）——
因为单行的结果被 `if fresh.len() > 1` 挡在缓存外。改成单行也存才拿到 24/29 的复用，
见 CONVENTIONS #161。

### 下一步（未开工，都有数字依据）

- `GetTextExtentExPointW` 一次拿到"给定宽度能放下几个字符"，可把长段落那 243 次量宽压到约 33 次
  （5.1 ms → 约 0.7 ms）。要改 `wrap::push_wrapped` 的度量回调签名，且"能放下几个字符"与现在
  "宽度严格小于"的口径可能差一个字符，需要另建对拍测试。
- §6-4 裸 Win32 收口残量、§6-7 `PaintBuffer` 复用 + 每帧 GDI 计数：仍留到下一批。
- Markdown 侧 `layout` 全量仍有 240 ms 余量（早停只解决首屏）。

## 2026-11-04 · §6-4 裸 Win32 收口（平台层完善）

### 做了什么

把 app 层最后 6 处直接调用的 Win32 计时/退出/时钟原语收进 `qlr-platform::window`，
app 只编排、不碰 FFI：

- `crates/qlr-platform/src/window.rs` 新增 4 个薄封装（紧挨 `start_timer`/`stop_timer`）：
  `stop_timers(hwnd, &[usize])`、`post_quit()`、`terminate_self(code)`、`now_ms()`。
- `crates/qlr-app/src/app/mod.rs`：prelude 不再再导出 `GetTickCount / GetCurrentProcess /
  TerminateProcess / KillTimer / PostQuitMessage / SetTimer`。
- 调用点改写：`app/boot.rs`（9 处：退出看门狗 `terminate_self`、热键时间戳 `now_ms`、
  `quit`/`WM_DESTROY` 的 `post_quit`、AUDIO/TOAST/SHELL_INPUT/SHELL_SWAP 的 `stop_timer`/`start_timer`）、
  `app/shell.rs`（3 处）、`app/chrome.rs`（1 处）、`app/show.rs`（3 个 `KillTimer` 合成一次 `stop_timers`）、
  `app/input.rs`（1 处 `now_ms`）。
- `scripts/verify-layers.ps1` 新增断言「app 不直接调 Win32 计时/退出原语」（扫
  `\b(KillTimer|SetTimer|PostQuitMessage|TerminateProcess|GetCurrentProcess|GetTickCount)\s*\(` 必须为 0）：
  分层断言 11 → **12 PASS**。

### 为什么

§6-4 是"后端契约 / 后端抽象 / 插件"的前置：app 直接摸 Win32，平台层就永远只是"薄封装的样子"，
换实现、加退出看门狗、做多后端都收不住。把边界钉成 `verify-layers.ps1` 的计数棘轮
（只许减不许增），口头规矩才不会漂（该脚本注释记着"漂移过三次"）。

### 怎么验证的（数字）

- `cargo fmt --all -- --check` / `cargo clippy --all-targets -- -D warnings`：零输出。
- `cargo test`：**306 passed / 0 failed**（app 67+1、core 54、platform 33+3、plugin-api 4、
  plugin-demo 7、plugin-office 11、render 120+6、fuzz 1、malformed 9），与改前一致。
- `scripts/verify-layers.ps1`：**12 PASS · 0 FAIL**（新增那条 PASS，旧 11 条不动）。
- 纯重构、无热路径改动：计时器/退出/时钟都不在 `repaint`/滚动/折行循环里，每个调用只多一层
  普通函数（<1%）开销；零分配、内存与包体基本不变。

### 下一步（未开工）

- §6-7 `PaintBuffer` 复用 + 每帧 GDI 计数；`GetTextExtentExPointW` 折行量宽（5.1 → 约 0.7 ms）。
- 之后才动"后端契约"第一刀（PreviewBackend + 现有自研渲染接入，无 sidecar）。

## 2026-11-04 · 折行量宽改 GetTextExtentExPointW（平台层完善，第 2 项）

### 做了什么

- `crates/qlr-platform/src/window.rs`：`Measurer` 抽出私有 `select_style(&self, style)`
  （原 `width_styled` 里内联的换字体逻辑），新增
  `pub fn fit_width_styled(&self, style: TextStyle, text: &str, width: u32) -> u32`——
  一次 `GetTextExtentExPointW` 问"给定宽度能放几个 UTF-16 码元"，用 `ch.len_utf16()`
  累加换算成 UTF-8 字节返回。
- `crates/qlr-render/src/wrap.rs`：新增 `type FitFn<'a> = &'a dyn Fn(&str, u32) -> usize`、
  `pub fn push_wrapped_fit(...)`；`push_wrapped` 委托 `push_wrapped_inner(..., None, ...)`；
  `fit_prefix` 增加 `fit: Option<FitFn>`：`Some` 走"一次适配"取 `lo`，`None` 走原二分；
  并对 `lo` 加"必须落在字符边界，否则回退到上一个边界"的保险。
- `crates/qlr-app/src/app/chrome.rs`：折行改调 `push_wrapped_fit` 并统计 `fitted`；
  `WRAP`/`SLOWWRAP` 日志增加「适配 N 次」。

### 为什么

二分每行要 `log₂(len)`（约 10–24）次 `GetTextExtentPoint32W`，是长段落滚动折行主成本。
`GetTextExtentExPointW` 一次调用就返回"宽度内能放几个字符"，把它压到常数级。

### 口径变化（重要）

既有二分在个别 CJK 尾段会**少填一个字符**（如 width=120 时 6 字 96px 而非 7 字 112px）；
加速通道按"能放下 ≤ width 的最长前缀"的线性口径填满，故与旧路径**不再逐行相等**——
有意统一到线性口径。影响：长 CJK 段落每行最多多填 1 字符（更优），短行 / ASCII 不变。
新增单测 `fit_path_matches_linear_reference` 以 `fit_prefix_linear` 为唯一 oracle 全程对拍。

### 怎么验证的（数字）

- `cargo fmt --all -- --check` / `cargo clippy --all-targets -- -D warnings`：零输出。
- `cargo test`：**307 passed / 0 failed**；`scripts/verify-layers.ps1`：**12 PASS · 0 FAIL**。
- 真窗口（`longlines.txt` 6648 行 / 宽 756px / 屏幕 29 行，滚 3 行，`QLRUST_LOG=debug`）：
  每滚 **量宽 243 → 30 次 + 适配 24 次**，折行耗时 **5.1 → 3.3 ms**
  （首帧冷启 9.4 ms 含首次选字体）。注：HANDOFF 原估 0.7 ms 偏乐观，实测 3.3 ms；
  剩余成本主要是 GDI 对长 CJK 串本身逐字测量，**调用次数已降一个量级**（约 54 次/滚 vs 243）。

### 下一步（未开工）

- §6-7 `PaintBuffer` 复用 + 每帧 GDI 计数；后端契约第一刀（PreviewBackend，无 sidecar）。

## 2026-11-04 · §6-7 PaintBuffer 复用 + 每帧 GDI 计数（平台层完善，第 3 项）

### 做了什么

- `crates/qlr-platform/src/window.rs`：`PaintBuffer` 增加 `w/h` 尺寸字段；新增线程本地缓存
  `PAINT_BUFFER`（`RefCell<Option<PaintBuffer>>`）、`acquire_paint_buffer`（尺寸不变复用、变了重建）、
  `release_paint_buffer`（`EndPaint` 前放回）、`pub fn drop_paint_buffer()`（收起预览时释放）、
  `log_paint_gdi(tag, ms)`（debug 级打每帧 GDI/USER 与本帧耗时）；`paint_list` / `paint_doc_with`
  改为"取用缓存缓冲 + 归还"，不再每帧 `PaintBuffer::new` 新建整窗位图。
- `crates/qlr-app/src/app/show.rs`：`hide` 里调 `window::drop_paint_buffer()`，收起后不再占一块整窗位图。
- A/B 开关：环境变量 `QLRUST_NO_PAINT_REUSE=1` 可退回"每帧新建"（仅用于实测对比，不影响默认行为）。

### 为什么

两条绘制通道原本每帧 `CreateCompatibleDC` + `CreateCompatibleBitmap`（整窗大小，4K≈33 MB）再在
`Drop` 里销毁——是"每次重来"的最后一块。复用后每帧只做一次 `BitBlt`，省掉一次整窗位图与 DC 的
分配/销毁及随之的 GDI 对象 churn。

### 怎么验证的（数字）

- `cargo fmt --all -- --check` / `cargo clippy --all-targets -- -D warnings`：零输出。
- `cargo test`：**307 passed / 0 failed**；`scripts/verify-layers.ps1`：**12 PASS · 0 FAIL**；
  `scripts/smoke.ps1`：**16 PASS · 1 SKIP · 0 FAIL**（SKIP=管理员会话托盘 UIPI）。
- A/B 实测（`_bench/paint-measure.ps1`，`longlines.txt`、`--show` 后 `--scroll 3` ×10、`QLRUST_LOG=debug`）：
  - 复用：`PAINTBUF new` 只 **1 次**（12 帧共用同一块 880×748）；`PAINT doc` 平均 **2.54 ms**（min 2.24 / max 3.98）。
  - 每帧新建（`QLRUST_NO_PAINT_REUSE=1`）：`PAINT doc` 平均 **3.13 ms**（min 2.78 / max 3.90）。
  - → 每帧省 **~0.59 ms（≈19%）**；GDI 对象恒为 **25**（user 17），多帧无增长（无泄漏）。

### 下一步（未开工）

- 后端契约第一刀（`PreviewBackend` + 现有自研渲染接入，无 sidecar）。

## 2026-11-04 · §7 PreviewBackend 预览后端契约（第一刀）

### 做了什么

把「这个文件由谁来画」从 `show.rs` / `render.rs` / `shell.rs` 三处重复枚举 `Kind` 收口到一个入口：

- 新建 `crates/qlr-app/src/app/backend.rs`：`trait PreviewBackend`（`id/priority/can_handle/surface/natural_size`）
  + 四个零大小后端 `NativeBackend` / `ArchiveBackend` / `ShellBackend` / `PluginBackend`
  + `resolve(path, kind)` 取首个 `can_handle`。
- `show.rs`：`supported`、`natural`、`plugin`、`archive`、`renderable`、`shell`、`shell_wanted`、build_lines 的
  插件提示，全部改由 `backend::resolve(...)` 驱动；新增 debug 级 `BACKEND <id> surface=<..>` 日志。
- `render.rs`：worker 的 `native` 判定与两处 prewarm 的前置判断改走 `backend::resolve`。
- `shell.rs`：删除 `shell_candidate`，规则并入 `ShellBackend`。
- **压缩包按扩展名优先**：`resolve` 第一步 `if is_archive(path)` 直接返回 `ArchiveBackend`——否则一个内容被
  嗅探成文本的假 `.zip` 会退化成文本预览、不再走"解析失败"提示（这是改造中实测发现并修掉的回归）。
- `scripts/verify-layers.ps1`：新增 §2c「app 只在 backend.rs 里枚举 `viewer_for`」（13 PASS）。

### 为什么

"加格式 = 改平台"的根因不是 `Kind` 太多，而是**判定散落**：每加一个后端要在四五个文件各补分支。
本刀只抽"选择 / 表面 / 自然尺寸"三件，不碰渲染/窗口/音视频，保证行为等价、可增量演进。

### 怎么验证的（数字）

- `cargo fmt --all -- --check` / `cargo clippy --all-targets -- -D warnings`：零输出。
- `cargo test`：**312 passed / 0 failed**（307 + backend 5 个单测）。
- `scripts/verify-layers.ps1`：**13 PASS · 0 FAIL**（新增 §2c PATCH）。
- `scripts/smoke.ps1`：**16 检查全过 · 1 SKIP · 0 FAIL**（SKIP=管理员会话托盘 UIPI）；冷启 81 ms、RENDER 110 ms。
- 端到端：`verify-archive` **11 PASS**、`verify-media` **15 PASS**、`verify-office` **23 PASS**、
  `verify-mediashape` **13 PASS**、`verify-pdf` 静默（夹具缺失）。

### 下一步（未开工）

- 让 `repaint` 的绘制通道按 `Surface` 驱动（本刀只分类未重构）。
- `PreviewBackend` 之上接入"office 轻型 / html 改进"新后端；PDF 单客户 sidecar 后续评估。

## 2026-11-04 · §7 第二刀：repaint 按 Surface 驱动（平台层完善第 5 项）

### 做了什么

- `State` 新增 `surface: Option<backend::Surface>`；`show_preview` 在 `backend::resolve` 之后
  `with_state(|s| s.surface = backend.map(|b| b.surface(kind)))` 写入；`hide_preview` 收起时清 `None`。
- `chrome.rs::repaint` 顶层通道改为按 `s.surface` 路由：`List`→`paint_archive`、`ChildWindow`→
  系统处理器子窗口（只画顶栏）、其余走 `Canvas::Bitmap/Text` 内容通道；`List`/`ChildWindow` 仍配
  `has_archive_view()` / `shell_preview.is_some()` 运行时守卫（槽没就绪退回占位，不画空树/不空窗）。
- 改动文件：`crates/qlr-app/src/app/session.rs`、`app/show.rs`、`app/chrome.rs`、`app/boot.rs`（State 初始化 +1 行）。

### 为什么

"用什么方式承载"原本散在 repaint 里各自探测（`has_archive_view` / `shell_preview.is_some`），
与 `backend` 的判定重复。本刀把"表面"显式化进状态，让 repaint 的问句从"现在是什么状态"
变成"这个后端声明的表面是什么"，为后面接 office 轻型 / html 后端铺路（它们只改 backend.rs）。
`fit_bitmap`（SVG/视频等比、图片按窗口比例）/`plain_doc`（HTML/音频去行号）/`reserve_bar`（视频留进度条）
三处是**真正的逐格式呈现特例**，`Surface` 只分 Bitmap/Text 无法表达，故暂留在 repaint，不硬塞。

### 怎么验证的（数字）

- `cargo fmt --all -- --check` / `cargo clippy --all-targets -- -D warnings`：零输出。
- `cargo test`：**312 passed / 0 failed**（与上一刀一致，未增删测试）。
- `verify-layers`：**13 PASS · 0 FAIL**；`cargo build --release` 24.20s。
- 端到端全绿：`smoke` 全过（16 检查 · 1 SKIP=托盘 UIPI）、`verify-archive` 11、`verify-office` 23、
  `verify-media` 15、`verify-mediashape` 13 —— 覆盖了 List / ChildWindow / Bitmap / Text 四条通道。

### 下一步

- 在 `PreviewBackend` 之上接 office 轻型 / html 改进后端；PDF 单客户 sidecar 后续评估；之后转插件打磨。

## 2026-11-04 · window.rs 纯搬家拆成 window/ 子模块（平台层完善第 6 项）

### 做了什么

- `crates/qlr-platform/src/window.rs`（3660 行）纯搬家拆成 `crates/qlr-platform/src/window/`：
  `mod.rs`（窗口类/创建/显示/几何/输入/消息泵/子窗口）+ `shadow`/`text`/`settings`/`chrome`/`paint`/
  `scroll`/`audio`/`toc`/`bitmap` 九个子模块。每个子模块顶部只多一行 `use super::*;`，函数体一字未改；
  `mod.rs` 末尾 `pub use <mod>::*;` 再导出（`paint` 用 `pub(crate) use`，因其四个原语只 crate 内用）。
- 仅做可见性微调（跨模块用到的 `fill_solid/frame_solid/draw_line/measure_line`、`FONTS`/`FontSet::get`、
  `TextStyle::weight/color` 加 `pub(crate)`），无逻辑改动。
- `scripts/verify-layers.ps1` 新增 §3b「platform 单文件 ≤ 1600 行」（防止再长出一个 3600 行文件）。

### 为什么

平台层 `window.rs` 3660 行把类注册/绘制原语/文本测量/滚动条/子窗口全挤一处，违背"薄平台"。按架构评估
第 3.1 项拆分（纯搬家、行为等价）：拆分后最大文件 `window/mod.rs` 1381 行、`window/text.rs` 915 行。

### 怎么验证的（数字）

- `cargo fmt --all -- --check` / `cargo clippy --all-targets -- -D warnings`：零输出。
- `cargo test`：**312 passed / 0 failed**（与拆分前一致）。
- `verify-layers`：**14 PASS · 0 FAIL**（新增 §3b PASS）。
- `cargo build --release` 26.05s；`smoke` **16 检查全过 · 1 SKIP**（tray UIPI；冷启 88ms、RENDER 111ms）。

### 下一步

- `PreviewBackend` 之上接 office 轻型 / html 改进后端；PDF 单客户 sidecar 后续评估；之后转插件打磨。

## 2026-11-04 · 平台层收尾三件套（State.reset_for_hide / Canvas-Surface 对齐 / Markdown 排版快通道）

### 做了什么
- **State 隐藏清理收口**：`session.rs` 新增 `impl State { fn new(...); fn reset_for_hide(&mut self) }`，把 `hide_preview` 里逐字段清 13 个瞬态搬进 `reset_for_hide`；`boot.rs` 的 State 字面量改用 `State::new(...)`；补单测 `reset_for_hide_clears_transient_state`（13 字段逐个断言）。
- **Canvas/Surface 对齐**：`backend.rs` 加 `Surface::from_canvas(&Canvas)`；`session.rs::set_content` 用实际 Canvas 校准 `State.surface`（`set_content(None)` 的 ChildWindow/List 不受影响），保证 `surface` 与 `get_content` 永远一致、repaint 不走错通道。
- **Markdown 排版快通道**：`markdown.rs` 加 `is_plain`，非代码块里的纯文本行（行首无 `#>|-*+`/数字、任意处无 `` * _ ~ ` [ ] < \ ! ``）直接 `clip(line)` 出 `Normal`，跳过全部 block/inline 解析；`layout_capped` 增加 `fast: bool` 参数，`fastpath_on()` 读 `QLRUST_NO_MD_FASTPATH` 做 A/B。新增单测 `fast_path_matches_slow_path` 用 20 行混合语料断言快慢路径**逐字节一致**。

### 为什么
- 隐藏时零散清字段，字段越多越容易漏清（本刀前刚因加 `surface` 需同步改 hide）；集中一处 + 单测兜底。
- 插件可能返回与 Kind 预算不同的内容，`State.surface` 若只按 Kind 设置会和实际 `Canvas` 不符，repaint 走错通道。
- 8 MB Markdown 的 `layout` 是首屏瓶颈；纯文本行占文档多数，却每行都做多次 `Vec<char>` 分配。

### 怎么验证（实测数字）
- Markdown 8 MB 合成（297354 行）`cargo test --release -p qlr-render --lib bench_md_phases -- --ignored --nocapture`：**layout 全量 346.1 → 190.0 ms（−45%）**；`render_with(head,200000 行)` 241.3 → 146.1 ms；`render_with(head,400 行)` 15.1 → 15.4 ms（首屏基本不变）。fastpath 关闭（`QLRUST_NO_MD_FASTPATH=1`）即基线。
- `cargo test` **314 passed / 0 failed**；fmt/clippy 零输出；`verify-layers` **14 PASS**；`cargo build --release` 32.71s；`smoke` 16 PASS · 1 SKIP（tray UIPI）。

## 2026-11-04 · ADR-0010 + 后端能力契约（isolation/interactive）+ 租约判挂日志带后端 id

### 做了什么
- 新增 `docs/adr/0010-no-full-multiprocess-optional-isolated-backend.md`：决策"不做全面多进程"，采用可选进程外后端，首个且仅 pdfium 试点（试点另行批准）。
- `backend.rs`：`PreviewBackend` 增加 `isolation() -> Isolation{InProc|SystemProcess|NeedsSidecar}` 与 `interactive() -> bool`；四个后端分别声明（Native/Archive=InProc+interactive；Shell=SystemProcess+interactive；Plugin=InProc+非交互）；补单测 `isolation_and_interactive_contract`。
- `show.rs`：`BACKEND` 调试日志打印 `isolation`/`interactive`。
- `render.rs::describe_job`：判挂/自愈日志描述前缀 `[后端id]`，定位"卡在哪个后端"。

### 为什么
评审要"把单进程渲染器升级为能管理多后端的宿主骨架"。但全面多进程会吃掉冷启动/内存/包体护城河；Office 已由 prevhost 隔离、WebView2 自带多进程，唯一值得隔离的是进程内 pdfium。故先把"这个后端在哪个进程/要不要转发输入"做成**显式契约**（不改 ABI），进程外留作后端形态，实现与否另行批准。

### 怎么验证
`cargo test` **315 passed / 0 failed**（+1 isolation 单测）；fmt/clippy 零输出；`verify-layers` **14 PASS**；`cargo build --release` 24.72s；`smoke` 16 PASS·1 SKIP（tray UIPI）。

## 2026-11-04 · 后端会话四态状态机（进程内，ADR-0010 跟进）

### 做了什么
- `session.rs`：新增 `SessionState{Idle,Active,Reclaiming,Crashed}` + `label()`；`RenderTrack` 增 `state`/`backend` 字段与 `set_state()`（只在真变化时打 `SESSION backend= state=` debug 日志）。四态直接挂在现有线程级租约表上，不新建并行状态。
- `render.rs`：`begin_lease`→Active（带后端 id）、`end_lease`/`abort_lease`→Idle、`mark_hung`→Reclaiming、`heal_failed`→Crashed、`heal_succeeded`→Active/Idle。
- 单测 `session_state_transitions_and_labels`（四态迁移 + 同 Active 换后端 + label）。

### 为什么
ADR-0010 定的下一步：把线程级租约推广出"后端会话"维度、给出可诊断四态 + 日志。复用现有 `RenderTrack`/`Lease` 生命周期事件，不新增进程、不动热路径（关键点只加一次状态比较，命中才打日志）。

### 怎么验证
`cargo test` **316 passed / 0 failed**（+1 四态单测）；fmt/clippy 零输出；`verify-layers` **14 PASS**；`cargo build --release` 24.38s；`smoke` 16 PASS·1 SKIP（tray UIPI）。

## 2026-11-04 · 复测原版对比 + 暂缓 pdfium sidecar（平台层完善收尾）

### 做了什么
- **崩溃探测**：对当前**进程内** pdfium 跑 6 类变异（随机截断 / 字节翻转 / 窗口清零 / 坏 `startxref` / 坏 `/Length` / 头部垃圾）× 4 种子（1/3/8/200 页），每个文件单独子进程跑、按退出码判 AV。第一轮 164 文件、扩大轮 1004 文件，均 **0 次访问违例、0 非 0 退出**，能解出帧或优雅报错/拒绝 → **暂缓 pdfium sidecar**（ADR-0010 补「已评估并暂缓」，保留 `Isolation::NeedsSidecar` 预留 + `QLRUST_PDF_INPROC` 回退；四态状态机保留作后端级可诊断底座）。
- **文档审计**：`AGENTS.md` 基线更新（316 passed / 10 ignored、qlr-app 19 文件 ~10.1k 行、qlr-platform ~22 文件含 `window/`、verify-layers 14 PASS）；`ARCH.md` platform 条目补 `window/` 拆分与后端四态；`STRUCTURE-REVIEW-1.8.3.md` 顶部加「拆分前快照、后已落地」注；`README.md` 对比表按 2026-11-04 复测刷新；新增 `docs/dev/COMPARE-live-1.8.3-fix.md`。
- **原版对比复测**（同机 18 夹具、8 s 超时、IPC 触发）：QLRust 出窗 **17/18**、窗口出现均值 **58 ms**、首帧稳定 16/18 收敛均值 **241 ms**、预览内存均值 **37.5 MB**；原版 **14/18**、**190 ms**、仅 4/18 稳定、**722.4 MB**。冷启动中位 **31 vs 1085 ms**、就绪 **90 vs 1110 ms**、看 1280×720 图 **22.2 vs 163.1 MB**。

### 为什么
用户要求：暂缓 sidecar 并把项目文件（尤其 md）更新到位，再跑一遍原版对比。崩溃探测证明「没崩过」在 1004 例畸形输入下仍成立，sidecar 的成本不值得为此常驻；对比复测确认本轮平台层改动没有拖慢性能。

### 怎么验证
`cargo test` **316 passed / 0 failed**、fmt/clippy 零输出、`verify-layers` **14 PASS**、`smoke` 16 PASS·1 SKIP、`verify-office` 23 / `verify-media` 15 / `verify-mediashape` 13 / `verify-archive` 11 全绿。对比明细见 `docs/dev/COMPARE-live-1.8.3-fix.md`。

## 2026-11-04 · 文档解析安全加固（P0：ZIP 上限 / 去 Box::leak / 图片与 PDF 尺寸预算）

### 做了什么
- `crates/qlr-render/src/archive.rs`：中央目录 `cd_size` 来自文件内容，新增 `MAX_CD_BYTES = 64 MiB` 上限 + `cd_offset + cd_size` 越界校验；新增 `MAX_ENTRIES = 100_000` 条目上限、`MAX_NAME_LEN = 4096` 文件名上限。恶意/畸形 zip 声明近 4 GB 目录时优雅报错，不再 `vec![0u8; cd_size]` 直接 OOM。
- `archive.rs` 去掉 `prefix_owned`（`Box::leak`），目录前缀改拥有型 `BTreeSet<String>`，常驻进程连看多个压缩包不再线性泄漏。
- `crates/qlr-render/src/image_view.rs`：`render_via_image` 整图解码前按头部尺寸挡 `MAX_IMAGE_PIXELS = 100_000_000`（TIFF/GIF/BMP 等整图路径）。
- `crates/qlr-render/src/pdf.rs`：单页渲染加 `MAX_PAGE_PIXELS = 50_000_000` / `MAX_PAGE_EDGE = 20_000`，异常页尺寸/超大画布直接报错，不分配位图。

### 为什么
外部《深度代码评估》指出 ZIP 中央目录无上限、`Box::leak` 永久分配、图片/PDF 缺少解码预算。核实属实后按"不可信输入"处理修补；均属 `qlr-render`，不改 app/platform/插件，对正常文件行为零变化。

### 怎么验证
新增 `archive::tests::oversized_central_directory_is_rejected`（声明 `cd_size=0xFFFFFFFF` 的假 zip 必须报错）；`cargo test` **317 passed / 0 failed**；fmt/clippy 零输出；`verify-archive` **11 PASS**、`verify-pdf` PASS、`verify-mediashape` **13 PASS**、`smoke` **16 PASS · 1 SKIP**（tray UIPI）。

## 2026-11-04 · 插件加载信任边界（安全加固第二刀）

### 做了什么
- `crates/qlr-core/src/plugins.rs`：`PluginHost::scan` 加加载信任边界——① 只接受 `plugins\` 目录内的真实文件（`canonicalize` 后必须 `starts_with` 插件目录，符号链接指到目录外一律拒绝）；② 单个 DLL `MAX_DLL_BYTES = 64 MiB` 上限，超限跳过、不 `LoadLibrary`；③ `MAX_PLUGINS = 64` 数量上限；④ 插件文本输出 `MAX_TEXT_OUTPUT_BYTES = 64 MiB` 上限（超限先 `free` 再当没有文本）。被跳过/拒绝的原因都记进 `host.errors()`，不致命。
- 新增单测 `oversized_dll_is_skipped`（`set_len(MAX_DLL_BYTES+1)` 的 `huge.dll` 必须被拒，错误含“DLL 过大”）。

### 为什么
外部评估把“插件跑在主进程、返回指针无法证明有效”列为架构级风险。真正的隔离要进程外（v2，见 ADR-0010 预留的 `NeedsSidecar`）。但**当前进程内的加载边界可以先收紧**：别让超大 / 越界 / 非本目录的 DLL 进来，别让插件返回超大文本把宿主吃穿。成本极低，对正常插件零影响。

### 怎么验证
`cargo test` **318 passed / 0 failed**（+1 新测）；fmt/clippy 零输出；`verify-layers` **14 PASS**；`cargo build --release` 37.02s；`smoke` **16 PASS · 1 SKIP**。原有 7 个 plugins 单测（坏 dll 不致命 / 非 dll 忽略 / 空目录 / copy_out 校验等）保持绿。
