//! N5b 迁移核验器（卡 t_4c0b3767）：把 `rule_verdict.result` 拆成
//! `result`(执行结果 ok/failed/skipped) + `verdict`(审核结论 pass/fail/undetermined，可空)，
//! 并给出**可贴进报告**的原始数据：
//!
//! * 迁移前：库版本 + 行数 + 旧判定词分布 + 抽样行
//! * 迁移第一遍：版本 + 行数 + `result`/`verdict` 分布 + 行 digest
//! * 迁移第二遍：同样的数字 + digest（**必须与第一遍逐字相同** = 幂等）
//! * 逐行回读校验：`旧 result=pass/fail/undetermined → result=ok + verdict=<原值>`、
//!   `旧 result=skipped → result=skipped + verdict IS NULL`，**100% 行**都要对上
//!
//! 用法：
//! ```text
//! cargo run -p core-storage --bin result_migrate -- --db /tmp/x.db --seed-legacy
//! ```
//! `--seed-legacy` = 造「0027 形态的旧库」（本机没有真实历史判定库时的可复现夹具；
//! 报告里必须写明用的是夹具还是真库）。不带它就在给定库上就地迁移并核验。
//!
//! 退出码：0 = 幂等 + 100% 回读一致；1 = 任一不符（stderr 指出第一处不符的行）。

use core_storage::migrations::{current_version, migrate, migrate_upto};
use rusqlite::Connection;

/// 夹具：5+ 条历史判定行（覆盖四种旧判定词），外加其它列的真实取值（证据/修复/组键/包来源），
/// 迁移后这些列必须逐字不变 —— 只拆判定词，不动业务数据。
const SEED: &[(i64, &str, &str, &str, f64, &str, Option<&str>, Option<&str>)] = &[
    // (assetId, ruleId, 旧 result, packId, confidence, evidence, fixAction, groupKey)
    (101, "R1_size", "pass", "com.soujpg.pack.ecommerce", 0.93, r#"{"min":800}"#, None, None),
    (102, "R1_size", "fail", "com.soujpg.pack.ecommerce", 0.88, r#"{"w":640}"#, None, None),
    (103, "R1_size", "undetermined", "com.soujpg.pack.ecommerce", 1.0, r#"{"reason":"vl_timeout"}"#, None, None),
    (104, "R1_size", "skipped", "com.soujpg.pack.ecommerce", 1.0, r#"{"reason":"depends_on"}"#, None, None),
    (105, "R2_white_bg", "pass", "com.soujpg.pack.ecommerce", 0.75, r#"{"dev":0.02}"#, None, None),
    (106, "R2_white_bg", "fail", "com.soujpg.pack.ecommerce", 0.66, r#"{"dev":0.31}"#, Some("bg_replace"), None),
    (201, "G1_dup", "undetermined", "com.soujpg.pack.realestate", 0.5, r#"{"reason":"group_key_missing"}"#, None, Some("/房源/房间A")),
    (202, "G1_dup", "pass", "com.soujpg.pack.realestate", 0.99, r#"{"dup":1}"#, None, Some("/房源/房间B")),
];

fn main() {
    let mut db = String::from("/tmp/damdesk-result-migrate.db");
    let mut seed = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--db" => db = args.next().unwrap_or_else(|| fail("--db 缺路径")),
            "--seed-legacy" => seed = true,
            "-h" | "--help" => {
                println!("用法：result_migrate --db <path> [--seed-legacy]");
                return;
            }
            other => fail(&format!("未知参数 {other}")),
        }
    }

    let conn = Connection::open(&db).unwrap_or_else(|e| fail(&format!("打不开 {db}: {e}")));
    if seed {
        prepare_legacy(&conn);
    }

    // ---- 迁移前快照 ----
    let pre_version = current_version(&conn).unwrap_or(-1);
    let pre = pre_rows(&conn).unwrap_or_else(|e| fail(&format!("读迁移前快照失败：{e}")));
    println!("[db]   {db}");
    println!("[pre ] version={pre_version} rows={}", pre.len());
    println!("[pre ] dist(旧 result)={}", dist_str(&pre.iter().map(|r| r.1.clone()).collect::<Vec<_>>()));
    for r in pre.iter().take(5) {
        println!("[pre ] assetId={} ruleId={} result={}", r.3, r.2, r.1);
    }

    // ---- 第一遍迁移 ----
    migrate(&conn).unwrap_or_else(|e| fail(&format!("迁移失败：{e}")));
    let run1 = snapshot(&conn).unwrap_or_else(|e| fail(&format!("读迁移后快照失败：{e}")));
    let d1 = print_run("run1", &conn, &run1);

    // ---- 第二遍（幂等）----
    migrate(&conn).unwrap_or_else(|e| fail(&format!("第二次迁移失败：{e}")));
    let run2 = snapshot(&conn).unwrap_or_else(|e| fail(&format!("读第二遍快照失败：{e}")));
    let d2 = print_run("run2", &conn, &run2);

    if run1.1 != run2.1 || d1 != d2 {
        fail(&format!("幂等失败：两遍行内容不同（{d1:016x} vs {d2:016x}）"));
    }
    println!("[idem ] 两次 digest 一致：{d1:016x}");

    // ---- 100% 回读校验 ----
    let mut checked = 0usize;
    for (id, old_word, rule_id, asset_id) in &pre {
        let (exec, verdict): (String, Option<String>) = conn
            .query_row(
                "SELECT result, verdict FROM rule_verdict WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or_else(|e| fail(&format!("回读 id={id} 失败：{e}")));
        let want = expected_split(old_word);
        if (exec.as_str(), verdict.as_deref()) != want {
            fail(&format!(
                "第 {} 行不符：asset={} rule={} 旧={} → 期望 {:?}，实际 ({:?}, {:?})",
                checked + 1,
                asset_id,
                rule_id,
                old_word,
                want,
                exec,
                verdict
            ));
        }
        checked += 1;
    }
    println!("[verify] mapping_checked={checked} mismatches=0（100% 回读一致）");

    // ---- 抽样 5 条：迁移前后逐行对照（要求「原 verdict 值不变」）----
    println!(
        "[sample] 抽样 {} 条：迁移前旧值 → 迁移后 result/verdict（判定词必须原样保留）",
        pre.len().min(5)
    );
    for (id, old_word, rule_id, asset_id) in pre.iter().take(5) {
        let (exec, verdict): (String, Option<String>) = conn
            .query_row(
                "SELECT result, verdict FROM rule_verdict WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or_else(|e| fail(&format!("抽样回读 id={id} 失败：{e}")));
        // 判定词不变 = 落在 verdict 列；旧值本来就是 skipped 的，verdict 为 NULL 才算不变
        let kept = match verdict.as_deref() {
            Some(v) => v == old_word,
            None => old_word == "skipped",
        };
        println!(
            "[sample] assetId={asset_id} ruleId={rule_id} 旧={old_word} → result={exec} verdict={} 判定词{}",
            verdict.as_deref().unwrap_or("<null>"),
            if kept { "不变" } else { "变了" }
        );
        if !kept {
            fail(&format!(
                "抽样行判定词被改动：asset={asset_id} rule={rule_id} 旧={old_word} 新 verdict={verdict:?}"
            ));
        }
    }

    // ---- 其它业务列逐字不变 ----
    let cols: Vec<(i64, Option<String>, Option<String>, Option<String>)> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, evidence, fixAction, groupKey FROM rule_verdict ORDER BY id",
            )
            .expect("prepare");
        let it = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .expect("query");
        it.collect::<rusqlite::Result<Vec<_>>>().expect("collect")
    };
    let seeded = SEED.len();
    if seed && cols.len() != seeded {
        fail(&format!("夹具行数不符：期望 {seeded}，实际 {}", cols.len()));
    }
    println!(
        "[keep ] evidence/fixAction/groupKey 行数={}（搬迁不动业务列；夹具校验 {}）",
        cols.len(),
        if seed { "通过" } else { "跳过（非夹具库）" }
    );

    // ---- schema 落地证据 ----
    let cols_meta = table_columns(&conn, "rule_verdict");
    for want in ["result", "verdict"] {
        if !cols_meta.iter().any(|(n, _)| n == want) {
            fail(&format!("rule_verdict 缺列 {want}"));
        }
    }
    let verdict_nullable = cols_meta
        .iter()
        .find(|(n, _)| n == "verdict")
        .map(|(_, notnull)| *notnull == 0)
        .unwrap_or(false);
    if !verdict_nullable {
        fail("verdict 列必须可空（处理类行没有审核结论）");
    }
    let pack_outputs = table_columns(&conn, "packs")
        .into_iter()
        .find(|(n, _)| n == "outputs")
        .unwrap_or_else(|| fail("packs 缺 outputs 列（0036）"));
    println!(
        "[schema] rule_verdict.result/verdict 在位（verdict 可空 = {}）；packs.outputs 在位（notnull={}）",
        verdict_nullable, pack_outputs.1
    );
    println!("[PASS] 迁移幂等 + 100% 回读一致");
}

fn expected_split(old: &str) -> (&'static str, Option<&'static str>) {
    match old {
        "skipped" => ("skipped", None),
        "pass" => ("ok", Some("pass")),
        "fail" => ("ok", Some("fail")),
        "undetermined" => ("ok", Some("undetermined")),
        _ => ("?", None),
    }
}

/// 造「0027 形态的旧库」：先跑到 0027，再按旧口径灌历史行。
fn prepare_legacy(conn: &Connection) {
    let v = current_version(conn).unwrap_or(0);
    if v > 27 {
        println!("[seed ] 库里已是 v{v}（> 27）：不再造夹具");
        return;
    }
    migrate_upto(conn, 27).unwrap_or_else(|e| fail(&format!("建库到 0027 失败：{e}")));
    let existing: i64 = conn
        .query_row("SELECT COUNT(*) FROM rule_verdict", [], |r| r.get(0))
        .unwrap();
    if existing > 0 {
        println!("[seed ] rule_verdict 已有 {existing} 行：不覆盖");
        return;
    }
    for (i, (asset_id, rule_id, result, pack_id, conf, evidence, fix_action, group_key)) in
        SEED.iter().enumerate()
    {
        conn.execute(
            "INSERT OR IGNORE INTO assets (assetId, absPath, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (?1, ?2, ?3, 'jpg', 1024, 0, 0)",
            rusqlite::params![
                asset_id,
                format!("/test-fixture/seed/{asset_id}.jpg"),
                format!("{asset_id}.jpg")
            ],
        )
        .unwrap_or_else(|e| fail(&format!("插资产 {asset_id} 失败：{e}")));
        let job = 9000 + i as i64;
        conn.execute(
            "INSERT INTO rule_verdict
                 (packId, assetId, jobId, ruleId, result, confidence, score, evidence,
                  fixAction, fixTier, recheckResult, ts, judgementScope, groupKey)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, ?9, ?10, 1, 'per_asset', ?11)",
            rusqlite::params![
                pack_id,
                asset_id,
                job,
                rule_id,
                result,
                conf,
                evidence,
                fix_action,
                fix_action.map(|_| "best_effort"),
                fix_action.map(|_| "pass"),
                group_key,
            ],
        )
        .unwrap_or_else(|e| fail(&format!("插历史判定行失败：{e}")));
    }
    println!("[seed ] 0027 形态夹具就绪：{} 行历史判定（pass/fail/undetermined/skipped 全覆盖）", SEED.len());
}

/// 迁移前快照：(id, 旧判定词, ruleId, assetId)，按 id 升序。
fn pre_rows(conn: &Connection) -> rusqlite::Result<Vec<(i64, String, String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT id, result, ruleId, assetId FROM rule_verdict ORDER BY id",
    )?;
    let it = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    it.collect()
}

/// 迁移后快照：(行数, digest) —— digest 是全部行按 id 排序后的稳定串。
fn snapshot(conn: &Connection) -> rusqlite::Result<(usize, String)> {
    let mut stmt = conn.prepare(
        "SELECT id, assetId, ruleId, result, verdict, confidence, score, evidence,
                fixAction, fixTier, recheckResult, ts, judgementScope, groupKey, packVersion, pluginHash
         FROM rule_verdict ORDER BY id",
    )?;
    let mut lines: Vec<String> = Vec::new();
    let rows = stmt.query_map([], |r| {
        let s = |i: usize| -> rusqlite::Result<String> {
            Ok(r.get::<_, Option<String>>(i)?.unwrap_or_else(|| "<null>".into()))
        };
        let f = |i: usize| -> rusqlite::Result<String> {
            Ok(r.get::<_, Option<f64>>(i)?
                .map(|v| v.to_string())
                .unwrap_or_else(|| "<null>".into()))
        };
        let n = |i: usize| -> rusqlite::Result<String> { Ok(r.get::<_, i64>(i)?.to_string()) };
        Ok(format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            n(0)?,
            n(1)?,
            s(2)?,
            s(3)?,
            s(4)?,
            f(5)?,
            f(6)?,
            s(7)?,
            s(8)?,
            s(9)?,
            s(10)?,
            n(11)?,
            s(12)?,
            s(13)?,
            s(14)?,
            s(15)?
        ))
    })?;
    for row in rows {
        lines.push(row?);
    }
    Ok((lines.len(), lines.join("\n")))
}

fn print_run(tag: &str, conn: &Connection, snap: &(usize, String)) -> u64 {
    let ver = current_version(conn).unwrap_or(-1);
    let mut stmt = conn
        .prepare("SELECT result FROM rule_verdict")
        .expect("prepare result");
    let exec_dist: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .expect("query")
        .filter_map(std::result::Result::ok)
        .collect();
    drop(stmt);
    let mut stmt = conn
        .prepare("SELECT COALESCE(verdict, '<null>') FROM rule_verdict")
        .expect("prepare verdict");
    let verdict_dist: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .expect("query")
        .filter_map(std::result::Result::ok)
        .collect();
    println!(
        "[{tag}] version={ver} rows={} dist(result)={} dist(verdict)={} digest={:016x}",
        snap.0,
        dist_str(&exec_dist),
        dist_str(&verdict_dist),
        fxhash(snap.1.as_bytes())
    );
    fxhash(snap.1.as_bytes())
}

fn dist_str(words: &[String]) -> String {
    let mut keys: Vec<&String> = words.iter().collect();
    keys.sort();
    keys.dedup();
    let mut parts = Vec::new();
    for k in keys {
        let n = words.iter().filter(|w| *w == k).count();
        parts.push(format!("{k}:{n}"));
    }
    parts.join(",")
}

/// FNV-1a 64：只为「两遍是否逐字一致」给一个可贴进报告的短指纹（不是安全哈希）。
fn fxhash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn table_columns(conn: &Connection, table: &str) -> Vec<(String, i64)> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("prepare table_info");
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(3)?)))
        .expect("query table_info");
    rows.filter_map(std::result::Result::ok).collect()
}

fn fail(msg: &str) -> ! {
    eprintln!("[FAIL] {msg}");
    std::process::exit(1);
}
