//! DESKTOP-MISSING-STAT-1 路由级验收：失联统计必须现场核查 + 带新鲜度标记。
//!
//! 复刻线上事故形态：库里有 3 张图，其中 1 张的源文件被**外部**删除（进程没在跑 /
//! watcher 没看到），随后：
//!   1) stats 仍然报旧值（证明"只读 missing 字段会撒谎"）；
//!   2) stats 必须显式报 missingStale=true（从未核查）—— 不撒谎的诚实标记；
//!   3) POST /api/library/verify 现场 stat → missing 与实际一致，再查 stats 已不 stale；
//!   4) UI 的失联筛选（?missing=1）能取到那张图；重复核查幂等（单飞不残留）。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tower::ServiceExt;

use app_web::AppState;

static SEQ: AtomicU64 = AtomicU64::new(0);

fn tmp_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "missing-stat-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

/// 最小合法 PNG（1×1 RGBA）。D3 起核查会做解码校验 → 夹具必须是**真图片**，
/// 否则这些资产会被如实判成「存在但解不开」，混进 samples 打乱本例的失联断言。
const PNG_1X1: [u8; 70] = [
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0, 0, 0,
    1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0, 0x0D, 0x49, 0x44, 0x41, 0x54,
    0x78, 0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xF0, 0x1F, 0, 5, 0, 1, 0xFF, 0x89, 0x99, 0x3D, 0x1D, 0, 0,
    0, 0, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

fn touch(p: &Path) -> PathBuf {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, PNG_1X1).unwrap();
    p.to_path_buf()
}

/// 建库 + 3 条资产（其中 1 条源文件已被外部删除）→ AppState。
fn seed_state() -> (AppState, PathBuf) {
    let data_dir = tmp_dir();
    std::fs::create_dir_all(&data_dir).unwrap();
    let conn = core_storage::open(&data_dir.join("dam.db")).unwrap();
    let lib = data_dir.join("lib");
    let kept = touch(&lib.join("kept.png"));
    let gone = touch(&lib.join("gone.png"));
    let kept2 = touch(&lib.join("kept2.png"));
    for (id, p) in [(1, &kept), (2, &gone), (3, &kept2)] {
        conn.execute(
            "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (?1, ?2, 0, ?3, 'png', 1, 1, 1)",
            rusqlite::params![
                id,
                p.to_string_lossy(),
                p.file_name().unwrap().to_string_lossy()
            ],
        )
        .unwrap();
    }
    // 外部删除（不经过 watcher）
    std::fs::remove_file(&gone).unwrap();
    let state = app_web::test_support::state(data_dir.clone(), conn);
    (state, data_dir)
}

fn router_with(state: AppState) -> axum::Router {
    app_web::router_without_dist(state)
}

async fn call(app: &axum::Router, method: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let body: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, body)
}

#[tokio::test]
async fn stats_is_honest_and_verify_makes_it_true() {
    let (state, _dir) = seed_state();
    let app = router_with(state);

    // 1) 核查前：missing 仍是旧值 0（这就是线上看到的撒谎形态）
    let (st, body) = call(&app, "GET", "/api/library/stats").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["assets"], 3);
    assert_eq!(body["missing"], 0, "未核查时它就是旧值 —— 卡面事故原形");
    // 2) 诚实标记：从未核查 → stale，且没有假时间戳
    assert_eq!(body["missingStale"], true, "{body}");
    assert!(body["missingCheckedAt"].is_null(), "{body}");
    assert_eq!(body["missingStaleSecs"], 600);

    // 3) 现场核查 → 与实际一致
    let (st, rep) = call(&app, "POST", "/api/library/verify").await;
    assert_eq!(st, StatusCode::OK, "{rep}");
    assert_eq!(rep["checked"], 3);
    assert_eq!(rep["notFound"], 1, "{rep}");
    assert_eq!(rep["missingBefore"], 0);
    assert_eq!(rep["missing"], 1, "核查后 missing 必须等于真实删除数");
    assert_eq!(rep["newlyMissing"], 1);
    assert_eq!(rep["recovered"], 0);
    assert_eq!(rep["unreadable"], 0);
    assert!(rep["durationMs"].as_i64().unwrap() >= 0);
    assert!(rep["checkedAt"].as_i64().unwrap() > 0);
    assert_eq!(rep["samples"][0]["class"], "notFound", "{rep}");

    // 4) 再查 stats：值对了、不再 stale、带核查时间
    let (st, body) = call(&app, "GET", "/api/library/stats").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["missing"], 1);
    assert_eq!(body["missingStale"], false, "{body}");
    assert!(body["missingCheckedAt"].as_i64().unwrap() > 0, "{body}");
    assert_eq!(body["missingUnreadable"], 0);

    // 5) UI 失联筛选能取到那张图
    let (st, list) = call(&app, "GET", "/api/library/assets?missing=1&limit=10").await;
    assert_eq!(st, StatusCode::OK, "{list}");
    assert_eq!(list["total"], 1, "{list}");
    assert!(list["items"][0]["absPath"]
        .as_str()
        .unwrap()
        .ends_with("gone.png"));

    // 6) 重复核查幂等（单飞 guard 释放；数字不变）
    let (st, rep2) = call(&app, "POST", "/api/library/verify").await;
    assert_eq!(st, StatusCode::OK, "重复核查不该被单飞卡死: {rep2}");
    assert_eq!(rep2["missing"], 1);
    assert_eq!(rep2["notFound"], 1);
    assert_eq!(rep2["newlyMissing"], 0, "第二次没有新失联");
}

#[tokio::test]
async fn verify_is_single_flight_while_running() {
    let (state, _dir) = seed_state();
    let app = router_with(state.clone());
    // 手工占住单飞位（模拟核查在跑）→ 第二次请求必须 409，而不是叠加一轮全库 IO
    let guard = state.verify.try_begin().expect("抢到核查位");
    assert!(state.verify.is_running());
    let (st, body) = call(&app, "POST", "/api/library/verify").await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["running"], true);
    drop(guard);
    assert!(!state.verify.is_running());
    let (st, _) = call(&app, "POST", "/api/library/verify").await;
    assert_eq!(st, StatusCode::OK, "释放后可再核查");
}

#[tokio::test]
async fn empty_library_verifies_and_clears_stale() {
    let data_dir = tmp_dir();
    std::fs::create_dir_all(&data_dir).unwrap();
    let conn = core_storage::open(&data_dir.join("dam.db")).unwrap();
    let app = router_with(app_web::test_support::state(data_dir, conn));

    let (_, body) = call(&app, "GET", "/api/library/stats").await;
    assert_eq!(body["assets"], 0);
    assert_eq!(body["missingStale"], true, "新库从未核查");

    let (st, rep) = call(&app, "POST", "/api/library/verify").await;
    assert_eq!(st, StatusCode::OK, "{rep}");
    assert_eq!(rep["checked"], 0);
    assert_eq!(rep["missing"], 0);

    let (_, body) = call(&app, "GET", "/api/library/stats").await;
    assert_eq!(body["missingStale"], false, "空库核查也算核查过");
}

/// 老记录的向后兼容：settings 里 missing_check 是坏 JSON 时，stats 不能炸
/// （只降级为 stale=true）。
#[tokio::test]
async fn corrupt_check_record_does_not_break_stats() {
    let data_dir = tmp_dir();
    std::fs::create_dir_all(&data_dir).unwrap();
    let conn = core_storage::open(&data_dir.join("dam.db")).unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO settings (key, value, updatedAt) VALUES ('missing_check', '{{not json', 1)",
        [],
    )
    .unwrap();
    let app = router_with(app_web::test_support::state(data_dir, conn));
    let (st, body) = call(&app, "GET", "/api/library/stats").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["missingStale"], true);
    assert!(body["missingCheckedAt"].is_null());
}
