//! 压缩包目录（1.7.0）：只读中央目录，不依赖任何第三方 crate。
//!
//! 为什么不引 `zip` crate：宿主对体积和启动时间敏感，读目录只需要解析
//! End-Of-Central-Directory + 每条 Central Directory File Header，约一百行代码就够，
//! 没必要为此把整个 deflate 实现链进主程序（压缩包内容我们不解压）。
//!
//! 目录项按**路径前缀**推断：很多打包器（含 python `zipfile`）根本不写目录条目，
//! 只按 `entry.is_dir()` 统计会得到"0 个文件夹"。

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// 安全上限：中央目录最大字节数（`cd_size` 来自文件内容，恶意 ZIP 可声明近 4 GB）。
const MAX_CD_BYTES: usize = 64 * 1024 * 1024;
/// 中央目录最多条目数（EOCD 的 16 位 count 本就 ≤ 65535，这里留余量兜底）。
const MAX_ENTRIES: usize = 100_000;
/// 单条目文件名最大长度（防畸形包把名字撑爆内存）。
const MAX_NAME_LEN: usize = 4096;

/// 中央目录里的一个条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// 包内路径（统一用 `/`）。
    pub path: String,
    /// 解压后字节数。
    pub size: u64,
    /// 修改时间（DOS 时间戳换算成 `YYYY/MM/DD HH:MM`，取不到就是空串）。
    pub time: String,
    /// 是否是目录（末尾带 `/` 的条目）。
    pub is_dir: bool,
    /// 层级（根下文件 = 0）。
    pub depth: usize,
}

/// 解析结果：条目 + 统计。
#[derive(Clone, Debug, Default)]
pub struct Archive {
    pub entries: Vec<Entry>,
    /// 目录数量（按路径前缀去重统计）。
    pub dirs: usize,
    /// 文件数量。
    pub files: usize,
    /// 解压后总字节数。
    pub uncompressed: u64,
}

/// 读一个压缩包的目录。不是 zip / 截断 / 加密包都返回 `Err`（调用方据此退回别的通道）。
pub fn open(path: &Path) -> Result<Archive, String> {
    let mut file = File::open(path).map_err(|e| format!("打不开：{e}"))?;
    let len = file
        .metadata()
        .map(|m| m.len())
        .map_err(|e| e.to_string())?;
    if len < 22 {
        return Err("文件太小，不是 zip".to_string());
    }
    // EOCD 在最末尾 22..(22+65535) 字节里，从尾部往前找签名。
    let tail_len = len.min(66_000) as usize;
    let mut tail = vec![0u8; tail_len];
    file.seek(SeekFrom::End(-(tail_len as i64)))
        .map_err(|e| e.to_string())?;
    file.read_exact(&mut tail).map_err(|e| e.to_string())?;
    let mut eocd = None;
    for i in (0..tail.len().saturating_sub(21)).rev() {
        if tail[i..i + 4] == [0x50, 0x4b, 0x05, 0x06] {
            eocd = Some(i);
            break;
        }
    }
    let Some(eocd) = eocd else {
        return Err("找不到 zip 中央目录结尾".to_string());
    };
    let count = u16::from_le_bytes([tail[eocd + 10], tail[eocd + 11]]) as usize;
    let cd_size = u32::from_le_bytes([
        tail[eocd + 12],
        tail[eocd + 13],
        tail[eocd + 14],
        tail[eocd + 15],
    ]) as usize;
    let cd_offset = u32::from_le_bytes([
        tail[eocd + 16],
        tail[eocd + 17],
        tail[eocd + 18],
        tail[eocd + 19],
    ]) as u64;
    if cd_size == 0 || cd_offset >= len {
        return Ok(Archive::default());
    }
    // 安全上限：cd_size 来自文件内容，恶意包可声明接近 4 GB，直接分配会 OOM。
    if cd_size > MAX_CD_BYTES {
        return Err(format!(
            "压缩包中央目录过大（{cd_size} 字节，上限 {MAX_CD_BYTES}）"
        ));
    }
    if cd_offset
        .checked_add(cd_size as u64)
        .is_none_or(|end| end > len)
    {
        return Err("压缩包中央目录越界".to_string());
    }
    let mut cd = vec![0u8; cd_size];
    file.seek(SeekFrom::Start(cd_offset))
        .map_err(|e| e.to_string())?;
    file.read_exact(&mut cd).map_err(|e| e.to_string())?;

    let mut archive = Archive::default();
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    let mut at = 0usize;
    for _ in 0..count {
        if archive.entries.len() >= MAX_ENTRIES {
            break;
        }
        if at + 46 > cd.len() || cd[at..at + 4] != [0x50, 0x4b, 0x01, 0x02] {
            break;
        }
        let name_len = u16::from_le_bytes([cd[at + 28], cd[at + 29]]) as usize;
        if name_len > MAX_NAME_LEN {
            break;
        }
        let extra_len = u16::from_le_bytes([cd[at + 30], cd[at + 31]]) as usize;
        let comment_len = u16::from_le_bytes([cd[at + 32], cd[at + 33]]) as usize;
        let size = u32::from_le_bytes([cd[at + 24], cd[at + 25], cd[at + 26], cd[at + 27]]) as u64;
        let time = u16::from_le_bytes([cd[at + 12], cd[at + 13]]);
        let date = u16::from_le_bytes([cd[at + 14], cd[at + 15]]);
        let name_start = at + 46;
        if name_start + name_len > cd.len() {
            break;
        }
        let raw = &cd[name_start..name_start + name_len];
        let name = decode_name(raw);
        at = name_start + name_len + extra_len + comment_len;
        if name.is_empty() {
            continue;
        }
        let is_dir = name.ends_with('/');
        let clean = name.trim_end_matches('/').to_string();
        if is_dir {
            // 拥有型 String：不同压缩包之间不再累积泄漏（去重交给集合）。
            dirs.insert(clean.clone());
        } else {
            archive.files += 1;
            archive.uncompressed += size;
            // 文件所在的每一级目录都记上（打包器可能不写目录条目）。
            let mut prefix = String::new();
            let parts: Vec<&str> = clean.split('/').collect();
            for part in parts.iter().take(parts.len().saturating_sub(1)) {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(part);
                // 只有新前缀才 leak：5000 个文件挤在同几级目录下时，
                // 无条件 leak 会让泄漏量随条目数线性涨（KB～MB 级）。
                dirs.insert(prefix.clone());
            }
        }
        let depth = clean.matches('/').count();
        archive.entries.push(Entry {
            path: clean,
            size,
            time: dos_time(date, time),
            is_dir,
            depth,
        });
    }
    archive.dirs = dirs.len();
    archive.entries.sort_by_key(|e| e.path.to_lowercase());
    Ok(archive)
}

fn decode_name(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(text) => text.to_string(),
        // 非 UTF-8（老工具打的中文包）按 GB18030 有损解码太重，这里退化成替换字符，
        // 至少不丢条目。
        Err(_) => String::from_utf8_lossy(raw).into_owned(),
    }
}

/// DOS 日期/时间 → `YYYY/MM/DD HH:MM`（解析失败给空串，不 panic）。
fn dos_time(date: u16, time: u16) -> String {
    let year = 1980 + ((date >> 9) & 0x7f);
    let month = (date >> 5) & 0x0f;
    let day = date & 0x1f;
    let hour = (time >> 11) & 0x1f;
    let minute = (time >> 5) & 0x3f;
    if month == 0 || day == 0 {
        return String::new();
    }
    format!("{year}/{month:02}/{day:02} {hour:02}:{minute:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dos_time_is_formatted() {
        // 2026-11-01 12:05
        let date = ((2026 - 1980) << 9) | (11 << 5) | 1;
        let time = (12 << 11) | (5 << 5);
        assert_eq!(dos_time(date, time), "2026/11/01 12:05");
    }

    #[test]
    fn zero_date_is_empty() {
        assert_eq!(dos_time(0, 0), "");
    }

    #[test]
    fn missing_file_is_an_error() {
        assert!(open(Path::new(r"C:\definitely\not\here.zip")).is_err());
    }

    #[test]
    fn oversized_central_directory_is_rejected() {
        // 只有 EOCD、却声明 cd_size = 0xFFFFFFFF 的假 zip：必须优雅报错而不是分配 4 GB。
        let mut data = vec![0u8; 22];
        data[0..4].copy_from_slice(&[0x50, 0x4b, 0x05, 0x06]);
        data[12..16].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../target/zip_bomb_{}.zip", std::process::id()));
        std::fs::write(&dir, &data).unwrap();
        let r = open(&dir);
        let _ = std::fs::remove_file(&dir);
        assert!(r.is_err(), "超大 cd_size 必须被拒绝");
    }
}
