//! W6 修复（task）回归钉：导入向导「导入后自动打标」全链路接线。
//!
//! 走真实 HTTP 路由 + 真实图片（libvips 合成）+ 真实 SQLite：
//! - POST /api/import/start {autoTag:true} → 批次 done 后 job_queue(kind='tag')
//!   出现对应新资产的 pending 行（provider 未配置 → 尽力启动失败保持 pending，
//!   导入结果不受影响）；import_batches.autoTag 落库（迁移 0010）。
//! - POST /api/import/start {autoTag:false} → 不产生任何 tag job。
use axum::body::Body;
use axum::http::Request;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tower::ServiceExt;

/// libvips 是进程级全局状态：先用 `core_media::init_backend()` 显式初始化一次（未 init 时
/// 多线程首次调用会并发注册 GObject class → BlackError / SIGABRT），再串行化本文件用 vips 的用例。
fn vips_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| {
        core_media::init_backend().expect("libvips 初始化失败");
        Mutex::new(())
    })
}

/// libvips 合成确定性灰度 PNG（与 svc-import tests/pipeline.rs 同法）。
fn make_png(path: &Path, brighten: f64) {
    let black = libvips::ops::black(64, 64).unwrap();
    let img = libvips::ops::linear(&black, &mut [1.0], &mut [brighten]).unwrap();
    libvips::ops::pngsave(&img, &path.to_string_lossy()).unwrap();
}

fn fixture_dir(root: &Path, n: usize) {
    std::fs::create_dir_all(root).unwrap();
    for i in 0..n {
        // 亮度互异 → fileHash 不同 → 全部 imported（无去重干扰）
        make_png(&root.join(format!("img{i}.png")), 30.0 + i as f64 * 25.0);
    }
}

fn spawn_app(tag: &str) -> (axum::Router, PathBuf) {
    let data = std::env::temp_dir().join(format!("w6-autotag-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let conn = core_storage::open(&data.join("dam.db")).unwrap();
    let state = app_web::test_support::state(data.clone(), conn);
    (app_web::router_without_dist(state), data)
}

async fn post_json(app: &axum::Router, uri: &str, body: &str) -> serde_json::Value {
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
    let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, 200, "{uri} → {j}");
    j
}

async fn get_json(app: &axum::Router, uri: &str) -> serde_json::Value {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 200, "{uri}");
    let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

/// 轮询 /api/import/status 直到批次不再 running，返回 outcome。
async fn wait_import_done(app: &axum::Router) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let j = get_json(app, "/api/import/status").await;
        if j["running"].as_bool() != Some(true) {
            return j["outcome"].clone();
        }
        assert!(Instant::now() < deadline, "import 60s 未结束");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// tag 队列 pending 数（直接读库：与 /api/tag/status 的 stats 同口径）。
fn pending_tag_jobs(data_dir: &Path) -> i64 {
    let conn = core_storage::open(&data_dir.join("dam.db")).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM job_queue WHERE kind='tag' AND status='pending'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

#[tokio::test]
async fn import_with_autotag_enqueues_pending_tag_jobs() {
    let _vips = vips_lock().lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    fixture_dir(&tmp.path().join("photos"), 3);

    let (app, data) = spawn_app("on");

    let j = post_json(
        &app,
        "/api/import/start",
        &format!(
            "{{\"path\":\"{}\",\"storeMode\":\"reference\",\"autoTag\":true}}",
            tmp.path().join("photos").display()
        ),
    )
    .await;
    let _batch: i64 = j["batchId"].as_i64().expect("batchId");

    let outcome = wait_import_done(&app).await;
    assert_eq!(outcome["status"], "done", "导入必须成功: {outcome}");
    assert_eq!(outcome["imported"], 3);

    // 核心：autoTag:true → 3 张新图全部出现在 tag 队列 pending
    assert_eq!(
        pending_tag_jobs(&data),
        3,
        "autoTag 批次完成后必须为新图入队 pending tag jobs"
    );

    // autoTag 落库（迁移 0010）：断点续跑 resume 据此恢复行为
    let conn = core_storage::open(&data.join("dam.db")).unwrap();
    let flag: i64 = conn
        .query_row("SELECT autoTag FROM import_batches", [], |r| r.get(0))
        .unwrap();
    assert_eq!(flag, 1, "批次记录必须落库 autoTag=1");
}

#[tokio::test]
async fn import_without_autotag_enqueues_nothing() {
    let _vips = vips_lock().lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    fixture_dir(&tmp.path().join("photos"), 2);

    let (app, data) = spawn_app("off");

    let j = post_json(
        &app,
        "/api/import/start",
        &format!(
            "{{\"path\":\"{}\",\"storeMode\":\"reference\",\"autoTag\":false}}",
            tmp.path().join("photos").display()
        ),
    )
    .await;
    assert!(j["batchId"].as_i64().is_some());

    let outcome = wait_import_done(&app).await;
    assert_eq!(outcome["status"], "done", "导入必须成功: {outcome}");
    assert_eq!(outcome["imported"], 2);

    assert_eq!(
        pending_tag_jobs(&data),
        0,
        "不勾选 autoTag 不得入队任何 tag job"
    );
}

#[tokio::test]
async fn autotag_defaults_to_false_when_field_missing() {
    // 老 payload（不带 autoTag 字段）行为不变：不入队
    let _vips = vips_lock().lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    fixture_dir(&tmp.path().join("photos"), 1);

    let (app, data) = spawn_app("legacy");

    let j = post_json(
        &app,
        "/api/import/start",
        &format!(
            "{{\"path\":\"{}\",\"storeMode\":\"reference\"}}",
            tmp.path().join("photos").display()
        ),
    )
    .await;
    assert!(j["batchId"].as_i64().is_some());

    let outcome = wait_import_done(&app).await;
    assert_eq!(outcome["status"], "done");

    assert_eq!(pending_tag_jobs(&data), 0, "缺省 autoTag 必须等价 false");
}
