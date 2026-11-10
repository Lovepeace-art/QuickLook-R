# 表格预览（1.9.0）端到端验证：原生 xlsx 网格
#
# 验证的事（全部靠日志 + PrintWindow 截图，不靠眼睛）：
#   1. `.xlsx` 由**内核算**接管：`SHOW … kind=sheet renderable=true plugin=-`，
#      没有 `SHELL 预览已接管`（一旦出现就说明又退回系统处理器了）。
#   2. 解析+渲染成功：`RENDER gen=1 sheet … ink=N`，`ink > 0`（画出来的不是空白）。
#   3. 窗口可见且像素非纯白（PrintWindow + 7px 网格采样），并留一张截图。
#   4. 大表不炸：`big-20k.xlsx`（160,008 格）能出帧，且 `RENDER elapsed` 在预算内（< 1500 ms）。
#   5. 坏 xlsx（截断/非 zip）优雅失败：`RENDER …失败` 或 `SHOW 拒绝`，全程无 `panic`；
#      进程仍在（没有把预览进程带走）。
#   6. 老式 `.xls` 不归原生：日志里不出现 `kind=sheet`（BIFF 不在本轮范围，见 ADR-0011）。
#
# 用法：
#   pwsh -NoProfile -File scripts/verify-sheet.ps1
#   pwsh -NoProfile -File scripts/verify-sheet.ps1 -Exe target\release\qlrust.exe -OutDir ..\_bench\sheet
#   pwsh -NoProfile -File scripts/verify-sheet.ps1 -Fixtures D:\office-samples
# 退出码：0 = 全过；1 = 有失败项。
#
# 前置：夹具目录里要有 `plugin-demo.xlsx`（默认取 `$env:QLRUST_FIXTURES`，否则仓库旁的
# `..\..\qlrust-dev`）。大表与坏文件夹具缺了对应项就 SKIP，不算失败。
#
# 注意：滚轮/方向键的滚动**不在**本脚本里验——低层键鼠钩子按设计忽略注入事件
# （CONVENTIONS #162），只能人工试。

param(
    [string]$Exe = "target\release\qlrust.exe",
    [string]$OutDir = "..\_bench\sheet",
    [string]$Fixtures = "",
    [int]$BudgetMs = 1500
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not $Fixtures) {
    $Fixtures = if ($env:QLRUST_FIXTURES) { $env:QLRUST_FIXTURES } else { Join-Path $root "..\..\qlrust-dev" }
}

if (-not (Test-Path $Exe)) { throw "找不到可执行体：$Exe（先 cargo build --release）" }
$Exe = (Resolve-Path $Exe).Path
if (-not (Test-Path $OutDir)) { New-Item -ItemType Directory -Path $OutDir -Force | Out-Null }
$OutDir = (Resolve-Path $OutDir).Path

# 便携模式（数据写在 exe 同目录）。副本放 %TEMP%：DSH 工作目录里的 exe 会被打 Low 完整性
# 标签，托盘 / Shell 预览 / UIPI 相关能力都会变（CONVENTIONS #29）。
$work = Join-Path $env:TEMP "qlrust-sheet-verify"
if (Test-Path $work) {
    Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 250
    Remove-Item $work -Recurse -Force
}
New-Item -ItemType Directory -Path $work -Force | Out-Null
$app = Join-Path $work "qlrust.exe"
Copy-Item $Exe $app
New-Item -ItemType File -Path (Join-Path $work "portable.lock") | Out-Null
$srcDlls = @(Get-ChildItem (Join-Path $root "target\release") -Filter "qlr_plugin_*.dll" -ErrorAction SilentlyContinue)
if ($srcDlls.Count -gt 0) {
    $dstPlugins = Join-Path $work "plugins"
    New-Item -ItemType Directory -Path $dstPlugins -Force | Out-Null
    $srcDlls | ForEach-Object { Copy-Item $_.FullName $dstPlugins -Force }
}
$logFile = Join-Path $work "UserData\logs\app.log"
New-Item -ItemType Directory -Path (Join-Path $work "UserData") -Force | Out-Null
'{"log_level":"debug"}' | Set-Content (Join-Path $work "UserData\config.json") -Encoding UTF8

Add-Type -Namespace SV -Name Win -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumWinProc cb, IntPtr p);
public delegate bool EnumWinProc(IntPtr hWnd, IntPtr lParam);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, System.Text.StringBuilder s, int n);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
[DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
[DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint f);
public struct RECT { public int L, T, R, B; }
'@

Add-Type -AssemblyName System.Drawing

$script:fail = 0
$script:pass = 0
$script:skip = 0

function Check { param([string]$Name, [bool]$Ok, [string]$Detail = "")
    if ($Ok) {
        $script:pass++
        Write-Host ("  PASS  {0}{1}" -f $Name, $(if ($Detail) { " — $Detail" } else { "" })) -ForegroundColor Green
    } else {
        $script:fail++
        Write-Host ("  FAIL  {0}{1}" -f $Name, $(if ($Detail) { " — $Detail" } else { "" })) -ForegroundColor Red
    }
}

function Skip { param([string]$Name, [string]$Why)
    $script:skip++
    Write-Host ("  SKIP  {0} — {1}" -f $Name, $Why) -ForegroundColor Yellow
}

function Get-Log {
    if (-not (Test-Path $logFile)) { return @() }
    try { return @(Get-Content $logFile -ErrorAction Stop) } catch { return @() }
}

function Start-Host {
    Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 300
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $app
    $psi.WorkingDirectory = $work
    $psi.UseShellExecute = $false
    [void][System.Diagnostics.Process]::Start($psi)
    $deadline = (Get-Date).AddSeconds(15)
    while ((Get-Date) -lt $deadline) {
        if ((Get-Log | Select-String -SimpleMatch "壳层就绪" -Quiet)) { return $true }
        Start-Sleep -Milliseconds 100
    }
    return $false
}

function Send-File { param([string]$Path)
    $args = @{ CommandLine = ('"' + $app + '" "' + $Path + '"') }
    $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments $args
    return ($null -ne $r.ProcessId)
}

function Find-Preview {
    $found = [IntPtr]::Zero
    $cb = [SV.Win+EnumWinProc] {
        param($h, $l)
        $sb = New-Object System.Text.StringBuilder 256
        [void][SV.Win]::GetClassName($h, $sb, 256)
        if ($sb.ToString() -eq "QLRust.PreviewWindow" -and [SV.Win]::IsWindowVisible($h)) {
            $script:found = $h
            return $false
        }
        return $true
    }
    $script:found = [IntPtr]::Zero
    [void][SV.Win]::EnumWindows($cb, [IntPtr]::Zero)
    return $script:found
}

# 截图非纯白占比（7px 网格采样）：空白/白底窗口会接近 0。
function Get-InkRatio { param([IntPtr]$Hwnd, [string]$Out)
    $r = New-Object SV.Win+RECT
    [void][SV.Win]::GetClientRect($Hwnd, [ref]$r)
    $w = $r.R - $r.L
    $h = $r.B - $r.T
    if ($w -lt 40 -or $h -lt 40) { return 0.0 }
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    [void][SV.Win]::PrintWindow($Hwnd, $hdc, 2)
    $g.ReleaseHdc($hdc)
    $g.Dispose()
    if ($Out) { $bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png) }
    $hit = 0
    $total = 0
    for ($y = 3; $y -lt $h; $y += 7) {
        for ($x = 3; $x -lt $w; $x += 7) {
            $c = $bmp.GetPixel($x, $y)
            $total++
            if ($c.R -lt 240 -or $c.G -lt 240 -or $c.B -lt 240) { $hit++ }
        }
    }
    $bmp.Dispose()
    if ($total -eq 0) { return 0.0 }
    return [double]$hit / [double]$total
}

Write-Host "== 表格预览（1.9.0）验证 ==" -ForegroundColor Cyan
Write-Host "exe=$Exe"
Write-Host ""

$sample = Join-Path $Fixtures "plugin-demo.xlsx"
$big = Join-Path $Fixtures "big-20k.xlsx"
if (-not (Test-Path $big)) { $big = Join-Path (Split-Path -Parent $root) "_bench\office2pdf\fixtures\big-20k.xlsx" }

# ---------- 1) 正常 xlsx：原生接管 + 出帧 ----------
if (-not (Test-Path $sample)) {
    Skip "xlsx 原生接管" "缺夹具 plugin-demo.xlsx（$Fixtures）"
    Skip "xlsx 出帧 ink" "缺夹具"
} else {
    $before = (Get-Log).Count
    if (-not (Start-Host)) { throw "宿主没起来（日志里没有「壳层就绪」）" }
    [void](Send-File $sample)
    $deadline = (Get-Date).AddSeconds(20)
    $ok = $false
    while ((Get-Date) -lt $deadline) {
        if ((Get-Log | Select-Object -Skip $before | Select-String -SimpleMatch "RENDER gen=1 sheet" -Quiet)) { $ok = $true; break }
        Start-Sleep -Milliseconds 100
    }
    $lines = @(Get-Log | Select-Object -Skip $before)
    $text = ($lines -join "`n")

    $native = $text -match "kind=sheet" -and $text -match "plugin=-"
    $shell = $text -match "SHELL 预览已接管"
    Check "xlsx 由内核算接管（kind=sheet 且 plugin=-，没有退回系统处理器）" ($native -and -not $shell)

    $ink = 0.0
    $hwnd = Find-Preview
    if ($hwnd -ne [IntPtr]::Zero) {
        $shot = Join-Path $OutDir "demo.png"
        $ink = Get-InkRatio $hwnd $shot
        $ok = $ok -and ($ink -gt 0.02)
        Check "出帧（RENDER gen=1 sheet）" ($text -match "RENDER gen=1 sheet")
        Check "画面非空白（ink>0.02）" ($ink -gt 0.02) ("ink={0:N3} → {1}" -f $ink, $shot)
    } else {
        Check "预览窗口出现" $false
        Skip "画面非空白" "没拿到窗口句柄"
    }
    Check "没有 panic" (-not ($text -match "panic"))
    Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force
}

# ---------- 2) 大表：出帧且在预算内 ----------
if (-not (Test-Path $big)) {
    Skip "大表（160k 格）出帧" "缺 big-20k.xlsx"
} else {
    $before = (Get-Log).Count
    if (-not (Start-Host)) { throw "宿主没起来（大表轮）" }
    [void](Send-File $big)
    $deadline = (Get-Date).AddSeconds(30)
    $m = $null
    while ((Get-Date) -lt $deadline) {
        $m = (Get-Log | Select-Object -Skip $before | Select-String -Pattern "RENDER gen=1 sheet.*elapsed=(\d+)ms" | Select-Object -Last 1)
        if ($m) { break }
        Start-Sleep -Milliseconds 100
    }
    if ($m) {
        $ms = [int]$m.Matches[0].Groups[1].Value
        Check ("大表出帧（elapsed={0}ms，预算 {1}ms）" -f $ms, $BudgetMs) ($ms -le $BudgetMs)
        $text = ((Get-Log | Select-Object -Skip $before) -join "`n")
        Check "大表没有 panic" (-not ($text -match "panic"))
    } else {
        Check "大表出帧" $false "20s 内没等到 RENDER"
    }
    Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force
}

# ---------- 3) 坏 xlsx：优雅失败、进程还在 ----------
$broken = Join-Path $OutDir "broken.xlsx"
[System.IO.File]::WriteAllText($broken, "this is not a zip at all")
$before = (Get-Log).Count
if (-not (Start-Host)) { throw "宿主没起来（坏文件轮）" }
[void](Send-File $broken)
Start-Sleep -Milliseconds 1500
$text = ((Get-Log | Select-Object -Skip $before) -join "`n")
Check "坏 xlsx 不 panic" (-not ($text -match "panic"))
Check "坏 xlsx 有明确失败/拒绝日志" (($text -match "RENDER") -or ($text -match "SHOW 拒绝") -or ($text -match "失败"))
$alive = @(Get-Process qlrust -ErrorAction SilentlyContinue).Count -gt 0
Check "坏 xlsx 之后预览进程仍在" $alive
Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force

# ---------- 4) 老式 .xls 不归原生 ----------
$legacy = Join-Path $OutDir "legacy.xls"
Copy-Item $sample $legacy -Force
$before = (Get-Log).Count
if (-not (Start-Host)) { throw "宿主没起来（legacy 轮）" }
[void](Send-File $legacy)
Start-Sleep -Milliseconds 1500
$text = ((Get-Log | Select-Object -Skip $before) -join "`n")
Check ".xls 不走原生表格（没有 kind=sheet）" (-not ($text -match "kind=sheet"))
Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force

Write-Host ""
Write-Host ("== 结果：{0} PASS / {1} FAIL / {2} SKIP ==" -f $script:pass, $script:fail, $script:skip) -ForegroundColor Cyan
if ($script:fail -gt 0) { exit 1 }
exit 0
