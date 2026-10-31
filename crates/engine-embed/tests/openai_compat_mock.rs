//! 远程 embedding 客户端 × mock OpenAI 服务端（真实 HTTP，不进外网）。
//!
//! 覆盖 D3 第二 provider（`OpenAiCompatProvider`）的对外契约：请求体形态
//! （`/v1/embeddings` + model + input + encoding_format）、图片按 data URL 提交、
//! 出口 L2 归一、维度校验（配置 dims vs 实测）、未配置/未 load/上游报错三条错误路径。
//!
//! mock 服务器自带（`common/mod.rs`）：engine-embed 与 engine-tag 各持一份，
//! 因为 L1 同层禁止互相引用，测试工具也不跨 crate 共享。

mod common;

use common::MockServer;
use core_config::{load_registry, RemoteEmbedSettings};
use engine_embed::openai_compat::OpenAiCompatProvider;
use engine_embed::{l2_norm, EmbedError, EmbedProvider};

fn embed_spec() -> core_config::ModelSpec {
    load_registry(None)
        .unwrap()
        .by_name("openai-compat-embedding")
        .unwrap()
        .clone()
}

fn cfg(base_url: &str, dims: u32) -> RemoteEmbedSettings {
    RemoteEmbedSettings {
        base_url: format!("{base_url}/v1"),
        api_key: "test-key".into(),
        model: "mock-embed".into(),
        dims,
        timeout_secs: 10,
        proxy: Some(String::new()), // 直连（绕开 env 代理）
    }
}

fn embeddings_response(dims: usize, n: usize) -> String {
    let data: Vec<serde_json::Value> = (0..n)
        .map(|i| {
            let mut v = vec![0.0f32; dims];
            v[i % dims] = 1.0; // 单位基向量，便于断言
            serde_json::json!({"object": "embedding", "index": i, "embedding": v})
        })
        .collect();
    serde_json::json!({"object": "list", "model": "mock-embed", "data": data}).to_string()
}

fn write_temp_png(tag: &str) -> std::path::PathBuf {
    // 1×1 PNG：真图片字节，验证 data URL 编码
    let png: Vec<u8> = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    let p = std::env::temp_dir().join(format!("dam-embed-mock-{tag}-{}.png", std::process::id()));
    std::fs::write(&p, &png).unwrap();
    p
}

#[test]
fn embed_texts_hits_openai_compatible_endpoint() {
    let srv = MockServer::start(|_p, _b| (200, embeddings_response(768, 2)));
    let mut p = OpenAiCompatProvider::new(embed_spec(), cfg(&srv.base_url, 0));
    p.load().expect("load with configured endpoint");
    assert!(p.ready());

    // load 含一次连通性探针（顺带锁定实测维度）→ 此时恰好 1 次请求，且是 ping 文本
    assert_eq!(srv.count(), 1, "load 的连通性探针应恰好发一次请求");
    assert_eq!(srv.last_path().as_deref(), Some("/v1/embeddings"));
    let probe: serde_json::Value = serde_json::from_str(&srv.last_body().unwrap()).unwrap();
    assert_eq!(probe["input"][0], "ping", "探针请求体形态");
    assert_eq!(p.measured_dims(), Some(768), "探针后应已锁定实测维度");

    let v = p.embed_texts(&["一只狗", "一辆汽车"]).unwrap();
    assert_eq!(v.len(), 2);
    assert_eq!(v[0].len(), 768);
    assert!((l2_norm(&v[0]) - 1.0).abs() < 1e-6, "出口必须 L2 归一");
    assert_eq!(p.measured_dims(), Some(768));

    assert_eq!(srv.count(), 2, "探针 1 次 + 真实调用 1 次");
    assert_eq!(srv.last_path().as_deref(), Some("/v1/embeddings"));
    let body: serde_json::Value = serde_json::from_str(&srv.last_body().unwrap()).unwrap();
    assert_eq!(body["model"], "mock-embed");
    assert_eq!(body["encoding_format"], "float");
    assert_eq!(body["input"][0], "一只狗");
    assert_eq!(body["input"][1], "一辆汽车");
}

#[test]
fn embed_images_sends_data_url_content_parts() {
    let srv = MockServer::start(|_p, _b| (200, embeddings_response(8, 1)));
    let mut p = OpenAiCompatProvider::new(embed_spec(), cfg(&srv.base_url, 0));
    p.load().unwrap();
    assert_eq!(srv.count(), 1, "load 只发探针");
    let img = write_temp_png("dataurl");

    let v = p.embed_images(&[img]).unwrap();
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].len(), 8);
    assert_eq!(srv.count(), 2, "探针 + 一次图片调用");

    // 探针是纯文本、图片调用是 content parts（逐个请求核对，不只看最后一次）
    let reqs: Vec<(String, String, String)> = srv.requests.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2);
    let probe: serde_json::Value = serde_json::from_str(&reqs[0].2).unwrap();
    assert_eq!(probe["input"][0], "ping", "第 1 次请求必须是探针");
    let body: serde_json::Value = serde_json::from_str(&reqs[1].2).unwrap();
    let input = &body["input"][0];
    assert_eq!(input["type"], "image_url");
    let url = input["image_url"]["url"].as_str().unwrap();
    assert!(url.starts_with("data:image/png;base64,"), "got {url}");
    // 鉴权头形态（bearer）
    // 说明：mock 记录的是请求体，鉴权头在 http.rs 统一注入，形态由 engine-tag 侧同源测试覆盖。
}

#[test]
fn dimension_mismatch_is_rejected_not_silently_truncated() {
    // 配置要求 768 维、端点实测 8 维 → load 阶段（连通性探针）就必须报 Dim，
    // 不能等到写库时才炸、更不能静默截断/补零
    let srv = MockServer::start(|_p, _b| (200, embeddings_response(8, 1)));
    let mut p = OpenAiCompatProvider::new(embed_spec(), cfg(&srv.base_url, 768));
    match p.load().unwrap_err() {
        EmbedError::Dim { expect, got } => assert_eq!((expect, got), (768, 8)),
        other => panic!("expected Dim, got {other:?}"),
    }
    assert!(!p.ready(), "维度不符时不得标记 ready");
    assert!(matches!(
        p.embed_texts(&["x"]).unwrap_err(),
        EmbedError::NotLoaded(_)
    ));

    // dims=0（「由端点决定」）→ load 通过，实测维度即端点维度，向量原样返回（不截断不补零）
    let by_endpoint = MockServer::start(|_p, _b| (200, embeddings_response(8, 1)));
    let mut p2 = OpenAiCompatProvider::new(embed_spec(), cfg(&by_endpoint.base_url, 0));
    p2.load().unwrap();
    assert_eq!(p2.measured_dims(), Some(8), "dims=0 时以端点实测为准");
    assert_eq!(p2.embed_texts(&["x"]).unwrap()[0].len(), 8);

    // 配置与端点一致 → 通过（正向路径，保证上面的报错不是误报）
    let same = MockServer::start(|_p, _b| (200, embeddings_response(8, 1)));
    let mut p3 = OpenAiCompatProvider::new(embed_spec(), cfg(&same.base_url, 8));
    p3.load().unwrap();
    assert!(p3.ready());
    assert_eq!(p3.embed_texts(&["x"]).unwrap()[0].len(), 8);
}

#[test]
fn upstream_error_and_unconfigured_paths_are_explicit() {
    // 上游 400 → load 的探针即报 Http（含状态码），不静默吞掉
    let bad = MockServer::start(|_p, _b| {
        (
            400,
            r#"{"error":{"message":"model not found","type":"invalid_request_error"}}"#.to_string(),
        )
    });
    let mut p = OpenAiCompatProvider::new(embed_spec(), cfg(&bad.base_url, 0));
    match p.load().unwrap_err() {
        EmbedError::Http(msg) => assert!(msg.contains("400"), "got {msg}"),
        other => panic!("expected Http, got {other:?}"),
    }
    assert!(!p.ready());
    // 未 load（load 失败）→ embed 必须报 NotLoaded
    assert!(matches!(
        p.embed_texts(&["x"]).unwrap_err(),
        EmbedError::NotLoaded(_)
    ));

    // 空 response data → 明确报错（不能返回空向量列表）
    let empty = MockServer::start(|_p, _b| (200, r#"{"data":[]}"#.to_string()));
    let mut p2 = OpenAiCompatProvider::new(embed_spec(), cfg(&empty.base_url, 0));
    assert!(p2.load().is_err(), "端点回空 data 必须报错");
    assert!(!p2.ready());

    // 未配置 endpoint → NotConfigured（引导用户去设置页填）
    let mut p3 = OpenAiCompatProvider::new(embed_spec(), RemoteEmbedSettings::default());
    assert!(matches!(
        p3.load().unwrap_err(),
        EmbedError::NotConfigured(_)
    ));
    assert!(matches!(
        p3.embed_texts(&["x"]).unwrap_err(),
        EmbedError::NotLoaded(_)
    ));
}

#[test]
fn base64_float_payload_is_accepted() {
    // 有些 OpenAI 兼容服务默认回 base64 float32（不是 JSON 数组）：必须同样可解析
    use base64::Engine;
    let mut raw = Vec::new();
    raw.extend_from_slice(&0.5f32.to_le_bytes());
    raw.extend_from_slice(&0.5f32.to_le_bytes());
    let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
    let body = serde_json::json!({"data":[{"embedding": b64}]}).to_string();
    let srv = MockServer::start(move |_p, _b| (200, body.clone()));
    let mut p = OpenAiCompatProvider::new(embed_spec(), cfg(&srv.base_url, 0));
    p.load().unwrap();
    let v = p.embed_texts(&["x"]).unwrap();
    assert_eq!(v[0].len(), 2);
    assert!((l2_norm(&v[0]) - 1.0).abs() < 1e-6, "base64 路径同样要归一");
}
