//! 真实 DINOv3 ViT-S/16 模型测试（D5 去重塔）。
//! 环境：DAM_TEST_MODELS_DEDUP=/models/dinov3-vits16（容器只读挂载；缺 → 显式失败）。
//! 覆盖：384 维 + L2 归一出口、同图 cos≈1、异图可分、embed_texts 明确不支持。

use std::path::PathBuf;

use engine_embed::onnx_dinov3::OnnxDinoV3Provider;
use engine_embed::EmbedProvider;

fn dir() -> PathBuf {
    let d = std::env::var("DAM_TEST_MODELS_DEDUP").unwrap_or_else(|_| {
        panic!(
            "需要真实模型目录：export DAM_TEST_MODELS_DEDUP=<含 model.onnx + model.onnx_data 的目录>\n\
             容器 dev/docker-compose.yml 已只读挂载为 /models（DAM_TEST_MODELS_DEDUP 指向具体模型目录）"
        );
    });
    PathBuf::from(d)
}

fn provider() -> OnnxDinoV3Provider {
    let reg = core_config::load_registry(None).unwrap();
    let spec = reg.by_name(core_config::DEDUP_EMBEDDER).unwrap().clone();
    let mut p = OnnxDinoV3Provider::new(spec, dir(), None);
    p.load().unwrap();
    p
}

fn fixture(name: &str) -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name);
    assert!(p.is_file(), "fixture missing: {}", p.display());
    p
}

fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

#[test]
fn real_dinov3_embeds_normalized_384_and_near_dup_sensitive() {
    let p = provider();

    // 维度与归一化（registry spec：384 / normalized=true）
    let car = p.embed_images(&[fixture("car.jpg")]).unwrap().remove(0);
    let dog = p.embed_images(&[fixture("dog.jpg")]).unwrap().remove(0);
    assert_eq!(car.len(), 384);
    assert!(
        (norm(&car) - 1.0).abs() < 1e-3,
        "出口 L2 归一，got {}",
        norm(&car)
    );
    assert!((norm(&dog) - 1.0).abs() < 1e-3);

    // 同一文件再次嵌入 → 确定性 + cos≈1
    let car2 = p.embed_images(&[fixture("car.jpg")]).unwrap().remove(0);
    let cos_same = engine_embed::cosine(&car, &car2);
    assert!(cos_same > 0.999, "同图 cos={cos_same}");

    // 不同语义图 → 显著低于近重复带（0.98）
    let cos_diff = engine_embed::cosine(&car, &dog);
    assert!(
        cos_diff < 0.98,
        "car vs dog cos={cos_diff}（去重塔必须能区分语义不同的图）"
    );
}

#[test]
fn real_dinov3_texts_unsupported_is_explicit() {
    let p = provider();
    let err = p.embed_texts(&["红色圆形"]).unwrap_err();
    assert!(err.to_string().contains("image-only"), "got {err}");
}
