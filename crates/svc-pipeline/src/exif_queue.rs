//! EXIF2（task）— 存量图 EXIF 回填队列：`job_queue(kind='exif')`，
//! 照 D4 打标队列范式（64/批、3 次重试、断点续跑、cancel 回 pending、limit 配额）。
//!
//! 语义要点：
//! - 入队幂等：`exif IS NULL` 的图片才入队；已有结论（含「无 EXIF」）不重跑。
//!   重跑入口 = retry-failed（只复活 failed）/ 显式范围 enqueue（id 区间）。
//! - 「无 EXIF / 解析失败 / 格式不支持」是**有效结论**：写 exif json 原因后 job done，
//!   不算失败（失败留给真故障：文件失联/IO 错误，重试 3 次后 failed，lastError 可查）。
//! - 回填同时回填画幅两列（aspectRatio/aspectClass，宽高取既有 resource，不重新解码）。
//! - 回填是显式用户动作：无视 parse_on_import 开关照常解析，但 GPS 档位照设置生效。

use std::sync::atomic::AtomicBool;

use core_types::ExifRunReport;
use rusqlite::Connection;
use std::sync::atomic::Ordering;

use engine_jobq::{self as jobq};

use crate::{PipelineError, Result};

/// 每批 claim 数（D4 同款）。
pub const EXIF_BATCH: i64 = 64;

/// 强制重解析（EXIF2 验收捕获的功能缺口）：**显式重跑**已出结论的图。
///
/// 为什么需要：`enqueue_missing` 只收 `exif IS NULL` 且没有 done job 的图 —— 用户在
/// 「EXIF 设置」里把 GPS 从 off 打开成 coords/place 后，存量图早已 done，回填按钮
/// 什么都不会做，坐标永远不会落库（档位切换形同无效）。这里把区间内图片的 exif 结论
/// 清空 + 删掉它们的 exif job 行（含 done），让 `enqueue_missing` 重新入队、逐张重解析。
///
/// 副作用与边界：清空期间该批图的 exif 结论短暂为空（重解析同一事务序列内立刻重建）；
/// 只对 `mediaType='image' AND deleted=0 AND missing=0` 生效；**必须由显式动作触发**
/// （`POST /api/exif/backfill {"force": true}`），不做隐式自动重跑（万级库不该被设置改动全量重扫）。
pub fn reset_for_reparse(conn: &Connection, from_id: Option<i64>, to_id: Option<i64>) -> Result<usize> {
    let from = from_id.unwrap_or(0);
    let to = to_id.unwrap_or(i64::MAX);
    conn.execute(
        "DELETE FROM job_queue WHERE kind='exif' AND targetId BETWEEN ?1 AND ?2",
        rusqlite::params![from, to],
    )
    .map_err(PipelineError::Sqlite)?;
    let n = conn
        .execute(
            "UPDATE assets SET exif=NULL
             WHERE mediaType='image' AND deleted=0 AND missing=0
               AND assetId BETWEEN ?1 AND ?2 AND exif IS NOT NULL",
            rusqlite::params![from, to],
        )
        .map_err(PipelineError::Sqlite)?;
    Ok(n)
}

/// 入队存量待解析图片（id 升序区间可选）。返回新增 job 数。
pub fn enqueue_missing(conn: &Connection, from_id: Option<i64>, to_id: Option<i64>) -> Result<usize> {
    let from = from_id.unwrap_or(0);
    let to = to_id.unwrap_or(i64::MAX);
    let n = conn.execute(
        "INSERT INTO job_queue (kind, targetId, createdAt, updatedAt)
         SELECT 'exif', a.assetId, unixepoch(), unixepoch()
         FROM assets a
         WHERE a.mediaType = 'image' AND a.deleted = 0 AND a.missing = 0
           AND a.exif IS NULL
           AND a.assetId BETWEEN ?1 AND ?2
           AND NOT EXISTS (
               SELECT 1 FROM job_queue q
               WHERE q.kind = 'exif' AND q.targetId = a.assetId
                 AND q.status IN ('pending', 'running', 'done')
           )",
        rusqlite::params![from, to],
    )?;
    Ok(n)
}

/// 复活 failed job（文件曾失联后恢复等场景）。
pub fn retry_failed(conn: &Connection) -> Result<usize> {
    let n = conn.execute(
        "UPDATE job_queue SET status='pending', updatedAt=unixepoch()
         WHERE kind='exif' AND status='failed'",
        [],
    )?;
    Ok(n)
}

/// 队列状态快照（GET /api/exif/status 用）。
pub struct ExifQueueStatus {
    pub done: i64,
    pub failed: i64,
    pub pending: i64,
    /// 仍未有结论的图片数（exif IS NULL）
    pub remaining: i64,
    pub last_error: Option<String>,
}

pub fn status(conn: &Connection) -> Result<ExifQueueStatus> {
    let (done, failed, pending) = jobq::stats(conn, "exif")?;
    let remaining: i64 = conn.query_row(
        "SELECT COUNT(*) FROM assets
         WHERE mediaType='image' AND deleted=0 AND missing=0 AND exif IS NULL",
        [],
        |r| r.get(0),
    )?;
    let last_error: Option<String> = conn
        .query_row(
            "SELECT lastError FROM job_queue WHERE kind='exif' AND status='failed'
             AND lastError IS NOT NULL ORDER BY updatedAt DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(ExifQueueStatus { done, failed, pending, remaining, last_error })
}

/// 跑回填队列（不限量）。cancel 置位时未处理 job 回 pending，断点续跑。
pub fn run_exif_queue(
    conn: &Connection,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(usize, usize, usize, usize), // (index, processed, done, failed)
) -> Result<ExifRunReport> {
    run_exif_queue_with_limit(conn, cancel, progress, 0)
}

/// 带 limit 的变体（web API 验收/分轮跑用）：limit>0 时本轮最多处理 limit 个。
pub fn run_exif_queue_with_limit(
    conn: &Connection,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(usize, usize, usize, usize),
    limit: usize,
) -> Result<ExifRunReport> {
    let _ = jobq::requeue_stale(conn)?;
    let mut report = ExifRunReport::default();

    let mut limited = false;
    loop {
        if cancel.load(Ordering::SeqCst) {
            report.cancelled = true;
            break;
        }
        let batch = jobq::claim_batch(conn, "exif", EXIF_BATCH)?;
        if batch.is_empty() {
            break;
        }
        for (job_id, asset_id, _) in &batch {
            if cancel.load(Ordering::SeqCst) || (limit > 0 && report.processed >= limit) {
                // 未处理的 claim 回 pending（不丢任务，断点续跑语义）
                conn.execute(
                    "UPDATE job_queue SET status = 'pending', updatedAt = unixepoch()
                     WHERE id = ?1 AND status = 'running'",
                    [job_id],
                )?;
                if cancel.load(Ordering::SeqCst) {
                    report.cancelled = true;
                } else {
                    limited = true;
                }
                break;
            }
            report.processed += 1;
            let index = report.processed;
            match exif_one(conn, *asset_id) {
                Ok(outcome) => {
                    jobq::complete(conn, *job_id)?;
                    match outcome {
                        ExifOne::Applied => report.done += 1,
                        ExifOne::Absent => report.done += 1,
                        ExifOne::Skipped => report.skipped += 1,
                    }
                    progress(index, report.processed, report.done, report.failed);
                }
                Err(e) => {
                    let msg = format!("{e}");
                    jobq::fail(conn, *job_id, &msg)?;
                    let st: String = conn.query_row(
                        "SELECT status FROM job_queue WHERE id = ?1",
                        [job_id],
                        |r| r.get(0),
                    )?;
                    if st == "failed" {
                        report.failed += 1;
                        if report.errors.len() < 50 {
                            report.errors.push(format!("{asset_id}: {msg}"));
                        }
                    } else {
                        report.retry += 1;
                    }
                    progress(index, report.processed, report.done, report.failed);
                }
            }
        }
        if report.cancelled || limited {
            break;
        }
    }

    let (d, f, p) = jobq::stats(conn, "exif")?;
    report.jobDone = d;
    report.jobFailed = f;
    report.jobPending = p;
    Ok(report)
}

enum ExifOne {
    /// 元数据已落库（含「无 EXIF」结论——原因已写 exif json）
    Applied,
    Absent,
    /// 资产已删/失联/非图片：直接 complete（不占失败名额）
    Skipped,
}

fn exif_one(conn: &Connection, asset_id: i64) -> Result<ExifOne> {
    let (abs_path, ext, deleted, missing, media_type, exif_null): (
        String,
        String,
        i64,
        i64,
        String,
        i64,
    ) = conn
        .query_row(
            "SELECT absPath, fileExt, deleted, missing, mediaType, (exif IS NULL)
             FROM assets WHERE assetId = ?1",
            [asset_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .map_err(|e| PipelineError::Sqlite(e))?;
    if deleted != 0 || missing != 0 || media_type != "image" {
        return Ok(ExifOne::Skipped);
    }
    if exif_null == 0 {
        // 已有结论（导入路径写过了）：只补画幅列，不重复解析
        let (w, h) = core_storage::exif_meta::primary_dims(conn, asset_id)
            .map_err(PipelineError::Storage)?;
        core_storage::exif_meta::set_aspect(conn, asset_id, w, h)
            .map_err(PipelineError::Storage)?;
        return Ok(ExifOne::Skipped);
    }
    if !std::path::Path::new(&abs_path).is_file() {
        return Err(PipelineError::Other(format!("文件失联: {abs_path}")));
    }
    let settings: core_config::ExifSettings = core_storage::settings::get_or_default(
        conn,
        core_config::EXIF_SETTINGS_KEY,
    )
    .unwrap_or_default();
    // 回填是显式动作：无视 parse_on_import；GPS 档位照设置生效
    let cfg = core_config::ExifSettings {
        parse_on_import: true,
        gps: settings.gps,
        gps_provider: settings.gps_provider,
    };
    match engine_exif::apply_for_asset(conn, asset_id, std::path::Path::new(&abs_path), &ext, &cfg) {
        Ok(engine_exif::ExifApplyOutcome::Parsed) => {
            let (w, h) = core_storage::exif_meta::primary_dims(conn, asset_id)
                .map_err(PipelineError::Storage)?;
            core_storage::exif_meta::set_aspect(conn, asset_id, w, h)
                .map_err(PipelineError::Storage)?;
            Ok(ExifOne::Applied)
        }
        Ok(engine_exif::ExifApplyOutcome::Absent(_)) => {
            // 无 EXIF/不支持/解析失败：结论已落 exif json，画幅列照补（宽高与 EXIF 无关）
            let (w, h) = core_storage::exif_meta::primary_dims(conn, asset_id)
                .map_err(PipelineError::Storage)?;
            core_storage::exif_meta::set_aspect(conn, asset_id, w, h)
                .map_err(PipelineError::Storage)?;
            Ok(ExifOne::Absent)
        }
        Ok(engine_exif::ExifApplyOutcome::SkippedDisabled) => Ok(ExifOne::Skipped),
        Err(e) => Err(PipelineError::Other(format!("exif 落库失败: {e}"))),
    }
}
