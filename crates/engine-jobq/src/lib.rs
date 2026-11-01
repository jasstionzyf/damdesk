//! L1 `engine-jobq` — 通用任务队列：pending→running→done/failed 状态机、
//! 重试上限、断点续跑（崩溃后 running 回置 pending）。不含 job 语义（D3/D5 由
//! svc-pipeline 赋予 embed/tag 语义）。

use rusqlite::Connection;

#[derive(Debug, thiserror::Error)]
pub enum JobqError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

pub type Result<T> = std::result::Result<T, JobqError>;

/// 队列状态常量（对齐 core-storage 迁移 0001 的 CHECK 约束）。
pub mod status {
    pub const PENDING: &str = "pending";
    pub const RUNNING: &str = "running";
    pub const DONE: &str = "done";
    pub const FAILED: &str = "failed";
}

/// 最大重试次数（超过转 failed）。
pub const MAX_ATTEMPTS: i32 = 3;

/// 入队（kind 由 L2 定义，如 "embed"/"tag"；targetId = resourceId 粒度）。
pub fn enqueue(
    conn: &Connection,
    kind: &str,
    target_id: i64,
    payload: Option<&str>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO job_queue (kind, targetId, payload, status, createdAt, updatedAt)
         VALUES (?1, ?2, ?3, 'pending', unixepoch(), unixepoch())",
        rusqlite::params![kind, target_id, payload],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 取一批 pending 任务置 running（断点续跑入口：先调 `requeue_stale`）。
pub fn claim_batch(
    conn: &Connection,
    kind: &str,
    limit: i64,
) -> Result<Vec<(i64, i64, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT id, targetId, payload FROM job_queue
         WHERE kind = ?1 AND status = 'pending'
         ORDER BY id LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![kind, limit], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for &(id, _, _) in &rows {
        conn.execute(
            "UPDATE job_queue SET status='running', updatedAt=unixepoch() WHERE id=?1",
            rusqlite::params![id],
        )?;
    }
    Ok(rows)
}

/// 任务成功。
pub fn complete(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(
        "UPDATE job_queue SET status='done', updatedAt=unixepoch() WHERE id=?1",
        rusqlite::params![id],
    )?;
    Ok(())
}

/// 任务失败：未超上限回 pending（重试），超限转 failed。
pub fn fail(conn: &Connection, id: i64, err: &str) -> Result<()> {
    conn.execute(
        "UPDATE job_queue
         SET attempts = attempts + 1,
             status = CASE WHEN attempts + 1 >= ?2 THEN 'failed' ELSE 'pending' END,
             lastError = ?3, updatedAt = unixepoch()
         WHERE id = ?1",
        rusqlite::params![id, MAX_ATTEMPTS, err],
    )?;
    Ok(())
}

/// 任务**确定性失败**：直接落 failed，不回 pending 重试。
///
/// 与 [`fail`] 的分界线是「重试有没有意义」：网络抖动/进程崩溃值得再试一次；
/// 而「插件说这次做不了」（目标超限、参数不合法）重试只会把同一句话再说 3 遍，
/// 把任务中心刷成噪音。调用方判定了「确定性」就用这个。
pub fn fail_terminal(conn: &Connection, id: i64, err: &str) -> Result<()> {
    conn.execute(
        "UPDATE job_queue
         SET attempts = attempts + 1, status = 'failed', lastError = ?2, updatedAt = unixepoch()
         WHERE id = ?1",
        rusqlite::params![id, err],
    )?;
    Ok(())
}

/// 断点续跑：进程崩溃残留的 running 全部回 pending。
pub fn requeue_stale(conn: &Connection) -> Result<usize> {
    let n = conn.execute(
        "UPDATE job_queue SET status='pending', updatedAt=unixepoch() WHERE status='running'",
        [],
    )?;
    Ok(n)
}

/// 统计（进度条用）。
pub fn stats(conn: &Connection, kind: &str) -> Result<(i64, i64, i64)> {
    // (done, failed, pending+running)
    let count = |s: &str| -> Result<i64> {
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM job_queue WHERE kind=?1 AND status=?2",
            rusqlite::params![kind, s],
            |r| r.get(0),
        )?)
    };
    let pending = count(status::PENDING)? + count(status::RUNNING)?;
    Ok((count(status::DONE)?, count(status::FAILED)?, pending))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        core_storage_migrations(&c);
        c
    }

    // core-storage 是 L0，engine-jobq 依赖它合法；但为了避免 crate 依赖，
    // 测试内联建最小 job_queue 表（与迁移 0001 相同 DDL）。
    fn core_storage_migrations(c: &Connection) {
        c.execute_batch(
            "CREATE TABLE job_queue (
                id INTEGER PRIMARY KEY, kind TEXT NOT NULL, targetId INTEGER NOT NULL,
                payload TEXT, status TEXT NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending','running','done','failed')),
                attempts INTEGER NOT NULL DEFAULT 0, lastError TEXT,
                createdAt INTEGER NOT NULL, updatedAt INTEGER NOT NULL);",
        )
        .unwrap();
    }

    #[test]
    fn lifecycle_with_retry() {
        let c = mem();
        let id = enqueue(&c, "embed", 42, None).unwrap();
        let batch = claim_batch(&c, "embed", 8).unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].0, id);
        fail(&c, id, "boom").unwrap(); // attempts=1 < 3 → pending
        assert_eq!(stats(&c, "embed").unwrap().2, 1);
        claim_batch(&c, "embed", 8).unwrap();
        fail(&c, id, "boom").unwrap(); // 2
        claim_batch(&c, "embed", 8).unwrap();
        fail(&c, id, "boom").unwrap(); // 3 → failed
        let (done, failed, pending) = stats(&c, "embed").unwrap();
        assert_eq!((done, failed, pending), (0, 1, 0));
    }

    #[test]
    fn requeue_stale_recovers_running() {
        let c = mem();
        enqueue(&c, "tag", 1, None).unwrap();
        claim_batch(&c, "tag", 8).unwrap(); // → running
        assert_eq!(requeue_stale(&c).unwrap(), 1);
        assert_eq!(stats(&c, "tag").unwrap().2, 1);
    }

    #[test]
    fn complete_terminates() {
        let c = mem();
        let id = enqueue(&c, "embed", 7, None).unwrap();
        claim_batch(&c, "embed", 8).unwrap();
        complete(&c, id).unwrap();
        assert_eq!(stats(&c, "embed").unwrap().0, 1);
    }

    #[test]
    fn claim_respects_kind() {
        let c = mem();
        enqueue(&c, "embed", 1, None).unwrap();
        enqueue(&c, "tag", 1, None).unwrap();
        assert_eq!(claim_batch(&c, "tag", 8).unwrap().len(), 1);
        assert_eq!(claim_batch(&c, "tag", 8).unwrap().len(), 0);
        assert_eq!(claim_batch(&c, "embed", 8).unwrap().len(), 1);
    }

    #[test]
    fn status_consts_match_ddl() {
        for s in [
            status::PENDING,
            status::RUNNING,
            status::DONE,
            status::FAILED,
        ] {
            assert!(matches!(s, "pending" | "running" | "done" | "failed"));
        }
    }
}
