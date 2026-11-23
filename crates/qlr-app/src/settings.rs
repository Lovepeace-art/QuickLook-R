//! 设置窗口（1.0.7）：把快捷键按类别摊开，点一行就能改键。
//!
//! 为什么是**独立顶层窗口**而不是预览窗里的一页：设置要能拿键盘焦点（改键就是
//! "按一下新键"），而预览窗是 `WS_EX_NOACTIVATE`（绝不抢资源管理器的焦点），
//! 两者需求正好相反。所以这里另开一个普通窗口，样式见
//! `qlr_platform::window::create_settings_window`。
//!
//! 分工：
//! - 本模块负责**画 + 收键 + 写 `config.json`**；
//! - 宿主（`app.rs`）负责**读配置 + 热更新钩子绑定**，靠 `WM_QLR_SETTINGS_SAVED` 通知。
//!
//! 这样"配置只有一个写者"，而"谁装钩子"也仍然只有宿主一处。
//!
//! 2026-11-23：改成**左侧分类栏 + 右侧单页**两栏布局。旧版把所有分组拼成一张
//! 超长单栏，10 个分组把窗口撑到 1100+ 物理像素高，1080p 屏上底部（更新 / 窗口与
//! 启动 / 文件）被挤出屏幕且无法滚动。现在每次只画一个分类的若干行，窗口固定 430
//! DIP 高，任何分类都放得下。

use std::cell::RefCell;
use std::path::PathBuf;

use qlr_core::config::{BACKEND_KINDS, BackendPref, Config, Prewarm};
use qlr_core::keymap::{Action, Binding, Category, Hotkeys};
use qlr_core::{ipc, log_info, log_warn};
use qlr_platform::window;
// 1.8.3：设置窗也得进入自己的 DPI 上下文（否则 width()/行高会沿用"上一个窗口"的 DPI）。
use qlr_platform::dpi;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_PAINT,
};

const SCOPE: &str = "settings";
const CLASS: &str = "QLRust.SettingsWindow";
const TITLE: &str = "QLRust 设置";

const VK_BACK: u16 = 0x08;
const VK_RETURN: u16 = 0x0D;
const VK_ESCAPE: u16 = 0x1B;

/// 正在等用户按键的那一行。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Edit {
    Action(Action),
    /// 放大镜的"按住"键（它不是一个动作，只是 `Hotkeys::lens_hold` 字符串）。
    LensHold,
    /// 图片初始缩放百分比（1.0.9）：不是快捷键，但同属"改一个值"的交互。
    ImageZoom,
}

/// 屏幕上的一行（属于某个 [`Section`]）。
///
/// 布局从"一张全局 `layout()` 大表"改成了"按分类分组"（见 [`sections`]），所以这里
/// 不再有 `Spacer` / `Header` —— 分组标题改由左栏和右侧标题承担。
#[derive(Clone, Copy)]
enum Row {
    Item(Action),
    LensHold,
    ImageZoom,
    /// 投机预渲染档位（1.1.0）：点一下换一档，不用打字。
    Prewarm,
    /// 某个格式类别的后端（内置 / 保真，1.15.x）：`(kind.name(), 显示名)`，点一下换档。
    Backend(&'static str, &'static str),
    /// 未知格式是否也交给系统预览处理器（长尾格式"先套用原版"的开关）。
    ShellUnsupported,
    /// 启动时自动检查新版本（1.15.x）：点一下开关。
    UpdateCheck,
    /// 预览窗口是否置顶（点一下开/关）。写回 `Config::topmost`，下一次打开预览生效。
    Topmost,
    /// 是否启用全局热键（点一下开/关）。写回 `Config::hotkey_enabled`，重启后生效。
    HotkeyEnabled,
    /// 正文/列表内容缩放（点一下换档）。写回 `Config::content_scale_percent`，重启后生效。
    ContentScale,
    /// 文件被外部改动时自动重载（点一下开/关）。写回 `Config::auto_reload`。
    AutoReload,
    Note(&'static str),
}

/// 一个分类 = 左栏一项 + 右侧一页的行。
struct Section {
    title: &'static str,
    rows: Vec<Row>,
}

/// 全部分类。顺序 = 左栏从上到下的顺序。
///
/// 说明性文字（鼠标手势）**不占动作 id**：它们不是可改的键，写成 `Note`
/// 既不会误点也不会被当成配置项存进 `config.json`。
fn sections() -> Vec<Section> {
    let mut secs: Vec<Section> = Vec::new();

    // 两个快捷键分类：全局 / 浏览。
    for category in Category::ALL {
        let rows = category.actions().into_iter().map(Row::Item).collect();
        secs.push(Section {
            title: category.label(),
            rows,
        });
    }

    secs.push(Section {
        title: "放大镜",
        rows: vec![
            Row::LensHold,
            Row::Note("按住它移动鼠标 = 放大镜；按住它滚轮 = 调整倍数（1.5×–8×）"),
        ],
    });

    secs.push(Section {
        title: "鼠标",
        rows: vec![
            Row::Note("左键拖动 = 移动窗口；画面比窗口大时拖动 = 平移画面"),
            Row::Note("右键拖动 = 图片/PDF 缩放；文本/Markdown 上 = 按行滚动"),
            Row::Note("滚轮 = 文本滚动；图片/PDF 缩放或上下滚动，到边翻页"),
        ],
    });

    secs.push(Section {
        title: "预览",
        rows: vec![
            Row::ImageZoom,
            Row::Note("图片按这个比例开窗（100% = 原图像素），窗口最大不超过工作区"),
        ],
    });

    secs.push(Section {
        title: "性能",
        rows: vec![
            Row::Prewarm,
            Row::Note("预渲染：窗口收着时先把选中的文件画好，按空格更快（点这行换档）。"),
            Row::Note("自适应 = 最近 5 分钟用过才预热；总是 = 选中就预热；关 = 不预热。"),
        ],
    });

    let mut backend = Vec::new();
    for &(key, label) in BACKEND_KINDS {
        backend.push(Row::Backend(key, label));
    }
    backend.push(Row::ShellUnsupported);
    backend.push(Row::Note(
        "保真 = 交给系统预览处理器（IPreviewHandler，需装 Office 或对应程序）；点这行换档。",
    ));
    backend.push(Row::Note(
        "内置 = QLRust 自己渲染（快、省内存）；系统没处理器时自动回退内置。",
    ));
    secs.push(Section {
        title: "格式后端",
        rows: backend,
    });

    secs.push(Section {
        title: "更新",
        rows: vec![
            Row::UpdateCheck,
            Row::Note(
                "启动时检查新版本；发现后弹托盘提示，点『检查更新』打开下载页，不自动下载替换。",
            ),
        ],
    });

    secs.push(Section {
        title: "窗口与启动",
        rows: vec![
            Row::Topmost,
            Row::Note("预览窗口是否置顶（下一次打开预览起生效）。"),
            Row::HotkeyEnabled,
            Row::Note("启用全局空格热键；关掉后只能用托盘 / 命令行触发（重启后生效）。"),
            Row::ContentScale,
            Row::Note(
                "正文/列表的内容缩放：自动 = 跟随系统 DPI，其余按百分比放大缩小（重启后生效）。",
            ),
        ],
    });

    secs.push(Section {
        title: "文件",
        rows: vec![
            Row::AutoReload,
            Row::Note("预览的文件被外部改动时自动重新加载（800 ms 轮询；默认关）。"),
        ],
    });

    secs
}

/// 设置窗客户区宽度（物理像素）：左栏 + 右侧内容区（够放左名称 + 172 DIP 值列）。
fn window_width() -> i32 {
    window::settings_sidebar_width() + dpi::px(560)
}

/// 设置窗客户区高度（物理像素）：固定 430 DIP，最高分类（窗口与启动 6 行）也放得下。
fn window_height() -> i32 {
    dpi::px(430)
}

struct Ui {
    hwnd: HWND,
    /// 宿主窗口（保存完成后往它发 `WM_QLR_SETTINGS_SAVED`）。
    host: HWND,
    path: PathBuf,
    draft: Hotkeys,
    editing: Option<Edit>,
    /// 图片初始缩放（%）。快捷键与预览参数都在这份草稿里，保存时一起写回。
    image_zoom: u32,
    /// 输入中的百分比文本（`editing == Some(Edit::ImageZoom)` 时有效）。
    zoom_buffer: String,
    /// 投机预渲染档位草稿（1.1.0），保存时写回 `Config::prewarm`。
    prewarm: Prewarm,
    /// 每格式后端偏好草稿（1.15.x）：`kind.name()` → 内置/保真，保存时写回 `Config::backend_prefs`。
    backend_prefs: std::collections::BTreeMap<String, BackendPref>,
    /// 未知格式是否也交给系统预览处理器；保存时写回 `Config::shell_for_unsupported`。
    shell_for_unsupported: bool,
    /// 启动时自动检查更新；保存时写回 `Config::update_check`。
    update_check: bool,
    /// 预览窗口是否置顶；保存时写回 `Config::topmost`。
    topmost: bool,
    /// 是否启用全局热键；保存时写回 `Config::hotkey_enabled`。
    hotkey_enabled: bool,
    /// 正文/列表内容缩放（%）；保存时写回 `Config::content_scale_percent`。
    content_scale: u32,
    /// 文件被外部改动时自动重载；保存时写回 `Config::auto_reload`。
    auto_reload: bool,
    /// 当前显示的分类（左栏选中项的下标，2026-11-23 两栏布局）。
    section: usize,
    /// 底部提示：按键冲突、保存失败、"已恢复默认"等。
    message: Option<String>,
}

thread_local! {
    /// 设置窗只有一个，所以状态放线程本地（与宿主同线程，不需要加锁）。
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

/// 打开（或前置）设置窗口。
///
/// `config` 是宿主当前的配置：每次打开都从它重新取一份草稿，
/// 免得上一次没保存的改动偷偷留在草稿里。
pub fn open(host: HWND, config: &Config) {
    let alive = UI.with(|cell| cell.borrow().as_ref().map(|ui| ui.hwnd));
    if let Some(hwnd) = alive {
        if window::is_window(hwnd) {
            UI.with(|cell| {
                if let Some(ui) = cell.borrow_mut().as_mut() {
                    ui.draft = config.hotkeys.clone();
                    ui.image_zoom = config.image_zoom_percent;
                    ui.prewarm = config.prewarm;
                    ui.backend_prefs = config.backend_prefs.clone();
                    ui.shell_for_unsupported = config.shell_for_unsupported;
                    ui.update_check = config.update_check;
                    ui.topmost = config.topmost;
                    ui.hotkey_enabled = config.hotkey_enabled;
                    ui.content_scale = config.content_scale_percent;
                    ui.auto_reload = config.auto_reload;
                    ui.section = ui.section.min(sections().len().saturating_sub(1));
                    ui.editing = None;
                    ui.zoom_buffer.clear();
                    ui.message = None;
                }
            });
            window::show_foreground(hwnd);
            window::invalidate(hwnd);
            log_info!(SCOPE, "设置窗口已存在，前置");
            return;
        }
        // 句柄失效（上次被强杀之类）：清掉残留状态，重新建。
        UI.with(|cell| *cell.borrow_mut() = None);
    }

    if let Err(e) = create(host, config, qlr_core::paths::config_path()) {
        log_warn!(SCOPE, "创建设置窗口失败：{e}");
    }
}

fn create(host: HWND, config: &Config, path: PathBuf) -> windows::core::Result<()> {
    // 类注册失败通常只是"已经注册过"，不致命。
    let _ = window::register_class(CLASS, Some(wndproc));
    let hwnd = window::create_settings_window(CLASS, TITLE)?;

    window::set_client_size(hwnd, window_width(), window_height());
    window::center_on_work_area(hwnd);

    UI.with(|cell| {
        *cell.borrow_mut() = Some(Ui {
            hwnd,
            host,
            path,
            draft: config.hotkeys.clone(),
            editing: None,
            image_zoom: config.image_zoom_percent,
            prewarm: config.prewarm,
            backend_prefs: config.backend_prefs.clone(),
            shell_for_unsupported: config.shell_for_unsupported,
            update_check: config.update_check,
            topmost: config.topmost,
            hotkey_enabled: config.hotkey_enabled,
            content_scale: config.content_scale_percent,
            auto_reload: config.auto_reload,
            section: 0,
            zoom_buffer: String::new(),
            message: None,
        })
    });

    window::show_foreground(hwnd);
    window::invalidate(hwnd);
    log_info!(
        SCOPE,
        "设置窗口已打开 {} 个分类 client={}x{}",
        sections().len(),
        window_width(),
        window_height()
    );
    Ok(())
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        // 1.8.3：每条消息都对齐到本窗口的 DPI（`enter` 内部是 O(1) 的 GetDpiForWindow）。
        dpi::enter(hwnd);
        match msg {
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            // 背景由 `paint_settings_two_pane` 自己刷。
            WM_ERASEBKGND => LRESULT(1),
            WM_LBUTTONDOWN => {
                on_click(hwnd, lparam);
                LRESULT(0)
            }
            WM_KEYDOWN => {
                on_key(hwnd, wparam.0 as u16);
                LRESULT(0)
            }
            WM_CLOSE => {
                // 关窗 = 保存（用户已经改完、直接点右上角是常见操作）。
                save_and_close(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                UI.with(|cell| *cell.borrow_mut() = None);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

fn paint(hwnd: HWND) {
    // 先把内容整理成"拥有字符串"的中间层，再借给平台层画：`SettingsRow`/`SidebarItem`
    // 只持 `&str`，直接在闭包里构造会活不过 `paint_settings_two_pane` 的调用。
    let (specs, sidebar, title) = UI.with(|cell| {
        let guard = cell.borrow();
        let Some(ui) = guard.as_ref() else {
            return (Vec::new(), Vec::new(), String::new());
        };
        let secs = sections();
        let idx = ui.section.min(secs.len().saturating_sub(1));
        let mut out: Vec<(String, String, window::TextStyle, window::TextStyle, bool)> = Vec::new();
        if let Some(sec) = secs.get(idx) {
            for &row in &sec.rows {
                match row {
                    Row::Item(action) => out.push((
                        format!("    {}", action.label()),
                        value_text(ui, Edit::Action(action)),
                        window::TextStyle::Normal,
                        window::TextStyle::Code,
                        ui.editing == Some(Edit::Action(action)),
                    )),
                    Row::LensHold => out.push((
                        "    放大镜开关（按住）".to_string(),
                        value_text(ui, Edit::LensHold),
                        window::TextStyle::Normal,
                        window::TextStyle::Code,
                        ui.editing == Some(Edit::LensHold),
                    )),
                    Row::ImageZoom => out.push((
                        "    图片初始缩放".to_string(),
                        value_text(ui, Edit::ImageZoom),
                        window::TextStyle::Normal,
                        window::TextStyle::Code,
                        ui.editing == Some(Edit::ImageZoom),
                    )),
                    Row::Prewarm => out.push((
                        "    投机预渲染".to_string(),
                        ui.prewarm.label().to_string(),
                        window::TextStyle::Normal,
                        window::TextStyle::Code,
                        false,
                    )),
                    Row::Backend(key, label) => out.push((
                        format!("    {label}"),
                        ui.backend_prefs
                            .get(key)
                            .copied()
                            .unwrap_or_default()
                            .label()
                            .to_string(),
                        window::TextStyle::Normal,
                        window::TextStyle::Code,
                        false,
                    )),
                    Row::ShellUnsupported => out.push((
                        "    其他格式（未知）".to_string(),
                        if ui.shell_for_unsupported {
                            "系统处理器"
                        } else {
                            "不支持"
                        }
                        .to_string(),
                        window::TextStyle::Normal,
                        window::TextStyle::Code,
                        false,
                    )),
                    Row::UpdateCheck => out.push((
                        "    自动检查更新".to_string(),
                        if ui.update_check { "开" } else { "关" }.to_string(),
                        window::TextStyle::Normal,
                        window::TextStyle::Code,
                        false,
                    )),
                    Row::Topmost => out.push((
                        "    窗口置顶".to_string(),
                        if ui.topmost { "开" } else { "关" }.to_string(),
                        window::TextStyle::Normal,
                        window::TextStyle::Code,
                        false,
                    )),
                    Row::HotkeyEnabled => out.push((
                        "    启用全局热键".to_string(),
                        if ui.hotkey_enabled { "开" } else { "关" }.to_string(),
                        window::TextStyle::Normal,
                        window::TextStyle::Code,
                        false,
                    )),
                    Row::ContentScale => out.push((
                        "    内容缩放".to_string(),
                        content_scale_label(ui.content_scale),
                        window::TextStyle::Normal,
                        window::TextStyle::Code,
                        false,
                    )),
                    Row::AutoReload => out.push((
                        "    自动重载".to_string(),
                        if ui.auto_reload { "开" } else { "关" }.to_string(),
                        window::TextStyle::Normal,
                        window::TextStyle::Code,
                        false,
                    )),
                    Row::Note(text) => out.push((
                        format!("  {text}"),
                        String::new(),
                        window::TextStyle::Quote,
                        window::TextStyle::Normal,
                        false,
                    )),
                }
            }
        }
        // 底部空行 + 提示行（固定追加，所有分类都一样）。
        out.push((
            String::new(),
            String::new(),
            window::TextStyle::Normal,
            window::TextStyle::Normal,
            false,
        ));
        let hint = ui.message.clone().unwrap_or_else(|| {
            "点击一行后直接按新键 · Backspace 恢复默认 · Enter 保存 · Esc 取消".to_string()
        });
        out.push((
            format!("  {hint}"),
            String::new(),
            window::TextStyle::Quote,
            window::TextStyle::Normal,
            false,
        ));
        let sidebar: Vec<(String, bool)> = secs
            .iter()
            .enumerate()
            .map(|(i, s)| (s.title.to_string(), i == idx))
            .collect();
        let title = secs
            .get(idx)
            .map(|s| s.title.to_string())
            .unwrap_or_default();
        (out, sidebar, title)
    });

    let rows: Vec<window::SettingsRow<'_>> = specs
        .iter()
        .map(
            |(left, right, style, value_style, highlight)| window::SettingsRow {
                left,
                right,
                style: *style,
                value_style: *value_style,
                highlight: *highlight,
            },
        )
        .collect();
    let items: Vec<window::SidebarItem<'_>> = sidebar
        .iter()
        .map(|(text, selected)| window::SidebarItem {
            text,
            selected: *selected,
        })
        .collect();
    window::paint_settings_two_pane(hwnd, &items, &title, &rows);
}

/// 值那一列显示什么：正在改键的行显示提示语，其余显示当前绑定。
fn value_text(ui: &Ui, target: Edit) -> String {
    if ui.editing == Some(target) {
        // 百分比是"逐位输入"的，所以编辑中显示输入缓冲而不是"按新键…"。
        if let Edit::ImageZoom = target {
            return format!("{}%", ui.zoom_buffer);
        }
        return "按新键…".to_string();
    }
    match target {
        Edit::Action(action) => ui.draft.get(action).to_string(),
        Edit::LensHold => ui.draft.lens_hold.clone(),
        Edit::ImageZoom => format!("{}%", ui.image_zoom),
    }
}

/// 内容缩放档位的显示文本：0 视为「自动」（跟随系统 DPI）。
fn content_scale_label(percent: u32) -> String {
    if percent == 0 {
        "自动".to_string()
    } else {
        format!("{percent}%")
    }
}

/// 内容缩放的固定档位（自动 / 100 / 125 / 150 / 200），点一下循环。
const CONTENT_SCALE_PRESETS: &[u32] = &[0, 100, 125, 150, 200];

fn cycle_content_scale(cur: u32) -> u32 {
    match CONTENT_SCALE_PRESETS.iter().position(|&v| v == cur) {
        Some(i) => CONTENT_SCALE_PRESETS[(i + 1) % CONTENT_SCALE_PRESETS.len()],
        None => CONTENT_SCALE_PRESETS[0],
    }
}

fn on_click(hwnd: HWND, lparam: LPARAM) {
    // WM_LBUTTONDOWN 的 lparam：低 16 位 x、高 16 位 y，**都是有符号**的。
    let x = (lparam.0 & 0xFFFF) as u16 as i16 as i32;
    let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;

    // ---- 左栏：点分类切换 ----
    if x < window::settings_sidebar_width() {
        let idx =
            (y - window::settings_sidebar_padding_top()) / window::settings_sidebar_item_height();
        if idx >= 0 {
            let idx = idx as usize;
            if idx < sections().len() {
                UI.with(|cell| {
                    if let Some(ui) = cell.borrow_mut().as_mut() {
                        ui.section = idx;
                        ui.editing = None;
                        ui.message = None;
                    }
                });
                window::invalidate(hwnd);
            }
        }
        return;
    }

    // ---- 右侧：点当前分类里的某一行 ----
    let local_y = y - (window::settings_padding_top() + window::settings_title_height());
    let index = local_y / window::settings_row_height();
    if index < 0 {
        return;
    }
    let secs = sections();
    let section = UI
        .with(|cell| cell.borrow().as_ref().map(|ui| ui.section).unwrap_or(0))
        .min(secs.len().saturating_sub(1));
    let Some(row) = secs
        .get(section)
        .and_then(|s| s.rows.get(index as usize))
        .copied()
    else {
        return;
    };
    let target = match row {
        Row::Item(action) => Edit::Action(action),
        Row::LensHold => Edit::LensHold,
        Row::ImageZoom => Edit::ImageZoom,
        // 预渲染是"点一下换一档"，不是等待按键，所以不走 `editing` 那条路。
        Row::Prewarm => {
            UI.with(|cell| {
                if let Some(ui) = cell.borrow_mut().as_mut() {
                    ui.prewarm = ui.prewarm.cycle();
                    ui.editing = None;
                    ui.message = Some(format!("预渲染：{}", ui.prewarm.label()));
                }
            });
            window::invalidate(hwnd);
            return;
        }
        // 格式后端：点一下在内置 / 保真之间换档，不走 `editing`。
        Row::Backend(key, label) => {
            UI.with(|cell| {
                if let Some(ui) = cell.borrow_mut().as_mut() {
                    let next = ui
                        .backend_prefs
                        .get(key)
                        .copied()
                        .unwrap_or_default()
                        .cycle();
                    ui.backend_prefs.insert(key.to_string(), next);
                    ui.editing = None;
                    ui.message = Some(format!("{label}：{}", next.label()));
                }
            });
            window::invalidate(hwnd);
            return;
        }
        // 未知格式：点一下在"不支持 / 系统处理器"之间切换。
        Row::ShellUnsupported => {
            UI.with(|cell| {
                if let Some(ui) = cell.borrow_mut().as_mut() {
                    ui.shell_for_unsupported = !ui.shell_for_unsupported;
                    ui.editing = None;
                    ui.message = Some(format!(
                        "其他格式：{}",
                        if ui.shell_for_unsupported {
                            "系统处理器"
                        } else {
                            "不支持"
                        }
                    ));
                }
            });
            window::invalidate(hwnd);
            return;
        }
        // 自动检查更新：点一下开/关。
        Row::UpdateCheck => {
            UI.with(|cell| {
                if let Some(ui) = cell.borrow_mut().as_mut() {
                    ui.update_check = !ui.update_check;
                    ui.editing = None;
                    ui.message = Some(format!(
                        "自动检查更新：{}",
                        if ui.update_check { "开" } else { "关" }
                    ));
                }
            });
            window::invalidate(hwnd);
            return;
        }
        // 窗口置顶：点一下开/关。
        Row::Topmost => {
            UI.with(|cell| {
                if let Some(ui) = cell.borrow_mut().as_mut() {
                    ui.topmost = !ui.topmost;
                    ui.editing = None;
                    ui.message = Some(format!(
                        "窗口置顶：{}",
                        if ui.topmost { "开" } else { "关" }
                    ));
                }
            });
            window::invalidate(hwnd);
            return;
        }
        // 启用全局热键：点一下开/关。
        Row::HotkeyEnabled => {
            UI.with(|cell| {
                if let Some(ui) = cell.borrow_mut().as_mut() {
                    ui.hotkey_enabled = !ui.hotkey_enabled;
                    ui.editing = None;
                    ui.message = Some(format!(
                        "全局热键：{}",
                        if ui.hotkey_enabled { "开" } else { "关" }
                    ));
                }
            });
            window::invalidate(hwnd);
            return;
        }
        // 内容缩放：点一下换一档（自动 / 100 / 125 / 150 / 200）。
        Row::ContentScale => {
            UI.with(|cell| {
                if let Some(ui) = cell.borrow_mut().as_mut() {
                    ui.content_scale = cycle_content_scale(ui.content_scale);
                    ui.editing = None;
                    ui.message = Some(format!(
                        "内容缩放：{}",
                        content_scale_label(ui.content_scale)
                    ));
                }
            });
            window::invalidate(hwnd);
            return;
        }
        // 自动重载：点一下开/关。
        Row::AutoReload => {
            UI.with(|cell| {
                if let Some(ui) = cell.borrow_mut().as_mut() {
                    ui.auto_reload = !ui.auto_reload;
                    ui.editing = None;
                    ui.message = Some(format!(
                        "自动重载：{}",
                        if ui.auto_reload { "开" } else { "关" }
                    ));
                }
            });
            window::invalidate(hwnd);
            return;
        }
        Row::Note(_) => return,
    };
    UI.with(|cell| {
        if let Some(ui) = cell.borrow_mut().as_mut() {
            ui.editing = Some(target);
            ui.message = None;
            // 进编辑就把当前值填进缓冲，否则"改一位数"还得先退格清空。
            if let Edit::ImageZoom = target {
                ui.zoom_buffer = ui.image_zoom.to_string();
            }
        }
    });
    window::invalidate(hwnd);
}

fn on_key(hwnd: HWND, vk: u16) {
    let mut wants_save = false;
    let mut wants_close = false;
    UI.with(|cell| {
        let mut guard = cell.borrow_mut();
        let Some(ui) = guard.as_mut() else {
            return;
        };
        let Some(target) = ui.editing else {
            match vk {
                VK_RETURN => wants_save = true,
                VK_ESCAPE => wants_close = true,
                _ => {}
            }
            return;
        };
        // 图片缩放是数字输入，跟"按新键"不是一回事，单独一条路径。
        if let Edit::ImageZoom = target {
            match vk {
                VK_ESCAPE => {
                    ui.editing = None;
                    ui.zoom_buffer.clear();
                    ui.message = None;
                }
                VK_RETURN => confirm_image_zoom(ui),
                VK_BACK => {
                    ui.zoom_buffer.pop();
                }
                // 主键区与数字小键盘：两边都收，用户按哪边都行。
                0x30..=0x39 | 0x60..=0x69 => {
                    let digit = if vk >= 0x60 { vk - 0x60 } else { vk - 0x30 };
                    if ui.zoom_buffer.len() < 3 {
                        ui.zoom_buffer.push(char::from(b'0' + digit as u8));
                    }
                }
                _ => {}
            }
            return;
        }
        match vk {
            // 改键过程中 Esc = 放弃这一行（不是关窗口）。
            VK_ESCAPE => {
                ui.editing = None;
                ui.message = None;
            }
            VK_RETURN => {
                ui.editing = None;
            }
            VK_BACK => {
                let default = match target {
                    Edit::Action(action) => action.default_binding().to_string(),
                    Edit::LensHold => Hotkeys::default().lens_hold,
                    // 走不到：图片缩放有自己的一条输入路径（见上面 `Edit::ImageZoom`）。
                    Edit::ImageZoom => return,
                };
                apply_binding(ui, target, &default);
                ui.editing = None;
                ui.message = Some(format!("已恢复默认：{default}"));
            }
            _ => {
                if is_modifier(vk) {
                    ui.message = Some("请按主键（修饰键可以先按住）".to_string());
                    return;
                }
                let (ctrl, alt, shift) = window::modifier_state();
                let binding = Binding {
                    vk,
                    ctrl,
                    alt,
                    shift,
                };
                if let Edit::LensHold = target {
                    // 放大镜是"按住才生效"，只接受修饰键——否则按住一个字母键
                    // 会变成一边打字一边出放大镜。
                    if !(0xA0..=0xA5).contains(&vk) {
                        ui.message = Some("放大镜只能用 Shift/Ctrl/Alt".to_string());
                        return;
                    }
                }
                if let Some(other) = conflict_of(ui, target, binding) {
                    ui.message = Some(format!("这个组合已给「{other}」了"));
                    return;
                }
                apply_binding(ui, target, &binding.text());
                ui.editing = None;
                ui.message = None;
            }
        }
    });

    if wants_save {
        save_and_close(hwnd);
        return;
    }
    if wants_close {
        // Esc = 放弃全部改动（所以不写文件，直接销毁窗口）。
        log_info!(SCOPE, "设置已取消（未保存）");
        window::destroy(hwnd);
        return;
    }
    window::invalidate(hwnd);
}

/// 确认图片缩放输入：解析失败就用原值，越界夹到 [10, 400]。
fn confirm_image_zoom(ui: &mut Ui) {
    match ui.zoom_buffer.trim().parse::<u32>() {
        Ok(value) => {
            let clamped = value.clamp(10, 400);
            ui.image_zoom = clamped;
            ui.message = if clamped == value {
                Some(format!("图片初始缩放 = {clamped}%"))
            } else {
                Some(format!("{value}% 超出范围，已改成 {clamped}%"))
            };
        }
        Err(_) => {
            ui.message = Some("请输入 10–400 之间的数字".to_string());
        }
    }
    ui.zoom_buffer.clear();
    ui.editing = None;
    log_info!(SCOPE, "图片初始缩放 = {}%", ui.image_zoom);
}

fn apply_binding(ui: &mut Ui, target: Edit, text: &str) {
    match target {
        Edit::Action(action) => ui.draft.set(action, text.to_string()),
        Edit::LensHold => ui.draft.lens_hold = text.to_string(),
        Edit::ImageZoom => {}
    }
    log_info!(SCOPE, "改键 {} = {text}", describe(target));
}

fn conflict_of(ui: &Ui, target: Edit, binding: Binding) -> Option<&'static str> {
    for action in Action::ALL {
        if target == Edit::Action(action) {
            continue;
        }
        if ui.draft.binding(action) == binding {
            return Some(action.label());
        }
    }
    if target != Edit::LensHold && qlr_core::keymap::parse(&ui.draft.lens_hold) == Some(binding) {
        return Some("放大镜开关");
    }
    None
}

fn describe(target: Edit) -> &'static str {
    match target {
        Edit::Action(action) => action.label(),
        Edit::LensHold => "放大镜开关",
        Edit::ImageZoom => "图片初始缩放",
    }
}

fn is_modifier(vk: u16) -> bool {
    // Shift/Ctrl/Alt/Win 与它们的左右变体。
    (0x10..=0x12).contains(&vk) || (0x5B..=0x5C).contains(&vk) || (0xA0..=0xA5).contains(&vk)
}

fn save_and_close(hwnd: HWND) {
    let saved = UI.with(|cell| {
        let mut guard = cell.borrow_mut();
        let ui = guard.as_mut()?;
        Some((
            ui.path.clone(),
            ui.draft.clone(),
            ui.host,
            ui.hwnd,
            ui.image_zoom,
            ui.prewarm,
            ui.backend_prefs.clone(),
            ui.shell_for_unsupported,
            ui.update_check,
            ui.topmost,
            ui.hotkey_enabled,
            ui.content_scale,
            ui.auto_reload,
        ))
    });
    let Some((
        path,
        hotkeys,
        host,
        own,
        image_zoom,
        prewarm,
        backend_prefs,
        shell_for_unsupported,
        update_check,
        topmost,
        hotkey_enabled,
        content_scale,
        auto_reload,
    )) = saved
    else {
        return;
    };
    // 重新读一次再写：这期间用户可能手改过配置文件，整份覆盖会吃掉别的字段。
    let (mut config, _) = Config::load(&path);
    config.hotkeys = hotkeys;
    config.image_zoom_percent = image_zoom;
    config.prewarm = prewarm;
    config.backend_prefs = backend_prefs;
    config.shell_for_unsupported = shell_for_unsupported;
    config.update_check = update_check;
    config.topmost = topmost;
    config.hotkey_enabled = hotkey_enabled;
    config.content_scale_percent = content_scale;
    config.auto_reload = auto_reload;
    match config.save(&path) {
        Ok(()) => {
            log_info!(
                SCOPE,
                "设置已保存 → {} ({})",
                path.display(),
                config.hotkeys.summary()
            );
            // 宿主会重读配置并热更新钩子绑定。
            window::post(host, ipc::WM_QLR_SETTINGS_SAVED);
            window::destroy(own);
        }
        Err(e) => {
            log_warn!(SCOPE, "设置保存失败：{e}");
            UI.with(|cell| {
                if let Some(ui) = cell.borrow_mut().as_mut() {
                    ui.message = Some(format!("保存失败：{e}"));
                }
            });
            window::invalidate(hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_cover_every_action_exactly_once() {
        let secs = sections();
        for action in Action::ALL {
            let count = secs
                .iter()
                .flat_map(|s| s.rows.iter())
                .filter(|row| matches!(row, Row::Item(a) if *a == action))
                .count();
            assert_eq!(count, 1, "动作 {:?} 应该正好出现一次", action);
        }
    }

    #[test]
    fn sections_have_a_row_for_each_backend_kind() {
        let secs = sections();
        for &(key, _) in BACKEND_KINDS {
            let seen = secs
                .iter()
                .flat_map(|s| s.rows.iter())
                .filter(|row| matches!(row, Row::Backend(k, _) if *k == key))
                .count();
            assert_eq!(seen, 1, "后端类别 {key} 应恰好出现一次");
        }
        assert!(
            secs.iter()
                .flat_map(|s| s.rows.iter())
                .any(|row| matches!(row, Row::ShellUnsupported))
        );
    }

    #[test]
    fn content_scale_cycles_through_presets_and_labels() {
        assert_eq!(content_scale_label(0), "自动");
        assert_eq!(content_scale_label(150), "150%");
        // 从第一档出发走完整一圈回到第一档。
        let mut v = CONTENT_SCALE_PRESETS[0];
        for _ in 0..CONTENT_SCALE_PRESETS.len() {
            v = cycle_content_scale(v);
        }
        assert_eq!(v, CONTENT_SCALE_PRESETS[0]);
        // 未知值（如旧配置里的 133）回落第一档。
        assert_eq!(cycle_content_scale(133), CONTENT_SCALE_PRESETS[0]);
    }

    #[test]
    fn modifier_keys_are_recognised() {
        assert!(is_modifier(0x10)); // Shift
        assert!(is_modifier(0x11)); // Ctrl
        assert!(is_modifier(0xA2)); // 左 Ctrl
        assert!(!is_modifier(0x20)); // 空格
        assert!(!is_modifier(0x0D)); // 回车
    }

    #[test]
    fn sidebar_and_content_hit_math() {
        // 左栏：第 1 个条目（含留白 + 一个条目高 + 半个条目高）应命中下标 1。
        let top = window::settings_sidebar_padding_top();
        let ih = window::settings_sidebar_item_height();
        let y = top + ih + ih / 2;
        assert_eq!((y - top) / ih, 1);
        // 右侧内容区：第一行在 padding_top + title_height 处。
        let ctop = window::settings_padding_top() + window::settings_title_height();
        let rh = window::settings_row_height();
        let y2 = ctop + rh + 3;
        assert_eq!((y2 - ctop) / rh, 1);
    }
}
