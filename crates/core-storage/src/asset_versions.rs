//! L0 `core-storage::asset_versions` — 资产**工作版本**账本（migration 0029，N6e `t_b6c2aa1e`）。
//!
//! 一句话：宿主侧动作链的落库出口。一次链 = **一行结果**（ok/failed）+ 至多**一张工作版本**。
//!
//! 存储解耦铁律（PLAN.md §2.1）：trait 与 SQL 实现分离 —— 调用方（`svc-pipeline` 的链执行）
//! 只依赖 [`AssetVersionRepo`]，禁止拼 SQL。PG 化 = 新增平行 impl，业务层零改动。
//!
//! 三条硬口径（与 0029 SQL 注释、`plugin-result-contract.md` 一一对应）：
//! 1. **ok 行才可能是工作版本**：`insert` 里把同资产旧的 `current` 全置 0，再插新行；
//!    失败行永远 `current = 0`（不留半成品）；
//! 2. **一资产一张**：`uq_asset_versions_current` 是部分唯一索引（`WHERE current = 1`），
//!    代码写错也插不进第二张 —— 约束落在 DB，不靠自觉；
//! 3. **历史不删**：重跑/换动作生成的是新行，旧行降级为「历史版本」（`current = 0`）。

use rusqlite::Connection;

use crate::Result;

/// 一行链结果（= `asset_versions` 表一行的可移植形态）。
#[derive(Debug, Clone, PartialEq)]
pub struct AssetVersionRecord {
    pub asset_id: i64,
    pub job_id: Option<i64>,
    pub run_id: Option<i64>,
    /// 插件包 id（`pack.info.id`）
    pub pack_id: String,
    pub pack_version: String,
    /// `manifest.json` 原文 sha256（与判定来源列同口径）
    pub plugin_hash: String,
    /// 动作链（顺序即执行序）
    pub actions: Vec<String>,
    /// `ok` | `failed`
    pub result: String,
    /// 失败码（`failed` 时必有；指向失败动作）
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    /// 产物（仅 `ok` 有）：链尾动作的产物路径
    pub path: Option<String>,
    pub sha256: Option<String>,
    pub bytes: Option<i64>,
    pub format: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// 链尾 `rename` 的资产名改写（无 rename = None）
    pub asset_name: Option<String>,
    /// 逐步证据（JSON 数组：`[{action,result,op,ms,outPath,sha256}]`）
    pub chain: serde_json::Value,
    pub duration_ms: i64,
    /// unix 秒
    pub created_at: i64,
}

impl AssetVersionRecord {
    pub fn new(
        asset_id: i64,
        pack_id: impl Into<String>,
        actions: Vec<String>,
        chain: serde_json::Value,
    ) -> Self {
        Self {
            asset_id,
            job_id: None,
            run_id: None,
            pack_id: pack_id.into(),
            pack_version: String::new(),
            plugin_hash: String::new(),
            actions,
            result: "ok".to_string(),
            error_code: None,
            error_message: None,
            path: None,
            sha256: None,
            bytes: None,
            format: None,
            width: None,
            height: None,
            asset_name: None,
            chain,
            duration_ms: 0,
            created_at: 0,
        }
    }
}

/// 读出来的行（带 `version_id` / `current`）。
#[derive(Debug, Clone, PartialEq)]
pub struct StoredVersion {
    pub version_id: i64,
    pub record: AssetVersionRecord,
    /// 该资产当前工作版本
    pub current: bool,
}

/// 工作版本仓库。
pub trait AssetVersionRepo {
    /// 落一行链结果。`result='ok'` 时事务内先把同资产旧的 `current` 置 0 再插新行
    /// （= 新产物顶替旧工作版本）；返回新行 `versionId`。
    fn insert(&self, rec: &AssetVersionRecord) -> Result<i64>;
    /// 该资产当前工作版本（无 → None）。
    fn current_working(&self, asset_id: i64) -> Result<Option<StoredVersion>>;
    /// 该资产的全部链结果行（新 → 旧；含失败行与历史版本）。
    fn list_for_asset(&self, asset_id: i64) -> Result<Vec<StoredVersion>>;
}

/// SQLite 实现（方言专有：`AUTOINCREMENT` / 部分唯一索引只在 0029 + 本 impl 出现）。
pub struct SqliteAssetVersionRepo<'a> {
    conn: &'a Connection,
}

impl<'a> SqliteAssetVersionRepo<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }
}

const COLS: &str = "versionId, assetId, jobId, runId, packId, packVersion, pluginHash, \
                    actions, result, errorCode, errorMessage, path, sha256, bytes, format, \
                    width, height, assetName, chain, durationMs, createdAt, current";

fn row_to_stored(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredVersion> {
    let actions_raw: String = r.get(7)?;
    let chain_raw: String = r.get(18)?;
    Ok(StoredVersion {
        version_id: r.get(0)?,
        record: AssetVersionRecord {
            asset_id: r.get(1)?,
            job_id: r.get(2)?,
            run_id: r.get(3)?,
            pack_id: r.get(4)?,
            pack_version: r.get(5)?,
            plugin_hash: r.get(6)?,
            actions: serde_json::from_str(&actions_raw).unwrap_or_default(),
            result: r.get(8)?,
            error_code: r.get(9)?,
            error_message: r.get(10)?,
            path: r.get(11)?,
            sha256: r.get(12)?,
            bytes: r.get(13)?,
            format: r.get(14)?,
            width: r.get(15)?,
            height: r.get(16)?,
            asset_name: r.get(17)?,
            chain: serde_json::from_str(&chain_raw).unwrap_or(serde_json::Value::Null),
            duration_ms: r.get(19)?,
            created_at: r.get(20)?,
        },
        current: r.get::<_, i64>(21)? != 0,
    })
}

impl AssetVersionRepo for SqliteAssetVersionRepo<'_> {
    fn insert(&self, rec: &AssetVersionRecord) -> Result<i64> {
        let is_ok = rec.result == "ok";
        let actions = serde_json::to_string(&rec.actions)
            .map_err(|e| crate::StorageError::Other(e.to_string()))?;
        let chain = serde_json::to_string(&rec.chain)
            .map_err(|e| crate::StorageError::Other(e.to_string()))?;
        let tx = self.conn.unchecked_transaction()?;
        if is_ok {
            // 新工作版本顶替旧的：先降级再插入（同一事务，中途失败不会两张 current）
            tx.execute(
                "UPDATE asset_versions SET current = 0 WHERE assetId = ?1 AND current = 1",
                [rec.asset_id],
            )?;
        }
        tx.execute(
            "INSERT INTO asset_versions
               (assetId, jobId, runId, packId, packVersion, pluginHash, actions, result,
                errorCode, errorMessage, path, sha256, bytes, format, width, height,
                assetName, chain, durationMs, createdAt, current)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                     ?17, ?18, ?19, ?20, ?21)",
            rusqlite::params![
                rec.asset_id,
                rec.job_id,
                rec.run_id,
                rec.pack_id,
                rec.pack_version,
                rec.plugin_hash,
                actions,
                rec.result,
                rec.error_code,
                rec.error_message,
                rec.path,
                rec.sha256,
                rec.bytes,
                rec.format,
                rec.width,
                rec.height,
                rec.asset_name,
                chain,
                rec.duration_ms,
                rec.created_at,
                i64::from(is_ok),
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    fn current_working(&self, asset_id: i64) -> Result<Option<StoredVersion>> {
        let sql = format!(
            "SELECT {COLS} FROM asset_versions WHERE assetId = ?1 AND current = 1 LIMIT 1"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query([asset_id])?;
        match rows.next()? {
            Some(r) => Ok(Some(row_to_stored(r)?)),
            None => Ok(None),
        }
    }

    fn list_for_asset(&self, asset_id: i64) -> Result<Vec<StoredVersion>> {
        let sql = format!(
            "SELECT {COLS} FROM asset_versions WHERE assetId = ?1 \
             ORDER BY createdAt DESC, versionId DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([asset_id], row_to_stored)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().expect("内存库");
        crate::migrations::migrate(&conn).expect("迁移");
        conn.execute(
            "INSERT INTO assets (assetId, absPath, fileName, fileExt, fileSz, mtime, importedAt) \
             VALUES (1, '/tmp/a.jpg', 'a.jpg', 'jpg', 10, 0, 0)",
            [],
        )
        .unwrap();
        conn
    }

    fn ok_row(asset_id: i64, path: &str) -> AssetVersionRecord {
        let mut rec = AssetVersionRecord::new(
            asset_id,
            "com.soujpg.pack.imaging",
            vec!["resize".into(), "convert".into()],
            serde_json::json!([{ "action": "resize" }, { "action": "convert" }]),
        );
        rec.result = "ok".into();
        rec.path = Some(path.to_string());
        rec.sha256 = Some("deadbeef".repeat(8));
        rec.created_at = 1;
        rec
    }

    /// 新工作版本顶替旧工作版本：同资产恒只有一行 current=1（DB 唯一索引兜底）。
    #[test]
    fn insert_ok_supersedes_previous_current() {
        let conn = db();
        let repo = SqliteAssetVersionRepo::new(&conn);
        let a = repo.insert(&ok_row(1, "/out/v1.jpg")).unwrap();
        let b = repo.insert(&ok_row(1, "/out/v2.jpg")).unwrap();
        assert_ne!(a, b);
        let cur = repo.current_working(1).unwrap().expect("有当前版本");
        assert_eq!(cur.version_id, b);
        assert_eq!(cur.record.path.as_deref(), Some("/out/v2.jpg"));
        let all = repo.list_for_asset(1).unwrap();
        assert_eq!(all.len(), 2, "历史版本不删");
        assert_eq!(all.iter().filter(|v| v.current).count(), 1);
    }

    /// 失败行不留版本：current 恒 0，`current_working` 仍返回上一次成功的那张。
    #[test]
    fn failed_row_never_becomes_working_version() {
        let conn = db();
        let repo = SqliteAssetVersionRepo::new(&conn);
        let ok_id = repo.insert(&ok_row(1, "/out/v1.jpg")).unwrap();
        let mut bad = AssetVersionRecord::new(
            1,
            "com.soujpg.pack.imaging",
            vec!["resize".into(), "watermark".into()],
            serde_json::json!([{ "action": "resize", "result": "ok" }]),
        );
        bad.result = "failed".into();
        bad.error_code = Some("action_failed".into());
        bad.error_message = Some("watermark 失败".into());
        repo.insert(&bad).unwrap();
        let cur = repo.current_working(1).unwrap().unwrap();
        assert_eq!(cur.version_id, ok_id, "失败不动工作版本");
        let all = repo.list_for_asset(1).unwrap();
        assert_eq!(all.len(), 2, "失败行也是结果行（可复盘）");
        assert_eq!(all.iter().filter(|v| v.current).count(), 1);
    }

    /// 部分唯一索引是硬约束：绕过 repo 直插第二张 current=1 必须被拒。
    #[test]
    fn unique_index_blocks_second_current() {
        let conn = db();
        let repo = SqliteAssetVersionRepo::new(&conn);
        repo.insert(&ok_row(1, "/out/v1.jpg")).unwrap();
        let err = conn.execute(
            "INSERT INTO asset_versions (assetId, packId, actions, result, chain, createdAt, current) \
             VALUES (1, 'p', '[]', 'ok', '[]', 0, 1)",
            [],
        );
        assert!(err.is_err(), "同资产第二行 current=1 必须被唯一索引拒");
    }
}
