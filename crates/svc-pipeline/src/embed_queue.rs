//! D5 — 嵌入队列（双塔，2026-10-13 裁决）：`job_queue`(kind='embed:<model>') 驱动，
//! 向量直接落 `core_storage::vecstore`（v_<model> 分表）。
//!
//! 语义：
//! - **双塔互不阻塞**：语义塔（siglip，`emb_status` 状态机，搜索可用）与
//!   去重塔（dinov3，`dedup_status` 状态机，查重可用）各自独立入队/消费。
//! - job kind 按 model 区分：`embed:siglip2-base-p224-onnx` / `embed:dinov3-vits16-onnx`。
//! - 断点续跑 / 取消 / 3 次重试 / 幂等入队，语义与 D4 tag_queue 一致。
//! - 单图粒度失败隔离：逐 job 调 provider（session 已缓存，无重复加载开销）；
//!   成功 → 向量 upsert + 状态 done + job complete。
//! - 禁混算铁律：向量只进 provider.spec().name 对应的表，路由由 kind 固化。
//! - **入库即算（KI-2a）**：`enq_import_batch` 让本批新增资产同时进两条队列
//!   （语义塔 active_embedder + 写死的判重塔 `core_config::DEDUP_EMBEDDER`）；
//!   存量库用 `enq_never_embedded` 幂等 backfill（`dedup_status IS NULL`）。
//!   两塔预处理参数由 registry 锁定（siglip 224 / dinov3 256），各 provider 各自解码，
//!   **不共用预处理张量**。

use std::sync::atomic::{AtomicBool, Ordering};

use core_types::{
    EmbedFailedItem, EmbedProgress, EmbedQueueStats, EmbedRunReport, EmbedScopeStats,
};
use rusqlite::Connection;

use crate::{PipelineError, Result};
use engine_embed::EmbedProvider;
use engine_jobq::{self as jobq};

/// 每批 claim 的 job 数。
pub const EMBED_BATCH: i64 = 64;

/// 可嵌入资产条件（与 D3 口径一致：图片、未删、未失联）。
const ELIGIBLE: &str = "deleted = 0 AND missing = 0 AND mediaType = 'image'";

/// 双塔中的哪一侧。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedScope {
    /// 语义塔：emb_status 状态机（搜索可用）
    Semantic,
    /// 去重塔：dedup_status 状态机（查重可用）
    Dedup,
}

impl EmbedScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            EmbedScope::Semantic => "semantic",
            EmbedScope::Dedup => "dedup",
        }
    }
    /// 该侧状态列（assets 表）。
    fn status_col(&self) -> &'static str {
        match self {
            EmbedScope::Semantic => "emb_status",
            EmbedScope::Dedup => "dedup_status",
        }
    }
    fn model_col(&self) -> &'static str {
        match self {
            EmbedScope::Semantic => "emb_model",
            EmbedScope::Dedup => "dedup_model",
        }
    }
    fn at_col(&self) -> &'static str {
        match self {
            EmbedScope::Semantic => "emb_at",
            EmbedScope::Dedup => "dedup_at",
        }
    }
}

/// job kind = `embed:<spec.name>`（表路由固化在 kind 里）。
pub fn embed_kind(model: &str) -> String {
    format!("embed:{model}")
}

// ---------- KI-2b：向量判重 → `dup_edges`（关系表） ----------
//
// 判重塔的向量落库后，**同一次消费里**就把「这张图和库内哪几张是同一视觉内容」
// 落成持久关系（边），而不是等去重页每次进页面重算全库 KNN。表结构与语义见
// `migrations/0019_dup_edges.sql`；分组/裁决在 svc-library（按边并查集）。

/// 判重 KNN 邻居数（含自身；与 `engine_search::DupOptions::k_per_item` 默认一致）。
pub const DUP_K_NEIGHBORS: usize = 9;
/// 同一视觉内容阈值（与 `engine_search::NEAR_DUP_THRESHOLD` 同值 —— 单测里钉住防漂移）。
pub const DUP_NEAR_THRESHOLD: f32 = 0.98;
/// 疑似变体阈值（同图不同构图；与 `engine_search::VARIANT_THRESHOLD` 同值）。
pub const DUP_VARIANT_THRESHOLD: f32 = 0.90;
/// 补判账本键（`dup_judge_state.key`）。
pub const DUP_STATE_BACKFILL_DONE: &str = "backfill_done";
pub const DUP_STATE_BACKFILL_TRUNCATED: &str = "backfill_truncated";
pub const DUP_STATE_LAST_JUDGE_AT: &str = "last_judge_at";

/// 相似度 → 边类别；`None` = 低于变体阈值（不成边）。
pub fn dup_edge_kind(score: f32) -> Option<&'static str> {
    if !score.is_finite() {
        return None;
    }
    if score >= DUP_NEAR_THRESHOLD {
        Some("near_visual")
    } else if score >= DUP_VARIANT_THRESHOLD {
        Some("variant")
    } else {
        None
    }
}

fn set_judge_state(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO dup_judge_state (key, value, updatedAt) VALUES (?1, ?2, unixepoch())
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updatedAt = unixepoch()",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

fn judge_state_value(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM dup_judge_state WHERE key = ?1", [key], |r| {
            r.get::<_, String>(0)
        })
        .ok())
}

/// 判重进度（去重页三态徽标的**唯一事实源**）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DupJudgeStatus {
    pub model: String,
    /// 判重表内向量行数：0 = 判重塔还没跑过 → 「未检测」（**不得**呈现为「无重复」）
    pub vectors: i64,
    /// 已落边数（关系表的行数）
    pub edges: i64,
    /// 存量补判是否已走完一遍（可重入；中途停下为 false）
    pub backfill_done: bool,
    /// 上次补判是否因上限提前停下（true = 还有资产未判，再调一次续）
    pub backfill_truncated: bool,
    /// 最近一次增量判重时间（unixepoch）
    pub last_judge_at: Option<i64>,
}

/// 读判重进度。vectors == 0 → 未检测；backfill_done && !truncated → 已确认；其余 → 检测中。
pub fn dup_judge_status(conn: &Connection, model: &str) -> Result<DupJudgeStatus> {
    let vectors = core_storage::vecstore::tables(conn)?
        .iter()
        .any(|(m, _, _)| m == model)
        .then(|| core_storage::vecstore::count(conn, model).unwrap_or(0))
        .unwrap_or(0);
    let edges: i64 = conn.query_row("SELECT COUNT(*) FROM dup_edges", [], |r| r.get(0))?;
    Ok(DupJudgeStatus {
        model: model.to_string(),
        vectors,
        edges,
        backfill_done: judge_state_value(conn, DUP_STATE_BACKFILL_DONE)?.as_deref() == Some("1"),
        backfill_truncated: judge_state_value(conn, DUP_STATE_BACKFILL_TRUNCATED)?.as_deref()
            == Some("1"),
        last_judge_at: judge_state_value(conn, DUP_STATE_LAST_JUDGE_AT)?
            .and_then(|v| v.parse::<i64>().ok()),
    })
}

/// 单资产判重：取该资产已落库的判重向量 → KNN 最近邻 → 写 `dup_edges`（幂等可重算）。
///
/// 语义（决定去重页看到什么，改之前先读 `migrations/0019_dup_edges.sql`）：
/// - **先删后写**：删掉该资产涉及的所有旧边再按当前向量重写 —— 重嵌入/换邻居集后自愈，
///   不会留下「已经不相似了边还在」的历史结论；
/// - 只连**未删资产**（deleted=0）的 **primary** resource；回收站资产不参与成组
///   （恢复后由补判/重新嵌入再判）；
/// - KNN 的 k 含自身，故取 `DUP_K_NEIGHBORS + 1` 再剔除自己；
/// - 返回**写入的边数**（不含被阈值挡下的邻居）；无 primary resource / 无向量 → 0。
pub fn judge_asset_dup_edges(conn: &Connection, model: &str, asset_id: i64) -> Result<usize> {
    let rid: Option<i64> = conn
        .query_row(
            "SELECT resourceId FROM resource WHERE assetId = ?1 AND role = 'primary'",
            [asset_id],
            |r| r.get(0),
        )
        .ok();
    let Some(rid) = rid else { return Ok(0) };

    // 回收站里的资产不判（恢复后由补判/重新嵌入再判）——判了也进不了分组，
    // 只会在关系表里留下指向垃圾的边
    let deleted: i64 = conn
        .query_row("SELECT deleted FROM assets WHERE assetId = ?1", [asset_id], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    if deleted != 0 {
        return Ok(0);
    }

    // 表不存在 = 该模型还没落过向量（判重塔没跑）→ 无事可做，不造数
    if !core_storage::vecstore::tables(conn)?
        .iter()
        .any(|(m, _, _)| m == model)
    {
        return Ok(0);
    }
    let Some((_, query)) = core_storage::vecstore::fetch_vectors(conn, model, &[rid])?
        .into_iter()
        .next()
    else {
        return Ok(0);
    };

    let hits = core_storage::vecstore::knn(conn, model, &query, DUP_K_NEIGHBORS + 1)?;

    // 先删后写：本次判定就是该资产边的完整真相
    conn.execute("DELETE FROM dup_edges WHERE a = ?1 OR b = ?1", [asset_id])?;

    let mut written = 0usize;
    for (other_rid, distance) in hits {
        if other_rid == rid {
            continue;
        }
        let score = 1.0 - distance;
        let Some(kind) = dup_edge_kind(score) else {
            continue;
        };
        // 只连未删资产的 primary resource（回收站/失联资产不进关系）
        let other: Option<i64> = conn
            .query_row(
                "SELECT r.assetId FROM resource r JOIN assets a ON a.assetId = r.assetId
                 WHERE r.resourceId = ?1 AND r.role = 'primary' AND a.deleted = 0",
                [other_rid],
                |r| r.get(0),
            )
            .ok();
        let Some(other_id) = other else { continue };
        if other_id == asset_id {
            continue;
        }
        let (a, b) = if asset_id < other_id {
            (asset_id, other_id)
        } else {
            (other_id, asset_id)
        };
        conn.execute(
            "INSERT INTO dup_edges (a, b, score, kind, model, createdAt)
             VALUES (?1, ?2, ?3, ?4, ?5, unixepoch())
             ON CONFLICT(a, b) DO UPDATE SET
                 kind      = CASE WHEN excluded.score >= dup_edges.score
                                  THEN excluded.kind ELSE dup_edges.kind END,
                 score     = MAX(excluded.score, dup_edges.score),
                 model     = excluded.model,
                 createdAt = unixepoch()",
            rusqlite::params![a, b, score, kind, model],
        )?;
        written += 1;
    }
    let now: i64 = conn.query_row("SELECT unixepoch()", [], |r| r.get(0))?;
    set_judge_state(conn, DUP_STATE_LAST_JUDGE_AT, &now.to_string())?;
    Ok(written)
}

/// 存量补判报告。
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DupBackfillReport {
    /// 判重表内向量行数（0 = 判重塔还没跑过 → UI 应呈现「未检测」）
    pub vectors: usize,
    /// 本轮实际判过的资产数
    pub scanned: usize,
    /// 本轮结束后关系表的总边数（不是增量）
    pub edges: i64,
    /// 是否因 `max_assets` 提前停下（true = 还有资产未判，再调一次即续）
    pub truncated: bool,
}

/// 存量一次性补判（**可重入**）：把所有已有判重向量的资产过一遍 `judge_asset_dup_edges`。
///
/// - `max_assets = 0` → 不设上限全量；上限触发 → `truncated = true` 并落账本 flag
///   （UI/接口据此显示「检测中」而不是「无重复」）；
/// - 边写入幂等（先删后写 + 主键），所以重跑、并发重跑都不会造出重复/脏边；
/// - 判重表不存在（判重塔从未跑过）→ 直接返回空报告并把 `backfill_done` 置 1，
///   避免 UI 永远卡在「检测中」（此时正确状态是「未检测」，由 vectors == 0 表达）。
pub fn backfill_dup_edges(
    conn: &Connection,
    model: &str,
    max_assets: usize,
) -> Result<DupBackfillReport> {
    let mut report = DupBackfillReport::default();
    let has_table = core_storage::vecstore::tables(conn)?
        .iter()
        .any(|(m, _, _)| m == model);
    if !has_table {
        set_judge_state(conn, DUP_STATE_BACKFILL_DONE, "1")?;
        set_judge_state(conn, DUP_STATE_BACKFILL_TRUNCATED, "0")?;
        return Ok(report);
    }
    report.vectors = core_storage::vecstore::count(conn, model)?.max(0) as usize;

    let mut stmt = conn.prepare(&format!(
        "SELECT r.assetId FROM \"{}\" v
         JOIN resource r ON r.resourceId = v.resourceId
         JOIN assets a ON a.assetId = r.assetId AND a.deleted = 0
         WHERE r.role = 'primary' ORDER BY r.assetId",
        core_storage::vecstore::table_name(model)
    ))?;
    let ids: Vec<i64> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let budget = if max_assets == 0 {
        ids.len()
    } else {
        max_assets
    };
    for id in ids.iter().take(budget) {
        judge_asset_dup_edges(conn, model, *id)?;
        report.scanned += 1;
    }
    report.truncated = ids.len() > report.scanned;
    report.edges = conn.query_row("SELECT COUNT(*) FROM dup_edges", [], |r| r.get(0))?;
    set_judge_state(
        conn,
        DUP_STATE_BACKFILL_DONE,
        if report.truncated { "0" } else { "1" },
    )?;
    set_judge_state(
        conn,
        DUP_STATE_BACKFILL_TRUNCATED,
        if report.truncated { "1" } else { "0" },
    )?;
    Ok(report)
}


/// 未嵌入（状态 none/failed）入队。返回入队数。幂等（NOT EXISTS 防重复行）。
pub fn enq_unembedded(conn: &Connection, model: &str, scope: EmbedScope) -> Result<usize> {
    let kind = embed_kind(model);
    let col = scope.status_col();
    Ok(conn.execute(
        &format!(
            "INSERT INTO job_queue (kind, targetId, payload, status, createdAt, updatedAt)
             SELECT ?1, assetId, NULL, 'pending', unixepoch(), unixepoch() FROM assets
             WHERE {ELIGIBLE} AND COALESCE({col}, 'none') IN ('none','failed')
               AND NOT EXISTS (SELECT 1 FROM job_queue j WHERE j.kind = ?1 AND j.targetId = assets.assetId)"
        ),
        [&kind],
    )?)
}

/// 全库重嵌入入队（换 preprocess/模型后用；老向量表保留，靠 embModel 路由区分新旧）。
pub fn enq_all(conn: &Connection, model: &str, scope: EmbedScope) -> Result<usize> {
    let kind = embed_kind(model);
    let (status_col, model_col) = (scope.status_col(), scope.model_col());
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM job_queue WHERE kind = ?1", [&kind])?;
    let n = tx.execute(
        &format!(
            "INSERT INTO job_queue (kind, targetId, payload, status, createdAt, updatedAt)
             SELECT ?1, assetId, NULL, 'pending', unixepoch(), unixepoch()
             FROM assets WHERE {ELIGIBLE}"
        ),
        [&kind],
    )?;
    tx.execute(
        &format!("UPDATE assets SET {status_col} = 'none', {model_col} = NULL WHERE {ELIGIBLE}"),
        [],
    )?;
    tx.commit()?;
    Ok(n)
}

/// 双塔入队结果（语义塔 / 判重塔分别计数，互不混算）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct ScopeEnqueue {
    pub semantic: usize,
    pub dedup: usize,
}

impl ScopeEnqueue {
    pub fn total(&self) -> usize {
        self.semantic + self.dedup
    }
}

/// 「本批新增资产」谓词（KI-2a 口径）：absPath 等于批次根或位于其下，且
/// `importedAt` 不早于批次 `startedAt`。用 import_batches 现有列界定，
/// 不新增 schema；历史存量不吃这条口径（由 `enq_never_embedded` 显式 backfill）。
const BATCH_ASSET: &str = "((assets.absPath = b.rootPath
            OR substr(assets.absPath, 1, length(b.rootPath) + 1) = b.rootPath || '/')
        AND assets.importedAt >= b.startedAt)";

/// 本批新增资产入某一塔队列。幂等（NOT EXISTS 防重复行；已 done 的状态不在 none/failed）。
pub fn enq_batch_scope(
    conn: &Connection,
    model: &str,
    scope: EmbedScope,
    batch_id: i64,
) -> Result<usize> {
    let kind = embed_kind(model);
    let col = scope.status_col();
    Ok(conn.execute(
        &format!(
            "INSERT INTO job_queue (kind, targetId, payload, status, createdAt, updatedAt)
             SELECT ?1, assets.assetId, NULL, 'pending', unixepoch(), unixepoch()
             FROM assets JOIN import_batches b ON b.batchId = ?2
             WHERE {ELIGIBLE} AND {BATCH_ASSET}
               AND COALESCE(assets.{col}, 'none') IN ('none','failed')
               AND NOT EXISTS (SELECT 1 FROM job_queue j WHERE j.kind = ?1 AND j.targetId = assets.assetId)"
        ),
        rusqlite::params![kind, batch_id],
    )?)
}

/// 导入落库后：本批新增资产**同时**入两塔队列（语义塔路由键 + 写死的判重塔）。
///
/// 两塔状态列独立（`emb_status` vs `dedup_status`），任一侧失败都不影响另一侧；
/// 判重塔模型标识写死为调用方传入的 dedup_model（生产 = `core_config::DEDUP_EMBEDDER`），
/// 不从设置读、不接受端点覆盖（KI-2 裁决：塔不可选）。
pub fn enq_import_batch(
    conn: &Connection,
    semantic_model: &str,
    dedup_model: &str,
    batch_id: i64,
) -> Result<ScopeEnqueue> {
    Ok(ScopeEnqueue {
        semantic: enq_batch_scope(conn, semantic_model, EmbedScope::Semantic, batch_id)?,
        dedup: enq_batch_scope(conn, dedup_model, EmbedScope::Dedup, batch_id)?,
    })
}

/// 历史存量 backfill：从未算过向量（`<scope>_status` 为 `none` 或 NULL）的资产入队。
///
/// 幂等：NOT EXISTS 防重复行 + 从未尝试判定让已 done/failed 的资产不再命中，
/// 重复跑第二次返回 0（可安全重跑，不重复写、不重置失败计数）。
pub fn enq_never_embedded(conn: &Connection, model: &str, scope: EmbedScope) -> Result<usize> {
    let kind = embed_kind(model);
    let col = scope.status_col();
    Ok(conn.execute(
        &format!(
            "INSERT INTO job_queue (kind, targetId, payload, status, createdAt, updatedAt)
             SELECT ?1, assets.assetId, NULL, 'pending', unixepoch(), unixepoch() FROM assets
             WHERE {ELIGIBLE} AND COALESCE(assets.{col}, 'none') = 'none'
               AND NOT EXISTS (SELECT 1 FROM job_queue j WHERE j.kind = ?1 AND j.targetId = assets.assetId)"
        ),
        [&kind],
    )?)
}

fn scope_stats(conn: &Connection, model: &str, scope: EmbedScope) -> Result<EmbedScopeStats> {
    let kind = embed_kind(model);
    let (done, failed, pending) = jobq::stats(conn, &kind)?;
    let col = scope.status_col();
    let remaining: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM assets WHERE {ELIGIBLE}
             AND COALESCE({col},'none') IN ('none','failed')"
        ),
        [],
        |r| r.get(0),
    )?;
    let vectors = core_storage::vecstore::count(conn, model).unwrap_or(0);
    Ok(EmbedScopeStats {
        done,
        failed,
        pending,
        remaining,
        vectors,
    })
}

/// 双塔队列统计。semantic_model = settings.active_embedder（调用方传入）。
pub fn embed_stats(conn: &Connection, semantic_model: &str) -> Result<EmbedQueueStats> {
    Ok(EmbedQueueStats {
        semantic: scope_stats(conn, semantic_model, EmbedScope::Semantic)?,
        dedup: scope_stats(conn, core_config::DEDUP_EMBEDDER, EmbedScope::Dedup)?,
    })
}

/// 嵌入失败 job 列表（两塔合并，最近优先）。
pub fn embed_failed_list(conn: &Connection, limit: i64) -> Result<Vec<EmbedFailedItem>> {
    let mut stmt = conn.prepare(
        "SELECT id, targetId, kind, attempts, COALESCE(lastError,'') FROM job_queue
         WHERE kind LIKE 'embed:%' AND status = 'failed'
         ORDER BY updatedAt DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit], |r| {
        let kind: String = r.get(2)?;
        Ok(EmbedFailedItem {
            jobId: r.get(0)?,
            assetId: r.get(1)?,
            model: kind.strip_prefix("embed:").unwrap_or(&kind).to_string(),
            attempts: r.get(3)?,
            lastError: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 失败 job 重置 pending（UI「重试失败项」）；资产状态列同步回 pending。
pub fn embed_retry_failed(conn: &Connection, model: &str, scope: EmbedScope) -> Result<usize> {
    let kind = embed_kind(model);
    conn.execute(
        &format!(
            "UPDATE assets SET {} = 'pending' WHERE assetId IN
             (SELECT targetId FROM job_queue WHERE kind = ?1 AND status = 'failed')",
            scope.status_col()
        ),
        [&kind],
    )?;
    Ok(conn.execute(
        "UPDATE job_queue SET status = 'pending', attempts = 0, lastError = NULL, updatedAt = unixepoch()
         WHERE kind = ?1 AND status = 'failed'",
        [&kind],
    )?)
}

fn set_status(
    conn: &Connection,
    scope: EmbedScope,
    id: i64,
    status: &str,
    model: &str,
) -> Result<()> {
    let (col, mcol, acol) = (scope.status_col(), scope.model_col(), scope.at_col());
    conn.execute(
        &format!(
            "UPDATE assets SET {col} = ?1, {mcol} = ?2,
                    {acol} = CASE WHEN ?1 IN ('done','failed') THEN unixepoch() ELSE {acol} END
             WHERE assetId = ?3"
        ),
        rusqlite::params![status, model, id],
    )?;
    Ok(())
}

fn eligible(conn: &Connection, asset_id: i64) -> Result<bool> {
    Ok(conn.query_row(
        &format!("SELECT COUNT(*) FROM assets WHERE assetId = ?1 AND {ELIGIBLE}"),
        [asset_id],
        |r| r.get::<_, i64>(0),
    )? > 0)
}

/// 跑嵌入队列直到空或取消。provider 未 ready 报错（不静默）。
/// 断点续跑：先 requeue_stale（崩溃残留 running 回 pending）。
///
/// 首次成功写入前会 `ensure_table` 建 v_<model> 虚表。
pub fn run_embed_queue(
    conn: &Connection,
    provider: &dyn EmbedProvider,
    scope: EmbedScope,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(EmbedProgress),
) -> Result<EmbedRunReport> {
    if !provider.ready() {
        return Err(PipelineError::ProviderNotReady(
            provider.spec().name.clone(),
        ));
    }
    let model = provider.spec().name.clone();
    let dims = provider.spec().dims;
    let kind = embed_kind(&model);
    let _ = jobq::requeue_stale(conn)?;

    let mut report = EmbedRunReport {
        scope: scope.as_str().to_string(),
        model: model.clone(),
        ..Default::default()
    };

    loop {
        if cancel.load(Ordering::SeqCst) {
            report.cancelled = true;
            break;
        }
        let batch = jobq::claim_batch(conn, &kind, EMBED_BATCH)?;
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
            let (_, _, pending) = jobq::stats(conn, &kind)?;
            let index = report.processed + 1;
            let total = pending as usize + index;

            if !eligible(conn, *asset_id)? {
                jobq::complete(conn, *job_id)?;
                report.skipped += 1;
                progress(EmbedProgress {
                    jobId: *job_id,
                    assetId: *asset_id,
                    scope: scope.as_str().into(),
                    model: model.clone(),
                    status: "skipped".into(),
                    index,
                    total,
                    doneCount: report.done,
                    failedCount: report.failed,
                    error: None,
                });
                report.processed += 1;
                continue;
            }

            let path: Option<String> = conn
                .query_row(
                    "SELECT absPath FROM assets WHERE assetId = ?1",
                    [asset_id],
                    |r| r.get(0),
                )
                .ok();

            let outcome = match path {
                None => Err(format!("asset {asset_id}: no absPath")),
                Some(p) if !std::path::Path::new(&p).is_file() => {
                    Err(format!("asset {asset_id}: file missing: {p}"))
                }
                Some(p) => provider
                    .embed_images(&[std::path::PathBuf::from(&p)])
                    .map_err(|e| format!("{e}"))
                    .and_then(|vecs| {
                        vecs.into_iter()
                            .next()
                            .ok_or_else(|| "empty batch".to_string())
                    }),
            };

            match outcome {
                Ok(v) => {
                    let write = (|| -> std::result::Result<(), String> {
                        engine_embed::validate_vectors(provider.spec(), std::slice::from_ref(&v))
                            .map_err(|e| format!("{e}"))?;
                        core_storage::vecstore::ensure_table(conn, &model, dims)
                            .map_err(|e| format!("{e}"))?;
                        // 向量挂 primary resource（1 asset = 1 primary resource）
                        let rid: i64 = conn
                            .query_row(
                                "SELECT resourceId FROM resource WHERE assetId = ?1 AND role = 'primary'",
                                [asset_id],
                                |r| r.get(0),
                            )
                            .map_err(|e| format!("primary resource: {e}"))?;
                        core_storage::vecstore::upsert(conn, &model, &[(rid, &v)])
                            .map_err(|e| format!("{e}"))?;
                        Ok(())
                    })();
                    match write {
                        Ok(()) => {
                            set_status(conn, scope, *asset_id, "done", &model)?;
                            jobq::complete(conn, *job_id)?;
                            // KI-2b：判重塔向量一落库就判（KNN 9 邻居 → dup_edges），
                            // 去重页不再靠「每次进页面重算全库」；判重失败只记账不回退
                            // ——向量已经落好，边是派生数据，补判（backfill）可自愈。
                            if scope == EmbedScope::Dedup {
                                if let Err(e) = judge_asset_dup_edges(conn, &model, *asset_id) {
                                    if report.errors.len() < 50 {
                                        report.errors
                                            .push(format!("{asset_id}: dedup judge: {e}"));
                                    }
                                }
                            }
                            report.done += 1;
                            progress(EmbedProgress {
                                jobId: *job_id,
                                assetId: *asset_id,
                                scope: scope.as_str().into(),
                                model: model.clone(),
                                status: "done".into(),
                                index,
                                total,
                                doneCount: report.done,
                                failedCount: report.failed,
                                error: None,
                            });
                        }
                        Err(msg) => {
                            set_status(conn, scope, *asset_id, "failed", &model)?;
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
                            progress(EmbedProgress {
                                jobId: *job_id,
                                assetId: *asset_id,
                                scope: scope.as_str().into(),
                                model: model.clone(),
                                status: if status == "failed" {
                                    "failed".into()
                                } else {
                                    "retry".into()
                                },
                                index,
                                total,
                                doneCount: report.done,
                                failedCount: report.failed,
                                error: Some(msg),
                            });
                        }
                    }
                }
                Err(msg) => {
                    set_status(conn, scope, *asset_id, "failed", &model)?;
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
                    progress(EmbedProgress {
                        jobId: *job_id,
                        assetId: *asset_id,
                        scope: scope.as_str().into(),
                        model: model.clone(),
                        status: if status == "failed" {
                            "failed".into()
                        } else {
                            "retry".into()
                        },
                        index,
                        total,
                        doneCount: report.done,
                        failedCount: report.failed,
                        error: Some(msg),
                    });
                }
            }
            report.processed += 1;
        }
        if report.cancelled {
            break;
        }
    }

    let (d, f, p) = jobq::stats(conn, &kind)?;
    report.jobDone = d;
    report.jobFailed = f;
    report.jobPending = p;
    Ok(report)
}
