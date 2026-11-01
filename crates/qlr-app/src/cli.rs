//! 命令行解析。
//!
//! 兼容 QuickLook 的行为：**直接把文件路径当参数**（`_src/QuickLook/QuickLook/App.xaml.cs:148-159`
//! 把第一个参数当文件路径转发给已运行实例）。我们额外提供几个开关，
//! 便于脚本化冒烟测试与后续 CLI（WBS A3.4）。

use qlr_core::ipc::{Action, Request};

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Version,
    /// 只查询一次"资源管理器当前选中了什么"然后退出（诊断用，A1.6）。
    ProbeSelection,
    /// 同上，但强制用给定的窗口 HWND（十进制）当 frame——自动化里没法保证
    /// 资源管理器当前是前台窗口，只能靠这个把被测窗口钉死。
    ProbeSelectionOf(isize),
    /// 无窗口渲染一次并退出（A2.10 基准/A2.8 PDF 诊断用）：
    /// 结果只写日志，不创建任何窗口。
    Render {
        path: String,
        page: usize,
    },
    /// 把内置支持的扩展名（一行一个，不含点）写进给定文件后退出。
    ///
    /// **为什么写文件而不是打 stdout**：本程序是 GUI 子系统
    /// （`#![windows_subsystem = "windows"]`），stdout 句柄经常无效，
    /// 重定向会拿到空文件 —— 那会让安装器静默注册 0 个扩展名。
    /// 有了这个命令，文件关联的扩展名清单就只有 `format::all_extensions()`
    /// 一个真值，安装器不再自己维护一份表。
    ListExtensions(String),
    Invoke(Request),
}

pub fn usage() -> &'static str {
    "QLRust — 空格预览（开发中）

用法：
  qlrust <文件路径>    预览该文件（若无实例则先启动）
  qlrust               显示/隐藏预览窗口
  qlrust --show        显示预览窗口
  qlrust --hide        隐藏预览窗口
  qlrust --quit        退出正在运行的实例
  qlrust --ping        探活（仅转发一条 Ping）
  qlrust --probe-selection  打印资源管理器当前选中项（诊断）
  qlrust --probe-selection-hwnd <HWND>  同上，但用指定窗口当 frame（诊断）
  qlrust --simulate-hotkey  模拟一次空格热键（冒烟测试用）
  qlrust --render <文件> [--page N]  无窗口渲染一次并退出（基准/诊断）
  qlrust --list-extensions <输出文件>  把内置扩展名写进文件后退出（安装器用）
  qlrust --next-page / --prev-page   翻页（PDF）
  qlrust --zoom-in / --zoom-out / --zoom-reset   缩放
  qlrust --pan <dx> <dy>   平移（诊断用，单位≈屏高 1/12）
 qlrust --wheel <delta>   模拟滚轮（±120 的倍数；正数向上；自动化验证用）
  qlrust --scroll <n>      文本滚动 n 行（正数向下；自动化验证用）
  qlrust --settings        打开设置窗口（快捷键等）
  qlrust -v|--version  打印版本（写入日志）
  qlrust -h|--help     打印本帮助（写入日志）

说明：本程序是 GUI 子系统，输出都写在 %LOCALAPPDATA%\\QLRust\\logs\\app.log。"
}

pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Command {
    let mut action: Option<Action> = None;
    let mut path: Option<String> = None;
    // --render 要立刻返回，单独记一份；--page 只对它有效。
    let mut render: Option<String> = None;
    let mut page = 0usize;

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" | "/?" => return Command::Help,
            "-v" | "--version" => return Command::Version,
            "--probe-selection" => return Command::ProbeSelection,
            "--probe-selection-hwnd" => {
                return match iter.next().and_then(|v| v.trim().parse::<isize>().ok()) {
                    Some(hwnd) => Command::ProbeSelectionOf(hwnd),
                    None => Command::Help,
                };
            }
            "--render" => match iter.next() {
                Some(p) => render = Some(p),
                None => return Command::Help,
            },
            "--list-extensions" => {
                return match iter.next() {
                    Some(p) => Command::ListExtensions(p),
                    None => Command::Help,
                };
            }
            "--page" => {
                page = iter
                    .next()
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0)
            }
            "--simulate-hotkey" => action = Some(Action::HotkeyProbe),
            "--next-page" => action = Some(Action::PageNext),
            "--prev-page" => action = Some(Action::PagePrev),
            "--zoom-in" => action = Some(Action::ZoomIn),
            "--zoom-out" => action = Some(Action::ZoomOut),
            "--zoom-reset" => action = Some(Action::ZoomReset),
            "--wheel" => {
                let delta = iter
                    .next()
                    .and_then(|v| v.parse::<i32>().ok())
                    .unwrap_or(120);
                action = Some(Action::Wheel { delta });
            }
            "--pan" => {
                let dx = iter.next().and_then(|v| v.parse::<i32>().ok()).unwrap_or(0);
                let dy = iter.next().and_then(|v| v.parse::<i32>().ok()).unwrap_or(0);
                action = Some(Action::Pan { dx, dy })
            }
            "--scroll" => {
                let delta = iter.next().and_then(|v| v.parse::<i32>().ok()).unwrap_or(0);
                action = Some(Action::Scroll { delta })
            }
            "--quit" => action = Some(Action::Quit),
            "--show" => action = Some(Action::Show),
            "--hide" => action = Some(Action::Hide),
            "--ping" => action = Some(Action::Ping),
            "--settings" => action = Some(Action::Settings),
            // 未知开关忽略（不报错退出，避免打包脚本因多传参数而失败）。
            other if other.starts_with('-') => continue,
            other => path = Some(other.to_string()),
        }
    }

    if let Some(path) = render {
        return Command::Render { path, page };
    }

    let action = action.unwrap_or(Action::Toggle);
    let request = match path {
        // 路径必须在本进程（用户当前目录）里就转成绝对路径再转发：
        // 常驻实例的 CWD 是它自己启动时的目录，相对路径到了那边就找不到了。
        Some(p) => Request::with_path(action, absolutize(&p)),
        None => Request::new(action),
    };
    Command::Invoke(request)
}

/// 把用户给的相对路径转成绝对路径（拿不到就原样返回）。
fn absolutize(path: &str) -> String {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        return path.to_owned();
    }
    std::path::absolute(p)
        .map(|abs| abs.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Command {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_args_means_toggle() {
        assert_eq!(p(&[]), Command::Invoke(Request::new(Action::Toggle)));
    }

    #[test]
    fn path_only_means_toggle_with_path() {
        let got = p(&[r"C:\tmp\a.png"]);
        let want = Command::Invoke(Request::with_path(Action::Toggle, r"C:\tmp\a.png"));
        assert_eq!(got, want);
    }

    #[test]
    fn switches_are_recognised() {
        assert_eq!(p(&["--quit"]), Command::Invoke(Request::new(Action::Quit)));
        assert_eq!(p(&["--hide"]), Command::Invoke(Request::new(Action::Hide)));
        assert_eq!(p(&["--ping"]), Command::Invoke(Request::new(Action::Ping)));
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert_eq!(p(&["-h"]), Command::Help);
        assert_eq!(p(&["--version"]), Command::Version);
        // 即使后面还跟着别的参数，help 优先。
        assert_eq!(p(&[r"C:\a.txt", "--help"]), Command::Help);
    }

    #[test]
    fn probe_selection_is_a_standalone_command() {
        assert_eq!(p(&["--probe-selection"]), Command::ProbeSelection);
        // 它是"立即返回"的命令，即使带了路径也不该变成预览请求。
        assert_eq!(
            p(&["--probe-selection", r"C:\a.txt"]),
            Command::ProbeSelection
        );
    }

    #[test]
    fn unknown_switch_is_ignored_but_path_kept() {
        let got = p(&["--nonsense", r"C:\b.txt"]);
        assert_eq!(
            got,
            Command::Invoke(Request::with_path(Action::Toggle, r"C:\b.txt"))
        );
    }

    #[test]
    fn list_extensions_takes_a_file_argument() {
        assert_eq!(
            p(&["--list-extensions", r"C:\tmp\ext.txt"]),
            Command::ListExtensions(r"C:\tmp\ext.txt".to_string())
        );
        // 缺参数时不猜默认值，直接给帮助。
        assert_eq!(p(&["--list-extensions"]), Command::Help);
    }

    #[test]
    fn settings_switch_opens_the_settings_window() {
        assert_eq!(
            p(&["--settings"]),
            Command::Invoke(Request::new(Action::Settings))
        );
    }
}
