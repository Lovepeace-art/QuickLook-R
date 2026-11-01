//! 系统「预览处理程序」（`IPreviewHandler`）——Office 文档借宿主自己的渲染器显示（1.4.1）。
//!
//! 为什么要它：我们自己的 Office 预览只是"抽文字画一张卡片"，而**系统里已经装好的
//! 预览处理器**（Word/Excel/PowerPoint 各一个）能画出真排版。上游 QuickLook 的 Office
//! 预览走的也是同一条路：不自己实现排版，直接借系统的。
//!
//! 机制（和资源管理器预览窗格完全一样）：
//! 1. 从注册表拿到该扩展名的预览处理器 CLSID：
//!    `HKCR\<.ext>\ShellEx\{8895b1c6-b41f-4c1c-a562-0d564250836f}`
//! 2. `CoCreateInstance` 出 `IPreviewHandler`（Office 的实现在 prevhost 代理进程里）。
//! 3. 用 `IInitializeWithFile` / `IInitializeWithItem` / `IInitializeWithStream` 里
//!    任意一个喂文件路径。
//! 4. `SetWindow(父窗口, 矩形)` + `DoPreview()`：处理器自己在这个位置开子窗口画内容。
//!
//! 约束（都踩过或必然遇到）：
//! - 处理器是**别人的窗口**，我们只能给它一个矩形；它不认我们的滚动/缩放/目录。
//! - 换文件前必须 `Unload()`，否则上一个文件的子窗口会留着。
//! - 一切失败都要能安静退回"我们自己的插件卡片"，不能让预览窗空着。

use std::path::Path;

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CLSCTX_LOCAL_SERVER, CLSIDFromString, CoCreateInstance,
};
use windows::Win32::System::Registry::{HKEY_CLASSES_ROOT, RRF_RT_REG_SZ, RegGetValueW};
use windows::Win32::UI::Shell::IPreviewHandler;
use windows::Win32::UI::Shell::PropertiesSystem::{IInitializeWithFile, IInitializeWithStream};
use windows::core::{HSTRING, Interface, PCWSTR};

/// 预览处理器的 ShellEx 键（微软固定的 GUID，所有预览处理器都注册在这里）。
const PREVIEW_HANDLER_KEY: &str = "ShellEx\\{8895b1c6-b41f-4c1c-a562-0d564250836f}";

/// 一个活着的系统预览处理器。
pub struct ShellPreview {
    handler: IPreviewHandler,
    /// 我们自己建的"内容宿主"子窗口：处理器往里铺，不会盖住顶栏。
    host: HWND,
    /// 宿主窗口在父窗口客户区里的矩形（用来判断要不要挪）。
    rect: RECT,
    /// 处理器真正画内容的那层窗口（1.7.4：PPT 的翻页键要投给它）。
    /// `DoPreview()` 之后才有，所以这里存 `Option`，用完即失效。
    nav: Option<HWND>,
    /// 输入是否已经被我们关掉（关掉后鼠标事件会回落到预览窗自己身上）。
    input_off: bool,
}

// SAFETY：处理器只在 UI 线程（STA 套间）创建和使用，从不跨线程调用。
// 加这个只是因为要把它放进 `State` 的 `Mutex` 里；真正的线程约束写在 Drop/close 的注释里。
unsafe impl Send for ShellPreview {}

impl ShellPreview {
    /// 用系统预览处理器打开 `path`，画在 `parent` 的 `rect`（父窗口客户区坐标）里。
    ///
    /// 返回 `None` 表示"这台机器上没有可用的预览处理器 / 初始化失败"，调用方应该
    /// 退回自己的渲染（插件卡片）。
    pub fn open(path: &Path, parent: HWND, rect: RECT) -> Option<Self> {
        let clsid = handler_clsid(path)?;
        let handler: IPreviewHandler = unsafe {
            // Office 的预览器通常注册为进程内服务器，但实际由 prevhost 代理承载：
            // 两个上下文都允许，让 COM 自己决定。
            CoCreateInstance(&clsid, None, CLSCTX_LOCAL_SERVER | CLSCTX_INPROC_SERVER).ok()?
        };

        if !initialize(&handler, path) {
            // 初始化不了就别让它半死不活地留在内存里。
            let _ = unsafe { handler.Unload() };
            return None;
        }

        // 关键：不能把主窗口直接交给处理器——它会按父窗口客户区铺满，把顶栏一起盖掉。
        // 先建一个只占内容区的子窗口，处理器再怎么铺也只铺在内容区。
        let Some(host) = crate::window::create_child_host(parent, rect) else {
            let _ = unsafe { handler.Unload() };
            return None;
        };
        let local = RECT {
            left: 0,
            top: 0,
            right: rect.right - rect.left,
            bottom: rect.bottom - rect.top,
        };
        unsafe {
            // 先把尺寸告诉它，再 DoPreview：顺序反了会出现"内容按 0×0 排版"的怪状。
            if handler.SetWindow(host, &local).is_err() || handler.DoPreview().is_err() {
                let _ = handler.Unload();
                crate::window::destroy(host);
                return None;
            }
        }
        qlr_core::log_debug!("shellpreview", "预览处理器已启动：{}", path.display());
        // 处理器铺内容的那层窗口是它自己建的，`DoPreview` 之后才存在；按面积挑最大的那个
        // （实测投给别的层没反应，投给这一层才认翻页键）。
        let nav = crate::window::largest_descendant(host);
        if nav.is_none() {
            qlr_core::log_debug!(
                "shellpreview",
                "找不到处理器的内容窗口（翻页键可能投不进去）"
            );
        }
        Some(Self {
            handler,
            host,
            rect,
            nav,
            input_off: false,
        })
    }

    /// 把处理器的输入关掉（1.7.4）：从此鼠标点击一律回落到预览窗自己身上。
    ///
    /// 两个用途：① 拖边改窗口大小在 Office 预览里也能用（否则子窗口会盖住边缘热区）；
    /// ② PPT 的翻页由我们计数，用户点一下处理器就翻页会让计数立刻失准。
    /// 处理器照样画（实测 PowerPoint 被禁用输入后仍然正常渲染、也还认我们投的按键）。
    pub fn take_over_input(&mut self) {
        if self.input_off {
            return;
        }
        crate::window::set_child_input(self.host, false);
        self.input_off = true;
        qlr_core::log_debug!("shellpreview", "已接管输入（子窗口只显示、不收鼠标）");
    }

    /// 把这个预览的子窗口提到同级最上层（1.7.6）。
    ///
    /// 换文件时用它让**旧**内容继续盖着，等新的处理器画出第一帧再卸掉旧的——
    /// 否则中间那几十到几百毫秒会露出空白（Office 预览的"割裂感"）。
    pub fn raise(&self) {
        crate::window::raise_child(self.host);
    }

    /// 内容子窗口的句柄（1.7.9）。
    ///
    /// 调用方**必须**在拿锁的代码块之外用它：`SetFocus` 会把激活/焦点消息泵回我们自己的
    /// 消息循环，重入的处理器再去拿同一把 `Mutex` 就是自锁死（CONVENTIONS #128）。
    /// 所以这里只在锁内取出句柄，真正调 `SetFocus` 时锁已经放开。
    pub fn nav_target(&self) -> Option<HWND> {
        let target = self.nav?;
        if crate::window::is_window(target) {
            Some(target)
        } else {
            None
        }
    }

    /// 把焦点交给内容子窗口（1.7.8）。
    ///
    /// 为什么需要：Word/Excel 的预览是 OLE 就地激活对象，**容器窗口没激活、焦点没进去，
    /// 滚轮就不响应**（用户实测：先点一下预览窗再滚就正常）。点窗口时我们激活它，
    /// 并把焦点直接塞给处理器的内容层，用户不必点第二次。
    pub fn focus_content(&self) -> bool {
        match self.nav_target() {
            Some(target) => crate::window::focus_child(target),
            None => false,
        }
    }

    /// 让处理器**自己**设焦点（`IPreviewHandler::SetFocus`，1.8.1）。
    ///
    /// 为什么必须走 COM：Excel/Word 的预览是"容器里就地激活的 OLE 对象"。我们只把焦点
    /// `SetFocus` 到它的内容层（`EXCEL9`）时，表格只是"看得见、点不动"——左键点单元格
    /// 选不中（1.8.1 用户反馈）。`IPreviewHandler::SetFocus` 由处理器自己把焦点送进真正的
    /// 文档视图（`EXCEL7`），并顺带完成 OLE 的 UI 激活，点击/选区才生效。
    ///
    /// 调用方**必须**在锁外调它：COM 调用会把消息泵回我们的窗口过程（CONVENTIONS #145）。
    pub fn set_com_focus(&mut self) -> bool {
        match unsafe { self.handler.SetFocus() } {
            Ok(()) => true,
            Err(e) => {
                qlr_core::log_debug!("shellpreview", "IPreviewHandler::SetFocus 失败：{e}");
                false
            }
        }
    }

    /// 内容已经画出来的程度：内容区里"可见后代窗口"最大一块的面积 ÷ 内容区面积。
    ///
    /// 用途（1.7.7）：`DoPreview` 之后要等处理器真的把第一帧画出来，再解除"冻帧"、
    /// 再决定要不要接管输入。按固定 400 ms 猜不准——Excel 实测要 **768 ms**，
    /// 猜早了就会露出占位页（用户反馈"Excel 还有那个占位页"）。
    pub fn visible_area(&self, content: RECT) -> i64 {
        let total =
            ((content.right - content.left) as i64) * ((content.bottom - content.top) as i64);
        if total <= 0 {
            return 0;
        }
        let Some(child) = crate::window::largest_descendant(self.host) else {
            return 0;
        };
        let rc = crate::window::window_rect(child);
        let area = ((rc.right - rc.left) as i64) * ((rc.bottom - rc.top) as i64);
        // 用百分比表示，调用方拿它比阈值（>= 50 就算画出来了）
        (area * 100) / total
    }

    /// 翻一页（`next = true` 下一页）。返回 false = 处理器不认这个按键。
    pub fn step(&self, next: bool) -> bool {
        let Some(target) = self.nav else {
            return false;
        };
        if !crate::window::is_window(target) {
            return false;
        }
        // VK_NEXT = 0x22 / VK_PRIOR = 0x21
        crate::window::post_key(target, if next { 0x22 } else { 0x21 })
    }

    /// 把滚轮转给处理器自己（Word/Excel 的预览靠它滚动）。
    ///
    /// 输入被我们关掉之后 `WindowFromPoint` 只会返回预览窗，原来那条
    /// "找到光标下面的子窗口再转发"的路子就断了（1.7.4），所以这里直接投给内容窗口。
    pub fn forward_wheel(&self, delta: i16) -> bool {
        let Some(target) = self.nav else {
            return false;
        };
        if !crate::window::is_window(target) {
            return false;
        }
        let rc = crate::window::window_rect(target);
        let screen = ((rc.left + rc.right) / 2, (rc.top + rc.bottom) / 2);
        crate::window::post_wheel(target, delta, screen)
    }

    /// 窗口尺寸/位置变了（拖边、缩放、换页导致内容区变化）。
    pub fn set_rect(&mut self, rect: RECT) {
        if rect.left == self.rect.left
            && rect.top == self.rect.top
            && rect.right == self.rect.right
            && rect.bottom == self.rect.bottom
        {
            return;
        }
        self.rect = rect;
        // 宿主子窗口先挪好，再告诉处理器新的（局部）尺寸。
        crate::window::move_child_host(self.host, rect);
        let local = RECT {
            left: 0,
            top: 0,
            right: rect.right - rect.left,
            bottom: rect.bottom - rect.top,
        };
        unsafe {
            if self.handler.SetRect(&local).is_err() {
                // 有些处理器（旧版 Office）不实现 SetRect：退化成重新给一次窗口。
                let _ = self.handler.SetWindow(self.host, &local);
            }
        }
    }

    /// 换文件 / 关窗时调用：卸载处理器（它会自己销毁子窗口）。
    pub fn close(self) {
        unsafe {
            let _ = self.handler.Unload();
            crate::window::destroy(self.host);
        }
    }
}

impl Drop for ShellPreview {
    fn drop(&mut self) {
        // 双保险：忘记显式 close 时也不要把处理器和宿主窗口漏在进程里。
        unsafe {
            let _ = self.handler.Unload();
            crate::window::destroy(self.host);
        }
    }
}

/// 这台机器上有没有给该扩展名注册预览处理器（只是查注册表，很便宜）。
pub fn has_handler(path: &Path) -> bool {
    handler_clsid(path).is_some()
}

/// 查这个文件对应的预览处理器 CLSID。
fn handler_clsid(path: &Path) -> Option<windows::core::GUID> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty())?;
    let subkey = HSTRING::from(format!(".{ext}\\{PREVIEW_HANDLER_KEY}"));
    let mut buf = [0u16; 128];
    let mut len = (buf.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CLASSES_ROOT,
            PCWSTR(subkey.as_ptr()),
            PCWSTR::null(),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            Some(&mut len),
        )
    };
    if status.is_err() {
        qlr_core::log_debug!("shellpreview", "{ext} 没有注册预览处理器");
        return None;
    }
    let text = String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]);
    let text = text.trim().trim_matches('\0').trim();
    if text.is_empty() {
        return None;
    }
    unsafe { CLSIDFromString(&HSTRING::from(text)).ok() }
}

/// 试三种初始化接口（不同处理器实现的支持面不同，按"最具体 → 最宽松"的顺序试）。
fn initialize(handler: &IPreviewHandler, path: &Path) -> bool {
    let wide = HSTRING::from(path.as_os_str().to_string_lossy().into_owned());

    if let Ok(with_file) = handler.cast::<IInitializeWithFile>() {
        // STGM_READ = 0
        if unsafe { with_file.Initialize(&wide, 0) }.is_ok() {
            return true;
        }
    }

    // 流式初始化留给将来（需要 IStream，Office 预览器一般不接受）。
    let _ = handler.cast::<IInitializeWithStream>();
    false
}
