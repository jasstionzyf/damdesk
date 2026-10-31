//! KI-9/KI-10 回归钉：**形态门控**在 HTTP 层的表现。
//!
//! 单独一个测试二进制：`DAM_WEB_FORM` 是进程级环境变量，跟桌面形态的用例放同一进程会互相污染
//! （`os_reveal_routes.rs` 全按桌面形态断言）。
//!
//! **形态是编译期事实**，不是跑测试的人能选的：
//! - `cargo test -p app-web`（本卡自测）：无 `local-infer` → `IS_DESKTOP_FORM=false` → 容器形态；
//! - `cargo test --workspace`（L0 全量）：`app-tauri` 把 `app-web/local-infer` 特性统一打开
//!   → `IS_DESKTOP_FORM=true` → 桌面形态（合流卡 task 已踩过，`webcap_capabilities.rs` 同处理）。
//!
//! 所以这里不硬编码期望值，而是**以 `/api/capabilities` 为唯一事实源**断言一致性：
//! `features.openInOS=false` → `POST /api/os/reveal` 必须 400 `feature_disabled` 且**零系统调用**；
//! `=true` → 必须 200 并真的走到执行器（不许偷偷门控掉）。
//! 「容器形态不给系统调用」这条硬要求由 `os_api::tests::decide_gates_form_and_existence`
//! （`is_desktop=false` 直接注入）独立守死，不受特性统一影响。
use axum::body::Body;
use axum::http::Request;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;
use tower::ServiceExt;

use app_web::os_api::{self, RevealPlan};

static PLANS: Mutex<Vec<RevealPlan>> = Mutex::new(Vec::new());

fn record(plan: &RevealPlan) -> Result<(), String> {
    PLANS.lock().unwrap().push(plan.clone());
    Ok(())
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dam-os-reveal-web-{}-{}", tag, std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn post(app: &axum::Router, body: Value) -> (u16, Value) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/os/reveal")
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

async fn caps(app: &axum::Router) -> Value {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/capabilities")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let raw = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    serde_json::from_slice(&raw).unwrap()
}

#[tokio::test]
async fn reveal_gate_matches_capabilities_open_in_os() {
    std::env::remove_var("DAM_WEB_FORM");
    os_api::set_runner(record);

    let data = tmpdir("data");
    let conn = core_storage::open(&data.join("dam.db")).unwrap();
    let file = data.join("in-root.png");
    std::fs::write(&file, b"x").unwrap();
    let path = file.to_str().unwrap().to_string();

    let a = app_web::router_without_dist(app_web::test_support::state(data.clone(), conn));
    let caps = caps(&a).await;
    let can_open = caps["features"]["openInOS"].as_bool().unwrap();

    let (status, body) = post(&a, json!({ "path": path, "mode": "reveal" })).await;
    if can_open {
        // 桌面形态（workspace 构建特性统一）：库内路径必须真的走到执行器
        assert_eq!(status, 200, "能力位为真时不该被门控：{body}");
        assert_eq!(body["ok"], json!(true));
        let plans = PLANS.lock().unwrap().iter().filter(|p| p.path == path).count();
        assert_eq!(plans, 1, "执行器应被调一次");
    } else {
        // 容器形态：400 feature_disabled，人话同 featureReasons.openInOS，且零系统调用
        assert_eq!(status, 400, "容器形态必须拒绝：{body}");
        assert_eq!(body["code"], json!("feature_disabled"));
        let reason = caps["featureReasons"]["openInOS"].as_str().unwrap();
        assert!(reason.contains("系统文件管理器"), "特征原因口径：{reason}");
        assert_eq!(body["error"], json!(reason), "接口文案与能力位原因必须同一句");
        assert!(
            PLANS.lock().unwrap().iter().all(|p| p.path != path),
            "容器形态不得尝试系统调用"
        );
    }

    os_api::reset_runner();
    let _ = std::fs::remove_dir_all(&data);
}
