# 对比模式（多窗口 / 锁，1.12.0）端到端验证
#
# 这是**有窗口**校验：没有桌面会话就整体 SKIP（不算失败，纯 CI 上无法造两个可见窗）。
# 流程（忠实复刻人工操作）：
#   1. 便携模式起常驻实例（宿主）。
#   2. `qlrust <A>` 让 A 在驱动窗显示——断言出现 1 个预览窗且画面非空白。
#   3. `qlrust --pin` 点「锁」——断言出现**第二个**预览窗（A 被冻结仍留在屏幕上）。
#   4. `qlrust <B>` 让 B 在新驱动窗显示——断言**两个预览窗同时存在且都非空白**、
#      日志出现两次出帧、无 panic、进程仍在。
#   5. `qlrust --quit` 收尾。
#
# 说明：`--pin` 是 Action::Pin 的等价入口（GUI 里没有可脚本化的点击）；页/滚轮等
# 键盘注入被低层钩子按设计忽略（CONVENTIONS #162），这里只验"多窗并存 + 各自出帧"。
#
# 用法：
#   pwsh -NoProfile -File scripts/verify-multiwin.ps1
#   pwsh -NoProfile -File scripts/verify-multiwin.ps1 -Exe target\debug\qlrust.exe -Fixtures D:\office-samples
# 退出码：0 = 全过/SKIP；1 = 有失败项。

param(
    [string]$Exe = "target\release\qlrust.exe",
    [string]$OutDir = "..\_bench\multiwin",
    [string]$Fixtures = "",
    [switch]$NoGui
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not $Fixtures) {
    $Fixtures = if ($env:QLRUST_DOCX_FIXTURES) { $env:QLRUST_DOCX_FIXTURES } else { Join-Path (Split-Path -Parent $root) "_bench\office2pdf" }
}
if (-not (Test-Path $Exe)) { throw "找不到可执行体：$Exe（先 cargo build --release）" }
$Exe = (Resolve-Path $Exe).Path
if (-not (Test-Path $OutDir)) { New-Item -ItemType Directory -Path $OutDir -Force | Out-Null }
$OutDir = (Resolve-Path $OutDir).Path

# 便携模式：副本放 %TEMP%（CWD 里的 exe 会被打 Low 完整性标签，见 CONVENTIONS #29）。
$work = Join-Path $env:TEMP "qlrust-multiwin-verify"
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

$script:fail = 0; $script:pass = 0; $script:skip = 0
function Check { param([string]$Name, [bool]$Ok, [string]$Detail = "")
    if ($Ok) { $script:pass++; Write-Host ("  PASS  {0}{1}" -f $Name, $(if ($Detail) { " — $Detail" } else { "" })) -ForegroundColor Green }
    else { $script:fail++; Write-Host ("  FAIL  {0}{1}" -f $Name, $(if ($Detail) { " — $Detail" } else { "" })) -ForegroundColor Red }
}
function Skip { param([string]$Name, [string]$Why)
    $script:skip++; Write-Host ("  SKIP  {0} — {1}" -f $Name, $Why) -ForegroundColor Yellow
}
function Get-Log { if (-not (Test-Path $logFile)) { return @() }; try { return @(Get-Content $logFile -ErrorAction Stop) } catch { return @() } }

Write-Host "== 对比模式（多窗口 / 锁，1.12.0）验证 ==" -ForegroundColor Cyan
Write-Host "exe=$Exe"
Write-Host "fixtures=$Fixtures"
Write-Host ""

if ($NoGui) { Skip "多窗口 GUI 校验" "指定了 -NoGui"; Write-Host ("== 结果：{0} PASS / {1} FAIL / {2} SKIP ==" -f $script:pass, $script:fail, $script:skip); exit 0 }

# 两个不同夹具，便于看"两个窗各自内容"。
$fileA = Join-Path $Fixtures "fixtures\cn-report.docx"
$fileB = Join-Path $Fixtures "fixtures\big-report.docx"
if (-not (Test-Path $fileA) -or -not (Test-Path $fileB)) {
    Skip "多窗口 GUI 校验" "缺夹具（需要 fixtures\cn-report.docx 与 big-report.docx）"
    Write-Host ("== 结果：{0} PASS / {1} FAIL / {2} SKIP ==" -f $script:pass, $script:fail, $script:skip); exit 0
}

try {
    Add-Type -Namespace MW -Name Win -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumWinProc cb, IntPtr p);
public delegate bool EnumWinProc(IntPtr hWnd, IntPtr lParam);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, System.Text.StringBuilder s, int n);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
[DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
[DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint f);
public struct RECT { public int L, T, R, B; }
'@
    Add-Type -AssemblyName System.Drawing
} catch {
    Skip "多窗口 GUI 校验" "GUI 不可用：$($_.Exception.Message)"
    Write-Host ("== 结果：{0} PASS / {1} FAIL / {2} SKIP ==" -f $script:pass, $script:fail, $script:skip); exit 0
}

function EnumPreviews {
    $list = New-Object System.Collections.ArrayList
    $cb = [MW.Win+EnumWinProc] {
        param($h, $l)
        $sb = New-Object System.Text.StringBuilder 256
        [void][MW.Win]::GetClassName($h, $sb, 256)
        if ($sb.ToString() -eq "QLRust.PreviewWindow" -and [MW.Win]::IsWindowVisible($h)) { [void]$list.Add($h) }
        return $true
    }
    [void][MW.Win]::EnumWindows($cb, [IntPtr]::Zero)
    return @($list)
}
function Get-InkRatio { param([IntPtr]$Hwnd, [string]$Out)
    $r = New-Object MW.Win+RECT
    [void][MW.Win]::GetClientRect($Hwnd, [ref]$r)
    $w = $r.R - $r.L; $h = $r.B - $r.T
    if ($w -lt 40 -or $h -lt 40) { return 0.0 }
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc(); [void][MW.Win]::PrintWindow($Hwnd, $hdc, 2); $g.ReleaseHdc($hdc); $g.Dispose()
    if ($Out) { $bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png) }
    $hit = 0; $total = 0
    for ($y = 3; $y -lt $h; $y += 7) { for ($x = 3; $x -lt $w; $x += 7) { $c = $bmp.GetPixel($x, $y); $total++; if ($c.R -lt 240 -or $c.G -lt 240 -or $c.B -lt 240) { $hit++ } } }
    $bmp.Dispose()
    if ($total -eq 0) { return 0.0 }
    return [double]$hit / [double]$total
}
function Start-Host {
    Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 300
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $app; $psi.WorkingDirectory = $work; $psi.UseShellExecute = $false
    [void][System.Diagnostics.Process]::Start($psi)
    $deadline = (Get-Date).AddSeconds(15)
    while ((Get-Date) -lt $deadline) {
        if ((Get-Log | Select-String -SimpleMatch "壳层就绪" -Quiet)) { return $true }
        Start-Sleep -Milliseconds 100
    }
    return $false
}
# 转发一条请求（文件路径或开关），转发进程很快退出。
function Send-Req { param([string[]]$ArgList)
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $app; $psi.WorkingDirectory = $work; $psi.UseShellExecute = $false
    foreach ($a in $ArgList) { [void]$psi.ArgumentList.Add($a) }
    $p = [System.Diagnostics.Process]::Start($psi); $p.WaitForExit(); return $p.ExitCode
}
function Wait-Count { param([int]$AtLeast, [int]$Seconds)
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        if (@(EnumPreviews).Count -ge $AtLeast) { return $true }
        Start-Sleep -Milliseconds 150
    }
    return (@(EnumPreviews).Count -ge $AtLeast)
}

$gui = $false
if (-not (Start-Host)) {
    Skip "多窗口 GUI 校验" "宿主没起来（无桌面会话）"
} else {
    $gui = $true
    $before = (Get-Log).Count

    # 1) 显示 A
    [void](Send-Req @($fileA))
    $ok1 = Wait-Count 1 20
    $textA = ((Get-Log | Select-Object -Skip $before) -join "`n")
    Check "显示 A：出现 1 个预览窗" $ok1 ("windows={0}" -f @(EnumPreviews).Count)
    Check "A 出帧（RENDER … document）" ($textA -match 'RENDER gen=\d+ document')

    # 记住 A 的窗口句柄（锁必须保证它不被复用/替换）
    $tmpA = @(EnumPreviews)
    $hwndA = if ($tmpA.Count -ge 1) { $tmpA[0] } else { [IntPtr]::Zero }

    # 2) 点锁：A 必须仍在（同一句柄，冻结在屏；新驱动窗在未给文件前不显示）
    [void](Send-Req @('--pin'))
    Start-Sleep -Milliseconds 500
    $afterPin = @(EnumPreviews)
    $aKept = $false
    foreach ($h in $afterPin) { if ($h -eq $hwndA) { $aKept = $true } }
    Check "点锁后 A 窗仍在（同一句柄，已冻结）" ($aKept) ("windows={0}" -f $afterPin.Count)

    # 3) 显示 B（新驱动窗）
    $before2 = (Get-Log).Count
    [void](Send-Req @($fileB))
    $deadline = (Get-Date).AddSeconds(20)
    while ((Get-Date) -lt $deadline) {
        if (((Get-Log | Select-Object -Skip $before2) -join "`n") -match 'RENDER gen=\d+ document') { break }
        Start-Sleep -Milliseconds 150
    }
    Start-Sleep -Milliseconds 400

    $wins = @(EnumPreviews)
    Check "两个预览窗同时存在" ($wins.Count -ge 2) ("windows={0}" -f $wins.Count)
    $newHandle = $false
    foreach ($h in $wins) { if ($h -ne $hwndA) { $newHandle = $true } }
    Check "B 开在*新*窗口（不是复用 A）" ($newHandle -and ($wins.Count -ge 2))

    $inks = @()
    $i = 0
    foreach ($h in $wins) {
        $shot = Join-Path $OutDir ("multiwin-{0}.png" -f $i)
        $inks += (Get-InkRatio $h $shot); $i++
    }
    $nonblank = @($inks | Where-Object { $_ -gt 0.02 }).Count
    Check "两个窗都非空白（各 ink>0.02）" ($nonblank -ge 2) ("inks=" + (($inks | ForEach-Object { "{0:N3}" -f $_ }) -join ","))

    $full = ((Get-Log) -join "`n")
    $renderCount = ([regex]::Matches($full, 'RENDER gen=\d+ document')).Count
    Check "至少两次出帧（A+B 各自渲染）" ($renderCount -ge 2) ("renders={0}" -f $renderCount)
    Check "GUI 无 panic" (-not ($full -match 'panic'))
    Check "预览进程仍在" (@(Get-Process qlrust -ErrorAction SilentlyContinue).Count -gt 0)

    [void](Send-Req @('--quit'))
}

if ($gui) { Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force }
Write-Host ""
Write-Host ("== 结果：{0} PASS / {1} FAIL / {2} SKIP（GUI {3}） ==" -f $script:pass, $script:fail, $script:skip, $(if ($gui) { "已跑" } else { "跳过" })) -ForegroundColor Cyan
if ($script:fail -gt 0) { exit 1 }
exit 0
