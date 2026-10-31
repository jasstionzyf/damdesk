//! E3 智能文件夹（保存搜索）HTTP API：CRUD 薄壳。
//! 语义约束在 L0 DAO（名称唯一/query 白名单归一化），本层零业务逻辑。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::AppState;

#[derive(Deserialize)]
pub struct UpsertFolderBody {
    pub name: Option<String>,
    /// AssetQuery 形态 JSON 字符串（也可以是对象，统一在这里字符串化）
    pub query: Option<serde_json::Value>,
}

fn query_str(v: Option<serde_json::Value>) -> Option<String> {
    v.map(|q| match q {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    })
}

fn folder_json(f: core_types::SmartFolder) -> serde_json::Value {
    json!({
        "id": f.id,
        "name": f.name,
        "query": f.query,
        "createdAt": f.createdAt,
        "updatedAt": f.updatedAt,
    })
}

/// GET /api/smart-folders — 全量（侧栏/命令面板）。
pub async fn list(State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match core_storage::smart_folders::list(&conn) {
        Ok(v) => {
            let items: Vec<serde_json::Value> = v.into_iter().map(folder_json).collect();
            (StatusCode::OK, Json(json!(items))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// GET /api/smart-folders/{id} — 单个（查看页直达）。
pub async fn get(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match core_storage::smart_folders::get(&conn, id) {
        Ok(Some(f)) => (StatusCode::OK, Json(folder_json(f))).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("智能文件夹不存在：{id}") })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// POST /api/smart-folders — 新建（name+query 必填）。
pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<UpsertFolderBody>,
) -> impl IntoResponse {
    let Some(name) = body.name.clone() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "缺少 name" })),
        )
            .into_response();
    };
    let Some(query) = query_str(body.query) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "缺少 query" })),
        )
            .into_response();
    };
    let conn = state.db.lock().await;
    match core_storage::smart_folders::create(&conn, &name, &query) {
        Ok(f) => (StatusCode::OK, Json(folder_json(f))).into_response(),
        Err(core_storage::StorageError::Invalid(msg)) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// PUT /api/smart-folders/{id} — 更新（name/query 任意子集）。
pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpsertFolderBody>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match core_storage::smart_folders::update(
        &conn,
        id,
        body.name.as_deref(),
        query_str(body.query).as_deref(),
    ) {
        Ok(f) => (StatusCode::OK, Json(folder_json(f))).into_response(),
        Err(core_storage::StorageError::Invalid(msg)) => {
            let status = if msg.contains("不存在") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            (status, Json(json!({ "error": msg }))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// DELETE /api/smart-folders/{id}。
pub async fn remove(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match core_storage::smart_folders::delete(&conn, id) {
        Ok(true) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("智能文件夹不存在：{id}") })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}
