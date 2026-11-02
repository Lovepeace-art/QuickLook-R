//! 全局键盘钩子（WBS A1.5）。
//!
//! 为什么用 `WH_KEYBOARD_LL` 而不是 `RegisterHotKey`：QuickLook 的交互是
//! "选中文件后按空格、松手收起"，需要**吞掉**这个按键（否则 Explorer 会收到空格），
//! 而 `RegisterHotKey` 只能通知、不能拦截。`Raw Input` 更轻量但同样不能吞键。
//!
//! 设计要点（对应 `docs/adr/0005-global-hotkey.md`）：
//!
//! 1. **钩子装在独立线程上**，回调只做三件事：判注入、判前台窗口类名、`PostMessage`。
//!    绝不在回调里做 COM / 文件 IO / 加锁——`LowLevelHooksTimeout` 默认 300 ms，
//!    累计超时 10 次后第 11 次 Windows 会把进程**静默**从钩子链摘掉且不通知任何人。
//! 2. **心跳重建**：每 [`HEARTBEAT_MS`] 毫秒主动 `Unhook` + `SetWindowsHookEx` 一次。
//!    这是唯一能在"被系统摘钩"后自愈的办法（没有官方通知机制），顺带把超时计数清零。
//! 3. 回调必须运行在有消息循环的线程上，所以线程主体是 `MsgWaitForMultipleObjects`
//!    + `PeekMessageW` 的泵，`GetMessageW` 会阻塞住心跳。
//!
//! 白名单只放 shell 宿主窗口类。**刻意不含 `#32770`**：它是最普通的对话框类名，
//! 放进来会在各种安装程序/设置对话框里把空格键吃掉（QuickLook 收录它是因为它只认
//! 带 `DUIViewWndClassName` 子窗口的那一种，我们要判断子窗口就得在回调里 `EnumChildWindows`，
//! 那是 100 µs 级别以上的活，违反第 1 条）。
//!
//! 注意：本模块的统计量是**进程级全局**的，一个进程只允许装一个钩子（见 `start`）。

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::thread::JoinHandle;

use qlr_core::keymap::{Action, Binding};
use qlr_core::{log_debug, log_error, log_info, log_warn};
use windows::Win32::Foundation::{
    HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcessId, SetEvent};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetFocus, INPUT, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_MENU,
    VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::MsgWaitForMultipleObjects;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GA_ROOT, GUITHREADINFO, GetAncestor, GetClassNameW,
    GetForegroundWindow, GetGUIThreadInfo, GetWindowRect, GetWindowThreadProcessId, HHOOK,
    IsWindowVisible, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, PM_REMOVE, PeekMessageW, PostMessageW,
    QS_ALLINPUT, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_MOUSEWHEEL, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

const SCOPE: &str = "hotkey";

/// 心跳间隔：30 s。比 `LowLevelHooksTimeout`（300 ms）大两个数量级，
/// 目的是"发现被摘钩后能自愈"，不是"维持超时预算"。
pub const HEARTBEAT_MS: u32 = 30_000;
/// 心跳必须远大于 `LowLevelHooksTimeout`（300 ms），它管的是"自愈周期"不是"超时预算"。
const _: () = assert!(HEARTBEAT_MS >= 10_000);

/// 允许吞空格的前台窗口类（大小写不敏感）。
///
/// - `CabinetWClass` / `ExploreWClass`：资源管理器文件夹窗口
/// - `ShellTabWindowClass`：Win11 22H2+ 的多标签页
/// - `Progman` / `WorkerW`：桌面（选中桌面图标时）
/// - `Shell_TrayWnd` / `Shell_SecondaryTrayWnd` / `NotifyIconOverflowWindow`：
///   任务栏与托盘溢出区（1.0.8 补：焦点落在任务栏时按空格以前会被放行掉，用户
///   反馈"偶有空格失效"）
/// - `MultitaskingViewFrame`：Win+Tab 任务视图
/// - `dopus.lister` / `Everything` / `FilePilot` / `DeskBox` / `IDM`：常见第三方文件管理器
pub const SHELL_CLASSES: &[&str] = &[
    "CabinetWClass",
    "ExploreWClass",
    "ShellTabWindowClass",
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "NotifyIconOverflowWindow",
    "MultitaskingViewFrame",
    "dopus.lister",
    "Everything",
    "FilePilot",
    "DeskBox",
    "IDM",
];

/// `KBDLLHOOKSTRUCT.flags` 位（`windows` crate 里是 `KBDLLHOOKSTRUCT_FLAGS`）。
const LLKHF_INJECTED: u32 = 0x10;
const LLKHF_LOWER_IL_INJECTED: u32 = 0x02;
/// `MSLLHOOKSTRUCT.flags` 位（鼠标事件的注入标记）。
const LLMHF_INJECTED: u32 = 0x01;

// ---- 回调可见的全局状态 -------------------------------------------------
// 回调是裸函数指针，拿不到闭包环境，所以配置走静态量。写入只发生在 `start` 里
// （钩子线程启动之前），之后只读，因此 `Relaxed` 足够。
// 1.0.7 起改成"动作 → 绑定"的表（下标 = `Action::index`，值 = `Binding::pack`）：
// 用户可以在设置页改键，改完 `set_bindings` 热更新，钩子线程不需要重启。
static BINDINGS: [AtomicU32; Action::COUNT] = [const { AtomicU32::new(0) }; Action::COUNT];
static NOTIFY_HWND: AtomicIsize = AtomicIsize::new(0);
static NOTIFY_MSG: AtomicU32 = AtomicU32::new(0);
/// 预览窗的 HWND（0 = 还没创建）。滚轮只在光标落在这个窗口上时才处理。
static PREVIEW_HWND: AtomicIsize = AtomicIsize::new(0);
/// Esc 收起预览用的消息（`ipc::WM_QLR_ESCAPE`）。
static ESCAPE_MSG: AtomicU32 = AtomicU32::new(0);
/// 滚轮缩放用的消息（`ipc::WM_QLR_WHEEL`，wparam = WHEEL_DELTA 的倍数）。
static WHEEL_MSG: AtomicU32 = AtomicU32::new(0);
/// 滚轮事件计数（只用于退出时的统计行）。
static WHEELS: AtomicU32 = AtomicU32::new(0);
/// 滚轮要不要**放行**给预览区里的子窗口（1.7.7）。
///
/// 背景：鼠标钩子命中预览窗时会 `LRESULT(1)` 把滚轮吞掉（否则"背景程序的滚轮"也会被我们吃掉）。
/// 但 Word/Excel 的预览是**子窗口自己的滚动条**，吞掉就等于它们永远滚不动——用户反馈
/// "滚动栏都不可用"就是这个。接管输入的（PPT）继续吞（我们自己翻页），
/// 其余（Word/Excel…）一律放行，让处理器自己滚。
static WHEEL_PASS_THROUGH: AtomicBool = AtomicBool::new(false);

/// 按键是否仍处于按下状态（用来滤掉自动重复）。
static KEY_DOWN: AtomicBool = AtomicBool::new(false);

/// 预览窗是否开着。翻页键只在开着的时候才需要被观测（否则会白占钩子回调）。
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// 旁听类动作：**不吞键**（用户在资源管理器里仍要能用它们移动选中项/打字），
/// 只把规范化后的键码告知 UI。
const WATCHED: [Action; 6] = [
    Action::PageNext,
    Action::PagePrev,
    Action::ScrollUp,
    Action::ScrollDown,
    Action::ZoomIn,
    Action::ZoomOut,
];

static CALLBACKS: AtomicU32 = AtomicU32::new(0);
static SWALLOWED: AtomicU32 = AtomicU32::new(0);
static REINSTALLS: AtomicU32 = AtomicU32::new(0);
static PASSED_OTHER_CLASS: AtomicU32 = AtomicU32::new(0);
/// 因为"前台正在输入文字"而放行的按键数（1.8.3 T02 的验收依据：
/// 用户在桌面改名时按空格，这个数应当 > 0；如果它一直是 0，说明判据没生效）。
static PASSED_TEXT_FOCUS: AtomicU32 = AtomicU32::new(0);
/// "前台在打字"是靠哪一级判据认出来的：1 = caret，2 = 焦点窗口类名，0 = 还没认过。
static TEXT_FOCUS_LEVEL: AtomicU32 = AtomicU32::new(0);

/// 钩子运行统计，给日志/冒烟测试看。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub callbacks: u32,
    pub swallowed: u32,
    pub reinstalls: u32,
    pub passed_other_class: u32,
    /// 被我们吞掉的滚轮事件数（只有光标在预览窗上时才会计数）。
    pub wheels: u32,
    /// 因为"前台在输入文字"而放行的按键数（1.8.3）。
    pub passed_text_focus: u32,
    /// 上次识别出"在打字"用的是哪一级判据（1=caret，2=类名，0=未触发）。
    pub text_focus_level: u32,
}

pub fn stats() -> Stats {
    Stats {
        callbacks: CALLBACKS.load(Ordering::Relaxed),
        swallowed: SWALLOWED.load(Ordering::Relaxed),
        reinstalls: REINSTALLS.load(Ordering::Relaxed),
        passed_other_class: PASSED_OTHER_CLASS.load(Ordering::Relaxed),
        wheels: WHEELS.load(Ordering::Relaxed),
        passed_text_focus: PASSED_TEXT_FOCUS.load(Ordering::Relaxed),
        text_focus_level: TEXT_FOCUS_LEVEL.load(Ordering::Relaxed),
    }
}

/// 前台窗口类名是否属于"可以吞空格"的 shell 类。
pub fn is_shell_class(name: &str) -> bool {
    let name = name.trim();
    SHELL_CLASSES
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(name))
        && !name.is_empty()
}

/// 读取前台窗口类名。失败返回 `None`（回调据此放行按键）。
pub fn foreground_class() -> Option<String> {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_invalid() {
            return None;
        }
        let mut buf = [0u16; 128];
        let len = GetClassNameW(fg, &mut buf);
        if len <= 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// 前台窗口是否属于**本进程**（预览窗、设置窗）。
///
/// 为什么需要它（1.0.8）：预览窗是 `WS_EX_NOACTIVATE`，正常不会成为前台，
/// 但设置窗是可激活的；用户开着设置窗按空格时，前台是我们的窗口、又不在
/// `SHELL_CLASSES` 里，以前会被当成"别的程序"直接放行——表现就是"空格偶有失效"。
pub fn foreground_is_ours() -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_invalid() {
            return false;
        }
        let mut pid = 0u32;
        let _ = GetWindowThreadProcessId(fg, Some(&mut pid));
        pid != 0 && pid == GetCurrentProcessId()
    }
}

/// 前台是不是"会放预览的那类窗口"（资源管理器 / 桌面 / 几个常见文件管理器）。
///
/// 给"跟随选中项"轮询用（1.0.10）：每 400 ms 都要决定要不要问一次 COM 拿选中项，
/// 而 `IShellWindows` 那套调用在慢机器上要几毫秒——先看类名能省掉绝大部分。
pub fn foreground_is_shell() -> bool {
    foreground_class().is_some_and(|name| is_shell_class(&name))
}

/// 前台是不是"正在输入文字"（1.8.3 T02）。
///
/// 报障：桌面选中文件夹按 F2 改名，输入法**打不出字**。根因是空格键被空格热键吞了
/// （重命名框里打空格就是空格字符）。
///
/// 两级判据，从便宜到贵：
/// 1. `GetGUIThreadInfo(0, ...)`（0 = 前台线程）看 `hwndCaret` / `GUI_CARETBLINKING`；
/// 2. Win11 桌面的就地重命名可能是 DirectUI 自绘、拿不到 `hwndCaret`，退一步看
///    **焦点窗口类名**——但只认真正的编辑框类，绝不放 `DirectUIHWND`/`SysListView32`
///    （那两个在资源管理器里永远处于焦点，会把所有预览都禁掉）。
///
/// 钩子里调用，必须快且**绝不阻塞**：`GetGUIThreadInfo` 读的是内核侧共享数据，
/// 不做 `SendMessage`；任何失败一律当"不在打字"（宁可吞键，也不能让钩子超时被摘）。
fn foreground_is_text_entry() -> bool {
    unsafe {
        // 级 1：caret 是否存在于前台线程。
        // `cbSize` 必须手填：`Default` 出来是 0，`GetGUIThreadInfo` 会直接返回失败，
        // 而且这个失败**看起来像"前台没在打字"**，极难发现。
        let mut gti = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(0, &mut gti).is_ok() && !gti.hwndCaret.is_invalid() {
            TEXT_FOCUS_LEVEL.store(1, Ordering::Relaxed);
            return true;
        }
        // 级 2：焦点窗口类名（只认编辑框）。
        let focus = GetFocus();
        if !focus.is_invalid() {
            let mut buf = [0u16; 64];
            let n = GetClassNameW(focus, &mut buf);
            if n > 0 {
                let name = String::from_utf16_lossy(&buf[..n as usize]);
                if matches!(
                    name.as_str(),
                    "Edit" | "RichEdit20W" | "RichEdit50W" | "RichEditD2DPT"
                ) {
                    TEXT_FOCUS_LEVEL.store(2, Ordering::Relaxed);
                    return true;
                }
            }
        }
    }
    false
}

/// 重放标记，放在 `KEYBDINPUT.dwExtraInfo` 里，便于以后要区分"自己发的键"。
///
/// QuickLook 用 `0x514C5452`（"QLTR"），这里沿用同一个习惯值。
pub const REPLAY_MARKER: usize = 0x514C_5452;

/// 把被吞掉的按键"还回去"。
///
/// 钩子为了拿到选中项必须先吞掉空格（回调里做不了 COM），但**如果最终什么也没显示**
/// （没有选中项、窗口也没开），这一下空格就凭空消失了——用户会觉得键盘坏了。
/// 所以这种情况必须 `SendInput` 重放一次。
///
/// 重放不会自激：钩子的注入过滤（`LLKHF_INJECTED`）对注入键一律 `CallNextHookEx`，
/// 即我们自己发的键会被放行、不会再被吞一次。
pub fn replay(vk: u32) {
    let mut inputs = [INPUT::default(); 2];
    for (index, flags) in [(0usize, 0u32), (1usize, KEYEVENTF_KEYUP.0)] {
        inputs[index].r#type = INPUT_KEYBOARD;
        inputs[index].Anonymous.ki = KEYBDINPUT {
            wVk: VIRTUAL_KEY(vk as u16),
            dwFlags: KEYBD_EVENT_FLAGS(flags),
            dwExtraInfo: REPLAY_MARKER,
            ..Default::default()
        };
    }
    let sent = unsafe { SendInput(&inputs, core::mem::size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        log_warn!(SCOPE, "重放按键失败：SendInput 只发出了 {sent} 个事件");
    }
}

/// 已安装的钩子。`Drop` 会停掉钩子线程并等它退出，所以请把它保存在
/// `run()` 的栈上，别让它被提前丢弃（丢了钩子就没了）。
pub struct HotkeyGuard {
    stop: isize,
    thread: Option<JoinHandle<()>>,
    installed: bool,
}

impl HotkeyGuard {
    /// 钩子线程是否成功装上（`false` 表示 `start` 返回后马上失败）。
    pub fn installed(&self) -> bool {
        self.installed
    }

    /// 主动停止（幂等）。退出路径上调用，保证钩子先摘掉再退进程。
    pub fn stop(&mut self) {
        if let Some(thread) = self.thread.take() {
            unsafe {
                let _ = SetEvent(HANDLE(self.stop as *mut core::ffi::c_void));
            }
            match thread.join() {
                Ok(()) => log_info!(SCOPE, "钩子线程已退出"),
                Err(_) => log_error!(SCOPE, "钩子线程 panic"),
            }
        }
    }
}

impl Drop for HotkeyGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 安装全局键盘钩子：按下 `trigger_vk` 且前台是 shell 类窗口时，
/// 向 `notify` 投递 `notify_msg`（`wparam` = 虚拟键码，`lparam` = 按下时的 tick）。
///
/// 返回 `Err` 表示钩子线程没能装上（通常是权限/策略问题），调用方应降级为
/// "只能用 CLI/托盘触发"而不是退出。
/// 告知钩子"预览窗是否开着"（翻页键只在开着时旁听）。
pub fn set_active(active: bool) {
    ACTIVE.store(active, Ordering::Relaxed);
}

/// 告知钩子"滚轮要不要放行给内容区的子窗口"（1.7.7）。
///
/// 只有"不接管输入"的系统预览（Word/Excel）才放行：它们的内容在自己的子窗口里，
/// 滚轮吞掉就没法滚。PPT 由我们接管翻页，仍然吞。
pub fn set_wheel_pass_through(pass: bool) {
    WHEEL_PASS_THROUGH.store(pass, Ordering::Relaxed);
}

/// 告知钩子预览窗的 HWND（滚轮命中判定要用它）。
/// 前台窗口是不是"我们预览窗的子孙窗口"。
///
/// 系统预览处理器（Word/Excel/PowerPoint）跑在独立进程里，键盘焦点落到它的子窗口上时
/// `foreground_is_ours()` 会返回 false —— 于是空格被当成"别的程序在按"而放行（用户报的
/// "office 预览时空格失效，要先点一下预览窗口"）。这里沿父链找到根窗口再比。
pub fn foreground_belongs_to_preview() -> bool {
    let preview = PREVIEW_HWND.load(Ordering::Relaxed);
    if preview == 0 {
        return false;
    }
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_invalid() {
            return false;
        }
        let mut root = fg;
        for _ in 0..8 {
            match GetAncestor(root, GA_ROOT) {
                parent if !parent.is_invalid() => {
                    if parent.0 as isize == preview {
                        return true;
                    }
                    if parent == root {
                        break;
                    }
                    root = parent;
                }
                _ => break,
            }
        }
        root.0 as isize == preview
    }
}

pub fn set_preview(hwnd: HWND) {
    PREVIEW_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
}

/// 告知钩子 Esc / 滚轮各自该投递哪条消息。
pub fn set_extra_messages(escape_msg: u32, wheel_msg: u32) {
    ESCAPE_MSG.store(escape_msg, Ordering::Relaxed);
    WHEEL_MSG.store(wheel_msg, Ordering::Relaxed);
}

/// 设定全部动作的绑定（下标 = `Action::index`，值 = `Binding::pack`）。
///
/// 设置页改键后就调它：钩子回调每次匹配都现读这张表，所以**不用重启钩子线程**
/// （重启会让用户正在按住的那一次按键丢掉，而且要等心跳，屏幕上看就是"改完没反应两秒"）。
pub fn set_bindings(packed: [u32; Action::COUNT]) {
    for (slot, value) in BINDINGS.iter().zip(packed) {
        slot.store(value, Ordering::Relaxed);
    }
    log_bindings();
}

/// 把当前绑定表打成一行日志（排查"我改了键怎么没生效"第一条要看的就是它）。
pub fn log_bindings() {
    let mut parts: Vec<String> = Vec::with_capacity(Action::COUNT);
    for action in Action::ALL {
        let binding = Binding::unpack(BINDINGS[action.index()].load(Ordering::Relaxed))
            .map(|b| b.text())
            .unwrap_or_else(|| "（未绑定）".to_string());
        parts.push(format!("{}={}", action.label(), binding));
    }
    log_info!(SCOPE, "按键绑定：{}", parts.join(" "));
}

pub fn start(notify: HWND, notify_msg: u32) -> windows::core::Result<HotkeyGuard> {
    if notify.is_invalid() {
        return Err(windows::core::Error::from(
            windows::Win32::Foundation::E_HANDLE,
        ));
    }
    // 没显式设过绑定就用默认值：命令行 `--render`、诊断路径都可能绕过配置加载。
    if BINDINGS[0].load(Ordering::Relaxed) == 0 {
        set_bindings(qlr_core::keymap::Hotkeys::default().packed());
    }
    NOTIFY_HWND.store(notify.0 as isize, Ordering::SeqCst);
    NOTIFY_MSG.store(notify_msg, Ordering::SeqCst);
    KEY_DOWN.store(false, Ordering::SeqCst);

    let stop = unsafe {
        CreateEventW(None, true, false, None).map_err(|e| windows::core::Error::from(e.code()))?
    };
    // HANDLE 里是裸指针，不是 Send；线程只传数值，进了闭包再还原。
    let stop_raw = stop.0 as isize;

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<windows::core::Result<()>>();
    let handle = std::thread::Builder::new()
        .name("qlr-hotkey".to_string())
        .spawn(move || hook_thread(HANDLE(stop_raw as *mut core::ffi::c_void), ready_tx))
        .map_err(|e| {
            log_error!(SCOPE, "创建钩子线程失败：{e}");
            windows::core::Error::from(windows::Win32::Foundation::E_FAIL)
        })?;

    // 等线程报告"装上了"再返回，否则 `installed()` 会撒谎。
    let installed = match ready_rx.recv_timeout(std::time::Duration::from_secs(3)) {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            log_error!(SCOPE, "键盘钩子安装失败：{e}");
            false
        }
        Err(_) => {
            log_error!(SCOPE, "等待钩子线程超时");
            false
        }
    };

    Ok(HotkeyGuard {
        stop: stop.0 as isize,
        thread: Some(handle),
        installed,
    })
}

fn hook_thread(stop: HANDLE, ready: std::sync::mpsc::Sender<windows::core::Result<()>>) {
    let hmod = unsafe { GetModuleHandleW(None) }
        .map(|h| HINSTANCE(h.0))
        .ok();
    let mut hook = match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), hmod, 0) } {
        Ok(hook) => {
            let _ = ready.send(Ok(()));
            hook
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    // 鼠标钩子：只为"预览窗开着时滚轮缩放"。装不上不影响键盘钩子（只是没有滚轮）。
    let mut mouse_hook = match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hmod, 0) }
    {
        Ok(h) => h,
        Err(e) => {
            log_warn!(SCOPE, "鼠标钩子安装失败（滚轮缩放不可用）：{e}");
            HHOOK::default()
        }
    };

    log_info!(
        SCOPE,
        "鼠标钩子已安装={}（滚轮缩放{}）",
        !mouse_hook.is_invalid(),
        if mouse_hook.is_invalid() {
            "不可用"
        } else {
            "可用"
        }
    );
    log_info!(
        SCOPE,
        "键盘钩子已安装 绑定={} 个 心跳={}ms 白名单={} 个类",
        Action::COUNT,
        HEARTBEAT_MS,
        SHELL_CLASSES.len()
    );
    log_bindings();

    loop {
        // 等的三件事：停止事件、窗口消息、心跳超时。
        let wait =
            unsafe { MsgWaitForMultipleObjects(Some(&[stop]), false, HEARTBEAT_MS, QS_ALLINPUT) };
        if wait == WAIT_OBJECT_0 {
            break;
        }
        if wait == WAIT_TIMEOUT {
            // 自愈：可能已被系统摘钩（无通知机制），重建一次。
            hook = reinstall(hook, hmod);
            if !mouse_hook.is_invalid() {
                unsafe {
                    let _ = UnhookWindowsHookEx(mouse_hook);
                }
                mouse_hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hmod, 0) }
                    .unwrap_or_default();
            }
            continue;
        }
        // 有消息：排空队列。钩子回调就在这里被系统调起。
        let mut msg = MSG::default();
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    unsafe {
        let _ = UnhookWindowsHookEx(hook);
        if !mouse_hook.is_invalid() {
            let _ = UnhookWindowsHookEx(mouse_hook);
        }
    }
    let s = stats();
    log_info!(
        SCOPE,
        "键盘钩子已卸载 callbacks={} swallowed={} reinstall={} passed={} wheels={} \
         passed_text_focus={} text_focus_level={}",
        s.callbacks,
        s.swallowed,
        s.reinstalls,
        s.passed_other_class,
        s.wheels,
        s.passed_text_focus,
        s.text_focus_level
    );
}

/// 低层鼠标钩子回调：**只在预览窗开着、且光标落在预览窗上时**处理滚轮。
///
/// 为什么非要钩子：预览窗是 `WS_EX_NOACTIVATE`，永远不拿焦点，`WM_MOUSEWHEEL`
/// 会被系统投给当前**焦点**窗口（也就是资源管理器），我们一条都收不到。
///
/// 只在命中预览窗时吞键，是为了不把"背景程序里的滚轮"一起吃掉。
unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code < 0 || !ACTIVE.load(Ordering::Relaxed) {
            return CallNextHookEx(None, code, wparam, lparam);
        }
        if wparam.0 as u32 != WM_MOUSEWHEEL {
            return CallNextHookEx(None, code, wparam, lparam);
        }
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        if info.flags & LLMHF_INJECTED != 0 {
            // 合成事件不处理（同键盘钩子的理由：防递归）。
            return CallNextHookEx(None, code, wparam, lparam);
        }

        let preview = PREVIEW_HWND.load(Ordering::Relaxed);
        if preview == 0 {
            return CallNextHookEx(None, code, wparam, lparam);
        }
        let hwnd = HWND(preview as *mut core::ffi::c_void);
        if !IsWindowVisible(hwnd).as_bool() {
            return CallNextHookEx(None, code, wparam, lparam);
        }
        let mut rect = windows::Win32::Foundation::RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return CallNextHookEx(None, code, wparam, lparam);
        }
        let pt: POINT = info.pt;
        if pt.x < rect.left || pt.x >= rect.right || pt.y < rect.top || pt.y >= rect.bottom {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        // 预览区里的内容由系统预览处理器自己滚（Word/Excel）：别吞，交给窗口自己。
        if WHEEL_PASS_THROUGH.load(Ordering::Relaxed) {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        // mouseData 高 16 位是有符号的滚轮增量（一格 = 120）。
        let delta = ((info.mouseData >> 16) & 0xFFFF) as u16 as i16;
        let msg = WHEEL_MSG.load(Ordering::Relaxed);
        let notify = NOTIFY_HWND.load(Ordering::Relaxed);
        if msg == 0 || notify == 0 {
            return CallNextHookEx(None, code, wparam, lparam);
        }
        let _ = PostMessageW(
            Some(HWND(notify as *mut core::ffi::c_void)),
            msg,
            WPARAM(delta as isize as usize),
            LPARAM(0),
        );
        WHEELS.fetch_add(1, Ordering::Relaxed);
        LRESULT(1)
    }
}
fn reinstall(old: HHOOK, hmod: Option<HINSTANCE>) -> HHOOK {
    unsafe {
        let _ = UnhookWindowsHookEx(old);
    }
    match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), hmod, 0) } {
        Ok(hook) => {
            let n = REINSTALLS.fetch_add(1, Ordering::Relaxed) + 1;
            log_debug!(SCOPE, "钩子心跳重建第 {n} 次");
            hook
        }
        Err(e) => {
            log_error!(SCOPE, "钩子心跳重建失败，继续用旧句柄：{e}");
            old
        }
    }
}

/// 读当前修饰键状态。回调里只在"这条绑定确实要求修饰"时才调用
/// （`GetAsyncKeyState` 每次约 0.1 µs，但钩子回调有 300 ms 硬预算，能省则省）。
/// 三个修饰键当前是否按下。
///
/// 1.8.3 起**无条件查全三个**。以前是"只查绑定里声明过的修饰键，没声明的当 false"，
/// 于是绑 `Space` 时按 `Ctrl+Space`（中文输入法切换）和 `Alt+Space`（系统菜单）
/// 都被当成命中并吞掉——钩子一吞，输入法就切不了、系统菜单也弹不出来。
fn modifiers_down() -> (bool, bool, bool) {
    let query = |vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY| unsafe {
        (GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000) != 0
    };
    let ctrl = query(VK_CONTROL) || query(VK_LCONTROL) || query(VK_RCONTROL);
    let alt = query(VK_MENU) || query(VK_LMENU) || query(VK_RMENU);
    // Shift 要单独查左右键：低层钩子拿到的是物理键码，左右各一个。
    let shift = query(VK_SHIFT) || query(VK_LSHIFT) || query(VK_RSHIFT);
    (ctrl, alt, shift)
}

/// 这条动作的绑定是否命中本次按键。
///
/// 绑定表在每次匹配时现读：设置页改键后立刻生效，不用重启钩子线程。
fn action_hits(action: Action, vk: u16) -> bool {
    let Some(binding) = Binding::unpack(BINDINGS[action.index()].load(Ordering::Relaxed)) else {
        return false;
    };
    if binding.vk != vk {
        return false;
    }
    let (ctrl, alt, shift) = modifiers_down();
    if action.ignores_shift() {
        // 放大/缩小例外：`=` 和 `+`、`-` 和 `_` 在物理上是同一个键（VK_OEM_PLUS /
        // VK_OEM_MINUS），严格比对会让用户"按 Shift+= 放大"失效（1.8.2 之前它其实
        // 能工作，是靠"没声明的修饰键一律当 false"这个 bug 意外实现的）。
        binding.matches_ignoring_shift(vk, ctrl, alt, shift)
    } else {
        binding.matches(vk, ctrl, alt, shift)
    }
}

/// 投一句"这个键被按了"给 UI 线程（wparam = 规范化后的键码）。
fn post_action(msg: u32, vk: u16, stamp: u32) -> bool {
    let notify = NOTIFY_HWND.load(Ordering::Relaxed);
    if notify == 0 {
        return false;
    }
    unsafe {
        PostMessageW(
            Some(HWND(notify as *mut core::ffi::c_void)),
            msg,
            WPARAM(vk as usize),
            LPARAM(stamp as isize),
        )
        .is_ok()
    }
}

/// 是否处于"数字输入模式"（顶栏页码框 / 设置页数字项在编辑中）。
///
/// 预览窗不激活、拿不到焦点，所以输入全靠钩子转发：见 `hook_proc` 里的分支。
static CAPTURE_DIGITS: AtomicBool = AtomicBool::new(false);

/// 开关数字输入模式。壳层进入/退出"编辑页码"时调用。
pub fn set_capture_digits(on: bool) {
    CAPTURE_DIGITS.store(on, Ordering::Relaxed);
}

/// 当前是否处于数字输入模式（1.8.3）。
///
/// 为什么要这个 getter：这个开关是**全局**的，而"关"的调用点分散在
/// 收起预览 / 关闭预览 / 取消编辑三处，漏一处就会出现"预览早就关了，全局还在吞数字键"
/// （用户在别的程序里打不出数字）。有了它就能在 smoke 脚本里断言"收起后必须是 false"。
pub fn capture_digits() -> bool {
    CAPTURE_DIGITS.load(Ordering::Relaxed)
}

/// 输入模式下要截获的键：0–9（主键盘与数字小键盘）、退格、回车、Esc。
fn is_capture_key(vk: u16) -> bool {
    matches!(vk, 0x30..=0x39) || matches!(vk, 0x60..=0x69) || matches!(vk, 0x08 | 0x0D | 0x1B)
}

/// 低层键盘钩子回调。**必须极快**（见模块文档第 1 条）。
unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code < 0 {
            return CallNextHookEx(None, code, wparam, lparam);
        }
        CALLBACKS.fetch_add(1, Ordering::Relaxed);

        let message = wparam.0 as u32;
        let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let vk = info.vkCode as u16;

        // 松开：清"按下"标记（自动重复判定依赖它），按键本身放行。
        if message == WM_KEYUP || message == WM_SYSKEYUP {
            KEY_DOWN.store(false, Ordering::Relaxed);
            return CallNextHookEx(None, code, wparam, lparam);
        }
        if message != WM_KEYDOWN && message != WM_SYSKEYDOWN {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        // ---- 数字输入模式（顶栏页码框 / 设置页数字项）----
        // 预览窗是 `WS_EX_NOACTIVATE`，永远拿不到键盘焦点，所以"在窗口里打字"只能
        // 借钩子：进入输入模式后把数字键、退格、回车、Esc 转给宿主并吞掉。
        // 只认这几类键，其余一律放行——避免"输入模式下别的键失灵"。
        // 这里**故意放在注入过滤之前**：输入模式是我们自己开的，合成按键也照收；
        // 而且捕获的键只转给宿主、不回放，不存在递归风险。
        if CAPTURE_DIGITS.load(Ordering::Relaxed)
            && is_capture_key(vk)
            && post_action(NOTIFY_MSG.load(Ordering::Relaxed), vk, GetTickCount())
        {
            SWALLOWED.fetch_add(1, Ordering::Relaxed);
            return LRESULT(1);
        }
        // 别的程序用 SendInput 合成出来的按键不处理（防递归/防自动化误触）。
        if info.flags.0 & (LLKHF_INJECTED | LLKHF_LOWER_IL_INJECTED) != 0 {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        // ---- 打开/关闭预览：吞键，且只在"前台是文件管理器/我们自己的窗口"时生效 ----
        // 这是"绝不抢别人的空格"的唯一保障：记事本里的空格必须还是空格。
        if action_hits(Action::Toggle, vk) {
            // 自动重复：不重复触发预览，也不吞（长按语义交给系统）。
            if KEY_DOWN.swap(true, Ordering::Relaxed) {
                return CallNextHookEx(None, code, wparam, lparam);
            }
            let ours = foreground_is_ours() || foreground_belongs_to_preview();
            let shell = foreground_class().is_some_and(|class| is_shell_class(&class));
            if !(ours || shell) {
                PASSED_OTHER_CLASS.fetch_add(1, Ordering::Relaxed);
                return CallNextHookEx(None, code, wparam, lparam);
            }
            // 1.8.3（T02）：前台**正在输入文字**时绝不吞键。
            // 报障场景：桌面上按 F2 给文件夹改名，空格被我们吞掉 → 输入法打不出字。
            // 只在 shell 场景检查（我们自己的窗口里没有"别人的编辑框"），
            // 这样多出来的两次轻量 API 调用只在真正需要时发生。
            if shell && foreground_is_text_entry() {
                PASSED_TEXT_FOCUS.fetch_add(1, Ordering::Relaxed);
                return CallNextHookEx(None, code, wparam, lparam);
            }
            if post_action(NOTIFY_MSG.load(Ordering::Relaxed), vk, GetTickCount()) {
                SWALLOWED.fetch_add(1, Ordering::Relaxed);
                return LRESULT(1);
            }
            return CallNextHookEx(None, code, wparam, lparam);
        }

        // 以下都只在预览窗开着时才有意义。
        if !ACTIVE.load(Ordering::Relaxed) {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        // ---- 关闭预览：吞键（否则别的程序里的 Esc 会被我们吃掉，所以看前台类）----
        if action_hits(Action::Close, vk) {
            let allowed = foreground_is_ours()
                || foreground_class().is_some_and(|class| is_shell_class(&class));
            if allowed {
                let msg = ESCAPE_MSG.load(Ordering::Relaxed);
                if msg != 0 && post_action(msg, vk, GetTickCount()) {
                    SWALLOWED.fetch_add(1, Ordering::Relaxed);
                    return LRESULT(1);
                }
            }
            return CallNextHookEx(None, code, wparam, lparam);
        }

        // ---- 浏览类动作：旁听，不吞（见 WATCHED 注释），投规范化键码 ----
        // 回调里不写日志：日志是磁盘 I/O，而钩子回调有 300 ms 硬预算
        // （超 10 次后第 11 次被系统静默摘钩），这里每按一次键都会走到。
        for action in WATCHED {
            if action_hits(action, vk) {
                post_action(
                    NOTIFY_MSG.load(Ordering::Relaxed),
                    action.canonical_vk(),
                    GetTickCount(),
                );
                break;
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_classes_match_case_insensitively() {
        assert!(is_shell_class("CabinetWClass"));
        assert!(is_shell_class("cabinetwclass"));
        assert!(is_shell_class("  CabinetWClass  "));
        assert!(is_shell_class("ShellTabWindowClass"));
        assert!(is_shell_class("Everything"));
    }

    #[test]
    fn unknown_classes_are_not_swallowed() {
        // #32770 是普通对话框类名，刻意不在白名单里：见模块文档。
        assert!(!is_shell_class("#32770"));
        assert!(!is_shell_class("Notepad"));
        assert!(!is_shell_class(""));
        assert!(!is_shell_class("   "));
        assert!(!is_shell_class("Chrome_WidgetWin_1"));
    }
}
