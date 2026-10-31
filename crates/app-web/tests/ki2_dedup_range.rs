//! KI-2 / KI-7（卡 task）：去重入口与去重页整合的**后端契约**。
//!
//! 钉住：
//! 1. `GET /api/dedup/groups?range=…` 四种取值（all / dir / ids / batch）的裁剪正确：
//!    只留范围内 ≥2 张的组、保留项在裁剪后重挑、`rangeMeta` 如实报出范围外重复；
//! 2. `variant` 类真的出现在响应里（判据分层：exact / near / variant）；
//! 3. 组级忽略：`POST /api/dedup/ignore` 后该组不再返回、`ignoredCount` 计数，
//!    `POST /api/dedup/unignore` 撤销后复现；
//! 4. `GET /api/dedup/export` 吐 CSV/Markdown 且元数据带算法与阈值（审计需要，KI-6）；
//! 5. 容器/网页形态 `features.scanExternalPath=false` 且 `POST /api/dedup/scan` 被拒。
//!
//! 本文件**不改环境变量**（进程内形态判定 = 容器形态），桌面形态的扫描在
//! `ki2_scan_external.rs` 里单独一个测试二进制跑（env 是进程级的，不能混在一个文件里）。

use axum::body::Body;
use axum::http::Request;
use rusqlite::Connection;
use std::path::PathBuf;
use tower::ServiceExt;

/// 6 张图，三套互不相连的判据：
/// - 1、2：同 fileHash（exact 对）→ 都在 /lib/dirA
/// - 3、4：`dup_edges` 里有一条 variant 边（余弦 0.95）→ 3 在 dirB、4 在 dirA
/// - 5、6：pHash 相同（near 对）→ 5 在 dirA、6 在 dirB
fn seed_db(path: &PathBuf) -> Connection {
    let _ = std::fs::remove_file(path);
    let conn = core_storage::open(path).unwrap();
    for (id, dir, hash, phash) in [
        (1, "dirA", Some("aaaa"), None),
        (2, "dirA", Some("aaaa"), None),
        (3, "dirB", Some("bbbb"), None),
        (4, "dirA", Some("cccc"), None),
        (5, "dirA", Some("dddd"), Some("0000000000000000")),
        (6, "dirB", Some("eeee"), Some("0000000000000000")),
    ] {
        conn.execute(
            "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime,
                                 fileHash, importedAt)
             VALUES (?1, ?2, 0, ?3, 'jpg', ?4, 1, ?5, 1000)",
            rusqlite::params![
                id,
                format!("/lib/{dir}/{id}.jpg"),
                format!("f{id}.jpg"),
                1000 + id,
                hash
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
    // 视觉判重边（KI-2b 关系表）：3 ↔ 4 余弦 0.95 → variant（同图不同构图）
    conn.execute(
        "INSERT INTO dup_edges (a, b, score, kind, model, createdAt)
         VALUES (3, 4, 0.95, 'variant', ?1, 1000)",
        [core_config::DEDUP_EMBEDDER],
    )
    .unwrap();
    // 导入批次：range=batch:<id> 用 rootPath 作前缀
    conn.execute(
        "INSERT INTO import_batches (batchId, rootPath, storeMode, status, totalFiles, doneFiles, startedAt)
         VALUES (7, '/lib/dirA', 'reference', 'done', 4, 4, 1000)",
        [],
    )
    .unwrap();
    conn
}

fn state(conn: Connection) -> app_web::AppState {
    app_web::test_support::state(std::env::temp_dir().join("ki2-dedup-data"), conn)
}

async fn get_raw(app: axum::Router, uri: &str) -> (u16, String, String) {
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
    let body = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    (status, ctype, String::from_utf8_lossy(&body).to_string())
}

async fn get_json(app: axum::Router, uri: &str) -> (u16, serde_json::Value) {
    let (status, _ct, body) = get_raw(app, uri).await;
    (
        status,
        serde_json::from_str(&body).unwrap_or(serde_json::Value::Null),
    )
}

async fn post_json(app: axum::Router, uri: &str, body: &str) -> (u16, serde_json::Value) {
    let res = app
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
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// 每组 kind + 成员 id（断言用；顺序与接口一致：大组在前）
fn shape(j: &serde_json::Value) -> Vec<(String, Vec<i64>)> {
    j["groups"]
        .as_array()
        .map(|gs| {
            gs.iter()
                .map(|g| {
                    let ids = g["members"]
                        .as_array()
                        .map(|ms| ms.iter().map(|m| m["assetId"].as_i64().unwrap()).collect())
                        .unwrap_or_default();
                    (g["kind"].as_str().unwrap_or("").to_string(), ids)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn range_parsing_is_explicit() {
    use app_web::dedup_api::{parse_range, Range};
    assert_eq!(parse_range(None).unwrap(), Range::All);
    assert_eq!(parse_range(Some("all")).unwrap(), Range::All);
    assert_eq!(
        parse_range(Some("dir:/lib/dirA")).unwrap(),
        Range::Dir("/lib/dirA".into())
    );
    assert_eq!(
        parse_range(Some("ids:3,1,3")).unwrap(),
        Range::Ids(vec![3, 1])
    );
    assert_eq!(parse_range(Some("batch:7")).unwrap(), Range::Batch(7));
    // 未知/残缺必须明确报错，不许静默退化成全库（否则用户以为在看选择集）
    assert!(parse_range(Some("dir:")).is_err());
    assert!(parse_range(Some("ids:")).is_err());
    assert!(parse_range(Some("ids:1,x")).is_err());
    assert!(parse_range(Some("batch:abc")).is_err());
    assert!(parse_range(Some("weird:1")).is_err());
}

#[tokio::test]
async fn groups_range_all_dir_ids_batch() {
    let dir = std::env::temp_dir().join(format!("ki2-range-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let conn = seed_db(&dir.join("dam.db"));
    let app = app_web::router_without_dist(state(conn));

    // 全库：exact{1,2} + near{5,6} + variant{3,4}
    let (st, j) = get_json(app.clone(), "/api/dedup/groups").await;
    assert_eq!(st, 200, "全库分组失败: {j}");
    assert_eq!(j["range"], "all");
    assert_eq!(j["totalGroups"], 3, "全库应有 3 组: {j}");
    let kinds: Vec<String> = shape(&j).into_iter().map(|(k, _)| k).collect();
    assert!(kinds.contains(&"exact".to_string()), "缺 exact 组: {j}");
    assert!(kinds.contains(&"near".to_string()), "缺 near 组: {j}");
    assert!(
        kinds.contains(&"variant".to_string()),
        "variant 类必须出现在响应里: {j}"
    );
    assert_eq!(j["kindCounts"]["variant"], 1);

    // 文件夹范围（dirA 里有 1、2、4、5）：只剩 exact{1,2}（其余组在 dirA 内不足 2 张）
    let (st, j) = get_json(app.clone(), "/api/dedup/groups?range=dir:%2Flib%2FdirA").await;
    assert_eq!(st, 200, "dir 范围失败: {j}");
    assert_eq!(j["rangeMeta"]["kind"], "dir");
    assert_eq!(j["totalGroups"], 1, "dirA 内应只剩精确重复组: {j}");
    assert_eq!(shape(&j)[0].1, vec![1, 2]);
    assert!(
        j["rangeMeta"]["outOfScopeAssets"].as_u64().unwrap() >= 2,
        "范围外成员要如实计数（3 不在 dirA、6 不在 dirA）: {j}"
    );

    // 选择集范围（1、2、3）：只有 {1,2} 命中；3 在库里仍有重复但不在选择集 → 明示
    let (st, j) = get_json(app.clone(), "/api/dedup/groups?range=ids:1,2,3").await;
    assert_eq!(st, 200, "ids 范围失败: {j}");
    assert_eq!(j["rangeMeta"]["kind"], "ids");
    assert_eq!(j["totalGroups"], 1, "选择集内只有 1,2 是重复: {j}");
    assert_eq!(shape(&j)[0].1, vec![1, 2]);
    assert_eq!(
        j["rangeMeta"]["outsideDupAssets"], 1,
        "3 的重复对象在选择集外，必须显式报出来（不许说成「没有重复」）: {j}"
    );

    // 批次范围（batch 7 的 rootPath=/lib/dirA，名下资产 batchId 全 NULL = 0033 之前的老库）
    // → DM-A4：回退成前缀近似，并在 rangeMeta.approx 明示（不许静默当精确）。
    let (st, j) = get_json(app.clone(), "/api/dedup/groups?range=batch:7").await;
    assert_eq!(st, 200, "batch 范围失败: {j}");
    assert_eq!(j["rangeMeta"]["kind"], "batch");
    assert_eq!(j["rangeMeta"]["prefix"], "/lib/dirA");
    assert_eq!(
        j["rangeMeta"]["approx"], true,
        "老库（batchId 全 NULL）必须标 approx=true: {j}"
    );
    assert_eq!(j["totalGroups"], 1);
    assert_eq!(shape(&j)[0].1, vec![1, 2]);

    // 批次不存在 → 明确 400（不静默当全库）
    let (st, j) = get_json(app.clone(), "/api/dedup/groups?range=batch:999").await;
    assert_eq!(st, 400, "不存在的批次必须报错: {j}");
    assert!(j["error"].as_str().unwrap_or("").contains("999"));

    // 未知 range → 400
    let (st, _j) = get_json(app.clone(), "/api/dedup/groups?range=nope").await;
    assert_eq!(st, 400);
}

#[tokio::test]
async fn ignore_hides_group_and_unignore_restores_it() {
    let dir = std::env::temp_dir().join(format!("ki2-ignore-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let conn = seed_db(&dir.join("dam.db"));
    let app = app_web::router_without_dist(state(conn));

    let (_, before) = get_json(app.clone(), "/api/dedup/groups").await;
    assert_eq!(before["totalGroups"], 3);
    assert_eq!(before["ignoredCount"], 0);

    // variant 组的 groupId = min(assetId) = 3
    let (st, r) = post_json(app.clone(), "/api/dedup/ignore", r#"{"groupIds":[3]}"#).await;
    assert_eq!(st, 200, "忽略失败: {r}");
    let sig = r["ignored"][0].as_str().unwrap().to_string();
    assert_eq!(sig, "3,4", "签名 = 成员 id 升序拼接");

    let (_, after) = get_json(app.clone(), "/api/dedup/groups").await;
    assert_eq!(after["totalGroups"], 2, "被忽略的组不该再返回: {after}");
    assert_eq!(after["ignoredCount"], 1);
    let kinds: Vec<String> = shape(&after).into_iter().map(|(k, _)| k).collect();
    assert!(!kinds.contains(&"variant".to_string()), "variant 组已忽略");

    // 忽略与否和「当前看的范围」无关：dir 范围下也必须还是 1 组（exact）
    let (_, dirj) = get_json(app.clone(), "/api/dedup/groups?range=dir:%2Flib%2FdirA").await;
    assert_eq!(
        dirj["totalGroups"], 1,
        "dir 范围不应让已忽略的组复活: {dirj}"
    );

    // 撤销忽略 → 复现
    let body = format!(r#"{{"signatures":["{sig}"]}}"#);
    let (st, r) = post_json(app.clone(), "/api/dedup/unignore", &body).await;
    assert_eq!(st, 200, "撤销失败: {r}");
    assert_eq!(r["removed"], 1);
    let (_, back) = get_json(app.clone(), "/api/dedup/groups").await;
    assert_eq!(back["totalGroups"], 3, "撤销后组要回来: {back}");

    // 空请求明确 400
    let (st, _) = post_json(app.clone(), "/api/dedup/ignore", r#"{}"#).await;
    assert_eq!(st, 400);
    let (st, _) = post_json(app.clone(), "/api/dedup/unignore", r#"{}"#).await;
    assert_eq!(st, 400);
}

#[tokio::test]
async fn export_has_algorithm_metadata_and_rows() {
    let dir = std::env::temp_dir().join(format!("ki2-export-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let conn = seed_db(&dir.join("dam.db"));
    let app = app_web::router_without_dist(state(conn));

    let (st, ctype, csv) = get_raw(app.clone(), "/api/dedup/export?format=csv").await;
    assert_eq!(st, 200, "导出失败: {csv}");
    assert!(
        ctype.starts_with("text/csv"),
        "content-type 应为 text/csv: {ctype}"
    );
    // 元数据：算法 + 阈值 + 视觉塔模型（KI-6：只进报告，不进界面）
    assert!(csv.contains("# algorithm="), "缺算法元数据: {csv}");
    assert!(
        csv.contains(&format!("visualModel={}", core_config::DEDUP_EMBEDDER)),
        "缺视觉塔模型元数据: {csv}"
    );
    assert!(csv.contains("phashThreshold=8"), "缺 pHash 阈值: {csv}");
    assert!(
        csv.contains("variantThreshold=0.9"),
        "缺 variant 阈值: {csv}"
    );
    assert!(
        csv.contains("groupId,kind,size,keepAssetId"),
        "缺表头: {csv}"
    );
    assert!(csv.contains("/lib/dirA/1.jpg"), "缺成员行: {csv}");
    assert!(csv.contains("variant"), "variant 组要进报告: {csv}");

    // Markdown + range 口径
    let (st, ctype, md) = get_raw(
        app.clone(),
        "/api/dedup/export?format=md&range=dir:%2Flib%2FdirA",
    )
    .await;
    assert_eq!(st, 200, "md 导出失败: {md}");
    assert!(ctype.starts_with("text/markdown"), "content-type: {ctype}");
    assert!(md.contains("range=dir:/lib/dirA"), "报告要写明范围: {md}");
    assert!(md.contains("共 1 组"), "dirA 范围只有 1 组: {md}");

    // 非法参数
    let (st, _) = get_json(app.clone(), "/api/dedup/export?format=xlsx").await;
    assert_eq!(st, 400);
    let (st, _) = get_json(app.clone(), "/api/dedup/export?scope=scan:nope").await;
    assert_eq!(st, 404, "过期/不存在的 scan 结果要明确 404");
}

/// 库外扫描能力位必须与**形态判定同源**（desktop=true / 容器=false），且接口自身也要拦
/// —— 不能只靠 UI 隐藏。
///
/// 注意 `cargo test --workspace` 的 feature 统一：app-tauri 依赖 app-web 时开了
/// `local-infer`，整个 workspace 里 app-web 都是「桌面形态」编译（`IS_DESKTOP_FORM=true`）。
/// 所以「容器形态必须 false」用**纯函数**钉（`evaluate_features(false, …)`，与编译形态无关），
/// 本用例只钉「HTTP 层与进程形态同源」这一条 —— 两种构建下都成立。
#[tokio::test]
async fn scan_capability_matches_form() {
    let dir = std::env::temp_dir().join(format!("ki2-caps-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let conn = seed_db(&dir.join("dam.db"));
    let app = app_web::router_without_dist(state(conn));

    // ① 纯函数（与构建 feature 无关）：容器形态一律 false
    let web_feats = svc_library::capabilities::evaluate_features(false, &[]);
    assert!(
        !web_feats.scan_external_path,
        "容器形态不得承诺读本机库外路径（UI 要隐藏入口，不灰置）"
    );
    let desk_feats = svc_library::capabilities::evaluate_features(true, &[]);
    assert!(
        desk_feats.scan_external_path,
        "桌面形态必须可用（否则库外扫描永远进不去）"
    );

    // ② HTTP 层：能力位必须与进程形态同源（不出现「接口说能、UI 不敢显示」这种错位）
    let desktop = app_web::is_desktop_form();
    let (st, caps) = get_json(app.clone(), "/api/capabilities").await;
    assert_eq!(st, 200);
    assert_eq!(
        caps["features"]["scanExternalPath"].as_bool(),
        Some(desktop),
        "scanExternalPath 必须跟 isDesktop 同源: {caps}"
    );
    if !desktop {
        assert!(
            caps["featureReasons"]["scanExternalPath"]
                .as_str()
                .unwrap_or("")
                .contains("服务器"),
            "false 的项要给人话原因: {caps}"
        );
        // 不传 capabilities 也不能扫：接口层同样拒绝
        let (st, j) = post_json(app.clone(), "/api/dedup/scan", r#"{"path":"/tmp"}"#).await;
        assert_eq!(st, 400, "容器形态必须拒绝库外扫描: {j}");
        assert!(j["error"].as_str().unwrap_or("").contains("桌面版"));
    }
}
