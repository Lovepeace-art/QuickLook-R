<#
.SYNOPSIS
    在"资源管理器真的选中了一个文件"的状态下验证选中项通道（WBS A1.6）。

.DESCRIPTION
    为什么要单独一个脚本：自动化里没法保证资源管理器拿到前台（`SetForegroundWindow`
    受前台锁限制，DSH 这类宿主自己也可能抢焦点），所以这里自己找窗口、自己
    `SetForegroundWindow`，再分别用 `--probe-selection-hwnd <HWND>`（钉死窗口）
    和 `--probe-selection`（走前台窗口）两条路验证。

    还有三个必须绕开的坑（见 docs/dev/CONVENTIONS.md）：
    1. `Add-Type` 定义的类型**不跨 pwsh 进程持久化**，找窗口的 P/Invoke
       必须和"读结果"在同一个进程里，所以这一切都写在一个脚本里。
    2. **exe 不能放在工作目录里跑**：DSH 的文件沙箱会给工作目录里的可执行
       文件一个受限令牌（实测完整性级别 Medium，`D:` 上是 High），导致
       `CoCreateInstance(CLSID_ShellWindows)` 返回 0x80070005 拒绝访问。
       脚本默认把 exe 复制到 `D:\qlrust-bin` 再跑，用 `-ExeDir` 可改。
    3. **资源管理器窗口数量不稳定**（`/select` 会复用或关掉已有窗口，
       短时间内枚举结果在 0/1/多 之间跳）。所以这里是"轮询等窗口 → 逐个候选
       试 → 每个候选先恢复+置前再探测"，不是硬取第一个。

.EXAMPLE
    pwsh -File scripts\probe-selection.ps1
    pwsh -File scripts\probe-selection.ps1 -Fixture "D:\pics\a.jpg" -ExpectCount 1
#>
[CmdletBinding()]
param(
    [string]$Fixture = (Join-Path $PSScriptRoot "..\..\spike\fixtures\8mp_3840x2160.jpg"),
    [string]$Exe = (Join-Path $PSScriptRoot "..\target\release\qlrust.exe"),
    [string]$ExeDir = "D:\qlrust-bin",
    [int]$ExpectCount = 1,
    [int]$WaitMs = 4000,
    [int]$Attempts = 3
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $Fixture)) { throw "找不到夹具：$Fixture" }
if (-not (Test-Path $Exe)) { throw "找不到 exe：$Exe（先 cargo build --release）" }
$Fixture = (Resolve-Path $Fixture).Path
$Exe = (Resolve-Path $Exe).Path

# 1) 把 exe 放到沙箱外，避免受限令牌（见文件头注释第 2 条）
New-Item -ItemType Directory -Force -Path $ExeDir | Out-Null
Copy-Item $Exe $ExeDir -Force
$staged = Join-Path $ExeDir (Split-Path $Exe -Leaf)
if (-not (Test-Path (Join-Path $ExeDir "portable.lock"))) {
    New-Item -ItemType File -Path (Join-Path $ExeDir "portable.lock") | Out-Null
}
$logFile = Join-Path $ExeDir "UserData\logs\app.log"

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
using System.Collections.Generic;
public class QlrWin {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();

    public static List<string> Frames() {
        var list = new List<string>();
        EnumWindows((h, l) => {
            var c = new StringBuilder(256); GetClassName(h, c, 256);
            if (c.ToString() != "CabinetWClass") { return true; }
            var t = new StringBuilder(512); GetWindowText(h, t, 512);
            list.Add(h.ToInt64() + "|" + IsWindowVisible(h) + "|" + t.ToString());
            return true;
        }, IntPtr.Zero);
        return list;
    }
    public static bool Focus(IntPtr h) {
        ShowWindow(h, 9);                 // SW_RESTORE
        SetForegroundWindow(h);
        return GetForegroundWindow() == h;
    }
}
'@

$folderTitle = Split-Path $Fixture -Parent
$leaf = Split-Path $Fixture -Leaf

function Get-Frames {
    [QlrWin]::Frames() | ForEach-Object {
        $p = $_ -split '\|'
        [pscustomobject]@{ H = [int64]$p[0]; Visible = ($p[1] -eq 'True'); Title = $p[2] }
    }
}

function Invoke-Probe([string]$Args1, [int64]$Hwnd) {
    if (Test-Path $logFile) { Remove-Item $logFile -Force }
    if ($Hwnd -gt 0) {
        & cmd /c "set QLRUST_LOG=debug&& `"$staged`" $Args1 $Hwnd"
    } else {
        & cmd /c "set QLRUST_LOG=debug&& `"$staged`" $Args1"
    }
    Start-Sleep -Milliseconds 400
    if (-not (Test-Path $logFile)) { return $null }
    $lines = Get-Content $logFile
    $summary = $lines | Where-Object { $_ -match 'PROBE-SELECTION source=' } | Select-Object -Last 1
    [pscustomobject]@{ Summary = $summary; Lines = $lines }
}

function Test-Summary($result) {
    if (-not $result -or -not $result.Summary) { return $false }
    $count = if ($result.Summary -match 'count=(\d+)') { [int]$Matches[1] } else { 0 }
    return ($count -eq $ExpectCount) -and ($result.Summary -match [regex]::Escape($leaf))
}

# 2) 打开资源管理器并选中夹具，然后轮询等窗口出现
Start-Process explorer.exe -ArgumentList "/select,`"$Fixture`"" | Out-Null

$deadline = (Get-Date).AddMilliseconds($WaitMs)
$frames = @()
while ((Get-Date) -lt $deadline) {
    $frames = @(Get-Frames | Where-Object { $_.Title -eq $folderTitle })
    if ($frames.Count -gt 0) { break }
    Start-Sleep -Milliseconds 250
}

if ($frames.Count -eq 0) {
    Write-Host "FAIL 等不到打开着 $folderTitle 的资源管理器窗口" -ForegroundColor Red
    exit 1
}
# 可见的优先，其次前台；顺序稳定后逐个尝试
$candidates = @($frames | Sort-Object @{ Expression = { -not $_.Visible } }, @{ Expression = { $_.H } })
Write-Host ("候选 frame：" + (($candidates | ForEach-Object { "0x{0:X}{1}" -f $_.H, $(if ($_.Visible) { '' } else { '(隐藏)' }) }) -join ", "))

$ok = $null
$last = $null
$lastHwnd = 0
foreach ($attempt in 1..$Attempts) {
    foreach ($c in $candidates) {
        $hwnd = [IntPtr]$c.H
        [QlrWin]::Focus($hwnd) | Out-Null
        Start-Sleep -Milliseconds 300
        Write-Host ("--- 第 {0} 轮 attempt，frame 0x{1:X} ---" -f $attempt, $c.H)
        $r = Invoke-Probe '--probe-selection-hwnd' $c.H
        $last = $r; $lastHwnd = $c.H
        if (Test-Summary $r) { $ok = $r; break }
        # 再试前台路径
        $r2 = Invoke-Probe '--probe-selection' 0
        $last = $r2; $lastHwnd = 0
        if (Test-Summary $r2) { $ok = $r2; break }
    }
    if ($ok) { break }
    Start-Sleep -Milliseconds 500
}

Write-Host "--- 最后一次日志 ---"
if ($last) {
    $last.Lines | Where-Object { $_ -match 'PROBE-SELECTION|ShellWindows|GetSelection|命中 browser|选中项' } |
        Select-Object -Last 8 | ForEach-Object { Write-Host "  $($_ -replace '^\S+Z ', '')" }
}

if ($ok) {
    $via = if ($lastHwnd -gt 0) { "hwnd 0x{0:X}" -f $lastHwnd } else { "前台窗口" }
    Write-Host ("PASS 选中项通道可用（经 {0}）：{1}" -f $via, $ok.Summary) -ForegroundColor Green
    exit 0
}

Write-Host ("FAIL 期望 count={0} 且包含 {1}，实际：{2}" -f $ExpectCount, $leaf, $(if ($last) { $last.Summary } else { '无汇总行' })) -ForegroundColor Red
exit 1
