//! 云 VL 打标客户端 × mock OpenAI 服务端（真实 HTTP，不进外网）。
//! 覆盖：请求体形态（model/messages/content parts/图片 data URL）、回复解析、
//! 未配置时的引导错误、上游报错路径。

mod common;

use common::MockServer;
use core_config::{load_registry, TagSettings};
use engine_tag::openai_compat::OpenAiCompatTagProvider;
use engine_tag::{TagError, TagProvider};

const FIXTURE_JSON: &str = r#"{
  "id": "chatcmpl-mock",
  "object": "chat.completion",
  "model": "mock-vl",
  "choices": [{
    "index": 0,
    "message": {
      "role": "assistant",
      "content": "```json\n{\"title_ch\":\"草地上的金毛犬\",\"title_en\":\"Golden retriever on grass\",\"description\":\"一只金毛寻回犬趴在草地上\",\"tags\":[{\"name\":\"狗\",\"score\":0.98},{\"name\":\"草地\",\"score\":0.8},{\"name\":\"宠物\",\"score\":0.75}]}\n```"
    },
    "finish_reason": "stop"
  }]
}"#;

fn tag_spec() -> core_config::ModelSpec {
    load_registry(None)
        .unwrap()
        .models
        .into_iter()
        .find(|m| m.kind == core_config::SpecKind::Tagging)
        .unwrap()
}

fn cfg_for(base_url: &str) -> TagSettings {
    TagSettings {
        enabled: true,
        base_url: format!("{base_url}/v1"),
        api_key: "test-key".into(),
        model: "mock-vl".into(),
        proxy: Some(String::new()), // 直连（绕过 env 代理）
        timeout_ms: 10_000,
        ..Default::default()
    }
}

fn write_temp_jpeg(tag: &str) -> std::path::PathBuf {
    // 1×1 PNG（真实图片字节，验证 data URL 编码）
    let png: Vec<u8> = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    let p = std::env::temp_dir().join(format!(
        "dam-tag-mock-{tag}-{}-{}.png",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&p, &png).unwrap();
    p
}

#[test]
fn tag_image_against_mock_openai_server() {
    let srv = MockServer::start(|_path, _body| (200, FIXTURE_JSON.to_string()));
    let mut provider = OpenAiCompatTagProvider::new(tag_spec(), cfg_for(&srv.base_url));
    provider.load().expect("load with configured endpoint");
    assert!(provider.ready());

    let img = write_temp_jpeg("ok");
    let out = provider.tag_image(&img).expect("tag");
    assert_eq!(out.title_ch, "草地上的金毛犬");
    assert_eq!(out.title_en, "Golden retriever on grass");
    assert_eq!(out.tags.len(), 3);
    assert_eq!(out.tags[0].name, "狗");
    assert_eq!(out.tags[0].origin, "auto");

    // 请求形态检查：POST /v1/chat/completions + model + 两个 content part + data URL 图片
    assert_eq!(srv.count(), 1);
    assert_eq!(srv.last_path().as_deref(), Some("/v1/chat/completions"));
    let body: serde_json::Value = serde_json::from_str(&srv.last_body().unwrap()).unwrap();
    assert_eq!(body["model"], "mock-vl");
    // max_tokens 默认取 TagSettings::DEFAULT_MAX_TOKENS（卡 task 定「默认够长」，2026-10-20 统一 4096）
    assert_eq!(body["max_tokens"], TagSettings::DEFAULT_MAX_TOKENS);
    let content = &body["messages"][0]["content"];
    assert_eq!(content[0]["type"], "text");
    assert!(content[0]["text"].as_str().unwrap().contains("title_ch"));
    assert_eq!(content[1]["type"], "image_url");
    let url = content[1]["image_url"]["url"].as_str().unwrap();
    // 送 VL 前统一归一为 JPEG 并缩到短边 512（见 engine-tag::vl_image）：
    // 修复前这里是 data:image/png 原图直送，3072² 会在 vLLM 侧超上下文（DESKTOP-TAG-CTX-1）
    assert!(url.starts_with("data:image/jpeg;base64,"), "got {url}");
}

#[test]
fn unconfigured_provider_gives_guidance_instead_of_panic() {
    let mut provider = OpenAiCompatTagProvider::new(
        tag_spec(),
        TagSettings {
            enabled: false,
            ..Default::default()
        },
    );
    assert!(!provider.ready());
    let err = provider.load().unwrap_err();
    match err {
        TagError::NotConfigured(msg) => assert!(msg.contains("未启用"), "got {msg}"),
        other => panic!("expected NotConfigured, got {other:?}"),
    }
    // 未 load → tag_image 必须报 NotLoaded（不能静默返回空结果）
    let img = write_temp_jpeg("notconfigured");
    assert!(matches!(
        provider.tag_image(&img).unwrap_err(),
        TagError::NotLoaded(_)
    ));
    // test_connection 也要给出可读错误（UI「测试连接」按钮直接展示）
    assert!(matches!(
        provider.test_connection().unwrap_err(),
        TagError::NotConfigured(_)
    ));
    assert!(!provider.usable());
}

#[test]
fn test_connection_and_upstream_error_paths() {
    // 正常：/chat/completions 回纯文本
    let ok = MockServer::start(|_p, _b| {
        (
            200,
            r#"{"choices":[{"message":{"content":"OK"}}]}"#.to_string(),
        )
    });
    let provider = OpenAiCompatTagProvider::new(tag_spec(), cfg_for(&ok.base_url));
    assert_eq!(provider.test_connection().unwrap(), "OK");
    assert_eq!(ok.count(), 1);

    // 上游返回 error 字段 → Upstream 错误（不静默）
    let bad = MockServer::start(|_p, _b| {
        (
            500,
            r#"{"error":{"message":"model not found","type":"invalid_request_error"}}"#.to_string(),
        )
    });
    let p2 = OpenAiCompatTagProvider::new(tag_spec(), cfg_for(&bad.base_url));
    match p2.test_connection() {
        Err(TagError::Upstream(msg)) => assert!(msg.contains("model not found"), "got {msg}"),
        other => panic!("expected Upstream, got {other:?}"),
    }

    // 回复里没有 JSON → Parse 错误（需先 load，否则 NotLoaded 挡在前面）
    let garbage = MockServer::start(|_p, _b| {
        (
            200,
            r#"{"choices":[{"message":{"content":"我看不出来"}}]}"#.to_string(),
        )
    });
    let mut p3 = OpenAiCompatTagProvider::new(tag_spec(), cfg_for(&garbage.base_url));
    p3.load().unwrap();
    let img = write_temp_jpeg("garbage");
    assert!(matches!(p3.tag_image(&img), Err(TagError::Parse(_))));
}

#[test]
fn tag_image_requires_ready_state() {
    let srv = MockServer::start(|_p, _b| (200, FIXTURE_JSON.to_string()));
    let provider = OpenAiCompatTagProvider::new(tag_spec(), cfg_for(&srv.base_url));
    // 没有 load() → 不发请求
    let img = write_temp_jpeg("notloaded");
    assert!(matches!(
        provider.tag_image(&img).unwrap_err(),
        TagError::NotLoaded(_)
    ));
    assert_eq!(srv.count(), 0, "未 ready 时不应发出请求");
}

// ---------- max_tokens 截断（卡 task） ----------

/// 真机形态：finish_reason=length + JSON 在 tags 数组里被砍断。
fn truncated_body(raw: &str, finish: &str) -> String {
    serde_json::json!({
        "id": "chatcmpl-trunc",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": raw },
            "finish_reason": finish,
        }],
    })
    .to_string()
}

const CUT_RAW: &str = "{\n  \"title_ch\": \"金黄炸薯条配番茄酱\",\n  \"title_en\": \"Golden French Fries with Ketchup\",\n  \"description\": \"木制砧板上金黄色的厚切炸薯条，配有红艳的番茄酱和新鲜迷迭香点缀。\",\n  \"tags\": [\n    {\"name\": \"炸薯条\", \"score\": 0.99},\n    {\"name\": \"番茄酱\", \"score\": 0.98},\n    {\"name\": \"木砧板\", \"score\": 0.95}";

#[test]
fn truncated_reply_is_repaired_and_not_counted_as_failure() {
    let srv = MockServer::start(|_p, _b| (200, truncated_body(CUT_RAW, "length")));
    let mut provider = OpenAiCompatTagProvider::new(tag_spec(), cfg_for(&srv.base_url));
    provider.load().unwrap();
    let img = write_temp_jpeg("truncated");
    let out = provider.tag_image(&img).expect("截断应补齐救回");
    assert!(out.repaired, "repaired 标记要落回调用方");
    assert_eq!(out.title_ch, "金黄炸薯条配番茄酱");
    assert_eq!(out.tags.len(), 3);
}

#[test]
fn unrepairable_truncation_reports_effective_max_tokens() {
    // 只有开头 → 补不出内容：报 Truncated，且文案带上本次实际下发的 max_tokens + 来源
    let srv = MockServer::start(|_p, _b| (200, truncated_body("{\"title_ch\": ", "length")));
    let mut provider = OpenAiCompatTagProvider::new(tag_spec(), cfg_for(&srv.base_url));
    provider.load().unwrap();
    let img = write_temp_jpeg("cut-head");
    match provider.tag_image(&img) {
        Err(TagError::Truncated {
            max_tokens,
            budget,
            detail,
        }) => {
            assert_eq!(max_tokens, TagSettings::DEFAULT_MAX_TOKENS, "默认生效值");
            assert!(budget.contains("默认"), "budget: {budget}");
            assert!(detail.contains("finish_reason=length"), "detail: {detail}");
        }
        other => panic!("expected Truncated, got {other:?}"),
    }

    // 用户把 max_tokens 改小 → 错误文案必须是改后的值 + 「设置页覆盖」
    let srv2 = MockServer::start(|_p, _b| (200, truncated_body("{\"title_ch\": ", "length")));
    let mut cfg = cfg_for(&srv2.base_url);
    cfg.max_tokens = 1024;
    let mut p2 = OpenAiCompatTagProvider::new(tag_spec(), cfg);
    p2.load().unwrap();
    let img2 = write_temp_jpeg("cut-head-1024");
    let msg = format!("{}", p2.tag_image(&img2).unwrap_err());
    assert!(msg.contains("max_tokens=1024"), "got {msg}");
    assert!(msg.contains("设置页覆盖"), "got {msg}");
    // 下发到 wire 的也是改后的值
    let body: serde_json::Value = serde_json::from_str(&srv2.last_body().unwrap()).unwrap();
    assert_eq!(body["max_tokens"], 1024);
}

#[test]
fn empty_content_with_length_finish_is_truncation_not_parse_error() {
    // 推理模型把预算全花在思考 token 上 → content 为空、finish_reason=length
    let srv = MockServer::start(|_p, _b| (200, truncated_body("", "length")));
    let mut provider = OpenAiCompatTagProvider::new(tag_spec(), cfg_for(&srv.base_url));
    provider.load().unwrap();
    let img = write_temp_jpeg("empty-length");
    let err = provider.tag_image(&img).unwrap_err();
    assert!(
        matches!(err, TagError::Truncated { .. }),
        "空 content + length 必须归为截断，而不是 response parse: got {err:?}"
    );
    assert!(format!("{err}").contains(&format!("max_tokens={}", TagSettings::DEFAULT_MAX_TOKENS)));
}
