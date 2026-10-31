//! D10 — 自定义打标任务队列（PLAN.md M3 裁决 2026-10-13）：
//! `job_queue`(kind='custom_tag', payload={"taskId":N}) 驱动的批量 inline 打标。
//!
//! 复用 D4 `tag_queue` 骨架：requeue_stale 断点续跑、64/批 claim、失败重试 3 次、
//! 取消旗标未处理 job 回 pending。差异点：
//! - 每个 job 按 payload 找任务；任务已删/停用 → 直接完成（跳过，不入库）。
//! - 推理走 [`TagProvider::tag_inline_values`]（统一标签数组通道，2026-10-19 裁决），
//!   结果行存数组快照 + key-val 拍平扇出到 tag(origin='custom', taskId)
//!   （core-storage::custom_tasks::upsert_custom_tag_values）。
//! - 空值约定：图片无该维度内容 → 不写 tag 行，job 正常完成。
//!
//! 入队（全量重跑/单资产自动入队）在 core-storage::custom_tasks（与 svc-import 共用）。

use std::sync::atomic::{AtomicBool, Ordering};

use core_types::{CustomTagFailedItem, TagProgress, TagRunReport};
use rusqlite::Connection;

use crate::{PipelineError, Result};
use core_storage::custom_tasks::{self as tasks, parse_custom_payload, CUSTOM_TAG_KIND};
use engine_jobq::{self as jobq};
use engine_tag::TagProvider;

/// 与 D4 tag 队列同规格：每批 claim 的 job 数。
pub const CUSTOM_TAG_BATCH: i64 = 64;

/// 失败 job 列表（最近优先，带任务名；任务已删则名字留空）。
pub fn custom_failed_list(conn: &Connection, limit: i64) -> Result<Vec<CustomTagFailedItem>> {
    let names: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT id, name FROM custom_tag_task")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut stmt = conn.prepare(
        "SELECT id, targetId, attempts, COALESCE(lastError,''), COALESCE(payload,'')
         FROM job_queue WHERE kind = ?1 AND status = 'failed'
         ORDER BY updatedAt DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![CUSTOM_TAG_KIND, limit], |r| {
        let payload: String = r.get(4)?;
        let task_id = parse_custom_payload(Some(&payload)).unwrap_or(-1);
        let task_name = names
            .iter()
            .find(|(id, _)| *id == task_id)
            .map(|(_, n)| n.clone())
            .unwrap_or_default();
        Ok(CustomTagFailedItem {
            jobId: r.get(0)?,
            taskId: task_id,
            taskName: task_name,
            assetId: r.get(1)?,
            attempts: r.get(2)?,
            lastError: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// job 对应资产是否可打标（与 tag_queue::eligible 同口径）。
fn eligible(conn: &Connection, asset_id: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM assets
             WHERE assetId = ?1 AND deleted = 0 AND missing = 0 AND mediaType = 'image'",
        [asset_id],
        |r| r.get::<_, i64>(0),
    )? > 0)
}

fn asset_path(conn: &Connection, asset_id: i64) -> Result<String> {
    conn.query_row(
        "SELECT absPath FROM assets WHERE assetId = ?1 AND deleted = 0",
        [asset_id],
        |r| r.get(0),
    )
    .map_err(|_| PipelineError::AssetNotFound(asset_id))
}

/// 跑自定义打标队列直到空或取消（断点续跑：内部先 requeue_stale）。
/// 进度事件复用 D4 的 TagProgress（"custom_tag:progress"）。
pub fn run_custom_queue(
    conn: &Connection,
    provider: &dyn TagProvider,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(TagProgress),
) -> Result<TagRunReport> {
    run_custom_queue_with_limit(conn, provider, cancel, progress, 0)
}

/// 带 `limit` 的变体（W4 web API 验收用）：`limit > 0` 时本轮最多处理 limit 个
/// job（含 skipped），剩余 pending 原样保留（0 = 不限量，桌面版语义）。
pub fn run_custom_queue_with_limit(
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
    let mut report = TagRunReport::default();
    let model = provider.spec().name.clone();

    let mut limited = false;
    loop {
        if cancel.load(Ordering::SeqCst) {
            report.cancelled = true;
            break;
        }
        let batch = jobq::claim_batch(conn, CUSTOM_TAG_KIND, CUSTOM_TAG_BATCH)?;
        if batch.is_empty() {
            break;
        }
        for (job_id, asset_id, payload) in &batch {
            if cancel.load(Ordering::SeqCst) {
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
            let (_, _, pending) = jobq::stats(conn, CUSTOM_TAG_KIND)?;
            let index = report.processed + 1;
            let total = pending as usize + index;
            let emit = |report: &TagRunReport, status: &str, error: Option<String>| TagProgress {
                jobId: *job_id,
                assetId: *asset_id,
                status: status.into(),
                index,
                total,
                doneCount: report.done,
                failedCount: report.failed,
                error,
            };

            // 任务已删/停用/坏 payload → 直接完成（不浪费重试）
            let task = parse_custom_payload(payload.as_deref())
                .and_then(|id| tasks::get_task(conn, id).ok().flatten());
            if !task.as_ref().is_some_and(|t| t.enabled) {
                jobq::complete(conn, *job_id)?;
                report.skipped += 1;
                progress(emit(&report, "skipped", None));
                report.processed += 1;
                continue;
            }
            let task = task.unwrap();
            if !eligible(conn, *asset_id)? {
                jobq::complete(conn, *job_id)?;
                report.skipped += 1;
                progress(emit(&report, "skipped", None));
                report.processed += 1;
                continue;
            }
            let outcome = (|| -> Result<()> {
                let path = asset_path(conn, *asset_id)?;
                let p = std::path::Path::new(&path);
                if !p.is_file() {
                    return Err(PipelineError::NoImage(*asset_id, path));
                }
                let values = provider.tag_inline_values(p, &task.prompt)?;
                // 空数组约定：无该维度内容 → 结果行与产出标签一并清掉（重跑不留旧值），
                // 不影响该图其他来源（人工/模板/目录）的标签。
                if values.is_empty() {
                    core_storage::tags::clear_tag_result(conn, *asset_id, task.id)?;
                    return Ok(());
                }
                // 结果表存数组快照（name=维度名）+ 统一扇出 tag（normalize + key-val 拍平）
                tasks::upsert_custom_tag_values(
                    conn, *asset_id, task.id, &task.name, &values, &model,
                )?;
                Ok(())
            })();

            match outcome {
                Ok(()) => {
                    jobq::complete(conn, *job_id)?;
                    report.done += 1;
                    progress(emit(&report, "done", None));
                }
                Err(e) => {
                    let msg = format!("{e}");
                    jobq::fail(conn, *job_id, &msg)?;
                    let status: String = conn.query_row(
                        "SELECT status FROM job_queue WHERE id = ?1",
                        [job_id],
                        |r| r.get(0),
                    )?;
                    if status == "failed" {
                        report.failed += 1;
                        if report.errors.len() < 50 {
                            report
                                .errors
                                .push(format!("任务[{}] 资产{asset_id}: {msg}", task.name));
                        }
                    } else {
                        report.retry += 1;
                    }
                    progress(emit(
                        &report,
                        if status == "failed" {
                            "failed"
                        } else {
                            "retry"
                        },
                        Some(msg),
                    ));
                }
            }
            report.processed += 1;
        }
        if report.cancelled || limited {
            break;
        }
    }

    let (d, f, p) = jobq::stats(conn, CUSTOM_TAG_KIND)?;
    report.jobDone = d;
    report.jobFailed = f;
    report.jobPending = p;
    Ok(report)
}
