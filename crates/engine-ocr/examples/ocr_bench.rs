//! CPU 吞吐实测（OCR 验收：逐张 det+rec 计时 + 行文本预览）。
//!
//! 用法（release，容器内）：
//!   DAM_MODELS_ROOT=/models OCR_BENCH_DIR=/tmp/ocr-corpus \
//!   cargo run --release -p engine-ocr --example ocr_bench
//!
//! 输出末行：`OCR_BENCH_RESULT {"n":N,"avg_ms":..,"p50_ms":..,"max_ms":..,"lines":L,"chars":C,"total_s":..}`

use std::time::Instant;

fn main() {
    let dir = std::env::var("OCR_BENCH_DIR").unwrap_or_else(|_| "/tmp/ocr-corpus".into());
    let limit: usize = std::env::var("OCR_BENCH_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);

    let t0 = Instant::now();
    let engine = engine_ocr::infer::engine().expect("load OCR engine (det+rec+dict)");
    println!("engine loaded in {:.1?} (models root = {:?})", t0.elapsed(), engine_ocr::models_root());

    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read_dir {dir}: {e}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("jpg" | "jpeg" | "png" | "webp")
            )
        })
        .collect();
    paths.sort();
    if paths.len() > limit {
        paths.truncate(limit);
    }
    assert!(!paths.is_empty(), "no images in {dir}");

    let mut times = Vec::new();
    let mut total_lines = 0usize;
    let mut total_chars = 0usize;
    for (i, p) in paths.iter().enumerate() {
        let t = Instant::now();
        let (w, h, px) = core_media::rgb8(p).expect("decode");
        let lines = engine.run_rgb(w as u32, h as u32, &px).expect("infer");
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        times.push(ms);
        let chars: usize = lines.iter().map(|l| l.text.chars().count()).sum();
        total_lines += lines.len();
        total_chars += chars;
        println!(
            "[{}/{}] {} {}x{} lines={} chars={} {:.0}ms",
            i + 1,
            paths.len(),
            p.file_name().unwrap().to_string_lossy(),
            w,
            h,
            lines.len(),
            chars,
            ms
        );
        for l in lines.iter().take(5) {
            println!("    [{},{} {},{}] box={:.2} ctc={:.2} {}", l.x0, l.y0, l.x1, l.y1, l.box_score, l.text_score, l.text);
        }
    }
    times.sort_by(|a, b| a.partial_cmp(&b).unwrap());
    let avg = times.iter().sum::<f64>() / times.len() as f64;
    let p50 = times[times.len() / 2];
    let max = times[times.len() - 1];
    println!(
        "OCR_BENCH_RESULT {{\"n\":{},\"avg_ms\":{avg:.1},\"p50_ms\":{p50:.1},\"max_ms\":{max:.1},\"lines\":{total_lines},\"chars\":{total_chars},\"total_s\":{:.1}}}",
        times.len(),
        t0.elapsed().as_secs_f64()
    );
}
