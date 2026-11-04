# 分层断言（结构评审 §6-10）——**不需要可执行体，也不用起窗口**
#
# 断言的是 ARCH.md 第 1 节写的那套依赖方向，而不是"我们希望的方向"：
#   qlr-app ──► qlr-platform ──► qlr-core
#      └────► qlr-render ──────► qlr-core
#
# 为什么要脚本化：这套规则在 1.8.3 之前**漂移过三次**（ARCH.md 说 render 不碰 windows，
# 实际 svg.rs 早就在用 Direct2D；说 app 是薄层，实际 layout.rs/settings.rs 直接 import
# Win32 类型）。文档靠人复述一定会漂，所以把"当前事实"钉成可执行断言——**改架构必须改这个
# 脚本**，改完就在 diff 里看得见。
#
# 断言的形状是"白名单 + 计数棘轮"：现存例外允许存在但**不许变多**（新增一处就红），
# 白名单只能缩小不能扩大。
#
# 用法：
#   pwsh -NoProfile -File scripts/verify-layers.ps1
#   pwsh -NoProfile -File scripts/verify-layers.ps1 -Root D:\some\clone
# 退出码：0 = 全过；1 = 有失败项。

param(
    [string]$Root = ""
)

$ErrorActionPreference = "Stop"
if ([string]::IsNullOrWhiteSpace($Root)) {
    $Root = Split-Path -Parent $PSScriptRoot
}
if (-not (Test-Path $Root)) { throw "找不到仓库根：$Root" }
$Root = (Resolve-Path $Root).Path
Set-Location $Root

$script:results = @()
function Check {
    param([string]$Name, [bool]$Ok, [string]$Detail = "")
    $script:results += [pscustomobject]@{ 检查项 = $Name; 结果 = $(if ($Ok) { "PASS" } else { "FAIL" }); 说明 = $Detail }
    $color = if ($Ok) { "Green" } else { "Red" }
    Write-Host ("{0,-34} {1} {2}" -f $Name, $(if ($Ok) { "PASS" } else { "FAIL" }), $Detail) -ForegroundColor $color
}

# ---- 0. 取某个 crate 的 [dependencies] 段（只认顶层依赖，不含 dev/target 段）----
function Get-DepBlock {
    param([string]$Crate)
    $toml = Join-Path $Root "crates\$Crate\Cargo.toml"
    if (-not (Test-Path $toml)) { throw "找不到 $toml" }
    $lines = Get-Content $toml -Encoding utf8
    $out = New-Object System.Collections.Generic.List[string]
    $in = $false
    foreach ($line in $lines) {
        if ($line -match '^\s*\[([^\]]+)\]\s*$') {
            $in = ($Matches[1] -eq "dependencies")
            continue
        }
        if ($in) { $out.Add($line) }
    }
    return ($out -join "`n")
}

$core = Get-DepBlock "qlr-core"
$platform = Get-DepBlock "qlr-platform"
$render = Get-DepBlock "qlr-render"
$app = Get-DepBlock "qlr-app"

# ---- 1. 依赖方向（Cargo.toml 级）----
Check "core 不依赖 windows" ($core -notmatch '(?m)^\s*windows\s*[.=]') ""
Check "core 不依赖上层 crate" (
    $core -notmatch '(?m)^\s*qlr-(platform|render|app)\s*[.=]'
) ""

Check "platform 不依赖 render/app" (
    $platform -notmatch '(?m)^\s*qlr-(render|app)\s*[.=]'
) "platform 只许往 core 走"
Check "platform 依赖 windows" ($platform -match '(?m)^\s*windows\s*[.=]') "Win32 薄封装在这里"

Check "render 不依赖 platform/app" (
    $render -notmatch '(?m)^\s*qlr-(platform|app)\s*[.=]'
) "渲染不许反向依赖宿主"

Check "app 三向都可依赖" (
    $app -match '(?m)^\s*qlr-core\s*[.=]' -and
    $app -match '(?m)^\s*qlr-platform\s*[.=]' -and
    $app -match '(?m)^\s*qlr-render\s*[.=]'
) ""

# ---- 2. 源码级：`use windows::` 出现在哪些文件 ----
function Get-WindowsUsers {
    param([string]$Crate)
    $dir = Join-Path $Root "crates\$Crate\src"
    $files = Get-ChildItem $dir -Recurse -Filter *.rs
    $hits = @{}
    foreach ($f in $files) {
        $n = (Select-String -Path $f.FullName -Pattern 'use\s+windows(::|::core)' -AllMatches -Encoding utf8 |
              Measure-Object).Count
        if ($n -gt 0) { $hits[$f.FullName] = $n }
    }
    return $hits
}

function RelName {
    param([string]$Full)
    return $Full.Replace($Root + "\", "").Replace("\", "/")
}

$coreHits = Get-WindowsUsers "qlr-core"
Check "core 源码零 use windows" ($coreHits.Count -eq 0) (
    $(if ($coreHits.Count -eq 0) { "" } else { ($coreHits.Keys | ForEach-Object { RelName $_ }) -join ", " })
)

$renderHits = Get-WindowsUsers "qlr-render"
$renderAllow = @("crates/qlr-render/src/media.rs", "crates/qlr-render/src/svg.rs")
$renderFiles = @($renderHits.Keys | ForEach-Object { RelName $_ })
$renderBad = @($renderFiles | Where-Object { $renderAllow -notcontains $_ })
Check "render 的 windows 用法在白名单内" ($renderBad.Count -eq 0) (
    $(if ($renderBad.Count -eq 0) {
        $detail = ($renderFiles | ForEach-Object { "$(($_ -split '/')[-1])=$($renderHits[$Root + '\' + ($_ -replace '/', '\')])" }) -join " "
        "$detail（Direct2D / Media Foundation 两处边界例外）"
    } else { "越界文件：" + ($renderBad -join ", ") })
)

# app 的白名单 + 棘轮：这些是 §6-4「裸 Win32 收口」还没做的存量，只许减不许增。
$appHits = Get-WindowsUsers "qlr-app"
$appCaps = @{
    "crates/qlr-app/src/app/mod.rs"  = 999   # prelude 再导出，收口后才可能消失
    "crates/qlr-app/src/layout.rs"   = 1     # RECT（纯几何，其实可以挪进 platform）
    "crates/qlr-app/src/settings.rs" = 2     # HWND/LPARAM/LRESULT/WPARAM + WindowsAndMessaging
}
$appViol = @()
foreach ($kv in $appHits.GetEnumerator()) {
    $rel = RelName $kv.Key
    if (-not $appCaps.ContainsKey($rel)) {
        $appViol += "$rel（$($kv.Value) 处，白名单外）"
    } elseif ($kv.Value -gt $appCaps[$rel]) {
        $appViol += "$rel（$($kv.Value) 处 > 上限 $($appCaps[$rel])）"
    }
}
Check "app 的 windows 用法没扩散" ($appViol.Count -eq 0) (
    $(if ($appViol.Count -eq 0) { "$($appHits.Count) 个文件，均在白名单内" } else { $appViol -join "; " })
)

# ---- 2b. app 不许直接调用 Win32 计时/退出/时钟原语（§6-4 收口到 qlr-platform）----
# 这些原语由 `qlr_platform::window` 封装提供（start_timer / stop_timer(s) / post_quit /
# terminate_self / now_ms）。app 直接调等于绕过平台层：换实现、加看门狗、做后端抽象都收不住。
$appPrim = @()
$appSrcFiles = Get-ChildItem (Join-Path $Root "crates\qlr-app\src") -Recurse -Filter *.rs
foreach ($f in $appSrcFiles) {
    $hits = Select-String -Path $f.FullName -Pattern '\b(KillTimer|SetTimer|PostQuitMessage|TerminateProcess|GetCurrentProcess|GetTickCount)\s*\(' -Encoding utf8
    foreach ($h in $hits) { $appPrim += "$(RelName $f.FullName):$($h.LineNumber)" }
}
Check "app 不直接调 Win32 计时/退出原语" ($appPrim.Count -eq 0) (
    $(if ($appPrim.Count -eq 0) { "全部走 qlr_platform::window" } else { $appPrim -join ", " })
)

# ---- 2c. §7 后端契约：app 侧不许再各自枚举 `viewer_for`（"谁来画"收口到 backend.rs）----
# 以前 show/render/shell 各自 `viewer_for(kind).is_some()` 判一遍，加一个后端要改一圈。
# 现在统一走 `backend::resolve(path, kind)`；`viewer_for` 只许出现在 backend.rs 的实现里。
$appViewerScatter = @()
foreach ($f in $appSrcFiles) {
    if ($f.Name -eq "backend.rs") { continue }
    $hits = Select-String -Path $f.FullName -Pattern 'viewer_for\s*\(' -Encoding utf8
    foreach ($h in $hits) { $appViewerScatter += "$(RelName $f.FullName):$($h.LineNumber)" }
}
Check "app 只在 backend.rs 里枚举 viewer_for" ($appViewerScatter.Count -eq 0) (
    $(if ($appViewerScatter.Count -eq 0) { "选择逻辑已收口到 backend.rs" } else { $appViewerScatter -join ", " })
)

# ---- 3. platform 不许调用上层的类型 ----
$platformFiles = Get-ChildItem (Join-Path $Root "crates\qlr-platform\src") -Recurse -Filter *.rs
$platformBad = @()
foreach ($f in $platformFiles) {
    $hit = Select-String -Path $f.FullName -Pattern 'use\s+qlr_(render|app)\b' -Encoding utf8
    if ($hit) { $platformBad += (RelName $f.FullName) }
}
Check "platform 不反向 use render/app" ($platformBad.Count -eq 0) ($platformBad -join ", ")

# ---- 4. app 的入口文件不该再长出 FFI ----
$mainRs = Join-Path $Root "crates\qlr-app\src\main.rs"
$mainHit = (Select-String -Path $mainRs -Pattern 'use\s+windows' -Encoding utf8 | Measure-Object).Count
Check "main.rs 零 use windows" ($mainHit -eq 0) "入口只该做参数解析与进程编排"

# ---- 汇总 ----
$failed = ($script:results | Where-Object { $_.结果 -eq "FAIL" }).Count
$passed = ($script:results | Where-Object { $_.结果 -eq "PASS" }).Count
$script:results | Format-Table -AutoSize | Out-String -Width 200 | Write-Host
Write-Output ("合计：{0} PASS · {1} FAIL（分层规则见 ARCH.md 第 1 节；改架构要同步改本脚本）" -f $passed, $failed)
exit $(if ($failed -gt 0) { 1 } else { 0 })
