//! 卡 task 回归钉：**装包账本的读写必须落在同一个已提交状态上**。
//!
//! 现场（N4 手验第 1 轮，reports/evidence/task）：`POST /api/compliance/packs/install`
//! 返回 200「已切为当前版本」，但同一进程紧接着 `GET /api/plugins/{id}/schema` 404
//! `plugin_not_found`，而 `/api/compliance/packs` 同一时刻显示包在 —— 因为 install 的
//! `upsert_pack_row` / `set_current` 写的是 `state.db` 连接，schema 却每请求
//! `core_storage::open(db_path)` 另开一条连接，把「装完能不能读到」押在「另一条连接看到的
//! 是不是同一个库」上。库被外部换掉/重建（探针 `rm -rf` 掉了运行中进程持有的 dam.db）时，
//! 两条连接就各说各话。
//!
//! 这里钉两件事：
//! 1. 正常路径：装完同进程连续读 schema 稳定 200 + `declared:true`，且账本行**已提交**
//!    （外部新连接可见、`current=1`），`/api/specs` 与带 `paramsDiff` 的建规格都通；
//! 2. 故障路径：库文件在进程底下被换掉后，schema 仍按**进程连接**的事实回答（与其他端点
//!    同口径），不再凭空 404。
use axum::body::Body;
use axum::http::Request;
use std::path::{Path, PathBuf};
use tower::ServiceExt;

async fn get(app: &axum::Router, uri: &str) -> (u16, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let body = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

async fn post(app: &axum::Router, uri: &str, body: &str) -> (u16, serde_json::Value) {
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
    (
        status,
        serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null),
    )
}

fn fresh_state(tag: &str) -> (PathBuf, axum::Router) {
    let sys = std::env::temp_dir().join(format!("task-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sys);
    std::fs::create_dir_all(&sys).unwrap();
    let db = sys.join("dam.db");
    drop(core_storage::open(&db).unwrap());
    let state = app_web::test_support::state(sys.clone(), core_storage::open(&db).unwrap());
    let app = app_web::router_without_dist(state);
    (sys, app)
}

const PACK_ID: &str = "com.soujpg.pack.ecommerce_ui";

fn manifest(id: &str, version: &str) -> String {
    format!(
        r#"{{
  "id": "{id}",
  "name": "电商主图合规（task 夹具）",
  "version": "{version}",
  "api_version": "1.1",
  "entrypoint": {{"cmd": "python3", "args": ["plugin/__init__.py"]}},
  "capabilities": ["io.read", "op.geometric", "vl.judge", "report.emit"],
  "deterministic": false,
  "timeout_secs": 30,
  "resources": ["resources/rules.json", "resources/params.schema.json"],
  "params": "resources/params.schema.json",
  "permissions": {{"read": true, "net": false}},
  "signature": null
}}"#
    )
}

fn entries(manifest: &str) -> Vec<(String, Vec<u8>)> {
    vec![
        ("manifest.json".to_string(), manifest.as_bytes().to_vec()),
        (
            "plugin/__init__.py".to_string(),
            b"def serve():\n    pass\n".to_vec(),
        ),
        (
            "resources/rules.json".to_string(),
            r#"{"rules":[{"id":"R1","name":"白底"},{"id":"R2","name":"尺寸"}]}"#
                .as_bytes()
                .to_vec(),
        ),
        (
            "resources/params.schema.json".to_string(),
            r#"{
  "schemaVersion": 1,
  "groups": [{"key": "basic", "title": "判定门槛"}],
  "properties": {
    "shortSideMin": {"type": "integer", "title": "短边下限", "min": 200, "max": 8000, "step": 100, "default": 1600, "unit": "px", "group": "basic"},
    "whiteMaxDev": {"type": "integer", "title": "白底允许偏差", "min": 0, "max": 64, "step": 1, "default": 8, "group": "basic"}
  },
  "required": []
}"#
            .as_bytes()
            .to_vec(),
        ),
        ("README.md".to_string(), b"# ui copy\n".to_vec()),
    ]
}

/// 按界面的投递方式（base64 归档上传）装一个声明了参数的包，返回 `archiveBase64` 请求体。
async fn install(app: &axum::Router) -> serde_json::Value {
    let mut buf = std::io::Cursor::new(Vec::new());
    pack_host::write_archive(&mut buf, &entries(&manifest(PACK_ID, "1.3.0"))).unwrap();
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(buf.into_inner());
    let body = format!(
        r#"{{"archiveBase64": "data:application/zip;base64,{b64}", "filename": "ui.dampack"}}"#
    );
    let (code, out) = post(app, "/api/compliance/packs/install", &body).await;
    assert_eq!(code, 200, "装包必须成功：{out}");
    out
}

/// 外部连接看账本（等价「进程退出后另开连接查」）：行必须在，且 `current=1`。
fn ledger_row(data_dir: &Path, id: &str) -> Option<(String, i64)> {
    let conn = core_storage::open(&data_dir.join("dam.db")).unwrap();
    use core_storage::packs::PackRepo as _;
    core_storage::packs::SqlitePackRepo::new(&conn)
        .current(id)
        .unwrap()
        .map(|r| (r.version, i64::from(r.current)))
}

// ---------------------------------------------------------------- 1. 正常路径

#[tokio::test]
async fn install_then_schema_is_stable_and_ledger_committed() {
    let (dir, app) = fresh_state("stable");
    let out = install(&app).await;
    assert_eq!(out["activated"], true, "首装必须自动生效：{out}");

    // 同进程连读 3 次：不闪（现场失败态是第 1 次就 404）
    for i in 1..=3 {
        let (code, st) = get(&app, &format!("/api/plugins/{PACK_ID}/schema")).await;
        assert_eq!(code, 200, "第 {i} 次读 schema 必须 200：{st}");
        assert_eq!(st["pluginId"], PACK_ID, "{st}");
        assert_eq!(st["version"], "1.3.0", "{st}");
        assert_eq!(st["declared"], true, "声明了 params 就必须 declared:true：{st}");
        assert_eq!(st["schemaFile"], "resources/params.schema.json", "{st}");
        let props = st["schema"]["properties"]
            .as_object()
            .expect("properties 必须是对象");
        assert_eq!(props.len(), 2, "{st}");
        assert_eq!(props["shortSideMin"]["default"], 1600, "{st}");
    }

    // 账本行已提交：另开连接（= 进程重启后的口径）必须看得见
    let row = ledger_row(&dir, PACK_ID);
    assert_eq!(
        row,
        Some(("1.3.0".to_string(), 1)),
        "账本行必须是 (1.3.0, current=1) 且对独立连接可见"
    );

    // 规格：列表 + 带 paramsDiff 的创建（现场的下游症状就是这条被 400 挡住）
    let (code, list) = get(&app, &format!("/api/specs?plugin={PACK_ID}")).await;
    assert_eq!(code, 200, "{list}");
    assert_eq!(list["total"], 0, "新装副本起步无规格：{list}");

    let (code, created) = post(
        &app,
        "/api/specs",
        &format!(
            r#"{{"pluginId": "{PACK_ID}", "name": "手验规格", "rules": ["R1"], "paramsDiff": {{"shortSideMin": 1601}}}}"#
        ),
    )
    .await;
    assert_eq!(code, 201, "带 paramsDiff 建规格必须成功：{created}");
    assert!(created["specId"].as_i64().unwrap_or(0) > 0, "{created}");
    assert_eq!(created["paramsDiff"]["shortSideMin"], 1601, "{created}");
    assert_eq!(
        created["pluginStatus"]["paramsSchemaDeclared"], true,
        "回执要能说清「这个包声明了参数」：{created}"
    );

    let (code, list) = get(&app, &format!("/api/specs?plugin={PACK_ID}")).await;
    assert_eq!(code, 200, "{list}");
    assert_eq!(list["total"], 1, "{list}");
    assert_eq!(list["items"][0]["name"], "手验规格", "{list}");
}

// ---------------------------------------------------------------- 2. 故障路径

/// 现场复刻：库文件在进程底下被换掉（探针把运行中进程持有的 dam.db 删掉又重建一个空库）。
///
/// 口径：schema 与 `/api/compliance/packs` 必须**说同一句话**（都按进程连接里的事实），
/// 不许出现「包清单说有、schema 说没有」的撕裂 —— 那次 404 就是这么来的。
#[tokio::test]
async fn schema_follows_process_connection_when_db_file_is_swapped() {
    let (dir, app) = fresh_state("swap");
    install(&app).await;

    // 把库（含 WAL 副产物）挪走，再在原地建一个空库 —— 进程手里的 fd 仍指向旧 inode
    let db = dir.join("dam.db");
    for suffix in ["", "-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{suffix}", db.display()));
        if from.exists() {
            std::fs::rename(&from, PathBuf::from(format!("{}{suffix}.swapped", db.display())))
                .unwrap();
        }
    }
    drop(core_storage::open(&db).unwrap()); // 新库：只有一个空的 packs 表

    let (code, st) = get(&app, &format!("/api/plugins/{PACK_ID}/schema")).await;
    assert_eq!(
        code, 200,
        "schema 读的是进程连接（装包写的同一份），换库文件不该让它 404：{st}"
    );
    assert_eq!(st["declared"], true, "{st}");

    // 同口径对照：包清单也仍然显示这个包在（两条端点不许各说各话）
    let (code, packs) = get(&app, "/api/compliance/packs").await;
    assert_eq!(code, 200, "{packs}");
    let plugins = packs["plugins"].as_array().expect("plugins 数组");
    let me = plugins
        .iter()
        .find(|p| p["id"] == PACK_ID)
        .expect("包必须在清单里");
    assert_eq!(me["currentVersion"], "1.3.0", "{me}");
}
