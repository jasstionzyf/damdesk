//! CMP-JOB-01（卡 t_6dc3aae4）回归钉：**一次批量审核 = 一个 run**，统计按 run 分隔。
//!
//! 用户裁决（原话语义）：一次批量图片审核跑完，就要能给这次出结果统计（成功/失败/跳过数），
//! **各次之间必须明显分开**；否则界面上「历史累计失败 30」与「本次失败 0」并排显示，
//! 用户会以为系统一直在坏。
//!
//! 本文件钉的五件事：
//! 1. run 账本口径：total = done + failed + skipped + cancelled（恒等式，`RunCounts::is_balanced`）；
//! 2. **统计只算本 run**：第二轮跑完不回头改写第一轮的行；
//! 3. **迁移前的老任务（runId IS NULL）不算进任何新 run**（BATCH-01 那 30 条坏图就属于这种）；
//! 4. 坏图（垃圾字节 `.jpg`）→ **skipped 且原因落库**，不再变成永久 failed；
//! 5. 取消后剩余的 task 会被下一轮**认领续跑**（跑了就要进那一轮的统计）；
//! 6. 进程死在中间 → 启动自愈把 run 收为 `interrupted`（不会永远 running）。

use std::path::Path;
use std::sync::atomic::AtomicBool;

use core_config::Pack;
use core_storage::compliance_runs::{run_status, ComplianceRunRepo, RunCounts};
use core_storage::compliance_runs_sqlite::SqliteComplianceRunRepo;
use rusqlite::Connection;
use svc_pipeline::compliance_queue::{
    compliance_enq, compliance_enq_in_run, run_compliance_queue_in_run,
    run_compliance_queue_with_limit_in_run, COMPLIANCE_KINDS, COMPLIANCE_KIND,
};

/// 1×1 真 PNG（与 custom_queue / tag_queue 测试同源字节）。
const PNG: [u8; 67] = [
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

/// 元数据档规则（不解码；确定性、无模型、无网络）。
const RULES: &str = r#"
- id: F0_domain
  name: 尺寸达标（元数据档）
  executor: classic
  evidence: image_only
  check: {detector: size_min, params: {min: 1}}
  fixer: null
  on_fail: pass
"#;
const PIPELINE: &str = "steps:\n  - {kind: judge, rule: F0_domain}\nrecheck_max: 0\n";

fn pack() -> Pack {
    Pack::from_yaml_strs(
        "cmp-job-01",
        "category: [x]\nscene: [y]\n",
        RULES,
        PIPELINE,
        "summary: [合格, 不合格, 未判定]\ndetail_columns: [imageSN, F0_domain]\n",
    )
    .unwrap()
}

fn db(dir: &Path) -> Connection {
    let conn = core_storage::open(&dir.join("cmp-job-01.db")).unwrap();
    conn
}

/// 造一张资产（`bad = true` → 垃圾字节 `.jpg`，模拟 BATCH-01 的坏图）。
fn seed_asset(conn: &Connection, dir: &Path, name: &str, bad: bool) -> i64 {
    let p = dir.join(name);
    if bad {
        std::fs::write(&p, b"this is definitely not a jpeg, just garbage bytes").unwrap();
    } else {
        std::fs::write(&p, PNG).unwrap();
    }
    conn.execute(
        "INSERT INTO assets (absPath, mediaType, missing, fileName, fileExt, fileSz, mtime, importedAt)
         VALUES (?1, 'image', 0, ?2, ?3, 10, 0, 0)",
        rusqlite::params![
            p.to_string_lossy(),
            name,
            p.extension().and_then(|e| e.to_str()).unwrap_or("jpg")
        ],
    )
    .unwrap();
    let id = conn.last_insert_rowid();
    // 元数据档检测器（size_min）读 primary resource 的宽高：给了才能真判出 pass
    conn.execute(
        "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1, 'primary', 0, 64, 64)",
        [id],
    )
    .unwrap();
    id
}

fn tmp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("cmp-job-01-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn balanced(run_id: i64, conn: &Connection) -> RunCounts {
    let repo = SqliteComplianceRunRepo::new(conn);
    let row = repo.get(run_id).unwrap().expect("run 行必须存在");
    let c = repo
        .counts(run_id, &COMPLIANCE_KINDS)
        .unwrap()
        .with_skipped(row.skipped);
    assert!(c.is_balanced(), "total 恒等式破了: {c:?}（run{run_id}）");
    c
}

/// 主钉：一次执行一行账；第二次执行不回头改写第一次的数字。
#[test]
fn each_run_keeps_its_own_result_stats() {
    let dir = tmp("per-run");
    let conn = db(&dir);
    let pack = pack();
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let out_s = out.to_string_lossy().to_string();

    // 迁移前的历史任务（runId IS NULL）—— 必须永远不算进任何新 run
    let legacy = seed_asset(&conn, &dir, "legacy.png", false);
    conn.execute(
        "INSERT INTO job_queue (kind, targetId, payload, status, attempts, lastError, createdAt, updatedAt)
         VALUES ('compliance', ?1, '{}', 'failed', 3, '历史坏图（迁移前）', 0, 0)",
        [legacy],
    )
    .unwrap();

    let a = seed_asset(&conn, &dir, "a.png", false);
    let b = seed_asset(&conn, &dir, "b.png", false);
    let repo = SqliteComplianceRunRepo::new(&conn);
    let cancel = AtomicBool::new(false);

    // ---- 第一轮：只判 a
    let run1 = repo.start(COMPLIANCE_KIND, Some("{\"round\":1}")).unwrap();
    assert_eq!(
        compliance_enq_in_run(&conn, &[a], &pack.id, Some(&out_s), Some(run1)).unwrap(),
        1
    );
    let rep1 = run_compliance_queue_in_run(&conn, &pack, &cancel, &mut |_| {}, Some(run1)).unwrap();
    assert_eq!(rep1.done, 1, "{rep1:?}");
    let row1 = repo
        .finish(run1, run_status::DONE, None, &COMPLIANCE_KINDS)
        .unwrap();
    assert_eq!(
        (row1.total, row1.done, row1.failed, row1.skipped, row1.cancelled),
        (1, 1, 0, 0, 0),
        "第一轮只该有 1 条任务（历史 NULL 行不算进来）: {row1:?}"
    );
    assert!(row1.finished_at.is_some(), "收工必须落戳: {row1:?}");
    let c1 = balanced(run1, &conn);
    assert_eq!((c1.total, c1.done, c1.failed, c1.skipped), (1, 1, 0, 0));

    // ---- 第二轮：只判 b
    let run2 = repo.start(COMPLIANCE_KIND, Some("{\"round\":2}")).unwrap();
    assert_eq!(
        compliance_enq_in_run(&conn, &[b], &pack.id, Some(&out_s), Some(run2)).unwrap(),
        1
    );
    run_compliance_queue_in_run(&conn, &pack, &cancel, &mut |_| {}, Some(run2)).unwrap();
    let row2 = repo
        .finish(run2, run_status::DONE, None, &COMPLIANCE_KINDS)
        .unwrap();
    assert_eq!(
        (row2.total, row2.done, row2.failed, row2.skipped, row2.cancelled),
        (1, 1, 0, 0, 0),
        "第二轮的数字只该算自己: {row2:?}"
    );

    // ---- 第一轮的行丝毫不动（历史互不改写）
    let row1_again = repo.get(run1).unwrap().unwrap();
    assert_eq!(row1_again, row1, "第二轮把第一轮的账改了");

    // ---- 列表按时间倒序，两行互不叠加
    let (total, rows) = repo.list(10, 0).unwrap();
    assert_eq!(total, 2);
    assert_eq!(
        rows.iter().map(|r| r.run_id).collect::<Vec<_>>(),
        vec![run2, run1]
    );
    assert_eq!(rows.iter().map(|r| r.done).sum::<i64>(), 2);

    // ---- 那次不带 run 的历史入队（dev-e2e 口径）依然落 NULL，不进任何 run
    let c = seed_asset(&conn, &dir, "c.png", false);
    compliance_enq(&conn, &[c], &pack.id, Some(&out_s)).unwrap();
    let null_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM job_queue WHERE runId IS NULL AND kind = 'compliance'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(null_rows, 2, "历史行 + 本次不带 run 的入队 = 2 条 NULL");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 坏图（垃圾字节 `.jpg`）→ skipped 且原因落库，**不是** failed。
#[test]
fn garbage_image_is_skipped_with_reason_not_failed() {
    let dir = tmp("bad-image");
    let conn = db(&dir);
    let pack = pack();
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let out_s = out.to_string_lossy().to_string();

    let good = seed_asset(&conn, &dir, "good.png", false);
    let bad = seed_asset(&conn, &dir, "bad.jpg", true);
    let repo = SqliteComplianceRunRepo::new(&conn);
    let cancel = AtomicBool::new(false);

    let run = repo.start(COMPLIANCE_KIND, None).unwrap();
    compliance_enq_in_run(&conn, &[good, bad], &pack.id, Some(&out_s), Some(run)).unwrap();
    let report = run_compliance_queue_in_run(&conn, &pack, &cancel, &mut |_| {}, Some(run)).unwrap();
    assert_eq!(report.done, 1, "{report:?}");
    assert_eq!(report.skipped, 1, "坏图应记跳过: {report:?}");
    assert_eq!(report.failed, 0, "坏图不该变成失败: {report:?}");
    let row = repo
        .finish(run, run_status::DONE, None, &COMPLIANCE_KINDS)
        .unwrap();
    assert_eq!(
        (row.total, row.done, row.failed, row.skipped),
        (2, 1, 0, 1),
        "{row:?}"
    );
    balanced(run, &conn);

    // task 明细：坏图那条是 done + 原因（界面据此解释「为什么这张没结论」）
    let (_, tasks) = repo.tasks(run, 10, 0).unwrap();
    let bad_task = tasks
        .iter()
        .find(|t| t.target_id == bad)
        .expect("坏图的 task 必须在");
    assert_eq!(bad_task.status, "done", "跳过与成功同终态: {bad_task:?}");
    let reason = bad_task.last_error.clone().unwrap_or_default();
    assert!(
        reason.starts_with("not_image:"),
        "跳过原因要落库（界面可解释）: {reason}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 取消后剩余的 task 留在 pending；下一轮**认领续跑**，跑了的必须进那一轮的统计。
#[test]
fn cancelled_run_leaves_pending_and_next_run_adopts_them() {
    let dir = tmp("cancel-adopt");
    let conn = db(&dir);
    let pack = pack();
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let out_s = out.to_string_lossy().to_string();

    let ids: Vec<i64> = (0..3)
        .map(|i| seed_asset(&conn, &dir, &format!("c{i}.png"), false))
        .collect();
    let repo = SqliteComplianceRunRepo::new(&conn);
    let cancel = AtomicBool::new(false);

    // ---- 第一轮：只跑 1 条就取消（limit=1 等价于用户点了取消）
    let run1 = repo.start(COMPLIANCE_KIND, None).unwrap();
    compliance_enq_in_run(&conn, &ids, &pack.id, Some(&out_s), Some(run1)).unwrap();
    run_compliance_queue_with_limit_in_run(&conn, &pack, &cancel, &mut |_| {}, 1, Some(run1))
        .unwrap();
    let row1 = repo
        .finish(run1, run_status::CANCELLED, None, &COMPLIANCE_KINDS)
        .unwrap();
    assert_eq!(
        (row1.total, row1.done, row1.failed, row1.cancelled),
        (3, 1, 0, 2),
        "取消后未执行的 2 条要算 cancelled（留在队列）: {row1:?}"
    );
    balanced(run1, &conn);

    // ---- 第二轮：认领上一轮遗留的 2 条 pending
    let run2 = repo.start(COMPLIANCE_KIND, None).unwrap();
    let adopted = repo.adopt_pending(run2, &COMPLIANCE_KINDS).unwrap();
    assert_eq!(adopted, 2, "上一轮遗留的 pending 应被认领");
    assert_eq!(
        compliance_enq_in_run(&conn, &ids, &pack.id, Some(&out_s), Some(run2)).unwrap(),
        1,
        "已 pending/running 的不会重复入队；只有上一轮已判完的那 1 张会重新入队"
    );
    let rep2 = run_compliance_queue_in_run(&conn, &pack, &cancel, &mut |_| {}, Some(run2)).unwrap();
    assert_eq!(rep2.done, 3, "{rep2:?}");
    let row2 = repo
        .finish(run2, run_status::DONE, None, &COMPLIANCE_KINDS)
        .unwrap();
    assert_eq!(
        (row2.total, row2.done, row2.cancelled),
        (3, 3, 0),
        "认领的 2 张 + 重新入队的 1 张都要进本轮的账: {row2:?}"
    );
    // 第一轮的行仍然是取消时的快照
    assert_eq!(repo.get(run1).unwrap().unwrap(), row1);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 进程死在中间：启动自愈把 running 收为 interrupted（界面不会永远显示在跑）。
#[test]
fn stale_running_run_is_finalized_as_interrupted() {
    let dir = tmp("stale");
    let conn = db(&dir);
    let pack = pack();
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let out_s = out.to_string_lossy().to_string();

    let a = seed_asset(&conn, &dir, "a.png", false);
    let repo = SqliteComplianceRunRepo::new(&conn);
    let run = repo.start(COMPLIANCE_KIND, None).unwrap();
    compliance_enq_in_run(&conn, &[a], &pack.id, Some(&out_s), Some(run)).unwrap();
    assert!(repo.running().unwrap().is_some(), "跑之前应为 running");

    assert_eq!(repo.interrupt_stale(&COMPLIANCE_KINDS).unwrap(), 1);
    let row = repo.get(run).unwrap().unwrap();
    assert_eq!(row.status, run_status::INTERRUPTED);
    assert!(row.finished_at.is_some());
    assert!(row.error.is_some(), "要能解释为什么中断: {row:?}");
    assert_eq!((row.total, row.cancelled), (1, 1), "未执行算 cancelled: {row:?}");
    balanced(run, &conn);
    assert!(repo.running().unwrap().is_none(), "自愈后不该还有 running");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 迁移对已有库跑两次不报错（升级路径：老库已有判定/任务数据）。
#[test]
fn migration_runs_twice_on_existing_db() {
    let dir = tmp("migrate");
    let db_path = dir.join("old.db");
    {
        let c = core_storage::open(&db_path).unwrap();
        c.execute(
            "INSERT INTO job_queue (kind, targetId, payload, status, createdAt, updatedAt)
             VALUES ('compliance', 1, '{}', 'done', 0, 0)",
            [],
        )
        .unwrap();
    }
    // 第二次 open = 再跑一遍账本（0001..0018 全部已应用 → 必须静默跳过）
    let c = core_storage::open(&db_path).unwrap();
    let v: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE version = 18",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(v, 1, "0018 只能应用一次");
    // 老行 runId 保持 NULL（不回填）
    let (kind, run_id): (String, Option<i64>) = c
        .query_row(
            "SELECT kind, runId FROM job_queue ORDER BY id LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(kind, "compliance");
    assert_eq!(run_id, None, "迁移前老行必须保持 NULL");

    let _ = std::fs::remove_dir_all(&dir);
}

/// CMP-JOB-FIX-1（卡 t_f737018e）：**资产状态分支的跳过也必须带原因**。
///
/// 事故形态（真机 E2E run6）：一张资产被 watcher 打了 `missing=1`，`process_job` 走
/// 「资产状态」分支直接 `Skipped`，既不写原因、也不清上一轮的失败原文 →
/// run 说「跳过 3」、明细只有 2 条带原因，第 3 条渲染成「已完成 + 失败原因」。
///
/// 本测试钉三件事：
/// 1. `run.skipped == 明细里带跳过前缀的 task 数`（不变量，界面靠它自洽）；
/// 2. 失联资产的原因前缀是 `file_missing:`，且**覆盖**旧失败原文（不留假故障）；
/// 3. 判定成功的 task 不再挂着上一轮的失败原文（`lastError` 清空）。
#[test]
fn asset_state_skips_carry_reason_and_overwrite_stale_error() {
    let dir = tmp("state-skip");
    let conn = db(&dir);
    let pack = pack();
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let out_s = out.to_string_lossy().to_string();

    let good = seed_asset(&conn, &dir, "good.png", false);
    let gone = seed_asset(&conn, &dir, "gone.png", false);
    // watcher 的口径：文件没了 → DB 打 missing=1（资产行仍在，任务仍会入队）
    std::fs::remove_file(dir.join("gone.png")).unwrap();
    conn.execute("UPDATE assets SET missing = 1 WHERE assetId = ?1", [gone])
        .unwrap();

    let repo = SqliteComplianceRunRepo::new(&conn);
    let cancel = AtomicBool::new(false);
    let run = repo.start(COMPLIANCE_KIND, None).unwrap();
    compliance_enq_in_run(&conn, &[good, gone], &pack.id, Some(&out_s), Some(run)).unwrap();
    // 上一轮残留的失败原文（rule 级隔离失败会写在这里；跳过时必须被盖掉）
    let stale = "compliance: rule R0_product_main vl: vl http: io: No such file or directory";
    conn.execute(
        "UPDATE job_queue SET lastError = ?2 WHERE runId = ?1",
        rusqlite::params![run, stale],
    )
    .unwrap();

    let report = run_compliance_queue_in_run(&conn, &pack, &cancel, &mut |_| {}, Some(run)).unwrap();
    assert_eq!(report.done, 1, "好图照常判完: {report:?}");
    assert_eq!(
        report.skipped, 1,
        "失联资产算跳过（不是失败 / 不是成功）: {report:?}"
    );
    assert_eq!(report.failed, 0, "{report:?}");

    let row = repo
        .finish(run, run_status::DONE, None, &COMPLIANCE_KINDS)
        .unwrap();
    assert_eq!(
        (row.total, row.done, row.failed, row.skipped),
        (2, 1, 0, 1),
        "{row:?}"
    );
    balanced(run, &conn);

    let (_, tasks) = repo.tasks(run, 10, 0).unwrap();
    let gone_task = tasks
        .iter()
        .find(|t| t.target_id == gone)
        .expect("失联资产的 task 必须在");
    assert_eq!(gone_task.status, "done", "跳过与成功同终态: {gone_task:?}");
    let reason = gone_task.last_error.clone().unwrap_or_default();
    assert!(
        reason.starts_with("file_missing:"),
        "失联资产跳过必须写原因（界面要能解释「为什么这张没结论」）: {reason:?}"
    );
    assert!(
        !reason.contains("R0_product_main"),
        "跳过原因要覆盖上一轮的失败原文，不能留着假故障: {reason:?}"
    );

    // 判成功的那条不留旧原文（否则「已完成 + 失败原因」）
    let good_task = tasks
        .iter()
        .find(|t| t.target_id == good)
        .expect("好图的 task 必须在");
    assert_eq!(
        good_task.last_error, None,
        "重试成功后不该还挂着上一轮的失败原文: {good_task:?}"
    );

    // 不变量：run.skipped == 明细里带跳过前缀的 task 数（界面抽屉据此自洽）
    let reasoned = tasks
        .iter()
        .filter(|t| {
            t.last_error
                .as_deref()
                .map(svc_pipeline::skip_reason::is_skip)
                .unwrap_or(false)
        })
        .count();
    assert_eq!(
        row.skipped as usize, reasoned,
        "run.skipped 与明细里带跳过原因的条数对不上（run={} 明细={reasoned}）",
        row.skipped
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// CMP-JOB-FIX-1：已删除 / 非图片媒体类型 也各有自己的原因前缀（不是一律 `not_image:`）。
#[test]
fn deleted_and_non_image_media_report_their_own_reason() {
    let dir = tmp("state-skip-cards");
    let conn = db(&dir);
    let pack = pack();
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let out_s = out.to_string_lossy().to_string();

    let del = seed_asset(&conn, &dir, "del.png", false);
    let video = seed_asset(&conn, &dir, "clip.mp4", false);
    conn.execute("UPDATE assets SET deleted = 1 WHERE assetId = ?1", [del])
        .unwrap();
    conn.execute(
        "UPDATE assets SET mediaType = 'video' WHERE assetId = ?1",
        [video],
    )
    .unwrap();

    let repo = SqliteComplianceRunRepo::new(&conn);
    let cancel = AtomicBool::new(false);
    let run = repo.start(COMPLIANCE_KIND, None).unwrap();
    compliance_enq_in_run(&conn, &[del, video], &pack.id, Some(&out_s), Some(run)).unwrap();
    let report = run_compliance_queue_in_run(&conn, &pack, &cancel, &mut |_| {}, Some(run)).unwrap();
    assert_eq!(report.skipped, 2, "{report:?}");
    assert_eq!(report.done, 0, "{report:?}");
    let row = repo
        .finish(run, run_status::DONE, None, &COMPLIANCE_KINDS)
        .unwrap();
    assert_eq!(
        (row.total, row.done, row.failed, row.skipped),
        (2, 0, 0, 2),
        "{row:?}"
    );

    let (_, tasks) = repo.tasks(run, 10, 0).unwrap();
    let reason_of = |id: i64| -> String {
        tasks
            .iter()
            .find(|t| t.target_id == id)
            .and_then(|t| t.last_error.clone())
            .unwrap_or_default()
    };
    assert!(
        reason_of(del).starts_with("deleted:"),
        "已删除资产的原因前缀: {:?}",
        reason_of(del)
    );
    let v = reason_of(video);
    assert!(
        v.starts_with("not_image_media:"),
        "非图片媒体的原因前缀: {v:?}"
    );
    assert!(v.contains("video"), "原因里要能看见真实媒体类型: {v:?}");

    let _ = std::fs::remove_dir_all(&dir);
}
