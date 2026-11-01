//! R4 修复闭环探针（语料评估用）：对每张图跑 u2net 占比 → 若 <min 走与 smart_crop
//! 插件同口径的裁剪 → 复测占比，统计「直接过 / 修复后过 / 修不动」三档。
//!
//! 用法（容器内）：
//!   DAM_MODELS_ROOT=<root> cargo run --release -p engine-segment \
//!     --features local-infer --example crop_close -- <img> [...]

#[cfg(feature = "local-infer")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let seg = engine_segment::segmenter("u2net").expect("segmenter");
    let side = engine_segment::SIDE as i32;
    let (mut direct, mut fixed, mut human, mut err) = (0usize, 0usize, 0usize, 0usize);
    for (i, p) in args.iter().enumerate() {
        let path = std::path::Path::new(p);
        // 唯一临时路径：libvips 对同路径「改写后重读」会命中操作缓存/mmap 旧内容
        let tmp = std::env::temp_dir().join(format!("crop_close_chk_{i}.jpg"));
        let (w, h, px) = match core_media::rgb8(path) {
            Ok(v) => v,
            Err(_) => {
                err += 1;
                continue;
            }
        };
        let s = match seg.run_rgb(w as u32, h as u32, &px) {
            Ok(v) => v,
            Err(_) => {
                err += 1;
                continue;
            }
        };
        let r = s.fg_ratio;
        if (0.65..=0.85).contains(&r) {
            direct += 1;
            println!("direct  r={r:.3} {p}");
            continue;
        }
        if r > 0.85 {
            human += 1;
            println!("human >max r={r:.3} {p}");
            continue;
        }
        let Some((win, _)) = s.crop_window(0.75) else {
            human += 1;
            println!("human no-window r={r:.3} {p}");
            continue;
        };
        let x = ((win[0] as i32) * w / side).clamp(0, w - 1);
        let y = ((win[1] as i32) * h / side).clamp(0, h - 1);
        let x2 = ((((win[2] as i32) + 1) * w + side - 1) / side).clamp(x + 1, w);
        let y2 = ((((win[3] as i32) + 1) * h + side - 1) / side).clamp(y + 1, h);
        let r2 = match core_media::crop_to(path, &tmp, x, y, x2 - x, y2 - y) {
            Ok((cw, ch)) => match core_media::rgb8(&tmp) {
                Ok((_, _, cpx)) => seg
                    .run_rgb(cw as u32, ch as u32, &cpx)
                    .map(|s| s.fg_ratio)
                    .unwrap_or_else(|e| {
                        println!("  [seg2 err] {e}");
                        0.0
                    }),
                Err(e) => {
                    println!("  [decode2 err] {e}");
                    0.0
                }
            },
            Err(e) => {
                println!("  [crop err] x={x} y={y} w={} h={} {e}", x2 - x, y2 - y);
                0.0
            }
        };
        let _ = std::fs::remove_file(&tmp);
        if r2 >= 0.65 {
            fixed += 1;
            println!("fixed   r={r:.3} -> {r2:.3} {p}");
        } else {
            human += 1;
            println!("human   r={r:.3} -> {r2:.3} {p}");
        }
    }
    println!("\n汇总: n={} direct={direct} fixed={fixed} human={human} err={err}", args.len());
}

#[cfg(not(feature = "local-infer"))]
fn main() {
    eprintln!("需要 --features local-infer");
    std::process::exit(2);
}
