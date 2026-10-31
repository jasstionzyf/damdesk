//! OCR 文本检索索引（OCR-MATCH，卡 task）——把 `assets.ocr_text` 的字面检索
//! 从「全表 LIKE 扫描」下推到 SQLite FTS5 倒排（中文 2-gram + 单字允许）。
//!
//! ## 为什么分段在 Rust（而不是全在 SQL 触发器里算）
//!
//! 1. 分段规则必须按 **Unicode 文字类别**区分中文/拉丁（`char::is_alphanumeric()` +
//!    CJK 区段判定），并对两类给不同的最小长度口径（中文允许单字、拉丁/数字 ≥2 字符）。
//!    SQLite 没有字符类别函数（只有 `length`/`substr`/`GLOB`），在 SQL 里复刻等于把同一
//!    套规则写两遍，而索引侧与查询侧的口径必须逐字一致（差一个字就是静默漏召回）。
//! 2. 查询侧本来就在 Rust（`svc-library::fuse_ocr_text`）——单一真源就是本模块的
//!    [`segment`]（索引）/ [`query_terms`]（查询），两条路径共用 [`runs`] 切分。
//!
//! ## 一致性怎么保证（不靠自觉）
//!
//! `0014_ocr_fts.sql` 的触发器在 `ocr_text` 任何变更时做两件事：
//! ① 先按 rowid 撤掉旧索引行 —— 陈旧命中不可能残留（哪怕写来自绕过应用的 SQL）；
//! ② 写 `ocr_fts_dirty` 标记。所有入口（`core_storage::open()`）都会跑 [`sync_if_dirty`]，
//! 用本模块的同一分段函数把 dirty 行补建 —— 索引缺失会自愈（最多迟到一次 open）。
//! 分段算法升级 → `ocr_fts_state.segment_v` 不匹配 → 全库重标 dirty（同样自愈）。
//!
//! ## 存储形态
//!
//! FTS5 虚表 `ocr_fts(grams)`，**rowid = assetId**（删除/覆盖按 rowid O(1)，不需要在
//! 虚表上做非索引列扫描）。`grams` = 分段 token 以空格连接，tokenizer `unicode61`
//! 把每个 token 当独立倒排项（中文 token 无空白分隔 → 整 token 一项）。

use std::collections::HashMap;

use rusqlite::{params, Connection};

use crate::{Result, StorageError};

/// 分段算法版本：口径变化必须 +1（`sync_if_dirty` 会据此全库重建索引）。
pub const SEGMENT_VERSION: &str = "1";
/// 单 term 候选上限基准：命中面极大的字（如单字「的」）按重要性取头部截断，
/// 不把整个命中集拉进内存。k 越大留的候选越多（见 [`term_candidate_cap`]）。
pub const TERM_CANDIDATE_BASE: usize = 256;
/// bm25 排序预算（命中面 ≤ 该值才按 `rank` 排序）。
///
/// 为什么需要预算：FTS5 的 `ORDER BY rank` 必须为**全部**命中逐条算 bm25 才能给出
/// 头部（实测 ~0.6µs/命中：2 万命中面 = 12ms/term，8 个 token 就到 100ms，检索路径上
/// 不可接受）。命中面超过预算时退化为 rowid 序 LIMIT（实测 0.06ms），牺牲「重要性序」
/// 换有界成本；实测数字见 reports/OCR-MATCH.md 与 `scripts/ocr_match_acceptance.sh`。
pub const RANK_BUDGET: usize = 4096;
/// 每次查询最多使用的 term 数（超长查询按前 8 个 token）。
pub const MAX_QUERY_TERMS: usize = 8;
/// 迁移账本键（`ocr_fts_state.key`）。
pub const STATE_KEY_SEGMENT_V: &str = "segment_v";

/// 单 term 候选上限：`max(k*8, 256)`。
pub fn term_candidate_cap(k: usize) -> usize {
    (k.saturating_mul(8)).max(TERM_CANDIDATE_BASE)
}

/// 字符类别：中文（含假名/谚文）/ 拉丁数字 / 分隔。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Cjk,
    Word,
    Sep,
}

/// 东亚文字（中文为主；含日文假名、谚文、兼容表意 —— 「一字一义」类文字按字符切分）。
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{30FF}'   // 平假名 / 片假名
        | '\u{3400}'..='\u{4DBF}' // CJK 扩展 A
        | '\u{4E00}'..='\u{9FFF}' // CJK 基本区
        | '\u{F900}'..='\u{FAFF}' // CJK 兼容表意
        | '\u{AC00}'..='\u{D7AF}' // 谚文音节
        | '\u{1100}'..='\u{11FF}' // 谚文字母
        | '\u{FF66}'..='\u{FF9D}' // 半角片假名
    )
}

fn classify(c: char) -> Class {
    if is_cjk(c) {
        Class::Cjk
    } else if c.is_alphanumeric() {
        // 拉丁/数字/西里尔…（Unicode 类别 L*/N*，由 std 给出，不做 ASCII 码点猜测）
        Class::Word
    } else {
        Class::Sep
    }
}

/// 按字符类别切连续段（小写折叠；分隔字符丢弃，且**断开**相邻同类段 —— 否则
/// "发票 报销" 会被并成一段产出 票报 这种跨词 2-gram）。索引与查询共用，口径不可能漂移。
fn runs(text: &str) -> Vec<(Class, Vec<char>)> {
    let mut out: Vec<(Class, Vec<char>)> = Vec::new();
    let mut brk = true; // 段首（或前一个字符是分隔符）
    for c in text.chars() {
        let k = classify(c);
        if k == Class::Sep {
            brk = true;
            continue;
        }
        // 大小写折叠（CJK 恒等；个别字符折叠后长度变化也逐个收进同一 run）
        let folded: Vec<char> = c.to_lowercase().collect();
        match out.last_mut() {
            Some((ck, cs)) if *ck == k && !brk => cs.extend(folded),
            _ => out.push((k, folded)),
        }
        brk = false;
    }
    out
}

/// 索引口径 token：中文 = 全部单字 + 相邻 2-gram（单字保证「发」能命中「发票」，
/// 2-gram 是中文检索主口径）；拉丁/数字 = 整词（单字符不索引，查询侧同样不允许）。
pub fn segment(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (k, cs) in runs(text) {
        match k {
            Class::Cjk => {
                for c in &cs {
                    out.push(c.to_string());
                }
                for w in cs.windows(2) {
                    out.push(w.iter().collect());
                }
            }
            Class::Word => {
                if cs.len() >= 2 {
                    out.push(cs.iter().collect());
                }
            }
            Class::Sep => {}
        }
    }
    out
}

/// 查询口径 term：中文 ≥2 字 → 2-gram（不再额外产出单字，避免超长 query 被单字淹没）；
/// 中文单字 → 单字（允许）；拉丁/数字 → 整词（≥2 字符）。去重后截断到
/// [`MAX_QUERY_TERMS`]。
pub fn query_terms(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (k, cs) in runs(text) {
        match k {
            Class::Cjk if cs.len() == 1 => out.push(cs[0].to_string()),
            Class::Cjk => {
                for w in cs.windows(2) {
                    out.push(w.iter().collect());
                }
            }
            Class::Word if cs.len() >= 2 => out.push(cs.iter().collect()),
            _ => {}
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|t| seen.insert(t.clone()));
    out.truncate(MAX_QUERY_TERMS);
    out
}

/// token 序列 → FTS5 列内容（空格连接）。
pub fn grams_string(text: &str) -> String {
    segment(text).join(" ")
}

/// FTS5 查询串：单个 term 用短语引号包住（term 由分段产出，只含字母/数字/汉字，
/// 但仍转义引号，杜绝 FTS5 语法注入）。
fn match_query(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

/// 写入/覆盖一个资产的索引行（同 rowid 先删后插；FTS5 无 UPSERT）。
/// 写成功后清掉该资产的 dirty 标记（写路径同步的收口点）。
pub fn upsert(conn: &Connection, asset_id: i64, text: &str) -> Result<()> {
    remove(conn, asset_id)?;
    let grams = grams_string(text);
    if !grams.is_empty() {
        conn.execute(
            "INSERT INTO ocr_fts (rowid, grams) VALUES (?1, ?2)",
            params![asset_id, grams],
        )?;
    }
    conn.execute("DELETE FROM ocr_fts_dirty WHERE assetId = ?1", [asset_id])?;
    Ok(())
}

/// 撤掉一个资产的索引行（rowid 定位，O(1)）。
pub fn remove(conn: &Connection, asset_id: i64) -> Result<usize> {
    Ok(conn.execute("DELETE FROM ocr_fts WHERE rowid = ?1", [asset_id])?)
}

/// 单 term 命中总数（诊断/预算判定；postings 计数，实测 2 万命中面 0.37ms）。
pub fn term_match_count(conn: &Connection, term: &str) -> Result<i64> {
    let q = match_query(term);
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM ocr_fts WHERE ocr_fts MATCH ?1",
        [q],
        |r| r.get(0),
    )?)
}

/// 单 term 命中资产集（受 [`term_candidate_cap`] 截断）。
///
/// 索引里只有「未删且带文本」的资产（触发器在 `ocr_text`/`deleted` 变更时同步），
/// 所以这里不需要再联 `assets` 过滤 deleted —— 早先版本把 `rowid IN (SELECT assetId
/// FROM assets WHERE deleted = 0)` 塞进 FTS 查询，SQLite 只好 `SCAN assets` 驱动、
/// 并对每个匹配行重算 rank，20k 库实测 1148ms（见 reports/OCR-MATCH.md 排障）。
fn term_hits(conn: &Connection, term: &str, cap: usize) -> Result<Vec<i64>> {
    let q = match_query(term);
    let matches = term_match_count(conn, term)?;
    // 三档（成本/质量取舍，数字见 RANK_BUDGET 与 reports/OCR-MATCH.md）：
    //   命中面 ≤ cap        —— 全部候选都会被用到，排序不改变结果 → 直接 LIMIT（0.06ms 量级）
    //   命中面 ≤ RANK_BUDGET —— 要截断，按重要性（bm25 rank）取头部
    //   命中面 > RANK_BUDGET —— 排序成本失控，退化为 rowid 序 LIMIT（确定性 + 有界）
    let sql = if (matches as usize) <= cap {
        "SELECT rowid FROM ocr_fts WHERE ocr_fts MATCH ?1 LIMIT ?2"
    } else if (matches as usize) <= RANK_BUDGET {
        "SELECT rowid FROM ocr_fts WHERE ocr_fts MATCH ?1 ORDER BY rank LIMIT ?2"
    } else {
        "SELECT rowid FROM ocr_fts WHERE ocr_fts MATCH ?1 LIMIT ?2"
    };
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(params![q, cap as i64], |r| r.get::<_, i64>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 每个候选资产命中的 term 数（n）：逐 term 取命中集后在内存累计（term ≤ 8）。
/// 返回按 (命中数降序, assetId 升序) 排序 —— 顺序确定，便于截断与测试。
pub fn match_counts(conn: &Connection, terms: &[String], cap: usize) -> Result<Vec<(i64, usize)>> {
    let mut counts: HashMap<i64, usize> = HashMap::new();
    for t in terms {
        for id in term_hits(conn, t, cap)? {
            *counts.entry(id).or_insert(0) += 1;
        }
    }
    let mut v: Vec<(i64, usize)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    Ok(v)
}

/// 取若干资产的 ocr_text（命中片段用；有界 IN 查询，不是全表扫描）。
pub fn texts(conn: &Connection, ids: &[i64]) -> Result<Vec<(i64, String)>> {
    let mut out = Vec::new();
    for chunk in ids.chunks(500) {
        let ph = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT assetId, ocr_text FROM assets
             WHERE deleted = 0 AND ocr_text <> '' AND assetId IN ({ph})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        out.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
    }
    Ok(out)
}

/// dirty 标记（触发器/写路径共用）。
pub fn mark_dirty(conn: &Connection, asset_id: i64) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO ocr_fts_dirty (assetId, ts) VALUES (?1, unixepoch())",
        [asset_id],
    )?;
    Ok(())
}

/// 索引是否需要维护（廉价探测：版本 + dirty 存在性，都是小表查询）。
pub fn needs_sync(conn: &Connection) -> Result<bool> {
    let v: Option<String> = conn
        .query_row(
            "SELECT value FROM ocr_fts_state WHERE key = ?1",
            [STATE_KEY_SEGMENT_V],
            |r| r.get(0),
        )
        .ok();
    if v.as_deref() != Some(SEGMENT_VERSION) {
        return Ok(true);
    }
    let exists: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM ocr_fts_dirty LIMIT 1)",
        [],
        |r| r.get(0),
    )?;
    Ok(exists != 0)
}

/// 索引自愈：分段算法版本不一致 → 全库重标 dirty 并按新口径重建；
/// 然后按 dirty 队列逐行重建（text 空 / 已删 → 只留删除）。返回重建行数。
///
/// 入口：`core_storage::open()`（进程启动/每次连接）都会跑一次 [`needs_sync`] 探测，
/// 无 dirty 时是两次小表查询（微秒级），有 dirty 时才做重活。
pub fn sync_if_dirty(conn: &Connection) -> Result<usize> {
    if !needs_sync(conn)? {
        return Ok(0);
    }
    let tx = conn.unchecked_transaction()?;
    let ver: Option<String> = tx
        .query_row(
            "SELECT value FROM ocr_fts_state WHERE key = ?1",
            [STATE_KEY_SEGMENT_V],
            |r| r.get(0),
        )
        .ok();
    if ver.as_deref() != Some(SEGMENT_VERSION) {
        // 分段口径变了：旧索引整表作废（不留按旧口径切的 token），全库重标
        tx.execute("DELETE FROM ocr_fts", [])?;
        tx.execute(
            "INSERT OR IGNORE INTO ocr_fts_dirty (assetId, ts)
             SELECT assetId, unixepoch() FROM assets WHERE ocr_text <> '' AND deleted = 0",
            [],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO ocr_fts_state (key, value) VALUES (?1, ?2)",
            params![STATE_KEY_SEGMENT_V, SEGMENT_VERSION],
        )?;
    }
    let rows: Vec<(i64, String, i64)> = {
        let mut stmt = tx.prepare(
            "SELECT d.assetId, COALESCE(a.ocr_text, ''), COALESCE(a.deleted, 0)
             FROM ocr_fts_dirty d LEFT JOIN assets a ON a.assetId = d.assetId",
        )?;
        let it = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        it.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let n = rows.len();
    for (asset_id, text, deleted) in rows {
        tx.execute("DELETE FROM ocr_fts WHERE rowid = ?1", [asset_id])?;
        if deleted == 0 && !text.trim().is_empty() {
            let grams = grams_string(&text);
            if !grams.is_empty() {
                tx.execute(
                    "INSERT INTO ocr_fts (rowid, grams) VALUES (?1, ?2)",
                    params![asset_id, grams],
                )?;
            }
        }
        tx.execute("DELETE FROM ocr_fts_dirty WHERE assetId = ?1", [asset_id])?;
    }
    tx.commit()?;
    Ok(n)
}

/// 索引行数（诊断/验收：与「有 ocr_text 的资产数」核对覆盖度）。
pub fn indexed_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM ocr_fts", [], |r| r.get(0))?)
}

/// 未收敛的 dirty 行数（0 = 索引与写入已同步）。
pub fn dirty_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM ocr_fts_dirty", [], |r| r.get(0))?)
}

/// 查询计划（验收存证：确认走 FTS5 倒排而非全表扫描）。
pub fn explain_query_plan(conn: &Connection, terms: &[String], cap: usize) -> Result<Vec<String>> {
    let term = terms.first().cloned().unwrap_or_default();
    let matches = term_match_count(conn, &term)?;
    let sql = if (matches as usize) > cap && (matches as usize) <= RANK_BUDGET {
        "EXPLAIN QUERY PLAN SELECT rowid FROM ocr_fts WHERE ocr_fts MATCH ?1 ORDER BY rank LIMIT ?2"
    } else {
        "EXPLAIN QUERY PLAN SELECT rowid FROM ocr_fts WHERE ocr_fts MATCH ?1 LIMIT ?2"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![match_query(&term), cap as i64], |r| {
        r.get::<_, String>(3)
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 迁移账本表缺失时的响亮错误（迁移没跑到 = 不能静默当空索引）。
pub fn ensure_ready(conn: &Connection) -> Result<()> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('ocr_fts_dirty', 'ocr_fts_state')",
        [],
        |r| r.get(0),
    )?;
    if n != 2 {
        return Err(StorageError::Invalid(
            "ocr_fts 索引表缺失（迁移 0014 未应用）".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::migrations::migrate(&conn).unwrap();
        conn
    }

    fn insert_asset(conn: &Connection, id: i64, text: &str) {
        conn.execute(
            "INSERT INTO assets (assetId, absPath, fileName, fileExt, fileSz, mtime, importedAt, mediaType, ocr_text)
             VALUES (?1, ?2, ?3, 'png', 1, 1, 1, 'image', ?4)",
            params![id, format!("/a/{id}.png"), format!("{id}.png"), text],
        )
        .unwrap();
    }

    #[test]
    fn segment_cuts_chinese_to_unigrams_and_bigrams() {
        let t = segment("数字资产管理");
        // 4 个单字 + 3 个 2-gram
        assert_eq!(
            t,
            vec![
                "数", "字", "资", "产", "管", "理", "数字", "字资", "资产", "产管", "管理"
            ]
        );
    }

    #[test]
    fn segment_keeps_latin_words_and_drops_single_chars() {
        let t = segment("Digital Asset Management SKU-8891 a");
        assert!(t.contains(&"digital".to_string()), "{t:?}");
        assert!(t.contains(&"asset".to_string()), "{t:?}");
        assert!(t.contains(&"management".to_string()), "{t:?}");
        assert!(t.contains(&"sku".to_string()), "{t:?}");
        assert!(t.contains(&"8891".to_string()), "{t:?}");
        assert!(!t.contains(&"a".to_string()), "单字符拉丁不索引：{t:?}");
    }

    #[test]
    fn segment_handles_mixed_script_adjacent() {
        // 中英紧邻不分词（类别切换即断句）：SKU-8891降价 → sku / 8891 / 降 / 价 / 降价
        let t = segment("SKU-8891降价");
        assert_eq!(t, vec!["sku", "8891", "降", "价", "降价"]);
    }

    #[test]
    fn query_terms_rules() {
        assert_eq!(query_terms("发票"), vec!["发票"]);
        assert_eq!(query_terms("发"), vec!["发"], "中文单字允许");
        assert!(query_terms("A").is_empty(), "拉丁单字符不允许");
        assert_eq!(query_terms("AB"), vec!["ab"]);
        assert_eq!(query_terms("SKU-8891"), vec!["sku", "8891"]);
        assert_eq!(
            query_terms("发票 发票"),
            vec!["发票"],
            "重复 term 去重（命中计数按去重后的 term 数算）"
        );
        assert_eq!(
            query_terms("一二三四五六七八九十").len(),
            MAX_QUERY_TERMS,
            "超长 query 截断到 8 个 term"
        );
        assert!(query_terms("，。 ！？").is_empty(), "纯标点无 term");
    }

    #[test]
    fn write_path_upsert_and_dirty_settled() {
        let c = setup();
        insert_asset(&c, 1, "数字资产管理平台");
        assert_eq!(dirty_count(&c).unwrap(), 1, "SQL 写入 → 触发器标 dirty");
        assert_eq!(indexed_count(&c).unwrap(), 0, "此时还没建索引行");
        let n = sync_if_dirty(&c).unwrap();
        assert_eq!(n, 1);
        assert_eq!(dirty_count(&c).unwrap(), 0);
        assert_eq!(indexed_count(&c).unwrap(), 1);
        assert!(!needs_sync(&c).unwrap(), "收敛后探测为 false");

        // 走写路径（ocr_one 的收口点）：直接 upsert，dirty 保持 0
        upsert(&c, 1, "发票号 8891").unwrap();
        assert_eq!(dirty_count(&c).unwrap(), 0);
        let hits = term_hits(&c, "发票", 100).unwrap();
        assert_eq!(hits, vec![1]);
        assert!(term_hits(&c, "资产", 100).unwrap().is_empty(), "旧文本已撤");
    }

    #[test]
    fn update_removes_stale_index_row_immediately() {
        let c = setup();
        insert_asset(&c, 1, "数字资产管理平台");
        sync_if_dirty(&c).unwrap();
        assert_eq!(term_hits(&c, "资产", 10).unwrap(), vec![1]);

        // 绕过应用的 SQL 改写（陈旧命中必须当场消失，而不是等 sync）
        c.execute(
            "UPDATE assets SET ocr_text = '无关内容' WHERE assetId = 1",
            [],
        )
        .unwrap();
        assert!(
            term_hits(&c, "资产", 10).unwrap().is_empty(),
            "旧索引行已由触发器撤掉"
        );
        assert_eq!(dirty_count(&c).unwrap(), 1);
        // 下一次 open 式探测自愈
        assert!(needs_sync(&c).unwrap());
        sync_if_dirty(&c).unwrap();
        assert_eq!(term_hits(&c, "无关", 10).unwrap(), vec![1]);
    }

    #[test]
    fn hard_delete_removes_index_row_and_soft_delete_is_filtered() {
        let c = setup();
        insert_asset(&c, 1, "授权书 模板");
        insert_asset(&c, 2, "授权书 单位");
        sync_if_dirty(&c).unwrap();
        let terms = query_terms("授权书");
        assert_eq!(match_counts(&c, &terms, 10).unwrap().len(), 2);

        // 软删除：索引行由触发器撤掉（索引只保留「未删且带文本」的资产），并标 dirty
        c.execute("UPDATE assets SET deleted = 1 WHERE assetId = 1", [])
            .unwrap();
        assert_eq!(
            match_counts(&c, &terms, 10).unwrap(),
            vec![(2, terms.len())]
        );
        assert_eq!(
            dirty_count(&c).unwrap(),
            1,
            "软删除 → 待建标记（sync 会按 deleted 收敛）"
        );
        assert_eq!(sync_if_dirty(&c).unwrap(), 1);
        assert_eq!(dirty_count(&c).unwrap(), 0);
        assert!(
            match_counts(&c, &terms, 10)
                .unwrap()
                .iter()
                .all(|(id, _)| *id != 1),
            "已删资产不得因 sync 回到索引"
        );

        // 恢复：deleted 0 → 重新标 dirty → sync 重建索引行
        c.execute("UPDATE assets SET deleted = 0 WHERE assetId = 1", [])
            .unwrap();
        assert_eq!(sync_if_dirty(&c).unwrap(), 1);
        assert_eq!(
            match_counts(&c, &terms, 10).unwrap().len(),
            2,
            "恢复后重新可搜"
        );

        // 硬删除：索引行与待建标记都由触发器清掉
        c.execute("DELETE FROM assets WHERE assetId = 2", [])
            .unwrap();
        assert_eq!(match_counts(&c, &terms, 10).unwrap().len(), 1);
        assert_eq!(dirty_count(&c).unwrap(), 0, "物理删除不需要重建");
    }

    #[test]
    fn large_match_face_falls_back_to_rowid_order() {
        // 命中面 > RANK_BUDGET（4096）时退化为 rowid 序 LIMIT（成本有界）：
        // 判据 = 取回的候选是 rowid 升序的前 cap 条（rank 序不会这么规整）
        let c = setup();
        let n = RANK_BUDGET + 1000;
        c.execute_batch("BEGIN").unwrap();
        for i in 1..=n as i64 {
            insert_asset(&c, i, "发票 号码 存根");
            upsert(&c, i, "发票 号码 存根").unwrap();
        }
        c.execute_batch("COMMIT").unwrap();
        let terms = query_terms("发票");
        assert!(term_match_count(&c, "发票").unwrap() > RANK_BUDGET as i64);
        let got = match_counts(&c, &terms, 10).unwrap();
        let ids: Vec<i64> = got.iter().map(|(i, _)| *i).collect();
        assert_eq!(
            ids,
            (1..=10).collect::<Vec<i64>>(),
            "大命中面 → rowid 序头部"
        );
    }

    #[test]
    fn match_counts_counts_terms_and_caps_candidates() {
        let c = setup();
        insert_asset(&c, 1, "发票 发票号");
        insert_asset(&c, 2, "发票 报销单");
        insert_asset(&c, 3, "报销单 模板");
        sync_if_dirty(&c).unwrap();

        let terms = query_terms("发票 报销");
        assert_eq!(terms, vec!["发票", "报销"]);
        let got = match_counts(&c, &terms, 100).unwrap();
        // 2 命中两个 term（n=2）排最前；1/3 各只命中一个（n=1）按 assetId 升序
        assert_eq!(got, vec![(2, 2), (1, 1), (3, 1)]);

        // 候选上限：每个 term 只取 rank 头部 cap 条
        let capped = match_counts(&c, &terms, 1).unwrap();
        assert!(capped.iter().all(|(_, n)| *n <= 2));
        assert!(
            capped.len() <= 2,
            "两 term 各 1 条 → 至多 2 个候选：{capped:?}"
        );
    }

    #[test]
    fn index_and_query_use_the_same_cut_for_single_char_query() {
        let c = setup();
        insert_asset(&c, 1, "发票号 8891");
        insert_asset(&c, 2, "报销单");
        sync_if_dirty(&c).unwrap();
        // 「发」单字命中「发票号」（旧 LIKE 口径 ≥2 字符会漏掉这条）
        assert_eq!(term_hits(&c, "发", 10).unwrap(), vec![1]);
        assert_eq!(term_hits(&c, "发票", 10).unwrap(), vec![1]);
        // 单字符数字不参与（索引与查询同口径）
        assert!(query_terms("8").is_empty());
    }

    #[test]
    fn segment_version_bump_rebuilds_everything() {
        let c = setup();
        insert_asset(&c, 1, "数字资产");
        sync_if_dirty(&c).unwrap();
        // 模拟口径升级：把账本版本改成旧值 → 应触发全库重标并重建
        c.execute(
            "UPDATE ocr_fts_state SET value = '0' WHERE key = ?1",
            [STATE_KEY_SEGMENT_V],
        )
        .unwrap();
        assert!(needs_sync(&c).unwrap());
        let n = sync_if_dirty(&c).unwrap();
        assert_eq!(n, 1);
        assert_eq!(indexed_count(&c).unwrap(), 1);
        assert_eq!(term_hits(&c, "资产", 10).unwrap(), vec![1]);
    }

    #[test]
    fn query_plan_uses_fts_index() {
        let c = setup();
        insert_asset(&c, 1, "数字资产管理");
        sync_if_dirty(&c).unwrap();
        let plan = explain_query_plan(&c, &query_terms("资产"), 256).unwrap();
        let joined = plan.join(" | ");
        assert!(
            joined.contains("VIRTUAL TABLE INDEX"),
            "查询计划应使用 FTS5 虚拟表索引：{joined}"
        );
    }

    #[test]
    fn texts_fetch_is_bounded_and_skips_deleted() {
        let c = setup();
        insert_asset(&c, 1, "发票号 8891");
        insert_asset(&c, 2, "已删除");
        c.execute("UPDATE assets SET deleted = 1 WHERE assetId = 2", [])
            .unwrap();
        let got = texts(&c, &[1, 2, 999]).unwrap();
        assert_eq!(got, vec![(1, "发票号 8891".to_string())]);
    }
}
