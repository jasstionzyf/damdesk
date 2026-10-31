#![allow(non_snake_case)] // 本模块 DTO 字段 = 前端 JSON camelCase 契约，勿改
//! W2 导入链路 API：目录扫描 / 批次启动 / 进度轮询 / 取消 / 断点续跑 / 历史批次。
//! 复用 svc-import（scan_dir + run/resume_batch + 幂等分类），导入跑在
//! spawn_blocking 线程、持独立 SQLite 连接（WAL 多连接安全，与 app-tauri 同构）。
//! 进度不上 SSE，前端轮询 `GET /api/import/status`（v1 单用户，1s 间隔足够）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::error_codes::{codes, err_coded, CodedError};
use crate::AppState;
use core_types::{BatchOutcome, ImportProgress};

/// 导入任务进程级状态（单一活动批次，与桌面版 v1 同语义）。
pub struct ImportState {
    pub running: AtomicBool,
    pub cancel: Arc<AtomicBool>,
    /// 最新进度快照（导入线程写，status 轮询读）
    pub progress: Arc<std::sync::Mutex<Option<ImportProgress>>>,
    /// 最近一次批次结果（含错误摘要）
    pub outcome: Arc<std::sync::Mutex<Option<BatchOutcome>>>,
}

impl ImportState {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(std::sync::Mutex::new(None)),
            outcome: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

impl Default for ImportState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
pub struct ScanReq {
    pub path: String,
}

#[derive(Deserialize)]
pub struct StartReq {
    pub path: String,
    #[serde(default = "default_store_mode")]
    pub storeMode: String,
    /// W6 修复（task）：向导「导入后自动打标」——批次完成后入队 + 尽力启动打标。
    #[serde(default)]
    pub autoTag: bool,
}

fn default_store_mode() -> String {
    "reference".into()
}

#[derive(Deserialize)]
pub struct ResumeReq {
    pub batchId: i64,
}

/// 「重试失败项」：只重跑前端列出的失败文件（不重扫整个目录）。
#[derive(Deserialize)]
pub struct RetryFailedReq {
    pub batchId: i64,
    /// 失败项的绝对路径（来自 `/api/import/status` 的 progress/outcome.failedItems）
    pub paths: Vec<String>,
}

fn err(status: StatusCode, msg: impl Into<String>) -> axum::response::Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

/// 校验目录：必须绝对路径且存在。
fn validate_dir(path: &str) -> Result<PathBuf, CodedError> {
    let p = PathBuf::from(path);
    if !p.is_absolute() {
        return Err(CodedError::bad_request(
            codes::IMPORT_PATH_NOT_ABSOLUTE,
            json!({ "path": path }),
            format!("path must be absolute: {path}"),
        ));
    }
    if !p.is_dir() {
        return Err(CodedError::bad_request(
            codes::IMPORT_PATH_NOT_DIRECTORY,
            json!({ "path": path }),
            format!("not a directory: {path}"),
        ));
    }
    Ok(p)
}

/// 库目录（copyin 目标）：settings 可配，默认 {data_dir}/library（与 app-tauri library_root 同语义）。
fn library_root(conn: &rusqlite::Connection, data_dir: &Path) -> PathBuf {
    let s: core_config::LibrarySettings =
        core_storage::settings::get_or_default(conn, core_config::LIBRARY_SETTINGS_KEY)
            .unwrap_or_default();
    s.effective_root(data_dir)
}

/// 导入向导第一步：扫描目录（不落库）。
pub async fn scan(State(_state): State<AppState>, Json(req): Json<ScanReq>) -> impl IntoResponse {
    let p = match validate_dir(&req.path) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let res = tokio::task::spawn_blocking(move || -> std::io::Result<serde_json::Value> {
        let files = svc_import::scan_dir(&p)?;
        let total_bytes: u64 = files
            .iter()
            .filter_map(|f| f.metadata().ok())
            .map(|m| m.len())
            .sum();
        let preview: Vec<String> = files
            .iter()
            .take(8)
            .map(|f| f.to_string_lossy().into_owned())
            .collect();
        Ok(json!({ "imageFiles": files.len(), "totalBytes": total_bytes, "preview": preview }))
    })
    .await;
    match res {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)).into_response(),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("scan: {e}")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}")),
    }
}

/// 启动导入批次（后台线程）：返回 batchId，进度轮询 /api/import/status。
pub async fn start(State(state): State<AppState>, Json(req): Json<StartReq>) -> impl IntoResponse {
    if state
        .import
        .running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return err_coded(
            StatusCode::CONFLICT,
            codes::IMPORT_ALREADY_RUNNING,
            json!({}),
            "an import batch is already running",
        );
    }
    if req.storeMode != "reference" && req.storeMode != "copyin" {
        state.import.running.store(false, Ordering::SeqCst);
        return CodedError::bad_request(
            codes::IMPORT_BAD_STORE_MODE,
            json!({ "storeMode": req.storeMode }),
            format!("invalid storeMode: {}", req.storeMode),
        )
        .into_response();
    }
    let p = match validate_dir(&req.path) {
        Ok(p) => p,
        Err(e) => {
            state.import.running.store(false, Ordering::SeqCst);
            return e.into_response();
        }
    };

    let st = state.clone();
    let res = tokio::task::spawn_blocking(move || -> Result<i64, String> {
        let db_path = st.data_dir.join("dam.db");
        let conn = core_storage::open(&db_path).map_err(|e| format!("open db: {e}"))?;
        let lib_root = library_root(&conn, &st.data_dir);
        if req.storeMode == "copyin" {
            std::fs::create_dir_all(&lib_root).map_err(|e| e.to_string())?;
            if p.starts_with(&lib_root) {
                return Err("source inside copy-in library dir".into());
            }
        }
        let files = svc_import::scan_dir(&p).map_err(|e| format!("scan: {e}"))?;
        if files.is_empty() {
            return Err(format!("no image files under {}", p.display()));
        }
        let batch_id = svc_import::create_batch(&conn, &p, &req.storeMode, files.len() as i64)
            .map_err(|e| format!("create batch: {e}"))?;
        svc_import::set_batch_auto_tag(&conn, batch_id, req.autoTag)
            .map_err(|e| format!("set autoTag: {e}"))?;
        Ok(batch_id)
    })
    .await;

    let batch_id = match res {
        Ok(Ok(id)) => id,
        Ok(Err(e)) => {
            state.import.running.store(false, Ordering::SeqCst);
            return err(StatusCode::BAD_REQUEST, e);
        }
        Err(e) => {
            state.import.running.store(false, Ordering::SeqCst);
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}"));
        }
    };

    spawn_import_job(state.clone(), batch_id, req.autoTag);
    (StatusCode::OK, Json(json!({ "batchId": batch_id }))).into_response()
}

/// 断点续跑（后台线程，进度同 start）。
pub async fn resume(
    State(state): State<AppState>,
    Json(req): Json<ResumeReq>,
) -> impl IntoResponse {
    if state
        .import
        .running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return err_coded(
            StatusCode::CONFLICT,
            codes::IMPORT_ALREADY_RUNNING,
            json!({}),
            "an import batch is already running",
        );
    }
    {
        let conn = state.db.lock().await;
        match svc_import::get_batch(&conn, req.batchId) {
            Ok(_) => {}
            Err(_) => {
                drop(conn);
                state.import.running.store(false, Ordering::SeqCst);
                return err_coded(
                    StatusCode::NOT_FOUND,
                    codes::IMPORT_BATCH_NOT_FOUND,
                    json!({ "batchId": req.batchId }),
                    format!("batch {} not found", req.batchId),
                );
            }
        }
    }
    let auto_tag = {
        let conn = state.db.lock().await;
        // 上面已确认批次存在；此处读 autoTag 标记（迁移 0010 落库，重启/续跑不丢）
        svc_import::get_batch_auto_tag(&conn, req.batchId)
    };
    spawn_import_job(state.clone(), req.batchId, auto_tag);
    (StatusCode::OK, Json(json!({ "batchId": req.batchId }))).into_response()
}

/// 重试失败项：只重跑传入路径（不重扫目录），进度口径 total = 待重试文件数。
pub async fn retry_failed(
    State(state): State<AppState>,
    Json(req): Json<RetryFailedReq>,
) -> impl IntoResponse {
    if state
        .import
        .running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return err_coded(
            StatusCode::CONFLICT,
            codes::IMPORT_ALREADY_RUNNING,
            json!({}),
            "an import batch is already running",
        );
    }
    let fail = |code: StatusCode, e: String| {
        state.import.running.store(false, Ordering::SeqCst);
        err(code, e)
    };
    if req.paths.is_empty() {
        state.import.running.store(false, Ordering::SeqCst);
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::IMPORT_PATHS_EMPTY,
            json!({}),
            "paths is empty",
        );
    }
    // 同步校验：批次存在 + 全部路径落在批次根内（越界立刻 400，不留「跑了一半才发现」）
    let root = {
        let conn = state.db.lock().await;
        match svc_import::get_batch(&conn, req.batchId) {
            Ok(b) => PathBuf::from(b.rootPath),
            Err(_) => {
                drop(conn);
                return fail(
                    StatusCode::NOT_FOUND,
                    format!("batch {} not found", req.batchId),
                );
            }
        }
    };
    for p in &req.paths {
        let pb = PathBuf::from(p);
        if !pb.is_absolute() || !pb.starts_with(&root) {
            return fail(
                StatusCode::BAD_REQUEST,
                format!("path outside batch root {}: {p}", root.display()),
            );
        }
    }
    let paths: Vec<PathBuf> = req.paths.iter().map(PathBuf::from).collect();
    let n = paths.len();
    spawn_retry_job(state.clone(), req.batchId, paths);
    (
        StatusCode::OK,
        Json(json!({ "batchId": req.batchId, "paths": n })),
    )
        .into_response()
}

/// 后台跑批次：独立连接 + resume_batch（幂等，Unchanged 快路径跳过已完成文件）。
/// 进度回调更新共享快照；取消旗置位时回调返回 Err → svc-import 落 cancelled。
fn spawn_import_job(state: AppState, batch_id: i64, auto_tag: bool) {
    let st = state.clone();
    let cancel = state.import.cancel.clone();
    let progress_slot = state.import.progress.clone();
    let outcome_slot = state.import.outcome.clone();
    state.import.cancel.store(false, Ordering::SeqCst);
    *progress_slot.lock().unwrap() = None;
    tokio::task::spawn_blocking(move || {
        let db_path = st.data_dir.join("dam.db");
        let conn = match core_storage::open(&db_path) {
            Ok(c) => c,
            Err(e) => {
                *outcome_slot.lock().unwrap() = Some(BatchOutcome {
                    batchId: batch_id,
                    status: "error".into(),
                    ..Default::default()
                });
                st.import.running.store(false, Ordering::SeqCst);
                eprintln!("dam-web import: open db failed: {e}");
                return;
            }
        };
        let copyin = library_root(&conn, &st.data_dir);
        let thumbs = st.thumbs_root.clone();
        // E5：批内 lastError 粘滞——单文件失败的错误文本不被后续成功 emit 冲掉
        // （任务中心/验收的错误汇总都读这个槽；批次结束由下次 start 清空）
        let last_err: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
        let emit = |p: &ImportProgress| -> Result<(), String> {
            if cancel.load(Ordering::SeqCst) {
                return Err("cancelled".into());
            }
            let sticky = {
                let mut le = last_err.lock().unwrap();
                if p.lastError.is_some() {
                    *le = p.lastError.clone();
                }
                le.clone()
            };
            *progress_slot.lock().unwrap() = Some(ImportProgress {
                lastError: sticky,
                ..p.clone()
            });
            Ok(())
        };
        let out = svc_import::resume_batch(&conn, batch_id, Some(copyin.as_path()), &thumbs, emit);
        match out {
            Ok(o) => {
                let imported = o.imported;
                *outcome_slot.lock().unwrap() = Some(o);
                // KI-2a：本批新增资产同时入两塔嵌入队列（语义塔 + 写死的判重塔），
                // 只是毫秒级 SQL 入队，不等嵌入；装配/消费在后台线程，失败只落日志。
                let queued = match crate::embed_api::enqueue_after_import(&conn, batch_id) {
                    Ok(q) => q,
                    Err(e) => {
                        eprintln!("dam-web import embed: batch {batch_id} enqueue failed: {e}");
                        svc_pipeline::ScopeEnqueue::default()
                    }
                };
                if queued.total() > 0 {
                    crate::embed_api::auto_start_after_import(&st, batch_id, queued);
                }
                // W6 修复（task）：autoTag 批次完成后入队 + 尽力启动打标。
                // 打标侧任何失败只落日志，绝不影响导入结果（outcome 已写 done）。
                if auto_tag && imported > 0 {
                    let queued = svc_pipeline::enq_untagged_review(&conn).unwrap_or_else(|e| {
                        eprintln!("dam-web import autoTag: enqueue failed: {e}");
                        0
                    });
                    if queued > 0 {
                        eprintln!(
                            "dam-web import autoTag: batch {batch_id} queued {queued} tag job(s)"
                        );
                        crate::tag_api::auto_start_after_import(&st, queued);
                    }
                }
            }
            Err(e) => {
                let _ = svc_import::finish_batch(&conn, batch_id, "error", Some(&e.to_string()));
                *outcome_slot.lock().unwrap() = Some(BatchOutcome {
                    batchId: batch_id,
                    status: "error".into(),
                    ..Default::default()
                });
                eprintln!("dam-web import: batch {batch_id} failed: {e}");
            }
        }
        st.import.running.store(false, Ordering::SeqCst);
    });
}

/// 后台跑「重试失败项」：独立连接 + svc_import::retry_failed（只处理传入路径）。
fn spawn_retry_job(state: AppState, batch_id: i64, paths: Vec<PathBuf>) {
    let st = state.clone();
    let cancel = state.import.cancel.clone();
    let progress_slot = state.import.progress.clone();
    let outcome_slot = state.import.outcome.clone();
    state.import.cancel.store(false, Ordering::SeqCst);
    *progress_slot.lock().unwrap() = None;
    tokio::task::spawn_blocking(move || {
        let db_path = st.data_dir.join("dam.db");
        let conn = match core_storage::open(&db_path) {
            Ok(c) => c,
            Err(e) => {
                *outcome_slot.lock().unwrap() = Some(BatchOutcome {
                    batchId: batch_id,
                    status: "error".into(),
                    ..Default::default()
                });
                st.import.running.store(false, Ordering::SeqCst);
                eprintln!("dam-web import retry: open db failed: {e}");
                return;
            }
        };
        let copyin = library_root(&conn, &st.data_dir);
        let thumbs = st.thumbs_root.clone();
        let emit = |p: &ImportProgress| -> Result<(), String> {
            if cancel.load(Ordering::SeqCst) {
                return Err("cancelled".into());
            }
            *progress_slot.lock().unwrap() = Some(p.clone());
            Ok(())
        };
        match svc_import::retry_failed(
            &conn,
            batch_id,
            &paths,
            Some(copyin.as_path()),
            &thumbs,
            emit,
        ) {
            Ok(o) => {
                *outcome_slot.lock().unwrap() = Some(o);
                // KI-2a：重试成功的新入库文件同样补齐两塔向量（与 start 同口径）
                match crate::embed_api::enqueue_after_import(&conn, batch_id) {
                    Ok(q) if q.total() > 0 => {
                        crate::embed_api::auto_start_after_import(&st, batch_id, q)
                    }
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!(
                            "dam-web import retry embed: batch {batch_id} enqueue failed: {e}"
                        )
                    }
                }
            }
            Err(e) => {
                // 整批级失败（批次不存在/路径越界/库错）：把每个待重试项都标成失败，
                // 任务中心照常能列出「哪几张、为什么」，不留空白失败。
                let msg = e.to_string();
                eprintln!("dam-web import retry: batch {batch_id} failed: {msg}");
                let failed_items = paths
                    .iter()
                    .map(|p| core_types::ImportFailure {
                        fileName: p
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| p.to_string_lossy().into_owned()),
                        path: p.to_string_lossy().into_owned(),
                        error: msg.clone(),
                    })
                    .collect::<Vec<_>>();
                *outcome_slot.lock().unwrap() = Some(BatchOutcome {
                    batchId: batch_id,
                    status: "error".into(),
                    total: paths.len() as i64,
                    failed: paths.len() as i64,
                    failedItems: failed_items,
                    ..Default::default()
                });
            }
        }
        st.import.running.store(false, Ordering::SeqCst);
    });
}

/// 取消当前导入批次（进度回调检查旗标 → status=cancelled）。
pub async fn cancel(State(state): State<AppState>) -> impl IntoResponse {
    if !state.import.running.load(Ordering::SeqCst) {
        return err_coded(
            StatusCode::CONFLICT,
            codes::IMPORT_NOT_RUNNING,
            json!({}),
            "no import batch running",
        );
    }
    state.import.cancel.store(true, Ordering::SeqCst);
    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
}

/// 进度轮询：{running, progress, outcome}。
pub async fn status(State(state): State<AppState>) -> impl IntoResponse {
    let running = state.import.running.load(Ordering::SeqCst);
    let progress = state.import.progress.lock().unwrap().clone();
    let outcome = state.import.outcome.lock().unwrap().clone();
    Json(json!({ "running": running, "progress": progress, "outcome": outcome }))
}

/// 历史批次列表（断点续跑入口）。
pub async fn batches(State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match svc_library::import_batches(&conn) {
        Ok(v) => (StatusCode::OK, Json(json!(v))).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
