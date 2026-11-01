//! WEBCAP 回归钉：`GET /api/capabilities` 契约 + `POST /api/library/move` 前置拦截（HTTP 真跑）。
//!
//! 覆盖卡内三件事的**路由层**行为（不依赖容器卷；read_only_fs / cross_device 的真值
//! 断言在 `scripts/webcap_acceptance.sh` 的真容器卷上做）：
//! 1. `/api/capabilities` 结构齐全 + 形态口径（本测试二进制 = web 形态 → isDesktop=false）
//! 2. `writableRoots` 来自实测（数据目录 + 导入批次根），不含只读根
//! 3. 前置拦截：`bad_path` / `not_writable_root` → 400 + code；库根内合法移动仍 200（② 不回归）
use axum::body::Body;
use axum::http::Request;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tower::ServiceExt;

fn tmpdir(tag: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("dam-webcap-{}-{}-{}", tag, std::process::id(), n));
    std::fs::create_dir_all(&d).unwrap();
    d
}

const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0, 0, 0,
    1, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78,
    0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xF0, 0x1F, 0, 5, 0, 1, 0xFF, 0x89, 0x99, 0x3D, 0x1D, 0, 0, 0, 0,
    0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

/// 建库：数据目录 = 库根 1（恒在）；两个导入批次根 = lib（可写）+ src（可写）。
/// `lib` 必须注册成批次根，否则它不是「库根」，`writableRoots` 里就没有它（踩过：
/// 只建目录不登记批次 → 断言 lib ∈ writableRoots 失败）。
fn seed(data_dir: &Path, lib: &Path, src: &Path) -> Connection {
    std::fs::create_dir_all(lib).unwrap();
    std::fs::create_dir_all(src).unwrap();
    let conn = core_storage::open(&data_dir.join("dam.db")).unwrap();
    for root in [src, lib] {
        conn.execute(
            "INSERT INTO import_batches (rootPath, storeMode, status, totalFiles, doneFiles, startedAt)
             VALUES (?1, 'reference', 'done', 1, 1, 0)",
            rusqlite::params![root.to_str().unwrap()],
        )
        .unwrap();
    }
    for (i, dir) in [lib, src].iter().enumerate() {
        let path = dir.join(format!("a{i}.png"));
        std::fs::write(&path, PNG).unwrap();
        conn.execute(
            "INSERT INTO assets (mediaType, absPath, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES ('image', ?1, ?2, 'png', ?3, 0, 0)",
            rusqlite::params![path.to_str().unwrap(), format!("a{i}.png"), PNG.len() as i64],
        )
        .unwrap();
    }
    conn
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

#[tokio::test]
async fn capabilities_contract_and_move_precheck() {
    let data_dir = tmpdir("data");
    let lib = data_dir.join("lib");
    let src = data_dir.join("src");
    let conn = seed(&data_dir, &lib, &src);
    let app = app_web::router_without_dist(app_web::test_support::state(data_dir.clone(), conn));

    // ---- 1. 契约 + 形态口径（测试二进制是 web 形态：无 local-infer）----
    let (code, caps) = get(&app, "/api/capabilities").await;
    assert_eq!(code, 200, "{caps}");
    for k in ["isDesktop", "storeMode", "writableRoots", "roots", "features", "featureReasons"] {
        assert!(caps.get(k).is_some(), "缺字段 {k}: {caps}");
    }
    // 形态是**编译期事实**：local-infer 开启 = 桌面壳形态。`cargo test -p app-web` 与 web 容器
    // 构建下关闭（本卡原口径），但 `cargo test --workspace` 会被 app-tauri 的特性统一打开
    // （合流卡 t_d99ca3fe 在 workspace 全量绿灯里踩到）。两形态各断言自己的口径，
    // 不变量（字段齐全 / writableRoots 实测 / 移动前置拦截）两种形态都必须成立。
    assert_eq!(caps["features"]["copyInImport"], json!(true), "复制入库两形态都可用");
    assert!(caps["featureReasons"].get("copyInImport").is_none(), "可用能力不该带 reason");
    if caps["isDesktop"].as_bool().unwrap() {
        assert_eq!(caps["storeMode"], json!("local-disk"), "{caps}");
        assert_eq!(caps["features"]["moveAsset"], json!(true), "桌面形态允许磁盘移动: {caps}");
    } else {
        assert_eq!(caps["storeMode"], json!("server-volume"), "{caps}");
        assert_eq!(caps["features"]["moveAsset"], json!(false), "网页形态不承诺磁盘移动: {caps}");
        let reason = caps["featureReasons"]["moveAsset"].as_str().unwrap_or("");
        assert!(reason.contains("桌面版"), "reason 要指路桌面版：{reason}");
    }

    // ---- 2. writableRoots 来自实测（数据目录 + 批次根），不含只读根 ----
    let writable: Vec<String> = caps["writableRoots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(writable.contains(&data_dir.to_str().unwrap().to_string()), "{writable:?}");
    assert!(writable.contains(&lib.to_str().unwrap().to_string()), "{writable:?}");
    assert!(writable.contains(&src.to_str().unwrap().to_string()), "{writable:?}");
    let roots = caps["roots"].as_array().unwrap();
    assert_eq!(roots.len(), 3, "roots = 数据目录 + 两个批次根: {caps}");
    assert!(roots.iter().all(|r| r["writable"] == json!(true)), "临时目录都该实测可写: {caps}");

    // ---- 3. 前置拦截 ----
    let (code, j) = post(&app, "/api/library/move", json!({"ids":[1],"targetDir":"rel/dir"})).await;
    assert_eq!(code, 400, "{j}");
    assert_eq!(j["code"], json!("bad_path"), "{j}");

    let outside = tmpdir("outside");
    let (code, j) = post(
        &app,
        "/api/library/move",
        json!({"ids":[1],"targetDir":outside.to_str().unwrap()}),
    )
    .await;
    assert_eq!(code, 400, "{j}");
    assert_eq!(j["code"], json!("not_writable_root"), "{j}");
    assert!(j["error"].as_str().unwrap_or("").len() > 10, "要带人话 error: {j}");

    // 库根内合法移动：仍然 200 且逐条回执（② 不回归）
    let target = lib.join("moved");
    let (code, j) = post(
        &app,
        "/api/library/move",
        json!({"ids":[1],"targetDir":target.to_str().unwrap()}),
    )
    .await;
    assert_eq!(code, 200, "{j}");
    assert_eq!(j["okCount"], json!(1), "{j}");
    assert!(target.join("a0.png").is_file(), "文件应真的在新目录: {j}");

    // 清理
    std::fs::remove_dir_all(&data_dir).ok();
    std::fs::remove_dir_all(&outside).ok();
}
