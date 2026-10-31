//! KI-16 #7（卡 task）：智能文件夹并入图库 —— 「保存时 count」与「应用后 total」同口径。
//!
//! 前端不再有独立页面：点侧栏 = `/api/library/assets` 带上保存的条件（`dirPrefix`/`tag`/
//! `untagged`/`missing`，见 `web/src/lib/galleryUrlState.ts::smartFolderAssetQuery`）。
//! 「条件已变」标记的判据是「保存时记下的 total ≠ 应用后 total」，所以这两次查询**必须**由
//! 同一组参数名表达同一段筛选 —— 本测试在真后端（SQLite + svc-library 过滤）上把两个数字量出来。
//!
//! 边界：不改 `/api/smart-folders` 契约（本测试只用既有 CRUD 与既有 assets 过滤参数）。

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

fn seed_asset(conn: &rusqlite::Connection, id: i64, dir: &str, name: &str, tag: Option<&str>) {
    conn.execute(
        "INSERT INTO assets (assetId, absPath, fileName, fileExt, fileSz, mtime, importedAt)
         VALUES (?1, ?2, ?3, 'jpg', 100, 0, ?4)",
        rusqlite::params![id, format!("{dir}/{name}.jpg"), format!("{name}.jpg"), id],
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

/// 「智能文件夹条件 → 图库 URL 查询」的两个数字：保存时 count、应用后 total。
#[tokio::test]
async fn smart_folder_count_matches_gallery_total() {
    let dir = std::env::temp_dir().join(format!("task-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let conn = core_storage::open(&dir.join("dam.db")).unwrap();
    let state = test_support::state(dir, conn);
    let app = app_web::router_without_dist(state.clone());

    // 库：精选目录里 2 张「雪山」+ 1 张「海滩」；别的目录里 1 张「雪山」（不该被目录条件算进来）
    const PICKED: &str = "/tmp/task/library/picked";
    const OTHER: &str = "/tmp/task/library/other";
    {
        let c = state.db.lock().await;
        seed_asset(&c, 1, PICKED, "snow-1", Some("雪山"));
        seed_asset(&c, 2, PICKED, "snow-2", Some("雪山"));
        seed_asset(&c, 3, PICKED, "beach", Some("海滩"));
        seed_asset(&c, 4, OTHER, "snow-3", Some("雪山"));
    }

    // 保存智能文件夹（前端只会保存图库能表达的维度：目录 + 标签）
    let (s, v) = req(
        app.clone(),
        "POST",
        "/api/smart-folders",
        Some(format!(
            r#"{{"name":"精选雪山","query":{{"dirPrefix":"{PICKED}","tag":"雪山"}}}}"#
        )),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK, "{v}");
    let fid = v["id"].as_i64().unwrap();

    // 两次都用「图库应用条件」时的同一组参数名（dirPrefix / tag / limit=1）
    let uri = format!(
        "/api/library/assets?dirPrefix={}&tag=%E9%9B%AA%E5%B1%B1&limit=1",
        urlencode(PICKED)
    );

    // ① 保存时 count（前端 SmartFolderDialog 保存后立刻取的那次）
    let (s, saved) = req(app.clone(), "GET", &uri, None).await;
    assert_eq!(s, axum::http::StatusCode::OK, "{saved}");
    let saved_count = saved["total"].as_i64().unwrap();

    // ② 打开智能文件夹后图库应用条件的 total（`/?dir=…&tags=…&sf=<id>` → 同一条查询）
    let (s, applied) = req(app.clone(), "GET", &uri, None).await;
    assert_eq!(s, axum::http::StatusCode::OK, "{applied}");
    let applied_total = applied["total"].as_i64().unwrap();

    println!("保存时 count = {saved_count}；应用后 total = {applied_total}（文件夹 id={fid}）");
    assert_eq!(
        saved_count, 2,
        "目录 + 标签条件应命中 2 张（别的目录的同名标签不算）"
    );
    assert_eq!(
        saved_count, applied_total,
        "同一条件两次查询必须同口径 —— 否则图库会假报「条件已变」"
    );

    // ③ 条件没变但库变了 → 两个数字不再相等（前端据此标「条件已变」）
    {
        let c = state.db.lock().await;
        seed_asset(&c, 5, PICKED, "snow-4", Some("雪山"));
    }
    let (_, after) = req(app.clone(), "GET", &uri, None).await;
    let now_total = after["total"].as_i64().unwrap();
    println!("库变化后 total = {now_total}（保存时 {saved_count} → 应标「条件已变」）");
    assert_ne!(now_total, saved_count, "库变化后应能检出「条件已变」");
}

/// 极小的 query 值转义（只用于本测试的路径常量，避免引入额外依赖）
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
