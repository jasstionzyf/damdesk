//! COMPLIANCE-DOMAIN spike（卡 task）—— **领域 gate 的真图端到端证据**。
//!
//! 只在**外面**观察引擎：造库（真图语料）→ 真 pack → 真编排（逐图 job + 组级 job）→
//! 真 VL → 逐图判定与证据 JSON。不实现任何判定逻辑。
//!
//! 为什么单开一个 spike 而不是改 c9_spike：c9 的结论是「组级判定按成员展开」，
//! 本卡的结论是「领域 gate 先判 + 非目标图短路且不产生 VL 调用」，两者证据结构不同
//! （这里要逐图 `F0_domain.raw` 原始返回、短路行、以及**组级阶段 VL 调用数**）。
//!
//! 环境变量：
//!   DSPIKE_DB         目标库 SQLite（默认 /tmp/c11/db/library.db；每次运行重建）
//!   DSPIKE_CORPUS     语料根（默认 /models/corpus；子目录 property / nonproperty）
//!   DSPIKE_WORK       资产落盘目录（默认 /tmp/c11/work；按 <bucket>/<keyword> 分目录 = 组键）
//!   DSPIKE_PACK       **必填**：自备四件 YAML 包目录（宿主已不内置行业包，IP-K16）
//!   DSPIKE_OUT        证据输出目录（默认 <repo>/reports/logs/c11）
//!   DSPIKE_TAG        证据文件名前缀（默认 domain-run；before/after 各自一个 tag）
//!   DSPIKE_WORKERS    并行 worker 数（默认 8）
//!   DSPIKE_SCOPE      asset|group|both（默认 both）
//!   DSPIKE_VL_BASE_URL / DSPIKE_VL_MODEL / DSPIKE_VL_API_KEY / DSPIKE_VL_PROXY
//!                     （缺 key → 直接报错退出：领域 gate 没有 mock 意义）

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use core_storage::compliance::ComplianceRepo;
use rusqlite::Connection;
use svc_pipeline::compliance_queue::{
    compliance_enq, compliance_enq_group, run_compliance_queue_par, ConnFactory,
};
use svc_pipeline::VlConfig;

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| d.to_string())
}

#[allow(dead_code)] // 保留：spike 桩 env 读取器
fn env_opt(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

fn repo_root() -> PathBuf {
    // dev-e2e 从 workspace 根跑（cargo 的 CARGO_MANIFEST_DIR = crates/dev-e2e）
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn vl_config() -> VlConfig {
    let key = env_or("DSPIKE_VL_API_KEY", "");
    if key.trim().is_empty() {
        eprintln!("缺 DSPIKE_VL_API_KEY：领域 gate 必须真 VL（本 spike 不做 mock）");
        std::process::exit(2);
    }
    VlConfig {
        base_url: env_or(
            "DSPIKE_VL_BASE_URL",
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
        ),
        api_key: key,
        model: env_or("DSPIKE_VL_MODEL", "qwen3-vl-plus"),
        timeout_secs: env_or("DSPIKE_VL_TIMEOUT", "90").parse().unwrap_or(90),
        max_tokens: 4096,
        // 显式直连（云端兼容端点不需要本机代理）
        proxy: Some(env_or("DSPIKE_VL_PROXY", "")),
    }
}

struct AssetRow {
    bucket: String,
    keyword: String,
    file: String,
}

/// 语料 → 资产落盘目录：`<work>/<bucket>/<keyword>/<file>`（目录 = 组键 = 一套「房源」）。
/// 返回 [(absPath, meta)]，顺序确定（bucket, keyword, file 升序）。
fn stage_corpus(corpus: &Path, work: &Path) -> Vec<(PathBuf, AssetRow)> {
    let mut out = Vec::new();
    // 目录语义：`<corpus>/<bucket>/<file>` = 该 bucket 一组；`<corpus>/<bucket>/<set>/<file>`
    // = 一个「套」（组键 = 套目录，用于组级规则 F3/F4 的齐套判定）。
    for bucket in ["property", "nonproperty"] {
        let dir = corpus.join(bucket);
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
        entries.sort();
        for entry in entries {
            let (files, set_name): (Vec<PathBuf>, Option<String>) = if entry.is_dir() {
                let mut inner: Vec<PathBuf> = std::fs::read_dir(&entry)
                    .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
                    .unwrap_or_default();
                inner.sort();
                (
                    inner,
                    Some(entry.file_name().unwrap().to_string_lossy().to_string()),
                )
            } else {
                (vec![entry.clone()], None)
            };
            for src in files {
                if !matches!(
                    src.extension().and_then(|x| x.to_str()),
                    Some("jpg") | Some("jpeg") | Some("png")
                ) {
                    continue;
                }
                let name = src.file_name().unwrap().to_string_lossy().to_string();
                // keyword = 文件名去 `-NN`（fetch 脚本约定）；套目录时 keyword = 套名
                let keyword = set_name.clone().unwrap_or_else(|| {
                    name.rsplit_once('-')
                        .map(|(k, _)| k.to_string())
                        .unwrap_or_else(|| "misc".into())
                });
                let dest_dir = work.join(bucket).join(&keyword);
                std::fs::create_dir_all(&dest_dir).expect("建资产目录");
                let dest = dest_dir.join(&name);
                if !dest.exists() {
                    std::fs::copy(&src, &dest).expect("拷贝语料");
                }
                out.push((
                    dest,
                    AssetRow {
                        bucket: bucket.to_string(),
                        keyword,
                        file: name,
                    },
                ));
            }
        }
    }
    out
}

fn conn_factory(db: PathBuf) -> ConnFactory {
    std::sync::Arc::new(move || {
        let c = Connection::open(&db)?;
        c.busy_timeout(std::time::Duration::from_secs(15))?;
        c.pragma_update(None, "journal_mode", "WAL")?;
        Ok(c)
    })
}

fn main() {
    let corpus = PathBuf::from(env_or("DSPIKE_CORPUS", "/models/corpus"));
    let work = PathBuf::from(env_or("DSPIKE_WORK", "/tmp/c11/work"));
    let db = PathBuf::from(env_or("DSPIKE_DB", "/tmp/c11/db/library.db"));
    let pack_dir = PathBuf::from(legacy_pack_dir("DSPIKE_PACK"));
    let out_dir = PathBuf::from(env_or(
        "DSPIKE_OUT",
        &repo_root().join("reports/logs/c11").to_string_lossy(),
    ));
    let tag = env_or("DSPIKE_TAG", "domain-run");
    let scope = env_or("DSPIKE_SCOPE", "both");
    let workers: usize = env_or("DSPIKE_WORKERS", "8").parse().unwrap_or(8);

    std::fs::create_dir_all(&out_dir).expect("建证据目录");
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent).expect("建库目录");
    }
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(db.with_extension("db-wal"));
    let _ = std::fs::remove_file(db.with_extension("db-shm"));

    // 1) 语料落盘（分组目录 = 组键）
    let staged = stage_corpus(&corpus, &work);
    if staged.is_empty() {
        eprintln!("语料为空：{corpus:?}");
        std::process::exit(2);
    }

    // 2) 建库 + 注册资产
    let conn = Connection::open(&db).expect("开库");
    core_storage::migrations::migrate(&conn).expect("迁移");
    let mut ids: Vec<i64> = Vec::new();
    for (path, _meta) in &staged {
        conn.execute(
            "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (NULL, ?1, 'image', ?2, ?3, ?4, 0, 0)",
            rusqlite::params![
                path.to_string_lossy(),
                path.file_name().unwrap().to_string_lossy(),
                path.extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("jpg")
                    .to_lowercase(),
                std::fs::metadata(path).map(|m| m.len() as i64).unwrap_or(1),
            ],
        )
        .expect("插资产");
        let id = conn.last_insert_rowid();
        let (w, h) = core_media::image_size(path).expect("读尺寸");
        conn.execute(
            "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1, 'primary', 0, ?2, ?3)",
            rusqlite::params![id, w as i64, h as i64],
        )
        .expect("插资源");
        ids.push(id);
    }

    // 3) 装载 pack（真装载路径：L0 键级 lint + L2 注册表 lint）
    let pack = core_config::Pack::from_dir(&pack_dir).expect("pack 装载失败（lint 不过）");

    // 4) 入队 + 真跑
    let out_s = db
        .parent()
        .unwrap()
        .join("fixes")
        .to_string_lossy()
        .to_string();
    std::fs::create_dir_all(&out_s).ok();
    let mut asset_jobs = 0usize;
    let mut group_jobs = 0usize;
    if scope != "group" {
        asset_jobs = compliance_enq(&conn, &ids, &pack.id, Some(&out_s)).expect("资产入队");
    }
    if scope != "asset" {
        group_jobs = compliance_enq_group(&conn, &pack, &ids, Some(&out_s)).expect("组级入队");
    }
    let vl = vl_config();
    let cancel = AtomicBool::new(false);
    let started = std::time::Instant::now();
    let (report, vl_snap) = run_compliance_queue_par(
        conn_factory(db.clone()),
        &pack,
        Some(vl),
        workers,
        &cancel,
        |p| {
            if p.status == "failed" || p.status == "retry" {
                eprintln!("[domain] job{} {}: {:?}", p.jobId, p.status, p.error);
            }
        },
    )
    .expect("编排失败");
    let wall = started.elapsed().as_secs_f64();

    // 5) 逐图判定（资产口径 + 组级口径都在；汇总走产品同一套 state_of_verdicts）
    let repo = core_storage::compliance_sqlite::SqliteComplianceRepo::new(&conn);
    let mut assets_json: Vec<serde_json::Value> = Vec::new();
    let (mut prop_pass, mut prop_fail, mut prop_undet, mut prop_unjudged) = (0, 0, 0, 0);
    let (mut non_pass, mut non_fail, mut non_undet, mut non_unjudged) = (0, 0, 0, 0);
    // 短路证据计数
    let mut skipped_by_rule: std::collections::BTreeMap<String, i64> = Default::default();
    for (i, id) in ids.iter().enumerate() {
        let meta = &staged[i].1;
        let rows = repo.find_asset_verdicts(&pack.id, *id).expect("查判定");
        let refs: Vec<&core_storage::compliance::VerdictRow> = rows.iter().collect();
        let state = svc_library::compliance_view::state_of_verdicts(&refs);
        let state_name = format!("{state:?}").to_lowercase();
        match state {
            svc_library::compliance_view::AssetComplianceState::Pass => {
                if meta.bucket == "property" {
                    prop_pass += 1
                } else {
                    non_pass += 1
                }
            }
            svc_library::compliance_view::AssetComplianceState::Fail => {
                if meta.bucket == "property" {
                    prop_fail += 1
                } else {
                    non_fail += 1
                }
            }
            svc_library::compliance_view::AssetComplianceState::Undetermined => {
                if meta.bucket == "property" {
                    prop_undet += 1
                } else {
                    non_undet += 1
                }
            }
            svc_library::compliance_view::AssetComplianceState::Unjudged => {
                if meta.bucket == "property" {
                    prop_unjudged += 1
                } else {
                    non_unjudged += 1
                }
            }
        }
        let mut verdicts = Vec::new();
        for r in &rows {
            if r.result == "skipped" {
                *skipped_by_rule.entry(r.rule_id.clone()).or_insert(0) += 1;
            }
            let evidence: serde_json::Value = r
                .evidence
                .as_deref()
                .and_then(|e| serde_json::from_str(e).ok())
                .unwrap_or(serde_json::Value::Null);
            verdicts.push(serde_json::json!({
                "ruleId": r.rule_id,
                "scope": r.judgement_scope,
                "result": r.result,
                "confidence": r.confidence,
                "score": r.score,
                "fixAction": r.fix_action,
                "recheck": r.recheck_result,
                "evidence": evidence,
            }));
        }
        assets_json.push(serde_json::json!({
            "assetId": id,
            "bucket": meta.bucket,
            "keyword": meta.keyword,
            "file": meta.file,
            "absPath": staged[i].0.to_string_lossy(),
            "state": state_name,
            "verdicts": verdicts,
        }));
    }

    let v = serde_json::json!({
        "pack": pack.id,
        "packDir": pack_dir.to_string_lossy(),
        "scope": scope,
        "workers": workers,
        "corpus": corpus.to_string_lossy(),
        "assets": ids.len(),
        "queuedAssetJobs": asset_jobs,
        "queuedGroupJobs": group_jobs,
        "wallSecs": wall,
        "report": serde_json::to_value(&report).unwrap(),
        "vlStats": vl_snap.map(|s| serde_json::to_value(s).unwrap()),
        "buckets": {
            "property": { "pass": prop_pass, "fail": prop_fail, "undetermined": prop_undet, "unjudged": prop_unjudged },
            "nonproperty": { "pass": non_pass, "fail": non_fail, "undetermined": non_undet, "unjudged": non_unjudged },
        },
        "skippedRowsByRule": skipped_by_rule,
        "assetsDetail": assets_json,
    });
    let path = out_dir.join(format!("{tag}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).expect("写证据");
    println!(
        "DOMAIN_RESULT tag={tag} pack={} assets={} prop(pass={} fail={} undet={}) nonprop(pass={} fail={} undet={}) vlCalls={} wall={:.1}s → {}",
        pack.id,
        ids.len(),
        prop_pass,
        prop_fail,
        prop_undet,
        non_pass,
        non_fail,
        non_undet,
        v["vlStats"]["calls"].as_u64().unwrap_or(0),
        wall,
        path.display()
    );
}

/// IP-K16：宿主不再内置 YAML 行业包（`packs/<行业>/*.yaml` 已删）——legacy 内联引擎的
/// 开发 spike 只能对**自备**的四件 YAML 包跑，目录必须显式给。
fn legacy_pack_dir(env: &str) -> String {
    std::env::var(env).unwrap_or_else(|_| {
        panic!(
            "宿主不再内置 YAML 行业包（IP-K16）：把 {env} 指到自备的 taxonomy/rules/pipeline/report 四件 YAML 目录"
        )
    })
}
