# Office 预览（1.7.4）端到端验证：顶栏标题 / PPT 翻页接管 / 拖边改窗口大小
#
# 验证的事（全部靠日志 + PrintWindow 截图，不靠眼睛）：
#   1. 顶栏标题的"时序"：`标题栏 ← <文件名>` 必须出现在 `SHELL 预览已接管` **之前**
#      ——这是 1.7.4 那个"顶栏一直是上一个文件名"的根因（跨进程 COM 泵消息时按旧标题
#      画了一帧，之后再没有 invalidate 去纠正）。
#   2. docx / pptx 都由**系统预览处理器**画（`SHELL 预览已接管`），且内容不是空白。
#   3. `.pptx` 页数被我们数出来（`SHELL 接管翻页：共 N 页`），滚轮/PageDown 能翻到 N，
#      **再翻一次停在 N 并给提示**（`已是最后一页`），画面不是 PowerPoint 的黑屏"放映结束"。
#   4. 拖右下角改窗口大小对 **pptx / docx 都有效**（接管输入之前，子窗口会吃掉边缘的鼠标）。
#   5. 改大小后日志里的内容区高度 = 客户区高度 − 顶栏 30px（1.7.4 修掉的 30px 漂移）。
#
# 用法：
#   pwsh -NoProfile -File scripts/verify-office.ps1
#   pwsh -NoProfile -File scripts/verify-office.ps1 -Exe target\release\qlrust.exe -OutDir ..\_bench\office
#   pwsh -NoProfile -File scripts/verify-office.ps1 -Fixtures D:\office-samples
# 退出码：0 = 全过；1 = 有失败项。
#
# 前置：夹具目录里要有 `plugin-demo.docx` / `plugin-demo.pptx`（默认取
# `$env:QLRUST_FIXTURES`，否则 `<桌面>\qlrust-dev`；可用 `..\_bench\make-office-fixtures.py` 重造）。
# 缺夹具 → 整段 SKIP（不算失败）。拖边那两项需要**真实桌面会话**（注入真鼠标事件）。

param(
    [string]$Exe = "target\release\qlrust.exe",
    [string]$OutDir = "..\_bench\office",
    [string]$Fixtures = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# 夹具目录：显式参数 > 环境变量 > 仓库旁的开发夹具目录（发布仓库不带夹具，缺了就 SKIP）。
if (-not $Fixtures) {
    $Fixtures = if ($env:QLRUST_FIXTURES) { $env:QLRUST_FIXTURES } else { Join-Path $root "..\..\qlrust-dev" }
}

if (-not (Test-Path $Exe)) { throw "找不到可执行体：$Exe（先 cargo build --release）" }
$Exe = (Resolve-Path $Exe).Path
if (-not (Test-Path $OutDir)) { New-Item -ItemType Directory -Path $OutDir -Force | Out-Null }
$OutDir = (Resolve-Path $OutDir).Path

# 便携模式（数据写在 exe 同目录）。副本放 %TEMP%：DSH 工作目录里的 exe 会被打 Low 完整性
# 标签，Shell COM / 托盘会被 UIPI 拒（CONVENTIONS #29）。
$work = Join-Path $env:TEMP "qlrust-office-verify"
if (Test-Path $work) {
    Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 250
    Remove-Item $work -Recurse -Force
}
New-Item -ItemType Directory -Path $work -Force | Out-Null
$app = Join-Path $work "qlrust.exe"
Copy-Item $Exe $app
New-Item -ItemType File -Path (Join-Path $work "portable.lock") | Out-Null
# 插件目录要跟着铺：真实安装形态里 office 插件也在（它注册了 docx/pptx → 预览才进得来，
# 否则 `SHOW 拒绝 kind=unsupported`），得证明"处理器优先、插件不抢"。
# cargo 把插件 dll 放在 target\release 根上，装到 `<便携目录>\plugins\` 才是运行时形态。
$srcDlls = @(Get-ChildItem (Join-Path $root "target\release") -Filter "qlr_plugin_*.dll" -ErrorAction SilentlyContinue)
if ($srcDlls.Count -gt 0) {
    $dstPlugins = Join-Path $work "plugins"
    New-Item -ItemType Directory -Path $dstPlugins -Force | Out-Null
    $srcDlls | ForEach-Object { Copy-Item $_.FullName $dstPlugins -Force }
}
$logFile = Join-Path $work "UserData\logs\app.log"
# 点击/翻页/改大小的证据都在 debug 级（`SHELL 翻页`、`标题栏 ←`）。
New-Item -ItemType Directory -Path (Join-Path $work "UserData") -Force | Out-Null
'{"log_level":"debug"}' | Set-Content (Join-Path $work "UserData\config.json") -Encoding UTF8

Add-Type -Namespace AV -Name Win -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumWinProc cb, IntPtr p);
[DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr h, EnumWinProc cb, IntPtr p);
public delegate bool EnumWinProc(IntPtr hWnd, IntPtr lParam);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, System.Text.StringBuilder s, int n);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
[DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
[DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
[DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
[DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
[DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
[DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
[DllImport("user32.dll")] public static extern void mouse_event(int f, int dx, int dy, int d, UIntPtr e);
[DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint f);
[DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
[DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
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

function Get-Preview {
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

# 预览窗可能还没显示出来（启动/转交有延迟），最多等 6 s。
function Wait-Preview { param([int]$TimeoutMs = 6000)
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $TimeoutMs) {
        $h = Get-Preview
        if ($h -ne [IntPtr]::Zero) { return $h }
        Start-Sleep -Milliseconds 150
    }
    [IntPtr]::Zero
}

# 预览窗里**可见**的子窗口（处理器铺满内容区的那层 + 我们的宿主）。
# 换到不走处理器的文件之后，它们必须全部消失——留着就是"正文还是上一个文件"。
function Get-VisibleChildren { param([IntPtr]$Hwnd)
    $script:kids = @()
    $cb = [AV.Win+EnumWinProc]{
        param($h, $l)
        if ([AV.Win]::IsWindowVisible($h)) { $script:kids += $h }
        return $true
    }
    [void][AV.Win]::EnumChildWindows($Hwnd, $cb, [IntPtr]::Zero)
    $script:kids
}

$results = [System.Collections.Generic.List[object]]::new()
function Check { param([string]$Name, [bool]$Ok, [string]$Detail = "")
    $results.Add([pscustomobject]@{ 检查 = $Name; 结果 = $(if ($Ok) { "PASS" } else { "FAIL" }); 细节 = $Detail }) }
function Skip { param([string]$Name, [string]$Why = "")
    $results.Add([pscustomobject]@{ 检查 = $Name; 结果 = "SKIP"; 细节 = $Why }) }

function Read-Log { if (Test-Path $logFile) { Get-Content $logFile -Raw } else { "" } }
function Wait-Log { param([string]$Pattern, [int]$TimeoutMs = 4000)
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $TimeoutMs) {
        if ((Read-Log) -match $Pattern) { return $true }
        Start-Sleep -Milliseconds 60
    }
    return $false
}

# 窗口若有一半在屏幕外，Windows 只画可见部分，PrintWindow 会抓到"新画一半 + 旧一半"的混合帧
# （CONVENTIONS #129）。所有截图前先挪回主屏。
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
    if ($Hwnd -eq [IntPtr]::Zero -or -not [AV.Win]::IsWindow($Hwnd)) {
        throw "Save-Shot($Name)：预览窗句柄无效（$Hwnd）"
    }
    $r = New-Object AV.Win+RECT
    [void][AV.Win]::GetWindowRect($Hwnd, [ref]$r)
    $w = $r.R - $r.L; $h = $r.B - $r.T
    if ($w -le 0 -or $h -le 0) { throw "Save-Shot($Name)：窗口矩形 ${w}x${h}" }
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

# 内容区的"墨水密度"：PowerPoint 黑屏"放映结束"几乎是全黑一片，正常幻灯片有白底和图形。
# 判据用**亮像素**占比：正常页 > 15%，黑屏 ≈ 0。
function Get-BrightRatio { param([string]$File, [int]$Top = 40)
    Add-Type -AssemblyName System.Drawing
    $img = [System.Drawing.Bitmap]::new($File)
    $bright = 0; $total = 0
    for ($y = $Top; $y -lt $img.Height; $y += 6) {
        for ($x = 4; $x -lt $img.Width; $x += 6) {
            $p = $img.GetPixel($x, $y)
            if ($p.R -gt 140 -and $p.G -gt 140 -and $p.B -gt 140) { $bright++ }
            $total++
        }
    }
    $img.Dispose()
    if ($total -eq 0) { return 0 }
    [math]::Round($bright / $total, 3)
}

$chromeHeight = 30
# 拖右下角：先挪回主屏（否则点到的可能是别的窗口），按下后分几步移动，
# 最后校对 WindowFromPoint 确实是预览窗自己（CONVENTIONS #129 的同款前置检查）。
function Drag-Corner { param([IntPtr]$Hwnd, [int]$Dx = 120, [int]$Dy = 90)
    Normalize-Position -Hwnd $Hwnd
    $r0 = New-Object AV.Win+RECT
    [void][AV.Win]::GetWindowRect($Hwnd, [ref]$r0)
    $x = $r0.R - 3
    $y = $r0.B - 3
    $pt = New-Object AV.Win+POINT
    $pt.X = $x; $pt.Y = $y
    $hit = [AV.Win]::WindowFromPoint($pt)
    [void][AV.Win]::SetCursorPos($x, $y)
    Start-Sleep -Milliseconds 200
    [void][AV.Win]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 120
    for ($i = 1; $i -le 4; $i++) {
        [void][AV.Win]::SetCursorPos($x + [int]($Dx * $i / 4), $y + [int]($Dy * $i / 4))
        Start-Sleep -Milliseconds 90
    }
    [void][AV.Win]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 500
    $r1 = New-Object AV.Win+RECT
    [void][AV.Win]::GetWindowRect($Hwnd, [ref]$r1)
    [pscustomobject]@{
        W0 = $r0.R - $r0.L; H0 = $r0.B - $r0.T
        W1 = $r1.R - $r1.L; H1 = $r1.B - $r1.T
        Hit = ($hit -eq $Hwnd)
    }
}

# ---- 0. 夹具 ----
Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 250
$docx = Join-Path $Fixtures "plugin-demo.docx"
$pptx = Join-Path $Fixtures "plugin-demo.pptx"
$haveFixtures = (Test-Path $docx) -and (Test-Path $pptx)
if (-not $haveFixtures) {
    Skip "Office 预览（全部）" "缺夹具 $docx / $pptx"
    $results | Format-Table -AutoSize
    Write-Output "合计：0 PASS · 0 FAIL（夹具缺失，整段跳过）"
    exit 0
}

# 换文件那两项还要一个压缩包和一张图片：优先用夹具目录里的，没有就现场造。
$zipLike = Join-Path $Fixtures "tree-demo.zip"
if (-not (Test-Path $zipLike)) {
    $zipLike = Join-Path $work "switch-demo.zip"
    $src = Join-Path $work "switch-src"
    New-Item -ItemType Directory -Path (Join-Path $src "docs") -Force | Out-Null
    "hello" | Set-Content (Join-Path $src "README.txt")
    "world" | Set-Content (Join-Path $src "docs\note.txt")
    Compress-Archive -Path (Join-Path $src "*") -DestinationPath $zipLike -Force
}
$pngLike = Join-Path $Fixtures "png_1920x1080.png"
if (-not (Test-Path $pngLike)) {
    $pngLike = Join-Path $work "switch-demo.png"
    Add-Type -AssemblyName System.Drawing
    $bmp = New-Object System.Drawing.Bitmap(800, 600)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.Clear([System.Drawing.Color]::FromArgb(240, 240, 245))
    $g.DrawString("switch demo", (New-Object System.Drawing.Font("Arial", 32)), [System.Drawing.Brushes]::Black, 40, 40)
    $g.Dispose()
    $bmp.Save($pngLike, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
}

$hostPid = Start-Qlr
Start-Sleep -Milliseconds 900

# ---- 1. docx：标题时序 + 处理器接管 + 内容非空白 ----
Start-Qlr -ArgList @($docx) | Out-Null
Check "docx 由系统处理器绘制" (Wait-Log "SHELL 预览已接管 .*plugin-demo\.docx" 12000)
$log1 = Read-Log
$titleIdx = $log1.IndexOf("标题栏 ← plugin-demo.docx")
$shellIdx = $log1.LastIndexOf("SHELL 预览已接管")
Check "标题在接管之前就设好（时序）" ($titleIdx -ge 0 -and $titleIdx -lt $shellIdx) ("标题@{0} 接管@{1}" -f $titleIdx, $shellIdx)

$hwnd = Wait-Preview 8000
Check "预览窗口可见" ($hwnd -ne [IntPtr]::Zero)
if ($hwnd -eq [IntPtr]::Zero) { $results | Format-Table -AutoSize; exit 1 }
Normalize-Position -Hwnd $hwnd
Start-Sleep -Milliseconds 600
$shot1 = Save-Shot -Hwnd $hwnd -Name "01-docx.png"
$ratio1 = Get-BrightRatio -File $shot1 -Top 40
Check "docx 内容画出来了" ($ratio1 -gt 0.15) ("亮像素占比 $ratio1，截图 $shot1")

# ---- 2. docx 上拖右下角 ----
$d1 = Drag-Corner -Hwnd $hwnd -Dx 100 -Dy 70
Check "docx 上拖边能改大小" (($d1.W1 - $d1.W0 -ge 60) -and ($d1.H1 - $d1.H0 -ge 40)) ("{0}x{1} → {2}x{3}" -f $d1.W0, $d1.H0, $d1.W1, $d1.H1)
$log2 = Read-Log
Check "改大小后内容高度 = 客户区−30" ($log2 -match ("RESIZE 结束 {0}x{1}" -f $d1.W1, ($d1.H1 - $chromeHeight))) ("期望 {0}x{1}" -f $d1.W1, ($d1.H1 - $chromeHeight))
# 1.8.0：拖边结束后要把新矩形告诉系统处理器（锁外调 SetRect），否则右边会留一条白（CONVENTIONS #145）。
$waited = 0
while ($waited -lt 60 -and -not ((Read-Log) -match '处理器重新对齐到')) {
    Start-Sleep -Milliseconds 100
    $waited++
}
Check "拖边后处理器重新对齐尺寸" ((Read-Log) -match '处理器重新对齐到') "等待 $($waited * 100) ms"

# ---- 3. pptx：数页 + 翻页 + 到最后一页停住 ----
Start-Qlr -ArgList @($pptx) | Out-Null
Check "pptx 由系统处理器绘制" (Wait-Log "SHELL 预览已接管 .*plugin-demo\.pptx" 12000)
Check "数出幻灯片页数" (Wait-Log "SHELL 接管翻页：共 \d+ 页" 12000)
$pages = 0
$m = [regex]::Match((Read-Log), "SHELL 接管翻页：共 (\d+) 页")
if ($m.Success) { $pages = [int]$m.Groups[1].Value }
Check "页数 ≥ 2（够翻页）" ($pages -ge 2) ("共 $pages 页")

$hwnd = Wait-Preview 8000
Check "pptx 预览窗口可见" ($hwnd -ne [IntPtr]::Zero)
if ($hwnd -eq [IntPtr]::Zero) { $results | Format-Table -AutoSize; exit 1 }
Normalize-Position -Hwnd $hwnd
Start-Sleep -Milliseconds 700
$shotP1 = Save-Shot -Hwnd $hwnd -Name "02-ppt-first.png"

# 滚轮翻页：CLI `--wheel` 走的是同一个 `on_wheel`（预览窗是 WS_EX_NOACTIVATE，收不到物理滚轮）
for ($i = 2; $i -le [Math]::Min($pages, 3); $i++) {
    & $app --wheel -120 2>&1 | Out-Null
    [void](Wait-Log ("SHELL 翻页：{0}/{1}" -f $i, $pages) 3000)
}
Check "滚轮到第 2 页" ((Read-Log) -match ("SHELL 翻页：2/{0}" -f $pages)) ""
$shotP2 = Save-Shot -Hwnd $hwnd -Name "03-ppt-second.png"
Check "第 1/2 页画面不同" ((Get-FileHash $shotP1).Hash -ne (Get-FileHash $shotP2).Hash)

# 一直翻到最后一页，再多翻 2 次
for ($i = 0; $i -lt ($pages + 2); $i++) { & $app --wheel -120 2>&1 | Out-Null; Start-Sleep -Milliseconds 260 }
$lastHit = Wait-Log "已是最后一页" 4000
Check "到头停住并给提示" $lastHit ""
Start-Sleep -Milliseconds 400
$shotP3 = Save-Shot -Hwnd $hwnd -Name "04-ppt-last.png"
$ratio3 = Get-BrightRatio -File $shotP3 -Top 40
# PowerPoint 放映过最后一页会切到黑屏"放映结束"（几乎全黑）；停住不翻则还是白底幻灯片
Check "最后一页不是黑屏" ($ratio3 -gt 0.15) ("亮像素占比 $ratio3")

# ---- 4. pptx 上拖边 ----
$d2 = Drag-Corner -Hwnd $hwnd -Dx 120 -Dy 80
Check "pptx 上拖边能改大小" (($d2.W1 - $d2.W0 -ge 60) -and ($d2.H1 - $d2.H0 -ge 40)) ("{0}x{1} → {2}x{3}" -f $d2.W0, $d2.H0, $d2.W1, $d2.H1)
# 接管输入之前边缘的鼠标会被处理器的子窗口吃掉（WindowFromPoint ≠ 预览窗），这条是回归哨兵
Check "边缘命中预览窗自己（输入已接管）" $d2.Hit ""

# ---- 5. 换文件：从 Office 文件切到"不走处理器"的文件，处理器必须当场卸掉 ----
#
# 1.7.5：`open_shell_preview` 只在"这次也用处理器"时收尸，于是 pptx → zip 之后
# PowerPoint 的子窗口会留在预览窗里继续盖着内容（顶栏是新的、正文是旧的；
# 拖动时那片残留还会画成竖条纹——用户报的就是这个）。
#
# 注意：对**同一个文件**再 Show 一次等于"开关切换"（窗口会被收起），所以这里换文件前
# 先换成别的文件，别拿同一个路径再 show 一遍。
Start-Qlr -ArgList @($zipLike) | Out-Null
Start-Sleep -Milliseconds 800
Start-Qlr -ArgList @($pptx) | Out-Null
[void](Wait-Log "SHELL 预览已接管 .*plugin-demo\.pptx" 8000)
$hwnd = Wait-Preview
if ($hwnd -eq [IntPtr]::Zero) {
    Skip "换文件：卸掉处理器" "预览窗没出现（本机可能有别的会话在开关窗口）"
} else {
    Normalize-Position -Hwnd $hwnd
    Start-Sleep -Milliseconds 900
    $kidsBefore = (Get-VisibleChildren -Hwnd $hwnd).Count
    Check "pptx 预览有处理器子窗口" ($kidsBefore -gt 0) ("子窗口 $kidsBefore 个")

    Start-Qlr -ArgList @($zipLike) | Out-Null
    $dropped = Wait-Log "换文件：卸掉上一个文件的系统预览处理器" 5000
    Check "换文件时卸掉处理器" $dropped ""
    Start-Sleep -Milliseconds 900
    $kidsAfter = (Get-VisibleChildren -Hwnd $hwnd).Count
    Check "切到压缩包后没有残留子窗口" ($kidsAfter -eq 0) ("子窗口 $kidsAfter 个")
    $shotSwitch = Save-Shot -Hwnd $hwnd -Name "05-after-switch.png"
    $ratioSwitch = Get-BrightRatio -File $shotSwitch -Top 40
    Check "切过来后画的是新内容" ($ratioSwitch -gt 0.15) ("亮像素占比 $ratioSwitch")
}

# 再验一次主路径（非压缩包）：pptx → 图片，走的是 `!shell_wanted` 那条分支。
$before = (Read-Log).Length
Start-Qlr -ArgList @($pptx) | Out-Null
[void](Wait-Log "SHELL 预览已接管 .*plugin-demo\.pptx" 8000)
Start-Sleep -Milliseconds 1400
Start-Qlr -ArgList @($pngLike) | Out-Null
Start-Sleep -Milliseconds 1400
$after = Read-Log
$fresh = $after.Substring([Math]::Min($before, $after.Length))
$dropped2 = ([regex]::Matches($fresh, "换文件：卸掉上一个文件的系统预览处理器")).Count
Check "切到图片时也卸掉处理器" ($dropped2 -ge 1) ("命中 {0} 次" -f $dropped2)
$hwndImg = Wait-Preview
if ($hwndImg -eq [IntPtr]::Zero) {
    Skip "切到图片后没有残留子窗口" "预览窗没出现"
} else {
    $kidsImg = (Get-VisibleChildren -Hwnd $hwndImg).Count
    Check "切到图片后没有残留子窗口" ($kidsImg -eq 0) ("子窗口 $kidsImg 个")
}

# ---- 6. 收尾 ----
& $app --quit 2>&1 | Out-Null
Start-Sleep -Milliseconds 600

$results | Format-Table -AutoSize
$failed = ($results | Where-Object { $_.结果 -eq "FAIL" }).Count
$passed = ($results | Where-Object { $_.结果 -eq "PASS" }).Count
Write-Output ("合计：{0} PASS · {1} FAIL（截图在 {2}）" -f $passed, $failed, $OutDir)
exit $(if ($failed -gt 0) { 1 } else { 0 })
