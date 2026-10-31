//! W3 设置 API：推理 provider 配置读写 + 连接测试（R2/R6：embed provider 可切、可自配）。
//!
//! 门控事实（R6）：web 构建 engine-embed/local-infer 关闭 → 本地 ONNX provider 不可用。
//! registry 里 onnx_* spec 仍可见（registry 是声明式的），但选中时抛明确错误
//! （provider 装配层 Unsupported），设置页按 `capabilities.localInfer=false` 标注不可用。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::error_codes::{codes, err_coded};
use crate::infer;
use crate::AppState;

fn err(status: StatusCode, code: &str, msg: impl Into<String>) -> axum::response::Response {
    (status, Json(json!({ "error": msg.into(), "code": code }))).into_response()
}

// ---- EXIF2（task）：EXIF/GPS 设置 ----

/// GET /api/settings/exif：EXIF 解析 + GPS 档位（默认 parse_on_import=true / gps=off）。
pub async fn get_exif(State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.db.lock().await;
    let cfg: core_config::ExifSettings =
        core_storage::settings::get_or_default(&conn, core_config::EXIF_SETTINGS_KEY)
            .unwrap_or_default();
    (StatusCode::OK, Json(json!(cfg))).into_response()
}

/// PUT /api/settings/exif：保存。GPS 档位合法性在此校验（off/coords/place）。
pub async fn put_exif(
    State(state): State<AppState>,
    Json(cfg): Json<core_config::ExifSettings>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match core_storage::settings::set_json(&conn, core_config::EXIF_SETTINGS_KEY, &cfg) {
        Ok(_) => (StatusCode::OK, Json(json!(cfg))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string(), "code": "storage" })),
        )
            .into_response(),
    }
}

/// GET /api/settings/inference：当前推理设置 + registry embedding spec 列表 + 门控能力。
pub async fn get_inference(State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.db.lock().await;
    let settings = infer::load_settings(&conn);
    let embedders = infer::registry()
        .map(|r| infer::embedding_specs(&r))
        .unwrap_or_default();
    (
        StatusCode::OK,
        Json(json!({
            "settings": settings,
            // 只有文搜图可用（modalities 含 text）的 spec：去重塔等仅图像塔不进候选，
            // 也不单列（KI-6：设置界面不展示去重模型信息）。
            "embedders": embedders,
            "capabilities": {
                "localInfer": engine_embed::LOCAL_INFER,
                "remoteProvider": true,
                "note": if engine_embed::LOCAL_INFER {
                    "本构建含本地 ONNX 推理"
                } else {
                    "web 构建不含本地 ONNX 推理（local-infer 关闭）：只能选 openai_compat 远程端点"
                },
            },
        })),
    )
        .into_response()
}

/// PUT /api/settings/inference：保存设置（校验 provider 可用性 + 远程必填项），
/// 成功后 provider 缓存失效（下次搜索/嵌入按新配置重建）。
pub async fn put_inference(
    State(state): State<AppState>,
    Json(mut settings): Json<core_config::InferenceSettings>,
) -> impl IntoResponse {
    let reg = match infer::registry() {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "registry", e),
    };
    let spec = match reg.by_name(&settings.active_embedder).cloned() {
        Some(s) => s,
        None => {
            return err(
                StatusCode::BAD_REQUEST,
                "unknown_model",
                format!("registry 里没有模型 {}", settings.active_embedder),
            )
        }
    };
    if spec.kind != core_config::SpecKind::Embedding {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::NOT_EMBEDDING,
            json!({ "model": spec.name }),
            format!("{} 不是 embedding 模型", spec.name),
        );
    }
    // 文搜图活跃分子必须有文本能力：仅图像塔（去重塔）被选成活跃 embedder 后
    // 查询文本编不出向量 → 文搜图静默全空转（KI-1）。明确拒绝，不静默降级。
    if !infer::text_capable(&spec) {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::NO_TEXT_MODALITY,
            json!({ "model": spec.name }),
            format!(
                "{} 仅图像向量（registry modalities 无 text），不能作为文搜图活跃 embedder：\
                 请改选含文本能力的模型（如 openai-compat-embedding 远程端点）",
                spec.name
            ),
        );
    }
    // 本地 ONNX provider 在 web 构建里不可用 —— 明确拒绝（不静默降级）
    if !engine_embed::LOCAL_INFER && spec.provider.starts_with("onnx_") {
        return err(
            StatusCode::BAD_REQUEST,
            "local_infer_disabled",
            "web 构建不含本地 ONNX 推理（local-infer 关闭）：请选 openai_compat 远程端点（dam-ai / dashscope / 自定义 base_url）",
        );
    }
    if spec.provider == infer::REMOTE_PROVIDER && !settings.embedding.configured() {
        return err(
            StatusCode::BAD_REQUEST,
            "provider_not_configured",
            "远程 embedding 需要 base_url + model",
        );
    }
    // base_url 规范化：去掉尾部斜杠（provider 内也会拼 /embeddings）
    settings.embedding.base_url = settings
        .embedding
        .base_url
        .trim()
        .trim_end_matches('/')
        .to_string();
    // W4：tag 段同校验（provider 预设 id 合法 + 启用时必填项齐备）
    if let Some(msg) = validate_tag(&settings) {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::TAG_NOT_CONFIGURED,
            json!({ "detail": msg }),
            msg,
        );
    }
    settings.tag.base_url = settings
        .tag
        .base_url
        .trim()
        .trim_end_matches('/')
        .to_string();
    let conn = state.db.lock().await;
    if let Err(e) = infer::save_settings(&conn, &settings) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "save", e.to_string());
    }
    state.provider.invalidate();
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "settings": settings })),
    )
        .into_response()
}

/// base_url 规范化（去尾部斜杠与首尾空白）。
fn normalize_base_url(u: &mut String) {
    *u = u.trim().trim_end_matches('/').to_string();
}

/// W4 打标设置段校验：provider 必须是已知预设 id（或 custom）；启用时
/// base_url + model 必填（configured）。返回错误消息（None = 通过）。
fn validate_tag(settings: &core_config::InferenceSettings) -> Option<String> {
    let tag = &settings.tag;
    let known = core_config::tag_provider_preset(&tag.provider).is_some();
    if !known {
        return Some(format!(
            "未知打标 provider: {}（可选 {}）",
            tag.provider,
            core_config::TAG_PROVIDER_PRESETS
                .iter()
                .map(|p| p.id)
                .collect::<Vec<_>>()
                .join(" / ")
        ));
    }
    if tag.enabled && !tag.configured() {
        return Some("打标已启用但 base_url / 模型名未填写".into());
    }
    None
}

/// GET /api/settings/tag/presets：D9 预设表（R7：首位 = dam-ai 自托管默认）。
pub async fn tag_presets() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(json!({ "presets": core_config::TAG_PROVIDER_PRESETS })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct TagScanQuery {
    pub base_url: String,
}

/// GET /api/settings/tag/scan?base_url=：Ollama 动态扫描已拉取模型。
pub async fn tag_scan(Query(q): Query<TagScanQuery>) -> impl IntoResponse {
    match engine_tag::scan::scan_ollama_models(&q.base_url, 5_000) {
        Ok(models) => (StatusCode::OK, Json(json!({ "models": models }))).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": e.to_string(), "code": "scan_failed" })),
        )
            .into_response(),
    }
}

/// POST /api/settings/tag/test：用表单里的 TagSettings 真实装配 provider 并测试。
/// damai → GET /v1/models（零 token 探活）；openai_compat → chat/completions「回复 OK」。
/// 不落库、不吃缓存；成功时回写 enabled 语义由前端决定（本接口只测连接）。
pub async fn test_tag(
    Json(mut tag): Json<core_config::TagSettings>,
) -> impl IntoResponse {
    normalize_base_url(&mut tag.base_url);
    if !core_config::tag_provider_preset(&tag.provider).is_some() {
        return err(
            StatusCode::BAD_REQUEST,
            "unknown_provider",
            format!("未知打标 provider: {}", tag.provider),
        );
    }
    if !tag.configured() {
        return err(
            StatusCode::BAD_REQUEST,
            "not_configured",
            "base_url / 模型名未填写",
        );
    }
    let res = tokio::task::spawn_blocking(move || -> Result<core_types::TagTestResult, String> {
        let reg = infer::registry().map_err(|e| e.to_string())?;
        let mut spec = reg
            .models
            .iter()
            .find(|m| m.kind == core_config::SpecKind::Tagging)
            .cloned()
            .ok_or("registry 无 tagging spec")?;
        spec.name = format!("{}:{}", tag.provider, tag.model.trim());
        let started = std::time::Instant::now();
        let mut provider = engine_tag::build_provider(spec, tag.clone());
        provider.load().map_err(|e| e.to_string())?;
        match provider.test_connection() {
            Ok(reply) => Ok(core_types::TagTestResult {
                ok: true,
                model: format!("{}:{}", tag.provider, tag.model.trim()),
                latencyMs: started.elapsed().as_millis() as u64,
                reply: Some(reply),
                error: None,
            }),
            Err(e) => Ok(core_types::TagTestResult {
                ok: false,
                model: format!("{}:{}", tag.provider, tag.model.trim()),
                latencyMs: started.elapsed().as_millis() as u64,
                reply: None,
                error: Some(e.to_string()),
            }),
        }
    })
    .await;
    match res {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)).into_response(),
        Ok(Err(m)) => err_coded(
            StatusCode::BAD_GATEWAY,
            codes::CONNECT_FAILED,
            json!({ "detail": m }),
            m,
        ),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
    }
}

/// POST /api/settings/embed/test：按当前设置真实装配 provider（不吃缓存）并 probe，
/// 返回实测维度（远程 provider 的 dims 以实测为准，并回写设置）。
pub async fn test_embed(State(state): State<AppState>) -> impl IntoResponse {
    let st = state.clone();
    let res = tokio::task::spawn_blocking(move || {
        let conn = core_storage::open(&infer::db_path(&st.data_dir)).map_err(|e| e.to_string())?;
        let settings = infer::load_settings(&conn);
        let (model_key, dims, endpoint_model) = st.provider.test_connection(&conn, &st.models_root)?;
        Ok::<_, String>(json!({
            "ok": true,
            "modelKey": model_key,
            "dims": dims,
            "endpointModel": endpoint_model,
            "baseUrl": settings.embedding.base_url,
        }))
    })
    .await;
    match res {
        Ok(Ok(v)) => {
            state.provider.invalidate();
            (StatusCode::OK, Json(v)).into_response()
        }
        Ok(Err(m)) => err_coded(
            StatusCode::BAD_GATEWAY,
            codes::CONNECT_FAILED,
            json!({ "detail": m }),
            m,
        ),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
    }
}
