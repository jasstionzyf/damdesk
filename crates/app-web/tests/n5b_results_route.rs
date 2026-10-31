//! N5b 结果表接口钉（卡 task）：`GET /api/compliance/results` 的契约面。
//!
//! 钉四件事（离线：只造判定行 + 安装记录，不起子进程）：
//! 1. **执行结果 ≠ 判定结论**：`result` 三值（ok/failed/skipped）与 `verdict{status,...}` 分列，
//!    处理类行（verdict NULL）如实回 `verdict=null`，不拿 ok 冒充 pass；
//! 2. `kinds` 来自**安装记录**（0028 `packs.kinds`），不从 manifest 现场猜；
//! 3. `columns` 来自包内 `resources/result_columns.json`，未声明 → null（前端用默认列）；
//! 4. `fields` 原样搬 `evidence.fields`（行业自定义列的数据来源）。
use axum::body::Body;
use axum::http::Request;
use core_storage::compliance::{ComplianceRepo, NewVerdict};
use core_storage::packs::{PackRecord, PackRepo};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use tower::ServiceExt;

async fn get(app: &axum::Router, uri: &str) -> (u16, serde_json::Value) {
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

/// 造「已安装的包目录」：manifest + 可选 result_columns + 账本行（kinds 落 0028 的列）。
fn install_pack(dir: &Path, id: &str, kinds: &str, columns: Option<&str>) -> PathBuf {
    let pack_dir = dir.join("packs").join(id).join("1.0.0");
    std::fs::create_dir_all(pack_dir.join("resources")).unwrap();
    std::fs::write(
        pack_dir.join("manifest.json"),
        format!(
            r#"{{"id":"{id}","name":"N5b 夹具","version":"1.0.0","api_version":"1.1",
                 "entrypoint":{{"cmd":"python3","args":["plugin/__init__.py"]}},
                 "capabilities":[],"deterministic":true,"timeout_secs":30,
                 "resources":["resources/rules.json"],"kinds":["{}"]}}"#,
            kinds.replace(',', "\",\"")
        ),
    )
    .unwrap();
    std::fs::write(pack_dir.join("resources/rules.json"), "[]").unwrap();
    if let Some(text) = columns {
        std::fs::write(pack_dir.join("resources/result_columns.json"), text).unwrap();
    }
    pack_dir
}

fn seed(tag: &str) -> (PathBuf, Connection, axum::Router) {
    let dir = std::env::temp_dir().join(format!("n5b-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("dam.db");
    let conn = core_storage::open(&db).unwrap();
    for (id, name) in [(1, "a.jpg"), (2, "b.jpg")] {
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

/// 判定类行（带 `evidence.fields` 行业列）。
fn judge_row(conn: &Connection, pack_id: &str, asset: i64, rule: &str, word: &str, fields: &str) {
    let repo = core_storage::compliance_sqlite::SqliteComplianceRepo::new(conn);
    let evidence = serde_json::json!({
        "reason": format!("{rule}_reason"),
        "fields": serde_json::from_str::<serde_json::Value>(fields).unwrap(),
    })
    .to_string();
    repo.insert_verdict(&NewVerdict {
        pack_id: pack_id.into(),
        asset_id: asset,
        job_id: 5,
        rule_id: rule.into(),
        result: word.into(),
        confidence: 0.81,
        score: Some(0.81),
        evidence: Some(evidence),
        fix_action: None,
        fix_tier: None,
        recheck_result: None,
        judgement_scope: Some("per_asset".into()),
        group_key: None,
    })
    .unwrap();
}

fn register(conn: &Connection, id: &str, dir: &Path, kinds: &str) {
    let repo = core_storage::packs::SqlitePackRepo::new(conn);
    repo.upsert(&PackRecord {
        id: id.into(),
        version: "1.0.0".into(),
        dir: dir.to_string_lossy().to_string(),
        manifest_hash: format!("hash-{id}"),
        api_version: "1.1".into(),
        deterministic: true,
        signature_status: "unsigned".into(),
        kinds: kinds.into(),
        installed_at: 1,
        current: true,
    })
    .unwrap();
    repo.set_current(id, "1.0.0").unwrap();
}

#[tokio::test]
async fn judge_pack_without_columns_uses_default_columns_and_splits_result_verdict() {
    let (dir, conn, app) = seed("judge-default");
    let id = "com.n5b.judge";
    let pack_dir = install_pack(&dir, id, "judge", None);
    register(&conn, id, &pack_dir, "judge");
    judge_row(&conn, id, 1, "R1_size", "pass", r#"{"dev":1.2}"#);
    judge_row(&conn, id, 1, "R2_bg", "fail", r#"{}"#);
    judge_row(&conn, id, 2, "R1_size", "skipped", r#"{}"#);

    let (status, body) = get(&app, &format!("/api/compliance/results?packId={id}")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["kinds"], serde_json::json!(["judge"]));
    assert_eq!(
        body["columns"],
        serde_json::Value::Null,
        "没声明列 → 默认列"
    );
    assert_eq!(body["total"], 3, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    // 行序：assetId, ruleId
    assert_eq!(items[0]["ruleId"], "R1_size");
    assert_eq!(items[0]["result"], "ok", "执行结果 = ok（跑成了）");
    assert_eq!(items[0]["verdict"]["status"], "pass", "审核结论单独一列");
    assert_eq!(items[0]["verdict"]["confidence"], 0.81);
    assert_eq!(items[0]["verdict"]["reasonCode"], "R1_size_reason");
    // skipped：执行跳过了，没有结论 —— 不许把 skipped 当结论
    assert_eq!(items[2]["result"], "skipped");
    assert_eq!(items[2]["verdict"], serde_json::Value::Null);
    // 行业自定义列的数据来源：evidence.fields 原样搬出来
    assert_eq!(items[0]["fields"]["dev"], 1.2);
    assert_eq!(items[1]["fields"], serde_json::json!({}));
}

#[tokio::test]
async fn process_pack_rows_have_no_verdict_and_columns_come_from_declaration() {
    let (dir, conn, app) = seed("process-cols");
    let id = "com.n5b.process";
    let pack_dir = install_pack(
        &dir,
        id,
        "process",
        Some(
            r#"[{"key":"cost","label":"耗时","type":"number","unit":"ms","order":2},
                {"key":"fmt","label":"格式","type":"text","order":1}]"#,
        ),
    );
    register(&conn, id, &pack_dir, "process");
    // PROC-RES-1（0032 口径）：处理类行在 asset_versions，不写 rule_verdict
    // （空判定行会污染「未判定」统计）—— result=ok + 产物 path/sha256
    conn.execute(
        "INSERT INTO asset_versions (assetId, jobId, runId, packId, packVersion, pluginHash, \
         actions, result, path, sha256, bytes, format, width, height, chain, durationMs, \
         createdAt, current) \
         VALUES (1, 9, NULL, ?1, '1.0.0', 'h', '[\"resize\"]', 'ok', '/fixes/a.resize.jpg', \
         'ab12', 1024, 'jpg', 800, 600, '[]', 42, 0, 1)",
        [id],
    )
    .unwrap();
    // 行级执行失败：result=failed 且无产物
    conn.execute(
        "INSERT INTO asset_versions (assetId, jobId, runId, packId, packVersion, pluginHash, \
         actions, result, errorCode, errorMessage, chain, durationMs, createdAt, current) \
         VALUES (2, 9, NULL, ?1, '1.0.0', 'h', '[\"convert\"]', 'failed', 'decode_failed', \
         'decode_failed', '[]', 1, 0, 0)",
        [id],
    )
    .unwrap();

    let (status, body) = get(&app, &format!("/api/compliance/results?packId={id}")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["kinds"],
        serde_json::json!(["process"]),
        "处理类：前端不画审核列"
    );
    let cols = body["columns"].as_array().unwrap();
    assert_eq!(cols.len(), 2);
    assert_eq!(cols[0]["key"], "fmt", "order 小的排前面");
    assert_eq!(cols[1]["unit"], "ms");
    let items = body["items"].as_array().unwrap();
    assert_eq!(body["total"], 2, "{body}");
    assert_eq!(items[0]["verdict"], serde_json::Value::Null);
    assert_eq!(items[0]["ruleId"], "resize", "ruleId 槽位 = 链拼接");
    let arts = items[0]["artifacts"].as_array().unwrap();
    assert_eq!(arts.len(), 1, "ok 行带产物：{}", items[0]);
    assert_eq!(arts[0]["kind"], "export_file");
    assert_eq!(arts[0]["sha256"], "ab12");
    assert_eq!(
        items[1]["result"], "failed",
        "跑失败如实回 failed，不改成 undetermined"
    );
    assert_eq!(items[1]["verdict"], serde_json::Value::Null);
    assert_eq!(items[1]["reason"], "decode_failed");
    assert_eq!(
        items[1]["artifacts"].as_array().unwrap().len(),
        0,
        "failed 行无产物"
    );
}

#[tokio::test]
async fn invalid_column_declarations_fall_back_to_default_columns_with_warnings() {
    let (dir, conn, app) = seed("bad-cols");
    let id = "com.n5b.badcols";
    // 自造类型 + 缺 key：全部非法 → columns=null（默认列），warnings 逐项给原因
    let pack_dir = install_pack(
        &dir,
        id,
        "judge",
        Some(r#"[{"key":"x","label":"X","type":"score"},{"label":"没 key","type":"text"}]"#),
    );
    register(&conn, id, &pack_dir, "judge");
    judge_row(&conn, id, 1, "R1", "pass", "{}");

    let (status, body) = get(&app, &format!("/api/compliance/results?packId={id}")).await;
    assert_eq!(status, 200, "坏声明不许让整页报错：{body}");
    assert_eq!(body["columns"], serde_json::Value::Null);
    assert_eq!(body["columnWarnings"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn job_filter_and_latest_row_per_asset_rule() {
    let (dir, conn, app) = seed("job-filter");
    let id = "com.n5b.jobs";
    let pack_dir = install_pack(&dir, id, "judge", None);
    register(&conn, id, &pack_dir, "judge");
    judge_row(&conn, id, 1, "R1", "fail", "{}");
    // 重跑：同 (asset, rule) 新一行（另一个 job）→ 默认视图只留最新一行
    conn.execute("UPDATE rule_verdict SET jobId = 1234 WHERE assetId = 1", [])
        .unwrap();
    judge_row(&conn, id, 1, "R1", "pass", "{}");
    conn.execute("UPDATE rule_verdict SET jobId = 1235 WHERE id = 2", [])
        .unwrap();

    let (_, body) = get(&app, &format!("/api/compliance/results?packId={id}")).await;
    assert_eq!(body["total"], 1, "同 (asset, rule) 只留最新一行");
    assert_eq!(body["items"][0]["verdict"]["status"], "pass");

    // 指定 jobId = 看那一次执行写了什么（不被后续重跑覆盖）
    let (_, job_a) = get(
        &app,
        &format!("/api/compliance/results?packId={id}&jobId=1234"),
    )
    .await;
    assert_eq!(job_a["total"], 1, "按 jobId 取那一次的行");
    assert_eq!(job_a["items"][0]["verdict"]["status"], "fail");
    assert_eq!(job_a["jobId"], 1234);
    let (_, job_b) = get(
        &app,
        &format!("/api/compliance/results?packId={id}&jobId=1235"),
    )
    .await;
    assert_eq!(job_b["items"][0]["verdict"]["status"], "pass");
}

/// N12（卡 task）：`?runId=` 按**一次执行**取行 —— `?jobId=` 是单资产队列行。
///
/// 为什么必须分开：任务中心列的是 run（用户点的那一次），一次 run 下有 N 个资产队列行；
/// 用 runId 去匹配 `rule_verdict.jobId` 只能命中第一张图（实测 3 张图只回 6 行，应 18 行）。
#[tokio::test]
async fn run_id_filters_all_assets_of_one_execution() {
    let (dir, conn, app) = seed("runid");
    let id = "com.n5b.runid";
    let pack_dir = install_pack(&dir, id, "judge", None);
    register(&conn, id, &pack_dir, "judge");

    // 两次执行：run 7 = 资产 1/2 两行队列任务；run 8 = 资产 1 一行
    for (job, run, target) in [(101, 7, 1), (102, 7, 2), (103, 8, 1)] {
        conn.execute(
            "INSERT INTO job_queue (id, kind, targetId, status, createdAt, updatedAt, runId)
             VALUES (?1, 'compliance', ?2, 'done', 0, 0, ?3)",
            rusqlite::params![job, target, run],
        )
        .unwrap();
    }
    for (job, asset) in [(101, 1), (102, 2), (103, 1)] {
        let repo = core_storage::compliance_sqlite::SqliteComplianceRepo::new(&conn);
        repo.insert_verdict(&NewVerdict {
            pack_id: id.into(),
            asset_id: asset,
            job_id: job,
            rule_id: "R1_size".into(),
            // `NewVerdict.result` 存的是**判定词**（pass/fail/undetermined/skipped）：0028 之后
            // result 列恒为执行结果 'ok'，判定词落 verdict 列 —— 这里给 'ok' 会被 CHECK 拒收。
            result: "pass".into(),
            confidence: 0.9,
            score: None,
            evidence: None,
            fix_action: None,
            fix_tier: None,
            recheck_result: None,
            judgement_scope: Some("per_asset".into()),
            group_key: None,
        })
        .unwrap();
    }

    // 一次执行 = 2 个资产的行（不是只有 asset 1）
    let (status, body) = get(
        &app,
        &format!("/api/compliance/results?packId={id}&runId=7"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["total"], 2, "{body}");
    assert_eq!(body["runId"], 7);
    let assets: Vec<i64> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["assetId"].as_i64().unwrap())
        .collect();
    assert_eq!(assets, vec![1, 2], "两行都在");

    // 单资产队列行口径仍然可用（诊断用）
    let (status, body) = get(
        &app,
        &format!("/api/compliance/results?packId={id}&jobId=102"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["total"], 1, "{body}");
    assert_eq!(body["items"][0]["assetId"], 2);

    // 缺省（不带任何过滤）= 每 (asset,rule) 最新一行，两次执行的 asset1 只算一行
    let (status, body) = get(&app, &format!("/api/compliance/results?packId={id}")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["total"], 2, "缺省仍按最新行：asset1 + asset2 各一行");
}
