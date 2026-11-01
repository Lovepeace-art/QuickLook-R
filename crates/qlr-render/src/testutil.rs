//! 测试用临时目录。
//!
//! **不要用 `std::env::temp_dir()`**：本机（以及这套开发环境）的 `%TEMP%` 写入会被拒
//! （`Os { code: 5, PermissionDenied }`），早期 spike 也是因为同样原因把日志写回仓库目录。
//! 统一落到 workspace 的 `target/test-tmp/`，已被 `.gitignore` 覆盖。

use std::path::PathBuf;

/// 取（并创建）workspace 下的 `target/test-tmp/<name>`。
pub fn dir(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/test-tmp")
        .join(name);
    std::fs::create_dir_all(&path).expect("创建测试临时目录失败");
    path
}

/// 在临时目录里写一个文件，返回路径。
pub fn write_file(name: &str, file: &str, data: &[u8]) -> PathBuf {
    let path = dir(name).join(file);
    std::fs::write(&path, data).expect("写测试文件失败");
    path
}
