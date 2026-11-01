//! N6e（卡 `t_b6c2aa1e`）**行为证据**：真插件 + 真网关 + 真图，一条
//! `resize → convert → watermark` 链跑通，并且只落**一张**工作版本。
//!
//! 为什么 `#[ignore]`：它要起 python 子进程 + 能力网关（秒级），还依赖仓库里的真图。
//! 门禁（`scripts/test_gate.sh --skip-heavy`）不该被这类用例拖慢；行为证据按需显式跑：
//!
//! ```bash
//! cargo test -p app-web --test t_b6c2aa1e_chain_real -- --ignored --nocapture
//! ```
//!
//! 断言（= 卡面验收第 2 条）：
//! 1. `POST /api/compliance/judge` 带 `actions[]` 返回 200（原始请求/返回打印出来贴报告）；
//! 2. 链尾产物存在、`sha256` 与磁盘文件一致（`sha256sum` 对得上）；
//! 3. `asset_versions` 里该资产**只有一行 current=1**，`actions` = 请求的三步；
//! 4. 中间产物（第 1、2 步的产物）不在磁盘上（用完即删）；
//! 5. `evidence.chain` 三步齐全、`outPath` 逐级串起来（后一步吃前一步的产物）。
use axum::body::Body;
use axum::http::Request;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tower::ServiceExt;

const PACK_ID: &str = "com.soujpg.pack.imaging";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

fn install_pack(data_dir: &Path) {
    let src = repo_root().join("packs/plugins/imaging");
    let dst = data_dir.join("packs").join(PACK_ID);
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
    let _ = std::fs::remove_dir_all(&dst);
    walk(&src, &dst);
}

fn sha256_of(path: &Path) -> String {
    core_hash::sha256_hex(&std::fs::read(path).expect("读产物"))
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

#[tokio::test]
#[ignore = "真插件子进程 + 真图；行为证据按需显式跑（--ignored）"]
async fn real_chain_resize_convert_watermark_lands_one_working_version() {
    let sys = std::env::temp_dir().join(format!("n6e-real-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sys);
    std::fs::create_dir_all(&sys).unwrap();
    let db = sys.join("dam.db");
    drop(core_storage::open(&db).unwrap());
    install_pack(&sys);

    // 真图（675×900，落在 assets 里指向**原图**，链的产物一律进 fixes/）
    let src = repo_root().join("packs/plugins/imaging/tests/fixtures/codec/orient/orient_6.jpg");
    let original = sys.join("orient_6.jpg");
    std::fs::copy(&src, &original).unwrap();
    let original_sha = sha256_of(&original);
    {
        let conn = core_storage::open(&db).unwrap();
        conn.execute(
            "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (1, ?1, 'image', 'orient_6.jpg', 'jpg', ?2, 0, 0)",
            rusqlite::params![original.to_string_lossy(), std::fs::metadata(&original).unwrap().len() as i64],
        )
        .unwrap();
    }

    let state = app_web::test_support::state(sys.clone(), core_storage::open(&db).unwrap());
    let app = app_web::router_without_dist(state);

    let request = json!({
        "packId": PACK_ID,
        "assetIds": [1],
        "scope": "asset",
        "workers": 1,
        "actions": ["resize", "convert", "watermark"],
        "options": {
            "resizeMode": "width",
            "resizeWidth": 400,
            "convertFormat": "webp",
            "watermarkText": "N6E-KANBAN",
            "watermarkAnchor": "bottom_right",
            "watermarkOpacity": 0.6
        }
    });
    println!("POST /api/compliance/judge\n{}", serde_json::to_string_pretty(&request).unwrap());
    let (code, body) = post(&app, "/api/compliance/judge", request).await;
    println!("{} {}", code, serde_json::to_string_pretty(&body).unwrap());
    assert_eq!(code, 200, "触发链失败：{body}");
    assert_eq!(body["actions"], json!(["resize", "convert", "watermark"]));
    let run_id = body["runId"].as_i64().expect("runId");

    // 等后台把这一条 job 跑完：running=false 且 job 不再 pending/running
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut last = Value::Null;
    loop {
        assert!(Instant::now() < deadline, "链跑超时，最后状态：{last}");
        tokio::time::sleep(Duration::from_millis(250)).await;
        let (_c, st) = get(&app, "/api/compliance/judge/status").await;
        last = st.clone();
        let running = st["running"].as_bool().unwrap_or(false);
        if !running {
            break;
        }
    }
    println!("judge/status 收工快照\n{}", serde_json::to_string_pretty(&last).unwrap());

    // 结果行 + 工作版本（读侧接口）
    let (code, versions) = get(&app, "/api/compliance/versions?assetId=1").await;
    println!("GET /api/compliance/versions?assetId=1 → {}\n{}", code, serde_json::to_string_pretty(&versions).unwrap());
    assert_eq!(code, 200, "{versions}");
    assert_eq!(versions["count"], 1, "一条链 = 一行结果：{versions}");
    assert_eq!(versions["okCount"], 1, "链应成功：{versions}");
    assert_eq!(versions["currentCount"], 1, "**只有一张**工作版本：{versions}");
    let cur = &versions["current"];
    assert_eq!(cur["result"], "ok", "{versions}");
    assert_eq!(cur["actions"], json!(["resize", "convert", "watermark"]));

    // 产物：存在、格式 webp、sha256 与磁盘一致
    let out_path = PathBuf::from(cur["path"].as_str().expect("产物路径"));
    assert!(out_path.is_file(), "产物不在磁盘：{}", out_path.display());
    let disk_sha = sha256_of(&out_path);
    assert_eq!(
        cur["sha256"].as_str().unwrap(),
        disk_sha,
        "落库 sha256 与磁盘文件不一致"
    );
    assert_eq!(cur["format"], "webp", "{versions}");
    let original_now = sha256_of(&original);
    assert_eq!(original_sha, original_now, "原图不许被改（产物只进 fixes/）");
    println!("产物 {} sha256={}", out_path.display(), disk_sha);

    // 链证据：三步、outPath 逐级串起来、中间产物已删
    let chain = cur["chain"]["chain"].as_array().expect("evidence.chain");
    assert_eq!(chain.len(), 3, "{versions}");
    assert_eq!(chain[0]["action"], "resize");
    assert_eq!(chain[1]["action"], "convert");
    assert_eq!(chain[2]["action"], "watermark");
    for step in chain {
        assert_eq!(step["result"], "ok", "每一步都应 ok：{step}");
    }
    let p1 = PathBuf::from(chain[0]["outPath"].as_str().unwrap());
    let p2 = PathBuf::from(chain[1]["outPath"].as_str().unwrap());
    assert!(!p1.exists(), "第 1 步中间产物应已删除：{}", p1.display());
    assert!(!p2.exists(), "第 2 步中间产物应已删除：{}", p2.display());
    assert_eq!(chain[2]["outPath"].as_str().unwrap(), out_path.to_string_lossy());

    // 库表直查（验收原文的口径）：SELECT * FROM asset_versions WHERE assetId=1
    let conn = core_storage::open(&db).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM asset_versions WHERE assetId = 1", [], |r| r.get(0))
        .unwrap();
    let cur_n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM asset_versions WHERE assetId = 1 AND current = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "只有一行结果");
    assert_eq!(cur_n, 1, "只有一张工作版本");
    let (vid, vpath, vsha, vrun): (i64, String, String, Option<i64>) = conn
        .query_row(
            "SELECT versionId, path, sha256, runId FROM asset_versions WHERE assetId = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    println!(
        "SELECT * FROM asset_versions WHERE assetId=1 → versionId={vid} runId={vrun:?} path={vpath} sha256={vsha}"
    );
    assert_eq!(vrun, Some(run_id), "结果行必须挂在本次 run 上");
    assert_eq!(vsha, disk_sha);

    // 再跑一次同一条链：仍只有一张 current（新版本顶替旧版本，历史行留着）
    let (code, body) = post(
        &app,
        "/api/compliance/judge",
        json!({
            "packId": PACK_ID,
            "assetIds": [1],
            "scope": "asset",
            "workers": 1,
            "actions": ["resize", "convert", "watermark"],
            "options": {
                "resizeMode": "width", "resizeWidth": 320,
                "convertFormat": "webp", "watermarkText": "N6E-KANBAN",
            }
        }),
    )
    .await;
    assert_eq!(code, 200, "重跑触发失败：{body}");
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        assert!(Instant::now() < deadline, "重跑超时");
        tokio::time::sleep(Duration::from_millis(250)).await;
        let (_c, st) = get(&app, "/api/compliance/judge/status").await;
        if !st["running"].as_bool().unwrap_or(false) {
            break;
        }
    }
    let (_c, versions2) = get(&app, "/api/compliance/versions?assetId=1").await;
    println!(
        "重跑后 versions\n{}",
        serde_json::to_string_pretty(&versions2).unwrap()
    );
    assert_eq!(versions2["count"], 2, "历史行留着（可复盘）：{versions2}");
    assert_eq!(versions2["currentCount"], 1, "同一时刻只有一张工作版本");
    // 历史版本必须仍然「sha256 对得上磁盘」：重跑不许覆盖上一版的产物文件
    for v in versions2["versions"].as_array().unwrap() {
        let p = PathBuf::from(v["path"].as_str().expect("每行都有产物路径"));
        assert!(p.is_file(), "历史产物被覆盖/删掉了：{}", p.display());
        assert_eq!(
            v["sha256"].as_str().unwrap(),
            sha256_of(&p),
            "行内 sha256 与磁盘不一致（历史版本已失真）：{}",
            p.display()
        );
    }
    let p_first = PathBuf::from(versions2["versions"][1]["path"].as_str().unwrap());
    let p_second = PathBuf::from(versions2["versions"][0]["path"].as_str().unwrap());
    assert_ne!(
        p_first, p_second,
        "两次跑必须落在不同目录（否则互相覆盖）"
    );
    let _ = std::fs::remove_dir_all(&sys);
}
