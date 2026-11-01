//! DEC-F1（卡 `t_92f2ab62`）：run 分布卡口径对齐 `state_of_verdicts_with`。
//!
//! 实锤（t_d84301f5 Mac 真机走查 run12）：`run_verdict_counts()` 把 `verdict IS NULL`
//! （= skipped 行）计入 undetermined —— 带 1 条 skipped（R5_attribute）的 pass 资产被
//! 分布卡错归「待人工看」（分布卡 0/2/8，资产行 fail2 pass6 undet2）。
//!
//! 正确口径（`svc_library::compliance_view::state_of_verdicts_with`，卡 t_9267ed7a）：
//! `skipped` = 该规则对这张图不适用 —— 既不是 undetermined、也不构成结论；
//! 混合 skipped+pass → Pass；**全 skipped** → Undetermined。一张图一个结论。
use axum::body::Body;
use axum::http::Request;
use core_storage::compliance::ComplianceRepo;
use rusqlite::Connection;
use std::path::PathBuf;
use tower::ServiceExt;

fn seed(dir: &PathBuf) -> Connection {
    let _ = std::fs::remove_file(dir.join("dam.db"));
    let conn = core_storage::open(&dir.join("dam.db")).unwrap();
    // 判定行有 FK → assets，补五张最小资产行（分布卡只读账本，不需要真文件）
    for id in 1..=5i64 {
        conn.execute(
            "INSERT INTO assets (assetId, absPath, mediaType, missing, fileName, fileExt, fileSz, mtime, fileHash, importedAt)
             VALUES (?1, ?2, 'image', 0, ?3, 'jpg', 9, 0, ?4, 1)",
            rusqlite::params![id, dir.join(format!("a{id}.jpg")).to_string_lossy(), format!("a{id}.jpg"), format!("hash-{id}")],
        )
        .unwrap();
    }
    for (run, started) in [(1i64, 1i64), (2, 3)] {
        conn.execute(
            "INSERT INTO compliance_runs (runId, kind, status, startedAt, finishedAt, total)
             VALUES (?1, 'compliance', 'done', ?2, ?2, 5)",
            rusqlite::params![run, started],
        )
        .unwrap();
    }
    // 队列行：run 1 五个资产，run 2 只重判资产 1（证明分布卡按 run 隔离）
    for (job_id, asset_id, run) in [
        (11i64, 1i64, 1i64),
        (12, 2, 1),
        (13, 3, 1),
        (14, 4, 1),
        (15, 5, 1),
        (21, 1, 2),
    ] {
        conn.execute(
            "INSERT INTO job_queue (id, kind, targetId, status, attempts, createdAt, updatedAt, runId)
             VALUES (?1, 'compliance', ?2, 'done', 0, 1, 1, ?3)",
            rusqlite::params![job_id, asset_id, run],
        )
        .unwrap();
    }
    let repo = core_storage::compliance_sqlite::SqliteComplianceRepo::new(&conn);
    let add = |job_id: i64, asset_id: i64, rule: &str, verdict: &str| {
        repo.insert_verdict(&core_storage::compliance::NewVerdict {
            pack_id: "com.soujpg.pack.ecommerce".into(),
            asset_id,
            job_id,
            rule_id: rule.into(),
            result: verdict.into(),
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
    };
    // run 1（真机 run12 的形态）：
    //   资产 1 = pass + skipped → 必须 Pass（缺陷版把它算进 undetermined —— F-1 本体）
    //   资产 2 = 全 skipped     → Undetermined（唯一该进「待人工看」的「没结论」形态）
    //   资产 3 = fail；资产 4 = pass；资产 5 = undetermined
    add(11, 1, "R1_size", "pass");
    add(11, 1, "R5_attribute", "skipped");
    add(12, 2, "R5_attribute", "skipped");
    add(12, 2, "R6_other", "skipped");
    add(13, 3, "R1_size", "fail");
    add(14, 4, "R1_size", "pass");
    add(15, 5, "R1_size", "undetermined");
    // run 2：资产 1 只有一条 pass 行 → 分布卡 pass=1
    add(21, 1, "R1_size", "pass");
    conn
}

fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("t92f2ab62-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
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

/// 分布卡（run 明细 `verdictCounts`）必须与同 run 资产行（L2 `states`）逐档相等。
#[tokio::test]
async fn run_distribution_card_agrees_with_asset_rows_on_skipped() {
    let dir = tmpdir("dist");
    let conn = seed(&dir);
    let state = app_web::test_support::state(dir.clone(), conn);
    let app = app_web::router_without_dist(state);

    // 分布卡：pass=2（资产 1 混合 skipped+pass + 资产 4）/ fail=1 / undetermined=2（资产 2 全 skipped + 资产 5）
    let (code, body) = get(&app, "/api/compliance/jobs/1").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["run"]["totalAssets"], 5, "{body}");
    let vc = &body["run"]["verdictCounts"];
    assert_eq!(
        vc["pass"], 2,
        "混合 skipped+pass 的资产必须计 Pass（skipped ≠ undetermined）: {vc}"
    );
    assert_eq!(vc["fail"], 1, "{vc}");
    assert_eq!(
        vc["undetermined"], 2,
        "只有「没有任何结论」（全 skipped / 判不了）的资产才计 undetermined: {vc}"
    );

    // F-1 判据：分布卡 == 同 run 资产行档位（L2 按生效结论分桶，svc-library 口径）
    let (code, l2) = get(&app, "/api/compliance/jobs/1/verdicts").await;
    assert_eq!(code, 200, "{l2}");
    assert_eq!(l2["total"], 5, "{l2}");
    assert_eq!(l2["states"]["pass"], 2, "{l2}");
    assert_eq!(l2["states"]["fail"], 1, "{l2}");
    assert_eq!(l2["states"]["undetermined"], 2, "{l2}");

    // run 隔离：run 2 里资产 1 只有一条 pass 行，不受 run 1 的 skipped 行影响
    let (code, body2) = get(&app, "/api/compliance/jobs/2").await;
    assert_eq!(code, 200, "{body2}");
    assert_eq!(body2["run"]["totalAssets"], 1, "{body2}");
    assert_eq!(body2["run"]["verdictCounts"]["pass"], 1, "{body2}");
}

/// DEC-CLOSURE（卡 `t_ad9f89b1`）容器实跑抓到的 F-1 残余缺口：
/// 人审改判后行级 `effectiveStatus` 已合成（= fail），分布卡 `verdictCounts` 却仍按
/// 纯机器行计数（undetermined）—— 同一 run 两处数字打架，违背 F-1 的验收不变量。
/// 修法：`run_verdict_counts` 与行级同源（`run_asset_statuses`，含人审覆盖合成）。
#[tokio::test]
async fn run_distribution_card_merges_human_override() {
    let dir = tmpdir("override");
    let conn = seed(&dir);
    let state = app_web::test_support::state(dir.clone(), conn);
    let app = app_web::router_without_dist(state);

    // 改判前：资产 3 = 机器 fail → 分布卡 fail=1
    let (code, body) = get(&app, "/api/compliance/jobs/1").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["run"]["verdictCounts"]["fail"], 1, "{body}");

    // 人审改判资产 3 → pass（非低置信度 → 显式 force）
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/compliance/review")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"assetId":3,"humanStatus":"pass","reason":"人工确认合格","force":true}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 200, "改判必须成功");

    // 分布卡（verdictCounts）必须反映生效结论：fail 1→0，pass 2→3
    let (code, body2) = get(&app, "/api/compliance/jobs/1").await;
    assert_eq!(code, 200, "{body2}");
    let vc = &body2["run"]["verdictCounts"];
    assert_eq!(vc["fail"], 0, "改判后分布卡不得仍计机器 fail: {vc}");
    assert_eq!(vc["pass"], 3, "改判后分布卡必须计入人审 pass: {vc}");
    assert_eq!(vc["undetermined"], 2, "{vc}");

    // 与同 run 资产行（L2 states）逐档相等 —— F-1 不变量在改判场景下也成立
    // （行级 states 零档不落键：as_u64().unwrap_or(0)）
    let (code, l2) = get(&app, "/api/compliance/jobs/1/verdicts").await;
    assert_eq!(code, 200, "{l2}");
    assert_eq!(l2["states"]["fail"].as_u64().unwrap_or(0), 0, "{l2}");
    assert_eq!(l2["states"]["pass"].as_u64().unwrap_or(0), 3, "{l2}");
    assert_eq!(l2["states"]["undetermined"].as_u64().unwrap_or(0), 2, "{l2}");
}
