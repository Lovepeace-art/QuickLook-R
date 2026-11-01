<#
.SYNOPSIS
    打发布包：dist\qlrust-<版本>-win64\ + 同名 zip + SHA256SUMS.txt。

.DESCRIPTION
    产物（与 README.txt 里写的一致）：
      qlrust.exe                  主程序（GUI 子系统）
      pdfium.dll                  PDF 渲染（运行时加载，缺了只是不能看 PDF）
      plugins\qlr_plugin_demo.dll 示例插件（视频占位图）
      plugins\qlr_plugin_office.dll Office/压缩包轻量预览（插件 ABI 参考实现）
      portable.lock               存在即"便携模式"：配置和日志留在本目录 UserData\
      README.txt                  上手说明

    `-Portable`（默认）会带 portable.lock；`-NoPortable` 打出来的包用于安装到
    %LOCALAPPDATA%，数据走用户目录。

.PARAMETER Version
    版本号，默认从 Cargo.toml 的 [workspace.package] version 读。

.PARAMETER Pdfium
    pdfium.dll 路径，默认按顺序找：
      _bench\pdfium\bin\pdfium.dll（仓库旁）
      ..\_bench\pdfium\bin\pdfium.dll

.EXAMPLE
    pwsh -NoProfile -File scripts\pack.ps1
#>
[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$Pdfium = "",
    [switch]$NoPortable,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repo = Split-Path -Parent $PSScriptRoot
Push-Location $repo
try {
    if (-not $Version) {
        $line = Select-String -Path 'Cargo.toml' -Pattern '^version = "(.+)"' | Select-Object -First 1
        if (-not $line) { throw 'Cargo.toml 里找不到 version' }
        $Version = $line.Matches[0].Groups[1].Value
    }
    Write-Host "打包 QLRust $Version"

    if (-not $SkipBuild) {
        Write-Host '  cargo build --release ...'
        & cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build 失败（$LASTEXITCODE）" }
    }

    $exe = Join-Path $repo 'target\release\qlrust.exe'
    if (-not (Test-Path $exe)) { throw "找不到 $exe" }

    if (-not $Pdfium) {
        foreach ($candidate in @(
                (Join-Path $repo '..\_bench\pdfium\bin\pdfium.dll'),
                (Join-Path $repo '_bench\pdfium\bin\pdfium.dll')
            )) {
            if (Test-Path $candidate) { $Pdfium = (Resolve-Path $candidate).Path; break }
        }
    }
    if (-not $Pdfium -or -not (Test-Path $Pdfium)) {
        throw '找不到 pdfium.dll（用 -Pdfium 指定；没有它 PDF 预览会不可用）'
    }

    $name = "qlrust-$Version-win64"
    $out = Join-Path $repo "dist\$name"
    if (Test-Path $out) { Remove-Item $out -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $out | Out-Null

    Copy-Item $exe $out
    Copy-Item $Pdfium (Join-Path $out 'pdfium.dll')
    # 插件：示例插件（视频占位图）+ Office/压缩包预览（插件 ABI 的第一个真实用例）
    foreach ($dll in @('qlr_plugin_demo.dll', 'qlr_plugin_office.dll')) {
        $plugin = Join-Path $repo ('target\release\' + $dll)
        if (-not (Test-Path $plugin)) { continue }
        New-Item -ItemType Directory -Force -Path (Join-Path $out 'plugins') | Out-Null
        Copy-Item $plugin (Join-Path $out 'plugins')
    }
    $readme = Join-Path $repo 'packaging\README.txt'
    if (Test-Path $readme) { Copy-Item $readme $out }
    if (-not $NoPortable) {
        New-Item -ItemType File -Force -Path (Join-Path $out 'portable.lock') | Out-Null
    }

    # zip + 校验和
    $zip = Join-Path $repo "dist\$name.zip"
    if (Test-Path $zip) { Remove-Item $zip -Force }
    Compress-Archive -Path (Join-Path $out '*') -DestinationPath $zip -CompressionLevel Optimal

    $sums = Get-ChildItem $out -Recurse -File | Sort-Object FullName | ForEach-Object {
        $rel = $_.FullName.Substring($out.Length + 1) -replace '\\', '/'
        $hash = (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower()
        "$hash  $rel"
    }
    $sums | Set-Content (Join-Path $out 'SHA256SUMS.txt') -Encoding utf8NoBOM
    $sums | ForEach-Object { Write-Host "  $_" }

    $exeSize = (Get-Item (Join-Path $out 'qlrust.exe')).Length
    Write-Host ("  产物：{0}" -f $out)
    Write-Host ("   zip：{0}（{1:N0} 字节）" -f $zip, (Get-Item $zip).Length)
    Write-Host ("   exe：{0:N0} 字节（{1:N2} MB）" -f $exeSize, ($exeSize / 1MB))
}
finally {
    Pop-Location
}
