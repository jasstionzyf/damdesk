//! W3 嵌入队列 API：双塔向量索引构建（语义塔增量/全库重嵌 + 判重塔入库即算/存量 backfill）
//! + 进度轮询 + 取消 + 失败重试。
//!
//! 复用 svc-pipeline 的嵌入队列（job_queue kind='embed:<model>' 驱动、向量落
//! core_storage::vecstore 的 v_<model> 分表、3 次重试、幂等入队）。app-web 只做：
//! 装配 provider（R6：远程 HTTP 实现 + 构建内编入的本地 ONNX）→ 入队 → 后台线程消费 → 进度快照。
//! 进度不上 SSE，前端 1s 轮询 `GET /api/embed/status`（v1 单用户，与导入同构）。
//!
//! **双塔口径（KI-2a）**
//! - 语义塔 = `active_embedder`（用户可选，检索用）；判重塔 = `core_config::DEDUP_EMBEDDER`
//!   写死（去重塔不可选，KI-2 裁决），只进 `dedup_vectors` / `dedup_status`。
//! - 两塔状态机独立（`emb_status` vs `dedup_status`），一个塔装配/运行失败不影响另一个：
//!   单塔失败写该塔 report + `lastError`，另一塔照常推进。
//! - 导入钩子（`enqueue_after_import` / `auto_start_after_import`）让新入库资产**不点按钮**
//!   就补齐去重向量；导入只做毫秒级入队，不等嵌入。

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::infer;
use crate::AppState;
use engine_embed::EmbedProvider;
use svc_pipeline::{EmbedScope, ScopeEnqueue};

/// 嵌入任务进程级状态（单一活动任务，与导入/桌面版 v1 同语义）。
pub struct EmbedState {
    pub running: AtomicBool,
    pub cancel: Arc<AtomicBool>,
    /// 本轮范围：semantic / dedup / all / import（任务中心展示用）
    pub scope: Mutex<Option<String>>,
    /// 逐图进度快照（消费线程写，status 轮询读；双塔共用一槽，进度项自带 scope）
    pub progress: Mutex<Option<core_types::EmbedProgress>>,
    /// 最近完成的一轮（前端任务中心契约不变）
    pub report: Mutex<Option<core_types::EmbedRunReport>>,
    /// 分塔最近一轮结论（semantic / dedup 各自一条，互不覆盖——两塔状态独立）
    pub reports: Mutex<BTreeMap<String, core_types::EmbedRunReport>>,
    /// 最近一次失败原因（装配/入队/单塔运行失败，任务栏展示）
    pub last_error: Mutex<Option<String>>,
}

impl EmbedState {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            cancel: Arc::new(AtomicBool::new(false)),
            scope: Mutex::new(None),
            progress: Mutex::new(None),
            report: Mutex::new(None),
            reports: Mutex::new(BTreeMap::new()),
            last_error: Mutex::new(None),
        }
    }
}

impl Default for EmbedState {
    fn default() -> Self {
        Self::new()
    }
}

fn err(status: StatusCode, code: &str, msg: impl Into<String>) -> axum::response::Response {
    (status, Json(json!({ "error": msg.into(), "code": code }))).into_response()
}

/// POST /api/embed/start 请求体。
#[derive(Debug, Deserialize)]
pub struct StartReq {
    /// pending（增量：none/failed 入队，默认）| all（全库重嵌入）| backfill（只补从未跑过的）
    #[serde(default)]
    pub mode: Option<String>,
    /// semantic（默认；语义塔）| dedup（判重塔，模型写死不接受调用方指定）| all（两塔都跑）
    #[serde(default)]
    pub scope: Option<String>,
}

/// scope 字符串 → 塔列表；未知值明确报错（不静默当语义塔）。
fn towers_of(scope: &str) -> Result<Vec<EmbedScope>, String> {
    match scope {
        "semantic" => Ok(vec![EmbedScope::Semantic]),
        "dedup" => Ok(vec![EmbedScope::Dedup]),
        "all" => Ok(vec![EmbedScope::Semantic, EmbedScope::Dedup]),
        other => Err(format!("未知 scope: {other}（semantic | dedup | all）")),
    }
}

/// 语义塔路由键（active_embedder + endpoint 模型；与 /api/embed/status 同口径）。
fn semantic_model_key(conn: &rusqlite::Connection) -> Result<String, String> {
    let settings = infer::load_settings(conn);
    let spec = infer::registry()?
        .by_name(&settings.active_embedder)
        .cloned()
        .ok_or_else(|| format!("registry 无模型 {}", settings.active_embedder))?;
    Ok(infer::vector_model_key(&spec, &settings))
}

/// 某塔的模型标识（判重塔写死，语义塔按设置）。
fn tower_model(conn: &rusqlite::Connection, scope: EmbedScope) -> String {
    match scope {
        EmbedScope::Semantic => semantic_model_key(conn).unwrap_or_default(),
        EmbedScope::Dedup => core_config::DEDUP_EMBEDDER.to_string(),
    }
}

/// 按 mode 入队某一塔：pending（none/failed）| all（全库重嵌）| backfill（只补从未跑过）。
fn enqueue_scope(
    conn: &rusqlite::Connection,
    model: &str,
    scope: EmbedScope,
    mode: &str,
) -> Result<usize, String> {
    let r = match mode {
        "all" => svc_pipeline::embed_enq_all(conn, model, scope),
        "backfill" => svc_pipeline::enq_never_embedded(conn, model, scope),
        _ => svc_pipeline::enq_unembedded(conn, model, scope),
    };
    r.map_err(|e| e.to_string())
}

/// 去重塔嵌入器（DINOv3，写死；web 构建未编入 ONNX → 明确报错，不静默降级）。
///
/// 报错文案带上「塔写死、不可替换」的口径，避免把 engine-embed 的
/// 「改选 openai_compat 远程端点」建议误读成「判重塔可以换」。
fn build_dedup(
    models_root: &Path,
    conn: &rusqlite::Connection,
) -> Result<Arc<dyn EmbedProvider>, String> {
    let reg = infer::registry()?;
    let settings = infer::load_settings(conn);
    let hint =
        "（判重塔写死不可选，需 local-infer 构建 + 本地模型文件；job 留队列，环境就绪后可续跑）";
    let mut p =
        engine_embed::factory::build_dedup_embedder(&reg, &settings, models_root).map_err(|e| {
            format!(
                "去重塔（{}）装配失败：{e}{hint}",
                core_config::DEDUP_EMBEDDER
            )
        })?;
    p.load().map_err(|e| {
        format!(
            "去重塔（{}）加载失败：{e}{hint}",
            core_config::DEDUP_EMBEDDER
        )
    })?;
    Ok(Arc::from(p))
}

/// 按塔装配 provider（语义塔吃缓存；判重塔固定 DINOv3）。
fn build_tower(
    state: &AppState,
    conn: &rusqlite::Connection,
    scope: EmbedScope,
) -> Result<Arc<dyn EmbedProvider>, String> {
    match scope {
        EmbedScope::Semantic => state
            .provider
            .get(conn, &state.models_root)
            .map(|e| e.provider),
        EmbedScope::Dedup => build_dedup(&state.models_root, conn),
    }
}

/// 单塔失败落痕：该塔 report 条目 + lastError（另一塔结论不被覆盖）。
///
/// 不写 `report`（那是「最近一轮完整结论」的前端契约槽）——只写分塔 `reports`，
/// 避免一次装配失败把上一轮成功结论从任务栏抹掉。
fn record_tower_failure(state: &AppState, scope: EmbedScope, model: String, msg: String) {
    eprintln!("dam-web embed: {} 塔（{model}）失败：{msg}", scope.as_str());
    *state.embed.last_error.lock().unwrap() = Some(msg.clone());
    state.embed.reports.lock().unwrap().insert(
        scope.as_str().to_string(),
        core_types::EmbedRunReport {
            scope: scope.as_str().to_string(),
            model,
            errors: vec![msg],
            ..Default::default()
        },
    );
}

/// 顺序消费给定塔（provider 已装配）：每塔独立记录 report，失败只影响自己。
fn consume_towers(
    state: &AppState,
    conn: &rusqlite::Connection,
    built: Vec<(EmbedScope, Arc<dyn EmbedProvider>)>,
) {
    for (scope, provider) in built {
        if state.embed.cancel.load(Ordering::SeqCst) {
            break;
        }
        let key = scope.as_str().to_string();
        let model = provider.spec().name.clone();
        let cancel = state.embed.cancel.clone();
        let res = {
            let mut progress = |p: core_types::EmbedProgress| {
                *state.embed.progress.lock().unwrap() = Some(p);
            };
            svc_pipeline::run_embed_queue(
                conn,
                provider.as_ref(),
                scope,
                cancel.as_ref(),
                &mut progress,
            )
        };
        match res {
            Ok(rep) => {
                *state.embed.report.lock().unwrap() = Some(rep.clone());
                state.embed.reports.lock().unwrap().insert(key, rep);
            }
            Err(e) => record_tower_failure(state, scope, model, format!("{e}")),
        }
    }
}

/// 装配 + 顺序消费（导入自动启动用：装配失败也不影响另一塔）。
fn build_and_consume(state: &AppState, conn: &rusqlite::Connection, scopes: &[EmbedScope]) {
    for scope in scopes {
        if state.embed.cancel.load(Ordering::SeqCst) {
            break;
        }
        match build_tower(state, conn, *scope) {
            Ok(p) => consume_towers(state, conn, vec![(*scope, p)]),
            Err(m) => record_tower_failure(state, *scope, tower_model(conn, *scope), m),
        }
    }
}

/// 后台消费线程（独立连接；进度回调写 EmbedState）。
fn spawn_embed_job(state: AppState, built: Vec<(EmbedScope, Arc<dyn EmbedProvider>)>) {
    tokio::task::spawn_blocking(move || {
        match core_storage::open(&infer::db_path(&state.data_dir)) {
            Ok(conn) => consume_towers(&state, &conn, built),
            Err(e) => {
                *state.embed.last_error.lock().unwrap() = Some(format!("open db: {e}"));
            }
        }
        state.embed.cancel.store(false, Ordering::SeqCst);
        state.embed.running.store(false, Ordering::SeqCst);
    });
}

/// 启动嵌入任务：入队 → 装配 provider → spawn_blocking 消费，返回各塔入队数。
/// 双塔装配结果：(成功塔, 失败原因)。
type BuiltTowers = (
    Vec<(EmbedScope, Arc<dyn EmbedProvider>)>,
    Vec<(EmbedScope, String)>,
);

pub async fn start(State(state): State<AppState>, Json(req): Json<StartReq>) -> impl IntoResponse {
    let mode = req.mode.unwrap_or_else(|| "pending".into());
    let scope_s = req.scope.unwrap_or_else(|| "semantic".into());
    if !matches!(mode.as_str(), "pending" | "all" | "backfill") {
        return err(
            StatusCode::BAD_REQUEST,
            "bad_mode",
            format!("未知 mode: {mode}"),
        );
    }
    let towers = match towers_of(&scope_s) {
        Ok(t) => t,
        Err(m) => return err(StatusCode::BAD_REQUEST, "bad_scope", m),
    };
    if state
        .embed
        .running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return err(StatusCode::CONFLICT, "already_running", "已有嵌入任务在跑");
    }

    let sem_model = {
        let conn = state.db.lock().await;
        match semantic_model_key(&conn) {
            Ok(m) => m,
            Err(m) => {
                state.embed.running.store(false, Ordering::SeqCst);
                return err(StatusCode::INTERNAL_SERVER_ERROR, "settings", m);
            }
        }
    };

    // 入队（主连接；快操作）。逐塔独立计数，一塔入队失败不默默清零另一塔。
    let queued: BTreeMap<String, usize> = {
        let conn = state.db.lock().await;
        let mut out = BTreeMap::new();
        for scope in &towers {
            let model = match scope {
                EmbedScope::Semantic => sem_model.as_str(),
                EmbedScope::Dedup => core_config::DEDUP_EMBEDDER,
            };
            match enqueue_scope(&conn, model, *scope, &mode) {
                Ok(n) => {
                    out.insert(scope.as_str().to_string(), n);
                }
                Err(e) => {
                    state.embed.running.store(false, Ordering::SeqCst);
                    return err(StatusCode::INTERNAL_SERVER_ERROR, "enqueue", e);
                }
            }
        }
        out
    };

    // 装配 provider（失败回滚 running，把原因回给前端）。双塔模式下只有全部塔都装配失败才算请求失败。
    let st = state.clone();
    let want = towers.clone();
    let built = tokio::task::spawn_blocking(move || -> BuiltTowers {
        let mut ok = Vec::new();
        let mut bad = Vec::new();
        match core_storage::open(&infer::db_path(&st.data_dir)) {
            Ok(conn) => {
                for scope in want {
                    match build_tower(&st, &conn, scope) {
                        Ok(p) => ok.push((scope, p)),
                        Err(m) => bad.push((scope, m)),
                    }
                }
            }
            Err(e) => {
                for scope in want {
                    bad.push((scope, format!("open db: {e}")));
                }
            }
        }
        (ok, bad)
    })
    .await;

    let (built, bad) = match built {
        Ok(v) => v,
        Err(e) => {
            state.embed.running.store(false, Ordering::SeqCst);
            return err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string());
        }
    };
    // 请求的各塔身份（装配成功的用 provider 实际身份，装配失败的用配置身份）
    let tower_models: BTreeMap<String, String> = towers
        .iter()
        .map(|s| {
            let from_provider = built
                .iter()
                .find(|(b, _)| b == s)
                .map(|(_, p)| p.spec().name.clone());
            let m = from_provider.unwrap_or_else(|| match s {
                EmbedScope::Semantic => sem_model.clone(),
                EmbedScope::Dedup => core_config::DEDUP_EMBEDDER.to_string(),
            });
            (s.as_str().to_string(), m)
        })
        .collect();

    // 先清上一轮痕迹，再记本轮的单塔装配失败（顺序不能反，否则错误被清掉）
    *state.embed.report.lock().unwrap() = None;
    *state.embed.last_error.lock().unwrap() = None;
    state.embed.reports.lock().unwrap().clear();
    for (scope, m) in &bad {
        let model = tower_models
            .get(scope.as_str())
            .cloned()
            .unwrap_or_default();
        record_tower_failure(&state, *scope, model, m.clone());
    }

    // 请求的塔全都装配不起来 → 502 明确报错（已入队的 job 留 pending，环境就绪后可续跑）
    if built.is_empty() {
        state.embed.running.store(false, Ordering::SeqCst);
        let msg = bad
            .iter()
            .map(|(s, m)| format!("{} 塔：{m}", s.as_str()))
            .collect::<Vec<_>>()
            .join("；");
        return err(StatusCode::BAD_GATEWAY, "provider_failed", msg);
    }

    *state.embed.scope.lock().unwrap() = Some(scope_s.clone());
    *state.embed.progress.lock().unwrap() = Some(core_types::EmbedProgress {
        jobId: 0,
        assetId: 0,
        scope: scope_s.clone(),
        model: tower_models.values().next().cloned().unwrap_or_default(),
        status: "queued".into(),
        index: 0,
        total: queued.values().sum(),
        doneCount: 0,
        failedCount: 0,
        error: None,
    });
    state.embed.cancel.store(false, Ordering::SeqCst);

    spawn_embed_job(state.clone(), built);

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "queued": queued.values().sum::<usize>(),
            "queuedByTower": queued,
            "mode": mode,
            "scope": scope_s,
            "model": tower_models.values().next().cloned().unwrap_or_default(),
            "models": tower_models,
            "towerErrors": bad.iter().map(|(s, m)| json!({"scope": s.as_str(), "error": m})).collect::<Vec<_>>(),
        })),
    )
        .into_response()
}

/// POST /api/embed/cancel：请求取消（当前图跑完后停；已入队 job 回 pending 可续跑）。
pub async fn cancel(State(state): State<AppState>) -> impl IntoResponse {
    if !state.embed.running.load(Ordering::SeqCst) {
        return err(StatusCode::CONFLICT, "not_running", "没有运行中的嵌入任务");
    }
    state.embed.cancel.store(true, Ordering::SeqCst);
    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
}

/// GET /api/embed/status：任务状态 + 队列统计 + 向量表 + 覆盖度 + 分塔结论。
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
        let stats = svc_pipeline::embed_stats(&conn, &active).map_err(|e| e.to_string())?;
        let models: Vec<serde_json::Value> = core_storage::vecstore::tables(&conn)
            .map_err(|e| e.to_string())?
            .iter()
            .map(|(model, dims, table)| {
                json!({
                    "model": model, "dims": dims, "table": table,
                    "rows": core_storage::vecstore::count(&conn, model).unwrap_or(0),
                    "active": *model == active,
                })
            })
            .collect();
        Ok(json!({
            "running": st.embed.running.load(Ordering::SeqCst),
            "scope": st.embed.scope.lock().unwrap().clone(),
            "progress": st.embed.progress.lock().unwrap().clone(),
            "report": st.embed.report.lock().unwrap().clone(),
            // 分塔结论：去重塔与语义塔各自一条，互不覆盖（KI-2a 可观测面）
            "reports": st.embed.reports.lock().unwrap().clone(),
            "lastError": st.embed.last_error.lock().unwrap().clone(),
            "activeModel": active,
            "stats": stats,
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

/// GET /api/embed/failed：失败 job 明细（UI 失败列表，两塔合并，按 model 区分）。
pub async fn failed(State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match svc_pipeline::embed_failed_list(&conn, 200) {
        Ok(items) => (StatusCode::OK, Json(json!({ "items": items }))).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "failed", e.to_string()),
    }
}

/// POST /api/embed/retry-failed：两塔失败项各自重置 pending（下次 start 消费）。
/// 判重塔的失败**不混进**语义塔计数（KI-2a：各自 scope 各自重试）。
pub async fn retry_failed(State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.db.lock().await;
    let sem_model = match semantic_model_key(&conn) {
        Ok(m) => m,
        Err(m) => return err(StatusCode::INTERNAL_SERVER_ERROR, "settings", m),
    };
    let reset = match svc_pipeline::embed_retry_failed(&conn, &sem_model, EmbedScope::Semantic) {
        Ok(n) => n,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "retry", e.to_string()),
    };
    let dedup_reset = match svc_pipeline::embed_retry_failed(
        &conn,
        core_config::DEDUP_EMBEDDER,
        EmbedScope::Dedup,
    ) {
        Ok(n) => n,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "retry", e.to_string()),
    };
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "reset": reset, "dedupReset": dedup_reset })),
    )
        .into_response()
}

// ---------------- 导入钩子（KI-2a：入库即算 dedup 向量） ----------------

/// 导入批次落库后：本批新增资产同时入两塔队列（语义塔 + 写死的判重塔）。
///
/// 纯 SQL、毫秒级 —— 调用方在导入线程内直接调用，**不阻塞导入返回**；
/// 消费由 `auto_start_after_import` 在后台线程做。返回各塔入队数。
pub(crate) fn enqueue_after_import(
    conn: &rusqlite::Connection,
    batch_id: i64,
) -> Result<ScopeEnqueue, String> {
    let sem_model = semantic_model_key(conn)?;
    svc_pipeline::enq_import_batch(conn, &sem_model, core_config::DEDUP_EMBEDDER, batch_id)
        .map_err(|e| e.to_string())
}

/// 导入完成后在后台尽力消费两塔（不阻塞导入；装配失败只落错误槽，job 留队列可见）。
pub(crate) fn auto_start_after_import(state: &AppState, batch_id: i64, queued: ScopeEnqueue) {
    if queued.total() == 0 {
        return;
    }
    if state
        .embed
        .running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        // 已有嵌入任务在跑：其 claim 循环跑到队列空，新入队 job 会被顺带消费
        eprintln!(
            "dam-web import embed: batch {batch_id} queued semantic={} dedup={}；已有嵌入任务在跑，留在队列",
            queued.semantic, queued.dedup
        );
        return;
    }
    eprintln!(
        "dam-web import embed: batch {batch_id} queued semantic={} dedup={}，后台启动消费",
        queued.semantic, queued.dedup
    );
    *state.embed.scope.lock().unwrap() = Some("import".into());
    *state.embed.progress.lock().unwrap() = Some(core_types::EmbedProgress {
        jobId: 0,
        assetId: 0,
        scope: "import".into(),
        model: core_config::DEDUP_EMBEDDER.to_string(),
        status: "queued".into(),
        index: 0,
        total: queued.total(),
        doneCount: 0,
        failedCount: 0,
        error: None,
    });
    *state.embed.report.lock().unwrap() = None;
    *state.embed.last_error.lock().unwrap() = None;
    state.embed.reports.lock().unwrap().clear();
    state.embed.cancel.store(false, Ordering::SeqCst);

    let st = state.clone();
    // 只有本批真的入了某一塔的 job 才去跑该塔（避免空转加载 86MB 判重模型）
    let scopes: Vec<EmbedScope> = [
        (EmbedScope::Semantic, queued.semantic),
        (EmbedScope::Dedup, queued.dedup),
    ]
    .into_iter()
    .filter_map(|(s, n)| if n > 0 { Some(s) } else { None })
    .collect();
    tokio::task::spawn_blocking(move || {
        match core_storage::open(&infer::db_path(&st.data_dir)) {
            Ok(conn) => build_and_consume(&st, &conn, &scopes),
            Err(e) => {
                *st.embed.last_error.lock().unwrap() = Some(format!("open db: {e}"));
            }
        }
        st.embed.cancel.store(false, Ordering::SeqCst);
        st.embed.running.store(false, Ordering::SeqCst);
    });
}
