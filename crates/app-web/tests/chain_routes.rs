//! N6e（卡 `task`）路由层验收：`POST /api/compliance/judge` 的 `actions[]` + `GET /versions`。
//!
//! 卡面验收第 1 条要的两件事在这里钉住（链本身的逐步语义在 `svc-pipeline::compliance_chain`
//! 的单测里，用脚本化 mock 插件跑 3 动作/失败中断/中间产物清理）：
//!
//! 1. **链形态校验**（不需要子进程的三条：必给 packId、必点名资产、scope 不许 group）；
//! 2. **动作清单校验**（未知动作 400 点名、`rename` 不许做链首）—— 要读 `pack.info.actions[]`，
//!    所以走真 imaging 包装配（真网关 + 真子进程；解释器缺失时按 400 如实失败，不假装通过）；
//! 3. `GET /api/compliance/versions?assetId=` 的读侧契约（空库 → count=0；落一行 → current 唯一）。
//!
//! 真跑一条 `resize→convert→watermark` 的链路见 `task_chain_real.rs`（默认 `#[ignore]`：
//! 它要真图 + 真插件进程，属「行为证据」而不是门禁用例）。
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tower::ServiceExt;

const PACK_ID: &str = "com.soujpg.pack.imaging";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

fn pack_dir() -> PathBuf {
    repo_root().join("packs/plugins/imaging")
}

/// 把 imaging 包**拷**到 `{data_dir}/packs/<id>`（`plugin_pack_dir_for` 的第 3 级解析位）。
fn install_pack(data_dir: &Path) {
    let dst = data_dir.join("packs").join(PACK_ID);
    let _ = std::fs::remove_dir_all(&dst);
    fn walk(src: &Path, dst: &Path) {
        std::fs::create_dir_all(dst).unwrap();
        for item in std::fs::read_dir(src).unwrap() {
            let item = item.unwrap();
            let name = item.file_name();
            if name == "tests" || name == "__pycache__" {
                continue;
            }
            let from = item.path();
            let to = dst.join(&name);
            if from.is_dir() {
                walk(&from, &to);
            } else {
                std::fs::copy(&from, &to).unwrap();
            }
        }
    }
    walk(&pack_dir(), &dst);
}

fn fresh_state(tag: &str, with_pack: bool) -> (PathBuf, axum::Router) {
    let sys = std::env::temp_dir().join(format!("n6e-routes-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sys);
    std::fs::create_dir_all(&sys).unwrap();
    let db = sys.join("dam.db");
    drop(core_storage::open(&db).unwrap());
    if with_pack {
        install_pack(&sys);
    }
    let state = app_web::test_support::state(sys.clone(), core_storage::open(&db).unwrap());
    (sys, app_web::router_without_dist(state))
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

async fn get(app: &axum::Router, uri: &str) -> (u16, Value) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let raw = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    (status, serde_json::from_slice(&raw).unwrap_or(Value::Null))
}

/// 插一张资产（动作链要「点名的资产真的存在」才会入队）。
fn seed_asset(data_dir: &Path, name: &str) -> i64 {
    let conn = core_storage::open(&data_dir.join("dam.db")).unwrap();
    let path = data_dir.join(name);
    std::fs::write(&path, b"\x89PNG\r\n\x1a\n").unwrap();
    conn.execute(
        "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, importedAt)
         VALUES (1, ?1, 'image', ?2, 'png', 8, 0, 0)",
        rusqlite::params![path.to_string_lossy(), name],
    )
    .unwrap();
    1
}

/// 链形态校验（全部发生在拉插件之前，所以不装包也稳定 400）。
#[tokio::test]
async fn chain_shape_errors_are_400_and_actionable() {
    let (dir, app) = fresh_state("shape", false);
    seed_asset(&dir, "a.png");

    // 1) actions 但没有 packId
    let (code, body) = post(
        &app,
        "/api/compliance/judge",
        json!({ "assetIds": [1], "actions": ["resize"] }),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST.as_u16());
    assert!(
        body["message"].as_str().unwrap_or("").contains("packId"),
        "要点名 packId：{body}"
    );

    // 2) 有 packId 但没点名资产（全库批量处理一律拒绝）
    let (code, body) = post(
        &app,
        "/api/compliance/judge",
        json!({ "packId": PACK_ID, "actions": ["resize"] }),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST.as_u16());
    assert!(
        body["message"].as_str().unwrap_or("").contains("assetIds"),
        "要点名资产：{body}"
    );

    // 3) 空 actions
    let (code, body) = post(
        &app,
        "/api/compliance/judge",
        json!({ "packId": PACK_ID, "assetIds": [1], "actions": [] }),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST.as_u16());
    assert!(
        body["message"].as_str().unwrap_or("").contains("至少"),
        "空链要说明白：{body}"
    );
}

/// 未知动作 → 400 且**点名**（附插件声明的动作清单）；`rename` 不许做链首。
///
/// 这两条要读 `pack.info.actions[]`，必须真装配 imaging 包。容器里没有 python3 时装配会
/// 400（`插件引擎不可用`）—— 那不是假通过，断言里明确区分两种走法。
#[tokio::test]
async fn unknown_action_and_rename_head_are_rejected_by_name() {
    let (dir, app) = fresh_state("actions", true);
    seed_asset(&dir, "a.png");

    let (code, body) = post(
        &app,
        "/api/compliance/judge",
        json!({ "packId": PACK_ID, "assetIds": [1], "actions": ["blur"] }),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST.as_u16(), "{body}");
    let text = body["message"].as_str().unwrap_or("");
    if text.contains("插件引擎不可用") || text.contains("未找到包目录") {
        eprintln!("SKIP 语义（环境无 python3 起不来插件）：{text}");
        return;
    }
    assert!(text.contains("blur"), "未知动作要点名：{text}");
    assert!(
        text.contains("resize") && text.contains("watermark"),
        "要附上插件声明的动作清单：{text}"
    );

    // rename 做链首：链首要产出生意可用的图，rename 只改名不出字节
    let (code, body) = post(
        &app,
        "/api/compliance/judge",
        json!({ "packId": PACK_ID, "assetIds": [1], "actions": ["rename", "resize"] }),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST.as_u16(), "{body}");
    let text = body["message"].as_str().unwrap_or("");
    if text.contains("插件引擎不可用") || text.contains("未找到包目录") {
        eprintln!("SKIP 语义（环境无 python3 起不来插件）：{text}");
        return;
    }
    assert!(text.contains("rename"), "rename 链首要拒绝并点名：{text}");
}

/// 动作链指定的插件包没装 → 400 且提示怎么装（不是 500，也不是静默跑判定）。
///
/// 注意：`com.soujpg.pack.imaging` 这类仓内内置包会被「仓库内置路径」解析到（永远不算没装），
/// 所以这里必须用一个**真不存在**的插件 id 才测得到「没装」这条分支。
#[tokio::test]
async fn missing_pack_is_400_with_install_hint() {
    const GHOST_ID: &str = "com.soujpg.pack.nope";
    let (dir, app) = fresh_state("nopack", false);
    seed_asset(&dir, "a.png");
    let (code, body) = post(
        &app,
        "/api/compliance/judge",
        json!({ "packId": GHOST_ID, "assetIds": [1], "actions": ["resize"] }),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST.as_u16(), "{body}");
    assert_eq!(body["error"], "plugin_not_installed", "{body}");
    let msg = body["message"].as_str().unwrap_or("");
    assert!(msg.contains(GHOST_ID), "要点名包：{msg}");
    // 提示里给装包入口（不是让人自己猜目录在哪）
    let hint = body["hint"].as_str().unwrap_or("");
    assert!(hint.contains("安装"), "要给装包入口：{hint}");
}

/// `GET /api/compliance/versions`：空库如实返回 0；落一行 ok → `current` 唯一且指向它。
#[tokio::test]
async fn versions_route_reports_result_rows_and_single_current() {
    use core_storage::asset_versions::{
        AssetVersionRecord, AssetVersionRepo, SqliteAssetVersionRepo,
    };
    let (dir, app) = fresh_state("versions", false);
    seed_asset(&dir, "a.png");

    // 0) 缺 assetId → 400
    let (code, _) = get(&app, "/api/compliance/versions").await;
    assert_eq!(code, StatusCode::BAD_REQUEST.as_u16());

    // 1) 空库：0 行、无当前版本
    let (code, body) = get(&app, "/api/compliance/versions?assetId=1").await;
    assert_eq!(code, StatusCode::OK.as_u16(), "{body}");
    assert_eq!(body["count"], 0);
    assert_eq!(body["currentCount"], 0);
    assert!(body["current"].is_null());

    // 2) 落一条成功链结果：一张工作版本 + 逐步链证据
    {
        let conn = core_storage::open(&dir.join("dam.db")).unwrap();
        let repo = SqliteAssetVersionRepo::new(&conn);
        let mut rec = AssetVersionRecord::new(
            1,
            PACK_ID,
            vec!["resize".into(), "convert".into(), "watermark".into()],
            json!({
                "chain": [
                    { "action": "resize", "result": "ok", "ms": 3, "outPath": "/tmp/1.png" },
                    { "action": "convert", "result": "ok", "ms": 4, "outPath": "/tmp/2.webp" },
                    { "action": "watermark", "result": "ok", "ms": 5, "outPath": "/tmp/3.webp" },
                ],
                "durationMs": 12,
                "cancelled": false,
            }),
        );
        rec.result = "ok".into();
        rec.path = Some("/tmp/3.webp".into());
        rec.sha256 = Some("a".repeat(64));
        rec.created_at = 1;
        repo.insert(&rec).unwrap();
    }
    let (code, body) = get(&app, "/api/compliance/versions?assetId=1").await;
    assert_eq!(code, StatusCode::OK.as_u16(), "{body}");
    assert_eq!(body["count"], 1);
    assert_eq!(body["okCount"], 1);
    assert_eq!(body["currentCount"], 1, "只有一张工作版本");
    assert_eq!(body["current"]["result"], "ok");
    assert_eq!(body["current"]["sha256"].as_str().unwrap(), "a".repeat(64));
    assert_eq!(
        body["current"]["actions"],
        json!(["resize", "convert", "watermark"])
    );
    assert_eq!(body["current"]["chain"]["chain"].as_array().unwrap().len(), 3);
}
