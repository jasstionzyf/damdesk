//! [`ComplianceRunRepo`] 的 SQLite 实现（存储解耦裁决：方言写法只允许出现在本模块）。
//!
//! 可移植性说明（PG 并行维护约定）：
//! - 时间戳 `unixepoch()`（PG 侧对应 extract(epoch from now())，届时换方言文件）；
//! - schema 类型全部可移植（INTEGER 主键 / TEXT / 整型时间戳），见 migration 0018；
//! - 「按状态汇总」用 `SUM(status = 'done')`（SQLite 布尔即 0/1；PG 侧写
//!   `COUNT(*) FILTER (WHERE status='done')`，届时只改本文件）。

use rusqlite::{params_from_iter, Connection};

use crate::compliance_runs::{
    run_status, ComplianceRunRepo, RunCounts, RunRow, TaskRow,
};
use crate::{Result, StorageError};

/// SQLite 连接上的 run 账本仓库。
pub struct SqliteComplianceRunRepo<'a> {
    conn: &'a Connection,
}

impl<'a> SqliteComplianceRunRepo<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }
}

/// `?` 占位符串（配合 params_from_iter 传 kind 列表）。
fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

const RUN_COLS: &str = "runId, kind, status, startedAt, finishedAt, total, done, failed, \
                        skipped, cancelled, error, params";

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        run_id: r.get(0)?,
        kind: r.get(1)?,
        status: r.get(2)?,
        started_at: r.get(3)?,
        finished_at: r.get(4)?,
        total: r.get(5)?,
        done: r.get(6)?,
        failed: r.get(7)?,
        skipped: r.get(8)?,
        cancelled: r.get(9)?,
        error: r.get(10)?,
        params: r.get(11)?,
    })
}

impl ComplianceRunRepo for SqliteComplianceRunRepo<'_> {
    fn start(&self, kind: &str, params: Option<&str>) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO compliance_runs (kind, status, startedAt, params)
             VALUES (?1, ?2, unixepoch(), ?3)",
            rusqlite::params![kind, run_status::RUNNING, params],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    fn adopt_pending(&self, run_id: i64, kinds: &[&str]) -> Result<usize> {
        if kinds.is_empty() {
            return Ok(0);
        }
        let sql = format!(
            "UPDATE job_queue SET runId = ?1, updatedAt = unixepoch()
             WHERE kind IN ({})
               AND status IN ('pending','running')
               AND runId IS NOT NULL AND runId <> ?1
               AND runId IN (SELECT runId FROM compliance_runs WHERE status <> 'running')",
            placeholders(kinds.len())
        );
        let mut vals: Vec<rusqlite::types::Value> = vec![run_id.into()];
        vals.extend(kinds.iter().map(|k| rusqlite::types::Value::Text((*k).to_string())));
        Ok(self.conn.execute(&sql, params_from_iter(vals))?)
    }

    fn note_skipped(&self, run_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE compliance_runs SET skipped = skipped + 1 WHERE runId = ?1",
            [run_id],
        )?;
        Ok(())
    }

    fn finish(
        &self,
        run_id: i64,
        status: &str,
        error: Option<&str>,
        kinds: &[&str],
    ) -> Result<RunRow> {
        let row = self
            .get(run_id)?
            .ok_or_else(|| StorageError::Other(format!("run{run_id} 不存在")))?;
        let counts = self.counts(run_id, kinds)?;
        // skipped 只可能来自计时器（job_queue 里与 done 同终态）；done = done行数 − skipped
        let solved = counts.with_skipped(row.skipped);
        self.conn.execute(
            "UPDATE compliance_runs
             SET status = ?2, finishedAt = unixepoch(),
                 total = ?3, done = ?4, failed = ?5, skipped = ?6, cancelled = ?7,
                 error = COALESCE(?8, error)
             WHERE runId = ?1",
            rusqlite::params![
                run_id,
                status,
                solved.total,
                solved.done,
                solved.failed,
                solved.skipped,
                solved.cancelled(),
                error,
            ],
        )?;
        self.get(run_id)?
            .ok_or_else(|| StorageError::Other(format!("run{run_id} 收工后读不回")))
    }

    fn get(&self, run_id: i64) -> Result<Option<RunRow>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {RUN_COLS} FROM compliance_runs WHERE runId = ?1"))?;
        let mut rows = stmt.query_map([run_id], row_from)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    fn latest(&self) -> Result<Option<RunRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {RUN_COLS} FROM compliance_runs ORDER BY startedAt DESC, runId DESC LIMIT 1"
        ))?;
        let mut rows = stmt.query_map([], row_from)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    fn running(&self) -> Result<Option<RunRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {RUN_COLS} FROM compliance_runs WHERE status = ?1
             ORDER BY startedAt DESC, runId DESC LIMIT 1"
        ))?;
        let mut rows = stmt.query_map([run_status::RUNNING], row_from)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    fn list(&self, limit: i64, offset: i64) -> Result<(i64, Vec<RunRow>)> {
        let total: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM compliance_runs", [], |r| r.get(0))?;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {RUN_COLS} FROM compliance_runs
             ORDER BY startedAt DESC, runId DESC LIMIT ?1 OFFSET ?2"
        ))?;
        let rows = stmt
            .query_map([limit.max(1), offset.max(0)], row_from)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok((total, rows))
    }

    fn tasks(&self, run_id: i64, limit: i64, offset: i64) -> Result<(i64, Vec<TaskRow>)> {
        let total: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM job_queue WHERE runId = ?1",
            [run_id],
            |r| r.get(0),
        )?;
        let mut stmt = self.conn.prepare(
            "SELECT id, runId, kind, targetId, status, attempts, lastError, updatedAt, payload
             FROM job_queue WHERE runId = ?1 ORDER BY id LIMIT ?2 OFFSET ?3",
        )?;
        let rows = stmt
            .query_map([run_id, limit.max(1), offset.max(0)], |r| {
                Ok(TaskRow {
                    job_id: r.get(0)?,
                    run_id: r.get(1)?,
                    kind: r.get(2)?,
                    target_id: r.get(3)?,
                    status: r.get(4)?,
                    attempts: r.get(5)?,
                    last_error: r.get(6)?,
                    updated_at: r.get(7)?,
                    payload: r.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok((total, rows))
    }

    fn counts(&self, run_id: i64, kinds: &[&str]) -> Result<RunCounts> {
        let mut sql = String::from(
            "SELECT COUNT(*),
                    COALESCE(SUM(status = 'done'), 0),
                    COALESCE(SUM(status = 'failed'), 0),
                    COALESCE(SUM(status IN ('pending','running')), 0)
             FROM job_queue WHERE runId = ?1",
        );
        let mut vals: Vec<rusqlite::types::Value> = vec![run_id.into()];
        if !kinds.is_empty() {
            sql.push_str(&format!(" AND kind IN ({})", placeholders(kinds.len())));
            vals.extend(kinds.iter().map(|k| rusqlite::types::Value::Text((*k).to_string())));
        }
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt.query_row(params_from_iter(vals), |r| {
            Ok(RunCounts {
                total: r.get(0)?,
                done: r.get(1)?,
                failed: r.get(2)?,
                pending: r.get(3)?,
                skipped: 0,
            })
        })?)
    }

    fn interrupt_stale(&self, kinds: &[&str]) -> Result<usize> {
        let mut stmt = self
            .conn
            .prepare("SELECT runId FROM compliance_runs WHERE status = ?1")?;
        let ids = stmt
            .query_map([run_status::RUNNING], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        let mut n = 0usize;
        for id in ids {
            self.finish(
                id,
                run_status::INTERRUPTED,
                Some("进程重启或崩溃：本次执行未收工（未执行的 task 留在队列里，可再次开始判定续跑）"),
                kinds,
            )?;
            n += 1;
        }
        Ok(n)
    }
}
