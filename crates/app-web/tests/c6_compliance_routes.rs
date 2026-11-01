//! C6 回归钉：/api/compliance/* 全链路走 HTTP 真跑。
//!
//! 覆盖卡内四件事 + 验收判据：
//! 1. 「误判」标记落库（POST /feedback；machineResult 由服务端从判定行推导，不接受客户端自报）
//! 2. 库视图合规状态过滤正确（/api/library/assets?compliance=…，且非法词 400）
//! 3. rule_feedback CSV 导出完整（表头 + 行数 + misjudged 列）
//! 4. 行业包管理：内置列表 / 本地导入 / 一键切换 / 只读预览；**换包不丢历史判定**
use axum::body::Body;
use axum::http::Request;
use core_storage::compliance::ComplianceRepo;
use rusqlite::Connection;
use std::path::PathBuf;
use tower::ServiceExt;

fn seed(path: &PathBuf) -> Connection {
    let _ = std::fs::remove_file(path);
    let conn = core_storage::open(path).unwrap();
    // 资产 1 合格 / 2 修复后合格（三态口径 = 仍算合格）/ 3 不合格 / 4 未判定（执行出错）/ 5 未跑
    for (id, name) in [(1, "a.jpg"), (2, "b.jpg"), (3, "c.jpg"), (4, "d.jpg"), (5, "e.jpg")] {
        conn.execute(
            "INSERT INTO assets (assetId, absPath, mediaType, missing, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (?1, ?2, 'image', 0, ?3, 'jpg', 10, 0, ?1)",
            rusqlite::params![id, format!("/x/{name}"), name],
        )
        .unwrap();
    }
    let repo = core_storage::compliance_sqlite::SqliteComplianceRepo::new(&conn);
    let add = |asset_id: i64, rule: &str, result: &str, fix: Option<&str>, ev: Option<&str>| {
        repo.insert_verdict(&core_storage::compliance::NewVerdict {
            pack_id: "com.soujpg.pack.ecommerce".into(),
            asset_id,
            job_id: 1,
            rule_id: rule.into(),
            result: result.into(),
            confidence: 0.9,
            score: Some(0.5),
            evidence: ev.map(|s| s.to_string()),
            fix_action: fix.map(|s| s.to_string()),
            fix_tier: None,
            recheck_result: None,
            judgement_scope: Some("per_asset".into()),
            group_key: None,
        })
        .unwrap();
    };
    add(1, "R1_size", "pass", None, None);
    add(2, "R1_size", "pass", Some("upscale_800"), Some(r#"{"recheck":{"ok":true}}"#));
    add(3, "R1_size", "fail", None, Some(r#"{"threshold":700}"#));
    // 资产 4：打标型软 fail + 一条真未判定（VL 超时：判定过程出错，不是「机器不敢判」）
    add(4, "R5_attribute", "fail", None, Some(r#"{"on_fail":"pass","detector":{"tags":[]}}"#));
    add(
        4,
        "R2_white_bg",
        "undetermined",
        Some("replace_bg"),
        Some(r#"{"reason":"vl_timeout","recheck":{"reason":"deviation_high"}}"#),
    );
    conn
}

async fn get(app: &axum::Router, uri: &str) -> (u16, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let body = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    let j = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    (status, j)
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
    let j = serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null);
    (status, j)
}

async fn get_text(app: &axum::Router, uri: &str) -> (u16, String) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let raw = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    (status, String::from_utf8_lossy(&raw).to_string())
}

#[tokio::test]
async fn compliance_full_flow_over_http() {
    let dir = std::env::temp_dir().join(format!("c6-compliance-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let conn = seed(&dir.join("dam.db"));
    let state = app_web::test_support::state(dir.clone(), conn);
    let app = app_web::router_without_dist(state);

    // ---------- 1) status：插件身份 + 五档分桶 + 队列口径 ----------
    let (code, st) = get(&app, "/api/compliance/status").await;
    assert_eq!(code, 200, "{st}");
    assert_eq!(
        st["verdictPackId"], "com.soujpg.pack.ecommerce",
        "判定行归属 = 处理插件身份 id：{st}"
    );
    assert!(
        st["activePack"].is_null() && st["packs"].is_null(),
        "「当前激活行业包 / 包清单」两个旧对象必须消失：{st}"
    );
    // 三态口径：修复成功不再单列「修复后合格」→ 资产 1/2 都是 pass
    assert_eq!(st["states"]["pass"], 2, "{st}");
    assert_eq!(st["states"]["fail"], 1);
    assert_eq!(st["states"]["undetermined"], 1);
    assert_eq!(st["states"]["unjudged"], 1);
    assert_eq!(st["states"]["total"], 5);
    assert!(st["states"].get("fixed").is_none(), "旧档位键必须消失: {st}");
    assert!(st["states"].get("pending_human").is_none(), "{st}");
    assert_eq!(st["queue"]["total"], 1, "低置信度复核队列 1 张（undetermined）");
    assert_eq!(st["queue"]["unresolved"], 1);
    assert_eq!(st["feedback"]["total"], 0);
    // 行业元数据来自插件自带 resources/rules.json（规则条数 > 0），不是宿主内置 YAML
    assert!(
        st["pack"]["id"] == "com.soujpg.pack.ecommerce" && st["pack"]["rules"].as_i64().unwrap() > 0,
        "规则清单必须来自处理插件：{st}"
    );

    // ---------- 2) 队列：按资产聚合 + 规则名/原因（来自激活包与证据） ----------
    let (code, q) = get(&app, "/api/compliance/queue?limit=10").await;
    assert_eq!(code, 200);
    assert_eq!(q["total"], 1);
    let card = &q["items"][0];
    assert_eq!(card["assetId"], 4);
    assert_eq!(card["fileName"], "d.jpg");
    assert_eq!(card["resolved"], false);
    let rules = card["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 1, "只列需要复核的规则（同资产软 fail 不列）");
    assert_eq!(rules[0]["ruleId"], "R2_white_bg");
    assert_eq!(rules[0]["ruleName"], "白底图纯白", "规则名来自 pack");
    assert_eq!(rules[0]["result"], "undetermined");
    assert_eq!(rules[0]["reason"], "vl_timeout");
    assert_eq!(rules[0]["feedback"], serde_json::Value::Null);

    // ---------- 3) 图库域与判定解耦（DECouple-BE，用户裁决 2026-10-31） ----------
    // 判定维度过滤已从图库列表**下线**：任何 compliance 词（含非法词）一律 400 + 稳定码 + 指路 job 域。
    // 分桶本身仍由 /api/compliance/status（上面第 1 段）与 job 域下钻负责 —— 这里只守「图库不读判定」。
    for state_word in ["pass", "fail", "undetermined", "unjudged", "bogus"] {
        let (code, r) = get(&app, &format!("/api/library/assets?compliance={state_word}")).await;
        assert_eq!(code, 400, "compliance={state_word} 必须被拒: {r}");
        assert!(
            r["error"]
                .as_str()
                .unwrap_or_default()
                .contains("compliance_filter_moved"),
            "{state_word}: {r}"
        );
    }
    // 组合维度也不再被接受（不静默忽略成「整库」）
    let (code, r) = get(&app, "/api/library/assets?compliance=fail&dirPrefix=/x&limit=50").await;
    assert_eq!(code, 400, "{r}");
    // 不带判定维度的图库列表照常工作（5 张全在）
    let (code, r) = get(&app, "/api/library/assets?limit=50").await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["total"], 5, "{r}");

    // ---------- 4) 「误判」标记：机器结论服务端推导；不一致自动记误判 ----------
    // 资产 3 机器 fail、人工判 pass → 误判
    let (code, f) = post(
        &app,
        "/api/compliance/feedback",
        r#"{"assetId":3,"ruleId":"R1_size","humanResult":"pass"}"#,
    )
    .await;
    assert_eq!(code, 200, "{f}");
    assert_eq!(f["feedback"]["machineResult"], "fail");
    assert_eq!(f["feedback"]["humanResult"], "pass");
    assert_eq!(f["feedback"]["misjudged"], true);
    assert!(
        f["feedback"]["reason"].as_str().unwrap().contains("误判"),
        "自动补误判备注: {f}"
    );
    // 客户端自报 machineResult 无效（字段被忽略，仍以库为准）
    let (code, f2) = post(
        &app,
        "/api/compliance/feedback",
        r#"{"assetId":1,"ruleId":"R1_size","humanResult":"promote","machineResult":"pass"}"#,
    )
    .await;
    assert_eq!(code, 400, "非法人工结论必须拒收: {f2}");
    // 未知规则 / 没判过的 (asset, rule)
    let (code, _) = post(
        &app,
        "/api/compliance/feedback",
        r#"{"assetId":1,"ruleId":"NOPE","humanResult":"pass"}"#,
    )
    .await;
    assert_eq!(code, 400);
    let (code, _) = post(
        &app,
        "/api/compliance/feedback",
        r#"{"assetId":5,"ruleId":"R1_size","humanResult":"pass"}"#,
    )
    .await;
    assert_eq!(code, 400, "没有机器判定的资产不能复核");
    // 队列项人工处置（undetermined → fail）：不算误判
    let (code, f3) = post(
        &app,
        "/api/compliance/feedback",
        r#"{"assetId":4,"ruleId":"R2_white_bg","humanResult":"fail","reason":"背景发灰，人工确认不合格"}"#,
    )
    .await;
    assert_eq!(code, 200, "{f3}");
    assert_eq!(f3["feedback"]["machineResult"], "undetermined");
    assert_eq!(f3["feedback"]["misjudged"], false, "未判定的人工处置不算误判");
    assert_eq!(f3["feedback"]["reason"], "背景发灰，人工确认不合格", "人工备注优先");

    // 队列：已处置 → resolved=true；onlyUnresolved 过滤掉
    let (_, q2) = get(&app, "/api/compliance/queue?onlyUnresolved=true").await;
    assert_eq!(q2["total"], 0, "只看未处置 → 空: {q2}");
    let (_, q3) = get(&app, "/api/compliance/queue").await;
    assert_eq!(q3["items"][0]["resolved"], true);
    assert_eq!(q3["items"][0]["rules"][0]["feedback"]["humanResult"], "fail");

    // ---------- 5) 反馈 CSV 导出（完整 + 内容正确） ----------
    let (code, csv) = get_text(&app, "/api/compliance/feedback/export").await;
    assert_eq!(code, 200);
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines.len(), 3, "表头 + 2 条反馈: {csv}");
    assert!(
        lines[0].contains("packId,assetId,ruleId,machineResult,humanResult,misjudged,reason,ts"),
        "表头: {}",
        lines[0]
    );
    assert!(csv.contains("\"com.soujpg.pack.ecommerce\",3,\"R1_size\",\"fail\",\"pass\",1,"), "{csv}");
    assert!(csv.contains(",0,"), "转人工处置 misjudged=0: {csv}");
    // 列表接口口径一致
    let (_, fl) = get(&app, "/api/compliance/feedback").await;
    assert_eq!(fl["total"], 2);
    let (_, one) = get(&app, "/api/compliance/feedback?assetId=4").await;
    assert_eq!(one["total"], 1);

    // 单资产判定明细（界面详情用）
    let (code, v) = get(&app, "/api/compliance/verdicts?assetId=4").await;
    assert_eq!(code, 200);
    // DECouple-BE：资产视图只回判定行原文（资产级 state/stateLabel 已下线）
    assert!(v.get("state").is_none() && v.get("stateLabel").is_none(), "{v}");
    let rows = v["items"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "该资产两条判定都列出");
    // 机器结论逐行原样保留（R2 = undetermined、R5 软 fail 仍是 fail；不因资产级汇总被改写）
    let r2 = rows.iter().find(|r| r["ruleId"] == "R2_white_bg").expect("R2 行");
    assert_eq!(r2["result"], "undetermined", "{v}");
    let r5 = rows.iter().find(|r| r["ruleId"] == "R5_attribute").expect("R5 行");
    assert_eq!(r5["result"], "fail", "{v}");

    // ---------- 6) 旧「行业包」管理面已下线（一个行业 = 一个处理插件） ----------
    for (uri, body) in [
        ("/api/compliance/packs/import", r#"{"dir":"/tmp/whatever"}"#),
        ("/api/compliance/packs/activate", r#"{"source":"builtin","id":"ecommerce-taobao"}"#),
    ] {
        let (code, res) = post(&app, uri, body).await;
        assert!(
            code == 404 || code == 405,
            "{uri} 必须已下线（不再有「装行业包 + 切引擎」两步）：{code} {res}"
        );
    }
    // 插件清单存在，且只剩插件一族
    let (code, list) = get(&app, "/api/compliance/packs").await;
    assert_eq!(code, 200, "{list}");
    assert!(list["plugins"].is_array(), "{list}");
    assert!(list["builtin"].is_null() && list["imported"].is_null(), "{list}");

    // ---------- 7) 插件详情：规则清单来自包自带 resources/rules.json ----------
    let (code, pv) = get(&app, "/api/compliance/packs/com.soujpg.pack.ecommerce?probe=false").await;
    assert_eq!(code, 200, "仓库自带的电商处理插件必须能读详情：{pv}");
    assert_eq!(pv["source"], "plugin");
    assert_eq!(pv["manifest"]["id"], "com.soujpg.pack.ecommerce");
    let rules = pv["rules"].as_array().expect("rules");
    assert!(!rules.is_empty(), "规则清单不许空：{pv}");
    assert_eq!(rules[0]["id"], "R0_product_main");
    // 不存在的插件 → 404（响亮，不静默返回空结构）
    let (code, _) = get(&app, "/api/compliance/packs/does-not-exist").await;
    assert_eq!(code, 404);

    // ---------- 8) 路由不泄漏到 /api/* 顶层 ----------
    let (code, _) = get(&app, "/api/feedback").await;
    assert_eq!(code, 404, "只能挂 /api/compliance/*");

    let _ = std::fs::remove_dir_all(&dir);
}
