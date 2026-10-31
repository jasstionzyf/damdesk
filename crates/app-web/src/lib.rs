//! app-web：dam-web 的 axum HTTP 壳。
//!
//! 分层位置：L3 进程壳（与 app-tauri 同级，业务逻辑零容忍）。
//! 职责：HTTP 路由 + 静态资源（web/dist）+ DB 打开（复用 core-storage open/migrate）。
//! W2：导入链路（import_api）/ 图库读模型（library_api）/ 媒体服务（media）。
//! W3：搜索（search_api，文搜图/以图搜图）+ 推理设置（settings_api）+ 嵌入队列（embed_api），
//!  provider 装配在 infer（R6：web 构建只有 openai_compat 远程实现）。
//! W7：可嵌入化（ServeConfig/prepare/serve_*）——Tauri 桌面壳把本 crate 编进同一进程
//!  （feature `local-infer`，R6），localhost 随机端口 + 一次性 token 引导（auth）。

pub mod auth;
pub mod capabilities_api;
pub mod compliance_api;
pub mod dedup_api;
pub mod embed_api;
pub mod error_codes;
pub mod exif_api;
pub mod export_api;
pub mod import_api;
pub mod infer;
pub mod library_api;
pub mod media;
pub mod model_api;
pub mod ocr_api;
pub mod os_api;
pub mod search_api;
pub mod settings_api;
pub mod smart_folder_api;
// N1 规格实体（卡 task）：`/api/specs` 的模型 + CRUD + 旧行业包迁移
pub mod spec_api;
pub mod spec_seed;
pub mod tag_api;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use tokio::sync::Mutex;

/// 默认监听端口（R4：v1 单用户无登录，默认只绑 127.0.0.1）。
pub const DEFAULT_PORT: u16 = 8787;

/// 全局状态：库数据库连接 + 数据目录（DAM_WEB_DATA）+ 导入任务状态 + 缩略图根。
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Mutex<rusqlite::Connection>>,
    pub data_dir: PathBuf,
    /// 缩略图根（{data_dir}/thumbs，与 app-tauri thumbs_root 同语义）
    pub thumbs_root: PathBuf,
    /// W7：生效模型根（factory 权重根：`<root>/models/<name>` 的 `<root>`；
    /// 桌面内嵌时由壳做三级解析传入——appData 优先、包内资源次之；容器 = `{data_dir}`）
    pub models_root: PathBuf,
    /// W7：一次性 token 引导鉴权（None = 关闭，web 容器 / dev 反代）
    pub auth: Arc<auth::Auth>,
    pub import: Arc<import_api::ImportState>,
    /// W3 嵌入任务状态（队列消费线程写、status 轮询读）
    pub embed: Arc<embed_api::EmbedState>,
    /// W4 打标任务状态（模板 + 自定义两条独立队列）
    pub tag: Arc<tag_api::TagState>,
    /// OCR-E 文字识别队列状态（单槽）
    pub ocr: Arc<ocr_api::OcrState>,
    /// W3 推理 provider 缓存（设置变更即失效）
    pub provider: Arc<infer::ProviderCache>,
    /// E2：导出任务状态（单一活动导出）
    pub export: Arc<export_api::ExportState>,
    /// JUDGE-ENTRY：合规判定运行状态（单槽：running/cancel/progress/report/error）
    pub compliance: Arc<compliance_api::ComplianceRunState>,
    /// DESKTOP-MISSING-STAT-1：失联核查单飞状态（POST /api/library/verify）
    pub verify: Arc<library_api::VerifyState>,
    /// 模型获取入口（卡 task）：下载单飞 + 进度快照（POST /api/models/{name}/download）
    pub models: Arc<model_api::ModelDownloadState>,
    /// KI-28：插件子进程的运行时资源（内置 python 解释器 + 启动器目录）。
    /// 桌面壳从 `<resource_dir>` 解析后注入；容器/开发形态两者皆 None（回落 `python3` + 仓库 `tools/`）。
    pub pack_runtime: PackRuntime,
}

/// KI-28：插件子进程运行时（内置 python + 启动器）的实际落位。
///
/// 为什么单独一个结构：`PackHostConfig` 在 `compliance_api` 两处装配（探针 + 真判定），
/// 解释器与启动器目录必须**同一份解析结果**，否则「探针能拉起、判定拉不起」这种分裂会出现。
///
/// 形态判定与 `GET /api/capabilities` 同源（[`is_desktop_form`]）：容器/开发机继续走裸 `python3`，
/// **不硬编码任何平台路径**（Windows `python.exe` / unix `bin/python3` 由候选表穷举 + `is_file` 实测）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackRuntime {
    /// 内置 runtime 解释器（`<resource>/runtime/python/python.exe` 等）；None = 未随包分发
    pub python: Option<PathBuf>,
    /// 随包分发的启动器目录（含 `dampack_launcher.py`）；None = 回落仓库 `tools/`
    pub launcher_dir: Option<PathBuf>,
}

impl PackRuntime {
    /// 解析运行时资源。`resource_dir` = 壳给的包内资源根（容器形态为 None）。
    ///
    /// 解释器候选顺序：`DAMPACK_RUNTIME_PYTHON`（测试/排障显式指定）→ `<res>/runtime/python/python.exe`
    /// → `<res>/runtime/python/bin/python3` → `<res>/runtime/python/python3`。
    /// 候选全部 `is_file()` 实测落选即 None —— 绝不返回「看起来对但不存在」的路径。
    pub fn resolve(resource_dir: Option<&Path>) -> Self {
        Self::resolve_with(is_desktop_form(), resource_dir)
    }

    /// 形态显式版（单测用）：`desktop=false` = 容器/开发机，一律不加载内置 runtime。
    pub fn resolve_with(desktop: bool, resource_dir: Option<&Path>) -> Self {
        if !desktop {
            // 容器/开发形态：既有行为（`python3` + 仓库 tools/）不回退
            return Self::default();
        }
        let python = std::env::var_os("DAMPACK_RUNTIME_PYTHON")
            .map(PathBuf::from)
            .filter(|p| p.is_file())
            .or_else(|| {
                let root = resource_dir?.join("runtime").join("python");
                [
                    root.join("python.exe"),
                    root.join("bin").join("python3"),
                    root.join("python3"),
                ]
                .into_iter()
                .find(|p| p.is_file())
            });
        let launcher_dir = resource_dir
            .map(|r| r.join("pack-launcher"))
            .filter(|d| d.join("dampack_launcher.py").is_file());
        Self {
            python,
            launcher_dir,
        }
    }

    /// 人话状态（`GET /api/capabilities.packRuntime`；缺失时界面/支持据此判断「重装还是改配置」）。
    pub fn describe(&self) -> serde_json::Value {
        self.describe_for(is_desktop_form())
    }

    /// 形态显式版（单测钉两种形态的文案，不受编译期特性统一影响）。
    pub fn describe_for(&self, desktop: bool) -> serde_json::Value {
        let note = match (&self.python, desktop) {
            (Some(_), _) => serde_json::Value::Null,
            (None, true) => json!(
                "内置 python 运行时缺失（安装包不完整或 resources/runtime/python 未随包分发）：\
                 行业包（合规判定插件）无法启动；可用 DAMPACK_RUNTIME_PYTHON 指定解释器"
            ),
            (None, false) => json!("容器/开发形态：插件子进程用系统 python3，不加载内置运行时"),
        };
        json!({
            "python": self.python.as_ref().map(|p| p.to_string_lossy().to_string()),
            "launcherDir": self.launcher_dir.as_ref().map(|p| p.to_string_lossy().to_string()),
            "bundled": self.python.is_some(),
            "note": note,
        })
    }
}

/// 前端产物目录：仓库根 `web/dist`（prod 由 Dockerfile 拷入；dev 由 vite 直连 /api）。
fn web_dist_dir() -> PathBuf {
    std::env::var("DAM_WEB_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("web/dist"))
}

/// 运行形态：桌面（Tauri 壳 / `local-infer` 构建）还是网页容器。
///
/// 两个来源取或：
/// - `library_api::IS_DESKTOP_FORM`（编译期 feature，真桌面构建）
/// - `DAM_WEB_FORM=desktop`（WIN-PRE2 验收钩子：无 GUI 的容器/CI 复刻桌面壳 seeding 形态，
///   见 `scripts/tag_default_acceptance.sh`）
///
/// WEBCAP 用同一口径算能力 flags / 前置拦截文案，避免「钩子形态」与「真桌面」两套判定。
pub fn is_desktop_form() -> bool {
    library_api::IS_DESKTOP_FORM || std::env::var("DAM_WEB_FORM").as_deref() == Ok("desktop")
}

pub fn router(state: AppState, dist: PathBuf) -> Router {
    let has_dist = dist.is_dir();
    let api = Router::new()
        .route("/app-info", get(app_info))
        .route("/capabilities", get(capabilities_api::capabilities))
        .route("/healthz", get(healthz))
        // KI-9/KI-10：在系统文件管理器中打开/选中（白名单 + 形态门控，见 os_api）
        .route("/os/reveal", post(os_api::reveal))
        // W2 导入链路
        .route("/import/scan", post(import_api::scan))
        .route("/import/start", post(import_api::start))
        .route("/import/resume", post(import_api::resume))
        .route("/import/retry-failed", post(import_api::retry_failed))
        .route("/import/cancel", post(import_api::cancel))
        .route("/import/status", get(import_api::status))
        .route("/import/batches", get(import_api::batches))
        // W2 图库读模型
        .route("/library/assets", get(library_api::assets))
        .route("/library/stats", get(library_api::stats))
        // DESKTOP-MISSING-STAT-1：全库失联现场核查（显式动作，结果落 settings）
        .route("/library/verify", post(library_api::verify))
        .route("/library/dirs", get(library_api::dirs))
        .route(
            "/library/assets/{asset_id}/tags",
            get(library_api::asset_tags),
        )
        .route(
            "/library/assets/{asset_id}/tag-results",
            get(library_api::asset_tag_results),
        )
        .route("/library/tags", get(library_api::tags))
        // task：标签总览（/tags 页面）——左栏 key 分组、样本图、批量合并/改名/删除
        .route("/library/tag-groups", get(library_api::tag_groups))
        .route("/library/tag-samples", post(library_api::tag_samples))
        .route("/library/tags/merge", post(library_api::tag_merge))
        .route(
            "/library/tags/merge-preview",
            post(library_api::tag_merge_preview),
        )
        .route("/library/tags/rename", post(library_api::tag_rename))
        .route("/library/tags/delete", post(library_api::tag_delete))
        // EXIF2：相机型号聚合（筛选下拉数据源）
        .route("/library/cameras", get(library_api::cameras))
        // E2 批量打分 / 拒选
        .route("/library/rate", post(library_api::rate))
        .route("/library/reject", post(library_api::reject))
        // P1-4 删除/撤销（软删进回收站；与去重裁决同一套 svc-library 语义）
        .route("/library/delete", post(library_api::delete_assets))
        .route("/library/restore", post(library_api::restore_assets))
        // E4 批量打标签 / 色标 / 移动（逐条回执）
        .route("/library/tag", post(library_api::tag))
        .route("/library/untag", post(library_api::untag))
        .route("/library/color", post(library_api::color))
        .route("/library/move", post(library_api::move_assets))
        // MIGRATE-TO-LIBRARY：原地索引资产一键迁入库根（后台跑 + 进度快照 + 审计账本）
        .route(
            "/library/migrate-to-library",
            post(library_api::migrate_to_library),
        )
        .route(
            "/library/migrate-to-library/status",
            get(library_api::migrate_status),
        )
        // 迁移清单（审计证据 dump）
        .route("/library/migrate-audit", get(library_api::migrate_audit))
        // E2 导出（复用 D11 svc-library::export）
        .route("/export/start", post(export_api::start))
        .route("/export/cancel", post(export_api::cancel))
        .route("/export/status", get(export_api::status))
        // W5 去重（fileHash ∪ pHash 分组 + 组裁决 + 回收站恢复）
        .route("/dedup/groups", get(dedup_api::groups))
        .route("/dedup/resolve", post(dedup_api::resolve))
        .route("/dedup/trash", get(dedup_api::trash))
        .route("/dedup/restore", post(dedup_api::restore))
        // KI-2b：存量一次性补判（可重入、分批；判重塔向量 → dup_edges）
        .route("/dedup/backfill", post(dedup_api::backfill))
        // KI-2 / KI-7：组级忽略名单（刷新后不复现）、库外只读扫描、报告导出
        .route("/dedup/ignore", post(dedup_api::ignore))
        .route("/dedup/unignore", post(dedup_api::unignore))
        .route("/dedup/scan", post(dedup_api::scan))
        .route("/dedup/export", get(dedup_api::export))
        // KI-11：回收站 → 永久删除（门控 physicalDelete；只接受 deleted=1）
        .route("/dedup/purge", post(dedup_api::purge))
        // W2 媒体服务（assetId 索引，无路径逃逸；原图白名单根校验）
        .route("/media/thumb/{asset_id}", get(media::thumb))
        .route("/media/original/{asset_id}", get(media::original))
        // W3 推理设置（provider 配置 + 连接测试）
        .route(
            "/settings/inference",
            get(settings_api::get_inference).put(settings_api::put_inference),
        )
        .route("/settings/embed/test", post(settings_api::test_embed))
        // EXIF2：元数据回填 + 设置（GPS 档位默认 off）
        .route("/exif/backfill", post(exif_api::backfill))
        .route("/exif/retry-failed", post(exif_api::retry_failed))
        .route("/exif/status", get(exif_api::status))
        .route(
            "/settings/exif",
            get(settings_api::get_exif).put(settings_api::put_exif),
        )
        // 模型获取入口（卡 task）：manifest 落盘状态 / 单飞下载 / 进度轮询
        .route("/models", get(model_api::list))
        .route("/models/download/status", get(model_api::status))
        .route("/models/{name}/download", post(model_api::download))
        // W4 打标设置（D9/R7：预设表 / Ollama 扫描 / 连接测试）
        .route("/settings/tag/presets", get(settings_api::tag_presets))
        .route("/settings/tag/scan", get(settings_api::tag_scan))
        .route("/settings/tag/test", post(settings_api::test_tag))
        // W3 搜索（文搜图 / 库内找相似 / 上传图搜索 / 可用性总览）
        .route("/search/text", post(search_api::text))
        .route("/search/similar", post(search_api::similar))
        .route("/search/image", post(search_api::by_image))
        .route("/search/status", get(search_api::status))
        // W3 嵌入队列（建向量索引：增量 / 全库重嵌入 / 进度 / 取消 / 失败重试）
        .route("/embed/start", post(embed_api::start))
        .route("/embed/cancel", post(embed_api::cancel))
        .route("/embed/status", get(embed_api::status))
        .route("/embed/failed", get(embed_api::failed))
        .route("/embed/retry-failed", post(embed_api::retry_failed))
        // W4 打标（模板队列 + 自定义任务 + 进度/取消/失败重试）；nest 挂 /api/tag/*
        .nest("/tag", tag_api::routes())
        // C6 合规审核（行业包管理 + 待人工队列 + 误判反馈）；nest 挂 /api/compliance/*
        .nest("/compliance", compliance_api::routes())
        // K07-I：插件参数声明（GET /api/plugins/{id}/schema，读包内 schema、不启子进程）。
        // 界面按它渲染唯一一份 ParamForm；参数仍在 /api/compliance/judge 的 body 里提交。
        .route(
            "/plugins/{id}/schema",
            get(compliance_api::plugin_params_schema),
        )
        // N1 规格（卡 task）：/api/specs* —— 规格 = 选规则 + 阈值 + 动作链 + 输出 + 参数 diff。
        // 规格不安装不分发；judge 用 `specId` 引用它（替代临时参数堆）。
        .nest("/specs", spec_api::routes())
        // N1：`POST /api/jobs` = 建一个执行任务，**接受 `specId`**（设计口径 §7 的入口名）。
        // 现阶段唯一会建任务的执行就是合规判定，故这里直通 judge（同一套入队/参数合并/快照路径）；
        // 处理类动作链（process kind）落地后再按 `kind` 分流，不另起第二条链路。
        .route("/jobs", post(compliance_api::judge))
        // OCR-E 文字识别队列（同 nest 语义 /api/ocr/*）
        .nest("/ocr", ocr_api::routes())
        // E3 智能文件夹（保存搜索：命名筛选条件 CRUD，结果动态现查）
        .route(
            "/smart-folders",
            get(smart_folder_api::list).post(smart_folder_api::create),
        )
        .route(
            "/smart-folders/{id}",
            get(smart_folder_api::get)
                .put(smart_folder_api::update)
                .delete(smart_folder_api::remove),
        )
        .with_state(state.clone());

    // /healthz 同时暴露在顶层（容器探活约定：不带 /api 前缀）
    let mut app = Router::new()
        .nest("/api", api)
        .route("/healthz", get(healthz))
        // W7：一次性 token 引导（桌面壳启动导航；web 容器 token=None 时交换恒拒绝但路由在）
        .route("/auth/boot", get(auth::boot))
        .with_state(state.clone());
    if has_dist {
        let index = dist.join("index.html");
        let serve = tower_http::services::ServeDir::new(&dist)
            .fallback(tower_http::services::ServeFile::new(index));
        app = app.fallback_service(serve);
    } else {
        app = app.fallback(get(|| async {
            (
                StatusCode::NOT_FOUND,
                "web/dist not built — run `pnpm -C web build` (dev: `pnpm -C web dev` + vite proxies /api)",
            )
                .into_response()
        }));
    }
    // W7：鉴权中间件最外层（豁免 /healthz 与 /auth/*；token 未配置时直通）
    app.layer(axum::middleware::from_fn_with_state(
        state,
        auth::require_session,
    ))
}

/// DB 连通性探测：`SELECT 1`。
async fn healthz(State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.db.lock().await;
    let db_ok = conn
        .query_row("SELECT 1", [], |r| r.get::<_, i64>(0))
        .is_ok();
    let code = if db_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        code,
        Json(
            json!({ "status": if db_ok { "ok" } else { "degraded" }, "db": if db_ok { "ok" } else { "fail" } }),
        ),
    )
}

/// 版本 / 模型根 / registry 概要。
async fn app_info(State(state): State<AppState>) -> impl IntoResponse {
    let models_root = state.models_root.clone();
    let registry = core_config::load_registry(None).ok();
    let models = registry
        .as_ref()
        .map(|r| r.models.iter().map(|m| m.name.clone()).collect::<Vec<_>>())
        .unwrap_or_default();
    Json(json!({
        "name": "dam-web",
        "version": env!("CARGO_PKG_VERSION"),
        "dataDir": state.data_dir,
        "modelsRoot": models_root,
        "registryModels": models,
    }))
}

/// 打开（或创建）库数据库。路径由 env `DAM_WEB_DATA` 指定，默认 `./data/`。
fn open_db(data_dir: &std::path::Path) -> core_storage::Result<rusqlite::Connection> {
    std::fs::create_dir_all(data_dir).ok();
    core_storage::open(&data_dir.join("dam.db"))
}

/// DESKTOP-MISSING-STAT-1：后台跑一次全库失联现场核查（独立连接，不阻塞启动）。
///
/// 独立连接的理由：WAL 多连接本就安全（导入线程同款做法），而 prepare() 在桌面壳
/// 主线程上执行 —— 大库全量 stat 放到前台会推迟窗口可交互。
/// 失败只记日志：核查不成功不能让服务起不来，stats 会显式报 `missingStale`（不撒谎）。
fn spawn_startup_missing_check(data_dir: std::path::PathBuf) {
    let spawned = std::thread::Builder::new()
        .name("missing-check".into())
        .spawn(move || match open_db(&data_dir) {
            Ok(conn) => match svc_import::verify_missing(&conn) {
                Ok(r) => eprintln!(
                    "dam-web: missing check checked={} missing={} notFound={} unreadable={} recovered={} {}ms",
                    r.checked, r.missing, r.notFound, r.unreadable, r.recovered, r.durationMs
                ),
                Err(e) => eprintln!("dam-web: missing check failed: {e}"),
            },
            Err(e) => eprintln!("dam-web: missing check open db failed: {e}"),
        });
    if let Err(e) = spawned {
        eprintln!("dam-web: missing check spawn failed: {e}");
    }
}

/// 测试用路由：不带前端产物（静态服务分支关闭，只测 /api/*）。
#[doc(hidden)]
pub fn router_without_dist(state: AppState) -> Router {
    router(state, PathBuf::from("/nonexistent-dist-for-tests"))
}

/// 集成测试（tests/*.rs）用的最小构造器，避免重复搭 state。
#[doc(hidden)]
pub mod test_support {
    use super::*;

    /// 用已打开的库连接 + 数据目录组装 AppState（不带引导 wizard / 导入任务状态）。
    pub fn state(data_dir: PathBuf, conn: rusqlite::Connection) -> AppState {
        AppState {
            db: Arc::new(Mutex::new(conn)),
            thumbs_root: data_dir.join("thumbs"),
            models_root: data_dir.clone(),
            auth: Arc::new(auth::Auth::new(None)),
            data_dir,
            import: Arc::new(import_api::ImportState::new()),
            embed: Arc::new(embed_api::EmbedState::new()),
            tag: Arc::new(tag_api::TagState::new()),
            ocr: Arc::new(ocr_api::OcrState::new()),
            provider: Arc::new(infer::ProviderCache::new()),
            export: Arc::new(export_api::ExportState::new()),
            compliance: Arc::new(compliance_api::ComplianceRunState::new()),
            verify: Arc::new(library_api::VerifyState::new()),
            models: Arc::new(model_api::ModelDownloadState::new()),
            pack_runtime: PackRuntime::default(),
        }
    }
}

/// W7：可嵌入服务配置（桌面壳 / 容器 run() 共用同一装配路径）。
pub struct ServeConfig {
    /// 库数据目录（dam.db / thumbs / models 默认落点）
    pub data_dir: PathBuf,
    /// 前端产物目录；None → 静态服务关闭（dev 由 vite 反代 /api）
    pub dist: Option<PathBuf>,
    /// W7：一次性引导 token；None = 关闭鉴权（web 容器 / dev 反代）
    pub token: Option<String>,
    /// R37：桌面壳注入的鉴权句柄（WebView2 崩溃重建窗口时要进程内重新签发引导 token）。
    /// Some 时以它为准（`token` 忽略）；None = 由 `token` 现场构造（容器 / 测试路径不变）。
    pub auth: Option<Arc<auth::Auth>>,
    /// 首启 seeding 的默认 embedder（桌面 = siglip2 onnx；None → env DAM_WEB_EMBEDDER → web 默认远程）
    pub default_embedder: Option<String>,
    /// 生效模型根（appData 式根，权重 `<root>/models/<name>`；None = `{data_dir}`）
    pub models_root: Option<PathBuf>,
    /// KI-28：包内资源根（桌面壳的 `<resource_dir>`）。用于解析内置 python runtime 与
    /// 随包启动器；None = 容器/开发形态（回落 `python3` + 仓库 `tools/`）。
    pub resource_dir: Option<PathBuf>,
}

/// 同步装配：开库 + 迁移 + 首启 seeding + 组路由。
/// 桌面壳在主线程调它（DB 就绪后再起服务线程，窗口导航不会撞 connection refused）。
pub fn prepare(cfg: &ServeConfig) -> Result<Router, String> {
    let conn = open_db(&cfg.data_dir).map_err(|e| e.to_string())?;
    // CMP-JOB-01 启动自愈：上一个进程死在判定中间 → 那些 `status='running'` 的执行永远收不了工，
    // 界面会显示一个不动的进度条。这里按队列事实回填计数并落 `interrupted`
    // （未执行的 task 留在 pending，下次开始判定会被认领续跑；迁移前 runId IS NULL 的老行不动）。
    {
        let runs = core_storage::compliance_runs_sqlite::SqliteComplianceRunRepo::new(&conn);
        match core_storage::compliance_runs::ComplianceRunRepo::interrupt_stale(
            &runs,
            &svc_pipeline::COMPLIANCE_KINDS,
        ) {
            Ok(0) => {}
            Ok(n) => eprintln!("dam-web: {n} 个未收工的合规执行已标记 interrupted"),
            Err(e) => eprintln!("dam-web: 合规执行启动自愈失败: {e}"),
        }
    }
    match infer::seed_defaults_opts(&conn, cfg.default_embedder.as_deref()) {
        Ok(true) => eprintln!("dam-web: seeded default inference settings"),
        Ok(false) => {}
        Err(e) => eprintln!("dam-web: seed inference settings failed: {e}"),
    }
    let state = AppState {
        db: Arc::new(Mutex::new(conn)),
        thumbs_root: cfg.data_dir.join("thumbs"),
        models_root: cfg
            .models_root
            .clone()
            .unwrap_or_else(|| cfg.data_dir.clone()),
        auth: cfg
            .auth
            .clone()
            .unwrap_or_else(|| Arc::new(auth::Auth::new(cfg.token.clone()))),
        data_dir: cfg.data_dir.clone(),
        import: Arc::new(import_api::ImportState::new()),
        embed: Arc::new(embed_api::EmbedState::new()),
        tag: Arc::new(tag_api::TagState::new()),
        ocr: Arc::new(ocr_api::OcrState::new()),
        provider: Arc::new(infer::ProviderCache::new()),
        export: Arc::new(export_api::ExportState::new()),
        compliance: Arc::new(compliance_api::ComplianceRunState::new()),
        verify: Arc::new(library_api::VerifyState::new()),
        models: Arc::new(model_api::ModelDownloadState::new()),
        pack_runtime: PackRuntime::resolve(cfg.resource_dir.as_deref()),
    };
    // KI-28：运行时缺失是「装了但插件起不来」的根因，启动日志必须留一行（真机排障第一现场）
    if let Some(py) = &state.pack_runtime.python {
        eprintln!("dam-web: 插件运行时 python = {}", py.display());
    } else if is_desktop_form() {
        eprintln!("dam-web: 警告 — 桌面形态未解析到内置 python 运行时（资源包不完整？）");
    }
    // DESKTOP-MISSING-STAT-1：启动即做一次全库失联现场核查。
    // 为什么必须在这里：assets.missing 只由 watcher 维护，进程没在跑的那段时间里源目录
    // 被外部清空（/tmp 清理、盘拔掉）永远不会被标记 → stats 会长期撒谎报 missing:0。
    // 后台线程 + 独立连接：不阻塞窗口/服务就绪（prepare 在桌面壳主线程上跑）。
    spawn_startup_missing_check(cfg.data_dir.clone());
    let dist = cfg
        .dist
        .clone()
        .unwrap_or_else(|| PathBuf::from("/nonexistent-dist-for-tests"));
    Ok(router(state, dist))
}

/// 便捷入口：绑定 listener → prepare → serve（无信号处理，适合嵌入宿主进程）。
pub async fn serve_on(listener: tokio::net::TcpListener, cfg: ServeConfig) -> Result<(), String> {
    let app = prepare(&cfg)?;
    axum::serve(listener, app).await.map_err(|e| e.to_string())
}

/// 已装配路由的服务（桌面壳：主线程 prepare，服务线程 serve）。
pub async fn serve_prepared(listener: tokio::net::TcpListener, app: Router) -> std::io::Result<()> {
    axum::serve(listener, app).await
}

/// 入口（web 容器）：解析 env → prepare → 绑定监听 → 优雅退出（SIGTERM/SIGINT）。
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = std::env::var("DAM_WEB_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./data"));
    let port: u16 = std::env::var("DAM_WEB_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    // 容器部署需 0.0.0.0（端口映射只绑宿主 127.0.0.1）；裸机默认 127.0.0.1（R4 单用户）
    let bind_host = std::env::var("DAM_WEB_BIND").unwrap_or_else(|_| "127.0.0.1".into());
    // WIN-PRE2 验收钩子：DAM_WEB_FORM=desktop 复刻桌面壳的 seeding 形态信号
    // （ServeConfig.default_embedder=Some），让无 GUI 的容器/CI 也能验收桌面默认口径。
    let desktop_form = std::env::var("DAM_WEB_FORM").as_deref() == Ok("desktop");

    let cfg = ServeConfig {
        data_dir,
        dist: Some(web_dist_dir()),
        token: std::env::var("DAM_WEB_TOKEN")
            .ok()
            .filter(|s| !s.is_empty()),
        // 容器/裸机形态没有 WebView2，也没有重建窗口的腿：鉴权句柄由 token 现场构造
        auth: None,
        default_embedder: if desktop_form {
            Some(core_config::DEFAULT_EMBEDDER.to_string())
        } else {
            None
        },
        models_root: std::env::var("DAM_WEB_MODELS_ROOT")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
        // KI-28：容器想要验「随包 runtime 解析」时用 env 指向资源根（默认 None = 系统 python3）
        resource_dir: std::env::var("DAM_RESOURCE_DIR")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
    };

    let addr: SocketAddr = format!("{bind_host}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let dist_display = cfg.dist.clone().unwrap_or_default();
    let app = prepare(&cfg)?;
    eprintln!(
        "dam-web listening on http://{addr} (dist: {})",
        dist_display.display()
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt; // Router::oneshot

    async fn test_state() -> AppState {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "dam-web-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let conn = open_db(&dir).expect("open db");
        AppState {
            db: Arc::new(Mutex::new(conn)),
            thumbs_root: dir.join("thumbs"),
            models_root: dir.clone(),
            auth: Arc::new(auth::Auth::new(None)),
            data_dir: dir,
            import: Arc::new(import_api::ImportState::new()),
            embed: Arc::new(embed_api::EmbedState::new()),
            tag: Arc::new(tag_api::TagState::new()),
            ocr: Arc::new(ocr_api::OcrState::new()),
            provider: Arc::new(infer::ProviderCache::new()),
            export: Arc::new(export_api::ExportState::new()),
            compliance: Arc::new(compliance_api::ComplianceRunState::new()),
            verify: Arc::new(library_api::VerifyState::new()),
            models: Arc::new(model_api::ModelDownloadState::new()),
            pack_runtime: PackRuntime::default(),
        }
    }

    #[tokio::test]
    async fn healthz_ok() {
        let app = router(test_state().await, std::env::temp_dir()); // 无 dist → fallback 404 分支
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// W3：迁移 0004 的 ('inference','null') 占位必须被 seeding 覆盖
    /// （否则 web 首启 active_embedder 落到桌面默认 siglip2 onnx，web 不可用）。
    #[tokio::test]
    async fn w3_seed_overrides_null_placeholder() {
        let state = test_state().await;
        let conn = state.db.lock().await;
        core_storage::settings::set_raw(&conn, core_config::INFERENCE_SETTINGS_KEY, "null")
            .unwrap();
        assert!(
            infer::seed_defaults_if_absent(&conn).unwrap(),
            "null 占位应触发 seeding"
        );
        let s = infer::load_settings(&conn);
        assert_eq!(s.active_embedder, "openai-compat-embedding");
        assert!(!s.embedding.base_url.is_empty());
        assert!(
            !infer::seed_defaults_if_absent(&conn).unwrap(),
            "已有真实配置 → 不覆盖"
        );
    }

    /// KI-1：仅图像向量 spec（去重塔）被提为活跃 embedder 必须 400 拒绝，
    /// 错误码 no_text_modality（设置页保存提示直接可读）。不静默降级成「文搜图无结果」。
    #[tokio::test]
    async fn embeddings_test_put_inference_rejects_image_only_embedder() {
        let state = test_state().await;
        let mut s: core_config::InferenceSettings = {
            let conn = state.db.lock().await;
            infer::load_settings(&conn)
        };
        s.active_embedder = core_config::DEDUP_EMBEDDER.to_string();
        let body = serde_json::to_vec(&s).unwrap();
        let app = router_without_dist(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/settings/inference")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let out = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let j: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(j["code"], "no_text_modality", "body={j}");
        assert!(
            j["error"]
                .as_str()
                .unwrap_or_default()
                .contains("仅图像向量"),
            "错误文案要能直接给用户看: {j}"
        );
    }

    /// WIN-PRE2：桌面形态（default_embedder=Some）首启 tag 默认 = DeepSeek 公网端点
    /// （base_url/model 预填、api_key 留空引导用户去设置页贴 Key；不含容器口径）。
    #[test]
    fn win_pre2_desktop_seed_tag_defaults_to_deepseek() {
        std::env::remove_var("DAM_WEB_TAG_BASE_URL");
        std::env::remove_var("DAM_WEB_TAG_MODEL");
        std::env::remove_var("DAM_WEB_TAG_ENABLED");
        let dir = std::env::temp_dir().join(format!("dam-web-winpre2-d-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let conn = open_db(&dir).expect("open db");
        infer::seed_defaults_opts(&conn, Some("siglip2-base-p224-onnx")).unwrap();
        let s = infer::load_settings(&conn);
        assert_eq!(s.tag.provider, core_config::TAG_PROVIDER_DEEPSEEK);
        assert_eq!(s.tag.base_url, "https://api.deepseek.com/v1");
        assert_eq!(s.tag.model, "deepseek-flash");
        assert!(s.tag.enabled, "桌面默认开箱即用语义不变");
        assert!(s.tag.api_key.is_empty(), "Key 用户自填，默认空");
        assert!(!s.tag.base_url.contains("host.docker.internal"));
        assert!(
            s.tag.configured(),
            "base_url+model 预填即 configured；UI 引导按 hasKey 判定"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// WIN-PRE2 回归钉：web/容器形态首启 tag 默认不变（dam-ai 容器口径）。
    #[test]
    fn win_pre2_web_seed_tag_defaults_unchanged() {
        std::env::remove_var("DAM_WEB_TAG_BASE_URL");
        std::env::remove_var("DAM_WEB_TAG_MODEL");
        let dir = std::env::temp_dir().join(format!("dam-web-winpre2-w-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let conn = open_db(&dir).expect("open db");
        infer::seed_defaults_if_absent(&conn).unwrap();
        let s = infer::load_settings(&conn);
        assert_eq!(s.tag.provider, core_config::TAG_PROVIDER_DAMAI);
        assert_eq!(s.tag.base_url, "http://host.docker.internal:8091/v1");
        assert_eq!(s.tag.model, "qwen3.5-4b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// WIN-PRE2：/api/tag/status 的 provider 摘要带 hasKey；桌面默认 prefill 下
    /// configured=true 但 hasKey=false —— 这正是 UI 显式引导（去贴 Key）的判定依据。
    #[tokio::test]
    async fn win_pre2_status_provider_has_key_flag() {
        let state = test_state().await;
        {
            let conn = state.db.lock().await;
            infer::seed_defaults_opts(&conn, Some("siglip2-base-p224-onnx")).unwrap();
        }
        let app = router_without_dist(state);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/tag/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let p = &j["provider"];
        assert_eq!(p["provider"], "deepseek", "桌面形态默认 provider");
        assert_eq!(p["configured"], true, "prefill base_url+model");
        assert_eq!(p["hasKey"], false, "Key 未填 → UI 引导");
    }

    #[tokio::test]
    async fn app_info_has_version_and_models() {
        let app = router(test_state().await, std::env::temp_dir());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/app-info")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["name"], "dam-web");
        assert!(!v["registryModels"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn library_assets_and_stats_empty_ok() {
        let app = router(test_state().await, std::env::temp_dir());
        for uri in [
            "/api/library/assets",
            "/api/library/stats",
            "/api/library/dirs",
        ] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "GET {uri}");
        }
    }

    /// DESKTOP-DIRS-TREE-1：Windows 盘符路径必须能取到父目录（写死 '/' 时目录树恒空）。
    #[tokio::test]
    async fn library_dirs_groups_windows_paths() {
        let state = test_state().await;
        {
            let conn = state.db.lock().await;
            let rows = [
                (1i64, r"D:\pics\2024\a.jpg"),
                (2, r"D:\pics\2024\b.jpg"),
                (3, r"D:\pics\2025\c.jpg"),
                (4, r"D:\pics\top.jpg"),
                (5, "/home/me/pics/posix.jpg"),
            ];
            for (id, abs) in rows {
                conn.execute(
                    "INSERT INTO assets (assetId, absPath, fileName, fileExt, fileSz, mtime, importedAt) \
                     VALUES (?1, ?2, ?3, 'jpg', 10, 0, 0)",
                    rusqlite::params![id, abs, abs.rsplit(['/', '\\']).next().unwrap_or(abs)],
                )
                .unwrap();
            }
            // 真机现场：批次根带尾分隔符
            conn.execute(
                "INSERT INTO import_batches (batchId, rootPath, startedAt) VALUES (1, ?1, 0)",
                [r"D:\pics\"],
            )
            .unwrap();
        }
        let app = router(state, std::env::temp_dir());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/library/dirs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let items = v["dirs"].as_array().unwrap();
        let count_of = |d: &str| -> i64 {
            items
                .iter()
                .find(|i| i["dir"] == d)
                .map(|i| i["count"].as_i64().unwrap())
                .unwrap_or_else(|| panic!("缺目录 {d}: {v}"))
        };
        assert_eq!(count_of(r"D:\pics\2024"), 2, "Windows 子目录计数");
        assert_eq!(count_of(r"D:\pics\2025"), 1);
        assert_eq!(count_of(r"D:\pics"), 1, "根目录下的文件归到根");
        assert_eq!(count_of("/home/me/pics"), 1, "POSIX 路径不受影响");
        // 批次根去尾分隔符（前端拿它做相对标签/根匹配）
        assert_eq!(v["roots"][0], r"D:\pics", "rootPath 尾分隔符需去掉: {v}");
    }

    #[tokio::test]
    async fn import_scan_rejects_non_absolute() {
        let app = router(test_state().await, std::env::temp_dir());
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/import/scan")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"path":"relative/dir"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn import_start_conflict_when_running() {
        let state = test_state().await;
        state
            .import
            .running
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let app = router(state, std::env::temp_dir());
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/import/start")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"path":"/definitely/not/a/dir","storeMode":"reference"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn media_unknown_asset_is_404() {
        let app = router(test_state().await, std::env::temp_dir());
        for uri in ["/api/media/thumb/99999", "/api/media/original/99999"] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "GET {uri}");
        }
    }

    /// E1：minRating 过滤（星级下限）+ /api/library/tags 标签频次。
    #[tokio::test]
    async fn e1_min_rating_filter_and_tags() {
        let state = test_state().await;
        {
            let conn = state.db.lock().await;
            for (id, rating) in [(1, 5), (2, 4), (3, 2)] {
                conn.execute(
                    "INSERT INTO assets (assetId, absPath, fileName, fileExt, fileSz, mtime, importedAt, starRating)
                     VALUES (?1, ?2, ?3, 'jpg', 100, 0, ?4, ?5)",
                    rusqlite::params![
                        id,
                        format!("/tmp/e1/p{id}.jpg"),
                        format!("p{id}.jpg"),
                        id, // importedAt 递增 → 列表顺序 p3,p2,p1
                        rating
                    ],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO tag (assetId, value, origin, createdAt) VALUES (1, '狗', 'manual', 1), (2, '狗', 'manual', 2), (3, '猫', 'manual', 3)",
                [],
            )
            .unwrap();
        }
        let app = router(state, std::env::temp_dir());

        // minRating=4 → p1(5★) + p2(4★)，按导入时间倒序
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/library/assets?minRating=4&limit=10")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["total"], 2, "minRating=4 → 2 张");
        let names: Vec<&str> = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["fileName"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["p2.jpg", "p1.jpg"], "导入时间倒序");

        // minRating 与 tag 组合：狗(1,2) ∩ ≥4 → 2
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/library/assets?minRating=4&tag=%E7%8B%97&limit=10")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["total"], 2, "minRating+tag 组合");

        // 标签频次：狗 2 / 猫 1，按热度降序
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/library/tags")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let tags = v.as_array().unwrap();
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0]["value"], "狗");
        assert_eq!(tags[0]["count"], 2);
        assert_eq!(tags[1]["value"], "猫");
        assert_eq!(tags[1]["count"], 1);
    }

    /// 1×1 RGBA PNG（红/绿各一）。D3 起导入会做解码校验，「夹具」必须是**真图片**：
    /// 随便塞 b"xxx" 的 .jpg 现在会被判 failed（这正是 D3 要的行为）。
    const PNG_1X1_RED: [u8; 70] = [
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0,
        0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0, 0x0D, 0x49, 0x44,
        0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xF0, 0x1F, 0, 5, 0, 1, 0xFF, 0x89, 0x99,
        0x3D, 0x1D, 0, 0, 0, 0, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    const PNG_1X1_GREEN: [u8; 70] = [
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0,
        0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0, 0x0D, 0x49, 0x44,
        0x41, 0x54, 0x78, 0x9C, 0x63, 0x60, 0xF8, 0xCF, 0xF0, 0x1F, 0, 4, 1, 1, 0xFF, 0x71, 0xEB,
        0x47, 0xE5, 0, 0, 0, 0, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    /// W2 全链路：临时目录造图（2 张不同 + 1 张哈希重复）→ import/start → 等完成 →
    /// library/assets 可见（去重生效）→ thumb 200 + original 200；
    /// 再验原图白名单外 403（伪造 absPath 指向 /etc）。
    #[tokio::test]
    async fn w2_import_full_flow() {
        let state = test_state().await;
        let src_dir = state.data_dir.join("src-photos");
        std::fs::create_dir_all(&src_dir).unwrap();
        // 1x1 RGBA png（红/绿各一张，绿图再复制一份验哈希去重）
        let red = PNG_1X1_RED;
        let green = PNG_1X1_GREEN;
        std::fs::write(src_dir.join("red.png"), red).unwrap();
        std::fs::write(src_dir.join("green.png"), green).unwrap();
        std::fs::write(src_dir.join("green-dup.png"), green).unwrap();
        let app = router(state.clone(), std::env::temp_dir());
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/import/start")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"path":{},"storeMode":"reference"}}"#,
                        serde_json::to_string(src_dir.to_str().unwrap()).unwrap()
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        // 等导入完成（spawn_blocking 线程）
        let mut waited = 0;
        while state
            .import
            .running
            .load(std::sync::atomic::Ordering::SeqCst)
            && waited < 150
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            waited += 1;
        }
        assert!(
            !state
                .import
                .running
                .load(std::sync::atomic::Ordering::SeqCst),
            "import should finish"
        );
        let outcome = state
            .import
            .outcome
            .lock()
            .unwrap()
            .clone()
            .expect("outcome");
        assert_eq!(outcome.status, "done");
        assert_eq!(outcome.imported, 2, "2 unique assets imported");
        assert_eq!(outcome.duplicates, 1, "1 hash-duplicate skipped");

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/library/assets?limit=10")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["total"], 2);
        let aid = v["items"][0]["assetId"].as_i64().unwrap();

        for uri in [
            format!("/api/media/thumb/{aid}?size=256"),
            format!("/api/media/thumb/{aid}?size=1024"),
            format!("/api/media/original/{aid}"),
        ] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "GET {uri}");
        }

        // 白名单外原图：把资产 absPath 改到白名单外（/etc 下）→ 403
        {
            let conn = state.db.lock().await;
            conn.execute(
                "UPDATE assets SET absPath = '/etc/hosts', missing = 0 WHERE assetId = ?1",
                rusqlite::params![aid],
            )
            .unwrap();
        }
        let res = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/media/original/{aid}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    /// D2：`/api/import/status` 要带失败明细；`/api/import/retry-failed` 只重跑失败项
    /// （含 paths 空 / 越界 400、批次不存在 404 三条守卫）。
    #[cfg(unix)]
    #[tokio::test]
    async fn d2_import_status_failed_items_and_retry_failed() {
        let state = test_state().await;
        let src = state.data_dir.join("src-fails");
        std::fs::create_dir_all(&src).unwrap();
        // D3：真图片才能入库（junk 内容会被解码校验判 failed）
        std::fs::write(src.join("ok.jpg"), PNG_1X1_RED).unwrap();
        // 悬空符号链接：扫描期算图片，处理期读不到 → 失败（真机上是 icacls deny / 占用）
        std::os::unix::fs::symlink(src.join("missing-target.jpg"), src.join("denied-read.jpg"))
            .unwrap();
        let app = router(state.clone(), std::env::temp_dir());

        let wait_done = || {
            let st = state.clone();
            async move {
                let mut waited = 0;
                while st.import.running.load(std::sync::atomic::Ordering::SeqCst) && waited < 300 {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    waited += 1;
                }
                assert!(
                    !st.import.running.load(std::sync::atomic::Ordering::SeqCst),
                    "import should finish"
                );
            }
        };

        let (code, _) = post_json(
            &app,
            "/api/import/start",
            json!({ "path": src.to_str().unwrap(), "storeMode": "reference" }),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        wait_done().await;

        // 失败明细：哪张、为什么，都在 status 里
        let st = get_json(&app, "/api/import/status").await;
        assert_eq!(st["outcome"]["status"], "done");
        assert_eq!(st["outcome"]["imported"], 1);
        assert_eq!(st["outcome"]["failed"], 1);
        let items = st["outcome"]["failedItems"]
            .as_array()
            .expect("failedItems");
        assert_eq!(items.len(), 1, "{st}");
        assert_eq!(items[0]["fileName"], "denied-read.jpg");
        assert!(
            items[0]["error"].as_str().is_some_and(|e| !e.is_empty()),
            "失败原因不得为空：{st}"
        );
        let failed_path = items[0]["path"].as_str().expect("path").to_string();
        let batch_id = st["outcome"]["batchId"].as_i64().unwrap();

        // 修好源文件 → 只重试这一张
        std::fs::write(src.join("missing-target.jpg"), PNG_1X1_GREEN).unwrap();
        let (code, body) = post_json(
            &app,
            "/api/import/retry-failed",
            json!({ "batchId": batch_id, "paths": [failed_path] }),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(body["paths"], 1);
        wait_done().await;
        let st = get_json(&app, "/api/import/status").await;
        assert_eq!(st["outcome"]["failed"], 0, "重试后不再有失败：{st}");
        assert_eq!(st["outcome"]["imported"], 1, "{st}");
        assert_eq!(st["outcome"]["total"], 1, "重试口径 total=待重试张数");

        // 守卫：空 paths / 越界路径 → 400；批次不存在 → 404
        assert_eq!(
            post_json(
                &app,
                "/api/import/retry-failed",
                json!({ "batchId": batch_id, "paths": [] })
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            post_json(
                &app,
                "/api/import/retry-failed",
                json!({ "batchId": batch_id, "paths": ["/etc/passwd"] })
            )
            .await
            .0,
            StatusCode::BAD_REQUEST,
            "批次根之外的路径必须拒绝"
        );
        assert_eq!(
            post_json(
                &app,
                "/api/import/retry-failed",
                json!({ "batchId": 999999, "paths": [failed_path] })
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }

    /// 测试辅助：等当前导入跑完（最多 ~6s）。
    async fn wait_import_done(state: &AppState) {
        let mut waited = 0;
        while state
            .import
            .running
            .load(std::sync::atomic::Ordering::SeqCst)
            && waited < 300
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            waited += 1;
        }
        assert!(
            !state
                .import
                .running
                .load(std::sync::atomic::Ordering::SeqCst),
            "import should finish"
        );
    }

    /// D3：扩展名对但内容不是图片 → **failed（进失败清单），不落资产**。
    ///
    /// 真机复现体：`C:\Users\Administrator\dam-usability2\c-broken.jpg`（21 字节
    /// "not an image at all"）被算成「导入成功 1 张」，落成 assetId 28（width=0、
    /// thumb 永远 500）。修法：入库前解码校验（svc_import::probe_image）。
    #[tokio::test]
    async fn d3_junk_jpg_fails_import_and_leaves_no_asset() {
        let state = test_state().await;
        let src = state.data_dir.join("d3-src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("good1.jpg"), PNG_1X1_RED).unwrap();
        std::fs::write(src.join("good2.jpg"), PNG_1X1_GREEN).unwrap();
        std::fs::write(src.join("c-broken.jpg"), b"not an image at all").unwrap();
        // 扩展名不在白名单 → 连扫描都不进（与「坏图」是两种语义，这里一起验不混淆）
        std::fs::write(src.join("note.txt"), b"not an image at all").unwrap();
        let app = router(state.clone(), std::env::temp_dir());

        let (code, body) = post_json(
            &app,
            "/api/import/start",
            json!({ "path": src.to_str().unwrap(), "storeMode": "reference" }),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        wait_import_done(&state).await;

        let st = get_json(&app, "/api/import/status").await;
        assert_eq!(st["outcome"]["imported"], 2, "只有两张真图入库：{st}");
        assert_eq!(
            st["outcome"]["failed"], 1,
            "坏图必须计失败，不能算成功：{st}"
        );
        assert_eq!(st["outcome"]["total"], 3, "扫描只收图片扩展名：{st}");
        let items = st["outcome"]["failedItems"]
            .as_array()
            .expect("failedItems");
        assert_eq!(items.len(), 1, "{st}");
        assert_eq!(items[0]["fileName"], "c-broken.jpg", "{st}");
        assert!(
            items[0]["error"]
                .as_str()
                .is_some_and(|e| e.contains("无法解码")),
            "失败原因要明说「无法解码」：{st}"
        );

        let list = get_json(&app, "/api/library/assets?limit=50").await;
        assert_eq!(list["total"], 2, "坏图不得产生资产：{list}");
        assert!(
            list["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|it| it["fileName"] != "c-broken.jpg"),
            "{list}"
        );
        // 坏图没入库 → 「无法预览」计数天然是 0（两条口径都不撒谎）
        let broken = get_json(&app, "/api/library/assets?broken=1").await;
        assert_eq!(broken["total"], 0, "{broken}");
        let stats = get_json(&app, "/api/library/stats").await;
        assert_eq!(stats["broken"], 0, "{stats}");
    }

    /// D3（历史数据）：旧版本已静默入库的坏资产 —— 缩略图不再 500、能筛出来、
    /// 核查能把它判成「无法预览」，修好文件后核查能自动修复。
    ///
    /// 直接落库复刻 assetId 28 的形态（assets 行 + primary resource width=0），
    /// 因为新导入路径已经造不出这种资产了。
    #[tokio::test]
    async fn d3_legacy_broken_asset_thumb_degrades_filter_and_repairs() {
        let state = test_state().await;
        let dir = state.data_dir.join("d3-legacy");
        std::fs::create_dir_all(&dir).unwrap();
        let junk = dir.join("c-broken.jpg");
        std::fs::write(&junk, b"not an image at all").unwrap();
        let aid = {
            let conn = state.db.lock().await;
            let sz = std::fs::metadata(&junk).unwrap().len() as i64;
            conn.execute(
                "INSERT INTO assets (mediaType, absPath, fileName, fileExt, fileSz, mtime, importedAt)
                 VALUES ('image', ?1, 'c-broken.jpg', 'jpg', ?2, unixepoch(), unixepoch())",
                rusqlite::params![junk.to_str().unwrap(), sz],
            )
            .unwrap();
            let aid = conn.last_insert_rowid();
            let thumb = state
                .thumbs_root
                .join("256")
                .join(format!("{}", aid % 100))
                .join(format!("{aid}_256.webp"))
                .to_string_lossy()
                .into_owned();
            conn.execute(
                "INSERT INTO resource (assetId, role, ordinal, thumbPath, width, height, pHash)
                 VALUES (?1, 'primary', 0, ?2, 0, 0, NULL)",
                rusqlite::params![aid, thumb],
            )
            .unwrap();
            aid
        };
        let app = router(state.clone(), std::env::temp_dir());

        // ① 缩略图：200 + 真 PNG 占位图 + 降级头（修复前是 500）
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/media/thumb/{aid}?size=256"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "坏图缩略图不得 500");
        assert_eq!(
            res.headers()
                .get("x-dam-degraded")
                .map(|v| v.to_str().unwrap()),
            Some("1"),
            "降级必须显式可判（不是靠猜 200 的内容）"
        );
        assert_eq!(res.headers().get("content-type").unwrap(), "image/png");
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.starts_with(b"\x89PNG"), "占位图要是真图片字节");

        // ② 筛选 + 统计：同一口径，且非法取值 400
        let broken = get_json(&app, "/api/library/assets?broken=1").await;
        assert_eq!(broken["total"], 1, "{broken}");
        assert_eq!(broken["items"][0]["assetId"], aid);
        let ok = get_json(&app, "/api/library/assets?broken=0").await;
        assert_eq!(ok["total"], 0, "{ok}");
        let stats = get_json(&app, "/api/library/stats").await;
        assert_eq!(stats["broken"], 1, "{stats}");
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/library/assets?broken=2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::BAD_REQUEST,
            "非法 broken 值必须 400"
        );

        // ③ 核查：文件在盘上但解不开 → 判 broken（不是 missing、也不是 unreadable）
        let (code, r) = post_json(&app, "/api/library/verify", json!({})).await;
        assert_eq!(code, StatusCode::OK, "{r}");
        assert_eq!(r["broken"], 1, "存在但解不开要单独计数：{r}");
        assert_eq!(r["notFound"], 0, "{r}");
        assert_eq!(r["repaired"], 0, "{r}");
        assert!(
            r["samples"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["class"] == "unparseable"),
            "报告里要留可定位的样本：{r}"
        );
        let broken = get_json(&app, "/api/library/assets?broken=1").await;
        assert_eq!(broken["total"], 1, "核查后仍筛得到：{broken}");

        // ④ 文件被换成真图 → 再核查：修复尺寸 + 重生成缩略图，自动流出「无法预览」
        std::fs::write(&junk, PNG_1X1_RED).unwrap();
        let (_, r) = post_json(&app, "/api/library/verify", json!({})).await;
        assert_eq!(r["repaired"], 1, "换回真图后要自动修复：{r}");
        assert_eq!(r["broken"], 0, "{r}");
        let stats = get_json(&app, "/api/library/stats").await;
        assert_eq!(stats["broken"], 0, "{stats}");
        let list = get_json(&app, &format!("/api/library/assets?ids={aid}")).await;
        assert_eq!(list["items"][0]["width"], 1, "尺寸要修回来：{list}");
        assert_eq!(list["items"][0]["height"], 1, "{list}");
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/media/thumb/{aid}?size=256"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            res.headers().get("x-dam-degraded").is_none(),
            "修好后必须回真缩略图，不能继续发占位图"
        );
        assert_eq!(res.headers().get("content-type").unwrap(), "image/webp");
    }

    /// E2：批量打分 / 拒选写 API —— 落库 + 读模型可见 + 参数校验 400。
    #[tokio::test]
    async fn e2_rate_and_reject_persist() {
        let state = test_state().await;
        let app = router(state.clone(), std::env::temp_dir());
        let ids = seed_assets(&state, 3).await;
        let (a, b) = (ids[0], ids[1]);

        // 打分 2 张
        let res = post_json(&app, "/api/library/rate", json!({"ids":[a,b],"rating":4})).await;
        assert_eq!(res.0, StatusCode::OK);
        assert_eq!(res.1["updated"], 2);

        // 读模型（网格数据源）立即可见
        let list = get_json(&app, "/api/library/assets?limit=10").await;
        let items = list["items"].as_array().unwrap();
        let r: Vec<i64> = items
            .iter()
            .filter(|it| it["assetId"] == a || it["assetId"] == b)
            .map(|it| it["starRating"].as_i64().unwrap())
            .collect();
        assert_eq!(r, vec![4, 4], "打分实时反映到读模型");

        // 清除打分（0）+ 越界 400 + 空集 400
        assert_eq!(
            post_json(&app, "/api/library/rate", json!({"ids":[a],"rating":0}))
                .await
                .1["updated"],
            1
        );
        assert_eq!(
            post_json(&app, "/api/library/rate", json!({"ids":[a],"rating":6}))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            post_json(&app, "/api/library/rate", json!({"ids":[],"rating":3}))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );

        // 拒选 → approvalStatus=rejected（读模型可见）；再取消
        let res = post_json(
            &app,
            "/api/library/reject",
            json!({"ids":[a,b],"rejected":true}),
        )
        .await;
        assert_eq!(res.0, StatusCode::OK);
        let list = get_json(&app, "/api/library/assets?limit=10").await;
        let rej: Vec<&str> = list["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|it| it["assetId"] == a || it["assetId"] == b)
            .map(|it| it["approvalStatus"].as_str().unwrap())
            .collect();
        assert_eq!(rej, vec!["rejected", "rejected"]);

        let res = post_json(
            &app,
            "/api/library/reject",
            json!({"ids":[a],"rejected":false}),
        )
        .await;
        assert_eq!(res.1["updated"], 1);
        let list = get_json(&app, "/api/library/assets?limit=10").await;
        let one = list["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|it| it["assetId"] == a)
            .unwrap();
        assert_eq!(one["approvalStatus"], "approved", "取消拒选回到 approved");

        // 拒选是标记，不是删除：资产仍在读模型里
        assert_eq!(list["total"], 3);
        assert_eq!(
            post_json(&app, "/api/library/reject", json!({"ids":[]}))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }

    /// P1-4：删除/撤销 API —— Delete 键与工具栏按钮的后端。
    /// 语义断言：软删（读模型消失，回收站可见，源文件仍在磁盘）→ 撤销恢复 → 幂等/参数校验。
    #[tokio::test]
    async fn p1_delete_and_restore_round_trip() {
        let state = test_state().await;
        let app = router(state.clone(), std::env::temp_dir());
        let ids = seed_assets(&state, 3).await;
        let (a, b, c) = (ids[0], ids[1], ids[2]);

        // 删 2 张：返回实际置位行数
        let (st, body) = post_json(&app, "/api/library/delete", json!({"ids":[a,b]})).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["deleted"], 2);

        // 读模型（图库网格数据源）里消失，总数下降
        let list = get_json(&app, "/api/library/assets?limit=10").await;
        assert_eq!(list["total"], 1, "软删后读模型恒过滤 deleted=1");
        let left: Vec<i64> = list["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|it| it["assetId"].as_i64().unwrap())
            .collect();
        assert_eq!(left, vec![c]);

        // 回收站可见（去重页同一个列表接口）
        let trash = get_json(&app, "/api/dedup/trash?limit=50").await;
        let trashed: Vec<i64> = trash["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|it| it["assetId"].as_i64().unwrap())
            .collect();
        assert!(trashed.contains(&a) && trashed.contains(&b), "进了回收站");
        assert!(!trashed.contains(&c));

        // 源文件不动（删除是软删，绝不碰磁盘）
        {
            let conn = state.db.lock().await;
            let path: String = conn
                .query_row("SELECT absPath FROM assets WHERE assetId=?1", [a], |r| {
                    r.get(0)
                })
                .unwrap();
            assert!(
                std::path::Path::new(&path).exists(),
                "源文件必须还在：{path}"
            );
        }

        // 重复删幂等：已经是 deleted=1 的不再计数
        assert_eq!(
            post_json(&app, "/api/library/delete", json!({"ids":[a]}))
                .await
                .1["deleted"],
            0
        );

        // 撤销（toast 的「撤销删除」按钮打这条）
        let (st, body) = post_json(&app, "/api/library/restore", json!({"ids":[a,b]})).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["restored"], 2);
        let list = get_json(&app, "/api/library/assets?limit=10").await;
        assert_eq!(list["total"], 3, "撤销后全部回到读模型");

        // 参数校验：空集 / 非正整数 / 超上限一律 400
        for bad in [
            json!({"ids":[]}),
            json!({"ids":[0]}),
            json!({"ids":[-3]}),
            json!({"ids": (1..=901).collect::<Vec<i64>>()}),
        ] {
            assert_eq!(
                post_json(&app, "/api/library/delete", bad.clone()).await.0,
                StatusCode::BAD_REQUEST,
                "delete 应拒绝 {bad}"
            );
            assert_eq!(
                post_json(&app, "/api/library/restore", bad).await.0,
                StatusCode::BAD_REQUEST,
                "restore 应拒绝非法 ids"
            );
        }
    }

    /// KI-11：网页容器形态下 `/api/dedup/purge` 必须 400 + 人话（不伪造成功）；
    /// 入参校验同样在门之前挡住空集。桌面钩子形态的完整链路在
    /// `tests/ki11_purge_routes.rs`（独立进程，可安全设 DAM_WEB_FORM）。
    #[tokio::test]
    async fn ki11_purge_blocked_in_web_form() {
        if crate::is_desktop_form() {
            eprintln!("skip: 本进程为桌面形态（DAM_WEB_FORM=desktop），容器形态断言不适用");
            return;
        }
        let state = test_state().await;
        let app = router(state.clone(), std::env::temp_dir());
        let ids = seed_assets(&state, 2).await;
        // 先软删进回收站（门在位时也不该被 400 的是「空集」这类入参错）
        assert_eq!(
            post_json(&app, "/api/library/delete", json!({"ids":[ids[0]]}))
                .await
                .1["deleted"],
            1
        );
        assert_eq!(
            post_json(&app, "/api/dedup/purge", json!({"ids":[]}))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        let (st, body) = post_json(&app, "/api/dedup/purge", json!({"ids":[ids[0]]})).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "网页形态不许物理删除: {body}");
        let msg = body["error"].as_str().unwrap_or_default();
        assert!(
            msg.contains("网页版不支持彻底删除"),
            "必须是能看懂的原因（不是 blackbox 500）: {body}"
        );
        // 资产仍在回收站（没有半截删除）
        let trash = get_json(&app, "/api/dedup/trash?limit=50").await;
        assert!(trash["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|it| it["assetId"] == ids[0]));
    }

    /// E2：导出选择集（目录 + manifest）全链路 —— 复用 D11 svc-library::export。
    #[tokio::test]
    async fn e2_export_selection_flow() {
        let state = test_state().await;
        let app = router(state.clone(), std::env::temp_dir());
        let ids = seed_assets(&state, 3).await;

        // 重复导出到同一目录 → 冲突名自动 _1（D11 unique_name 语义）
        let out = state.data_dir.join("export-out");
        let req = json!({
            "assetIds": ids,
            "content": "original",
            "destDir": out.to_str().unwrap(),
            "manifest": true
        });
        let (status, body) = post_json(&app, "/api/export/start", req).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        wait_export(&state).await;
        let st = get_json(&app, "/api/export/status").await;
        assert_eq!(st["running"], false);
        let rep = &st["report"];
        assert_eq!(rep["total"], 3);
        assert_eq!(rep["exported"], 3);
        assert!(rep["errors"].as_array().unwrap().is_empty(), "{rep}");
        assert_eq!(rep["manifestWritten"], true);
        assert_eq!(rep["dest"], out.to_str().unwrap());
        assert!(out.join("manifest.csv").exists(), "manifest.csv 落盘");

        // 二次导出同目录：文件已存在 → 追加序号，不覆盖
        let (status, _) = post_json(
            &app,
            "/api/export/start",
            json!({
                "assetIds": ids,
                "content": "original",
                "destDir": out.to_str().unwrap(),
                "manifest": false
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        wait_export(&state).await;
        let names: Vec<String> = std::fs::read_dir(&out)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.contains("_1")),
            "二次导出应加序号: {names:?}"
        );
        assert_eq!(names.len(), 3 + 3 + 1, "两轮 6 个文件 + manifest");

        // zip 模式
        let zip = state.data_dir.join("export.zip");
        let (status, _) = post_json(
            &app,
            "/api/export/start",
            json!({
                "assetIds": ids,
                "content": "original",
                "zipPath": zip.to_str().unwrap(),
                "manifest": true
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        wait_export(&state).await;
        let st = get_json(&app, "/api/export/status").await;
        assert_eq!(st["report"]["exported"], 3);
        assert!(zip.exists(), "zip 落盘: {zip:?}");

        // 参数校验：来源二选一 / 目标必须绝对路径 / 非法内容档位 → 400
        for bad in [
            json!({"content":"original","destDir":out.to_str().unwrap()}), // 无来源
            json!({"assetIds":ids,"content":"original","destDir":"relative/dir"}), // 相对路径
            json!({"assetIds":ids,"content":"webp","destDir":out.to_str().unwrap()}), // 非法内容
            json!({"assetIds":ids,"content":"original"}),                  // 无目标
        ] {
            let (status, _) = post_json(&app, "/api/export/start", bad.clone()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "应拒绝 {bad}");
        }

        // 无导出在跑时取消 → 409
        assert_eq!(
            post_json(&app, "/api/export/cancel", json!({})).await.0,
            StatusCode::CONFLICT
        );
    }

    /// E2：原图失联的资产进 skipped 清单，不阻断整批导出。
    #[tokio::test]
    async fn e2_export_skips_missing_original() {
        let state = test_state().await;
        let app = router(state.clone(), std::env::temp_dir());
        let ids = seed_assets(&state, 2).await;
        // 把第 2 张的 absPath 指到不存在的文件
        {
            let conn = state.db.lock().await;
            conn.execute(
                "UPDATE assets SET absPath = '/nonexistent/missing.png' WHERE assetId = ?1",
                rusqlite::params![ids[1]],
            )
            .unwrap();
        }
        let out = state.data_dir.join("export-missing");
        let (status, _) = post_json(
            &app,
            "/api/export/start",
            json!({
                "assetIds": ids,
                "content": "original",
                "destDir": out.to_str().unwrap(),
                "manifest": true
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        wait_export(&state).await;
        let st = get_json(&app, "/api/export/status").await;
        let rep = &st["report"];
        assert_eq!(rep["total"], 2);
        assert_eq!(rep["exported"], 1);
        assert_eq!(rep["skipped"].as_array().unwrap().len(), 1);
        assert_eq!(rep["skipped"][0]["assetId"], ids[1]);
    }

    /// E4：批量标签 / 色标 / 移动 + ids 集合回读 + 逐条失败回执。
    #[tokio::test]
    async fn e4_tag_color_move_batch() {
        let state = test_state().await;
        let app = router(state.clone(), std::env::temp_dir());
        let ids = seed_assets(&state, 3).await;
        let (a, b, c) = (ids[0], ids[1], ids[2]);
        // c 的磁盘文件删掉 → 模拟「盘上失联」
        let c_path = {
            let conn = state.db.lock().await;
            conn.query_row("SELECT absPath FROM assets WHERE assetId=?1", [c], |r| {
                r.get::<_, String>(0)
            })
            .unwrap()
        };
        std::fs::remove_file(&c_path).unwrap();

        // —— 批量打标签：3 张 × 2 个标签，全部成功
        let res = post_json(
            &app,
            "/api/library/tag",
            json!({"ids":[a,b,c],"values":["旅行","夕阳"]}),
        )
        .await;
        assert_eq!(res.0, StatusCode::OK);
        assert_eq!(res.1["okCount"], 3);
        assert_eq!(res.1["failCount"], 0);
        let conn = state.db.lock().await;
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tag WHERE value IN ('旅行','夕阳')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        drop(conn);
        assert_eq!(n, 6, "3 张 × 2 标签 = 6 行");

        // —— 重复打标：成功但「无变化」（INSERT OR IGNORE 幂等）
        let res = post_json(
            &app,
            "/api/library/tag",
            json!({"ids":[a],"values":["旅行"]}),
        )
        .await;
        assert_eq!(res.1["okCount"], 1);
        assert_eq!(res.1["results"][0]["detail"], "标签已存在（无变化）");

        // —— 单资产移除标签
        let res = post_json(
            &app,
            "/api/library/untag",
            json!({"assetId":a,"value":"夕阳"}),
        )
        .await;
        assert_eq!(res.1["removed"], 1);
        let tags = get_json(&app, &format!("/api/library/assets/{a}/tags")).await;
        let names: Vec<&str> = tags
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["value"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["旅行"]);

        // —— 空标签名 / 不存在的资产：400 / 逐条失败
        assert_eq!(
            post_json(&app, "/api/library/tag", json!({"ids":[a],"values":["  "]}))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        let res = post_json(
            &app,
            "/api/library/tag",
            json!({"ids":[99999],"values":["x"]}),
        )
        .await;
        assert_eq!(res.1["failCount"], 1);
        assert_eq!(res.1["results"][0]["error"], "资产不存在");

        // —— 批量色标：设置 / 读模型可见 / 非法色名 400 / 清除
        let res = post_json(
            &app,
            "/api/library/color",
            json!({"ids":[a,b],"color":"red"}),
        )
        .await;
        assert_eq!(res.1["updated"], 2);
        let list = get_json(&app, &format!("/api/library/assets?ids={a},{b},{c}")).await;
        assert_eq!(list["total"], 3, "ids 集合回读");
        let colors: Vec<&str> = list["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|it| it["assetId"].as_i64() != Some(c))
            .map(|it| it["colorLabel"].as_str().unwrap())
            .collect();
        assert_eq!(colors, vec!["red", "red"]);
        assert_eq!(
            post_json(
                &app,
                "/api/library/color",
                json!({"ids":[a],"color":"purple"})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let res = post_json(&app, "/api/library/color", json!({"ids":[a],"color":null})).await;
        assert_eq!(res.1["updated"], 1);

        // —— 批量移动：目标目录 + 重名自动改名 + 磁盘失联逐条报错
        let target = state.data_dir.join("e4-target");
        // 预置重名文件（内容不同）→ e2-0.png 应落为 "e2-0 (2).png"
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("e2-0.png"), b"conflict").unwrap();
        let res = post_json(
            &app,
            "/api/library/move",
            json!({"ids":[a,b,c],"targetDir":target.to_str().unwrap()}),
        )
        .await;
        assert_eq!(res.0, StatusCode::OK);
        assert_eq!(res.1["okCount"], 2, "a/b 成功，c 磁盘失联失败");
        assert_eq!(res.1["failCount"], 1);
        let results = res.1["results"].as_array().unwrap();
        let a_res = results.iter().find(|r| r["assetId"] == a).unwrap();
        assert_eq!(a_res["ok"], true);
        let new_path = a_res["detail"].as_str().unwrap();
        assert!(
            new_path.ends_with("e2-0 (2).png"),
            "重名自动改名：{new_path}"
        );
        assert!(std::path::Path::new(new_path).is_file());
        let c_res = results.iter().find(|r| r["assetId"] == c).unwrap();
        assert_eq!(c_res["ok"], false);
        assert!(c_res["error"].as_str().unwrap().contains("磁盘文件不存在"));

        // —— DB absPath 同步更新 + 失联标记
        let list = get_json(&app, &format!("/api/library/assets?ids={a},{c}")).await;
        let items = list["items"].as_array().unwrap();
        let a_row = items.iter().find(|it| it["assetId"] == a).unwrap();
        assert!(a_row["absPath"].as_str().unwrap().contains("e4-target"));
        let c_row = items.iter().find(|it| it["assetId"] == c).unwrap();
        assert_eq!(c_row["missing"], 1, "盘上失联 → missing=1");

        // —— 再移到同目录：幂等「已在目标目录」
        let res = post_json(
            &app,
            "/api/library/move",
            json!({"ids":[a],"targetDir":target.to_str().unwrap()}),
        )
        .await;
        assert_eq!(res.1["results"][0]["detail"], "已在目标目录（未变动）");

        // —— 非法目标（相对路径）→ 400
        assert_eq!(
            post_json(
                &app,
                "/api/library/move",
                json!({"ids":[a],"targetDir":"rel/dir"})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }

    /// 测试辅助：往库里塞 N 张真实存在的 1x1 png，返回 assetId 列表。
    async fn seed_assets(state: &AppState, n: usize) -> Vec<i64> {
        let png: [u8; 70] = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52,
            0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0, 0x0D, 0x49,
            0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xF0, 0x1F, 0, 5, 0, 1, 0xFF,
            0x89, 0x99, 0x3D, 0x1D, 0, 0, 0, 0, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        let dir = state.data_dir.join("e2-src");
        std::fs::create_dir_all(&dir).unwrap();
        let mut ids = Vec::new();
        for i in 0..n {
            let name = format!("e2-{i}.png");
            let path = dir.join(&name);
            std::fs::write(&path, png).unwrap();
            let conn = state.db.lock().await;
            conn.execute(
                "INSERT INTO assets (mediaType, absPath, fileName, fileExt, fileSz, mtime, importedAt)
                 VALUES ('image', ?1, ?2, 'png', ?3, unixepoch(), unixepoch())",
                rusqlite::params![path.to_str().unwrap(), name, png.len() as i64],
            )
            .unwrap();
            ids.push(conn.last_insert_rowid());
        }
        ids
    }

    /// 测试辅助：等导出跑完（最多 ~10s）。
    async fn wait_export(state: &AppState) {
        let mut waited = 0;
        while state
            .export
            .running
            .load(std::sync::atomic::Ordering::SeqCst)
            && waited < 500
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            waited += 1;
        }
        assert!(
            !state
                .export
                .running
                .load(std::sync::atomic::Ordering::SeqCst),
            "export should finish"
        );
    }

    async fn post_json(
        app: &Router,
        uri: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, v)
    }

    async fn get_json(app: &Router, uri: &str) -> serde_json::Value {
        let res = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "GET {uri}");
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// C7（卡 task）：关键词检索必须**服务端全库**。
    ///
    /// 用户实测「来得及输全就搜 / 搜出来的结果不全」的根因：关键词曾是客户端筛选，
    /// 只作用于已加载的这一页（PAGE_SIZE 60）。这里断言三件事：
    ///  1. `?q=` 真的进了请求（命中集跨全库，不依赖前端已加载多少）；
    ///  2. `total` 与 `items` 同口径（同一 WHERE）——「命中 N 张」与分页不能打架；
    ///  3. 导出 payload 用同一个键名 `q`（`ImportFilter` 的 serde alias）——
    ///     否则会出现「图库筛出 2 张、导出却是整库」。
    #[tokio::test]
    async fn c7_assets_keyword_is_server_side() {
        let state = test_state().await;
        {
            let conn = state.db.lock().await;
            conn.execute_batch(
                "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt, title_ch) VALUES
                    (1, '/lib/snow-1.jpg', 0, 'snow-1.jpg', 'jpg', 10, 10, 10, '雪山日出'),
                    (2, '/lib/beach.jpg',  0, 'beach.jpg',  'jpg', 10, 10, 20, '海滩'),
                    (3, '/lib/snow-2.jpg', 0, 'snow-2.jpg', 'jpg', 10, 10, 30, '雪山云海');",
            )
            .unwrap();
        }
        let app = router_without_dist(state);
        assert_eq!(get_json(&app, "/api/library/assets").await["total"], 3);
        // 雪山（URL 编码，避免测试框架对非 ASCII 的处理差异）
        let hit = get_json(&app, "/api/library/assets?q=%E9%9B%AA%E5%B1%B1").await;
        assert_eq!(hit["total"], 2, "total 必须是全库命中数: {hit}");
        let ids: Vec<i64> = hit["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["assetId"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![3, 1], "命中两页之后的资产也要能搜到");
        // 分页：limit=1 → items 1 条，total 仍是全库命中 2（「命中 N 张」取 total）
        let page = get_json(
            &app,
            "/api/library/assets?q=%E9%9B%AA%E5%B1%B1&limit=1&offset=1",
        )
        .await;
        assert_eq!(page["total"], 2);
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        // 导出 payload 键名口径（serde alias：`q` → query）
        let f: core_types::ImportFilter =
            serde_json::from_value(serde_json::json!({ "q": "雪山" })).unwrap();
        assert_eq!(f.query.as_deref(), Some("雪山"));
    }
}
