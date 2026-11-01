# QLRust 冒烟测试（W1–W2 验收脚本）
#
# 验证四件事：
#   1. 冷启动能起来，日志写到了正确位置；
#   2. 第二个进程能把请求转交给常驻实例并自行退出（单实例 + WM_COPYDATA）；
#   3. 预览窗口真的出现（按窗口类名枚举，且 IsWindowVisible 为真）；
#   4. --quit 能让常驻实例干净退出。
#
# 用法：pwsh -File scripts/smoke.ps1                      （用 debug 构建）
#       pwsh -File scripts/smoke.ps1 -Exe target\release\qlrust.exe
# 退出码：0 = 全过；1 = 有失败项。

param(
    [string]$Exe = "target\debug\qlrust.exe",
    [string]$Fixture = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location (Join-Path $root ".")

if (-not (Test-Path $Exe)) { throw "找不到可执行体：$Exe（先 cargo build）" }
$Exe = (Resolve-Path $Exe).Path
$exeDir = Split-Path -Parent $Exe

# 便携模式：数据写在 exe 同目录，测试结束可以整体删掉，不污染 %LOCALAPPDATA%。
$portableLock = Join-Path $exeDir "portable.lock"
if (-not (Test-Path $portableLock)) { New-Item -ItemType File -Path $portableLock | Out-Null }
$logFile = Join-Path $exeDir "UserData\logs\app.log"
if (Test-Path $logFile) { Remove-Item $logFile -Force }

if (-not $Fixture) {
    # 夹具搜索顺序：仓库内 → 同级 spike 目录（本仓库的夹具放在上级）→ 现场生成。
    # 绝不退化成"用脚本自身当夹具"——那会让渲染断言永远失败（.ps1 走不到图片渲染器）。
    $candidates = @(
        (Join-Path $root "spike\fixtures\8mp_3840x2160.jpg"),
        (Join-Path (Split-Path -Parent $root) "spike\fixtures\8mp_3840x2160.jpg")
    )
    foreach ($c in $candidates) {
        if (Test-Path $c) { $Fixture = (Resolve-Path $c).Path; break }
    }
    if (-not $Fixture) {
        $genDir = Join-Path $exeDir "fixtures"
        if (-not (Test-Path $genDir)) { New-Item -ItemType Directory -Path $genDir -Force | Out-Null }
        $Fixture = Join-Path $genDir "qlrust-smoke.jpg"
        if (-not (Test-Path $Fixture)) {
            Add-Type -AssemblyName System.Drawing
            $bmp = New-Object System.Drawing.Bitmap 1200, 800
            $g = [System.Drawing.Graphics]::FromImage($bmp)
            $brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
                (New-Object System.Drawing.Point 0, 0),
                (New-Object System.Drawing.Point 1200, 800),
                [System.Drawing.Color]::FromArgb(255, 30, 60, 120),
                [System.Drawing.Color]::FromArgb(255, 200, 90, 40))
            $g.FillRectangle($brush, 0, 0, 1200, 800)
            $font = New-Object System.Drawing.Font("Segoe UI", 48)
            $g.DrawString("QLRust smoke fixture", $font, [System.Drawing.Brushes]::White, 120, 360)
            $g.Dispose()
            $bmp.Save($Fixture, [System.Drawing.Imaging.ImageFormat]::Jpeg)
            $bmp.Dispose()
        }
    }
}
Write-Output ("夹具：{0}" -f $Fixture)

Add-Type -Namespace Qlr -Name Win -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr p);
public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr hWnd, System.Text.StringBuilder name, int max);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
[DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
public struct RECT { public int Left, Top, Right, Bottom; }
'@

function Get-QlrWindows {
    param([int]$ProcessId)
    $found = [System.Collections.Generic.List[object]]::new()
    $callback = {
        param([IntPtr]$hwnd, [IntPtr]$lparam)
        $sb = [System.Text.StringBuilder]::new(256)
        [void][Qlr.Win]::GetClassName($hwnd, $sb, 256)
        $class = $sb.ToString()
        if ($class -like "QLRust.*") {
            $owner = 0
            [void][Qlr.Win]::GetWindowThreadProcessId($hwnd, [ref]$owner)
            if ($ProcessId -eq 0 -or $owner -eq $ProcessId) {
                $rect = New-Object Qlr.Win+RECT
                [void][Qlr.Win]::GetWindowRect($hwnd, [ref]$rect)
                $found.Add([pscustomobject]@{
                    Class   = $class
                    Hwnd    = $hwnd
                    Pid     = $owner
                    Visible = [Qlr.Win]::IsWindowVisible($hwnd)
                    Size    = "$($rect.Right - $rect.Left)x$($rect.Bottom - $rect.Top)"
                })
            }
        }
        return $true
    }
    [void][Qlr.Win]::EnumWindows($callback, [IntPtr]::Zero)
    return $found
}

$results = [System.Collections.Generic.List[object]]::new()
function Check {
    param([string]$Name, [bool]$Ok, [string]$Detail = "")
    $results.Add([pscustomobject]@{ 检查 = $Name; 结果 = $(if ($Ok) { "PASS" } else { "FAIL" }); 细节 = $Detail })
}
function Skip {
    param([string]$Name, [string]$Why = "")
    $results.Add([pscustomobject]@{ 检查 = $Name; 结果 = "SKIP"; 细节 = $Why })
}

# 为什么不用 Start-Process：它让子进程继承当前 shell 的 stdio 句柄，在非交互环境
# （CI、agent harness、被捕获输出的终端）里调用方会一直挂着等管道关闭，看起来像卡死。
# 用 WMI 的 Win32_Process::Create 启动的进程完全脱离父进程，测出来的也是"干净启动"。
function Start-Qlr {
    param([string]$File, [string[]]$ArgList = @())
    $quoted = ($ArgList | ForEach-Object { '"' + $_ + '"' }) -join " "
    $cmdLine = '"' + $File + '"'
    if ($quoted) { $cmdLine = $cmdLine + " " + $quoted }
    $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $cmdLine }
    if ($r.ReturnValue -ne 0) { throw "WMI 创建进程失败，ReturnValue=$($r.ReturnValue)" }
    return [int]$r.ProcessId
}

function Wait-QlrExit {
    param([int]$ProcessId, [int]$TimeoutMs = 5000)
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $TimeoutMs) {
        if (-not (Get-Process -Id $ProcessId -ErrorAction SilentlyContinue)) { return $true }
        Start-Sleep -Milliseconds 25
    }
    return $false
}

# 管理员身份运行时，explorer 是非提权的，UIPI 会拦掉 Shell_NotifyIconW（0x80070005）。
# 这是环境限制而不是代码缺陷，所以托盘项降级为 SKIP 并说明原因。
$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

# ---- 0. 清理残留 ----
Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 200

# ---- 1. 冷启动 ----
$coldStart = [System.Diagnostics.Stopwatch]::StartNew()
$hostPid = Start-Qlr -File $Exe
$deadline = (Get-Date).AddSeconds(5)
while ((Get-Date) -lt $deadline) {
    if (Test-Path $logFile) {
        $text = Get-Content $logFile -Raw -ErrorAction SilentlyContinue
        if ($text -and $text -match "壳层就绪") { break }
    }
    Start-Sleep -Milliseconds 20
}
$coldStart.Stop()
Check "冷启动并进入壳层" (Test-Path $logFile) ("{0} ms" -f $coldStart.ElapsedMilliseconds)

$trayWarn = $false
if (Test-Path $logFile) { $trayWarn = ((Get-Content $logFile -Raw) -match "托盘图标创建失败") }

# ---- 2. 第二个实例转交请求 ----
$forwardStart = [System.Diagnostics.Stopwatch]::StartNew()
$secondPid = Start-Qlr -File $Exe -ArgList @($Fixture)
$secondExited = Wait-QlrExit -ProcessId $secondPid -TimeoutMs 5000
$forwardStart.Stop()
Check "第二实例自行退出" $secondExited ("{0} ms" -f $forwardStart.ElapsedMilliseconds)

$text = if (Test-Path $logFile) { ((Get-Content $logFile -ErrorAction SilentlyContinue) -join "`n") } else { "" }
Check "收到转交的请求" ($text -match "请求已转交")
Check "常驻实例处理了 SHOW" ($text -match "SHOW target=")

# ---- 3. 预览窗口真的出现 ----
$previewVisible = $false
$previewSize = ""
$deadline = (Get-Date).AddSeconds(3)
while ((Get-Date) -lt $deadline) {
    $wins = Get-QlrWindows -ProcessId $hostPid | Where-Object { $_.Class -eq "QLRust.PreviewWindow" -and $_.Visible }
    if ($wins) {
        $previewVisible = $true
        $previewSize = ($wins | Select-Object -First 1).Size
        break
    }
    Start-Sleep -Milliseconds 50
}
Check "预览窗口可见" $previewVisible $previewSize

if ($trayWarn -and $elevated) {
    Skip "托盘图标已创建" "本会话以管理员身份运行，explorer 非提权 → Shell_NotifyIconW 被 UIPI 拒绝（0x80070005），非代码问题"
} else {
    Check "托盘图标已创建" ($trayWarn -eq $false) $(if ($trayWarn) { "日志里有托盘失败警告" } else { "" })
}

# ---- 3.5 全局热键（A1.5）----
# 钩子会过滤 SendInput 合成出来的按键（防自动化误触），所以自动化里没法"真按空格"，
# 这里分两段验证：钩子装上了（日志）+ 钩子命中后的处理链可用（--simulate-hotkey 直投消息）。
$text = if (Test-Path $logFile) { ((Get-Content $logFile -ErrorAction SilentlyContinue) -join "`n") } else { "" }
Check "全局热键已安装" ($text -match "键盘钩子已安装") $(if ($text -match "键盘钩子已安装") { "vk=Space" } else { "日志里没有安装记录" })
Check "鼠标钩子已安装" ($text -match "鼠标钩子已安装=true") $(if ($text -match "鼠标钩子已安装=true") { "滚轮缩放可用" } else { "滚轮不可用（不影响其它功能）" })

Start-Qlr -File $Exe -ArgList @("--simulate-hotkey") | Out-Null
Start-Sleep -Milliseconds 500
$text = if (Test-Path $logFile) { ((Get-Content $logFile -ErrorAction SilentlyContinue) -join "`n") } else { "" }
Check "热键处理链可用" ($text -match "HOTKEY vk=0x20")

# 滚轮缩放：--wheel 直投 Action::Wheel（真滚轮走鼠标钩子，同样被注入过滤挡掉）
& $Exe --wheel 120 2>&1 | Out-Null
Start-Sleep -Milliseconds 500
$text = if (Test-Path $logFile) { ((Get-Content $logFile -ErrorAction SilentlyContinue) -join "`n") } else { "" }
Check "滚轮缩放已生效" ($text -match "ZOOM 1\.00 -> 1\.25") $(if ($text -match "ZOOM 1\.00 -> 1\.25") { "1.00 -> 1.25" } else { "日志里没有 ZOOM 记录" })

# ---- 3.6 选中项通道（A1.6）----
# 独立进程跑一次就退：验证 COM 初始化 + Shell 爬取不崩（有没有选中项取决于当前桌面状态）。
$probePid = Start-Qlr -File $Exe -ArgList @("--probe-selection")
$probeExited = Wait-QlrExit -ProcessId $probePid -TimeoutMs 5000
Start-Sleep -Milliseconds 300
$text = if (Test-Path $logFile) { ((Get-Content $logFile -ErrorAction SilentlyContinue) -join "`n") } else { "" }
$probeLine = ($text -split "`n" | Where-Object { $_ -match "PROBE-SELECTION source=" } | Select-Object -Last 1)
Check "选中项探测可运行" ($probeExited -and [bool]$probeLine) $(if ($probeLine) { $probeLine.Trim() } else { "无输出或进程未退出" })

# ---- 3.7 渲染管线（M2 A2.1/A2.2）----
# 夹具是 JPEG，走图片渲染器：期望 worker 线程上出现 "RENDER gen=… bitmap bytes=…"。
# 这里只断言"渲染真的发生了、而且没报错"，不比对像素（像素比对留给 bench 脚本）。
$renderText = if (Test-Path $logFile) { Get-Content $logFile -Raw } else { "" }
$renderLine = ($renderText -split "`n" | Where-Object { $_ -match "RENDER gen=\d+ bitmap" } | Select-Object -Last 1)
Check "图片渲染完成" ([bool]$renderLine) $(if ($renderLine) { ($renderLine.Trim() -replace '^.*RENDER ', 'RENDER ') } else { "日志里没有 RENDER bitmap" })
$renderFail = ($renderText -split "`n" | Where-Object { $_ -match "RENDER gen=\d+ 失败" } | Select-Object -Last 1)
Check "渲染无失败" (-not $renderFail) $(if ($renderFail) { $renderFail.Trim() } else { "" })

# ---- 4. --quit ----
$quitStart = [System.Diagnostics.Stopwatch]::StartNew()
Start-Qlr -File $Exe -ArgList @("--quit") | Out-Null
$hostExited = Wait-QlrExit -ProcessId $hostPid -TimeoutMs 5000
$quitStart.Stop()
Check "常驻实例按请求退出" $hostExited ("{0} ms" -f $quitStart.ElapsedMilliseconds)

$text = if (Test-Path $logFile) { ((Get-Content $logFile -ErrorAction SilentlyContinue) -join "`n") } else { "" }
Check "退出时移除了托盘图标" ($text -match "退出：总运行")
Check "退出时卸载了热键钩子" ($text -match "键盘钩子已卸载")
Check "热键统计已记录" ($text -match "热键统计")

# ---- 汇总 ----
Write-Output ""
$results | Format-Table -AutoSize
$failed = $results | Where-Object { $_.结果 -eq "FAIL" }
Write-Output ("日志：{0}" -f $logFile)
if ($failed) {
    Write-Output ""
    Write-Output "失败项："
    $failed | Format-Table -AutoSize
    Write-Output "---- 日志尾部 ----"
    if (Test-Path $logFile) { Get-Content $logFile -Tail 40 }
    exit 1
}
Write-Output "全部通过。"
exit 0




