//! DT6 路由钉（卡 t_fcb18ceb）：`POST /api/library/migrate-to-library`（后台跑 + 立即返回）
//! + `GET .../status` 进度快照 + `GET /api/library/migrate-audit` 审计 dump。
//!
//! 真 HTTP（tower oneshot）+ 真 SQLite + 真文件；后台任务是 spawn_blocking 的独立连接，
//! 所以断言一律用「轮询 status 到终态」而不是假设同步完成。

use axum::body::Body;
use axum::http::Request;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tower::ServiceExt;

/// 迁移任务是**进程内单槽**（同一时刻只允许一批，这是产品口径）——同文件多个用例必须
/// 串行跑，否则彼此抢槽/互清快照。锁在整个用例期间持有。
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

struct Fx {
    app: axum::Router,
    dir: PathBuf,
    conn: rusqlite::Connection,
}

fn make_fx(tag: &str) -> Fx {
    let dir = std::env::temp_dir().join(format!("dt6-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // 路由侧用独立连接（AppState 持锁），本文件的 conn 只做「造数据 + 断言」
    let conn = core_storage::open(&dir.join("dam.db")).unwrap();
    let state = app_web::test_support::state(
        dir.clone(),
        core_storage::open(&dir.join("dam.db")).unwrap(),
    );
    app_web::library_api::migrate_slot_reset();
    Fx {
        app: app_web::router_without_dist(state),
        dir,
        conn,
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        app_web::library_api::migrate_slot_reset();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Fx {
    fn add_reference(&self, name: &str, bytes: &[u8]) -> (i64, PathBuf, String) {
        let d = self.dir.join("external");
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join(name);
        std::fs::write(&p, bytes).unwrap();
        let hash = core_hash::sha256_hex(bytes);
        let ext = p
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        self.conn
            .execute(
                "INSERT INTO assets (mediaType, absPath, storeMode, fileName, fileExt, fileSz,
                                     mtime, fileHash, importedAt)
                 VALUES ('image', ?1, 'reference', ?2, ?3, ?4, 0, ?5, unixepoch())",
                rusqlite::params![p.to_string_lossy(), name, ext, bytes.len() as i64, hash],
            )
            .unwrap();
        (self.conn.last_insert_rowid(), p, hash)
    }

    fn lib_root(&self) -> PathBuf {
        self.dir.join("library")
    }

    fn abs_path(&self, id: i64) -> String {
        self.conn
            .query_row("SELECT absPath FROM assets WHERE assetId=?1", [id], |r| r.get(0))
            .unwrap()
    }

    fn store_mode(&self, id: i64) -> String {
        self.conn
            .query_row("SELECT storeMode FROM assets WHERE assetId=?1", [id], |r| r.get(0))
            .unwrap()
    }
}

async fn post_json(app: &axum::Router, uri: &str, body: &str) -> (u16, serde_json::Value) {
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
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

async fn get_json(app: &axum::Router, uri: &str) -> (u16, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

/// 轮询 status 直到离开 running（后台任务是 spawn_blocking，天然异步）。
async fn wait_done(app: &axum::Router, batch: &str) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (st, j) = get_json(
            app,
            &format!("/api/library/migrate-to-library/status?batchId={batch}"),
        )
        .await;
        assert_eq!(st, 200, "{j}");
        if j["state"] != "running" {
            return j;
        }
        assert!(Instant::now() < deadline, "迁移任务 30s 未结束：{j}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn migrate_http_copies_files_and_reports_progress() {
    let _g = serial();
    let fx = make_fx("happy");
    let (a, pa, ha) = fx.add_reference("a.jpg", b"aaa-payload");
    let (b, pb, hb) = fx.add_reference("b.jpg", b"bbbb-payload-longer");

    let (st, j) = post_json(
        &fx.app,
        "/api/library/migrate-to-library",
        &format!(r#"{{"ids":[{a},{b}]}}"#),
    )
    .await;
    assert_eq!(st, 202, "{j}");
    assert_eq!(j["state"], "running");
    assert_eq!(j["total"], 2);
    let batch = j["batchId"].as_str().expect("必须回传 batchId").to_string();

    let fin = wait_done(&fx.app, &batch).await;
    assert_eq!(fin["state"], "done", "{fin}");
    assert_eq!(fin["batchId"], batch.as_str());
    assert_eq!(fin["migrated"], 2);
    assert_eq!(fin["skipped"], 0);
    assert_eq!(fin["failed"], 0);
    assert_eq!(
        fin["bytes"].as_i64().unwrap(),
        (b"aaa-payload".len() + b"bbbb-payload-longer".len()) as i64
    );
    assert!(fin["finishedAt"].as_i64().unwrap() > 0);
    assert_eq!(fin["report"]["migrated"], 2);

    for (id, old, hash, name, content) in [
        (a, &pa, &ha, "a.jpg", &b"aaa-payload"[..]),
        (b, &pb, &hb, "b.jpg", &b"bbbb-payload-longer"[..]),
    ] {
        let expect = fx.lib_root().join(&hash[..16]).join(name);
        assert_eq!(PathBuf::from(fx.abs_path(id)), expect);
        assert_eq!(std::fs::read(&expect).unwrap(), content);
        assert!(old.is_file(), "源文件默认保留");
        assert_eq!(fx.store_mode(id), "copyin");
    }

    // 审计 dump：3 项审计要素（人 / 字节数 / 原目录→库根）都在
    let (st, j) = get_json(&fx.app, &format!("/api/library/migrate-audit?batchId={batch}")).await;
    assert_eq!(st, 200);
    assert_eq!(j["count"], 2, "{j}");
    let rows = j["rows"].as_array().unwrap();
    assert!(rows.iter().all(|r| r["status"] == "done" && r["kind"] == "copyin"));
    assert!(rows.iter().all(|r| r["operator"] == "local" && r["at"].as_i64().unwrap() > 0));
    assert!(rows
        .iter()
        .all(|r| r["from"].as_str().unwrap().contains("/external/")));
    assert!(rows.iter().all(|r| r["to"].as_str().unwrap().contains("/library/")));
}

#[tokio::test]
async fn delete_source_flag_and_operator_pass_through() {
    let _g = serial();
    let fx = make_fx("delsrc");
    let (a, pa, _ha) = fx.add_reference("a.jpg", b"payload");
    let (st, j) = post_json(
        &fx.app,
        "/api/library/migrate-to-library",
        &format!(r#"{{"ids":[{a}],"deleteSource":true,"operator":"bob"}}"#),
    )
    .await;
    assert_eq!(st, 202, "{j}");
    let batch = j["batchId"].as_str().unwrap().to_string();
    let fin = wait_done(&fx.app, &batch).await;
    assert_eq!(fin["migrated"], 1, "{fin}");
    assert!(!pa.exists(), "deleteSource:true 应删源");
    assert!(PathBuf::from(fx.abs_path(a)).is_file());
    let (_, j) = get_json(&fx.app, &format!("/api/library/migrate-audit?batchId={batch}")).await;
    assert_eq!(j["rows"][0]["operator"], "bob");
}

#[tokio::test]
async fn skips_are_reported_per_asset_not_as_errors() {
    let _g = serial();
    let fx = make_fx("skips");
    let (gone, p_gone, _h1) = fx.add_reference("gone.jpg", b"gone");
    std::fs::remove_file(&p_gone).unwrap();
    let (ok, _p, _h2) = fx.add_reference("ok.jpg", b"fine");

    let (st, j) = post_json(
        &fx.app,
        "/api/library/migrate-to-library",
        &format!(r#"{{"ids":[{gone},{ok}]}}"#),
    )
    .await;
    assert_eq!(st, 202, "{j}");
    let batch = j["batchId"].as_str().unwrap().to_string();
    let fin = wait_done(&fx.app, &batch).await;
    // 一条跳过不影响另一条（900 张的批次不该被一张坏图卡死）
    assert_eq!(fin["migrated"], 1, "{fin}");
    assert_eq!(fin["skipped"], 1);
    assert_eq!(fin["failed"], 0);
    let skips = fin["report"]["skipped"].as_array().unwrap();
    assert_eq!(skips[0]["assetId"].as_i64().unwrap(), gone);
    assert_eq!(skips[0]["reason"], "missing_file");
    assert!(!skips[0]["detail"].as_str().unwrap().is_empty());
    assert_eq!(fx.store_mode(ok), "copyin");
}

#[tokio::test]
async fn request_validation_returns_400() {
    let _g = serial();
    let fx = make_fx("validate");
    for body in [
        r#"{"ids":[]}"#.to_string(),
        r#"{"ids":[1],"batchId":"   "}"#.to_string(),
        format!(
            r#"{{"ids":[{}]}}"#,
            vec!["1"; svc_library::MIGRATE_MAX_IDS + 1].join(",")
        ),
    ] {
        let (st, j) = post_json(&fx.app, "/api/library/migrate-to-library", &body).await;
        assert_eq!(st, 400, "{body} → {j}");
    }
}

#[tokio::test]
async fn unwritable_library_root_is_rejected_before_any_work() {
    let _g = serial();
    let fx = make_fx("gate");
    let (a, pa, _ha) = fx.add_reference("a.jpg", b"payload");
    // 库根指向一个普通文件 → 实测可写性必然失败（不是目录）
    let bogus = fx.dir.join("not-a-dir");
    std::fs::write(&bogus, b"x").unwrap();
    core_storage::settings::set_json(
        &fx.conn,
        core_config::LIBRARY_SETTINGS_KEY,
        &core_config::LibrarySettings {
            root: Some(bogus.to_string_lossy().to_string()),
        },
    )
    .unwrap();

    let (st, j) = post_json(
        &fx.app,
        "/api/library/migrate-to-library",
        &format!(r#"{{"ids":[{a}]}}"#),
    )
    .await;
    assert_eq!(st, 400, "{j}");
    assert!(j.get("code").is_some(), "必须带稳定分类 code：{j}");
    // 零副作用：资产未变、文件在原处、账本无行、也没启动任务
    assert_eq!(fx.store_mode(a), "reference");
    assert_eq!(PathBuf::from(fx.abs_path(a)), pa);
    assert!(pa.is_file());
    let total: i64 = fx
        .conn
        .query_row("SELECT COUNT(*) FROM migrate_audit", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 0);
    let (st, j) = get_json(&fx.app, "/api/library/migrate-to-library/status").await;
    assert_eq!(st, 200);
    assert_eq!(j["state"], "idle", "被门禁挡下时不该起任务：{j}");
}

#[tokio::test]
async fn concurrent_batch_is_rejected_with_409() {
    let _g = serial();
    let fx = make_fx("busy");
    let (a, _pa, _ha) = fx.add_reference("a.jpg", b"payload");
    // 人为占住槽（模拟「上一批还在跑」），提交必须 409
    assert!(
        app_web::library_api::migrate_slot_start("other-batch", 1).is_none(),
        "首次占用应成功"
    );
    let (st, j) = post_json(
        &fx.app,
        "/api/library/migrate-to-library",
        &format!(r#"{{"ids":[{a}]}}"#),
    )
    .await;
    assert_eq!(st, 409, "{j}");
    assert_eq!(j["batchId"], "other-batch");
    // 放槽后可以正常提交
    app_web::library_api::migrate_slot_reset();
    let (st, j) = post_json(
        &fx.app,
        "/api/library/migrate-to-library",
        &format!(r#"{{"ids":[{a}]}}"#),
    )
    .await;
    assert_eq!(st, 202, "{j}");
    let batch = j["batchId"].as_str().unwrap().to_string();
    assert_eq!(wait_done(&fx.app, &batch).await["migrated"], 1);
}

#[tokio::test]
async fn status_is_idle_for_unknown_or_stale_batch_id() {
    let _g = serial();
    let fx = make_fx("statusidle");
    let (st, j) = get_json(&fx.app, "/api/library/migrate-to-library/status?batchId=nope").await;
    assert_eq!(st, 200);
    assert_eq!(j["state"], "idle");

    let (a, _pa, _ha) = fx.add_reference("a.jpg", b"payload");
    let (_, j) = post_json(
        &fx.app,
        "/api/library/migrate-to-library",
        &format!(r#"{{"ids":[{a}]}}"#),
    )
    .await;
    let batch = j["batchId"].as_str().unwrap().to_string();
    wait_done(&fx.app, &batch).await;
    // 用旧/别的 batchId 轮询不该串台
    let (_, j) = get_json(
        &fx.app,
        "/api/library/migrate-to-library/status?batchId=some-old-batch",
    )
    .await;
    assert_eq!(j["state"], "idle");
    // 不带 batchId = 最近一次任务
    let (_, j) = get_json(&fx.app, "/api/library/migrate-to-library/status").await;
    assert_eq!(j["batchId"], batch.as_str());
    assert_eq!(j["state"], "done");
}

#[tokio::test]
async fn job_failure_is_reported_as_failed_state() {
    let _g = serial();
    let fx = make_fx("jobfail");
    let (a, _pa, _ha) = fx.add_reference("a.jpg", b"payload");
    // 把数据目录搬走 → 后台任务 open(db) 必然失败（路由侧连接仍持有句柄）
    let moved = fx.dir.with_extension("moved");
    let _ = std::fs::remove_dir_all(&moved);
    std::fs::rename(&fx.dir, &moved).unwrap();
    let (st, j) = post_json(
        &fx.app,
        "/api/library/migrate-to-library",
        &format!(r#"{{"ids":[{a}]}}"#),
    )
    .await;
    // 目录没了 → 库根探针也可能直接挡住（400）；只要不是 202 静默成功即可
    assert!(st == 400 || st == 202, "{j}");
    if st == 202 {
        let batch = j["batchId"].as_str().unwrap().to_string();
        let fin = wait_done(&fx.app, &batch).await;
        assert_eq!(fin["state"], "failed", "{fin}");
        assert!(!fin["error"].as_str().unwrap().is_empty());
    }
    std::fs::rename(&moved, &fx.dir).unwrap();
}

#[tokio::test]
async fn audit_endpoint_reports_db_error_as_500() {
    let _g = serial();
    let fx = make_fx("auditerr");
    // 账本表被外部破坏（这里改名，等价于「查不到这张表」）→ 审计 dump 必须诚实地 500，
    // 而不是返回空列表假装没事
    fx.conn
        .execute_batch("ALTER TABLE migrate_audit RENAME TO migrate_audit_gone")
        .unwrap();
    let (st, j) = get_json(&fx.app, "/api/library/migrate-audit").await;
    assert_eq!(st, 500, "{j}");
    assert!(j["error"].as_str().is_some(), "{j}");
}
