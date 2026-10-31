//! W5 回归钉：/api/dedup/* 全链路（分组 → 裁决 → 回收站恢复）走 HTTP 路由真跑。
use axum::body::Body;
use axum::http::Request;
use rusqlite::Connection;
use std::path::PathBuf;
use tower::ServiceExt;

fn seed_db(path: &PathBuf) -> Connection {
    let _ = std::fs::remove_file(path);
    let conn = core_storage::open(path).unwrap();
    // 组1（exact）：同 fileHash；asset 3 是近似邻居（pHash 差 1 bit）→ 三张连成一组（near）
    for (id, hash, phash) in [
        (1, Some("aaaa"), Some(0u64)),
        (2, Some("aaaa"), Some(0u64)),
        (3, Some("bbbb"), Some(1u64)),
        (4, None, None), // 无 hash 无 pHash：永不入组
    ] {
        conn.execute(
            "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime,
                                 fileHash, importedAt)
             VALUES (?1, ?2, 0, ?3, 'jpg', 100, 1, ?4, ?1)",
            rusqlite::params![id, format!("/x/{id}.jpg"), format!("f{id}.jpg"), hash],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO resource (assetId, role, ordinal, width, height, pHash)
             VALUES (?1, 'primary', 0, 800, 600, ?2)",
            rusqlite::params![id, phash.map(|v| format!("{v:016x}"))],
        )
        .unwrap();
    }
    conn
}

#[tokio::test]
async fn dedup_groups_resolve_restore_roundtrip() {
    let db = std::env::temp_dir().join("w5-dedup-route.db");
    let conn = seed_db(&db);
    let state = app_web::test_support::state(PathBuf::from("/tmp/w5-dedup-route-data"), conn);
    let app = app_web::router_without_dist(state);

    // 1) 分组：三张连一组（kind=near，混合 hash），单例不入组
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/dedup/groups")
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
    let groups = j["groups"].as_array().expect("groups 数组");
    assert_eq!(groups.len(), 1, "应恰有一组: {j}");
    assert_eq!(groups[0]["kind"], "near");
    assert_eq!(groups[0]["size"], 3);
    assert_eq!(groups[0]["members"].as_array().unwrap().len(), 3);

    // 2) 裁决：保留 1，删 2、3
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/dedup/resolve")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"keepAssetId":1,"removeIds":[2,3]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(j["removed"], 2);

    // 裁决后再查：组消失
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/dedup/groups")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(j["groups"].as_array().unwrap().len(), 0);

    // 3) 回收站有 2 条，恢复后归位
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/dedup/trash")
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
    assert_eq!(j["items"].as_array().unwrap().len(), 2);

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/dedup/restore")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"ids":[2,3]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(j["restored"], 2);

    // 恢复后组回来
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/dedup/groups")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(j["groups"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn dedup_resolve_rejects_bad_input_with_400() {
    let db = std::env::temp_dir().join("w5-dedup-route2.db");
    let conn = seed_db(&db);
    let state = app_web::test_support::state(PathBuf::from("/tmp/w5-dedup-route2-data"), conn);
    let app = app_web::router_without_dist(state);

    // 保留项混入删除列表 → 400（svc Err::Invalid 映射）
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/dedup/resolve")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"keepAssetId":1,"removeIds":[1,2]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 400);

    // 未知资产 → 400，且不得部分生效（trash 仍为空）
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/dedup/resolve")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"keepAssetId":1,"removeIds":[2,99]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 400);

    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/dedup/trash")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        j["items"].as_array().unwrap().len(),
        0,
        "防呆失败单不得部分生效"
    );
}
