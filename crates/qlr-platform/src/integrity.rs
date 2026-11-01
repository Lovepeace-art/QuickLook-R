//! 进程令牌的完整性级别（integrity level）。
//!
//! 为什么要读这个：Windows 的 UIPI（用户界面特权隔离）**不是**按"是否管理员"分，
//! 而是按完整性级别分。低完整性（Low）进程不能向普通（Medium）进程发窗口消息，
//! 于是这些操作全部会被拒成 `0x80070005 拒绝访问`：
//!
//! - `Shell_NotifyIconW`（加托盘图标）
//! - `CoCreateInstance(CLSID_ShellWindows)`（读选区）
//!
//! 实测踩过的坑：DSH 文件沙箱会给**工作目录内**的可执行文件打一个
//! `Mandatory Label\Low Mandatory Level` 标签（`icacls <exe>` 能看到
//! `Mandatory Label\Low Mandatory Level:(I)(NW)`）。从这种目录里启动的 exe
//! 继承低完整性，托盘与选区双双失败，表现为"程序起来了但托盘没图标"。
//! 复制到普通目录（如 `D:\qlrust\`）后标签消失，一切正常。
//!
//! 详见 `docs/dev/CONVENTIONS.md` 坑 #29。

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    GetTokenInformation, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// 完整性级别。数值是 SID 的最后一个 SubAuthority（`SECURITY_MANDATORY_*_RID`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Integrity {
    Untrusted,
    Low,
    Medium,
    MediumPlus,
    High,
    System,
    Protected,
    Other(u32),
}

impl Integrity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Untrusted => "untrusted",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::MediumPlus => "medium-plus",
            Self::High => "high",
            Self::System => "system",
            Self::Protected => "protected",
            Self::Other(_) => "other",
        }
    }

    /// 低于 Medium 的级别没法跟 explorer 通信 —— 托盘图标与 Shell 选区都会失败。
    pub fn below_medium(self) -> bool {
        matches!(self, Self::Untrusted | Self::Low | Self::Other(_))
    }
}

fn from_rid(rid: u32) -> Integrity {
    match rid {
        0x0000 => Integrity::Untrusted,
        0x1000 => Integrity::Low,
        0x2000 => Integrity::Medium,
        0x2100 => Integrity::MediumPlus,
        0x3000 => Integrity::High,
        0x4000 => Integrity::System,
        0x5000 => Integrity::Protected,
        other => Integrity::Other(other),
    }
}

/// 读当前进程的完整性级别。失败返回 `None`（只用于诊断，不该阻塞启动）。
pub fn current() -> Option<Integrity> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;

        // 先问需要多大，再按需要大小分配。
        let mut needed = 0u32;
        let _ = GetTokenInformation(
            token,
            TOKEN_INFORMATION_CLASS(25), // TokenIntegrityLevel
            None,
            0,
            &mut needed,
        );
        if needed == 0 {
            let _ = CloseHandle(token);
            return None;
        }

        let mut buf = vec![0u8; needed as usize];
        let ok = GetTokenInformation(
            token,
            TOKEN_INFORMATION_CLASS(25),
            Some(buf.as_mut_ptr().cast()),
            needed,
            &mut needed,
        );
        let _ = CloseHandle(token);
        ok.ok()?;

        // TOKEN_MANDATORY_LABEL { Label: SID_AND_ATTRIBUTES { Sid: PSID, Attributes } }
        // SID 的 SubAuthority[0] 在偏移 8（u8 Revision + u8 SubAuthorityCount + 6 字节 IA）。
        let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let sid = label.Label.Sid.0 as *const u8;
        if sid.is_null() {
            return None;
        }
        let sub_count = *sid.add(1);
        if sub_count == 0 {
            return None;
        }
        let rid = std::ptr::read_unaligned(sid.add(8) as *const u32);
        Some(from_rid(rid))
    }
}

/// 启动日志用的可读描述，例如 `high` 或 `low（低于 medium：托盘与 Shell 选区都会被拒）`。
pub fn describe() -> String {
    match current() {
        Some(level) if level.below_medium() => format!(
            "{}（低于 medium：托盘图标与 Shell 选区都会被 UIPI 拒绝，常见原因是 exe 放在受限目录里）",
            level.as_str()
        ),
        Some(level) => level.as_str().to_string(),
        None => "未知".to_string(),
    }
}
