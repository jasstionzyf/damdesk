//! OCR-E 端到端：临时库 + 真实三件模型 → 入队 → 跑队列 → 校验 ocr_text 落库 + 检索融合。
//!
//! 用法（容器内）：
//!   DAM_MODELS_ROOT=/ cargo run --release -p dev-e2e --bin ocr_e2e -- /tmp/ocr-corpus
//!
//! 退出码 0 = 全部通过（逐项打印 OCR_E2E_RESULT）。

use std::path::Path;

fn main() {
    let corpus = std::env::args().nth(1).unwrap_or_else(|| "/tmp/ocr-corpus".into());
    let dir = std::env::temp_dir().join(format!("dam-ocr-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");

    // 1) 开库 + 建资产（直接 INSERT，绕开导入向导；三张真文字图）
    let db_path = dir.join("dam.db");
    let conn = core_storage::open(&db_path).expect("open db");
    let mut asset_ids = Vec::new();
    for f in ["cn.png", "en.png", "mixed.png"] {
        let p = Path::new(&corpus).join(f);
        assert!(p.is_file(), "fixture missing: {}", p.display());
        conn.execute(
            "INSERT INTO assets (mediaType, absPath, storeMode, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES ('image', ?1, 'reference', ?2, 'png', 1, unixepoch(), unixepoch())",
            rusqlite::params![p.display().to_string(), f],
        )
        .expect("insert asset");
        asset_ids.push(conn.last_insert_rowid());
    }

    // 2) 入队 + 跑队列（真实模型）
    let enq = svc_pipeline::ocr_enq_untagged(&conn).expect("enqueue");
    assert_eq!(enq, 3, "应入队 3 个 job");
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let mut steps = Vec::new();
    let report = svc_pipeline::run_ocr_queue(&conn, &cancel, &mut |p| steps.push(p.status.clone()))
        .expect("run ocr queue");
    println!("report: processed={} done={} failed={} skipped={}", report.processed, report.done, report.failed, report.skipped);
    assert_eq!(report.done, 3, "三张全部成功");
    assert_eq!(report.failed, 0);

    // 3) 校验落库文本
    let mut hits = 0;
    for (i, id) in asset_ids.iter().enumerate() {
        let text: String = conn
            .query_row("SELECT ocr_text FROM assets WHERE assetId = ?1", [id], |r| r.get(0))
            .expect("ocr_text");
        println!("asset {id} ({}):\n---\n{text}\n---", ["cn.png", "en.png", "mixed.png"][i]);
        if !text.trim().is_empty() {
            hits += 1;
        }
        assert!(!text.trim().is_empty(), "asset {id} ocr_text 为空");
    }
    // 关键字断言（识别质量锚点）
    let all: String = asset_ids
        .iter()
        .map(|id| {
            conn.query_row("SELECT ocr_text FROM assets WHERE assetId = ?1", [id], |r| r.get::<_, String>(0))
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join("\n");
    for kw in ["数字资产管理", "Digital Asset Management", "SOUJPG DAM", "Model Release"] {
        assert!(all.contains(kw), "OCR 文本缺少关键字: {kw}\n{all}");
    }

    // 4) 检索融合：字面命中必须落在 FTS5 倒排索引上（OCR-MATCH 下推），
    //    同时保留 LIKE 口径对照（Raw 文本仍在 assets.ocr_text，未做物化替换）
    let n_like: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM assets WHERE ocr_text LIKE '%数字资产管理%' AND deleted = 0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(n_like >= 1, "LIKE 检索未命中 OCR 文本");
    let terms = core_storage::ocr_index::query_terms("数字资产管理");
    let indexed = core_storage::ocr_index::match_counts(&conn, &terms, 256).expect("index query");
    println!(
        "fusion: terms={terms:?} indexed_hits={} like_hits={n_like} indexed_rows={} dirty={}",
        indexed.len(),
        core_storage::ocr_index::indexed_count(&conn).unwrap(),
        core_storage::ocr_index::dirty_count(&conn).unwrap()
    );
    assert!(
        indexed.len() >= 3,
        "三张资产都应命中「数字资产管理」（FTS5 倒排路径），实际 {}",
        indexed.len()
    );
    assert_eq!(
        core_storage::ocr_index::dirty_count(&conn).unwrap(),
        0,
        "写路径（ocr_one）应已把索引与 dirty 收敛"
    );

    // 5) 统计 + 幂等：再次入队 = 0（增量语义）
    let stats = svc_pipeline::ocr_stats(&conn).expect("stats");
    assert_eq!(stats.done, 3);
    assert_eq!(stats.pending, 0);
    let again = svc_pipeline::ocr_enq_untagged(&conn).expect("re-enqueue");
    assert_eq!(again, 0, "增量入队应为 0（全部已有文本）");

    println!(
        "OCR_E2E_RESULT {{\"assets\":{},\"done\":{},\"failed\":{},\"steps\":{},\"keywords\":\"ok\",\"fusion_like\":\"ok\"}}",
        asset_ids.len(),
        report.done,
        report.failed,
        steps.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
