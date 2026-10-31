//! KI-2b 真库探针（卡 task）：**造一个真实 SQLite 库** → 跑存量补判 → 打印可核对结论。
//!
//! 这不是 mock：库是真的（迁移全跑、vec0 向量表真的写入），走的是与线上同一条
//! `backfill_dup_edges → dup_edges → dup_groups` 路径。用途是让验收脚本与人工复核
//! 能一条命令复现「视觉判重分组」：
//!
//!   cargo run -p app-web --example ki2b_dedup_probe -- /tmp/ki2b.db
//!
//! 输出四行 JSON（前缀固定，脚本按前缀取）：
//!   BEFORE  —— 补判前的 judge 状态（vectors>0 & edges=0 = 「未判完」，不是「无重复」）
//!   REPORT  —— backfill 报告（scanned/edges/truncated）
//!   AFTER   —— 补判后的 judge 状态（backfillDone=true）
//!   GROUPS  —— dup_groups 分组结果（kind / mixed / layers / 成员相似度）
//!
//! 造的数据（6 张图，三套互不相连的判据）：
//!   1、2   同 fileHash（exact 对），向量正交（只有一层判据）
//!   3      与 1 余弦 0.99（near_visual 同一视觉内容）
//!   4      谁都像不了（孤立）
//!   5、6   余弦 0.95（variant 疑似变体）
//! 期望：两个组 —— {1,2,3} kind=near_visual mixed=true / {5,6} kind=variant mixed=false。

use rusqlite::Connection;
use std::path::Path;

/// 判重塔向量维度（探针用 4 维，只为让余弦关系一眼可算）。
const DIMS: u32 = 4;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/ki2b-dedup-probe.db".to_string());
    let _ = std::fs::remove_file(&path);
    let conn = core_storage::open(Path::new(&path))?;
    seed(&conn)?;

    let model = core_config::DEDUP_EMBEDDER;
    println!(
        "BEFORE {}",
        serde_json::to_string(&svc_pipeline::dup_judge_status(&conn, model)?)?
    );
    println!(
        "REPORT {}",
        serde_json::to_string(&svc_pipeline::backfill_dup_edges(&conn, model, 0)?)?
    );
    println!(
        "AFTER {}",
        serde_json::to_string(&svc_pipeline::dup_judge_status(&conn, model)?)?
    );
    println!(
        "GROUPS {}",
        serde_json::to_string(&svc_library::dup_groups(&conn)?)?
    );
    println!("DB {path}");
    Ok(())
}

fn seed(conn: &Connection) -> Result<(), Box<dyn std::error::Error>> {
    for (id, hash, phash) in [
        (1, Some("aaaa"), Some(0u64)),
        (2, Some("aaaa"), Some(0u64)),
        // 3..6 不给 pHash：让它们只可能靠视觉判据成组（pHash 汉明会互相连成一片）
        (3, Some("bbbb"), None),
        (4, Some("cccc"), None),
        (5, Some("dddd"), None),
        (6, Some("eeee"), None),
    ] {
        conn.execute(
            "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime,
                                 fileHash, importedAt)
             VALUES (?1, ?2, 0, ?3, 'jpg', 100, 1, ?4, ?1)",
            rusqlite::params![id, format!("/probe/{id}.jpg"), format!("p{id}.jpg"), hash],
        )?;
        conn.execute(
            "INSERT INTO resource (assetId, role, ordinal, width, height, pHash)
             VALUES (?1, 'primary', 0, 800, 600, ?2)",
            rusqlite::params![id, phash.map(|v| format!("{v:016x}"))],
        )?;
    }

    core_storage::vecstore::ensure_table(conn, core_config::DEDUP_EMBEDDER, DIMS)?;
    let cos = |c: f32| {
        let s = (1.0 - c * c).sqrt();
        [c, s, 0.0, 0.0]
    };
    for (asset_id, v) in [
        (1i64, [1.0f32, 0.0, 0.0, 0.0]),
        (2, [0.0, 1.0, 0.0, 0.0]),
        (3, cos(0.99)),
        (4, [0.0, 0.0, 1.0, 0.0]),
        (5, [0.0, 0.0, 0.0, 1.0]),
        (6, [0.0, (1.0f32 - 0.95 * 0.95).sqrt(), 0.0, 0.95]),
    ] {
        let rid: i64 = conn.query_row(
            "SELECT resourceId FROM resource WHERE assetId = ?1 AND role = 'primary'",
            [asset_id],
            |r| r.get(0),
        )?;
        core_storage::vecstore::upsert(conn, core_config::DEDUP_EMBEDDER, &[(rid, &v)])?;
    }
    Ok(())
}
