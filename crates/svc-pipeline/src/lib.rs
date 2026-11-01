//! L2 `svc-pipeline` — embedding / 打标编排（asset 状态机 + provider 调用）。
//!
//! D3 落点（PLAN.md §9）：
//! - embedding：`assets.emb_status` 状态机 `none → pending → done/failed`，
//!   `emb_model` = provider spec.name（禁混算锚点）。**向量不落盘**——`vectors_<model>`
//!   分表与索引是 D5 的职责；本层产出向量并随 `EmbRunReport.vectors` 回传，
//!   D5 的写入器直接消费该返回值（D3 明确不做向量索引）。
//! - 打标：云 VL 输出（title/description/tags）落 `assets.title_*`/`description`
//!   + `tag` 表（origin='auto'，不覆盖人工 manual 标签、不覆盖已有人工标题）。

use core_types::{EmbStats, TagApplyResult};
use engine_embed::{validate_vectors, EmbedProvider};
use engine_tag::{TagProvider, TaggingOutput};
use rusqlite::Connection;

pub mod tag_queue;
pub use tag_queue::{
    classify_tag_failure, enq_all, enq_untagged, retry_failed, retry_failed_class, run_tag_queue,
    run_tag_queue_with_limit, tag_failed_list, tag_stats, TAG_BATCH,
};

// OCR-E — 文字识别队列（job kind='ocr'；run_* 在 local-infer 构建下编译）
pub mod ocr_queue;
pub use ocr_queue::{
    enq_all as ocr_enq_all, enq_untagged as ocr_enq_untagged, ocr_failed_list, ocr_stats,
    retry_failed as ocr_retry_failed, OCR_BATCH,
};
#[cfg(feature = "local-infer")]
pub use ocr_queue::{ocr_one, run_ocr_queue, run_ocr_queue_with_limit};

pub mod custom_queue;
pub use custom_queue::{
    custom_failed_list, run_custom_queue, run_custom_queue_with_limit, CUSTOM_TAG_BATCH,
};

pub mod review;
pub use review::{
    enq_untagged_review, review_commit, review_discard, review_items, review_stats, review_update,
    run_custom_queue_review_with_limit, run_tag_queue_review_with_limit, stage_custom_values,
    stage_template, ReviewAddField, ReviewAssetView, ReviewCommitReport, ReviewItemView,
    ReviewUpdateField, ReviewUpdateReq,
};

pub mod embed_queue;
pub use embed_queue::{
    backfill_dup_edges, dup_edge_kind, dup_judge_status, embed_failed_list, embed_kind,
    embed_retry_failed, embed_stats, enq_all as embed_enq_all, enq_batch_scope, enq_import_batch,
    enq_never_embedded, enq_unembedded, judge_asset_dup_edges, run_embed_queue, DupBackfillReport,
    DupJudgeStatus, EmbedScope, ScopeEnqueue, DUP_K_NEIGHBORS, DUP_NEAR_THRESHOLD,
    DUP_VARIANT_THRESHOLD, EMBED_BATCH,
};

// C2 — compliance 编排（job kind='compliance' + step 双语义管道编排器 + 插件注册表）
pub mod compliance_plugins;
pub mod compliance_queue;
// CMP-JOB-01 — 坏图入口收敛：消费前按魔数判「是不是图」（不是图 → skipped，不再永久 failed）
pub mod image_probe;
// CMP-JOB-FIX-1 — 跳过原因词表（唯一口径：资产状态分支与坏图分支都从这里取前缀，
// 每条计入 run.skipped 的 task 必须带原因；界面 / E2E 探针同口径）
pub mod skip_reason;
/// C5 — compliance `executor: vl` 通道（judge + 打标 + bbox→inpaint 闭环）。
pub mod compliance_vl;
/// DM-C1（t_d5c6c6a2）— inpaint 重绘区域三态（explicit/ocr_text/u2net）+ VL bbox × OCR 行框交叉验证。
pub mod inpaint_region;
// C8 — 装载期 lint（注册表层：检测器/参数键/插件 id）+ 规则级失败隔离的输入
pub mod compliance_lint;
// C9 — 组级判定（per_group）：L2 组级检测器注册表（phash_dup）+ 组键解析
pub mod compliance_group;
/// N6e — 宿主侧动作链（一条 job 串多个动作、只落一张工作版本；插件不互相调用）
pub mod compliance_chain;
pub use compliance_chain::{
    chain_budget, run_chain, validate_actions, ChainArtifact, ChainFailure, ChainOutcome, ChainStep,
    CHAIN_BUDGET_CAP_SECS, CHAIN_DEFAULT_STEP_SECS, CHAIN_MAX_ACTIONS,
};
pub use compliance_group::{
    GroupDetectorOutput, GroupInput, GroupMemberInput, ResolvedGroups, GROUP_DETECTORS,
};
pub use compliance_queue::{
    compliance_enq, compliance_enq_actions_in_run, compliance_enq_group, compliance_enq_group_in_run,
    compliance_enq_in_run,
    compliance_group_stats, compliance_retry_failed, compliance_stats,
    run_compliance_queue, run_compliance_queue_in_run, run_compliance_queue_par,
    run_compliance_queue_par_in_run,
    // IP-K05：执行通道（插件）入口 + 端口
    run_compliance_queue_par_in_run_engine, run_compliance_queue_with_limit_in_run_engine,
    PackIdentityInfo, PackJudge, PluginAsset, PluginFailure, PluginOutcome, PluginVerdict,
    run_compliance_queue_with_limit,
    ComplianceProgress, ComplianceRunReport, ConnFactory, COMPLIANCE_BATCH,
    COMPLIANCE_GROUP_KIND, COMPLIANCE_KINDS, COMPLIANCE_KIND, VERDICT_SCOPE_ASSET,
    VERDICT_SCOPE_GROUP,
};
pub use compliance_vl::{
    group_asks_from_pack, vl_group_params, VlCache, VlConfig, VlError, VlGroupAsk, VlHandle,
    VlJudgeAsk, VlJudgeOut, VlSession, VlStats, VlStatsSnapshot,
};

// EXIF2 — 存量图 EXIF/画幅回填队列（job kind='exif'，D4 范式）
pub mod exif_queue;
pub use exif_queue::{
    enqueue_missing as exif_enqueue_missing, enqueue_missing,
    reset_for_reparse as exif_reset_for_reparse, retry_failed as exif_retry_failed,
    run_exif_queue, run_exif_queue_with_limit, status as exif_status, ExifQueueStatus,
    EXIF_BATCH,
};

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("provider not ready: {0}")]
    ProviderNotReady(String),
    #[error("asset {0} not found")]
    AssetNotFound(i64),
    #[error("embed: {0}")]
    Embed(#[from] engine_embed::EmbedError),
    #[error("tag: {0}")]
    Tag(#[from] engine_tag::TagError),
    #[error("jobq: {0}")]
    Jobq(#[from] engine_jobq::JobqError),
    /// D10：custom_tasks DAO（任务读取/upsert）错误透传。
    #[error("storage: {0}")]
    Storage(#[from] core_storage::StorageError),
    /// C2：compliance 编排错误（payload/pack 不一致/检测器与插件分发/产物 IO）。
    #[error("compliance: {0}")]
    Compliance(String),
    #[error("asset {0} has no readable image ({1})")]
    NoImage(i64, String),
    /// EXIF2：通用错误（落库失败等）。
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, PipelineError>;

/// 一批 embedding 的汇总。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct EmbRunReport {
    pub total: usize,
    pub done: usize,
    pub failed: usize,
    /// 失败明细（`assetId: msg`），最多保留 50 条
    pub errors: Vec<String>,
    /// 已产出的向量（assetId, vector）——D5 落 `vectors_<model>`；D3 不落盘
    #[serde(skip)]
    pub vectors: Vec<(i64, Vec<f32>)>,
}

/// 逐图进度回调载荷（上层转 app 事件）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct EmbProgress {
    pub assetId: i64,
    /// done / failed
    pub status: String,
    pub index: usize,
    pub total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 嵌入状态计数（设置页/状态栏）。
pub fn emb_stats(conn: &Connection) -> Result<EmbStats> {
    let mut st = EmbStats::default();
    let mut stmt = conn.prepare(
        "SELECT COALESCE(emb_status,'none') AS s, COUNT(*) FROM assets
         WHERE deleted = 0 GROUP BY s",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    for row in rows {
        let (s, n) = row?;
        match s.as_str() {
            "none" => st.none = n,
            "pending" => st.pending = n,
            "done" => st.done = n,
            "failed" => st.failed = n,
            _ => {}
        }
    }
    Ok(st)
}

/// 待嵌入资产（图片、未删除、未失联）→ (assetId, absPath)。limit 0 = 不限。
pub fn pending_assets(conn: &Connection, limit: usize) -> Result<Vec<(i64, String)>> {
    let base = "SELECT assetId, absPath FROM assets
         WHERE deleted = 0 AND missing = 0 AND mediaType = 'image'
           AND COALESCE(emb_status,'none') IN ('none','pending')
         ORDER BY assetId";
    let sql = if limit == 0 {
        base.to_string()
    } else {
        format!("{base} LIMIT {limit}")
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 打 pending 标记（入队语义：先把范围标成 pending，再由 `embed_pending` 消费）。
pub fn mark_pending(conn: &Connection, ids: &[i64], model: &str) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    let mut n = 0usize;
    {
        let mut stmt = tx.prepare(
            "UPDATE assets SET emb_status = 'pending', emb_model = ?1 WHERE assetId = ?2",
        )?;
        for id in ids {
            n += stmt.execute(rusqlite::params![model, id])?;
        }
    }
    tx.commit()?;
    Ok(n)
}

/// 把「未嵌入 / 曾失败」的图片全部标 pending（一键补嵌入）。
pub fn mark_all_pending(conn: &Connection, model: &str) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE assets SET emb_status = 'pending', emb_model = ?1
         WHERE deleted = 0 AND missing = 0 AND mediaType = 'image'
           AND COALESCE(emb_status,'none') IN ('none','failed')",
        rusqlite::params![model],
    )?)
}

/// 失败项重置为 pending（重试）。
pub fn reset_failed(conn: &Connection, model: &str) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE assets SET emb_status = 'pending', emb_model = ?1 WHERE emb_status = 'failed'",
        rusqlite::params![model],
    )?)
}

fn set_status(conn: &Connection, id: i64, status: &str, model: &str) -> Result<()> {
    conn.execute(
        "UPDATE assets SET emb_status = ?1, emb_model = ?2,
                emb_at = CASE WHEN ?1 IN ('done','failed') THEN unixepoch() ELSE emb_at END
         WHERE assetId = ?3",
        rusqlite::params![status, model, id],
    )?;
    Ok(())
}

/// 单图嵌入：状态机推进 + 向量自检（产出向量但不落盘）。
pub fn embed_one(
    conn: &Connection,
    provider: &dyn EmbedProvider,
    asset_id: i64,
) -> Result<Vec<f32>> {
    let model = provider.spec().name.clone();
    let path: Option<String> = conn
        .query_row(
            "SELECT absPath FROM assets WHERE assetId = ?1 AND deleted = 0",
            [asset_id],
            |r| r.get(0),
        )
        .ok();
    let path = match path {
        Some(p) => p,
        None => return Err(PipelineError::AssetNotFound(asset_id)),
    };
    if !std::path::Path::new(&path).is_file() {
        set_status(conn, asset_id, "failed", &model)?;
        return Err(PipelineError::NoImage(asset_id, path));
    }
    set_status(conn, asset_id, "pending", &model)?;
    let vecs = match provider.embed_images(&[std::path::PathBuf::from(&path)]) {
        Ok(v) => v,
        Err(e) => {
            set_status(conn, asset_id, "failed", &model)?;
            return Err(PipelineError::Embed(e));
        }
    };
    match vecs.first() {
        Some(v) => {
            validate_vectors(provider.spec(), std::slice::from_ref(v))?;
            set_status(conn, asset_id, "done", &model)?;
            Ok(v.clone())
        }
        None => {
            set_status(conn, asset_id, "failed", &model)?;
            Err(PipelineError::Embed(engine_embed::EmbedError::BatchSize(
                1, 0,
            )))
        }
    }
}

/// 批量嵌入 pending 资产。provider 未 ready 时按 spec 报错（不静默跳过）。
pub fn embed_pending(
    conn: &Connection,
    provider: &dyn EmbedProvider,
    limit: usize,
    progress: &mut dyn FnMut(EmbProgress),
) -> Result<EmbRunReport> {
    if !provider.ready() {
        return Err(PipelineError::ProviderNotReady(
            provider.spec().name.clone(),
        ));
    }
    let items = pending_assets(conn, limit)?;
    let total = items.len();
    let mut report = EmbRunReport {
        total,
        ..Default::default()
    };
    for (i, (asset_id, _)) in items.iter().enumerate() {
        match embed_one(conn, provider, *asset_id) {
            Ok(v) => {
                report.done += 1;
                report.vectors.push((*asset_id, v));
                progress(EmbProgress {
                    assetId: *asset_id,
                    status: "done".into(),
                    index: i + 1,
                    total,
                    error: None,
                });
            }
            Err(e) => {
                report.failed += 1;
                let msg = format!("{asset_id}: {e}");
                if report.errors.len() < 50 {
                    report.errors.push(msg.clone());
                }
                progress(EmbProgress {
                    assetId: *asset_id,
                    status: "failed".into(),
                    index: i + 1,
                    total,
                    error: Some(msg),
                });
            }
        }
    }
    Ok(report)
}

/// 单图打标：云 VL → 标题/描述落 assets（只填空字段）+ 标签落 tag 表。
pub fn tag_one(
    conn: &Connection,
    provider: &dyn TagProvider,
    asset_id: i64,
) -> Result<TagApplyResult> {
    if !provider.ready() {
        return Err(PipelineError::ProviderNotReady(
            provider.spec().name.clone(),
        ));
    }
    let row: Option<(String, Option<String>, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT absPath, title_ch, title_en, description FROM assets
             WHERE assetId = ?1 AND deleted = 0",
            [asset_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .ok();
    let (path, cur_ch, cur_en, cur_desc) = match row {
        Some(v) => v,
        None => return Err(PipelineError::AssetNotFound(asset_id)),
    };
    if !std::path::Path::new(&path).is_file() {
        return Err(PipelineError::NoImage(asset_id, path));
    }
    let out: TaggingOutput = provider.tag_image(std::path::Path::new(&path))?;
    apply_tagging(
        conn,
        provider.spec().name.as_str(),
        asset_id,
        &out,
        cur_ch,
        cur_en,
        cur_desc,
    )
}

/// 落库（供 `tag_one` 与测试复用）：空字段才填、auto 标签不覆盖 manual。
pub fn apply_tagging(
    conn: &Connection,
    model: &str,
    asset_id: i64,
    out: &TaggingOutput,
    cur_ch: Option<String>,
    cur_en: Option<String>,
    cur_desc: Option<String>,
) -> Result<TagApplyResult> {
    let keep = |cur: Option<String>, new: &str| -> Option<String> {
        match cur.filter(|c| !c.trim().is_empty()) {
            Some(existing) => Some(existing),
            None => (!new.trim().is_empty()).then(|| new.trim().to_string()),
        }
    };
    let title_ch = keep(cur_ch, &out.title_ch);
    let title_en = keep(cur_en, &out.title_en);
    let description = keep(cur_desc, &out.description);
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE assets SET title_ch = ?1, title_en = ?2, description = ?3 WHERE assetId = ?4",
        rusqlite::params![title_ch, title_en, description, asset_id],
    )?;
    let mut inserted = 0usize;
    for t in &out.tags {
        let name = t.name.trim();
        if name.is_empty() {
            continue;
        }
        // 标签值落 tag（只有 value）；auto 不覆盖 manual/path/custom（人工与其他来源优先）
        for v in core_storage::tags::split_values(name) {
            if core_storage::tags::upsert_tag(
                &tx,
                asset_id,
                &v,
                "auto",
                Some(model),
                None,
                t.score as f64,
            )? {
                inserted += 1;
            }
        }
    }
    tx.commit()?;
    Ok(TagApplyResult {
        assetId: asset_id,
        titleCh: title_ch.unwrap_or_default(),
        titleEn: title_en.unwrap_or_default(),
        description: description.unwrap_or_default(),
        tagsInserted: inserted,
        repaired: out.repaired,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_config::SpecKind;
    use engine_embed::EmbedError;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// 测试用假 embedder：固定归一向量 + 可注入失败。
    struct FakeEmbed {
        spec: core_config::ModelSpec,
        dims: usize,
        fail: Mutex<bool>,
    }

    impl FakeEmbed {
        fn new(dims: usize) -> Self {
            Self {
                spec: core_config::ModelSpec {
                    name: "fake-onnx".into(),
                    kind: SpecKind::Embedding,
                    dims: dims as u32,
                    modalities: vec!["image".into()],
                    preprocess: serde_yaml::Value::Null,
                    normalized: true,
                    provider: "fake".into(),
                },
                dims,
                fail: Mutex::new(false),
            }
        }
    }

    impl EmbedProvider for FakeEmbed {
        fn load(&mut self) -> std::result::Result<(), EmbedError> {
            Ok(())
        }
        fn embed_images(
            &self,
            paths: &[PathBuf],
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            if *self.fail.lock().unwrap() {
                return Err(EmbedError::Onnx("boom".into()));
            }
            Ok(paths
                .iter()
                .map(|_| vec![1.0f32 / (self.dims as f32).sqrt(); self.dims])
                .collect())
        }
        fn embed_texts(&self, texts: &[&str]) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|_| vec![0f32; self.dims]).collect())
        }
        fn ready(&self) -> bool {
            true
        }
        fn spec(&self) -> &core_config::ModelSpec {
            &self.spec
        }
    }

    /// 测试用假 tagger。
    struct FakeTag {
        spec: core_config::ModelSpec,
        out: TaggingOutput,
    }

    impl engine_tag::TagProvider for FakeTag {
        fn load(&mut self) -> std::result::Result<(), engine_tag::TagError> {
            Ok(())
        }
        fn tag_image(
            &self,
            _path: &std::path::Path,
        ) -> std::result::Result<TaggingOutput, engine_tag::TagError> {
            Ok(self.out.clone())
        }
        fn test_connection(&self) -> std::result::Result<String, engine_tag::TagError> {
            Ok("OK".into())
        }
        fn ready(&self) -> bool {
            true
        }
        fn spec(&self) -> &core_config::ModelSpec {
            &self.spec
        }
    }

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&c).unwrap();
        c
    }

    fn add_asset(c: &Connection, path: &str) -> i64 {
        c.execute(
            "INSERT INTO assets (mediaType, absPath, storeMode, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES ('image', ?1, 'reference', 'a.jpg', 'jpg', 1, 0, 0)",
            rusqlite::params![path],
        )
        .unwrap();
        c.last_insert_rowid()
    }

    fn tmp_file(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("dam-pipe-{tag}-{}.jpg", std::process::id()));
        std::fs::write(&p, b"pretend-image-bytes").unwrap();
        p
    }

    #[test]
    fn state_machine_none_pending_done_failed_and_retry() {
        let c = db();
        let img = tmp_file("sm");
        let a1 = add_asset(&c, img.to_str().unwrap());
        let a2 = add_asset(&c, img.to_str().unwrap());
        assert_eq!(emb_stats(&c).unwrap().none, 2);

        let p = FakeEmbed::new(8);
        let v = embed_one(&c, &p, a1).unwrap();
        assert_eq!(v.len(), 8);
        assert!((engine_embed::l2_norm(&v) - 1.0).abs() < 1e-6);
        let (status, model): (String, Option<String>) = c
            .query_row(
                "SELECT emb_status, emb_model FROM assets WHERE assetId = ?1",
                [a1],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "done");
        assert_eq!(model.as_deref(), Some("fake-onnx"));
        let stats = emb_stats(&c).unwrap();
        assert_eq!((stats.done, stats.none), (1, 1));

        // 失败 → failed（不留 done）
        *p.fail.lock().unwrap() = true;
        assert!(embed_one(&c, &p, a2).is_err());
        let st2: String = c
            .query_row(
                "SELECT emb_status FROM assets WHERE assetId = ?1",
                [a2],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(st2, "failed");
        assert_eq!(emb_stats(&c).unwrap().failed, 1);

        // 重试：reset_failed → pending → 跑通
        assert_eq!(reset_failed(&c, "fake-onnx").unwrap(), 1);
        assert_eq!(emb_stats(&c).unwrap().pending, 1);
        *p.fail.lock().unwrap() = false;
        let report = embed_pending(&c, &p, 0, &mut |_| {}).unwrap();
        assert_eq!((report.done, report.failed), (1, 0));
        assert_eq!(emb_stats(&c).unwrap().done, 2);
    }

    #[test]
    fn missing_file_marks_failed() {
        let c = db();
        let a = add_asset(&c, "/nonexistent/path.jpg");
        let p = FakeEmbed::new(4);
        let err = embed_one(&c, &p, a).unwrap_err();
        assert!(matches!(err, PipelineError::NoImage(_, _)), "got {err:?}");
        let st: String = c
            .query_row(
                "SELECT emb_status FROM assets WHERE assetId = ?1",
                [a],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(st, "failed");
    }

    #[test]
    fn sweep_reports_progress_and_vectors() {
        let c = db();
        let img = tmp_file("sweep");
        for _ in 0..3 {
            add_asset(&c, img.to_str().unwrap());
        }
        assert_eq!(mark_all_pending(&c, "fake-onnx").unwrap(), 3);
        assert_eq!(emb_stats(&c).unwrap().pending, 3);
        let p = FakeEmbed::new(4);
        let mut seen = 0;
        let report = embed_pending(&c, &p, 0, &mut |ev| {
            seen += 1;
            assert_eq!(ev.status, "done");
            assert_eq!(ev.total, 3);
        })
        .unwrap();
        assert_eq!(seen, 3);
        assert_eq!((report.total, report.done, report.failed), (3, 3, 0));
        assert_eq!(report.vectors.len(), 3);
    }

    #[test]
    fn embed_pending_refuses_unready_provider() {
        struct NotReady(FakeEmbed);
        impl EmbedProvider for NotReady {
            fn load(&mut self) -> std::result::Result<(), EmbedError> {
                Ok(())
            }
            fn embed_images(
                &self,
                _p: &[PathBuf],
            ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
                unreachable!()
            }
            fn embed_texts(&self, _t: &[&str]) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
                unreachable!()
            }
            fn ready(&self) -> bool {
                false
            }
            fn spec(&self) -> &core_config::ModelSpec {
                &self.0.spec
            }
        }
        let c = db();
        let p = NotReady(FakeEmbed::new(4));
        let err = embed_pending(&c, &p, 0, &mut |_| {}).unwrap_err();
        assert!(
            matches!(err, PipelineError::ProviderNotReady(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn tagging_fills_empty_fields_and_inserts_auto_tags() {
        let c = db();
        let img = tmp_file("tag");
        let a = add_asset(&c, img.to_str().unwrap());
        // 人工已有中文标题 + 一个 manual 标签 → 都不能被覆盖
        c.execute(
            "UPDATE assets SET title_ch = '人工标题' WHERE assetId = ?1",
            [a],
        )
        .unwrap();
        c.execute(
            "INSERT INTO tag (assetId, value, origin, model, score, createdAt)
             VALUES (?1, '狗', 'manual', NULL, 1.0, 0)",
            [a],
        )
        .unwrap();

        let spec = core_config::load_registry(None)
            .unwrap()
            .by_name("openai-compat-tagger")
            .unwrap()
            .clone();
        let tagger = FakeTag {
            spec,
            out: TaggingOutput {
                title_ch: "AI 标题".into(),
                title_en: "AI Title".into(),
                description: "AI 描述".into(),
                tags: vec![
                    engine_tag::TagItem::new("狗", 0.5),
                    engine_tag::TagItem::new("草地", 0.8),
                    engine_tag::TagItem::new("  ", 0.9),
                ],
                repaired: false,
            },
        };
        let res = tag_one(&c, &tagger, a).unwrap();
        assert_eq!(res.titleCh, "人工标题"); // 人工优先
        assert_eq!(res.titleEn, "AI Title");
        assert_eq!(res.description, "AI 描述");
        // 「狗」撞 manual 被 ON CONFLICT WHERE 拒（0 行）+「草地」insert（1 行）；
        // 空白标签丢弃。tagsInserted = 真实 DB 变更行数。
        assert_eq!(res.tagsInserted, 1);

        let (origin, score): (String, f64) = c
            .query_row(
                "SELECT origin, score FROM tag WHERE assetId = ?1 AND value = '狗'",
                [a],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(origin, "manual");
        assert!((score - 1.0).abs() < 1e-9); // manual 分不被 auto 覆盖
        let (o2, m2): (String, Option<String>) = c
            .query_row(
                "SELECT origin, model FROM tag WHERE assetId = ?1 AND value = '草地'",
                [a],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(o2, "auto");
        assert_eq!(m2.as_deref(), Some("openai-compat-tagger"));
    }
}
