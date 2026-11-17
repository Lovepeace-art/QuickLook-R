# 演示文稿预览（1.11.0）端到端验证：原生 pptx 分页渲染
#
# 分两段：
#   A. **无窗口** `--render` 结构校验（不依赖桌面会话，CI 也能跑）：
#      每个 pptx 都由内核算接管（日志 `RENDER-CLI ok kind=presentation bytes=N`），
#      坏文件优雅失败（`RENDER-CLI FAIL`、退出码 3、无 panic），老式 .ppt 不归原生。
#   B. **有窗口** GUI 校验（找不到预览窗口就整体 SKIP，不算失败）：
#      `.pptx` 由内核算 SHOW（`SHOW … kind=presentation renderable=true plugin=-`）；
#      出帧（`RENDER gen=1 presentation`）；画面非空白（PrintWindow + 7px 网格采样）；
#      留一张截图；全程无 panic、进程还在。
#
# 用法：
#   pwsh -NoProfile -File scripts/verify-pptx.ps1
#   pwsh -NoProfile -File scripts/verify-pptx.ps1 -Exe target\release\qlrust.exe -OutDir ..\_bench\pptx
#   pwsh -NoProfile -File scripts/verify-pptx.ps1 -NoGui          # 只跑 A 段
#   pwsh -NoProfile -File scripts/verify-pptx.ps1 -Fixtures D:\office-samples
# 退出码：0 = 全过；1 = 有失败项。
#
# 前置：夹具默认取仓库旁 `..\_bench\office2pdf`（含 real\real-proposal.pptx、
# real\real-deck-9mb.pptx、real\real-deck-62mb.pptx、spike\pptxslide\out\truncated.pptx
# 与 fixtures\ext\office-demo.ppt）。缺哪项就 SKIP。

param(
    [string]$Exe = "target\release\qlrust.exe",
    [string]$OutDir = "..\_bench\pptx",
    [string]$Fixtures = "",
    [int]$BudgetMs = 4000,
    [switch]$NoGui
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not $Fixtures) {
    $Fixtures = if ($env:QLRUST_PPTX_FIXTURES) { $env:QLRUST_PPTX_FIXTURES } else { Join-Path (Split-Path -Parent $root) "_bench\office2pdf" }
}

if (-not (Test-Path $Exe)) { throw "找不到可执行体：$Exe（先 cargo build --release）" }
$Exe = (Resolve-Path $Exe).Path
if (-not (Test-Path $OutDir)) { New-Item -ItemType Directory -Path $OutDir -Force | Out-Null }
$OutDir = (Resolve-Path $OutDir).Path

# 便携模式：副本放 %TEMP%（工作目录里的 exe 会被打 Low 完整性标签，见 CONVENTIONS #29）。
$work = Join-Path $env:TEMP "qlrust-pptx-verify"
if (Test-Path $work) {
    Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 250
    Remove-Item $work -Recurse -Force
}
New-Item -ItemType Directory -Path $work -Force | Out-Null
$app = Join-Path $work "qlrust.exe"
Copy-Item $Exe $app
New-Item -ItemType File -Path (Join-Path $work "portable.lock") | Out-Null
New-Item -ItemType Directory -Path (Join-Path $work "UserData") -Force | Out-Null
'{"log_level":"debug"}' | Set-Content (Join-Path $work "UserData\config.json") -Encoding UTF8
$logFile = Join-Path $work "UserData\logs\app.log"

if (-not $NoGui) {
    try {
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
        $script:canGui = $true
    } catch {
        Write-Host "（GUI 校验不可用：$($_.Exception.Message)，跳过 B 段）" -ForegroundColor Yellow
        $script:canGui = $false
    }
} else {
    $script:canGui = $false
}

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
    $script:found = [IntPtr]::Zero
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
    [void][SV.Win]::EnumWindows($cb, [IntPtr]::Zero)
    return $script:found
}
function Get-InkRatio { param([IntPtr]$Hwnd, [string]$Out)
    $r = New-Object SV.Win+RECT
    [void][SV.Win]::GetClientRect($Hwnd, [ref]$r)
    $w = $r.R - $r.L; $h = $r.B - $r.T
    if ($w -lt 40 -or $h -lt 40) { return 0.0 }
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    [void][SV.Win]::PrintWindow($Hwnd, $hdc, 2)
    $g.ReleaseHdc($hdc); $g.Dispose()
    if ($Out) { $bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png) }
    $hit = 0; $total = 0
    for ($y = 3; $y -lt $h; $y += 7) {
        for ($x = 3; $x -lt $w; $x += 7) {
            $c = $bmp.GetPixel($x, $y); $total++
            if ($c.R -lt 240 -or $c.G -lt 240 -or $c.B -lt 240) { $hit++ }
        }
    }
    $bmp.Dispose()
    if ($total -eq 0) { return 0.0 }
    return [double]$hit / [double]$total
}

# 无窗口渲染一次（便携模式）。日志在每次调用前清空。返回 @{ Code; Text; PeakMB; Wall }。
function Run-Cli { param([string]$File)
    if (Test-Path $logFile) { Remove-Item $logFile -Force }
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $app
    $psi.WorkingDirectory = $work
    $psi.UseShellExecute = $false
    [void]$psi.ArgumentList.Add('--render')
    [void]$psi.ArgumentList.Add($File)
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $proc = [System.Diagnostics.Process]::Start($psi)
    $proc.WaitForExit()
    $sw.Stop()
    $proc.Refresh()
    $peak = $proc.PeakWorkingSet64
    $code = $proc.ExitCode
    $text = (Get-Log) -join "`n"
    $peakMb = if ($peak -gt 0) { [math]::Round($peak / 1MB, 1) } else { -1 }
    return @{ Code = $code; Text = $text; PeakMB = $peakMb; Wall = $sw.ElapsedMilliseconds }
}

Write-Host "== 演示文稿预览（1.11.0）验证 ==" -ForegroundColor Cyan
Write-Host "exe=$Exe"
Write-Host "fixtures=$Fixtures"
Write-Host ""

# ---------------- A 段：无窗口结构校验 ----------------
Write-Host "-- A 段：无窗口 --render 结构校验 --" -ForegroundColor Cyan
$good = @(
    @{ Name = "real-proposal.pptx";  File = (Join-Path $Fixtures "real\real-proposal.pptx");        Budget = $BudgetMs },
    @{ Name = "real-deck-9mb.pptx";  File = (Join-Path $Fixtures "real\real-deck-9mb.pptx");        Budget = $BudgetMs },
    @{ Name = "real-deck-62mb.pptx"; File = (Join-Path $Fixtures "real\real-deck-62mb.pptx");       Budget = $BudgetMs },
    @{ Name = "truncated.pptx";      File = (Join-Path $Fixtures "spike\pptxslide\out\truncated.pptx"); Budget = $BudgetMs }
)
foreach ($g in $good) {
    if (-not (Test-Path $g.File)) { Skip "$($g.Name) 原生接管" "缺夹具 $($g.File)"; continue }
    $r = Run-Cli $g.File
    $isPres = $r.Text -match 'RENDER-CLI ok kind=presentation'
    $noPanic = -not ($r.Text -match 'panic')
    $m = [regex]::Match($r.Text, 'bytes=(\d+)')
    $bytes = if ($m.Success) { [int]$m.Groups[1].Value } else { 0 }
    $mm = [regex]::Match($r.Text, 'elapsed=(\d+)ms')
    $ms = if ($mm.Success) { [int]$mm.Groups[1].Value } else { -1 }
    Check "$($g.Name) 由内核算接管（kind=presentation）" $isPres
    Check "$($g.Name) 解析出内容（bytes>0）" ($bytes -gt 0) ("bytes={0}" -f $bytes)
    Check "$($g.Name) 退出码 0" ($r.Code -eq 0) ("exit={0}" -f $r.Code)
    Check "$($g.Name) 无 panic" $noPanic
    Check ("{0} 渲染耗时在预算内（{1}ms / {2}ms）" -f $g.Name, $ms, $g.Budget) ($ms -ge 0 -and $ms -le $g.Budget)
    Write-Host ("        峰值常驻 ~{0} MB，整进程墙钟 {1} ms" -f $r.PeakMB, $r.Wall) -ForegroundColor DarkGray
}

# 坏文件：优雅失败、退出码 3、无 panic（现造一个假 pptx）
$fake = Join-Path $OutDir "fake-pptx.pptx"
[System.IO.File]::WriteAllBytes($fake, [byte[]](0x50, 0x4B, 0x03, 0x04, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05))
$r = Run-Cli $fake
Check "fake-pptx.pptx 明确失败（RENDER-CLI FAIL）" ($r.Text -match 'RENDER-CLI FAIL')
Check "fake-pptx.pptx 退出码 3" ($r.Code -eq 3) ("exit={0}" -f $r.Code)
Check "fake-pptx.pptx 无 panic" (-not ($r.Text -match 'panic'))
Check "fake-pptx.pptx 失败原因是 pptx/zip 而非其它" ($r.Text -match '不是有效的 pptx')

# 老式 .ppt 不归原生（二进制 ppt 不在本轮范围，走系统处理器兜底）
$legacy = Join-Path $Fixtures "fixtures\ext\office-demo.ppt"
if (Test-Path $legacy) {
    $r = Run-Cli $legacy
    Check "老式 .ppt 不归原生（没有 kind=presentation）" (-not ($r.Text -match 'kind=presentation'))
} else {
    Skip "老式 .ppt 不归原生" "缺夹具 $legacy"
}

# ---------------- B 段：有窗口 GUI 校验 ----------------
Write-Host ""
Write-Host "-- B 段：有窗口 GUI 校验 --" -ForegroundColor Cyan
$gui = $false
if ($script:canGui -and -not $NoGui) {
    $sample = Join-Path $Fixtures "real\real-proposal.pptx"
    if (-not (Test-Path $sample)) {
        Skip "pptx GUI 出帧" "缺夹具 real-proposal.pptx"
    } else {
        if (Test-Path $logFile) { Remove-Item $logFile -Force }
        try {
            if (-not (Start-Host)) {
                Skip "pptx GUI 出帧" "宿主没起来（无桌面会话）"
            } else {
                $before = (Get-Log).Count
                [void](Send-File $sample)
                $deadline = (Get-Date).AddSeconds(20)
                while ((Get-Date) -lt $deadline) {
                    if ((Get-Log | Select-Object -Skip $before | Select-String -SimpleMatch "RENDER gen=1 presentation" -Quiet)) { break }
                    Start-Sleep -Milliseconds 100
                }
                $text = ((Get-Log | Select-Object -Skip $before) -join "`n")
                $native = ($text -match 'kind=presentation') -and ($text -match 'plugin=-') -and ($text -match 'renderable=true')
                $shell = $text -match 'SHELL 预览已接管'
                Check "pptx 由内核算接管（kind=presentation/renderable=true/plugin=-）" ($native -and -not $shell)
                Check "出帧（RENDER gen=1 presentation）" ($text -match 'RENDER gen=1 presentation')
                $hwnd = Find-Preview
                if ($hwnd -ne [IntPtr]::Zero) {
                    $shot = Join-Path $OutDir "pptx-real-proposal.png"
                    $ink = Get-InkRatio $hwnd $shot
                    Check "画面非空白（ink>0.02）" ($ink -gt 0.02) ("ink={0:N3} → {1}" -f $ink, $shot)
                } else {
                    Skip "画面非空白" "没拿到预览窗口句柄"
                }
                Check "GUI 无 panic" (-not ($text -match 'panic'))
                $alive = @(Get-Process qlrust -ErrorAction SilentlyContinue).Count -gt 0
                Check "预览进程仍在" $alive
                $gui = $true
            }
        } catch {
            Skip "pptx GUI 出帧" "GUI 启动异常：$($_.Exception.Message)"
        } finally {
            Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force
        }
    }
} else {
    Skip "pptx GUI 出帧" "未启用（-NoGui 或无桌面）"
}

Write-Host ""
Write-Host ("== 结果：{0} PASS / {1} FAIL / {2} SKIP（GUI {3}） ==" -f $script:pass, $script:fail, $script:skip, $(if ($gui) { "已跑" } else { "跳过" })) -ForegroundColor Cyan
if ($script:fail -gt 0) { exit 1 }
exit 0
