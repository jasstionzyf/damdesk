//! COMPLIANCE-HUMAN（卡 t_03127b19）：人工二次审核全链路走 HTTP 真跑。
//!
//! 覆盖卡内验收判据：
//! 1. 改判 → effectiveStatus（行级生效结论）变化（机器结论不变、置信度/快照留档）；
//! 2. reason 必填（错误码 review_reason_required）+ humanStatus 白名单（review_bad_status）；
//! 3. 低置信度门槛：高置信度结果不给 force 就拒（review_not_low_confidence），给 force 才落；
//! 4. 撤销 → 回机器结论（DELETE；重复撤销 404 review_not_found；历史保留 active=false）；
//! 5. 重复改判覆盖（只留一条生效，旧行 revokeReason=superseded）；
//! 6. 口径联动：合规页分桶（/status）、队列卡片、复核列表(/review)、改判历史(/review/history)
//!    全部按 effectiveStatus = humanStatus ?? result；**图库侧不再有判定维度**（DECouple-BE 起
//!    `/api/library/assets?compliance=` 一律 400 `compliance_filter_moved`，见 `decouple_be_verdicts.rs`）；
//! 7. 资产级改判（ruleId 缺省）整张图一条结论；无机器判定的资产拒收（review_no_machine_verdict）。
//!
//! 证据：`cargo test -p app-web --test compliance_review_routes -- --nocapture` 会把关键响应
//! 以 `EVIDENCE-…` 行打出来（reports/COMPLIANCE-HUMAN.md §5 的「改判前后对照」就是从这抄的）。
use axum::body::Body;
use axum::http::Request;
use core_storage::compliance::ComplianceRepo;
use rusqlite::Connection;
use std::path::PathBuf;
use tower::ServiceExt;

fn seed(path: &PathBuf) -> Connection {
    let _ = std::fs::remove_file(path);
    let conn = core_storage::open(path).unwrap();
    // 1 = 低置信度 fail（0.3，应进复核队列）/ 2 = undetermined / 3 = 高置信度 pass（0.95）/
    // 4 = 完全没跑判定
    for (id, name) in [(1, "a.jpg"), (2, "b.jpg"), (3, "c.jpg"), (4, "d.jpg")] {
        conn.execute(
            "INSERT INTO assets (assetId, absPath, mediaType, missing, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (?1, ?2, 'image', 0, ?3, 'jpg', 10, 0, ?1)",
            rusqlite::params![id, format!("/x/{name}"), name],
        )
        .unwrap();
    }
    // 复核视图要缩略图：资产 1 有 primary 缩略图，资产 3 没有（验证 null 不报错）
    conn.execute(
        "INSERT INTO resource (assetId, role, ordinal, thumbPath, width, height)
         VALUES (1, 'primary', 0, '/thumbs/1.webp', 256, 256)",
        [],
    )
    .unwrap();
    let repo = core_storage::compliance_sqlite::SqliteComplianceRepo::new(&conn);
    let add = |asset_id: i64, rule: &str, result: &str, confidence: f64, ev: Option<&str>| {
        repo.insert_verdict(&core_storage::compliance::NewVerdict {
            pack_id: "com.soujpg.pack.ecommerce".into(),
            asset_id,
            job_id: 1,
            rule_id: rule.into(),
            result: result.into(),
            confidence,
            score: Some(0.4),
            evidence: ev.map(|s| s.to_string()),
            fix_action: None,
            fix_tier: None,
            recheck_result: None,
            judgement_scope: Some("per_asset".into()),
            group_key: None,
        })
        .unwrap();
    };
    add(1, "R1_size", "fail", 0.30, Some(r#"{"threshold":700}"#));
    add(2, "R1_size", "undetermined", 0.90, Some(r#"{"reason":"vl_timeout"}"#));
    add(3, "R1_size", "pass", 0.95, None);
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
    (status, serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null))
}

async fn send(app: &axum::Router, method: &str, uri: &str, body: Option<&str>) -> (u16, serde_json::Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let req = req
        .body(body.map(|b| Body::from(b.to_string())).unwrap_or_else(Body::empty))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status().as_u16();
    let raw = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    (status, serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null))
}

fn library_ids(v: &serde_json::Value) -> Vec<i64> {
    // AssetView 是扁平结构（assetId 在顶层，不是 asset.assetId）
    let mut ids: Vec<i64> = v["items"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x["assetId"].as_i64()).collect())
        .unwrap_or_default();
    ids.sort_unstable();
    ids
}

#[tokio::test]
async fn human_review_full_flow_over_http() {
    let dir = std::env::temp_dir().join(format!("compliance-human-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let conn = seed(&dir.join("dam.db"));
    let state = app_web::test_support::state(dir.clone(), conn);
    let app = app_web::router_without_dist(state);

    // ---------- 0) 初始分桶：1 不合格 / 2 未判定 / 3 合格 / 4 未跑 ----------
    let (code, st) = get(&app, "/api/compliance/status").await;
    assert_eq!(code, 200, "{st}");
    assert_eq!(st["states"]["fail"], 1, "{st}");
    assert_eq!(st["states"]["undetermined"], 1, "{st}");
    assert_eq!(st["states"]["pass"], 1, "{st}");
    assert_eq!(st["states"]["unjudged"], 1, "{st}");
    // DECouple-BE：图库列表不再接受判定维度过滤（400 + 稳定码），判定视图只在 job 域
    let (code, moved) = get(&app, "/api/library/assets?compliance=pass").await;
    assert_eq!(code, 400, "图库与 job 解耦: {moved}");
    assert!(
        moved["error"].as_str().unwrap_or_default().contains("compliance_filter_moved"),
        "{moved}"
    );
    let (_, lib) = get(&app, "/api/library/assets").await;
    assert_eq!(library_ids(&lib), vec![1, 2, 3, 4], "图库列表与判定无关（全部资产）: {lib}");

    // ---------- 1) 校验矩阵：reason 必填 / humanStatus 白名单 ----------
    let low_conf_body = |extra: &str| {
        format!(r#"{{"assetId":1,"ruleId":"R1_size","humanStatus":"pass","reason":"阈值过严，人工确认尺寸达标"{extra}}}"#)
    };
    // reason 纯空白
    let (code, r) = send(
        &app,
        "POST",
        "/api/compliance/review",
        Some(r#"{"assetId":1,"ruleId":"R1_size","humanStatus":"pass","reason":"   "}"#),
    )
    .await;
    assert_eq!(code, 400, "{r}");
    assert_eq!(r["error"], "review_reason_required", "{r}");
    assert!(r["message"].as_str().unwrap_or_default().contains("理由"), "{r}");
    // humanStatus 非法
    let (code, r) = send(
        &app,
        "POST",
        "/api/compliance/review",
        Some(r#"{"assetId":1,"ruleId":"R1_size","humanStatus":"undetermined","reason":"x"}"#),
    )
    .await;
    assert_eq!(code, 400, "{r}");
    assert_eq!(r["error"], "review_bad_status", "{r}");
    // 无机器判定的资产（4）
    let (code, r) = send(
        &app,
        "POST",
        "/api/compliance/review",
        Some(r#"{"assetId":4,"humanStatus":"pass","reason":"人工放行"}"#),
    )
    .await;
    assert_eq!(code, 400, "{r}");
    assert_eq!(r["error"], "review_no_machine_verdict", "{r}");
    // 未落库
    let (_, hist) = get(&app, "/api/compliance/review/history").await;
    assert_eq!(hist["total"], 0, "{hist}");

    // ---------- 2) 低置信度改判（0.30 < 0.70，免 force）：fail → pass ----------
    let (code, r) = send(&app, "POST", "/api/compliance/review", Some(&low_conf_body(""))).await;
    assert_eq!(code, 200, "{r}");
    // DECouple-BE：回执只给行级生效结论（资产级合成结论已从资产视图下线，只在 job 域给）
    assert_eq!(r["effectiveStatus"], "pass", "{r}");
    assert_eq!(r["machineResult"], "fail", "机器结论留档（快照）: {r}");
    assert_eq!(r["machineConfidence"], 0.30, "{r}");
    assert!(r.get("finalStatus").is_none(), "finalStatus 已废弃: {r}");
    assert!(r.get("assetState").is_none(), "assetState 已下线: {r}");
    assert_eq!(r["humanReview"]["humanStatus"], "pass", "{r}");
    eprintln!("EVIDENCE-POST-REVIEW: {}", serde_json::to_string(&r).unwrap());
    // 分桶联动（合规页）：不合格 1 → 0，合格 1 → 2
    let (_, st) = get(&app, "/api/compliance/status").await;
    assert_eq!(st["states"]["fail"], 0, "{st}");
    assert_eq!(st["states"]["pass"], 2, "{st}");
    // DECouple-BE：分桶只走 job 域接口；图库侧永远不接受判定维度
    let (code, moved) = get(&app, "/api/library/assets?compliance=fail").await;
    assert_eq!(code, 400, "图库不接受判定维度: {moved}");

    // ---------- 3) 判定明细：机器结论保留 + effectiveStatus/humanReview 由服务端算 ----------
    let (code, vd) = get(&app, "/api/compliance/verdicts?assetId=1").await;
    assert_eq!(code, 200, "{vd}");
    // 资产视图只回判定行原文（资产级合成结论 state/machineState/finalStatus 已下线）
    assert!(vd.get("state").is_none() && vd.get("machineState").is_none(), "{vd}");
    let item = vd["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["ruleId"] == "R1_size")
        .expect("明细行");
    assert_eq!(item["result"], "fail", "机器结论不被改写: {item}");
    assert_eq!(item["effectiveStatus"], "pass", "{item}");
    assert_eq!(item["confidence"], 0.30, "{item}");
    assert_eq!(item["humanReview"]["humanStatus"], "pass", "{item}");
    eprintln!("EVIDENCE-VERDICTS: {}", serde_json::to_string(&vd).unwrap());

    // ---------- 4) 复核队列：卡片自带 effectiveStatus + reviewed 过滤 ----------
    let (code, q) = get(&app, "/api/compliance/queue?lowConfidence=true&onlyUnresolved=false").await;
    assert_eq!(code, 200, "{q}");
    let card = q["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["assetId"] == 1)
        .expect("资产 1 在低置信度队列里");
    assert_eq!(card["reviewed"], true, "{card}");
    assert_eq!(card["rules"][0]["result"], "fail", "{card}");
    assert_eq!(card["rules"][0]["effectiveStatus"], "pass", "卡片直接给生效结论: {card}");
    assert_eq!(card["rules"][0]["humanReview"]["humanStatus"], "pass", "{card}");
    assert_eq!(card["thumbPath"], "/thumbs/1.webp", "复核卡片自带缩略图: {card}");
    eprintln!("EVIDENCE-QUEUE-CARD: {}", serde_json::to_string(&card).unwrap());
    // 只看未复核 → 资产 1 已不再出现（其余资产仍在）
    let (_, q2) = get(&app, "/api/compliance/queue?lowConfidence=true&reviewed=false").await;
    assert!(
        !q2["items"].as_array().unwrap().iter().any(|c| c["assetId"] == 1),
        "{q2}"
    );

    // 排序：confidence 升序（最没把握的排最前）；非法 sort 值 400（不静默）
    let (code, q3) = get(&app, "/api/compliance/queue?lowConfidence=true&sort=confidence").await;
    assert_eq!(code, 200, "{q3}");
    let confs: Vec<f64> = q3["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["rules"][0]["confidence"].as_f64().unwrap_or(-1.0))
        .collect();
    assert!(confs.windows(2).all(|w| w[0] <= w[1]), "置信度升序: {confs:?}");
    let (code, r) = get(&app, "/api/compliance/queue?sort=bogus").await;
    assert_eq!(code, 400, "{r}");
    assert_eq!(r["error"], "bad_sort", "{r}");
    let (code, list) = get(&app, "/api/compliance/review?lowConfidence=false&sort=confidence").await;
    assert_eq!(code, 200, "{list}");
    let confs: Vec<f64> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["machineConfidence"].as_f64().unwrap_or(-1.0))
        .collect();
    assert!(confs.windows(2).all(|w| w[0] <= w[1]), "/review 置信度升序: {confs:?}");

    // ---------- 5) 高置信度门槛：不给 force 拒收，给 force 才落 ----------
    let (code, r) = send(
        &app,
        "POST",
        "/api/compliance/review",
        Some(r#"{"assetId":3,"ruleId":"R1_size","humanStatus":"fail","reason":"人工确认有水印"}"#),
    )
    .await;
    assert_eq!(code, 400, "{r}");
    assert_eq!(r["error"], "review_not_low_confidence", "{r}");
    assert!(r["message"].as_str().unwrap_or_default().contains("0.95"), "{r}");
    let (code, r) = send(
        &app,
        "POST",
        "/api/compliance/review",
        Some(r#"{"assetId":3,"ruleId":"R1_size","humanStatus":"fail","reason":"人工确认有水印","force":true}"#),
    )
    .await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["effectiveStatus"], "fail", "{r}");
    let (_, st) = get(&app, "/api/compliance/status").await;
    assert_eq!((st["states"]["fail"].as_i64(), st["states"]["pass"].as_i64()), (Some(1), Some(1)), "{st}");

    // ---------- 6) 重复改判覆盖：只留一条生效，历史保留 superseded ----------
    let (code, r) = send(
        &app,
        "POST",
        "/api/compliance/review",
        Some(r#"{"assetId":1,"ruleId":"R1_size","humanStatus":"fail","reason":"再看一遍确实不合格"}"#),
    )
    .await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["effectiveStatus"], "fail", "{r}");
    let (_, list) = get(&app, "/api/compliance/review?assetId=1&lowConfidence=false").await;
    let items = list["items"].as_array().unwrap();
    let r1: Vec<&serde_json::Value> = items
        .iter()
        .filter(|i| i["ruleId"] == "R1_size")
        .collect();
    assert_eq!(r1.len(), 1, "同一 (asset, rule) 只有一条生效改判: {list}");
    assert_eq!(r1[0]["humanReview"]["humanStatus"], "fail", "{list}");
    assert_eq!(r1[0]["machineResult"], "fail", "{list}");
    assert_eq!(r1[0]["effectiveStatus"], "fail", "{list}");
    let (_, hist) = get(&app, "/api/compliance/review/history?assetId=1").await;
    let h = hist["items"].as_array().unwrap();
    assert_eq!(h.len(), 2, "被覆盖的旧改判留历史: {hist}");
    assert_eq!(h[0]["humanStatus"], "pass", "{hist}");
    assert_eq!(h[0]["active"], false, "{hist}");
    assert_eq!(h[0]["revokeReason"], "superseded", "{hist}");
    assert_eq!(h[1]["active"], true, "{hist}");

    // ---------- 7) 撤销 → 回机器结论 ----------
    let (code, r) = send(&app, "DELETE", "/api/compliance/review?assetId=1&ruleId=R1_size", None).await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["revoked"], 1, "{r}");
    assert!(r.get("assetState").is_none(), "资产级合成结论已下线: {r}");
    eprintln!("EVIDENCE-REVOKE: {}", serde_json::to_string(&r).unwrap());
    let (code, vd) = get(&app, "/api/compliance/verdicts?assetId=1").await;
    assert_eq!(code, 200, "{vd}");
    assert!(vd.get("state").is_none(), "资产视图不下发资产级结论: {vd}");
    let item = vd["items"].as_array().unwrap().iter().find(|i| i["ruleId"] == "R1_size").unwrap();
    assert!(item["humanReview"].is_null(), "撤销后没有生效改判: {item}");
    assert_eq!(item["effectiveStatus"], "fail", "{item}");
    let (_, st) = get(&app, "/api/compliance/status").await;
    assert_eq!((st["states"]["fail"].as_i64(), st["states"]["pass"].as_i64()), (Some(2), Some(0)), "{st}");
    // 图库侧不参与判定（撤销后判定维度仍被拒）
    let (code, moved) = get(&app, "/api/library/assets?compliance=fail").await;
    assert_eq!(code, 400, "{moved}");
    // 重复撤销 → 404（语义明确，不静默成功）
    let (code, r) = send(&app, "DELETE", "/api/compliance/review?assetId=1&ruleId=R1_size", None).await;
    assert_eq!(code, 404, "{r}");
    assert_eq!(r["error"], "review_not_found", "{r}");
    // 历史两条都在（pass 被覆盖 / fail 被撤销），且都 active=false
    let (_, hist) = get(&app, "/api/compliance/review/history?assetId=1").await;
    let h = hist["items"].as_array().unwrap();
    assert_eq!(h.len(), 2, "{hist}");
    assert!(h.iter().all(|r| r["active"] == false), "撤销后无生效改判: {hist}");

    // ---------- 8) 资产级改判（ruleId 缺省）：整张图一条结论 ----------
    let (code, r) = send(
        &app,
        "POST",
        "/api/compliance/review",
        Some(r#"{"assetId":2,"humanStatus":"pass","reason":"VL 超时但人工看图达标","reviewedBy":"yufei"}"#),
    )
    .await;
    assert_eq!(code, 200, "{r}");
    assert!(r["ruleId"].is_null(), "资产级改判 ruleId = null: {r}");
    assert_eq!(r["effectiveStatus"], "pass", "{r}");
    assert!(r.get("assetState").is_none() && r.get("machineState").is_none(), "{r}");
    // 资产视图不再合成资产级结论（assetHumanReview 已下线）；资产级裁决只在 job 域/复核列表可见
    let (_, vd) = get(&app, "/api/compliance/verdicts?assetId=2").await;
    assert!(vd.get("state").is_none() && vd.get("assetHumanReview").is_none(), "{vd}");
    // 资产级行单独出现在复核列表里（ruleId = null）
    let (_, list) = get(&app, "/api/compliance/review?assetId=2&lowConfidence=false").await;
    let asset_row = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["ruleId"].is_null())
        .expect("资产级改判行");
    assert_eq!(asset_row["effectiveStatus"], "pass", "{list}");
    assert_eq!(asset_row["machineResult"], "undetermined", "{list}");

    // ---------- 9) reviewed=false：只剩没改判的行 ----------
    let (code, list) = get(&app, "/api/compliance/review?lowConfidence=false&reviewed=false").await;
    assert_eq!(code, 200, "{list}");
    assert!(
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["humanReview"].is_null()),
        "{list}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
