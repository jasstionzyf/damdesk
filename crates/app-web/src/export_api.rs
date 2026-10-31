#![allow(non_snake_case)] // 本模块 DTO 字段 = 前端 JSON camelCase 契约，勿改
//! E2 导出 API：POST /api/export/start 启动导出（选择集 → svc-library::export），
//! GET /api/export/status 轮询进度/报告，POST /api/export/cancel 取消。
//!
//! 完全复用 D11 的 svc-library::export（同一套 ExportSpec/run_export：
//! 目录 zip 双模式、manifest.csv、missing 跳过清单、取消安全），HTTP 层只是薄壳，
//! 参数形状与 app-tauri 的 commands_export（ExportSpecParams）保持一致。
//! 批次留痕由 run_export 内部落 job_queue(kind='export')。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::error_codes::{codes, err_coded, CodedError};
use crate::AppState;
use core_types::{ExportProgress, ExportReport, ImportFilter};
use svc_library::export::{ExportContent, ExportSource, ExportSpec, ExportTarget};

/// 导出任务进程级状态（单一活动导出，与桌面版 v1 同语义）。
pub struct ExportState {
    pub running: AtomicBool,
    pub cancel: Arc<AtomicBool>,
    pub progress: Arc<Mutex<Option<ExportProgress>>>,
    /// 最近一次报告（成功）或错误摘要（`report` 为 None 时看 `error`）
    pub report: Arc<Mutex<Option<ExportReport>>>,
    pub error: Arc<Mutex<Option<String>>>,
}

impl ExportState {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(Mutex::new(None)),
            report: Arc::new(Mutex::new(None)),
            error: Arc::new(Mutex::new(None)),
        }
    }
}

impl Default for ExportState {
    fn default() -> Self {
        Self::new()
    }
}

/// 导出请求（对齐 core-types::ExportSpecParams：来源二选一、目标二选一）。
#[derive(Deserialize, Default)]
pub struct ExportReq {
    #[serde(default)]
    pub assetIds: Option<Vec<i64>>,
    #[serde(default)]
    pub filter: Option<ImportFilter>,
    /// original / thumb256 / thumb1024
    pub content: String,
    #[serde(default)]
    pub destDir: Option<String>,
    #[serde(default)]
    pub zipPath: Option<String>,
    #[serde(default)]
    pub manifest: bool,
    /// EXIF2：导出副本剥离 GPS（JPEG 重排 APP1 去 GPS IFD）
    #[serde(default)]
    pub strip_gps: Option<bool>,
}

#[allow(dead_code)] // 保留：本模块错误面统一出口（与 err_coded 并存）
fn err(status: StatusCode, msg: impl Into<String>) -> axum::response::Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

/// 参数 → 领域 spec（与 app-tauri build_spec 同一套校验）。
///
/// 校验失败返回 `CodedError`（code + params + 原文案），handler 顶层直接落带码 400。
fn build_spec(p: &ExportReq) -> Result<ExportSpec, CodedError> {
    // DECouple-BE（用户裁决 2026-10-31）：图库导出**只按资产域条件**，不读任何判定字段。
    // 带 compliance 的过滤一律 400 并指路 job 域导出 —— 静默忽略会让「按判定结果导出」变成
    // 「导出了整库」而没人发现。
    if let Some(f) = &p.filter {
        if f.compliance.is_some() {
            return Err(CodedError::bad_request(
                codes::EXPORT_SOURCE_INVALID,
                json!({ "filter": "compliance" }),
                "图库导出不接受判定维度（compliance）过滤：按判定结果导出请用 POST /api/compliance/jobs/{runId}/export",
            ));
        }
    }
    let source = match (&p.assetIds, &p.filter) {
        (Some(ids), None) if !ids.is_empty() => ExportSource::Ids(ids.clone()),
        (None, Some(f)) => ExportSource::Filter(f.clone()),
        _ => {
            return Err(CodedError::bad_request(
                codes::EXPORT_SOURCE_INVALID,
                json!({}),
                "导出来源非法：assetIds（非空选择集）与 filter（查询过滤）必须二选一",
            ))
        }
    };
    let content = match p.content.as_str() {
        "original" => ExportContent::Original,
        "thumb256" => ExportContent::Thumb(256),
        "thumb1024" => ExportContent::Thumb(1024),
        other => {
            return Err(CodedError::bad_request(
                codes::EXPORT_CONTENT_INVALID,
                json!({ "content": other }),
                format!("非法导出内容 {other}（original/thumb256/thumb1024）"),
            ))
        }
    };
    let target = match (&p.destDir, &p.zipPath) {
        (Some(d), None) if !d.trim().is_empty() => {
            let pb = PathBuf::from(d);
            if !pb.is_absolute() {
                return Err(CodedError::bad_request(
                    codes::EXPORT_ZIP_PATH_INVALID,
                    json!({ "destDir": d }),
                    format!("destDir must be absolute: {d}"),
                ));
            }
            ExportTarget::Dir(pb)
        }
        (None, Some(z)) if !z.trim().is_empty() => {
            let zs = z.trim();
            let pb = PathBuf::from(zs);
            if !pb.is_absolute() {
                return Err(CodedError::bad_request(
                    codes::EXPORT_ZIP_PATH_INVALID,
                    json!({ "zipPath": zs }),
                    format!("zipPath must be absolute: {zs}"),
                ));
            }
            // task 回归：zip 目标必须是 .zip 文件路径——以分隔符结尾或已存在
            // 同名目录 → 参数校验期直接 400，不再落进 run_export 报 os error 21 (EISDIR)。
            if zs.ends_with('/') || zs.ends_with('\\') || pb.is_dir() {
                return Err(CodedError::bad_request(
                    codes::EXPORT_ZIP_PATH_INVALID,
                    json!({ "zipPath": zs }),
                    format!(
                        "zipPath 是目录或以分隔符结尾，导出 zip 需要形如 /path/export.zip 的文件路径: {zs}"
                    ),
                ));
            }
            ExportTarget::Zip(pb)
        }
        _ => {
            return Err(CodedError::bad_request(
                codes::EXPORT_DEST_REQUIRED,
                json!({}),
                "导出目标非法：destDir 与 zipPath 必须二选一且非空",
            ))
        }
    };
    Ok(ExportSpec {
        source,
        content,
        target,
        manifest: p.manifest,
        // C6 起前端传 report（判定报告）；HTTP 层 v1 暂不透传
        report: None,
        // EXIF2：导出剥 GPS（隐私；清单恒不落坐标数值）
        strip_gps: p.strip_gps.unwrap_or(false),
    })
}

/// POST /api/export/start —— 启动导出（后台 spawn_blocking）；已在导出中 → 409。
pub async fn start(State(state): State<AppState>, Json(req): Json<ExportReq>) -> impl IntoResponse {
    if state
        .export
        .running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return err_coded(
            StatusCode::CONFLICT,
            codes::EXPORT_ALREADY_RUNNING,
            json!({}),
            "已有导出任务在进行中",
        );
    }
    let spec = match build_spec(&req) {
        Ok(s) => s,
        Err(e) => {
            state.export.running.store(false, Ordering::SeqCst);
            return e.into_response();
        }
    };
    let total_hint = match &spec.source {
        ExportSource::Ids(ids) => ids.len(),
        ExportSource::Filter(_) => 0,
    };

    state.export.cancel.store(false, Ordering::SeqCst);
    *state.export.progress.lock().unwrap() = None;
    *state.export.report.lock().unwrap() = None;
    *state.export.error.lock().unwrap() = None;

    let st = state.clone();
    let cancel = state.export.cancel.clone();
    let progress_slot = state.export.progress.clone();
    let report_slot = state.export.report.clone();
    let error_slot = state.export.error.clone();

    tokio::task::spawn_blocking(move || {
        // 独立连接（WAL 多连接安全，与导入/桌面版同构）
        let db_path = st.data_dir.join("dam.db");
        let result = (|| -> Result<ExportReport, String> {
            let conn = core_storage::open(&db_path).map_err(|e| format!("open db: {e}"))?;
            let thumbs = st.thumbs_root.clone();
            let mut emit = |p: ExportProgress| {
                *progress_slot.lock().unwrap() = Some(p);
            };
            svc_library::export::run_export(&conn, &spec, &thumbs, &cancel, &mut emit)
                .map_err(|e| e.to_string())
        })();
        match result {
            Ok(rep) => *report_slot.lock().unwrap() = Some(rep),
            Err(e) => {
                eprintln!("dam-web export: failed: {e}");
                *error_slot.lock().unwrap() = Some(e);
            }
        }
        st.export.running.store(false, Ordering::SeqCst);
    });

    (
        StatusCode::OK,
        Json(json!({ "ok": true, "total": total_hint })),
    )
        .into_response()
}

/// POST /api/export/cancel —— 取消当前导出（无在跑 → 409）。
pub async fn cancel(State(state): State<AppState>) -> impl IntoResponse {
    if !state.export.running.load(Ordering::SeqCst) {
        return err_coded(
            StatusCode::CONFLICT,
            codes::EXPORT_NOT_RUNNING,
            json!({}),
            "no export running",
        );
    }
    state.export.cancel.store(true, Ordering::SeqCst);
    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
}

/// GET /api/export/status —— {running, progress, report, error}。
pub async fn status(State(state): State<AppState>) -> impl IntoResponse {
    let running = state.export.running.load(Ordering::SeqCst);
    let progress = state.export.progress.lock().unwrap().clone();
    let report = state.export.report.lock().unwrap().clone();
    let error = state.export.error.lock().unwrap().clone();
    Json(json!({
        "running": running,
        "progress": progress,
        "report": report,
        "error": error,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use svc_library::export::ExportTarget;

    fn req(zip_path: Option<&str>, dest_dir: Option<&str>) -> ExportReq {
        ExportReq {
            filter: Some(ImportFilter::default()),
            content: "original".to_string(),
            zipPath: zip_path.map(|s| s.to_string()),
            destDir: dest_dir.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn zip_path_trailing_separator_is_rejected() {
        let e = build_spec(&req(Some("/data/exports/export-001/"), None)).unwrap_err();
        assert!(e.msg.contains("分隔符"), "unexpected error: {e}");
    }

    #[test]
    fn zip_path_pointing_to_existing_dir_is_rejected() {
        let dir = std::env::temp_dir().join(format!("dam-web-zipdir-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let e = build_spec(&req(Some(&dir.to_string_lossy()), None)).unwrap_err();
        assert!(e.msg.contains("目录"), "unexpected error: {e}");
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn zip_path_normal_file_is_accepted() {
        let spec = build_spec(&req(Some("/data/exports/export-001.zip"), None)).expect("ok");
        assert!(matches!(spec.target, ExportTarget::Zip(_)));
    }

    #[test]
    fn dest_dir_with_trailing_separator_is_still_accepted() {
        // 目录目标以 / 结尾合法，不受 zip 校验影响
        let spec = build_spec(&req(None, Some("/data/exports/export-001/"))).expect("ok");
        assert!(matches!(spec.target, ExportTarget::Dir(_)));
    }

    // ---- DT4（卡 task）：按标签导出「退化成整库」的防线 ----
    //
    // 现场报告：导出页填标签后预览命中数正确，实际导出全量。端到端复现结论见
    // reports/DT4-export-tag-filter.md（两形态 build + 真实 UI 点击，导出的都是
    // 标签命中数，未复现）。但这条链上**唯一会静默变成整库**的环节就是 HTTP 层：
    // filter 一旦没被 deserialize 进 ExportSource::Filter（字段名不一致、被前端吞掉、
    // 被后续重构改成默认值），导出不会报错，只会安静地导出整库。所以在这一层钉死。

    /// 前端实际载荷（ExportPage 实测抓包同形）→ serde → build_spec 后 tag 仍在。
    #[test]
    fn dt4_filter_tag_survives_serde_and_reaches_filter_source() {
        let body = r#"{"filter":{"tag":"南瓜"},"content":"original",
                      "destDir":"/data/exports/dt4","manifest":true}"#;
        let parsed: ExportReq = serde_json::from_str(body).expect("deserialize");
        assert_eq!(
            parsed.filter.as_ref().and_then(|f| f.tag.as_deref()),
            Some("南瓜"),
            "serde 丢了 tag —— 导出会在无人察觉的情况下变成整库"
        );
        let spec = build_spec(&parsed).expect("build spec");
        // 失败时断言而不是 panic 分支：panic 分支的代码行永远覆盖率 0，会稀释「改动行 100%」
        assert!(
            matches!(spec.source, ExportSource::Filter(_)),
            "导出来源被换掉了：{:?}",
            spec.source
        );
        // 用 Debug 形态断言字段真值：不写 if-let/match 分支，避免分支块尾行永远 0 覆盖
        let src_dbg = format!("{:?}", spec.source);
        assert!(
            src_dbg.contains("tag: Some(\"南瓜\")"),
            "导出来源丢了 tag: {src_dbg}"
        );
        assert!(
            matches!(spec.content, ExportContent::Original),
            "内容选项被改：{:?}",
            spec.content
        );
        assert!(spec.manifest, "manifest 标记丢了");
    }

    /// 空 filter = 显式整库（UI「全部留空 = 整库」的契约），不是 None、不是报错。
    #[test]
    fn dt4_empty_filter_is_explicit_whole_library() {
        let body = r#"{"filter":{},"content":"thumb256","destDir":"/data/exports/dt4-all"}"#;
        let parsed: ExportReq = serde_json::from_str(body).expect("deserialize");
        let spec = build_spec(&parsed).expect("build spec");
        assert!(
            matches!(spec.source, ExportSource::Filter(_)),
            "空 filter 也要走 Filter 来源"
        );
        // 空 filter：所有字段都是 None（字段名/无分页参数一并钉住）
        let src_dbg = format!("{:?}", spec.source);
        assert!(
            !src_dbg.contains("Some("),
            "空 filter 应全为 None: {src_dbg}"
        );
        for k in [
            "mediaType",
            "tag",
            "dirPrefix",
            "minRating",
            "colorLabel",
            "limit",
            "offset",
            "sort",
        ] {
            assert!(
                src_dbg.contains(k),
                "过滤字段 {k} 不在来源结构里: {src_dbg}"
            );
        }
        assert!(
            matches!(spec.content, ExportContent::Thumb(256)),
            "缩略图档位应为 256"
        );
    }

    /// 来源二选一不被悄悄放宽：既有 assetIds 又有 filter → 400（不猜用户意图）。
    #[test]
    fn dt4_both_sources_is_rejected_not_silently_whole_library() {
        let body = r#"{"assetIds":[1,2],"filter":{"tag":"南瓜"},"content":"original",
                      "destDir":"/data/exports/dt4"}"#;
        let parsed: ExportReq = serde_json::from_str(body).expect("deserialize");
        let e = build_spec(&parsed).unwrap_err();
        assert!(e.msg.contains("二选一"), "unexpected error: {e}");
    }

    /// 状态机默认构造（唯一活动导出的初值：不在跑、不取消、无进度/报告/错误）。
    #[test]
    fn dt4_export_state_default_is_idle() {
        let st = ExportState::default();
        assert!(!st.running.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!st.cancel.load(std::sync::atomic::Ordering::SeqCst));
        assert!(st.progress.lock().unwrap().is_none());
        assert!(st.report.lock().unwrap().is_none());
        assert!(st.error.lock().unwrap().is_none());
    }

    /// 相对路径一律 400（导出目标必须是绝对路径，避免落到进程 cwd 里没人找得到）。
    #[test]
    fn dt4_relative_paths_are_rejected() {
        let e = build_spec(&req(Some("exports/a.zip"), None)).unwrap_err();
        assert!(
            e.msg.contains("zipPath must be absolute"),
            "unexpected: {e}"
        );
        let e = build_spec(&req(None, Some("exports/a"))).unwrap_err();
        assert!(
            e.msg.contains("destDir must be absolute"),
            "unexpected: {e}"
        );
    }

    /// 选择集为空 + 无 filter → 也是 400（空选择集不能被当成「整库」）。
    #[test]
    fn dt4_empty_ids_without_filter_is_rejected() {
        let body = r#"{"assetIds":[],"content":"original","destDir":"/data/exports/dt4"}"#;
        let parsed: ExportReq = serde_json::from_str(body).expect("deserialize");
        let e = build_spec(&parsed).unwrap_err();
        assert!(e.msg.contains("二选一"), "unexpected error: {e}");
    }
}
