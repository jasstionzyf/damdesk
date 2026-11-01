//! L0 `core-storage::specs` — 规格实体（migration 0029，卡 t_1cde30f2）。
//!
//! 存储解耦铁律（PLAN.md §2.1 / 用户裁决 2026-10-16）：**trait 定义与 SQL 实现分离**。
//! 调用方（app-web 的 `/api/specs`、judge 的 `specId`）只依赖 [`SpecRepo`]，
//! 禁止直接拼 SQL。PG 化 = 新增平行 impl，业务层零改动。
//!
//! 语义（与 0029 注释一致，两处必须同时改）：
//! * 规格是**配置**：不安装、不分发、不拥有图片；卸载插件**不删规格**；
//! * `params_diff` 只存 diff，默认值唯一定义在插件 resources；
//! * `thresholds` 是规则级阈值原文（旧行业包迁移过来的那批值），**不许静默丢**；
//! * `origin_key`（`legacy-pack:<id>`）带唯一索引 → 迁移幂等；
//! * `updated_at` 由写路径刷新，`created_at` 只在首次插入写。

use rusqlite::Connection;
use serde_json::Value;

use crate::Result;

/// 规格行（= `plugin_spec` 表一行的可移植形态）。
///
/// JSON 列（`rules` / `thresholds` / `actions` / `output` / `params_diff`）在这里
/// 已经是**解析后的值**：调用方不该再关心 SQLite 里存的是文本。
#[derive(Debug, Clone, PartialEq)]
pub struct SpecRecord {
    pub spec_id: i64,
    pub plugin_id: String,
    /// `Some` = 绑定具体插件版本；`None` = 跟随当前生效版本
    pub plugin_version_pin: Option<String>,
    pub name: String,
    /// 行业标记（`ecommerce` / `realestate` / 自由文本；NULL = 未标）
    pub industry: Option<String>,
    /// 选哪些规则（空 = 插件全部规则）
    pub rules: Vec<String>,
    /// 规则级阈值：`{ruleId: {key: value}}`（旧 YAML 原文的同构保真）
    pub thresholds: Value,
    /// 处理类动作链（有序）
    pub actions: Value,
    /// 输出设置
    pub output: Value,
    /// 插件 schema 键的参数 diff（只存改过的键）
    pub params_diff: Value,
    pub enabled: bool,
    /// `legacy-pack:<packId>` = 迁移来源；`None` = 用户自建
    pub origin_key: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl SpecRecord {
    /// 插件身份 + 版本绑定的**冲突判定**材料（读接口据此算 `pluginStatus`）。
    ///
    /// 只做「钉了版本而实际版本不同」这一条，别的状态（包在不在）由调用方按包目录解析。
    pub fn version_mismatch(&self, installed: &str) -> bool {
        match self.plugin_version_pin.as_deref() {
            Some(pin) => pin != installed,
            None => false,
        }
    }
}

/// 新建规格的入参（`spec_id` / 时间戳由库给）。
#[derive(Debug, Clone, PartialEq)]
pub struct NewSpec {
    pub plugin_id: String,
    pub plugin_version_pin: Option<String>,
    pub name: String,
    pub industry: Option<String>,
    pub rules: Vec<String>,
    pub thresholds: Value,
    pub actions: Value,
    pub output: Value,
    pub params_diff: Value,
    pub enabled: bool,
    pub origin_key: Option<String>,
}

/// 局部更新（`None` = 该列不动；规则/动作链想清空时传空数组而不是 `None`）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpecPatch {
    pub name: Option<String>,
    pub industry: Option<Option<String>>,
    pub plugin_version_pin: Option<Option<String>>,
    pub rules: Option<Vec<String>>,
    pub thresholds: Option<Value>,
    pub actions: Option<Value>,
    pub output: Option<Value>,
    pub params_diff: Option<Value>,
    pub enabled: Option<bool>,
}

impl SpecPatch {
    pub fn is_empty(&self) -> bool {
        self == &SpecPatch::default()
    }
}

/// 规格仓库。
pub trait SpecRepo {
    /// 新建。同名/同 `origin_key` 冲突 → `StorageError::Invalid`（调用方翻成 400）。
    fn create(&self, new: &NewSpec) -> Result<i64>;
    /// 按 `origin_key` 幂等插入：已存在 → 返回 `(id, false)` 且**不改动**既有行
    /// （迁移跑第二遍时不许覆盖用户后来改过的阈值）。
    fn insert_if_absent(&self, new: &NewSpec) -> Result<(i64, bool)>;
    fn get(&self, spec_id: i64) -> Result<Option<SpecRecord>>;
    /// 列出规格；`plugin` 为 `None` = 全量（按 pluginId, specId 排序）。
    fn list(&self, plugin: Option<&str>) -> Result<Vec<SpecRecord>>;
    fn find_by_origin(&self, origin_key: &str) -> Result<Option<SpecRecord>>;
    /// 局部更新；行不存在 → `false`（调用方翻成 404），无字段变化 → 仍刷新 `updated_at`。
    fn update(&self, spec_id: i64, patch: &SpecPatch) -> Result<bool>;
    /// 删除；行不存在 → `false`。**只删规格行**：verdict / 产物 / 插件包一律不动。
    fn delete(&self, spec_id: i64) -> Result<bool>;
}

/// SQLite 实现（方言专有：`ON CONFLICT` / `unixepoch` 只在 impl 内出现）。
pub struct SqliteSpecRepo<'a> {
    conn: &'a Connection,
}

impl<'a> SqliteSpecRepo<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }
}

const COLS: &str = "specId, pluginId, pluginVersionPin, name, industry, rules, thresholds, \
                    actions, output, paramsDiff, enabled, originKey, createdAt, updatedAt";

fn row_to_record(r: &rusqlite::Row<'_>) -> rusqlite::Result<SpecRecord> {
    let spec_id: i64 = r.get(0)?;
    let rules_raw: String = r.get(5)?;
    let thresholds_raw: String = r.get(6)?;
    let actions_raw: String = r.get(7)?;
    let output_raw: String = r.get(8)?;
    let diff_raw: String = r.get(9)?;
    // 行内解析失败要能被上层看见：转成 sqlite 错误带列名（外面会包一层 StorageError::Sqlite）
    let parse = |raw: &str, col: &str| -> rusqlite::Result<Value> {
        serde_json::from_str(raw).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other(format!(
                    "plugin_spec#{spec_id}.{col}: {e}"
                ))),
            )
        })
    };
    let rules: Vec<String> = match parse(&rules_raw, "rules")? {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    Ok(SpecRecord {
        spec_id,
        plugin_id: r.get(1)?,
        plugin_version_pin: r.get(2)?,
        name: r.get(3)?,
        industry: r.get(4)?,
        rules,
        thresholds: parse(&thresholds_raw, "thresholds")?,
        actions: parse(&actions_raw, "actions")?,
        output: parse(&output_raw, "output")?,
        params_diff: parse(&diff_raw, "paramsDiff")?,
        enabled: r.get::<_, i64>(10)? != 0,
        origin_key: r.get(11)?,
        created_at: r.get(12)?,
        updated_at: r.get(13)?,
    })
}

impl SqliteSpecRepo<'_> {
    fn insert(&self, new: &NewSpec) -> Result<i64> {
        let rules = serde_json::to_string(&new.rules).map_err(|e| self.bad(e.to_string()))?;
        let thresholds = serde_json::to_string(&new.thresholds).map_err(|e| self.bad(e.to_string()))?;
        let actions = serde_json::to_string(&new.actions).map_err(|e| self.bad(e.to_string()))?;
        let output = serde_json::to_string(&new.output).map_err(|e| self.bad(e.to_string()))?;
        let diff = serde_json::to_string(&new.params_diff).map_err(|e| self.bad(e.to_string()))?;
        self.conn
            .execute(
                "INSERT INTO plugin_spec (pluginId, pluginVersionPin, name, industry, rules,
                                          thresholds, actions, output, paramsDiff, enabled,
                                          originKey, createdAt, updatedAt)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, unixepoch(), unixepoch())",
                rusqlite::params![
                    new.plugin_id,
                    new.plugin_version_pin,
                    new.name,
                    new.industry,
                    rules,
                    thresholds,
                    actions,
                    output,
                    diff,
                    i64::from(new.enabled),
                    new.origin_key,
                ],
            )
            .map_err(|e| self.conflict_aware(e))?;
        Ok(self.conn.last_insert_rowid())
    }

    fn bad(&self, msg: String) -> crate::StorageError {
        crate::StorageError::Other(msg)
    }

    /// 唯一索引冲突翻成人读错误（调用方直接 400，不让 SQL 方言漏到界面）。
    fn conflict_aware(&self, e: rusqlite::Error) -> crate::StorageError {
        let msg = e.to_string();
        if msg.contains("UNIQUE constraint failed") {
            if msg.contains("plugin_spec.originKey") {
                return crate::StorageError::Invalid(format!("该迁移来源已有规格（originKey 重复）：{msg}"));
            }
            return crate::StorageError::Invalid(format!("同一插件下已有同名规格：{msg}"));
        }
        crate::StorageError::Sqlite(e)
    }
}

impl SpecRepo for SqliteSpecRepo<'_> {
    fn create(&self, new: &NewSpec) -> Result<i64> {
        self.insert(new)
    }

    fn insert_if_absent(&self, new: &NewSpec) -> Result<(i64, bool)> {
        if let Some(key) = new.origin_key.as_deref() {
            if let Some(existing) = self.find_by_origin(key)? {
                return Ok((existing.spec_id, false));
            }
        }
        // 同名规格已存在（用户先手建过同名）→ 迁移不再插第二条，返回既有行（幂等优先）
        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT specId FROM plugin_spec WHERE pluginId = ?1 AND name = ?2",
                rusqlite::params![new.plugin_id, new.name],
                |r| r.get(0),
            )
            .ok();
        if let Some(id) = existing {
            return Ok((id, false));
        }
        Ok((self.insert(new)?, true))
    }

    fn get(&self, spec_id: i64) -> Result<Option<SpecRecord>> {
        let sql = format!("SELECT {COLS} FROM plugin_spec WHERE specId = ?1");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(rusqlite::params![spec_id])?;
        match rows.next()? {
            Some(r) => Ok(Some(row_to_record(r)?)),
            None => Ok(None),
        }
    }

    fn list(&self, plugin: Option<&str>) -> Result<Vec<SpecRecord>> {
        let mut out = Vec::new();
        match plugin {
            Some(pid) => {
                let sql = format!(
                    "SELECT {COLS} FROM plugin_spec WHERE pluginId = ?1 ORDER BY specId"
                );
                let mut stmt = self.conn.prepare(&sql)?;
                let mut rows = stmt.query(rusqlite::params![pid])?;
                while let Some(r) = rows.next()? {
                    out.push(row_to_record(r)?);
                }
            }
            None => {
                let sql = format!("SELECT {COLS} FROM plugin_spec ORDER BY pluginId, specId");
                let mut stmt = self.conn.prepare(&sql)?;
                let mut rows = stmt.query([])?;
                while let Some(r) = rows.next()? {
                    out.push(row_to_record(r)?);
                }
            }
        }
        Ok(out)
    }

    fn find_by_origin(&self, origin_key: &str) -> Result<Option<SpecRecord>> {
        let sql = format!("SELECT {COLS} FROM plugin_spec WHERE originKey = ?1");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(rusqlite::params![origin_key])?;
        match rows.next()? {
            Some(r) => Ok(Some(row_to_record(r)?)),
            None => Ok(None),
        }
    }

    fn update(&self, spec_id: i64, patch: &SpecPatch) -> Result<bool> {
        // 逐列拼 SET：只写真的给了的列（`None` = 不动），JSON 列统一序列化
        let mut sets: Vec<&str> = Vec::new();
        let mut vals: Vec<rusqlite::types::Value> = Vec::new();
        let push_json = |sets: &mut Vec<&'static str>,
                             vals: &mut Vec<rusqlite::types::Value>,
                             col: &'static str,
                             v: &Value|
         -> Result<()> {
            let s = serde_json::to_string(v).map_err(|e| self.bad(e.to_string()))?;
            sets.push(col);
            vals.push(rusqlite::types::Value::Text(s));
            Ok(())
        };
        if let Some(name) = &patch.name {
            sets.push("name");
            vals.push(rusqlite::types::Value::Text(name.clone()));
        }
        if let Some(ind) = &patch.industry {
            sets.push("industry");
            vals.push(match ind {
                Some(v) => rusqlite::types::Value::Text(v.clone()),
                None => rusqlite::types::Value::Null,
            });
        }
        if let Some(pin) = &patch.plugin_version_pin {
            sets.push("pluginVersionPin");
            vals.push(match pin {
                Some(v) => rusqlite::types::Value::Text(v.clone()),
                None => rusqlite::types::Value::Null,
            });
        }
        if let Some(rules) = &patch.rules {
            let s = serde_json::to_string(rules).map_err(|e| self.bad(e.to_string()))?;
            sets.push("rules");
            vals.push(rusqlite::types::Value::Text(s));
        }
        if let Some(v) = &patch.thresholds {
            push_json(&mut sets, &mut vals, "thresholds", v)?;
        }
        if let Some(v) = &patch.actions {
            push_json(&mut sets, &mut vals, "actions", v)?;
        }
        if let Some(v) = &patch.output {
            push_json(&mut sets, &mut vals, "output", v)?;
        }
        if let Some(v) = &patch.params_diff {
            push_json(&mut sets, &mut vals, "paramsDiff", v)?;
        }
        if let Some(en) = patch.enabled {
            sets.push("enabled");
            vals.push(rusqlite::types::Value::Integer(i64::from(en)));
        }
        // `updatedAt` 用 SQL 表达式而不是绑定值：**必须严格大于 createdAt**
        // （同一秒内插入又更新时，`unixepoch()` 会等于 createdAt，而「createdAt == updatedAt」
        // 正是迁移回读校验用来判断「这条规格还是种子原样、没被用户动过」的依据 ——
        // 时间戳撞秒会让用户改过的规格被当成种子去比对源值，报出假事故）。
        let mut assignments: Vec<String> = sets
            .iter()
            .enumerate()
            .map(|(i, col)| format!("{col} = ?{}", i + 1))
            .collect();
        assignments.push("updatedAt = MAX(updatedAt + 1, unixepoch())".to_string());
        let assignments = assignments.join(", ");
        let sql = format!("UPDATE plugin_spec SET {assignments} WHERE specId = ?{}", vals.len() + 1);
        vals.push(rusqlite::types::Value::Integer(spec_id));
        let n = self
            .conn
            .execute(&sql, rusqlite::params_from_iter(vals.iter()))
            .map_err(|e| self.conflict_aware(e))?;
        Ok(n > 0)
    }

    fn delete(&self, spec_id: i64) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM plugin_spec WHERE specId = ?1", rusqlite::params![spec_id])?;
        Ok(n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::migrations::migrate(&conn).unwrap();
        conn
    }

    fn sample(name: &str, origin: Option<&str>) -> NewSpec {
        NewSpec {
            plugin_id: "com.soujpg.pack.ecommerce".into(),
            plugin_version_pin: None,
            name: name.into(),
            industry: Some("ecommerce".into()),
            rules: vec!["R1_size".into()],
            thresholds: json!({"R1_size": {"min": 700}}),
            actions: json!([]),
            output: json!({}),
            params_diff: json!({"shortSideMin": 700}),
            enabled: true,
            origin_key: origin.map(str::to_string),
        }
    }

    #[test]
    fn create_get_list_roundtrip() {
        let conn = db();
        let repo = SqliteSpecRepo::new(&conn);
        let id = repo.create(&sample("淘宝主图", None)).unwrap();
        let got = repo.get(id).unwrap().unwrap();
        assert_eq!(got.name, "淘宝主图");
        assert_eq!(got.rules, vec!["R1_size".to_string()]);
        assert_eq!(got.thresholds, json!({"R1_size": {"min": 700}}));
        assert_eq!(got.params_diff, json!({"shortSideMin": 700}));
        assert!(got.enabled);
        assert_eq!(repo.list(Some("com.soujpg.pack.ecommerce")).unwrap().len(), 1);
        assert!(repo.list(Some("other")).unwrap().is_empty());
        assert_eq!(repo.list(None).unwrap().len(), 1);
    }

    #[test]
    fn duplicate_name_under_same_plugin_is_rejected() {
        let conn = db();
        let repo = SqliteSpecRepo::new(&conn);
        repo.create(&sample("淘宝主图", None)).unwrap();
        let e = repo.create(&sample("淘宝主图", None)).unwrap_err();
        assert!(matches!(e, crate::StorageError::Invalid(_)), "{e}");
        // 别的插件同名可以（规格名是插件内唯一）
        let mut other = sample("淘宝主图", None);
        other.plugin_id = "realestate".into();
        repo.create(&other).unwrap();
    }

    #[test]
    fn insert_if_absent_is_idempotent_and_never_overwrites() {
        let conn = db();
        let repo = SqliteSpecRepo::new(&conn);
        let (id1, created1) = repo
            .insert_if_absent(&sample("淘宝主图", Some("legacy-pack:ecommerce-taobao")))
            .unwrap();
        assert!(created1);
        // 用户后来改了阈值
        repo.update(
            id1,
            &SpecPatch {
                thresholds: Some(json!({"R1_size": {"min": 999}})),
                ..Default::default()
            },
        )
        .unwrap();
        // 迁移再跑一遍：不新建、不覆盖
        let (id2, created2) = repo
            .insert_if_absent(&sample("淘宝主图", Some("legacy-pack:ecommerce-taobao")))
            .unwrap();
        assert_eq!((id1, created1), (id2, created1));
        assert!(!created2);
        assert_eq!(
            repo.get(id1).unwrap().unwrap().thresholds,
            json!({"R1_size": {"min": 999}})
        );
    }

    #[test]
    fn patch_only_touches_given_columns() {
        let conn = db();
        let repo = SqliteSpecRepo::new(&conn);
        let id = repo.create(&sample("淘宝主图", None)).unwrap();
        repo.update(
            id,
            &SpecPatch {
                enabled: Some(false),
                industry: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
        let got = repo.get(id).unwrap().unwrap();
        assert!(!got.enabled);
        assert_eq!(got.industry, None);
        // 没给的列保持原值
        assert_eq!(got.thresholds, json!({"R1_size": {"min": 700}}));
        assert_eq!(got.rules, vec!["R1_size".to_string()]);
    }

    #[test]
    fn delete_removes_only_the_spec_row() {
        let conn = db();
        let repo = SqliteSpecRepo::new(&conn);
        let id = repo.create(&sample("淘宝主图", None)).unwrap();
        assert!(repo.delete(id).unwrap());
        assert!(repo.get(id).unwrap().is_none());
        assert!(!repo.delete(id).unwrap());
    }

    #[test]
    fn update_strictly_bumps_updated_at() {
        let conn = db();
        let repo = SqliteSpecRepo::new(&conn);
        let id = repo.create(&sample("淘宝主图", None)).unwrap();
        let before = repo.get(id).unwrap().unwrap();
        assert_eq!(before.created_at, before.updated_at, "新行：两个时间戳相等 = 未被编辑");
        repo.update(
            id,
            &SpecPatch {
                name: Some("淘宝主图 v2".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let after = repo.get(id).unwrap().unwrap();
        assert!(
            after.updated_at > after.created_at,
            "PATCH 后 updatedAt 必须严格大于 createdAt（迁移回读靠它区分「种子原样」与「用户改过」）"
        );
    }

    #[test]
    fn version_pin_mismatch_is_reported() {
        let conn = db();
        let repo = SqliteSpecRepo::new(&conn);
        let mut new = sample("淘宝主图", None);
        new.plugin_version_pin = Some("1.3.0".into());
        let id = repo.create(&new).unwrap();
        let got = repo.get(id).unwrap().unwrap();
        assert!(!got.version_mismatch("1.3.0"));
        assert!(got.version_mismatch("2.0.0"));
        // 未钉版本 = 跟随当前版本，永远不算不匹配
        let mut follow = sample("亚马逊主图", None);
        follow.plugin_version_pin = None;
        let id2 = repo.create(&follow).unwrap();
        assert!(!repo.get(id2).unwrap().unwrap().version_mismatch("9.9.9"));
    }
}
