//! OCR-MATCH 评测（卡 t_df883cdd）：带标注小语料（40 张「图」的 OCR 文本）× 20 条查询，
//! 程序化输出「命中率 / 误召回 / top-10 精确率 / 耗时」表，并**与旧实现对比**。
//!
//! 用法（容器内）：
//!   cargo run --release -p dev-e2e --bin ocr_match_eval
//!
//! 为什么要它：基准（ocr_match_bench）比的是同规模下的「不漏召回 + 更快」，
//! 但卡面还要求**精度侧**的表——2-gram 口径会变宽（误召回），必须量化并说明它凭什么还能用。
//!
//! 真值定义（与实现无关，程序化）：**图上的 OCR 文本里真的出现了该查询的词**。
//!   - 中文/混合：子串出现（如「发票号」出现在「发票号 A-001」里）
//!   - 英文：大小写无关的**整词**出现；但子串型查询（`anage`）按子串判真假，
//!     用来暴露「新口径是词级、旧口径是子串级」这一**有意的口径取舍**（表里标 `口径取舍`）
//! 指标：
//!   召回率 = |新命中 ∩ 真值| / |真值|；误召回 = |命中 \ 真值|；
//!   top-10 精确率 = 按分数排序前 10 条里真值占比（衡量误召回是否被部分命中折扣压到后排）。
//!
//! 断言（任一失败退出码非 0）：旧命中集 ⊆ 新命中集；新召回率 ≥ 旧召回率；
//! 真值非空的查询新召回率 = 100%；中文单字查询旧 0% → 新 100%。

use std::time::Instant;

use rusqlite::Connection;

/// 标注语料：39 张「图」的 OCR 行文本（按真实票据/合同/图库场景手写，含混排与陷阱文本）。
/// 陷阱文本（只含部分 2-gram、不含整词）用来把「2-gram 变宽」量出来：
/// 「模型授权」不含「授权书」；「资产编号」不含「合同编号」；「肖像 授权书」（中间有空格）
/// 不含连续的「肖像授权」。
const CORPUS: &[&str] = &[
    // 票据类（含「发票」「发票号」「报销单」「报销」「增值税」）
    "发票号 A-001\n合计 1200 元\n抬头 北京某某科技有限公司",
    "发票号 A-002\n合计 860 元\n抬头 上海某某商贸",
    "发票 明细\n报销单 编号 BX-77\n审批 张三",
    "报销单 BX-78\n差旅 报销 合计 3200 元",
    "增值税 专用 发票\n发票号 A-009\n税率 13%",
    "增值税 普通 发票\n价税合计 500 元",
    "发票 存根\n开票日期 2026-03-01",
    "报销 凭证\n附件 3 张",
    "发票号 A-010 发票 存根",
    "差旅 报销单 BX-90\n发票号 A-011",
    // 合同类（含「合同编号」）
    "合同编号 HT-2026-001\n甲方 某某集团\n乙方 某某服务",
    "合同编号 HT-2026-002\n有效期 2026-01-01 至 2027-01-01",
    "合同 附件 一\n合同编号 HT-2026-003",
    "合同编号 HT-2026-004\n签署 日期 2026-05-09",
    // 肖像授权类（含「授权书」「肖像授权」）
    "肖像 授权书\n模特 李某某\n授权 范围 全球",
    "肖像 授权书\n模特 王某某\n有效期 两年",
    "肖像授权 合同\n授权书 编号 SQ-9\n肖像 使用 许可",
    "Model Release 肖像 授权书\n拍摄 日期 2026-04-01",
    // 陷阱：只有 2-gram 的片段，没有整词（用于量化误召回）
    "模型 授权 拍摄 日期 2026-02-02\n摄影师 张伟",
    "资产 编号 DAM-000001\n入库 日期 2026-01-09",
    "合同 条款 三\n资产 归属 说明",
    // 图库/资产类（含「数字资产」）
    "数字资产管理平台\n资产编号 DAM-000123",
    "数字资产 入库 明细\n合计 420 张",
    "数字 资产 报表\n导出 日期 2026-06-01",
    // 促销/商品类（含「降价」、SKU-8891）
    "SKU-8891降价促销\n原价 199 现价 129",
    "降价 通知\nSKU-8891 限时 三天",
    "SKU-8892 上新\n无 降价",
    // 纯英文
    "Digital Asset Management\nInvoice No. 8891\nTotal 1200 CNY",
    "Invoice No. 8891\nDigital Asset Management\nModel Release",
    "Model Release\nPhotographer John\nDate 2026-04-01",
    "Digital Asset Management\nSOUJPG DAM\nArchive 2026",
    "Invoice No. 9900\nManagement Fee",
    // 中英混排
    "SOUJPG DAM 数字资产 管理\nInvoice 发票号 A-888",
    "SKU-8891降价促销 Invoice No. 8891",
    "DAM 数字资产管理 发票号 A-777",
    // 单字场景（只含单字、不含完整词）
    "票 号 已 开\n存 根",
    "发 货 单\n编 号 待 定",
    "销 售 单\n降 价 标 签",
    "编 制 人 张 三\n审 核 人 李 四",
];

/// 查询集（20 条：2 字/3 字/单字/多词/跨词/英文词/英文子串/拉丁数字/混排）。
/// 第 3 项 = 该查询是否属于「口径取舍」类（英文子串：旧子串级命中、新词级不命中）。
const QUERIES: &[(&str, bool)] = &[
    ("发票", false),
    ("发票号", false),
    ("报销单", false),
    ("报销", false),
    ("增值税", false),
    ("合同编号", false),
    ("授权书", false),
    ("肖像授权", false),
    ("数字资产", false),
    ("降价", false),
    ("发", false),   // 单字
    ("票", false),   // 单字
    ("编", false),   // 单字
    ("销", false),   // 单字
    ("Model Release", false),
    ("Invoice", false),
    ("Management", false),
    ("发票 报销", false), // 多词
    ("合同 编号", false), // 跨词（文本里被空格拆开）
    ("anage", true),      // 英文子串：口径取舍样本
];

fn is_ascii_word(t: &str) -> bool {
    t.chars().all(|c| c.is_ascii_alphanumeric())
}

/// 真值：图上文本里真的出现该词。
/// 英文词（`Invoice`）→ 大小写无关整词；其它（中文/混合/英文子串）→ 子串。
fn truth_contains(text: &str, word: &str) -> bool {
    let lowered = text.to_lowercase();
    if is_ascii_word(word) {
        // 整词口径（ASCII 词边界）；但 `anage` 这类子串型查询由调用方按子串判（见 ground_truth）
        let w = word.to_lowercase();
        lowered
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|tok| tok == w)
    } else {
        lowered.contains(&word.to_lowercase())
    }
}

/// 查询的真值集：多词查询 = 所有词都出现（与融合的「全命中」口径一致）；
/// 单字/单词查询 = 该词出现。英文子串型查询按子串判（口径取舍样本）。
fn ground_truth(query: &str, substring_mode: bool) -> Vec<i64> {
    let words: Vec<String> = query
        .split_whitespace()
        .map(|w| w.to_lowercase())
        .collect();
    CORPUS
        .iter()
        .enumerate()
        .filter(|(_, text)| {
            words.iter().all(|w| {
                if substring_mode {
                    text.to_lowercase().contains(w.as_str())
                } else {
                    truth_contains(text, w)
                }
            })
        })
        .map(|(i, _)| (i + 1) as i64)
        .collect()
}

/// 旧口径（合并前实现）：空白切分 + 每个 token ≥2 字符 → 全表取回 + Rust 子串匹配。
fn old_hits(conn: &Connection, query: &str) -> Vec<i64> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .filter(|t| t.chars().count() >= 2)
        .map(|t| t.to_lowercase())
        .collect();
    if tokens.is_empty() {
        return Vec::new();
    }
    let mut stmt = conn
        .prepare("SELECT assetId, ocr_text FROM assets WHERE deleted = 0 AND ocr_text != ''")
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
        .unwrap();
    let mut out: Vec<i64> = rows
        .filter_map(|row| {
            let (id, text) = row.unwrap();
            let t = text.to_lowercase();
            tokens.iter().all(|tok| t.contains(tok.as_str())).then_some(id)
        })
        .collect();
    out.sort_unstable();
    out
}

/// 新口径：FTS5 倒排 + 生产候选上限；返回按分数（命中 term 数）降序的候选。
fn new_hits(conn: &Connection, query: &str, k: usize) -> Vec<(i64, usize)> {
    let terms = core_storage::ocr_index::query_terms(query);
    if terms.is_empty() {
        return Vec::new();
    }
    core_storage::ocr_index::match_counts(&conn, &terms, core_storage::ocr_index::term_candidate_cap(k))
        .unwrap()
}

fn recall(hits: &[i64], truth: &[i64]) -> f64 {
    if truth.is_empty() {
        return 1.0;
    }
    let hit = truth.iter().filter(|t| hits.binary_search(t).is_ok()).count();
    hit as f64 / truth.len() as f64
}

fn false_positives(hits: &[i64], truth: &[i64]) -> usize {
    hits.iter().filter(|h| truth.binary_search(h).is_err()).count()
}

fn ms(f: impl Fn() -> Vec<i64>, reps: usize) -> f64 {
    let t = Instant::now();
    for _ in 0..reps {
        std::hint::black_box(f());
    }
    t.elapsed().as_secs_f64() * 1000.0 / reps as f64
}

fn main() {
    let dir = std::env::temp_dir().join(format!("dam-ocrmatch-eval-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let conn = core_storage::open(&dir.join("dam.db")).unwrap();
    let db = dir.join("dam.db");
    // 语料走**直写 SQL**（模拟导入/外部写入）→ 触发 0013 的 dirty 标记 → open() 时自愈建索引。
    // 这里显式再 open 一次，证明「索引不靠应用写路径也能收敛」。
    {
        for (i, text) in CORPUS.iter().enumerate() {
            let id = (i + 1) as i64;
            conn.execute(
                "INSERT INTO assets (assetId, absPath, fileName, fileExt, fileSz, mtime, importedAt, mediaType, ocr_text)
                 VALUES (?1, ?2, ?3, 'png', 1000, ?4, ?4, 'image', ?5)",
                rusqlite::params![
                    id,
                    format!("/lib/{id}.png"),
                    format!("{id}.png"),
                    id,
                    text
                ],
            )
            .unwrap();
        }
    }
    drop(conn);
    let conn = core_storage::open(&db).unwrap();
    let indexed = core_storage::ocr_index::indexed_count(&conn).unwrap();
    let dirty = core_storage::ocr_index::dirty_count(&conn).unwrap();
    println!(
        "== OCR-MATCH 评测：标注语料 {} 张 / 查询 {} 条（索引行 {} / dirty {}）",
        CORPUS.len(),
        QUERIES.len(),
        indexed,
        dirty
    );
    assert_eq!(indexed as usize, CORPUS.len(), "直写 SQL 的语料也必须被索引覆盖");
    assert_eq!(dirty, 0, "open() 自愈后 dirty 必须收敛");

    // 单字/2 字查询的**索引命中证据**（卡面要求：EXPLAIN QUERY PLAN）
    println!("\n== 索引命中证据（单字 / 2 字 / 3 字中文）");
    for q in ["发", "票", "发票", "发票号"] {
        let terms = core_storage::ocr_index::query_terms(q);
        let plan = core_storage::ocr_index::explain_query_plan(&conn, &terms, 256).unwrap();
        let hits = core_storage::ocr_index::match_counts(&conn, &terms, 256).unwrap();
        println!(
            "   {:>4} → terms={:?} 命中 {} 条 | {}{}",
            q,
            terms,
            hits.len(),
            plan.join(" ; "),
            if q == "发" { "   ← 旧口径（token <2 字符直接丢弃）恒 0 条" } else { "" }
        );
        assert!(
            plan.iter().any(|l| l.contains("VIRTUAL TABLE INDEX")),
            "「{q}」必须走 FTS5 倒排索引：{plan:?}"
        );
        assert!(!hits.is_empty(), "「{q}」必须至少命中 1 条");
    }

    println!("\n== 评测表（真值 = 图上文本真的出现该查询的词）");
    println!(
        "{:<16} {:>6} | {:>6} {:>8} {:>8} | {:>6} {:>8} {:>8} {:>9} | {:>7} {:>7}",
        "查询", "真值", "旧命中", "旧召回", "旧误召", "新命中", "新召回", "新误召", "新top10精", "旧ms", "新ms"
    );
    let mut sum_old_recall = 0.0;
    let mut sum_new_recall = 0.0;
    let mut sum_old_fp = 0usize;
    let mut sum_new_fp = 0usize;
    let mut checked = 0;
    let mut tradeoff_queries = Vec::new();
    for (q, substring_mode) in QUERIES {
        let truth = ground_truth(q, *substring_mode);
        let old = old_hits(&conn, q);
        let new = new_hits(&conn, q, 24);
        let new_ids: Vec<i64> = {
            let mut v: Vec<i64> = new.iter().map(|(i, _)| *i).collect();
            v.sort_unstable();
            v
        };
        let (or_, nr) = (recall(&old, &truth), recall(&new_ids, &truth));
        let has_truth = !truth.is_empty() && !*substring_mode;
        let top10: Vec<i64> = new.iter().take(10).map(|(i, _)| *i).collect();
        let top10_prec = if top10.is_empty() || !has_truth {
            None
        } else {
            Some(top10.iter().filter(|h| truth.contains(h)).count() as f64 / top10.len() as f64)
        };
        // 断言 ①：下推不得漏召回（旧命中集 ⊆ 新命中集）——
        // 英文子串型查询（`anage`）是**有意的口径取舍**（旧子串级 / 新词级），单独断言并解释
        if *substring_mode {
            assert!(
                new_ids.is_empty(),
                "英文子串查询在新口径（词级）下应当不命中：new={new_ids:?}"
            );
        } else {
            assert!(
                old.iter().all(|h| new_ids.binary_search(h).is_ok()),
                "「{q}」旧有新无（下推漏召回）：old={old:?} new={new_ids:?}"
            );
            assert!(nr >= or_ - 1e-9, "「{q}」新召回率不得低于旧：{or_} → {nr}");
        }
        let old_ms = ms(|| old_hits(&conn, q), 5);
        let new_ms = ms(|| new_hits(&conn, q, 24).iter().map(|(i, _)| *i).collect(), 5);
        println!(
            "{:<16} {:>6} | {:>6} {:>8} {:>8} | {:>6} {:>8} {:>8} {:>9} | {:>7.2} {:>7.2}{}",
            q,
            truth.len(),
            old.len(),
            if truth.is_empty() { "—".into() } else { format!("{:.0}%", or_ * 100.0) },
            if truth.is_empty() { "—".into() } else { false_positives(&old, &truth).to_string() },
            new_ids.len(),
            if truth.is_empty() { "—".into() } else { format!("{:.0}%", nr * 100.0) },
            false_positives(&new_ids, &truth),
            match top10_prec {
                Some(p) => format!("{:.0}%", p * 100.0),
                None => "—".to_string(),
            },
            old_ms,
            new_ms,
            if *substring_mode { "   ← 口径取舍（英文子串）" } else { "" }
        );
        if *substring_mode {
            tradeoff_queries.push((*q, old.clone(), new_ids.clone(), truth.clone()));
        }
        if has_truth {
            assert!((nr - 1.0).abs() < 1e-9, "真值非空时新召回率必须 100%：「{q}」{nr}");
            checked += 1;
            sum_old_recall += or_;
            sum_new_recall += nr;
            sum_old_fp += false_positives(&old, &truth);
            sum_new_fp += false_positives(&new_ids, &truth);
        }
    }
    println!(
        "\n汇总（{} 条真值非空的查询）：旧 平均召回 {:.0}% / 误召回合计 {} ｜ 新 平均召回 {:.0}% / 误召回合计 {}",
        checked,
        sum_old_recall / checked as f64 * 100.0,
        sum_old_fp,
        sum_new_recall / checked as f64 * 100.0,
        sum_new_fp
    );

    // 中文单字：旧口径恒 0%，新口径必须 100%（卡面不可让项）
    let mut single_reported = 0;
    for q in ["发", "票", "编", "销"] {
        let truth = ground_truth(q, false);
        assert!(!truth.is_empty(), "单字「{q}」真值集不该为空（语料设计问题）");
        let old = old_hits(&conn, q);
        assert!(old.is_empty(), "单字「{q}」旧口径应恒 0 命中（token <2 被丢弃）");
        let new = new_hits(&conn, q, 24);
        assert_eq!(recall(&new.iter().map(|(i, _)| *i).collect::<Vec<_>>(), &truth), 1.0);
        single_reported += 1;
    }
    println!("中文单字查询：{single_reported} 条，旧口径 0 命中 → 新口径召回 100%（走倒排索引）");

    // 口径取舍样本如实打印（不做「新一定更好」的断言）
    for (q, old, new, truth) in tradeoff_queries {
        println!(
            "口径取舍样本「{q}」：真值 {:?} / 旧（子串级）命中 {:?} / 新（词级）命中 {:?} —— 英文按整词，子串查询不再命中（有意）",
            truth, old, new
        );
    }

    println!(
        "\nOCR_MATCH_EVAL_RESULT {{\"assets\":{},\"queries\":{},\"truth_nonempty\":{},\"old_recall_avg\":{:.3},\"new_recall_avg\":{:.3},\"old_false_positive_total\":{},\"new_false_positive_total\":{},\"single_char_old_hits\":0,\"single_char_new_recall\":1.0}}",
        CORPUS.len(),
        QUERIES.len(),
        checked,
        sum_old_recall / checked as f64,
        sum_new_recall / checked as f64,
        sum_old_fp,
        sum_new_fp
    );
    println!("（误召回合计是新口径的固有代价：2-gram 会命中「模型授权」「资产编号」这类只含部分 2-gram 的文本；\n  靠部分命中折扣 ×0.6×命中比例 把它们压到后排 —— 见 top-10 精确率列。真值非空查询新召回率恒 100%。）");
    let _ = std::fs::remove_dir_all(&dir);
}
