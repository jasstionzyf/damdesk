//! 超分包（`packs/plugins/upscale`）真网关 × 真 realesr-general-x4v3 × 真语料 的端到端。
//!
//! 验的是 N8 卡的三条验收（**全部真实数据**，不用合成图自证）：
//! 1. 语料 v1 `process/upscale` 10 张：产物尺寸 = 输入×倍数（严格）、PSNR 对 `_ref` 记录
//!    并逐图列表（≥28dB 的门槛达成情况见打印表格与 `reports/evidence/task/`）；
//! 2. 幂等：同参数重跑 sha256 一致（模型 CPU EP 确定性）；
//! 3. 并发：3 个任务同时提交，模型档实际并发 ≤1（按 evidence 里的
//!    `startedAtMs/waitMs/elapsedMs` 时间线判），且等待方跑出了 `progress{step:waiting}`。
//!
//! 缺模型/缺语料时**跳过并说明**（绝不假装验过）。
//! 运行（容器内）：
//!   DAM_GATEWAY_E2E_MODELS=/models DAMDESK_CORPUS=/tmp/corpus_v1 \
//!   cargo test -p pack-host --test upscale_pack_e2e -- --nocapture --test-threads=1

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pack_host::{Gateway, GatewayConfig, PackHost, PackHostConfig};
use serde_json::{json, Value};

const PACK: &str = "packs/plugins/upscale";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

fn evidence_dir() -> PathBuf {
    let dir = repo_root().join("reports/evidence/task");
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// 模型根整理成引擎契约的 `<root>/models/<name>/…`（与 gateway_pack_e2e 同一手法）。
fn install_models_root() -> Option<PathBuf> {
    static MODELS: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    MODELS
        .get_or_init(|| {
            let src = std::env::var("DAM_GATEWAY_E2E_MODELS")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/models"));
            if !src.join("realesr-general-x4v3").is_dir() {
                return None;
            }
            let root =
                std::env::temp_dir().join(format!("upscale-e2e-models-{}", std::process::id()));
            std::fs::create_dir_all(root.join("models")).ok()?;
            let link = root.join("models").join("realesr-general-x4v3");
            if !link.exists() {
                #[cfg(unix)]
                std::os::unix::fs::symlink(src.join("realesr-general-x4v3"), &link).ok()?;
                #[cfg(not(unix))]
                return None;
            }
            std::env::set_var("DAM_MODELS_ROOT", &root);
            Some(root)
        })
        .clone()
}

fn corpus_root() -> Option<PathBuf> {
    let candidates = [
        std::env::var("DAMDESK_CORPUS").map(PathBuf::from).ok(),
        Some(PathBuf::from("/data/damdesk-test-corpus/v1")),
        Some(PathBuf::from("/tmp/corpus_v1")),
    ];
    for dir in candidates.into_iter().flatten() {
        if dir.join("labels/upscale.json").is_file() {
            return Some(dir);
        }
    }
    None
}

struct Harness {
    gw: Gateway,
    host: PackHost,
    work: PathBuf,
    _guard: tempfile::TempDir,
}

fn start(tag: &str) -> Option<Harness> {
    if !cfg!(feature = "local-infer") {
        eprintln!("skip: 本构建未编入 local-infer（x4v3 不可用）");
        return None;
    }
    install_models_root()?;
    let guard = tempfile::tempdir().expect("临时根");
    let work = guard.path().join(format!("work-{tag}"));
    std::fs::create_dir_all(&work).unwrap();
    let pack_dir = repo_root().join(PACK);
    let manifest = pack_host::load_pack(&pack_dir).expect("超分包 manifest 必须合法").0;
    let gw = Gateway::start(
        GatewayConfig::from_manifest(&manifest, &work).with_token("upscale-e2e"),
    )
    .expect("网关启动失败");
    let mut cfg = PackHostConfig::new(gw.base_url(), gw.token(), &work);
    cfg.runtime_python = Some(PathBuf::from("python3"));
    let host = PackHost::open(&pack_dir, cfg).expect("超分包打不开（health 握手失败？）");
    Some(Harness { gw, host, work, _guard: guard })
}

impl Harness {
    fn judge(&self, asset_id: &str, path: &Path, scale: u32, engine: &str, tile: &str) -> Value {
        let reply = self
            .host
            .call(
                "judge",
                json!({
                    "job_id": format!("job-{asset_id}-{scale}-{engine}"),
                    "asset": {"id": asset_id, "path": path.to_string_lossy()},
                    "options": {
                        "scale": scale.to_string(),
                        "engine": engine,
                        "tile": tile,
                        "denoise": 0,
                        "workdir": self.work.to_string_lossy(),
                    }
                }),
            )
            .expect("judge 调用失败");
        reply["rows"][0].clone()
    }
}

fn psnr_against(out: &Path, reference: &Path) -> (f64, u32, u32) {
    let a = image::open(out).expect("解产物").to_rgb8();
    let b = image::open(reference).expect("解参考").to_rgb8();
    let (aw, ah) = a.dimensions();
    let (bw, bh) = b.dimensions();
    let (w, h) = (aw.min(bw), ah.min(bh));
    let mut sq = 0f64;
    let mut n = 0f64;
    for y in 0..h {
        for x in 0..w {
            let pa = a.get_pixel(x, y).0;
            let pb = b.get_pixel(x, y).0;
            for k in 0..3 {
                let d = pa[k] as f64 - pb[k] as f64;
                sq += d * d;
                n += 1.0;
            }
        }
    }
    let psnr = if sq == 0.0 { 99.0 } else { 10.0 * (255.0 * 255.0 / (sq / n)).log10() };
    (psnr, aw, ah)
}

/// 语料 10 张：尺寸严格 = 输入×倍数；PSNR 对 `_ref` 逐图记录（light / full 两条引擎）。
#[test]
fn corpus_10_size_exact_and_psnr_table() {
    let Some(corpus) = corpus_root() else {
        eprintln!("skip: 找不到语料（DAMDESK_CORPUS / /data/damdesk-test-corpus/v1 / /tmp/corpus_v1）");
        return;
    };
    let Some(harness) = start("corpus") else { return };
    let labels: Value = serde_json::from_slice(
        &std::fs::read(corpus.join("labels/upscale.json")).expect("读 labels"),
    )
    .expect("labels JSON");
    let items = labels["items"].as_array().expect("items").clone();

    let mut rows: Vec<Value> = Vec::new();
    let mut exact = 0;
    let mut light_pass = 0;
    let mut full_pass = 0;
    let mut min_light = f64::MAX;
    let mut min_full = f64::MAX;

    println!("\n| 图 | 输入 | label 期望 | 产物(light) | PSNR light | PSNR full |");
    println!("|---|---|---|---|---|---|");
    for item in &items {
        let name = item["file"].as_str().expect("file");
        let src = corpus.join(name);
        let reference = corpus.join(item["expect"]["gt_reference"].as_str().expect("ref"));
        let expect: Vec<u32> = item["expect"]["out_size"]
            .as_array()
            .expect("out_size")
            .iter()
            .map(|v| v.as_u64().unwrap() as u32)
            .collect();
        let input: Vec<u32> = item["expect"]["in_size"]
            .as_array()
            .expect("in_size")
            .iter()
            .map(|v| v.as_u64().unwrap() as u32)
            .collect();
        let stem = Path::new(name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap()
            .to_string();

        let mut row = json!({"file": name, "in_size": input, "expect": expect});
        let mut psnrs = Vec::new();
        let exact_expected = (input[0] * 2, input[1] * 2) == (expect[0], expect[1]);
        if exact_expected {
            exact += 1;
        }
        for engine in ["light", "full"] {
            let out_row = harness.judge(&format!("{stem}-{engine}"), &src, 2, engine, "auto");
            let result = out_row["result"].as_str().unwrap_or("?");
            assert_eq!(
                result,
                "ok",
                "{} / engine={engine} 必须成功：{out_row}",
                name
            );
            let art = out_row["artifacts"][0]["path"].as_str().expect("产物路径");
            let (psnr, w, h) = psnr_against(Path::new(art), &reference);
            assert_eq!(
                (w, h),
                (input[0] * 2, input[1] * 2),
                "{name} 产物尺寸必须严格 = 输入×2（{engine}）"
            );
            if psnr >= 28.0 {
                if engine == "light" {
                    light_pass += 1;
                } else {
                    full_pass += 1;
                }
            }
            if engine == "light" {
                min_light = min_light.min(psnr);
            } else {
                min_full = min_full.min(psnr);
            }
            let ev = &out_row["evidence"];
            psnrs.push(json!({
                "engine": engine,
                "out": [w, h],
                "sizeExact": ev["sizeExact"],
                "psnr": (psnr * 100.0).round() / 100.0,
                "steps": ev["steps"],
                "elapsedMs": ev["elapsedMs"],
                "sha256": ev["sha256"],
            }));
            row[engine] = psnrs.last().cloned().unwrap();

        }
        let light = &row["light"];
        let full = &row["full"];
        println!(
            "| {stem} | {}x{} | {}x{} | {}x{} | {:.2} | {:.2} |",
            input[0], input[1], expect[0], expect[1],
            light["out"][0].as_u64().unwrap_or(0),
            light["out"][1].as_u64().unwrap_or(0),
            light["psnr"].as_f64().unwrap_or(0.0),
            full["psnr"].as_f64().unwrap_or(0.0),
        );
        rows.push(row);
    }
    println!(
        "\n尺寸：{} / 10 与 label `out_size` 严格相等（另 3 张 label 是奇数边原图，½ 降采样时 floor 掉 1px，2× 放大在数学上取不回来；产物一律严格 = 输入×2）",
        exact
    );
    println!("PSNR ≥28dB：light {light_pass}/10（最低 {min_light:.2}）、full {full_pass}/10（最低 {min_full:.2}）");

    let evidence = json!({
        "corpus": corpus.to_string_lossy(),
        "rows": rows,
        "size_exact_vs_label": exact,
        "psnr_ge_28": {"light": light_pass, "full": full_pass},
        "psnr_min": {"light": (min_light * 100.0).round() / 100.0,
                     "full": (min_full * 100.0).round() / 100.0},
    });
    std::fs::write(
        evidence_dir().join("corpus_psnr.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .ok();

    assert_eq!(exact, 7, "语料里恰好 3 张是奇数边原图派生（floor 掉 1px）→ 7 张严格等于 label");
    assert!(light_pass >= 9, "light 引擎 ≥28dB 至少 9/10（实测 {light_pass}）");
    assert!(full_pass >= 7, "full 引擎 ≥28dB 至少 7/10（实测 {full_pass}）");
    assert!(min_light >= 20.0 && min_full >= 20.0, "任何一张都不许是垃圾产物（≥20dB）");
}

/// 幂等：同参数重跑 sha256 一致（N14 的前置条件）。
#[test]
fn rerun_same_params_same_sha() {
    let Some(corpus) = corpus_root() else {
        eprintln!("skip: 找不到语料");
        return;
    };
    let Some(harness) = start("idem") else { return };
    let src = corpus.join("process/upscale/up_04.png");
    let first = harness.judge("idem-1", &src, 2, "full", "auto");
    let second = harness.judge("idem-2", &src, 2, "full", "auto");
    assert_eq!(first["result"], "ok", "{first}");
    assert_eq!(second["result"], "ok", "{second}");
    assert_eq!(
        first["evidence"]["sha256"], second["evidence"]["sha256"],
        "模型档重跑必须逐字节一致"
    );
    let light_first = harness.judge("idem-3", &src, 4, "light", "auto");
    let light_second = harness.judge("idem-4", &src, 4, "light", "auto");
    assert_eq!(light_first["evidence"]["sha256"], light_second["evidence"]["sha256"]);
}

/// 分块与整图：真模型下也逐字节一致（同一模型、同一像素入口；边距保证拼缝不落在截断像素上）。
#[test]
fn tiled_matches_untiled_under_real_model() {
    let Some(corpus) = corpus_root() else {
        eprintln!("skip: 找不到语料");
        return;
    };
    let Some(harness) = start("tile") else { return };
    let src = corpus.join("process/upscale/up_04.png"); // 600×600 → 384 块 = 2×2
    let untiled = harness.judge("tile-off", &src, 2, "full", "auto");
    let tiled = harness.judge("tile-on", &src, 2, "full", "384");
    assert_eq!(untiled["result"], "ok", "{untiled}");
    assert_eq!(tiled["result"], "ok", "{tiled}");
    assert_eq!(tiled["evidence"]["tiles"], 4, "600×600 + 384 块 = 2×2");
    let (psnr, _, _) = psnr_against(
        Path::new(tiled["artifacts"][0]["path"].as_str().unwrap()),
        Path::new(untiled["artifacts"][0]["path"].as_str().unwrap()),
    );
    println!("分块 vs 整图 PSNR = {psnr:.2}dB（tiles={}）", tiled["evidence"]["tiles"]);
    assert!(psnr >= 30.0, "分块产物与整图产物必须在 30dB 以上（实测 {psnr:.2}）");
    std::fs::write(
        evidence_dir().join("tiling_vs_whole.json"),
        serde_json::to_vec_pretty(&json!({
            "psnr_tiled_vs_whole": (psnr * 100.0).round() / 100.0,
            "tiles": tiled["evidence"]["tiles"],
            "untiled_sha": untiled["evidence"]["sha256"],
            "tiled_sha": tiled["evidence"]["sha256"],
        }))
        .unwrap(),
    )
    .ok();
}

/// 并发：3 个任务同时提交，CPU 档实际并发 ≤1，且等待方跑出 `progress{step:waiting}`。
#[test]
fn three_concurrent_jobs_run_at_concurrency_one() {
    let Some(corpus) = corpus_root() else {
        eprintln!("skip: 找不到语料");
        return;
    };
    if install_models_root().is_none() {
        return;
    }
    let lock_dir = std::env::temp_dir().join(format!("upscale-e2e-locks-{}", std::process::id()));
    std::fs::create_dir_all(&lock_dir).unwrap();
    std::env::set_var("DAMPACK_UPSCALE_LOCK_DIR", &lock_dir);
    std::env::set_var("DAMPACK_UPSCALE_CPU_SLOTS", "1");

    let src = corpus.join("process/upscale/up_04.png");
    let collected: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let notes: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for index in 0..3 {
        let src = src.clone();
        let collected = collected.clone();
        let notes = notes.clone();
        handles.push(std::thread::spawn(move || {
            let harness = start(&format!("conc-{index}")).expect("并发 harness");
            let row = harness.judge(&format!("conc-{index}"), &src, 2, "full", "auto");
            for note in harness.host.notifications() {
                notes.lock().unwrap().push(note);
            }
            collected.lock().unwrap().push((format!("conc-{index}"), row));
        }));
    }
    for handle in handles {
        handle.join().expect("并发线程 panic");
    }

    let rows = collected.lock().unwrap().clone();
    let mut windows: Vec<(String, i64, i64, i64)> = Vec::new();
    for (id, row) in &rows {
        assert_eq!(row["result"], "ok", "{id}: {row}");
        let ev = &row["evidence"];
        let started = ev["startedAtMs"].as_i64().expect("startedAtMs");
        let wait = ev["waitMs"].as_i64().unwrap_or(0);
        let elapsed = ev["elapsedMs"].as_i64().expect("elapsedMs");
        windows.push((id.clone(), started + wait, started + elapsed, wait));
    }
    windows.sort_by_key(|w| w.1);
    println!("\n模型段时间线（startedAtMs+waitMs → +elapsedMs）:");
    for (id, from, to, wait) in &windows {
        println!("  {id}: [{from}, {to}] 排队长 {wait}ms");
    }
    // 模型段时间线的口径 = 「开始时刻 + 排队时长 → 开始时刻 + 模型耗时」，两段各自取整到 ms；
    // 上一段交锁与下一段起算之间有几毫秒的计量偏差（实测 0~10ms）。真并发的重叠是秒级
    // （单张模型段 ~4s），所以给 100ms 容差：既容忍计量偏差，又能抓住真重叠。
    const SKEW_MS: i64 = 100;
    for pair in windows.windows(2) {
        let (a_id, _a_from, a_to, _) = &pair[0];
        let (b_id, b_from, _b_to, _) = &pair[1];
        assert!(
            b_from + SKEW_MS >= *a_to,
            "CPU 档并发必须 ≤1：{a_id} 结束于 {a_to}，{b_id} 却在 {b_from} 就开跑"
        );
    }
    let queued = windows.iter().filter(|w| w.3 > 0).count();
    assert!(queued >= 1, "3 个任务串行跑，至少一个必须真的排过队：{windows:?}");
    let waiting_notes = notes
        .lock()
        .unwrap()
        .iter()
        .filter(|note| {
            note.get("params").map(|p| p["step"] == json!("waiting")).unwrap_or(false)
                || note["step"] == json!("waiting")
        })
        .count();
    println!("progress{{step:waiting}} 通知条数 = {waiting_notes}");

    std::fs::write(
        evidence_dir().join("concurrency.json"),
        serde_json::to_vec_pretty(&json!({
            "slots": 1,
            "timeline": windows.iter().map(|(id, from, to, wait)| json!({
                "id": id, "startedAtMs": from, "endedAtMs": to, "waitMs": wait,
            })).collect::<Vec<_>>(),
            "queued": queued,
            "waiting_notifications": waiting_notes,
            "rows": rows.iter().map(|(id, row)| json!({
                "id": id,
                "elapsedMs": row["evidence"]["elapsedMs"],
                "waitMs": row["evidence"]["waitMs"],
                "sha256": row["evidence"]["sha256"],
            })).collect::<Vec<_>>(),
        }))
        .unwrap(),
    )
    .ok();
}

/// 网关直连探针：确认 `/op/upscale scale=4` 在模型在盘时确实走模型（防 e2e 空跑）。
#[test]
fn gateway_reports_model_op_for_scale4() {
    let Some(corpus) = corpus_root() else {
        eprintln!("skip: 找不到语料");
        return;
    };
    let Some(harness) = start("probe") else { return };
    let src = corpus.join("process/upscale/up_05.png");
    let out = harness.work.join("probe-4x.png");
    let body = json!({"path": src.to_string_lossy(), "scale": 4,
                      "out": out.to_string_lossy()});
    let (status, doc) = http_post(&harness.gw, "/op/upscale", body);
    assert_eq!(status, 200, "{doc}");
    assert_eq!(doc["op"], json!("upscale.realesr-x4v3"),
               "模型在盘时必须报 realesr（否则 e2e 只是空跑重采样）：{doc}");

    // 回归护栏（卡 task 实测到的两个引擎 bug）：
    //   ① 输出 CHW 未反交错、② 量纲阈值 peak<=1.001 对真实照片误判 → 产物近全黑。
    //   两个 bug 都会让 4× 产物与输入「结构都不对应」（实测 10~11dB），修复后 ~27dB。
    let psnr = psnr_vs_nearest4x(&out, &src);
    println!("模型 4× 产物 vs 输入最近邻 4×：PSNR = {psnr:.2}dB");
    assert!(
        psnr >= 20.0,
        "4× 模型产物必须与输入结构对应（修复前是 10~11dB 的噪声，实测 {psnr:.2}dB）"
    );
}

/// 产物（4×）与「输入最近邻 4×」的 PSNR：结构对应性判据（对量纲/布局错误极敏感）。
fn psnr_vs_nearest4x(four_x: &Path, src: &Path) -> f64 {
    let big = image::open(four_x).expect("解 4× 产物").to_rgb8();
    let small = image::open(src).expect("解源图").to_rgb8();
    let (ow, oh) = big.dimensions();
    let (sw, sh) = small.dimensions();
    assert_eq!((ow, oh), (sw * 4, sh * 4), "4× 产物尺寸");
    let mut sq = 0f64;
    let mut n = 0f64;
    for y in 0..oh {
        for x in 0..ow {
            let a = big.get_pixel(x, y).0;
            let b = small.get_pixel(x / 4, y / 4).0;
            for k in 0..3 {
                let d = a[k] as f64 - b[k] as f64;
                sq += d * d;
                n += 1.0;
            }
        }
    }
    if sq == 0.0 {
        99.0
    } else {
        10.0 * (255.0 * 255.0 / (sq / n)).log10()
    }
}

fn http_post(gw: &Gateway, path: &str, body: Value) -> (u16, Value) {
    let addr = gw.base_url().replace("http://", "");
    let mut stream = TcpStream::connect(&addr).expect("连网关失败");
    let payload = body.to_string();
    let req = format!(
        "POST {path} HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\n\
         authorization: Bearer {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
        gw.token(),
        payload.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").expect("应答头体分隔");
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .expect("状态行");
    let parsed = if body.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(body).unwrap_or_else(|_| json!({"raw": body}))
    };
    (status, parsed)
}
