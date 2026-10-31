#![allow(non_snake_case)] // 本模块 DTO 字段 = 前端 JSON camelCase 契约，勿改
#![allow(clippy::result_large_err)] // 检索面错误即 Response（axum handler 惯例）
//! W3 搜索 API：文搜图（query text）/ 以图搜图（上传图片或库内资产「找相似」）。
//!
//! 编排复用 svc-library（provider 查询向量化 + engine-search 召回 + 标量过滤），
//! provider 由 [`crate::infer::ProviderCache`] 装配（R6：web 只有 openai_compat 远程实现）。
//! 重活（HTTP 推理 + SQLite 扫描）在 spawn_blocking 里跑，持独立连接（WAL 多连接安全），
//! 不阻塞 axum 的 DB 互斥锁。

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::error_codes::{codes, err_coded};
use crate::infer;
use crate::AppState;

/// 默认返回条数（UI 网格一屏 24）。
pub const DEFAULT_K: usize = 24;
/// k 上限（防误传大值把 KNN 拉爆）。
pub const MAX_K: usize = 200;

/// 搜索标量过滤（HTTP 形态；camelCase）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchFilterReq {
    pub tag: Option<String>,
    pub dirPrefix: Option<String>,
    pub ext: Option<String>,
    pub minWidth: Option<i64>,
    pub minHeight: Option<i64>,
    pub dateFrom: Option<i64>,
    pub dateTo: Option<i64>,
}

impl From<SearchFilterReq> for engine_search::SearchFilter {
    fn from(f: SearchFilterReq) -> Self {
        engine_search::SearchFilter {
            tag: f.tag.filter(|s| !s.trim().is_empty()),
            dir_prefix: f.dirPrefix.filter(|s| !s.trim().is_empty()),
            ext: f.ext.filter(|s| !s.trim().is_empty()),
            min_width: f.minWidth,
            min_height: f.minHeight,
            date_from: f.dateFrom,
            date_to: f.dateTo,
        }
    }
}

/// POST /api/search/text 请求体。
#[derive(Debug, Deserialize)]
pub struct TextSearchReq {
    pub query: String,
    #[serde(default)]
    pub k: Option<usize>,
    #[serde(default)]
    pub filter: Option<SearchFilterReq>,
    /// 相关性下限（TEXT-EMBED-1 配套）：低于该 cos 的命中不返回（当无结果）。
    /// 缺省用 svc-library::DEFAULT_TEXT_MIN_SCORE(0.085，2026-10-17 D3 重标定)；
    /// 显式传 0 = 不过滤。字面通道（文件名/标题/标签/OCR）命中不受该下限约束。
    #[serde(default)]
    pub minScore: Option<f32>,
}

/// POST /api/search/similar 请求体（库内资产「找相似」）。
#[derive(Debug, Deserialize)]
pub struct SimilarReq {
    pub assetId: i64,
    #[serde(default)]
    pub k: Option<usize>,
    #[serde(default)]
    pub filter: Option<SearchFilterReq>,
    /// 相似度下限（W3 以图搜图相关性过滤）：缺省用按模型标定的
    /// `svc_library::default_similar_min_score`（siglip2=0.72 / dinov3=0.20）；
    /// 显式传 0 = 不过滤（旧行为）。
    /// **只对 `mode=semantic` 生效**；`visual_precise` 用固定分带（0.90/0.98），带 minScore 会 400。
    #[serde(default)]
    pub minScore: Option<f32>,
    /// 找相似模式（KI-5）：`semantic`（缺省，行为与 W3 完全一致）/ `visual_precise`（精准匹配）。
    #[serde(default)]
    pub mode: Option<svc_library::SimilarMode>,
}

/// POST /api/search/image 的查询参数（body 是图片字节流，所以阈值/条数/过滤只能走 query）。
///
/// 过滤字段名与 `/api/search/text`、`/api/search/similar` 的 body 完全同名（camelCase），
/// 前端把同一份 `SearchFilterReq` 序列化到 query 上即可 —— 修复前这里只接 minScore、
/// 过滤与 k 被静默丢弃（审计 AUDIT-B2 §1：填 ext=png 仍返回 24 条含 jpg）。
#[derive(Debug, Default, Deserialize)]
pub struct ByImageQuery {
    #[serde(rename = "minScore", default)]
    pub min_score: Option<f32>,
    #[serde(default)]
    pub k: Option<usize>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(rename = "dirPrefix", default)]
    pub dir_prefix: Option<String>,
    #[serde(default)]
    pub ext: Option<String>,
    #[serde(rename = "minWidth", default)]
    pub min_width: Option<i64>,
    #[serde(rename = "minHeight", default)]
    pub min_height: Option<i64>,
    #[serde(rename = "dateFrom", default)]
    pub date_from: Option<i64>,
    #[serde(rename = "dateTo", default)]
    pub date_to: Option<i64>,
}

impl ByImageQuery {
    /// 组装标量过滤（空串/空白 = 不限，沿用 SearchFilterReq::into 的同一套归一化）。
    fn filter(&self) -> engine_search::SearchFilter {
        SearchFilterReq {
            tag: self.tag.clone(),
            dirPrefix: self.dir_prefix.clone(),
            ext: self.ext.clone(),
            minWidth: self.min_width,
            minHeight: self.min_height,
            dateFrom: self.date_from,
            dateTo: self.date_to,
        }
        .into()
    }
}

/// 条数校验：缺省用 DEFAULT_K；显式传必须落在 [1, MAX_K]（不静默 clamp，非法就是 400）。
fn validate_k(k: Option<usize>) -> Result<usize, axum::response::Response> {
    match k {
        None => Ok(DEFAULT_K),
        Some(v) if (1..=MAX_K).contains(&v) => Ok(v),
        Some(v) => Err(err(
            StatusCode::BAD_REQUEST,
            "bad_k",
            format!("k 必须在 1~{MAX_K} 之间（收到 {v}）"),
        )),
    }
}

/// 相关性下限校验：缺省 = 后端按模型标定；显式传必须落在 [0,1]。
fn validate_min_score(m: Option<f32>) -> Result<Option<f32>, axum::response::Response> {
    match m {
        None => Ok(None),
        Some(v) if v.is_finite() && (0.0..=1.0).contains(&v) => Ok(Some(v)),
        Some(v) => Err(err(
            StatusCode::BAD_REQUEST,
            "bad_min_score",
            format!("minScore 必须在 0~1 之间（收到 {v}）"),
        )),
    }
}

fn err(status: StatusCode, code: &str, msg: impl Into<String>) -> axum::response::Response {
    (status, Json(json!({ "error": msg.into(), "code": code }))).into_response()
}

/// 把 svc-library 错误翻成 HTTP + 稳定 error code（前端按 code 给引导）。
fn map_lib_err(e: svc_library::LibraryError) -> axum::response::Response {
    use svc_library::LibraryError as L;
    match &e {
        L::Search(engine_search::SearchError::NoStore(m)) => err_coded(
            StatusCode::CONFLICT,
            codes::NO_VECTOR_INDEX,
            json!({ "model": m }),
            format!("当前模型还没有向量索引（{m}）——请先在图库或设置页跑一次嵌入"),
        ),
        L::Search(engine_search::SearchError::ModelMismatch { query, index }) => err_coded(
            StatusCode::CONFLICT,
            codes::MODEL_MISMATCH,
            json!({ "query": query, "index": index }),
            format!("查询模型 {query} 与索引模型 {index} 不一致（禁混算）：请重跑当前模型的嵌入"),
        ),
        L::Search(engine_search::SearchError::Dim { expect, got }) => err_coded(
            StatusCode::CONFLICT,
            codes::DIM_MISMATCH,
            json!({ "expect": expect, "got": got }),
            format!("向量维度不一致：索引 {expect} / 查询 {got}"),
        ),
        L::TextUnsupported(m) => err(
            StatusCode::BAD_REQUEST,
            "text_unsupported",
            format!("模型 {m} 不支持文本查询（image-only）"),
        ),
        L::Embed(engine_embed::EmbedError::NotConfigured(m)) => err_coded(
            StatusCode::BAD_REQUEST,
            codes::PROVIDER_NOT_CONFIGURED,
            json!({ "detail": m }),
            format!("推理端点未配置：{m}"),
        ),
        _ => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "search_failed",
            e.to_string(),
        ),
    }
}

/// 统一搜索执行体（阻塞线程内跑）。返回 (model_key, dims, 结果)。
fn run_search<T>(
    state: &AppState,
    f: impl FnOnce(&rusqlite::Connection, &dyn engine_embed::EmbedProvider) -> svc_library::Result<T>,
) -> Result<(String, u32, T), axum::response::Response> {
    let conn = core_storage::open(&infer::db_path(&state.data_dir))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "db_open", e.to_string()))?;
    let embedder = state.provider.get(&conn, &state.models_root).map_err(|m| {
        err_coded(
            StatusCode::BAD_REQUEST,
            codes::PROVIDER_ERROR,
            json!({ "detail": m }),
            m,
        )
    })?;
    let hits = f(&conn, embedder.provider.as_ref()).map_err(map_lib_err)?;
    Ok((embedder.model_key, embedder.dims, hits))
}

fn hits_response(
    model: String,
    dims: u32,
    k: usize,
    hits: Vec<engine_search::SearchHit>,
    min_score: Option<f32>,
) -> axum::response::Response {
    // minScore = 本次实际生效的相似度下限（前端据此显示"相似度 ≥ X%"）；
    // 文搜图有自己的一套下限（0.085），这里不回传，避免混用。
    let calibrated = svc_library::similar_min_score_calibrated(&model);
    let mut body =
        json!({ "model": model, "dims": dims, "k": k, "count": hits.len(), "hits": hits });
    if let Some(m) = min_score {
        body["minScore"] = json!(m);
        body["minScoreCalibrated"] = json!(calibrated);
    }
    (StatusCode::OK, Json(body)).into_response()
}

fn clamp_k(k: Option<usize>) -> usize {
    k.unwrap_or(DEFAULT_K).clamp(1, MAX_K)
}

/// 精准匹配响应（KI-5）：hits 的 `score` 是**判重塔 cos**，`band` 是分带；
/// 同时回带候选池/缺向量计数/分带阈值 —— 前端不写死数字，缺向量必须提示。
fn precise_response(
    model: String,
    dims: u32,
    k: usize,
    res: svc_library::PreciseResult,
) -> axum::response::Response {
    let count = res.hits.len();
    let mut body = json!({
        "model": model,
        "dims": dims,
        "k": k,
        "count": count,
        "mode": svc_library::SimilarMode::VisualPrecise.as_str(),
    });
    if let Ok(serde_json::Value::Object(map)) = serde_json::to_value(&res) {
        for (key, value) in map {
            body[key] = value;
        }
    }
    (StatusCode::OK, Json(body)).into_response()
}

/// 文搜图：中文/英文文本 → 语义塔文本向量 → KNN + 过滤。
pub async fn text(
    State(state): State<AppState>,
    Json(req): Json<TextSearchReq>,
) -> impl IntoResponse {
    let q = req.query.trim().to_string();
    if q.is_empty() {
        return err(StatusCode::BAD_REQUEST, "empty_query", "query 不能为空");
    }
    let k = clamp_k(req.k);
    let filter: engine_search::SearchFilter = req.filter.unwrap_or_default().into();
    let opts = svc_library::TextSearchOptions {
        min_score: req.minScore,
    };
    let st = state.clone();
    let res = tokio::task::spawn_blocking(move || {
        run_search(&st, |conn, p| {
            svc_library::search_by_text(conn, p, &q, k, &filter, &opts)
        })
    })
    .await;
    match res {
        Ok(Ok((model, dims, hits))) => hits_response(model, dims, k, hits, None),
        Ok(Err(resp)) => resp,
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
    }
}

/// 以图搜图（库内资产）：取 absPath → 图片向量 → KNN（结果含自身，满分 1.0 排第一）。
///
/// `mode`（KI-5）：
/// - `semantic`（缺省）= W3 语义相似，行为与之前完全一致（零回归）；
/// - `visual_precise` = 精准匹配：语义塔扩池召回 → 判重塔（DINOv3）cos rerank → 0.90/0.98 分带，
///   自身恒剔除，缺判重向量**显式计数**（`missingDedupVectors` / `queryMissingDedupVector`），
///   不许静默变少。
pub async fn similar(
    State(state): State<AppState>,
    Json(req): Json<SimilarReq>,
) -> impl IntoResponse {
    let k = match validate_k(req.k) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let mode = req.mode.unwrap_or_default();
    // 精准匹配的分带是写死的（同查重页口径）；带 minScore 进来只会误导，直接拒。
    if mode == svc_library::SimilarMode::VisualPrecise && req.minScore.is_some() {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::BAD_MIN_SCORE,
            json!({
                "near": (svc_library::PRECISE_NEAR_THRESHOLD * 100.0).round() as i64,
                "variant": (svc_library::PRECISE_VARIANT_THRESHOLD * 100.0).round() as i64,
            }),
            format!(
                "精准匹配用固定分带（≥{}% 近似重复 / ≥{}% 同图变体），不接受 minScore；要自定义下限请用语义相似模式",
                (svc_library::PRECISE_NEAR_THRESHOLD * 100.0).round(),
                (svc_library::PRECISE_VARIANT_THRESHOLD * 100.0).round()
            ),
        );
    }
    let min_req = match validate_min_score(req.minScore) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let asset_id = req.assetId;
    let filter: engine_search::SearchFilter = req.filter.unwrap_or_default().into();
    if mode == svc_library::SimilarMode::VisualPrecise {
        let st = state.clone();
        let res = tokio::task::spawn_blocking(move || {
            run_search(&st, |conn, p| {
                svc_library::find_similar_precise(conn, p, asset_id, k, &filter)
            })
        })
        .await;
        return match res {
            Ok(Ok((model, dims, precise))) => precise_response(model, dims, k, precise),
            Ok(Err(resp)) => resp,
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
        };
    }
    let opts = svc_library::SimilarOptions { min_score: min_req };
    let st = state.clone();
    let res = tokio::task::spawn_blocking(move || {
        run_search(&st, |conn, p| {
            svc_library::find_similar(conn, p, asset_id, k, &filter, &opts)
        })
    })
    .await;
    match res {
        Ok(Ok((model, dims, hits))) => {
            // 生效阈值 = 与 svc-library 内同一套解析（同一 model 名 + 同一请求值）
            let min = svc_library::SimilarOptions { min_score: min_req }.effective(&model);
            hits_response(model, dims, k, hits, Some(min))
        }
        Ok(Err(resp)) => resp,
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
    }
}

/// 以图搜图（上传图片）：原始字节 body（Content-Type 任意 image/*）→ 临时文件 → 图片向量 → KNN。
/// 上传图不属于库，返回 hits 里没有自身（相似图才排前面）。
pub async fn by_image(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<ByImageQuery>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let min_req = match validate_min_score(q.min_score) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let k = match validate_k(q.k) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let filter = q.filter();
    if body.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "empty_body",
            "请求体为空（需图片字节流）",
        );
    }
    if body.len() > 32 * 1024 * 1024 {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "图片超过 32MB");
    }
    let ext = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(ext_for_mime)
        .unwrap_or("jpg");
    let dir = state.data_dir.join("query-tmp");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "io", e.to_string());
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("upload-{ts}.{ext}"));
    if let Err(e) = std::fs::write(&path, &body) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "io", e.to_string());
    }
    let opts = svc_library::SimilarOptions { min_score: min_req };
    let st = state.clone();
    let res = tokio::task::spawn_blocking(move || {
        run_search(&st, |conn, p| {
            svc_library::search_by_image_path(conn, p, &path, k, &filter, &opts)
        })
    })
    .await;
    match res {
        Ok(Ok((model, dims, hits))) => {
            let min = svc_library::SimilarOptions { min_score: min_req }.effective(&model);
            hits_response(model, dims, k, hits, Some(min))
        }
        Ok(Err(resp)) => resp,
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
    }
}

fn ext_for_mime(mime: &str) -> &'static str {
    match mime.split(';').next().unwrap_or("").trim() {
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        _ => "jpg",
    }
}

/// GET /api/search/status：搜索可用性总览（向量表 / 覆盖度 / 当前模型）。
/// 前端用它决定「可以搜」还是「先跑一次嵌入」。
pub async fn status(State(state): State<AppState>) -> impl IntoResponse {
    let st = state.clone();
    let res = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, String> {
        let conn = core_storage::open(&infer::db_path(&st.data_dir)).map_err(|e| e.to_string())?;
        let settings = infer::load_settings(&conn);
        let spec = infer::registry()
            .ok()
            .and_then(|r| r.by_name(&settings.active_embedder).cloned());
        let active = spec
            .as_ref()
            .map(|s| infer::vector_model_key(s, &settings))
            .unwrap_or_default();
        // 有效维度：registry spec 声明优先；dims=0 的远程 spec 用设置里实测回的维度
        let active_dims = spec
            .as_ref()
            .map(|s| {
                if s.dims > 0 {
                    s.dims
                } else {
                    settings.embedding.dims
                }
            })
            .unwrap_or(0);
        let tables = core_storage::vecstore::tables(&conn).map_err(|e| e.to_string())?;
        let models: Vec<serde_json::Value> = tables
            .iter()
            .map(|(model, dims, table)| {
                json!({
                    "model": model,
                    "dims": dims,
                    "table": table,
                    "rows": core_storage::vecstore::count(&conn, model).unwrap_or(0),
                    "active": *model == active,
                })
            })
            .collect();
        let emb = svc_pipeline::emb_stats(&conn).map_err(|e| e.to_string())?;
        let stats = svc_pipeline::embed_stats(&conn, &active).map_err(|e| e.to_string())?;
        let active_rows = core_storage::vecstore::count(&conn, &active).unwrap_or(0);
        Ok(json!({
            "activeModel": active,
            "activeDims": active_dims,
            "activeRows": active_rows,
            "searchable": active_rows > 0,
            // 以图搜图相关性下限（W3）：按当前模型的标定值；calibrated=false 表示未见过的模型
            // （远程 openai_compat 等，cos 尺度未知 → 不过滤，UI 不应显示"相似度 ≥ N%"）
            "similarMinScore": svc_library::default_similar_min_score(&active),
            "similarMinScoreCalibrated": svc_library::similar_min_score_calibrated(&active),
            "models": models,
            "emb": emb,
            "semantic": stats.semantic,
            "otherModelVectors": models.iter().any(|m| m["active"] == false),
        }))
    })
    .await;
    match res {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)).into_response(),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, "status", e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
    }
}

/// 供 embed_api 复用：构造空过滤。
pub fn empty_filter() -> engine_search::SearchFilter {
    engine_search::SearchFilter::default()
}
