# QLRust 基准脚本（M6 A6.4：可无人值守跑，进 CI）
#
# 量四张表：冷启动、首帧（按键→可见）、内存（峰值/稳态）、GDI/USER 对象（收起之后）。
# 与上级目录 docs/bench-compare.md 里 QuickLook 4.5.0 的同机数字可比：
#   QuickLook 4K 热路径 421 ms / 48MP 418 ms / 26 MB 文本 923 ms
#
# 用法：
#   pwsh -NoProfile -File scripts\bench.ps1                       # 默认夹具目录自动找
#   pwsh -NoProfile -File scripts\bench.ps1 -FixtureDir target\bench-fixtures -Budget scripts\bench-budget.json
#   pwsh -NoProfile -File scripts\bench.ps1 -Only pdf -Rounds 2   # 只跑名字含 pdf 的场景
#   pwsh -NoProfile -File scripts\bench.ps1 -Only 4K -Rounds 1 -Passes 20   # GDI/内存泄漏浸泡
# 输出：控制台三张表 + <JsonOut>（默认 scripts\bench-results.json）
# 退出码：0 全通过；1 有 FAIL 或超预算
#
# 口径（很重要）：
#   * ColdStartMs  = 日志里 `壳层就绪 window_created=Nms`（进程启动到窗口建好）
#   * RenderMs     = worker 线程里 `qlr_render::render_file` 的耗时（日志 elapsed=）
#   * PerceivedMs  = 从"第二实例投递请求"到"日志里出现 RENDER 行"的墙钟时间，含
#                    进程创建、WM_COPYDATA、首帧上屏 —— 用户真正感觉到的那个数
#   * PeakRSSMB    = 渲染过程中采样到的 working set 峰值（渲染完缓冲就释放，事后读不到）
#   * PeakPrivateMB = 同上，但取 private bytes 峰值 —— 大图/PDF 的真实内存占用看这个，
#                     working set 会被系统裁剪，看着小。
#   * RSSMB / PrivateMB = 渲染完成后的稳态
#   * AllocKB / LiveKB = 这一帧渲染一共分配了多少字节（日志 `RENDERALLOC gen=N bytes=… live=…`）。
#                    只有 `cargo build --release -p qlr-app --target-dir target\alloc-stats
#                    --features alloc-stats` 构建的 exe 才有这行；普通构建这一列是空的。
#   * GdiAfterHide / UserAfterHide = 每轮"收起预览之后"日志里的 GDI/USER 对象数
#                    （app 的 hide 打 `HIDE … GDI=n USER=n`）。判据是每轮回到同一个值：
#                    每轮 +1 就是泄漏（进程级配额默认 1 万，攒够界面开始掉字）。
#                    这条不依赖预算文件也会判红。
#
# 历史坑（见 docs/dev/CONVENTIONS.md #37）：上一轮的常驻实例没退出时，新实例只把请求
# 转交给它，而它握着已删除日志的句柄 → 每轮 FAIL(无 RENDER)。本脚本因此对"停旧宿主"
# 做了强制收尾：--quit → 等退出 → 强杀 → 等退出 → 删日志（带重试）→ 断言无残留。

param(
    [string]$Exe = "target\release\qlrust.exe",
    [string]$FixtureDir = "",
    # 每场景重复几轮；第 0 轮是冷启动（新常驻实例），第 1 轮起是热路径
    [int]$Rounds = 2,
    # 每轮里"显示→收起"重复几次；第 0 次是冷路径，第 1 次起是热路径。
    # 想抓 GDI/内存泄漏就跑 `-Rounds 1 -Passes 20`（同一进程里连续开合 20 次）。
    [int]$Passes = 2,
    # 只跑名字匹配的场景（-like 语义），空 = 全跑
    [string]$Only = "",
    # 预算文件（JSON）；给出则超预算退出码 1
    [string]$Budget = "",
    [string]$JsonOut = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not (Test-Path $Exe)) { throw "找不到可执行体：$Exe（先 cargo build --release）" }
$Exe = (Resolve-Path $Exe).Path
$exeDir = Split-Path -Parent $Exe
$logFile = Join-Path $exeDir "UserData\logs\app.log"
if (-not $JsonOut) { $JsonOut = Join-Path $PSScriptRoot "bench-results.json" }

if (-not $FixtureDir) {
    foreach ($c in @((Join-Path $root "spike\fixtures"), (Join-Path (Split-Path -Parent $root) "spike\fixtures"), (Join-Path $root "target\bench-fixtures"))) {
        if (Test-Path $c) { $FixtureDir = (Resolve-Path $c).Path; break }
    }
}
if (-not $FixtureDir) { throw "找不到夹具目录（用 -FixtureDir 指定，或先跑 scripts\make-fixtures.ps1）" }
$FixtureDir = (Resolve-Path $FixtureDir).Path

# 场景表：文件不存在就跳过，所以本地大夹具与 CI 小夹具可以共用这一份脚本
$allScenarios = @(
    @{ Name = "4K 3840x2160 JPEG"; File = "8mp_3840x2160.jpg" },
    @{ Name = "48MP 8000x6000 JPEG"; File = "48mp_8000x6000.jpg" },
    @{ Name = "1920x1080 PNG"; File = "png_1920x1080.png" },
    @{ Name = "16 MB 文本"; File = "big_16mb.txt" },
    @{ Name = "26.6 MB 文本"; File = "big_100mb.txt" },
    @{ Name = "8 页 PDF"; File = "pdf_8page.pdf" },
    @{ Name = "Markdown 排版"; File = "md_styled.md" }
)
$scenarios = $allScenarios
if ($Only) { $scenarios = $allScenarios | Where-Object { $_.Name -like "*$Only*" -or $_.File -like "*$Only*" } }

function Start-Qlr {
    param([string]$Exe, [string[]]$ArgList = @())
    $cmd = '"' + $Exe + '"'
    foreach ($a in $ArgList) { $cmd += ' "' + $a + '"' }
    $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $cmd }
    return [int]$r.ProcessId
}

function Get-HostProcess {
    param([string]$Path)
    Get-Process qlrust -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $Path } | Select-Object -First 1
}

# 共享读日志：app 用 FILE_SHARE_READ|WRITE|DELETE 打开，这里必须同样宽松，
# 否则轮询期间的开合会干扰写端（历史上表现为"日志里缺 RENDER 行"）。
function Read-LogText {
    if (-not (Test-Path $logFile)) { return "" }
    try {
        $fs = [System.IO.File]::Open($logFile, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, ([System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete))
        try {
            $reader = New-Object System.IO.StreamReader($fs, [System.Text.Encoding]::UTF8)
            return $reader.ReadToEnd()
        } finally { $fs.Dispose() }
    } catch { return "" }
}

function Wait-Until {
    param([scriptblock]$Condition, [int]$TimeoutMs = 5000)
    $deadline = (Get-Date).AddMilliseconds($TimeoutMs)
    while ((Get-Date) -lt $deadline) {
        if (& $Condition) { return $true }
        Start-Sleep -Milliseconds 20
    }
    return $false
}

# 把常驻实例彻底请走：--quit → 等 → 强杀 → 等 → 删日志（重试）→ 断言无残留。
# 少任何一步都会重演 CONVENTIONS #37 的"每轮 FAIL(无 RENDER)"。
function Stop-Host {
    param([string]$Exe)
    $host_ = Get-HostProcess -Path $Exe
    if ($host_) {
        Start-Qlr -Exe $Exe -ArgList @("--quit") | Out-Null
        if (-not (Wait-Until { -not (Get-HostProcess -Path $Exe) } 4000)) {
            Get-HostProcess -Path $Exe | ForEach-Object { Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue }
            if (-not (Wait-Until { -not (Get-HostProcess -Path $Exe) } 4000)) {
                throw "常驻实例杀不掉（pid=$($host_.Id)）"
            }
        }
    }
    # 句柄未关时 Remove-Item 会成功但文件仍在（删除挂起），所以删完要复查
    for ($i = 0; $i -lt 50; $i++) {
        Remove-Item $logFile -Force -ErrorAction SilentlyContinue
        if (-not (Test-Path $logFile)) { break }
        Start-Sleep -Milliseconds 40
    }
    if (Test-Path $logFile) { throw "删不掉日志文件：$logFile（还有进程握着句柄）" }
}

# 便携模式，别污染 %LOCALAPPDATA%
$portableLock = Join-Path $exeDir "portable.lock"
if (-not (Test-Path $portableLock)) { New-Item -ItemType File -Path $portableLock | Out-Null }

# 清掉残留实例（所有同路径的）
Get-Process qlrust -ErrorAction SilentlyContinue | ForEach-Object {
    if ($_.Path -eq $Exe) { Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue }
}
Start-Sleep -Milliseconds 200

$budgetObj = $null
if ($Budget) {
    if (-not (Test-Path $Budget)) { throw "找不到预算文件：$Budget" }
    $budgetObj = Get-Content $Budget -Raw | ConvertFrom-Json
}

$results = [System.Collections.Generic.List[object]]::new()
$failed = $false
$overBudget = [System.Collections.Generic.List[string]]::new()

foreach ($fx in $scenarios) {
    $path = Join-Path $FixtureDir $fx.File
    if (-not (Test-Path $path)) { Write-Output ("跳过（缺夹具）：{0}" -f $fx.File); continue }

    for ($round = 0; $round -lt $Rounds; $round++) {
        # 每轮都重启常驻实例：第 0 轮走冷启动路径（用户第一次按空格），
        # 第 1 轮起复用同一个宿主 —— 但为了让"热路径"也能量到稳定值，本轮仍先重启宿主，
        # 再让它连续渲染两次：第一次算冷、第二次算热。
        Stop-Host -Exe $Exe

        $swShell = [System.Diagnostics.Stopwatch]::StartNew()
        $hostPid = Start-Qlr -Exe $Exe
        $ready = Wait-Until { (Test-Path $logFile) -and ((Get-Content $logFile -Raw) -match "壳层就绪") } 15000
        $swShell.Stop()
        if (-not $ready) {
            $results.Add([pscustomobject]@{
                场景 = $fx.Name; 轮次 = $round; 结果 = "FAIL(壳层未就绪)"; 类型 = $null
                ColdStartMs = $null; RenderMs = $null; PerceivedMs = $null
                字节 = $null; PeakRSSMB = $null; PeakPrivateMB = $null; RSSMB = $null; PrivateMB = $null
                GdiAfterHide = $null; UserAfterHide = $null; AllocKB = $null; LiveKB = $null
            })
            $failed = $true
            continue
        }
        $shellLine = Select-String -Path $logFile -Pattern "壳层就绪 window_created=(\d+)ms" | Select-Object -Last 1
        $coldStartMs = if ($shellLine) { [int]$shellLine.Matches[0].Groups[1].Value } else { $null }

        # 冷启动那一下：投递请求并等 RENDER
        $passCount = [math]::Max(1, $Passes)
        $prevGen = 0
        for ($pass = 0; $pass -lt $passCount; $pass++) {
            # 同一个文件再请求一次是 Toggle（把窗口关掉），所以每轮先显式 --hide，
            # 否则第二轮只会看到 HIDE、永远等不到新的 RENDER。
            Start-Qlr -Exe $Exe -ArgList @("--hide") | Out-Null
            Start-Sleep -Milliseconds 80

            # 收起之后的 GDI/USER 残留：app 在 hide 里打 "HIDE elapsed=… GDI=n USER=n"。
            # 判据是"每轮回到同一个数"——现场报障过"每预览一个文件 GDI +1 且收起不释放"，
            # 那是进程级配额（默认 1 万），攒够了界面开始掉字，靠肉眼发现已经太晚。
            $gdiAfterHide = $null
            $userAfterHide = $null
            if (Test-Path $logFile) {
                $hideText = Read-LogText
                $mh = [regex]::Matches($hideText, "HIDE elapsed=(\d+)ms GDI=(\d+) USER=(\d+)")
                if ($mh.Count -gt 0) {
                    $lastHide = $mh[$mh.Count - 1]
                    $gdiAfterHide = [int]$lastHide.Groups[2].Value
                    $userAfterHide = [int]$lastHide.Groups[3].Value
                }
            }

            $sw = [System.Diagnostics.Stopwatch]::StartNew()
            Start-Qlr -Exe $Exe -ArgList @($path) | Out-Null
            $renderLine = $null
            $peakRss = 0
            $peakPrivate = 0
            $deadline = (Get-Date).AddSeconds(20)
            while ((Get-Date) -lt $deadline) {
                $sampled = Get-HostProcess -Path $Exe
                if ($sampled) {
                    $peakRss = [math]::Max($peakRss, $sampled.WorkingSet64)
                    $peakPrivate = [math]::Max($peakPrivate, $sampled.PrivateMemorySize64)
                }
                if (Test-Path $logFile) {
                    # 日志行形如：
                    #   RENDER gen=1 bitmap 1189x669 bytes=3181764 ink=198243 elapsed=28ms (请求 1190x748)
                    # 注意 bytes= 与 elapsed= 中间还有 ink=，所以两处都要 .*? 兜住。
                    # 用 FileStream 共享读，别让 Get-Content 把写端卡住。
                    $text = Read-LogText
                    $mt = [regex]::Matches($text, "RENDER gen=(\d+) (\w+).*?bytes=(\d+).*?elapsed=(\d+)ms")
                    if ($mt.Count -gt 0) {
                        $last = $mt[$mt.Count - 1]
                        if ([int]$last.Groups[1].Value -gt $prevGen) {
                            $renderLine = $last
                            $prevGen = [int]$last.Groups[1].Value
                            break
                        }
                    }
                }
                Start-Sleep -Milliseconds 8
            }
            $sw.Stop()

            if (-not $renderLine) {
                $results.Add([pscustomobject]@{
                    场景 = $fx.Name; 轮次 = "$round.$pass"; 结果 = "FAIL(无 RENDER)"; 类型 = $null
                    ColdStartMs = $coldStartMs; RenderMs = $null; PerceivedMs = $sw.ElapsedMilliseconds
                    字节 = $null; PeakRSSMB = [math]::Round($peakRss / 1MB, 1)
                    PeakPrivateMB = [math]::Round($peakPrivate / 1MB, 1); RSSMB = $null; PrivateMB = $null
                    GdiAfterHide = $gdiAfterHide; UserAfterHide = $userAfterHide
                    AllocKB = $null; LiveKB = $null
                })
                $failed = $true
                continue
            }

            $proc = Get-HostProcess -Path $Exe
            # 帧级分配：只有 `--features alloc-stats` 构建的 exe 才打 RENDERALLOC 行，
            # 普通构建这里就是 $null（列保持为空，不伪造 0）。
            $allocBytes = $null
            $liveBytes = $null
            if (Test-Path $logFile) {
                $allocText = Read-LogText
                $gen = [int]$renderLine.Groups[1].Value
                foreach ($m in [regex]::Matches($allocText, "RENDERALLOC gen=(\d+) bytes=(\d+) live=(\d+)")) {
                    if ([int]$m.Groups[1].Value -eq $gen) {
                        $allocBytes = [int]$m.Groups[2].Value
                        $liveBytes = [int]$m.Groups[3].Value
                    }
                }
            }
            $results.Add([pscustomobject]@{
                场景 = $fx.Name
                轮次 = "$round.$pass"
                结果 = "OK"
                类型 = $renderLine.Groups[2].Value
                ColdStartMs = if ($pass -eq 0) { $coldStartMs } else { $null }
                RenderMs = [int]$renderLine.Groups[4].Value
                PerceivedMs = $sw.ElapsedMilliseconds
                字节 = [int]$renderLine.Groups[3].Value
                PeakRSSMB = [math]::Round($peakRss / 1MB, 1)
                PeakPrivateMB = [math]::Round($peakPrivate / 1MB, 1)
                RSSMB = if ($proc) { [math]::Round($proc.WorkingSet64 / 1MB, 1) } else { $null }
                PrivateMB = if ($proc) { [math]::Round($proc.PrivateMemorySize64 / 1MB, 1) } else { $null }
                GdiAfterHide = $gdiAfterHide
                UserAfterHide = $userAfterHide
                AllocKB = if ($null -ne $allocBytes) { [math]::Round($allocBytes / 1KB, 1) } else { $null }
                LiveKB = if ($null -ne $liveBytes) { [math]::Round($liveBytes / 1KB, 1) } else { $null }
            })
        }

        # 预算核对（只看热路径那一轮，冷启动单列）
        if ($budgetObj) {
            $hot = $results | Where-Object { $_.场景 -eq $fx.Name -and $_.轮次 -eq "$round.1" -and $_.结果 -eq "OK" } | Select-Object -Last 1
            $b = $budgetObj.scenarios.PSObject.Properties[$fx.File]
            if ($hot -and $b) {
                foreach ($field in @(@{ Key = "RenderMs"; Budget = $b.Value.render_ms },
                                     @{ Key = "PerceivedMs"; Budget = $b.Value.perceived_ms },
                                     @{ Key = "PeakRSSMB"; Budget = $b.Value.peak_rss_mb },
                                     @{ Key = "PeakPrivateMB"; Budget = $b.Value.peak_private_mb },
                                     @{ Key = "GdiAfterHide"; Budget = $b.Value.gdi_after_hide },
                                     @{ Key = "ColdStartMs"; Budget = $budgetObj.cold_start_ms })) {
                    if ($null -ne $field.Budget -and $null -ne $hot.($field.Key) -and $hot.($field.Key) -gt $field.Budget) {
                        $overBudget.Add(("{0} {1}={2} > 预算 {3}" -f $fx.Name, $field.Key, $hot.($field.Key), $field.Budget))
                    }
                }
            }
        }

        # GDI/USER 泄漏判据（不依赖预算文件）：同一个进程里连续"显示→收起"之后，
        # 对象数应该稳定不涨。现场报障的形态是"每预览一个文件 GDI +1 且收起不释放"，
        # 攒到进程级配额（默认 1 万）界面就开始掉字 —— 靠肉眼发现已经太晚。
        #
        # 注意口径：每轮都会重启常驻实例，所以第 1 次收起是"冷启动后的稳态基线"
        # （窗口刚建好，比空进程多几个对象是正常的）；判据只用第 2 次及以后的样本。
        # 默认 2 次样本看不出趋势，想抓泄漏跑 `-Rounds 1 -Passes 20` 浸泡。
        foreach ($key in @(@{ Field = "GdiAfterHide"; What = "GDI" },
                           @{ Field = "UserAfterHide"; What = "USER" })) {
            $series = @($results |
                Where-Object { $_.场景 -eq $fx.Name -and $_.轮次 -like "$round.*" -and $null -ne $_.($key.Field) } |
                ForEach-Object { [int]$_.($key.Field) })
            if ($series.Count -ge 3) {
                $settled = $series[1]
                $worst = ($series[1..($series.Count - 1)] | Measure-Object -Maximum).Maximum
                if ($worst -gt $settled + 2) {
                    $overBudget.Add(("{0} 第 {1} 轮 {2} 收起后从 {3} 涨到 {4}（{5}）：疑似泄漏" -f
                        $fx.Name, $round, $key.What, $settled, $worst, ($series -join " → ")))
                }
            }
        }
    }
}

Stop-Host -Exe $Exe

Write-Output ""
Write-Output "=== 冷启动（进程启动 → 窗口建好）==="
$results | Where-Object { $null -ne $_.ColdStartMs } | Select-Object 场景, 轮次, ColdStartMs |
    Format-Table -AutoSize | Out-String | Write-Output

Write-Output "=== 首帧（渲染耗时 / 用户感知）==="
$results | Select-Object 场景, 轮次, 结果, 类型, RenderMs, PerceivedMs, 字节 |
    Format-Table -AutoSize | Out-String | Write-Output

Write-Output "=== 内存（峰值 / 稳态）==="
$results | Select-Object 场景, 轮次, 结果, PeakRSSMB, PeakPrivateMB, RSSMB, PrivateMB |
    Format-Table -AutoSize | Out-String | Write-Output

Write-Output "=== 帧级分配（只有 alloc-stats 构建才有数）==="
$results | Select-Object 场景, 轮次, 结果, AllocKB, LiveKB |
    Format-Table -AutoSize | Out-String | Write-Output

Write-Output "=== GDI / USER 对象（收起之后）==="
$results | Select-Object 场景, 轮次, 结果, GdiAfterHide, UserAfterHide |
    Format-Table -AutoSize | Out-String | Write-Output

$payload = [ordered]@{
    generated_at = (Get-Date).ToString("o")
    exe          = $Exe
    exe_bytes    = (Get-Item $Exe).Length
    fixture_dir  = $FixtureDir
    rounds       = $Rounds
    results      = $results
    failures     = @($results | Where-Object { $_.结果 -ne "OK" }).Count
    over_budget  = @($overBudget)
}
$json = $payload | ConvertTo-Json -Depth 6
[System.IO.File]::WriteAllText($JsonOut, $json, (New-Object System.Text.UTF8Encoding($false)))
Write-Output ("结果已写入 {0}" -f $JsonOut)

if ($overBudget.Count -gt 0) {
    Write-Output "=== 超预算 ==="
    $overBudget | ForEach-Object { Write-Output ("  " + $_) }
}
if ($failed -or $overBudget.Count -gt 0) { exit 1 }
exit 0
