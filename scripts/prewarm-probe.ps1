# 预热命中率与首帧延迟探针（修「预热永远打不中」前后各跑一遍）
#
# 为什么这么绕：
#   1) 预热的目标是**资源管理器当前选中的那个文件**（`selection::current_selection()` 走前台
#      窗口），所以必须真的开一个 Explorer 窗口选中文件、并让它保持前台；
#   2) `show_preview` 先查「上一帧缓存」（LASTFRAME），同一个文件连开两次会命中 LASTFRAME
#      而轮不到预热槽 —— 所以每次尝试都是「先开 A（占位）→ 收起 → 改选 B → 等预热 → 再开 B」；
#   3) 日志里同一批 pattern 会出现在占位文件上，所以所有数字都必须取「本次动作之后的新行」。
#
# 用法：
#   pwsh -NoProfile -File scripts/prewarm-probe.ps1 -ExeDir <dir> -Tag before -Runs 5
# 输出：控制台表 + scripts\prewarm-<Tag>.json

param(
    [Parameter(Mandatory = $true)][string]$ExeDir,
    [string]$Tag = "run",
    [int]$Runs = 5,
    [string]$Fixtures = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
if (-not $Fixtures) {
    if ($env:QLRUST_FIXTURES) { $Fixtures = $env:QLRUST_FIXTURES }
    else { $Fixtures = Join-Path ([Environment]::GetFolderPath('Desktop')) 'qlrust-dev' }
}
$ExeDir = (Resolve-Path $ExeDir).Path
$exe = Join-Path $ExeDir "qlrust.exe"
$cfg = Join-Path $ExeDir "UserData\config.json"
$log = Join-Path $ExeDir "UserData\logs\app.log"

Add-Type -Namespace QlrP -Name W -MemberDefinition @'
public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr p);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr hWnd, System.Text.StringBuilder name, int max);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
[DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int cmd);
[DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr hWnd, System.Text.StringBuilder name, int max);
[DllImport("user32.dll")] public static extern bool AttachThreadInput(uint idAttach, uint idAttachTo, bool attach);
    [DllImport("user32.dll")] public static extern void keybd_event(byte bVk, byte bScan, uint dwFlags, UIntPtr dwExtraInfo);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, IntPtr pid);
[DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
'@

function Kill-Qlr {
    Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    for ($i = 0; $i -lt 100; $i++) {
        if (-not (Get-Process qlrust -ErrorAction SilentlyContinue)) { break }
        Start-Sleep -Milliseconds 20
    }
    Start-Sleep -Milliseconds 700
}

function Start-Qlr {
    param([string[]]$ArgList = @(), [hashtable]$Env = $null)
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $exe
    $psi.WorkingDirectory = $ExeDir
    $psi.UseShellExecute = $false
    foreach ($a in $ArgList) { [void]$psi.ArgumentList.Add($a) }
    if ($Env) { foreach ($kv in $Env.GetEnumerator()) { $psi.EnvironmentVariables[$kv.Key] = [string]$kv.Value } }
    return [System.Diagnostics.Process]::Start($psi)
}

function Get-Text { if (-not (Test-Path $log)) { return "" }; ((Get-Content $log -ErrorAction SilentlyContinue) -join "`n") }
function Get-Mark([string]$Pattern) { ([regex]::Matches((Get-Text), [regex]::Escape($Pattern))).Count }
function Get-Lines([string]$Pattern) {
    if (-not (Test-Path $log)) { return @() }
    @(Select-String -Path $log -Pattern $Pattern -ErrorAction SilentlyContinue | ForEach-Object { $_.Line })
}
function Get-LinesSince([string]$Pattern, [int]$Mark) {
    $a = @(Get-Lines $Pattern)
    if ($a.Count -le $Mark) { return @() }
    return @($a[$Mark..($a.Count - 1)])
}
function Test-Log([string]$Pattern) { (Get-Text) -match [regex]::Escape($Pattern) }

function Wait-Log([string]$Pattern, [int]$TimeoutMs = 6000) {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $TimeoutMs) {
        if (Test-Log $Pattern) { return $true }
        Start-Sleep -Milliseconds 20
    }
    return $false
}

# 等"新出现的匹配行里含有 $Needle"
function Wait-LogSince([string]$Pattern, [int]$Mark, [string]$Needle, [int]$TimeoutMs = 8000) {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $TimeoutMs) {
        foreach ($l in (Get-LinesSince $Pattern $Mark)) { if ($l.Contains($Needle)) { return $true } }
        Start-Sleep -Milliseconds 20
    }
    return $false
}

# 把窗口钉到前台。两种办法都试：
#  1) 先按一下 Alt（`keybd_event`）"解锁"前台锁，再 `SetForegroundWindow` —— 这是标准做法，
#     实测 DSH/Chrome 抢焦点时也能生效；
#  2) 兜底 `AttachThreadInput`（对某些窗口更硬）。
# 只靠裸 `SetForegroundWindow` 会被前台锁静默拒绝（脚本里踩过）。
function Force-Foreground {
    param([IntPtr]$Hwnd)
    if ($Hwnd -eq [IntPtr]::Zero) { return $false }
    [void][QlrP.W]::ShowWindow($Hwnd, 9)   # SW_RESTORE
    [QlrP.W]::keybd_event(0x12, 0, 0, [UIntPtr]::Zero)
    [QlrP.W]::keybd_event(0x12, 0, 2, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 60
    [void][QlrP.W]::SetForegroundWindow($Hwnd)
    Start-Sleep -Milliseconds 120
    if ([QlrP.W]::GetForegroundWindow() -eq $Hwnd) { return $true }
    $fg = [QlrP.W]::GetForegroundWindow()
    $fgT = [QlrP.W]::GetWindowThreadProcessId($fg, [IntPtr]::Zero)
    $myT = [QlrP.W]::GetCurrentThreadId()
    if ($fgT -ne $myT) {
        [void][QlrP.W]::AttachThreadInput($myT, $fgT, $true)
        [void][QlrP.W]::SetForegroundWindow($Hwnd)
        [void][QlrP.W]::AttachThreadInput($myT, $fgT, $false)
    }
    Start-Sleep -Milliseconds 120
    return ([QlrP.W]::GetForegroundWindow() -eq $Hwnd)
}

# 找"标题 = 目标文件夹"的资源管理器窗口（同目录可能开着好几个）
function Find-Explorer {
    param([string]$Folder)
    $script:__cands = @()
    $cb = {
        param([IntPtr]$h, [IntPtr]$l)
        $sb = [System.Text.StringBuilder]::new(256)
        [void][QlrP.W]::GetClassName($h, $sb, 256)
        if ($sb.ToString() -ne "CabinetWClass") { return $true }
        if (-not [QlrP.W]::IsWindowVisible($h)) { return $true }
        $s2 = [System.Text.StringBuilder]::new(512)
        [void][QlrP.W]::GetWindowText($h, $s2, 512)
        $script:__cands += [pscustomobject]@{ Hwnd = $h; Title = $s2.ToString() }
        return $true
    }
    [void][QlrP.W]::EnumWindows($cb, [IntPtr]::Zero)
    $match = @($script:__cands | Where-Object { $_.Title -eq $Folder })
    if ($match.Count -eq 0) { return [IntPtr]::Zero }
    $fg = [QlrP.W]::GetForegroundWindow()
    foreach ($m in $match) { if ($m.Hwnd -eq $fg) { return $m.Hwnd } }
    return $match[$match.Count - 1].Hwnd
}

function Ensure-Foreground {
    param([IntPtr]$Hwnd, [int]$Tries = 4)
    for ($i = 0; $i -lt $Tries; $i++) {
        if ([QlrP.W]::GetForegroundWindow() -eq $Hwnd) { return $true }
        if (Force-Foreground -Hwnd $Hwnd) { return $true }
        Start-Sleep -Milliseconds 100
    }
    return $false
}

# `/select` 会让资源管理器**自己**激活那个窗口（这是最可靠的一次激活）；
# 所以优先用"select 之后的前台窗口"，只有在它不匹配时才去枚举里找。
function Select-File {
    param([string]$File)
    $folder = Split-Path -Parent $File
    explorer.exe "/select,$File" | Out-Null
    Start-Sleep -Milliseconds 900
    $fg = [QlrP.W]::GetForegroundWindow()
    $cls = [System.Text.StringBuilder]::new(256)
    [void][QlrP.W]::GetClassName($fg, $cls, 256)
    $hwnd = if ($cls.ToString() -eq "CabinetWClass") { $fg } else { Find-Explorer -Folder $folder }
    if ($hwnd -eq [IntPtr]::Zero) { return [IntPtr]::Zero }
    [void](Ensure-Foreground -Hwnd $hwnd)
    return $hwnd
}
function Get-LineTime([string]$Line) {
    if ($Line -match '^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3})Z') {
        return [datetime]::ParseExact($Matches[1], 'yyyy-MM-ddTHH:mm:ss.fff', $null)
    }
    return $null
}

# ---- config：备份 → prewarm=always → 测完恢复 ----
if (-not (Test-Path $cfg)) { throw "没有 config.json（先裸启一次再退出，让它落盘）：$cfg" }
$backup = Join-Path $PSScriptRoot ("prewarm-config-backup-" + $Tag + ".json")
Copy-Item $cfg $backup -Force
$json = Get-Content $cfg -Raw | ConvertFrom-Json
$json.prewarm = "always"
($json | ConvertTo-Json -Depth 8) | Set-Content $cfg -Encoding UTF8

$warmupFile = Join-Path $Fixtures "demo.md"
$targets = @(
    @{ key = "pdf"; file = Join-Path $Fixtures "pdf_8page.pdf" },
    @{ key = "48mp"; file = Join-Path $Fixtures "48mp_8000x6000.png" },
    @{ key = "png1920"; file = Join-Path $Fixtures "png_1920x1080.png" },
    @{ key = "jpg8mp"; file = Join-Path $Fixtures "8mp_3840x2160.jpg" }
)

$rows = @()
try {
    foreach ($t in $targets) {
        $leaf = Split-Path -Leaf $t.file
        for ($i = 1; $i -le $Runs; $i++) {
            Kill-Qlr
            if (Test-Path $log) { Remove-Item $log -Force -ErrorAction SilentlyContinue }
            $null = Start-Qlr
            if (-not (Wait-Log -Pattern "热键就绪" -TimeoutMs 12000)) { throw "宿主没起来" }

            # 1) 先开占位文件（把上一帧缓存换成它，保证目标文件不会命中 LASTFRAME）
            $exp = Select-File -File $warmupFile
            $mShow = Get-Mark "SHOW target="
            [void](Ensure-Foreground -Hwnd $exp)
            $q = Start-Qlr -ArgList @("--simulate-hotkey"); $null = $q.WaitForExit(6000)
            $null = Wait-LogSince "SHOW target=" $mShow "demo.md" 6000
            Start-Sleep -Milliseconds 400
            $mHide = Get-Mark "HIDE elapsed="
            [void](Ensure-Foreground -Hwnd $exp)
            $q = Start-Qlr -ArgList @("--simulate-hotkey"); $null = $q.WaitForExit(6000)
            $swHide = [System.Diagnostics.Stopwatch]::StartNew()
            while ((Get-Mark "HIDE elapsed=") -le $mHide -and $swHide.ElapsedMilliseconds -lt 5000) { Start-Sleep -Milliseconds 20 }
            Start-Sleep -Milliseconds 200

            # 2) 改选目标文件 → 窗口收着 → 让预热跑起来
            #    这里**不按行号基线**判断：`/select` 一执行选中项就变了，250 ms 定时器可能
            #    立刻预热，而 `Select-File` 自己要花 ~1 s；基线若取在它返回之后，就会把
            #    "新"日志吃掉（踩过一次，命中率恒为 0 的假象）。每个迭代都删过日志，
            #    所以整份日志只属于这一次 → 直接全文查。
            $exp = Select-File -File $t.file
            $swPw = [System.Diagnostics.Stopwatch]::StartNew()
            while ($swPw.ElapsedMilliseconds -lt 12000) {
                [void](Ensure-Foreground -Hwnd $exp)
                if (@(Get-Lines "PREWARM 就绪" | Where-Object { $_.Contains($leaf) }).Count -gt 0) { break }
                Start-Sleep -Milliseconds 150
            }

            # 3) 按空格打开目标文件
            [void](Ensure-Foreground -Hwnd $exp)
            $q = Start-Qlr -ArgList @("--simulate-hotkey"); $null = $q.WaitForExit(6000)
            $swShow = [System.Diagnostics.Stopwatch]::StartNew()
            while ($swShow.ElapsedMilliseconds -lt 6000) {
                if (@(Get-Lines "SHOW target=" | Where-Object { $_.Contains($leaf) }).Count -gt 0) { break }
                Start-Sleep -Milliseconds 20
            }
            Start-Sleep -Milliseconds 800

            $pwReadyLines = @(Get-Lines "PREWARM 就绪" | Where-Object { $_.Contains($leaf) })
            $hitLines = @(Get-Lines "PREWARM 命中" | Where-Object { $_.Contains($leaf) })
            $showLines = @(Get-Lines "SHOW target=" | Where-Object { $_.Contains($leaf) })
            $renderLines = @(Get-Lines "RENDER gen=")
            $hotLines = @(Get-Lines "HOTKEY vk=")

            $hit = ($hitLines.Count -gt 0)
            $pwReadyOk = ($pwReadyLines.Count -gt 0)
            $showLine = if ($showLines.Count -gt 0) { $showLines[$showLines.Count - 1] } else { "" }
            $renderLine = if (-not $hit -and $renderLines.Count -gt 0) { $renderLines[$renderLines.Count - 1] } else { "" }
            $hotLine = if ($hotLines.Count -gt 0) { $hotLines[$hotLines.Count - 1] } else { "" }
            $pwReadyLine = if ($pwReadyLines.Count -gt 0) { $pwReadyLines[$pwReadyLines.Count - 1] } else { "" }

            $hotT = Get-LineTime $hotLine
            $showT = Get-LineTime $showLine
            $renderT = Get-LineTime $renderLine
            $contentMs = -1
            if ($hotT -and $hit -and $showT) { $contentMs = [int](($showT - $hotT).TotalMilliseconds) }
            elseif ($hotT -and $renderT) { $contentMs = [int](($renderT - $hotT).TotalMilliseconds) }

            $showElapsed = -1; if ($showLine -match 'elapsed=(\d+)ms') { $showElapsed = [int]$Matches[1] }
            $renderElapsed = -1; if ($renderLine -match 'elapsed=(\d+)ms') { $renderElapsed = [int]$Matches[1] }
            $pwReadySize = ""
            if ($pwReadyLine -match '\s(\d+x\d+)\s') { $pwReadySize = $Matches[1] }
            $reqSize = ""; if ($renderLine -match '请求 (\d+x\d+)') { $reqSize = $Matches[1] }

            $rows += [pscustomobject]@{
                夹具         = $t.key
                轮           = $i
                预热就绪     = $pwReadyOk
                命中         = $hit
                就绪尺寸     = $pwReadySize
                渲染请求尺寸 = $reqSize
                SHOWelapsed  = $showElapsed
                内容可见ms   = $contentMs
                RENDERms     = $renderElapsed
                前台类       = $(if ($hotLine -match 'fg=(\S+)') { $Matches[1] } else { "-" })
            }
            Kill-Qlr
        }
    }
}
finally {
    Kill-Qlr
    Copy-Item $backup $cfg -Force
}

Write-Host ""
Write-Host ("==== 预热探针（{0}，prewarm=always，真实热键路径） ====" -f $Tag)
Write-Host (($rows | Format-Table -AutoSize | Out-String -Width 230).TrimEnd())
$hitCount = @($rows | Where-Object { $_.命中 }).Count
$fgStats = (($rows | Group-Object 前台类 | ForEach-Object { "$($_.Name)×$($_.Count)" }) -join ", ")
Write-Host ("  命中 {0}/{1} = {2:N0}%   （前台上报：{3}）" -f $hitCount, $rows.Count, (100.0 * $hitCount / $rows.Count), $fgStats)
[ordered]@{ tag = $Tag; rows = $rows; hit = $hitCount; total = $rows.Count } |
    ConvertTo-Json -Depth 6 | Set-Content (Join-Path $PSScriptRoot ("prewarm-" + $Tag + ".json")) -Encoding UTF8







