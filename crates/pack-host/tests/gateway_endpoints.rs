//! 能力网关 HTTP 面单测（协议 §6）：鉴权、能力白名单、路径边界、十个端点的应答形状。
//!
//! 这一组**不依赖真模型**：`DAM_MODELS_ROOT` 指向空目录 → detect/generative 端点必须
//! 回 403 `capability_missing`（绝不静默降级成假结果）。真模型链路见
//! `gateway_pack_e2e.rs`（真网关 + 真 Python 插件 + conformance 套件）。
//!
//! 客户端是手写的最小 HTTP/1.1 请求（本机 POST + Content-Length），不用额外 dev 依赖。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Once;

use pack_host::gateway::{
    VlError, VlJudge, VlReply, VlRule, ENDPOINTS, MAX_BODY_BYTES, VL_MAX_TOKENS_CAP,
};
use pack_host::{Gateway, GatewayConfig};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

static ENV_ONCE: Once = Once::new();

/// 模型根指向空目录：保证「模型未装 → 403 capability_missing」在测试机上也确定成立，
/// 同时**绝不**让本二进制跑真推理（真推理只在 `gateway_pack_e2e.rs` 里验）。
fn use_empty_models_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!("pack-host-empty-models-{}", std::process::id()));
    std::fs::create_dir_all(root.join("models")).unwrap();
    ENV_ONCE.call_once(|| {
        // 进程级全局态，只在首次设置（并行用例共享同一空根，语义一致）
        std::env::set_var("DAM_MODELS_ROOT", &root);
    });
    root
}

fn caps() -> Vec<String> {
    ENDPOINTS.iter().map(|(_, cap)| cap.to_string()).collect()
}

fn start_gateway(work_dir: &Path) -> Gateway {
    use_empty_models_root();
    let mut cfg = GatewayConfig::new(caps(), work_dir).with_token("test-token");
    cfg.vl = None;
    Gateway::start(cfg).expect("网关启动失败")
}

fn start_gateway_with_vl(work_dir: &Path, vl: std::sync::Arc<dyn VlJudge>) -> Gateway {
    use_empty_models_root();
    Gateway::start(GatewayConfig::new(caps(), work_dir).with_token("test-token").with_vl(vl))
        .expect("网关启动失败")
}

struct Response {
    status: u16,
    body: Value,
}

fn send(gw: &Gateway, method: &str, path: &str, token: Option<&str>, body: &str) -> Response {
    let addr = gw.base_url().replace("http://", "");
    let mut stream = TcpStream::connect(&addr).expect("连接网关失败");
    let auth = match token {
        Some(t) => format!("authorization: Bearer {t}\r\n"),
        None => String::new(),
    };
    let req = format!(
        "{method} {path} HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\n\
         content-length: {}\r\n{auth}connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Response {
    let text = String::from_utf8_lossy(raw).to_string();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("应答没有分隔头体：{text:?}"));
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("应答状态行不合法：{head:?}"));
    let body: Value = serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("应答体不是 JSON（{e}）：{body:?}"));
    Response { status, body }
}

fn post(gw: &Gateway, path: &str, body: Value) -> Response {
    send(gw, "POST", path, Some(gw.token()), &body.to_string())
}

fn post_raw(gw: &Gateway, path: &str, raw: &str) -> Response {
    send(gw, "POST", path, Some(gw.token()), raw)
}

/// 生成一张真 PNG（纯色 + 中心白块）：几何算子需要能真解码的像素。
fn make_png(dir: &Path, name: &str, w: i32, h: i32) -> PathBuf {
    let mut rgb = vec![128u8; (w as usize) * (h as usize) * 3];
    let (cx, cy) = (w / 2, h / 2);
    for y in (cy - h / 8).max(0)..(cy + h / 8).min(h) {
        for x in (cx - w / 8).max(0)..(cx + w / 8).min(w) {
            let i = ((y * w + x) * 3) as usize;
            rgb[i] = 250;
            rgb[i + 1] = 250;
            rgb[i + 2] = 250;
        }
    }
    let path = dir.join(name);
    core_media::encode_rgb_to(w, h, &rgb, &path, None).expect("生成测试图失败");
    path
}

/// 造一张「外圈纯色 + 中心另一色」的 PNG（白底统计用：边缘带 = 外圈）。
fn make_png_with_border(
    dir: &Path,
    name: &str,
    w: i32,
    h: i32,
    border: [u8; 3],
    center: [u8; 3],
) -> PathBuf {
    let mut rgb = vec![0u8; (w as usize) * (h as usize) * 3];
    let band = (w.min(h) / 10).max(1);
    for y in 0..h {
        for x in 0..w {
            let on_border = x < band || x >= w - band || y < band || y >= h - band;
            let px = if on_border { border } else { center };
            let i = ((y * w + x) * 3) as usize;
            rgb[i..i + 3].copy_from_slice(&px);
        }
    }
    let path = dir.join(name);
    core_media::encode_rgb_to(w, h, &rgb, &path, None).expect("生成测试图失败");
    path
}

fn tmp_workdir(tag: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let work = dir.path().join(format!("work-{tag}"));
    std::fs::create_dir_all(&work).unwrap();
    (dir, work)
}

// ---------------------------------------------------------------------------
// 01 鉴权 / 路由 / 能力白名单（协议 §6 开头三条约定）
// ---------------------------------------------------------------------------

#[test]
fn auth_routing_and_capability_whitelist() {
    let (_tmp, work) = tmp_workdir("auth");
    let gw = start_gateway(&work);

    // 令牌错 → 401，且不透露端点存在性
    let r = send(&gw, "POST", "/io/read", Some("wrong"), "{}");
    assert_eq!(r.status, 401, "{}", r.body);
    assert_eq!(r.body["error"], "unauthorized");

    // 完全不带 Authorization → 401
    let r = send(&gw, "POST", "/io/read", None, "{}");
    assert_eq!(r.status, 401);

    // 非 POST → 405
    let r = send(&gw, "GET", "/io/read", Some(gw.token()), "");
    assert_eq!(r.status, 405, "{}", r.body);

    // 未知端点 → 404 no_such_endpoint
    let r = post(&gw, "/no/such/endpoint", json!({}));
    assert_eq!(r.status, 404, "{}", r.body);
    assert_eq!(r.body["error"], "no_such_endpoint");

    // 未声明能力 → 403 capability_missing，且点名缺哪项
    let narrow = Gateway::start(
        GatewayConfig::new(vec!["io.read".into()], &work).with_token("test-token"),
    )
    .unwrap();
    let r = post(&narrow, "/vl/judge", json!({ "path": "/tmp/x.png", "prompt": "p" }));
    assert_eq!(r.status, 403, "{}", r.body);
    assert_eq!(r.body["error"], "capability_missing");
    assert_eq!(r.body["capability"], "vl.judge");

    // 端点 → 能力映射与协议 §6 表逐字一致（顺序也一致）
    let expected = [
        ("/io/read", "io.read"),
        ("/detect/u2net", "detect.u2net"),
        ("/detect/bbox", "detect.u2net"),
        ("/detect/ocr", "detect.ocr"),
        ("/detect/white_stats", "detect.white"),
        ("/op/crop", "op.geometric"),
        ("/op/resize", "op.geometric"),
        ("/op/letterbox", "op.geometric"),
        ("/op/upscale", "op.geometric"),
        ("/op/encode", "op.geometric"),
        ("/op/watermark", "op.overlay"),
        ("/op/replace_bg", "op.generative"),
        ("/op/inpaint", "op.generative"),
        ("/vl/judge", "vl.judge"),
        ("/report/emit", "report.emit"),
    ];
    assert_eq!(ENDPOINTS, expected);
}

#[test]
fn random_token_when_not_configured() {
    let (_tmp, work) = tmp_workdir("token");
    use_empty_models_root();
    let gw = Gateway::start(GatewayConfig::new(caps(), &work)).unwrap();
    assert_eq!(gw.token().len(), 64, "随机令牌应为 32 字节 hex");
    assert!(gw.token().chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(gw.base_url(), String::new());
}

// ---------------------------------------------------------------------------
// 02 /io/read（元信息）
// ---------------------------------------------------------------------------

#[test]
fn io_read_reports_real_metadata() {
    let (tmp, work) = tmp_workdir("io");
    let gw = start_gateway(&work);
    let img = make_png(tmp.path(), "asset_640x480.png", 640, 480);

    let r = post(&gw, "/io/read", json!({ "path": img.to_string_lossy() }));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["width"], 640);
    assert_eq!(r.body["height"], 480);
    assert_eq!(r.body["mime"], "image/png");
    let bytes = std::fs::read(&img).unwrap();
    assert_eq!(r.body["size"], bytes.len() as u64);
    assert_eq!(
        r.body["sha256"],
        core_hash::sha256_hex(&bytes),
        "sha256 必须与文件内容一致"
    );

    // 文件不存在 → 404 asset_unreadable（不是 500，也不是 200 空壳）
    let r = post(&gw, "/io/read", json!({ "path": "/nonexistent/never.png" }));
    assert_eq!(r.status, 404, "{}", r.body);
    assert_eq!(r.body["error"], "asset_unreadable");

    // 缺 path / 相对路径 → 400 invalid_params
    let r = post(&gw, "/io/read", json!({}));
    assert_eq!(r.status, 400);
    assert_eq!(r.body["error"], "invalid_params");
    let r = post(&gw, "/io/read", json!({ "path": "relative.png" }));
    assert_eq!(r.status, 400);

    // 请求体不是 JSON → 400（不崩进程）
    let r = post_raw(&gw, "/io/read", "{not json");
    assert_eq!(r.status, 400);
    assert_eq!(r.body["error"], "invalid_params");
}

// ---------------------------------------------------------------------------
// 02b /io/read 的可选白度统计（K07-FIX-D：R2 走 classic 的数据来源）
// ---------------------------------------------------------------------------

#[test]
fn io_read_white_measure_matches_classic_white_edge() {
    let (tmp, work) = tmp_workdir("io-white");
    let gw = start_gateway(&work);

    // 外圈纯白 + 红中心 → 边缘带偏差 0（classic white_edge 只统计外圈）
    let white = make_png_with_border(
        tmp.path(),
        "white_bg.png",
        400,
        400,
        [255, 255, 255],
        [220, 30, 30],
    );
    let r = post(
        &gw,
        "/io/read",
        json!({ "path": white.to_string_lossy(), "measure": ["white"] }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["white_dev"].as_f64().unwrap(), 0.0);
    assert_eq!(r.body["white_worst_dev"].as_f64().unwrap(), 0.0);
    assert_eq!(r.body["white_band"], 4);
    // 与 engine-classic 同一份实现：直接对拍，防止两条口径悄悄跑偏
    let (w, h, px) = core_media::rgb8(&white).unwrap();
    let direct = engine_classic::detectors::white_edge(
        &px,
        w as u32,
        h as u32,
        &engine_classic::detectors::WhiteEdgeParams {
            max_dev: f64::INFINITY,
            band: 4,
        },
    )
    .unwrap();
    assert!((r.body["white_dev"].as_f64().unwrap() - direct.mean_dev).abs() < 1e-9);

    // 外圈灰（200）→ 偏差 55（255-200），worst 同值
    let grey = make_png_with_border(
        tmp.path(),
        "grey_bg.png",
        400,
        400,
        [200, 200, 200],
        [220, 30, 30],
    );
    let r = post(
        &gw,
        "/io/read",
        json!({ "path": grey.to_string_lossy(), "measure": ["white"] }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert!((r.body["white_dev"].as_f64().unwrap() - 55.0).abs() < 1.0);

    // 不带 measure → 不多解一次图、也不回 white_dev（老调用方零变化）
    let r = post(&gw, "/io/read", json!({ "path": grey.to_string_lossy() }));
    assert_eq!(r.status, 200);
    assert!(r.body.get("white_dev").is_none(), "{}", r.body);

    // band 非法 → 400 invalid_params（不静默兜底）
    let r = post(
        &gw,
        "/io/read",
        json!({ "path": grey.to_string_lossy(), "measure": ["white"], "band": 0 }),
    );
    assert_eq!(r.status, 400);
    assert_eq!(r.body["error"], "invalid_params");
}

// ---------------------------------------------------------------------------
// 03 几何算子 + 写根边界（协议 §6「out 必须落在写根之下」）
// ---------------------------------------------------------------------------

#[test]
fn geometric_ops_write_inside_workroot_only() {
    let (tmp, work) = tmp_workdir("geo");
    let gw = start_gateway(&work);
    let img = make_png(tmp.path(), "src_1000x1000.png", 1000, 1000);

    // crop：[x1,y1,x2,y2] 源图绝对像素
    let out = work.join("crop.png");
    let r = post(
        &gw,
        "/op/crop",
        json!({ "path": img.to_string_lossy(), "box": [100, 50, 700, 450], "out": out.to_string_lossy() }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["width"], 600);
    assert_eq!(r.body["height"], 400);
    assert_eq!(r.body["op"], "crop");
    assert!(out.is_file(), "产物必须真的落盘");

    // 越界 box → 400（不静默钳制）
    let r = post(
        &gw,
        "/op/crop",
        json!({ "path": img.to_string_lossy(), "box": [0, 0, 5000, 5000],
                "out": work.join("bad.png").to_string_lossy() }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    assert_eq!(r.body["error"], "invalid_params");

    // letterbox：ratio=2 的方形图 → 2000x1000（只补边，不缩放）
    let out = work.join("lb.png");
    let r = post(
        &gw,
        "/op/letterbox",
        json!({ "path": img.to_string_lossy(), "target_ratio": 2.0,
                "background": "#ffffff", "out": out.to_string_lossy() }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!((r.body["width"].clone(), r.body["height"].clone()), (json!(2000), json!(1000)));
    assert_eq!(r.body["op"], "letterbox");
    let (w, h) = core_media::image_size(&out).unwrap();
    assert_eq!((w, h), (2000, 1000));

    // letterbox：宽图 + ratio=1 → 补成正方形
    let wide = make_png(tmp.path(), "wide_2000x1000.png", 2000, 1000);
    let out = work.join("lb-square.png");
    let r = post(
        &gw,
        "/op/letterbox",
        json!({ "path": wide.to_string_lossy(), "target_ratio": 1.0, "out": out.to_string_lossy() }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!((r.body["width"].clone(), r.body["height"].clone()), (json!(2000), json!(2000)));

    // upscale：无 x4v3（模型根为空）→ Lanczos 重采样，尺寸严格 ×scale
    let small = make_png(tmp.path(), "small_200x100.png", 200, 100);
    let out = work.join("up.png");
    let r = post(
        &gw,
        "/op/upscale",
        json!({ "path": small.to_string_lossy(), "scale": 2.0, "out": out.to_string_lossy() }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!((r.body["width"].clone(), r.body["height"].clone()), (json!(400), json!(200)));
    assert_eq!(r.body["op"], "upscale.resample");
    assert_eq!(core_media::image_size(&out).unwrap(), (400, 200));

    // 越界 scale → 400
    let r = post(
        &gw,
        "/op/upscale",
        json!({ "path": small.to_string_lossy(), "scale": 99.0, "out": work.join("x.png").to_string_lossy() }),
    );
    assert_eq!(r.status, 400);

    // 产物越界：写根之外的目录 → 400 out_of_workroot（绝不写出去）
    let outside = tmp.path().join("outside.png");
    let r = post(
        &gw,
        "/op/crop",
        json!({ "path": img.to_string_lossy(), "box": [0, 0, 10, 10], "out": outside.to_string_lossy() }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    assert_eq!(r.body["error"], "out_of_workroot");
    assert!(!outside.exists(), "越界请求不能落盘");

    // 相对路径 out → 400
    let r = post(
        &gw,
        "/op/crop",
        json!({ "path": img.to_string_lossy(), "box": [0, 0, 10, 10], "out": "rel.png" }),
    );
    assert_eq!(r.status, 400);

    // 父目录自动创建（job workdir 允许嵌套多级）
    let nested = work.join("job-1/step-2/deep/out.png");
    let r = post(
        &gw,
        "/op/crop",
        json!({ "path": img.to_string_lossy(), "box": [0, 0, 100, 100], "out": nested.to_string_lossy() }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(nested.is_file());

    // L3 登记的额外写根（job workdir 不一定在 work_dir 之下）
    let other = tmp.path().join("job-workdir");
    std::fs::create_dir_all(&other).unwrap();
    gw.allow_write_root(&other);
    let out = other.join("ok.png");
    let r = post(
        &gw,
        "/op/crop",
        json!({ "path": img.to_string_lossy(), "box": [0, 0, 100, 100], "out": out.to_string_lossy() }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(out.is_file());
}

// ---------------------------------------------------------------------------
// 03b /op/resize（N19）：只缩不放 + 预算边界（大图预缩放的数据入口）
// ---------------------------------------------------------------------------

#[test]
fn resize_endpoint_only_shrinks_and_rejects_bad_budget() {
    let (tmp, work) = tmp_workdir("resize");
    let gw = start_gateway(&work);
    let img = make_png(tmp.path(), "src_1000x500.png", 1000, 500);

    // 长边 1000 > 上限 400 → 等比缩到 400×200
    let out = work.join("scaled.png");
    let r = post(
        &gw,
        "/op/resize",
        json!({ "path": img.to_string_lossy(), "max_side": 400, "out": out.to_string_lossy() }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["op"], "resize");
    assert_eq!(r.body["width"], 400);
    assert_eq!(r.body["height"], 200);
    assert_eq!(r.body["source_width"], 1000);
    assert_eq!(r.body["source_height"], 500);
    assert_eq!(r.body["scaled"], true);
    assert!(out.is_file(), "产物必须真的落盘");
    let (w, h) = core_media::image_size(&out).unwrap();
    assert_eq!((w, h), (400, 200), "落盘像素必须与应答一致（不靠应答自证）");

    // 上限大于长边 → 只缩不放（尺寸不变，scaled=false）
    let keep = work.join("keep.png");
    let r = post(
        &gw,
        "/op/resize",
        json!({ "path": img.to_string_lossy(), "max_side": 4000, "out": keep.to_string_lossy() }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!((r.body["width"].as_i64(), r.body["height"].as_i64()), (Some(1000), Some(500)));
    assert_eq!(r.body["scaled"], false);

    // max_side 缺省 / 0 / 越界 → 400（「不想缩就别调这个端点」，不猜默认值）
    for bad in [json!(0), json!(8), json!(9000), Value::Null] {
        let r = post(
            &gw,
            "/op/resize",
            json!({ "path": img.to_string_lossy(), "max_side": bad,
                    "out": work.join("bad.png").to_string_lossy() }),
        );
        assert_eq!(r.status, 400, "max_side={bad} 应被拒：{}", r.body);
        assert_eq!(r.body["error"], "invalid_params");
    }

    // out 不许等于 path（原图只读）：源图放进写根，才能确保拒的理由是「原地重编码」而不是写根
    let same = work.join("same.png");
    std::fs::copy(&img, &same).unwrap();
    let r = post(
        &gw,
        "/op/resize",
        json!({ "path": same.to_string_lossy(), "max_side": 400, "out": same.to_string_lossy() }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    assert_eq!(r.body["error"], "invalid_params");

    // out 必须落在写根之下（与 crop 同一权限档）
    let outside = tmp.path().join("outside.png");
    let r = post(
        &gw,
        "/op/resize",
        json!({ "path": img.to_string_lossy(), "max_side": 400, "out": outside.to_string_lossy() }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(!outside.exists());
}

// ---------------------------------------------------------------------------
// 04 detect / generative：模型未装必须 403（协议 §5「模型未安装」）
// ---------------------------------------------------------------------------

#[test]
fn model_backed_endpoints_report_capability_missing_without_models() {
    let (tmp, work) = tmp_workdir("models");
    let gw = start_gateway(&work);
    let img = make_png(tmp.path(), "asset_320x240.png", 320, 240);
    let p = img.to_string_lossy().to_string();

    for (path, cap) in [
        ("/detect/u2net", "detect.u2net"),
        ("/detect/ocr", "detect.ocr"),
        ("/op/replace_bg", "op.generative"),
    ] {
        let r = post(&gw, path, json!({ "path": p, "out": work.join("o.png").to_string_lossy() }));
        assert_eq!(r.status, 403, "{path} → {}", r.body);
        assert_eq!(r.body["error"], "capability_missing", "{path}");
        assert_eq!(r.body["capability"], cap, "{path}");
    }

    // /detect/bbox 复用 detect.u2net（同一次推理的掩码），模型未装同样 403
    let r = post(&gw, "/detect/bbox", json!({ "path": p }));
    assert_eq!(r.status, 403, "{}", r.body);
    assert_eq!(r.body["error"], "capability_missing");
    assert_eq!(r.body["capability"], "detect.u2net");

    // inpaint：掩码不存在 → 404（参数层先于模型层，错误定位才准）
    let r = post(
        &gw,
        "/op/inpaint",
        json!({ "path": p, "mask": "/nonexistent/mask.png",
                "out": work.join("i.png").to_string_lossy() }),
    );
    assert_eq!(r.status, 404, "{}", r.body);
}

// ---------------------------------------------------------------------------
// 04b /detect/white_stats（协议 1.1）：纯 Rust 像素统计，不需要模型
// ---------------------------------------------------------------------------

/// 生成一张「边框 = border、内部 = inner」的真 PNG（白边统计要可控的边缘带像素）。
fn make_border_png(dir: &Path, name: &str, side: i32, border: [u8; 3], inner: [u8; 3],
                   band: i32) -> PathBuf {
    let mut rgb = vec![0u8; (side as usize) * (side as usize) * 3];
    for y in 0..side {
        for x in 0..side {
            let on_edge = x < band || x >= side - band || y < band || y >= side - band;
            let c = if on_edge { border } else { inner };
            let i = ((y * side + x) * 3) as usize;
            rgb[i..i + 3].copy_from_slice(&c);
        }
    }
    let path = dir.join(name);
    core_media::encode_rgb_to(side, side, &rgb, &path, None).expect("生成白边测试图失败");
    path
}

#[test]
fn detect_white_edge_measures_edge_band_without_models() {
    let (tmp, work) = tmp_workdir("white");
    let gw = start_gateway(&work);

    // 纯白边框（band=8）+ 深灰中心：边缘带偏差 0 → pass，中心不参与统计
    let pure = make_border_png(tmp.path(), "pure_400x400.png", 400, [255, 255, 255], [40, 40, 40], 8);
    let r = post(&gw, "/detect/white_stats", json!({ "path": pure.to_string_lossy(), "band": 8 }));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["mean_dev"], 0.0);
    assert_eq!(r.body["worst_dev"], 0.0);
    assert_eq!(r.body["band"], 8);
    assert_eq!(r.body["edge_pixels"], 400 * 400 - 384 * 384);
    assert_eq!(r.body["pass"], true);
    assert_eq!(r.body["engine"], "classic-white-edge");

    // 偏白边框 240：偏差 = 255-240 = 15 → mean_dev 15，默认阈值 8 判不合格
    let off = make_border_png(tmp.path(), "off_400x400.png", 400, [240, 240, 240], [40, 40, 40], 8);
    let r = post(&gw, "/detect/white_stats", json!({ "path": off.to_string_lossy(), "band": 8 }));
    assert_eq!(r.status, 200, "{}", r.body);
    assert!((r.body["mean_dev"].as_f64().unwrap() - 15.0).abs() < 1e-9, "{}", r.body);
    assert_eq!(r.body["pass"], false);
    // 阈值放宽到 20 → 同一张图同一统计变合格（判定阈值不由宿主编死）
    let r = post(&gw, "/detect/white_stats",
                 json!({ "path": off.to_string_lossy(), "band": 8, "max_dev": 20 }));
    assert_eq!(r.body["pass"], true, "{}", r.body);

    // 色偏比单通道均值敏感：纯蓝边框 (0,0,255) → 偏差 = 255-0 = 255
    let blue = make_border_png(tmp.path(), "blue_400x400.png", 400, [0, 0, 255], [255, 255, 255], 8);
    let r = post(&gw, "/detect/white_stats", json!({ "path": blue.to_string_lossy(), "band": 8 }));
    assert_eq!(r.body["mean_dev"], 255.0, "{}", r.body);
    assert_eq!(r.body["worst_dev"], 255.0);

    // 确定性：同图同参数两次调用逐值相等（协议 §7 铁律 2）
    let a = post(&gw, "/detect/white_stats", json!({ "path": off.to_string_lossy() }));
    let b = post(&gw, "/detect/white_stats", json!({ "path": off.to_string_lossy() }));
    assert_eq!(a.body, b.body);

    // band 越界 / 非整数 → 400；缺 path → 400；文件不存在 → 404
    for bad in [json!({ "path": off.to_string_lossy(), "band": 0 }),
                json!({ "path": off.to_string_lossy(), "band": "8" }),
                json!({ "path": off.to_string_lossy(), "max_dev": 300 })] {
        let r = post(&gw, "/detect/white_stats", bad.clone());
        assert_eq!(r.status, 400, "{bad} → {}", r.body);
        assert_eq!(r.body["error"], "invalid_params");
    }
    let r = post(&gw, "/detect/white_stats", json!({}));
    assert_eq!(r.status, 400, "{}", r.body);
    let r = post(&gw, "/detect/white_stats", json!({ "path": "/nonexistent/none.png" }));
    assert_eq!(r.status, 404, "{}", r.body);
    assert_eq!(r.body["error"], "asset_unreadable");
}

#[test]
fn detect_white_edge_needs_only_detect_white_capability() {
    let (tmp, work) = tmp_workdir("white-cap");
    use_empty_models_root();
    // 只授权 detect.white：白边可用，模型类端点仍 403（子集授权语义）
    let gw = Gateway::start(
        GatewayConfig::new(vec!["detect.white".into()], &work).with_token("t"),
    )
    .unwrap();
    let img = make_border_png(tmp.path(), "pure_200x200.png", 200, [255, 255, 255], [0, 0, 0], 4);
    let r = send(&gw, "POST", "/detect/white_stats", Some("t"),
                 &json!({ "path": img.to_string_lossy() }).to_string());
    assert_eq!(r.status, 200, "{}", r.body);
    let r = send(&gw, "POST", "/detect/bbox", Some("t"),
                 &json!({ "path": img.to_string_lossy() }).to_string());
    assert_eq!(r.status, 403, "{}", r.body);
    assert_eq!(r.body["capability"], "detect.u2net");
}

#[test]
fn detect_bbox_validates_params_before_model() {
    let (tmp, work) = tmp_workdir("bbox-params");
    let gw = start_gateway(&work);
    let img = make_png(tmp.path(), "asset_400x400.png", 400, 400);

    // target_ratio 越界/非数字 → 400（参数层，与模型是否可装无关）
    for bad in [json!(0), json!(1.5), json!("0.75"), json!(-0.2)] {
        let r = post(&gw, "/detect/bbox",
                     json!({ "path": img.to_string_lossy(), "target_ratio": bad }));
        assert_eq!(r.status, 400, "{bad} → {}", r.body);
        assert_eq!(r.body["error"], "invalid_params");
    }
    // 缺 path → 400；文件不存在 → 404
    let r = post(&gw, "/detect/bbox", json!({}));
    assert_eq!(r.status, 400, "{}", r.body);
    let r = post(&gw, "/detect/bbox", json!({ "path": "/nonexistent/none.png" }));
    assert_eq!(r.status, 404, "{}", r.body);
}

// ---------------------------------------------------------------------------
// 05 /report/emit 归账隔离（协议 §3.5 / §7 铁律 5）
// ---------------------------------------------------------------------------

#[test]
fn report_emit_is_accounted_per_job() {
    let (_tmp, work) = tmp_workdir("emit");
    let gw = start_gateway(&work);
    assert!(gw.emits("job-A").is_empty());

    for n in 0..2 {
        let r = post(
            &gw,
            "/report/emit",
            json!({ "job_id": "job-A", "entry": { "asset_id": format!("a{n}"), "verdicts": [] } }),
        );
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(r.body["ok"], true);
        assert_eq!(r.body["job_id"], "job-A");
    }
    let r = post(
        &gw,
        "/report/emit",
        json!({ "job_id": "job-B", "entry": { "asset_id": "b0", "verdicts": [] } }),
    );
    assert_eq!(r.status, 200);

    let a = gw.emits("job-A");
    let b = gw.emits("job-B");
    assert_eq!(a.len(), 2, "job-A 应收到 2 条");
    assert_eq!(b.len(), 1, "job-B 应收到 1 条");
    assert_eq!(a[0]["asset_id"], "a0");
    assert_eq!(b[0]["asset_id"], "b0");
    assert_eq!(gw.emit_jobs(), vec!["job-A".to_string(), "job-B".to_string()]);

    // 参数非法 → 400
    let r = post(&gw, "/report/emit", json!({ "job_id": "", "entry": {} }));
    assert_eq!(r.status, 400);
    let r = post(&gw, "/report/emit", json!({ "job_id": "job-C", "entry": "not-an-object" }));
    assert_eq!(r.status, 400, "{}", r.body);
    // 失败的 emit 不能污染账本
    assert!(gw.emits("job-C").is_empty());
}

// ---------------------------------------------------------------------------
// 06 /vl/judge：无后端 403；后端不可判 → undetermined（协议 §7 铁律 1）
// ---------------------------------------------------------------------------

/// 测试用 VL 后端：按文件名约定回结论（与 conformance 假网关同一套约定，便于交叉验证）。
#[derive(Debug)]
struct StubVl {
    seen: std::sync::Mutex<Vec<(String, u32)>>,
}

impl VlJudge for StubVl {
    fn judge(&self, path: &Path, prompt: &str, max_tokens: u32) -> Result<VlReply, VlError> {
        self.seen
            .lock()
            .unwrap()
            .push((prompt.to_string(), max_tokens));
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.contains("vlundet") {
            Ok(VlReply::undetermined("vl_parse_failed"))
        } else if name.contains("vlerror") {
            Err(VlError::Unavailable("endpoint 401".into()))
        } else {
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
}

#[test]
fn vl_judge_maps_backend_states_to_protocol() {
    let (tmp, work) = tmp_workdir("vl");
    let img = make_png(tmp.path(), "asset_800x800.png", 800, 800);
    let vlundet = make_png(tmp.path(), "vlundet_800x800.png", 800, 800);
    let vlerror = make_png(tmp.path(), "vlerror_800x800.png", 800, 800);

    // 未装配后端 → 403 capability_missing（模型/通道未装，不是「不可判定」）
    let gw = start_gateway(&work);
    let r = post(
        &gw,
        "/vl/judge",
        json!({ "path": img.to_string_lossy(), "prompt": "是否含促销文字？", "max_tokens": 512 }),
    );
    assert_eq!(r.status, 403, "{}", r.body);
    assert_eq!(r.body["capability"], "vl.judge");

    let stub = std::sync::Arc::new(StubVl { seen: Default::default() });
    let gw = start_gateway_with_vl(&work, stub.clone());

    // 正常判定 → 200 + rules
    let r = post(
        &gw,
        "/vl/judge",
        json!({ "path": img.to_string_lossy(), "prompt": "是否含促销文字？", "max_tokens": 512 }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["undetermined"], false);
    assert_eq!(r.body["rules"][0]["status"], "pass");
    assert_eq!(r.body["rules"][0]["label"], "apparel");
    assert_eq!(r.body["raw"], "{\"status\":\"pass\"}");
    assert_eq!(stub.seen.lock().unwrap()[0].1, 512, "max_tokens 必须原样透传给后端");

    // 后端说不可判 → 200 undetermined:true + reason（插件据此落 undetermined）
    let r = post(
        &gw,
        "/vl/judge",
        json!({ "path": vlundet.to_string_lossy(), "prompt": "类目？" }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["undetermined"], true);
    assert_eq!(r.body["reason"], "vl_parse_failed");
    // 未给 max_tokens → 用协议上限
    assert_eq!(stub.seen.lock().unwrap().last().unwrap().1, VL_MAX_TOKENS_CAP);

    // 后端端点挂了（401/超时）→ 也是 undetermined，绝不冒充 pass/fail
    let r = post(
        &gw,
        "/vl/judge",
        json!({ "path": vlerror.to_string_lossy(), "prompt": "类目？" }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["undetermined"], true);
    assert!(
        r.body["reason"].as_str().unwrap_or("").starts_with("vl_unavailable"),
        "{}",
        r.body
    );

    // max_tokens 超上限 → 400（协议 §6 上限 4096，不静默截断）
    let r = post(
        &gw,
        "/vl/judge",
        json!({ "path": img.to_string_lossy(), "prompt": "p", "max_tokens": 5000 }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body["detail"].as_str().unwrap_or("").contains("4096"));
    // 0 也不合法
    let r = post(
        &gw,
        "/vl/judge",
        json!({ "path": img.to_string_lossy(), "prompt": "p", "max_tokens": 0 }),
    );
    assert_eq!(r.status, 400);
    // prompt 空 → 400
    let r = post(&gw, "/vl/judge", json!({ "path": img.to_string_lossy(), "prompt": "" }));
    assert_eq!(r.status, 400);
    // 图不在盘 → 404
    let r = post(&gw, "/vl/judge", json!({ "path": "/nope/x.png", "prompt": "p" }));
    assert_eq!(r.status, 404);
}

// ---------------------------------------------------------------------------
// 07 网关自身健壮性：并发请求互不串、超大体拒绝
// ---------------------------------------------------------------------------

#[test]
fn concurrent_requests_do_not_cross_talk() {
    let (tmp, work) = tmp_workdir("concurrent");
    let gw = std::sync::Arc::new(start_gateway(&work));
    let img = make_png(tmp.path(), "asset_900x600.png", 900, 600);

    let mut handles = Vec::new();
    for n in 0..8 {
        let gw = gw.clone();
        let img = img.clone();
        let work = work.clone();
        handles.push(std::thread::spawn(move || {
            let out = work.join(format!("c{n}.png"));
            let r = post(
                &gw,
                "/op/crop",
                json!({ "path": img.to_string_lossy(), "box": [10, 10, 110, 110],
                        "out": out.to_string_lossy() }),
            );
            assert_eq!(r.status, 200, "并发请求 {n} 失败：{}", r.body);
            assert!(out.is_file());
            (r.body["width"].clone(), r.body["height"].clone())
        }));
    }
    for h in handles {
        assert_eq!(h.join().unwrap(), (json!(100), json!(100)));
    }

    // 请求体超上限 → 413（协议不带图，超这个数是客户端搞错了）
    let big = "x".repeat(MAX_BODY_BYTES + 16);
    let r = post_raw(&gw, "/io/read", &big);
    assert_eq!(r.status, 413, "{}", r.body);
}

// ---------------------------------------------------------------------------
// 08 卡面验收口径：未声明的能力必须 403 且**不产生副作用**；令牌一次性（重启即废）
// ---------------------------------------------------------------------------

#[test]
fn undeclared_capability_is_denied_without_side_effects() {
    let (tmp, work) = tmp_workdir("deny");
    use_empty_models_root();
    // manifest 只声明 io.read + op.geometric（**没有** op.generative）
    let gw = Gateway::start(
        GatewayConfig::new(
            vec!["io.read".into(), "op.geometric".into()],
            &work,
        )
        .with_token("test-token"),
    )
    .unwrap();
    let img = make_png(tmp.path(), "asset_400x300.png", 400, 300);
    let mask = make_png(tmp.path(), "mask_400x300.png", 400, 300);

    let out = work.join("inpaint-out.png");
    let r = post(
        &gw,
        "/op/inpaint",
        json!({ "path": img.to_string_lossy(), "mask": mask.to_string_lossy(),
                "out": out.to_string_lossy() }),
    );
    assert_eq!(r.status, 403, "{}", r.body);
    assert_eq!(r.body["error"], "capability_missing");
    assert_eq!(r.body["capability"], "op.generative");
    assert!(!out.exists(), "被拒的请求不允许留下产物");
    let leftovers: Vec<_> = std::fs::read_dir(&work)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .filter(|n| n != "inpaint-out.png")
        .collect();
    assert!(leftovers.is_empty(), "被拒的请求不允许留下任何中间文件：{leftovers:?}");

    // 同一包声明了 op.geometric → crop 放行（白名单是按能力放，不是一刀切拒）
    let ok = work.join("crop-ok.png");
    let r = post(
        &gw,
        "/op/crop",
        json!({ "path": img.to_string_lossy(), "box": [0, 0, 100, 100],
                "out": ok.to_string_lossy() }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(ok.is_file());
}

#[test]
fn token_is_one_shot_per_gateway_instance() {
    let (tmp, work) = tmp_workdir("oneshot");
    use_empty_models_root();
    let img = make_png(tmp.path(), "asset_320x320.png", 320, 320);
    let body = json!({ "path": img.to_string_lossy() });

    // 第一代：随机令牌（生产口径 = 每次启动生成）
    let first = Gateway::start(GatewayConfig::new(caps(), &work)).unwrap();
    let old_token = first.token().to_string();
    let r = post(&first, "/io/read", body.clone());
    assert_eq!(r.status, 200, "首启令牌必须可用：{}", r.body);
    first.stop();

    // 第二代：新实例、新令牌；旧令牌立即失效（网关随包进程生命周期重建）
    let second = Gateway::start(GatewayConfig::new(caps(), &work)).unwrap();
    assert_ne!(second.token(), old_token, "每次启动必须换令牌");
    let r = send(&second, "POST", "/io/read", Some(&old_token), &body.to_string());
    assert_eq!(r.status, 401, "旧令牌打新网关必须 401：{}", r.body);
    let r = post(&second, "/io/read", body);
    assert_eq!(r.status, 200, "新令牌必须可用：{}", r.body);
}
