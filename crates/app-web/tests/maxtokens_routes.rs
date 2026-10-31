//! 卡 task 回归钉：max_tokens 可见面 + 失败按类重试（不用重建整队列）。
//!
//! 覆盖：
//! 1. `GET /api/tag/status` 的 provider 段必须带「当前生效 max_tokens」（默认 4096 /
//!    `maxTokensSource` / `defaultMaxTokens`）——UI 靠它显示「默认 / 设置页覆盖」；
//! 2. `POST /api/tag/retry-failed?class=truncated` 只重置截断类失败项，其它不动；
//! 3. 无 class 时重置全部；`GET /api/tag/failed` 的条目带 `errorClass` 分桶。
use axum::body::Body;
use axum::http::Request;
use std::path::PathBuf;
use tower::ServiceExt;

fn seed_failed(conn: &rusqlite::Connection, target: i64, err: &str) {
    conn.execute(
        "INSERT INTO job_queue (kind, targetId, payload, status, attempts, lastError, createdAt, updatedAt)
         VALUES ('tag', ?1, NULL, 'failed', 3, ?2, 0, 0)",
        rusqlite::params![target, err],
    )
    .unwrap();
}

#[tokio::test]
async fn tag_status_exposes_effective_max_tokens() {
    let db = std::env::temp_dir().join("tag-maxtok-status.db");
    let _ = std::fs::remove_file(&db);
    let data_dir = PathBuf::from("/tmp/tag-maxtok-status-data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let conn = core_storage::open(&db).unwrap();
    let state = app_web::test_support::state(data_dir, conn);
    let app = app_web::router_without_dist(state);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/tag/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let p = &j["provider"];
    // 2026-10-20 用户裁决：max_tokens 统一上限 4096（core-config 默认值同步）→ 断言跟到 4096
    assert_eq!(p["maxTokens"], 4096, "默认生效值必须是 4096: {p}");
    assert_eq!(p["defaultMaxTokens"], 4096);
    assert_eq!(p["maxTokensSource"], "default");
    assert!(
        p["maxTokensLabel"].as_str().unwrap_or("").contains("默认"),
        "label: {}",
        p["maxTokensLabel"]
    );
}

#[tokio::test]
async fn retry_failed_by_class_only_touches_truncated() {
    let db = std::env::temp_dir().join("tag-maxtok-retry.db");
    let _ = std::fs::remove_file(&db);
    let data_dir = PathBuf::from("/tmp/tag-maxtok-retry-data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let conn = core_storage::open(&db).unwrap();
    seed_failed(
        &conn,
        1,
        "tag: response truncated at max_tokens=1024（设置页覆盖）: finish_reason=length",
    );
    seed_failed(
        &conn,
        2,
        "tag: response parse: json: EOF while parsing a list at line 29 column 5; raw={",
    );
    seed_failed(&conn, 3, "tag: upstream: HTTP 500 from http://x/v1: model not found");
    let state = app_web::test_support::state(data_dir, conn);
    let app = app_web::router_without_dist(state);

    // 1) 只重试截断类 → reset=2；剩下 1 条 other
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/tag/retry-failed?class=truncated")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(j["reset"], 2, "只重置截断类: {j}");
    // provider 未配置 → 不自动续跑，但重置必须落库（不能报错）
    assert_eq!(j["started"], false, "未配置 provider 时不应声称已续跑: {j}");

    // 2) 失败明细里只剩 other 类，且带 errorClass
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/tag/failed")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let items = j["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "只剩非截断类: {j}");
    assert_eq!(items[0]["errorClass"], "other");

    // 3) 无 class = 全部重置
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/tag/retry-failed")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(j["reset"], 1);
}
