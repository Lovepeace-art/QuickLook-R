# 发布与签名（QLRust 1.0.0）

## 1. 发布步骤（一条命令）

```powershell
pwsh -NoProfile -File scripts\pack.ps1 -Version 1.0.0
```

它做四件事：

1. `cargo build --release`（除非 `-SkipBuild`）；
2. 组 `dist\qlrust-1.0.0-win64\`：`qlrust.exe`、`pdfium.dll`（`-NoPdfium` 可省）、`plugins\qlr_plugin_demo.dll`、`README.txt`、`portable.lock`、`SHA256SUMS.txt`；
3. 打 `dist\qlrust-1.0.0-win64.zip`；
4. 打印每个文件的字节数与 SHA-256。

版本号有**三处**必须同步（`pack.ps1` 只读第一处，另两处要手动改）：

| 位置 | 字段 |
|---|---|
| `Cargo.toml` | `[workspace.package] version` |
| `crates/qlr-app/resources/qlrust.rc` | `FILEVERSION` / `PRODUCTVERSION` / `FileVersion` / `ProductVersion` |
| `packaging/README.txt` | 性能表与版本段落（如有版本字样） |

验证方式：`(Get-Item dist\qlrust-1.0.0-win64\qlrust.exe).VersionInfo.FileVersion` 应为 `1.0.0.0`。

## 2. 代码签名：现状是没有证书

**QLRust 1.0.0 未做代码签名**，原因很直接：EV/OV 代码签名证书要企业资质 + 每年几百到上千元，个人项目当前不具备。

后果与对策：

| 现象 | 原因 | 对策 |
|---|---|---|
| SmartScreen 弹「Windows 已保护你的电脑」 | 可执行文件无签名 + 下载量小 | 用户点「更多信息」→「仍要运行」；发布页直接给出 SHA-256 供核对 |
| 文件名带 `(1)`、Mark-of-the-Web | 浏览器下载的 zip 会带 Zone.Identifier | 解压后仍带标记；`Unblock-File` 可清 |
| 企业环境被 AppLocker 拦 | 未签名的 exe | 只能由管理员加白名单，无法绕过 |

发布页给用户的核对命令：

```powershell
Get-FileHash .\qlrust-1.0.0-win64.zip -Algorithm SHA256
Get-Content .\qlrust-1.0.0-win64\SHA256SUMS.txt
```

## 3. 将来怎么签（清单，照做即可）

1. 买证书：**OV**（便宜、仍需攒信誉）或 **EV**（贵，但 SmartScreen 立刻转正、且支持 Azure Trusted Signing 之外的硬件令牌）。
2. 或者用 **Azure Trusted Signing**（微软托管的签名服务，个人开发者可申请，按签名次数计费）——这是当前个人项目性价比最高的一条路。
3. 签名命令（证书在手时）：

   ```powershell
   # signtool 来自 Windows SDK
   signtool sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 `
     /a dist\qlrust-1.0.0-win64\qlrust.exe
   signtool verify /pa /v dist\qlrust-1.0.0-win64\qlrust.exe
   ```

   **必须先签 exe 再打包 zip**，否则 zip 里是未签名产物；签完重算 `SHA256SUMS.txt`（`pack.ps1` 的顺序会把 SHA256SUMS 写在打包前，签名后要重跑一次）。
4. 时间戳服务器必须带（`/tr`），否则证书过期后旧版本全部失效。
5. 签完把证书指纹记进本文件的表格，下一次发布照抄。

## 4. 发布检查清单

- [ ] `cargo fmt --all --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test` 全绿
- [ ] `scripts\smoke.ps1` 无 FAIL（唯一允许的 SKIP 是提权会话下的托盘 UIPI 项）
- [ ] `scripts\bench.ps1` 无 FAIL、无超预算
- [ ] `scripts\sample-matrix.ps1` 通过率 ≥ 98%
- [ ] 三处版本号一致，`FileVersion` 正确
- [ ] `docs/dev/{LOG,STATUS,ARCH,CONVENTIONS}.md` 已记录本次变更
- [ ] dist 里含 `plugins\qlr_plugin_demo.dll` 与 `SHA256SUMS.txt`
- [ ] zip 解压到干净目录 → 双击 `qlrust.exe` → 手动按空格试一次图片/PDF/文本/Markdown

## 5. 回滚

`dist\` 下每个版本都是自包含目录，回滚 = 把旧目录重新装一次（`install.ps1 -Source dist\qlrust-<旧版本>-win64`）。数据目录 `%LOCALAPPDATA%\QLRust\UserData` 不含版本相关结构，跨版本兼容。
