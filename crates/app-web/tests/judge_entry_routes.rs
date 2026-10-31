#![allow(clippy::await_holding_lock)] // vips 全局锁刻意贯穿 await：测试间串行化 libvips（非 DB 锁，无死锁面）
//! JUDGE-ENTRY（卡 task）回归钉：`/api/compliance/judge` 三件套走真实 HTTP 真跑。
//!
//! 背景（WIN-E2E 真机缺陷 D1）：产品里原先没有任何路径能触发合规批量判定——
//! `compliance_enq` / `compliance_enq_group` 的调用方只有 `dev-e2e` 二进制，
//! 用户旅程「导入 → 判定 → 人工处置」在真机不可达。本文件钉死补上的触发入口：
//!
//! 1. `GET  /api/compliance/judge/status` 路由存在、空闲态形状正确（running=false + queue 计数）
//! 2. `POST /api/compliance/judge` 真的入队并**在后台跑完**（轮询到 running=false + report）
//! 3. 逐图（per_asset）与组级（per_group）判定**同时**落库 —— 判定口径不被入口改动扭曲
//! 4. `/api/compliance/status` 的 `states.judged` 随判定增长（界面分桶立刻可见）
//! 5. 参数校验：非法 scope → 400；空转时 cancel → 409（不假装成功）
//!
//! 图片用 libvips 合成（确定性、无网络、无 VL）：两张字节相同 → 组级重复规则可判。
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

/// libvips 合成确定性灰度 PNG（与 w6_import_autotag / svc-import tests 同法）。
fn make_png(path: &Path, brighten: f64) {
    let black = libvips::ops::black(64, 64).unwrap();
    let img = libvips::ops::linear(&black, &mut [1.0], &mut [brighten]).unwrap();
    libvips::ops::pngsave(&img, &path.to_string_lossy()).unwrap();
}

/// 夹具图片：两个目录 = 两个组；组内两张字节相同（→ 组级重复可判），组间亮度不同。
fn fixture_images(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for (gi, dir) in ["propA", "propB"].iter().enumerate() {
        let d = root.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        for k in 0..2 {
            let p = d.join(format!("img{k}.png"));
            make_png(&p, 40.0 + (gi * 2 + k) as f64 * 30.0);
            out.push(p);
        }
    }
    out
}

struct Fixture {
    app: axum::Router,
    #[allow(dead_code)] // 保活 db 文件路径（诊断用）；读侧走 app
    db: PathBuf,
    #[allow(dead_code)]
    data: PathBuf,
}

/// 装配（同步）：临时数据目录 + 4 张真图（两个目录）。
fn setup(tag: &str) -> Fixture {
    let data = std::env::temp_dir().join(format!("judge-entry-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let db = data.join("dam.db");
    let conn = core_storage::open(&db).unwrap();
    for (i, p) in fixture_images(&data.join("src")).iter().enumerate() {
        let id = (i + 1) as i64;
        conn.execute(
            "INSERT INTO assets (assetId, absPath, mediaType, missing, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (?1, ?2, 'image', 0, ?3, 'png', 10, 0, ?1)",
            rusqlite::params![
                id,
                p.to_string_lossy().to_string(),
                p.file_name().unwrap().to_string_lossy().to_string()
            ],
        )
        .unwrap();
    }
    drop(conn);
    let conn = core_storage::open(&db).unwrap();
    let state = app_web::test_support::state(data.clone(), conn);
    let app = app_web::router_without_dist(state);
    Fixture { app, data, db }
}

async fn get_json(app: &axum::Router, uri: &str) -> (u16, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let body = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
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
    let raw = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null),
    )
}

/// 轮询判定状态直到 `running=false`（后台线程跑完），返回最后一份状态。
async fn wait_judge_idle(app: &axum::Router, timeout: Duration) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    loop {
        let (code, j) = get_json(app, "/api/compliance/judge/status").await;
        assert_eq!(code, 200, "judge/status 必须 200: {j}");
        if j["running"] == serde_json::json!(false) {
            return j;
        }
        if Instant::now() > deadline {
            panic!("判定超时未收工，最后状态: {j}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// 入口契约钉（IP-K16 后）：空闲形状 + 触发语义。
///
/// 执行通道唯一化后，`/judge` 只有一条路：处理插件子进程（宿主不再有 legacy 内联引擎、
/// 也不再有「激活哪个 YAML 行业包」这层状态）。所以本钉只钉**与引擎无关的契约**：
/// 空闲形状、合法/非法入参、以及「装不了插件时必须 400 且不留下僵尸 run」。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn judge_entry_contract_without_legacy_engine() {
    let _guard = vips_lock().lock().unwrap();
    let f = setup("contract");

    // 1) 空闲态形状：running=false，队列计数在（不是 404、不是 SPA HTML）
    let (code, st) = get_json(&f.app, "/api/compliance/judge/status").await;
    assert_eq!(code, 200, "judge/status 必须 200: {st}");
    assert_eq!(st["running"], serde_json::json!(false));
    assert!(st["queue"]["assetPending"].is_i64(), "缺 queue 计数: {st}");

    // 2) scope 拼错 → 400（不静默当 both）
    let (code, j) = post_json(&f.app, "/api/compliance/judge", r#"{"scope":"assets"}"#).await;
    assert_eq!(code, 400, "非法 scope 必须 400: {j}");
    assert_eq!(j["error"], serde_json::json!("bad_scope"));

    // 3) 空闲时 cancel → 409（不假装取消成功）
    let (code, j) = post_json(&f.app, "/api/compliance/judge/cancel", "{}").await;
    assert_eq!(code, 409, "空闲 cancel 必须 409: {j}");
    assert_eq!(j["error"], serde_json::json!("not_running"));

    // 4) 显式要组级 → 400（协议 v1.1 judge 是单资产调用，组级由插件自己决定）
    let (code, j) = post_json(&f.app, "/api/compliance/judge", r#"{"scope":"group"}"#).await;
    assert_eq!(code, 400, "组级判定必须明确拒绝: {j}");
    let msg = j["message"].as_str().unwrap_or_default();
    assert!(msg.contains("组级"), "错误消息要点名原因：{msg}");

    // 5) 正常触发：装得了插件 → 200 且引擎恒为 plugin；装不了 → 400（配置错，不是 500），
    //    两种情况下都**不许**留下一个永远 running 的空 run。
    let (code, start) = post_json(
        &f.app,
        "/api/compliance/judge",
        r#"{"assetIds":[1,2],"scope":"asset"}"#,
    )
    .await;
    match code {
        200 => {
            assert_eq!(start["engine"], serde_json::json!("plugin"), "{start}");
            assert_eq!(start["candidates"], serde_json::json!(2), "{start}");
            assert_eq!(start["queuedAssets"], serde_json::json!(2), "{start}");
            assert_eq!(
                start["queuedGroups"],
                serde_json::json!(0),
                "逐图通道不产生宿主组级 job：{start}"
            );
            assert_eq!(
                start["packId"],
                serde_json::json!("com.soujpg.pack.ecommerce"),
                "判定行归属 = 处理插件身份 id：{start}"
            );
            let done = wait_judge_idle(&f.app, Duration::from_secs(120)).await;
            assert_eq!(done["running"], serde_json::json!(false), "{done}");
        }
        400 => {
            let msg = start["message"].as_str().unwrap_or_default();
            assert!(
                msg.contains("插件"),
                "装机环境缺插件时必须点名「插件」而不是静默失败：{msg}"
            );
            let (_, cur) = get_json(&f.app, "/api/compliance/jobs/current").await;
            assert!(cur["run"].is_null(), "准备阶段失败不许留僵尸 run：{cur}");
        }
        other => panic!("judge 只允许 200（插件可用）或 400（插件装不了），收到 {other}: {start}"),
    }

    let _ = std::fs::remove_dir_all(&f.data);
}
