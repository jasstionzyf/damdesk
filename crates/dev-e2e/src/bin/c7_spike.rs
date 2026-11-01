//! C7 房产包 spike 探针（不改引擎代码：本文件是 L3 dev-harness，只在**外面**观察引擎行为）。
//!
//! 目的：回答「换行业 = 换 pack 文件」这条产品承诺里，**哪些部分是纯配置覆盖，哪些必须动
//! 引擎/插件**。四个模式各自产一份 JSON 证据（落 `C7_OUT`）：
//!
//! - `C7_MODE=calibrate`  在真图上量 `sharpness`/`exposure`/`size_min` 的分数分布 →
//!   房产包阈值不是拍脑袋（draft 里写 `<C1 实测定>`，这里就是那个「实测定」）。
//! - `C7_MODE=load`       `Pack::from_dir` 装载 + **逐规则配置探针**：classic 规则拿合成图
//!   真跑一次注册表；vl 规则把 `check.params` 的键与引擎真正读的键对账；local_model 查模型
//!   注册表。产「零代码覆盖 / 未知检测器 / 未知参数（被静默忽略）」三类判定。
//! - `C7_MODE=run`        在真图库上跑该 pack 的**真合规编排**（同一套编排器 / 落库 / VL 通道），
//!   产真判定 + 逐规则分桶 + 失败明细。
//! - `C7_MODE=groups`     从真库读判定 → 按资产分组键（文件名前缀）聚合；同时用 L1
//!   `engine-dedup` 在组内算 pHash 近似重复分组，用来区分「能力不存在」与「能力存在但 pack
//!   够不着」。
//!
//! 环境变量：
//!   C7_MODE=calibrate|load|run|groups（默认 load）
//!   C7_PACK_DIR   **必填**：自备四件 YAML 包目录（宿主已不内置行业包，IP-K16）
//!   C7_DB         真图库 SQLite（run/groups）
//!   C7_STAGE      真图目录（calibrate；默认 /tmp/c5-stage）
//!   C7_LIMIT      处理张数上限（0 = 全部）
//!   C7_OUT        证据输出目录（默认 /workspace/reports/logs/c7）
//!   C7_VL_BASE_URL / C7_VL_MODEL / C7_VL_API_KEY / C7_VL_PROXY（run 模式的 vl 通道；缺 key → 记

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use svc_pipeline::compliance_queue::{compliance_enq, run_compliance_queue_par, ConnFactory};
use svc_pipeline::VlConfig;

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| d.to_string())
}

fn out_dir() -> PathBuf {
    PathBuf::from(env_or("C7_OUT", "/workspace/reports/logs/c7"))
}

fn dump(name: &str, v: &serde_json::Value) {
    let dir = out_dir();
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join(name);
    std::fs::write(&p, serde_json::to_string_pretty(v).unwrap()).unwrap();
    println!("[C7] wrote {}", p.display());
    println!("[C7_JSON] {} {}", name, serde_json::to_string(v).unwrap());
}

fn main() {
    let mode = env_or("C7_MODE", "load");
    let code = match mode.as_str() {
        "calibrate" => calibrate(),
        "load" => load(),
        "probes" => probes(),
        "dedup_probe" => dedup_probe(),
        "run" => run(),
        "groups" => groups(),
        other => {
            eprintln!("C7_RESULT FAIL unknown mode {other}");
            2
        }
    };
    std::process::exit(code);
}

/// IP-K16：宿主不再内置 YAML 行业包 —— legacy 内联引擎的开发 spike 只能对自备包跑。
fn pack_dir() -> PathBuf {
    PathBuf::from(std::env::var("C7_PACK_DIR").unwrap_or_else(|_| {
        panic!(
            "宿主不再内置 YAML 行业包（IP-K16）：把 C7_PACK_DIR 指到自备的 taxonomy/rules/pipeline/report 四件 YAML 目录"
        )
    }))
}

fn limit() -> usize {
    env_or("C7_LIMIT", "0").parse().unwrap_or(0)
}

// ================= 模式 1：真图阈值标定 =================

fn calibrate() -> i32 {
    let stage = PathBuf::from(env_or("C7_STAGE", "/tmp/c5-stage"));
    let lim = limit();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&stage)
        .unwrap_or_else(|e| panic!("读语料目录 {stage:?}: {e}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase()).as_deref(),
                Some("jpg") | Some("jpeg") | Some("png")
            )
        })
        .collect();
    files.sort();
    if lim > 0 {
        files.truncate(lim);
    }
    let t = Instant::now();
    let mut sharp: Vec<f64> = Vec::new();
    let mut dead: Vec<f64> = Vec::new();
    let mut mean_l: Vec<f64> = Vec::new();
    let mut short_side: Vec<f64> = Vec::new();
    let mut decode_fail = 0usize;
    let mut probe_err: BTreeMap<String, usize> = Default::default();
    for f in &files {
        let (w, h, px) = match core_media::rgb8(f) {
            Ok(v) => v,
            Err(_) => {
                decode_fail += 1;
                continue;
            }
        };
        short_side.push(w.min(h) as f64);
        let input = engine_classic::registry::DetectorInput::Rgb {
            width: w as u32,
            height: h as u32,
            pixels: px,
        };
        // 阈值取「宽到底」的 params：这里量的是分数分布，不是判定；宽阈值保证 0 误报。
        match engine_classic::registry::run(
            "sharpness",
            &input,
            &serde_yaml::from_str("{min: 0.0}").unwrap(),
        ) {
            Ok(o) => sharp.push(o.score),
            Err(e) => *probe_err.entry(format!("sharpness: {e}")).or_default() += 1,
        }
        match engine_classic::registry::run(
            "exposure",
            &input,
            &serde_yaml::from_str("{max_dead_ratio: 1.0}").unwrap(),
        ) {
            Ok(o) => {
                dead.push(o.score);
                mean_l.push(o.metrics.get("mean").copied().unwrap_or(f64::NAN));
            }
            Err(e) => *probe_err.entry(format!("exposure: {e}")).or_default() += 1,
        }
    }
    let pct = |v: &mut Vec<f64>, p: f64| -> f64 {
        if v.is_empty() {
            return f64::NAN;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let idx = ((v.len() - 1) as f64 * p).round() as usize;
        v[idx]
    };
    let stat = |v: &mut Vec<f64>| -> serde_json::Value {
        let n = v.len();
        let (p1, p5, p10, p25, p50, p75, p90, p99) = (
            pct(v, 0.01),
            pct(v, 0.05),
            pct(v, 0.10),
            pct(v, 0.25),
            pct(v, 0.50),
            pct(v, 0.75),
            pct(v, 0.90),
            pct(v, 0.99),
        );
        let min = v.first().copied().unwrap_or(f64::NAN);
        let max = v.last().copied().unwrap_or(f64::NAN);
        let mean = if n > 0 { v.iter().sum::<f64>() / n as f64 } else { f64::NAN };
        serde_json::json!({
            "n": n, "min": min, "p1": p1, "p5": p5, "p10": p10, "p25": p25,
            "p50": p50, "p75": p75, "p90": p90, "p99": p99, "max": max, "mean": mean,
        })
    };
    let out = serde_json::json!({
        "stage": stage.to_string_lossy(),
        "images": files.len(),
        "decode_fail": decode_fail,
        "probe_errors": probe_err,
        "sharpness_variance": stat(&mut sharp),
        "exposure_dead_ratio": stat(&mut dead),
        "exposure_mean_luma": stat(&mut mean_l),
        "short_side_px": stat(&mut short_side),
        "secs": t.elapsed().as_secs_f64(),
    });
    dump("c7-calibration.json", &out);
    println!(
        "C7_RESULT CALIBRATED images={} decode_fail={} secs={:.1}",
        files.len(),
        decode_fail,
        t.elapsed().as_secs_f64()
    );
    0
}

// ================= 模式 2：pack 装载 + 逐规则配置探针 =================

fn load() -> i32 {
    let dir = pack_dir();
    let pack = match core_config::Pack::from_dir(&dir) {
        Ok(p) => p,
        Err(e) => {
            println!("C7_RESULT LOAD_FAIL pack={} reason={e}", dir.display());
            return 1;
        }
    };
    // 合成探针图（非平凡：能真跑通拉普拉斯/直方图路径）
    let rgb = synth_rgb();

    let mut rules_out = Vec::new();
    let mut zero_code = 0usize;
    let mut needs_code: Vec<String> = Vec::new();
    for r in &pack.rules {
        let mut item = serde_json::json!({
            "ruleId": r.id,
            "executor": format!("{:?}", r.executor).to_lowercase(),
            "onFail": format!("{:?}", r.on_fail).to_lowercase(),
            "tagging": r.is_tagging(),
        });
        let mut verdict = "zero_code";
        let mut detail = serde_json::Value::Null;
        let mut ignored: Vec<String> = Vec::new();
        match r.executor {
            core_config::RuleExecutor::Classic => {
                let det = r.check.as_ref().map(|c| c.detector.clone()).unwrap_or_default();
                let params = r
                    .check
                    .as_ref()
                    .map(|c| c.params.clone())
                    .unwrap_or(serde_yaml::Value::Null);
                match engine_classic::registry::lookup(&det) {
                    None => {
                        verdict = "unknown_detector";
                        detail = serde_json::json!({
                            "detector": det,
                            "known": engine_classic::registry::known_ids(),
                        });
                        needs_code.push(format!("{}: 检测器 `{}` 未注册", r.id, det));
                    }
                    Some(entry) => {
                        // params 探针：deny_unknown_fields 的检测器会响亮拒绝拼错的键
                        let probe = match entry.input {
                            engine_classic::registry::InputKind::Metadata => {
                                engine_classic::registry::DetectorInput::Metadata {
                                    width: 1600,
                                    height: 1200,
                                }
                            }
                            engine_classic::registry::InputKind::Rgb => rgb.clone(),
                        };
                        match engine_classic::registry::run(&det, &probe, &params) {
                            Ok(o) => {
                                detail = serde_json::json!({
                                    "detector": det, "input": format!("{:?}", entry.input),
                                    "probe_score": o.score, "probe_pass": o.pass,
                                });
                                zero_code += 1;
                            }
                            Err(e) => {
                                verdict = "params_rejected";
                                detail = serde_json::json!({ "detector": det, "error": e.to_string() });
                                needs_code.push(format!("{}: 检测器参数被拒 — {e}", r.id));
                            }
                        }
                    }
                }
            }
            core_config::RuleExecutor::Vl => {
                let det = r.check.as_ref().map(|c| c.detector.clone()).unwrap_or_default();
                let params = r
                    .check
                    .as_ref()
                    .map(|c| c.params.clone())
                    .unwrap_or(serde_yaml::Value::Null);
                let keys: Vec<String> = match &params {
                    serde_yaml::Value::Mapping(m) => m
                        .keys()
                        .filter_map(|k| k.as_str().map(|s| s.to_string()))
                        .collect(),
                    _ => Vec::new(),
                };
                // C8：vl 通道查注册表 + params 键白名单（原先只看 executor，写错不报错）
                let det_known = svc_pipeline::compliance_vl::vl_lookup(&det).is_some();
                let params_err = svc_pipeline::compliance_vl::vl_judge_params(&params).err();
                detail = serde_json::json!({
                    "detector": det,
                    "detector_checked_by_engine": true,
                    "detector_known": det_known,
                    "known_detectors": svc_pipeline::compliance_vl::vl_known_ids(),
                    "known_params": ["prompt", "expect"],
                    "params_keys_seen": keys,
                    "params_rejected": params_err,
                });
                if !det_known || params_err.is_some() {
                    verdict = "config_error";
                    needs_code.push(format!(
                        "{}: vl 配置错（检测器已知={det_known}，params 错误={params_err:?}）——C8 起装载期报错",
                        r.id
                    ));
                } else {
                    zero_code += 1;
                }
            }
            core_config::RuleExecutor::LocalModel => {
                let det = r.check.as_ref().map(|c| c.detector.clone()).unwrap_or_default();
                let params = r
                    .check
                    .as_ref()
                    .map(|c| c.params.clone())
                    .unwrap_or(serde_yaml::Value::Null);
                match engine_segment::registry::required_model(&det, &params) {
                    Ok(m) => {
                        detail = serde_json::json!({ "detector": det, "model": m, "model_ready": engine_segment::model_ready(&m) });
                        zero_code += 1;
                    }
                    Err(e) => {
                        verdict = "unknown_model_detector";
                        detail = serde_json::json!({ "detector": det, "error": e.to_string() });
                        needs_code.push(format!("{}: 模型检测器 `{}` 未注册", r.id, det));
                    }
                }
            }
        }
        item["configCoverage"] = serde_json::Value::String(verdict.into());
        item["detail"] = detail;
        rules_out.push(item);
    }

    // vl 通道实际组装的 prompt（C8：前言由 pack 的 vl_intro 提供，缺省中性措辞）
    let (judge_asks, tag_asks) = svc_pipeline::compliance_vl::asks_from_pack(&pack);
    let intro = svc_pipeline::compliance_vl::pack_vl_intro(&pack);
    let prompt = svc_pipeline::compliance_vl::build_prompt(&intro, &judge_asks, &tag_asks);

    let steps: Vec<serde_json::Value> = pack
        .pipeline
        .steps
        .iter()
        .map(|s| {
            serde_json::json!({
                "kind": format!("{:?}", s.kind).to_lowercase(),
                "rule": s.rule, "plugin": s.plugin, "dependsOn": s.depends_on,
            })
        })
        .collect();
    let out = serde_json::json!({
        "packDir": dir.to_string_lossy(),
        "schemaDiff": schema_diff(&dir),
        // C8：真实装载路径的 lint（键级 schema + 注册表层）——schemaDiff 是 C7 的探针版，
        // lintIssues 是引擎装载时真会用的那份（同一套判定）
        "lintIssues": svc_pipeline::compliance_lint::lint_dir(&dir)
            .iter().map(|i| i.to_string()).collect::<Vec<_>>(),
        "loadRejected": !svc_pipeline::compliance_lint::lint_dir(&dir).is_empty(),
        "packId": pack.id,
        "rules": pack.rules.len(),
        "steps": steps,
        "report": { "summary": pack.report.summary, "detail_columns": pack.report.detail_columns },
        "taxonomy_fields": pack.taxonomy.fields.keys().collect::<Vec<_>>(),
        "zero_code_rules": zero_code,
        "zero_code_of": pack.rules.len(),
        "needs_code": needs_code,
        "rule_probe": rules_out,
        "vl_prompt_sent": prompt,
        "vl_prompt_mentions_group_by": prompt.contains("group_by"),
        "verdict_table_group_key": serde_json::json!({
            "columns": ["packId","assetId","jobId","ruleId","result","score","evidence","fixAction","fixTier","recheckResult","ts"],
            "assetId_nullable": false,
            "group_key_column": serde_json::Value::Null,
            "note": "rule_verdict.assetId NOT NULL REFERENCES assets(assetId)（migration 0011）→ 无「组级判定」落点",
        }),
        "job_model": {
            "kind": "compliance",
            "targetId": "assetId",
            "note": "compliance_enq 逐 assetId 入队（job_queue.targetId = assetId）→ 一个 job = 一张图",
        },
    });
    dump("c7-load.json", &out);
    println!(
        "C7_RESULT LOADED pack={} rules={} zero_code={}/{} needs_code={}",
        pack.id,
        pack.rules.len(),
        zero_code,
        pack.rules.len(),
        needs_code.len()
    );
    0
}

// ================= 模式 3：真图真编排 =================

fn factory_for(db: PathBuf) -> ConnFactory {
    Arc::new(move || {
        core_storage::open(&db).map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e)))
        })
    })
}

fn run() -> i32 {
    let dir = pack_dir();
    let pack = match core_config::Pack::from_dir(&dir) {
        Ok(p) => p,
        Err(e) => {
            println!("C7_RESULT RUN_FAIL pack={} reason={e}", dir.display());
            return 1;
        }
    };
    let db = PathBuf::from(env_or("C7_DB", "/tmp/c7-work/db/library.db"));
    if !db.is_file() {
        println!("C7_RESULT RUN_FAIL db 不存在: {}", db.display());
        return 1;
    }
    let conn = match core_storage::open(&db) {
        Ok(c) => c,
        Err(e) => {
            println!("C7_RESULT RUN_FAIL db={} reason={e}", db.display());
            return 1;
        }
    };
    let _ = conn.busy_timeout(std::time::Duration::from_secs(20));
    let mut ids: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT assetId FROM assets WHERE deleted = 0 ORDER BY assetId")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    let lim = limit();
    if lim > 0 {
        ids.truncate(lim);
    }
    let out_root = out_dir().join("out").join(&pack.id);
    let _ = std::fs::create_dir_all(&out_root);
    let enqed = match compliance_enq(&conn, &ids, &pack.id, Some(&out_root.to_string_lossy())) {
        Ok(n) => n,
        Err(e) => {
            println!("C7_RESULT RUN_FAIL enq reason={e}");
            return 1;
        }
    };
    let api_key = env_or("C7_VL_API_KEY", "");
    let vl = if api_key.is_empty() {
        None
    } else {
        Some(VlConfig {
            base_url: env_or("C7_VL_BASE_URL", "https://api.deepseek.com/v1"),
            api_key,
            model: env_or("C7_VL_MODEL", "deepseek-flash"),
            timeout_secs: 120,
            max_tokens: 4096,
            proxy: Some(env_or("C7_VL_PROXY", "")),
        })
    };
    // 同 pack 反复跑：判定行按 jobId 代际累积（历史判定保留是产品语义）→
    // 本次运行的判定 = jobId > 运行前基线的那批，断言用这个数，不用库内总数。
    let job_baseline: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(jobId), 0) FROM rule_verdict WHERE packId = ?1",
            [&pack.id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let t = Instant::now();
    let cancel = AtomicBool::new(false);
    let (report, vl_snap) = match run_compliance_queue_par(
        factory_for(db.clone()),
        &pack,
        vl.clone(),
        4,
        &cancel,
        |_p| {},
    ) {
        Ok(v) => v,
        Err(e) => {
            println!("C7_RESULT RUN_FAIL run reason={e}");
            return 1;
        }
    };
    let secs = t.elapsed().as_secs_f64();

    let rows = {
        let mut stmt = conn
            .prepare(
                "SELECT r.assetId, r.ruleId, r.result, r.score, r.evidence, a.fileName
                 FROM rule_verdict r JOIN assets a ON a.assetId = r.assetId
                 WHERE r.packId = ?1 AND r.jobId > ?2 ORDER BY r.assetId, r.ruleId",
            )
            .unwrap();
        stmt.query_map(rusqlite::params![&pack.id, job_baseline], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<f64>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, String>(5)?,
            ))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect::<Vec<_>>()
    };
    let mut by_rule: BTreeMap<String, BTreeMap<String, i64>> = Default::default();
    let mut first_evidence: BTreeMap<String, serde_json::Value> = Default::default();
    let mut csv = String::from("assetId,fileName,ruleId,result,score\n");
    for (aid, rule, result, score, ev, fname) in &rows {
        *by_rule
            .entry(rule.clone())
            .or_default()
            .entry(result.clone())
            .or_default() += 1;
        first_evidence.entry(rule.clone()).or_insert_with(|| {
            serde_json::from_str(ev.as_deref().unwrap_or("null")).unwrap_or(serde_json::Value::Null)
        });
        csv.push_str(&format!(
            "{aid},{fname},{rule},{result},{}\n",
            score.map(|s| s.to_string()).unwrap_or_default()
        ));
    }
    let _ = std::fs::write(out_root.join("verdicts.csv"), &csv);
    let by_rule_json: serde_json::Value = serde_json::json!(by_rule
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::json!(v)))
        .collect::<BTreeMap<_, _>>());
    let out = serde_json::json!({
        "packId": pack.id,
        "db": db.to_string_lossy(),
        "assets": ids.len(),
        "enqueued": enqed,
        "done": report.done,
        "failed": report.failed,
        "retry": report.retry,
        "skipped": report.skipped,
        "errors": report.errors,
        // C8：被隔离的规则级失败（资产仍 done，判定照落；每条 `asset<id> rule<id>: msg`）
        "ruleErrors": report.ruleErrors,
        "verdicts": rows.len(),
        "verdictsTotalInDb": conn
            .query_row("SELECT COUNT(*) FROM rule_verdict WHERE packId = ?1", [&pack.id], |r| r.get::<_, i64>(0))
            .unwrap_or(0),
        "jobIdBaseline": job_baseline,
        "byRule": by_rule_json,
        "firstEvidencePerRule": first_evidence,
        "vl": vl_snap.map(|s| serde_json::json!({
            "calls": s.calls, "http_errors": s.http_errors,
            "parse_failures": s.parse_failures, "retries": s.retries,
        })),
        "vl_configured": vl.is_some(),
        "secs": secs,
        "verdictsCsv": out_root.join("verdicts.csv").to_string_lossy(),
    });
    dump("c7-run.json", &out);
    println!(
        "C7_RESULT RAN pack={} assets={} done={} failed={} verdicts={} secs={:.1} vl_configured={}",
        pack.id,
        ids.len(),
        report.done,
        report.failed,
        rows.len(),
        secs,
        vl.is_some()
    );
    if report.failed > 0 {
        for e in report.errors.iter().take(5) {
            println!("[C7_ERR] {e}");
        }
    }
    // 有失败 job 也返回 0：spike 要的是「如实记录失败模式」，不是「必须全绿」
    0
}

// ================= 模式 4：分组语义 / 组内去重能力探针 =================

fn groups() -> i32 {
    let pack = match core_config::Pack::from_dir(&pack_dir()) {
        Ok(p) => p,
        Err(e) => {
            println!("C7_RESULT GROUP_FAIL reason={e}");
            return 1;
        }
    };
    let db = PathBuf::from(env_or("C7_DB", "/tmp/c7-work/db/library.db"));
    let conn = match core_storage::open(&db) {
        Ok(c) => c,
        Err(e) => {
            println!("C7_RESULT GROUP_FAIL db={} reason={e}", db.display());
            return 1;
        }
    };
    // 资产（含 pHash / fileHash）——「房源号」在真库里没有这个字段，用文件名前缀当组键
    let assets: Vec<(i64, String, Option<String>, Option<String>)> = {
        let mut stmt = conn
            .prepare(
                "SELECT a.assetId, a.fileName, a.fileHash, r.phash
                 FROM assets a LEFT JOIN resource r ON r.assetId = a.assetId
                 WHERE a.deleted = 0 ORDER BY a.assetId",
            )
            .unwrap();
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
    };
    // 判定行过滤的 packId：默认 = 本 pack；变体包（realestate-no-f3）跑出的判定可用
    // C7_PACK_ID 指过来（同一套规则、同一套落库路径，只是少了缺检测器的那条）
    let pack_id_filter = env_or("C7_PACK_ID", &pack.id);
    let verdicts: Vec<(i64, String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT r.assetId, r.ruleId, COALESCE(r.verdict, CASE r.result WHEN 'skipped' THEN 'skipped' ELSE 'undetermined' END) FROM rule_verdict r
                 WHERE r.packId = ?1 AND r.jobId = (
                   SELECT MAX(x.jobId) FROM rule_verdict x
                   WHERE x.packId = r.packId AND x.assetId = r.assetId AND x.ruleId = r.ruleId)",
            )
            .unwrap();
        stmt.query_map([&pack_id_filter], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    let mut vmap: BTreeMap<(i64, String), String> = Default::default();
    for (a, r, s) in &verdicts {
        vmap.insert((*a, r.clone()), s.clone());
    }
    // 组键：文件名前缀（realestate draft 里的「房源号」占位）
    let group_key = |name: &str| -> String {
        let stem = name.rsplit_once('.').map(|(a, _)| a).unwrap_or(name);
        match stem.split_once(['-', '_']) {
            Some((pre, _)) if !pre.is_empty() => pre.to_string(),
            _ => stem.to_string(),
        }
    };
    let mut by_group: BTreeMap<String, Vec<i64>> = Default::default();
    for (aid, name, _, _) in &assets {
        by_group.entry(group_key(name)).or_default().push(*aid);
    }
    let rule_ids: Vec<String> = pack.rules.iter().map(|r| r.id.clone()).collect();
    let mut group_rows = Vec::new();
    let mut dup_groups_total = 0usize;
    for (g, ids) in &by_group {
        let mut per_rule: BTreeMap<String, BTreeMap<String, i64>> = Default::default();
        for id in ids {
            for rid in &rule_ids {
                let res = vmap
                    .get(&(*id, rid.clone()))
                    .cloned()
                    .unwrap_or_else(|| "<无判定行>".into());
                *per_rule.entry(rid.clone()).or_default().entry(res).or_default() += 1;
            }
        }
        // 组内 pHash 近似重复（L1 engine-dedup 能力直调）
        let members: Vec<engine_dedup::DupMember> = ids
            .iter()
            .map(|id| {
                let a = assets.iter().find(|(aid, _, _, _)| aid == id).unwrap();
                engine_dedup::DupMember {
                    asset_id: *id,
                    file_hash: a.2.clone(),
                    phash: a.3.as_deref().and_then(|h| u64::from_str_radix(h, 16).ok()),
                }
            })
            .collect();
        let dg = engine_dedup::build_dup_groups(&members, engine_dedup::PHASH_THRESHOLD);
        dup_groups_total += dg.len();
        group_rows.push(serde_json::json!({
            "group": g,
            "images": ids.len(),
            "perRule": per_rule,
            "phashDupGroupsInGroup": dg.len(),
            "phashDupKinds": dg.iter().map(|x| x.kind.as_str()).collect::<Vec<_>>(),
        }));
    }
    let out = serde_json::json!({
        "packId": pack.id,
        "verdictPackIdFilter": pack_id_filter,
        "groupKeySource": "fileName 前缀（真库里没有「房源号」字段；pack 的 params.group_by 只是一段字符串）",
        "groups": group_rows.len(),
        "groupRows": group_rows,
        "verdictRows": verdicts.len(),
        "verdictRowsSemantics": "每个 (资产,规则) 取最新一次判定（同 pack 多代际跑时不重复计）",
        "verdictRowsWithGroupKey": 0,
        "engineGroupSupport": {
            "rule_verdict_group_column": false,
            "job_target_is_asset": true,
            "pack_schema_group_by": false,
            "vl_prompt_includes_group_by": false,
        },
        "phashDupGroupsInCorpus": dup_groups_total,
        "phashDetectorInPackRegistry": engine_classic::registry::lookup("phash_dup").is_some(),
        "classicDetectorIdsKnown": engine_classic::registry::known_ids(),
        "dedupCapabilityAtL1": {
            "crate": "engine-dedup",
            "api": "build_dup_groups(DupMember[], PHASH_THRESHOLD=8)",
            "reachable_from_pack": false,
        },
    });
    dump("c7-groups.json", &out);
    println!(
        "C7_RESULT GROUPED groups={} verdicts={} phash_dup_groups={} phash_dup_detector_registered={}",
        group_rows.len(),
        verdicts.len(),
        dup_groups_total,
        engine_classic::registry::lookup("phash_dup").is_some()
    );
    0
}

// ================= 模式 5：pack 文件 schema 对账 =================

/// 逐文件把 YAML 里写的键与 C0 定稿 schema 的键对齐。**未被读的键 = 被静默吞掉的键**
/// （serde 默认不 deny_unknown_fields）——这是「换行业=换文件」承诺的隐形陷阱：
/// 作者以为写了、引擎根本没看。返回 {文件: {位置: [多出来的键]}}。
fn schema_diff(dir: &Path) -> serde_json::Value {
    const RULE_KEYS: [&str; 8] =
        ["id", "name", "executor", "evidence", "check", "outputs", "fixer", "on_fail"];
    const CHECK_KEYS: [&str; 2] = ["detector", "params"];
    const FIXER_KEYS: [&str; 3] = ["type", "tier", "params"];
    const PIPE_KEYS: [&str; 2] = ["steps", "recheck_max"];
    const STEP_KEYS: [&str; 6] = ["kind", "id", "rule", "plugin", "params", "depends_on"];
    const REPORT_KEYS: [&str; 2] = ["summary", "detail_columns"];

    let read = |f: &str| -> Option<serde_yaml::Value> {
        std::fs::read_to_string(dir.join(f))
            .ok()
            .and_then(|t| serde_yaml::from_str(&t).ok())
    };
    let map_keys = |v: &serde_yaml::Value| -> Vec<String> {
        match v {
            serde_yaml::Value::Mapping(m) => m
                .keys()
                .filter_map(|k| k.as_str().map(|s| s.to_string()))
                .collect(),
            _ => Vec::new(),
        }
    };
    let extra = |seen: &[String], known: &[&str]| -> Vec<String> {
        seen.iter().filter(|k| !known.contains(&k.as_str())).cloned().collect()
    };

    let mut out: BTreeMap<String, serde_json::Value> = Default::default();
    // rules.yaml：逐规则 + 逐 check/fixer
    if let Some(serde_yaml::Value::Sequence(rules)) = read("rules.yaml") {
        let mut per_rule = Vec::new();
        let mut top_extra: Vec<String> = Vec::new();
        for r in &rules {
            let keys = map_keys(r);
            let e = extra(&keys, &RULE_KEYS);
            if !e.is_empty() {
                top_extra.extend(e.clone());
            }
            let mut sub = serde_json::json!({ "unknownRuleKeys": e });
            if let Some(chk) = r.get("check") {
                sub["unknownCheckKeys"] = serde_json::json!(extra(&map_keys(chk), &CHECK_KEYS));
            }
            if let Some(fx) = r.get("fixer") {
                if !fx.is_null() {
                    sub["unknownFixerKeys"] = serde_json::json!(extra(&map_keys(fx), &FIXER_KEYS));
                }
            }
            per_rule.push(sub);
        }
        out.insert(
            "rules.yaml".into(),
            serde_json::json!({ "unknownRuleKeys": top_extra, "perRule": per_rule }),
        );
    }
    // pipeline.yaml
    if let Some(p) = read("pipeline.yaml") {
        let mut step_extra: Vec<String> = Vec::new();
        if let Some(serde_yaml::Value::Sequence(steps)) = p.get("steps") {
            for s in steps {
                step_extra.extend(extra(&map_keys(s), &STEP_KEYS));
            }
        }
        out.insert(
            "pipeline.yaml".into(),
            serde_json::json!({
                "unknownTopKeys": extra(&map_keys(&p), &PIPE_KEYS),
                "unknownStepKeys": step_extra,
            }),
        );
    }
    // report.yaml
    if let Some(r) = read("report.yaml") {
        out.insert(
            "report.yaml".into(),
            serde_json::json!({ "unknownTopKeys": extra(&map_keys(&r), &REPORT_KEYS) }),
        );
    }
    serde_json::json!(out)
}

// ================= 模式 6：schema 探针夹具（故意写错，看引擎响不响） =================

fn probes() -> i32 {
    let dir = PathBuf::from(env_or("C7_PROBE_DIR", "/workspace/scripts/c7-fixtures/schema-probes"));
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|d| {
            d.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    entries.sort();
    let mut rows = Vec::new();
    for d in &entries {
        let name = d.file_name().unwrap().to_string_lossy().to_string();
        let mut row = serde_json::json!({ "probe": name });
        // C8：装载前 lint（键级 schema + 注册表层）= 装载/导入/激活同一道门
        let issues = svc_pipeline::compliance_lint::lint_dir(d);
        row["lintIssues"] = serde_json::json!(issues.iter().map(|i| i.to_string()).collect::<Vec<_>>());
        row["gated"] = serde_json::json!(if issues.is_empty() { "accepted" } else { "rejected" });
        let loaded = core_config::Pack::from_dir(d);
        match loaded {
            Err(e) => {
                row["load"] = serde_json::json!("rejected");
                row["loadError"] = serde_json::json!(e.to_string());
            }
            Ok(pack) => {
                row["load"] = serde_json::json!("accepted");
                let mut per_rule = Vec::new();
                for r in &pack.rules {
                    let mut it = serde_json::json!({ "ruleId": r.id });
                    if let Some(chk) = r.check.as_ref() {
                        match r.executor {
                            core_config::RuleExecutor::Classic => {
                                it["detector"] = serde_json::json!(chk.detector);
                                let known = engine_classic::registry::lookup(&chk.detector);
                                it["detectorKnown"] = serde_json::json!(known.is_some());
                                it["params"] = serde_json::json!(format!("{:?}", chk.params));
                                // 真跑一次：参数写错的检测器在这一步响亮报错（deny_unknown_fields）
                                if let Some(entry) = known {
                                    let input = match entry.input {
                                        engine_classic::registry::InputKind::Metadata => {
                                            engine_classic::registry::DetectorInput::Metadata {
                                                width: 1600,
                                                height: 1200,
                                            }
                                        }
                                        engine_classic::registry::InputKind::Rgb => synth_rgb(),
                                    };
                                    match engine_classic::registry::run(
                                        &chk.detector,
                                        &input,
                                        &chk.params,
                                    ) {
                                        Ok(o) => it["probeRun"] = serde_json::json!(format!(
                                            "ok score={} pass={}",
                                            o.score, o.pass
                                        )),
                                        Err(e) => {
                                            it["probeRun"] = serde_json::json!(format!("ERR {e}"))
                                        }
                                    }
                                }
                            }
                            core_config::RuleExecutor::Vl => {
                                let keys = map_keys_of(&chk.params);
                                it["paramsKeys"] = serde_json::json!(keys);
                                it["vlDetectorKnown"] = serde_json::json!(
                                    svc_pipeline::compliance_vl::vl_lookup(&chk.detector).is_some()
                                );
                                // C8：vl params 走 deny_unknown_fields → 未知键 = 参数错误（不再是静默忽略）
                                it["paramsRejected"] = serde_json::json!(
                                    svc_pipeline::compliance_vl::vl_judge_params(&chk.params).err()
                                );
                            }
                            core_config::RuleExecutor::LocalModel => {
                                it["detector"] = serde_json::json!(chk.detector);
                            }
                        }
                    }
                    per_rule.push(it);
                }
                row["rules"] = serde_json::json!(per_rule);
                row["schemaDiff"] = schema_diff(d);
            }
        }
        rows.push(row);
    }
    let out = serde_json::json!({ "probeDir": dir.to_string_lossy(), "probes": rows });
    dump("c7-schema-probes.json", &out);
    println!("C7_RESULT PROBES n={}", rows.len());
    0
}

fn map_keys_of(v: &serde_yaml::Value) -> Vec<String> {
    match v {
        serde_yaml::Value::Mapping(m) => m
            .keys()
            .filter_map(|k| k.as_str().map(|s| s.to_string()))
            .collect(),
        _ => Vec::new(),
    }
}

/// 合成探针图（64×64 棋盘 + 水平渐变；非平凡，能真跑通拉普拉斯/直方图/白边路径）。
fn synth_rgb() -> engine_classic::registry::DetectorInput {
    let (w, h) = (64u32, 64u32);
    let mut px = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let v = if (x / 8 + y / 8) % 2 == 0 { 40u8 } else { 220u8 };
            let g = ((x * 4) as u8).wrapping_add(v / 2);
            px.extend_from_slice(&[g, v, 255 - v]);
        }
    }
    engine_classic::registry::DetectorInput::Rgb {
        width: w,
        height: h,
        pixels: px,
    }
}

// ================= 模式 7：重复检测能力探针（pHash 能力 vs pack 可达性） =================

/// 在夹具目录上量「重复检测」的**真实能力**：SHA-256 精确重复 ∪ pHash(≤8) 近似重复，
/// 用 L1 engine-dedup 的同一套判定（svc-library 分组也是它）。
/// 目的：把「同房间去重需新插件」拆成两句可验证的话——
///   ① 视觉重复检测能力**已经存在**（夹具里给出真实汉明距离）；
///   ② 但 pack 够不着（没有 phash_dup 检测器键），而且 pHash 抓不住「同房间不同角度」
///      这类**语义重复**（夹具里给出跨图距离 > 阈值）。
fn dedup_probe() -> i32 {
    let dir = PathBuf::from(env_or(
        "C7_DUP_DIR",
        "/workspace/reports/logs/c7/dup-fixture",
    ));
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("读夹具目录 {dir:?}: {e}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase()).as_deref(),
                Some("jpg") | Some("jpeg") | Some("png")
            )
        })
        .collect();
    files.sort();
    let mut names = Vec::new();
    let mut members = Vec::new();
    let mut phashes: Vec<u64> = Vec::new();
    for (i, f) in files.iter().enumerate() {
        let bytes = std::fs::read(f).unwrap_or_default();
        let sha = core_hash::sha256_hex(&bytes);
        let ph = core_media::luma32(f).ok().and_then(|l| core_hash::phash_hex(&l).ok());
        names.push(f.file_name().unwrap().to_string_lossy().to_string());
        members.push(engine_dedup::DupMember {
            asset_id: i as i64,
            file_hash: Some(sha),
            phash: ph.as_deref().and_then(|h| u64::from_str_radix(h, 16).ok()),
        });
        phashes.push(
            ph.as_deref()
                .and_then(|h| u64::from_str_radix(h, 16).ok())
                .unwrap_or(0),
        );
    }
    let groups = engine_dedup::build_dup_groups(&members, engine_dedup::PHASH_THRESHOLD);
    let mut pairs = Vec::new();
    for i in 0..names.len() {
        for j in (i + 1)..names.len() {
            pairs.push(serde_json::json!({
                "a": names[i], "b": names[j],
                "hamming": core_hash::hamming(phashes[i], phashes[j]),
                "same_sha": members[i].file_hash == members[j].file_hash,
            }));
        }
    }
    let out = serde_json::json!({
        "fixtureDir": dir.to_string_lossy(),
        "files": names,
        "phash_hex": phashes.iter().map(|p| format!("{p:016x}")).collect::<Vec<_>>(),
        "threshold": engine_dedup::PHASH_THRESHOLD,
        "dupGroups": groups.iter().map(|g| serde_json::json!({
            "kind": g.kind.as_str(),
            "members": g.members.iter().map(|&i| names[i].clone()).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "pairwiseHamming": pairs,
        "reachableFromPack": {
            "phash_dup_detector_registered": engine_classic::registry::lookup("phash_dup").is_some(),
            "classicDetectorIdsKnown": engine_classic::registry::known_ids(),
            "conclusion": "视觉重复检测能力在 L1 已存在（本夹具实测出组），但 classic 注册表没有 phash_dup 键 → 纯 pack 文件表达不出来；语义级「同房间不同角度」超出 pHash 能力，需要新插件",
        },
    });
    dump("c7-dedup-probe.json", &out);
    println!(
        "C7_RESULT DEDUP_PROBE files={} dup_groups={} group_shapes={:?}",
        names.len(),
        groups.len(),
        groups.iter().map(|g| g.kind.as_str()).collect::<Vec<_>>()
    );
    0
}
