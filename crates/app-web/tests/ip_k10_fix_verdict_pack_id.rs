//! IP-K10-FIX 回归钉：判定行**读口径**包 id 必须与写侧同源（引擎感知）。
//!
//! 缺陷现场（`reports/t_77ba4297-pack-ui-e2e.md` §四）：`judge()` 在插件通道把判定行写在
//! **插件包 id**（`com.soujpg.pack.ecommerce`）下，而 `status/queue/verdicts/review*/库视图筛选`
//! 全按**激活配置包 id**（`ecommerce-taobao`）查 —— 结果是「库里有 12 行判定，合规页说未判定」，
//! K09 的「插件通道」来源标记在真实插件运行下永远不出现。
//!
//! 本文件钉四件事（离线：只造 `rule_verdict` 行 + settings，不起子进程、不跑 VL）：
//! 1. 插件引擎下，读接口用插件包 id：verdicts 行数 = 造的行数、state≠unjudged、evidence 带
//!    `engine=plugin`（DOM 来源标记的数据源），status/queue/review/库视图筛选中同一族；
//! 2. 切版本后仍能看到**最新版本**的行（`packVersion` 更新），旧版本行不被遮蔽成「没判过」；
//! 3. 旧「配置包 id」（`ecommerce-taobao`）族的历史行不再混进读口径（迁移 0028 已改键，新写入也不再用它）；
//! 4. 人工改判（POST /review）落在同一族 id 下，改判后 verdicts 的生效结论立刻变。
use axum::body::Body;
use axum::http::Request;
use core_storage::compliance::{ComplianceRepo, NewVerdict};
use rusqlite::Connection;
use std::path::PathBuf;
use tower::ServiceExt;

const PLUGIN_ID: &str = "com.soujpg.pack.ecommerce";
const CONFIG_ID: &str = "ecommerce-taobao";
/// 插件规则的 6 条 id（与官方电商插件 `pack.info.rules` 同口径；这里只用来造行）
const RULES: [&str; 6] = [
    "R0_product_main",
    "R1_size",
    "R2_white_bg",
    "R3_text_ocr",
    "R4_watermark",
    "R5_attribute",
];

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

/// 空库 + 两个资产（1 = 插件通道判过 / 2 = legacy 通道判过）+ 独立 data_dir。
fn seed(tag: &str) -> (PathBuf, Connection, axum::Router) {
    let dir = std::env::temp_dir().join(format!("ipk10fix-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("dam.db");
    let conn = core_storage::open(&db).unwrap();
    for (id, name) in [(1, "plugin.jpg"), (2, "legacy.jpg")] {
        conn.execute(
            "INSERT INTO assets (assetId, absPath, mediaType, missing, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (?1, ?2, 'image', 0, ?3, 'jpg', 10, 0, ?1)",
            rusqlite::params![id, format!("/x/{name}"), name],
        )
        .unwrap();
    }
    let app = app_web::router_without_dist(app_web::test_support::state(
        dir.clone(),
        core_storage::open(&db).unwrap(),
    ));
    (dir, conn, app)
}

/// 造一行判定（插件通道：packId=插件 id + 0027 来源列 + `evidence.engine=plugin`）。
fn plugin_row(
    conn: &Connection,
    asset_id: i64,
    rule: &str,
    result: &str,
    version: &str,
    plugin_hash: &str,
) -> i64 {
    let repo = core_storage::compliance_sqlite::SqliteComplianceRepo::new(conn);
    let short = &plugin_hash[..12.min(plugin_hash.len())];
    let evidence = serde_json::json!({
        "engine": "plugin",
        "pack": format!("{PLUGIN_ID}@{version}#{short}"),
        "note": "IP-K10-FIX 夹具",
    })
    .to_string();
    let id = repo
        .insert_verdict(&NewVerdict {
            pack_id: PLUGIN_ID.into(),
            asset_id,
            job_id: 1,
            rule_id: rule.into(),
            result: result.into(),
            confidence: 0.42,
            score: Some(0.42),
            evidence: Some(evidence),
            fix_action: None,
            fix_tier: None,
            recheck_result: None,
            judgement_scope: Some("per_asset".into()),
            group_key: None,
        })
        .unwrap();
    // 写侧（svc-pipeline）在插件通道逐行补写来源列 —— 读侧可见性不该依赖它，但断言要看它
    repo.set_verdict_pack_source(id, version, plugin_hash).unwrap();
    id
}

/// 造一行 legacy 判定（配置包 id，无来源列）。
fn legacy_row(conn: &Connection, asset_id: i64, rule: &str, result: &str) {
    let repo = core_storage::compliance_sqlite::SqliteComplianceRepo::new(conn);
    repo.insert_verdict(&NewVerdict {
        pack_id: CONFIG_ID.into(),
        asset_id,
        job_id: 1,
        rule_id: rule.into(),
        result: result.into(),
        confidence: 0.9,
        score: Some(0.9),
        evidence: None,
        fix_action: None,
        fix_tier: None,
        recheck_result: None,
        judgement_scope: Some("per_asset".into()),
        group_key: None,
    })
    .unwrap();
}

const HASH_A: &str = "922b063b296d0000000000000000000000000000000000000000000000000000aa";
const HASH_B: &str = "f5fe452a1da1000000000000000000000000000000000000000000000000000000bb";

/// 插件引擎跑完 1 张图 → 合规页全链路（verdicts/status/queue/review/库视图筛选）都看得到那一族行。
///
/// 反向验证：把 `verdict_pack_id` 的插件分支摘掉（退回 `active_pack().id`）→ 本用例必须 RED。
#[tokio::test]
async fn plugin_runs_are_visible_on_every_read_path() {
    let (dir, conn, app) = seed("visible");
        // 1 张图 6 条规则：域闸门 undetermined + 其余按短路口径 skipped（与 K10 真机现场同形）
    for (i, rule) in RULES.iter().enumerate() {
        plugin_row(
            &conn,
            1,
            rule,
            if i == 0 { "undetermined" } else { "skipped" },
            "1.2.0",
            HASH_B,
        );
    }
    // 旧「配置包 id」族的历史行也在库里 —— 不该混进读口径
    legacy_row(&conn, 2, "R1_size", "pass");

    // ---- verdicts（网格/检查器的数据源）
    let (code, v) = get(&app, "/api/compliance/verdicts?assetId=1").await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["packId"], PLUGIN_ID, "verdicts 的读口径必须是插件包 id：{v}");
    let items = v["items"].as_array().expect("items");
    assert_eq!(items.len(), RULES.len(), "6 条规则的行必须都能取到：{v}");
    // DECouple-BE：资产级 state 已下线（资产视图只回判定行原文）；「判过没有」看行数/行结论
    // 等价于旧口径 state=undetermined：没有硬 fail、且至少一行 undetermined（其余 depends_on 短路成 skipped）
    assert!(
        items.iter().all(|i| i["result"] != "fail"),
        "没有硬 fail：{v}"
    );
    assert!(
        items.iter().any(|i| i["result"] == "undetermined"),
        "至少一行 undetermined（判过、但过程出错）：{v}"
    );
    let ev = items[0]["evidence"].as_str().unwrap_or_default();
    assert!(
        ev.contains("\"engine\":\"plugin\""),
        "行级来源证据（DOM「插件通道」标记的数据源）必须原样带出：{ev}"
    );
    for it in items {
        let ev = it["evidence"].as_str().unwrap_or_default();
        assert!(ev.contains(PLUGIN_ID), "每行都该带包身份：{ev}");
        assert!(ev.contains("@1.2.0#"), "包身份带版本：{ev}");
    }

    // ---- status（三档计数）与 verdicts 同口径：不许「verdicts 有行、states 说未跑」
    let (code, st) = get(&app, "/api/compliance/status").await;
    assert_eq!(code, 200, "{st}");
    assert_eq!(st["verdictPackId"], PLUGIN_ID, "{st}");
    assert_eq!(st["states"]["undetermined"], 1, "{st}");
    assert_eq!(st["states"]["judged"], 1, "{st}");
    assert_eq!(st["states"]["unjudged"], 1, "2 张图只有 1 张判过：{st}");
    assert_eq!(st["states"]["total"], 2, "{st}");

    // ---- queue（待人工队列）：undetermined 行必须出现在复核队列里
    let (code, q) = get(&app, "/api/compliance/queue").await;
    assert_eq!(code, 200, "{q}");
    assert_eq!(q["packId"], PLUGIN_ID, "{q}");
    assert!(q["total"].as_i64().unwrap_or(0) >= 1, "复核队列不能空：{q}");
    let qids: Vec<i64> = q["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["assetId"].as_i64().unwrap_or(-1))
        .collect();
    assert!(qids.contains(&1), "队列里应有插件判过的资产 1：{q}");

    // ---- review 列表（扁平行：全库 / 单资产都走同一族 id）
    // 口径注意：非「只看待复核」时列表跳过 skipped 行（它们是短路口径的结果，不是待办），
    // 所以这里期望 = 1 条 undetermined，而不是 6 条。
    let (code, rl) = get(&app, "/api/compliance/review?assetId=1").await;
    assert_eq!(code, 200, "{rl}");
    assert_eq!(rl["packId"], PLUGIN_ID, "{rl}");
    let rl_items = rl["items"].as_array().expect("review items");
    assert_eq!(rl_items.len(), 1, "复核列表要能看到插件族的待复核行：{rl}");
    assert_eq!(rl_items[0]["ruleId"], "R0_product_main", "{rl}");
    assert_eq!(rl_items[0]["machineResult"], "undetermined", "{rl}");

    // ---- 分族口径：图库侧判定维度已下线（DECouple-BE），分桶改由 /status（插件族）守
    let (code, lib) = get(&app, "/api/library/assets?compliance=undetermined").await;
    assert_eq!(code, 400, "图库不读判定: {lib}");
    let (code, st) = get(&app, "/api/compliance/status").await;
    assert_eq!(code, 200, "{st}");
    assert_eq!(st["states"]["undetermined"], 1, "插件判过的那张图：{st}");
    assert_eq!(
        st["states"]["pass"], 0,
        "legacy 族的 pass 行不许混进插件族：{st}"
    );

    // ---- 人工改判落在同一族：改完 verdicts 的生效结论立刻变
    let (code, rv) = post(
        &app,
        "/api/compliance/review",
        r#"{"assetId":1,"ruleId":"R0_product_main","humanStatus":"pass","reason":"主图合规","force":true}"#,
    )
    .await;
    assert_eq!(code, 200, "插件族判定行必须能人工改判：{rv}");
    assert_eq!(rv["packId"], PLUGIN_ID, "{rv}");
    let (_, v2) = get(&app, "/api/compliance/verdicts?assetId=1").await;
    let r0 = v2["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["ruleId"] == "R0_product_main")
        .expect("R0 行");
    assert_eq!(
        r0["effectiveStatus"], "pass",
        "改判后行级生效结论要跟着变（humanStatus ?? result）：{v2}"
    );
    assert_eq!(r0["result"], "undetermined", "机器结论不被改写：{v2}");
    let hist = get(&app, "/api/compliance/review/history?assetId=1").await;
    assert_eq!(hist.0, 200, "{}", hist.1);
    assert_eq!(hist.1["packId"], PLUGIN_ID, "{}", hist.1);
    assert_eq!(
        hist.1["items"].as_array().map(|a| a.len()),
        Some(1),
        "改判历史也在同一族 id 下：{}",
        hist.1
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 切版本后：verdicts 仍能看到**新版本**那批行（同一 (asset,rule) 取最新，`packVersion` 跟着更新）。
#[tokio::test]
async fn version_switch_shows_latest_rows_and_pack_version() {
    let (dir, conn, app) = seed("switch");
        // 旧版本 1.1.0 先把 6 条规则判一遍
    for rule in RULES {
        plugin_row(&conn, 1, rule, "undetermined", "1.1.0", HASH_A);
    }
    // 切到 1.2.0 再判一遍（行追加，不删旧行 —— 历史可复现）
    for rule in RULES {
        plugin_row(&conn, 1, rule, "fail", "1.2.0", HASH_B);
    }

    let (code, v) = get(&app, "/api/compliance/verdicts?assetId=1").await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["packId"], PLUGIN_ID, "{v}");
    let items = v["items"].as_array().expect("items");
    assert_eq!(items.len(), RULES.len(), "每规则只留最新一行：{v}");
    assert!(
        items.iter().all(|i| i["result"] == "fail"),
        "看到的是新版本结论：{v}"
    );
    for it in items {
        let ev = it["evidence"].as_str().unwrap_or_default();
        assert!(
            ev.contains("@1.2.0#"),
            "切版本后必须是新版本的行（旧版本被同 rule 的新行取代）：{ev}"
        );
        assert!(!ev.contains("@1.1.0#"), "旧版本行不该盖住新行：{ev}");
    }
    // 库内来源列也跟着最新行：读口径不依赖它，但报告/审计要能对上
    let latest_version: String = conn
        .query_row(
            "SELECT packVersion FROM rule_verdict WHERE packId = ?1 AND assetId = 1
             ORDER BY id DESC LIMIT 1",
            [PLUGIN_ID],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(latest_version, "1.2.0", "最新判定行的 packVersion 应是新版本");

    let _ = std::fs::remove_dir_all(&dir);
}
