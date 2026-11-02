# 视觉基线截图：把 5 类界面各截一张，作为"改 DPI 前后逐像素对比"的基准（1.8.3 起）。
#
# 用法：
#   pwsh -File scripts\baseline-shot.ps1 -OutDir baseline\before
#   pwsh -File scripts\baseline-shot.ps1 -OutDir baseline\after -KeepConfig
#
# 为什么必须要有它：1.8.3 把 160 处几何量从"物理像素字面量"改成"DIP → 物理像素"，
# 96 DPI 下的硬指标是**一个像素都不许变**。没有基准图，就分不清"没改坏"和"只是看起来像"。
#
# 三个刻意的设计（都是踩过的坑）：
# - 默认先删 config.json：窗口尺寸有记忆（1.0.14）、图片缩放有记忆（1.0.9），
#   残留配置会让两次运行的窗口大小不同，对比就失去意义。
# - 截之前把窗口挪到主屏 (8,8)：窗口一半在屏幕外时 PrintWindow 只拿到可见部分
#   （docs/dev/CONVENTIONS.md #129），会出现"新一半 + 旧一半"的混合帧。
# - **默认把 exe 复制到中性目录再拍**（1.8.3，#152）：仓库目录（Desktop\DSH\...）里的
#   exe 会被系统打上 Low Mandatory Level 标签 → 托盘注册被 UIPI 拒（0x80070005）→
#   1.8.3 起会给一条"托盘图标没加上"的顶栏 toast，于是**第一张截图（text）多一个气泡**，
#   看起来像基线漂移。中性目录里托盘正常、没有 toast，截图才只反映"我们画了什么"。
#   想拍"低完整性环境的样子"（带 toast）就加 -NoNeutral。见 docs/dev/CONVENTIONS.md #152。
param(
    [Parameter(Mandatory = $true)][string]$OutDir,
    # 源目录（默认 target\release）：中性目录是从这里复制过去的
    [string]$ExeDir = "",
    # 保留 UserData\config.json（跑"改前 / 改后"两次都要用同一份配置时加这个）
    [switch]$KeepConfig,
    # 中性目录（放 exe + pdfium + 插件；默认 <桌面>\qlr-ab\base）
    [string]$NeutralDir = "",
    # 就地拍（不做中性目录复制）。只有想复现"托盘失败 + toast"时才用。
    [switch]$NoNeutral
)

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
if (-not $ExeDir) { $ExeDir = Join-Path $root "target\release" }
$ExeDir = (Resolve-Path -LiteralPath $ExeDir).Path
if (-not [System.IO.Path]::IsPathRooted($OutDir)) { $OutDir = Join-Path $root $OutDir }

# ---- 中性目录：把 exe / pdfium / 插件复制出去再拍 ----
# 为什么复制而不是"就地拍"：见文件头 #152 那条。仓库目录跑出来的进程令牌完整性是 low，
# 托盘注册必然失败，第一张截图会多一条 toast——那是环境噪音，不是我们要盯的视觉回归。
if (-not $NoNeutral) {
    if (-not $NeutralDir) { $NeutralDir = Join-Path $env:USERPROFILE "Desktop\qlr-ab\base" }
    New-Item -ItemType Directory -Path $NeutralDir -Force | Out-Null
    $NeutralDir = (Resolve-Path -LiteralPath $NeutralDir).Path
    $srcExe = Join-Path $ExeDir "qlrust.exe"
    if (-not (Test-Path $srcExe)) { throw "找不到 $srcExe，先 cargo build --release" }
    Copy-Item -LiteralPath $srcExe -Destination $NeutralDir -Force
    foreach ($dll in @("pdfium.dll", "qlr_plugin_demo.dll", "qlr_plugin_office.dll")) {
        $p = Join-Path $ExeDir $dll
        if (Test-Path $p) { Copy-Item -LiteralPath $p -Destination $NeutralDir -Force }
    }
    # 便携模式标记：不带它的话配置会落到 %LOCALAPPDATA%，中性目录就没意义了
    if (-not (Test-Path (Join-Path $NeutralDir "portable.lock"))) {
        New-Item -ItemType File -Path (Join-Path $NeutralDir "portable.lock") | Out-Null
    }
    # 每次重拍都清掉上一轮的 UserData（日志/配置/窗口记忆），否则"默认删 config"那一条就白写了
    Remove-Item -LiteralPath (Join-Path $NeutralDir "UserData") -Recurse -Force -ErrorAction SilentlyContinue
    $ExeDir = $NeutralDir
}

$exe = Join-Path $ExeDir "qlrust.exe"
if (-not (Test-Path $exe)) { throw "找不到 $exe，先 cargo build --release" }
$logPath = Join-Path $ExeDir "UserData\logs\app.log"
$configPath = Join-Path $ExeDir "UserData\config.json"
"中性目录 : $ExeDir"

# 夹具目录：与 scripts\verify-*.ps1 同一套回退规则
$fixtures = $env:QLRUST_FIXTURES
if (-not $fixtures) { $fixtures = Join-Path $env:USERPROFILE "Desktop\qlrust-dev" }
if (-not (Test-Path $fixtures)) { throw "找不到夹具目录 $fixtures（用 QLRUST_FIXTURES 指定）" }

New-Item -ItemType Directory -Path $OutDir -Force | Out-Null

Add-Type -AssemblyName System.Drawing
$src = @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public class QlrShot {
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint f);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint f);
  [DllImport("user32.dll")] public static extern int GetDpiForWindow(IntPtr h);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  public static IntPtr Find(string cls) {
    IntPtr found = IntPtr.Zero;
    EnumWindows((h, p) => {
      if (!IsWindowVisible(h)) return true;
      var sb = new StringBuilder(256); GetClassName(h, sb, 256);
      if (sb.ToString() == cls) { found = h; return false; }
      return true;
    }, IntPtr.Zero);
    return found;
  }
}
'@
if (-not ("QlrShot" -as [type])) { Add-Type -TypeDefinition $src }

$PREVIEW = "QLRust.PreviewWindow"
$SETTINGS = "QLRust.SettingsWindow"
$SWP = 0x0015   # NOSIZE | NOZORDER | NOACTIVATE

function Start-Qlr {
    param([string[]]$Arguments)
    $line = '"' + $exe + '" ' + ($Arguments -join ' ')
    $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $line.Trim() }
    return [int]$r.ProcessId
}

function Wait-Window {
    param([string]$Class, [int]$TimeoutMs = 8000)
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $TimeoutMs) {
        $h = [QlrShot]::Find($Class)
        if ($h -ne [IntPtr]::Zero) { return $h }
        Start-Sleep -Milliseconds 120
    }
    return [IntPtr]::Zero
}

# 窗口挪到主屏左上角并等它画完：半屏外的窗口截出来是混合帧（#129）
#
# 还要等**尺寸稳定**（1.8.3 实测）：show 之后窗口还可能被缩放/自然尺寸路径再改一次大小，
# 早 200 ms 截就会拍到"小一号的中间态"（同一套 exe 两次跑出 814x488 和 1589x924）。
# 判据：连续两次采样矩形完全一致才算稳，然后额外再等 250 ms 让绘制落盘。
function Settle-Window {
    param([IntPtr]$Hwnd, [int]$TimeoutMs = 3000)
    [void][QlrShot]::SetWindowPos($Hwnd, [IntPtr]::Zero, 8, 8, 0, 0, $SWP)
    $prev = ""
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $TimeoutMs) {
        $rect = New-Object QlrShot+RECT
        [void][QlrShot]::GetWindowRect($Hwnd, [ref]$rect)
        $now = "{0},{1},{2},{3}" -f $rect.L, $rect.T, $rect.R, $rect.B
        if ($now -eq $prev) { break }
        $prev = $now
        Start-Sleep -Milliseconds 120
    }
    Start-Sleep -Milliseconds 250
}

function Save-Shot {
    param([IntPtr]$Hwnd, [string]$Path)
    # 尺寸可能"刚稳住之后又被改一次"（实测偶发：1589x924 拍成 1271x745 / 814x488）。
    # 所以拍完再比一次矩形：变了就丢掉重拍（最多 3 次），仍然在变就直接报错 ——
    # 宁可让脚本失败，也不要往基准目录里写一张"中间态"的图。
    for ($attempt = 1; $attempt -le 3; $attempt++) {
        $rect = New-Object QlrShot+RECT
        [void][QlrShot]::GetWindowRect($Hwnd, [ref]$rect)
        $w = $rect.R - $rect.L
        $h = $rect.B - $rect.T
        if ($w -le 0 -or $h -le 0) { throw "窗口矩形无效：${w}x${h}" }
        $bmp = New-Object System.Drawing.Bitmap $w, $h
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $dc = $g.GetHdc()
        [void][QlrShot]::PrintWindow($Hwnd, $dc, 2)   # PW_RENDERFULLCONTENT
        $g.ReleaseHdc($dc)
        $g.Dispose()
        $after = New-Object QlrShot+RECT
        [void][QlrShot]::GetWindowRect($Hwnd, [ref]$after)
        $stable = (($after.R - $after.L) -eq $w) -and (($after.B - $after.T) -eq $h)
        if (-not $stable) {
            $bmp.Dispose()
            Start-Sleep -Milliseconds 350
            continue
        }
        $bmp.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
        $bmp.Dispose()
        return [pscustomobject]@{ Width = $w; Height = $h; Left = $rect.L; Top = $rect.T; Dpi = [QlrShot]::GetDpiForWindow($Hwnd) }
    }
    throw "窗口尺寸在截图过程中反复变化（attempt=3，hwnd=$Hwnd）"
}

# ---- 0) 清场 ----
Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force
for ($i = 0; $i -lt 20; $i++) {
    if (-not (Get-Process qlrust -ErrorAction SilentlyContinue)) { break }
    Start-Sleep -Milliseconds 100
}
Remove-Item $logPath -ErrorAction SilentlyContinue
if (-not $KeepConfig) { Remove-Item $configPath -ErrorAction SilentlyContinue }

# ---- 1) 起宿主 ----
[void](Start-Qlr -Arguments @())
for ($i = 0; $i -lt 60; $i++) {
    Start-Sleep -Milliseconds 100
    if ((Test-Path $logPath) -and (Select-String -Path $logPath -Pattern "壳层就绪" -Quiet)) { break }
}

$cases = @(
    @{ Name = "text";    Class = $PREVIEW;  File = "lines-200.txt" },
    @{ Name = "image";   Class = $PREVIEW;  File = "png_1920x1080.png" },
    @{ Name = "pdf";     Class = $PREVIEW;  File = "pdf_8page.pdf"; Settle = 900 },
    @{ Name = "archive"; Class = $PREVIEW;  File = "plugin-demo.zip" },
    @{ Name = "settings"; Class = $SETTINGS; File = "" }
)

$manifest = @()
foreach ($c in $cases) {
    # 收起上一个（设置窗走 WM_CLOSE 等价路径：直接 --hide 只收预览窗，所以设置窗放最后）
    if ($c.Name -ne "settings") {
        [void](Start-Qlr -Arguments @("--hide"))
        Start-Sleep -Milliseconds 300
        $path = Join-Path $fixtures $c.File
        if (-not (Test-Path $path)) { throw "夹具缺失：$path" }
        [void](Start-Qlr -Arguments @('"' + $path + '"'))
    } else {
        [void](Start-Qlr -Arguments @("--hide"))
        Start-Sleep -Milliseconds 300
        [void](Start-Qlr -Arguments @("--settings"))
    }

    $hwnd = Wait-Window -Class $c.Class
    if ($hwnd -eq [IntPtr]::Zero) { throw "没等到可见的 $($c.Class)（case=$($c.Name)）" }
    Settle-Window -Hwnd $hwnd
    if ($c.Settle) { Start-Sleep -Milliseconds $c.Settle } else { Start-Sleep -Milliseconds 250 }

    $png = Join-Path $OutDir ("{0}.png" -f $c.Name)
    $info = Save-Shot -Hwnd $hwnd -Path $png
    $sha = (Get-FileHash -LiteralPath $png -Algorithm SHA256).Hash
    "{0,-9} {1,5}x{2,-5} dpi={3,-4} {4}" -f $c.Name, $info.Width, $info.Height, $info.Dpi, $sha.Substring(0, 12)
    $manifest += [pscustomobject]@{
        case = $c.Name; png = $png; sha256 = $sha
        width = $info.Width; height = $info.Height; dpi = $info.Dpi
    }
}

# ---- 2) 收尾 ----
[void](Start-Qlr -Arguments @("--quit"))
Start-Sleep -Milliseconds 600
Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force

$screen = Get-ItemProperty "HKCU:\Control Panel\Desktop" -ErrorAction SilentlyContinue
$meta = [pscustomobject]@{
    exe      = $exe
    version  = (& $exe --version 2>$null | Select-Object -First 1)
    time     = (Get-Date -Format "yyyy-MM-dd HH:mm:ss")
    appliedDpi = $screen.AppliedDPI
    logonScreen = $screen.LogPixels
    cases    = $manifest
}
$manifestPath = Join-Path $OutDir "manifest.json"
$meta | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $manifestPath -Encoding UTF8
"清单 : $manifestPath"
exit 0
