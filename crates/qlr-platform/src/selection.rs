//! 读取资源管理器当前选中的文件（WBS A1.6）。
//!
//! 这是整个项目**最脆**的一块（`docs/route-a-plan.md` §8 风险登记第 1 条）：
//! Shell 没有官方 API 告诉你"用户选了什么"，只能顺着
//! `IShellWindows → IServiceProvider → IShellBrowser → IShellView → IFolderView` 爬。
//!
//! 已核实的关键事实（2026-10-29，Win10 19045 实测，`comtest` 诊断程序）：
//! - `IShellWindows` **每条注册项的 HWND 是 `ShellTabWindowClass`，不是 `CabinetWClass` frame**。
//!   `IOleWindow::GetWindow()` 反查出来的就是这个子窗口，`GetAncestor(.., GA_ROOT)` 才等于 frame。
//!   所以匹配必须用"同根"而不是"相等"，见 [`query_by_hwnd`]。
//! - **不能用 `FindWindowSW(locator=hwnd)` 定位资源管理器窗口**：实测把任意 frame HWND
//!   传进去都解析成桌面的那条，拿回来的选区属于桌面，`GetDisplayName` 直接
//!   `E_INVALIDARG`（0x80070057）。资源管理器必须逐条 `Item(i)` 枚举 + `GetWindow()` 反查。
//! - **桌面反过来只能用 `FindWindowSW(SWC_DESKTOP)`**：桌面前台时 `IShellWindows` 里
//!   经常根本没有桌面那一条（2026-10-30 实测 count=3，全是资源管理器窗口），
//!   所以按 HWND 匹配永远命中不了桌面，见 [`query_desktop`]。
//! - 桌面图标大多是真实文件，`SIGDN_FILESYSPATH` 可用；只有"回收站"这类虚拟项
//!   才 `E_INVALIDARG`，靠 [`paths_from_browser`] 里的 `PARENTRELATIVEPARSING` 兜底跳过。
//! - `IFolderView2::GetSelection(..)` 在"选区为空"时返回 `S_FALSE`（`HRESULT=1`）或
//!   `0x80070490`（`ERROR_NOT_FOUND`，视图还没建好），都是**正常状态**，不是错误。
//! - Win11 多标签：一个 frame 下多个 `ShellTabWindowClass`，只有活动标签可见；
//!   本模块按"可见优先"逐个试（PowerToys 在 `microsoft/PowerToys#22641` 踩过非活动标签）。
//! - 第三段兜底（UI Automation `SelectionPattern`，50–200 ms）与第四段（鼠标命中的
//!   第一项）留到 M2 做，见 `docs/dev/ARCH.md` 的 `[计划]` 段。
//!
//! 调用约定：**必须在已经 `CoInitializeEx(APARTMENTTHREADED)` 的线程上调用**
//! （本项目的 UI 线程）。别在线程池里调——STA 封送会把结果悄悄变成空。
//! 另外：**exe 不能放在"工作目录"这类受限目录里跑**，否则 ShellOutOfProc 激活被拒
//! （`0x80070005`），见 `docs/dev/CONVENTIONS.md` 坑 #9。

use std::path::PathBuf;

use qlr_core::{log_debug, log_warn};
use windows::Win32::Foundation::{HWND, MAX_PATH};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, IDispatch, IServiceProvider,
};
use windows::Win32::System::Ole::IOleWindow;
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_I4};
use windows::Win32::UI::Shell::{
    IFolderView2, IShellBrowser, IShellItem, IShellWindows, SIGDN, SIGDN_FILESYSPATH,
    SIGDN_PARENTRELATIVEPARSING, SWC_DESKTOP, SWFO_NEEDDISPATCH, ShellWindowFindWindowOptions,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetClassNameW, GetForegroundWindow, IsWindowVisible,
};
use windows::core::{GUID, Interface};

const SCOPE: &str = "selection";

/// `CLSID_ShellWindows`。`windows` crate **没有**导出这个常量（它在 `exdisp.h` 里），
/// 手写：`{9BA05972-F6A8-11CF-A442-00A0C90A8F39}`。
const CLSID_SHELL_WINDOWS: GUID = GUID::from_u128(0x9BA0_5972_F6A8_11CF_A442_00A0_C90A_8F39);
/// `SID_SShellBrowser`：`{000214E2-0000-0000-C000-000000000046}`，同样需要手写。
const SID_S_SHELL_BROWSER: GUID = GUID::from_u128(0x0002_14E2_0000_0000_C000_0000_0000_0046);

/// 一次最多取多少个选中项（选中 1 万个文件时不该把主线程拖死）。
pub const MAX_ITEMS: u32 = 32;
/// 选区上限必须是个小常数，否则"选中一万个文件"会拖死主线程。
const _: () = assert!(MAX_ITEMS > 0 && MAX_ITEMS <= 64);

/// 选中项是怎么拿到的——写日志用，出问题时能一眼看出走的哪条路。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Source {
    /// 活动标签页窗口（Win11 多标签）
    Tab,
    /// 前台窗口本身就是 shell frame
    Frame,
    /// 不指定 HWND，让 shell 自己找当前浏览器窗口（`SWFO_NEEDDISPATCH`）
    Foreground,
    /// 桌面（`Progman` / `WorkerW`），走 `FindWindowSW(SWC_DESKTOP)` 单独取
    Desktop,
    /// 四条路都失败
    #[default]
    None,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Tab => "tab",
            Source::Frame => "frame",
            Source::Foreground => "swfo",
            Source::Desktop => "desktop",
            Source::None => "none",
        }
    }
}

/// 选中项查询结果。
#[derive(Clone, Debug, Default)]
pub struct Selection {
    pub paths: Vec<PathBuf>,
    pub source: Source,
}

impl Selection {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn first(&self) -> Option<&PathBuf> {
        self.paths.first()
    }

    /// 日志用摘要（不打印完整路径列表，避免刷屏）。
    pub fn summary(&self) -> String {
        match self.paths.first() {
            Some(p) => format!(
                "source={} count={} first={}",
                self.source.as_str(),
                self.paths.len(),
                p.display()
            ),
            None => format!("source={} count=0", self.source.as_str()),
        }
    }
}

/// 在 UI 线程初始化 COM（STA）。返回 `true` 表示可以继续用 shell API。
pub fn init_com() -> bool {
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    // S_OK(0) / S_FALSE(1, 已经初始化过) 都算成功；
    // RPC_E_CHANGED_MODE(0x80010106) 说明线程已经是 MTA，属于编程错误，记日志但不致命。
    if hr.is_ok() {
        true
    } else {
        log_warn!(SCOPE, "CoInitializeEx 失败：HRESULT=0x{:08X}", hr.0);
        false
    }
}

pub fn uninit_com() {
    unsafe { CoUninitialize() };
}

/// 取"指定窗口"的选中项。
///
/// 单独拆出来是为了**可测**——自动化里没法保证资源管理器是前台窗口
/// （`SetForegroundWindow` 受前台锁限制），但可以拿到它的 HWND。
/// 传 frame（`CabinetWClass`）或标签子窗口（`ShellTabWindowClass`）都可以。
pub fn selection_of_hwnd(frame: HWND) -> Selection {
    if frame.is_invalid() {
        return Selection::default();
    }
    let shell_windows: IShellWindows = match unsafe {
        CoCreateInstance::<_, IShellWindows>(&CLSID_SHELL_WINDOWS, None, CLSCTX_ALL)
    } {
        Ok(w) => w,
        Err(e) => {
            log_warn!(
                SCOPE,
                "创建 ShellWindows 失败：{e} (HRESULT=0x{:08X})",
                e.code().0
            );
            return Selection::default();
        }
    };
    if let Some((paths, source)) = query_shell(&shell_windows, frame) {
        return Selection { paths, source };
    }
    Selection::default()
}

/// 取当前选中项。**不返回 `Result`**：拿不到就是空（这是常态，不是错误）。
pub fn current_selection() -> Selection {
    let fg = unsafe { GetForegroundWindow() };
    if fg.is_invalid() {
        log_debug!(SCOPE, "没有前台窗口");
        return Selection::default();
    }

    let Some(shell_windows) = shell_windows_cached() else {
        log_warn!(SCOPE, "创建 ShellWindows 失败（COM 未初始化？）");
        return Selection::default();
    };

    // 前台窗口通常就是 `CabinetWClass` frame；`query_by_hwnd` 会自己处理
    // "frame ↔ 标签子窗口"的配对，所以这里只需要一条路径。
    if let Some((paths, source)) = query_shell(&shell_windows, fg) {
        return Selection { paths, source };
    }
    Selection::default()
}

fn variant_i4(value: i32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: core::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_I4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { lVal: value },
            }),
        },
    }
}

fn query_shell(shell_windows: &IShellWindows, hwnd: HWND) -> Option<(Vec<PathBuf>, Source)> {
    // 桌面是个例外：**`IShellWindows` 枚举里经常根本没有桌面那一条**
    // （2026-10-30 实测：桌面前台时 count=3，全是资源管理器窗口）。
    // 桌面必须用 `FindWindowSW(SWC_DESKTOP)` 单独取，见 [`query_desktop`]。
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    let root = if root.is_invalid() { hwnd } else { root };
    if is_desktop_class(&class_of(root)) {
        log_debug!(SCOPE, "目标是桌面（{}），走 SWC_DESKTOP", class_of(root));
        return query_desktop(shell_windows);
    }
    query_by_hwnd(shell_windows, hwnd)
}

/// 取桌面（`Progman` / `WorkerW`）的选中项。
///
/// 桌面不能用 `Item(i)` 枚举到——`SWC_DESKTOP` 是唯一可靠入口：
/// `FindWindowSW(&VARIANT::default(), &VARIANT::default(), SWC_DESKTOP, .., SWFO_NEEDDISPATCH)`。
/// 返回的 browser 的 HWND 就是 `Progman`（实测 `resolved_hwnd=0x30322`）。
fn query_desktop(shell_windows: &IShellWindows) -> Option<(Vec<PathBuf>, Source)> {
    // `SWFO_NEEDDISPATCH` 是官方推荐；实测在进程刚起来、shell 视图还没注册好时
    // `FindWindowSW` 会返回 **S_OK + 空 IDispatch**（`windows` crate 把它变成
    // `Err(code=0)`），重试一两次就好。带/不带标志各试。
    let flags = [SWFO_NEEDDISPATCH, ShellWindowFindWindowOptions(0)];
    for attempt in 0..3 {
        for flag in flags {
            let locator = VARIANT::default();
            let root = VARIANT::default();
            let mut resolved = 0i32;
            let result = unsafe {
                shell_windows.FindWindowSW(&locator, &root, SWC_DESKTOP, &mut resolved, flag)
            };
            match result {
                Ok(dispatch) => {
                    let browser = match browser_from_dispatch(&dispatch) {
                        Some(b) => b,
                        None => {
                            log_debug!(SCOPE, "桌面 browser 转换失败（flags={}）", flag.0);
                            continue;
                        }
                    };
                    if let Some(paths) = paths_from_browser(&browser) {
                        log_debug!(
                            SCOPE,
                            "命中桌面 browser hwnd=0x{:X} flags={} attempt={attempt}",
                            resolved,
                            flag.0
                        );
                        return Some((paths, Source::Desktop));
                    }
                }
                Err(e) => log_debug!(
                    SCOPE,
                    "FindWindowSW(SWC_DESKTOP) 失败：0x{:08X} flags={} attempt={attempt}",
                    e.code().0,
                    flag.0
                ),
            }
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
    }
    None
}

/// 把一个 `IShellWindows` 条目（`IDispatch`）转成 `IShellBrowser`。
fn browser_from_dispatch(dispatch: &IDispatch) -> Option<IShellBrowser> {
    let provider: IServiceProvider = dispatch.cast().ok()?;
    unsafe { provider.QueryService(&SID_S_SHELL_BROWSER) }.ok()
}

/// `IShellWindows` 里的一条注册项：浏览器窗口 HWND + 它的 `IShellBrowser`。
struct BrowserEntry {
    hwnd: HWND,
    browser: IShellBrowser,
}

thread_local! {
    /// 复用的 `IShellWindows`（1.7.6）：`CoCreateInstance` 每次都要几毫秒，
    /// 跟随轮询是每 90 ms 一次，缓存下来最省。COM 接口是线程局部的，所以按线程存。
    static SHELL_WINDOWS: core::cell::RefCell<Option<IShellWindows>> =
        const { core::cell::RefCell::new(None) };
    /// 上次成功命中的 browser（目标 HWND 的顶层祖先 → IShellBrowser）。
    /// 键用 `GA_ROOT`：前台窗口常常是标签子窗口，命中过 frame 之后同一个窗口里换文件
    /// 也能直接复用这个 browser。
    static LAST_BROWSER: core::cell::RefCell<Option<(isize, IShellBrowser)>> =
        const { core::cell::RefCell::new(None) };
}

/// 取（必要时创建）本线程复用的 `IShellWindows`。
fn shell_windows_cached() -> Option<IShellWindows> {
    SHELL_WINDOWS.with(|cell| {
        if let Some(ws) = cell.borrow().as_ref() {
            return Some(ws.clone());
        }
        let created: IShellWindows =
            unsafe { CoCreateInstance(&CLSID_SHELL_WINDOWS, None, CLSCTX_ALL) }.ok()?;
        *cell.borrow_mut() = Some(created.clone());
        Some(created)
    })
}

/// 缓存里的 browser 还能不能取出选区（取到空集也算失效）。
fn try_cached_browser(target: HWND) -> Option<Vec<PathBuf>> {
    let key = root_key(target);
    LAST_BROWSER.with(|cell| {
        let borrow = cell.borrow();
        let (cached, browser) = borrow.as_ref()?;
        if *cached != key.0 {
            return None;
        }
        let paths = paths_from_browser(browser)?;
        if paths.is_empty() {
            return None;
        }
        Some(paths)
    })
}

fn remember_browser(target: HWND, browser: &IShellBrowser) {
    let key = root_key(target);
    LAST_BROWSER.with(|cell| {
        *cell.borrow_mut() = Some((key.0, browser.clone()));
    });
}

/// 缓存键：顶层祖先的句柄值（`GA_ROOT` 失败就用自己）。
fn root_key(hwnd: HWND) -> (isize, HWND) {
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    let root = if root.is_invalid() { hwnd } else { root };
    (root.0 as isize, root)
}

/// 枚举 `IShellWindows` 的所有注册项。
///
/// **为什么不用 `FindWindowSW` 定位**（2026-10-29 实测教训）：在 Win10 19045 上
/// `FindWindowSW(hwnd)` 会把 **桌面** 的 browser 还给你（传入 `0x10F6C` 解析成
/// `0xC08D8`，即 `Progman` 的那条），于是 `GetSelection` 拿到的是桌面选区，
/// `GetDisplayName` 直接 `E_INVALIDARG`（0x80070057）。
/// 逐条 `Item(i)` + `IOleWindow::GetWindow()` 反查 HWND 才是可靠做法：
/// 同一份诊断里 11 个 browser 全部拿到了正确路径。
fn collect_browsers(shell_windows: &IShellWindows) -> Vec<BrowserEntry> {
    let mut out = Vec::new();
    let count = unsafe { shell_windows.Count().unwrap_or(0) };
    for index in 0..count {
        let idx = variant_i4(index);
        let dispatch = match unsafe { shell_windows.Item(&idx) } {
            Ok(d) => d,
            Err(e) => {
                log_debug!(
                    SCOPE,
                    "ShellWindows.Item({index}) 失败：0x{:08X}",
                    e.code().0
                );
                continue;
            }
        };
        let provider: IServiceProvider = match dispatch.cast() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let browser: IShellBrowser = match unsafe { provider.QueryService(&SID_S_SHELL_BROWSER) } {
            Ok(b) => b,
            Err(_) => continue,
        };
        let hwnd = match unsafe { IOleWindow::GetWindow(&browser) } {
            Ok(h) => h,
            Err(_) => continue,
        };
        out.push(BrowserEntry { hwnd, browser });
    }
    let list = out
        .iter()
        .map(|e| format!("0x{:X}", e.hwnd.0 as isize))
        .collect::<Vec<_>>()
        .join(",");
    log_debug!(SCOPE, "枚举到 {} 个 shell browser：[{list}]", out.len());
    out
}

/// 按 HWND 匹配一个 browser 并取它的选中项。
///
/// 候选顺序：① HWND 完全相同（传进来的就是 `ShellTabWindowClass`）；
/// ② 两边顶层祖先（`GA_ROOT`）相同（传进来的是 `CabinetWClass` frame，
///    browser 是它的标签子窗口），可见的标签优先。
/// 逐个尝试而不是"选中第一个候选"，因为实测里有的标签窗口
/// `GetSelection` 返回 `0x80070490`（`ERROR_NOT_FOUND`，视图还没建好）而兄弟标签能出结果。
fn query_by_hwnd(shell_windows: &IShellWindows, target: HWND) -> Option<(Vec<PathBuf>, Source)> {
    if target.is_invalid() {
        return None;
    }
    // 1.7.6：先用**上次命中的那个 browser** 直接取一次，取到就返回。
    // 枚举成本不低（CoCreateInstance + 逐条 Item(i) + QueryService + GetWindow），
    // 而跟随选中项是每 90 ms 一次的轮询，最热的场景恰恰是"同一个窗口里换文件"；
    // 缓存命中时整条路只剩一次 GetSelection。失效（切标签/窗口关了/空选区）就走完整枚举。
    if let Some(paths) = try_cached_browser(target) {
        log_debug!(SCOPE, "命中缓存 browser（hwnd=0x{:X}）", target.0 as isize);
        return Some((paths, Source::Tab));
    }
    let entries = collect_browsers(shell_windows);

    // ① 精确命中。
    if let Some(entry) = entries.iter().find(|e| e.hwnd == target) {
        if let Some(paths) = paths_from_browser(&entry.browser) {
            remember_browser(target, &entry.browser);
            log_debug!(SCOPE, "命中 browser hwnd=0x{:X}（精确）", target.0 as isize);
            return Some((paths, Source::Frame));
        }
        log_debug!(
            SCOPE,
            "精确命中 browser hwnd=0x{:X} 但取不到选区，降级找同根标签",
            target.0 as isize
        );
    }

    // ② 同根（frame ↔ 标签子窗口）。
    let root = unsafe { GetAncestor(target, GA_ROOT) };
    if root.is_invalid() {
        return None;
    }
    let target_is_desktop = is_desktop_class(&class_of(root));
    let same_root: Vec<&BrowserEntry> = entries
        .iter()
        .filter(|e| {
            if e.hwnd == target {
                return false;
            }
            let entry_root = unsafe { GetAncestor(e.hwnd, GA_ROOT) };
            // 桌面比较特殊：选中桌面图标时前台可能是 `Progman`，而 shell window 挂在
            // `WorkerW` 上（或反过来）。两者是同一个"桌面"，不能按 HWND 相等来比。
            entry_root == root || (target_is_desktop && is_desktop_class(&class_of(entry_root)))
        })
        .collect();
    if same_root.is_empty() {
        return None;
    }
    // 活动标签是可见的那个，优先试它，再按顺序兜底。
    let mut ordered: Vec<&&BrowserEntry> = same_root.iter().collect();
    ordered.sort_by_key(|e| !unsafe { IsWindowVisible(e.hwnd).as_bool() });
    for entry in ordered {
        if let Some(paths) = paths_from_browser(&entry.browser) {
            remember_browser(entry.hwnd, &entry.browser);
            log_debug!(
                SCOPE,
                "命中 browser hwnd=0x{:X}（同根 0x{:X}）",
                entry.hwnd.0 as isize,
                root.0 as isize
            );
            return Some((paths, Source::Tab));
        }
    }
    None
}

/// 读窗口类名（失败返回空串）。
fn class_of(hwnd: HWND) -> String {
    if hwnd.is_invalid() {
        return String::new();
    }
    let mut buf = [0u16; 128];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len <= 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&buf[..len as usize])
    }
}

/// 桌面窗口的两种类名。
fn is_desktop_class(name: &str) -> bool {
    name.eq_ignore_ascii_case("Progman") || name.eq_ignore_ascii_case("WorkerW")
}

/// `IShellBrowser` → `IShellView` → `IFolderView` → 选中项。
fn paths_from_browser(browser: &IShellBrowser) -> Option<Vec<PathBuf>> {
    unsafe {
        let view = match browser.QueryActiveShellView() {
            Ok(v) => v,
            Err(e) => {
                log_debug!(SCOPE, "QueryActiveShellView 失败：0x{:08X}", e.code().0);
                return None;
            }
        };
        let folder: IFolderView2 = match view.cast() {
            Ok(f) => f,
            Err(e) => {
                log_debug!(SCOPE, "cast IFolderView2 失败：0x{:08X}", e.code().0);
                return None;
            }
        };
        // fnoneimpliesfolder=false：选区为空时不要"把文件夹自己当选中项"。
        let items = match folder.GetSelection(false) {
            Ok(items) => items,
            // S_FALSE / E_FAIL：没有选区，属于常态。
            Err(e) => {
                log_debug!(SCOPE, "GetSelection 返回：HRESULT=0x{:08X}", e.code().0);
                return None;
            }
        };
        let count = items.GetCount().unwrap_or(0) as u32;
        log_debug!(SCOPE, "GetSelection 命中 {count} 项");
        if count == 0 {
            return None;
        }

        // 有些项（桌面上的"此电脑"、控制面板项）没有文件系统路径，
        // 退化成"文件夹 + 相对名"再拼。桌面的文件夹就是 `C:\Users\<用户>\Desktop`。
        let folder_path = folder
            .GetFolder::<IShellItem>()
            .ok()
            .and_then(|f| display_name(&f, SIGDN_FILESYSPATH))
            .map(PathBuf::from);

        let mut paths = Vec::new();
        for index in 0..count.min(MAX_ITEMS) {
            let item = match items.GetItemAt(index) {
                Ok(i) => i,
                Err(e) => {
                    log_debug!(SCOPE, "GetItemAt({index}) 失败：0x{:08X}", e.code().0);
                    continue;
                }
            };
            let resolved = match display_name(&item, SIGDN_FILESYSPATH) {
                Some(path) => Some(PathBuf::from(path)),
                None => {
                    // 没有绝对路径 → 用"文件夹 + 相对解析名"兜一层。
                    let relative = display_name(&item, SIGDN_PARENTRELATIVEPARSING);
                    let joined = match (&folder_path, relative) {
                        (Some(dir), Some(name)) => Some(dir.join(name)),
                        _ => None,
                    };
                    if joined.is_none() {
                        log_debug!(SCOPE, "选中项[{index}] 无法解析成文件系统路径，跳过");
                    }
                    joined
                }
            };
            match resolved {
                Some(path) if path.exists() => {
                    log_debug!(SCOPE, "选中项[{index}] = {}", path.display());
                    paths.push(path);
                }
                Some(path) => log_debug!(
                    SCOPE,
                    "选中项[{index}] 拼出的路径不存在，跳过：{}",
                    path.display()
                ),
                None => {}
            }
        }
        (!paths.is_empty()).then_some(paths)
    }
}

/// 取 `IShellItem` 的某个显示名（`SIGDN_*`），并释放 COM 分配的内存。
///
/// 失败返回 `None`——**这是常态**：非文件系统项（"此电脑"、"回收站"）没有
/// `SIGDN_FILESYSPATH`，桌面那条注册项在 Win10 上也会直接 `E_INVALIDARG`。
unsafe fn display_name(item: &IShellItem, sigdn: SIGDN) -> Option<String> {
    unsafe {
        let name = item.GetDisplayName(sigdn).ok()?;
        let text = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const core::ffi::c_void));
        match text {
            Some(s) if !s.is_empty() && s.len() < MAX_PATH as usize * 4 => Some(s),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_selection_summary_is_stable() {
        let s = Selection::default();
        assert!(s.is_empty());
        assert_eq!(s.summary(), "source=none count=0");
        assert_eq!(s.first(), None);
    }

    #[test]
    fn summary_mentions_source_and_count() {
        let s = Selection {
            paths: vec![PathBuf::from(r"C:\a\b.png")],
            source: Source::Tab,
        };
        assert!(s.summary().contains("source=tab"));
        assert!(s.summary().contains("count=1"));
        assert!(s.summary().contains("b.png"));
    }

    #[test]
    fn variant_i4_sets_type_and_value() {
        let v = variant_i4(0x1234);
        unsafe {
            let inner = &*v.Anonymous.Anonymous;
            assert_eq!(inner.vt, VT_I4);
            assert_eq!(inner.Anonymous.lVal, 0x1234);
        }
    }
}
