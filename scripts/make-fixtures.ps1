# 生成基准/样本夹具（M6 A6.4/A6.7）
#
# 夹具不进仓库（图片/大文本动辄几十 MB），所以进 CI 前先用本脚本现场造。
#   pwsh -NoProfile -File scripts\make-fixtures.ps1                       # 默认一套（小，约 20 MB）
#   pwsh -NoProfile -File scripts\make-fixtures.ps1 -Big                  # 再加 48MP 图与 26 MB 文本
#   pwsh -NoProfile -File scripts\make-fixtures.ps1 -OutDir D:\tmp\fx     # 指定目录
#
# 产物（-OutDir 下的文件名就是 scripts\bench.ps1 里场景表的名字）：
#   png_1920x1080.png      UI 常见尺寸，验证 PNG 解码
#   8mp_3840x2160.jpg      4K，JPEG 解码热点
#   48mp_8000x6000.jpg     （-Big）超大图，验证缩放与内存上限
#   big_16mb.txt           中等文本，验证读取上限与虚拟化
#   big_100mb.txt          （-Big）大文本，超过 READ_LIMIT=8 MB 的截断路径
#   pdf_8page.pdf          8 页，验证 pdfium 绑定与翻页
#   md_styled.md           标题/代码/引用/列表/分隔线，验证 Markdown 排版通道
#
# 注意：工作区里的 exe 是 Low 完整性（CONVENTIONS #39），**别把默认目录设成 %TEMP%**。

param(
    [string]$OutDir = "",
    [switch]$Big,
    # 额外生成「样本矩阵」：每类 ≥20 个文件，供 A6.7 通过率统计
    [switch]$Matrix
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
if (-not $OutDir) { $OutDir = Join-Path $root "target\bench-fixtures" }
if (-not (Test-Path $OutDir)) { New-Item -ItemType Directory -Path $OutDir -Force | Out-Null }
$OutDir = (Resolve-Path $OutDir).Path
Write-Output ("夹具目录：{0}" -f $OutDir)

Add-Type -AssemblyName System.Drawing

function New-GradientImage {
    param([string]$Path, [int]$Width, [int]$Height, [string]$Format = "jpg", [long]$Quality = 90)
    $bmp = New-Object System.Drawing.Bitmap($Width, $Height, [System.Drawing.Imaging.PixelFormat]::Format24bppRgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $rect = New-Object System.Drawing.Rectangle(0, 0, $Width, $Height)
    $brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush($rect, [System.Drawing.Color]::FromArgb(20, 40, 90), [System.Drawing.Color]::FromArgb(240, 210, 150), 45.0)
    $g.FillRectangle($brush, $rect)
    # 几条斜纹，避免整图纯渐变被解码器特殊对待
    $pen = New-Object System.Drawing.Pen([System.Drawing.Color]::FromArgb(90, 255, 255, 255), [math]::Max(2, $Width / 400))
    for ($i = 0; $i -lt 12; $i++) {
        $x = $Width * $i / 12.0
        $g.DrawLine($pen, [single]$x, [single]0, [single]($x + $Width / 6), [single]$Height)
    }
    $font = New-Object System.Drawing.Font("Segoe UI", [math]::Max(14, $Width / 40), [System.Drawing.FontStyle]::Bold)
    $g.DrawString(("{0}x{1}" -f $Width, $Height), $font, [System.Drawing.Brushes]::White, 40, 40)
    $g.Dispose()
    $brush.Dispose()
    $pen.Dispose()
    $font.Dispose()

    if ($Format -eq "png") {
        $bmp.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
    } else {
        $codec = [System.Drawing.Imaging.ImageCodecInfo]::GetImageEncoders() | Where-Object { $_.MimeType -eq "image/jpeg" }
        $params = New-Object System.Drawing.Imaging.EncoderParameters(1)
        $params.Param[0] = New-Object System.Drawing.Imaging.EncoderParameter([System.Drawing.Imaging.Encoder]::Quality, $Quality)
        $bmp.Save($Path, $codec, $params)
    }
    $bmp.Dispose()
    Write-Output ("  {0}  {1:N0} 字节" -f (Split-Path -Leaf $Path), (Get-Item $Path).Length)
}

# 极简 PDF 生成：ASCII only，所以字符数 == 字节数，xref 偏移可以直接数出来
function New-Pdf {
    param([string]$Path, [int]$Pages = 8)
    $sb = New-Object System.Text.StringBuilder
    $offsets = New-Object System.Collections.Generic.List[int]
    $objectCount = 2 + 2 * $Pages   # 1=catalog 2=pages ... 每页一个 Page + 一个 Contents
    function Add-Obj([string]$body) {
        $offsets.Add($sb.Length)
        [void]$sb.Append($body)
    }
    [void]$sb.Append("%PDF-1.4`n")
    $kids = (3..(2 + 2 * $Pages) | Where-Object { ($_ % 2) -eq 1 } | ForEach-Object { "$_ 0 R" }) -join " "
    Add-Obj "1 0 obj`n<< /Type /Catalog /Pages 2 0 R >>`nendobj`n"
    Add-Obj ("2 0 obj`n<< /Type /Pages /Count $Pages /Kids [$kids] >>`nendobj`n")
    for ($i = 0; $i -lt $Pages; $i++) {
        $pageObj = 3 + 2 * $i
        $contentObj = $pageObj + 1
        Add-Obj ("$pageObj 0 obj`n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >> /Contents $contentObj 0 R >>`nendobj`n")
        $stream = "BT /F1 36 Tf 72 740 Td (QLRust bench page " + ($i + 1) + " / " + $Pages + ") Tj ET`n" +
                  "BT /F1 14 Tf 72 690 Td (The quick brown fox jumps over the lazy dog.) Tj ET`n" +
                  "BT /F1 14 Tf 72 665 Td (0123456789 ABCDEFGHIJKLMNOPQRSTUVWXYZ abcdefghijklmnopqrstuvwxyz) Tj ET`n"
        for ($line = 0; $line -lt 20; $line++) {
            $stream += ("BT /F1 11 Tf 72 " + (630 - 22 * $line) + " Td (line " + ($line + 1) + " of synthetic page content for rendering checks.) Tj ET`n")
        }
        Add-Obj ("$contentObj 0 obj`n<< /Length " + $stream.Length + " >>`nstream`n" + $stream + "endstream`nendobj`n")
    }
    $xrefPos = $sb.Length
    [void]$sb.Append("xref`n0 " + ($objectCount + 1) + "`n")
    [void]$sb.Append("0000000000 65535 f `n")
    foreach ($off in $offsets) { [void]$sb.Append(("{0:D10} 00000 n `n" -f $off)) }
    [void]$sb.Append("trailer`n<< /Size " + ($objectCount + 1) + " /Root 1 0 R >>`nstartxref`n$xrefPos`n%%EOF`n")
    [System.IO.File]::WriteAllText($Path, $sb.ToString(), (New-Object System.Text.ASCIIEncoding))
    Write-Output ("  {0}  {1:N0} 字节" -f (Split-Path -Leaf $Path), (Get-Item $Path).Length)
}

function New-Text {
    param([string]$Path, [long]$TargetBytes)
    $line = "2026-10-31T09:00:00.123Z INFO  [ThreadId(7)] bench  the quick brown fox jumps over the lazy dog 0123456789 中文示例文本用于折行测试`n"
    $lineBytes = [System.Text.Encoding]::UTF8.GetByteCount($line)
    $repeat = [int][math]::Ceiling($TargetBytes / $lineBytes)
    $sb = New-Object System.Text.StringBuilder ($repeat * $line.Length)
    for ($i = 0; $i -lt $repeat; $i++) { [void]$sb.Append($line) }
    [System.IO.File]::WriteAllText($Path, $sb.ToString(), (New-Object System.Text.UTF8Encoding($false)))
    Write-Output ("  {0}  {1:N0} 字节" -f (Split-Path -Leaf $Path), (Get-Item $Path).Length)
}

function New-Markdown {
    param([string]$Path)
    $sb = New-Object System.Text.StringBuilder
    [void]$sb.Append("# QLRust 排版夹具`n`n")
    [void]$sb.Append("这一段是普通正文，用来验证按宽度折行、以及折行后翻页不会跳过内容。Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.`n`n")
    for ($i = 1; $i -le 6; $i++) {
        [void]$sb.Append("## 二级标题 $i`n`n")
        [void]$sb.Append("正文第 $i 段：含中文与 ASCII 混排，验证行宽测量对宽字符的处理是否与绘制一致。`n`n")
        [void]$sb.Append("- 列表项 A$i`n- 列表项 B$i，故意写长一点让它在窄窗口里折行：the quick brown fox jumps over the lazy dog again and again.`n- 列表项 C$i`n`n")
        [void]$sb.Append("> 引用块 $i：行高固定 24 px，只变颜色与字重。`n`n")
        [void]$sb.Append("``````rust`nfn page_$i(rows: usize) -> usize {`n    rows.saturating_sub(1)`n}`n``````n`n")
        [void]$sb.Append("### 三级标题 $i`n`n---`n`n")
    }
    [System.IO.File]::WriteAllText($Path, $sb.ToString(), (New-Object System.Text.UTF8Encoding($false)))
    Write-Output ("  {0}  {1:N0} 字节" -f (Split-Path -Leaf $Path), (Get-Item $Path).Length)
}

Write-Output "生成中："
New-GradientImage -Path (Join-Path $OutDir "png_1920x1080.png") -Width 1920 -Height 1080 -Format png
New-GradientImage -Path (Join-Path $OutDir "8mp_3840x2160.jpg") -Width 3840 -Height 2160
if ($Big) { New-GradientImage -Path (Join-Path $OutDir "48mp_8000x6000.jpg") -Width 8000 -Height 6000 }
New-Text -Path (Join-Path $OutDir "big_16mb.txt") -TargetBytes (16MB)
if ($Big) { New-Text -Path (Join-Path $OutDir "big_100mb.txt") -TargetBytes (26MB) }
New-Pdf -Path (Join-Path $OutDir "pdf_8page.pdf") -Pages 8
New-Markdown -Path (Join-Path $OutDir "md_styled.md")

if ($Matrix) {
    $matrixDir = Join-Path $OutDir "matrix"
    foreach ($sub in @("image", "text", "markdown", "pdf")) {
        $p = Join-Path $matrixDir $sub
        if (-not (Test-Path $p)) { New-Item -ItemType Directory -Path $p -Force | Out-Null }
    }
    Write-Output ""
    Write-Output "样本矩阵（A6.7，每类 ≥20 个）："

    # ---- 图片：覆盖解码器表里全部格式 × 有无 alpha × 大中小尺寸 ----
    $sizes = @(@(64, 48), @(320, 200), @(1024, 768), @(1920, 1080), @(2048, 1152))
    $imgFormats = @(
        @{ Ext = "png"; Fmt = [System.Drawing.Imaging.ImageFormat]::Png },
        @{ Ext = "jpg"; Fmt = [System.Drawing.Imaging.ImageFormat]::Jpeg },
        @{ Ext = "bmp"; Fmt = [System.Drawing.Imaging.ImageFormat]::Bmp },
        @{ Ext = "gif"; Fmt = [System.Drawing.Imaging.ImageFormat]::Gif },
        @{ Ext = "tif"; Fmt = [System.Drawing.Imaging.ImageFormat]::Tiff }
    )
    $n = 0
    foreach ($f in $imgFormats) {
        foreach ($s in $sizes) {
            $n++
            $path = Join-Path $matrixDir ("image\{0:D2}_{1}x{2}.{3}" -f $n, $s[0], $s[1], $f.Ext)
            New-GradientImage -Path $path -Width $s[0] -Height $s[1] -Format $f.Ext
        }
    }
    # 换色系再来一批，避免"同一张图只是尺寸不同"被解码器缓存/启发式掩盖
    for ($i = 0; $i -lt 6; $i++) {
        $n++
        New-GradientImage -Path (Join-Path $matrixDir ("image\{0:D2}_tone{1}.png" -f $n, $i)) -Width (200 + 137 * $i) -Height (150 + 91 * $i) -Format png
    }

    # ---- 文本：编码 × 结构（CRLF/LF、BOM、超长行、空文件、只有换行）----
    $encCases = @(
        @{ Name = "utf8"; Enc = New-Object System.Text.UTF8Encoding($false) },
        @{ Name = "utf8bom"; Enc = New-Object System.Text.UTF8Encoding($true) },
        @{ Name = "utf16le"; Enc = New-Object System.Text.UnicodeEncoding($false, $true) },
        @{ Name = "ascii"; Enc = New-Object System.Text.ASCIIEncoding },
        @{ Name = "gb18030"; Enc = [System.Text.Encoding]::GetEncoding("GB18030") }
    )
    $n = 0
    foreach ($e in $encCases) {
        $n++
        $body = "第一行：中文编码测试 $($e.Name)`r`nsecond line ascii`r`n第三行 tab`there`r`n"
        [System.IO.File]::WriteAllText((Join-Path $matrixDir ("text\{0:D2}_{1}.txt" -f $n, $e.Name)), $body, $e.Enc)
        $n++
        $body2 = ("log line {0} — the quick brown fox`n" -f $n) * 40
        [System.IO.File]::WriteAllText((Join-Path $matrixDir ("text\{0:D2}_{1}_logs.log" -f $n, $e.Name)), $body2, $e.Enc)
    }
    $n++
    [System.IO.File]::WriteAllText((Join-Path $matrixDir ("text\{0:D2}_crlf.md" -f $n)), "# t`r`n`r`n- a`r`n", (New-Object System.Text.UTF8Encoding($false)))
    $n++
    [System.IO.File]::WriteAllBytes((Join-Path $matrixDir ("text\{0:D2}_empty.txt" -f $n)), @())
    $n++
    [System.IO.File]::WriteAllText((Join-Path $matrixDir ("text\{0:D2}_only-newlines.txt" -f $n)), "`n`n`n`n`n", (New-Object System.Text.UTF8Encoding($false)))
    $n++
    [System.IO.File]::WriteAllText((Join-Path $matrixDir ("text\{0:D2}_one-long-line.txt" -f $n)), ("x" * 500000), (New-Object System.Text.UTF8Encoding($false)))
    $n++
    [System.IO.File]::WriteAllText((Join-Path $matrixDir ("text\{0:D2}_json.json" -f $n)), ('{"a":1,"b":[2,3],"c":"中文"}'), (New-Object System.Text.UTF8Encoding($false)))
    $n++
    [System.IO.File]::WriteAllText((Join-Path $matrixDir ("text\{0:D2}_csv.csv" -f $n)), "a,b,c`r`n1,2,3`r`n", (New-Object System.Text.UTF8Encoding($false)))
    $n++
    [System.IO.File]::WriteAllText((Join-Path $matrixDir ("text\{0:D2}_code.rs" -f $n)), "fn main() {`n    println!(`"hi`");`n}`n", (New-Object System.Text.UTF8Encoding($false)))
    $n++
    [System.IO.File]::WriteAllText((Join-Path $matrixDir ("text\{0:D2}_cjk.txt" -f $n)), ("中文宽字符折行测试 " * 200), (New-Object System.Text.UTF8Encoding($false)))
    # 再补几条边角，把 text 类凑到 24 个（A6.7 要求每类 ≥20）
    $n++
    [System.IO.File]::WriteAllText((Join-Path $matrixDir ("text\{0:D2}_utf16be.txt" -f $n)), "utf16 big endian 文本`r`n", (New-Object System.Text.UnicodeEncoding($true, $true)))
    $n++
    [System.IO.File]::WriteAllText((Join-Path $matrixDir ("text\{0:D2}_no-trailing-newline.txt" -f $n)), "最后一行没有换行符", (New-Object System.Text.UTF8Encoding($false)))
    $n++
    $chunk = ("0123456789abcdef 一行用来撑到 1 MB 的文本`n") * 30000
    [System.IO.File]::WriteAllText((Join-Path $matrixDir ("text\{0:D2}_1mb.txt" -f $n)), $chunk, (New-Object System.Text.UTF8Encoding($false)))
    $n++
    [System.IO.File]::WriteAllText((Join-Path $matrixDir ("text\{0:D2}_tabs.tsv" -f $n)), "a`tb`tc`r`n1`t2`t3`r`n", (New-Object System.Text.UTF8Encoding($false)))
    $n++
    [System.IO.File]::WriteAllText((Join-Path $matrixDir ("text\{0:D2}_mixed-eol.txt" -f $n)), "line1`nline2`r`nline3`rline4`n", (New-Object System.Text.UTF8Encoding($false)))
    $n++
    [System.IO.File]::WriteAllText((Join-Path $matrixDir ("text\{0:D2}_ini.ini" -f $n)), "[section]`r`nkey=value`r`n", (New-Object System.Text.UTF8Encoding($false)))

    # ---- Markdown：块级元素全覆盖 + 若干边角 ----
    $n = 0
    for ($i = 1; $i -le 10; $i++) {
        $n++
        $md = "# 文档 $i`n`n正文段落 $i，含中文与 ASCII。`n`n## 二级 $i`n`n- 项 A$i`n- 项 B$i`n`n> 引用 $i`n`n``````rust`nfn f$i() {}`n```````n`n---`n`n### 三级 $i`n"
        [System.IO.File]::WriteAllText((Join-Path $matrixDir ("markdown\{0:D2}_doc{1}.md" -f $n, $i)), $md, (New-Object System.Text.UTF8Encoding($false)))
    }
    $n++
    [System.IO.File]::WriteAllText((Join-Path $matrixDir ("markdown\{0:D2}_unclosed-fence.md" -f $n)), "```ncode never closed`n", (New-Object System.Text.UTF8Encoding($false)))
    $n++
    [System.IO.File]::WriteAllText((Join-Path $matrixDir ("markdown\{0:D2}_no-blocks.md" -f $n)), ("纯段落文本 " * 100), (New-Object System.Text.UTF8Encoding($false)))
    $n++
    [System.IO.File]::WriteAllText((Join-Path $matrixDir ("markdown\{0:D2}_deep-headings.md" -f $n)), "# 1`n## 2`n### 3`n#### 4`n##### 5`n###### 6`n", (New-Object System.Text.UTF8Encoding($false)))
    $n++
    [System.IO.File]::WriteAllText((Join-Path $matrixDir ("markdown\{0:D2}_empty.md" -f $n)), "", (New-Object System.Text.UTF8Encoding($false)))
    for ($i = 1; $i -le 8; $i++) {
        $n++
        $items = ("- item " + $i + "`n") * 20
        [System.IO.File]::WriteAllText((Join-Path $matrixDir ("markdown\{0:D2}_list{1}.md" -f $n, $i)), $items, (New-Object System.Text.UTF8Encoding($false)))
    }

    # ---- PDF：1/2/8/20 页各来几份 ----
    foreach ($pages in @(1, 2, 8, 20)) {
        for ($k = 1; $k -le 5; $k++) {
            New-Pdf -Path (Join-Path $matrixDir ("pdf\{0}_{1}page_v{2}.pdf" -f $pages, $pages, $k)) -Pages $pages
        }
    }

    $counts = @{}
    foreach ($sub in @("image", "text", "markdown", "pdf")) {
        $counts[$sub] = (Get-ChildItem (Join-Path $matrixDir $sub) -File | Measure-Object).Count
    }
    Write-Output ("  矩阵：image {0} / text {1} / markdown {2} / pdf {3}" -f $counts["image"], $counts["text"], $counts["markdown"], $counts["pdf"])
}

Write-Output ""
Write-Output ("完成：{0} 个文件，合计 {1:N1} MB" -f
    (Get-ChildItem $OutDir | Measure-Object).Count,
    ((Get-ChildItem $OutDir | Measure-Object -Property Length -Sum).Sum / 1MB))
