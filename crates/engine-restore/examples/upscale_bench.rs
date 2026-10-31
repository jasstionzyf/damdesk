//! realesr-general-x4v3 CPU 吞吐抽查（C5 语料构成决策输入：R1 fail → upscale 的单张成本）。
//!
//! 用法（容器内）：
//!   DAM_MODELS_ROOT=<root> cargo run --release -p engine-restore \
//!     --features local-infer --example upscale_bench -- <img> [<img>...]
//!
//! `<root>/models/realesr-general-x4v3/realesr-general-x4v3.onnx` 需在盘。
//! 输出逐张耗时（解码/推理/编码分列）+ 汇总中位数。

#[cfg(feature = "local-infer")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("用法: upscale_bench <img> [...]");
        std::process::exit(2);
    }
    let up = engine_restore::upscale::upscaler(engine_restore::REALESR_GENERAL_X4V3)
        .expect("load upscaler");
    let mut ms_all = Vec::new();
    for p in &args {
        let t0 = std::time::Instant::now();
        let (w, h, px) = core_media::rgb8(std::path::Path::new(p)).expect("decode");
        let (w, h): (u32, u32) = (w.try_into().expect("w"), h.try_into().expect("h"));
        let dec = t0.elapsed().as_millis();
        let t1 = std::time::Instant::now();
        let (ow, oh, _out) = up.run_rgb(w, h, &px).expect("run_rgb");
        let inf = t1.elapsed().as_millis();
        println!(
            "{p}: {w}x{h} -> {ow}x{oh} | decode {dec}ms | infer {inf}ms | out {:.1}MP",
            (ow as f64 * oh as f64) / 1e6
        );
        ms_all.push(inf as u64);
    }
    ms_all.sort_unstable();
    let n = ms_all.len();
    println!("BENCH_SUMMARY n={n} infer_ms_median={}", ms_all[n / 2]);
}

#[cfg(not(feature = "local-infer"))]
fn main() {
    eprintln!("需要 --features local-infer");
    std::process::exit(2);
}
