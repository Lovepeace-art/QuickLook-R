# 接手说明（HANDOFF）

> 目标：一个没参与过本项目的人，**10 分钟内**能构建、运行、验证、并知道下一步做什么。
>
> **当前状态（1.0.0，2026-10-31）**：内置图片/PDF/文本/Markdown 四类 + 插件体系（C ABI v1）+ 每用户安装器，M1–M6 已收口。
> 想做新格式 → 读 [PLUGINS.md](PLUGINS.md)（十分钟模板）；想发版 → 读 [RELEASE.md](RELEASE.md)；
> 想知道坑 → [CONVENTIONS.md](CONVENTIONS.md) 48 条；想知道做到哪了 → [STATUS.md](STATUS.md)。

## 1. 环境

| 项 | 值 |
|---|---|
| OS | Windows 10 22H2 build 19045 x64（**不是 Win11**，Win11 分支的圆角/backdrop 已写但本机只走降级路径） |
| 工具链 | rustc/cargo 1.99.0，target `x86_64-pc-windows-msvc`（`D:\Ruanjian\Rust\cargo\bin\`） |
| 编辑器 | 任意；建议 `rust-analyzer` + `cargo clippy` |
| 依赖 | 无系统级依赖。`windows` crate 走静态链接，不需要装 SDK（rustc 自己经 vswhere 找 MSVC linker） |

## 2. 构建与运行

```powershell
cd <仓库>
cargo build --release
.\target\release\qlrust.exe                    # 常驻：显示空窗
.\target\release\qlrust.exe .\a.jpg            # 第二实例：请求预览后退出
.\target\release\qlrust.exe --render .\a.pdf   # 无窗口渲染一帧（脚本/CI 用）
.\target\release\qlrust.exe --quit             # 退出
```

日志：`target\release\UserData\logs\app.log`（便携模式下才有该目录；测试时脚本会创建 `portable.lock`）。
想看 debug 级日志：设环境变量 `QLRUST_LOG=debug`，或把 `UserData\config.json` 的 `log_level` 改成 `"debug"`。

## 3. 验证

```powershell
cargo clippy --all-targets -- -D warnings
cargo test
pwsh -NoProfile -File scripts/smoke.ps1 -Exe target\release\qlrust.exe
```

冒烟输出示例（提权环境下托盘项是 SKIP，属正常）：

```
冷启动并进入壳层     PASS 56 ms
第二实例自行退出     PASS 40 ms
收到转交的请求       PASS
常驻实例处理了 SHOW  PASS
预览窗口可见         PASS 1190x748
托盘图标已创建       SKIP 本会话以管理员身份运行…
常驻实例按请求退出   PASS 37 ms
退出时移除了托盘图标 PASS
```

## 4. 调试手法（本项目已验证有效，直接抄）

### 4.1 进程"无声消失"

GUI 子系统没有 stderr，两步定位：

```powershell
# ① 看 Windows 事件日志：拿到异常码、错误模块、偏移
Get-WinEvent -FilterHashtable @{LogName='Application'; StartTime=(Get-Date).AddMinutes(-15)} -MaxEvents 60 |
  Where-Object { $_.Message -match 'qlrust' } | Select-Object -First 2 -ExpandProperty Message
```

- `0xc0000005` = 访问违例；`0xc000041d` = 用户回调里发生的致命异常（通常是同一个 bug 的伴生记录）。
- 错误模块 + 偏移量能区分"崩在 user32（我们的窗口过程）"还是"崩在我们自己的代码"。

```powershell
# ② 事件日志只给模块，不给我们的行号 → 用消息级打点缩小范围
#    app.rs 的 wndproc 开头有 log_debug!("wm=0x{msg:04X}")，日志里最后一条消息就是现场
```

本项目第一次崩溃就是靠这两步定位的：最后一条是 `wm=0x000F`(WM_PAINT) → 再在 `paint_text` 里逐步打点 → 发现是第 4 行（**空字符串**）的 `DrawTextW`。
完整分析见 `docs/dev/LOG.md` 2026-10-29 条目，与 `CONVENTIONS.md` 坑清单 #1。

### 4.2 自动化里启动进程不能挂住

`Start-Process` 会让子进程继承 stdio 句柄，在捕获输出的环境里调用方会一直等 → 卡死。
用 WMI：`Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{CommandLine='"…exe" 参数'}`。

### 4.3 想确认窗口真的存在

```powershell
# 冒烟脚本里已经封装好：按类名 QLRust.PreviewWindow 枚举 + IsWindowVisible
```

### 4.4 想知道"选中项到底读到了什么"

```powershell
# A. 钉死一个已知的资源管理器窗口（推荐，不受前台锁影响）
#    先从日志里确认 frame hwnd：scripts/probe-selection.ps1 会打印候选 frame
pwsh -NoProfile -File scripts\probe-selection.ps1     # 全自动：开 explorer /select → 找窗口 → 探测 → 判定

# B. 手动两条
D:\qlrust-bin\qlrust.exe --probe-selection-hwnd 1312812   # 十进制 HWND
D:\qlrust-bin\qlrust.exe --probe-selection                # 走 GetForegroundWindow()

# C. 一定要看 debug 日志（默认 info 会把 selection 的细节全吞掉）
& cmd /c "set QLRUST_LOG=debug&& D:\qlrust-bin\qlrust.exe --probe-selection-hwnd 1312812"
Get-Content D:\qlrust-bin\UserData\logs\app.log
```

日志里该看到四行：`枚举到 N 个 shell browser：[0x..]` → `GetSelection 命中 K 项` →
`选中项[0] = <路径>` → `PROBE-SELECTION source=tab count=1 first=<路径>`。

**注意 `D:\qlrust-bin` 那份是副本**：改完代码必须重新 `Copy-Item`（CONVENTIONS 坑 #10），
否则你测的是旧二进制，而症状会伪装成"某个 debug 行不生效"。

### 4.5 桌面（Progman/WorkerW）上的选中项怎么验

桌面**不在** `IShellWindows` 里，必须走 `SWC_DESKTOP`（CONVENTIONS 坑 #25）。
所以桌面场景要专门验，而且要能在自动化里"造出选中状态"：

```powershell
# 1) 让桌面露出来（否则前台是终端/浏览器）
(New-Object -ComObject Shell.Application).MinimizeAll()
Start-Sleep -Milliseconds 900

# 2) 点桌面空白拿焦点，再点第一列第一个图标，然后连按 Down 逐项移动
#    （跨进程 LVM_SETITEMSTATE 无效，见 CONVENTIONS 坑 #28；type-ahead 也不生效）
#    鼠标点 + keybd_event 0x28(Down) / 0x11+0x41(Ctrl+A)

# 3) 钉死 Progman 句柄探测
$prog = ([PI]::FindWindow("Progman", $null)).ToInt64()
& D:\qlrust-try\qlrust.exe --probe-selection-hwnd $prog
```

日志里该看到：`目标是桌面（Progman），走 SWC_DESKTOP` → `GetSelection 命中 N 项` →
`命中桌面 browser hwnd=0x.. flags=1 attempt=..` → `PROBE-SELECTION source=desktop count=.. first=..`。

两条常见困惑：
- `FAIL 期望 count=1 实际 count=0` —— 十成是**没有选中项**（点空白处会清空选区），不是代码坏。
- `FindWindowSW` 报 `Err(code=0x00000000)` —— 那是 S_OK + 空指针，Shell 还没把 desktop view 注册好，
  代码里已经重试（3 轮 × 2 种 flags × 15 ms），手工重跑一次通常就过。

### 4.6 想确认"图片真的渲染出来了"

渲染在 worker 线程上，**只有日志能证明它做了什么**（GUI 没有 stdout）：

```powershell
# 1) 起常驻
Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = '"target\release\qlrust.exe"' } | Out-Null
Start-Sleep -Milliseconds 900

# 2) 让它预览一张真实图片（第二实例会转交）
Invoke-CimMethod -ClassName Win32_Process -MethodName Create `
  -Arguments @{ CommandLine = '"target\release\qlrust.exe" "..\spike\fixtures\8mp_3840x2160.jpg"' } | Out-Null
Start-Sleep -Milliseconds 1500

# 3) 看日志里这两行
Get-Content target\release\UserData\logs\app.log | Select-String 'SHOW target=|RENDER'
```

期望看到 `SHOW target=… kind=image renderable=true` 紧接着
`RENDER gen=1 bitmap bytes=… elapsed=…ms`（本机 4K 约 53 ms）。

如果只有 `SHOW` 没有 `RENDER`：渲染线程没起来（日志会有"渲染线程启动失败"）；
如果 `RENDER … 失败`：错误会带上具体原因（解码失败 / 打不开图片）。

## 5. 已知坑（完整清单在 CONVENTIONS.md §2）

1. `DrawTextW` 空切片 → 0xC0000005（已修，规则：空行跳过）。
2. 持 `STATE` 锁时动窗口 → 死锁（规则：先写 `LINES` 放锁再动窗口）。
3. `Shell_NotifyIconW` 在提权环境返回 0x80070005（UIPI，非 bug）。
4. `windows` 0.62 的模块归属反直觉（`PCWSTR` 在 `windows::core`，`COPYDATASTRUCT` 在 `System::DataExchange`…）。
5. `HWND` 不是 `Send`，进全局状态要包 `Hwnd(isize)`。
6. **工作目录里的 exe 拿不到 Shell COM**（令牌完整性被降 → `0x80070005`），集成测试要复制到 `D:\` 跑。
7. **`Add-Type` 定义的类型不跨 `pwsh` 进程**，枚举窗口与读结果必须同一次调用。
8. **`FindWindowSW` 定位不了资源管理器窗口**，必须逐条枚举 `IShellWindows` + `GetAncestor(GA_ROOT)` 同根匹配。

## 6. 下一步做什么（按顺序）

1. **A2.9/A2.10 基准先跑起来**：4K / 48MP / 26 MB 文本三项写成 `scripts/bench.ps1`。
   现在只有 4K 的 53 ms 是实测；48MP 与文本没有数字就没法判断 A2.3 该优化到什么程度。
2. **A2.3 缩放解码**：48MP 当前会解出约 192 MB RGBA（朴素整图路径），这是最大内存风险。
3. **A2.6 PDF**：`pdfium-render` + 自备 `pdfium.dll`，只在 worker 里延迟加载，**绝不进主 exe**。
4. **A2.8 交互**：滚轮缩放、拖动平移、Esc 关闭；文本补可视区虚拟化。
5. **A1.6 兜底**：UIA `SelectionPattern`（200 ms 超时）与鼠标命中测试。
6. **人工确认 M1 Gate**（随时可做，不阻塞）：选中 jpg → 真按空格 → 现在应该看到**图片**而不只是占位文字。

详细任务拆分与工时在仓库上级的 `docs/route-a-wbs.md`。

## 7. 文档地图

| 想了解 | 看 |
|---|---|
| 模块怎么分层、进程/线程/锁模型、热键与选中项怎么跑 | `docs/dev/ARCH.md` |
| 做到哪了、下一步 | `docs/dev/STATUS.md` |
| 代码/日志/提交约定 + FFI 坑（12 条） | `docs/dev/CONVENTIONS.md` |
| 每天做了什么、为什么、验证数据 | `docs/dev/LOG.md` |
| 架构级决策与取舍（含热键 ADR-0005、选中项 ADR-0006） | `docs/adr/` |
| 整体计划、里程碑、风险、工时 | 仓库上级 `docs/route-a-plan.md`、`docs/route-a-wbs.md` |
| 为什么重写、性能对拍 | 仓库上级 `docs/bench-compare.md` |

### 4.7 托盘图标到底有没有加上 / 用户能不能看见

两步，都别靠眼睛：

```powershell
# 1) 日志里有没有坐标（说明 Shell 认了这次注册）
Select-String -Path D:\qlrust-try\UserData\logs\app.log -Pattern '托盘图标已添加'
#   托盘图标已添加 rect=(1683,1040)-(1707,1080) 尺寸=24x40
#   报不出坐标 → 被折叠进溢出区，或根本没注册上（提权 UIPI）

# 2) 截屏按颜色找图标（蓝色且 B-R>60）
Get-Process qlrust | Stop-Process -Force          # 先杀干净
# 用 SetCursorPos 在托盘区来回扫 5-8 遍（清僵尸图标）
# 再启动一个实例，截屏扫 y=1040..1080, x=1300..1900 的蓝色像素
```

判据：**新图标位置应与日志里的 rect 吻合**。不吻合先怀疑僵尸图标，别怀疑 API
（我们误判过一次，见 CONVENTIONS #21）。

注意 `Shell_NotifyIconW` 在**提权会话里必然失败**（UIPI，0x80070005），
所以本仓库的 `smoke.ps1` 在管理员会话下把托盘检查标 SKIP 而不是 FAIL。

### 4.8 改版本号要同步三处

1. 根 `Cargo.toml` 的 `[workspace.package] version`
2. `crates/qlr-app/resources/qlrust.rc` 里的 `FILEVERSION` / `PRODUCTVERSION` /
   `VALUE "FileVersion"` / `VALUE "ProductVersion"`（**手工改，不会自动跟**）
3. `dist/qlrust-<版本>-win64/README.txt`

漏改 .rc 的后果：文件属性里显示旧版本号，排查现场时会看错版本。

## 4.9 托盘加不上 / 选区读不到？先看完整性级别

一条命令定性：
```powershell
icacls qlrust.exe | findstr Mandatory
# 有输出（Low Mandatory Level）→ exe 被降权了，见 CONVENTIONS 坑 #29
# 无输出 → 令牌正常，继续查别的
```
或者直接看启动日志（`UserData\logs\app.log` 头几行）：
```
令牌完整性=medium   ← 正常
令牌完整性=low（低于 medium：…）  ← 被降权：托盘与 Shell 选区都会被 UIPI 拒成 0x80070005
```
修法只有一条：**把 exe 复制到工作目录之外**再运行（`D:\qlrust-bin\`、`D:\qlrust-try\`、桌面）。
所有涉及托盘 / Shell COM 的自动化也必须这样跑，否则测的是"降权后的行为"。

---

## 接力说明（1.7.4 已完成，2026-11-01 16:30）

**③ 与 ⑤ 都已落地并端到端验证**（细节见 `LOG.md` 1.7.4、`STATUS.md §36`、`CONVENTIONS.md #131–#133`）：

- **③ PPT 最后一页**：翻页由我们接管（数 `ppt/slides/slide*.xml` 的页数），到头**停住**并在顶栏
  给「已是最后一页」，PowerPoint 再也不会切到黑屏"放映结束"。滚轮与 PageUp/PageDown 都走这条路。
- **③ 顶栏文件名错乱**：不是取值问题，是**时序**——`last_path` 必须在 `window::show_at` **之前**记好
  （`DoPreview()` 的跨进程 COM 会泵消息，`WM_PAINT` 会按旧标题画一帧且没有后续 invalidate）。
  顺带修了非 PDF 长文件名被页码框挤掉（页码框只在 `chrome.page.is_some()` 时让位）。
- **⑤ 拖边改大小**：处理器子窗口会吃掉边缘鼠标 → `DoPreview` 后 400ms `EnableWindow(宿主, FALSE)`
  接管输入；另补 `WS_CLIPCHILDREN`（否则填底色擦掉子窗口像素＝"翻页后画面不动"）、
  热区按 DPI 缩放、`finish_resize` 用 `content_size_of()`（内容高 = 客户区 − 30px）。
- 端到端脚本：`pwsh -NoProfile -File scripts/verify-office.ps1` → **15 项断言** + 截图（`_bench\office\`）。

**怎么自己复核一遍**（三条脚本，全绿才算没退化）：

```powershell
$env:PATH="D:\Ruanjian\Rust\cargo\bin;$env:PATH"
cargo test; cargo fmt --check; cargo clippy --all-targets -- -D warnings
pwsh -NoProfile -File scripts/smoke.ps1            # 16 PASS + 1 SKIP（托盘，管理员会话 UIPI）
pwsh -NoProfile -File scripts/verify-archive.ps1   # 11 PASS
pwsh -NoProfile -File scripts/verify-office.ps1    # 15 PASS（需本机装 Office + 夹具）
```

**纪律（仍然有效）**：

- 改 `crates/qlr-app/src/app.rs` **一律用 `edit` 工具**（唯一锚点），不要用 PowerShell 按行号删改
  ——上一个会话就是这么把注释写成字面量 `\n` 的。
- 中文内容的 PowerShell 脚本先写成 `.ps1` 文件再 `pwsh -NoProfile -File` 跑（CONVENTIONS #127）。
- `git commit` 的中文正文写进消息文件用 `-F`（直接塞 `-m` 会被 PowerShell 拆坏）。
- 实测/截图前先 `Normalize-Position`（CONVENTIONS #129）；预览窗类名 `QLRust.PreviewWindow`，
  exe 必须放在 DSH 工作目录**之外**（否则被打 Low 完整性，Shell COM/托盘被 UIPI 拒，#29）。

**接下来的顺序**：

1. 列表视图打磨：双击/回车用系统程序打开条目、拖滚动条、按名字过滤；
2. Office 预览的键盘只有 PageUp/PageDown + 滚轮，`Home/End`、方向键还没接；
3. `toast` 通道目前只服务 PPT 边界提示，其他一次性提示（如"已复制路径"）还没并进来。

**已完成且验证过的**（不用重做）：1.4.10 拖动/投影、1.4.11 顶栏发灰、1.5.0–1.5.2 删亚克力+双缓冲、
1.7.0 插本文本通道 + `qlr-render/archive.rs`、1.7.1 行号贴左边框、1.7.3 压缩包列表视图、
**1.7.4 Office 预览三件修复（本次）**。

## 接力说明（1.7.5 补丁，2026-11-01 17:05）

用户报"预览窗口拖动会这样"（zip 列表右侧一大片竖条纹）。查下来不是绘制问题，是
**换文件时没卸系统预览处理器**：`open_shell_preview()` 只在"这次也走处理器"时收尸，
从 docx/pptx/xlsx 切到 zip/图片/文本时，Office 的子窗口留在预览窗里继续盖着内容
（顶栏是新的、正文是旧的；拖动时那片残留由别的进程按旧尺寸重绘 → 竖条纹）。
修法：`app.rs` 的 `drop_stale_shell_preview()`，两个调用点（压缩包分支 + 主路径 `!shell_wanted`）。
验证：`verify-office.ps1` 扩到 **21 项全 PASS**（切换前子窗口 4 个 → 切换后 0 个）。

**如果还能复现"拖动出条纹"**：先看 `EnumChildWindows(预览窗)` 是不是 0；不是 0 就是还有
别的路径没走到收尸（比如从文本切过来）。CONVENTIONS #134 有自查手法。

## 接力说明（1.7.6，2026-11-01 17:25）

用户提了两条手感问题，都已修并量过数字：

1. **"点别的文件，预览换得慢"**：原先"线程睡 300 ms 再查 + UI 400 ms 计时器取结果"最坏要等
   约 700 ms（实测 419 ms 平均）。现在线程窗口开着时 **90 ms** 轮一次、一发现就
   `PostMessage(WM_QLR_FOLLOW)` 叫醒 UI；`IShellWindows` 与"上次命中的 browser"都缓存。
   **实测 41 ms 平均 / 72 ms 最差**。改的是 `app.rs`（跟随线程 + `on_follow_wake`）、
   `selection.rs`（缓存）、`ipc.rs`（新消息）。
2. **"Office 切换偶尔跳回占位页"**：`DoPreview` 那 130–260 ms 里旧处理器已卸、新的没画，
   整窗重画就露出文字占位页。现在换文件时只重画顶栏（`window::paint_chrome_strip`），
   内容区像素冻住，旧处理器还被 `raise()` 到最上层盖 260 ms（`TIMER_SHELL_SWAP`）。
   实测抓帧：137 ms 空白 Word 窗（Office 自己的首帧）、199 ms 正文；我们的占位页不再出现。

**还没解决的**：Office → Office 切换仍剩约 60 ms 白底 —— Office 开始第二次 `DoPreview` 时会把
旧处理器的窗口拆掉，`shell_cover` 盖不住。下一步可试"新宿主窗口先隐藏、画好再显示"，
但要先确认隐藏窗口下 `IPreviewHandler` 会不会照常绘制（拿 `_bench/switch-frames.ps1` 抓帧验）。

量延迟的脚本：`..\_bench\follow-latency.ps1`（对照旧二进制用 `follow-latency-old.ps1`，
它指向 `Desktop\qlrust-try-old`）；抓切换帧：`..\_bench\switch-frames.ps1 -Tag xxx`。

## 接力说明（1.7.7，2026-11-01 18:10）

用户报"Excel 还有那个占位页，滚动栏都不可用"。根因不是绘制：Excel 的预览是 OLE 就地激活
对象，`DoPreview()` 之后我们紧接着调 `SetWindow`（`sync_shell_rect`），**自等自死锁** ——
进程 `Responding=False`、日志停在 takeover、屏幕上只剩占位页。修法与验证见 `LOG.md` 1.7.7 /
`STATUS.md §39` / `CONVENTIONS.md #137/#138`。

**这一轮改了**（`app.rs` / `shellpreview.rs` / `hotkey.rs` / `window.rs`）：
1. 子窗口矩形在 `open()` 前算好（`shell_content_rect`），打开后不再进 COM；
2. 非 PPT 的 Office 预览**内缩 10 DIP**，四条边留给拖边改大小，内容交互归处理器；
3. 就绪判断改成轮询 `visible_area ≥ 50%`（120 ms 一次，最多 3 s）；
4. Office 预览允许激活（滚动条可拖）、滚轮对 Word/Excel 放行。

**没解决的**：Excel 的滚轮仍然不动（试过钩子放行、`PostMessage(WM_MOUSEWHEEL)`、
`WM_VSCROLL`、`VK_NEXT`；而资源管理器预览窗格里同一文件能滚）。下一步候选：
把焦点交给 Excel 的子窗口，或读 UIA 的滚动条 range 后自己 `SetValue`。
探针脚本都在 `_bench\test-act-wheel.ps1`、`test-post-wheel.ps1`、`test-vscroll.ps1`、
`test-excel-interaction.ps1`，对照用的资源管理器预览格截图在 `_bench\drag\pane-*.png`。

## 接力说明（1.7.9，2026-11-01 22:10）

**这一轮交付了什么**：内置预览从 4 类扩到 **8 类**（图片 / PDF / 文本 / Markdown /
**SVG / HTML / 音频 / 视频**），图片改成**等比居中**；同时修掉三处"点一下就卡死/没反应"。
细节见 `LOG.md` 1.7.8 / 1.7.9、`STATUS.md §40`、`CONVENTIONS.md #139–#144`。

**上手顺序（用户定的顺序，别改）**：

1. 先跑一遍回归，确认基线没被破坏：
   ```powershell
   $env:PATH="D:\Ruanjian\Rust\cargo\bin;$env:PATH"
   cargo test; cargo fmt --all -- --check; cargo clippy --all-targets -- -D warnings
   pwsh -NoProfile -File scripts\smoke.ps1
   pwsh -NoProfile -File scripts\verify-office.ps1     # 期望 22 PASS
   pwsh -NoProfile -File scripts\verify-archive.ps1    # 期望 11 PASS
   pwsh -NoProfile -File scripts\verify-media.ps1      # 期望 15 PASS
   ```
2. 剩下的用户需求都已完成；下一步是**发布准备**（用户明确说上 GitHub 是最后一步）：
   - `dist/` 被 git 跟踪 **354 文件 / 678 MB**（46 份 7.24 MB 的 pdfium.dll、52 个版本目录）；
   - 根目录**没有 LICENSE**（`Cargo.toml` 已写 `GPL-3.0-or-later`）；
   - 15 个文件写死 `<用户目录>\…`（`scripts/stress.ps1:12`、
     `scripts/verify-office.ps1:20,26`、`scripts/bench-results.json`、`docs/dev/LOG.md` 等）；
   - 版本漂移：`README.md:5` 写 1.4.2、`crates/qlr-app/resources/qlrust.rc:12`、`Cargo.toml`；
   - 无 `.gitattributes`（45 mixed / 52 crlf / 93 lf）。
3. 可选打磨：视频**出声**（`IMFMediaSession` + EVR 或 WASAPI 混流，MCI 在本机播不了 mp4，
   见 #144）；列表视图双击/回车打开条目、拖滚动条、按名字过滤。

**踩坑提醒（这一轮新踩的，别再踩）**：

- `SetFocus` 千万别在持锁时调（#139）。判据：点一下 Office 预览就 `Application Hang`，
  日志最后一行 `wm=0x0006`。
- `WM_MOUSEACTIVATE` 要返回 **1**，不是 2（#140）——2 会把这次点击吃掉。
- `IMediaSourceReader::SetCurrentPosition` 的 guid 传 `GUID::zeroed()`，别传 NULL（#141）。
- 便携实测 exe 必须放在 DSH 工作目录**之外**（`<便携测试目录>`），
  复制新 exe 前先 `Get-Process qlrust | Stop-Process -Force` 并等 ~800 ms。
- 截图一律用 `..\_bench\shot3.ps1`（PrintWindow PW_RENDERFULLCONTENT）；
  窗口有一半在屏幕外会抓到"新画一半 + 旧一半"的混合帧（#129）。

---

## 接力说明（1.8.2，2026-11-01 23:40）

**1.8.2 这一轮**（用户一句话，见 CONVENTIONS **#150**、LOG "1.8.2"、STATUS §43）：
「把图片和视频的窗口伸缩功能去掉，滚轮对窗口进行缩放就足够了。」

- `resizable_kind()`（`Image | Video` → false）+ `resize_margin()` 返回 0 + `edges_at()` 对
  `margin <= 0` 短路 → **图片/视频拖边不再改大小，光标也不变双向箭头**。
- 窗口大小只在打开时按素材比例定一次；`window_aspect_differs()` 让"最大化这类外部改大小"
  退回等比居中，不把图拉变形。
- 版本 1.8.1 → **1.8.2**。

**1.8.1 那一轮**（用户三条反馈，CONVENTIONS **#147 / #148 / #149**、LOG "1.8.1"、STATUS §42）：

1. **Excel 预览里左键能点选单元格**。真因不是鼠标被吞，而是焦点只到 Excel 的容器层
   `EXCEL9`，画表格的 `EXCEL7` 从未拿到 UI 激活（`GetGUIThreadInfo` 实锤；往 `EXCEL7`
   投合成 `WM_LBUTTONDOWN` 无效）。修法：`ShellPreview::set_com_focus()` 调
   `IPreviewHandler::SetFocus()`，并且**只能在退栈后调**——`WM_ACTIVATE`/`WM_SETFOCUS` 里会被
   `0x8001010D` 拒，`WM_MOUSEACTIVATE` 里调会把这一次点击吃掉；最终放在
   `TIMER_SHELL_INPUT`（`visible_area >= 50`）里调一次，`WM_MOUSEACTIVATE` 只返回 `MA_ACTIVATE`。
   同时 `take_over_input()`（`EnableWindow(FALSE)`）**只给 PPT**，Excel/Word 不再被禁输入。
2. **图片/视频按素材比例开窗**：`State.media_aspect` + `media_window_size`；`repaint` 里只有
   `Svg | Video` 走 `paint_bitmap_fit`，图片铺满。视频扣 `window::AUDIO_BAR_H = 52`。
3. **交给系统处理器的类型不再画占位页**（`shell_candidate` 命中 → `lines.clear()`）。

**接手先跑回归**（期望数字已更新）：

```powershell
$env:PATH="D:\Ruanjian\Rust\cargo\bin;$env:PATH"
cargo test                                          # 期望 251 passed
cargo fmt --all -- --check; cargo clippy --all-targets -- -D warnings
pwsh -NoProfile -File scripts\smoke.ps1             # 全过（托盘 SKIP=管理员会话 UIPI）
pwsh -NoProfile -File scripts\verify-office.ps1      # 期望 23 PASS
pwsh -NoProfile -File scripts\verify-archive.ps1     # 期望 11 PASS
pwsh -NoProfile -File scripts\verify-media.ps1       # 期望 15 PASS · 0 SKIP
pwsh -NoProfile -File scripts\verify-mediashape.ps1  # 期望 13 PASS · 0 SKIP（1.8.1 新增）
```

`verify-mediashape.ps1` 需要 `..\..\qlrust-dev\compare\` 里的 `big-sheet.xlsx` /
`img-photo.jpg` / `video-demo.mp4`（缺哪个就把相关检查 SKIP）；`.xlsx` 走系统处理器，
所以脚本必须把 `target\release\qlr_plugin_*.dll` 铺进便携目录的 `plugins\`，否则会
`SHOW 拒绝 kind=unsupported`（跟 `verify-office.ps1` 同款处理）。

**下一步**：用户需求都已完成，只剩**发布准备**（用户说上 GitHub 是最后一步）——
`dist/` 354 文件 / 678 MB 被跟踪、根目录缺 LICENSE、15 处 `<用户目录>\…` 绝对路径、
README 1.4.2 / rc / Cargo 版本漂移、无 `.gitattributes`。

**可选打磨**：视频出声（MCI 播不了 mp4，需 `IMFMediaSession` + EVR 或 WASAPI，见 #144）；
列表视图双击/回车打开条目、拖滚动条、按名字过滤。

**新踩的坑**：

- `IPreviewHandler::SetFocus` / `SetRect` 这类 COM 调用：**不能在输入同步呼叫里**（`0x8001010D`），
  也**不能持锁**（#139/#145），更不能放在 `WM_MOUSEACTIVATE`（会吃掉当前点击）。
- 给媒体锁比例时，内容高度别忘底部进度条（视频 52px），否则比例算出来偏 0.05+。
- `smoke.ps1` 的 `Get-Content -Raw` 偶发返回数组 → `Check` 布尔参数报错，已改成 `-join "`n"`。
- 用户说「暂停」时记得 `update_goal` 暂停目标（用户 2026-11-01 明确要求）。

---

## 发布准备记录（1.8.2 之后，2026-11-02）

用户定了三件事：**历史压成一个初始提交**、**`docs/dev` 全部保留只洗路径**、**仓库名 `QuickLook-R`**。

已做完的（都在这次提交里）：

| 项 | 结果 |
|---|---|
| `dist/` 不再跟踪 | `.gitignore` 加 `/dist/`、`*.zip`、`_bench_tmp/`；历史上跟踪过的 354 个文件随"压成一个初始提交"一起消失 |
| LICENSE | 根目录 `LICENSE` = GPL-3.0 全文（674 行），与 `Cargo.toml` 的 `license = "GPL-3.0-or-later"` 一致 |
| 来源声明 | 新 `NOTICE.md`：独立重写、与上游 QuickLook 的关系、第三方组件（PDFium BSD-3 运行时动态加载 / windows crate MIT OR Apache-2.0） |
| 行尾与二进制 | 新 `.gitattributes`：`* text=auto`，源码/文档 LF，`.ps1`/`.rc` CRLF，位图与 dll/zip/pdf 标 binary |
| 绝对路径 | 22 处文档 + 3 个脚本 + 2 处单测夹具回退，共 27 处；`crates/*` 里改成 `env!("CARGO_MANIFEST_DIR")` 推算 |
| 脚本夹具参数 | `verify-office.ps1` / `stress.ps1` / `verify-pdf.ps1` 的默认值改成「显式参数 > `$env:QLRUST_FIXTURES` > `<桌面>\qlrust-dev`」，截图默认落 `%TEMP%` |
| AGENTS.md | 重写成给 AI 看的完整协作手册（14 节：仓库地图 / crate 索引 / 分层决策 / 类型矩阵 / 硬约定 / 改码规则 / 日志 / 构建测试 / 提交纪律 / 版本发布 / 文档义务 / 禁区 / 坑速查 / 协作） |
| README | 版本 1.8.2、11 类内置查看器、性能对比表（引 `docs/dev/COMPARE-live-1.8.2.md`）、验证脚本清单、已知限制（字体缺失 / 视频无声 / 图片视频不可拖边） |
| ARCH 漂移 | `acrylic.rs`（1.5.0 已删）相关段落改成「只剩 Win11 圆角 + backdrop，Win10 纯色」；模块清单按当前 13 + 16 个文件重写 |
| STATUS 漂移 | 里程碑表 M3/M4 从"不做了改插件"改成实际状态（压缩包/源码已内置、字体仍缺、Office + 视频已完成） |
| 版本对齐 | `Cargo.toml` / `crates/qlr-app/resources/qlrust.rc` = 1.8.2；README 同步 |

**发布步骤**（本机 `gh` 已登录，token 含 `repo` / `workflow`）：

```powershell
cd <仓库>
git checkout --orphan publish-1.8.2   # 从当前工作树造一个没有父提交的分支
git add -A                            # .gitignore 会挡掉 dist/ 与 target/
git commit -F $env:TEMP\commit-msg.txt
gh repo create QuickLook-R --public --source . --push --remote origin
```

之后日常开发就在这条新历史上继续；老的 `master`（含 200+ 条提交与 dist）留在本地当备份。

**还没做**（发布后可选）：字体预览（ttf/otf，`qlr-render` 加 DirectWrite 通道）、视频出声
（`IMFMediaSession` + EVR 或 WASAPI）、GitHub Release 里挂 `scripts\pack.ps1` 打出来的 zip、
CI 里加 `verify-*.ps1`（需要 Windows runner + Office）。



## 1.8.3 接力说明（2026-11-02）

- **分支**：`release/1.8.3`（5 个提交：基线脚本 → 图片 100%+迁移 → DIP 常量 → 字体/自然尺寸/预热 → 热键三卡）。
- **回归命令**（顺序照抄）：
  ```powershell
  cargo fmt --all -- --check
  cargo clippy --all-targets -- -D warnings
  cargo test                      # 期望 264 passed / 3 ignored
  cargo build --release
  pwsh -File scripts\baseline-shot.ps1 -OutDir baseline\after
  pwsh -File scripts\baseline-diff.ps1 -A baseline\before -B baseline\after   # 96 DPI 下必须 PASS
  pwsh -File scripts\smoke.ps1    # 17 PASS / 1 SKIP（管理员会话 UIPI）
  ```
- **`baseline\before` 是 1.8.3 的视觉基准**（图片已按 100% 缩放截取）。改任何绘制/尺寸代码后
  必须重跑 `baseline-diff`，否则"没改坏"无法证明。基准图不入库（`.gitignore` 里有 `/baseline/`）。
- **待验证**：200% 真机视觉（用户手上那台 Win11 200%）。`scripts\collect-diag.ps1` 一键收集
  日志 + DPI + 7 条口述答案。重点看 `passed_text_focus` 与 `text_focus_level` 两个统计：
  前者 > 0 说明 T02 的判据真的生效了，后者 1/2 说明 Win11 走的是 caret 还是类名兜底。
- **已知限制**：多屏且各屏缩放不同时，"开窗尺寸"用的是**系统 DPI**（窗口还没建，没有 hwnd 可查），
  只有建出来之后的绘制才是逐窗口精确的。单屏不受影响。
- **下一步（未开工）**：P0 度量（`bench.ps1` 补 PeakPrivate / GDI 对象 / 帧级 alloc + CI 门禁脚本），
  然后才是 T05–T11 里真正剩下的项（PDF 每次预览开 3 次文档、`video_tick` 锁内解码、`PageKey` 无 mtime）。

## 1.8.3-fix 接力说明（2026-11-03）

- **分支**：`fix/1.8.3-batch1`（基于 `71999be`，7 个提交，**快进关系**，不用 merge commit）。
- **这批是什么**：评审报告里的"批次一"6 项小修（嗅探有界读 / 热键 `GetFocus` 死代码 + 钩子句柄 /
  压缩包 `Box::leak` / `PageCache` 文件戳 / 相邻页预取复用文件戳 / `pack.ps1` 校验和顺序）。
  细节与数字见 `docs/dev/LOG.md` 的 2026-11-03 条目、`STATUS.md` §45。
- **评审里明确没做的**：`ShellPreview::close()` 的 `mem::forget` 方案——那个方法零调用点，
  而 `forget` 会连 `IPreviewHandler` 的 COM 引用一起跳过释放。要收拾就单独提一个提交，
  写成 `close(self) { drop(self) }` 或直接删掉死方法。
- **回归命令**（顺序照抄；数字是 1.8.3-fix 之后的口径）：
  ```powershell
  cargo fmt --all -- --check                  # 零输出
  cargo clippy --all-targets -- -D warnings   # 零输出
  cargo test                                  # 期望 287 passed / 4 ignored
  cargo build --release
  pwsh -File scripts\smoke.ps1                # 16 PASS / 1 SKIP（管理员会话 UIPI）——合并后已跑
  pwsh -File scripts\verify-archive.ps1       # 11 PASS · 0 FAIL（压缩包前缀改动在这里，已跑）
  pwsh -File scripts\verify-pdf.ps1 -Pdf target\bench-fixtures\pdf_8page.pdf   # PASS（页缓存改动，已跑）
  pwsh -File scripts\verify-office.ps1        # 23 PASS（本批未碰 Office；下次动 IPreviewHandler 再跑）
  ```
- **必须人工确认的两条**（自动化测不出来）：
  1. 嗅探：把几 GB 的 `.iso`/`.vhdx` 改名成 `.xyz` 后按空格——应提示"不支持"，任务管理器内存不飙升。
  2. 热键（Win11）：桌面选中文件按 `F2` 进入就地重命名，再按空格——应能打出空格；
     看 `app.log` 里 `passed_text_focus=` 与 `text_focus_level=`（**小写**，2 = 走类名兜底那条新生效的判据）。
- **下一步（评审的批次二）**：JPEG 解码入 `source_cache` + PDF/视频缩放统一 `downscale_exact`
  （同一处代码，一个提交）→ PDF 文档单槽缓存 → md/html 排版早停。这几项才是用户体感的那部分
  （4K JPEG 每档重解码、30MB PDF 每页重读全文件），且都在渲染热路径上，**必须每步带基线**。
  ——**已完成，见下一节「1.8.3-fix 批次二接力说明」。**
- **文档口径提醒**：本文件上面 1.8.3 节里写的 "期望 264 passed" 是发布当时的数字，实测基线已是 286，
  以本节为准。


## 1.8.3-fix 批次二接力说明（2026-11-03）

- **分支**：还是 `fix/1.8.3-batch1`（批次一 + 批次二都在上面，共 15 个提交，`71999be` 快进）。
  批次二 7 个提交：`4352c0f` JPEG 档位缓存 → `a2a6439` PDF 单槽缓存 → `d38506a` Markdown 早停 →
  `c0af9f5` HTML 单遍剥离 → `f76e1ec` 缩放内循环 → `58c53d2` 帧级分配计数 → `57fdcb3` bench 度量。
- **这批是什么**：评审报告 🔴 分组里"每次重来都付一遍"的开销。数字与口径见 `LOG.md` 2026-11-03
  批次二条目、`STATUS.md` §46。**每项都是先加 `#[ignore]` 基准、拿到基线、再改、再量**，
  没有一项是"看起来更快"。
- **回归命令**（数字是批次二之后的口径）：
  ```powershell
  cargo fmt --all -- --check                  # 零输出
  cargo clippy --all-targets -- -D warnings   # 零输出
  cargo test                                  # 期望 290 passed / 9 ignored（9 = 5 个基准 + 原有）
  cargo build --release
  pwsh -File scripts\smoke.ps1                # 16 PASS / 1 SKIP（管理员会话 UIPI）
  pwsh -File scripts\verify-archive.ps1       # 11 PASS · 0 FAIL
  pwsh -File scripts\verify-pdf.ps1 -Pdf target\bench-fixtures\pdf_8page.pdf   # PASS
  pwsh -File scripts\bench.ps1 -Passes 2      # 需要交互式桌面；看 GdiAfterHide 有没有涨
  ```
  基准（都要夹具，`$env:QLRUST_FIXTURES` 指 `spike\fixtures`，`$env:QLR_BENCH_PDF` 指 PDF，
  `$env:QLRUST_PDFIUM` 指 `target\release\pdfium.dll`）：
  ```powershell
  cargo test --release -p qlr-render --lib bench_jpeg_zoom_steps -- --ignored --nocapture
  cargo test --release -p qlr-render --lib bench_downscale      -- --ignored --nocapture
  cargo test --release -p qlr-render --lib bench_pdf_costs      -- --ignored --nocapture
  cargo test --release -p qlr-render --lib bench_md_phases      -- --ignored --nocapture
  cargo test --release -p qlr-render --lib bench_html_phases    -- --ignored --nocapture
  ```
- **`alloc-stats` 特性怎么用**（默认不编，不影响发版体积）：
  `cargo build --release --features alloc-stats`，然后看 `app.log` 里的
  `RENDERALLOC gen=N bytes=… live=…`（`bytes` = 这一帧总共分配，`live` = 当前存活）。
  冷帧 65.7 MB / 热帧 5.6 MB / live 19.2 MB 是 4K 图的参考值。
- **两个故意的取舍**（下次不要"顺手改回去"）：
  1. HTML 不早停——状态行的"全文共 N 行"是排版后的显示行数，早停就算不出来。
     想省这几十毫秒得先改成流式词法（tokenize 现在整篇 33 ms）。
  2. `RENDER` 行不加字段——`bench.ps1` 正则在依赖它；帧级分配另起 `RENDERALLOC` 行。
- **必须人工确认**（自动化测不出来）：200% 缩放下滚轮缩放 4K JPEG、来回翻 PDF 页、
  打开 8 MB 的 md/html 看首屏——视觉应与上一版完全一致（本批没动几何/DPI）。
- **下一步（建议顺序）**：
  1. STRUCTURE-REVIEW §7-1「持锁动窗口」（`hide_preview`/`video_paint` 改成锁内取句柄→放锁→动窗口），
     P0，和 `UISTALL 2721ms` 那条现场日志直接相关；
  2. §6-9 内存总预算闸门（pagecache 64 MiB + 帧缓存 + 预热统一 Budget；现在全是加法，
     最坏能到近 500 MB）；
  3. HTML 流式词法（tokenize 33 ms 是 md/html 里最后一块大数）；
  4. §6-10 `verify-layers.ps1` 分层断言（最便宜，能防 `qlr-render` 反向依赖 `qlr-app`）。

## 1.8.3-fix 批次三接力说明（2026-11-03）

**状态**：分支 `fix/1.8.3-batch1` 上共有 **21 个提交**（批次一 8 + 批次二 7 + 批次三 4 + 文档 2），
`main` 仍是 `71999be`，未推送。批次三的四个提交：

| 提交 | 内容 |
|---|---|
| `8dd38dc` | 视频帧改 `Arc` 共享、`video_paint` 贴图移出 `STATE` 锁；`with_state` 锁中毒不再静默哑火 |
| `3a0d720` | 新增 `scripts/verify-layers.ps1`（11 条分层断言）+ `ARCH.md` 按代码事实重写 |
| `915ef49` | 三处缓存共用 256 MB 总预算（`crates/qlr-render/src/budget.rs`），日志多一行 `MEM` |
| `eca8801` | `html.rs` 词法改流式（不再先攒 96 万个 `Vec<Tok>`）+ 文本按非空白段整段拷入 |

**回归结果（release，本机交互式会话）**：

```
cargo fmt --all -- --check                        # 零输出
cargo clippy --all-targets -- -D warnings         # 零输出（--features alloc-stats 也零）
cargo test                                        # 299 passed / 10 ignored / 0 failed
pwsh -NoProfile -File scripts\smoke.ps1 -Exe target\release\qlrust.exe            # 16 PASS · 1 SKIP · 0 FAIL
pwsh -NoProfile -File scripts\verify-layers.ps1                                   # 11 PASS · 0 FAIL
pwsh -NoProfile -File scripts\verify-media.ps1 -Video <720p mp4>                  # 15 PASS · 0 FAIL · 0 SKIP
pwsh -NoProfile -File scripts\verify-pdf.ps1 -Pdf target\bench-fixtures\pdf_pad_100mb.pdf -ExeDir target\release  # PASS
```

`smoke.ps1` 那条 SKIP 仍是管理员会话 UIPI 拒 `Shell_NotifyIconW`（`0x80070005`），不是缺陷。

**新工具：`budget.rs`（进程级总账）**

- `budget::try_reserve(n) -> Option<Permit>`，`Permit` drop 即归还；上限 `LIMIT_BYTES = 256 MB`。
- 接入三处：`source_cache::store_tagged`、`pdf::with_document`、`pagecache::put`。三处策略一致：
  **先满足自己的上限 → 再申请全局额度 → 拿不到就继续淘汰自家条目 → 自家空了还不给就不缓存**。
- 排查内存就搜日志里的 `MEM used=..MB cap=256MB`（每帧一行，紧跟 `RENDER`）。
  参考值：100 MB 填充 PDF → `used=100MB`；8 MP JPEG 1986 宽 → `used=39MB`；小图 → `7MB`。
- 要调上限只改 `LIMIT_BYTES` 一处；**不要**再把"每处各自的上限"当总上限用
  （64+64+24+256 是加法，不是上限）。

**新工具：`verify-layers.ps1`（分层断言）**

- 纯静态扫描，**不用起窗口、不需要 exe**，CI 里最便宜的一条。
- 白名单是"计数棘轮"：`crates/qlr-app/src/layout.rs` 的 `use windows::` ≤1、`settings.rs` ≤2、
  `app/mod.rs`（prelude）不限；`crates/qlr-render/src` 只允许 `media.rs` / `svg.rs`。
  **想让白名单变大，得先改脚本**——这正是它的作用。
- 反向验证方法（改架构时自查）：随便往 `crates/qlr-render/src` 塞一个 `use windows::…` 的文件，
  脚本必须变成 `10 PASS · 1 FAIL` + 退出码 1。

**评审 §6 十项现况（别照着旧评审重复劳动）**

| 项 | 现况 |
|---|---|
| §6-1 持锁动窗口 | 已过期（`hide_preview` 早已锁外动窗口；本批把 `video_paint` 也改了） |
| §6-2 几何单点化 | 已过期（`crates/qlr-app/src/layout.rs` 的 `Layout`/`Opts`/`Bar`） |
| §6-3 `app.rs` 拆分 | 已完成（`app/` 下 10 个文件） |
| §6-4 裸 Win32 收口 | 未做（纯重构、有回归风险，收益低） |
| §6-5 渲染 worker 可重建 | 已过期（`RENDER_EPOCH` + `check_render_hang()`） |
| §6-6 折行结果缓存 | 未做（下一个候选，先量 wrap 占比） |
| §6-7 `PaintBuffer` 复用 / 每帧 GDI | 部分（`bench.ps1` 的 GDI 列与泄漏判据已有） |
| §6-8 空闲唤醒降频 | **实测不值得做**：空闲 30 s CPU 15.6 ms（单核 0.05%） |
| §6-9 内存总预算 | 本批已做（`budget.rs`） |
| §6-10 分层断言 | 本批已做（`verify-layers.ps1`） |

**下一步（建议顺序）**

1. 每行落盘的分配：`emit_line`/`flush` 里 `trim().to_string()` + `format!` + `clip` 是三次分配，
   8 MB HTML 有 233638 行、合计约 56 ms——要么改成"一个可复用缓冲拼好再 push"，要么让 `clip` 就地截断。
2. §6-6 折行结果缓存：先加一条 `#[ignore]` 基准量 `paint_code` 里 wrap 的占比，再决定做不做。
3. 发布前：把 `AGENTS.md` §8 的"当前基线（1.8.2 / 251 passed）"和 §4 的矩阵按现状同步
   （本批没改，因为基线每批都在动）。
4. 推送/发版：`git checkout main; git merge --ff-only fix/1.8.3-batch1; git push origin main`，
   然后 `pwsh -File scripts\pack.ps1`（批次一已把 `SHA256SUMS.txt` 改成先生成再压缩）。

**必须人工确认**（自动化测不出来）：200% 缩放下滚轮缩放 4K JPEG、来回翻 PDF 页、md/html 长文档首屏，
视觉应与上一版完全一致；以及"预览器占内存"的实际体感（看 `MEM` 行是否稳定在 256 MB 以内）。

## 1.8.3-fix 批次四接力说明（2026-11-03，滚一下卡在折行上）

**这一批做了什么**：HTML 每行的三次分配压成一次（8 MB 首屏 87.1 → 62.9 ms）；
折行量宽从逐字符累加改二分（长段落文档每滚一格 1359 次量宽 / 12.1 ms → 243 次 / 5.1 ms）；
新增折行结果缓存 `crates/qlr-app/src/wrapcache.rs`（短行文档每帧量宽 29 → 5 次）；
绘制循环补折行计时与 `SLOWWRAP` 日志。分支 `main`（已合并批次一～三，本地领先 origin 若干提交）。

**新增/改动的文件**

- `crates/qlr-app/src/wrapcache.rs`（新）：`WrapCache::{clear, set_width, get, put, stats}`，
  `CAP = 512`，满了整本清空；`State.wrap_cache`，`set_content` 时清空。
- `crates/qlr-render/src/wrap.rs`：`fit_prefix` 二分；单测
  `fit_prefix_matches_the_linear_reference`（与旧线性算法逐字节对拍）、
  `fit_prefix_measures_logarithmically_not_per_char`（单行 ≤24 次量宽）。
- `crates/qlr-app/src/app/chrome.rs`：绘制循环里 `set_width` → 查缓存 → 未命中才折 →
  `put`；`WRAP` 行带 `量宽 N 次 / x.x ms · 缓存命中 h/m（存 k 条）`。
- `crates/qlr-render/src/html.rs`：`clip_line` + 字段级借用分离（`clip_line_matches_truncate_chars` 对拍）。

**看到 `WRAP` 行怎么读**

| 字段 | 说明 | 怎么用 |
|---|---|---|
| `量宽 N 次` | 本帧调用 `GetTextExtentPoint32W` 的次数 | 短行文档应在 5 上下（缓存生效）；几十次以上说明缓存没命中或行太长 |
| `x.x ms` | 折行总耗时 | ≥4 ms 会额外打 `SLOWWRAP` |
| `缓存命中 h/m（存 k 条）` | 累计命中/未命中/缓存条数 | **`存 0 条` = 没进缓存**（#161 那个坑）；`h` 不涨说明屏上全是新行 |
| `吃掉 N 原始行` | 折出 29 显示行用掉了多少原始行 | 4 行 = 长段落；29 行 = 短行 |

**下一步（有数字依据，别凭感觉选）**

1. `GetTextExtentExPointW` 一次拿到"宽度内能放几个字符"，把长段落 243 次量宽压到约 33 次
   （5.1 ms → 约 0.7 ms）。要改 `wrap::push_wrapped` 的度量回调签名；注意"能放下几个字符"与
   现在"宽度严格小于"的口径可能差一个字符，需另建对拍测试。
2. §6-4 裸 Win32 收口残量（`platform` 补 `stop_timer_by_ids`/`post_quit`/`terminate_self`/`now_ms`，
   然后 `verify-layers.ps1` 加一条"app 不准直接调这几个"）。
3. §6-7 `PaintBuffer` 复用 + 每帧 GDI 计数（`bench.ps1` 已有 GDI 列与泄漏判据，app 侧还没有每帧数）。
4. Markdown `layout` 全量仍有 240 ms 余量（早停只解决首屏）。
5. **WebView2 已留档，别重做调研**：分支 `spike/webview2`（提交 `3f2a3e7`）里有完整的承载层
   （`crates/qlr-platform/src/webview.rs`、`build.rs`、`third_party/webview2/`）与实测代价：
   6 个 `msedgewebview2.exe`、工作集 287–294 MB、建 145–328 ms、导航 75–81 ms、空闲 CPU 1.5%、
   数据目录 8.9 MB、关闭 1–2 s 全退、包体 +195 KB。用户决定**暂时不做**，重启这个话题前先看这份数字。

**必须人工确认**（自动化测不出来）

- 200% 缩放下滚轮缩放 4K JPEG、来回翻 PDF 页、md/html 长文档首屏：视觉应与上一版一致
  （本批没动几何/DPI 与字体路径）。
- 拿一份**长段落**文档（自己写一份每行 300 字以上的 txt/md 即可）滚轮滚动，
  确认不再有"一格一顿"的顿感；再看 `app.log` 是否还有 `SLOWWRAP`。

## §6-4 裸 Win32 收口接力说明（2026-11-04，平台层完善）

**这一批做了什么**：app 层最后 6 处直接 Win32 调用收进 `qlr-platform::window`
（新增 `stop_timers / post_quit / terminate_self / now_ms`），`app/mod.rs` prelude 不再再导出
`GetTickCount / GetCurrentProcess / TerminateProcess / KillTimer / PostQuitMessage / SetTimer`；
`scripts/verify-layers.ps1` 新增「app 不直接调 Win32 计时/退出原语」断言（**11 → 12 PASS**）。
纯重构、行为与热路径零改动（不在 `repaint`/滚动/折行循环里，零分配）。

**改动文件**：`crates/qlr-platform/src/window.rs`、`crates/qlr-app/src/app/{mod,boot,shell,chrome,show,input}.rs`、
`scripts/verify-layers.ps1`。

**回归**：`cargo test` **306 passed / 0 failed**；`cargo fmt`/`clippy` 零输出；`verify-layers` **12 PASS · 0 FAIL**。

**下一步（建议顺序，都有数字依据）**：
1. §6-7 `PaintBuffer` 复用 + 每帧 GDI 计数（`bench.ps1` 已有 GDI 列，app 侧还没有每帧数）；
2. `GetTextExtentExPointW` 折行量宽（长段落 5.1 ms → 约 0.7 ms，需"能放几个字符"口径的对拍测试）；
3. 后端契约第一刀（`PreviewBackend` + 现有自研渲染接入，不含 sidecar）。

## 折行量宽 GetTextExtentExPointW 接力说明（2026-11-04，平台层完善第 2 项）

**这一批做了什么**：`Measurer` 新增 `fit_width_styled`（一次 `GetTextExtentExPointW` 返回可放字符数）；
`wrap.rs` 增 `FitFn` / `push_wrapped_fit` / `fit_prefix(Option<FitFn>)`；`chrome.rs` 折行改走加速通道并记
「适配 N 次」。改动文件：`crates/qlr-platform/src/window.rs`、`crates/qlr-render/src/wrap.rs`、
`crates/qlr-app/src/app/chrome.rs`。

**口径提醒（不是回归）**：加速通道与旧二分在个别 CJK 尾段差 1 字符（按线性口径填满，更优），
短行 / ASCII 不变；对拍测试 `fit_path_matches_linear_reference`（以 `fit_prefix_linear` 为 oracle）。

**回归**：`cargo test` **307 passed / 0 failed**；fmt/clippy 零输出；`verify-layers` **12 PASS**。
真窗口 `longlines.txt`（6648 行 / 756px）每滚 量宽 243→30、适配 24、折行 5.1→3.3 ms。

**下一步**：§6-7 `PaintBuffer` 复用 + 每帧 GDI 计数；后端契约第一刀（`PreviewBackend`，无 sidecar）。

## §6-7 PaintBuffer 复用 + 每帧 GDI 计数接力说明（2026-11-04，平台层完善第 3 项）

**这一批做了什么**：`window.rs` 的 `PaintBuffer` 加 `w/h`；新增线程本地缓存 `PAINT_BUFFER` +
`acquire_paint_buffer` / `release_paint_buffer` / `pub drop_paint_buffer` / `log_paint_gdi`；
`paint_list`、`paint_doc_with` 改"取用缓存缓冲 + `EndPaint` 前归还"，不再每帧新建/销毁整窗位图；
`app/show.rs` 的 `hide` 调 `window::drop_paint_buffer()`。改动文件：`crates/qlr-platform/src/window.rs`、
`crates/qlr-app/src/app/show.rs`。

**A/B 开关**：`QLRUST_NO_PAINT_REUSE=1` 退回每帧新建（仅实测用）；debug 级每帧打 `PAINT doc gdi=.. user=.. Xms`。

**回归**：`cargo test` **307 passed / 0 failed**；fmt/clippy 零输出；`verify-layers` **12 PASS**；
`smoke.ps1` **16 PASS · 1 SKIP**。A/B（880×748、longlines.txt、滚 10 次）：每帧绘制 **3.13 → 2.54 ms**
（省 ~0.59 ms / ≈19%），GDI 恒 **25**、无增长。

**下一步**：后端契约第一刀（`PreviewBackend` + 现有自研渲染接入，无 sidecar）。

## §7 PreviewBackend 接力说明（2026-11-04，平台层完善第 4 项）

**这一批做了什么**：新建 `crates/qlr-app/src/app/backend.rs` —— `trait PreviewBackend`
（`id/priority/can_handle/surface/natural_size`）+ 四个零大小后端（`NativeBackend`/`ArchiveBackend`/
`ShellBackend`/`PluginBackend`）+ `resolve(path,kind)` 单一入口；`show.rs`/`render.rs` 的选路全部改走
`resolve`，`shell.rs::shell_candidate` 删除并入 `ShellBackend`。改动文件：`app/backend.rs`(新)、
`app/mod.rs`、`app/show.rs`、`app/render.rs`、`app/shell.rs`、`scripts/verify-layers.ps1`；文档 ADR-0009 + ARCH + LOG + STATUS。

**关键口径（不是回归，是修正）**：`resolve` 第一步 `if is_archive(path)` 直接返回 `ArchiveBackend`——
压缩包按**扩展名**判定，即使内容被嗅探成文本（假 `.zip`）也走"解析失败"提示，而不是退化成文本预览。

**回归**：`cargo test` **312 passed / 0 failed**；fmt/clippy 零输出；`verify-layers` **13 PASS**
（新增 §2c「app 只在 backend.rs 里枚举 viewer_for」）；`smoke` **16 PASS · 1 SKIP**；
端到端 verify-archive **11 PASS**、verify-media 15、verify-office 23、verify-mediashape 13。

**下一步**：让 `repaint` 的绘制通道按 `Surface` 驱动（本刀只分类未重构）；然后在 `PreviewBackend`
之上接 office 轻型 / html 改进后端；PDF 单客户 sidecar 后续评估；之后重心转插件。

## §7 第二刀 repaint 按 Surface 驱动 接力说明（2026-11-04，平台层完善第 5 项）

**这一批做了什么**：`State` 加 `surface: Option<backend::Surface>`；`show_preview` 在 `backend::resolve`
后写入（`hide_preview` 收起清 `None`）；`chrome.rs::repaint` 顶层通道改按 `surface` 路由
（List→paint_archive、ChildWindow→系统处理器只画顶栏、其余 Canvas::Bitmap/Text），仍保留
`has_archive_view()`/`shell_preview.is_some()` 运行时守卫。改动文件：`crates/qlr-app/src/app/session.rs`、
`app/show.rs`、`app/chrome.rs`、`app/boot.rs`。

**设计边界**：`fit_bitmap`（SVG/视频等比、图片按窗口比例）/`plain_doc`（HTML/音频去行号）/`reserve_bar`
（视频留进度条）是逐格式呈现特例，`Surface` 只分 Bitmap/Text 表达不了，**故意留在 repaint**，不硬塞。

**回归**：`cargo test` **312 passed / 0 failed**；fmt/clippy 零输出；`verify-layers` **13 PASS**；
`cargo build --release` 24.20s；`smoke` 全过（16·1 SKIP=托盘 UIPI）、verify-archive 11 / office 23 /
media 15 / mediashape 13。

**下一步**：`PreviewBackend` 之上接 office 轻型 / html 改进后端；PDF 单客户 sidecar 后续评估；之后重心转插件。

## window.rs 纯搬家拆 window/ 接力说明（2026-11-04，平台层完善第 6 项）

**这一批做了什么**：`crates/qlr-platform/src/window.rs`（3660 行）纯搬家拆成 `window/` 目录：
`mod.rs`（窗口类/创建/显示/几何/输入/消息泵/子窗口）+ `shadow`/`text`/`settings`/`chrome`/`paint`/`scroll`/
`audio`/`toc`/`bitmap` 九个子模块。每个子模块顶部只多一行 `use super::*;`，函数体一字未改；`mod.rs` 末尾
`pub use <mod>::*;` 再导出（`paint` 用 `pub(crate) use`）。改动文件：`crates/qlr-platform/src/window/**`、
`scripts/verify-layers.ps1`；文档 ARCH + LOG + STATUS。

**口径**：只动了可见性（跨模块把 `fill_solid/frame_solid/draw_line/measure_line`、`FONTS`/`FontSet::get`、
`TextStyle::weight/color` 提到 `pub(crate)`），**无任何逻辑改动**。`window/mod.rs` 的 `mod tests` 仍在，
经 `use super::*` + 再导出看到各子模块项。

**回归**：`cargo test` **312 passed / 0 failed**；fmt/clippy 零输出；`verify-layers` **14 PASS**（新增 §3b
「platform 单文件 ≤ 1600 行」）；`cargo build --release` 26.05s；`smoke` 16 PASS · 1 SKIP（tray UIPI）。

**下一步**：`PreviewBackend` 之上接 office 轻型 / html 改进后端；PDF 单客户 sidecar 后续评估；Markdown
`layout` 240ms 可作渲染层独立小项；之后重心转插件。
