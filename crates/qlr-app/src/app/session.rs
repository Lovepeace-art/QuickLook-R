//! 会话状态：`State` 与它的全局槽（锁 / `OnceLock`）+ 访问器。
//!
//! 几何、渲染、输入都从这里取状态；本模块只回答"现在是什么"，不做业务判断。

use super::*;

pub(crate) const SCOPE: &str = "app";
pub(crate) const PREVIEW_CLASS: &str = "QLRust.PreviewWindow";
/// 投影浮层的窗口类（1.0.10；分层窗口，永远贴在预览窗下面）。
pub(crate) const SHADOW_CLASS: &str = "QLRust.ShadowWindow";
pub(crate) const TRAY_ID: u32 = 1;

/// 托盘注册失败、还没告诉过用户（1.8.3）。
///
/// 为什么用"待办标记"而不是当场弹窗：托盘失败发生在 `State` 还没建好、预览窗
/// 还没显示的时候（裸启动），此刻既没有可以挂 toast 的窗口，也不该阻塞启动。
/// 所以只记一笔，等第一次真的把窗口显示出来时用顶栏 toast 说一次，然后清掉——
/// 每次打开都提示会把预览变成噪音。
pub(crate) static TRAY_FAILURE_NOTICE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 放大镜默认倍数与上下限（Shift + 滚轮每一格 ±1）。
pub(crate) const LENS_DEFAULT_ZOOM: f32 = 3.0;
pub(crate) const LENS_MIN_ZOOM: f32 = 1.5;
pub(crate) const LENS_MAX_ZOOM: f32 = 8.0;

/// 跟随选中项的轮询定时器（1.0.10）。
///
/// 为什么轮询而不是等通知：资源管理器的选中项只有 `IShellWindows` 能问，
/// 而它没有可订阅的变更事件（`DShellWindowsEvents` 只报窗口的开关，不报选中项）。
/// 400 ms 是"跟得上手"和"别把 CPU 拖起来"的折中（每次只是一次类名比较，
/// 前台不是资源管理器时连 COM 都不碰）。
pub(crate) const TIMER_FOLLOW: usize = 0x71;
pub(crate) const FOLLOW_INTERVAL_MS: u32 = 400;
/// 渲染看门狗（1.2.0）：250 ms 看一眼"这一帧跑多久了"。
pub(crate) const TIMER_WATCHDOG: usize = 0x72;
pub(crate) const WATCHDOG_INTERVAL_MS: u32 = 250;
/// 顶栏一次性提示的擦除定时器（1.7.4）。
pub(crate) const TIMER_TOAST: usize = 0x73;
/// 接管系统预览处理器输入的延时定时器（1.7.4）。
pub(crate) const TIMER_SHELL_INPUT: usize = 0x74;
/// 换文件时"旧内容还要盖多久"（1.7.6）：到点卸掉旧的，露出新的第一帧。
pub(crate) const TIMER_SHELL_SWAP: usize = 0x75;
/// 音频进度条刷新（1.7.9）：只在播放时开着，暂停/收起就杀掉。
pub(crate) const TIMER_AUDIO: usize = 0x76;
/// 进度条刷新间隔（毫秒）。250 ms 对"看进度条"足够顺，又不会为了它整窗重画。
pub(crate) const AUDIO_TICK_MS: u32 = 250;
/// 音频快进/快退步长（毫秒）：左右键一次 5 秒（与常见播放器的方向键粒度一致）。
pub(crate) const AUDIO_SEEK_STEP_MS: i64 = 5_000;
/// 视频逐帧播放的取帧定时器（1.7.9）。
pub(crate) const TIMER_VIDEO: usize = 0x77;
/// 视频取帧间隔（毫秒）。40 ms ≈ 25 fps：720p 解码一帧只要几毫秒，
/// 剩下的时间留给"到点该显示哪一帧"的判断（落后就跳帧，见 `video_tick`）。
pub(crate) const VIDEO_TICK_MS: u32 = 40;
/// 旧内容遮盖时长（毫秒）。实测 Word/PowerPoint 预览首帧在 120–260 ms 之间落地，
/// 取 260 让绝大多数情况"换的那一刻已经是新画面"。
pub(crate) const SHELL_SWAP_DELAY_MS: u32 = 260;

/// 轮询间隔（1.7.7）：等处理器"内容真的画出来"（见 `ShellPreview::visible_area`）。
///
/// 为什么不再死等固定 400 ms：Excel 的 `DoPreview` 实测要 **768 ms**，猜早了就露出占位页
/// （用户反馈"Excel 还有那个占位页"）。改成每 120 ms 看一眼，画出来就收尾，最多等 3 s。
pub(crate) const SHELL_POLL_MS: u32 = 120;
/// 最多轮询多少次（120 ms × 25 ≈ 3 s）——和渲染看门狗同一个数量级。
pub(crate) const SHELL_POLL_MAX: u32 = 25;
/// 内容区四周留给"拖边改大小"的那一圈像素（1.7.7，1.7.8 从 10 收到 4）。
///
/// 只有不接管输入的处理器（Word/Excel…）才内缩：子窗口让出这一圈，
/// 窗口四条边就归我们，拖动改大小照样能用，而内容区里的滚动条/选区仍是处理器自己的。
pub(crate) const SHELL_RING_DIP: i32 = 4;
/// 超过这个时间就认为"这帧不正常"，记一条 warn 并请求取消（协作式，见 `qlr_render::cancel`）。
/// 定 3 s 的理由：本机最慢的正常帧（200 页 PDF 首页、48MP JPEG）都在 100 ms 量级，
/// 3 s 已是 30 倍；再久基本就是解析器卡住或文件躺在慢速网络盘上。
pub(crate) const RENDER_TIMEOUT_MS: u128 = 3000;

/// 渲染线程"挂死"的判定阈值（1.8.3，自愈用）。
///
/// 和上面的 `RENDER_TIMEOUT_MS` 是两件事：3 s 那条只是"记 warn + 请求取消"（协作式，
/// 渲染器在阶段边界自己退出），线程本身还会接着做下一帧；这里回答的是"线程根本回不来了"。
///
/// 为什么是 10 s：本机最慢的正常帧（200 页 PDF 首页、48 MP JPEG）实测都在 100–400 ms，
/// 3 s 已经是 7–30 倍余量；真到 10 s 说明它卡死在某次不会返回的调用里（PDFium /
/// MediaFoundation / 插件 cdylib 的 FFI，或 COM）。阈值再短会把"慢速网络盘上读一个
/// 30 MB 扫描件"误判成挂死——每误判一次就白扔一条线程、重发一帧。
pub(crate) const RENDER_HANG_MS: u64 = 10_000;

/// 连续自愈的退避上限（1.8.3）：第 1 次 10 s、第 2 次 20 s、第 3 次起 40 s。
///
/// 为什么要退避：新线程如果也立刻挂死（PDFium 的全局锁还被旧线程攥着、插件里的静态锁
/// 没释放），固定 10 s 就变成"每 10 s spawn 一条死线程"的连环抖动。退避到 40 s 仍然会
/// 自愈，只是给旧线程留出"自己爬回来"的窗口；一旦有帧正常完成，计数立即清零（见
/// `end_lease`），退避不会累积到下一次偶发挂死上。
pub(crate) const RENDER_HANG_BACKOFF_MAX: u32 = 2;

/// 拖边改大小的热区宽度（DIP）。太小不好抓，太大会吃掉顶栏按钮。
pub(crate) const RESIZE_MARGIN_DIP: i32 = 6;
/// 拖边之后的窗口最小尺寸（DIP；再小就只剩顶栏，用户没法再拖回来）。
///
/// 1.8.3：这两个值改成"设计值 + DPI 换算"。200% 缩放下如果还按 240 物理像素算，
/// 最小窗口只有 120 DIP 宽——比顶栏上三个按钮还窄，拖到极限就再也拖不回来了。
pub(crate) const MIN_WINDOW_W_DIP: i32 = 240;
pub(crate) const MIN_WINDOW_H_DIP: i32 = 160;

/// 最小窗口宽（物理像素，随 DPI）。
pub(crate) fn min_window_w() -> i32 {
    dpi::px(MIN_WINDOW_W_DIP)
}

/// 最小窗口高（物理像素，随 DPI）。
pub(crate) fn min_window_h() -> i32 {
    dpi::px(MIN_WINDOW_H_DIP)
}

/// `HWND` 不是 `Send`，而全局状态需要 `Send`。项目是单线程 GUI，
/// 这个包装只用来把句柄跨函数传递，不代表可以在别的线程用。
#[derive(Clone, Copy)]
pub(crate) struct Hwnd(pub(crate) isize);

impl Hwnd {
    pub(crate) fn raw(self) -> HWND {
        HWND(self.0 as *mut core::ffi::c_void)
    }

    pub(crate) fn from_raw(hwnd: HWND) -> Self {
        Self(hwnd.0 as isize)
    }
}

pub(crate) struct State {
    pub(crate) config: Config,
    pub(crate) host: Hwnd,
    pub(crate) preview: Hwnd,
    /// 投影浮层（1.0.10）：跟随预览窗的位置与尺寸，收起时一起隐藏。
    pub(crate) shadow: Option<Hwnd>,
    pub(crate) visible: bool,
    pub(crate) last_path: Option<PathBuf>,
    /// 本次请求（热键/命令行/转交）开始处理的时刻。
    ///
    /// 注意这里**没有**"进程启动时刻"：常驻实例里那可能是几小时前，
    /// 拿它算"按下空格到出窗"的延迟会得到荒谬的数字（实测在常驻 6.5 s 的宿主上
    /// 报过 `elapsed=6583ms`）。延迟一律以这个字段为准；进程总运行时长用局部变量。
    pub(crate) request_started: Instant,
    /// 渲染代际号：每次请求自增。worker 结果带着它回来，代际不匹配就丢弃
    /// （用户连按空格翻文件时，上一张的解码结果不能盖掉当前这张）。
    pub(crate) render_gen: u64,
    /// 当前 PDF 页（0 起）；非 PDF 恒为 0。
    pub(crate) page: usize,
    /// PDF 总页数（非 PDF 为 None）。
    pub(crate) pages: Option<usize>,
    /// 缩放倍数：1.0 = 适应窗口。>1 时按放大后的像素尺寸重新渲染并允许平移。
    pub(crate) zoom: f32,
    /// 文本内容的滚动位置（顶部显示第几行，0 基）。
    pub(crate) scroll: usize,
    /// 上次绘制时当前窗口吃掉了多少**原始行**（折行后一屏 ≠ 一屏原始行）。
    /// 翻页按它前进，否则折行会让 PageDown 漏掉大半屏内容。
    pub(crate) wrapped_consumed: usize,
    /// 鼠标拖拽平移的起点（屏幕无关的客户区坐标），`None` = 没在拖。
    pub(crate) drag_from: Option<(i32, i32)>,
    /// 这一次按下是否已经拖动过（1.0.2 前用来区分"点击关闭 / 拖拽"，1.0.3 起只用于日志）。
    pub(crate) dragged: bool,
    /// 文本拖拽滚动攒下的零头像素（不够一行的高度时先记着，攒够再滚一行）。
    pub(crate) drag_residual: i32,
    /// 右键缩放的锚点：`(按下时的 y, 按下时的缩放倍数)`，`None` = 没在右键拖。
    pub(crate) zoom_anchor: Option<(i32, f32)>,
    /// 右键拖动**滚动文本**的锚点（屏幕 y）。文本没有缩放概念，右键上下拖就是滚动。
    pub(crate) scroll_anchor: Option<i32>,
    /// 缩放后的位图还没渲染回来：这段时间先用旧位图**拉伸**顶住（1.0.5）。
    ///
    /// 为什么要这样：窗口尺寸是立刻能改的，但按新尺寸重新光栅化要时间（PDF 一页
    /// 几十毫秒）。以前拖动缩放时每一帧都去重渲染，画面一顿一顿，用户的原话是
    /// "右键缩放的时候卡顿比较强，按逻辑应该先缩放窗口"。现在拖动期间只改窗口 +
    /// 拉伸预览，松手才渲染一张高清的。
    pub(crate) zoom_preview: bool,
    /// 平移偏移（位图像素，左上为原点的裁剪起点）。
    pub(crate) pan: (i32, i32),
    /// 当前内容的**自然尺寸**（图片像素 / PDF 首页像素），只用于日志与"原始尺寸"提示。
    pub(crate) natural: Option<(u32, u32)>,
    /// 开窗时的尺寸（= 自然尺寸等比缩进工作区之后的像素），**zoom 的基准**。
    ///
    /// 为什么要单独存一份：zoom = 1.0 的语义是"适应窗口"，不是"100% 原始像素"。
    /// 拿自然尺寸当基准会让 0.8× 这类缩小也超出工作区、结果窗口不变而画面被裁。
    pub(crate) base: Option<(u32, u32)>,
    /// 带样式的内容是否已经打过"样式分布"日志（每份内容只打一次，避免刷屏）。
    pub(crate) styled_logged: bool,
    /// 放大镜镜头中心（客户区坐标）；`None` = 没开。
    ///
    /// 为什么要放大镜：用户真正想要的是"看清素材里某个细节"。把整个窗口放大到超屏
    /// 既要拖窗口、又要拖画面，还得等重光栅化，体验反而差。放大镜直接放大**屏幕像素**，
    /// 一次 GDI 搬运就出来，按住 Shift 随手可看。
    pub(crate) lens: Option<(i32, i32)>,
    /// 放大镜倍数（Shift + 滚轮调，见 `LENS_MIN_ZOOM` / `LENS_MAX_ZOOM`）。
    pub(crate) lens_zoom: f32,
    /// 顶栏上鼠标悬停在哪个按钮上（决定底色与提示气泡）。
    pub(crate) hover_button: Option<window::ChromeButton>,
    /// 页码输入框里正在输入的内容；`None` = 没在编辑。
    pub(crate) page_edit: Option<String>,
    /// 滚轮进量的"零头"（1.8.3）。
    ///
    /// 为什么需要：一格滚轮是 120，但**高分辨率滚轮和精密触控板每次只发 30/60**。
    /// 1.8.2 的算法是 `delta / 120`，零头直接扔掉 → 每一次都算出 0 档 → 滚轮在这类设备上
    /// **彻底没反应**（缩放开不了、正文不滚、目录不滚，而且日志里一条记录都没有）。
    /// 现在把零头攒起来：4 次 30 = 1 档。实测复现与修复见 `wheel_take` 的单测。
    pub(crate) wheel_acc: i32,
    /// 目录栏滚动到第几条（1.8.3：目录可以独立滚动，所以**不需要滚动条**）。
    pub(crate) toc_scroll: usize,
    /// 鼠标当前是否悬停在目录栏上（决定滚轮滚目录还是滚正文）。
    pub(crate) hover_toc: bool,
    /// 是否正在拖右侧滚动条。
    pub(crate) scroll_drag: bool,
    /// 当前内容的目录（Markdown 才有）：`(级别, 标题, 显示行下标)`。
    ///
    /// 只保留这三样而不是整棵 AST：目录只需要"显示什么"和"跳到哪里"。
    pub(crate) toc: Vec<(u8, String, usize)>,
    /// 目录里鼠标悬停/选中的下标。
    pub(crate) toc_active: Option<usize>,
    /// "阻止关闭"是否已开启（1.0.10）。开启后空格/Esc 都关不掉，且**不能取消**。
    pub(crate) pinned: bool,
    /// 正在拖哪几条边改窗口大小（1.0.10 把拖边改大小加回来）。
    pub(crate) resize_edges: Option<ResizeEdges>,
    /// 拖边开始时的 `(鼠标屏幕坐标, 窗口矩形)`：改大小必须按"原始矩形 + 累计位移"算，
    /// 不能每帧累加增量——一旦被最小尺寸夹住，增量就会丢，窗口跟不上鼠标。
    pub(crate) resize_from: Option<(i32, i32, RECT)>,
    /// 图片/视频预览的**长宽比**（`宽 / 高`，1.8.1）。
    ///
    /// 这两类不跟着窗口"拉伸缩放"：窗口本身锁成素材的比例（`clamp_media_resize` /
    /// `window_size_for` 都按它算），于是内容区永远是"铺满"，不会出现留白或变形。
    /// `None` = 当前不是图片/视频（PDF/文本/Office/SVG 照旧自由改大小）。
    pub(crate) media_aspect: Option<f64>,
    /// 上一次预览窗口的左上角屏幕坐标（1.0.13 位置记忆）。
    ///
    /// 显隐之间要跨越进程不重启，所以真正的持久化在 `persist_position()`（写 `config.json`）；
    /// 这里只是"当前会话里最后一次已知位置"，用于窗口还开着时换文件不跳位。
    pub(crate) last_pos: Option<(i32, i32)>,
    /// 本次会话里各格式的窗口尺寸（键 = `format::Kind::name()`；1.0.14 尺寸记忆）。
    ///
    /// 与 `config.window_sizes` 分开：这里是"现在的"，配置里是"上次落盘的"；
    /// 收起/退出时比对两者，没变就不写盘。
    pub(crate) last_sizes: std::collections::BTreeMap<String, (i32, i32)>,

    /// 上一次**真的显示出内容**的时刻（1.1.0）：自适应预渲染靠它判断"这人最近在用"。
    pub(crate) last_show_at: Option<Instant>,

    /// 预渲染的候选文件（1.1.0）：连续两拍都是同一个才动手预热，用户扫文件时不会抢 CPU。
    pub(crate) prewarm_candidate: Option<PathBuf>,

    /// 当前这一帧是什么时候交给渲染线程的（1.2.0）。`None` = 没有活在做。
    pub(crate) render_since: Option<Instant>,

    /// 这一帧是否已经报过"渲染很慢"（1.2.0）。只报一次，免得看门狗每 250 ms 刷一行日志。
    pub(crate) render_warned: bool,

    /// 已经交给渲染线程的那一帧的缓存 key（1.3.0）：算完回来按这个塞进页缓存。
    pub(crate) pending_key: Option<pagecache::PageKey>,

    /// 已经交给渲染线程的那一帧的"上一帧缓存" key（1.4.0）：算完回来顺手存成上一帧。
    pub(crate) pending_frame: Option<FrameKey>,

    /// 系统预览处理器（1.4.1）：Office 等格式优先交给它画真排版，失败退回插件卡片。
    pub(crate) shell_preview: Option<shellpreview::ShellPreview>,

    /// 顶栏一次性提示（1.7.4）：`(文字, 过期时刻)`。
    ///
    /// 为什么需要它：PPT 的翻页由我们接管（系统处理器没有"跳页"接口，只能投翻页键），
    /// 翻到最后一页再按就**不该**再投给 PowerPoint（它会切到"放映结束"的黑屏），
    /// 而是要在顶栏给一句"已是最后一页"。定时器 `TIMER_TOAST` 负责擦掉。
    pub(crate) toast: Option<(String, Instant)>,

    /// 接管了翻页的系统预览内容（目前只有 PPT）：`(当前页 1 基, 总页数)`。
    pub(crate) slide: Option<(usize, usize)>,

    /// 等着"关掉系统预览处理器的输入"（1.7.4）：等首帧画完再关，否则内容区是白的。
    pub(crate) shell_input_pending: bool,

    /// 换文件时把内容区"冻"在上一帧（1.7.6）。
    /// 背景：Office 预览要先跨进程 `DoPreview`（几十到几百毫秒），这期间旧处理器的子窗口
    /// 已经卸掉、新的一帧还没有，整窗重画就会露出那句"占位文字页"——用户说的"跳之前那个
    /// 占位的页面 / 割裂感"就是这个。冻住之后只更新顶栏（标题要立刻对），
    /// 内容区保持上一帧直到新内容覆盖上来（`TIMER_SHELL_INPUT` 到点解除）。
    pub(crate) hold_frame: bool,

    /// 换帧期间用来"盖住"的旧预览处理器（1.7.6）。
    ///
    /// 光冻我们自己的像素不够：新处理器的子窗口一建出来就是空白白板，立刻就把我们的像素盖掉了。
    /// 所以旧处理器先不卸，`SetWindowPos(HWND_TOP)` 顶到同级最上层继续盖着，
    /// `TIMER_SHELL_SWAP` 到点再卸——那一刻新的处理器已经画出首帧，切换看不出中间态。
    pub(crate) shell_cover: Option<shellpreview::ShellPreview>,

    /// 内容区是否"整块交给处理器、由我们接管输入"（1.7.7）。
    ///
    /// 只有 **PPT** 需要（翻页由我们计数，用户点一下处理器就会翻页让计数失准）。
    /// 其余 Office（Word/Excel）**不能接管**：接管等于禁用子窗口，Excel 的滚动条、
    /// 工作表标签、Word 的选区全都点不动——用户反馈"滚动栏都不可用"就是这个。
    /// 不接管时把子窗口**内缩一圈**，把窗口四条边留给我们做拖边改大小。
    pub(crate) shell_owns_input: bool,

    /// `TIMER_SHELL_INPUT` 的轮询次数（1.7.7）：等"内容真的画出来"再收尾，不按固定毫秒猜。
    pub(crate) shell_poll: u32,

    /// 音频预览的播放器（1.7.9）：`Some` 就代表"当前这个预览是有声的"。
    ///
    /// 生命周期跟着预览走：`show_preview` 里打开（自动播放），`hide_preview` /
    /// 换文件 / 退出时 drop（`AudioPlayer::drop` 会 `MCI_CLOSE`，不关会把设备占住）。
    pub(crate) audio: Option<qlr_platform::audio::AudioPlayer>,
    /// 音频播放进度条的刷新计时器是否开着（1.7.9）：只在播放时开着，停就杀掉。
    pub(crate) audio_ticking: bool,

    /// 视频预览的逐帧播放状态（1.7.9）：`Some` 就代表"当前预览是能放的视频"。
    pub(crate) video: Option<VideoPlay>,
}

/// 视频播放状态（1.7.9）。
///
/// 只有 UI 线程碰它（`TIMER_VIDEO` 也是在 UI 线程上来的），所以不需要锁。
pub(crate) struct VideoPlay {
    /// 逐帧解码流（Media Foundation 的 `SourceReader`）。
    pub(crate) stream: qlr_render::media::FrameStream,
    /// 最新解出来的一帧（窗口重画/改大小时拿它重贴，不必再解码一次）。
    pub(crate) last: Option<qlr_render::media::RawFrame>,
    /// 播放锚点：这一时刻对应的时间轴位置，以及当时的挂钟时间。
    pub(crate) anchor_pos_ms: u64,
    pub(crate) anchor_at: std::time::Instant,
    pub(crate) playing: bool,
    /// 总时长（毫秒）：进度条与"到头"判断用。
    pub(crate) duration_ms: u64,
}

/// 正在拖的边（`true` 表示这一边跟着鼠标动）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResizeEdges {
    pub(crate) left: bool,
    pub(crate) top: bool,
    pub(crate) right: bool,
    pub(crate) bottom: bool,
}

impl ResizeEdges {
    pub(crate) fn any(self) -> bool {
        self.left || self.top || self.right || self.bottom
    }

    /// 对应的鼠标指针形状。对角优先：两条边同时命中时是角。
    pub(crate) fn cursor(self) -> window::CursorKind {
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

    pub(crate) fn describe(self) -> String {
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

pub(crate) static STATE: OnceLock<Mutex<State>> = OnceLock::new();
/// 绘制内容单独放一个锁：`WM_PAINT` 可能在 `ShowWindow` 里同步重入，
/// 若此时还持有 `STATE` 就会死锁。规矩：**先写 LINES、放锁，再动窗口**。
pub(crate) static LINES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
/// 已渲染好的画面（`Arc` 是因为要给 worker 回传与 UI 共享，避免整块像素拷贝）。
pub(crate) static CONTENT: OnceLock<Mutex<Option<Arc<Canvas>>>> = OnceLock::new();
/// 渲染任务队列的发送端。worker 线程在 `run()` 里拉起后长期存活。
pub(crate) static JOB_TX: OnceLock<Mutex<Sender<RenderJob>>> = OnceLock::new();
/// worker 的产出槽：`(代际号, 结果)`。`PostMessage` 只能带两个整数，所以载荷走这里。
pub(crate) type RenderOutcome = (u64, Result<Canvas, qlr_core::Error>);
pub(crate) static RESULT: OnceLock<Mutex<Option<RenderOutcome>>> = OnceLock::new();

/// 渲染线程代号（1.8.3）：每 spawn 一条就换一个新的、更大的号。
///
/// 渲染结果异步回来，自愈换线程之后**旧线程仍可能从第三方代码里爬出来交活**（PDFium 的
/// 锁一放、COM 调用一返回）。结果槽只存"最后一帧"，旧线程的迟到结果要是写进去，就会顶掉
/// 新线程刚算好的那一帧（UI 看到过期代际 → 丢弃 → 画面永远不出现）。有了代号，"这条线程
/// 还是不是当前那条"就是一次原子读：旧线程在写槽/发通知之前先自报家门。
///
/// 初值 0 = "还没有渲染线程"；`spawn_render_worker` 成功换上新通道之后才把新号写进来。
pub(crate) static RENDER_EPOCH: AtomicU64 = AtomicU64::new(0);

/// 渲染线程的"租约"（1.8.3）：渲染线程领活时登记，交活时注销。
///
/// 为什么需要它：渲染线程是**单例**，一旦它卡在第三方 FFI 里就永不返回，之后所有任务
/// 排队无人消费 —— 本会话预览永久不出图，只能重启。租约让 UI 线程能回答"这一帧从什么
/// 时候开始跑、跑的是哪个文件"，从而判挂并换一条线程。
///
/// 另外三个字段的含义：`epoch` 领这条租约的渲染线程代号，`generation` 任务代际
/// （防止 A 的完成把 B 的租约清掉），`job` 留着自愈之后原样重发。
pub(crate) struct Lease {
    pub(crate) epoch: u64,
    pub(crate) generation: u64,
    pub(crate) started: Instant,
    pub(crate) job: RenderJob,
}

/// 租约 + 自愈状态（1.8.3）。**一把锁**，因为这三件事每次都要一起看。
///
/// 为什么不塞进 `State`：`State` 是 UI 线程的大锁（里面的字段牵着窗口/COM 句柄的生命
/// 周期），渲染线程绝不能去碰它。这把锁只在三处各拿几微秒：派发时登记、交活时注销、
/// 250 ms 看门狗判挂 —— UI 线程读它不会等渲染，渲染线程写它也不会等 UI。
pub(crate) struct RenderTrack {
    pub(crate) lease: Option<Lease>,
    /// 自上次有帧真正完成以来，连续自愈了几次（成功后清零，用于退避）。
    pub(crate) consecutive: u32,
    /// 本会话累计重建次数（写进日志，便于判断"是不是一直在自愈"）。
    pub(crate) total: u32,
    /// 有重建在飞：从判挂到换完发送端之间置位，保证任何时刻最多一个重建。
    pub(crate) healing: bool,
}

pub(crate) static RENDER_TRACK: OnceLock<Mutex<RenderTrack>> = OnceLock::new();

pub(crate) fn render_track() -> &'static Mutex<RenderTrack> {
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
pub(crate) static PENDING: OnceLock<Mutex<Vec<Request>>> = OnceLock::new();

/// 一帧渲染任务。只带不可变输入 + 显示预算，不碰任何窗口句柄。
///
/// `Clone` 只为租约服务（1.8.3）：自愈之后要把"卡住的那一帧"原样重发给新线程。
#[derive(Clone)]
pub(crate) struct RenderJob {
    pub(crate) generation: u64,
    pub(crate) path: PathBuf,
    pub(crate) max_width: u32,
    pub(crate) max_height: u32,
    /// PDF 要第几页（其他格式忽略）。
    pub(crate) page: usize,
    /// 允许放大渲染（缩放到 100% 以上时为 `true`，否则位图封顶在原始像素）。
    pub(crate) upscale: bool,
}

pub(crate) fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    let mutex: MutexGuard<'_, State> = STATE.get()?.lock().ok()?;
    let mut guard = mutex;
    Some(f(&mut guard))
}

pub(crate) fn set_lines(lines: Vec<String>) {
    if let Some(slot) = LINES.get()
        && let Ok(mut guard) = slot.lock()
    {
        *guard = lines;
    }
}

pub(crate) fn get_lines() -> Vec<String> {
    LINES
        .get()
        .and_then(|slot| slot.lock().ok().map(|guard| guard.clone()))
        .unwrap_or_default()
}

pub(crate) fn set_content(content: Option<Arc<Canvas>>) {
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

pub(crate) fn get_content() -> Option<Arc<Canvas>> {
    CONTENT
        .get()
        .and_then(|slot| slot.lock().ok().and_then(|guard| guard.clone()))
}

/// 当前预览的文件类型（没有预览或路径未知时 `None`）。
pub(crate) fn current_kind() -> Option<format::Kind> {
    with_state(|s| s.last_path.clone())
        .flatten()
        .map(|p| format::kind_for_path(&p))
}

/// 预览窗句柄（还没建过就是 `None`）。
pub(crate) fn preview_raw() -> Option<HWND> {
    with_state(|s| s.preview.raw())
}
