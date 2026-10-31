//! W4 回归钉：/api/tag/* 必须挂在 /tag 前缀下（曾用 merge 误挂 /api 根，SPA fallback 吞请求）。
use axum::body::Body;
use axum::http::Request;
use std::path::PathBuf;
use tower::ServiceExt;

#[tokio::test]
async fn tag_routes_under_api_tag() {
    let _ = std::fs::remove_file(std::env::temp_dir().join("tag-route-test.db"));
    let conn = core_storage::open(&std::env::temp_dir().join("tag-route-test.db")).unwrap();
    let state = app_web::test_support::state(PathBuf::from("/tmp/tag-route-test-data"), conn);
    let app = app_web::router_without_dist(state);
    // status: 路由命中（handler 可能因测试环境 registry 报 500，但绝不能 404/SPA HTML）
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/tag/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(res.status(), 404, "/api/tag/status 必须命中路由");
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    if status.as_u16() == 200 {
        let j: serde_json::Value =
            serde_json::from_slice(&body).expect("status 200 时必须是 tag JSON");
        assert!(j.get("template").is_some(), "缺少 template 槽: {j}");
    }
    // tasks: 直接 200 JSON
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/tag/tasks")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(j.get("items").is_some(), "tasks 应返回 items 数组");
}

#[tokio::test]
async fn tag_routes_not_leaked_to_api_root() {
    // 防回归：merge 误挂 /api 根的旧错误形态（/api/tasks 曾 200）
    let _ = std::fs::remove_file(std::env::temp_dir().join("tag-route-test2.db"));
    let conn = core_storage::open(&std::env::temp_dir().join("tag-route-test2.db")).unwrap();
    let state = app_web::test_support::state(PathBuf::from("/tmp/tag-route-test-data2"), conn);
    let app = app_web::router_without_dist(state);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/tasks")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        404,
        "/api/tasks 不应存在（路由只能挂在 /api/tag/tasks）"
    );
}
