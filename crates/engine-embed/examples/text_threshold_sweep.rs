//! D3 标定 v4（临时 example，卡 t_e8c656a4）：修正标注后扫阈值。
//!
//! 背景：WIN-E2E 的 24 图语料 manifest 标注大量与画面不符（k01 标「马」实为狼、k06 标
//! 「滑板」实为虾、k12/k13 标「打印机/复印机」实为超声仪、k19 标「冰棍」实为扳手图标、
//! k21 标「沙滩」实为珊瑚、k22 标「樱桃」实为婚戒）。本 probe 用逐图人工核对后的
//! 真值表（见 TRUTH 常量）重测，并扫描 minScore 阈值：
//!   - 命中保持率（期望图是否仍超过阈值）
//!   - top1 正确率（24 图库）
//!   - 无关词漏出条数
//!
//! 用法：cargo run --release -p engine-embed --example text_threshold_sweep -- /models/siglip2-base-p224-onnx /corpus

use std::path::PathBuf;

use engine_embed::onnx_siglip::OnnxSiglipProvider;
use engine_embed::EmbedProvider;

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

/// (文件名前缀, 中文查询, 英文查询) —— 逐图视觉核对后的真值（2026-10-17）
const TRUTH: &[(&str, &str, &str)] = &[
    ("k00-husky-dog", "狗", "dog"),
    ("k01-running-horse", "狼", "wolf"),
    ("k02-french-fries", "薯条", "french fries"),
    ("k03-mountain-landscape", "海岸礁石", "rocky coast"),
    ("k04-tropical-fish", "鱼", "tropical fish"),
    ("k05-gorilla", "猴子", "monkey"),
    ("k06-skateboards", "虾", "shrimp"),
    ("k07-wine-glasses", "红酒杯", "wine glass"),
    ("k08-earth-space", "地球", "earth from space"),
    ("k09-boxing", "夜景派对", "rooftop party at night"),
    ("k10-dental-chair", "牙科椅", "dental chair"),
    ("k11-skincare", "护士", "nurse"),
    ("k12-printer", "超声波仪器", "ultrasound machine"),
    ("k14-dahlia-flower", "花", "flower"),
    ("k15-bricks", "砖墙", "brick wall"),
    ("k16-denim-bag", "蓝色包", "blue handbag"),
    ("k17-dancing-person", "卡通人物", "3d cartoon man"),
    ("k18-pastries", "糕点", "pastry"),
    ("k19-popsicles", "扳手", "wrench"),
    ("k20-leather-bag", "棕色皮包", "brown leather bag"),
    ("k21-sand", "珊瑚", "coral"),
    ("k22-cherries", "婚戒", "wedding rings"),
    ("k23-dancing-couple", "跳舞", "dancing couple"),
];

/// 库内无对应物的无关词（语义近邻也排除）
const NEGS: &[&str] = &[
    "quantum computer chip",
    "银行存款利率调整",
    "abstract business strategy",
    "香蕉",
    "微积分公式",
    "a person playing violin",
    "火山",
    "skyscraper",
    "长颈鹿",
    "snowmobile",
];

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

    let mj: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(corpus.join("manifest.json")).unwrap()).unwrap();
    let files: Vec<String> = mj["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["file"].as_str().unwrap().to_string())
        .collect();
    let imgs: Vec<Vec<f32>> = files
        .iter()
        .map(|f| {
            prov.embed_image_bytes(&std::fs::read(corpus.join(f)).unwrap())
                .unwrap()
        })
        .collect();
    println!("语料 {} 张，真值 {} 条", files.len(), TRUTH.len());

    // 查询 → 期望资产索引
    let mut queries: Vec<(String, Option<usize>)> = Vec::new();
    for (prefix, zh, en) in TRUTH {
        let idx = files
            .iter()
            .position(|f| f.starts_with(prefix))
            .unwrap_or_else(|| panic!("语料缺 {prefix}"));
        queries.push((zh.to_string(), Some(idx)));
        queries.push((en.to_string(), Some(idx)));
    }
    for n in NEGS {
        queries.push((n.to_string(), None));
    }

    let tvecs: Vec<Vec<f32>> = prov
        .embed_texts(&queries.iter().map(|q| q.0.as_str()).collect::<Vec<_>>())
        .unwrap();

    // 全量打分矩阵
    let matrix: Vec<Vec<f32>> = tvecs
        .iter()
        .map(|t| imgs.iter().map(|iv| cos(t, iv)).collect())
        .collect();

    println!("\n== 逐查询 top1 与期望图分数（raw cos，无阈值）==");
    for (qi, (q, want)) in queries.iter().enumerate() {
        let mut order: Vec<usize> = (0..imgs.len()).collect();
        order.sort_by(|a, b| matrix[qi][*b].partial_cmp(&matrix[qi][*a]).unwrap());
        let top1 = order[0];
        match want {
            Some(w) => println!(
                "  {:<26} top1={:<28} {:.4}  期望={:<28} {:.4} {}",
                q,
                files[top1],
                matrix[qi][top1],
                files[*w],
                matrix[qi][*w],
                if top1 == *w { "OK" } else { "MISS" }
            ),
            None => println!(
                "  {:<26} top1={:<28} {:.4}  （无关词）",
                q, files[top1], matrix[qi][top1]
            ),
        }
    }

    println!("\n== 阈值扫描（minScore 语义：cos ≥ 阈值的命中才返回）==");
    for thr in [0.06f32, 0.07, 0.08, 0.085, 0.09, 0.095, 0.10, 0.11, 0.12] {
        let mut kept = 0;
        let mut total = 0;
        let mut leak_q = 0;
        let mut neg_total = 0;
        let mut top1_ok = 0;
        let mut top1_total = 0;
        let mut min_expected = f32::INFINITY;
        for (qi, (_, want)) in queries.iter().enumerate() {
            let hits: Vec<usize> = (0..imgs.len()).filter(|i| matrix[qi][*i] >= thr).collect();
            match want {
                Some(w) => {
                    total += 1;
                    top1_total += 1;
                    min_expected = min_expected.min(matrix[qi][*w]);
                    if hits.contains(w) {
                        kept += 1;
                    }
                    let mut order: Vec<usize> = (0..imgs.len()).collect();
                    order.sort_by(|a, b| matrix[qi][*b].partial_cmp(&matrix[qi][*a]).unwrap());
                    if order[0] == *w || hits.is_empty() {
                        // top1 判定：命中集合首位 == 期望
                        if !hits.is_empty() && hits[0] == *w {
                            top1_ok += 1;
                        }
                    }
                }
                None => {
                    neg_total += 1;
                    if !hits.is_empty() {
                        leak_q += 1;
                    }
                }
            }
        }
        println!(
            "  thr={thr:.3}  命中保持 {kept}/{total}  top1 正确 {top1_ok}/{top1_total}  无关词有返回 {leak_q}/{neg_total}  期望图最低分 {min_expected:.4}"
        );
    }

    // ---------- z 归一 + 绝对下限双门 ----------
    // 逐查询把 24 个分数标准化：z_i = (cos_i - mean_lib) / std_lib。
    // 语义：真正匹配的图应当显著高于本查询在全库的基线（z 大）；无关词全库分数
    // 彼此接近（z 小）。门 = cos ≥ abs_floor 且 z ≥ z_min。
    println!("\n== z 双门扫描（cos ≥ abs_floor 且 z ≥ z_min）==");
    let stats: Vec<(f32, f32)> = matrix
        .iter()
        .map(|row| {
            let m = row.iter().sum::<f32>() / row.len() as f32;
            let sd = (row.iter().map(|x| (x - m) * (x - m)).sum::<f32>() / row.len() as f32)
                .sqrt()
                .max(1e-6);
            (m, sd)
        })
        .collect();
    for abs_floor in [0.05f32, 0.06, 0.065, 0.07] {
        for z_min in [1.5f32, 1.75, 2.0, 2.25, 2.5] {
            let mut kept = 0;
            let mut total = 0;
            let mut leak_q = 0;
            let mut neg_total = 0;
            let mut zh_kept = 0;
            let mut zh_total = 0;
            let mut top1_ok = 0;
            let mut top1_total = 0;
            for (qi, (_, want)) in queries.iter().enumerate() {
                let (m, sd) = stats[qi];
                let z = |i: usize| (matrix[qi][i] - m) / sd;
                let hits: Vec<usize> = (0..imgs.len())
                    .filter(|i| matrix[qi][*i] >= abs_floor && z(*i) >= z_min)
                    .collect();
                match want {
                    Some(w) => {
                        total += 1;
                        top1_total += 1;
                        if q_is_zh(qi) {
                            zh_total += 1;
                        }
                        if hits.contains(w) {
                            kept += 1;
                            if q_is_zh(qi) {
                                zh_kept += 1;
                            }
                        }
                        let mut order: Vec<usize> = hits.clone();
                        order.sort_by(|a, b| matrix[qi][*b].partial_cmp(&matrix[qi][*a]).unwrap());
                        if order.first() == Some(w) {
                            top1_ok += 1;
                        }
                    }
                    None => {
                        neg_total += 1;
                        if !hits.is_empty() {
                            leak_q += 1;
                        }
                    }
                }
            }
            println!(
                "  abs={abs_floor:.3} z≥{z_min:.2}  命中保持 {kept}/{total}（中文 {zh_kept}/{zh_total}）  top1 {top1_ok}/{top1_total}  无关词有返回 {leak_q}/{neg_total}"
            );
        }
    }
}

/// 查询序号奇偶 = 真值表里「先中文后英文」的排列（0=中文）。
fn q_is_zh(qi: usize) -> bool {
    qi % 2 == 0 && qi < TRUTH.len() * 2
}
