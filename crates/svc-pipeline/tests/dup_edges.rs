//! KI-2b（卡 t_8c1c9b89）：向量判重 → `dup_edges` 关系表的**行为**测试。
//!
//! 钉住的事（改之前先读 `migrations/0019_dup_edges.sql` 与
//! `embed_queue::judge_asset_dup_edges` 的语义注释）：
//! 1. 阈值分档：≥0.98 同一视觉内容（near_visual）/ [0.90,0.98) 疑似变体（variant）/ 其余不成边；
//! 2. 判重塔向量一落库就判（embed 队列 Dedup scope），语义塔不写边；
//! 3. 先删后写：重嵌入后「已经不相似了」的旧边会消失（自愈，不留历史结论）；
//! 4. 回收站资产（deleted=1）两边都不参与（不判、也不被别人连）；
//! 5. 存量补判可重入 + truncated flag + 账本状态（vectors==0 → 未检测）。

use std::sync::atomic::AtomicBool;

use core_config::{ModelSpec, SpecKind};
use engine_embed::EmbedProvider;
use rusqlite::Connection;
use svc_pipeline::{
    backfill_dup_edges, dup_edge_kind, dup_judge_status, enq_unembedded, judge_asset_dup_edges,
    run_embed_queue, EmbedScope, DUP_NEAR_THRESHOLD, DUP_VARIANT_THRESHOLD,
};

const MODEL: &str = "fake-dedup";
const DIMS: u32 = 4;

fn spec(name: &str) -> ModelSpec {
    ModelSpec {
        name: name.to_string(),
        kind: SpecKind::Embedding,
        dims: DIMS,
        modalities: vec!["image".into()],
        preprocess: serde_yaml::Value::Null,
        normalized: true,
        provider: "fake".into(),
    }
}

const PNG: [u8; 67] = [
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

/// 定长向量 provider：所有图返回同一向量（= 全库互为同一视觉内容，便于断言成边）。
struct ConstProvider {
    spec: ModelSpec,
}

impl EmbedProvider for ConstProvider {
    fn load(&mut self) -> engine_embed::Result<()> {
        Ok(())
    }
    fn embed_images(&self, paths: &[std::path::PathBuf]) -> engine_embed::Result<Vec<Vec<f32>>> {
        Ok(paths.iter().map(|_| vec![1.0, 0.0, 0.0, 0.0]).collect())
    }
    fn embed_texts(&self, _texts: &[&str]) -> engine_embed::Result<Vec<Vec<f32>>> {
        Err(engine_embed::EmbedError::Unsupported("image-only".into()))
    }
    fn ready(&self) -> bool {
        true
    }
    fn spec(&self) -> &ModelSpec {
        &self.spec
    }
}

fn noprogress(_p: core_types::EmbedProgress) {}

/// 建库 + n 张图（含 primary resource 行），返回 (assetId, resourceId) 列表。
fn seed(dir: &std::path::Path, n: usize) -> (Connection, Vec<(i64, i64)>) {
    let conn = core_storage::open(&dir.join("test.db")).unwrap();
    let mut out = Vec::new();
    for i in 0..n {
        let p = dir.join(format!("img{i}.png"));
        std::fs::write(&p, PNG).unwrap();
        conn.execute(
            "INSERT INTO assets (mediaType, absPath, storeMode, fileName, fileExt, fileSz, mtime,
                source, searchable, deleted, approvalStatus, starRating, importedAt)
             VALUES ('image', ?1, 'reference', ?2, 'png', 67, 0, 'local', 1, 0, 'approved', 0, unixepoch())",
            rusqlite::params![p.display().to_string(), format!("img{i}.png")],
        )
        .unwrap();
        let aid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1, 'primary', 0, 10, 10)",
            [aid],
        )
        .unwrap();
        out.push((aid, conn.last_insert_rowid()));
    }
    (conn, out)
}

/// 直接把向量写进判重表（模拟「向量已在库、边还没判」的存量状态）。
fn put_vec(conn: &Connection, model: &str, rid: i64, v: [f32; DIMS as usize]) {
    core_storage::vecstore::ensure_table(conn, model, DIMS).unwrap();
    core_storage::vecstore::upsert(conn, model, &[(rid, &v)]).unwrap();
}

fn edge_rows(conn: &Connection) -> Vec<(i64, i64, f32, String)> {
    let mut stmt = conn
        .prepare("SELECT a, b, score, kind FROM dup_edges ORDER BY a, b")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

/// cos = 0.99 / 0.95 的 4 维向量（与 [1,0,0,0] 的夹角）。
fn vec_cos(c: f32) -> [f32; DIMS as usize] {
    let s = (1.0 - c * c).sqrt();
    [c, s, 0.0, 0.0]
}

#[test]
fn edge_kind_thresholds_are_exact() {
    assert_eq!(dup_edge_kind(1.0), Some("near_visual"));
    assert_eq!(dup_edge_kind(DUP_NEAR_THRESHOLD), Some("near_visual"), "阈值本身算 near_visual");
    assert_eq!(dup_edge_kind(DUP_NEAR_THRESHOLD - 0.001), Some("variant"));
    assert_eq!(dup_edge_kind(DUP_VARIANT_THRESHOLD), Some("variant"), "阈值本身算 variant");
    assert_eq!(dup_edge_kind(DUP_VARIANT_THRESHOLD - 0.001), None, "低于变体阈值不成边");
    assert_eq!(dup_edge_kind(f32::NAN), None, "脏分数不造边");
    assert_eq!(dup_edge_kind(f32::INFINITY), None, "脏分数不造边");
}

#[test]
fn judge_writes_near_and_variant_edges_but_not_trash_or_strangers() {
    let dir = tempfile::tempdir().unwrap();
    let (conn, ids) = seed(dir.path(), 5);
    let (a1, r1) = ids[0];
    let (_a2, r2) = ids[1];
    let (_a3, r3) = ids[2];
    let (a4, r4) = ids[3];
    let (_a5, r5) = ids[4];
    put_vec(&conn, MODEL, r1, [1.0, 0.0, 0.0, 0.0]);
    put_vec(&conn, MODEL, r2, vec_cos(0.99));
    put_vec(&conn, MODEL, r3, vec_cos(0.95));
    put_vec(&conn, MODEL, r4, vec_cos(0.999)); // 回收站里的图：不该被连
    put_vec(&conn, MODEL, r5, [0.0, 0.0, 1.0, 0.0]); // 无关图：低于阈值
    conn.execute("UPDATE assets SET deleted = 1 WHERE assetId = ?1", [a4])
        .unwrap();

    let written = judge_asset_dup_edges(&conn, MODEL, a1).unwrap();
    assert_eq!(written, 2, "只有 0.99 与 0.95 两个邻居成边");
    let edges = edge_rows(&conn);
    assert_eq!(edges.len(), 2);
    assert_eq!((edges[0].0, edges[0].1, edges[0].3.as_str()), (a1, ids[1].0, "near_visual"));
    assert!((edges[0].2 - 0.99).abs() < 1e-3, "score 落在 0.99 附近: {}", edges[0].2);
    assert_eq!((edges[1].0, edges[1].1, edges[1].3.as_str()), (a1, ids[2].0, "variant"));
    assert!((edges[1].2 - 0.95).abs() < 1e-3, "score 落在 0.95 附近: {}", edges[1].2);

    // 幂等：再判一次，边不变
    assert_eq!(judge_asset_dup_edges(&conn, MODEL, a1).unwrap(), 2);
    assert_eq!(edge_rows(&conn).len(), 2);

    // 回收站资产本身不判（判了也进不了分组，只会留下指向垃圾的边）
    assert_eq!(judge_asset_dup_edges(&conn, MODEL, a4).unwrap(), 0);
    assert_eq!(edge_rows(&conn).len(), 2, "回收站资产的判定不新增边");
}

#[test]
fn judge_is_self_healing_after_reembed() {
    let dir = tempfile::tempdir().unwrap();
    let (conn, ids) = seed(dir.path(), 3);
    let (a1, r1) = ids[0];
    let (a2, r2) = ids[1];
    let (_a3, r3) = ids[2];
    put_vec(&conn, MODEL, r1, [1.0, 0.0, 0.0, 0.0]);
    put_vec(&conn, MODEL, r2, vec_cos(0.99));
    put_vec(&conn, MODEL, r3, vec_cos(0.95));
    judge_asset_dup_edges(&conn, MODEL, a1).unwrap();
    assert_eq!(edge_rows(&conn).len(), 2);

    // 重嵌入：a2 变成无关图 → 旧边必须消失（先删后写，不留历史结论）
    core_storage::vecstore::upsert(&conn, MODEL, &[(r2, &[0.0, 0.0, 1.0, 0.0])]).unwrap();
    let written = judge_asset_dup_edges(&conn, MODEL, a1).unwrap();
    assert_eq!(written, 1, "只剩 0.95 的那条变体边");
    let edges = edge_rows(&conn);
    assert_eq!(edges.len(), 1);
    assert_eq!((edges[0].0, edges[0].1), (a1, ids[2].0));
    assert!(edges.iter().all(|e| e.0 != a2 && e.1 != a2), "a2 的边已清掉");
}

#[test]
fn judge_returns_zero_without_resource_vector_or_table() {
    let dir = tempfile::tempdir().unwrap();
    let (conn, ids) = seed(dir.path(), 3);
    let (a1, r1) = ids[0];

    // 1) 判重表还不存在（判重塔从未跑过）
    assert_eq!(judge_asset_dup_edges(&conn, MODEL, a1).unwrap(), 0);

    // 2) 表有了但这张图没向量
    core_storage::vecstore::ensure_table(&conn, MODEL, DIMS).unwrap();
    assert_eq!(judge_asset_dup_edges(&conn, MODEL, a1).unwrap(), 0);

    // 3) 没有 primary resource 的资产（只有附件 resource）
    put_vec(&conn, MODEL, r1, [1.0, 0.0, 0.0, 0.0]);
    let (a3, _r3) = ids[2];
    conn.execute("DELETE FROM resource WHERE assetId = ?1", [a3])
        .unwrap();
    assert_eq!(judge_asset_dup_edges(&conn, MODEL, a3).unwrap(), 0);
}

#[test]
fn dedup_scope_queue_judges_edges_semantic_scope_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let (conn, ids) = seed(dir.path(), 3);
    let cancel = AtomicBool::new(false);

    // 语义塔：即使向量完全一样也不写去重边（两塔职责分离）
    enq_unembedded(&conn, "fake-semantic", EmbedScope::Semantic).unwrap();
    let sem = ConstProvider {
        spec: spec("fake-semantic"),
    };
    let rep = run_embed_queue(&conn, &sem, EmbedScope::Semantic, &cancel, &mut noprogress).unwrap();
    assert_eq!(rep.done, 3);
    assert_eq!(edge_rows(&conn).len(), 0, "语义塔不产生去重边");

    // 判重塔：3 张同向量图 → 两两成边（完全图 3 条）
    enq_unembedded(&conn, MODEL, EmbedScope::Dedup).unwrap();
    let ded = ConstProvider { spec: spec(MODEL) };
    let rep = run_embed_queue(&conn, &ded, EmbedScope::Dedup, &cancel, &mut noprogress).unwrap();
    assert_eq!(rep.done, 3);
    assert!(rep.errors.is_empty(), "判重不该报错: {:?}", rep.errors);
    let edges = edge_rows(&conn);
    assert_eq!(edges.len(), 3, "3 张两两成边: {edges:?}");
    assert!(edges.iter().all(|e| e.3 == "near_visual"), "score=1.0 全是同一视觉内容");
    // 边落在真实 assetId 上（不是 resourceId），且 a<b 规范化
    let expected: Vec<(i64, i64)> = vec![
        (ids[0].0, ids[1].0),
        (ids[0].0, ids[2].0),
        (ids[1].0, ids[2].0),
    ];
    assert_eq!(
        edges.iter().map(|e| (e.0, e.1)).collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn backfill_is_reentrant_flags_truncation_and_reports_status() {
    let dir = tempfile::tempdir().unwrap();
    let (conn, ids) = seed(dir.path(), 4);
    for (_, rid) in &ids {
        put_vec(&conn, MODEL, *rid, [1.0, 0.0, 0.0, 0.0]);
    }

    // 判之前：向量在、边没有 → 「未检测完」而不是「无重复」
    let st = dup_judge_status(&conn, MODEL).unwrap();
    assert_eq!(st.vectors, 4);
    assert_eq!(st.edges, 0);
    assert!(!st.backfill_done);
    assert!(st.last_judge_at.is_none());

    // 分批：只判 2 个 → truncated，账本不算走完
    let r1 = backfill_dup_edges(&conn, MODEL, 2).unwrap();
    assert_eq!(r1.scanned, 2);
    assert!(r1.truncated, "还有 2 个资产没判");
    assert_eq!(r1.vectors, 4);
    let st = dup_judge_status(&conn, MODEL).unwrap();
    assert!(!st.backfill_done, "中途停下不算走完");
    assert!(st.backfill_truncated);
    assert!(st.last_judge_at.is_some());

    // 续跑到底（可重入）：4 张同向量 → 6 条边
    let r2 = backfill_dup_edges(&conn, MODEL, 0).unwrap();
    assert_eq!(r2.scanned, 4);
    assert!(!r2.truncated);
    assert_eq!(r2.edges, 6, "4 张两两成边");
    let st = dup_judge_status(&conn, MODEL).unwrap();
    assert!(st.backfill_done);
    assert!(!st.backfill_truncated);
    assert_eq!(st.edges, 6);

    // 再跑一遍：边不重复（先删后写 + 主键幂等）
    let r3 = backfill_dup_edges(&conn, MODEL, 0).unwrap();
    assert_eq!(r3.edges, 6);
    assert_eq!(edge_rows(&conn).len(), 6);
}

#[test]
fn backfill_without_dedup_vectors_marks_done_but_reports_zero_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let (conn, _ids) = seed(dir.path(), 2);
    let r = backfill_dup_edges(&conn, MODEL, 0).unwrap();
    assert_eq!(r.vectors, 0, "判重塔没跑过 → vectors=0（UI 该显示「未检测」）");
    assert_eq!(r.scanned, 0);
    assert!(!r.truncated);
    assert_eq!(edge_rows(&conn).len(), 0);
    let st = dup_judge_status(&conn, MODEL).unwrap();
    assert_eq!(st.vectors, 0);
    assert!(st.backfill_done, "没向量=无事可做，别让 UI 永远卡「检测中」");
}

#[test]
fn backfill_skips_trash_assets() {
    let dir = tempfile::tempdir().unwrap();
    let (conn, ids) = seed(dir.path(), 3);
    for (_, rid) in &ids {
        put_vec(&conn, MODEL, *rid, [1.0, 0.0, 0.0, 0.0]);
    }
    conn.execute("UPDATE assets SET deleted = 1 WHERE assetId = ?1", [ids[2].0])
        .unwrap();
    let r = backfill_dup_edges(&conn, MODEL, 0).unwrap();
    assert_eq!(r.scanned, 2, "回收站资产不扫");
    assert_eq!(r.edges, 1, "只剩 1 对未删资产成边");
    let edges = edge_rows(&conn);
    assert_eq!((edges[0].0, edges[0].1), (ids[0].0, ids[1].0));
}

/// 彻底删除（回收站 purge）资产 → 它的边随之消失（FK CASCADE：不留脏边）。
#[test]
fn purge_asset_cascades_its_edges() {
    let dir = tempfile::tempdir().unwrap();
    let (conn, ids) = seed(dir.path(), 2);
    for (_, rid) in &ids {
        put_vec(&conn, MODEL, *rid, [1.0, 0.0, 0.0, 0.0]);
    }
    let r = backfill_dup_edges(&conn, MODEL, 0).unwrap();
    assert_eq!(r.edges, 1);

    conn.execute("DELETE FROM assets WHERE assetId = ?1", [ids[1].0])
        .unwrap();
    assert_eq!(edge_rows(&conn).len(), 0, "asset 删了，边必须一起没");
}
