<#
.SYNOPSIS
    QLRust 卸载器（无需管理员）。

.DESCRIPTION
    彻底移除 QLRust：文件、开始菜单快捷方式、卸载项、开机自启项，以及**旧版本留下的
    文件关联痕迹**。可以在任何时候重复运行（幂等），不存在的键直接跳过。

    ## 为什么要专门"清关联痕迹"
    1.0.0 的安装器（旧版）默认就注册了：ProgID `QLRust.File`、`Applications\qlrust.exe`、
    123 个扩展名的 `OpenWithProgids`、`Capabilities` + `RegisteredApplications`；带
    `-Default` 时还会把 `HKCU\Software\Classes\.<ext>` 的默认值改成 `QLRust.File` ——
    而 HKCU 的扩展名默认值**优先于 HKLM**（本机实测：HKLM\.md=Nutstore.LightApp.md、
    HKCU\.md=Markdown，`reg query HKCR\.md /ve` 给出 Markdown），所以那一写就等于把
    用户的双击打开方式改成了"预览"。这就是用户报告的
    "会抢文件的打开方式，直接变成预览"。

    因此本脚本必须把默认值也撤回：
      - 有 `-Default` 时代的备份（`HKCU\Software\QLRust\DefaultBackup`）→ 恢复原 ProgID；
      - 没有备份 → 把默认值删掉，让 Windows 重新回落到 HKLM 的登记。

    ## 做不到的事（要说清楚，不能假装成功）
    Windows 10/11 的 `UserChoice`（真正决定双击打开方式的那一项）带哈希保护，
    安装器从来没写过它，卸载器也不可能替用户"重建"原来的关联。所以如果用户曾经在
    "默认应用"里手动选过 QLRust，卸载后那项会变成悬空的 ProgID，需要用户自己去
    "设置 → 应用 → 默认应用"里重新指回原来的程序。脚本会在结尾明确提示。

.PARAMETER InstallDir
    安装目录，默认 $env:LOCALAPPDATA\Programs\QLRust。

.PARAMETER KeepData
    保留用户数据（%LOCALAPPDATA%\QLRust，含日志/缓存/配置），不删。

.PARAMETER Force
    不询问直接执行（配合 -Quiet 时自动生效）。

.PARAMETER Quiet
    不打印进度（脚本仍返回退出码：0 成功，非 0 失败）。

.EXAMPLE
    pwsh -NoProfile -File scripts\uninstall.ps1 -Quiet

.EXAMPLE
    # 只卸载程序，保留日志和配置
    pwsh -NoProfile -File scripts\uninstall.ps1 -KeepData
#>
[CmdletBinding()]
param(
    [ValidateNotNullOrEmpty()]
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\QLRust'),
    [switch]$KeepData,
    [switch]$Force,
    [switch]$Quiet
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$ProgId = 'QLRust.File'
$ClassesRoot = 'HKCU:\Software\Classes'
$RustKey = 'HKCU:\Software\QLRust'
$UninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\QLRust'
$RunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$RegAppsKey = 'HKCU:\Software\RegisteredApplications'
$FileExtsRoot = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts'
$StartMenu = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\QLRust.lnk'
$DataDir = Join-Path $env:LOCALAPPDATA 'QLRust'

function Say([string]$Message) {
    if (-not $Quiet) { Write-Host $Message }
}

function Warn([string]$Message) {
    if (-not $Quiet) { Write-Host $Message -ForegroundColor Yellow }
}

# ---------------------------------------------------------------------------
# 注册表小工具（与 install.ps1 同一套，删默认值只能走 .NET）
# ---------------------------------------------------------------------------
function Get-RegDefaultValue {
    param([string]$KeyPath)
    $item = Get-Item -LiteralPath $KeyPath -ErrorAction SilentlyContinue
    if (-not $item) { return $null }
    if ($item.GetValueNames() -contains '') { return [string]$item.GetValue('') }
    return $null
}

# `Remove-ItemProperty -Name '(Default)'` 不管用（PS 5.1/7.6 实测都报
# "路径 ... 中不存在属性 (Default)"，它找的是名字 literally 叫 "(Default)" 的值）。
# 必须开一个可写句柄删空名值。
function Remove-RegDefaultValue {
    param([string]$KeyPath)
    if ($KeyPath -notlike 'HKCU:\*') { throw "Remove-RegDefaultValue 只接受 HKCU 路径：$KeyPath" }
    $sub = $KeyPath.Substring(6)   # 去掉 'HKCU:\'
    $rk = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($sub, $true)
    if ($null -eq $rk) { return $false }
    try {
        if ($rk.GetValueNames() -notcontains '') { return $false }
        $rk.DeleteValue('', $false)
        return $true
    }
    finally { $rk.Close() }
}

function Remove-RegValueIfExists {
    param([string]$KeyPath, [string]$Name)
    if (-not (Test-Path $KeyPath)) { return $false }
    if ($null -eq (Get-ItemProperty -Path $KeyPath -Name $Name -ErrorAction SilentlyContinue)) { return $false }
    Remove-ItemProperty -Path $KeyPath -Name $Name -Force
    return $true
}

# -Default 时代写的备份：值名 = 带点的扩展名，内容 = 被覆盖前的 ProgID。
function Get-DefaultBackupValue {
    param([string]$Ext)
    $bk = Join-Path $RustKey 'DefaultBackup'
    if (-not (Test-Path $bk)) { return $null }
    if ($null -eq (Get-ItemProperty -Path $bk -Name $Ext -ErrorAction SilentlyContinue)) { return $null }
    return [string](Get-ItemPropertyValue -Path $bk -Name $Ext -ErrorAction SilentlyContinue)
}

# ---------------------------------------------------------------------------
# 1) 停进程（必须最先做：exe 被占用就删不掉）
# ---------------------------------------------------------------------------
# 绝不能用 `& $Exe --quit`：GUI 子系统程序在无常驻实例时会自己接管单实例并进消息
# 循环，PowerShell 的调用运算符会等到它退出 —— 实测把脚本挂死。WMI 的 Create 立即返回。
function Stop-Qlr {
    param([string]$Exe)
    if (Test-Path $Exe) {
        Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{
            CommandLine = '"' + $Exe + '" --quit'
        } -ErrorAction SilentlyContinue | Out-Null
        Start-Sleep -Milliseconds 600
    }
    $procs = @(Get-Process qlrust -ErrorAction SilentlyContinue)
    foreach ($p in $procs) {
        try { $p.Kill(); [void]$p.WaitForExit(3000) } catch { }
    }
    return $procs.Count
}

# ---------------------------------------------------------------------------
# 主流程
# ---------------------------------------------------------------------------
try {
    $exe = Join-Path $InstallDir 'qlrust.exe'

    Say 'QLRust 卸载器'
    Say "  安装目录: $InstallDir"
    Say "  用户数据: $(if ($KeepData) { '保留' } else { $DataDir + '（删除）' })"

    if (-not $Force -and -not $Quiet) {
        $answer = Read-Host '继续卸载？(y/N)'
        if ($answer -notmatch '^(y|yes|是)$') {
            Write-Host '已取消。'
            exit 0
        }
    }

    # 1) 停进程
    $killed = Stop-Qlr -Exe $exe
    if ($killed -gt 0) { Say "  已停止  : $killed 个进程" }

    # 2) 关联痕迹：ProgID / Applications / 扩展名默认值 / OpenWithProgids
    $restoredExt = @()
    $clearedExt = @()
    $openWithCount = 0
    $mruCount = 0

    # 2a) ProgID 本体（双击最终落到它的 shell\open\command）
    $progKey = Join-Path $ClassesRoot $ProgId
    $hadProgId = Test-Path $progKey
    if ($hadProgId) { Remove-Item $progKey -Recurse -Force }

    # 2b) "打开方式"里的可执行体条目
    $appsKey = Join-Path $ClassesRoot 'Applications\qlrust.exe'
    $hadApps = Test-Path $appsKey
    if ($hadApps) { Remove-Item $appsKey -Recurse -Force }

    # 2c) 每个扩展名：默认值 + OpenWithProgids 里名叫 QLRust.File 的那一条
    if (Test-Path $ClassesRoot) {
        Get-ChildItem $ClassesRoot -ErrorAction SilentlyContinue |
            Where-Object { $_.PSChildName -like '.*' } |
            ForEach-Object {
                $ext = $_.PSChildName
                $extKey = Join-Path $ClassesRoot $ext

                # 默认值 == QLRust.File → 旧版 -Default 抢过，必须撤回
                if ($_.GetValue('') -eq $ProgId) {
                    $old = Get-DefaultBackupValue -Ext $ext
                    if ($null -ne $old -and $old -ne '' -and $old -ne $ProgId) {
                        Set-Item -LiteralPath $extKey -Value $old
                        $restoredExt += "$ext→$old"
                    }
                    else {
                        # 没有备份：删掉默认值，让 Windows 回落 到 HKLM 的登记（可能是
                        # 系统默认，也可能落空 —— 落空时用户要自己去"默认应用"重设）。
                        [void](Remove-RegDefaultValue -KeyPath $extKey)
                        $clearedExt += $ext
                    }
                }

                # OpenWithProgids：只删值为/名为 QLRust.File 的条目，别人的一律不动
                $openWith = Join-Path $extKey 'OpenWithProgids'
                if (Test-Path $openWith) {
                    $item = Get-Item $openWith
                    $names = @($item.GetValueNames())
                    foreach ($name in $names) {
                        # 正常是"值名 = QLRust.File、内容为空"；也兼容"值名随便、内容 = QLRust.File"
                        if ($name -eq $ProgId -or ($name -ne '' -and [string]$item.GetValue($name) -eq $ProgId)) {
                            Remove-ItemProperty -Path $openWith -Name $name -Force
                            $openWithCount++
                        }
                    }
                    if ((Get-Item $openWith).GetValueNames().Count -eq 0 -and
                        @(Get-ChildItem $openWith -ErrorAction SilentlyContinue).Count -eq 0) {
                        Remove-Item $openWith -Force
                    }
                }
            }
    }

    # 2d) FileExts 里的"打开方式"记忆（资源管理器写的 MRU，不清会留下 qlrust.exe 残影）
    if (Test-Path $FileExtsRoot) {
        Get-ChildItem $FileExtsRoot -ErrorAction SilentlyContinue | ForEach-Object {
            $owp = Join-Path $_.PSPath 'OpenWithProgids'
            if (Test-Path $owp) {
                if ((Get-Item $owp).GetValueNames() -contains $ProgId) {
                    Remove-ItemProperty -Path $owp -Name $ProgId -Force
                    $mruCount++
                }
            }
            $owl = Join-Path $_.PSPath 'OpenWithList'
            if (Test-Path $owl) {
                $item = Get-Item $owl
                foreach ($name in @($item.GetValueNames())) {
                    if ($name -eq 'MRUList') { continue }
                    if ([string]$item.GetValue($name) -eq 'qlrust.exe') {
                        Remove-ItemProperty -Path $owl -Name $name -Force
                        $mruCount++
                    }
                }
            }
        }
    }

    # 3) Capabilities / RegisteredApplications（"默认应用"里的候选列表）
    $caps = Join-Path $RustKey 'Capabilities'
    $hadCaps = Test-Path $caps
    if ($hadCaps) { Remove-Item $caps -Recurse -Force }
    $hadRegApp = Remove-RegValueIfExists -KeyPath $RegAppsKey -Name 'QLRust'

    # 4) 卸载项
    $hadUninstall = Test-Path $UninstallKey
    if ($hadUninstall) { Remove-Item $UninstallKey -Recurse -Force }

    # 5) 开机自启
    $hadRun = Remove-RegValueIfExists -KeyPath $RunKey -Name 'QLRust'

    # 6) 开始菜单快捷方式
    $hadLnk = Test-Path $StartMenu
    if ($hadLnk) { Remove-Item $StartMenu -Force }

    # 7) 程序文件（-KeepData 不影响这里：程序文件永远删）
    $hadInstallDir = Test-Path $InstallDir
    if ($hadInstallDir) {
        Remove-Item $InstallDir -Recurse -Force
        if (Test-Path $InstallDir) { throw "删除安装目录失败（可能有文件被占用）：$InstallDir" }
    }

    # 8) 用户数据 + 自己的软件键（-KeepData 时都留着）
    $hadData = $false
    if (-not $KeepData) {
        $hadData = Test-Path $DataDir
        if ($hadData) { Remove-Item $DataDir -Recurse -Force }
        $hadRust = Test-Path $RustKey
        if ($hadRust) { Remove-Item $RustKey -Recurse -Force }
    }

    # -----------------------------------------------------------------------
    # 汇总
    # -----------------------------------------------------------------------
    Say ''
    Say '已清理（有则清，无则跳过）：'
    Say ("  程序文件    : {0}" -f $(if ($hadInstallDir) { '已删除 ' + $InstallDir } else { '本来就不存在' }))
    Say ("  开始菜单    : {0}" -f $(if ($hadLnk) { '已删除' } else { '本来就不存在' }))
    Say ("  开机自启    : {0}" -f $(if ($hadRun) { '已删除' } else { '本来就没有' }))
    Say ("  卸载项      : {0}" -f $(if ($hadUninstall) { '已删除' } else { '本来就不存在' }))
    Say ("  ProgID      : {0}" -f $(if ($hadProgId) { 'QLRust.File 已删除' } else { '本来就不存在' }))
    Say ("  Applications: {0}" -f $(if ($hadApps) { 'Applications\qlrust.exe 已删除' } else { '本来就不存在' }))
    Say ("  打开方式候选: 摘除 OpenWithProgids={0} 条，打开方式记忆={1} 条" -f $openWithCount, $mruCount)
    Say ("  Capabilities: {0} / RegisteredApplications: {1}" -f `
            $(if ($hadCaps) { '已删除' } else { '本无' }), $(if ($hadRegApp) { '已删除' } else { '本无' }))
    Say ("  用户数据    : {0}" -f $(if ($KeepData) { '保留（-KeepData）' } else { if ($hadData) { '已删除 ' + $DataDir } else { '本来就不存在' } }))

    # -----------------------------------------------------------------------
    # 关联恢复的真实情况：说清楚，不能假装成功
    # -----------------------------------------------------------------------
    if ($restoredExt.Count -gt 0) {
        Say ''
        Say ('已把扩展名默认值恢复成覆盖前登记的 ProgID：' + ($restoredExt -join ', '))
    }

    $hijackWarn = $false
    if ($clearedExt.Count -gt 0) {
        $hijackWarn = $true
        Write-Host ''
        Write-Host '注意：以下扩展名的默认打开方式曾被 QLRust 占用，已清空默认值：' -ForegroundColor Yellow
        Write-Host ('  ' + ($clearedExt -join ' ')) -ForegroundColor Yellow
    }

    # 退役后仍然可能"看起来还在抢"的唯一来源：UserChoice（哈希保护，卸载器动不了）
    $ucStale = @()
    if (Test-Path $FileExtsRoot) {
        Get-ChildItem $FileExtsRoot -ErrorAction SilentlyContinue | ForEach-Object {
            $uc = Join-Path $_.PSPath 'UserChoice'
            if (Test-Path $uc) {
                $p = Get-ItemProperty -Path $uc -Name 'ProgId' -ErrorAction SilentlyContinue
                if ($null -ne $p -and [string]$p.ProgId -eq $ProgId) { $ucStale += $_.PSChildName }
            }
        }
    }
    if ($ucStale.Count -gt 0) { $hijackWarn = $true }

    if ($hijackWarn) {
        Write-Host ''
        Write-Host '需要你手动做一步（卸载器做不到）：' -ForegroundColor Yellow
        Write-Host '  Windows 用 UserChoice（带哈希）记录"双击打开方式"，任何程序都不能静默改写它。' -ForegroundColor Yellow
        Write-Host "  请打开 设置 → 应用 → 默认应用，把受影响的扩展名（$([string]::Join(' ', ($clearedExt + $ucStale | Select-Object -Unique)))）" -ForegroundColor Yellow
        Write-Host '  重新指回你原来的程序（例如"照片"、"Chrome"、"Visual Studio Code"）。' -ForegroundColor Yellow
        Write-Host '  指向原来程序后，双击行为就恢复正常了。' -ForegroundColor Yellow
    }

    # 清理后的自检：注册表里不应再有任何指向 QLRust 的默认值
    $leftover = @()
    if (Test-Path $ClassesRoot) {
        Get-ChildItem $ClassesRoot -ErrorAction SilentlyContinue |
            Where-Object { $_.PSChildName -like '.*' } |
            ForEach-Object { if ($_.GetValue('') -eq $ProgId) { $leftover += $_.PSChildName } }
    }
    if ($leftover.Count -gt 0) {
        throw ('自检失败：这些扩展名的默认打开方式仍指向 ' + $ProgId + ' —— ' + ($leftover -join ' '))
    }

    Say ''
    Say '完成：QLRust 已卸载，没有任何扩展名的默认打开方式指向 QLRust ✓'
    exit 0
}
catch {
    Write-Host ''
    Write-Host ('[卸载失败] ' + $_.Exception.Message) -ForegroundColor Red
    if ($_.InvocationInfo -and $_.InvocationInfo.PositionMessage) {
        Write-Host ('[卸载失败] 位置：' + ($_.InvocationInfo.PositionMessage -replace "`r?`n", ' ')) -ForegroundColor Red
    }
    exit 1
}
