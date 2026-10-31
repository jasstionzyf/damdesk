//! 调试：夹具行分布 + det 概率图分量统计（OCR 验收排障用）。
//! 用法：DAM_MODELS_ROOT=/ cargo run --release -p engine-ocr --example ocr_debug -- /tmp/ocr-corpus/cn.png

use core_media::rgb8;

fn main() {
    let path = std::env::args().nth(1).expect("image path");
    let (w, h, px) = rgb8(std::path::Path::new(&path)).expect("decode");
    println!("image {w}x{h}");
    // 暗像素行分布（20 行一桶）
    let mut bands = vec![0usize; (h as usize).div_ceil(20)];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let i = (y * w as usize + x) * 3;
            if px[i] < 128 {
                bands[y / 20] += 1;
            }
        }
    }
    for (b, n) in bands.iter().enumerate() {
        if *n > 0 {
            println!("rows {:3}-{:3}: dark={}", b * 20, b * 20 + 19, n);
        }
    }
    // det 概率图
    let engine = engine_ocr::infer::engine().expect("engine");
    let (map, mw, mh) = engine.det.run(w as u32, h as u32, &px).expect("det");
    println!("det map {mw}x{mh}");
    let mut probs: Vec<f32> = map.clone();
    probs.sort_by(|a, b| b.partial_cmp(a).unwrap());
    println!(
        "p max={:.3} p99={:.3} p90={:.3} p50={:.3}",
        probs[0],
        probs[probs.len() / 100],
        probs[probs.len() / 10],
        probs[probs.len() / 2]
    );
    for t in [0.1f32, 0.2, 0.3, 0.45, 0.5] {
        let n = map.iter().filter(|&&p| p > t).count();
        println!(">{t:.2}: {n} px");
    }
    // 行分布（20 行一桶，map 坐标）
    let mut mbands = vec![0usize; (mh as usize).div_ceil(20)];
    for (i, &p) in map.iter().enumerate() {
        if p > 0.2 {
            mbands[i / mw as usize / 20] += 1;
        }
    }
    for (b, n) in mbands.iter().enumerate() {
        if *n > 0 {
            println!("map rows {:3}-{:3}: >0.2={}", b * 20, b * 20 + 19, n);
        }
    }
    let boxes = engine_ocr::db_boxes(
        &map,
        mw,
        mh,
        w as u32,
        h as u32,
        engine_ocr::DB_MAX_CANDIDATES,
    )
    .unwrap();
    println!("boxes: {boxes:?}");
}
