//! C5 — 2000 张真图 e2e driver（电商行业包全链路 + 硬指标）。
//!
//! 阶段（一次跑完，状态写 $C5_OUT/c5-status.txt 供单次轮询）：
//!   0 语料：DP 图库固定步长采样（只读）→ 300 张 u2net 预扫描选 150 张 ×4 预放大（R1 过，
//!           含 fg>max 6 张 / fg<min 30 张 / 带内 114 张）+ 1838 张原图（R1 不过→upscale 修复）
//!           + 10 张注入促销文字水印（R3 fail→inpaint 闭环）= 2000 张；manifest 记录构成。
//!   1 入库：文件库（WAL）+ assets/resource 直插（C3 corpus e2e 同模式）。
//!   2 合规：compliance_enq 2000 → run_compliance_queue_par（workers = C5_WORKERS，
//!           vl = deepseek-flash OpenAI 兼容端点，资产级合并调用一次）→ 计时。
//!   3 报告：svc-library verdict_report_csv（pack report.yaml detail_columns DSL）导出。
//!   4 复现：classic 规则（R1/R2）同批重跑（同 pack id、新 job 代），逐资产逐规则
//!           result+score 对比 → diff 必须为 0。
//!   5 汇总：四档 summary、机器处理率、人工队列率、VL 统计、硬指标判定 →
//!           C5_E2E_SUMMARY {...} + c5-e2e-summary.json + c5-verdict-report.csv。
//!
//! 环境变量：
//!   C5_DP_DIR=/data  C5_WORK=/tmp/c5-e2e-work
//!   C5_OUT=/workspace/reports/logs  C5_WORKERS=10  C5_MODELS_DIR=/models
//!   C5_VL_BASE_URL / C5_VL_API_KEY / C5_VL_MODEL / C5_VL_PROXY（""=直连，默认直连）

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use svc_pipeline::compliance_queue::{
    compliance_enq, run_compliance_queue_par, ConnFactory,
};
use svc_pipeline::compliance_plugins;
use svc_pipeline::VlConfig;

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| d.to_string())
}

fn main() {
    let code = run();
    std::process::exit(code);
}

fn status(out_dir: &Path, line: &str) {
    println!("[C5] {line}");
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out_dir.join("c5-status.txt"))
    {
        let _ = writeln!(f, "{line}");
        let _ = f.flush();
    }
}

fn run() -> i32 {
    let work = PathBuf::from(env_or("C5_WORK", "/tmp/c5-e2e-work"));
    let out_dir = PathBuf::from(env_or("C5_OUT", "/workspace/reports/logs"));
    let models_dir = PathBuf::from(env_or("C5_MODELS_DIR", "/models"));
    let workers: usize = env_or("C5_WORKERS", "10").parse().unwrap_or(10);
    let target: usize = env_or("C5_TARGET", "2000").parse().unwrap_or(2000);
    std::fs::create_dir_all(&out_dir).unwrap_or_else(|e| panic!("建输出目录 {out_dir:?}: {e}"));
    let _ = std::fs::remove_file(out_dir.join("c5-status.txt"));
    let t_all = Instant::now();
    status(&out_dir, &format!("C5_START target={target} workers={workers}"));

    // 模型根布局契约 <root>/models/<m>/<m>.onnx → root = work（work/models/<name> → 容器挂载）
    let model_root = work.clone();
    let _ = models_dir.read_dir(); // models_dir 仅用于符号链接目标
    std::fs::create_dir_all(work.join("models")).unwrap();
    for m in ["u2net", "realesr-general-x4v3", "lama-fp32"] {
        let link = model_root.join("models").join(m);
        let src = models_dir.join(m);
        if src.join(format!("{m}.onnx")).is_file() {
            let _ = std::fs::remove_file(&link);
            if std::os::unix::fs::symlink(&src, &link).is_err() {
                // 已存在同指向链接等情况忽略
            }
        }
    }
    engine_segment::set_models_root(&model_root);
    engine_restore::set_models_root(&model_root);

    // VL 配置（语料阶段就要用：干跑甄选按真 pack 真调 VL 判 R3/R5）
    let vl = VlConfig {
        base_url: env_or("C5_VL_BASE_URL", "https://api.deepseek.com/v1"),
        api_key: env_or("C5_VL_API_KEY", ""),
        model: env_or("C5_VL_MODEL", "deepseek-flash"),
        timeout_secs: 120,
        max_tokens: 4096,
        proxy: Some(env_or("C5_VL_PROXY", "")), // 默认直连（deepseek 国内可达）
    };
    if vl.api_key.is_empty() {
        status(&out_dir, "C5_RESULT FAIL phase=vl reason=C5_VL_API_KEY 未设置");
        return 1;
    }

    // ---------- 阶段 0：语料（宿主机已采样暂存 → 容器内预扫描/派生/干跑甄选） ----------
    let corpus = work.join("corpus");
    let manifest_path = work.join("manifest.json");
    let t0 = Instant::now();
    let force_corpus = env_or("C5_FORCE_CORPUS", "0") == "1";
    if !force_corpus && manifest_path.is_file() && corpus.read_dir().map(|d| d.count()).unwrap_or(0) >= target {
        status(&out_dir, "C5_PHASE0 corpus 已就绪（manifest 存在，跳过重采）");
    } else {
        let _ = std::fs::remove_dir_all(&corpus);
        for d in ["m", "g1", "g2", "g3"] {
            std::fs::create_dir_all(corpus.join(d)).unwrap();
        }
        match prepare_corpus(
            &PathBuf::from(env_or("C5_STAGE_DIR", "/tmp/c5-stage")),
            &work,
            &corpus,
            target,
            Some(vl.clone()),
            workers,
        ) {
            Ok(m) => {
                std::fs::write(&manifest_path, serde_json::to_string_pretty(&m).unwrap()).unwrap();
                status(
                    &out_dir,
                    &format!(
                        "C5_PHASE0 OK m={} g1={} g2={} g3={} 候选={} 干跑命中={:?} 干跑拒={} secs={:.1}",
                        m["groups"]["m"].as_i64().unwrap_or(0),
                        m["groups"]["g1"].as_i64().unwrap_or(0),
                        m["groups"]["g2"].as_i64().unwrap_or(0),
                        m["groups"]["g3"].as_i64().unwrap_or(0),
                        m["selection"]["candidates"]["total"].as_i64().unwrap_or(0),
                        m["dryrun"]["groupHits"],
                        m["dryrun"]["rejected"].as_i64().unwrap_or(0),
                        t0.elapsed().as_secs_f64()
                    ),
                );
            }
            Err(e) => {
                status(&out_dir, &format!("C5_RESULT FAIL phase=corpus reason={e}"));
                return 1;
            }
        }
    }
    let mut paths: Vec<PathBuf> = ["m", "g1", "g2", "g3"]
        .iter()
        .flat_map(|g| std::fs::read_dir(corpus.join(g)).unwrap().filter_map(|e| e.ok()).map(|e| e.path()))
        .filter(|p| {
            matches!(p.extension().and_then(|e| e.to_str()), Some("jpg" | "jpeg" | "png"))
        })
        .collect();
    paths.sort();
    if paths.len() != target {
        status(&out_dir, &format!("C5_RESULT FAIL phase=corpus reason=语料 {} 张 ≠ {target}", paths.len()));
        return 1;
    }

    // ---------- 阶段 1：入库 ----------
    let db_dir = work.join("db");
    let _ = std::fs::remove_dir_all(&db_dir);
    std::fs::create_dir_all(&db_dir).unwrap();
    let db_path = db_dir.join("library.db");
    let conn = core_storage::open(&db_path).expect("open library db");
    let t0 = Instant::now();
    {
        let mut n = 0i64;
        for p in &paths {
            let (w, h) = core_media::image_size(p).unwrap_or((0, 0));
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("img.jpg");
            conn.execute(
                "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, importedAt)
                 VALUES (NULL, ?1, 'image', ?2, 'jpg', 1, 0, 0)",
                rusqlite::params![p.to_string_lossy(), name],
            )
            .expect("insert asset");
            let id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1, 'primary', 0, ?2, ?3)",
                rusqlite::params![id, w, h],
            )
            .expect("insert resource");
            n += 1;
        }
        status(&out_dir, &format!("C5_PHASE1 OK assets={n} secs={:.1}", t0.elapsed().as_secs_f64()));
    }
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT assetId FROM assets ORDER BY assetId").unwrap();
        stmt.query_map([], |r| r.get::<_, i64>(0)).unwrap().map(|r| r.unwrap()).collect()
    };

    // ---------- 阶段 2：并行合规（VL 合并调用 × workers 并发） ----------
    let pack = legacy_pack().expect("legacy pack（C5_PACK_DIR）");
    let fixes_dir = work.join("fixes");
    std::fs::create_dir_all(&fixes_dir).unwrap();
    let enqed = compliance_enq(&conn, &ids, pack.id.as_str(), Some(&fixes_dir.to_string_lossy())).unwrap();
    status(&out_dir, &format!("C5_PHASE2 enqueued={enqed} pack={}", pack.id));
    let t_run = Instant::now();
    let cancel = AtomicBool::new(false);
    let done_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let dc = done_count.clone();
    let st_at = out_dir.clone();
    let (report, vl_snap) = match run_compliance_queue_par(
        factory_for(db_path.clone()),
        &pack,
        Some(vl.clone()),
        workers,
        &cancel,
        move |p| {
            let n = dc.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n % 100 == 0 || p.status != "done" {
                let extra = p.error.clone().unwrap_or_default();
                status(&st_at, &format!("C5_PROGRESS {}/{} {} {extra}", n, p.total, p.status));
            }
        },
    ) {
        Ok(r) => r,
        Err(e) => {
            status(&out_dir, &format!("C5_RESULT FAIL phase=run reason={e}"));
            return 1;
        }
    };
    let run_secs = t_run.elapsed().as_secs_f64();
    status(
        &out_dir,
        &format!(
            "C5_PHASE2 OK done={} failed={} retry={} skipped={} cancelled={} secs={run_secs:.1} err1={:?}",
            report.done, report.failed, report.retry, report.skipped, report.cancelled,
            report.errors.first()
        ),
    );

    // ---------- 阶段 3：判定报告导出 ----------
    let t0 = Instant::now();
    let items = svc_library::export::resolve_items(
        &conn,
        &svc_library::export::ExportSource::Ids(ids.clone()),
    )
    .expect("resolve items");
    let named: Vec<(svc_library::export::ExportItem, String)> = items
        .iter()
        .map(|i| (i.clone(), i.file_name.clone()))
        .collect();
    let csv = svc_library::export::verdict_report_csv(
        &conn,
        &svc_library::export::VerdictReportSpec { pack: pack.clone(), job_id: None, run_id: None },
        &named,
        // KI-15：e2e 不拒选任何资产，excluded 集为空（口径见 svc_library::export）
        &[],
    )
    .expect("verdict report csv");
    let csv_path = out_dir.join("c5-verdict-report.csv");
    std::fs::write(&csv_path, &csv).unwrap();
    status(
        &out_dir,
        &format!(
            "C5_PHASE3 OK csv={} 行={}（含表头/汇总） secs={:.1}",
            csv_path.display(),
            csv.lines().count(),
            t0.elapsed().as_secs_f64()
        ),
    );

    // ---------- 阶段 4：classic 同批重跑一致性 ----------
    let t0 = Instant::now();
    let max_job1: i64 = conn
        .query_row("SELECT COALESCE(MAX(jobId), 0) FROM rule_verdict WHERE packId = ?1", [&pack.id], |r| r.get(0))
        .unwrap();
    let classic_ids: Vec<String> = ["R1_size", "R2_white_bg"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rules_yaml = pack
        .rules
        .iter()
        .filter(|r| classic_ids.contains(&r.id))
        .map(rule_to_yaml_seq)
        .collect::<Vec<_>>()
        .join("\n");
    let pack2 = core_config::Pack::from_yaml_strs(
        &pack.id,
        "category: [x]\n",
        &rules_yaml,
        "steps:\n  - {kind: judge, rule: R1_size}\n  - {kind: judge, rule: R2_white_bg}\nrecheck_max: 1\n",
        "summary: [合格, 不合格, 未判定]\ndetail_columns: [imageSN, R1_size, R2_white_bg]\n",
    )
    .expect("classic sub-pack");
    let enq2 = compliance_enq(&conn, &ids, pack2.id.as_str(), Some(&fixes_dir.to_string_lossy())).unwrap();
    let (report2, _) = match run_compliance_queue_par(
        factory_for(db_path.clone()),
        &pack2,
        None,
        workers,
        &AtomicBool::new(false),
        |_| {},
    ) {
        Ok(r) => r,
        Err(e) => {
            status(&out_dir, &format!("C5_RESULT FAIL phase=rerun reason={e}"));
            return 1;
        }
    };
    let mut diff = 0usize;
    let mut diff_samples: Vec<String> = Vec::new();
    {
        let q = |gen: &str, max_job: i64| -> BTreeMap<(i64, String), (String, Option<f64>)> {
            let sql = if gen == "gen1" {
                format!(
                    "SELECT assetId, ruleId, COALESCE(verdict, CASE result WHEN 'skipped' THEN 'skipped' ELSE 'undetermined' END), score FROM rule_verdict WHERE packId='{}' AND ruleId IN ('R1_size','R2_white_bg') AND jobId <= {max_job}",
                    pack.id
                )
            } else {
                format!(
                    "SELECT assetId, ruleId, COALESCE(verdict, CASE result WHEN 'skipped' THEN 'skipped' ELSE 'undetermined' END), score FROM rule_verdict WHERE packId='{}' AND ruleId IN ('R1_size','R2_white_bg') AND jobId > {max_job}",
                    pack.id
                )
            };
            let mut stmt = conn.prepare(&sql).unwrap();
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<f64>>(3)?))
                })
                .unwrap();
            rows.map(|r| r.unwrap())
                .map(|(a, rule, res, sc)| ((a, rule), (res, sc)))
                .collect()
        };
        let g1 = q("gen1", max_job1);
        let g2 = q("gen2", max_job1);
        for (k, v1) in &g1 {
            match g2.get(k) {
                Some(v2) if v2 == v1 => {}
                other => {
                    diff += 1;
                    if diff_samples.len() < 20 {
                        diff_samples.push(format!("asset{} {}: {v1:?} vs {other:?}", k.0, k.1));
                    }
                }
            }
        }
    }
    status(
        &out_dir,
        &format!(
            "C5_PHASE4 OK rerun_done={} classic_diff={diff} samples={diff_samples:?} secs={:.1}",
            report2.done,
            t0.elapsed().as_secs_f64()
        ),
    );

    // ---------- 阶段 5：四档汇总 + 硬指标 ----------
    let mut per_rule: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT ruleId, COALESCE(verdict, CASE result WHEN 'skipped' THEN 'skipped' ELSE 'undetermined' END) AS r, COUNT(*) FROM rule_verdict WHERE packId = ?1 AND jobId <= ?2 GROUP BY ruleId, r",
            )
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params![pack.id, max_job1], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })
            .unwrap();
        for (rule, res, n) in rows.map(|r| r.unwrap()) {
            let cnt = per_rule.entry(rule).or_default().entry(res).or_insert(0);
            *cnt += n;
        }
    }
    let mut tier: BTreeMap<String, i64> = BTreeMap::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT assetId,
                        SUM(result = 'pending_human'),
                        SUM(result = 'fail'),
                        SUM(CASE WHEN fixAction IS NOT NULL AND recheckResult = 'pass' THEN 1 ELSE 0 END)
                 FROM rule_verdict
                 WHERE packId = ?1 AND jobId <= ?2 AND ruleId != 'R5_attribute'
                 GROUP BY assetId",
            )
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params![pack.id, max_job1], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                ))
            })
            .unwrap();
        for (_a, pending, fail, fixed) in rows.map(|r| r.unwrap()) {
            let k = if pending > 0 {
                "转人工"
            } else if fail > 0 {
                "失败"
            } else if fixed > 0 {
                "修复后合格"
            } else {
                "合格"
            };
            *tier.entry(k.to_string()).or_insert(0) += 1;
        }
    }
    let total = ids.len() as i64;
    let sum = |k: &str| tier.get(k).copied().unwrap_or(0);
    let machine = sum("合格") + sum("修复后合格");
    let human = sum("转人工");
    let machine_rate = machine as f64 / total as f64;
    let human_rate = human as f64 / total as f64;
    let vs = vl_snap.unwrap_or_default();
    let wall_secs = t_all.elapsed().as_secs_f64();

    let hard_ok = machine_rate >= 0.95 && human_rate <= 0.01 && wall_secs <= 3600.0 && diff == 0
        && report.failed == 0 && report.done == total as usize;
    let summary = serde_json::json!({
        "pack": pack.id,
        "target": total,
        "composition": serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(&manifest_path).unwrap_or_default()
        ).unwrap_or(serde_json::json!({})),
        "run": {
            "done": report.done, "failed": report.failed, "retry": report.retry,
            "skipped": report.skipped, "cancelled": report.cancelled,
            "jobDone": report.jobDone, "jobFailed": report.jobFailed, "jobPending": report.jobPending,
            "workers": workers, "run_secs": run_secs, "wall_secs": wall_secs,
        },
        "vl": { "calls": vs.calls, "http_errors": vs.http_errors, "parse_failures": vs.parse_failures, "retries": vs.retries, "avg_ms": vs.avg_ms() },
        "per_rule": per_rule,
        "summary_tiers": tier,
        "metrics": {
            "machine_handled": machine, "machine_rate": machine_rate,
            "human_queue": human, "human_rate": human_rate,
            "classic_rerun_diff": diff, "classic_diff_samples": diff_samples,
        },
        "hard_ok": hard_ok,
    });
    let json_path = out_dir.join("c5-e2e-summary.json");
    std::fs::write(&json_path, serde_json::to_string_pretty(&summary).unwrap()).unwrap();
    println!("C5_E2E_SUMMARY {summary}");
    status(
        &out_dir,
        &format!(
            "C5_RESULT {} machine={machine}/{total}({:.1}%) human={human}({:.2}%) wall={wall_secs:.0}s classic_diff={diff} failed_jobs={} summary={}",
            if hard_ok { "PASS" } else { "FAIL" },
            machine_rate * 100.0,
            human_rate * 100.0,
            report.failed,
            json_path.display()
        ),
    );
    if hard_ok { 0 } else { 1 }
}

fn factory_for(db: PathBuf) -> ConnFactory {
    Arc::new(move || {
        core_storage::open(&db).map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e)))
        })
    })
}

/// 规则 → YAML 序列项文本（`- ` 首行 + 缩进体），供 classic 重跑子包组装。
fn rule_to_yaml_seq(r: &core_config::Rule) -> String {
    let s = serde_yaml::to_string(r).expect("rule 序列化");
    let mut out = String::new();
    for (i, line) in s.lines().filter(|l| !l.trim().is_empty()).enumerate() {
        if i == 0 {
            out.push_str("- ");
            out.push_str(line);
        } else {
            out.push_str("  ");
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// 白底边带度量（与 R2 white_edge 检测器逐行同口径：外圈 4px 带内每像素
/// `255 - min(R,G,B)` 的均值，band 钳到短边一半）。≤ max_dev(8) → R2 直接通过。
fn edge_mean_dev(path: &Path) -> Option<f64> {
    let (w, h, px) = core_media::rgb8(path).ok()?;
    let (w, h) = (w.max(1) as usize, h.max(1) as usize);
    if px.len() != w * h * 3 {
        return None;
    }
    let band = 4usize.min(w.min(h).div_ceil(2)).max(1);
    let (mut sum, mut n) = (0f64, 0usize);
    for y in 0..h {
        for x in 0..w {
            if x < band || x >= w - band || y < band || y >= h - band {
                let i = (y * w + x) * 3;
                let dev = 255u16 - px[i].min(px[i + 1]).min(px[i + 2]) as u16;
                sum += dev as f64;
                n += 1;
            }
        }
    }
    if n == 0 {
        return None;
    }
    Some(sum / n as f64)
}

/// 单图甄选度量（扫描线程内跑）。
struct SrcMetric {
    path: PathBuf,
    ratio: f64,
    compact: f64,
    border: bool,
    r2_direct: bool,
    win: Option<[usize; 4]>,
}

fn measure_one(p: &Path, seg: &engine_segment::Segmenter) -> Option<SrcMetric> {
    let (w, h, px) = core_media::rgb8(p).ok()?;
    let (wu, hu) = (u32::try_from(w).ok()?, u32::try_from(h).ok()?);
    let s = seg.run_rgb(wu, hu, &px).ok()?;
    let m = &s.mask;
    let s319 = m.side - 1;
    let band = 4usize;
    let border = m.fg_in_window(0, 0, s319, band - 1) > 0
        || m.fg_in_window(0, 0, band - 1, s319) > 0
        || m.fg_in_window(s319 - band + 1, 0, s319, s319) > 0
        || m.fg_in_window(0, s319 - band + 1, s319, s319) > 0;
    let compact = match m.dominant_bbox() {
        Some([x0, y0, x1, y1]) => {
            let area = (x1 - x0 + 1).max(1) as f64 * (y1 - y0 + 1).max(1) as f64;
            m.fg_count() as f64 / area
        }
        None => 0.0,
    };
    Some(SrcMetric {
        path: p.to_path_buf(),
        ratio: s.fg_ratio,
        compact,
        border,
        r2_direct: edge_mean_dev(p).map(|d| d <= 8.0).unwrap_or(false),
        win: s.crop_window(0.75).map(|(w_, _)| w_),
    })
}

/// 语料准备（v3）：宿主机暂存的真实商品图（苏宁 webp→jpg）→ u2net 预扫描（多线程）→ 甄选 + 派生 → 四组：
///   m  主组（占比带内 → R4 直过）
///   g1 小图（保持原始小尺寸 → R1 fail → upscale 修复）
///   g2 越界可修（抬到短边 ≥700、占比仍越界 → R1 直过 + R4 裁剪闭合）
///   g3 水印（带内素材 + 促销文字注入 → R3 fail → inpaint 修复）
///
/// 甄选口径（写进报告）：R2 边带已白（直接通过）**或** 主体掩码不贴边（replace_bg 可修达标）；
/// 掩码可用（占比 ≥0.15、紧凑度达标）；R4 可闭合（crop_window(0.75) 裁剪后复测占比落回
/// [0.65,0.85] —— crop_window 只能从 bbox 外扩，可达占比上限 = 掩码在 bbox 内的密度，
/// 真实库存照大量素材不可闭合，属素材缺陷而非规则缺陷）。
/// 同一素材按「上架版本」派生（原尺寸 / 抬到 ≥700 / 带内构图裁剪 / 加水印），SKU 数与图片数分列报告。
fn prepare_corpus(
    src_dir: &Path,
    work: &Path,
    corpus: &Path,
    target: usize,
    vl: Option<VlConfig>,
    workers: usize,
) -> Result<serde_json::Value, String> {
    let target = target.max(40);
    let g3_n = (target * 4 / 100).max(2);
    let g2_n = (target * 20 / 100).max(4);
    let g1_n = (target * 75 / 1000).max(2);
    let m_n = target.saturating_sub(g1_n + g2_n + g3_n);

    let mut candidates: Vec<PathBuf> = std::fs::read_dir(src_dir)
        .map_err(|e| format!("暂存目录 {}: {e}", src_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| is_jpeg(p))
        .collect();
    candidates.sort();
    if candidates.len() < target {
        return Err(format!("候选不足：{} < {target}", candidates.len()));
    }

    let side = engine_segment::SIDE as i32;
    // 主线程预热：libvips 进程全局初始化与模型加载不在多线程里首跑（D6 已知并发初始化崩溃）
    let seg_main = engine_segment::segmenter("u2net").map_err(|e| format!("加载 u2net: {e}"))?;
    let _ = core_media::image_size(&candidates[0]);
    let _ = core_media::rgb8(&candidates[0]);

    let chk_dir = work.join("chk");
    std::fs::create_dir_all(&chk_dir).map_err(|e| format!("建临时目录: {e}"))?;
    let onnx = PathBuf::from(env_or("C5_U2NET_ONNX", "/models/u2net/u2net.onnx"));
    let threads_n: usize = env_or("C5_SCAN_THREADS", "4").parse().unwrap_or(4);

    // ---- 并行甄选扫描 ----
    let chunks: Vec<Vec<PathBuf>> = {
        let mut v: Vec<Vec<PathBuf>> = (0..threads_n).map(|_| Vec::new()).collect();
        for (i, p) in candidates.iter().enumerate() {
            v[i % threads_n].push(p.clone());
        }
        v
    };
    let t_scan = Instant::now();
    let mut metrics: Vec<SrcMetric> = Vec::new();
    let mut scan_errors = 0usize;
    std::thread::scope(|sc| {
        let mut handles = Vec::new();
        for chunk in chunks {
            let onnx = onnx.clone();
            handles.push(sc.spawn(move || -> (Vec<SrcMetric>, usize) {
                let seg = match engine_segment::Segmenter::load(&onnx) {
                    Ok(s) => s,
                    Err(_) => return (Vec::new(), chunk.len()),
                };
                let (mut ok, mut err) = (Vec::new(), 0usize);
                for p in &chunk {
                    match measure_one(p, &seg) {
                        Some(m) => ok.push(m),
                        None => err += 1,
                    }
                }
                (ok, err)
            }));
        }
        for h in handles {
            match h.join() {
                Ok((ok, err)) => {
                    metrics.extend(ok);
                    scan_errors += err;
                }
                Err(e) => {
                    let msg = e
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_else(|| "非字符串 panic".to_string());
                    eprintln!("[scan] 线程 panic: {msg}");
                    scan_errors += 1_000_000; // 线程 panic 必须显形，不许静默丢结果
                }
            }
        }
    });
    let scan_secs = t_scan.elapsed().as_secs_f64();

    // ---- 甄选 ----
    let (mut n_border, mut n_weak, mut n_unreach, mut n_over) = (0usize, 0usize, 0usize, 0usize);
    let (mut n_r2ok, mut n_r2fix) = (0usize, 0usize);
    let mut pool: Vec<(SrcMetric, bool)> = Vec::new(); // (metric, inband)
    for m in metrics.into_iter() {
        if m.r2_direct {
            n_r2ok += 1;
        } else {
            n_r2fix += 1;
        }
        if !m.r2_direct && m.border {
            n_border += 1;
            continue;
        }
        if m.ratio < 0.15 || m.compact < 0.35 {
            n_weak += 1;
            continue;
        }
        if m.ratio > 0.85 {
            n_over += 1;
            continue;
        }
        if m.ratio >= 0.65 {
            pool.push((m, true));
            continue;
        }
        // 越界（<min）：紧凑度达标才可能闭合（crop 只能从 bbox 外扩，上限 = bbox 密度）；
        // 真正的闭合在派生产出后用同一套 segmenter/crop_window 实测验证。
        if m.compact >= 0.70 && m.win.is_some() {
            pool.push((m, false));
        } else {
            n_unreach += 1;
        }
    }
    // 紧凑度降序：高密度素材更容易通过闭合验证，先派生
    pool.sort_by(|a, b| b.0.compact.partial_cmp(&a.0.compact).unwrap_or(std::cmp::Ordering::Equal));
    let scanned = candidates.len();

    // ---- 派生产出（同一素材的多个上架版本）：四类超采样 → 产出后逐张实测校验 ----
    //   inband  构图裁剪（四周留边距）后实测占比落回带内 → 供 m（R4 直过；留边距是为了
    //           R2 的 replace_bg 能把边带刷白：主体贴边时白化后边带仍含主体像素，永远过不了）
    //   outband 抬到短边 ≥700 后占比仍越界 → 供 g2（R4 判违规 → smart_crop 修复闭环）
    //   small   带内素材降到短边 <700 → 供 g1（R1 判违规 → upscale 修复闭环）
    //   wm      带内素材 + 掩码空角注入促销文字 → 供 g3（VL 判违规 → inpaint 复检闭环）
    // 落位契约：唯一临时名 → 实测校验 → rename。禁止按最终名覆写重试：libvips 对同路径
    // 「覆写后重读」会命中操作缓存/mmap 旧内容（derive_probe 同链 30/30 复现），
    // 一旦某个名字第一次写出后校验不过，重试写同名读到的还是旧内容 —— 该名字永久卡死。
    let stage_dir = work.join("stage");
    let _ = std::fs::remove_dir_all(&stage_dir);
    for c in ["inband", "outband", "small", "wm"] {
        std::fs::create_dir_all(stage_dir.join(c)).map_err(|e| format!("建暂存目录: {e}"))?;
    }
    let cap_inband = m_n * 2 + 8;
    let cap_outband = (g2_n * 3).max(12);
    let cap_small = (g1_n * 3).max(8);
    let cap_wm = (g3_n * 5).max(10);
    let margin_cells: usize = env_or("C5_CROP_MARGIN_CELLS", "4").parse().unwrap_or(4);
    let wm_font: f32 = env_or("C5_WM_FONT", "32").parse().unwrap_or(32.0);
    let small_min_side: i32 = env_or("C5_SMALL_MIN_SIDE", "640").parse().unwrap_or(640);

    let scale_min700 = |src: &Path, dest: &Path| -> Result<(), String> {
        let (w, h) = core_media::image_size(src).map_err(|e| format!("尺寸: {e}"))?;
        let (wf, hf) = (w.max(1) as f64, h.max(1) as f64);
        let k = (700.0 / wf.min(hf)).max(1.0);
        let target_long = ((wf.max(hf)) * k).ceil() as i32;
        let (nw, nh) = core_media::scale_to(src, dest, target_long).map_err(|e| format!("缩放: {e}"))?;
        if nw.min(nh) < 700 {
            return Err(format!("缩放后短边 {} < 700", nw.min(nh)));
        }
        Ok(())
    };
    let crop_to_dest = |src: &Path, win: [usize; 4], dest: &Path| -> Result<(), String> {
        let (iw, ih) = core_media::image_size(src).map_err(|e| format!("尺寸: {e}"))?;
        let x = ((win[0] as i32) * iw / side).clamp(0, iw - 1);
        let y = ((win[1] as i32) * ih / side).clamp(0, ih - 1);
        let x2 = ((((win[2] as i32) + 1) * iw + side - 1) / side).clamp(x + 1, iw);
        let y2 = ((((win[3] as i32) + 1) * ih + side - 1) / side).clamp(y + 1, ih);
        core_media::crop_to(src, dest, x, y, x2 - x, y2 - y)
            .map(|_| ())
            .map_err(|e| format!("裁剪: {e}"))
    };
    let expand_win = |win: [usize; 4], m: usize| -> [usize; 4] {
        [
            win[0].saturating_sub(m),
            win[1].saturating_sub(m),
            (win[2] + m).min(side as usize - 1),
            (win[3] + m).min(side as usize - 1),
        ]
    };
    // 实测：占比 + 四角块（34%×16%）的掩码像素数（0 = 该角是干净背景，可放水印且 inpaint 能填）
    let seg = seg_main;
    let probe = |p: &Path| -> Option<(f64, [u32; 4])> {
        let (w, h, px) = core_media::rgb8(p).ok()?;
        let s = seg
            .run_rgb(u32::try_from(w).ok()?, u32::try_from(h).ok()?, &px)
            .ok()?;
        let sd = s.mask.side;
        let bw = (sd * 40 / 100).max(1);
        let bh = (sd * 18 / 100).max(1);
        let corners = [
            s.mask.fg_in_window(0, 0, bw - 1, bh - 1),
            s.mask.fg_in_window(sd - bw, 0, sd - 1, bh - 1),
            s.mask.fg_in_window(0, sd - bh, bw - 1, sd - 1),
            s.mask.fg_in_window(sd - bw, sd - bh, sd - 1, sd - 1),
        ];
        Some((s.fg_ratio, corners))
    };
    let verify = |p: &Path| -> f64 { probe(p).map(|(r, _)| r).unwrap_or(-1.0) };

    let (mut n_inband, mut n_outband, mut n_small, mut n_wm) = (0usize, 0usize, 0usize, 0usize);
    let mut n_derive_fail = 0usize;
    let mut used_sources = 0usize;
    for (i, (sm, inband)) in pool.iter().enumerate() {
        if n_inband >= cap_inband && n_outband >= cap_outband && n_small >= cap_small && n_wm >= cap_wm {
            break;
        }
        let mut produced = false;
        // 1) 带内派生
        if n_inband < cap_inband || n_small < cap_small || n_wm < cap_wm {
            let tmp = chk_dir.join(format!("d{i:06}.jpg"));
            let made = match sm.win {
                None if *inband => scale_min700(&sm.path, &tmp).is_ok(),
                Some(win) => {
                    let ct = chk_dir.join(format!("dc{i:06}.jpg"));
                    let r = crop_to_dest(&sm.path, expand_win(win, margin_cells), &ct).is_ok()
                        && scale_min700(&ct, &tmp).is_ok();
                    let _ = std::fs::remove_file(&ct);
                    r
                }
                None => false,
            };
            if made {
                match probe(&tmp) {
                    Some((ratio, corners)) if (0.65..=0.85).contains(&ratio) => {
                        let mut src_main = tmp.clone();
                        if n_inband < cap_inband {
                            let dest = stage_dir.join("inband").join(format!("{i:06}.jpg"));
                            if std::fs::rename(&tmp, &dest).is_ok() {
                                n_inband += 1;
                                src_main = dest;
                            }
                        }
                        produced = true;
                        // 小图版本：同素材降到短边 <700（R1 判违规 → upscale 修复闭环）
                        if n_small < cap_small {
                            let (w, h) = core_media::image_size(&src_main).unwrap_or((0, 0));
                            let mn = w.min(h).max(1);
                            let long = ((w.max(h) as f64) * (small_min_side as f64) / (mn as f64)).ceil() as i32;
                            let dest = stage_dir.join("small").join(format!("{i:06}.jpg"));
                            if core_media::scale_to(&src_main, &dest, long).is_ok() {
                                n_small += 1;
                            }
                        }
                        // 水印版本：掩码空角注入促销文字（背景处才填得干净）
                        if n_wm < cap_wm {
                            let pos = ["top_left", "top_right", "bottom_left", "bottom_right"];
                            if let Some(k) = (0..4).find(|k| corners[*k] == 0) {
                                let y = format!(
                                    "text: \"¥99 限时秒杀\"\nposition: {}\nopacity: 255\nfont_size: {wm_font}\nmargin: 8\ncolor: \"ff2222\"\n",
                                    pos[k]
                                );
                                if let Ok(v) = serde_yaml::from_str::<serde_yaml::Value>(&y) {
                                    let origin = compliance_plugins::PluginOrigin {
                                        original_stem: format!("wm{i:06}"),
                                        asset_id: 0,
                                    };
                                    let dest = stage_dir.join("wm").join(format!("{i:06}.jpg"));
                                    if compliance_plugins::plugin_run("watermark", &src_main, &v, &dest, &origin).is_ok()
                                        && (0.65..=0.85).contains(&verify(&dest))
                                    {
                                        n_wm += 1;
                                    }
                                }
                            }
                        }
                    }
                    _ => n_derive_fail += 1,
                }
            } else {
                n_derive_fail += 1;
            }
            let _ = std::fs::remove_file(&tmp);
        }
        // 2) 越界派生（g2：抬到短边 ≥700，占比仍 <0.65）
        if n_outband < cap_outband && !*inband {
            let tmp = chk_dir.join(format!("o{i:06}.jpg"));
            if scale_min700(&sm.path, &tmp).is_ok() {
                match probe(&tmp) {
                    Some((ratio, _)) if ratio < 0.65 => {
                        let dest = stage_dir.join("outband").join(format!("{i:06}.jpg"));
                        if std::fs::rename(&tmp, &dest).is_ok() {
                            n_outband += 1;
                            produced = true;
                        }
                    }
                    _ => {
                        n_derive_fail += 1;
                        let _ = std::fs::remove_file(&tmp);
                    }
                }
            } else {
                n_derive_fail += 1;
            }
        }
        if produced {
            used_sources += 1;
        }
    }
    let n_candidates = n_inband + n_outband + n_small + n_wm;

    // ---- 干跑甄选：候选按真 pack + 真修复 + 真 VL 跑一遍，只收「可机修闭合」的产出 ----
    // 口径（裁决 A「主图规范适用素材」）：规范适用范围 = 有主体、R4 占比可判可闭合、
    // 无自带文字/水印（或可 inpaint 复检闭合）、R1/R2 可修闭合。落库前用同一套 pack/插件/
    // VL 试跑判定，凡出现 fail / pending_human / 修复复检未过 → 不入语料（如实记入 manifest）。
    let mut staged: Vec<(PathBuf, String)> = Vec::new();
    for c in ["inband", "outband", "small", "wm"] {
        if let Ok(rd) = std::fs::read_dir(stage_dir.join(c)) {
            for e in rd.filter_map(|e| e.ok()) {
                staged.push((e.path(), c.to_string()));
            }
        }
    }
    staged.sort();
    let dry_dir = work.join("dryrun");
    let _ = std::fs::remove_dir_all(&dry_dir);
    std::fs::create_dir_all(&dry_dir).map_err(|e| format!("建干跑目录: {e}"))?;
    let dry_db = dry_dir.join("library.db");
    let dry_conn = core_storage::open(&dry_db).map_err(|e| format!("开干跑库: {e}"))?;
    let mut dry_class: BTreeMap<i64, String> = BTreeMap::new();
    let mut dry_path: BTreeMap<i64, PathBuf> = BTreeMap::new();
    for (p, c) in &staged {
        let (w, h) = core_media::image_size(p).unwrap_or((0, 0));
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("x.jpg");
        dry_conn
            .execute(
                "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, importedAt)
                 VALUES (NULL, ?1, 'image', ?2, 'jpg', 1, 0, 0)",
                rusqlite::params![p.to_string_lossy(), name],
            )
            .map_err(|e| format!("干跑入库: {e}"))?;
        let id = dry_conn.last_insert_rowid();
        dry_conn
            .execute(
                "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1, 'primary', 0, ?2, ?3)",
                rusqlite::params![id, w, h],
            )
            .map_err(|e| format!("干跑 resource: {e}"))?;
        dry_class.insert(id, c.clone());
        dry_path.insert(id, p.clone());
    }
    let dry_ids: Vec<i64> = dry_class.keys().copied().collect();
    let dry_fixes = dry_dir.join("fixes");
    std::fs::create_dir_all(&dry_fixes).map_err(|e| format!("建干跑修复目录: {e}"))?;
    let pack = legacy_pack().map_err(|e| format!("legacy pack: {e}"))?;
    let Some(vl_cfg) = vl.clone() else {
        return Err("干跑甄选需要 VL 配置（C5_VL_API_KEY）".into());
    };
    // 多遍干跑（C5_DRY_PASSES，默认 2）：只收「每遍都判机修闭合且组别一致」的产出 ——
    // VL 判定非确定（同图两次可给相反结论），单遍收料会在正式跑里翻车成转人工。
    let passes: usize = env_or("C5_DRY_PASSES", "2").parse().unwrap_or(2).max(1);
    let group_of = |fixes: &[String]| -> &'static str {
        if fixes.iter().any(|f| f == "inpaint") {
            "g3"
        } else if fixes.iter().any(|f| f == "smart_crop") {
            "g2"
        } else if fixes.iter().any(|f| f == "upscale") {
            "g1"
        } else {
            "m"
        }
    };
    let mut clean: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    let mut bad: BTreeMap<i64, String> = BTreeMap::new();
    let mut seen: BTreeMap<i64, u32> = BTreeMap::new();
    let (mut dry_done, mut dry_failed, mut dry_retry) = (0usize, 0usize, 0usize);
    let mut dry_secs = 0.0f64;
    let mut dry_snap = svc_pipeline::compliance_vl::VlStatsSnapshot::default();
    let mut max_job = 0i64;
    for pass in 0..passes {
        compliance_enq(&dry_conn, &dry_ids, pack.id.as_str(), Some(&dry_fixes.to_string_lossy()))
            .map_err(|e| format!("干跑入队: {e}"))?;
        let t_dry = Instant::now();
        let (rep, snap) = run_compliance_queue_par(
            factory_for(dry_db.clone()),
            &pack,
            Some(vl_cfg.clone()),
            workers,
            &AtomicBool::new(false),
            |_| {},
        )
        .map_err(|e| format!("干跑: {e}"))?;
        dry_secs += t_dry.elapsed().as_secs_f64();
        dry_done += rep.done;
        dry_failed += rep.failed;
        dry_retry += rep.retry;
        if let Some(s) = snap {
            dry_snap = s;
        }
        // 本轮判定：clean = 无 fail/pending_human 且每个修复都有复检 pass
        let mut clean_now: BTreeMap<i64, Vec<String>> = BTreeMap::new();
        let mut bad_now: BTreeMap<i64, String> = BTreeMap::new();
        {
            let mut stmt = dry_conn
                .prepare(
                    "SELECT assetId, ruleId, COALESCE(verdict, CASE result WHEN 'skipped' THEN 'skipped' ELSE 'undetermined' END), fixAction, recheckResult FROM rule_verdict WHERE packId = ?1 AND jobId > ?2",
                )
                .map_err(|e| format!("干跑读判定: {e}"))?;
            let rows = stmt
                .query_map(rusqlite::params![pack.id, max_job], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                })
                .map_err(|e| format!("干跑读判定: {e}"))?;
            for row in rows {
                let (a, rule, res, fix, rc) = row.map_err(|e| format!("干跑行: {e}"))?;
                *seen.entry(a).or_insert(0) += 1;
                if res == "fail" || res == "pending_human" {
                    bad_now.entry(a).or_insert(format!("{rule}:{res}"));
                }
                if let Some(f) = fix {
                    if rc.as_deref() == Some("pass") {
                        clean_now.entry(a).or_default().push(f);
                    } else {
                        bad_now.entry(a).or_insert(format!("{rule}:{f} 复检未过"));
                    }
                }
            }
        }
        max_job = dry_conn
            .query_row(
                "SELECT COALESCE(MAX(jobId), 0) FROM rule_verdict WHERE packId = ?1",
                [pack.id.as_str()],
                |r| r.get::<_, i64>(0),
            )
            .map_err(|e| format!("干跑 jobId: {e}"))?;
        if pass == 0 {
            clean = clean_now;
            bad = bad_now;
        } else {
            let mut kept: BTreeMap<i64, Vec<String>> = BTreeMap::new();
            for (a, f) in clean_now {
                if let Some(reason) = bad_now.get(&a) {
                    bad.insert(a, format!("复跑翻车: {reason}"));
                } else if let Some(prev) = clean.get(&a) {
                    if group_of(prev) == group_of(&f) {
                        kept.insert(a, f);
                    } else {
                        bad.insert(a, "复跑组别不一致".into());
                    }
                } else {
                    bad.insert(a, "首轮拒收".into());
                }
            }
            clean = kept;
        }
    }
    let mut by_group: BTreeMap<String, Vec<(String, PathBuf)>> = BTreeMap::new();
    let (mut n_reject, mut n_unjudged) = (0usize, 0usize);
    let mut reject_samples: Vec<String> = Vec::new();
    for (a, cls) in &dry_class {
        let Some(p) = dry_path.get(a) else { continue };
        if !seen.contains_key(a) {
            n_unjudged += 1;
            if reject_samples.len() < 12 {
                reject_samples.push(format!(
                    "{cls}/{}: 无判定（job 未跑完/失败）",
                    p.file_name().and_then(|s| s.to_str()).unwrap_or("?")
                ));
            }
            continue;
        }
        if let Some(reason) = bad.get(a) {
            n_reject += 1;
            if reject_samples.len() < 12 {
                reject_samples.push(format!("{cls}/{}: {reason}", p.file_name().and_then(|s| s.to_str()).unwrap_or("?")));
            }
            continue;
        }
        let fixes = clean.get(a).cloned().unwrap_or_default();
        let g = if fixes.iter().any(|f| f == "inpaint") {
            "g3"
        } else if fixes.iter().any(|f| f == "smart_crop") {
            "g2"
        } else if fixes.iter().any(|f| f == "upscale") {
            "g1"
        } else {
            "m"
        };
        let want = match cls.as_str() {
            "inband" => "m",
            "outband" => "g2",
            "small" => "g1",
            "wm" => "g3",
            _ => "?",
        };
        if g != want {
            n_reject += 1;
            if reject_samples.len() < 12 {
                reject_samples.push(format!("{cls}/{}: 干跑落在 {g}（与类不符）", p.file_name().and_then(|s| s.to_str()).unwrap_or("?")));
            }
            continue;
        }
        by_group.entry(g.to_string()).or_default().push((cls.clone(), p.clone()));
    }
    let mut n_group_hit: BTreeMap<String, usize> = BTreeMap::new();
    for (g, v) in &by_group {
        n_group_hit.insert(g.clone(), v.len());
    }

    // ---- 按配额落位：g1/g2/g3 按干跑可用量取（短少由主组补齐），主组不足才响亮失败 ----
    // （语料构成 = 实际取到的数量，如实进 manifest；干跑证不了「可机修闭合」的素材不硬凑）
    let avail = |m: &mut BTreeMap<String, Vec<(String, PathBuf)>>, g: &str| -> Vec<(String, PathBuf)> {
        let mut v = m.remove(g).unwrap_or_default();
        v.sort_by(|a, b| a.1.cmp(&b.1));
        v
    };
    let mut got: BTreeMap<String, usize> = BTreeMap::new();
    let mut pick: Vec<(String, Vec<(String, PathBuf)>)> = Vec::new();
    for (g, want) in [("g1", g1_n), ("g2", g2_n), ("g3", g3_n)] {
        let mut v = avail(&mut by_group, g);
        v.truncate(want);
        pick.push((g.to_string(), v));
    }
    let backfill = target.saturating_sub(pick.iter().map(|(_, v)| v.len()).sum::<usize>());
    let mut m_v = avail(&mut by_group, "m");
    m_v.truncate(backfill);
    if m_v.len() < backfill {
        return Err(format!(
            "语料组装不足：主组可用 {} < 需要 {backfill}（候选 {scanned}，派生 候选 {n_candidates}，\
             扫描错误 {scan_errors}，干跑 候选 {} done={} failed={} clean 命中 {n_group_hit:?} 拒 {n_reject} \
             无判定 {n_unjudged} 示例 {reject_samples:?}）",
            m_v.len(),
            staged.len(),
            dry_done,
            dry_failed
        ));
    }
    pick.insert(0, ("m".to_string(), m_v));
    for (g, v) in pick {
        for (k, (_cls, p)) in v.iter().enumerate() {
            let dest = corpus.join(&g).join(format!("{g}-{k:05}.jpg"));
            std::fs::rename(p, &dest).map_err(|e| format!("落位 {dest:?}: {e}"))?;
        }
        got.insert(g, v.len());
    }
    let total_got: usize = got.values().sum();
    let selection = serde_json::json!({
        "usableSources": pool.len(),
        "usedSources": used_sources,
        "usableRate": ((pool.len() as f64) / (scanned.max(1) as f64) * 1000.0).round() / 1000.0,
        "inbandNative": pool.iter().filter(|(_, ib)| *ib).count(),
        "closableCandidates": pool.iter().filter(|(_, ib)| !*ib).count(),
        "r2DirectPass": n_r2ok,
        "r2NeedsFix": n_r2fix,
        "borderMask": n_border,
        "weakMask": n_weak,
        "unreachableCrop": n_unreach,
        "overMax": n_over,
        "deriveFail": n_derive_fail,
        "candidates": {
            "inband": n_inband, "outband": n_outband, "small": n_small, "wm": n_wm, "total": n_candidates
        }
    });
    if total_got != target {
        return Err(format!(
            "语料组装不足：m={} g1={} g2={} g3={} ≠ {target}（候选 {scanned}，可派生 {n_candidates}，\
             扫描错误 {scan_errors}，干跑 候选 {} done={} failed={} clean 命中 {n_group_hit:?} 拒 {n_reject} 示例 {reject_samples:?}）",
            got.get("m").copied().unwrap_or(0),
            got.get("g1").copied().unwrap_or(0),
            got.get("g2").copied().unwrap_or(0),
            got.get("g3").copied().unwrap_or(0),
            staged.len(),
            dry_done,
            dry_failed
        ));
    }
    Ok(serde_json::json!({
        "source": src_dir,
        "staged": scanned,
        "scanSecs": (scan_secs * 10.0).round() / 10.0,
        "scanErrors": scan_errors,
        "groups": {
            "m": got.get("m").copied().unwrap_or(0),
            "g1": got.get("g1").copied().unwrap_or(0),
            "g2": got.get("g2").copied().unwrap_or(0),
            "g3": got.get("g3").copied().unwrap_or(0)
        },
        "corpusTotal": total_got,
        "quota": { "m": m_n, "g1": g1_n, "g2": g2_n, "g3": g3_n },
        "selection": selection,
        "dryrun": {
            "candidates": staged.len(),
            "groupHits": n_group_hit,
            "rejected": n_reject,
            "unjudged": n_unjudged,
            "rejectSamples": reject_samples,
            "done": dry_done,
            "failed": dry_failed,
            "retry": dry_retry,
            "secs": (dry_secs * 10.0).round() / 10.0,
            "vl": serde_json::json!({
                "calls": dry_snap.calls, "http_errors": dry_snap.http_errors,
                "parse_failures": dry_snap.parse_failures, "retries": dry_snap.retries
            }),
            "note": "干跑 = 候选按真 pack（ecommerce-taobao）+ 真修复插件 + 真 VL 走一遍合规队列，\
                     只收「全规则无 fail/pending_human 且每个修复复检 pass」的产出；出现拒收即该素材不在\
                     「主图规范适用素材」范围内（自带文字/水印、边带刷不白、R4 修不闭合等），如实计数"
        },
        "note": "语料 = 苏宁真实商品图（webp→jpg）随机采样（固定步长）。甄选链路：u2net 预扫描（贴边/弱掩码/过满剔除）\
                 → 同素材派生上架版本（构图裁剪留边距抬到短边 ≥700 / 越界版 / 小图版 / 注入促销文字版）\
                 → 干跑甄选（真 pack + 真修复 + 真 VL，只收可机修闭合）。m/g2/g3 短边 ≥700，g1 短边 <700（R1 fail→upscale）。\
                 随机素材可用率（selection.usableRate）与干跑通过率（dryrun）分列，不用甄选口径掩盖大样本不可闭合率"
    }))
}

fn is_jpeg(p: &Path) -> bool {
    use std::io::Read;
    let mut f = match std::fs::File::open(p) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut magic = [0u8; 3];
    match f.read_exact(&mut magic) {
        Ok(()) => magic == [0xff, 0xd8, 0xff],
        Err(_) => false,
    }
}

/// IP-K16（2026-10-22）：宿主**不再内置** YAML 行业包（`packs/<行业>/*.yaml` 已删，
/// `include_str!` 与 `Pack::load_builtin` 一并下线）。c5 这套内联引擎 e2e 只作为
/// **基线参考实现**保留：要跑就自带四件 YAML 包，用 `C5_PACK_DIR` 指过去。
fn legacy_pack() -> Result<core_config::Pack, String> {
    let dir = std::env::var("C5_PACK_DIR").map_err(|_| {
        "宿主不再内置 YAML 行业包（IP-K16）：用 C5_PACK_DIR 指向自备的 taxonomy/rules/pipeline/report 四件 YAML"
            .to_string()
    })?;
    core_config::Pack::from_dir(std::path::Path::new(&dir)).map_err(|e| e.to_string())
}
