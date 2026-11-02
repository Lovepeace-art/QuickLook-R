# QLRust 1.8.3 性能测量（对照 docs/dev/COMPARE-1.8.2-data.md 的留存口径）
#
# 六组指标 + 一组 1.8.3 专项（PDF HALFTONE/STRETCH 回归验证）：
#   cold   冷启动 → 预览窗出现 → 画面稳定（对齐 1.8.2 的"出窗/稳定"口径）
#   res    常驻资源（裸启静置 10s / 看大 PDF / 看 48MP 图 / 收起是否回收 / 连看是否泄漏）
#   frame  首帧渲染耗时（RENDER ... elapsed=，5 类格式各 5 次取中位）
#   pdf    PDF 翻页（--render --page N 的 RENDER-CLI + 常驻实例 --next-page 的端到端）
#   scroll 文本连续滚动、图片缩放 1.25× 的重渲染
#   exit   预览态 --quit → 进程消失（并确认 5 秒退出看门狗没有误触发）
#   dpi    QLRUST_DPI_FAKE=192 打开 PDF：不应出现 STRETCH，只应有 LETTERBOX
#
# 纪律：每轮测量前 `Get-Process qlrust | Stop-Process -Force` 清场，测完也清场。
#       被测 exe 用 release 构建、默认配置（唯一例外：dpi 组显式用
#       QLRUST_DPI_FAKE / QLRUST_LOG，报告里写明）。
#
# 注意：宿主进程运行期间**不能**删日志文件：Rust 的 OpenOptions 带 FILE_SHARE_DELETE，
#       删掉之后进程继续往已删除的 inode 写，新开文件的读者永远看不到后续行。
#       所以组内一律用"行号基线"取增量（Get-Mark / Get-LinesSince）。
#
# 用法：
#   pwsh -NoProfile -File scripts/measure-183.ps1 -Group all
#   pwsh -NoProfile -File scripts/measure-183.ps1 -Group frame -Runs 5
# 结果：控制台表格 + scripts\perf-183-<group>.json

param(
    [string]$Exe = "target\release\qlrust.exe",
    [string]$Fixtures = "",
    [int]$Runs = 5,
    [string]$Group = "all"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not (Test-Path $Exe)) { throw "找不到可执行体：$Exe（先 cargo build --release）" }
$Exe = (Resolve-Path $Exe).Path
$exeDir = Split-Path -Parent $Exe

# 夹具目录：显式参数 > QLRUST_FIXTURES > 开发机默认（<桌面>\qlrust-dev），不写死绝对路径。
if (-not $Fixtures) {
    if ($env:QLRUST_FIXTURES) { $Fixtures = $env:QLRUST_FIXTURES }
    else { $Fixtures = Join-Path ([Environment]::GetFolderPath('Desktop')) 'qlrust-dev' }
}
if (-not (Test-Path $Fixtures)) { throw "找不到夹具目录：$Fixtures（用 -Fixtures 或 QLRUST_FIXTURES 指定）" }
$Fixtures = (Resolve-Path $Fixtures).Path

# 便携模式：数据写在 exe 同目录，测完可以整体删掉，不污染 %LOCALAPPDATA%。
$portableLock = Join-Path $exeDir "portable.lock"
if (-not (Test-Path $portableLock)) { New-Item -ItemType File -Path $portableLock | Out-Null }
$logFile = Join-Path $exeDir "UserData\logs\app.log"

$F = @{
    img_small = Join-Path $Fixtures "compare\img-photo.jpg"   # 1280x720，1.8.2 冷启动夹具
    png1920   = Join-Path $Fixtures "png_1920x1080.png"
    img_48mp  = Join-Path $Fixtures "48mp_8000x6000.png"
    img_8mp   = Join-Path $Fixtures "8mp_3840x2160.jpg"
    pdf       = Join-Path $Fixtures "pdf_8page.pdf"
    md        = Join-Path $Fixtures "demo.md"
    txt16     = Join-Path $Fixtures "big_16mb.txt"
    txt40     = Join-Path $Fixtures "scroll-40.txt"
    lines200  = Join-Path $Fixtures "lines-200.txt"
    docx      = Join-Path $Fixtures "plugin-demo.docx"
    zip       = Join-Path $Fixtures "tree-demo.zip"
}

Add-Type -Namespace Qlr -Name Win -MemberDefinition @'
[StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr p);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr hWnd, System.Text.StringBuilder name, int max);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
[DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
[DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdc, uint flags);
[DllImport("user32.dll")] public static extern uint GetGuiResources(IntPtr hProcess, uint flags);
[DllImport("kernel32.dll")] public static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
[DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
'@
Add-Type -AssemblyName System.Drawing

$results = [ordered]@{}

# ---------------------------------------------------------------------------
# 基础设施
# ---------------------------------------------------------------------------

function Kill-Qlr {
    $p = Get-Process qlrust -ErrorAction SilentlyContinue
    if ($p) {
        $p | Stop-Process -Force -ErrorAction SilentlyContinue
        for ($i = 0; $i -lt 100; $i++) {
            if (-not (Get-Process qlrust -ErrorAction SilentlyContinue)) { break }
            Start-Sleep -Milliseconds 20
        }
    }
    # 1.8.2 的口径是"杀干净后等 0.9 s"，这里对齐。
    Start-Sleep -Milliseconds 900
}

function Reset-Log { if (Test-Path $logFile) { Remove-Item $logFile -Force -ErrorAction SilentlyContinue } }

function Get-LogText {
    if (-not (Test-Path $logFile)) { return "" }
    ((Get-Content $logFile -ErrorAction SilentlyContinue) -join "`n")
}

# 为什么不用 Start-Process：它让子进程继承当前 shell 的 stdio 句柄，在非交互环境
# （CI / agent harness）里调用方会一直挂着等管道关闭。WMI Create 出来的进程脱离父进程。
function Start-Qlr {
    param([string[]]$ArgList = @(), [hashtable]$Env = $null)
    if ($Env -and $Env.Count -gt 0) {
        # WmiPrvSE 不继承调用方的环境变量，所以用 cmd 先 set 再起进程。
        # 为什么写成 .bat：`set X=Y && prog` 会把值记成 "Y "（带尾随空格），而
        # `Level::from_env_or` 是**精确匹配**（Some("debug")），带空格就退回 info；
        # 用 `set "X=Y"` 才不会带上空格，但 cmd /c "..." 里再套引号会把命令行拆坏。
        $batch = Join-Path $env:TEMP ("qlrust-launch-" + [guid]::NewGuid().ToString('N').Substring(0, 8) + ".bat")
        $lines = @("@echo off", "chcp 65001 >nul")
        foreach ($kv in $Env.GetEnumerator()) { $lines += ('set "' + $kv.Key + '=' + $kv.Value + '"') }
        $cmd = '"' + $Exe + '"'
        if ($ArgList.Count -gt 0) { $cmd += " " + (($ArgList | ForEach-Object { '"' + $_ + '"' }) -join " ") }
        $lines += $cmd
        $lines | Set-Content -Path $batch -Encoding ASCII
        $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = ('cmd.exe /c "' + $batch + '"') }
        if ($r.ReturnValue -ne 0) { throw "WMI 创建进程失败，ReturnValue=$($r.ReturnValue)" }
        for ($i = 0; $i -lt 400; $i++) {
            $p = Get-Process qlrust -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($p) { return [int]$p.Id }
            Start-Sleep -Milliseconds 5
        }
        throw "cmd 包装启动后没等到 qlrust 进程"
    }
    $quoted = ($ArgList | ForEach-Object { '"' + $_ + '"' }) -join " "
    $cmdLine = '"' + $Exe + '"'
    if ($quoted) { $cmdLine = $cmdLine + " " + $quoted }
    $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $cmdLine }
    if ($r.ReturnValue -ne 0) { throw "WMI 创建进程失败，ReturnValue=$($r.ReturnValue)" }
    return [int]$r.ProcessId
}

function Wait-ProcExit {
    param([int]$ProcessId, [int]$TimeoutMs = 10000)
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $TimeoutMs) {
        if (-not (Get-Process -Id $ProcessId -ErrorAction SilentlyContinue)) { return $sw.ElapsedMilliseconds }
        Start-Sleep -Milliseconds 2
    }
    return -1
}

function Find-PreviewWindow {
    param([int]$ProcessId)
    $script:__found = [IntPtr]::Zero
    $callback = {
        param([IntPtr]$hwnd, [IntPtr]$lparam)
        $sb = [System.Text.StringBuilder]::new(256)
        [void][Qlr.Win]::GetClassName($hwnd, $sb, 256)
        if ($sb.ToString() -eq "QLRust.PreviewWindow") {
            if (-not [Qlr.Win]::IsWindowVisible($hwnd)) { return $true }
            $owner = 0
            [void][Qlr.Win]::GetWindowThreadProcessId($hwnd, [ref]$owner)
            if ($ProcessId -eq 0 -or $owner -eq $ProcessId) { $script:__found = $hwnd; return $false }
        }
        return $true
    }
    [void][Qlr.Win]::EnumWindows($callback, [IntPtr]::Zero)
    return $script:__found
}

function Get-WindowSize {
    param([IntPtr]$Hwnd)
    if ($Hwnd -eq [IntPtr]::Zero) { return "-" }
    $rc = New-Object Qlr.Win+RECT
    [void][Qlr.Win]::GetWindowRect($Hwnd, [ref]$rc)
    return "$($rc.Right - $rc.Left)x$($rc.Bottom - $rc.Top)"
}

# 画面哈希：PrintWindow(PW_RENDERFULLCONTENT) 到内存位图，再取**内容区一小块**做 MD5。
# 1.8.2 的口径是"连续两次采样相同 = 画面稳定"，这里对齐。
# 为什么只哈希一小块：PrintWindow 本身要 14–16 ms（与目标位图大小关系不大），
# 而整窗 BMP 编码 + MD5 在 1280×750 上还要 20–25 ms；取 240×180 的正文区域后
# 单次采样 ≈ 16 ms，滚动"首变"的时间分辨率从 ±40 ms 提到 ±16 ms。
function Get-WindowHash {
    param([IntPtr]$Hwnd, [int]$RegionX = 60, [int]$RegionY = 80, [int]$RegionW = 240, [int]$RegionH = 180)
    if ($Hwnd -eq [IntPtr]::Zero) { return "" }
    $rc = New-Object Qlr.Win+RECT
    [void][Qlr.Win]::GetWindowRect($Hwnd, [ref]$rc)
    $w = $rc.Right - $rc.Left
    $h = $rc.Bottom - $rc.Top
    if ($w -le 0 -or $h -le 0) { return "" }
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    [void][Qlr.Win]::PrintWindow($Hwnd, $hdc, 2)   # PW_RENDERFULLCONTENT
    $g.ReleaseHdc($hdc)
    $g.Dispose()
    $rx = [math]::Max(0, [math]::Min($RegionX, $w - 1))
    $ry = [math]::Max(0, [math]::Min($RegionY, $h - 1))
    $rw = [math]::Max(1, [math]::Min($RegionW, $w - $rx))
    $rh = [math]::Max(1, [math]::Min($RegionH, $h - $ry))
    $rect = New-Object System.Drawing.Rectangle $rx, $ry, $rw, $rh
    $sub = $bmp.Clone($rect, $bmp.PixelFormat)
    $bmp.Dispose()
    $ms = New-Object System.IO.MemoryStream
    $sub.Save($ms, [System.Drawing.Imaging.ImageFormat]::Bmp)
    $sub.Dispose()
    $bytes = $ms.ToArray()
    $ms.Dispose()
    $md5 = [System.Security.Cryptography.MD5]::Create()
    $hash = [BitConverter]::ToString($md5.ComputeHash($bytes))
    $md5.Dispose()
    return $hash
}

# 等到"连续两次采样相同"，返回自 $Since 起的毫秒数（-1 = 超时）。
function Wait-Stable {
    param([IntPtr]$Hwnd, [int]$TimeoutMs = 8000, [System.Diagnostics.Stopwatch]$Since,
        [int]$RegionX = 60, [int]$RegionY = 80, [int]$RegionW = 240, [int]$RegionH = 180)
    $prev = ""
    while ($Since.ElapsedMilliseconds -lt $TimeoutMs) {
        $h = Get-WindowHash $Hwnd -RegionX $RegionX -RegionY $RegionY -RegionW $RegionW -RegionH $RegionH
        if ($h -and $h -eq $prev) { return $Since.ElapsedMilliseconds }
        $prev = $h
    }
    return -1
}

function Get-ProcRes {
    param([int]$ProcessId)
    $p = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
    if (-not $p) { return $null }
    $gdi = 0; $user = 0
    $h = [Qlr.Win]::OpenProcess(0x0400 -bor 0x1000, $false, [uint32]$ProcessId)  # QUERY_INFORMATION|LIMITED
    if ($h -ne [IntPtr]::Zero) {
        $gdi = [Qlr.Win]::GetGuiResources($h, 0)
        $user = [Qlr.Win]::GetGuiResources($h, 1)
        [void][Qlr.Win]::CloseHandle($h)
    }
    return [pscustomobject]@{
        PrivateMB = [math]::Round($p.PrivateMemorySize64 / 1MB, 2)
        WSMB      = [math]::Round($p.WorkingSet64 / 1MB, 2)
        Handles   = $p.HandleCount
        Threads   = $p.Threads.Count
        GDI       = $gdi
        USER      = $user
    }
}

function Get-LogLine {
    param([string]$Pattern)
    $m = Select-String -Path $logFile -Pattern $Pattern -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($m) { return $m.Line }
    return ""
}

function Get-AllLogLines {
    param([string]$Pattern)
    if (-not (Test-Path $logFile)) { return @() }
    @(Select-String -Path $logFile -Pattern $Pattern -ErrorAction SilentlyContinue | ForEach-Object { $_.Line })
}

# 最后一条匹配行；没有就返回空串（空数组 [-1] 会抛 InvalidOperation）
function Get-LastLogLine {
    param([string]$Pattern)
    $all = @(Get-AllLogLines $Pattern)
    if ($all.Count -eq 0) { return "" }
    return $all[$all.Count - 1]
}

# 行号基线：动作前后各取一次，用 Get-LinesSince 拿增量（宿主运行中不能删日志）
function Get-Mark {
    param([string]$Pattern)
    return @(Get-AllLogLines $Pattern).Count
}

function Get-LinesSince {
    param([string]$Pattern, [int]$Mark)
    $all = @(Get-AllLogLines $Pattern)
    if ($all.Count -le $Mark) { return @() }
    return @($all[$Mark..($all.Count - 1)])
}

# 日志行首的 UTC 时间戳 → DateTime（UTC）
function Get-LineTime {
    param([string]$Line)
    if ($Line -match '^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3})Z') {
        return [datetime]::ParseExact($Matches[1], 'yyyy-MM-ddTHH:mm:ss.fff', $null)
    }
    return $null
}

function Get-LogField {
    param([string]$Line, [string]$Pattern)
    if ($Line -and $Line -match $Pattern) { return $Matches[1] }
    return -1
}

function Get-Median {
    param([double[]]$Values)
    $v = @($Values | Where-Object { $_ -ne $null -and $_ -ge 0 })
    if ($v.Count -eq 0) { return -1 }
    $s = @($v | Sort-Object)
    $n = $s.Count
    if ($n % 2 -eq 1) { return $s[[int](($n - 1) / 2)] }
    return [math]::Round(($s[$n / 2 - 1] + $s[$n / 2]) / 2, 1)
}

function Show-Table {
    param($Rows, [string]$Title)
    Write-Host ""
    Write-Host "==== $Title ===="
    Write-Host (($Rows | Format-Table -AutoSize | Out-String -Width 220).TrimEnd())
}

# 冷启动一次：清场 → WMI 起 --show <file> → 轮询可见窗 → 等到画面稳定
function Measure-Cold {
    param([string]$File, [string]$Label)
    Kill-Qlr
    Reset-Log
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $procId = Start-Qlr -ArgList @("--show", $File)
    $hwnd = [IntPtr]::Zero
    while ($sw.ElapsedMilliseconds -lt 8000) {
        $hwnd = Find-PreviewWindow -ProcessId $procId
        if ($hwnd -ne [IntPtr]::Zero) { break }
    }
    $winMs = $sw.ElapsedMilliseconds
    # 窗口先出现、内容后到位（SHOW 只保证窗口可见）：先等日志里出现第一帧 RENDER，
    # 再判"画面稳定"。否则 48MP 这种 200 ms 的大图会量到**占位页**稳定（假稳定）。
    $contentMs = -1
    if ($hwnd -ne [IntPtr]::Zero) {
        $deadline = (Get-Date).AddSeconds(6)
        while ((Get-Date) -lt $deadline) {
            if (Get-LogText) {
                if ((Get-LogText) -match "RENDER gen=") { $contentMs = $sw.ElapsedMilliseconds; break }
            }
            Start-Sleep -Milliseconds 5
        }
    }
    $stableMs = -1
    if ($hwnd -ne [IntPtr]::Zero) { $stableMs = Wait-Stable -Hwnd $hwnd -TimeoutMs 9000 -Since $sw }
    $size = Get-WindowSize $hwnd
    Start-Sleep -Milliseconds 150
    $showLine = Get-LogLine 'SHOW target='
    $renderLine = Get-LogLine 'RENDER gen='
    $shellLine = Get-LogLine '壳层就绪'
    $hotkeyLine = Get-LogLine '热键就绪'
    $firstLine = (Get-Content $logFile -TotalCount 1 -ErrorAction SilentlyContinue)
    $p = Get-Process -Id $procId -ErrorAction SilentlyContinue
    $procStartMs = -1
    if ($p -and $firstLine) {
        $t0 = Get-LineTime $firstLine
        if ($t0) { $procStartMs = [int](($t0 - $p.StartTime.ToUniversalTime()).TotalMilliseconds) }
    }
    $res = Get-ProcRes -ProcessId $procId
    Kill-Qlr
    return [pscustomobject]@{
        夹具               = $Label
        出窗ms             = $winMs
        首帧内容ms         = $contentMs
        稳定ms             = $stableMs
        窗口尺寸           = $size
        进程创建到首日志ms = $procStartMs
        壳层就绪ms         = (Get-LogField $shellLine 'window_created=(\d+)ms')
        热键就绪ms         = (Get-LogField $hotkeyLine 'elapsed=(\d+)ms')
        SHOWelapsed       = (Get-LogField $showLine 'elapsed=(\d+)ms')
        RENDERelapsed     = (Get-LogField $renderLine 'elapsed=(\d+)ms')
        内存MB             = $(if ($res) { $res.WSMB } else { -1 })
    }
}

# 常驻宿主：裸启并等到热键就绪（= 能接 IPC）
function Start-Host {
    param([hashtable]$Env = $null)
    Kill-Qlr
    Reset-Log
    $procId = Start-Qlr -Env $Env
    $deadline = (Get-Date).AddSeconds(10)
    while ((Get-Date) -lt $deadline) {
        if ((Get-LogText) -match "热键就绪") { break }
        Start-Sleep -Milliseconds 10
    }
    return $procId
}

# 往常驻宿主投一条请求，返回第二实例的存活时长（= CLI 启动 + IPC 投递）
function Invoke-Qlr {
    param([string[]]$ArgList)
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $procId = Start-Qlr -ArgList $ArgList
    $createMs = $sw.ElapsedMilliseconds
    $exitMs = Wait-ProcExit -ProcessId $procId -TimeoutMs 5000
    if ($exitMs -gt 800 -or $createMs -gt 800) {
        Write-Host ("  [诊断] WMI创建={0}ms 退出={1}ms pid={2}" -f $createMs, $exitMs, $procId)
    }
    return $sw.ElapsedMilliseconds
}

# ---------------------------------------------------------------------------
# 一、冷启动
# ---------------------------------------------------------------------------
function Run-Cold {
    $rows = @()
    for ($i = 1; $i -le $Runs; $i++) {
        $r = Measure-Cold -File $F.img_small -Label "img-photo.jpg(1280x720)"
        $rows += $r
        Write-Host ("  冷启动轮 {0}/{1}：出窗 {2}ms 稳定 {3}ms" -f $i, $Runs, $r.出窗ms, $r.稳定ms)
    }
    Show-Table $rows "冷启动（img-photo.jpg 1280x720，$Runs 轮原始值）"
    $results.cold = [ordered]@{
        runs       = $rows
        win_med    = (Get-Median ($rows | ForEach-Object { $_.出窗ms }))
        stable_med = (Get-Median ($rows | ForEach-Object { $_.稳定ms }))
        boot_med   = (Get-Median ($rows | ForEach-Object { $_.进程创建到首日志ms }))
        shell_med  = (Get-Median ($rows | ForEach-Object { $_.壳层就绪ms }))
        hotkey_med = (Get-Median ($rows | ForEach-Object { $_.热键就绪ms }))
    }
    Write-Host ("  中位：出窗 {0}ms / 稳定 {1}ms / 进程创建→首日志 {2}ms / 壳层 {3}ms / 热键 {4}ms" -f `
            $results.cold.win_med, $results.cold.stable_med, $results.cold.boot_med, $results.cold.shell_med, $results.cold.hotkey_med)
}

# ---------------------------------------------------------------------------
# 二、常驻资源
# ---------------------------------------------------------------------------
function Run-Res {
    $rows = @()
    $procId = Start-Host
    $t0 = Get-ProcRes -ProcessId $procId
    $rows += [pscustomobject]@{ 场景 = "裸启 1s"; PrivateMB = $t0.PrivateMB; WSMB = $t0.WSMB; 句柄 = $t0.Handles; 线程 = $t0.Threads; GDI = $t0.GDI; USER = $t0.USER }
    Start-Sleep -Seconds 10
    $idle = Get-ProcRes -ProcessId $procId
    $rows += [pscustomobject]@{ 场景 = "裸启静置 10s"; PrivateMB = $idle.PrivateMB; WSMB = $idle.WSMB; 句柄 = $idle.Handles; 线程 = $idle.Threads; GDI = $idle.GDI; USER = $idle.USER }

    $null = Invoke-Qlr @("--show", $F.pdf)
    Start-Sleep -Seconds 3
    $pdf = Get-ProcRes -ProcessId $procId
    $rows += [pscustomobject]@{ 场景 = "pdf_8page.pdf 看 3s"; PrivateMB = $pdf.PrivateMB; WSMB = $pdf.WSMB; 句柄 = $pdf.Handles; 线程 = $pdf.Threads; GDI = $pdf.GDI; USER = $pdf.USER }
    $null = Invoke-Qlr @("--hide")
    Start-Sleep -Seconds 3
    $afterPdf = Get-ProcRes -ProcessId $procId
    $rows += [pscustomobject]@{ 场景 = "收起 PDF 后 3s"; PrivateMB = $afterPdf.PrivateMB; WSMB = $afterPdf.WSMB; 句柄 = $afterPdf.Handles; 线程 = $afterPdf.Threads; GDI = $afterPdf.GDI; USER = $afterPdf.USER }
    Start-Sleep -Seconds 10
    $afterPdf10 = Get-ProcRes -ProcessId $procId
    $rows += [pscustomobject]@{ 场景 = "收起 PDF 后 13s"; PrivateMB = $afterPdf10.PrivateMB; WSMB = $afterPdf10.WSMB; 句柄 = $afterPdf10.Handles; 线程 = $afterPdf10.Threads; GDI = $afterPdf10.GDI; USER = $afterPdf10.USER }

    $null = Invoke-Qlr @("--show", $F.img_48mp)
    Start-Sleep -Seconds 3
    $big = Get-ProcRes -ProcessId $procId
    $rows += [pscustomobject]@{ 场景 = "48MP PNG 看 3s"; PrivateMB = $big.PrivateMB; WSMB = $big.WSMB; 句柄 = $big.Handles; 线程 = $big.Threads; GDI = $big.GDI; USER = $big.USER }
    $null = Invoke-Qlr @("--hide")
    Start-Sleep -Seconds 3
    $afterBig = Get-ProcRes -ProcessId $procId
    $rows += [pscustomobject]@{ 场景 = "收起大图后 3s"; PrivateMB = $afterBig.PrivateMB; WSMB = $afterBig.WSMB; 句柄 = $afterBig.Handles; 线程 = $afterBig.Threads; GDI = $afterBig.GDI; USER = $afterBig.USER }

    # 反复预览（换格式），看提交内存/GDI/句柄是否单调增长（= 泄漏排查）
    $mix = @($F.pdf, $F.img_48mp, $F.txt16, $F.md, $F.png1920, $F.img_8mp, $F.lines200, $F.txt40, $F.docx, $F.zip)
    for ($round = 1; $round -le 4; $round++) {
        foreach ($f in $mix) {
            if (-not (Test-Path $f)) { continue }
            $null = Invoke-Qlr @("--show", $f)
            Start-Sleep -Milliseconds 700
        }
        $snap = Get-ProcRes -ProcessId $procId
        $rows += [pscustomobject]@{ 场景 = "连看 10 个文件（第 $round 轮）"; PrivateMB = $snap.PrivateMB; WSMB = $snap.WSMB; 句柄 = $snap.Handles; 线程 = $snap.Threads; GDI = $snap.GDI; USER = $snap.USER }
    }
    $null = Invoke-Qlr @("--hide")
    Start-Sleep -Seconds 3
    $end = Get-ProcRes -ProcessId $procId
    $rows += [pscustomobject]@{ 场景 = "连看后收起 3s"; PrivateMB = $end.PrivateMB; WSMB = $end.WSMB; 句柄 = $end.Handles; 线程 = $end.Threads; GDI = $end.GDI; USER = $end.USER }

    Show-Table $rows "常驻资源（单进程）"
    $results.res = $rows
    Kill-Qlr
}

# ---------------------------------------------------------------------------
# 三、首帧渲染耗时（RENDER ... elapsed=）
# ---------------------------------------------------------------------------
function Run-Frame {
    $rows = @()
    $perFormat = [ordered]@{}
    foreach ($key in @("md", "txt16", "pdf", "png1920", "img_48mp")) {
        $elapsedList = @(); $stableList = @(); $winList = @(); $contentList = @()
        for ($i = 1; $i -le $Runs; $i++) {
            $r = Measure-Cold -File $F[$key] -Label $key
            if ($r.RENDERelapsed -ge 0) { $elapsedList += $r.RENDERelapsed }
            if ($r.稳定ms -ge 0) { $stableList += $r.稳定ms }
            if ($r.首帧内容ms -ge 0) { $contentList += $r.首帧内容ms }
            $winList += $r.出窗ms
        }
        $med = Get-Median $elapsedList
        $rows += [pscustomobject]@{
            格式       = $key
            文件       = (Split-Path -Leaf $F[$key])
            RENDER样本 = ($elapsedList -join "/")
            中位ms     = $med
            最小ms     = ($elapsedList | Measure-Object -Minimum).Minimum
            出窗中位ms = (Get-Median $winList)
            内容中位ms = (Get-Median $contentList)
            稳定中位ms = (Get-Median $stableList)
        }
        $perFormat[$key] = [ordered]@{
            samples        = $elapsedList
            median         = $med
            min            = ($elapsedList | Measure-Object -Minimum).Minimum
            win_median     = (Get-Median $winList)
            content_median = (Get-Median $contentList)
            stable_median = (Get-Median $stableList)
            file          = (Split-Path -Leaf $F[$key])
        }
        Write-Host ("  首帧 {0}：{1}ms（{2}）" -f $key, $med, ($elapsedList -join "/"))
    }
    Show-Table $rows "首帧渲染（$Runs 次取中位）"
    $results.frame = $perFormat
}

# ---------------------------------------------------------------------------
# 三点五、对齐 1.8.2 留存表的 4 个同夹具（compare\ 目录，冷启动口径）
# ---------------------------------------------------------------------------
function Run-Fixtures {
    $rows = @()
    $cases = @(
        @{ key = "compare\markdown-demo.md"; name = "markdown-demo.md"; ref = "1.8.2 稳定 110 ms" },
        @{ key = "compare\text-long.txt";    name = "text-long.txt(500 行)"; ref = "1.8.2 稳定 97 ms" },
        @{ key = "compare\doc-demo.pdf";     name = "doc-demo.pdf(8 页)"; ref = "1.8.2 稳定 99 ms" },
        @{ key = "compare\img-huge.png";     name = "img-huge.png(8000x6000)"; ref = "1.8.2 稳定 104 ms" }
    )
    foreach ($c in $cases) {
        $file = Join-Path $Fixtures $c.key
        if (-not (Test-Path $file)) { Write-Host ("  跳过（缺夹具）：{0}" -f $file); continue }
        $win = @(); $stable = @(); $mem = @(); $render = @(); $content = @()
        for ($i = 1; $i -le $Runs; $i++) {
            $r = Measure-Cold -File $file -Label $c.name
            $win += $r.出窗ms
            if ($r.稳定ms -ge 0) { $stable += $r.稳定ms }
            if ($r.内存MB -ge 0) { $mem += $r.内存MB }
            if ($r.RENDERelapsed -ge 0) { $render += $r.RENDERelapsed }
            if ($r.首帧内容ms -ge 0) { $content += $r.首帧内容ms }
        }
        $rows += [pscustomobject]@{
            夹具         = $c.name
            出窗中位ms   = (Get-Median $win)
            内容中位ms   = (Get-Median $content)
            稳定中位ms   = (Get-Median $stable)
            RENDER中位ms = (Get-Median $render)
            内存调用后MB = (Get-Median $mem)
            "1.8.2留存"  = $c.ref
        }
        Write-Host ("  {0}：出窗 {1}ms 稳定 {2}ms" -f $c.name, (Get-Median $win), (Get-Median $stable))
    }
    Show-Table $rows "同夹具对齐（compare\ 目录，$Runs 轮中位）"
    $results.fixtures = $rows
}

# ---------------------------------------------------------------------------
# 四、PDF 翻页
# ---------------------------------------------------------------------------
function Run-Pdf {
    # 4a. 无窗口渲染：--render <pdf> --page N（含打开文档的固定开销，与 bench.md 同口径）
    $cli = @()
    for ($page = 0; $page -lt 10; $page++) {
        Kill-Qlr
        Reset-Log
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        $procId = Start-Qlr -ArgList @("--render", $F.pdf, "--page", "$page")
        $null = Wait-ProcExit -ProcessId $procId -TimeoutMs 10000
        $wall = $sw.ElapsedMilliseconds
        $line = Get-LogLine 'RENDER-CLI'
        $cli += [pscustomobject]@{ 页 = $page; 渲染ms = (Get-LogField $line 'elapsed=(\d+)ms'); 进程墙钟ms = $wall }
    }
    Show-Table $cli "PDF 逐页渲染（--render --page N，含打开文档）"
    $results.pdf_cli = $cli

    # 4b. 常驻实例连续翻 10 页：--next-page
    $procId = Start-Host
    $null = Invoke-Qlr @("--show", $F.pdf)
    Start-Sleep -Milliseconds 900
    $turns = @()
    $markPage = Get-Mark 'PAGE '
    $markRender = Get-Mark 'RENDER gen='
    $markCache = Get-Mark 'PAGECACHE'
    for ($i = 1; $i -le 10; $i++) {
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        $null = Invoke-Qlr @("--next-page")
        $ipcMs = $sw.ElapsedMilliseconds
        $deadline = (Get-Date).AddSeconds(3)
        while ((Get-Date) -lt $deadline) {
            if ((Get-Mark 'PAGE ') -gt $markPage) { break }
            Start-Sleep -Milliseconds 10
        }
        Start-Sleep -Milliseconds 150
        $newPages = @(Get-LinesSince 'PAGE ' $markPage)
        $newRenders = @(Get-LinesSince 'RENDER gen=' $markRender)
        $newCache = @(Get-LinesSince 'PAGECACHE' $markCache)
        $markPage = Get-Mark 'PAGE '
        $markRender = Get-Mark 'RENDER gen='
        $markCache = Get-Mark 'PAGECACHE'
        $pageText = "（未翻页）"
        if ($newPages.Count -gt 0 -and $newPages[-1] -match 'PAGE (\S+)') { $pageText = $Matches[1] }
        $cacheText = "（无）"
        if ($newCache.Count -gt 0 -and $newCache[-1] -match 'PAGECACHE ([^（]*)') { $cacheText = $Matches[1].Trim() }
        $turns += [pscustomobject]@{
            次         = $i
            PAGE       = $pageText
            IPC墙钟ms  = $ipcMs
            页面缓存   = $cacheText
            新RENDERms = $(if ($newRenders.Count -gt 0) { (Get-LogField $newRenders[-1] 'elapsed=(\d+)ms') } else { -1 })
        }
    }
    Show-Table $turns "PDF 翻页（常驻实例 --next-page，连续 10 次）"
    $results.pdf_turns = $turns
    Kill-Qlr
}

# ---------------------------------------------------------------------------
# 五、滚动 / 缩放
# ---------------------------------------------------------------------------
function Run-Scroll {
    $procId = Start-Host

    # 5a. 文本滚动：每次 3 行，量"投递 → 画面首次变化 → 稳定"
    # 变化判据用**左侧行号栏**（x 0..80）：big_16mb.txt 的行内容完全相同，正文区域
    # 滚一屏看不出差别，行号却每次都变（实测：正文区哈希不变、行号栏哈希必变）。
    $region = @{ RegionX = 0; RegionY = 60; RegionW = 80; RegionH = 500 }
    $null = Invoke-Qlr @("--show", $F.txt16)
    Start-Sleep -Milliseconds 1200
    $win = Find-PreviewWindow -ProcessId $procId
    $ipcFloor = @()
    for ($i = 0; $i -lt 5; $i++) { $ipcFloor += (Invoke-Qlr @("--ping")) }
    $hs = [System.Diagnostics.Stopwatch]::StartNew()
    $null = Get-WindowHash $win @region
    $null = Get-WindowHash $win @region
    $sampleMs = [int]($hs.ElapsedMilliseconds / 2)
    $rows = @()
    for ($i = 1; $i -le 10; $i++) {
        $prev = Get-WindowHash $win @region
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        $procId2 = Start-Qlr -ArgList @("--scroll", "3")
        $null = Wait-ProcExit -ProcessId $procId2 -TimeoutMs 5000
        $sentMs = $sw.ElapsedMilliseconds
        $changeMs = -1
        while ($sw.ElapsedMilliseconds -lt 3000) {
            $h = Get-WindowHash $win @region
            if ($h -ne "" -and $h -ne $prev) { $changeMs = $sw.ElapsedMilliseconds; break }
        }
        $stableMs = Wait-Stable -Hwnd $win -TimeoutMs 3000 -Since $sw -RegionX 0 -RegionY 60 -RegionW 80 -RegionH 500
        $rows += [pscustomobject]@{ 次 = $i; IPCms = $sentMs; 首变ms = $changeMs; 稳定ms = $stableMs }
    }
    Show-Table $rows "文本滚动（--scroll 3，big_16mb.txt）"
    $slow = (Get-AllLogLines 'SLOWPAINT').Count
    $results.scroll = [ordered]@{
        ping_floor_med = (Get-Median $ipcFloor)
        sample_ms      = $sampleMs
        runs           = $rows
        change_med     = (Get-Median ($rows | ForEach-Object { $_.首变ms }))
        stable_med     = (Get-Median ($rows | ForEach-Object { $_.稳定ms }))
        slowpaint      = $slow
    }
    Write-Host ("  ping 往返中位 {0}ms；单次采样 {1}ms；滚动首变中位 {2}ms；稳定中位 {3}ms；SLOWPAINT {4} 条" -f `
            $results.scroll.ping_floor_med, $sampleMs, $results.scroll.change_med, $results.scroll.stable_med, $slow)

    # 5b. 图片缩放 1.25×：ZOOM 行 + 随后的 RENDER elapsed（连续 5 档）
    $null = Invoke-Qlr @("--show", $F.png1920)
    Start-Sleep -Milliseconds 1200
    $zoomRows = @()
    $markRender = Get-Mark 'RENDER gen='
    $markZoom = Get-Mark 'ZOOM \d'
    for ($i = 1; $i -le 5; $i++) {
        $null = Invoke-Qlr @("--zoom-in")
        $deadline = (Get-Date).AddSeconds(3)
        while ((Get-Date) -lt $deadline) {
            if ((Get-Mark 'RENDER gen=') -gt $markRender) { break }
            Start-Sleep -Milliseconds 10
        }
        Start-Sleep -Milliseconds 150
        $newZoom = @(Get-LinesSince 'ZOOM \d' $markZoom)
        $newRender = @(Get-LinesSince 'RENDER gen=' $markRender)
        $markRender = Get-Mark 'RENDER gen='
        $markZoom = Get-Mark 'ZOOM \d'
        $zline = "（无 ZOOM 行）"
        if ($newZoom.Count -gt 0) { $zline = ($newZoom[-1] -replace '^\S+Z INFO\s+\[\S+\] app\s+', '') }
        $rline = ""
        if ($newRender.Count -gt 0) { $rline = $newRender[-1] }
        $zoomRows += [pscustomobject]@{
            次       = $i
            ZOOM行   = $zline
            请求     = $(if ($rline -match '请求 (\d+x\d+)') { $Matches[1] } else { "-" })
            位图     = $(if ($rline -match 'bitmap (\d+x\d+)') { $Matches[1] } else { "-" })
            重渲染ms = (Get-LogField $rline 'elapsed=(\d+)ms')
        }
    }
    Show-Table $zoomRows "图片缩放 1.25× 重渲染（png_1920x1080.png）"
    $results.zoom = $zoomRows

    # 5c. 48MP 图放大一档的重渲染（最坏情况）
    $null = Invoke-Qlr @("--show", $F.img_48mp)
    Start-Sleep -Milliseconds 2500
    $markRender = Get-Mark 'RENDER gen='
    $null = Invoke-Qlr @("--zoom-in")
    $deadline = (Get-Date).AddSeconds(6)
    while ((Get-Date) -lt $deadline) {
        if ((Get-Mark 'RENDER gen=') -gt $markRender) { break }
        Start-Sleep -Milliseconds 10
    }
    Start-Sleep -Milliseconds 200
    $bigLines = @(Get-LinesSince 'RENDER gen=' $markRender)
    Show-Table ($bigLines | ForEach-Object { [pscustomobject]@{ 行 = ($_ -replace '^\S+Z INFO\s+\[\S+\] app\s+', '') } }) "48MP 图放大一档的 RENDER 行"
    $results.zoom_big = $bigLines
    Kill-Qlr
}

# ---------------------------------------------------------------------------
# 六、退出耗时
# ---------------------------------------------------------------------------
function Run-Exit {
    $rows = @()
    for ($i = 1; $i -le $Runs; $i++) {
        $procId = Start-Host
        $null = Invoke-Qlr @("--show", $F.img_48mp)
        Start-Sleep -Seconds 2
        $markWd = Get-Mark '强制结束进程'
        $markExit = Get-Mark '退出：总运行'
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        $quitPid = Start-Qlr -ArgList @("--quit")
        $exitMs = Wait-ProcExit -ProcessId $procId -TimeoutMs 15000
        $sw.Stop()
        $null = Wait-ProcExit -ProcessId $quitPid -TimeoutMs 3000
        Start-Sleep -Milliseconds 200
        $rows += [pscustomobject]@{
            次         = $i
            预览态文件 = "48mp_8000x6000.png"
            退出ms     = $exitMs
            墙钟ms     = $sw.ElapsedMilliseconds
            看门狗触发 = @(Get-LinesSince '强制结束进程' $markWd).Count
            总运行ms   = (Get-LogField (@(Get-LinesSince '退出：总运行' $markExit | Select-Object -Last 1)) '总运行 (\d+)ms')
        }
    }
    Show-Table $rows "退出耗时（预览态 --quit → 进程消失）"
    $results.exit = [ordered]@{
        runs      = $rows
        median_ms = (Get-Median ($rows | ForEach-Object { $_.退出ms }))
        watchdog  = (($rows | ForEach-Object { $_.看门狗触发 }) | Measure-Object -Sum).Sum
    }
    Write-Host ("  退出中位 {0}ms；看门狗触发 {1} 次" -f $results.exit.median_ms, $results.exit.watchdog)
    Kill-Qlr
}

# ---------------------------------------------------------------------------
# 七、1.8.3 专项：QLRUST_DPI_FAKE=192 下的 PDF（STRETCH 应消失）
# ---------------------------------------------------------------------------
function Run-Dpi {
    $rows = @()
    foreach ($dpi in @(96, 192)) {
        $envs = @{ QLRUST_LOG = "debug" }
        if ($dpi -ne 96) { $envs["QLRUST_DPI_FAKE"] = "$dpi" }
        $procId = Start-Host -Env $envs
        $null = Invoke-Qlr @("--show", $F.pdf)
        Start-Sleep -Seconds 2
        $win = Find-PreviewWindow -ProcessId $procId
        $row = [ordered]@{
            DPI       = $dpi
            窗口      = (Get-WindowSize $win)
            RENDER行  = (Get-AllLogLines 'RENDER gen=').Count
            STRETCH   = (Get-AllLogLines 'STRETCH').Count
            LETTERBOX = (Get-AllLogLines 'LETTERBOX').Count
            SLOWPAINT = (Get-AllLogLines 'SLOWPAINT').Count
            PAINT行   = (Get-AllLogLines 'PAINT bitmap').Count
        }
        # 再翻一页，确认翻页后也不出现拉伸
        $null = Invoke-Qlr @("--next-page")
        Start-Sleep -Milliseconds 1200
        $row.STRETCH_翻页后 = (Get-AllLogLines 'STRETCH').Count
        $row.LETTERBOX_翻页后 = (Get-AllLogLines 'LETTERBOX').Count
        $row.末条PAINT = Get-LastLogLine 'PAINT bitmap'
        $row.末条LETTERBOX = Get-LastLogLine 'LETTERBOX'
        $row.末条STRETCH = Get-LastLogLine 'STRETCH'
        $row.末条RENDER = Get-LastLogLine 'RENDER gen='
        $rows += [pscustomobject]$row
        Kill-Qlr
    }
    Show-Table $rows "PDF 在 96 / 192 DPI 下的拉伸告警（QLRUST_DPI_FAKE，QLRUST_LOG=debug）"
    foreach ($r in $rows) {
        Write-Host ("  [{0} DPI] PAINT: {1}" -f $r.DPI, ($r.末条PAINT -replace '^\S+Z DEBUG\s+\[\S+\] app\s+', ''))
        Write-Host ("  [{0} DPI] RENDER: {1}" -f $r.DPI, ($r.末条RENDER -replace '^\S+Z INFO\s+\[\S+\] app\s+', ''))
        Write-Host ("  [{0} DPI] LETTERBOX: {1}" -f $r.DPI, ($r.末条LETTERBOX -replace '^\S+Z DEBUG\s+\[\S+\] app\s+', ''))
        if ($r.末条STRETCH) { Write-Host ("  [{0} DPI] STRETCH: {1}" -f $r.DPI, $r.末条STRETCH) }
    }
    $results.dpi = $rows
}

# ---------------------------------------------------------------------------
# 主流程
# ---------------------------------------------------------------------------
Kill-Qlr
$startedAt = Get-Date
Write-Host ("QLRust 性能测量开始 {0}" -f $startedAt)
Write-Host ("exe = {0}（FileVersion {1}）" -f $Exe, (Get-Item $Exe).VersionInfo.FileVersion)
Write-Host ("夹具 = {0}" -f $Fixtures)

switch ($Group) {
    "cold"     { Run-Cold }
    "res"      { Run-Res }
    "fixtures" { Run-Fixtures }
    "frame"    { Run-Frame }
    "pdf"      { Run-Pdf }
    "scroll"   { Run-Scroll }
    "exit"     { Run-Exit }
    "dpi"      { Run-Dpi }
    "all"      { Run-Cold; Run-Res; Run-Fixtures; Run-Frame; Run-Pdf; Run-Scroll; Run-Exit; Run-Dpi }
    default    { throw "未知 -Group：$Group" }
}

Kill-Qlr
$out = Join-Path $PSScriptRoot ("perf-183-" + $Group + ".json")
$results | ConvertTo-Json -Depth 8 | Set-Content -Path $out -Encoding UTF8
Write-Host ""
Write-Host ("结果已写入 {0}；总耗时 {1:N0}s" -f $out, ((Get-Date) - $startedAt).TotalSeconds)


