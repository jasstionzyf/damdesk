//! L2 `svc-library` — 面向 UI 的读模型（列表/过滤/统计/标签面板/向量检索，纯读）。

use core_types::{
    AssetView, ImportFilter, LibraryStats, MediaTypeCount, ResourceRecord, TagRecord,
    TagResultRecord,
};
#[cfg(feature = "embed")]
use engine_embed::EmbedProvider;
#[cfg(feature = "embed")]
use engine_search::{DupGroup, DupOptions, SearchFilter, SearchHit};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

pub mod capabilities;
pub mod compliance_view;
pub mod export;

#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// C6：core-storage repository 错误（判定/反馈读写走 trait，错误类型统一上抛）
    /// OCR-MATCH：core-storage 的索引/DAO 错误（分段索引表缺失等）
    #[error("storage: {0}")]
    Storage(#[from] core_storage::StorageError),
    #[error("vector store: {0}")]
    Search(#[from] engine_search::SearchError),
    #[cfg(feature = "embed")]
    #[error("embed: {0}")]
    Embed(#[from] engine_embed::EmbedError),
    #[error("provider {0} 不支持文本查询（image-only）")]
    TextUnsupported(String),
    #[error("image read failed: {0}")]
    ImageIo(String),
    /// E2：入参非法（打分越界等），HTTP 层映射 400
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("job queue: {0}")]
    Jobq(#[from] engine_jobq::JobqError),
}

pub type Result<T> = std::result::Result<T, LibraryError>;

/// D3「无法预览」的**唯一判定口径**：没有 primary resource，或它的宽/高不是正数
/// （导入时解码失败就写成 0；见 `svc_import::refresh_resource`）。
///
/// 用 `NOT EXISTS(...)` 而不是 `r.width IS NULL`：`list_assets` 有 LEFT JOIN resource，
/// 但 `export::count_assets` 的 SQL 只有 `FROM assets a` —— 三处（列表/总数/统计）
/// 必须能贴同一段条件，否则「列表 3 条、总数 5」这类假象立刻出现。
/// 静态常量（不带参数）→ 不占 SQLite 参数位。允许 `width > 0` 而不判 height：
/// 两侧写库恒同时写（见 refresh_resource），判一个就够。
pub const BROKEN_COND: &str = "NOT EXISTS (SELECT 1 FROM resource rb \
     WHERE rb.assetId = a.assetId AND rb.role = 'primary' AND rb.width > 0 AND rb.height > 0)";

/// `BROKEN_COND` 的正向补集（?broken=0 用）。两段必须成对改，别只改一边。
pub const PREVIEWABLE_COND: &str = "EXISTS (SELECT 1 FROM resource rb \
     WHERE rb.assetId = a.assetId AND rb.role = 'primary' AND rb.width > 0 AND rb.height > 0)";

/// 分隔符无关的父目录：POSIX `/a/b`、Windows `D:\a\b`、UNC `\\srv\share\a`。
/// 取不到分隔符（相对名）返回 None；根下的 `/a` 返回空串由调用方兜底。
pub fn parent_dir_any_sep(abs: &str) -> Option<&str> {
    abs.rfind(['/', '\\']).map(|i| &abs[..i])
}

/// 去尾分隔符（导入根 `D:\pics\` → `D:\pics`），卷根保留成 `/`、`D:`。
pub fn trim_dir_sep(s: &str) -> &str {
    let t = s.trim_end_matches(['/', '\\']);
    if t.is_empty() {
        s.get(..1).unwrap_or("/")
    } else {
        t
    }
}

/// 绝对目录判定（跨平台）：POSIX `/…`、Windows `D:\…`/`D:/…`、UNC `\\srv\…`。
pub fn is_abs_dir(p: &str) -> bool {
    p.starts_with('/')
        || p.starts_with('\\')
        || (p.len() >= 3
            && p.as_bytes()[1] == b':'
            && matches!(p.as_bytes()[2], b'\\' | b'/'))
}

#[cfg(test)]
mod path_helper_tests {
    use super::{is_abs_dir, parent_dir_any_sep, trim_dir_sep};

    #[test]
    fn parent_dir_is_separator_agnostic() {
        assert_eq!(parent_dir_any_sep("/a/b/c.jpg"), Some("/a/b"));
        assert_eq!(parent_dir_any_sep(r"D:\pics\2024\a.jpg"), Some(r"D:\pics\2024"));
        assert_eq!(parent_dir_any_sep(r"\\srv\share\a.jpg"), Some(r"\\srv\share"));
        assert_eq!(parent_dir_any_sep("no-sep.jpg"), None);
        // 根下文件：父目录是空串，调用方负责归到根
        assert_eq!(parent_dir_any_sep(r"D:\a.jpg"), Some("D:"));
        assert_eq!(parent_dir_any_sep("/a.jpg"), Some(""));
    }

    #[test]
    fn trim_dir_sep_keeps_volume_root() {
        assert_eq!(trim_dir_sep(r"D:\pics\"), r"D:\pics");
        assert_eq!(trim_dir_sep(r"D:\pics"), r"D:\pics");
        assert_eq!(trim_dir_sep("/pics/"), "/pics");
        assert_eq!(trim_dir_sep(r"D:\"), "D:");
        assert_eq!(trim_dir_sep("/"), "/");
    }

    #[test]
    fn abs_dir_accepts_windows_and_posix() {
        assert!(is_abs_dir("/pics"));
        assert!(is_abs_dir(r"D:\pics"));
        assert!(is_abs_dir("D:/pics"));
        assert!(is_abs_dir(r"\\srv\share\pics"));
        assert!(!is_abs_dir("pics/rel"));
        assert!(!is_abs_dir("D:pics"));
        assert!(!is_abs_dir(""));
    }
}

/// 关键词 → `LIKE '%…%'` 模式串，转义 `\` `%` `_`（配合 SQL 的 `ESCAPE '\'`）。
///
/// 不转义的话用户搜 `100%` / `a_b` 会变成通配符，命中一堆无关图 —— 「搜出来的结果不对」
/// 正是这类静默放大。`\` 自身必须最先转义，否则会被当成转义前缀吃掉后一个字符。
fn like_contains_pattern(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('%');
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// 复合过滤 WHERE（list_assets / export::count_assets / export::resolve_items 共用，
/// D5 过滤结果管道直通 D11 导出的落点）。
fn filter_where(f: &ImportFilter) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let mut where_clauses = vec!["a.deleted = 0".to_string()];
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(mt) = &f.mediaType {
        where_clauses.push(format!("a.mediaType = ?{}", params.len() + 1));
        params.push(Box::new(mt.clone()));
    }
    if let Some(m) = f.missing {
        where_clauses.push(format!("a.missing = ?{}", params.len() + 1));
        params.push(Box::new(m));
    }
    // D3 无法预览：Some(1)=只看解码失败；Some(0)=只看可预览（口径见 BROKEN_COND）
    if let Some(b) = f.broken {
        where_clauses.push(if b == 1 {
            BROKEN_COND.to_string()
        } else {
            PREVIEWABLE_COND.to_string()
        });
    }
    if let Some(ext) = &f.ext {
        where_clauses.push(format!("a.fileExt = ?{}", params.len() + 1));
        params.push(Box::new(ext.to_ascii_lowercase()));
    }
    // t_4f145794：入口归一化（trim/空白折叠）+ COLLATE NOCASE（走 0024 的 nocase 索引）。
    // 归一化后为空 = 不过滤（与前端「空串不发送」同一口径）。
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
    if let Some(dir) = &f.dirPrefix {
        where_clauses.push(format!("a.absPath LIKE ?{}", params.len() + 1));
        params.push(Box::new(format!("{}%", dir.trim_end_matches('/'))));
    }
    if f.untagged == Some(true) {
        where_clauses.push("a.assetId NOT IN (SELECT DISTINCT assetId FROM tag)".to_string());
    }
    if let Some(r) = f.minRating {
        where_clauses.push(format!("a.starRating >= ?{}", params.len() + 1));
        params.push(Box::new(r));
    }
    if let Some(c) = &f.colorLabel {
        where_clauses.push(format!("a.colorLabel = ?{}", params.len() + 1));
        params.push(Box::new(c.clone()));
    }
    // C7 关键词全库检索（服务端）：文件名 / 标题中英 / 描述 / 路径 的包含匹配。
    // 为什么是 LIKE 而不是客户端 filterByQuery：客户端只能筛「已加载的这一页」（60 张），
    // 全库命中的图只要不在首页就永远搜不到（用户实测的「结果不全」）。这里必须与
    // list_assets / count_assets 同一 WHERE，否则 total 与实际行数会打架。
    if let Some(q) = &f.query {
        let needle = q.trim();
        if !needle.is_empty() {
            let ph = params.len() + 1;
            where_clauses.push(format!(
                "(a.fileName LIKE ?{ph} ESCAPE '\\' OR a.title_ch LIKE ?{ph} ESCAPE '\\' \
                 OR a.title_en LIKE ?{ph} ESCAPE '\\' OR a.description LIKE ?{ph} ESCAPE '\\' \
                 OR a.absPath LIKE ?{ph} ESCAPE '\\')"
            ));
            params.push(Box::new(like_contains_pattern(needle)));
        }
    }
    if let Some(ids) = &f.ids {
        if ids.is_empty() {
            // 显式空集合 = 空结果（而不是退化为全库）
            where_clauses.push("0".to_string());
        } else {
            let placeholders = vec!["?"; ids.len()].join(",");
            where_clauses.push(format!("a.assetId IN ({placeholders})"));
            params.extend(
                ids.iter()
                    .map(|id| Box::new(*id) as Box<dyn rusqlite::ToSql>),
            );
        }
    }
    // ---- EXIF2 维度（走 0013 派生列，禁运行时表达式扫全表） ----
    if let Some(t) = f.takenFrom {
        where_clauses.push(format!("a.takenAt >= ?{}", params.len() + 1));
        params.push(Box::new(t));
    }
    if let Some(t) = f.takenTo {
        where_clauses.push(format!("a.takenAt <= ?{}", params.len() + 1));
        params.push(Box::new(t));
    }
    if let Some(m) = &f.cameraModel {
        where_clauses.push(format!("a.cameraModel = ?{}", params.len() + 1));
        params.push(Box::new(m.clone()));
    }
    if let Some(m) = &f.lensModel {
        where_clauses.push(format!("a.lensModel = ?{}", params.len() + 1));
        params.push(Box::new(m.clone()));
    }
    if let Some(v) = f.isoMin {
        where_clauses.push(format!("a.iso >= ?{}", params.len() + 1));
        params.push(Box::new(v));
    }
    if let Some(v) = f.isoMax {
        where_clauses.push(format!("a.iso <= ?{}", params.len() + 1));
        params.push(Box::new(v));
    }
    if let Some(a) = &f.aspect {
        // 非法值 = 明确 400（上层校验），这里只放行三个合法词
        where_clauses.push(format!("a.aspectClass = ?{}", params.len() + 1));
        params.push(Box::new(a.clone()));
    }
    // ---- C6 合规状态维度（口径见 svc_library::compliance_view；SQL 与 Rust 口径有对账测试）----
    if let Some(cf) = &f.compliance {
        let (cond, vals) = crate::compliance_view::state_condition(cf, params.len() + 1);
        where_clauses.push(cond);
        for v in vals {
            params.push(Box::new(v));
        }
    }
    (where_clauses.join(" AND "), params)
}

/// 排序键（EXIF2）：缺省 importedAt DESC；"takenAt" = 拍摄时间 DESC、NULL 殿后。
fn order_by(f: &ImportFilter) -> &'static str {
    if f.sort.as_deref() == Some("takenAt") {
        "(a.takenAt IS NULL), a.takenAt DESC, a.assetId DESC"
    } else {
        "a.importedAt DESC, a.assetId DESC"
    }
}

/// 资产列表（asset LEFT JOIN primary resource 扁平化；deleted 恒排除）。
/// 过滤维度：mediaType / missing / ext / tag / dirPrefix + EXIF2（拍摄时间/相机/
/// 镜头/ISO/画幅）；limit 默认 200。
pub fn list_assets(conn: &Connection, f: &ImportFilter) -> Result<Vec<AssetView>> {
    let (wh, params) = filter_where(f);

    let limit = f.limit.unwrap_or(200).clamp(1, 1000);
    let offset = f.offset.unwrap_or(0).max(0);
    let sql = format!(
        "SELECT a.assetId, a.mediaType, a.absPath, a.storeMode, a.missing, a.fileName, a.fileExt,
                a.fileSz, a.mtime, a.fileHash, a.title_ch, a.title_en, a.description, a.source,
                a.parentAssetId, a.searchable, a.deleted, a.approvalStatus, a.starRating,
                a.colorLabel, a.cloudAssetId, a.importedAt,
                COALESCE(a.emb_status, 'none'), a.emb_model,
                COALESCE(a.dedup_status, 'none'), a.dedup_model,
                r.thumbPath, r.width, r.height, r.pHash,
                a.takenAt, a.cameraMake, a.cameraModel, a.lensModel, a.iso,
                a.focalLenMm, a.aperture, a.shutterText, a.orientation,
                a.aspectRatio, a.aspectClass, a.gps_lat, a.gps_lon, a.gpsSource, a.exif
         FROM assets a
         LEFT JOIN resource r ON r.assetId = a.assetId AND r.role = 'primary'
         WHERE {wh}
         ORDER BY {order}
         LIMIT {limit} OFFSET {offset}",
        order = order_by(f)
    );

    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = stmt.query_map(param_refs.as_slice(), map_asset_view)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn map_asset_view(r: &rusqlite::Row<'_>) -> rusqlite::Result<AssetView> {
    Ok(AssetView {
        asset: core_types::AssetRecord {
            assetId: r.get(0)?,
            mediaType: r.get(1)?,
            absPath: r.get(2)?,
            storeMode: r.get(3)?,
            missing: r.get(4)?,
            fileName: r.get(5)?,
            fileExt: r.get(6)?,
            fileSz: r.get(7)?,
            mtime: r.get(8)?,
            fileHash: r.get(9)?,
            titleCh: r.get(10)?,
            titleEn: r.get(11)?,
            description: r.get(12)?,
            source: r.get(13)?,
            parentAssetId: r.get(14)?,
            searchable: r.get(15)?,
            deleted: r.get(16)?,
            approvalStatus: r.get(17)?,
            starRating: r.get(18)?,
            colorLabel: r.get(19)?,
            cloudAssetId: r.get(20)?,
            importedAt: r.get(21)?,
            embStatus: r.get(22)?,
            embModel: r.get(23)?,
            dedupStatus: r.get(24)?,
            dedupModel: r.get(25)?,
            // EXIF2 派生列（真值在 exif json）
            takenAt: r.get(30)?,
            cameraMake: r.get(31)?,
            cameraModel: r.get(32)?,
            lensModel: r.get(33)?,
            iso: r.get(34)?,
            focalLenMm: r.get(35)?,
            aperture: r.get(36)?,
            shutterText: r.get(37)?,
            orientation: r.get(38)?,
            aspectRatio: r.get(39)?,
            aspectClass: r.get(40)?,
            gpsLat: r.get(41)?,
            gpsLon: r.get(42)?,
            gpsSource: r.get(43)?,
            exif: r.get(44)?,
        },
        thumbPath: r.get(26)?,
        width: r.get(27)?,
        height: r.get(28)?,
        pHash: r.get(29)?,
    })
}

/// 资产的 primary resource 完整记录（详情面板）。
pub fn primary_resource(conn: &Connection, asset_id: i64) -> Result<Option<ResourceRecord>> {
    let r = conn
        .query_row(
            "SELECT resourceId, assetId, role, ordinal, thumbPath, width, height, pHash
             FROM resource WHERE assetId=?1 AND role='primary'",
            rusqlite::params![asset_id],
            |r| {
                Ok(ResourceRecord {
                    resourceId: r.get(0)?,
                    assetId: r.get(1)?,
                    role: r.get(2)?,
                    ordinal: r.get(3)?,
                    thumbPath: r.get(4)?,
                    width: r.get(5)?,
                    height: r.get(6)?,
                    pHash: r.get(7)?,
                })
            },
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(r)
}

/// EXIF2：相机型号聚合（筛选下拉数据源，含量计数；仅图片、未删）。
pub fn camera_models(conn: &Connection, limit: u32) -> Result<Vec<(String, i64)>> {
    let limit = limit.clamp(1, 100);
    let mut stmt = conn.prepare(
        "SELECT cameraModel, COUNT(*) AS n
         FROM assets
         WHERE deleted = 0 AND mediaType = 'image' AND cameraModel IS NOT NULL
         GROUP BY cameraModel
         ORDER BY n DESC, cameraModel
         LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 资产标签（详情面板；manual 排前）。
pub fn asset_tags(conn: &Connection, asset_id: i64) -> Result<Vec<TagRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, assetId, value, origin, model, taskId, score, createdAt
         FROM tag WHERE assetId=?1
         ORDER BY CASE origin WHEN 'manual' THEN 0 ELSE 1 END, score DESC, value",
    )?;
    let rows = stmt.query_map(rusqlite::params![asset_id], |r| {
        Ok(TagRecord {
            id: r.get(0)?,
            assetId: r.get(1)?,
            value: r.get(2)?,
            origin: r.get(3)?,
            model: r.get(4)?,
            taskId: r.get(5)?,
            score: r.get(6)?,
            createdAt: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 单资产的打标任务结果（维度名: 值）——与标签值分开读，UI 分两块展示。
pub fn asset_tag_results(conn: &Connection, asset_id: i64) -> Result<Vec<TagResultRecord>> {
    Ok(core_storage::tags::asset_tag_results(conn, asset_id)?)
}

/// 标签面板：全库标签频次（侧栏标签云）。
pub fn tag_cloud(conn: &Connection, limit: i64) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT t.value, COUNT(DISTINCT t.assetId) AS n
         FROM tag t JOIN assets a ON a.assetId = t.assetId AND a.deleted = 0
         GROUP BY t.value ORDER BY n DESC, t.value LIMIT ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![limit], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 标签候选（t_4f145794 搜索页输入建议；t_d4895738 扩展 key/offset）：前缀/组匹配 + 频次，
/// 返回 (行, 命中总数)。
///
/// - `prefix`/`key` 都走同一套入口归一化（`normalize_tag_query`）+ ASCII 小写折叠
///   （NOCASE 语义下大小写等价，统一小写让 Rust 侧范围求交与 SQLite 排序一致）；
///   空/None 的条件不参与过滤；两个都缺 = 全量标签云语义（此时也可仅带 offset 翻页）；
/// - **范围扫描，不用 LIKE**（t_d4895738）：LIKE 前缀在默认 case_sensitive_like=OFF 下
///   实测退化为覆盖索引全扫描（7.5M 行 220.8ms）；显式 `value >= p AND value < p‖U+10FFFF`
///   走 `SEARCH t USING INDEX idx_tag_value_nocase (value>? AND value<?)`，同一查询 28.1ms。
///   `U+10FFFF` 上界保证「p 及其一切后延」都落在半开区间内；连接顺序用 CROSS JOIN 钉死
///   （普通 JOIN 在无 ANALYZE 统计的库上会被反着驱动，见下）；
/// - `key` 语义 = `value ∈ ['key-', 'key-'‖U+10FFFF)`；与 `prefix` 组合时对两个范围**求交**
///   （取更紧的下界/上界，Rust 侧短路空集，不下 SQL）；
/// - 大小写不敏感保持（ASCII，经 `COLLATE NOCASE` 吃 0024 索引）；用户输入里的
///   `%`/`_` 是字面量（无 LIKE 即无通配符问题）；
/// - `total` 是未截断的命中值数，`hasMore` 由上层按 `total > rows.len()` 推；
/// - 排序 `(count DESC, value ASC)` 稳定，`offset` 为兼容分页（同一排序下的位移）；
/// - limit 夹取 [1, 200]，offset 夹取 >= 0。
pub fn tag_candidates(
    conn: &Connection,
    prefix: Option<&str>,
    key: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<(String, i64)>, i64)> {
    let limit = limit.clamp(1, 200);
    let offset = offset.max(0);
    let norm = |s: &str| core_types::normalize_tag_query(s).to_ascii_lowercase();
    let key_norm = key.map(|k| norm(k)).filter(|s| !s.is_empty());
    let prefix_norm = prefix.map(|p| norm(p)).filter(|s| !s.is_empty());
    // 每个条件 → 半开区间 [lo, hi)；key 追加 `-` 后成前缀区间。求交后单对边界一次下发。
    let mut ranges: Vec<(String, String)> = Vec::new();
    if let Some(k) = &key_norm {
        ranges.push((format!("{k}-"), format!("{k}-\u{10FFFF}")));
    }
    if let Some(p) = &prefix_norm {
        ranges.push((p.clone(), format!("{p}\u{10FFFF}")));
    }
    let bounds = ranges
        .into_iter()
        .reduce(|(al, ah), (bl, bh)| (al.max(bl), ah.min(bh)));
    match bounds {
        Some((l, h)) if l < h => {
            // CROSS JOIN = 显式钉连接顺序（tag 先于 assets）。普通 JOIN 在无 ANALYZE 统计的库上
            // 会被优化器反着驱动（SEARCH a USING idx_assets_deleted → 逐资产走 uq_tag_asset_value），
            // 等于全量活资产扫描；CROSS JOIN 后两种统计状态都是
            // `SEARCH t USING INDEX idx_tag_value_nocase (value>? AND value<?)`（t_d4895738 实测）。
            let total: i64 = conn.query_row(
                "SELECT COUNT(DISTINCT t.value)
                 FROM tag t CROSS JOIN assets a ON a.assetId = t.assetId AND a.deleted = 0
                 WHERE t.value >= ?1 COLLATE NOCASE AND t.value < ?2 COLLATE NOCASE",
                rusqlite::params![l, h],
                |r| r.get(0),
            )?;
            let mut stmt = conn.prepare(
                "SELECT t.value, COUNT(DISTINCT t.assetId) AS n
                 FROM tag t CROSS JOIN assets a ON a.assetId = t.assetId AND a.deleted = 0
                 WHERE t.value >= ?1 COLLATE NOCASE AND t.value < ?2 COLLATE NOCASE
                 GROUP BY t.value ORDER BY n DESC, t.value LIMIT ?3 OFFSET ?4",
            )?;
            let rows = stmt.query_map(rusqlite::params![l, h, limit, offset], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
            Ok((rows.collect::<rusqlite::Result<Vec<_>>>()?, total))
        }
        // 范围不相交（如 key=尺寸 × prefix=滚）→ 空集短路，不下 SQL
        Some(_) => Ok((Vec::new(), 0)),
        None => {
            // 无 prefix/key：全量候选（与 tag_cloud 同序，支持 offset 翻页）
            let total: i64 = conn.query_row(
                "SELECT COUNT(DISTINCT t.value)
                 FROM tag t JOIN assets a ON a.assetId = t.assetId AND a.deleted = 0",
                [],
                |r| r.get(0),
            )?;
            let mut stmt = conn.prepare(
                "SELECT t.value, COUNT(DISTINCT t.assetId) AS n
                 FROM tag t JOIN assets a ON a.assetId = t.assetId AND a.deleted = 0
                 GROUP BY t.value ORDER BY n DESC, t.value LIMIT ?1 OFFSET ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![limit, offset], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
            Ok((rows.collect::<rusqlite::Result<Vec<_>>>()?, total))
        }
    }
}

// ==================== 标签总览（/tags 页面）与批量整理（t_cafb2e45） ====================
//
// 数据源仍然是唯一的 `tag(assetId, value)` 明细表 + `uq_tag_asset_value`；**不加 key 列**：
// key = value 里第一个 `-` 前的段（与前端 `splitTagKeyValue` 同口径，首字符 `-` 视作裸值）。
//
// 实测口径（scripts/bench_tag_scale.py，7.5M 行 / 10^4 值 / 100 万资产）：
//   distinct 值字典（index-only `GROUP BY value`）        0.92s  ← 左栏唯一全库读
//   key 范围 + 每值活资产计数（一页 50）                  0.06s
//   key × 资产数聚合（COUNT(DISTINCT assetId) + join）    3.26s  ← 页面上不能用
//   DISTINCT value + assets 连接                         12.5s  ← 优化器被反驱动，禁用
// 结论：左栏给「key + 值数」，不给「key × 资产数」——后者要么 3.26s 要么需要维护字典表
// （plan §5.5 的 tag_dict，写路径 8 处以上要同步计数，本卡不做）。右栏选中 key 后由
// `tag_page` 给出该组的每值计数 + 合计，口径一致（都 join `deleted=0`）。

/// 左栏一行：key（'' = 无前缀裸值组）+ 该组下的**值数**。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TagKeyGroup {
    pub key: String,
    pub values: i64,
}

/// 值 → key（'' = 裸值）。首字符是 `-` 也归裸值（与前端 splitTagKeyValue 一致）。
pub fn tag_key_of(value: &str) -> &str {
    match value.find('-') {
        Some(i) if i > 0 => &value[..i],
        _ => "",
    }
}

/// 只在回收站资产上出现的值，最多逐个核对多少个；超过就放弃修正（口径退化为「含回收站」）。
/// 正常库里这个集合很小（回收站是少数），逐个核对是一次索引探测。
const DEAD_VALUE_CHECK_CAP: usize = 2000;

/// 字典扫描：distinct 值全集（走 `idx_tag_value_nocase` 的有序索引，index-only）。
fn distinct_tag_values(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT value FROM tag INDEXED BY idx_tag_value_nocase GROUP BY value")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 只出现在 `deleted=1` 资产上的值集合。
///
/// 为什么需要：字典扫描只看 tag 表（快），右栏值列表 join `assets.deleted=0`（口径）。
/// 两者差集就是「回收站里独有的值」——左栏若把它算进值数，就会出现「左显示 3 值、右栏 2 行」。
/// 代价 = distinct(回收站资产上的值) 次索引探测；回收站规模异常时（> CAP）放弃修正。
fn dead_only_values(conn: &Connection) -> Result<std::collections::HashSet<String>> {
    let mut out = std::collections::HashSet::new();
    let mut stmt = conn.prepare(
        "SELECT DISTINCT t.value FROM tag t
         WHERE t.assetId IN (SELECT assetId FROM assets WHERE deleted = 1)",
    )?;
    let cands: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if cands.len() > DEAD_VALUE_CHECK_CAP {
        return Ok(out);
    }
    let mut alive = conn.prepare(
        "SELECT 1 FROM tag t CROSS JOIN assets a ON a.assetId = t.assetId AND a.deleted = 0
         WHERE t.value = ?1 LIMIT 1",
    )?;
    for v in cands {
        let live = alive
            .query_row([&v], |_| Ok(()))
            .map(|_| true)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(false),
                other => Err(other),
            })?;
        if !live {
            out.insert(v);
        }
    }
    Ok(out)
}

/// 无前缀（裸值）谓词：值里没有 `-`，或 `-` 在首位。前端 `splitTagKeyValue` 的同口径 SQL 版。
const KEYLESS_COND: &str = "(instr(t.value, '-') = 0 OR instr(t.value, '-') = 1)";

/// 左栏 key 分组（`GET /api/library/tag-groups`）：每 key 的值数 + 值总数。
///
/// 排序：值数降序、key 升序（裸值组 key='' 也参与排序，不特殊对待）。
pub fn tag_key_groups(conn: &Connection) -> Result<(Vec<TagKeyGroup>, i64)> {
    let t0 = std::time::Instant::now();
    let dead = dead_only_values(conn)?;
    let t1 = std::time::Instant::now();
    let values = distinct_tag_values(conn)?;
    let t2 = std::time::Instant::now();
    if std::env::var("DAM_TAG_TIMING").is_ok() {
        eprintln!(
            "dam-web: tag_key_groups dead={}ms dict={}ms n={}",
            t1.duration_since(t0).as_millis(),
            t2.duration_since(t1).as_millis(),
            values.len()
        );
    }
    let mut map: std::collections::HashMap<&str, i64> = std::collections::HashMap::new();
    let mut total = 0i64;
    for v in &values {
        if dead.contains(v) {
            continue;
        }
        total += 1;
        *map.entry(tag_key_of(v)).or_insert(0) += 1;
    }
    let mut items: Vec<TagKeyGroup> = map
        .into_iter()
        .map(|(k, n)| TagKeyGroup {
            key: k.to_string(),
            values: n,
        })
        .collect();
    items.sort_by(|a, b| b.values.cmp(&a.values).then_with(|| a.key.cmp(&b.key)));
    Ok((items, total))
}

/// `/tags` 右栏一页的查询（`GET /api/library/tags` 的 key/prefix/keyless + keyset 分支）。
pub struct TagPageQuery<'a> {
    /// 组过滤（`value ∈ ['key-', 'key-'‖U+10FFFF)`）
    pub key: Option<&'a str>,
    /// 搜索框前缀
    pub prefix: Option<&'a str>,
    /// 无前缀（裸值）组
    pub keyless: bool,
    pub limit: i64,
    /// 兼容分页（与 keyset 二选一；keyset 优先）
    pub offset: i64,
    /// keyset 游标 = 上一页最后一行的 (count, value)，排序 `(count DESC, value ASC)` 下无缝续页
    pub after: Option<(i64, String)>,
}

/// `/tags` 右栏一页的结果。
#[derive(Debug, Clone)]
pub struct TagPage {
    /// `(value, 在库资产计数)`，按 `(count DESC, value ASC)`（keyless 分支按 `value ASC`）
    pub rows: Vec<(String, i64)>,
    /// 命中值总数（不随游标/页码变）
    pub total: i64,
    /// 本页之后是否还有行 —— 由「多取一行」判定，不用 `total > 已消费` 推算
    ///（keyset 游标下「已消费」不可知，推算会给出假的 hasMore）
    pub has_more: bool,
}

impl TagPage {
    /// 多取一行判 hasMore，然后截断回 limit（唯一实现点，两个分支共用）。
    fn from_rows(mut rows: Vec<(String, i64)>, total: i64, limit: i64) -> TagPage {
        let has_more = rows.len() as i64 > limit;
        if has_more {
            rows.truncate(limit as usize);
        }
        TagPage {
            rows,
            total,
            has_more,
        }
    }
}

/// `/tags` 右栏一页：`(value, 在库资产计数)` + 命中值总数。
///
/// - keyed / prefix：范围扫描 + `GROUP BY value`（走 0024 的 NOCASE 索引），keyset 用
///   `HAVING count < ?c OR (count = ?c AND value > ?v)`，排序 `(count DESC, value ASC)` 唯一；
/// - keyless：裸值散落在整个有序索引里，无法用区间表达；这里按 `value ASC` 走同一张索引
///   **提前终止**（`LIMIT` 到 50 条就不再往前扫），每值计数仍是索引内连接，实测 7.5M 行 80ms；
/// - 三个条件都不给 → `Invalid`：全量按 count 排序要 3.26s，别从这条路进（候选走 tag_candidates）。
pub fn tag_page(conn: &Connection, q: &TagPageQuery<'_>) -> Result<TagPage> {
    let limit = q.limit.clamp(1, 200);
    // 多取一行用于 hasMore 判定
    let take = limit + 1;
    let offset = q.offset.max(0);
    let norm = |s: &str| core_types::normalize_tag_query(s).to_ascii_lowercase();
    let key_norm = q.key.map(norm).filter(|s| !s.is_empty());
    let prefix_norm = q.prefix.map(norm).filter(|s| !s.is_empty());

    if q.keyless {
        // 值总数：字典扫描（与左栏同一来源，含回收站修正）
        let dead = dead_only_values(conn)?;
        let mut total = 0i64;
        for v in distinct_tag_values(conn)? {
            if dead.contains(&v) || !tag_key_of(&v).is_empty() {
                continue;
            }
            total += 1;
        }
        let mut sql = String::from(
            "SELECT t.value, COUNT(DISTINCT t.assetId) AS n
             FROM tag t CROSS JOIN assets a ON a.assetId = t.assetId AND a.deleted = 0
             WHERE ",
        );
        sql.push_str(KEYLESS_COND);
        let mut params: Vec<rusqlite::types::Value> = Vec::new();
        if let Some((_, after)) = &q.after {
            sql.push_str(" AND t.value > ?");
            params.push(rusqlite::types::Value::Text(after.clone()));
        }
        sql.push_str(" GROUP BY t.value ORDER BY t.value ASC LIMIT ? OFFSET ?");
        params.push(rusqlite::types::Value::Integer(take));
        params.push(rusqlite::types::Value::Integer(offset));
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        return Ok(TagPage::from_rows(rows, total, limit));
    }

    // keyed / prefix：与 tag_candidates 同一套半开区间（两条件求交）
    let mut ranges: Vec<(String, String)> = Vec::new();
    if let Some(k) = &key_norm {
        ranges.push((format!("{k}-"), format!("{k}-\u{10FFFF}")));
    }
    if let Some(p) = &prefix_norm {
        ranges.push((p.clone(), format!("{p}\u{10FFFF}")));
    }
    let bounds = ranges
        .into_iter()
        .reduce(|(al, ah), (bl, bh)| (al.max(bl), ah.min(bh)));
    let (lo, hi) = match bounds {
        Some((l, h)) if l < h => (l, h),
        // 范围不相交（key=尺寸 × prefix=滚）→ 空集短路
        Some(_) => return Ok(TagPage::from_rows(Vec::new(), 0, limit)),
        None => {
            return Err(LibraryError::Invalid(
                "需要 key / prefix / keyless 之一（全量按计数排序代价过高）".into(),
            ))
        }
    };
    let total: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT t.value)
         FROM tag t CROSS JOIN assets a ON a.assetId = t.assetId AND a.deleted = 0
         WHERE t.value >= ?1 COLLATE NOCASE AND t.value < ?2 COLLATE NOCASE",
        rusqlite::params![lo, hi],
        |r| r.get(0),
    )?;
    let mut sql = String::from(
        "SELECT t.value, COUNT(DISTINCT t.assetId) AS n
         FROM tag t CROSS JOIN assets a ON a.assetId = t.assetId AND a.deleted = 0
         WHERE t.value >= ?1 COLLATE NOCASE AND t.value < ?2 COLLATE NOCASE
         GROUP BY t.value",
    );
    let mut params: Vec<rusqlite::types::Value> = vec![
        rusqlite::types::Value::Text(lo),
        rusqlite::types::Value::Text(hi),
    ];
    if let Some((c, v)) = &q.after {
        sql.push_str(
            " HAVING (COUNT(DISTINCT t.assetId) < ?3)
                OR (COUNT(DISTINCT t.assetId) = ?3 AND t.value > ?4)",
        );
        params.push(rusqlite::types::Value::Integer(*c));
        params.push(rusqlite::types::Value::Text(v.clone()));
    }
    let lim_at = params.len() + 1;
    let off_at = params.len() + 2;
    sql.push_str(&format!(
        " ORDER BY n DESC, t.value LIMIT ?{lim_at} OFFSET ?{off_at}"
    ));
    params.push(rusqlite::types::Value::Integer(take));
    params.push(rusqlite::types::Value::Integer(offset));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(TagPage::from_rows(rows, total, limit))
}

/// 每值样本图（`GET /api/library/tag-samples`）：值 → 最多 `limit` 个在库 assetId。
/// 索引内连接（`idx_tag_value` + assets PK），50 个值实测 46ms。
pub fn tag_samples(
    conn: &Connection,
    values: &[String],
    limit: i64,
) -> Result<Vec<(String, Vec<i64>)>> {
    let limit = limit.clamp(1, 6);
    let mut stmt = conn.prepare(
        "SELECT t.assetId FROM tag t CROSS JOIN assets a ON a.assetId = t.assetId AND a.deleted = 0
         WHERE t.value = ?1 LIMIT ?2",
    )?;
    let mut out = Vec::new();
    for v in values.iter().take(200) {
        let ids = stmt
            .query_map(rusqlite::params![v, limit], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        out.push((v.clone(), ids));
    }
    Ok(out)
}

/// 单次合并/改名的资产上限（超出要求分批；plan §4-C 安全边界）。
pub const TAG_MERGE_MAX_ASSETS: i64 = 5000;

/// 合并/改名回执。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TagMergeOutcome {
    /// 受影响资产数（去重、仅 deleted=0）
    pub affected: i64,
    /// 被合并掉的值（原来有行、合并后消失）
    pub removed_values: Vec<String>,
    /// 保留值
    pub kept_value: String,
}

/// 合并计划（预览与执行共用）：目标值、去重后的源值、受影响资产数、将消失的源值。
///
/// 抽出来是为了让**确认框里的数字**与真正执行时用的数字来自同一段代码 —— 预览另写一份
/// 计数逻辑，迟早会出现「确认框说 3 张、执行回执说 4 张」。
fn merge_plan(
    conn: &Connection,
    sources: &[String],
    to: &str,
) -> Result<(String, Vec<String>, i64, Vec<String>)> {
    let kept = core_storage::tags::normalize_tag_value(to)
        .ok_or_else(|| LibraryError::Invalid("目标标签值不能为空".into()))?;
    let mut srcs: Vec<String> = Vec::new();
    for s in sources {
        let t = core_storage::tags::normalize_tag_value(s)
            .ok_or_else(|| LibraryError::Invalid("源标签值不能为空".into()))?;
        if t != kept && !srcs.contains(&t) {
            srcs.push(t);
        }
    }
    if srcs.is_empty() {
        return Err(LibraryError::Invalid("至少选择一个与目标值不同的标签".into()));
    }
    // 受影响资产数（去重、仅在库）——确认框与上限都用这个数
    let affected = {
        let ph = vec!["?"; srcs.len()].join(",");
        let sql = format!(
            "SELECT COUNT(DISTINCT t.assetId) FROM tag t
             CROSS JOIN assets a ON a.assetId = t.assetId AND a.deleted = 0
             WHERE t.value IN ({ph})"
        );
        conn.query_row(
            &sql,
            rusqlite::params_from_iter(srcs.iter()),
            |r| r.get::<_, i64>(0),
        )?
    };
    if affected > TAG_MERGE_MAX_ASSETS {
        return Err(LibraryError::Invalid(format!(
            "本次将影响 {affected} 张资产，超过单次上限 {TAG_MERGE_MAX_ASSETS}，请分批合并"
        )));
    }
    let mut removed = Vec::new();
    for s in &srcs {
        let rows: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tag WHERE value = ?1",
            rusqlite::params![s],
            |r| r.get(0),
        )?;
        if rows > 0 {
            removed.push(s.clone());
        }
    }
    Ok((kept, srcs, affected, removed))
}

/// 合并预览（`POST /api/library/tags/merge-preview`）：只算不写，给确认框用。
pub fn tag_merge_preview(
    conn: &Connection,
    sources: &[String],
    to: &str,
) -> Result<TagMergeOutcome> {
    let (kept, _srcs, affected, removed) = merge_plan(conn, sources, to)?;
    Ok(TagMergeOutcome {
        affected,
        removed_values: removed,
        kept_value: kept,
    })
}

/// 把 `sources` 里所有标签行合到 `to`（`rename` = 单源合并到新值）。
///
/// 语义（plan §4-C / 卡 t_cafb2e45）：
/// - **逐值精确匹配**（BINARY，不做大小写折叠）：列表里的值是原样回传的，折叠只会制造意外命中；
/// - `uq(assetId, value)` 冲突（同一张图已经有目标值）→ `INSERT OR IGNORE` 去重，原行删除；
/// - 单事务：中途失败整批回滚；审计（`tag_merge_log`）与数据变更同事务提交；
/// - **不动 `tag_result`**（打标任务结果留档，合并只改标签实体）；
/// - 上限 `TAG_MERGE_MAX_ASSETS` 张（超限返回 Invalid，前端提示分批）；
/// - **不提供撤销**（裁决 2026-10-20）。
pub fn tag_merge(
    conn: &Connection,
    sources: &[String],
    to: &str,
) -> Result<TagMergeOutcome> {
    let (kept, srcs, affected, _planned_removed) = merge_plan(conn, sources, to)?;
    let tx = conn.unchecked_transaction()?;
    let now: i64 = tx.query_row("SELECT unixepoch()", [], |r| r.get(0))?;
    let mut removed = Vec::new();
    for s in &srcs {
        // 审计口径：本源值在合并前有多少在库资产（不是合并后目标值的总数）
        let src_affected: i64 = tx.query_row(
            "SELECT COUNT(DISTINCT t.assetId) FROM tag t
             CROSS JOIN assets a ON a.assetId = t.assetId AND a.deleted = 0
             WHERE t.value = ?1",
            rusqlite::params![s],
            |r| r.get(0),
        )?;
        // 目标值已有的同图行让 `INSERT OR IGNORE` 吃掉（uq_tag_asset_value 兜底）
        tx.execute(
            "INSERT OR IGNORE INTO tag (assetId, value, origin, model, taskId, score, createdAt)
             SELECT assetId, ?1, origin, model, taskId, score, createdAt FROM tag WHERE value = ?2",
            rusqlite::params![kept, s],
        )?;
        let del = tx.execute("DELETE FROM tag WHERE value = ?1", rusqlite::params![s])?;
        if del > 0 {
            removed.push(s.clone());
        }
        // 逐源值一行审计（一动作多源时便于按值追溯）
        tx.execute(
            "INSERT INTO tag_merge_log (ts, kind, fromValue, toValue, affected, removed, operator)
             VALUES (?1, 'merge', ?2, ?3, ?4, ?5, 'local')",
            rusqlite::params![
                now,
                s,
                kept,
                src_affected,
                serde_json::to_string(&vec![s.clone()]).unwrap_or_else(|_| "[]".into())
            ],
        )?;
    }
    tx.commit()?;
    Ok(TagMergeOutcome {
        affected,
        removed_values: removed,
        kept_value: kept,
    })
}

/// 删除标签值回执。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TagDeleteOutcome {
    /// 真正删掉的值（原来有行）
    pub deleted: Vec<String>,
    /// 删掉的行数（只在回收站资产上的行）
    pub removed_rows: usize,
}

/// 删除标签值：**只允许在库资产计数为 0 的值**（否则先合并）。
///
/// 现实里能删掉的是「只剩回收站资产在用」的值——右侧值列表（join `deleted=0`）本来就看不到
/// 它们，但左栏值数/历史链接可能还留着，清理入口由此承担。有在库资产 → Invalid + 可读原因。
pub fn tag_delete(conn: &Connection, values: &[String]) -> Result<TagDeleteOutcome> {
    let mut vals: Vec<String> = Vec::new();
    for v in values {
        let t = core_storage::tags::normalize_tag_value(v)
            .ok_or_else(|| LibraryError::Invalid("标签值不能为空".into()))?;
        if !vals.contains(&t) {
            vals.push(t);
        }
    }
    if vals.is_empty() {
        return Err(LibraryError::Invalid("至少选择一个标签".into()));
    }
    let mut blocked: Vec<String> = Vec::new();
    for v in &vals {
        let live: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT t.assetId) FROM tag t
             CROSS JOIN assets a ON a.assetId = t.assetId AND a.deleted = 0
             WHERE t.value = ?1",
            rusqlite::params![v],
            |r| r.get(0),
        )?;
        if live > 0 {
            blocked.push(format!("「{v}」还有 {live} 张资产"));
        }
    }
    if !blocked.is_empty() {
        return Err(LibraryError::Invalid(format!(
            "只能删除没有资产的标签：{}。请先把它们合并到其它值。",
            blocked.join("；")
        )));
    }
    let tx = conn.unchecked_transaction()?;
    let now: i64 = tx.query_row("SELECT unixepoch()", [], |r| r.get(0))?;
    let mut deleted = Vec::new();
    let mut removed_rows = 0usize;
    for v in &vals {
        let n = tx.execute("DELETE FROM tag WHERE value = ?1", rusqlite::params![v])?;
        if n > 0 {
            deleted.push(v.clone());
            removed_rows += n;
            tx.execute(
                "INSERT INTO tag_merge_log (ts, kind, fromValue, toValue, affected, removed, operator)
                 VALUES (?1, 'delete', ?2, '', 0, ?3, 'local')",
                rusqlite::params![
                    now,
                    v,
                    serde_json::to_string(&vec![v.clone()]).unwrap_or_else(|_| "[]".into())
                ],
            )?;
        }
    }
    tx.commit()?;
    Ok(TagDeleteOutcome {
        deleted,
        removed_rows,
    })
}

/// 库统计（状态栏）：总数/失联/已打标/总字节/mediaType 分布。
///
/// DESKTOP-MISSING-STAT-1：`missing` 只反映**上次现场核查**的结果（watcher 事件 + 显式
/// 核查共同维护），因此必须同时给出 `missingCheckedAt`/`missingStale` —— 单看 `missing`
/// 无法区分"确实没有失联"和"从没核查过"。核查动作在 svc-import（L2 同层不互引，
/// 记录经 core-types 的 MISSING_CHECK_KEY/记录类型走 settings 表传递）。
pub fn stats(conn: &Connection) -> Result<LibraryStats> {
    let assets = conn.query_row("SELECT COUNT(*) FROM assets WHERE deleted=0", [], |r| {
        r.get(0)
    })?;
    let missing = conn.query_row(
        "SELECT COUNT(*) FROM assets WHERE deleted=0 AND missing=1",
        [],
        |r| r.get(0),
    )?;
    let tagged = conn.query_row(
        "SELECT COUNT(DISTINCT assetId) FROM tag WHERE assetId IN (SELECT assetId FROM assets WHERE deleted=0)",
        [],
        |r| r.get(0),
    )?;
    let total_bytes = conn.query_row(
        "SELECT IFNULL(SUM(fileSz),0) FROM assets WHERE deleted=0",
        [],
        |r| r.get(0),
    )?;
    // D3 无法预览：与 /api/library/assets?broken=1 同一段条件（单一口径，不另算一套）
    let broken = conn.query_row(
        &format!("SELECT COUNT(*) FROM assets a WHERE a.deleted=0 AND {}", BROKEN_COND),
        [],
        |r| r.get(0),
    )?;
    let mut stmt =
        conn.prepare("SELECT mediaType, COUNT(*) FROM assets WHERE deleted=0 GROUP BY mediaType ORDER BY COUNT(*) DESC")?;
    let by = stmt
        .query_map([], |r| {
            Ok(MediaTypeCount {
                mediaType: r.get(0)?,
                count: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // 核查新鲜度：记录缺失（老库/从未核查）或解析失败 → 视为过期，不阻断统计本身
    let check: Option<core_types::MissingCheckRecord> = core_storage::settings::get_json(
        conn,
        core_types::MISSING_CHECK_KEY,
    )
    .ok()
    .flatten();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let stale = match &check {
        Some(c) => now - c.at > core_types::MISSING_CHECK_STALE_SECS,
        None => true,
    };
    Ok(LibraryStats {
        assets,
        missing,
        tagged,
        totalBytes: total_bytes,
        byMediaType: by,
        missingCheckedAt: check.as_ref().map(|c| c.at),
        missingStale: stale,
        missingUnreadable: check.as_ref().map(|c| c.unreadable).unwrap_or(0),
        broken,
        missingStaleSecs: core_types::MISSING_CHECK_STALE_SECS,
    })
}

/// 导入批次列表（导入向导显示历史/断点）。
pub fn import_batches(conn: &Connection) -> Result<Vec<core_types::ImportBatchRecord>> {
    let mut stmt = conn.prepare(
        "SELECT batchId, rootPath, storeMode, status, totalFiles, doneFiles, error, startedAt, endedAt
         FROM import_batches ORDER BY batchId DESC LIMIT 50",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(core_types::ImportBatchRecord {
            batchId: r.get(0)?,
            rootPath: r.get(1)?,
            storeMode: r.get(2)?,
            status: r.get(3)?,
            totalFiles: r.get(4)?,
            doneFiles: r.get(5)?,
            error: r.get(6)?,
            startedAt: r.get(7)?,
            endedAt: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        // 用真实迁移建库（而不是手写 schema）：迁移加列时这里永远不会漂移
        let conn = Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        conn
    }

    fn seed(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt) VALUES
                (1, '/a/x1.jpg', 0, 'x1.jpg', 'jpg', 100, 10, 10),
                (2, '/a/x2.png', 1, 'x2.png', 'png', 200, 20, 20),
                (3, '/a/x3.jpg', 0, 'x3.jpg', 'jpg', 300, 30, 30);
             INSERT INTO resource (assetId, role, thumbPath, width, height, pHash) VALUES
                (1, 'primary', '/t/1.webp', 100, 80, 'ab'),
                (2, 'primary', '/t/2.webp', 640, 480, 'cd'),
                (3, 'primary', '/t/3.webp', 1, 1, NULL);
             INSERT INTO tag (assetId, value, origin, score, createdAt) VALUES
                (1, '海', 'auto', 0.9, 1), (1, '手动', 'manual', 1.0, 2), (3, '海', 'auto', 0.8, 3);",
        )
        .unwrap();
    }

    #[test]
    fn list_filter_missing() {
        let c = setup();
        seed(&c);
        let all = list_assets(&c, &ImportFilter::default()).unwrap();
        assert_eq!(all.len(), 3);
        let miss = list_assets(
            &c,
            &ImportFilter {
                missing: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(miss.len(), 1);
        assert_eq!(miss[0].asset.assetId, 2);
        assert_eq!(all[0].thumbPath.as_deref(), Some("/t/3.webp"));
    }

    #[test]
    fn list_filter_tag_and_dir() {
        let c = setup();
        seed(&c);
        let by_tag = list_assets(
            &c,
            &ImportFilter {
                tag: Some("海".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(by_tag.len(), 2);
        let by_dir = list_assets(
            &c,
            &ImportFilter {
                dirPrefix: Some("/a".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(by_dir.len(), 3);
        let none = list_assets(
            &c,
            &ImportFilter {
                dirPrefix: Some("/b".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(none.is_empty());
    }

    /// C7（卡 t_eadc13a3）：关键词全库检索 —— 命中的字段集 / 大小写 / 通配符转义 / 与 total 同口径。
    ///
    /// 用户实测的两条症状各对应一个断言：
    ///  - 「结果不全」= 关键词必须命中**全库**（不是已加载的那一页）→ 这里断言 3 张里
    ///    第 2、3 张（排在 list 尾部）也能命中；
    ///  - 「结果不对」= 用户输入里的 `%`/`_` 不许当通配符放大匹配 → 断言 0 命中。
    #[test]
    fn list_filter_query_keyword() {
        let c = setup();
        seed(&c);
        c.execute_batch(
            "UPDATE assets SET title_ch = '雪山日出' WHERE assetId = 1;
             UPDATE assets SET title_ch = NULL, description = 'Snow mountain at dawn' WHERE assetId = 2;
             UPDATE assets SET title_ch = '海滩', fileName = 'a_b.jpg' WHERE assetId = 3;",
        )
        .unwrap();
        let by = |q: &str| {
            list_assets(
                &c,
                &ImportFilter {
                    query: Some(q.to_string()),
                    ..Default::default()
                },
            )
            .unwrap()
            .into_iter()
            .map(|a| a.asset.assetId)
            .collect::<Vec<_>>()
        };
        // title_ch 中文字面
        assert_eq!(by("雪山"), vec![1]);
        // 描述 + 大小写不敏感（LIKE 默认 ASCII nocase）
        assert_eq!(by("snow"),
            vec![2],
            "英文描述应命中且大小写不敏感");
        assert_eq!(by("SNOW MOUNTAIN"), vec![2]);
        // 文件名子串
        assert_eq!(by("x3.jpg"), vec![3]);
        // 路径维度
        assert_eq!(by("/a/x1"), vec![1]);
        // 空白串 = 不过滤（与前端「空串不发送」同口径）
        assert_eq!(by("   ").len(), 3);
        // 通配符必须被转义：`a_b` 是字面量，不许把 a→b 之间任意字符都算命中
        assert_eq!(by("a_b"), vec![3]);
        assert!(by("a%b").is_empty(), "`%` 不许当通配符放大匹配");
        assert!(by("a\\b").is_empty(), "`\\` 不许吃掉后面的字符");
        // total 与 items 同口径（同一 WHERE）
        let f = ImportFilter {
            query: Some("snow".into()),
            ..Default::default()
        };
        assert_eq!(crate::export::count_assets(&c, &f).unwrap(), 1);
        assert_eq!(list_assets(&c, &f).unwrap().len(), 1);
        let f = ImportFilter {
            query: Some("雪山".into()),
            ..Default::default()
        };
        assert_eq!(crate::export::count_assets(&c, &f).unwrap(), 1);
    }

    /// D3：无法预览的筛选/总数/统计必须**同一口径**（BROKEN_COND）。
    /// 两种坏法都算：没有 primary resource（半截导入）+ resource 尺寸为 0（解码失败）。
    /// 列表走 LEFT JOIN、总数（export::count_assets）只有 FROM assets —— 条件写成
    /// `NOT EXISTS` 才能两处同时贴，不然会出现「列表 2 条、总数 5」。
    #[test]
    fn d3_broken_filter_total_and_stats_share_one_criteria() {
        let c = setup();
        seed(&c); // 1/2/3 都有正尺寸
        c.execute_batch(
            "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt) VALUES
                (4, '/a/x4.jpg', 0, 'x4.jpg', 'jpg', 400, 40, 40),
                (5, '/a/x5.jpg', 0, 'x5.jpg', 'jpg', 500, 50, 50);
             -- 4：半截导入（没有 resource 行）；5：解码失败（导入时写成 0×0）
             INSERT INTO resource (assetId, role, thumbPath, width, height, pHash) VALUES
                (5, 'primary', '/t/5.webp', 0, 0, NULL);",
        )
        .unwrap();

        let broken = list_assets(
            &c,
            &ImportFilter {
                broken: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(broken.len(), 2, "半截导入 + 解码失败都要算「无法预览」");
        assert_eq!(
            broken.iter().map(|v| v.asset.assetId).collect::<Vec<_>>(),
            vec![5, 4],
            "按导入时间倒序"
        );
        let ok = list_assets(
            &c,
            &ImportFilter {
                broken: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ok.len(), 3, "broken=0 是正向补集，不能漏也不能多");
        // 列表条数 == 总数（两段 SQL 同口径）
        assert_eq!(
            crate::export::count_assets(
                &c,
                &ImportFilter {
                    broken: Some(1),
                    ..Default::default()
                }
            )
            .unwrap(),
            2
        );
        // 统计同一口径
        let s = stats(&c).unwrap();
        assert_eq!(s.broken, 2);
        assert_eq!(s.assets, 5, "broken 不影响总数");
        // 组合维度：broken × 失联 互不干扰（失联=2 是 seed 里的 x2.png，它有正尺寸）
        let both = list_assets(
            &c,
            &ImportFilter {
                broken: Some(1),
                missing: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(both.is_empty(), "x2.png 可预览，只是失联");
    }

    #[test]
    fn list_ordering_latest_first() {
        let c = setup();
        seed(&c);
        let all = list_assets(&c, &ImportFilter::default()).unwrap();
        assert_eq!(all[0].asset.assetId, 3);
    }

    #[test]
    fn tags_manual_first() {
        let c = setup();
        seed(&c);
        let tags = asset_tags(&c, 1).unwrap();
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].value, "手动");
        assert_eq!(tags[1].value, "海");
        let cloud = tag_cloud(&c, 10).unwrap();
        assert_eq!(cloud[0], ("海".to_string(), 2));
    }

    #[test]
    fn stats_counts() {
        let c = setup();
        seed(&c);
        let s = stats(&c).unwrap();
        assert_eq!(s.assets, 3);
        assert_eq!(s.missing, 1);
        assert_eq!(s.tagged, 2);
        assert_eq!(s.totalBytes, 600);
        assert_eq!(s.byMediaType.len(), 1);
        assert_eq!(s.byMediaType[0].count, 3);
    }

    #[test]
    fn primary_resource_lookup() {
        let c = setup();
        seed(&c);
        assert!(primary_resource(&c, 1).unwrap().is_some());
        assert!(primary_resource(&c, 999).unwrap().is_none());
    }

    // ---------- OCR-MATCH：字面融合（FTS5 倒排） ----------

    /// 建 N 个资产（assetId 1..=N），第 i 个带 ocr_text = texts[i-1]，并建索引行。
    fn seed_ocr(conn: &Connection, texts: &[&str]) {
        for (i, t) in texts.iter().enumerate() {
            let id = (i + 1) as i64;
            conn.execute(
                "INSERT INTO assets (assetId, absPath, fileName, fileExt, fileSz, mtime, importedAt, mediaType)
                 VALUES (?1, ?2, ?3, 'png', 1, 1, 1, 'image')",
                rusqlite::params![id, format!("/a/{id}.png"), format!("{id}.png")],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO resource (assetId, role, thumbPath, width, height) VALUES (?1, 'primary', ?2, 100, 80)",
                rusqlite::params![id, format!("/t/{id}.webp")],
            )
            .unwrap();
            // 先落 ocr_text（触发器会标 dirty），再走索引写路径
            // （生产 = svc-pipeline::ocr_one：UPDATE assets.ocr_text → ocr_index::upsert）
            conn.execute(
                "UPDATE assets SET ocr_text = ?2 WHERE assetId = ?1",
                rusqlite::params![id, t],
            )
            .unwrap();
            core_storage::ocr_index::upsert(conn, id, t).unwrap();
        }
    }

    fn hit(asset_id: i64, score: f32) -> engine_search::SearchHit {
        engine_search::SearchHit {
            assetId: asset_id,
            resourceId: asset_id,
            score,
            fileName: format!("{asset_id}.png"),
            titleCh: None,
            thumbPath: Some(format!("/t/{asset_id}.webp")),
            width: Some(100),
            height: Some(80),
            ocrMatched: false,
            ocrSnippet: None,
        }
    }

    #[test]
    fn ocr_fuse_raises_semantic_and_adds_literal_only_hit() {
        let c = setup();
        seed_ocr(&c, &["发票号 8891\n合计 1200 元", "无关内容"]);
        // 语义只召回了资产 1（弱相关 0.2，低于 OCR 融合分）
        let mut hits = vec![hit(1, 0.2)];
        // 资产 2 也应被「发票」字面命中？不 —— 2 的文本无关，这里先只验证抬分
        fuse_ocr_text(&c, &mut hits, "发票", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].ocrMatched, "字面命中必须标记 ocrMatched");
        assert!((hits[0].score - OCR_FUSION_SCORE).abs() < 1e-6, "弱语义抬到融合分");
        assert_eq!(hits[0].ocrSnippet.as_deref(), Some("发票号 8891"));

        // 语义一个都没召回：字面命中要能补入（此前 `resources` 表名错误导致静默丢弃）
        let c2 = setup();
        seed_ocr(&c2, &["数字资产管理平台", "授权书 模板"]);
        let mut hits = vec![hit(1, 0.9), hit(9, 0.8)];
        fuse_ocr_text(&c2, &mut hits, "授权书", 10).unwrap();
        let added = hits.iter().find(|h| h.assetId == 2).expect("字面独有命中必须补入");
        assert!(added.ocrMatched);
        assert!((added.score - OCR_FUSION_SCORE).abs() < 1e-6);
        assert_eq!(added.ocrSnippet.as_deref(), Some("授权书 模板"));
        assert_eq!(added.resourceId, 2);
        // 语义命中（id 1/9）与本次查询无字面关系 → 不得被误标
        assert!(!hits.iter().find(|h| h.assetId == 1).unwrap().ocrMatched);
        assert!(!hits.iter().find(|h| h.assetId == 9).unwrap().ocrMatched);
    }

    #[test]
    fn ocr_fuse_partial_hit_is_discounted() {
        let c = setup();
        seed_ocr(&c, &["发票 报销单", "发票 号 001"]);
        let mut hits = Vec::new();
        fuse_ocr_text(&c, &mut hits, "发票 报销", 10).unwrap();
        assert_eq!(hits.len(), 2);
        // 全命中：0.35；部分命中（2 个 term 命中 1 个）：0.35 × 0.6 × 1/2
        assert!((hits[0].score - OCR_FUSION_SCORE).abs() < 1e-6, "{:?}", hits[0]);
        assert!((hits[1].score - OCR_FUSION_SCORE * OCR_PARTIAL_FACTOR * 0.5).abs() < 1e-6);
        assert!(hits.iter().all(|h| h.ocrMatched));
    }

    #[test]
    fn ocr_fuse_cjk_single_char_allowed_latin_needs_whole_word() {
        let c = setup();
        seed_ocr(&c, &["发票号 8891", "Digital Asset Management"]);
        // 中文单字（旧口径 ≥2 字符会漏）
        let mut hits = Vec::new();
        fuse_ocr_text(&c, &mut hits, "发", 10).unwrap();
        assert_eq!(hits.len(), 1, "单字「发」应命中含「发票号」的资产");
        assert_eq!(hits[0].assetId, 1);

        // 英文按整词：词命中
        let mut hits = Vec::new();
        fuse_ocr_text(&c, &mut hits, "Management", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].assetId, 2);
        // 英文子串不命中词内（口径声明：词级，不是子串）
        let mut hits = Vec::new();
        fuse_ocr_text(&c, &mut hits, "anage", 10).unwrap();
        assert!(hits.is_empty(), "英文子串不应命中：{hits:?}");
        // 单字符数字不参与
        let mut hits = Vec::new();
        fuse_ocr_text(&c, &mut hits, "8", 10).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn ocr_fuse_truncates_to_k_with_deterministic_tie_order() {
        let c = setup();
        seed_ocr(&c, &["发票 A", "发票 B", "发票 C"]);
        let mut hits = Vec::new();
        fuse_ocr_text(&c, &mut hits, "发票", 2).unwrap();
        assert_eq!(hits.len(), 2, "同分也要截断到 k");
        assert_eq!(
            hits.iter().map(|h| h.assetId).collect::<Vec<_>>(),
            vec![1, 2],
            "同分按 assetId 升序（结果确定）"
        );
    }

    #[test]
    fn ocr_fuse_no_terms_no_text_is_noop() {
        let c = setup();
        seed_ocr(&c, &["发票"]);
        let mut hits = vec![hit(1, 0.5)];
        fuse_ocr_text(&c, &mut hits, "   ", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].ocrMatched);
        assert!((hits[0].score - 0.5).abs() < 1e-6);

        // 空库/无 ocr_text：不误标
        let c2 = setup();
        let mut hits = vec![hit(1, 0.5)];
        fuse_ocr_text(&c2, &mut hits, "发票", 10).unwrap();
        assert!(!hits[0].ocrMatched);
    }

    #[test]
    fn ocr_snippet_windows_and_keeps_original_case() {
        let long_line = format!("{}发票号 A-001 合计 1200 元{}", "前".repeat(60), "后".repeat(200));
        let s = ocr_snippet(&long_line, &["发票".to_string()]).unwrap();
        assert!(s.starts_with('…'), "长行左侧截断要带省略号：{s}");
        assert!(s.ends_with('…'), "长行右侧截断要带省略号");
        assert!(s.chars().count() <= OCR_SNIPPET_CHARS + 2, "窗口不超过 {} 字符", OCR_SNIPPET_CHARS);
        assert!(s.contains("发票号 A-001"));

        // 第二个匹配行（第一行不含 term）
        let text = "第一行 无关\n第二行 Management 授权\n第三行";
        let s = ocr_snippet(text, &["management".to_string()]).unwrap();
        assert_eq!(s, "第二行 Management 授权", "大小写折叠定位，但返回原文");

        // 找不到 term → 回退首行；空文本 → None
        assert_eq!(ocr_snippet("只有这行", &["不存在".to_string()]).unwrap(), "只有这行");
        assert!(ocr_snippet("   \n\n", &["x".to_string()]).is_none());
    }
}

// ---------- D5：向量检索编排（provider 查询向量化 → engine-search 召回合并） ----------
// R6：web 容器 default-features=false 不编入（无 engine-embed/ort）；W3 接 dam-ai
// HTTP provider 时在 app-web 侧以 provider trait 重新编排，不在本 crate 拉 ort。

/// 文搜图：文本 → 语义塔（provider 必须 text-capable）→ KNN + 标量过滤。
/// provider = 当前激活语义模型（settings.active_embedder），路由即分表。
///
/// `k` = **交付条数**（语义召回本身由 engine-search 内部过采样/兜底保证，调用方不必为
/// 「过滤会吃掉几条」而放大 k）；返回可能少于 k 只有两个正当原因：库内可用向量不足，
/// 或下面 `min_score` 把低相关命中当噪声截断。
///
/// TEXT-EMBED-1 配套：结果按相关性下限截断——低于 `min_score` 的命中一律
/// 不返回（上层 UI 显式呈现「没有命中」空态），不再把 0.0x 的噪声当结果列出。
///
/// `DEFAULT_TEXT_MIN_SCORE` 定标依据（2026-10-17 重标定，卡 t_e8c656a4，
/// 见 reports/D3-text-calibration.md）：
/// - 语料 = WIN-E2E 的 24 张真图（实际画面逐张视觉核对，**不用**语料 manifest 的
///   自动标注——那份标注与画面大量不符：k01 标「马」实为狼、k06 标「滑板」实为虾、
///   k12/k13 标「打印机/复印机」实为超声仪、k19 标「冰棍」实为扳手图标、
///   k21 标「沙滩」实为珊瑚、k22 标「樱桃」实为婚戒）；
/// - 46 条匹配查询（23 图 × 中英文）+ 10 条无关词实测：匹配词 cos 0.0672~0.1503，
///   无关词最高 0.0825（`abstract business strategy`）；
/// - 阈值扫描（见报告表）：0.080 → 无关词漏出 3/10；0.085 → 漏出 **0/10**，
///   匹配命中保持 43/46；0.090 → 保持 41/46。取 **0.085**（漏出清零优先，
///   保留率 93%）。被 0.085 滤掉的 3 条全是中文短查询（中文侧 cos 系统性比英文
///   低 0.02~0.03），由混合字面召回通道兜底（见 [`fuse_literal_text`]）。
/// 要放宽/关闭用 `TextSearchOptions.min_score`（Some(0.0)=不过滤）。
#[derive(Debug, Clone)]
pub struct TextSearchOptions {
    /// 相关性下限；None = 用 [`DEFAULT_TEXT_MIN_SCORE`]，Some(0.0) = 关闭。
    pub min_score: Option<f32>,
}

impl Default for TextSearchOptions {
    fn default() -> Self {
        Self { min_score: None }
    }
}

/// 语义相关性下限（cos）。定标过程与依据见 [`TextSearchOptions`] 文档与
/// reports/D3-text-calibration.md（24 真图 × 46 匹配查询 + 10 无关词）。
pub const DEFAULT_TEXT_MIN_SCORE: f32 = 0.085;

#[cfg(feature = "embed")]
pub fn search_by_text(
    conn: &Connection,
    provider: &dyn EmbedProvider,
    text: &str,
    k: usize,
    filter: &SearchFilter,
    opts: &TextSearchOptions,
) -> Result<Vec<SearchHit>> {
    let spec = provider.spec();
    if !spec.modalities.iter().any(|m| m == "text") {
        return Err(LibraryError::TextUnsupported(spec.name.clone()));
    }
    if text.trim().is_empty() {
        return Ok(vec![]);
    }
    let min = opts.min_score.unwrap_or(DEFAULT_TEXT_MIN_SCORE);
    let mut hits = engine_search::search_by_vector(
        conn,
        &spec.name,
        spec.dims as usize,
        &provider.embed_texts(&[text])?.into_iter().next().unwrap(),
        k,
        filter,
    )?;
    // 合流裁决（t_d99ca3fe）：先按语义相关性下限截断（TEXT-EMBED-1 的 0.085 重标定，
    // D3 t_e8c656a4），再叠加字面证据融合（文件名/标题/标签 + OCR 字面，见 fuse_literal_text）。
    // `min_score` 的语义是"语义相关性下限"，只约束语义塔噪声；字面命中属确凿证据，其融合分
    // （0.45 / 0.35）不被该下限过滤。
    if min > 0.0 {
        hits.retain(|h| h.score >= min);
    }
    fuse_literal_text(conn, &mut hits, text, k)?;
    Ok(hits)
}

// ---------- 混合检索：字面证据融合（D3 全通道 + OCR-MATCH 的 FTS5 倒排） ----------
//
// 语义塔在中文短查询上 cos 系统性偏低（同概念英文高 0.02~0.03），单靠阈值会
// 把真实命中当噪声滤掉。产品口径：**语义负责排序，字面负责兜底召回**——
// fileName / title_ch / title_en / tag.value / ocr_text 命中即「确凿证据」，
// 其融合分不被语义下限过滤（与 OCR-E 原有裁决一致）。
//
// 合流（t_d99ca3fe）：字面三通道走 D3 的全表扫（fileName/标题/标签），OCR 通道统一走
// OCR-MATCH 的 FTS5 倒排（索引 + 候选截断 + 片段）——两套口径都在，OCR 只有一套打分。

/// 字面证据（文件名 / 标题 / 标签）命中分：整词全中 → 该分；部分命中按比例折扣。
/// 选 0.45：高于语义塔噪声带（≤0.085 被下限截断）与 OCR 弱命中（0.35），
/// 低于强语义命中（英文实测 0.11~0.15）—— 字面命中排在同分语义命中之后，
/// 但能把阈值以下的真实资产召回。
pub const LITERAL_FUSION_SCORE: f32 = 0.45;
/// OCR 文本命中分（融合基准）：整词全部命中 → 该分值；语义分高于它的保持原序。
/// 选 0.35：高于语义塔常见的弱相关尾段（<0.3），低于强语义命中（>0.4）——OCR 是
/// 「确凿字面证据」，但不该压过一张语义强相关的图。
pub const OCR_FUSION_SCORE: f32 = 0.35;
/// 单 token 部分命中的折扣（多词查询只命中部分时按命中比例给分）。
pub const OCR_PARTIAL_FACTOR: f32 = 0.6;
/// 命中片段展示窗口（字符数；超出按窗口截断并加省略号）。
pub const OCR_SNIPPET_CHARS: usize = 120;
/// 片段里匹配位置之前保留的上下文字符数。
pub const OCR_SNIPPET_LEAD: usize = 24;

/// 单个通道的融合分：全 token 命中给满分，部分命中按 [`OCR_PARTIAL_FACTOR`] × 命中比例。
fn fusion_score(full: f32, hits: usize, total: usize) -> f32 {
    if total == 0 || hits == 0 {
        return 0.0;
    }
    if hits == total {
        full
    } else {
        full * OCR_PARTIAL_FACTOR * (hits as f32 / total as f32)
    }
}

/// 按分排序 + 截断 k（同分按 assetId 升序 —— 结果确定，不随哈希/查询计划顺序漂移）。
fn sort_and_truncate(hits: &mut Vec<engine_search::SearchHit>, k: usize) {
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.assetId.cmp(&b.assetId))
    });
    hits.truncate(k);
}

/// 片段回填：只为最终返回的 OCR 命中取原文（≤ k 行的有界 IN 查询）。
fn fill_ocr_snippets(
    conn: &Connection,
    hits: &mut [engine_search::SearchHit],
    terms: &[String],
) -> Result<()> {
    let need: Vec<i64> = hits
        .iter()
        .filter(|h| h.ocrMatched)
        .map(|h| h.assetId)
        .collect();
    if need.is_empty() {
        return Ok(());
    }
    let texts: std::collections::HashMap<i64, String> =
        core_storage::ocr_index::texts(conn, &need)?.into_iter().collect();
    for hit in hits.iter_mut() {
        if hit.ocrMatched {
            hit.ocrSnippet = texts.get(&hit.assetId).and_then(|t| ocr_snippet(t, terms));
        }
    }
    Ok(())
}

/// OCR 通道（OCR-MATCH 重写，2026-10-16）：召回来源是 `core-storage::ocr_index` 的 FTS5
/// 倒排（中文 2-gram + 单字允许），**不再** `SELECT assetId, ocr_text FROM assets ...`
/// 全表扫描 + 逐行 `LIKE`。
/// ① 已在语义结果里的资产：命中 → 分数提升到 max(原分, OCR 融合分) 并标 `ocrMatched`；
/// ② 语义未召回但字面命中的资产：按 OCR 分补入（resourceId 取 primary resource）。
///
/// 本函数只做「召回 + 抬分 + 标记 + 补入」，不排序不截断：D3 合流后字面通道与 OCR 通道
/// 必须在同一轮排序里比较（见 [`fuse_literal_text`]），否则先截断会把另一通道的命中挤掉。
///
/// 召回口径（索引/查询同一分段函数，见 `core_storage::ocr_index`）：中文 ≥2 字按
/// 相邻 2-gram、中文单字按单字、英文/数字按整词（≥2 字符）。
fn fuse_ocr_channel(
    conn: &Connection,
    hits: &mut Vec<engine_search::SearchHit>,
    text: &str,
    k: usize,
) -> Result<()> {
    let terms = core_storage::ocr_index::query_terms(text);
    if terms.is_empty() {
        return Ok(());
    }
    // 候选截断：命中面大的 term 按 FTS5 rank 取头部（见 term_candidate_cap）
    let cap = core_storage::ocr_index::term_candidate_cap(k);
    let counts = core_storage::ocr_index::match_counts(conn, &terms, cap)?;
    for (asset_id, n) in &counts {
        let s = fusion_score(OCR_FUSION_SCORE, *n, terms.len());
        if let Some(hit) = hits.iter_mut().find(|h| h.assetId == *asset_id) {
            if s > hit.score {
                hit.score = s;
            }
            hit.ocrMatched = true;
            continue;
        }
        // 语义未召回：按 OCR 分补入（表名 `resource`，此前写成 `resources` 导致
        // 补入项被 `if let Ok` 静默吞掉 —— 字面独有命中永远不返回，已修）
        let row = conn.query_row(
            "SELECT r.resourceId, a.fileName, a.title_ch, r.thumbPath, r.width, r.height
             FROM resource r JOIN assets a ON a.assetId = r.assetId
             WHERE r.assetId = ?1 AND a.deleted = 0 AND r.role = 'primary'
             ORDER BY r.resourceId LIMIT 1",
            [asset_id],
            |r| {
                Ok(engine_search::SearchHit {
                    assetId: *asset_id,
                    resourceId: r.get(0)?,
                    score: s,
                    fileName: r.get(1)?,
                    titleCh: r.get(2)?,
                    thumbPath: r.get(3)?,
                    width: r.get(4)?,
                    height: r.get(5)?,
                    ocrMatched: true,
                    ocrSnippet: None,
                })
            },
        );
        match row {
            Ok(hit) => hits.push(hit),
            // 没有 primary resource 的资产（半截导入）不进结果；但这不是静默丢分：
            // 有文本命中却没 resource 行是数据问题，调用方统计里能看出候选数 > 返回数。
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// 把 OCR 字面命中融合进语义结果（OCR-MATCH 时期的入口，行为与合并前一致）：
/// 召回（FTS5）→ 按分排序截断 k → 回填 `ocrSnippet`。
pub fn fuse_ocr_text(
    conn: &Connection,
    hits: &mut Vec<engine_search::SearchHit>,
    text: &str,
    k: usize,
) -> Result<()> {
    let terms = core_storage::ocr_index::query_terms(text);
    fuse_ocr_channel(conn, hits, text, k)?;
    sort_and_truncate(hits, k);
    fill_ocr_snippets(conn, hits, &terms)
}

/// 把「字面证据」融合进语义结果（D3 混合检索的入口 = 全通道）：
/// ① 字面通道 fileName / title_ch / title_en / tag.value → [`LITERAL_FUSION_SCORE`]；
/// ② OCR 通道 `assets.ocr_text` → [`OCR_FUSION_SCORE`]（FTS5 倒排，见 [`fuse_ocr_channel`]）；
/// ③ 同一资产取各通道最高分，已在语义结果里的只升不降；语义未召回的字面独有命中补入。
/// 查询分词 = 空白切分（检索框语义，≥2 字符，上限 8 个 token），拉丁字符大小写不敏感。
///
/// 合流说明（t_d99ca3fe）：D3 原实现把 ocr_text 也放在这条全表扫里逐行 contains；
/// 合流后 OCR 通道统一走 FTS5 倒排（索引 + 候选截断 + 片段），全表扫只留
/// fileName / 标题 / 标签三通道 —— 两张卡的口径同时保留，OCR 不再有第二套打分。
pub fn fuse_literal_text(
    conn: &Connection,
    hits: &mut Vec<engine_search::SearchHit>,
    text: &str,
    k: usize,
) -> Result<()> {
    let terms = core_storage::ocr_index::query_terms(text);
    let tokens: Vec<String> = text
        .split_whitespace()
        .filter(|t| t.chars().count() >= 2)
        .take(8)
        .map(|t| t.to_lowercase())
        .collect();
    if tokens.is_empty() && terms.is_empty() {
        return Ok(());
    }
    if !tokens.is_empty() {
        let count_hits = |haystack: &str| -> usize {
            if haystack.is_empty() {
                return 0;
            }
            let low = haystack.to_lowercase();
            tokens.iter().filter(|t| low.contains(t.as_str())).count()
        };
        // 单 SQL 一次扫 assets（文件名 + 中英标题）；命中数在 Rust 侧统计
        let mut best: std::collections::HashMap<i64, f32> = std::collections::HashMap::new();
        {
            let mut stmt = conn.prepare_cached(
                "SELECT assetId, fileName, COALESCE(title_ch,''), COALESCE(title_en,'')
                 FROM assets WHERE deleted = 0",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            for row in rows {
                let (asset_id, file_name, title_ch, title_en) = row?;
                let lit = count_hits(&file_name)
                    .max(count_hits(&title_ch))
                    .max(count_hits(&title_en));
                let s = fusion_score(LITERAL_FUSION_SCORE, lit, tokens.len());
                if s > 0.0 {
                    best.insert(asset_id, s);
                }
            }
        }
        // 标签通道（tag.value：标签值参与字面检索）
        {
            let mut stmt = conn.prepare_cached(
                "SELECT t.assetId, t.value FROM tag t JOIN assets a ON a.assetId = t.assetId
                 WHERE a.deleted = 0",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (asset_id, value) = row?;
                let s = fusion_score(LITERAL_FUSION_SCORE, count_hits(&value), tokens.len());
                if s > 0.0 {
                    let e = best.entry(asset_id).or_insert(0.0);
                    if s > *e {
                        *e = s;
                    }
                }
            }
        }
        if !best.is_empty() {
            let mut matched: Vec<(i64, f32)> = best.into_iter().collect();
            matched.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            for hit in hits.iter_mut() {
                if let Some((_, s)) = matched.iter().find(|(a, _)| *a == hit.assetId) {
                    if *s > hit.score {
                        hit.score = *s;
                    }
                }
            }
            for (asset_id, s) in matched {
                if hits.iter().any(|h| h.assetId == asset_id) {
                    continue;
                }
                let row = conn.query_row(
                    "SELECT r.resourceId, a.fileName, a.title_ch, r.thumbPath, r.width, r.height
                     FROM resource r JOIN assets a ON a.assetId = r.assetId
                     WHERE r.assetId = ?1 AND a.deleted = 0 AND r.role = 'primary'
                     ORDER BY r.resourceId LIMIT 1",
                    [asset_id],
                    |r| {
                        Ok(engine_search::SearchHit {
                            assetId: asset_id,
                            resourceId: r.get(0)?,
                            score: s,
                            fileName: r.get(1)?,
                            titleCh: r.get(2)?,
                            thumbPath: r.get(3)?,
                            width: r.get(4)?,
                            height: r.get(5)?,
                            ocrMatched: false,
                            ocrSnippet: None,
                        })
                    },
                );
                match row {
                    Ok(hit) => hits.push(hit),
                    Err(rusqlite::Error::QueryReturnedNoRows) => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
    // OCR 通道（FTS5 倒排）：抬分 + 标 ocrMatched + 补入
    fuse_ocr_channel(conn, hits, text, k)?;
    sort_and_truncate(hits, k);
    fill_ocr_snippets(conn, hits, &terms)
}

/// 命中片段：取第一个含任一 term 的行，按 [`OCR_SNIPPET_CHARS`] 字符开窗
/// （命中位置前留 [`OCR_SNIPPET_LEAD`] 字符；截断侧加省略号）。大小写折叠后定位，
/// 但返回的是**原文**（不把小写化文本当结果）。找不到任何 term → 回退首个非空行。
pub fn ocr_snippet(text: &str, terms: &[String]) -> Option<String> {
    let mut fallback: Option<&str> = None;
    for line in text.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if fallback.is_none() {
            fallback = Some(trimmed);
        }
        let orig: Vec<char> = trimmed.chars().collect();
        let folded: Vec<char> = trimmed.chars().flat_map(char::to_lowercase).collect();
        // folded 下标 → 原文下标（折叠可能 1→N，取每个折叠字符对应的原文起点）
        let map = fold_map(trimmed);
        for t in terms {
            let tc: Vec<char> = t.chars().collect();
            if tc.is_empty() || tc.len() > folded.len() {
                continue;
            }
            let found = (0..=folded.len() - tc.len()).find(|i| folded[*i..i + tc.len()] == tc[..]);
            if let Some(p) = found {
                let start_orig = map[p];
                let end_orig = map[p + tc.len() - 1] + 1;
                return Some(window(&orig, start_orig, end_orig));
            }
        }
    }
    fallback.map(|s| s.to_string())
}

/// 每个折叠后字符对应的原文 char 下标（与 `chars().flat_map(to_lowercase)` 同序）。
fn fold_map(line: &str) -> Vec<usize> {
    let mut map = Vec::new();
    for (i, c) in line.chars().enumerate() {
        for _ in 0..c.to_lowercase().count() {
            map.push(i);
        }
    }
    map
}

/// 取 [start, end) 附近的展示窗口（前留 LEAD 字符，总长 ≤ SNIPPET_CHARS）。
fn window(chars: &[char], start: usize, end: usize) -> String {
    let lead = start.saturating_sub(OCR_SNIPPET_LEAD);
    let take = OCR_SNIPPET_CHARS
        .min(chars.len().saturating_sub(lead))
        .max(end - lead);
    let tail = (lead + take).min(chars.len());
    let mut s = String::new();
    if lead > 0 {
        s.push('…');
    }
    s.extend(chars[lead..tail].iter().collect::<String>().trim().chars());
    if tail < chars.len() {
        s.push('…');
    }
    s.trim().to_string()
}

/// 以图搜图的相关性下限（**按模型标定**，不同模型 cos 尺度差 3 倍以上，不能共用同一个数）。
///
/// 定标依据（卡 t_d00827d5，2026-10-17，报告 `reports/W3-image-search.md`）：
/// 27 张真图（24 张 WIN-E2E 语料 + 4 张 smoke，全量 351 对 cos）+ 逐张视觉真值判定
/// （L3 严格同类 / L2 宽泛同类 / L1 无关）：
/// - `siglip2-base-p224-onnx`：无关对最高 0.7108（山↔地球），严格同类最低 0.7207（两个包）
///   → **0.72**（无关漏出 0/351，严格同类 top1 5/13；解耦带宽仅 0.01，是这套图的现实上限）
/// - `dinov3-vits16-onnx`：无关对最高 0.1979，同类命中带 0.30~0.78 → **0.20**
///   （无关漏出 0，严格同类 top1 7/13——同为零漏出下比语义塔多 2 条，且单图编码 2.8× 快）
///   —— **KI-3：该标定已随 dinov3 不再可当活跃语义塔一并删除**（去重塔写死不可选，
///   判重阈值是另一套常量 `NEAR_DUP_THRESHOLD` 0.98 / `VARIANT_THRESHOLD` 0.90），
///   此数只作历史记录。
/// - 未标定模型（远程 `openai_compat` 等）：返回 0.0 = **不过滤**（余弦尺度未知，不猜数字），
///   `/api/search/status` 会把它标成 calibrated=false 让 UI 说明。
///
/// 入参可能是**路由键**而非裸 spec 名：`vector_model_key` 给远程端点加
/// `:{endpoint model}` 后缀（`openai-compat-embedding:qwen3-vl-embedding-2b`），
/// 内嵌 spec 传裸名。统一按 `:` 前缀归一，避免同一个模型两种写法拿到不同下限
/// （t_835d7366）。
pub fn default_similar_min_score(model: &str) -> f32 {
    match model.split(':').next().unwrap_or(model).trim() {
        "siglip2-base-p224-onnx" => 0.72,
        _ => 0.0,
    }
}

/// 该模型是否已标定相关性下限（UI 用来决定是否展示"相似度 ≥ N%"）。
pub fn similar_min_score_calibrated(model: &str) -> bool {
    default_similar_min_score(model) > 0.0
}

/// 以图搜图选项（形态与 [`TextSearchOptions`] 一致）。
#[derive(Debug, Clone)]
pub struct SimilarOptions {
    /// 相关性下限；None = 用 [`default_similar_min_score`]（按模型标定），Some(0.0) = 关闭。
    pub min_score: Option<f32>,
}

impl Default for SimilarOptions {
    fn default() -> Self {
        Self { min_score: None }
    }
}

impl SimilarOptions {
    /// 解析出生效下限（clamp 到 [0,1]，防误传把结果全砍光/全放过）。
    pub fn effective(&self, model: &str) -> f32 {
        self.min_score
            .unwrap_or_else(|| default_similar_min_score(model))
            .clamp(0.0, 1.0)
    }
}

/// 以图搜图（语义相似）：图片文件 → 语义塔 image 向量 → KNN + 标量过滤 + 相关性下限。
/// 传磁盘路径（provider embed_images 以路径为批单位；UI「找相似」资产本来就有 absPath）。
///
/// `k` = **交付条数**（口径同 [`search_by_text`]）：回收站/孤儿向量不占名额，
/// 少于 k 只因可用向量不足或被 `min_score` 截断。
#[cfg(feature = "embed")]
pub fn search_by_image_path(
    conn: &Connection,
    provider: &dyn EmbedProvider,
    image_path: &std::path::Path,
    k: usize,
    filter: &SearchFilter,
    opts: &SimilarOptions,
) -> Result<Vec<SearchHit>> {
    let spec = provider.spec();
    if !image_path.is_file() {
        return Err(LibraryError::ImageIo(format!("{image_path:?} not found")));
    }
    let qvec = provider
        .embed_images(&[image_path.to_path_buf()])?
        .into_iter()
        .next()
        .ok_or_else(|| LibraryError::Embed(engine_embed::EmbedError::BatchSize(1, 0)))?;
    let mut hits = engine_search::search_by_vector(
        conn,
        &spec.name,
        spec.dims as usize,
        &qvec,
        k,
        filter,
    )?;
    // 相关性下限（TEXT-EMBED-1 的同款裁决，作用于 image 相似度）：低分命中当"没有相似图"。
    // 库内资产查询时自身命中恒为 1.0，不受影响。
    let min = opts.effective(&spec.name);
    if min > 0.0 {
        hits.retain(|h| h.score >= min);
    }
    Ok(hits)
}

/// 资产以图搜图（详情页「找相似」入口）：取该资产 absPath 走 image 塔。
#[cfg(feature = "embed")]
pub fn find_similar(
    conn: &Connection,
    provider: &dyn EmbedProvider,
    asset_id: i64,
    k: usize,
    filter: &SearchFilter,
    opts: &SimilarOptions,
) -> Result<Vec<SearchHit>> {
    let path: Option<String> = conn
        .query_row(
            "SELECT absPath FROM assets WHERE assetId = ?1 AND deleted = 0",
            [asset_id],
            |r| r.get(0),
        )
        .ok();
    let path = path.ok_or_else(|| LibraryError::Sqlite(rusqlite::Error::QueryReturnedNoRows))?;
    // G5-A：碰磁盘前先探测（相似图重算 = 真实读原图点）——不在了 → missing=1；
    // 文件回来了 → 清标。判据与导出/原图读取同源（core_storage::presence）。
    core_storage::presence::probe_and_note(conn, asset_id, std::path::Path::new(&path));
    Ok(search_by_image_path(
        conn,
        provider,
        std::path::Path::new(&path),
        k,
        filter,
        opts,
    )?)
}

// ---------- KI-5「精准匹配」：语义塔扩池召回 + 判重塔（DINOv3）rerank ----------
//
// 需求口径（2026-10-18 用户）：「视觉反查（只要极像的图）」与「相似图搜索」是两种意图，
// 不新开页面/不新增第二个搜索入口，而是给相似图搜索一个**模式**：
//  - `semantic`（默认）= 今天的行为，零回归；
//  - `visual_precise` = 语义塔扩池取候选 → 判重塔 cos 排序 → 0.90/0.98 分带截断。
//
// 与 W3 架构裁决 C（`references/w3-image-similarity.md` §4「不做双塔」）的关系：
// 本模式是**唯一被允许**读 `dedup_vectors` 的搜索路径，范围写死 ——
// 不新增可选 embedder、不动 `/api/embed`、不与语义向量混算、不把 dinov3 当 active_embedder。
// 判重塔名恒为 `core_config::DEDUP_EMBEDDER`（接口无模型名参数，与 KI-2 同口径）。

/// 精准匹配的分带阈值：与查重页同一套口径（`engine_search::NEAR_DUP_THRESHOLD` /
/// `VARIANT_THRESHOLD`，即 0.98 / 0.90），不另立数字。
///
/// **不要复用 `default_similar_min_score` 的 0.20**：那是语义塔「零无关漏出」的检索下限，
/// 不是「仅高度相似」的下限；0.20 会把语义相近但主体不同的图全放进来。
pub const PRECISE_NEAR_THRESHOLD: f32 = engine_search::NEAR_DUP_THRESHOLD;
pub const PRECISE_VARIANT_THRESHOLD: f32 = engine_search::VARIANT_THRESHOLD;

/// 精准匹配候选池倍数：先用语义塔取 k×N（minScore=0）当候选，再用判重塔 rerank。
/// 直接在语义 top-k（已被 0.72 截断）上 rerank 会让精准模式大量空结果 —— 这是最容易踩的坑。
pub const PRECISE_POOL_FACTOR: usize = 4;
/// 候选池上限（`engine_search::search_by_vector` 的 KNN 上限也是 500）。
pub const PRECISE_POOL_CAP: usize = 500;

/// 找相似的模式（KI-5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimilarMode {
    /// 语义相似：今天的默认行为（W3 标定阈值，siglip2 0.72 / dinov3 0.20）
    #[default]
    Semantic,
    /// 精准匹配：同图变体 / 近似重复（DINOv3 rerank + 0.90/0.98 分带）
    VisualPrecise,
}

impl SimilarMode {
    /// 接口/日志用的稳定字面量（与 serde 取值一致）。
    pub fn as_str(&self) -> &'static str {
        match self {
            SimilarMode::Semantic => "semantic",
            SimilarMode::VisualPrecise => "visual_precise",
        }
    }
}

/// 精准匹配命中行（`SearchHit` 的字段 + 分带 + 语义塔原始分，便于解释「为什么这条进来了」）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreciseHit {
    pub asset_id: i64,
    pub resource_id: i64,
    /// **判重塔 cos**（本次排序分，不是语义分）
    pub score: f32,
    /// 分带：`near_duplicate`（≥0.98，压缩/重存）或 `variant`（0.90~0.98，裁剪/换色/加字）
    pub band: String,
    pub file_name: String,
    pub title_ch: Option<String>,
    pub thumb_path: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// 同一候选在语义塔里的 cos（诊断用：它凭什么叫候选）
    pub semantic_score: f32,
}

/// 精准匹配结果（不只是 hits：缺向量必须**显式可见**，不许静默变少）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreciseResult {
    pub hits: Vec<PreciseHit>,
    /// 语义塔扩池后的候选数（不含查询图自身）
    pub candidate_pool: usize,
    /// 候选里没有判重向量（`dedup_vectors` 缺行）的条数 —— UI 必须提示「先跑嵌入」
    pub missing_dedup_vectors: usize,
    /// 查询图自己没有判重向量（此时无从 rerank，hits 恒空）
    pub query_missing_dedup_vector: bool,
    /// 判重向量表是否存在（false = 全库还没算过判重向量）
    pub dedup_index_ready: bool,
    /// 本次分带阈值（前端不写死数字，回带显示）
    pub near_threshold: f32,
    pub variant_threshold: f32,
    /// 查询图自身已被剔除（自身 cos 恒 1.0，留着必然占满 near 档）
    pub self_excluded: bool,
}

/// 取资产的 primary resource id（判重向量表按 resourceId 存）。
fn primary_resource_id(conn: &Connection, asset_id: i64) -> Option<i64> {
    conn.query_row(
        "SELECT r.resourceId FROM resource r
         WHERE r.assetId = ?1 AND r.role = 'primary'
         ORDER BY r.resourceId LIMIT 1",
        [asset_id],
        |r| r.get(0),
    )
    .ok()
}

/// 精准匹配（KI-5）：图片 → 语义塔扩池候选 → 判重塔 cos → 0.90/0.98 分带。
///
/// 两条硬约束（都有回归测试守着）：
/// 1. **候选池必须扩**：语义塔用 `minScore=0` 取 k×[`PRECISE_POOL_FACTOR`]，不沿用 0.72 截断；
/// 2. **缺向量不许静默变少**：候选无判重向量进 `missing_dedup_vectors`，查询图无向量进
///    `query_missing_dedup_vector`（依赖 KI-2a：入库即算判重向量，否则整库恒空）。
#[cfg(feature = "embed")]
pub fn find_similar_precise(
    conn: &Connection,
    provider: &dyn EmbedProvider,
    asset_id: i64,
    k: usize,
    filter: &SearchFilter,
) -> Result<PreciseResult> {
    let path: Option<String> = conn
        .query_row(
            "SELECT absPath FROM assets WHERE assetId = ?1 AND deleted = 0",
            [asset_id],
            |r| r.get(0),
        )
        .ok();
    let path = path.ok_or_else(|| LibraryError::Sqlite(rusqlite::Error::QueryReturnedNoRows))?;

    // G5-A：精准匹配同样要读原图（语义塔 query 向量）→ 同一个失败点顺手记账
    core_storage::presence::probe_and_note(conn, asset_id, std::path::Path::new(&path));

    let k = k.clamp(1, PRECISE_POOL_CAP);
    let pool = k.saturating_mul(PRECISE_POOL_FACTOR).clamp(k, PRECISE_POOL_CAP);
    // ① 语义塔扩池召回：显式 min_score=0（不套标定阈值），候选留给判重塔裁决
    let semantic = search_by_image_path(
        conn,
        provider,
        std::path::Path::new(&path),
        pool,
        filter,
        &SimilarOptions {
            min_score: Some(0.0),
        },
    )?;
    let candidates: Vec<&SearchHit> = semantic.iter().filter(|h| h.assetId != asset_id).collect();
    let dedup_model = core_config::DEDUP_EMBEDDER;
    let dedup_index_ready = engine_search::has_store(conn, dedup_model);

    // ② 查询图自己的判重向量（判重塔「写一次、读多处」：这里只读不算，避免在 web 形态要求本地 ONNX）
    let query_vec: Option<Vec<f32>> = if dedup_index_ready {
        primary_resource_id(conn, asset_id).and_then(|rid| {
            core_storage::vecstore::fetch_vectors(conn, dedup_model, &[rid])
                .ok()
                .and_then(|mut v| v.pop().map(|(_, x)| x))
        })
    } else {
        None
    };

    let cand_ids: Vec<i64> = candidates.iter().map(|h| h.resourceId).collect();
    let fetched: std::collections::HashMap<i64, Vec<f32>> = if dedup_index_ready && !cand_ids.is_empty() {
        core_storage::vecstore::fetch_vectors(conn, dedup_model, &cand_ids)?
            .into_iter()
            .collect()
    } else {
        Default::default()
    };
    let missing_dedup_vectors = cand_ids
        .iter()
        .filter(|rid| !fetched.contains_key(rid))
        .count();

    let mut hits: Vec<PreciseHit> = Vec::new();
    if let Some(qv) = query_vec.as_ref() {
        for h in &candidates {
            let Some(cv) = fetched.get(&h.resourceId) else {
                continue;
            };
            let score = engine_search::cosine(qv, cv);
            if !score.is_finite() || score < PRECISE_VARIANT_THRESHOLD {
                continue;
            }
            hits.push(PreciseHit {
                asset_id: h.assetId,
                resource_id: h.resourceId,
                score,
                band: if score >= PRECISE_NEAR_THRESHOLD {
                    "near_duplicate".to_string()
                } else {
                    "variant".to_string()
                },
                file_name: h.fileName.clone(),
                title_ch: h.titleCh.clone(),
                thumb_path: h.thumbPath.clone(),
                width: h.width,
                height: h.height,
                semantic_score: h.score,
            });
        }
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(k);

    Ok(PreciseResult {
        hits,
        candidate_pool: candidates.len(),
        missing_dedup_vectors,
        query_missing_dedup_vector: query_vec.is_none(),
        dedup_index_ready,
        near_threshold: PRECISE_NEAR_THRESHOLD,
        variant_threshold: PRECISE_VARIANT_THRESHOLD,
        self_excluded: true,
    })
}

/// 查重（DINOv3 塔）：阈值分带 near（≥0.98）/ variant（[0.90,0.98)）。
#[cfg(feature = "embed")]
pub fn find_duplicates(
    conn: &Connection,
    dedup_provider: &dyn EmbedProvider,
    opts: &DupOptions,
) -> Result<Vec<DupGroup>> {
    Ok(engine_search::find_duplicates(
        conn,
        &dedup_provider.spec().name,
        opts,
    )?)
}

// ---------- D7：精确重复分组 + 软删除/恢复（回收站读模型） ----------

/// 精确重复组成员（同 fileHash）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExactDupMember {
    pub asset_id: i64,
    pub file_name: String,
    pub abs_path: String,
    pub file_sz: i64,
    pub imported_at: i64,
}

/// 精确重复组：同 fileHash（SHA-256）且 deleted=0 的资产集合。
/// 导入器在入口已按 fileHash 去重（Duplicate 跳过），因此常规导入不会产生组；
/// 组的来源：软删后重导入再恢复、跨库合并、旧库迁移等。发现即整组冗余。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExactDupGroup {
    pub file_hash: String,
    pub members: Vec<ExactDupMember>,
}

/// 精确重复分组（fileHash 聚类，组内 ≥2 个未删除资产）。
pub fn exact_dup_groups(conn: &Connection) -> Result<Vec<ExactDupGroup>> {
    let mut stmt = conn.prepare(
        "SELECT a.fileHash, a.assetId, a.fileName, a.absPath, a.fileSz, a.importedAt
         FROM assets a
         WHERE a.deleted = 0 AND a.fileHash IS NOT NULL
         ORDER BY a.fileHash, a.importedAt, a.assetId",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(ExactDupGroup {
            file_hash: r.get(0)?,
            members: vec![ExactDupMember {
                asset_id: r.get(1)?,
                file_name: r.get(2)?,
                abs_path: r.get(3)?,
                file_sz: r.get(4)?,
                imported_at: r.get(5)?,
            }],
        })
    })?;
    let mut ordered: Vec<ExactDupGroup> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    // 相邻合并（已按 fileHash 排序）；只留 ≥2 成员的组
    let mut groups: Vec<ExactDupGroup> = Vec::new();
    for g in ordered.drain(..) {
        match groups.last_mut() {
            Some(prev) if prev.file_hash == g.file_hash => prev.members.extend(g.members),
            _ => groups.push(g),
        }
    }
    groups.retain(|g| g.members.len() >= 2);
    Ok(groups)
}

/// 软删除（回收站）：deleted=1，可恢复。返回实际置位行数（幂等，重复删不计）。
/// 软删除后资产即从列表/搜索/查重/统计全部消失（读模型恒过滤 deleted=0）。
pub fn soft_delete_assets(conn: &Connection, asset_ids: &[i64]) -> Result<usize> {
    if asset_ids.is_empty() {
        return Ok(0);
    }
    let mut n = 0usize;
    for chunk in asset_ids.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "UPDATE assets SET deleted = 1 WHERE deleted = 0 AND assetId IN ({placeholders})"
        );
        let params: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        n += conn.execute(&sql, params.as_slice())?;
    }
    Ok(n)
}

/// 从回收站恢复：deleted=0。返回实际恢复行数（幂等）。
pub fn restore_assets(conn: &Connection, asset_ids: &[i64]) -> Result<usize> {
    if asset_ids.is_empty() {
        return Ok(0);
    }
    let mut n = 0usize;
    for chunk in asset_ids.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "UPDATE assets SET deleted = 0 WHERE deleted = 1 AND assetId IN ({placeholders})"
        );
        let params: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        n += conn.execute(&sql, params.as_slice())?;
    }
    Ok(n)
}

/// 回收站恢复的逐条回执（RESTORE-CHECKED，卡 t_c117a719）。
///
/// 为什么不能只看 [`restore_assets`] 的返回行数：它对「本来就不在回收站」的 id
/// 返回 0，调用方无法区分「什么都没做」和「这些 id 全都不存在了（已被彻底删除）」。
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreReport {
    /// 真的从回收站恢复回来的（deleted 1 → 0）
    pub restored: Vec<i64>,
    /// 不在回收站：未软删除 / 从未存在 / 已被彻底删除（调用方必须响亮报错）
    pub not_in_trash: Vec<i64>,
}

/// 带回执的恢复：逐 id 判定「在回收站 / 不在」，不再静默把遗漏当成功。
pub fn restore_assets_checked(conn: &Connection, asset_ids: &[i64]) -> Result<RestoreReport> {
    let mut rep = RestoreReport::default();
    for &id in asset_ids {
        let in_trash: Option<i64> = conn
            .query_row("SELECT deleted FROM assets WHERE assetId = ?1", [id], |r| r.get(0))
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        match in_trash {
            Some(1) => {
                conn.execute(
                    "UPDATE assets SET deleted = 0 WHERE assetId = ?1 AND deleted = 1",
                    [id],
                )?;
                rep.restored.push(id);
            }
            _ => rep.not_in_trash.push(id),
        }
    }
    Ok(rep)
}

/// 彻底删除回执（PURGE，不可逆）——逐条回执，前端据 `purged`/`notInTrash` 如实汇报。
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PurgeReport {
    /// 真的被彻底清掉的资产 id（只接受回收站里的）
    pub purged: Vec<i64>,
    /// 拒绝的 id：不在回收站（未软删 / 不存在 / 已彻底删过）——调用方据此如实报「跳过 N 张」
    pub not_in_trash: Vec<i64>,
    /// 删掉的库行数（assets + resource + tag/verdict/ocr/job 等 asset 作用域行）
    pub rows: usize,
    /// 删掉的向量行数（各已登记模型的 resourceId 行）
    pub vectors: usize,
    /// 删掉的缩略图文件数（256 / 1024 两档）
    pub thumbs: usize,
    /// 删掉的磁盘文件总数（缩略图 + copyin 库内原图）
    pub files_deleted: usize,
    /// 真实释放的字节数（只统计确实删掉的文件；NotFound 不计）
    pub freed_bytes: u64,
}

impl PurgeReport {
    /// 被跳过的张数（不在回收站 / 已不存在）—— 前端 toast 直接用它。
    pub fn skipped(&self) -> usize {
        self.not_in_trash.len()
    }
}

/// 彻底删除（回收站 → 永久删除）：清库行 + 向量 + 缩略图 [+ copyin 库内原图]，不可恢复。
///
/// 语义裁决（KI-11）：
/// - **只接受 `deleted = 1`（回收站内）的资产** —— 未软删 / 不存在的 id 一律进 `not_in_trash`，
///   不允许绕过软删一步销毁数据（防「误点把整个图库删了」）；
/// - 库行清理范围按 `PRAGMA table_info` 反射出**所有带 assetId 列的表**（迁移只加表不改表，
///   手写清单会漂移 → 幽灵行），再加 `resource` / `job_queue` / `ocr_fts`；
/// - 向量行按 resourceId 逐已登记模型表删（`core_storage::vecstore::delete_resources`，无 FK）；
/// - 缩略图全档（`core_media::THUMB_SIZES`）删除；
/// - **原图只在 `storeMode = 'copyin'` 时删**（库内副本，库拥有它）；`reference` 是用户原地文件，
///   只删记录、绝不碰磁盘；
/// - 库行在事务内完成（中途失败整批回滚）；文件删除在事务提交之后（文件系统不在事务里，
///   删失败只留残留垃圾，不推翻「资产已不可查」的结论），计数如实反映。
pub fn purge_assets(
    conn: &Connection,
    thumbs_root: &std::path::Path,
    asset_ids: &[i64],
) -> Result<PurgeReport> {
    let mut rep = PurgeReport::default();
    if asset_ids.is_empty() {
        return Ok(rep);
    }
    // 资产作用域表清单（带 assetId 列的全部用户表，除 assets 自身）——一次算好，逐 id 复用
    let tables = asset_scoped_tables(conn)?;
    let mut thumb_files: Vec<std::path::PathBuf> = Vec::new();
    let mut original_files: Vec<std::path::PathBuf> = Vec::new();

    let tx = conn.unchecked_transaction()?;
    for &id in asset_ids {
        // 读资产行：只有 deleted=1 才允许销毁
        let row: Option<(i64, String, String, i64)> = tx
            .query_row(
                "SELECT deleted, storeMode, absPath, missing FROM assets WHERE assetId = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        let (deleted, store_mode, abs_path, missing) = match row {
            Some(v) => v,
            None => {
                rep.not_in_trash.push(id);
                continue;
            }
        };
        if deleted != 1 {
            rep.not_in_trash.push(id);
            continue;
        }

        // 1) 向量：resourceId 粒度，逐已登记模型表清行（表不存在返回 0，不报错）
        let rids: Vec<i64> = {
            let mut stmt = tx.prepare("SELECT resourceId FROM resource WHERE assetId = ?1")?;
            let rows = stmt.query_map([id], |r| r.get::<_, i64>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if !rids.is_empty() {
            for (model, _dims, _tbl) in core_storage::vecstore::tables(&tx)? {
                rep.vectors += core_storage::vecstore::delete_resources(&tx, &model, &rids)?;
            }
        }
        // 2) OCR 倒排（rowid = assetId，走 core-storage 的单一真源）
        core_storage::ocr_index::remove(&tx, id)?;
        // 3) 一切带 assetId 列的表（除 assets 自身；resource 单独处理，见下）
        for t in &tables {
            if t == "resource" {
                continue;
            }
            rep.rows += tx.execute(&format!("DELETE FROM \"{t}\" WHERE assetId = ?1"), [id])?;
        }
        // 4) resource 行（向量已先删，这里才允许断引用）
        rep.rows += tx.execute("DELETE FROM resource WHERE assetId = ?1", [id])?;
        // 5) 排队中/已完成的任务（targetId = assetId；export 批量 job 的 targetId 恒 0）
        rep.rows += tx.execute("DELETE FROM job_queue WHERE targetId = ?1", [id])?;
        // 6) 资产行本身
        rep.rows += tx.execute("DELETE FROM assets WHERE assetId = ?1", [id])?;

        // 缩略图全档（{root}/{size}/{id%100:02}/{id}_{size}.webp）
        for size in core_media::THUMB_SIZES {
            thumb_files.push(
                thumbs_root
                    .join(size.to_string())
                    .join(format!("{:02}", id % 100))
                    .join(format!("{id}_{size}.webp")),
            );
        }
        // 原图：只有库内副本（copyin）才由库删除；reference 是用户原地文件，只删记录
        if store_mode == "copyin" && missing == 0 {
            original_files.push(std::path::PathBuf::from(abs_path));
        }
        rep.purged.push(id);
    }
    tx.commit()?;

    // 文件删除放事务之后（见函数注释）；字节数按删前实测，NotFound 不计（不算释放）
    for p in thumb_files.iter() {
        if remove_file_counted(p, &mut rep) {
            rep.thumbs += 1;
        }
    }
    for p in original_files.iter() {
        remove_file_counted(p, &mut rep);
    }
    Ok(rep)
}

/// 删单个文件并把「文件数 / 字节数」记进回执：NotFound 视为已不在（不计），
/// 其它 IO 错误如实上抛（不吞、不伪造成功）。
fn remove_file_counted(p: &std::path::Path, rep: &mut PurgeReport) -> bool {
    let bytes = std::fs::symlink_metadata(p)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .unwrap_or(0);
    match std::fs::remove_file(p) {
        Ok(()) => {
            rep.files_deleted += 1;
            rep.freed_bytes += bytes;
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            // 行已定局（资产不可查）；文件残留按 IO 错误如实上报，交给上层人化
            eprintln!("[purge] 删文件失败 {}: {e}", p.display());
            false
        }
    }
}

/// 库内所有带 `assetId` 列的用户表（除 `assets` 自身）——彻底删除的作用域真源。
///
/// 用 `PRAGMA table_info` 反射而不是手写表清单：迁移只加表不改表，手写清单会随新迁移
/// （判定/复核/打标提案…）漂移，漏一张表就是「彻底删了但还能查到」的幽灵行。
fn asset_scoped_tables(conn: &Connection) -> Result<Vec<String>> {
    let names: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name <> 'assets'
             ORDER BY name",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut out = Vec::new();
    for n in names {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info(\"{n}\")"))?;
        let cols = stmt.query_map([], |r| r.get::<_, String>(1))?;
        let mut found = false;
        for c in cols {
            if c? == "assetId" {
                found = true;
                break;
            }
        }
        if found {
            out.push(n);
        }
    }
    Ok(out)
}

/// E2 批量打分（Lightroom 惯例）：rating 0 = 清除打分，1-5 = 星级。
/// 返回实际改动行数（幂等：同值也计入 affected 由 sqlite 语义决定，语义上按行返回）。
pub fn set_rating(conn: &Connection, asset_ids: &[i64], rating: i64) -> Result<usize> {
    if asset_ids.is_empty() {
        return Ok(0);
    }
    if !(0..=5).contains(&rating) {
        return Err(LibraryError::Invalid(format!(
            "starRating out of range 0..5: {rating}"
        )));
    }
    let mut n = 0usize;
    for chunk in asset_ids.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "UPDATE assets SET starRating = ?1 WHERE deleted = 0 AND assetId IN ({placeholders})"
        );
        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&rating];
        params.extend(chunk.iter().map(|id| id as &dyn rusqlite::ToSql));
        n += conn.execute(&sql, params.as_slice())?;
    }
    Ok(n)
}

/// E2 批量拒选/取消拒选：approvalStatus = 'rejected' / 'approved'。
/// 与 deleted 正交（拒选只是标记，资产仍在库里；deleted 才从读模型消失）。
pub fn set_rejected(conn: &Connection, asset_ids: &[i64], rejected: bool) -> Result<usize> {
    if asset_ids.is_empty() {
        return Ok(0);
    }
    let status = if rejected { "rejected" } else { "approved" };
    let mut n = 0usize;
    for chunk in asset_ids.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "UPDATE assets SET approvalStatus = ?1 WHERE deleted = 0 AND assetId IN ({placeholders})"
        );
        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&status];
        params.extend(chunk.iter().map(|id| id as &dyn rusqlite::ToSql));
        n += conn.execute(&sql, params.as_slice())?;
    }
    Ok(n)
}

/// E4 批量操作的单条结果（逐条回执：部分失败不静默）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetOpResult {
    pub asset_id: i64,
    pub ok: bool,
    /// 成功时的说明（新路径 / 已存在 / 未变动）
    pub detail: String,
    /// 失败原因（ok=false 时非空；底层原文，交给上层按形态人化）
    pub error: Option<String>,
    /// 失败分类（`read_only_fs` / `cross_device` / `permission_denied` / `not_found`）——
    /// 只在已知的 IO 归因时出现，供 API 层按运行形态（桌面 / 网页容器）改写用户文案。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// 把 `std::fs` 的底层失败归类成稳定 code（跨平台，含 Windows 错误码）。
pub fn classify_fs_error(e: &std::io::Error) -> &'static str {
    match e.kind() {
        std::io::ErrorKind::ReadOnlyFilesystem => return "read_only_fs",
        std::io::ErrorKind::CrossesDevices => return "cross_device",
        std::io::ErrorKind::PermissionDenied => return "permission_denied",
        std::io::ErrorKind::NotFound => return "not_found",
        _ => {}
    }
    match e.raw_os_error() {
        // Linux: EXDEV / EROFS / EACCES / EPERM
        Some(18) | Some(30) => "cross_device",
        Some(13) | Some(1) => "permission_denied",
        // Windows: ERROR_NOT_SAME_DEVICE / ERROR_ACCESS_DENIED / ERROR_WRITE_PROTECT
        Some(17) | Some(19) => "cross_device",
        Some(5) => "permission_denied",
        _ => "other",
    }
}

impl AssetOpResult {
    fn okf(asset_id: i64, detail: impl Into<String>) -> Self {
        Self {
            asset_id,
            ok: true,
            detail: detail.into(),
            error: None,
            code: None,
        }
    }
    fn errf(asset_id: i64, error: impl Into<String>) -> Self {
        Self {
            asset_id,
            ok: false,
            detail: String::new(),
            error: Some(error.into()),
            code: None,
        }
    }
    /// 带分类 code 的失败（上层据此产出形态相关的人化文案）。
    fn errf_code(asset_id: i64, code: &str, error: impl Into<String>) -> Self {
        Self {
            asset_id,
            ok: false,
            detail: String::new(),
            error: Some(error.into()),
            code: Some(code.to_string()),
        }
    }
}

/// E4 批量打标签（origin='manual'）：对每个 (assetId, value) 写标签值。
/// 单条失败不影响其余；返回逐条回执。
pub fn add_tags(
    conn: &Connection,
    asset_ids: &[i64],
    names: &[String],
) -> Result<Vec<AssetOpResult>> {
    let mut clean: Vec<String> = Vec::new();
    for n in names {
        let t = n.trim();
        if t.is_empty() {
            return Err(LibraryError::Invalid("标签名不能为空".into()));
        }
        if t.len() > 64 {
            return Err(LibraryError::Invalid(format!(
                "标签名过长（>64 字符）：{t}"
            )));
        }
        if !clean.iter().any(|x| x == t) {
            clean.push(t.to_string());
        }
    }
    if clean.is_empty() {
        return Err(LibraryError::Invalid("names 不能为空".into()));
    }
    let mut out = Vec::new();
    for &id in asset_ids {
        let exists = conn
            .query_row(
                "SELECT 1 FROM assets WHERE assetId=?1 AND deleted=0",
                [id],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v == 1)
            .unwrap_or(false);
        if !exists {
            out.push(AssetOpResult::errf(id, "资产不存在"));
            continue;
        }
        let mut added = 0usize;
        let mut dup = 0usize;
        let mut first_err: Option<rusqlite::Error> = None;
        for name in &clean {
            let r = conn.execute(
                "INSERT INTO tag (assetId, value, origin, score, createdAt) VALUES (?1, ?2, 'manual', 1.0, unixepoch())
         ON CONFLICT(assetId, value) DO UPDATE SET origin='manual', model=NULL, taskId=NULL
         WHERE tag.origin != 'manual'",
                rusqlite::params![id, name],
            );
            match r {
                Ok(n) if n > 0 => added += 1,
                Ok(_) => dup += 1,
                Err(e) => {
                    first_err = Some(e);
                    break;
                }
            }
        }
        match first_err {
            None => {
                let mut detail = if added > 0 {
                    format!("已加 {} 个标签", added)
                } else {
                    "标签已存在（无变化）".to_string()
                };
                if dup > 0 && added > 0 {
                    detail.push_str(&format!("，{} 个已存在", dup));
                }
                out.push(AssetOpResult::okf(id, detail));
            }
            Some(e) => out.push(AssetOpResult::errf(id, format!("sqlite: {e}"))),
        }
    }
    Ok(out)
}

/// E4 移除单个资产的一个标签（检查器内联编辑 × 按钮）。
pub fn remove_tag(conn: &Connection, asset_id: i64, value: &str) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM tag WHERE assetId=?1 AND value=?2",
        rusqlite::params![asset_id, value.trim()],
    )?)
}

/// E4 色标白名单（Lightroom 五色；None=清除）。
pub const COLOR_LABELS: [&str; 5] = ["red", "orange", "yellow", "green", "blue"];

/// E4 批量设置色标（colorLabel；None 清除）。非法色名 → Invalid（HTTP 400）。
pub fn set_color(conn: &Connection, asset_ids: &[i64], color: Option<&str>) -> Result<usize> {
    if let Some(c) = color {
        if !COLOR_LABELS.contains(&c) {
            return Err(LibraryError::Invalid(format!(
                "color 必须是 {:?} 之一或 null",
                COLOR_LABELS
            )));
        }
    }
    let mut n = 0usize;
    for chunk in asset_ids.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "UPDATE assets SET colorLabel = ?1 WHERE deleted = 0 AND assetId IN ({placeholders})"
        );
        let color_dyn: &dyn rusqlite::ToSql = &color;
        let mut params: Vec<&dyn rusqlite::ToSql> = vec![color_dyn];
        params.extend(chunk.iter().map(|id| id as &dyn rusqlite::ToSql));
        n += conn.execute(&sql, params.as_slice())?;
    }
    Ok(n)
}

/// 生成目标目录里不冲突的文件名：`name (2).ext`、`name (3).ext` …
fn free_target(target_dir: &std::path::Path, file_name: &str) -> std::path::PathBuf {
    let mut candidate = target_dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }
    let stem = std::path::Path::new(file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(file_name)
        .to_string();
    let ext = std::path::Path::new(file_name)
        .extension()
        .and_then(|s| s.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let mut n = 2;
    loop {
        candidate = target_dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

/// E4 批量移动（原地索引语义：磁盘 rename + absPath 同步更新，单条失败回滚不影响其余）。
/// 目标目录不存在则创建（拖拽目标来自目录树，通常已存在，防御性兜底）。
pub fn move_assets(
    conn: &mut Connection,
    asset_ids: &[i64],
    target_dir: &str,
) -> Result<Vec<AssetOpResult>> {
    let target = std::path::PathBuf::from(target_dir);
    if !is_abs_dir(target_dir) || target_dir.contains("..") {
        return Err(LibraryError::Invalid(format!(
            "目标目录必须是绝对路径：{target_dir}"
        )));
    }
    std::fs::create_dir_all(&target)
        .map_err(|e| LibraryError::Invalid(format!("目标目录不可创建 {target_dir}: {e}")))?;

    let mut out = Vec::new();
    for &id in asset_ids {
        let row: Option<(String, i64)> = conn
            .query_row(
                "SELECT absPath, missing FROM assets WHERE assetId=?1 AND deleted=0",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let Some((abs, missing)) = row else {
            out.push(AssetOpResult::errf(id, "资产不存在"));
            continue;
        };
        if missing == 1 {
            out.push(AssetOpResult::errf(id, "文件失联（missing=1），无法移动"));
            continue;
        }
        let Some(idx) = abs.rfind(['/', '\\']) else {
            out.push(AssetOpResult::errf(id, format!("路径异常：{abs}")));
            continue;
        };
        let (parent, file_name) = (abs[..idx].to_string(), abs[idx + 1..].to_string());
        if trim_dir_sep(&parent) == trim_dir_sep(target_dir) {
            out.push(AssetOpResult::okf(id, "已在目标目录（未变动）"));
            continue;
        }
        let from = std::path::PathBuf::from(&abs);
        if !from.is_file() {
            let _ = conn.execute("UPDATE assets SET missing=1 WHERE assetId=?1", [id]);
            out.push(AssetOpResult::errf(id, "磁盘文件不存在"));
            continue;
        }
        let to = free_target(&target, &file_name);
        if let Err(e) = std::fs::rename(&from, &to) {
            let code = classify_fs_error(&e);
            out.push(AssetOpResult::errf_code(
                id,
                code,
                format!("rename 失败：{e}"),
            ));
            continue;
        }
        // DB 更新失败 → 把文件移回去，保持盘上/库一致
        if let Err(e) = conn.execute(
            "UPDATE assets SET absPath=?1 WHERE assetId=?2 AND deleted=0",
            rusqlite::params![to.to_string_lossy(), id],
        ) {
            let _ = std::fs::rename(&to, &from);
            out.push(AssetOpResult::errf(
                id,
                format!("absPath 更新失败（已回滚）：{e}"),
            ));
            continue;
        }
        out.push(AssetOpResult::okf(id, to.to_string_lossy().to_string()));
    }
    Ok(out)
}

/// 回收站列表（deleted=1，最近软删在前）。供查重页「回收站」面板恢复用。
pub fn list_deleted(conn: &Connection, limit: u32) -> Result<Vec<AssetView>> {
    let limit = limit.clamp(1, 1000);
    let mut stmt = conn.prepare(&format!(
        "SELECT a.assetId, a.mediaType, a.absPath, a.storeMode, a.missing, a.fileName, a.fileExt,
                a.fileSz, a.mtime, a.fileHash, a.title_ch, a.title_en, a.description, a.source,
                a.parentAssetId, a.searchable, a.deleted, a.approvalStatus, a.starRating,
                a.colorLabel, a.cloudAssetId, a.importedAt,
                COALESCE(a.emb_status, 'none'), a.emb_model,
                COALESCE(a.dedup_status, 'none'), a.dedup_model,
                r.thumbPath, r.width, r.height, r.pHash,
                a.takenAt, a.cameraMake, a.cameraModel, a.lensModel, a.iso,
                a.focalLenMm, a.aperture, a.shutterText, a.orientation,
                a.aspectRatio, a.aspectClass, a.gps_lat, a.gps_lon, a.gpsSource, a.exif
         FROM assets a
         LEFT JOIN resource r ON r.assetId = a.assetId AND r.role = 'primary'
         WHERE a.deleted = 1
         ORDER BY a.importedAt DESC, a.assetId DESC
         LIMIT {limit}"
    ))?;
    let rows = stmt.query_map([], map_asset_view)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---------- W5：去重分组（fileHash 精确 ∪ pHash 近似）+ 组裁决（软删保留项之外） ----------

/// 去重组成员（web 呈现：缩略图 URL 前端按 assetId 自拼 /api/media/thumb/{id}）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DupMemberView {
    pub asset_id: i64,
    pub file_name: String,
    pub abs_path: String,
    pub file_sz: i64,
    pub imported_at: i64,
    pub file_hash: Option<String>,
    #[serde(rename = "pHash")]
    pub p_hash: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub star_rating: i64,
    /// approvalStatus == 'rejected'（拒选与去重正交，仅呈现提示）
    pub rejected: bool,
    /// KI-2b：该成员在组内的**最高视觉相似度**（dup_edges.score）；None = 只靠
    /// fileHash/pHash 进组（没有视觉判据，UI 不要编百分比）。
    #[serde(default)]
    pub similarity: Option<f32>,
}

/// 去重组：老判据（fileHash 精确 ∪ pHash 汉明 ≤8）与视觉判重边（dup_edges）**分层合并**
/// —— 并查集连通分量即组，但 kind 分开标注（组内最保守的那一层说话）：
/// variant > near_visual > near > exact（越靠前越危险：variant 不许「一键全处理」）。
/// `mixed` = 组内混了不止一层判据（UI 可标「混合」）；`layers` = 命中的判据层
/// （`phash` = fileHash/pHash，`visual` = 判重塔向量）。
/// `suggested_keep` 为预选保留项（前端可改选）；`reclaim_bytes` = 按 suggested_keep
/// 裁决后可释放的源文件字节数。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DupGroupView {
    pub group_id: i64,
    pub kind: String,
    pub size: usize,
    pub suggested_keep: i64,
    pub reclaim_bytes: i64,
    /// 组内混了不止一层判据（kind 已按最保守层呈现）
    #[serde(default)]
    pub mixed: bool,
    /// 命中的判据层：`phash`（fileHash/pHash）/ `visual`（判重塔向量）
    #[serde(default)]
    pub layers: Vec<String>,
    pub members: Vec<DupMemberView>,
}

/// dup_edges 的一行（a<b 已规范化）。
#[derive(Debug, Clone)]
pub struct DupEdgeRow {
    pub a: i64,
    pub b: i64,
    pub score: f32,
    pub kind: String,
}

/// 读全部视觉判重边（`dup_edges`，卡 KI-2b）。表缺失 = 老库还没跑过 0019 →
/// 返回空（分组退化成纯 fileHash/pHash，行为与升级前一致，不报错）。
pub fn dup_edges(conn: &Connection) -> Result<Vec<DupEdgeRow>> {
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='dup_edges'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)?;
    if !exists {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare("SELECT a, b, score, kind FROM dup_edges ORDER BY a, b")?;
    let rows = stmt.query_map([], |r| {
        Ok(DupEdgeRow {
            a: r.get(0)?,
            b: r.get(1)?,
            score: r.get(2)?,
            kind: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 判据层的「危险度」排序：数字越小越保守（决定组 kind）。
/// 未知 kind 一律按最保守处理（宁可不给一键删除，也不误删）。
fn dup_kind_rank(kind: &str) -> u8 {
    match kind {
        "variant" => 0,
        "near_visual" => 1,
        "near" => 2,
        "exact" => 3,
        _ => 0,
    }
}

/// kind → 判据层名（分组 `layers` 用）。
fn dup_kind_layer(kind: &str) -> &'static str {
    match kind {
        "exact" | "near" => "phash",
        _ => "visual",
    }
}

/// 对外的组 kind **只有四种**（前端按它分色/分动作）。写入方将来扩 kind 时，
/// 未知值一律落到 `variant` 档（最保守：不给「一键全处理」），绝不透传给前端。
fn normalize_group_kind(raw: &str) -> &'static str {
    match raw {
        "exact" => "exact",
        "near" => "near",
        "near_visual" => "near_visual",
        _ => "variant",
    }
}

/// 并查集（按大小合并 + 路径压缩）。
struct KindDsu {
    parent: Vec<usize>,
    size: Vec<usize>,
}

impl KindDsu {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            size: vec![1; n],
        }
    }
    fn find(&mut self, x: usize) -> usize {
        let mut root = x;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut cur = x;
        while self.parent[cur] != root {
            let next = self.parent[cur];
            self.parent[cur] = root;
            cur = next;
        }
        root
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        let (big, small) = if self.size[ra] >= self.size[rb] {
            (ra, rb)
        } else {
            (rb, ra)
        };
        self.parent[small] = big;
        self.size[big] += self.size[small];
    }
}

/// 全库去重分组（deleted=0）：**两套判据并查集合并**
/// 1. 老判据：fileHash 精确连边 ∪ pHash 汉明 ≤8 近似连边（`engine_dedup::build_dup_groups`）；
/// 2. 视觉判据：判重塔向量 KNN 落下的 `dup_edges`（KI-2b，near_visual / variant）。
///
/// 每个连通分量出一个组；组 kind 取组内**最保守**的一层（见 `dup_kind_rank`），
/// `mixed` 标出「混合组」，`layers` 标出命中的判据层 —— 前端据此分层展示，
/// 不把视觉近似混进「精确重复」概念里。
pub fn dup_groups(conn: &Connection) -> Result<Vec<DupGroupView>> {
    let mut stmt = conn.prepare(
        "SELECT a.assetId, a.fileName, a.absPath, a.fileSz, a.importedAt, a.fileHash,
                a.starRating, a.approvalStatus, r.width, r.height, r.pHash
         FROM assets a
         LEFT JOIN resource r ON r.assetId = a.assetId AND r.role = 'primary'
         WHERE a.deleted = 0
         ORDER BY a.assetId",
    )?;
    let rows: Vec<(engine_dedup::DupMember, DupMemberView)> = stmt
        .query_map([], |r| {
            let asset_id: i64 = r.get(0)?;
            let file_hash: Option<String> = r.get(5)?;
            let p_hash: Option<String> = r.get(10)?;
            let phash = p_hash
                .as_deref()
                .and_then(|h| u64::from_str_radix(h, 16).ok());
            Ok((
                engine_dedup::DupMember {
                    asset_id,
                    file_hash: file_hash.clone(),
                    phash,
                },
                DupMemberView {
                    asset_id,
                    file_name: r.get(1)?,
                    abs_path: r.get(2)?,
                    file_sz: r.get(3)?,
                    imported_at: r.get(4)?,
                    file_hash,
                    p_hash,
                    width: r.get(8)?,
                    height: r.get(9)?,
                    star_rating: r.get(6)?,
                    rejected: r.get::<_, String>(7)? == "rejected",
                    similarity: None,
                },
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let engine_members: Vec<engine_dedup::DupMember> =
        rows.iter().map(|(m, _)| m.clone()).collect();
    let legacy_groups = engine_dedup::build_dup_groups(&engine_members, engine_dedup::PHASH_THRESHOLD);

    // 视觉判据边：只认本次可见（未删）资产之间的边
    let idx_of: std::collections::HashMap<i64, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, (m, _))| (m.asset_id, i))
        .collect();
    let mut visual_edges: Vec<(usize, usize, f32, String)> = Vec::new();
    for e in dup_edges(conn)? {
        if e.a == e.b {
            continue;
        }
        if let (Some(&ia), Some(&ib)) = (idx_of.get(&e.a), idx_of.get(&e.b)) {
            visual_edges.push((ia, ib, e.score, e.kind));
        }
    }

    // 并查集：老判据分量 ∪ 视觉边
    let mut dsu = KindDsu::new(rows.len());
    let mut legacy_group_kinds: Vec<(i64, String)> = Vec::new();
    for g in &legacy_groups {
        if let Some(&first) = g.members.first() {
            for &m in &g.members[1..] {
                dsu.union(first, m);
            }
            legacy_group_kinds.push((
                engine_members[first].asset_id,
                g.kind.as_str().to_string(),
            ));
        }
    }
    for (ia, ib, _, _) in &visual_edges {
        dsu.union(*ia, *ib);
    }

    // 分量 → 成员下标 / 命中判据层
    let mut buckets: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    for i in 0..rows.len() {
        let root = dsu.find(i);
        buckets.entry(root).or_default().push(i);
    }
    let mut kind_set_by_root: std::collections::HashMap<usize, std::collections::BTreeSet<String>> =
        std::collections::HashMap::new();
    for (asset_id, kind) in &legacy_group_kinds {
        if let Some(root) = idx_of.get(asset_id).map(|&i| dsu.find(i)) {
            kind_set_by_root.entry(root).or_default().insert(kind.clone());
        }
    }
    for (ia, _, _, kind) in &visual_edges {
        let root = dsu.find(*ia);
        kind_set_by_root
            .entry(root)
            .or_default()
            .insert(kind.clone());
    }

    let mut out: Vec<DupGroupView> = Vec::with_capacity(buckets.len());
    for (root, idxs) in buckets {
        if idxs.len() < 2 {
            continue;
        }
        let kind_set = kind_set_by_root.remove(&root).unwrap_or_default();
        // 组 kind 取「最保守」的一层；未知 kind（未来写入方扩展）落到 variant 档，
        // 保证对外的 kind 只可能是 exact / near / near_visual / variant 四种。
        let raw_kind = kind_set
            .iter()
            .min_by_key(|k| dup_kind_rank(k))
            .cloned()
            .unwrap_or_else(|| "near".to_string());
        let kind = normalize_group_kind(&raw_kind).to_string();
        let mixed = kind_set.len() > 1;
        let mut layers: Vec<String> = kind_set
            .iter()
            .map(|k| dup_kind_layer(k).to_string())
            .collect();
        layers.sort();
        layers.dedup();

        let mut members: Vec<DupMemberView> = idxs.iter().map(|&i| rows[i].1.clone()).collect();
        // 成员的组内最高视觉相似度（视觉边才有；老判据留 None）
        let in_group: std::collections::HashMap<i64, usize> = members
            .iter()
            .enumerate()
            .map(|(i, m)| (m.asset_id, i))
            .collect();
        for (ia, ib, score, _) in &visual_edges {
            let (aid, bid) = (rows[*ia].1.asset_id, rows[*ib].1.asset_id);
            if let (Some(&ma), Some(&mb)) = (in_group.get(&aid), in_group.get(&bid)) {
                for i in [ma, mb] {
                    let cur = members[i].similarity.unwrap_or(f32::MIN);
                    members[i].similarity = Some(cur.max(*score));
                }
            }
        }

        let keep = suggest_keep(&kind, &members);
        let reclaim: i64 = members.iter().map(|m| m.file_sz).sum::<i64>()
            - members
                .iter()
                .find(|m| m.asset_id == keep)
                .map(|m| m.file_sz)
                .unwrap_or(0);
        // 呈现序：预选保留项第一，其余按 assetId 升序（人工核对视线稳定）
        members.sort_by_key(|m| (m.asset_id != keep, m.asset_id));
        let group_id = members.iter().map(|m| m.asset_id).min().unwrap_or(keep);
        out.push(DupGroupView {
            group_id,
            kind,
            size: members.len(),
            suggested_keep: keep,
            reclaim_bytes: reclaim,
            mixed,
            layers,
            members,
        });
    }
    // 大组在前（组大小相同按 groupId 升序：同一份库两次查询顺序一致）
    out.sort_by(|a, b| b.size.cmp(&a.size).then(a.group_id.cmp(&b.group_id)));
    Ok(out)
}

/// 预选保留项启发式：
/// - exact（同文件多份）：最早导入的算「原件」（importedAt 最小，次选 assetId 小）；
/// - near / near_visual / variant（近似/视觉变体）：分辨率最大优先 → 文件最大 → 最早导入。
fn suggest_keep(kind: &str, members: &[DupMemberView]) -> i64 {
    let key = |m: &DupMemberView| match kind {
        "exact" => (m.imported_at, m.asset_id, 0i64, 0i64),
        _ => (
            -(m.width.unwrap_or(0) * m.height.unwrap_or(0)),
            -m.file_sz,
            m.imported_at,
            m.asset_id,
        ),
    };
    members
        .iter()
        .min_by_key(|m| key(m))
        .map(|m| m.asset_id)
        .unwrap_or_else(|| members[0].asset_id)
}

/// 组裁决：保留 `keep_asset_id`，其余 `remove_ids` 软删进回收站（可恢复）。
/// 防呆：removeIds 非空、不含保留项、全部存在且未删（任一违规整单拒绝，不部分生效）。
pub fn dedup_resolve(conn: &Connection, keep_asset_id: i64, remove_ids: &[i64]) -> Result<usize> {
    if remove_ids.is_empty() {
        return Err(LibraryError::Invalid("removeIds 不能为空".into()));
    }
    if remove_ids.contains(&keep_asset_id) {
        return Err(LibraryError::Invalid("保留项不能同时出现在删除列表".into()));
    }
    let exists = |id: i64| -> Result<bool> {
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM assets WHERE assetId = ?1 AND deleted = 0",
            [id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    };
    if !exists(keep_asset_id)? {
        return Err(LibraryError::Invalid(format!(
            "保留项 assetId={keep_asset_id} 不存在或已删除"
        )));
    }
    for &id in remove_ids {
        if !exists(id)? {
            return Err(LibraryError::Invalid(format!(
                "删除项 assetId={id} 不存在或已删除"
            )));
        }
    }
    soft_delete_assets(conn, remove_ids)
}

#[cfg(test)]
mod w5_dedup_tests {
    use super::*;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        conn
    }

    /// 16 位 pHash hex helper（64bit 值 → 定宽 hex，与 core_hash::phash_hex 同形）。
    fn hex64(v: u64) -> String {
        format!("{v:016x}")
    }

    fn seed_asset(
        conn: &Connection,
        id: i64,
        hash: Option<&str>,
        phash: Option<u64>,
        w: i64,
        h: i64,
        sz: i64,
        imported_at: i64,
    ) {
        conn.execute(
            "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime,
                                 fileHash, importedAt)
             VALUES (?1, ?2, 0, ?3, 'jpg', ?4, ?5, ?6, ?7)",
            rusqlite::params![
                id,
                format!("/a/{id}.jpg"),
                format!("f{id}.jpg"),
                sz,
                imported_at,
                hash,
                imported_at
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO resource (assetId, role, ordinal, width, height, pHash)
             VALUES (?1, 'primary', 0, ?2, ?3, ?4)",
            rusqlite::params![id, w, h, phash.map(hex64)],
        )
        .unwrap();
    }

    #[test]
    fn dup_groups_exact_and_near() {
        let conn = setup();
        // 组1：同 fileHash（pHash 同为 0）→ exact
        seed_asset(&conn, 1, Some("h1"), Some(0), 800, 600, 100, 10);
        seed_asset(&conn, 2, Some("h1"), Some(0), 800, 600, 100, 20);
        // 孤儿：pHash 全 1（与 0 距 64，不连任何边）
        seed_asset(&conn, 3, Some("h3"), Some(u64::MAX), 640, 480, 200, 30);
        // 组2：与 3 只差 1 bit（不同 hash）→ near
        seed_asset(&conn, 4, Some("h4"), Some(u64::MAX ^ 1), 640, 480, 200, 40);

        let gs = dup_groups(&conn).unwrap();
        assert_eq!(gs.len(), 2);
        // 大组在前？两都 2 人：按最小 assetId 稳定 → 组1 (1,2) 在前
        assert_eq!(gs[0].kind, "exact");
        assert_eq!(gs[0].suggested_keep, 1, "exact 组预选最早导入");
        assert_eq!(gs[0].members[0].asset_id, 1, "预选保留项排第一");
        assert_eq!(gs[0].reclaim_bytes, 100);
        assert_eq!(gs[1].kind, "near");
        assert_eq!(gs[1].members.len(), 2);
    }

    #[test]
    fn near_keep_prefers_largest_resolution() {
        let conn = setup();
        // 小图先导入；大图后导入 → near 组预选大图（分辨率优先于时间）
        seed_asset(&conn, 1, Some("h1"), Some(0), 640, 480, 500, 10);
        seed_asset(&conn, 2, Some("h2"), Some(0), 1920, 1080, 400, 20);
        let gs = dup_groups(&conn).unwrap();
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0].kind, "near");
        assert_eq!(gs[0].suggested_keep, 2);
        assert_eq!(gs[0].reclaim_bytes, 500);
    }

    #[test]
    fn deleted_members_leave_groups() {
        let conn = setup();
        seed_asset(&conn, 1, Some("h1"), Some(0), 800, 600, 100, 10);
        seed_asset(&conn, 2, Some("h1"), Some(0), 800, 600, 100, 20);
        assert_eq!(dup_groups(&conn).unwrap().len(), 1);
        soft_delete_assets(&conn, &[2]).unwrap();
        assert!(dup_groups(&conn).unwrap().is_empty());
        restore_assets(&conn, &[2]).unwrap();
        assert_eq!(dup_groups(&conn).unwrap().len(), 1);
    }

    #[test]
    fn resolve_validates_then_soft_deletes() {
        let conn = setup();
        seed_asset(&conn, 1, Some("h1"), Some(0), 800, 600, 100, 10);
        seed_asset(&conn, 2, Some("h1"), Some(0), 800, 600, 100, 20);
        seed_asset(&conn, 3, Some("h9"), Some(0), 800, 600, 100, 30);

        // 空删除列表
        assert!(matches!(
            dedup_resolve(&conn, 1, &[]),
            Err(LibraryError::Invalid(_))
        ));
        // 保留项混入删除列表
        assert!(matches!(
            dedup_resolve(&conn, 1, &[1, 2]),
            Err(LibraryError::Invalid(_))
        ));
        // 不存在的资产
        assert!(matches!(
            dedup_resolve(&conn, 1, &[2, 99]),
            Err(LibraryError::Invalid(_))
        ));
        // 以上任何一步都不得部分生效
        assert_eq!(soft_deleted_count(&conn), 0);

        // 合法裁决：保留 2（改选），删 1、3
        let n = dedup_resolve(&conn, 2, &[1, 3]).unwrap();
        assert_eq!(n, 2);
        assert_eq!(soft_deleted_count(&conn), 2);
        let remaining: Vec<DupGroupView> = dup_groups(&conn).unwrap();
        assert!(remaining.is_empty(), "裁决后组应消失");
        // 回收站可恢复
        let trash = list_deleted(&conn, 100).unwrap();
        assert_eq!(trash.len(), 2);
        restore_assets(&conn, &[1, 3]).unwrap();
        assert_eq!(dup_groups(&conn).unwrap().len(), 1);
    }

    fn soft_deleted_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM assets WHERE deleted = 1", [], |r| {
            r.get(0)
        })
        .unwrap()
    }
}

#[cfg(test)]
mod fs_error_classify_tests {
    use super::classify_fs_error;
    use std::io::Error;

    #[test]
    fn classifies_known_codes_and_falls_back() {
        // Linux 原生码：EXDEV=18，EACCES=13，ENOENT=2（30=EROFS 归 read_only_fs 或 cross_device，随平台 kind 映射）
        assert_eq!(classify_fs_error(&Error::from_raw_os_error(18)), "cross_device");
        assert_eq!(classify_fs_error(&Error::from_raw_os_error(13)), "permission_denied");
        assert_eq!(classify_fs_error(&Error::from_raw_os_error(2)), "not_found");
        let e30 = classify_fs_error(&Error::from_raw_os_error(30));
        assert!(e30 == "read_only_fs" || e30 == "cross_device", "EROFS 归类异常：{e30}");
        // Windows 码（在非 Windows 平台走 raw 兜底）：17=NOT_SAME_DEVICE，5=ACCESS_DENIED
        assert_eq!(classify_fs_error(&Error::from_raw_os_error(17)), "cross_device");
        assert_eq!(classify_fs_error(&Error::from_raw_os_error(5)), "permission_denied");
        // 无法归因 → other（上层原样透出）
        assert_eq!(classify_fs_error(&Error::other("boom")), "other");
    }

    #[test]
    fn asset_op_result_serializes_code_only_when_present() {
        let ok = super::AssetOpResult::okf(1, "/a/b.jpg");
        assert!(serde_json::to_string(&ok).unwrap().find("code").is_none());
        let bad = super::AssetOpResult::errf_code(1, "cross_device", "rename 失败");
        let j = serde_json::to_string(&bad).unwrap();
        assert!(j.contains("\"code\":\"cross_device\""), "{j}");
    }
}

// ---------- TEXT-EMBED-1：文搜图相关性下限 ----------

#[cfg(all(test, feature = "embed"))]
mod text_min_score_tests {
    use super::*;
    use engine_embed::EmbedProvider;
    use std::path::PathBuf;

    /// 固定查询向量的 mock provider（embed_texts 恒返回 qvec）。
    struct FixedTextProvider {
        spec: core_config::ModelSpec,
        qvec: Vec<f32>,
    }

    impl EmbedProvider for FixedTextProvider {
        fn load(&mut self) -> engine_embed::Result<()> {
            Ok(())
        }
        fn embed_images(&self, _paths: &[PathBuf]) -> engine_embed::Result<Vec<Vec<f32>>> {
            Err(engine_embed::EmbedError::Unsupported("mock: images".into()))
        }
        fn embed_texts(&self, texts: &[&str]) -> engine_embed::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| self.qvec.clone()).collect())
        }
        fn ready(&self) -> bool {
            true
        }
        fn spec(&self) -> &core_config::ModelSpec {
            &self.spec
        }
    }

    fn mock_spec() -> core_config::ModelSpec {
        serde_json::from_value(serde_json::json!({
            "name": "mock-embed-model",
            "kind": "embedding",
            "dims": 4,
            "modalities": ["image", "text"],
            "normalized": true,
            "provider": "mock"
        }))
        .expect("mock spec")
    }

    /// 3 条资源向量：r1 与查询同向 cos=1.0；r3 cos=0.6；r2 正交 cos=0.0。
    fn open_db() -> Connection {
        // sqlite-vec auto_extension 必须在 Connection 打开前注册（见 vecstore 模块注释）
        core_storage::vecstore::init_vec_ext();
        let conn = Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        conn
    }

    fn seed_vectors(conn: &Connection) -> Vec<f32> {
        let qvec = vec![1.0, 0.0, 0.0, 0.0];
        conn.execute_batch(
            "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt) VALUES
                (1, '/a/v1.jpg', 0, 'v1.jpg', 'jpg', 1, 1, 1),
                (2, '/a/v2.jpg', 0, 'v2.jpg', 'jpg', 1, 2, 2),
                (3, '/a/v3.jpg', 0, 'v3.jpg', 'jpg', 1, 3, 3);
             INSERT INTO resource (assetId, role, thumbPath, width, height) VALUES
                (1, 'primary', '/t/1.webp', 10, 10),
                (2, 'primary', '/t/2.webp', 10, 10),
                (3, 'primary', '/t/3.webp', 10, 10);",
        )
        .unwrap();
        let model = "mock-embed-model";
        core_storage::vecstore::ensure_table(conn, model, 4).unwrap();
        core_storage::vecstore::upsert(
            conn,
            model,
            &[(1, &qvec), (2, &[0.0, 1.0, 0.0, 0.0]), (3, &[0.6, 0.8, 0.0, 0.0])],
        )
        .unwrap();
        qvec
    }

    #[test]
    fn default_min_score_filters_noise_but_keeps_real_matches() {
        let conn = open_db();
        let qvec = seed_vectors(&conn);
        let p = FixedTextProvider { spec: mock_spec(), qvec };
        let hits = search_by_text(
            &conn,
            &p,
            "狗",
            10,
            &SearchFilter::default(),
            &TextSearchOptions::default(),
        )
        .unwrap();
        // 正交噪声（cos=0.0）被 0.08 下限滤掉；同向 1.0 与 0.6 保留
        let scores: Vec<f32> = hits.iter().map(|h| h.score).collect();
        assert_eq!(hits.len(), 2, "{scores:?}");
        assert!((scores[0] - 1.0).abs() < 1e-4);
        assert!((scores[1] - 0.6).abs() < 1e-4);
    }

    #[test]
    fn min_score_zero_disables_filter() {
        let conn = open_db();
        let qvec = seed_vectors(&conn);
        let p = FixedTextProvider { spec: mock_spec(), qvec };
        let hits = search_by_text(
            &conn,
            &p,
            "狗",
            10,
            &SearchFilter::default(),
            &TextSearchOptions { min_score: Some(0.0) },
        )
        .unwrap();
        assert_eq!(hits.len(), 3, "关闭阈值后正交向量也应返回");
    }

    #[test]
    fn high_min_score_yields_explicit_empty() {
        let conn = open_db();
        let qvec = seed_vectors(&conn);
        let p = FixedTextProvider { spec: mock_spec(), qvec };
        let hits = search_by_text(
            &conn,
            &p,
            "狗",
            10,
            &SearchFilter::default(),
            &TextSearchOptions { min_score: Some(0.9) },
        )
        .unwrap();
        // 只剩同向那条（cos=1.0 ≥ 0.9）；0.6 与 0.0 被滤掉
        assert_eq!(hits.len(), 1);
        assert!((hits[0].score - 1.0).abs() < 1e-4);
        // 阈值高到不可能 → 显式空（UI 呈现「没有命中」）
        let none = search_by_text(
            &conn,
            &p,
            "狗",
            10,
            &SearchFilter::default(),
            &TextSearchOptions { min_score: Some(1.5) },
        )
        .unwrap();
        assert!(none.is_empty());
    }
}

// ---------- W3：以图搜图相关性下限（按模型标定）----------

/// W3 标定值自检 + 过滤行为回归（卡 t_d00827d5）。
#[cfg(all(test, feature = "embed"))]
mod similar_min_score_tests {
    use super::*;
    use engine_embed::EmbedProvider;
    use std::path::PathBuf;

    /// 固定查询向量的 mock provider（embed_images 恒返回 qvec）。
    struct FixedImageProvider {
        spec: core_config::ModelSpec,
        qvec: Vec<f32>,
    }

    impl EmbedProvider for FixedImageProvider {
        fn load(&mut self) -> engine_embed::Result<()> {
            Ok(())
        }
        fn embed_images(&self, _paths: &[PathBuf]) -> engine_embed::Result<Vec<Vec<f32>>> {
            Ok(vec![self.qvec.clone()])
        }
        fn embed_texts(&self, _texts: &[&str]) -> engine_embed::Result<Vec<Vec<f32>>> {
            Err(engine_embed::EmbedError::Unsupported("mock: texts".into()))
        }
        fn ready(&self) -> bool {
            true
        }
        fn spec(&self) -> &core_config::ModelSpec {
            &self.spec
        }
    }

    fn spec_named(name: &str) -> core_config::ModelSpec {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "kind": "embedding",
            "dims": 4,
            "modalities": ["image", "text"],
            "normalized": true,
            "provider": "mock"
        }))
        .expect("mock spec")
    }

    /// 3 条向量：与查询同向 cos=1.0 / cos=0.6 / 正交 cos=0.0。
    fn seed(conn: &Connection, model: &str) -> Vec<f32> {
        core_storage::vecstore::init_vec_ext();
        conn.execute_batch(
            "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt) VALUES
                (1, '/a/v1.jpg', 0, 'v1.jpg', 'jpg', 1, 1, 1),
                (2, '/a/v2.jpg', 0, 'v2.jpg', 'jpg', 1, 2, 2),
                (3, '/a/v3.jpg', 0, 'v3.jpg', 'jpg', 1, 3, 3);
             INSERT INTO resource (assetId, role, thumbPath, width, height) VALUES
                (1, 'primary', '/t/1.webp', 10, 10),
                (2, 'primary', '/t/2.webp', 10, 10),
                (3, 'primary', '/t/3.webp', 10, 10);",
        )
        .unwrap();
        let qvec = vec![1.0, 0.0, 0.0, 0.0];
        core_storage::vecstore::ensure_table(conn, model, 4).unwrap();
        core_storage::vecstore::upsert(
            conn,
            model,
            &[(1, &qvec), (2, &[0.0, 1.0, 0.0, 0.0]), (3, &[0.6, 0.8, 0.0, 0.0])],
        )
        .unwrap();
        qvec
    }

    fn open_db() -> Connection {
        core_storage::vecstore::init_vec_ext();
        let conn = Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        conn
    }

    /// is_file() 前置检查需要一个真实文件（内容不参与 mock 推理）。
    fn query_file(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("dam-w3-min-{tag}-{}.jpg", std::process::id()));
        std::fs::write(&p, b"not-an-image").unwrap();
        p
    }

    #[test]
    fn calibrated_defaults_are_the_documented_numbers() {
        assert!((default_similar_min_score("siglip2-base-p224-onnx") - 0.72).abs() < 1e-6);
        // KI-3：dinov3 的 0.20 分支已删（去重塔永不当活跃语义塔 → 该下限不可达）
        assert_eq!(default_similar_min_score("dinov3-vits16-onnx"), 0.0);
        assert!(!similar_min_score_calibrated("dinov3-vits16-onnx"));
        // 未标定模型不猜数字：不过滤（保持旧行为）
        assert_eq!(default_similar_min_score("openai-compat-embedding"), 0.0);
        assert!(similar_min_score_calibrated("siglip2-base-p224-onnx"));
        assert!(!similar_min_score_calibrated("openai-compat-embedding"));
    }

    /// t_835d7366：路由键（`spec:endpoint-model`）与裸 spec 名必须拿到同一个下限 ——
    /// 远程 provider 的向量路由键带 `:` 后缀，`split(':')` 归一到 spec 名再查表。
    #[test]
    fn router_key_prefix_keeps_calibrated_min_score() {
        let suffixed = "siglip2-base-p224-onnx:qwen3-vl-embedding-2b";
        assert!((default_similar_min_score(suffixed) - 0.72).abs() < 1e-6);
        assert!(similar_min_score_calibrated(suffixed));
        // KI-3：去重塔已从标定表删除 → 带后缀写法同样是「未标定」
        assert_eq!(default_similar_min_score("dinov3-vits16-onnx:whatever"), 0.0);
        // 远程未标定模型的后缀写法仍然不猜数字
        assert_eq!(
            default_similar_min_score("openai-compat-embedding:qwen3-vl-embedding-2b"),
            0.0
        );
        // 空串/纯后缀不 panic
        assert_eq!(default_similar_min_score(""), 0.0);
        assert_eq!(default_similar_min_score(":abc"), 0.0);
    }

    #[test]
    fn effective_clamps_to_unit_range() {
        let o = |v: Option<f32>| SimilarOptions { min_score: v }.effective("siglip2-base-p224-onnx");
        assert!((o(None) - 0.72).abs() < 1e-6, "None → 标定值");
        assert_eq!(o(Some(0.0)), 0.0, "显式 0 = 关闭");
        assert_eq!(o(Some(-1.0)), 0.0, "负数钳到 0");
        assert_eq!(o(Some(1.5)), 1.0, "超 1 钳到 1");
        // 未标定模型缺省 = 0（不过滤）
        assert_eq!(
            SimilarOptions::default().effective("openai-compat-embedding"),
            0.0
        );
    }

    #[test]
    fn uncalibrated_model_keeps_old_behaviour() {
        let conn = open_db();
        let qvec = seed(&conn, "mock-embed-model");
        let p = FixedImageProvider {
            spec: spec_named("mock-embed-model"),
            qvec,
        };
        let q = query_file("uncal");
        let hits = search_by_image_path(
            &conn,
            &p,
            &q,
            10,
            &SearchFilter::default(),
            &SimilarOptions::default(),
        )
        .unwrap();
        assert_eq!(hits.len(), 3, "未标定模型默认不过滤（含正交噪声）");
        let _ = std::fs::remove_file(q);
    }

    #[test]
    fn search_by_image_path_applies_k_and_scalar_filter() {
        // seed: 3 张（v1/v2/v3 各 cos 1.0 / 0.6 / 0.0，fileExt 全 jpg）
        // 服务层这层必须把 k 与 SearchFilter 真交给 engine_search（AUDIT-B2 §1 的根因就在上层丢了它们）
        let conn = open_db();
        let qvec = seed(&conn, "mock-embed-model");
        let p = FixedImageProvider {
            spec: spec_named("mock-embed-model"),
            qvec,
        };
        let q = query_file("kfilter");
        // k=1 → 只回 1 条
        let one = search_by_image_path(
            &conn,
            &p,
            &q,
            1,
            &SearchFilter::default(),
            &SimilarOptions::default(),
        )
        .unwrap();
        assert_eq!(one.len(), 1, "k=1 只回 1 条");
        // ext 不匹配 → 0 条；匹配 → 3 条
        let miss = search_by_image_path(
            &conn,
            &p,
            &q,
            10,
            &SearchFilter {
                ext: Some("png".into()),
                ..Default::default()
            },
            &SimilarOptions::default(),
        )
        .unwrap();
        assert!(miss.is_empty(), "ext=png 不该命中 jpg：{miss:?}");
        let hit = search_by_image_path(
            &conn,
            &p,
            &q,
            10,
            &SearchFilter {
                ext: Some("jpg".into()),
                ..Default::default()
            },
            &SimilarOptions::default(),
        )
        .unwrap();
        assert_eq!(hit.len(), 3, "ext=jpg 应全中");
        // tag 过滤（seed 无标签）→ 0 条
        let by_tag = search_by_image_path(
            &conn,
            &p,
            &q,
            10,
            &SearchFilter {
                tag: Some("no-such-tag".into()),
                ..Default::default()
            },
            &SimilarOptions::default(),
        )
        .unwrap();
        assert!(by_tag.is_empty(), "tag 不匹配应 0 条：{by_tag:?}");
        let _ = std::fs::remove_file(q);
    }

    #[test]
    fn calibrated_model_default_drops_low_scores() {
        // 用真实标定名 → 默认下限生效（siglip2 0.72）
        for (model, expect) in [("siglip2-base-p224-onnx", 1usize)] {
            let conn = open_db();
            let qvec = seed(&conn, model);
            let p = FixedImageProvider {
                spec: spec_named(model),
                qvec,
            };
            let q = query_file(model);
            let hits = search_by_image_path(
                &conn,
                &p,
                &q,
                10,
                &SearchFilter::default(),
                &SimilarOptions::default(),
            )
            .unwrap();
            assert_eq!(hits.len(), expect, "{model} 默认下限过滤后条数");
            assert!(hits.iter().all(|h| h.score >= default_similar_min_score(model) - 1e-6));
            let _ = std::fs::remove_file(q);
        }
    }

    /// KI-3：去重塔不再有标定下限（`dinov3 → 0.20` 分支已删，它永不当活跃语义塔）——
    /// 即使被当模型名传进来，也只能走「未标定 = 不过滤」，不会凭空砍结果。
    #[test]
    fn dinov3_is_uncalibrated_so_default_does_not_filter() {
        let model = core_config::DEDUP_EMBEDDER;
        assert_eq!(default_similar_min_score(model), 0.0);
        assert!(!similar_min_score_calibrated(model));
        let conn = open_db();
        let qvec = seed(&conn, model);
        let p = FixedImageProvider {
            spec: spec_named(model),
            qvec,
        };
        let q = query_file("dinov3-uncalibrated");
        let hits = search_by_image_path(
            &conn,
            &p,
            &q,
            10,
            &SearchFilter::default(),
            &SimilarOptions::default(),
        )
        .unwrap();
        assert_eq!(hits.len(), 3, "未标定 → 不过滤，seed 的三条全回：{hits:?}");
        let _ = std::fs::remove_file(q);
    }

    #[test]
    fn explicit_min_score_zero_disables_and_high_yields_empty() {
        let conn = open_db();
        let qvec = seed(&conn, "mock-embed-model");
        let p = FixedImageProvider {
            spec: spec_named("mock-embed-model"),
            qvec,
        };
        let q = query_file("explicit");
        let off = search_by_image_path(
            &conn,
            &p,
            &q,
            10,
            &SearchFilter::default(),
            &SimilarOptions { min_score: Some(0.0) },
        )
        .unwrap();
        assert_eq!(off.len(), 3, "minScore=0 关闭过滤");
        let mid = search_by_image_path(
            &conn,
            &p,
            &q,
            10,
            &SearchFilter::default(),
            &SimilarOptions { min_score: Some(0.5) },
        )
        .unwrap();
        assert_eq!(mid.len(), 2, "0.5 下限保留 1.0 与 0.6");
        let high = search_by_image_path(
            &conn,
            &p,
            &q,
            10,
            &SearchFilter::default(),
            &SimilarOptions { min_score: Some(0.99) },
        )
        .unwrap();
        assert_eq!(high.len(), 1, "0.99 只留同向 1.0");
        let none = search_by_image_path(
            &conn,
            &p,
            &q,
            10,
            &SearchFilter::default(),
            &SimilarOptions { min_score: Some(1.0) },
        )
        .unwrap();
        assert!(none.iter().all(|h| h.score >= 1.0), "1.0 只可能剩同向");
        let _ = std::fs::remove_file(q);
    }
}

// ---------- D3：混合检索（字面证据融合）----------

#[cfg(test)]
mod literal_fusion_tests {
    use super::*;

    fn open_db() -> Connection {
        core_storage::vecstore::init_vec_ext();
        let conn = Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        conn
    }

    fn seed_literal(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO assets (assetId, absPath, fileName, fileExt, fileSz, mtime, importedAt, title_ch, ocr_text)
             VALUES
                (1, '/a/k06-skateboards.jpg', 'k06-skateboards.jpg', 'jpg', 1, 1, 1, NULL, ''),
                (2, '/a/002.jpg', '002.jpg', 'jpg', 1, 2, 2, '海边日落', ''),
                (3, '/a/003.jpg', '003.jpg', 'jpg', 1, 3, 3, NULL, '数字资产管理 发票'),
                (4, '/a/004.jpg', '004.jpg', 'jpg', 1, 4, 4, NULL, '');
             INSERT INTO resource (assetId, role, thumbPath, width, height) VALUES
                (1, 'primary', '/t/1.webp', 10, 10),
                (2, 'primary', '/t/2.webp', 10, 10),
                (3, 'primary', '/t/3.webp', 10, 10),
                (4, 'primary', '/t/4.webp', 10, 10);
             INSERT INTO tag (assetId, value, origin, createdAt) VALUES (4, '滑板', 'auto', 1);",
        )
        .unwrap();
        // 合流（t_d99ca3fe）：OCR 通道走 FTS5 倒排（OCR-MATCH），索引行由生产路径
        // `svc-pipeline::ocr_one` 在 ocr_text 落库后建；这里按同一口径补建，否则只有
        // ocr_text 列、没有索引行的库查不到（这正是 FTS5 改造要替换掉的全表扫口径）。
        core_storage::ocr_index::upsert(conn, 3, "数字资产管理 发票").unwrap();
    }

    #[test]
    fn literal_fusion_recalls_by_filename_title_tag_and_ocr() {
        let conn = open_db();
        seed_literal(&conn);
        // 文件名（英文、大小写不敏感）
        let mut hits = vec![];
        fuse_literal_text(&conn, &mut hits, "skateboards", 10).unwrap();
        assert_eq!(hits.len(), 1, "{:?}", hits.iter().map(|h| h.assetId).collect::<Vec<_>>());
        assert_eq!(hits[0].assetId, 1);
        assert!((hits[0].score - LITERAL_FUSION_SCORE).abs() < 1e-6);

        // 中文标题
        let mut hits = vec![];
        fuse_literal_text(&conn, &mut hits, "海边日落", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].assetId, 2);

        // 标签通道（中文标签）
        let mut hits = vec![];
        fuse_literal_text(&conn, &mut hits, "滑板", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].assetId, 4);

        // OCR 通道（分数低于字面通道）
        let mut hits = vec![];
        fuse_literal_text(&conn, &mut hits, "发票", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].assetId, 3);
        assert!((hits[0].score - OCR_FUSION_SCORE).abs() < 1e-6);
    }

    #[test]
    fn literal_fusion_boosts_existing_hit_only_upward() {
        let conn = open_db();
        seed_literal(&conn);
        let mk = |asset_id: i64, score: f32| engine_search::SearchHit {
            assetId: asset_id,
            resourceId: asset_id,
            score,
            fileName: "x.jpg".into(),
            titleCh: None,
            thumbPath: None,
            width: None,
            height: None,
            ocrMatched: false,
            ocrSnippet: None,
        };
        // 弱语义命中（低于下限本会被滤掉）→ 字面命中把它抬到融合分
        let mut hits = vec![mk(1, 0.02)];
        fuse_literal_text(&conn, &mut hits, "skateboards", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!((hits[0].score - LITERAL_FUSION_SCORE).abs() < 1e-6);
        // 强语义命中（高于融合分）保持原分
        let mut hits = vec![mk(1, 0.9)];
        fuse_literal_text(&conn, &mut hits, "skateboards", 10).unwrap();
        assert!((hits[0].score - 0.9).abs() < 1e-6);
    }

    #[test]
    fn literal_fusion_no_token_noop() {
        let conn = open_db();
        seed_literal(&conn);
        let mut hits = vec![];
        // 单字符 token 被过滤（<2 字符）→ 不命中、不报错
        fuse_literal_text(&conn, &mut hits, "a", 10).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn default_min_score_matches_calibration() {
        // 0.085 = 2026-10-17 重标定（D3）；被改动必须同步 reports/D3-text-calibration.md
        assert!((DEFAULT_TEXT_MIN_SCORE - 0.085).abs() < 1e-6);
    }
}
// ---------- KI-5：精准匹配（语义塔扩池 → 判重塔 rerank → 0.90/0.98 分带）----------

/// 精准模式的四条硬口径：扩池（不沿用语义阈值）、按判重塔排序、分带截断、自身剔除；
/// 外加「缺判重向量必须可见」的三种形态（候选缺 / 查询图缺 / 整表没建）。
#[cfg(all(test, feature = "embed"))]
mod precise_mode_tests {
    use super::*;
    use engine_embed::EmbedProvider;
    use std::path::PathBuf;

    const SEMANTIC_MODEL: &str = "mock-embed-model";
    /// 判重塔名恒为 DEDUP_EMBEDDER（接口不暴露模型名）。
    const DEDUP: &str = core_config::DEDUP_EMBEDDER;

    struct FixedImageProvider {
        spec: core_config::ModelSpec,
        qvec: Vec<f32>,
    }

    impl EmbedProvider for FixedImageProvider {
        fn load(&mut self) -> engine_embed::Result<()> {
            Ok(())
        }
        fn embed_images(&self, _paths: &[PathBuf]) -> engine_embed::Result<Vec<Vec<f32>>> {
            Ok(vec![self.qvec.clone()])
        }
        fn embed_texts(&self, _texts: &[&str]) -> engine_embed::Result<Vec<Vec<f32>>> {
            Err(engine_embed::EmbedError::Unsupported("mock: texts".into()))
        }
        fn ready(&self) -> bool {
            true
        }
        fn spec(&self) -> &core_config::ModelSpec {
            &self.spec
        }
    }

    fn spec_named(name: &str) -> core_config::ModelSpec {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "kind": "embedding",
            "dims": 4,
            "modalities": ["image"],
            "normalized": true,
            "provider": "mock"
        }))
        .expect("mock spec")
    }

    fn open_db() -> Connection {
        core_storage::vecstore::init_vec_ext();
        let conn = Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        conn
    }

    /// is_file() 前置检查要真实文件（内容不参与 mock 推理）；DB 里的 absPath 必须指向它。
    fn query_file(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("dam-ki5-precise-{tag}-{}.jpg", std::process::id()));
        std::fs::write(&p, b"not-an-image").unwrap();
        p
    }

    fn provider() -> FixedImageProvider {
        FixedImageProvider {
            spec: spec_named(SEMANTIC_MODEL),
            qvec: vec![1.0, 0.0, 0.0, 0.0],
        }
    }

    /// 5 张图（语义塔 cos：自身 1.0 / 0.71 / 0.50 / 0.60 / 0.70）：
    /// - 语义阈值 0.72 下**只剩自身**（a3/a4/a5 全被砍）→ 证明精准模式必须扩池；
    /// - 判重塔 cos：自身 1.0 / a2 0.60（应被 0.90 截掉）/ a3 1.00（near）/ a4 0.95（variant）/ a5 无向量。
    fn seed(conn: &Connection, qpath: &std::path::Path, with_dedup: bool) -> Vec<i64> {
        let abs = qpath.to_string_lossy().to_string();
        conn.execute_batch(&format!(
            "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt) VALUES
                (1, '{abs}', 0, 'a1.jpg', 'jpg', 1, 1, 1),
                (2, '{abs}', 0, 'a2.jpg', 'jpg', 1, 2, 2),
                (3, '{abs}', 0, 'a3.jpg', 'jpg', 1, 3, 3),
                (4, '{abs}', 0, 'a4.jpg', 'jpg', 1, 4, 4),
                (5, '{abs}', 0, 'a5.jpg', 'jpg', 1, 5, 5);
             INSERT INTO resource (assetId, role, thumbPath, width, height) VALUES
                (1, 'primary', '/t/1.webp', 10, 10),
                (2, 'primary', '/t/2.webp', 10, 10),
                (3, 'primary', '/t/3.webp', 10, 10),
                (4, 'primary', '/t/4.webp', 10, 10),
                (5, 'primary', '/t/5.webp', 10, 10);"
        ))
        .unwrap();
        core_storage::vecstore::ensure_table(conn, SEMANTIC_MODEL, 4).unwrap();
        core_storage::vecstore::upsert(
            conn,
            SEMANTIC_MODEL,
            &[
                (1, &[1.0, 0.0, 0.0, 0.0]),
                (2, &[0.71, 0.704_203_2, 0.0, 0.0]),
                (3, &[0.5, 0.866_025_4, 0.0, 0.0]),
                (4, &[0.6, 0.8, 0.0, 0.0]),
                (5, &[0.7, 0.714_142_8, 0.0, 0.0]),
            ],
        )
        .unwrap();
        if with_dedup {
            core_storage::vecstore::ensure_table(conn, DEDUP, 4).unwrap();
            core_storage::vecstore::upsert(
                conn,
                DEDUP,
                &[
                    (1, &[0.0, 0.0, 1.0, 0.0]),
                    (2, &[0.0, 0.0, 0.6, 0.8]),
                    (3, &[0.0, 0.0, 1.0, 0.0]),
                    (4, &[0.0, 0.0, 0.95, 0.312_249_9]),
                ],
            )
            .unwrap();
        }
        // assetId → resourceId（vecstore 按 resourceId 存）
        (1..=5)
            .map(|aid| primary_resource_id(conn, aid).expect("primary resource"))
            .collect()
    }

    #[test]
    fn similar_mode_serde_contract() {
        // 接口字面量：前端只认这两个值（'precise' 之类的别名一律不接）
        let m: SimilarMode = serde_json::from_str("\"visual_precise\"").unwrap();
        assert_eq!(m, SimilarMode::VisualPrecise);
        let m: SimilarMode = serde_json::from_str("\"semantic\"").unwrap();
        assert_eq!(m, SimilarMode::Semantic);
        assert!(serde_json::from_str::<SimilarMode>("\"precise\"").is_err());
        // 缺省 = 语义相似（零回归的承诺）
        assert_eq!(SimilarMode::default(), SimilarMode::Semantic);
        assert_eq!(SimilarMode::default().as_str(), "semantic");
        assert_eq!(SimilarMode::VisualPrecise.as_str(), "visual_precise");
        // 分带阈值与查重页同一套数字，不许各写一份
        assert!((PRECISE_NEAR_THRESHOLD - 0.98).abs() < 1e-6);
        assert!((PRECISE_VARIANT_THRESHOLD - 0.90).abs() < 1e-6);
    }

    #[test]
    fn semantic_mode_still_drops_below_calibrated_threshold() {
        // 对照组：语义模式套 0.72（siglip2 标定值）→ 只剩自身，剔除自身后 0 条。
        // 精准模式命中的 a3/a4 在这里全都看不见 —— 这就是「必须扩池」的证据。
        let q = query_file("semantic");
        let conn = open_db();
        let rids = seed(&conn, &q, true);
        assert_eq!(rids.len(), 5);
        let p = provider();
        let hits = search_by_image_path(
            &conn,
            &p,
            &q,
            10,
            &SearchFilter::default(),
            &SimilarOptions {
                min_score: Some(0.72),
            },
        )
        .unwrap();
        assert_eq!(hits.len(), 1, "0.72 下只有自身：{hits:?}");
        assert_eq!(hits[0].assetId, 1);
        let _ = std::fs::remove_file(q);
    }

    #[test]
    fn precise_reranks_by_dedup_tower_and_bands() {
        let q = query_file("rerank");
        let conn = open_db();
        seed(&conn, &q, true);
        let p = provider();
        let res = find_similar_precise(&conn, &p, 1, 10, &SearchFilter::default()).unwrap();

        assert!(res.dedup_index_ready);
        assert!(res.self_excluded);
        assert!(!res.query_missing_dedup_vector);
        // 候选池 = 自身之外的语义全部（扩池、不看 0.72）
        assert_eq!(res.candidate_pool, 4, "候选池不该被语义阈值砍：{res:?}");
        // a5 没有判重向量 → 必须计数，不许静默消失
        assert_eq!(res.missing_dedup_vectors, 1);
        // 命中 = a3(1.00 near) + a4(0.95 variant)；a2 的 0.60 被 0.90 截掉；自身被剔除
        let ids: Vec<i64> = res.hits.iter().map(|h| h.asset_id).collect();
        assert_eq!(ids, vec![3, 4], "按判重塔 cos 降序：{res:?}");
        assert_eq!(res.hits[0].band, "near_duplicate");
        assert!((res.hits[0].score - 1.0).abs() < 1e-5);
        assert_eq!(res.hits[1].band, "variant");
        assert!((res.hits[1].score - 0.95).abs() < 1e-4);
        // 语义分随行回带（诊断「它凭什么叫候选」）：a3 语义只有 0.50（语义模式里根本看不见）
        assert!((res.hits[0].semantic_score - 0.50).abs() < 1e-4);
        assert!(res.hits.iter().all(|h| h.asset_id != 1), "自身必须剔除");
        let _ = std::fs::remove_file(q);
    }

    #[test]
    fn precise_respects_k_and_scalar_filter() {
        let q = query_file("kfilter");
        let conn = open_db();
        seed(&conn, &q, true);
        let p = provider();
        // k=2 → 交付条数确实被 k 裁（扩池后命中 2 条，全部交付）
        let two = find_similar_precise(&conn, &p, 1, 2, &SearchFilter::default()).unwrap();
        assert_eq!(two.hits.len(), 2);
        assert_eq!(two.hits[0].asset_id, 3);
        // k=1 → 候选池只有 4 条（k×4），语义排第 5 的 a3 进不了池 → 交付 a4。
        // 这是候选池倍数的**已知边界**（k=60 时池 = 240，实际远大于此处的测试夹具）。
        let one = find_similar_precise(&conn, &p, 1, 1, &SearchFilter::default()).unwrap();
        assert_eq!(one.hits.len(), 1);
        assert_eq!(one.hits[0].asset_id, 4);
        // 标量过滤（ext=png 无匹配）→ 语义召回为空，精准结果也空，但不是报错
        let none = find_similar_precise(
            &conn,
            &p,
            1,
            10,
            &SearchFilter {
                ext: Some("png".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(none.hits.is_empty());
        assert_eq!(none.candidate_pool, 0);
        let _ = std::fs::remove_file(q);
    }

    #[test]
    fn precise_query_without_dedup_vector_is_visible_not_silent() {
        // 查询图自己没判重向量（KI-2a 尚未覆盖到它）→ 空结果 + 明确标记，不是 panic 也不是「没有相似图」
        let q = query_file("querymiss");
        let conn = open_db();
        seed(&conn, &q, true);
        conn.execute(&format!("DELETE FROM \"{}\" WHERE resourceId = 1", core_storage::vecstore::table_name(DEDUP)), [])
            .unwrap();
        let p = provider();
        let res = find_similar_precise(&conn, &p, 1, 10, &SearchFilter::default()).unwrap();
        assert!(res.query_missing_dedup_vector);
        assert!(res.dedup_index_ready);
        assert_eq!(res.candidate_pool, 4);
        assert_eq!(res.missing_dedup_vectors, 1, "只有 a5 缺向量");
        assert!(res.hits.is_empty(), "查询图没向量时无从 rerank");
        let _ = std::fs::remove_file(q);
    }

    #[test]
    fn precise_without_dedup_table_reports_not_ready() {
        // 整库还没算过判重向量（KI-2a 之前的历史库）→ 表不存在也不能报错，要给「先跑嵌入」的依据
        let q = query_file("notable");
        let conn = open_db();
        seed(&conn, &q, false);
        let p = provider();
        let res = find_similar_precise(&conn, &p, 1, 10, &SearchFilter::default()).unwrap();
        assert!(!res.dedup_index_ready);
        assert!(res.query_missing_dedup_vector);
        assert!(res.hits.is_empty());
        assert_eq!(res.missing_dedup_vectors, res.candidate_pool);
        assert_eq!(res.candidate_pool, 4);
        let _ = std::fs::remove_file(q);
    }

    /// 零回归钉：语义模式（默认路径）的交付顺序与分数必须与改造前一致。
    /// 夹具 cos：a1=1.0（自身）/ a2=0.71 / a5=0.70 / a4=0.60 / a3=0.50 —— 精确值 + 精确顺序都断言。
    #[test]
    fn semantic_mode_topk_and_scores_unchanged() {
        let q = query_file("zeroregress");
        let conn = open_db();
        seed(&conn, &q, true);
        let p = provider();
        let hits = find_similar(&conn, &p, 1, 10, &SearchFilter::default(), &SimilarOptions::default())
            .unwrap();
        let got: Vec<(i64, f32)> = hits.iter().map(|h| (h.assetId, h.score)).collect();
        let expect = [(1i64, 1.0f32), (2, 0.71), (5, 0.70), (4, 0.60), (3, 0.50)];
        assert_eq!(got.len(), expect.len(), "默认模式不丢候选：{got:?}");
        for (i, (id, score)) in expect.iter().enumerate() {
            assert_eq!(got[i].0, *id, "第 {i} 条资产变了：{got:?}");
            assert!(
                (got[i].1 - score).abs() < 1e-3,
                "第 {i} 条分数变了：期望 {score} 实际 {}",
                got[i].1
            );
        }
        let _ = std::fs::remove_file(q);
    }

    #[test]
    fn precise_unknown_asset_is_an_error_not_empty_result() {
        let q = query_file("noasset");
        let conn = open_db();
        seed(&conn, &q, true);
        let p = provider();
        let e = find_similar_precise(&conn, &p, 999, 10, &SearchFilter::default());
        assert!(e.is_err(), "资产不存在必须是错误（HTTP 500/sqlite no rows），不能装成空结果");
        let _ = std::fs::remove_file(q);
    }
}

/// KI-2b（卡 t_8c1c9b89）：视觉判重边（`dup_edges`）并入分组的行为测试。
/// 钉住的是**用户看到的结论**：两套判据分层合并、kind 分开标注、混合组标 mixed、
/// 回收站资产不进组、成员的视觉相似度可呈现。
#[cfg(test)]
mod ki2b_dup_edge_tests {
    use super::*;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        conn
    }

    fn seed_asset(
        conn: &Connection,
        id: i64,
        hash: Option<&str>,
        phash: Option<u64>,
        w: i64,
        h: i64,
        sz: i64,
    ) {
        conn.execute(
            "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime,
                                 fileHash, importedAt)
             VALUES (?1, ?2, 0, ?3, 'jpg', ?4, ?5, ?6, ?1)",
            rusqlite::params![id, format!("/a/{id}.jpg"), format!("f{id}.jpg"), sz, id, hash],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO resource (assetId, role, ordinal, width, height, pHash)
             VALUES (?1, 'primary', 0, ?2, ?3, ?4)",
            rusqlite::params![id, w, h, phash.map(|v| format!("{v:016x}"))],
        )
        .unwrap();
    }

    fn insert_edge(conn: &Connection, a: i64, b: i64, score: f32, kind: &str) {
        let (a, b) = if a < b { (a, b) } else { (b, a) };
        conn.execute(
            "INSERT INTO dup_edges (a, b, score, kind, model, createdAt)
             VALUES (?1, ?2, ?3, ?4, 'dinov3-vits16-onnx', unixepoch())",
            rusqlite::params![a, b, score, kind],
        )
        .unwrap();
    }

    /// 精确对（同 fileHash）+ 一条视觉边连进来 → 一个组、kind 取最保守层、mixed=true
    #[test]
    fn visual_edge_merges_into_legacy_group_as_mixed() {
        let conn = setup();
        seed_asset(&conn, 1, Some("h1"), Some(0), 800, 600, 100);
        seed_asset(&conn, 2, Some("h1"), Some(0), 800, 600, 100);
        // 3 与 1/2 无 hash / pHash 关系（u64::MAX 与 0 距 64 bit）
        seed_asset(&conn, 3, Some("h3"), Some(u64::MAX), 1200, 900, 300);
        insert_edge(&conn, 1, 3, 0.99, "near_visual");

        let gs = dup_groups(&conn).unwrap();
        assert_eq!(gs.len(), 1, "视觉边把精确对和第三张连成一组: {gs:?}");
        let g = &gs[0];
        assert_eq!(g.size, 3);
        assert_eq!(g.kind, "near_visual", "组内最保守的一层说话（不是 exact）");
        assert!(g.mixed, "混了 fileHash 与向量两层判据");
        assert_eq!(g.layers, vec!["phash".to_string(), "visual".to_string()]);
        // 视觉相似度只落在真正有边的成员上
        let sim = |id: i64| {
            g.members
                .iter()
                .find(|m| m.asset_id == id)
                .unwrap()
                .similarity
        };
        assert!((sim(1).unwrap() - 0.99).abs() < 1e-6);
        assert!((sim(3).unwrap() - 0.99).abs() < 1e-6);
        assert_eq!(sim(2), None, "没有视觉边的成员不编造百分比");
        // 预选保留项按分辨率最大（视觉层口径）
        assert_eq!(g.suggested_keep, 3);
        assert_eq!(g.reclaim_bytes, 200);
    }

    /// 纯视觉组：[0.90,0.98) → variant（不给一键全处理）
    #[test]
    fn pure_visual_variant_group() {
        let conn = setup();
        seed_asset(&conn, 4, Some("h4"), Some(0), 640, 480, 200);
        seed_asset(&conn, 5, Some("h5"), Some(u64::MAX), 1024, 768, 500);
        insert_edge(&conn, 4, 5, 0.95, "variant");

        let gs = dup_groups(&conn).unwrap();
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0].kind, "variant");
        assert!(!gs[0].mixed);
        assert_eq!(gs[0].layers, vec!["visual".to_string()]);
        assert_eq!(gs[0].suggested_keep, 5, "分辨率大的留下");
        assert!((gs[0].members[0].similarity.unwrap() - 0.95).abs() < 1e-6);
    }

    /// 边连到回收站资产 → 该资产不进组（恢复后由补判重建）
    #[test]
    fn edge_to_deleted_asset_is_ignored() {
        let conn = setup();
        seed_asset(&conn, 6, Some("h6"), Some(0), 800, 600, 100);
        seed_asset(&conn, 7, Some("h7"), Some(u64::MAX), 800, 600, 100);
        insert_edge(&conn, 6, 7, 0.99, "near_visual");
        conn.execute("UPDATE assets SET deleted = 1 WHERE assetId = 7", [])
            .unwrap();

        assert!(dup_groups(&conn).unwrap().is_empty(), "只剩 6 一张，不构成组");
    }

    /// 老库（0019 未跑 / 表被清）→ 行为与升级前一致，不报错
    #[test]
    fn missing_edges_table_degrades_to_legacy_grouping() {
        let conn = setup();
        seed_asset(&conn, 8, Some("h8"), Some(0), 800, 600, 100);
        seed_asset(&conn, 9, Some("h8"), Some(0), 800, 600, 100);
        let drop_sql = ["DROP", "TABLE", "dup_edges"].join(" ");
        conn.execute_batch(&drop_sql).unwrap();

        assert!(dup_edges(&conn).unwrap().is_empty());
        let gs = dup_groups(&conn).unwrap();
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0].kind, "exact");
        assert!(!gs[0].mixed);
        assert_eq!(gs[0].layers, vec!["phash".to_string()]);
    }

    /// kind 归一：对外只有四种；未知值落最保守档（不透传给前端）
    #[test]
    fn group_kind_normalization_is_closed_set() {
        assert_eq!(normalize_group_kind("exact"), "exact");
        assert_eq!(normalize_group_kind("near"), "near");
        assert_eq!(normalize_group_kind("near_visual"), "near_visual");
        assert_eq!(normalize_group_kind("variant"), "variant");
        assert_eq!(normalize_group_kind("weird_future_kind"), "variant");
        assert_eq!(dup_kind_layer("weird_future_kind"), "visual");
        assert_eq!(dup_kind_layer("exact"), "phash");
        assert_eq!(dup_kind_layer("near"), "phash");
    }
}

/// KI-11 彻底删除（purge）领域单测：软删门禁、磁盘边界（copyin vs reference）、
/// 库行/向量/缩略图清理、字节统计与幂等。全部走真 SQLite 文件（不 mock）。
#[cfg(test)]
mod purge_tests {
    use super::*;

    struct Fixture {
        dir: std::path::PathBuf,
        conn: Connection,
        thumbs: std::path::PathBuf,
        copyin_abs: std::path::PathBuf,
        reference_abs: std::path::PathBuf,
        model: &'static str,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn write(p: &std::path::Path, bytes: usize) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, vec![7u8; bytes]).unwrap();
    }

    /// 3 张资产：a = 回收站内 copyin（原图库内副本）、b = 回收站内 reference（用户原地文件）、
    /// c = 仍在库（deleted=0）。每张各 1 resource + 1 tag + 1 向量行 + 2 档缩略图。
    fn fixture() -> Fixture {
        core_storage::vecstore::init_vec_ext();
        let dir = std::env::temp_dir().join(format!(
            "dam-purge-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let thumbs = dir.join("thumbs");
        std::fs::create_dir_all(&thumbs).unwrap();
        let conn = Connection::open(dir.join("db.sqlite")).unwrap();
        core_storage::migrations::migrate(&conn).unwrap();

        let copyin_abs = dir.join("library/aa.jpg");
        let reference_abs = dir.join("outside/bb.jpg");
        write(&copyin_abs, 100);
        write(&reference_abs, 200);

        conn.execute_batch(
            "INSERT INTO assets (assetId, absPath, storeMode, missing, fileName, fileExt, fileSz, mtime, importedAt, deleted) VALUES
                (1, '', 'copyin',    0, 'aa.jpg', 'jpg', 100, 1, 1, 1),
                (2, '', 'reference', 0, 'bb.jpg', 'jpg', 200, 1, 1, 1),
                (3, '/x/cc.jpg', 'reference', 0, 'cc.jpg', 'jpg', 50, 1, 1, 0);
             INSERT INTO resource (assetId, role, thumbPath, width, height) VALUES
                (1, 'primary', '/t/1.webp', 10, 10),
                (2, 'primary', '/t/2.webp', 10, 10),
                (3, 'primary', '/t/3.webp', 10, 10);
             INSERT INTO tag (assetId, value, origin, createdAt) VALUES (1, '狗', 'manual', 1), (2, '车', 'auto', 1), (3, '人', 'auto', 1);
             INSERT INTO job_queue (kind, targetId, status, createdAt, updatedAt) VALUES ('tag', 1, 'done', 1, 1), ('tag', 2, 'done', 1, 1);",
        )
        .unwrap();
        conn.execute(
            "UPDATE assets SET absPath = ?1 WHERE assetId = 1",
            [copyin_abs.to_string_lossy().to_string()],
        )
        .unwrap();
        conn.execute(
            "UPDATE assets SET absPath = ?1 WHERE assetId = 2",
            [reference_abs.to_string_lossy().to_string()],
        )
        .unwrap();

        // 缩略图两档（a/b 有文件，c 没有 —— 让「NotFound 不计入」也被覆盖）
        for id in [1i64, 2] {
            for size in core_media::THUMB_SIZES {
                let p = thumbs
                    .join(size.to_string())
                    .join(format!("{:02}", id % 100))
                    .join(format!("{id}_{size}.webp"));
                write(&p, 10);
            }
        }

        // 向量：每张 resource 一行
        let model = "m-purge";
        core_storage::vecstore::ensure_table(&conn, model, 4).unwrap();
        let rids: Vec<i64> = ["SELECT resourceId FROM resource ORDER BY assetId"]
            .iter()
            .flat_map(|sql| {
                let mut stmt = conn.prepare(sql).unwrap();
                stmt.query_map([], |r| r.get::<_, i64>(0))
                    .unwrap()
                    .map(|r| r.unwrap())
                    .collect::<Vec<_>>()
            })
            .collect();
        let v = [1.0f32, 0.0, 0.0, 0.0];
        let rows: Vec<(i64, &[f32])> = rids.iter().map(|&rid| (rid, &v[..])).collect();
        core_storage::vecstore::upsert(&conn, model, &rows).unwrap();

        Fixture {
            dir,
            conn,
            thumbs,
            copyin_abs,
            reference_abs,
            model,
        }
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn thumb_count(fx: &Fixture, id: i64) -> usize {
        core_media::THUMB_SIZES
            .iter()
            .filter(|s| {
                fx.thumbs
                    .join(s.to_string())
                    .join(format!("{:02}", id % 100))
                    .join(format!("{id}_{s}.webp"))
                    .is_file()
            })
            .count()
    }

    #[test]
    fn purge_accepts_trash_only_and_reports_skipped() {
        let fx = fixture();
        let rep = purge_assets(&fx.conn, &fx.thumbs, &[1, 2, 3, 999]).unwrap();
        // 只接受回收站内（deleted=1）
        assert_eq!(rep.purged, vec![1, 2]);
        assert_eq!(rep.not_in_trash, vec![3, 999], "未软删/不存在一律拒绝");
        assert_eq!(rep.skipped(), 2);
        // 未软删的 c 完好无损
        assert_eq!(count(&fx.conn, "SELECT COUNT(*) FROM assets WHERE assetId=3"), 1);
        assert_eq!(count(&fx.conn, "SELECT COUNT(*) FROM resource WHERE assetId=3"), 1);
        assert_eq!(
            count(&fx.conn, "SELECT COUNT(*) FROM tag WHERE assetId=3"),
            1,
            "被跳过资产的标签必须原样保留"
        );
    }

    #[test]
    fn purge_copyin_original_deleted_reference_original_kept() {
        let fx = fixture();
        assert!(fx.copyin_abs.is_file() && fx.reference_abs.is_file());
        purge_assets(&fx.conn, &fx.thumbs, &[1, 2]).unwrap();
        assert!(!fx.copyin_abs.exists(), "copyin = 库内副本，彻底删要连原图一起删");
        assert!(
            fx.reference_abs.is_file(),
            "reference = 用户原地文件，只删记录，磁盘必须不动"
        );
    }

    #[test]
    fn purge_clears_rows_vectors_and_thumbs() {
        let fx = fixture();
        assert_eq!(thumb_count(&fx, 1), 2);
        let rep = purge_assets(&fx.conn, &fx.thumbs, &[1, 2]).unwrap();
        // 库行：assets/resource/tag/job_queue 全部消失
        assert_eq!(count(&fx.conn, "SELECT COUNT(*) FROM assets WHERE assetId IN (1,2)"), 0);
        assert_eq!(
            count(&fx.conn, "SELECT COUNT(*) FROM resource WHERE assetId IN (1,2)"),
            0
        );
        assert_eq!(count(&fx.conn, "SELECT COUNT(*) FROM tag WHERE assetId IN (1,2)"), 0);
        assert_eq!(
            count(&fx.conn, "SELECT COUNT(*) FROM job_queue WHERE targetId IN (1,2)"),
            0,
            "指向被删资产的任务队列行不能留"
        );
        assert!(rep.rows >= 8, "行数如实统计，实际 {}", rep.rows);
        // 向量：两张各 1 行被清，c 那行还在
        assert_eq!(rep.vectors, 2);
        assert_eq!(core_storage::vecstore::count(&fx.conn, fx.model).unwrap(), 1);
        // 缩略图两档全删
        assert_eq!(rep.thumbs, 4, "每张 2 档，共 4 个文件");
        assert_eq!(thumb_count(&fx, 1) + thumb_count(&fx, 2), 0);
    }

    #[test]
    fn purge_counts_real_freed_bytes() {
        let fx = fixture();
        // 目标文件体积：copyin 原图 100 + 2 档缩略图（1 号 10+10、2 号 10+10）= 140
        let rep = purge_assets(&fx.conn, &fx.thumbs, &[1, 2]).unwrap();
        assert_eq!(rep.files_deleted, 5, "4 个缩略图 + 1 个 copyin 原图");
        assert_eq!(rep.freed_bytes, 140, "只统计真实删掉的字节");
    }

    #[test]
    fn purge_is_idempotent_and_second_call_is_noop() {
        let fx = fixture();
        let first = purge_assets(&fx.conn, &fx.thumbs, &[1]).unwrap();
        assert_eq!(first.purged, vec![1]);
        let second = purge_assets(&fx.conn, &fx.thumbs, &[1]).unwrap();
        assert!(second.purged.is_empty(), "第二次 purged 必须为 0");
        assert_eq!(second.not_in_trash, vec![1], "已彻底删过的 id 报「不在回收站」");
        assert_eq!(second.files_deleted, 0);
        assert_eq!(second.freed_bytes, 0);
        assert_eq!(second.vectors, 0);
        // 空入参：什么都不做（不报错）
        let empty = purge_assets(&fx.conn, &fx.thumbs, &[]).unwrap();
        assert_eq!(empty.purged.len() + empty.not_in_trash.len(), 0);
    }

    #[test]
    fn purge_clears_ocr_index_entries() {
        let fx = fixture();
        fx.conn
            .execute("INSERT INTO ocr_fts (rowid, grams) VALUES (1, '狗 狗狗')", [])
            .unwrap();
        assert_eq!(count(&fx.conn, "SELECT COUNT(*) FROM ocr_fts WHERE rowid=1"), 1);
        purge_assets(&fx.conn, &fx.thumbs, &[1]).unwrap();
        assert_eq!(
            count(&fx.conn, "SELECT COUNT(*) FROM ocr_fts WHERE rowid=1"),
            0,
            "彻底删后 OCR 倒排不能留幽灵行"
        );
    }
}

// ==================== MIGRATE-TO-LIBRARY（卡 t_fcb18ceb / DT6） ====================
// 「原地索引（storeMode='reference'）」→「复制入库（'copyin'）」的显式**单向**迁移。
//
// 口径（产品定义已批准，照做）：
// * **单向**：只做 reference → copyin；不做反向，也不改库目录设置表单。
// * **源文件默认保留**（`MigrateOptions::delete_source=false`）：迁移是「再放一份进库根 +
//   切 storeMode」，不是搬走；只有 `deleteSource:true` 才在**复制成功且 DB 落定之后**删源。
// * **目标布局与 copyin 导入完全一致**：`<library_root>/<hash 前 16 位>/<原文件名>`
//   —— 与 `svc_import::process_file` 的落盘路径逐字符相同（本卡要求「先 grep 导入侧实现
//   对齐」）。两处不一致会让同一 storeMode 出现两套路径形态，去重/目录树/导出跟着分叉；
//   且**同路径**才能让「导入已经落过的那份」被续跑判定直接命中，不产生第二份物理文件。
// * **不重算派生数据**：缩略图/pHash/去重向量/语义向量/标签/评分/审批/项目归属都绑
//   assetId 或内容 hash，迁移只动 absPath + storeMode（resource.thumbPath 不动）。
// * **先核对内容再搬**：迁移前用 `fileHash` 与磁盘实况比对；对不上说明文件被替换过 →
//   skip(reason=content_changed)，让用户走重新导入（updated 语义），不把错内容搬进库。
// * **不留半成品**：复制写 `<target>.part-<pid>-<seq>`（同目录 → rename 原子），字节数
//   校验后才 rename 生效；任何一步失败都清掉 .part，资产保持原样（仍指向原路径）。
// * **可中断续跑**：目标文件已存在且字节数与源一致 = 这份内容已落库过 → 0 字节复制，
//   直接切 storeMode。断点判据是**盘上事实**（路径由内容 hash 决定），不依赖内存状态。
// * **审计**：每个真正迁移的资产落一行 `migrate_audit`（人/张数/原路径→库根/字节数/时间/
//   状态），一次调用 = 一个 batchId（可回查、可作迁移清单证据）。

/// 单机形态没有用户身份：审计里的 `operator` 默认 `local`（请求可覆盖）。
pub const MIGRATE_DEFAULT_OPERATOR: &str = "local";

/// 单批上限（与 `library_ids` 其它批量口一致：SQLite 变量数上限内留余量）。
pub const MIGRATE_MAX_IDS: usize = 900;

/// 目标路径（**纯函数**）：与 copyin 导入同布局 `<library_root>/<hash 前 16 位>/<原文件名>`。
///
/// 文件名缺失时退化为 `<hash>.<ext>`（导入侧同样要求 fileName 非空，这条只是兜底，
/// 保证「有 hash 就一定落得下去」）。
pub fn migration_target(
    library_root: &Path,
    file_hash: &str,
    file_name: &str,
    file_ext: &str,
) -> std::result::Result<PathBuf, LibraryError> {
    let hash = file_hash.trim().to_ascii_lowercase();
    if hash.len() < 16 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(LibraryError::Invalid(format!(
            "fileHash 不是合法的 sha256 十六进制串：{file_hash}"
        )));
    }
    let name = {
        let n = file_name.trim();
        if !n.is_empty() {
            n.to_string()
        } else {
            let ext = file_ext.trim().trim_start_matches('.');
            if ext.is_empty() {
                hash.clone()
            } else {
                format!("{hash}.{ext}")
            }
        }
    };
    Ok(library_root.join(&hash[..16]).join(name))
}

/// 写盘中间态文件名（同目录 → rename 原子；不覆盖目标，失败可直接删）。
fn part_path(target: &Path) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let base = target
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "asset".to_string());
    target.with_file_name(format!(".{base}.part-{}-{}", std::process::id(), seq))
}

/// 续跑判定（**纯函数**）：目标文件已存在且字节数一致 = 这份内容已在库内。
///
/// 为什么字节数够用：目标路径由此前写入的那份内容的 `fileHash` 决定，同路径出现不同
/// 内容只可能是 sha256 碰撞；字节数是零成本第二道闸（.part 残留不会命中）。
pub fn is_resume_hit(target: &Path, expected_bytes: i64) -> bool {
    std::fs::metadata(target)
        .map(|m| m.is_file() && m.len() as i64 == expected_bytes)
        .unwrap_or(false)
}

/// skip 分类（**纯函数**，单测逐档覆盖）。顺序即优先级。
///
/// 返回 `None` = 该资产可以被迁移。
pub fn skip_reason(
    persisted: bool,
    has_hash: bool,
    store_mode: &str,
    file_readable: bool,
    hash_matches: bool,
) -> Option<&'static str> {
    if !persisted {
        return Some("not_found");
    }
    if !has_hash {
        return Some("no_hash");
    }
    if store_mode != "reference" {
        return Some("already_copyin");
    }
    if !file_readable {
        return Some("missing_file");
    }
    if !hash_matches {
        return Some("content_changed");
    }
    None
}

/// skip 原因的人话（UI/报告用；code 保持稳定，文案随产品改）。
pub fn skip_reason_text(reason: &str) -> &'static str {
    match reason {
        "not_found" => "资产不存在或已删除",
        "no_hash" => "资产缺少内容指纹（fileHash），无法定位库内路径",
        "already_copyin" => "已经是复制入库（copyin），无需迁移",
        "missing_file" => "磁盘上找不到原文件（失联），无法迁移",
        "content_changed" => "磁盘文件内容与入库指纹不一致（被替换过），请重新导入而不是迁移",
        _ => "跳过",
    }
}

/// 进度快照（批量跑长时给 `GET .../status` 用；只读计数，不含逐条回执）。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateProgress {
    pub total: usize,
    pub processed: usize,
    pub migrated: usize,
    pub skipped: usize,
    pub failed: usize,
    pub bytes: i64,
}

/// 进度回调（app-web 用它把快照写进 status 端点；L2 不依赖任何 HTTP 类型）。
pub type MigrateProgressFn = std::sync::Arc<dyn Fn(&MigrateProgress) + Send + Sync>;

/// 迁移选项。
#[derive(Clone, Default)]
pub struct MigrateOptions {
    /// 批次 id；None/空 = 本层生成（`mig-<ms>-<pid>-<seq>`）
    pub batch_id: Option<String>,
    /// 审计里的「人」；空 = `local`
    pub operator: String,
    /// 复制成功后删源文件（默认 false：源默认保留）
    pub delete_source: bool,
    /// 每处理完一个资产回调一次（可选）
    pub progress: Option<MigrateProgressFn>,
    /// 删源动作注入口（**仅 crate 内测试用**：容器里以 root 跑，chmod/权限造不出 unlink
    /// 失败，这条防线只能靠注入验证）。生产一律 `None` → `std::fs::remove_file`。
    pub(crate) remove_src: Option<std::sync::Arc<dyn Fn(&Path) -> std::io::Result<()> + Send + Sync>>,
}

/// 跳过明细。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateSkip {
    pub asset_id: i64,
    pub reason: String,
    pub detail: String,
}

/// 失败明细（真错误：复制/rename/DB 写失败；资产保持原样）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateFailure {
    pub asset_id: i64,
    pub code: String,
    pub detail: String,
}

/// 告警（不影响 migrated 结论，例如 `deleteSource:true` 但源删不掉）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateWarning {
    pub asset_id: i64,
    pub detail: String,
}

/// 整批报告：`{ migrated, skipped: [{assetId, reason, detail}], bytes }` + 失败/告警/批次。
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateReport {
    pub batch_id: String,
    /// 本次请求的 id 数（含重复入参）
    pub total: usize,
    pub migrated: usize,
    pub skipped: Vec<MigrateSkip>,
    pub failed: Vec<MigrateFailure>,
    /// 本次真正写进库根的字节数（续跑命中 = 0）
    pub bytes: i64,
    pub warnings: Vec<MigrateWarning>,
}

/// `migrate_audit` 的一行（迁移清单 / 证据 dump）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateAuditRow {
    pub id: i64,
    pub batch_id: String,
    pub kind: String,
    pub asset_id: i64,
    pub from: String,
    pub to: String,
    pub bytes: i64,
    pub operator: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub at: i64,
}

/// 资产迁移所需的表头（一次查询，避免同一资产重复查库）。
struct MigrateHeader {
    abs_path: String,
    store_mode: String,
    missing: i64,
    file_name: String,
    file_ext: String,
    file_hash: Option<String>,
}

fn migrate_header(conn: &Connection, asset_id: i64) -> Result<Option<MigrateHeader>> {
    Ok(conn
        .query_row(
            "SELECT absPath, storeMode, missing, fileName, fileExt, fileHash
             FROM assets WHERE assetId=?1 AND deleted=0",
            [asset_id],
            |r| {
                Ok(MigrateHeader {
                    abs_path: r.get(0)?,
                    store_mode: r.get(1)?,
                    missing: r.get(2)?,
                    file_name: r.get(3)?,
                    file_ext: r.get(4)?,
                    file_hash: r.get(5)?,
                })
            },
        )
        .ok())
}

/// 批次 id 生成（毫秒 + pid + 进程内序号：单机唯一、肉眼可读、不引 uuid 依赖）。
pub fn new_migrate_batch_id() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!(
        "mig-{}-{}-{}",
        ms,
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// 写一行审计（`status` = done|failed）。
#[allow(clippy::too_many_arguments)]
fn insert_audit(
    conn: &rusqlite::Connection,
    batch_id: &str,
    kind: &str,
    asset_id: i64,
    from: &str,
    to: &str,
    bytes: i64,
    operator: &str,
    status: &str,
    error: Option<&str>,
) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO migrate_audit
             (batchId, kind, assetId, fromPath, toPath, bytes, operator, status, error, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, unixepoch())",
        rusqlite::params![
            batch_id, kind, asset_id, from, to, bytes, operator, status, error
        ],
    )
}

/// 审计清单（`batch_id=None` → 全部；最近行在前）。
pub fn migrate_audit_rows(
    conn: &Connection,
    batch_id: Option<&str>,
    limit: u32,
) -> Result<Vec<MigrateAuditRow>> {
    let limit = limit.clamp(1, 1000);
    let map = |r: &rusqlite::Row<'_>| {
        Ok(MigrateAuditRow {
            id: r.get(0)?,
            batch_id: r.get(1)?,
            kind: r.get(2)?,
            asset_id: r.get(3)?,
            from: r.get(4)?,
            to: r.get(5)?,
            bytes: r.get(6)?,
            operator: r.get(7)?,
            status: r.get(8)?,
            error: r.get(9)?,
            at: r.get(10)?,
        })
    };
    let mut out = Vec::new();
    match batch_id {
        Some(b) => {
            let mut stmt = conn.prepare(
                "SELECT id, batchId, kind, assetId, fromPath, toPath, bytes, operator, status,
                        error, at
                 FROM migrate_audit WHERE batchId=?1 ORDER BY id DESC LIMIT ?2",
            )?;
            for row in stmt.query_map(rusqlite::params![b, limit], map)? {
                out.push(row?);
            }
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT id, batchId, kind, assetId, fromPath, toPath, bytes, operator, status,
                        error, at
                 FROM migrate_audit ORDER BY id DESC LIMIT ?1",
            )?;
            for row in stmt.query_map([limit], map)? {
                out.push(row?);
            }
        }
    }
    Ok(out)
}

/// 单个资产迁移：置文件 → DB 落定 → 审计（DB 与审计同一事务）。
///
/// 失败返回 `(code, detail)`，且**磁盘与 DB 都回到迁移前**（本次新建的目标文件会被删掉）。
/// 单个资产的迁移实现：`remove_src` 是删源动作（生产 = `std::fs::remove_file`，
/// 测试可注入失败以验证「删不掉源只挂 warning」这条防线）。
#[allow(clippy::too_many_arguments)]
fn migrate_one_with(
    conn: &mut Connection,
    library_root: &Path,
    h: &MigrateHeader,
    asset_id: i64,
    batch_id: &str,
    operator: &str,
    delete_source: bool,
    remove_src: &dyn Fn(&Path) -> std::io::Result<()>,
) -> std::result::Result<(i64, Option<MigrateWarning>), (String, String)> {
    let hash = h.file_hash.as_deref().map(str::trim).unwrap_or_default();
    let src = PathBuf::from(&h.abs_path);
    let src_len = std::fs::metadata(&src).map(|m| m.len() as i64).unwrap_or(0);
    let target = migration_target(library_root, hash, &h.file_name, &h.file_ext)
        .map_err(|e| ("no_hash".to_string(), e.to_string()))?;

    let mut copied = 0i64;
    let mut created = false;
    if is_resume_hit(&target, src_len) {
        // 续跑命中：库内已有同字节数的这份内容 → 0 字节复制，只切形态
    } else {
        if let Some(parent) = target.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return Err((
                    classify_fs_error(&e).to_string(),
                    format!("库内分目录不可创建 {}：{e}", parent.display()),
                ));
            }
        }
        let part = part_path(&target);
        if let Err(e) = std::fs::copy(&src, &part) {
            let _ = std::fs::remove_file(&part);
            return Err((
                classify_fs_error(&e).to_string(),
                format!("复制到库根失败：{e}"),
            ));
        }
        let part_len = std::fs::metadata(&part).map(|m| m.len() as i64).unwrap_or(-1);
        if part_len != src_len {
            let _ = std::fs::remove_file(&part);
            return Err((
                "copy_failed".to_string(),
                format!("复制后字节数不符（{part_len} ≠ {src_len}），已清掉临时文件"),
            ));
        }
        // rename 生效（先删同名旧目标：Windows 上 rename 到已存在会失败）
        if target.exists() {
            let _ = std::fs::remove_file(&target);
        }
        if let Err(e) = std::fs::rename(&part, &target) {
            let _ = std::fs::remove_file(&part);
            return Err((
                classify_fs_error(&e).to_string(),
                format!("临时文件转正失败：{e}"),
            ));
        }
        created = true;
        copied = src_len;
    }

    let to_str = target.to_string_lossy().to_string();
    // DB 落定 + 审计同一事务：要么「absPath 变了且有账」，要么都不发生
    let cleanup = |created: bool, target: &Path| {
        if created {
            let _ = std::fs::remove_file(target);
        }
    };
    let tx = match conn.transaction() {
        Ok(t) => t,
        Err(e) => {
            cleanup(created, &target);
            return Err(("sqlite".to_string(), format!("开启事务失败：{e}")));
        }
    };
    let changed = match tx.execute(
        "UPDATE assets SET absPath=?1, storeMode='copyin', missing=0
         WHERE assetId=?2 AND deleted=0",
        rusqlite::params![to_str, asset_id],
    ) {
        Ok(n) => n,
        Err(e) => {
            drop(tx);
            cleanup(created, &target);
            return Err(("sqlite".to_string(), format!("absPath 更新失败：{e}")));
        }
    };
    if changed != 1 {
        drop(tx);
        cleanup(created, &target);
        return Err((
            "not_found".to_string(),
            "资产在迁移过程中被删除（更新 0 行）".to_string(),
        ));
    }
    if let Err(e) = insert_audit(
        &tx,
        batch_id,
        "copyin",
        asset_id,
        &h.abs_path,
        &to_str,
        copied,
        operator,
        "done",
        None,
    ) {
        drop(tx);
        cleanup(created, &target);
        return Err(("sqlite".to_string(), format!("迁移账本写入失败：{e}")));
    }
    if let Err(e) = tx.commit() {
        cleanup(created, &target);
        return Err(("sqlite".to_string(), format!("提交失败：{e}")));
    }

    // 源文件默认保留；显式要求删源时才动它（此时库内那份已经落定）
    let mut warning = None;
    if delete_source {
        if let Err(e) = remove_src(&src) {
            warning = Some(MigrateWarning {
                asset_id,
                detail: format!("复制成功但删不掉源文件（稍后手动清理）：{e}"),
            });
        }
    }
    Ok((copied, warning))
}

/// 整批迁移：reference → copyin（单向，可中断续跑）。
///
/// - 只处理「reference + missing 可读 + 磁盘内容与 fileHash 一致」的资产；
///   其余进 `skipped[]`（带稳定 reason + 人话 detail）。
/// - 单条失败进 `failed[]`：资产仍指向原路径、库内不留半成品。
/// - 一条失败**不**中止整批（900 张的批次不该被一张坏图卡死），回执逐条可查。
pub fn migrate_to_library(
    conn: &mut Connection,
    ids: &[i64],
    library_root: &Path,
    opts: &MigrateOptions,
) -> Result<MigrateReport> {
    if ids.is_empty() {
        return Err(LibraryError::Invalid("ids 不能为空".into()));
    }
    if ids.len() > MIGRATE_MAX_IDS {
        return Err(LibraryError::Invalid(format!(
            "单批上限 {MIGRATE_MAX_IDS} 个资产，收到 {}",
            ids.len()
        )));
    }
    if !library_root.is_absolute() {
        return Err(LibraryError::Invalid(format!(
            "库根必须是绝对路径：{}",
            library_root.display()
        )));
    }
    std::fs::create_dir_all(library_root).map_err(|e| {
        LibraryError::Invalid(format!("库根不可创建 {}：{e}", library_root.display()))
    })?;

    let batch_id = opts
        .batch_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(new_migrate_batch_id);
    let operator = {
        let o = opts.operator.trim();
        if o.is_empty() {
            MIGRATE_DEFAULT_OPERATOR.to_string()
        } else {
            o.to_string()
        }
    };

    let mut report = MigrateReport {
        batch_id: batch_id.clone(),
        total: ids.len(),
        ..Default::default()
    };
    let mut progress = MigrateProgress {
        total: ids.len(),
        ..Default::default()
    };
    let mut done_ids: Vec<i64> = Vec::new();

    for &id in ids {
        if done_ids.contains(&id) {
            continue; // 重复入参只处理一次（total 仍按请求计）
        }
        done_ids.push(id);

        let Some(h) = migrate_header(conn, id)? else {
            let reason = skip_reason(false, false, "", false, false).unwrap_or("not_found");
            report.skipped.push(MigrateSkip {
                asset_id: id,
                reason: reason.to_string(),
                detail: skip_reason_text(reason).to_string(),
            });
            progress.processed += 1;
            progress.skipped += 1;
            if let Some(cb) = &opts.progress {
                cb(&progress);
            }
            continue;
        };

        let has_hash = h
            .file_hash
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty());
        let src = PathBuf::from(&h.abs_path);
        // 只在「会去迁移」的前提下做 IO：rename/exists 已经够便宜，hash 要读全文件
        let readable = src.is_file() && std::fs::File::open(&src).is_ok();
        let hash_matches = if h.store_mode == "reference" && readable && has_hash {
            match std::fs::read(&src) {
                Ok(bytes) => core_hash::sha256_hex(&bytes)
                    .eq_ignore_ascii_case(h.file_hash.as_deref().unwrap_or_default()),
                Err(_) => false,
            }
        } else {
            true
        };

        if let Some(reason) = skip_reason(true, has_hash, &h.store_mode, readable, hash_matches) {
            if reason == "missing_file" {
                // 与 move_assets 同口径：盘上没文件就标失联（资产本身不动）
                let _ = conn.execute(
                    "UPDATE assets SET missing=1 WHERE assetId=?1 AND missing=0",
                    [id],
                );
            }
            report.skipped.push(MigrateSkip {
                asset_id: id,
                reason: reason.to_string(),
                detail: skip_reason_text(reason).to_string(),
            });
            progress.processed += 1;
            progress.skipped += 1;
            if let Some(cb) = &opts.progress {
                cb(&progress);
            }
            continue;
        }

        progress.processed += 1;
        let remove_src: &dyn Fn(&Path) -> std::io::Result<()> = match opts.remove_src.as_deref() {
            Some(f) => f,
            None => &|p: &Path| std::fs::remove_file(p),
        };
        match migrate_one_with(
            conn,
            library_root,
            &h,
            id,
            &batch_id,
            &operator,
            opts.delete_source,
            remove_src,
        ) {
            Ok((bytes, warning)) => {
                report.migrated += 1;
                report.bytes += bytes;
                if let Some(w) = warning {
                    report.warnings.push(w);
                }
                progress.migrated += 1;
                progress.bytes += bytes;
            }
            Err((code, detail)) => {
                // 失败也记账（重试时据此定位；历史行不改写）
                let to = migration_target(
                    library_root,
                    h.file_hash.as_deref().unwrap_or_default(),
                    &h.file_name,
                    &h.file_ext,
                )
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
                let _ = insert_audit(
                    conn,
                    &batch_id,
                    "copyin",
                    id,
                    &h.abs_path,
                    &to,
                    0,
                    &operator,
                    "failed",
                    Some(&detail),
                );
                report.failed.push(MigrateFailure {
                    asset_id: id,
                    code,
                    detail,
                });
                progress.failed += 1;
            }
        }
        if let Some(cb) = &opts.progress {
            cb(&progress);
        }
    }
    Ok(report)
}

#[cfg(test)]
mod migrate_fault_tests {
    //! 防线单测：**删不掉源文件**（`deleteSource:true`）。
    //!
    //! 为什么放在「同文件 `#[cfg(test)]`」而不是集成测试：这条失败要用注入的删源动作造
    //! （容器里以 root 跑，chmod/权限造不出 unlink 失败），注入点 `migrate_one_with` 是
    //! 私有实现细节，不对 crate 外暴露。
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dam-migrate-fault-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn delete_source_failure_keeps_copy_and_returns_warning() {
        let dir = temp_dir("delsrc");
        let mut conn = core_storage::open(&dir.join("dam.db")).unwrap();
        let src = dir.join("src.jpg");
        let payload = b"payload-bytes";
        std::fs::write(&src, payload).unwrap();
        let hash = core_hash::sha256_hex(payload);
        conn.execute(
            "INSERT INTO assets (mediaType, absPath, storeMode, fileName, fileExt, fileSz,
                                 mtime, fileHash, importedAt)
             VALUES ('image', ?1, 'reference', 'src.jpg', 'jpg', ?2, 0, ?3, unixepoch())",
            rusqlite::params![src.to_string_lossy(), payload.len() as i64, hash],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        let h = migrate_header(&conn, id).unwrap().expect("header");
        let lib_root = dir.join("library");

        let res = migrate_one_with(
            &mut conn,
            &lib_root,
            &h,
            id,
            "b-fault",
            "local",
            true,
            &|_p: &Path| Err(std::io::Error::other("模拟：删源被拒")),
        );
        let (bytes, warning) = res.expect("删源失败不影响迁移结论");
        assert_eq!(bytes, payload.len() as i64);
        let w = warning.expect("必须挂一条 warning");
        assert_eq!(w.asset_id, id);
        assert!(w.detail.contains("删不掉源文件"), "{}", w.detail);
        // 库内那份在、形态已切、源仍保留
        let abs: String = conn
            .query_row("SELECT absPath FROM assets WHERE assetId=?1", [id], |r| r.get(0))
            .unwrap();
        assert!(Path::new(&abs).is_file());
        let mode: String = conn
            .query_row("SELECT storeMode FROM assets WHERE assetId=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "copyin");
        assert!(src.is_file(), "删不掉的部分不该假装成功");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 整批层面：warning 必须回到 report.warnings（前端要能显示「已入库，源文件没删掉」）。
    #[test]
    fn batch_report_carries_delete_source_warning() {
        let dir = temp_dir("batchwarn");
        let mut conn = core_storage::open(&dir.join("dam.db")).unwrap();
        let src = dir.join("b.jpg");
        let payload = b"batch-payload";
        std::fs::write(&src, payload).unwrap();
        let hash = core_hash::sha256_hex(payload);
        conn.execute(
            "INSERT INTO assets (mediaType, absPath, storeMode, fileName, fileExt, fileSz,
                                 mtime, fileHash, importedAt)
             VALUES ('image', ?1, 'reference', 'b.jpg', 'jpg', ?2, 0, ?3, unixepoch())",
            rusqlite::params![src.to_string_lossy(), payload.len() as i64, hash],
        )
        .unwrap();
        let id = conn.last_insert_rowid();

        let opts = MigrateOptions {
            batch_id: Some("b-batchwarn".into()),
            delete_source: true,
            remove_src: Some(std::sync::Arc::new(|_p: &Path| {
                Err(std::io::Error::other("模拟：删源被拒"))
            })),
            ..Default::default()
        };
        let rep =
            migrate_to_library(&mut conn, &[id], &dir.join("library"), &opts).unwrap();
        assert_eq!(rep.migrated, 1, "{rep:?}");
        assert_eq!(rep.warnings.len(), 1);
        assert_eq!(rep.warnings[0].asset_id, id);
        assert!(src.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
