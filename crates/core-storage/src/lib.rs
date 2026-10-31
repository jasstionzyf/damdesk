//! L0 `core-storage` — SQLite 封装（打开/PRAGMA/版本化迁移）、DAO 与文件工具（PLAN.md §2.1）。
//!
//! D1 落地：`open()` 打开库文件 → 设置 PRAGMA → 跑完 `schema_migrations`。
//! D5 落地：`vecstore`（sqlite-vec 按 embModel 分表的向量库，禁混算铁律）。

pub mod compliance;
pub mod compliance_runs;
pub mod compliance_runs_sqlite;
pub mod compliance_sqlite;
// N6e（task）：资产工作版本账本（migration 0029；一资产至多一张 current=1）
pub mod asset_versions;
pub mod exif_meta;
pub mod custom_tasks;
pub mod migrations;
// OCR-MATCH：ocr_text 字面检索的 FTS5 倒排（中文 2-gram + 单字允许；见模块头）
pub mod ocr_index;
// DM-C1（task）：OCR 文本行框落库（migration 0034；inpaint 重绘区域的可复核来源）
pub mod ocr_boxes;
// IP-K05：行业包身份账本（migration 0026；manifest 的落库快照 + 生效版本指针）
pub mod packs;
// G5-A：失联「失败即标」的唯一写入口（导出/读原图/相似图重算共用）
pub mod presence;
pub mod settings;
pub mod smart_folders;
pub mod tags;
// N1 规格实体（migration 0028；插件下的「规格」= 配置，不安装不分发）
pub mod specs;
pub mod vecstore;

use std::path::Path;

use rusqlite::Connection;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("migration {version} ({name}) failed: {msg}")]
    Migration {
        version: i64,
        name: String,
        msg: String,
    },
    /// D10：DAO 层入参校验（任务名/提示词为空、重名等，UI 可直接展示）。
    #[error("{0}")]
    Invalid(String),
    /// EXIF2：通用错误（目标资产不存在等）。
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, StorageError>;

/// 缩略图路径约定（D11 上移 L0，svc-import / svc-library 共用，避免同层互引）：
/// `thumbs/<size>/<assetId%100 两位分片>/<assetId>_<size>.webp`
pub fn thumb_path(thumbs_root: &Path, asset_id: i64, size: i32) -> std::path::PathBuf {
    thumbs_root
        .join(size.to_string())
        .join(format!("{:02}", asset_id % 100))
        .join(format!("{asset_id}_{size}.webp"))
}

/// 打开（或创建）库数据库并跑完全部迁移，随后做一次 OCR 索引自愈探测。
///
/// OCR 索引（0013）的写入同步靠触发器 + 索引自愈：`ocr_text` 变更由触发器撤旧行并标
/// dirty，这里用 `ocr_index::sync_if_dirty` 把 dirty 行按同一分段函数补建。无 dirty 时
/// 只是两次小表探测（微秒级），所以每连接调用是安全的；有 dirty 时（首启升级、外部
/// 写入 ocr_text）在此一次性收敛，避免索引缺失在检索路径上静默漏召回。
pub fn open(path: &Path) -> Result<Connection> {
    // sqlite-vec 扩展必须在首个连接打开前注册（进程级 auto_extension）
    vecstore::init_vec_ext();
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;",
    )?;
    migrations::migrate(&conn)?;
    if ocr_index::needs_sync(&conn)? {
        ocr_index::sync_if_dirty(&conn)?;
    }
    Ok(conn)
}
