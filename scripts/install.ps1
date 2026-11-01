<#
.SYNOPSIS
    QLRust 每用户安装器（免管理员）。

.DESCRIPTION
    把 QLRust 安装到当前用户的 %LOCALAPPDATA%\Programs\QLRust。**不写 HKLM、不需要管理员**，
    所有注册表改动都在 HKCU 下，卸载时能被完整回滚（见 uninstall.ps1）。

    ## 默认安装 = 一行"关联"都不写
    默认只做四件事：复制文件、建开始菜单快捷方式、写卸载项（控制面板可见）、
    可选的 -Autostart 开机自启。下面这些"会抢双击打开方式"的东西默认**全都不写**：

      - HKCU\Software\Classes\.<ext> 的默认值 —— 默认值就是双击时的打开方式，写了就是抢
      - UserChoice —— Win10/11 用哈希保护用户选择，本来也写不进去
      - HKCU\Software\Classes\Applications\qlrust.exe
      - Capabilities / RegisteredApplications

    产品定位是"选中文件按空格预览"，不是文件处理器：装完以后双击 .jpg/.pdf/.md 仍然
    打开用户原来的程序。想出现在"右键 → 打开方式"列表里，用户得自己加 -Associate。

    ## 为什么必须这么保守（1.0.0 缺陷复盘，实测证据）
    旧安装器无条件给 123 个扩展名写 OpenWithProgids，还写了 Applications\qlrust.exe 与
    Capabilities\FileAssociations；旧的 -Default 会直接写 .<ext> 的默认值。而注册表合并时
    **HKCU 的扩展名默认值优先于 HKLM**（本机实测）：

        HKLM\Software\Classes\.md  (Default) = Nutstore.LightApp.md    ← 系统/别人登记的
        HKCU\Software\Classes\.md  (Default) = Markdown
        HKCR\.md                   (Default) = Markdown                ← 合并视图听 HKCU 的

    所以往 HKCU 的 .<ext> 写默认值 = 直接把用户的双击打开方式改掉。实测复现（一次性
    扩展名 .qlrtest，写完立即清理）：写好默认值后用 ShellExecute 打开测试文件，qlrust
    进程直接起来并渲染预览，日志 `SHOW target=...qlrtest kind=text renderable=true`，
    全程零交互 —— 这就是用户报告的"会抢文件的打开方式，直接变成预览"。
    同一机制的反面证据：本机 HKCU\.jpg 出现过一个空字符串默认值，把 HKLM\.jpg 的
    `jpegfile` 顶成了空 ProgID（`reg query HKCR\.jpg /ve` 只剩空 REG_SZ），而 .jpg 的
    OpenWithProgids 里当时只有 QLRust.File —— 一旦 UserChoice 丢失，QLRust 就是唯一候选。

    因此本脚本的规矩：**默认不写任何关联语义的键；每次安装都会把旧版本留下的抢占痕迹
    清干净（只清自己的名字）**，所以重跑一遍本脚本就能修好被旧版抢过的机器。

.PARAMETER Source
    含 qlrust.exe 的目录。省略时依次尝试：
      - <仓库>\dist\qlrust-<最新版本>-win64
      - <仓库>\target\release

.PARAMETER InstallDir
    安装目录，默认 $env:LOCALAPPDATA\Programs\QLRust。

.PARAMETER Autostart
    写入 HKCU\...\Run，登录后自动启动（托盘常驻，随时可空格预览）。这是唯一会写进
    注册表"自启"位置的开关，保留。

.PARAMETER Associate
    （可选，默认关闭）把自己登记成"打开方式"列表里的**候选**：写 ProgID QLRust.File、
    每个扩展名的 OpenWithProgids、Capabilities + RegisteredApplications。
    **只登记候选，绝不改任何默认值、不写 UserChoice**：用户仍要自己在
    "右键 → 打开方式 → 选择其他应用"里选一次，双击默认程序不变。

.PARAMETER Default
    （可选，默认关闭，且必须配合 -Associate）把内置格式的默认打开程序写成 QLRust。
    这是**抢夺**语义，普通人不需要 —— QLRust 的价值是按空格预览，不抢双击默认程序。
    Win10/11 用 UserChoice 哈希保护用户选择，系统可能在下次设置界面操作后把它改回去，
    这是操作系统的限制，不是安装器失败。本开关会先把每个扩展名**原来的**默认 ProgID
    备份到 HKCU\Software\QLRust\DefaultBackup，uninstall.ps1 据此恢复。

.PARAMETER NoAssoc
    （已废弃）旧版的"跳过关联"开关。现在默认就不注册关联，这个开关只为兼容旧命令行
    而保留，等价于什么都不做。

.PARAMETER Quiet
    不打印进度（脚本仍会返回退出码：0 成功，非 0 失败）。

.EXAMPLE
    pwsh -NoProfile -File scripts\install.ps1

.EXAMPLE
    pwsh -NoProfile -File scripts\install.ps1 -Autostart -Source D:\build\qlrust

.EXAMPLE
    # 只有明确想让 QLRust 出现在"打开方式"列表里时才这么装
    pwsh -NoProfile -File scripts\install.ps1 -Associate
#>
[CmdletBinding()]
param(
    [string]$Source = "",
    [ValidateNotNullOrEmpty()]
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\QLRust'),
    [switch]$Autostart,
    [switch]$Associate,
    [switch]$Default,
    [switch]$NoAssoc,
    [switch]$Quiet
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$ProgId = 'QLRust.File'
$ClassesRoot = 'HKCU:\Software\Classes'
$RustKey = 'HKCU:\Software\QLRust'
$UninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\QLRust'
$RunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$FileExtsRoot = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts'

function Say([string]$Message) {
    if (-not $Quiet) { Write-Host $Message }
}

function Warn([string]$Message) {
    if (-not $Quiet) { Write-Host $Message -ForegroundColor Yellow }
}

# ---------------------------------------------------------------------------
# 注册表小工具
# ---------------------------------------------------------------------------

# 读键的默认值：$null = 默认值不存在；'' = 存在但是空字符串（两者语义不同，
# 空字符串同样会顶掉 HKLM 的登记，见文件头注释里的 .jpg 案例）。
function Get-RegDefaultValue {
    param([string]$KeyPath)
    $item = Get-Item -LiteralPath $KeyPath -ErrorAction SilentlyContinue
    if (-not $item) { return $null }
    if ($item.GetValueNames() -contains '') { return [string]$item.GetValue('') }
    return $null
}

# 删掉键的默认值（回到"没有默认值"的状态）。
#
# 注意：`Remove-ItemProperty -Name '(Default)'` **不管用**——PS 5.1 与 7.6 上都会报
# "路径 ... 中不存在属性 (Default)"，它去找一个名字 literally 叫 "(Default)" 的值
# （和 New-ItemProperty 那个坑对称，见下面 Set-Item 处的注释）。所以只能用 .NET
# 开一个可写句柄删空名值。
function Remove-RegDefaultValue {
    param([string]$KeyPath)
    if ($KeyPath -notlike 'HKCU:\*') { throw "Remove-RegDefaultValue 只接受 HKCU 路径：$KeyPath" }
    $sub = $KeyPath.Substring(6)   # 去掉 'HKCU:\'
    $rk = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($sub, $true)
    if ($null -eq $rk) { return }
    try { $rk.DeleteValue('', $false) } finally { $rk.Close() }
}

# -Default 用的备份：值名 = 带点的扩展名，内容 = 覆盖之前的 ProgID（可能是空串）。
function Test-DefaultBackupRecord {
    param([string]$Ext)
    $bk = Join-Path $RustKey 'DefaultBackup'
    if (-not (Test-Path $bk)) { return $false }
    return ($null -ne (Get-ItemProperty -Path $bk -Name $Ext -ErrorAction SilentlyContinue))
}

function Get-DefaultBackupValue {
    param([string]$Ext)
    $bk = Join-Path $RustKey 'DefaultBackup'
    if (-not (Test-Path $bk)) { return $null }
    if (-not (Test-DefaultBackupRecord -Ext $Ext)) { return $null }
    return [string](Get-ItemPropertyValue -Path $bk -Name $Ext -ErrorAction SilentlyContinue)
}

function Set-DefaultBackupValue {
    param([string]$Ext, [string]$OldProgId)
    $bk = Join-Path $RustKey 'DefaultBackup'
    New-Item -Force -Path $bk | Out-Null
    New-ItemProperty -Force -Path $bk -Name $Ext -Value $OldProgId | Out-Null
}

# UserChoice 快照：用来证明安装器**没有**动用户自己的默认程序选择。
function Get-UserChoiceMap {
    $map = @{}
    if (-not (Test-Path $FileExtsRoot)) { return $map }
    Get-ChildItem $FileExtsRoot -ErrorAction SilentlyContinue | ForEach-Object {
        $uc = Join-Path $_.PSPath 'UserChoice'
        if (Test-Path $uc) {
            $p = (Get-ItemProperty -Path $uc -Name 'ProgId' -ErrorAction SilentlyContinue)
            if ($null -ne $p -and $null -ne $p.ProgId) { $map[$_.PSChildName] = [string]$p.ProgId }
        }
    }
    return $map
}

# 扫出所有"默认值被写成了 QLRust.File"的扩展名（自检 & 修复都用它）。
function Get-HijackedExtensions {
    $list = @()
    if (-not (Test-Path $ClassesRoot)) { return $list }
    Get-ChildItem $ClassesRoot -ErrorAction SilentlyContinue |
        Where-Object { $_.PSChildName -like '.*' } |
        ForEach-Object {
            if ($_.GetValue('') -eq $ProgId) { $list += $_.PSChildName }
        }
    return $list
}

# ---------------------------------------------------------------------------
# 清理旧版本留下的"抢占"痕迹（幂等，只动自己的名字）
# ---------------------------------------------------------------------------
# 返回各项清理数量。每次安装都会跑，所以重跑安装器就能修好被旧版抢过的机器。
function Clear-LegacyAssoc {
    $r = @{ ProgId = 0; Apps = 0; OpenWith = 0; ExtDefault = 0; Capabilities = 0; Mru = 0 }

    # a) ProgID 本体（双击最终落到它的 shell\open\command）
    $progKey = Join-Path $ClassesRoot $ProgId
    if (Test-Path $progKey) {
        Remove-Item $progKey -Recurse -Force
        $r['ProgId'] = 1
    }

    # b) "打开方式"里的可执行体条目（旧版写过；新版默认不注册）
    $appsKey = Join-Path $ClassesRoot 'Applications\qlrust.exe'
    if (Test-Path $appsKey) {
        Remove-Item $appsKey -Recurse -Force
        $r['Apps'] = 1
    }

    # c) 扩展名：默认值 + OpenWithProgids 里只摘自己那一条
    if (Test-Path $ClassesRoot) {
        Get-ChildItem $ClassesRoot -ErrorAction SilentlyContinue |
            Where-Object { $_.PSChildName -like '.*' } |
            ForEach-Object {
                $ext = $_.PSChildName
                $extKey = Join-Path $ClassesRoot $ext

                # c1) 默认值 == QLRust.File → 这是真正的抢占，必须撤回。
                #     有 -Default 时的备份就恢复原 ProgID，否则把值删掉（回到"没登记"）。
                if ($_.GetValue('') -eq $ProgId) {
                    $old = Get-DefaultBackupValue -Ext $ext
                    if ($null -ne $old -and $old -ne '' -and $old -ne $ProgId) {
                        Set-Item -LiteralPath $extKey -Value $old
                    }
                    else {
                        Remove-RegDefaultValue -KeyPath $extKey
                    }
                    $r['ExtDefault']++
                }

                # c2) OpenWithProgids：只删名为 QLRust.File 的值，别人的条目一律不动
                $openWith = Join-Path $extKey 'OpenWithProgids'
                if (Test-Path $openWith) {
                    $item = Get-Item $openWith
                    if ($item.GetValueNames() -contains $ProgId) {
                        Remove-ItemProperty -Path $openWith -Name $ProgId -Force
                        $r['OpenWith']++
                    }
                    # 摘空了的 OpenWithProgids 子键顺手删掉（不留空壳）
                    if ((Get-Item $openWith).GetValueNames().Count -eq 0 -and @(Get-ChildItem $openWith -ErrorAction SilentlyContinue).Count -eq 0) {
                        Remove-Item $openWith -Force
                    }
                }
            }
    }

    # d) FileExts 里的"打开方式"记忆残留（资源管理器会往这里写 qlrust.exe）
    if (Test-Path $FileExtsRoot) {
        Get-ChildItem $FileExtsRoot -ErrorAction SilentlyContinue | ForEach-Object {
            $owp = Join-Path $_.PSPath 'OpenWithProgids'
            if (Test-Path $owp) {
                if ((Get-Item $owp).GetValueNames() -contains $ProgId) {
                    Remove-ItemProperty -Path $owp -Name $ProgId -Force
                    $r['Mru']++
                }
            }
            $owl = Join-Path $_.PSPath 'OpenWithList'
            if (Test-Path $owl) {
                $item = Get-Item $owl
                foreach ($name in @($item.GetValueNames())) {
                    if ($name -eq 'MRUList') { continue }
                    # 值名是 a/b/c…，值内容才是 exe 名；只删内容正好是 qlrust.exe 的
                    if ([string]$item.GetValue($name) -eq 'qlrust.exe') {
                        Remove-ItemProperty -Path $owl -Name $name -Force
                        $r['Mru']++
                    }
                }
            }
        }
    }

    # e) Capabilities / RegisteredApplications（只在 -Associate 时才由本脚本重建）
    $caps = Join-Path $RustKey 'Capabilities'
    if (Test-Path $caps) {
        Remove-Item $caps -Recurse -Force
        $r['Capabilities']++
    }
    $ra = 'HKCU:\Software\RegisteredApplications'
    if (Test-Path $ra) {
        if ($null -ne (Get-ItemProperty -Path $ra -Name 'QLRust' -ErrorAction SilentlyContinue)) {
            Remove-ItemProperty -Path $ra -Name 'QLRust' -Force
        }
    }

    return $r
}

# ---------------------------------------------------------------------------
# 安装源 / 进程 / 扩展名清单
# ---------------------------------------------------------------------------
function Find-Source {
    param([string]$Hint)
    if ($Hint) {
        if (-not (Test-Path (Join-Path $Hint 'qlrust.exe'))) {
            throw "在 $Hint 里找不到 qlrust.exe"
        }
        return (Resolve-Path $Hint).Path
    }
    $repo = Split-Path -Parent $PSScriptRoot
    $dist = Join-Path $repo 'dist'
    if (Test-Path $dist) {
        $candidate = Get-ChildItem $dist -Directory |
            Where-Object { $_.Name -like 'qlrust-*-win64' -and (Test-Path (Join-Path $_.FullName 'qlrust.exe')) } |
            Sort-Object { try { [version]($_.Name -replace '^qlrust-', '' -replace '-win64$', '') } catch { [version]'0.0.0' } } |
            Select-Object -Last 1
        if ($candidate) { return $candidate.FullName }
    }
    $release = Join-Path $repo 'target\release'
    if (Test-Path (Join-Path $release 'qlrust.exe')) { return $release }
    throw '找不到 qlrust.exe：请用 -Source 指定包含 qlrust.exe 的目录（或先 cargo build --release）'
}

function Stop-Running {
    param([string]$Exe)
    # 先礼貌退出（--quit 走单实例 IPC），再兜底杀进程。
    #
    # 这里**绝对不能**写 `& $Exe --quit`：qlrust 是 GUI 子系统程序，没有常驻实例
    # 时它会自己接管单实例并进入消息循环，PowerShell 的调用运算符会一直等它退出
    # —— 实测把脚本挂死（跑满 5 分钟）。WMI 的 Create 立即返回。
    if (Test-Path $Exe) {
        Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{
            CommandLine = '"' + $Exe + '" --quit'
        } -ErrorAction SilentlyContinue | Out-Null
        Start-Sleep -Milliseconds 600
    }
    Get-Process qlrust -ErrorAction SilentlyContinue | ForEach-Object {
        try { $_.Kill(); [void]$_.WaitForExit(3000) } catch { }
    }
}

# 扩展名清单不在脚本里维护：由 `qlrust.exe --list-extensions <文件>` 导出，
# 单一真值在 crates/qlr-core/src/format.rs 的 all_extensions()。
function Get-QlrExtensions {
    param([string]$Exe)
    $extFile = Join-Path $env:TEMP ("qlrust-ext-" + [guid]::NewGuid().ToString('N') + '.txt')
    try {
        # 同样不能 `& $Exe`（见 Stop-Running 的注释），但它会立即返回，所以只等文件。
        Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{
            CommandLine = '"' + $Exe + '" --list-extensions "' + $extFile + '"'
        } -ErrorAction SilentlyContinue | Out-Null
        $deadline = (Get-Date).AddSeconds(10)
        while (-not (Test-Path $extFile) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 100 }
        if (-not (Test-Path $extFile)) { throw "qlrust.exe --list-extensions 没有产出 $extFile" }
        $exts = @(Get-Content $extFile | Where-Object { $_ -match '^[a-z0-9]+$' } | Sort-Object -Unique)
        if ($exts.Count -eq 0) { throw "扩展名清单为空（$extFile）" }
        return $exts
    }
    finally {
        Remove-Item $extFile -Force -ErrorAction SilentlyContinue
    }
}

# ---------------------------------------------------------------------------
# 主流程
# ---------------------------------------------------------------------------
try {
    # 0) 参数校验：宁可提前报错，也不要"跑完看起来成功"
    if ($Default -and -not $Associate) {
        throw '-Default 必须配合 -Associate 使用：没有 ProgID 时写默认值只会把用户的关联写坏。'
    }
    if ($NoAssoc) {
        Warn '  提示    : -NoAssoc 已废弃；现在默认就不注册任何关联，这个开关等于什么都不做。'
    }

    $source = Find-Source -Hint $Source
    $exeSrc = Join-Path $source 'qlrust.exe'
    $version = (Get-Item $exeSrc).VersionInfo.FileVersion
    if (-not $version) { $version = '0.0.0.0' }

    Say 'QLRust 安装器'
    Say "  源目录  : $source"
    Say "  版本    : $version"
    Say "  安装到  : $InstallDir"
    Say "  管理员  : 不需要（全部写 HKCU + 用户目录）"
    Say ("  关联    : {0}" -f $(if ($Associate) { '登记为"打开方式"候选（不改默认程序）' } else { '不注册（默认；双击仍用用户原来的程序）' }))
    if ($Default) {
        Warn '  警告    : -Default 会抢双击默认打开方式（会把用户的选择改掉）。'
        Warn '            只有你确定要这么做才继续；uninstall.ps1 会按备份恢复。'
    }

    # UserChoice 快照（装完对比，证明没动用户的默认程序）
    $ucBefore = Get-UserChoiceMap

    # 1) 停掉正在跑的实例（否则 exe 被占用，复制会失败）
    Stop-Running -Exe (Join-Path $InstallDir 'qlrust.exe')

    # 2) 复制文件
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $copied = @()
    foreach ($name in @('qlrust.exe', 'pdfium.dll', 'README.txt', 'LICENSE.txt')) {
        $from = Join-Path $source $name
        if (Test-Path $from) {
            Copy-Item $from -Destination $InstallDir -Force
            $copied += $name
        }
    }
    # 插件目录（可缺省：没有插件也能正常用四类内置格式）
    $pluginSrc = Join-Path $source 'plugins'
    if (Test-Path $pluginSrc) {
        $pluginDst = Join-Path $InstallDir 'plugins'
        New-Item -ItemType Directory -Force -Path $pluginDst | Out-Null
        Get-ChildItem $pluginSrc -Filter *.dll | ForEach-Object {
            Copy-Item $_.FullName -Destination $pluginDst -Force
            $copied += ('plugins\' + $_.Name)
        }
    }
    # 安装目录里的 exe 必须是"非便携"模式：便携标记会让数据留在安装目录里，
    # 而 %LOCALAPPDATA%\Programs 下的 UserData 是被卸载器清理的对象。
    Remove-Item (Join-Path $InstallDir 'portable.lock') -ErrorAction SilentlyContinue
    $exe = Join-Path $InstallDir 'qlrust.exe'
    if (-not (Test-Path $exe)) { throw "复制失败：$exe 不存在" }
    Say ("  已复制  : " + ($copied -join ', '))

    # 3) 卸载器（复制到安装目录，注册表里的 UninstallString 直接指向它）
    $uninstaller = Join-Path $InstallDir 'uninstall.ps1'
    Copy-Item (Join-Path $PSScriptRoot 'uninstall.ps1') -Destination $uninstaller -Force

    # 4) 先清干净旧版本留下的抢占痕迹（幂等；这一步让"重跑安装器"能修好被抢的机器）
    $cleared = Clear-LegacyAssoc
    Say ("  旧状态  : 清理 ProgID={0} Applications={1} OpenWithProgids={2} 扩展名默认值={3} 打开方式记忆={4}" -f `
            $cleared['ProgId'], $cleared['Apps'], $cleared['OpenWith'], $cleared['ExtDefault'], $cleared['Mru'])

    # 5) 关联登记（只有显式 -Associate 才做；只登记候选，不改默认值）
    $assocCount = 0
    $defaultCount = 0
    if ($Associate) {
        $exts = @(Get-QlrExtensions -Exe $exe)

        # 5a) ProgID：双击/打开方式最终落到这条命令。**注意这里的 ProgID 只是
        #     "被选中的时候用哪条命令"，它自己不会成为任何扩展名的默认值。**
        $progKey = Join-Path $ClassesRoot $ProgId
        New-Item -Force -Path (Join-Path $progKey 'shell\open\command') | Out-Null
        New-Item -Force -Path (Join-Path $progKey 'DefaultIcon') | Out-Null
        # 默认值必须用 Set-Item：New-ItemProperty -Name '(Default)' 会造出一个
        # 名字 literally 叫 "(Default)" 的值，资源管理器根本认不到（踩过一次）。
        Set-Item -LiteralPath $progKey -Value 'QLRust 预览'
        New-ItemProperty -Force -Path $progKey -Name 'FriendlyTypeName' -Value 'QLRust 预览' | Out-Null
        Set-Item -LiteralPath (Join-Path $progKey 'DefaultIcon') -Value "`"$exe`",0"
        Set-Item -LiteralPath (Join-Path $progKey 'shell\open\command') -Value "`"$exe`" `"%1`""

        # 5b) 每个扩展名只加一条 OpenWithProgids（"打开方式"的候选），
        #     **不写 .<ext> 的默认值** —— 那才是抢。
        foreach ($ext in $exts) {
            $openWith = Join-Path $ClassesRoot ".$ext\OpenWithProgids"
            New-Item -Force -Path $openWith | Out-Null
            # 值内容为空（REG_NONE 语义），名字就是 ProgID
            New-ItemProperty -Force -Path $openWith -Name $ProgId -Value ([byte[]]@()) -PropertyType Binary | Out-Null
        }
        $assocCount = $exts.Count

        # 5c) Capabilities：让 QLRust 出现在"设置 → 默认应用"的**可选列表**里。
        #     它只影响"能被选"，不会自动成为任何扩展名的默认程序。
        New-Item -Force -Path (Join-Path $RustKey 'Capabilities\FileAssociations') | Out-Null
        New-ItemProperty -Force -Path (Join-Path $RustKey 'Capabilities') -Name 'ApplicationName' -Value 'QLRust' | Out-Null
        New-ItemProperty -Force -Path (Join-Path $RustKey 'Capabilities') -Name 'ApplicationDescription' -Value 'Rust 重写的空格预览工具（图片 / PDF / 文本 / Markdown）' | Out-Null
        New-ItemProperty -Force -Path (Join-Path $RustKey 'Capabilities') -Name 'ApplicationIcon' -Value "`"$exe`",0" | Out-Null
        foreach ($ext in $exts) {
            New-ItemProperty -Force -Path (Join-Path $RustKey 'Capabilities\FileAssociations') -Name ".$ext" -Value $ProgId | Out-Null
        }
        New-Item -Force -Path 'HKCU:\Software\RegisteredApplications' | Out-Null
        New-ItemProperty -Force -Path 'HKCU:\Software\RegisteredApplications' -Name 'QLRust' -Value 'Software\QLRust\Capabilities' | Out-Null

        # 5d) -Default：唯一会真正"抢"的一步。先备份原来的 ProgID，再覆盖默认值。
        if ($Default) {
            foreach ($ext in $exts) {
                $extKey = Join-Path $ClassesRoot ".$ext"
                New-Item -Force -Path $extKey | Out-Null
                $old = Get-RegDefaultValue -KeyPath $extKey
                # 只备份第一次：重复安装不能把最原始的取值覆盖成 QLRust.File 或空
                if ($null -ne $old -and $old -ne '' -and $old -ne $ProgId -and -not (Test-DefaultBackupRecord -Ext ".$ext")) {
                    Set-DefaultBackupValue -Ext ".$ext" -OldProgId $old
                }
                Set-Item -LiteralPath $extKey -Value $ProgId
                $defaultCount++
            }
        }
        Say ("  关联扩展: {0} 个（只登记为打开方式候选，不改默认程序）" -f $assocCount)
        if ($Default) {
            Say ("  默认程序: {0} 个扩展名被改成 QLRust（-Default；旧值已备份，卸载可恢复）" -f $defaultCount)
        }
    }
    else {
        Say '  关联扩展: 未注册（双击仍用用户原来的程序；要出现在"打开方式"列表请加 -Associate）'
    }

    # 6) 开始菜单快捷方式
    $startMenu = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\QLRust.lnk'
    $shell = New-Object -ComObject WScript.Shell
    $link = $shell.CreateShortcut($startMenu)
    $link.TargetPath = $exe
    $link.WorkingDirectory = $InstallDir
    $link.IconLocation = "$exe,0"
    $link.Description = 'QLRust 空格预览'
    $link.Save()
    Say "  开始菜单: $startMenu"

    # 7) 开机启动（可选）
    if ($Autostart) {
        New-Item -Force -Path $RunKey | Out-Null
        New-ItemProperty -Force -Path $RunKey -Name 'QLRust' -Value "`"$exe`" --hide" | Out-Null
        Say '  开机启动: 已启用（登录后静默进托盘）'
    }
    else {
        $existing = Get-ItemProperty -Path $RunKey -Name 'QLRust' -ErrorAction SilentlyContinue
        if ($existing) {
            Remove-ItemProperty -Path $RunKey -Name 'QLRust' -Force
            Say '  开机启动: 已关闭'
        }
    }

    # 8) 控制面板里能看到、能卸载
    New-Item -Force -Path $UninstallKey | Out-Null
    $size = [int]((Get-ChildItem $InstallDir -Recurse -File | Measure-Object Length -Sum).Sum / 1KB)
    foreach ($pair in @(
            @('DisplayName', 'QLRust'),
            @('DisplayVersion', $version),
            @('Publisher', 'QLRust'),
            @('DisplayIcon', "$exe,0"),
            @('InstallLocation', $InstallDir),
            @('UninstallString', "powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$uninstaller`""),
            @('QuietUninstallString', "powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$uninstaller`" -Quiet")
        )) {
        New-ItemProperty -Force -Path $UninstallKey -Name $pair[0] -Value $pair[1] | Out-Null
    }
    foreach ($name in @('NoModify', 'NoRepair')) {
        New-ItemProperty -Force -Path $UninstallKey -Name $name -Value 1 -PropertyType DWord | Out-Null
    }
    New-ItemProperty -Force -Path $UninstallKey -Name 'EstimatedSize' -Value $size -PropertyType DWord | Out-Null
    Say "  卸载入口: 设置 → 应用 → QLRust（或 $uninstaller）"

    # 9) 自检：用注册表事实证明"没有抢默认打开方式"
    $hijacked = @(Get-HijackedExtensions)
    if (-not $Default) {
        if ($hijacked.Count -gt 0) {
            throw ("自检失败：这些扩展名的默认打开方式仍指向 $ProgId —— " + ($hijacked -join ' '))
        }
        Say '  自检    : 没有任何扩展名的默认打开方式指向 QLRust ✓'
    }
    else {
        Say ("  自检    : -Default 生效，{0} 个扩展名的默认打开方式指向 QLRust（卸载时按备份恢复）" -f $hijacked.Count)
    }
    $ucAfter = Get-UserChoiceMap
    $ucDiff = @()
    foreach ($ext in @($ucBefore.Keys)) {
        if ($ucBefore[$ext] -ne $ucAfter[$ext]) { $ucDiff += ("{0}: {1} -> {2}" -f $ext, $ucBefore[$ext], $ucAfter[$ext]) }
    }
    if ($ucDiff.Count -eq 0) {
        Say ("  自检    : UserChoice 未被改动（{0} 个扩展名前后一致）✓" -f $ucBefore.Count)
    }
    else {
        Warn ("  自检    : UserChoice 发生了变化（安装器不写 UserChoice，可能是系统/其它程序动的）：" + ($ucDiff -join '; '))
    }

    Say '完成。'
    exit 0
}
catch {
    Write-Host ''
    Write-Host ('[安装失败] ' + $_.Exception.Message) -ForegroundColor Red
    if ($_.InvocationInfo -and $_.InvocationInfo.PositionMessage) {
        Write-Host ('[安装失败] 位置：' + ($_.InvocationInfo.PositionMessage -replace "`r?`n", ' ')) -ForegroundColor Red
    }
    exit 1
}
