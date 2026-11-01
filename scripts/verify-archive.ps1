# 压缩包列表视图（1.7.3）端到端验证
#
# 验证的事（全部靠日志 + PrintWindow 截图，不靠眼睛）：
#   1. `.zip` 打开的是**内置列表视图**（日志 `ARCHIVE` + `SHOW archive`），不再走插本文本通道；
#   2. 预览窗真的画出了内容（截图非空白，且有"名字 / 大小 / 时间"三列）；
#   3. 点目录行 → 折叠（日志 `ARCHIVE 点击 … 展开=false`），再点 → 展开；
#   4. 滚轮按行滚动（日志 `ARCHIVE 滚动`）；
#   5. `.cbz/.jar/.war` 也进同一条通道（内置优先于 office 插件）；
#   6. 不是 zip 的 `.zip` → 解析失败、响一声、不显示（不残留占位）。
#
# 用法：
#   pwsh -NoProfile -File scripts/verify-archive.ps1
#   pwsh -NoProfile -File scripts/verify-archive.ps1 -Exe target\release\qlrust.exe -OutDir ..\_bench\archive
# 退出码：0 = 全过；1 = 有失败项。
#
# 注意：点击与滚轮段需要**真实桌面会话**（脚本用 SetCursorPos + mouse_event 造真鼠标事件）。
# 无交互桌面的 CI 上这两项会 FAIL——它们是"给人看的"验证，别当门槛用。

param(
    [string]$Exe = "target\release\qlrust.exe",
    [string]$OutDir = "..\_bench\archive"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not (Test-Path $Exe)) { throw "找不到可执行体：$Exe（先 cargo build --release）" }
$Exe = (Resolve-Path $Exe).Path
if (-not (Test-Path $OutDir)) { New-Item -ItemType Directory -Path $OutDir -Force | Out-Null }
$OutDir = (Resolve-Path $OutDir).Path

# 便携模式（数据写在 exe 同目录）。副本放在 %TEMP%：DSH 工作目录里的 exe 会被打 Low
# 完整性标签，托盘/Shell 相关能力会被 UIPI 拒（CONVENTIONS #29）。
$work = Join-Path $env:TEMP "qlrust-archive-verify"
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
# 点击/滚轮/展平的证据都在 debug 级日志里（`ARCHIVE 点击 …`、`ARCHIVE 滚动 …`），
# 所以先落一份 debug 配置再启动，别指望默认的 info 级能看到它们。
New-Item -ItemType Directory -Path (Join-Path $work "UserData") -Force | Out-Null
'{"log_level":"debug"}' | Set-Content (Join-Path $work "UserData\config.json") -Encoding UTF8

Add-Type -Namespace AV -Name Win -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumWinProc cb, IntPtr p);
public delegate bool EnumWinProc(IntPtr hWnd, IntPtr lParam);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, System.Text.StringBuilder s, int n);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
[DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
[DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
[DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
[DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
[DllImport("user32.dll")] public static extern void mouse_event(int f, int dx, int dy, int d, UIntPtr e);
[DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint f);
[DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
public struct RECT { public int L, T, R, B; }
public struct POINT { public int X, Y; }
'@

function Start-Qlr { param([string[]]$ArgList = @())
    $cmd = '"' + $app + '"'
    if ($ArgList.Count -gt 0) { $cmd += " " + (($ArgList | ForEach-Object { '"' + $_ + '"' }) -join " ") }
    $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $cmd }
    if ($r.ReturnValue -ne 0) { throw "WMI 创建进程失败，ReturnValue=$($r.ReturnValue)" }
    [int]$r.ProcessId
}

function Get-Preview { param([int]$ProcessId)
    $script:found = [IntPtr]::Zero
    $cb = [AV.Win+EnumWinProc]{
        param($h, $l)
        if ([AV.Win]::IsWindowVisible($h)) {
            $sb = [System.Text.StringBuilder]::new(256)
            [void][AV.Win]::GetClassName($h, $sb, 256)
            if ($sb.ToString() -eq "QLRust.PreviewWindow") { $script:found = $h; return $false }
        }
        return $true
    }
    [void][AV.Win]::EnumWindows($cb, [IntPtr]::Zero)
    $script:found
}

$results = [System.Collections.Generic.List[object]]::new()
function Check { param([string]$Name, [bool]$Ok, [string]$Detail = "")
    $results.Add([pscustomobject]@{ 检查 = $Name; 结果 = $(if ($Ok) { "PASS" } else { "FAIL" }); 细节 = $Detail }) }
function Skip { param([string]$Name, [string]$Why = "")
    $results.Add([pscustomobject]@{ 检查 = $Name; 结果 = "SKIP"; 细节 = $Why }) }

function New-Zip { param([string]$Path, [int]$Dirs, [int]$FilesPerDir)
    if (Test-Path $Path) { Remove-Item $Path -Force }
    $src = Join-Path $env:TEMP ("zipgen-" + [guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $src -Force | Out-Null
    "root file" | Set-Content (Join-Path $src "LICENSE")
    for ($d = 1; $d -le $Dirs; $d++) {
        $dir = Join-Path $src ("dir{0:d2}" -f $d)
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
        for ($f = 1; $f -le $FilesPerDir; $f++) {
            ("row {0}-{1}" -f $f, ("x" * ($f * 8))) | Set-Content (Join-Path $dir ("file{0:d2}.txt" -f $f))
        }
    }
    Compress-Archive -Path (Join-Path $src "*") -DestinationPath $Path -Force
    Remove-Item $src -Recurse -Force
}

function Read-Log { if (Test-Path $logFile) { Get-Content $logFile -Raw } else { "" } }

function Wait-Log { param([string]$Pattern, [int]$TimeoutMs = 4000)
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $TimeoutMs) {
        if ((Read-Log) -match $Pattern) { return $true }
        Start-Sleep -Milliseconds 60
    }
    return $false
}

# 把窗口挪回主屏：窗口若有一半在屏幕外，Windows 只画可见部分，
# PrintWindow 抓回来的图会掺着上一帧的"幽灵"（排查时白花过二十分钟）。
function Normalize-Position { param([IntPtr]$Hwnd)
    $r = New-Object AV.Win+RECT
    [void][AV.Win]::GetWindowRect($Hwnd, [ref]$r)
    $sw = [AV.Win]::GetSystemMetrics(0)
    $sh = [AV.Win]::GetSystemMetrics(1)
    if ($r.R -gt $sw -or $r.B -gt $sh -or $r.L -lt 0 -or $r.T -lt 0) {
        $w = $r.R - $r.L
        $h = $r.B - $r.T
        [void][AV.Win]::SetWindowPos($Hwnd, [IntPtr]::Zero, 60, 40, $w, $h, 0x0004)
        Start-Sleep -Milliseconds 250
    }
}

function Save-Shot { param([IntPtr]$Hwnd, [string]$Name)
    Add-Type -AssemblyName System.Drawing
    $r = New-Object AV.Win+RECT
    [void][AV.Win]::GetWindowRect($Hwnd, [ref]$r)
    $w = $r.R - $r.L; $h = $r.B - $r.T
    $bmp = New-Object System.Drawing.Bitmap($w, $h)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    [void][AV.Win]::PrintWindow($Hwnd, $hdc, 2)   # 2 = PW_RENDERFULLCONTENT
    $g.ReleaseHdc($hdc)
    $file = Join-Path $OutDir $Name
    $bmp.Save($file, [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bmp.Dispose()
    $file
}

# 行高/顶栏高度必须和 `window::LIST_ROW_HEIGHT` / `window::CHROME_HEIGHT` 对得上；
# 这里只用来算"点第几行"，数值不对会在日志里立刻看出来（点错行 → 名字对不上）。
$rowHeight = 26
$chromeHeight = 30
$paddingTop = 16
function Click-Row { param([IntPtr]$Hwnd, [int]$Row)
    $pt = New-Object AV.Win+POINT
    [void][AV.Win]::ClientToScreen($Hwnd, [ref]$pt)
    $y = $pt.Y + $chromeHeight + $paddingTop + ($Row * $rowHeight) + [int]($rowHeight / 2)
    [void][AV.Win]::SetCursorPos($pt.X + 120, $y)
    Start-Sleep -Milliseconds 180
    [void][AV.Win]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 120
    [void][AV.Win]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 350
}

# ---- 0. 清理 + 夹具 ----
Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 250
$fixtures = Join-Path $work "fixtures"
New-Item -ItemType Directory -Path $fixtures -Force | Out-Null
$zip = Join-Path $fixtures "sample.zip"
New-Zip -Path $zip -Dirs 3 -FilesPerDir 3
$big = Join-Path $fixtures "big.zip"
New-Zip -Path $big -Dirs 5 -FilesPerDir 12
foreach ($ext in "cbz", "jar", "war") { Copy-Item $zip (Join-Path $fixtures "sample.$ext") -Force }
$broken = Join-Path $fixtures "broken.zip"
Set-Content -Path $broken -Value "this is definitely not a zip file" -Encoding ASCII

# ---- 1. 冷启动 + SHOW .zip ----
$hostPid = Start-Qlr
Start-Sleep -Milliseconds 900
Start-Qlr -ArgList @($zip) | Out-Null
Check "`.zip 进内置列表视图" (Wait-Log "SHOW archive") ""
$text = Read-Log
Check "解析出目录树" ($text -match "ARCHIVE \d+ 个文件夹 · \d+ 个文件 · 解压后 .*（目录树 \d+ 个节点）")

$hwnd = Get-Preview -ProcessId $hostPid
Check "预览窗口可见" ($hwnd -ne [IntPtr]::Zero)
if ($hwnd -eq [IntPtr]::Zero) { $results | Format-Table -AutoSize; exit 1 }
Normalize-Position -Hwnd $hwnd
$shot1 = Save-Shot -Hwnd $hwnd -Name "01-expanded.png"

# 截图里应该有三列内容：名字（左）、大小（中）、时间（右）
Add-Type -AssemblyName System.Drawing
$img = [System.Drawing.Bitmap]::new($shot1)
$hasName = $false; $hasSize = $false; $hasTime = $false
for ($y = 40; $y -lt [Math]::Min($img.Height, 400); $y++) {
    for ($x = 20; $x -lt 200; $x++) { if ($img.GetPixel($x, $y).R -lt 120) { $hasName = $true } }
    for ($x = 600; $x -lt 700; $x++) { if ($img.GetPixel($x, $y).R -lt 200) { $hasSize = $true } }
    for ($x = 700; $x -lt 860; $x++) { if ($img.GetPixel($x, $y).R -lt 200) { $hasTime = $true } }
}
$img.Dispose()
Check "列表三列都画出来了" ($hasName -and $hasSize -and $hasTime) ("名字={0} 大小={1} 时间={2} 截图={3}" -f $hasName, $hasSize, $hasTime, $shot1)

# ---- 2. 点击折叠 / 展开（第 0 行是第一个目录）----
Click-Row -Hwnd $hwnd -Row 0
$collapsed = Wait-Log "ARCHIVE 点击 .*展开=false" 2500
Check "点目录行折叠" $collapsed
$shot2 = Save-Shot -Hwnd $hwnd -Name "02-collapsed.png"
Click-Row -Hwnd $hwnd -Row 0
Check "再点同目录展开" (Wait-Log "ARCHIVE 点击 .*展开=true" 2500)

# ---- 3. 滚轮按行滚动 ----
Start-Qlr -ArgList @($big) | Out-Null
Start-Sleep -Milliseconds 1200
$hwnd = Get-Preview -ProcessId $hostPid
Normalize-Position -Hwnd $hwnd
$shot3 = Save-Shot -Hwnd $hwnd -Name "03-big-top.png"
& $app --wheel -720 2>&1 | Out-Null
$scrolled = Wait-Log "ARCHIVE 滚动" 2500
Check "滚轮按行滚动" $scrolled
Start-Sleep -Milliseconds 300
$shot4 = Save-Shot -Hwnd $hwnd -Name "04-big-scrolled.png"
Check "滚动前后画面不同" ((Get-FileHash $shot3).Hash -ne (Get-FileHash $shot4).Hash)

# ---- 4. cbz / jar / war 也走同一条通道 ----
$before = (Read-Log)
foreach ($ext in "cbz", "jar", "war") {
    Start-Qlr -ArgList @(Join-Path $fixtures "sample.$ext") | Out-Null
    Start-Sleep -Milliseconds 800
}
$after = Read-Log
$hits = ([regex]::Matches($after, "SHOW archive .*sample\.(cbz|jar|war)")).Count
Check ".cbz/.jar/.war 内置接管" ($hits -ge 3) ("命中 {0} 次" -f $hits)

# ---- 5. 坏包按不支持处理 ----
Start-Qlr -ArgList @($broken) | Out-Null
Check "坏包解析失败不显示" (Wait-Log "压缩包解析失败" 2500)
$afterBroken = Read-Log
Check "坏包没走渲染通道" ($afterBroken -notmatch "SHOW archive .*broken\.zip")

# ---- 6. 收尾 ----
& $app --quit 2>&1 | Out-Null
Start-Sleep -Milliseconds 600

$results | Format-Table -AutoSize
$failed = ($results | Where-Object { $_.结果 -eq "FAIL" }).Count
$passed = ($results | Where-Object { $_.结果 -eq "PASS" }).Count
Write-Output ("合计：{0} PASS · {1} FAIL（截图在 {2}）" -f $passed, $failed, $OutDir)
exit $(if ($failed -gt 0) { 1 } else { 0 })
