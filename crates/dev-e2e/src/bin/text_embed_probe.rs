//! TEXT-EMBED-1 文本塔诊断/验收 probe（L3 dev-e2e，仅开发验收用，不进安装包）。
//!
//! 走**生产同路径**（OnnxSiglipProvider：engine-embed 预处理 + tokenizer + ONNX 双塔），
//! 对一批标注真图 + 中英文词表输出词×图 cos 矩阵 + top1 命中，并做硬断言：
//! - 匹配词 → 正确图片 top1（狗/一只狗/a dog→狗图；汽车/a car→车图；护士/nurse→护士图；地球→地球图）
//! - 狗图 × 「狗」 cos ≥ 0.08（基线：左填充修复后实测 0.10~0.14，见 reports/TEXT-EMBED-1.md）
//! - 无关词（香蕉/banana，库内无香蕉）× 任意图 ≤ 0.07（实测噪声带 ≤0.066）
//! - 图-图自检不回归：自检索 =1.0，狗图 vs 其他真图 cos ∈ [0.3, 0.7]（实测 0.47~0.59）
//!
//! 用法（dam-dev 容器内，/models 挂 /data）：
//! ```sh
//! cargo run --release -p dev-e2e --bin text_embed_probe -- \
//!   --models-root / --photos /workspace/tests/fixtures
//! ```

use std::path::PathBuf;

use core_config::{load_registry, DEFAULT_EMBEDDER};
use engine_embed::onnx_siglip::OnnxSiglipProvider;
use engine_embed::EmbedProvider;

/// 词 → 应命中的图（文件名前缀匹配；库内没有对应物的词不进这张表）。
const EXPECTED_TOP1: &[(&str, &str)] = &[
    ("狗", "dog.jpg"),
    ("一只狗", "dog.jpg"),
    ("a dog", "dog.jpg"),
    ("汽车", "car.jpg"),
    ("a car", "car.jpg"),
    ("护士", "dp-0003"),
    ("nurse", "dp-0003"),
    ("地球", "dp-0005"),
];

/// 库内没有对应物的无关词（应全图低分）。
const UNRELATED: &[&str] = &["香蕉", "banana", "雪山", "snow mountain", "食物"];

const MIN_MATCHED_COS: f32 = 0.08;
const MAX_UNRELATED_COS: f32 = 0.07;

struct Args {
    models_root: String,
    photos: PathBuf,
}

fn parse_args() -> Args {
    let mut models_root = std::env::var("TE_MODELS")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/".into());
    let mut photos = PathBuf::from("/workspace/tests/fixtures");
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--models-root" => models_root = it.next().unwrap_or(models_root),
            "--photos" => photos = PathBuf::from(it.next().unwrap_or_default()),
            other => eprintln!("未知参数 {other}"), // 容错：忽略
        }
    }
    Args {
        models_root,
        photos,
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

fn main() {
    let args = parse_args();
    let models_dir = PathBuf::from(&args.models_root).join("siglip2-base-p224-onnx");
    let spec = load_registry(None)
        .expect("registry")
        .by_name(DEFAULT_EMBEDDER)
        .expect("默认语义模型")
        .clone();
    let mut provider = OnnxSiglipProvider::new(spec.clone(), &models_dir, None);
    provider
        .load()
        .unwrap_or_else(|e| panic!("加载模型失败（{models_dir:?}）: {e}"));

    // 语料：dog/car 固定 fixtures + reals 里的 dp-0000..0005（已人工核对内容）
    // + reals 其余任取补足 ≥10 张。词断言只依赖已核对的 8 张。
    let mut photos: Vec<PathBuf> = vec![args.photos.join("dog.jpg"), args.photos.join("car.jpg")];
    let reals_dir = args.photos.join("reals");
    let mut reals: Vec<PathBuf> = std::fs::read_dir(&reals_dir)
        .expect("reals 目录")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("jpg") | Some("jpeg") | Some("png")
            )
        })
        .filter(|p| {
            let n = p.file_name().unwrap_or_default().to_string_lossy();
            n.starts_with("dp-0000")
                || n.starts_with("dp-0001")
                || n.starts_with("dp-0002")
                || n.starts_with("dp-0003")
                || n.starts_with("dp-0004")
                || n.starts_with("dp-0005")
        })
        .collect();
    reals.sort();
    photos.extend(reals);
    // 补足到 ≥10 张真图（不做词断言，仅扩库）
    let extra: Vec<PathBuf> = std::fs::read_dir(&reals_dir)
        .expect("reals 目录")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("jpg") | Some("jpeg") | Some("png")
            )
        })
        .filter(|p| {
            let n = p.file_name().unwrap_or_default().to_string_lossy();
            !photos.iter().any(|q| q.file_name() == p.file_name())
                && (n.starts_with("dp-") || n.starts_with("sj-"))
        })
        .take(10usize.saturating_sub(photos.len()))
        .collect();
    photos.extend(extra);
    assert!(
        photos.len() >= 10,
        "真图语料不足 10 张（got {}）",
        photos.len()
    );
    println!("语料 {} 张：", photos.len());
    for p in &photos {
        println!("  {}", p.file_name().unwrap_or_default().to_string_lossy());
    }

    // 词表：中文 ≥3 + 英文 ≥3
    let words = [
        "狗",
        "一只狗",
        "汽车",
        "雪山",
        "护士",
        "地球",
        "香蕉",
        "食物",
        "a dog",
        "a car",
        "snow mountain",
        "nurse",
        "banana",
    ];

    let img_vecs = provider.embed_images(&photos).expect("图像塔嵌入");
    let text_vecs = provider.embed_texts(&words).expect("文本塔嵌入");

    let names: Vec<String> = photos
        .iter()
        .map(|p| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        })
        .collect();

    // 维度/归一自检
    let dims = img_vecs[0].len();
    println!("\n== 出口自检：dim={}，向量范数（应≈1）", dims);
    println!(
        "  img norms: {}",
        img_vecs[..3]
            .iter()
            .map(|v| format!("{:.4}", v.iter().map(|x| x * x).sum::<f32>().sqrt()))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "  text norms: {}",
        text_vecs[..3]
            .iter()
            .map(|v| format!("{:.4}", v.iter().map(|x| x * x).sum::<f32>().sqrt()))
            .collect::<Vec<_>>()
            .join(", ")
    );

    // 词×图 cos 矩阵（原始输出）
    println!("\n== 词×图 cos 矩阵（原始输出，逐格 dot）");
    print!("{:<16}", "word\\img");
    for n in &names {
        print!("{:>18}", &n[..n.len().min(17)]);
    }
    println!();
    let mut fails: Vec<String> = Vec::new();
    for (wi, w) in words.iter().enumerate() {
        print!("{:<16}", w);
        for iv in &img_vecs {
            print!("{:18.4}", cosine(&text_vecs[wi], iv));
        }
        // top1
        let (bi, bs) = img_vecs
            .iter()
            .enumerate()
            .map(|(i, v)| (i, cosine(&text_vecs[wi], v)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .unwrap();
        print!("   top1={} {:.4}", names[bi], bs);
        if let Some((_, want)) = EXPECTED_TOP1.iter().find(|(q, _)| q == w) {
            if names[bi].starts_with(want) {
                println!("  [PASS top1]");
            } else {
                println!("  [FAIL top1 期望 {}]", want);
                fails.push(format!("「{w}」top1 应为 {want}，实得 {}", names[bi]));
            }
        } else {
            println!();
        }
    }

    // 匹配词下限断言（期望表里全部词）
    println!("\n== 匹配词 cos ≥ {MIN_MATCHED_COS} 断言");
    for (w, want) in EXPECTED_TOP1 {
        let wi = words.iter().position(|q| q == w).unwrap();
        let ii = names.iter().position(|n| n.starts_with(want)).unwrap();
        let c = cosine(&text_vecs[wi], &img_vecs[ii]);
        let ok = c >= MIN_MATCHED_COS;
        println!(
            "  「{w}」×{} = {:.4}  [{}]",
            names[ii],
            c,
            if ok { "PASS" } else { "FAIL" }
        );
        if !ok {
            fails.push(format!(
                "「{w}」×{} cos {c:.4} < {MIN_MATCHED_COS}",
                names[ii]
            ));
        }
    }

    // 无关词上限断言（库内无对应物的词，全图 ≤ 0.07）
    println!("\n== 无关词全图 ≤ {MAX_UNRELATED_COS} 断言");
    for w in UNRELATED {
        let wi = words.iter().position(|q| q == w).unwrap();
        let mx = img_vecs
            .iter()
            .map(|v| cosine(&text_vecs[wi], v))
            .fold(f32::MIN, f32::max);
        let ok = mx <= MAX_UNRELATED_COS;
        println!(
            "  「{w}」max={mx:.4}  [{}]",
            if ok { "PASS" } else { "FAIL" }
        );
        if !ok {
            fails.push(format!(
                "无关词「{w}」max cos {mx:.4} > {MAX_UNRELATED_COS}"
            ));
        }
    }

    // 图-图自检（无回归）：自检索=1.0；狗图 vs 其他 ∈ [0.3,0.7]
    println!("\n== 图-图自检（无回归断言）");
    for (i, n) in names.iter().enumerate() {
        let c = cosine(&img_vecs[0], &img_vecs[i]);
        let ok = if i == 0 {
            c >= 0.999
        } else {
            (0.3..=0.7).contains(&c)
        };
        println!(
            "  dog.jpg × {n:<28} {c:.4}  [{}]",
            if ok { "PASS" } else { "FAIL" }
        );
        if !ok {
            fails.push(format!("图-图自检 dog.jpg×{n} = {c:.4} 越界"));
        }
    }

    if fails.is_empty() {
        println!("\nTEXT_EMBED_PROBE: ALL PASS");
    } else {
        println!("\nTEXT_EMBED_PROBE: FAILED（{} 项）", fails.len());
        for f in &fails {
            println!("  - {f}");
        }
        std::process::exit(1);
    }
}
