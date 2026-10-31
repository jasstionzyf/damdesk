//! D6 — 真图端到端 harness（L3 dev-e2e，仅开发/验收用，不进安装包）。
//!
//! 链路：真照片扫描 → 导入（SHA-256 + pHash + 双档缩略图）→ 语义塔(SigLIP2 ONNX)
//! 嵌入 + 去重塔(DINOv3 ONNX)嵌入 → 文搜图 / 以图搜图 / 查重 → 云 VL 打标（可选）
//! → 全表统计 + JSON 汇总。每步计时，关键结论硬断言（不通过则退出码 1）。
//!
//! 用法（容器内）：
//! ```sh
//! cargo run --release -p dev-e2e --bin d6_e2e -- \
//!   --photos /workspace/tests/fixtures/reals --work /tmp/d6
//! ```
//! 云 VL 打标：`D6_TAG_BASE_URL` + `D6_TAG_API_KEY` + `D6_TAG_MODEL`（缺 key 则跳过并在
//! 汇总里标 tag.skipped=no-credentials，不静默、不伪造）。`--no-tag` 可强制关闭。
//!
//! 环境变量：D6_PHOTOS / D6_WORK / D6_MODELS(=appData 根，权重放 <root>/models/<spec.name>；
//! 容器里 /models 挂的是 /data，故默认 `/` → /models/<spec.name>) / D6_TOP_K(=10)
//! D6_TAG_N(=50) / D6_TAG_BASE_URL / D6_TAG_API_KEY / D6_TAG_MODEL / D6_TAG_TIMEOUT_MS
//! D6_QUERIES（逗号分隔中文 query） / D6_JSON

use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use core_config::{load_registry, InferenceSettings, SpecKind, TagSettings};
use engine_embed::factory;
use engine_embed::EmbedProvider;
use engine_search::{DupOptions, SearchFilter};
use engine_tag::openai_compat::OpenAiCompatTagProvider;
use engine_tag::TagProvider;
use rusqlite::Connection;
use serde_json::json;
use svc_pipeline::{enq_unembedded, enq_untagged, run_embed_queue, run_tag_queue, EmbedScope};

const DEFAULT_QUERIES: &[&str] = &[
    "狗",
    "一只猫",
    "汽车",
    "食物 美食",
    "自然风景 山水",
    "人物肖像",
];

struct Args {
    photos: PathBuf,
    work: PathBuf,
    models: PathBuf,
    top_k: usize,
    tag_n: usize,
    tag_enabled: bool,
    only_dedup: bool,
    queries: Vec<String>,
    json_out: PathBuf,
    tag_base_url: String,
    tag_api_key: String,
    tag_model: String,
    tag_provider: String,
    tag_timeout_ms: u64,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut map: BTreeMap<String, String> = BTreeMap::new();
        let mut no_tag = false;
        let mut only_dedup = false;
        let mut it = std::env::args().skip(1);
        while let Some(a) = it.next() {
            match a.as_str() {
                "--no-tag" => no_tag = true,
                "--only-dedup" => only_dedup = true,
                _ if a.starts_with("--") => {
                    let key = a.trim_start_matches("--").to_string();
                    let val = it.next().ok_or_else(|| format!("{key} 缺值"))?;
                    map.insert(key, val);
                }
                other => return Err(format!("未知参数 {other}")),
            }
        }
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let get = |k: &str, e: &str, d: &str| {
            map.get(k)
                .cloned()
                .or_else(|| env(e))
                .unwrap_or_else(|| d.to_string())
        };

        let work = PathBuf::from(get("work", "D6_WORK", "/tmp/d6"));
        let queries = get("queries", "D6_QUERIES", "")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        Ok(Self {
            photos: PathBuf::from(get("photos", "D6_PHOTOS", "")),
            json_out: PathBuf::from(get(
                "json",
                "D6_JSON",
                &work.join("d6_summary.json").to_string_lossy(),
            )),
            models: PathBuf::from(get("models", "D6_MODELS", "/")),
            top_k: get("top-k", "D6_TOP_K", "10")
                .parse()
                .map_err(|e| format!("top-k: {e}"))?,
            tag_n: get("tag-n", "D6_TAG_N", "50")
                .parse()
                .map_err(|e| format!("tag-n: {e}"))?,
            tag_enabled: !no_tag,
            only_dedup,
            queries: if queries.is_empty() {
                DEFAULT_QUERIES.iter().map(|s| s.to_string()).collect()
            } else {
                queries
            },
            tag_base_url: get("tag-base-url", "D6_TAG_BASE_URL", ""),
            tag_api_key: get("tag-api-key", "D6_TAG_API_KEY", ""),
            tag_model: get("tag-model", "D6_TAG_MODEL", ""),
            // D9：provider 预设 id（dashscope/zhipu/ollama/custom），报告记录用
            tag_provider: get("tag-provider", "D9_TAG_PROVIDER", "custom"),
            tag_timeout_ms: get("tag-timeout-ms", "D6_TAG_TIMEOUT_MS", "60000")
                .parse()
                .map_err(|e| format!("tag-timeout-ms: {e}"))?,
            work,
        })
    }
}

/// 断言收集器：不 panic，全部跑完后统一裁决（一次跑出完整问题清单）。
struct Checks {
    fails: Vec<String>,
}

impl Checks {
    fn new() -> Self {
        Self { fails: Vec::new() }
    }
    fn ok(&mut self, cond: bool, what: &str) {
        if cond {
            println!("  [PASS] {what}");
        } else {
            println!("  [FAIL] {what}");
            self.fails.push(what.to_string());
        }
    }
}

fn main() {
    match run() {
        Ok(fails) => {
            if fails.is_empty() {
                println!("\nD6_E2E: ALL PASS");
            } else {
                println!("\nD6_E2E: FAILED ({} 项硬断言未通过)", fails.len());
                for f in &fails {
                    println!("  - {f}");
                }
                std::process::exit(1);
            }
        }
        Err(e) => {
            println!("\nD6_E2E: ERROR {e}");
            std::process::exit(2);
        }
    }
}

fn run() -> Result<Vec<String>, Box<dyn Error>> {
    let a = Args::parse().map_err(|e| -> Box<dyn Error> { e.into() })?;
    let mut c = Checks::new();

    // --only-dedup：复用已有 work 目录的库（向量已在），只重跑查重并打印成员明细。
    if a.only_dedup {
        let db = a.work.join("dam.db");
        if !db.is_file() {
            return Err(format!("{} 不存在：--only-dedup 需要先跑完整 e2e", db.display()).into());
        }
        let conn = core_storage::open(&db)?;
        let registry = load_registry(None)?;
        let settings = InferenceSettings::default();
        let mut dedup = factory::build_dedup_embedder(&registry, &settings, &a.models)?;
        dedup.load()?;
        println!("== 6'. 查重明细（复用 {}）", db.display());
        dedup_step(&conn, dedup.as_ref(), &mut c, true)?;
        return Ok(c.fails);
    }
    if !a.photos.is_dir() {
        return Err(format!(
            "真图目录不存在：{}（先跑 scripts/sample_real_photos.sh）",
            a.photos.display()
        )
        .into());
    }
    std::fs::create_dir_all(&a.work)?;
    let thumbs = a.work.join("thumbs");
    std::fs::create_dir_all(&thumbs)?;

    let mut c = Checks::new();
    let mut summary = serde_json::Map::new();
    summary.insert("started_at".into(), json!(now_iso()));
    summary.insert("photos_dir".into(), json!(a.photos.display().to_string()));
    summary.insert("work_dir".into(), json!(a.work.display().to_string()));
    summary.insert("models_dir".into(), json!(a.models.display().to_string()));
    summary.insert(
        "cpu_threads".into(),
        json!(std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)),
    );

    // ---------- 0. 真图集 ----------
    let files = svc_import::scan_dir(&a.photos)?;
    let n = files.len();
    let total_bytes: u64 = files
        .iter()
        .filter_map(|f| std::fs::metadata(f).ok().map(|m| m.len()))
        .sum();
    let exts: BTreeMap<String, usize> = files.iter().fold(BTreeMap::new(), |mut acc, f| {
        let e = f
            .extension()
            .map(|x| x.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        *acc.entry(e).or_insert(0) += 1;
        acc
    });
    println!("== 0. 真图集 ==");
    println!(
        "  photos={} 张 / {:.1} MB / 扩展名 {:?}",
        n,
        total_bytes as f64 / 1e6,
        exts
    );
    println!(
        "  models_root={} → 语义塔权重 {}",
        a.models.display(),
        core_config::model_dir(&a.models, core_config::DEFAULT_EMBEDDER).display()
    );
    if !core_config::model_dir(&a.models, core_config::DEFAULT_EMBEDDER).is_dir() {
        return Err(format!(
            "模型权重目录不存在：{}（D6_MODELS 语义 = appData 根，权重在 <root>/models/<name>）",
            core_config::model_dir(&a.models, core_config::DEFAULT_EMBEDDER).display()
        )
        .into());
    }
    summary.insert(
        "photos".into(),
        json!({"count": n, "bytes": total_bytes, "exts": exts}),
    );
    c.ok(n >= 200, &format!("真图数量 ≥200（实际 {n}）"));

    // ---------- 1. 导入（SHA-256 + pHash + 缩略图）----------
    println!("\n== 1. 导入（含 hash/pHash/缩略图）==");
    let db = a.work.join("dam.db");
    let conn = core_storage::open(&db)?;
    let t = Instant::now();
    let batch = svc_import::create_batch(&conn, &a.photos, "reference", n as i64)?;
    let outcome = svc_import::run_batch(&conn, batch, None, &thumbs, |p| {
        if p.done % 50 == 0 {
            println!(
                "  import {}/{} imported={} dup={} failed={}",
                p.done, p.total, p.imported, p.duplicates, p.failed
            );
        }
        Ok(())
    })?;
    let import_ms = t.elapsed().as_millis() as u64;
    println!(
        "  imported={} duplicates={} unchanged={} failed={} status={} 用时 {} ms ({:.1} img/s)",
        outcome.imported,
        outcome.duplicates,
        outcome.unchanged,
        outcome.failed,
        outcome.status,
        import_ms,
        n as f64 / (import_ms as f64 / 1000.0).max(1e-6)
    );
    let hashed: i64 = conn.query_row(
        "SELECT COUNT(*) FROM assets WHERE fileHash IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    let phashed: i64 = conn.query_row(
        "SELECT COUNT(*) FROM resource WHERE pHash IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    let thumbed: i64 = conn.query_row(
        "SELECT COUNT(*) FROM resource WHERE thumbPath IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    println!("  fileHash={hashed} pHash={phashed} thumb={thumbed}");
    c.ok(
        outcome.imported == n as i64,
        &format!("导入数 == 真图数（{}=?{n}）", outcome.imported),
    );
    c.ok(
        outcome.failed == 0,
        &format!("导入失败 0（实际 {}）", outcome.failed),
    );
    c.ok(
        hashed == n as i64,
        &format!("SHA-256 全量落库（{hashed}/{n}）"),
    );
    c.ok(
        phashed == n as i64,
        &format!("pHash 全量落库（{phashed}/{n}）"),
    );
    summary.insert(
        "import".into(),
        json!({"ms": import_ms, "imported": outcome.imported, "duplicates": outcome.duplicates,
               "unchanged": outcome.unchanged, "failed": outcome.failed, "status": outcome.status,
               "fileHash": hashed, "pHash": phashed, "thumbs": thumbed,
               "img_per_s": n as f64 / (import_ms as f64 / 1000.0).max(1e-6)}),
    );

    // 纯算法基准：SHA-256 / pHash 单独计时（隔离 IO 与解码）
    let t = Instant::now();
    for f in &files {
        let d = std::fs::read(f)?;
        std::hint::black_box(core_hash::sha256_hex(&d));
    }
    let sha_ms = t.elapsed().as_millis().max(1) as u64;
    let t = Instant::now();
    let mut luma_fail = 0usize;
    for f in &files {
        match core_media::luma32(f) {
            Ok(l) => {
                if core_hash::phash_hex(&l).is_err() {
                    luma_fail += 1;
                }
            }
            Err(_) => luma_fail += 1,
        }
    }
    let phash_ms = t.elapsed().as_millis().max(1) as u64;
    println!(
        "  算法分离计时：SHA-256 {sha_ms} ms ({:.1} MB/s)，pHash+解码 {phash_ms} ms ({:.1} img/s, 失败 {luma_fail})",
        total_bytes as f64 / 1e6 / (sha_ms as f64 / 1000.0),
        n as f64 / (phash_ms as f64 / 1000.0)
    );
    summary.insert(
        "hash_bench".into(),
        json!({"sha256_ms": sha_ms, "pHash_ms": phash_ms, "luma_fail": luma_fail}),
    );

    // ---------- 2. 语义塔嵌入（SigLIP2 ONNX，CPU）----------
    println!("\n== 2. 语义塔嵌入（SigLIP2 ONNX / CPU）==");
    let registry = load_registry(None)?;
    let settings = InferenceSettings::default();
    let mut sem = factory::build_embedder(&registry, &settings, &a.models)?;
    let t_load = Instant::now();
    sem.load()?;
    let sem_load_ms = t_load.elapsed().as_millis() as u64;
    let sem_model = sem.spec().name.clone();
    let sem_dims = sem.spec().dims;
    let queued = enq_unembedded(&conn, &sem_model, EmbedScope::Semantic)?;
    let cancel = AtomicBool::new(false);
    let t = Instant::now();
    let rep = run_embed_queue(
        &conn,
        sem.as_ref(),
        EmbedScope::Semantic,
        &cancel,
        &mut |p| {
            if p.doneCount % 50 == 0 && p.doneCount > 0 {
                println!("  embed(semantic) {}/{} done", p.doneCount, p.total);
            }
        },
    )?;
    let sem_ms = t.elapsed().as_millis().max(1) as u64;
    let sem_vecs = core_storage::vecstore::count(&conn, &sem_model)?;
    let sem_ips = n as f64 / (sem_ms as f64 / 1000.0);
    println!(
        "  model={sem_model} dims={sem_dims} load={sem_load_ms}ms queued={queued} done={} failed={} vectors={sem_vecs} 用时 {sem_ms} ms ({sem_ips:.2} img/s)",
        rep.done, rep.failed
    );
    if !rep.errors.is_empty() {
        println!(
            "  errors: {:?}",
            rep.errors.iter().take(5).collect::<Vec<_>>()
        );
    }
    c.ok(
        rep.done == n,
        &format!("语义嵌入 done == 真图数（{}={n}）", rep.done),
    );
    c.ok(
        rep.failed == 0,
        &format!("语义嵌入失败 0（实际 {}）", rep.failed),
    );
    c.ok(
        sem_vecs == n as i64,
        &format!("语义向量落库 {sem_vecs}/{n}"),
    );
    c.ok(
        sem_ips > 1.0,
        &format!("语义吞吐 > 1 img/s（实测 {sem_ips:.2}）"),
    );
    summary.insert(
        "embed_semantic".into(),
        json!({"model": sem_model, "dims": sem_dims, "load_ms": sem_load_ms, "ms": sem_ms,
               "done": rep.done, "failed": rep.failed, "vectors": sem_vecs, "img_per_s": sem_ips,
               "per_img_ms": sem_ms as f64 / n as f64}),
    );

    // ---------- 3. 去重塔嵌入（DINOv3 ONNX）----------
    println!("\n== 3. 去重塔嵌入（DINOv3 ONNX / CPU）==");
    let mut dedup = factory::build_dedup_embedder(&registry, &settings, &a.models)?;
    let t_load = Instant::now();
    dedup.load()?;
    let dedup_load_ms = t_load.elapsed().as_millis() as u64;
    let dedup_model = dedup.spec().name.clone();
    let dedup_dims = dedup.spec().dims;
    enq_unembedded(&conn, &dedup_model, EmbedScope::Dedup)?;
    let t = Instant::now();
    let drep = run_embed_queue(
        &conn,
        dedup.as_ref(),
        EmbedScope::Dedup,
        &cancel,
        &mut |_p| {},
    )?;
    let dedup_ms = t.elapsed().as_millis().max(1) as u64;
    let dedup_vecs = core_storage::vecstore::count(&conn, &dedup_model)?;
    let dedup_ips = n as f64 / (dedup_ms as f64 / 1000.0);
    println!(
        "  model={dedup_model} dims={dedup_dims} load={dedup_load_ms}ms done={} failed={} vectors={dedup_vecs} 用时 {dedup_ms} ms ({dedup_ips:.2} img/s)",
        drep.done, drep.failed
    );
    c.ok(
        drep.done == n,
        &format!("去重嵌入 done == 真图数（{}={n}）", drep.done),
    );
    c.ok(
        drep.failed == 0,
        &format!("去重嵌入失败 0（实际 {}）", drep.failed),
    );
    c.ok(
        dedup_vecs == n as i64,
        &format!("去重向量落库 {dedup_vecs}/{n}"),
    );
    summary.insert(
        "embed_dedup".into(),
        json!({"model": dedup_model, "dims": dedup_dims, "load_ms": dedup_load_ms, "ms": dedup_ms,
               "done": drep.done, "failed": drep.failed, "vectors": dedup_vecs, "img_per_s": dedup_ips,
               "per_img_ms": dedup_ms as f64 / n as f64}),
    );

    // ---------- 4. 文搜图 ----------
    println!("\n== 4. 文搜图（text → image）==");
    let filter = SearchFilter::default();
    let mut queries_json = Vec::new();
    for q in &a.queries {
        let t = Instant::now();
        let hits = svc_library::search_by_text(
            &conn,
            sem.as_ref(),
            q,
            a.top_k,
            &filter,
            &svc_library::TextSearchOptions::default(),
        )?;
        let ms = t.elapsed().as_micros() as f64 / 1000.0;
        println!("  query「{q}」 hits={} 用时 {ms:.1} ms", hits.len());
        let mut rows = Vec::new();
        for (i, h) in hits.iter().enumerate() {
            let abs: Option<String> = conn
                .query_row(
                    "SELECT absPath FROM assets WHERE assetId=?1",
                    [h.assetId],
                    |r| r.get(0),
                )
                .ok();
            println!(
                "    {}. assetId={} score={:.4} file={} abs={}",
                i + 1,
                h.assetId,
                h.score,
                h.fileName,
                abs.clone().unwrap_or_default()
            );
            rows.push(
                json!({"rank": i + 1, "assetId": h.assetId, "score": h.score,
                             "fileName": h.fileName, "absPath": abs,
                             "titleCh": h.titleCh, "width": h.width, "height": h.height}),
            );
        }
        let descending = hits.windows(2).all(|w| w[0].score >= w[1].score);
        c.ok(!hits.is_empty(), &format!("query「{q}」有命中"));
        c.ok(
            hits.len() == a.top_k.min(n),
            &format!("query「{q}」返回 top{}（实际 {}）", a.top_k, hits.len()),
        );
        c.ok(descending, &format!("query「{q}」score 降序"));
        let in_top10 = hits.len();
        queries_json.push(json!({"query": q, "ms": ms, "hits": in_top10, "rows": rows}));
    }
    summary.insert(
        "text_search".into(),
        json!({"top_k": a.top_k, "queries": queries_json}),
    );

    // ---------- 5. 以图搜图（自检索 top1 = 自身）----------
    println!("\n== 5. 以图搜图（image → image，自检索一致性）==");
    let mut stmt =
        conn.prepare("SELECT assetId, absPath FROM assets WHERE deleted=0 ORDER BY assetId")?;
    let all: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    let probe_n = 20.min(all.len());
    let mut self_hits = 0usize;
    let mut lat = Vec::new();
    for (aid, path) in all.iter().take(probe_n) {
        let t = Instant::now();
        let hits = svc_library::search_by_image_path(
            &conn,
            sem.as_ref(),
            Path::new(path),
            a.top_k.min(5),
            &filter,
            // dev 基准测的是 KNN 机制本身，关掉相关性下限（阈值另由 w3 标定脚本验）
            &svc_library::SimilarOptions { min_score: Some(0.0) },
        )?;
        lat.push(t.elapsed().as_micros() as f64 / 1000.0);
        let top = hits.first();
        let hit_self = top
            .map(|h| h.assetId == *aid && h.score > 0.99)
            .unwrap_or(false);
        if hit_self {
            self_hits += 1;
        }
        println!(
            "  asset {aid} → top1={:?} score={:.4} self_hit={hit_self}",
            top.map(|h| h.assetId),
            top.map(|h| h.score).unwrap_or(0.0)
        );
    }
    let lat_avg = lat.iter().sum::<f64>() / lat.len().max(1) as f64;
    println!("  {self_hits}/{probe_n} 自检索命中，平均 {lat_avg:.1} ms/次");
    c.ok(
        self_hits == probe_n,
        &format!("以图搜图自检索 top1=自身 {self_hits}/{probe_n}"),
    );
    summary.insert(
        "image_search".into(),
        json!({"probes": probe_n, "self_hits": self_hits, "avg_ms": lat_avg}),
    );

    // ---------- 6. 查重（DINOv3）----------
    println!("\n== 6. 查重（DINOv3 阈值分带）==");
    let dedup_json = dedup_step(&conn, dedup.as_ref(), &mut c, true)?;
    summary.insert("dedup".into(), dedup_json);

    // ---------- 7. 云 VL 打标 ----------
    println!("\n== 7. 云 VL 打标（真实 API）==");
    let tag_report = if !a.tag_enabled {
        println!("  跳过（--no-tag）");
        json!({"skipped": "flag --no-tag"})
    } else if a.tag_api_key.trim().is_empty()
        || a.tag_base_url.trim().is_empty()
        || a.tag_model.trim().is_empty()
    {
        println!("  跳过（缺 D6_TAG_BASE_URL / D6_TAG_API_KEY / D6_TAG_MODEL）——未配置即不跑，不伪造结果");
        json!({"skipped": "no-credentials"})
    } else {
        let spec = registry
            .models
            .iter()
            .find(|m| m.kind == SpecKind::Tagging)
            .cloned()
            .ok_or("registry 无 tagging spec")?;
        let cfg = TagSettings {
            enabled: true,
            provider: a.tag_provider.clone(),
            base_url: a.tag_base_url.clone(),
            api_key: a.tag_api_key.clone(),
            model: a.tag_model.clone(),
            prompt_template: core_config::DEFAULT_TAG_PROMPT.to_string(),
            max_tokens: 1024,
            timeout_ms: a.tag_timeout_ms,
            // 外网云 API：直连（容器 env 里有 HTTP(S)_PROXY，显式置空避免走代理抖动）
            proxy: Some(String::new()),
        };
        let mut provider = OpenAiCompatTagProvider::new(spec, cfg);
        provider.load()?;
        // D9 验收：测试连接（最小 chat/completions，文本即可）先于批量打标
        let conn_t0 = Instant::now();
        match provider.test_connection() {
            Ok(reply) => println!(
                "  测试连接 OK（{} ms）：{}",
                conn_t0.elapsed().as_millis(),
                reply.chars().take(40).collect::<String>()
            ),
            Err(e) => {
                let msg = format!("测试连接失败：{e}");
                c.ok(false, &msg);
                return Err(msg.into());
            }
        }
        let queued = enq_untagged(&conn)?;
        // 只跑前 tag_n 张：删掉多余的 pending job（诚实裁剪，报告里说明）
        let trimmed = conn.execute(
            "DELETE FROM job_queue WHERE kind='tag' AND status='pending' AND id NOT IN
             (SELECT id FROM job_queue WHERE kind='tag' AND status='pending' ORDER BY id LIMIT ?1)",
            [a.tag_n as i64],
        )?;
        let cancel = AtomicBool::new(false);
        let mut prev = Instant::now();
        let mut per_img_ms: Vec<f64> = Vec::new();
        let t = Instant::now();
        let trep = run_tag_queue(&conn, &provider, &cancel, &mut |p| {
            let now = Instant::now();
            let dt = now.duration_since(prev).as_secs_f64() * 1000.0;
            prev = now;
            if p.status == "done" {
                per_img_ms.push(dt);
                println!(
                    "  tag {}/{} asset={} done {:.0} ms",
                    p.index, p.total, p.assetId, dt
                );
            } else {
                println!(
                    "  tag {}/{} asset={} {} {:?}",
                    p.index, p.total, p.assetId, p.status, p.error
                );
            }
        })?;
        let tag_ms = t.elapsed().as_millis() as u64;
        let tagged: i64 = conn.query_row(
            "SELECT COUNT(*) FROM assets WHERE title_ch IS NOT NULL AND TRIM(title_ch) <> ''",
            [],
            |r| r.get(0),
        )?;
        let tag_rows: i64 =
            conn.query_row("SELECT COUNT(*) FROM tag WHERE origin='auto'", [], |r| {
                r.get(0)
            })?;
        let tagless: i64 = conn.query_row(
            "SELECT COUNT(*) FROM assets a WHERE a.deleted=0 AND NOT EXISTS (SELECT 1 FROM tag t WHERE t.assetId=a.assetId)",
            [],
            |r| r.get(0),
        )?;
        let avg = per_img_ms.iter().sum::<f64>() / per_img_ms.len().max(1) as f64;
        println!(
            "  queued={queued} 裁剪={trimmed} processed={} done={} failed={} 用时 {tag_ms} ms，平均 {avg:.0} ms/张（{:.2} img/min）",
            trep.processed,
            trep.done,
            trep.failed,
            60_000.0 / avg.max(1e-6)
        );
        if !trep.errors.is_empty() {
            println!(
                "  errors: {:?}",
                trep.errors.iter().take(5).collect::<Vec<_>>()
            );
        }
        // 抽样打印 10 条打标结果（人工可核）
        let mut stmt = conn.prepare(
            "SELECT assetId, fileName, title_ch, title_en, description FROM assets
             WHERE title_ch IS NOT NULL AND TRIM(title_ch) <> '' ORDER BY assetId LIMIT 10",
        )?;
        let samples: Vec<(i64, String, String, Option<String>, Option<String>)> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        let mut sample_json = Vec::new();
        for (aid, file, ch, en, desc) in samples {
            let mut tags_stmt = conn.prepare(
                "SELECT name, score FROM tag WHERE assetId=?1 ORDER BY score DESC LIMIT 8",
            )?;
            let tags: Vec<(String, f64)> = tags_stmt
                .query_map([aid], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(tags_stmt);
            println!(
                "  asset {aid} {file}\n    title_ch={ch}\n    title_en={}\n    desc={}\n    tags={:?}",
                en.clone().unwrap_or_default(),
                desc.clone().unwrap_or_default(),
                tags
            );
            sample_json.push(
                json!({"assetId": aid, "fileName": file, "titleCh": ch, "titleEn": en,
                                    "description": desc, "tags": tags}),
            );
        }
        c.ok(
            trep.done == a.tag_n,
            &format!("打标 done == {}（实际 {}）", a.tag_n, trep.done),
        );
        c.ok(
            trep.failed == 0,
            &format!("打标失败 0（实际 {}）", trep.failed),
        );
        c.ok(
            tagged == a.tag_n as i64,
            &format!("title_ch 落库 {tagged}/{}", a.tag_n),
        );
        // 本步有意只跑前 tag_n 张（job 裁剪），故未打标资产应为 总数 - tag_n
        c.ok(
            tagless == (n - a.tag_n) as i64,
            &format!("未打标资产 = 总数-打标数（{} vs {}）", tagless, n - a.tag_n),
        );
        c.ok(
            tag_rows >= a.tag_n as i64,
            &format!("auto 标签行 ≥{0}（实际 {1}）", a.tag_n, tag_rows),
        );
        json!({"model": a.tag_model, "base_url": a.tag_base_url, "provider": a.tag_provider,
               "api_key_masked": mask(&a.tag_api_key),
               "queued": queued, "trimmed_to": trimmed, "processed": trep.processed,
               "done": trep.done, "failed": trep.failed, "ms": tag_ms, "avg_ms_per_img": avg,
               "img_per_min": 60_000.0 / avg.max(1e-6),
               "title_ch_count": tagged, "auto_tag_rows": tag_rows, "samples": sample_json})
    };
    summary.insert("tagging".into(), tag_report);

    // ---------- 8. 库统计 + 规模外推 ----------
    println!("\n== 8. 库统计与规模外推 ==");
    let stats = svc_library::stats(&conn)?;
    let tables = core_storage::vecstore::tables(&conn)?;
    println!(
        "  assets={} missing={} tagged={} bytes={}",
        stats.assets, stats.missing, stats.tagged, stats.totalBytes
    );
    for (model, dims, table) in &tables {
        println!(
            "  向量表 {table} model={model} dims={dims} rows={}",
            core_storage::vecstore::count(&conn, model)?
        );
    }
    let per_img_ms = sem_ms as f64 / n as f64;
    let extrapolate = |count: f64| {
        json!({
            "images": count,
            "hours_serial": count * per_img_ms / 3_600_000.0,
            "hours_8way": count * per_img_ms / 3_600_000.0 / 8.0,
        })
    };
    println!(
        "  外推（语义塔 {per_img_ms:.1} ms/图，单进程串行）：10 万 ≈ {:.1} h，100 万 ≈ {:.1} h；8 路并发 ≈ {:.1} h / {:.1} h",
        extrapolate(100_000.0)["hours_serial"].as_f64().unwrap(),
        extrapolate(1_000_000.0)["hours_serial"].as_f64().unwrap(),
        extrapolate(100_000.0)["hours_8way"].as_f64().unwrap(),
        extrapolate(1_000_000.0)["hours_8way"].as_f64().unwrap()
    );
    summary.insert("library".into(), json!({"assets": stats.assets, "missing": stats.missing,
        "tagged": stats.tagged, "totalBytes": stats.totalBytes,
        "vec_tables": tables.iter().map(|(n, d, t)| json!({"name": n, "dims": d, "type": t})).collect::<Vec<_>>()}));
    summary.insert(
        "extrapolation".into(),
        json!({
        "semantic_per_img_ms": per_img_ms,
        "100k": extrapolate(100_000.0), "1M": extrapolate(1_000_000.0)}),
    );
    summary.insert("finished_at".into(), json!(now_iso()));

    std::fs::write(
        &a.json_out,
        serde_json::to_string_pretty(&serde_json::Value::Object(summary))?,
    )?;
    println!("\n汇总 JSON → {}", a.json_out.display());
    Ok(c.fails)
}

/// 查重步骤（可单独调用）：find_duplicates + 成员明细（assetId/fileName/与锚点相似度）。
fn dedup_step(
    conn: &Connection,
    provider: &dyn EmbedProvider,
    c: &mut Checks,
    detail: bool,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let opts = DupOptions::default();
    let t = Instant::now();
    let groups = svc_library::find_duplicates(conn, provider, &opts)?;
    let ms = t.elapsed().as_millis() as u64;
    let bands: BTreeMap<String, usize> = groups.iter().fold(BTreeMap::new(), |mut acc, g| {
        *acc.entry(g.band.clone()).or_insert(0) += 1;
        acc
    });
    println!("  分组 {} 组（{:?}）用时 {ms} ms", groups.len(), bands);
    let mut groups_json = Vec::new();
    for g in &groups {
        let name = |aid: i64| -> String {
            conn.query_row("SELECT fileName FROM assets WHERE assetId=?1", [aid], |r| {
                r.get(0)
            })
            .unwrap_or_default()
        };
        let members: Vec<String> = g
            .members
            .iter()
            .map(|m| format!("{}:{}:{:.4}", m.assetId, name(m.assetId), m.score))
            .collect();
        if detail {
            println!(
                "    anchor={} ({}) band={} members=[{}]",
                g.anchorAssetId,
                name(g.anchorAssetId),
                g.band,
                members.join(", ")
            );
        }
        groups_json.push(
            json!({"anchorAssetId": g.anchorAssetId, "anchorFileName": name(g.anchorAssetId),
                                "band": g.band, "members": members}),
        );
    }
    let bands_valid = groups
        .iter()
        .all(|g| g.band == "near" || g.band == "variant");
    c.ok(bands_valid, "查重分组 band 取值合法（near/variant）");
    Ok(json!({"groups": groups.len(), "bands": bands, "ms": ms, "detail": groups_json}))
}

fn mask(secret: &str) -> String {
    let s = secret.trim();
    if s.len() <= 6 {
        "***".to_string()
    } else {
        format!("{}…{}", &s[..4], &s[s.len() - 2..])
    }
}

fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("epoch:{secs}")
}
