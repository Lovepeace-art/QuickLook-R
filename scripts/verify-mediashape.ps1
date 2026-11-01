# 1.8.1 形状/交互专项验证：Excel 能点选单元格 + 图片/视频窗口比例锁死 + Excel 改大小无白边
#
# 验证的事（全靠日志 + PrintWindow 截图 + 真鼠标注入，不靠眼睛）：
#   1. Excel 预览打开后**没有文字占位页**：第一帧到内容帧之间，内容区不该出现"文件信息页"；
#      用日志判定 —— 交给系统处理器时不该有 `正在渲染` 的占位（1.8.1 起直接清空）；
#   2. Excel 里**左键点一下就能选中单元格**（1.8.1 用户反馈"焦点进去后选不中"）：
#      注入一次点击，点击前后截图像素差 > 0.3% 即视为选区变了；
#   3. Excel 拖右边改大小后**右缘没有一条白**（1.8.0 修的，别退化）：右缘 40px 纯白占比 < 85%；
#   4. 图片窗口比例 = 素材比例，**拖右边/下边之后比例不变**（1.8.1：取消窗口拉伸缩放）；
#   5. 视频同图片，且内容高度要额外扣掉底部进度条（`AUDIO_BAR_H = 52`）。
#
# 用法：
#   pwsh -NoProfile -File scripts/verify-mediashape.ps1
#   pwsh -NoProfile -File scripts/verify-mediashape.ps1 -Exe target\release\qlrust.exe -OutDir ..\_bench\shape
# 退出码：0 = 全过；1 = 有失败项。
#
# 夹具默认取 `..\..\qlrust-dev\compare\` 里的 big-sheet.xlsx / img-photo.jpg / video-demo.mp4；
# 缺哪个就把相关检查 SKIP（其它照跑）。

param(
    [string]$Exe = "target\release\qlrust.exe",
    [string]$OutDir = "..\_bench\shape",
    [string]$Sheet = "",
    [string]$Image = "",
    [string]$Video = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not (Test-Path $Exe)) { throw "找不到可执行体：$Exe（先 cargo build --release）" }
$Exe = (Resolve-Path $Exe).Path
if (-not (Test-Path $OutDir)) { New-Item -ItemType Directory -Path $OutDir -Force | Out-Null }
$OutDir = (Resolve-Path $OutDir).Path

$devDir = Join-Path (Split-Path -Parent (Split-Path -Parent $root)) "qlrust-dev\compare"
if (-not $Sheet) { $Sheet = Join-Path $devDir "big-sheet.xlsx" }
if (-not $Image) { $Image = Join-Path $devDir "img-photo.jpg" }
if (-not $Video) { $Video = Join-Path $devDir "video-demo.mp4" }

# 便携模式副本放 %TEMP%（工作目录里的 exe 带 Low 完整性标签，CONVENTIONS #29）。
$work = Join-Path $env:TEMP "qlrust-shape-verify"
if (Test-Path $work) {
    Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 300
    Remove-Item $work -Recurse -Force
}
New-Item -ItemType Directory -Path $work -Force | Out-Null
$app = Join-Path $work "qlrust.exe"
Copy-Item $Exe $app
New-Item -ItemType File -Path (Join-Path $work "portable.lock") | Out-Null
# 插件目录要跟着铺：`.xlsx` 在真实安装形态里由 office 插件注册，少了插件
# `format::kind_for_path` 会退回 unsupported → `SHOW 拒绝 kind=unsupported`，
# 系统预览处理器根本不会被叫起来（verify-office.ps1 同款处理）。
$srcDlls = @(Get-ChildItem (Join-Path $root "target\release") -Filter "qlr_plugin_*.dll" -ErrorAction SilentlyContinue)
if ($srcDlls.Count -gt 0) {
    $dstPlugins = Join-Path $work "plugins"
    New-Item -ItemType Directory -Path $dstPlugins -Force | Out-Null
    $srcDlls | ForEach-Object { Copy-Item $_.FullName $dstPlugins -Force }
}

New-Item -ItemType Directory -Path (Join-Path $work "UserData") -Force | Out-Null
'{"log_level":"debug"}' | Set-Content (Join-Path $work "UserData\config.json") -Encoding UTF8
$logFile = Join-Path $work "UserData\logs\app.log"

Add-Type -AssemblyName System.Drawing
Add-Type -Namespace SH -Name Win -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumWinProc cb, IntPtr p);
public delegate bool EnumWinProc(IntPtr hWnd, IntPtr lParam);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, System.Text.StringBuilder s, int n);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
[DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
[DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
[DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
[DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint f);
[DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
[DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
[DllImport("user32.dll")] public static extern void mouse_event(int flags, int dx, int dy, int data, IntPtr extra);
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

function Stop-Qlr {
    Get-CimInstance Win32_Process -Filter "Name='qlrust.exe'" | ForEach-Object {
        Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
    }
    Start-Sleep -Milliseconds 900
}

function Get-Preview {
    $script:found = [IntPtr]::Zero
    $cb = [SH.Win+EnumWinProc]{
        param($h, $l)
        if ([SH.Win]::IsWindowVisible($h)) {
            $sb = [System.Text.StringBuilder]::new(256)
            [void][SH.Win]::GetClassName($h, $sb, 256)
            if ($sb.ToString() -eq "QLRust.PreviewWindow") { $script:found = $h; return $false }
        }
        return $true
    }
    [void][SH.Win]::EnumWindows($cb, [IntPtr]::Zero)
    $script:found
}

function Wait-Preview { param([int]$Ms = 8000)
    $deadline = (Get-Date).AddMilliseconds($Ms)
    while ((Get-Date) -lt $deadline) {
        $h = Get-Preview
        if ($h -ne [IntPtr]::Zero) { return $h }
        Start-Sleep -Milliseconds 80
    }
    [IntPtr]::Zero
}

# 窗口摆回主屏（否则 PrintWindow 只抓得到露在屏幕里的那半，CONVENTIONS #129）
function Normalize-Position { param([IntPtr]$H)
    $screenW = [SH.Win]::GetSystemMetrics(0)
    $screenH = [SH.Win]::GetSystemMetrics(1)
    $r = New-Object SH.Win+RECT
    [void][SH.Win]::GetWindowRect($H, [ref]$r)
    $w = $r.R - $r.L
    $hh = $r.B - $r.T
    $x = $r.L
    $y = $r.T
    $changed = $false
    if ($x -lt 0 -or ($x + $w) -gt $screenW -or $y -lt 0 -or ($y + $hh) -gt $screenH) {
        $x = 120
        $y = 90
        $changed = $true
    }
    if ($changed) { [void][SH.Win]::SetWindowPos($H, [IntPtr]::Zero, $x, $y, $w, $hh, 0x0004) }
    Start-Sleep -Milliseconds 250
}

function Shot { param([IntPtr]$H, [string]$Name)
    $r = New-Object SH.Win+RECT
    [void][SH.Win]::GetWindowRect($H, [ref]$r)
    $w = $r.R - $r.L
    $hh = $r.B - $r.T
    if ($w -le 0 -or $hh -le 0) { throw "Shot($Name)：窗口矩形非法 ${w}x${hh}" }
    $bmp = New-Object System.Drawing.Bitmap($w, $hh)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    [void][SH.Win]::PrintWindow($H, $hdc, 2)
    $g.ReleaseHdc($hdc)
    $g.Dispose()
    $file = Join-Path $OutDir "$Name.png"
    $bmp.Save($file, [System.Drawing.Imaging.ImageFormat]::Png)
    return @{ Bitmap = $bmp; File = $file }
}

function Shot-Diff { param($A, $B, [int]$Top = 34)
    $w = [math]::Min($A.Bitmap.Width, $B.Bitmap.Width)
    $h = [math]::Min($A.Bitmap.Height, $B.Bitmap.Height)
    $diff = 0
    $total = 0
    for ($y = $Top; $y -lt $h; $y += 2) {
        for ($x = 0; $x -lt $w; $x += 2) {
            $total++
            $c1 = $A.Bitmap.GetPixel($x, $y)
            $c2 = $B.Bitmap.GetPixel($x, $y)
            if (([math]::Abs($c1.R - $c2.R) + [math]::Abs($c1.G - $c2.G) + [math]::Abs($c1.B - $c2.B)) -gt 30) { $diff++ }
        }
    }
    if ($total -eq 0) { return 0 }
    return [math]::Round(100.0 * $diff / $total, 2)
}

function Right-White { param($Shot)
    $bmp = $Shot.Bitmap
    $w = $bmp.Width
    $h = $bmp.Height
    $lit = 0
    $total = 0
    for ($x = [math]::Max(0, $w - 40); $x -lt $w; $x += 2) {
        for ($y = 34; $y -lt $h; $y += 2) {
            $total++
            $c = $bmp.GetPixel($x, $y)
            if ($c.R -gt 245 -and $c.G -gt 245 -and $c.B -gt 245) { $lit++ }
        }
    }
    if ($total -eq 0) { return 0 }
    return [math]::Round(100.0 * $lit / $total, 1)
}

# 内容区比例：整窗宽 ÷（客户区高 − 顶栏 30 − 底部进度条）
function Content-Ratio { param([IntPtr]$H, [int]$Bar = 0)
    $r = New-Object SH.Win+RECT
    [void][SH.Win]::GetWindowRect($H, [ref]$r)
    $c = New-Object SH.Win+RECT
    [void][SH.Win]::GetClientRect($H, [ref]$c)
    $cw = $r.R - $r.L
    $ch = ($c.B - $c.T) - 30 - $Bar
    if ($ch -le 0) { return 0 }
    return [math]::Round($cw / $ch, 3)
}

function Win-Size { param([IntPtr]$H)
    $r = New-Object SH.Win+RECT
    [void][SH.Win]::GetWindowRect($H, [ref]$r)
    return @{ W = ($r.R - $r.L); H = ($r.B - $r.T) }
}

function Drag-Edge { param([IntPtr]$H, [string]$Edge, [int]$Delta = 200)
    $r = New-Object SH.Win+RECT
    [void][SH.Win]::GetWindowRect($H, [ref]$r)
    if ($Edge -eq 'right') {
        $x = $r.R - 2
        $y = [int](($r.T + $r.B) / 2)
        [void][SH.Win]::SetCursorPos($x, $y)
        Start-Sleep -Milliseconds 200
        [void][SH.Win]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
        Start-Sleep -Milliseconds 120
        for ($i = 1; $i -le 6; $i++) {
            [void][SH.Win]::SetCursorPos($x + [int]($Delta * $i / 6), $y)
            Start-Sleep -Milliseconds 45
        }
        [void][SH.Win]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
    } else {
        $x = [int](($r.L + $r.R) / 2)
        $y = $r.B - 2
        [void][SH.Win]::SetCursorPos($x, $y)
        Start-Sleep -Milliseconds 200
        [void][SH.Win]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
        Start-Sleep -Milliseconds 120
        for ($i = 1; $i -le 6; $i++) {
            [void][SH.Win]::SetCursorPos($x, $y + [int]($Delta * $i / 6))
            Start-Sleep -Milliseconds 45
        }
        [void][SH.Win]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
    }
    Start-Sleep -Milliseconds 700
}

function Wait-Log { param([string]$Pattern, [int]$Ms = 8000)
    $deadline = (Get-Date).AddMilliseconds($Ms)
    while ((Get-Date) -lt $deadline) {
        if ((Test-Path $logFile) -and (Select-String -Path $logFile -Pattern $Pattern -Quiet)) { return $true }
        Start-Sleep -Milliseconds 120
    }
    $false
}

# ---------------- 1) Excel：占位页 / 点选单元格 / 改大小无白边 ----------------
if (Test-Path $Sheet) {
    Stop-Qlr
    Start-Qlr @('--show', $Sheet) | Out-Null
    $h = Wait-Preview 12000
    Check "Excel 预览窗口出现" ($h -ne [IntPtr]::Zero)
    if ($h -ne [IntPtr]::Zero) {
        Normalize-Position $h
        $ready = Wait-Log 'SHELL 就绪' 12000
        Check "系统预览处理器就绪（SHELL 就绪）" $ready
        Check "交给处理器时没有文字占位页" (-not (Select-String -Path $logFile -Pattern '正在渲染' -Quiet)) `
            "日志里不该有 `正在渲染` 占位"
        Start-Sleep -Milliseconds 500

        # 点一下内容区（相对窗口 210,250 ≈ C11），点击前后画面应当变（选中单元格 + 可能滚动）
        $before = Shot $h "excel-before-click"
        $r = New-Object SH.Win+RECT
        [void][SH.Win]::GetWindowRect($h, [ref]$r)
        $px = $r.L + 210
        $py = $r.T + 250
        [void][SH.Win]::SetCursorPos($px, $py)
        Start-Sleep -Milliseconds 200
        [void][SH.Win]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
        Start-Sleep -Milliseconds 120
        [void][SH.Win]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
        Start-Sleep -Milliseconds 800
        $after = Shot $h "excel-after-click"
        $diff = Shot-Diff $before $after
        Check "Excel 左键点一下就选中单元格" ($diff -gt 0.3) "点击前后画面差 $diff%（>0.3 视为选区变了）"
        $before.Bitmap.Dispose()
        $after.Bitmap.Dispose()

        # 拖右边改大小：右缘不该出现一条白（1.8.0 修的）
        Drag-Edge $h 'right' 220
        $wide = Shot $h "excel-wider"
        $white = Right-White $wide
        Check "Excel 改大小后右缘没有白边" ($white -lt 85.0) "右缘 40px 纯白占比 $white%（<85 视为铺满）"
        $wide.Bitmap.Dispose()
    }
} else {
    Skip "Excel 点选单元格" "没有夹具 $Sheet"
    Skip "Excel 改大小后右缘没有白边" "没有夹具 $Sheet"
    Skip "交给处理器时没有文字占位页" "没有夹具 $Sheet"
}
Stop-Qlr

# ---------------- 2) 图片：窗口比例锁死素材比例 ----------------
if (Test-Path $Image) {
    Start-Qlr @('--show', $Image) | Out-Null
    $h = Wait-Preview 8000
    Check "图片预览窗口出现" ($h -ne [IntPtr]::Zero)
    if ($h -ne [IntPtr]::Zero) {
        Normalize-Position $h
        Start-Sleep -Milliseconds 900
        $open = Content-Ratio $h
        $s0 = Shot $h "image-open"
        $w0 = Win-Size $h
        Drag-Edge $h 'right' 220
        $w1 = Win-Size $h
        $wide = Content-Ratio $h
        $s1 = Shot $h "image-wider"
        Drag-Edge $h 'bottom' 160
        $w2 = Win-Size $h
        $tall = Content-Ratio $h
        $s2 = Shot $h "image-taller"
        Check "图片窗口比例 = 素材比例（16:9 ≈ 1.778）" ([math]::Abs($open - 1.778) -le 0.02) "打开时 $open"
        Check "图片拖右边不改窗口大小（1.8.2）" (($w1.W -le $w0.W + 2) -and ($w1.H -le $w0.H + 2)) "拖前 $($w0.W)x$($w0.H) → 拖后 $($w1.W)x$($w1.H)"
        Check "图片拖下边不改窗口大小（1.8.2）" (($w2.W -le $w0.W + 2) -and ($w2.H -le $w0.H + 2)) "拖前 $($w0.W)x$($w0.H) → 拖后 $($w2.W)x$($w2.H)"
        $s0.Bitmap.Dispose(); $s1.Bitmap.Dispose(); $s2.Bitmap.Dispose()
    }
} else {
    Skip "图片窗口比例锁定" "没有夹具 $Image"
}
Stop-Qlr

# ---------------- 3) 视频：比例锁死（内容高度要扣掉底部进度条） ----------------
if (Test-Path $Video) {
    Start-Qlr @('--show', $Video) | Out-Null
    $h = Wait-Preview 8000
    Check "视频预览窗口出现" ($h -ne [IntPtr]::Zero)
    if ($h -ne [IntPtr]::Zero) {
        Normalize-Position $h
        Start-Sleep -Milliseconds 1200
        $open = Content-Ratio $h 52
        $s0 = Shot $h "video-open"
        $v0 = Win-Size $h
        Drag-Edge $h 'right' 220
        $v1 = Win-Size $h
        $wide = Content-Ratio $h 52
        $s1 = Shot $h "video-wider"
        Drag-Edge $h 'bottom' 160
        $v2 = Win-Size $h
        $tall = Content-Ratio $h 52
        $s2 = Shot $h "video-taller"
        Check "视频窗口比例 = 素材比例（16:9 ≈ 1.778）" ([math]::Abs($open - 1.778) -le 0.02) "打开时 $open（已扣 52px 进度条）"
        Check "视频拖右边不改窗口大小（1.8.2）" (($v1.W -le $v0.W + 2) -and ($v1.H -le $v0.H + 2)) "拖前 $($v0.W)x$($v0.H) → 拖后 $($v1.W)x$($v1.H)"
        Check "视频拖下边不改窗口大小（1.8.2）" (($v2.W -le $v0.W + 2) -and ($v2.H -le $v0.H + 2)) "拖前 $($v0.W)x$($v0.H) → 拖后 $($v2.W)x$($v2.H)"
        $s0.Bitmap.Dispose(); $s1.Bitmap.Dispose(); $s2.Bitmap.Dispose()
    }
} else {
    Skip "视频窗口比例锁定" "没有夹具 $Video"
}
Stop-Qlr

# ---------------- 汇总 ----------------
$results | Format-Table -AutoSize | Out-String -Width 200 | Write-Output
$pass = ($results | Where-Object { $_.结果 -eq 'PASS' }).Count
$fail = ($results | Where-Object { $_.结果 -eq 'FAIL' }).Count
$skip = ($results | Where-Object { $_.结果 -eq 'SKIP' }).Count
"合计：$pass PASS · $fail FAIL · $skip SKIP（截图在 $OutDir）"
$results | Export-Csv (Join-Path $OutDir "results.csv") -NoTypeInformation -Encoding UTF8
if ($fail -gt 0) { exit 1 } else { exit 0 }

