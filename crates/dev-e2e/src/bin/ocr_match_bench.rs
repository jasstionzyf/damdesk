//! OCR-MATCH 基准/对拍（卡 t_df883cdd）：字面检索「全表 LIKE 扫描」vs「FTS5 倒排」。
//!
//! 用法（容器内）：
//!   cargo run --release -p dev-e2e --bin ocr_match_bench [资产数=20000] [带文本比例=0.6]
//!
//! 两个语料，分别回答两个不同的问题（都是本机实测，不写死结论）：
//!
//!   A) **真实语料**（N 张，≈1.5KB/篇，关键词与填充词共享 2-gram，如「合同编号」vs 填充词
//!      「资产编号」）：断言 旧命中集 ⊆ 新命中集（下推**不会漏召回**），并打印「新多出来的
//!      部分」与原因样本 —— 这是 2-gram 口径的**预期变宽**，必须量化而不是藏着。
//!   B) **净化语料**（填充词刻意不含任何查询 2-gram）：逐查询断言新旧命中集**完全一致**
//!      —— 证明文本里真有该词时，倒排既不漏也不凭空多召回。
//!
//! 另外：覆盖度（indexed == 带文本资产数 / dirty == 0）、单字中文召回（旧口径 <2 字符
//! 恒丢）、候选截断（大命中面按 rank 取头部）、EXPLAIN QUERY PLAN（必须走 VIRTUAL TABLE
//! INDEX）与逐查询耗时（avg/p50/max）。
//!
//! 退出码 0 = 全部断言通过。

use std::time::Instant;

use rusqlite::{Connection, params};

/// 查询词（真实工单/票据场景词）。
const QUERIES: &[&str] = &["授权书", "发票号", "数字资产", "报销", "合同编号", "Model Release"];

fn ocr_text_for(i: i64, scrub: bool) -> String {
    // 真实语料：填充词撞关键词 2-gram（授权 / 资产 / 编号…）
    // 净化语料：填充词刻意避开所有查询 2-gram，只在「关键词行」出现查询词
    let (base, filler) = if scrub {
        ("图库平台 档案 DAM-{:06}", "模型声明 拍摄日期 摄影师 张伟 影像 号 IMG-{:06}-{}")
    } else {
        ("数字资产管理平台 资产编号 DAM-{:06}", "模型授权 拍摄日期 摄影师 张伟 编号 IMG-{:06}-{}")
    };
    let mut s = String::with_capacity(1600);
    s.push_str(&format!("{}\n", base.replace("{:06}", &format!("{i:06}"))));
    for line in 0..12 {
        s.push_str(&format!(
            "第{}行 {} 2026-{:02}-{:02}\n",
            line + 1,
            filler
                .replace("{:06}", &format!("{i:06}"))
                .replace("{}", &line.to_string()),
            (i % 12) + 1,
            (i % 28) + 1
        ));
    }
    if scrub {
        // 关键词行（每个词各自出现，命中面小）
        let rare = ["授权书", "发票号", "数字资产", "合同编号", "报销单"][(i as usize) % 5];
        if i % 40 == 0 {
            s.push_str(&format!("{rare} 盖章 生效\n"));
        }
    } else {
        let rare = ["授权书", "发票号", "合同编号", "Model Release"][(i as usize) % 4];
        if i % 40 == 0 {
            s.push_str(&format!("{rare} 盖章 生效\n"));
        }
        // 高频词（大命中面：走候选截断路径）
        let common = ["数字资产", "拍摄日期", "资产编号"][(i as usize) % 3];
        if i % 7 == 0 {
            s.push_str(&format!("{common} 明细\n"));
        }
    }
    if i % 11 == 0 {
        s.push_str("Digital Asset Management Model Release SOUJPG-DAM\n");
    }
    s
}

fn build_lib(conn: &Connection, n: i64, ocr_fraction: f64, scrub: bool) -> i64 {
    conn.execute_batch("BEGIN").unwrap();
    let mut with_text = 0i64;
    for i in 1..=n {
        conn.execute(
            "INSERT INTO assets (assetId, absPath, fileName, fileExt, fileSz, mtime, importedAt, mediaType)
             VALUES (?1, ?2, ?3, 'png', 1000, 0, ?4, 'image')",
            params![i, format!("/lib/{i}.png"), format!("{i}.png"), i],
        )
        .unwrap();
        if (i as f64) <= n as f64 * ocr_fraction {
            let text = ocr_text_for(i, scrub);
            conn.execute(
                "UPDATE assets SET ocr_text = ?2 WHERE assetId = ?1",
                params![i, text],
            )
            .unwrap();
            core_storage::ocr_index::upsert(conn, i, &text).unwrap();
            with_text += 1;
        }
    }
    conn.execute_batch("COMMIT").unwrap();
    with_text
}

/// 旧口径（合并前实现）：全表取 `(assetId, ocr_text)` → Rust 侧逐 token `contains`。
fn old_like_scan(conn: &Connection, tokens: &[String]) -> Vec<i64> {
    let mut stmt = conn
        .prepare("SELECT assetId, ocr_text FROM assets WHERE deleted = 0 AND ocr_text != ''")
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
        .unwrap();
    let mut out = Vec::new();
    for row in rows {
        let (id, text) = row.unwrap();
        if tokens.iter().any(|t| text.contains(t.as_str())) {
            out.push(id);
        }
    }
    out.sort_unstable();
    out
}

fn legacy_tokens(q: &str) -> Vec<String> {
    q.split_whitespace()
        .filter(|t| t.chars().count() >= 2)
        .map(|t| t.to_string())
        .collect()
}

fn ids_of(v: &[(i64, usize)]) -> Vec<i64> {
    let mut ids: Vec<i64> = v.iter().map(|(i, _)| *i).collect();
    ids.sort_unstable();
    ids
}

fn timed<T>(f: impl Fn() -> T) -> (T, f64) {
    let t = Instant::now();
    let out = f();
    (out, t.elapsed().as_secs_f64() * 1000.0)
}

fn stats(samples: &[f64]) -> (f64, f64, f64) {
    let mut v = samples.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg = v.iter().sum::<f64>() / v.len() as f64;
    (avg, v[v.len() / 2], *v.last().unwrap())
}

fn open_lib(name: &str) -> Connection {
    let dir = std::env::temp_dir().join(format!("dam-ocrmatch-bench-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    core_storage::open(&dir.join("dam.db")).unwrap()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let n: i64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let frac: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(0.6);
    let cap = core_storage::ocr_index::term_candidate_cap(24); // 生产上限（k=24 → 256/term）

    // ---------- A) 真实语料 ----------
    let a = open_lib("real");
    println!("== A) 真实语料：{n} 张资产（{:.0}% 带文本，≈1.5KB/篇）", frac * 100.0);
    let t0 = Instant::now();
    let with_text = build_lib(&a, n, frac, false);
    println!("   建库+建索引 {:.1}s；带文本 {with_text}", t0.elapsed().as_secs_f64());
    let indexed = core_storage::ocr_index::indexed_count(&a).unwrap();
    let dirty = core_storage::ocr_index::dirty_count(&a).unwrap();
    println!("   索引行 {indexed} / dirty {dirty}");
    assert_eq!(indexed, with_text, "索引覆盖度必须等于带文本资产数");
    assert_eq!(dirty, 0, "写路径应清掉 dirty 标记");

    let single = core_storage::ocr_index::match_counts(&a, &["授".to_string()], cap).unwrap();
    println!(
        "   单字「授」：新路径命中 {} 条（cap={cap} 截断；旧口径 token <2 字符直接丢弃 → 恒 0）",
        single.len()
    );
    assert!(!single.is_empty(), "单字中文必须能命中（卡判据）");

    println!("\n== A) 对拍：旧 ⊆ 新（下推不漏召回）+ 量化 2-gram 变宽");
    println!(
        "{:<14} {:>10} {:>10} {:>10} | 关系",
        "查询词", "旧命中", "新命中(全量)", "新命中(cap)"
    );
    let mut widened: Vec<(&str, usize, usize, String)> = Vec::new();
    for q in QUERIES {
        let tokens = legacy_tokens(q);
        let terms = core_storage::ocr_index::query_terms(q);
        assert!(!tokens.is_empty() && !terms.is_empty(), "查询词应产出 token：{q}");
        let old = old_like_scan(&a, &tokens);
        let new_all = core_storage::ocr_index::match_counts(&a, &terms, 1_000_000).unwrap();
        let new_capped = core_storage::ocr_index::match_counts(&a, &terms, cap).unwrap();
        let new_ids = ids_of(&new_all);
        assert!(
            old.iter().all(|id| new_ids.binary_search(id).is_ok()),
            "下推不得漏召回（旧有新无）：{q}"
        );
        assert!(
            ids_of(&new_capped)
                .iter()
                .all(|id| new_ids.binary_search(id).is_ok()),
            "截断结果必须是全量命中的子集：{q}"
        );
        let extra: Vec<i64> = new_ids
            .iter()
            .copied()
            .filter(|id| old.binary_search(id).is_err())
            .collect();
        let why = if extra.is_empty() {
            "完全一致".to_string()
        } else {
            format!(
                "多 {} 条（2-gram 命中填充词：{}）",
                extra.len(),
                sample_reason(&a, extra[0], &terms)
            )
        };
        println!(
            "{:<14} {:>10} {:>10} {:>10} | {}",
            q,
            old.len(),
            new_ids.len(),
            new_capped.len(),
            why
        );
        if !extra.is_empty() {
            widened.push((q, old.len(), new_ids.len(), why));
        }
    }
    assert!(!widened.is_empty(), "真实语料上应有可量化的 2-gram 变宽样本（口径变更必须可见）");

    println!("\n== A) 计时（旧 = 全表扫描 + 逐行 LIKE；新 = FTS5 倒排 + 生产 cap）");
    println!(
        "{:<14} | {:>9} {:>9} {:>9} | {:>9} {:>9} {:>9} | {:>7} | 截断口径",
        "查询词", "旧avg(ms)", "旧p50", "旧max", "新avg(ms)", "新p50", "新max", "加速"
    );
    let mut old_all = 0.0f64;
    let mut new_all = 0.0f64;
    for q in QUERIES {
        let tokens = legacy_tokens(q);
        let terms = core_storage::ocr_index::query_terms(q);
        let face: Vec<i64> = terms
            .iter()
            .map(|t| core_storage::ocr_index::term_match_count(&a, t).unwrap())
            .collect();
        let branch = if face.iter().any(|c| *c as usize > core_storage::ocr_index::RANK_BUDGET) {
            "rowid 序（超 RANK_BUDGET）"
        } else {
            "rank（bm25 重要性序）"
        };
        let mut old_s = Vec::new();
        let mut new_s = Vec::new();
        for _ in 0..3 {
            let (_, ms) = timed(|| old_like_scan(&a, &tokens));
            old_s.push(ms);
            let (_, ms) = timed(|| core_storage::ocr_index::match_counts(&a, &terms, cap).unwrap());
            new_s.push(ms);
        }
        let (oa, op, om) = stats(&old_s);
        let (na, np, nm) = stats(&new_s);
        old_all += oa;
        new_all += na;
        println!(
            "{:<14} | {:>9.2} {:>9.2} {:>9.2} | {:>9.2} {:>9.2} {:>9.2} | {:>6.1}x | {} 命中面={:?}",
            q,
            oa,
            op,
            om,
            na,
            np,
            nm,
            oa / na.max(0.001),
            branch,
            face
        );
    }
    println!(
        "   合计 avg：旧 {old_all:.2}ms / 新 {new_all:.2}ms（{:.1}x）",
        old_all / new_all
    );
    assert!(
        new_all < old_all,
        "新路径必须更快（实测 旧 {old_all:.2}ms vs 新 {new_all:.2}ms）"
    );

    let common_terms = core_storage::ocr_index::query_terms("数字资产");
    let capped128 = core_storage::ocr_index::match_counts(&a, &common_terms, 128).unwrap();
    let uncapped = core_storage::ocr_index::match_counts(&a, &common_terms, 100_000).unwrap();
    println!(
        "\n== A) 候选截断：词「数字资产」cap=128 → {} 条；cap=100000 → {} 条",
        capped128.len(),
        uncapped.len()
    );
    assert!(
        capped128.len() <= 128 * common_terms.len(),
        "cap 是 per-term 上限（并集可超过 128）：{}",
        capped128.len()
    );

    println!("\n== A) EXPLAIN QUERY PLAN（新路径）");
    let plan = core_storage::ocr_index::explain_query_plan(&a, &common_terms, cap).unwrap();
    for l in &plan {
        println!("   {l}");
    }
    assert!(
        plan.join(" ").contains("VIRTUAL TABLE INDEX"),
        "查询计划必须走 FTS5 虚拟表索引"
    );

    // ---------- B) 净化语料：逐查询完全一致 ----------
    let nb = (n / 2).max(2000);
    let b = open_lib("clean");
    println!("\n== B) 净化语料（填充词不含任何查询 2-gram）：{nb} 张资产");
    build_lib(&b, nb, 1.0, true);
    println!(
        "   索引行 {} / dirty {}",
        core_storage::ocr_index::indexed_count(&b).unwrap(),
        core_storage::ocr_index::dirty_count(&b).unwrap()
    );
    println!(
        "{:<14} {:>10} {:>10} | 命中集",
        "查询词", "旧命中", "新命中"
    );
    for q in QUERIES {
        let tokens = legacy_tokens(q);
        let terms = core_storage::ocr_index::query_terms(q);
        let old = old_like_scan(&b, &tokens);
        let new_ids = ids_of(&core_storage::ocr_index::match_counts(&b, &terms, 1_000_000).unwrap());
        println!("{:<14} {:>10} {:>10} | {}", q, old.len(), new_ids.len(), if old == new_ids { "完全一致 ✓" } else { "不一致 ✗" });
        assert_eq!(old, new_ids, "净化语料上命中集必须完全一致：{q}");
    }

    println!(
        "\nOCR_MATCH_BENCH_RESULT {{\"assets\":{n},\"with_text\":{with_text},\"indexed\":{indexed},\"dirty\":{dirty},\"old_ms\":{old_all:.2},\"new_ms\":{new_all:.2},\"speedup\":{:.1},\"single_char_hits\":{},\"clean_exact_parity\":true}}",
        old_all / new_all,
        single.len()
    );
}

/// 解释「多出来的字面命中」：打印该资产文本里含查询 token 的那一行。
fn sample_reason(conn: &Connection, asset_id: i64, terms: &[String]) -> String {
    let text: String = conn
        .query_row(
            "SELECT ocr_text FROM assets WHERE assetId = ?1",
            [asset_id],
            |r| r.get(0),
        )
        .unwrap_or_default();
    for line in text.split('\n') {
        if terms.iter().any(|t| line.contains(t.as_str())) {
            return format!("#{asset_id} {:?}", line.trim());
        }
    }
    format!("#{asset_id}（命中 token 在词内/跨词位置）")
}
