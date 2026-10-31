//! C3b：Rust ort 端一致性抽查 — 对指定图片逐张输出 u2net 320² 掩码 PGM + fg_ratio。
//!
//! 与 seg_bench 同一生产像素入口（core-media 解码 → engine-segment 预处理/推理/后处理），
//! 供 python 侧 c3b_compare_rust.py 对齐 FP32@320 一致性（ORT 版本/预处理差异量化）。
//!
//! 用法（release，容器内）：
//!   SEG_DUMP_LIST=/tmp/c3b-out/compare8.txt SEG_DUMP_OUT=/tmp/c3b-out/rust \
//!   cargo run --release -p engine-segment --example c3b_dump
//!
//! 末行：`C3B_DUMP_RESULT {"n":N,"total_s":..}`

fn main() {
    let list = std::env::var("SEG_DUMP_LIST").unwrap_or_else(|_| "/tmp/c3b-out/compare8.txt".into());
    let out_dir = std::env::var("SEG_DUMP_OUT").unwrap_or_else(|_| "/tmp/c3b-out/rust".into());
    let model = std::env::var("SEG_DUMP_MODEL").unwrap_or_else(|_| "/models/u2net/u2net.onnx".into());
    std::fs::create_dir_all(&out_dir).expect("create out dir");

    let t0 = std::time::Instant::now();
    let seg = engine_segment::Segmenter::load(std::path::Path::new(&model)).expect("load u2net onnx");

    let paths: Vec<String> = std::fs::read_to_string(&list)
        .expect("read list")
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    assert!(!paths.is_empty(), "empty list: {list}");

    let mut recs = Vec::new();
    for p in &paths {
        let (w, h, px) = core_media::rgb8(std::path::Path::new(p)).expect("decode");
        let seg_ = seg.run_rgb(w as u32, h as u32, &px).expect("infer");
        let name = std::path::Path::new(p)
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let m = &seg_.mask;
        assert_eq!(m.side, 320);
        let mut buf = format!("P5\n320 320\n255\n").into_bytes();
        buf.extend(m.data.iter().map(|&v| if v == 1 { 255u8 } else { 0u8 }));
        std::fs::write(format!("{out_dir}/{name}.pgm"), &buf).expect("write pgm");
        recs.push(format!(
            "{{\"img\":\"{name}\",\"fg_ratio\":{:.6}}}",
            seg_.fg_ratio
        ));
        println!("{name} {w}x{h} fg_ratio={:.6}", seg_.fg_ratio);
    }
    std::fs::write(
        format!("{out_dir}/ratios.json"),
        format!("[{}]", recs.join(",")),
    )
    .expect("write ratios");
    println!(
        "C3B_DUMP_RESULT {{\"n\":{},\"total_s\":{:.2}}}",
        recs.len(),
        t0.elapsed().as_secs_f64()
    );
}
