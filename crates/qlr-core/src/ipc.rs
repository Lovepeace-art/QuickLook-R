//! 进程间协议：第二个实例怎么把"预览某个文件"的请求交给常驻实例。
//!
//! 选型（见 `docs/adr/0002-single-instance-and-ipc.md`）：**命名互斥体 + `WM_COPYDATA`**。
//! 不用命名管道，理由：
//! 1. `WM_COPYDATA` 由内核做跨进程封送，一行 `SendMessage` 就够，零额外依赖（A8.5 装载裁剪）；
//! 2. 数据量小（一个路径 + 一个动作），不需要流式；
//! 3. 阻塞语义天然给了"对方已收到"的确认（`SendMessage` 返回即对方处理完）。
//!
//! 约束：载荷必须是 UTF-8 JSON，且不超过 [`MAX_PAYLOAD`]（超过就退化成"只切窗口"）。

use serde::{Deserialize, Serialize};

/// 常驻实例的隐藏宿主窗口类名（第二个实例靠它 `FindWindowW` 找人）。
pub const HOST_WINDOW_CLASS: &str = "QLRust.HostWindow";
/// 单实例互斥体名（`Local\` 前缀 = 当前会话，避免多用户/多会话互相锁）。
pub const INSTANCE_MUTEX: &str = "Local\\QLRust.Instance";
/// 自定义消息：`WM_APP + 0x51`。
pub const WM_QLR_INVOKE: u32 = 0x8000 + 0x51;
/// 热键钩子命中：`WM_APP + 0x52`。`wparam` = 虚拟键码，`lparam` = 按下时刻的 tick（低 32 位）。
pub const WM_QLR_HOTKEY: u32 = 0x8000 + 0x52;
/// 托盘回调消息：`WM_APP + 1`。
pub const WM_TRAY_CALLBACK: u32 = 0x8000 + 1;
/// 渲染完成通知：`WM_APP + 0x53`。worker 线程渲染完就把它 `PostMessage` 给宿主窗口，
/// UI 线程收到后从结果槽里取 `Canvas` 上屏。**不携带数据**（`PostMessage` 只能带两个整数），
/// 真正的载荷放在 `app` 的结果槽里，靠代际号（generation）判新旧。
pub const WM_QLR_RENDER: u32 = 0x8000 + 0x53;
/// Esc：收起预览（由键盘钩子投递，见 `qlr_platform::hotkey`）。
pub const WM_QLR_ESCAPE: u32 = 0x8000 + 0x54;
/// 滚轮：`wparam` = 滚轮增量（一格 ±120）。由鼠标钩子投递。
pub const WM_QLR_WHEEL: u32 = 0x8000 + 0x55;
/// 设置窗口保存完毕 → 常驻实例（1.0.7）。
///
/// 设置窗是**独立的顶层窗口**，但它不自己热更新钩子：钩子的绑定表在平台层，
/// 而两个窗口本来就同进程，所以走窗口消息而不是跨进程 IPC。
/// 分工是"设置窗负责写 `config.json`，宿主负责读配置并重装绑定"。
pub const WM_QLR_SETTINGS_SAVED: u32 = 0x8000 + 0x56;
/// 跟随线程发现"资源管理器选中项变了"（1.7.6）。
///
/// 为什么需要它：以前只有 400 ms 的 UI 计时器去取跟随线程的结果，最坏情况是
/// "线程刚查完 → 计时器刚过"≈ 400 ms 白等。现在线程一发现变化就 `PostMessage`，
/// UI 立刻切内容（计时器退化成兜底/预热用）。**不携带数据**，路径在跟随槽里。
pub const WM_QLR_FOLLOW: u32 = 0x8000 + 0x57;
/// 窗口尺寸稳下来了，去把系统预览处理器的内容子窗口对齐到新尺寸（1.7.9）。
///
/// 为什么必须**投递**而不是在 `WM_SIZE` 里直接干：`SetWindowPos` 派发的 `WM_SIZE`
/// 落在输入同步呼叫（input synchronous call）里，那时调
/// `IPreviewHandler::SetWindow` / `SetRect` 会被系统直接拒绝
/// （`RPC_E_CANTCALLOUT_ININPUTSYNCCALL`, 0x8001010D）。
/// 同理，`WM_LBUTTONUP`（拖边结束）也不能直接干，所以 `finish_resize` 也投这条。
pub const WM_QLR_SHELLFIT: u32 = 0x8000 + 0x58;
/// `COPYDATASTRUCT.dwData` 魔数，用来识别"这是我们自己的载荷"。
pub const COPYDATA_MAGIC: usize = 0x514C_5253; // "QLRS"
/// 载荷上限：路径最长 32K 字符的极端情况也够用，同时挡住异常大的数据。
pub const MAX_PAYLOAD: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    /// 有路径就预览它，没有就切换当前窗口。
    Toggle,
    /// 显示（无路径时显示上一次的文件）。
    Show,
    /// 隐藏。
    Hide,
    /// 交给正在运行的实例去退出（`Exit` 由常驻实例自己走清理流程）。
    Quit,
    /// 探活，用于测试与 `--ping`。
    Ping,
    /// 模拟一次空格热键（等价于钩子命中，但绕过钩子）。只给冒烟测试用，
    /// 因为钩子会过滤掉 `SendInput` 合成出来的按键，自动化里没法真按空格。
    HotkeyProbe,
    /// 翻页（目前只有 PDF 有页的概念）。真人按 PageDown/PageUp 走钩子旁听，
    /// 这两个是给自动化与文档示例用的等价入口。
    PageNext,
    /// 上一页。
    PagePrev,
    /// 放大一档 / 缩小一档 / 复位到适应窗口。
    ZoomIn,
    ZoomOut,
    ZoomReset,
    /// 滚轮缩放：`delta` 是 `WHEEL_DELTA`(120) 的整数倍，正数放大。
    Wheel {
        delta: i32,
    },

    /// 平移 (dx, dy) 个"步"（1 步 ≈ 屏高的 1/12）。只给诊断/自动化用，
    /// 真人按上下方向键走钩子旁听。
    Pan {
        dx: i32,
        dy: i32,
    },

    /// 文本滚动 `delta` 行（正数向下）。只给诊断/自动化用：
    /// 键盘钩子会过滤掉 `SendInput` 合成出来的按键，脚本里按不出真的方向键。
    Scroll {
        delta: i32,
    },

    /// 打开设置窗口（托盘菜单「设置…」与 `--settings` 都走这里）。
    ///
    /// 设置窗不在这里改任何东西：它自己读写 `config.json`，改完通过
    /// `QLR_SETTINGS_SAVED` 消息把新配置交给常驻实例热更新。
    Settings,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub action: Action,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl Request {
    pub fn new(action: Action) -> Self {
        Self { action, path: None }
    }

    pub fn with_path(action: Action, path: impl Into<String>) -> Self {
        Self {
            action,
            path: Some(path.into()),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| br#"{"action":"Ping"}"#.to_vec())
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_PAYLOAD {
            return None;
        }
        serde_json::from_slice(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let r = Request::with_path(Action::Toggle, r"C:\Windows\Web\Wallpaper\Windows\img0.jpg");
        let back = Request::from_bytes(&r.to_bytes()).expect("应当能解析回来");
        assert_eq!(r, back);
    }

    #[test]
    fn request_without_path_roundtrip() {
        let r = Request::new(Action::Quit);
        let back = Request::from_bytes(&r.to_bytes()).unwrap();
        assert_eq!(back.path, None);
        assert_eq!(back.action, Action::Quit);
    }

    #[test]
    fn oversized_payload_is_rejected() {
        let big = vec![b' '; MAX_PAYLOAD + 1];
        assert!(Request::from_bytes(&big).is_none());
    }

    #[test]
    fn garbage_is_rejected_without_panic() {
        assert!(Request::from_bytes(b"\x00\xff not json").is_none());
        assert!(Request::from_bytes(b"").is_none());
    }
}
