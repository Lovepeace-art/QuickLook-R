# Windows FFI 约定与坑清单（CONVENTIONS）

## 1. 代码约定

### 命名

- crate 内模块名用单数、短名：`window` / `tray` / `config`。
- platform 层函数一律 `动词_名词`：`create_preview_window` / `show_at` / `send_request`。
- 不做类型再包装：直接用 `windows` crate 的 `HWND`/`HDC`/`LRESULT`。唯一的例外是 `qlr-app` 里的 `Hwnd(isize)`——因为 `HWND` 不是 `Send`，要进 `Mutex<State>` 必须换成可 `Send` 的表示。

### 错误

- platform 层：返回 `windows::Result<T>` 或 `Option<T>`，**不吞错误**。
- 跨层：用 `qlr_platform::to_core(e)` 转成 `qlr_core::Error::Platform`（保留 `HRESULT=0x…`）。
- 降级策略写在调用点（app 层），不要藏在 platform 里。

### 日志

- 只有 `qlr_core::log_{error,warn,info,debug}!` 四个宏，形式 `(SCOPE, fmt...)`。
- `SCOPE` 用模块名小写（`"app"` / `"platform"`）。
- 级别纪律：`error` = 功能不可用且用户能感知；`warn` = 降级但仍可用（托盘失败、亚克力不可用）；`info` = 生命周期事件（启动/就绪/SHOW/HIDE/退出，**带毫秒耗时**）；`debug` = 排查用（消息 id、请求内容）。
- **禁止 `println!`/`eprintln!`**：`#![windows_subsystem = "windows"]` 下没有 stderr。

### 测试

- core 层每个模块都要有单测（配置默认值/钳制、路径便携判定、IPC 编解码、日志时间格式）。
- platform 层只测纯函数（`wide`、`acrylic::gradient_color`、`centered_rect`）。
- 涉及真实窗口/进程的用 `scripts/smoke.ps1`，不写成 `#[test]`。

## 2. 坑清单（都是本项目真实踩过的）

### #1 `DrawTextW` 传空字符串 → 进程直接 0xC0000005

`windows` crate 的包装是：

```rust
pub unsafe fn DrawTextW(hdc: HDC, lpchtext: &mut [u16], lprc: *mut RECT, format: DRAW_TEXT_FORMAT) -> i32 {
    DrawTextW(hdc, transmute(lpchtext.as_ptr()), lpchtext.len().try_into().unwrap(), lprc, format)
}
```

空切片 → `cchText = 0`，而 `cchText == 0` 在 user32 里表示"**自己去 `wcslen`**"，那个指针是空 `Vec` 的悬垂指针 → 访问违例。

现场特征（记下来，将来还会遇到同类）：进程在 `WM_PAINT` 后无声消失、Rust 的 panic hook **没有**输出、事件日志里是

```
错误模块名称: user32.dll
异常代码: 0xc0000005 / 0xc000041d
错误偏移量: 0x000000000001f16b
```

修法：绘制前 `if buf.is_empty() { y += line_height; continue; }`（空行只占高度不画字）。
**同类 API 都要注意**：凡是 `&[u16]` + 由长度推 `cch` 的 Win32 包装（`ExtTextOutW`、`TextOutW`、`SetWindowTextW` 等），空切片语义都和 C 侧不同。

### #2 windows 0.62 的模块归属（写错就是编译错误，别去猜）

| 项 | 所在模块 |
|---|---|
| `PCWSTR` / `BOOL`(部分) | `windows::core` |
| `HBRUSH` `MONITORINFO` `GetMonitorInfoW` `MonitorFromWindow` `BeginPaint` `EndPaint` `DrawTextW` | `windows::Win32::Graphics::Gdi` |
| `COPYDATASTRUCT` | `windows::Win32::System::DataExchange` |
| `CreateMutexW` | 需要 feature `Win32_Security` |
| `NOTIFYICONDATAW` `Shell_NotifyIconW` | `windows::Win32::UI::Shell` |

### #3 `HWND` 不是 `Send`

要放进全局 `Mutex<State>` 就包一层 `Hwnd(isize)`（app.rs 有现成实现），用时 `Hwnd::raw()` 取回。

### #4 持锁动窗口 = 死锁

`ShowWindow`/`SetWindowPos` 会**同步**重入 `wndproc` 派发 `WM_PAINT`。所以绘制内容必须先写 `LINES` 并放锁，再动窗口。参见 ARCH.md §4。

### #5 `Shell_NotifyIconW` 返回 `拒绝访问 (0x80070005)`

进程以管理员身份运行、explorer 非提权时，UIPI 会拦掉托盘消息。**这是环境限制，不是代码 bug**：代码按"降级不致命"处理（WARN + 继续），`scripts/smoke.ps1` 在检测到提权时把该项标成 `SKIP` 而不是 `FAIL`。

### #6 用 `Start-Process` 启动被测进程会挂住自动化

子进程继承当前 shell 的 stdio 句柄，在 CI/agent harness 这类"捕获输出"的环境里，调用方会一直等管道关闭 → 看起来像卡死（实测 120 s 超时）。
用 WMI：`Invoke-CimMethod -ClassName Win32_Process -MethodName Create`（见 `scripts/smoke.ps1` 的 `Start-Qlr`）。

### #7 PowerShell 脚本的两个小坑

- 文件首行不能写 `//` 注释（会被当成命令）。
- `pwsh -File x.ps1 -Fixtures @('a','b')` 会把数组当成一个字符串传进去；要传数组用 `pwsh -Command "& '.\x.ps1' -Fixtures @('a','b')"`。

### #8 `FindWindowW` 的类名必须和注册时完全一致

类名是硬编码常量（`qlr_core::ipc::HOST_WINDOW_CLASS`），改一处必须改另一处，冒烟脚本也按这个类名枚举窗口。

### #9 工作目录里的 exe 拿不到 Shell COM（`0x80070005`）

> 根因已在 **#29** 查明：文件被打了 Low 完整性标签。两节一起看。

**症状**：`--probe-selection` 永远是 `source=none count=0`，debug 日志里是
`创建 ShellWindows 失败：拒绝访问。 (0x80070005)`；而同一个 exe 复制到别处就正常。

**根因**（用最小复现程序打印进程令牌确认）：DSH 的文件沙箱给"工作目录内"的可执行文件一个**降了完整性级别的令牌**（实测 `il_rid=4096` = Medium；同一文件放到 `D:\` 或 `C:\Windows\Temp\` 下是 `il_rid=12288` = High）。完整性级别不足 → `CoCreateInstance(CLSID_ShellWindows)` 这类**跨进程 COM 激活**被拒。注意 `CoCreateInstance` 的 `CLSCTX` 不是原因（`1` 无 in-proc → `0x80040154`，`4/5/21/23` 在正常令牌下都 OK）。

**规避**：跑需要访问 Shell 的集成测试时，把 exe 复制到沙箱外（`scripts/probe-selection.ps1` 默认 `-ExeDir D:\qlrust-bin`）。**开发正常构建不受影响**，只有"要拿资源管理器选区"时才有这个问题。

### #10 复制到 `D:\` 的那份 exe 会变旧

因为 #9，集成测试跑的是 `D:\qlrust-bin\qlrust.exe` 这份**副本**。改完代码只 `cargo build` 而忘了重新复制，就会拿旧二进制测新代码——症状极其迷惑：日志格式像是新的，但某个新加的 debug 行就是不出现（本项目在这上面白绕了一轮，误判成 "`QLRUST_LOG` 不生效"）。
`scripts/probe-selection.ps1` 每次运行都会自动复制，手工验证时记得手动 `Copy-Item`。

### #11 `Add-Type` 定义的类型不跨 `pwsh` 进程

在工具调用 A 里 `Add-Type -TypeDefinition ... QlrWin`，在工具调用 B 里 `[QlrWin]::Frames()` 一定报 "找不到类型"——每次 `pwsh` 调用都是新进程。**枚举窗口、启动被测进程、读结果必须在同一次调用/同一个脚本内完成**。

### #12 资源管理器窗口数量不稳定，且"前台"必须是真前台

- `Start-Process explorer.exe "/select,<file>"` 会复用或关闭已有窗口，短时间内 `EnumWindows` 枚举到的 `CabinetWClass` 数量在 `0 / 1 / 多` 之间跳。脚本要**轮询等到窗口出现**，并逐个候选尝试，不能硬取第一个。
- `--probe-selection`（不带 hwnd）走 `GetForegroundWindow()`，而自动化里前台往往是宿主自己的终端窗口 → 必然 `source=none`。要么先 `SetForegroundWindow` 并**校验 `GetForegroundWindow()==目标`**，要么直接用 `--probe-selection-hwnd <HWND>` 把窗口钉死。`SetForegroundWindow` 本身受前台锁限制，返回值不可信，必须回读校验。

### #13 `InvalidateRect` 在 `Graphics::Gdi`，不在 `WindowsAndMessaging`

`windows` 0.62 把它归到了 GDI：`windows::Win32::Graphics::Gdi::InvalidateRect`。
按名字直觉往 `WindowsAndMessaging` 里加会得到 `no InvalidateRect in ...`。

### #14 `std::env::temp_dir()` 在本机不可写

单元测试里往 `%TEMP%` 写文件会得到
`Os { code: 5, kind: PermissionDenied, message: "拒绝访问。" }`。
`qlr-render` 的测试统一走 `crates/qlr-render/src/testutil.rs`（落到 `target/test-tmp/`）。

### #15 冒烟夹具必须有"第三档"兜底

`smoke.ps1` 原来只找仓库内的 `spike\fixtures\`，而本仓库的夹具在**上级目录**，
找不到就"用脚本自身当夹具"——于是渲染断言永远失败，还会让人误判成渲染坏了。
现在是三级：仓库内 → 上级 `spike\fixtures` → 用 `System.Drawing` 现场生成一张 JPEG。

### #16 文本行数：结尾换行不算一行

`"a\n"` 是 1 行不是 2 行。`split('\n')` 之后要判断原文是否以换行结尾，是则 pop 掉尾部空行，
否则行数统计、`tail` 模式取最后 N 行都会错位。

### #17 未文档化的合成开关影响的是**整窗内容**，不是背景

`SetWindowCompositionAttribute` + `ACCENT_ENABLE_ACRYLICBLURBEHIND` 会把整个窗口
（含我们画上去的位图）变成与壁纸混色的半透明。症状会被误判成"解码错/颜色空间错"：
实测纯灰 128,128,128 在屏上是 194,185,177，红块 255,0,0 变成 255,37,36。

排查口诀：**颜色不对时先分清"解码错"还是"合成错"** —— 造一张程序生成的纯色图
（`System.Drawing` 画色块），`PrintWindow`/`CopyFromScreen` 截屏后按坐标取像素，
对照"亚克力开 / 关"两组数字，比读解码代码快十倍。

结论：**显示位图时必须关亚克力**，并且自己 `FillRect` 刷一层不透明底色
（窗口类背景刷是空的，否则周围留白会露出桌面）。文字/占位状态才用亚克力。

### #18 `KEYBDINPUT.dwFlags` 的类型叫 `KEYBD_EVENT_FLAGS`

没有 `VIRTUAL_KEY_FLAGS` 这个名字。`SendInput` 的 `INPUT` 数组要按 `size_of::<INPUT>()`
传步长。重放按键要带 `dwExtraInfo` 标记（本项目 `REPLAY_MARKER = 0x514C_5452`），
否则将来做按键统计时分不清是用户按的还是自己重放的。

### #19 `WM_LBUTTONDBLCLK` 等消息常量不在已有的 import 列表里

`use windows::Win32::UI::WindowsAndMessaging::{WM_LBUTTONUP, WM_RBUTTONUP, …}` 这种
逐项 import 很容易漏（本次就漏了 `WM_LBUTTONDBLCLK`，`E0425: cannot find value`）。
加消息分支时先确认常量已 import。

### #20 PowerShell 里构造"带标签的点"别用嵌套数组

`@(@($x,$y,'标签'))` 再 `$p[0]` 得到的是嵌套数组不是 `$x`。用哈希表
`@{n='标签';x=$x;y=$y}`，字段名即语义，取错会在屏幕上立刻看出来。


## 3. 提交规范

```
type(scope): 一句话摘要

为什么这么做（不是"改了什么"）。
验证：跑了什么命令、看到什么数字。
```

`type ∈ {feat, fix, docs, refactor, test, chore, perf}`；`scope` 用 crate 名或模块名（`core`/`platform`/`app`/`docs`）。

### #21 验证托盘图标前必须先"单实例 + 清僵尸图标"

`Stop-Process` 杀掉进程后，它的托盘图标**还留在任务栏上**（僵尸图标，
鼠标划过托盘区才消失）。如果直接截屏比对新旧图标位置，会得出
"`Shell_NotifyIconGetRect` 偏了 100 px"这种错误结论 —— 我们就这样误判过一次。

正确流程：杀光所有实例 → `SetCursorPos` 在托盘区来回扫几遍清僵尸 → 截屏确认没有残留
→ 启动一个实例 → 再截屏。`Shell_NotifyIconGetRect` 本身是准的（实测与截屏逐像素吻合，
只有高度报 40 含内边距、宽度 24 是图标本体）。

### #22 Windows 10 默认把新托盘图标塞进 `^` 溢出区

`Shell_NotifyIconW(NIM_ADD)` 返回 TRUE **不等于用户能看见**。新图标默认进溢出区，
要用户自己拖出来。所以"启动气泡"不能省 —— 它是唯一主动告诉用户"我在这儿"的手段。

### #23 exe 没有资源时 Windows 会显示"未知发布者"

没有 `VERSIONINFO`、没有 manifest、没有签名的 exe，运行时 Windows 会提示
"未知发布者 / 没有签名"，用户第一反应是"这程序是不是有毒/没编译成功"。
解决方案分两层：
- **能做**：`build.rs` + `.rc` 嵌入 `ICON` + `VERSIONINFO` + `RT_MANIFEST`，
  manifest 里声明 `asInvoker` / `PerMonitorV2` / `supportedOS`，文件属性里就有名字和版本。
- **做不到**：真签名需要代码签名证书（几千元/年），个人项目不买，只能在 README 写清楚
  "右键属性→解除锁定"或"更多信息→仍要运行"。

`.rc` 里写中文必须加 `#pragma code_page(65001)`，文件存 UTF-8 无 BOM；
否则 `rc.exe` 会按系统 ANSI 解，报 `RC2133: unexpected value in value data`。

### #24 manifest 里千万别声明 `requireAdministrator`

提权进程无法向非提权的 explorer 托盘注册图标（UIPI 拒绝，`0x80070005`）。
本项目读 Shell 选区、加托盘图标都不需要提权，manifest 固定用 `asInvoker`。
另外 `RT_MANIFEST` 的资源 ID **必须是 1**，否则系统不认。

### #25 桌面不在 `IShellWindows` 里，只能走 `SWC_DESKTOP`

`IShellWindows` 只枚举**资源管理器窗口**。桌面（`Progman`/`WorkerW`）经常根本不在列表里
（实测同一台机器上时有时无：`count=3` 全是被打开的文件夹窗口）。
所以"桌面上选中文件按空格没反应"的根因不是钩子、不是重放，而是**没有候选可查**。

正确做法：先判断前台根窗口类名是不是 `Progman`/`WorkerW`，是则走

```rust
shell_windows.FindWindowSW(
    &VARIANT::default(), &VARIANT::default(),
    SWC_DESKTOP,                       // ShellWindowTypeConstants(8)
    &mut resolved,
    SWFO_NEEDDISPATCH,                 // ShellWindowFindWindowOptions(1)
)
```

`SWC_DESKTOP` 在 `windows 0.62.2` 的 `Win32/UI/Shell/mod.rs:55807` 有导出
（**没有** 导出 `SWFO_FOREGROUND`，别照着 QuickLook 的 C++ 抄）。

桌面那一项的 `SIGDN_FILESYSPATH` 对真实文件是好的；只有回收站这类虚拟项会
`E_INVALIDARG`，靠"`PARENTRELATIVEPARSING` + 文件夹路径 join + `exists()`"兜底跳过。

### #26 `FindWindowSW` 会间歇性返回 S_OK + 空指针，必须重试

进程刚起来、或 Shell 的 desktop view 还没注册好时，`FindWindowSW(SWC_DESKTOP, …)`
返回 **S_OK 但 `*ppdisp` 是 NULL**，在 `windows` crate 里表现为 `Err(code=0x00000000)`
—— 看起来像"成功但没用"。实测同一状态下 `comtest` 能成、`qlrust` 报 `Err(0x0)`，
所以**必须带重试**：本项目用 3 轮 × 2 种 `ShellWindowFindWindowOptions`（`SWFO_NEEDDISPATCH`
与 `0`），每轮之间 `sleep(15ms)`。

### #27 `WM_COPYDATA` 处理期间不能调 COM（0x8001010D）

`WM_COPYDATA` 是别的进程 `SendMessage` 送来的，处理它时本线程处于
**输入同步呼叫**（input-synchronous call）状态。此时发起 COM 激活
（`CoCreateInstance(CLSID_ShellWindows)`）会被系统直接拒绝：

```
创建 ShellWindows 失败：因为应用程序正在发送一个输入同步呼叫，
所以无法执行传出的呼叫。 (0x8001010D) (HRESULT=0x8001010D)
```

正确做法是**只排队、不处理**：把请求塞进 `static PENDING: Mutex<Vec<Request>>`，
再 `PostMessage(hwnd, ipc::WM_QLR_INVOKE, 0, 0)`；等这条普通队列消息被取出时，
线程早已退出输入同步状态，COM / 渲染都能正常跑。
（真实按空格不受影响：那条路径是钩子线程 `PostMessage` 过来的，本来就不同步。）

### #28 跨进程给桌面 `SysListView32` 设选中状态无效

想自动化"选中桌面某个图标"，`LVM_SETITEMSTATE (0x102B)` 无论怎么发都不生效
（`LVM_GETITEMTEXT`/`LVM_GETITEMPOSITION` 也一样：要么名字为空，要么坐标都是 `20,20`）。
跨进程读写 `LVITEM` 需要 `VirtualAllocEx` + `WriteProcessMemory`，直接传本进程指针无效。

**可用手段**（按可靠性排序）：
1. `keybd_event` 发方向键：先点桌面空白，再点第一列图标，然后连按 `Down` 逐项移动；
2. `Ctrl+A` 全选（`keybd_event` 0x11 / 0x41）；
3. 鼠标点空白 + 回车打开。

不要指望首字母 type-ahead（按 `w` 选 `webwxgetmsgimg.jpg`）—— 桌面 ListView 上实测不生效。
### #29 DSH 工作目录里的 exe 会被打 Low 完整性标签

现象：同一份 exe，放在 `<工作区>\...` 里运行时
托盘图标加不上、Shell 选区读不到，双双 `0x80070005`；复制到 `D:\` 或桌面就正常。

自查（powershell）：
```powershell
icacls qlrust.exe | findstr Mandatory
# 有输出（Mandatory Label\Low Mandatory Level）→ 被降权
# 无输出 → 正常
```

原因：DSH 的文件沙箱给工作目录内的可执行文件打了 `Low Mandatory Level` 标签，
进程启动后继承低完整性；低完整性进程不能与 Medium 的 explorer 通信（UIPI），
于是 `Shell_NotifyIconW` 与 `CoCreateInstance(CLSID_ShellWindows)` 都返回 `0x80070005`。

**规矩**：
1. 任何"给用户试"的产物、以及所有 Shell COM / 托盘相关的自动化测试，
   exe 必须复制到工作目录之外（`D:\qlrust-bin\`、`D:\qlrust-try\`、桌面）再跑；
2. 程序侧不能自救（令牌是进程级的），能做的是**启动第一屏把完整性级别写进日志**，
   托盘失败时把原因一起打出来（0.1.5 已实现：`integrity::describe()` + `warn_box`）；
3. 排障顺序：先看 `令牌完整性=`，再看 `托盘图标创建失败`，再看 `创建 ShellWindows 失败`
   —— 三条一起出现就是这一个坑，不要再往程序逻辑里找。


### #30 FFI 里"借用调用方缓冲区"的 API：缓冲区必须由句柄结构体持有

`FPDF_LoadMemDocument64` **不拷贝**字节流，文档在整个生命周期内一直引用调用方内存。
第一版把 `Vec<u8>` 留在 `open_document()` 的栈上，函数一返回就释放 →
pdfium 读到已释放内存，表现是**某些页偶发 `LoadPage` 返回 NULL**、
且 `FPDF_GetLastError()` 返回 **0（"成功"）**，错误信息完全误导。
规矩：这类 API（pdfium load_mem、stb/图像库的"外部内存"入口等）
一律把 buffer 作为字段放进守卫结构体（`DocHandle { _data: Vec<u8> }`），
让所有权与句柄同寿。**错误码 0 + 指针为空 = 我们自己搞坏了内存**，别再怀疑文件。

### #31 渲染器基准要用"真 PDF"，手写生成器容易生成坏文件

`_bench/pdfium/makepdf.ps1` 手写 PDF 对象表时，`/Length` 少算 1 字节（漏了 stream 数据末尾的换行），
pdfium 不报错、直接把页面渲染成**全白**——看起来像"渲染器坏了"，其实是夹具坏了。
做 PDF 基准请用 Word 导出（`$doc.SaveAs($path, 17)`），或至少用像素断言
（`scripts/verify-pdf.ps1` 会检查"有白底 + 有深色内容"）而不是肉眼判断。

### #32 StretchDIBits 的源矩形 y 从位图**底部**量起（top-down DIB 也一样）

画位图用 top-down DIB（BITMAPINFOHEADER.biHeight 取负）是对的，但**源矩形**的坐标
仍按"从底部量"解释。想把"从顶部往下平移 N 像素"画出来，必须写 sy = bh - ch - N，
横向同理 sx = bw - cw - N。

写错的症状极隐蔽：**不裁剪时（位图完全装得进客户区）一切正常**，一旦位图比客户区大，
屏幕上是纯白一片（其实画的是页面/图片另一端），平移几步又会"偶然"露出一点内容——
很容易误判成"渲染器没渲染出东西"。定位手段：渲染日志里打 ink=（位图内非白像素数）
与 PAINT bitmap … pan=… client=…，两者对比就能确定是"内容没渲染"还是"画错了位置"。

---

## 33. `WS_EX_NOACTIVATE` 的窗口收不到 `WM_MOUSEWHEEL`

预览窗刻意用 `WS_EX_NOACTIVATE`（不抢焦点），代价是：系统把滚轮消息投给**前台窗口**
（资源管理器/桌面），我们永远收不到 `WM_MOUSEWHEEL`。想在"鼠标停在预览窗上时"缩放，
只能旁听低级鼠标钩子：

```rust
SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hmod, 0)   // 回调里
// 1) code < 0 → CallNextHookEx
// 2) !ACTIVE（预览窗没开）→ 放行
// 3) wparam != WM_MOUSEWHEEL → 放行
// 4) flags & LLMHF_INJECTED → 放行（不然自己 SendInput 会自激）
// 5) GetWindowRect(预览窗) 后判断 pt 是否落在矩形内 → 不在就放行
// 6) 以上全过：PostMessage(UI 线程, WM_QLR_WHEEL, 增量) 并 return LRESULT(1)
```

滚轮增量在 `MSLLHOOKSTRUCT.mouseData` 的高 16 位：`((mouseData >> 16) & 0xFFFF) as u16 as i16`。
一格 = `WHEEL_DELTA`(120)；**一次最多吃 4 格**，触控板会一口气甩几十格。

## 34. `SetCapture`/`ReleaseCapture` 在 `UI::Input::KeyboardAndMouse`

不在 `WindowsAndMessaging`（我第一版按 `WindowsAndMessaging` 写、第二版按 `Graphics::Gdi`
写，都报 `E0425: cannot find function ... in this scope`）。正确归属：

```rust
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
// SetCapture(hwnd) -> HWND；ReleaseCapture() -> Result<()>
```

拖拽平移必须 `SetCapture`，否则鼠标移出窗口就收不到 `WM_MOUSEMOVE`，拖到一半卡住。
`WM_LBUTTONUP` 里一定要 `ReleaseCapture()`，否则别的窗口点不动。

鼠标坐标（`lparam` 低/高 16 位）是**有符号**的：

```rust
let x = (lparam.0 as i32) as i16 as i32;          // 对
let y = ((lparam.0 >> 16) as i32) as i16 as i32;  // 对
// let x = lparam.0 & 0xFFFF   → 错：负坐标会读成 6 万多
```

---

## 35. PowerShell 改代码的三个静默陷阱（都实际踩过）

1. **多行 `Replace` 会因为是 CRLF / LF 不一致而静默失配**：`Get-Content -Raw` 拿到的换行
   与 here-string 里的换行不一致时，`$c.Replace(<多行>)` 返回原串、不报错。
   改代码块请用 `edit` 工具（按字面匹配），PowerShell 只做单行替换。
2. **`Set-Content -Encoding utf8NoBOM` 会把 `\0` 原样写进文件**：一次替换把注释里的 `0`
   写成了 NUL，`edit` 工具随即判定 "binary file" 拒绝编辑。
   自查：`[System.IO.File]::ReadAllBytes($p) | Where-Object { $_ -eq 0 } | Measure-Object`。
3. **`.NET` 的当前目录不跟随 PowerShell 的 `Set-Location`**：
   `[System.IO.File]::ReadAllText("crates\...")` 会去**进程启动目录**下找（实测报
   "Could not find a part of the path ...\DSH\crates\..."）。.NET 文件 API 一律传绝对路径。

## 36. 文本预览的三条底线（对齐上游踩过的坑）

- **读入有硬上限**：`READ_LIMIT = 8 MB`，超出只读前 8 MB 并标 `truncated`。
- **行数有硬上限**：`MAX_LINES = 200_000`（与读入上限一起把最坏内存钉死）。
- **只画可视区**：`repaint` 只把 `scroll..scroll+rows` 这几十行交给 `DrawTextW`。
  上游 QuickLook 在 64 KB 文本上滚屏吃掉 6.6 GB 内存，就是把整篇塞给了控件。

### #37 基准脚本：必须先把老宿主"请走"，否则日志看不到
`scripts/bench.ps1` 每轮会 `Remove-Item` 日志再起新实例。若上一轮的常驻实例还活着：
新实例 `FindWindowW` 找到它 → 把文件**转交**过去 → 新实例写自己的启动块到新日志、随即退出；
而老宿主握着的是**已删除的旧文件句柄**，它的 `RENDER` 行写进那个看不见的旧文件。
现象：每轮 `FAIL(无 RENDER)`、日志里只有"请求已转交"、`PeakRSS` 只有十几 MB（那是发送方）。
修法：每轮先 `qlrust.exe --quit` 并等进程真的消失，再删日志、再起宿主。
（2026-10-30 补了前置 `--quit`，但脚本仍未彻底稳定，M2 的数字改用手工流程实测。）

### #38 折行宽度必须等于绘制宽度
`window::paint_text` 每行 `RECT{ left: rc.left+24, right: rc.right-24 }`、字体 `DEFAULT_GUI_FONT`、
`line_height = 24`。所以：
- `window::Measurer` 必须 `SelectObject` **同一个** `DEFAULT_GUI_FONT`（换字体，量的宽度立刻失真）；
- 折行可用宽度 = `(rc.right - rc.left) - 48`；
- 状态行也占一行，正文行数 = `行数 - 1`。
不这么对齐的典型症状：明明折了行，右边还是出现 `…`（`DT_END_ELLIPSIS` 生效）。
另外 `Measurer` 这类新增类型**要放在 `#[cfg(test)] mod tests` 之前**，
否则 `clippy -D warnings` 会报 `items after a test module`。

### #39 工作区里的 exe 是 Low 完整性：连测试都不能往 %TEMP% 写
在 DSH 工作目录（`<工作区>`）内编译出来的 exe 带
`Mandatory Label\Low Mandatory Level:(I)(NW)` —— `NW` = no-write-up，**Low 进程写 Medium 对象一律 `ACCESS_DENIED(5)`**。
于是 `cargo test` 里任何"写临时文件"的用例都会挂，且症状有欺骗性：
- `create_dir_all(%TEMP%\xxx)` 被拒（被 `let _ =` 吞掉）→ 紧接着 `write` 报 **`NotFound(3)`**（父目录根本不存在）；
- 目录已存在时 `write` 直接报 **`AccessDenied(5)`**；
- 同样代码用 `rustc` 编到 `%TEMP%` 下再跑却正常（那个 exe 是 Medium），极易误判成"代码有问题"。
自查：`icacls <exe> | Select-String Mandatory`。
**规矩：测试夹具一律写 `<workspace>/target/…`（用 `env!("CARGO_MANIFEST_DIR")` 拼绝对路径），绝不写 `%TEMP%`。**
### #40 windows-rs 0.62 的 GDI 字体参数：三个 newtype 陷阱
用 `CreateFontW` + `SelectObject` 时连续踩三次，错误信息各不相同：
- `SelectObject(hdc, hfont)` → `expected HGDIOBJ, found HFONT`：`HFONT` 不自动转换，要 **`.into()`**。
  （`GetStockObject` 返回的 `HGDIOBJ` 则要反过来 `HFONT(x.0)` 才能塞进自己的结构体字段。）
- `CreateFontW(..., FW_BOLD, ...)` → `E0605: non-primitive cast: FONT_WEIGHT as i32`：
  `FW_BOLD` / `FW_NORMAL` 是 **`FONT_WEIGHT` newtype**，取 `.0`（本来就是 `i32`）。
- `DEFAULT_PITCH as u32` → 同样报错：`DEFAULT_PITCH` 在 `FONT_PITCH` 里是 **`u8`**，要 `DEFAULT_PITCH.0 as u32`。
另：批量文本替换时**别把 import 列表也替换掉**（把 `use ... DEFAULT_PITCH,` 改成 `DEFAULT_PITCH.0,`
会报 `expected one of ,/::/as/}, found .`）；改源码用 `edit` 工具定位，不要整段正则替换。

### #41 COLORREF 是 0x00BBGGRR，而 ClearType 让"截图判色"不可信
`SetTextColor` 要的是 `COLORREF(0x00BBGGRR)` —— **R 在最低字节**，和 HTML 的 `#RRGGBB` 正好反着写。
把 `#F0F0F0` 写成 `0x00F0F0F0` 恰好对称所以看不出来，一旦写彩色（标题金 `#FFD7A0` → `0x00A0D7FF`）就容易红蓝颠倒。
想验证颜色是否真的生效时，**不要靠截图取像素**：ClearType 次像素抗锯齿会把纯色文字混成
`(255,255,243)` 这类采样不到的值，亚克力背景本身又是 `(77,78,78)`，按精确 RGB 统计几乎全是漏检。
**以日志为准**（本项目每份内容绘制一次 `STYLED 屏幕 N 行（样式分布：…）`），截图只作留档。
### #42 GUI 程序的 `& $exe --quit` 会把脚本挂死，一律走 WMI Create
`& $exe --quit | Out-Null` 看着无害，但 **没有常驻实例时** qlrust 会自己抢到单实例、
进入消息循环不出来，PowerShell 的调用运算符就一直等它退出 —— 实测把安装/卸载脚本挂满
5 分钟被外部杀掉。正确写法：
```powershell
Invoke-CimMethod -ClassName Win32_Process -MethodName Create `
  -Arguments @{ CommandLine = '"' + $exe + '" --quit' } | Out-Null
Start-Sleep -Milliseconds 600
Get-Process qlrust -ErrorAction SilentlyContinue | ForEach-Object { $_.Kill() }
```
顺带：`$_.WaitForExit(3000)` **返回 bool**，不加 `[void]` 会往脚本输出里漏一个 `True`。

### #43 PowerShell 变量名大小写不敏感：`$AppKey` 被 `$appKey` 覆盖
脚本顶部定义 `$AppKey = 'HKCU:\Software\QLRust'`，后面又写
`$appKey = 'HKCU:\Software\Classes\Applications\qlrust.exe'` —— 这是**同一个变量**，
于是 Capabilities 全被写进了 `Applications\qlrust.exe\Capabilities` 下，
`HKCU:\Software\QLRust` 根本不存在，而脚本一路无错退出（因为每一步都"成功"了）。
命名要有区分度：`$RustKey` / `$appsKey` / `$uninstallKey`。

### #44 写注册表**默认值**必须用 `Set-Item`，不能 `New-ItemProperty -Name '(Default)'`
`New-ItemProperty -Path $k -Name '(Default)' -Value x` 会造出一个名字**字面叫 `(Default)`** 的值，
资源管理器读的是真正的默认值 —— 关联看起来注册成功了，双击却打不开。
默认值只能 `Set-Item -Path $k -Value x`；同理 `DefaultIcon` 这类子键要先 `New-Item` 建出来再 `Set-Item`。
### #45 基准脚本的两个"看不见的失败"：RENDER 行带 `ink=`、同文件二次投递是 Toggle

`scripts/bench.ps1` 曾经每轮都 `FAIL(无 RENDER)`，根因不是渲染坏了，而是正则漏了字段：

```text
RENDER gen=1 bitmap 1189x669 bytes=3181764 ink=198243 elapsed=28ms (请求 1190x748)
                              ^^^^^^^^^^^^^^^ 旧正则 bytes=(\d+) elapsed= 在这里匹配不上
```

正解：`RENDER gen=(\d+) (\w+).*?bytes=(\d+).*?elapsed=(\d+)ms`（用 `.*?` 跳过中间的 `ink=`）。

第二个坑：**对同一个文件再投一次是 Toggle（关窗）**，所以"第二轮冷却"永远等不到新 RENDER。
每轮先 `--hide`（或换文件）再投递。

### #46 PowerShell 参数名与局部变量大小写不敏感，会撞车

`param([switch]$Matrix)` 里再写 `$matrix = ...` 会报
`无法将值"System.String"转换为类型"System.Management.Automation.SwitchParameter"` ——
因为 `$matrix` 就是 `$Matrix`。改名（`$matrixDir`）即可。同理 `$AppKey`/`$appKey` 会互相覆盖且**脚本仍显示成功**。

### #47 测试夹具不能用 Windows 设备名

`nul.txt`、`con.txt`、`aux.txt`、`prn.txt` 是保留设备名：写进去等于丢进黑洞，读回 0 字节，
测试就会以"渲染空文件"的诡异方式失败。夹具名加前缀（`tbody_...`）或干脆不用这些名字。

### #48 带 BOM 的 UTF-16 是文本，不是二进制

PowerShell 的 `>` 重定向默认写 UTF-16LE+BOM，前 8 KB 里有一半是 `0x00`。
只按"含 NUL 即二进制"判会把整份日志判死。修法（两处都要）：

- `qlr-render/src/text.rs`：`looks_binary()` 开头 `if is_utf16_bom(bytes) { return false; }`
- `qlr-core/src/format.rs`：`looks_like_text()` 开头对 `FF FE` / `FE FF` 直接 `return true`

这个缺陷是 A6.7 的样本矩阵（不是单测）抓到的——所以**样本矩阵要真的喂真实文件**。

### #49 `WM_ERASEBKGND => 1` 等于承诺"我自己刷底"，不刷就是文字重影

`wndproc` 里返回 `LRESULT(1)` 表示背景已被处理，系统不再擦；而 `invalidate()` 走的是
`InvalidateRect(Some(hwnd), None, false)`（`bErase=false`），即"也别擦"。两条加起来 = 谁也不刷底。
再叠加 `SetBkMode(hdc, TRANSPARENT)`（只画字不刷底），滚动后**旧字形留在客户区**，
两屏文字叠成一片糊 —— 1.0.1 用户报的"翻页逻辑不对"的真身。

修法（`crates/qlr-platform/src/window.rs`）：

- `paint_styled` 每次先 `FillRect(hdc, &rc, CreateSolidBrush(COLORREF(TEXT_BACKGROUND_RGB)))`，用完 `DeleteObject`；
- 行起点从"内容垂直居中"改成 `rc.top + TEXT_PADDING_TOP` —— 居中会让行数一变整屏上下平移，看起来像错位。

判据：滚动一圈再滚回来，截图 `Ink` 像素必须**精确回到原值**（有残留就会单调累加）。

### #50 `WM_QLR_WHEEL` 绕过文本滚动 = 滚轮把文本当图片缩放

钩子把滚轮投成 `WM_QLR_WHEEL`，`wndproc` 曾经直接 `wheel_zoom(delta)`，
而 `Action::Wheel`（CLI `--wheel`）走的是"先 `scroll_text_by`，滚不动才缩放"。
两条入口必须汇到同一个 `fn on_wheel(delta)`，否则**鼠标滚轮在文本上会触发重渲染缩放**，
用户看到"画面错乱/翻页逻辑不对"，自动化却测不出来（测的是 CLI 那条正确路径）。

同理：`show_preview` 开头要把 `zoom`/`pan` 复位，否则下一个文件继承上一个文件的裁剪。

### #51 安装器"抢打开方式"的三个事实

1. **`HKCU\Software\Classes\.<ext>` 的默认值优先于 HKLM**：实测 `HKLM\.md=Nutstore.LightApp.md`、
   `HKCU\.md=Markdown`，`reg query HKCR\.md /ve` 给出 `Markdown`。往 HKCU 写扩展名默认值
   = 直接改掉用户的双击行为（ShellExecute 实测会拉起 qlrust 并渲染预览）。
2. **"只是登记候选"也会变成抢**：无条件写 ProgID + `Applications\<exe>` + 每个扩展名的
   `OpenWithProgids`，等于把自己塞进所有格式的"打开方式"候选；UserChoice 一旦失守就是唯一落点。
   正确做法：默认**零关联**；要登记候选必须显式 `-Associate`；写默认值必须 `-Associate -Default` 且先备份。
3. **PS 5.1 按 ANSI/GBK 读脚本文件**：`uninstall.ps1` 存成 UTF-8 **无 BOM** 时，
   由 `powershell.exe`（5.1）解析会报 `parseErrors=5`，控制面板"卸载"按钮点下去直接坏。
   凡被 5.1 调用的脚本一律 **UTF-8 with BOM**，命令行还得带 `-ExecutionPolicy Bypass`（本机默认 Restricted）。

补充：`Remove-ItemProperty -Name '(Default)'` 在 PS 5.1 / 7.6 下**都不生效**，
删默认值只能 `Microsoft.Win32.Registry.CurrentUser.OpenSubKey($path, $true).DeleteValue('', $false)`。

## 52. 弹簧式"套尺寸"必须一次算 scale，不能宽高各夹一次

`centered_rect_px` 曾经这么写：`w = min(w, work*0.92); h = min(h, work*0.92)`。
竖图 1279×1706 于是变成 1279×956 —— **宽高比被悄悄改掉**，预览位图却是等比适配的 717×956，
两边各留 260 px 黑边。用户看到的就是"窗口没自适应"。

规矩：任何"塞进某个框"的尺寸计算，先算一个 `scale = min(fit_w, fit_h, 1.0)`，再 `w*scale / h*scale`。
单纯 clamp 每个分量只有在**容器与内容同比例**时才等价，别赌。

## 53. 缩放的窗口跟手：基准是"开窗尺寸"，不是"原始像素"

`zoom = 1.0` 的语义是"适应窗口"。如果把自然尺寸当基准，
0.8× 对一张竖图仍然超过工作区 → 窗口原地不动、位图变大 → 用户觉得"缩小反而被裁了"。

正确做法：开窗时把实际窗口尺寸存进 `State.base`，缩放一律 `base × zoom`，
再等比夹进工作区；位图按同一个 `base × zoom` 渲染，于是"窗口顶满工作区 + 位图更大"天然等于可平移的放大。

## 54. 用户给的样本文件可能含隐私信息，别拿它生成演示截图

一次为了验证"黑边"给用户发来的图截了屏，那张图是身份证照片，截图直接落进工作目录。
之后一律：演示/截图用**自己生成的合成夹具**；用户文件只用来读日志与尺寸，不截屏、不复制、不 present。
## 55. 鼠标语义只有一处真相：左键=拖、右键=缩、滚轮=滚

用户 1.0.3 的明确要求：左键专用于拖动、PDF 用滚轮上下滑动。
落地时要防三种"半改"：

1. **入口分裂**：滚轮有两条入口（`WM_QLR_WHEEL`（钩子转投）与 CLI `--wheel`），
   任何一条直连具体动作就会出现"鼠标滚和脚本滚行为不一样"。两条都只调 `on_wheel`。
2. **动作重叠**：滚轮在"能滚"和"不能滚"时语义不同（PDF 页内滑动 vs 翻页、图片缩放 vs 滑动），
   所以 `scroll_page_by` 返回 `(是否真的动了, 是否顶到边界)` 两个布尔，
   由 `on_wheel` 统一决定收尾，别在滚轮里塞 if-else 森林。
3. **残留开关**：删掉"点一下关闭"时要把 `Config.hide_on_click` 一起删。
   留着不读的字段 = 用户改了配置却毫无效果，比没有这个选项更糟
   （`#[serde(default)]` 没开 `deny_unknown_fields`，老 config.json 里多出来的键会被忽略，不影响启动）。

拖拽滚动的**像素零头必须攒**：`LINE_HEIGHT = 24`，鼠标每帧只动 3–5 px，
逐帧取整会让慢速拖动完全不动（像"拖不动"）。`pixels_to_rows(&mut residual, px)` 就是干这个的，
且它是纯函数，可以直接单测。

## 56. 忘了 import 的 `WM_*` 常量会变成"绑定模式"，不是编译错误

`WM_RBUTTONDOWN` 漏在 `use windows::Win32::UI::WindowsAndMessaging::{...}` 之外时，
`match msg { WM_RBUTTONDOWN => ... }` 里它被当成**新的绑定变量**，于是这一臂匹配一切，
后面所有臂都成 `unreachable pattern`——`cargo build` 只给警告，行为却是"右键按下之后什么都不响应"。

症状记法：`match` 里出现 `unreachable pattern` 警告 + 后续分支全失效 = 先查 import。
同一类坑：`windows-rs` 的常量类型是 `u32`/newtype，不是 `const` 就一律当变量。

## 57. 拖动"内容"会频闪，拖动"窗口"不会

预览大图时把 `pan` 当成拖动目标，每帧都要 `InvalidateRect` + 重画一整张位图
（4K 一帧十几毫秒），桌面合成器在**半成品**上取一帧，用户看到的就是频闪。
`SetWindowPos(SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE)` 挪窗口不产生任何重绘，
合成器搬现成的表面，成本接近 0（`window::move_by`）。

判据：拖动期间日志里应该**没有** `RENDER`/`PAINT`/`PAN`，但有 `MOVE (dx, dy)`。

## 58. 自己在挪窗口时，`WM_MOUSEMOVE` 的 client 坐标不能用

拖动时若用 client 坐标算位移，会与"窗口自己也在移动"互相抵消：鼠标走 90 px，
窗口只走 27 px（约 1/3），手感是"拖不动"。因为每挪一次窗口，下一次 client 坐标
就退回原点附近（`client = 屏幕坐标 - 窗口原点`，两者同向变化）。

正确做法：拖动一律取 `GetCursorPos()` 的**屏幕坐标**（`window::cursor_pos()`），
`WM_MOUSEMOVE` 的 `lparam` 只在拿不到光标位置时兜底。

同一类坑还有**右键拖动缩放**：缩放会改窗口位置（居中），锚点若用 client y 会漂，
表现为来回抖（`ZOOM 1.05 -> 0.87 -> 1.17 -> 0.91 …`）；锚点改用屏幕 y 后单调
`1.00 -> 1.05 -> 1.10 … -> 1.75`。
## 59. 缩放要用"窗口自身中心"当锚点，不能按工作区重新摆

`zoomed_rect(work, …)` 每次都把窗口摆到工作区中央：用户把窗口拖到右下角再缩放，
窗口会瞬移回中间——这就是"移动后再缩放，会回到初始打开的位置"。

正确做法：`resize_around_center(hwnd, w, h)` 先 `GetWindowRect` 取当前中心，
以中心摆新尺寸，再用 `clamp_into(&mut rc, work)` 只挪位置（尺寸不变）推回工作区。
判据：拖动后缩放，前后窗口中心偏移应 ≤ 1 px（含取整）。

## 60. 拖动期间不要重渲染，用"拉伸旧帧"顶住

PDF 右键拖动缩放，若每帧 `request_render`（pdfium 一页几十毫秒），鼠标一动画面就断续。
拆成两段：拖动期间只改窗口 + `StretchDIBits` 把**旧位图**拉伸铺满（`paint_bitmap_scaled`），
松手（`on_zoom_drag_end`）按最终尺寸补一次渲染。

判据：一次拖动 10+ 个 `ZOOM` 帧，`RENDER gen=` 只应出现 1 次，且日志里有
`（预览，松手再渲染）` 与 `ZOOM-DRAG 松手补渲染`。新帧到位后必须把 `zoom_preview`
复位（在 `on_render_done` 里做），否则窗口会一直显示放大的糊图。

## 61. 行号栏宽度必须是常数

行号宽度若按"当前最大行号的位数"动态算，折行量宽度和 `DrawTextW` 实际绘制会各自
量一遍，两边不一致 → 折了行的文本又被 `DT_END_ELLIPSIS` 吃掉（同 #38）。
固定 `TEXT_GUTTER = 64`（4 位数行号 + 右对齐余量），量宽与绘制都减同一个常数。
折行续行 `number = None`，只有源文件第一行带行号。

## 62. 去 Markdown 行内标记：三条否决条件 + 转义要先锁

朴素"找成对分隔符删掉"会毁掉普通文本：`2 * 3 * 4` 变 `2  3  4`、`snake_case_name`
变 `snakecasename`。`strip_pairs` 必须否决：中间为空或两头有空格、开头紧贴 ASCII
字母数字、结尾紧贴 ASCII 字母数字。注意 **CJK 不能算"词内"**——Rust 的
`char::is_alphanumeric()` 对汉字返回 true，直接用它会让 `这是**粗**体` 永远去不掉
（判据用 `is_ascii_alphanumeric()`）。

转义（`\*星号\*`）必须在**任何**去标记之前锁起来（换成私有区 `U+E000 + code`，
最后统一还原），否则反斜杠被当成成对分隔符的边界吃掉。

HTML 标签判定同理不能只看"有 `<` 有 `>`"：`#include <stdio.h>` 会被吃掉后半截。
要求标签名 `[A-Za-z][A-Za-z0-9-]*` 且紧跟 `>` 或空白。

## 63. 位图比客户区少 1 px 时**拉伸铺满**，不要居中——否则就是一条深色细线

渲染器按内容宽高比取整，出来的位图经常比客户区少 1 px（实测 `开窗尺寸 734x1040` 却是
`PAINT bitmap 734x1039 client=734x1040`）。`paint_bitmap_at` 原来在"位图不大于客户区"时
居中绘制（`dx = rc.left + (cw - bw) / 2`，整数除法再丢 1 px），于是边缘留下一条
`#1A1A1A` 的细线——用户反复反馈的"pdf 缩小后有一点黑边"就是它，而且**1 px 的线在不同缩放下
出现在不同边**，看起来像"到处都有黑边"。

规矩：**位图不大于客户区时，直接把目标矩形设成整个客户区**（`dw = cw, dh = ch`，源矩形仍是
整张位图）。差 1–2 px 的拉伸肉眼不可见，线彻底没了。判断标准：截图取窗口最后一行/最后一列，
底色像素必须为 0（`_bench/verify-lens.ps1` 的 A/E 组）。

顺带：缩放预览帧（`paint_bitmap_scaled`）也一律铺满，别等比留白——预览帧本来就是临时的，
露一圈底色比比例略差更难看。

## 64. 放大镜放大的是**屏幕像素**，不是重新渲染

用户要"看素材里某处细节"时，最好的做法不是把窗口/文档放大（那要拖窗口、拖画面、重光栅化），
而是按住 Shift 时在光标处叠一个镜头：`StretchBlt` 把光标周围 `2R/zoom` 见方的一块画面放大到
内存位图，再 `BitBlt` 贴回，最后 `FrameRect` 画两圈边框（亮 `#E0E0E0` + 暗 `#404040`）。

好处：与文件大小、页数、缩放倍数、平移位置全部无关，代价是一次 GDI 搬运（<1 ms）；
对 PDF/图片/文本统一有效，也不用担心"放大镜里的图和主视图对不齐"。

实现注意：
- 必须**在内容绘制之后**、同一次 `WM_PAINT` 内调用；用 `GetDC`/`ReleaseDC` 而不是第二次
  `BeginPaint`（同一消息里 `BeginPaint` 两次会把更新区搞乱）。
- 镜头位置要夹在客户区内（`x.clamp(r, cw - r)`），否则靠边时被裁掉一半。
- Shift 状态用 `GetAsyncKeyState(VK_SHIFT)` 现查，别自己跟踪按键——焦点在别的窗口时按键消息
  根本收不到，但 `GetAsyncKeyState` 照样准。
- Shift + 滚轮要**先**被放大镜吃掉（调倍数），否则用户在看细节时滚轮会顺手把文档滚走。
## 65. 新增"启动时就该看见效果"的命令，必须同步 `startup_shows`

`run()` 里判断"启动后是否立刻处理首个请求"的条件原来只有
`request.path.is_some() || matches!(request.action, Action::Show)`，
其它请求会被当成"纯后台启动"直接丢掉。

加 `--settings` 时就踩了：日志里一条错误都没有，设置窗就是不出来（`settings::open` 压根没被调用）。
修法：把 `Action::Settings` 加进那个 `matches!`。教训——**新增任何"启动即生效"的命令，
不仅要加 CLI 分支，还要检查启动期的这一处白名单**；这类"静默丢弃"最难查，因为没有任何报错。

## 66. 设置页的行高常量必须"绘制"和"命中测试"共用一份

设置窗是自绘的：`paint_settings` 按 `SETTINGS_PADDING_TOP + i * SETTINGS_ROW_HEIGHT` 画，
点击也按同一个公式反算行号。两处各写一份数字，改一处就会"点一行改到邻居那一行"，
而且**看起来一切正常**（点得中、也能改键，只是改错了键）。

所以常量放 `qlr-platform/src/window.rs`（`SETTINGS_ROW_HEIGHT` / `SETTINGS_PADDING_TOP` /
`SETTINGS_PADDING_X`），上屏与 `settings.rs` 的 `layout()` / `on_click()` 都引用它；
再补一条单测把"layout 行数 × 行高 + 上下留白 = 客户区高度"钉住。

## 67. 键盘钩子回调里禁止写日志（300 ms 预算）

`WH_KEYBOARD_LL` 的回调累计超时（`LowLevelHooksTimeout`，默认 300 ms）会被系统静默摘钩。
日志一次要加锁、格式化、写文件，赶上磁盘抖动就可能吃掉几十毫秒。

所以钩子回调里只做"比较 vk + `PostMessageW`"，日志一律放在：
- `hotkey::set_bindings()` 里打一次全量 `按键绑定：打开 / 关闭预览=Space …`（配置变了才有）；
- 宿主线程收到 `WM_QLR_HOTKEY` 之后用 info 打 `HOTKEY vk=0x20`。

## 68. `ipc::Action` 与 `keymap::Action` 同名，必须写全路径

1.0.7 起有了两套 `Action`：`qlr_core::ipc::Action`（进程间命令：Show/Hide/Toggle/Quit/Settings…）
与 `qlr_core::keymap::Action`（可配置的按键动作：Toggle/Close/PageNext…）。名字有重叠。

在 `qlr-app` 里 `use qlr_core::ipc::Action;` 之后写 `Action::Toggle` 会被解析成 ipc 的那个，
编译错是 `expected qlr_core::keymap::Action, found qlr_core::ipc::Action`。
修法：涉及按键动作的地方一律 `qlr_core::keymap::Action::Toggle` 全路径（`HotkeyProbe` 分支就是这么修的）。

同时注意**语义不要串**：`ipc::Action::Toggle` 是"切换预览窗"，`keymap::Action::Toggle` 是
"打开/关闭预览这个按键动作"，两者靠 `on_hotkey(vk)` 串起来，别把 `Action::Settings` 也做成按键动作。

## 69. 改主题色会废掉所有"判色"脚本

1.0.8 把底色从 `#1A1A1A` 改成白色后，`_bench/shot.ps1` 的 `Ink`（"偏离旧底色 0x1A1A1A 的像素数"）
在白底上整屏都算墨，数字从 1.5 万跳到 32 万，指标彻底失效。

规矩：判色脚本**先取一个已知背景像素**（例如 `(0,0)`）当基准，或者干脆只信日志
（`STYLED` / `CODE` / `PAINT`）；主题一改就要重算阈值，不要把绝对像素数写死在断言里。

## 70. 行数预算必须扣掉顶部 padding

`visible_rows` 原来是 `(client_h / LINE_HEIGHT) - 1`（减 1 行给状态行），但 `paint_styled`
是从 `rc.top + TEXT_PADDING_TOP(16)` 开始画的：748 高的窗口算出 30 行正文 + 1 行状态行 =
16 + 31×24 = 760 > 748 → **最后一行被窗口裁掉**。

正确写法：`usable = h - TEXT_PADDING_TOP; rows = (usable / LINE_HEIGHT) - 1`。
判据：截图看"状态行 + 正文"最后一行是否完整（`_bench/v108-text.png` 第 29 行齐全）。

## 71. 新内容没到之前不要 `invalidate`

PDF 翻页曾经这样写：改占位文字 → `set_lines` → `invalidate` → `request_render`。
结果是"文字占位闪一下 → 新页再闪一下"，用户眼里就是**频闪**。

正确做法：只 `request_render`，**旧帧留在屏幕上**，等 `on_render_done` 里新内容到位再刷。
判据：一次翻页日志里只有 `PAGE n/m` + 一帧 `RENDER`，没有中间的占位写入。

## 72. 缩小态的清晰度靠"超采样再缩"

PDF 直接按目标像素渲染时，字符边缘只有 1 px 可用，缩到 0.6× 就糊。
做法：目标尺寸 ×2 渲染（`SUPERSAMPLE=2.0`，加 24 MP 预算上限兜住内存），再用
`image::imageops::thumbnail` 盒式缩到目标——代价是一次多渲染 4 倍像素（本例 3 ms → 8 ms），换肉眼可见的清晰度。

规矩：超采样要有**像素预算**（`SUPERSAMPLE_BUDGET`），不然一张大页面就能吃掉几百 MB。

## 73. 一行多段文字要自己量宽推进

`DrawTextW` 一次只画一段，代码着色的"关键字 + 空格 + 字符串"必须逐段画：
先用 `GetTextExtentPoint32W` 量这一段的宽，`rect.left += width` 再画下一段，且**每段都要
`SelectObject` 同一个字体**（字体不同宽度就对不上，段之间会出现缝隙）。

`paint_doc` 就是这么实现的；`paint_styled` 退化成"每行一段"的特例，保证只有一条绘制路径。

## 74. 热键前台判定要包含"自己进程的窗口"

`WH_KEYBOARD_LL` 里"只有资源管理器类窗口在前台才响应空格"这条旧规则，会漏掉
**我们自己的设置窗**（打开设置窗后按空格完全没反应）。

修法：`foreground_is_ours()`（`GetWindowThreadProcessId(GetForegroundWindow()) == GetCurrentProcessId()`）
与 shell 类白名单取**或**：`ours || shell`。同时白名单补上 `Shell_TrayWnd` /
`Shell_SecondaryTrayWnd` / `NotifyIconOverflowWindow` / `MultitaskingViewFrame`。


## 75. 窗口高度必须按"实际会画的行数"算

设置页 `create()` 用 `layout().len()` 算客户区高度，而 `paint()` 会多画一个空行 + 一行提示
（`+2` 行）。差的这两行不会报错、不会崩，只是**底部提示被客户区裁掉半截**——
看起来像"忘了画"，其实是算漏了。

规矩：绘制用多少行、窗口就开多高，两处从**同一个函数**取值（`settings::client_height()`），
并写一条单测把"放得下的行数 ≥ 要画的行数"钉住。同理适用于任何"动态行数 + 固定多一行页脚"的窗口。

## 76. 截自己的窗口用 PrintWindow，不要 CopyFromScreen

预览窗/设置窗**随时可能被别的程序盖住**（视频、浏览器、IDE）。`CopyFromScreen` 抓的是屏幕矩形，
于是截到的是别人的内容：既是错误截图，也是隐私事故（第一次重拍设置页就截到用户正在看的视频）。

正确做法：`PrintWindow(hwnd, hdc, PW_RENDERFULLCONTENT = 2)`，让窗口自己画到 DC 上，
与 z 序无关。脚本在 `_bench/shot3.ps1`；`_bench/shot2.ps1` 保留作"需要抓真实屏幕像素"的场合
（例如判断边框/底色，那时要的就是屏上像素）。
---

## 77. 叠层（目录栏 / 滚动条 / 放大镜）必须在正文之后画

预览窗一次 `WM_PAINT` 里只能有一个 `BeginPaint`；目录栏与滚动条用 `GetDC` 直接往窗口上画。
于是顺序被写死：**先正文（把整块内容区刷成不透明底色）→ 再叠层**。
反过来做，正文的 `FillRect`/`StretchDIBits` 会把叠层整块盖掉，表现为"目录和滚动条时有时无"。
`app.rs::paint_overlays()` 就是这条顺序的唯一出口，任何新叠层都加在它里面。

## 78. 绘制入口必须显式接"内容矩形"，不能自己算

`chrome_rects()` 是顶栏、按钮、页码框、内容区的**唯一真值**；`frame()` 在此之上再扣目录与滚动条。
1.0.9 踩过：`paint_styled` 内部把 content 传成 `None`，正文从 `x = 0` 起画，左半截被目录栏盖住。
症状很误导——"文字左边缺一块"看着像排版 bug，其实是绘制原点错了。
所以 `paint_doc`/`paint_styled`/`paint_text`/`paint_bitmap_at`/`paint_bitmap_scaled` 全部要求传 content 矩形。

## 79. 预览窗要收键盘输入，只能借钩子；钩子里的输入分支必须放在"过滤注入键"之前

预览窗是 `WS_EX_NOACTIVATE`，永远拿不到键盘焦点 —— 顶栏页码框要输入数字，
只能 `hotkey::set_capture_digits(true)` 让 `WH_KEYBOARD_LL` 把数字/退格/回车/Esc 转给宿主。
关键顺序：**先判输入模式，再过滤 `LLKHF_INJECTED`**。反过来的话，自动化注入的按键永远进不了输入态，
测试脚本 100% 失败而真人手按一切正常 —— 这种"只有测试坏"的 bug 最难查。

## 80. 目录高亮要跟着滚动走，不能只跟鼠标

只按悬停/点击高亮时，用户滚动正文，目录却指着别处。正确做法是
`selected = hovered.or_else(|| 最后一个 line <= scroll 的条目)`（`app.rs::toc_following()`），
悬停时优先显示悬停项，否则显示"当前读到哪一节"。目录项的 `line` 用**显示行下标**（`TextDoc::lines`），
点一下直接设 `scroll`，不需要任何二次映射。

## 81. 数字型设置项必须给范围提示

"图片初始缩放 %"这类行，用户敲 1000 时如果静默忽略，会以为功能坏了。
`settings.rs::confirm_image_zoom()` 统一 `clamp(10, 400)` 并在越界时回一句
`1000% 超出范围，已改成 400%`；解析失败回 `请输入 10–400 之间的数字`（同一套写进 `message` 行）。

## 82. `MessageBeep` 不在 windows crate 里

想"叮"一声时本能写 `MessageBeep(MB_ICONWARNING)`，但 `windows` 0.62 的
`Win32::UI::WindowsAndMessaging` **没有导出 `MessageBeep`**（编译期直接报找不到）。
改用 `Win32::Media::Audio::PlaySoundW` 播系统别名：

```rust
PlaySoundW(PCWSTR(wide("SystemHand").as_ptr()), None, SND_ALIAS | SND_ASYNC | SND_NODEFAULT | SND_SYSTEM);
```

代价：`Cargo.toml` 的 windows features 要多开 `"Win32_Media_Audio"`。`window::beep()` 里也只在
`PlaySoundW` 失败时静默（声音不是功能，不该因为编译进不去的 API 让预览流程失败）。

## 83. 无边框弹窗没有系统投影，`CS_DROPSHADOW` 不可用

预览窗是 `WS_POPUP` + 无边框 + `WS_EX_TOOLWINDOW`，`WM_NCCALCSIZE` 返 0 之后**四条边都是客户区**：
既没有 `DwmExtendFrameIntoClientArea` 的暗边，也不会画系统投影。
试过的两条死路：

1. `SetClassLongPtrW(hwnd, GCL_STYLE, style | CS_DROPSHADOW.0)` —— 组合合法、调用成功，本机**完全看不见**。
2. 客户区里套几圈 `FrameRect`（#909090 / #C4C4C4 / #ECECEC）—— 在白底程序旁边像"塑料卡纸"，用户一眼否掉。

正解：**独立的分层窗口**做投影，`z 序` 压在预览窗之下：

- `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE`；
- 每像素 `alpha = SHADOW_ALPHA * t^2`（`t` = 到内框的归一化距离，平方让边缘更快消失）；
- `UpdateLayeredWindow` 上屏，窗口尺寸 = 目标窗口外扩 `SHADOW_MARGIN`；
- 预览窗移动/改尺寸时 `sync_shadow()` 跟一次。

客户区自己再画 **1 px `#C8C8C8` 发丝边**负责"边界清楚"，投影负责"立体感"，两件事分开做。

## 84. `UpdateLayeredWindow` 的 `pptDst` 必须传 `None`

`UpdateLayeredWindow` 的 `pptDst` 是"新的屏幕位置"。传 `Some(&POINT::default())` 会**把窗口挪到 (0,0)**，
症状是"预览窗好好的、阴影跑到屏幕左上角"。位置交给 `SetWindowPos` 摆好之后，`pptDst` 一律传 `None`。

## 85. 空格的"收起"有两条路径，拦一条会漏

1.0.10 加"阻止关闭"锁时只在 `toggle()` 里拦，实测 6b/6c 两条断言当场挂：
热键回调 `on_hotkey()` 在"当前没有选中项"时会**直接收起**，根本不走 `toggle()`。
规则：所有"收起预览"的入口必须收敛到同一个判定 `close_blocked(how)`——
`toggle` 的同文件分支、`toggle` 的空选中项分支、`on_hotkey` 的空选中项分支、`WM_QLR_ESCAPE`。
`×` 按钮与托盘「退出」是**显式意图**，不拦。

## 86. 跟随选中项的轮询要先问前台类名，再动 COM

`selection::current_selection()` 会走 COM（`IShellWindows` / `FindWindowSW`，桌面还要 3 轮重试），
400 ms 一次地调会在后台把 CPU 拉起来。正解是两级短路：

```rust
if !hotkey::foreground_is_shell() { return; }          // 前台不是资源管理器/桌面 → 直接跳过
let picked = selection::current_selection().first();   // 才动 COM
```

并且跟随中遇到**不支持的文件要静默跳过**（不响铃）：用户按住方向键扫过一串文件时，
每个不支持的文件都"叮"一次是不能接受的；响铃只在**用户主动按空格**时给。

## 87. 拖边改尺寸只认"按下时原始矩形 + 累计位移"

增量累加（`每次 dx` 直接叠加到当前矩形）看起来更自然，但会和"最小尺寸 clamp"打架：
把左边一路拖过右边之后，每次移动都先被 clamp 掉一部分，累计误差不可逆，窗口会"粘住"。
固定基准的算法：

```rust
let (x0, y0, r0) = state.resize_from;  // 按下时的鼠标屏幕坐标 + 窗口矩形
if edges.left   { r.left   = (r0.left   + (x - x0)).min(r.right - MIN_W); }
if edges.right  { r.right  = (r0.right  + (x - x0)).max(r.left  + MIN_W); }
// top/bottom 同理，最后 resize_to(hwnd, r.left, r.top, w, h)
```

松手（`finish_resize`）后必须把 `base` 更新成新客户区、`zoom` 复位 1.0，再 `request_render`——
否则位图会按旧基准缩放，看起来"改了窗口但内容比例还是老的"。

## 88. 热区要用"客户区坐标"，光标要用"屏幕坐标"

`WM_SETCURSOR` 与 `WM_LBUTTONDOWN` 里判热区用的是客户区坐标，但 `GetCursorPos()` 是屏幕坐标，
必须 `- window_rect().left/top` 换算；反向地，`resize_to` 只吃屏幕坐标。
混用一次的典型症状是"要精准点在边上才变指针，差十几像素就失效"。
## 89. 顶栏里"谁挨着谁"只能由 `chrome_rects()` 一处决定

1.0.11 只是把页码框和「锁」换个位置，却差点改三处：绘制顺序、点击命中、悬停判定。
规矩是**所有矩形只在 `chrome_rects()` 里算一次**，其余函数全部按名字取（`rects.pin` / `rects.page` / …）；
`paint_chrome` 的按钮循环只关心"画哪个字形"，不关心"排第几"。
换顺序时同步改两件事就够：`chrome_rects()` 里谁减谁的宽度，以及那条钉住顺序的单测断言。
## 90. 浮层窗口必须"谁改尺寸谁同步"，再加一道每帧兜底

投影是独立的分层窗口，它的矩形**不会**跟着预览窗自动变。1.0.10 只在显示/拖动/拖边时同步，
漏了"缩放改窗口尺寸"这条路径，于是缩小图片后外面留了一圈比窗口大的灰框
（中心不变 ⇒ 四边均匀露出，看起来像"多了一层边框"）。

两条规矩：

1. **改窗口矩形的函数，末尾立刻 `sync_shadow()`**（`set_zoom_to` / `finish_resize` / `move_window_by` / `show_preview`）；
2. 绘制入口（`repaint`）再兜一次。`sync_shadow` 内部按尺寸缓存 alpha 位图，
   尺寸没变时只是一次 `SetWindowPos`，代价可以忽略——"每帧调一次"比"记得在十个地方调"可靠得多。

## 91. 窗口位置记忆：只记位置不记尺寸，落盘只放在显隐边界上

用户量好一个窗口位置，最忌讳"下次打开又跑回屏幕中央"。三条要点：

1. **只记位置**。尺寸是内容的函数（原图 50% / 880 阅读宽度 / 用户拖边结果），
   位置是人的习惯。两者混在一起存，换文件时必然互相打架。
2. **"窗口开着"时一律以它现在的位置为准**，不要读存档——跟随选中项换文件、
   连按空格换内容都会重新走 `show_preview`，读存档就会跳回旧位置。
3. **落盘只放在显隐边界**（收起 / 退出）。拖动每帧调 `remember_position()` 只更新内存，
   写盘会变成拖一次写一次 JSON。
4. 存档坐标必须过 `clamp_into`：换显示器后记下的点可能在工作区外，直接用等于把窗口弄丢。

## 92. 尺寸记忆要按格式分开，且只记"用户改过"的

一条可以被用户当 bug 报的坑：窗口尺寸只记一份。

- 文本 880 px 阅读版式、图片原图 50%、PDF 页宽比例，三者差一个数量级；
  混成一份记忆就会看到"1200 宽的窗口里嵌着 631 宽的页面"（硬留白），
  或者"小图外面一圈白边"。**按 `format::Kind::name()` 分开记**。
- 只记"用户改过"的尺寸（拖边 / 缩放）。把"首次按内容算出来的尺寸"也记下来，
  一张 8000×6000 的大图就会把之后所有图片的初始尺寸顶到工作区上限，
  "按原图 50% 开窗"的设定等于失效。
- 读配置时丢掉手改坏的值（0、负数、比任何显示器都大），并夹回工作区。

## 93. 投机预渲染：闸门按"便宜的先过"排序，槽位只要一个

- 闸门顺序就是开销顺序：先 `foreground_is_shell()`（一次 `GetClassNameW`），
  再 `current_selection()`（COM 枚举，几毫秒），最后才是渲染。反过来写，
  一个只把程序挂在托盘的用户会每分钟被我们摸几十次 COM。
- **只留一个槽**：用户选中的永远只有一个文件；N 个槽 = 把内存花在不会被看到的文件上。
- **算前算后各查一次"还要不要"**：预热是几十毫秒的活，这期间用户完全可能已经换文件了；
  算完再查一次，过时就丢弃（日志 `PREWARM 丢弃（算完已过时）`）。
- **尺寸算术必须与开窗共用**（`window_size_for()`）：两边各写一份，只要有一处漂移几像素，
  预热结果就永远打不中槽 —— 而且失败得很安静（只是"没变快"）。
- 预热的对象只能是**内置查看器**：插件是第三方代码，不能在用户没要求时执行。

## 94. 日志级别断言要用 `-cmatch`：PowerShell 的 `-match` 不区分大小写

- 配置行里有 `prewarm=off`，用 `-match 'PREWARM'` 判断"关档后没有预热日志"会**假失败**。
- 同类坑：`-match 'HIDE'` 会被 `hide_on_click` 之类的小写词命中。凡是拿大写标记词做断言，一律 `-cnotmatch`。

## 95. 测量脚本不要删正在写入的日志，改用"行号切片"

- 宿主持着日志句柄，`Remove-Item` 之后它继续往**已删除的 inode** 里写，脚本只会读到空文件，
  所有测量都变成 `-1` / "数据不足"，看起来像功能没生效。
- 正确做法：`$mark = (Get-Content $log).Count`，之后只看 `$lines[$mark..]`。
  要清空就让宿主先退出（`--quit` / `Kill`），再删文件、再启动。

## 96. 枚举型配置项要自定义反序列化，别让一个拼写错误打回整份配置

- `#[serde(deserialize_with = ...)]` + 手动 match：未知值落到默认档并 `log_warn`，
  而不是让 `Config::load` 整体失败、静默把日志级别/窗口尺寸/快捷键全打回默认。
- 判据：手改 `config.json` 写一个 `"prewarm": "瞎写"`，其余字段必须原样保留（有单测钉住）。
## 97. `MessageBeep` 不在 windows crate 里，崩溃/提示音走 `PlaySoundW`

- `windows` crate 的 `Win32::UI::WindowsAndMessaging` 没有 `MessageBeep`（不在它的元数据里）。
- 用 `PlaySoundW(PCWSTR(w!("SystemHand")), None, SND_ALIAS | SND_ASYNC | SND_NODEFAULT | SND_SYSTEM)`
  播系统"严重错误"音；要在 `Cargo.toml` 的 windows features 里加 `Win32_Media_Audio`。
- `SND_SYSTEM` 让声音走系统通知会话，不会因为前台静音而丢。

## 98. `MiniDumpWriteDump` 的 cfg 门要三个 feature 一起加

- 只加 `Win32_System_Diagnostics_Debug` 会报
  `no MiniDumpWriteDump in Win32::System::Diagnostics::Debug`（"found an item that was configured out"）。
- 必须同时有 `Win32_Storage_FileSystem` + `Win32_System_Kernel` + `Win32_System_Memory`
  （它的 cfg 是 `all(feature = "Win32_Storage_FileSystem", feature = "Win32_System_Kernel", feature = "Win32_System_Memory")`）。

## 99. 崩溃报告的文件名必须自己消毒

- `utc_timestamp()` 含 `:`，Windows 文件名非法 → `File::create` 直接失败，
  而异常路径里没人看返回值，于是**静默**没有报告（第一次验证时日志里 `dump=false` 就是这个）。
- 修法：白名单 `0-9 A-Z a-z - _`，其余一律换成 `-`；写失败要能在日志里看出来。

## 100. 只能有一个 panic hook：后装的会覆盖先装的

- 曾经在 `run()` 里有一条"把 panic 写进日志"的简易 hook，启动早期又装了带 minidump 的那个，
  结果后者被前者覆盖（或者反过来，取决于顺序）——现象是"日志里有 PANIC，但 crashes 目录空着"。
- 规矩：全程序只有 `crash::install_hook()` 一处；它同时负责日志、txt 报告、minidump。
  想在日志里保留 `PANIC: ...` 文案就在这个 hook 里打。

## 101. `panic = "abort"` 的进程会把日志缓冲带走

- release profile 是 `abort`：hook 里直接写文件（`std::fs`）能落地，但**日志缓冲可能丢**。
  所以崩溃证据不要只依赖 `log_*!`，必须落一份独立文件。
- 同理，`std::panic::catch_unwind` 在 release 里救不回进程（只有 dev/测试档有效），
  写代码时别把它当"沙箱"用；真正隔离要等进程外插件。

## 102. WMI 起进程不继承 PowerShell 的环境变量

- `Invoke-CimMethod Win32_Process::Create` 用的是 WMI provider 的环境，不是调用者的。
  所以要注入测试用环境变量，必须借 `cmd.exe`：`cmd /c "set X=1 && exe ..."`。
- **cmd 的引号有两层坑**：① `set X=1 && ...` 会把 `&&` 前的空格算进值里（值变 `"1 "`），
  代码里读环境变量必须 `trim()`；② `cmd /c "脚本.cmd" "arg"` 这种"第一个参数带引号"的形式
  偶发什么都不做就退出，稳定写法是**双层引号**：`cmd /c ""脚本.cmd" "arg1" "arg2""`。
- 最稳的是把 "设变量 + 启动" 写进一个 `.cmd` 批处理，只传参数进去。

## 103. 自动化验证"进程崩溃"类行为，要先保证没有残留实例

- 单实例转发会让"新进程"只把请求丢给老宿主然后以 0 退出：
  脚本看到的"进程消失了"是转发造成的，不是崩溃；此时日志/崩溃目录都不会有你想要的东西。
- 规矩：`Stop-Qlr` 必须**等到进程真的消失**（轮询 `Get-Process` 直到空，而不是 `Kill` + 睡 400 ms），
  并加一条 `C0 启动前没有残留实例` 的断言。
- 另外 `panic = abort` 的进程只活几十毫秒，150 ms 的轮询会整个漏掉——观察窗口要 ≤ 50 ms，
  或者干脆只看"崩溃目录有没有东西"。

## 104. 长稳压测的内存判据：看私有字节，别只看 RSS

- Windows 堆在密集分配/释放后会保留已提交页面，RSS 涨几 MB 不等于泄漏
  （实测 12.9 → 18.0 MB，重启后回到 12.9）。
- 真泄漏先体现在 **GDI / USER / 句柄**（`GetGuiResources` + `HandleCount`）与**私有字节**上。
  三条一起判，再加一条"重启后回到基线"，并允许 +8 MB 的堆保留余量。
- `GetGuiResources` 要 `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`，
  且进程可能有"转发请求"的短暂双实例，取进程时要 `Sort-Object StartTime | Select-Object -First 1`。

## 105. 直接依赖某个解码库时，版本必须与 `image` 依赖的那个一致

- 为了"逐行解码"直接 `use png`，锁文件里已经有一份 `png`（`image` 拉的）。
  自己写一个不同的 major/minor 会编译出**两份** `png`：体积翻倍、类型不兼容、
  而且很难看出为什么"用了新 API 却报找不到"。
- 做法：`cargo tree -i png` 看现有版本，`Cargo.toml` 里写同一个大版本（本例 `png = "0.18"`），
  并在注释里写明"版本必须与 image 依赖的一致"。

## 106. `png` 的 `info.color_type` 是**文件里写的**色型，不是解码后的色型

- 设了 `Transformations::EXPAND` 之后，索引色/低位深会被展开成 RGB(A)，
  但 `reader.info().color_type` 仍然是 `Indexed` —— 照它算"每像素多少字节"会整体错位，
  表现在画面上是斜条纹 / 颜色乱掉。
- 正确做法：`let (out_color, _) = reader.output_color_type();` 用输出色型算通道数。
- png 0.18 的 `Reader::next_row()` 直接借出一行切片（`Row::data()`），不再需要自己备缓冲；
  它的编码器**不能生成隔行 PNG**（没有 `set_interlaced`），所以"隔行拒绝"这条分支只能靠真实文件验，单测覆盖不到。

## 107. 大图降采样：宁可"逐行边解边缩"，不要"整图解码再缩"

- `image` 的解码器是整图解码：48MP PNG = 192 MB 缓冲，最后只留 4 MB 的预览图。
- 自己按行解码 + 面积平均累加到目标尺寸，常驻内存只与**目标尺寸**有关（4 MB 量级）。
- 累加器只要一行的宽度：读一行 → 累加进"当前目标行" → 目标行变了就 `flush` 并清零，
  这样连"整张目标图的 f32 累加缓冲"都不用备（否则 1200×900×4×4B ≈ 17 MB）。
- 判据：`_bench/measure-bigpng.ps1` 里"峰值 Private 增量"必须落在目标画布量级（个位数 MB），
  而且耗时不能比整图解码更慢（省掉两趟大内存搬运后通常更快）。

## 108. 页缓存的 key 必须与"渲染请求"逐字段一致

- 现象：缓存写进去了、命中率却是 0。日志里写的是 `631x924`、读的是 `631x894`——差 30 px 正是顶栏高度。
- 根因：开窗时按**整窗尺寸**投渲染请求，翻页时按 `view_size()`（客户区 − 顶栏）投请求，两条路各算一次尺寸。
- 规矩：**同一份"要渲染多大"只能有一个算法**。1.3.0 把开窗的请求尺寸改成 `(宽, 高 − reserve)`，
  与 `view_size()` 对齐，缓存 key 才对得上（`pagecache::PageKey` 是精确匹配，差 1 px 就不命中）。
- 判据：`_bench/verify-pagecache.ps1` 的 C1/E2 两项——开窗后再翻回来必须命中。

## 109. 页缓存要 LRU + 字节上限，且"单页超限就不进缓存"

- 一页 631×893 BGRA 是 2.25 MB，随手翻十几页就是几十 MB；没有上限的话"看个 PDF 吃掉 200 MB"。
- 上限 64 MB、`VecDeque` 头最久未用、`get()` 命中后挪到队尾。
- **单页大于上限时直接不缓存**：否则一页就把别人全挤掉，命中率反而更差。
- 换文件时 `pagecache::clear()`：缓存是按路径记的，旧文档的页继续占内存没有意义。

## 110. 预取要跟着"预渲染档位"走，缓存建档不跟

- 相邻页预取属于投机行为，用户把预渲染设成"关"就该完全不动；
  **但"当前页算完就存进缓存"不算投机**（那一页本来就要算），所以关档后翻回去仍然命中。
- 判据：`verify-pagecache.ps1` 的 E1（关档后无 `PAGECACHE 预取`）与 E2（关档后仍命中）。
- 预取只做前后各一页：用户翻页是循序的，一页够用；整本预取会跟"真正要看的那一帧"抢 CPU。

## 111. GPU 呈现（D3D11）在同机上量不出收益，先不做

- 五期原计划 5a 是 D3D11 flip-model + 关垂直同步 + `ALLOW_TEARING`。
- 实测数据（1.3.0，i5-12400F 集显）：631×893 页面 `PAINT` 与 `RENDER` 同一毫秒完成（< 1 ms），
  993×559 位图同样 0–1 ms；4K 图 `PAINT` 从未触发过"≥8 ms 才打印"的慢路径。
- 结论：**GDI 的 `StretchDIBits` 在我们这个尺寸上已经不是瓶颈**，GPU 呈现最多省下 0–1 ms，
  却要新增 D3D 设备/交换链、处理"分层阴影窗口 + 截图（`PrintWindow`）可能变黑"的兼容问题。
- 记录在这里是为了留证据：将来若真做 4K/8K 大图平滑平移，再回头看这条。

---

## #112 上一帧缓存的 key 必须带文件戳

`FrameKey` = 路径 + **mtime** + **文件大小** + 页号 + 渲染尺寸 + 放大标志。
少任何一个都会出错：

- 少了文件戳：编辑器保存后重开，会拿旧位图冒充新内容（用户会以为"预览不刷新"）。
- 少了渲染尺寸/放大标志：缩放态与适应态互相命中，画面会被裁一刀。
- 少了页号：PDF 第 1 页顶替第 5 页。

**同一个坑踩第二次（1.8.3-fix）**：`PageCache` 的 `PageKey` 当初就少了文件戳，预览中文件被覆盖后
翻页/翻回会直接命中旧像素（`crates/qlr-app/src/pagecache.rs`）。现已补 `mtime`/`len`，并有单测
`stale_stamp_does_not_hit` 钉住。**结论：凡是"缓存到磁盘内容"的 key，都必须带文件戳**——
`FrameKey`、`PageKey`、以及将来任何解码/排版缓存，一个都不能少。

## #113 `with_state` 不可重入：闭包里别再调 `with_state`

`with_state` 底层是普通 `std::sync::Mutex`，**在闭包里再调一次就是自己锁自己**（死锁，不 panic）。
症状：进程卡在某一步，日志停在上一行，看起来像"启动卡住"。
规矩：**进闭包之前把要用的值全部取出来**，闭包里只做写入。

## #114 标记抓取（`extract_between`）的 open 标记不要自带 `>`

`extract_between` 的实现是"找到 open → 跳到它后面第一个 `>` → 取到 close 之前"。
所以 open 要写成 `<v`（不带 `>`）；写成 `<v>` 时，跳过的那个 `>` 是 `</v>` 的 `>`，
结果整段被吞掉——表现为"xlsx 里字符串有、数字没有"。

## #115 xlsx 的两种字符串存法都要管

- Excel 自己写：`xl/sharedStrings.xml`，单元格是 `<c t="s"><v>下标</v></c>`。
- openpyxl 等库默认写：**行内字符串** `<c t="inlineStr"><is><t>文字</t></is></c>`，**没有 sharedStrings.xml**。

只读共享字符串表 → openpyxl 生成的文件在插件里显示成一张空表。
另外单元格分隔要按 `<c`（不带空格）切：`<c>` 没有属性、`<c r="A1">` 有。

## #116 插件导出函数的 clippy 硬要求

`crate-type = ["cdylib"]` + `#[unsafe(no_mangle)] pub unsafe extern "C" fn` 时：
- 每个 `unsafe fn` 都要有 `# Safety` 文档段（`clippy::missing_safety_doc`，`-D warnings` 会拦）。
- GDI 的 `DeleteDC` / `DeleteObject` 返回 `BOOL` 且 `#[must_use]`，要 `let _ = ...`。
- 声明 `const _: QlrPluginRenderFn = qlr_plugin_render;` 把签名钉在编译期（改 ABI 时立刻报错）。

## #117 IPreviewHandler 必须给一块独立子宿主窗口

系统预览处理器拿到 `SetWindow(parent, rect)` 之后会 `GetClientRect(parent)` 自己铺满。
把**主窗口**交出去，它会把顶栏一起吃掉（症状：顶栏变白、标题消失）。
做法：`window::create_child_host(parent, rect)` 建一个 `WS_CHILD|WS_VISIBLE` 的子窗口，
只把这块区域交给处理器；窗口尺寸一变就 `move_child_host` + `SetRect`。

## #118 IPreviewHandler 的 DoPreview 必须等窗口可见

在隐藏窗口（`ShowWindow` 之前 / `SW_HIDE` 状态）上调用 `DoPreview()`，处理器会等一个
永远不会到来的激活消息，**UI 线程直接卡死**，任务管理器里是"未响应"的幽灵窗口，只能杀进程。
顺序必须是：`show_at(...)` → `open_shell_preview(...)` → `sync_shell_rect()`。

## #119 IInitializeWithFile 在 UI::Shell_PropertiesSystem

`windows` crate 里 `IInitializeWithFile` / `IInitializeWithStream` 在
`Win32::UI::Shell_PropertiesSystem`，不在 `Win32::UI::Shell`；查 CLSID 还要
`Win32_System_Registry`。该版本没有 `IInitializeWithItem`，只走文件路径这一条就够了。

## #120 预览处理器会自己吞滚轮

`IPreviewHandler` 是别人家的窗口，不认我们的 `WH_MOUSE_LL` 那套路由。
做法：`window_from_point(光标)` 拿到光标下的真实窗口，不是主窗口就
`post_wheel(target, delta, 屏幕坐标)` 转给它——`WM_MOUSEWHEEL` 的 lParam 是**屏幕**坐标，
不是客户区坐标（这一点和 `WM_MOUSEMOVE` 相反）。

## #121 1:1 拷贝不要用 HALFTONE

`SetStretchBltMode(HALFTONE)` 要做颜色修正，代价按**纹理大小**算；而拖动/平移每帧都在
`paint_bitmap_at` 里 1:1 贴图，等于每帧白烧一遍插值。
判断 `dw == sw && dh == sh` 就走 `COLORONCOLOR`（纯拷贝），只有真正缩放时才 `HALFTONE` + `SetBrushOrgEx(hdc,0,0,None)`。

## #122 CS_HREDRAW | CS_VREDRAW 会让每次改尺寸整窗重画

这两个类样式是给"用 GDI 直接画界面"的程序用的：尺寸一变就 `InvalidateRect` 整个客户区。
我们的客户区是几 MB 的位图，重画一次就是几十毫秒。预览窗不需要，`register_class` 一律传
`style: WNDCLASS_STYLES(0)`。

## #123 压缩包的目录条目可能不存在

`zip` 里"目录"只是**可选**条目：Windows 资源管理器压缩、python `zipfile` 都不写。
按 `entry.is_dir()` 统计目录数会得到"0 个文件夹"，而文件路径里明明有层级。
做法：目录数按**路径前缀**去重统计，目录树也从前缀推（见 `qlr-plugin-office` 的 `tree_lines`）。
## #124 IPreviewHandler 的 Unload 不能放在同步消息处理里

`ShellPreview::drop` 会 `Unload()` + `DestroyWindow`。如果这个 drop 发生在 `WM_COPYDATA`
（别的进程 `SendMessage` 同步等我们）的调用栈里，UI 线程会卡死：**窗口留在屏幕上、正文变白、日志断在
`收到请求`、进程还在但不响应**。
做法：`retire_shell_preview()` 把句柄挪进 `SHELL_TRASH`，`PostMessage(WM_QLR_INVOKE)` 回到自己的消息循环里再卸。
顺带：`hide_preview` 必须**先隐藏窗口**再交句柄，否则中间会闪一帧白窗。

## #125 跟随轮询会和拖动抢 UI 线程

`selection::current_selection()` 要枚举 `IShellWindows` 并重试 `FindWindowSW`，几十毫秒是常态。
它在 400 ms 计时器里跑、和拖动/改尺寸共用 UI 线程时，表现就是"窗口跟不上鼠标、过一会儿才追上来"。
做法：计时节流 + **拖动/改尺寸期间直接跳过轮询**，并给 >40 ms 的情况记 `SLOWSELECT`。

## #126 分层投影窗口要节流同步

投影是独立的 `WS_EX_LAYERED` 窗口，`SetWindowPos` 每次都要走 DWM。拖动时每帧同步会让主窗口掉队。
做法：拖动热路径最多 8 ms 同步一次，松手时强制对齐一次。

## #127 pwsh 里别用 -Command 传中文脚本

带中文注释的脚本塞进 `pwsh -Command` 会被控制台代码页搞坏（`ParserError` + 乱码，行号还对不上）。
一律 `WriteAllText` 成 `.ps1`（UTF-8 无 BOM）再 `pwsh -File` 跑。
## #128 持锁调"也要锁同一把锁"的函数 = 自锁死

1.7.3 的压缩包列表视图黑屏就是这个：`paint_archive()` 拿着 `archive_slot` 的 `MutexGuard`
去调 `archive_rows()`，后者第一行也要 `archive_slot().lock()`。`std::sync::Mutex` 不可重入，
于是 UI 线程**永久阻塞**——表现是窗口一片黑、点哪儿都没反应、日志停在最后一条 `wm=0x000F`。
做法：铁律改成"**锁内只拷字段，放锁之后再调下一层**"。读 `paint_archive` 的第一段代码当模板。
自查手法：`grep` 一遍被 `*_slot()` 包住的那几行，只要里面出现了另一个会加锁的函数名就有问题。

## #129 窗口在屏幕外时，`PrintWindow` 抓回来的是"混合帧"

排查黑屏时被这个坑了二十分钟：截图里名字列是新的、大小/时间列还是上一帧的。
根因不在绘制——窗口有一半在屏幕外（1920 屏，窗口 rect 1510..2390），Windows 只画**可见部分**，
`PrintWindow` 拿到的表面就是"新画的部分 + 陈旧的部分"。
判据：`GetWindowRect` 的 `right/bottom` 超出 `GetSystemMetrics(0/1)`，或 `WindowFromPoint` 在窗口内的点返回别家窗口。
做法：自动化截图前先把窗口挪回主屏（`scripts/verify-archive.ps1` 里的 `Normalize-Position`）；
看到"一半新一半旧"的截图先量窗口矩形，别先怀疑绘制代码。

## #130 GDI 的 `DrawTextW` 不做字体回退，彩色 emoji 会画成豆腐块

列表里的 📁/📄 在 `TextStyle::Normal` 下全是空框。做法：只用几何符号（▾ / ▸）与文本，
文件类型靠后缀区分；要真图标就得 `DrawIconEx` + 自备位图，别指望 emoji。

## #131 `WS_CLIPCHILDREN`：不写它，父窗口的底色会擦掉子窗口的像素

系统预览处理器（PowerPoint/Word）是画在**子窗口**上的。`create_preview_window` 少了
`WS_CLIPCHILDREN`，我们每次整窗填底色都会把子窗口的像素一起抹掉，然后处理器只在
真正"有脏区"时才重画——表现是"翻页后画面还停在第 1 页、只零星重画几块"，极像翻页失效。
做法：预览窗样式恒为 `WS_POPUP | WS_CLIPCHILDREN`（0x8000_0000 | 0x0200_0000）。
判据：用 `EnumChildWindows` 列出子窗口，若内容区明明有子窗口却"画不出来"，先查这个样式。

## #132 想吃掉处理器区域的鼠标，就 `EnableWindow(FALSE)`——但要等首帧落地

Office 预览的子窗口铺满内容区，鼠标在**窗口边缘**按下时事件会进到子窗口里，我们收不到，
于是"拖边改窗口大小"对 Office 文件彻底失效（用户反馈）。
做法：`DoPreview()` 之后约 400ms（等首帧画完）`EnableWindow(宿主子窗口, FALSE)`，
此后 `WindowFromPoint`/点击一律回落到预览窗自己；处理器**照样渲染**，也照样认
`PostMessage(WM_KEYDOWN, VK_NEXT)`（这就是 PPT 翻页能接管的物理基础）。
两个坑：① 立刻禁用会让内容区**一直是白的**（`DoPreview` 只是"开始准备"）；② 禁用后
`WindowFromPoint` 永远返回预览窗，靠它"判断光标在哪个子窗口上再转发"的代码全部失效，
滚轮必须改成直接投给处理器里"面积最大的那个后代窗口"。

## #133 顶栏标题错乱的根因是**时序**，不是取值

`show_preview` 里"记下文件名"必须发生在**显示窗口之前**。因为后面 `DoPreview()` 是跨进程
COM 调用，STA 上会泵消息，期间到达的 `WM_PAINT` 会真的跑一遍 `repaint()`；那时 `last_path`
还是上一个文件，顶栏就按旧名字画了一帧，而这之后没有任何 `invalidate` 去纠正它——
旧标题会一直留在屏幕上，直到用户滚动/缩放/碰顶栏。
做法：`with_state(|s| s.last_path = target.clone())` 放在 `window::show_at` 之前；
验证脚本靠日志顺序断言（`标题栏 ← X` 必须在 `SHELL 预览已接管` 之前）。
顺带：非 PDF 的长文件名曾被无条件扣掉页码框（约 220px）导致 `DT_END_ELLIPSIS` 截半、
窄窗（240px）甚至算出反向矩形让标题整条消失——页码框按 `chrome.page.is_some()` 才让位。

## #134 换文件时"不再需要处理器"也要收尸，否则旧子窗口盖在新内容上

`open_shell_preview()` 只在"这次也要用系统预览处理器"时才会把上一个处理器 retire 掉。
于是从 docx/pptx/xlsx 切到 zip / 图片 / 文本时，处理器的子窗口会**留在预览窗里继续盖着**：
用户看到的是「顶栏是新的文件名、正文还是上一个文件」；拖动/改大小时那片残留由别人的进程
按旧尺寸重绘，还会出现竖条纹（1.7.5 用户报的"拖动就这样"）。
做法：换文件时，只要这次不走处理器（`!shell_wanted`）就先 `drop_stale_shell_preview()`；
压缩包分支是**提前 return** 的，容易漏——那里也要调一次。
自查：`EnumChildWindows(预览窗)` 在"切到 zip 之后"必须为 0。

## #135 跟随选中项：90 ms 轮询 + 线程直接叫醒 UI

以前是"跟随线程先睡 300 ms 再查 → UI 侧 400 ms 计时器取结果"，最坏情况白等 **≈700 ms**
（实测 419 ms 平均 / 440 ms 最差）。两个数都要改：

- **发现**要快：窗口开着时线程每 **90 ms** 摸一次（收着时 250 ms，只为投机预热）；
  睡眠要扣掉本次查询的开销，否则查询 30 ms 就变成 120 ms 一圈。
- **通知**不要等计时器：线程一发现"UI 显示的不是这个文件"就 `PostMessage(WM_QLR_FOLLOW)`，
  UI 立刻切。计时器退化成兜底（同时负责预热）。
- 别对同一个路径反复通知：`last_path` 与"已通知过"两个条件都满足才投消息，
  否则"不支持"的文件会每 90 ms 抖一次消息。
- 查询本身也要省：`IShellWindows` 按线程缓存（`CoCreateInstance` 每次几毫秒），
  再用"上次命中的 browser"直接取一次选区（同一窗口里换文件的命中率极高），
  失效（切标签/窗口关了/空选区）才走完整枚举。

实测（真按键 `SendKeys Down` 改选中项，日志时间戳算差）：**平均 41 ms，最差 72 ms**（4 轮）；
同一脚本在旧二进制上是 **平均 419 ms、最差 440 ms**。

## #136 换文件的"装帧"：先冻像素，旧内容留一会儿

用户反馈"Office 预览切换时偶尔跳回那个占位页面、割裂感重"。原因是 `DoPreview` 是跨进程的，
要 130–260 ms，这期间旧处理器已经卸掉、新的还没画，整窗重画就露出我们自己的文字占位页。
两层处理：

1. `hold_frame`：换文件时只画顶栏那一条（`window::paint_chrome_strip`），内容区像素不碰
   ——窗口类背景刷是空的、`WM_ERASEBKGND` 返回 1，没画到的地方自然保持原样；
   `TIMER_SHELL_INPUT`（首帧落地）到点解除。
2. `shell_cover`：旧处理器先不卸，`SetWindowPos(HWND_TOP)` 顶到同级最上层继续盖着，
   `TIMER_SHELL_SWAP`（260 ms）到点再卸。

注意：Office→Office 时 Office **自己**会把旧处理器的窗口拆掉，所以那几十毫秒里剩下的白底
是 Office 的首帧延迟，不是我们的占位页了（抓帧对比：137 ms 一帧是空白 Word 窗，
199 ms 就是正文）。

## #137 `IPreviewHandler::DoPreview` 之后**不能立刻再进 COM**（Excel 直接卡死）

Excel 的预览器是 OLE 就地激活对象。`DoPreview()` 返回后我们马上去
`SetWindow(host, rect)` 去对齐尺寸 —— 实测**整个进程卡死**：`Responding=False`，
日志停在 "SHELL 预览已接管"，用户看到的就是"占位页不动 + 滚动条全废"。
Excel 在激活过程中要向容器（我们）同步请求，而我们是发出调用的那一方，自己等自己。
做法：**子窗口矩形在 `open()` 之前就算好**（内缩/铺满都提前定），打开之后不要碰
`SetWindow`；真要改尺寸（拖边、缩放）也只能等它画完（轮询 `visible_area` 之后）。
判据：`Get-Process qlrust | Select Responding` 为 False，且日志停在 takeover 那行。

## #138 Office 预览要"可激活"才有滚动条，但滚轮仍可能不动

Word/Excel 的预览对象只有**容器能被激活**时，滚动条拖动、选区这些才响应。
我们的预览窗是 `WS_EX_NOACTIVATE`（为了不抢资源管理器焦点），于是 Excel 的滚动条
"看得见、点不动"。做法：**只在打开不接管输入的系统预览前**清掉 `WS_EX_NOACTIVATE`
（`window::set_activatable`），图片/文本/压缩包/PPT 保持不抢焦点。
已解决：拖动滚动条（实测生效）。
**未解决**：Excel 的**滚轮**——钩子放行、`PostMessage(WM_MOUSEWHEEL)`、`WM_VSCROLL`、
`VK_NEXT` 全试过都不动；而资源管理器自己的预览窗格里同一个文件能滚
（对照截图 `_bench\drag\pane-after.png` 滚到了第 25–50 行），说明差在宿主激活状态/焦点，
下一步候选：把焦点交给 Excel 的子窗口，或读 UIA 的滚动条 range 后自己设值。

## #139 `SetFocus` 不能持锁调（焦点切换会把消息泵回自己 → 自锁死）

`WM_ACTIVATE` 里拿着 `STATE` 锁调 `SetFocus(子窗口)`，Word 预览实测**整个进程卡死**，
Windows 事件日志记 `Application Hang`（1002）后强杀。原因不是 `SetFocus` 本身跨进程失败，
而是焦点切换会同步把激活/焦点相关消息**泵回我们自己的窗口过程**，重入的处理器再去拿
同一把 `std::sync::Mutex` —— 非重入锁，自己等自己（和 #128 同一个根，换了入口）。
做法：**句柄只在锁内取出，`SetFocus` 在锁放开之后调**；再加一个 `thread_local` 重入闸门
（`IN_FOCUS_TRANSFER`）避免"激活 → 转交焦点 → 再激活"来回套娃。
判据：点一下 Office 预览窗 → `Get-Process qlrust | Select Responding` 变 False，
日志最后一行是 `wm=0x0006`（`WM_ACTIVATE`）。

## #140 `WM_MOUSEACTIVATE` 返回 2 = `MA_ACTIVATEANDEAT`，会吞掉第一次点击

实测：未激活的预览窗上第一次按下 → 只收到 `WM_MOUSEACTIVATE` + `WM_ACTIVATE`，
**收不到 `WM_LBUTTONDOWN`**，于是拖边改大小"第一次没用、第二次才行"。
`MA_ACTIVATEANDEAT(2)` 的语义就是"激活并**丢弃**这次鼠标消息"，我们要的是
`MA_ACTIVATE(1)`（激活，且消息照发）。四个值：1=`MA_ACTIVATE`、2=`MA_ACTIVATEANDEAT`、
3=`MA_NOACTIVATE`、4=`MA_NOACTIVATEANDEAT`。

## #141 `IMediaSourceReader::SetCurrentPosition` 的 `guidtimeformat` 不能传 NULL

MSDN 说传 NULL 表示"100 ns 单位"，但本机 `mfreadwrite` **直接解引用**它 →
`STATUS_ACCESS_VIOLATION`（`seek` 视频必崩）。传 `GUID::zeroed()`（等价 `GUID_NULL`）
即可，官方样例也是这么写的。二分定位靠临时探针 `examples/probe_stream.rs`（已删）：
零 `PROPVARIANT` + `GUID_NULL` 成功，`std::ptr::null()` 必崩。

## #142 COLORREF 是 `0x00BBGGRR`，写颜色别按 RGB 顺序想当然

`AUDIO_FILL_RGB` 第一版写 `0x004A90D9`（想表达 #4A90D9 蓝），实际画出来是**橙色**：
低位字节是红。要 #4A90D9 得写 `0x00D9904A`。写不对称的颜色常量时在注释里标明目标色值。

## #143 视频走"位图通道"时，进度条要留出来（`reserve_bar`）

`paint_bitmap_fit_window` 铺满整个内容区 → 每一帧都把底部 52 px 进度条擦掉
（表现为"进度条闪一下就没了"）。做法：`window::picture_area(area)` 先扣掉
`audio_bar_rect`，`paint_bitmap_fit(..., reserve_bar: bool)` 按需用它；
另外整窗重画（`paint_overlays`）也必须补画进度条，否则通道切换时又丢。

## #144 本机 MCI 播不了 mp4（视频只能出画面，没有声音）

实测三种类型名：`type mpegvideo` → `MCIERROR=277`（"初始化 MCI 时发生问题"）、
`avivideo` → `296`、auto → `277`；而 `type MPEGVideo` 打开 wav 是 OK 的（说明类型名有效）。
结论：**视频播放不能走 MCI**，只能用 Media Foundation 解帧自绘（1.7.9 的做法），
要出声得再上 `IMFMediaSession` + EVR 或 WASAPI。

## #145 改窗口大小：处理器要 SetRect，但**必须等输入同步呼叫退栈 + 全程不持锁**

症状（用户 1.8.0 报）：「窗口伸缩会引起白边」——把 docx/xlsx 预览拉宽，右边留一条空白。
根因：Office 预览是 OLE 就地激活对象，它自己建的子窗口（`EXCEL9 → EXCEL7`）不会跟着我们的
宿主长大；1.7.2 时代走插件位图渲染所以没这问题。
三个坑，按踩的顺序：
1. WM_SIZE / WM_LBUTTONUP 里直接调 IPreviewHandler::SetWindow/SetRect → 被拒
   `HRESULT(0x8001010D)` = `RPC_E_CANTCALLOUT_ININPUTSYNCCALL`（那时还在输入同步呼叫里）。
   做法：window::post(hwnd, ipc::WM_QLR_SHELLFIT)，等这条同步呼叫退栈再动 COM。
2. 投递消息里**持锁**调 SetRect → **整进程卡死**（事件日志 Id 1002 Application Hang）。
   SetRect 会把消息泵回我们自己的窗口过程，重入再拿同一把 `Mutex` = 自等（#139 那一族）。
   做法：`with_state(|s| s.shell_preview.take())` 取出句柄 → **锁外**调 → 再放回去。
3. 想改用"改完大小重开一次预览"绕开 → 能用但**不该用**（用户明确否掉：慢、费资源）。
   现在这条路只是让处理器重新排版，不 `DoPreview`、不丢页码、不闪。
验证：`big-sheet.xlsx` 拉宽 760→1040→1040x660，右缘纯白 78.5%（未改尺寸基线）/78.8%/79.7%
= 白边消失；`docx` 正文铺满整宽；进程 `Responding=True`、无新 1002 事件；
Excel 滚轮 / 横向滚动条拖动 / Shift+滚轮 全 True。

## #146 改窗口大小时投影窗口要自己跟一次

投影（`QLRust.ShadowWindow`）是另一个分层窗口，不会随预览窗的 `SetWindowPos` 自动走。
只改窗口不同步投影 → 用户看到"画框和阴影对不上"（1.8.0 用户截图）。
做法：`WM_SIZE` 里调 `sync_shadow_with(true)`（拖动中限流 8 ms），`finish_resize` 里
再 `sync_shadow()` 精确对齐一次。实测 1040x660 的窗口配 1072x692 / 偏移 -16 的投影 = 一致。

## #147 Excel 预览"看得见、点不动"：焦点只到 EXCEL9 容器，要用 `IPreviewHandler::SetFocus`

症状（用户 1.8.1 报）：「Excel 焦点聚焦到预览窗口的时候，鼠标左键无法选中目标单元格」。
排查：`EXCEL9/EXCEL7/NUIScrollbar/NetUIHWND` 全部 `en=True`（输入没被禁），`WindowFromPoint`
返回 `EXCEL7`，但日志里只有 `wm=0x0021`（WM_MOUSEACTIVATE）、之后没有 `WM_LBUTTONDOWN`；
`GetGUIThreadInfo` 显示线程焦点停在**容器层 `EXCEL9`**，画表格的 `EXCEL7` 从未拿到 UI 激活。
向 `EXCEL7` 投 `WM_LBUTTONDOWN`/`WM_LBUTTONUP` 也没用（点击前后像素差 0）——合成消息不行。
做法（`ShellPreview::set_com_focus()` → `IPreviewHandler::SetFocus()`）：
1. `WM_ACTIVATE` / `WM_SETFOCUS` 里调 → `0x8001010D`（输入同步呼叫里，COM 出不去）；
2. `WM_MOUSEACTIVATE` 里调能成功，但 COM 泵消息会**吃掉这一次点击**（用户要连点两下）；
3. 最终：`WM_MOUSEACTIVATE` 只 `LRESULT(1) // MA_ACTIVATE` 不抢焦点；在
   `TIMER_SHELL_INPUT`（处理器内容占到 50%、已退栈）里调一次 `focus_shell_content()`
   （内部先 COM SetFocus、失败再退回 `window::focus_child`）。**第一次点击就选中**。
另一个前提：`take_over_input()`（`EnableWindow(FALSE)`）只能给 PPT；给 Excel/Word 用会把
所有子窗口一起禁掉，同样点不动（1.8.1 起按 `shell_owns_input` 分流）。

## #148 图片/视频：锁**窗口比例**，不是给图片做等比留白

用户 1.8.1 原话：「图片类型取消窗口的拉伸缩放，且窗口比例固定预览图片比例，不能破坏图像撑满窗口」。
1.7.8 的做法（`paint_bitmap_fit` 等比居中留白）方向错了：用户要的是**窗口本身**只能按素材
比例改大小，内容永远铺满。做法：
- `State.media_aspect`（`宽/高`）：`show_preview` 对 `Image | Video` 记下来，`hide_preview` 清；
- `window_size_for` 走 `media_window_size(work, aspect, width, bar)`（超工作区先压高再反算宽）；
- `on_resize_move` 走 `clamp_media_resize(rc, edges, aspect, bar)`：只拖上下 → 高度驱动，
  其余宽度驱动；拖左/上边时对边不动，只拖上下时水平居中；
- `repaint` 的 `Canvas::Bitmap` 只对 `Svg | Video` 走 `paint_bitmap_fit`，**图片走铺满**。
- `bar` = 视频底部进度条 `window::AUDIO_BAR_H = 52`（算内容高必须扣掉，否则比例会偏 0.05+）。
实测（`scripts/verify-mediashape.ps1`）：图片 `1.778 → 1.777 → 1.778`、视频 `1.778 → 1.777 → 1.778`。

## #149 交给系统预览处理器时不要留文字占位页

用户 1.8.1 报：「Excel预览窗口打开的时候我还是看到了一个占位页面」。系统处理器冷启动要
几百毫秒到 1 秒多，这期间我们画的是"文件信息 + 正在渲染…"那一页，观感像卡住。
做法：`show_preview` 里 `if shell_candidate(target) { lines.clear(); }` —— 交给处理器的类型
连文件信息页都不画，窗口先空着等首帧（`SHELL_SWAP` 的 cover 机制保证不闪白）。

## #150 图片/视频：连"拖边改大小"也不要，缩放交给滚轮

用户 1.8.2 原话：「把图片和视频的窗口伸缩功能去掉，滚轮对窗口进行缩放就足够了」。
1.8.1 做的是"窗口比例锁成素材比例"（拖边仍可，但只能等比），用户觉得多余——
看细节用滚轮缩放就行，窗口每张图大小不一样反而烦。
做法：
- 新增 `resizable_kind(kind) -> bool`（`Image | Video` 为 false），`resize_margin()` 对它们
  **返回 0**；`edges_at()` 在 `margin <= 0` 时直接返回 `ResizeEdges::default()`
  （否则 `x <= left && x >= left` 在正好压线时还能抓到一个 0 宽的边）。
  `WM_SETCURSOR` 与 `WM_LBUTTONDOWN` 都用同一个 `resize_margin`，所以光标也不会变成双向箭头。
- 窗口大小只在打开时按素材比例定一次（`media_window_size`）；`clamp_media_resize` 留作兜底
  （系统 `SetWindowPos`/DPI 变化时别把内容拉变形），`on_resize_move` 里那条分支保留。
- **最大化**这类"外部改大小"仍会发生：图片在 `window_aspect_differs(hwnd, w, h)`（比例差 >2%）
  时退回 `paint_bitmap_fit` 等比居中，其余时候铺满。
验证：`_bench\noresize-probe.ps1` —— 图片 `707x428` 拖右/拖下后仍是 `707x428`，滚轮前后画面差
**56.48%**（能缩放）；视频 `1109x706` 拖边不变。`scripts\verify-mediashape.ps1` 新增两条
"拖右边/拖下边不改窗口大小"（`640x390 / 640x442` 前后一致），**13 PASS · 0 FAIL**。
注意：**视频的滚轮是快进/快退**（一格 5 秒，1.7.9 的既定行为），不是缩放。

## #151 MCI 不认带 `..` 的路径，单测夹具路径必须先 `canonicalize`

发布准备时把单测夹具回退路径换成 `env!("CARGO_MANIFEST_DIR")` 拼 `../../../../qlrust-dev/compare`
（为了不再写死开发机绝对路径），结果 `qlr-platform` 的 3 条 audio 测试（`aliases_are_unique_so_two_players_coexist`、
`open_reads_duration_of_wav_fixture`、`seek_and_position_are_milliseconds`）在**全量**运行里失败，
单独跑 `cargo test -p qlr-platform audio` 又全过；报错是 `AudioPlayer::open` 返回 `None`
（`第一个会话应能打开`），不是"文件不存在"。

根因：`mciSendString` 的 `open "<path>" alias ...` **不接受含 `..` 的路径**（同样的文件换成
`QLRUST_MEDIA_FIXTURES` 的绝对路径就能打开）。修法：`fixture()` 命中文件后先
`std::fs::canonicalize(&path).unwrap_or(path)` 再返回。
教训：**任何传给 MCI 的路径都必须是解析好的绝对路径**；顺便，`canonicalize` 也能消掉 8.3 短名与符号链接的差异。

## #152 拍视觉基线必须先把 exe 复制到"中性目录"，否则会拍到托盘失败的 toast

症状：同一套 exe、同一份夹具，`scripts\baseline-shot.ps1` 在仓库目录
（`<桌面>\DSH\quicklook-rs\target\release`）跑出来 **text 截图多一个顶栏气泡**
（「托盘图标没加上：后台常驻，qlrust.exe --quit 可退出」），`baseline-diff` 报
`text DIFF 7580 px 最大通道差 200`，看着像 1.8.3 的视觉回归；把 exe 复制到
`<桌面>\qlr-ab\base` 再拍就 5/5 SAME。

根因：仓库/工作目录里的可执行文件会被打上 **Low Mandatory Level** 标签，低完整性进程给 explorer
加托盘图标会被 UIPI 拒成 `0x80070005`；1.8.3 起"托盘失败"改成**第一次显示窗口时弹一条顶栏 toast**
（不再起线程弹模态框，见 `app.rs::notify_tray_failure_once`），而基线脚本第一张恰好是 text，于是
toast 进了基准图。这是环境噪音，不是绘制差异。

修法（脚本已内置，别绕开）：
- `scripts\baseline-shot.ps1` 默认把 `qlrust.exe` + `pdfium.dll` + 插件复制到
  `<桌面>\qlr-ab\base`（可用 `-NeutralDir` 改）再拍，并清掉该目录的 `UserData`；
  想复现"低完整性 + toast"的样子才加 `-NoNeutral`。
- 同时加了**尺寸稳定**判据：show 之后窗口还会被缩放/自然尺寸路径改一次大小，早 200 ms 截会拍到
  "小一号的中间态"（实测同一套 exe 两次跑出 `814x488` 与 `1589x924`）。现在要求连续两次
  `GetWindowRect` 一致才截；截完再比一次矩形，变了就丢掉重拍（最多 3 次），仍在变就直接报错——
  宁可脚本失败，也不往基准目录写中间态。
- 刷新基准的固定流程（1.8.3 起）：备份 `baseline\before` → 用脚本重拍 → 立刻再拍一次
  `baseline\after-check` → `baseline-diff -A baseline\before -B baseline\after-check` 必须 5/5 SAME。

教训：**跑任何"像素级"脚本之前先问一句"这个进程在当前目录里能不能加托盘"**；环境降级会通过
1.8.3 新增的 toast 混进画面。

### #52 `dpi::enter` 不许缓存 hwnd：`if CURRENT == hwnd { return }` 会自锁

症状：把一个窗口从 100% 屏拖到 150% 屏，字号/行高/顶栏高度全按旧 DPI 画，而且**永远**不恢复。
根因：缓存以 hwnd 为键，判断"同一个窗口就直接返回"。跨屏时 hwnd 没变、DPI 变了，于是永不重查；
而"该失效的条件"补不完（跨屏、改缩放、RDP 重连都会变）。
修法：`enter(hwnd)` 每次都调 `GetDpiForWindow`（用户态 O(1)，内核侧已有缓存），只缓存 **DPI 值**，
值变化时才清字体缓存（`FontSet::clear()`）并 invalidate。
数字：`GetDpiForWindow` 一帧几十次的成本在微秒级，`bench.ps1` 的连续 200 帧重绘项可作回归门禁。

### #53 `GetGUIThreadInfo` 的 `cbSize` 必须手填，否则失败长得像"没在打字"

症状：前台明明在输入框里打字，判据却返回 false，空格照样被吞。
根因：`GUITHREADINFO { cbSize: size_of::<GUITHREADINFO>() as u32, ..Default::default() }` ——
若写成 `let mut gti = GUITHREADINFO::default(); gti.cbSize = ...`，clippy 会拦；
而**忘记填**（`cbSize = 0`）时 API 直接失败，返回 false，和"前台确实没在打字"完全无法区分。
规矩：这个结构体的 `cbSize` 与 `MONITORINFO` / `OSVERSIONINFO` 一样，是入参不是出参，
必须显式赋值并加注释；判失败时按"不在打字"处理（宁可吞键，也不能让钩子超时被摘）。

### #54 判断"在打字"不能用 `DirectUIHWND` / `SysListView32`

症状（若这么写）：资源管理器里所有预览都失效。
根因：Win11 文件列表本身就是 `DirectUIHWND`，且**永远处于焦点**；把它当"编辑框"就等于
把空格热键整体禁用。
规矩：只认真正的编辑框类（`Edit` / `RichEdit20W` / `RichEdit50W` / `RichEditD2DPT`）。
首选判据仍然是 caret（`hwndCaret` 非空），类名只作兜底；并记录"哪一级命中"（`text_focus_level`），
否则换一台系统版本就不知道退化到哪一级了。


## #153 早停省时间可以，但**不能让界面上显示的数字变成假的**

`markdown.rs` 加了 `layout_capped(lines, max_out)`：首屏只需要 400 行时不再把 8 MB 全文排版
一遍（356 ms → 16 ms）。但 `html.rs` 同一套改法**必须放弃**——HTML 的状态行报的是"全文共 N 行"
（**排版之后**的显示行数），排到一半就再也算不出 N；Markdown 报的是源文件行数（`raw.len()`），
才不受早停影响。试过 `total_lines = if capped { max_lines }` 硬凑，单测
`long_document_is_truncated_by_budget` 立刻红（断言 50、实得 10）——这条单测就是为这个坑留的。

判据：**早停之后凡是还要显示给用户看的"总量/总数"，必须来自不需要全量计算的源头**
（源文件行数、`metadata().len()`、索引计数）；拿不到就别早停，或者把界面措辞改成
"已显示前 N 行"。省几十毫秒不值得让预览开始撒谎。

## #154 热循环别按字节下标取像素，但也别急着上 SIMD

`scale.rs::downscale_exact` 原本逐字节 `src[...]` 取值再拼成像素，4K→3178 一档 51.4 ms。
改成整行切片 `&src[y*sw*4..(y+1)*sw*4]` + 按输出列的 `band` 切片 + `band.chunks_exact(4)`
+ 行内局部 `[u32; 4]` 累加器（原来每个源像素都要读改写一次输出缓冲），同一档 46.1 ms，
48MP→3178 100.6 → 71.2 ms。**纯标量，没有手写 SIMD。**

两条经验：

- **先量再猜**：这一档省 5 ms，而"少解码一档"省 105 ms——批次二的收益大头始终在缓存上，
  不在指令级。别一头扎进 SIMD。
- **朴素基线必须留着钉**：改写后要有"与另一种实现逐像素相同"的单测
  （`downscale_matches_image_crate` 对着 `image` 的 `thumbnail` 逐字节比），否则这类优化
  出问题只表现为"某些尺寸下颜色差一点"，肉眼基本发现不了。
- 累加器的量级心里要有数：`acc` 只在**一个源行的一个列带**内累加（≤ 带宽 × 255），
  跨行的 `sums` 是**一个输出像素覆盖的块内所有源像素**之和（≤ 块面积 × 255，块面积 =
  源像素数/输出像素数，真实图片只有个位到几百），`u32` 绰绰有余；末尾的 `.min(255)`
  是沿用原实现的防御性钳制，不是必需。

## #155 每处缓存各自的上限是**加法**，不是上限；要有一本总账

1.8.3 结束时进程里同时住着四笔内存，各自守着自己的常量、互不知情：

| 谁 | 常量 | 位置 | 上限 |
|---|---|---|---|
| 源图档位缓存 | `CACHE_LIMIT_BYTES` | `crates/qlr-render/src/source_cache.rs` | 64 MB |
| PDF 页缓存 | `CACHE_LIMIT_BYTES` | `crates/qlr-app/src/pagecache.rs` | 64 MB |
| 上一帧位图 | `LAST_FRAME_MAX_BYTES` × `LAST_FRAME_CAP` | `crates/qlr-app/src/app/render.rs` | 24 MB |
| PDF 文档槽 | `DOC_SLOT_MAX_BYTES` | `crates/qlr-render/src/pdf.rs` | 256 MB |

加起来 ≈ 408 MB，再叠一帧 32 MP（`MAX_RENDER_PIXELS`，≈128 MB）就能摸到 500 MB——
而每个常量单独看都很合理。**症状**：用户报"预览器占内存"，谁都觉得"我这项没超"。

修法：`crates/qlr-render/src/budget.rs` —— 一本 256 MB 的账（`LIMIT_BYTES`），
`try_reserve(n) -> Option<Permit>`，`Permit` drop 即归还（**不要 `mem::forget`**）。
三处缓存的策略统一为：

1. 先满足自己的上限（保证"只有这一项在用时"能吃饱）；
2. 再向总账申请额度；
3. 拿不到就继续淘汰自家条目；自家空了还不给 → **不缓存**（缓存只影响快慢，不影响对错）。

细节：`pdf::with_document` 换文档时必须**先 `*held = None` 放掉旧文档再申请**，
否则"旧文档还占着额度"会把同尺寸的新文档挡在门外（自己挡自己）。
观测入口是日志每帧一行的 `MEM used=..MB cap=256MB`，别再加第二套计数。

## #156 单测别碰全局账——把账做成可实例化的 `Budget`，靠生命周期还账

`budget.rs` 第一版是 `static USED: AtomicUsize` + `reset_for_test()` + 一个测试互斥锁。
结果：并行跑的 `source_cache` 用例会**莫名**拿不到额度（`cargo test` 默认多线程），
报出来的原因看起来像"缓存逻辑坏了"。

改成结构体就能同时满足两边：

- `pub struct Budget { used: AtomicUsize, limit: usize }`；
- `pub fn try_reserve(&self, n) -> Option<Permit<'_>>`，`Permit<'a> { budget: &'a Budget, bytes }`，
  `Drop` 里 `fetch_sub` —— 归还靠**借用的生命周期**证明，不需要 `reset`；
- 生产代码用 `static GLOBAL: Budget = Budget::new(LIMIT_BYTES)` + 模块级薄包装；
- 单测各开一本 `Budget::new(小上限)`（如 `the_three_caches_fit_or_the_big_document_stays_out`
  直接 `Budget::new(233 MB)`），彼此完全不共享。

顺带一条：`checked_add` 写成 `let next = used.checked_add(bytes)?;` ——
先写 `if let Some(..) = .. else { return None }` 会被 clippy 的 `question_mark` 拦下。

## #157 先量再改：空闲 CPU 只有 0.05% 时，"降频优化"只是给自己加的风险

结构评审把"空闲唤醒降频"（`FOLLOW` 400→1500 ms、`WATCHDOG` 250→1000 ms）列为 P2。
实测量法：预览窗开着、什么都不做，30 s 后读进程 CPU 时间——**15.6 ms**（单核 0.05%），
而且这还是 debug 日志开着（30 s 记了 331 条 `wm=0x0113`）时量的。

结论：**不做**。省下的是零头，代价是"选中项跟随"变迟钝、挂死发现从 250 ms 拖到 1 s。
留下判据：**给"定时器/轮询"类优化立项前，先量住空闲 CPU 或唤醒次数**；
没有数字的"应该会更省"不算理由（对照 #154 的"先量再猜"）。
量法：`Get-Process` 的 CPU 秒数差值，或 `_inbox\idle-cpu.ps1` 这类探针（**不进仓库**）。

## #158 顺序消费的东西别先攒成 `Vec`；逐字符 `push` 是 8 MB 文档的隐形税

两处都是 1.8.3 遗留的"看起来没问题"的写法，8 MB 的 HTML 上合计吃掉 40+ ms 和 46 MB：

1. **词法先攒后消费**：`tokenize(src) -> Vec<Tok>` 把 96 万个 token 全建出来再交给排版。
   排版（`Builder`）严格顺序消费、从不回看，`Vec` 完全是多余的中间态：
   结构体本身 967933 × ~48 B ≈ 46 MB，每个标签名还要各 allocate 一个 `String`（近百万次小分配）。
   换成 `for_each_token(src, |tok| …)` + 复用一个标签名缓冲，**38.1 → 10.2 ms**。
   - 借用怎么给：`fn for_each_token<F>(src: &str, f: F) where F: for<'a> FnMut(Tok<'a>)`
     ——标签名借的是循环里的局部缓冲，必须 HRTB 才能过编译；`Tok` 的变体要用 `&'a str` 而不是 `String`。
   - 双缓冲/复用 `String` 的时候注意非 ASCII：`for b in raw.bytes() { push(b as char) }`
     会把 UTF-8 拆成 mojibake，先 `raw.is_ascii()` 分流。
2. **文本逐字符入缓冲**：`for c in s.chars() { self.buf.push(c) }` 对 8 MB 正文就是 800 万次
   `push`（每次都查容量）。改成按"非空白段"整段 `push_str`：ASCII 空白按字节判，
   遇到非 ASCII 字节才解一个 `char` 问 `is_whitespace()`（U+3000 全角空格也得算空白）。

**测量的纪律**：这台机器单次 `Instant` 测量抖动 ±6 ms，单独测第 2 项时数字落在噪声里。
所以：① 基准里每个阶段跑 3 遍取最好；② 只在"两项合起来"这一级下结论
（`layout 122.7 → 74.8 ms`、`render_with(head,400) 120.7 → 87.1 ms`）；
③ 别把一次跑出来的小差异写进提交说明（对照 #154 / #157）。

## #159 一行文字拼三次 = 每行三次分配；把它压成一次，8 MB 文档省 24%

**症状**：HTML 8 MB（23 万显示行）排版 `head` 400 行要 87 ms，其中"落行"这一段占了 56 ms，
比词法（10 ms）和预处理（9 ms）加起来还多五倍。

**根因**：`emit_line`/`flush` 每落一行做了三次分配 + 三次拷贝：

```rust
let body = std::mem::take(&mut self.buf);        // ① 把缓冲搬走，下一行要重新扩容
let text = body.trim().to_string();             // ② 再拷一份（只为去掉首尾空白）
let line = format!("{prefix}{text}");            // ③ 拼前缀，又一次拷贝
self.lines.push(clip(&line));                    // ④ truncate_chars 里 s.to_string() 再拷一次
```

23 万行 × 3～4 次分配 = 近 80 万次小分配，全在排版热路径上。

**改法**：

1. `trim()` 只借不拷——`let text: &str = self.buf.trim();`，靠字段级借用分离（`self.buf`
   不可变借、`self.lines` 可变借，编译器允许），落完行再 `self.buf.clear()` 复用容量，
   不要 `mem::take`（那会把容量丢掉，下一行又得扩容）。
2. 拼行只做一次分配：`clip_line(prefix, text)` 先用 `char_indices().nth(预算)` 算出截断点，
   `String::with_capacity(prefix.len() + cut)` 一次建成，超预算才补 `…`。
   **截断语义必须与 `text::truncate_chars` 逐字符一致**，为此留了 `clip_line_matches_truncate_chars`
   做等价性断言（含 `prefix` 自己就超长的边界）。
3. 表格行同理：`collect::<Vec<_>>().join()` 改成循环 `push_str`。

**实测**（release，8 MB 夹具）：layout 全量 74.8 → 51.2 ms，`render_with(head, 400)` 87.1 → 62.9 ms；
叠加 #158 的流式词法，这条路径从最初的 120.7 ms 降到 62.9 ms（−48%）。

**推广**：凡是"容器里存一堆 String、每行现拼"的地方（Markdown 的 `layout`、代码高亮的断行结果），
都按这个顺序检查：能不能借（trim/切片）→ 能不能复用缓冲（`clear()` 而非 `take()`）→
最后才是一次性 `with_capacity` 拼装。`Vec<String>` 的 `push` 本身不贵，贵的是每行那几次小分配。

## #160 `SLOWPAINT` 不含折行——"滚一下卡"要先给折行加计时，别急着猜离屏缓冲

**症状**：大文本滚动时肉眼可见地顿，但 `app.log` 里的 `SLOWPAINT` 一片安静（都是 3–6 ms）。

**根因**：绘制路径上是两段截然不同的工作——"把原始行折成显示行"（`wrap::push_wrapped`，
每行要多次 `GetTextExtentPoint32W` 量宽，单次约 21 µs）与"真正画字"（`DrawTextW`）。
`SLOWPAINT` 只包住后者。折行一帧能吃掉 12 ms，日志里却看不见，
于是很容易把锅甩给离屏缓冲、双缓冲、DWM——那些都不是原因。

**做法**：在绘制循环里就地量：

```rust
let wrap_started = Instant::now();
let measured = std::cell::Cell::new(0usize);// 量宽次数（闭包里累加）
// …折行…
let wrap_ms = wrap_started.elapsed();
log_debug!(SCOPE, "WRAP … · 量宽 {} 次 / {:.1} ms · 缓存命中 {}/{}（存 {} 条）");
if wrap_ms >= Duration::from_millis(4) { log_warn!(SCOPE, "SLOWWRAP …"); }
```

三个字段各有各的用处：**量宽次数**说明是不是"量得太多"（→ 二分/缓存），
**耗时**说明总量，**折出多少行 / 吃掉多少原始行**说明这次折行值不值（长段落 vs 短行差 7 倍）。

**推广**：任何"用户说卡、日志说没事"的场景，先补度量再动手——本项目已经因此否掉过两个方案
（§6-8 空闲唤醒降频：实测空闲 30 s 只有 15.6 ms CPU；批次二"PDF 每次重读全文件"：合成夹具上
重读只占 0.2 ms）。度量代码留在主路径上，下一个接手的人不用重新怀疑一遍。

## #161 折行缓存要连"只折一行"的结果一起存，否则短行文档命中率为零

**症状**：折行缓存做好了、命中率却一直是 0：

```
WRAP 原文 121425 行 从 20 起，屏幕 29 行 -> 折成 29 行（吃掉 29 原始行）· 量宽 29 次 · 缓存命中 0/203（存 0 条）
```

注意 **`存 0 条`**——不是"存了没命中"，是压根没进缓存。

**根因**：第一版为了省内存，只缓存"折成 2 行以上"的结果（理由听起来很合理：单行的只要 1 次量宽，
存它浪费）。可屏幕上一屏 29 行里 24 行都是"一行放得下"的短行，恰好一条都进不了缓存；
而真正折成多行的长段落，一行占 7 个显示行，一屏只装 4 个原始行，滚一格整屏换掉，
同样命中不了。于是两种情况都是 0%。

**做法**：单行结果也存。代价是每行多一次 `Vec<String>` 克隆（几十字节），
换来短行文档每帧量宽 **29 → 5 次**（`缓存命中 144/203`）；长段落文档依然是 0 命中，
那里的收益要靠算法本身（`fit_prefix` 二分，1359 → 243 次），缓存救不了。

**推广**：判断"要不要缓存"别看**单条命中**的概率，要看**整屏命中**的比例——
滚动重绘的场景里，重复的是"屏上没动的那些行"，它们绝大多数是"一行放得下"的。
调试这类问题一定要把 `存 N 条` 和 `命中 h/m` 一起打出来，否则分不清"没存"和"没命中"。

## #162 注入的键鼠事件会被钩子按设计丢掉——滚轮/键位验不了，只能人手点

**现象**：给 `qlr` 加了表格二维滚动后，想用脚本验"滚轮到底滚没滚"：`SetCursorPos` 到内容区中央 + `mouse_event(MOUSEEVENTF_WHEEL, ±120)`，截图前后 **sha256 完全一致**（`7b36d203…`），连 `SHEET-SCROLL` 日志都没有。

**根因**：`crates/qlr-platform/src/hotkey.rs` 的底层钩子按设计丢弃注入事件——`:589` 有 `if info.flags & LLMHF_INJECTED != 0 { CallNextHookEx }`，键盘侧同样过滤 `LLKHF_INJECTED=0x10` / `LLKHF_LOWER_IL_INJECTED=0x02`（`:765`）。这是**故意**的：防自激（我们重放按键时不希望钩子把自己重放的当成用户输入）。所以任何 `SendInput` 玩法都进不来。

**做法**：`scripts/verify-sheet.ps1` 只验"能出帧 / 解析耗时 / 不 panic / `.xls` 不误判成表格"，滚轮与键位留给人手确认，并在脚本注释里写明原因。别再花时间给自动化脚本加注入通道——要验就先改钩子的注入过滤，那是安全取舍，不是测试问题。

**推广**：凡是"钩子驱动的交互"（滚轮、方向键、拖拽选区、托盘点击）都不要指望注入式自动化；能自动验的是**纯函数**（`sheet_extent`/`sheet_cell_at`/`scroll_clamp` 都有单测）和**窗口首帧**（`PrintWindow` 截图）。

## #163 单测并行 + 固定文件名 = 偶发"一个读一个写"

**现象**：`cargo test -p qlr-render` 偶发失败，`sheet` 模块里 4 个用例都往 `%TEMP%\qlr-sheet-tests\sample.xlsx` 造夹具——`cargo test` 默认多线程并行，A 用例正在 `zip` 读，B 用例 `File::create` 把它截断，于是 `Failed to read from zip`。

**做法**：每个测试用**自己的文件名**（`sample_parse.xlsx`、`sample_styles.xlsx`…），别共用。测试函数里别依赖"上一个测试跑完了"。

**推广**：造夹具的测试一律"一测一文件"；要复用就用 `std::sync::OnceLock` + 全局唯一目录，且只读。

## #164 "看起来像数字"的启发式别把日期/时间吞进去——预览只读，宁给半截文本也不给 `####`

**现象**：`plugin-demo.xlsx` 的日期列是 `t="inlineStr"` 文本（`2026-10-01`），预览里整列显示 `#####`，用户根本看不到日期。

**根因**：绘制端 `looks_numeric()` 的旧实现允许 `-`/`:`/`/` 出现在任意位置（想兼容 `12-3` 这种），于是 `2026-10-01` 被判成"数字"，列宽不够就走 Excel 的 `####` 分支。

**做法**：`-` 只允许作**前导符号**（正负号），`:` 与 `/` 一律判非数字；日期/时间文本落回"文本"路径（右侧空则溢出、有内容则硬裁）。

**为什么与 Excel 不同**：Excel 里日期本质是数字、`####` 是"请拉宽列"的提示，而**预览窗口不能拉宽列**，`####` 等于什么都没给；给半截 `2026-10` 至少还能看出是哪个月。这条是有意为之，别"修正"回去。单测 `numeric_heuristic_matches_excel_intent` 里 `2024-01-05` / `12:30:45` / `2026/10/01` 都在 non-numeric 列表。

## #165 首帧渲染别写死 `page = 0`——状态改了、页号没传出去，看起来像"功能没生效"

**现象**：给表格加上"翻页键切工作表"（`pages` = 工作表数、`page` 初值 = `workbook.xml` 的 `activeTab`）后，
`cargo check` 干净、状态也确实写进了 `State`，但真窗口截图**仍是第 1 张表**，顶栏也没有 `4 / 5` 页码框。

**根因**：`crates/qlr-app/src/app/show.rs` 里两处**首帧渲染**都写死了 `request_render(p, render_w, render_h, 0)`
（正常路径与"系统处理器失败后回退"路径各一处）。`s.page` 后来确实被设成 3，但第一帧早就按 `page = 0`
渲完了，于是用户看到的是第 1 张；`pages` 同理没进到 chrome，页码框自然不画。

**做法**：首帧一律传 `first_page`（PDF 的 `first_page` 仍是 0，行为不变），别再写死常量。

**推广**：凡是"先置状态、再渲首帧"的活儿，**所有首帧入口都要用同一份初值**——本项目里这类入口不止一个
（正常路径、处理器回退、冻帧hold_frame、shell 预览接管）。改状态语义时 `grep request_render` 全过一遍，
别只改看起来"主"的那条路径。
