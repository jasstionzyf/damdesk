//! W3 以图搜图模型对比 probe（L3 dev-e2e，仅开发验收用，不进安装包）。
//!
//! 走**生产同路径**（engine-embed 的 OnnxSiglipProvider / OnnxDinoV3Provider：同一套
//! ImagePreprocess + 出口 L2 归一），对一批真图输出
//! - 全量图×图 cos 矩阵（JSON，供 Python 定阈值）
//! - 单图编码耗时（逐图 ms；含首次加载的冷启动单列）
//!
//! 用法（dam-dev 容器内）：
//! ```sh
//! cargo run --release -p dev-e2e --bin image_sim_probe -- \
//!   --models-root /models --images /data \
//!   --model siglip2-base-p224-onnx --out /tmp/m.json
//! ```
use std::path::{Path, PathBuf};
use std::time::Instant;

use core_config::load_registry;
use engine_embed::onnx_dinov3::OnnxDinoV3Provider;
use engine_embed::onnx_siglip::OnnxSiglipProvider;
use engine_embed::EmbedProvider;

struct Args {
    models_root: PathBuf,
    images: PathBuf,
    model: String,
    out: PathBuf,
}

fn parse_args() -> Args {
    let mut a = Args {
        models_root: PathBuf::from("/models"),
        images: PathBuf::from("/workspace/tests/fixtures"),
        model: "siglip2-base-p224-onnx".into(),
        out: PathBuf::from("/tmp/image-sim.json"),
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let v = it.next().unwrap_or_default();
        match k.as_str() {
            "--models-root" => a.models_root = PathBuf::from(v),
            "--images" => a.images = PathBuf::from(v),
            "--model" => a.model = v,
            "--out" => a.out = PathBuf::from(v),
            other => eprintln!("未知参数 {other}"),
        }
    }
    a
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn images_in(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("images dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && matches!(
                    p.extension().and_then(|s| s.to_str()).map(|s| s.to_ascii_lowercase()),
                    Some(ref e) if e == "jpg" || e == "jpeg" || e == "png" || e == "webp"
                )
        })
        .collect();
    v.sort();
    v
}

fn main() {
    let args = parse_args();
    let spec = load_registry(None)
        .expect("registry")
        .by_name(&args.model)
        .unwrap_or_else(|| panic!("registry 无模型 {}", args.model))
        .clone();
    let dir = args.models_root.join(&args.model);

    // 两种 provider 的统一入口（dyn 里拿 embed_images）
    let provider: Box<dyn EmbedProvider> = if args.model.starts_with("dinov3") {
        Box::new(OnnxDinoV3Provider::new(spec.clone(), &dir, None))
    } else {
        Box::new(OnnxSiglipProvider::new(spec.clone(), &dir, None))
    };

    let files = images_in(&args.images);
    println!("model={} dims={} files={}", args.model, spec.dims, files.len());

    // 冷启动（首次调用含会话加载），单独计时
    let cold = Instant::now();
    let mut vecs: Vec<Vec<f32>> = Vec::with_capacity(files.len());
    let mut times: Vec<f64> = Vec::with_capacity(files.len());
    for (i, f) in files.iter().enumerate() {
        let t = Instant::now();
        let v = provider
            .embed_images(std::slice::from_ref(f))
            .unwrap_or_else(|e| panic!("embed {:?}: {e}", f));
        let dt = t.elapsed().as_secs_f64() * 1000.0;
        // 自检：向量自洽（L2 归一后范数≈1；未归一 spec 时跳过）
        if spec.normalized {
            let n: f32 = v[0].iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((n - 1.0).abs() < 1e-3, "非归一向量 {:?} L2={n}", f);
        }
        times.push(dt);
        vecs.push(v.into_iter().next().unwrap());
        println!("  {:>2} {:>30} {:>8.1}ms", i, f.file_name().unwrap().to_string_lossy(), dt);
    }
    let cold_ms = cold.elapsed().as_secs_f64() * 1000.0;
    let mut sorted = times.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pick = |q: f64| sorted[((sorted.len() as f64 - 1.0) * q).round() as usize];
    println!(
        "encoding: total {:.0}ms | p50 {:.1}ms | p95 {:.1}ms | max {:.1}ms | cold(first) {:.1}ms",
        times.iter().sum::<f64>(),
        pick(0.5),
        pick(0.95),
        sorted[sorted.len() - 1],
        times[0]
    );
    println!("cold_start_wall {:.1}ms (含 provider 首次建会话)", cold_ms);

    // 全量矩阵
    let mut js = String::from("{\n \"model\": \"");
    js.push_str(&args.model);
    js.push_str("\",\n \"dims\": ");
    js.push_str(&spec.dims.to_string());
    js.push_str(",\n \"enc_ms\": [");
    for (i, t) in times.iter().enumerate() {
        if i > 0 {
            js.push(',');
        }
        js.push_str(&format!("{t:.1}"));
    }
    js.push_str("],\n \"files\": [");
    for (i, f) in files.iter().enumerate() {
        if i > 0 {
            js.push(',');
        }
        js.push('"');
        js.push_str(&f.file_name().unwrap().to_string_lossy());
        js.push('"');
    }
    js.push_str("],\n \"matrix\": {\n");
    for (i, f) in files.iter().enumerate() {
        if i > 0 {
            js.push_str(",\n");
        }
        js.push('"');
        js.push_str(&f.file_name().unwrap().to_string_lossy());
        js.push_str("\": {");
        for (j, g) in files.iter().enumerate() {
            if j > 0 {
                js.push(',');
            }
            js.push('"');
            js.push_str(&g.file_name().unwrap().to_string_lossy());
            js.push_str(&format!("\": {:.6}", cosine(&vecs[i], &vecs[j])));
        }
        js.push('}');
    }
    js.push_str("\n }\n}\n");
    std::fs::write(&args.out, js).expect("write out");
    println!("WROTE {}", args.out.display());
}
