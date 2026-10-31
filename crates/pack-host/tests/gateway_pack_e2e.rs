//! 真网关 × 真插件 × 真模型 的端到端（K03 验收主证据）。
//!
//! 三件事一起验：
//! 1. **真网关接真插件**：`PackHost` 按协议拉起 K01 参考包
//!    （`tools/pack-conformance/fixtures/good`），插件的 `judge`/`repair` 全部打到本进程的
//!    `pack_host::gateway`（真 `io.read` / 真 u2net / 真 OCR / 真 crop·letterbox·upscale），
//!    不是 K01 假网关的桩数据；
//! 2. **产物真落盘且尺寸对得上**：`repair` 的 crop→letterbox→upscale 闭式几何在真算子上
//!    必须产出磁盘文件，宽高与 `artifacts[].width/height` 一致；
//! 3. **K01 conformance 套件对真网关复跑**：`run.py --gateway-url … --work-dir …`
//!    （真网关模式的断言子集：02/03/04/05/06/07/09/10，见 `run.py` 头部说明）。
//!
//! 模型根：`/models`（容器内挂载）或 `DAM_GATEWAY_E2E_MODELS` 指向「按模型名分目录的根」。
//! 缺模型时自动降级为「无模型路径」——仍然跑真网关 + 真插件，但只验不依赖推理的部分
//! （并在输出里明确标注跳过了什么，绝不假装验过）。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};

use pack_host::gateway::{VlError, VlJudge, VlReply, VlRule};
use pack_host::{Gateway, GatewayConfig, PackHost, PackHostConfig};
use serde_json::{json, Value};

const GOOD_PACK: &str = "tools/pack-conformance/fixtures/good";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

/// 把「按模型名分目录的根」整理成引擎契约的 `<root>/models/<name>/…` 布局。
/// 返回 `None` = 本机没有模型（测试降级）。
///
/// **进程级只建一次**（`OnceLock` 缓存）：三个用例并行跑同一个二进制，若每次都
/// `remove_dir_all` + 重建，先拿到根的用例正在加载模型时会撞上另一线程删目录
/// → 干净地退化成 `capability_missing`，表现为 conformance 07「网关没收到任何
/// /vl/judge 调用」随机 FAIL（2026-10-21 实测：串行/覆盖率跑法必红，默认并行偶红）。
fn install_models_root() -> Option<PathBuf> {
    static MODELS: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    MODELS
        .get_or_init(|| {
            let src = std::env::var("DAM_GATEWAY_E2E_MODELS")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/models"));
            let needed = [
                "u2net",
                "ppocrv6-medium-det",
                "ppocrv6-medium-rec",
                "realesr-general-x4v3",
                "lama-fp32",
            ];
            if !needed.iter().all(|n| src.join(n).is_dir()) {
                return None;
            }
            // 一次性建在固定的临时根（不再每次重建）：pid 隔离并发跑的不同测试进程
            let root = std::env::temp_dir().join(format!("pack-host-e2e-models-{}", std::process::id()));
            std::fs::create_dir_all(root.join("models")).unwrap();
            for name in needed {
                #[cfg(unix)]
                {
                    let link = root.join("models").join(name);
                    if !link.exists() {
                        std::os::unix::fs::symlink(src.join(name), &link).ok()?;
                    }
                }
                #[cfg(not(unix))]
                return None;
            }
            // 引擎模型根是进程级全局态：本二进制里所有用例共用这一份
            std::env::set_var("DAM_MODELS_ROOT", &root);
            Some(root)
        })
        .clone()
}

/// 与 conformance 假网关同约定的 VL 后端：真网关装上它才能跑 06/07 两条断言。
#[derive(Debug, Default)]
struct ConformanceVl;

impl VlJudge for ConformanceVl {
    fn judge(&self, path: &Path, prompt: &str, _max_tokens: u32) -> Result<VlReply, VlError> {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.contains("vlundet") {
            return Ok(VlReply::undetermined("vl_parse_failed"));
        }
        if name.contains("catfail") {
            return Ok(VlReply::decided(
                "{\"status\":\"fail\"}",
                vec![VlRule {
                    status: "fail".into(),
                    label: "not_apparel".into(),
                    score: Some(0.31),
                }],
            ));
        }
        let _ = prompt;
        Ok(VlReply::decided(
            "{\"status\":\"pass\"}",
            vec![VlRule {
                status: "pass".into(),
                label: "apparel".into(),
                score: Some(0.92),
            }],
        ))
    }
}

/// 宿主侧配置：网关地址/令牌/工作目录 + 解释器（参考包是 Python 形态，直接跑 entrypoint）。
fn pack_cfg(gw: &Gateway, work: &Path) -> PackHostConfig {
    let mut cfg = PackHostConfig::new(gw.base_url(), gw.token(), work);
    cfg.runtime_python = Some(PathBuf::from("python3"));
    cfg
}

fn start_gateway(work: &Path) -> Gateway {
    let manifest = pack_host::load_pack(&repo_root().join(GOOD_PACK)).expect("参考包 manifest").0;
    Gateway::start(
        GatewayConfig::from_manifest(&manifest, work)
            .with_token("e2e-token")
            .with_vl(std::sync::Arc::new(ConformanceVl)),
    )
    .expect("真网关启动失败")
}

fn make_png(path: &Path, w: i32, h: i32) {
    let mut rgb = vec![128u8; (w as usize) * (h as usize) * 3];
    for y in h / 4..3 * h / 4 {
        for x in w / 5..4 * w / 5 {
            let i = ((y * w + x) * 3) as usize;
            rgb[i] = 240;
            rgb[i + 1] = 230;
            rgb[i + 2] = 40;
        }
    }
    core_media::encode_rgb_to(w, h, &rgb, path, None).expect("写测试图失败");
}

// ---------------------------------------------------------------------------
// 极简 HTTP 客户端（验真网关的原始应答）
// ---------------------------------------------------------------------------

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
    (status, serde_json::from_str(body).expect("应答体 JSON"))
}

// ---------------------------------------------------------------------------
// 1. 真网关的推理端点（真模型）
// ---------------------------------------------------------------------------

#[test]
fn real_models_serve_detect_endpoints() {
    if !cfg!(feature = "local-infer") {
        eprintln!("skip: 本构建未编入 local-infer（web 形态）→ detect/generative 端点按协议回 403");
        return;
    }
    let Some(root) = install_models_root() else {
        eprintln!("skip: 本机没有 /models（u2net/ppocrv6/realesr/lama），跳过真模型端点验证");
        return;
    };
    eprintln!("DAM_MODELS_ROOT={}", root.display());

    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let gw = start_gateway(&work);
    let img = dir.path().join("asset_600x400.png");
    make_png(&img, 600, 400);
    let p = img.to_string_lossy().to_string();

    // detect.u2net：真 u2net，占比为 (0,1] 的实数，掩码必须真落盘且与源图同尺寸
    let (status, body) = http_post(&gw, "/detect/u2net", json!({ "path": p }));
    assert_eq!(status, 200, "{body}");
    let ratio = body["fg_ratio"].as_f64().expect("fg_ratio 必须是数字");
    assert!(ratio > 0.0 && ratio <= 1.0, "fg_ratio 越界：{ratio}");
    assert_eq!(body["width"], 600);
    assert_eq!(body["height"], 400);
    let mask = PathBuf::from(body["mask_path"].as_str().expect("mask_path"));
    assert!(mask.is_file(), "掩码必须真落盘：{mask:?}");
    assert_eq!(core_media::image_size(&mask).unwrap(), (600, 400));
    // 协议 1.1：u2net 应答顺带带主体框（零额外推理，不许多跑一次模型）
    assert!(body["bbox"].is_array(), "u2net 应答必须带 bbox：{body}");
    assert!(body["dominant_bbox"].is_array(), "{body}");

    // detect.bbox（协议 1.1）：真 u2net 掩码 → 主体框 + 目标裁剪窗
    let (status, bbox_body) = http_post(
        &gw,
        "/detect/bbox",
        json!({ "path": p, "target_ratio": 0.75 }),
    );
    assert_eq!(status, 200, "{bbox_body}");
    assert_eq!(bbox_body["grid"], 320);
    let same_ratio = bbox_body["fg_ratio"].as_f64().expect("fg_ratio 必须是数字");
    assert!(
        (same_ratio - ratio).abs() < 1e-12,
        "同一张图两个端点必须给同一个掩码口径：u2net {ratio} vs bbox {same_ratio}"
    );
    let bbox = bbox_body["bbox"].as_array().expect("bbox 必须是数组");
    let (bx1, by1, bx2, by2) = (
        bbox[0].as_u64().unwrap() as u32,
        bbox[1].as_u64().unwrap() as u32,
        bbox[2].as_u64().unwrap() as u32,
        bbox[3].as_u64().unwrap() as u32,
    );
    assert!(bx1 < bx2 && by1 < by2, "框退化：{bbox:?}");
    assert!(bx2 <= 600 && by2 <= 400, "框越界：{bbox:?}");
    let dominant = bbox_body["dominant_bbox"].as_array().expect("主主体框");
    assert!(dominant[0].as_u64() >= bbox[0].as_u64() && dominant[2].as_u64() <= bbox[2].as_u64(),
            "主主体框必须落在全掩码框内：{bbox:?} vs {dominant:?}");
    // 窗：贴主体、落在画布内、占比接近目标（网格级误差）
    let window = &bbox_body["window"];
    assert!(window.is_object(), "给 target_ratio 必须回窗：{bbox_body}");
    let win = window["box"].as_array().expect("window.box");
    let (wx1, wy1) = (win[0].as_u64().unwrap(), win[1].as_u64().unwrap());
    let (wx2, wy2) = (win[2].as_u64().unwrap(), win[3].as_u64().unwrap());
    assert!(wx1 < wx2 && wy1 < wy2 && wx2 <= 600 && wy2 <= 400, "窗越界：{win:?}");
    let win_ratio = window["ratio"].as_f64().expect("window.ratio");
    eprintln!(
        "detect/bbox 实测：fg={same_ratio:.4} bbox={bbox:?} dominant={dominant:?} window={win:?} window_ratio={win_ratio:.4}"
    );
    assert!(
        (win_ratio - 0.75).abs() <= 0.25,
        "窗内占比该往 0.75 靠：{win_ratio}（fg={same_ratio}）"
    );
    // 主体（若真探测到）必须整块落在窗内：这是 KI-24 的验收口径
    if same_ratio > 0.0 {
        assert!(
            dominant[0].as_u64().unwrap() >= wx1 && dominant[1].as_u64().unwrap() >= wy1
                && dominant[2].as_u64().unwrap() <= wx2
                && dominant[3].as_u64().unwrap() <= wy2,
            "窗必须覆盖主主体紧框：win={win:?} dominant={dominant:?}"
        );
    }

    // detect.ocr：真 PP-OCRv6（行结构 + 引擎名；纯色图可能 0 行，不臆造文本）
    let (status, body) = http_post(&gw, "/detect/ocr", json!({ "path": p }));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["engine"], "ppocrv6-medium");
    let lines = body["lines"].as_array().expect("lines 必须是数组");
    for line in lines {
        assert!(line["text"].is_string());
        assert_eq!(line["box"].as_array().map(|a| a.len()), Some(4));
        assert!(line["score"].is_number());
    }

    // op.replace_bg：真 u2net 掩码 + 纯色复合 → 尺寸不变
    let out = work.join("bg.png");
    let (status, body) = http_post(
        &gw,
        "/op/replace_bg",
        json!({ "path": p, "color": "#00ff00", "out": out.to_string_lossy() }),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(core_media::image_size(&out).unwrap(), (600, 400));

    // op.inpaint：真 lama，掩码与原图同尺寸
    let mask = dir.path().join("hole.png");
    let mut mk = vec![0u8; 600 * 400 * 3];
    for y in 150..250 {
        for x in 200..400 {
            let i = ((y * 600 + x) * 3) as usize;
            mk[i] = 255;
            mk[i + 1] = 255;
            mk[i + 2] = 255;
        }
    }
    core_media::encode_rgb_to(600, 400, &mk, &mask, None).unwrap();
    let out = work.join("inpainted.png");
    let (status, body) = http_post(
        &gw,
        "/op/inpaint",
        json!({ "path": p, "mask": mask.to_string_lossy(), "out": out.to_string_lossy() }),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(core_media::image_size(&out).unwrap(), (600, 400));

    // op.upscale(scale=4)：有 x4v3 时走超分，产物边长严格 ×4
    let small = dir.path().join("small_120x80.png");
    make_png(&small, 120, 80);
    let out = work.join("up4.png");
    let (status, body) = http_post(
        &gw,
        "/op/upscale",
        json!({ "path": small.to_string_lossy(), "scale": 4.0, "out": out.to_string_lossy() }),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["op"], "upscale.realesr-x4v3", "有模型时必须走 x4v3");
    assert_eq!(
        core_media::image_size(&out).unwrap(),
        (480, 320),
        "x4 超分产物必须是源图 ×4"
    );
}

// ---------------------------------------------------------------------------
// 2. 真网关 × 真 Python 插件：judge / repair 全链路
// ---------------------------------------------------------------------------

#[test]
fn real_plugin_judge_and_repair_through_real_gateway() {
    // 真模型断言要有两层前提：模型在盘 **且** 本构建编入了 local-infer
    let has_models = cfg!(feature = "local-infer") && install_models_root().is_some();
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let gw = start_gateway(&work);

    let host = PackHost::open(repo_root().join(GOOD_PACK), pack_cfg(&gw, &work))
        .expect("打开参考包失败");

    // --- judge（只走 io.read 的 A2 规则：与模型无关，任何机器都必须成立）---
    let big = dir.path().join("main_1600x1600.png");
    make_png(&big, 1600, 1600);
    let out = host
        .call(
            "judge",
            json!({
                "job_id": "job-A2-big",
                "asset": { "id": "big", "path": big.to_string_lossy(), "mime": "image/png" },
                "rules": ["A2_min_short_side"],
            }),
        )
        .expect("judge 失败（A2）");
    let verdicts = out["verdicts"].as_array().expect("verdicts");
    assert_eq!(verdicts.len(), 1);
    assert_eq!(verdicts[0]["status"], "pass", "{out}");
    assert_eq!(verdicts[0]["evidence"]["short_side"], 1600);
    host.shutdown();

    // 真网关侧收到了按 job_id 归账的 emit（协议 §3.5）
    let emits = gw.emits("job-A2-big");
    assert_eq!(emits.len(), 1, "真网关必须收到 1 条 emit：{emits:?}");
    assert_eq!(emits[0]["asset_id"], "big");
    assert!(gw.emits("job-never-existed").is_empty());

    // 小图必须 fail（真尺寸 600 < 1600，不是桩数据）
    let small = dir.path().join("small_600x600.png");
    make_png(&small, 600, 600);
    let host = PackHost::open(repo_root().join(GOOD_PACK), pack_cfg(&gw, &work)).unwrap();
    let out = host
        .call(
            "judge",
            json!({
                "job_id": "job-A2-small",
                "asset": { "id": "small", "path": small.to_string_lossy() },
                "rules": ["A2_min_short_side"],
            }),
        )
        .expect("judge 失败（A2 small）");
    assert_eq!(out["verdicts"][0]["status"], "fail", "{out}");
    assert_eq!(out["verdicts"][0]["evidence"]["short_side"], 600);

    // --- repair：crop → letterbox → upscale，真算子真产物 ---
    let repair_dir = work.join("repair-job");
    let out = host
        .call(
            "repair",
            json!({
                "job_id": "job-repair",
                "asset": { "id": "big", "path": big.to_string_lossy() },
                "rules": ["A2_min_short_side"],
                "options": { "workdir": repair_dir.to_string_lossy() },
            }),
        )
        .expect("repair 失败");
    let artifacts = out["artifacts"].as_array().expect("artifacts");
    assert_eq!(artifacts.len(), 1, "{out}");
    let art = &artifacts[0];
    let path = PathBuf::from(art["path"].as_str().expect("artifact.path"));
    assert!(path.is_file(), "修复产物必须真落盘：{path:?}");
    assert_eq!(art["kind"], "image/png");
    let (w, h) = core_media::image_size(&path).unwrap();
    assert_eq!(
        (w, h),
        (
            art["width"].as_i64().unwrap() as i32,
            art["height"].as_i64().unwrap() as i32
        ),
        "声明尺寸必须与磁盘产物一致"
    );
    // 参考实现：crop 1000x1000 → letterbox ratio=1（不变）→ upscale ×2 = 2000x2000
    assert_eq!((w, h), (2000, 2000), "闭环几何尺寸不对：{path:?}");
    assert!(repair_dir.join("A2_min_short_side-1-crop.png").is_file());
    assert!(repair_dir.join("A2_min_short_side-2-letterbox.png").is_file());
    assert_eq!(out["verdicts"][0]["fixed"], true);
    host.shutdown();

    // --- A4（主体占比）：有模型才验，缺模型时明确跳过而不是假装通过 ---
    if has_models {
        let host = PackHost::open(repo_root().join(GOOD_PACK), pack_cfg(&gw, &work)).unwrap();
        let out = host
            .call(
                "judge",
                json!({
                    "job_id": "job-A4",
                    "asset": { "id": "big", "path": big.to_string_lossy() },
                    "rules": ["A4_subject_ratio", "A5_overlay_text", "A6_category"],
                }),
            )
            .expect("judge 失败（A4/A5/A6）");
        let verdicts = out["verdicts"].as_array().unwrap();
        assert_eq!(verdicts.len(), 3, "{out}");
        assert_eq!(verdicts[0]["ruleId"], "A4_subject_ratio");
        let ratio = verdicts[0]["evidence"]["fg_ratio"]
            .as_f64()
            .expect("A4 证据里必须有真 fg_ratio");
        assert!(ratio > 0.0 && ratio <= 1.0, "fg_ratio 越界：{ratio}");
        // A5 先 OCR（真 OCR 对纯色图 0 行）→ 再问 VL（桩回 pass）
        assert_eq!(verdicts[1]["ruleId"], "A5_overlay_text");
        assert_eq!(verdicts[2]["ruleId"], "A6_category");
        assert_eq!(verdicts[2]["status"], "pass");
        host.shutdown();
    } else {
        eprintln!("skip: 本机无模型 → 不验 A4/A5/A6 真推理（其余断言已通过）");
    }
}

// ---------------------------------------------------------------------------
// 3. K01 conformance 套件 × 真网关（真网关模式）
// ---------------------------------------------------------------------------

#[test]
fn conformance_suite_passes_against_real_gateway() {
    // 真模型断言要有两层前提：模型在盘 **且** 本构建编入了 local-infer
    let has_models = cfg!(feature = "local-infer") && install_models_root().is_some();
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let gw = start_gateway(&work);

    let root = repo_root();
    let runner = root.join("tools/pack-conformance/run.py");
    let pack = root.join(GOOD_PACK);
    // 06 判 A4（真 u2net）+ A5（真 OCR）→ 无模型时不能要求它通过
    let only = if has_models {
        "02,03,04,05,06,07,09,10"
    } else {
        "02,03,04,05,09,10"
    };
    let expected_pass = only.split(',').count();

    let output = std::process::Command::new("python3")
        .arg(&runner)
        .arg("--plugin")
        .arg(&pack)
        .arg("--only")
        .arg(only)
        .arg("--gateway-url")
        .arg(gw.base_url())
        .arg("--gateway-token")
        .arg(gw.token())
        .arg("--work-dir")
        .arg(&work)
        .current_dir(&root)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .output()
        .expect("起 conformance 套件失败（需要 python3）");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    eprintln!("--- conformance(stdout) ---\n{stdout}\n--- stderr ---\n{stderr}");

    assert!(
        output.status.success(),
        "conformance 对真网关未全绿（only={only}）：\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains(&format!("{expected_pass}/{expected_pass} PASS")),
        "期望 {expected_pass}/{expected_pass} PASS，实际输出：\n{stdout}"
    );

    // 真网关侧账本：参考包 judge 时按 job 上报（check 02~10 都跑过 judge）
    assert!(
        !gw.emit_jobs().is_empty(),
        "真网关没有收到任何 report/emit，插件没走网关"
    );
    gw.stop();
}
