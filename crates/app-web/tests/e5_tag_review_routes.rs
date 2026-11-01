//! E5 回归钉：/api/tag/review/* 确认流全链路（暂存 → 列表 → 修正 → 确认/跳过）走 HTTP 路由真跑。
//! 提案由测试直接种入 tag_proposal（推理侧 stage_template/stage_custom 已有 svc-pipeline 单测覆盖）。
use axum::body::Body;
use axum::http::Request;
use rusqlite::Connection;
use std::path::PathBuf;
use tower::ServiceExt;

fn seed_db(path: &PathBuf) -> Connection {
    let _ = std::fs::remove_file(path);
    let conn = core_storage::open(path).unwrap();
    for id in 1..=3 {
        conn.execute(
            "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (?1, ?2, 0, ?3, 'jpg', 100, 1, ?1)",
            rusqlite::params![id, format!("/x/{id}.jpg"), format!("f{id}.jpg")],
        )
        .unwrap();
    }
    // 资产 1：模板提案（title + 2 标签，一个低置信）；资产 2：自定义任务提案；资产 3：仅 title
    conn.execute_batch(
        "INSERT INTO tag_proposal (assetId, kind, name, value, model, score, createdAt)
         VALUES (1, 'title', '', '海边日落', 'damai', 1.0, 0),
                (1, 'tag', '日落', '', 'damai', 0.92, 0),
                (1, 'tag', '人像', '', 'damai', 0.41, 0),
                (3, 'title', '', '未确认资产', 'damai', 1.0, 0);
        INSERT INTO custom_tag_task (id, name, prompt, enabled, createdAt, updatedAt)
         VALUES (7, '地点', '图中地点?', 1, 0, 0);
        INSERT INTO tag_proposal (assetId, kind, name, value, model, taskId, score, createdAt)
         VALUES (2, 'tag', '地点', '室内', 'damai', 7, 1.0, 0);",
    )
    .unwrap();
    conn
}

async fn post_json(app: &axum::Router, uri: &str, body: &str) -> (u16, serde_json::Value) {
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
    let status = res.status().as_u16();
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
    (status, j)
}

#[tokio::test]
async fn tag_review_full_flow() {
    let db = std::env::temp_dir().join("e5-review-route.db");
    let conn = seed_db(&db);
    let state = app_web::test_support::state(PathBuf::from("/tmp/e5-review-route-data"), conn);
    let app = app_web::router_without_dist(state);

    // 1) status 带 review 统计
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
    if res.status().as_u16() == 200 {
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(j["review"]["assets"], 3, "status 必须暴露 review 统计: {j}");
        assert_eq!(j["review"]["rows"], 6);
    }

    // 2) 列表：按资产分组 + 分页 total
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/tag/review?limit=2")
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
    assert_eq!(j["total"], 3);
    let items = j["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "limit=2 → 每页 2 资产卡");
    assert_eq!(items[0]["assetId"], 1);
    // 组内顺序：title 打头，tag 在后
    assert_eq!(items[0]["items"][0]["kind"], "title");
    let tag_rows = items[0]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["kind"] == "tag")
        .count();
    assert_eq!(tag_rows, 2);
    // 自定义提案带 taskName
    assert_eq!(items[1]["assetId"], 2);
    assert_eq!(items[1]["items"][0]["taskName"], "地点");

    // 3) 修正资产 2：改值（edited 置位）
    let rowid: i64 = {
        let c = Connection::open(&db).unwrap();
        c.query_row("SELECT id FROM tag_proposal WHERE assetId = 2", [], |r| {
            r.get(0)
        })
        .unwrap()
    };
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/tag/review/items/2")
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"updates":[{{"id":{rowid},"value":"海边"}}]}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    // 4) 确认资产 1 + 2；跳过资产 3
    let (status, j) = post_json(&app, "/api/tag/review/commit", r#"{"assetIds":[1,2]}"#).await;
    assert_eq!(status, 200, "commit 应成功: {j}");
    assert_eq!(j["report"]["assets"], 2);

    let (status, j) = post_json(&app, "/api/tag/review/discard", r#"{"assetIds":[3]}"#).await;
    assert_eq!(status, 200);
    assert_eq!(j["removed"], 1);

    // 5) 落库校验：
    //    - 资产 1 未修改提案 → auto 落库 + title 只填空字段
    //    - 资产 2 修正值 → custom 行更新为「海边」
    //    - 资产 3 跳过 → 零写入
    {
        let c = Connection::open(&db).unwrap();
        let auto: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM tag WHERE assetId = 1 AND origin = 'auto'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(auto, 2, "未修改标签提案按 auto 落库");
        let title: String = c
            .query_row("SELECT title_ch FROM assets WHERE assetId = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(title, "海边日落");
        // TAG-VALUE（0021）：维度名在结果表 tag_result，标签值在 tag；
        // 人工改过的值按 manual 落库，并按统一标签规则拍平 key-val（地点-海边）
        let (value, origin): (String, String) = c
            .query_row(
                "SELECT value, origin FROM tag WHERE assetId = 2",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(value, "地点-海边", "修正值拍平成 key-val 落 manual");
        assert_eq!(origin, "manual", "人工改过的提案按 manual 落库");
        let (dim, raw): (String, String) = c
            .query_row(
                "SELECT name, value FROM tag_result WHERE assetId = 2 AND taskId = 7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((dim.as_str(), raw.as_str()), ("地点", "海边"), "维度名与输出留结果表");
        let n: i64 = c
            .query_row(
                "SELECT (SELECT COUNT(*) FROM tag WHERE assetId = 3)
                      + (SELECT COUNT(*) FROM tag_proposal)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "跳过资产零写入 + 确认后提案清空");
    }

    // 6) 防呆：空选择 → 400
    let (status, _) = post_json(&app, "/api/tag/review/commit", r#"{"assetIds":[]}"#).await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn tag_review_routes_not_leaked() {
    // 路由必须挂在 /api/tag/review 前缀（nest），不能漏到 /api/review
    let db = std::env::temp_dir().join("e5-review-route2.db");
    let conn = seed_db(&db);
    let state = app_web::test_support::state(PathBuf::from("/tmp/e5-review-route2-data"), conn);
    let app = app_web::router_without_dist(state);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/review")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        404,
        "/api/review 不应存在（只能挂 /api/tag/review）"
    );
}
