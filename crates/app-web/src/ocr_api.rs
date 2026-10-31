//! OCR-E 文字识别 API：批量识别队列（PP-OCRv6_medium 本地 ONNX）。
//!
//! 复用 svc-pipeline::ocr_queue（`job_queue(kind='ocr')`，与打标/嵌入同一套状态机：
//! 断点续跑 / 3 次重试 / 取消回 pending / limit 配额）。app-web 只做：模型就绪预检
//! （engine-ocr 三件在盘）→ 入队 → 后台线程消费 → 进度快照轮询。
//! web 容器（无 local-infer）：start/retry 返回 409 `ocr_not_built`，status/failed/stats
//! 照常可查（队列数据可见，执行能力桌面专属）。
//!
//! 检索融合不在本文件：assets.ocr_text 的加权融合在 svc-library::fuse_ocr_text，
//! 召回走 core-storage::ocr_index 的 FTS5 倒排（OCR-MATCH，卡 task）。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::error_codes::{codes, err_coded};
use crate::{infer, AppState};

/// OCR 队列进程级状态：单槽（同一时刻一轮；与 tag 模板队列同语义）。
pub struct OcrState {
    pub running: AtomicBool,
    pub cancel: Arc<AtomicBool>,
    pub progress: Mutex<Option<core_types::OcrProgress>>,
    pub report: Mutex<Option<core_types::OcrRunReport>>,
    pub error: Mutex<Option<String>>,
    /// 本轮目标 job 数（进度条分母；0 = 未知）
    pub total: AtomicUsize,
}

impl OcrState {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Mutex::new(None),
            report: Mutex::new(None),
            error: Mutex::new(None),
            total: AtomicUsize::new(0),
        }
    }
}

impl Default for OcrState {
    fn default() -> Self {
        Self::new()
    }
}

fn err(status: StatusCode, code: &str, msg: impl Into<String>) -> axum::response::Response {
    (status, Json(json!({ "error": msg.into(), "code": code }))).into_response()
}

#[derive(Debug, Deserialize)]
pub struct StartReq {
    /// pending（增量：无 ocr_text 的图片，默认）| all（全库重跑，覆盖已有文本）
    #[serde(default)]
    pub mode: Option<String>,
    /// 本轮最多处理 N 张（0/缺省 = 不限量）；剩余 job 留 pending 可续跑
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct LimitQuery {
    #[serde(default)]
    pub limit: Option<i64>,
}

/// 三件（det / rec / dict）的落盘绝对路径：`<models_root>/models/<model>/<file>`。
#[cfg(feature = "local-infer")]
fn model_file_paths() -> Vec<std::path::PathBuf> {
    let Ok(root) = engine_ocr::models_root() else {
        return Vec::new();
    };
    let base = core_config::models_root(&root);
    [
        (engine_ocr::DET_MODEL, engine_ocr::DET_FILE),
        (engine_ocr::REC_MODEL, engine_ocr::REC_FILE),
        (engine_ocr::REC_MODEL, engine_ocr::DICT_FILE),
    ]
    .iter()
    .map(|(m, f)| base.join(m).join(f))
    .collect()
}

/// 未就绪时缺哪几个文件（响亮给出绝对路径，用户不需猜目录结构）。
#[cfg(feature = "local-infer")]
fn missing_model_files() -> Vec<String> {
    model_file_paths()
        .into_iter()
        .filter(|p| !p.is_file())
        .map(|p| p.display().to_string())
        .collect()
}

#[cfg(feature = "local-infer")]
fn models_ready_flag() -> bool {
    engine_ocr::models_ready()
}

#[cfg(not(feature = "local-infer"))]
fn models_ready_flag() -> bool {
    false
}

#[cfg(not(feature = "local-infer"))]
fn missing_model_files() -> Vec<String> {
    Vec::new()
}

/// 409 文案（纯函数，便于单测覆盖；用户口径：必须给绝对路径 + 缺失清单 + 放好后直接重试）。
#[cfg(feature = "local-infer")]
fn missing_models_message(missing: &[String], models_dir: &str) -> String {
    format!(
        "OCR 模型未就绪（PP-OCRv6_medium，Apache-2.0）：缺 {} 个文件，共约 132MB。\
         放置根目录 {models_dir}，det 一件 inference.onnx 放 ppocrv6-medium-det/，\
         rec 的 inference.onnx + ppocrv6_dict.txt 放 ppocrv6-medium-rec/；\
         放好后直接重试，无需重启。缺失清单：{}",
        missing.len(),
        missing.join("；")
    )
}

/// 本机是否具备 OCR 执行能力（local-infer 构建 + det/rec/dict 三件在盘）。
#[cfg(feature = "local-infer")]
fn capability() -> Result<(), axum::response::Response> {
    if !engine_ocr::models_ready() {
        let missing = missing_model_files();
        let dir = model_file_paths()
            .first()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<模型根>/models".into());
        return Err(err(
            StatusCode::CONFLICT,
            "ocr_models_missing",
            missing_models_message(&missing, &dir),
        ));
    }
    Ok(())
}

#[cfg(not(feature = "local-infer"))]
fn capability() -> Result<(), axum::response::Response> {
    Err(err(
        StatusCode::CONFLICT,
        "ocr_not_built",
        "web 容器构建不含本地 OCR 运行时（local-infer off）；文字识别为桌面专属能力",
    ))
}

/// POST /api/ocr/start：入队（仅新增 / 全库重跑）+ 后台消费。
pub async fn start(State(state): State<AppState>, Json(req): Json<StartReq>) -> axum::response::Response {
    let mode = req.mode.unwrap_or_else(|| "pending".into());
    let limit = req.limit.unwrap_or(0);
    if mode != "pending" && mode != "all" {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::BAD_MODE,
            json!({ "mode": mode }),
            format!("未知 mode: {mode}"),
        );
    }
    if state
        .ocr
        .running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return err(StatusCode::CONFLICT, "already_running", "OCR 队列已在运行");
    }
    if let Err(resp) = capability() {
        state.ocr.running.store(false, Ordering::SeqCst);
        return resp;
    }
    let data_dir = state.data_dir.clone();
    let cancel = state.ocr.cancel.clone();
    cancel.store(false, Ordering::SeqCst);
    *state.ocr.error.lock().unwrap() = None;
    *state.ocr.report.lock().unwrap() = None;
    *state.ocr.progress.lock().unwrap() = None;

    let all = mode == "all";
    let enqueue_res = tokio::task::spawn_blocking(move || -> Result<usize, String> {
        let conn = core_storage::open(&infer::db_path(&data_dir)).map_err(|e| e.to_string())?;
        if all {
            svc_pipeline::ocr_enq_all(&conn)
        } else {
            svc_pipeline::ocr_enq_untagged(&conn)
        }
        .map_err(|e| e.to_string())
    })
    .await;
    let enqueued = match enqueue_res {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => {
            state.ocr.running.store(false, Ordering::SeqCst);
            return err(StatusCode::INTERNAL_SERVER_ERROR, "enqueue_failed", e);
        }
        Err(e) => {
            state.ocr.running.store(false, Ordering::SeqCst);
            return err(StatusCode::INTERNAL_SERVER_ERROR, "enqueue_failed", e.to_string());
        }
    };
    spawn_ocr_job(state.clone(), limit);
    Json(json!({ "enqueued": enqueued, "mode": mode, "limit": limit })).into_response()
}

fn spawn_ocr_job(state: AppState, limit: usize) {
    let slot = state.ocr.clone();
    let cancel = slot.cancel.clone();
    let data_dir = state.data_dir.clone();
    tokio::task::spawn_blocking(move || {
        #[cfg(feature = "local-infer")]
        {
            let opened = core_storage::open(&infer::db_path(&data_dir));
            let mut last_progress = None;
            let res = (|| -> Result<core_types::OcrRunReport, String> {
                let conn = opened.map_err(|e| e.to_string())?;
                svc_pipeline::run_ocr_queue_with_limit(&conn, &cancel, &mut |p: core_types::OcrProgress| {
                    slot.total.store(p.total, Ordering::SeqCst);
                    last_progress = Some(p);
                }, limit)
                .map_err(|e| e.to_string())
            })();
            match res {
                Ok(report) => {
                    *slot.report.lock().unwrap() = Some(report);
                }
                Err(e) => {
                    *slot.error.lock().unwrap() = Some(e.to_string());
                }
            }
            if let Some(p) = last_progress {
                *slot.progress.lock().unwrap() = Some(p);
            }
        }
        #[cfg(not(feature = "local-infer"))]
        {
            // web 构建：capability() 已在 start() 拦截，此分支仅兜底（不静默假跑）
            let _ = (&data_dir, &cancel, &limit);
            *slot.error.lock().unwrap() = Some("ocr_not_built: web 容器无本地 OCR 运行时".into());
        }
        slot.running.store(false, Ordering::SeqCst);
    });
}

/// POST /api/ocr/cancel：置取消旗标（本轮处理完当前 job 即停；已 claim 未处理回 pending）。
pub async fn cancel(State(state): State<AppState>) -> impl IntoResponse {
    state.ocr.cancel.store(true, Ordering::SeqCst);
    Json(json!({ "cancelling": true }))
}

/// GET /api/ocr/status：队列统计 + 进度快照 + 最近一轮汇总。
pub async fn status(State(state): State<AppState>) -> axum::response::Response {
    let data_dir = state.data_dir.clone();
    let res = tokio::task::spawn_blocking(move || -> Result<core_types::OcrQueueStats, String> {
        let conn = core_storage::open(&infer::db_path(&data_dir)).map_err(|e| e.to_string())?;
        svc_pipeline::ocr_stats(&conn).map_err(|e| e.to_string())
    })
    .await;
    match res {
        Ok(Ok(stats)) => Json(json!({
            "running": state.ocr.running.load(Ordering::SeqCst),
            "stats": stats,
            "progress": *state.ocr.progress.lock().unwrap(),
            "report": *state.ocr.report.lock().unwrap(),
            "error": *state.ocr.error.lock().unwrap(),
            // 常驻可见：模型是否就绪 + 缺哪几个文件（不是等用户点「识别」才报 409）
            "modelsReady": models_ready_flag(),
            "missingModelFiles": missing_model_files(),
        }))
        .into_response(),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, "stats_failed", e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join_failed", e.to_string()),
    }
}

/// GET /api/ocr/failed?limit=50：失败 job 明细。
pub async fn failed(
    State(state): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> axum::response::Response {
    let data_dir = state.data_dir.clone();
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let res = tokio::task::spawn_blocking(move || -> Result<Vec<core_types::OcrFailedItem>, String> {
        let conn = core_storage::open(&infer::db_path(&data_dir)).map_err(|e| e.to_string())?;
        svc_pipeline::ocr_failed_list(&conn, limit).map_err(|e| e.to_string())
    })
    .await;
    match res {
        Ok(Ok(items)) => Json(json!({ "items": items })).into_response(),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, "list_failed", e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join_failed", e.to_string()),
    }
}

/// POST /api/ocr/retry-failed：失败 job 回 pending + 后台消费。
pub async fn retry_failed(State(state): State<AppState>) -> axum::response::Response {
    if state
        .ocr
        .running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return err(StatusCode::CONFLICT, "already_running", "OCR 队列已在运行");
    }
    if let Err(resp) = capability() {
        state.ocr.running.store(false, Ordering::SeqCst);
        return resp;
    }
    let data_dir = state.data_dir.clone();
    *state.ocr.error.lock().unwrap() = None;
    *state.ocr.report.lock().unwrap() = None;
    let reset = tokio::task::spawn_blocking(move || -> Result<usize, String> {
        let conn = core_storage::open(&infer::db_path(&data_dir)).map_err(|e| e.to_string())?;
        svc_pipeline::ocr_retry_failed(&conn).map_err(|e| e.to_string())
    })
    .await;
    match reset {
        Ok(Ok(n)) => {
            spawn_ocr_job(state.clone(), 0);
            Json(json!({ "requeued": n })).into_response()
        }
        Ok(Err(e)) => {
            state.ocr.running.store(false, Ordering::SeqCst);
            err(StatusCode::INTERNAL_SERVER_ERROR, "reset_failed", e)
        }
        Err(e) => {
            state.ocr.running.store(false, Ordering::SeqCst);
            err(StatusCode::INTERNAL_SERVER_ERROR, "join_failed", e.to_string())
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct AssetQuery {
    pub assetId: i64,
}

/// GET /api/ocr/asset?assetId=N：单资产识别文本（详情面板展示）。
pub async fn asset_text(
    State(state): State<AppState>,
    Query(q): Query<AssetQuery>,
) -> axum::response::Response {
    let data_dir = state.data_dir.clone();
    let asset_id = q.assetId;
    let res = tokio::task::spawn_blocking(move || -> Result<String, String> {
        let conn = core_storage::open(&infer::db_path(&data_dir)).map_err(|e| e.to_string())?;
        conn.query_row(
            "SELECT ocr_text FROM assets WHERE assetId = ?1",
            [asset_id],
            |r| r.get::<_, String>(0),
        )
        .map_err(|e| e.to_string())
    })
    .await;
    match res {
        Ok(Ok(text)) => Json(json!({ "assetId": asset_id, "ocrText": text })).into_response(),
        Ok(Err(_)) => err(StatusCode::NOT_FOUND, "asset_not_found", format!("asset {asset_id} 不存在")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join_failed", e.to_string()),
    }
}

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/start", axum::routing::post(start))
        .route("/cancel", axum::routing::post(cancel))
        .route("/status", axum::routing::get(status))
        .route("/failed", axum::routing::get(failed))
        .route("/retry-failed", axum::routing::post(retry_failed))
        .route("/asset", axum::routing::get(asset_text))
}

#[cfg(all(test, feature = "local-infer"))]
mod tests {
    use super::*;

    /// 模型根是进程级全局态 → 同一测试二进制内用 core-config 的互斥锁串行化。
    fn with_models_root<T>(root: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _g = core_config::models_root_lock();
        engine_ocr::set_models_root(root);
        let out = f();
        engine_ocr::clear_models_root();
        out
    }

    #[test]
    fn model_file_paths_follow_models_dir_contract() {
        let root = std::path::Path::new("/tmp/dt-ocr-root");
        let paths = with_models_root(root, model_file_paths);
        let got: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        assert_eq!(got.len(), 3);
        assert!(
            got[0].ends_with("/tmp/dt-ocr-root/models/ppocrv6-medium-det/inference.onnx"),
            "{got:?}"
        );
        assert!(
            got[1].ends_with("/tmp/dt-ocr-root/models/ppocrv6-medium-rec/inference.onnx"),
            "{got:?}"
        );
        assert!(
            got[2].ends_with("/tmp/dt-ocr-root/models/ppocrv6-medium-rec/ppocrv6_dict.txt"),
            "{got:?}"
        );
    }

    #[test]
    fn missing_lists_only_absent_files() {
        let dir = std::env::temp_dir().join(format!("dt-ocr-missing-{}", std::process::id()));
        let det = dir.join("models/ppocrv6-medium-det");
        let rec = dir.join("models/ppocrv6-medium-rec");
        std::fs::create_dir_all(&det).unwrap();
        std::fs::create_dir_all(&rec).unwrap();
        std::fs::write(det.join("inference.onnx"), b"x").unwrap();

        let missing = with_models_root(&dir, missing_model_files);
        assert_eq!(missing.len(), 2, "rec 两件应缺失: {missing:?}");
        assert!(missing.iter().all(|p| p.contains("ppocrv6-medium-rec")));
        assert!(!with_models_root(&dir, models_ready_flag));

        // 补齐 rec 两件 → 就绪，缺失清单为空
        std::fs::write(rec.join("inference.onnx"), b"x").unwrap();
        std::fs::write(rec.join("ppocrv6_dict.txt"), b"x").unwrap();
        assert!(with_models_root(&dir, models_ready_flag));
        assert!(with_models_root(&dir, missing_model_files).is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn message_carries_paths_count_and_retry_hint() {
        let missing = vec![
            "C:/root/models/ppocrv6-medium-det/inference.onnx".to_string(),
            "C:/root/models/ppocrv6-medium-rec/ppocrv6_dict.txt".to_string(),
        ];
        let msg = missing_models_message(&missing, "C:/root/models");
        assert!(msg.contains("缺 2 个文件"), "{msg}");
        assert!(msg.contains("C:/root/models"), "{msg}");
        assert!(msg.contains("ppocrv6-medium-det"), "{msg}");
        assert!(msg.contains("ppocrv6-medium-rec"), "{msg}");
        assert!(msg.contains("无需重启"), "{msg}");
        assert!(msg.contains("Apache-2.0"), "{msg}");
        assert!(msg.contains(&missing[0]), "{msg}");
        assert!(msg.contains(&missing[1]), "{msg}");
    }
}
