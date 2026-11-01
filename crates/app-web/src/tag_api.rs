//! W4 打标 API：模板打标队列（dam-ai /v1/tagging 默认 + 可切 provider）+ 自定义打标任务。
//!
//! 复用 svc-pipeline：模板队列 `job_queue(kind='tag')`、自定义任务队列
//! `job_queue(kind='custom_tag', payload={"taskId":N})`，与桌面 D4/D10 同一套
//! 状态机（断点续跑 / 3 次重试 / 取消回 pending / limit 配额）。
//! app-web 只做：装配 provider（R7：TagProvider 统一接口，damai/openai_compat
//! 按 TagSettings.provider 分派）→ 入队 → 后台线程消费 → 进度快照轮询。
//! 模板与自定义两条队列独立运行（桌面 tag_running / custom_running 同语义）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::infer;
use crate::AppState;
use core_storage::custom_tasks as tasks;

/// 打标任务进程级状态：模板 / 自定义两条独立单槽（与桌面版 v1 同语义）。
pub struct TagState {
    pub template_running: AtomicBool,
    pub template_cancel: Arc<AtomicBool>,
    pub template_progress: Mutex<Option<core_types::TagProgress>>,
    pub template_report: Mutex<Option<core_types::TagRunReport>>,
    pub template_error: Mutex<Option<String>>,
    pub custom_running: AtomicBool,
    pub custom_cancel: Arc<AtomicBool>,
    pub custom_progress: Mutex<Option<core_types::TagProgress>>,
    pub custom_report: Mutex<Option<core_types::TagRunReport>>,
    pub custom_error: Mutex<Option<String>>,
}

impl TagState {
    pub fn new() -> Self {
        Self {
            template_running: AtomicBool::new(false),
            template_cancel: Arc::new(AtomicBool::new(false)),
            template_progress: Mutex::new(None),
            template_report: Mutex::new(None),
            template_error: Mutex::new(None),
            custom_running: AtomicBool::new(false),
            custom_cancel: Arc::new(AtomicBool::new(false)),
            custom_progress: Mutex::new(None),
            custom_report: Mutex::new(None),
            custom_error: Mutex::new(None),
        }
    }
}

impl Default for TagState {
    fn default() -> Self {
        Self::new()
    }
}

fn err(status: StatusCode, code: &str, msg: impl Into<String>) -> axum::response::Response {
    (status, Json(json!({ "error": msg.into(), "code": code }))).into_response()
}

#[derive(Debug, Deserialize)]
pub struct StartReq {
    /// pending（增量：无 auto 标签的图片，默认）| all（全库重打，人工数据不动）
    #[serde(default)]
    pub mode: Option<String>,
    /// 本轮最多处理 N 张（0/缺省 = 不限量）；剩余 job 留 pending 可续跑
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct TaskRunReq {
    /// all（全量重跑：清空该任务 job 重入队，默认）| resume（续跑现有 pending）
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct TaskUpsertReq {
    pub name: String,
    pub prompt: String,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct LimitQuery {
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct RetryQuery {
    /// `truncated` / `other`；缺省 = 全部失败项
    #[serde(default)]
    pub class: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PurgeQuery {
    #[serde(default)]
    pub purge: Option<bool>,
}

// ---------------------------------------------------------------- 模板打标

/// POST /api/tag/start：入队（仅新增 / 全库重打）+ 后台消费。
pub async fn start(State(state): State<AppState>, Json(req): Json<StartReq>) -> impl IntoResponse {
    let mode = req.mode.unwrap_or_else(|| "pending".into());
    let limit = req.limit.unwrap_or(0);
    if mode != "pending" && mode != "all" {
        return err(
            StatusCode::BAD_REQUEST,
            "bad_mode",
            format!("未知 mode: {mode}"),
        );
    }
    if state
        .tag
        .template_running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return err(StatusCode::CONFLICT, "already_running", "已有打标任务在跑");
    }
    // 装配 provider（失败立即回滚 running，把原因回给前端）
    let st = state.clone();
    let built = tokio::task::spawn_blocking(move || -> Result<infer::WebTagger, String> {
        let conn = core_storage::open(&infer::db_path(&st.data_dir)).map_err(|e| e.to_string())?;
        infer::build_tagger(&conn)
    })
    .await;
    let tagger = match built {
        Ok(Ok(t)) => t,
        Ok(Err(m)) => {
            state.tag.template_running.store(false, Ordering::SeqCst);
            *state.tag.template_error.lock().unwrap() = Some(m.clone());
            return err(StatusCode::BAD_GATEWAY, "provider_failed", m);
        }
        Err(e) => {
            state.tag.template_running.store(false, Ordering::SeqCst);
            return err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string());
        }
    };

    // 入队（主连接；快操作）
    let queued = {
        let conn = state.db.lock().await;
        let r = if mode == "all" {
            svc_pipeline::enq_all(&conn)
        } else {
            // E5 确认流：增量入队排除已有待确认提案的资产（不重复打标）
            svc_pipeline::enq_untagged_review(&conn)
        };
        match r {
            Ok(n) => n,
            Err(e) => {
                state.tag.template_running.store(false, Ordering::SeqCst);
                return err(StatusCode::INTERNAL_SERVER_ERROR, "enqueue", e.to_string());
            }
        }
    };

    *state.tag.template_progress.lock().unwrap() = Some(core_types::TagProgress {
        jobId: 0,
        assetId: 0,
        status: "queued".into(),
        index: 0,
        total: queued,
        doneCount: 0,
        failedCount: 0,
        error: None,
    });
    *state.tag.template_report.lock().unwrap() = None;
    *state.tag.template_error.lock().unwrap() = None;
    state.tag.template_cancel.store(false, Ordering::SeqCst);

    spawn_template_job(state.clone(), tagger, limit);

    (
        StatusCode::OK,
        Json(json!({ "ok": true, "queued": queued, "mode": mode, "limit": limit })),
    )
        .into_response()
}

/// 后台消费线程（独立连接；进度回调写 TagState）。
fn spawn_template_job(state: AppState, tagger: infer::WebTagger, limit: usize) {
    tokio::task::spawn_blocking(move || {
        let res = (|| -> Result<core_types::TagRunReport, String> {
            let conn =
                core_storage::open(&infer::db_path(&state.data_dir)).map_err(|e| e.to_string())?;
            let report = svc_pipeline::run_tag_queue_review_with_limit(
                &conn,
                tagger.provider.as_ref(),
                state.tag.template_cancel.as_ref(),
                &mut |p: core_types::TagProgress| {
                    // 逐图回调：覆盖快照（前端 1s 轮询取最新值）
                    *state.tag.template_progress.lock().unwrap() = Some(p);
                },
                limit,
            )
            .map_err(|e| e.to_string())?;
            Ok(report)
        })();

        match res {
            Ok(rep) => {
                *state.tag.template_report.lock().unwrap() = Some(rep);
            }
            Err(m) => {
                *state.tag.template_error.lock().unwrap() = Some(m.clone());
                *state.tag.template_report.lock().unwrap() = Some(core_types::TagRunReport {
                    errors: vec![m],
                    ..Default::default()
                });
            }
        }
        state.tag.template_cancel.store(false, Ordering::SeqCst);
        state.tag.template_running.store(false, Ordering::SeqCst);
    });
}

/// W6 修复（t_4834c167）：导入 autoTag 自动接线 —— 批次导入完成后由 import_api 调用。
/// 入队（enq_untagged_review）已由调用方完成；这里尽力启动模板打标消费，
/// 与 /api/tag/start 走同一条 run_tag_queue_review_with_limit 路径（E5 两段式：
/// 产出提案到「打标确认」页，不直接写标签）。
/// 尽力语义：provider 未配置 / 装配失败 / 已有打标任务在跑时静默让路（任务保持
/// pending，由打标任务页兜底手动启动），绝不因打标侧问题让导入报错。
pub fn auto_start_after_import(state: &AppState, queued: usize) {
    if queued == 0 {
        return;
    }
    if state
        .tag
        .template_running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        // 已有打标任务在跑：其 claim 循环跑到队列空，新入队 job 会被顺带消费
        eprintln!(
            "dam-web import autoTag: template run already active, {queued} job(s) left queued"
        );
        return;
    }
    let conn = match core_storage::open(&infer::db_path(&state.data_dir)) {
        Ok(c) => c,
        Err(e) => {
            state.tag.template_running.store(false, Ordering::SeqCst);
            eprintln!("dam-web import autoTag: open db failed: {e}");
            return;
        }
    };
    let tagger = match infer::build_tagger(&conn) {
        Ok(t) => t,
        Err(m) => {
            state.tag.template_running.store(false, Ordering::SeqCst);
            eprintln!("dam-web import autoTag: provider not started, jobs stay pending: {m}");
            return;
        }
    };

    // 与 start 端点同构的初始快照（前端任务中心看到 queued → running → report）
    *state.tag.template_progress.lock().unwrap() = Some(core_types::TagProgress {
        jobId: 0,
        assetId: 0,
        status: "queued".into(),
        index: 0,
        total: queued,
        doneCount: 0,
        failedCount: 0,
        error: None,
    });
    *state.tag.template_report.lock().unwrap() = None;
    *state.tag.template_error.lock().unwrap() = None;
    state.tag.template_cancel.store(false, Ordering::SeqCst);

    eprintln!("dam-web import autoTag: auto-starting template run ({queued} queued)");
    spawn_template_job(state.clone(), tagger, 0);
}

/// POST /api/tag/cancel：请求取消（已入队 job 回 pending 可续跑）。
pub async fn cancel(State(state): State<AppState>) -> impl IntoResponse {
    if !state.tag.template_running.load(Ordering::SeqCst) {
        return err(StatusCode::CONFLICT, "not_running", "没有运行中的打标任务");
    }
    state.tag.template_cancel.store(true, Ordering::SeqCst);
    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
}

/// 打标端配置摘要（status 响应里的 provider 字段）。
/// hasKey：api_key 是否已填——桌面默认 deepseek 端点 prefill 了 base_url/model，
/// configured() 单看这两项会误判「已配置」；UI 引导按 configured && hasKey 判定。
fn provider_info(conn: &rusqlite::Connection) -> serde_json::Value {
    let settings = infer::load_settings(conn);
    let tag = &settings.tag;
    json!({
        "enabled": tag.enabled,
        "configured": tag.configured(),
        "hasKey": !tag.api_key.trim().is_empty(),
        "provider": tag.provider,
        "baseUrl": tag.base_url,
        "model": tag.model,
        // 「当前生效 max_tokens」的可见面：默认值 / 设置页覆盖，UI 直接展示
        "maxTokens": tag.effective_max_tokens(),
        "maxTokensSource": tag.max_tokens_source(),
        "maxTokensLabel": tag.max_tokens_label(),
        "defaultMaxTokens": core_config::TagSettings::DEFAULT_MAX_TOKENS,
        "activeModel": if tag.configured() { infer::tag_model_key(&settings) } else { String::new() },
    })
}

/// GET /api/tag/status：模板 + 自定义两条队列状态 / 统计 / 按模型覆盖度。
pub async fn status(State(state): State<AppState>) -> impl IntoResponse {
    let st = state.clone();
    let res = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, String> {
        let conn = core_storage::open(&infer::db_path(&st.data_dir)).map_err(|e| e.to_string())?;
        let stats = svc_pipeline::tag_stats(&conn).map_err(|e| e.to_string())?;
        // 按模型覆盖度（origin='auto' 分组）——「标签按 tagModel 隔离」的可见面
        let mut stmt = conn
            .prepare(
                "SELECT COALESCE(model,''), COUNT(DISTINCT assetId) FROM tag
                 WHERE origin = 'auto' GROUP BY model ORDER BY 2 DESC",
            )
            .map_err(|e| e.to_string())?;
        let models: Vec<serde_json::Value> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                ))
            })
            .map_err(|e| e.to_string())?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|(model, assets)| json!({ "model": model, "assets": assets }))
            .collect();
        // 自定义任务数（跑过 / 未跑过概览）
        let custom_running_jobs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM job_queue WHERE kind = 'custom_tag' AND status IN ('pending','running')",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        // E5 确认流：待确认提案统计
        let (review_assets, review_rows) =
            svc_pipeline::review_stats(&conn).map_err(|e| e.to_string())?;
        Ok(json!({
            "template": {
                "running": st.tag.template_running.load(Ordering::SeqCst),
                "progress": st.tag.template_progress.lock().unwrap().clone(),
                "report": st.tag.template_report.lock().unwrap().clone(),
                "lastError": st.tag.template_error.lock().unwrap().clone(),
            },
            "custom": {
                "running": st.tag.custom_running.load(Ordering::SeqCst),
                "progress": st.tag.custom_progress.lock().unwrap().clone(),
                "report": st.tag.custom_report.lock().unwrap().clone(),
                "lastError": st.tag.custom_error.lock().unwrap().clone(),
                "pendingJobs": custom_running_jobs,
            },
            "review": { "assets": review_assets, "rows": review_rows },
            "stats": stats,
            "provider": provider_info(&conn),
            "models": models,
        }))
    })
    .await;
    match res {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)).into_response(),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, "status", e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
    }
}

/// GET /api/tag/failed：模板队列失败明细。
pub async fn failed(
    State(state): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match svc_pipeline::tag_failed_list(&conn, q.limit.unwrap_or(100)) {
        Ok(items) => (StatusCode::OK, Json(json!({ "items": items }))).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "failed", e.to_string()),
    }
}

/// POST /api/tag/retry-failed?class=truncated|other（缺省=全部）：
/// 失败 job 重置 pending，**并直接续跑消费**（不用再手动「增量打标」，
/// 也不用重建整队列 —— 卡 t_9fe8501e）。
pub async fn retry_failed(
    State(state): State<AppState>,
    Query(q): Query<RetryQuery>,
) -> impl IntoResponse {
    let class = q
        .class
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| c.to_string());
    let reset = {
        let conn = state.db.lock().await;
        match svc_pipeline::retry_failed_class(&conn, class.as_deref()) {
            Ok(n) => n,
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "retry", e.to_string()),
        }
    };

    // 重置成功后立刻起一轮消费（provider 未配置/已有任务在跑 → 保持 pending 等下一轮）
    let mut started = false;
    if reset > 0 {
        let st = state.clone();
        let built = tokio::task::spawn_blocking(move || -> Result<infer::WebTagger, String> {
            let conn =
                core_storage::open(&infer::db_path(&st.data_dir)).map_err(|e| e.to_string())?;
            infer::build_tagger(&conn)
        })
        .await;
        if let Ok(Ok(tagger)) = built {
            if state
                .tag
                .template_running
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                *state.tag.template_progress.lock().unwrap() = Some(core_types::TagProgress {
                    jobId: 0,
                    assetId: 0,
                    status: "queued".into(),
                    index: 0,
                    total: reset,
                    doneCount: 0,
                    failedCount: 0,
                    error: None,
                });
                *state.tag.template_report.lock().unwrap() = None;
                *state.tag.template_error.lock().unwrap() = None;
                state.tag.template_cancel.store(false, Ordering::SeqCst);
                spawn_template_job(state.clone(), tagger, 0);
                started = true;
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "reset": reset,
            "class": class,
            "started": started,
        })),
    )
        .into_response()
}

// ------------------------------------------------------------ 自定义任务

/// GET /api/tag/tasks：任务列表（含每任务队列计数与最近执行时间）。
pub async fn task_list(State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match tasks::list_tasks(&conn) {
        Ok(items) => (StatusCode::OK, Json(json!({ "items": items }))).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "list", e.to_string()),
    }
}

/// POST /api/tag/tasks：建任务（name 库内唯一）。返回 {id}。
pub async fn task_create(
    State(state): State<AppState>,
    Json(req): Json<TaskUpsertReq>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match tasks::create_task(&conn, &req.name, &req.prompt, req.enabled.unwrap_or(true)) {
        Ok(id) => (StatusCode::OK, Json(json!({ "ok": true, "id": id }))).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, "create", e.to_string()),
    }
}

/// PUT /api/tag/tasks/{id}：改任务（改名联动历史 custom 标签；撞人工同名保留人工）。
pub async fn task_update(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<TaskUpsertReq>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match tasks::update_task(
        &conn,
        id,
        &req.name,
        &req.prompt,
        req.enabled.unwrap_or(true),
    ) {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, "update", e.to_string()),
    }
}

/// DELETE /api/tag/tasks/{id}?purge=true：删任务（purge 连历史 custom 标签清理）。
pub async fn task_delete(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<PurgeQuery>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match tasks::delete_task(&conn, id, q.purge.unwrap_or(false)) {
        Ok((n_task, n_tag)) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "deleted": n_task, "purgedTags": n_tag })),
        )
            .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "delete", e.to_string()),
    }
}

/// POST /api/tag/tasks/{id}/run：全量重跑 / 续跑该任务。
pub async fn task_run(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<TaskRunReq>,
) -> impl IntoResponse {
    let mode = req.mode.unwrap_or_else(|| "all".into());
    let limit = req.limit.unwrap_or(0);
    if mode != "all" && mode != "resume" {
        return err(
            StatusCode::BAD_REQUEST,
            "bad_mode",
            format!("未知 mode: {mode}"),
        );
    }
    if state
        .tag
        .custom_running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return err(
            StatusCode::CONFLICT,
            "already_running",
            "已有自定义打标在跑",
        );
    }
    let st = state.clone();
    let built = tokio::task::spawn_blocking(move || -> Result<infer::WebTagger, String> {
        let conn = core_storage::open(&infer::db_path(&st.data_dir)).map_err(|e| e.to_string())?;
        infer::build_tagger(&conn)
    })
    .await;
    let tagger = match built {
        Ok(Ok(t)) => t,
        Ok(Err(m)) => {
            state.tag.custom_running.store(false, Ordering::SeqCst);
            *state.tag.custom_error.lock().unwrap() = Some(m.clone());
            return err(StatusCode::BAD_GATEWAY, "provider_failed", m);
        }
        Err(e) => {
            state.tag.custom_running.store(false, Ordering::SeqCst);
            return err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string());
        }
    };

    // 入队（all = 清空该任务 job 重入队；resume = 直接消费现有 pending）
    let queued = if mode == "all" {
        let conn = state.db.lock().await;
        match tasks::enqueue_task_all(&conn, id) {
            Ok(n) => n,
            Err(e) => {
                state.tag.custom_running.store(false, Ordering::SeqCst);
                return err(StatusCode::BAD_REQUEST, "enqueue", e.to_string());
            }
        }
    } else {
        0
    };

    *state.tag.custom_progress.lock().unwrap() = Some(core_types::TagProgress {
        jobId: 0,
        assetId: 0,
        status: "queued".into(),
        index: 0,
        total: queued,
        doneCount: 0,
        failedCount: 0,
        error: None,
    });
    *state.tag.custom_report.lock().unwrap() = None;
    *state.tag.custom_error.lock().unwrap() = None;
    state.tag.custom_cancel.store(false, Ordering::SeqCst);

    spawn_custom_job(state.clone(), tagger, limit);

    (
        StatusCode::OK,
        Json(json!({ "ok": true, "queued": queued, "mode": mode, "limit": limit })),
    )
        .into_response()
}

/// 后台消费线程（独立连接；进度回调写 TagState.custom 槽）。
fn spawn_custom_job(state: AppState, tagger: infer::WebTagger, limit: usize) {
    tokio::task::spawn_blocking(move || {
        let res = (|| -> Result<core_types::TagRunReport, String> {
            let conn =
                core_storage::open(&infer::db_path(&state.data_dir)).map_err(|e| e.to_string())?;
            let report = svc_pipeline::run_custom_queue_review_with_limit(
                &conn,
                tagger.provider.as_ref(),
                state.tag.custom_cancel.as_ref(),
                &mut |p: core_types::TagProgress| {
                    *state.tag.custom_progress.lock().unwrap() = Some(p);
                },
                limit,
            )
            .map_err(|e| e.to_string())?;
            Ok(report)
        })();

        match res {
            Ok(rep) => {
                *state.tag.custom_report.lock().unwrap() = Some(rep);
            }
            Err(m) => {
                *state.tag.custom_error.lock().unwrap() = Some(m.clone());
                *state.tag.custom_report.lock().unwrap() = Some(core_types::TagRunReport {
                    errors: vec![m],
                    ..Default::default()
                });
            }
        }
        state.tag.custom_cancel.store(false, Ordering::SeqCst);
        state.tag.custom_running.store(false, Ordering::SeqCst);
    });
}

/// POST /api/tag/custom/cancel：取消当前自定义打标批次。
pub async fn custom_cancel(State(state): State<AppState>) -> impl IntoResponse {
    if !state.tag.custom_running.load(Ordering::SeqCst) {
        return err(
            StatusCode::CONFLICT,
            "not_running",
            "没有运行中的自定义打标任务",
        );
    }
    state.tag.custom_cancel.store(true, Ordering::SeqCst);
    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
}

/// GET /api/tag/custom/failed：自定义队列失败明细（带任务名）。
pub async fn custom_failed(
    State(state): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match svc_pipeline::custom_failed_list(&conn, q.limit.unwrap_or(100)) {
        Ok(items) => (StatusCode::OK, Json(json!({ "items": items }))).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "failed", e.to_string()),
    }
}

/// POST /api/tag/custom/retry-failed：失败 job 重置 pending。
pub async fn custom_retry_failed(State(state): State<AppState>) -> impl IntoResponse {
    let reset = {
        let conn = state.db.lock().await;
        match tasks::retry_failed(&conn) {
            Ok(n) => n,
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "retry", e.to_string()),
        }
    };
    // 同模板队列：重置后直接续跑（不再要求用户再点一次「运行」）
    let mut started = false;
    if reset > 0 {
        let st = state.clone();
        let built = tokio::task::spawn_blocking(move || -> Result<infer::WebTagger, String> {
            let conn =
                core_storage::open(&infer::db_path(&st.data_dir)).map_err(|e| e.to_string())?;
            infer::build_tagger(&conn)
        })
        .await;
        if let Ok(Ok(tagger)) = built {
            if state
                .tag
                .custom_running
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                *state.tag.custom_progress.lock().unwrap() = Some(core_types::TagProgress {
                    jobId: 0,
                    assetId: 0,
                    status: "queued".into(),
                    index: 0,
                    total: reset,
                    doneCount: 0,
                    failedCount: 0,
                    error: None,
                });
                *state.tag.custom_report.lock().unwrap() = None;
                *state.tag.custom_error.lock().unwrap() = None;
                state.tag.custom_cancel.store(false, Ordering::SeqCst);
                spawn_custom_job(state.clone(), tagger, 0);
                started = true;
            }
        }
    }
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "reset": reset, "started": started })),
    )
        .into_response()
}

// ---------------------------------------------------------------- E5 确认流（打标结果两段式）

/// GET /api/tag/review?limit&offset —— 待确认提案（按资产分组）。
#[derive(Debug, Deserialize)]
pub struct ReviewQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub async fn review_list(
    State(state): State<AppState>,
    Query(q): Query<ReviewQuery>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match svc_pipeline::review_items(
        &conn,
        q.limit.unwrap_or(200).clamp(1, 2000),
        q.offset.unwrap_or(0),
    ) {
        Ok((total, items)) => (
            StatusCode::OK,
            Json(json!({ "total": total, "items": items })),
        )
            .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "review", e.to_string()),
    }
}

/// PUT /api/tag/review/items/{asset_id} —— 单资产修正（删/改/增提案；改动置 edited）。
pub async fn review_update(
    State(state): State<AppState>,
    axum::extract::Path(asset_id): axum::extract::Path<i64>,
    Json(req): Json<svc_pipeline::ReviewUpdateReq>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match svc_pipeline::review_update(&conn, asset_id, &req) {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, "review_update", e.to_string()),
    }
}

/// POST /api/tag/review/commit 请求体：assetIds 或 all。
#[derive(Debug, Deserialize)]
pub struct ReviewSelection {
    #[serde(default)]
    pub assetIds: Vec<i64>,
    #[serde(default)]
    pub all: bool,
}

fn validate_selection(req: &ReviewSelection) -> Result<(), axum::response::Response> {
    if !req.all && req.assetIds.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "empty_selection",
            "assetIds 不能为空（或 all=true）",
        ));
    }
    Ok(())
}

/// POST /api/tag/review/commit —— 确认入库（edited 行按人工数据落库）。
pub async fn review_commit(
    State(state): State<AppState>,
    Json(req): Json<ReviewSelection>,
) -> impl IntoResponse {
    if let Err(e) = validate_selection(&req) {
        return e;
    }
    let conn = state.db.lock().await;
    match svc_pipeline::review_commit(&conn, &req.assetIds, req.all) {
        Ok(rep) => (StatusCode::OK, Json(json!({ "ok": true, "report": rep }))).into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "review_commit",
            e.to_string(),
        ),
    }
}

/// POST /api/tag/review/discard —— 跳过（清提案，原图零改动）。
pub async fn review_discard(
    State(state): State<AppState>,
    Json(req): Json<ReviewSelection>,
) -> impl IntoResponse {
    if let Err(e) = validate_selection(&req) {
        return e;
    }
    let conn = state.db.lock().await;
    match svc_pipeline::review_discard(&conn, &req.assetIds, req.all) {
        Ok(n) => (StatusCode::OK, Json(json!({ "ok": true, "removed": n }))).into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "review_discard",
            e.to_string(),
        ),
    }
}

/// W4 打标路由表（lib.rs 挂 /api/tag 前缀）。
pub fn routes() -> axum::Router<AppState> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/start", post(start))
        .route("/cancel", post(cancel))
        .route("/status", get(status))
        .route("/failed", get(failed))
        .route("/retry-failed", post(retry_failed))
        .route("/tasks", get(task_list).post(task_create))
        .route(
            "/tasks/{id}",
            axum::routing::put(task_update).delete(task_delete),
        )
        .route("/tasks/{id}/run", post(task_run))
        .route("/custom/cancel", post(custom_cancel))
        .route("/custom/failed", get(custom_failed))
        .route("/custom/retry-failed", post(custom_retry_failed))
        // E5 确认流
        .route("/review", get(review_list))
        .route(
            "/review/items/{asset_id}",
            axum::routing::put(review_update),
        )
        .route("/review/commit", post(review_commit))
        .route("/review/discard", post(review_discard))
}
