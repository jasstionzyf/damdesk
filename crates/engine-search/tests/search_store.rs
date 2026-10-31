//! engine-search 集成测试：真实 sqlite-vec 表 + 假向量，覆盖——
//! embModel 路由（两套模型向量共存只命中当前表）、500 假向量过滤/分页边界、
//! 维度不匹配/无表报错、find_duplicates 阈值分带、10 万向量 KNN 性能（#[ignore]，
//! 验收跑 `cargo test --release -- --ignored`，断言 <1s）。

use core_storage::vecstore;
use engine_search::{
    find_duplicates, search_by_vector, DupOptions, SearchFilter, NEAR_DUP_THRESHOLD,
};
use rusqlite::Connection;

const DIMS: u32 = 16;

fn conn() -> Connection {
    core_storage::vecstore::init_vec_ext();
    let c = Connection::open_in_memory().unwrap();
    core_storage::migrations::migrate(&c).unwrap();
    c
}

/// 确定性伪随机向量（xorshift，可复现）。
struct Rng(u64);
impl Rng {
    fn next_f32(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 as f32 / u32::MAX as f32 - 0.5
    }
    fn unit(&mut self, dims: usize) -> Vec<f32> {
        let v: Vec<f32> = (0..dims).map(|_| self.next_f32()).collect();
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.into_iter().map(|x| x / n).collect()
    }
}

/// 插一个资产 + primary resource，返回 (assetId, resourceId)。
fn add_asset(
    conn: &Connection,
    name: &str,
    ext: &str,
    w: i64,
    h: i64,
    imported: i64,
    dir: &str,
) -> (i64, i64) {
    conn.execute(
        "INSERT INTO assets (mediaType, absPath, storeMode, fileName, fileExt, fileSz, mtime,
            source, searchable, deleted, approvalStatus, starRating, importedAt)
         VALUES ('image', ?1, 'reference', ?2, ?3, 100, 0, 'local', 1, 0, 'approved', 0, ?4)",
        rusqlite::params![
            format!("{dir}/{name}.{ext}"),
            format!("{name}.{ext}"),
            ext,
            imported
        ],
    )
    .unwrap();
    let asset_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1, 'primary', 0, ?2, ?3)",
        rusqlite::params![asset_id, w, h],
    )
    .unwrap();
    let rid = conn.last_insert_rowid();
    (asset_id, rid)
}

fn upsert(conn: &Connection, model: &str, rows: &[(i64, Vec<f32>)]) {
    vecstore::ensure_table(conn, model, DIMS).unwrap();
    let borrowed: Vec<(i64, &[f32])> = rows.iter().map(|(r, v)| (*r, v.as_slice())).collect();
    vecstore::upsert(conn, model, &borrowed).unwrap();
}

// ---------- 验收 5：embModel 路由 ----------

#[test]
fn embmodel_routing_hits_only_active_table() {
    let c = conn();
    vecstore::ensure_table(&c, "model-a", DIMS).unwrap();
    vecstore::ensure_table(&c, "model-b", DIMS).unwrap();

    // 同一 resource 在两套模型下向量不同（模型 A：第一维主导；模型 B：第二维主导）
    let (a1, r1) = add_asset(&c, "one", "png", 100, 100, 1000, "/x");
    let (_a2, r2) = add_asset(&c, "two", "png", 100, 100, 1000, "/x");
    let va1 = {
        let mut v = vec![0.0; DIMS as usize];
        v[0] = 1.0;
        v
    };
    let va2 = {
        let mut v = vec![0.0; DIMS as usize];
        v[1] = 1.0;
        v
    };
    let vb1 = {
        let mut v = vec![0.0; DIMS as usize];
        v[2] = 1.0; // 模型 B 空间里 r1 方向完全不同
        v
    };
    upsert(&c, "model-a", &[(r1, va1.clone()), (r2, va2.clone())]);
    upsert(&c, "model-b", &[(r1, vb1), (r2, va2.clone())]);

    // 查 model-a：top1 = r1（第一维方向）；不出现只在 model-b 有的向量语义
    let hits = search_by_vector(
        &c,
        "model-a",
        DIMS as usize,
        &va1,
        2,
        &SearchFilter::default(),
    )
    .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].resourceId, r1);
    assert!((hits[0].score - 1.0).abs() < 1e-3);
    assert_eq!(hits[0].assetId, a1);

    // 查 model-b：同一查询向量在 B 空间最近的是 r2（r1 在 B 空间是第三维）
    let hits = search_by_vector(
        &c,
        "model-b",
        DIMS as usize,
        &va1,
        1,
        &SearchFilter::default(),
    )
    .unwrap();
    assert_eq!(hits[0].resourceId, r2, "路由到 B 表后结果必须按 B 空间排序");

    // 未建表的模型 → NoStore 明确报错
    let err = search_by_vector(
        &c,
        "model-c",
        DIMS as usize,
        &va1,
        1,
        &SearchFilter::default(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("no vector store"), "got {err}");

    // 维度不匹配 → Dim 报错（禁混算）
    let err = search_by_vector(&c, "model-a", 768, &va1, 1, &SearchFilter::default()).unwrap_err();
    assert!(err.to_string().contains("dim mismatch"), "got {err}");
}

// ---------- 验收 3/4 边界：500 假向量 + 过滤/分页 ----------

fn seed_500(c: &Connection) -> Vec<(i64, i64)> {
    let mut rng = Rng(42);
    let mut ids = Vec::new();
    let mut rows = Vec::new();
    for i in 0..500 {
        let ext = if i % 2 == 0 { "jpg" } else { "png" };
        let dir = if i % 3 == 0 {
            "/photos/2023"
        } else {
            "/photos/2024"
        };
        let (a, r) = add_asset(
            c,
            &format!("im{i:03}"),
            ext,
            200 + i,
            300 + i,
            1_700_000_000 + (i as i64) * 1000,
            dir,
        );
        ids.push((a, r));
        rows.push((r, rng.unit(DIMS as usize)));
    }
    upsert(c, "model-a", &rows);
    ids
}

fn target_vec_of(c: &Connection, rid: i64) -> Vec<f32> {
    vecstore::fetch_vectors(c, "model-a", &[rid])
        .unwrap()
        .remove(0)
        .1
}

#[test]
fn filter_ext_boundary_500() {
    let c = conn();
    let ids = seed_500(&c);
    let q = target_vec_of(&c, ids[0].1);

    // 无过滤 k=600（> 500）→ 全库返回但封顶 500
    let hits = search_by_vector(
        &c,
        "model-a",
        DIMS as usize,
        &q,
        600,
        &SearchFilter::default(),
    )
    .unwrap();
    assert_eq!(hits.len(), 500, "k 超过总量时返回全部");
    assert!((hits[0].score - 1.0).abs() < 1e-4, "自匹配第一");
    assert!(hits[0].resourceId == ids[0].1);

    // ext 过滤：q 命中的是 im000（jpg）；png 过滤必须把 top1 排除
    let hits = search_by_vector(
        &c,
        "model-a",
        DIMS as usize,
        &q,
        10,
        &SearchFilter {
            ext: Some("png".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        !hits.iter().any(|h| h.resourceId == ids[0].1),
        "ext=png 必须排除 jpg 自匹配"
    );
    // 精确路径结果正确性：所有命中的文件扩展名都是 png
    for h in &hits {
        let ext: String = c
            .query_row(
                "SELECT fileExt FROM assets WHERE assetId = ?1",
                [h.assetId],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ext, "png");
    }

    // dir 前缀过滤
    let hits = search_by_vector(
        &c,
        "model-a",
        DIMS as usize,
        &q,
        500,
        &SearchFilter {
            dir_prefix: Some("/photos/2024".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(hits.len() > 0 && hits.len() < 500);
    for h in &hits {
        let p: String = c
            .query_row(
                "SELECT absPath FROM assets WHERE assetId = ?1",
                [h.assetId],
                |r| r.get(0),
            )
            .unwrap();
        assert!(p.starts_with("/photos/2024"), "{p}");
    }

    // 尺寸过滤：min_width=600 → 只有 i>=400 的资产（width=200+i）
    let hits = search_by_vector(
        &c,
        "model-a",
        DIMS as usize,
        &q,
        500,
        &SearchFilter {
            min_width: Some(600),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(hits.len(), 100, "width>=600 恰好 100 个");

    // 日期窗：单日（importedAt = 1_700_000_000 + i*1000；i=100 → 1_700_100_000）
    let day = 1_700_100_000i64;
    let hits = search_by_vector(
        &c,
        "model-a",
        DIMS as usize,
        &q,
        500,
        &SearchFilter {
            date_from: Some(day),
            date_to: Some(day),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(hits.len(), 1, "恰好命中单条");
    assert_eq!(hits[0].assetId, ids[100].0);

    // 组合过滤（ext + dir）：两个条件 AND
    let hits = search_by_vector(
        &c,
        "model-a",
        DIMS as usize,
        &q,
        500,
        &SearchFilter {
            ext: Some("png".into()),
            dir_prefix: Some("/photos/2023".into()),
            ..Default::default()
        },
    )
    .unwrap();
    for h in &hits {
        let (ext, p): (String, String) = c
            .query_row(
                "SELECT fileExt, absPath FROM assets WHERE assetId = ?1",
                [h.assetId],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(ext, "png");
        assert!(p.starts_with("/photos/2023"));
    }

    // 空候选：不可能满足的过滤 → 空（不报错）
    let hits = search_by_vector(
        &c,
        "model-a",
        DIMS as usize,
        &q,
        10,
        &SearchFilter {
            ext: Some("webp".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(hits.is_empty());
}

#[test]
fn tag_filter_and_deleted_exclusion() {
    let c = conn();
    let ids = seed_500(&c);
    let q = target_vec_of(&c, ids[0].1);
    // 给 top1 打标签「精选」
    c.execute(
        "INSERT INTO tag (assetId, value, origin, score, createdAt) VALUES (?1, '精选', 'manual', 1.0, 0)",
        [ids[0].0],
    )
    .unwrap();
    let hits = search_by_vector(
        &c,
        "model-a",
        DIMS as usize,
        &q,
        10,
        &SearchFilter {
            tag: Some("精选".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(hits.len(), 1, "只有打了标签的资产命中");
    assert_eq!(hits[0].resourceId, ids[0].1);

    // deleted 资产即使向量在表里也绝不出现
    c.execute(
        "UPDATE assets SET deleted = 1 WHERE assetId = ?1",
        [ids[0].0],
    )
    .unwrap();
    let hits = search_by_vector(
        &c,
        "model-a",
        DIMS as usize,
        &q,
        10,
        &SearchFilter {
            tag: Some("精选".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(hits.is_empty(), "deleted 资产被排除");
    let hits = search_by_vector(
        &c,
        "model-a",
        DIMS as usize,
        &q,
        1,
        &SearchFilter::default(),
    )
    .unwrap();
    assert!(
        hits.iter().all(|h| h.assetId != ids[0].0),
        "无过滤路径同样排除 deleted"
    );
}

// ---------- find_duplicates 阈值分带 ----------

#[test]
fn find_duplicates_bands_near_and_variant() {
    let c = conn();
    let mut rng = Rng(7);
    let mut rows = Vec::new();
    let mut ids = Vec::new();
    for i in 0..6 {
        let (a, r) = add_asset(&c, &format!("d{i}"), "png", 100, 100, 1000, "/d");
        ids.push((a, r));
        rows.push((r, rng.unit(DIMS as usize)));
    }
    // 0/1 完全相同（cos=1）→ near
    rows[1].1 = rows[0].1.clone();
    // 2/3 相似 ~0.93 → variant：v3 = 0.93·v2 + √(1-0.93²)·p（p 为 v2 的正交单位向量）
    {
        let mut p = rng.unit(DIMS as usize);
        let dot: f32 = rows[2].1.iter().zip(&p).map(|(a, b)| a * b).sum();
        for (pi, vi) in p.iter_mut().zip(&rows[2].1) {
            *pi -= dot * vi;
        }
        let pn: f32 = p.iter().map(|x| x * x).sum::<f32>().sqrt();
        let s = 0.368f32; // √(1-0.93²)≈0.368
        let mut v: Vec<f32> = rows[2]
            .1
            .iter()
            .zip(&p)
            .map(|(a, b)| 0.93 * a + s * b / pn)
            .collect();
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        for x in &mut v {
            *x /= n;
        }
        rows[3].1 = v;
    }
    // 4/5 无关（随机）→ 不成组
    upsert(&c, "dinov3-fake", &rows);

    let cos01 = engine_search::cosine(&rows[0].1, &rows[1].1);
    let cos23 = engine_search::cosine(&rows[2].1, &rows[3].1);
    let cos45 = engine_search::cosine(&rows[4].1, &rows[5].1);
    assert!(cos01 >= NEAR_DUP_THRESHOLD, "构造自检 cos01={cos01}");
    assert!(
        cos23 >= 0.90 && cos23 < NEAR_DUP_THRESHOLD,
        "构造自检 cos23={cos23}"
    );
    assert!(cos45 < 0.90, "构造自检 cos45={cos45}");

    let groups = find_duplicates(
        &c,
        "dinov3-fake",
        &DupOptions {
            k_per_item: 5,
            min_score: 0.90,
            max_assets: 0,
        },
    )
    .unwrap();
    assert_eq!(
        groups.len(),
        2,
        "near 一组 + variant 一组，无关不成组: {groups:?}"
    );
    let near = groups.iter().find(|g| g.band == "near").expect("near band");
    let variant = groups
        .iter()
        .find(|g| g.band == "variant")
        .expect("variant band");
    assert_eq!(near.anchorResourceId, ids[0].1);
    assert_eq!(near.members.len(), 1);
    assert!((near.members[0].score - 1.0).abs() < 1e-3);
    assert_eq!(variant.members.len(), 1);
    assert!(variant.members[0].score >= 0.90 && variant.members[0].score < NEAR_DUP_THRESHOLD);

    // 阈值上调到 0.98 → variant 组消失
    let groups = find_duplicates(
        &c,
        "dinov3-fake",
        &DupOptions {
            k_per_item: 5,
            min_score: NEAR_DUP_THRESHOLD,
            max_assets: 0,
        },
    )
    .unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].band, "near");
}

// ---------- 验收 7：100K×512 性能（#[ignore]；release 跑） ----------

#[test]
#[ignore = "性能基准：cargo test --release -p engine-search -- --ignored perf"]
fn perf_100k_knn_under_1s() {
    let c = conn();
    let dims = 512usize;
    vecstore::ensure_table(&c, "perf-512", dims as u32).unwrap();
    let mut rng = Rng(99);
    // 分块插入（500/批，与 fetch_vectors 同粒度）
    let t_ins = std::time::Instant::now();
    for chunk_start in (0..100_000i64).step_by(500) {
        let rows: Vec<(i64, Vec<f32>)> = (chunk_start..chunk_start + 500)
            .map(|i| {
                let mut v = rng.unit(dims);
                v.shrink_to_fit();
                (i, v)
            })
            .collect();
        let borrowed: Vec<(i64, &[f32])> = rows.iter().map(|(r, v)| (*r, v.as_slice())).collect();
        vecstore::upsert(&c, "perf-512", &borrowed).unwrap();
    }
    let ins = t_ins.elapsed();
    assert_eq!(vecstore::count(&c, "perf-512").unwrap(), 100_000);

    // 30 次查询取最坏值（验收 <1s）
    let mut worst = std::time::Duration::ZERO;
    let mut worst_k10 = std::time::Duration::ZERO;
    for i in 0..30 {
        let q = rng.unit(dims);
        let t = std::time::Instant::now();
        let hits = vecstore::knn(&c, "perf-512", &q, 10).unwrap();
        let d = t.elapsed();
        assert_eq!(hits.len(), 10);
        if i % 3 == 0 {
            worst_k10 = worst_k10.max(d);
        }
        worst = worst.max(d);
    }
    println!("insert 100K x 512: {ins:?}");
    println!("knn k=10 worst: {worst_k10:?}; overall worst: {worst:?}");
    assert!(worst.as_millis() < 1000, "KNN 最坏 {worst:?} 超过 1s");

    // search_by_vector 端到端（含 join）也 <1s
    // 造一个资产行让 join 有数据（id 对齐 resource id 0..99 —— 无资产行时 join 为空，只测 knn 部分）
    let q = rng.unit(dims);
    let t = std::time::Instant::now();
    let _ = vecstore::knn(&c, "perf-512", &q, 50).unwrap();
    let d = t.elapsed();
    assert!(d.as_millis() < 1000, "k=50 knn {d:?} 超过 1s");
}

/// TAG-VALUE（0021）：自定义打标任务的输出以标签值（tag.value）落库，复合过滤
/// tag=标签值 必须命中（candidate_ids 的 tag 子查询不带 origin 限制——本测试把这一契约钉死）。
///
/// 注：本函数原先被误嵌在 perf_100k_knn_under_1s 函数体内（`#[test]` 对内部项
/// 不生效 → 从来没跑过），卡 task 修交付条数时顺手提出到顶层。
#[test]
fn tag_filter_matches_custom_origin() {
    let c = conn();
    let ids = seed_500(&c);
    let q = target_vec_of(&c, ids[0].1);
    c.execute(
        "INSERT INTO tag (assetId, value, origin, model, taskId, score, createdAt)
         VALUES (?1, '海边日落', 'custom', 'mock-vl', NULL, 1.0, 0)",
        [ids[0].0],
    )
    .unwrap();
    let hits = search_by_vector(
        &c,
        "model-a",
        DIMS as usize,
        &q,
        10,
        &SearchFilter {
            tag: Some("海边日落".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(hits.len(), 1, "origin='custom' 标签参与复合过滤");
    assert_eq!(hits[0].resourceId, ids[0].1);
}

// ---------- k = 交付条数：回收站 / 孤儿向量不得吃掉 top-k 名额 ----------
// BATCH-02（卡 task）缺陷复现：软删不动向量（恢复要用），无过滤路径直接全表
// KNN → top-k 被 assets.deleted=1 的向量占满 → 24 张库 k=24 只出 12、k=5 只出 2。

/// 与查询 e0 相似度恰为 `c` 的向量（e0/e1 平面内，分数可精确断言）。
fn vec_with_cos(c: f32) -> Vec<f32> {
    let mut v = vec![0.0f32; DIMS as usize];
    v[0] = c;
    v[1] = (1.0 - c * c).max(0.0).sqrt();
    v
}

fn query_e0() -> Vec<f32> {
    let mut q = vec![0.0f32; DIMS as usize];
    q[0] = 1.0;
    q
}

#[test]
fn unfiltered_delivers_k_with_trashed_vectors() {
    let c = conn();
    // 24 张：前 12 张（相似度最高 0.99→0.88）进回收站，后 12 张存活（0.80→0.58）
    let mut trashed: Vec<(i64, i64)> = Vec::new();
    let mut live: Vec<(i64, i64)> = Vec::new();
    let mut rows: Vec<(i64, Vec<f32>)> = Vec::new();
    for i in 0..24i64 {
        let (a, r) = add_asset(&c, &format!("t{i:02}"), "png", 100, 100, 1000, "/x");
        let cos = if i < 12 {
            0.99 - 0.01 * i as f32
        } else {
            0.80 - 0.02 * (i - 12) as f32
        };
        rows.push((r, vec_with_cos(cos)));
        if i < 12 {
            trashed.push((a, r));
        } else {
            live.push((a, r));
        }
    }
    upsert(&c, "model-a", &rows);
    for (a, _) in &trashed {
        c.execute("UPDATE assets SET deleted = 1 WHERE assetId = ?1", [a])
            .unwrap();
    }
    let q = query_e0();
    let qn = DIMS as usize;

    // 修复前：top-5 全是回收站向量（0.99~0.95）→ 过滤后 0 条交付（这行就是缺陷复现）
    let hits = search_by_vector(&c, "model-a", qn, &q, 5, &SearchFilter::default()).unwrap();
    assert_eq!(hits.len(), 5, "k=5 必须交付 5 条存活命中（修复前为 0）");
    for (i, h) in hits.iter().enumerate() {
        assert_eq!(h.resourceId, live[i].1, "按相似度降序交付存活集");
        let expect = 0.80 - 0.02 * i as f32;
        assert!(
            (h.score - expect).abs() < 1e-3,
            "第 {i} 条 score={} 期望 {expect}",
            h.score
        );
        assert_eq!(h.assetId, live[i].0);
    }

    // k=8 → 8 条；k=24（≥ 存活总量）→ 恰好 12 条（存活向量就这么多，回收站的绝不补位）
    let hits = search_by_vector(&c, "model-a", qn, &q, 8, &SearchFilter::default()).unwrap();
    assert_eq!(hits.len(), 8);
    let hits = search_by_vector(&c, "model-a", qn, &q, 24, &SearchFilter::default()).unwrap();
    assert_eq!(hits.len(), 12, "k 大于存活数时交付全部存活命中");
    assert!(
        hits.iter().all(|h| live.iter().any(|(_, r)| *r == h.resourceId)),
        "回收站资产不得出现在结果里"
    );

    // 回收站恢复 → 全部 24 条回到结果（向量一直在，恢复即可用）
    for (a, _) in &trashed {
        c.execute("UPDATE assets SET deleted = 0 WHERE assetId = ?1", [a])
            .unwrap();
    }
    let hits = search_by_vector(&c, "model-a", qn, &q, 24, &SearchFilter::default()).unwrap();
    assert_eq!(hits.len(), 24, "恢复后回收站资产重新参与召回");
    assert!((hits[0].score - 0.99).abs() < 1e-3, "恢复后最近的那张回到首位");
}

#[test]
fn unfiltered_delivers_k_with_orphan_vectors() {
    // 孤儿向量：重导入替换 resource（svc-import 先删旧 resource 行再插新的）后留在表里，
    // 只靠「回收站存在」判不出来，必须由「表行数 > 存活 primary 数」兜住。
    let c = conn();
    let mut rows: Vec<(i64, Vec<f32>)> = Vec::new();
    let mut ids: Vec<(i64, i64)> = Vec::new();
    for i in 0..6i64 {
        let (a, r) = add_asset(&c, &format!("o{i}"), "png", 100, 100, 1000, "/x");
        // 前 3 张是最近的（0.99/0.97/0.95），后 3 张 0.80/0.75/0.70
        let cos = [0.99, 0.97, 0.95, 0.80, 0.75, 0.70][i as usize];
        rows.push((r, vec_with_cos(cos)));
        ids.push((a, r));
    }
    upsert(&c, "model-a", &rows);
    // 前 3 个 resource 行被替换掉 → 其向量成为孤儿（assets 行仍在，但无 primary resource）
    for (_, r) in &ids[0..3] {
        c.execute("DELETE FROM resource WHERE resourceId = ?1", [r])
            .unwrap();
    }

    let hits = search_by_vector(
        &c,
        "model-a",
        DIMS as usize,
        &query_e0(),
        3,
        &SearchFilter::default(),
    )
    .unwrap();
    assert_eq!(hits.len(), 3, "孤儿向量不得占名额（修复前 top-3 全是孤儿 → 0 条）");
    for (i, h) in hits.iter().enumerate() {
        assert_eq!(h.resourceId, ids[3 + i].1, "只剩存活且仍有 primary 的 3 张");
    }
}

#[test]
fn unfiltered_large_live_set_falls_back_to_exact_scan() {
    // 存活候选 > EXACT_SCAN_MAX → 走「KNN 过采样 + 候选过滤」；若过采样窗口被回收站向量
    // 占满而没有足够存活命中，必须回退精确扫描（正确性优先），k 仍按交付条数。
    let c = conn();
    let total = 8_300i64; // 8_100 存活 + 200 回收站，刚好越过 EXACT_SCAN_MAX
    let mut rng = Rng(2026);
    let mut rows: Vec<(i64, Vec<f32>)> = Vec::new();
    let mut trashed: Vec<i64> = Vec::new();
    let mut live: Vec<(i64, i64)> = Vec::new();
    for i in 0..total {
        let (a, r) = add_asset(&c, &format!("b{i:05}"), "png", 100, 100, 1000, "/b");
        if i < 200 {
            // 回收站：相似度 0.99 起（占住 top 窗口）
            rows.push((r, vec_with_cos(0.99 - 0.0001 * i as f32)));
            trashed.push(a);
        } else {
            rows.push((r, rng.unit(DIMS as usize)));
            live.push((a, r));
        }
    }
    upsert(&c, "model-a", &rows);
    for a in &trashed {
        c.execute("UPDATE assets SET deleted = 1 WHERE assetId = ?1", [a])
            .unwrap();
    }

    let hits = search_by_vector(
        &c,
        "model-a",
        DIMS as usize,
        &query_e0(),
        10,
        &SearchFilter::default(),
    )
    .unwrap();
    assert_eq!(hits.len(), 10, "过采样窗口全被回收站占满时必须精确兜底");
    assert!(
        hits.iter().all(|h| live.iter().any(|(_, r)| *r == h.resourceId)),
        "结果里不得出现回收站资产"
    );
    // 降序自检
    for w in hits.windows(2) {
        assert!(w[0].score >= w[1].score, "必须按 score 降序");
    }
}
