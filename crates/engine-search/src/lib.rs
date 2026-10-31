//! L1 `engine-search` — 检索编排：向量召回 + SQLite 标量过滤 + 合并排序。
//! 不含 UI 语义；embModel 标识随查询路由，禁跨模型混算（PLAN.md §9）。
//!
//! 查询入口是**向量**（`search_by_vector`）；把文本/图片变成向量是 provider 的事
//! （engine-embed，同层禁互引），由 L2（svc-library）编排。
//!
//! 过滤策略（spike 实测，reports/D5-spike-decision.md）：
//! - 无过滤 → vec0 KNN 直查（C 扫描，100K×512 实测 41ms）
//! - 有过滤 → 先 SQL 取候选 resourceId 集：
//!   - 候选 ≤ `EXACT_SCAN_MAX`：取回向量 Rust 精确打分（结果数学上精确）
//!   - 候选 > `EXACT_SCAN_MAX`：KNN 过采样（k×10 上限 4096）后按候选集过滤；
//!     不足 k 条回退精确扫描（正确性优先，慢路径兜底）
//!   （不可用 JOIN：spike 实测 vec0 的 JOIN 是 lossy post-filter，20 中只回 6）
//!
//! **k 的语义 = 交付条数**（不是召回预算，见 [`search_by_vector`]）：召回预算由本模块
//! 内部（过采样倍数 / 精确兜底）决定，调用方传 k 就拿到 k 条。
//!
//! 软删（回收站）**不动向量**（恢复要用），因此向量表里会长期混着 `assets.deleted=1`
//! 的向量、以及重导入替换 resource 后留下的孤儿向量。无过滤路径若只做一次全表 KNN，
//! 交付不足 k 条时这些非存活行就占了名额（BATCH-02 / 卡 task：24 张库 k=24 只出
//! 12、k=5 只出 2）。故无过滤路径是「乐观快路径 + 一次收口重试」：先原样 KNN + join，
//! 交付满 k 就直接返回（干净库零额外开销）；不足 k 才按存活集重来一遍（候选 = 全库存活
//! primary 资源，与有过滤路径同一套策略）。

use core_storage::vecstore;
use rusqlite::Connection;

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("embedding model mismatch: query={query}, index={index}")]
    ModelMismatch { query: String, index: String },
    #[error("no vector store for model {0} (先跑一次嵌入建索引)")]
    NoStore(String),
    #[error("dim mismatch: expect {expect}, got {got}")]
    Dim { expect: usize, got: usize },
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("vector store: {0}")]
    Storage(String),
}

pub type Result<T> = std::result::Result<T, SearchError>;

/// 过滤候选集超过该值时走「KNN 过采样 + 过滤」而非精确扫描。
pub const EXACT_SCAN_MAX: usize = 8_000;
/// KNN 过采样的结果集上限。
pub const OVERSAMPLE_CAP: usize = 4_096;

/// 标量过滤（复合过滤与 SQLite 端 AND 组合；None 字段不过滤）。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SearchFilter {
    pub tag: Option<String>,
    /// 目录前缀（assets.absPath LIKE prefix%）
    pub dir_prefix: Option<String>,
    /// 扩展名（小写，如 "jpg"）
    pub ext: Option<String>,
    /// 最小分辨率（resource 宽高都 ≥）
    pub min_width: Option<i64>,
    pub min_height: Option<i64>,
    /// 导入时间窗（unix 秒，闭区间）
    pub date_from: Option<i64>,
    pub date_to: Option<i64>,
}

impl SearchFilter {
    pub fn is_empty(&self) -> bool {
        self.tag.is_none()
            && self.dir_prefix.is_none()
            && self.ext.is_none()
            && self.min_width.is_none()
            && self.min_height.is_none()
            && self.date_from.is_none()
            && self.date_to.is_none()
    }
}

/// 命中行（向量分 + 标量行合并后的最终返回单元）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub assetId: i64,
    pub resourceId: i64,
    /// 余弦相似度（= 1 - vec distance），降序
    pub score: f32,
    pub fileName: String,
    pub titleCh: Option<String>,
    pub thumbPath: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// OCR 字面命中（OCR-MATCH）：该命中含 `assets.ocr_text` 的字面证据（可能是
    /// 语义命中被字面分数抬高，也可能是语义未召回、仅字面命中的补入项）。
    /// 纯语义命中恒为 false —— 前端据此显示「文字命中」标记，不猜。
    pub ocrMatched: bool,
    /// 字面命中所在的 OCR 原文片段（匹配行，窗口截断；None = 非字面命中或无文本）。
    pub ocrSnippet: Option<String>,
}

/// 去重组成员。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DupMember {
    pub assetId: i64,
    pub resourceId: i64,
    /// 与组锚点的余弦相似度
    pub score: f32,
    /// 展示字段（D7 查重页组内审阅用；加字段不改语义）
    pub fileName: String,
    pub absPath: String,
    pub thumbPath: Option<String>,
}

/// 去重组（并查集聚类；锚点 = 组内最小 resourceId）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DupGroup {
    pub anchorAssetId: i64,
    pub anchorResourceId: i64,
    /// 锚点展示字段（D7）
    pub anchorFileName: String,
    pub anchorAbsPath: String,
    pub anchorThumbPath: Option<String>,
    pub members: Vec<DupMember>,
    /// near（≥0.98 近重复）/ variant（[0.90,0.98) 疑似变体）——组内最高相似度定带
    pub band: String,
}

/// find_duplicates 阈值分带（2026-10-13 裁决）。
pub const NEAR_DUP_THRESHOLD: f32 = 0.98;
pub const VARIANT_THRESHOLD: f32 = 0.90;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DupOptions {
    /// 每项 KNN 邻居数（含自身；默认 9）
    pub k_per_item: usize,
    /// 成组最低相似度（默认 0.90；< 该值的邻居不成组）
    pub min_score: f32,
    /// 最多扫描前 N 个资源（0 = 全库）
    pub max_assets: usize,
}

impl Default for DupOptions {
    fn default() -> Self {
        Self {
            k_per_item: 9,
            min_score: VARIANT_THRESHOLD,
            max_assets: 0,
        }
    }
}

/// 余弦相似度（向量已归一化时=点积；这里做通用实现）。
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// 模型是否已有向量表（无表 = 尚未嵌入，上层给引导文案）。
pub fn has_store(conn: &Connection, model: &str) -> bool {
    vecstore::tables(conn)
        .map(|ts| ts.iter().any(|t| t.0 == model))
        .unwrap_or(false)
}

/// 过滤条件 → 候选 SQL（WHERE 段 + 绑定参数；`candidate_ids` 执行，
/// 单测拿同一段文本跑 EXPLAIN QUERY PLAN 钉索引，防 SQL 漂移后悄悄退回全表扫）。
fn candidate_sql(f: &SearchFilter) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let mut where_clauses = vec!["a.deleted = 0".to_string()];
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    // task：入口归一化（trim/空白折叠）+ COLLATE NOCASE（走 0024 的 nocase 索引）。
    // 全值精确匹配的语义不变，只是不再被大小写/首尾空格卡死。
    if let Some(tag) = &f.tag {
        let norm = core_types::normalize_tag_query(tag);
        if !norm.is_empty() {
            where_clauses.push(format!(
                "a.assetId IN (SELECT assetId FROM tag WHERE value = ?{} COLLATE NOCASE)",
                params.len() + 1
            ));
            params.push(Box::new(norm));
        }
    }
    if let Some(dir) = &f.dir_prefix {
        where_clauses.push(format!("a.absPath LIKE ?{}", params.len() + 1));
        params.push(Box::new(format!("{}%", dir.trim_end_matches('/'))));
    }
    if let Some(ext) = &f.ext {
        where_clauses.push(format!("a.fileExt = ?{}", params.len() + 1));
        params.push(Box::new(ext.to_ascii_lowercase()));
    }
    if let Some(w) = f.min_width {
        where_clauses.push(format!("r.width >= ?{}", params.len() + 1));
        params.push(Box::new(w));
    }
    if let Some(h) = f.min_height {
        where_clauses.push(format!("r.height >= ?{}", params.len() + 1));
        params.push(Box::new(h));
    }
    if let Some(d) = f.date_from {
        where_clauses.push(format!("a.importedAt >= ?{}", params.len() + 1));
        params.push(Box::new(d));
    }
    if let Some(d) = f.date_to {
        where_clauses.push(format!("a.importedAt <= ?{}", params.len() + 1));
        params.push(Box::new(d));
    }
    let sql = format!(
        "SELECT r.resourceId
         FROM resource r JOIN assets a ON a.assetId = r.assetId
         WHERE r.role = 'primary' AND {}
         ORDER BY r.resourceId",
        where_clauses.join(" AND ")
    );
    (sql, params)
}

/// 过滤条件 → 候选 resourceId 集（resource 粒度；deleted 恒排除）。
fn candidate_ids(conn: &Connection, f: &SearchFilter) -> Result<Vec<i64>> {
    let (sql, params) = candidate_sql(f);
    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |r| r.get::<_, i64>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn map_storage_err(e: core_storage::StorageError) -> SearchError {
    SearchError::Storage(e.to_string())
}

/// 向量检索主入口：qvec 已按 provider 出口归一；k 夹取 [1, 500]。
///
/// **k = 交付条数**（不是召回预算）：返回 `min(k, 可用存活向量数)` 条，按相似度降序。
/// 召回预算（KNN 过采样倍数、精确兜底）是本模块内部实现细节，调用方不需要也不该
/// 为了「过滤掉一些」而放大 k。上层若要按相关性下限再截断（`min_score`），那是**另一个**
/// 语义（「没有足够相似的图」），与条数无关。
pub fn search_by_vector(
    conn: &Connection,
    model: &str,
    dims: usize,
    qvec: &[f32],
    k: usize,
    filter: &SearchFilter,
) -> Result<Vec<SearchHit>> {
    if qvec.len() != dims {
        return Err(SearchError::Dim {
            expect: dims,
            got: qvec.len(),
        });
    }
    if !has_store(conn, model) {
        return Err(SearchError::NoStore(model.to_string()));
    }
    let k = k.clamp(1, 500);

    // ①+② 乐观召回 + 标量行合并（deleted 排除 + 字段回填）。
    //   有过滤：候选集收口后召回（唯一一轮）。无过滤：先原样全表 KNN —— 干净库上零额外开销。
    let hits = if !filter.is_empty() {
        let cands = candidate_ids(conn, filter)?;
        join_hits(conn, recall_among(conn, model, qvec, &cands, k)?)?
    } else {
        let ranked: Vec<(i64, f32)> = vecstore::knn(conn, model, qvec, k)
            .map_err(map_storage_err)?
            .into_iter()
            .map(|(rid, dist)| (rid, 1.0 - dist))
            .collect();
        join_hits(conn, ranked)?
    };
    if !filter.is_empty() || hits.len() >= k {
        return Ok(hits);
    }

    // ③ 无过滤但交付不足 k：向量表里混着回收站/孤儿向量（软删不动向量），它们占掉了
    //    top-k 名额 → 按存活集收口重来一次（候选 = 全库存活 primary，与有过滤同一套策略）。
    //   一次重试即收敛：收口后交付数只取决于存活向量总量，非存活行不再参与。
    let cands = candidate_ids(conn, &SearchFilter::default())?;
    join_hits(conn, recall_among(conn, model, qvec, &cands, k)?)
}

/// 候选集内召回 top-k（k = 交付条数，结果按 score 降序、至多 k 条）。
/// - 候选 ≤ `EXACT_SCAN_MAX`：精确扫描（数学上精确）
/// - 候选 > `EXACT_SCAN_MAX`：KNN 过采样（k×10 上限 `OVERSAMPLE_CAP`）+ 候选过滤，
///   不足 k 条回退精确扫描兜底
fn recall_among(
    conn: &Connection,
    model: &str,
    qvec: &[f32],
    cands: &[i64],
    k: usize,
) -> Result<Vec<(i64, f32)>> {
    if cands.is_empty() {
        return Ok(vec![]);
    }
    if cands.len() <= EXACT_SCAN_MAX {
        return exact_scan(conn, model, qvec, cands, k);
    }
    let cand_set: std::collections::HashSet<i64> = cands.iter().copied().collect();
    let oversample = (k.saturating_mul(10)).min(OVERSAMPLE_CAP).max(k);
    let hits: Vec<(i64, f32)> = vecstore::knn(conn, model, qvec, oversample)
        .map_err(map_storage_err)?
        .into_iter()
        .filter(|(rid, _)| cand_set.contains(rid))
        .map(|(rid, dist)| (rid, 1.0 - dist))
        .take(k)
        .collect();
    if hits.len() >= k {
        return Ok(hits);
    }
    exact_scan(conn, model, qvec, cands, k)
}

/// 精确路径：取回候选向量逐个打分取 top-k（数学上精确，无过采样损失）。
fn exact_scan(
    conn: &Connection,
    model: &str,
    qvec: &[f32],
    cands: &[i64],
    k: usize,
) -> Result<Vec<(i64, f32)>> {
    let mut scored: Vec<(i64, f32)> = vecstore::fetch_vectors(conn, model, cands)
        .map_err(map_storage_err)?
        .into_iter()
        .map(|(rid, v)| (rid, cosine(qvec, &v)))
        .filter(|(_, s)| s.is_finite())
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);
    Ok(scored)
}

/// (resourceId, score) → 资产行（保持 score 降序）。
fn join_hits(conn: &Connection, ranked: Vec<(i64, f32)>) -> Result<Vec<SearchHit>> {
    let mut out = Vec::with_capacity(ranked.len());
    for chunk in ranked.chunks(500) {
        let ph = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT r.resourceId, a.assetId, a.fileName, a.title_ch, r.thumbPath, r.width, r.height
             FROM resource r JOIN assets a ON a.assetId = r.assetId AND a.deleted = 0
             WHERE r.role = 'primary' AND r.resourceId IN ({ph})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter().map(|h| h.0)), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, Option<i64>>(6)?,
            ))
        })?;
        let rows: Vec<_> = rows.collect::<rusqlite::Result<_>>()?;
        for (rid, asset_id, file_name, title_ch, thumb, w, h) in rows {
            let score = chunk
                .iter()
                .find(|(rr, _)| *rr == rid)
                .map(|(_, s)| *s)
                .unwrap_or(0.0);
            out.push(SearchHit {
                assetId: asset_id,
                resourceId: rid,
                score,
                fileName: file_name,
                titleCh: title_ch,
                thumbPath: thumb,
                width: w,
                height: h,
                // 纯向量召回路径：字面证据由 svc-library::fuse_ocr_text 事后标注
                ocrMatched: false,
                ocrSnippet: None,
            });
        }
    }
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
}

/// 以图找重（DINOv3 塔）：逐资源 KNN → 阈值分带 → 并查集聚组。
/// 复杂度 O(n) 次 KNN；桌面库规模（≤10 万）后台任务可承受，超大库后续 M5 再优化。
pub fn find_duplicates(conn: &Connection, model: &str, opts: &DupOptions) -> Result<Vec<DupGroup>> {
    if !has_store(conn, model) {
        return Err(SearchError::NoStore(model.to_string()));
    }
    let mut rids: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT r.resourceId FROM resource r
             JOIN assets a ON a.assetId = r.assetId AND a.deleted = 0
             WHERE r.role = 'primary' ORDER BY r.resourceId",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if opts.max_assets > 0 {
        rids.truncate(opts.max_assets);
    }
    if rids.is_empty() {
        return Ok(vec![]);
    }

    // 边收集：只记 rid < neighbor 的无向边一次
    let mut edges: Vec<(i64, i64, f32)> = Vec::new();
    let rid_set: std::collections::HashSet<i64> = rids.iter().copied().collect();
    for rid in &rids {
        let qv = match vecstore::fetch_vectors(conn, model, &[*rid])
            .map_err(map_storage_err)?
            .into_iter()
            .next()
        {
            Some((_, v)) => v,
            None => continue, // 该资源无向量（未嵌入），跳过
        };
        let hits =
            vecstore::knn(conn, model, &qv, opts.k_per_item.max(2)).map_err(map_storage_err)?;
        for (nb, dist) in hits {
            let score = 1.0 - dist;
            if nb == *rid || !rid_set.contains(&nb) || score < opts.min_score {
                continue;
            }
            let (a, b) = if *rid < nb { (*rid, nb) } else { (nb, *rid) };
            edges.push((a, b, score));
        }
    }

    // 并查集
    let mut uf: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    fn find(uf: &mut std::collections::HashMap<i64, i64>, mut x: i64) -> i64 {
        let mut root = x;
        while let Some(&p) = uf.get(&root) {
            if p == root {
                break;
            }
            root = p;
        }
        while let Some(&p) = uf.get(&x) {
            if p == root {
                break;
            }
            let next = p;
            uf.insert(x, root);
            x = next;
        }
        root
    }
    for rid in &rids {
        uf.insert(*rid, *rid);
    }
    for (a, b, _) in &edges {
        let (ra, rb) = (find(&mut uf, *a), find(&mut uf, *b));
        if ra != rb {
            uf.insert(ra, rb);
        }
    }

    // 组装：root → 成员
    let mut groups: std::collections::HashMap<i64, Vec<i64>> = std::collections::HashMap::new();
    for rid in &rids {
        let root = find(&mut uf, *rid);
        groups.entry(root).or_default().push(*rid);
    }

    /// resourceId → (assetId, fileName, absPath, thumbPath)（D7 展示字段）。
    let info_of = |conn: &Connection, rid: i64| -> Result<(i64, String, String, Option<String>)> {
        Ok(conn.query_row(
            "SELECT r.assetId, a.fileName, a.absPath, r.thumbPath
             FROM resource r JOIN assets a ON a.assetId = r.assetId
             WHERE r.resourceId = ?1",
            [rid],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?)
    };

    let mut out = Vec::new();
    for (_, members) in groups {
        if members.len() < 2 {
            continue;
        }
        let mut members = members;
        members.sort_unstable();
        let anchor = members[0];
        let with_vecs = vecstore::fetch_vectors(conn, model, &members).map_err(map_storage_err)?;
        let anchor_vec = with_vecs
            .iter()
            .find(|(rid, _)| *rid == anchor)
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let mut dup_members = Vec::new();
        let mut best = 0f32;
        for (rid, v) in &with_vecs {
            if *rid == anchor {
                continue;
            }
            let score = cosine(&anchor_vec, v);
            best = best.max(score);
            let (aid, file_name, abs_path, thumb) = info_of(conn, *rid)?;
            dup_members.push(DupMember {
                assetId: aid,
                resourceId: *rid,
                score,
                fileName: file_name,
                absPath: abs_path,
                thumbPath: thumb,
            });
        }
        if dup_members.is_empty() {
            continue;
        }
        dup_members.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let (anchor_aid, anchor_name, anchor_path, anchor_thumb) = info_of(conn, anchor)?;
        let band = if best >= NEAR_DUP_THRESHOLD {
            "near"
        } else {
            "variant"
        };
        out.push(DupGroup {
            anchorAssetId: anchor_aid,
            anchorResourceId: anchor,
            anchorFileName: anchor_name,
            anchorAbsPath: anchor_path,
            anchorThumbPath: anchor_thumb,
            members: dup_members,
            band: band.to_string(),
        });
    }
    out.sort_by(|a, b| b.anchorResourceId.cmp(&a.anchorResourceId));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_basics() {
        let a = [1.0f32, 0.0];
        let b = [0.0f32, 1.0];
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-6);
        assert!(cosine(&a, &b).abs() < 1e-6);
        assert!((cosine(&a, &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
        assert_eq!(cosine(&[], &[]), 0.0);
        assert_eq!(cosine(&[1.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn filter_empty_semantics() {
        let f = SearchFilter::default();
        assert!(f.is_empty());
        assert!(!SearchFilter {
            ext: Some("jpg".into()),
            ..Default::default()
        }
        .is_empty());
    }

    #[test]
    fn dup_options_defaults_match_ruling_thresholds() {
        let o = DupOptions::default();
        assert!((o.min_score - VARIANT_THRESHOLD).abs() < 1e-6);
        assert!(NEAR_DUP_THRESHOLD > VARIANT_THRESHOLD);
    }

    // ---- task：过滤候选集语义 + 索引钉死 ----

    /// 3 张资产：1/2 带 `尺寸-滚筒_10KG`，2 另带 `Sunset`，3 不带标签。
    fn candidate_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = core_storage::open(&dir.path().join("dam.db")).unwrap();
        for id in 1..=3i64 {
            conn.execute(
                "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt, deleted)
                 VALUES (?1, ?2, 0, ?3, 'jpg', 1, 0, ?1, 0)",
                rusqlite::params![id, format!("/lib/d{id}.jpg"), format!("d{id}.jpg")],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1,'primary',0,1,1)",
                [id],
            )
            .unwrap();
        }
        for (asset, value) in [(1i64, "尺寸-滚筒_10KG"), (2, "尺寸-滚筒_10KG"), (2, "Sunset")] {
            conn.execute(
                "INSERT INTO tag (assetId, value, origin, score, createdAt) VALUES (?1, ?2, 'manual', 1.0, 1)",
                rusqlite::params![asset, value],
            )
            .unwrap();
        }
        (dir, conn)
    }

    #[test]
    fn tag_candidate_filter_is_exact_normalized_nocase() {
        let (_d, conn) = candidate_db();
        let ids = |tag: Option<&str>| -> Vec<i64> {
            let mut v = candidate_ids(&conn, &SearchFilter { tag: tag.map(String::from), ..Default::default() }).unwrap();
            v.sort_unstable();
            v
        };
        // 全值精确命中
        assert_eq!(ids(Some("尺寸-滚筒_10KG")), vec![1, 2]);
        // 大小写 / 首尾空格 / 内部空白（→_）不影响命中（B07-2④）
        assert_eq!(ids(Some("尺寸-滚筒_10kg")), vec![1, 2]);
        assert_eq!(ids(Some("  尺寸-滚筒_10KG  ")), vec![1, 2]);
        assert_eq!(ids(Some("sunset")), vec![2]);
        // 自然词不是前缀匹配：过滤路径语义仍是全值精确（前缀只在候选提示里）
        assert!(ids(Some("尺寸")).is_empty());
        assert!(ids(Some("滚筒")).is_empty());
        // 归一化后为空 = 不过滤
        assert_eq!(ids(Some("   ")).len(), 3);
        assert_eq!(ids(None).len(), 3);
    }

    #[test]
    fn dir_prefix_candidate_filter() {
        let (_d, conn) = candidate_db();
        let f = SearchFilter {
            dir_prefix: Some("/lib".into()),
            ..Default::default()
        };
        assert_eq!(candidate_ids(&conn, &f).unwrap().len(), 3);
        let f = SearchFilter {
            dir_prefix: Some("/lib/d1.jpg".into()),
            ..Default::default()
        };
        assert_eq!(candidate_ids(&conn, &f).unwrap(), vec![1]);
    }

    #[test]
    fn explain_query_plan_tag_filter_uses_nocase_index() {
        let (_d, conn) = candidate_db();
        let f = SearchFilter {
            tag: Some("尺寸-滚筒_10KG".into()),
            ..Default::default()
        };
        let (sql, params) = candidate_sql(&f);
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let plans: Vec<String> = stmt
            .query_map(param_refs.as_slice(), |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let joined = plans.join("\n");
        assert!(
            joined.contains("idx_tag_value_nocase"),
            "标签过滤必须走 0024 nocase 索引，实际计划:\n{joined}"
        );
        assert!(
            !joined.to_lowercase().contains("scan table tag") && !joined.to_lowercase().contains("scan tag"),
            "tag 子查询不允许全表扫，实际计划:\n{joined}"
        );
    }
}
