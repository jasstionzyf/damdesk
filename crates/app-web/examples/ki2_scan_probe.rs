//! KI-2（卡 task）探针：为「库外扫描」测试挑选**判据可分辨**的像素图案。
//!
//! 为什么需要它：pHash 是低频判据 —— 只差整体亮度或条纹相位的规则图案，DCT 低频谱几乎
//! 一样，汉明距离会小到 ≤8 而被判成「近似重复」。用这种图当 fixture 会让「无关图」也
//! 落进同一组：那是测试数据造得假，不是实现错。
//!
//! 本探针**走真实解码路径**（`core_media::luma32`，与 `dedup_api::scan` 完全同一条链），
//! 把候选图案的两两 D4 最小汉明距离打出来，据此挑 fixture。
//!
//! 跑法（容器内）：`cargo run -p app-web --example ki2_scan_probe`

use std::path::Path;

fn pattern(kind: &str) -> Vec<u8> {
    let mut d = vec![0u8; 64 * 64];
    for y in 0..64 {
        for x in 0..64 {
            let v: u8 = match kind {
                "quad" => match (x < 32, y < 32) {
                    (true, true) => 235,
                    (false, true) => 20,
                    (true, false) => 20,
                    (false, false) => 180,
                },
                "quad_c" => match (x < 32, y < 34) {
                    (true, true) => 235,
                    (false, true) => 20,
                    (true, false) => 20,
                    (false, false) => 180,
                },
                "half" => {
                    if y < 32 {
                        235
                    } else {
                        20
                    }
                }
                "leftright" => {
                    if x < 32 {
                        235
                    } else {
                        20
                    }
                }
                "blocknoise" => {
                    let bx = (x / 8) as u64;
                    let by = (y / 8) as u64;
                    let mut s = 0x9e37_79b9u64 ^ (bx << 8) ^ (by << 24) ^ (bx * 31 + by * 17);
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    (s & 0xff) as u8
                }
                "diag" => {
                    if ((x + y) / 6) % 2 == 0 {
                        235
                    } else {
                        20
                    }
                }
                "checker" => {
                    if ((x / 4) + (y / 4)) % 2 == 0 {
                        235
                    } else {
                        20
                    }
                }
                "grad_x" => (x * 4) as u8,
                _ => 128,
            };
            d[y * 64 + x] = v;
        }
    }
    d
}

fn write_png(path: &Path, data: &[u8]) {
    let img = image::GrayImage::from_raw(64, 64, data.to_vec()).unwrap();
    img.save(path).unwrap();
}

fn main() {
    let kinds = [
        "quad",
        "quad_c",
        "half",
        "leftright",
        "blocknoise",
        "diag",
        "checker",
        "grad_x",
    ];
    let dir = std::env::temp_dir().join("ki2-scan-probe");
    std::fs::create_dir_all(&dir).unwrap();
    let mut hashes: Vec<(&str, [u64; 8])> = Vec::new();
    for k in kinds {
        let p = dir.join(format!("{k}.png"));
        write_png(&p, &pattern(k));
        let luma = core_media::luma32(&p).expect("真实解码失败");
        let hs = engine_dedup::d4_phashes(&luma).expect("pHash 失败");
        println!("{k:<12} identity={:016x}", hs[0]);
        hashes.push((k, hs));
    }
    println!("\n两两最小汉明距离（走 core_media::luma32 真实解码，8×8 变体组合最小值）：");
    print!("{:<12}", "");
    for (k, _) in &hashes {
        print!("{k:>12}");
    }
    println!();
    for (i, (ka, ha)) in hashes.iter().enumerate() {
        print!("{ka:<12}");
        for (j, (_, hb)) in hashes.iter().enumerate() {
            let d = if i == j {
                0
            } else {
                engine_dedup::best_variant_match(ha, hb)
                    .map(|m| m.distance)
                    .unwrap_or(999)
            };
            let mark = if i != j && d <= engine_dedup::PHASH_THRESHOLD {
                "*"
            } else {
                " "
            };
            print!("{d:>11}{mark}");
        }
        println!();
    }
    println!(
        "\n`*` = 汉明 ≤ {}（会被判成近似重复）",
        engine_dedup::PHASH_THRESHOLD
    );
}
