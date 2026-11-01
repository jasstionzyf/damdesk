//! DEC-F2（卡 `t_a4b7e0ee`）：skipped 判定行**不带分数与把握度**。
//!
//! 契约（t_9267ed7a 资产级口径 → 本卡落到规则行存储层）：`rule_verdict` 中
//! `result='skipped'` 的行必须 `confidence IS NULL` 且 `score IS NULL`
//! （NULL，不是 0，也不是 1.0）。写侧强制点 = `insert_verdict` 的 skipped
//! 拆列（0028 同一条 CASE 的延伸，唯一入口）；存量行由迁移 0037 回填，且
//! 迁移幂等（跑两次结果一致）。
use core_storage::compliance::ComplianceRepo;
use core_storage::compliance::NewVerdict;
use core_storage::compliance_sqlite::SqliteComplianceRepo;
use rusqlite::Connection;

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("t_a4b7e0ee-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn asset(c: &Connection, path: &str) -> i64 {
    c.execute(
        "INSERT INTO assets (assetId, absPath, mediaType, missing, fileName, fileExt, fileSz, mtime, importedAt)
         VALUES (?1, ?2, 'image', 0, ?3, 'jpg', 10, 0, ?1)",
        rusqlite::params![path_hash(path), path, file_name(path)],
    )
    .unwrap();
    c.last_insert_rowid()
}

fn path_hash(p: &str) -> i64 {
    p.bytes().map(|b| b as i64).sum::<i64>().max(1)
}

fn file_name(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

fn nv(asset_id: i64, rule: &str, result: &str, confidence: f64, score: Option<f64>) -> NewVerdict {
    NewVerdict {
        pack_id: "com.soujpg.pack.ecommerce".into(),
        asset_id,
        job_id: 21,
        rule_id: rule.into(),
        result: result.into(),
        confidence,
        score,
        evidence: Some("{\"k\":1}".into()),
        fix_action: None,
        fix_tier: None,
        recheck_result: None,
        judgement_scope: Some("per_asset".into()),
        group_key: None,
    }
}

/// 写入侧契约：skipped 行落库后 confidence/score 必须是 NULL；
/// 其余行（pass/fail/undetermined）的 confidence/score 原样保留。
#[test]
fn skipped_row_lands_null_confidence_and_score() {
    let dir = tmpdir("write");
    let c = core_storage::open(&dir.join("dam.db")).unwrap();
    let repo = SqliteComplianceRepo::new(&c);
    let a = asset(&c, "/x/a.jpg");

    repo.insert_verdict(&nv(a, "R_skip", "skipped", 1.0, Some(0.9)))
        .unwrap();
    repo.insert_verdict(&nv(a, "R_pass", "pass", 0.8, Some(0.42)))
        .unwrap();

    let row = |rule: &str| -> (String, Option<f64>, Option<f64>) {
        c.query_row(
            "SELECT result, confidence, score FROM rule_verdict WHERE ruleId = ?1",
            [rule],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    };

    let (result, confidence, score) = row("R_skip");
    assert_eq!(result, "skipped");
    assert!(confidence.is_none(), "skipped 行 confidence 必须 IS NULL（存量缺陷写 1.0）");
    assert!(score.is_none(), "skipped 行 score 必须 IS NULL");

    // 对照：有结论的行不受影响
    let (result, confidence, score) = row("R_pass");
    assert_eq!(result, "ok");
    assert_eq!(confidence, Some(0.8));
    assert_eq!(score, Some(0.42));
}

/// 迁移 0037：存量 skipped 行回填 NULL；幂等（跑两次结果一致）；非 skipped 行不动。
#[test]
fn migration_backfills_legacy_skipped_rows_idempotent() {
    let dir = tmpdir("mig");
    let db_path = dir.join("dam.db");
    let c = Connection::open(&db_path).unwrap();
    c.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;").unwrap();
    // 造「迁移前形态」：只建到 0036（无 0037）
    core_storage::migrations::migrate_upto(&c, 36).unwrap();
    let top36 = core_storage::migrations::current_version(&c).unwrap();
    assert!(top36 >= 36, "legacy 库至少建到 36，实际 {top36}");

    let a = asset(&c, "/x/legacy.jpg");
    // 存量形态（缺陷态）：skipped 行带 1.0 / 0.9
    for rule in ["R_skip_old", "R_skip_old2"] {
        c.execute(
            "INSERT INTO rule_verdict (packId, assetId, jobId, ruleId, result, verdict, confidence, score, ts)
             VALUES ('com.soujpg.pack.ecommerce', ?1, 7, ?2, 'skipped', NULL, 1.0, 0.9, 100)",
            rusqlite::params![a, rule],
        )
        .unwrap();
    }
    // 对照行：pass 不许被动
    c.execute(
        "INSERT INTO rule_verdict (packId, assetId, jobId, ruleId, result, verdict, confidence, score, ts)
         VALUES ('com.soujpg.pack.ecommerce', ?1, 7, 'R_pass_old', 'ok', 'pass', 0.66, 0.42, 100)",
        [a],
    )
    .unwrap();
    let ledger_before: i64 = c
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
        .unwrap();

    // 第一次 migrate：应用 0037（若已存在则跳过）
    core_storage::migrations::migrate(&c).unwrap();
    let snap = |c: &Connection| -> Vec<(String, Option<f64>, Option<f64>)> {
        let mut stmt = c
            .prepare("SELECT ruleId, confidence, score FROM rule_verdict ORDER BY ruleId")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let first = snap(&c);
    assert_eq!(first.len(), 3, "{first:?}");
    for (rule, confidence, score) in &first {
        if rule.starts_with("R_skip") {
            assert!(confidence.is_none(), "{rule}: 存量 skipped 行 confidence 必须回填 NULL，实际 {confidence:?}");
            assert!(score.is_none(), "{rule}: 存量 skipped 行 score 必须回填 NULL，实际 {score:?}");
        } else {
            assert_eq!(*score, Some(0.42), "pass 行不许被动：{rule}");
            assert_eq!(*confidence, Some(0.66), "pass 行不许被动：{rule}");
        }
    }

    // 第二次 migrate：幂等 —— 结果与第一次完全一致，账本不长行
    core_storage::migrations::migrate(&c).unwrap();
    let second = snap(&c);
    assert_eq!(first, second, "跑两次结果必须一致（幂等）");
    let ledger_after: i64 = c
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ledger_before + 1, ledger_after, "只允许长一条迁移记录");
}
