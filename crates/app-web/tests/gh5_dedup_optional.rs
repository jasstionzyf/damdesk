//! GH-5 路由级验收：DINOv3 不随安装包分发（Meta 许可禁止再分发）→ 判重塔降级为可选塔。
//!
//! 断言三件事：
//! 1. 权重缺件时 `/api/library/stats` **如实**报 `dedupAvailable=false` + 自备指引
//!    （「判重不可用，需自备模型（Meta 许可）」+ 模型根绝对路径）—— 不 panic、不撒谎；
//! 2. 桌面构建（local-infer）把两件套放进模型根 → `dedupAvailable=true`（自备即恢复）；
//! 3. 响应其余字段不受影响（assets 等照常返回 —— 可用性是**增量**字段，不是替代）。
//!
//! 「不许静默把 pHash 当判重」在 dedup_api 的三层判据口径（near_visual 缺向量 = 未检测），
//! 本文件钉的是**可用性报告面**：stats 报缺 = 引擎真读不到（同一 models_root 契约）。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::path::{Path, PathBuf};
use tower::ServiceExt;

use app_web::AppState;

fn tmp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gh5-dedup-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 最小 AppState：空库（stats 对空库照常统计），models_root = data_dir（test_support 契约）。
fn seed_state(tag: &str) -> (axum::Router, PathBuf) {
    let data_dir = tmp_dir(tag);
    let conn = core_storage::open(&data_dir.join("dam.db")).unwrap();
    let state: AppState = app_web::test_support::state(data_dir.clone(), conn);
    (app_web::router_without_dist(state), data_dir)
}

async fn call_stats(app: &axum::Router) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri("/api/library/stats")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    (status, body)
}

/// 模型根下判重塔目录（与引擎同一契约：`core_config::model_dir(root, DEDUP_EMBEDDER)`）。
fn dedup_dir(data_dir: &Path) -> PathBuf {
    core_config::model_dir(data_dir, core_config::DEDUP_EMBEDDER)
}

/// 缺件（常态）：dedupAvailable=false + 「判重不可用」理由；不 panic；其余统计字段照常。
#[tokio::test]
async fn stats_reports_dedup_unavailable_without_weights() {
    let (app, data_dir) = seed_state("missing");
    let (st, body) = call_stats(&app).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["assets"], 0, "空库统计照常（可用性是增量字段）");
    assert_eq!(body["dedupAvailable"], false, "缺件必须如实报缺：{body}");
    let reason = body["dedupUnavailableReason"].as_str().unwrap_or("");
    assert!(reason.contains("判重不可用"), "理由必须点名判重不可用：{reason}");
    assert!(
        !reason.is_empty(),
        "false 必须带理由（不许静默降级）"
    );
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 桌面构建：自备两件套 → available=true（自备即恢复，无需重启）；只放一件仍报缺。
/// web 容器构建（无 local-infer）没有判重塔，恒 false —— 那半个断言编译掉。
#[cfg(feature = "local-infer")]
#[tokio::test]
async fn stats_flips_to_available_when_self_provided() {
    let (app, data_dir) = seed_state("present");
    // 只放图件 → 仍缺
    let dir = dedup_dir(&data_dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("model.onnx"), b"graph").unwrap();
    let (st, body) = call_stats(&app).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["dedupAvailable"], false, "缺外置权重必须仍报缺：{body}");
    let reason = body["dedupUnavailableReason"].as_str().unwrap_or("");
    assert!(reason.contains("Meta 许可"), "缺件理由必须指引自备：{reason}");
    assert!(
        reason.contains(&dedup_dir(&data_dir).to_string_lossy().to_string()),
        "理由必须带模型根绝对路径（自备落点）：{reason}"
    );
    // 两件齐 → true（同进程、不重启）
    std::fs::write(dir.join("model.onnx_data"), b"data").unwrap();
    let (st, body) = call_stats(&app).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["dedupAvailable"], true, "自备两件套后必须可用：{body}");
    assert!(
        body.get("dedupUnavailableReason").is_none(),
        "可用时不得残留报缺理由：{body}"
    );
    let _ = std::fs::remove_dir_all(&data_dir);
}
