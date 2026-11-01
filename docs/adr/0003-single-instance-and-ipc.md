# ADR-0003：单实例用命名互斥体 + `WM_COPYDATA` 转交，暂时不上命名管道

- 状态：已采纳
- 日期：2026-10-29
- 相关：`docs/dev/ARCH.md` §5

## 背景

"按空格预览"是常驻进程模型：第一个实例常驻，之后每次按空格都是**新进程**去请求老实例干活。需要三件事：

1. 判断"是否已有实例"；
2. 找到那个实例并通信；
3. 通信语义要能承载"请求 + 结果"。

参考实现（上游 QuickLook）用 `Mutex("QuickLook.App.Mutex")` + 命名管道 `QuickLook.App.Pipe.<SID>`，管道服务端要额外线程和重连逻辑。

## 决策

M1 采用**最小可用**组合：

- 单实例：`CreateMutexW(Local\QLRust.Instance)` + `GetLastError() == ERROR_ALREADY_EXISTS`，`InstanceGuard` 的 `Drop` 关句柄。
- 定位：`FindWindowW("QLRust.HostWindow")`（隐藏的宿主窗口，创建就存在）。
- 通信：`SendMessageW(WM_COPYDATA)`，payload 是 JSON 的 `Request{action, path}`，带魔数 `0x514C_5253` 校验，上限 64 KiB。

## 理由

1. **零线程**：`WM_COPYDATA` 由目标窗口的消息泵同步处理，不需要管道服务端线程、不需要重连与超时管理。
2. **自动同步**：`SendMessageW` 返回时对方已经处理完，调用方可以打印真实耗时（实测 0 ms，转发总耗时 37–40 ms 全在进程创建上）。
3. **双方都有消息泵**：宿主本来就跑 Win32 消息循环，`WM_COPYDATA` 是"顺便"的能力。
4. **`Local\` 前缀**：限制在登录会话内，避免多用户/多会话互相顶。

## 后果

- **只适合"一次性小请求"**：64 KiB 上限、无流式响应、无背压。M3 起若要"边解码边推送进度"，必须换命名管道（候选 `interprocess 2.4.7`）或共享内存。
- 换协议时**窗口类名与魔数是兼容面**：老版本进程遇到新版本请求要能优雅忽略（当前：魔数不匹配 → WARN 后忽略）。
- 单实例互斥体持有期间崩溃会导致"看起来没实例但互斥体还在"——靠内核在进程退出时自动释放，无需额外清理。

## 备选

- **命名管道（上游做法）**：能力更强，但要服务端线程 + 重连 + 超时，M1 不值得。
- **`RegisterWindowMessage` + `PostMessage`**：异步、无返回，无法确知对方处理结果。
- **`single-instance` crate**：2021 年后停维护，自己写 30 行更可控。
