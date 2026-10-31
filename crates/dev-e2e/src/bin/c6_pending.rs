//! C6 验收工具：在**真图库**上用一个严格包跑一遍真合规编排，产出真实的 `pending_human`
//! 判定，供 C6 审核队列/误判反馈/库视图过滤在真数据上验收。
//!
//! 为什么需要它：内置电商包在真图 e2e 里机器处理率 100%（人工队列 0）——那正是 C5 的成绩，
//! 但也意味着 C6 的"待人工队列"在真数据上没有素材。本工具换一个 classic-only 的严格包
//! （阈值高到必然 fail 且不带修复器 → 转人工），用**同一套编排器/同一套落库路径**跑真图，
//! 于是队列里的卡片是真判定、真图片、真证据，而不是测试夹具。
//!
//! 只跑 classic 通道（不需要 VL/模型），确定性、秒级完成。
//!
//! 环境变量：
//!   C6_DB=/tmp/c6-webdata/dam.db        真图库路径（必填）
//!   C6_PACK_DIR=/workspace/scripts/c6-fixtures/c6-strict-pack  包目录（必填）
//!   C6_LIMIT=0                          仅处理前 N 张（0 = 全部）

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use core_storage::compliance::{ComplianceRepo, VerdictFilter};
use core_storage::compliance_sqlite::SqliteComplianceRepo;
use svc_pipeline::compliance_queue::{compliance_enq, run_compliance_queue};

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| d.to_string())
}

fn main() {
    let db = PathBuf::from(env_or("C6_DB", "/tmp/c6-webdata/dam.db"));
    let pack_dir = PathBuf::from(env_or("C6_PACK_DIR", ""));
    let limit: usize = env_or("C6_LIMIT", "0").parse().unwrap_or(0);

    let pack = match core_config::Pack::from_dir(&pack_dir) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "C6_PENDING_RESULT FAIL pack={} reason={e}",
                pack_dir.display()
            );
            std::process::exit(1);
        }
    };
    let conn = match core_storage::open(&db) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("C6_PENDING_RESULT FAIL db={} reason={e}", db.display());
            std::process::exit(1);
        }
    };
    // 本工具与 dam-web 同时开着真图库（Web 在读/偶发写 settings）——WAL 下并发写会立刻
    // SQLITE_BUSY（core_storage::open 未设 busy_timeout），这里给工具侧一个等待窗口。
    let _ = conn.busy_timeout(std::time::Duration::from_secs(15));
    let mut ids: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT assetId FROM assets WHERE deleted = 0 ORDER BY assetId")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    if limit > 0 {
        ids.truncate(limit);
    }
    let enqed = match compliance_enq(&conn, &ids, pack.id.as_str(), None) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("C6_PENDING_RESULT FAIL enq reason={e}");
            std::process::exit(1);
        }
    };
    let cancel = AtomicBool::new(false);
    let report = match run_compliance_queue(&conn, &pack, &cancel, &mut |_| {}) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("C6_PENDING_RESULT FAIL run reason={e}");
            std::process::exit(1);
        }
    };

    // 落库自检：按 result 分桶（真判定）
    let repo = SqliteComplianceRepo::new(&conn);
    let rows = repo.find_verdicts(&VerdictFilter::pack(&pack.id)).unwrap();
    let mut by_result: std::collections::BTreeMap<String, i64> = Default::default();
    let mut assets_with_pending: std::collections::BTreeSet<i64> = Default::default();
    for r in &rows {
        *by_result.entry(r.result.clone()).or_default() += 1;
        if r.result == "pending_human" {
            assets_with_pending.insert(r.asset_id);
        }
    }
    println!(
        "C6_PENDING_RESULT {} assets={} enqueued={} done={} failed={} verdicts={} by_result={:?} assets_with_pending={}",
        if report.failed == 0 && report.done == ids.len() { "OK" } else { "PARTIAL" },
        ids.len(),
        enqed,
        report.done,
        report.failed,
        rows.len(),
        by_result,
        assets_with_pending.len(),
    );
}
