//! L0 `core-storage::settings` — settings 表（key → json value）读写。
//! 全部推理/打标配置落这里（PLAN.md §9 单机零配置：默认值来自代码，用户改动才落库）。

use rusqlite::{Connection, OptionalExtension};

use crate::{Result, StorageError};

/// 读原始 json 文本。
pub fn get_raw(conn: &Connection, key: &str) -> Result<Option<String>> {
    let v = conn
        .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
            r.get::<_, String>(0)
        })
        .optional()?;
    Ok(v)
}

/// 写原始 json 文本（upsert）。
pub fn set_raw(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value, updatedAt) VALUES (?1, ?2, unixepoch())
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updatedAt = excluded.updatedAt",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

/// 读并反序列化（缺失或占位 'null' → None）。
pub fn get_json<T: serde::de::DeserializeOwned>(conn: &Connection, key: &str) -> Result<Option<T>> {
    match get_raw(conn, key)? {
        None => Ok(None),
        Some(s) if s.trim().is_empty() || s.trim() == "null" => Ok(None),
        Some(s) => serde_json::from_str(&s)
            .map(Some)
            .map_err(|e| StorageError::Migration {
                version: 0,
                name: format!("settings:{key}"),
                msg: format!("invalid json: {e}"),
            }),
    }
}

/// 序列化后写入。
pub fn set_json<T: serde::Serialize>(conn: &Connection, key: &str, v: &T) -> Result<()> {
    let s = serde_json::to_string(v).map_err(|e| StorageError::Migration {
        version: 0,
        name: format!("settings:{key}"),
        msg: format!("serialize: {e}"),
    })?;
    set_raw(conn, key, &s)
}

/// 读 + 应用默认值（缺失时返回 default，不落库）。
pub fn get_or_default<T: serde::de::DeserializeOwned + Default>(
    conn: &Connection,
    key: &str,
) -> Result<T> {
    Ok(get_json(conn, key)?.unwrap_or_default())
}

/// 全部设置（诊断用）。
pub fn all(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT key, value FROM settings ORDER BY key")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        crate::migrations::migrate(&c).unwrap();
        c
    }

    #[test]
    fn raw_roundtrip_and_upsert() {
        let c = conn();
        assert_eq!(get_raw(&c, "inference").unwrap(), Some("null".into()));
        set_raw(&c, "inference", r#"{"a":1}"#).unwrap();
        assert_eq!(get_raw(&c, "inference").unwrap(), Some(r#"{"a":1}"#.into()));
        set_raw(&c, "inference", r#"{"a":2}"#).unwrap();
        assert_eq!(get_raw(&c, "inference").unwrap(), Some(r#"{"a":2}"#.into()));
    }

    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize, Default)]
    struct T {
        a: u32,
        b: String,
    }

    #[test]
    fn json_roundtrip_placeholder_and_default() {
        let c = conn();
        // 占位 'null' → None → default
        assert_eq!(get_json::<T>(&c, "inference").unwrap(), None);
        assert_eq!(get_or_default::<T>(&c, "inference").unwrap(), T::default());
        let v = T {
            a: 7,
            b: "x".into(),
        };
        set_json(&c, "inference", &v).unwrap();
        assert_eq!(get_json::<T>(&c, "inference").unwrap(), Some(v));
        assert!(get_json::<T>(&c, "seed_missing_key").unwrap().is_none());
    }
}
