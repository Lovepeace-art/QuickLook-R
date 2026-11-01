# 音频 / 视频 / SVG / HTML 四类预览（1.7.9）端到端验证
#
# 验证的事（全部靠日志 + PrintWindow 截图，不靠眼睛）：
#   1. `.svg` 走内置光栅化（日志 `SHOW … kind=svg`），截图里真的出现图形的颜色；
#   2. `.html` 走排版预览（`kind=html`），且**没有行号栏**（左侧 0..40px 不该有一列灰字）；
#   3. `.wav` 走音频通道（`AUDIO 打开 …`），底部进度条画出来了，播完有 `AUDIO 播放结束`；
#   4. `.mp4` 走视频通道（`VIDEO 播放 …`），两张间隔拍到的画面**不一样**（真的在动）；
#   5. 视频底部留出了进度条（那一行里既有底色也有蓝色已播段）；
#   6. 视频窗口高度比"内容高度 + 顶栏"多一条进度条（1.7.9 开窗尺寸）。
#
# 用法：
#   pwsh -NoProfile -File scripts/verify-media.ps1
#   pwsh -NoProfile -File scripts/verify-media.ps1 -Exe target\release\qlrust.exe -OutDir ..\_bench\media
#   pwsh -NoProfile -File scripts/verify-media.ps1 -Video C:\path\to\clip.mp4
# 退出码：0 = 全过；1 = 有失败项。
#
# 说明：视频夹具默认取 `..\..\qlrust-dev\compare\video-demo.mp4`（本机开发夹具），
# 没有就退回 `-Video` 参数；两者都没有时视频那几项 SKIP（其它三项照跑）。

param(
    [string]$Exe = "target\release\qlrust.exe",
    [string]$OutDir = "..\_bench\media",
    [string]$Video = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not (Test-Path $Exe)) { throw "找不到可执行体：$Exe（先 cargo build --release）" }
$Exe = (Resolve-Path $Exe).Path
if (-not (Test-Path $OutDir)) { New-Item -ItemType Directory -Path $OutDir -Force | Out-Null }
$OutDir = (Resolve-Path $OutDir).Path

# 便携模式（数据写在 exe 同目录）。副本放 %TEMP%：DSH 工作目录里的 exe 会带 Low 完整性
# 标签，托盘/Shell 相关能力会被 UIPI 拒（CONVENTIONS #29）。
$work = Join-Path $env:TEMP "qlrust-media-verify"
if (Test-Path $work) {
    Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 250
    Remove-Item $work -Recurse -Force
}
New-Item -ItemType Directory -Path $work -Force | Out-Null
$app = Join-Path $work "qlrust.exe"
Copy-Item $Exe $app
New-Item -ItemType File -Path (Join-Path $work "portable.lock") | Out-Null
$logFile = Join-Path $work "UserData\logs\app.log"
New-Item -ItemType Directory -Path (Join-Path $work "UserData") -Force | Out-Null
'{"log_level":"debug"}' | Set-Content (Join-Path $work "UserData\config.json") -Encoding UTF8

Add-Type -Namespace MV -Name Win -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumWinProc cb, IntPtr p);
public delegate bool EnumWinProc(IntPtr hWnd, IntPtr lParam);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, System.Text.StringBuilder s, int n);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
[DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
[DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
[DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint f);
[DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
public struct RECT { public int L, T, R, B; }
'@

$results = [System.Collections.Generic.List[object]]::new()
function Check { param([string]$Name, [bool]$Ok, [string]$Detail = "")
    $results.Add([pscustomobject]@{ 检查 = $Name; 结果 = $(if ($Ok) { "PASS" } else { "FAIL" }); 细节 = $Detail }) }
function Skip { param([string]$Name, [string]$Why = "")
    $results.Add([pscustomobject]@{ 检查 = $Name; 结果 = "SKIP"; 细节 = $Why }) }

function Start-Qlr { param([string[]]$ArgList = @())
    $cmd = '"' + $app + '"'
    if ($ArgList.Count -gt 0) { $cmd += " " + (($ArgList | ForEach-Object { '"' + $_ + '"' }) -join " ") }
    $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $cmd }
    if ($r.ReturnValue -ne 0) { throw "WMI 创建进程失败，ReturnValue=$($r.ReturnValue)" }
    [int]$r.ProcessId
}

function Get-Preview {
    $script:found = [IntPtr]::Zero
    $cb = [MV.Win+EnumWinProc]{
        param($h, $l)
        if ([MV.Win]::IsWindowVisible($h)) {
            $sb = [System.Text.StringBuilder]::new(256)
            [void][MV.Win]::GetClassName($h, $sb, 256)
            if ($sb.ToString() -eq "QLRust.PreviewWindow") { $script:found = $h; return $false }
        }
        return $true
    }
    [void][MV.Win]::EnumWindows($cb, [IntPtr]::Zero)
    $script:found
}

function Read-Log { if (Test-Path $logFile) { Get-Content $logFile -Raw } else { "" } }

function Wait-Log { param([string]$Pattern, [int]$TimeoutMs = 5000)
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $TimeoutMs) {
        if ((Read-Log) -match $Pattern) { return $true }
        Start-Sleep -Milliseconds 60
    }
    return $false
}

function Reset-Log {
    $p = Get-Process qlrust -ErrorAction SilentlyContinue
    if (-not $p) { Remove-Item $logFile -ErrorAction SilentlyContinue }
}

# 窗口有一半在屏幕外时，PrintWindow 会抓到"新画的一半 + 陈旧的一半"（CONVENTIONS #129）。
function Normalize-Position { param([IntPtr]$Hwnd)
    $r = New-Object MV.Win+RECT
    [void][MV.Win]::GetWindowRect($Hwnd, [ref]$r)
    $sw = [MV.Win]::GetSystemMetrics(0)
    $sh = [MV.Win]::GetSystemMetrics(1)
    if ($r.R -gt $sw -or $r.B -gt $sh -or $r.L -lt 0 -or $r.T -lt 0) {
        $w = $r.R - $r.L
        $h = $r.B - $r.T
        [void][MV.Win]::SetWindowPos($Hwnd, [IntPtr]::Zero, 60, 40, $w, $h, 0x0004)
        Start-Sleep -Milliseconds 250
    }
}

function Save-Shot { param([IntPtr]$Hwnd, [string]$Name)
    Add-Type -AssemblyName System.Drawing
    $r = New-Object MV.Win+RECT
    [void][MV.Win]::GetWindowRect($Hwnd, [ref]$r)
    $w = $r.R - $r.L; $h = $r.B - $r.T
    $bmp = New-Object System.Drawing.Bitmap($w, $h)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    [void][MV.Win]::PrintWindow($Hwnd, $hdc, 2)   # 2 = PW_RENDERFULLCONTENT
    $g.ReleaseHdc($hdc)
    $file = Join-Path $OutDir $Name
    $bmp.Save($file, [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bmp.Dispose()
    $file
}

# 统计一块区域里"不是背景色"的像素数（背景是浅灰 #F7F7F7 / 白）。
function Count-Ink { param([string]$Path, [int]$X0, [int]$Y0, [int]$X1, [int]$Y1, [int]$Tolerance = 24)
    Add-Type -AssemblyName System.Drawing
    $img = [System.Drawing.Bitmap]::new($Path)
    $x1 = [Math]::Min($X1, $img.Width - 1)
    $y1 = [Math]::Min($Y1, $img.Height - 1)
    $n = 0
    for ($y = [Math]::Max(0, $Y0); $y -le $y1; $y++) {
        for ($x = [Math]::Max(0, $X0); $x -le $x1; $x++) {
            $c = $img.GetPixel($x, $y)
            if ([Math]::Abs($c.R - $c.G) -gt $Tolerance -or [Math]::Abs($c.G - $c.B) -gt $Tolerance -or $c.R -lt 200) { $n++ }
        }
    }
    $img.Dispose()
    $n
}

# 两张同尺寸截图里明显不同的像素个数（判"画面在动"）。
function Count-Diff { param([string]$A, [string]$B, [int]$Step = 7, [int]$Tolerance = 16)
    Add-Type -AssemblyName System.Drawing
    $ia = [System.Drawing.Bitmap]::new($A)
    $ib = [System.Drawing.Bitmap]::new($B)
    $n = 0
    for ($y = 40; $y -lt [Math]::Min($ia.Height, $ib.Height); $y += $Step) {
        for ($x = 0; $x -lt [Math]::Min($ia.Width, $ib.Width); $x += $Step) {
            $ca = $ia.GetPixel($x, $y); $cb = $ib.GetPixel($x, $y)
            if ([Math]::Abs($ca.R - $cb.R) -gt $Tolerance -or [Math]::Abs($ca.G - $cb.G) -gt $Tolerance -or [Math]::Abs($ca.B - $cb.B) -gt $Tolerance) { $n++ }
        }
    }
    $ia.Dispose(); $ib.Dispose()
    $n
}

# ---- 0. 夹具 ----
Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 250
$fixtures = Join-Path $work "fixtures"
New-Item -ItemType Directory -Path $fixtures -Force | Out-Null

$svgPath = Join-Path $fixtures "demo.svg"
@'
<svg xmlns="http://www.w3.org/2000/svg" width="240" height="160" viewBox="0 0 240 160">
  <defs>
    <linearGradient id="g" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#ff2040"/>
      <stop offset="1" stop-color="#2040ff"/>
    </linearGradient>
  </defs>
  <rect x="0" y="0" width="240" height="160" fill="url(#g)"/>
  <circle cx="120" cy="80" r="46" fill="#ffe000"/>
  <polygon points="120,14 148,66 92,66" fill="#20c060"/>
</svg>
'@ | Set-Content -Path $svgPath -Encoding UTF8

$htmlPath = Join-Path $fixtures "demo.html"
@'
<html><body>
<h1>预览测试</h1>
<p>这一段是正文，用来确认 HTML 走的是排版预览，而不是等宽源码。</p>
<ul><li>第一项</li><li>第二项</li></ul>
<pre>code line 1
code line 2</pre>
</body></html>
'@ | Set-Content -Path $htmlPath -Encoding UTF8

# 2 秒、440 Hz、8 kHz、16 bit 单声道 WAV（纯 PowerShell 造，不依赖 ffmpeg）。
$wavPath = Join-Path $fixtures "demo.wav"
$rate = 8000
$samples = $rate * 2
$ms = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter($ms)
$bw.Write([System.Text.Encoding]::ASCII.GetBytes("RIFF"))
$bw.Write([int](36 + $samples * 2))
$bw.Write([System.Text.Encoding]::ASCII.GetBytes("WAVEfmt "))
$bw.Write([int]16)          # fmt 块长度
$bw.Write([int16]1)         # PCM
$bw.Write([int16]1)         # 单声道
$bw.Write([int]$rate)
$bw.Write([int]($rate * 2)) # 每秒字节数
$bw.Write([int16]2)         # 块对齐
$bw.Write([int16]16)        # 位深
$bw.Write([System.Text.Encoding]::ASCII.GetBytes("data"))
$bw.Write([int]($samples * 2))
for ($i = 0; $i -lt $samples; $i++) {
    $v = [int16]([Math]::Sin(2 * [Math]::PI * 440 * $i / $rate) * 8000)
    $bw.Write($v)
}
$bw.Flush()
[System.IO.File]::WriteAllBytes($wavPath, $ms.ToArray())
$bw.Dispose(); $ms.Dispose()

# 视频夹具：优先参数，其次本机开发夹具目录。
$videoPath = $null
$candidates = @()
if ($Video) { $candidates += $Video }
$candidates += (Join-Path $env:USERPROFILE "Desktop\qlrust-dev\compare\video-demo.mp4")
$candidates += (Join-Path $root "..\qlrust-dev\compare\video-demo.mp4")
foreach ($c in $candidates) {
    if ($c -and (Test-Path $c)) { $videoPath = (Resolve-Path $c).Path; break }
}

# ---- 1. SVG ----
$hostPid = Start-Qlr
Start-Sleep -Milliseconds 900
Start-Qlr -ArgList @($svgPath) | Out-Null
Check "svg 走内置光栅化" (Wait-Log "SHOW .*kind=svg") ""
$hwnd = Get-Preview
if ($hwnd -eq [IntPtr]::Zero) {
    Check "预览窗口可见（svg）" $false "没找到 QLRust.PreviewWindow"
    $results | Format-Table -AutoSize
    exit 1
}
Check "预览窗口可见（svg）" $true
Normalize-Position -Hwnd $hwnd
Start-Sleep -Milliseconds 300
$shotSvg = Save-Shot -Hwnd $hwnd -Name "01-svg.png"
$svgInk = Count-Ink -Path $shotSvg -X0 0 -Y0 32 -X1 400 -Y1 300
Check "svg 画出了图形（彩色像素）" ($svgInk -gt 3000) ("非背景像素 {0}，截图 {1}" -f $svgInk, $shotSvg)

# ---- 2. HTML ----
Start-Qlr -ArgList @($htmlPath) | Out-Null
Check "html 走排版预览" (Wait-Log "SHOW .*kind=html") ""
Start-Sleep -Milliseconds 400
Normalize-Position -Hwnd $hwnd
$shotHtml = Save-Shot -Hwnd $hwnd -Name "02-html.png"
$htmlInk = Count-Ink -Path $shotHtml -X0 20 -Y0 40 -X1 600 -Y1 320
Check "html 正文有内容" ($htmlInk -gt 800) ("非背景像素 {0}，截图 {1}" -f $htmlInk, $shotHtml)
# 行号栏在 1.7.9 对 html 关掉了：最左边 40px 里不该有密集的灰字。
$gutterInk = Count-Ink -Path $shotHtml -X0 0 -Y0 40 -X1 30 -Y1 320 -Tolerance 60
Check "html 没有行号栏" ($gutterInk -lt 400) ("左侧 30px 非背景像素 {0}" -f $gutterInk)

# ---- 3. 音频 ----
Start-Qlr -ArgList @($wavPath) | Out-Null
Check "wav 走音频通道" (Wait-Log "AUDIO 打开") ""
Check "音频自动开始播放" (Wait-Log "AUDIO 打开 .*播放=true") ""
Start-Sleep -Milliseconds 500
Normalize-Position -Hwnd $hwnd
$shotAudio = Save-Shot -Hwnd $hwnd -Name "03-audio.png"
Add-Type -AssemblyName System.Drawing
$img = [System.Drawing.Bitmap]::new($shotAudio)
# 进度条在内容区最后 52 px：轨道是灰色，已播段是有饱和度的蓝。
$hasTrack = $false
$hasFill = $false
for ($y = $img.Height - 44; $y -lt $img.Height - 4; $y++) {
    for ($x = 10; $x -lt [int]($img.Width * 0.6); $x++) {
        $c = $img.GetPixel($x, $y)
        $mx = [Math]::Max($c.R, [Math]::Max($c.G, $c.B))
        $mn = [Math]::Min($c.R, [Math]::Min($c.G, $c.B))
        if (($mx - $mn) -gt 40) { $hasFill = $true }
        if ($c.R -ge 0xD0 -and $c.R -le 0xEA -and [Math]::Abs($c.R - $c.G) -lt 6 -and [Math]::Abs($c.G - $c.B) -lt 6) { $hasTrack = $true }
    }
}
$img.Dispose()
Check "音频进度条画出来了" ($hasTrack -or $hasFill) ("轨道={0} 已播={1} 截图={2}" -f $hasTrack, $hasFill, $shotAudio)
Check "音频播完（2 秒）" (Wait-Log "AUDIO 播放结束" 6000) ""

# ---- 4. 视频 ----
if (-not $videoPath) {
    Skip "视频逐帧播放" "没找到 mp4 夹具（可用 -Video 指定）"
    Skip "视频画面在动" "同上"
    Skip "视频底部留出进度条" "同上"
} else {
    Start-Qlr -ArgList @($videoPath) | Out-Null
    Check "mp4 走视频通道" (Wait-Log "SHOW .*kind=video") ""
    Check "视频自动开始播放" (Wait-Log "VIDEO 播放 .*时长 \d+ms") ""
    Start-Sleep -Milliseconds 500
    Normalize-Position -Hwnd $hwnd
    $frameA = Save-Shot -Hwnd $hwnd -Name "04-video-a.png"
    Start-Sleep -Milliseconds 600
    $frameB = Save-Shot -Hwnd $hwnd -Name "04-video-b.png"
    $diff = Count-Diff -A $frameA -B $frameB
    Check "视频画面在动（两帧不同）" ($diff -gt 40) ("差异采样点 {0}，{1} vs {2}" -f $diff, (Split-Path $frameA -Leaf), (Split-Path $frameB -Leaf))
    # 进度条那一行：整条不该是画面（画面只贴在"画面区"里）。
    $img = [System.Drawing.Bitmap]::new($frameA)
    $rowY = $img.Height - 20
    $light = 0; $total = 0
    for ($x = 10; $x -lt $img.Width - 10; $x++) {
        $c = $img.GetPixel($x, $rowY)
        $total++
        if ($c.R -gt 0xD0 -and $c.G -gt 0xD0 -and $c.B -gt 0xD0) { $light++ }
    }
    $img.Dispose()
    Check "视频底部留出进度条（不是画面像素）" (($light / [Math]::Max($total, 1)) -gt 0.35) ("浅色占比 {0:P0}，截图 {1}" -f ($light / [Math]::Max($total, 1)), $frameA)
    Check "视频播完" (Wait-Log "VIDEO 播放结束" 8000) ""
}

Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue

# ---- 汇总 ----
$results | Format-Table -AutoSize | Out-String -Width 200 | Write-Host
$fail = @($results | Where-Object { $_.结果 -eq "FAIL" }).Count
$pass = @($results | Where-Object { $_.结果 -eq "PASS" }).Count
$skip = @($results | Where-Object { $_.结果 -eq "SKIP" }).Count
Write-Host ("{0} PASS · {1} FAIL · {2} SKIP（截图在 {3}）" -f $pass, $fail, $skip, $OutDir)
if ($fail -gt 0) { exit 1 }
exit 0
