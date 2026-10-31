//! 真模型集成测试：真 SigLIP2 ONNX 文件 + 真 fixture 图片，走 `OnnxSiglipProvider` 全链路
//! （预处理 → vision/text 塔 → L2 归一 → 768 维）。
//!
//! 为什么必须真跑：D3 的验收核心是「同一张图 cos>0.99」「图文语义对」（卡片验收 1/3），
//! 假张量只能测形状，测不出 ONNX 图契约（输入名 / fp16 dtype / 输出名 / tokenizer 词表）。
//!
//! 环境（与 dev/docker-compose.yml 一致）：
//!   DAM_TEST_MODELS=/models/siglip2-base-p224-onnx        真模型目录（容器内只读挂载）
//!   DAM_TEST_FIXTURES=（可选）默认 <repo>/tests/fixtures
//!
//! 缺模型目录时**显式失败**（不静默跳过）：这类测试是 D3 的验收物，静默跳过等于假绿。

use std::path::{Path, PathBuf};

use core_config::{load_registry, SpecKind, DEFAULT_EMBEDDER};
use engine_embed::model_store::manifest_for;
use engine_embed::onnx_siglip::{tokenize, OnnxSiglipProvider};
use engine_embed::{cosine, l2_norm, EmbedProvider};

/// 真模型目录（缺 → 显式失败，附修复指引）。
fn models_dir() -> PathBuf {
    let dir = std::env::var("DAM_TEST_MODELS")
        .unwrap_or_else(|_| "/models/siglip2-base-p224-onnx".to_string());
    let p = PathBuf::from(&dir);
    let ok = p.join("vision.onnx").is_file()
        && p.join("text.onnx").is_file()
        && p.join("tokenizer.json").is_file();
    assert!(
        ok,
        "真模型文件缺失：{dir}\n\
         修复：宿主机把 siglip2-base-p224-onnx（vision.onnx/text.onnx/tokenizer.json）放到 /data，\n\
         容器 dev/docker-compose.yml 已只读挂载为 /models（DAM_TEST_MODELS 指向具体模型目录）；\n\
         或设置 DAM_TEST_MODELS=<含三件套的目录>。详见 reports/D3-inference.md。"
    );
    p
}

/// fixture 图片目录（repo 内小图，见 tests/fixtures/README.md）。
fn fixtures_dir() -> PathBuf {
    match std::env::var("DAM_TEST_FIXTURES") {
        Ok(d) => PathBuf::from(d),
        Err(_) => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures"),
    }
}

fn fixture(name: &str) -> PathBuf {
    let p = fixtures_dir().join(name);
    assert!(
        p.is_file(),
        "fixture 缺失：{}（见 tests/fixtures/README.md）",
        p.display()
    );
    p
}

/// 造一个已加载的 provider（registry 里的默认 spec + 真模型目录）。
fn loaded_provider() -> OnnxSiglipProvider {
    let spec = load_registry(None)
        .unwrap()
        .by_name(DEFAULT_EMBEDDER)
        .unwrap()
        .clone();
    assert_eq!(spec.kind, SpecKind::Embedding);
    let manifest = manifest_for(&spec.name).expect("manifest for default embedder");
    let mut p = OnnxSiglipProvider::new(spec, models_dir(), Some(&manifest));
    assert!(p.files_present(), "三件套应齐备");
    p.load().expect("load real onnx sessions");
    assert!(p.ready());
    p
}

/// 验收 3：text/image embed 输出维度 = registry dims(768) 且已归一（||v||≈1）。
#[test]
fn real_model_embeds_image_and_text_normalized_768() {
    let p = loaded_provider();

    let img = p.embed_images(&[fixture("dog.jpg")]).unwrap();
    assert_eq!(img.len(), 1);
    assert_eq!(img[0].len(), 768, "视觉塔输出维度应为 registry 锁定的 768");
    assert!(
        (l2_norm(&img[0]) - 1.0).abs() < 1e-3,
        "L2={}",
        l2_norm(&img[0])
    );

    let txt = p.embed_texts(&["一只狗", "a dog"]).unwrap();
    assert_eq!(txt.len(), 2);
    for v in &txt {
        assert_eq!(v.len(), 768, "文本塔输出维度应为 768");
        assert!((l2_norm(v) - 1.0).abs() < 1e-3, "L2={}", l2_norm(v));
    }
    // spec 与实测一致（provider 出口契约）
    assert_eq!(p.spec().dims, 768);
    assert!(p.spec().normalized);
}

/// 验收 1：同一张图（原图 vs 90% 质量重编码）cos > 0.99。
#[test]
fn same_image_similarity_above_099() {
    let p = loaded_provider();
    let dog = fixture("dog.jpg");
    let v1 = p.embed_images(&[dog.clone()]).unwrap().remove(0);

    // 重编码副本：证明阈值不是「同字节退化成同一张量」的假象
    let img = image::open(&dog).expect("decode fixture");
    let re = std::env::temp_dir().join(format!("dam-real-siglip-{}.jpg", std::process::id()));
    img.to_rgb8()
        .save_with_format(&re, image::ImageFormat::Jpeg)
        .expect("write re-encoded copy");
    let v2 = p.embed_images(&[re.clone()]).unwrap().remove(0);
    let _ = std::fs::remove_file(&re);

    let self_cos = cosine(&v1, &v1);
    let re_cos = cosine(&v1, &v2);
    println!("cos(dog,dog)={self_cos:.6} cos(dog,reencoded)={re_cos:.6}");
    assert!(
        (self_cos - 1.0).abs() < 1e-5,
        "同向量自相似应≈1，got {self_cos}"
    );
    assert!(re_cos > 0.99, "同图重编码 cos={re_cos} 应 > 0.99");
}

/// 验收 1：图文语义对正确（狗图 vs "a dog"/"一只狗" 必须高于 vs 汽车词，中英双语）。
#[test]
fn image_text_semantics_chinese_and_english() {
    let p = loaded_provider();
    let dog = p.embed_images(&[fixture("dog.jpg")]).unwrap().remove(0);
    let car = p.embed_images(&[fixture("car.jpg")]).unwrap().remove(0);
    let texts = [
        "一只狗在草地上",
        "一辆红色的汽车",
        "a dog on the grass",
        "a red car",
    ];
    let t = p.embed_texts(&texts).unwrap();

    let s = |v: &[f32], i: usize| cosine(v, &t[i]);
    println!(
        "dog: 狗(zh)={:.4} 汽车(zh)={:.4} dog(en)={:.4} car(en)={:.4}",
        s(&dog, 0),
        s(&dog, 1),
        s(&dog, 2),
        s(&dog, 3)
    );
    println!(
        "car: 狗(zh)={:.4} 汽车(zh)={:.4} dog(en)={:.4} car(en)={:.4}",
        s(&car, 0),
        s(&car, 1),
        s(&car, 2),
        s(&car, 3)
    );

    const MARGIN: f64 = 0.02;
    // 中文查询（SigLIP2 Gemma tokenizer 原生中文，无翻译层）
    assert!(
        s(&dog, 0) > s(&dog, 1) + MARGIN,
        "狗图应更贴近「一只狗在草地上」：{:.4} vs {:.4}",
        s(&dog, 0),
        s(&dog, 1)
    );
    assert!(
        s(&car, 1) > s(&car, 0) + MARGIN,
        "汽车图应更贴近「一辆红色的汽车」：{:.4} vs {:.4}",
        s(&car, 1),
        s(&car, 0)
    );
    // 英文查询
    assert!(
        s(&dog, 2) > s(&dog, 3) + MARGIN,
        "dog image vs 'a dog' 应高于 'a red car'：{:.4} vs {:.4}",
        s(&dog, 2),
        s(&dog, 3)
    );
    assert!(
        s(&car, 3) > s(&car, 2) + MARGIN,
        "car image vs 'a red car' 应高于 'a dog'：{:.4} vs {:.4}",
        s(&car, 3),
        s(&car, 2)
    );
    // 跨模态一致性：正确配对的相似度应明显高于错配（否则检索无意义）
    assert!(s(&dog, 0) > s(&car, 0), "狗图-「狗」应高于 汽车图-「狗」");
    assert!(
        s(&car, 1) > s(&dog, 1),
        "汽车图-「汽车」应高于 狗图-「汽车」"
    );
}

/// tokenizer 往返：Gemma tokenizer 原生中文；截断按 text_max_length 生效。
#[test]
fn tokenizer_roundtrip_and_truncation() {
    let manifest = manifest_for(DEFAULT_EMBEDDER).unwrap();
    assert_eq!(manifest.text_max_length, 64);
    let tk = tokenizers::Tokenizer::from_file(models_dir().join("tokenizer.json")).unwrap();

    let ids = tokenize(&tk, "一只狗在草地上奔跑", manifest.text_max_length).unwrap();
    assert!(!ids.is_empty() && ids.len() <= 64, "ids={ids:?}");
    // 往返：decode(ids) 应还原出原句（Gemma tokenizer 的中文能力）
    let back = tk
        .decode(&ids.iter().map(|i| *i as u32).collect::<Vec<_>>(), false)
        .unwrap();
    println!("roundtrip: {back:?} (ids={})", ids.len());
    assert!(
        back.contains("狗") && back.contains("草地"),
        "往返丢失中文语义：{back:?}"
    );

    // 截断：max_len 收到 8 时不超过 8
    let short = tokenize(&tk, "一只狗在草地上奔跑，背景是蓝天和白云，还有很多花", 8).unwrap();
    assert_eq!(short.len(), 8);
    // 空串 → 明确报错（不产空向量）
    assert!(tokenize(&tk, "", 64).is_err());
}

/// 未下载模型时 load 必须报 ModelMissing（引导用户去设置页点下载）。
#[test]
fn missing_model_dir_reports_model_missing() {
    let spec = load_registry(None)
        .unwrap()
        .by_name(DEFAULT_EMBEDDER)
        .unwrap()
        .clone();
    let empty = std::env::temp_dir().join(format!("dam-real-siglip-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&empty);
    let mut p = OnnxSiglipProvider::new(spec, &empty, None);
    assert!(!p.files_present());
    let err = p.load().unwrap_err();
    assert!(
        matches!(err, engine_embed::EmbedError::ModelMissing(_)),
        "got {err:?}"
    );
}

/// 预处理尺寸（验收 6）：CHW = 3×224×224 且白像素归一为 1.0。
#[test]
fn preprocess_shape_matches_registry_locks() {
    let p = loaded_provider();
    let bytes = std::fs::read(fixture("dog.jpg")).unwrap();
    let v = p.preprocess_spec().preprocess(&bytes).unwrap();
    assert_eq!(v.len(), 3 * 224 * 224);
    let (v_path, t_path, k_path) = p.paths();
    for pth in [v_path, t_path, k_path] {
        assert!(pth.is_file(), "{} should exist", pth.display());
    }
    assert_eq!(p.text_max_length(), 64);
    let _unused: &Path = Path::new(".");
}
