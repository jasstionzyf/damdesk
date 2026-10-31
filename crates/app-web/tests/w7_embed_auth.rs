//! W7：Tauri 内嵌场景（一次性 token 引导 + 会话 cookie + 鉴权中间件）集成测试。
//!
//! 桌面壳语义：启动时生成 token → 窗口导航 /auth/boot?token=.. → 302 / + HttpOnly
//! cookie → 后续请求全靠 cookie；token 用后即焚（重复引导 403）；/healthz 永远放行。
//! web 容器语义：ServeConfig.token=None → 鉴权关闭，全部放行（R4 v1 不变）。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt; // Router::oneshot

use app_web::{auth, router, AppState};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// 独立临时数据目录（互不串库）。
fn tmp_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "w7-auth-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

fn state_with(data_dir: PathBuf, token: Option<String>) -> AppState {
    std::fs::create_dir_all(&data_dir).expect("mkdir tmp data dir");
    let conn = core_storage::open(&data_dir.join("dam.db")).expect("open test db");
    AppState {
        db: Arc::new(tokio::sync::Mutex::new(conn)),
        thumbs_root: data_dir.join("thumbs"),
        models_root: data_dir.clone(),
        auth: Arc::new(auth::Auth::new(token)),
        data_dir,
        import: Arc::new(app_web::import_api::ImportState::new()),
        embed: Arc::new(app_web::embed_api::EmbedState::new()),
        tag: Arc::new(app_web::tag_api::TagState::new()),
        ocr: Arc::new(app_web::ocr_api::OcrState::new()),
        provider: Arc::new(app_web::infer::ProviderCache::new()),
        export: Arc::new(app_web::export_api::ExportState::new()),
        compliance: Arc::new(app_web::compliance_api::ComplianceRunState::new()),
        verify: Arc::new(app_web::library_api::VerifyState::new()),
        models: Arc::new(app_web::model_api::ModelDownloadState::new()),
        pack_runtime: app_web::PackRuntime::default(),
    }
}

fn router_with(state: AppState) -> axum::Router {
    // dist 传不存在目录 → 走 404 fallback 分支，只测 API/鉴权行为
    router(state, PathBuf::from("/nonexistent-dist-for-tests"))
}

async fn get(app: axum::Router, uri: &str, cookie: Option<&str>) -> axum::response::Response {
    let mut builder = Request::builder().uri(uri);
    if let Some(c) = cookie {
        builder = builder.header("cookie", c);
    }
    app.oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn healthz_always_open_without_auth() {
    let app = router_with(state_with(tmp_dir(), Some(auth::generate_token())));
    let res = get(app, "/healthz", None).await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn api_blocked_without_boot() {
    let app = router_with(state_with(tmp_dir(), Some(auth::generate_token())));
    let res = get(app, "/api/app-info", None).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn boot_wrong_token_forbidden_and_not_burning_valid_token() {
    let token = auth::generate_token();
    let app = router_with(state_with(tmp_dir(), Some(token.clone())));
    let res = get(
        app.clone(),
        &format!("/auth/boot?token={token}x&next=/"),
        None,
    )
    .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    // 错误 token 不得烧掉正确 token
    let res = get(app, &format!("/auth/boot?token={token}&next=/"), None).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn boot_token_is_one_time_then_rejected() {
    let token = auth::generate_token();
    let app = router_with(state_with(tmp_dir(), Some(token)));
    let res = get(app.clone(), "/auth/boot?token=INVALID&next=/", None).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn boot_success_sets_cookie_and_grants_access() {
    let token = auth::generate_token();
    let app = router_with(state_with(tmp_dir(), Some(token.clone())));
    // 引导成功：302 + Set-Cookie
    let res = get(
        app.clone(),
        &format!("/auth/boot?token={token}&next=/"),
        None,
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers().get("location").unwrap(), "/");
    let set_cookie = res
        .headers()
        .get("set-cookie")
        .expect("set-cookie")
        .to_str()
        .unwrap()
        .to_string();
    assert!(set_cookie.starts_with("dam_session="));
    assert!(set_cookie.contains("HttpOnly"));
    let cookie = set_cookie.split(';').next().unwrap().to_string();
    // 会话 cookie 放行 API
    let res = get(app.clone(), "/api/app-info", Some(&cookie)).await;
    assert_eq!(res.status(), StatusCode::OK);
    // token 一次性：同 token 再次引导 403
    let res = get(app, &format!("/auth/boot?token={token}&next=/"), None).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn open_redirect_next_is_sanitized() {
    let token = auth::generate_token();
    let app = router_with(state_with(tmp_dir(), Some(token.clone())));
    // 站外 next 被归一为 "/"
    let res = get(
        app,
        &format!("/auth/boot?token={token}&next=https://evil.example"),
        None,
    )
    .await;
    assert_eq!(res.headers().get("location").unwrap(), "/");
}

#[tokio::test]
async fn web_container_mode_has_auth_disabled() {
    // token=None（web 容器 / dev 反代）：不引导也可访问（R4 v1 语义不变）
    let app = router_with(state_with(tmp_dir(), None));
    let res = get(app, "/api/app-info", None).await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn boot_route_itself_exempt_but_exchange_still_validated() {
    let app = router_with(state_with(tmp_dir(), Some(auth::generate_token())));
    // 无 token 参数的引导 = 交换失败（不是 500/401，而是 403 引导失效）
    let res = get(app, "/auth/boot", None).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}
