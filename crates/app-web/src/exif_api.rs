//! EXIF2（t_7d9bb323）— 元数据回填 API：
//! - POST /api/exif/backfill  入队存量（exif IS NULL）+ 同步跑完（解析纯本地、毫秒/张）
//! - POST /api/exif/retry-failed  复活 failed job（文件恢复后）
//! - GET  /api/exif/status  队列统计 + 剩余未解析数 + GPS/解析开关快照
//!
//! 同步跑的取舍：解析无网络依赖（Place 档逆编码除外，5s 超时/张），万级库秒级；
//! 拿 db 锁直跑避免后台任务生命周期管理（embed/tag 队列是网络型才有后台态）。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use std::sync::atomic::AtomicBool;

use crate::AppState;
use svc_pipeline::{
    exif_enqueue_missing, exif_reset_for_reparse, exif_retry_failed, exif_status,
    run_exif_queue_with_limit,
};

fn err(status: StatusCode, msg: impl Into<String>) -> axum::response::Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

#[derive(Deserialize, Default)]
pub struct BackfillReq {
    /// 可选 assetId 区间（默认全库缺元数据集合）
    pub fromId: Option<i64>,
    pub toId: Option<i64>,
    /// 本轮上限（0/缺省 = 跑完）
    pub limit: Option<usize>,
    /// 强制重解析：已出结论的图也重跑（换 GPS 档 / 换解析器版本后让存量图生效）。
    /// 见 svc_pipeline::exif_reset_for_reparse（显式动作，不隐式触发）。
    #[serde(default)]
    pub force: bool,
}

/// POST /api/exif/backfill
pub async fn backfill(State(state): State<AppState>, Json(req): Json<BackfillReq>) -> impl IntoResponse {
    let mut conn = state.db.lock().await;
    let reset = if req.force {
        match exif_reset_for_reparse(&conn, req.fromId, req.toId) {
            Ok(n) => n,
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    } else {
        0
    };
    let enqueued = match exif_enqueue_missing(&conn, req.fromId, req.toId) {
        Ok(n) => n,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let cancel = AtomicBool::new(false);
    let run = run_exif_queue_with_limit(&conn, &cancel, &mut |_, _, _, _| {}, req.limit.unwrap_or(0));
    let cfg: core_config::ExifSettings = core_storage::settings::get_or_default(
        &conn,
        core_config::EXIF_SETTINGS_KEY,
    )
    .unwrap_or_default();
    match run {
        Ok(report) => (
            StatusCode::OK,
            Json(json!({
                "enqueued": enqueued,
                "reset": reset,
                "processed": report.processed,
                "done": report.done,
                "failed": report.failed,
                "retry": report.retry,
                "skipped": report.skipped,
                "cancelled": report.cancelled,
                "errors": report.errors,
                "jobDone": report.jobDone,
                "jobFailed": report.jobFailed,
                "jobPending": report.jobPending,
                "gpsMode": format!("{:?}", cfg.gps).to_lowercase(),
            })),
        )
            .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// POST /api/exif/retry-failed → 复活后立即再跑一轮
pub async fn retry_failed(State(state): State<AppState>) -> impl IntoResponse {
    let mut conn = state.db.lock().await;
    let revived = match exif_retry_failed(&conn) {
        Ok(n) => n,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let cancel = AtomicBool::new(false);
    let run = run_exif_queue_with_limit(&conn, &cancel, &mut |_, _, _, _| {}, 0);
    match run {
        Ok(report) => (
            StatusCode::OK,
            Json(json!({ "revived": revived, "processed": report.processed,
                         "done": report.done, "failed": report.failed,
                         "errors": report.errors })),
        )
            .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// GET /api/exif/status
pub async fn status(State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.db.lock().await;
    let st = match exif_status(&conn) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let cfg: core_config::ExifSettings =
        core_storage::settings::get_or_default(&conn, core_config::EXIF_SETTINGS_KEY)
            .unwrap_or_default();
    (
        StatusCode::OK,
        Json(json!({
            "jobDone": st.done,
            "jobFailed": st.failed,
            "jobPending": st.pending,
            "remaining": st.remaining,
            "lastError": st.last_error,
            "parseOnImport": cfg.parse_on_import,
            "gpsMode": format!("{:?}", cfg.gps).to_lowercase(),
            "gpsProviderConfigured": cfg.gps_provider.configured(),
        })),
    )
        .into_response()
}
