//! DT4（卡 task）HTTP 面回归钉：**「按标签导出」不许退化成整库**。
//!
//! 现场报告是「导出页填标签，预览命中数对、实际导出全量」。端到端复现（两形态 build +
//! 真实 UI 点击，见 `reports/DT4-export-tag-filter.md`）没复现出全量；但这条链上唯一
//! 会**静默**变成整库的位置是 HTTP 层：`POST /api/export/start` 的 `filter` 一旦没被
//! 反序列化进 `ExportSource::Filter`，导出不会报错、只会安静地导出全库。
//!
//! 所以这里走真路由（不是直接调 `run_export`）钉死四件事：
//! 1. 预览口径（`GET /api/library/assets?tag=`）与导出报告口径**同源**：都是标签命中数；
//! 2. 标签导出落盘文件数 = 标签命中数（+ manifest.csv），不是整库；
//! 3. 空 filter = 显式整库（对照组：同一库里整库数 > 标签命中数，证明 filter 真生效）；
//! 4. 入参/并发/失败语义：来源二选一 400、已在导出 409、目标目录建不出来 → 错误可见不假装成功。
use axum::body::Body;
use axum::http::Request;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tower::ServiceExt;

const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0, 0, 0,
    1, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78,
    0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xF0, 0x1F, 0, 5, 0, 1, 0xFF, 0x89, 0x99, 0x3D, 0x1D, 0, 0, 0, 0,
    0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

const TAG: &str = "海";

/// 5 张资产：1/2/3 带标签「海」，4/5 不带（且 5 已软删 → 整库口径不算它）。
fn seed(data_dir: &Path) {
    let src = data_dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let conn = core_storage::open(&data_dir.join("dam.db")).unwrap();
    for id in 1..=5i64 {
        let p = src.join(format!("p{id}.png"));
        std::fs::write(&p, PNG).unwrap();
        conn.execute(
            "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime,
                                 importedAt, deleted)
             VALUES (?1, ?2, 0, ?3, 'png', ?4, 0, ?1, ?5)",
            rusqlite::params![
                id,
                p.to_str().unwrap(),
                format!("p{id}.png"),
                PNG.len() as i64,
                if id == 5 { 1i64 } else { 0 },
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1,'primary',0,1,1)",
            [id],
        )
        .unwrap();
    }
    // 标签 schema（0021 TAG-VALUE）：tag 只有 value
    for id in [1i64, 2, 3] {
        conn.execute(
            "INSERT INTO tag (assetId, value, origin, score, createdAt) VALUES (?1, ?2, 'manual', 1.0, 1)",
            rusqlite::params![id, TAG],
        )
        .unwrap();
    }
}

fn fresh_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dam-dt4-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn call(app: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> (u16, Value) {
    let req = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(v) => req
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status().as_u16();
    let raw = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    (status, serde_json::from_slice(&raw).unwrap_or(Value::Null))
}

/// 轮询到 running=false（导出是 spawn_blocking 后台跑，最多等 10s）。
async fn wait_report(app: &axum::Router) -> Value {
    let t0 = Instant::now();
    loop {
        let (_, st) = call(app, "GET", "/api/export/status", None).await;
        if st["running"] == json!(false) {
            return st;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "导出 10s 未结束: {st}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// 目录里导出的文件（不含 manifest.csv）
fn exported_files(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n != "manifest.csv")
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

#[tokio::test]
async fn dt4_tag_filter_export_matches_preview_not_whole_library() {
    let data_dir = fresh_dir("tag");
    seed(&data_dir);
    let out_dir = data_dir.join("out");
    let state = app_web::test_support::state(
        data_dir.clone(),
        core_storage::open(&data_dir.join("dam.db")).unwrap(),
    );
    let app = app_web::router_without_dist(state);

    // 预览口径：标签命中 3（整库未删 4 张 → 两者不同，才有判别力）
    let (code, preview) = call(
        &app,
        "GET",
        "/api/library/assets?tag=%E6%B5%B7&limit=1",
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(
        preview["total"],
        json!(3),
        "预览口径应只算标签命中: {preview}"
    );
    let (_, whole) = call(&app, "GET", "/api/library/assets?limit=1", None).await;
    assert_eq!(
        whole["total"],
        json!(4),
        "整库口径（deleted=0）应为 4: {whole}"
    );

    // 导出：同一 filter → 报告与落盘都必须只有 3 张
    let (code, started) = call(
        &app,
        "POST",
        "/api/export/start",
        Some(json!({
            "filter": { "tag": TAG },
            "content": "original",
            "destDir": out_dir.to_str().unwrap(),
            "manifest": true,
        })),
    )
    .await;
    assert_eq!(code, 200, "启动失败: {started}");
    let st = wait_report(&app).await;
    let rep = &st["report"];
    assert_eq!(
        rep["total"],
        json!(3),
        "导出 total 必须是标签命中数，不是整库: {st}"
    );
    assert_eq!(
        rep["exported"],
        json!(3),
        "导出文件数必须是标签命中数: {st}"
    );
    assert_eq!(rep["skipped"], json!([]));
    assert_eq!(rep["errors"], json!([]));
    let files = exported_files(&out_dir);
    assert_eq!(
        files,
        vec!["p1.png", "p2.png", "p3.png"],
        "落盘文件必须就是标签命中的 3 张"
    );
    assert!(
        out_dir.join("manifest.csv").is_file(),
        "manifest.csv 应写入"
    );

    // 同一批资产用整库 filter 对照：4 张（含未打标签的 4 号）——证明 tag 真起了作用
    let out_all = data_dir.join("out-all");
    let (code, _) = call(
        &app,
        "POST",
        "/api/export/start",
        Some(json!({
            "filter": {},
            "content": "original",
            "destDir": out_all.to_str().unwrap(),
            "manifest": false,
        })),
    )
    .await;
    assert_eq!(code, 200);
    let st = wait_report(&app).await;
    assert_eq!(
        st["report"]["total"],
        json!(4),
        "空 filter = 整库（4 张）: {st}"
    );
    assert_eq!(exported_files(&out_all).len(), 4);
}

#[tokio::test]
async fn dt4_export_source_and_target_guards() {
    let data_dir = fresh_dir("guards");
    seed(&data_dir);
    let out_dir = data_dir.join("out");
    let state = app_web::test_support::state(
        data_dir.clone(),
        core_storage::open(&data_dir.join("dam.db")).unwrap(),
    );
    let app = app_web::router_without_dist(state.clone());

    // 来源二选一：两个都给 → 400（不猜用户意图，更不落成整库）
    let (code, body) = call(
        &app,
        "POST",
        "/api/export/start",
        Some(json!({
            "assetIds": [1],
            "filter": { "tag": TAG },
            "content": "original",
            "destDir": out_dir.to_str().unwrap(),
        })),
    )
    .await;
    assert_eq!(code, 400, "两个来源同时给应 400: {body}");
    assert!(body["error"]
        .as_str()
        .unwrap_or_default()
        .contains("二选一"));

    // 选择集来源（ExportDialog 那条路）：只导 1 张，且提示 total_hint = 1
    let (code, started) = call(
        &app,
        "POST",
        "/api/export/start",
        Some(json!({
            "assetIds": [4],
            "content": "original",
            "destDir": data_dir.join("out-ids").to_str().unwrap(),
            "manifest": false,
        })),
    )
    .await;
    assert_eq!(code, 200, "{started}");
    assert_eq!(
        started["total"],
        json!(1),
        "选择集来源的 total_hint 应是选择集大小: {started}"
    );
    assert_eq!(wait_report(&app).await["report"]["total"], json!(1));

    // 目标建不出来（/proc 下不可写）→ 错误进 status.error，不假装成功
    let (code, _) = call(
        &app,
        "POST",
        "/api/export/start",
        Some(json!({
            "filter": { "tag": TAG },
            "content": "original",
            "destDir": "/proc/dt4-not-writable/out",
            "manifest": false,
        })),
    )
    .await;
    assert_eq!(code, 200);
    let st = wait_report(&app).await;
    assert_eq!(st["report"], Value::Null, "失败时不应有报告: {st}");
    assert!(
        st["error"]
            .as_str()
            .unwrap_or_default()
            .contains("dt4-not-writable"),
        "失败原因要能看见（含目标路径）: {st}"
    );

    // 已在导出中 → 409；此时取消请求应被受理（把 cancel 位置 true）
    state
        .export
        .running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (code, body) = call(
        &app,
        "POST",
        "/api/export/start",
        Some(json!({
            "filter": { "tag": TAG },
            "content": "original",
            "destDir": out_dir.to_str().unwrap(),
        })),
    )
    .await;
    assert_eq!(code, 409, "重入应 409: {body}");
    let (code, _) = call(&app, "POST", "/api/export/cancel", Some(json!({}))).await;
    assert_eq!(code, 200, "导出中取消应 200");
    assert!(
        state
            .export
            .cancel
            .load(std::sync::atomic::Ordering::SeqCst),
        "取消位应被置起"
    );
    state
        .export
        .running
        .store(false, std::sync::atomic::Ordering::SeqCst);
    // 无导出在跑时取消 → 409（不静默吞）
    let (code, _) = call(&app, "POST", "/api/export/cancel", Some(json!({}))).await;
    assert_eq!(code, 409);
}
