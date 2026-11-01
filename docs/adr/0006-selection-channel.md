# ADR-0006：选中项通道——逐条枚举 `IShellWindows` + 同根匹配，不用 `FindWindowSW`

- 状态：已采纳
- 日期：2026-10-29
- 相关：`crates/qlr-platform/src/selection.rs`、`docs/route-a-plan.md` §8 风险第 1 条、`docs/dev/CONVENTIONS.md` 坑 #9/#12

## 背景

Shell 没有"告诉我用户选了什么文件"的官方 API。QuickLook 的 C# 实现
（`QuickLook.Native/Shell32.cpp` → `GetSelectedInternal`）走的是
`CLSID_ShellWindows → IShellWindows → IServiceProvider → IShellBrowser → IShellView → IFolderView::GetSelection`。
本项目要复刻这条路径，但有几个在文档里查不到、只能实测的坑。

## 实测事实（2026-10-29，Win10 19045，`comtest` 诊断程序 + P/Invoke 探针）

1. **`IShellWindows` 每条注册项的 HWND 是 `ShellTabWindowClass`，不是 `CabinetWClass` frame。**
   通过 `Item(i) → cast<IServiceProvider> → QueryService(SID_SShellBrowser) → IOleWindow::GetWindow()`
   反查出来的 HWND 是那个标签的子窗口；`GetAncestor(hwnd, GA_ROOT)` 才等于资源管理器主窗口。
   实测：`0x10F6C`（tab）→ root `0x20F3C`（CabinetWClass，`parent=0x0`）。
2. **`FindWindowSW(locator=frame HWND)` 不可用。** 传任意 frame HWND 都解析到 `0xC08D8`
   （桌面的那一条），拿回来的选区属于桌面，`GetDisplayName` 返回 `E_INVALIDARG`（0x80070057）。
   这正是最初"选中项永远是空的"的直接原因——绕过它以后当天就通了。
3. **空选区是常态，不是错误。** `IFolderView2::GetSelection(..)` 在选区为空时返回
   `S_FALSE`（`HRESULT=1`）或 `0x80070490`（`ERROR_NOT_FOUND`，视图还没建好）。
   必须按"没选中"处理，不能当异常抛出，更不能因此拦住空格键。
4. **Win11 多标签**：一个 frame 下有多个 `ShellTabWindowClass`，只有一个可见。
   `IShellWindows` 是**按 frame 注册共享的**，所以取到非活动标签的选区是真实存在的故障
   （PowerToys 的 `microsoft/PowerToys#22641` 就是这个问题）。
5. **`CoCreateInstance(CLSID_ShellWindows)` 需要足够的完整性级别。**
   同一个 exe，放在 DSH 工作目录里跑 → 进程令牌 `il_rid=4096`（Medium）→ `0x80070005`；
   复制到 `D:\` 或 `C:\Windows\Temp\` → `il_rid=12288`（High）→ 正常。
   这是宿主沙箱的行为，不是代码问题（见 CONVENTIONS 坑 #9）。

## 决策

### 1. 逐条枚举，不猜

```text
CoCreateInstance(CLSID_ShellWindows) -> IShellWindows
for i in 0..Count():
    Item(VT_I4(i)) -> cast<IServiceProvider> -> QueryService(SID_SShellBrowser)
        -> IOleWindow::GetWindow()  // 记录每个 browser 的 HWND
```

`CLSID_ShellWindows = {9BA05972-F6A8-11CF-A442-00A0C90A8F39}` 与
`SID_SShellBrowser = {000214E2-0000-0000-C000-000000000046}` 在 `windows` crate 里**没有导出**，
本模块手写 GUID。

### 2. 两级匹配（`query_by_hwnd`）

1. **精确命中**：目标 HWND 本身就等于某个 browser 的 HWND（目标直接是标签窗口）→ 用它，`Source::Frame`。
2. **同根命中**：`GetAncestor(browser, GA_ROOT) == GetAncestor(target, GA_ROOT)` → `Source::Tab`。
   多个候选时按 `!IsWindowVisible(hwnd)` 排序（**可见的优先**）并逐个尝试，直到某个返回非空选区。

不取第一个匹配项的原因：多标签场景下第一个往往是非活动标签（空选区）。

### 3. 空选区一路降级，不阻塞按键

`source=none count=0` 是合法结果。上层（`app::on_hotkey`）在无选中项时：
窗口开着就收起，否则什么都不做。**绝不因为"读不到选区"让空格键失效**。

### 4. 第三/第四级兜底留到 M2

- 三级：UI Automation `SelectionPattern`（50–200 ms，带超时）
- 四级：鼠标位置命中测试取第一项

M1 只做 Shell COM 两级，因为它们覆盖了 99% 的"在资源管理器里选中文件"的场景，
且是唯一毫秒级的路径。UIA 的延迟对按键路径不可接受（见 ADR-0005 的降级原则）。

## 后果

好的方面：

- 不需要 C++ 辅助 dll：QuickLook 为此单独维护 `QuickLook.Native32/64/Arm64`，
  本项目纯 `windows` crate 就够（同一份代码出 x64/ARM64）。
- 多标签场景可用（PowerToys 踩过的坑在这里是同根匹配 + 可见优先）。
- 端到端实测通过：`--probe-selection-hwnd 0x14082C` → `source=tab count=1 first=...8mp_3840x2160.jpg`。

代价与限制：

- 每次探测都要 `CoCreateInstance` + 遍历（实测 1 个 browser 时 <1 ms，10+ 个时需实测）。
  M2 会加"进程内缓存 IShellWindows 指针"，但缓存 COM 指针有生命周期风险，先不优化。
- 只对走 Shell 的文件管理器有效（Explorer、Files 等）；第三方管理器（DOpus、Everything）
  有自己的 API，白名单里已列出来但通道不同，M2 再补。
- **必须在 STA 线程上调用**（本项目的 UI 线程）。放到线程池里调，STA 封送会让结果悄悄变空。

## 备选方案为什么不用

- **`FindWindowSW`（用来定位资源管理器窗口）**：实测解析错误（见事实 2），直接出局。
  —— 但**桌面例外**，见下面的修订。
- **UI Automation**：50–200 ms 且要跨进程遍历树，作为主路径太慢；保留为兜底。
- **`SHGetFileInfo` / 剪贴板**：拿不到"当前选区"语义。
- **`IFolderView2::GetSelectionMarkedItem` 等变体**：只给索引不给路径，还要额外的 `IShellItem` 转换，
  相比 `GetSelection` + `GetItemAt` 没有收益。

## 修订（2026-10-30，0.1.4）：桌面走 `SWC_DESKTOP`

用户实测反馈"桌面上的图片按空格没反应"。日志是
`HOTKEY vk=0x20 fg=WorkerW hook_to_handler=0ms source=none count=0`（连 14 次），
说明钩子正常、键被吞了，问题在**取不到选区**。用独立诊断程序把 `IShellWindows`
每条注册项经 `IOleWindow::GetWindow()` 反查成 HWND 之后真相出来了：

- 桌面前台时 `IShellWindows count=3`，**三条全是已打开的文件夹窗口，没有"桌面"那一条**；
  按 HWND 同根匹配的 `query_by_hwnd` 因此永远没有候选（之前写的 `is_desktop_class` 特判无处生效）。
- `FindWindowSW(…, SWC_DESKTOP, …, SWFO_NEEDDISPATCH)` 能稳定拿到桌面 browser，
  之后可以复用同一条 `IServiceProvider → IShellBrowser → QueryActiveShellView → IFolderView2 → GetSelection` 链路。

所以第 1 条决策补一个分支：**前台根窗口类名是 `Progman`/`WorkerW` 时走 `SWC_DESKTOP`，
其余走 `IShellWindows` 枚举**；`Source` 增加 `Desktop`（日志 `source=desktop`）。

实现要点：

- `SWC_DESKTOP` 在 `windows 0.62.2` 的 `Win32/UI/Shell/mod.rs:55807` 有导出；
  该 crate **没有** `SWFO_FOREGROUND`（别照 QuickLook 的 C++ 抄）。
- `FindWindowSW` 会间歇性返回 S_OK + 空指针（`windows` crate 里是 `Err(code=0x0)`，
  看起来像"成功但没用"）→ 3 轮 × 2 种 flags × 15 ms 重试。
- 桌面项多是真实文件，`SIGDN_FILESYSPATH` 可用；回收站等虚拟项会 `E_INVALIDARG`，
  由"`PARENTRELATIVEPARSING` + 文件夹路径 join + `exists()`"兜底跳过。
- 桌面这一条**是否出现在 `IShellWindows` 里不稳定**（早期一次诊断里 11–12 条中是含桌面的），
  所以不要依赖枚举，直接按前台类名分流。

另一个必须记住的约束：**`WM_COPYDATA` 处理期间不能调 COM**（`0x8001010D`
RPC_E_CANTCALLOUT_ININPUTSYNCCALL）。第二实例转交的请求现在只排队，
再 `PostMessage(WM_QLR_INVOKE)` 延后到普通队列消息里处理。真实按空格不受影响
（钩子线程 `PostMessage` 过来的，本就不是输入同步呼叫）。
