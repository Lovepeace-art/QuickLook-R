//! 把 exe 的图标与版本信息嵌进去。
//!
//! 为什么要做：`qlrust.exe` 原来是"三无"可执行文件——资源管理器里没有图标、
//! 属性里没有版本号、文件说明是空的。用户实测时第一反应就是
//! "运行的时候显示没有签名，我以为没成功"，所以至少在 Windows 的展示层面
//! 让它看起来像个正常程序。
//!
//! 做法：找 Windows SDK 里的 `rc.exe`，把 `resources/qlrust.rc` 编成 `.res`，
//! 再用 `cargo:rustc-link-arg-bins` 塞给链接器（MSVC 的 link.exe 直接吃 .res）。
//! 找不到 rc.exe 就打印 warning 跳过——资源不是功能，缺了也能跑。

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=resources/qlrust.rc");
    println!("cargo:rerun-if-changed=resources/qlrust.ico");
    println!("cargo:rerun-if-changed=resources/qlrust.manifest");

    let rc = match find_rc() {
        Some(path) => path,
        None => {
            println!(
                "cargo:warning=没找到 Windows SDK 的 rc.exe，跳过图标/版本信息嵌入（不影响功能）"
            );
            return;
        }
    };

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let res = out_dir.join("qlrust.res");

    let status = Command::new(&rc)
        .arg("/nologo")
        .arg(format!("/fo{}", res.display()))
        .arg("resources/qlrust.rc")
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("cargo:rustc-link-arg-bins={}", res.display());
        }
        Ok(s) => println!("cargo:warning=rc.exe 退出码 {:?}，跳过资源嵌入", s.code()),
        Err(e) => println!("cargo:warning=调用 rc.exe 失败（{e}），跳过资源嵌入"),
    }
}

/// 在 Windows Kits 下找 x64 的 rc.exe，取版本号最大的那个。
fn find_rc() -> Option<PathBuf> {
    let kits = [
        r"C:\Program Files (x86)\Windows Kits\10\bin",
        r"C:\Program Files\Windows Kits\10\bin",
    ];
    let mut best: Option<(Vec<u32>, PathBuf)> = None;

    for root in kits {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let candidate = entry.path().join("x64").join("rc.exe");
            if !candidate.is_file() {
                continue;
            }
            let version = parse_version(&name);
            if best.as_ref().is_none_or(|(v, _)| version > *v) {
                best = Some((version, candidate));
            }
        }
    }

    best.map(|(_, path)| path)
}

/// 把 `10.0.26100.0` 这种目录名解析成可比较的版本号；解析不了当 0。
fn parse_version(name: &str) -> Vec<u32> {
    name.split('.')
        .map(|part| part.parse::<u32>().unwrap_or(0))
        .collect()
}

#[allow(dead_code)]
fn exists(path: &Path) -> bool {
    path.is_file()
}
