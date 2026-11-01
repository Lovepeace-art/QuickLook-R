# 验证 PDF 预览：起常驻实例 → 投 PDF → 截预览窗 → 采样像素 → 退出。
# 用法： pwsh -File scripts\verify-pdf.ps1 -Pdf <文件> [-Out <截图路径>]
#
# 注意（踩过的坑，见 docs/dev/CONVENTIONS.md #6）：不能用 Start-Process / `& cmd /c`
# 直接起宿主实例——宿主是常驻进程，调用方会一直等它退出，自动化就挂住了。
# 统一走 WMI Win32_Process::Create 起进程、轮询 Get-Process 等结果。
param(
    [Parameter(Mandatory = $true)][string]$Pdf,
    [string]$Out = "",
    [string]$ExeDir = "",
    # 打开后再放大几档（每档 1.25×），验证矢量放大真的重栅格化了
    [int]$Zoom = 0,
    # 放大后往下平移几步（一步 ≈ 屏高 1/12）
    [int]$Pan = 0
)

$ErrorActionPreference = "Stop"

# WMI 起进程用的是 C:\Windows\System32 作为工作目录，相对路径会静默失败，
# 所以 ExeDir 必须先绝对化（踩过：日志一行都没有，脚本报"没找到可见窗口"）。
if ($ExeDir) { $ExeDir = (Resolve-Path -LiteralPath $ExeDir).Path }
$Pdf = (Resolve-Path -LiteralPath $Pdf).Path
$root = Split-Path -Parent $PSScriptRoot
if (-not $ExeDir) { $ExeDir = Join-Path $root "target\release" }
if (-not $Out) { $Out = Join-Path ([System.IO.Path]::GetTempPath()) "qlrust-pdf-shot.png" }
$exe = Join-Path $ExeDir "qlrust.exe"
if (-not (Test-Path $exe)) { throw "找不到 $exe，先 cargo build --release" }
$logPath = Join-Path $ExeDir "UserData\logs\app.log"

Add-Type -AssemblyName System.Drawing
$src = @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public class PdfWin {
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
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
if (-not ("PdfWin" -as [type])) { Add-Type -TypeDefinition $src }

function Start-Qlr {
    param([string[]]$Arguments)
    $line = '"' + $exe + '" ' + ($Arguments -join ' ')
    $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $line.Trim() }
    return [int]$r.ProcessId
}

# 1) 清干净：残留实例会让日志与窗口都对不上
Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 400
Remove-Item $logPath -ErrorAction SilentlyContinue

# 2) 起宿主
[void](Start-Qlr -Arguments @())
for ($i = 0; $i -lt 40; $i++) {
    Start-Sleep -Milliseconds 100
    if ((Test-Path $logPath) -and (Select-String -Path $logPath -Pattern "壳层就绪" -Quiet)) { break }
}

# 3) 投 PDF（第二实例转交后自行退出）
[void](Start-Qlr -Arguments @('"' + $Pdf + '"'))
Start-Sleep -Milliseconds 1500

# 3.5) 可选：放大 / 平移（A2.8 的缩放与平移）
for ($i = 0; $i -lt $Zoom; $i++) {
    [void](Start-Qlr -Arguments @("--zoom-in"))
    Start-Sleep -Milliseconds 600
}
for ($i = 0; $i -lt $Pan; $i++) {
    [void](Start-Qlr -Arguments @("--pan", "0", "1"))
    Start-Sleep -Milliseconds 300
}
if ($Zoom -gt 0 -or $Pan -gt 0) { Start-Sleep -Milliseconds 400 }

$text = if (Test-Path $logPath) { Get-Content $logPath -Raw } else { "" }
$showLine = ($text -split "`n" | Select-String "SHOW target=.*kind=pdf" | Select-Object -Last 1)
$renderLine = ($text -split "`n" | Select-String "RENDER gen=" | Select-Object -Last 1)
$zoomLines = (($text -split "`n" | Select-String "ZOOM |PAN ") -join " | ")
"SHOW   : $($showLine -replace '^\S+\s+\S+\s+\[\S+\]\s+app\s+','')"
"RENDER : $($renderLine -replace '^\S+\s+\S+\s+\[\S+\]\s+app\s+','')"

# 4) 截预览窗并采样
$hwnd = [PdfWin]::Find("QLRust.PreviewWindow")
if ($hwnd -eq [IntPtr]::Zero) { throw "没找到可见的 QLRust.PreviewWindow（PDF 没出窗？）" }
$rect = New-Object PdfWin+RECT
[void][PdfWin]::GetWindowRect($hwnd, [ref]$rect)
$w = $rect.R - $rect.L
$h = $rect.B - $rect.T
"窗口   : $($rect.L),$($rect.T) ${w}x${h}"

$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)
$dc = $g.GetHdc()
[void][PdfWin]::PrintWindow($hwnd, $dc, 2)
$g.ReleaseHdc($dc)
$g.Dispose()

New-Item -ItemType Directory -Path (Split-Path -Parent $Out) -Force | Out-Null
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)

# 页面是居中绘制的：只统计中间 520px 宽的一列，避免把两侧留白算成"白底"
$left = [int](($w - 520) / 2)
$dark = 0
$white = 0
for ($x = $left; $x -lt $left + 520; $x += 3) {
    for ($y = 20; $y -lt $h - 20; $y += 3) {
        $c = $bmp.GetPixel($x, $y)
        if ($c.R -lt 120 -and $c.G -lt 120 -and $c.B -lt 120) { $dark++ }
        elseif ($c.R -gt 245 -and $c.G -gt 245 -and $c.B -gt 245) { $white++ }
    }
}
$corner = $bmp.GetPixel(4, 4)
$bmp.Dispose()

# 平移之后可能已经把文字滚出可视区（页面下部是空白），所以有 Pan 时改看"有白底"，
# 没 Pan 时必须看到文字，否则说明画的不是页面内容。
$hasContent = if ($Pan -gt 0) { $white -gt 500 } else { $dark -gt 50 }
$ok = ($showLine -match "kind=pdf") -and ($renderLine -match "bitmap") -and $hasContent
"ZOOM   : $zoomLines"
"截图   : $Out"
"像素   : 深色=$dark 纯白=$white 角落留白=$($corner.R),$($corner.G),$($corner.B)"

# 5) 收尾
[void](Start-Qlr -Arguments @("--quit"))
Start-Sleep -Milliseconds 500
$left = (Get-Process qlrust -ErrorAction SilentlyContinue | Measure-Object).Count
if ($left -ne 0) { Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force }
"残留   : $left"

if ($ok) { "PASS  PDF 预览渲染出页面（有文字、有白底）"; exit 0 }
"FAIL  SHOW/RENDER/像素 有一项不符合预期"; exit 1




