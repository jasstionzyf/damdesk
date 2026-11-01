//! E2（卡 t_63c5b793）：高频用户可见错误的**稳定错误码**集中地 + 带码响应助手。
//!
//! 契约（零破坏）：响应体 = `{"error": <中文原句>, "code": <稳定 snake_case>, "params": {...}?}`。
//! * `error` 永远是中文原句 —— 老前端/日志照读它，行为不变；
//! * `code` 给前端词典查 `errors.<code>` 词条（`web/src/i18n/dict.en-US.ts`），查不到显示中文原文；
//! * `params` 只装译文占位符需要的值（数字/字符串），空对象省略字段。
//!
//! 码的纪律：`codes` 常量只收本卡新增/补 params 的码。既有稳定码
//! （`no_vector_index` / `model_mismatch` / `dim_mismatch` / `provider_error` / `connect_failed` /
//! `tag_not_configured` / `empty_selection` / `bad_mode` / `spec_*` / `pack_*` / `model_source_unconfigured`）
//! **保持原值不动**（前端按值分支，改名即破坏），只补 `params` 与词典词条。
//! 泛动词码（`io` / `join` / `save` / `enqueue` / `already_running`…）是长尾，本卡不碰。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;

/// 带码错误响应：`{"error": 中文原句, "code": 稳定码, "params": 插值参数（空则省略）}`。
pub fn err_coded(status: StatusCode, code: &str, params: Value, msg: impl Into<String>) -> Response {
    let mut body = serde_json::Map::new();
    body.insert("error".to_string(), Value::String(msg.into()));
    body.insert("code".to_string(), Value::String(code.to_string()));
    if params.as_object().is_some_and(|m| !m.is_empty()) {
        body.insert("params".to_string(), params);
    }
    (status, Json(Value::Object(body))).into_response()
}

/// 模块内校验失败的中转类型：`validate_dir` / `build_spec` 一类返回 `Result<T, CodedError>`，
/// handler 顶层 `Err(e) => e.into_response()` 一次落成带码响应。
#[derive(Debug, Clone)]
pub struct CodedError {
    pub status: StatusCode,
    pub code: &'static str,
    pub params: Value,
    pub msg: String,
}

impl CodedError {
    pub fn bad_request(code: &'static str, params: Value, msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code,
            params,
            msg: msg.into(),
        }
    }
}

impl std::fmt::Display for CodedError {
    /// 日志/断言消息用：`[code] 中文原句`
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.msg)
    }
}

impl IntoResponse for CodedError {
    fn into_response(self) -> Response {
        err_coded(self.status, self.code, self.params, self.msg)
    }
}

/// 本卡新增/收敛的稳定码。命名 `<域>_<问题>`，只加不改（前端按值查词典 + 分支）。
pub mod codes {
    // ---- 导入 / 库操作 ----
    pub const IMPORT_PATH_NOT_ABSOLUTE: &str = "import_path_not_absolute";
    pub const IMPORT_PATH_NOT_DIRECTORY: &str = "import_path_not_directory";
    pub const IMPORT_ALREADY_RUNNING: &str = "import_already_running";
    pub const IMPORT_NOT_RUNNING: &str = "import_not_running";
    pub const IMPORT_BATCH_NOT_FOUND: &str = "import_batch_not_found";
    pub const IMPORT_PATHS_EMPTY: &str = "import_paths_empty";
    pub const IMPORT_BAD_STORE_MODE: &str = "import_bad_store_mode";
    pub const LIBRARY_IDS_EMPTY: &str = "library_ids_empty";
    pub const LIBRARY_ID_INVALID: &str = "library_id_invalid";
    pub const LIBRARY_TOO_MANY_IDS: &str = "library_too_many_ids";
    pub const LIBRARY_VALUES_EMPTY: &str = "library_values_empty";
    pub const LIBRARY_RATING_OUT_OF_RANGE: &str = "library_rating_out_of_range";
    pub const LIBRARY_DELETE_FAILED: &str = "library_delete_failed";
    pub const LIBRARY_RESTORE_FAILED: &str = "library_restore_failed";

    // ---- 去重（导入/库组的用户高频入口）----
    pub const DEDUP_TARGET_REQUIRED: &str = "dedup_target_required";
    pub const DEDUP_SIGNATURES_EMPTY: &str = "dedup_signatures_empty";
    pub const DEDUP_PATH_EMPTY: &str = "dedup_path_empty";
    pub const DEDUP_DIR_NOT_FOUND: &str = "dedup_dir_not_found";
    pub const DEDUP_LOCAL_DIR_UNSUPPORTED: &str = "dedup_local_dir_unsupported";
    pub const DEDUP_DIR_READ_FAILED: &str = "dedup_dir_read_failed";
    pub const DEDUP_BAD_FORMAT: &str = "dedup_bad_format";
    pub const DEDUP_SCAN_STALE: &str = "dedup_scan_stale";
    pub const DEDUP_IDS_EMPTY: &str = "dedup_ids_empty";
    pub const DEDUP_TOO_MANY_IDS: &str = "dedup_too_many_ids";
    pub const DEDUP_PATH_NOT_ABSOLUTE: &str = "dedup_path_not_absolute";

    // ---- 导出与交付 ----
    pub const EXPORT_ALREADY_RUNNING: &str = "export_already_running";
    pub const EXPORT_NOT_RUNNING: &str = "export_not_running";
    pub const EXPORT_SOURCE_INVALID: &str = "export_source_invalid";
    pub const EXPORT_CONTENT_INVALID: &str = "export_content_invalid";
    pub const EXPORT_ZIP_PATH_INVALID: &str = "export_zip_path_invalid";
    pub const EXPORT_DEST_REQUIRED: &str = "export_dest_required";

    // ---- 搜索 / 探针（沿用既有稳定码：补 params 与词典词条，不改值）----
    pub const NO_VECTOR_INDEX: &str = "no_vector_index";
    pub const MODEL_MISMATCH: &str = "model_mismatch";
    pub const DIM_MISMATCH: &str = "dim_mismatch";
    pub const PROVIDER_ERROR: &str = "provider_error";
    pub const PROVIDER_NOT_CONFIGURED: &str = "provider_not_configured";
    pub const EMPTY_QUERY: &str = "empty_query";
    pub const EMPTY_BODY: &str = "empty_body";
    pub const TOO_LARGE: &str = "too_large";
    pub const BAD_MIN_SCORE: &str = "bad_min_score";
    pub const TEXT_UNSUPPORTED: &str = "text_unsupported";
    pub const CONNECT_FAILED: &str = "connect_failed";
    pub const TAG_NOT_CONFIGURED: &str = "tag_not_configured";
    pub const EMPTY_SELECTION: &str = "empty_selection";
    pub const BAD_MODE: &str = "bad_mode";
    pub const MODEL_SOURCE_UNCONFIGURED: &str = "model_source_unconfigured";
    pub const OCR_NOT_BUILT: &str = "ocr_not_built";
    pub const NOT_EMBEDDING: &str = "not_embedding";
    pub const NO_TEXT_MODALITY: &str = "no_text_modality";
    pub const LOCAL_INFER_DISABLED: &str = "local_infer_disabled";

    // ---- 合规判定与复核（判定前置四码已有值，这里补 params 用常量）----
    pub const PLUGIN_NOT_INSTALLED: &str = "plugin_not_installed";
    pub const SPEC_PLUGIN_NOT_INSTALLED: &str = "spec_plugin_not_installed";
    pub const SPEC_PLUGIN_VERSION_MISMATCH: &str = "spec_plugin_version_mismatch";
    pub const SPEC_NOT_FOUND: &str = "spec_not_found";
    pub const SPEC_DISABLED: &str = "spec_disabled";
    pub const SPEC_UNKNOWN_RULE: &str = "spec_unknown_rule";
    // judge_prepare 是「内部失败的兜底码」，message 恒为动态中文现场（比静态英文更有用），
    // 故**不进** ALL、不进词典：永远走中文原句兜底。
    pub const JUDGE_PREPARE: &str = "judge_prepare";
    pub const COMPLIANCE_ASSETS_REQUIRED: &str = "compliance_assets_required";

    // ---- 插件装卸 ----
    pub const PACK_BUILTIN_PROTECTED: &str = "pack_builtin_protected";
    pub const PACK_NOT_INSTALLED: &str = "pack_not_installed";
    pub const PACK_VERSION_NOT_INSTALLED: &str = "pack_version_not_installed";
    pub const PACK_VERSION_CURRENT: &str = "pack_version_current";

    /// 全量码清单（错误码 ↔ 词典键一致性门禁用：`scripts/i18n_error_code_check.py`）。
    pub const ALL: &[&str] = &[
        IMPORT_PATH_NOT_ABSOLUTE,
        IMPORT_PATH_NOT_DIRECTORY,
        IMPORT_ALREADY_RUNNING,
        IMPORT_NOT_RUNNING,
        IMPORT_BATCH_NOT_FOUND,
        IMPORT_PATHS_EMPTY,
        IMPORT_BAD_STORE_MODE,
        LIBRARY_IDS_EMPTY,
        LIBRARY_ID_INVALID,
        LIBRARY_TOO_MANY_IDS,
        LIBRARY_VALUES_EMPTY,
        LIBRARY_RATING_OUT_OF_RANGE,
        LIBRARY_DELETE_FAILED,
        LIBRARY_RESTORE_FAILED,
        DEDUP_TARGET_REQUIRED,
        DEDUP_SIGNATURES_EMPTY,
        DEDUP_PATH_EMPTY,
        DEDUP_DIR_NOT_FOUND,
        DEDUP_LOCAL_DIR_UNSUPPORTED,
        DEDUP_DIR_READ_FAILED,
        DEDUP_BAD_FORMAT,
        DEDUP_SCAN_STALE,
        DEDUP_IDS_EMPTY,
        DEDUP_TOO_MANY_IDS,
        DEDUP_PATH_NOT_ABSOLUTE,
        EXPORT_ALREADY_RUNNING,
        EXPORT_NOT_RUNNING,
        EXPORT_SOURCE_INVALID,
        EXPORT_CONTENT_INVALID,
        EXPORT_ZIP_PATH_INVALID,
        EXPORT_DEST_REQUIRED,
        NO_VECTOR_INDEX,
        MODEL_MISMATCH,
        DIM_MISMATCH,
        PROVIDER_ERROR,
        PROVIDER_NOT_CONFIGURED,
        EMPTY_QUERY,
        EMPTY_BODY,
        TOO_LARGE,
        BAD_MIN_SCORE,
        TEXT_UNSUPPORTED,
        CONNECT_FAILED,
        TAG_NOT_CONFIGURED,
        EMPTY_SELECTION,
        BAD_MODE,
        MODEL_SOURCE_UNCONFIGURED,
        OCR_NOT_BUILT,
        NOT_EMBEDDING,
        NO_TEXT_MODALITY,
        LOCAL_INFER_DISABLED,
        PLUGIN_NOT_INSTALLED,
        SPEC_PLUGIN_NOT_INSTALLED,
        SPEC_PLUGIN_VERSION_MISMATCH,
        SPEC_NOT_FOUND,
        SPEC_DISABLED,
        SPEC_UNKNOWN_RULE,
        COMPLIANCE_ASSETS_REQUIRED,
        PACK_BUILTIN_PROTECTED,
        PACK_NOT_INSTALLED,
        PACK_VERSION_NOT_INSTALLED,
        PACK_VERSION_CURRENT,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use serde_json::json;

    #[tokio::test]
    async fn err_coded_shape_with_params() {
        let resp = err_coded(
            StatusCode::BAD_REQUEST,
            codes::IMPORT_PATH_NOT_DIRECTORY,
            json!({ "path": "/tmp/x" }),
            "不是文件夹：/tmp/x",
        );
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        // 零破坏契约：中文原句在 error，code/params 并列；不新增别名字段
        assert_eq!(v["error"], "不是文件夹：/tmp/x");
        assert_eq!(v["code"], "import_path_not_directory");
        assert_eq!(v["params"]["path"], "/tmp/x");
        assert!(v.get("message").is_none());
    }

    #[tokio::test]
    async fn err_coded_empty_params_omitted() {
        let resp = err_coded(
            StatusCode::CONFLICT,
            codes::IMPORT_ALREADY_RUNNING,
            json!({}),
            "an import batch is already running",
        );
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["code"], "import_already_running");
        assert!(v.get("params").is_none(), "空 params 必须省略字段");
    }

    #[tokio::test]
    async fn coded_error_into_response() {
        let e = CodedError::bad_request(
            codes::EXPORT_DEST_REQUIRED,
            json!({}),
            "导出目标非法：destDir 与 zipPath 必须二选一且非空",
        );
        let resp = e.into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["code"], "export_dest_required");
        assert!(v["error"].as_str().unwrap().contains("destDir"));
    }

    #[test]
    fn codes_all_unique() {
        let mut all = codes::ALL.to_vec();
        all.sort_unstable();
        let n = all.len();
        all.dedup();
        assert_eq!(all.len(), n, "codes::ALL 不得有重复码");
    }
}
