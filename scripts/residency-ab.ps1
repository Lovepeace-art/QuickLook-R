# 1.8.2 vs 1.8.3「空闲常驻」A/B + 1.8.3 归因（RESIDENCY-1.8.3 的测量脚本）
#
# 为什么要这个脚本：上一轮报告（PERF-1.8.3-vs-1.8.2.md）里的 1.8.3 常驻数字是在
# DSH 工作目录下跑出来的（令牌完整性=low → 托盘注册失败 → 代码会另起线程弹
# MessageBox 兜底），而 1.8.2 的对照组只有文档留存、且环境是普通目录。
# 所以这里把两边都**复制到同构的中性目录**（同样的 plugins\、pdfium.dll、portable.lock），
# 用同一段 pwsh（System.Diagnostics.Process，直接 CreateProcess）裸启、不预览、静置 10 s。
#
# 模式：
#   -Mode ab      1.8.2(中性) / 1.8.3(中性) / 1.8.3(DSH 原地) 各 3 轮
#   -Mode attrib  1.8.3 中性目录上逐项关候选（DPI_FIX / content_scale / prewarm / 预览一次）
#
# 用法：
#   pwsh -NoProfile -File scripts/residency-ab.ps1 -Mode ab
#   pwsh -NoProfile -File scripts/residency-ab.ps1 -Mode attrib
# 结果：scripts/residency-<mode>.json

param(
    [string]$Mode = "ab",
    [int]$Runs = 3,
    [string]$Ver182 = "C:\Users\Administrator\Desktop\qlr-182\target\release",
    [string]$Ver183 = "target\release",
    [string]$NeutralRoot = "C:\Users\Administrator\Desktop\qlr-ab",
    [string]$Fixtures = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Resolve-Dir([string]$p) { return (Resolve-Path $p).Path }

if (-not $Fixtures) {
    if ($env:QLRUST_FIXTURES) { $Fixtures = $env:QLRUST_FIXTURES }
    else { $Fixtures = Join-Path ([Environment]::GetFolderPath('Desktop')) 'qlrust-dev' }
}
$Fixtures = (Resolve-Path $Fixtures).Path

Add-Type -Namespace QlrR -Name W -MemberDefinition @'
[DllImport("user32.dll")] public static extern uint GetGuiResources(IntPtr hProcess, uint flags);
[DllImport("kernel32.dll")] public static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
[DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
'@

# ---------------------------------------------------------------------------
# 进程与采样
# ---------------------------------------------------------------------------

function Kill-All {
    Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    for ($i = 0; $i -lt 200; $i++) {
        if (-not (Get-Process qlrust -ErrorAction SilentlyContinue)) { break }
        Start-Sleep -Milliseconds 20
    }
    Start-Sleep -Milliseconds 900
}

# 直接用 pwsh 里的 CreateProcess 起（不经 WMI，也不经 Start-Process 的 stdio 继承）
function Start-Exe {
    param([string]$Exe, [string]$WorkDir, [string[]]$ArgList = @(), [hashtable]$Env = $null)
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Exe
    $psi.WorkingDirectory = $WorkDir
    $psi.UseShellExecute = $false
    foreach ($a in $ArgList) { [void]$psi.ArgumentList.Add($a) }
    if ($Env) { foreach ($kv in $Env.GetEnumerator()) { $psi.EnvironmentVariables[$kv.Key] = [string]$kv.Value } }
    $p = [System.Diagnostics.Process]::Start($psi)
    return $p
}

function Get-Snap {
    param([int]$ProcessId)
    $p = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
    if (-not $p) { return $null }
    $gdi = 0; $user = 0
    $h = [QlrR.W]::OpenProcess(0x0400 -bor 0x1000, $false, [uint32]$ProcessId)
    if ($h -ne [IntPtr]::Zero) {
        $gdi = [QlrR.W]::GetGuiResources($h, 0)
        $user = [QlrR.W]::GetGuiResources($h, 1)
        [void][QlrR.W]::CloseHandle($h)
    }
    return [pscustomobject]@{
        PrivateMB = [math]::Round($p.PrivateMemorySize64 / 1MB, 2)
        WSMB      = [math]::Round($p.WorkingSet64 / 1MB, 2)
        句柄      = $p.HandleCount
        线程      = $p.Threads.Count
        GDI       = $gdi
        USER      = $user
    }
}

function Read-Log([string]$dir) { Join-Path $dir "UserData\logs\app.log" }

function Get-LogText([string]$dir) {
    $f = Read-Log $dir
    if (-not (Test-Path $f)) { return "" }
    ((Get-Content $f -ErrorAction SilentlyContinue) -join "`n")
}

# 走完一次"裸启 → 静置 10 s → 采样 → --quit"
function Measure-Idle {
    param([string]$ExeDir, [string]$Tag, [hashtable]$Env = $null, [int]$IdleMs = 10000)
    Kill-All
    $exe = Join-Path $ExeDir "qlrust.exe"
    if (-not (Test-Path (Join-Path $ExeDir "portable.lock"))) { New-Item -ItemType File -Path (Join-Path $ExeDir "portable.lock") | Out-Null }
    $log = Read-Log $ExeDir
    if (Test-Path $log) { Remove-Item $log -Force -ErrorAction SilentlyContinue }

    $t0 = Get-Date
    $proc = Start-Exe -Exe $exe -WorkDir $ExeDir -Env $Env
    $readyMs = -1
    $deadline = (Get-Date).AddSeconds(15)
    while ((Get-Date) -lt $deadline) {
        if ((Get-LogText $ExeDir) -match "热键就绪") { $readyMs = [int]((Get-Date) - $t0).TotalMilliseconds; break }
        Start-Sleep -Milliseconds 10
    }
    # 从**进程创建**起算 10 s 静置
    $left = $IdleMs - [int]((Get-Date) - $t0).TotalMilliseconds
    if ($left -gt 0) { Start-Sleep -Milliseconds $left }
    $snap = Get-Snap -ProcessId $proc.Id
    $text = Get-LogText $ExeDir
    $tray = if ($text -match "托盘图标已添加") { "成功" } elseif ($text -match "托盘图标创建失败") { "失败" } else { "未尝试" }
    $integrity = ""
    if ($text -match "令牌完整性=(\w+)") { $integrity = $Matches[1] }
    $plugins = ""
    if ($text -match "插件就绪 dir=.*? loaded=(\d+) extensions=(\d+)") { $plugins = "loaded=$($Matches[1]) ext=$($Matches[2])" }
    $ver = (Get-Item $exe).VersionInfo.FileVersion

    # 退出（同一套启动方式发 --quit）
    $q = Start-Exe -Exe $exe -WorkDir $ExeDir -ArgList @("--quit")
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt 10000) {
        if (-not (Get-Process -Id $proc.Id -ErrorAction SilentlyContinue)) { break }
        Start-Sleep -Milliseconds 10
    }
    Kill-All

    return [pscustomobject]@{
        组           = $Tag
        版本         = $ver
        目录         = $ExeDir
        就绪ms       = $readyMs
        PrivateMB    = $(if ($snap) { $snap.PrivateMB } else { -1 })
        WSMB         = $(if ($snap) { $snap.WSMB } else { -1 })
        句柄         = $(if ($snap) { $snap.句柄 } else { -1 })
        线程         = $(if ($snap) { $snap.线程 } else { -1 })
        GDI          = $(if ($snap) { $snap.GDI } else { -1 })
        USER         = $(if ($snap) { $snap.USER } else { -1 })
        托盘         = $tray
        完整性       = $integrity
        插件         = $plugins
    }
}

function Get-Median {
    param([double[]]$Values)
    $v = @($Values | Where-Object { $_ -ne $null -and $_ -ge 0 })
    if ($v.Count -eq 0) { return -1 }
    $s = @($v | Sort-Object)
    $n = $s.Count
    if ($n % 2 -eq 1) { return $s[[int](($n - 1) / 2)] }
    return [math]::Round(($s[$n / 2 - 1] + $s[$n / 2]) / 2, 2)
}

function Show-MedianTable {
    param($Runs, [string]$Title)
    $groups = $Runs | Group-Object 组
    $rows = @()
    foreach ($g in $groups) {
        $m = $g.Group
        $rows += [pscustomobject]@{
            组      = $g.Name
            版本    = $m[0].版本
            Private = (Get-Median ($m | ForEach-Object { $_.PrivateMB }))
            WS      = (Get-Median ($m | ForEach-Object { $_.WSMB }))
            句柄    = (Get-Median ($m | ForEach-Object { $_.句柄 }))
            线程    = (Get-Median ($m | ForEach-Object { $_.线程 }))
            GDI     = (Get-Median ($m | ForEach-Object { $_.GDI }))
            USER    = (Get-Median ($m | ForEach-Object { $_.USER }))
            托盘    = $m[0].托盘
            完整性  = $m[0].完整性
            插件    = $m[0].插件
        }
    }
    Write-Host ""
    Write-Host "==== $Title ===="
    Write-Host (($rows | Format-Table -AutoSize | Out-String -Width 220).TrimEnd())
    return $rows
}

# 把某个构建复制到中性目录（同构：exe + pdfium.dll + plugins\ + portable.lock）
function New-Neutral([string]$SrcDir, [string]$DstDir) {
    if (Test-Path $DstDir) { Remove-Item $DstDir -Recurse -Force }
    New-Item -ItemType Directory -Path $DstDir -Force | Out-Null
    Copy-Item (Join-Path $SrcDir "qlrust.exe") $DstDir -Force
    if (Test-Path (Join-Path $SrcDir "pdfium.dll")) { Copy-Item (Join-Path $SrcDir "pdfium.dll") $DstDir -Force }
    $plug = Join-Path $DstDir "plugins"
    New-Item -ItemType Directory -Path $plug -Force | Out-Null
    Get-ChildItem (Join-Path $SrcDir "qlr_plugin_*.dll") -ErrorAction SilentlyContinue | Copy-Item -Destination $plug -Force
    New-Item -ItemType File -Path (Join-Path $DstDir "portable.lock") | Out-Null
    return $DstDir
}

# ---------------------------------------------------------------------------
# Mode ab：1.8.2 vs 1.8.3
# ---------------------------------------------------------------------------
function Run-AB {
    $src182 = Resolve-Dir $Ver182
    $src183 = Resolve-Dir $Ver183
    if (-not (Test-Path (Join-Path $src182 "qlrust.exe"))) { throw "找不到 1.8.2 产物：$src182（先 worktree + cargo build --release）" }
    $a = New-Neutral $src182 (Join-Path $NeutralRoot "a182")
    $b = New-Neutral $src183 (Join-Path $NeutralRoot "b183")
    # 原地（DSH 工作目录，令牌完整性 low → 托盘失败）——只用来量化"环境假象"
    $inplace = $src183

    $allRuns = @()
    for ($i = 1; $i -le $Runs; $i++) {
        $allRuns += Measure-Idle -ExeDir $a -Tag "1.8.2 中性目录"
        Write-Host ("  [1.8.2] 轮 {0}：WS {1} / Private {2} / 句柄 {3} / 线程 {4} / GDI {5} / USER {6} / 托盘 {7}" -f `
                $i, $allRuns[-1].WSMB, $allRuns[-1].PrivateMB, $allRuns[-1].句柄, $allRuns[-1].线程, $allRuns[-1].GDI, $allRuns[-1].USER, $allRuns[-1].托盘)
    }
    for ($i = 1; $i -le $Runs; $i++) {
        $allRuns += Measure-Idle -ExeDir $b -Tag "1.8.3 中性目录"
        Write-Host ("  [1.8.3 中性] 轮 {0}：WS {1} / Private {2} / 句柄 {3} / 线程 {4} / GDI {5} / USER {6} / 托盘 {7}" -f `
                $i, $allRuns[-1].WSMB, $allRuns[-1].PrivateMB, $allRuns[-1].句柄, $allRuns[-1].线程, $allRuns[-1].GDI, $allRuns[-1].USER, $allRuns[-1].托盘)
    }
    for ($i = 1; $i -le $Runs; $i++) {
        $allRuns += Measure-Idle -ExeDir $inplace -Tag "1.8.3 DSH 原地"
        Write-Host ("  [1.8.3 原地] 轮 {0}：WS {1} / Private {2} / 句柄 {3} / 线程 {4} / GDI {5} / USER {6} / 托盘 {7}" -f `
                $i, $allRuns[-1].WSMB, $allRuns[-1].PrivateMB, $allRuns[-1].句柄, $allRuns[-1].线程, $allRuns[-1].GDI, $allRuns[-1].USER, $allRuns[-1].托盘)
    }
    $rows = Show-MedianTable -Runs $allRuns -Title ("A/B（裸启、不预览、静置 10 s；每组 $Runs 轮中位）")
    $out = [ordered]@{ runs = $allRuns; medians = $rows }
    $out | ConvertTo-Json -Depth 6 | Set-Content (Join-Path $PSScriptRoot "residency-ab.json") -Encoding UTF8
}

# ---------------------------------------------------------------------------
# Mode attrib：1.8.3 上逐项关候选
# ---------------------------------------------------------------------------
function Set-ConfigField {
    param([string]$Dir, [string]$Field, $Value)
    $f = Join-Path $Dir "UserData\config.json"
    $json = Get-Content $f -Raw | ConvertFrom-Json
    $json.$Field = $Value
    ($json | ConvertTo-Json -Depth 8) | Set-Content $f -Encoding UTF8
}

function Run-Attrib {
    $src183 = Resolve-Dir $Ver183
    $b = New-Neutral $src183 (Join-Path $NeutralRoot "b183")
    # 先把 config.json 造出来：裸启不预览时程序没有位置/尺寸要记，退出时不写配置文件；
    # 所以先真预览一次再退出，让 `remember_position` 把它落盘。
    $null = Measure-Previewed -ExeDir $b -Tag "warmup" -File (Join-Path $Fixtures "png_1920x1080.png")
    $cfg = Join-Path $b "UserData\config.json"
    $backup = Join-Path $PSScriptRoot "residency-config-backup.json"
    if (-not (Test-Path $cfg)) { throw "暖机后仍然没有生成 config.json：$cfg" }
    Copy-Item $cfg $backup -Force

    $allRuns = @()
    $variants = @(
        @{ tag = "基线（默认配置）"; env = @{}; mutate = $null },
        @{ tag = "QLRUST_DPI_FIX=0"; env = @{ QLRUST_DPI_FIX = "0" }; mutate = $null },
        @{ tag = "content_scale_percent=100"; env = @{}; mutate = @{ field = "content_scale_percent"; value = 100 } },
        @{ tag = "content_scale_percent=0（自动 sqrt）"; env = @{}; mutate = @{ field = "content_scale_percent"; value = 0 } },
        @{ tag = "prewarm=off"; env = @{}; mutate = @{ field = "prewarm"; value = "off" } },
        @{ tag = "prewarm=always"; env = @{}; mutate = @{ field = "prewarm"; value = "always" } }
    )
    foreach ($v in $variants) {
        if ($v.mutate) { Set-ConfigField -Dir $b -Field $v.mutate.field -Value $v.mutate.value }
        for ($i = 1; $i -le $Runs; $i++) {
            $r = Measure-Idle -ExeDir $b -Tag $v.tag -Env $v.env
            $allRuns += $r
            Write-Host ("  [{0}] 轮 {1}：WS {2} / Private {3} / 句柄 {4} / 线程 {5} / GDI {6} / USER {7}" -f `
                    $v.tag, $i, $r.WSMB, $r.PrivateMB, $r.句柄, $r.线程, $r.GDI, $r.USER)
        }
        if ($v.mutate) { Copy-Item $backup $cfg -Force }   # 每项测完都回默认，避免相互污染
    }

    # 「从未预览」vs「预览过一次图片」：字体/绘制相关对象是首次绘制才建的
    $previewFile = Join-Path $Fixtures "png_1920x1080.png"
    for ($i = 1; $i -le $Runs; $i++) {
        $r = Measure-Previewed -ExeDir $b -Tag "预览过一次图片（1920 PNG）" -File $previewFile
        $allRuns += $r
        Write-Host ("  [预览一次] 轮 {0}：WS {1} / Private {2} / 句柄 {3} / 线程 {4} / GDI {5} / USER {6}" -f `
                $i, $r.WSMB, $r.PrivateMB, $r.句柄, $r.线程, $r.GDI, $r.USER)
    }

    Copy-Item $backup $cfg -Force
    $rows = Show-MedianTable -Runs $allRuns -Title ("归因（1.8.3 中性目录，裸启静置 10 s；每项 $Runs 轮中位）")
    $out = [ordered]@{ runs = $allRuns; medians = $rows }
    $out | ConvertTo-Json -Depth 6 | Set-Content (Join-Path $PSScriptRoot "residency-attrib.json") -Encoding UTF8
}

# 预览一次（show 一张图 → 等 3 s → 采样；**不**退出，量的是"看过一张图之后的水位"）
function Measure-Previewed {
    param([string]$ExeDir, [string]$Tag, [string]$File)
    Kill-All
    $exe = Join-Path $ExeDir "qlrust.exe"
    $log = Read-Log $ExeDir
    if (Test-Path $log) { Remove-Item $log -Force -ErrorAction SilentlyContinue }
    $proc = Start-Exe -Exe $exe -WorkDir $ExeDir
    $deadline = (Get-Date).AddSeconds(15)
    while ((Get-Date) -lt $deadline) {
        if ((Get-LogText $ExeDir) -match "热键就绪") { break }
        Start-Sleep -Milliseconds 10
    }
    Start-Sleep -Milliseconds 1500
    $idle = Get-Snap -ProcessId $proc.Id
    $q = Start-Exe -Exe $exe -WorkDir $ExeDir -ArgList @("--show", $File)
    $null = $q.WaitForExit(8000)
    Start-Sleep -Seconds 3
    $snap = Get-Snap -ProcessId $proc.Id
    $idleTxt = "idle WS $($idle.WSMB) / Private $($idle.PrivateMB) / 句柄 $($idle.句柄) / 线程 $($idle.线程) / GDI $($idle.GDI) / USER $($idle.USER)"
    # 优雅退出（--quit）：配置文件只在正常退出时落盘，强杀不会写
    $q2 = Start-Exe -Exe $exe -WorkDir $ExeDir -ArgList @("--quit")
    $null = $q2.WaitForExit(8000)
    for ($k = 0; $k -lt 500; $k++) {
        if (-not (Get-Process -Id $proc.Id -ErrorAction SilentlyContinue)) { break }
        Start-Sleep -Milliseconds 10
    }
    Kill-All
    return [pscustomobject]@{
        组        = $Tag
        版本      = (Get-Item $exe).VersionInfo.FileVersion
        目录      = $ExeDir
        就绪ms    = -1
        PrivateMB = $(if ($snap) { $snap.PrivateMB } else { -1 })
        WSMB      = $(if ($snap) { $snap.WSMB } else { -1 })
        句柄      = $(if ($snap) { $snap.句柄 } else { -1 })
        线程      = $(if ($snap) { $snap.线程 } else { -1 })
        GDI       = $(if ($snap) { $snap.GDI } else { -1 })
        USER      = $(if ($snap) { $snap.USER } else { -1 })
        托盘      = ""
        完整性    = $idleTxt
        插件      = ""
    }
}

# ---------------------------------------------------------------------------
# Mode after：预览过一次再收起，静置 10 s 的水位（看"最后一帧缓存"留在哪一边）
# ---------------------------------------------------------------------------
function Run-After {
    $src182 = Resolve-Dir $Ver182
    $src183 = Resolve-Dir $Ver183
    $a = New-Neutral $src182 (Join-Path $NeutralRoot "a182")
    $b = New-Neutral $src183 (Join-Path $NeutralRoot "b183")
    $file = Join-Path $Fixtures "png_1920x1080.png"
    $allRuns = @()
    foreach ($pair in @(@{ dir = $a; tag = "1.8.2 中性" }, @{ dir = $b; tag = "1.8.3 中性" })) {
        for ($i = 1; $i -le $Runs; $i++) {
            $r = Measure-Hidden -ExeDir $pair.dir -Tag $pair.tag -File $file
            $allRuns += $r
            Write-Host ("  [{0}] 轮 {1}：WS {2} / Private {3} / 句柄 {4} / 线程 {5} / GDI {6} / USER {7}" -f `
                    $pair.tag, $i, $r.WSMB, $r.PrivateMB, $r.句柄, $r.线程, $r.GDI, $r.USER)
        }
    }
    $rows = Show-MedianTable -Runs $allRuns -Title ("预览一次并收起后静置 10 s（每组 $Runs 轮中位）")
    [ordered]@{ runs = $allRuns; medians = $rows } |
        ConvertTo-Json -Depth 6 | Set-Content (Join-Path $PSScriptRoot "residency-after.json") -Encoding UTF8
}

function Measure-Hidden {
    param([string]$ExeDir, [string]$Tag, [string]$File)
    Kill-All
    $exe = Join-Path $ExeDir "qlrust.exe"
    $log = Read-Log $ExeDir
    if (Test-Path $log) { Remove-Item $log -Force -ErrorAction SilentlyContinue }
    $proc = Start-Exe -Exe $exe -WorkDir $ExeDir
    $deadline = (Get-Date).AddSeconds(15)
    while ((Get-Date) -lt $deadline) {
        if ((Get-LogText $ExeDir) -match "热键就绪") { break }
        Start-Sleep -Milliseconds 10
    }
    Start-Sleep -Milliseconds 800
    $q = Start-Exe -Exe $exe -WorkDir $ExeDir -ArgList @("--show", $File)
    $null = $q.WaitForExit(8000)
    Start-Sleep -Seconds 3
    $shown = Get-Snap -ProcessId $proc.Id
    $q2 = Start-Exe -Exe $exe -WorkDir $ExeDir -ArgList @("--hide")
    $null = $q2.WaitForExit(8000)
    Start-Sleep -Seconds 10
    $snap = Get-Snap -ProcessId $proc.Id
    $q3 = Start-Exe -Exe $exe -WorkDir $ExeDir -ArgList @("--quit")
    $null = $q3.WaitForExit(8000)
    for ($k = 0; $k -lt 500; $k++) {
        if (-not (Get-Process -Id $proc.Id -ErrorAction SilentlyContinue)) { break }
        Start-Sleep -Milliseconds 10
    }
    Kill-All
    return [pscustomobject]@{
        组        = $Tag
        版本      = (Get-Item $exe).VersionInfo.FileVersion
        目录      = $ExeDir
        就绪ms    = -1
        PrivateMB = $(if ($snap) { $snap.PrivateMB } else { -1 })
        WSMB      = $(if ($snap) { $snap.WSMB } else { -1 })
        句柄      = $(if ($snap) { $snap.句柄 } else { -1 })
        线程      = $(if ($snap) { $snap.线程 } else { -1 })
        GDI       = $(if ($snap) { $snap.GDI } else { -1 })
        USER      = $(if ($snap) { $snap.USER } else { -1 })
        托盘      = ""
        完整性    = "预览时 WS $($shown.WSMB) / Private $($shown.PrivateMB) / GDI $($shown.GDI)"
        插件      = ""
    }
}

# ---------------------------------------------------------------------------
# Mode cross：把 1.8.2 也搬进 DSH 工作目录（令牌完整性=low，托盘必然失败）
# 目的：证明「线程 +2 / 句柄 +77 / GDI +8 / WS +4.9 MB」是环境造成，而不是 1.8.3 代码
# ---------------------------------------------------------------------------
function Run-Cross {
    $src182 = Resolve-Dir $Ver182
    $src183 = Resolve-Dir $Ver183
    # 放在仓库旁边的 DSH 目录里（同样是 low 完整性），但不在仓库内，避免污染 git 工作树
    $lowRoot = Join-Path (Split-Path -Parent $root) "_qlr-lowtest"   # = <DSH 根>\_qlr-lowtest（低完整性）
    $a = New-Neutral $src182 (Join-Path $lowRoot "a182-low")
    $allRuns = @()
    for ($i = 1; $i -le $Runs; $i++) {
        $r = Measure-Idle -ExeDir $a -Tag "1.8.2 DSH 目录（low）"
        $allRuns += $r
        Write-Host ("  [1.8.2 low] 轮 {0}：WS {1} / Private {2} / 句柄 {3} / 线程 {4} / GDI {5} / USER {6} / 托盘 {7}" -f `
                $i, $r.WSMB, $r.PrivateMB, $r.句柄, $r.线程, $r.GDI, $r.USER, $r.托盘)
    }
    $rows = Show-MedianTable -Runs $allRuns -Title ("交叉验证：1.8.2 放进 low 完整性目录（$Runs 轮中位）")
    [ordered]@{ runs = $allRuns; medians = $rows } |
        ConvertTo-Json -Depth 6 | Set-Content (Join-Path $PSScriptRoot "residency-cross.json") -Encoding UTF8
}

# ---------------------------------------------------------------------------
Kill-All
Write-Host ("空闲常驻测量开始 {0}（Mode={1}）" -f (Get-Date), $Mode)
switch ($Mode) {
    "ab"     { Run-AB }
    "attrib" { Run-Attrib }
    "after"  { Run-After }
    "cross"  { Run-Cross }
    default  { throw "未知 -Mode：$Mode" }
}
Kill-All
Write-Host "完成；已清场"


