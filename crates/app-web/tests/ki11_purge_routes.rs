//! KI-11 回归钉：`POST /api/dedup/purge`（回收站 → 彻底删除）走 HTTP 路由真跑。
//!
//! 本文件是**独立测试二进制**，进程级 env 只在这里设：`DAM_WEB_FORM=desktop`
//! （与真桌面壳 / `scripts/webcap_acceptance.sh` 同一口径）。容器形态的 400 拒绝在
//! `crates/app-web/src/lib.rs` 的 `ki11_purge_blocked_in_web_form` 里断言（那边默认非桌面形态）。
//!
//! 断言的都是用户能看见的结果：
//! 1. 只接受回收站内（deleted=1）的 id，未软删/不存在的进 skipped（不静默放过）；
//! 2. copyin 库内副本原图被删；reference 用户原地文件**仍在**（磁盘边界）；
//! 3. 缩略图两档文件消失、库行消失、freedBytes 只统计真实删掉的字节；
//! 4. 第二次调用幂等（purged=0）；
//! 5. 入参校验：空 ids / 超上限 → 400。
use axum::body::Body;
use axum::http::Request;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tower::ServiceExt;

const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0, 0, 0,
    1, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78,
    0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xF0, 0x1F, 0, 5, 0, 1, 0xFF, 0x89, 0x99, 0x3D, 0x1D, 0, 0, 0, 0,
    0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

struct Fx {
    data_dir: PathBuf,
    copyin_abs: PathBuf,
    reference_abs: PathBuf,
}

fn thumb(data_dir: &Path, id: i64, size: i32) -> PathBuf {
    data_dir
        .join("thumbs")
        .join(size.to_string())
        .join(format!("{:02}", id % 100))
        .join(format!("{id}_{size}.webp"))
}

/// 3 张资产：1 = 回收站内 copyin、2 = 回收站内 reference、3 = 仍在库（deleted=0）。
fn seed(data_dir: &Path) -> (Connection, Fx) {
    let copyin_abs = data_dir.join("library/aa.png");
    let reference_abs = data_dir.join("src/bb.png");
    std::fs::create_dir_all(copyin_abs.parent().unwrap()).unwrap();
    std::fs::create_dir_all(reference_abs.parent().unwrap()).unwrap();
    std::fs::write(&copyin_abs, PNG).unwrap();
    std::fs::write(&reference_abs, PNG).unwrap();

    let conn = core_storage::open(&data_dir.join("dam.db")).unwrap();
    for (id, path, mode, deleted) in [
        (1i64, &copyin_abs, "copyin", 1i64),
        (2, &reference_abs, "reference", 1),
        (3, &reference_abs, "reference", 0),
    ] {
        conn.execute(
            "INSERT INTO assets (assetId, absPath, storeMode, missing, fileName, fileExt, fileSz,
                                 mtime, importedAt, deleted)
             VALUES (?1, ?2, ?3, 0, ?4, 'png', ?5, 0, 0, ?6)",
            rusqlite::params![
                id,
                path.to_str().unwrap(),
                mode,
                format!("{id}.png"),
                PNG.len() as i64,
                deleted
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1,'primary',0,1,1)",
            [id],
        )
        .unwrap();
    }
    // 缩略图：1/2 各有 256/1024 两档（3 号没有 —— 顺带覆盖「NotFound 不计入 filesDeleted」）
    for id in [1i64, 2] {
        for size in core_media::THUMB_SIZES {
            let p = thumb(data_dir, id, *size);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"t").unwrap();
        }
    }
    (
        conn,
        Fx {
            data_dir: data_dir.to_path_buf(),
            copyin_abs,
            reference_abs,
        },
    )
}

async fn post(app: &axum::Router, uri: &str, body: Value) -> (u16, Value) {
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
    let raw = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    (status, serde_json::from_slice(&raw).unwrap_or(Value::Null))
}

#[tokio::test]
async fn purge_route_end_to_end_in_desktop_form() {
    // 桌面形态钩子（与真桌面壳同一判定 is_desktop_form()）——能力位 physicalDelete 才为 true
    std::env::set_var("DAM_WEB_FORM", "desktop");

    let data_dir = std::env::temp_dir().join(format!("dam-ki11-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    std::fs::create_dir_all(&data_dir).unwrap();
    let (conn, fx) = seed(&data_dir);

    let state = app_web::test_support::state(data_dir.clone(), conn);
    let app = app_web::router_without_dist(state.clone());

    // 能力位自查：桌面形态 + 数据目录可写 → physicalDelete = true（门不会被误拦）
    let caps = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/capabilities")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let caps_raw = axum::body::to_bytes(caps.into_body(), 1 << 20).await.unwrap();
    let caps: Value = serde_json::from_slice(&caps_raw).unwrap();
    assert_eq!(
        caps["features"]["physicalDelete"], true,
        "桌面钩子形态应允许物理删除: {caps}"
    );

    // 1) 3 号未软删 + 999 不存在 → 只删 1/2，其余如实 skipped
    let (st, body) = post(
        &app,
        "/api/dedup/purge",
        json!({"ids": [1, 2, 3, 999]}),
    )
    .await;
    assert_eq!(st, 200, "purge 应 200: {body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["purged"], 2, "{body}");
    assert_eq!(body["skipped"], 2, "{body}");
    assert_eq!(body["purgedIds"], json!([1, 2]));
    assert_eq!(body["skippedIds"], json!([3, 999]));
    assert_eq!(body["thumbs"], 4, "两档 × 2 张: {body}");
    assert_eq!(
        body["filesDeleted"], 5,
        "4 缩略图 + 1 个 copyin 原图: {body}"
    );
    let freed = body["freedBytes"].as_u64().unwrap();
    assert_eq!(
        freed,
        (PNG.len() as u64) + 4 * 1,
        "只统计真实删掉的字节: {body}"
    );

    // 2) 磁盘边界：copyin 原图删了、reference 原地文件必须还在、缩略图全没
    assert!(!fx.copyin_abs.exists(), "copyin 库内副本应被删除");
    assert!(fx.reference_abs.is_file(), "reference 用户原地文件不许动");
    for id in [1i64, 2] {
        for size in core_media::THUMB_SIZES {
            assert!(!thumb(&data_dir, id, *size).exists(), "{id}/{size} 缩略图应删除");
        }
    }

    // 3) 库行：1/2 全清（含 resource），3 完好
    {
        let conn = state.db.lock().await;
        let n = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(n("SELECT COUNT(*) FROM assets WHERE assetId IN (1,2)"), 0);
        assert_eq!(n("SELECT COUNT(*) FROM resource WHERE assetId IN (1,2)"), 0);
        assert_eq!(n("SELECT COUNT(*) FROM assets WHERE assetId=3"), 1);
        assert_eq!(n("SELECT COUNT(*) FROM resource WHERE assetId=3"), 1);
    }

    // 4) 幂等：再删同一批 → purged=0，全部如实 skipped
    let (st, body) = post(&app, "/api/dedup/purge", json!({"ids": [1, 2]})).await;
    assert_eq!(st, 200);
    assert_eq!(body["purged"], 0, "第二次调用不应再删任何东西: {body}");
    assert_eq!(body["skipped"], 2);
    assert_eq!(body["filesDeleted"], 0);
    assert_eq!(body["freedBytes"], 0);

    // 5) 入参校验
    assert_eq!(
        post(&app, "/api/dedup/purge", json!({"ids": []})).await.0,
        400
    );
    let too_many: Vec<i64> = (1..=901).collect();
    let (st, body) = post(&app, "/api/dedup/purge", json!({"ids": too_many})).await;
    assert_eq!(st, 400, "{body}");

    let _ = std::fs::remove_dir_all(&data_dir);
}
