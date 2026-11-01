//! D4 — 自动打标任务队列（PLAN.md M3）：`job_queue`(kind='tag') 驱动的批量打标。
//!
//! 语义：
//! - 入队：`enq_untagged`（仅新增：无 auto 标签记录的图片）/ `enq_all` + `reset_tag_jobs`
//!   （全库重跑：换模板/换模型后用，job 表清空重建，人工数据不动）。
//! - 断点续跑：`run_tag_queue` 先 `requeue_stale`（崩溃残留 running 回 pending），
//!   64/批 claim → `tag_one` → complete/fail；失败重试 3 次（jobq::MAX_ATTEMPTS）后 failed。
//! - 取消：cancel 旗标置位后，已 claim 未处理的 job 回 pending，本轮返回。
//! - 幂等：`apply_tagging` 的 ON CONFLICT 保证重跑不重复追加；job 入队用
//!   NOT EXISTS 防重复行。
//!
//! 打标结果的落库（title/description/tags origin='auto'）复用 `crate::tag_one`。

use std::sync::atomic::{AtomicBool, Ordering};

use core_types::{TagFailedItem, TagProgress, TagQueueStats, TagRunReport};
use rusqlite::Connection;

use crate::{tag_one, PipelineError, Result};
use engine_jobq::{self as jobq};
use engine_tag::TagProvider;

/// 每批 claim 的 job 数（PLAN.md M3：64/批）。
pub const TAG_BATCH: i64 = 64;

/// 队列统计、失败明细与进度/汇总 DTO 定义在 core-types（L3 命令层直接透传）。
/// 可打标资产条件（与 tag_one 的 SELECT 对齐：图片、未删、未失联）。
const ELIGIBLE: &str = "deleted = 0 AND missing = 0 AND mediaType = 'image'";

/// 仅新增入队：还没有 auto 标签的图片资产。返回入队数。
pub fn enq_untagged(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "INSERT INTO job_queue (kind, targetId, payload, status, createdAt, updatedAt)
         SELECT 'tag', assetId, NULL, 'pending', unixepoch(), unixepoch() FROM assets
         WHERE assetId IN (
             SELECT assetId FROM assets WHERE deleted = 0 AND missing = 0 AND mediaType = 'image'
             EXCEPT
             SELECT assetId FROM tag WHERE origin = 'auto'
         )
         AND NOT EXISTS (SELECT 1 FROM job_queue j WHERE j.kind = 'tag' AND j.targetId = assets.assetId)",
        [],
    )?)
}

/// 全库重跑入队（换模板/换模型）：清空 tag job 行后全量入队。
/// 人工标签与已有标题不受影响（apply 侧人工优先）。
pub fn enq_all(conn: &Connection) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM job_queue WHERE kind = 'tag'", [])?;
    let n = tx.execute(
        "INSERT INTO job_queue (kind, targetId, payload, status, createdAt, updatedAt)
         SELECT 'tag', assetId, NULL, 'pending', unixepoch(), unixepoch()
         FROM assets WHERE deleted = 0 AND missing = 0 AND mediaType = 'image'",
        [],
    )?;
    tx.commit()?;
    Ok(n)
}

/// 队列统计 + 待入队资产数。
pub fn tag_stats(conn: &Connection) -> Result<TagQueueStats> {
    let (done, failed, pending) = jobq::stats(conn, "tag")?;
    let untagged: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM assets WHERE {ELIGIBLE} AND assetId NOT IN
             (SELECT assetId FROM tag WHERE origin = 'auto')"
        ),
        [],
        |r| r.get(0),
    )?;
    Ok(TagQueueStats {
        done,
        failed,
        pending,
        untaggedAssets: untagged,
    })
}

/// 失败 job 列表（最近优先）。
pub fn tag_failed_list(conn: &Connection, limit: i64) -> Result<Vec<TagFailedItem>> {
    let mut stmt = conn.prepare(
        "SELECT id, targetId, attempts, COALESCE(lastError,'') FROM job_queue
         WHERE kind = 'tag' AND status = 'failed'
         ORDER BY updatedAt DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit], |r| {
        let lastError: String = r.get(3)?;
        Ok(TagFailedItem {
            jobId: r.get(0)?,
            assetId: r.get(1)?,
            attempts: r.get(2)?,
            errorClass: classify_tag_failure(&lastError).to_string(),
            lastError,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 失败归类（UI 按类重试用）：`truncated` = 输出被 max_tokens 截断；
/// `other` = 网络/上游/真·非法输出。老版本落库的错误（无 `truncated` 文案但
/// 有 `EOF while parsing`）也归到截断类，历史 job 一并可救。
pub fn classify_tag_failure(last_error: &str) -> &'static str {
    let e = last_error.to_ascii_lowercase();
    if e.contains("truncated") || e.contains("eof while parsing") || e.contains("max_tokens") {
        "truncated"
    } else {
        "other"
    }
}

/// 失败 job 重置为 pending（UI「重试失败项」）。
pub fn retry_failed(conn: &Connection) -> Result<usize> {
    retry_failed_class(conn, None)
}

/// 按类重置失败 job（`class` 缺省/空 = 全部）。
/// `truncated` 只重置截断类（调大 max_tokens 后精准续跑），`other` 反之。
pub fn retry_failed_class(conn: &Connection, class: Option<&str>) -> Result<usize> {
    let truncated_like =
        "(lastError LIKE '%truncated%' OR lastError LIKE '%EOF while parsing%' OR lastError LIKE '%max_tokens%')";
    let sql = match class.map(str::trim).filter(|c| !c.is_empty()) {
        None => format!(
            "UPDATE job_queue SET status = 'pending', attempts = 0, lastError = NULL,
             updatedAt = unixepoch() WHERE kind = 'tag' AND status = 'failed'"
        ),
        Some("truncated") => format!(
            "UPDATE job_queue SET status = 'pending', attempts = 0, lastError = NULL,
             updatedAt = unixepoch() WHERE kind = 'tag' AND status = 'failed' AND {truncated_like}"
        ),
        Some(_) => format!(
            "UPDATE job_queue SET status = 'pending', attempts = 0, lastError = NULL,
             updatedAt = unixepoch() WHERE kind = 'tag' AND status = 'failed' AND NOT {truncated_like}"
        ),
    };
    Ok(conn.execute(&sql, [])?)
}

/// job 对应资产是否可打标（claim 后已删/missing/非图片 → 直接完成不浪费重试）。
fn eligible(conn: &Connection, asset_id: i64) -> Result<bool> {
    Ok(conn.query_row(
        &format!("SELECT COUNT(*) FROM assets WHERE assetId = ?1 AND {ELIGIBLE}"),
        [asset_id],
        |r| r.get::<_, i64>(0),
    )? > 0)
}

/// 跑打标队列直到空或取消。provider 未 ready 报错（不静默）。
///
/// 断点续跑：内部先 `requeue_stale`；调用方进程被 kill 后再次进入本函数即续跑，
/// job 行不会重复（入队侧 NOT EXISTS + 本函数只更新已有行）。
pub fn run_tag_queue(
    conn: &Connection,
    provider: &dyn TagProvider,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(TagProgress),
) -> Result<TagRunReport> {
    run_tag_queue_with_limit(conn, provider, cancel, progress, 0)
}

/// 带 `limit` 的变体（W4 web API 验收用）：`limit > 0` 时本轮最多处理 limit 个
/// job（含 skipped），剩余 pending 原样保留（0 = 不限量，桌面版语义）。
pub fn run_tag_queue_with_limit(
    conn: &Connection,
    provider: &dyn TagProvider,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(TagProgress),
    limit: usize,
) -> Result<TagRunReport> {
    if !provider.ready() {
        return Err(PipelineError::ProviderNotReady(
            provider.spec().name.clone(),
        ));
    }
    let _ = jobq::requeue_stale(conn)?;
    let (init_done, init_failed, _) = jobq::stats(conn, "tag")?;
    let mut report = TagRunReport::default();

    let mut limited = false;
    loop {
        if cancel.load(Ordering::SeqCst) {
            report.cancelled = true;
            break;
        }
        let batch = jobq::claim_batch(conn, "tag", TAG_BATCH)?;
        if batch.is_empty() {
            break;
        }
        for (job_id, asset_id, _) in &batch {
            if cancel.load(Ordering::SeqCst) {
                // 未处理的 job 回 pending（不丢任务）
                conn.execute(
                    "UPDATE job_queue SET status = 'pending', updatedAt = unixepoch()
                     WHERE id = ?1 AND status = 'running'",
                    [job_id],
                )?;
                report.cancelled = true;
                break;
            }
            if limit > 0 && report.processed >= limit {
                // 本轮配额用完：当前已 claim 的 job 回 pending（不丢任务）
                conn.execute(
                    "UPDATE job_queue SET status = 'pending', updatedAt = unixepoch()
                     WHERE id = ?1 AND status = 'running'",
                    [job_id],
                )?;
                limited = true;
                break;
            }
            let (_, _, pending) = jobq::stats(conn, "tag")?;
            let index = report.processed + 1;
            if !eligible(conn, *asset_id)? {
                jobq::complete(conn, *job_id)?;
                report.skipped += 1;
                progress(TagProgress {
                    jobId: *job_id,
                    assetId: *asset_id,
                    status: "skipped".into(),
                    index,
                    total: pending as usize + index,
                    doneCount: report.done,
                    failedCount: report.failed,
                    error: None,
                });
                report.processed += 1;
                continue;
            }
            match tag_one(conn, provider, *asset_id) {
                Ok(res) => {
                    jobq::complete(conn, *job_id)?;
                    report.done += 1;
                    if res.repaired {
                        report.repaired += 1;
                    }
                    progress(TagProgress {
                        jobId: *job_id,
                        assetId: *asset_id,
                        status: "done".into(),
                        index,
                        total: pending as usize + index,
                        doneCount: report.done,
                        failedCount: report.failed,
                        error: None,
                    });
                }
                Err(e) => {
                    let msg = format!("{e}");
                    jobq::fail(conn, *job_id, &msg)?;
                    // fail 内部：attempts<MAX → pending（还要重试），否则 failed
                    let status: String = conn.query_row(
                        "SELECT status FROM job_queue WHERE id = ?1",
                        [job_id],
                        |r| r.get(0),
                    )?;
                    if status == "failed" {
                        report.failed += 1;
                        if report.errors.len() < 50 {
                            report.errors.push(format!("{asset_id}: {msg}"));
                        }
                    } else {
                        report.retry += 1;
                    }
                    progress(TagProgress {
                        jobId: *job_id,
                        assetId: *asset_id,
                        status: if status == "failed" {
                            "failed".into()
                        } else {
                            "retry".into()
                        },
                        index,
                        total: pending as usize + index,
                        doneCount: report.done,
                        failedCount: report.failed,
                        error: Some(msg),
                    });
                }
            }
            report.processed += 1;
        }
        if report.cancelled || limited {
            break;
        }
    }

    // 注意 done/failed 统计口径：队列里还含历史 done（首轮 run 后重跑时 init_done≠0），
    // 事件里的 doneCount/failedCount 是本轮增量；jobDone/jobFailed 是全队列终态。
    let (d, f, p) = jobq::stats(conn, "tag")?;
    report.jobDone = d;
    report.jobFailed = f;
    report.jobPending = p;
    let _ = (init_done, init_failed);
    Ok(report)
}
