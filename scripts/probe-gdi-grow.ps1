# 追问：常驻进程反复预览时 GDI 对象/句柄/提交内存是否单调增长（= 泄漏还是缓存）
#
# 为什么单独一个脚本：measure-183.ps1 的 res 组已经看到"连看 4 轮 GDI +5/轮"的趋势，
# 这里把"每次只看同一种格式"拆开跑，定位是哪条路径在涨。
#
# 用法：pwsh -NoProfile -File scripts/probe-gdi-grow.ps1
# 输出：控制台表格（每轮一行）

param(
    [string]$Exe = "target\release\qlrust.exe",
    [string]$Fixtures = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$Exe = (Resolve-Path $Exe).Path
$exeDir = Split-Path -Parent $Exe
if (-not $Fixtures) {
    if ($env:QLRUST_FIXTURES) { $Fixtures = $env:QLRUST_FIXTURES }
    else { $Fixtures = Join-Path ([Environment]::GetFolderPath('Desktop')) 'qlrust-dev' }
}
$Fixtures = (Resolve-Path $Fixtures).Path
if (-not (Test-Path (Join-Path $exeDir "portable.lock"))) { New-Item -ItemType File -Path (Join-Path $exeDir "portable.lock") | Out-Null }
$logFile = Join-Path $exeDir "UserData\logs\app.log"

Add-Type -Namespace QlrG -Name W -MemberDefinition @'
[DllImport("user32.dll")] public static extern uint GetGuiResources(IntPtr hProcess, uint flags);
[DllImport("kernel32.dll")] public static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
[DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
'@

function Kill-Qlr {
    Get-Process qlrust -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    for ($i = 0; $i -lt 100; $i++) {
        if (-not (Get-Process qlrust -ErrorAction SilentlyContinue)) { break }
        Start-Sleep -Milliseconds 20
    }
    Start-Sleep -Milliseconds 900
}

function Start-Qlr {
    param([string[]]$ArgList = @())
    $quoted = ($ArgList | ForEach-Object { '"' + $_ + '"' }) -join " "
    $cmdLine = '"' + $Exe + '"'
    if ($quoted) { $cmdLine = $cmdLine + " " + $quoted }
    $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $cmdLine }
    if ($r.ReturnValue -ne 0) { throw "WMI 创建进程失败" }
    return [int]$r.ProcessId
}

function Wait-Exit {
    param([int]$ProcessId)
    for ($i = 0; $i -lt 2500; $i++) {
        if (-not (Get-Process -Id $ProcessId -ErrorAction SilentlyContinue)) { return }
        Start-Sleep -Milliseconds 2
    }
}

function Snap {
    param([int]$ProcessId)
    $p = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
    if (-not $p) { return $null }
    $gdi = 0; $user = 0
    $h = [QlrG.W]::OpenProcess(0x0400 -bor 0x1000, $false, [uint32]$ProcessId)
    if ($h -ne [IntPtr]::Zero) {
        $gdi = [QlrG.W]::GetGuiResources($h, 0)
        $user = [QlrG.W]::GetGuiResources($h, 1)
        [void][QlrG.W]::CloseHandle($h)
    }
    return [pscustomobject]@{
        PrivateMB = [math]::Round($p.PrivateMemorySize64 / 1MB, 2)
        WSMB      = [math]::Round($p.WorkingSet64 / 1MB, 2)
        句柄      = $p.HandleCount
        线程      = $p.Threads.Count
        GDI       = $gdi
        USER      = $user
    }
}

function Invoke-Req { param([string[]]$ArgList) $p = Start-Qlr $ArgList; Wait-Exit $p }

$F = @{
    pdf  = Join-Path $Fixtures "pdf_8page.pdf"
    png  = Join-Path $Fixtures "png_1920x1080.png"
    big  = Join-Path $Fixtures "48mp_8000x6000.png"
    txt  = Join-Path $Fixtures "big_16mb.txt"
    md   = Join-Path $Fixtures "demo.md"
}

Kill-Qlr
$procId = Start-Qlr
for ($i = 0; $i -lt 400; $i++) {
    if ((Test-Path $logFile) -and ((Get-Content $logFile -Raw) -match "热键就绪")) { break }
    Start-Sleep -Milliseconds 10
}

function Loop-Show {
    param([string]$Name, [string]$File, [int]$Times, [switch]$WithHide)
    $base = Snap $procId
    $rows = @()
    for ($i = 1; $i -le $Times; $i++) {
        Invoke-Req @("--show", $File) | Out-Null
        Start-Sleep -Milliseconds 260
        if ($WithHide) { Invoke-Req @("--hide") | Out-Null; Start-Sleep -Milliseconds 160 }
    }
    $after = Snap $procId
    $rows += [pscustomobject]@{
        场景       = ("$Name ×$Times" + $(if ($WithHide) { "（每次收起）" } else { "（连续替换）" }))
        GDI基线    = $base.GDI
        GDI结束    = $after.GDI
        GDI增量    = $after.GDI - $base.GDI
        句柄基线   = $base.句柄
        句柄结束   = $after.句柄
        句柄增量   = $after.句柄 - $base.句柄
        Private基线 = $base.PrivateMB
        Private结束 = $after.PrivateMB
    }
    Write-Host (($rows | Format-Table -AutoSize | Out-String -Width 200).TrimEnd())
}

Write-Host ("宿主 pid={0}，日志 {1}" -f $procId, $logFile)
Write-Host ("裸启基线：{0}" -f ((Snap $procId) | ConvertTo-Json -Compress))

Loop-Show -Name "PDF" -File $F.pdf -Times 10
Loop-Show -Name "PNG 1920" -File $F.png -Times 10
Loop-Show -Name "48MP PNG" -File $F.big -Times 10
Loop-Show -Name "16MB 文本" -File $F.txt -Times 10
Loop-Show -Name "Markdown" -File $F.md -Times 10
Loop-Show -Name "PDF（每次收起）" -File $F.pdf -Times 10 -WithHide
Loop-Show -Name "PNG（每次收起）" -File $F.png -Times 10 -WithHide

Write-Host ("结束快照：{0}" -f ((Snap $procId) | ConvertTo-Json -Compress))
Kill-Qlr
