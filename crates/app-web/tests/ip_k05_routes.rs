//! 插件唯一化（IP-K16）后的路由级回归钉：插件状态 + 入队语义。
//!
//! 覆盖三件（不跑真实语料，只验接口契约）：
//! 1. `/api/compliance/status` 的 `pluginState`：处理插件身份（能/不能用都要说得清），
//!    且**不再有引擎档位字段**（执行通道唯一化，没有 legacy/plugin 双跑开关）；
//! 2. `/api/compliance/packs`：只剩**处理插件**一族（旧的内置/已导入 YAML 行业包列表已删）；
//! 3. 组级判定被明确拒绝（协议 v1.1 单资产），逐图入队带上 `engine=plugin`/`runId`，
//!    且包身份已登记进 `packs` 账本（migration 0026）。
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
    (status, serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null))
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
    (status, serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null))
}

/// 独立 data_dir（空库：一个资产都没有 —— 本卡不跑语料）。
fn fresh_state(tag: &str) -> (PathBuf, axum::Router) {
    let sys = std::env::temp_dir().join(format!("ipk05-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sys);
    std::fs::create_dir_all(&sys).unwrap();
    let db = sys.join("dam.db");
    let conn = core_storage::open(&db).unwrap();
    drop(conn);
    let state = app_web::test_support::state(sys.clone(), core_storage::open(&db).unwrap());
    let app = app_web::router_without_dist(state);
    (sys, app)
}

/// `pluginState` 必须如实报「处理插件在不在、是什么身份」。
#[tokio::test]
async fn status_reports_plugin_identity() {
    let (dir, app) = fresh_state("status");
    let (code, st) = get(&app, "/api/compliance/status").await;
    assert_eq!(code, 200, "{st}");
    let ps = &st["pluginState"];
    assert!(
        ps["engine"].is_null(),
        "执行通道唯一化后不许再回引擎档位（legacy|plugin 双跑已删）：{st}"
    );
    assert!(
        st["activePack"].is_null() && st["packs"].is_null(),
        "「当前激活行业包 / 包清单」两个旧对象必须消失：{st}"
    );
    // 仓库里带着官方电商包 → 三级解析会命中第 3 级；这里只断言「结构与身份」，不断言可用性
    // （可用性取决于该机器的包目录，缺包时必须是 available=false + reason，而不是静默）。
    assert!(ps["available"].is_boolean(), "{ps}");
    if ps["available"] == true {
        assert_eq!(ps["id"], "com.soujpg.pack.ecommerce");
        assert_eq!(
            ps["pluginHash"].as_str().map(|h| h.len()),
            Some(64),
            "pluginHash 必须是 sha256 全文：{ps}"
        );
        assert_eq!(ps["apiVersion"], "1.1");
        assert_eq!(ps["signatureStatus"], "unsigned", "开发期 manifest 无签名");
    } else {
        assert!(
            ps["reason"] == "plugin_pack_missing" || ps["reason"] == "plugin_manifest_invalid",
            "不可用时必须给原因词：{ps}"
        );
        assert!(ps["note"].is_string(), "不可用时必须给人读解释：{ps}");
    }
    // /packs：只剩处理插件一族（旧的内置 YAML 包 / 已导入包两族已删）
    let (code, list) = get(&app, "/api/compliance/packs").await;
    assert_eq!(code, 200, "{list}");
    assert!(list["plugins"].is_array(), "处理插件清单必须在：{list}");
    for gone in ["builtin", "imported", "activePack", "engine", "plugin"] {
        assert!(
            list[gone].is_null(),
            "旧概念 {gone} 必须消失（一个行业 = 一个插件，没有「行业包规则」独立实体）：{list}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// 插件模式下：组级判定被明确拒绝（协议 v1.1 只有单资产 judge），且不产生 run。
#[tokio::test]
async fn plugin_mode_rejects_group_scope() {
    let (dir, app) = fresh_state("group");
    // 环境里没有 python3 时插件装配会失败 → 那样也必须是 400（配置错，不是 500 静默）
    let (code, res) = post(&app, "/api/compliance/judge", r#"{"scope":"group"}"#).await;
    assert_eq!(code, 400, "插件模式请求组级判定必须 400：{res}");
    let msg = res["message"].as_str().unwrap_or_default().to_string();
    assert!(
        msg.contains("组级") || msg.contains("插件") || msg.contains("plugin"),
        "错误消息要点名原因：{msg}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 插件模式 + 逐图入队：回包带 engine/runId，包身份登记进 `packs` 账本。
#[tokio::test]
async fn plugin_mode_enqueues_assets_and_registers_pack() {
    let (dir, app) = fresh_state("enqueue");
    // 一张资产（不下载任何语料：路径不存在也没关系，本用例只验**入队与登记**，
    // 判定本身由单测/后续 E2E 卡验）
    {
        let conn = core_storage::open(&dir.join("dam.db")).unwrap();
        conn.execute(
            "INSERT INTO assets (assetId, absPath, mediaType, missing, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (1, '/nonexistent/ipk05.png', 'image', 0, 'ipk05.png', 'png', 10, 0, 1)",
            [],
        )
        .unwrap();
    }
    let (code, res) = post(
        &app,
        "/api/compliance/judge",
        r#"{"scope":"both","assetIds":[1],"workers":1,"limit":1}"#,
    )
    .await;
    assert_eq!(code, 200, "插件模式逐图入队应成功：{res}");
    assert_eq!(res["engine"], "plugin", "{res}");
    assert_eq!(res["queuedAssets"], 1, "{res}");
    assert_eq!(res["queuedGroups"], 0, "插件模式不排组级 job：{res}");
    assert_eq!(res["groupSkipped"], true, "scope=both 时组级被跳过要明说：{res}");
    assert!(res["runId"].as_i64().unwrap_or(0) > 0, "{res}");
    // 0026：包身份登记（current=1 且 manifest_hash 是 sha256 全文）
    let conn = core_storage::open(&dir.join("dam.db")).unwrap();
    use core_storage::packs::PackRepo as _;
    let repo = core_storage::packs::SqlitePackRepo::new(&conn);
    let cur = repo
        .current("com.soujpg.pack.ecommerce")
        .unwrap()
        .expect("插件包必须登记进 packs 账本");
    assert_eq!(cur.version.len() > 0, true);
    assert_eq!(cur.manifest_hash.len(), 64, "manifest_hash = sha256 全文");
    assert_eq!(cur.api_version, "1.1");
    assert_eq!(cur.signature_status, "unsigned");
    assert_eq!(cur.current, true);
    let _ = std::fs::remove_dir_all(&dir);
}
