# ADR-0004：基础设施零依赖——自研日志/错误/时间，不上 `tracing`/`anyhow`/`chrono`

- 状态：已采纳
- 日期：2026-10-29
- 相关：`docs/dev/ARCH.md` §8、`qlr-core/src/{log,error}.rs`

## 背景

本项目的第一目标就是"更快更小"：冷启动 56 ms、常驻 7.6 MB、单 exe 0.27 MB，这三条预算是硬指标（`docs/route-a-wbs.md` §7 A8.5）。

社区默认组合是 `tracing` + `tracing-subscriber` + `anyhow` + `chrono`。这套东西在服务端项目里是标准配置，但在这里要逐个掂量。

## 决策

基础设施全部手写，`qlr-core` **零第三方依赖**（只有 `serde`/`serde_json` 用于配置与 IPC）：

| 需求 | 自研实现 | 位置 |
|---|---|---|
| 文件日志（级别/轮转/UTC 时间） | ~200 行，`civil_from_days` 手写日期换算 | `qlr-core/src/log.rs` |
| 错误类型 | 手写 `enum Error{Io,Json,Platform,Config,Message}` | `qlr-core/src/error.rs` |
| 版本号/启动横幅 | `env!("CARGO_PKG_VERSION")` | `qlr-core/src/version.rs` |
| 字符串转宽字符 | `to_wide` / `fill_buffer` | `qlr-platform/src/wide.rs` |

## 理由

1. **`tracing` 的初始化成本与二进制约 300–500 KB**，对一个 0.27 MB 的 exe 是翻倍级开销；而我们需要的能力只是"带级别写文件 + 轮转"。
2. **`anyhow` 的 `Box<dyn Error>` 会丢失 `HRESULT`**。Win32 排错必须看到 `0x80070005` 这种码，所以错误类型要能原样携带。
3. **`chrono` 只为格式化一个 UTC 时间戳**：`civil_from_days` 是 20 行教科书算法，且日志只需要 UTC（用户看时间靠行首相对耗时更有用）。
4. **可测性**：自研的部分能跨平台单测（core 13 个测试全在没有 Windows 的假设下写），第三方则依赖其自身质量。

## 后果

- 日志功能克制：**没有** 结构化字段、没有 span、没有异步 appender、没有多输出目标。需要时再加，不要为了"将来可能"提前引依赖。
- 轮转策略简单（单文件 4 MB → `.1`），不做按天归档。
- `Drop`/panic 路径下的日志写失败必须静默（不能在日志里再崩一次），因此 `log::write` 返回 `()`。
- 手工维护错误枚举：新增错误类别时要同时改 `Display` 与构造器。

## 何时该推翻这个决策

出现下列任一情况时，重新评估引入第三方库：

- 需要结构化日志接 ELK/Prometheus 一类后端；
- 需要多线程 span 追踪（M2 worker 线程起来后若排查困难）；
- 错误类型超过 15 个变体、`Display` 手写维护成本明显；
- 引入 `wgpu`/`wasmtime` 等重依赖后，几百 KB 的日志库不再是体积瓶颈。

## 备选

- `log` + `simplelog`：轻，但仍是两个 crate 且格式化能力不比自己写多。
- `tracing` 全家桶：能力过剩，体积与初始化成本不合算。
- `thiserror`：宏生成 `Display`，省 30 行；但会把"错误分类"藏在宏里，且与手写 `platform()` 构造器风格冲突。当前规模手写更直白。
