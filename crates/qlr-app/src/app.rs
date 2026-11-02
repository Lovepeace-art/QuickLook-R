//! 应用编排：启动、单实例转交、窗口/托盘创建、消息泵、动作分派。
//!
//! 分层约定（见 `docs/dev/ARCH.md`）：本文件只做"决策"，所有 Win32 细节都在
//! `qlr-platform`；本文件里出现的 `unsafe` 仅限于窗口过程签名。
//!
//! 线程模型：**UI 单线程 + 一个常驻渲染线程**。
//! - UI 线程独占所有窗口、托盘、IPC；
//! - 渲染线程只做 `qlr_render::render_file`（纯 CPU，不碰 `HWND`），
//!   完成后把结果放进 `RESULT` 槽并 `PostMessage(WM_QLR_RENDER)` 通知 UI；
//! - 两者之间只靠"代际号 + 结果槽"通信，不做锁内回调，避免重蹈 `WM_PAINT` 递归死锁。
//! - 渲染线程是单例，但**不是永生的**（1.8.3）：它卡死在第三方 FFI 里回不来时，看门狗
//!   会换一条新的顶上去（`check_render_hang`），旧线程不 join 不 kill、自然烂掉。

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use qlr_core::ipc::{self, Action, Request};
use qlr_core::{
    Config, Level, format, log, log_debug, log_error, log_info, log_warn, paths, plugins, version,
};
use qlr_platform::{
    crash, dpi, hotkey, instance, integrity, selection, shellpreview, tray, win11, window,
};
use qlr_render::Canvas;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, KillTimer, PostQuitMessage, SetTimer, WM_ACTIVATE, WM_CLOSE, WM_COPYDATA,
    WM_DESTROY, WM_ERASEBKGND, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE,
    WM_MOUSEMOVE, WM_NCCALCSIZE, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SETFOCUS,
    WM_SIZE, WM_TIMER,
};

use crate::cli::{self, Command};
use crate::pagecache;
use crate::settings;

const SCOPE: &str = "app";
const PREVIEW_CLASS: &str = "QLRust.PreviewWindow";
/// 投影浮层的窗口类（1.0.10；分层窗口，永远贴在预览窗下面）。
const SHADOW_CLASS: &str = "QLRust.ShadowWindow";
const TRAY_ID: u32 = 1;

/// 托盘注册失败、还没告诉过用户（1.8.3）。
///
/// 为什么用"待办标记"而不是当场弹窗：托盘失败发生在 `State` 还没建好、预览窗
/// 还没显示的时候（裸启动），此刻既没有可以挂 toast 的窗口，也不该阻塞启动。
/// 所以只记一笔，等第一次真的把窗口显示出来时用顶栏 toast 说一次，然后清掉——
/// 每次打开都提示会把预览变成噪音。
static TRAY_FAILURE_NOTICE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 放大镜默认倍数与上下限（Shift + 滚轮每一格 ±1）。
const LENS_DEFAULT_ZOOM: f32 = 3.0;
const LENS_MIN_ZOOM: f32 = 1.5;
const LENS_MAX_ZOOM: f32 = 8.0;

/// 跟随选中项的轮询定时器（1.0.10）。
///
/// 为什么轮询而不是等通知：资源管理器的选中项只有 `IShellWindows` 能问，
/// 而它没有可订阅的变更事件（`DShellWindowsEvents` 只报窗口的开关，不报选中项）。
/// 400 ms 是"跟得上手"和"别把 CPU 拖起来"的折中（每次只是一次类名比较，
/// 前台不是资源管理器时连 COM 都不碰）。
const TIMER_FOLLOW: usize = 0x71;
const FOLLOW_INTERVAL_MS: u32 = 400;
/// 渲染看门狗（1.2.0）：250 ms 看一眼"这一帧跑多久了"。
const TIMER_WATCHDOG: usize = 0x72;
const WATCHDOG_INTERVAL_MS: u32 = 250;
/// 顶栏一次性提示的擦除定时器（1.7.4）。
const TIMER_TOAST: usize = 0x73;
/// 接管系统预览处理器输入的延时定时器（1.7.4）。
const TIMER_SHELL_INPUT: usize = 0x74;
/// 换文件时"旧内容还要盖多久"（1.7.6）：到点卸掉旧的，露出新的第一帧。
const TIMER_SHELL_SWAP: usize = 0x75;
/// 音频进度条刷新（1.7.9）：只在播放时开着，暂停/收起就杀掉。
const TIMER_AUDIO: usize = 0x76;
/// 进度条刷新间隔（毫秒）。250 ms 对"看进度条"足够顺，又不会为了它整窗重画。
const AUDIO_TICK_MS: u32 = 250;
/// 音频快进/快退步长（毫秒）：左右键一次 5 秒（与常见播放器的方向键粒度一致）。
const AUDIO_SEEK_STEP_MS: i64 = 5_000;
/// 视频逐帧播放的取帧定时器（1.7.9）。
const TIMER_VIDEO: usize = 0x77;
/// 视频取帧间隔（毫秒）。40 ms ≈ 25 fps：720p 解码一帧只要几毫秒，
/// 剩下的时间留给"到点该显示哪一帧"的判断（落后就跳帧，见 `video_tick`）。
const VIDEO_TICK_MS: u32 = 40;
/// 旧内容遮盖时长（毫秒）。实测 Word/PowerPoint 预览首帧在 120–260 ms 之间落地，
/// 取 260 让绝大多数情况"换的那一刻已经是新画面"。
const SHELL_SWAP_DELAY_MS: u32 = 260;

/// 轮询间隔（1.7.7）：等处理器"内容真的画出来"（见 `ShellPreview::visible_area`）。
///
/// 为什么不再死等固定 400 ms：Excel 的 `DoPreview` 实测要 **768 ms**，猜早了就露出占位页
/// （用户反馈"Excel 还有那个占位页"）。改成每 120 ms 看一眼，画出来就收尾，最多等 3 s。
const SHELL_POLL_MS: u32 = 120;
/// 最多轮询多少次（120 ms × 25 ≈ 3 s）——和渲染看门狗同一个数量级。
const SHELL_POLL_MAX: u32 = 25;
/// 内容区四周留给"拖边改大小"的那一圈像素（1.7.7，1.7.8 从 10 收到 4）。
///
/// 只有不接管输入的处理器（Word/Excel…）才内缩：子窗口让出这一圈，
/// 窗口四条边就归我们，拖动改大小照样能用，而内容区里的滚动条/选区仍是处理器自己的。
const SHELL_RING_DIP: i32 = 4;
/// 超过这个时间就认为"这帧不正常"，记一条 warn 并请求取消（协作式，见 `qlr_render::cancel`）。
/// 定 3 s 的理由：本机最慢的正常帧（200 页 PDF 首页、48MP JPEG）都在 100 ms 量级，
/// 3 s 已是 30 倍；再久基本就是解析器卡住或文件躺在慢速网络盘上。
const RENDER_TIMEOUT_MS: u128 = 3000;

/// 渲染线程"挂死"的判定阈值（1.8.3，自愈用）。
///
/// 和上面的 `RENDER_TIMEOUT_MS` 是两件事：3 s 那条只是"记 warn + 请求取消"（协作式，
/// 渲染器在阶段边界自己退出），线程本身还会接着做下一帧；这里回答的是"线程根本回不来了"。
///
/// 为什么是 10 s：本机最慢的正常帧（200 页 PDF 首页、48 MP JPEG）实测都在 100–400 ms，
/// 3 s 已经是 7–30 倍余量；真到 10 s 说明它卡死在某次不会返回的调用里（PDFium /
/// MediaFoundation / 插件 cdylib 的 FFI，或 COM）。阈值再短会把"慢速网络盘上读一个
/// 30 MB 扫描件"误判成挂死——每误判一次就白扔一条线程、重发一帧。
const RENDER_HANG_MS: u64 = 10_000;

/// 连续自愈的退避上限（1.8.3）：第 1 次 10 s、第 2 次 20 s、第 3 次起 40 s。
///
/// 为什么要退避：新线程如果也立刻挂死（PDFium 的全局锁还被旧线程攥着、插件里的静态锁
/// 没释放），固定 10 s 就变成"每 10 s spawn 一条死线程"的连环抖动。退避到 40 s 仍然会
/// 自愈，只是给旧线程留出"自己爬回来"的窗口；一旦有帧正常完成，计数立即清零（见
/// `end_lease`），退避不会累积到下一次偶发挂死上。
const RENDER_HANG_BACKOFF_MAX: u32 = 2;

/// 拖边改大小的热区宽度（DIP）。太小不好抓，太大会吃掉顶栏按钮。
const RESIZE_MARGIN_DIP: i32 = 6;
/// 拖边之后的窗口最小尺寸（DIP；再小就只剩顶栏，用户没法再拖回来）。
///
/// 1.8.3：这两个值改成"设计值 + DPI 换算"。200% 缩放下如果还按 240 物理像素算，
/// 最小窗口只有 120 DIP 宽——比顶栏上三个按钮还窄，拖到极限就再也拖不回来了。
const MIN_WINDOW_W_DIP: i32 = 240;
const MIN_WINDOW_H_DIP: i32 = 160;

/// 最小窗口宽（物理像素，随 DPI）。
fn min_window_w() -> i32 {
    dpi::px(MIN_WINDOW_W_DIP)
}

/// 最小窗口高（物理像素，随 DPI）。
fn min_window_h() -> i32 {
    dpi::px(MIN_WINDOW_H_DIP)
}

/// `HWND` 不是 `Send`，而全局状态需要 `Send`。项目是单线程 GUI，
/// 这个包装只用来把句柄跨函数传递，不代表可以在别的线程用。
#[derive(Clone, Copy)]
struct Hwnd(isize);

impl Hwnd {
    fn raw(self) -> HWND {
        HWND(self.0 as *mut core::ffi::c_void)
    }

    fn from_raw(hwnd: HWND) -> Self {
        Self(hwnd.0 as isize)
    }
}

struct State {
    config: Config,
    host: Hwnd,
    preview: Hwnd,
    /// 投影浮层（1.0.10）：跟随预览窗的位置与尺寸，收起时一起隐藏。
    shadow: Option<Hwnd>,
    visible: bool,
    last_path: Option<PathBuf>,
    /// 本次请求（热键/命令行/转交）开始处理的时刻。
    ///
    /// 注意这里**没有**"进程启动时刻"：常驻实例里那可能是几小时前，
    /// 拿它算"按下空格到出窗"的延迟会得到荒谬的数字（实测在常驻 6.5 s 的宿主上
    /// 报过 `elapsed=6583ms`）。延迟一律以这个字段为准；进程总运行时长用局部变量。
    request_started: Instant,
    /// 渲染代际号：每次请求自增。worker 结果带着它回来，代际不匹配就丢弃
    /// （用户连按空格翻文件时，上一张的解码结果不能盖掉当前这张）。
    render_gen: u64,
    /// 当前 PDF 页（0 起）；非 PDF 恒为 0。
    page: usize,
    /// PDF 总页数（非 PDF 为 None）。
    pages: Option<usize>,
    /// 缩放倍数：1.0 = 适应窗口。>1 时按放大后的像素尺寸重新渲染并允许平移。
    zoom: f32,
    /// 文本内容的滚动位置（顶部显示第几行，0 基）。
    scroll: usize,
    /// 上次绘制时当前窗口吃掉了多少**原始行**（折行后一屏 ≠ 一屏原始行）。
    /// 翻页按它前进，否则折行会让 PageDown 漏掉大半屏内容。
    wrapped_consumed: usize,
    /// 鼠标拖拽平移的起点（屏幕无关的客户区坐标），`None` = 没在拖。
    drag_from: Option<(i32, i32)>,
    /// 这一次按下是否已经拖动过（1.0.2 前用来区分"点击关闭 / 拖拽"，1.0.3 起只用于日志）。
    dragged: bool,
    /// 文本拖拽滚动攒下的零头像素（不够一行的高度时先记着，攒够再滚一行）。
    drag_residual: i32,
    /// 右键缩放的锚点：`(按下时的 y, 按下时的缩放倍数)`，`None` = 没在右键拖。
    zoom_anchor: Option<(i32, f32)>,
    /// 右键拖动**滚动文本**的锚点（屏幕 y）。文本没有缩放概念，右键上下拖就是滚动。
    scroll_anchor: Option<i32>,
    /// 缩放后的位图还没渲染回来：这段时间先用旧位图**拉伸**顶住（1.0.5）。
    ///
    /// 为什么要这样：窗口尺寸是立刻能改的，但按新尺寸重新光栅化要时间（PDF 一页
    /// 几十毫秒）。以前拖动缩放时每一帧都去重渲染，画面一顿一顿，用户的原话是
    /// "右键缩放的时候卡顿比较强，按逻辑应该先缩放窗口"。现在拖动期间只改窗口 +
    /// 拉伸预览，松手才渲染一张高清的。
    zoom_preview: bool,
    /// 平移偏移（位图像素，左上为原点的裁剪起点）。
    pan: (i32, i32),
    /// 当前内容的**自然尺寸**（图片像素 / PDF 首页像素），只用于日志与"原始尺寸"提示。
    natural: Option<(u32, u32)>,
    /// 开窗时的尺寸（= 自然尺寸等比缩进工作区之后的像素），**zoom 的基准**。
    ///
    /// 为什么要单独存一份：zoom = 1.0 的语义是"适应窗口"，不是"100% 原始像素"。
    /// 拿自然尺寸当基准会让 0.8× 这类缩小也超出工作区、结果窗口不变而画面被裁。
    base: Option<(u32, u32)>,
    /// 带样式的内容是否已经打过"样式分布"日志（每份内容只打一次，避免刷屏）。
    styled_logged: bool,
    /// 放大镜镜头中心（客户区坐标）；`None` = 没开。
    ///
    /// 为什么要放大镜：用户真正想要的是"看清素材里某个细节"。把整个窗口放大到超屏
    /// 既要拖窗口、又要拖画面，还得等重光栅化，体验反而差。放大镜直接放大**屏幕像素**，
    /// 一次 GDI 搬运就出来，按住 Shift 随手可看。
    lens: Option<(i32, i32)>,
    /// 放大镜倍数（Shift + 滚轮调，见 `LENS_MIN_ZOOM` / `LENS_MAX_ZOOM`）。
    lens_zoom: f32,
    /// 顶栏上鼠标悬停在哪个按钮上（决定底色与提示气泡）。
    hover_button: Option<window::ChromeButton>,
    /// 页码输入框里正在输入的内容；`None` = 没在编辑。
    page_edit: Option<String>,
    /// 滚轮进量的"零头"（1.8.3）。
    ///
    /// 为什么需要：一格滚轮是 120，但**高分辨率滚轮和精密触控板每次只发 30/60**。
    /// 1.8.2 的算法是 `delta / 120`，零头直接扔掉 → 每一次都算出 0 档 → 滚轮在这类设备上
    /// **彻底没反应**（缩放开不了、正文不滚、目录不滚，而且日志里一条记录都没有）。
    /// 现在把零头攒起来：4 次 30 = 1 档。实测复现与修复见 `wheel_take` 的单测。
    wheel_acc: i32,
    /// 目录栏滚动到第几条（1.8.3：目录可以独立滚动，所以**不需要滚动条**）。
    toc_scroll: usize,
    /// 鼠标当前是否悬停在目录栏上（决定滚轮滚目录还是滚正文）。
    hover_toc: bool,
    /// 是否正在拖右侧滚动条。
    scroll_drag: bool,
    /// 当前内容的目录（Markdown 才有）：`(级别, 标题, 显示行下标)`。
    ///
    /// 只保留这三样而不是整棵 AST：目录只需要"显示什么"和"跳到哪里"。
    toc: Vec<(u8, String, usize)>,
    /// 目录里鼠标悬停/选中的下标。
    toc_active: Option<usize>,
    /// "阻止关闭"是否已开启（1.0.10）。开启后空格/Esc 都关不掉，且**不能取消**。
    pinned: bool,
    /// 正在拖哪几条边改窗口大小（1.0.10 把拖边改大小加回来）。
    resize_edges: Option<ResizeEdges>,
    /// 拖边开始时的 `(鼠标屏幕坐标, 窗口矩形)`：改大小必须按"原始矩形 + 累计位移"算，
    /// 不能每帧累加增量——一旦被最小尺寸夹住，增量就会丢，窗口跟不上鼠标。
    resize_from: Option<(i32, i32, RECT)>,
    /// 图片/视频预览的**长宽比**（`宽 / 高`，1.8.1）。
    ///
    /// 这两类不跟着窗口"拉伸缩放"：窗口本身锁成素材的比例（`clamp_media_resize` /
    /// `window_size_for` 都按它算），于是内容区永远是"铺满"，不会出现留白或变形。
    /// `None` = 当前不是图片/视频（PDF/文本/Office/SVG 照旧自由改大小）。
    media_aspect: Option<f64>,
    /// 上一次预览窗口的左上角屏幕坐标（1.0.13 位置记忆）。
    ///
    /// 显隐之间要跨越进程不重启，所以真正的持久化在 `persist_position()`（写 `config.json`）；
    /// 这里只是"当前会话里最后一次已知位置"，用于窗口还开着时换文件不跳位。
    last_pos: Option<(i32, i32)>,
    /// 本次会话里各格式的窗口尺寸（键 = `format::Kind::name()`；1.0.14 尺寸记忆）。
    ///
    /// 与 `config.window_sizes` 分开：这里是"现在的"，配置里是"上次落盘的"；
    /// 收起/退出时比对两者，没变就不写盘。
    last_sizes: std::collections::BTreeMap<String, (i32, i32)>,

    /// 上一次**真的显示出内容**的时刻（1.1.0）：自适应预渲染靠它判断"这人最近在用"。
    last_show_at: Option<Instant>,

    /// 预渲染的候选文件（1.1.0）：连续两拍都是同一个才动手预热，用户扫文件时不会抢 CPU。
    prewarm_candidate: Option<PathBuf>,

    /// 当前这一帧是什么时候交给渲染线程的（1.2.0）。`None` = 没有活在做。
    render_since: Option<Instant>,

    /// 这一帧是否已经报过"渲染很慢"（1.2.0）。只报一次，免得看门狗每 250 ms 刷一行日志。
    render_warned: bool,

    /// 已经交给渲染线程的那一帧的缓存 key（1.3.0）：算完回来按这个塞进页缓存。
    pending_key: Option<pagecache::PageKey>,

    /// 已经交给渲染线程的那一帧的"上一帧缓存" key（1.4.0）：算完回来顺手存成上一帧。
    pending_frame: Option<FrameKey>,

    /// 系统预览处理器（1.4.1）：Office 等格式优先交给它画真排版，失败退回插件卡片。
    shell_preview: Option<shellpreview::ShellPreview>,

    /// 顶栏一次性提示（1.7.4）：`(文字, 过期时刻)`。
    ///
    /// 为什么需要它：PPT 的翻页由我们接管（系统处理器没有"跳页"接口，只能投翻页键），
    /// 翻到最后一页再按就**不该**再投给 PowerPoint（它会切到"放映结束"的黑屏），
    /// 而是要在顶栏给一句"已是最后一页"。定时器 `TIMER_TOAST` 负责擦掉。
    toast: Option<(String, Instant)>,

    /// 接管了翻页的系统预览内容（目前只有 PPT）：`(当前页 1 基, 总页数)`。
    slide: Option<(usize, usize)>,

    /// 等着"关掉系统预览处理器的输入"（1.7.4）：等首帧画完再关，否则内容区是白的。
    shell_input_pending: bool,

    /// 换文件时把内容区"冻"在上一帧（1.7.6）。
    /// 背景：Office 预览要先跨进程 `DoPreview`（几十到几百毫秒），这期间旧处理器的子窗口
    /// 已经卸掉、新的一帧还没有，整窗重画就会露出那句"占位文字页"——用户说的"跳之前那个
    /// 占位的页面 / 割裂感"就是这个。冻住之后只更新顶栏（标题要立刻对），
    /// 内容区保持上一帧直到新内容覆盖上来（`TIMER_SHELL_INPUT` 到点解除）。
    hold_frame: bool,

    /// 换帧期间用来"盖住"的旧预览处理器（1.7.6）。
    ///
    /// 光冻我们自己的像素不够：新处理器的子窗口一建出来就是空白白板，立刻就把我们的像素盖掉了。
    /// 所以旧处理器先不卸，`SetWindowPos(HWND_TOP)` 顶到同级最上层继续盖着，
    /// `TIMER_SHELL_SWAP` 到点再卸——那一刻新的处理器已经画出首帧，切换看不出中间态。
    shell_cover: Option<shellpreview::ShellPreview>,

    /// 内容区是否"整块交给处理器、由我们接管输入"（1.7.7）。
    ///
    /// 只有 **PPT** 需要（翻页由我们计数，用户点一下处理器就会翻页让计数失准）。
    /// 其余 Office（Word/Excel）**不能接管**：接管等于禁用子窗口，Excel 的滚动条、
    /// 工作表标签、Word 的选区全都点不动——用户反馈"滚动栏都不可用"就是这个。
    /// 不接管时把子窗口**内缩一圈**，把窗口四条边留给我们做拖边改大小。
    shell_owns_input: bool,

    /// `TIMER_SHELL_INPUT` 的轮询次数（1.7.7）：等"内容真的画出来"再收尾，不按固定毫秒猜。
    shell_poll: u32,

    /// 音频预览的播放器（1.7.9）：`Some` 就代表"当前这个预览是有声的"。
    ///
    /// 生命周期跟着预览走：`show_preview` 里打开（自动播放），`hide_preview` /
    /// 换文件 / 退出时 drop（`AudioPlayer::drop` 会 `MCI_CLOSE`，不关会把设备占住）。
    audio: Option<qlr_platform::audio::AudioPlayer>,
    /// 音频播放进度条的刷新计时器是否开着（1.7.9）：只在播放时开着，停就杀掉。
    audio_ticking: bool,

    /// 视频预览的逐帧播放状态（1.7.9）：`Some` 就代表"当前预览是能放的视频"。
    video: Option<VideoPlay>,
}

/// 视频播放状态（1.7.9）。
///
/// 只有 UI 线程碰它（`TIMER_VIDEO` 也是在 UI 线程上来的），所以不需要锁。
struct VideoPlay {
    /// 逐帧解码流（Media Foundation 的 `SourceReader`）。
    stream: qlr_render::media::FrameStream,
    /// 最新解出来的一帧（窗口重画/改大小时拿它重贴，不必再解码一次）。
    last: Option<qlr_render::media::RawFrame>,
    /// 播放锚点：这一时刻对应的时间轴位置，以及当时的挂钟时间。
    anchor_pos_ms: u64,
    anchor_at: std::time::Instant,
    playing: bool,
    /// 总时长（毫秒）：进度条与"到头"判断用。
    duration_ms: u64,
}

/// 正在拖的边（`true` 表示这一边跟着鼠标动）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ResizeEdges {
    left: bool,
    top: bool,
    right: bool,
    bottom: bool,
}

impl ResizeEdges {
    fn any(self) -> bool {
        self.left || self.top || self.right || self.bottom
    }

    /// 对应的鼠标指针形状。对角优先：两条边同时命中时是角。
    fn cursor(self) -> window::CursorKind {
        match (self.left || self.right, self.top || self.bottom) {
            (true, true) => {
                if self.left == self.top {
                    window::CursorKind::SizeNWSE
                } else {
                    window::CursorKind::SizeNESW
                }
            }
            (true, false) => window::CursorKind::SizeWE,
            (false, true) => window::CursorKind::SizeNS,
            (false, false) => window::CursorKind::Arrow,
        }
    }

    fn describe(self) -> String {
        let mut parts = Vec::new();
        if self.left {
            parts.push("左");
        }
        if self.top {
            parts.push("上");
        }
        if self.right {
            parts.push("右");
        }
        if self.bottom {
            parts.push("下");
        }
        format!("{}边", parts.join("/"))
    }
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();
/// 绘制内容单独放一个锁：`WM_PAINT` 可能在 `ShowWindow` 里同步重入，
/// 若此时还持有 `STATE` 就会死锁。规矩：**先写 LINES、放锁，再动窗口**。
static LINES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
/// 已渲染好的画面（`Arc` 是因为要给 worker 回传与 UI 共享，避免整块像素拷贝）。
static CONTENT: OnceLock<Mutex<Option<Arc<Canvas>>>> = OnceLock::new();
/// 渲染任务队列的发送端。worker 线程在 `run()` 里拉起后长期存活。
static JOB_TX: OnceLock<Mutex<Sender<RenderJob>>> = OnceLock::new();
/// worker 的产出槽：`(代际号, 结果)`。`PostMessage` 只能带两个整数，所以载荷走这里。
type RenderOutcome = (u64, Result<Canvas, qlr_core::Error>);
static RESULT: OnceLock<Mutex<Option<RenderOutcome>>> = OnceLock::new();

/// 渲染线程代号（1.8.3）：每 spawn 一条就换一个新的、更大的号。
///
/// 渲染结果异步回来，自愈换线程之后**旧线程仍可能从第三方代码里爬出来交活**（PDFium 的
/// 锁一放、COM 调用一返回）。结果槽只存"最后一帧"，旧线程的迟到结果要是写进去，就会顶掉
/// 新线程刚算好的那一帧（UI 看到过期代际 → 丢弃 → 画面永远不出现）。有了代号，"这条线程
/// 还是不是当前那条"就是一次原子读：旧线程在写槽/发通知之前先自报家门。
///
/// 初值 0 = "还没有渲染线程"；`spawn_render_worker` 成功换上新通道之后才把新号写进来。
static RENDER_EPOCH: AtomicU64 = AtomicU64::new(0);

/// 渲染线程的"租约"（1.8.3）：渲染线程领活时登记，交活时注销。
///
/// 为什么需要它：渲染线程是**单例**，一旦它卡在第三方 FFI 里就永不返回，之后所有任务
/// 排队无人消费 —— 本会话预览永久不出图，只能重启。租约让 UI 线程能回答"这一帧从什么
/// 时候开始跑、跑的是哪个文件"，从而判挂并换一条线程。
///
/// 另外三个字段的含义：`epoch` 领这条租约的渲染线程代号，`generation` 任务代际
/// （防止 A 的完成把 B 的租约清掉），`job` 留着自愈之后原样重发。
struct Lease {
    epoch: u64,
    generation: u64,
    started: Instant,
    job: RenderJob,
}

/// 租约 + 自愈状态（1.8.3）。**一把锁**，因为这三件事每次都要一起看。
///
/// 为什么不塞进 `State`：`State` 是 UI 线程的大锁（里面的字段牵着窗口/COM 句柄的生命
/// 周期），渲染线程绝不能去碰它。这把锁只在三处各拿几微秒：派发时登记、交活时注销、
/// 250 ms 看门狗判挂 —— UI 线程读它不会等渲染，渲染线程写它也不会等 UI。
struct RenderTrack {
    lease: Option<Lease>,
    /// 自上次有帧真正完成以来，连续自愈了几次（成功后清零，用于退避）。
    consecutive: u32,
    /// 本会话累计重建次数（写进日志，便于判断"是不是一直在自愈"）。
    total: u32,
    /// 有重建在飞：从判挂到换完发送端之间置位，保证任何时刻最多一个重建。
    healing: bool,
}

static RENDER_TRACK: OnceLock<Mutex<RenderTrack>> = OnceLock::new();

fn render_track() -> &'static Mutex<RenderTrack> {
    RENDER_TRACK.get_or_init(|| {
        Mutex::new(RenderTrack {
            lease: None,
            consecutive: 0,
            total: 0,
            healing: false,
        })
    })
}

/// 从 `WM_COPYDATA` 收到、还没处理的请求。
/// 不能在 `WM_COPYDATA` 里直接处理：那会儿线程处于输入同步呼叫状态，调 COM 会被拒。
static PENDING: OnceLock<Mutex<Vec<Request>>> = OnceLock::new();

/// 一帧渲染任务。只带不可变输入 + 显示预算，不碰任何窗口句柄。
///
/// `Clone` 只为租约服务（1.8.3）：自愈之后要把"卡住的那一帧"原样重发给新线程。
#[derive(Clone)]
struct RenderJob {
    generation: u64,
    path: PathBuf,
    max_width: u32,
    max_height: u32,
    /// PDF 要第几页（其他格式忽略）。
    page: usize,
    /// 允许放大渲染（缩放到 100% 以上时为 `true`，否则位图封顶在原始像素）。
    upscale: bool,
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    let mutex: MutexGuard<'_, State> = STATE.get()?.lock().ok()?;
    let mut guard = mutex;
    Some(f(&mut guard))
}

fn set_lines(lines: Vec<String>) {
    if let Some(slot) = LINES.get()
        && let Ok(mut guard) = slot.lock()
    {
        *guard = lines;
    }
}

fn get_lines() -> Vec<String> {
    LINES
        .get()
        .and_then(|slot| slot.lock().ok().map(|guard| guard.clone()))
        .unwrap_or_default()
}

fn set_content(content: Option<Arc<Canvas>>) {
    // 目录要在**换内容时**算一次：Markdown 可能有十几万行，每次重绘都抽一遍太浪费。
    let toc = match content.as_deref() {
        Some(Canvas::Text(doc)) => match &doc.styles {
            Some(styles) => qlr_render::markdown::outline(&doc.lines, styles)
                .into_iter()
                .map(|entry| (entry.level, entry.text, entry.line))
                .collect(),
            None => Vec::new(),
        },
        _ => Vec::new(),
    };
    if let Some(slot) = CONTENT.get()
        && let Ok(mut guard) = slot.lock()
    {
        *guard = content;
    }
    // 换了内容就把文本滚动位置归零，否则打开新文件还停在上一个文件的中段
    with_state(|s| {
        s.scroll = 0;
        s.styled_logged = false;
        s.toc = toc;
        s.toc_active = None;
        // 1.8.3：目录的滚动位置也要跟着新文件归零（否则换文件后目录停在半截）。
        s.toc_scroll = 0;
    });
}

fn get_content() -> Option<Arc<Canvas>> {
    CONTENT
        .get()
        .and_then(|slot| slot.lock().ok().and_then(|guard| guard.clone()))
}

/// 程序入口。返回进程退出码。
pub fn run() -> ExitCode {
    dpi::enable_per_monitor_v2();
    // 1.8.3：主线程先按"系统 DPI"起步。开窗尺寸是在**窗口建出来之前**算的
    // （`window_size_for`），那时没有 hwnd 可查，只能用系统 DPI；单屏用户这就是对的。
    // 窗口建好之后由窗口过程里的 `dpi::enter(hwnd)` 按窗口实际 DPI 细化。
    dpi::seed_thread(dpi::system_dpi());
    // 装"DPI 变了 → 重建字体"的钩子：字体是按像素高度 CreateFontW 出来的，
    // 不重建就会一直是旧尺寸。
    window::install_dpi_hook();
    let _ = paths::ensure_dirs();

    let (config, config_err) = Config::load(&paths::config_path());
    // 1.8.3：内容缩放（正文/目录/列表）与外壳分开——见 `Config::content_scale_percent`。
    // 必须在读配置之后设（默认 0 = 自动，200% 下按 1.41 倍而不是 2 倍）。
    dpi::set_content_percent(config.content_scale_percent);
    let level = Level::from_env_or(level_from_str(&config.log_level));
    let _ = log::init(level, &paths::log_file());

    log_info!(SCOPE, "=== 启动 {} ===", version::banner());
    // 崩溃兜底（1.2.0）：先装 hook（越早越好，后面任何 panic 都能留下 dmp），
    // 再报告"上次是不是崩过"。
    crash::install_hook();
    crash::report_previous_crash();
    log_info!(
        SCOPE,
        "exe_dir={:?} portable={} data_dir={:?}",
        paths::exe_dir(),
        paths::is_portable(),
        paths::data_dir()
    );
    log_info!(SCOPE, "config: {}", config.summary());
    let hotkey_enabled = config.hotkey_enabled;
    if let Some(err) = config_err {
        log_warn!(SCOPE, "{err}");
    }
    log_info!(SCOPE, "log_file={:?}", log::path());

    // Shell 选中项要 COM（STA）。必须在 UI 线程上初始化，且要在任何 shell 调用之前。
    let com_ok = selection::init_com();
    log_info!(SCOPE, "COM 初始化={com_ok}");
    // 完整性级别决定"能不能跟 explorer 说话"：低于 medium 时托盘图标与 Shell 选区
    // 都会被 UIPI 拒成 0x80070005（坑 #29）。写进启动日志，排障第一眼就有答案。
    log_info!(SCOPE, "令牌完整性={}", integrity::describe());

    // GUI 子系统没有 stderr：panic 必须同时写进日志 + 落盘报告。
    // 这一步已经由启动时的 `crash::install_hook()` 做完（删掉这里旧的一条，
    // 否则后装的 hook 会把前面那个带 minidump 的覆盖掉——1.2.0 踩过）。

    let started = Instant::now();
    let command = cli::parse(std::env::args().skip(1));

    // 诊断命令：查一次选中项就退出（早于单实例判定，可以和常驻实例共存）。
    let probe_target = match &command {
        Command::ProbeSelection => Some(None),
        Command::ProbeSelectionOf(hwnd) => Some(Some(*hwnd)),
        _ => None,
    };
    if let Some(target) = probe_target {
        let found = match target {
            Some(raw) => {
                log_info!(SCOPE, "PROBE-SELECTION frame=0x{raw:X}");
                selection::selection_of_hwnd(HWND(raw as *mut core::ffi::c_void))
            }
            None => selection::current_selection(),
        };
        for (index, path) in found.paths.iter().enumerate() {
            log_info!(SCOPE, "PROBE-SELECTION [{index}] {}", path.display());
        }
        log_info!(SCOPE, "PROBE-SELECTION {}", found.summary());
        if com_ok {
            selection::uninit_com();
        }
        return ExitCode::SUCCESS;
    }

    // 安装器要的文件关联扩展名清单：由 `format::all_extensions()`（唯一真值）写出，
    // 不建窗口、不进单实例，写完就退出。
    if let Command::ListExtensions(out) = &command {
        let mut text = String::new();
        for ext in format::all_extensions() {
            text.push_str(ext);
            text.push('\n');
        }
        return match std::fs::write(out, text.as_bytes()) {
            Ok(()) => {
                log_info!(
                    SCOPE,
                    "LIST-EXTENSIONS {} 个 -> {:?}",
                    format::all_extensions().len(),
                    out
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                log_error!(SCOPE, "LIST-EXTENSIONS 写 {:?} 失败：{e}", out);
                ExitCode::from(4)
            }
        };
    }

    // 无窗口渲染（A2.10 基准 / A2.8 PDF 诊断）：不建窗口、不进单实例，
    // 渲染一次把结果写日志就退出 —— 这样基准脚本不需要桌面会话也能跑。
    if let Command::Render { path, page } = &command {
        let target = std::path::Path::new(path);
        let t0 = Instant::now();
        let pages = qlr_render::pdf_page_count(target);
        match qlr_render::render_file_at(target, 1190, 748, *page) {
            Ok(canvas) => {
                log_info!(
                    SCOPE,
                    "RENDER-CLI ok kind={} bytes={} pages={:?} page={} elapsed={}ms",
                    canvas.kind_name(),
                    canvas.approx_bytes(),
                    pages,
                    page,
                    t0.elapsed().as_millis()
                );
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                log_error!(
                    SCOPE,
                    "RENDER-CLI FAIL page={} elapsed={}ms err={e}",
                    page,
                    t0.elapsed().as_millis()
                );
                return ExitCode::from(3);
            }
        }
    }

    let request = match command {
        Command::Help => {
            log_info!(SCOPE, "帮助：\n{}", cli::usage());
            return ExitCode::SUCCESS;
        }
        Command::Version => {
            log_info!(SCOPE, "{}", version::banner());
            return ExitCode::SUCCESS;
        }
        // 上面已经处理并 return 了。
        Command::ProbeSelection
        | Command::ProbeSelectionOf(_)
        | Command::Render { .. }
        | Command::ListExtensions(_) => {
            unreachable!()
        }
        Command::Invoke(req) => req,
    };

    // 已有实例在跑 → 把请求转交过去，本进程立刻退出。
    if let Some(host) = instance::find_host(ipc::HOST_WINDOW_CLASS) {
        return forward(host, &request, started);
    }

    // 抢单实例。Ok(None) 说明刚好有人抢先，再试一次转交。
    let _guard = match instance::acquire(ipc::INSTANCE_MUTEX) {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            return match instance::find_host(ipc::HOST_WINDOW_CLASS) {
                Some(host) => forward(host, &request, started),
                None => {
                    log_error!(SCOPE, "互斥体被占用但找不到宿主窗口，放弃启动");
                    ExitCode::from(2)
                }
            };
        }
        Err(e) => {
            log_error!(SCOPE, "创建单实例互斥体失败：{}", qlr_platform::to_core(e));
            return ExitCode::from(2);
        }
    };

    // 注册窗口类（宿主 + 预览，共用一个窗口过程）。
    for class in [ipc::HOST_WINDOW_CLASS, PREVIEW_CLASS] {
        if let Err(e) = window::register_class(class, Some(wndproc)) {
            log_error!(
                SCOPE,
                "注册窗口类 {class} 失败：{}",
                qlr_platform::to_core(e)
            );
            return ExitCode::from(4);
        }
    }

    let host = match window::create_host_window(ipc::HOST_WINDOW_CLASS, &version::banner()) {
        Ok(h) => h,
        Err(e) => {
            log_error!(SCOPE, "创建宿主窗口失败：{}", qlr_platform::to_core(e));
            return ExitCode::from(5);
        }
    };
    let preview = match window::create_preview_window(PREVIEW_CLASS, "QLRust 预览") {
        Ok(h) => h,
        Err(e) => {
            log_error!(SCOPE, "创建预览窗口失败：{}", qlr_platform::to_core(e));
            return ExitCode::from(5);
        }
    };
    // 柔和投影（1.0.10）：一个独立的分层窗口贴在预览窗下面。
    // 拿不到就当作没有阴影（不影响预览本身），所以失败只记日志。
    let shadow = match window::register_class(SHADOW_CLASS, Some(wndproc))
        .and_then(|()| window::create_shadow_window(SHADOW_CLASS, "QLRust 阴影"))
    {
        Ok(h) => Some(Hwnd::from_raw(h)),
        Err(e) => {
            log_warn!(
                SCOPE,
                "创建阴影窗口失败（不显示投影）：{}",
                qlr_platform::to_core(e)
            );
            None
        }
    };
    // Win11 才有 DWM 圆角/backdrop（Win10 上这两个属性不存在，调用静默失败）。
    // 圆角/backdrop 都走 DWM（Win11 才有 backdrop，Win10 自动降级为无效果）。
    // Win11 走官方 DWM 属性。两者都取不到时退回纯色背景。
    let effects = win11::apply(preview);
    log_info!(
        SCOPE,
        "系统 build={} 圆角={} backdrop={}",
        win11::build_number(),
        effects.rounded,
        effects.backdrop
    );

    let tray_ready = match tray::add(
        host,
        TRAY_ID,
        &format!("{}（右键菜单）", version::banner()),
        ipc::WM_TRAY_CALLBACK,
    ) {
        Ok(()) => {
            // 图标加成功 ≠ 用户能看见：可能被塞进 `^` 溢出区，或被"隐藏的图标"设置关掉。
            // 用 Shell_NotifyIconGetRect 问一次 Shell 它在哪儿，写进日志便于排查。
            match tray::rect(host, TRAY_ID) {
                Some(r) => log_info!(
                    SCOPE,
                    "托盘图标已添加 rect=({},{})-({},{}) 尺寸={}x{}",
                    r.left,
                    r.top,
                    r.right,
                    r.bottom,
                    r.right - r.left,
                    r.bottom - r.top
                ),
                None => log_warn!(
                    SCOPE,
                    "托盘图标已添加但 Shell 报不出位置（可能被折叠进溢出区，或图标未真正注册）"
                ),
            }
            true
        }
        Err(e) => {
            let code = qlr_platform::to_core(e);
            log_error!(SCOPE, "托盘图标创建失败（不影响预览）：{}", code);
            if let Some(level) = integrity::current().filter(|l| l.below_medium()) {
                log_error!(
                    SCOPE,
                    "根因疑似令牌完整性={}：低完整性进程不能向 explorer 加托盘图标（UIPI）。\
                     通常是因为 exe 放在受限目录里（例如 DSH 工作目录：文件被打了 \
                     Low Mandatory Level 标签）。把 qlrust.exe 复制到普通目录再运行。",
                    level.as_str()
                );
            }
            // 托盘失败时程序会静默常驻后台、用户完全找不到入口（也没法退出），
            // 所以必须显式告知 —— 否则用户只会看到"程序好像没起来"。
            //
            // 1.8.3 改：不再单独起线程弹模态 MessageBox。
            // 老弹框的三个副作用（实测都成立）：
            // 1. 那条 `std::thread::spawn` 让进程形态变样（线程 7 → 9），
            //    任何"常驻资源"对比都会被它带偏（上一版 +9 线程 / +46% WS 就是这么来的）；
            // 2. 模态框会盖住屏幕、只能点"确定"，自动化脚本会被它卡住；
            // 3. 框关掉之后用户仍然不知道程序还在跑——信息只活在那一次弹窗里。
            //
            // 现在改成"日志 + 第一次显示窗口时的顶栏 toast"（见 `notify_tray_failure_once`）：
            // 无窗口时也能在日志里查到根因和手工退出方式，第一次预览时再用非模态提示说一次。
            // 仍然只在"用户双击启动"（没有路径、动作是裸 Toggle）时标记，
            // 命令行/自动化调用不打扰（这条约束从 1.0.x 就有，别去掉）。
            let bare_launch = request.path.is_none() && matches!(request.action, Action::Toggle);
            if bare_launch {
                TRAY_FAILURE_NOTICE.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            false
        }
    };

    // 配置马上要被移进 State，热键摘要与绑定表先留一份出来（下面装钩子要用）。
    let hotkeys = config.hotkeys.clone();

    let _ = STATE.set(Mutex::new(State {
        config,
        host: Hwnd::from_raw(host),
        preview: Hwnd::from_raw(preview),
        shadow,
        visible: false,
        last_path: None,
        request_started: started,
        render_gen: 0,
        page: 0,
        pages: None,
        zoom: 1.0,
        scroll: 0,
        wrapped_consumed: 0,
        natural: None,
        base: None,
        drag_from: None,
        dragged: false,
        drag_residual: 0,
        zoom_anchor: None,
        scroll_anchor: None,
        zoom_preview: false,
        pan: (0, 0),
        styled_logged: false,
        lens: None,
        lens_zoom: LENS_DEFAULT_ZOOM,
        hover_button: None,
        page_edit: None,
        wheel_acc: 0,
        toc_scroll: 0,
        hover_toc: false,
        scroll_drag: false,
        toc: Vec::new(),
        toc_active: None,
        pinned: false,
        resize_edges: None,
        resize_from: None,
        media_aspect: None,
        last_pos: None,
        last_sizes: std::collections::BTreeMap::new(),
        last_show_at: None,
        prewarm_candidate: None,
        render_since: None,
        render_warned: false,
        pending_key: None,
        pending_frame: None,
        shell_preview: None,
        toast: None,
        slide: None,
        shell_input_pending: false,
        hold_frame: false,
        shell_cover: None,
        shell_owns_input: false,
        shell_poll: 0,
        audio: None,
        audio_ticking: false,
        video: None,
    }));
    let _ = LINES.set(Mutex::new(vec!["QLRust".to_string()]));
    let _ = CONTENT.set(Mutex::new(None));
    let _ = RESULT.set(Mutex::new(None));

    // 渲染线程（M2）：一次拉起、长期存活。它拿不到 HWND，只能 PostMessage 通知 UI。
    match spawn_render_worker(host) {
        Ok((tx, epoch)) => {
            let _ = JOB_TX.set(Mutex::new(tx));
            // 通道就位之后再让代号生效（理由见 `spawn_render_worker` 的注释）。
            RENDER_EPOCH.store(epoch, Ordering::Release);
        }
        Err(e) => log_error!(SCOPE, "渲染线程启动失败（只能出占位文字）：{e}"),
    }

    // 预渲染线程（1.1.0）：只做"窗口收着时先把下一个文件画好"，与渲染线程分开。
    match spawn_prewarm_worker() {
        Ok(()) => log_info!(SCOPE, "预渲染线程就绪（档位={}）", {
            with_state(|s| s.config.prewarm.label()).unwrap_or("-")
        }),
        Err(e) => log_warn!(SCOPE, "预渲染线程启动失败（预热不可用）：{e}"),
    }

    // 插件（M6）：懒加载其实也行，但启动时扫一次能把"加载失败"提前暴露在日志里。
    let host_plugins = plugins::host();
    log_info!(
        SCOPE,
        "插件就绪 dir={} loaded={} extensions={} errors={}",
        host_plugins.dir().display(),
        host_plugins.plugins().len(),
        host_plugins.extension_count(),
        host_plugins.errors().len()
    );
    for plugin in host_plugins.plugins() {
        log_info!(
            SCOPE,
            "插件 {} 接管 [{}] 文件={}",
            plugin.name(),
            plugin.extensions().join(" "),
            plugin.file().display()
        );
    }
    for error in host_plugins.errors() {
        log_warn!(SCOPE, "插件加载失败 {error}");
    }

    log_info!(
        SCOPE,
        "壳层就绪 window_created={}ms tray={}",
        started.elapsed().as_millis(),
        tray_ready
    );

    // 跟随 + 预渲染共用同一只 400 ms 计时器（1.1.0 起常驻）：
    // - 窗口开着 → 跟着资源管理器的选中项换内容；
    // - 窗口收着 → 悄悄把当前选中的那个文件先画好，等用户按空格。
    window::start_timer(preview, TIMER_FOLLOW, FOLLOW_INTERVAL_MS);
    // 渲染看门狗（1.2.0）：和跟随计时器分开，因为它要 250 ms 一拍才够灵敏。
    window::start_timer(preview, TIMER_WATCHDOG, WATCHDOG_INTERVAL_MS);

    // 全局空格热键（A1.5）。失败不致命：还可以用 CLI / 托盘触发。
    let mut hotkey_guard = if hotkey_enabled {
        // 滚轮缩放与 Esc 收起都靠钩子线程旁听（预览窗是 WS_EX_NOACTIVATE，永远
        // 拿不到焦点，系统的 WM_MOUSEWHEEL/WM_KEYDOWN 根本不会投给它）。
        hotkey::set_preview(preview);
        hotkey::set_extra_messages(ipc::WM_QLR_ESCAPE, ipc::WM_QLR_WHEEL);
        // 1.0.7：按键全部来自配置（设置页可改），钩子按绑定表匹配。
        hotkey::set_bindings(hotkeys.packed());
        match hotkey::start(host, ipc::WM_QLR_HOTKEY) {
            Ok(guard) => {
                log_info!(
                    SCOPE,
                    "热键就绪 [{}] installed={} elapsed={}ms",
                    hotkeys.summary(),
                    guard.installed(),
                    started.elapsed().as_millis()
                );
                Some(guard)
            }
            Err(e) => {
                log_error!(
                    SCOPE,
                    "热键安装失败（降级为仅 CLI/托盘触发）：{}",
                    qlr_platform::to_core(e)
                );
                None
            }
        }
    } else {
        log_warn!(SCOPE, "配置关闭了热键（hotkey_enabled=false）");
        None
    };

    // 首个请求。
    //
    // 启动语义（用户实测反馈）：**双击启动就应该是纯后台**，不要弹一个空窗口占着前台。
    // 所以只有"明确要看某个文件"（带路径）或显式 `--show` 才在启动时出窗；
    // 裸启动 = 安静驻留后台，靠托盘图标 + 空格热键工作。
    // 1.0.7：`--settings` 也算"启动就要做一件看得见的事"，否则设置页永远打不开。
    let startup_shows =
        request.path.is_some() || matches!(request.action, Action::Show | Action::Settings);
    if startup_shows {
        apply_request(&request);
    } else {
        log_info!(
            SCOPE,
            "启动后保持后台：不显示窗口（选中文件按空格预览，托盘图标右键退出）"
        );
    }

    // 后台状态没有窗口、也没有任务栏按钮，用户很容易"不知道它起没起来"。
    // 起一次气泡提示（失败不致命：提权会话下托盘本身就可能被 UIPI 拒）。
    if tray_ready
        && !startup_shows
        && tray::notify(
            host,
            TRAY_ID,
            &version::banner(),
            "已在后台运行：选中文件按空格预览；右键托盘图标可退出。",
        )
    {
        log_info!(SCOPE, "已弹出后台运行提示（托盘气泡）");
    }

    loop {
        match window::pump() {
            window::Pump::Message(msg) => window::dispatch(&msg),
            window::Pump::Quit => {
                log_info!(SCOPE, "消息泵收到 WM_QUIT");
                break;
            }
            window::Pump::Error => {
                log_error!(SCOPE, "消息泵 GetMessageW 返回错误");
                break;
            }
        }
    }

    // 退出兜底（1.8.3）：消息泵已经退出，下面全是"尽力而为"的清理。清理有可能被
    // 第三方 DLL 吊住——插件是用 LoadLibrary 进来的，进程退出时 Windows 会进它的
    // `DllMain(DLL_PROCESS_DETACH)`；插件在 DllMain 里等锁/等 COM/跑静态析构，
    // 进程就停在"窗口没了、任务管理器里还在"的半死状态（现场报障就是这个现象）。
    // 托盘图标已经摘掉、配置也已经落盘，所以超时直接强制结束自己最干脆。
    arm_exit_watchdog(Duration::from_secs(5));
    if tray_ready {
        tray::remove(host, TRAY_ID);
    }
    // 先摘钩子再退进程：钩子线程还在跑时退进程，Windows 会给调试器记一条"钩子未摘除"。
    if let Some(mut guard) = hotkey_guard.take() {
        let stats = hotkey::stats();
        log_info!(
            SCOPE,
            "热键统计 callbacks={} swallowed={} reinstall={} passed={} wheels={} \
             passed_text_focus={} text_focus_level={}",
            stats.callbacks,
            stats.swallowed,
            stats.reinstalls,
            stats.passed_other_class,
            stats.wheels,
            stats.passed_text_focus,
            stats.text_focus_level
        );
        guard.stop();
    }
    if com_ok {
        selection::uninit_com();
    }
    log_info!(SCOPE, "退出：总运行 {}ms", started.elapsed().as_millis());
    ExitCode::SUCCESS
}

/// 退出看门狗：清理阶段卡住就强制结束进程（调用点见 `run()`）。
///
/// 为什么需要它：`quit()` 只负责把消息泵停掉，之后还要摘热键钩子、卸托盘、
/// 反初始化 COM。这几步都会进第三方代码（钩子线程、插件 DLL 的 DllMain），
/// 一旦对方卡住，进程就永远停在"没有窗口、任务管理器里还在"的状态——用户唯一的
/// 出路是手动结束进程。这里给清理一个硬上限，到点自己走 `TerminateProcess`：
/// 不触发 DLL detach 流程（卡住我们的正是它），也不等任何线程收尾。
fn arm_exit_watchdog(limit: Duration) {
    let spawned = std::thread::Builder::new()
        .name("qlrust-exit-watchdog".into())
        .spawn(move || {
            std::thread::sleep(limit);
            log_error!(
                SCOPE,
                "退出清理超过 {}s 仍未结束（第三方 DLL/钩子线程卡住），强制结束进程",
                limit.as_secs()
            );
            unsafe {
                let _ = TerminateProcess(GetCurrentProcess(), 1);
            }
            // TerminateProcess 正常情况下当场生效；万一被拒（受保护句柄），
            // 再用 std 的保底退出，至少不会把控制权交给卡住的清理代码。
            std::process::exit(1);
        });
    if spawned.is_err() {
        log_error!(
            SCOPE,
            "退出看门狗线程起不来（内存不足？），清理阶段没有超时保护"
        );
    }
}

fn level_from_str(s: &str) -> Level {
    match s {
        "error" => Level::Error,
        "warn" => Level::Warn,
        "debug" => Level::Debug,
        "trace" => Level::Trace,
        _ => Level::Info,
    }
}

// ---------------------------------------------------------------------------
// 渲染线程（M2）
// ---------------------------------------------------------------------------

/// 拉起常驻渲染线程。返回任务发送端。
///
/// 线程只拿 `host` 的裸句柄用于 `PostMessageW`（`HWND` 不是 `Send`，所以只能传 `isize`），
/// 绝不碰窗口的其他属性——窗口只能在 UI 线程上操作。
/// 测试钩子：`QLRUST_SLOW_RENDER_MS=5000` 会让每个渲染任务先睡这么久。
///
/// 存在的唯一目的是验证 1.2.0 的"渲染超时/取消"链路——真做出一个要跑 5 s 的
/// 合法文件很难（那才是问题），而"注入延迟"能稳定复现看门狗的行为。
/// 生产环境不会设置这个变量，`std::env::var` 只查一次。
fn fake_slow_ms() -> Option<u64> {
    use std::sync::OnceLock;
    static CACHED: OnceLock<Option<u64>> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("QLRUST_SLOW_RENDER_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|ms| *ms > 0 && *ms <= 60_000)
    })
}

/// 测试钩子（1.8.3）：`QLRUST_RENDER_HANG_MS=30000` 让渲染线程在处理**第一个**任务前
/// 先睡这么久，模拟"卡在第三方 FFI（PDFium / MediaFoundation / 插件 cdylib）里回不来"，
/// 用来验证租约判挂 + 线程自愈这条链路。
///
/// **仅开发/自动化验证用**，和 `QLRUST_DPI_FAKE` / `QLRUST_DPI_FIX` 是同一类开关：
/// 不设置这个变量时行为与改动前完全一致（`std::env::var` 只查一次，热路径零开销）。
///
/// 为什么只注入到**第一条**线程：要验证的是"打开文件 → 约 10 s 后自愈 ERROR →
/// 再打开另一个文件能正常出图"。重建出来的线程如果也睡 30 s，就永远验证不到"恢复"。
fn fake_hang_ms() -> Option<u64> {
    use std::sync::OnceLock;
    static CACHED: OnceLock<Option<u64>> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("QLRUST_RENDER_HANG_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|ms| *ms > 0 && *ms <= 600_000)
    })
}

/// 派发任务前登记租约（UI 线程）。
///
/// 顺序是"先登记、再 `send`"：反过来的话，一个几十微秒就完成的渲染可能先注销，
/// 然后被我们写成一条再也不会被清掉的空租约 —— 10 s 后看门狗就会对一条不存在的活判挂。
///
/// 这里**不**预先生成任务描述：那要在每一帧（包括 3 ms 的文本帧）上多一次 `format!` 和
/// 一次堆分配，而描述只在"真的判挂"那一次才用得上（`mark_hung` 里现算）。
fn begin_lease(job: &RenderJob) {
    let lease = Lease {
        epoch: RENDER_EPOCH.load(Ordering::Acquire),
        generation: job.generation,
        started: Instant::now(),
        job: job.clone(),
    };
    if let Ok(mut guard) = render_track().lock() {
        guard.lease = Some(lease);
    }
}

/// 任务描述（写日志用）：文件名 + 类型 + 目标尺寸 + 页码，**不含完整用户路径**。
fn describe_job(job: &RenderJob) -> String {
    let name = job
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("<无名>");
    let kind = format::kind_for_path(&job.path);
    if kind == format::Kind::Pdf {
        format!(
            "{name} {kind} {}x{} 第{}页",
            job.max_width,
            job.max_height,
            job.page + 1
        )
    } else {
        format!("{name} {kind} {}x{}", job.max_width, job.max_height)
    }
}

/// 投递失败（线程已退出）：撤掉刚登记的租约，否则看门狗会为一条根本不存在的活重建线程。
fn abort_lease(generation: u64) {
    if let Ok(mut guard) = render_track().lock()
        && let Some(lease) = guard.lease.as_ref()
        && lease.generation == generation
    {
        guard.lease = None;
    }
}

/// 渲染线程交活：只有"同一条线程 + 同一代"的租约才有资格注销。
///
/// 两个条件各挡一种错：`epoch` 挡"自愈前那条旧线程迟到返回"（它手里那张租约早属于新世界
/// 了）；`generation` 挡"同一线程里 A 交活时把 B 的租约清掉"（UI 在 A 还在跑的时候就已经
/// 派了 B，单线程 worker 手里是 A，租约却已经是 B 的）。
fn end_lease(epoch: u64, generation: u64) {
    let Ok(mut guard) = render_track().lock() else {
        return;
    };
    if let Some(lease) = guard.lease.as_ref()
        && lease.epoch == epoch
        && lease.generation == generation
    {
        guard.lease = None;
        // 有帧真的跑完了 = 线程是活的：连续自愈计数清零，退避不会累积到下一次偶发挂死。
        guard.consecutive = 0;
    }
}

/// 判挂阈值：连续自愈时翻倍退避（10 s → 20 s → 40 s 封顶，理由见 `RENDER_HANG_BACKOFF_MAX`）。
fn hang_threshold(consecutive: u32) -> Duration {
    Duration::from_millis(RENDER_HANG_MS << consecutive.min(RENDER_HANG_BACKOFF_MAX))
}

/// 判挂（UI 线程，250 ms 一拍）：租约超时、且没有重建在飞时占住 `healing`。
///
/// 返回 `(任务描述, 已卡毫秒, 原任务, 连续第几次, 本会话第几次)`。
fn mark_hung() -> Option<(String, u128, RenderJob, u32, u32)> {
    let mut guard = render_track().lock().ok()?;
    if guard.healing {
        return None;
    }
    let (desc, ms, job) = {
        let lease = guard.lease.as_ref()?;
        let ms = lease.started.elapsed();
        if ms < hang_threshold(guard.consecutive) {
            return None;
        }
        (describe_job(&lease.job), ms.as_millis(), lease.job.clone())
    };
    guard.healing = true;
    Some((
        desc,
        ms,
        job,
        guard.consecutive.saturating_add(1),
        guard.total.saturating_add(1),
    ))
}

/// 重建失败（线程起不来 / 发送端锁被毒化）：退避一次，别每 250 ms 刷一行 ERROR。
fn heal_failed() {
    if let Ok(mut guard) = render_track().lock() {
        guard.healing = false;
        guard.consecutive = guard.consecutive.saturating_add(1);
        // 时钟重启：下一次判挂要再等一个（已经翻倍过的）阈值。
        if let Some(lease) = guard.lease.as_mut() {
            lease.started = Instant::now();
        }
    }
}

/// 重建成功：记一次，并把卡住的那一帧重新登记成新租约（`Some` 表示要重发）。
///
/// **必须在"把这一帧投给新线程"之前调用**：新线程可能几微秒就算完（文本类），
/// 要是它先交活、我们再登记租约，就会留下一条永远没人清的空租约 —— 下一个阈值到点
/// 时看门狗会为一条根本不存在的活再判一次挂死。
fn heal_succeeded(requeue: Option<&RenderJob>) {
    if let Ok(mut guard) = render_track().lock() {
        guard.healing = false;
        guard.lease = None;
        guard.consecutive = guard.consecutive.saturating_add(1);
        guard.total = guard.total.saturating_add(1);
    }
    if let Some(job) = requeue {
        begin_lease(job);
    }
}

/// 渲染线程挂死 → 换一条新线程（1.8.3）。
///
/// 旧线程**不 join、不 kill**：Windows 上 `TerminateThread` 会在它正持有锁（堆锁、加载器锁、
/// 第三方库内部锁）时把进程状态撕坏 —— 那比"烂着一条线程"危险得多。旧线程的 `Sender`
/// 在这里就地换成新的、旧的被 drop，所以它一旦从 FFI 里爬出来，`rx.recv()` 立刻报错、
/// 自己安静退出（线程自然死亡，不占任何资源）。
///
/// 保证任何时刻最多一个重建在飞：`healing` 从判挂起一直占到发送端换完；重建本身是同步的
/// （只 spawn + 换 Sender，不 join 任何东西），UI 线程不会被它拖住。
fn check_render_hang() {
    let Some((desc, ms, job, consecutive, total)) = mark_hung() else {
        return;
    };
    let Some(host) = with_state(|s| s.host) else {
        heal_failed();
        return;
    };
    // 用户还停在这一帧上（代际没变 + 窗口开着）才值得重发，否则没必要白占一条新线程。
    let requeue = with_state(|s| s.visible && s.render_gen == job.generation).unwrap_or(false);
    let (tx, epoch) = match spawn_render_worker(host.raw()) {
        Ok(pair) => pair,
        Err(e) => {
            log_error!(SCOPE, "RENDER 挂死 {ms}ms（{desc}），重建渲染线程失败：{e}");
            heal_failed();
            return;
        }
    };
    // 换发送端 + 让新代号生效 + 登记新租约 + 重发，四件事在同一把锁里按这个顺序做完：
    // - 代号必须先于投递：否则新线程可能在我们写代号之前就算完，结果被当成"旧线程"丢弃；
    // - 租约必须先于投递：否则"几微秒就算完"的那一帧交活时找不到租约（见 `heal_succeeded`）。
    // 锁序与 `request_render` 一致（JOB_TX → RENDER_TRACK），不会与渲染线程那把单锁成环。
    let swapped = match JOB_TX.get().map(|slot| slot.lock()) {
        Some(Ok(mut guard)) => {
            *guard = tx;
            RENDER_EPOCH.store(epoch, Ordering::Release);
            heal_succeeded(requeue.then_some(&job));
            if requeue {
                let _ = guard.send(job.clone());
            }
            true
        }
        _ => false,
    };
    if !swapped {
        // 只有锁被毒化才可能走到这里：新通道随 `tx` 一起丢弃，新线程自己退出。
        log_error!(SCOPE, "RENDER 挂死 {ms}ms（{desc}），重建后没换上发送端");
        heal_failed();
        return;
    }
    if requeue {
        // 重发也算"这一帧重新开始"：让 3 s 的取消告警重新计时。
        with_state(|s| {
            s.render_since = Some(Instant::now());
            s.render_warned = false;
        });
    }
    log_error!(
        SCOPE,
        "RENDER 挂死 {ms}ms（{desc}，代际 {}），已重建渲染线程 epoch={}（连续第 {consecutive} 次 / 本会话第 {total} 次）{}",
        job.generation,
        epoch,
        if requeue {
            "，这一帧已重发给新线程"
        } else {
            ""
        }
    );
}

/// 拉起一条渲染线程。返回 `(任务发送端, 线程代号)`。
///
/// 线程只拿 `host` 的裸句柄用于 `PostMessageW`（`HWND` 不是 `Send`，所以只能传 `isize`），
/// 绝不碰窗口的其他属性——窗口只能在 UI 线程上操作。
///
/// 代号**由调用方**在把发送端换进 `JOB_TX` 之后才写进 `RENDER_EPOCH`（见
/// `check_render_hang`）：换不上通道时新线程根本收不到活，代号也就不该生效，否则旧线程
/// 会立刻变成"已退役"，它下一次交活的结果被白白丢掉。
///
/// 测试钩子：`QLRUST_SLOW_RENDER_MS=5000` 让每个渲染任务先睡这么久（验证 1.2.0 的
/// 取消链路——真造一个要跑 5 s 的合法文件很难，"注入延迟"才能稳定复现看门狗的行为）；
/// `QLRUST_RENDER_HANG_MS` 让第一条线程的第一个任务睡（验证 1.8.3 的挂死自愈）。
/// 两者都只读一次环境变量，生产环境不设置时零开销。
fn spawn_render_worker(host: HWND) -> Result<(Sender<RenderJob>, u64), qlr_core::Error> {
    let (tx, rx) = mpsc::channel::<RenderJob>();
    let host_raw = host.0 as isize;
    let epoch = RENDER_EPOCH.load(Ordering::Acquire) + 1;
    // 挂死只注入到第一条线程：要验证的是"自愈之后能恢复"，新线程必须是好的。
    let hang_ms = if epoch == 1 { fake_hang_ms() } else { None };
    std::thread::Builder::new()
        .name("qlr-render".to_string())
        .spawn(move || {
            let mut hang_simulated = false;
            while let Ok(mut job) = rx.recv() {
                // 1.2.0：队列里堆着好几个过期的活（用户连按方向键）时，只做最新的那个。
                // 工作线程是单线程，不这么做"过期帧"会一个个渲染完再被丢弃——纯浪费。
                let mut dropped = 0u32;
                while let Ok(newer) = rx.try_recv() {
                    job = newer;
                    dropped += 1;
                }
                if dropped > 0 {
                    log_debug!(
                        SCOPE,
                        "RENDER 丢弃 {dropped} 个排队任务，只做最新的 gen={}",
                        job.generation
                    );
                }
                // 新活开跑：清掉上一条活留下的取消信号（否则这一帧一进来就自杀）。
                qlr_render::cancel::reset();
                let started = Instant::now();
                // 测试钩子（1.2.0）：注入一段假的慢渲染，用来验证看门狗与取消链路。
                // 只读一次环境变量，正常使用时不产生任何开销。
                if let Some(ms) = fake_slow_ms() {
                    log_warn!(SCOPE, "RENDER 注入慢渲染 {ms}ms（测试钩子）");
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                }
                // 测试钩子（1.8.3）：模拟"卡在第三方 FFI 里不返回"——不查取消标志、
                // 不响应任何协作式退出，只能靠 UI 线程换一条线程来恢复。
                if let Some(ms) = hang_ms.filter(|_| !hang_simulated) {
                    hang_simulated = true;
                    log_warn!(
                        SCOPE,
                        "RENDER 注入挂死 {ms}ms（测试钩子 QLRUST_RENDER_HANG_MS，仅开发用）"
                    );
                    std::thread::sleep(Duration::from_millis(ms));
                }
                // 测试钩子（1.2.0）：验证崩溃兜底（panic → 报告 + minidump）。
                // release 是 `panic = "abort"`，所以这里**一定会**把进程带走——
                // 这正是要验证的路径：进程没了，但证据必须留下。
                // 注意 `cmd /c "set X=1 && ..."` 里 `set` 会把 `&&` 前的空格算进值里
                // （值变成 `"1 "`），所以这里必须 trim 再比，否则测试钩子静默失效。
                if std::env::var("QLRUST_PANIC_TEST")
                    .ok()
                    .map(|v| v.trim().to_string())
                    .as_deref()
                    == Some("1")
                {
                    panic!("QLRust 崩溃测试（QLRUST_PANIC_TEST=1）");
                }
                // 内置查看器优先；内置不接的格式才问插件（"插件只补空缺，不抢内置"）。
                let kind = format::kind_for_path(&job.path);
                let result = if qlr_render::viewer_for(kind).is_some() {
                    qlr_render::render_file_at_zoom(
                        &job.path,
                        job.max_width,
                        job.max_height,
                        job.page,
                        job.upscale,
                    )
                } else {
                    render_with_plugin(&job.path, kind, job.max_width, job.max_height)
                };
                let elapsed = started.elapsed().as_millis();
                match &result {
                    Ok(canvas) => log_info!(
                        SCOPE,
                        "RENDER gen={} {} {}x{} bytes={} ink={} elapsed={}ms (请求 {}x{})",
                        job.generation,
                        canvas.kind_name(),
                        canvas.dimensions().0,
                        canvas.dimensions().1,
                        canvas.approx_bytes(),
                        canvas.ink_pixels(),
                        elapsed,
                        job.max_width,
                        job.max_height
                    ),
                    Err(e) => log_warn!(
                        SCOPE,
                        "RENDER gen={} 失败 elapsed={}ms：{e}",
                        job.generation,
                        elapsed
                    ),
                }
                // 1.8.3：自愈换线程之后，旧线程迟早会从第三方代码里爬出来交活。
                // 它的结果绝不能进结果槽、也不能通知 UI —— 那会顶掉新线程刚算好的那一帧
                // （UI 只看代际号，一代就对得上），或者把用户早翻过去的旧图糊上来。
                if RENDER_EPOCH.load(Ordering::Acquire) != epoch {
                    log_debug!(
                        SCOPE,
                        "RENDER gen={} 来自已退役的渲染线程（epoch={epoch}），丢弃",
                        job.generation
                    );
                    continue;
                }
                end_lease(epoch, job.generation);
                if let Some(slot) = RESULT.get()
                    && let Ok(mut guard) = slot.lock()
                {
                    *guard = Some((job.generation, result));
                }
                window::post(HWND(host_raw as *mut core::ffi::c_void), ipc::WM_QLR_RENDER);
            }
            log_debug!(SCOPE, "渲染线程退出");
        })
        .map_err(|e| qlr_core::Error::Message(format!("创建渲染线程失败：{e}")))?;
    Ok((tx, epoch))
}

/// 走插件渲染（只在渲染线程调用）。
///
/// 失败原因要能直接写给用户看：插件没装、插件报错、插件给了非法数据，三种情况文案不同。
fn render_with_plugin(
    path: &Path,
    kind: format::Kind,
    max_w: u32,
    max_h: u32,
) -> qlr_core::Result<Canvas> {
    let host = plugins::host();
    let Some(plugin) = host.find_for_path(path) else {
        return Err(qlr_core::Error::Message(format!(
            "{kind} 暂无内置查看器，也没有插件接管这个扩展名：{}",
            path.display()
        )));
    };
    let started = Instant::now();
    // 1.6.0：插件能给出"文本大纲"时优先用文本渲染（等宽、可滚动、缩放不发虚），
    // 位图那条路仍然保留给只画图的插件。
    if let Some(text) = plugin.text(path) {
        // 去掉成串的空行：插件的排版留白在文本视图里会变成"隔一行一个"，很浪费屏幕。
        let mut lines: Vec<String> = Vec::new();
        for raw in text.split('\n') {
            let line = raw.trim_end_matches('\r').to_string();
            if line.is_empty() && lines.last().is_some_and(String::is_empty) {
                continue;
            }
            lines.push(line);
        }
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        if !lines.is_empty() {
            log_info!(
                SCOPE,
                "PLUGIN-TEXT {} {} 行 elapsed={}ms",
                plugin.name(),
                lines.len(),
                started.elapsed().as_millis()
            );
            let total = lines.len();
            // 1.7.0：走**普通文本**通道（spans = None），不带行号栏 —— 插件给的是
            // "目录树/大纲"这类结构性内容，行号是给源码用的，加上反而像日志。
            return Ok(Canvas::Text(qlr_render::TextDoc {
                lines,
                styles: None,
                spans: None,
                total_lines: total,
                truncated: false,
                encoding: "utf-8",
            }));
        }
    }
    let image = plugin.render(path, max_w, max_h).map_err(|e| {
        log_warn!(SCOPE, "插件失败 {}", e);
        qlr_core::Error::Message(e)
    })?;
    log_info!(
        SCOPE,
        "PLUGIN {} {}x{} bytes={} elapsed={}ms (请求 {}x{})",
        plugin.name(),
        image.width,
        image.height,
        image.bytes(),
        started.elapsed().as_millis(),
        max_w,
        max_h
    );
    Ok(Canvas::Bitmap(qlr_render::Bitmap::from_rgba(
        image.width,
        image.height,
        image.rgba,
    )))
}

/// 单帧渲染的像素上限（32 MP ≈ 128 MB BGRA）。
///
/// 高倍缩放时"位图 = 自然尺寸 × zoom"会线性膨胀（8× 的 12 MP 图 = 768 MP），
/// 这里统一兜住：超了就等比缩回去，宁可放大后略微发虚，也不能把内存吃爆。
const MAX_RENDER_PIXELS: u64 = 32_000_000;

/// 把渲染目标夹进 `MAX_RENDER_PIXELS` 内（等比，保持宽高比）。
fn clamp_render_size(w: u32, h: u32) -> (u32, u32) {
    let (w, h) = (w.max(1) as u64, h.max(1) as u64);
    let pixels = w.saturating_mul(h);
    if pixels <= MAX_RENDER_PIXELS {
        return (w as u32, h as u32);
    }
    let scale = (MAX_RENDER_PIXELS as f64 / pixels as f64).sqrt();
    (
        ((w as f64 * scale) as u32).max(1),
        ((h as f64 * scale) as u32).max(1),
    )
}

/// 投一个渲染任务，返回本次的代际号。
fn request_render(path: &Path, max_w: u32, max_h: u32, page: usize) {
    let (max_w, max_h) = clamp_render_size(max_w, max_h);
    let is_pdf = with_state(|s| s.pages.is_some()).unwrap_or(false);
    // 1.3.0：PDF 页缓存。翻回去看一眼是 PDF 最常见的动作，命中就别再麻烦渲染线程。
    // 注意代际也要 +1：否则先前那一帧算完回来会把刚上屏的这一页顶掉。
    if is_pdf {
        let key = pagecache::PageKey::new(path, page, max_w, max_h);
        if let Some(canvas) = pagecache::get(&key) {
            let generation = with_state(|s| {
                s.render_gen = s.render_gen.wrapping_add(1);
                s.render_gen
            })
            .unwrap_or(0);
            log_info!(
                SCOPE,
                "PAGECACHE 命中 gen={generation} page={} {}x{}（跳过渲染，缓存 {} 页/{} MB）",
                page + 1,
                max_w,
                max_h,
                pagecache::stats().0,
                pagecache::stats().1 / (1024 * 1024)
            );
            with_state(|s| {
                s.render_since = None;
                s.render_warned = false;
                s.pending_key = None;
            });
            present_canvas(canvas);
            return;
        }
    }
    let generation = with_state(|s| {
        s.render_gen = s.render_gen.wrapping_add(1);
        s.render_gen
    })
    .unwrap_or(0);
    let Some(tx_slot) = JOB_TX.get() else {
        log_warn!(SCOPE, "渲染线程未就绪，跳过渲染");
        return;
    };
    let Ok(tx) = tx_slot.lock() else { return };
    // 1.2.0：新请求发出去之前，先让**正在跑**的那个活自己退出（阶段边界检查点会看到），
    // 否则用户连按方向键时，工作线程要先把前面几个没人要的文件渲染完。
    qlr_render::cancel::cancel();
    let is_pdf = with_state(|s| s.pages.is_some()).unwrap_or(false);
    // 注意：`with_state` 是普通 `Mutex`，**嵌套调用会自己把自己锁死**（这里踩过：
    // 在闭包里又调 `with_state(|s| s.zoom > 1.0)`，进程卡在开窗那一步，日志只到"开窗尺寸"）。
    // 所以先把要用的值取出来，再进闭包。
    let upscale = with_state(|s| s.zoom > 1.0).unwrap_or(false);
    let frame = frame_key(path, page, max_w, max_h, upscale);
    with_state(|s| {
        s.render_since = Some(Instant::now());
        s.render_warned = false;
        // PDF 这一帧算完回来要进页缓存，先把 key 记下来（非 PDF 不记）。
        s.pending_key = if is_pdf {
            Some(pagecache::PageKey::new(path, page, max_w, max_h))
        } else {
            None
        };
        // 任何格式都记"上一帧"的 key：重开同一个文件时可以直接上屏（1.4.0）。
        s.pending_frame = Some(frame);
    });
    let job = RenderJob {
        generation,
        path: path.to_path_buf(),
        max_width: max_w,
        max_height: max_h,
        page,
        upscale: with_state(|s| s.zoom > 1.0).unwrap_or(false),
    };
    // 1.8.3：登记租约（先登记再投递，顺序理由见 `begin_lease`）。
    begin_lease(&job);
    if tx.send(job).is_err() {
        abort_lease(generation);
        log_warn!(SCOPE, "渲染线程已退出，丢弃本次任务");
    }
}

/// 渲染看门狗（1.2.0）：每 250 ms 看一次"这一帧跑了多久"。
///
/// 只做两件事：**记一条 warn**（带毫秒数，便于事后判断是解析器卡住还是慢盘）、
/// **请求取消**（协作式：渲染器在下一个阶段边界退出；单个 FFI 调用打断不了，
/// 这条限制写在 `qlr_render::cancel` 的模块文档里）。UI 线程绝不因此阻塞——
/// 用户随时还能按空格换文件、按 × 关窗口。
///
/// 1.8.3 起多一件事：**判挂换线程**（`check_render_hang`）。取消只在渲染器肯查标志时
/// 有用；卡死在第三方 FFI 里的线程连标志都摸不到，只能换一条新的顶上去。
fn on_render_watchdog() {
    check_render_hang();
    let slow = with_state(|s| {
        let since = s.render_since?;
        let ms = since.elapsed().as_millis();
        if ms < RENDER_TIMEOUT_MS {
            return None;
        }
        let first = !s.render_warned;
        s.render_warned = true;
        Some((ms, first))
    })
    .flatten();
    let Some((ms, first)) = slow else {
        return;
    };
    if !first {
        return;
    }
    log_warn!(
        SCOPE,
        "RENDER 超时 {ms}ms —— 已请求取消这一帧（窗口仍可关闭/换文件）"
    );
    qlr_render::cancel::cancel();
    // 只有当窗口现在显示的是"占位文字"时才改文案；已经有内容（缩放重渲染）就别动，
    // 否则用户正看着的图会被一行提示顶掉。
    if get_content().is_none() {
        set_lines(vec![
            version::banner(),
            format!(
                "这一帧渲染超过 {} s，正在请求取消…",
                RENDER_TIMEOUT_MS / 1000
            ),
            String::new(),
            "换一个文件或按 × 关闭即可（渲染线程会自己接着做下一帧）".to_string(),
        ]);
        if let Some(preview) = with_state(|s| s.preview)
            && with_state(|s| s.visible).unwrap_or(false)
        {
            window::invalidate(preview.raw());
        }
    }
}

/// UI 线程收到 `WM_QLR_RENDER`：取结果、校验代际、上屏。
fn on_render_done() {
    let taken = RESULT
        .get()
        .and_then(|slot| slot.lock().ok().and_then(|mut guard| guard.take()));
    let Some((generation, result)) = taken else {
        return;
    };
    // 有结果（哪怕是失败/取消）就算这一帧结束，看门狗从零开始计。
    with_state(|s| {
        s.render_since = None;
        s.render_warned = false;
    });
    let Some((current, visible, preview)) = with_state(|s| (s.render_gen, s.visible, s.preview))
    else {
        return;
    };
    if generation != current {
        // 用户已经翻到下一个文件了，这一帧作废。
        log_debug!(SCOPE, "丢弃过期帧 gen={generation} current={current}");
        return;
    }

    // 被更新的请求取消掉的帧：不是失败，静默丢弃（屏幕上继续显示上一帧/占位）。
    if let Err(e) = &result
        && e.is_cancelled()
    {
        log_debug!(SCOPE, "RENDER gen={generation} 已取消（有更新的请求）");
        return;
    }

    let is_bitmap = matches!(&result, Ok(Canvas::Bitmap(_)));
    let rendered_text = matches!(&result, Ok(Canvas::Text(_)));
    // 新的一帧到位了，缩放预览态结束（之后的绘制按 1:1 裁剪/居中）。
    with_state(|s| s.zoom_preview = false);
    match result {
        Ok(canvas) => {
            let canvas = Arc::new(canvas);
            // 1.3.0：PDF 这一帧进页缓存（翻回去就不用再算），并顺手预热相邻页。
            if let Some(key) = with_state(|s| s.pending_key.take()).flatten()
                && matches!(canvas.as_ref(), Canvas::Bitmap(_))
            {
                pagecache::put(key.clone(), canvas.clone());
                prefetch_neighbor_pages(&key);
            }
            // 1.4.0：把这一帧记成"上一帧"，重开同一个文件时直接上屏。
            if let Some(key) = with_state(|s| s.pending_frame.take()).flatten() {
                remember_last_frame(key, canvas.clone());
            }
            set_content(Some(canvas));
            // 内容不透明（1.5.0 起不再有亚克力材质），只保证 DWM backdrop 关掉。
            if is_bitmap || rendered_text {
                win11::clear_backdrop(preview.raw());
            }
            if visible {
                window::invalidate(preview.raw());
            }
        }
        Err(e) => {
            set_content(None);
            set_lines(vec![
                version::banner(),
                format!("渲染失败：{e}"),
                String::new(),
                "点击窗口关闭 · 托盘右键菜单退出".to_string(),
            ]);

            if visible {
                window::invalidate(preview.raw());
            }
        }
    }
}

/// 一帧内容到位：上屏 + 挑材质 + 重画。
///
/// 正常渲染（`on_render_done`）与预渲染命中（`show_preview`）共用这条路，
/// 免得其中一条忘了关亚克力 → 图片被壁纸洗白（1.0.1 踩过的坑）。
/// 上一帧的 key（1.4.0）：路径 + 文件戳 + 页号 + 渲染尺寸 + 是否放大。
///
/// 为什么要文件戳：同一个路径的文件可能被改过（编辑器保存、下载覆盖），
/// 只比路径的话会把旧内容当成新的显示出来。
#[derive(Clone, Debug, PartialEq, Eq)]
struct FrameKey {
    path: PathBuf,
    mtime: u64,
    size: u64,
    page: usize,
    width: u32,
    height: u32,
    upscale: bool,
}

/// 上一帧本身：只留一帧（和 `PREPARED` 一个思路）。
///
/// 内存代价接近 0：这一帧同时被 `CONTENT` 和这里各持一个 `Arc`，是同一块内存；
/// 真正多出来的只是"换文件之后旧那一帧还活着"的那几 MB。
/// 上一帧缓存最多留几帧：3 帧足够覆盖"在 A 和 B 之间来回看"的常见用法。
const LAST_FRAME_CAP: usize = 3;

/// 上一帧缓存的总内存上限（字节）。1.8.3：从"只数帧数"改成"帧数 + 字节"双限。
///
/// 为什么必须按字节兜一道：一帧位图的体积 = 宽 × 高 × 4，跟文件格式无关——
/// 1080p 一帧 5.7 MB，放大预览（8×，按 32 MP 上限截断）一帧能到 128 MB。
/// 只数帧数的话"3 帧"最坏是 ~260 MB，而用户换文件时对此毫无感知，
/// 收盘后内存一直挂着（实测每预览一个文件 Private 就 +5.7 MB 且收起不回落）。
///
/// 24 MB 这个数的取舍：
/// - 1080p（5.7 MB/帧）能留 4 帧，"A/B 来回看"这个主要收益还在；
/// - 换过 4 个 1080p 文件后常驻约 23 MB，而不是"一人一帧"线性涨到几十 MB；
/// - 单帧就超过 24 MB 时（大窗口 / 放大预览）靠 `trim_last_frames` 里的
///   `len() > 1` 仍然保留最近 1 帧——"同一文件重开秒开"这条最大收益不能整条扔掉。
const LAST_FRAME_MAX_BYTES: usize = 24 * 1024 * 1024;

/// 上一帧们的 LRU：头是最久没用到，尾是刚用过。
type LastFrames = std::collections::VecDeque<(FrameKey, Arc<Canvas>)>;
static LAST_FRAMES: OnceLock<Mutex<LastFrames>> = OnceLock::new();

fn last_frame_slot() -> &'static Mutex<LastFrames> {
    LAST_FRAMES.get_or_init(|| Mutex::new(std::collections::VecDeque::new()))
}

/// 取文件戳（mtime 秒 + 字节数）。拿不到就是 (0, 0)，此时缓存按"永不过期"处理。
fn file_stamp(path: &Path) -> (u64, u64) {
    match std::fs::metadata(path) {
        Ok(meta) => {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (mtime, meta.len())
        }
        Err(_) => (0, 0),
    }
}

/// 组装一帧的 key。
fn frame_key(path: &Path, page: usize, width: u32, height: u32, upscale: bool) -> FrameKey {
    let (mtime, size) = file_stamp(path);
    FrameKey {
        path: path.to_path_buf(),
        mtime,
        size,
        page,
        width,
        height,
        upscale,
    }
}

/// 上一帧还在吗（且就是这个 key）？命中就把整帧交出去，调用方直接上屏、不用再渲染。
fn take_last_frame(key: &FrameKey) -> Option<Arc<Canvas>> {
    let mut guard = last_frame_slot().lock().ok()?;
    let pos = guard.iter().position(|(stored, _)| stored == key)?;
    // 命中就挪到队尾（最近使用），这样"在 A/B 之间来回看"不会把 A 挤掉。
    let entry = guard.remove(pos)?;
    let canvas = entry.1.clone();
    guard.push_back(entry);
    Some(canvas)
}

fn remember_last_frame(key: FrameKey, canvas: Arc<Canvas>) {
    let Ok(mut guard) = last_frame_slot().lock() else {
        return;
    };
    // 同一个 key 再来一次就替换（内容以新的为准），并挪到队尾。
    if let Some(pos) = guard.iter().position(|(stored, _)| stored == &key) {
        guard.remove(pos);
    }
    guard.push_back((key, canvas));
    trim_last_frames(&mut guard);
}

/// 一帧占多少字节（只用于"上一帧缓存"的预算，不做精确统计）。
fn canvas_bytes(canvas: &Canvas) -> usize {
    match canvas {
        Canvas::Bitmap(b) => b.bgra.len(),
        // 文本帧是重排结果，量级只有几十 KB：按行估一个偏大的上限即可。
        Canvas::Text(t) => t.lines.len().saturating_mul(256),
    }
}

/// 淘汰最旧的帧，直到同时满足"帧数 ≤ [`LAST_FRAME_CAP`]"和"总字节 ≤ [`LAST_FRAME_MAX_BYTES`]"。
///
/// 唯一的例外是**至少保留最近 1 帧**：单帧就可能超预算（放大预览一帧 128 MB），
/// 这时限额给"同一文件重开秒开"让路——宁可超一点，也不要每次重开都重新渲染。
fn trim_last_frames(frames: &mut LastFrames) {
    while frames.len() > LAST_FRAME_CAP {
        frames.pop_front();
    }
    let mut total: usize = frames.iter().map(|(_, canvas)| canvas_bytes(canvas)).sum();
    while total > LAST_FRAME_MAX_BYTES && frames.len() > 1 {
        let Some((_, evicted)) = frames.pop_front() else {
            break;
        };
        total = total.saturating_sub(canvas_bytes(&evicted));
    }
}

/// 试一把系统预览处理器（1.4.1）：只对"内置四类之外、且有插件接管"的文件尝试，
/// 因为这类文件我们自己画得最简陋，系统处理器画得最好。
///
/// 成功返回 true（调用方跳过插件渲染），失败返回 false（照常走插件卡片）。
/// 只判断"要不要试系统预览处理器"，不做任何 COM 调用（COM 部分要等窗口显示后再做）。
fn shell_candidate(path: Option<&Path>) -> bool {
    let Some(path) = path else {
        return false;
    };
    let kind = format::kind_for_path(path);
    if qlr_render::viewer_for(kind).is_some() {
        // 图片/PDF/文本/Markdown 由我们自己画，且我们的交互（缩放/滚动/目录）比处理器好。
        return false;
    }
    if qlr_core::plugins::host().find_for_path(path).is_none() {
        // 没插件 = 本来就不支持（预览窗根本不会开），不用试。
        return false;
    }
    shellpreview::has_handler(path)
}

/// 系统预览处理器的"延迟拆卸"队列。
///
/// 为什么不能就地 `Drop`：`ShellPreview::drop` 要 `Unload()`，而收起窗口的调用链常常发生在
/// **`WM_COPYDATA` 处理期间**（另一个进程用 `SendMessage` 同步等我们）。在这种调用栈里进 COM
/// 会死锁（表现为窗口留在屏幕上、日志断在这里、进程"还活着"但不再响应）。
/// 所以：把句柄挪进这个队列，`PostMessage(WM_QLR_INVOKE)` 回到自己的消息循环里再卸。
static SHELL_TRASH: OnceLock<Mutex<Vec<shellpreview::ShellPreview>>> = OnceLock::new();

fn shell_trash() -> &'static Mutex<Vec<shellpreview::ShellPreview>> {
    SHELL_TRASH.get_or_init(|| Mutex::new(Vec::new()))
}

fn retire_shell_preview(handle: Option<shellpreview::ShellPreview>) {
    let Some(handle) = handle else {
        return;
    };
    if let Ok(mut slot) = shell_trash().lock() {
        slot.push(handle);
    }
    // 提醒消息循环来收尸（posted，不是 send —— 这里绝不能是同步调用）。
    if let Some(host) = with_state(|s| s.host) {
        window::post(host.raw(), ipc::WM_QLR_INVOKE);
    }
}

/// 在自己的消息循环里真正卸掉处理器（此时已脱离 `WM_COPYDATA` 调用栈）。
fn drain_shell_trash() {
    let pending: Vec<shellpreview::ShellPreview> = match shell_trash().lock() {
        Ok(mut slot) => std::mem::take(&mut *slot),
        Err(_) => return,
    };
    if pending.is_empty() {
        return;
    }
    let count = pending.len();
    drop(pending);
    log_debug!(SCOPE, "SHELL 预览已卸下 {count} 个");
}

/// 换到"不需要系统预览处理器"的文件时，把上一个文件的处理器当场卸掉（1.7.5）。
///
/// `open_shell_preview` 只在"这次也要用处理器"时才会顺带收尸，于是从 docx/pptx/xlsx
/// 切到 zip/图片/文本时，处理器的子窗口会留在预览窗里继续盖着内容——用户看到的是
/// 「顶栏是新的文件名、正文还是上一个文件」；拖动/改大小时那片残留子窗口由别人按旧尺寸
/// 重绘，还会出现竖条纹。调用点：`show_preview` 的压缩包分支 + 非 `shell_wanted` 的主路径。
fn drop_stale_shell_preview() {
    // 注意：不能就地 drop（会把 COM 带进 `WM_COPYDATA` 的同步调用栈里，见 SHELL_TRASH 注释）。
    let stale = with_state(|s| s.shell_preview.take()).flatten();
    if stale.is_none() {
        return;
    }
    retire_shell_preview(stale);
    with_state(|s| {
        s.slide = None;
        s.shell_input_pending = false;
    });
    if let Some(preview) = with_state(|s| s.preview) {
        unsafe {
            let _ = KillTimer(Some(preview.raw()), TIMER_SHELL_INPUT);
        }
    }
    // 处理器卸掉了：滚轮回到我们自己手里（缩放/滚文本），窗口也回到"不抢焦点"。
    if let Some(preview) = with_state(|s| s.preview) {
        window::set_activatable(preview.raw(), false);
    }
    hotkey::set_wheel_pass_through(false);
    log_debug!(SCOPE, "换文件：卸掉上一个文件的系统预览处理器");
}

fn open_shell_preview(path: Option<&Path>) -> bool {
    // 上一个"遮盖用"的旧处理器先卸掉（换两次文件可能都没走到换帧定时器）。
    let cover = with_state(|s| s.shell_cover.take()).flatten();
    retire_shell_preview(cover);
    // 换文件时（`hold_frame` 有效）**先不卸旧的**：让它继续盖着内容区，等新的处理器画出
    // 第一帧再换（`TIMER_SHELL_SWAP`）。否则中间几十到几百毫秒露出空白/占位页。
    let hold = with_state(|s| s.hold_frame).unwrap_or(false);
    let mut old = with_state(|s| s.shell_preview.take()).flatten();
    if !hold {
        // 注意：不能就地 drop（会把 COM 带进 `WM_COPYDATA` 的同步调用栈里，见 SHELL_TRASH 注释）。
        retire_shell_preview(old.take());
    }
    with_state(|s| s.slide = None);
    let Some(path) = path else {
        retire_shell_preview(old.take());
        return false;
    };
    let Some(preview) = with_state(|s| s.preview) else {
        retire_shell_preview(old.take());
        return false;
    };
    // 子窗口的矩形要**在这里就定下来**（不接管输入时内缩一圈）。
    //
    // 为什么不能等打开后再 `sync_shell_rect()` 去调：`set_rect` 会走
    // `IPreviewHandler::SetWindow`，而 Excel 的预览（OLE 就地激活）在刚 `DoPreview`
    // 之后是**不能立刻再进 COM** 的——实测直接卡死（`Responding=False`，日志停在
    // "SHELL 预览已接管"，用户看到的就是占位页不动 + 滚动条全废）。
    let owns_input = is_slides(path);
    with_state(|s| s.shell_owns_input = owns_input);
    // Office（Word/Excel）的预览是 OLE 就地激活对象：容器窗口必须先"可激活"，
    // 否则滚轮/滚动条/选区全都不响应（对比：资源管理器预览窗格里同一个 xlsx 可以滚）。
    window::set_activatable(preview.raw(), !owns_input);
    let content = shell_content_rect(preview.raw(), owns_input);
    match shellpreview::ShellPreview::open(path, preview.raw(), content) {
        Some(handle) => {
            with_state(|s| s.shell_preview = Some(handle));
            // 旧内容顶到最前面盖住新的（新子窗口建出来就在最上层，会被它挡住）。
            if let Some(previous) = old.take() {
                previous.raise();
                with_state(|s| s.shell_cover = Some(previous));
                let _ = unsafe {
                    SetTimer(
                        Some(preview.raw()),
                        TIMER_SHELL_SWAP,
                        SHELL_SWAP_DELAY_MS,
                        None,
                    )
                };
                log_debug!(SCOPE, "换帧：旧内容继续盖住，等新处理器出首帧");
            }
            log_info!(
                SCOPE,
                "SHELL 预览已接管 {}（内容由系统预览处理器绘制）",
                path.display()
            );
            // 输入接管（1.7.4 起，1.7.7 收窄到"只有 PPT"）。
            //
            // 历史：1.7.4 为了让 Office 预览也能拖边改大小，对所有处理器都
            // `EnableWindow(宿主, FALSE)`。代价是 **Excel/Word 的滚动条、工作表标签、选区全都点不动**
            // （用户反馈"滚动栏都不可用"）。1.7.7 改成：
            //   - PPT：整块接管（翻页由我们计数，点一下就翻页会让计数失准）；
            //   - 其余：**不接管**，只把子窗口内缩一圈（`sync_shell_rect`），窗口四条边照样能拖。
            //
            // 另外不再按固定 400 ms 猜：Excel 的 `DoPreview` 实测要 768 ms，
            // 猜早了就露出占位页（用户反馈"Excel 还有那个占位页"）。改成轮询
            // `ShellPreview::visible_area`，画出来才收尾（`TIMER_SHELL_INPUT` 每次 120 ms）。
            let owns_input = is_slides(path);
            // 不接管输入的（Word/Excel）把滚轮**放行**给内容子窗口，让处理器自己滚；
            // 接管了的（PPT）继续由我们吞滚轮做翻页。
            hotkey::set_wheel_pass_through(!owns_input);
            with_state(|s| {
                s.shell_input_pending = owns_input;
                s.shell_poll = 0;
            });
            if let Some(preview) = with_state(|s| s.preview) {
                let _ = unsafe {
                    SetTimer(Some(preview.raw()), TIMER_SHELL_INPUT, SHELL_POLL_MS, None)
                };
            }
            // PPT：翻页也由我们接管。为什么必须接管：处理器的"放映"没有"跳页"接口，
            // 我们只能给它投 `VK_NEXT`/`VK_PRIOR`；而它自己**翻过最后一页就切到黑屏
            // "放映结束"**（用户截图反馈的"PPT 最后一页"）。接管后由我们数页、到头停住并提示。
            if let Some(total) = slide_count(path) {
                with_state(|s| s.slide = Some((1, total)));
                log_info!(
                    SCOPE,
                    "SHELL 接管翻页：共 {total} 页（PageDown/PageUp/滚轮）"
                );
            }
            true
        }
        None => {
            // 新处理器起不来：旧的还得卸掉（否则它会一直盖着插件渲染出来的内容）。
            retire_shell_preview(old.take());
            log_info!(SCOPE, "SHELL 预览不可用 {}（退回插件卡片）", path.display());
            false
        }
    }
}

/// 这个文件是不是"一页一页放"的演示文稿（PPT 家族）。
///
/// 只认 pptx 家族：它们内部是 zip，页数能从目录里数出来；`.ppt`（旧二进制格式）数不出来，
/// 交回给系统处理器自己放（不接管就不会误判页码）。
fn is_slides(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("pptx" | "pptm" | "potx" | "potm" | "ppsx" | "ppsm")
    )
}

/// 数 pptx 里有多少页（`ppt/slides/slideN.xml` 的条目数）。数不出来返回 `None`。
fn slide_count(path: &Path) -> Option<usize> {
    if !is_slides(path) {
        return None;
    }
    let archive = qlr_render::archive::open(path).ok()?;
    let total = archive
        .entries
        .iter()
        .filter(|entry| {
            !entry.is_dir
                && entry.path.starts_with("ppt/slides/slide")
                && entry.path.ends_with(".xml")
        })
        .count();
    (total > 0).then_some(total)
}

/// 翻一页 PPT（我们自己接管的通道）。到头就停住并给提示。
///
/// 返回 true = 这次翻页请求已经被"幻灯片通道"吃掉了，调用方不要再走文本/PDF 那套。
fn step_slide(delta: isize) -> bool {
    let Some((current, total)) = with_state(|s| s.slide).flatten() else {
        return false;
    };
    let Some(hwnd) = with_state(|s| s.preview).map(|p| p.raw()) else {
        return false;
    };
    let next = (current as isize + delta).clamp(1, total as isize) as usize;
    if next == current {
        // 到头了：**不再投给处理器**（它翻过最后一页会切到黑屏"放映结束"），
        // 只在顶栏给一句提示——这就是用户要的"到最后一页就停住并提示"。
        let text = if delta > 0 {
            "已是最后一页"
        } else {
            "已是第一页"
        };
        show_toast(hwnd, text);
        log_debug!(SCOPE, "SHELL 翻页：{text}（{current}/{total}）");
        return true;
    }
    let went = with_state(|s| {
        s.shell_preview
            .as_ref()
            .map(|handle| handle.step(delta > 0))
            .unwrap_or(false)
    })
    .unwrap_or(false);
    if !went {
        // 处理器不认按键（换了别的预览器）：别给用户"按了没反应"的错觉。
        show_toast(hwnd, "这个演示文稿不支持翻页");
        return true;
    }
    with_state(|s| s.slide = Some((next, total)));
    window::invalidate(hwnd);
    log_debug!(SCOPE, "SHELL 翻页：{next}/{total}");
    true
}

/// 窗口尺寸稳下来之后：让系统预览处理器的内容跟上新尺寸（1.7.9）。
///
/// 背景（用户反馈「窗口伸缩会引起白边」）：Word/Excel 的预览是 OLE 就地激活对象，
/// 它自己建的子窗口**不会**跟着我们的窗口变——把预览窗拉宽，右边就多出一条空白。
/// （1.7.2 时代 Office 走插件位图渲染，位图会随窗口重画，所以那时候没这个问题。）
///
/// **试过但不可行的路子**（都实机验证过，别再走一遍）：
/// - 在 `WM_SIZE` 里直接 `SetWindow/SetRect`：被系统拒（`0x8001010D`
///   `RPC_E_CANTCALLOUT_ININPUTSYNCCALL`，那时还在输入同步呼叫里）。
/// - 投递一条消息、等退栈后再 `SetWindow/SetRect` + 手工撑 `EXCEL9` + 兜底 `fill_child`：
///   进程**卡死**（事件日志 `Application Hang` 1002）。Excel 就地激活对象在改大小路径上
///   进 COM，很容易自等（同一类坑见 CONVENTIONS #139）。
/// - 照原版 QuickLook `PreviewHandlerHost.OnResize` 那样只调 `SetRect`（它靠"下一个
///   resize 事件再试一次"兜住 `0x8001010D`）：我们也卡死——原版宿主在 WinForms 里、
///   处理器常驻 `prevhost`，跨进程调用不会把 UI 线程拖住；我们是同进程调用。
///
/// 所以唯一可行的是：**等输入同步呼叫退栈后**（投递消息里）再调 `SetRect`，**并且全程不持锁**
/// （`SetRect` 会把消息泵回我们自己的窗口过程，持锁调用必然自等——CONVENTIONS #139 那一族；
/// 之前的卡死就是这一条，不是 COM 本身不行）。
/// 这样改大小只是"让处理器重新排版"，不重开预览、不重新 `DoPreview`、不丢页码。
fn fit_shell_to_window() {
    if with_state(|s| s.resize_edges.is_some()).unwrap_or(false) {
        // 还在拖：拖完 `finish_resize` 会再投一次。
        return;
    }
    sync_shell_rect();
}

/// 内容区变了（拖边改大小、缩放、换页）：把新矩形告诉系统预览处理器。
///
/// 1.7.7：不接管输入时（Word/Excel…）子窗口**内缩一圈**——那一圈归我们，拖边改大小照旧，
/// 而内容区里的滚动条、工作表标签、选区都还是处理器自己的（能点、能拖）。
fn sync_shell_rect() {
    let Some(preview) = with_state(|s| s.preview) else {
        return;
    };
    let owns = with_state(|s| s.shell_owns_input).unwrap_or(false);
    let content = shell_content_rect(preview.raw(), owns);
    // **锁外**调 COM：`SetRect` 会把消息泵回我们自己的窗口过程，持锁调用必然自等（#139 那一族）。
    // 所以先把句柄从 `State` 里取出来，调用完再放回去；这段窗口里别的路径看到 `None`，
    // 只会跳过"跟处理器有关"的操作，不会崩。
    let Some(mut handle) = with_state(|s| s.shell_preview.take()).flatten() else {
        return;
    };
    log_debug!(SCOPE, "处理器重新对齐到 {content:?}");
    handle.set_rect(content);
    with_state(|s| {
        s.shell_preview = Some(handle);
    });
}

/// 系统预览处理器该占的那块矩形（1.7.7）。
///
/// 接管输入的（PPT）铺满内容区；其余内缩一圈，把窗口四条边留给"拖边改大小"。
fn shell_content_rect(preview: HWND, owns_input: bool) -> RECT {
    let content = window::chrome_rects(window::client_rect(preview)).content;
    if owns_input {
        return content;
    }
    let ring = dpi::scaled(preview, SHELL_RING_DIP);
    shell_inset(content, ring)
}

/// 内容区四边各收 `ring` 像素（下限 80×60，窗口很小时不至于把子窗口压没）。
fn shell_inset(content: RECT, ring: i32) -> RECT {
    let ring = ring.max(0);
    let left = content.left + ring;
    let top = content.top + ring;
    let right = (content.right - ring).max(left + 80);
    let bottom = (content.bottom - ring).max(top + 60);
    RECT {
        left,
        top,
        right,
        bottom,
    }
}

/// 滚轮转给系统预览处理器（它自己不监听我们的全局钩子）。
///
/// 返回 true 表示"已经转出去了"，调用方就不要再自己滚/缩放。
///
/// 1.7.7 起只有"接管输入"的处理器（目前只有 PPT）才需要转发：输入没被接管时
/// （Word/Excel）鼠标本来就在子窗口上，滚轮由它自己处理，我们再投一次就成了双倍滚动。
fn forward_wheel_to_shell(delta: i16) -> bool {
    let owns = with_state(|s| s.shell_owns_input).unwrap_or(false);
    if !owns {
        return with_state(|s| s.shell_preview.is_some()).unwrap_or(false);
    }
    with_state(|s| {
        s.shell_preview
            .as_ref()
            .map(|handle| handle.forward_wheel(delta))
    })
    .flatten()
    .unwrap_or(false)
}

/// 缓存里的这一帧是不是按"这次要用的尺寸"渲出来的。
///
/// 尺寸不一致还拿来用的话，上屏那一侧会走拉伸路径：画面发虚、每帧重采样（拖动掉帧、滚动频闪）。
fn frame_matches_size(canvas: &Canvas, width: u32, height: u32) -> bool {
    match canvas {
        Canvas::Bitmap(bitmap) => bitmap.width == width && bitmap.height == height,
        // 文本是无损重排的，尺寸无关（重新排版即可）。
        Canvas::Text(_) => true,
    }
}

fn present_canvas(canvas: Arc<Canvas>) {
    let Some(preview) = with_state(|s| s.preview) else {
        return;
    };
    let visible = with_state(|s| s.visible).unwrap_or(false);
    let opaque = matches!(canvas.as_ref(), Canvas::Bitmap(_) | Canvas::Text(_));
    with_state(|s| s.zoom_preview = false);
    set_content(Some(canvas));
    let _ = opaque;
    win11::clear_backdrop(preview.raw());
    if visible {
        window::invalidate(preview.raw());
    }
}

// ---------------------------------------------------------------------------
// 投机预渲染（1.1.0）
// ---------------------------------------------------------------------------

/// 预渲染留下的那一帧：只留一格，尺寸/路径都对得上才用，否则直接丢（省内存）。
struct Prepared {
    path: PathBuf,
    width: u32,
    height: u32,
    canvas: Arc<Canvas>,
}

/// 预渲染线程收到的任务（1.3.0 起有两种）：塞进单槽给"按空格"用，还是只写页缓存。
enum WarmTask {
    /// 给"用户选中了这个文件、可能要按空格"准备的（1.1.0 的投机预渲染）。
    Prepared(RenderJob),
    /// 给"用户正在看这个 PDF 的相邻页"准备的，结果只进页缓存（1.3.0）。
    Page(RenderJob),
}

/// 预渲染结果的槽（和 `RESULT` 分开：它不进 UI 队列，也不占代际号）。
static PREPARED: OnceLock<Mutex<Option<Prepared>>> = OnceLock::new();
/// 预渲染线程的投递口。
static PREWARM_TX: OnceLock<Mutex<Option<Sender<WarmTask>>>> = OnceLock::new();

fn prepared_slot() -> &'static Mutex<Option<Prepared>> {
    PREPARED.get_or_init(|| Mutex::new(None))
}

/// 预渲染的图像上限（像素）。20 MP ≈ 80 MB BGRA，超过就不值得为一个"可能不按"的文件预花。
const PREWARM_MAX_PIXELS: u64 = 20_000_000;
/// 文本类预热上限（字节）：和查看器的 8 MB 读入上限一致，再大预热也只是一部分。
const PREWARM_MAX_TEXT_BYTES: u64 = 8 * 1024 * 1024;
/// PDF 预热上限（字节）：30 MB 之外就别预读了（200 页的扫描件预渲染一次要几百毫秒）。
const PREWARM_MAX_PDF_BYTES: u64 = 30 * 1024 * 1024;
/// 视频预热上限（字节）：首帧解码要过 Media Foundation，100 MB 之外不预。
const PREWARM_MAX_VIDEO_BYTES: u64 = 100 * 1024 * 1024;

/// 开一条独立的预渲染线程。
///
/// 为什么不复用 `qlr-render` 那条：预热是"抢在用户按空格之前先算"，一旦用户真按了，
/// 真正要看的这一帧不能排在预热后面等——两条独立线程，最坏情况只是多占几毫秒 CPU。
fn spawn_prewarm_worker() -> qlr_core::Result<()> {
    let (tx, rx) = mpsc::channel::<WarmTask>();
    std::thread::Builder::new()
        .name("qlr-prewarm".to_string())
        .spawn(move || {
            while let Ok(task) = rx.recv() {
                match task {
                    WarmTask::Page(job) => warm_page(&job),
                    WarmTask::Prepared(job) => warm_prepared(&job),
                }
            }
            log_debug!(SCOPE, "预渲染线程退出");
        })
        .map_err(|e| qlr_core::Error::Message(format!("创建预渲染线程失败：{e}")))?;
    if let Ok(mut guard) = PREWARM_TX.get_or_init(|| Mutex::new(None)).lock() {
        *guard = Some(tx);
    }
    Ok(())
}

/// 投机预渲染：算好一帧塞进单槽，等用户按空格。
fn warm_prepared(job: &RenderJob) {
    // 排队期间用户可能已经换了选中项：先看一眼还值不值得算。
    if !prewarm_still_wanted(&job.path) {
        log_debug!(SCOPE, "PREWARM 跳过（已过时）：{}", job.path.display());
        return;
    }
    let kind = format::kind_for_path(&job.path);
    if qlr_render::viewer_for(kind).is_none() {
        // 插件渲染一律不预热：插件是第三方代码，不该在"用户没要求"时跑。
        return;
    }
    let started = Instant::now();
    let result =
        qlr_render::render_file_at_zoom(&job.path, job.max_width, job.max_height, job.page, false);
    let elapsed = started.elapsed().as_millis();
    match result {
        Ok(canvas) => {
            // 算完再确认一次：这中间用户可能已经选到别的文件了，别把过期帧塞进槽。
            if !prewarm_still_wanted(&job.path) {
                log_debug!(
                    SCOPE,
                    "PREWARM 丢弃（算完已过时）{} elapsed={}ms",
                    job.path.display(),
                    elapsed
                );
                return;
            }
            if let Ok(mut guard) = prepared_slot().lock() {
                *guard = Some(Prepared {
                    path: job.path.clone(),
                    width: job.max_width,
                    height: job.max_height,
                    canvas: Arc::new(canvas),
                });
            }
            log_info!(
                SCOPE,
                "PREWARM 就绪 {} {}x{} elapsed={}ms",
                job.path.display(),
                job.max_width,
                job.max_height,
                elapsed
            );
        }
        Err(e) => log_debug!(
            SCOPE,
            "PREWARM 失败 {} elapsed={}ms：{e}",
            job.path.display(),
            elapsed
        ),
    }
}

/// 相邻页预取（1.3.0）：只把结果写进页缓存，不上屏、不碰单槽。
fn warm_page(job: &RenderJob) {
    let key = pagecache::PageKey::new(&job.path, job.page, job.max_width, job.max_height);
    if pagecache::has(&key) {
        return;
    }
    let started = Instant::now();
    match qlr_render::render_file_at_zoom(&job.path, job.max_width, job.max_height, job.page, false)
    {
        Ok(canvas) => {
            let elapsed = started.elapsed().as_millis();
            let canvas = Arc::new(canvas);
            pagecache::put(key, canvas);
            let (pages, mb) = pagecache::stats();
            log_debug!(
                SCOPE,
                "PAGECACHE 预取 page={} elapsed={}ms（缓存 {} 页/{} MB）",
                job.page + 1,
                elapsed,
                pages,
                mb / (1024 * 1024)
            );
        }
        Err(e) => log_debug!(SCOPE, "PAGECACHE 预取失败 page={}：{e}", job.page + 1),
    }
}

/// 当前这一页算完/上屏之后，顺手把前后各一页丢给预渲染线程。
///
/// 只做相邻一页（不是整本）：用户翻页基本是循序的，一页够用；
/// 整本预取既慢又占内存，还会跟"用户真正要看的那一帧"抢 CPU。
fn prefetch_neighbor_pages(key: &pagecache::PageKey) {
    let (pages, prefetch) =
        with_state(|s| (s.pages, s.config.prewarm.is_on())).unwrap_or((None, false));
    let Some(total) = pages else { return };
    if !prefetch {
        return;
    }
    let Some(slot) = PREWARM_TX.get() else {
        return;
    };
    let Ok(guard) = slot.lock() else { return };
    let Some(tx) = guard.as_ref() else { return };
    for delta in [1_i64, -1] {
        let target = key.page as i64 + delta;
        if target < 0 || target as usize >= total {
            continue;
        }
        let neighbor = pagecache::PageKey::new(&key.path, target as usize, key.width, key.height);
        if pagecache::has(&neighbor) {
            continue;
        }
        let _ = tx.send(WarmTask::Page(RenderJob {
            generation: 0,
            path: neighbor.path.clone(),
            max_width: neighbor.width,
            max_height: neighbor.height,
            page: neighbor.page,
            upscale: false,
        }));
    }
}

/// 这个路径还是当前"准备预热"的候选吗？（用户已经换选中项就作废）
fn prewarm_still_wanted(path: &Path) -> bool {
    with_state(|s| s.prewarm_candidate.as_deref() == Some(path)).unwrap_or(false)
}

/// 投一个预渲染任务。
fn request_prewarm(path: &Path, width: u32, height: u32) {
    let (max_width, max_height) = clamp_render_size(width, height);
    let Some(slot) = PREWARM_TX.get() else {
        return;
    };
    let Ok(guard) = slot.lock() else { return };
    let Some(tx) = guard.as_ref() else { return };
    let job = RenderJob {
        generation: 0,
        path: path.to_path_buf(),
        max_width,
        max_height,
        page: 0,
        upscale: false,
    };
    if tx.send(WarmTask::Prepared(job)).is_err() {
        log_warn!(SCOPE, "预渲染线程已退出，丢弃本次预热");
    }
}

/// 取出与 `path` + 目标尺寸匹配的预渲染帧（对不上就把槽清空，别占着几 MB 内存）。
fn take_prepared(path: Option<&Path>, width: u32, height: u32) -> Option<Arc<Canvas>> {
    let path = path?;
    let mut guard = prepared_slot().lock().ok()?;
    let prepared = guard.take()?;
    // 尺寸允许 ±2 px 误差：两次计算中间可能被 DPI/工作区微调过一次，差两三像素看不出来。
    let close = |a: u32, b: u32| a.abs_diff(b) <= 2;
    if prepared.path != path || !close(prepared.width, width) || !close(prepared.height, height) {
        log_debug!(
            SCOPE,
            "PREWARM 未命中（{} {}x{} vs 需要 {}x{}）",
            prepared.path.display(),
            prepared.width,
            prepared.height,
            width,
            height
        );
        return None;
    }
    Some(prepared.canvas)
}

/// 预热预算闸门：按类型封顶，超过就不预热。
///
/// 预热是"用户没要求时花 CPU"，所以宁可少预几个，也不要在 200 页扫描件上卡一下。
fn prewarm_within_budget(path: &Path, kind: format::Kind) -> bool {
    match kind {
        format::Kind::Image => match qlr_render::natural_size(path, kind) {
            Some((w, h)) => (w as u64) * (h as u64) <= PREWARM_MAX_PIXELS,
            None => false,
        },
        // SVG / 视频也按"原始像素"算预算：8000×6000 的矢量图跟同尺寸位图一样贵。
        format::Kind::Svg => match qlr_render::natural_size(path, kind) {
            Some((w, h)) => (w as u64) * (h as u64) <= PREWARM_MAX_PIXELS,
            None => false,
        },
        // 视频要过一遍 Media Foundation 解码首帧（几十毫秒级），只在文件不大时预。
        format::Kind::Video => std::fs::metadata(path)
            .map(|m| m.len() <= PREWARM_MAX_VIDEO_BYTES)
            .unwrap_or(false),
        format::Kind::Pdf => std::fs::metadata(path)
            .map(|m| m.len() <= PREWARM_MAX_PDF_BYTES)
            .unwrap_or(false),
        // HTML 跟文本同量级（读 + 排版），共用文本预算。
        format::Kind::Text | format::Kind::Markdown | format::Kind::Html => std::fs::metadata(path)
            .map(|m| m.len() <= PREWARM_MAX_TEXT_BYTES)
            .unwrap_or(false),
        // 音频：卡片内容只靠元信息，渲染本身极便宜；但"预渲染"对听声音没意义，不预。
        format::Kind::Audio => false,
        format::Kind::Unsupported => false,
    }
}

/// 把请求转交给已在运行的实例。
fn forward(host: HWND, request: &Request, started: Instant) -> ExitCode {
    match instance::send_request(host, &request.to_bytes()) {
        Ok(()) => {
            log_info!(
                SCOPE,
                "请求已转交：action={:?} path={:?}，耗时 {}ms",
                request.action,
                request.path,
                started.elapsed().as_millis()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            log_error!(SCOPE, "转交请求失败：{}", qlr_platform::to_core(e));
            ExitCode::from(3)
        }
    }
}

///
/// 每次请求进来先 `begin_request()`：SHOW/HIDE 报的延迟是"这一步花了多久"，
/// 而不是"进程活了多久"。`on_hotkey` 走的是另一条入口（真实空格），那里也要调一次，
/// 否则从托盘/热键进来的预览会报出几十秒的假延迟。
fn begin_request() {
    with_state(|s| s.request_started = Instant::now());
}

fn apply_request(request: &Request) {
    log_debug!(
        SCOPE,
        "收到请求 action={:?} path={:?}",
        request.action,
        request.path
    );
    begin_request();
    match request.action {
        Action::Toggle => toggle(request.path.as_deref().map(PathBuf::from)),
        Action::Show => show_preview(request.path.as_deref().map(PathBuf::from)),
        Action::Hide => {
            hide_preview();
        }
        Action::Quit => quit(),
        Action::Ping => log_debug!(SCOPE, "收到 Ping"),
        // 模拟热键：和真实钩子走同一个处理函数，延迟按 0 记（没有经过钩子）。
        // 键码取配置里"打开/关闭预览"的绑定，改键后冒烟测试仍能对准。
        Action::HotkeyProbe => {
            let vk = with_state(|s| {
                s.config
                    .hotkeys
                    .binding(qlr_core::keymap::Action::Toggle)
                    .vk
            })
            .unwrap_or(0x20);
            on_hotkey(vk as usize, unsafe { GetTickCount() })
        }
        Action::Settings => open_settings(),
        Action::PageNext => turn_page(1),
        Action::PagePrev => turn_page(-1),
        Action::ZoomIn => set_zoom(1.25),
        Action::ZoomOut => set_zoom(0.8),
        Action::ZoomReset => zoom_reset(),
        Action::Pan { dx, dy } => pan_by(dx, dy),
        Action::Wheel { delta } => on_wheel(delta),
        // 只给自动化用：钩子会过滤合成按键，脚本按不出真的方向键。
        Action::Scroll { delta } => {
            scroll_text_by(delta as isize);
        }
    }
}

fn toggle(path: Option<PathBuf>) {
    let (visible, last) = with_state(|s| (s.visible, s.last_path.clone())).unwrap_or((false, None));
    // Toggle 的语义（对齐 QuickLook 的空格键行为）：
    // - 带路径的调用 = "我要看这个文件"：同一个文件再按一次才收起，换了文件就直接换成新的；
    // - 不带路径 = 纯开/关。
    // 1.0.10：开了"阻止关闭"之后，空格不能再把它收起来（换成别的文件仍然允许）。
    match path {
        Some(new_path) => {
            if visible && last.as_deref() == Some(new_path.as_path()) {
                if close_blocked("空格") {
                    return;
                }
                hide_preview();
            } else {
                show_preview(Some(new_path));
            }
        }
        None => {
            // 不带路径 = 纯开关，但**只允许"收起"**。
            //
            // 用户实测反馈："打开程序的时候，只应该出现在后台，前台不要留窗口"。
            // 早期版本在裸启动时弹一个空窗口（还会把上次的内容重新翻出来），
            // 双击一下就被窗口占住前台 —— 所以没有路径时一律不新建窗口。
            // 想看说明/占位内容请用托盘菜单或 `--show`，那是显式意图。
            if visible {
                if close_blocked("空格") {
                    return;
                }
                hide_preview();
            } else {
                log_debug!(SCOPE, "裸 Toggle 且窗口未显示：忽略（保持后台）");
            }
        }
    }
}

/// "阻止关闭"开着时拒绝收起窗口，返回 `true` 表示这次收起被挡下了。
///
/// 只挡**含糊意图**（空格、Esc 这类"顺手关掉"）；顶栏的 × 和托盘退出是显式点击，
/// 照样能关——否则用户开了锁就再也关不掉，只能杀进程。
fn close_blocked(how: &str) -> bool {
    let pinned = with_state(|s| s.pinned && s.visible).unwrap_or(false);
    if pinned {
        log_info!(SCOPE, "已开启「阻止关闭」，忽略{how}");
        window::beep();
    }
    pinned
}

/// 翻页键 → 方向：PageDown/右 = +1，PageUp/左 = -1。
fn page_delta_for(vk: usize) -> Option<isize> {
    match vk as u32 {
        0x22 | 0x27 => Some(1),
        0x21 | 0x25 => Some(-1),
        _ => None,
    }
}

/// 缩放范围：0.25×–8×。再大 pdfium 会把内存吃爆，再小没意义。
const ZOOM_MIN: f32 = 0.25;
const ZOOM_MAX: f32 = 8.0;

fn scaled(value: u32, zoom: f32) -> u32 {
    ((value as f32 * zoom).round() as i64).clamp(1, 16_384) as u32
}

/// 当前该按多大像素渲染：客户区尺寸（扣掉顶部状态条）× 缩放。
fn view_size(preview: HWND) -> (u32, u32) {
    // 渲染盒必须**等于绘制/命中用的内容区**（`frame().content`）。
    //
    // 以前这里用整窗客户区减顶栏，比 `frame().content` 多出一条滚动条的宽度：
    // 位图按全宽渲染、上屏却只铺在"扣掉滚动条"的内容区里，于是
    // ①页面右边那条被滚动条压住（96 DPI 下 12 px，200% 下 24 px）；
    // ②宽高都和内容区差几像素，`StretchDIBits` 只能拉伸 → 每帧重采样。
    // 现场日志就是这条：`STRETCH 位图 1044x1477 != 内容区 1020x1479`。
    //
    // 顶栏（1.0.9 起四种格式统一）也已经在 `chrome_rects().content` 里扣过了。
    let f = frame(preview);
    let (w, h) = (
        (f.content.right - f.content.left).max(1) as u32,
        (f.content.bottom - f.content.top).max(1) as u32,
    );
    let zoom = with_state(|s| s.zoom).unwrap_or(1.0);
    (scaled(w, zoom), scaled(h, zoom))
}

/// 复位到"适应窗口"（100%）。
fn zoom_reset() {
    let current = with_state(|s| s.zoom).unwrap_or(1.0);
    set_zoom_to(1.0, current, true);
}

/// 按倍数缩放。
fn set_zoom(factor: f32) {
    let current = with_state(|s| s.zoom).unwrap_or(1.0);
    set_zoom_to((current * factor).clamp(ZOOM_MIN, ZOOM_MAX), current, true);
}

/// 改缩放并（可选）重渲染当前文件。
///
/// 1.0.2 起：**窗口跟着缩放走**（有自然尺寸的内容）。以前只在固定窗口里改渲染分辨率，
/// 放大后画面被裁掉、缩小后四周一圈黑边，用户反馈"窗口应该跟着缩放"。
/// 1.0.5 两处改动：
/// - 窗口尺寸变了以后**保持窗口中心不动**（`resize_around_center`），不再按工作区重新居中
///   ——用户把窗口拖到别处再缩放，画面"跳回屏幕中间"是很难受的；
/// - `render = false` 时**不重渲染**，只把旧位图拉伸铺上新窗口（`zoom_preview`）。
///   右键拖动缩放时每一帧重渲染一页 PDF 是卡顿的根源，松手（`on_zoom_drag_end`）才出高清图。
/// - 文本/Markdown 没有自然尺寸（滚动是按行走的），保持原行为：只改渲染分辨率。
fn set_zoom_to(new: f32, old: f32, render: bool) {
    let Some((preview, path)) = with_state(|s| (s.preview, s.last_path.clone())) else {
        return;
    };
    let (new, base) = with_state(|s| {
        s.zoom = new;
        // 缩放后原来的平移量不再有意义（渲染尺寸变了），直接回中。
        s.pan = (0, 0);
        (s.zoom, s.base)
    })
    .unwrap_or((new, None));
    if (old - new).abs() < f32::EPSILON {
        log_debug!(SCOPE, "缩放已在边界 {new:.2}×，忽略");
        return;
    }
    // 有开窗基准尺寸：窗口跟着缩放走（1.0.2），位图按"基准 × zoom"渲染。
    // 窗口塞不进工作区时会被夹住，此时位图比窗口大 → 用户可以平移看局部。
    let (width, height) = match base {
        Some((bw, bh)) => {
            let cw = ((bw as f32) * new).round().max(1.0) as u32;
            let ch = ((bh as f32) * new).round().max(1.0) as u32;
            // base 记的是**内容区**尺寸：PDF 的窗口还要多出一条页码栏的高度。
            let reserve = window::chrome_height();
            window::resize_around_center(preview.raw(), cw as i32, (ch as i32) + reserve);
            (cw, ch)
        }
        None => view_size(preview.raw()),
    };
    // 从"改完窗口"到"新位图到位"这段时间，画面只能是旧位图 —— 标记成预览态，
    // 让 `repaint` 用拉伸方式铺满窗口，而不是按 1:1 裁剪（那会露出半屏底色）。
    with_state(|s| s.zoom_preview = true);
    if render {
        if let Some(path) = path {
            let page = with_state(|s| s.page).unwrap_or(0);
            request_render(&path, width, height, page);
        }
    }
    let mut lines = get_lines();
    for line in lines.iter_mut() {
        if line.starts_with("缩放：") {
            *line = format!("缩放：{new:.0}%");
            break;
        }
    }
    set_lines(lines);
    // 缩放会改窗口尺寸，投影浮层必须跟着重算——不跟就会留下一个比窗口大的灰框
    // （用户在缩小图片时看到"外圈多了一层阴影框"，就是这个）。
    sync_shadow();
    // 位置记忆（1.0.13）：缩放按窗口中心改尺寸，左上角会跟着变，记下新位置。
    remember_position();
    // 尺寸记忆（1.0.14）：缩放也是用户对尺寸的决定（放大看细节后会保持这个大小）。
    remember_size();
    window::invalidate(preview.raw());
    // 用户可见的缩放（滚轮/快捷键）打 info，拖动预览帧打 debug：
    // 一次拖动几十帧，全打 info 会把日志刷爆；但冒烟脚本要能看到"滚轮真的缩放了"。
    if render {
        log_info!(SCOPE, "ZOOM {old:.2} -> {new:.2} render={width}x{height}");
    } else {
        log_debug!(
            SCOPE,
            "ZOOM {old:.2} -> {new:.2} render={width}x{height}（预览，松手再渲染）"
        );
    }
}

/// 文本滚动：当前内容不是文本时返回 false（调用方据此退回缩放/翻页/平移）。
fn scroll_text_by(lines: isize) -> bool {
    let Some(preview) = with_state(|s| s.preview) else {
        return false;
    };
    if !matches!(get_content().as_deref(), Some(Canvas::Text(_))) {
        return false;
    }
    let rows = visible_rows(preview.raw());
    let total = text_total_lines();
    // 内容比窗口还短：什么都不用滚，但也算"已处理"，别再退回去缩放
    if total <= rows {
        return true;
    }
    let current = with_state(|s| s.scroll).unwrap_or(0);
    let next = scroll_clamp(current, lines, total, rows);
    if next != current {
        with_state(|s| s.scroll = next);
        log_debug!(
            SCOPE,
            "SCROLL {current} -> {next}（共 {total} 行，可见 {rows} 行）"
        );
        window::invalidate(preview.raw());
    }
    true
}

/// 滚动位置钳制（纯函数，便于单测）。
fn scroll_clamp(current: usize, delta: isize, total: usize, rows: usize) -> usize {
    let max = total.saturating_sub(rows);
    let raw = current as i64 + delta as i64;
    raw.clamp(0, max as i64) as usize
}

/// 状态行占一行，正文能用的行数要减 1。
/// 一屏能放几行正文（不含顶部留白与状态行）。
///
/// 1.0.8 修：以前按 `h / window::line_height() - 1` 算，**没扣顶部留白**，748 高的窗口算出
/// 30 行正文，加上状态行就是 `16 + 31*24 = 760 > 748` —— 最后一行被窗口裁掉半截，
/// 用户看到的就是"md 和文本最后一行显示不全"。
fn visible_rows(hwnd: HWND) -> usize {
    rows_in(window::client_rect(hwnd))
}

/// 给定一块内容区，能放几行正文（不含顶部留白与状态行）。
///
/// 1.0.9 起顶栏、目录栏、滚动条都会吃掉内容区，所以行数必须按**内容区**算，
/// 否则文本最后一行会跑到顶栏下面去（或反过来多留一条空白）。
fn rows_in(content: RECT) -> usize {
    let usable = (content.bottom - content.top) - window::text_padding_top();
    let rows = (usable / window::line_height()).max(1) as usize;
    // 再让出一行给状态行（状态行画在正文之上，两行一起必须塞得下）。
    rows.saturating_sub(1).max(1)
}

/// 翻页：文本按"上次绘制实际吃掉多少原始行"滚动，其它内容（PDF/图片）换页或换张。
fn turn_page(delta: isize) {
    // PPT（1.7.4）：翻页由我们接管（数页、到头停住并提示）。放在最前面，
    // 因为这类内容既没有文本行也没有 `pages`，落到下面只会什么都不做。
    if step_slide(delta) {
        return;
    }
    let step = with_state(|s| s.preview)
        .map(|p| page_lines(p.raw()) as isize)
        .unwrap_or(10)
        .max(1);
    if !scroll_text_by(delta * step) {
        step_page(delta);
    }
}

/// 翻页步长：优先用上次绘制实际吃掉的**原始行数**（折行后一屏原始行更少），
/// 还没画过就退回屏幕行数。
fn page_lines(hwnd: HWND) -> usize {
    let consumed = with_state(|s| s.wrapped_consumed).unwrap_or(0);
    if consumed > 0 {
        consumed
    } else {
        visible_rows(hwnd)
    }
}

fn text_total_lines() -> usize {
    match get_content().as_deref() {
        Some(Canvas::Text(t)) => t.lines.len(),
        _ => 0,
    }
}

/// 滚轮总入口：文本先滚，其余（图片/PDF）才滑动/缩放。
///
/// **钩子投递的滚轮和 CLI 的 `--wheel` 必须走同一条路**（1.0.1 修）：
/// 之前 `WM_QLR_WHEEL` 直接调 `wheel_zoom`，导致鼠标滚轮在文本上不是滚动而是
/// "缩放重新渲染"——用户看到的是屏幕内容突然变形/错位（以为是翻页逻辑坏了）。
///
/// 1.0.3 起的约定（按用户要求）：
/// - **文本**：一格 3 行（折行后的显示行，滚到底自然停）。向上滚 = 看上面的内容。
/// - **PDF**：页内上下滑动，一格 1/4 屏；滑到页顶/页底还继续滚 = 翻到上一页/下一页。
/// - **图片**：放得下就保持老习惯（滚轮 = 缩放）；放大到超出窗口后可上下滑动。
/// - **缩放**：图片/PDF 都改用**右键上下拖**（`on_zoom_drag_*`），滚轮不再改缩放。
fn on_wheel(delta: i32) {
    // 档数走**带零头累积**的入口（1.8.3）。这里有一个真实事故：
    // 高分辨率滚轮/触控板每次只发 30–60，`delta / 120` 永远是 0，
    // 于是"滚轮完全没反应"——图片不缩放、正文不滚、目录不滚，日志里也没有任何记录。
    let notches = take_wheel_notches(delta);
    // 这个函数有 8 条分支，出问题（"滚轮没反应"）时必须能一眼看出停在哪一支。
    log_debug!(
        SCOPE,
        "WHEEL 进入 delta={delta} 本次档数={notches} 累计零头={}",
        with_state(|s| s.wheel_acc).unwrap_or(0)
    );
    // 音频/视频（1.7.9）：滚轮 = 快进/快退（一格 5 秒）。放在最前面——它们没有可滚内容，
    // 落到下面会去缩放/翻页，用户听到的是"声音还在放、画面莫名其妙变了"。
    if playback_active() {
        if notches != 0 {
            // 往上滚 = 往回（与"翻到前面"同一方向约定）。
            playback_seek(-(notches.signum() as i64) * AUDIO_SEEK_STEP_MS);
        }
        return;
    }
    // PPT（1.7.4）：翻页由我们接管，滚轮 = 翻页（和 PDF 一致：往下滚 = 下一页）。
    // 必须放在 `forward_wheel_to_shell` 前面：那个函数会把滚轮转给处理器的子窗口，
    // 而它的翻页计数只有我们自己在维护，转出去就失准了。
    if with_state(|s| s.slide).flatten().is_some() {
        if notches != 0 {
            step_slide(if notches > 0 { -1 } else { 1 });
        }
        return;
    }
    // 零头还没攒够一整档：什么都不做，等下一发继续攒（**不要**在这里调 wheel_zoom，
    // 否则高分辨率滚轮会拿 delta=30 去缩放，一格变成十几次缩放）。
    if notches == 0 {
        return;
    }
    // 系统预览处理器接管时，滚轮归它（它自己有滚动条/翻页），我们不抢。
    if forward_wheel_to_shell(delta as i16) {
        return;
    }
    // Shift + 滚轮：调放大镜倍数（按住 Shift 时用户在看细节，不该拿去滚动/缩放内容）。
    if lens_wheel(notches) {
        return;
    }
    // 压缩包列表（1.7.3）：按行滚。放在文本/位图前面——列表视图的内容不在 `CONTENT` 里，
    // 继续往下走只会误伤（比如被当成"图片放得下"去缩放）。
    if archive_scroll_by(wheel_text_rows(notches)) {
        return;
    }
    // 目录栏（1.8.3）：鼠标悬停在目录上时，滚轮滚**目录**而不是正文。
    // 目录比窗口高的时候以前根本翻不到后面几条（没有滚动条，也没有滚轮），
    // 用户的原话："目录页可以不要滚动栏，支持鼠标滚动即可"。
    if with_state(|s| s.hover_toc).unwrap_or(false) && scroll_toc_by(notches) {
        return;
    }
    // 文本：一格 3 行。滚得动就不缩放。
    if scroll_text_by(wheel_text_rows(notches)) {
        return;
    }
    let (moved, at_edge) = scroll_page_by(notches);
    if moved && !at_edge {
        return;
    }
    // PDF：一页看完继续滚 = 翻页（滚轮向下 = 下一页，和"往下读"一致）。
    if with_state(|s| s.pages).flatten().is_some() {
        step_page(if notches > 0 { -1 } else { 1 });
        return;
    }
    // 图片：本来就没得滚（整张放得下）→ 保留"滚轮缩放"的老习惯。
    if !moved {
        wheel_zoom(notches);
    }
}

/// 滚动目录栏（1.8.3）。返回是否处理了这次滚轮。
///
/// 目录一屏放不下时，滚轮在目录上就滚目录；放得下就返回 false，让给正文
/// （否则"目录只有 5 条"的文件滚轮会失效，看起来像坏了）。
fn scroll_toc_by(notches: isize) -> bool {
    let Some(preview) = with_state(|s| s.preview) else {
        return false;
    };
    let layout = frame(preview.raw());
    let Some(toc) = layout.toc else {
        return false;
    };
    let visible = window::toc_visible_rows(layout.below_chrome).max(1);
    let total = with_state(|s| s.toc.len()).unwrap_or(0);
    let max = total.saturating_sub(visible);
    if max == 0 {
        return false;
    }
    let cur = with_state(|s| s.toc_scroll).unwrap_or(0);
    let step = wheel_text_rows(notches); // 一格 3 条，与正文手感一致
    let next = (cur as i64 + step as i64).clamp(0, max as i64) as usize;
    if next != cur {
        with_state(|s| s.toc_scroll = next);
        window::invalidate(preview.raw());
        log_debug!(
            SCOPE,
            "TOCSCROLL {cur} -> {next}（共 {total} 条，可见 {visible} 条）"
        );
    }
    let _ = toc;
    true
}

/// 滚轮档数 → 文本滚动行数（纯函数，便于单测）。
///
/// 1.0.4 修方向：**往上滚必须看上面的内容**。1.0.3 写成了 `notches * 3`，
/// 结果向上滚反而往下翻（和拖拽、PageUp 全反着），用户一眼就看出来了。
fn wheel_text_rows(notches: isize) -> isize {
    -notches * 3
}

/// 页内上下滑动（位图比窗口高才有意义）。
///
/// 返回 `(这次是否真的动了, 是否已经顶到边界)`：PDF 用第二个值判断"该翻页了"。
fn scroll_page_by(notches: isize) -> (bool, bool) {
    let Some((preview, content_h)) = with_state(|s| s.preview).zip(bitmap_height()) else {
        return (false, false);
    };
    let rc = window::client_rect(preview.raw());
    let ch = (rc.bottom - rc.top).max(1);
    let max = (content_h - ch).max(0);
    if max == 0 {
        return (false, false);
    }
    // 一格 = 1/4 屏；滚轮向下（notches < 0）→ 看更下面的内容（pan.1 增大）。
    let step = (ch / 4).max(48) * -(notches as i32);
    let (cur, next) = with_state(|s| {
        let cur = s.pan.1;
        let next = (cur + step).clamp(0, max);
        s.pan.1 = next;
        (cur, next)
    })
    .unwrap_or((0, 0));
    let at_edge = if notches > 0 { next == 0 } else { next == max };
    if next != cur {
        window::invalidate(preview.raw());
        log_debug!(
            SCOPE,
            "VSCROLL pan.y {cur} -> {next}（内容 {content_h}px / 窗口 {ch}px）"
        );
    }
    (next != cur, at_edge)
}

/// 当前位图的高度（像素）；文本/占位内容返回 `None`。
fn bitmap_height() -> Option<i32> {
    match get_content().as_deref() {
        Some(Canvas::Bitmap(b)) => Some(b.height as i32),
        _ => None,
    }
}

/// 滚轮缩放：一格（`WHEEL_DELTA`=120）放大 1.25 倍，负向缩小。
///
/// 入参是**档数**（已经过 `take_wheel_notches` 的零头累积），不是原始 delta：
/// 高分辨率滚轮单发只有 30，再在这里除一次 120 就又变回 0 —— 1.8.3 前就是这个问题。
fn wheel_zoom(notches: isize) {
    if notches == 0 {
        return;
    }
    let factor = if notches > 0 {
        1.25_f32
    } else {
        1.0 / 1.25_f32
    };
    for _ in 0..notches.abs() {
        set_zoom(factor);
    }
    log_debug!(SCOPE, "ZOOM 档数={notches}");
}

/// 滚轮增量 → 档数（一格 `WHEEL_DELTA`=120，一次最多 4 档）。
fn wheel_notches(delta: i32) -> i32 {
    (delta / 120).clamp(-4, 4)
}

/// 把滚轮增量攒成"档"（1.8.3）：**零头不再扔掉**。
///
/// 一格滚轮 = 120，但高分辨率滚轮 / 精密触控板每次只发 30–60。
/// 1.8.2 直接 `delta / 120`，零头被丢 → 这类设备上每一发都算出 0 档 →
/// 图片不缩放、正文不滚、目录不滚，**而且日志里一条都不留**（看起来像"功能消失了"）。
///
/// 纯函数便于单测：`acc` 是跨事件累积的零头，本次拿走整档后把余数写回。
fn wheel_take(acc: &mut i32, delta: i32) -> i32 {
    *acc = acc.saturating_add(delta);
    let notches = (*acc / 120).clamp(-4, 4);
    if notches != 0 {
        if notches.abs() == 4 {
            // 触控板一次甩出几百，别把余数留成巨款（否则停止后还会继续滚）。
            *acc = 0;
        } else {
            *acc -= notches * 120;
        }
    }
    // 长时间不滚时零头可能慢慢攒；夹一下防止异常设备把它撑爆。
    *acc = (*acc).clamp(-119, 119);
    notches
}

/// 取本次滚轮对应的档数（带零头累积，见 [`wheel_take`]）。
fn take_wheel_notches(delta: i32) -> isize {
    with_state(|s| wheel_take(&mut s.wheel_acc, delta)).unwrap_or_else(|| wheel_notches(delta))
        as isize
}

/// 客户区坐标（`lparam` 的低/高 16 位是**有符号**的，别直接用 `& 0xFFFF`）。
fn client_xy(lparam: LPARAM) -> (i32, i32) {
    let x = (lparam.0 as i32) as i16 as i32;
    let y = ((lparam.0 >> 16) as i32) as i16 as i32;
    (x, y)
}

/// 按下：记录起点，开始拖拽（内容比窗口大才有意义，但捕获总是设上，
/// 否则鼠标移出窗口后 `WM_MOUSEMOVE` 就断了）。
fn on_drag_start(hwnd: HWND, lparam: LPARAM) {
    trace_drag_start();
    // 拖动期间先收起投影：投影是独立的分层窗口，只按 8 ms 节流跟随，
    // 窗口以 144 Hz 走、投影以 ~60 Hz 走，肉眼就是"窗口在抖、边缘在拖影"（1.4.8）。
    if let Some(shadow) = with_state(|s| s.shadow).flatten() {
        window::hide_shadow(shadow.raw());
    }
    let (x, y) = drag_origin(lparam);
    with_state(|s| {
        s.drag_from = Some((x, y));
        s.dragged = false;
        s.drag_residual = 0;
    });
    window::set_capture(hwnd);
}

/// 拖动用的坐标：**屏幕坐标优先**。
///
/// 1.0.4 踩的坑：拖动时我们一边挪窗口、一边用 client 坐标算位移，两者互相抵消——
/// 鼠标走 90 px，窗口只走 27 px（约 1/3），手感是"拖不动"。改用 `GetCursorPos`
/// 的屏幕坐标后，位移与鼠标 1:1。
fn drag_origin(lparam: LPARAM) -> (i32, i32) {
    window::cursor_pos().unwrap_or_else(|| client_xy(lparam))
}

/// 移动：累积位移超过 3 px 才算"拖动"，之后每帧把增量交给平移 / 滚动。
fn on_drag_move(lparam: LPARAM) {
    let (x, y) = drag_origin(lparam);
    let mut step: Option<(i32, i32)> = None;
    with_state(|s| {
        if let Some((fx, fy)) = s.drag_from {
            let (dx, dy) = (x - fx, y - fy);
            if !s.dragged && (dx.abs() + dy.abs()) >= 3 {
                s.dragged = true;
            }
            if s.dragged {
                s.drag_from = Some((x, y));
                step = Some((dx, dy));
            }
        }
    });
    if let Some((dx, dy)) = step {
        // 1.0.5：左键拖动在**所有内容**上都是"移动窗口"（用户要求文本也能整窗拖动）。
        //
        // 1.0.3 语义：挪的是位图的绘制偏移，实测会频闪——每帧重画一整张位图，
        // 4K 一帧十几毫秒，桌面合成器在半成品上取帧就是闪烁。挪窗口由合成器搬
        // 现成的表面，零重绘，手感也更像"拖一张纸"。
        // 文本的滚动改由滚轮/右键拖动/PageUp·PageDown 承担（见 `on_zoom_drag_*`）。
        move_window_by(dx, dy);
    }
}

/// 拖动窗口：位移直接跟随鼠标（不重绘任何内容）。
fn move_window_by(dx: i32, dy: i32) {
    if dx == 0 && dy == 0 {
        return;
    }
    let Some(preview) = with_state(|s| s.preview) else {
        return;
    };
    // 拖动是"每次鼠标移动都调一次"的热路径：任何一步慢都会直接表现为"窗口追不上鼠标"。
    // 这里量每一步的耗时，超过 5 ms 就记一条 warn（平时不打，免得日志爆炸）。
    let started = Instant::now();
    window::move_by(preview.raw(), dx, dy);
    // 投影是另一个窗口，挪了主窗口就得跟着挪（不跟会拖下一条"鬼影"）；
    // 拖动热路径上节流到 8 ms 一次，松手时 `on_drag_end` 会强制对齐。
    sync_shadow_with(true);
    // 位置记忆（1.0.13）：拖完立刻记，收起时统一落盘。
    remember_position();
    let ms = started.elapsed().as_millis();
    trace_drag_moved(dx, dy);
    trace_drag_cost(ms);
    trace_drag_msg();
    if ms >= 5 {
        log_warn!(
            SCOPE,
            "SLOWMOVE ({dx}, {dy}) {ms}ms —— 拖动跟不上鼠标就查这里"
        );
    }
    log_debug!(SCOPE, "MOVE ({dx}, {dy})");
}

/// 鼠标落在窗口**客户区**的哪几条边（`margin` 像素以内算命中）。纯函数，便于单测。
///
/// 只算客户区：预览窗无边框、`WM_NCCALCSIZE` 返回 0 把整块窗口都当客户区，
/// 所以两者等价，不必再去问 `GetWindowRect`（拖动过程中它每帧都在变）。
///
/// `margin` 由调用方按 DPI 算（1.7.4）：PMv2 下坐标是物理像素，固定 6 px 在
/// 125%/150% 屏上只有 4~5 DIP，"贴不准就拖不动"。
fn edges_at(rc: RECT, x: i32, y: i32, margin: i32) -> ResizeEdges {
    // 1.8.2：图片/视频不允许拖边改大小（`resize_margin` 会返回 0），这里直接短路，
    // 免得 `x <= left && x >= left` 在正好压线时还能抓到一个 0 宽的边。
    if margin <= 0 {
        return ResizeEdges::default();
    }
    let inside = x >= rc.left - margin
        && x <= rc.right + margin
        && y >= rc.top - margin
        && y <= rc.bottom + margin;
    if !inside {
        return ResizeEdges::default();
    }
    // 顶栏也算窗口的一部分：用户要求"顶栏也能拖动改大小"以外的位置本来就该能拖。
    ResizeEdges {
        left: x <= rc.left + margin,
        right: x >= rc.right - margin,
        top: y <= rc.top + margin,
        bottom: y >= rc.bottom - margin,
    }
}

/// 这类预览**不允许拖边改大小**（1.8.2，用户要求）：
/// 「把图片和视频的窗口伸缩功能去掉，滚轮对窗口进行缩放就足够了」——
/// 图片/视频靠滚轮缩放看细节，窗口本身只在打开时按素材比例定一次大小。
fn resizable_kind(kind: Option<format::Kind>) -> bool {
    !matches!(kind, Some(format::Kind::Image) | Some(format::Kind::Video))
}

/// 这台机器/这个窗口上的边缘热区宽度（DIP 8 → 物理像素，最少 6）。
///
/// Excel/Word 预览（1.8.1）：处理器收到鼠标事件后，它的子窗口会盖住内容区，只留下
/// 我们 `SHELL_RING_DIP` 那一圈。这时热区不能还是 8 DIP——超出的部分落到子窗口上，
/// 用户就"抓不到边"。所以把热区收进那一圈里（同时保证 ≥ 正常最小值的 1/2）。
///
/// 图片/视频（1.8.2）：返回 0 = 四条边都不响应，窗口大小固定（缩放交给滚轮）。
fn resize_margin(hwnd: HWND) -> i32 {
    if !resizable_kind(current_kind()) {
        return 0;
    }
    let ring = dpi::scaled(hwnd, SHELL_RING_DIP);
    let live_shell =
        with_state(|s| s.shell_preview.is_some() && !s.shell_owns_input).unwrap_or(false);
    if live_shell {
        return ring.max(dpi::px(RESIZE_MARGIN_DIP) / 2);
    }
    dpi::scaled(hwnd, 8).max(dpi::px(RESIZE_MARGIN_DIP))
}

/// 按下：命中边缘就进入"改大小"模式（否则交给移动窗口）。
fn begin_resize(hwnd: HWND, edges: ResizeEdges, lparam: LPARAM) {
    window::suppress_shadow();
    let (x, y) = drag_origin(lparam);
    let rc = window::window_rect(hwnd);
    with_state(|s| {
        s.resize_edges = Some(edges);
        s.resize_from = Some((x, y, rc));
        // 改大小期间不移动窗口，别让 `on_drag_move` 也跟着跑。
        s.drag_from = None;
        s.dragged = false;
    });
    window::set_capture(hwnd);
    window::set_cursor(edges.cursor());
    log_debug!(SCOPE, "RESIZE 开始（{}）", edges.describe());
}

/// 图片/视频预览改大小（1.8.1）：把"想改到的矩形"夹成**素材的比例**。
///
/// 用户口径：「图片、视频类都不需要拉伸缩放窗口，设计和图片一样的」——即窗口本身按素材
/// 比例长，内容永远铺满，既不拉伸也不留白。`bar` 是视频底部进度条的高度（图片传 0）。
///
/// 哪一维在拖就由哪一维驱动（只拖上下 → 高度驱动；其余 → 宽度驱动），
/// 对面那条边（或中线）保持不动，手感才自然。
///
/// **1.8.2 起这条路径在 UI 上已经走不到了**：图片/视频的 `resize_margin` 返回 0，
/// 拖边不再进入改大小（用户：「把图片和视频的窗口伸缩功能去掉，滚轮缩放就足够了」）。
/// 保留为兜底（别的路径真的改大小时也不会变形），单测照旧覆盖。
fn clamp_media_resize(rc: RECT, edges: ResizeEdges, aspect: f64, bar: i32) -> RECT {
    if !(aspect.is_finite() && aspect > 0.0) {
        return rc;
    }
    let extra = window::chrome_height() + bar;
    let mut out = rc;
    let vertical_only = (edges.top || edges.bottom) && !(edges.left || edges.right);
    let (content_w, content_h) = if vertical_only {
        let ch = ((out.bottom - out.top) - extra).max(min_window_h());
        let cw = (ch as f64 * aspect).round().max(min_window_w() as f64) as i32;
        (cw, ch)
    } else {
        let cw = (out.right - out.left).max(min_window_w());
        let ch = (cw as f64 / aspect).round().max(min_window_h() as f64) as i32;
        (cw, ch)
    };
    // 水平：拖左边 → 右边不动；拖右边 → 左边不动（两种都拖 = 右边不动）；
    // 只拖上下 → 保持水平居中。
    if !(edges.left || edges.right) {
        let center = (out.left + out.right) / 2;
        out.left = center - content_w / 2;
    } else if edges.left {
        out.left = out.right - content_w;
    }
    out.right = out.left + content_w;
    // 垂直：同理（上下两条边的坐标含顶栏与进度条，所以按"内容高 + 附加高"算总高）。
    let total_h = content_h + extra;
    if !(edges.top || edges.bottom) {
        let center = (out.top + out.bottom) / 2;
        out.top = center - total_h / 2;
    } else if edges.top {
        out.top = out.bottom - total_h;
    }
    out.bottom = out.top + total_h;
    out
}

/// 拖动：按"按下时的原始矩形 + 累计位移"重算，而不是每帧累加增量。
///
/// 为什么：一旦某一维被最小尺寸夹住，增量就丢了；之后往回拖，窗口跟不上鼠标，
/// 手感是"卡住一截"。用原始矩形算，怎么夹都不会漂。
fn on_resize_move(lparam: LPARAM) {
    let (x, y) = drag_origin(lparam);
    let (Some((x0, y0, original)), Some(edges)) = (
        with_state(|s| s.resize_from).flatten(),
        with_state(|s| s.resize_edges).flatten(),
    ) else {
        return;
    };
    let (dx, dy) = (x - x0, y - y0);
    let mut rc = original;
    if edges.left {
        rc.left = (original.left + dx).min(original.right - min_window_w());
    }
    if edges.right {
        rc.right = (original.right + dx).max(original.left + min_window_w());
    }
    if edges.top {
        rc.top = (original.top + dy).min(original.bottom - min_window_h());
    }
    if edges.bottom {
        rc.bottom = (original.bottom + dy).max(original.top + min_window_h());
    }
    // 图片/视频（1.8.1）：窗口比例锁死素材比例——无论拖哪条边，先夹回等比再落地。
    // （1.8.2 起拖边这条路对图片/视频已关闭，这里保留兜底：万一别处真的在改大小，
    //  也不会把素材拉变形。）
    if let Some(aspect) = with_state(|s| s.media_aspect).flatten() {
        let bar = current_kind().map(media_bar).unwrap_or(0);
        rc = clamp_media_resize(rc, edges, aspect, bar);
    }
    window::resize_to(
        with_state(|s| s.preview)
            .map(|h| h.raw())
            .unwrap_or_default(),
        rc.left,
        rc.top,
        rc.right - rc.left,
        rc.bottom - rc.top,
    );
    sync_shadow();
    log_debug!(
        SCOPE,
        "RESIZE {}x{}",
        rc.right - rc.left,
        rc.bottom - rc.top
    );
}

/// 窗口客户区尺寸 → 内容区尺寸（扣掉顶栏，下限 1）。
///
/// 全局约定：`request_render` 与 `s.base` 都用**内容区**尺寸（见 `view_size()`、
/// `show_preview` 里 `height - reserve`、`set_zoom_to` 里 `+ reserve`）。
/// 拖边结束时以前把整窗高度当内容高度传下去，于是位图比内容区高 30px →
/// 上屏走拉伸（图片/PDF 拖完发虚），`base` 也高 30px → 之后每缩放一次高度再漂 30px。
fn content_size_of(client: RECT) -> (u32, u32) {
    (
        (client.right - client.left).max(1) as u32,
        ((client.bottom - client.top) - window::chrome_height()).max(1) as u32,
    )
}

/// 这一类内容在内容区里会不会占掉一条竖向滚动条。
///
/// 渲染盒、绘制布局（`frame()`）、预热三条路必须给出同一个答案，否则位图和内容区
/// 宽度差出一条滚动条：右边那一截被滚动条压住，上屏还得拉伸（每帧重采样）。
/// 现场日志就是这么来的：`STRETCH 位图 1044x1477 != 内容区 1020x1479`。
///
/// 只有 PDF 是"位图 + 滚动条"同时存在的：图片/SVG/视频没有条，文本没有位图。
/// 单页 PDF 放得下，不画条（`ScrollInfo::new` 的规则），这也是唯一需要看页数的地方。
fn render_reserves_scrollbar(kind: format::Kind, pages: Option<usize>) -> bool {
    kind == format::Kind::Pdf && pages.is_none_or(|n| n > 1)
}

/// 内容区尺寸 → 渲染盒（已扣掉滚动条、已按缩放放大）。
///
/// 跟 `view_size()` 同一个口径：`view_size` 是"窗口已经有了，直接量绘制用的内容区"，
/// 这里是"窗口还没开，按算出来的尺寸推"。两边算出来的必须是同一个数，
/// 否则页缓存 key 对不上、预热也永远打不中（1.3.0 的坑）。
fn render_box(
    kind: format::Kind,
    pages: Option<usize>,
    width: i32,
    height: i32,
    zoom: f32,
) -> (u32, u32) {
    let width = if render_reserves_scrollbar(kind, pages) {
        width - window::scrollbar_w()
    } else {
        width
    };
    (
        scaled(width.max(1) as u32, zoom),
        scaled(height.max(1) as u32, zoom),
    )
}

/// 抬起：结束改大小，并按新尺寸重画（位图内容需要重新光栅化到新尺寸）。
fn finish_resize(edges: ResizeEdges) {
    window::allow_shadow();
    window::release_capture();
    let Some((preview, path, page)) = with_state(|s| (s.preview, s.last_path.clone(), s.page))
    else {
        return;
    };
    let size = window::client_rect(preview.raw());
    let (width, height) = content_size_of(size);
    log_info!(
        SCOPE,
        "RESIZE 结束 {}x{}（{}）",
        width,
        height,
        edges.describe()
    );
    // 窗口尺寸变了，原来的"zoom 基准"就不成立了：把新尺寸当成新的 1.0×。
    with_state(|s| {
        s.base = Some((width, height));
        s.zoom = 1.0;
        s.pan = (0, 0);
        s.zoom_preview = false;
    });
    if let Some(path) = path {
        request_render(&path, width, height, page);
    }
    sync_shadow();
    // 位置记忆（1.0.13）：拖左边/上边会同时移动位置，一并记下。
    remember_position();
    // 尺寸记忆（1.0.14）：拖边改出来的大小下次照用。
    remember_size();
    // 拖边过程中 `WM_SIZE` 故意不同步内容（每像素重开一次太慢），松手补一次；
    // 同样只能投递（`WM_LBUTTONUP` 也在输入同步呼叫里）。
    window::post(preview.raw(), ipc::WM_QLR_SHELLFIT);
    window::invalidate(preview.raw());
}

/// 跟随选中项 + 投机预渲染（1.0.10 / 1.1.0），共用一只 400 ms 计时器。
///
/// - 窗口**开着**：跟着资源管理器的选中项换内容（用户原话："预览文件随鼠标选中的文件而变化"）；
/// - 窗口**收着**：把当前选中的那个文件先渲染好（见 `maybe_prewarm`），用户按空格时直接上屏。
///
/// 两条路都先过 `foreground_is_shell()` 这道便宜的门：`IShellWindows` 那套调用要几毫秒，
/// 用户切到浏览器/游戏时继续问会白烧 CPU。
/// 后台轮询得到的"资源管理器当前选中的文件"。
///
/// 为什么放到线程里：`selection::current_selection()` 要枚举 `IShellWindows` 再 `FindWindowSW` 重试，
/// 在用户机器上实测有一次 **1538 ms**（日志 `SLOWSELECT`）。它跑在 UI 线程的 400 ms 计时器里，
/// 那就是"拖动时窗口定住、过一会儿才追上来"的真正原因。
static FOLLOW_SLOT: OnceLock<Mutex<Option<(PathBuf, Instant)>>> = OnceLock::new();
static FOLLOW_STARTED: OnceLock<()> = OnceLock::new();

fn follow_slot() -> &'static Mutex<Option<(PathBuf, Instant)>> {
    FOLLOW_SLOT.get_or_init(|| Mutex::new(None))
}

/// 跟随线程的轮询间隔（1.7.6）。
///
/// 以前是"先睡 300 ms 再查"，再加上 UI 侧 400 ms 的计时器取结果：用户点一下文件到画面换掉
/// 最坏要等 **700 ms**（用户反馈"延迟有点大"）。现在线程一发现变化就 `PostMessage`
/// （`WM_QLR_FOLLOW`），UI 立刻动手；轮询间隔只决定"发现得有多快"。
const FOLLOW_AWAKE_MS: u64 = 90;
/// 窗口收着时的轮询间隔：这时只喂投机预热，没必要跟手。
const FOLLOW_ASLEEP_MS: u64 = 250;

/// 起一个后台线程，每 [`FOLLOW_AWAKE_MS`] 摸一次选中项（COM 初始化在那个线程里做）。
///
/// 发现"UI 显示的不是这个文件"就先把路径放进 [`follow_slot`]，再 `PostMessage(WM_QLR_FOLLOW)`
/// 叫醒 UI —— 这样延迟只剩"一次轮询 + 一次查询"，不用再等 400 ms 计时器。
fn ensure_follow_worker() {
    FOLLOW_STARTED.get_or_init(|| {
        std::thread::Builder::new()
            .name("qlrust-follow".into())
            .spawn(|| {
                let _ = qlr_platform::selection::init_com();
                // 1.8.3：这个线程也会算窗口尺寸（投机预热），而 `dpi::px()` 读的是
                // **线程本地** DPI。不 seed 的话它按 96 算，与 UI 线程算出的 w/h 不一致，
                // 而 `pagecache` 的 key 含 w/h → 预热永远打不中，白烧 CPU 还看不出效果。
                dpi::seed_thread(dpi::system_dpi());
                // 已经通知过、但 UI 没切过去（比如"不支持"被跳过）：别每 90 ms 抖一次消息。
                let mut notified: Option<PathBuf> = None;
                loop {
                    let (visible, has_path) = with_state(|s| (s.visible, s.last_path.is_some()))
                        .unwrap_or((false, false));
                    let awake = visible && has_path;
                    let interval = if awake {
                        FOLLOW_AWAKE_MS
                    } else {
                        FOLLOW_ASLEEP_MS
                    };
                    let busy = with_state(|s| s.drag_from.is_some() || s.resize_edges.is_some())
                        .unwrap_or(false);
                    if !awake || busy || !qlr_platform::hotkey::foreground_is_shell() {
                        std::thread::sleep(std::time::Duration::from_millis(interval));
                        continue;
                    }
                    let started = Instant::now();
                    let picked = qlr_platform::selection::current_selection()
                        .first()
                        .map(|p| p.to_path_buf());
                    let cost = started.elapsed();
                    if cost.as_millis() >= 300 {
                        log_warn!(
                            SCOPE,
                            "SLOWSELECT {}ms（后台线程，不影响界面）",
                            cost.as_millis()
                        );
                    }
                    if let Some(next) = picked {
                        let shown = with_state(|s| s.last_path.clone()).flatten();
                        let fresh = shown.as_deref() != Some(next.as_path())
                            && notified.as_deref() != Some(next.as_path());
                        if fresh && let Ok(mut slot) = follow_slot().lock() {
                            *slot = Some((next.clone(), Instant::now()));
                            notified = Some(next);
                            drop(slot);
                            if let Some(host) = with_state(|s| s.host) {
                                log_debug!(
                                    SCOPE,
                                    "FOLLOW 发现新选中（查询 {}ms）",
                                    cost.as_millis()
                                );
                                window::post(host.raw(), ipc::WM_QLR_FOLLOW);
                            }
                        }
                    }
                    // 睡之前扣掉这次查询的开销：查询本身可能就要几十毫秒。
                    let wait = interval.saturating_sub(cost.as_millis() as u64).max(10);
                    std::thread::sleep(std::time::Duration::from_millis(wait));
                }
            })
            .ok();
    });
}

/// UI 线程收到"选中项变了"：计时器兜底与 `WM_QLR_FOLLOW` 都走这里。
fn on_follow_wake() {
    // 窗口收着就别跟了：线程可能在我们刚收起时就投了消息，照做会把窗口重新弹出来。
    if !with_state(|s| s.visible).unwrap_or(false) {
        return;
    }
    let picked = follow_slot().lock().ok().and_then(|mut slot| slot.take());
    let Some((path, at)) = picked else {
        return;
    };
    log_debug!(SCOPE, "FOLLOW 应用 +{}ms", at.elapsed().as_millis());
    follow_picked(path);
}

/// 拖动复证：整段拖动结束时打一行摘要（次数、最大间隔、最大单次耗时、期间有没有重绘）。
struct DragTrace {
    started: Option<Instant>,
    moves: u32,
    last_msg: Option<Instant>,
    gap_max_ms: u128,
    cost_max_ms: u128,
    paints: u32,
    moved: (i32, i32),
    origin: (i32, i32),
}

static DRAG_TRACE: OnceLock<Mutex<DragTrace>> = OnceLock::new();

fn drag_trace() -> &'static Mutex<DragTrace> {
    DRAG_TRACE.get_or_init(|| {
        Mutex::new(DragTrace {
            started: None,
            moves: 0,
            last_msg: None,
            gap_max_ms: 0,
            cost_max_ms: 0,
            paints: 0,
            moved: (0, 0),
            origin: (0, 0),
        })
    })
}

fn trace_drag_start() {
    if let Ok(mut trace) = drag_trace().lock() {
        *trace = DragTrace {
            started: Some(Instant::now()),
            moves: 0,
            last_msg: Some(Instant::now()),
            gap_max_ms: 0,
            cost_max_ms: 0,
            paints: 0,
            moved: (0, 0),
            origin: qlr_platform::window::cursor_pos().unwrap_or((0, 0)),
        };
    }
}

fn trace_drag_msg() {
    if let Ok(mut trace) = drag_trace().lock() {
        if let Some(last) = trace.last_msg {
            let gap = last.elapsed().as_millis();
            if gap > trace.gap_max_ms {
                trace.gap_max_ms = gap;
            }
        }
        trace.last_msg = Some(Instant::now());
        trace.moves += 1;
    }
}

fn trace_drag_moved(dx: i32, dy: i32) {
    if let Ok(mut trace) = drag_trace().lock() {
        trace.moved.0 += dx;
        trace.moved.1 += dy;
    }
}

fn trace_drag_cost(ms: u128) {
    if let Ok(mut trace) = drag_trace().lock() {
        if ms > trace.cost_max_ms {
            trace.cost_max_ms = ms;
        }
    }
}

fn trace_drag_paint() {
    if let Ok(mut trace) = drag_trace().lock() {
        trace.paints += 1;
    }
}

fn trace_drag_end() {
    let Ok(mut trace) = drag_trace().lock() else {
        return;
    };
    let Some(started) = trace.started.take() else {
        return;
    };
    log_info!(
        SCOPE,
        "DRAGTRACE 鼠标消息={} 次 总时长={}ms 消息最大间隔={}ms 单次最大耗时={}ms 拖动期间重绘={} 次 窗口位移={}x{} 鼠标位移={}x{}",
        trace.moves,
        started.elapsed().as_millis(),
        trace.gap_max_ms,
        trace.cost_max_ms,
        trace.paints,
        trace.moved.0,
        trace.moved.1,
        qlr_platform::window::cursor_pos()
            .map(|p| p.0 - trace.origin.0)
            .unwrap_or(0),
        qlr_platform::window::cursor_pos()
            .map(|p| p.1 - trace.origin.1)
            .unwrap_or(0)
    );
    trace.moves = 0;
    trace.gap_max_ms = 0;
    trace.cost_max_ms = 0;
    trace.paints = 0;
    trace.last_msg = None;
}

/// UI 线程卡顿探测器：跟随计时器每 400 ms 转一次，间隔明显变大就说明界面被别的东西堵住了。
static UI_TICK: OnceLock<Mutex<Instant>> = OnceLock::new();

fn ui_tick() -> &'static Mutex<Instant> {
    UI_TICK.get_or_init(|| Mutex::new(Instant::now()))
}

fn on_follow_timer() {
    if let Ok(mut last) = ui_tick().lock() {
        let gap = last.elapsed().as_millis();
        if gap > 700 {
            log_warn!(SCOPE, "UISTALL {}ms（UI 线程被堵住，计时器迟到了）", gap);
        }
        *last = Instant::now();
    }
    // 兜底收尸：处理器没被卸掉会让它的子窗口留在屏幕上。
    drain_shell_trash();
    // 正在拖动/改尺寸时不要碰 COM —— `current_selection()` 要枚举 IShellWindows，
    // 一次几十到几百毫秒，正好卡在拖动热路径上会表现为"窗口跟不上鼠标"。
    if with_state(|s| s.drag_from.is_some() || s.resize_edges.is_some()).unwrap_or(false) {
        return;
    }
    if !hotkey::foreground_is_shell() {
        return;
    }
    let (visible, has_path) =
        with_state(|s| (s.visible, s.last_path.is_some())).unwrap_or((false, false));
    if visible && has_path {
        ensure_follow_worker();
        // 主通道是线程的 `WM_QLR_FOLLOW`；这里只是兜底（线程恰好没投成/消息丢了）。
        on_follow_wake();
    } else {
        maybe_prewarm();
    }
}

/// 窗口开着：把预览切到资源管理器当前选中的文件。
fn follow_picked(next: PathBuf) {
    let same = with_state(|s| s.last_path.as_deref() == Some(next.as_path())).unwrap_or(true);
    if same {
        return;
    }
    // 不支持的文件直接忽略（不切换、也不响铃）：鼠标扫过一堆无关文件时，
    // 每越过一个就"叮"一声会把用户逼疯；响铃只在**用户主动按空格**时给。
    let kind = format::kind_for_path(&next);
    let supported =
        qlr_render::viewer_for(kind).is_some() || plugins::host().find_for_path(&next).is_some();
    if !supported {
        log_debug!(SCOPE, "跟随：跳过不支持的 {}（{kind}）", next.display());
        return;
    }
    log_info!(SCOPE, "跟随选中项 → {}", next.display());
    // 计时器路径不经过 `apply_request`，延迟起点要自己重置（否则日志里 elapsed 是上次的）。
    begin_request();
    show_preview(Some(next));
}

/// 窗口收着：把"当前选中的这一个文件"先渲染好，等用户按空格。
///
/// 用户原话（m16283）："二期默认自适应吧，我试试效果，后续我觉得没用的话会放弃掉这项改进"。
/// 所以每一道闸门都往"少烧 CPU"那边偏：多花的那点 CPU 必须换来看得见的即时感。
fn maybe_prewarm() {
    let Some(config) = with_state(|s| s.config.clone()) else {
        return;
    };
    if !config.prewarm.is_on() {
        return;
    }
    // 自适应档：只有"最近真的按过空格"才预热。一个只把 QLRust 当托盘程序挂着的人
    // 不应该被我们每 400 ms 摸一次资源管理器。
    if config.prewarm == qlr_core::config::Prewarm::Adaptive {
        let recent = with_state(|s| {
            s.last_show_at
                .map(|t| t.elapsed() < qlr_core::config::Prewarm::RECENT)
                .unwrap_or(false)
        })
        .unwrap_or(false);
        if !recent {
            return;
        }
    }
    let selection = selection::current_selection();
    // 多选不猜：用户框选一堆文件时"他会看哪个"是纯随机。
    if selection.paths.len() != 1 {
        return;
    }
    let Some(path) = selection.first().map(|p| p.to_path_buf()) else {
        return;
    };
    let kind = format::kind_for_path(&path);
    // 不支持/只有插件接的格式不预热：插件是第三方代码，不该在"用户没要求"时跑。
    if qlr_render::viewer_for(kind).is_none() {
        return;
    }
    if !prewarm_within_budget(&path, kind) {
        // 超预算的文件会一直被选中，每 400 ms 记一行会把日志刷满；只在"换了个文件"时记一次。
        let first = with_state(|s| {
            let fresh = s.prewarm_candidate.as_deref() != Some(path.as_path());
            s.prewarm_candidate = Some(path.clone());
            fresh
        })
        .unwrap_or(false);
        if first {
            log_debug!(SCOPE, "PREWARM 超预算跳过 {}（{kind}）", path.display());
        }
        return;
    }
    // 两拍稳定：同一路径连续两次轮询才动手，用户按住方向键扫文件时不会跟着抢 CPU。
    let stable = with_state(|s| {
        let same = s.prewarm_candidate.as_deref() == Some(path.as_path());
        s.prewarm_candidate = Some(path.clone());
        same
    })
    .unwrap_or(false);
    if !stable {
        log_debug!(SCOPE, "PREWARM 候选 {}（{kind}）", path.display());
        return;
    }
    // 已经有这文件的结果了就别重算。
    if prepared_slot()
        .lock()
        .map(|g| g.as_ref().is_some_and(|p| p.path == path))
        .unwrap_or(false)
    {
        return;
    }
    let Some(preview) = with_state(|s| s.preview) else {
        return;
    };
    let work = window::work_area(preview.raw());
    let natural = qlr_render::natural_size(&path, kind);
    let remembered = with_state(|s| {
        s.last_sizes
            .get(kind.name())
            .copied()
            .or_else(|| s.config.window_size_for(kind.name()))
    })
    .flatten();
    let (width, height) = window_size_for(work, kind, natural, &config, remembered);
    // 预热必须用和上屏**同一个**渲染尺寸，否则结果永远打不中缓存键
    // （1.8.3：PDF 要扣掉滚动条，见 `render_box`）。页数在这里多读一次文件头，
    // 代价远小于"预热永远白做"。
    let pages = if kind == format::Kind::Pdf {
        qlr_render::pdf_page_count(&path)
    } else {
        None
    };
    let (width, height) = render_box(kind, pages, width, height, 1.0);
    log_debug!(
        SCOPE,
        "PREWARM 开始 {} {width}x{height}（自然 {:?}）",
        path.display(),
        natural
    );
    request_prewarm(&path, width.max(1), height.max(1));
}

/// 文本拖拽滚动：把像素位移折算成"行"。
///
/// 攒零头是必须的：`window::line_height()` = 24 px，鼠标一次只动 3–5 px，
/// 每帧取整会让慢速拖动完全不动（体验上像"拖不动"）。
fn scroll_text_by_pixels(pixels: i32) -> bool {
    if !matches!(get_content().as_deref(), Some(Canvas::Text(_))) {
        return false;
    }
    let rows = with_state(|s| pixels_to_rows(&mut s.drag_residual, pixels)).unwrap_or(0);
    if rows != 0 {
        scroll_text_by(rows);
    }
    // 是文本内容就一律"已处理"，别退回平移（文本没有可平移的位图）。
    true
}

/// 像素位移 + 零头 → 要滚几行（负数 = 向上）。纯函数，便于单测。
fn pixels_to_rows(residual: &mut i32, pixels: i32) -> isize {
    *residual += pixels;
    let rows = *residual / window::line_height();
    *residual -= rows * window::line_height();
    rows as isize
}

/// 抬起：**只结束拖拽**。1.0.3 起左键不再关闭窗口（用户要求：左键专用于拖动，
/// 长文件靠拖拽下滑；关窗用 Esc / 再次按空格 / 托盘菜单）。
fn on_drag_end() {
    let dragged = with_state(|s| {
        let d = s.dragged;
        s.drag_from = None;
        s.dragged = false;
        s.drag_residual = 0;
        d
    })
    .unwrap_or(false);
    window::release_capture();
    window::allow_shadow();
    trace_drag_end();
    if dragged {
        // 抖动结束：把拖动期间被节流掉的投影对齐补回来。
        sync_shadow_with(false);
        log_debug!(SCOPE, "拖拽结束");
    }
}

/// 右键按下（1.0.5，1.0.8 收窄）：
/// - **PDF**：记住缩放锚点，上下拖 = 缩放；
/// - **文本/Markdown**：记住滚动锚点，上下拖 = 滚动（左键专职挪窗口）；
/// - **图片**：什么都不做——用户反馈"图片的右键缩放可以取消掉"（图片用滚轮就够，
///   右键拖动留着空，免得误触把窗口尺寸改了）。
///
/// 锚点用**屏幕 y**：缩放会改窗口位置，client y 会跟着漂，
/// 1.0.4 之前用 client y 时右拖缩放会来回抖（1.05 → 0.87 → 1.17 …）。
fn on_zoom_drag_start(hwnd: HWND, lparam: LPARAM) {
    let (_, y) = drag_origin(lparam);
    window::set_capture(hwnd);
    if matches!(get_content().as_deref(), Some(Canvas::Text(_))) {
        with_state(|s| {
            s.scroll_anchor = Some(y);
            s.drag_residual = 0;
        });
        return;
    }
    // 1.0.9：PDF 的右键缩放先整体关掉（用户原话"pdf 先关闭右键缩放的逻辑"）。
    // 缩放改由顶栏/快捷键做，右键只保留文本滚动；图片的右键缩放 1.0.8 已撤。
    window::release_capture();
}

/// 右键拖动：图像往上是放大（每 60 px 一档 1.25×）；文本往上是往上滚。
fn on_zoom_drag_move(lparam: LPARAM) {
    let (_, y) = drag_origin(lparam);
    // 文本：右键上下拖 = 按行滚动。
    if let Some(anchor_y) = with_state(|s| s.scroll_anchor).flatten() {
        let dy = y - anchor_y;
        if dy != 0 {
            with_state(|s| s.scroll_anchor = Some(y));
            scroll_text_by_pixels(-dy);
        }
        return;
    }
    let Some((anchor_y, anchor_zoom)) = with_state(|s| s.zoom_anchor).flatten() else {
        return;
    };
    let dy = (anchor_y - y) as f32; // 往上拖为正
    let target = zoom_drag_target(anchor_zoom, dy);
    let current = with_state(|s| s.zoom).unwrap_or(1.0);
    // 变化不到 2% 就别动：省下无意义的窗口重排。
    if (target / current - 1.0).abs() < 0.02 {
        return;
    }
    // 拖动期间只改窗口、不重渲染（1.0.5）：每一帧重渲染一页 PDF 就是"卡顿"的来源。
    set_zoom_to(target, current, false);
    // 缩放后重新锚定：增量式拖动，松手回拖不会"跳"。
    with_state(|s| s.zoom_anchor = Some((y, target)));
}

/// 右键拖动 → 目标缩放（纯函数，便于单测）。
///
/// 往上拖（`dy > 0`）放大、往下拖缩小，每 60 px 一档 1.25×——和老滚轮手感一致。
fn zoom_drag_target(anchor_zoom: f32, dy: f32) -> f32 {
    (anchor_zoom * 1.25_f32.powf(dy / 60.0)).clamp(ZOOM_MIN, ZOOM_MAX)
}

/// 右键抬起：结束缩放/滚动，**缩放这时候才真正渲染**（1.0.5）。
///
/// 拖动期间只改窗口 + 拉伸旧位图，松手才按最终尺寸出一张高清图：一次渲染，
/// 而不是每一帧一次。
fn on_zoom_drag_end() {
    let (zooming, scrolling) = with_state(|s| {
        let zooming = s.zoom_anchor.take().is_some();
        let scrolling = s.scroll_anchor.take().is_some();
        s.drag_residual = 0;
        (zooming, scrolling)
    })
    .unwrap_or((false, false));
    window::release_capture();
    if scrolling && !zooming {
        log_debug!(
            SCOPE,
            "右键滚动结束 scroll={}",
            with_state(|s| s.scroll).unwrap_or(0)
        );
        return;
    }
    if !zooming {
        return;
    }
    let zoom = with_state(|s| s.zoom).unwrap_or(1.0);
    // 预览态还没被新帧顶掉 → 补一次真正的渲染。
    let pending = with_state(|s| s.zoom_preview).unwrap_or(false);
    if pending && let Some((preview, Some(path))) = with_state(|s| (s.preview, s.last_path.clone()))
    {
        let (width, height) = view_size(preview.raw());
        let page = with_state(|s| s.page).unwrap_or(0);
        request_render(&path, width, height, page);
        log_debug!(SCOPE, "ZOOM-DRAG 松手补渲染 {width}x{height}");
    }
    log_info!(SCOPE, "ZOOM-DRAG 结束 zoom={zoom:.2}");
}

/// `WM_MOUSEMOVE` 分流：放大镜（Shift）优先，其次右键（缩放/滚动），否则左键拖动（挪窗口）。
fn on_mouse_move(lparam: LPARAM) {
    // 放大镜：按住 Shift 就在光标处开镜头，松开即关。它只是叠一层放大，不影响其他手势，
    // 所以先处理、并且不 return —— 用户可能一边按 Shift 一边拖动。
    update_lens(lparam);
    // 正在拖边改大小（1.0.10）：这时只改窗口矩形，不挪窗口、不平移画面。
    if with_state(|s| s.resize_edges.is_some()).unwrap_or(false) {
        on_resize_move(lparam);
        return;
    }
    // 正在拖滚动条（1.0.9）：这时只改滚动位置，不挪窗口、不平移画面。
    if with_state(|s| s.scroll_drag).unwrap_or(false) {
        let (_, y) = client_xy(lparam);
        if let Some(preview) = with_state(|s| s.preview) {
            apply_scroll_y(preview.raw(), y);
        }
        return;
    }
    let (zooming, scrolling) = with_state(|s| (s.zoom_anchor.is_some(), s.scroll_anchor.is_some()))
        .unwrap_or((false, false));
    if zooming || scrolling {
        on_zoom_drag_move(lparam);
    } else {
        on_drag_move(lparam);
    }
}

/// 按 Shift 状态更新放大镜位置；有变化才重绘。
///
/// 只有位图（图片 / PDF）才需要放大——文本本来就是矢量字，放大只会变模糊。
fn update_lens(lparam: LPARAM) {
    let Some(preview) = with_state(|s| s.preview) else {
        return;
    };
    if !with_state(|s| s.visible).unwrap_or(false) {
        return;
    }
    let want = lens_down() && matches!(get_content().as_deref(), Some(Canvas::Bitmap(_)));
    if !want {
        let changed = with_state(|s| {
            if s.lens.take().is_some() {
                log_debug!(SCOPE, "LENS 关");
                true
            } else {
                false
            }
        })
        .unwrap_or(false);
        if changed {
            window::invalidate(preview.raw());
        }
        return;
    }
    let (x, y) = client_xy(lparam);
    let changed = with_state(|s| {
        if s.lens == Some((x, y)) {
            false
        } else {
            let first = s.lens.is_none();
            s.lens = Some((x, y));
            if first {
                log_debug!(SCOPE, "LENS 开 {x}x{y} zoom={:.1}×", s.lens_zoom);
            }
            true
        }
    })
    .unwrap_or(false);
    if changed {
        window::invalidate(preview.raw());
    }
}

/// 内容画完之后再叠一层放大镜（必须在同一次 `WM_PAINT` 内、内容之后调用）。
fn paint_lens_if_any(hwnd: HWND) {
    let Some(center) = with_state(|s| s.lens).flatten() else {
        return;
    };
    let zoom = with_state(|s| s.lens_zoom).unwrap_or(LENS_DEFAULT_ZOOM);
    // 镜头整体留在窗口里：靠边时把镜头往里推，而不是让 GDI 裁掉一半。
    let rc = window::client_rect(hwnd);
    let (r, cw, ch) = (
        window::lens_radius(),
        (rc.right - rc.left).max(1),
        (rc.bottom - rc.top).max(1),
    );
    let (x, y) = (
        center.0.clamp(r, (cw - r).max(r)),
        center.1.clamp(r, (ch - r).max(r)),
    );
    window::paint_lens(hwnd, (x, y), zoom);
}

/// 放大镜的"按住"键此刻是否按着（1.0.7 起可配置，默认 Shift）。
///
/// 不用 `window::shift_down()`：设置页允许把放大镜换成 Ctrl/Alt，
/// 而"哪个键"的真值只在 `config.hotkeys.lens_hold` 里。
fn lens_down() -> bool {
    let Some(binding) = with_state(|s| s.config.hotkeys.lens_binding()) else {
        return window::shift_down();
    };
    window::key_down(binding.vk)
}

/// 放大镜键 + 滚轮 = 调放大镜倍数；返回 true 表示这次滚轮已被放大镜吃掉。
fn lens_wheel(notches: isize) -> bool {
    if !lens_down() {
        return false;
    }
    let Some(preview) = with_state(|s| s.preview) else {
        return false;
    };
    if !matches!(get_content().as_deref(), Some(Canvas::Bitmap(_))) {
        return false;
    }
    let zoom = with_state(|s| {
        s.lens_zoom = (s.lens_zoom + notches as f32).clamp(LENS_MIN_ZOOM, LENS_MAX_ZOOM);
        s.lens_zoom
    })
    .unwrap_or(LENS_DEFAULT_ZOOM);
    log_debug!(SCOPE, "LENS 倍数 {zoom:.1}×");
    window::invalidate(preview.raw());
    true
}

/// 平移（像素）。只有位图大于客户区时才有视觉效果。
fn pan_by(dx: i32, dy: i32) {
    let Some((preview, rc, zoom)) =
        with_state(|s| (s.preview, window::client_rect(s.preview.raw()), s.zoom))
    else {
        return;
    };
    let (cw, ch) = ((rc.right - rc.left).max(1), (rc.bottom - rc.top).max(1));
    // 每一步挪 1/12 屏，和缩放倍数无关（用户按的是"屏"的直觉，不是像素）。
    let (step_x, step_y) = ((cw / 12).max(8), (ch / 12).max(8));
    with_state(|s| {
        let (bw, bh) = (
            scaled(cw as u32, zoom) as i32,
            scaled(ch as u32, zoom) as i32,
        );
        s.pan.0 = (s.pan.0 + dx * step_x).clamp(0, (bw - cw).max(0));
        s.pan.1 = (s.pan.1 + dy * step_y).clamp(0, (bh - ch).max(0));
    });
    window::invalidate(preview.raw());
    log_info!(
        SCOPE,
        "PAN {:?} zoom={zoom:.2}",
        with_state(|s| s.pan).unwrap_or((0, 0))
    );
}

/// 在当前文档里翻 delta 页（目前只有 PDF 有页的概念）。
fn step_page(delta: isize) {
    let Some((preview, path, pages, page)) =
        with_state(|s| (s.preview, s.last_path.clone(), s.pages, s.page))
    else {
        return;
    };
    let Some(pages) = pages.filter(|n| *n > 1) else {
        return;
    };
    let Some(path) = path else { return };
    let target = (page as isize + delta).clamp(0, pages as isize - 1) as usize;
    if target == page {
        log_debug!(SCOPE, "已经在第 {} 页，忽略翻页", page + 1);
        return;
    }
    with_state(|s| {
        s.page = target;
        // 换页从页顶看起（滚轮滚到页底再往下滚就翻页，1.0.3）。
        s.pan = (0, 0);
    });

    let (width, height) = view_size(preview.raw());
    request_render(&path, width, height, target);

    // 1.0.8 修"翻页频闪"：以前这里立刻把内容改成占位文字再 invalidate，
    // 新位图回来之前窗口就闪一下白底+文字。现在**什么都不动**——旧的一页继续留在
    // 屏幕上，等 `on_render_done` 拿到新页再整帧换掉（顶部页码也一起更新）。
    log_info!(SCOPE, "PAGE {}/{}", target + 1, pages);
}

/// 是否按"图片缩放比例"开窗。
///
/// 1.7.9 起 SVG / 视频也归到这里：它们和位图一样有"原始像素尺寸"，用户设的初始缩放
/// （默认 50%）对它们同样合理；PDF 不跟（PDF 有自己的"适应窗口"口径）。
fn uses_image_zoom(kind: format::Kind) -> bool {
    matches!(
        kind,
        format::Kind::Image | format::Kind::Svg | format::Kind::Video
    )
}

/// 这个文件打开时窗口该多大（宽, 高，**含顶栏**）。
///
/// `show_preview` 与预渲染（1.1.0）共用这一份计算：两边一漂移，预热就永远打不中
/// （白烧 CPU 还看不出效果），所以尺寸规则只写在这里。
fn window_size_for(
    work: RECT,
    kind: format::Kind,
    natural: Option<(u32, u32)>,
    config: &Config,
    remembered: Option<(i32, i32)>,
) -> (i32, i32) {
    // 顶栏（1.0.9）四种格式都有：只有"按内容开窗"的格式才需要额外留一条。
    // 视频再加一条进度条的高度（1.7.9）：不留的话画面会被进度条挤扁一圈。
    let reserve = if natural.is_some() {
        window::chrome_height()
    } else {
        0
    } + if kind == format::Kind::Video {
        window::audio_bar_h()
    } else {
        0
    };
    let base = match natural {
        Some((w, h)) => {
            // 图片的初始缩放走配置（默认 50%，用户可在设置里改）；PDF 保持"适应窗口"。
            let factor = if uses_image_zoom(kind) {
                config.image_zoom_percent as f32 / 100.0
            } else {
                1.0
            };
            let mut b = window::natural_rect_scaled(work, w as i32, h as i32, factor);
            b.bottom = (b.bottom + reserve).min(work.bottom);
            b
        }
        // 文本/Markdown 没有"原始尺寸"（1.0.5）：固定阅读宽度，高度按比例。
        None => window::text_rect(work, config.height_ratio),
    };
    let (mut width, mut height) = (
        (base.right - base.left).max(1),
        (base.bottom - base.top).max(1),
    );
    // 尺寸记忆（1.0.14）：用户自己改过这个格式的窗口大小就照它开。
    if let Some((w, h)) = remembered {
        let (ww, wh) = (work.right - work.left, work.bottom - work.top);
        width = w.clamp(min_window_w(), ww);
        height = h.clamp(min_window_h(), wh);
    }
    // 图片/视频（1.8.1）：窗口比例 = 素材比例，记忆下来的尺寸也要夹回来——
    // 否则上次拖成方形，下次开图又变成"内容在方窗口里留白/变形"。
    if matches!(kind, format::Kind::Image | format::Kind::Video) {
        if let Some((w, h)) = natural {
            if w > 0 && h > 0 {
                let aspect = w as f64 / h as f64;
                (width, height) = media_window_size(work, aspect, width, media_bar(kind));
            }
        }
    }
    (width, height)
}

/// 视频画面下面还有一条进度条（`window::audio_bar_h()`），图片没有。
fn media_bar(kind: format::Kind) -> i32 {
    if kind == format::Kind::Video {
        window::audio_bar_h()
    } else {
        0
    }
}

/// 让内容铺满且不变形的窗口尺寸（1.8.1 图片/视频）。
///
/// `content_w` 是"想开多宽"，高度由比例反算；超出工作区就先压高度再反算宽度，
/// 绝不把内容算出 0。返回的是**整窗**尺寸（内容 + 顶栏 + 进度条）。
fn media_window_size(work: RECT, aspect: f64, content_w: i32, bar: i32) -> (i32, i32) {
    let extra = window::chrome_height() + bar;
    let (ww, wh) = (work.right - work.left, work.bottom - work.top);
    let mut cw = content_w.max(min_window_w());
    let mut ch = (cw as f64 / aspect).round().max(1.0) as i32;
    if ch + extra > wh {
        ch = (wh - extra).max(min_window_h());
        cw = (ch as f64 * aspect).round().max(1.0) as i32;
    }
    if cw > ww {
        cw = ww;
        ch = (cw as f64 / aspect).round().max(1.0) as i32;
    }
    (cw, ch + extra)
}

/// 这个格式记住的窗口尺寸：优先本次会话的（`last_sizes`），其次上次落盘的（1.0.14）。
fn remembered_size(kind_key: &str) -> Option<(i32, i32)> {
    with_state(|s| {
        s.last_sizes
            .get(kind_key)
            .copied()
            .or_else(|| s.config.window_size_for(kind_key))
    })
    .flatten()
}

fn show_preview(path: Option<PathBuf>) {
    let Some((config, preview_hwnd, last)) =
        with_state(|s| (s.config.clone(), s.preview, s.last_path.clone()))
    else {
        return;
    };
    let target = path.or(last);
    // 与 hide_preview 的 GDI/USER 计数配对：两个数字一起看，才知道增长发生在 show 还是 hide。
    let (gdi0, user0) = window::gui_resources();
    log_debug!(SCOPE, "SHOW 起点 GDI={gdi0} USER={user0}");

    // 现场报障："md 有时候第一次点 × 没反应，第二次才关"。如果这次 SHOW 紧跟在一次
    // HIDE 之后，用户看到的就是"点了没反应"——不管是谁（渲染回调、跟随计时器、
    // 热键）把窗口叫回来的，先把这条痕留下，下次复现直接看日志定位。
    if let Some(at) = *last_hide_slot() {
        let gap = at.elapsed().as_millis();
        if gap < 1500 {
            log_info!(SCOPE, "HIDE→SHOW 间隔 {gap}ms（会表现为「点了 × 没反应」）");
        }
    }

    // 换文件就把页缓存清掉（1.3.0）：缓存按路径记，旧文档的几十页继续占内存没意义。
    // 同一个文件翻页不清——那正是缓存要服务的场景。
    if let Some(new) = target.as_deref() {
        let same = with_state(|s| s.last_path.as_deref() == Some(new)).unwrap_or(false);
        if !same {
            pagecache::clear();
        }
    }

    let work = window::work_area(preview_hwnd.raw());
    let kind = target
        .as_deref()
        .map(format::kind_for_path)
        .unwrap_or(format::Kind::Unsupported);
    // 按**原始尺寸**开窗（1.0.1）：图片读文件头像素尺寸、PDF 取首页 point 尺寸，
    // 探测不到（文本/插件/坏文件）才退回配置里的比例。
    let natural = target
        .as_deref()
        .and_then(|p| qlr_render::natural_size(p, kind));
    // 图片/视频（1.8.1）：窗口比例锁成素材比例——之后改大小只做等比缩放、内容始终铺满。
    let aspect = match (kind, natural) {
        (format::Kind::Image | format::Kind::Video, Some((w, h))) if w > 0 && h > 0 => {
            Some(w as f64 / h as f64)
        }
        _ => None,
    };
    with_state(|s| s.media_aspect = aspect);
    // PDF 先问总页数（同一把锁里再开文档，代价只有一次 read；页数拿不到不影响渲染）。
    let pages = if kind == format::Kind::Pdf {
        target.as_deref().and_then(qlr_render::pdf_page_count)
    } else {
        None
    };
    // 尺寸规则（1.0.1 / 1.0.9 / 1.0.14 / 1.1.0）：集中到 `window_size_for`，
    // 预渲染用同一份算，两边才不会漂移。
    let kind_key = kind.name();
    let (content_w, content_h) =
        window_size_for(work, kind, natural, &config, remembered_size(kind_key));
    // 顶栏（1.0.9）：只有按内容开窗的格式才把顶栏算在内容之外。
    let reserve = if natural.is_some() {
        window::chrome_height()
    } else {
        0
    };
    // 居中的"底稿"只用来定 left/top，宽高一律以 `window_size_for` 为准。
    let mut rect = match natural {
        Some((w, h)) => {
            let factor = if uses_image_zoom(kind) {
                config.image_zoom_percent as f32 / 100.0
            } else {
                1.0
            };
            window::natural_rect_scaled(work, w as i32, h as i32, factor)
        }
        // 文本/Markdown 没有"原始尺寸"（1.0.5）：固定阅读宽度，高度按比例。
        None => window::text_rect(work, config.height_ratio),
    };
    rect.right = rect.left + content_w;
    rect.bottom = rect.top + content_h;
    // 位置记忆（1.0.13）：
    // - 窗口已经开着（跟随选中项换文件、或连按两次空格换内容）→ 就用它现在的位置，
    //   换文件不该把窗口搬回屏幕中央；
    // - 刚打开 → 用上次记住的位置，没记过才保持居中。
    let wanted = if with_state(|s| s.visible).unwrap_or(false) {
        current_window_pos().or(config.window_pos)
    } else {
        config.window_pos
    };
    place_at(&mut rect, wanted, work);
    let (width, height) = (
        (rect.right - rect.left).max(1) as u32,
        (rect.bottom - rect.top).max(1) as u32,
    );
    if let Some((nw, nh)) = natural {
        log_info!(SCOPE, "开窗尺寸 {}x{}（原始 {}x{}）", width, height, nw, nh);
    }

    // 1) 先定内容与占位：旧帧立刻作废，避免"上一张图"残留在新窗口上。
    // 内置查看器优先，内置没有才问插件（与渲染线程的分发规则一致）。
    let plugin = target
        .as_deref()
        .filter(|_| qlr_render::viewer_for(kind).is_none())
        .and_then(|p| plugins::host().find_for_path(p));
    // 压缩包（1.7.3）：内置列表视图是壳层自己画的，既没有 `Viewer` 也可能没有插件，
    // 所以单独放行一次——否则 `.zip` 会掉进下面"不支持"分支被响铃拒绝。
    let archive = target.as_deref().is_some_and(is_archive);
    let renderable =
        target.is_some() && (qlr_render::viewer_for(kind).is_some() || plugin.is_some() || archive);

    // 不支持的文件（1.0.10）：**一个像素都不画**，只"响一声"然后收起窗口。
    //
    // 用户原话："预览不支持的文件时，不要有任何预览，系统直接响铃报错即可"。
    // 以前会弹一个占位文字窗口（写着插件目录、怎么退出），扫文件时非常吵；
    // 而且那个窗口一出现，用户按空格的第一反应是"我明明按了怎么没反应"。
    if !renderable {
        log_info!(
            SCOPE,
            "SHOW 拒绝 kind={} target={}",
            kind,
            target
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "-".to_string())
        );
        window::beep();
        if with_state(|s| s.visible).unwrap_or(false) {
            hide_preview();
        }
        return;
    }

    // 新文件一律回到 100%：上一个文件留下的缩放/平移套到这张图上会"开着就是裁切过的"
    // （1.0.1 修：之前 show_preview 不重置 zoom）。
    // PDF 有顶部页码条（1.0.8）：窗口高 = 内容高 + HEADER_HEIGHT，所以缩放基准确记"内容区"。
    with_state(|s| {
        s.zoom = 1.0;
        s.pan = (0, 0);
        s.zoom_preview = false;
        // 带上一个文件残留的顶栏状态会串味：悬停按钮、页码输入、滚动条拖拽都清掉。
        s.hover_button = None;
        s.page_edit = None;
        s.scroll_drag = false;
        // "阻止关闭"只对当前这一次预览有效（换文件就重新问一次）。
        s.pinned = false;
        s.resize_edges = None;
        s.resize_from = None;
        // 记住自然尺寸（日志用）与开窗尺寸（zoom 的基准，1.0.2 窗口跟随缩放）。
        s.natural = natural;
        s.base = natural.map(|_| (width, (height as i32 - reserve).max(1) as u32));
    });
    hotkey::set_capture_digits(false);
    let zoom = 1.0_f32;
    with_state(|s| {
        s.page = 0;
        s.pages = pages;
    });

    clear_archive_view();
    if let Some(p) = target.as_deref()
        && is_archive(p)
    {
        if !open_archive_view(p) {
            // 解析失败（不是真 zip / 截断 / 加密 / 空）：按"不支持"处理——响一声、
            // 不画任何东西。别往下面掉：那条路会给压缩包建"正在渲染…"的占位，
            // 再等一次注定失败的渲染，用户看到的是"卡住了一下然后报错"。
            log_warn!(SCOPE, "压缩包解析失败，按不支持处理：{}", p.display());
            window::beep();
            if with_state(|s| s.visible).unwrap_or(false) {
                hide_preview();
            }
            return;
        }
        set_content(None);
        set_lines(Vec::new());
        // 压缩包走内置列表视图，不需要系统预览处理器：上一个文件的处理器必须当场卸掉，
        // 否则它的子窗口会一直盖在列表上面（1.7.5，见 `drop_stale_shell_preview`）。
        drop_stale_shell_preview();
        window::show_at(preview_hwnd.raw(), rect, config.topmost);
        sync_shadow();
        hotkey::set_active(true);
        // 托盘没加上时，第一次"窗口真的可见"就用 toast 说一次（1.8.3）。
        notify_tray_failure_once(preview_hwnd.raw());
        with_state(|s| {
            s.visible = true;
            s.last_path = Some(p.to_path_buf());
            s.last_show_at = Some(Instant::now());
        });
        window::invalidate(preview_hwnd.raw());
        log_info!(SCOPE, "SHOW archive {}", p.display());
        return;
    }
    set_content(None);
    let mut lines = build_lines(target.as_deref());
    // 交给系统预览处理器（Office）时**连文件信息页也一起删掉**（1.8.1 用户要求
    // "把占位页面直接删掉"）：窗口先空着，处理器第一帧到了直接铺满，中间不闪一页文字。
    // 只有走我们自己渲染的格式才留"正在渲染…"那一行。
    let shell = shell_candidate(target.as_deref());
    if shell {
        lines.clear();
    } else if renderable {
        lines.insert(2, "正在渲染…".to_string());
    }
    set_lines(lines);
    // 2) 放锁之后再投任务/动窗口（见 LINES 的注释）。
    //
    // 先看预渲染槽（1.1.0）：命中的话直接上屏，省掉一次 16–60 ms 的渲染。
    // 渲染尺寸要和翻页时算的一致（PDF 扣掉顶栏）：`view_size` 就是这么算的，
    // 两边不一致的话页缓存 key 永远对不上，缓存等于白做（1.3.0 踩过）。
    // 渲染盒要扣掉滚动条：`view_size` 量的就是这个宽度（1.8.3，见 `render_box`）。
    let (render_w, render_h) = render_box(
        kind,
        pages,
        width as i32,
        (height as i32 - reserve).max(1),
        zoom,
    );
    // 1.4.0：重开同一个文件（没改过）直接上屏上一帧，连渲染都省掉。
    // 先查这个再看预渲染槽：上一帧是"刚刚真的显示过"的，比预热结果更新。
    let wanted = target
        .as_deref()
        .map(|p| frame_key(p, 0, render_w, render_h, zoom > 1.0));
    let last = wanted
        .as_ref()
        .and_then(take_last_frame)
        .filter(|canvas| frame_matches_size(canvas, render_w, render_h));
    let prepared = if last.is_some() {
        None
    } else {
        // 预热结果是按"当时算出来的尺寸"渲的，窗口/页数一变就对不上了。
        // 尺寸不匹配还拿去上屏，就是一次满屏拉伸（发虚 + 每帧重采样），
        // 还不如就地下场重渲（下一页缓存/预渲染还会命中）。
        take_prepared(target.as_deref(), render_w, render_h)
            .filter(|canvas| frame_matches_size(canvas, render_w, render_h))
    };
    // 1.4.1：系统装了 Office 预览处理器就优先交给它（真排版）。
    // 这里只做"判断"：真正 DoPreview 必须等窗口 show 出来之后——在隐藏窗口上 DoPreview
    // 会让处理器等一个永远不会到来的激活，UI 线程直接卡死（踩过，表现为"未响应"）。
    let shell_wanted = shell_candidate(target.as_deref());
    // 换到"不需要系统预览处理器"的文件（图片/文本…）时同样要卸掉上一个文件的处理器，
    // 否则它的子窗口会盖住新内容（1.7.5，见 `drop_stale_shell_preview`）。
    if !shell_wanted {
        drop_stale_shell_preview();
    }
    // 命中缓存/预渲染时内容已经是"不透明"的，后面就不能再开亚克力（否则整个窗口变半透明灰）。
    let mut presented = false;
    if !shell_wanted
        && renderable
        && let Some(p) = target.as_deref()
    {
        match (last, prepared) {
            (Some(canvas), _) => {
                log_info!(SCOPE, "LASTFRAME 命中 {}（跳过渲染）", p.display());
                present_canvas(canvas);
                presented = true;
            }
            (None, Some(canvas)) => {
                log_info!(SCOPE, "PREWARM 命中 {}（跳过渲染）", p.display());
                present_canvas(canvas);
                presented = true;
            }
            (None, None) => request_render(p, render_w, render_h, 0),
        }
    }

    // 占位文字阶段才用亚克力；已经有真内容（缓存命中）就必须按"不透明"处理，
    // 否则第二次预览会整窗发灰 —— 顶栏也会跟着变灰（用户 1.4.10 反馈）。
    let _ = presented;

    // 1.7.4 修（用户反馈"顶栏一直是错的文件名"）：**先把文件名记进状态，再显示窗口**。
    //
    // 为什么顺序这么重要：下面 `open_shell_preview` 是跨进程 COM 调用，STA 上会泵消息，
    // 期间到达的 WM_PAINT 会真的跑一遍 `repaint()`；那时若 `last_path` 还是上一个文件，
    // 顶栏就按旧名字画了一帧，而且这之后没有任何 `invalidate` 去纠正它——旧标题就留在
    // 屏幕上了（直到用户滚动/缩放/碰顶栏才被刷掉）。
    // 换文件的"冻帧"（1.7.6）：窗口已经开着、并且这次要交给系统预览处理器时，
    // 内容区先停在上一帧——`open_shell_preview` 里的 `DoPreview` 要几十到几百毫秒，
    // 这期间旧处理器已经卸掉、新的一帧还没来，整窗重画就会露出占位文字页（割裂感）。
    let hold = shell_wanted && with_state(|s| s.visible && s.last_path.is_some()).unwrap_or(false);
    with_state(|s| s.hold_frame = hold);
    with_state(|s| s.last_path = target.clone());
    // 留一条可断言的日志：验证脚本靠"这一行出现在 `SHELL 预览已接管` 之前"证明上面那个顺序。
    if let Some(p) = target.as_deref() {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.display().to_string());
        log_debug!(SCOPE, "标题栏 ← {name}");
    }
    window::show_at(preview_hwnd.raw(), rect, config.topmost);
    // 窗口已经可见，这时才能把内容交给系统预览处理器（顺序反了会卡死，见上面注释）。
    // 处理器失败（没装 Office、注册表指向的 DLL 缺失等）要立刻退回插件渲染，
    // 否则屏幕上只留着"正在渲染…"的占位文字，用户会以为卡住了。
    if shell_wanted && !open_shell_preview(target.as_deref()) && renderable {
        // 处理器没起来：立刻退回插件渲染（否则屏幕上只剩"正在渲染…"的占位文字）。
        if let Some(p) = target.as_deref() {
            request_render(p, render_w, render_h, 0);
        }
    }
    // 处理器没起来（或压根没要处理器）就别冻帧了，让占位/渲染帧正常上屏。
    if with_state(|s| s.hold_frame).unwrap_or(false)
        && with_state(|s| s.shell_preview.is_none()).unwrap_or(true)
    {
        with_state(|s| s.hold_frame = false);
        window::invalidate(preview_hwnd.raw());
    }
    sync_shell_rect();
    // 位置记忆（1.0.13）：窗口刚摆好，把落点记下来（不管来自记忆还是居中）。
    if let Some(pos) = current_window_pos() {
        with_state(|s| s.last_pos = Some(pos));
    }
    // 柔和投影（1.0.10）：显示后立刻同步一次色调层（位置 + alpha 图）。
    sync_shadow();
    // 开着的时候才需要旁听翻页键。
    // 跟随计时器从启动就跑（1.1.0）：窗口收着的时候它负责预渲染，所以这里不再重开。
    hotkey::set_active(true);
    // 托盘没加上时，第一次"窗口真的可见"就用 toast 说一次（1.8.3）。
    notify_tray_failure_once(preview_hwnd.raw());

    // 声音与画面（1.7.9）：窗口已经可见，音频自动开始播、视频自动开始放；
    // 其它格式一律停掉（从 wav 切到图片时不能继续在后台响）。
    match kind {
        format::Kind::Audio => {
            if let Some(p) = target.as_deref() {
                start_audio(p);
            }
            stop_video();
        }
        format::Kind::Video => {
            if let Some(p) = target.as_deref() {
                start_video(p);
            }
            stop_audio();
        }
        _ => {
            stop_audio();
            stop_video();
        }
    }

    let shown_at = with_state(|s| {
        s.visible = true;
        // `last_path` 在显示窗口**之前**就写好了（1.7.4，见上面那段注释）。
        // 自适应预渲染靠"最近按过空格没有"决定要不要提前画（1.1.0）。
        s.last_show_at = Some(Instant::now());
        s.request_started.elapsed().as_millis()
    })
    .unwrap_or_default();
    log_info!(
        SCOPE,
        "SHOW target={} kind={} renderable={} plugin={} size={width}x{height} elapsed={shown_at}ms",
        with_state(|s| s.last_path.as_ref().map(|p| p.display().to_string()))
            .flatten()
            .unwrap_or_else(|| "(无)".to_string()),
        kind,
        renderable,
        plugin.map(|p| p.name()).unwrap_or("-")
    );
}

/// 预览窗当前的左上角屏幕坐标（窗口无效时 `None`）。
fn current_window_pos() -> Option<(i32, i32)> {
    let preview = with_state(|s| s.preview)?;
    let rc = window::window_rect(preview.raw());
    if rc.right <= rc.left && rc.bottom <= rc.top {
        return None;
    }
    Some((rc.left, rc.top))
}

/// 把"预览窗现在在哪"记进内存（1.0.13 位置记忆）。
///
/// 调用的地方 = 所有会改变窗口位置的路径：显示、拖动、拖边改大小、缩放（缩放会按中心改尺寸）。
/// 只更新内存，不落盘；落盘在 `persist_position()`（收起窗口 / 退出时才写，避免拖一次写一次盘）。
fn remember_position() {
    if !with_state(|s| s.visible).unwrap_or(false) {
        return;
    }
    if let Some(pos) = current_window_pos() {
        with_state(|s| s.last_pos = Some(pos));
    }
}

/// 把"预览窗现在多大"记进内存（1.0.14 尺寸记忆），按当前内容的格式分别记。
///
/// 只在**用户自己改过尺寸**的路径上调用（拖边改大小、缩放），不在显示时调用：
/// 显示时按内容算出来的尺寸如果也记，一张 8000×6000 的大图就会把之后所有图片的
/// 初始尺寸都顶到工作区上限，"按原图 50% 开窗"的设定就形同虚设。
fn remember_size() {
    if !with_state(|s| s.visible).unwrap_or(false) {
        return;
    }
    let Some(kind) = with_state(|s| s.last_path.clone())
        .flatten()
        .map(|p| format::kind_for_path(&p))
    else {
        return;
    };
    let Some((w, h)) = current_window_size() else {
        return;
    };
    // 太小的不记（拖到最小尺寸时的一瞬间），否则下次开窗只有一条缝。
    if w < min_window_w() || h < min_window_h() {
        return;
    }
    with_state(|s| {
        s.last_sizes.insert(kind.name().to_string(), (w, h));
    });
    log_debug!(SCOPE, "记住窗口尺寸 {} {}x{}", kind.name(), w, h);
}

/// 预览窗当前的客户区尺寸（窗口无效时 `None`）。
fn current_window_size() -> Option<(i32, i32)> {
    let preview = with_state(|s| s.preview)?;
    let rc = window::window_rect(preview.raw());
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    if w <= 0 || h <= 0 {
        return None;
    }
    Some((w, h))
}

/// 把记住的位置与尺寸写进 `config.json`（1.0.13 / 1.0.14）。
///
/// 先 `remember_position()` 再写，所以调用方不必自己先捕获。写失败只记警告——
/// 记忆丢了不影响这次预览，没必要打断用户。
fn persist_position() {
    remember_position();
    let Some((mut config, pos, sizes)) = with_state(|s| {
        (
            s.config.clone(),
            s.last_pos.or(s.config.window_pos),
            s.last_sizes.clone(),
        )
    }) else {
        return;
    };
    let mut changed = false;
    if let Some(pos) = pos
        && config.window_pos != Some(pos)
    {
        config.window_pos = Some(pos);
        changed = true;
    }
    for (kind, size) in sizes {
        if config.window_size_for(&kind) != Some(size) {
            let (w, h) = size;
            if w >= 200 && h >= 120 {
                config.set_window_size(&kind, size);
                changed = true;
            }
        }
    }
    if !changed {
        return; // 没变就别写盘
    }
    let path = paths::config_path();
    match config.save(&path) {
        Ok(()) => {
            with_state(|s| {
                s.config.window_pos = config.window_pos;
                s.config.window_sizes = config.window_sizes.clone();
            });
            log_debug!(
                SCOPE,
                "记住窗口几何 pos={:?} sizes={}",
                config.window_pos,
                config.window_sizes.len()
            );
        }
        Err(e) => log_warn!(SCOPE, "窗口几何保存失败：{e}"),
    }
}

/// 把矩形（只取尺寸）摆到记住的位置上，夹回工作区（1.0.13）。
///
/// `wanted` 为 `None` 表示没有记忆，保持调用方算好的居中位置。
fn place_at(rect: &mut RECT, wanted: Option<(i32, i32)>, work: RECT) {
    let Some((x, y)) = wanted else {
        return;
    };
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    let mut rc = RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    };
    // 换了显示器/分辨率时，记下的坐标可能在工作区外——夹回来，别让窗口消失。
    window::clamp_into(&mut rc, work);
    *rect = rc;
}

/// 把投影浮层对齐到预览窗（拖动/改大小/显示后都要调；内部会缓存 alpha 图）。
/// 上一次真正同步投影的时刻（拖动热路径上做节流用）。
static LAST_SHADOW_SYNC: OnceLock<Mutex<Instant>> = OnceLock::new();

fn last_shadow_sync() -> &'static Mutex<Instant> {
    LAST_SHADOW_SYNC.get_or_init(|| Mutex::new(Instant::now() - std::time::Duration::from_secs(1)))
}

/// 把投影窗口对齐到预览窗口。
///
/// `throttle` 为真时最多每 8 ms 做一次：投影是**另一个分层窗口**，`SetWindowPos` 一次要走 DWM，
/// 拖动时每帧都同步会让主窗口"追不上鼠标"（用户 1.4.1 反馈）。拖动结束会强制同步一次。
fn sync_shadow_with(throttle: bool) {
    // 拖动期间投影必须一直藏着：`sync_shadow` 会把投影窗口 `ShowWindow` 出来，
    // 只在 on_drag_start 里藏一次会被下一次移动又亮起来（1.4.8 踩过）。
    if with_state(|s| s.drag_from.is_some()).unwrap_or(false) {
        if let Some(shadow) = with_state(|s| s.shadow).flatten() {
            window::suppress_shadow();
            window::hide_shadow(shadow.raw());
        }
        return;
    }
    if throttle {
        let Ok(mut last) = last_shadow_sync().lock() else {
            return;
        };
        if last.elapsed() < std::time::Duration::from_millis(8) {
            return;
        }
        *last = Instant::now();
    } else if let Ok(mut last) = last_shadow_sync().lock() {
        *last = Instant::now();
    }
    let Some((shadow, preview)) = with_state(|s| s.shadow.map(|h| (h, s.preview))).flatten() else {
        return;
    };
    window::sync_shadow(shadow.raw(), preview.raw());
}

fn sync_shadow() {
    sync_shadow_with(false);
}

/// 当前预览的文件类型（没有预览或路径未知时 `None`）。
fn current_kind() -> Option<format::Kind> {
    with_state(|s| s.last_path.clone())
        .flatten()
        .map(|p| format::kind_for_path(&p))
}

/// 预览窗句柄（还没建过就是 `None`）。
fn preview_raw() -> Option<HWND> {
    with_state(|s| s.preview.raw())
}

/// 打开音频并**自动播放**（1.7.9）。
///
/// 打不开就打不开：只在顶栏给一句提示，卡片（时长/格式）照常显示，用户至少知道文件是什么。
/// 自动播放是有意的——预览工具"点到一个声音文件就该出声"（原版 QuickLook 也是这个行为）；
/// 想静音按 P 暂停，收起窗口自动停（[`stop_audio`]）。
fn start_audio(path: &Path) {
    stop_audio();
    let hwnd = preview_raw();
    let Some(player) = qlr_platform::audio::AudioPlayer::open(path) else {
        log_warn!(
            SCOPE,
            "AUDIO 打不开（缺解码器或不支持的容器）：{}",
            path.display()
        );
        if let Some(hwnd) = hwnd {
            show_toast(hwnd, "这个音频打不开（缺解码器）");
        }
        return;
    };
    let duration = player.duration_ms();
    let played = player.play();
    log_info!(
        SCOPE,
        "AUDIO 打开 {} · 时长 {}ms · 播放={}",
        path.display(),
        duration,
        played
    );
    with_state(|s| {
        s.audio = Some(player);
        s.audio_ticking = played;
    });
    match hwnd {
        Some(hwnd) if played => {
            window::start_timer(hwnd, TIMER_AUDIO, AUDIO_TICK_MS);
            paint_audio_now(hwnd);
        }
        Some(hwnd) => show_toast(hwnd, "这个音频播不了（缺解码器）"),
        None => {}
    }
}

/// 停声并释放 MCI 设备（收起窗口 / 换文件 / 退出都走这里）。
///
/// 必须真的 drop `AudioPlayer`：它的 `Drop` 会 `MCI_CLOSE`。不关的话设备被占住，
/// 再预览同一个文件会拿到同一个 alias，表现成"第二次预览没声音"。
fn stop_audio() {
    let (had, ticking, hwnd) =
        with_state(|s| (s.audio.take().is_some(), s.audio_ticking, s.preview.raw())).unwrap_or((
            false,
            false,
            HWND::default(),
        ));
    with_state(|s| s.audio_ticking = false);
    if ticking && !hwnd.0.is_null() {
        window::stop_timer(hwnd, TIMER_AUDIO);
    }
    if had {
        log_debug!(SCOPE, "AUDIO 停止并释放设备");
    }
}

/// 当前音频进度：`(已播 ms, 总 ms)`。没有音频预览时 `None`。
fn audio_progress() -> Option<(u64, u64)> {
    with_state(|s| s.audio.as_ref().map(|p| (p.position_ms(), p.duration_ms()))).flatten()
}

/// 播放 / 暂停（P 键）。返回是否真的切了（没有音频预览时 `false`）。
///
/// 用 `audio_ticking`（刷新计时器是否开着）当"正在播"的判据：计时器只在播放时开，
/// 暂停就杀掉——比每帧去问 MCI 状态便宜，也不会因为 `status` 报错而误判。
fn audio_toggle() -> bool {
    let Some(playing_now) = with_state(|s| s.audio.as_ref().map(|_| s.audio_ticking)).flatten()
    else {
        return false;
    };
    let ok = with_state(|s| {
        s.audio
            .as_ref()
            .map(|p| if playing_now { p.pause() } else { p.resume() })
    })
    .flatten()
    .unwrap_or(false);
    if !ok {
        return false;
    }
    let playing = !playing_now;
    with_state(|s| s.audio_ticking = playing);
    if let Some(hwnd) = preview_raw() {
        if playing {
            window::start_timer(hwnd, TIMER_AUDIO, AUDIO_TICK_MS);
        } else {
            window::stop_timer(hwnd, TIMER_AUDIO);
        }
        show_toast(hwnd, if playing { "播放" } else { "暂停" });
        paint_audio_now(hwnd);
    }
    log_info!(SCOPE, "AUDIO {}", if playing { "继续播放" } else { "暂停" });
    true
}

/// 快进 / 快退 `delta_ms`（左右键、滚轮）。到两头就夹住，不报错。
fn audio_seek(delta_ms: i64) -> bool {
    let Some((pos, dur)) = audio_progress() else {
        return false;
    };
    let target = (pos as i64 + delta_ms).clamp(0, dur.max(1) as i64) as u64;
    let ok = with_state(|s| s.audio.as_ref().map(|p| p.seek(target)))
        .flatten()
        .unwrap_or(false);
    if ok {
        log_info!(SCOPE, "AUDIO 定位 {}ms / {}ms", target, dur);
        if let Some(hwnd) = preview_raw() {
            paint_audio_now(hwnd);
        }
    }
    ok
}

/// 立刻重画进度条那一条（只刷这一块，不整窗重画——音频每 250ms 刷一次，整窗重画太贵）。
fn paint_audio_now(hwnd: HWND) {
    let Some((pos, dur)) = playback_progress() else {
        return;
    };
    let playing = playback_playing();
    let content = window::chrome_rects(window::client_rect(hwnd)).content;
    window::paint_audio_bar(hwnd, content, pos, dur, playing);
}

/// 打开视频并开始逐帧播放（1.7.9）。
///
/// 打不开就只给一句提示（和音频一致）：卡片/首帧照旧显示，用户至少知道文件是什么。
fn start_video(path: &Path) {
    stop_video();
    let hwnd = preview_raw();
    let Some(mut stream) = qlr_render::media::FrameStream::open(path) else {
        log_warn!(
            SCOPE,
            "VIDEO 打不开（缺解码器或不支持的容器）：{}",
            path.display()
        );
        if let Some(hwnd) = hwnd {
            show_toast(hwnd, "这个视频打不开（缺解码器）");
        }
        return;
    };
    let duration = stream.duration_ms();
    let Some(first) = stream.next_frame() else {
        if let Some(hwnd) = hwnd {
            show_toast(hwnd, "这个视频解不出画面");
        }
        return;
    };
    log_info!(
        SCOPE,
        "VIDEO 播放 {} · {}x{} · 时长 {}ms",
        path.display(),
        first.width,
        first.height,
        duration
    );
    with_state(|s| {
        s.video = Some(VideoPlay {
            stream,
            last: Some(first),
            anchor_pos_ms: 0,
            anchor_at: std::time::Instant::now(),
            playing: true,
            duration_ms: duration,
        });
    });
    if let Some(hwnd) = hwnd {
        video_paint(hwnd);
        window::start_timer(hwnd, TIMER_VIDEO, VIDEO_TICK_MS);
        paint_audio_now(hwnd);
    }
}

/// 停视频并放掉解码器（收起窗口 / 换文件 / 退出都走这里）。
fn stop_video() {
    let (had, hwnd) = with_state(|s| (s.video.take().is_some(), s.preview.raw()))
        .unwrap_or((false, HWND::default()));
    if had && !hwnd.0.is_null() {
        window::stop_timer(hwnd, TIMER_VIDEO);
    }
    if had {
        log_debug!(SCOPE, "VIDEO 停止并释放解码器");
    }
}

/// 画面播放到哪一毫秒了（暂停时就是暂停的那个点）。
fn video_position_ms() -> Option<u64> {
    with_state(|s| {
        s.video.as_ref().map(|v| {
            if v.playing {
                let elapsed = v.anchor_at.elapsed().as_millis() as u64;
                let pos = v.anchor_pos_ms.saturating_add(elapsed);
                // 别越过总时长（否则进度条会画到 100% 之外、"到头"判断也会抖）。
                if v.duration_ms > 0 {
                    pos.min(v.duration_ms)
                } else {
                    pos
                }
            } else {
                v.anchor_pos_ms
            }
        })
    })
    .flatten()
}

/// 把当前这一帧贴到内容区（等比居中）。
///
/// 直接在锁内画：`paint_bitmap_fit_window` 只做 GDI（GetDC/FillRect/StretchDIBits），
/// 不会回调进 state；反过来先把 720p 帧拷一份（3.6MB）每帧一次反而更贵。
fn video_paint(hwnd: HWND) {
    let content = window::chrome_rects(window::client_rect(hwnd)).content;
    // 只贴"画面区"：底部那条进度条不归帧管，否则每帧都会把它擦掉。
    let area = window::picture_area(content);
    let _ = with_state(|s| {
        if let Some(v) = s.video.as_ref()
            && let Some(f) = v.last.as_ref()
        {
            window::paint_bitmap_fit_window(hwnd, area, f.width, f.height, &f.bgra);
        }
    });
}

/// 取帧定时器：追上"此刻应该显示的那一帧"，落后就跳帧（宁可丢帧也不放慢）。
fn video_tick(hwnd: HWND) {
    let Some(target) = video_position_ms() else {
        window::stop_timer(hwnd, TIMER_VIDEO);
        return;
    };
    let mut ended = false;
    let mut painted = false;
    let _ = with_state(|s| {
        let Some(v) = s.video.as_mut() else {
            return;
        };
        if !v.playing {
            return;
        }
        loop {
            let Some(frame) = v.stream.next_frame() else {
                ended = true;
                break;
            };
            let ts = frame.ts_ms;
            v.last = Some(frame);
            painted = true;
            // 解到的帧已经追上目标时刻就不再往下解（剩下的时间留给上屏）。
            if ts + VIDEO_TICK_MS as u64 >= target {
                break;
            }
        }
    });
    if ended {
        let dur = with_state(|s| s.video.as_ref().map(|v| v.duration_ms))
            .flatten()
            .unwrap_or(0);
        with_state(|s| {
            if let Some(v) = s.video.as_mut() {
                v.playing = false;
                // 停在末尾：进度条显示满格，再按 P 就从头重放（`video_toggle` 里判断）。
                v.anchor_pos_ms = dur;
            }
        });
        window::stop_timer(hwnd, TIMER_VIDEO);
        let content = window::chrome_rects(window::client_rect(hwnd)).content;
        window::paint_audio_bar(hwnd, content, dur, dur, false);
        show_toast(hwnd, "播放结束");
        log_debug!(SCOPE, "VIDEO 播放结束（{}ms）", dur);
        return;
    }
    if painted {
        video_paint(hwnd);
        paint_audio_now(hwnd);
    }
}

/// 视频播放 / 暂停（P 键）。停在末尾时再按 = 从头重放。
fn video_toggle() -> bool {
    let hwnd = preview_raw();
    let Some(now_playing) = with_state(|s| s.video.as_ref().map(|v| v.playing)).flatten() else {
        return false;
    };
    let pos = video_position_ms().unwrap_or(0);
    let ok = with_state(|s| {
        let Some(v) = s.video.as_mut() else {
            return false;
        };
        if !now_playing && v.duration_ms > 0 && pos + 200 >= v.duration_ms {
            // 已经到末尾了：再按 P 就是"重放"。
            let _ = v.stream.seek(0);
            v.last = v.stream.next_frame();
            v.anchor_pos_ms = 0;
        } else {
            v.anchor_pos_ms = pos;
        }
        v.anchor_at = std::time::Instant::now();
        v.playing = !now_playing;
        true
    })
    .unwrap_or(false);
    if !ok {
        return false;
    }
    if let Some(hwnd) = hwnd {
        if now_playing {
            window::stop_timer(hwnd, TIMER_VIDEO);
        } else {
            window::start_timer(hwnd, TIMER_VIDEO, VIDEO_TICK_MS);
            video_paint(hwnd);
        }
        show_toast(hwnd, if now_playing { "暂停" } else { "播放" });
        paint_audio_now(hwnd);
    }
    log_info!(
        SCOPE,
        "VIDEO {}",
        if now_playing {
            "暂停"
        } else {
            "继续播放"
        }
    );
    true
}

/// 视频快进 / 快退 `delta_ms`（左右键、滚轮）。到两头就夹住。
fn video_seek(delta_ms: i64) -> bool {
    let base = video_position_ms();
    let (Some(base), Some(dur)) = (
        base,
        with_state(|s| s.video.as_ref().map(|v| v.duration_ms)).flatten(),
    ) else {
        return false;
    };
    let target = (base as i64 + delta_ms).clamp(0, dur.max(1) as i64) as u64;
    let ok = with_state(|s| {
        let Some(v) = s.video.as_mut() else {
            return false;
        };
        if !v.stream.seek(target) {
            return false;
        }
        // 跳完立刻解一帧：不然画面还停在原处，看起来像"没生效"。
        v.last = v.stream.next_frame();
        v.anchor_pos_ms = target;
        v.anchor_at = std::time::Instant::now();
        true
    })
    .unwrap_or(false);
    if ok {
        log_info!(SCOPE, "VIDEO 定位 {}ms / {}ms", target, dur);
        if let Some(hwnd) = preview_raw() {
            video_paint(hwnd);
            paint_audio_now(hwnd);
        }
    }
    ok
}

/// 当前有没有"会动"的预览（音频或视频）：滚轮/左右键的语义就跟着它变。
///
/// 注意判据是"有没有这种东西"，不是"正在不在播"——暂停中的视频也归它管，
/// 否则暂停时滚轮会掉进下面的缩放/翻页分支。
fn playback_active() -> bool {
    with_state(|s| s.visible && (s.audio.is_some() || s.video.is_some())).unwrap_or(false)
}

/// 播放/暂停（P 键），音频与视频共用。
fn playback_toggle() -> bool {
    if with_state(|s| s.audio.is_some()).unwrap_or(false) {
        audio_toggle()
    } else {
        video_toggle()
    }
}

/// 快进/快退（左右键、滚轮），音频与视频共用。
fn playback_seek(delta_ms: i64) -> bool {
    if with_state(|s| s.audio.is_some()).unwrap_or(false) {
        audio_seek(delta_ms)
    } else {
        video_seek(delta_ms)
    }
}

/// 进度条要显示的 `(已播 ms, 总 ms)`：音频优先，其次视频。
fn playback_progress() -> Option<(u64, u64)> {
    if let Some(p) = audio_progress() {
        return Some(p);
    }
    let pos = video_position_ms()?;
    let dur = with_state(|s| s.video.as_ref().map(|v| v.duration_ms)).flatten()?;
    Some((pos, dur))
}

/// 进度条上该显示"播放中"还是"已暂停"。
fn playback_playing() -> bool {
    if with_state(|s| s.audio.is_some()).unwrap_or(false) {
        return with_state(|s| s.audio_ticking).unwrap_or(false);
    }
    with_state(|s| s.video.as_ref().map(|v| v.playing))
        .flatten()
        .unwrap_or(false)
}

fn hide_preview() {
    // 压缩包视图只在"当前这次预览"里有效（1.7.3）：收起就放掉，几千条目的树不该常驻。
    clear_archive_view();
    // 音频（1.7.9）：收起必须停声。`AudioPlayer` 的 drop 会 `MCI_CLOSE`，
    // 不关的话 MCI 设备一直被占着——再次预览同一个文件会拿到同一个 alias，
    // 表现成"第二次预览没声音"。
    stop_audio();
    // 视频同理：放掉 MF 的 SourceReader，否则下一次预览同一文件会多占一个解码器。
    stop_video();
    // 系统预览处理器要先卸掉，否则它会把自己的子窗口留在屏幕上（1.4.1）。
    // 顺序很关键：先取走句柄、**等窗口隐藏之后**再真正 Unload（见 SHELL_TRASH 注释）。
    let old_shell = with_state(|s| s.shell_preview.take()).flatten();
    // 换帧期间"盖住"的旧处理器也一起放掉（1.7.6）：收回时不卸，下次显示会带着僵尸子窗口。
    let old_cover = with_state(|s| s.shell_cover.take()).flatten();
    // 位置记忆（1.0.13）：收起之前先记一次——窗口一旦隐藏，`GetWindowRect` 还可能有效，
    // 但"visible"已经是 false，`remember_position` 会直接返回，所以必须抢在前面。
    remember_position();
    // 锁内**只改状态、只取句柄**，`window::hide` 放到锁外。
    //
    // 为什么必须这样（结构评审 #1，与历史 #128 死锁同型）：`window::hide` 内部是
    // `ShowWindow(SW_HIDE)`，它会**同步**把 WM_WINDOWPOSCHANGED / WM_PAINT 派发回
    // 我们自己的 wndproc，而 `repaint` 那条路要再拿一次 STATE 锁。此时本线程正持着
    // 锁 → 重入等待，整窗假死（空格关不掉、托盘退出卡住）。
    let Some((preview, shadow, elapsed)) = with_state(|s| {
        s.visible = false;
        // 收起之后不再跟着选中项跑（否则看不见的窗口还在后台不停重光栅化）。
        s.pinned = false;
        // 1.7.4：**拖边/拖动的中间状态必须一起清掉**。
        // 以前这里不清，若恰好在拖动/改大小的过程中被收起（空格、`--hide`、托盘），
        // capture 与 `resize_from` 会留在状态里：下一次显示后第一条鼠标移动
        // 会拿"上一次的原始矩形 + 这一次的位移"把窗口瞬间跳到旧尺寸（用户看到的
        // 是"窗口自己跳回去了"）。同时 PPT 的翻页计数与提示也要跟本次预览一起失效。
        s.resize_edges = None;
        s.resize_from = None;
        s.media_aspect = None;
        s.drag_from = None;
        s.dragged = false;
        s.slide = None;
        s.toast = None;
        s.shell_input_pending = false;
        s.hold_frame = false;
        // 1.8.3（T03）：页码输入状态必须跟预览一起收起。
        // 以前这里不清：点开页码框输到一半按空格/`--hide` 收起窗口，`page_edit` 留在
        // 状态里、`CAPTURE_DIGITS` 还开着 → 全局钩子继续吞数字/退格/回车，用户在**别的
        // 程序**里打不出数字（这个"谁吞了我的按键"极难自查）。`finish_page_edit(false)`
        // 会同时把 `page_edit` 清掉并 `set_capture_digits(false)`。
        s.page_edit = None;
        (s.preview, s.shadow, s.request_started.elapsed().as_millis())
    }) else {
        return;
    };
    // 锁已经放掉了，这时候才真正隐藏窗口（见上面那段注释）。
    window::hide(preview.raw());
    if let Some(shadow) = shadow {
        window::hide_shadow(shadow.raw());
    }
    // 数字输入模式的开关是**全局**的，所以放在锁外关（与 capture 一起收拾临时状态）。
    // 记一行日志：smoke 脚本可以按稳定字符串断言"收起之后确实释放了"。
    if hotkey::capture_digits() {
        hotkey::set_capture_digits(false);
        log_info!(SCOPE, "收起预览：数字输入模式已释放（capture=false）");
    }
    // capture 也要放掉：它挂在窗口上，隐藏窗口持有 capture 会吞掉后续鼠标消息。
    window::release_capture();
    window::allow_shadow();
    unsafe {
        let _ = KillTimer(Some(preview.raw()), TIMER_TOAST);
        let _ = KillTimer(Some(preview.raw()), TIMER_SHELL_INPUT);
        let _ = KillTimer(Some(preview.raw()), TIMER_SHELL_SWAP);
    }
    // 计时器不在这里停（1.1.0）：窗口收着的时候它转去做投机预渲染。
    // 位置记忆（1.0.13）：收起时把位置落盘，下次打开直接用。
    persist_position();
    retire_shell_preview(old_shell);
    retire_shell_preview(old_cover);
    let _ = preview;
    hotkey::set_wheel_pass_through(false);
    hotkey::set_active(false);
    if let Some(preview) = with_state(|s| s.preview) {
        window::set_activatable(preview.raw(), false);
    }
    // GDI/USER 计数：现场报障「每预览一个文件 GDI +1 且收起不释放」，这两行是二分的地基。
    // 正常应该每次回到同一个值；连续 10 次稳定 +1 就是泄漏。
    let (gdi, user) = window::gui_resources();
    log_info!(SCOPE, "HIDE elapsed={elapsed}ms GDI={gdi} USER={user}");
    *last_hide_slot() = Some(Instant::now());
}

static LAST_HIDE: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

/// 上一次"收起预览"的时刻（见 `LAST_HIDE`）。
fn last_hide_slot() -> MutexGuard<'static, Option<Instant>> {
    LAST_HIDE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn quit() {
    log_info!(SCOPE, "收到退出请求");
    // 位置记忆（1.0.13）：托盘退出时窗口可能还开着，退出前补一次落盘。
    persist_position();
    unsafe { PostQuitMessage(0) };
}

fn build_lines(target: Option<&Path>) -> Vec<String> {
    let zoom = with_state(|s| s.zoom).unwrap_or(1.0);
    let mut lines = vec![
        version::banner(),
        "内置支持：图片 · PDF · 文本 · Markdown（其余格式以插件形式扩展）".to_string(),
    ];
    match target {
        Some(p) => {
            lines.push(format!("文件：{}", p.display()));
            lines.push(format!(
                "缩放：{:.0}%（+ / - 缩放，上下方向键平移，PageDown 翻页）",
                zoom * 100.0
            ));
            match std::fs::metadata(p) {
                Ok(meta) => lines.push(format!(
                    "大小：{} · 类型：{} ({})",
                    human_size(meta.len()),
                    extension_of(p),
                    format::kind_for_path(p)
                )),
                Err(e) => lines.push(format!("无法读取文件信息：{e}")),
            }
            // 1.0.10：不支持的文件不再画占位（`show_preview` 直接响铃收起），
            // 所以这里只剩"插件接管了"这一种非内置情形。
            if qlr_render::viewer_for(format::kind_for_path(p)).is_none()
                && let Some(plugin) = plugins::host().find_for_path(p)
            {
                lines.push(format!(
                    "插件：{}（接管 .{}）",
                    plugin.name(),
                    extension_of(p).trim_start_matches('.')
                ));
                lines.push(format!("插件文件：{}", plugin.file().display()));
            }
        }
        None => lines.push("没有目标文件（试试 qlrust <文件路径>）".to_string()),
    }
    lines.push(String::new());
    lines.push("托盘右键菜单可退出。".to_string());
    lines
}

fn extension_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_else(|| "（无扩展名）".to_string())
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// 渲染层的语义样式 → 平台层的像素样式。
///
/// 两边故意各有一份枚举：渲染层不依赖 windows，平台层不依赖渲染实现。
/// 中间这条转换是唯一耦合点，加新样式时**两边都要改**，编译器会提醒（match 是穷尽的）。
fn to_text_style(style: qlr_render::LineStyle) -> window::TextStyle {
    match style {
        qlr_render::LineStyle::Normal => window::TextStyle::Normal,
        qlr_render::LineStyle::Heading1 => window::TextStyle::Heading1,
        qlr_render::LineStyle::Heading2 => window::TextStyle::Heading2,
        qlr_render::LineStyle::Heading3 => window::TextStyle::Heading3,
        qlr_render::LineStyle::Code => window::TextStyle::Code,
        qlr_render::LineStyle::Quote => window::TextStyle::Quote,
        qlr_render::LineStyle::List => window::TextStyle::List,
        qlr_render::LineStyle::Rule => window::TextStyle::Rule,
        qlr_render::LineStyle::Table => window::TextStyle::Table,
    }
}

/// Markdown 块级底色（1.0.8）：代码块 / 引用块给一条浅灰底，视觉上和正文分开。
///
/// `COLORREF` 是 `0x00BBGGRR`（CONVENTIONS #41）：`#F6F8FA` → `0x00FA_F8F6`。
fn line_bg(style: qlr_render::LineStyle) -> Option<u32> {
    match style {
        qlr_render::LineStyle::Code => Some(0x00FA_F8F6),
        qlr_render::LineStyle::Quote => Some(0x00F7_F7F7),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 顶栏 / 内容区布局（1.0.9：四种格式统一）
// ---------------------------------------------------------------------------

/// 顶栏标题：文件名（不带路径）。
fn file_title() -> String {
    with_state(|s| {
        s.last_path
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().to_string())
    })
    .flatten()
    .unwrap_or_else(|| "QLRust".to_string())
}

/// 顶栏一次性提示（1.7.4）：写进状态并起一个 1.2s 的定时器擦掉。
///
/// 用在"翻页到边界"这种"用户按了但内容不会再动"的场合——不给反馈的话，
/// 用户会以为按键没生效（PPT 以前就是按到底直接切到 PowerPoint 自己的黑屏结束页）。
fn show_toast(hwnd: HWND, text: impl Into<String>) {
    show_toast_ms(hwnd, text, 1200);
}

/// 带时长的提示：[`show_toast`] 的通用版本。
///
/// 翻页到边界那种"按了但内容不再动"的瞬时反馈 1.2 s 够；而"托盘图标没加上、
/// 程序还在后台跑"这种需要读两遍的信息给 5 s，否则用户只看到一闪。
fn show_toast_ms(hwnd: HWND, text: impl Into<String>, ms: u64) {
    let until = Instant::now() + std::time::Duration::from_millis(ms);
    with_state(|s| s.toast = Some((text.into(), until)));
    unsafe {
        let _ = SetTimer(Some(hwnd), TIMER_TOAST, ms as u32, None);
    }
    window::invalidate(hwnd);
}

/// 托盘注册失败后的第一次"可见提示"（1.8.3）。
///
/// 必须在**窗口已经显示出来之后**调用：toast 画在顶栏上，窗口没显示时写进去
/// 谁也看不见，而且定时器一过就被擦掉了。所以 `show_preview` 在每次成功
/// `show_at` 之后都问一次这里，标记被取走之后就不再打扰。
fn notify_tray_failure_once(hwnd: HWND) {
    if !TRAY_FAILURE_NOTICE.swap(false, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    log_warn!(
        SCOPE,
        "托盘图标没加上（见启动日志）：已在预览窗顶栏提示一次，退出用 qlrust.exe --quit"
    );
    show_toast_ms(
        hwnd,
        "托盘图标没加上：后台常驻，qlrust.exe --quit 可退出",
        5000,
    );
}

/// 当前还该显示的那条提示（过期的不返回）。
fn current_toast() -> Option<String> {
    with_state(|s| {
        s.toast
            .as_ref()
            .filter(|(_, until)| Instant::now() < *until)
            .map(|(text, _)| text.clone())
    })
    .flatten()
}

/// 当前内容的滚动位置（0.0–1.0）。文本按行、PDF 按页；放得下就返回 `None`。
///
/// 注意锁的顺序：先取内容（`CONTENT`）、再取状态（`STATE`），**不要**在持有
/// `STATE` 的时候去碰 `CONTENT`——`repaint` 是在 `WM_PAINT` 里跑的，那条路径
/// 已经证明过会重入。
fn scroll_info() -> Option<window::ScrollInfo> {
    let content = get_content()?;
    let (page, pages, scroll, consumed) =
        with_state(|s| (s.page, s.pages, s.scroll, s.wrapped_consumed)).unwrap_or((0, None, 0, 1));
    // PDF：滚动条只看**页数**，和"现在上屏的是占位文字还是真页面"无关。
    // 这一条必须放在 match 之前：`view_size()` 要靠它算出"内容区宽不宽"，
    // 而算渲染尺寸时上屏的往往还是那句"正在渲染…"——按占位文字算出来的
    // "没有滚动条"会让位图按全宽渲染，右边正好被滚动条压掉一条。
    if let Some(total) = pages {
        return window::ScrollInfo::new(page as f32, 1.0, total as f32);
    }
    match &*content {
        Canvas::Text(doc) => {
            let total = doc.lines.len() as f32;
            window::ScrollInfo::new(scroll as f32, consumed.max(1) as f32, total)
        }
        Canvas::Bitmap(_) => {
            let total = pages? as f32;
            window::ScrollInfo::new(page as f32, 1.0, total)
        }
    }
}

/// 内容区布局。绘制与命中测试都从这里取，保证"看到的位置"和"点到的位置"一致。
struct Frame {
    /// 扣掉顶栏后的区域（目录栏 / 滚动条都从它里面切）。
    below_chrome: RECT,
    /// 真正排版内容的区域。
    content: RECT,
    /// 目录栏（Markdown 且窗口够宽才有）。
    toc: Option<RECT>,
    /// 滚动条（内容放不下才有）。
    bar: Option<RECT>,
}

fn frame(hwnd: HWND) -> Frame {
    let client = window::client_rect(hwnd);
    let below = window::chrome_rects(client).content;
    let has_toc = with_state(|s| !s.toc.is_empty()).unwrap_or(false);
    let mut content = below;
    // 窗口太窄就不显示目录：正文被挤到 300 px 还不如没有目录。
    let toc = if has_toc && (below.right - below.left) > 520 {
        content.left += window::toc_width();
        Some(window::toc_rect(below))
    } else {
        None
    };
    // 压缩包列表（1.7.3）：内容单位是"展平后的行"，和文本/PDF 的 `scroll_info()` 不是一套，
    // 所以这里单独问一次；命中测试和绘制都读同一个 `Frame`，两边才不会错位。
    let bar = if has_archive_view() {
        if archive_scroll_info(below).is_some() {
            content.right -= window::scrollbar_w();
            Some(window::scrollbar_rect(below))
        } else {
            None
        }
    } else if scroll_info().is_some() {
        content.right -= window::scrollbar_w();
        Some(window::scrollbar_rect(below))
    } else {
        None
    };
    Frame {
        below_chrome: below,
        content,
        toc,
        bar,
    }
}

/// 跳到第 `page` 页（1 基，越界会被夹住）。
fn goto_page(page: usize) {
    let (preview, path, pages) = match with_state(|s| (s.preview, s.last_path.clone(), s.pages)) {
        Some(triple) => triple,
        None => return,
    };
    let (Some(path), Some(total)) = (path, pages) else {
        return;
    };
    let target = page.clamp(1, total);
    let changing = with_state(|s| {
        let changed = s.page + 1 != target;
        s.page = target - 1;
        s.pan = (0, 0);
        s.zoom_preview = false;
        changed
    })
    .unwrap_or(false);
    if !changing {
        return;
    }
    let (width, height) = view_size(preview.raw());
    request_render(&path, width, height, target - 1);
    window::invalidate(preview.raw());
    log_info!(SCOPE, "PAGE {}/{}（跳转）", target, total);
}

/// 用系统默认程序打开当前文件（顶栏的「在其他应用打开」）。
fn open_external() {
    let path = with_state(|s| s.last_path.clone()).flatten();
    let Some(path) = path else { return };
    let shown = path.to_string_lossy().to_string();
    let ok = window::open_external(&shown);
    log_info!(SCOPE, "在其他应用打开：{shown} ok={ok}");
}

/// 退出页码编辑模式（`submit` 为真时先按输入内容跳页）。
fn finish_page_edit(submit: bool) {
    let text = with_state(|s| s.page_edit.take()).flatten();
    hotkey::set_capture_digits(false);
    let Some(text) = text else { return };
    if submit && let Ok(page) = text.trim().parse::<usize>() {
        goto_page(page);
    }
    if let Some(preview) = with_state(|s| s.preview) {
        window::invalidate(preview.raw());
    }
}

/// 页码输入模式下的按键（数字/退格/回车/Esc）。返回 `true` 表示这个键被吃掉了。
///
/// 为什么要单独处理：预览窗是 `WS_EX_NOACTIVATE`，永远拿不到键盘焦点，
/// 数字键是 `WH_KEYBOARD_LL` 钩子转进来的（`hotkey::set_capture_digits`）。
fn page_edit_key(vk: usize) -> bool {
    let digit = match vk as u32 {
        0x30..=0x39 => Some((vk as u32 - 0x30) as u8),
        0x60..=0x69 => Some((vk as u32 - 0x60) as u8),
        _ => None,
    };
    if let Some(digit) = digit {
        with_state(|s| {
            if let Some(text) = s.page_edit.as_mut()
                && text.len() < 6
            {
                text.push(char::from(b'0' + digit));
            }
        });
    } else {
        match vk as u32 {
            0x08 => {
                with_state(|s| {
                    if let Some(text) = s.page_edit.as_mut() {
                        text.pop();
                    }
                });
            }
            0x0D => {
                finish_page_edit(true);
                return true;
            }
            0x1B => {
                finish_page_edit(false);
                return true;
            }
            _ => return false,
        }
    }
    if let Some(preview) = with_state(|s| s.preview) {
        window::invalidate(preview.raw());
    }
    true
}

/// 顶栏/目录/滚动条的点击。返回 `true` 表示这次点击已被消费（不要再去拖窗口）。
fn on_chrome_click(hwnd: HWND, lparam: LPARAM) -> bool {
    let (x, y) = client_xy(lparam);
    let client = window::client_rect(hwnd);
    let rects = window::chrome_rects(client);

    if y < rects.content.top {
        // ---- 顶栏 ----
        if point_in(x, y, rects.close) {
            // 现场报障："md 有时候第一次点 × 没反应"。这一行留在 info：
            // 有它就说明点击**确实到了**关闭按钮（窗口却还在 = 收起后又被谁 SHOW 回来，
            // 见 `show_preview` 里那条 HIDE→SHOW 追踪）；没有它就要往别处查
            // （命中区不对 / 被非客户区当成改大小吞掉）。
            log_info!(SCOPE, "顶栏：关闭（{x},{y}）");
            hide_preview();
            return true;
        }
        // 挨着 × 却没点中：记一条，专门用来抓"差几像素"的命中问题。
        // 顶栏 + 内容区都没命中时才会走到，不会刷屏（只有真的贴边点才触发）。
        let near_close = x >= rects.close.left - 6
            && x < rects.close.right + 6
            && y >= rects.close.top - 6
            && y < rects.close.bottom + 6;
        if near_close {
            log_info!(
                SCOPE,
                "顶栏：点击 ({x},{y}) 贴着关闭按钮但没命中，close=({},{})-({},{})",
                rects.close.left,
                rects.close.top,
                rects.close.right,
                rects.close.bottom
            );
        }
        if point_in(x, y, rects.external) {
            open_external();
            return true;
        }
        if point_in(x, y, rects.pin) {
            // 单向锁：点一次就开，**再点也不关**（用户原话"不能取消阻止关闭"）。
            // 防的就是"手忙脚乱时随手又点一下把锁关了"。
            let already = with_state(|s| s.pinned).unwrap_or(false);
            if already {
                log_debug!(SCOPE, "顶栏：阻止关闭已开启（不能取消）");
                window::beep();
            } else {
                with_state(|s| s.pinned = true);
                log_info!(SCOPE, "已开启「阻止关闭」：空格/Esc 关不掉这个窗口");
            }
            window::invalidate(hwnd);
            return true;
        }
        let has_page = with_state(|s| s.pages.is_some()).unwrap_or(false);
        if has_page && point_in(x, y, rects.page) {
            // 点页码框 = 开始输入（窗口不激活，输入靠键盘钩子转发数字键）。
            with_state(|s| s.page_edit = Some(String::new()));
            hotkey::set_capture_digits(true);
            log_debug!(SCOPE, "页码输入：开始");
            window::invalidate(hwnd);
            return true;
        }
        // 点顶栏空白处（1.0.10）：结束页码输入，并且**像正文一样拖窗口**。
        // 用户要求"鼠标左键拖拽窗口的作用域增加到顶栏上"——以前顶栏是"死"的，
        // 想挪窗口只能抓内容区，而图片/PDF 撑满时内容区反而不好抓。
        finish_page_edit(false);
        on_drag_start(hwnd, lparam);
        return true;
    }

    let layout = frame(hwnd);
    if let Some(bar) = layout.bar
        && point_in(x, y, bar)
    {
        with_state(|s| s.scroll_drag = true);
        window::set_capture(hwnd);
        apply_scroll_y(hwnd, y);
        return true;
    }
    if let Some(toc) = layout.toc
        && point_in(x, y, toc)
    {
        let index = window::toc_item_at(
            layout.below_chrome,
            y,
            with_state(|s| s.toc_scroll).unwrap_or(0),
        );
        if let Some(index) = index {
            let target = with_state(|s| s.toc.get(index).map(|item| item.2)).flatten();
            if let Some(line) = target {
                with_state(|s| {
                    s.scroll = line;
                    s.toc_active = Some(index);
                });
                window::invalidate(hwnd);
                log_debug!(SCOPE, "目录跳转：第 {} 行", line + 1);
            }
        }
        return true;
    }
    false
}

/// 点/拖滚动条：把鼠标 y 换成"滚到哪儿"。
fn apply_scroll_y(hwnd: HWND, y: i32) {
    let layout = frame(hwnd);
    let (Some(_bar), Some(info)) = (layout.bar, scroll_info()) else {
        return;
    };
    let ratio = window::scroll_start_at(layout.below_chrome, y, info);
    let is_pdf = with_state(|s| s.pages.is_some()).unwrap_or(false);
    if is_pdf {
        let total = with_state(|s| s.pages).flatten().unwrap_or(1);
        let page = (ratio * (total.saturating_sub(1)) as f32).round() as usize + 1;
        goto_page(page);
    } else {
        let total = text_total_lines();
        let visible = with_state(|s| s.wrapped_consumed).unwrap_or(1).max(1);
        let max = total.saturating_sub(visible);
        let scroll = (ratio * max as f32).round() as usize;
        with_state(|s| s.scroll = scroll.min(max));
        window::invalidate(hwnd);
    }
}

/// 鼠标是否落在某个矩形里。
fn point_in(x: i32, y: i32, rect: RECT) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

/// 鼠标移动时更新"悬停在哪"（顶栏按钮 / 滚动条 / 目录），变化了才重绘。
fn update_hover(hwnd: HWND, lparam: LPARAM) -> bool {
    let (x, y) = client_xy(lparam);
    let client = window::client_rect(hwnd);
    let rects = window::chrome_rects(client);
    let button = if y < rects.content.top {
        if point_in(x, y, rects.close) {
            Some(window::ChromeButton::Close)
        } else if point_in(x, y, rects.external) {
            Some(window::ChromeButton::External)
        } else if point_in(x, y, rects.pin) {
            Some(window::ChromeButton::Pin)
        } else {
            None
        }
    } else {
        None
    };
    let layout = frame(hwnd);
    let toc_active = layout.toc.filter(|toc| point_in(x, y, *toc)).and_then(|_| {
        window::toc_item_at(
            layout.below_chrome,
            y,
            with_state(|s| s.toc_scroll).unwrap_or(0),
        )
    });
    let hover_toc = layout.toc.is_some_and(|toc| point_in(x, y, toc));
    let changed = with_state(|s| {
        let changed =
            s.hover_button != button || s.toc_active != toc_active || s.hover_toc != hover_toc;
        s.hover_button = button;
        s.toc_active = toc_active;
        // 滚轮落点：悬停在目录栏上就滚目录，否则滚正文（1.8.3）。
        s.hover_toc = hover_toc;
        changed
    })
    .unwrap_or(false);
    if changed {
        window::invalidate(hwnd);
    }
    changed
}

// ---------------------------------------------------------------- 压缩包列表视图（1.7.3）

/// 目录树里的一个节点（目录或文件）。兄弟顺序在打开时就排好，绘制时只做一次整型 DFS。
struct ArchiveNode {
    /// 显示名（目录不带 `/`，绘制时才补）。
    name: String,
    /// 是否是目录。
    is_dir: bool,
    /// 解压后字节数（目录恒 0）。
    size: u64,
    /// 修改时间（取不到是空串）。
    time: String,
    /// 是否展开（只有目录有意义）。
    expanded: bool,
    /// 子节点下标（打开时就排好序：目录在前，同类按名字小写排序）。
    children: Vec<usize>,
}

/// 内置压缩包视图：整棵目录树 + 滚动行号 + 选中节点。
///
/// 为什么打开时就建树：滚动/折叠/悬停每次都要重新展平，边展平边拼字符串会把大包拖成
/// 每帧毫秒级；预建树之后展平只是一次整型 DFS（几千条目也只是一趟数组遍历）。
struct ArchiveView {
    path: std::path::PathBuf,
    nodes: Vec<ArchiveNode>,
    /// 根的直接子节点。
    roots: Vec<usize>,
    /// 第一行对应的行号（滚动偏移按"行"算）。
    scroll: usize,
    /// 选中的**节点下标**（不是行号：折叠/展开后行号会变，节点不会）。
    selected: Option<usize>,
}

static ARCHIVE: OnceLock<Mutex<Option<ArchiveView>>> = OnceLock::new();

fn archive_slot() -> &'static Mutex<Option<ArchiveView>> {
    ARCHIVE.get_or_init(|| Mutex::new(None))
}

fn is_archive(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "zip" | "cbz" | "jar" | "war"
    )
}

/// 打开压缩包并建目录树（默认全展开）。失败返回 false，调用方退回文本/插件通道。
fn open_archive_view(path: &Path) -> bool {
    let Ok(parsed) = qlr_render::archive::open(path) else {
        log_warn!(SCOPE, "压缩包解析失败：{}", path.display());
        return false;
    };
    let (nodes, roots) = build_archive_tree(&parsed.entries);
    log_info!(
        SCOPE,
        "ARCHIVE {} 个文件夹 · {} 个文件 · 解压后 {}（目录树 {} 个节点）",
        parsed.dirs,
        parsed.files,
        human_size(parsed.uncompressed),
        nodes.len()
    );
    if let Ok(mut slot) = archive_slot().lock() {
        *slot = Some(ArchiveView {
            path: path.to_path_buf(),
            nodes,
            roots,
            scroll: 0,
            selected: None,
        });
    }
    true
}

/// 把中央目录的线性条目表搭成树（1.7.3）。
///
/// 两件事必须在这里做掉，绘制路径就不必再管：
/// 1. **补目录**：很多打包器（含 `python zipfile`、`Compress-Archive` 的部分路径）不写
///    目录条目，只靠条目的 `/` 前缀推断，否则树是散的；
/// 2. **排序**：目录在前、同类按名字小写排，跟资源管理器的习惯一致。
fn build_archive_tree(entries: &[qlr_render::archive::Entry]) -> (Vec<ArchiveNode>, Vec<usize>) {
    let mut nodes: Vec<ArchiveNode> = Vec::new();
    let mut roots: Vec<usize> = Vec::new();
    let mut dirs: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    // 先建目录：这样 "src/" 一定排在 "src/main.rs" 前面，不依赖条目顺序。
    for entry in entries.iter().filter(|entry| entry.is_dir) {
        let path = entry.path.trim_end_matches('/');
        if !path.is_empty() {
            ensure_archive_dir(path, &entry.time, &mut nodes, &mut roots, &mut dirs);
        }
    }
    for entry in entries.iter().filter(|entry| !entry.is_dir) {
        let path = entry.path.trim_end_matches('/');
        if path.is_empty() {
            continue;
        }
        let id = nodes.len();
        nodes.push(ArchiveNode {
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            is_dir: false,
            size: entry.size,
            time: entry.time.clone(),
            expanded: false,
            children: Vec::new(),
        });
        match path.rsplit_once('/') {
            Some((parent, _)) if !parent.is_empty() => {
                match ensure_archive_dir(parent, "", &mut nodes, &mut roots, &mut dirs) {
                    Some(pid) => nodes[pid].children.push(id),
                    None => roots.push(id),
                }
            }
            _ => roots.push(id),
        }
    }
    sort_archive_children(&mut nodes);
    sort_archive_ids(&mut roots, &nodes);
    (nodes, roots)
}

/// 确保目录 `full` 在树里，返回它的节点下标（父目录链递归补齐）。
///
/// `time` 只在"由目录条目本身建出来"时有值；补齐出来的中间目录没有时间戳。
///
/// 防御：`full` 末尾的 `/` 一律先去掉——条目表里目录路径本应已经去掉（`archive.rs` 的
/// `clean`），但这里再兜一次：`"a/"` 直接喂进来会 `rsplit_once('/')` 成 (`"a"`, `""`)，
/// 于是凭空多出一个名字为空的目录行（1.7.3 单测逮到过）。
fn ensure_archive_dir(
    full: &str,
    time: &str,
    nodes: &mut Vec<ArchiveNode>,
    roots: &mut Vec<usize>,
    dirs: &mut std::collections::HashMap<String, usize>,
) -> Option<usize> {
    let full = full.trim_end_matches('/');
    if full.is_empty() {
        return None;
    }
    if let Some(&id) = dirs.get(full) {
        return Some(id);
    }
    let (parent, name) = match full.rsplit_once('/') {
        Some((parent, name)) if !parent.is_empty() => (Some(parent), name),
        Some((_, name)) => (None, name),
        None => (None, full),
    };
    let id = nodes.len();
    nodes.push(ArchiveNode {
        name: name.to_string(),
        is_dir: true,
        size: 0,
        time: time.to_string(),
        expanded: true,
        children: Vec::new(),
    });
    dirs.insert(full.to_string(), id);
    match parent {
        Some(parent) => {
            if let Some(pid) = ensure_archive_dir(parent, "", nodes, roots, dirs) {
                nodes[pid].children.push(id);
            } else {
                roots.push(id);
            }
        }
        None => roots.push(id),
    }
    Some(id)
}

/// 目录在前，同类按名字小写排（大小写只用来打破平手，保证顺序稳定）。
fn cmp_archive_nodes(a: &ArchiveNode, b: &ArchiveNode) -> std::cmp::Ordering {
    b.is_dir
        .cmp(&a.is_dir)
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        .then_with(|| a.name.cmp(&b.name))
}

fn sort_archive_ids(ids: &mut [usize], nodes: &[ArchiveNode]) {
    ids.sort_by(|a, b| cmp_archive_nodes(&nodes[*a], &nodes[*b]));
}

fn sort_archive_children(nodes: &mut [ArchiveNode]) {
    for i in 0..nodes.len() {
        // 先把子表搬出来再排：否则同时要 `&mut nodes[i]` 和 `&nodes[..]`。
        let mut kids = std::mem::take(&mut nodes[i].children);
        kids.sort_by(|a, b| cmp_archive_nodes(&nodes[*a], &nodes[*b]));
        nodes[i].children = kids;
    }
}

/// 按展开状态把树展平成"行"：`(节点下标, 缩进层级)`。
fn flatten_archive(nodes: &[ArchiveNode], roots: &[usize]) -> Vec<(usize, usize)> {
    fn walk(nodes: &[ArchiveNode], ids: &[usize], depth: usize, out: &mut Vec<(usize, usize)>) {
        for &id in ids {
            out.push((id, depth));
            let node = &nodes[id];
            if node.is_dir && node.expanded {
                walk(nodes, &node.children, depth + 1, out);
            }
        }
    }
    let mut out = Vec::with_capacity(nodes.len());
    walk(nodes, roots, 0, &mut out);
    out
}

fn clear_archive_view() {
    if let Ok(mut slot) = archive_slot().lock() {
        *slot = None;
    }
}

/// 一行：已经展开成绘制/命中测试直接可用的一组值。
struct ArchiveRow {
    /// 树节点下标（点击折叠时用它定位节点：行号会随折叠变，节点不会）。
    node: usize,
    /// 缩进层级。
    depth: usize,
    is_dir: bool,
    expanded: bool,
    /// 显示名（目录补了 `/`）。
    name: String,
    /// 右列：大小（目录留空）。
    size: String,
    /// 右列：时间。
    time: String,
}

/// 当前可见行（按展开状态把树展平）。
fn archive_rows() -> Vec<ArchiveRow> {
    let Ok(slot) = archive_slot().lock() else {
        return Vec::new();
    };
    let Some(view) = slot.as_ref() else {
        return Vec::new();
    };
    flatten_archive(&view.nodes, &view.roots)
        .into_iter()
        .map(|(node, depth)| {
            let entry = &view.nodes[node];
            ArchiveRow {
                node,
                depth,
                is_dir: entry.is_dir,
                expanded: entry.expanded,
                name: if entry.is_dir {
                    format!("{}/", entry.name)
                } else {
                    entry.name.clone()
                },
                size: if entry.is_dir {
                    String::new()
                } else {
                    human_size(entry.size)
                },
                time: entry.time.clone(),
            }
        })
        .collect()
}

/// 点 `y` 落在第几行（已加上当前滚动偏移）。落在内容区之外返回 `None`。
///
/// 口径必须和 `window::paint_list` 完全一致：起点同样加 `window::text_padding_top()`，
/// 行高同样用 `window::list_row_height()`，否则"看到的行"和"点到的行"会差一行。
fn archive_row_index(content: RECT, scroll: usize, y: i32) -> Option<usize> {
    let top = content.top + window::text_padding_top();
    if y < top || y >= content.bottom {
        return None;
    }
    Some(scroll + ((y - top) / window::list_row_height()) as usize)
}

/// 压缩包列表的点击（1.7.3）：选中这一行；目录再翻转展开。
///
/// 返回 `true` = 这次点击被列表吃掉了。不的话会落到 `on_drag_start`，
/// 用户点一下目录就等于把整个窗口拖走。
fn archive_click(hwnd: HWND, lparam: LPARAM) -> bool {
    if !has_archive_view() {
        return false;
    }
    let (x, y) = client_xy(lparam);
    let layout = frame(hwnd);
    if !point_in(x, y, layout.content) {
        return false;
    }
    let Ok(mut slot) = archive_slot().lock() else {
        return false;
    };
    let Some(view) = slot.as_mut() else {
        return false;
    };
    let Some(index) = archive_row_index(layout.content, view.scroll, y) else {
        return false;
    };
    let flat = flatten_archive(&view.nodes, &view.roots);
    let Some(&(node, _)) = flat.get(index) else {
        // 点在最后一行下面的空白：吃掉，别变成拖窗口。
        return true;
    };
    view.selected = Some(node);
    let (name, is_dir) = (view.nodes[node].name.clone(), view.nodes[node].is_dir);
    if is_dir {
        view.nodes[node].expanded = !view.nodes[node].expanded;
    }
    // 折叠之后行数变少，滚动位置可能越界：按"一屏几行"重新夹一次。
    let total = flatten_archive(&view.nodes, &view.roots).len();
    let per_screen = archive_rows_per_screen(layout.content);
    view.scroll = view.scroll.min(total.saturating_sub(per_screen));
    let (scroll, expanded) = (view.scroll, view.nodes[node].expanded);
    drop(slot);
    if is_dir {
        log_debug!(
            SCOPE,
            "ARCHIVE 点击 {name}/ 展开={expanded}（共 {total} 行，第 {index} 行，scroll={scroll}）"
        );
    }
    window::invalidate(hwnd);
    true
}

/// 压缩包列表的滚轮（1.7.3）：按行滚，到头就停。
///
/// 无论有没有真的动，都返回 `true`（"这个滚轮我处理了"）——否则会继续往下走成
/// 图片缩放或 PDF 翻页。
fn archive_scroll_by(rows: isize) -> bool {
    if !has_archive_view() {
        return false;
    }
    let Some(preview) = with_state(|s| s.preview) else {
        return false;
    };
    let hwnd = preview.raw();
    let layout = frame(hwnd);
    let per_screen = archive_rows_per_screen(layout.content);
    let Ok(mut slot) = archive_slot().lock() else {
        return false;
    };
    let Some(view) = slot.as_mut() else {
        return false;
    };
    let total = flatten_archive(&view.nodes, &view.roots).len();
    let max = total.saturating_sub(per_screen);
    let next = (view.scroll as isize + rows).clamp(0, max as isize) as usize;
    let moved = next != view.scroll;
    view.scroll = next;
    drop(slot);
    if moved {
        log_debug!(
            SCOPE,
            "ARCHIVE 滚动 {next}/{max}（一屏 {per_screen} 行，共 {total} 行）"
        );
        window::invalidate(hwnd);
    }
    true
}

/// 压缩包列表的滚动条信息（内容单位 = 行）。放得下返回 `None`。
fn archive_scroll_info(area: RECT) -> Option<window::ScrollInfo> {
    let per_screen = archive_rows_per_screen(area) as f32;
    let slot = archive_slot().lock().ok()?;
    let view = slot.as_ref()?;
    let total = flatten_archive(&view.nodes, &view.roots).len() as f32;
    window::ScrollInfo::new(view.scroll as f32, per_screen, total)
}

/// 当前是否有内置压缩包视图（`repaint` 用它决定走列表通道还是文本通道）。
fn has_archive_view() -> bool {
    archive_slot()
        .lock()
        .map(|slot| slot.is_some())
        .unwrap_or(false)
}

/// 一屏能放下几行。绘制与滚轮/命中测试共用同一份口径，否则"看到的行"和"点到的行"会错位。
fn archive_rows_per_screen(content: RECT) -> usize {
    ((content.bottom - content.top) / window::list_row_height()).max(1) as usize
}

/// 画压缩包列表（1.7.3）。
fn paint_archive(hwnd: HWND) {
    // 一次性把要用的字段拷出来、**立刻放锁**再往下走。
    // 铁律：`archive_rows()` 自己也要锁 `archive_slot`，在持有该锁时调用它 = 自锁死
    // （表现：窗口一片黑、界面卡住）。1.7.3 踩过，别再把读取塞回锁里。
    let (title, selected, scroll) = {
        let Ok(slot) = archive_slot().lock() else {
            return;
        };
        let Some(view) = slot.as_ref() else {
            return;
        };
        let title = view
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        (title, view.selected, view.scroll)
    };
    // 顶栏的悬停/锁状态和文本通道共用一份（1.7.3）：列表视图的顶栏也是同一套按钮。
    let hover = with_state(|s| s.hover_button).flatten();
    let pinned = with_state(|s| s.pinned).unwrap_or(false);
    let hint = hover.map(|button| match button {
        window::ChromeButton::External => "在其他应用打开",
        window::ChromeButton::Close => "关闭预览",
        window::ChromeButton::Pin => {
            if pinned {
                "已阻止关闭（Esc/空格关不掉）"
            } else {
                "阻止关闭（开了就不能取消）"
            }
        }
    });
    // 拖边/缩放后投影要跟上（和文本通道同样的道理，见 repaint 里的注释）。
    sync_shadow();
    let rows = archive_rows();
    let layout = frame(hwnd);
    let rows_per_screen = archive_rows_per_screen(layout.content);
    let path_name = title;
    let start = scroll.min(rows.len());
    let end = (start + rows_per_screen).min(rows.len());
    let page: Vec<window::ListRow<'_>> = rows[start..end]
        .iter()
        .map(|row| window::ListRow {
            depth: row.depth,
            is_dir: row.is_dir,
            expanded: row.expanded,
            name: &row.name,
            size: &row.size,
            time: &row.time,
            selected: selected == Some(row.node),
        })
        .collect();
    let toast = current_toast();
    let chrome = window::Chrome {
        title: &path_name,
        page: None,
        hover,
        hint,
        toast: toast.as_deref(),
        pinned,
    };
    window::paint_list(hwnd, Some(&chrome), Some(layout.content), &page);
    // 滚动条（1.7.3）：`paint_overlays` 里的那条是按 `scroll_info()`（文本行/PDF 页）算的，
    // 压缩包的"内容单位"是自己展平后的行，所以这里单独画一条。
    if layout.bar.is_some()
        && let Some(info) = archive_scroll_info(layout.below_chrome)
    {
        window::paint_scrollbar(hwnd, layout.below_chrome, info, false);
    }
    paint_overlays(hwnd, &layout);
}

thread_local! {
    /// 「正在把焦点交给预览处理器」的重入闸门（1.7.9）。见 `focus_shell_content`。
    static IN_FOCUS_TRANSFER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 把焦点交给系统预览处理器的内容子窗口（1.7.8）。
///
/// 用户实测：把鼠标点进预览窗之后，Excel 的滚轮立刻就能用——所以点窗口时必须把焦点
/// **直接**塞给处理器的内容层，而不是只把容器窗口激活。
///
/// 1.7.9 修自锁死：`SetFocus` 会把激活/焦点消息**泵回我们自己的消息循环**
/// （实测 Word 预览点一下就卡死，最后被 Windows 判为 Application Hang 杀掉）。
/// 所以句柄只在锁内取出，`SetFocus` 在锁放开之后调；再加一个重入闸门，防止
/// 「激活 → 转交焦点 → 又激活」来回套娃。
fn focus_shell_content() {
    if IN_FOCUS_TRANSFER.get() {
        return;
    }
    // 1.8.1：交给处理器**自己**设焦点（`IPreviewHandler::SetFocus`）——Excel 的预览是
    // OLE 就地激活对象，只把焦点塞给它的容器层（`EXCEL9`）时表格"看得见、点不动"，
    // 单元格选不中。COM 调用会把消息泵回我们，所以必须**锁外**调（CONVENTIONS #145）。
    let Some(mut handle) = with_state(|s| s.shell_preview.take()).flatten() else {
        return;
    };
    let target = handle.nav_target();
    IN_FOCUS_TRANSFER.set(true);
    let com_focused = handle.set_com_focus();
    let focused = com_focused || target.map(window::focus_child).unwrap_or(false);
    IN_FOCUS_TRANSFER.set(false);
    with_state(|s| s.shell_preview = Some(handle));
    if focused {
        log_debug!(SCOPE, "焦点已交给预览处理器（滚轮/滚动条可用）");
    }
}

/// 窗口内容区的比例和位图比例差得多不多（>2% 算"不一样"）。
///
/// 1.8.2：图片/视频不再拖边改大小，但最大化、`SetWindowPos`、系统 DPI 变化仍可能让窗口
/// 变成别的比例；那种时候铺满会把图拉变形，所以退回"等比居中"。
fn window_aspect_differs(hwnd: HWND, w: u32, h: u32) -> bool {
    if w == 0 || h == 0 {
        return false;
    }
    let content = window::chrome_rects(window::client_rect(hwnd)).content;
    let cw = (content.right - content.left).max(1) as f64;
    let ch = (content.bottom - content.top).max(1) as f64;
    let bitmap = w as f64 / h as f64;
    ((cw / ch) - bitmap).abs() / bitmap > 0.02
}

fn repaint(hwnd: HWND) {
    if with_state(|s| s.drag_from.is_some()).unwrap_or(false) {
        trace_drag_paint();
    }
    // 换文件时的"冻帧"（1.7.6）：只更新顶栏（标题必须立刻对），内容区保持上一帧。
    // 这样 Office 预览那几十到几百毫秒的 `DoPreview` 期间不会闪出占位文字页。
    if with_state(|s| s.hold_frame).unwrap_or(false) {
        let title = file_title();
        let hover = with_state(|s| s.hover_button).unwrap_or(None);
        let hint = hover.map(|button| match button {
            window::ChromeButton::External => "在其他应用打开",
            window::ChromeButton::Close => "关闭预览",
            window::ChromeButton::Pin => "阻止关闭",
        });
        let toast = current_toast();
        let chrome = window::Chrome {
            title: &title,
            page: None,
            hover,
            hint,
            toast: toast.as_deref(),
            pinned: with_state(|s| s.pinned).unwrap_or(false),
        };
        window::paint_chrome_strip(hwnd, &chrome);
        return;
    }
    // 压缩包（1.7.3）：内置列表视图，不走文本/位图通道。
    if has_archive_view() {
        paint_archive(hwnd);
        return;
    }
    // 系统预览处理器接管内容时（1.4.1）：正文由它的子窗口自己画，我们只画顶栏。
    if with_state(|s| s.shell_preview.is_some()).unwrap_or(false) {
        let title = file_title();
        let (page_now, page_total, editing) =
            with_state(|s| (s.page, s.pages, s.page_edit.clone())).unwrap_or((0, None, None));
        let page = page_total.map(|total| window::PageBox {
            current: (page_now + 1) as u32,
            total: total as u32,
            editing: editing.as_deref(),
        });
        let hover = with_state(|s| s.hover_button).unwrap_or(None);
        let hint = hover.map(|button| match button {
            window::ChromeButton::External => "在其他应用打开",
            window::ChromeButton::Close => "关闭预览",
            window::ChromeButton::Pin => "阻止关闭",
        });
        let toast = current_toast();
        let chrome = window::Chrome {
            title: &title,
            page,
            hover,
            hint,
            toast: toast.as_deref(),
            pinned: with_state(|s| s.pinned).unwrap_or(false),
        };
        // 传空 rows：只把底色和顶栏画出来，内容区留给子窗口。
        window::paint_doc(hwnd, Some(&chrome), None, &[]);
        return;
    }
    // 绘制前兜一次投影：窗口尺寸/位置只要变了（缩放、拖边、外部改尺寸），
    // 阴影浮层就必须跟上，否则会留下一个比窗口大的灰框。
    // `sync_shadow` 内部按尺寸缓存 alpha 图，尺寸没变时只是一次 `SetWindowPos`，可以随便调。
    sync_shadow();
    // 顶栏（1.0.9）：四种格式共用一套。标题=文件名、PDF 多一个页码框、
    // 右侧三个按钮（阻止关闭 / 在其他应用打开 / 关闭）。
    let title = file_title();
    let (page_now, page_total, editing) =
        with_state(|s| (s.page + 1, s.pages, s.page_edit.clone())).unwrap_or((1, None, None));
    let hover = with_state(|s| s.hover_button).flatten();
    let pinned = with_state(|s| s.pinned).unwrap_or(false);
    let hint = hover.map(|button| match button {
        window::ChromeButton::External => "在其他应用打开",
        window::ChromeButton::Close => "关闭预览",
        // 锁是单向的：开着就告诉用户"已经锁了、关不掉"，没开就说清点它会怎样。
        window::ChromeButton::Pin => {
            if pinned {
                "已阻止关闭（Esc/空格关不掉）"
            } else {
                "阻止关闭（开了就不能取消）"
            }
        }
    });
    let page = page_total.map(|total| window::PageBox {
        current: page_now as u32,
        total: total as u32,
        editing: editing.as_deref(),
    });
    let toast = current_toast();
    let chrome = window::Chrome {
        title: &title,
        page,
        hover,
        hint,
        toast: toast.as_deref(),
        pinned,
    };
    let layout = frame(hwnd);

    // 优先画渲染好的内容；还没到位就画占位文字。
    if let Some(canvas) = get_content() {
        match &*canvas {
            Canvas::Bitmap(b) => {
                let pan = with_state(|s| s.pan).unwrap_or((0, 0));
                // 缩放后新位图还没回来 → 把旧位图拉伸铺满窗口（1.0.5），
                // 画面立刻跟着手走；按 1:1 裁剪的话窗口一变就是大半屏底色。
                let preview = with_state(|s| s.zoom_preview).unwrap_or(false);
                let rc = window::client_rect(hwnd);
                log_debug!(
                    SCOPE,
                    "PAINT bitmap {}x{} pan={:?} client={}x{} ink={}{}",
                    b.width,
                    b.height,
                    pan,
                    rc.right - rc.left,
                    rc.bottom - rc.top,
                    canvas.ink_pixels(),
                    if preview {
                        "（缩放预览，拉伸）"
                    } else {
                        ""
                    }
                );
                // SVG / 视频首帧：**只等比缩放**（1.7.8 用户要求"定死长宽比"）。
                // 图片（1.8.1）改由"窗口比例锁死图片比例"来保证不变形，所以这里铺满。
                // PDF/文本/Office 是"页面"语义，继续铺满内容区（比例由渲染尺寸保证）。
                let fit_bitmap = with_state(|s| s.last_path.clone())
                    .flatten()
                    .map(|p| match format::kind_for_path(&p) {
                        // SVG / 视频：窗口比例本身是自由的，等比居中留白。
                        format::Kind::Svg | format::Kind::Video => true,
                        // 图片（1.8.1）：窗口比例已在开窗时锁成图片比例，铺满即等比。
                        // 1.8.2 起拖边改大小对图片关掉了，但**最大化/系统改大小**仍可能
                        // 把窗口变成别的比例——这时退回等比居中，别把图拉变形。
                        format::Kind::Image => window_aspect_differs(hwnd, b.width, b.height),
                        _ => false,
                    })
                    .unwrap_or(false);
                if fit_bitmap {
                    // 视频（1.7.9）：播放时 `s.video` 里有"当前这一帧"，它比渲染出来的首帧新。
                    // 用帧画（而不是首帧位图），窗口改大小/被别的窗口盖住后重画才不会"倒带"。
                    let painted_frame = with_state(|s| {
                        s.video.as_ref().and_then(|v| {
                            v.last.as_ref().map(|f| (f.width, f.height, f.bgra.clone()))
                        })
                    })
                    .flatten();
                    // 视频底部有进度条：贴图时要把那条留出来，否则这一帧会把进度条盖掉。
                    let reserve_bar = current_kind() == Some(format::Kind::Video);
                    match painted_frame {
                        Some((w, h, bgra)) => {
                            window::paint_bitmap_fit(hwnd, w, h, &bgra, Some(&chrome), reserve_bar)
                        }
                        None => window::paint_bitmap_fit(
                            hwnd,
                            b.width,
                            b.height,
                            &b.bgra,
                            Some(&chrome),
                            reserve_bar,
                        ),
                    }
                    paint_overlays(hwnd, &layout);
                    paint_lens_if_any(hwnd);
                    return;
                }
                if preview {
                    window::paint_bitmap_scaled(hwnd, b.width, b.height, &b.bgra, Some(&chrome));
                } else {
                    // 1.4.3：位图尺寸和内容区尺寸不一致时 `paint_bitmap_at` 会**拉伸**，
                    // 每一帧都要重采样整张图 —— 表现就是"画面发虚 + 拖动跟不上 + 滚动频闪"。
                    // 这里量一次并记 warn，同时把尺寸对不上的事实写进日志（便于定位）。
                    let content = layout.content;
                    let (cw, ch) = (
                        (content.right - content.left).max(1),
                        (content.bottom - content.top).max(1),
                    );
                    // 只统计"真的差很多"的情况：位图按宽高比取整，和内容区常常差 1–2 px，
                    // 那种拉伸肉眼看不出来、代价也可以忽略，不该刷日志。
                    //
                    // 判据只认**宽度**：宽度对不上说明"渲染口径"和"绘制口径"又不一致了
                    // （1.8.3 修过一次：渲染盒多了滚动条宽，右边被压住 24 px）。
                    // 高度对不上是正常的——PDF 页面有固定宽高比，比内容区矮的那一截
                    // 就是留白，不该被当成 bug 报警（现场日志里那 3 行 STRETCH 就是这么来的）。
                    let off_w = (b.width as i32 - cw).abs();
                    if off_w > 16 && !with_state(|s| s.zoom_preview).unwrap_or(false) {
                        log_warn!(
                            SCOPE,
                            "STRETCH 位图宽 {} != 内容区 {cx}（渲染口径不一致，上屏要拉伸）",
                            b.width,
                            cx = cw
                        );
                    } else if (b.height as i32 - ch).abs() > 16 {
                        log_debug!(
                            SCOPE,
                            "LETTERBOX 位图高 {} < 内容区 {cy}（页面比例所限，正常留白）",
                            b.height,
                            cy = ch
                        );
                    }
                    window::paint_bitmap_at(
                        hwnd,
                        b.width,
                        b.height,
                        &b.bgra,
                        pan.0,
                        pan.1,
                        Some(&chrome),
                    );
                }
                paint_overlays(hwnd, &layout);
                paint_lens_if_any(hwnd);
                return;
            }
            Canvas::Text(t) => {
                // 代码预览（1.0.8）：不折行、一行一源行，走 paint_doc 的"一行多色"通道。
                if let Some(spans) = &t.spans {
                    paint_code(hwnd, &chrome, &layout, t, spans);
                    paint_overlays(hwnd, &layout);
                    return;
                }
                // 可视区虚拟化：只把当前窗口该显示的那几十行交给 GDI。
                // 上游 QuickLook 在 64 KB 文本上滚屏吃掉 6.6 GB，就是因为把整篇都塞给了控件。
                let rows = visible_rows(hwnd);
                let scroll = with_state(|s| s.scroll).unwrap_or(0).min(t.lines.len());
                // paint_styled 左边留 24 px 边距 + 一条行号栏，右边留 24 px。
                // 折行可用宽度必须把这几块减掉（CONVENTIONS #38：量宽 != 画宽就会"折了行还被吃字"）。
                // 排版宽度必须等于**绘制用的**内容区宽度（CONVENTIONS #38）：
                // 内容区已经扣掉了顶栏、目录栏、滚动条，这里照着它算。
                let rc = layout.content;
                // HTML（1.7.9）是"排版出来的文档"，不是源码：行号栏和"第 X–Y 行"都是噪音，
                // 排出来的效果应该像一篇网页而不是带行号的文本。音频卡片同理（就几行元信息）。
                let plain_doc = matches!(
                    current_kind(),
                    Some(format::Kind::Html) | Some(format::Kind::Audio)
                );
                let gutter = if plain_doc { 0 } else { window::text_gutter() };
                let width = ((rc.right - rc.left) - 48 - gutter).max(0) as u32;
                // 折行需要"这段文字有多宽"，只能问 GDI：抓一次 DC 量一屏，
                // 量不到（拿 DC 失败）就退回不折行 —— 宁可不折行也不能画错。
                let measurer = window::Measurer::new(hwnd);
                // 每行记 `(文本, 样式, 行号)`：折行产生的续行不带行号，只有源行首行带。
                let mut body: Vec<(String, qlr_render::LineStyle, Option<u32>)> = Vec::new();
                let mut consumed = 0usize;
                if let Some(m) = &measurer {
                    // 逐原始行量、逐行记样式（Markdown 每行样式不同：标题字号更大、代码等宽）。
                    for (i, line) in t.lines.iter().enumerate().skip(scroll) {
                        if body.len() >= rows {
                            break;
                        }
                        let style = t
                            .styles
                            .as_ref()
                            .and_then(|all| all.get(i).copied())
                            .unwrap_or_default();
                        let budget = rows - body.len();
                        let mut texts: Vec<String> = Vec::new();
                        let measure = |s: &str| m.width_styled(to_text_style(style), s);
                        qlr_render::wrap::push_wrapped(line, width, &measure, &mut texts, budget);
                        if texts.is_empty() {
                            continue;
                        }
                        consumed += 1;
                        for (k, text) in texts.into_iter().enumerate() {
                            let number = if plain_doc {
                                None
                            } else if k == 0 {
                                Some((i + 1) as u32)
                            } else {
                                None
                            };
                            body.push((text, style, number));
                        }
                    }
                } else {
                    let end = (scroll + rows).min(t.lines.len());
                    for i in scroll..end {
                        let style = t
                            .styles
                            .as_ref()
                            .and_then(|all| all.get(i).copied())
                            .unwrap_or_default();
                        body.push((
                            t.lines[i].clone(),
                            style,
                            if plain_doc {
                                None
                            } else {
                                Some((i + 1) as u32)
                            },
                        ));
                    }
                    consumed = end - scroll;
                }
                with_state(|s| s.wrapped_consumed = consumed);
                log_debug!(
                    SCOPE,
                    "WRAP 原文 {} 行 从 {} 起，屏幕 {} 行，宽 {}px -> 折成 {} 行（吃掉 {} 原始行）{}",
                    t.lines.len(),
                    scroll,
                    rows,
                    width,
                    body.len(),
                    consumed,
                    if t.styles.is_some() {
                        "（带样式）"
                    } else {
                        ""
                    }
                );
                let last = scroll + consumed;
                let status = format!(
                    "── 第 {}–{} 行 / 共 {} 行{} ──",
                    scroll + 1,
                    last.max(scroll + 1),
                    t.lines.len(),
                    if t.truncated {
                        "，文件被截断"
                    } else {
                        ""
                    }
                );
                let mut styled: Vec<window::StyledRow<'_>> = Vec::with_capacity(body.len() + 1);
                if !plain_doc {
                    styled.push(window::StyledRow {
                        number: None,
                        text: &status,
                        style: window::TextStyle::Normal,
                        bg: None,
                    });
                }
                for (text, style, number) in &body {
                    styled.push(window::StyledRow {
                        number: *number,
                        text,
                        style: to_text_style(*style),
                        bg: line_bg(*style),
                    });
                }
                // 每份内容打一次样式分布（INFO 级）：渲染样式是"看不见的"东西，
                // 出问题时只有这条日志能证明"确实是按样式画的、各占几行"。
                let first_time = with_state(|s| {
                    let first = !s.styled_logged;
                    s.styled_logged = true;
                    first
                })
                .unwrap_or(false);
                if first_time && !body.is_empty() {
                    let mut tally = [0usize; qlr_render::LineStyle::COUNT];
                    for (_, style, _) in &body {
                        tally[style.as_index()] += 1;
                    }
                    let mut parts: Vec<String> = Vec::new();
                    for style in [
                        qlr_render::LineStyle::Heading1,
                        qlr_render::LineStyle::Heading2,
                        qlr_render::LineStyle::Heading3,
                        qlr_render::LineStyle::Code,
                        qlr_render::LineStyle::Quote,
                        qlr_render::LineStyle::List,
                        qlr_render::LineStyle::Rule,
                        qlr_render::LineStyle::Table,
                        qlr_render::LineStyle::Normal,
                    ] {
                        let n = tally[style.as_index()];
                        if n > 0 {
                            parts.push(format!("{}={n}", style.name()));
                        }
                    }
                    log_info!(
                        SCOPE,
                        "STYLED 屏幕 {} 行（样式分布：{}）行号栏={}px",
                        styled.len() - 1,
                        parts.join(" "),
                        window::text_gutter()
                    );
                }
                let _ = t.encoding;
                // 量一帧的绘制耗时：滚动"频闪/卡"时这条日志能直接指认是绘制太慢。
                let paint_started = Instant::now();
                // 正文和叠加层画进同一块内存 DC，整窗只上屏一次（1.5.2，消掉侧栏闪）。
                let overlay = |hdc: HDC, content: RECT| paint_overlays_dc(hdc, &layout, content);
                window::paint_styled_with(
                    hwnd,
                    Some(&chrome),
                    Some(layout.content),
                    &styled,
                    Some(&overlay),
                );
                let ms = paint_started.elapsed().as_millis();
                if ms >= 8 {
                    log_warn!(SCOPE, "SLOWPAINT text {ms}ms（{} 行）", styled.len());
                }
                return;
            }
        }
    }
    let lines = get_lines();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let overlay = |hdc: HDC, content: RECT| paint_overlays_dc(hdc, &layout, content);
    window::paint_text_with(
        hwnd,
        Some(&chrome),
        Some(layout.content),
        &refs,
        Some(&overlay),
    );
}

/// 目录里"当前读到哪一节"：取最后一个起始行 ≤ 滚动位置的小节。
fn toc_following(items: &[(u8, String, usize)], scroll: usize) -> Option<usize> {
    let mut found = None;
    for (index, (_, _, line)) in items.iter().enumerate() {
        if *line <= scroll {
            found = Some(index);
        }
    }
    found
}

/// 在内容之上叠加目录栏与滚动条（1.0.9）。
///
/// 为什么单独画：这两块用 `GetDC` 直接往窗口上画（一次 `BeginPaint` 只能有一个），
/// 所以必须在正文绘制**之后**调用，否则会被正文的 `FillRect` 盖掉。
/// 把目录栏与滚动条画到**给定 DC** 上（1.5.2：和正文共用内存 DC，整窗一次上屏）。
fn paint_overlays_dc(hdc: HDC, layout: &Frame, content: RECT) {
    if let Some(toc) = layout.toc {
        let (items, hovered, scroll) = with_state(|s| (s.toc.clone(), s.toc_active, s.scroll))
            .unwrap_or_else(|| (Vec::new(), None, 0));
        let refs: Vec<window::TocItem<'_>> = items
            .iter()
            .map(|(level, text, _)| window::TocItem {
                level: *level,
                text: text.as_str(),
            })
            .collect();
        let selected = hovered.or_else(|| toc_following(&items, scroll));
        let first_toc = with_state(|s| s.toc_scroll).unwrap_or(0);
        window::paint_toc_dc(hdc, toc, &refs, selected, first_toc);
    }
    if layout.bar.is_some()
        && let Some(info) = scroll_info()
    {
        window::paint_scrollbar_dc(hdc, layout.below_chrome, info, false);
    }
    // 音频/视频进度条（1.7.9）：整窗重画时也要画一次，否则拖动窗口/改大小之后那条就没了
    // （`TIMER_AUDIO`/`TIMER_VIDEO` 只在播放时刷，暂停状态下不会有第二次机会补上）。
    if let Some((pos, dur)) = playback_progress() {
        let playing = playback_playing();
        window::paint_audio_bar_dc(hdc, content, pos, dur, playing);
    }
    let _ = content;
}

fn paint_overlays(hwnd: HWND, layout: &Frame) {
    if let Some(toc) = layout.toc {
        let (items, hovered, scroll) =
            with_state(|s| (s.toc.clone(), s.toc_active, s.scroll)).unwrap_or_default();
        // 悬停优先，否则跟着当前滚动位置高亮（像阅读器那样，滚到哪里目录指到哪里）。
        let selected = hovered.or_else(|| toc_following(&items, scroll));
        let first_toc = with_state(|s| s.toc_scroll).unwrap_or(0);
        let refs: Vec<window::TocItem<'_>> = items
            .iter()
            .map(|(level, text, _)| window::TocItem {
                level: *level,
                text: text.as_str(),
            })
            .collect();
        window::paint_toc(hwnd, toc, &refs, selected, first_toc);
    }
    if layout.bar.is_some()
        && let Some(info) = scroll_info()
    {
        window::paint_scrollbar(hwnd, layout.below_chrome, info, false);
    }
    // 音频/视频进度条（1.7.9）：位图通道（图片/视频/SVG）走的是这里而不是 `_dc` 版本，
    // 而贴图会把整块内容区先铺一遍底色——不补这一笔，进度条就会在重画时消失。
    if let Some((pos, dur)) = playback_progress() {
        let playing = playback_playing();
        window::paint_audio_bar(hwnd, layout.below_chrome, pos, dur, playing);
    }
    // 最外圈那三条灰边（1.0.10）：必须**最后**画，否则会被正文/位图的整块填充盖掉。
    // 用户要求"窗口周围新增一圈灰色阴影，方便边边和其他程序的白色区分"。
    window::paint_window_frame(hwnd);
}

/// `WM_COPYDATA` 是 `SendMessage` 送来的，处理它时本线程处于"输入同步呼叫"状态。
///
/// 这个状态下调用 COM（`CoCreateInstance(ShellWindows)`）会被系统直接拒成
/// `RPC_E_CANTCALLOUT_ININPUTSYNCCALL (0x8001010D)` —— 实测踩到过：
/// 第二个实例转交 `--simulate-hotkey` 时，主实例拿不到选中项。
/// 所以这里**只排队**，用 `PostMessage` 转成一条普通队列消息再处理，
/// 那时输入同步状态已经退出，COM 与渲染都能正常跑。
fn on_copy_data(hwnd: HWND, lparam: LPARAM) {
    let Some(bytes) = instance::copy_data_payload(lparam) else {
        log_warn!(SCOPE, "收到无法识别的 WM_COPYDATA，已忽略");
        return;
    };
    let Some(request) = Request::from_bytes(&bytes) else {
        log_warn!(SCOPE, "WM_COPYDATA 载荷解析失败（{} 字节）", bytes.len());
        return;
    };
    if let Ok(mut pending) = pending_requests().lock() {
        pending.push(request);
    }
    window::post(hwnd, ipc::WM_QLR_INVOKE);
}

/// 代码预览上屏（1.0.8）：不折行、一行一个源行，按 token 分段着色。
///
/// 为什么不折行：VSCode 默认也不折行；折行会让"一行代码变两行"，行号对不上，
/// 也会打乱缩进语义。超长行由 `paint_doc` 在右边界截断。
fn paint_code(
    hwnd: HWND,
    chrome: &window::Chrome<'_>,
    layout: &Frame,
    t: &qlr_render::TextDoc,
    spans: &[Vec<qlr_render::Span>],
) {
    let rows = rows_in(layout.content);
    let scroll = with_state(|s| s.scroll).unwrap_or(0).min(t.lines.len());
    let end = (scroll + rows).min(t.lines.len()).min(spans.len());

    let status = format!(
        "── 第 {}–{} 行 / 共 {} 行{} · 源码 ──",
        scroll + 1,
        end.max(scroll + 1),
        t.lines.len(),
        if t.truncated {
            "，文件被截断"
        } else {
            ""
        }
    );
    // 先把 token 段映射成平台层样式（`paint_doc` 只认 `&[window::Span]`）。
    let mut lane: Vec<Vec<window::Span<'_>>> = Vec::with_capacity(end - scroll + 1);
    lane.push(vec![window::Span {
        text: &status,
        style: window::TextStyle::Normal,
    }]);
    for row in &spans[scroll..end] {
        lane.push(
            row.iter()
                .map(|s| window::Span {
                    text: s.text.as_str(),
                    style: to_token_style(s.token),
                })
                .collect(),
        );
    }
    let mut doc: Vec<window::DocRow<'_>> = Vec::with_capacity(lane.len());
    doc.push(window::DocRow {
        number: None,
        spans: &lane[0],
        bg: None,
    });
    for (k, spans_for_row) in lane.iter().enumerate().skip(1) {
        doc.push(window::DocRow {
            number: Some((scroll + k) as u32),
            spans: spans_for_row,
            bg: None,
        });
    }
    with_state(|s| s.wrapped_consumed = end - scroll);
    // 着色是"看不见的"东西，出问题时只有这条日志能证明各 token 各占几段。
    let first_time = with_state(|s| {
        let first = !s.styled_logged;
        s.styled_logged = true;
        first
    })
    .unwrap_or(false);
    if first_time {
        let mut tally = [0usize; qlr_render::Token::COUNT];
        for row in &spans[scroll..end] {
            for s in row {
                tally[s.token.as_index()] += 1;
            }
        }
        log_info!(
            SCOPE,
            "CODE 屏幕 {} 行 token 段：plain={} keyword={} str={} comment={} number={} 行号栏={}px",
            end - scroll,
            tally[0],
            tally[1],
            tally[2],
            tally[3],
            tally[4],
            window::text_gutter()
        );
    }
    window::paint_doc(hwnd, Some(chrome), Some(layout.content), &doc);
}

/// 渲染层 token → 平台层字体/颜色。
fn to_token_style(token: qlr_render::Token) -> window::TextStyle {
    match token {
        qlr_render::Token::Plain => window::TextStyle::Code,
        qlr_render::Token::Keyword => window::TextStyle::Keyword,
        qlr_render::Token::Str => window::TextStyle::Str,
        qlr_render::Token::Comment => window::TextStyle::Comment,
        qlr_render::Token::Number => window::TextStyle::Number,
    }
}

fn pending_requests() -> &'static Mutex<Vec<Request>> {
    PENDING.get_or_init(|| Mutex::new(Vec::new()))
}
/// 处理排队的请求（从消息循环里调用，此时已不在输入同步呼叫里）。
fn drain_pending() {
    loop {
        let next = pending_requests().lock().ok().and_then(|mut q| {
            if q.is_empty() {
                None
            } else {
                Some(q.remove(0))
            }
        });
        match next {
            Some(request) => apply_request(&request),
            None => return,
        }
    }
}

fn on_tray(lparam: LPARAM) {
    // 低 16 位是鼠标消息，高 16 位是图标 id（这里只有一个图标，忽略 id）。
    let mouse_message = (lparam.0 as u32) & 0xFFFF;
    // 双击托盘图标 = 打开说明/占位窗口。
    // 程序常驻后台后没有任何可见入口（tool window 不进任务栏），
    // 双击是最符合直觉的"我要看看这东西"的动作。
    if mouse_message == WM_LBUTTONDBLCLK {
        if with_state(|s| s.visible).unwrap_or(false) {
            hide_preview();
        } else {
            show_preview(None);
        }
        return;
    }
    if mouse_message != WM_RBUTTONUP {
        return;
    }
    let Some(host) = with_state(|s| s.host) else {
        return;
    };
    match tray::show_menu(host.raw()) {
        tray::menu::SETTINGS => open_settings(),
        tray::menu::ABOUT => log_info!(
            SCOPE,
            "{} — GPL-3.0-or-later，配置目录 {:?}",
            version::banner(),
            paths::data_dir()
        ),
        tray::menu::EXIT => quit(),
        _ => {}
    }
}

/// 打开设置窗口（托盘菜单 + `--settings` 共用）。
fn open_settings() {
    let Some((host, config)) = with_state(|s| (s.host, s.config.clone())) else {
        return;
    };
    settings::open(host.raw(), &config);
    log_info!(SCOPE, "设置窗口：已请求打开");
}

/// 设置窗保存完毕：重读配置 → 热更新钩子绑定。
///
/// 为什么不直接让设置窗改钩子：钩子的绑定表是平台层的**进程级全局量**，
/// 而"谁读配置"应该只有一处。设置窗写文件、宿主读文件，两边职责不交叉。
fn on_settings_saved() {
    let path = paths::config_path();
    let (config, err) = Config::load(&path);
    if let Some(e) = err {
        log_warn!(SCOPE, "设置保存后重读配置有问题：{e}");
    }
    hotkey::set_bindings(config.hotkeys.packed());
    with_state(|s| s.config = config.clone());
    log_info!(SCOPE, "配置已热更新：{}", config.summary());
}

/// 空格热键命中（由钩子线程 `PostMessage` 过来，跑在 UI 线程上，所以能安全调 COM）。
///
/// `vk` = 虚拟键码，`pressed_at` 是钩子按下时记的 tick（`GetTickCount`），
/// 用来量"钩子 → 到手"这段延迟——它应该是个位数毫秒，涨上去说明 UI 线程被什么阻塞了。
fn on_hotkey(vk: usize, pressed_at: u32) {
    // 真实空格/翻页键从钩子线程过来，不经过 `apply_request`，延迟起点要在这里重置。
    begin_request();
    // 页码输入（1.0.9）：预览窗没有键盘焦点，数字键是钩子转发过来的。
    if with_state(|s| s.page_edit.is_some()).unwrap_or(false) && page_edit_key(vk) {
        return;
    }
    // 音频（1.7.9）：P 播放/暂停，左右键快退/快进 5 秒。
    // 必须放在 `page_delta_for` 之前：左右键在那条路上会被当成"上一页/下一页"，
    // 而音频没有页——最坏情况是翻页逻辑把窗口内容换成占位文字。
    if playback_active() {
        match vk as u32 {
            0x50 => {
                playback_toggle();
                return;
            }
            0x25 => {
                playback_seek(-AUDIO_SEEK_STEP_MS);
                return;
            }
            0x27 => {
                playback_seek(AUDIO_SEEK_STEP_MS);
                return;
            }
            _ => {}
        }
    }
    // 翻页键（PageDown/PageUp/右/左）：钩子只旁听不吞，走到这里就直接翻页。
    if let Some(delta) = page_delta_for(vk) {
        turn_page(delta);
        return;
    }
    match vk as u32 {
        // + / = 放大，- 缩小，0 复位；上下方向键纵向平移（放大后才有意义）。
        0xBB | 0x6B => return set_zoom(1.25),
        0xBD | 0x6D => return set_zoom(0.8),
        0x30 | 0x60 => return zoom_reset(),
        0x28 => {
            if !scroll_text_by(1) {
                pan_by(0, 1);
            }
            return;
        }
        0x26 => {
            if !scroll_text_by(-1) {
                pan_by(0, -1);
            }
            return;
        }
        _ => {}
    }

    let now = unsafe { GetTickCount() };
    let hook_to_handler = now.wrapping_sub(pressed_at);
    let found = selection::current_selection();
    log_info!(
        SCOPE,
        "HOTKEY vk=0x{:02X} fg={} hook_to_handler={}ms {}",
        vk,
        hotkey::foreground_class().unwrap_or_else(|| "(无)".to_string()),
        hook_to_handler,
        found.summary()
    );
    match found.first().cloned() {
        Some(path) => toggle(Some(path)),
        None => {
            // 没有选中项：窗口开着就当"再按一次空格收起"；否则**把空格还回去**。
            //
            // 钩子为了拿选中项必须先吞键（回调里做不了 COM），如果这里什么也不做，
            // 用户就会觉得"空格按了没反应、键盘坏了"——所以必须重放。
            if with_state(|s| s.visible).unwrap_or(false) {
                // 1.0.10：开了"阻止关闭"之后这条路径也要挡住（它不经过 `toggle`）。
                if !close_blocked("空格") {
                    hide_preview();
                }
            } else {
                log_debug!(SCOPE, "热键命中但没有选中项，重放按键让系统继续处理");
                hotkey::replay(vk as u32);
            }
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        // 1.8.3：每条消息先对齐到本窗口的 DPI。放在最前面有两个作用：
        // 1) 绘制前 `dpi::px()` 拿到的是本窗口 DPI（预热线程、设置窗、跨屏拖动都靠它）；
        // 2) `WM_DPICHANGED` 到达时 DPI 已经变了，`enter` 会发现变化并触发字体重建钩子。
        dpi::enter(hwnd);
        log_debug!(SCOPE, "wm=0x{msg:04X}");
        match msg {
            // 去掉系统边框/标题栏，同时保留 DWM 阴影与亚克力。
            WM_NCCALCSIZE if wparam.0 != 0 => LRESULT(0),
            // 返回 1 表示"背景我自己管"，避免擦除时把亚克力刷成实色。
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                repaint(hwnd);
                LRESULT(0)
            }
            WM_COPYDATA => {
                on_copy_data(hwnd, lparam);
                LRESULT(1)
            }
            ipc::WM_QLR_INVOKE => {
                // 先收掉待卸的系统预览处理器（此时已经脱离了 WM_COPYDATA 的同步调用栈）。
                drain_shell_trash();
                drain_pending();
                LRESULT(0)
            }
            ipc::WM_TRAY_CALLBACK => {
                on_tray(lparam);
                LRESULT(0)
            }
            ipc::WM_QLR_HOTKEY => {
                on_hotkey(wparam.0, lparam.0 as u32);
                LRESULT(0)
            }
            ipc::WM_QLR_ESCAPE => {
                // 1.0.10：开了"阻止关闭"之后 Esc 也关不掉（锁是单向的）。
                if close_blocked("Esc") {
                    return LRESULT(0);
                }
                log_debug!(SCOPE, "ESC 收起预览");
                hide_preview();
                LRESULT(0)
            }
            ipc::WM_QLR_WHEEL => {
                // wparam 里是 i16 的滚轮增量（一格 ±120）
                let delta = (wparam.0 as i32) as i16 as i32;
                // 走总入口：文本滚、图片/PDF 缩（1.0.1 前这里直连 wheel_zoom 是错的）。
                on_wheel(delta);
                LRESULT(0)
            }
            ipc::WM_QLR_RENDER => {
                on_render_done();
                LRESULT(0)
            }
            ipc::WM_QLR_SETTINGS_SAVED => {
                on_settings_saved();
                LRESULT(0)
            }
            ipc::WM_QLR_FOLLOW => {
                // 跟随线程发现选中项变了（1.7.6）：立刻切，不等计时器。
                on_follow_wake();
                LRESULT(0)
            }
            ipc::WM_QLR_SHELLFIT => {
                // 窗口尺寸稳下来了：把系统预览处理器的内容子窗口对齐到新尺寸（1.7.9）。
                fit_shell_to_window();
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                // 1) 先看是不是按在窗口边缘（拖边改大小，1.0.10 加回来）。
                let (x, y) = client_xy(lparam);
                let hit_rect = window::client_rect(hwnd);
                let edges = edges_at(hit_rect, x, y, resize_margin(hwnd));
                // 1.7.4 起埋点：`按下 (x, y) 客户区 WxH 边=?`——"拖边不生效"这类反馈
                // 全看这一条就能分清是"点没到边缘"还是"点根本没进来"。
                log_debug!(
                    SCOPE,
                    "按下 ({}, {}) 客户区 {}x{} 边={}",
                    x,
                    y,
                    hit_rect.right - hit_rect.left,
                    hit_rect.bottom - hit_rect.top,
                    if edges.any() {
                        edges.describe()
                    } else {
                        "无".to_string()
                    }
                );
                if edges.any() {
                    begin_resize(hwnd, edges, lparam);
                    return LRESULT(0);
                }
                // 2) 顶栏 / 目录 / 滚动条（1.0.9）：这些是"窗口的家具"。
                if on_chrome_click(hwnd, lparam) {
                    LRESULT(0)
                } else if archive_click(hwnd, lparam) {
                    // 3) 压缩包行（1.7.3）：选中 + 目录折叠，不吃掉的话会变成拖窗口。
                    LRESULT(0)
                } else {
                    on_drag_start(hwnd, lparam);
                    LRESULT(0)
                }
            }
            WM_MOUSEMOVE => {
                // 悬停只影响顶栏按钮与目录高亮，先更新再走手势分派。
                update_hover(hwnd, lparam);
                on_mouse_move(lparam);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                if with_state(|s| std::mem::replace(&mut s.scroll_drag, false)).unwrap_or(false) {
                    window::release_capture();
                }
                // 拖边结束要按新尺寸重画（位图得重新光栅化）。
                if let Some(edges) = with_state(|s| s.resize_edges.take()).flatten() {
                    with_state(|s| s.resize_from = None);
                    finish_resize(edges);
                    return LRESULT(0);
                }
                on_drag_end();
                LRESULT(0)
            }
            WM_RBUTTONDOWN => {
                on_zoom_drag_start(hwnd, lparam);
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                on_zoom_drag_end();
                LRESULT(0)
            }
            WM_CLOSE => {
                hide_preview();
                LRESULT(0)
            }
            // 跟随选中项（1.0.10）：只有我们自己的定时器才处理。
            WM_TIMER if wparam.0 == TIMER_FOLLOW => {
                on_follow_timer();
                LRESULT(0)
            }
            // 渲染看门狗（1.2.0）：只记日志 + 请求取消，绝不阻塞 UI。
            WM_TIMER if wparam.0 == TIMER_WATCHDOG => {
                on_render_watchdog();
                LRESULT(0)
            }
            // 音频进度条（1.7.9）：只重画进度条那一条，整窗不动。
            WM_TIMER if wparam.0 == TIMER_AUDIO => {
                let Some((pos, dur)) = audio_progress() else {
                    let _ = KillTimer(Some(hwnd), TIMER_AUDIO);
                    with_state(|s| s.audio_ticking = false);
                    return LRESULT(0);
                };
                // 播完了（MCI 停在末尾不再前进）：收掉计时器、给一句提示、进度条复位到 0。
                if dur > 0 && pos + AUDIO_TICK_MS as u64 >= dur {
                    let _ = KillTimer(Some(hwnd), TIMER_AUDIO);
                    with_state(|s| s.audio_ticking = false);
                    let _ = with_state(|s| s.audio.as_ref().map(|p| p.seek(0)));
                    let content = window::chrome_rects(window::client_rect(hwnd)).content;
                    window::paint_audio_bar(hwnd, content, 0, dur, false);
                    show_toast(hwnd, "播放结束");
                    log_debug!(SCOPE, "AUDIO 播放结束（{}ms）", dur);
                    return LRESULT(0);
                }
                paint_audio_now(hwnd);
                LRESULT(0)
            }
            // 视频逐帧播放（1.7.9）：解一帧贴一帧，落后就跳帧。
            WM_TIMER if wparam.0 == TIMER_VIDEO => {
                video_tick(hwnd);
                LRESULT(0)
            }
            // 顶栏提示到期（1.7.4）：擦掉那一句话，并把顶栏重画一次。
            WM_TIMER if wparam.0 == TIMER_TOAST => {
                with_state(|s| s.toast = None);
                let _ = KillTimer(Some(hwnd), TIMER_TOAST);
                window::invalidate(hwnd);
                LRESULT(0)
            }
            // 接管处理器输入（1.7.4）：等首帧画完再关，给它留出 DoPreview 的时间。
            WM_TIMER if wparam.0 == TIMER_SHELL_INPUT => {
                let _ = KillTimer(Some(hwnd), TIMER_SHELL_INPUT);
                // 轮询"内容真的画出来了吗"（1.7.7）。判据 = 内容区里最大那块可见后代窗口
                // 占到 50% 以上；到点没收尾就再等 120 ms，最多 3 s（`SHELL_POLL_MAX`）。
                let content = window::chrome_rects(window::client_rect(hwnd)).content;
                let (ratio, has_preview) = with_state(|s| {
                    (
                        s.shell_preview
                            .as_ref()
                            .map(|handle| handle.visible_area(content))
                            .unwrap_or(0),
                        s.shell_preview.is_some(),
                    )
                })
                .unwrap_or((0, false));
                let poll = with_state(|s| {
                    s.shell_poll = s.shell_poll.saturating_add(1);
                    s.shell_poll
                })
                .unwrap_or(0);
                let ready = ratio >= 50 || !has_preview || poll >= SHELL_POLL_MAX;
                if !ready {
                    let _ = SetTimer(Some(hwnd), TIMER_SHELL_INPUT, SHELL_POLL_MS, None);
                    return LRESULT(0);
                }
                log_debug!(SCOPE, "SHELL 就绪：内容占 {ratio}%（轮询 {poll} 次）");
                // 只有 PPT 接管输入（翻页由我们计数）；Excel/Word 必须让处理器自己收鼠标——
                // 禁用宿主 `EnableWindow(FALSE)` 会连子窗口一起禁掉，用户点不到单元格、
                // 拖不了滚动条（1.8.1 用户反馈"Excel 焦点进去后左键选不中单元格"）。
                if with_state(|s| s.shell_input_pending).unwrap_or(false) {
                    let owns = with_state(|s| s.shell_owns_input).unwrap_or(false);
                    if owns {
                        with_state(|s| {
                            if let Some(handle) = s.shell_preview.as_mut() {
                                handle.take_over_input();
                            }
                        });
                    }
                    with_state(|s| s.shell_input_pending = false);
                }
                // 一打开就把"UI 激活"做完（1.8.1）：`IPreviewHandler::SetFocus` 在
                // `WM_ACTIVATE`/`WM_SETFOCUS` 里调会被拒（`0x8001010D` 输入同步呼叫），
                // 只有从定时器这种"退栈后"的地方才调得通。做完这一步，用户**第一次**点
                // 单元格就能选中，不必先点一下"激活"。只有非 PPT 走这条（PPT 由我们接管翻页）。
                if !with_state(|s| s.shell_owns_input).unwrap_or(false) {
                    focus_shell_content();
                }
                // 冻帧解除（1.7.6）：内容已经画出来，可以整窗重画。
                if with_state(|s| s.hold_frame).unwrap_or(false) {
                    with_state(|s| s.hold_frame = false);
                    window::invalidate(hwnd);
                }
                LRESULT(0)
            }
            // 换帧（1.7.6）：到点把"盖住"的旧处理器卸掉，露出新处理器的首帧。
            WM_TIMER if wparam.0 == TIMER_SHELL_SWAP => {
                let _ = KillTimer(Some(hwnd), TIMER_SHELL_SWAP);
                // 换帧不再"死等 260 ms"（1.7.8）：Excel 的 `DoPreview` 要 768 ms，
                // 掀开得太早会露出还没画完的白/黑底（用户说的"Office 之间切换有黑边"）。
                // 判据同 `TIMER_SHELL_INPUT`：新处理器内容占到 50% 才掀开，最多等 1.8 s。
                let content = window::chrome_rects(window::client_rect(hwnd)).content;
                let (ratio, has_preview) = with_state(|s| {
                    (
                        s.shell_preview
                            .as_ref()
                            .map(|handle| handle.visible_area(content))
                            .unwrap_or(0),
                        s.shell_preview.is_some(),
                    )
                })
                .unwrap_or((0, false));
                let waited = with_state(|s| {
                    s.shell_poll = s.shell_poll.saturating_add(1);
                    s.shell_poll
                })
                .unwrap_or(0);
                if has_preview && ratio < 50 && waited < 15 {
                    let _ = SetTimer(Some(hwnd), TIMER_SHELL_SWAP, SHELL_POLL_MS, None);
                    return LRESULT(0);
                }
                let cover = with_state(|s| s.shell_cover.take()).flatten();
                if cover.is_some() {
                    retire_shell_preview(cover);
                    with_state(|s| s.hold_frame = false);
                    window::invalidate(hwnd);
                    log_debug!(SCOPE, "换帧：卸掉遮盖的旧内容（新内容占 {ratio}%）");
                }
                LRESULT(0)
            }
            // 光标形状（1.0.10）：无边框窗口 + 边缘拖拽，系统完全不知道"这里能拖"，
            // 必须自己换指针，否则用户看不出边可以抓。
            WM_SETCURSOR => {
                let over = window::cursor_pos()
                    .map(|(cx, cy)| {
                        let rc = window::window_rect(hwnd);
                        edges_at(rc, cx - rc.left, cy - rc.top, resize_margin(hwnd))
                    })
                    .unwrap_or_default();
                let dragging = with_state(|s| s.resize_edges).flatten().unwrap_or_default();
                let kind = if dragging.any() {
                    dragging.cursor()
                } else {
                    over.cursor()
                };
                if window::set_cursor(kind) {
                    LRESULT(1)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            // 窗口尺寸变了：**投递**一条消息，等这条同步呼叫退栈后再对齐系统预览处理器（1.7.9）。
            //
            // 症状：把 Office 预览窗拉宽，右边/下边留一条白（用户反馈「窗口伸缩会引起白边」）。
            // 原因：内容其实是处理器自己建的子窗口，不会跟着我们的窗口变（窗口树
            // `QLRust.ChildHost → EXCEL9 → EXCEL7`，宿主撑大了，`EXCEL7` 还是旧尺寸）。
            // 为什么不能在这里直接干：`SetWindowPos` 派发的 `WM_SIZE` 在输入同步呼叫里，
            // 那时进 COM 会被系统拒（0x8001010D），见 `ipc::WM_QLR_SHELLFIT` 的注释。
            // 拖边过程中不同步：每像素一次重开太慢，`finish_resize` 里再投一次。
            WM_SIZE => {
                if with_state(|s| s.resize_edges.is_none()).unwrap_or(true) {
                    window::post(hwnd, ipc::WM_QLR_SHELLFIT);
                }
                // 投影是另一个分层窗口，尺寸变了必须自己跟一次：不然改完大小投影还停在上一个
                // 尺寸上（看起来像"画框和阴影对不上"，用户 1.8.0 报）。拖动中限流到 8 ms。
                sync_shadow_with(true);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            // 点窗口就是要"激活它"（1.7.8）。
            //
            // 背景：Word/Excel 的预览是 OLE 就地激活对象，**容器不活、它就不收滚轮**
            // （用户实测：点一下预览窗再滚就能滚，且要求"点一下就立刻把焦点放进去"）。
            // 这里返回 MA_ACTIVATE(1) 让窗口正常激活，并把焦点交给处理器的内容子窗口，
            // 这样滚轮/滚动条/选区立刻可用，不用用户再点一次。
            //
            // 1.7.9 修：原来返回的是 2 = `MA_ACTIVATEANDEAT`（激活**并吞掉**这次点击）——
            // 于是"未激活的窗口上的第一次点击"永远到不了 `WM_LBUTTONDOWN`，拖边改大小
            // 只有第二次才生效（用户报的「拖边失效」）。
            WM_MOUSEACTIVATE => {
                // 这里**不**把焦点转交处理器（1.8.1）：`IPreviewHandler::SetFocus` 是 COM 调用，
                // 会把消息泵回我们自己，正好把"这次点击"吃掉——用户要连点两下才能选中单元格。
                // 焦点改在后面的 `WM_ACTIVATE` / `WM_SETFOCUS` 里给（那两条一定在点击落地前到）。
                LRESULT(1) // MA_ACTIVATE
            }
            // 点预览窗会先激活它、再给它焦点（1.7.8）：这两处都要把焦点转交给处理器的
            // 内容子窗口，否则 Excel/Word 的滚轮仍然不动（实测：`WM_MOUSEACTIVATE` 有时压根不来，
            // 但 `WM_ACTIVATE` / `WM_SETFOCUS` 一定会来）。
            WM_ACTIVATE => {
                if (wparam.0 & 0xFFFF) != 0 {
                    focus_shell_content();
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_SETFOCUS => {
                focus_shell_content();
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1.8.3：开窗尺寸必须随 DPI 变，且"宽随 DPI、高随工作区比例"这条口径不能被改坏。
    ///
    /// 这是 DPI 改动唯一能自动断言的部分（视觉要靠 `scripts\baseline-shot.ps1` +
    /// 用户真机）。它同时锁住两个曾经的坑：
    /// - 文本窗宽度要 ×DPI（否则 200% 下只有 440 DIP 宽的一条缝）；
    /// - 文本窗高度**不能** ×DPI（高度 = 工作区 × height_ratio，卡里写的"2478 px"
    ///   就是这么算错的：1080 高的屏上根本放不下）。
    #[test]
    fn window_size_scales_with_dpi() {
        // 用一块比窗口大得多的"工作区"，免得 0.92 的上限夹住宽度，让 2× 关系可断言。
        let work = RECT {
            left: 0,
            top: 0,
            right: 3840,
            bottom: 2160,
        };
        let cfg = Config::default();

        dpi::seed_thread(96);
        let text96 = window_size_for(work, format::Kind::Text, None, &cfg, None);
        let pdf96 = window_size_for(work, format::Kind::Pdf, Some((794, 1123)), &cfg, None);

        dpi::seed_thread(192);
        let text192 = window_size_for(work, format::Kind::Text, None, &cfg, None);
        let pdf192 = window_size_for(work, format::Kind::Pdf, Some((794, 1123)), &cfg, None);
        dpi::seed_thread(96); // 别把 DPI 留给同线程的其它测试

        assert_eq!(text192.0, text96.0 * 2, "文本窗宽度应该 ×DPI");
        assert_eq!(text192.1, text96.1, "文本窗高度按工作区比例，不随 DPI 变");
        // PDF 的自然尺寸也是 ×DPI 的：200% 下一页 A4 明显更大（受工作区 86% 夹持，
        // 所以只能断言"明显变大"，不能断言恰好 2 倍）。
        assert!(
            pdf192.0 as f64 >= pdf96.0 as f64 * 1.5,
            "PDF 200% 下应该明显变大：{} -> {}",
            pdf96.0,
            pdf192.0
        );
        assert!(pdf192.1 >= pdf96.1, "PDF 高度也不该变小");
    }

    /// 滚动位置永远落在 [0, total - rows]：不能滚成负数，也不能滚过头看不到最后一行。
    #[test]
    fn scroll_clamp_stays_in_range() {
        // 100 行、可见 10 行 → 最大偏移 90
        assert_eq!(scroll_clamp(0, 0, 100, 10), 0);
        assert_eq!(scroll_clamp(0, 5, 100, 10), 5);
        assert_eq!(scroll_clamp(0, -5, 100, 10), 0);
        assert_eq!(scroll_clamp(90, 10, 100, 10), 90);
        assert_eq!(scroll_clamp(50, 1_000, 100, 10), 90);
        // 内容比窗口还短：永远 0
        assert_eq!(scroll_clamp(7, 3, 5, 10), 0);
        // 空文件不 panic
        assert_eq!(scroll_clamp(0, 3, 0, 10), 0);
    }

    /// 目录高亮跟随滚动：取最后一个起始行 ≤ scroll 的小节。
    #[test]
    fn toc_highlight_follows_scroll() {
        let items = vec![
            (1u8, "一".to_string(), 0usize),
            (2u8, "1.1".to_string(), 10),
            (2u8, "1.2".to_string(), 20),
        ];
        assert_eq!(toc_following(&items, 0), Some(0));
        assert_eq!(toc_following(&items, 9), Some(0));
        assert_eq!(toc_following(&items, 10), Some(1));
        assert_eq!(toc_following(&items, 19), Some(1));
        assert_eq!(toc_following(&items, 999), Some(2));
        // 空目录不选出任何项
        assert_eq!(toc_following(&[], 5), None);
    }

    #[test]
    fn wheel_notches_clamps_and_ignores_jitter() {
        assert_eq!(wheel_notches(0), 0);
        // 触控板的细碎增量不该触发缩放
        assert_eq!(wheel_notches(60), 0);
        assert_eq!(wheel_notches(-60), 0);
        assert_eq!(wheel_notches(120), 1);
        assert_eq!(wheel_notches(-240), -2);
        // 触控板甩一下可能几百格，必须钳到 4
        assert_eq!(wheel_notches(12000), 4);
        assert_eq!(wheel_notches(-12000), -4);
    }

    /// 1.8.3 真实事故：高分辨率滚轮/精密触控板每次只发 30–60，
    /// `delta / 120` 恒为 0 → 滚轮在整台机器上"完全失效"（不缩放、不滚动、日志无记录）。
    /// 修法是把零头攒起来：4 次 30 = 1 档。
    #[test]
    fn wheel_residual_accumulates_into_notches() {
        let mut acc = 0;
        // 触控板细碎增量：前三发不动，第四发凑满一格
        assert_eq!(wheel_take(&mut acc, -30), 0);
        assert_eq!(wheel_take(&mut acc, -30), 0);
        assert_eq!(wheel_take(&mut acc, -30), 0);
        assert_eq!(wheel_take(&mut acc, -30), -1, "4×30=120，正好一档");
        assert_eq!(acc, 0, "整档用掉之后零头要清零");

        // 普通滚轮一格一档，行为不变
        assert_eq!(wheel_take(&mut acc, 120), 1);
        assert_eq!(acc, 0);

        // 触控板甩一下：钳到 4 档，且零头清零（否则松手后还会继续滚）
        assert_eq!(wheel_take(&mut acc, 12000), 4);
        assert_eq!(acc, 0);

        // 方向反转不能卡住
        assert_eq!(wheel_take(&mut acc, 60), 0);
        assert_eq!(wheel_take(&mut acc, -60), 0, "先 +60 再 -60 应该回到 0");
        assert_eq!(acc, 0);
        assert_eq!(wheel_take(&mut acc, -60), 0);
        assert_eq!(wheel_take(&mut acc, -60), -1, "反向攒够一格要能出档");
    }

    #[test]
    fn client_xy_sign_extends_both_halves() {
        // 低 16 位是 x、高 16 位是 y，两者都是有符号的（负坐标不能靠 & 0xFFFF）
        let pack = |x: i32, y: i32| -> isize {
            (((y as u32) & 0xFFFF) << 16 | ((x as u32) & 0xFFFF)) as i64 as isize
        };
        assert_eq!(client_xy(LPARAM(pack(120, 80))), (120, 80));
        assert_eq!(client_xy(LPARAM(pack(-5, -7))), (-5, -7));
        assert_eq!(client_xy(LPARAM(pack(0, 0))), (0, 0));
    }

    #[test]
    fn human_size_switches_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn extension_is_reported() {
        assert_eq!(extension_of(Path::new(r"C:\a\b.PNG")), ".PNG");
        assert_eq!(extension_of(Path::new(r"C:\a\b")), "（无扩展名）");
    }

    #[test]
    fn scaled_keeps_pixels_sane() {
        assert_eq!(scaled(100, 1.0), 100);
        assert_eq!(scaled(100, 2.0), 200);
        // 极端缩小不能变 0（位图宽度 0 是非法状态）
        assert_eq!(scaled(1, 0.001), 1);
        // 再大也封顶，避免一次渲染吃掉几百 MB
        assert_eq!(scaled(20_000, 8.0), 16_384);
    }

    /// 滚轮方向：往上滚必须看上面的内容（1.0.4 修的反向 bug）。
    #[test]
    fn wheel_up_scrolls_towards_the_top() {
        // 向上滚（notches > 0）→ 负数 → scroll 变小 → 显示更前面的行
        assert_eq!(wheel_text_rows(1), -3);
        assert_eq!(wheel_text_rows(4), -12);
        // 向下滚 → 正数
        assert_eq!(wheel_text_rows(-1), 3);
        assert_eq!(wheel_text_rows(0), 0);
        // 和"拖拽向上 = 往下读"方向相反，这才是所有编辑器的习惯
        assert!(wheel_text_rows(1) < 0, "向上滚不能往下翻");
    }

    /// 文本拖拽滚动：像素攒够一行才滚（慢速拖动不能一步不动，也不能一抖滚十行）。
    #[test]
    fn pixels_accumulate_into_rows() {
        let mut residual = 0;
        // 24 px 一行：不到一行先攒着，滚 0 行
        assert_eq!(pixels_to_rows(&mut residual, 10), 0);
        assert_eq!(residual, 10);
        // 再动 14 px 凑满 24 → 滚 1 行，零头清零
        assert_eq!(pixels_to_rows(&mut residual, 14), 1);
        assert_eq!(residual, 0);
        // 一次拖 100 px → 滚 4 行，余 4 px
        assert_eq!(pixels_to_rows(&mut residual, 100), 4);
        assert_eq!(residual, 4);
        // 往回拖同样按行算（负号保留），零头跟着走
        assert_eq!(pixels_to_rows(&mut residual, -28), -1);
        assert_eq!(residual, 0);
    }

    /// 右键拖动 = 缩放：上拖变大、下拖变小，且不越界。
    #[test]
    fn right_drag_scales_both_ways() {
        // 往上拖 60 px = 一档 1.25×
        assert!((zoom_drag_target(1.0, 60.0) - 1.25).abs() < 0.001);
        // 往下拖 60 px = 一档缩小
        assert!((zoom_drag_target(1.0, -60.0) - 0.8).abs() < 0.001);
        // 不动就不变
        assert!((zoom_drag_target(1.5, 0.0) - 1.5).abs() < 0.001);
        // 极端拖动被 ZOOM_MIN/ZOOM_MAX 夹住
        assert_eq!(zoom_drag_target(1.0, 10_000.0), ZOOM_MAX);
        assert_eq!(zoom_drag_target(1.0, -10_000.0), ZOOM_MIN);
    }

    #[test]
    fn page_keys_map_to_direction() {
        assert_eq!(page_delta_for(0x22), Some(1)); // PageDown
        assert_eq!(page_delta_for(0x27), Some(1)); // →
        assert_eq!(page_delta_for(0x21), Some(-1)); // PageUp
        assert_eq!(page_delta_for(0x25), Some(-1)); // ←
        assert_eq!(page_delta_for(0xBB), None); // + 是缩放，不是翻页
    }

    #[test]
    fn zoom_limits_are_sane() {
        // 编译期就该拦住"缩放范围写反了"这种事
        const _: () = assert!(ZOOM_MIN < 1.0 && ZOOM_MAX > 1.0);
        const _: () = assert!(ZOOM_MAX <= 16.0);
    }

    #[test]
    fn build_lines_without_target_mentions_usage() {
        let lines = build_lines(None);
        assert!(lines.iter().any(|l| l.contains("qlrust")));
        assert!(lines[0].contains("QLRust"));
    }

    #[test]
    fn build_lines_with_missing_file_reports_error() {
        let lines = build_lines(Some(Path::new(r"Z:\nope\missing.bin")));
        assert!(lines.iter().any(|l| l.contains("无法读取文件信息")));
    }

    #[test]
    fn unsupported_kind_placeholder_points_to_plugins() {
        // 1.0.10 起不支持的文件根本不会画占位（只响铃），但 `build_lines` 仍要
        // 在"插件接管了"的时候把插件信息写出来，方便排查。
        let lines = build_lines(Some(Path::new(r"C:\x\a.mp4")));
        assert!(lines[1].contains("图片"), "横幅应列出四类：{lines:?}");
    }

    /// 窗口位置记忆（1.0.13）：记忆的坐标可能来自另一台显示器，摆位置时必须夹进工作区，
    /// 而且**只挪位置、不改尺寸**（尺寸是内容决定的）。
    #[test]
    fn place_at_moves_without_resizing() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let mut rect = RECT {
            left: 400,
            top: 300,
            right: 1200,
            bottom: 800,
        };
        place_at(&mut rect, Some((100, 60)), work);
        assert_eq!(
            (
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top
            ),
            (100, 60, 800, 500),
            "记住的位置照搬，尺寸不变"
        );

        // 记在屏幕外（换过显示器）→ 整体挪回工作区，尺寸仍然不变。
        let mut rect = RECT {
            left: 400,
            top: 300,
            right: 1200,
            bottom: 800,
        };
        place_at(&mut rect, Some((-500, -400)), work);
        assert_eq!(
            (
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top
            ),
            (0, 0, 800, 500),
            "越界后夹回工作区左上角"
        );

        // 没有记忆 → 一个字都不改（调用方算好的居中位置保留）。
        let mut rect = RECT {
            left: 400,
            top: 300,
            right: 1200,
            bottom: 800,
        };
        place_at(&mut rect, None, work);
        assert_eq!((rect.left, rect.top), (400, 300), "没记忆就不动");
    }

    #[test]
    fn content_size_of_subtracts_chrome_height() {
        // 顶栏占 `window::chrome_height()`，内容区 = 客户区减去它（渲染尺寸与 `base` 都按内容区算）。
        let client = RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 600,
        };
        let (w, h) = content_size_of(client);
        assert_eq!(w, 800);
        assert_eq!(h, (600 - window::chrome_height()) as u32);
        // 窗口被夹到极矮时不能出 0（0 高会让渲染线程直接报错）。
        let tiny = RECT {
            left: 0,
            top: 0,
            right: 10,
            bottom: 5,
        };
        assert_eq!(content_size_of(tiny), (10, 1));
    }

    /// 取"内容区"尺寸（窗口矩形扣掉顶栏），测试里反复用。
    fn content_of(rc: RECT) -> (i32, i32) {
        (
            rc.right - rc.left,
            rc.bottom - rc.top - window::chrome_height(),
        )
    }

    #[test]
    fn media_window_size_locks_to_picture_aspect() {
        // 1.8.1（用户口径）：图片窗口的比例 = 图片比例，**记忆的尺寸也要夹回来**，
        // 否则上次把窗口拖成方形，下次开图又会"图片在方窗口里留白/变形"。
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let config = Config::default();
        for remembered in [None, Some((900, 900)), Some((300, 900))] {
            let (w, h) = window_size_for(
                work,
                format::Kind::Image,
                Some((1280, 720)),
                &config,
                remembered,
            );
            let rc = RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            };
            let (cw, ch) = content_of(rc);
            let want = (cw as f64 * 720.0 / 1280.0).round() as i32;
            assert!(
                (ch - want).abs() <= 1,
                "记忆 {remembered:?}：内容区 {cw}x{ch} 不是 16:9（期望高 {want}）"
            );
        }
        // 非图片格式不受影响（文本只看宽度，保持原样）。
        let (_, h) = window_size_for(work, format::Kind::Text, None, &config, Some((900, 900)));
        assert_eq!(h, 900);
        // 视频（1.8.1 同图片口径）：比例锁死，但要额外留出底部进度条的高度。
        let (w, h) = window_size_for(work, format::Kind::Video, Some((1280, 720)), &config, None);
        let content_h = h - window::chrome_height() - window::audio_bar_h();
        let want = (w as f64 * 720.0 / 1280.0).round() as i32;
        assert!(
            (content_h - want).abs() <= 1,
            "视频内容区 {w}x{content_h} 不是 16:9（期望高 {want}）"
        );
    }

    #[test]
    fn clamp_media_resize_drives_width_from_dragged_edge() {
        // 拖右边：高度跟着比例长，**左边不动**，垂直居中。
        let rc = RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 630,
        };
        let edges = ResizeEdges {
            right: true,
            ..ResizeEdges::default()
        };
        let out = clamp_media_resize(rc, edges, 2.0, 0);
        assert_eq!(content_of(out), (800, 400));
        assert_eq!(out.left, 0, "拖右边时左边界不能动");
        // 总高 = 400 + 顶栏，垂直居中：上下各让一半。
        let total = out.bottom - out.top;
        assert_eq!(out.top, 315 - total / 2);
    }

    #[test]
    fn clamp_media_resize_drives_height_when_only_vertical_edges_drag() {
        // 只拖下边：宽度由高度反算（600 高 × 2.0 = 1200 宽），水平居中。
        let rc = RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 630,
        };
        let edges = ResizeEdges {
            bottom: true,
            ..ResizeEdges::default()
        };
        let out = clamp_media_resize(rc, edges, 2.0, 0);
        assert_eq!(content_of(out), (1200, 600));
        assert_eq!(out.top, 0, "拖下边时上边界不能动");
        assert_eq!(out.left, 400 - 600);
    }

    #[test]
    fn clamp_media_resize_anchors_opposite_edge() {
        // 拖左边：右边界钉住，窗口往右"长"。
        let rc = RECT {
            left: 100,
            top: 0,
            right: 800,
            bottom: 630,
        };
        let edges = ResizeEdges {
            left: true,
            ..ResizeEdges::default()
        };
        let out = clamp_media_resize(rc, edges, 2.0, 0);
        assert_eq!(content_of(out), (700, 350));
        assert_eq!(out.right, 800, "拖左边时右边界不能动");
        // 比例非法（0 / NaN）时原样返回，绝不把窗口算成 0 宽。
        assert_eq!(clamp_media_resize(rc, edges, 0.0, 0), rc);
        assert_eq!(clamp_media_resize(rc, edges, f64::NAN, 0), rc);
    }

    #[test]
    fn clamp_media_resize_reserves_video_bar() {
        // 视频（1.8.1 与图片同口径）：总高 = 内容高 + 顶栏 + 进度条。
        // 拖下边 → 内容高驱动宽度，比例仍然严格（宽度 = 内容高 × 2）。
        let rc = RECT {
            left: 0,
            top: 0,
            right: 640,
            bottom: 400,
        };
        let edges = ResizeEdges {
            bottom: true,
            ..ResizeEdges::default()
        };
        let out = clamp_media_resize(rc, edges, 2.0, window::audio_bar_h());
        let content_h = out.bottom - out.top - window::chrome_height() - window::audio_bar_h();
        assert_eq!(out.right - out.left, content_h * 2);
        assert_eq!(out.top, 0, "拖下边时上边界不能动");
    }

    #[test]
    fn edges_at_detects_borders_and_corners() {
        // 热区宽度按 DPI 算（1.7.4），测试里固定成 100% 缩放时的 8 px。
        const MARGIN: i32 = 8;
        let rc = RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 600,
        };
        // 正中：不命中任何边。
        assert!(!edges_at(rc, 400, 300, MARGIN).any());
        // 四条边。
        assert_eq!(
            edges_at(rc, 2, 300, MARGIN),
            ResizeEdges {
                left: true,
                ..Default::default()
            }
        );
        assert_eq!(
            edges_at(rc, 798, 300, MARGIN),
            ResizeEdges {
                right: true,
                ..Default::default()
            }
        );
        assert_eq!(
            edges_at(rc, 400, 1, MARGIN),
            ResizeEdges {
                top: true,
                ..Default::default()
            }
        );
        assert_eq!(
            edges_at(rc, 400, 598, MARGIN),
            ResizeEdges {
                bottom: true,
                ..Default::default()
            }
        );
        // 左上角：两条边同时命中。
        let corner = edges_at(rc, 1, 1, MARGIN);
        assert!(corner.left && corner.top && !corner.right && !corner.bottom);
        // 窗口外太远：不算（否则在别处点一下就开始改大小）。
        assert!(!edges_at(rc, 400, 900, MARGIN).any());
        // 热区变宽（高 DPI 屏）：原来够不着的点现在算命中。
        // 800 宽的窗口、margin=12 → x=790 落在右边热区里。
        assert!(edges_at(rc, 790, 300, 12).right);
        assert!(!edges_at(rc, 790, 300, MARGIN).right);
    }

    #[test]
    fn resize_edges_pick_the_matching_cursor() {
        let left = ResizeEdges {
            left: true,
            ..Default::default()
        };
        assert_eq!(left.cursor(), window::CursorKind::SizeWE);
        let top = ResizeEdges {
            top: true,
            ..Default::default()
        };
        assert_eq!(top.cursor(), window::CursorKind::SizeNS);
        // 左上 / 右下 = NWSE；右上 / 左下 = NESW。
        let nw = ResizeEdges {
            left: true,
            top: true,
            ..Default::default()
        };
        assert_eq!(nw.cursor(), window::CursorKind::SizeNWSE);
        let se = ResizeEdges {
            right: true,
            bottom: true,
            ..Default::default()
        };
        assert_eq!(se.cursor(), window::CursorKind::SizeNWSE);
        let ne = ResizeEdges {
            right: true,
            top: true,
            ..Default::default()
        };
        assert_eq!(ne.cursor(), window::CursorKind::SizeNESW);
        assert_eq!(ResizeEdges::default().cursor(), window::CursorKind::Arrow);
        assert!(!ResizeEdges::default().any());
    }

    /// 1.8.2：图片/视频不响应拖边改大小（`resize_margin` 返回 0 → 这里必须一条边都不命中）。
    #[test]
    fn edges_at_no_resize_for_media() {
        let rc = RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 600,
        };
        // margin = 0 时，就算鼠标正好压在边上也不该抓边。
        for (x, y) in [
            (0, 300),
            (800, 300),
            (400, 0),
            (400, 600),
            (0, 0),
            (800, 600),
        ] {
            assert!(
                !edges_at(rc, x, y, 0).any(),
                "({x},{y}) 不该命中边缘（margin=0）"
            );
        }
        assert!(edges_at(rc, 798, 300, 8).any(), "正常窗口仍然要能拖边");
    }

    /// 1.8.2：只有图片/视频取消拖边改大小，其它类型照旧。
    #[test]
    fn resizable_kind_excludes_image_and_video() {
        assert!(!resizable_kind(Some(format::Kind::Image)));
        assert!(!resizable_kind(Some(format::Kind::Video)));
        assert!(resizable_kind(Some(format::Kind::Pdf)));
        assert!(resizable_kind(Some(format::Kind::Text)));
        assert!(resizable_kind(None));
    }

    // ---------------------------------------------------------- 压缩包目录树（1.7.3）

    fn entry(path: &str, size: u64, is_dir: bool) -> qlr_render::archive::Entry {
        qlr_render::archive::Entry {
            path: path.to_string(),
            size,
            time: String::new(),
            is_dir,
            depth: path.matches('/').count(),
        }
    }

    /// 展平结果：`(名字, 层级)`，方便逐行断言。
    fn flat_names(nodes: &[ArchiveNode], roots: &[usize]) -> Vec<(String, usize)> {
        flatten_archive(nodes, roots)
            .into_iter()
            .map(|(id, depth)| (nodes[id].name.clone(), depth))
            .collect()
    }

    /// 目录行必须排在子项之前、缩进逐级 +1，且折叠状态会被展平尊重。
    #[test]
    fn archive_tree_nests_dirs_before_their_children() {
        let entries = vec![
            entry("a/", 0, true),
            entry("a/b/", 0, true),
            entry("a/b/x.txt", 10, false),
            entry("a/y.txt", 20, false),
            entry("root.txt", 30, false),
        ];
        let (mut nodes, roots) = build_archive_tree(&entries);
        assert_eq!(
            flat_names(&nodes, &roots),
            vec![
                ("a".to_string(), 0),
                ("b".to_string(), 1),
                ("x.txt".to_string(), 2),
                ("y.txt".to_string(), 1),
                ("root.txt".to_string(), 0),
            ]
        );
        // 折叠 "a"（roots 的第一个）之后，它下面三行必须一起消失。
        let a = roots[0];
        nodes[a].expanded = false;
        assert_eq!(
            flat_names(&nodes, &roots),
            vec![("a".to_string(), 0), ("root.txt".to_string(), 0)]
        );
    }

    /// 打包器常常不写目录条目（`Compress-Archive`、`python zipfile`），
    /// 父目录链必须在建树时补出来，否则 `src/core/main.rs` 会变成三行平铺。
    #[test]
    fn archive_tree_invents_missing_parent_dirs() {
        let entries = vec![
            entry("src/core/main.rs", 100, false),
            entry("LICENSE", 5, false),
        ];
        let (nodes, roots) = build_archive_tree(&entries);
        assert_eq!(
            flat_names(&nodes, &roots),
            vec![
                ("src".to_string(), 0),
                ("core".to_string(), 1),
                ("main.rs".to_string(), 2),
                ("LICENSE".to_string(), 0),
            ]
        );
        assert!(nodes[roots[0]].is_dir, "补出来的 src 必须是目录行");
    }

    /// 目录在前、同类按名字（大小写不敏感）排：跟资源管理器的顺序一致。
    #[test]
    fn archive_tree_sorts_dirs_then_names() {
        let entries = vec![
            entry("zeta.txt", 1, false),
            entry("Alpha.txt", 1, false),
            entry("mid/", 0, true),
            entry("beta/", 0, true),
        ];
        let (nodes, roots) = build_archive_tree(&entries);
        assert_eq!(
            flat_names(&nodes, &roots),
            vec![
                ("beta".to_string(), 0),
                ("mid".to_string(), 0),
                ("Alpha.txt".to_string(), 0),
                ("zeta.txt".to_string(), 0),
            ]
        );
    }

    /// 点击反算行号：口径必须和 `paint_list` 一致（内容区顶 + `window::text_padding_top()`，行高 `window::list_row_height()`）。
    #[test]
    fn archive_row_index_matches_paint_list_metrics() {
        let content = RECT {
            left: 0,
            top: 30,
            right: 800,
            bottom: 748,
        };
        let top = 30 + window::text_padding_top();
        assert_eq!(archive_row_index(content, 0, top), Some(0));
        assert_eq!(
            archive_row_index(content, 0, top + window::list_row_height() - 1),
            Some(0)
        );
        assert_eq!(
            archive_row_index(content, 0, top + window::list_row_height()),
            Some(1)
        );
        // 滚动 10 行后，同样的 y 对应第 10 行。
        assert_eq!(archive_row_index(content, 10, top), Some(10));
        // 点在内容区之上（顶栏）或之下（窗口底）都不算行。
        assert_eq!(archive_row_index(content, 0, top - 1), None);
        assert_eq!(archive_row_index(content, 0, 748), None);
    }

    /// 1.8.3 自愈：判挂阈值必须"起步 10 s、连续自愈翻倍、封顶 40 s"。
    ///
    /// 这条锁住三件事：① 阈值不能偷偷改小（会把慢盘/大图误判成挂死）；
    /// ② 连续重建必须退避（否则新线程也挂死时变成每 10 s 死一条的连环抖动）；
    /// ③ 退避有上限（否则挂死次数一多，阈值涨到几分钟，用户以为程序又"永久失效"了）。
    #[test]
    fn hang_threshold_backs_off_then_caps() {
        assert_eq!(hang_threshold(0), Duration::from_millis(10_000));
        assert_eq!(hang_threshold(1), Duration::from_millis(20_000));
        assert_eq!(hang_threshold(2), Duration::from_millis(40_000));
        assert_eq!(
            hang_threshold(9),
            Duration::from_millis(40_000),
            "封顶后不再翻倍"
        );
    }

    /// 1.8.3 自愈日志：任务描述带文件名/类型/尺寸/页码，但**不许**带上完整用户路径。
    #[test]
    fn job_desc_has_no_user_path() {
        let job = RenderJob {
            generation: 7,
            path: PathBuf::from(r"C:\Users\someone\Desktop\report.pdf"),
            max_width: 800,
            max_height: 1000,
            page: 2,
            upscale: false,
        };
        let desc = describe_job(&job);
        assert!(desc.contains("report.pdf"), "要有文件名：{desc}");
        assert!(desc.contains("800x1000"), "要有目标尺寸：{desc}");
        assert!(
            desc.contains("第3页"),
            "PDF 要带页码（0 基转 1 基）：{desc}"
        );
        assert!(
            !desc.contains("someone"),
            "日志不许出现完整用户路径：{desc}"
        );
    }

    /// 造一帧"占 `mb` 兆字节"的位图：`canvas_bytes` 只看 `bgra.len()`。
    fn fake_frame(mb: usize, tag: u8) -> Arc<Canvas> {
        let bytes = mb * 1024 * 1024;
        Arc::new(Canvas::Bitmap(qlr_render::Bitmap {
            width: 1024,
            height: (bytes / 4 / 1024) as u32,
            bgra: vec![tag; bytes],
        }))
    }

    fn fake_key(n: usize) -> FrameKey {
        FrameKey {
            path: PathBuf::from(format!("f{n}.png")),
            mtime: 0,
            size: 0,
            page: 0,
            width: 1024,
            height: 1024,
            upscale: false,
        }
    }

    /// 1.8.3：上一帧缓存同时受"帧数 ≤ 3"和"总字节 ≤ 24 MB"两条上限约束，
    /// 且单帧就超预算时仍保留最近 1 帧（"同一文件重开秒开"这条收益不能被限额吃掉）。
    #[test]
    fn last_frame_cache_respects_byte_budget() {
        // 3 帧 × 10 MB = 30 MB > 24 MB → 只能留 2 帧，且被淘汰的必须是最旧那帧。
        let mut frames: LastFrames = std::collections::VecDeque::new();
        for i in 0..3 {
            frames.push_back((fake_key(i), fake_frame(10, i as u8)));
        }
        trim_last_frames(&mut frames);
        assert_eq!(frames.len(), 2, "24 MB 预算下 10 MB 的帧最多留 2 帧");
        assert_eq!(
            frames.back().expect("至少要留一帧").0.path,
            PathBuf::from("f2.png"),
            "最新的那帧不许被淘汰"
        );

        // 单帧 30 MB 已超预算：留 1 帧；再来一帧 30 MB 时淘汰旧的，仍留 1 帧。
        let mut frames: LastFrames = std::collections::VecDeque::new();
        frames.push_back((fake_key(0), fake_frame(30, 0)));
        trim_last_frames(&mut frames);
        assert_eq!(frames.len(), 1, "单帧超预算也要保住最近 1 帧");
        frames.push_back((fake_key(1), fake_frame(30, 1)));
        trim_last_frames(&mut frames);
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames.back().expect("至少要留一帧").0.path,
            PathBuf::from("f1.png")
        );

        // 小帧照旧受"最多 3 帧"约束（1 MB × 5 → 3）。
        let mut frames: LastFrames = std::collections::VecDeque::new();
        for i in 0..5 {
            frames.push_back((fake_key(i), fake_frame(1, i as u8)));
        }
        trim_last_frames(&mut frames);
        assert_eq!(frames.len(), LAST_FRAME_CAP);
    }
}
