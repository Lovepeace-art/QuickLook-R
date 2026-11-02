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

use std::cell::RefCell;
use std::path::PathBuf;

use qlr_core::config::{Config, Prewarm};
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
/// 客户区宽度（DIP）：左列名称 + 右列键值（值列 172 DIP）留够余量。
///
/// 1.8.3 起是**设计值**，实际像素由 [`width()`] 按 DPI 换算——不换算的话 200% 缩放下
/// 620 物理像素只有 310 DIP 宽，键值列会被挤成一条缝。
const WIDTH_DIP: i32 = 620;

/// 设置窗客户区宽度（物理像素，随 DPI）。
fn width() -> i32 {
    dpi::px(WIDTH_DIP)
}

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

/// 屏幕上的一行。布局是**常量表**：[`layout`] 决定顺序，
/// 绘制和命中测试都调它，所以"看到的行"和"点到的行"永远一致。
enum Row {
    Spacer,
    Header(&'static str),
    Item(Action),
    LensHold,
    ImageZoom,
    /// 投机预渲染档位（1.1.0）：点一下换一档，不用打字。
    Prewarm,
    Note(&'static str),
}

/// 一页设置的全部行。
///
/// 说明性文字（鼠标手势）**不占动作 id**：它们不是可改的键，写成 `Note`
/// 既不会误点也不会被当成配置项存进 `config.json`。
fn layout() -> Vec<Row> {
    let mut rows = vec![Row::Spacer];
    for category in Category::ALL {
        rows.push(Row::Header(category.label()));
        for action in category.actions() {
            rows.push(Row::Item(action));
        }
        rows.push(Row::Spacer);
    }
    rows.push(Row::Header("放大镜"));
    rows.push(Row::LensHold);
    rows.push(Row::Note(
        "按住它移动鼠标 = 放大镜；按住它滚轮 = 调整倍数（1.5×–8×）",
    ));
    rows.push(Row::Spacer);
    rows.push(Row::Header("鼠标（固定，不能改键）"));
    rows.push(Row::Note(
        "左键拖动 = 移动窗口；画面比窗口大时拖动 = 平移画面",
    ));
    rows.push(Row::Note(
        "右键拖动 = 图片/PDF 缩放；文本/Markdown 上 = 按行滚动",
    ));
    rows.push(Row::Note(
        "滚轮 = 文本滚动；图片/PDF 缩放或上下滚动，到边翻页",
    ));
    rows.push(Row::Spacer);
    rows.push(Row::Header("预览"));
    rows.push(Row::ImageZoom);
    rows.push(Row::Note(
        "图片按这个比例开窗（100% = 原图像素），窗口最大不超过工作区",
    ));
    rows.push(Row::Spacer);
    rows.push(Row::Header("性能"));
    rows.push(Row::Prewarm);
    rows.push(Row::Note(
        "预渲染：窗口收着时先把选中的文件画好，按空格更快（点这行换档）。",
    ));
    rows.push(Row::Note(
        "自适应 = 最近 5 分钟用过才预热；总是 = 选中就预热；关 = 不预热。",
    ));
    rows
}

/// 底部固定追加的视觉行数：一个空行 + 一行提示（改键冲突、保存失败等）。
///
/// 窗口高度必须按 `layout().len() + EXTRA_ROWS` 算，否则最后那行提示会被
/// 客户区裁掉半截 —— 1.0.8 的截图里就露出来了（见 CONVENTIONS #66）。
const EXTRA_ROWS: usize = 2;

/// 设置窗客户区应有的高度。绘制与实际窗口都从这里取，避免两处各算一遍。
fn client_height() -> i32 {
    window::settings_padding_top() * 2
        + (layout().len() + EXTRA_ROWS) as i32 * window::settings_row_height()
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

    let rows = layout();
    let height = client_height();
    window::set_client_size(hwnd, width(), height);
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
            zoom_buffer: String::new(),
            message: None,
        })
    });

    window::show_foreground(hwnd);
    window::invalidate(hwnd);
    log_info!(
        SCOPE,
        "设置窗口已打开 {} 行（含底部 {} 行）client={}x{}",
        rows.len() + EXTRA_ROWS,
        EXTRA_ROWS,
        width(),
        height
    );
    Ok(())
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        // 1.8.3：每条消息都对齐到本窗口的 DPI（`enter` 内部是 O(1) 的 GetDpiForWindow）。
        // 设置窗是独立顶层窗口，可能被拖到另一块缩放的屏上——不重新对齐，字号和行高
        // 会一直用"上一个窗口"的 DPI。
        dpi::enter(hwnd);
        match msg {
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            // 背景由 `paint_settings` 自己刷（与文本预览同一个理由：没人刷底就会重影）。
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
    // 先把内容整理成"拥有字符串"的中间层，再借给平台层画：`SettingsRow` 只持 `&str`，
    // 直接在闭包里构造会活不过 `paint_settings` 的调用。
    let specs: Vec<(String, String, window::TextStyle, window::TextStyle, bool)> =
        UI.with(|cell| {
            let guard = cell.borrow();
            let Some(ui) = guard.as_ref() else {
                return Vec::new();
            };
            let rows = layout();
            let mut out = Vec::with_capacity(rows.len() + 3);
            for row in rows {
                match row {
                    Row::Spacer => out.push((
                        String::new(),
                        String::new(),
                        window::TextStyle::Normal,
                        window::TextStyle::Normal,
                        false,
                    )),
                    Row::Header(text) => out.push((
                        text.to_string(),
                        String::new(),
                        window::TextStyle::Heading3,
                        window::TextStyle::Normal,
                        false,
                    )),
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
                    Row::Note(text) => out.push((
                        format!("  {text}"),
                        String::new(),
                        window::TextStyle::Quote,
                        window::TextStyle::Normal,
                        false,
                    )),
                }
            }
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
                // 提示行用 Quote（深灰）而不是 Rule（浅灰分隔线色）：白底下 Rule 太淡，
                // 截图里几乎看不见。冲突/失败消息也走这一行，必须清楚。
                window::TextStyle::Quote,
                window::TextStyle::Normal,
                false,
            ));
            out
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
    window::paint_settings(hwnd, &rows);
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

fn on_click(hwnd: HWND, lparam: LPARAM) {
    // WM_LBUTTONDOWN 的 lparam：低 16 位 x、高 16 位 y，**都是有符号**的。
    let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    let index = (y - window::settings_padding_top()) / window::settings_row_height();
    if index < 0 {
        return;
    }
    let rows = layout();
    let Some(row) = rows.get(index as usize) else {
        return;
    };
    let target = match row {
        Row::Item(action) => Edit::Action(*action),
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
        _ => return,
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
        ))
    });
    let Some((path, hotkeys, host, own, image_zoom, prewarm)) = saved else {
        return;
    };
    // 重新读一次再写：这期间用户可能手改过配置文件，整份覆盖会吃掉别的字段。
    let (mut config, _) = Config::load(&path);
    config.hotkeys = hotkeys;
    config.image_zoom_percent = image_zoom;
    config.prewarm = prewarm;
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
    fn layout_lists_every_action_exactly_once() {
        let rows = layout();
        for action in Action::ALL {
            let count = rows
                .iter()
                .filter(|row| matches!(row, Row::Item(a) if *a == action))
                .count();
            assert_eq!(count, 1, "动作 {:?} 应该正好出现一次", action);
        }
    }

    #[test]
    fn layout_has_one_row_per_category_header() {
        let rows = layout();
        let headers = rows
            .iter()
            .filter(|row| matches!(row, Row::Header(_)))
            .count();
        // 两个动作分类 + 放大镜 + 鼠标说明 + 预览参数 + 性能（1.1.0）。
        assert_eq!(headers, Category::ALL.len() + 4);
    }

    #[test]
    fn hit_test_maps_rows_to_the_right_target() {
        // 行索引 → 目标：第 0 行是 Spacer、第 1 行是第一个分类标题、
        // 第 2 行就是第一个动作（Toggle）。
        let rows = layout();
        assert!(matches!(rows[0], Row::Spacer));
        assert!(matches!(rows[1], Row::Header(_)));
        assert!(matches!(rows[2], Row::Item(Action::Toggle)));
        // 命中测试的换算：y = padding + index * row_height。
        let index = 2;
        let y = window::settings_padding_top() + index * window::settings_row_height() + 3;
        let got = (y - window::settings_padding_top()) / window::settings_row_height();
        assert_eq!(got, index);
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
    fn client_height_covers_every_painted_row() {
        // 客户区能放下的行数（扣掉上下 padding）必须 ≥ 实际会画的行数，
        // 否则最后那行提示会被裁掉半截。
        let fits =
            (client_height() - window::settings_padding_top() * 2) / window::settings_row_height();
        let painted = (layout().len() + EXTRA_ROWS) as i32;
        assert!(
            fits >= painted,
            "客户区只放得下 {fits} 行，但要画 {painted} 行"
        );
    }
}
