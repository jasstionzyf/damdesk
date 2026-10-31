//! D9 Ollama 模型扫描 × mock 服务端（真实 HTTP，不进外网）：
//! 扫描命中 `/api/tags`（base_url 带 /v1 也能定位到原生口）、解析 models[].name、
//! 空模型表给「先 pull」指引、非 JSON 给解析错误。

mod common;

use common::MockServer;
use engine_tag::scan::scan_ollama_models;

#[test]
fn scan_lists_models_from_api_tags() {
    let srv = MockServer::start(|path, _body| {
        assert_eq!(
            path, "/api/tags",
            "必须打到原生口 /api/tags，不是 /v1/api/tags"
        );
        (
            200,
            r#"{"models":[
                 {"name":"qwen2.5vl:7b"},
                 {"name":"llava:13b"},
                 {"name":"qwen2.5vl:7b"}
               ]}"#
            .to_string(),
        )
    });
    // base_url 带 /v1（预设形态）→ 扫描自动回退到原生根
    let models = scan_ollama_models(&format!("{}/v1", srv.base_url), 5_000).expect("scan");
    assert_eq!(
        models,
        vec!["llava:13b".to_string(), "qwen2.5vl:7b".to_string()]
    );
    // 不带 /v1 也能扫
    let models2 = scan_ollama_models(&srv.base_url, 5_000).expect("scan2");
    assert_eq!(models2.len(), 2);
}

#[test]
fn scan_empty_model_list_guides_pull() {
    let srv = MockServer::start(|_p, _b| (200, r#"{"models":[]}"#.to_string()));
    let err = scan_ollama_models(&format!("{}/v1", srv.base_url), 5_000).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("没有任何模型"), "got: {msg}");
    assert!(msg.contains("ollama pull"), "got: {msg}");
}

#[test]
fn scan_non_json_is_parse_error() {
    let srv = MockServer::start(|_p, _b| (200, "<html>hi</html>".to_string()));
    let err = scan_ollama_models(&format!("{}/v1", srv.base_url), 5_000).unwrap_err();
    assert!(err.to_string().contains("不是 JSON"), "got: {err}");
}
