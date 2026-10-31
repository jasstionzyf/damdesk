//! D3 诊断（临时 example，卡 task）：文本塔喂法变体对跨模态对齐的影响。
//!
//! 事实基线（本 probe 实测）：
//! - vision.onnx / text.onnx 只导出 last_hidden_state + pooler_output（无 image_embeds/text_embeds，
//!   SigLIP 本身无 projection 层），pooler_output 即共享空间向量；
//! - text.onnx 无 attention_mask 输入、pooler = Gather(final_norm, -1)，所以「左填充定长 64」
//!   让 <eos> 落在 -1 位能用，但 62 个 <pad> 参与自注意力 → 文本向量被常数分量稀释
//!   （文本两两 cos≈0.85，跨模态匹配词 cos 仅 0.10，无关词 0.08，区分度≈0）。
//!
//! 本 probe 对比「左填充长度 L」：L 越小 pad 越少（污染越轻），但内容位置也随之变化。
//!   L=64（生产）/48/32/24/16/8 + last_hidden_state 末位池化 + 中英双语平均。
//!
//! 用法：cargo run --release -p engine-embed --example text_tower_variants -- /models/siglip2-base-p224-onnx /corpus

use std::path::PathBuf;

use engine_embed::onnx_siglip::OnnxSiglipProvider;
use engine_embed::EmbedProvider;
use ort::session::Session;
use ort::value::Tensor;

fn cos(a: &[f32], b: &[f32]) -> f32 {
    let d: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        d / (na * nb)
    }
}

fn l2(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

/// 跑文本塔：out = pooler_output（末位池化）或 last_hidden_state（自取末位）。
fn run(sess: &mut Session, out: &str, ids: &[i64]) -> Vec<f32> {
    let t = Tensor::from_array((vec![1usize, ids.len()], ids.to_vec().into_boxed_slice())).unwrap();
    let outputs = sess.run(ort::inputs!["input_ids" => t]).unwrap();
    let arr = outputs.get(out).unwrap().try_extract_array::<f32>().unwrap();
    let shape = arr.shape().to_vec();
    let flat: Vec<f32> = arr.iter().copied().collect();
    let v = if shape.len() <= 2 {
        flat
    } else {
        let dims = *shape.last().unwrap();
        let l = shape[shape.len() - 2];
        flat[(l - 1) * dims..l * dims].to_vec()
    };
    l2(v)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let models = PathBuf::from(args.next().unwrap_or("/models/siglip2-base-p224-onnx".into()));
    let corpus = PathBuf::from(args.next().unwrap_or("/corpus".into()));

    let spec = core_config::load_registry(None)
        .unwrap()
        .by_name(core_config::DEFAULT_EMBEDDER)
        .unwrap()
        .clone();
    let mut prov = OnnxSiglipProvider::new(spec, &models, None);
    prov.load().unwrap();

    let manifest = std::fs::read_to_string(corpus.join("manifest.json")).unwrap();
    let mj: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    let items: Vec<(String, String, String)> = mj["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            (
                v["file"].as_str().unwrap().to_string(),
                v["zh"].as_str().unwrap().to_string(),
                v["en"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let imgs: Vec<Vec<f32>> = items
        .iter()
        .map(|i| {
            prov.embed_image_bytes(&std::fs::read(corpus.join(&i.0)).unwrap())
                .unwrap()
        })
        .collect();

    let mut sess = Session::builder()
        .and_then(|b| b.commit_from_file(models.join("text.onnx")))
        .unwrap();
    let out_names: Vec<String> = sess.outputs.iter().map(|o| o.name.clone()).collect();
    println!("text.onnx outputs={out_names:?}");
    let tk = tokenizers::Tokenizer::from_file(models.join("tokenizer.json")).unwrap();

    // 查询集：(文本, 期望资产, 类别)
    let mut queries: Vec<(String, Option<usize>, &str)> = Vec::new();
    for (i, it) in items.iter().enumerate() {
        queries.push((
            it.1.split_whitespace().last().unwrap_or(&it.1).to_string(),
            Some(i),
            "zh",
        ));
        queries.push((it.2.clone(), Some(i), "en"));
    }
    for c in ["滑板", "婚戒", "复印机", "砖墙", "皮包", "冰棍"] {
        let idx = items.iter().position(|i| {
            i.1.contains(c)
                || (c == "婚戒" && i.0.contains("wine"))
                || (c == "砖墙" && i.0.contains("bricks"))
                || (c == "皮包" && i.0.contains("leather"))
                || (c == "冰棍" && i.0.contains("popsicles"))
                || (c == "复印机" && i.0.contains("copier"))
        });
        queries.push((c.to_string(), idx, "concept"));
    }
    for n in [
        "quantum computer chip",
        "银行存款利率调整",
        "abstract business strategy",
        "香蕉",
        "雪山",
        "微积分公式",
        "a person playing violin",
        "火山",
    ] {
        queries.push((n.to_string(), None, "neg"));
    }

    // (名, 序列长度 L, 输出名, 是否双语平均)
    let variants: Vec<(&str, usize, &str, bool)> = vec![
        ("prod_L64_pool", 64, "pooler_output", false),
        ("L48_pool", 48, "pooler_output", false),
        ("L32_pool", 32, "pooler_output", false),
        ("L24_pool", 24, "pooler_output", false),
        ("L16_pool", 16, "pooler_output", false),
        ("L8_pool", 8, "pooler_output", false),
        ("L64_lhs", 64, "last_hidden_state", false),
        ("L16_lhs", 16, "last_hidden_state", false),
        ("L64_bilingual", 64, "pooler_output", true),
        ("L16_bilingual", 16, "pooler_output", true),
    ];

    for (name, len, out, bilingual) in &variants {
        let mut top1 = 0;
        let mut n_match = 0;
        let mut min_matched = f32::INFINITY;
        let mut max_neg = f32::NEG_INFINITY;
        let mut misses: Vec<String> = Vec::new();
        let mut vecs: Vec<Vec<f32>> = Vec::new();
        for (q, want, kind) in &queries {
            let ids = engine_embed::onnx_siglip::tokenize(&tk, q, *len).unwrap();
            let mut v = run(&mut sess, out, &ids);
            if *bilingual {
                let hint = want
                    .map(|w| items[w].2.clone())
                    .unwrap_or_else(|| q.clone());
                let ids2 = engine_embed::onnx_siglip::tokenize(&tk, &hint, *len).unwrap();
                let v2 = run(&mut sess, out, &ids2);
                let m: Vec<f32> = v.iter().zip(&v2).map(|(a, b)| a + b).collect();
                v = l2(m);
            }
            let scores: Vec<f32> = imgs.iter().map(|iv| cos(&v, iv)).collect();
            let best = scores
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                .unwrap();
            match want {
                Some(w) => {
                    n_match += 1;
                    if best.0 == *w {
                        top1 += 1;
                        min_matched = min_matched.min(*best.1);
                    } else {
                        misses.push(format!("{q}({kind}) {:.4}→{}", best.1, items[best.0].0));
                    }
                }
                None => max_neg = max_neg.max(*best.1),
            }
            vecs.push(v);
        }
        let mut s = 0.0;
        let mut c = 0;
        for i in 0..vecs.len() {
            for j in (i + 1)..vecs.len() {
                s += cos(&vecs[i], &vecs[j]);
                c += 1;
            }
        }
        println!("\n== {name} ==");
        println!(
            "top1 {top1}/{n_match}  匹配词 top1 最低 {min_matched:.4}  无关词最高 {max_neg:.4}  区分度 {:.4}  文本对平均cos {:.4}",
            min_matched - max_neg,
            s / c as f32
        );
        for m in misses.iter().take(12) {
            println!("  MISS {m}");
        }
    }
}
