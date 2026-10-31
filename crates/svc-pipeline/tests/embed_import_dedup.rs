//! KI-2a 集成测试：入库即算去重向量（双塔入队 + 存量 backfill + 塔间隔离）。
//!
//! 覆盖（真实 SQLite + 假 EmbedProvider，确定性向量、可控失败）：
//! 1. 本批新增资产同时进两塔队列（批次界定：rootPath 下 + importedAt ≥ startedAt）；
//! 2. 判重塔失败不影响语义塔（状态列/计数各算各的，重试按各自 scope 重置）；
//! 3. 存量 backfill（`dedup_status IS NULL`）幂等可重跑；
//! 4. 批次入队会重跑 failed、跳过 done；
//! 5. 两塔预处理参数由 registry 锁定且互不相同（不共用预处理张量）；
//! 6. 两塔向量落不同分表、维度互不串（禁混算）。

use std::sync::atomic::AtomicBool;

use core_config::{ModelSpec, SpecKind};
use engine_embed::EmbedProvider;
use rusqlite::Connection;
use svc_pipeline::{
    embed_kind, embed_retry_failed, embed_stats, enq_batch_scope, enq_import_batch,
    enq_never_embedded, run_embed_queue, EmbedScope, ScopeEnqueue,
};

const DIMS_SEM: u32 = 8;
const DIMS_DED: u32 = 4;

/// 1×1 PNG（真实可读文件即可，假 provider 不解码）。
const PNG: [u8; 67] = [
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

fn spec(name: &str, dims: u32, modalities: &[&str]) -> ModelSpec {
    ModelSpec {
        name: name.to_string(),
        kind: SpecKind::Embedding,
        dims,
        modalities: modalities.iter().map(|m| m.to_string()).collect(),
        preprocess: serde_yaml::Value::Null,
        normalized: true,
        provider: "fake".into(),
    }
}

/// 假 provider：向量 = 路径 hash 派生（L2 归一），`fail_paths` 命中恒失败。
struct FakeProvider {
    spec: ModelSpec,
    fail_paths: Vec<String>,
}

impl FakeProvider {
    fn new(name: &str, dims: u32) -> Self {
        Self {
            spec: spec(name, dims, &["image"]),
            fail_paths: vec![],
        }
    }
    fn with_failures(name: &str, dims: u32, fail_paths: Vec<String>) -> Self {
        Self {
            spec: spec(name, dims, &["image"]),
            fail_paths,
        }
    }
    fn vec_of(path: &str, dims: u32) -> Vec<f32> {
        let mut h: u64 = 1469598103934665603;
        for b in path.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        let v: Vec<f32> = (0..dims)
            .map(|i| (((h >> (i * 5)) & 0xff) as f32 / 255.0) - 0.5 + 0.01)
            .collect();
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.into_iter().map(|x| x / n).collect()
    }
}

impl EmbedProvider for FakeProvider {
    fn load(&mut self) -> engine_embed::Result<()> {
        Ok(())
    }
    fn embed_images(&self, paths: &[std::path::PathBuf]) -> engine_embed::Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(paths.len());
        for p in paths {
            if self
                .fail_paths
                .iter()
                .any(|f| f == &p.display().to_string())
            {
                return Err(engine_embed::EmbedError::Image(format!(
                    "injected failure for {}",
                    p.display()
                )));
            }
            out.push(Self::vec_of(&p.display().to_string(), self.spec.dims));
        }
        Ok(out)
    }
    fn embed_texts(&self, _texts: &[&str]) -> engine_embed::Result<Vec<Vec<f32>>> {
        Err(engine_embed::EmbedError::Unsupported(
            "fake image-only".into(),
        ))
    }
    fn ready(&self) -> bool {
        true
    }
    fn spec(&self) -> &ModelSpec {
        &self.spec
    }
}

/// 建库 + 在 `dir/root` 下放 n 张图（每张一个 primary resource 行）。
fn setup(dir: &std::path::Path, n_images: usize) -> Connection {
    let conn = core_storage::open(&dir.join("test.db")).unwrap();
    let root = dir.join("lib");
    std::fs::create_dir_all(&root).unwrap();
    for i in 0..n_images {
        insert_asset(&conn, &root.join(format!("img{i}.png")), "local", 0);
    }
    conn
}

/// 插一条资产（+ primary resource）；返回 assetId。
fn insert_asset(conn: &Connection, path: &std::path::Path, source: &str, imported_at: i64) -> i64 {
    std::fs::write(path, PNG).unwrap();
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    conn.execute(
        "INSERT INTO assets (mediaType, absPath, storeMode, fileName, fileExt, fileSz, mtime,
            source, searchable, deleted, approvalStatus, starRating, importedAt)
         VALUES ('image', ?1, 'reference', ?2, 'png', 67, 0, ?3, 1, 0, 'approved', 0, ?4)",
        rusqlite::params![path.display().to_string(), name, source, imported_at],
    )
    .unwrap();
    let id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1, 'primary', 0, 1, 1)",
        [id],
    )
    .unwrap();
    id
}

/// 建一个导入批次行（startedAt = ts），返回 batchId。
fn insert_batch(conn: &Connection, root: &std::path::Path, started_at: i64) -> i64 {
    conn.execute(
        "INSERT INTO import_batches (rootPath, storeMode, status, totalFiles, doneFiles, startedAt)
         VALUES (?1, 'reference', 'done', 0, 0, ?2)",
        rusqlite::params![root.display().to_string(), started_at],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn noprogress(_p: core_types::EmbedProgress) {}

fn job_ids(conn: &Connection, model: &str) -> Vec<i64> {
    let mut stmt = conn
        .prepare("SELECT targetId FROM job_queue WHERE kind = ?1 ORDER BY targetId")
        .unwrap();
    stmt.query_map([embed_kind(model)], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<i64>>>()
        .unwrap()
}

fn col(conn: &Connection, sql_col: &str, asset_id: i64) -> Option<String> {
    conn.query_row(
        &format!("SELECT {sql_col} FROM assets WHERE assetId = ?1"),
        [asset_id],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn import_batch_enqueues_both_towers_for_new_assets_only() {
    let dir = tempfile::tempdir().unwrap();
    let conn = setup(dir.path(), 0);
    let root = dir.path().join("lib");
    let started = 1_700_000_000i64;

    // 3 张本批新增（root 下，importedAt ≥ startedAt）
    let mut batch_ids = vec![];
    for i in 0..3 {
        batch_ids.push(insert_asset(
            &conn,
            &root.join(format!("new{i}.png")),
            "local",
            started + 5,
        ));
    }
    // 存量：root 下但早于批次 → 不算本批
    let old = insert_asset(&conn, &root.join("old.png"), "local", started - 100);
    // 批外：别的目录同时间导入 → 不算本批
    let outside = insert_asset(&conn, &dir.path().join("other.png"), "local", started + 5);
    let batch_id = insert_batch(&conn, &root, started);

    let q = enq_import_batch(&conn, "fake-semantic", "fake-dedup", batch_id).unwrap();
    assert_eq!(
        q,
        ScopeEnqueue {
            semantic: 3,
            dedup: 3
        },
        "仅本批新增资产入两塔队列"
    );
    assert_eq!(job_ids(&conn, "fake-semantic"), batch_ids);
    assert_eq!(job_ids(&conn, "fake-dedup"), batch_ids);
    assert!(!job_ids(&conn, "fake-dedup").contains(&old));
    assert!(!job_ids(&conn, "fake-dedup").contains(&outside));

    // 幂等：重跑不重复写
    let q2 = enq_import_batch(&conn, "fake-semantic", "fake-dedup", batch_id).unwrap();
    assert_eq!(q2.total(), 0);
    assert_eq!(job_ids(&conn, "fake-dedup").len(), 3);
}

#[test]
fn dedup_failure_does_not_touch_semantic_tower() {
    let dir = tempfile::tempdir().unwrap();
    let conn = setup(dir.path(), 6);
    let root = dir.path().join("lib");
    let started = 1_700_000_000i64;
    let batch_id = insert_batch(&conn, &root, started);
    // setup 里 importedAt = 0（早于批次）→ 改成批次内，模拟本批新增
    conn.execute("UPDATE assets SET importedAt = ?1", [started + 1])
        .unwrap();

    let q = enq_import_batch(&conn, "fake-semantic", "fake-dedup", batch_id).unwrap();
    assert_eq!(q.semantic, 6);
    assert_eq!(q.dedup, 6);

    // 语义塔全成功
    let sem = FakeProvider::new("fake-semantic", DIMS_SEM);
    let cancel = AtomicBool::new(false);
    let rep = run_embed_queue(&conn, &sem, EmbedScope::Semantic, &cancel, &mut noprogress).unwrap();
    assert_eq!(rep.done, 6);

    // 判重塔：img2 恒失败（3 次重试后 failed）
    let bad = root.join("img2.png").display().to_string();
    let ded = FakeProvider::with_failures("fake-dedup", DIMS_DED, vec![bad]);
    let rep = run_embed_queue(&conn, &ded, EmbedScope::Dedup, &cancel, &mut noprogress).unwrap();
    assert_eq!(rep.done, 5);
    assert_eq!(rep.failed, 1);

    // 两塔状态列各算各的：失败那张 emb_status 仍是 done
    let failed_id: i64 = conn
        .query_row(
            "SELECT assetId FROM assets WHERE fileName = 'img2.png'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        col(&conn, "dedup_status", failed_id).as_deref(),
        Some("failed")
    );
    assert_eq!(col(&conn, "emb_status", failed_id).as_deref(), Some("done"));
    assert_eq!(
        col(&conn, "emb_model", failed_id).as_deref(),
        Some("fake-semantic")
    );
    assert_eq!(
        col(&conn, "dedup_model", failed_id).as_deref(),
        Some("fake-dedup")
    );

    // 计数不混算（语义塔走 embed_stats；判重塔计数在测试里按状态列直查，
    // 因为 embed_stats 的 dedup 槽固定取 DEDUP_EMBEDDER 生产常量）
    let st = embed_stats(&conn, "fake-semantic").unwrap();
    assert_eq!(st.semantic.done, 6);
    assert_eq!(st.semantic.failed, 0);
    let dedup_count = |status: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM assets WHERE dedup_status = ?1",
            [status],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(dedup_count("done"), 5);
    assert_eq!(dedup_count("failed"), 1);

    // 重试按各自 scope 重置：语义塔没有失败项（重置 0），判重塔重置 1
    let sem_reset = embed_retry_failed(&conn, "fake-semantic", EmbedScope::Semantic).unwrap();
    let ded_reset = embed_retry_failed(&conn, "fake-dedup", EmbedScope::Dedup).unwrap();
    assert_eq!(sem_reset, 0, "判重塔失败不混进语义塔计数");
    assert_eq!(ded_reset, 1);

    // 修复后续跑：判重塔补齐那张，语义塔不动
    let ded_fixed = FakeProvider::new("fake-dedup", DIMS_DED);
    let rep = run_embed_queue(
        &conn,
        &ded_fixed,
        EmbedScope::Dedup,
        &cancel,
        &mut noprogress,
    )
    .unwrap();
    assert_eq!(rep.done, 1);
    assert_eq!(
        col(&conn, "dedup_status", failed_id).as_deref(),
        Some("done")
    );
    assert_eq!(
        core_storage::vecstore::count(&conn, "fake-dedup").unwrap(),
        6
    );
}

#[test]
fn backfill_never_embedded_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let conn = setup(dir.path(), 5);
    // a0 done / a1 failed / a2..a4 NULL（历史存量：从未算过判重向量）
    conn.execute(
        "UPDATE assets SET dedup_status = 'done' WHERE fileName = 'img0.png'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE assets SET dedup_status = 'failed' WHERE fileName = 'img1.png'",
        [],
    )
    .unwrap();

    let n = enq_never_embedded(&conn, "fake-dedup", EmbedScope::Dedup).unwrap();
    assert_eq!(n, 3, "只补 dedup_status IS NULL 的资产");
    // 幂等：第二次 0，job 行数不增
    assert_eq!(
        enq_never_embedded(&conn, "fake-dedup", EmbedScope::Dedup).unwrap(),
        0
    );
    assert_eq!(job_ids(&conn, "fake-dedup").len(), 3);

    // 跑完再 backfill 仍是 0（NULL 判定 + NOT EXISTS 双保险）
    let ded = FakeProvider::new("fake-dedup", DIMS_DED);
    let cancel = AtomicBool::new(false);
    run_embed_queue(&conn, &ded, EmbedScope::Dedup, &cancel, &mut noprogress).unwrap();
    assert_eq!(
        enq_never_embedded(&conn, "fake-dedup", EmbedScope::Dedup).unwrap(),
        0
    );
    // failed 那条没被 backfill 拿走（仍 failed，等显式 retry）
    let failed_id: i64 = conn
        .query_row(
            "SELECT assetId FROM assets WHERE fileName = 'img1.png'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        col(&conn, "dedup_status", failed_id).as_deref(),
        Some("failed")
    );
}

#[test]
fn batch_enqueue_retries_failed_but_skips_done() {
    let dir = tempfile::tempdir().unwrap();
    let conn = setup(dir.path(), 4);
    let root = dir.path().join("lib");
    let started = 1_700_000_000i64;
    let batch_id = insert_batch(&conn, &root, started);
    conn.execute("UPDATE assets SET importedAt = ?1", [started + 1])
        .unwrap();
    // 一张已 done（不该再入队）、一张 failed（应重跑）
    conn.execute(
        "UPDATE assets SET dedup_status = 'done' WHERE fileName = 'img0.png'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE assets SET dedup_status = 'failed' WHERE fileName = 'img1.png'",
        [],
    )
    .unwrap();

    let n = enq_batch_scope(&conn, "fake-dedup", EmbedScope::Dedup, batch_id).unwrap();
    assert_eq!(n, 3, "done 跳过，failed + none 入队");
    let names: Vec<String> = job_ids(&conn, "fake-dedup")
        .iter()
        .map(|id| {
            conn.query_row(
                "SELECT fileName FROM assets WHERE assetId = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        })
        .collect();
    assert!(!names.contains(&"img0.png".to_string()));
    assert!(names.contains(&"img1.png".to_string()));
}

#[test]
fn tower_preprocess_locked_and_distinct() {
    let reg = core_config::load_registry(None).unwrap();
    let sem = reg.by_name(core_config::DEFAULT_EMBEDDER).unwrap();
    let ded = reg.by_name(core_config::DEDUP_EMBEDDER).unwrap();

    // 判重塔只有 image 模态（不能当语义塔/文搜图塔）
    assert_eq!(ded.modalities, vec!["image".to_string()]);
    assert!(sem.modalities.iter().any(|m| m == "text"));
    // 维度与预处理参数各自锁定、互不相同（不得共用预处理张量）
    assert_ne!(sem.dims, ded.dims);
    let size = |s: &ModelSpec| s.preprocess["image_size"].as_i64().unwrap();
    assert_eq!(size(sem), 224);
    assert_eq!(size(ded), 256);
    assert_ne!(
        sem.preprocess["mean"].as_sequence().unwrap(),
        ded.preprocess["mean"].as_sequence().unwrap()
    );
    assert_ne!(
        sem.preprocess["std"].as_sequence().unwrap(),
        ded.preprocess["std"].as_sequence().unwrap()
    );
}

#[test]
fn dual_tower_vectors_land_in_separate_tables() {
    let dir = tempfile::tempdir().unwrap();
    let conn = setup(dir.path(), 5);
    let cancel = AtomicBool::new(false);
    let sem = FakeProvider::new("fake-semantic", DIMS_SEM);
    let ded = FakeProvider::new("fake-dedup", DIMS_DED);

    assert_eq!(
        enq_import_batch(&conn, "fake-semantic", "fake-dedup", {
            let root = dir.path().join("lib");
            conn.execute("UPDATE assets SET importedAt = 1", [])
                .unwrap();
            insert_batch(&conn, &root, 1)
        })
        .unwrap()
        .total(),
        10,
        "两塔各 5 条"
    );
    run_embed_queue(&conn, &sem, EmbedScope::Semantic, &cancel, &mut noprogress).unwrap();
    run_embed_queue(&conn, &ded, EmbedScope::Dedup, &cancel, &mut noprogress).unwrap();

    let mut tables: Vec<(String, u32, i64)> = core_storage::vecstore::tables(&conn)
        .unwrap()
        .into_iter()
        .map(|(model, dims, _t)| {
            let n = core_storage::vecstore::count(&conn, &model).unwrap();
            (model, dims, n)
        })
        .collect();
    tables.sort();
    assert_eq!(
        tables,
        vec![
            ("fake-dedup".to_string(), DIMS_DED, 5),
            ("fake-semantic".to_string(), DIMS_SEM, 5),
        ]
    );
    // 两塔向量不共表：各自的 knn 只命中自己的维度
    let q_sem = FakeProvider::vec_of("x", DIMS_SEM);
    let q_ded = FakeProvider::vec_of("x", DIMS_DED);
    assert_eq!(
        core_storage::vecstore::knn(&conn, "fake-semantic", &q_sem, 5)
            .unwrap()
            .len(),
        5
    );
    assert_eq!(
        core_storage::vecstore::knn(&conn, "fake-dedup", &q_ded, 5)
            .unwrap()
            .len(),
        5
    );
}
