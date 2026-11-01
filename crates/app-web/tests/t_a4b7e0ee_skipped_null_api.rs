//! DEC-F2（卡 `t_a4b7e0ee`）验收 3：API `verdicts` 行 `confidence: null`。
//!
//! skipped 判定行在存储层落 NULL（写侧 `insert_verdict` 拆列 + 迁移 0037 回填）后，
//! 资产视图 `/api/compliance/verdicts` 的行序列化必须是字面 `null`——
//! 不是 0、不是 1.0。有结论行（pass）不受影响。
use axum::body::Body;
use axum::http::Request;
use core_storage::compliance::ComplianceRepo;
use rusqlite::Connection;
use std::path::PathBuf;
use tower::ServiceExt;

fn seed(dir: &PathBuf) -> Connection {
    let _ = std::fs::remove_file(dir.join("dam.db"));
    let conn = core_storage::open(&dir.join("dam.db")).unwrap();
    for (id, name) in [(1i64, "skip.jpg"), (2, "pass.jpg")] {
        conn.execute(
            "INSERT INTO assets (assetId, absPath, mediaType, missing, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (?1, ?2, 'image', 0, ?3, 'jpg', 10, 0, ?1)",
            rusqlite::params![id, dir.join(name).to_string_lossy(), name],
        )
        .unwrap();
    }
    let repo = core_storage::compliance_sqlite::SqliteComplianceRepo::new(&conn);
    let add = |asset_id: i64, rule: &str, result: &str, confidence: f64, score: Option<f64>| {
        repo.insert_verdict(&core_storage::compliance::NewVerdict {
            pack_id: "com.soujpg.pack.ecommerce".into(),
            asset_id,
            job_id: 1,
            rule_id: rule.into(),
            result: result.into(),
            confidence,
            score,
            evidence: Some("{\"reason\":\"x\"}".into()),
            fix_action: None,
            fix_tier: None,
            recheck_result: None,
            judgement_scope: Some("per_asset".into()),
            group_key: None,
        })
        .unwrap();
    };
    // 缺陷形态的写法（confidence=1.0）：修复后落库必须仍是 NULL
    add(1, "R_skip", "skipped", 1.0, Some(0.4));
    add(2, "R_pass", "pass", 0.8, Some(0.4));
    conn
}

async fn get_raw(app: &axum::Router, uri: &str) -> (u16, String) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let body = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

/// 资产视图 verdicts 行：skipped 行 `confidence`/`score` 字面 null；pass 行保留数值。
#[tokio::test]
async fn skipped_verdict_row_serializes_null_confidence() {
    let dir = std::env::temp_dir().join(format!("t_a4b7e0ee-api-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let conn = seed(&dir);
    let state = app_web::test_support::state(dir.clone(), conn);
    let app = app_web::router_without_dist(state);

    let (code, raw) = get_raw(&app, "/api/compliance/verdicts?assetId=1").await;
    assert_eq!(code, 200, "{raw}");
    let body: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let items = body["items"].as_array().expect("items 数组");
    let skip = items
        .iter()
        .find(|r| r["ruleId"] == "R_skip")
        .expect("skipped 行在列");
    assert_eq!(skip["result"], "skipped", "{skip}");
    println!("SKIPPED_ROW_JSON: {skip}");
    assert_eq!(
        skip["confidence"],
        serde_json::Value::Null,
        "skipped 行 confidence 必须序列化为 null：{skip}"
    );
    assert_eq!(skip["score"], serde_json::Value::Null, "{skip}");
    // 原始文本证据（验收要「贴原始 JSON 片段」的字面 null，不是缺字段）
    assert!(
        raw.contains("\"confidence\":null"),
        "响应原文必须含字面 null：{raw}"
    );

    // 对照：pass 资产行数值原样
    let (_, raw2) = get_raw(&app, "/api/compliance/verdicts?assetId=2").await;
    let body2: serde_json::Value = serde_json::from_str(&raw2).unwrap();
    let pass = body2["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["ruleId"] == "R_pass")
        .expect("pass 行在列");
    assert_eq!(pass["confidence"], 0.8, "{pass}");
    assert_eq!(pass["score"], 0.4, "{pass}");
}
