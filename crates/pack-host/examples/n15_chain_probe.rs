//! N15 真机处理链探针：**3 条动作链 × 10 张语料**，在宿主同一条路径上跑
//! （`PackHost` 真子进程 + 真能力网关 + 真模型），产物逐张落盘并做尺寸/体积/白底断言。
//!
//! 为什么不是 HTTP：`POST /api/jobs` 现阶段直通 `judge`（`crates/app-web/src/lib.rs` 注释：
//! 「处理类动作链（process kind）落地后再按 kind 分流」），N12 报告 §7.1 也写明「处理跑 1 条链
//! 的**执行路径不在本卡**」。所以真机上能跑处理链的只有宿主同一条 PackHost 路径 —— 本 example
//! 就是那条路径的最小驱动（与 `n14_process_gate` 同一套参数口径、同一套 `parse_row` 契约关）。
//!
//! 三条链（卡面口径）：
//! 1. `resize_webp`：缩放（长边 1600）→ 转 webp（第二步的输入就是第一步的产物，验链式衔接）
//! 2. `compress`：压缩到 ≤200KB（`target_bytes` + 质量底线 60 + 允许降采样到长边 640）
//! 3. `white_bg_fill`：白底化（segment 包 `bgMode=white`）
//!
//! 用法（真机 / 容器，路径都要显式给 —— example 里不许出现构建机的 `CARGO_MANIFEST_DIR`）：
//!
//! ```text
//! n15_chain_probe.exe \
//!   --packs  <含 imaging/segment/upscale 三目录的父目录> \
//!   --corpus <语料 v1.1 根> --work-dir <工作目录> --evidence <证据 json> \
//!   [--runtime-python <解释器>] [--launcher-dir <含 dampack_launcher.py 的目录>] \
//!   [--per-chain 10]
//! ```
//!
//! 退出码：0 = 全部链 × 全部图 PASS；1 = 有失败（证据照样落盘）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use image::{DynamicImage, GenericImageView};
use serde_json::{json, Value};

use pack_host::result::{parse_row, PluginOutput};
use pack_host::{load_pack, Gateway, GatewayConfig, PackHost, PackHostConfig};

/// 三个处理包（判定类不在本探针范围）。
const PACK_IMAGING: &str = "imaging";
const PACK_SEGMENT: &str = "segment";
const PACK_UPSCALE: &str = "upscale";

/// 能力并集（三个包声明的合集）—— 一条网关同时喂三个包，缺一个端点就是 403。
const CAPABILITIES: [&str; 7] = [
    "io.read",
    "detect.u2net",
    "detect.white",
    "op.geometric",
    "op.overlay",
    "op.generative",
    "report.emit",
];

/// 与 `n14_process_gate` 完全同口径的常量（改这里就等于改门禁，别动）。
const RESIZE_LONG_EDGE: u64 = 1600;
const COMPRESS_MAX_BYTES: u64 = 204_800;
const BG_BAND_PX: u32 = 4;
const BG_MAX_DEV: f64 = 3.0;

struct Args {
    packs: PathBuf,
    corpus: PathBuf,
    work: PathBuf,
    evidence: PathBuf,
    runtime_python: Option<PathBuf>,
    launcher_dir: Option<PathBuf>,
    per_chain: usize,
    /// `white_bg_fill` 的 `maxSide`（0 = 不预缩放）。N19 反向验证用：给一张 6000px 级的图
    /// 打开上限，必须看到 `preScale` 段 + 产物尺寸=上限内，而不是靠「缩到很小」蒙过去。
    max_side: u32,
}

/// 进度行（显式 flush）：真机日志重定向到文件时，卡死那一步必须能看出「上一步是什么」。
fn say(msg: impl AsRef<str>) {
    println!("{}", msg.as_ref());
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn parse_args() -> Result<Args, String> {
    let mut packs = None;
    let mut corpus = None;
    let mut work = None;
    let mut evidence = None;
    let mut runtime_python = None;
    let mut launcher_dir = None;
    let mut per_chain = 10usize;
    let mut max_side = 0u32;
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let need = |i: usize| -> Result<String, String> {
            argv.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{} 缺参数值", argv[i]))
        };
        match argv[i].as_str() {
            "--packs" => packs = Some(PathBuf::from(need(i)?)),
            "--corpus" => corpus = Some(PathBuf::from(need(i)?)),
            "--work-dir" => work = Some(PathBuf::from(need(i)?)),
            "--evidence" => evidence = Some(PathBuf::from(need(i)?)),
            "--runtime-python" => runtime_python = Some(PathBuf::from(need(i)?)),
            "--launcher-dir" => launcher_dir = Some(PathBuf::from(need(i)?)),
            "--per-chain" => {
                per_chain = need(i)?.parse().map_err(|e| format!("--per-chain: {e}"))?;
            }
            "--max-side" => {
                max_side = need(i)?.parse().map_err(|e| format!("--max-side: {e}"))?;
            }
            other => return Err(format!("未知参数 {other}")),
        }
        i += 2;
    }
    Ok(Args {
        packs: packs.ok_or("缺 --packs")?,
        corpus: corpus.ok_or("缺 --corpus")?,
        work: work.ok_or("缺 --work-dir")?,
        evidence: evidence.ok_or("缺 --evidence")?,
        runtime_python,
        launcher_dir,
        per_chain,
        max_side,
    })
}

// ---------------------------------------------------------------------------
// 宿主装配
// ---------------------------------------------------------------------------

struct Harness {
    _gw: Gateway,
    imaging: PackHost,
    segment: PackHost,
    upscale: PackHost,
}

fn start_harness(a: &Args) -> Result<Harness, String> {
    std::fs::create_dir_all(&a.work).map_err(|e| format!("建 work-dir 失败：{e}"))?;
    let caps: Vec<String> = CAPABILITIES.iter().map(|s| s.to_string()).collect();
    let gw = Gateway::start(GatewayConfig::new(caps, &a.work).with_token("n15-chain-probe"))
        .map_err(|e| format!("网关启动失败：{e}"))?;
    let open = |pack: &str, tag: &str| -> Result<PackHost, String> {
        let dir = a.packs.join(pack);
        let (manifest, _) = load_pack(&dir).map_err(|e| format!("{pack} manifest 不合法：{e}"))?;
        if manifest.has_verdict() {
            return Err(format!("{pack} 不应声明 outputs=verdict（本探针只跑处理包）"));
        }
        let dir_work = a.work.join(format!("pack-{tag}"));
        std::fs::create_dir_all(&dir_work).ok();
        let mut cfg = PackHostConfig::new(gw.base_url(), gw.token(), &dir_work);
        // 解释器序：包内 venv → runtime_python（真机＝内置 runtime）→ PATH 上的 python3
        cfg.runtime_python = a
            .runtime_python
            .clone()
            .or_else(|| Some(PathBuf::from("python3")));
        if let Some(ld) = a.launcher_dir.clone() {
            // 发行形态：启动器脚本在 `<app>/pack-launcher/`，不在包目录里
            cfg = cfg.with_launcher_dir(ld);
        }
        PackHost::open(&dir, cfg).map_err(|e| format!("{pack} 打不开（health 握手失败？）：{e}"))
    };
    let imaging = open(PACK_IMAGING, "imaging")?;
    let segment = open(PACK_SEGMENT, "segment")?;
    let upscale = open(PACK_UPSCALE, "upscale")?;
    Ok(Harness {
        _gw: gw,
        imaging,
        segment,
        upscale,
    })
}

// ---------------------------------------------------------------------------
// 单步调用（一行 = 一次处理）
// ---------------------------------------------------------------------------

struct Step {
    label: String,
    row: Value,
    contract_ok: bool,
    contract_detail: String,
    out_path: Option<PathBuf>,
    bytes: u64,
    error: Option<String>,
}

fn run_step(
    host: &PackHost,
    tag: &str,
    src: &Path,
    workdir: &Path,
    mut opts: Value,
) -> Result<Step, String> {
    std::fs::create_dir_all(workdir).map_err(|e| format!("建资产目录失败：{e}"))?;
    opts["workdir"] = json!(workdir.to_string_lossy());
    let call = host.call(
        "judge",
        json!({
            "job_id": format!("n15-{tag}"),
            "asset": { "id": tag, "path": src.to_string_lossy() },
            "options": opts,
        }),
    );
    let reply = match call {
        Ok(v) => v,
        Err(e) => {
            // N19：调用失败（尤其超时）也出一行**结构化**结果（`error.code/detail`），
            // 不是只把中文塞进 evidence —— 报告与后续卡要按 code 聚合，不解析文案。
            let row = pack_host::result::failure_row(tag, &e);
            let detail = row["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            return Ok(Step {
                label: tag.to_string(),
                row,
                contract_ok: false,
                contract_detail: format!("宿主调用失败：{detail}"),
                out_path: None,
                bytes: 0,
                error: Some(detail),
            });
        }
    };
    let rows = reply.get("rows").and_then(Value::as_array).cloned().unwrap_or_default();
    if rows.len() != 1 {
        return Err(format!("应答 rows 数量 {} != 1：{reply}", rows.len()));
    }
    let row = rows[0].clone();
    // 宿主契约关：处理类包每行都必须过 parse_row（产物去处 / verdict 红线都在这）
    let (contract_ok, contract_detail) = match parse_row(&[PluginOutput::Artifact], &row) {
        Ok(_) => (true, String::new()),
        Err(v) => (false, format!("{}: {v}", v.code())),
    };
    let art = row.get("artifacts").and_then(Value::as_array).cloned().unwrap_or_default();
    let out_path = art
        .first()
        .and_then(|a| a.get("path"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .filter(|p| p.is_file());
    let bytes = out_path
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .unwrap_or(0);
    Ok(Step {
        label: tag.to_string(),
        row,
        contract_ok,
        contract_detail,
        out_path,
        bytes,
        error: None,
    })
}

// ---------------------------------------------------------------------------
// 像素探针
// ---------------------------------------------------------------------------

fn magic_format(path: &Path) -> &'static str {
    let raw = std::fs::read(path).unwrap_or_default();
    if raw.len() >= 3 && raw[0] == 0xFF && raw[1] == 0xD8 {
        return "jpg";
    }
    if raw.len() >= 8 && &raw[..8] == b"\x89PNG\r\n\x1a\n" {
        return "png";
    }
    if raw.len() >= 12 && &raw[..4] == b"RIFF" && &raw[8..12] == b"WEBP" {
        return "webp";
    }
    if raw.len() >= 12 && &raw[4..8] == b"ftyp" {
        return "avif";
    }
    "unknown"
}

fn load(path: &Path) -> Result<DynamicImage, String> {
    image::open(path).map_err(|e| format!("解码失败 {}：{e}", path.display()))
}

fn dims(path: &Path) -> Result<(u32, u32), String> {
    let img = load(path)?;
    let (w, h) = img.dimensions();
    Ok((w, h))
}

/// 4px 边框带对纯白（255,255,255）的最大通道偏差 —— 白底判据（与 n14 同口径）。
fn border_dev_max(path: &Path, band: u32) -> Result<f64, String> {
    let img = load(path)?.to_rgb8();
    let (w, h) = img.dimensions();
    let mut worst = 0f64;
    for y in 0..h {
        for x in 0..w {
            let edge = x < band || y < band || x + band >= w || y + band >= h;
            if !edge {
                continue;
            }
            let [r, g, b] = img.get_pixel(x, y).0;
            for c in [r, g, b] {
                worst = worst.max((255.0 - c as f64).abs());
            }
        }
    }
    Ok(worst)
}

// ---------------------------------------------------------------------------
// 三条链
// ---------------------------------------------------------------------------

fn chain_opts(chain: &str, expect: &Value, max_side: u32) -> Value {
    match chain {
        "resize_webp" => json!({
            "action": "resize",
            "resizeMode": "long_edge",
            "resizeLongEdge": RESIZE_LONG_EDGE,
        }),
        "compress" => json!({
            "action": "compress",
            "compressMode": "target_bytes",
            "compressTargetBytes": COMPRESS_MAX_BYTES,
            "compressMinQuality": 60,
            "compressAllowDownscale": true,
            "compressMinLongEdge": 640,
            "compressFormat": expect["out_format"].as_str().unwrap_or("jpg"),
        }),
        "white_bg_fill" => json!({ "bgMode": "white", "maxSide": max_side }),
        other => json!({ "action": other }),
    }
}

fn labels_file(chain: &str) -> &'static str {
    match chain {
        "resize_webp" => "resize.json",
        "compress" => "compress.json",
        _ => "white_bg_fill.json",
    }
}

fn host_for<'a>(h: &'a Harness, chain: &str) -> &'a PackHost {
    match chain {
        "white_bg_fill" => &h.segment,
        _ => &h.imaging,
    }
}

fn check(rows: &mut Vec<Value>, name: &str, ok: bool, detail: impl Into<String>) {
    rows.push(json!({ "assert": name, "ok": ok, "detail": detail.into() }));
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[n15] {e}");
            std::process::exit(2);
        }
    };
    if args.work.exists() {
        std::fs::remove_dir_all(&args.work).ok();
    }
    std::fs::create_dir_all(&args.work).ok();

    let started = Instant::now();
    println!("== N15 三条处理链（真宿主路径）==");
    println!(
        "packs={} corpus={} work={} python={:?} launcher={:?}",
        args.packs.display(),
        args.corpus.display(),
        args.work.display(),
        args.runtime_python,
        args.launcher_dir
    );
    let harness = match start_harness(&args) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[n15] 宿主装配失败：{e}");
            let _ = std::fs::write(
                &args.evidence,
                serde_json::to_vec_pretty(&json!({
                    "ok": false, "stage": "harness", "error": e,
                }))
                .unwrap_or_default(),
            );
            std::process::exit(4);
        }
    };
    println!("== 宿主握手完成（3 个处理包）==");

    let mut chain_rows: Vec<Value> = Vec::new();
    let mut totals: BTreeMap<String, (u32, u32)> = BTreeMap::new();

    for chain in ["resize_webp", "compress", "white_bg_fill"] {
        let labels: Value = match std::fs::read(
            args.corpus.join("labels").join(labels_file(chain)),
        )
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        {
            Some(v) => v,
            None => {
                eprintln!("[n15] 读不到 labels/{}", labels_file(chain));
                std::process::exit(5);
            }
        };
        let items = labels["items"].as_array().cloned().unwrap_or_default();
        let take = items.len().min(args.per_chain);
        let (mut pass, mut fail) = (0u32, 0u32);
        for (idx, item) in items.iter().take(take).enumerate() {
            let file = item["file"].as_str().unwrap_or_default();
            let src = args.corpus.join(file);
            let stem = Path::new(file)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("item")
                .to_string();
            let tag = format!("{chain}-{idx:02}-{stem}");
            let workdir = args.work.join(chain).join(format!("{idx:02}-{stem}"));
            let expect = item["expect"].clone();
            let host = host_for(&harness, chain);
            let mut asserts: Vec<Value> = Vec::new();
            let mut steps: Vec<Value> = Vec::new();

            // ---- 第一步
            say(format!(".. {chain}[{idx}] {file} 开始"));
            let opts = chain_opts(chain, &expect, args.max_side);
            let step1 = match run_step(host, &format!("{tag}-1"), &src, &workdir, opts.clone()) {
                Ok(s) => s,
                Err(e) => {
                    fail += 1;
                    chain_rows.push(json!({
                        "chain": chain, "need": chain, "file": file, "fatal": e,
                        "asserts": [{ "assert": "call", "ok": false, "detail": e }],
                    }));
                    continue;
                }
            };
            check(&mut asserts, "契约关(step1)", step1.contract_ok, &step1.contract_detail);
            say(format!(
                ".. {chain}[{idx}] step1 返回（ok={}）",
                step1.contract_ok
            ));
            // 已白底跳过（守卫①）是**正确语义**：输入本来就是要的形态，不产生产物。
            // 这条路径必须显式认账，否则探针把「对的行为」记成 FAIL（N19 容器基线实测：
            // wbf_09/wbf_10 两张纯白图就卡在这条断言上）。
            let skipped_already_white = step1.row.get("evidence")
                .and_then(|e| e.get("skipReason").or_else(|| e.get("skip_reason")))
                .and_then(Value::as_str)
                .map(|s| s == "already_white")
                .unwrap_or(false)
                || step1.row.to_string().contains("already_white");
            if skipped_already_white {
                check(
                    &mut asserts,
                    "已白底跳过(step1，按契约不产生产物)",
                    step1.out_path.is_none(),
                    if step1.out_path.is_none() {
                        "输入已满足目标底色 → result=ok + skipReason=already_white".to_string()
                    } else {
                        format!("声称跳过却落盘了：{}", step1.out_path.as_ref().unwrap().display())
                    },
                );
            } else {
                check(
                    &mut asserts,
                    "产物落盘(step1)",
                    step1.out_path.is_some(),
                    step1
                        .out_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| format!("无产物行：{}", step1.row)),
                );
            }
            let mut cur = step1.out_path.clone();
            steps.push(json!({
                "label": step1.label, "opts": opts, "out": cur.as_ref().map(|p| p.display().to_string()),
                "bytes": step1.bytes, "error": step1.error,
            }));

            // ---- 链式第二步（只有 resize_webp 有）
            if chain == "resize_webp" {
                if let Some(in1) = cur.as_ref() {
                    let conv = json!({ "action": "convert", "convertFormat": "webp" });
                    match run_step(
                        &harness.imaging,
                        &format!("{tag}-2"),
                        in1,
                        &workdir.join("step2"),
                        conv.clone(),
                    ) {
                        Ok(s2) => {
                            check(&mut asserts, "契约关(step2)", s2.contract_ok, &s2.contract_detail);
                            check(
                                &mut asserts,
                                "产物落盘(step2)",
                                s2.out_path.is_some(),
                                s2.out_path
                                    .as_ref()
                                    .map(|p| p.display().to_string())
                                    .unwrap_or_else(|| format!("无产物行：{}", s2.row)),
                            );
                            steps.push(json!({
                                "label": s2.label, "opts": conv,
                                "out": s2.out_path.as_ref().map(|p| p.display().to_string()),
                                "bytes": s2.bytes, "error": s2.error,
                            }));
                            if let (Some(in1), Some(out2)) = (cur.clone(), s2.out_path.clone()) {
                                let d1 = dims(&in1).ok();
                                let d2 = dims(&out2).ok();
                                let fmt = magic_format(&out2);
                                check(&mut asserts, "第二步格式=webp", fmt == "webp", fmt);
                                check(
                                    &mut asserts,
                                    "第二步尺寸=第一步尺寸",
                                    d1.is_some() && d1 == d2,
                                    format!("{d1:?} -> {d2:?}"),
                                );
                                check(
                                    &mut asserts,
                                    "链产物体积>0",
                                    s2.bytes > 0,
                                    format!("{} B", s2.bytes),
                                );
                            }
                            cur = s2.out_path;
                        }
                        Err(e) => check(&mut asserts, "call(step2)", false, e),
                    }
                }
            }

            // ---- 各链自己的硬判据（与 n14 门禁同口径）
            if let Some(out) = cur.clone() {
                match chain {
                    "resize_webp" => {
                        // 第一步：长边 == 1600（expect.out_size），且不放大
                        if let (Some(d1), Some(exp)) =
                            (dims(step1.out_path.as_ref().unwrap_or(&out)).ok(), expect.get("out_size"))
                        {
                            let exp_w = exp[0].as_u64().unwrap_or(0) as u32;
                            let exp_h = exp[1].as_u64().unwrap_or(0) as u32;
                            check(
                                &mut asserts,
                                "缩放后尺寸==expect.out_size",
                                d1 == (exp_w, exp_h),
                                format!("实测 {}x{} expect {exp_w}x{exp_h}", d1.0, d1.1),
                            );
                            let in_max = expect["in_size"][0]
                                .as_u64()
                                .unwrap_or(0)
                                .max(expect["in_size"][1].as_u64().unwrap_or(0));
                            check(
                                &mut asserts,
                                "不放大",
                                (d1.0.max(d1.1) as u64) <= in_max.max(RESIZE_LONG_EDGE),
                                format!("in_max={in_max} out_max={}", d1.0.max(d1.1)),
                            );
                        }
                    }
                    "compress" => {
                        let target = expect["target_max_bytes"].as_u64().unwrap_or(COMPRESS_MAX_BYTES);
                        check(
                            &mut asserts,
                            "体积<=target",
                            step1.bytes <= target,
                            format!("{} B <= {} B", step1.bytes, target),
                        );
                        let want = expect["out_format"].as_str().unwrap_or("jpg");
                        let fmt = magic_format(&out);
                        check(&mut asserts, "格式==expect.out_format", fmt == want, format!("{fmt} vs {want}"));
                    }
                    "white_bg_fill" => {
                        match border_dev_max(&out, BG_BAND_PX) {
                            Ok(dev) => {
                                let lim = expect["out_bg_dev_max"].as_f64().unwrap_or(BG_MAX_DEV);
                                check(
                                    &mut asserts,
                                    "白底4px带偏差<=上限",
                                    dev <= lim,
                                    format!("dev={dev:.2} <= {lim:.2}"),
                                );
                            }
                            Err(e) => check(&mut asserts, "白底4px带偏差", false, e),
                        }
                    }
                    _ => {}
                }
            }

            let ok = asserts
                .iter()
                .all(|a| a["ok"].as_bool().unwrap_or(false));
            if ok {
                pass += 1;
            } else {
                fail += 1;
            }
            say(format!(
                "{} {} {}:{}",
                if ok { "PASS" } else { "FAIL" },
                chain,
                file,
                if ok {
                    "ok".to_string()
                } else {
                    format!("失败断言 {}", serde_json::to_string(&asserts).unwrap_or_default())
                }
            ));
            chain_rows.push(json!({
                "chain": chain, "need": chain, "file": file, "pass": ok, "asserts": asserts,
                "steps": steps, "expect": expect,
            }));
        }
        totals.insert(chain.to_string(), (pass, fail));
    }

    let all_pass = totals.values().all(|(_, f)| *f == 0);
    println!("== 汇总 ==");
    for (chain, (p, f)) in &totals {
        println!("  {chain:<16} PASS {p}  FAIL {f}");
    }
    println!("  耗时 {:.1}s", started.elapsed().as_secs_f64());

    let evidence = json!({
        "ok": all_pass,
        "packs": args.packs.to_string_lossy(),
        "corpus": args.corpus.to_string_lossy(),
        "runtime_python": args.runtime_python.as_ref().map(|p| p.to_string_lossy().to_string()),
        "launcher_dir": args.launcher_dir.as_ref().map(|p| p.to_string_lossy().to_string()),
        "target": { "os": std::env::consts::OS, "arch": std::env::consts::ARCH },
        "totals": totals.iter().map(|(k, (p, f))| json!({"chain": k, "pass": p, "fail": f})).collect::<Vec<_>>(),
        "elapsed_s": started.elapsed().as_secs_f64(),
        "rows": chain_rows,
    });
    if let Some(dir) = args.evidence.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    match serde_json::to_vec_pretty(&evidence) {
        Ok(raw) => {
            if let Err(e) = std::fs::write(&args.evidence, raw) {
                eprintln!("[n15] 证据落盘失败：{e}");
            }
        }
        Err(e) => eprintln!("[n15] 证据序列化失败：{e}"),
    }
    // 关子进程（真机不让残留 python 进程把临时目录锁住）
    harness.imaging.shutdown();
    harness.segment.shutdown();
    harness.upscale.shutdown();
    std::process::exit(if all_pass { 0 } else { 1 });
}
