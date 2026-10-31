//! D5 向量存储（sqlite-vec）：按 embModel 分表（`v_<sanitized>`，禁混算铁律），
//! vec0 虚表 + cosine KNN。裁决依据见 `reports/D5-spike-decision.md`。
//!
//! 表布局：`v_<model 非字母数字换 _>`（如 v_siglip2_base_p224_onnx），
//! 列 `resourceId INTEGER PRIMARY KEY, embedding float[dims] distance_metric=cosine`。
//! 登记在 `vector_tables(model, dims, tableName)`（migration 0005）。
//!
//! spike 实测（24 核容器，100K×512）：
//! - KNN k=10/100：41/45ms（<1s 验收的 20 倍余量）
//! - JOIN 标量过滤是 **lossy post-filter**（20 里只回 6）→ 过滤语义必须由
//!   调用方（engine-search）预取候选 id 集或用 aux 列；本模块只提供原语。

use rusqlite::Connection;
use std::sync::Once;

use crate::{Result, StorageError};

/// 进程级一次性注册 sqlite-vec 自动扩展（必须在任何 Connection 打开前；
/// `open()` 已统一调用，业务侧无感）。
pub fn init_vec_ext() {
    static INIT: Once = Once::new();
    INIT.call_once(|| unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    });
}

/// 表名：`v_` + model 名中非 [a-zA-Z0-9_] 字符替换为 `_`。
pub fn table_name(model: &str) -> String {
    let sanitized: String = model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("v_{sanitized}")
}

/// 建表（幂等）：vec0 虚表 + 登记簿。返回表名。
pub fn ensure_table(conn: &Connection, model: &str, dims: u32) -> Result<String> {
    init_vec_ext();
    let tbl = table_name(model);
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
            [&tbl],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)?;
    if !exists {
        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE \"{tbl}\" USING vec0(
                 resourceId INTEGER PRIMARY KEY,
                 embedding float[{dims}] distance_metric=cosine
             );"
        ))
        .map_err(|e| StorageError::Migration {
            version: 0,
            name: format!("vecstore:{model}"),
            msg: e.to_string(),
        })?;
    }
    conn.execute(
        "INSERT INTO vector_tables (model, dims, tableName, createdAt)
         VALUES (?1, ?2, ?3, unixepoch())
         ON CONFLICT(model) DO UPDATE SET dims = excluded.dims, tableName = excluded.tableName",
        rusqlite::params![model, dims, tbl],
    )?;
    Ok(tbl)
}

/// 已登记的向量表。
pub fn tables(conn: &Connection) -> Result<Vec<(String, u32, String)>> {
    let mut stmt =
        conn.prepare("SELECT model, dims, tableName FROM vector_tables ORDER BY model")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn vec_blob(v: &[f32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(v.len() * 4);
    for x in v {
        b.extend_from_slice(&x.to_le_bytes());
    }
    b
}

/// 批量 upsert（重嵌入覆盖旧向量；spike 实测 vec0 不支持 INSERT OR REPLACE，
/// 用 delete + insert）。返回写入行数。不内嵌事务：崩溃中断只是重嵌入（幂等）。
pub fn upsert(conn: &Connection, model: &str, rows: &[(i64, &[f32])]) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let tbl = table_name(model);
    let mut del = conn.prepare(&format!("DELETE FROM \"{tbl}\" WHERE resourceId = ?1"))?;
    let mut ins = conn.prepare(&format!(
        "INSERT INTO \"{tbl}\" (resourceId, embedding) VALUES (?1, ?2)"
    ))?;
    for (rid, v) in rows {
        del.execute([rid])?;
        ins.execute(rusqlite::params![rid, vec_blob(v)])?;
    }
    Ok(rows.len())
}

/// 按 resourceId 批量删向量行（彻底删除资产时清向量；无 FK，必须显式删）。
///
/// - 表不存在（该模型从没建过表 / 已被 `drop_table`）→ 返回 0，不报错；
/// - 返回真实删掉的行数（不存在的 rid 静默跳过，与 `fetch_vectors` 同语义）。
pub fn delete_resources(conn: &Connection, model: &str, rids: &[i64]) -> Result<usize> {
    if rids.is_empty() {
        return Ok(0);
    }
    let tbl = table_name(model);
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
            [&tbl],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)?;
    if !exists {
        return Ok(0);
    }
    let mut stmt = conn.prepare(&format!("DELETE FROM \"{tbl}\" WHERE resourceId = ?1"))?;
    let mut n = 0usize;
    for rid in rids {
        n += stmt.execute([rid])?;
    }
    Ok(n)
}

/// 余弦 KNN：返回 (resourceId, distance)，distance = 1 - cos（升序）。
pub fn knn(conn: &Connection, model: &str, query: &[f32], k: usize) -> Result<Vec<(i64, f32)>> {
    let tbl = table_name(model);
    let mut stmt = conn.prepare(&format!(
        "SELECT resourceId, distance FROM \"{tbl}\" WHERE embedding MATCH ?1 AND k = ?2"
    ))?;
    let rows = stmt.query_map(rusqlite::params![vec_blob(query), k as i64], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 按 id 批量取回向量（过滤路径精确打分用；500/批分块）。
pub fn fetch_vectors(conn: &Connection, model: &str, ids: &[i64]) -> Result<Vec<(i64, Vec<f32>)>> {
    let tbl = table_name(model);
    let mut out = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(500) {
        let ph = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!("SELECT resourceId, embedding FROM \"{tbl}\" WHERE resourceId IN ({ph})");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
            let rid: i64 = r.get(0)?;
            let blob: Vec<u8> = r.get(1)?;
            Ok((rid, blob_to_vec(&blob)))
        })?;
        for row in rows {
            out.push(row?);
        }
    }
    Ok(out)
}

fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// 表内向量行数。
pub fn count(conn: &Connection, model: &str) -> Result<i64> {
    let tbl = table_name(model);
    Ok(conn.query_row(&format!("SELECT COUNT(*) FROM \"{tbl}\""), [], |r| r.get(0))?)
}

/// 删表（清空某模型向量；老向量在模型切换时保留，仅显式调用才删）。
pub fn drop_table(conn: &Connection, model: &str) -> Result<bool> {
    let tbl = table_name(model);
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
            [&tbl],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)?;
    if exists {
        conn.execute_batch(&format!("DROP TABLE \"{tbl}\";"))?;
    }
    let n = conn.execute("DELETE FROM vector_tables WHERE model = ?1", [model])?;
    Ok(exists || n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        // vec0 依赖 auto_extension：必须在任何连接打开前注册（生产路径 core_storage::open 已做）
        init_vec_ext();
        let c = Connection::open_in_memory().unwrap();
        crate::migrations::migrate(&c).unwrap();
        c
    }

    fn unit(v: &[f32]) -> Vec<f32> {
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / n).collect()
    }

    #[test]
    fn ensure_upsert_knn_roundtrip() {
        let c = conn();
        let tbl = ensure_table(&c, "siglip2-base-p224-onnx", 768).unwrap();
        assert_eq!(tbl, "v_siglip2_base_p224_onnx");
        // 幂等
        assert_eq!(
            ensure_table(&c, "siglip2-base-p224-onnx", 768).unwrap(),
            tbl
        );

        let e1 = unit(&[1.0, 0.0, 0.0]);
        let e2 = unit(&[1.0, 1.0, 0.0]);
        // 768 维补零
        let pad = |mut v: Vec<f32>| {
            v.resize(768, 0.0);
            unit(&v)
        };
        let (e1, e2) = (pad(e1), pad(e2));
        upsert(
            &c,
            "siglip2-base-p224-onnx",
            &[(1, &e1), (2, &e2), (3, &e1)],
        )
        .unwrap();
        assert_eq!(count(&c, "siglip2-base-p224-onnx").unwrap(), 3);

        let hits = knn(&c, "siglip2-base-p224-onnx", &e1, 2).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(
            hits[0].1.abs() < 1e-4,
            "self distance ~0, got {}",
            hits[0].1
        );
        // 1 与 3 同向量并列前二 → id 2 必然不在 top-2
        let ids: Vec<i64> = hits.iter().map(|h| h.0).collect();
        assert!(ids.contains(&1) && ids.contains(&3), "ids={ids:?}");
        assert!(!ids.contains(&2));

        // upsert 覆盖：id=3 换向量后自查询第一
        let e3 = pad(unit(&[0.0, 1.0, 0.0]));
        upsert(&c, "siglip2-base-p224-onnx", &[(3, &e3)]).unwrap();
        assert_eq!(count(&c, "siglip2-base-p224-onnx").unwrap(), 3);
        let hits = knn(&c, "siglip2-base-p224-onnx", &e3, 1).unwrap();
        assert_eq!(hits[0].0, 3);
        assert!(hits[0].1.abs() < 1e-4);
    }

    #[test]
    fn fetch_and_drop() {
        let c = conn();
        ensure_table(&c, "m-a", 4).unwrap();
        ensure_table(&c, "m-b", 4).unwrap();
        let v = unit(&[1.0, 2.0, 3.0, 4.0]);
        upsert(&c, "m-a", &[(7, &v), (8, &v)]).unwrap();
        upsert(&c, "m-b", &[(7, &v)]).unwrap();
        let got = fetch_vectors(&c, "m-a", &[7, 8, 999]).unwrap();
        assert_eq!(got.len(), 2, "不存在 id 静默跳过");
        assert_eq!(got[0].1.len(), 4);
        // 两模型分表互不可见（禁混算的结构保证）
        assert_eq!(count(&c, "m-a").unwrap(), 2);
        assert_eq!(count(&c, "m-b").unwrap(), 1);
        assert!(drop_table(&c, "m-a").unwrap());
        assert_eq!(count(&c, "m-b").unwrap(), 1);
        let ts = tables(&c).unwrap();
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].0, "m-b");
    }

    #[test]
    fn delete_resources_clears_rows_and_tolerates_missing_table() {
        let c = conn();
        ensure_table(&c, "m-del", 4).unwrap();
        let v = unit(&[1.0, 0.0, 0.0, 0.0]);
        upsert(&c, "m-del", &[(11, &v), (12, &v), (13, &v)]).unwrap();
        // 只删两条：11 命中、999 不存在（静默跳过）
        assert_eq!(delete_resources(&c, "m-del", &[11, 999]).unwrap(), 1);
        assert_eq!(count(&c, "m-del").unwrap(), 2);
        let left: Vec<i64> = fetch_vectors(&c, "m-del", &[11, 12, 13])
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert!(!left.contains(&11) && left.contains(&12) && left.contains(&13));
        // 空入参 / 未建表模型 → 0（不报错、不建表）
        assert_eq!(delete_resources(&c, "m-del", &[]).unwrap(), 0);
        assert_eq!(delete_resources(&c, "never-created", &[12]).unwrap(), 0);
        assert_eq!(count(&c, "m-del").unwrap(), 2);
    }
}
