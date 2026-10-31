//! KI-2b（卡 task）：`/api/dedup/*` 视觉判重边端到端（走 HTTP 路由真跑）。
//!
//! 钉住：
//! 1. `POST /api/dedup/backfill` 把「已有判重向量、还没有边」的存量库补出边，
//!    并回 `judge` 进度（vectors/edges/backfillDone）——UI 三态徽标的事实源；
//! 2. `GET /api/dedup/groups` 把视觉边与 fileHash/pHash 判据**分层合并**：
//!    kind = near_visual / variant，混合组 mixed=true，`kindCounts` 可核对；
//! 3. 阈值口径与检索侧（engine-search）一致（防两处漂移）；
//! 4. 没有判重向量的形态（web 容器关本地推理）→ backfill 明确回 hint，不谎报「无重复」。

use axum::body::Body;
use axum::http::Request;
use rusqlite::Connection;
use std::path::PathBuf;
use tower::ServiceExt;

const DIMS: u32 = 4;
const MODEL: &str = core_config::DEDUP_EMBEDDER;

/// 6 张图，三套互不相连的判据：
/// - 1、2：同 fileHash（exact 对，pHash 同）；
/// - 3：与 1 视觉同内容（cos 0.99），与 5/6 正交；
/// - 4：与谁都不相关（pHash 全 1，向量正交）；
/// - 5、6：同图不同构图（cos 0.95），落在 y/z 平面。
fn seed_db(path: &PathBuf) -> Connection {
    let _ = std::fs::remove_file(path);
    let conn = core_storage::open(path).unwrap();
    for (id, hash, phash) in [
        (1, Some("aaaa"), Some(0u64)),
        (2, Some("aaaa"), Some(0u64)),
        // 3..6 不给 pHash：让它们只可能靠视觉判据成组（pHash 汉明会互相连成一片）
        (3, Some("bbbb"), None),
        (4, Some("cccc"), None),
        (5, Some("dddd"), None),
        (6, Some("eeee"), None),
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

fn primary_rid(conn: &Connection, asset_id: i64) -> i64 {
    conn.query_row(
        "SELECT resourceId FROM resource WHERE assetId = ?1 AND role = 'primary'",
        [asset_id],
        |r| r.get(0),
    )
    .unwrap()
}

fn put_vec(conn: &Connection, asset_id: i64, v: [f32; DIMS as usize]) {
    core_storage::vecstore::ensure_table(conn, MODEL, DIMS).unwrap();
    let rid = primary_rid(conn, asset_id);
    core_storage::vecstore::upsert(conn, MODEL, &[(rid, &v)]).unwrap();
}

fn seed_vectors(conn: &Connection) {
    put_vec(conn, 1, [1.0, 0.0, 0.0, 0.0]);
    put_vec(conn, 2, [0.0, 1.0, 0.0, 0.0]); // 与 1 正交：只有 fileHash 一层判据
    put_vec(conn, 3, [0.99, (1.0f32 - 0.99 * 0.99).sqrt(), 0.0, 0.0]); // 与 1 cos=0.99
    put_vec(conn, 4, [0.0, 0.0, 1.0, 0.0]); // 谁都不像
    put_vec(conn, 5, [0.0, 0.0, 0.0, 1.0]); // 与 6 cos=0.95，与前四张都正交
    put_vec(conn, 6, [0.0, (1.0f32 - 0.95 * 0.95).sqrt(), 0.0, 0.95]);
}

fn state(conn: Connection) -> app_web::AppState {
    app_web::test_support::state(
        PathBuf::from(std::env::temp_dir().join("ki2b-dedup-data")),
        conn,
    )
}

async fn post(app: axum::Router, uri: &str) -> (u16, serde_json::Value) {
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status().as_u16();
    let body = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

async fn get(app: axum::Router, uri: &str) -> (u16, serde_json::Value) {
    let res = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let body = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

/// 阈值口径必须与检索侧一致：两处各写一份常量最容易漂移，这里钉死。
#[test]
fn dedup_thresholds_match_search_side() {
    assert_eq!(
        svc_pipeline::DUP_NEAR_THRESHOLD,
        engine_search::NEAR_DUP_THRESHOLD,
        "near 阈值与 engine-search 不一致"
    );
    assert_eq!(
        svc_pipeline::DUP_VARIANT_THRESHOLD,
        engine_search::VARIANT_THRESHOLD,
        "variant 阈值与 engine-search 不一致"
    );
    assert_eq!(svc_pipeline::DUP_K_NEIGHBORS, 9, "KNN 9 邻居口径");
}

/// 存量库（有向量无边）→ backfill 补边 → groups 出现视觉组（分层合并 + kind 分标注）
#[tokio::test]
async fn backfill_then_groups_reports_visual_kinds() {
    let db = std::env::temp_dir().join("ki2b-dedup-route.db");
    let conn = seed_db(&db);
    seed_vectors(&conn);
    let app = app_web::router_without_dist(state(conn));

    // 补判前：向量在、边没有 → judge 明确说「还没判完」（不是「无重复」）
    let (code, j) = get(app.clone(), "/api/dedup/groups").await;
    assert_eq!(code, 200, "{j}");
    assert_eq!(j["judge"]["vectors"], 6);
    assert_eq!(j["judge"]["edges"], 0);
    assert_eq!(j["judge"]["backfillDone"], false);
    assert_eq!(j["judge"]["backfillTruncated"], false);

    // 补判（全量，`max_assets=0`）
    let (code, j) = post(app.clone(), "/api/dedup/backfill?max_assets=0").await;
    assert_eq!(code, 200, "{j}");
    assert_eq!(j["report"]["scanned"], 6);
    assert_eq!(j["report"]["truncated"], false);
    assert_eq!(j["report"]["edges"], 2, "只有 (1,3) 与 (5,6) 两条边: {j}");
    assert_eq!(j["judge"]["backfillDone"], true);
    assert_eq!(j["judge"]["backfillTruncated"], false);
    assert!(j["judge"]["lastJudgeAt"].as_i64().unwrap() > 0);

    // 分组：exact 对 + 视觉边合并成一个混合组；变体对单独一组
    let (code, j) = get(app.clone(), "/api/dedup/groups").await;
    assert_eq!(code, 200, "{j}");
    let groups = j["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "1+2+3 一组、5+6 一组: {j}");

    let mixed_group = groups.iter().find(|g| g["size"] == 3).expect("混合组");
    assert_eq!(mixed_group["kind"], "near_visual", "组内最保守层说话");
    assert_eq!(mixed_group["mixed"], true);
    assert_eq!(mixed_group["layers"], serde_json::json!(["phash", "visual"]));
    assert!(mixed_group["members"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["assetId"] == 3
            && (m["similarity"].as_f64().unwrap() - 0.99).abs() < 1e-3));
    assert!(mixed_group["members"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["assetId"] == 2 && m["similarity"].is_null()));

    let variant_group = groups.iter().find(|g| g["size"] == 2).expect("变体组");
    assert_eq!(variant_group["kind"], "variant");
    assert_eq!(variant_group["mixed"], false);
    assert_eq!(variant_group["layers"], serde_json::json!(["visual"]));

    assert_eq!(j["kindCounts"]["near_visual"], 1);
    assert_eq!(j["kindCounts"]["variant"], 1);

    // 幂等：再补判一次，边数不变（不重复造边）
    let (code, j2) = post(app.clone(), "/api/dedup/backfill?max_assets=0").await;
    assert_eq!(code, 200, "{j2}");
    assert_eq!(j2["judge"]["edges"], 2, "补判可重入");

    // 视觉端点进回收站 → 视觉边在分组时被忽略，剩下纯 fileHash 组
    let conn2 = rusqlite::Connection::open(&db).unwrap();
    conn2
        .execute("UPDATE assets SET deleted = 1 WHERE assetId IN (3, 6)", [])
        .unwrap();
    drop(conn2);
    let (code, j3) = get(app, "/api/dedup/groups").await;
    assert_eq!(code, 200, "{j3}");
    let groups = j3["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "5+6 少了一端、1+3 少了一端: {j3}");
    assert_eq!(groups[0]["kind"], "exact", "只剩 fileHash 一层: {j3}");
    assert_eq!(groups[0]["mixed"], false);
    assert_eq!(groups[0]["layers"], serde_json::json!(["phash"]));
    // 关系表里边的行数不变（清边是 purge 的事，不是分组的事）
    assert_eq!(j3["judge"]["edges"], 2);
}

/// 没有判重向量的形态（web 容器关本地推理 / 桌面还没嵌入）→
/// backfill 不谎报：hint 说明原因，groups 的 judge.vectors=0（= 未检测）
#[tokio::test]
async fn backfill_without_vectors_reports_hint_not_empty_library() {
    let db = std::env::temp_dir().join("ki2b-dedup-novec.db");
    let conn = seed_db(&db);
    let app = app_web::router_without_dist(state(conn));

    let (code, j) = post(app.clone(), "/api/dedup/backfill").await;
    assert_eq!(code, 200, "{j}");
    assert_eq!(j["ok"], true);
    assert_eq!(j["report"]["vectors"], 0);
    assert_eq!(j["report"]["scanned"], 0);
    assert!(
        j["hint"].as_str().unwrap().contains("判重塔还没有向量"),
        "{j}"
    );

    // 默认批次上限存在且为正（不传 max_assets 时不会全量占锁）
    assert!(app_web::dedup_api::DEFAULT_BACKFILL_BATCH > 0);

    let (code, j) = get(app, "/api/dedup/groups").await;
    assert_eq!(code, 200, "{j}");
    assert_eq!(j["judge"]["vectors"], 0);
    assert_eq!(
        j["judge"]["backfillDone"], true,
        "没向量=无事可做，别让 UI 卡「检测中」"
    );
}
