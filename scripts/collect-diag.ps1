# 现场诊断包（崩溃 / 缩放 / 句柄 / 配置，一键收集成一个 zip）。
#
# 用法（最简单）：右键这个文件 →「使用 PowerShell 运行」。
# 或者命令行：pwsh -File scripts\collect-diag.ps1
# 自动化/无人值守（跳过交互问答）：pwsh -File scripts\collect-diag.ps1 -NonInteractive
# 指定输出目录：pwsh -File scripts\collect-diag.ps1 -OutDir D:\tmp
#
# 它会收集：
#   - 版本与系统信息（exe FileVersion、OS、PowerShell 版本）
#   - 缩放与分辨率（AppliedDPI / LogPixels / 屏幕尺寸 / 显示器数）
#   - 程序日志 app.log（便携模式在 exe 旁边 UserData\logs\，安装模式在 %LOCALAPPDATA%）
#   - 崩溃现场 crashes\：MiniDumpWriteDump 落下的 .dmp + 同目录 .txt 报告
#   - 配置文件 config.json（便携/安装两种位置）
#   - 进程的 GDI / USER 对象数（判断有没有句柄泄漏）
#   - （交互模式）显式问一遍"你看到的现象"（7 个问题）
# 全部打成一个 zip，发回来即可。
#
# 为什么要问这 7 个问题：缩放类问题只看截图很难判断是"没生效"还是"生效了但口径不对"。

param(
    [string]$OutDir = "",
    [switch]$NonInteractive
)

$ErrorActionPreference = "Continue"

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

function Get-DisplayInfo {
    $desk = Get-ItemProperty "HKCU:\Control Panel\Desktop" -ErrorAction SilentlyContinue
    $mon = Get-ItemProperty "HKCU:\Control Panel\Desktop\WindowMetrics" -ErrorAction SilentlyContinue
    $screens = [System.Windows.Forms.Screen]::AllScreens | ForEach-Object {
        "$($_.DeviceName) $($_.Bounds.Width)x$($_.Bounds.Height) primary=$($_.Primary)"
    }
    [pscustomobject]@{
        AppliedDPI   = $desk.AppliedDPI
        LogPixels    = $mon.AppliedDPI
        Screens      = ($screens -join "; ")
        ScreenCount  = [System.Windows.Forms.Screen]::AllScreens.Count
        ScalePercent = if ($desk.AppliedDPI) { [math]::Round([int]$desk.AppliedDPI / 96.0 * 100) } else { "?" }
    }
}

# GDI / USER 对象数：泄漏时这个数会一路涨（任务是"每帧别新建 GDI 对象"）
function Get-GdiCounters {
    $src = @'
using System;
using System.Runtime.InteropServices;
public class QlrGdi {
  [DllImport("user32.dll")] public static extern uint GetGuiResources(IntPtr h, uint flags);
  [DllImport("kernel32.dll")] public static extern IntPtr OpenProcess(uint access, bool inherit, int pid);
  [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
  public static string Count(int pid) {
    IntPtr p = OpenProcess(0x1000, false, pid);   // PROCESS_QUERY_LIMITED_INFORMATION
    if (p == IntPtr.Zero) return "open-failed";
    try {
      uint gdi = GetGuiResources(p, 0);
      uint usr = GetGuiResources(p, 1);
      return "gdi=" + gdi + " user=" + usr;
    } finally { CloseHandle(p); }
  }
}
'@
    if (-not ("QlrGdi" -as [type])) { Add-Type -TypeDefinition $src }
    $procs = Get-Process qlrust -ErrorAction SilentlyContinue
    if (-not $procs) { return "(qlrust 没在运行)" }
    ($procs | ForEach-Object { "pid=$($_.Id) " + [QlrGdi]::Count($_.Id) }) -join "; "
}

function Find-Logs {
    $out = @()
    $procs = Get-Process qlrust -ErrorAction SilentlyContinue
    foreach ($p in $procs) {
        $dir = Split-Path -Parent $p.Path
        $cand = Join-Path $dir "UserData\logs\app.log"
        if (Test-Path $cand) { $out += $cand }
    }
    $installed = Join-Path $env:LOCALAPPDATA "QLrust\logs\app.log"
    if (Test-Path $installed) { $out += $installed }
    # 兜底：用户可能把绿色版解压在桌面/下载里
    if (-not $out) {
        $out += Get-ChildItem $env:USERPROFILE -Recurse -Filter app.log -ErrorAction SilentlyContinue -Depth 5 |
            Where-Object { $_.FullName -match 'UserData\\logs|QLrust\\logs' } |
            Select-Object -First 3 -ExpandProperty FullName
    }
    return $out | Select-Object -Unique
}

# 崩溃现场目录：<exe 旁>\UserData\crashes 或 %LOCALAPPDATA%\QLrust\crashes。
function Find-CrashDirs {
    $out = @()
    $procs = Get-Process qlrust -ErrorAction SilentlyContinue
    foreach ($p in $procs) {
        $dir = Split-Path -Parent $p.Path
        $cand = Join-Path $dir "UserData\crashes"
        if (Test-Path $cand) { $out += $cand }
    }
    $installed = Join-Path $env:LOCALAPPDATA "QLrust\crashes"
    if (Test-Path $installed) { $out += $installed }
    if (-not $out) {
        $out += Get-ChildItem $env:USERPROFILE -Recurse -Filter *.dmp -ErrorAction SilentlyContinue -Depth 5 |
            Where-Object { $_.FullName -match 'crashes' } |
            ForEach-Object { Split-Path -Parent $_.FullName } |
            Select-Object -Unique -First 3
    }
    return $out | Select-Object -Unique
}

# 配置文件：<exe 旁>\UserData\config.json 或 %LOCALAPPDATA%\QLrust\config.json。
function Find-Configs {
    $out = @()
    $procs = Get-Process qlrust -ErrorAction SilentlyContinue
    foreach ($p in $procs) {
        $dir = Split-Path -Parent $p.Path
        $cand = Join-Path $dir "UserData\config.json"
        if (Test-Path $cand) { $out += $cand }
    }
    $installed = Join-Path $env:LOCALAPPDATA "QLrust\config.json"
    if (Test-Path $installed) { $out += $installed }
    return $out | Select-Object -Unique
}

function Get-VersionText {
    $ver = "(qlrust 未运行，无法读取 exe 版本)"
    $procs = Get-Process qlrust -ErrorAction SilentlyContinue
    if ($procs) {
        try { $ver = (Get-Item $procs[0].Path).VersionInfo.FileVersion } catch { }
    }
    $os = try { (Get-CimInstance Win32_OperatingSystem).Caption } catch { [Environment]::OSVersion.VersionString }
    "QLRust 版本 : $ver`nOS          : $os`nPowerShell  : $($PSVersionTable.PSVersion)`n时间        : $(Get-Date -Format o)"
}

$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$base = if ($OutDir) { $OutDir } else { [Environment]::GetFolderPath("Desktop") }
New-Item -ItemType Directory -Path $base -Force | Out-Null
$workDir = Join-Path $base "qlrust-diag-$stamp"
New-Item -ItemType Directory -Path $workDir -Force | Out-Null

Write-Host "== 1/6 版本与系统信息 ==" -ForegroundColor Cyan
Get-VersionText | Set-Content (Join-Path $workDir "version.txt") -Encoding UTF8
Get-VersionText | Write-Host

Write-Host "== 2/6 收集显示信息 ==" -ForegroundColor Cyan
$disp = Get-DisplayInfo
$disp | Format-List | Out-String | Write-Host
$disp | ConvertTo-Json | Set-Content (Join-Path $workDir "display.json") -Encoding UTF8

Write-Host "== 3/6 收集日志 ==" -ForegroundColor Cyan
$logs = Find-Logs
if ($logs) {
    $i = 0
    foreach ($l in $logs) {
        $i++
        Copy-Item $l (Join-Path $workDir ("app-$i.log")) -Force
        Write-Host "  已复制：$l"
    }
} else {
    Write-Host "  没找到 app.log —— 请先运行一次 qlrust 并复现问题，再跑本脚本" -ForegroundColor Yellow
    "没找到日志" | Set-Content (Join-Path $workDir "NO-LOG.txt") -Encoding UTF8
}

Write-Host "== 4/6 收集崩溃现场 ==" -ForegroundColor Cyan
$crashDirs = Find-CrashDirs
if ($crashDirs) {
    $cDir = Join-Path $workDir "crashes"
    New-Item -ItemType Directory -Path $cDir -Force | Out-Null
    $copied = 0
    foreach ($cd in $crashDirs) {
        Get-ChildItem $cd -File -ErrorAction SilentlyContinue |
            Where-Object { $_.Extension -in ".dmp", ".txt" } |
            ForEach-Object {
                Copy-Item $_.FullName (Join-Path $cDir $_.Name) -Force
                $copied++
            }
    }
    Write-Host "  已复制 $copied 个崩溃文件"
    if ($copied -eq 0) { "崩溃目录存在但没有 .dmp/.txt" | Set-Content (Join-Path $cDir "README.txt") -Encoding UTF8 }
} else {
    Write-Host "  没有崩溃现场（这是好事，说明没崩过）"
    "没有崩溃现场" | Set-Content (Join-Path $workDir "NO-CRASH.txt") -Encoding UTF8
}

Write-Host "== 5/6 收集配置与句柄统计 ==" -ForegroundColor Cyan
$configs = Find-Configs
$i = 0
foreach ($c in $configs) {
    $i++
    Copy-Item $c (Join-Path $workDir ("config-$i.json")) -Force
    Write-Host "  已复制配置：$c"
}
if (-not $configs) { Write-Host "  没找到 config.json（可能还没改过设置）" }
$gdi = Get-GdiCounters
Write-Host "  $gdi"
$gdi | Set-Content (Join-Path $workDir "gdi.txt") -Encoding UTF8

if ($NonInteractive) {
    Write-Host "== 6/6 跳过交互问答（-NonInteractive）==" -ForegroundColor Cyan
    "(-NonInteractive：未采集用户问答)" | Set-Content (Join-Path $workDir "answers.txt") -Encoding UTF8
} else {
    Write-Host "== 6/6 回答 7 个问题（直接回车 = 跳过）==" -ForegroundColor Cyan
    $questions = @(
        "1. 打开任意 .txt：顶栏和字号看起来和 100% 缩放下一样大吗？(y/n + 一句话)",
        "2. 打开一个 PDF：窗口大概占屏幕高度的百分之几十？(例如 60%)",
        "3. 打开一张照片：图片大概占屏幕宽度的百分之几十？(例如 80%)",
        "4. 桌面上按 F2 给文件夹改名，中文和数字能正常输入吗？(y/n)",
        "5. 打开 PDF 点页码框输入 3，然后按 Esc 关闭预览，切到记事本按 123，数字能打出来吗？(y/n)",
        "6. Ctrl+Tab / Alt+Space 还会触发预览吗？Shift+= 还能放大吗？(各写 y/n)",
        "7. 打开设置窗：文字和行高正常（不是特别小）吗？(y/n)"
    )
    $answers = @()
    foreach ($q in $questions) {
        Write-Host ""
        Write-Host $q -ForegroundColor White
        $a = Read-Host "  回答"
        $answers += "$q`n   -> $a"
    }
    $answers -join "`n`n" | Set-Content (Join-Path $workDir "answers.txt") -Encoding UTF8
}

# 打包
$zip = Join-Path $base "qlrust-diag-$stamp.zip"
Compress-Archive -Path (Join-Path $workDir "*") -DestinationPath $zip -Force
Remove-Item $workDir -Recurse -Force -ErrorAction SilentlyContinue

Write-Host ""
Write-Host "完成：$zip" -ForegroundColor Green
Write-Host "把这个 zip 发回来就行（里面没有你的文件内容，只有日志/崩溃现场/配置和问答）。"
Write-Host ""
if (-not $NonInteractive -and $Host.Name -eq "ConsoleHost") { Read-Host "按回车关闭" | Out-Null }
