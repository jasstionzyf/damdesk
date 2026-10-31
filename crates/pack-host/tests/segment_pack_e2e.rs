//! 抠图/换背景包（`packs/plugins/segment`）真网关 × 真 u2net × 真语料 的端到端。
//!
//! 验的是 N7 卡的三条验收（全部用**真实语料**，不用合成图自证）：
//! 1. 语料 v1 `white_bg_fill` 10 张：输出边框带偏差 ≤3（label `out_bg_dev_max`）、
//!    主体面积损失 ≤2%（输入/输出的 u2net `fg_ratio` 对比）、幂等（同参数重跑同字节）；
//! 2. 另取 3 张出透明 PNG：alpha 通道存在、边缘无白边（半透明像素不得是纯白）；
//! 3. 处理类包的产物形状：只落 `export_file`，不掺 verdict，中间件不残留。
//!
//! 缺模型/缺语料时**跳过并说明**（绝不假装验过）。
//! 运行（容器内）：
//!   DAM_GATEWAY_E2E_MODELS=/models DAMDESK_CORPUS=/tmp/damdesk-corpus/v1 \
//!   cargo test -p pack-host --test segment_pack_e2e -- --nocapture

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};

use image::GenericImageView;
use pack_host::{Gateway, GatewayConfig, PackHost, PackHostConfig};
use serde_json::{json, Value};

const PACK: &str = "packs/plugins/segment";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

/// 模型根整理成引擎契约的 `<root>/models/<name>/…`（与 gateway_pack_e2e 同一手法）。
fn install_models_root() -> Option<PathBuf> {
    static MODELS: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    MODELS
        .get_or_init(|| {
            let src = std::env::var("DAM_GATEWAY_E2E_MODELS")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/models"));
            if !src.join("u2net").is_dir() {
                return None;
            }
            let root = std::env::temp_dir().join(format!("segment-e2e-models-{}", std::process::id()));
            std::fs::create_dir_all(root.join("models")).ok()?;
            let link = root.join("models").join("u2net");
            if !link.exists() {
                #[cfg(unix)]
                std::os::unix::fs::symlink(src.join("u2net"), &link).ok()?;
                #[cfg(not(unix))]
                return None;
            }
            std::env::set_var("DAM_MODELS_ROOT", &root);
            Some(root)
        })
        .clone()
}

fn corpus_root() -> Option<PathBuf> {
    let dir = std::env::var("DAMDESK_CORPUS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/data/damdesk-test-corpus/v1"));
    if dir.join("labels/white_bg_fill.json").is_file() {
        Some(dir)
    } else {
        None
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
        serde_json::from_str(body).unwrap_or_else(|_| json!({ "raw": body }))
    };
    (status, parsed)
}

struct Harness {
    gw: Gateway,
    host: PackHost,
    _work: tempfile::TempDir,
}

fn start() -> Option<Harness> {
    if !cfg!(feature = "local-infer") {
        eprintln!("skip: 本构建未编入 local-infer（u2net 不可用）");
        return None;
    }
    install_models_root()?;
    let work = tempfile::tempdir().expect("临时工作目录");
    let pack_dir = repo_root().join(PACK);
    let manifest = pack_host::load_pack(&pack_dir).expect("抠图包 manifest 必须合法").0;
    let gw = Gateway::start(
        GatewayConfig::from_manifest(&manifest, work.path()).with_token("segment-e2e"),
    )
    .expect("网关启动失败");
    let mut cfg = PackHostConfig::new(gw.base_url(), gw.token(), work.path());
    cfg.runtime_python = Some(PathBuf::from("python3"));
    let host = PackHost::open(&pack_dir, cfg).expect("抠图包打不开（health 握手失败？）");
    Some(Harness { gw, host, _work: work })
}

impl Harness {
    fn judge(&self, asset_id: &str, path: &str, options: Value) -> Value {
        let workdir = self._work.path().join("out");
        let mut opts = options;
        opts["workdir"] = json!(workdir.to_string_lossy());
        let params = json!({
            "job_id": "job-n7",
            "asset": {"id": asset_id, "path": path},
            "options": opts,
        });
        let reply = self.host.call("judge", params).expect("judge 调用失败");
        let rows = reply.get("rows").and_then(Value::as_array).expect("缺 rows");
        assert_eq!(rows.len(), 1, "一个资产一次调用只应回一行");
        rows[0].clone()
    }

    fn fg_ratio(&self, path: &str) -> f64 {
        let (status, body) = http_post(&self.gw, "/detect/u2net", json!({ "path": path }));
        assert_eq!(status, 200, "u2net 失败：{body}");
        body["fg_ratio"].as_f64().expect("fg_ratio")
    }

    /// 边框带白度：**直接用引擎的 `white_edge` 判定器**（band=4，与 R2 同一份实现）。
    /// 不走网关端点：包自己也调 `/detect/white_stats` 时，测试要的是**独立重算**，
    /// 拿同一条调用链的数字对账等于自己给自己作证（K07-FIX-N 守卫②的核对口径）。
    fn edge_dev(&self, path: &str) -> (f64, f64, i64) {
        let img = image::open(path).expect("解产物").to_rgb8();
        let (w, h) = img.dimensions();
        let report = engine_classic::detectors::white_edge(
            img.as_raw(),
            w,
            h,
            &engine_classic::detectors::WhiteEdgeParams { max_dev: 3.0, band: 4 },
        )
        .expect("white_edge 计算失败");
        (report.mean_dev, report.max_dev, report.band as i64)
    }
}

fn sha256_file(path: &str) -> String {
    let mut buf = Vec::new();
    let mut file = std::fs::File::open(path).expect("产物必须存在");
    file.read_to_end(&mut buf).expect("读产物");
    // 复用 core-hash（宿主同一份实现）
    core_hash::sha256_hex(&buf)
}

/// 主体完整度用**直接像素对账**（不用「重跑一次 u2net」，见测试内注释）。
/// 返回 `(掩码主体像素数, 与源图差 > tol 的像素数)`。
fn damage_count(mask: &image::GrayImage, out: &image::RgbImage, src: &image::RgbImage,
                tol: i32) -> (usize, usize) {
    let (w, hgt) = src.dimensions();
    assert_eq!(mask.dimensions(), src.dimensions(), "掩码必须与源图同尺寸");
    assert_eq!(out.dimensions(), src.dimensions(), "产物必须与源图同尺寸");
    let (mut subject, mut damaged) = (0usize, 0usize);
    for y in 0..hgt {
        for x in 0..w {
            if mask.get_pixel(x, y)[0] < 128 {
                continue;
            }
            subject += 1;
            let o = out.get_pixel(x, y);
            let s = src.get_pixel(x, y);
            let diff = (0..3)
                .map(|i| (o[i] as i32 - s[i] as i32).unsigned_abs() as i32)
                .max()
                .unwrap_or(0);
            if diff > tol {
                damaged += 1;
            }
        }
    }
    (subject, damaged)
}

/// 源图的**基线重编码**：同一张图经宿主 `/op/crop`（全框）压成 JPEG。
/// 用它量出「这个质量档本身就有的误差」，才知道产物里哪些差异是本包造成的。
fn baseline_jpeg(h: &Harness, src_path: &str, tag: &str) -> String {
    let (w, hgt) = image::open(src_path).expect("解源图").dimensions();
    let dir = h._work.path().join("baseline");
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join(format!("{tag}.jpg"));
    let (status, body) = http_post(&h.gw, "/op/crop", json!({
        "path": src_path, "box": [0, 0, w, hgt], "out": out.to_string_lossy(),
    }));
    assert_eq!(status, 200, "基线重编码失败：{body}");
    out.to_string_lossy().to_string()
}

#[test]
fn white_bg_fill_corpus_all_pass_and_idempotent() {
    let Some(h) = start() else { return };
    let Some(corpus) = corpus_root() else {
        eprintln!("skip: 语料 v1 不在（DAMDESK_CORPUS）—— 未验 = 不算过");
        return;
    };
    let labels: Value = serde_json::from_str(
        &std::fs::read_to_string(corpus.join("labels/white_bg_fill.json")).expect("读 labels"),
    )
    .expect("labels JSON");
    let items = labels["items"].as_array().expect("items");
    assert_eq!(items.len(), 10, "white_bg_fill 语料应为 10 张");

    let mut failures: Vec<String> = Vec::new();
    let mut table: BTreeMap<String, Value> = BTreeMap::new();
    let mut produced = 0usize;
    let mut skipped = 0usize;

    for item in items {
        let rel = item["file"].as_str().expect("file");
        let file = corpus.join(rel);
        let path = file.to_string_lossy().to_string();
        let name = file.file_name().unwrap().to_string_lossy().to_string();
        let out_dev_max = item["expect"]["out_bg_dev_max"].as_f64().unwrap_or(3.0);

        let row = h.judge(&name, &path, json!({ "bgMode": "white", "feather": 2 }));
        if row["result"] != "ok" {
            failures.push(format!("{name}: result={}", row["result"]));
            continue;
        }
        if row.get("verdict").is_some() {
            failures.push(format!("{name}: 处理类包不许回 verdict"));
        }
        // 守卫②：目标白底 + 输入本来就是白底 → 合法跳过（result=ok + skipReason=already_white + 零产物）。
        // 「跳过」不许当通关用，所以三条都要核：①语料 label 说输入已是白底；
        // ②测试**自己重算**输入 4px 边框带偏差 ≤3（不信包回的数字）；③跳过没有产物、二次跑一样跳过。
        if let Some(reason) = row.get("skipReason").and_then(Value::as_str) {
            let label_in = item["expect"]["in_bg_dev_max"].as_f64().unwrap_or(f64::MAX);
            let (_mean, in_worst, _band) = h.edge_dev(&path);
            if reason != "already_white" {
                failures.push(format!("{name}: 意外的 skipReason={reason}"));
                continue;
            }
            if !row.get("artifacts").and_then(Value::as_array).map(|a| a.is_empty()).unwrap_or(true) {
                failures.push(format!("{name}: 跳过却带产物（跳过 = 不产生新工作版本）"));
            }
            if label_in > 3.0 || in_worst > 3.0 {
                failures.push(format!(
                    "{name}: 输入不是白底却跳过（label in_bg_dev_max={label_in}, 实测 {in_worst}）"));
            }
            let again = h.judge(&name, &path, json!({ "bgMode": "white", "feather": 2 }));
            if again.get("skipReason").and_then(Value::as_str) != Some("already_white") {
                failures.push(format!("{name}: 重跑没跳过，跳过不稳定：{again}"));
            }
            skipped += 1;
            let ev = &row["evidence"];
            let entry = json!({
                "file": name, "skipped": true, "skipReason": reason,
                "labelInBorderDev": label_in, "inputBorderDevMeasured": in_worst,
                "bgBlockMeanDev": ev["bgBlockMeanDev"], "bgBlockBandPx": ev["bgBlockBandPx"],
                "artifacts": row.get("artifacts").cloned().unwrap_or(json!([])),
                "stableOnRerun": again.get("skipReason").and_then(Value::as_str) == Some("already_white"),
                "elapsedMs": ev["elapsedMs"],
            });
            println!("SEG_METRIC {}", serde_json::to_string(&entry).unwrap());
            table.insert(name, entry);
            continue;
        }
        let art = &row["artifacts"][0];
        assert_eq!(art["kind"], "export_file", "产物形态");
        let out_path = art["path"].as_str().expect("artifact path").to_string();
        let sha = sha256_file(&out_path);
        if art["sha256"].as_str() != Some(sha.as_str()) {
            failures.push(format!("{name}: 回填 sha256 与文件不一致"));
        }

        let fg_in = h.fg_ratio(&path);
        let fg_out = h.fg_ratio(&out_path);
        let (mean_dev, worst_dev, band) = h.edge_dev(&out_path);
        // 主体完整度用**直接像素对账**，不用「重跑一次 u2net」：
        //   派生语料是「白底商品图贴到有色背景」，原图上的掩码把整块贴图矩形（含留白）都算主体
        //   （wbf_07 输入 66% vs 输出 28%），重分割比的其实是「不同背景下的显著性定义」，
        //   不是「抠图吃掉主体没有」。这里按原掩码逐像素比对，并减去**基线重编码**的 JPEG 误差：
        //   产物里多出来的差异才是本包刷白刷掉的像素。
        let (status, body) = http_post(&h.gw, "/detect/u2net", json!({ "path": path }));
        assert_eq!(status, 200, "u2net 失败：{body}");
        let mask = image::open(body["mask_path"].as_str().unwrap()).expect("解宿主掩码").to_luma8();
        let src_img = image::open(&path).expect("解原图").to_rgb8();
        let out_img = image::open(&out_path).expect("解产物").to_rgb8();
        let base = image::open(baseline_jpeg(&h, &path, &name)).expect("解基线").to_rgb8();
        let (subject_px, damaged_px) = damage_count(&mask, &out_img, &src_img, 12);
        let (_base_subject, base_damaged) = damage_count(&mask, &base, &src_img, 12);
        let extra_damaged = damaged_px.saturating_sub(base_damaged);
        let subject_loss = if subject_px > 0 {
            extra_damaged as f64 / subject_px as f64
        } else {
            0.0
        };

        // 幂等：同参数重跑 → 同字节
        let again = h.judge(&name, &path, json!({ "bgMode": "white", "feather": 2 }));
        let sha2 = again["artifacts"][0]["sha256"].as_str().unwrap_or("").to_string();
        let idempotent = sha == sha2;

        if worst_dev > out_dev_max {
            failures.push(format!("{name}: 边框带偏差 {worst_dev:.2} > {out_dev_max}"));
        }
        if subject_loss > 0.02 {
            failures.push(format!(
                "{name}: 主体像素被刷掉 {:.2}% > 2%（超出基线 {extra_damaged} / 主体 {subject_px}；\
                 产物 {damaged_px} 基线 {base_damaged}）", subject_loss * 100.0));
        }
        if !idempotent {
            failures.push(format!("{name}: 重跑 sha256 不一致（{sha} vs {sha2}）"));
        }
        produced += 1;
        let ev = &row["evidence"];
        let entry = json!({
            "file": name, "out": out_path, "sha256": sha, "outDevMax": out_dev_max,
            "cropArea": ev["cropArea"], "opaquePixels": ev["opaquePixels"],
            "subjectPixels": ev["subjectPixels"], "imagePixels": ev["width"].as_i64().unwrap_or(0) * ev["height"].as_i64().unwrap_or(0),
            "meanDev": mean_dev, "worstDev": worst_dev, "band": band,
            "maskSubjectPx": subject_px, "damagedSubjectPx": damaged_px,
            "baselineDamagedPx": base_damaged, "extraDamagedPx": extra_damaged,
            "subjectLossPct": (subject_loss * 10000.0).round() / 100.0,
            "fgIn": fg_in, "fgOut": fg_out,
            "idempotent": idempotent, "elapsedMs": ev["elapsedMs"],
        });
        println!("SEG_METRIC {}", serde_json::to_string(&entry).unwrap());
        table.insert(name, entry);
    }
    assert!(failures.is_empty(), "white_bg_fill 有 FAIL：\n{}", failures.join("\n"));
    let expected_skips = items
        .iter()
        .filter(|i| i["expect"]["in_bg_dev_max"].as_f64().unwrap_or(255.0) <= 3.0)
        .count();
    assert_eq!(
        skipped, expected_skips,
        "跳过张数必须等于语料里「本来就白底」的张数（{expected_skips}）：多跳 = 漏刷，少跳 = 白跑"
    );
    assert_eq!(produced + skipped, 10, "10 张要么产出产物、要么合法跳过");

    // 中间件不残留：assets 数量 == 产物数量（每资产一个 jpg）
    let out_dir = h._work.path().join("out");
    let files: Vec<String> = std::fs::read_dir(&out_dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
        .collect();
    assert_eq!(files.len(), produced, "out 目录里的产物数应等于「真的刷了」的张数，实得 {files:?}");
    assert!(files.iter().all(|f| f.ends_with(".jpg")), "白底模式产物应为 jpg：{files:?}");
    println!("SEG_SUMMARY {}", json!({
        "need": "white_bg_fill", "pass": produced + skipped, "fail": 0,
        "produced": produced, "skippedAlreadyWhite": skipped,
    }));
}

#[test]
fn second_application_on_own_output_is_skipped() {
    // 真语料里最狠的那张：N14 旧行为下 wbf_07 的产物被「再施加一次」刷掉 **28.5%** 像素
    // （主体左下底座被背景吃掉）。守卫② 上线后，白底化对白底产物必须是 no-op：
    // 第二次直接 `skipReason=already_white`、零产物、原产物 sha256 不变。
    let Some(h) = start() else { return };
    let Some(corpus) = corpus_root() else {
        eprintln!("skip: 语料 v1 不在（DAMDESK_CORPUS）");
        return;
    };
    let file = corpus.join("process/white_bg_fill/wbf_07.jpg");
    let path = file.to_string_lossy().to_string();
    let first = h.judge("wbf_07", &path, json!({ "bgMode": "white", "feather": 2 }));
    assert_eq!(first["result"], "ok", "{first}");
    assert!(first.get("skipReason").is_none(),
            "输入是「白底图贴到有色背景」的派生图，第一次必须真刷，不许跳过：{first}");
    let out_path = first["artifacts"][0]["path"].as_str().expect("artifact path").to_string();
    let sha_before = sha256_file(&out_path);

    let second = h.judge("wbf_07-again", &out_path, json!({ "bgMode": "white", "feather": 2 }));
    assert_eq!(second["result"], "ok", "{second}");
    assert_eq!(second["skipReason"], "already_white",
               "白底化对白底产物必须是 no-op（旧行为会重刷、吃掉主体）：{second}");
    let empty = second.get("artifacts").and_then(Value::as_array)
        .map(|a| a.is_empty()).unwrap_or(true);
    assert!(empty, "跳过 = 不产生新工作版本：{second}");
    assert_eq!(sha256_file(&out_path), sha_before, "原产物一个字节都不许动");
    let ev = &second["evidence"];
    assert_eq!(ev["inputBorderDev"], 0.0, "产物边框带必须实测为纯白：{ev}");
    println!("SEG_METRIC {}", json!({
        "case": "second_application_on_own_output",
        "file": "wbf_07",
        "firstSha256": sha_before,
        "secondSkipReason": second["skipReason"],
        "secondArtifacts": second.get("artifacts").cloned().unwrap_or(json!([])),
        "inputBorderDev": ev["inputBorderDev"],
        "bgBlockMeanDev": ev["bgBlockMeanDev"],
        "bandPx": ev["inputBorderBandPx"],
    }));
}

#[test]
fn transparent_output_has_alpha_and_no_white_halo() {
    let Some(h) = start() else { return };
    let Some(corpus) = corpus_root() else {
        eprintln!("skip: 语料 v1 不在（DAMDESK_CORPUS）");
        return;
    };
    // 取三张不同尺寸/背景色的真实图（不是合成图）。
    // 注意：wbf_09 / wbf_10 是**原生白底图**（label in_bg_dev_max=0）——它们在白底模式下会被
    // 守卫②跳过，这里出透明 PNG 仍然必须真跑：守卫② 只作用于「目标底色是纯白」的那件事。
    let picks = ["process/white_bg_fill/wbf_09.jpg",
                 "process/white_bg_fill/wbf_10.jpg",
                 "process/white_bg_fill/wbf_01.jpg"];
    let mut checked = 0usize;
    for rel in picks {
        let file = corpus.join(rel);
        let path = file.to_string_lossy().to_string();
        let row = h.judge(file.file_name().unwrap().to_str().unwrap(), &path,
                          json!({ "bgMode": "transparent", "feather": 4 }));
        assert_eq!(row["result"], "ok", "{rel}: {row}");
        let out_path = row["artifacts"][0]["path"].as_str().unwrap().to_string();
        assert!(out_path.ends_with(".png"), "透明输出必须是 PNG：{out_path}");

        let img = image::open(&out_path).expect("解透明产物").to_rgba8();
        let (w, hh) = img.dimensions();
        let source = image::open(&file).expect("解原图").to_rgb8();
        assert_eq!((w, hh), source.dimensions(), "透明产物必须是整图尺寸");
        let mut partial = 0usize;
        let mut opaque = 0usize;
        let mut transparent = 0usize;
        let mut whitened = 0usize;   // 半透明像素的 RGB 被背景「漂白」的个数
        let mut max_channel_diff = 0u32;
        let mut halo = 0usize;       // 原图自身边缘就偏白的个数（上传图自带，不是本包造的）
        for y in 0..hh {
            for x in 0..w {
                let p = img.get_pixel(x, y);
                let a = p[3];
                if a == 0 {
                    transparent += 1;
                } else if a == 255 {
                    opaque += 1;
                } else {
                    partial += 1;
                    // 铁证：alpha 斜坡像素的 RGB 必须**逐字节等于原图**（非预乘、未被背景污染）
                    let s = source.get_pixel(x, y);
                    // 判「被背景漂白」：三个通道**同时**比源图更亮（±1 是两条解码路径的舍入差）
                    if (0..3).all(|i| p[i] as i32 > s[i] as i32 + 1) {
                        whitened += 1;
                        if whitened <= 5 {
                            eprintln!("WHITEN {rel} ({x},{y}) out={:?} src={:?} a={a}",
                                      (p[0], p[1], p[2]), (s[0], s[1], s[2]));
                        }
                    }
                    let dmax = (0..3)
                        .map(|i| (p[i] as i32 - s[i] as i32).unsigned_abs())
                        .max()
                        .unwrap_or(0);
                    max_channel_diff = max_channel_diff.max(dmax);
                    if s[0] >= 250 && s[1] >= 250 && s[2] >= 250 {
                        halo += 1;
                    }
                }
            }
        }
        assert!(transparent > 0, "{rel}: 背景必须透明");
        assert!(opaque > 0, "{rel}: 主体必须不透明");
        assert!(partial > 0, "{rel}: 边缘必须有 alpha 斜坡（抗锯齿）");
        assert_eq!(whitened, 0,
                   "{rel}: {whitened} 个半透明像素被背景刷亮（白边根因）");
        assert!(max_channel_diff <= 3,
                "{rel}: 半透明像素与源图最大通道差 {max_channel_diff} > 3（RGB 被污染）");
        println!("SEG_METRIC {}", json!({
            "file": rel, "mode": "transparent", "width": w, "height": hh,
            "opaque": opaque, "partial": partial, "transparent": transparent,
            "whitenedPixels": whitened, "maxChannelDiff": max_channel_diff,
            "sourceSideNearWhiteEdge": halo,
        }));
        checked += 1;
    }
    assert_eq!(checked, 3, "必须验满 3 张透明输出");
}

#[test]
fn bad_params_are_rejected_not_silently_ignored() {
    let Some(h) = start() else { return };
    let Some(corpus) = corpus_root() else { return };
    let path = corpus.join("process/white_bg_fill/wbf_10.jpg").to_string_lossy().to_string();
    let workdir = h._work.path().join("out");
    let params = json!({
        "job_id": "job-n7-bad",
        "asset": {"id": "bad", "path": path},
        "options": {"bgMode": "purple", "workdir": workdir.to_string_lossy()},
    });
    let err = h.host.call("judge", params).expect_err("非法 bgMode 必须报错");
    let text = format!("{err}");
    assert!(text.contains("-32003") || text.contains("参数非法"), "错误码应为 -32003：{text}");
    println!("SEG_METRIC {}", json!({"case": "bad_bg_mode", "error": text}));
}
