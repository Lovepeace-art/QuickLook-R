//! 畸形输入回归 + 简易 fuzz（WBS A6.5）。
//!
//! 目标不是"证明解析器无 bug"，而是把三类必然踩到的东西钉成回归：
//!   1. **绝不 panic**：渲染是在常驻 worker 线程里跑的，一次 panic 会把 shell 一起带走
//!      （用户看到的是"按了空格什么都没发生"，日志里一行都没有）。
//!   2. **错就返回 `Err`**：返回的错误文本要能给人看（壳层直接显示）。
//!   3. **不 OOM**：截断/超长行/离谱尺寸必须被预算挡住（A8.5：单张图 ≤ 64 MP）。
//!
//! 为什么不做真的 fuzz（cargo-fuzz/libFuzzer）：那需要 nightly + 专用 corpus，
//! 与"每次 `cargo test` 都能跑、10 秒内跑完"冲突。这里用确定性伪随机（xorshift64）
//! 对真实样本做字节级变异，跑固定 320 例 —— 每轮结果可复现，失败时能按种子定位。

use std::path::{Path, PathBuf};

use qlr_render::render_file;

/// 测试临时目录（不用 `%TEMP%`：见 crates/qlr-render/src/testutil.rs 的说明，
/// 工作区里的 exe 是 Low 完整性，写 `%TEMP%` 会被拒）。
fn dir(tag: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/test-tmp/malformed")
        .join(tag);
    std::fs::create_dir_all(&path).expect("创建畸形输入测试目录失败");
    path
}

fn write(name: &str, file: &str, data: &[u8]) -> PathBuf {
    let path = dir(name).join(file);
    std::fs::write(&path, data).expect("写畸形输入夹具失败");
    path
}

/// 渲染一次，只断言"没 panic 且返回类型合法"。
/// 返回 `Some(msg)` 表示这是 `Err`（调用方可选地检查文本）。
fn render_quietly(path: &Path) -> Option<String> {
    match render_file(path, 1024, 768) {
        Ok(canvas) => {
            // 出来的画布必须是自洽的：位图有像素、文本有行
            match canvas {
                qlr_render::Canvas::Bitmap(b) => {
                    assert!(
                        b.width > 0 && b.height > 0,
                        "位图尺寸非法：{}",
                        path.display()
                    );
                    assert_eq!(
                        b.bgra.len(),
                        (b.width as usize) * (b.height as usize) * 4,
                        "位图字节数与尺寸不符：{}",
                        path.display()
                    );
                }
                qlr_render::Canvas::Text(t) => {
                    assert_eq!(
                        t.styles.as_ref().map(|s| s.len()),
                        t.styles.as_ref().map(|_| t.lines.len())
                    );
                    assert!(
                        t.total_lines >= t.lines.len(),
                        "总行数小于可见行：{}",
                        path.display()
                    );
                }
                // 表格帧（1.9.0）也是合法的：坏 xlsx 要么 Err、要么退化成空表，
                // 但绝不能是"N 行 M 列却一个 Sheet 都没有"这种自相矛盾的样子。
                qlr_render::Canvas::Sheet(d) => {
                    assert!(
                        !d.sheets.is_empty(),
                        "表格帧至少要有 1 张表：{}",
                        path.display()
                    );
                }
            }
            None
        }
        Err(e) => Some(e.to_string()),
    }
}

/// 最小可用的 1x1 PNG（合法样本，用来做变异种子）。
const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // \x89PNG\r\n\x1a\n
    0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR 长度 + 类型
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1x1
    0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, // 8bit RGBA + crc
    0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, // IDAT 长度 + 类型
    0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, // 压缩数据
    0x0D, 0x0A, 0x2D, 0xB4, // crc
    0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82, // IEND
];

/// 手搓的最小 PDF（1 页，Helvetica 一行字）。xref 偏移是数出来的，
/// 改字符串时长度就变了 —— 这是故意的：pdfium 对坏 xref 很宽容，
/// 但坏 `/Length` 会报错，正好覆盖"能打开但内容坏"这一类。
fn minimal_pdf() -> Vec<u8> {
    let body = "%PDF-1.4\n\
1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
2 0 obj\n<< /Type /Pages /Count 1 /Kids [3 0 R] >>\nendobj\n\
3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R >>\nendobj\n\
4 0 obj\n<< /Length 44 >>\nstream\n\
BT /F1 12 Tf 20 100 Td (hello qlrust) Tj ET\n\
endstream\nendobj\n\
trailer\n<< /Size 5 /Root 1 0 R >>\n%%EOF\n";
    body.as_bytes().to_vec()
}

#[test]
fn empty_files_do_not_panic() {
    // 位图/PDF 的空文件必须报错（解码器无内容可解）；
    // 文本/Markdown 的空文件是**合法**的（就是空文档），返回空画布即可，不许 panic。
    for (file, must_error) in [
        ("a.png", true),
        ("a.jpg", true),
        ("a.pdf", true),
        ("a.txt", false),
        ("a.md", false),
    ] {
        let path = write("empty", file, b"");
        let err = render_quietly(&path);
        if must_error {
            assert!(err.is_some(), "空文件应当报错而不是给出空画布：{file}");
        } else if let Some(msg) = err {
            panic!("空文本应当渲染成空画布而不是报错（{file}）：{msg}");
        }
    }
}

#[test]
fn truncated_images_report_errors() {
    // 完整 PNG 截掉一半：解码器必须报错，不能读到越界内存
    let half = &PNG_1X1[..PNG_1X1.len() / 2];
    let path = write("trunc", "half.png", half);
    assert!(render_quietly(&path).is_some(), "截断 PNG 应当报错");

    // 只有魔数的 JPEG
    let path = write("trunc", "magic-only.jpg", &[0xFF, 0xD8, 0xFF]);
    assert!(render_quietly(&path).is_some(), "只有魔数的 JPEG 应当报错");

    // 声称 60000x60000 的 PNG（1.44 GB 像素）—— 必须被尺寸预算挡住
    let mut huge = PNG_1X1.to_vec();
    huge[16..20].copy_from_slice(&60000u32.to_be_bytes());
    huge[20..24].copy_from_slice(&60000u32.to_be_bytes());
    let path = write("trunc", "huge-header.png", &huge);
    assert!(render_quietly(&path).is_some(), "超预算尺寸应当报错");
}

#[test]
fn broken_pdf_reports_error_without_crashing() {
    for (file, data) in [
        (
            "garbage.pdf",
            b"%PDF-1.4\nthis is not a pdf at all\n".to_vec(),
        ),
        ("header-only.pdf", b"%PDF-1.7\n".to_vec()),
        (
            "zero-page.pdf",
            b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog >>\nendobj\ntrailer\n<< /Root 1 0 R >>\n%%EOF\n"
                .to_vec(),
        ),
    ] {
        let path = write("pdf", file, &data);
        // pdfium 不在机器上时也会返回 Err（"缺少 pdfium.dll"），同样算通过
        assert!(render_quietly(&path).is_some(), "坏 PDF 应当报错：{file}");
    }
    // 合法 PDF 是否成功取决于 pdfium 是否就位，两种结果都允许，但都不许 panic
    let path = write("pdf", "minimal.pdf", &minimal_pdf());
    let _ = render_quietly(&path);
}

#[test]
fn binary_and_odd_text_are_handled() {
    // 含 NUL：文本管线应当直说"看起来是二进制"。
    // 注意文件名不能叫 `nul.txt` —— Windows 上 `nul` 是设备名，写进去等于丢进黑洞，
    // 读回来是 0 字节，会让人误以为"含 NUL 也能渲染"。
    let path = write("text", "with-nul.txt", b"abc\0def\0ghi");
    assert!(render_quietly(&path).is_some(), "含 NUL 的文本应当报错");

    // 非法 UTF-8：允许有损解码（GB18030/UTF-8 启发），但不能 panic
    let path = write(
        "text",
        "bad-utf8.txt",
        &[0x41, 0x42, 0xFF, 0xFE, 0x43, 0x0A],
    );
    let _ = render_quietly(&path);

    // 10 MB 的单行（无换行）：MAX_LINE_CHARS 要把它截断，不能整行塞进画布
    let long = vec![b'x'; 10 * 1024 * 1024];
    let path = write("text", "one-long-line.txt", &long);
    match render_file(&path, 1024, 768) {
        Ok(qlr_render::Canvas::Text(t)) => {
            assert!(!t.lines.is_empty(), "超长单行应当至少产出一行");
            assert!(
                t.lines[0].chars().count() <= qlr_render::text::MAX_LINE_CHARS + 1,
                "单行没有被截断：{} 字符",
                t.lines[0].chars().count()
            );
        }
        Ok(_) => panic!("文本文件不该出位图"),
        Err(e) => panic!("超长单行应当能渲染（截断后）：{e}"),
    }
}

#[test]
fn pathological_markdown_is_bounded() {
    // 未闭合围栏 + 几千个 # + 超长列表行：排版器不能崩，也不能把内存吃光
    let mut md = String::new();
    md.push_str("```\n"); // 围栏一直不闭合
    for i in 0..2000 {
        md.push_str(&"#".repeat(i % 50));
        md.push_str(" heading-ish\n");
        md.push_str("- ");
        md.push_str(&"x".repeat(i % 500));
        md.push('\n');
    }
    let path = write("md", "pathological.md", md.as_bytes());
    match render_file(&path, 1024, 768) {
        Ok(qlr_render::Canvas::Text(t)) => {
            assert_eq!(
                t.styles.as_ref().map(|s| s.len()),
                Some(t.lines.len()),
                "样式数组必须与行一一对应"
            );
        }
        other => panic!("病态 Markdown 应当渲染成文本画布：{other:?}"),
    }
}

/// xorshift64*：确定性伪随机，失败能按种子复现。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

fn mutate(seed: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut out = seed.to_vec();
    if out.is_empty() {
        return out;
    }
    // 1~8 处随机单字节改写
    let hits = 1 + rng.below(8);
    for _ in 0..hits {
        let i = rng.below(out.len());
        out[i] = (rng.next() & 0xFF) as u8;
    }
    // 一半概率再砍掉尾巴（模拟传输截断）
    if rng.next() % 2 == 0 {
        let keep = 1 + rng.below(out.len());
        out.truncate(keep);
    }
    out
}

#[test]
fn fuzz_mutated_samples_never_panic() {
    // 预算：320 例 × 小文件，本机 ~1 秒
    let seeds: [(&str, Vec<u8>, &str); 3] = [
        ("fuzz", PNG_1X1.to_vec(), "f.png"),
        ("fuzz", minimal_pdf(), "f.pdf"),
        (
            "fuzz",
            b"# title\n\n- item\n```rust\nfn x() {}\n```\n> quote\n---\n".to_vec(),
            "f.md",
        ),
    ];
    let mut rng = Rng(0x5EED_1234_ABCD_5678);
    let mut cases = 0;
    for (tag, seed, file) in seeds {
        for _ in 0..(320 / 3) {
            let data = mutate(&seed, &mut rng);
            let path = write(tag, file, &data);
            // 只关心"别崩"：Ok/Err 都合法
            let _ = render_file(&path, 1024, 768);
            cases += 1;
        }
    }
    assert_eq!(cases, 318, "固定跑 318 例（320/3 取整）");
}

#[test]
fn unsupported_kind_never_reaches_a_parser() {
    // 八类之外的扩展名必须直接拒绝，不能落到某个渲染器里
    let path = write("unsupported", "doc.docx", b"PK\x03\x04");
    let err = render_file(&path, 1024, 768).unwrap_err().to_string();
    assert!(err.contains("插件"), "应当提示走插件：{err}");
}

#[test]
fn broken_video_degrades_to_card_instead_of_error() {
    // 坏的 mp4：不能 panic，也不能返回 Err——退回"元信息卡片"，用户至少知道文件坏了。
    // 注意 `media::probe` 在坏文件上可能返回 None（属性读不到），卡片仍然要出得来。
    let path = write(
        "unsupported",
        "broken.mp4",
        b"\x00\x00\x00\x18ftypmp42garbage",
    );
    let canvas = render_file(&path, 1024, 768).expect("坏视频应降级成卡片而不是 Err");
    match canvas {
        qlr_render::Canvas::Text(doc) => {
            assert!(!doc.lines.is_empty(), "卡片不能是空的");
            assert!(
                doc.lines.iter().any(|l| l.contains("broken.mp4")),
                "卡片应带文件名：{:?}",
                doc.lines
            );
        }
        qlr_render::Canvas::Bitmap(b) => panic!("坏视频不该解出位图：{}x{}", b.width, b.height),
        // 视频只能是卡片或位图；表格帧出现在这里就是类别串了。
        qlr_render::Canvas::Sheet(_) => panic!("坏视频不该解出表格：{}", path.display()),
    }
}

#[test]
fn broken_svg_and_html_do_not_panic() {
    for (name, ext) in [("broken", "svg"), ("broken", "html")] {
        let path = write(
            "unsupported",
            &format!("{name}.{ext}"),
            b"<svg><g><path d='M0 0 L'",
        );
        // SVG 两种后端都可能放弃 → Err 合法；HTML 只是排版 → 通常 Ok。都不许 panic。
        let _ = render_file(&path, 400, 300);
    }
}
