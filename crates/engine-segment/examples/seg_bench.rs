//! CPU 吞吐实测（C3 验收：目标 ≤0.5s/张）：解码（core-media，与生产同像素入口）
//! → 预处理 → u2net 推理 → 掩码/占比统计，逐张计时。
//!
//! 用法（release，容器内）：
//!   SEG_BENCH_MODEL=/models/u2net/u2net.onnx SEG_BENCH_DIR=/tmp/c3-corpus \
//!   cargo run --release -p engine-segment --example seg_bench
//!
//! 输出末行：`SEG_BENCH_RESULT {"n":N,"avg_ms":..,"p50_ms":..,"max_ms":..,"total_s":..}`

use std::time::Instant;

fn main() {
    let model = std::env::var("SEG_BENCH_MODEL").unwrap_or_else(|_| "/models/u2net/u2net.onnx".into());
    let dir = std::env::var("SEG_BENCH_DIR").unwrap_or_else(|_| "/tmp/c3-corpus".into());
    let limit: usize = std::env::var("SEG_BENCH_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);

    let t0 = Instant::now();
    let seg = engine_segment::Segmenter::load(std::path::Path::new(&model))
        .expect("load u2net onnx");
    println!("model loaded in {:.1?} from {model}", t0.elapsed());

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
    for (i, p) in paths.iter().enumerate() {
        let t = Instant::now();
        let (w, h, px) = core_media::rgb8(p).expect("decode");
        let seg_ = seg.run_rgb(w as u32, h as u32, &px).expect("infer");
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        times.push(ms);
        println!(
            "[{}/{}] {} {}x{} fg_ratio={:.3} bbox={:?} {:.0}ms",
            i + 1,
            paths.len(),
            p.file_name().unwrap().to_string_lossy(),
            w,
            h,
            seg_.fg_ratio,
            seg_.bbox,
            ms
        );
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg = times.iter().sum::<f64>() / times.len() as f64;
    let p50 = times[times.len() / 2];
    let max = times[times.len() - 1];
    println!(
        "SEG_BENCH_RESULT {{\"n\":{},\"avg_ms\":{avg:.1},\"p50_ms\":{p50:.1},\"max_ms\":{max:.1},\"total_s\":{:.1}}}",
        times.len(),
        t0.elapsed().as_secs_f64()
    );
}
