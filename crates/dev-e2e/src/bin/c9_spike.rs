//! C9 组级判定 spike（卡 task）—— L3 dev-harness，只在**外面**观察引擎行为。
//!
//! 三个模式各自产一份 JSON 证据（落 `C9_OUT`，同时以 `C9_JSON` 单行打印便于脚本吃）：
//!
//! - `C9_MODE=migrate`  在**已有旧库**上跑迁移 → 迁前/迁后表结构对照（升级路径证据：
//!   `0014 → 0015` 只加列加索引，不 drop、不重建、历史判定行数不变）。
//! - `C9_MODE=fixture`  用**真 JPEG 字节**造确定性夹具（三个目录 = 三个组；组 A 内含
//!   字节复制的精确重复 + 重编码的近似重复）→ 跑真编排（资产 job + 组级 job）→
//!   验证 F3 组级重复判定、F4 组级齐套判定（mock VL 给固定答案）、per_asset 规则不回归。
//! - `C9_MODE=run`      在真图库（默认 C7 的 40 张语料库）上跑真编排 → 真 VL（或 mock）下的
//!   组级判定结构健康度 + 逐规则分桶 + 组级证据样例。
//!
//! 环境变量：
//!   C9_MODE        migrate|fixture|run（默认 run）
//!   C9_DB          目标库 SQLite（三个模式都要；fixture 会先建库）
//!   C9_SRC         真图源目录（fixture 造夹具用；默认 /tmp/c7-work/corpus）
//!   C9_PACK_DIR    **必填**：自备四件 YAML 包目录（宿主已不内置行业包，IP-K16）
//!   C9_OUT         证据输出目录（默认 /workspace/reports/logs/c9）
//!   C9_WORKERS     并行 worker 数（默认 6）
//!   C9_MOCK_LABEL  mock VL 的固定答案（默认 客厅）；C9_MOCK_ALL=1 时所有成员都答这个
//!   C9_VL_BASE_URL / C9_VL_MODEL / C9_VL_API_KEY / C9_VL_PROXY（run 模式的真 VL；缺 key →
//!                  自动退回 mock 并如实标注 vlSource=mock）

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use rusqlite::Connection;
use svc_pipeline::compliance_queue::{compliance_enq, compliance_enq_group, run_compliance_queue_par, ConnFactory};
use svc_pipeline::VlConfig;

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| d.to_string())
}

fn out_dir() -> PathBuf {
    PathBuf::from(env_or("C9_OUT", "/workspace/reports/logs/c9"))
}

fn dump(name: &str, v: &serde_json::Value) {
    let dir = out_dir();
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join(name);
    std::fs::write(&p, serde_json::to_string_pretty(v).unwrap()).unwrap();
    println!("[C9] wrote {}", p.display());
    println!("[C9_JSON] {} {}", name, serde_json::to_string(v).unwrap());
}

/// IP-K16：宿主不再内置 YAML 行业包 —— legacy 内联引擎的开发 spike 只能对自备包跑。
fn pack_dir() -> PathBuf {
    PathBuf::from(std::env::var("C9_PACK_DIR").unwrap_or_else(|_| {
        panic!(
            "宿主不再内置 YAML 行业包（IP-K16）：把 C9_PACK_DIR 指到自备的 taxonomy/rules/pipeline/report 四件 YAML 目录"
        )
    }))
}

/// 极简 OpenAI 兼容 mock（与 c5 单测同款）：固定答案，按请求序回同样的 content。
fn spawn_mock(label: &str) -> String {
    let answer = format!("{{\"label\":\"{label}\"}}");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for req in listener.incoming() {
            let Ok(mut stream) = req else { break };
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 16384];
            loop {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header_end = pos + 4;
                    let head = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= header_end + len {
                        break;
                    }
                }
            }
            let body = serde_json::json!({ "choices": [{ "message": { "content": answer } }] }).to_string();
            let http = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(http.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://{addr}")
}

/// VL 配置：有真 key 用真的，否则 mock（如实标注）。
fn vl_config() -> (VlConfig, &'static str) {
    let key = std::env::var("C9_VL_API_KEY").unwrap_or_default();
    let label = env_or("C9_MOCK_LABEL", "客厅");
    if key.trim().is_empty() {
        let base = spawn_mock(&label);
        (
            VlConfig {
                base_url: base,
                api_key: "mock".into(),
                model: "mock-vl".into(),
                timeout_secs: 30,
                max_tokens: 200,
                proxy: Some(String::new()), // 显式直连（mock 是本机）
            },
            "mock",
        )
    } else {
        (
            VlConfig {
                base_url: env_or("C9_VL_BASE_URL", "https://api.deepseek.com/v1"),
                api_key: key,
                model: env_or("C9_VL_MODEL", "deepseek-flash"),
                timeout_secs: 90,
                max_tokens: 4096,
                proxy: std::env::var("C9_VL_PROXY").ok(),
            },
            "real",
        )
    }
}

fn conn_factory(db: PathBuf) -> ConnFactory {
    Arc::new(move || {
        let c = Connection::open(&db)?;
        c.busy_timeout(std::time::Duration::from_secs(15))?;
        c.pragma_update(None, "journal_mode", "WAL")?;
        Ok(c)
    })
}

fn table_cols(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

fn indexes(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='index' ORDER BY name")
        .unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

fn verdict_counts(conn: &Connection, pack: &str, baseline: i64) -> Vec<serde_json::Value> {
    let mut stmt = conn
        .prepare(
            "SELECT ruleId, judgementScope, COALESCE(verdict, CASE result WHEN 'skipped' THEN 'skipped' ELSE 'undetermined' END) AS vres, COUNT(*) FROM rule_verdict
             WHERE packId = ?1 AND jobId > ?2 GROUP BY ruleId, judgementScope, vres
             ORDER BY ruleId, vres",
        )
        .unwrap();
    let rows = stmt
        .query_map(rusqlite::params![pack, baseline], |r| {
            Ok(serde_json::json!({
                "ruleId": r.get::<_, String>(0)?,
                "scope": r.get::<_, Option<String>>(1)?,
                "result": r.get::<_, String>(2)?,
                "rows": r.get::<_, i64>(3)?,
            }))
        })
        .unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

fn group_rows(conn: &Connection, pack: &str, baseline: i64, limit: usize) -> Vec<serde_json::Value> {
    let mut stmt = conn
        .prepare(
            "SELECT ruleId, groupKey, assetId, COALESCE(verdict, CASE result WHEN 'skipped' THEN 'skipped' ELSE 'undetermined' END), score, evidence, jobId FROM rule_verdict
             WHERE packId = ?1 AND jobId > ?2 AND groupKey IS NOT NULL
             ORDER BY ruleId, groupKey, assetId LIMIT ?3",
        )
        .unwrap();
    let rows = stmt
        .query_map(rusqlite::params![pack, baseline, limit as i64], |r| {
            let ev: Option<String> = r.get(5)?;
            Ok(serde_json::json!({
                "ruleId": r.get::<_, String>(0)?,
                "groupKey": r.get::<_, String>(1)?,
                "assetId": r.get::<_, i64>(2)?,
                "result": r.get::<_, String>(3)?,
                "score": r.get::<_, Option<f64>>(4)?,
                "jobId": r.get::<_, Option<i64>>(6)?,
                "evidence": ev.as_deref().and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()),
            }))
        })
        .unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

/// C9：资产口径上的**组级规则占位行**（groupKey IS NULL）——用来证明资产 job 不擅自执行组级
/// 规则（只落 skipped + reason=group_scoped），不把「未知检测器」之类的噪声写进资产判定空间。
fn asset_scope_group_rows(
    conn: &Connection,
    pack: &str,
    ids: &[String],
    baseline: i64,
    limit: usize,
) -> Vec<serde_json::Value> {
    if ids.is_empty() {
        return Vec::new();
    }
    let marks = ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 3))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT ruleId, assetId, COALESCE(verdict, CASE result WHEN 'skipped' THEN 'skipped' ELSE 'undetermined' END), evidence FROM rule_verdict
         WHERE packId = ?1 AND jobId > ?2 AND groupKey IS NULL AND ruleId IN ({marks})
         ORDER BY ruleId, assetId LIMIT ?{}",
        ids.len() + 3
    );
    let mut stmt = conn.prepare(&sql).unwrap();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![
        Box::new(pack.to_string()),
        Box::new(baseline),
    ];
    for id in ids {
        params.push(Box::new(id.clone()));
    }
    params.push(Box::new(limit as i64));
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = stmt
        .query_map(refs.as_slice(), |r| {
            let ev: Option<String> = r.get(3)?;
            Ok(serde_json::json!({
                "ruleId": r.get::<_, String>(0)?,
                "assetId": r.get::<_, i64>(1)?,
                "result": r.get::<_, String>(2)?,
                "evidence": ev.as_deref().and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()),
            }))
        })
        .unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

/// job 队列快照（**本次运行口径**：只数 id > baseline 的 job，避免历史代际混入）。
fn job_counts(conn: &Connection, kind: &str, baseline: i64) -> serde_json::Value {
    let mut stmt = conn
        .prepare("SELECT status, COUNT(*) FROM job_queue WHERE kind = ?1 AND id > ?2 GROUP BY status")
        .unwrap();
    let rows: Vec<(String, i64)> = stmt
        .query_map(rusqlite::params![kind, baseline], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    let get = |k: &str| rows.iter().find(|(s, _)| s == k).map(|(_, n)| *n).unwrap_or(0);
    serde_json::json!({
        "done": get("done"), "failed": get("failed"),
        "pending": get("pending"), "running": get("running"),
    })
}

fn max_job_id(conn: &Connection) -> i64 {
    conn.query_row("SELECT COALESCE(MAX(id),0) FROM job_queue", [], |r| r.get(0))
        .unwrap()
}

fn asset_ids(conn: &Connection) -> Vec<i64> {
    let mut stmt = conn
        .prepare("SELECT assetId FROM assets WHERE deleted = 0 ORDER BY assetId")
        .unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, i64>(0)).unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

// ---------- 模式 1：迁移（旧库升级路径） ----------

fn migrate() -> i32 {
    let db = PathBuf::from(env_or("C9_DB", "/tmp/c9-fixture/db/library.db"));
    let conn = Connection::open(&db).unwrap();
    let before_cols = table_cols(&conn, "rule_verdict");
    let before_idx = indexes(&conn);
    let before_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM rule_verdict", [], |r| r.get(0))
        .unwrap_or(0);
    let before_sm: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT group_concat(version || ':' || name, ',') FROM (SELECT version, name FROM schema_migrations ORDER BY version)")
            .unwrap();
        stmt.query_row([], |r| r.get::<_, String>(0)).unwrap_or_default()
            .split(',')
            .map(|s| s.to_string())
            .collect()
    };
    core_storage::migrations::migrate(&conn).unwrap();
    let after_cols = table_cols(&conn, "rule_verdict");
    let after_idx = indexes(&conn);
    let after_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM rule_verdict", [], |r| r.get(0))
        .unwrap_or(0);
    let after_sm: String = conn
        .query_row(
            "SELECT group_concat(version || ':' || name, ',') FROM (SELECT version, name FROM schema_migrations ORDER BY version)",
            [],
            |r| r.get(0),
        )
        .unwrap_or_default();
    let new_cols: Vec<String> = after_cols
        .iter()
        .filter(|c| !before_cols.contains(c))
        .cloned()
        .collect();
    let new_idx: Vec<String> = after_idx
        .iter()
        .filter(|c| !before_idx.contains(c))
        .cloned()
        .collect();
    let v = serde_json::json!({
        "db": db.to_string_lossy(),
        "before": { "ruleVerdictColumns": before_cols, "indexes": before_idx,
                    "verdictRows": before_rows, "schemaMigrations": before_sm },
        "after": { "ruleVerdictColumns": after_cols, "indexes": after_idx,
                   "verdictRows": after_rows, "schemaMigrations": after_sm },
        // 迁前确实是「旧库形态」（无组级两列 + 账本最高 13）——升级证据才有意义
        "beforeStateIsOld": !before_cols.iter().any(|c| c == "judgementScope" || c == "groupKey")
            && before_cols.len() + 2 == after_cols.len(),
        "newColumns": new_cols,
        "newIndexes": new_idx,
        "verdictRowsKept": after_rows >= before_rows,
    });
    dump("c9-migrate.json", &v);
    println!(
        "C9_RESULT MIGRATE db={} beforeIsOld={} newCols={:?} newIdx={:?} rows {} -> {}",
        db.display(),
        v["beforeStateIsOld"],
        new_cols,
        new_idx,
        before_rows,
        after_rows
    );
    0
}

// ---------- 模式 2：真图夹具（确定性） ----------

/// 造夹具库：三个目录组（propA 内含精确重复 + 近似重复），真 JPEG 字节。
fn fixture() -> i32 {
    let db = PathBuf::from(env_or("C9_DB", "/tmp/c9-fixture/db/library.db"));
    let src = PathBuf::from(env_or("C9_SRC", "/tmp/c7-work/corpus"));
    let root = db.parent().unwrap().parent().unwrap().to_path_buf();
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&db.parent().unwrap()).unwrap();

    // 源图：取语料里前 N 张（真实照片）
    let mut pool: Vec<PathBuf> = std::fs::read_dir(&src)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().map(|e| e == "jpg").unwrap_or(false))
                .collect()
        })
        .unwrap_or_default();
    pool.sort();
    // 语料按目录分组，全部收进来
    if let Ok(rd) = std::fs::read_dir(&src) {
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() {
                if let Ok(inner) = std::fs::read_dir(&p) {
                    for f in inner.filter_map(|f| f.ok()) {
                        let fp = f.path();
                        if fp.extension().map(|x| x == "jpg").unwrap_or(false) {
                            pool.push(fp);
                        }
                    }
                }
            }
        }
    }
    pool.sort();
    pool.dedup();
    assert!(!pool.is_empty(), "语料为空：{src:?}");

    let prop_a = root.join("corpus/propA");
    let prop_b = root.join("corpus/propB");
    std::fs::create_dir_all(&prop_a).unwrap();
    std::fs::create_dir_all(&prop_b).unwrap();

    // 选图：先算 pHash，贪心挑「两两汉明距离 > 阈值」的真图，避免语料自带的相似图
    // 污染夹具结论（否则 propB「无重复」这一条会被语料自身的近似对打破）。
    let mut cand: Vec<(PathBuf, u64)> = Vec::new();
    for p in &pool {
        if let Ok(luma) = core_media::luma32(p) {
            if let Ok(ph) = core_hash::phash(&luma) {
                cand.push((p.clone(), ph));
            }
        }
    }
    let mut distinct: Vec<(PathBuf, u64)> = Vec::new();
    for (p, ph) in cand {
        if distinct.iter().all(|(_, q)| core_hash::hamming(*q, ph) > 8) {
            distinct.push((p, ph));
        }
        if distinct.len() >= 6 {
            break;
        }
    }
    assert!(
        distinct.len() >= 6,
        "语料 {src:?} 里挑不出 6 张两两 pHash 距离>8 的真图（拿到 {}）",
        distinct.len()
    );
    let mut pairwise = Vec::new();
    for i in 0..distinct.len() {
        for j in (i + 1)..distinct.len() {
            pairwise.push(serde_json::json!({
                "i": i, "j": j,
                "hamming": core_hash::hamming(distinct[i].1, distinct[j].1),
            }));
        }
    }
    let sel_paths: Vec<PathBuf> = distinct.iter().map(|(p, _)| p.clone()).collect();

    let mut fixture_files: Vec<(PathBuf, &'static str)> = Vec::new();
    // propA：4 张不同真图 + 1 张字节复制（精确重复）+ 1 张重编码（近似重复）
    let a_src: Vec<PathBuf> = sel_paths.iter().take(4).cloned().collect();
    for (i, p) in a_src.iter().enumerate() {
        let dest = prop_a.join(format!("propA-{i:05}.jpg"));
        std::fs::copy(p, &dest).unwrap();
        fixture_files.push((dest, "distinct"));
    }
    let exact = prop_a.join("propA-00000-copy.jpg");
    std::fs::copy(&a_src[0], &exact).unwrap();
    fixture_files.push((exact, "exact_dup"));
    let near = prop_a.join("propA-00001-reencode.jpg");
    {
        let (w, h, rgb) = core_media::rgb8(&a_src[1]).unwrap();
        core_media::encode_rgb_to(w, h, &rgb, &near, Some(85)).unwrap();
    }
    fixture_files.push((near, "near_dup"));
    // propB：2 张不同真图（与前面 4 张及彼此 pHash 距离都 >8）→ F3 pass
    for (i, p) in sel_paths.iter().skip(4).take(2).enumerate() {
        let dest = prop_b.join(format!("propB-{i:05}.jpg"));
        std::fs::copy(p, &dest).unwrap();
        fixture_files.push((dest, "distinct"));
    }

    let conn = Connection::open(&db).unwrap();
    core_storage::migrations::migrate(&conn).unwrap();
    for (path, _kind) in &fixture_files {
        conn.execute(
            "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (NULL, ?1, 'image', ?2, 'jpg', 1, 0, 0)",
            rusqlite::params![
                path.to_string_lossy(),
                path.file_name().unwrap().to_string_lossy()
            ],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        let (w, h) = core_media::image_size(path).unwrap();
        conn.execute(
            "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1, 'primary', 0, ?2, ?3)",
            rusqlite::params![id, w as i64, h as i64],
        )
        .unwrap();
    }
    drop(conn);

    let pack = core_load_pack();
    let (report, vl_snap, vl_source, baseline) = run_all(&db, fixture_files.len(), &pack);
    let conn = Connection::open(&db).unwrap();
    let v = serde_json::json!({
        "db": db.to_string_lossy(),
        "fixture": {
            "dir": root.to_string_lossy(),
            "files": fixture_files.iter().map(|(p, k)| serde_json::json!({
                "path": p.to_string_lossy(), "kind": k })).collect::<Vec<_>>(),
            "groups": { "propA": 6, "propB": 2 },
            "selection": {
                "distinctSources": sel_paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
                "pairwiseHamming": pairwise,
            },
        },
        "pack": pack.id,
        "baselineJobId": baseline,
        "vlSource": vl_source,
        "report": report,
        "vlStats": vl_snap,
        "assetJobQueue": job_counts(&conn, "compliance", baseline),
        "groupJobQueue": job_counts(&conn, "compliance_group", baseline),
        "verdictCounts": verdict_counts(&conn, &pack.id, baseline),
        "groupRows": group_rows(&conn, &pack.id, baseline, 24),
        "assetScopeGroupRows": asset_scope_group_rows(
            &conn,
            &pack.id,
            &group_rule_ids(&pack),
            baseline,
            6,
        ),
    });
    dump("c9-fixture.json", &v);
    println!(
        "C9_RESULT FIXTURE assets={} groups=2 vl={} groupDone={} groupFailed={} verdicts={}",
        fixture_files.len(),
        vl_source,
        report["groupDone"],
        report["groupFailed"],
        report["groupTotal"]
    );
    0
}

/// 装载 pack（走真装载路径：L0 键级 lint + 语义校验）。
fn core_load_pack() -> core_config::Pack {
    core_config::Pack::from_dir(&pack_dir()).expect("pack 装载失败")
}

/// pack 里的组级规则 id（资产口径上应只有占位 skipped 行）。
fn group_rule_ids(pack: &core_config::Pack) -> Vec<String> {
    pack.rules
        .iter()
        .filter(|r| r.is_group_scoped())
        .map(|r| r.id.clone())
        .collect()
}

// ---------- 模式 3：真图库（40 张语料） ----------

fn run() -> i32 {
    let db = PathBuf::from(env_or("C9_DB", "/tmp/c7-work/db/library.db"));
    let pack = core_load_pack();
    let ids = {
        let conn = Connection::open(&db).unwrap();
        // 升级路径：真库上也跑一次迁移（0014 → 0015）
        core_storage::migrations::migrate(&conn).unwrap();
        asset_ids(&conn)
    };
    let (report, vl_snap, vl_source, baseline) = run_all(&db, ids.len(), &pack);
    let conn = Connection::open(&db).unwrap();
    let gkeys: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT groupKey FROM rule_verdict
                 WHERE packId = ?1 AND jobId > ?2 AND groupKey IS NOT NULL ORDER BY groupKey",
            )
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params![pack.id, baseline], |r| r.get::<_, String>(0))
            .unwrap();
        rows.filter_map(|r| r.ok()).collect()
    };
    let group_row_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM rule_verdict WHERE packId = ?1 AND jobId > ?2 AND groupKey IS NOT NULL",
            rusqlite::params![pack.id, baseline],
            |r| r.get(0),
        )
        .unwrap_or(-1);
    let v = serde_json::json!({
        "db": db.to_string_lossy(),
        "pack": pack.id,
        "assets": ids.len(),
        "baselineJobId": baseline,
        "vlSource": vl_source,
        "report": report,
        "vlStats": vl_snap,
        "assetJobQueue": job_counts(&conn, "compliance", baseline),
        "groupJobQueue": job_counts(&conn, "compliance_group", baseline),
        "verdictCounts": verdict_counts(&conn, &pack.id, baseline),
        "groupRows": group_rows(&conn, &pack.id, baseline, 300),
        "assetScopeGroupRows": asset_scope_group_rows(
            &conn,
            &pack.id,
            &group_rule_ids(&pack),
            baseline,
            6,
        ),
        "distinctGroups": { "rows": group_row_count, "keys": gkeys },
    });
    dump("c9-run.json", &v);
    println!(
        "C9_RESULT RUN assets={} vl={} groupDone={} groupFailed={} groupTotal={} done={} failed={}",
        ids.len(),
        vl_source,
        report["groupDone"],
        report["groupFailed"],
        report["groupTotal"],
        report["done"],
        report["failed"]
    );
    0
}

/// 资产入队 + 组级入队 + 并行跑一轮；返回 (报告, VL 快照, VL 来源, baseline jobId)。
fn run_all(
    db: &Path,
    asset_count: usize,
    pack: &core_config::Pack,
) -> (serde_json::Value, Option<serde_json::Value>, &'static str, i64) {
    let conn = Connection::open(db).unwrap();
    let ids = asset_ids(&conn);
    let baseline = max_job_id(&conn);
    let out = db.parent().unwrap().join("fixes");
    let out_s = out.to_string_lossy().to_string();
    let n_asset = compliance_enq(&conn, &ids, &pack.id, Some(&out_s)).unwrap();
    let n_group = compliance_enq_group(&conn, pack, &ids, Some(&out_s)).unwrap();
    println!(
        "[C9] 入队 asset_jobs={n_asset} group_jobs={n_group}（资产 {asset_count} / 组级 {} 个 job）",
        group_rule_ids(pack).len()
    );
    let (vl_cfg, vl_source) = vl_config();
    let workers = env_or("C9_WORKERS", "6").parse::<usize>().unwrap_or(6);
    let cancel = AtomicBool::new(false);
    let (report, snap) = run_compliance_queue_par(
        conn_factory(db.to_path_buf()),
        pack,
        Some(vl_cfg),
        workers,
        &cancel,
        |p| {
            if p.status == "failed" || p.status == "retry" {
                eprintln!("[C9] job{} {}: {:?}", p.jobId, p.status, p.error);
            }
        },
    )
    .expect("compliance 并行运行失败");
    (
        serde_json::to_value(&report).unwrap(),
        snap.map(|s| serde_json::to_value(s).unwrap()),
        vl_source,
        baseline,
    )
}

fn main() {
    let mode = env_or("C9_MODE", "run");
    let code = match mode.as_str() {
        "migrate" => migrate(),
        "fixture" => fixture(),
        "run" => run(),
        other => {
            eprintln!("未知 C9_MODE={other}（migrate|fixture|run）");
            2
        }
    };
    std::process::exit(code);
}
