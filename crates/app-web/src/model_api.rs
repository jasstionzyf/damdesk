//! 模型获取入口（卡 task）：`engine-embed::model_store` 有 lama manifest + 下载/哈希
//! 校验实现，但**从来没有调用方** —— 208MB 的 `lama-fp32` 只能靠人工把文件拖进模型目录，
//! 于是 A5 局部重绘在真机上永远是 `ModelMissing`（「判定能跑、修复不能跑」）。
//!
//! 本模块只做三件事（推理/加载仍属 engine-restore 领地，不在这里）：
//! 1. `GET  /api/models`                  列出 manifest 的落盘状态（缺哪件、多大、目标绝对路径）
//! 2. `POST /api/models/{name}/download`  单飞后台下载（sha256 强校验，可重试，失败不留 .part）
//! 3. `GET  /api/models/download/status`  进度轮询（不做 SSE：桌面 WebView 与 web 同口径）
//!
//! 落盘根 = `<models_root>/models/<manifest.name>/`（`core_config::models_root` 契约），
//! 与 `engine_restore::model_file` / `engine-ocr` 读的**同一个**根 —— 下载入口写别处
//! 就是「下完了还是 ModelMissing」，这是本卡最要紧的不变量（单测钉住）。
//!
//! 默认**不做**首启静默下载：208MB 在用户没同意时不值得，且 settings 的
//! `auto_download` 语义只覆盖 embedding 权重（见 model-distribution.md）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

// model_api 的 download 是 `#[cfg(feature = "local-infer")]`（桌面专属），web 形态下不编译，
// 导入必须随 cfg 走，否则 web check 报 unused。
#[cfg(feature = "local-infer")]
use crate::error_codes::{codes, err_coded};
use crate::infer;
use crate::AppState;

/// 未随安装包内置、因而需要「下载入口」的模型（manifest 名）。
///
/// 内置清单在 `app-tauri::shell::BUNDLED_MODELS`（安装包资源目录）；`lama-fp32`(208MB) 与
/// PP-OCRv6 两件按体积裁决不内置。OCR 的缺件提示已自带手工放置路径（ocr_api），
/// 本清单先只收 lama，接口本身对任意 manifest 通用（加模型 = 加一条 + registry 一条）。
pub const DOWNLOADABLE: &[&str] = &["lama-fp32"];

fn err(status: StatusCode, code: &str, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into(), "code": code }))).into_response()
}

/// 下载目标的生效根（`<base>/models`）。`models_root` 与引擎读的根同一口径：
/// 桌面壳 boot 把 `plan.base` 同时交给 app-web（models_root）与 `DAM_MODELS_ROOT`。
pub fn model_root(state: &AppState) -> std::path::PathBuf {
    core_config::models_root(&state.models_root)
}

/// 分发源：显式入参 > settings.model_base_url > 内置默认。尾部斜杠去掉（store 自己拼 "/"）。
fn effective_base_url(settings: &str, explicit: Option<&str>) -> String {
    let picked = explicit
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let s = settings.trim();
            (!s.is_empty()).then_some(s)
        })
        .unwrap_or(core_config::DEFAULT_MODEL_BASE_URL);
    picked.trim_end_matches('/').to_string()
}

// ---------- 单飞状态 + 进度快照 ----------

/// 进度/结果快照（用 JSON Value 存事件，避免本模块在 `local-infer` 关闭的 web 构建里
/// 依赖 `engine-embed::model_store` 的类型 —— 结构体本身两种构建都要存在）。
#[derive(Default)]
struct Progress {
    model: Option<String>,
    status: Option<String>,
    done_bytes: u64,
    total_bytes: u64,
    completed_files: usize,
    total_files: usize,
    error: Option<String>,
    outcome: Option<serde_json::Value>,
    started_at: Option<i64>,
    finished_at: Option<i64>,
}

/// 模型下载单飞状态（同一时刻只允许一个下载；进度供 `GET /api/models/download/status` 轮询）。
#[derive(Default)]
pub struct ModelDownloadState {
    running: AtomicBool,
    progress: Mutex<Progress>,
}

impl ModelDownloadState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// 当前正在下载的模型名（空闲 = None）。
    pub fn current(&self) -> Option<String> {
        self.progress.lock().expect("model progress lock").model.clone()
    }

    /// 抢到即拿 guard（drop 释放；panic 也不会把状态钉死）；已有下载在跑 → None。
    pub fn try_begin(self: &Arc<Self>, model: &str) -> Option<ModelDownloadGuard> {
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return None;
        }
        {
            let mut p = self.progress.lock().expect("model progress lock");
            *p = Progress {
                model: Some(model.to_string()),
                started_at: Some(now_ms()),
                ..Default::default()
            };
        }
        Some(ModelDownloadGuard(self.clone()))
    }

    fn record_json(&self, ev: serde_json::Value) {
        let mut p = self.progress.lock().expect("model progress lock");
        p.status = ev.get("status").and_then(|v| v.as_str()).map(str::to_string);
        p.done_bytes = ev.get("doneBytes").and_then(|v| v.as_u64()).unwrap_or(p.done_bytes);
        p.total_bytes = ev.get("totalBytes").and_then(|v| v.as_u64()).unwrap_or(p.total_bytes);
        p.completed_files = ev
            .get("completedFiles")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(p.completed_files);
        p.total_files = ev
            .get("totalFiles")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(p.total_files);
        if let Some(e) = ev.get("error").and_then(|v| v.as_str()) {
            p.error = Some(e.to_string());
        }
    }

    fn set_error(&self, msg: &str) {
        let mut p = self.progress.lock().expect("model progress lock");
        p.error = Some(msg.to_string());
        p.status = Some("failed".into());
    }

    fn set_outcome(&self, outcome: serde_json::Value) {
        let mut p = self.progress.lock().expect("model progress lock");
        p.status = Some("done".into());
        p.outcome = Some(outcome);
    }

    fn snapshot(&self) -> serde_json::Value {
        let p = self.progress.lock().expect("model progress lock");
        json!({
            "running": self.running.load(Ordering::SeqCst),
            "model": p.model,
            "status": p.status,
            "doneBytes": p.done_bytes,
            "totalBytes": p.total_bytes,
            "completedFiles": p.completed_files,
            "totalFiles": p.total_files,
            "error": p.error,
            "outcome": p.outcome,
            "startedAt": p.started_at,
            "finishedAt": p.finished_at,
        })
    }
}

/// 单飞 guard：drop 即释放（含 panic 路径）+ 打完成时间。
pub struct ModelDownloadGuard(Arc<ModelDownloadState>);

impl Drop for ModelDownloadGuard {
    fn drop(&mut self) {
        {
            let mut p = self.0.progress.lock().expect("model progress lock");
            p.finished_at = Some(now_ms());
        }
        self.0.running.store(false, Ordering::SeqCst);
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------- 路由 ----------

/// GET /api/models：全部 manifest 的落盘状态（`downloadable` = 未内置、需下载）。
#[cfg(feature = "local-infer")]
pub async fn list(State(state): State<AppState>) -> Response {
    use engine_embed::model_store::{manifests, ModelStore};

    let root = model_root(&state);
    let settings = {
        let conn = state.db.lock().await;
        infer::load_settings(&conn)
    };
    let base = effective_base_url(&settings.model_base_url, None);
    let store = ModelStore::new(root.clone(), base.clone());
    let models: Vec<serde_json::Value> = manifests()
        .into_iter()
        .map(|m| {
            let checks = store.status(&m);
            let ready = checks.iter().all(|c| c.present);
            let present_bytes: u64 = checks.iter().filter(|c| c.present).map(|c| c.bytes).sum();
            let missing: Vec<String> = checks
                .iter()
                .filter(|c| !c.present)
                .filter_map(|c| {
                    m.file(&c.name)
                        .map(|f| store.file_path(&m, f).display().to_string())
                })
                .collect();
            json!({
                "name": m.name,
                "downloadable": DOWNLOADABLE.contains(&m.name.as_str()),
                "ready": ready,
                "presentBytes": present_bytes,
                "totalBytes": m.total_bytes(),
                "missingPaths": missing,
                "files": checks,
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(json!({
            "root": root.display().to_string(),
            "baseUrl": base,
            "downloadable": DOWNLOADABLE,
            "models": models,
        })),
    )
        .into_response()
}

#[cfg(not(feature = "local-infer"))]
pub async fn list(State(state): State<AppState>) -> Response {
    let root = model_root(&state);
    (
        StatusCode::OK,
        Json(json!({
            "root": root.display().to_string(),
            "baseUrl": serde_json::Value::Null,
            "downloadable": DOWNLOADABLE,
            "models": [],
            "note": "本构建不含本地 ONNX 运行时（local-infer off）：模型下载为桌面专属能力",
        })),
    )
        .into_response()
}

/// POST /api/models/{name}/download：单飞后台下载。
///
/// body（可省略）：`{"baseUrl": "...", "force": false}` —— baseUrl 覆盖 settings 的
/// `model_base_url`（镜像/内网 `file://` 目录）；force = 已在盘也重下（校验损坏后重试）。
#[derive(Debug, Default, Deserialize)]
pub struct DownloadReq {
    #[serde(default, alias = "base_url", alias = "baseUrl")]
    pub base_url: Option<String>,
    #[serde(default)]
    pub force: bool,
}

#[cfg(feature = "local-infer")]
pub async fn download(
    State(state): State<AppState>,
    Path(name): Path<String>,
    body: Option<Json<DownloadReq>>,
) -> Response {
    use engine_embed::model_store::{manifest_for, ModelStore};

    let Some(manifest) = manifest_for(&name) else {
        return err(
            StatusCode::NOT_FOUND,
            "unknown_model",
            format!("manifest 里没有模型 {name}（已知见 GET /api/models）"),
        );
    };
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let settings = {
        let conn = state.db.lock().await;
        infer::load_settings(&conn)
    };
    let base = effective_base_url(&settings.model_base_url, req.base_url.as_deref());
    // 源门控：内置默认源是 siglip 的 HF 仓，别的模型在那儿**不存在** —— 与其让 ureq
    // 返回 404 让人猜，不如直接说清「要配镜像根」。
    if base == core_config::DEFAULT_MODEL_BASE_URL && name != core_config::DEFAULT_EMBEDDER {
        let url_paths = manifest
            .files
            .iter()
            .map(|f| f.url_path.as_str())
            .collect::<Vec<_>>()
            .join(" / ");
        return err_coded(
            StatusCode::CONFLICT,
            codes::MODEL_SOURCE_UNCONFIGURED,
            json!({ "base": base, "name": name }),
            format!(
                "未配置模型分发源：默认源是 siglip 官方仓（{base}），其中没有 {name}。\
                 请把设置里的「模型下载源」改成镜像根（该根下需有 {url_paths}），\
                 或在请求体里传 baseUrl（支持 file:// 本地目录）。"
            ),
        );
    }
    let Some(guard) = state.models.try_begin(&name) else {
        let current = state.models.current().unwrap_or_default();
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!("已有模型在下载（{current}），等它结束或稍后重试"),
                "code": "download_running",
                "running": true,
                "model": current,
            })),
        )
            .into_response();
    };
    let files = manifest.files.len();
    let total_bytes = manifest.total_bytes();
    let root = model_root(&state);
    let store = ModelStore::new(root.clone(), base.clone());
    let force = req.force;
    let st = state.models.clone();
    tokio::task::spawn_blocking(move || {
        let _ = perform(&store, &manifest, force, &st);
        drop(guard);
    });
    (
        StatusCode::ACCEPTED,
        Json(json!({
            "ok": true,
            "model": name,
            "baseUrl": base,
            "root": root.display().to_string(),
            "totalBytes": total_bytes,
            "files": files,
            "force": force,
        })),
    )
        .into_response()
}

#[cfg(not(feature = "local-infer"))]
pub async fn download(
    State(_state): State<AppState>,
    Path(_name): Path<String>,
    _body: Option<Json<DownloadReq>>,
) -> Response {
    err(
        StatusCode::CONFLICT,
        "local_infer_disabled",
        "本构建不含本地 ONNX 运行时（local-infer off）：模型下载为桌面专属能力",
    )
}

/// GET /api/models/download/status：进度快照（含上次结果与失败原因）。
pub async fn status(State(state): State<AppState>) -> Response {
    (StatusCode::OK, Json(state.models.snapshot())).into_response()
}

/// 同步下载（spawn_blocking 里的实体；单测直接调它验证成功/失败/重试语义）。
///
/// 语义：**先**显式建目录（失败给带绝对路径的错，而不是 ureq 的裸 io 错），再交给
/// `ModelStore::download`（sha256 强校验、失败删 .part、已在盘的跳过 → 可重入）。
#[cfg(feature = "local-infer")]
pub(crate) fn perform(
    store: &engine_embed::model_store::ModelStore,
    manifest: &engine_embed::model_store::ModelManifest,
    force: bool,
    state: &ModelDownloadState,
) -> Result<serde_json::Value, String> {
    let dir = store.dir(manifest);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        let msg = format!("模型目录不可写 {}: {e}", dir.display());
        state.set_error(&msg);
        return Err(msg);
    }
    let mut cb = |ev: engine_embed::model_store::DownloadEvent| {
        state.record_json(serde_json::to_value(&ev).unwrap_or(serde_json::Value::Null));
    };
    match store.download(manifest, force, &mut cb) {
        Ok(outcome) => {
            let v = json!({
                "model": outcome.model,
                "downloaded": outcome.downloaded,
                "skipped": outcome.skipped,
                "totalBytes": outcome.totalBytes,
                "root": dir.display().to_string(),
            });
            state.set_outcome(v.clone());
            Ok(v)
        }
        Err(e) => {
            let msg = e.to_string();
            state.set_error(&msg);
            Err(msg)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 最要紧的不变量：下载落点 == 引擎读取点（`<root>/models/<name>/<name>.onnx`）。
    /// 写偏一处就是「下完了还是 ModelMissing」，所以这里对 `core_config::models_root`
    /// 契约做显式断言（engine-restore 用的同一个函数）。
    #[test]
    fn download_root_matches_engine_contract() {
        let fake = std::path::PathBuf::from("/data/app");
        let expect = std::path::PathBuf::from("/data/app/models/lama-fp32/lama-fp32.onnx");
        let dir = core_config::model_dir(&fake, "lama-fp32");
        assert_eq!(dir, std::path::PathBuf::from("/data/app/models/lama-fp32"));
        assert_eq!(dir.join("lama-fp32.onnx"), expect);
        // 与 core_config::models_root 一致（本模块 model_root() 的实体）
        assert_eq!(
            core_config::models_root(&fake).join("lama-fp32/lama-fp32.onnx"),
            expect
        );
    }

    #[test]
    fn base_url_resolution_order() {
        assert_eq!(
            effective_base_url("https://mirror/a/", None),
            "https://mirror/a"
        );
        assert_eq!(
            effective_base_url("https://mirror/a", Some(" file:///tmp/src/ ")),
            "file:///tmp/src"
        );
        // settings 里的空串 → 内置默认（不让空源把下载打到 "文件不存在" 的裸路径上）
        assert_eq!(
            effective_base_url("   ", None),
            core_config::DEFAULT_MODEL_BASE_URL
        );
    }

    /// 单飞：并发的第二次必须拿不到守卫；guard drop 后能再拿（可重试）。
    #[test]
    fn single_flight_guard_releases_on_drop() {
        let st = Arc::new(ModelDownloadState::new());
        let g = st.try_begin("lama-fp32").expect("首次应拿到");
        assert!(st.is_running());
        assert_eq!(st.current().as_deref(), Some("lama-fp32"));
        assert!(st.try_begin("lama-fp32").is_none(), "并发第二次必须被拒");
        drop(g);
        assert!(!st.is_running());
        assert!(st.try_begin("lama-fp32").is_some(), "释放后可重试");
    }

    /// 失败要**如实**进状态：错误进 snapshot、状态 failed、磁盘不留半截文件。
    #[cfg(feature = "local-infer")]
    #[test]
    fn perform_failure_is_visible_and_leaves_no_part_file() {
        use engine_embed::model_store::{sha256_file, ModelFile, ModelManifest, ModelStore};

        let base = std::env::temp_dir().join(format!("dam-model-api-{}-fail", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("src-empty");
        std::fs::create_dir_all(&src).unwrap();
        let manifest = ModelManifest {
            name: "fake-lama".into(),
            text_max_length: 0,
            files: vec![ModelFile {
                name: "fake-lama.onnx".into(),
                url_path: "fake-lama.onnx".into(),
                bytes: 3,
                sha256: sha256_file(&base.join("nope")).map(|v| v.1).unwrap_or_else(|_| "0".into()),
            }],
        };
        let store = ModelStore::new(base.join("models"), format!("file://{}", src.display()));
        let st = ModelDownloadState::new();
        let err = perform(&store, &manifest, false, &st).expect_err("源里没有文件必须失败");
        assert!(err.contains("fake-lama.onnx"), "错误要指到具体文件: {err}");
        let snap = st.snapshot();
        assert_eq!(snap["status"], "failed");
        assert!(snap["error"].as_str().is_some_and(|e| e.contains("fake-lama.onnx")));
        assert!(!store.file_path(&manifest, &manifest.files[0]).exists());
        assert!(!store
            .file_path(&manifest, &manifest.files[0])
            .with_extension("part")
            .exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 成功路径（真 sha 校验 + 幂等跳过）+ 进度进 snapshot。
    #[cfg(feature = "local-infer")]
    #[test]
    fn perform_success_reports_progress_then_skips() {
        use engine_embed::model_store::{sha256_file, ModelFile, ModelManifest, ModelStore};

        let base = std::env::temp_dir().join(format!("dam-model-api-{}-ok", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let p = src.join("fake.onnx");
        std::fs::write(&p, b"onnx-bytes").unwrap();
        let (bytes, sha256) = sha256_file(&p).unwrap();
        let manifest = ModelManifest {
            name: "fake-model".into(),
            text_max_length: 0,
            files: vec![ModelFile {
                name: "fake.onnx".into(),
                url_path: "fake.onnx".into(),
                bytes,
                sha256,
            }],
        };
        let store = ModelStore::new(base.join("models"), format!("file://{}", src.display()));
        let st = ModelDownloadState::new();
        let out = perform(&store, &manifest, false, &st).expect("应下载成功");
        assert_eq!(out["downloaded"], 1);
        assert_eq!(out["skipped"], 0);
        let snap = st.snapshot();
        assert_eq!(snap["status"], "done");
        assert!(snap["doneBytes"].as_u64().unwrap() > 0);
        assert!(snap["error"].is_null());
        assert!(store.is_complete(&manifest));

        // 幂等：再点一次 = skipped，不重下
        let st2 = ModelDownloadState::new();
        let out2 = perform(&store, &manifest, false, &st2).expect("二次应跳过");
        assert_eq!(out2["skipped"], 1);
        assert_eq!(out2["downloaded"], 0);
        let _ = std::fs::remove_dir_all(&base);
    }
}
