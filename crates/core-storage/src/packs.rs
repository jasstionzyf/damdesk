//! L0 `core-storage::packs` — 行业包身份账本（migration 0026，IP-K05）。
//!
//! 存储解耦铁律（PLAN.md §2.1 / 用户裁决 2026-10-16）：**trait 定义与 SQL 实现分离**。
//! 调用方（app-web 的插件运行时、后续安装/回滚流程）只依赖 [`PackRepo`]，
//! 禁止直接拼 SQL。PG 化 = 新增平行 impl，业务层零改动。
//!
//! 语义（与 0026 注释一致，两处必须同时改）：
//! * 主键 `(id, version)`：同 id 多版本并存，升级不删旧行（历史判定的 `packVersion` 要能对上）；
//! * `manifest_hash` = `manifest.json` 原文 sha256，与判定行 `pluginHash` 逐字相同；
//! * `current` = 该 id 当前生效版本，**一个 id 只能有一个**（DB 唯一索引兜底）；
//! * `installed_at` 只在首次登记时写，重复登记不覆盖（它是「何时进过这个库」的审计事实）。

use rusqlite::Connection;

use crate::Result;

/// 包登记行（= `packs` 表一行的可移植形态）。
#[derive(Debug, Clone, PartialEq)]
pub struct PackRecord {
    pub id: String,
    pub version: String,
    /// 包目录绝对路径（排障时定位实物）
    pub dir: String,
    /// `manifest.json` 原文 sha256
    pub manifest_hash: String,
    pub api_version: String,
    pub deterministic: bool,
    /// `unsigned` | `declared` | `verified` | `invalid`（v1 只写前两种）
    pub signature_status: String,
    /// 插件种类（manifest `kinds` 的规范化形态，0028）：`judge` | `process` | `judge,process`
    /// （逗号分隔、固定顺序）。宿主据此决定有没有审核列/审核页 —— **不靠猜**，
    /// 也不在渲染期现读 manifest（包目录被删/被换掉时判定仍要能按安装时的事实渲染）。
    pub kinds: String,
    /// unix 秒（首次登记时间）
    pub installed_at: i64,
    /// 该 id 当前生效版本
    pub current: bool,
}

/// 包登记仓库。
pub trait PackRepo {
    /// 登记/更新一个包版本行（同 `(id, version)` 幂等；`installed_at` 保持首次值）。
    /// 返回该行是否**新建**（true = 之前不存在）。
    fn upsert(&self, rec: &PackRecord) -> Result<bool>;
    /// 把某 id 的生效版本切到 `version`（事务内先全置 0 再置 1）。
    /// 目标版本未登记 → 返回 0（调用方必须当错误处理，不许切到不存在的版本）。
    fn set_current(&self, id: &str, version: &str) -> Result<usize>;
    /// 全量列出（按 id, version 排序）。
    fn list(&self) -> Result<Vec<PackRecord>>;
    /// 取某 id 的生效版本（无 → None）。
    fn current(&self, id: &str) -> Result<Option<PackRecord>>;
}

/// SQLite 实现（方言专有：`ON CONFLICT` / `unixepoch` 兜底值只在 impl 内出现）。
pub struct SqlitePackRepo<'a> {
    conn: &'a Connection,
}

impl<'a> SqlitePackRepo<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }
}

const COLS: &str = "id, version, dir, manifest_hash, api_version, deterministic, \
                    signature_status, kinds, installed_at, current";

fn row_to_record(r: &rusqlite::Row<'_>) -> rusqlite::Result<PackRecord> {
    Ok(PackRecord {
        id: r.get(0)?,
        version: r.get(1)?,
        dir: r.get(2)?,
        manifest_hash: r.get(3)?,
        api_version: r.get(4)?,
        deterministic: r.get::<_, i64>(5)? != 0,
        signature_status: r.get(6)?,
        kinds: r.get(7)?,
        installed_at: r.get(8)?,
        current: r.get::<_, i64>(9)? != 0,
    })
}

impl PackRepo for SqlitePackRepo<'_> {
    fn upsert(&self, rec: &PackRecord) -> Result<bool> {
        let existed: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM packs WHERE id = ?1 AND version = ?2",
                rusqlite::params![rec.id, rec.version],
                |_| Ok(true),
            )
            .ok()
            .unwrap_or(false);
        self.conn.execute(
            "INSERT INTO packs
                 (id, version, dir, manifest_hash, api_version, deterministic,
                  signature_status, kinds, installed_at, current)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id, version) DO UPDATE SET
                 dir = excluded.dir,
                 manifest_hash = excluded.manifest_hash,
                 api_version = excluded.api_version,
                 deterministic = excluded.deterministic,
                 signature_status = excluded.signature_status,
                 -- kinds 随包内容走：同版本重装若改了 kinds，安装记录必须跟着变
                 kinds = excluded.kinds,
                 current = excluded.current",
            rusqlite::params![
                rec.id,
                rec.version,
                rec.dir,
                rec.manifest_hash,
                rec.api_version,
                i64::from(rec.deterministic),
                rec.signature_status,
                rec.kinds,
                rec.installed_at,
                i64::from(rec.current),
            ],
        )?;
        Ok(!existed)
    }

    fn set_current(&self, id: &str, version: &str) -> Result<usize> {
        // 目标必须已登记（否则「切指针」会静默变成「没有生效版本」）
        let exists: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM packs WHERE id = ?1 AND version = ?2",
                rusqlite::params![id, version],
                |_| Ok(true),
            )
            .ok()
            .unwrap_or(false);
        if !exists {
            return Ok(0);
        }
        self.conn
            .execute("UPDATE packs SET current = 0 WHERE id = ?1", [id])?;
        Ok(self.conn.execute(
            "UPDATE packs SET current = 1 WHERE id = ?1 AND version = ?2",
            rusqlite::params![id, version],
        )?)
    }

    fn list(&self) -> Result<Vec<PackRecord>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {COLS} FROM packs ORDER BY id, version"))?;
        let rows = stmt.query_map([], |r| row_to_record(r))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn current(&self, id: &str) -> Result<Option<PackRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM packs WHERE id = ?1 AND current = 1"
        ))?;
        let mut rows = stmt.query_map([id], |r| row_to_record(r))?;
        Ok(match rows.next() {
            Some(r) => Some(r?),
            None => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        crate::migrations::migrate(&c).unwrap();
        c
    }

    fn rec(id: &str, ver: &str, current: bool) -> PackRecord {
        PackRecord {
            id: id.into(),
            version: ver.into(),
            dir: format!("/packs/{id}/{ver}"),
            manifest_hash: format!("hash-{ver}"),
            api_version: "1.1".into(),
            deterministic: false,
            signature_status: "unsigned".into(),
            kinds: "judge".into(),
            installed_at: 100,
            current,
        }
    }

    #[test]
    fn upsert_is_idempotent_and_keeps_first_installed_at() {
        let c = conn();
        let repo = SqlitePackRepo::new(&c);
        assert!(repo.upsert(&rec("com.soujpg.pack.ecommerce", "1.0.0", true)).unwrap());
        let mut again = rec("com.soujpg.pack.ecommerce", "1.0.0", true);
        again.installed_at = 999; // 重复登记不得覆盖首次时间
        assert!(!repo.upsert(&again).unwrap(), "第二次必须是更新而非新建");
        let rows = repo.list().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].installed_at, 100);
    }

    #[test]
    fn two_versions_coexist_and_current_only_one() {
        let c = conn();
        let repo = SqlitePackRepo::new(&c);
        repo.upsert(&rec("p", "1.0.0", true)).unwrap();
        repo.upsert(&rec("p", "1.1.0", false)).unwrap();
        assert_eq!(repo.list().unwrap().len(), 2, "多版本必须并存（回滚=切指针）");
        assert_eq!(repo.current("p").unwrap().unwrap().version, "1.0.0");
        assert_eq!(repo.set_current("p", "1.1.0").unwrap(), 1);
        assert_eq!(repo.current("p").unwrap().unwrap().version, "1.1.0");
        let flags: Vec<bool> = repo.list().unwrap().iter().map(|r| r.current).collect();
        assert_eq!(flags.iter().filter(|c| **c).count(), 1, "一个 id 只能有一个 current");
    }

    #[test]
    fn set_current_to_unknown_version_is_zero_not_silent() {
        let c = conn();
        let repo = SqlitePackRepo::new(&c);
        repo.upsert(&rec("p", "1.0.0", true)).unwrap();
        assert_eq!(repo.set_current("p", "9.9.9").unwrap(), 0);
        // 原生效版本不受影响
        assert_eq!(repo.current("p").unwrap().unwrap().version, "1.0.0");
    }

    #[test]
    fn unique_index_blocks_two_current_versions_of_same_id() {
        let c = conn();
        let repo = SqlitePackRepo::new(&c);
        repo.upsert(&rec("p", "1.0.0", true)).unwrap();
        // 绕过 set_current 直接插第二条 current=1 → 必须被唯一索引拒绝
        let err = repo.upsert(&rec("p", "2.0.0", true)).unwrap_err();
        assert!(
            err.to_string().contains("UNIQUE") || err.to_string().contains("constraint"),
            "同一 id 两个 current 必须被 DB 拒绝：{err}"
        );
    }
}
