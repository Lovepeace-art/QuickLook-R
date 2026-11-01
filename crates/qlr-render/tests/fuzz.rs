//! 确定性 fuzz（1.2.0）：把解码器当成不可信输入处理器来考。
//!
//! 为什么不用 `cargo-fuzz`（libFuzzer）：它要 nightly + `-Z sanitizer`，CI 里跑不动；
//! 而这套变异是**确定性**的（固定种子），一条失败能立刻重放，失败样本直接进
//! `malformed.rs` 做回归——对我们这种"四种格式 + 手写 PDF 绑定"的规模够用。
//!
//! 规模：默认 2000 例（秒级，跟着 `cargo test` 跑）；`QLR_FUZZ_ITERS=10000` 给夜间任务。
//! 判据只有一条：**不 panic、不 abort、不超时**（返回 Err 是合法结果）。

use std::path::PathBuf;

use qlr_render::render_file_at_zoom;

/// 迭代次数：默认 2000，可用 `QLR_FUZZ_ITERS` 覆盖（夜间任务用 10000）。
fn iterations() -> usize {
    std::env::var("QLR_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0 && *n <= 200_000)
        .unwrap_or(2000)
}

/// 夹具目录：必须放 `target/`（工作区可执行文件是 Low 完整性，写不了 `%TEMP%`）。
fn work_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/qlrust-fuzz");
    std::fs::create_dir_all(&dir).expect("建 fuzz 夹具目录");
    dir
}

/// xorshift64*：确定性变异源，宁可朴素也不要引入依赖。
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
            return 0;
        }
        (self.next() % n as u64) as usize
    }
    fn byte(&mut self) -> u8 {
        (self.next() >> 33) as u8
    }
}

/// 1×1 的合法 PNG（67 字节，`malformed.rs` 里也用它）。
const PNG_1PX: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

/// 最小可解析 PDF（一页，手搓 xref）。
fn tiny_pdf() -> Vec<u8> {
    let body = "1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
                2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
                3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]>>endobj\n";
    let mut out = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for chunk in body.split_inclusive("endobj\n") {
        offsets.push(out.len());
        out.push_str(chunk);
    }
    let xref_at = out.len();
    out.push_str(&format!(
        "xref\n0 {}\n0000000000 65535 f \n",
        offsets.len() + 1
    ));
    for off in &offsets {
        out.push_str(&format!("{off:010} 00000 n \n"));
    }
    out.push_str(&format!(
        "trailer<</Size {}/Root 1 0 R>>\nstartxref\n{xref_at}\n%%EOF\n",
        offsets.len() + 1
    ));
    out.into_bytes()
}

struct Seed {
    name: &'static str,
    ext: &'static str,
    bytes: Vec<u8>,
}

fn seeds() -> Vec<Seed> {
    vec![
        Seed {
            name: "png",
            ext: "png",
            bytes: PNG_1PX.to_vec(),
        },
        Seed {
            name: "pdf",
            ext: "pdf",
            bytes: tiny_pdf(),
        },
        Seed {
            name: "text",
            ext: "txt",
            bytes: "第一行\nsecond line\r\n第三行\ttab\n".as_bytes().to_vec(),
        },
        Seed {
            name: "md",
            ext: "md",
            bytes: "# 标题\n\n- 列表项\n\n```rs\nfn main() {}\n```\n\n> 引用\n"
                .as_bytes()
                .to_vec(),
        },
        Seed {
            name: "code",
            ext: "rs",
            bytes: "// 注释\nfn main() { let x = 0xFF; let s = \"str\"; }\n"
                .as_bytes()
                .to_vec(),
        },
    ]
}

/// 施加一次随机变异：位翻转、字节替换、截断、插入、复制粘贴。
fn mutate(rng: &mut Rng, base: &[u8]) -> Vec<u8> {
    let mut out = base.to_vec();
    if out.is_empty() {
        return out;
    }
    match rng.below(5) {
        // 单字节位翻转（改 header 字段的经典手法）
        0 => {
            let at = rng.below(out.len());
            out[at] ^= 1 << rng.below(8);
        }
        // 若干个字节替换成随机值
        1 => {
            for _ in 0..1 + rng.below(8) {
                let at = rng.below(out.len());
                out[at] = rng.byte();
            }
        }
        // 截断（半截文件是最常见的一类坏输入）
        2 => {
            let keep = 1 + rng.below(out.len());
            out.truncate(keep);
        }
        // 插入一段随机字节（打乱长度/偏移字段）
        3 => {
            let at = rng.below(out.len());
            let n = 1 + rng.below(16);
            let mut insert = Vec::with_capacity(n);
            for _ in 0..n {
                insert.push(rng.byte());
            }
            out.splice(at..at, insert);
        }
        // 复制一段到自己身上（制造超长字段/重复块）
        _ => {
            let from = rng.below(out.len());
            let len = (1 + rng.below(32)).min(out.len() - from);
            let chunk: Vec<u8> = out[from..from + len].to_vec();
            let at = rng.below(out.len());
            out.splice(at..at, chunk);
        }
    }
    out
}

#[test]
fn mutated_samples_never_panic_or_hang() {
    let dir = work_dir();
    let start = std::time::Instant::now();
    let total = iterations();
    let seeds = seeds();
    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    let mut errors = 0usize;
    let mut oks = 0usize;

    for i in 0..total {
        let seed = &seeds[i % seeds.len()];
        let data = mutate(&mut rng, &seed.bytes);
        let path = dir.join(format!("fuzz-{}-{i}.{}", seed.name, seed.ext));
        std::fs::write(&path, &data).expect("写 fuzz 夹具");
        // 任何一个 panic 都会让测试失败；Err 是合法结果，只做统计。
        if render_file_at_zoom(&path, 64, 64, 0, false).is_ok() {
            oks += 1;
        } else {
            errors += 1;
        }
        let _ = std::fs::remove_file(&path);
    }

    let elapsed = start.elapsed();
    // 不许有"每次都成功"或"每次都失败"这种退化（说明变异没生效 / 入口被短路）。
    assert!(errors > 0, "{total} 例全部成功，变异多半没生效");
    assert!(oks > 0, "{total} 例全部失败，入口可能被短路了");
    // 兜底的时间上限：本地 2–5 s，夜间 20–40 s；超过就说明有路径卡住了。
    assert!(
        elapsed.as_secs() < 600,
        "{total} 例花了 {elapsed:?}，疑似有输入把解析器卡住"
    );
}
