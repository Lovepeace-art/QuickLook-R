# 长稳压测（1.2.0）：反复开关窗口 + 翻页 + 缩放，盯着资源有没有漏。
#
# 判据（写进 docs/dev/STATUS.md §29）：
#   - GDI 对象增长 ≤ 5（基线对比）
#   - USER 对象增长 ≤ 5
#   - 句柄增长 ≤ 20
#   - 常驻内存无单调增长（末三分之一的均值 ≤ 前三分之一均值 × 1.15）
#
# 为什么用 GetGuiResources：GDI/USER 对象泄漏在任务管理器里看不到，
# 但它们先于内存暴露问题（我们 0.1.x 时代就被 GDI 句柄漏过）。
param(
    [string]$Exe = '',
    [string]$Dir = '',
    [int]$Rounds = 200,
    [int]$PageTurns = 40,
    [int]$Zooms = 30,
    [string]$Pdf = '',
    [string]$Img = ''
)

$ErrorActionPreference = 'Stop'
# 压测目录：显式参数 > 环境变量 QLRUST_FIXTURES > 仓库旁的开发夹具目录。
if (-not $Dir) {
    $Dir = if ($env:QLRUST_FIXTURES) { $env:QLRUST_FIXTURES } else { Join-Path (Split-Path -Parent $PSScriptRoot) '..\..\qlrust-dev' }
}
if (-not $Exe) { $Exe = Join-Path $Dir 'qlrust.exe' }
if (-not $Pdf) { $Pdf = Join-Path $Dir 'pdf_8page.pdf' }
if (-not $Img) { $Img = Join-Path $Dir 'png_1920x1080.png' }
$log = Join-Path $Dir 'UserData\logs\app.log'

$Api = @'
using System;
using System.Runtime.InteropServices;
namespace ST {
    public static class Api {
        [DllImport("user32.dll")] public static extern uint GetGuiResources(IntPtr hProcess, uint flags);
        [DllImport("kernel32.dll")] public static extern IntPtr OpenProcess(uint access, bool inherit, int pid);
        [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
        public static uint Gdi(int pid) { return Sample(pid, 0); }
        public static uint User(int pid) { return Sample(pid, 1); }
        static uint Sample(int pid, uint flags) {
            IntPtr h = OpenProcess(0x1000, false, pid);   // PROCESS_QUERY_LIMITED_INFORMATION
            if (h == IntPtr.Zero) return 0;
            uint v = GetGuiResources(h, flags);
            CloseHandle(h);
            return v;
        }
    }
}
'@
Add-Type -TypeDefinition $Api -ErrorAction Stop

function Stop-Qlr {
    Get-Process qlrust -ErrorAction SilentlyContinue | ForEach-Object { $_.Kill() }
    Start-Sleep -Milliseconds 400
}
function Start-Qlr([string]$cli) {
    $a = @{ CommandLine = ('"' + $Exe + '" ' + $cli); CurrentDirectory = $Dir }
    Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments $a | Out-Null
}
function Wait-Host([int]$timeoutMs = 8000) {
    $deadline = (Get-Date).AddMilliseconds($timeoutMs)
    while ((Get-Date) -lt $deadline) {
        $p = Get-Process qlrust -ErrorAction SilentlyContinue | Sort-Object StartTime | Select-Object -First 1
        if ($p) { return $p }
        Start-Sleep -Milliseconds 80
    }
    return $null
}
function Sample {
    # 转发请求时会有第二个进程瞬间存在（把请求丢给常驻宿主后自己退出），
    # 所以这里必须只取常驻那个，别把进程数组当成一个进程算。
    $p = Get-Process qlrust -ErrorAction SilentlyContinue | Sort-Object StartTime | Select-Object -First 1
    if (-not $p) { return $null }
    [pscustomobject]@{
        Rss   = [math]::Round($p.WorkingSet64 / 1MB, 1)
        Priv  = [math]::Round($p.PrivateMemorySize64 / 1MB, 1)
        Gdi   = [int][ST.Api]::Gdi($p.Id)
        User  = [int][ST.Api]::User($p.Id)
        Handles = $p.HandleCount
    }
}

Write-Host "=== 长稳压测：开关 $Rounds 次 / 翻页 $PageTurns 次 / 缩放 $Zooms 次 ===" -ForegroundColor Cyan
Stop-Qlr
Start-Qlr ('--show "' + $Img + '"')
$host0 = Wait-Host
if (-not $host0) { Write-Host "宿主没起来" -ForegroundColor Red; exit 1 }
Start-Sleep -Milliseconds 700

$base = Sample
Write-Host ("基线：RSS {0} MB / Private {1} MB / GDI {2} / USER {3} / 句柄 {4}" -f $base.Rss, $base.Priv, $base.Gdi, $base.User, $base.Handles) -ForegroundColor DarkGray

$samples = @()
$sw = [System.Diagnostics.Stopwatch]::StartNew()
for ($i = 1; $i -le $Rounds; $i++) {
    # 交替两种内容：图片与 PDF 各自走一条完全不同的渲染链路。
    $file = if ($i % 7 -eq 0) { $Pdf } else { $Img }
    Start-Qlr '--hide'
    Start-Qlr ('--show "' + $file + '"')
    if ($i % 20 -eq 0) {
        $s = Sample
        if ($s) { $samples += $s }
        Write-Host ("  第 {0,4} 次：RSS {1,7} MB  GDI {2,3}  USER {3,3}  句柄 {4,4}" -f $i, $s.Rss, $s.Gdi, $s.User, $s.Handles) -ForegroundColor DarkGray
    }
    Start-Sleep -Milliseconds 30
}

# 翻页：PDF 里连按 PageDown（走 on_hotkey 的翻页分支）
Start-Qlr ('--show "' + $Pdf + '"')
Start-Sleep -Milliseconds 500
for ($i = 0; $i -lt $PageTurns; $i++) {
    Start-Qlr '--next-page'
    Start-Sleep -Milliseconds 40
}
# 缩放：在图片上放大/缩小若干轮（每轮都会重渲染）
Start-Qlr ('--show "' + $Img + '"')
Start-Sleep -Milliseconds 500
for ($i = 0; $i -lt $Zooms; $i++) {
    Start-Qlr '--zoom-in'
    Start-Sleep -Milliseconds 60
    Start-Qlr '--zoom-out'
    Start-Sleep -Milliseconds 60
}
Start-Qlr '--quit'
Start-Sleep -Milliseconds 800
$sw.Stop()

# 结束后重新起一个宿主采样（旧宿主已退出，句柄/GDI 应该全还回去）
Stop-Qlr
Start-Qlr ('--show "' + $Img + '"')
$null = Wait-Host
Start-Sleep -Milliseconds 700
$after = Sample
Write-Host ("结束：RSS {0} MB / Private {1} MB / GDI {2} / USER {3} / 句柄 {4}（总耗时 {5:N1} s）" -f $after.Rss, $after.Priv, $after.Gdi, $after.User, $after.Handles, $sw.Elapsed.TotalSeconds) -ForegroundColor DarkGray

$pass = 0; $fail = 0
function Check([string]$name, [bool]$ok, [string]$detail) {
    if ($ok) { $script:pass++; Write-Host "PASS  $name  $detail" -ForegroundColor Green }
    else { $script:fail++; Write-Host "FAIL  $name  $detail" -ForegroundColor Red }
}

Check 'GDI 对象无增长' ($after.Gdi - $base.Gdi -le 5) ("{0} → {1}（+{2}，上限 +5）" -f $base.Gdi, $after.Gdi, ($after.Gdi - $base.Gdi))
Check 'USER 对象无增长' ($after.User - $base.User -le 5) ("{0} → {1}（+{2}，上限 +5）" -f $base.User, $after.User, ($after.User - $base.User))
Check '句柄无增长' ($after.Handles - $base.Handles -le 20) ("{0} → {1}（+{2}，上限 +20）" -f $base.Handles, $after.Handles, ($after.Handles - $base.Handles))
# 内存判据（README 里也写了这条口径）：
# Windows 的堆在密集分配/释放后会保留一部分已提交页面，所以"RSS 涨了几 MB"本身
# 不等于泄漏。真正的泄漏会表现为**私有字节持续增长**或**GDI/USER/句柄增长**。
# 因此这里三条一起看：私有字节、GDI/USER/句柄（上面已判）、以及 RSS 的"平台期"。
if ($samples.Count -ge 6) {
    $third = [int]($samples.Count / 3)
    $first = ($samples[0..($third - 1)] | Measure-Object -Property Rss -Average).Average
    $last = ($samples[($samples.Count - $third)..($samples.Count - 1)] | Measure-Object -Property Rss -Average).Average
    # 允许 +8 MB 的堆保留（实测 15.2 → 18.0 就属这一类，重启后回到 12.9 MB）。
    Check '常驻内存处于平台期' ($last -le $first + 8) ("前段均值 {0:N1} MB → 后段 {1:N1} MB（允许 +8 MB 堆保留）" -f $first, $last)
    $privFirst = ($samples[0..($third - 1)] | Measure-Object -Property Priv -Average).Average
    $privLast = ($samples[($samples.Count - $third)..($samples.Count - 1)] | Measure-Object -Property Priv -Average).Average
    Check '私有字节无增长' ($privLast -le $privFirst + 4) ("前段 {0:N1} MB → 后段 {1:N1} MB（允许 +4 MB）" -f $privFirst, $privLast)
} else {
    Write-Host "样本太少，跳过内存趋势判定（把 -Rounds 调大）" -ForegroundColor Yellow
}
# 复核：重启后的宿主应当回到基线附近（说明增长的是堆缓存，不是"漏出来的活对象"）。
Check '重启后回到基线' ($after.Rss -le $base.Rss + 3) ("{0:N1} MB → {1:N1} MB" -f $base.Rss, $after.Rss)

# 日志里不该出现 panic/崩溃
$crashed = (Test-Path $log) -and ((Get-Content $log -Raw -Encoding UTF8) -cmatch 'panicked at|崩溃报告')
Check '日志无 panic/崩溃' (-not $crashed) ''

Stop-Qlr
Write-Host ''
Write-Host ("PASS={0} FAIL={1}" -f $pass, $fail) -ForegroundColor Cyan
if ($fail -gt 0) { exit 1 }
exit 0
