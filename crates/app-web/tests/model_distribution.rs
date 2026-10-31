//! 卡 task：模型分发接线 —— 一键下载 + 手动兜底（file:// 导入）+ 默认镜像组。
//!
//! 钉住的契约（不拉真权重；4.8MB realesr 用仓内 staged 资源当 file:// 源）：
//!   1) GET /api/models 聚合：missingCount / missingBytes / optionalBytes + sources/sourceMode；
//!      settings.model_base_url 覆盖后 sourceMode=override 且 sources 收缩为单源；
//!   2) 409（唯一源 = siglip 官方仓）已含两条出路 —— 在 task 文件里钉，这里不重复；
//!   3) file:// 导入（设置页「从文件夹导入」的等价请求）→ 下载 → ready 翻转 +
//!      missingCount/missingBytes 相应下降，outcome.baseUrl = 成功源。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tower::ServiceExt;

use app_web::AppState;

static SEQ: AtomicU64 = AtomicU64::new(0);

fn tmp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "t-a148715b-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn seed_state(tag: &str) -> (AppState, PathBuf) {
    let data_dir = tmp_dir(tag);
    let conn = core_storage::open(&data_dir.join("dam.db")).unwrap();
    (
        app_web::test_support::state(data_dir.clone(), conn),
        data_dir,
    )
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let req = builder
        .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
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

async fn wait_idle(app: &axum::Router) -> Value {
    for _ in 0..300 {
        let (st, body) = call(app, "GET", "/api/models/download/status", None).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        if body["running"] == false {
            return body;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("下载任务 15s 内未收工");
}

/// settings.model_base_url 覆盖 → /api/models 的 sources 收缩为单源、sourceMode=override。
/// （409 的镜像分支在 task 钉过；这里钉 list 视角的回告。）
#[tokio::test]
async fn settings_override_shrinks_sources_to_single() {
    let (state, data_dir) = seed_state("override");
    {
        let conn = state.db.lock().await;
        let mut s = app_web::infer::load_settings(&conn);
        s.model_base_url = "https://mirror.example.com/dam-models/".into();
        app_web::infer::save_settings(&conn, &s).unwrap();
    }
    let app = app_web::router_without_dist(state);
    let (st, body) = call(&app, "GET", "/api/models", None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["sourceMode"], "override");
    assert_eq!(
        body["sources"],
        serde_json::json!(["https://mirror.example.com/dam-models"])
    );
    assert_eq!(body["baseUrl"], "https://mirror.example.com/dam-models");
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 每模型行带许可/来源（设置页「体积/来源/许可/缺失绝对路径」一行的后端口径）。
#[tokio::test]
async fn per_model_rows_carry_license_and_source() {
    let (state, data_dir) = seed_state("meta");
    let app = app_web::router_without_dist(state);
    let (st, body) = call(&app, "GET", "/api/models", None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let models = body["models"].as_array().unwrap();
    let realesr = models
        .iter()
        .find(|m| m["name"] == "realesr-general-x4v3")
        .expect("realesr 条目");
    assert_eq!(realesr["license"], "BSD-3-Clause");
    assert!(realesr["source"].as_str().unwrap().contains("Real-ESRGAN"));
    let det = models
        .iter()
        .find(|m| m["name"] == "ppocrv6-medium-det")
        .unwrap();
    assert_eq!(det["license"], "Apache-2.0");
    assert_eq!(det["totalBytes"], 62_032_837u64);
    // 内置塔与判重塔也如实标注（自备/内置口径在 source 文案里）
    let dinov3 = models.iter().find(|m| m["name"] == "dinov3-vits16-onnx").unwrap();
    assert_eq!(dinov3["downloadable"], false);
    assert!(dinov3["license"].as_str().unwrap().contains("DINOv3"));
    let siglip = models
        .iter()
        .find(|m| m["name"] == "siglip2-base-p224-onnx")
        .unwrap();
    assert_eq!(siglip["downloadable"], false);
    assert!(siglip["source"].as_str().unwrap().contains("内置"));
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// file:// 导入（「从文件夹导入」的等价请求）→ 下载 → ready 翻转 + 聚合下降。
///
/// 源 = 仓内 staged 打包资源（crates/app-tauri/resources/models/realesr-general-x4v3，
/// 4_866_417 B，sha256 与 manifest 锁定一致）；资源不在的机器（稀检出库）跳过，不红。
#[tokio::test]
async fn file_import_downloads_realesr_and_flips_ready() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../app-tauri/resources/models/realesr-general-x4v3");
    if !src.join("realesr-general-x4v3.onnx").is_file() {
        eprintln!("skip: 仓内无 staged realesr 资源（{}），file:// 导入验收在打包机跑", src.display());
        return;
    }
    let (state, data_dir) = seed_state("import");
    let app = app_web::router_without_dist(state);
    let (st, body) = call(
        &app,
        "POST",
        "/api/models/realesr-general-x4v3/download",
        Some(serde_json::json!({ "baseUrl": format!("file://{}", src.display()) })),
    )
    .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["sources"], serde_json::json!([format!("file://{}", src.display())]));

    let status = wait_idle(&app).await;
    assert_eq!(status["status"], "done", "{status}");
    assert_eq!(status["outcome"]["downloaded"], 1);
    // 成功源回告（「当前生效源」的依据）
    assert_eq!(
        status["outcome"]["baseUrl"].as_str(),
        Some(format!("file://{}", src.display()).as_str())
    );

    // 落点 == 引擎读取路径（sigma 边界：models/<name>/ 语义不动）
    let landed = data_dir.join("models/realesr-general-x4v3/realesr-general-x4v3.onnx");
    assert!(landed.is_file(), "{} 不存在", landed.display());
    assert_eq!(std::fs::metadata(&landed).unwrap().len(), 4_866_417);

    // ready 翻转 + 聚合下降：5 → 4 个缺件，missingBytes 减去 realesr 体积
    let (_, list) = call(&app, "GET", "/api/models", None).await;
    let m = list["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "realesr-general-x4v3")
        .unwrap()
        .clone();
    assert_eq!(m["ready"], true, "{m}");
    assert!(m["missingPaths"].as_array().unwrap().is_empty());
    assert_eq!(list["missingCount"], 4);
    assert_eq!(list["missingBytes"], 527_571_637u64 - 4_866_417u64);
    assert_eq!(list["optionalBytes"], 527_571_637u64);
    let _ = std::fs::remove_dir_all(&data_dir);
}
