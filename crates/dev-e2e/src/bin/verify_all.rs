//! D7 链尾全链路复验 harness（verify_all）：零外部依赖（无模型下载、无云 API、无真图），
//! mock 打标 + mock 嵌入（2026-10-13 裁决口径），其余环节全部走真实代码路径：
//!
//!   清空库 → 建库(迁移) → 生成确定性 fixture → 真实导入(SHA-256/pHash/缩略图)
//!   → 真实打标队列(FakeTag) → 真实双塔嵌入队列(FakeEmbed → sqlite-vec 真向量表)
//!   → 三种搜索(文搜图/以图搜图/资产找相似) → 查重(fileHash 精确 + 向量 near/variant 分带)
//!   → 软删/恢复回路 → 清理
//!
//! 每步计时 + assert，任一步失败非零退出（scripts/verify_all.sh 消费）。
//! 幂等：每次从头 rm -rf 工作目录重建，中断重跑结果一致。

use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use engine_embed::EmbedProvider;
use engine_search::DupOptions;
use image::{Rgb, RgbImage};
use rusqlite::Connection;

use svc_library::{
    exact_dup_groups, find_duplicates, find_similar, list_deleted, restore_assets,
    search_by_image_path, search_by_text, soft_delete_assets, SimilarOptions,
};
use svc_pipeline::{enq_unembedded, enq_untagged, run_embed_queue, run_tag_queue, EmbedScope};

const DIMS: u32 = 64;
const SEM_MODEL: &str = "mock-siglip";
const DEDUP_MODEL: &str = "mock-dinov3";

fn main() {
    let work =
        PathBuf::from(std::env::var("VERIFY_WORK").unwrap_or_else(|_| "/tmp/verify-all".into()));
    let keep = std::env::args().any(|a| a == "--keep");
    match run(&work, keep) {
        Ok(fails) if fails.is_empty() => {
            println!("\nVERIFY_ALL: ALL PASS");
            std::process::exit(0);
        }
        Ok(fails) => {
            println!("\nVERIFY_ALL: FAILED — {} 处不过：{fails:?}", fails.len());
            std::process::exit(1);
        }
        Err(e) => {
            println!("\nVERIFY_ALL: ERROR — {e}");
            std::process::exit(2);
        }
    }
}

fn ok(fails: &mut Vec<String>, cond: bool, what: &str) -> bool {
    if cond {
        println!("  PASS {what}");
    } else {
        println!("  FAIL {what}");
        fails.push(what.to_string());
    }
    cond
}

fn step(title: &str, t: &mut Instant) -> u128 {
    let ms = t.elapsed().as_millis();
    println!("\n==== {title}（上一步 {ms} ms）====");
    *t = Instant::now();
    ms
}

// ---------- mock providers ----------

struct FakeTag {
    spec: core_config::ModelSpec,
}

impl engine_tag::TagProvider for FakeTag {
    fn load(&mut self) -> engine_tag::Result<()> {
        Ok(())
    }
    fn tag_image(&self, path: &std::path::Path) -> engine_tag::Result<engine_tag::TaggingOutput> {
        let stem = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        Ok(engine_tag::TaggingOutput {
            title_ch: format!("自动标签·{stem}"),
            title_en: stem.clone(),
            description: "mock tagger output".into(),
            tags: vec![
                engine_tag::TagItem::new("mock", 0.95),
                engine_tag::TagItem::new(stem, 0.85),
            ],
            repaired: false,
        })
    }
    fn test_connection(&self) -> engine_tag::Result<String> {
        Ok("mock OK".into())
    }
    fn ready(&self) -> bool {
        true
    }
    fn spec(&self) -> &core_config::ModelSpec {
        &self.spec
    }
}

/// 文件名 → 确定性向量（mock 嵌入）：
///   g<NN>      → unit(30+NN)            孤立项
///   n<NN>[ab]  → unit(NN)               近重复对（两员同向量，cos=1.0）
///   v<NN>a     → unit(10+NN)            变体对锚
///   v<NN>b     → perturbed(10+NN, 0.33) 变体对成员（cos≈0.95 ∈ [0.90,0.98)）
/// 文本查询（如 "v03"）走同一解析 → 文搜图 top1 可精确断言。
fn stem_vector(stem: &str) -> Vec<f32> {
    let bytes = stem.as_bytes();
    let prefix = bytes.first().copied().unwrap_or(b'x');
    let digits: String = stem
        .chars()
        .skip(1)
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let n: usize = digits.parse().unwrap_or(63);
    let suffix = stem.chars().next_back().unwrap_or('x');

    match (prefix, suffix) {
        (b'g', _) => unit(30 + (n % 30)),
        (b'n', _) => unit(n % DIMS as usize),
        (b'v', 'a') => unit(10 + (n % 20)),
        (b'v', _) => perturbed(10 + (n % 20), 45 + (n % 10), 0.33),
        _ => unit(63),
    }
}

struct FakeEmbed {
    spec: core_config::ModelSpec,
}

impl FakeEmbed {
    fn new(name: &str, text_capable: bool) -> Self {
        let mut modalities = vec!["image".to_string()];
        if text_capable {
            modalities.push("text".into());
        }
        Self {
            spec: core_config::ModelSpec {
                name: name.into(),
                kind: core_config::SpecKind::Embedding,
                dims: DIMS,
                modalities,
                preprocess: Default::default(),
                normalized: true,
                provider: "mock".into(),
            },
        }
    }
}

impl EmbedProvider for FakeEmbed {
    fn load(&mut self) -> engine_embed::Result<()> {
        Ok(())
    }
    fn embed_images(&self, paths: &[PathBuf]) -> engine_embed::Result<Vec<Vec<f32>>> {
        Ok(paths
            .iter()
            .map(|p| {
                let stem = p
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                stem_vector(&stem)
            })
            .collect())
    }
    fn embed_texts(&self, texts: &[&str]) -> engine_embed::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| stem_vector(t.trim())).collect())
    }
    fn ready(&self) -> bool {
        true
    }
    fn spec(&self) -> &core_config::ModelSpec {
        &self.spec
    }
}

fn unit(at: usize) -> Vec<f32> {
    let mut v = vec![0.0; DIMS as usize];
    v[at % DIMS as usize] = 1.0;
    v
}

/// normalize(e_base + eps·e_noise)：与 base 的 cos = 1/sqrt(1+eps²)。
fn perturbed(base: usize, noise: usize, eps: f32) -> Vec<f32> {
    let mut v = unit(base);
    v[noise % DIMS as usize] = eps;
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter_mut().for_each(|x| *x /= norm);
    v
}

// ---------- fixture ----------

/// 36 张确定性 PNG：12 孤立 g00-11 + 6 近重复对 n00a/b-n05a/b + 6 变体对 v00a/b-v05a/b。
/// 对内成员同基色不同角落像素 → SHA-256 不同（导入不判 Duplicate），向量由文件名控制。
fn gen_fixtures(dir: &Path) -> std::io::Result<Vec<String>> {
    std::fs::create_dir_all(dir)?;
    let mut names = Vec::new();
    let save = |name: String, color: Rgb<u8>, corner: u8, names: &mut Vec<String>| {
        let mut img = RgbImage::from_pixel(64, 64, color);
        img.put_pixel(0, 0, Rgb([corner, corner, corner]));
        img.save(dir.join(format!("{name}.png"))).unwrap();
        names.push(format!("{name}.png"));
    };
    for i in 0..12u8 {
        save(
            format!("g{i:02}"),
            Rgb([20 + i * 15, 90, 160 - i * 8]),
            i,
            &mut names,
        );
    }
    for i in 0..6u8 {
        let c = Rgb([40 + i * 30, 120 + i * 10, 60]);
        save(format!("n{i:02}a"), c, 10, &mut names);
        save(format!("n{i:02}b"), c, 200, &mut names);
        let c2 = Rgb([200 - i * 20, 60 + i * 25, 130 + i * 12]);
        save(format!("v{i:02}a"), c2, 30, &mut names);
        save(format!("v{i:02}b"), c2, 220, &mut names);
    }
    Ok(names)
}

fn asset_id_by_name(conn: &Connection, file_name: &str) -> Option<i64> {
    conn.query_row(
        "SELECT assetId FROM assets WHERE fileName = ?1 AND deleted = 0",
        [file_name],
        |r| r.get(0),
    )
    .ok()
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap_or(-1)
}

fn run(work: &Path, keep: bool) -> Result<Vec<String>, Box<dyn Error>> {
    let mut fails: Vec<String> = Vec::new();
    let mut t = Instant::now();

    // ---------- S1 清空库 ----------
    step(
        "S1 清空工作目录（库/thumbs/fixture 全清，幂等起点）",
        &mut t,
    );
    if work.exists() {
        std::fs::remove_dir_all(work)?;
    }
    std::fs::create_dir_all(work)?;
    ok(
        &mut fails,
        !work.join("dam.db").exists(),
        "旧库已清（dam.db 不存在）",
    );

    // ---------- S2 建库 ----------
    step("S2 建库（core_storage::open → 迁移全量执行）", &mut t);
    let db = work.join("dam.db");
    let conn = core_storage::open(&db)?;
    let ver = core_storage::migrations::current_version(&conn)?;
    println!("  schema_version={ver}");
    ok(
        &mut fails,
        ver >= 5,
        "迁移版本 ≥5（0005 dedup_vectors 已应用）",
    );
    let tables: i64 = count(&conn, "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('assets','resource','tag','job_queue','settings','provider_registry','vector_tables')");
    ok(
        &mut fails,
        tables == 7,
        format!("核心 7 表齐备（实际 {tables}）").as_str(),
    );

    // ---------- S3 导入 fixture ----------
    step("S3 生成 fixture 并真实导入（SHA-256/pHash/缩略图）", &mut t);
    let fix_dir = work.join("fixtures");
    let names = gen_fixtures(&fix_dir)?;
    let n = names.len();
    println!("  fixture：{n} 张确定性 PNG");
    let thumbs = work.join("thumbs");
    std::fs::create_dir_all(&thumbs)?;
    let batch = svc_import::create_batch(&conn, &fix_dir, "reference", n as i64)?;
    let outcome = svc_import::run_batch(&conn, batch, None, &thumbs, |_| Ok(()))?;
    println!(
        "  imported={} duplicates={} unchanged={} failed={} status={}",
        outcome.imported, outcome.duplicates, outcome.unchanged, outcome.failed, outcome.status
    );
    ok(
        &mut fails,
        outcome.imported == n as i64,
        "导入数 == fixture 数",
    );
    ok(&mut fails, outcome.failed == 0, "导入失败 0");
    let hashed = count(
        &conn,
        "SELECT COUNT(*) FROM assets WHERE fileHash IS NOT NULL AND deleted=0",
    );
    let phashed = count(
        &conn,
        "SELECT COUNT(*) FROM resource WHERE pHash IS NOT NULL",
    );
    ok(
        &mut fails,
        hashed == n as i64,
        format!("SHA-256 全量落库（{hashed}/{n}）").as_str(),
    );
    ok(
        &mut fails,
        phashed == n as i64,
        format!("pHash 全量落库（{phashed}/{n}）").as_str(),
    );

    // ---------- S4 mock 打标 ----------
    step(
        "S4 mock 打标（真实队列 job_queue → FakeTag → tag 表）",
        &mut t,
    );
    let tagger = FakeTag {
        spec: core_config::ModelSpec {
            name: "mock-tagger".into(),
            kind: core_config::SpecKind::Tagging,
            dims: 0,
            modalities: vec!["image".into()],
            preprocess: Default::default(),
            normalized: false,
            provider: "mock".into(),
        },
    };
    let enq = enq_untagged(&conn)?;
    let cancel = AtomicBool::new(false);
    let report = run_tag_queue(&conn, &tagger, &cancel, &mut |_| {})?;
    println!("  enq={enq} done={} failed={}", report.done, report.failed);
    let tagged_assets = count(&conn, "SELECT COUNT(DISTINCT assetId) FROM tag");
    ok(
        &mut fails,
        enq == n && tagged_assets == n as i64,
        format!("打标覆盖全部资产（{tagged_assets}/{n}）").as_str(),
    );
    ok(&mut fails, report.failed == 0, "打标失败 0");
    let cloud = svc_library::tag_cloud(&conn, 5)?;
    println!("  tag_cloud top5 = {cloud:?}");
    ok(&mut fails, !cloud.is_empty(), "标签云非空");
    let titled = count(
        &conn,
        "SELECT COUNT(*) FROM assets WHERE title_ch IS NOT NULL AND deleted=0",
    );
    ok(
        &mut fails,
        titled == n as i64,
        "title 全量写回（{titled}/{n}）",
    );

    // ---------- S5 mock 嵌入（双塔） ----------
    step("S5 mock 嵌入（真实双塔队列 → sqlite-vec 真向量表）", &mut t);
    let sem = FakeEmbed::new(SEM_MODEL, true);
    let dedup = FakeEmbed::new(DEDUP_MODEL, false);
    let mut timings = BTreeMap::new();
    for (label, provider, scope, col) in [
        ("semantic", &sem, EmbedScope::Semantic, "emb_status"),
        ("dedup", &dedup, EmbedScope::Dedup, "dedup_status"),
    ] {
        let t0 = Instant::now();
        let enq = enq_unembedded(&conn, provider.spec().name.clone().as_str(), scope)?;
        let cancel = AtomicBool::new(false);
        let rep = run_embed_queue(&conn, provider, scope, &cancel, &mut |_| {})?;
        timings.insert(label.to_string(), t0.elapsed().as_millis());
        println!(
            "  {label}（{}）：enq={enq} done={} failed={} {}ms",
            provider.spec().name,
            rep.done,
            rep.failed,
            t0.elapsed().as_millis()
        );
        ok(
            &mut fails,
            rep.done == n && rep.failed == 0,
            format!("{label} 塔嵌入 done={n} failed=0").as_str(),
        );
        let done = count(
            &conn,
            &format!("SELECT COUNT(*) FROM assets WHERE {col}='done' AND deleted=0"),
        );
        ok(
            &mut fails,
            done == n as i64,
            format!("{col} done={n}（实际 {done}）").as_str(),
        );
    }
    let sem_cnt = core_storage::vecstore::count(&conn, SEM_MODEL)?;
    let dedup_cnt = core_storage::vecstore::count(&conn, DEDUP_MODEL)?;
    println!("  向量表：v_{SEM_MODEL}={sem_cnt} v_{DEDUP_MODEL}={dedup_cnt}");
    ok(
        &mut fails,
        sem_cnt == n as i64 && dedup_cnt == n as i64,
        "双塔向量表各 {n} 条（禁混算：分表独立）",
    );

    // ---------- S6 三种搜索 ----------
    step(
        "S6 三种搜索（文搜图 / 以图搜图 / 资产找相似；sqlite-vec 真召回）",
        &mut t,
    );
    // 1) 文搜图：query "v03" 的 mock 向量 == v03a/v03b 锚向量 → top1 属于 v03 对
    let hits = search_by_text(
        &conn,
        &sem,
        "v03",
        5,
        &Default::default(),
        &svc_library::TextSearchOptions::default(),
    )?;
    println!(
        "  文搜图 \"v03\" top3 = {:?}",
        hits.iter()
            .map(|h| (h.fileName.clone(), h.score))
            .collect::<Vec<_>>()
    );
    ok(
        &mut fails,
        hits.first()
            .map(|h| h.fileName.starts_with("v03"))
            .unwrap_or(false),
        "文搜图 top1 命中 v03 对",
    );
    // 2) 以图搜图：查询图自身在 top2 内且分 =1.0（孪生对同分并列，名次任意属正确行为）
    let hits = search_by_image_path(
        &conn,
        &sem,
        &fix_dir.join("n01b.png"),
        5,
        &Default::default(),
        &SimilarOptions {
            min_score: Some(0.0),
        },
    )?;
    println!(
        "  以图搜图 n01b.png top3 = {:?}",
        hits.iter()
            .map(|h| (h.fileName.clone(), h.score))
            .collect::<Vec<_>>()
    );
    let self_rank = hits.iter().position(|h| h.fileName == "n01b.png");
    ok(
        &mut fails,
        self_rank
            .map(|r| r < 2 && hits[r].score >= 0.999)
            .unwrap_or(false),
        "以图搜图：查询图自身 top2 内且 cos=1.0",
    );
    // 3) 资产找相似：n01a 的相似应含同对成员 n01b
    let aid = asset_id_by_name(&conn, "n01a.png").ok_or("n01a.png 不在库")?;
    let hits = find_similar(
        &conn,
        &sem,
        aid,
        5,
        &Default::default(),
        &SimilarOptions {
            min_score: Some(0.0),
        },
    )?;
    println!(
        "  找相似 n01a top5 = {:?}",
        hits.iter()
            .map(|h| (h.fileName.clone(), h.score))
            .collect::<Vec<_>>()
    );
    ok(
        &mut fails,
        hits.iter().any(|h| h.fileName == "n01b.png"),
        "找相似含同对成员 n01b",
    );

    // ---------- S7 查重 ----------
    step(
        "S7 查重（fileHash 精确 + 向量 near/variant 分带，真实流程造精确组）",
        &mut t,
    );
    // 7.1 精确组真实用户流：软删 g00 → 同字节重导入（此时放行）→ 恢复 g00 → 两活跃同 fileHash
    let g00 = asset_id_by_name(&conn, "g00.png").ok_or("g00.png 不在库")?;
    let nd = soft_delete_assets(&conn, &[g00])?;
    ok(&mut fails, nd == 1, "软删 g00 置位 1 行");
    let copy_dir = work.join("copies");
    std::fs::create_dir_all(&copy_dir)?;
    std::fs::copy(fix_dir.join("g00.png"), copy_dir.join("copy00.png"))?;
    let batch2 = svc_import::create_batch(&conn, &copy_dir, "reference", 1)?;
    let out2 = svc_import::run_batch(&conn, batch2, None, &thumbs, |_| Ok(()))?;
    println!(
        "  重导入同字节副本：imported={} duplicates={}",
        out2.imported, out2.duplicates
    );
    ok(
        &mut fails,
        out2.imported == 1,
        "软删后同字节可重导入（fileHash 防线只认活跃行）",
    );
    let nr = restore_assets(&conn, &[g00])?;
    ok(&mut fails, nr == 1, "恢复 g00 置位 1 行");
    let exact = exact_dup_groups(&conn)?;
    println!("  exact_dup_groups = {exact:?}");
    ok(
        &mut fails,
        exact.len() == 1,
        "精确组 1（g00 + 副本同 fileHash）",
    );
    ok(
        &mut fails,
        exact.first().map(|g| g.members.len() == 2).unwrap_or(false),
        "精确组 2 成员",
    );

    // 7.2 向量分带：6 near + 6 variant，100% 召回
    let dups = find_duplicates(&conn, &dedup, &DupOptions::default())?;
    let near: Vec<_> = dups.iter().filter(|d| d.band == "near").collect();
    let variant: Vec<_> = dups.iter().filter(|d| d.band == "variant").collect();
    println!(
        "  find_duplicates：near={} variant={}",
        near.len(),
        variant.len()
    );
    for d in &dups {
        let mut names = vec![asset_name(&conn, d.anchorAssetId)];
        names.extend(d.members.iter().map(|m| asset_name(&conn, m.assetId)));
        println!("    [{}] {:?}", d.band, names);
    }
    ok(
        &mut fails,
        near.len() == 6,
        format!("near 组 6/6（实际 {}）", near.len()).as_str(),
    );
    ok(
        &mut fails,
        variant.len() == 6,
        format!("variant 组 6/6（实际 {}）", variant.len()).as_str(),
    );
    for i in 0..6 {
        let want = |a: &str, b: &str, band: &str| -> bool {
            dups.iter().any(|d| {
                let mut ns = vec![asset_name(&conn, d.anchorAssetId)];
                ns.extend(d.members.iter().map(|m| asset_name(&conn, m.assetId)));
                d.band == band && ns.contains(&a.to_string()) && ns.contains(&b.to_string())
            })
        };
        ok(
            &mut fails,
            want(&format!("n{i:02}a.png"), &format!("n{i:02}b.png"), "near"),
            format!("near 对 n{i:02} 完整召回").as_str(),
        );
        ok(
            &mut fails,
            want(
                &format!("v{i:02}a.png"),
                &format!("v{i:02}b.png"),
                "variant",
            ),
            format!("variant 对 v{i:02} 完整召回").as_str(),
        );
    }
    // 孤立项不入组
    let all_grouped: Vec<String> = dups
        .iter()
        .flat_map(|d| {
            std::iter::once(asset_name(&conn, d.anchorAssetId))
                .chain(d.members.iter().map(|m| asset_name(&conn, m.assetId)))
        })
        .collect();
    let singles_in = all_grouped
        .iter()
        .any(|f| f.starts_with('g') && f != "copy00.png");
    ok(&mut fails, !singles_in, "12 个孤立项 0 误报");

    // ---------- S8 软删/恢复回路 ----------
    step("S8 软删/恢复回路（清理只软删、可恢复）", &mut t);
    let n00b = asset_id_by_name(&conn, "n00b.png").ok_or("n00b.png 不在库")?;
    let nd = soft_delete_assets(&conn, &[n00b])?;
    let again = soft_delete_assets(&conn, &[n00b])?;
    ok(
        &mut fails,
        nd == 1 && again == 0,
        "软删幂等（首次 1、重复 0）",
    );
    let dups2 = find_duplicates(&conn, &dedup, &DupOptions::default())?;
    let near2 = dups2.iter().filter(|d| d.band == "near").count();
    ok(&mut fails, near2 == 5, "软删 n00b 后 near 组 5（组消失）");
    let trash = list_deleted(&conn, 200)?;
    println!(
        "  回收站 = {:?}",
        trash
            .iter()
            .map(|a| a.asset.fileName.clone())
            .collect::<Vec<_>>()
    );
    ok(
        &mut fails,
        trash.iter().any(|a| a.asset.assetId == n00b),
        "回收站可见 n00b",
    );
    let hist = search_by_text(
        &conn,
        &sem,
        "n00",
        10,
        &Default::default(),
        &svc_library::TextSearchOptions::default(),
    )?;
    ok(
        &mut fails,
        !hist.iter().any(|h| h.fileName == "n00b.png"),
        "软删资产从搜索消失",
    );
    let nr = restore_assets(&conn, &[n00b])?;
    let dups3 = find_duplicates(&conn, &dedup, &DupOptions::default())?;
    ok(
        &mut fails,
        nr == 1 && dups3.iter().filter(|d| d.band == "near").count() == 6,
        "恢复后 near 组回到 6（100% 召回可重现）",
    );

    // ---------- S9 清理 ----------
    step("S9 清理工作目录", &mut t);
    if keep {
        println!("  --keep：保留 {work:?}（复现/排查用）");
    } else {
        std::fs::remove_dir_all(work)?;
        ok(&mut fails, !work.exists(), "工作目录已删除");
    }

    println!("\n各阶段耗时：{timings:?} ms（嵌入队列）");
    Ok(fails)
}

fn asset_name(conn: &Connection, id: i64) -> String {
    conn.query_row(
        "SELECT fileName FROM assets WHERE assetId = ?1",
        [id],
        |r| r.get(0),
    )
    .unwrap_or_else(|_| format!("#{id}"))
}
