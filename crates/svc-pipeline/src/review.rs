//! E5 — 打标交互式确认流（两段式）：AI 打标结果先暂存 `tag_proposal`（预览/修正/跳过），
//! 确认后才搬进正式 `assets`/`tag` 表；跳过的图零写入。
//!
//! 与 D4 直落队列（tag_queue / custom_queue）的关系：web 侧改走本模块的 review 版
//! runner（同样的 jobq 骨架：requeue_stale / 64 claim / 失败重试 / 取消回 pending），
//! 差异只在「落库」一步换成 `stage_*` 暂存。桌面版（app-tauri）仍用直落 runner，不受影响。
//!
//! 语义裁决：
//! * 暂存区逐资产整批替换（同一资产重跑打标 → 旧提案被新提案覆盖，人工修正以提交为准）；
//! * 确认时 `edited=1` 的行按人工数据落库（tag origin='manual' / 字段覆写）；
//!   未修改行维持原语义（auto 不覆盖 manual；custom 同任务重跑可更新 custom 行）;
//! * 增量入队（enq_untagged_review）排除已有暂存的资产——待确认的图不会重复打标。

use std::sync::atomic::{AtomicBool, Ordering};

use core_types::{TagProgress, TagRunReport};
use engine_tag::{TagProvider, TaggingOutput};
use rusqlite::Connection;

use crate::custom_queue::CUSTOM_TAG_BATCH;
use crate::tag_queue::TAG_BATCH;
use crate::{PipelineError, Result};
use core_storage::custom_tasks::CUSTOM_TAG_KIND;
use engine_jobq::{self as jobq};

const TAG_KIND: &str = "tag";

const ELIGIBLE: &str = "deleted = 0 AND missing = 0 AND mediaType = 'image'";

// ---------- 暂存（runner 落库步） ----------

/// 模板打标结果暂存：整批替换该资产的提案（title/titleEn/description 非空才入）。
/// 返回插入的提案行数。
pub fn stage_template(
    conn: &Connection,
    model: &str,
    asset_id: i64,
    out: &TaggingOutput,
) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM tag_proposal WHERE assetId = ?1", [asset_id])?;
    let mut n = 0usize;
    for (kind, text) in [
        ("title", out.title_ch.trim()),
        ("titleEn", out.title_en.trim()),
        ("description", out.description.trim()),
    ] {
        if text.is_empty() {
            continue;
        }
        n += tx.execute(
            "INSERT INTO tag_proposal (assetId, kind, name, value, model, score, createdAt)
             VALUES (?1, ?2, '', ?3, ?4, 1.0, unixepoch())",
            rusqlite::params![asset_id, kind, text, model],
        )?;
    }
    for t in &out.tags {
        let name = t.name.trim();
        if name.is_empty() {
            continue;
        }
        n += tx.execute(
            "INSERT INTO tag_proposal (assetId, kind, name, value, model, score, createdAt)
             VALUES (?1, 'tag', ?2, '', ?3, ?4, unixepoch())",
            rusqlite::params![asset_id, name, model, t.score],
        )?;
    }
    tx.commit()?;
    Ok(n)
}

/// 自定义任务结果暂存：按 (asset, taskId) 整批替换，value 列存**数组快照 JSON**
/// （统一标签：服务端已直出数组；旧分隔符原文在确认侧用 proposal_values 兜底解析）。
/// 空数组约定与直落一致（无该维度内容 → 清掉旧提案，不占确认位）。
pub fn stage_custom_values(
    conn: &Connection,
    model: &str,
    asset_id: i64,
    task_id: i64,
    values: &[String],
) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM tag_proposal WHERE assetId = ?1 AND kind = 'tag' AND taskId = ?2",
        rusqlite::params![asset_id, task_id],
    )?;
    let n = if values.is_empty() {
        0
    } else {
        let snapshot = serde_json::to_string(values)
            .map_err(|e| core_storage::StorageError::Invalid(format!("数组快照序列化失败: {e}")))?;
        tx.execute(
            "INSERT INTO tag_proposal (assetId, kind, name, value, model, taskId, score, createdAt)
             VALUES (?1, 'tag', (SELECT name FROM custom_tag_task WHERE id = ?2), ?3, ?4, ?2, 1.0, unixepoch())",
            rusqlite::params![asset_id, task_id, snapshot, model],
        )?
    };
    tx.commit()?;
    Ok(n)
}

/// 提案 value 列 → 值数组（确认侧统一入口）：数组快照 JSON 直接解析；
/// 旧数据（分隔符原文/人工改过的自由文本）退回 split_values。每值 normalize + 去重。
fn proposal_values(value: &str) -> Vec<String> {
    let t = value.trim();
    let mut raw: Vec<String> = Vec::new();
    if let (Some(start), Some(end)) = (t.find('['), t.rfind(']')) {
        if end > start {
            if let Ok(serde_json::Value::Array(arr)) = serde_json::from_str(&t[start..=end]) {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        raw.push(s.to_string());
                    }
                }
                return normalize_values(raw);
            }
        }
    }
    raw.extend(core_storage::tags::split_values(t));
    normalize_values(raw)
}

fn normalize_values(values: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for v in values {
        if let Some(n) = core_storage::tags::normalize_tag_value(&v) {
            if !out.iter().any(|x| *x == n) {
                out.push(n);
            }
        }
    }
    out
}

// ---------- review 版 runner（与 tag_queue/custom_queue 同骨架，落库步换 stage_*） ----------

fn emit(
    job_id: i64,
    asset_id: i64,
    status: &str,
    index: usize,
    pending: i64,
    report: &TagRunReport,
    error: Option<String>,
) -> TagProgress {
    TagProgress {
        jobId: job_id,
        assetId: asset_id,
        status: status.into(),
        index,
        total: pending as usize + index,
        doneCount: report.done,
        failedCount: report.failed,
        error,
    }
}

fn requeue_running(conn: &Connection, job_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE job_queue SET status = 'pending', updatedAt = unixepoch()
         WHERE id = ?1 AND status = 'running'",
        [job_id],
    )?;
    Ok(())
}

fn asset_abs_path(conn: &Connection, asset_id: i64) -> std::result::Result<String, PipelineError> {
    conn.query_row(
        &format!("SELECT absPath FROM assets WHERE assetId = ?1 AND {ELIGIBLE}"),
        [asset_id],
        |r| r.get(0),
    )
    .map_err(|_| PipelineError::AssetNotFound(asset_id))
}

/// 模板队列 review 版：推理后暂存提案（不写 assets/tag）。
/// `limit > 0` 时本轮最多处理 limit 个 job（与 tag_queue_with_limit 同语义）。
pub fn run_tag_queue_review_with_limit(
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
    let model = provider.spec().name.clone();
    let mut report = TagRunReport::default();
    let mut limited = false;
    loop {
        if cancel.load(Ordering::SeqCst) {
            report.cancelled = true;
            break;
        }
        let batch = jobq::claim_batch(conn, TAG_KIND, TAG_BATCH)?;
        if batch.is_empty() {
            break;
        }
        for (job_id, asset_id, _) in &batch {
            if cancel.load(Ordering::SeqCst) {
                requeue_running(conn, *job_id)?;
                report.cancelled = true;
                break;
            }
            if limit > 0 && report.processed >= limit {
                requeue_running(conn, *job_id)?;
                limited = true;
                break;
            }
            let (_, _, pending) = jobq::stats(conn, TAG_KIND)?;
            let index = report.processed + 1;
            let eligible: bool = conn.query_row(
                &format!("SELECT COUNT(*) FROM assets WHERE assetId = ?1 AND {ELIGIBLE}"),
                [asset_id],
                |r| r.get::<_, i64>(0),
            )? > 0;
            if !eligible {
                jobq::complete(conn, *job_id)?;
                report.skipped += 1;
                progress(emit(
                    *job_id, *asset_id, "skipped", index, pending, &report, None,
                ));
                report.processed += 1;
                continue;
            }
            let outcome = (|| -> Result<bool> {
                let path = asset_abs_path(conn, *asset_id)?;
                if !std::path::Path::new(&path).is_file() {
                    return Err(PipelineError::NoImage(*asset_id, path));
                }
                let out = provider.tag_image(std::path::Path::new(&path))?;
                stage_template(conn, &model, *asset_id, &out)?;
                Ok(out.repaired)
            })();
            match outcome {
                Ok(repaired) => {
                    jobq::complete(conn, *job_id)?;
                    report.done += 1;
                    if repaired {
                        report.repaired += 1;
                    }
                    progress(emit(
                        *job_id, *asset_id, "done", index, pending, &report, None,
                    ));
                }
                Err(e) => {
                    let msg = format!("{e}");
                    jobq::fail(conn, *job_id, &msg)?;
                    report.failed += 1;
                    if report.errors.len() < 50 {
                        report.errors.push(format!("资产{asset_id}: {msg}"));
                    }
                    progress(emit(
                        *job_id,
                        *asset_id,
                        "failed",
                        index,
                        pending,
                        &report,
                        Some(msg),
                    ));
                }
            }
            report.processed += 1;
        }
        if limited {
            break;
        }
    }
    Ok(report)
}

/// 自定义队列 review 版：推理后暂存提案（不写 tag 表）。
pub fn run_custom_queue_review_with_limit(
    conn: &Connection,
    provider: &dyn TagProvider,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(TagProgress),
    limit: usize,
) -> Result<TagRunReport> {
    use core_storage::custom_tasks::{self as tasks, parse_custom_payload};

    if !provider.ready() {
        return Err(PipelineError::ProviderNotReady(
            provider.spec().name.clone(),
        ));
    }
    let _ = jobq::requeue_stale(conn)?;
    let model = provider.spec().name.clone();
    let mut report = TagRunReport::default();
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
                requeue_running(conn, *job_id)?;
                report.cancelled = true;
                break;
            }
            if limit > 0 && report.processed >= limit {
                requeue_running(conn, *job_id)?;
                limited = true;
                break;
            }
            let (_, _, pending) = jobq::stats(conn, CUSTOM_TAG_KIND)?;
            let index = report.processed + 1;
            // 任务已删/停用 → 直接完成（跳过，不产生提案）
            let task = parse_custom_payload(payload.as_deref())
                .and_then(|id| tasks::get_task(conn, id).ok().flatten());
            let Some(task) = task.filter(|t| t.enabled) else {
                jobq::complete(conn, *job_id)?;
                report.skipped += 1;
                progress(emit(
                    *job_id, *asset_id, "skipped", index, pending, &report, None,
                ));
                report.processed += 1;
                continue;
            };
            let outcome = (|| -> Result<()> {
                let path = asset_abs_path(conn, *asset_id)?;
                if !std::path::Path::new(&path).is_file() {
                    return Err(PipelineError::NoImage(*asset_id, path));
                }
                let values = provider.tag_inline_values(std::path::Path::new(&path), &task.prompt)?;
                stage_custom_values(conn, &model, *asset_id, task.id, &values)?;
                Ok(())
            })();
            match outcome {
                Ok(()) => {
                    jobq::complete(conn, *job_id)?;
                    report.done += 1;
                    progress(emit(
                        *job_id, *asset_id, "done", index, pending, &report, None,
                    ));
                }
                Err(e) => {
                    let msg = format!("{e}");
                    jobq::fail(conn, *job_id, &msg)?;
                    report.failed += 1;
                    if report.errors.len() < 50 {
                        report
                            .errors
                            .push(format!("任务[{}] 资产{asset_id}: {msg}", task.name));
                    }
                    progress(emit(
                        *job_id,
                        *asset_id,
                        "failed",
                        index,
                        pending,
                        &report,
                        Some(msg),
                    ));
                }
            }
            report.processed += 1;
        }
        if limited {
            break;
        }
    }
    Ok(report)
}

// ---------- 入队 / 统计 ----------

/// 增量入队（review 版）：未打标 且 没有待确认提案的资产才入队。
pub fn enq_untagged_review(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        &format!(
            "INSERT INTO job_queue (kind, targetId, payload, status, createdAt, updatedAt)
             SELECT '{TAG_KIND}', assetId, NULL, 'pending', unixepoch(), unixepoch() FROM assets
             WHERE assetId IN (
                 SELECT assetId FROM assets WHERE {ELIGIBLE}
                 EXCEPT SELECT assetId FROM tag WHERE origin = 'auto'
                 EXCEPT SELECT assetId FROM tag_proposal
             )
             AND NOT EXISTS (SELECT 1 FROM job_queue j WHERE j.kind = '{TAG_KIND}' AND j.targetId = assets.assetId)"
        ),
        [],
    )?)
}

/// 确认流统计：(待确认资产数, 提案行数)。
pub fn review_stats(conn: &Connection) -> Result<(i64, i64)> {
    let assets: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT assetId) FROM tag_proposal",
        [],
        |r| r.get(0),
    )?;
    let rows: i64 = conn.query_row("SELECT COUNT(*) FROM tag_proposal", [], |r| r.get(0))?;
    Ok((assets, rows))
}

// ---------- 确认页读模型 ----------

/// 单条提案视图。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewItemView {
    pub id: i64,
    pub kind: String,
    pub name: String,
    pub value: String,
    pub model: Option<String>,
    #[serde(rename = "taskId")]
    pub task_id: Option<i64>,
    pub task_name: Option<String>,
    pub score: f64,
    pub edited: bool,
}

/// 单资产提案组（确认页一张卡）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewAssetView {
    pub asset_id: i64,
    pub file_name: String,
    pub items: Vec<ReviewItemView>,
}

/// GET review 列表：分页按资产（limit = 资产卡数），组内 title → titleEn → description → tag 排序。
pub fn review_items(
    conn: &Connection,
    limit: i64,
    offset: i64,
) -> Result<(i64, Vec<ReviewAssetView>)> {
    let total = review_stats(conn)?.0;
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT assetId FROM tag_proposal ORDER BY assetId LIMIT ?1 OFFSET ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit, offset], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if ids.is_empty() {
        return Ok((total, Vec::new()));
    }
    let in_clause = ids
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT p.id, p.assetId, p.kind, p.name, p.value, p.model, p.taskId, p.score, p.edited,
                a.fileName, t.name
         FROM tag_proposal p
         JOIN assets a ON a.assetId = p.assetId
         LEFT JOIN custom_tag_task t ON t.id = p.taskId
         WHERE p.assetId IN ({in_clause})
         ORDER BY p.assetId, CASE p.kind WHEN 'title' THEN 0 WHEN 'titleEn' THEN 1
                                          WHEN 'description' THEN 2 ELSE 3 END, p.id"
    ))?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, Option<i64>>(6)?,
            r.get::<_, f64>(7)?,
            r.get::<_, i64>(8)? != 0,
            r.get::<_, String>(9)?,
            r.get::<_, Option<String>>(10)?,
        ))
    })?;
    let mut out: Vec<ReviewAssetView> = Vec::new();
    for row in rows {
        let (id, asset_id, kind, name, value, model, task_id, score, edited, file_name, task_name) =
            row?;
        match out.last_mut() {
            Some(g) if g.asset_id == asset_id => {}
            _ => out.push(ReviewAssetView {
                asset_id,
                file_name,
                items: Vec::new(),
            }),
        }
        out.last_mut().unwrap().items.push(ReviewItemView {
            id,
            kind,
            name,
            value,
            model,
            task_id,
            task_name,
            score,
            edited,
        });
    }
    Ok((total, out))
}

// ---------- 修正 / 确认 / 跳过 ----------

/// PUT 单资产修正项。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewUpdateField {
    pub id: i64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
}

/// 新增提案（确认页「添加标签」）。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewAddField {
    pub kind: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewUpdateReq {
    #[serde(default)]
    pub deletes: Vec<i64>,
    #[serde(default)]
    pub updates: Vec<ReviewUpdateField>,
    #[serde(default)]
    pub adds: Vec<ReviewAddField>,
}

/// 应用单资产修正：删除提案 / 改名改值（内容真变了才置 edited=1）/ 新增（edited=1）。
pub fn review_update(conn: &Connection, asset_id: i64, req: &ReviewUpdateReq) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    for id in &req.deletes {
        tx.execute(
            "DELETE FROM tag_proposal WHERE id = ?1 AND assetId = ?2",
            rusqlite::params![id, asset_id],
        )?;
    }
    for u in &req.updates {
        let n = tx.execute(
            "UPDATE tag_proposal SET
               name = COALESCE(?2, name),
               value = COALESCE(?3, value),
               edited = CASE WHEN (?2 IS NOT NULL AND name <> ?2)
                                OR (?3 IS NOT NULL AND value <> ?3)
                             THEN 1 ELSE edited END
             WHERE id = ?1 AND assetId = ?4",
            rusqlite::params![u.id, u.name, u.value, asset_id],
        )?;
        let _ = n;
    }
    for a in &req.adds {
        if a.kind != "tag" {
            return Err(core_storage::StorageError::Invalid(format!(
                "仅支持新增标签提案，got kind={}",
                a.kind
            ))
            .into());
        }
        let name = a.name.trim();
        if name.is_empty() {
            return Err(core_storage::StorageError::Invalid("标签名不能为空".into()).into());
        }
        tx.execute(
            "INSERT INTO tag_proposal (assetId, kind, name, value, score, edited, createdAt)
             VALUES (?1, 'tag', ?2, ?3, 1.0, 1, unixepoch())
             ON CONFLICT(assetId, kind, name) DO UPDATE SET value = excluded.value, edited = 1",
            rusqlite::params![asset_id, name, a.value.trim()],
        )?;
    }
    tx.commit()?;
    Ok(())
}

#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCommitReport {
    pub assets: usize,
    pub tags: usize,
    pub fields: usize,
}

/// 确认入库：把选中资产的提案搬进正式表（edited 行按人工数据，其余维持原语义），
/// 然后清掉这些资产的提案。`all=true` 忽略 assetIds 全量确认。
pub fn review_commit(
    conn: &Connection,
    asset_ids: &[i64],
    all: bool,
) -> Result<ReviewCommitReport> {
    let mut rep = ReviewCommitReport::default();
    let tx = conn.unchecked_transaction()?;
    let ids: Vec<i64> = if all {
        tx.query_row(
            "SELECT COALESCE(group_concat(assetId), '') FROM (SELECT DISTINCT assetId FROM tag_proposal)",
            [],
            |r| r.get::<_, String>(0),
        )
        .map(|s| {
            s.split(',')
                .filter(|s| !s.is_empty())
                .filter_map(|s| s.parse().ok())
                .collect()
        })?
    } else {
        asset_ids.to_vec()
    };
    for asset_id in &ids {
        let mut stmt = tx.prepare(
            "SELECT id, kind, name, value, model, taskId, score, edited
             FROM tag_proposal WHERE assetId = ?1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map([asset_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, f64>(6)?,
                    r.get::<_, i64>(7)? != 0,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for (_, kind, name, value, model, task_id, score, edited) in rows {
            match kind.as_str() {
                "title" | "titleEn" | "description" => {
                    let col = match kind.as_str() {
                        "title" => "title_ch",
                        "titleEn" => "title_en",
                        _ => "description",
                    };
                    let sql = if edited {
                        format!("UPDATE assets SET {col} = ?1 WHERE assetId = ?2")
                    } else {
                        // 未修改：维持「只填空字段」语义（人工/已有内容优先）
                        format!("UPDATE assets SET {col} = CASE WHEN {col} IS NULL OR TRIM({col}) = '' THEN ?1 ELSE {col} END WHERE assetId = ?2")
                    };
                    tx.execute(&sql, rusqlite::params![value, asset_id])?;
                    rep.fields += 1;
                }
                _ => {
                    if let Some(task_id) = task_id {
                        // 打标任务提案：结果表记维度名 + 输出原文（数组快照或旧原文），
                        // 非空值统一扇出到 tag（normalize + key-val 拍平，t_d65d55a1）
                        core_storage::tags::write_tag_result_row(
                            &tx,
                            *asset_id,
                            task_id,
                            &name,
                            &value,
                            model.as_deref().unwrap_or(""),
                        )?;
                        let values = proposal_values(&value);
                        if edited {
                            // 人工改过 → 按人工数据落库：旧 custom 标签清掉，
                            // 同规则拍平后写 manual（人工优先，后续 auto/custom 不覆盖）
                            core_storage::tags::clear_custom_tags(&tx, *asset_id, task_id)?;
                            for v in values {
                                let kv = core_storage::tags::flatten_key_value(&name, &v);
                                core_storage::tags::upsert_tag(
                                    &tx, *asset_id, &kv, "manual", None, None, 1.0,
                                )?;
                            }
                        } else if !values.is_empty() {
                            core_storage::tags::fanout_values(
                                &tx,
                                *asset_id,
                                task_id,
                                &name,
                                &values,
                                model.as_deref().unwrap_or(""),
                            )?;
                        }
                    } else {
                        // 模板提案：name 就是标签值。
                        // 人工改过 → manual（后续 auto 不得覆盖）；未改 → auto（不覆盖 manual/path/custom）
                        let origin = if edited { "manual" } else { "auto" };
                        let (m, s) = if edited {
                            (None, 1.0)
                        } else {
                            (model.as_deref(), score)
                        };
                        for v in core_storage::tags::split_values(&name) {
                            core_storage::tags::upsert_tag(
                                &tx, *asset_id, &v, origin, m, None, s,
                            )?;
                        }
                    }
                    rep.tags += 1;
                }
            }
        }
        tx.execute("DELETE FROM tag_proposal WHERE assetId = ?1", [asset_id])?;
        rep.assets += 1;
    }
    tx.commit()?;
    Ok(rep)
}

/// 跳过：只清提案，原图零改动（返回删除的提案行数）。`all=true` 忽略 assetIds。
pub fn review_discard(conn: &Connection, asset_ids: &[i64], all: bool) -> Result<usize> {
    let n = if all {
        conn.execute("DELETE FROM tag_proposal", [])?
    } else {
        let mut total = 0usize;
        for id in asset_ids {
            total += conn.execute("DELETE FROM tag_proposal WHERE assetId = ?1", [id])?;
        }
        total
    };
    // 受影响资产数：按提案行数近似（每资产 ≥1 行；空提案资产不会出现）
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_tag::TagItem;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&c).unwrap();
        c
    }

    fn add_asset(c: &Connection, name: &str) -> i64 {
        c.execute(
            "INSERT INTO assets (mediaType, absPath, storeMode, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES ('image', ?1, 'reference', ?1, 'jpg', 1, 0, 0)",
            [name],
        )
        .unwrap();
        c.last_insert_rowid()
    }

    fn out(tags: &[(&str, f32)]) -> TaggingOutput {
        TaggingOutput {
            title_ch: "黑背犬".into(),
            title_en: "German Shepherd".into(),
            description: "一只坐在草地上的黑背犬".into(),
            tags: tags
                .iter()
                .map(|(n, s)| TagItem {
                    name: n.to_string(),
                    score: *s,
                    origin: "auto".into(),
                })
                .collect(),
            repaired: false,
        }
    }

    #[test]
    fn staging_does_not_touch_library() {
        let c = db();
        let a = add_asset(&c, "dog.jpg");
        stage_template(&c, "m1", a, &out(&[("狗", 0.9), ("草地", 0.4)])).unwrap();
        // 两段式保证：暂存不写正式表
        let tags: i64 = c
            .query_row("SELECT COUNT(*) FROM tag", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tags, 0);
        let title: Option<String> = c
            .query_row("SELECT title_ch FROM assets WHERE assetId = ?1", [a], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(title, None);
        let (assets, rows) = review_stats(&c).unwrap();
        assert_eq!((assets, rows), (1, 5)); // 3 字段提案 + 2 标签提案
    }

    #[test]
    fn commit_unchanged_vs_edited_vs_discard() {
        let c = db();
        let a1 = add_asset(&c, "a.jpg");
        let a2 = add_asset(&c, "b.jpg");
        let a3 = add_asset(&c, "c.jpg");
        stage_template(&c, "m1", a1, &out(&[("狗", 0.9)])).unwrap();
        stage_template(&c, "m1", a2, &out(&[("猫", 0.9)])).unwrap();
        stage_template(&c, "m1", a3, &out(&[("鸟", 0.9)])).unwrap();

        // a2 人工修正：改标签 + 改标题
        let (total, items) = review_items(&c, 50, 0).unwrap();
        assert_eq!(total, 3);
        let card = items.iter().find(|g| g.asset_id == a2).unwrap();
        let tag = card.items.iter().find(|i| i.kind == "tag").unwrap();
        let title = card.items.iter().find(|i| i.kind == "title").unwrap();
        review_update(
            &c,
            a2,
            &ReviewUpdateReq {
                deletes: vec![],
                updates: vec![
                    ReviewUpdateField {
                        id: tag.id,
                        name: Some("橘猫".into()),
                        value: None,
                    },
                    ReviewUpdateField {
                        id: title.id,
                        name: None,
                        value: Some("手改标题".into()),
                    },
                ],
                adds: vec![],
            },
        )
        .unwrap();

        // a3 跳过
        review_discard(&c, &[a3], false).unwrap();

        // 全部确认
        let rep = review_commit(&c, &[], true).unwrap();
        assert_eq!(rep.assets, 2);

        // a1 未修改：auto + 只填空字段
        let (origin, score): (String, f64) = c
            .query_row(
                "SELECT origin, score FROM tag WHERE assetId = ?1 AND value = '狗'",
                [a1],
                |r| r.get(0).map(|o: String| (o, r.get(1).unwrap())),
            )
            .unwrap();
        assert_eq!(origin, "auto");
        assert!((score - 0.9).abs() < 1e-6);
        let t1: String = c
            .query_row(
                "SELECT title_ch FROM assets WHERE assetId = ?1",
                [a1],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(t1, "黑背犬");

        // a2 修正：manual 落库 + 字段覆写
        let origin: String = c
            .query_row(
                "SELECT origin FROM tag WHERE assetId = ?1 AND value = '橘猫'",
                [a2],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(origin, "manual");
        let old: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM tag WHERE assetId = ?1 AND value = '猫'",
                [a2],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old, 0, "被改名标签以新值落库，旧提案值不落地");
        let t2: String = c
            .query_row(
                "SELECT title_ch FROM assets WHERE assetId = ?1",
                [a2],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(t2, "手改标题");

        // a3 跳过：零写入
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM tag WHERE assetId = ?1", [a3], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 0);
        let t3: Option<String> = c
            .query_row(
                "SELECT title_ch FROM assets WHERE assetId = ?1",
                [a3],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(t3, None);
        let left: i64 = c
            .query_row("SELECT COUNT(*) FROM tag_proposal", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn enq_review_excludes_pending_proposals_and_rerun_replaces_staging() {
        let c = db();
        let a1 = add_asset(&c, "a.jpg");
        let a2 = add_asset(&c, "b.jpg");
        assert_eq!(enq_untagged_review(&c).unwrap(), 2);
        stage_template(&c, "m1", a1, &out(&[("狗", 0.9)])).unwrap();
        // 模拟 a1 的 job 已消费完（提案已生成）；a2 还挂着 pending job
        c.execute(
            "DELETE FROM job_queue WHERE kind = 'tag' AND targetId = ?1",
            [a1],
        )
        .unwrap();
        // a1 已有提案 → 不再入队；a2 已有 pending job → 也不重复入队
        assert_eq!(enq_untagged_review(&c).unwrap(), 0);
        // a2 的 job 消费完毕后重入队：只补 a2，带提案的 a1 不回来
        c.execute(
            "DELETE FROM job_queue WHERE kind = 'tag' AND targetId = ?1",
            [a2],
        )
        .unwrap();
        assert_eq!(enq_untagged_review(&c).unwrap(), 1);
        let a1_jobs: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM job_queue WHERE kind = 'tag' AND targetId = ?1",
                [a1],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(a1_jobs, 0);
        // 同资产重跑 → 整批替换（旧行不残留）
        stage_template(&c, "m2", a1, &out(&[("牧羊犬", 0.8)])).unwrap();
        let names: Vec<String> = {
            let mut stmt = c
                .prepare("SELECT name FROM tag_proposal WHERE assetId = ?1 AND kind = 'tag'")
                .unwrap();
            stmt.query_map([a1], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(names, vec!["牧羊犬".to_string()]);
    }

    #[test]
    fn custom_stage_commit_and_empty_value_convention() {
        use core_storage::custom_tasks;
        let c = db();
        let a = add_asset(&c, "a.jpg");
        let tid = custom_tasks::create_task(&c, "地点", "图中地点?", true).unwrap();
        // 统一标签：暂存存数组快照，确认扇出拍平成 key-val
        stage_custom_values(&c, "m1", a, tid, &["室内".to_string()]).unwrap();
        let rep = review_commit(&c, &[a], false).unwrap();
        assert_eq!(rep.assets, 1);
        let (origin, value, task): (String, String, i64) = c
            .query_row(
                "SELECT origin, value, taskId FROM tag WHERE assetId = ?1 AND value = '地点-室内'",
                [a],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2).unwrap())),
            )
            .unwrap();
        assert_eq!((origin.as_str(), value.as_str()), ("custom", "地点-室内"));
        assert_eq!(task, tid);
        // 维度名与输出原文（数组快照）留在结果表
        let (dim, raw): (String, String) = c
            .query_row(
                "SELECT name, value FROM tag_result WHERE assetId = ?1 AND taskId = ?2",
                [a, tid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((dim.as_str(), raw.as_str()), ("地点", "[\"室内\"]"));
        // 空数组 → 清提案不留确认位
        stage_custom_values(&c, "m1", a, tid, &[]).unwrap();
        let n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM tag_proposal WHERE assetId = ?1",
                [a],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    /// 统一标签确认侧回归（t_d65d55a1）：旧分隔符提案与人工修正两条路都拍平；
    /// 人工修正行升级 manual（不被重扇出覆盖）。
    #[test]
    fn custom_commit_accepts_legacy_value_and_edited_flattens_manual() {
        use core_storage::custom_tasks;
        let c = db();
        let a = add_asset(&c, "a.jpg");
        let tid = custom_tasks::create_task(&c, "色调", "主色调", true).unwrap();
        // 旧数据形态：提案 value 是分隔符原文（升级前的暂存）
        c.execute(
            "INSERT INTO tag_proposal (assetId, kind, name, value, model, taskId, score, createdAt)
             VALUES (?1, 'tag', '色调', '暖 橙;冷蓝', 'm1', ?2, 1.0, unixepoch())",
            rusqlite::params![a, tid],
        )
        .unwrap();
        review_commit(&c, &[a], false).unwrap();
        let vals: Vec<String> = c
            .prepare("SELECT value FROM tag WHERE assetId = ?1 ORDER BY value")
            .unwrap()
            .query_map([a], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            vals,
            vec!["色调-冷蓝".to_string(), "色调-暖_橙".to_string()],
            "旧原文按分隔符拆 + 归一 + 拍平"
        );

        // 人工修正：改值后确认 → edited 分支先清该任务旧 custom 标签再写 manual
        // （人工优先；重跑/重确认都不残留旧 custom 值）
        c.execute(
            "INSERT INTO tag_proposal (assetId, kind, name, value, model, taskId, score, edited, createdAt)
             VALUES (?1, 'tag', '色调', '[\"极简\"]', 'm1', ?2, 1.0, 1, unixepoch())",
            rusqlite::params![a, tid],
        )
        .unwrap();
        review_commit(&c, &[a], false).unwrap();
        let rows: Vec<(String, String)> = c
            .prepare("SELECT origin, value FROM tag WHERE assetId = ?1 ORDER BY value")
            .unwrap()
            .query_map([a], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            rows,
            vec![("manual".to_string(), "色调-极简".to_string())],
            "edited 行拍平写 manual；该任务旧 custom 行被清掉（人工接管）"
        );
        // 结果表仍留档（最后一次输出）
        let raw: String = c
            .query_row(
                "SELECT value FROM tag_result WHERE assetId = ?1 AND taskId = ?2",
                [a, tid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(raw, "[\"极简\"]");
    }
}
