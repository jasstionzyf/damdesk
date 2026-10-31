//! OCR-E — 文字识别任务队列：`job_queue`(kind='ocr') 驱动的批量识别。
//!
//! 语义与 tag_queue 完全同构（断点续跑 / 3 次重试 / 取消回 pending / limit 配额）：
//! - 入队：`enq_untagged`（仅新增：`ocr_text = ''` 的图片）/ `enq_all` + 清空 job
//!   （全库重跑；重跑会覆盖已有 ocr_text，人工不可编辑所以无「人工数据不动」问题）。
//! - 推理：`run_ocr_queue*`（`local-infer` 门控）→ `ocr_one`：core-media 解码 →
//!   engine-ocr（det DB 后处理 + rec CTC）→ 行文本 `'\n'` 连接写 `assets.ocr_text`。
//! - web 容器（default-features=false）：统计/入队/失败列表可用，`run_*` 缺席 →
//!   app-web 侧启动即 409（模型/运行时未内置，响亮不静默）。
//!
//! 模型缺文件不是 job 失败：入队前 `engine_ocr::models_ready()` 预检，未就绪 →
//! 启动报错（ProviderNotReady 同语义），单张中途文件丢失仍按失败重试（可重试错误）。

use std::sync::atomic::{AtomicBool, Ordering};

use core_types::{OcrFailedItem, OcrProgress, OcrQueueStats, OcrRunReport};
use rusqlite::Connection;

use crate::{PipelineError, Result};
use engine_jobq::{self as jobq};

/// 每批 claim 的 job 数（与 tag 队列同 64/批）。
pub const OCR_BATCH: i64 = 64;
/// ocr_text 落库上限（防超长截图把行撑爆；超过截断并追加省略标记）。
pub const OCR_TEXT_MAX_BYTES: usize = 65_536;

/// 可识别资产条件（与 tag 队列对齐：图片、未删、未失联）。
const ELIGIBLE: &str = "deleted = 0 AND missing = 0 AND mediaType = 'image'";

/// 仅新增入队：还没有 ocr_text 的图片资产。返回入队数。
pub fn enq_untagged(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "INSERT INTO job_queue (kind, targetId, payload, status, createdAt, updatedAt)
         SELECT 'ocr', assetId, NULL, 'pending', unixepoch(), unixepoch() FROM assets
         WHERE deleted = 0 AND missing = 0 AND mediaType = 'image' AND ocr_text = ''
         AND NOT EXISTS (SELECT 1 FROM job_queue j WHERE j.kind = 'ocr' AND j.targetId = assets.assetId)",
        [],
    )?)
}

/// 全库重跑入队：清空 ocr job 行后全量入队（重跑覆盖已有 ocr_text）。
pub fn enq_all(conn: &Connection) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM job_queue WHERE kind = 'ocr'", [])?;
    let n = tx.execute(
        "INSERT INTO job_queue (kind, targetId, payload, status, createdAt, updatedAt)
         SELECT 'ocr', assetId, NULL, 'pending', unixepoch(), unixepoch()
         FROM assets WHERE deleted = 0 AND missing = 0 AND mediaType = 'image'",
        [],
    )?;
    tx.commit()?;
    Ok(n)
}

/// 队列统计 + 待入队资产数。
pub fn ocr_stats(conn: &Connection) -> Result<OcrQueueStats> {
    let (done, failed, pending) = jobq::stats(conn, "ocr")?;
    let no_ocr: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM assets WHERE {ELIGIBLE} AND ocr_text = ''"),
        [],
        |r| r.get(0),
    )?;
    Ok(OcrQueueStats {
        done,
        failed,
        pending,
        noOcrAssets: no_ocr,
    })
}

/// 失败 job 列表（最近优先）。
pub fn ocr_failed_list(conn: &Connection, limit: i64) -> Result<Vec<OcrFailedItem>> {
    let mut stmt = conn.prepare(
        "SELECT id, targetId, attempts, COALESCE(lastError,'') FROM job_queue
         WHERE kind = 'ocr' AND status = 'failed'
         ORDER BY updatedAt DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit], |r| {
        Ok(OcrFailedItem {
            jobId: r.get(0)?,
            assetId: r.get(1)?,
            attempts: r.get(2)?,
            lastError: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 失败 job 重置为 pending（UI「重试失败项」）。
pub fn retry_failed(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE job_queue SET status = 'pending', attempts = 0, lastError = NULL, updatedAt = unixepoch()
         WHERE kind = 'ocr' AND status = 'failed'",
        [],
    )?)
}

/// 单资产识别 + 落库（返回识别文本；空图/无字 → ''）。local-infer 专用。
#[cfg(feature = "local-infer")]
pub fn ocr_one(conn: &Connection, engine: &engine_ocr::OcrEngine, asset_id: i64) -> Result<String> {
    let path: String = conn.query_row(
        "SELECT absPath FROM assets WHERE assetId = ?1 AND deleted = 0",
        [asset_id],
        |r| r.get(0),
    )?;
    let (w, h, rgb) = core_media::rgb8(std::path::Path::new(&path))
        .map_err(|e| crate::PipelineError::NoImage(asset_id, format!("{path}: {e}")))?;
    let lines = engine
        .run_rgb(w as u32, h as u32, &rgb)
        .map_err(|e| crate::PipelineError::NoImage(asset_id, format!("{path}: {e}")))?;
    let mut text = lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if text.len() > OCR_TEXT_MAX_BYTES {
        // 按字符边界截断（行结构在截断点丢弃可接受，响亮标记）
        let mut cut = OCR_TEXT_MAX_BYTES;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n…[truncated]");
    }
    conn.execute(
        "UPDATE assets SET ocr_text = ?2 WHERE assetId = ?1",
        rusqlite::params![asset_id, text],
    )?;
    // DM-C1：行框落库（归一化 0..1）—— inpaint 重绘区域的本地证据。
    // 与文本同一次识别结果，整组替换；退化矩形（宽/高 <= 0）跳过（rec 没跑的空框）。
    let boxes: Vec<core_storage::ocr_boxes::OcrBox> = lines
        .iter()
        .filter_map(|l| {
            let r = core_storage::ocr_boxes::normalize_box(
                l.x0,
                l.y0,
                l.x1,
                l.y1,
                w.max(0) as u32,
                h.max(0) as u32,
            );
            let b = core_storage::ocr_boxes::OcrBox {
                x0: r[0],
                y0: r[1],
                x1: r[2],
                y1: r[3],
                text: l.text.clone(),
                box_score: f64::from(l.box_score),
                text_score: f64::from(l.text_score),
            };
            core_storage::ocr_boxes::validate(&b).ok().map(|_| b)
        })
        .collect();
    core_storage::ocr_boxes::replace(conn, asset_id, &boxes)?;
    // OCR-MATCH：写路径同步——把同一分段口径的倒排索引就地更新（upsert 同时清掉
    // 0013 触发器标的 dirty 标记；漏了也会在下一次 core_storage::open 自愈）。
    core_storage::ocr_index::upsert(conn, asset_id, &text)?;
    Ok(text)
}

/// job 对应资产是否可识别（claim 后已删/missing/非图片 → 直接完成不浪费重试）。
fn eligible(conn: &Connection, asset_id: i64) -> Result<bool> {
    Ok(conn.query_row(
        &format!("SELECT COUNT(*) FROM assets WHERE assetId = ?1 AND {ELIGIBLE}"),
        [asset_id],
        |r| r.get::<_, i64>(0),
    )? > 0)
}

/// 跑 OCR 队列直到空或取消。引擎未就绪报错（不静默）。local-infer 门控。
#[cfg(feature = "local-infer")]
pub fn run_ocr_queue(
    conn: &Connection,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(OcrProgress),
) -> Result<OcrRunReport> {
    run_ocr_queue_with_limit(conn, cancel, progress, 0)
}

/// 带 `limit` 的变体（web API 验收用）：`limit > 0` 时本轮最多处理 limit 个 job。
#[cfg(feature = "local-infer")]
pub fn run_ocr_queue_with_limit(
    conn: &Connection,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(OcrProgress),
    limit: usize,
) -> Result<OcrRunReport> {
    if !engine_ocr::models_ready() {
        return Err(PipelineError::ProviderNotReady("ppocrv6-medium".into()));
    }
    let engine = engine_ocr::infer::engine()
        .map_err(|e| PipelineError::ProviderNotReady(format!("ppocrv6-medium: {e}")))?;
    let _ = jobq::requeue_stale(conn)?;
    let mut report = OcrRunReport::default();

    let mut limited = false;
    loop {
        if cancel.load(Ordering::SeqCst) {
            report.cancelled = true;
            break;
        }
        let batch = jobq::claim_batch(conn, "ocr", OCR_BATCH)?;
        if batch.is_empty() {
            break;
        }
        for (job_id, asset_id, _) in &batch {
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
                conn.execute(
                    "UPDATE job_queue SET status = 'pending', updatedAt = unixepoch()
                     WHERE id = ?1 AND status = 'running'",
                    [job_id],
                )?;
                limited = true;
                break;
            }
            let (_, _, pending) = jobq::stats(conn, "ocr")?;
            let index = report.processed + 1;
            let mut progress = |status: &str, error: Option<String>, report: &mut OcrRunReport| {
                progress(OcrProgress {
                    jobId: *job_id,
                    assetId: *asset_id,
                    status: status.into(),
                    index,
                    total: pending as usize + index,
                    doneCount: report.done,
                    failedCount: report.failed,
                    error,
                });
            };
            if !eligible(conn, *asset_id)? {
                jobq::complete(conn, *job_id)?;
                report.skipped += 1;
                progress("skipped", None, &mut report);
                report.processed += 1;
                continue;
            }
            match ocr_one(conn, &engine, *asset_id) {
                Ok(_) => {
                    jobq::complete(conn, *job_id)?;
                    report.done += 1;
                    progress("done", None, &mut report);
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
                            report.errors.push(format!("{asset_id}: {msg}"));
                        }
                    } else {
                        report.retry += 1;
                    }
                    progress(
                        if status == "failed" {
                            "failed"
                        } else {
                            "retry"
                        },
                        Some(msg),
                        &mut report,
                    );
                }
            }
            report.processed += 1;
        }
        if report.cancelled || limited {
            break;
        }
    }

    let (d, f, p) = jobq::stats(conn, "ocr")?;
    report.jobDone = d;
    report.jobFailed = f;
    report.jobPending = p;
    Ok(report)
}
