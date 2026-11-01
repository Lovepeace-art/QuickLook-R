# ADR-0005：全局热键用独立线程的 `WH_KEYBOARD_LL` + 心跳自愈

- 状态：已采纳
- 日期：2026-10-29
- 相关：`crates/qlr-platform/src/hotkey.rs`、`docs/dev/ARCH.md`、`docs/route-a-wbs.md` §6 A1.5

## 背景

核心交互是"在资源管理器里选中文件 → 按空格 → 立刻出预览窗 → 松手收起"。这条路径有两个硬要求：

1. 必须**看见**在别的进程（explorer）里按下的键；
2. 必须**吞掉**这个空格，否则 Explorer 自己会响应空格（在文件列表里空格是"切换选中"），
   预览窗和选中状态会互相打架。

候选方案只有三个：

| 方案 | 能看见全局按键 | 能吞键 | 代价 |
|---|---|---|---|
| `RegisterHotKey` | 能 | **不能** | 系统级独占，冲突时报注册失败 |
| Raw Input（`RIDEV_INPUTSINK`） | 能 | **不能** | 轻量，但只读 |
| `WH_KEYBOARD_LL` 低级钩子 | 能 | 能 | 回调跑在**我们的**消息循环里，超时会被系统摘钩 |

QuickLook 用的是第三种（`QuickLook/Helpers/GlobalKeyboardHook.cs`），本项目沿用。
这个方案的代价在微软文档里写得很清楚，也是这个 ADR 存在的唯一理由。

## 决策

用 `WH_KEYBOARD_LL`，但按下面四条约束实现：

### 1. 钩子装在独立线程，回调只做三件廉价的事

回调里允许做的只有：判注入标记、读前台窗口类名、`PostMessageW` 给自己的宿主窗口。
**绝不允许**：COM 调用、文件 IO、加锁、`EnumChildWindows`、日志格式化（`format!` 分配内存也可能触发锁）。

原因：`LowLevelHooksTimeout` 默认 **300 ms**（Win7 起）。回调超时会被系统计数，
**累计 10 次之后第 11 次 Windows 直接把进程从钩子链上摘掉**，此后钩子彻底失效，
而且**没有任何通知机制**（没有事件、没有返回值、没有 API 能查）。这是 by design，不是 bug。

顺带：改注册表把 `LowLevelHooksTimeout` 调大**不是**解法——社区（AHK 等）实测调大后
输入延迟变得不可接受，且系统对非法值的行为未文档化。本项目不走这条路。

### 2. 独立线程 + 自己的消息循环

低级钩子的回调在**安装钩子的那个线程**上被调用，那个线程必须有消息循环。
主线程的消息泵要处理窗口消息，不能被钩子拖住，所以钩子线程自己跑
`MsgWaitForMultipleObjects(stop_event, HEARTBEAT_MS, QS_ALLINPUT)` + `PeekMessageW`。
用 `MsgWaitForMultipleObjects` 而不是 `GetMessageW`，是为了让心跳超时可打断阻塞。

### 3. 心跳重建（30 s）

没有"我被摘钩了"的通知，就只能主动重装：每 `HEARTBEAT_MS = 30_000` 毫秒
`UnhookWindowsHookEx` + `SetWindowsHookExW` 一次。这既是自愈手段，也顺带清零超时计数。
代价是重装瞬间（微秒级）的按键会漏掉——相对"钩子静默死掉直到重启程序"完全可以接受。

`const _: () = assert!(HEARTBEAT_MS >= 10_000);` 把"心跳不能太频繁"写进编译期约束。

### 4. 前台类名白名单 + 注入过滤 + 自动重复过滤

- 白名单：`CabinetWClass` / `ExploreWClass` / `ShellTabWindowClass` / `Progman` / `WorkerW` /
  `dopus.lister` / `Everything` / `FilePilot` / `DeskBox` / `IDM`。
- **刻意不含 `#32770`**：它是最普通的对话框类名（安装程序、设置框都是），
  收录它会导致在很多无关界面里把空格吃掉。QuickLook 收录它是因为它只认"带
  `DUIViewWndClassName` 子窗口"的那一种，而判断子窗口要在回调里 `EnumChildWindows`，
  违反第 1 条，所以这里放弃这个场景。
- 注入过滤：`LLKHF_INJECTED(0x10)` / `LLKHF_LOWER_IL_INJECTED(0x02)` 的按键一律放行，
  避免和第三方重放（QuickLook 的 `SendInput` + `0x514C5452` 约定）互相打架。
- 自动重复：`WM_KEYDOWN` 只在 `KEY_DOWN.swap(true)` 返回 false 时才处理，
  按住空格不放不会连续触发预览。

### 5. 失败要降级，不能致命

`hotkey::start` 失败（比如另一个程序占着、或权限问题）只记 `ERROR` 日志，
程序继续跑，热键可通过托盘菜单和命令行触发。**绝不能因为热键装不上就退出**。

## 后果

好的方面：

- 保持了"选中文件按空格即出"这个核心手势，兼容 Explorer / 第三方文件管理器。
- 心跳自愈让"钩子半夜静默死掉"这类最难查的故障变成自恢复。
- 32 个单测 + 冒烟脚本能覆盖安装/卸载/统计，进程退出时钩子一定被卸载（`HotkeyGuard::drop`）。

代价与限制：

- **收不到提权窗口的按键**（UIPI）：以管理员身份运行的窗口里的空格拿不到。无解，除非自己也提权。
- 钩子对全系统按键有微秒级延迟影响，回调里的任何分配都可能被放大成输入卡顿——所以第 1 条是红线。
- 冒烟脚本无法模拟"真空格"（注入键被自己过滤掉），所以 `Action::HotkeyProbe` 这条测试专用入口存在，
  它只验证"钩子 → 消息 → 处理链"这段，不验证真实的键盘硬件路径。

## 备选方案为什么不用

- **`RegisterHotKey`**：不能吞键，Explorer 仍会响应空格。可以退一步做"不吞键"的版本，
  但那会改变产品的交互手感（预览窗和文件选中状态冲突），不采纳。保留为**热键安装失败时的降级开关**。
- **Raw Input**：同样不能吞键，且要注册到具体设备类，多设备场景更麻烦。
- **UI Automation 轮询**：延迟 50–200 ms 且要轮询，完全不适合按键路径（它是选中项通道的第三级兜底，见 ADR-0006）。
