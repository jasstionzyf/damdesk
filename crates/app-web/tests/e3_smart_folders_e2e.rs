//! E3 验收（HTTP 集成）：智能文件夹 CRUD + 校验 + colorLabel 过滤 + 动态现查语义。
//!
//! 动态视图语义验证方式：建文件夹（tag=狗, minRating=4）→ 资产入库前后两次按
//! 文件夹查询，命中数随库变化（不落快照）。

use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

use app_web::test_support;

async fn req(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<String>,
) -> (axum::http::StatusCode, serde_json::Value) {
    let mut b = Request::builder().method(method).uri(uri);
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    let res = app
        .oneshot(b.body(Body::from(body.unwrap_or_default())).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let v = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, v)
}

fn seed_asset(
    conn: &rusqlite::Connection,
    id: i64,
    name: &str,
    tag: Option<&str>,
    rating: i64,
    color: Option<&str>,
) {
    conn.execute(
        "INSERT INTO assets (assetId, absPath, fileName, fileExt, fileSz, mtime, importedAt, starRating, colorLabel)
         VALUES (?1, ?2, ?3, 'jpg', 100, 0, ?4, ?5, ?6)",
        rusqlite::params![id, format!("/tmp/e3/{name}.jpg"), format!("{name}.jpg"), id, rating, color],
    )
    .unwrap();
    if let Some(t) = tag {
        conn.execute(
            "INSERT INTO tag (assetId, value, origin, createdAt) VALUES (?1, ?2, 'manual', ?3)",
            rusqlite::params![id, t, id],
        )
        .unwrap();
    }
}

#[tokio::test]
async fn e3_smart_folder_crud_and_validation() {
    let dir = std::env::temp_dir().join(format!("e3-api-{}-1", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let conn = core_storage::open(&dir.join("dam.db")).unwrap();
    let state = test_support::state(dir, conn);
    let app = app_web::router_without_dist(state);

    // 空列表
    let (s, v) = req(app.clone(), "GET", "/api/smart-folders", None).await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), 0);

    // 建组合条件文件夹
    let (s, v) = req(
        app.clone(),
        "POST",
        "/api/smart-folders",
        Some(r#"{"name":"精选狗","query":{"tag":"狗","minRating":4,"colorLabel":"green"}}"#.into()),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK, "{v}");
    let fid = v["id"].as_i64().unwrap();
    assert_eq!(v["name"], "精选狗");
    assert!(v["query"].as_str().unwrap().contains("colorLabel"));

    // 重名 400 / 空条件 400 / 未知键 400 / 非法颜色 400
    let (s, _) = req(
        app.clone(),
        "POST",
        "/api/smart-folders",
        Some(r#"{"name":"精选狗","query":{"untagged":true}}"#.into()),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::BAD_REQUEST);
    for bad in [
        r#"{"name":"x1","query":{}}"#,
        r#"{"name":"x2","query":{"limit":10}}"#,
        r#"{"name":"x3","query":{"colorLabel":"cyan"}}"#,
    ] {
        let (s, _) = req(app.clone(), "POST", "/api/smart-folders", Some(bad.into())).await;
        assert_eq!(s, axum::http::StatusCode::BAD_REQUEST, "{bad}");
    }

    // 改名 + 改条件
    let (s, v) = req(
        app.clone(),
        "PUT",
        &format!("/api/smart-folders/{fid}"),
        Some(r#"{"name":"精选"}"#.into()),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert_eq!(v["name"], "精选");
    let (s, _) = req(
        app.clone(),
        "PUT",
        &format!("/api/smart-folders/{fid}"),
        Some(r#"{"query":{"tag":"狗"}}"#.into()),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK);

    // 单个 404
    let (s, _) = req(app.clone(), "GET", "/api/smart-folders/9999", None).await;
    assert_eq!(s, axum::http::StatusCode::NOT_FOUND);

    // 删除 + 二次删除 404
    let (s, _) = req(
        app.clone(),
        "DELETE",
        &format!("/api/smart-folders/{fid}"),
        None,
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK);
    let (s, _) = req(
        app.clone(),
        "DELETE",
        &format!("/api/smart-folders/{fid}"),
        None,
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn e3_folder_results_are_dynamic_with_color_and_combinations() {
    let dir = std::env::temp_dir().join(format!("e3-dyn-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let conn = core_storage::open(&dir.join("dam.db")).unwrap();
    let state = test_support::state(dir, conn);
    let app = app_web::router_without_dist(state.clone());

    // 建文件夹：狗 + ≥4 + 绿色
    let (_, v) = req(
        app.clone(),
        "POST",
        "/api/smart-folders",
        Some(
            r#"{"name":"绿狗精选","query":{"tag":"狗","minRating":4,"colorLabel":"green"}}"#.into(),
        ),
    )
    .await;
    let _fid = v["id"].as_i64().unwrap();

    // 库里有 1 张命中（绿狗5星）+ 2 张不命中
    {
        let c = state.db.lock().await;
        seed_asset(&c, 1, "green-dog", Some("狗"), 5, Some("green"));
        seed_asset(&c, 2, "red-dog", Some("狗"), 5, Some("red"));
        seed_asset(&c, 3, "green-cat", Some("猫"), 5, Some("green"));
    }
    // 首次按文件夹条件查询：1 张
    let (s, v) = req(
        app.clone(),
        "GET",
        "/api/library/assets?tag=%E7%8B%97&minRating=4&colorLabel=green&limit=50",
        None,
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert_eq!(v["total"], 1, "组合条件首次命中");

    // 库变化：再来一张绿狗4星 → 同一条件命中数变化（动态现查，非快照）
    {
        let c = state.db.lock().await;
        seed_asset(&c, 4, "green-dog-2", Some("狗"), 4, Some("green"));
    }
    let (s, v) = req(
        app.clone(),
        "GET",
        "/api/library/assets?tag=%E7%8B%97&minRating=4&colorLabel=green&limit=50",
        None,
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert_eq!(v["total"], 2, "库变化后同一智能文件夹条件命中更新");

    // colorLabel 单维度
    let (s, v) = req(
        app.clone(),
        "GET",
        "/api/library/assets?colorLabel=red&limit=50",
        None,
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert_eq!(v["total"], 1);
}
