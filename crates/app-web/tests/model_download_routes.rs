//! 卡 task：模型获取入口路由验收（lama-fp32 从「只能手工拖文件」变成可下载）。
//!
//! 本卡范围裁决（2026-10-20）：功能代码 + 单元/路由测试，不跑真机与 208MB 全链路。
//! 因此这里钉的是**入口契约与诚实失败**，不是「下载 208MB 成功」：
//!   1) GET /api/models 如实报落盘状态（lama 未就绪 + 缺件绝对路径 + 属于可下载档）；
//!   2) 未知模型 404、并发第二次 409（单飞），不静默排第二个下载；
//!   3) 分发源没配（默认源是 siglip 官方仓）→ 409 model_source_unconfigured，不假装在下；
//!   4) 源里没有文件 → 后台任务失败但**如实**进 status（error 指到具体文件）、磁盘不留 .part、
//!      模型仍报未就绪（不会因为「点过下载」就变成可用）。
//!
//! 成功路径（sha256 强校验 + 幂等跳过 + 进度）在 `app_web::model_api::tests` 里用真文件覆盖。

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
        "t6e8888ca-{tag}-{}-{}",
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
    // 无 body 时**不带** content-type：axum 的 `Option<Json<_>>` 只在「没有 JSON 载荷」时给 None
    // （声明了 application/json 却给空串是 400 —— 那不是我们要测的语义）。
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

/// 逐字节比较（分块读，避免把两个 208MB 文件整个塞进内存）。
fn same_bytes(a: &std::path::Path, b: &std::path::Path) -> bool {
    use std::io::Read;
    let (Ok(mut fa), Ok(mut fb)) = (std::fs::File::open(a), std::fs::File::open(b)) else {
        return false;
    };
    let mut ba = vec![0u8; 1 << 20];
    let mut bb = vec![0u8; 1 << 20];
    loop {
        let na = fa.read(&mut ba).unwrap_or(0);
        let nb = fb.read(&mut bb).unwrap_or(0);
        if na != nb {
            return false;
        }
        if na == 0 {
            return true;
        }
        if ba[..na] != bb[..nb] {
            return false;
        }
    }
}

/// 等后台下载收工（最多 ~15s）；返回最终 status 快照。
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

#[tokio::test]
async fn list_reports_lama_missing_with_absolute_paths() {
    let (state, data_dir) = seed_state("list");
    let app = app_web::router_without_dist(state);
    let (st, body) = call(&app, "GET", "/api/models", None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    // 落盘根 = <models_root>/models（引擎读的同一个根，见 model_api::model_root）
    assert_eq!(
        body["root"].as_str().unwrap(),
        data_dir.join("models").display().to_string()
    );
    let models = body["models"].as_array().unwrap();
    let lama = models
        .iter()
        .find(|m| m["name"] == "lama-fp32")
        .expect("lama-fp32 必须在清单里");
    assert_eq!(lama["downloadable"], true);
    assert_eq!(lama["ready"], false, "{lama}");
    assert_eq!(lama["totalBytes"], 208_044_816u64);
    assert_eq!(lama["presentBytes"], 0);
    let missing = lama["missingPaths"].as_array().unwrap();
    assert_eq!(missing.len(), 1);
    assert!(
        missing[0]
            .as_str()
            .unwrap()
            .ends_with("models/lama-fp32/lama-fp32.onnx"),
        "缺件路径必须指到引擎实际读取的文件: {missing:?}"
    );
    // 内置档（随安装包分发）不标可下载
    let siglip = models
        .iter()
        .find(|m| m["name"] == "siglip2-base-p224-onnx")
        .expect("siglip 条目");
    assert_eq!(siglip["downloadable"], false);
    // 下载清单（task）：4 个可选增强 + lama，共 5 项；内置塔与 DINOv3（自备）不进
    assert_eq!(
        body["downloadable"],
        serde_json::json!([
            "u2net",
            "ppocrv6-medium-det",
            "ppocrv6-medium-rec",
            "realesr-general-x4v3",
            "lama-fp32"
        ])
    );
    // 聚合（task）：全新库全部可选模型缺件 —— 个数 5、缺件字节 == 可选总体积
    assert_eq!(body["missingCount"], 5);
    assert_eq!(body["optionalBytes"], 527_571_637u64);
    assert_eq!(body["missingBytes"], 527_571_637u64);
    // 默认分发源 = 内置镜像组（HF 主源 → COS 镜像），sourceMode 如实标 default
    assert_eq!(body["sourceMode"], "default");
    assert_eq!(
        body["sources"],
        serde_json::json!([
            "https://huggingface.co/damdesk/models/resolve/main",
            "https://soujpg-images-1307121509.cos.ap-shanghai.myqcloud.com/damdesk/models"
        ])
    );

    // 状态接口空闲时也要有完整形状（UI 首屏读它就够）
    let (st, status) = call(&app, "GET", "/api/models/download/status", None).await;
    assert_eq!(st, StatusCode::OK, "{status}");
    assert_eq!(status["running"], false);
    assert!(status["model"].is_null());
    assert!(status["error"].is_null());
}

#[tokio::test]
async fn unknown_model_is_404_not_silent_noop() {
    let (state, _d) = seed_state("unknown");
    let app = app_web::router_without_dist(state);
    let (st, body) = call(&app, "POST", "/api/models/nope/download", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "unknown_model");
}

#[tokio::test]
async fn siglip_official_repo_override_is_refused_with_two_actionable_paths() {
    let (state, _d) = seed_state("source");
    let app = app_web::router_without_dist(state);
    // task 后默认源 = 内置镜像组（HF → COS），全新库不再 409；
    // 409 分支留给「唯一生效源 = siglip 官方仓」的显式覆盖（那儿没有可选模型）。
    let (st, body) = call(
        &app,
        "POST",
        "/api/models/lama-fp32/download",
        Some(serde_json::json!({
            "baseUrl": "https://hf-mirror.com/onnx-community/siglip2-base-patch16-224-ONNX/resolve/main"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "model_source_unconfigured");
    let msg = body["error"].as_str().unwrap();
    assert!(msg.contains("lama-fp32"), "{msg}");
    // 两条可执行出路都要点名（不只报「未配置」）
    assert!(msg.contains("model_base_url"), "{msg}");
    assert!(msg.contains("从文件夹导入"), "{msg}");
    // 没有偷偷起任务
    let (_, status) = call(&app, "GET", "/api/models/download/status", None).await;
    assert_eq!(status["running"], false);
}

#[tokio::test]
async fn single_flight_rejects_second_download() {
    let (state, _d) = seed_state("flight");
    let app = app_web::router_without_dist(state.clone());
    let guard = state.models.try_begin("lama-fp32").expect("占住单飞位");
    let (st, body) = call(
        &app,
        "POST",
        "/api/models/lama-fp32/download",
        Some(serde_json::json!({ "baseUrl": format!("file://{}", _d.join("src").display()) })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "download_running");
    assert_eq!(body["running"], true);
    assert_eq!(body["model"], "lama-fp32");
    drop(guard);
    assert!(!state.models.is_running());
}

#[tokio::test]
async fn failed_download_is_reported_and_leaves_nothing_behind() {
    let (state, data_dir) = seed_state("fail");
    let app = app_web::router_without_dist(state);
    let empty_src = tmp_dir("empty-src");
    let (st, body) = call(
        &app,
        "POST",
        "/api/models/lama-fp32/download",
        Some(serde_json::json!({ "baseUrl": format!("file://{}", empty_src.display()) })),
    )
    .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["model"], "lama-fp32");
    assert_eq!(body["totalBytes"], 208_044_816u64);

    let status = wait_idle(&app).await;
    assert_eq!(status["status"], "failed", "{status}");
    assert_eq!(status["model"], "lama-fp32");
    let err = status["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("lama-fp32.onnx"),
        "错误要指到具体文件: {status}"
    );
    assert!(status["finishedAt"].as_i64().unwrap_or(0) > 0);

    // 磁盘：不留正式文件也不留 .part；模型仍报未就绪（点过下载 ≠ 可用）
    let dir = data_dir.join("models").join("lama-fp32");
    let left: Vec<String> = std::fs::read_dir(&dir)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    assert!(left.is_empty(), "失败后目录不该留东西: {left:?}");
    let (_, list) = call(&app, "GET", "/api/models", None).await;
    let lama = list["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "lama-fp32")
        .unwrap()
        .clone();
    assert_eq!(lama["ready"], false);

    // 可重试：再点一次仍然被接受（单飞位已释放）
    let (st, _) = call(
        &app,
        "POST",
        "/api/models/lama-fp32/download",
        Some(serde_json::json!({ "baseUrl": format!("file://{}", empty_src.display()) })),
    )
    .await;
    assert_eq!(st, StatusCode::ACCEPTED);
    let again = wait_idle(&app).await;
    assert_eq!(again["status"], "failed");
}

/// 真模型验收（验收 3 的本地源口径）：`DAM_TEST_LAMA_SRC=<含 lama-fp32.onnx 的目录>` 时跑。
///
/// 走**真实路由 + 真 208MB 权重 + 真 sha256 校验**，然后把结论钉在「引擎读的路径上确有
/// 这件文件、且哈希 == manifest」—— 这就是「下载入口把模型真正喂给 A5 inpaint」的证据。
/// 未设环境变量则跳过（CI/无模型机器不红）。
#[tokio::test]
async fn real_lama_download_from_local_source_is_usable() {
    let Ok(src) = std::env::var("DAM_TEST_LAMA_SRC") else {
        eprintln!("skip: 未设 DAM_TEST_LAMA_SRC（真模型验收需要本地分发目录）");
        return;
    };
    let (state, data_dir) = seed_state("real");
    let app = app_web::router_without_dist(state);
    let (st, body) = call(
        &app,
        "POST",
        "/api/models/lama-fp32/download",
        Some(serde_json::json!({ "baseUrl": format!("file://{src}") })),
    )
    .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    let status = wait_idle(&app).await;
    assert_eq!(status["status"], "done", "{status}");
    assert_eq!(status["doneBytes"], 208_044_816u64);
    assert_eq!(status["outcome"]["downloaded"], 1);

    // 落点 == engine-restore / engine-embed 读的同一路径（`<root>/models/<name>/<file>`），
    // 且与源文件逐字节一致（等价于 manifest 的 sha256 锁定：能落盘就说明校验通过）
    let landed = data_dir.join("models/lama-fp32/lama-fp32.onnx");
    assert!(landed.is_file(), "{} 不存在", landed.display());
    assert_eq!(std::fs::metadata(&landed).unwrap().len(), 208_044_816);
    assert!(same_bytes(
        &std::path::Path::new(&src).join("lama-fp32.onnx"),
        &landed
    ));

    // 就绪状态翻转（UI 据此从「下载」变「已就绪」）
    let (_, list) = call(&app, "GET", "/api/models", None).await;
    let lama = list["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "lama-fp32")
        .unwrap()
        .clone();
    assert_eq!(lama["ready"], true, "{lama}");
    assert!(lama["missingPaths"].as_array().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&data_dir);
}
