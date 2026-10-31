#![allow(clippy::await_holding_lock)] // vips 全局锁刻意贯穿 await：测试间串行化 libvips（非 DB 锁，无死锁面）
//! KI-2 / KI-7（卡 task）：「库外文件夹只读查重」端到端（真图片 + 真 HTTP 路由）。
//!
//! 单独一个测试二进制的原因：形态判定读进程级环境变量（`DAM_WEB_FORM=desktop`），
//! 而 Rust 测试是同进程多线程跑，混在一个文件里会互相污染（容器形态的断言在
//! `ki2_dedup_range.rs`，那边不碰 env）。
//!
//! 钉住：
//! 1. `POST /api/dedup/scan` 只读：扫前扫后 `assets` / `resource` / `dedup_vectors` /
//!    `dup_edges` 四张表计数一模一样（**不入库**是硬承诺，不是文案）；
//! 2. 同像素两份（字节相同）→ `exact` 组；条纹图平移 2px → `near` 组（汉明距离小）；
//!    无关噪点图不成组（不误判）；
//! 3. 结果带算法/阈值元数据，且能 `GET /api/dedup/export?scope=scan:<id>` 导出
//!    CSV/Markdown（库外图没有资产记录，导出是唯一的落地方式）；
//! 4. 相对路径 / 不存在的目录 / 空路径 → 明确 400（不静默返回空报告）。

use axum::body::Body;
use axum::http::Request;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tower::ServiceExt;

/// libvips 全局状态不允许多线程并发初始化 → 本文件测试串行化。
fn vips_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// 64×64 灰度图：四种象限灰块（低频谱与「半黑半白」类图案拉开距离）。
///
/// 图案选型依据见 `examples/ki2_scan_probe.rs`（走真实解码路径打印两两汉明距离）：
/// pHash 对「只差亮度/相位」的规则条纹不敏感，用它当 fixture 会让无关图也成组。
fn quad(x: usize, y: usize) -> u8 {
    match (x < 32, y < 32) {
        (true, true) => 235,
        (false, true) => 20,
        (true, false) => 20,
        (false, false) => 180,
    }
}

/// 整半色块（上白下黑）
fn half_vertical(_x: usize, y: usize) -> u8 {
    if y < 32 {
        235
    } else {
        20
    }
}

/// 整半色块（左白右黑）—— 与「上白下黑」在 pHash 意义下同向（汉明 0），但像素不同、
/// fileHash 不同 → 正好用来造「近似重复但不是同一文件」的组。
fn half_horizontal(x: usize, _y: usize) -> u8 {
    if x < 32 {
        235
    } else {
        20
    }
}

/// 8×8 块状噪点（中频结构）：与上面两类低频图案的汉明距离 24~29 → 不该成组。
fn blocknoise(x: usize, y: usize) -> u8 {
    let bx = (x / 8) as u64;
    let by = (y / 8) as u64;
    let mut s = 0x9e37_79b9u64 ^ (bx << 8) ^ (by << 24) ^ (bx * 31 + by * 17);
    s ^= s << 13;
    s ^= s >> 7;
    s ^= s << 17;
    (s & 0xff) as u8
}

fn render(f: fn(usize, usize) -> u8) -> Vec<u8> {
    let mut d = vec![0u8; 64 * 64];
    for y in 0..64 {
        for x in 0..64 {
            d[y * 64 + x] = f(x, y);
        }
    }
    d
}

fn write_png(path: &Path, data: &[u8]) {
    let img = image::GrayImage::from_raw(64, 64, data.to_vec()).expect("64x64 灰度缓冲");
    img.save(path).expect("png 编码失败");
}

/// 目录内容：
/// - `a1.png` / `a2.png`：块状噪点，**同一份字节**（复制文件）→ exact 组；
/// - `b1.png` / `b2.png`：上白下黑 / 左白右黑（像素不同、fileHash 不同，pHash 同向）
///   → near 组（「近似但不是同一文件」）；
/// - `z1.png`：象限灰块 —— 与上面两组距离 10/24 → 单张，不许成组。
fn fixture(root: &Path) -> Vec<PathBuf> {
    std::fs::create_dir_all(root).unwrap();
    let a = root.join("a1.png");
    write_png(&a, &render(blocknoise));
    std::fs::copy(&a, root.join("a2.png")).unwrap();
    write_png(&root.join("b1.png"), &render(half_vertical));
    write_png(&root.join("b2.png"), &render(half_horizontal));
    write_png(&root.join("z1.png"), &render(quad));
    vec![a]
}

fn state(conn: rusqlite::Connection, data_dir: PathBuf) -> app_web::AppState {
    app_web::test_support::state(data_dir, conn)
}

async fn post_json(app: axum::Router, uri: &str, body: &str) -> (u16, serde_json::Value) {
    let res = app
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
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn get_raw(app: axum::Router, uri: &str) -> (u16, String, String) {
    let res = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let ctype = res
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap_or("").to_string())
        .unwrap_or_default();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    (status, ctype, String::from_utf8_lossy(&bytes).to_string())
}

#[tokio::test]
async fn scan_external_folder_is_read_only_and_finds_groups() {
    let _guard = vips_lock().lock().unwrap();
    // 桌面形态（Tauri 壳同款判定：env 验收钩子）
    std::env::set_var("DAM_WEB_FORM", "desktop");

    let data_dir = std::env::temp_dir().join(format!("ki2-scan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    std::fs::create_dir_all(&data_dir).unwrap();
    let db_path = data_dir.join("dam.db");
    let conn = core_storage::open(&db_path).unwrap();
    let fixture_dir = data_dir.join("external");
    fixture(&fixture_dir);

    // 库里先放一张图（证明「扫描不进库」而不是「库本来就是空的」）
    conn.execute(
        "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, fileHash, importedAt)
         VALUES (1, '/lib/x.jpg', 0, 'x.jpg', 'jpg', 10, 1, 'aaaa', 1000)",
        [],
    )
    .unwrap();
    let before = app_web::dedup_api::table_counts(&conn);
    for (k, v) in &before {
        // 表存在且非空（dup_edges 可能是 0：只断言差值，不断言初值）
        assert!(*v >= 0, "{k} 计数读不到: {before:?}");
    }

    let app = app_web::router_without_dist(state(conn, data_dir.clone()));
    let body = serde_json::json!({ "path": fixture_dir.to_string_lossy() }).to_string();
    let (st, j) = post_json(app.clone(), "/api/dedup/scan", &body).await;
    assert_eq!(st, 200, "库外扫描失败: {j}");
    let rep = &j["report"];
    assert_eq!(rep["scanned"], 5, "应扫到 5 个 png: {rep}");
    assert_eq!(rep["truncated"], false);
    assert_eq!(
        rep["unreadable"].as_array().map(|a| a.len()).unwrap_or(9),
        0,
        "这 5 张都应能解码: {rep}"
    );
    let groups = rep["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "应正好两组（同字节 + 半色块近似）: {rep}");

    // exact 组：a1/a2（同字节）
    let exact = groups
        .iter()
        .find(|g| g["kind"] == "exact")
        .unwrap_or_else(|| panic!("缺 exact 组: {rep}"));
    let names: Vec<String> = exact["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["fileName"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names.len(), 2, "exact 组两个成员: {exact}");
    assert!(names.iter().any(|n| n == "a1.png") && names.iter().any(|n| n == "a2.png"));

    // near 组：b1/b2（条纹平移 2px，pHash 汉明小），且证据里带汉明距离
    let near = groups
        .iter()
        .find(|g| g["kind"] == "near")
        .unwrap_or_else(|| panic!("缺 near 组: {rep}"));
    let near_names: Vec<String> = near["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["fileName"].as_str().unwrap().to_string())
        .collect();
    assert!(
        near_names.iter().any(|n| n == "b1.png") && near_names.iter().any(|n| n == "b2.png"),
        "半色块（像素不同、pHash 同向）应判为近似重复: {near}"
    );
    assert!(
        near["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e.as_str().unwrap_or("").contains("汉明距离")),
        "证据要写汉明距离: {near}"
    );
    // 象限图（与两组距离都 >8）不许混进任何组
    let all_names: Vec<String> = groups
        .iter()
        .flat_map(|g| g["members"].as_array().unwrap().iter())
        .map(|m| m["fileName"].as_str().unwrap().to_string())
        .collect();
    assert!(
        !all_names.iter().any(|n| n.starts_with('z')),
        "无关象限图不该成组: {all_names:?}"
    );

    // 算法/阈值元数据进报告（模型名只在这里，不进 UI）
    assert!(
        rep["algorithm"].as_str().unwrap().contains("SHA-256"),
        "{rep}"
    );
    assert!(rep["thresholds"].as_str().unwrap().contains("8"), "{rep}");
    let scan_id = rep["scanId"].as_str().unwrap().to_string();

    // 导出（库外结果落地的唯一方式）
    let (st, ctype, csv) = get_raw(
        app.clone(),
        &format!("/api/dedup/export?format=csv&scope=scan:{scan_id}"),
    )
    .await;
    assert_eq!(st, 200, "scan 导出失败: {csv}");
    assert!(ctype.starts_with("text/csv"), "{ctype}");
    assert!(csv.contains("groupNo,kind,size,keep"), "缺表头: {csv}");
    assert!(
        csv.contains("a1.png") && csv.contains("b2.png"),
        "缺成员行: {csv}"
    );
    assert!(csv.contains("# path="), "缺目录元数据: {csv}");

    let (st, _ctype, md) = get_raw(
        app.clone(),
        &format!("/api/dedup/export?format=md&scope=scan:{scan_id}"),
    )
    .await;
    assert_eq!(st, 200);
    assert!(md.contains("库外文件夹查重报告"), "{md}");

    // 只读硬承诺：四类表计数一张都没变（assets 不新增、也不写向量/边）
    {
        let conn2 = core_storage::open(&db_path).unwrap();
        let after = app_web::dedup_api::table_counts(&conn2);
        assert_eq!(
            before, after,
            "库外扫描不许改库: before={before:?} after={after:?}"
        );
    }

    // 参数校验：相对路径 / 不存在目录 / 空路径
    let (st, j) = post_json(app.clone(), "/api/dedup/scan", r#"{"path":"relative/dir"}"#).await;
    assert_eq!(st, 400, "相对路径必须拒绝: {j}");
    let (st, j) = post_json(
        app.clone(),
        "/api/dedup/scan",
        r#"{"path":"/definitely/not/here/xyz"}"#,
    )
    .await;
    assert_eq!(st, 400, "不存在的目录必须拒绝: {j}");
    let (st, _) = post_json(app.clone(), "/api/dedup/scan", r#"{"path":"  "}"#).await;
    assert_eq!(st, 400, "空路径必须拒绝");

    std::env::remove_var("DAM_WEB_FORM");
}
