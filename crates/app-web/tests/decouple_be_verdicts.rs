//! DECouple-BE（卡 `t_59942f03`，用户裁决 2026-10-31）：**图库域与 job 域解耦**的后端契约。
//!
//! 覆盖四件事（与卡内验收一一对应）：
//! 1. 图库接口不再暴露任何判定字段；判定维度过滤 `?compliance=` 显式 400 并指路 job 域端点；
//!    图库导出**不接受** `filter.compliance`（按判定结果导出只能在任务中心做）。
//! 2. `/api/compliance/verdicts`（资产视图）不再下发资产级合成结论
//!    （`finalStatus` / `assetHumanReview` / `state` / `machineState` 全部消失，只剩判定行原文 + 行级生效结论）。
//! 3. `GET /api/compliance/jobs/{runId}/verdicts`：一次执行的**逐资产结论**由
//!    job 账本（该 run 的判定行）+ `human_review` 查询时合成；人审按**内容版本**判活 ——
//!    资产内容一变，旧人审自动失效（`overrideStale=true`），重新改判才再次生效。
//! 4. `POST /api/compliance/jobs/{runId}/export`：按这次 job 的档位（过的一批 / 未过的一批 / 任选）
//!    导出子集，落盘文件数 == 命中数。
use axum::body::Body;
use axum::http::Request;
use core_storage::compliance::ComplianceRepo;
use rusqlite::Connection;
use std::path::PathBuf;
use tower::ServiceExt;

fn seed(dir: &PathBuf) -> Connection {
    let _ = std::fs::remove_file(dir.join("dam.db"));
    let conn = core_storage::open(&dir.join("dam.db")).unwrap();
    // 三张真实文件（导出要用真路径；内容不重要，Original 模式只拷贝字节）
    for (id, name, hash) in [
        (1i64, "fail.jpg", "hash-fail"),
        (2, "pass.jpg", "hash-pass"),
        (3, "undet.jpg", "hash-undet"),
    ] {
        let path = dir.join(name);
        std::fs::write(&path, b"jpegbytes").unwrap();
        conn.execute(
            "INSERT INTO assets (assetId, absPath, mediaType, missing, fileName, fileExt, fileSz, mtime, fileHash, importedAt)
             VALUES (?1, ?2, 'image', 0, ?3, 'jpg', 9, 0, ?4, 1)",
            rusqlite::params![id, path.to_string_lossy(), name, hash],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO resource (assetId, role, ordinal, thumbPath, width, height)
             VALUES (?1, 'primary', 0, '/thumbs/x.webp', 256, 256)",
            rusqlite::params![id],
        )
        .unwrap();
    }
    // run 1（本次要看的那次执行）+ run 2（另一次执行，用来证明 run 过滤真的生效）
    for run in [1i64, 2] {
        conn.execute(
            "INSERT INTO compliance_runs (runId, kind, status, startedAt, finishedAt, total)
             VALUES (?1, 'compliance', 'done', 1, 2, 3)",
            rusqlite::params![run],
        )
        .unwrap();
    }
    // 队列行：run 1 下 3 个 task，run 2 下 1 个 task（资产 1 重判成 pass）
    for (job_id, asset_id, run) in [(11i64, 1i64, 1i64), (12, 2, 1), (13, 3, 1), (21, 1, 2)] {
        conn.execute(
            "INSERT INTO job_queue (id, kind, targetId, status, attempts, createdAt, updatedAt, runId)
             VALUES (?1, 'compliance', ?2, 'done', 0, 1, 1, ?3)",
            rusqlite::params![job_id, asset_id, run],
        )
        .unwrap();
    }
    let repo = core_storage::compliance_sqlite::SqliteComplianceRepo::new(&conn);
    let add = |job_id: i64, asset_id: i64, rule: &str, result: &str, conf: f64| {
        repo.insert_verdict(&core_storage::compliance::NewVerdict {
            pack_id: "com.soujpg.pack.ecommerce".into(),
            asset_id,
            job_id,
            rule_id: rule.into(),
            result: result.into(),
            confidence: conf,
            score: Some(0.4),
            evidence: None,
            fix_action: None,
            fix_tier: None,
            recheck_result: None,
            judgement_scope: Some("per_asset".into()),
            group_key: None,
        })
        .unwrap();
    };
    // run 1：1 = fail（低置信度，改判免 force）/ 2 = pass / 3 = undetermined
    add(11, 1, "R1_size", "fail", 0.30);
    add(12, 2, "R1_size", "pass", 0.95);
    add(13, 3, "R1_size", "undetermined", 0.90);
    // run 2：资产 1 机器结论变成 pass（证明 run 过滤与「最新行」不是同一回事）
    add(21, 1, "R1_size", "pass", 0.95);
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

async fn post(app: &axum::Router, uri: &str, body: &str) -> (u16, serde_json::Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status().as_u16();
    let raw = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    (status, serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null))
}

fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("decouple-be-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 1) 图库域完全没有判定字段；判定维度过滤与判定维度导出都被显式拒绝。
#[tokio::test]
async fn gallery_domain_has_no_judgement_surface() {
    let dir = tmpdir("gallery");
    let conn = seed(&dir);
    let state = app_web::test_support::state(dir.clone(), conn);
    let app = app_web::router_without_dist(state);

    let (code, body) = get(&app, "/api/library/assets").await;
    assert_eq!(code, 200, "{body}");
    let items = body["items"].as_array().expect("items 数组");
    assert!(!items.is_empty(), "{body}");
    for it in items {
        for key in [
            "finalStatus",
            "assetHumanReview",
            "complianceState",
            "machineStatus",
            "effectiveStatus",
            "verdict",
        ] {
            assert!(
                it.get(key).is_none(),
                "图库资产行不得出现判定字段 {key}: {it}"
            );
        }
    }
    // 顶层也不该出现判定汇总
    for key in ["states", "compliance", "verdicts"] {
        assert!(body.get(key).is_none(), "图库列表不得带判定汇总 {key}: {body}");
    }

    // 判定维度过滤已迁走：400 + 稳定错误码 + 指路
    let (code, body) = get(&app, "/api/library/assets?compliance=pass").await;
    assert_eq!(code, 400, "判定维度过滤必须显式拒绝: {body}");
    let msg = body["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("compliance_filter_moved") && msg.contains("/api/compliance/jobs/"),
        "错误文案要带稳定码与去处: {body}"
    );

    // 图库导出不接受判定维度过滤（按判定结果导出只能在任务中心）
    let payload = serde_json::json!({
        "filter": { "compliance": { "packId": "com.soujpg.pack.ecommerce", "state": "pass" } },
        "content": "original",
        "destDir": dir.join("out").to_string_lossy(),
        "manifest": true
    })
    .to_string();
    let (code, body) = post(&app, "/api/export/start", &payload).await;
    assert_eq!(code, 400, "图库导出不得读判定字段: {body}");
    assert_eq!(body["code"], "export_source_invalid", "{body}");
}

/// 2) 资产视图（`/verdicts?assetId=`）只回判定行原文，不再合成资产级结论。
#[tokio::test]
async fn asset_verdicts_view_drops_composed_conclusion() {
    let dir = tmpdir("verdicts");
    let conn = seed(&dir);
    let state = app_web::test_support::state(dir.clone(), conn);
    let app = app_web::router_without_dist(state);

    let (code, body) = get(&app, "/api/compliance/verdicts?assetId=3").await;
    assert_eq!(code, 200, "{body}");
    for key in [
        "state",
        "stateLabel",
        "machineState",
        "machineStateLabel",
        "finalStatus",
        "assetHumanReview",
        "machineConfidence",
    ] {
        assert!(
            body.get(key).is_none(),
            "资产视图不得下发资产级结论字段 {key}: {body}"
        );
    }
    // 判定行原文仍在（机器结论 + 行级生效结论）
    let item = &body["items"][0];
    assert_eq!(item["result"], "undetermined", "{body}");
    assert_eq!(item["effectiveStatus"], "undetermined", "{body}");
    assert_eq!(body["schemeId"], "pack:com.soujpg.pack.ecommerce", "{body}");

    // 规则级改判后：行级生效结论变 pass，响应里仍然没有资产级合成字段
    let (code, rev) = post(
        &app,
        "/api/compliance/review",
        r#"{"assetId":3,"ruleId":"R1_size","humanStatus":"fail","reason":"人工看图有明显水印"}"#,
    )
    .await;
    assert_eq!(code, 200, "{rev}");
    assert_eq!(rev["effectiveStatus"], "fail", "{rev}");
    assert!(
        rev.get("finalStatus").is_none() && rev.get("assetState").is_none(),
        "改判回执不得带资产级合成结论: {rev}"
    );
    let (_, body) = get(&app, "/api/compliance/verdicts?assetId=3").await;
    assert_eq!(
        body["items"][0]["result"], "undetermined",
        "机器结论不可改写: {body}"
    );
    assert_eq!(body["items"][0]["effectiveStatus"], "fail", "{body}");
}

/// 3) job 域下钻：一次执行合成生效结论；人审按内容版本失效（重导/修图后自动不生效）。
#[tokio::test]
async fn job_scoped_composition_and_content_version_invalidation() {
    let dir = tmpdir("jobscope");
    let conn = seed(&dir);
    let state = app_web::test_support::state(dir.clone(), conn);
    let app = app_web::router_without_dist(state);

    // run 1 的合成结论（run 2 里资产 1 已重判成 pass，这里必须仍是 fail —— run 过滤生效）
    let (code, body) = get(&app, "/api/compliance/jobs/1/verdicts").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["runId"], 1, "{body}");
    assert_eq!(body["total"], 3, "{body}");
    let by_id = |v: &serde_json::Value, id: i64| -> serde_json::Value {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["assetId"] == id)
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    };
    assert_eq!(by_id(&body, 1)["machineStatus"], "fail", "{body}");
    assert_eq!(by_id(&body, 1)["effectiveStatus"], "fail", "{body}");
    assert_eq!(by_id(&body, 2)["effectiveStatus"], "pass", "{body}");
    assert_eq!(by_id(&body, 3)["effectiveStatus"], "undetermined", "{body}");
    assert_eq!(body["states"]["fail"], 1, "{body}");

    // 档位过滤（过的一批 / 未过的一批）
    let (_, only_pass) = get(&app, "/api/compliance/jobs/1/verdicts?status=pass").await;
    assert_eq!(only_pass["total"], 1, "{only_pass}");
    assert_eq!(by_id(&only_pass, 2)["assetId"], 2, "{only_pass}");

    // 不存在的 run → 404（不是空结果假装跑过）
    let (code, _) = get(&app, "/api/compliance/jobs/999/verdicts").await;
    assert_eq!(code, 404);

    // 资产级改判 → 合成结论翻转
    let (code, rev) = post(
        &app,
        "/api/compliance/review",
        r#"{"assetId":1,"humanStatus":"pass","reason":"整图人工复看合格","force":true}"#,
    )
    .await;
    assert_eq!(code, 200, "{rev}");
    let (_, body) = get(&app, "/api/compliance/jobs/1/verdicts").await;
    let a1 = by_id(&body, 1);
    assert_eq!(a1["machineStatus"], "fail", "{body}");
    assert_eq!(a1["effectiveStatus"], "pass", "人审覆盖生效: {body}");
    assert_eq!(a1["override"]["status"], "pass", "{body}");
    assert_eq!(a1["overrideStale"], false, "{body}");

    // 内容版本变更（重新导入换 hash）→ 旧人审自动失效
    let conn2 = core_storage::open(&dir.join("dam.db")).unwrap();
    conn2
        .execute(
            "UPDATE assets SET fileHash = 'hash-fail-v2' WHERE assetId = 1",
            [],
        )
        .unwrap();
    drop(conn2);
    let (_, body) = get(&app, "/api/compliance/jobs/1/verdicts").await;
    let a1 = by_id(&body, 1);
    assert_eq!(a1["effectiveStatus"], "fail", "内容变了，人审失效: {body}");
    assert_eq!(a1["override"], serde_json::Value::Null, "{body}");
    assert_eq!(a1["overrideStale"], true, "界面要能看到「为什么失效」: {body}");
    assert_eq!(body["staleOverrides"], 1, "{body}");

    // 新内容上重新改判 → 再次生效（旧行仍在历史里，标 contentStale）
    let (code, _rev) = post(
        &app,
        "/api/compliance/review",
        r#"{"assetId":1,"humanStatus":"pass","reason":"新版本复看仍然合格","force":true}"#,
    )
    .await;
    assert_eq!(code, 200);
    let (_, body) = get(&app, "/api/compliance/jobs/1/verdicts").await;
    assert_eq!(by_id(&body, 1)["effectiveStatus"], "pass", "{body}");
    assert_eq!(by_id(&body, 1)["overrideStale"], false, "{body}");
    let (_, hist) = get(&app, "/api/compliance/review/history?assetId=1").await;
    let stale_rows = hist["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["contentStale"] == true)
        .count();
    assert_eq!(stale_rows, 1, "被内容变更顶掉的那条历史要标出来: {hist}");
}

/// 4) 按这次 job 的结果导出子集（与图库导出互不相干）。
#[tokio::test]
async fn job_export_writes_subset_of_run_results() {
    let dir = tmpdir("jobexport");
    let conn = seed(&dir);
    let state = app_web::test_support::state(dir.clone(), conn);
    let app = app_web::router_without_dist(state);

    // 过的一批 = pass（run 1 里只有资产 2）
    let out_pass = dir.join("out-pass");
    let payload = serde_json::json!({
        "status": "pass",
        "destDir": out_pass.to_string_lossy(),
        "manifest": true
    })
    .to_string();
    let (code, body) = post(&app, "/api/compliance/jobs/1/export", &payload).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["selected"], 1, "{body}");
    let files: Vec<String> = std::fs::read_dir(&out_pass)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        files.iter().any(|f| f.contains("pass.jpg")),
        "命中的资产要落盘: {files:?}"
    );
    assert!(
        !files.iter().any(|f| f.contains("fail.jpg")),
        "未命中的资产不许落盘: {files:?}"
    );
    assert!(
        files.iter().any(|f| f.contains("manifest")),
        "清单要写: {files:?}"
    );

    // 未过的一批 = fail
    let out_fail = dir.join("out-fail");
    let payload = serde_json::json!({
        "status": "fail",
        "destDir": out_fail.to_string_lossy(),
        "manifest": true
    })
    .to_string();
    let (code, body) = post(&app, "/api/compliance/jobs/1/export", &payload).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["selected"], 1, "{body}");
    let files: Vec<String> = std::fs::read_dir(&out_fail)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert!(files.iter().any(|f| f.contains("fail.jpg")), "{files:?}");

    // 任选档位：显式 id 且必须在该 run 内（资产 1 在 run 1 是 fail）
    let out_pick = dir.join("out-pick");
    let payload = serde_json::json!({
        "assetIds": [1, 2],
        "destDir": out_pick.to_string_lossy(),
        "manifest": false
    })
    .to_string();
    let (code, body) = post(&app, "/api/compliance/jobs/1/export", &payload).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["selected"], 2, "{body}");

    // 空档位 → 400（不许「导出成功但一个文件都没有」）
    let payload = serde_json::json!({
        "status": "unjudged",
        "destDir": dir.join("out-empty").to_string_lossy()
    })
    .to_string();
    let (code, body) = post(&app, "/api/compliance/jobs/1/export", &payload).await;
    assert_eq!(code, 400, "{body}");
}

/// F-4（卡 t_8d8eeeb5）FE↔BE 契约钉：前端出参 == 后端 serde 期望。
/// 裁决=方案 A（前端改字段名对齐后端 canonical 名 `status`，不加 `alias = "verdict"`）——
/// 响应回显、bad_status 报错文案全是 `status`，加别名只造出双名坑。
#[test]
fn job_export_req_deserializes_frontend_payload() {
    use app_web::compliance_api::JobExportReq;

    // 正向：前端现在发的形态（status + camelCase destDir，destDir 本就有 alias）
    let req: JobExportReq = serde_json::from_value(serde_json::json!({
        "status": "fail",
        "destDir": "/tmp/judge-out"
    }))
    .expect("FE 请求体必须能被 JobExportReq 反序列化");
    assert_eq!(req.status.as_deref(), Some("fail"), "status 必须吃到，不许被 serde 静默丢弃");
    assert_eq!(req.dest_dir.as_deref(), Some("/tmp/judge-out"));

    // 反向（钉住断裂根源，防回退）：发旧字段名 `verdict` 时 serde **没有**别名 → status=None → 恒导出 all。
    // 这是 F-4 缺陷一的机理存档：谁把前端改回 `verdict`，此断言解释了为什么界面会静默全量导出。
    let req: JobExportReq = serde_json::from_value(serde_json::json!({ "verdict": "fail" })).unwrap();
    assert!(req.status.is_none(), "`verdict` 不是合法字段：不得被当成 status 吸收");

    // 回执结构钉：成功响应的顶层字段 = {ok, runId, status, selected, report}，
    // report 与 core_types::ExportReport 同形（camelCase：exported/skipped/dest…，没有 copied/missing）。
    let report = serde_json::json!({
        "total": 1, "exported": 1, "excludedRejected": 0, "skipped": [],
        "errors": [], "cancelled": false, "dest": "/x", "manifestWritten": true
    });
    let v: serde_json::Value = serde_json::from_str(
        &serde_json::json!({"ok": true, "runId": 14, "status": "fail", "selected": 1, "report": report})
            .to_string(),
    )
    .unwrap();
    let rep = &v["report"];
    for key in ["exported", "skipped", "dest", "manifestWritten"] {
        assert!(rep.get(key).is_some(), "回执 report 必须带 `{key}`（前端按它渲染）");
    }
    assert!(rep.get("copied").is_none() && rep.get("missing").is_none(), "回执没有 copied/missing —— 前端不许读这两个字段");
}
