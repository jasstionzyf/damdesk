//! G5-A（卡 task）路由级验收：失联**失败即标**，没有「核查」动作。
//!
//! 复刻形态：文件被外部删掉（改名/移走/拔盘），watcher 没看到 → DB 里 `missing=0`
//! 却是个谎。真正碰磁盘的入口 `/api/media/original/{id}`（分享出去的原图链接）
//! 一读就失败 —— 这次失败必须顺手把该资产标 `missing=1`，图库「失联 N」行/`?missing=1`
//! 筛选随即能看到它；文件放回后同一入口读成功 → 清回 `missing=0`。
//!
//! 断言全部走 HTTP（接口原始返回），不直接改库假装结果。

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
        "missing-on-failure-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

/// 最小合法 PNG（1×1 RGBA）
const PNG_1X1: [u8; 70] = [
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0, 0, 0,
    1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78,
    0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xF0, 0x1F, 0, 5, 0, 1, 0, 0xFF, 0x89, 0x99, 0x3D, 0x1D, 0, 0, 0,
    0, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

fn touch(p: &Path) -> PathBuf {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, PNG_1X1).unwrap();
    p.to_path_buf()
}

/// 建库 + 2 条资产（kept / gone），gone 的源文件已被外部删除、DB 仍 missing=0。
/// 白名单根用 import_batches.rootPath 提供（与真实导入批次同一条路径）。
fn seed_state() -> (AppState, PathBuf) {
    let data_dir = tmp_dir();
    std::fs::create_dir_all(&data_dir).unwrap();
    let conn = core_storage::open(&data_dir.join("dam.db")).unwrap();
    let lib = data_dir.join("lib");
    let kept = touch(&lib.join("kept.png"));
    let gone = touch(&lib.join("gone.png"));
    conn.execute(
        "INSERT INTO import_batches (rootPath, storeMode, status, totalFiles, doneFiles, startedAt)
         VALUES (?1, 'reference', 'done', 2, 2, 1)",
        [lib.to_string_lossy()],
    )
    .unwrap();
    for (id, p) in [(1, &kept), (2, &gone)] {
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
    std::fs::remove_file(&gone).unwrap();
    let state = app_web::test_support::state(data_dir.clone(), conn);
    (state, data_dir)
}

async fn call(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
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
async fn original_read_failure_marks_missing_and_return_clears_it() {
    let (state, dir) = seed_state();
    let app = app_web::router_without_dist(state);

    // 0) 读之前：DB 撒谎（missing=0），筛选拿不到任何失联
    let (st, list) = call(&app, "/api/library/assets?missing=1&limit=10").await;
    assert_eq!(st, StatusCode::OK, "{list}");
    assert_eq!(
        list["total"], 0,
        "删文件不经过 watcher → 还没人记账: {list}"
    );

    // 1) 原图链接（分享出去的就是这条）读失败 → 404
    let (st, body) = call(&app, "/api/media/original/2").await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");

    // 2) 失败即标：图库 missing=1 现在能看到它（唯一入口的筛选口径）
    let (st, list) = call(&app, "/api/library/assets?missing=1&limit=10").await;
    assert_eq!(st, StatusCode::OK, "{list}");
    assert_eq!(list["total"], 1, "读失败必须当场落库: {list}");
    assert!(list["items"][0]["absPath"]
        .as_str()
        .unwrap()
        .ends_with("gone.png"));
    // 读成功的那张不受影响
    let (_, all) = call(&app, "/api/library/assets?limit=10").await;
    assert_eq!(all["total"], 2);

    // 3) 文件放回 → 同一入口读成功 → 清标（不需要额外核查动作）
    std::fs::write(dir.join("lib/gone.png"), PNG_1X1).unwrap();
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/media/original/2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "文件回来了就该读得到");
    let (_, list) = call(&app, "/api/library/assets?missing=1&limit=10").await;
    assert_eq!(list["total"], 0, "读成功顺手清标: {list}");

    // 4) 权限/其它 IO 错误不折进失联（只测口径函数，DAC 在 root 下构造不出来）
    assert!(!core_storage::presence::is_gone(&std::io::Error::from(
        std::io::ErrorKind::PermissionDenied
    )));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn missing_filter_row_entry_contract_unchanged() {
    // 保留的唯一入口 = `/api/library/assets?missing=1`（图库侧栏「失联 N」行的点击语义）。
    // 本用例只钉契约形态：不动字段名、不动筛选语义。
    let (state, dir) = seed_state();
    let app = app_web::router_without_dist(state);
    let (st, list) = call(&app, "/api/library/assets?missing=1&limit=10").await;
    assert_eq!(st, StatusCode::OK, "{list}");
    assert!(list["items"].is_array());
    assert!(list.get("total").is_some());
    let _ = std::fs::remove_dir_all(&dir);
}
