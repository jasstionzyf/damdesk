//! DM-A4（卡 task，KI-2 残项）：`range=batch:<id>` **按导入事件精确过滤**。
//!
//! 钉住的三件事：
//! 1. **同一目录分两次导入互不串批** —— 两个批次的 `rootPath` 完全相同，按老口径
//!    （rootPath 前缀）`batch:1` 会把批次 2 的图也圈进来；现在按 `assets.batchId`
//!    等值过滤，两个范围各只看见自己的成员。
//! 2. **混合库里的老行（`batchId IS NULL`）不进精确范围** —— 这批只要有一张带批次，
//!    就按精确集合走，不许再偷偷回落到前缀（否则又一次近似）。
//! 3. **老库回退**：这批名下 `batchId` 全 NULL（0033 之前的导入）→ 回退成 rootPath
//!    前缀近似，并在 `rangeMeta.approx=true` + `rangeMeta.prefix` 里明示，不许说成精确。
//!
//! 反向验证：把 `resolve_scope` 的 batch 分支改回「拿 rootPath 当 prefix」，
//! `two_batches_same_root_do_not_leak` 立刻红（它会数到 2 组）。

use axum::body::Body;
use axum::http::Request;
use rusqlite::Connection;
use std::path::PathBuf;
use tower::ServiceExt;

/// 两批图都在 `/lib/dirA`（rootPath 完全相同 —— 老口径的失效现场）：
///
/// * 批次 1：asset 1、2 —— 同 fileHash（exact 对）
/// * 批次 2：asset 3、4 —— 同 pHash（near 对）
/// * 老行：asset 5 —— 同目录但 `batchId IS NULL`（迁移前入库），自成一个 hash，不参与任何组
/// * 批次 9：rootPath 同为 `/lib/dirA`，名下没有任何带批次的资产（老库回退用）
fn seed_db(path: &PathBuf) -> Connection {
    let _ = std::fs::remove_file(path);
    let conn = core_storage::open(path).unwrap();
    // 批次先落（`assets.batchId` 有外键 → FK 真开着，顺序不能反）
    // 批次 1、2：同根目录的两次导入；批次 9：老库批次（名下零带批次资产）
    for (id, root) in [(1, "/lib/dirA"), (2, "/lib/dirA"), (9, "/lib/dirA")] {
        conn.execute(
            "INSERT INTO import_batches (batchId, rootPath, storeMode, status, totalFiles, doneFiles, startedAt)
             VALUES (?1, ?2, 'reference', 'done', 2, 2, 1000)",
            rusqlite::params![id, root],
        )
        .unwrap();
    }
    for (id, hash, phash, batch) in [
        (1, "aaaa", None, Some(1)),
        (2, "aaaa", None, Some(1)),
        (3, "bbbb", Some("0000000000000000"), Some(2)),
        (4, "cccc", Some("0000000000000000"), Some(2)),
        (5, "dddd", None, None),
    ] {
        conn.execute(
            "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime,
                                 fileHash, importedAt, batchId)
             VALUES (?1, ?2, 0, ?3, 'jpg', ?4, 1, ?5, 1000, ?6)",
            rusqlite::params![
                id,
                format!("/lib/dirA/{id}.jpg"),
                format!("f{id}.jpg"),
                1000 + id,
                hash,
                batch
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO resource (assetId, role, ordinal, width, height, pHash)
             VALUES (?1, 'primary', 0, 400, 300, ?2)",
            rusqlite::params![id, phash],
        )
        .unwrap();
    }
    conn
}

fn state(conn: Connection) -> app_web::AppState {
    app_web::test_support::state(
        PathBuf::from(std::env::temp_dir().join("dm-a4-batch-data")),
        conn,
    )
}

async fn get(app: axum::Router, uri: &str) -> (u16, String, String) {
    let res = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let ctype = res
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap_or("").to_string())
        .unwrap_or_default();
    let body = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    (status, ctype, String::from_utf8_lossy(&body).to_string())
}

async fn get_json(app: axum::Router, uri: &str) -> (u16, serde_json::Value) {
    let (st, _ct, body) = get(app, uri).await;
    (
        st,
        serde_json::from_str(&body).unwrap_or(serde_json::Value::Null),
    )
}

/// 响应里的组 → `Vec<Vec<assetId>>`（组内升序，组间按最小 assetId 排）。
fn shape(j: &serde_json::Value) -> Vec<Vec<i64>> {
    let mut out: Vec<Vec<i64>> = j["groups"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|g| {
                    let mut ids: Vec<i64> = g["members"]
                        .as_array()
                        .map(|m| m.iter().map(|x| x["assetId"].as_i64().unwrap()).collect())
                        .unwrap_or_default();
                    ids.sort_unstable();
                    ids
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

#[tokio::test]
async fn two_batches_same_root_do_not_leak() {
    let dir = std::env::temp_dir().join(format!("dm-a4-batch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let conn = seed_db(&dir.join("dam.db"));
    let app = app_web::router_without_dist(state(conn));

    // 全库：两组（1,2 exact + 3,4 near）
    let (st, all) = get_json(app.clone(), "/api/dedup/groups").await;
    assert_eq!(st, 200, "{all}");
    assert_eq!(all["totalGroups"], 2, "全库两组: {all}");

    // dir 范围：同目录两组（老口径下 batch 与 dir 同解 —— 现在必须分道扬镳）
    let (st, dirj) = get_json(app.clone(), "/api/dedup/groups?range=dir:/lib/dirA").await;
    assert_eq!(st, 200, "{dirj}");
    assert_eq!(dirj["totalGroups"], 2, "dir 范围两组: {dirj}");
    assert_eq!(dirj["rangeMeta"]["approx"], false);

    // 批次 1：只有自己导进来的 (1,2)；批次 2 的 (3,4) 一张都不许出现
    let (st, b1) = get_json(app.clone(), "/api/dedup/groups?range=batch:1").await;
    assert_eq!(st, 200, "{b1}");
    assert_eq!(b1["rangeMeta"]["kind"], "batch");
    assert_eq!(b1["rangeMeta"]["approx"], false, "带 batchId 的批次是精确的: {b1}");
    assert_eq!(b1["rangeMeta"]["batchAssets"], 2, "{b1}");
    assert_eq!(b1["rangeMeta"]["prefix"], serde_json::Value::Null, "精确范围不该有目录前缀: {b1}");
    assert_eq!(b1["totalGroups"], 1, "同目录两次导入不许串批: {b1}");
    assert_eq!(shape(&b1)[0], vec![1, 2], "{b1}");
    assert!(
        b1["rangeMeta"]["outOfScopeAssets"].as_u64().unwrap() >= 2,
        "被裁掉的 (3,4) 要报出来（范围外还有重复，不是没有重复）: {b1}"
    );

    let (st, b2) = get_json(app.clone(), "/api/dedup/groups?range=batch:2").await;
    assert_eq!(st, 200, "{b2}");
    assert_eq!(b2["totalGroups"], 1, "批次 2 只看见自己导的: {b2}");
    assert_eq!(shape(&b2)[0], vec![3, 4], "{b2}");
    assert_eq!(b2["rangeMeta"]["batchAssets"], 2, "{b2}");

    // 混合库：老行（asset 5, batchId NULL）不进精确范围（它也不在任何组里，
    // 这里断言 batchAssets 只数带批次的 2 行，而不是「前缀下的 5 行」）
    assert_eq!(b1["rangeMeta"]["batchAssets"], 2, "只数本批，不数同目录老行: {b1}");
}

#[tokio::test]
async fn old_library_batch_falls_back_to_approx() {
    let dir = std::env::temp_dir().join(format!("dm-a4-approx-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let conn = seed_db(&dir.join("dam.db"));
    let app = app_web::router_without_dist(state(conn));

    // 批次 9：名下零带批次资产（= 0033 之前导入的老库批次）→ 回退前缀近似 + 明示 approx
    let (st, j) = get_json(app.clone(), "/api/dedup/groups?range=batch:9").await;
    assert_eq!(st, 200, "{j}");
    assert_eq!(j["rangeMeta"]["kind"], "batch");
    assert_eq!(
        j["rangeMeta"]["approx"], true,
        "老库必须标明「按目录近似」，不许静默当精确: {j}"
    );
    assert_eq!(j["rangeMeta"]["prefix"], "/lib/dirA", "{j}");
    assert_eq!(j["rangeMeta"]["batchAssets"], 0, "{j}");
    assert_eq!(j["totalGroups"], 2, "近似口径 = 该目录内的两组: {j}");

    // 反向：批次不存在仍然 400（回退不等于什么都认）
    let (st, e) = get_json(app.clone(), "/api/dedup/groups?range=batch:999").await;
    assert_eq!(st, 400, "{e}");
    assert!(e["error"].as_str().unwrap_or("").contains("999"), "{e}");
}

#[tokio::test]
async fn trashed_batch_assets_are_not_counted() {
    let dir = std::env::temp_dir().join(format!("dm-a4-trash-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let conn = seed_db(&dir.join("dam.db"));
    // 批次 1 的 2 号进了回收站 → 本批「在库里」只剩 1 号
    conn.execute("UPDATE assets SET deleted = 1 WHERE assetId = 2", [])
        .unwrap();
    let app = app_web::router_without_dist(state(conn));

    let (st, j) = get_json(app.clone(), "/api/dedup/groups?range=batch:1").await;
    assert_eq!(st, 200, "{j}");
    assert_eq!(j["rangeMeta"]["batchAssets"], 1, "回收站里的本批资产不算在库里: {j}");
    assert_eq!(
        j["totalGroups"], 0,
        "只剩 1 张凑不出重复组（不是「没重复」，outOfScopeAssets 会说明）: {j}"
    );
}

#[tokio::test]
async fn export_batch_range_uses_same_scope() {
    let dir = std::env::temp_dir().join(format!("dm-a4-export-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let conn = seed_db(&dir.join("dam.db"));
    let app = app_web::router_without_dist(state(conn));

    let (st, ctype, body) = get(app, "/api/dedup/export?format=csv&range=batch:1").await;
    assert_eq!(st, 200, "{body}");
    assert!(ctype.contains("csv"), "{ctype}");
    assert!(body.contains("# range=batch:1"), "报告必须回显范围: {body}");
    assert!(body.contains("/lib/dirA/1.jpg"), "{body}");
    assert!(body.contains("/lib/dirA/2.jpg"), "{body}");
    assert!(
        !body.contains("/lib/dirA/3.jpg") && !body.contains("/lib/dirA/4.jpg"),
        "导出与页面同口径：批次 2 的图不许出现在批次 1 的报告里: {body}"
    );
}
