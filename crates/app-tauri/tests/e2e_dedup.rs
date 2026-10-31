//! D7 端到端查重测试（零模型依赖：向量直接注入 sqlite-vec，验证的是查重引擎本身）：
//!
//! 注入 10 组已知重复组（每组 3 个资产 + 10 个孤立干扰项，共 40 资产）：
//! - 组 0-4：同 fileHash + 向量完全相同（cos=1.0 ≥0.98 → near 带）
//! - 组 5-9：同 fileHash + 向量受控扰动（cos∈[0.90,0.98) → variant 带）
//! 验收：
//! 1. exact_dup_groups 100% 召回 10 组 fileHash；
//! 2. find_duplicates 100% 召回：5 near + 5 variant，带归属正确、成员完整、无干扰项误报；
//! 3. 软删除（保留 1 张删其余）→ 组从所有读模型消失；回收站可见；幂等；
//! 4. 恢复 → 组重新 100% 召回。
//!
//! 真实模型下的近重复/变体召回由 e2e_search.rs（真 DINOv3）覆盖；本文件用受控
//! 向量把「召回率」变成可精确断言的确定性命题（mock 注入，2026-10-13 裁决口径）。

use std::path::PathBuf;

use engine_embed::{EmbedError, EmbedProvider};
use engine_search::DupOptions;
use rusqlite::Connection;

use core_config::ModelSpec;
use svc_library::{
    exact_dup_groups, find_duplicates, list_assets, list_deleted, restore_assets,
    soft_delete_assets,
};

const DIMS: u32 = 64;
const MODEL: &str = "mock-dedup-64";

/// 只提供 spec 的假 provider：find_duplicates 只用 spec().name 路由向量表，
/// 不调用 embed（向量已直接注入 vecstore）。
struct SpecOnly {
    spec: ModelSpec,
}

impl EmbedProvider for SpecOnly {
    fn load(&mut self) -> engine_embed::Result<()> {
        Ok(())
    }
    fn embed_images(&self, _paths: &[PathBuf]) -> engine_embed::Result<Vec<Vec<f32>>> {
        Err(EmbedError::NotLoaded("spec-only".into()))
    }
    fn embed_texts(&self, _texts: &[&str]) -> engine_embed::Result<Vec<Vec<f32>>> {
        Err(EmbedError::NotLoaded("spec-only".into()))
    }
    fn ready(&self) -> bool {
        true
    }
    fn spec(&self) -> &ModelSpec {
        &self.spec
    }
}

fn mock_spec(name: &str) -> ModelSpec {
    ModelSpec {
        name: name.into(),
        kind: core_config::SpecKind::Embedding,
        dims: DIMS,
        modalities: vec!["image".into()],
        preprocess: Default::default(),
        normalized: true,
        provider: "mock".into(),
    }
}

fn unit(dims: u32, at: usize) -> Vec<f32> {
    let mut v = vec![0.0; dims as usize];
    v[at] = 1.0;
    v
}

/// normalize(e_base + eps * e_noise)：与 base 的 cos = 1/sqrt(1+eps²)。
fn perturbed(dims: u32, base: usize, noise: usize, eps: f32) -> Vec<f32> {
    let mut v = unit(dims, base);
    v[noise] = eps;
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter_mut().for_each(|x| *x /= norm);
    v
}

/// 注入一个资产（同 fileHash 组由调用方控制）。返回 (assetId, resourceId)。
fn insert_asset(conn: &Connection, name: &str, file_hash: &str) -> (i64, i64) {
    conn.execute(
        "INSERT INTO assets (mediaType, absPath, storeMode, fileName, fileExt, fileSz, mtime,
                             fileHash, source, importedAt)
         VALUES ('image', ?1, 'reference', ?2, 'jpg', 1024, 0, ?3, 'userUpload', unixepoch())",
        rusqlite::params![format!("/tmp/d7/{name}.jpg"), name, file_hash],
    )
    .unwrap();
    let asset_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1, 'primary', 0, 64, 64)",
        [asset_id],
    )
    .unwrap();
    (asset_id, conn.last_insert_rowid())
}

fn upsert_vec(conn: &Connection, rid: i64, v: &[f32]) {
    core_storage::vecstore::ensure_table(conn, MODEL, DIMS).unwrap();
    core_storage::vecstore::upsert(conn, MODEL, &[(rid, v)]).unwrap();
}

/// 10 组 × 3 成员 + 10 干扰项。组 g 向量基座 = e_{g}（近组）或受控扰动（变体组）。
/// 返回 (组资产 id 列表, 干扰项资产 id 列表)。
fn seed(conn: &Connection) -> (Vec<Vec<i64>>, Vec<i64>) {
    let mut groups = Vec::new();
    for g in 0..10usize {
        let hash = format!("exact-hash-{g:02}");
        let base_dim = g; // 组间正交（cos=0，绝不互串）
        let mut members = Vec::new();
        for m in 0..3usize {
            let (aid, rid) = insert_asset(conn, &format!("g{g:02}m{m}"), &hash);
            let v = if g < 5 {
                // near 带：与锚完全同向量（cos=1.0）；第 3 员给 0.993 的小扰动仍在带内
                match m {
                    2 => perturbed(DIMS, base_dim, 40, 0.12), // cos≈0.9893 ≥0.98
                    _ => unit(DIMS, base_dim),
                }
            } else {
                // variant 带：锚为 e_g，成员 cos≈0.95 / 0.93（∈[0.90,0.98)）
                match m {
                    0 => unit(DIMS, base_dim),
                    1 => perturbed(DIMS, base_dim, 41, 0.33), // cos≈0.9497
                    _ => perturbed(DIMS, base_dim, 42, 0.40), // cos≈0.9285
                }
            };
            upsert_vec(conn, rid, &v);
            members.push(aid);
        }
        groups.push(members);
    }
    let mut singles = Vec::new();
    for s in 0..10usize {
        let (aid, rid) = insert_asset(
            conn,
            &format!("single{s:02}"),
            &format!("single-hash-{s:02}"),
        );
        upsert_vec(conn, rid, &unit(DIMS, 50 + s)); // 独立维度，与所有组正交
        singles.push(aid);
    }
    (groups, singles)
}

#[test]
fn d7_dedup_full_cycle() {
    let dir = std::env::temp_dir().join(format!("dam-d7-dedup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let conn = core_storage::open(&dir.join("dam.db")).unwrap();
    let (groups, singles) = seed(&conn);
    let provider = SpecOnly {
        spec: mock_spec(MODEL),
    };

    // ---------- 1. 精确重复：10 组 100% 召回 ----------
    let exact = exact_dup_groups(&conn).unwrap();
    println!("== exact_dup_groups 原始输出（{} 组）==", exact.len());
    for eg in &exact {
        println!(
            "  fileHash={} members={:?}",
            eg.file_hash,
            eg.members
                .iter()
                .map(|m| (m.asset_id, m.file_name.clone()))
                .collect::<Vec<_>>()
        );
    }
    assert_eq!(exact.len(), 10, "精确组数 = 10（实际 {}）", exact.len());
    let mut recalled: Vec<String> = exact.iter().map(|g| g.file_hash.clone()).collect();
    recalled.sort();
    let mut expect: Vec<String> = (0..10).map(|g| format!("exact-hash-{g:02}")).collect();
    expect.sort();
    assert_eq!(recalled, expect, "10 组 fileHash 全量召回，无遗漏无多余");
    for eg in &exact {
        assert_eq!(eg.members.len(), 3, "组 {} 应 3 成员", eg.file_hash);
    }

    // ---------- 2. 向量查重：5 near + 5 variant，100% 召回 ----------
    let dups = find_duplicates(&conn, &provider, &DupOptions::default()).unwrap();
    println!("== find_duplicates 原始输出（{} 组）==", dups.len());
    for dg in &dups {
        println!(
            "  band={} anchor=#{} members={:?}",
            dg.band,
            dg.anchorAssetId,
            dg.members
                .iter()
                .map(|m| (m.assetId, m.resourceId, m.score))
                .collect::<Vec<_>>()
        );
    }
    let near: Vec<_> = dups.iter().filter(|d| d.band == "near").collect();
    let variant: Vec<_> = dups.iter().filter(|d| d.band == "variant").collect();
    assert_eq!(near.len(), 5, "near 组数 = 5（实际 {}）", near.len());
    assert_eq!(
        variant.len(),
        5,
        "variant 组数 = 5（实际 {}）",
        variant.len()
    );

    // 逐组校验成员完整（含锚 3 人）且无干扰项混入
    let mut seen_members: Vec<i64> = Vec::new();
    for (gi, expect_members) in groups.iter().enumerate() {
        let want_band = if gi < 5 { "near" } else { "variant" };
        let pool: Vec<_> = dups.iter().filter(|d| d.band == want_band).collect();
        let hit = pool.iter().find(|d| {
            let mut ids = vec![d.anchorAssetId];
            ids.extend(d.members.iter().map(|m| m.assetId));
            ids.sort_unstable();
            let mut want = expect_members.clone();
            want.sort_unstable();
            ids == want
        });
        assert!(
            hit.is_some(),
            "组 {gi}（{want_band}）未被完整召回：期望 {:?}",
            expect_members
        );
        seen_members.extend(expect_members);
    }
    let intruders: Vec<i64> = seen_members
        .iter()
        .copied()
        .filter(|id| singles.contains(id))
        .collect();
    assert!(intruders.is_empty(), "干扰项被误报进组：{intruders:?}");
    // near 组成员分 ≥0.98；variant 组成员分 ∈[0.90,0.98)
    for d in &near {
        for m in &d.members {
            assert!(m.score >= 0.98, "near 组出现 <0.98 成员分 {}", m.score);
        }
    }
    for d in &variant {
        let best = d.members.iter().map(|m| m.score).fold(f32::MIN, f32::max);
        assert!(
            (0.90..0.98).contains(&best),
            "variant 组最高分 {best} 不在 [0.90,0.98)"
        );
    }

    // ---------- 3. 软删除：保留组 0 首成员，删其余 2 ----------
    let keep = groups[0][0];
    let drop_ids: Vec<i64> = groups[0][1..].to_vec();
    let n = soft_delete_assets(&conn, &drop_ids).unwrap();
    assert_eq!(n, 2, "软删置位 2 行（实际 {n}）");
    let n2 = soft_delete_assets(&conn, &drop_ids).unwrap();
    assert_eq!(n2, 0, "软删幂等：重复删 0 行（实际 {n2}）");

    let exact = exact_dup_groups(&conn).unwrap();
    assert_eq!(
        exact.len(),
        9,
        "软删后精确组 9（组0 只剩 1 活跃成员，不再成组）"
    );
    assert!(
        !exact.iter().any(|g| g.file_hash == "exact-hash-00"),
        "组 0 的 fileHash 不应再出现"
    );
    let dups = find_duplicates(&conn, &provider, &DupOptions::default()).unwrap();
    let near: Vec<_> = dups.iter().filter(|d| d.band == "near").collect();
    assert_eq!(near.len(), 4, "软删后 near 组 4（实际 {}）", near.len());
    for d in &dups {
        let mut ids = vec![d.anchorAssetId];
        ids.extend(d.members.iter().map(|m| m.assetId));
        assert!(
            !ids.contains(&drop_ids[0]) && !ids.contains(&drop_ids[1]),
            "已软删资产不应出现在查重结果"
        );
    }
    let listed = list_assets(&conn, &Default::default()).unwrap();
    assert!(
        !listed.iter().any(|a| a.asset.assetId == drop_ids[0]),
        "软删资产不应出现在资产列表"
    );
    assert!(
        listed.iter().any(|a| a.asset.assetId == keep),
        "保留资产仍在列表"
    );
    let trash = list_deleted(&conn, 200).unwrap();
    let trash_ids: Vec<i64> = trash.iter().map(|a| a.asset.assetId).collect();
    assert_eq!(trash_ids.len(), 2, "回收站 2 项");
    assert!(trash_ids.contains(&drop_ids[0]) && trash_ids.contains(&drop_ids[1]));
    println!(
        "== 软删后回收站原始输出 ==\n  {:?}",
        trash
            .iter()
            .map(|a| (a.asset.assetId, a.asset.fileName.clone()))
            .collect::<Vec<_>>()
    );

    // ---------- 4. 恢复：组 0 重新 100% 召回 ----------
    let r = restore_assets(&conn, &drop_ids).unwrap();
    assert_eq!(r, 2, "恢复 2 行（实际 {r}）");
    let exact = exact_dup_groups(&conn).unwrap();
    assert_eq!(exact.len(), 10, "恢复后精确组回到 10");
    assert!(exact.iter().any(|g| g.file_hash == "exact-hash-00"));
    let dups = find_duplicates(&conn, &provider, &DupOptions::default()).unwrap();
    assert_eq!(
        dups.iter().filter(|d| d.band == "near").count(),
        5,
        "恢复后 near 组回到 5"
    );
    assert!(list_deleted(&conn, 200).unwrap().is_empty(), "回收站清空");

    let _ = std::fs::remove_dir_all(&dir);
    println!("D7_DEDUP_TEST: ALL PASS");
}
