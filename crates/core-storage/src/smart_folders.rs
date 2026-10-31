//! E3 — 智能文件夹（保存搜索）存储（L0 DAO）。
//!
//! 职责边界：smart_folder 表 CRUD + 保存条件（query JSON）的白名单校验/归一化。
//! 结果查询不在本层：查看时由 svc-library list_assets 按条件现查（动态视图语义），
//! 前端把 query 反序列化为 AssetQuery 参数走既有 /api/library/assets 管道。
//!
//! query 契约（冻结）：JSON 对象，键白名单 = mediaType/missing/ext/tag/dirPrefix/
//! untagged/minRating/colorLabel；limit/offset 属分页不保存；未知键拒绝（400 语义）。

use core_types::SmartFolder;
use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::{Result, StorageError};

/// 允许出现在保存条件里的键 → 取值校验（返回归一化后的 Value）。
const ALLOWED_QUERY_KEYS: &[&str] = &[
    "mediaType",
    "missing",
    "ext",
    "tag",
    "dirPrefix",
    "untagged",
    "minRating",
    "colorLabel",
];

/// 颜色标签允许值（v1 固定 6 色，与前端展示契约一致）。
pub const COLOR_LABELS: &[&str] = &["red", "orange", "yellow", "green", "blue", "purple"];

/// 校验并归一化保存条件：白名单键 + 合法取值，输出紧凑 JSON（键序按白名单，稳定）。
/// 非对象 / 空对象 / 未知键 / 非法值 → Invalid（HTTP 400 直通）。
pub fn normalize_query(raw: &str) -> Result<String> {
    let v: Value = serde_json::from_str(raw)
        .map_err(|_| StorageError::Invalid("query 不是合法 JSON".into()))?;
    let obj = match v {
        Value::Object(m) => m,
        _ => return Err(StorageError::Invalid("query 必须是 JSON 对象".into())),
    };
    for k in obj.keys() {
        if !ALLOWED_QUERY_KEYS.contains(&k.as_str()) {
            return Err(StorageError::Invalid(format!("query 不支持的字段：{k}")));
        }
    }
    let mut out = Map::new();
    for key in ALLOWED_QUERY_KEYS {
        let Some(val) = obj.get(*key) else { continue };
        let norm = normalize_value(key, val)?;
        if !matches!(norm, Value::Null) {
            out.insert((*key).to_string(), norm);
        }
    }
    if out.is_empty() {
        return Err(StorageError::Invalid(
            "至少要有一个筛选条件（标签/颜色/评分/目录等）".into(),
        ));
    }
    serde_json::to_string(&Value::Object(out)).map_err(|e| StorageError::Invalid(e.to_string()))
}

fn normalize_value(key: &str, v: &Value) -> Result<Value> {
    let bad = |msg: &str| Err(StorageError::Invalid(format!("{key}: {msg}")));
    match key {
        "mediaType" | "ext" | "tag" | "dirPrefix" | "colorLabel" => match v {
            Value::String(s) if !s.trim().is_empty() => {
                if key == "colorLabel" && !COLOR_LABELS.contains(&s.as_str()) {
                    return bad("颜色只支持 red/orange/yellow/green/blue/purple");
                }
                Ok(Value::String(s.trim().to_string()))
            }
            _ => bad("应为非空字符串"),
        },
        "missing" | "minRating" => match v {
            Value::Number(n) => {
                let i = n
                    .as_i64()
                    .ok_or_else(|| StorageError::Invalid(format!("{key}: 应为整数")))?;
                match key {
                    "minRating" if (1..=5).contains(&i) => Ok(Value::from(i)),
                    "missing" if (0..=1).contains(&i) => Ok(Value::from(i)),
                    _ => bad("取值越界"),
                }
            }
            _ => bad("应为整数"),
        },
        "untagged" => match v {
            Value::Bool(b) => Ok(Value::Bool(*b)),
            _ => bad("应为布尔"),
        },
        _ => unreachable!("白名单外键已在入口拒绝"),
    }
}

fn validate_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(StorageError::Invalid("名称不能为空".into()));
    }
    if name.len() > 80 {
        return Err(StorageError::Invalid("名称过长（≤80 字符）".into()));
    }
    Ok(name.to_string())
}

fn row_to_folder(r: &rusqlite::Row<'_>) -> rusqlite::Result<SmartFolder> {
    Ok(SmartFolder {
        id: r.get(0)?,
        name: r.get(1)?,
        query: r.get(2)?,
        createdAt: r.get(3)?,
        updatedAt: r.get(4)?,
    })
}

/// 全量列表（侧栏顺序 = 创建序；量小不做分页）。
pub fn list(conn: &Connection) -> Result<Vec<SmartFolder>> {
    let mut stmt =
        conn.prepare("SELECT id, name, query, createdAt, updatedAt FROM smart_folder ORDER BY id")?;
    let rows = stmt.query_map([], row_to_folder)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 单个（查看页直达 / PUT 前置检查用）。
pub fn get(conn: &Connection, id: i64) -> Result<Option<SmartFolder>> {
    let mut stmt = conn
        .prepare("SELECT id, name, query, createdAt, updatedAt FROM smart_folder WHERE id = ?1")?;
    let mut rows = stmt.query_map([id], row_to_folder)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

/// 新建（name 库内唯一；query 白名单归一化后入库）。
pub fn create(conn: &Connection, name: &str, query_raw: &str) -> Result<SmartFolder> {
    let name = validate_name(name)?;
    let query = normalize_query(query_raw)?;
    let dup: i64 = conn.query_row(
        "SELECT COUNT(*) FROM smart_folder WHERE name = ?1",
        [&name],
        |r| r.get(0),
    )?;
    if dup > 0 {
        return Err(StorageError::Invalid(format!("名称已存在：{name}")));
    }
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM smart_folder", [], |r| r.get(0))?;
    if count >= 200 {
        return Err(StorageError::Invalid("智能文件夹已达上限（200）".into()));
    }
    conn.execute(
        "INSERT INTO smart_folder (name, query, createdAt, updatedAt)
         VALUES (?1, ?2, unixepoch(), unixepoch())",
        rusqlite::params![name, query],
    )?;
    Ok(get(conn, conn.last_insert_rowid())?.expect("just inserted"))
}

/// 更新（name/query 任意子集；重名校验排除自身）。
pub fn update(
    conn: &Connection,
    id: i64,
    name: Option<&str>,
    query_raw: Option<&str>,
) -> Result<SmartFolder> {
    if get(conn, id)?.is_none() {
        return Err(StorageError::Invalid(format!("智能文件夹不存在：{id}")));
    }
    let name = match name {
        Some(n) => {
            let n = validate_name(n)?;
            let dup: i64 = conn.query_row(
                "SELECT COUNT(*) FROM smart_folder WHERE name = ?1 AND id != ?2",
                rusqlite::params![n, id],
                |r| r.get(0),
            )?;
            if dup > 0 {
                return Err(StorageError::Invalid(format!("名称已存在：{n}")));
            }
            Some(n)
        }
        None => None,
    };
    let query = match query_raw {
        Some(q) => Some(normalize_query(q)?),
        None => None,
    };
    conn.execute(
        "UPDATE smart_folder SET
            name    = COALESCE(?2, name),
            query   = COALESCE(?3, query),
            updatedAt = unixepoch()
         WHERE id = ?1",
        rusqlite::params![id, name, query],
    )?;
    Ok(get(conn, id)?.expect("just updated"))
}

/// 删除（存在返回 true；幂等删除返回 false 由调用方决定 404）。
pub fn delete(conn: &Connection, id: i64) -> Result<bool> {
    let n = conn.execute("DELETE FROM smart_folder WHERE id = ?1", [id])?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open;

    fn test_conn() -> Connection {
        let dir = std::env::temp_dir().join(format!(
            "e3-dao-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        open(&dir.join("t.db")).unwrap()
    }

    #[test]
    fn create_list_update_delete_roundtrip() {
        let conn = test_conn();
        let f = create(&conn, "精选风景", r#"{"tag":"风景","minRating":4}"#).unwrap();
        assert_eq!(f.name, "精选风景");
        let q: Value = serde_json::from_str(&f.query).unwrap();
        assert_eq!(q["tag"], "风景");
        assert_eq!(q["minRating"], 4);
        assert_eq!(list(&conn).unwrap().len(), 1);

        let f = update(&conn, f.id, Some("精选"), None).unwrap();
        assert_eq!(f.name, "精选");
        let f = update(
            &conn,
            f.id,
            None,
            Some(r#"{"tag":"风景","minRating":4,"colorLabel":"green"}"#),
        )
        .unwrap();
        assert!(f.query.contains("colorLabel"));

        assert!(delete(&conn, f.id).unwrap());
        assert!(!delete(&conn, f.id).unwrap());
        assert!(list(&conn).unwrap().is_empty());
    }

    #[test]
    fn validation_rejects_bad_input() {
        let conn = test_conn();
        // 空条件
        assert!(create(&conn, "a", "{}").is_err());
        // 未知键（分页不保存）
        assert!(create(&conn, "a", r#"{"limit":10}"#).is_err());
        // 非法颜色
        assert!(create(&conn, "a", r#"{"colorLabel":"cyan"}"#).is_err());
        // 评分越界
        assert!(create(&conn, "a", r#"{"minRating":9}"#).is_err());
        // 非对象
        assert!(create(&conn, "a", "[1]").is_err());
        // 空名 / 重名
        assert!(create(&conn, "  ", r#"{"untagged":true}"#).is_err());
        create(&conn, "x", r#"{"untagged":true}"#).unwrap();
        assert!(create(&conn, "x", r#"{"tag":"狗"}"#).is_err());
        // 归一化：空串剔除 + 结果等价（键序不敏感比较）
        let got = normalize_query(r#"{"minRating":3,"tag":"","tag":"狗"}"#).unwrap();
        let want = normalize_query(r#"{"tag":"狗","minRating":3}"#).unwrap();
        assert_eq!(got, want);
        let g: Value = serde_json::from_str(&got).unwrap();
        assert_eq!(g["tag"], "狗");
        assert_eq!(g["minRating"], 3);
        assert!(g.get("tag").is_some());
    }
}
