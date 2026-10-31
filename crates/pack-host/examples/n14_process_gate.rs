//! N14 process 门禁：语料 v1 的 **7 个处理 need × 10 张**，一次跑完、逐项断言、可复现。
//!
//! 与 N6a/N6b/N6c/N7/N8 分组验收的分工：那几张卡各自证「这一组算法对不对」；
//! 本卡证「7 个处理 need 在**同一条宿主路径**上一起发货时全部达标」——
//! 一律走 [`PackHost`]（真子进程 + 真能力网关 + 真模型 + 真语料），
//! 每行再进 `result::parse_row(kinds=[Process])`：
//!
//! - 产物只准两种去处（`export_file{path,sha256}` / `working_version{assetId,versionId}`），
//!   插件不许自建目录树、不许落平行集合 —— 被拒即本行 FAIL（`row_artifact_invalid`）；
//! - 处理类插件回 `verdict` 一律拒收（`plugin_verdict_not_allowed`），本卡单独贴一次原始拒收；
//! - 同参数两趟跑（pass A / pass B，各自独立工作目录）→ 产物 sha256 逐张对账。
//!
//! 逐 need 断言（卡面口径，阈值不许改）：
//!
//! | need | 断言 |
//! |---|---|
//! | `resize` | 输出尺寸 == `expect.out_size`；两条边都不放大 |
//! | `convert` | 魔数格式 == `expect.out_format`；尺寸不变；alpha 源转 png 保 alpha |
//! | `compress` | 产物 ≤ 200KB；SSIM（参考图按产物尺寸重采样）≥ 0.95 |
//! | `watermark` | 水印墨迹框落在 `expect.anchor` 的角/中心窗内；实测不透明度与参数一致 |
//! | `white_bg_fill` | 4px 边框带偏差 ≤ 3（对纯白）；已白底输入（守卫②）跳过：`result=ok` + `skipReason=already_white` + 零产物，且门禁自己重算输入 4px 带 ≤3 才认；把自己的产物再跑一次必须跳过（幂等） |
//! | `smart_crop` | 输出比例 == `expect.aspect`（1.5px 容差）；label 主体框映射后完整落在产物内（±`subject_bbox_tol_px`=2px 标注容差） |
//! | `upscale` | 硬门 = 输出尺寸 == `expect.out_size`（严格）+ 3 张人工抽检（无重影/无伪纹理/边缘不发灰，结论见报告 §6.2）；指标三列（插件 PSNR / Lanczos 2× 基线 / Δ）逐图照实记录；红线 `ΔPSNR ≥ -3dB` **且** `ΔSSIM ≥ -0.02` 逐图计算，v1 语料上只作观测（`redlineArmed=false`），v2+ 自动 arm 成硬门 |
//!
//! 用法（dam-dev 容器内）：
//!
//! ```text
//! DAM_MODELS_ROOT=/ CARGO_TARGET_DIR=/cargo-target/task \
//!   cargo run -q --release -p pack-host --example n14_process_gate -- \
//!     --corpus /data/damdesk-test-corpus/v1 \
//!     --work-dir /tmp/n14-gate --evidence reports/evidence/task/process_gate.json
//! ```
//!
//! 退出码：0 = 70 项全 PASS；1 = 有 FAIL（逐项表格照样打印，evidence 里带 why）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use image::imageops::{resize as img_resize, FilterType};
use image::{DynamicImage, GenericImageView};
use serde_json::{json, Value};

use pack_host::result::{parse_row, PluginKind};
use pack_host::{load_pack, Gateway, GatewayConfig, PackHost, PackHostConfig};

/// 一条链上的三个处理包；ecommerce/realestate 是判定类，不在本卡。
const PACK_IMAGING: &str = "packs/plugins/imaging";
const PACK_SEGMENT: &str = "packs/plugins/segment";
const PACK_UPSCALE: &str = "packs/plugins/upscale";

/// 能力并集（三个包声明的合集）——一条网关同时喂三个包，缺一个端点就是 403。
const CAPABILITIES: [&str; 7] = [
    "io.read",
    "detect.u2net",
    "detect.white",
    "op.geometric",
    "op.overlay",
    "op.generative",
    "report.emit",
];

/// 7 个处理 need（顺序 = 卡面顺序 = 表格顺序）。
const NEEDS: [&str; 7] = [
    "resize",
    "convert",
    "compress",
    "watermark",
    "white_bg_fill",
    "smart_crop",
    "upscale",
];

const RESIZE_LONG_EDGE: u64 = 1600;
const COMPRESS_MAX_BYTES: u64 = 204_800;
const COMPRESS_SSIM_MIN: f64 = 0.95;
const WATERMARK_OPACITY: f64 = 0.35;
const WATERMARK_TEXT: &str = "DAMDesk";
/// 水印边距/字号：宿主 `/op/watermark` 的默认口径（margin=16、fontSize=48）。
/// 判「贴角」时用 `margin + font/2` 当容差（字形自带行内留白），不是随手放的数。
const WATERMARK_MARGIN: f64 = 16.0;
const WATERMARK_FONT_SIZE: f64 = 48.0;
const BG_BAND_PX: u32 = 4;
const BG_MAX_DEV: f64 = 3.0;
const ASPECT_TOL_PX: f64 = 1.5;
/// `smart_crop` 的 label 标注容差缺省值（语料 v1.1 的 `subject_bbox_tol_px`，边缘鲁棒估计 ±2px）。
const SUBJECT_BBOX_TOL_PX: f64 = 2.0;
/// 超分判据（2026-10-23 裁决第二次修订）：
///
/// **硬门只有两条** —— ① 产物尺寸 == `expect.out_size`（严格）；② 人工抽检 3 张（含 up_01）
/// 无重影 / 无伪纹理 / 边缘不发灰（结论写进报告 §6.2，是人的判定，不在代码里）。
///
/// **指标三列**（插件 PSNR/SSIM、同尺寸 Lanczos 2× 基线、Δ）逐图照实记录，**只作观测值**，
/// 不作通过门槛；「不崩坏红线」= `ΔPSNR ≥ -3dB` **且** `ΔSSIM ≥ -0.02`，逐图算出来记在
/// `items[].metrics.redline*`。
///
/// 红线在**语料 v1 上不参与 PASS/FAIL**：v1 的超分派生链是「原图缩到 1/2」再放大回去的干净
/// 回环，`_ref` 就是原图，Lanczos 2× 几乎就是这条链的逆过程（基线 PSNR 天然贴天花板），
/// 生成式引擎必然 PSNR 输 —— 指标要到 v2「真实退化源」语料才可作门槛。语料版本非 `v1*`
/// 时自动 arm（evidence `params.upscale_metric_gate_armed=true`），届时红线进硬门。
const UPSCALE_REDLINE_DB: f64 = 3.0;
const UPSCALE_REDLINE_SSIM: f64 = 0.02;

/// 红线是否 arm 成硬门：main 里按语料版本设一次（v1* → false）。
static UPSCALE_METRIC_GATE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn upscale_metric_gate_armed() -> bool {
    *UPSCALE_METRIC_GATE.get().unwrap_or(&false)
}


// ---------------------------------------------------------------------------
// 语料自检
// ---------------------------------------------------------------------------

/// `--verify` 语料：`FROZEN` 版本/digest + `CHECKSUMS.sha256` 逐文件重算。
fn verify_corpus(root: &Path) -> Value {
    let frozen = std::fs::read_to_string(root.join("FROZEN")).unwrap_or_default();
    let mut version = String::new();
    let mut digest = String::new();
    let mut declared_files = 0u64;
    for line in frozen.lines() {
        if let Some(v) = line.strip_prefix("version=") {
            version = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("digest=") {
            digest = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("files=") {
            declared_files = v.trim().parse().unwrap_or(0);
        }
    }
    let sums = std::fs::read_to_string(root.join("CHECKSUMS.sha256")).unwrap_or_default();
    let mut checked = 0u64;
    let mut bad: Vec<String> = Vec::new();
    for line in sums.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.splitn(2, char::is_whitespace);
        let (Some(expect), Some(rel)) = (it.next(), it.next()) else {
            continue;
        };
        let rel = rel.trim();
        let path = root.join(rel);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let got = core_hash_sha256_hex(&bytes);
                if got == expect {
                    checked += 1;
                } else {
                    bad.push(format!("{rel}: sha256 不符（期望 {expect}，实测 {got}）"));
                }
            }
            Err(e) => bad.push(format!("{rel}: 读不到（{e}）")),
        }
    }
    json!({
        "root": root.to_string_lossy(),
        "version": version,
        "declared_digest": digest,
        "declared_files": declared_files,
        "checksums_lines": sums.lines().filter(|l| !l.trim().is_empty()).count(),
        "verified_files": checked,
        "mismatches": bad,
        "frozen_ok": bad.is_empty() && checked > 0 && checked == declared_files,
    })
}

/// 语料不引 core-hash 依赖：这里只需要普通 sha256，直接用 `core-hash` 的实现。
fn core_hash_sha256_hex(bytes: &[u8]) -> String {
    core_hash::sha256_hex(bytes)
}

// ---------------------------------------------------------------------------
// 宿主装配
// ---------------------------------------------------------------------------

struct Harness {
    gw: Gateway,
    imaging: PackHost,
    segment: PackHost,
    upscale: PackHost,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

fn start_harness(work: &Path) -> Result<Harness, String> {
    std::fs::create_dir_all(work).map_err(|e| format!("建 work-dir 失败：{e}"))?;
    let caps: Vec<String> = CAPABILITIES.iter().map(|s| s.to_string()).collect();
    let gw = Gateway::start(GatewayConfig::new(caps, work).with_token("n14-process-gate"))
        .map_err(|e| format!("网关启动失败：{e}"))?;
    let open = |pack: &str, tag: &str| -> Result<PackHost, String> {
        let dir = repo_root().join(pack);
        let (manifest, _) = load_pack(&dir).map_err(|e| format!("{pack} manifest 不合法：{e}"))?;
        if manifest.is_judge() {
            return Err(format!("{pack} 不应声明 kinds=judge（本卡只跑处理包）"));
        }
        let dir_work = work.join(format!("pack-{tag}"));
        std::fs::create_dir_all(&dir_work).ok();
        let mut cfg = PackHostConfig::new(gw.base_url(), gw.token(), &dir_work);
        cfg.runtime_python = Some(PathBuf::from("python3"));
        PackHost::open(&dir, cfg).map_err(|e| format!("{pack} 打不开（health 握手失败？{e}）"))
    };
    let imaging = open(PACK_IMAGING, "imaging")?;
    let segment = open(PACK_SEGMENT, "segment")?;
    let upscale = open(PACK_UPSCALE, "upscale")?;
    Ok(Harness { gw, imaging, segment, upscale })
}

impl Harness {
    fn host_for(&self, need: &str) -> &PackHost {
        match need {
            "white_bg_fill" => &self.segment,
            "upscale" => &self.upscale,
            _ => &self.imaging,
        }
    }
}

// ---------------------------------------------------------------------------
// 单次处理（一行 = 一个 need × 一张图）
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Attempt {
    row: Value,
    contract: Value,
    out_path: Option<PathBuf>,
    sha256: String,
    bytes: u64,
    workdir: PathBuf,
    error: Option<String>,
}

/// 一个 need 的参数（卡面口径；**不使用**超出 schema 的键，也绝不改阈值）。
fn options_for(need: &str, item: &Value) -> Result<Value, String> {
    let expect = &item["expect"];
    let opts = match need {
        "resize" => json!({
            "action": "resize",
            "resizeMode": "long_edge",
            "resizeLongEdge": RESIZE_LONG_EDGE,
        }),
        "convert" => json!({
            "action": "convert",
            "convertFormat": expect["out_format"].as_str().unwrap_or("webp"),
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
        "watermark" => {
            let anchor = match expect["anchor"].as_str().unwrap_or("br") {
                "tl" => "top_left",
                "tr" => "top_right",
                "bl" => "bottom_left",
                "br" => "bottom_right",
                "center" => "center",
                other => return Err(format!("未知 anchor {other}")),
            };
            json!({
                "action": "watermark",
                "watermarkText": expect["text"].as_str().unwrap_or(WATERMARK_TEXT),
                "watermarkAnchor": anchor,
                "watermarkOpacity": expect["opacity"].as_f64().unwrap_or(WATERMARK_OPACITY),
            })
        }
        "white_bg_fill" => json!({ "bgMode": "white" }),
        "smart_crop" => json!({
            "action": "crop",
            "cropAspect": expect["aspect"].as_str().unwrap_or("1:1"),
        }),
        "upscale" => json!({
            "scale": expect["scale"].as_u64().unwrap_or(2).to_string(),
            "engine": "full",
            "tile": "auto",
            "denoise": 0,
        }),
        other => return Err(format!("未知 need {other}")),
    };
    Ok(opts)
}

fn attempt(
    host: &PackHost,
    need: &str,
    tag: &str,
    src: &Path,
    workdir: &Path,
    extra: Value,
) -> Result<Attempt, String> {
    std::fs::create_dir_all(workdir).map_err(|e| format!("建资产目录失败：{e}"))?;
    let mut opts = extra;
    opts["workdir"] = json!(workdir.to_string_lossy());
    let job_id = format!("n14-{need}-{tag}");
    let call = host.call(
        "judge",
        json!({
            "job_id": job_id,
            "asset": { "id": tag, "path": src.to_string_lossy() },
            "options": opts,
        }),
    );
    let reply = match call {
        Ok(v) => v,
        Err(e) => {
            return Ok(Attempt {
                row: json!({ "error": e.to_string() }),
                contract: json!({ "ok": false, "code": "host_error", "detail": e.to_string() }),
                out_path: None,
                sha256: String::new(),
                bytes: 0,
                workdir: workdir.to_path_buf(),
                error: Some(format!("宿主调用失败：{e}")),
            })
        }
    };
    let rows = reply.get("rows").and_then(Value::as_array).cloned().unwrap_or_default();
    if rows.len() != 1 {
        return Err(format!("应答 rows 数量 {} != 1：{reply}", rows.len()));
    }
    let row = rows[0].clone();
    // 宿主契约关：处理类包的每一行都必须过 parse_row（产物去处、verdict 红线都在这）
    let contract = match parse_row(&[PluginKind::Process], &row) {
        Ok(_) => json!({ "ok": true }),
        Err(v) => json!({ "ok": false, "code": v.code(), "detail": v.to_string() }),
    };
    let art = row.get("artifacts").and_then(Value::as_array).cloned().unwrap_or_default();
    let out_path = art
        .first()
        .and_then(|a| a.get("path"))
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let (sha256, bytes) = match &out_path {
        Some(p) if p.is_file() => {
            let raw = std::fs::read(p).map_err(|e| format!("读不到产物 {}：{e}", p.display()))?;
            (core_hash_sha256_hex(&raw), raw.len() as u64)
        }
        Some(p) => (String::new(), {
            eprintln!("[n14] 产物缺失：{}", p.display());
            0
        }),
        None => (String::new(), 0),
    };
    Ok(Attempt { row, contract, out_path, sha256, bytes, workdir: workdir.to_path_buf(), error: None })
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

fn gray(img: &DynamicImage) -> (Vec<f64>, u32, u32) {
    let rgb = img.to_rgb8();
    let (w, h) = rgb.dimensions();
    let mut out = Vec::with_capacity((w * h) as usize);
    for p in rgb.pixels() {
        let [r, g, b] = p.0;
        out.push(0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64);
    }
    (out, w, h)
}

/// SSIM（口径与 `codec_probe` 一致：Y=BT.601、11×11 高斯 σ=1.5、边界裁 5px、
/// C1=(0.01·255)²、C2=(0.03·255)²、不做样本协方差修正）。
fn ssim_gray(a: &[f64], b: &[f64], w: u32, h: u32) -> f64 {
    const R: i32 = 5;
    let mut kernel = [[0f64; 11]; 11];
    let sigma = 1.5f64;
    let mut sum = 0f64;
    for (y, row) in kernel.iter_mut().enumerate() {
        for (x, v) in row.iter_mut().enumerate() {
            let dy = y as f64 - R as f64;
            let dx = x as f64 - R as f64;
            *v = (-(dx * dx + dy * dy) / (2.0 * sigma * sigma)).exp();
            sum += *v;
        }
    }
    for row in kernel.iter_mut() {
        for v in row.iter_mut() {
            *v /= sum;
        }
    }
    let c1 = (0.01 * 255.0f64).powi(2);
    let c2 = (0.03 * 255.0f64).powi(2);
    let (iw, ih) = (w as i32, h as i32);
    let mut acc = 0f64;
    let mut n = 0f64;
    let mut y = R;
    while y < ih - R {
        let mut x = R;
        while x < iw - R {
            let (mut mu_a, mut mu_b) = (0f64, 0f64);
            let (mut va, mut vb, mut cov) = (0f64, 0f64, 0f64);
            for ky in 0..11i32 {
                for kx in 0..11i32 {
                    let wgt = kernel[ky as usize][kx as usize];
                    let idx = ((y + ky - R) * iw + (x + kx - R)) as usize;
                    mu_a += wgt * a[idx];
                    mu_b += wgt * b[idx];
                }
            }
            for ky in 0..11i32 {
                for kx in 0..11i32 {
                    let wgt = kernel[ky as usize][kx as usize];
                    let idx = ((y + ky - R) * iw + (x + kx - R)) as usize;
                    let da = a[idx] - mu_a;
                    let db = b[idx] - mu_b;
                    va += wgt * da * da;
                    vb += wgt * db * db;
                    cov += wgt * da * db;
                }
            }
            let num = (2.0 * mu_a * mu_b + c1) * (2.0 * cov + c2);
            let den = (mu_a * mu_a + mu_b * mu_b + c1) * (va + vb + c2);
            acc += if den == 0.0 { 1.0 } else { num / den };
            n += 1.0;
            x += 4;
        }
        y += 4;
    }
    if n == 0.0 {
        1.0
    } else {
        acc / n
    }
}

/// 压缩类 SSIM：参考图（原图）按产物尺寸 lanczos3 重采样后再比（只量编码损失，
/// 不把「合规降采样」算成糊图 —— 与 N6b `codec_probe ssim-to` 同口径）。
fn ssim_to(reference: &Path, out: &Path) -> Result<f64, String> {
    let out_img = load(out)?;
    let (ow, oh) = out_img.dimensions();
    let ref_img = load(reference)?;
    let (rw, rh) = ref_img.dimensions();
    let ref_scaled = if (rw, rh) != (ow, oh) {
        DynamicImage::ImageRgb8(img_resize(&ref_img.to_rgb8(), ow, oh, FilterType::Lanczos3))
    } else {
        ref_img
    };
    let (ga, gw_, gh_) = gray(&ref_scaled);
    let (gb, w2, h2) = gray(&out_img);
    if (gw_, gh_) != (w2, h2) {
        return Err(format!("SSIM 尺寸不一致：{gw_}x{gh_} vs {w2}x{h2}"));
    }
    Ok(ssim_gray(&ga, &gb, gw_, gh_))
}

/// PSNR + SSIM（对 `_ref`）：尺寸不一致按 **min 裁齐**（语料 `psnr_crop: min`；奇数边原图
/// 2× 后按 floor 少 1px 是派生语义，不是质量差）。SSIM 口径同 `ssim_gray`
/// （Y=BT.601、11×11 高斯 σ=1.5、边界裁 5px、4px 取样步长）。
fn psnr_ssim_vs(out: &DynamicImage, reference: &Path) -> Result<(f64, f64, u32, u32), String> {
    let ref_img = load(reference)?.to_rgb8();
    let out_img = out.to_rgb8();
    let (aw, ah) = out_img.dimensions();
    let (bw, bh) = ref_img.dimensions();
    let (w, h) = (aw.min(bw), ah.min(bh));
    if w == 0 || h == 0 {
        return Err(format!("尺寸不可比：{aw}x{ah} vs {bw}x{bh}"));
    }
    let crop = |img: &image::RgbImage| -> image::RgbImage {
        if img.dimensions() == (w, h) {
            img.clone()
        } else {
            image::imageops::crop_imm(img, 0, 0, w, h).to_image()
        }
    };
    let a = crop(&out_img);
    let b = crop(&ref_img);
    let mut sq = 0f64;
    let mut n = 0f64;
    for (pa, pb) in a.pixels().zip(b.pixels()) {
        for k in 0..3 {
            let d = pa.0[k] as f64 - pb.0[k] as f64;
            sq += d * d;
            n += 1.0;
        }
    }
    let psnr = if sq == 0.0 { 99.0 } else { 10.0 * (255.0 * 255.0 / (sq / n)).log10() };
    let (ga, _, _) = gray(&DynamicImage::ImageRgb8(a));
    let (gb, _, _) = gray(&DynamicImage::ImageRgb8(b));
    Ok((psnr, ssim_gray(&ga, &gb, w, h), w, h))
}

/// Lanczos 2× 重采样基线：把**输入**直接重采样到产物尺寸（`want`）再对同一张 `_ref` 计分。
/// 基线不是常数——同一张图、同一目标尺寸下最朴素的重采样能拿到多少分，才是「模型档有没有
/// 白干」的参照系（旧判据的 28dB 绝对门槛把「语料天花板」和「模型质量」混成了一件事）。
fn lanczos_baseline(src: &Path, want: (u32, u32)) -> Result<DynamicImage, String> {
    let img = load(src)?.to_rgb8();
    if want.0 == 0 || want.1 == 0 {
        return Err("目标尺寸为 0".into());
    }
    Ok(DynamicImage::ImageRgb8(img_resize(&img, want.0, want.1, FilterType::Lanczos3)))
}

fn aspect_ok(w: u32, h: u32, target: f64) -> bool {
    if h == 0 {
        return false;
    }
    (w as f64 / h as f64 - target).abs() <= ASPECT_TOL_PX / (w.min(h).max(1) as f64) + 1e-9
}

fn parse_aspect(spec: &str) -> Option<f64> {
    let raw = spec.replace('：', ":").replace('/', ":");
    let (l, r) = raw.split_once(':')?;
    let w: f64 = l.trim().parse().ok()?;
    let h: f64 = r.trim().parse().ok()?;
    if w <= 0.0 || h <= 0.0 {
        None
    } else {
        Some(w / h)
    }
}

/// 边框带最大偏差（对纯白 255，逐通道取最大 |255-c|）。
fn border_band_max_dev(path: &Path, band: u32) -> Result<f64, String> {
    let img = load(path)?.to_rgb8();
    let (w, h) = img.dimensions();
    let mut worst = 0i32;
    for y in 0..h {
        for x in 0..w {
            let in_band = x < band || y < band || x + band >= w || y + band >= h;
            if !in_band {
                continue;
            }
            for c in img.get_pixel(x, y).0.iter() {
                worst = worst.max((255i32 - *c as i32).abs());
            }
        }
    }
    Ok(worst as f64)
}

/// 水印：按**混合方程**判落墨（`out = src*(1-α) + color*α`），不用「差值幅度」——
/// JPEG 重编码噪声在纹理区能到 30+，35% 的白字在中等底色上差值也就 20~60，单看幅度分不开
/// （N6c 语料脚本踩过这个坑）。这里解出每像素 α，只收「反差 ≥45」的像素（除法不放大噪声），
/// 再要求成片（4 邻域里 ≥2 个同带）→ 字形内部，最后量那个块的框与中位 α。
fn watermark_probe(src: &Path, out: &Path, anchor: &str, color: [f64; 3], want: f64) -> Result<Value, String> {
    let a = load(src)?.to_rgb8();
    let b = load(out)?.to_rgb8();
    let (w, h) = a.dimensions();
    if b.dimensions() != (w, h) {
        return Err(format!("叠加后尺寸变了：{}x{} → {:?}", w, h, b.dimensions()));
    }
    const MIN_DIFF: i32 = 8;
    const MIN_CONTRAST: f64 = 45.0;
    const ALPHA_BAND: f64 = 0.06;
    let (wus, hus) = (w as usize, h as usize);
    let mut band = vec![false; wus * hus];
    let mut band_alpha = vec![0f64; wus * hus];
    let mut band_count = 0usize;
    for y in 0..hus {
        for x in 0..wus {
            let pa = a.get_pixel(x as u32, y as u32).0;
            let pb = b.get_pixel(x as u32, y as u32).0;
            let maxdiff = (0..3).map(|k| (pb[k] as i32 - pa[k] as i32).abs()).max().unwrap_or(0);
            if maxdiff <= MIN_DIFF {
                continue;
            }
            let mut best: Option<f64> = None;
            for k in 0..3 {
                let delta = color[k] - pa[k] as f64;
                if delta.abs() < MIN_CONTRAST {
                    continue;
                }
                let alpha = ((pb[k] as f64) - (pa[k] as f64)) / delta;
                best = Some(match best {
                    None => alpha,
                    Some(v) if (alpha - want).abs() < (v - want).abs() => alpha,
                    Some(v) => v,
                });
            }
            let Some(alpha) = best else { continue };
            let alpha = alpha.clamp(0.0, 1.0);
            let idx = y * wus + x;
            if (alpha - want).abs() <= ALPHA_BAND {
                band[idx] = true;
                band_alpha[idx] = alpha;
                band_count += 1;
            }
        }
    }
    // 成片过滤：4 邻域里 ≥2 个也在带内（噪声散点几乎不可能满足）
    let mut coherent: Vec<(usize, usize, f64)> = Vec::new();
    for y in 0..hus {
        for x in 0..wus {
            let idx = y * wus + x;
            if !band[idx] {
                continue;
            }
            let mut nb = 0;
            for (dx, dy) in [(1i64, 0i64), (-1, 0), (0, 1), (0, -1)] {
                let nx = x as i64 + dx;
                let ny = y as i64 + dy;
                if nx < 0 || ny < 0 || nx >= wus as i64 || ny >= hus as i64 {
                    continue;
                }
                if band[ny as usize * wus + nx as usize] {
                    nb += 1;
                }
            }
            if nb >= 2 {
                coherent.push((x, y, band_alpha[idx]));
            }
        }
    }
    let mut alphas: Vec<f64> = coherent.iter().map(|(_, _, v)| *v).collect();
    alphas.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
    let median = alphas.get(alphas.len() / 2).copied().unwrap_or(f64::NAN);
    let (mut x1, mut y1, mut x2, mut y2) = (usize::MAX, usize::MAX, 0usize, 0usize);
    for (x, y, _) in &coherent {
        x1 = x1.min(*x);
        y1 = y1.min(*y);
        x2 = x2.max(*x + 1);
        y2 = y2.max(*y + 1);
    }
    let box_out = if coherent.is_empty() {
        Value::Null
    } else {
        json!([x1, y1, x2, y2])
    };
    let (wf, hf) = (w as f64, h as f64);
    // 贴角容差 = 水印边距（宿主默认 16px）+ 半个字高（字形到框边的留白，字号 48 → 24px）。
    // 这不是「放宽阈值凑绿」：算的是字形的行内留白，量的是「墨迹是否贴在指定角」。
    let tol = WATERMARK_MARGIN + WATERMARK_FONT_SIZE / 2.0;
    let placed = if coherent.is_empty() {
        false
    } else {
        match anchor {
            "bottom_right" => (wf - x2 as f64) <= tol && (hf - y2 as f64) <= tol,
            "bottom_left" => (x1 as f64) <= tol && (hf - y2 as f64) <= tol,
            "top_right" => (wf - x2 as f64) <= tol && (y1 as f64) <= tol,
            "top_left" => (x1 as f64) <= tol && (y1 as f64) <= tol,
            "center" => {
                let cx = (x1 + x2) as f64 / 2.0;
                let cy = (y1 + y2) as f64 / 2.0;
                (cx - wf / 2.0).abs() <= wf * 0.06 && (cy - hf / 2.0).abs() <= hf * 0.06
            }
            _ => false,
        }
    };
    Ok(json!({
        "inkPixels": coherent.len(),
        "bandCount": band_count,
        "inkBox": box_out,
        "anchor": anchor,
        "placed": placed,
        "opacity": if median.is_nan() { Value::Null } else { json!((median * 1000.0).round() / 1000.0) },
    }))
}

/// smart_crop：label 主体框映射到产物坐标后必须完整落在产物内。
fn crop_box_mapping(fields: &Value, subject: &Value, out_dims: (u32, u32), tol: f64) -> (bool, Value) {
    let Some(bbox) = subject.as_array() else {
        return (false, json!({ "why": "label 缺 subject_bbox" }));
    };
    if bbox.len() != 4 {
        return (false, json!({ "why": "label subject_bbox 不是 4 元" }));
    }
    let (x1, y1, x2, y2) = (
        bbox[0].as_f64().unwrap_or(0.0),
        bbox[1].as_f64().unwrap_or(0.0),
        bbox[2].as_f64().unwrap_or(0.0),
        bbox[3].as_f64().unwrap_or(0.0),
    );
    let Some(box_used) = fields.get("box").and_then(Value::as_array) else {
        return (false, json!({ "why": "行里缺 fields.box" }));
    };
    let bx1 = box_used.first().and_then(Value::as_f64).unwrap_or(0.0);
    let by1 = box_used.get(1).and_then(Value::as_f64).unwrap_or(0.0);
    let pad = fields.get("padOffset").and_then(Value::as_array).cloned().unwrap_or_default();
    let ox = pad.first().and_then(Value::as_f64).unwrap_or(0.0);
    let oy = pad.get(1).and_then(Value::as_f64).unwrap_or(0.0);
    let mapped = [x1 - bx1 + ox, y1 - by1 + oy, x2 - bx1 + ox, y2 - by1 + oy];
    let (ow, oh) = (out_dims.0 as f64, out_dims.1 as f64);
    // 容差口径（裁决明确）：label 主体框本身有 ±`subject_bbox_tol_px`（v1.1 标 2px，边缘鲁棒估计）的
    // 标注误差，映射后允许落在产物边界外 tol 以内仍算「完整落在产物内」；默认 0 容差会把这 2px 的
    // 标注噪声当成算法切图。
    let inside = mapped[0] >= -tol
        && mapped[1] >= -tol
        && mapped[2] <= ow + tol
        && mapped[3] <= oh + tol;
    (
        inside,
        json!({
            "labelBbox": [x1, y1, x2, y2],
            "boxUsed": box_used,
            "padOffset": pad,
            "mapped": mapped,
            "tolPx": tol,
            "out": [ow, oh],
        }),
    )
}

// ---------------------------------------------------------------------------
// 逐 need 断言（pass A）
// ---------------------------------------------------------------------------

fn eval_item(
    need: &str,
    item: &Value,
    corpus: &Path,
    a: &Attempt,
    probe_attempt: Option<&Attempt>,
) -> (bool, Vec<Value>, String, Value) {
    let mut crit: Vec<Value> = Vec::new();
    let mut why = String::new();
    let mut metrics = json!({});
    let fail = |why: &mut String, msg: String| {
        if why.is_empty() {
            *why = msg;
        }
    };
    let expect = &item["expect"];

    // 契约关先跑：产物去处不合契约 → 后面像素再好也不算 PASS
    if a.contract.get("ok").and_then(Value::as_bool) != Some(true) {
        let code = a.contract.get("code").and_then(Value::as_str).unwrap_or("?");
        fail(&mut why, format!("结果行契约被拒（{code}）"));
    }

    // 处理类的**合法跳过**（K07-FIX-N `task` 守卫②）：目标白底 + 输入本来就是白底。
    // 这一行没有产物才是对的，但「跳过」本身不算证据——必须三条同时成立：
    //   ① 结果行是 `result=ok` + `skipReason=already_white`（不是别的借口）；
    //   ② **门禁自己重算**输入 4px 边框带偏差 ≤ 3（不信 evidence、也不信 label 一句话）；
    //   ③ 没有产物、工作目录里零残留（跳过 = 不产生新工作版本）。
    // 输入不是白底却跳过 → FAIL：这是「拿守卫偷懒/跳过当通关」的靶子。
    if need == "white_bg_fill" {
        if let Some(reason) = a.row.get("skipReason").and_then(Value::as_str) {
            let src = corpus.join(item["file"].as_str().unwrap_or_default());
            let measured = border_band_max_dev(&src, BG_BAND_PX).unwrap_or(f64::MAX);
            let label_in_dev = expect["in_bg_dev_max"].as_f64();
            let stray: Vec<String> = std::fs::read_dir(&a.workdir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect()
                })
                .unwrap_or_default();
            let ok_row = a.row.get("result").and_then(Value::as_str) == Some("ok");
            let no_artifact = a.out_path.is_none()
                && a.row
                    .get("artifacts")
                    .and_then(Value::as_array)
                    .map(|v| v.is_empty())
                    .unwrap_or(true);
            let legit = reason == "already_white"
                && ok_row
                && no_artifact
                && stray.is_empty()
                && label_in_dev.map(|v| v <= BG_MAX_DEV).unwrap_or(false)
                && measured <= BG_MAX_DEV;
            crit.push(json!({
                "name": "skip_already_white",
                "ok": legit,
                "detail": {
                    "skipReason": reason,
                    "result": a.row.get("result"),
                    "noArtifact": no_artifact,
                    "workdirStray": stray,
                    "labelInBorderDev": label_in_dev,
                    "inputBorderDevMeasured": measured,
                    "allowed": BG_MAX_DEV,
                    "bandPx": BG_BAND_PX,
                    "evidence": a.row.get("evidence"),
                },
            }));
            if !legit {
                fail(
                    &mut why,
                    format!(
                        "跳过不合法（reason={reason}）：label_in_dev={label_in_dev:?} 实测={measured} \
                         允许≤{BG_MAX_DEV}，产物={}，残留={stray:?}",
                        !no_artifact
                    ),
                );
            }
            metrics = json!({
                "skipped": true,
                "skipReason": reason,
                "inputBorderDevMeasured": measured,
            });
            return (legit, crit, why, metrics);
        }
    }

    match (&a.error, &a.out_path) {
        (Some(e), _) => fail(&mut why, e.clone()),
        (None, None) => fail(&mut why, "行里没有产物（处理类必须落 working version）".into()),
        _ => {}
    }

    // 产物去处：只能一个 export_file/working_version，且落在宿主给的资产目录里
    let art = a.row.get("artifacts").and_then(Value::as_array).cloned().unwrap_or_default();
    let art_ok = art.len() == 1
        && matches!(
            art[0].get("kind").and_then(Value::as_str),
            Some("export_file") | Some("working_version")
        );
    let in_workdir = a
        .out_path
        .as_ref()
        .map(|p| p.starts_with(&a.workdir))
        .unwrap_or(false);
    let stray = std::fs::read_dir(&a.workdir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| {
                    a.out_path
                        .as_ref()
                        .and_then(|p| p.file_name())
                        .map(|f| f.to_string_lossy() != *n)
                        .unwrap_or(true)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    crit.push(json!({
        "name": "artifact_working_version",
        "ok": art_ok && in_workdir && stray.is_empty(),
        "detail": {
            "artifacts": art,
            "insideHostWorkdir": in_workdir,
            "workdir": a.workdir.to_string_lossy(),
            "strayFiles": stray,
        },
    }));
    if !(art_ok && in_workdir && stray.is_empty()) && why.is_empty() {
        why = "产物不是单一的 working_version/export_file，或没落宿主给的资产目录".into();
    }

    if a.error.is_some() || a.out_path.is_none() {
        return (false, crit, why, metrics);
    }
    let out = a.out_path.as_ref().unwrap();
    if !out.is_file() {
        fail(&mut why, format!("产物文件不存在：{}", out.display()));
        return (false, crit, why, metrics);
    }
    let src = corpus.join(item["file"].as_str().unwrap_or_default());
    let dims = match load(out) {
        Ok(img) => img.dimensions(),
        Err(e) => {
            fail(&mut why, e);
            return (false, crit, why, metrics);
        }
    };

    match need {
        "resize" => {
            let want: Vec<u64> = expect["out_size"]
                .as_array()
                .map(|a| a.iter().map(|v| v.as_u64().unwrap_or(0)).collect())
                .unwrap_or_default();
            let got = vec![dims.0 as u64, dims.1 as u64];
            let size_ok = got == want;
            let in_size: Vec<u64> = expect["in_size"]
                .as_array()
                .map(|a| a.iter().map(|v| v.as_u64().unwrap_or(0)).collect())
                .unwrap_or_default();
            let no_up = in_size.len() == 2 && got[0] <= in_size[0] && got[1] <= in_size[1];
            crit.push(json!({ "name": "out_size", "ok": size_ok, "detail": { "got": got, "expect": want } }));
            crit.push(json!({ "name": "no_upscale", "ok": no_up, "detail": { "got": got, "in": in_size } }));
            if !size_ok {
                fail(&mut why, format!("尺寸 {got:?} != 期望 {want:?}"));
            } else if !no_up {
                fail(&mut why, format!("放大了：{got:?} > {in_size:?}"));
            }
            metrics = json!({ "out": got, "bytes": a.bytes });
        }
        "convert" => {
            let want = expect["out_format"].as_str().unwrap_or("?");
            let got = magic_format(out);
            let fmt_ok = got == want;
            let size_ok = load(&src).map(|i| i.dimensions() == dims).unwrap_or(false);
            // alpha：源有 alpha → 产物必须也有 alpha 且极值不丢
            let src_alpha = load(&src).map(|i| i.to_rgba8()).ok().map(|rgba| {
                rgba.pixels().map(|p| p.0[3]).collect::<Vec<_>>()
            });
            let out_alpha = load(out).map(|i| i.to_rgba8()).ok().map(|rgba| {
                rgba.pixels().map(|p| p.0[3]).collect::<Vec<_>>()
            });
            let src_has_alpha = src_alpha
                .as_ref()
                .map(|v| v.iter().any(|a| *a != 255))
                .unwrap_or(false);
            let alpha_ok = if !src_has_alpha {
                true
            } else {
                match (&src_alpha, &out_alpha) {
                    (Some(s), Some(o)) if s.len() == o.len() => {
                        let smin = s.iter().copied().min().unwrap_or(255);
                        let omin = o.iter().copied().min().unwrap_or(255);
                        o.iter().any(|a| *a != 255) && omin <= smin.saturating_add(1)
                    }
                    _ => false,
                }
            };
            crit.push(json!({ "name": "format", "ok": fmt_ok, "detail": { "got": got, "expect": want } }));
            crit.push(json!({ "name": "size_unchanged", "ok": size_ok, "detail": { "out": [dims.0, dims.1] } }));
            crit.push(json!({ "name": "png_keeps_alpha", "ok": alpha_ok,
                             "detail": { "srcHasAlpha": src_has_alpha, "checked": src_has_alpha } }));
            if !fmt_ok {
                fail(&mut why, format!("格式 {got} != 期望 {want}"));
            } else if !size_ok {
                fail(&mut why, "转格式改了尺寸".into());
            } else if !alpha_ok {
                fail(&mut why, "alpha 没保留".into());
            }
            metrics = json!({ "format": got, "bytes": a.bytes });
        }
        "compress" => {
            let max_bytes = expect["target_max_bytes"].as_u64().unwrap_or(COMPRESS_MAX_BYTES);
            let ssim_min = expect["ssim_min"].as_f64().unwrap_or(COMPRESS_SSIM_MIN);
            let size_ok = a.bytes <= max_bytes;
            let fmt_ok = magic_format(out) == expect["out_format"].as_str().unwrap_or("jpg");
            match ssim_to(&src, out) {
                Ok(ssim) => {
                    let ssim_ok = ssim >= ssim_min;
                    crit.push(json!({ "name": "ssim_to_source", "ok": ssim_ok,
                                     "detail": { "ssim": (ssim * 10000.0).round() / 10000.0, "min": ssim_min } }));
                    metrics = json!({ "bytes": a.bytes, "ssim": (ssim * 10000.0).round() / 10000.0 });
                    if !ssim_ok {
                        fail(&mut why, format!("SSIM {ssim:.4} < {ssim_min}"));
                    }
                }
                Err(e) => {
                    crit.push(json!({ "name": "ssim_to_source", "ok": false, "detail": e }));
                    fail(&mut why, e);
                }
            }
            crit.push(json!({ "name": "max_bytes", "ok": size_ok, "detail": { "bytes": a.bytes, "max": max_bytes } }));
            crit.push(json!({ "name": "format", "ok": fmt_ok, "detail": { "magic": magic_format(out) } }));
            if !size_ok && why.is_empty() {
                fail(&mut why, format!("{}B > {}B", a.bytes, max_bytes));
            }
            if !fmt_ok && why.is_empty() {
                fail(&mut why, "压缩产物格式不对".into());
            }
        }
        "watermark" => {
            let anchor = match expect["anchor"].as_str().unwrap_or("br") {
                "tl" => "top_left",
                "tr" => "top_right",
                "bl" => "bottom_left",
                "br" => "bottom_right",
                other => other,
            };
            let want_opacity = expect["opacity"].as_f64().unwrap_or(WATERMARK_OPACITY);
            // 白字（产品默认色）是交付物本体；但 35% 白字落在亮底上时 |255-src| < 45，
            // 混合方程解不出 α（N6c 踩过）。此时再用「同样透明度、同样锚点」的黑字探头量一次——
            // 量的是**混合语义**，不是换参数凑绿，evidence 里逐条记明用的是哪支探头。
            let white = watermark_probe(&src, out, anchor, [255.0, 255.0, 255.0], want_opacity);
            let use_probe = match &white {
                Ok(p) => p["inkPixels"].as_u64().unwrap_or(0) < 60,
                Err(_) => true,
            };
            let (probe, measured_with) = if use_probe {
                match probe_attempt.and_then(|b| b.out_path.as_ref()) {
                    Some(p) => (
                        watermark_probe(&src, p, anchor, [0.0, 0.0, 0.0], want_opacity),
                        "black（测量探头：白字在亮底解不出 α）",
                    ),
                    None => (white, "white"),
                }
            } else {
                (white, "white（交付物本色）")
            };
            match probe {
                Ok(probe) => {
                    let placed = probe["placed"].as_bool().unwrap_or(false);
                    let ink = probe["inkPixels"].as_u64().unwrap_or(0);
                    let opacity = probe["opacity"].as_f64().unwrap_or(f64::NAN);
                    let ink_ok = ink >= 60;
                    let opacity_ok = (opacity - want_opacity).abs() <= 0.04;
                    let size_ok = dims == load(&src).map(|i| i.dimensions()).unwrap_or((0, 0));
                    crit.push(json!({ "name": "anchor", "ok": placed, "detail": probe.clone() }));
                    crit.push(json!({ "name": "opacity", "ok": opacity_ok,
                                     "detail": { "measured": probe["opacity"], "want": want_opacity,
                                                 "tol": 0.04, "measuredWith": measured_with } }));
                    crit.push(json!({ "name": "ink_drawn", "ok": ink_ok,
                                     "detail": { "inkPixels": ink, "min": 60, "measuredWith": measured_with } }));
                    crit.push(json!({ "name": "size_unchanged", "ok": size_ok, "detail": { "out": [dims.0, dims.1] } }));
                    metrics = probe;
                    if !placed && why.is_empty() {
                        fail(&mut why, format!("水印没落在 {anchor}"));
                    } else if !opacity_ok && why.is_empty() {
                        fail(&mut why, format!("不透明度实测 {opacity:.3} != {want_opacity}"));
                    } else if !ink_ok && why.is_empty() {
                        fail(&mut why, "水印几乎没画出来".into());
                    }
                }
                Err(e) => {
                    crit.push(json!({ "name": "anchor", "ok": false, "detail": e }));
                    fail(&mut why, e);
                }
            }
        }
        "white_bg_fill" => {
            let max_dev = expect["out_bg_dev_max"].as_f64().unwrap_or(BG_MAX_DEV);
            let dev = border_band_max_dev(out, BG_BAND_PX).unwrap_or(f64::MAX);
            let band_ok = dev <= max_dev;
            crit.push(json!({ "name": "border_band", "ok": band_ok,
                             "detail": { "maxDev": dev, "allowed": max_dev, "bandPx": BG_BAND_PX } }));
            metrics = json!({ "borderMaxDev": dev, "bytes": a.bytes });
            if !band_ok {
                fail(&mut why, format!("边框带偏差 {dev} > {max_dev}"));
            }
        }
        "smart_crop" => {
            let aspect = parse_aspect(expect["aspect"].as_str().unwrap_or("1:1")).unwrap_or(f64::NAN);
            let ratio_ok = aspect_ok(dims.0, dims.1, aspect);
            let fields = a.row.get("fields").cloned().unwrap_or(json!({}));
            let tol = expect["subject_bbox_tol_px"].as_f64().unwrap_or(SUBJECT_BBOX_TOL_PX);
            let (inside, detail) = crop_box_mapping(&fields, &expect["subject_bbox"], dims, tol);
            crit.push(json!({ "name": "aspect", "ok": ratio_ok,
                             "detail": { "out": [dims.0, dims.1], "aspect": aspect,
                                         "actual": if dims.1 > 0 { dims.0 as f64 / dims.1 as f64 } else { 0.0 } } }));
            crit.push(json!({ "name": "subject_inside", "ok": inside, "detail": detail }));
            metrics = json!({ "out": [dims.0, dims.1], "mode": fields.get("mode") });
            if !ratio_ok {
                fail(&mut why, format!("比例不对：{}x{}", dims.0, dims.1));
            } else if !inside {
                fail(&mut why, "主体框超出产物".into());
            }
        }
        "upscale" => {
            let want: Vec<u64> = expect["out_size"]
                .as_array()
                .map(|a| a.iter().map(|v| v.as_u64().unwrap_or(0)).collect())
                .unwrap_or_default();
            let got = vec![dims.0 as u64, dims.1 as u64];
            let size_ok = got == want;
            crit.push(json!({ "name": "out_size", "ok": size_ok, "detail": { "got": got, "expect": want } }));
            let reference = corpus.join(expect["gt_reference"].as_str().unwrap_or_default());
            let out_img = match load(out) {
                Ok(i) => i,
                Err(e) => {
                    fail(&mut why, e);
                    return (false, crit, why, metrics);
                }
            };
            // 两条腿：① 插件产物对 `_ref`；② 同尺寸 Lanczos 2× 重采样对同一张 `_ref`（基线）。
            let plugin_score = psnr_ssim_vs(&out_img, &reference);
            let base_score = lanczos_baseline(&src, dims).and_then(|b| psnr_ssim_vs(&b, &reference));
            match (plugin_score, base_score) {
                (Ok((p_psnr, p_ssim, cw, ch)), Ok((b_psnr, b_ssim, _, _))) => {
                    let d_psnr = p_psnr - b_psnr;
                    let d_ssim = p_ssim - b_ssim;
                    let redline_ok = d_psnr >= -UPSCALE_REDLINE_DB && d_ssim >= -UPSCALE_REDLINE_SSIM;
                    let armed = upscale_metric_gate_armed();
                    let r2 = |v: f64| (v * 100.0).round() / 100.0;
                    let r4 = |v: f64| (v * 10000.0).round() / 10000.0;
                    // 观测列：三列照实记录（不作门槛）
                    crit.push(json!({ "name": "metrics_observed", "ok": true,
                                     "detail": {
                                         "pluginPsnr": r2(p_psnr), "baselinePsnr": r2(b_psnr), "deltaPsnr": r2(d_psnr),
                                         "pluginSsim": r4(p_ssim), "baselineSsim": r4(b_ssim), "deltaSsim": r4(d_ssim),
                                         "gate": false,
                                         "note": "PSNR/SSIM/Δ 三列只作观测值，不参与 PASS/FAIL（2026-10-23 裁决）",
                                         "baseline": "lanczos3 2x → 产物尺寸，再对同一张 _ref 计分",
                                         "reference": reference.to_string_lossy(), "compareSize": [cw, ch] } }));
                    // 红线：逐图算，v1 语料上只记录（redlineArmed=false），v2+ 进硬门
                    crit.push(json!({ "name": "no_break_redline", "ok": if armed { redline_ok } else { true },
                                     "detail": {
                                         "deltaPsnr": r2(d_psnr), "deltaSsim": r4(d_ssim),
                                         "rule": format!("deltaPsnr >= -{UPSCALE_REDLINE_DB}dB 且 deltaSsim >= -{UPSCALE_REDLINE_SSIM}"),
                                         "redlineOk": redline_ok, "redlineArmed": armed,
                                         "note": if armed { "红线已 arm：Δ 越线即 FAIL" }
                                                 else { "v1 语料（超分派生链不真实）：红线只作观测，不参与 PASS/FAIL；v2+ 语料自动 arm" } } }));
                    metrics = json!({
                        "out": got,
                        "psnr": r2(p_psnr), "psnrLanczos": r2(b_psnr), "psnrGain": r2(d_psnr),
                        "ssim": r4(p_ssim), "ssimLanczos": r4(b_ssim), "ssimGain": r4(d_ssim),
                        "redlineOk": redline_ok, "redlineArmed": armed,
                        "redlineDb": UPSCALE_REDLINE_DB, "redlineSsim": UPSCALE_REDLINE_SSIM,
                    });
                    if !size_ok {
                        fail(&mut why, format!("尺寸 {got:?} != {want:?}"));
                    } else if armed && !redline_ok {
                        fail(&mut why, format!(
                            "越过不崩坏红线：ΔPSNR {d_psnr:+.2}dB（限 -{UPSCALE_REDLINE_DB}dB）/ ΔSSIM {d_ssim:+.4}（限 -{UPSCALE_REDLINE_SSIM}）"
                        ));
                    }
                }
                (Err(e), _) | (_, Err(e)) => {
                    crit.push(json!({ "name": "metrics_observed", "ok": false, "detail": e }));
                    fail(&mut why, e);
                }
            }
        }
        _ => unreachable!(),
    }
    let all_ok = crit.iter().all(|c| c["ok"].as_bool().unwrap_or(false)) && a.error.is_none();
    (all_ok, crit, why, metrics)
}

/// `white_bg_fill` 幂等（K07-FIX-N `task` 起口径变了，改的是「二次施加」这一条）：
///
/// * **输入本来就白底** → 两次都必须 `skipReason=already_white`、两次都零产物；
/// * **输入不是白底** → 同输入同参数重跑字节一致（pass A/B 已逐张对账 sha256），
///   再把产物当输入跑一次：**必须被守卫②跳过**、零产物、原产物 sha256 不变。
///
/// 为什么不再量「二次施加后像素变了多少」：那是把 fixed-point 当幂等（u2net 在已白底图上
/// 会给出略不同的掩码，天然不是不动点）；而且旧行为的实测就是 bug——wbf_07 的产物被二次
/// 施加刷掉 28.5% 像素、主体底部没了（见 N14 证据 §idempotency）。现在正确行为是**不跑**，
/// 所以断言从「变了多少」换成「根本不许跑」。
fn idempotency_probe(
    host: &PackHost,
    need: &str,
    src: &Path,
    opts: Value,
    workdir: &Path,
) -> Value {
    let first = match attempt(host, need, "idem-first", src, workdir, opts.clone()) {
        Ok(a) => a,
        Err(e) => return json!({ "ok": false, "why": e }),
    };
    let first_skip = first
        .row
        .get("skipReason")
        .and_then(Value::as_str)
        .map(str::to_string);

    // 输入本来就白底（语料里的 native 白底图）→ 正确行为是**两次都跳过**、两次都不产生产物。
    // 这同样是幂等：f(f(x)) = f(x) 在这里就是「第一次已经把结果交付了」。
    if first_skip.as_deref() == Some("already_white") {
        let again = match attempt(host, need, "idem-same-input", src, workdir, opts) {
            Ok(a) => a,
            Err(e) => return json!({ "ok": false, "why": e }),
        };
        let again_skip = again
            .row
            .get("skipReason")
            .and_then(Value::as_str)
            .map(str::to_string);
        let ok = again_skip.as_deref() == Some("already_white")
            && first.out_path.is_none()
            && again.out_path.is_none();
        return json!({
            "ok": ok,
            "mode": "skip_already_white",
            "skipReason": first_skip,
            "secondRunSkipReason": again_skip,
            "artifacts": 0,
            "evidence": first.row.get("evidence"),
        });
    }

    let Some(first_out) = first.out_path.clone() else {
        return json!({ "ok": false, "why": "第一次既没产物也不是合法跳过" });
    };
    // 同输入同参数再跑一次：字节必须一致（幂等 = 不累积状态）
    let again = match attempt(host, need, "idem-same-input", src, workdir, opts.clone()) {
        Ok(a) => a,
        Err(e) => return json!({ "ok": false, "why": e }),
    };
    let same_input_sha = !first.sha256.is_empty() && first.sha256 == again.sha256;

    // 二次施加（把产物当输入再跑一次）：产物是白底 → **必须被守卫②挡下来**。
    // 守卫之前这里是「再刷一遍」，实测 wbf_07 会被刷掉 28.5% 像素（主体底部没了）；
    // 现在要求：再跑一次 → skipReason=already_white、零产物、像素一个不动。
    let second = match attempt(host, need, "idem-second", &first_out, workdir, opts) {
        Ok(a) => a,
        Err(e) => return json!({ "ok": false, "why": e }),
    };
    let second_skip = second
        .row
        .get("skipReason")
        .and_then(Value::as_str)
        .map(str::to_string);
    let second_skipped = second_skip.as_deref() == Some("already_white") && second.out_path.is_none();
    let first_out_band = border_band_max_dev(&first_out, BG_BAND_PX).unwrap_or(f64::MAX);
    let unchanged = if second.out_path.is_none() {
        // 没产物 = 原文件没被覆盖（产物文件名与输入不同，另外再对一次 sha 才叫证据）
        std::fs::read(&first_out)
            .map(|raw| core_hash_sha256_hex(&raw) == first.sha256)
            .unwrap_or(false)
    } else {
        false
    };
    json!({
        "ok": same_input_sha && second_skipped && unchanged,
        "mode": "paint_then_skip",
        "sameInputSha256": same_input_sha,
        "sha256": first.sha256,
        "firstOutBorderDev": first_out_band,
        "secondApplication": {
            "skipped": second_skipped,
            "skipReason": second_skip,
            "artifacts": second.row.get("artifacts").cloned().unwrap_or(json!([])),
            "firstOutUnchanged": unchanged,
        },
    })
}

// ---------------------------------------------------------------------------
// 反向断言：处理类包回 verdict → 宿主必须拒收
// ---------------------------------------------------------------------------

fn reverse_verdict(reply: &Value) -> Value {
    let mut injected = reply.clone();
    if let Some(row) = injected.get_mut("rows").and_then(Value::as_array_mut).and_then(|r| r.first_mut()) {
        row["verdict"] = json!({ "status": "pass", "confidence": 0.99 });
    }
    let kinds = [PluginKind::Process];
    let row_value = injected["rows"][0].clone();
    match parse_row(&kinds, &row_value) {
        // 单行关口
        Ok(_) => json!({ "rejected": false, "why": "处理类带 verdict 竟然被接受了（红线破了）" }),
        Err(v) => {
            let via_batch = pack_host::result::rows_from_reply(&kinds, &injected).err();
            json!({
                "rejected": true,
                "injected": injected["rows"][0],
                "raw_error": { "code": v.code(), "message": v.to_string() },
                "batch_error": via_batch.map(|e| json!({ "code": e.code(), "message": e.to_string() })),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

struct Args {
    corpus: PathBuf,
    work: PathBuf,
    evidence: PathBuf,
    keep: bool,
    /// 证据文件里记的卡号（复跑本门禁的卡传自己的 id，别把别人的证据顶掉）
    task: String,
}

fn parse_args() -> Result<Args, String> {
    let mut corpus = PathBuf::from("/data/damdesk-test-corpus/v1");
    let mut work = std::env::temp_dir().join("n14-process-gate");
    let mut evidence = PathBuf::from("reports/evidence/task/process_gate.json");
    let mut keep = false;
    let mut task = "task".to_string();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--corpus" => corpus = PathBuf::from(it.next().ok_or("--corpus 缺值")?),
            "--work-dir" => work = PathBuf::from(it.next().ok_or("--work-dir 缺值")?),
            "--evidence" => evidence = PathBuf::from(it.next().ok_or("--evidence 缺值")?),
            "--keep" => keep = true,
            "--task" => task = it.next().ok_or("--task 缺值")?,
            other => return Err(format!("未知参数：{other}")),
        }
    }
    Ok(Args { corpus, work, evidence, keep, task })
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[n14] {e}");
            std::process::exit(2);
        }
    };
    let started = Instant::now();
    // 每次跑之前清空工作目录：上一次跑残留的产物会被「合法跳过」的行当成 stray 文件（工作目录不许有多余文件）
    if args.work.exists() {
        std::fs::remove_dir_all(&args.work).ok();
    }
    std::fs::create_dir_all(&args.work).ok();
    let mutation = verify_corpus(&args.corpus);
    // 超分红线是否 arm：语料版本决定（v1* = 超分派生链是干净回环，指标不作门槛；v2+ 自动 arm）
    let corpus_ver = mutation["version"].as_str().unwrap_or("").to_string();
    let metric_gate_armed = !corpus_ver.trim_start_matches('v').starts_with('1');
    UPSCALE_METRIC_GATE.set(metric_gate_armed).ok();
    println!("== 语料 v1 自检 ==");
    println!("{}", serde_json::to_string_pretty(&mutation).unwrap_or_default());
    if mutation["frozen_ok"].as_bool() != Some(true) {
        eprintln!("[n14] 语料自检未过（frozen_ok != true）——拒绝在不可信语料上跑门禁");
        std::process::exit(3);
    }

    std::fs::create_dir_all(&args.work).ok();
    let harness = match start_harness(&args.work) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[n14] 宿主装配失败：{e}");
            std::process::exit(4);
        }
    };

    let mut results: Vec<Value> = Vec::new();
    let mut per_need: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    let mut first_reply: Option<Value> = None;

    for pass in ["a", "b"] {
        for need in NEEDS {
            let labels: Value = match std::fs::read(args.corpus.join("labels").join(format!("{need}.json")))
                .ok()
                .and_then(|raw| serde_json::from_slice(&raw).ok())
            {
                Some(v) => v,
                None => {
                    eprintln!("[n14] 读不到 labels/{need}.json");
                    std::process::exit(5);
                }
            };
            let items = labels["items"].as_array().cloned().unwrap_or_default();
            if items.len() != 10 {
                eprintln!("[n14] {need} 语料数量 {} != 10", items.len());
                std::process::exit(5);
            }
            for (idx, item) in items.iter().enumerate() {
                let file = item["file"].as_str().unwrap_or_default();
                let src = args.corpus.join(file);
                let stem = Path::new(file)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("item")
                    .to_string();
                let workdir = args.work.join(format!("pass-{pass}")).join(need).join(format!("{idx:02}-{stem}"));
                let opts = match options_for(need, item) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("[n14] {need} 参数构造失败：{e}");
                        std::process::exit(5);
                    }
                };
                let host = harness.host_for(need);
                let att = match attempt(host, need, &format!("{pass}-{idx:02}-{stem}"), &src, &workdir, opts.clone()) {
                    Ok(a) => a,
                    Err(e) => {
                        eprintln!("[n14] {need}/{stem} 调用失败：{e}");
                        std::process::exit(6);
                    }
                };
                if pass == "a" {
                    // 水印：另起一支黑字探头（同锚点同透明度），只在白字量不出 α 时用来量
                    let probe_attempt = if need == "watermark" {
                        let mut popts = opts.clone();
                        popts["watermarkColor"] = json!("#000000");
                        attempt(
                            host,
                            need,
                            &format!("{pass}-{idx:02}-{stem}-probe"),
                            &src,
                            &args.work.join(format!("pass-{pass}")).join("watermark-probe").join(format!("{idx:02}-{stem}")),
                            popts,
                        )
                        .ok()
                    } else {
                        None
                    };
                    if first_reply.is_none() {
                        if let Some(rows) = att.row.as_object() {
                            let _ = rows;
                            first_reply = Some(json!({ "rows": [att.row] }));
                        }
                    }
                    let (ok, crit, why, metrics) = eval_item(need, item, &args.corpus, &att, probe_attempt.as_ref());
                    let entry = per_need.entry(need.to_string()).or_insert((0, 0));
                    entry.0 += 1;
                    if ok {
                        entry.1 += 1;
                    }
                    results.push(json!({
                        "need": need,
                        "file": file,
                        "ok": ok,
                        "why": why,
                        "criteria": crit,
                        "metrics": metrics,
                        "sha256_pass_a": att.sha256,
                        "bytes": att.bytes,
                        "outPath": att.out_path.as_ref().map(|p| p.to_string_lossy().to_string()),
                        "contract": att.contract,
                        "row": att.row,
                    }));
                } else {
                    // pass B：只做 sha256 对账（跳过的行没有产物 → 对账「两趟跳过原因一致」）
                    if let Some(slot) = results.iter_mut().find(|r| {
                        r["need"] == json!(need) && r["file"] == json!(file)
                    }) {
                        let a_sha = slot["sha256_pass_a"].as_str().unwrap_or("").to_string();
                        let a_skip = slot["row"]
                            .get("skipReason")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        let b_skip = att
                            .row
                            .get("skipReason")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        let skipped = a_sha.is_empty() && att.sha256.is_empty();
                        let same = if skipped {
                            a_skip.is_some() && a_skip == b_skip
                        } else {
                            !a_sha.is_empty() && a_sha == att.sha256
                        };
                        slot["sha256_pass_b"] = json!(att.sha256);
                        slot["skipReason_pass_b"] = json!(b_skip);
                        slot["reproducible"] = json!(same);
                        if !same {
                            slot["ok"] = json!(false);
                            let mut why = slot["why"].as_str().unwrap_or("").to_string();
                            let msg = if skipped {
                                "两趟跳过原因不一致（跳过的行也不许两趟不同）"
                            } else {
                                "两趟 sha256 不一致（不可复现）"
                            };
                            if why.is_empty() {
                                why = msg.into();
                            } else {
                                why.push_str("；");
                                why.push_str(msg);
                            }
                            slot["why"] = json!(why);
                            if let Some(crit) = slot["criteria"].as_array_mut() {
                                let b_skip_detail = b_skip.clone();
                                crit.push(json!({
                                    "name": "reproducible",
                                    "ok": false,
                                    "detail": { "passA": a_sha, "passB": att.sha256,
                                                "skipA": a_skip, "skipB": b_skip_detail },
                                }));
                            }
                            if let Some(e) = per_need.get_mut(need) {
                                e.1 = e.1.saturating_sub(1);
                            }
                        } else if let Some(crit) = slot["criteria"].as_array_mut() {
                            crit.push(json!({
                                "name": "reproducible",
                                "ok": true,
                                "detail": if skipped {
                                    json!({ "skippedBothPasses": true, "skipReason": a_skip })
                                } else {
                                    json!({ "sha256": a_sha })
                                },
                            }));
                        }
                    }
                }
            }
        }
        println!("[n14] pass {pass} 完成（{:.1}s）", started.elapsed().as_secs_f64());
    }

    // white_bg_fill 幂等（附加探针，不计入 70 项）
    let mut idem: Vec<Value> = Vec::new();
    if let Ok(raw) = std::fs::read(args.corpus.join("labels/white_bg_fill.json")) {
        if let Ok(labels) = serde_json::from_slice::<Value>(&raw) {
            for item in labels["items"].as_array().cloned().unwrap_or_default() {
                let file = item["file"].as_str().unwrap_or_default();
                let src = args.corpus.join(file);
                let stem = Path::new(file).file_stem().and_then(|s| s.to_str()).unwrap_or("i");
                let workdir = args.work.join("idempotency").join(stem);
                let probe = idempotency_probe(
                    &harness.segment,
                    "white_bg_fill",
                    &src,
                    json!({ "bgMode": "white" }),
                    &workdir,
                );
                idem.push(json!({ "file": file, "probe": probe }));
            }
        }
    }
    let idem_ok = !idem.is_empty() && idem.iter().all(|r| r["probe"]["ok"].as_bool() == Some(true));

    // 反向：处理类包回 verdict 必须被拒
    let reverse = match first_reply {
        Some(reply) => reverse_verdict(&reply),
        None => json!({ "rejected": false, "why": "没有拿到真实应答，无法做反向断言" }),
    };

    // 附带参考：upscale 的 light 档 PSNR（卡面只要求默认档达标；light 是重采样基线，
    // 给报告一张「两条引擎 × 10 张」的对照表，避免把「模型档不占优」看成本卡新发现）
    let mut upscale_light: Vec<Value> = Vec::new();
    if let Ok(raw) = std::fs::read(args.corpus.join("labels/upscale.json")) {
        if let Ok(labels) = serde_json::from_slice::<Value>(&raw) {
            for item in labels["items"].as_array().cloned().unwrap_or_default() {
                let file = item["file"].as_str().unwrap_or_default();
                let src = args.corpus.join(file);
                let stem = Path::new(file).file_stem().and_then(|s| s.to_str()).unwrap_or("i");
                let workdir = args.work.join("upscale-light").join(stem);
                let mut opts = json!({ "scale": item["expect"]["scale"].as_u64().unwrap_or(2).to_string(),
                                       "engine": "light", "tile": "auto", "denoise": 0 });
                opts["workdir"] = json!(workdir.to_string_lossy());
                let att = match attempt(&harness.upscale, "upscale", &format!("light-{stem}"), &src, &workdir, opts) {
                    Ok(a) => a,
                    Err(e) => {
                        upscale_light.push(json!({ "file": file, "error": e }));
                        continue;
                    }
                };
                let reference = args.corpus.join(item["expect"]["gt_reference"].as_str().unwrap_or_default());
                let scored = att.out_path.as_ref().and_then(|p| {
                    let out_img = load(p).ok()?;
                    let dims = out_img.dimensions();
                    let (p_psnr, p_ssim, _, _) = psnr_ssim_vs(&out_img, &reference).ok()?;
                    let base = lanczos_baseline(&src, dims).ok()?;
                    let (b_psnr, b_ssim, _, _) = psnr_ssim_vs(&base, &reference).ok()?;
                    Some((p_psnr, p_ssim, b_psnr, b_ssim))
                });
                upscale_light.push(match scored {
                    Some((p_psnr, p_ssim, b_psnr, b_ssim)) => {
                        let d_psnr = p_psnr - b_psnr;
                        let d_ssim = p_ssim - b_ssim;
                        json!({
                            "file": file,
                            "psnr": (p_psnr * 100.0).round() / 100.0,
                            "psnrLanczos": (b_psnr * 100.0).round() / 100.0,
                            "psnrGain": (d_psnr * 100.0).round() / 100.0,
                            "ssim": (p_ssim * 10000.0).round() / 10000.0,
                            "ssimLanczos": (b_ssim * 10000.0).round() / 10000.0,
                            "redlineOk": d_psnr >= -UPSCALE_REDLINE_DB && d_ssim >= -UPSCALE_REDLINE_SSIM,
                            "redlineArmed": upscale_metric_gate_armed(),
                        })
                    }
                    None => json!({ "file": file, "psnr": Value::Null, "why": "产物缺失或计分失败" }),
                });
            }
        }
    }

    // 附带探针：`convert` 的 alpha 保留（语料 10 张里没有透明源，用 N6b 的 alpha 夹具补这一条）
    let mut alpha_probe: Vec<Value> = Vec::new();
    let alpha_manifest = repo_root().join("packs/plugins/imaging/tests/fixtures/codec/manifest.json");
    if let Ok(raw) = std::fs::read(&alpha_manifest) {
        if let Ok(man) = serde_json::from_slice::<Value>(&raw) {
            for item in man["alpha"].as_array().cloned().unwrap_or_default() {
                let rel = item["file"].as_str().unwrap_or_default();
                let src = alpha_manifest.parent().unwrap().join(rel);
                let stem = Path::new(rel).file_stem().and_then(|s| s.to_str()).unwrap_or("a");
                let workdir = args.work.join("convert-alpha").join(stem);
                let opts = json!({ "action": "convert", "convertFormat": "png" });
                let att = match attempt(&harness.imaging, "convert", &format!("alpha-{stem}"), &src, &workdir, opts) {
                    Ok(a) => a,
                    Err(e) => {
                        alpha_probe.push(json!({ "file": rel, "ok": false, "why": e }));
                        continue;
                    }
                };
                let Some(out) = att.out_path.clone() else {
                    alpha_probe.push(json!({ "file": rel, "ok": false, "why": "无产物" }));
                    continue;
                };
                let stat = |p: &Path| -> Option<(u8, u64, u64)> {
                    let rgba = load(p).ok()?.to_rgba8();
                    let mut min = 255u8;
                    let mut below = 0u64;
                    let mut total = 0u64;
                    for px in rgba.pixels() {
                        min = min.min(px.0[3]);
                        if px.0[3] != 255 {
                            below += 1;
                        }
                        total += 1;
                    }
                    Some((min, below, total))
                };
                let ok = match (stat(&src), stat(&out)) {
                    (Some((smin, sbelow, _)), Some((omin, obelow, ototal))) => {
                        omin <= smin.saturating_add(1)
                            && obelow as f64 >= sbelow as f64 * 0.9
                            && ototal > 0
                    }
                    _ => false,
                };
                alpha_probe.push(json!({
                    "file": rel,
                    "out": out.to_string_lossy(),
                    "ok": ok,
                    "src": stat(&src).map(|(m, b, t)| json!({ "alphaMin": m, "below255": b, "pixels": t })),
                    "outStat": stat(&out).map(|(m, b, t)| json!({ "alphaMin": m, "below255": b, "pixels": t })),
                }));
            }
        }
    }
    let alpha_ok = !alpha_probe.is_empty() && alpha_probe.iter().all(|r| r["ok"].as_bool() == Some(true));

    let total: u32 = per_need.values().map(|(n, _)| *n).sum();
    let passed: u32 = per_need.values().map(|(_, p)| *p).sum();
    let failed = total.saturating_sub(passed);

    println!("\n== N14 process 门禁：7 need × 10 张 ==");
    println!("{:<14} {:<34} {:<6} {}", "need", "file", "结果", "为什么/指标");
    for r in &results {
        let ok = r["ok"].as_bool().unwrap_or(false);
        let metrics = &r["metrics"];
        let note = if ok {
            format!(
                "{} reproducible={}",
                metrics,
                r["reproducible"].as_bool().unwrap_or(false)
            )
        } else {
            r["why"].as_str().unwrap_or("").to_string()
        };
        println!(
            "{:<14} {:<34} {:<6} {}",
            r["need"].as_str().unwrap_or("?"),
            r["file"].as_str().unwrap_or("?"),
            if ok { "PASS" } else { "FAIL" },
            note
        );
    }
    println!("\n逐 need：");
    for (need, (n, p)) in &per_need {
        println!("  {:<14} {}/{}", need, p, n);
    }
    println!("\n幂等（white_bg_fill：已白底两次都跳过 / 重跑同字节 / 产物再跑必须被守卫挡下）：{}",
             if idem_ok { "PASS" } else { "FAIL" });
    println!("convert alpha 保留（3 张透明夹具 → png）：{}", if alpha_ok { "PASS" } else { "FAIL" });
    println!("反向（处理类回 verdict 拒收）：{}", if reverse["rejected"].as_bool() == Some(true) { "PASS" } else { "FAIL" });
    if !upscale_light.is_empty() {
        let light_redline = upscale_light.iter().filter(|r| r["redlineOk"].as_bool() == Some(true)).count();
        println!(
            "参考（upscale light 档 vs Lanczos 基线）：红线 ΔPSNR ≥ -{UPSCALE_REDLINE_DB}dB 且 ΔSSIM ≥ -{UPSCALE_REDLINE_SSIM} → {light_redline}/{}（arm={}）",
            upscale_light.len(),
            upscale_metric_gate_armed()
        );
    }
    println!(
        "\n合计：{} / {} PASS，{} FAIL（{:.1}s）",
        passed,
        total,
        failed,
        started.elapsed().as_secs_f64()
    );

    let evidence = json!({
        "task": args.task,
        "card": "N14 process 门禁（7 处理 need × 10 张）",
        "corpus": mutation,
        "packs": [PACK_IMAGING, PACK_SEGMENT, PACK_UPSCALE],
        "capabilities": CAPABILITIES,
        "host": "PackHost（真子进程 + 真能力网关 + 真模型）",
        "params": {
            "resize_long_edge": RESIZE_LONG_EDGE,
            "compress_max_bytes": COMPRESS_MAX_BYTES,
            "compress_ssim_min": COMPRESS_SSIM_MIN,
            "watermark_text": WATERMARK_TEXT,
            "watermark_opacity": WATERMARK_OPACITY,
            "bg_band_px": BG_BAND_PX,
            "bg_max_dev": BG_MAX_DEV,
            "aspect_tol_px": ASPECT_TOL_PX,
            "subject_bbox_tol_px": SUBJECT_BBOX_TOL_PX,
            "upscale_redline_db": UPSCALE_REDLINE_DB,
            "upscale_redline_ssim": UPSCALE_REDLINE_SSIM,
            "upscale_metric_gate_armed": upscale_metric_gate_armed(),
            "upscale_metric_gate_reason": if upscale_metric_gate_armed() {
                "语料版本 ≥ v2（真实退化源）：红线进硬门"
            } else {
                "语料 v1：超分派生链为「原图缩小」的干净回环，PSNR/感知指标不作门槛（卡内已知限制）；红线仍逐图计算并记录"
            },
            "upscale_baseline": "lanczos3 2x（输入 → 产物尺寸），同一 _ref 计分",
            "upscale_engine": "full（包默认档）",
        },
        "totals": { "checks": total, "pass": passed, "fail": failed },
        "per_need": per_need.iter().map(|(k, (n, p))| (k.clone(), json!({ "checks": n, "pass": p }))).collect::<BTreeMap<_, _>>(),
        "items": results,
        "idempotency": { "ok": idem_ok, "items": idem },
        "convert_alpha": { "ok": alpha_ok, "items": alpha_probe },
        "upscale_engine_reference": { "light": upscale_light },
        "reverse_verdict": reverse,
        "elapsed_s": (started.elapsed().as_secs_f64() * 10.0).round() / 10.0,
    });
    if let Some(dir) = args.evidence.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    match std::fs::write(&args.evidence, serde_json::to_vec_pretty(&evidence).unwrap_or_default()) {
        Ok(_) => println!("evidence → {}", args.evidence.display()),
        Err(e) => eprintln!("[n14] 写 evidence 失败：{e}"),
    }
    if !args.keep {
        // 产物留在 work-dir 供复核；不清空（路径已写进 evidence）
    }
    harness.gw.stop();
    if failed > 0 || !idem_ok || reverse["rejected"].as_bool() != Some(true) {
        std::process::exit(1);
    }
}
