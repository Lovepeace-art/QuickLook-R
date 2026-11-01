# 真实样本矩阵通过率（WBS A6.7）
#
# 用 `qlrust --render <文件>` 无窗口地把每个样本渲一遍，按类别统计通过率。
# 「通过」= 退出码 0（日志里 `RENDER-CLI ok kind=…`），「失败」= 退出码 3（`RENDER-CLI FAIL`）。
#
# 用法：
#   pwsh -NoProfile -File scripts\sample-matrix.ps1                                  # 跑 target\bench-fixtures\matrix
#   pwsh -NoProfile -File scripts\sample-matrix.ps1 -Dir D:\samples                  # 跑自己的样本（按扩展名分类）
#   pwsh -NoProfile -File scripts\sample-matrix.ps1 -Matrix target\bench-fixtures\matrix -MinRate 0.98
#
# 输出：控制台目录明细 + scripts\sample-matrix.json；退出码 0 达标 / 1 不达标或有用例失败。
#
# 门限：WBS 要求「每种格式 ≥20 个真实文件，通过率 ≥98%」。合成矩阵用来做回归；
# 真实样本（自己收集的 jpg/pdf/md/txt）用 -Dir 指过来，两类都要能过。

param(
    [string]$Exe = "target\release\qlrust.exe",
    # 矩阵根目录（含 image/text/markdown/pdf 子目录）；-Dir 给出时忽略
    [string]$Matrix = "",
    # 任意目录：按扩展名归类后逐个渲染
    [string]$Dir = "",
    [double]$MinRate = 0.98,
    [string]$JsonOut = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not (Test-Path $Exe)) { throw "找不到可执行体：$Exe（先 cargo build --release）" }
$Exe = (Resolve-Path $Exe).Path
if (-not $JsonOut) { $JsonOut = Join-Path $PSScriptRoot "sample-matrix.json" }

function Get-KindOf([string]$Path) {
    switch -Regex ([System.IO.Path]::GetExtension($Path).ToLowerInvariant()) {
        '^\.(png|jpg|jpeg|jpe|jfif|gif|bmp|dib|webp|tif|tiff|ico|cur)$' { return "image" }
        '^\.pdf$' { return "pdf" }
        '^\.(md|markdown|mdown|mkd|mkdn)$' { return "markdown" }
        default { return "text" }
    }
}

# 收集样本：优先 -Dir，其次矩阵子目录
$samples = [System.Collections.Generic.List[object]]::new()
if ($Dir) {
    if (-not (Test-Path $Dir)) { throw "找不到样本目录：$Dir" }
    foreach ($f in Get-ChildItem (Resolve-Path $Dir).Path -File -Recurse) {
        $samples.Add([pscustomobject]@{ Kind = (Get-KindOf $f.FullName); Path = $f.FullName })
    }
} else {
    if (-not $Matrix) { $Matrix = Join-Path $root "target\bench-fixtures\matrix" }
    if (-not (Test-Path $Matrix)) { throw "找不到矩阵目录：$Matrix（先跑 scripts\make-fixtures.ps1 -Matrix）" }
    foreach ($sub in Get-ChildItem (Resolve-Path $Matrix).Path -Directory) {
        $kind = $sub.Name
        foreach ($f in Get-ChildItem $sub.FullName -File) {
            $samples.Add([pscustomobject]@{ Kind = $kind; Path = $f.FullName })
        }
    }
}
if ($samples.Count -eq 0) { throw "没有找到任何样本" }

# 单实例常驻宿主会抢走预览请求，先请走（CONVENTIONS #42：GUI 程序不能直接 & 调用）
Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 200

# 日志位置：便携模式在 <exe>\UserData\logs，否则在 %LOCALAPPDATA%\QLRust\logs
$exeDir = Split-Path -Parent $Exe
$logFile = if (Test-Path (Join-Path $exeDir "portable.lock")) {
    Join-Path $exeDir "UserData\logs\app.log"
} else {
    Join-Path $env:LOCALAPPDATA "QLRust\logs\app.log"
}

# 为什么不用 Start-Process / 退出码：本机这套环境里从 PowerShell 直接启动
# 工作区里的 exe 会以 ERROR_CANCELLED(1223) 失败（Low 完整性 + 提权会话的组合拳），
# 而 WMI Create 可以正常拉起。拿不到进程退出码，就从日志里读结论
# （`RENDER-CLI ok …` / `RENDER-CLI FAIL …`），这在自动化里更可靠。
function Read-LogText {
    if (-not (Test-Path $logFile)) { return "" }
    try {
        $fs = [System.IO.File]::Open($logFile, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read,
            ([System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete))
        try {
            $reader = New-Object System.IO.StreamReader($fs, [System.Text.Encoding]::UTF8)
            return $reader.ReadToEnd()
        } finally { $fs.Dispose() }
    } catch { return "" }
}

function Invoke-Render([string]$Path) {
    $before = (Read-LogText).Length
    $cmd = '"' + $Exe + '" --render "' + $Path + '"'
    Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $cmd } | Out-Null
    $deadline = (Get-Date).AddSeconds(30)
    while ((Get-Date) -lt $deadline) {
        $text = Read-LogText
        if ($text.Length -gt $before) {
            $fresh = $text.Substring($before)
            if ($fresh -match "RENDER-CLI ok") { return @{ Ok = $true; Detail = "ok" } }
            if ($fresh -match "RENDER-CLI FAIL[^\r\n]*") { return @{ Ok = $false; Detail = $Matches[0].Trim() } }
        }
        Start-Sleep -Milliseconds 40
    }
    return @{ Ok = $false; Detail = "TIMEOUT(30s 内没有 RENDER-CLI 结论)" }
}

$rows = [System.Collections.Generic.List[object]]::new()
foreach ($s in $samples) {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $r = Invoke-Render $s.Path
    $sw.Stop()
    $rows.Add([pscustomobject]@{
        类别 = $s.Kind
        文件 = (Split-Path $s.Path -Leaf)
        结果 = if ($r.Ok) { "OK" } else { "FAIL" }
        明细 = $r.Detail
        Ms   = [int]$sw.ElapsedMilliseconds
    })
}

Write-Output ""
foreach ($group in ($rows | Group-Object 类别 | Sort-Object Name)) {
    $ok = ($group.Group | Where-Object { $_.结果 -eq "OK" } | Measure-Object).Count
    $rate = if ($group.Count -gt 0) { [math]::Round(100.0 * $ok / $group.Count, 1) } else { 0 }
    Write-Output ("{0,-9} {1,3} 个  通过 {2,3}  通过率 {3,5}%  最长 {4,4} ms" -f
        $group.Name, $group.Count, $ok, $rate, (($group.Group | Measure-Object -Property Ms -Maximum).Maximum))

    # 每类至少 20 个（WBS 的硬要求），不足要明说而不是静默通过
    if ($group.Count -lt 20) { Write-Output ("  ⚠ {0} 类只有 {1} 个样本，未达到 ≥20 的要求" -f $group.Name, $group.Count) }
    foreach ($bad in ($group.Group | Where-Object { $_.结果 -ne "OK" })) {
        Write-Output ("  ✗ {0}  {1}" -f $bad.文件, $bad.结果)
    }
}

$total = $rows.Count
$passed = ($rows | Where-Object { $_.结果 -eq "OK" } | Measure-Object).Count
$rate = if ($total -gt 0) { $passed / $total } else { 0 }
Write-Output ""
Write-Output ("合计 {0} 个样本，通过 {1}，通过率 {2:P1}（门限 {3:P0}）" -f $total, $passed, $rate, $MinRate)

$payload = [ordered]@{
    generated_at = (Get-Date).ToString("o")
    exe          = $Exe
    source       = if ($Dir) { $Dir } else { $Matrix }
    total        = $total
    passed       = $passed
    rate         = [math]::Round($rate, 4)
    min_rate     = $MinRate
    by_kind      = @($rows | Group-Object 类别 | ForEach-Object {
            [ordered]@{
                kind  = $_.Name
                count = $_.Count
                ok    = ($_.Group | Where-Object { $_.结果 -eq "OK" } | Measure-Object).Count
            }
        })
    failures     = @($rows | Where-Object { $_.结果 -ne "OK" } | ForEach-Object { "$($_.类别)/$($_.文件)：$($_.结果)" })
}
[System.IO.File]::WriteAllText($JsonOut, ($payload | ConvertTo-Json -Depth 5), (New-Object System.Text.UTF8Encoding($false)))
Write-Output ("结果已写入 {0}" -f $JsonOut)

if ($rate -lt $MinRate) { exit 1 }
exit 0
