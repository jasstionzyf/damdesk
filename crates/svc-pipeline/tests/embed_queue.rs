//! D5 嵌入队列集成测试：真实 SQLite + 假 EmbedProvider（确定性向量，可控失败），
//! 覆盖——双塔互不阻塞、全量跑完落向量表、断点续跑（崩溃残留 running 回收）、
//! 失败 3 次转 failed + 重试恢复、幂等（enq 不重复、upsert 覆盖）。

use std::sync::atomic::AtomicBool;

use core_config::{ModelSpec, SpecKind};
use engine_embed::EmbedProvider;
use rusqlite::Connection;
use svc_pipeline::{
    embed_failed_list, embed_kind, embed_retry_failed, embed_stats, enq_unembedded,
    run_embed_queue, EmbedScope,
};

fn spec(name: &str) -> ModelSpec {
    ModelSpec {
        name: name.to_string(),
        kind: SpecKind::Embedding,
        dims: DIMS,
        modalities: vec!["image".into(), "text".into()],
        preprocess: serde_yaml::Value::Null,
        normalized: true,
        provider: "fake".into(),
    }
}

/// 1×1 PNG（真实可读文件即可，假 provider 不解码）。
const PNG: [u8; 67] = [
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

const DIMS: u32 = 8;

/// 假 provider：向量 = 由路径 hash 派生的确定值，L2 归一。
/// `fail_paths` 命中的路径恒失败（重试语义验证）。
struct FakeProvider {
    spec: ModelSpec,
    fail_paths: Vec<String>,
}

impl FakeProvider {
    fn new(name: &str) -> Self {
        Self {
            spec: spec(name),
            fail_paths: vec![],
        }
    }
    fn with_failures(name: &str, fail_paths: Vec<String>) -> Self {
        Self {
            spec: spec(name),
            fail_paths,
        }
    }
    fn vec_of(path: &str) -> Vec<f32> {
        let mut h: u64 = 1469598103934665603;
        for b in path.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        let v: Vec<f32> = (0..DIMS as usize)
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
            out.push(Self::vec_of(&p.display().to_string()));
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

/// 建库 + n 张图（含 primary resource 行——向量挂 resource 粒度）。
fn setup(dir: &std::path::Path, n_images: usize) -> Connection {
    let conn = core_storage::open(&dir.join("test.db")).unwrap();
    for i in 0..n_images {
        let p = dir.join(format!("img{i}.png"));
        std::fs::write(&p, PNG).unwrap();
        conn.execute(
            "INSERT INTO assets (mediaType, absPath, storeMode, fileName, fileExt, fileSz, mtime,
                source, searchable, deleted, approvalStatus, starRating, importedAt)
             VALUES ('image', ?1, 'reference', ?2, 'png', 67, 0, 'local', 1, 0, 'approved', 0, unixepoch())",
            rusqlite::params![p.display().to_string(), format!("img{i}.png")],
        )
        .unwrap();
        let asset_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1, 'primary', 0, 1, 1)",
            [asset_id],
        )
        .unwrap();
    }
    conn
}

fn primary_resource_id(conn: &Connection, asset_id: i64) -> i64 {
    conn.query_row(
        "SELECT resourceId FROM resource WHERE assetId = ?1 AND role = 'primary'",
        [asset_id],
        |r| r.get(0),
    )
    .unwrap()
}

fn noprogress(_p: core_types::EmbedProgress) {}

#[test]
fn full_run_semantic_and_dedup_queues_independent() {
    let dir = tempfile::tempdir().unwrap();
    let conn = setup(dir.path(), 12);
    let sem = FakeProvider::new("fake-semantic");
    let ded = FakeProvider::new("fake-dedup");
    let cancel = AtomicBool::new(false);

    // 语义塔全跑
    let enq = enq_unembedded(&conn, "fake-semantic", EmbedScope::Semantic).unwrap();
    assert_eq!(enq, 12);
    let rep = run_embed_queue(&conn, &sem, EmbedScope::Semantic, &cancel, &mut noprogress).unwrap();
    assert_eq!(rep.done, 12);
    assert_eq!(rep.failed, 0);
    assert_eq!(rep.cancelled, false);

    // 向量落表 + 状态机推进
    assert_eq!(
        core_storage::vecstore::count(&conn, "fake-semantic").unwrap(),
        12
    );
    let (emb_done, ded_none): (i64, i64) = conn
        .query_row(
            "SELECT SUM(emb_status='done'), SUM(COALESCE(dedup_status,'none')='none') FROM assets",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(emb_done, 12, "语义塔全部 done");
    assert_eq!(ded_none, 12, "去重塔 untouched（互不阻塞）");

    // 去重塔独立跑
    let enq = enq_unembedded(&conn, "fake-dedup", EmbedScope::Dedup).unwrap();
    assert_eq!(enq, 12);
    let rep = run_embed_queue(&conn, &ded, EmbedScope::Dedup, &cancel, &mut noprogress).unwrap();
    assert_eq!(rep.done, 12);
    assert_eq!(
        core_storage::vecstore::count(&conn, "fake-dedup").unwrap(),
        12
    );
    // 两表共存（禁混算的结构保证）
    let tables = core_storage::vecstore::tables(&conn).unwrap();
    assert_eq!(tables.len(), 2);

    // emb_model / dedup_model 各自记录
    let (em, dm): (String, String) = conn
        .query_row(
            "SELECT emb_model, dedup_model FROM assets WHERE assetId = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(em, "fake-semantic");
    assert_eq!(dm, "fake-dedup");
}

#[test]
fn vectors_attach_to_primary_resource_and_knn_works() {
    let dir = tempfile::tempdir().unwrap();
    let conn = setup(dir.path(), 3);
    let sem = FakeProvider::new("fake-semantic");
    let cancel = AtomicBool::new(false);
    enq_unembedded(&conn, "fake-semantic", EmbedScope::Semantic).unwrap();
    run_embed_queue(&conn, &sem, EmbedScope::Semantic, &cancel, &mut noprogress).unwrap();

    // resourceId 粒度：向量 key = primary resource id
    let rid0 = primary_resource_id(&conn, 1);
    let q = FakeProvider::vec_of(&dir.path().join("img0.png").display().to_string());
    let hits = core_storage::vecstore::knn(&conn, "fake-semantic", &q, 1).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, rid0);
    assert!(hits[0].1.abs() < 1e-4, "自匹配 distance≈0");
}

#[test]
fn crash_resume_requeues_running_without_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let conn = setup(dir.path(), 8);
    enq_unembedded(&conn, "fake-semantic", EmbedScope::Semantic).unwrap();

    // 模拟崩溃残留：2 个 job 卡 running
    conn.execute(
        "UPDATE job_queue SET status='running' WHERE id IN (
             SELECT id FROM job_queue WHERE kind = ?1 ORDER BY id LIMIT 2)",
        [embed_kind("fake-semantic")],
    )
    .unwrap();
    // 其中一个已写向量但状态没推进（幂等 upsert 必须覆盖不重复）
    let rid = primary_resource_id(&conn, 1);
    core_storage::vecstore::ensure_table(&conn, "fake-semantic", DIMS).unwrap();
    core_storage::vecstore::upsert(&conn, "fake-semantic", &[(rid, &[0.5f32; 8])]).unwrap();

    let sem = FakeProvider::new("fake-semantic");
    let cancel = AtomicBool::new(false);
    let rep = run_embed_queue(&conn, &sem, EmbedScope::Semantic, &cancel, &mut noprogress).unwrap();
    assert_eq!(rep.done, 8, "残留 running 回收后续跑全部完成");

    // job 行无重复：8 个资产恰好 8 行
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM job_queue WHERE kind = ?1",
            [embed_kind("fake-semantic")],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 8);
    // 向量行数 = 资产数（upsert 覆盖不追加）
    assert_eq!(
        core_storage::vecstore::count(&conn, "fake-semantic").unwrap(),
        8
    );
}

#[test]
fn permanent_failure_three_attempts_then_failed_and_retry_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let conn = setup(dir.path(), 6);
    // img3 恒失败（损坏文件语义）
    let bad = dir.path().join("img3.png").display().to_string();
    let sem = FakeProvider::with_failures("fake-semantic", vec![bad.clone()]);
    let cancel = AtomicBool::new(false);

    enq_unembedded(&conn, "fake-semantic", EmbedScope::Semantic).unwrap();
    let rep = run_embed_queue(&conn, &sem, EmbedScope::Semantic, &cancel, &mut noprogress).unwrap();
    assert_eq!(rep.done, 5);
    assert_eq!(rep.failed, 1, "恰好 1 个资产失败");
    assert!(rep.errors.iter().any(|e| e.contains("img3")));

    // 3 次尝试后 job 终态 failed + attempts=3 + lastError 留痕
    let (attempts, status, last_err): (i64, String, String) = conn
        .query_row(
            "SELECT attempts, status, COALESCE(lastError,'') FROM job_queue
             WHERE kind = ?1 AND targetId = (SELECT assetId FROM assets WHERE fileName = 'img3.png')",
            [embed_kind("fake-semantic")],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(attempts, 3);
    assert_eq!(status, "failed");
    assert!(last_err.contains("injected failure"));

    // 失败列表 + 资产状态 failed
    let failed = embed_failed_list(&conn, 10).unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].model, "fake-semantic");
    let emb: String = conn
        .query_row(
            "SELECT emb_status FROM assets WHERE fileName = 'img3.png'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(emb, "failed");

    // 统计口径
    let st = embed_stats(&conn, "fake-semantic").unwrap();
    assert_eq!(st.semantic.done, 5);
    assert_eq!(st.semantic.failed, 1);
    assert_eq!(st.semantic.vectors, 5);

    // 修复后重试 → 全部完成（幂等：其余 5 个不再入队）
    let fixed = FakeProvider::new("fake-semantic");
    let reset = embed_retry_failed(&conn, "fake-semantic", EmbedScope::Semantic).unwrap();
    assert_eq!(reset, 1, "只重置失败的 1 个");
    let rep = run_embed_queue(
        &conn,
        &fixed,
        EmbedScope::Semantic,
        &cancel,
        &mut noprogress,
    )
    .unwrap();
    assert_eq!(rep.done, 1);
    assert_eq!(
        core_storage::vecstore::count(&conn, "fake-semantic").unwrap(),
        6
    );
    let st = embed_stats(&conn, "fake-semantic").unwrap();
    assert_eq!(st.semantic.failed, 0);
}

#[test]
fn enqueue_idempotent_and_skips_done() {
    let dir = tempfile::tempdir().unwrap();
    let conn = setup(dir.path(), 5);
    let cancel = AtomicBool::new(false);
    let sem = FakeProvider::new("fake-semantic");
    enq_unembedded(&conn, "fake-semantic", EmbedScope::Semantic).unwrap();
    run_embed_queue(&conn, &sem, EmbedScope::Semantic, &cancel, &mut noprogress).unwrap();

    // 已全部 done → 再入队 0 条
    let n = enq_unembedded(&conn, "fake-semantic", EmbedScope::Semantic).unwrap();
    assert_eq!(n, 0);
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM job_queue WHERE kind = ?1",
            [embed_kind("fake-semantic")],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 5, "job 行不重复追加");
}

#[test]
fn ineligible_assets_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let conn = setup(dir.path(), 3);
    // 一个删除、一个失联 → 不入队
    conn.execute(
        "UPDATE assets SET deleted = 1 WHERE fileName = 'img1.png'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE assets SET missing = 1 WHERE fileName = 'img2.png'",
        [],
    )
    .unwrap();
    let n = enq_unembedded(&conn, "fake-semantic", EmbedScope::Semantic).unwrap();
    assert_eq!(n, 1, "只入队合格资产");
}
