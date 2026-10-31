//! KI-9/KI-10 回归钉：`POST /api/os/reveal` 的 **HTTP 契约**（桌面形态，真路由 + 假执行器）。
//!
//! 形态信号：`DAM_WEB_FORM=desktop`（WIN-PRE2 验收钩子）——本文件所有用例都按桌面形态断言，
//! 容器形态的 `feature_disabled` 单独在 `os_reveal_web_form.rs`（**另一个测试二进制**）里跑，
//! 两个文件各自一个进程，`DAM_WEB_FORM` 才不会互相污染。
//!
//! 执行器用 `os_api::set_runner` 换成记录桩：**绝不真的拉起 explorer/xdg-open**。
//! 真机 Explorer/Finder 行为归链末收尾卡（真机 E2E），本文件守的是「路由 + 白名单 + 错误码 + 参数形状」。
//!
//! 并发注意：同一二进制的用例并行跑，执行器是进程级桩 → **记录按 path 过滤归属**，
//! 绝不在用例末尾 `reset_runner()`（会把别的用例的执行器换回真进程）。
use axum::body::Body;
use axum::http::Request;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};
use tower::ServiceExt;

use app_web::os_api::{self, RevealPlan};

static PLANS: Mutex<Vec<RevealPlan>> = Mutex::new(Vec::new());

fn record(plan: &RevealPlan) -> Result<(), String> {
    PLANS.lock().unwrap().push(plan.clone());
    Ok(())
}

/// 本用例自己那条路径被执行的计划（并行用例互不可见）。
fn plans_for(path: &str) -> Vec<RevealPlan> {
    PLANS
        .lock()
        .unwrap()
        .iter()
        .filter(|p| p.path == path)
        .cloned()
        .collect()
}

fn tmpdir(tag: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!(
        "dam-os-reveal-rt-{}-{}-{}",
        tag,
        std::process::id(),
        n
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 建库：数据目录本身是库根，另登记一个导入批次根 `lib`。
fn seed(data_dir: &Path, lib: &Path) -> Connection {
    std::fs::create_dir_all(lib).unwrap();
    let conn = core_storage::open(&data_dir.join("dam.db")).unwrap();
    conn.execute(
        "INSERT INTO import_batches (rootPath, storeMode, status, totalFiles, doneFiles, startedAt)
         VALUES (?1, 'reference', 'done', 1, 1, 0)",
        rusqlite::params![lib.to_str().unwrap()],
    )
    .unwrap();
    conn
}

fn app(state: app_web::AppState) -> axum::Router {
    app_web::router_without_dist(state)
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

/// 桌面形态信号 + 假执行器（进程内只装一次，避免并行用例互相复位）。
fn desktop_env() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // SAFETY：本测试二进制内所有用例都按桌面形态断言，进程内统一设置无竞争语义。
        std::env::set_var("DAM_WEB_FORM", "desktop");
        os_api::set_runner(record);
    });
}

#[tokio::test]
async fn reveal_inside_library_root_opens_parent_dir() {
    desktop_env();
    let data = tmpdir("hit-data");
    let lib = tmpdir("hit-lib");
    let conn = seed(&data, &lib);
    let file = lib.join("a.png");
    std::fs::write(&file, b"x").unwrap();
    let path = file.to_str().unwrap().to_string();

    let (status, body) = post(
        &app(app_web::test_support::state(data.clone(), conn)),
        "/api/os/reveal",
        json!({ "path": path, "mode": "reveal" }),
    )
    .await;
    assert_eq!(status, 200, "库内路径应放行：{body}");
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["mode"], json!("reveal"));
    assert_eq!(body["label"], json!("在文件夹中显示"));

    let plans = plans_for(&path);
    assert_eq!(plans.len(), 1, "执行器应被调一次：{plans:?}");
    // 本机 = Linux（CI/容器）：reveal 退化为打开所在目录，且命令是 xdg-open
    #[cfg(target_os = "linux")]
    {
        assert_eq!(plans[0].program, "xdg-open");
        assert_eq!(plans[0].args, vec![lib.to_string_lossy().to_string()]);
    }
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&lib);
}

#[tokio::test]
async fn outside_whitelist_is_rejected_even_in_desktop_form() {
    desktop_env();
    let data = tmpdir("deny-data");
    let lib = tmpdir("deny-lib");
    let conn = seed(&data, &lib);
    let a = app(app_web::test_support::state(data.clone(), conn));

    for outside in ["/etc", "/etc/passwd"] {
        let (status, body) = post(&a, "/api/os/reveal", json!({ "path": outside, "mode": "reveal" })).await;
        assert_eq!(status, 400, "库外路径必须拒绝：{outside} → {body}");
        assert_eq!(body["code"], json!("path_not_allowed"));
        assert!(plans_for(outside).is_empty(), "被拒的请求不得触发系统调用");
    }
    // Windows 专有路径在非 Windows 主机上连「绝对路径」都不是 → 同样 400（bad_path）。
    // 两端 400 都是拒绝；具体 code 取决于宿主平台，这里断言的是「不放过」。
    for win in ["C:\\Windows", "C:\\Windows\\System32"] {
        let (status, body) = post(&a, "/api/os/reveal", json!({ "path": win, "mode": "reveal" })).await;
        assert_eq!(status, 400, "Windows 路径在 Linux 主机必须被拒：{win} → {body}");
        #[cfg(target_os = "windows")]
        assert_eq!(body["code"], json!("path_not_allowed"));
        #[cfg(not(target_os = "windows"))]
        assert_eq!(body["code"], json!("bad_path"));
        assert!(plans_for(win).is_empty());
    }
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&lib);
}

#[tokio::test]
async fn missing_path_returns_409_with_human_message() {
    desktop_env();
    let data = tmpdir("gone-data");
    let lib = tmpdir("gone-lib");
    let conn = seed(&data, &lib);
    let gone = lib.join("gone.png");
    let path = gone.to_str().unwrap().to_string();

    let (status, body) = post(
        &app(app_web::test_support::state(data.clone(), conn)),
        "/api/os/reveal",
        json!({ "path": path, "mode": "reveal" }),
    )
    .await;
    assert_eq!(status, 409, "不存在的路径给 409 而不是静默无反应：{body}");
    assert_eq!(body["code"], json!("path_missing"));
    assert!(body["error"].as_str().unwrap().contains("可移动盘"));
    assert!(plans_for(&path).is_empty());
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&lib);
}

#[tokio::test]
async fn bad_mode_and_relative_path_are_400_bad_path() {
    desktop_env();
    let data = tmpdir("bad-data");
    let lib = tmpdir("bad-lib");
    let conn = seed(&data, &lib);
    let a = app(app_web::test_support::state(data.clone(), conn));

    let (status, body) = post(&a, "/api/os/reveal", json!({ "path": lib.join("x.png"), "mode": "explode" })).await;
    assert_eq!(status, 400);
    assert_eq!(body["code"], json!("bad_mode"));

    let (status, body) = post(&a, "/api/os/reveal", json!({ "path": "relative/x.png", "mode": "open" })).await;
    assert_eq!(status, 400);
    assert_eq!(body["code"], json!("bad_path"));

    let (status, body) = post(&a, "/api/os/reveal", json!({ "path": "/data/../etc", "mode": "open" })).await;
    assert_eq!(status, 400);
    assert_eq!(body["code"], json!("bad_path"));

    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&lib);
}

#[tokio::test]
async fn export_dest_from_job_queue_is_openable() {
    desktop_env();
    let data = tmpdir("exp-data");
    let lib = tmpdir("exp-lib");
    let out = tmpdir("exp-out");
    let conn = seed(&data, &lib);
    // 导出留痕（svc-library::export 写 job_queue kind='export' payload.dest）
    conn.execute(
        "INSERT INTO job_queue (kind, targetId, payload, status, createdAt, updatedAt)
         VALUES ('export', 0, ?1, 'done', 0, 0)",
        rusqlite::params![format!("{{\"dest\":\"{}\"}}", out.to_string_lossy())],
    )
    .unwrap();
    let path = out.to_str().unwrap().to_string();

    let (status, body) = post(
        &app(app_web::test_support::state(data.clone(), conn)),
        "/api/os/reveal",
        json!({ "path": path, "mode": "open" }),
    )
    .await;
    assert_eq!(status, 200, "导出目标应可打开（KI-9 的核心诉求）：{body}");
    assert_eq!(body["label"], json!("打开文件夹"));
    let plans = plans_for(&path);
    assert_eq!(plans.len(), 1, "{plans:?}");
    assert_eq!(plans[0].mode, os_api::Mode::Open);
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&lib);
    let _ = std::fs::remove_dir_all(&out);
}
