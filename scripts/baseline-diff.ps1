# 逐像素对比两批基线截图（scripts\baseline-shot.ps1 的产物）。
#
# 用法：
#   pwsh -File scripts\baseline-diff.ps1 -A baseline\before -B baseline\after
#
# 判定：任何一张图有 1 个像素不同就 FAIL（96 DPI 下"改 DPI"必须零视觉变化）。
# 尺寸不同单独报 SIZE —— 那通常意味着开窗尺寸算错了，比像素差异严重。
#
# 为什么不用 Add-Type 写 C#：pwsh 7 里 `Add-Type -TypeDefinition` 的默认引用集没有
# System.Drawing.Common，`-ReferencedAssemblies System.Drawing` 也不管用（CS1069，
# 本会话实测两次）。纯 PowerShell + LockBits + Marshal.Copy 反而更省事，速度也够：
# 先用整幅缓冲区的 SHA256 判"完全一致"，只有不一致的图才逐像素数。
param(
    [Parameter(Mandatory = $true)][string]$A,
    [Parameter(Mandatory = $true)][string]$B,
    # 允许的每通道最大差值（0 = 严格逐像素）
    [int]$Tolerance = 0
)

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
if (-not [System.IO.Path]::IsPathRooted($A)) { $A = Join-Path $root $A }
if (-not [System.IO.Path]::IsPathRooted($B)) { $B = Join-Path $root $B }
if (-not (Test-Path $A)) { throw "没有 $A" }
if (-not (Test-Path $B)) { throw "没有 $B" }

Add-Type -AssemblyName System.Drawing

# 把整幅图读成 32bppArgb 的裸缓冲区（跨步补白来自源图，同尺寸的补白也一致）
function Read-Pixels {
    param([string]$Path)
    $bmp = New-Object System.Drawing.Bitmap $Path
    try {
        $conv = New-Object System.Drawing.Bitmap $bmp.Width, $bmp.Height, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
        try {
            $g = [System.Drawing.Graphics]::FromImage($conv)
            $g.DrawImage($bmp, 0, 0, $bmp.Width, $bmp.Height)
            $g.Dispose()
            $rect = New-Object System.Drawing.Rectangle 0, 0, $conv.Width, $conv.Height
            $data = $conv.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
            try {
                $len = $data.Stride * $conv.Height
                $buf = New-Object byte[] $len
                [System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $buf, 0, $len)
            } finally { $conv.UnlockBits($data) }
            return [pscustomobject]@{ W = $conv.Width; H = $conv.Height; Stride = $data.Stride; Bytes = $buf }
        } finally { $conv.Dispose() }
    } finally { $bmp.Dispose() }
}

function Get-BufHash {
    param([byte[]]$Buf)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try { return [System.Convert]::ToHexString($sha.ComputeHash($Buf)) } finally { $sha.Dispose() }
}

$files = Get-ChildItem $A -Filter *.png | Sort-Object Name
if (-not $files) { throw "$A 里没有 png" }

$bad = 0
$rows = @()
foreach ($f in $files) {
    $other = Join-Path $B $f.Name
    if (-not (Test-Path $other)) {
        $rows += [pscustomobject]@{ case = $f.BaseName; result = "MISSING"; detail = "B 里没有这张" }
        $bad++
        continue
    }
    $pa = Read-Pixels $f.FullName
    $pb = Read-Pixels $other
    if ($pa.W -ne $pb.W -or $pa.H -ne $pb.H) {
        $rows += [pscustomobject]@{ case = $f.BaseName; result = "SIZE"; detail = "$($pa.W)x$($pa.H) -> $($pb.W)x$($pb.H)" }
        $bad++
        continue
    }
    if ((Get-BufHash $pa.Bytes) -eq (Get-BufHash $pb.Bytes)) {
        $rows += [pscustomobject]@{ case = $f.BaseName; result = "SAME"; detail = "逐字节一致 $($pa.W)x$($pa.H)" }
        continue
    }
    # 走到这里才逐像素数（只会是少数图）
    $diff = 0
    $max = 0
    for ($i = 0; $i -lt $pa.Bytes.Length; $i += 4) {
        $d = [Math]::Abs($pa.Bytes[$i] - $pb.Bytes[$i])
        $d1 = [Math]::Abs($pa.Bytes[$i + 1] - $pb.Bytes[$i + 1])
        $d2 = [Math]::Abs($pa.Bytes[$i + 2] - $pb.Bytes[$i + 2])
        if ($d1 -gt $d) { $d = $d1 }
        if ($d2 -gt $d) { $d = $d2 }
        if ($d -gt $Tolerance) { $diff++; if ($d -gt $max) { $max = $d } }
    }
    $rows += [pscustomobject]@{ case = $f.BaseName; result = "DIFF"; detail = "不同像素=$diff 最大通道差=$max" }
    $bad++
}

$rows | Format-Table -AutoSize | Out-String -Width 120 | Write-Output

if ($bad -eq 0) {
    "PASS  $($files.Count) 张截图与基准逐像素一致（tolerance=$Tolerance）"
    exit 0
}
"FAIL  $bad / $($files.Count) 张有差异"
exit 1
