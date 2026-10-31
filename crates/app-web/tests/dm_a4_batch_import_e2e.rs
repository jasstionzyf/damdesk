#![allow(clippy::await_holding_lock)] // vips 全局锁刻意贯穿 await：测试间串行化 libvips（非 DB 锁，无死锁面）
//! DM-A4（卡 task，KI-2 残项）：**真导入 → 精确批次范围**端到端。
//!
//! 卡面验收第 1 条就是这条链路，必须用真导入跑（不是手插 `assets.batchId` 自证）：
//! 同一个目录分两次导入（第一次 3 张、第二次新增 2 张），然后：
//!
//! * `range=batch:<第一次>` → 只看见第一次的 3 张（本批资产 3，组内成员 3）；
//! * `range=batch:<第二次>` → 只看见第二次新增的 2 张（本批资产 2），第一次的 3 张
//!   一张都不许出现（老口径按 rootPath 前缀会把 5 张全圈进来）。
//!
//! 图都是纯色（libvips black + linear）：AC 系数全 0 → pHash 相同 → 5 张落进**同一个**
//! near 组。这正好把「同一组跨两批」这个最难的情况摆上台面：范围裁剪后每批只看自己那几个成员。
//!
//! `cargo test -p app-web --test dm_a4_batch_import_e2e -- --nocapture` 会把两段
//! 原始 JSON 打出来（卡面「贴 JSON」的凭据）。

use axum::body::Body;
use axum::http::Request;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tower::ServiceExt;

fn vips_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| {
        core_media::init_backend().expect("libvips 初始化失败");
        Mutex::new(())
    })
}

/// 纯色 PNG（black + linear）：任何亮度的 AC 系数都是 0 → pHash 全 0（同一条 near 判据）。
fn make_solid_png(path: &Path, brighten: f64) {
    let black = libvips::ops::black(64, 64).unwrap();
    let img = libvips::ops::linear(&black, &mut [1.0], &mut [brighten]).unwrap();
    libvips::ops::pngsave(&img, &path.to_string_lossy()).unwrap();
}

async fn get_json(app: axum::Router, uri: &str) -> (u16, serde_json::Value) {
    let res = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let body = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

fn member_ids(j: &serde_json::Value) -> Vec<i64> {
    let mut ids: Vec<i64> = j["groups"]
        .as_array()
        .map(|gs| {
            gs.iter()
                .flat_map(|g| {
                    g["members"]
                        .as_array()
                        .map(|ms| {
                            ms.iter()
                                .map(|m| m["assetId"].as_i64().unwrap())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default();
    ids.sort_unstable();
    ids
}

fn open_db(dir: &Path) -> Connection {
    std::fs::create_dir_all(dir).unwrap();
    core_storage::open(&dir.join("dam.db")).unwrap()
}

#[tokio::test]
async fn two_imports_same_dir_split_exactly_by_batch() {
    let _g = vips_lock().lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let work = tmp.path().join("app");
    let thumbs = work.join("thumbs");
    std::fs::create_dir_all(&thumbs).unwrap();
    let conn = open_db(&work);

    // ---- 第一次导入：3 张 ----
    for i in 1..=3 {
        make_solid_png(&root.join(format!("a{i}.png")), (i as f64) * 50.0);
    }
    let b1 = svc_import::create_batch(&conn, &root, "reference", 3).unwrap();
    let o1 = svc_import::run_batch(&conn, b1, None, &thumbs, |_| Ok(())).unwrap();
    assert_eq!((o1.imported, o1.updated, o1.unchanged), (3, 0, 0), "{o1:?}");

    // ---- 第二次导入同一目录：新增 2 张（老 3 张走 Unchanged 快路径）----
    for i in 4..=5 {
        make_solid_png(&root.join(format!("a{i}.png")), (i as f64) * 50.0);
    }
    let b2 = svc_import::create_batch(&conn, &root, "reference", 5).unwrap();
    let o2 = svc_import::run_batch(&conn, b2, None, &thumbs, |_| Ok(())).unwrap();
    assert_eq!(
        (o2.imported, o2.updated, o2.unchanged),
        (2, 0, 3),
        "第二次：新增 2，老 3 张 Unchanged: {o2:?}"
    );

    // 库内 5 张，批次归属 3 / 2
    let per_batch: Vec<(Option<i64>, i64)> = {
        let mut stmt = conn
            .prepare("SELECT batchId, COUNT(*) FROM assets GROUP BY batchId ORDER BY batchId")
            .unwrap();
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(
        per_batch,
        vec![(Some(b1), 3), (Some(b2), 2)],
        "{per_batch:?}"
    );

    let app = app_web::router_without_dist(app_web::test_support::state(work.clone(), conn));

    // ---- 全库：5 张同一个 near 组 ----
    let (st, all) = get_json(app.clone(), "/api/dedup/groups").await;
    assert_eq!(st, 200, "{all}");
    assert_eq!(all["totalGroups"], 1, "{all}");
    assert_eq!(
        all["totalAssets"], 5,
        "5 张纯色图 pHash 相同 → 一个组: {all}"
    );

    // ---- 批次 1：只有第一次的 3 张 ----
    let (st, j1) = get_json(app.clone(), &format!("/api/dedup/groups?range=batch:{b1}")).await;
    assert_eq!(st, 200, "{j1}");
    println!(
        "\n[DM-A4] range=batch:{b1} →\n{}",
        serde_json::to_string_pretty(&j1).unwrap()
    );
    assert_eq!(
        j1["rangeMeta"]["approx"], false,
        "本批带 batchId → 精确: {j1}"
    );
    assert_eq!(j1["rangeMeta"]["batchAssets"], 3, "第一次导入 3 张: {j1}");
    assert_eq!(j1["totalAssets"], 3, "本批只返回自己导进来的 3 张: {j1}");
    assert_eq!(member_ids(&j1).len(), 3, "{j1}");
    assert_eq!(
        j1["rangeMeta"]["outOfScopeAssets"], 2,
        "第二次的 2 张被裁掉，必须报出来（范围外还有重复）: {j1}"
    );

    // ---- 批次 2：只有第二次新增的 2 张（老口径会把 5 张全圈进来）----
    let (st, j2) = get_json(app.clone(), &format!("/api/dedup/groups?range=batch:{b2}")).await;
    assert_eq!(st, 200, "{j2}");
    println!(
        "\n[DM-A4] range=batch:{b2} →\n{}",
        serde_json::to_string_pretty(&j2).unwrap()
    );
    assert_eq!(j2["rangeMeta"]["approx"], false, "{j2}");
    assert_eq!(j2["rangeMeta"]["batchAssets"], 2, "第二次只新增 2 张: {j2}");
    assert_eq!(j2["totalAssets"], 2, "本批只返回新增的 2 张: {j2}");
    assert_eq!(
        j2["rangeMeta"]["outOfScopeAssets"], 3,
        "第一次的 3 张必须被裁掉: {j2}"
    );

    // ---- 两批成员互斥（这是「精确」的定义）----
    let ids1 = member_ids(&j1);
    let ids2 = member_ids(&j2);
    assert!(
        ids1.iter().all(|i| !ids2.contains(i)),
        "两批成员不许交叉: batch{b1}={ids1:?} batch{b2}={ids2:?}"
    );
    assert_eq!(ids1.len() + ids2.len(), 5, "5 张全被各自的批次覆盖");
}

/// 老库现场（0033 之前导入的行 `batchId IS NULL`）：`batch:<id>` 回退 rootPath 近似，
/// 并在 `rangeMeta.approx=true` 明示 —— 用真导入建库，再把 batchId 抹成 NULL 模拟老库。
#[tokio::test]
async fn legacy_rows_fall_back_to_approx_flag() {
    let _g = vips_lock().lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let work = tmp.path().join("app");
    let thumbs = work.join("thumbs");
    std::fs::create_dir_all(&thumbs).unwrap();
    let conn = open_db(&work);
    for i in 1..=2 {
        make_solid_png(&root.join(format!("l{i}.png")), (i as f64) * 50.0);
    }
    let b = svc_import::create_batch(&conn, &root, "reference", 2).unwrap();
    svc_import::run_batch(&conn, b, None, &thumbs, |_| Ok(())).unwrap();
    // 模拟 0033 之前建出来的老库：归属列不存在 → 迁到新库后全是 NULL
    conn.execute("UPDATE assets SET batchId = NULL", [])
        .unwrap();

    let app = app_web::router_without_dist(app_web::test_support::state(work.clone(), conn));
    let (st, j) = get_json(app, &format!("/api/dedup/groups?range=batch:{b}")).await;
    assert_eq!(st, 200, "{j}");
    assert_eq!(
        j["rangeMeta"]["approx"], true,
        "老库必须标 approx=true（按目录近似），不许静默当精确: {j}"
    );
    assert_eq!(
        j["rangeMeta"]["prefix"],
        serde_json::Value::String(root.to_string_lossy().into_owned()),
        "{j}"
    );
    assert_eq!(j["totalGroups"], 1, "近似口径下同目录的 2 张仍成组: {j}");
}

#[allow(dead_code)]
fn _unused(_: PathBuf) {}
