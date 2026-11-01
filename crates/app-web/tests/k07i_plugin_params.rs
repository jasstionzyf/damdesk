//! K07-I 路由级钉：插件**参数声明**（schema 接口）+ `/judge` 的参数合并校验。
//!
//! 覆盖卡内五条硬要求里能纯路由验证的部分（不跑真实语料、不启插件子进程）：
//! 1. `GET /api/plugins/{id}/schema` 读包内 `params.schema.json`（不启子进程）；
//! 2. 老包（manifest 没声明 `params`）→ `declared:false`，界面给空面板；
//! 3. 包不存在 → 404 `plugin_not_found`（不是空 schema，免得界面以为「这插件没参数」）；
//! 4. `/judge` 参数不合法 → **400 + 稳定错误码，不入队、不建 run 行**（当场看见）；
//! 5. legacy 引擎不消费 options：带了也不报错（协议 v1.1 兼容，不许因为新字段 400）。
use axum::body::Body;
use axum::http::Request;
use std::path::PathBuf;
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
    let sys = std::env::temp_dir().join(format!("k07i-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sys);
    std::fs::create_dir_all(&sys).unwrap();
    let db = sys.join("dam.db");
    drop(core_storage::open(&db).unwrap());
    let state = app_web::test_support::state(sys.clone(), core_storage::open(&db).unwrap());
    let app = app_web::router_without_dist(state);
    (sys, app)
}

/// N18：判定通道恒为处理插件 —— 不再有「设一个引擎档位」这种准备动作。
/// 保留同名空函数只为让调用点读起来仍是「执行前准备」（不再写任何 settings）。
fn set_engine(_data_dir: &std::path::Path) {}

fn run_rows(data_dir: &std::path::Path) -> i64 {
    let conn = core_storage::open(&data_dir.join("dam.db")).unwrap();
    conn.query_row("SELECT COUNT(*) FROM compliance_runs", [], |r| r.get(0))
        .unwrap()
}

const PACK_ID: &str = "com.soujpg.pack.ecommerce";

/// 1 + 2 的前半：官方包声明了参数 → 接口给 schema 原文（界面据此画控件）。
#[tokio::test]
async fn schema_endpoint_returns_declared_schema_for_official_pack() {
    let (dir, app) = fresh_state("schema-ok");
    let (code, st) = get(&app, &format!("/api/plugins/{PACK_ID}/schema")).await;
    if code == 404 {
        // 发行形态可能没把仓库内置包带上：这不是本卡的行为问题，跳过而不是假绿。
        eprintln!("skip: 本机没有仓库内置包 {}（{st}）", PACK_ID);
        return;
    }
    assert_eq!(code, 200, "{st}");
    assert_eq!(st["pluginId"], PACK_ID);
    assert_eq!(st["declared"], true, "官方包必须声明参数 schema：{st}");
    assert_eq!(st["schemaFile"], "resources/params.schema.json");
    assert_eq!(
        st["pluginHash"].as_str().map(str::len),
        Some(64),
        "身份哈希要是 sha256 全文"
    );
    let schema = &st["schema"];
    assert_eq!(schema["schemaVersion"], 1, "{st}");
    let props = schema["properties"].as_object().expect("properties 必须是对象");
    assert!(props.contains_key("shortSideMin"), "{schema}");
    assert_eq!(props["shortSideMin"]["default"], 1600);
    assert_eq!(props["shortSideMin"]["type"], "integer");
    // 分组也要透出来（界面靠它分区 + 高级折叠）
    let groups = schema["groups"].as_array().expect("groups 必须是数组");
    assert!(groups.iter().any(|g| g["key"] == "adv" && g["collapsed"] == true));
    let _ = dir;
}

/// 2：老包（manifest 没声明 params）→ declared:false，不报错。
#[tokio::test]
async fn schema_endpoint_reports_legacy_pack_without_schema() {
    let (dir, app) = fresh_state("schema-legacy");
    let legacy_dir = dir.join("packs").join("com.example.legacy");
    std::fs::create_dir_all(legacy_dir.join("resources")).unwrap();
    std::fs::write(
        legacy_dir.join("manifest.json"),
        serde_json::json!({
            "id": "com.example.legacy",
            "name": "老包（无参数声明）",
            "version": "1.0.0",
            "api_version": "1.1",
            "entrypoint": {"cmd": "python3", "args": ["plugin/__init__.py"]},
            "capabilities": ["io.read"],
            "deterministic": true,
            "timeout_secs": 30,
            "resources": ["resources/rules.json"]
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        legacy_dir.join("resources").join("rules.json"),
        r#"{"rules":[{"id":"R1","name":"尺寸"}]}"#,
    )
    .unwrap();
    let (code, st) = get(&app, "/api/plugins/com.example.legacy/schema").await;
    assert_eq!(code, 200, "{st}");
    assert_eq!(st["declared"], false, "{st}");
    assert!(st["schema"].is_null(), "{st}");
    assert_eq!(st["version"], "1.0.0");
}

/// 3：包不存在 → 404（不是 200 + null，否则界面会把「没装」显示成「无参数」）。
#[tokio::test]
async fn schema_endpoint_404s_for_unknown_plugin() {
    let (dir, app) = fresh_state("schema-404");
    // 发行形态下仓库包可能在（DAM_PACK_DIR 命中）—— 换个肯定不存在的 id 就行
    let (code, st) = get(&app, "/api/plugins/com.example.nope/schema").await;
    assert_eq!(code, 404, "{st}");
    assert_eq!(st["error"], "plugin_not_found", "{st}");
    let _ = dir;
}

/// 3b：解析到的包身份与请求 id 不一致 → 404。
///
/// 现实触发点是 `DAM_PACK_DIR`（四级解析第 1 级不看 id）：不核身份就会把 A 包的表单
/// 当成 B 包的报上去，界面按错误 id 画控件、参数发给错的插件。
/// 这里用「目录名 = 请求 id，但 manifest.id 是另一个」复现同一条守卫（不依赖环境变量）。
#[tokio::test]
async fn schema_endpoint_rejects_pack_whose_identity_does_not_match() {
    let (dir, app) = fresh_state("schema-mismatch");
    let trick_dir = dir.join("packs").join("com.example.asked");
    std::fs::create_dir_all(trick_dir.join("resources")).unwrap();
    std::fs::write(
        trick_dir.join("manifest.json"),
        serde_json::json!({
            "id": "com.example.actual",
            "name": "身份与目录名不一致的包",
            "version": "1.0.0",
            "api_version": "1.1",
            "entrypoint": {"cmd": "python3", "args": ["plugin/__init__.py"]},
            "capabilities": ["io.read"],
            "deterministic": true,
            "timeout_secs": 30,
            "resources": ["resources/rules.json"]
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        trick_dir.join("resources").join("rules.json"),
        r#"{"rules":[{"id":"R1","name":"尺寸"}]}"#,
    )
    .unwrap();
    let (code, st) = get(&app, "/api/plugins/com.example.asked/schema").await;
    assert_eq!(code, 404, "{st}");
    assert_eq!(st["error"], "plugin_not_found", "{st}");
    assert!(
        st["message"].as_str().unwrap().contains("com.example.actual"),
        "错误里要说清解析到的是哪个包：{st}"
    );
}

/// 4：参数不合法 → 400 + 稳定错误码，**不留 run 行**（"参数错了当场看见"，不是跑 500 张才发现）。
#[tokio::test]
async fn judge_rejects_invalid_params_before_enqueue() {
    let (dir, app) = fresh_state("judge-bad-params");
    set_engine(&dir);
    let before = run_rows(&dir);

    // a) 类型错
    let (code, st) = post(
        &app,
        "/api/compliance/judge",
        r#"{"options":{"shortSideMin":"huge"}}"#,
    )
    .await;
    if code == 400 && st["error"] == "pack_manifest" {
        eprintln!("skip: 本机没有仓库内置包（{st}）");
        return;
    }
    assert_eq!(code, 400, "{st}");
    assert_eq!(st["error"], "params_type", "{st}");
    assert!(st["message"].as_str().unwrap().contains("shortSideMin"), "{st}");

    // b) 越界
    let (code, st) = post(
        &app,
        "/api/compliance/judge",
        r#"{"options":{"shortSideMin":99999}}"#,
    )
    .await;
    assert_eq!(code, 400, "{st}");
    assert_eq!(st["error"], "params_range", "{st}");

    // c) 枚举外取值
    let (code, st) = post(
        &app,
        "/api/compliance/judge",
        r#"{"specOptions":{"attribute":"yes"}}"#,
    )
    .await;
    assert_eq!(code, 400, "{st}");
    assert_eq!(st["error"], "params_type", "{st}");

    // d) schema 未声明的键（拼错键名不许静默失效）
    let (code, st) = post(
        &app,
        "/api/compliance/judge",
        r#"{"options":{"shortSidMin":1600}}"#,
    )
    .await;
    assert_eq!(code, 400, "{st}");
    assert_eq!(st["error"], "params_unknown_key", "{st}");

    // e) 层不是对象
    let (code, st) = post(&app, "/api/compliance/judge", r#"{"options":[1,2]}"#).await;
    assert_eq!(code, 400, "{st}");
    assert_eq!(st["error"], "params_layer", "{st}");

    assert_eq!(
        run_rows(&dir),
        before,
        "参数错误不许建 run 行（也不许占运行槽）"
    );
    // 校验失败不该把运行槽卡住：紧接着的错误仍应是参数错，而不是 already_running
    let (code, st) = post(
        &app,
        "/api/compliance/judge",
        r#"{"options":{"whiteMaxDev":-1}}"#,
    )
    .await;
    assert_eq!(code, 400, "{st}");
    assert_ne!(st["error"], "already_running", "参数错误不许占用 running 槽：{st}");
}

/// 5：legacy 引擎不消费 options —— 带了也不 400（协议 v1.1 的 body 兼容）。
#[tokio::test]
async fn unknown_option_key_is_rejected_instead_of_silently_ignored() {
    // N18：没有 legacy 通道了 —— `options` 一律按插件 schema 校验，「配了不读」不再静默放过。
    let (dir, app) = fresh_state("judge-options-unknown");
    let (code, st) = post(
        &app,
        "/api/compliance/judge",
        r#"{"options":{"anything":1},"all":false,"assetIds":[]}"#,
    )
    .await;
    match st["error"].as_str() {
        // 插件包在（仓库内置）→ 未知键当场 400（参数错了先看见，不是跑完才发现）
        Some("params_unknown_key") => assert_eq!(code, 400, "{st}"),
        // 发行形态没带插件 → 「插件没装」，同样是可操作错误，不是 5xx
        Some("plugin_not_installed") => assert_eq!(code, 400, "{st}"),
        other => panic!("未知参数键不许被静默接受（error={other:?} code={code}）：{st}"),
    }
    let _ = dir;
}
