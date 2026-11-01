//! `/op/encode`（N6b 编码组）网关面单测：转格式 / 质量 / alpha / EXIF 方向 / 参数边界。
//!
//! 与 `gateway_endpoints.rs` 同一个套路：本机最小 HTTP 客户端 + 真网关 + 真像素
//! （**不依赖任何模型**——编码是纯 Rust 像素算子，`op.geometric` 能力）。
//!
//! 断言口径（卡 t_83afd618）：
//! 1. 格式：产物魔数/尺寸真对（jpg = FFD8、avif = `ftypavif`），不是"看着成功"；
//! 2. alpha：源带透明 → png **逐字节保留** alpha；jpeg 无 alpha → 按 background 合成
//!    （透明像素必须变成背景色，不许静默丢通道）；
//! 3. 方向：EXIF Orientation=6 的图默认摆正（宽高互换 + 像素落到该到的地方），
//!    `auto_orient:false` 则原样；
//! 4. 质量：jpeg/avif 生效（字节随 q 单调），png/webp 无损 → `quality_applied:false`；
//! 5. 错误面：format 与 out 扩展名不一致 / quality 越界 / out==path / 越界写根 / 未声明能力。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Once;

use pack_host::gateway::ENDPOINTS;
use pack_host::{Gateway, GatewayConfig};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

static ENV_ONCE: Once = Once::new();

fn use_empty_models_root() {
    let root = std::env::temp_dir().join(format!("pack-host-empty-models-enc-{}", std::process::id()));
    std::fs::create_dir_all(root.join("models")).unwrap();
    ENV_ONCE.call_once(|| {
        std::env::set_var("DAM_MODELS_ROOT", &root);
    });
}

fn caps() -> Vec<String> {
    ENDPOINTS.iter().map(|(_, cap)| cap.to_string()).collect()
}

fn start_gateway(work_dir: &Path) -> Gateway {
    use_empty_models_root();
    Gateway::start(GatewayConfig::new(caps(), work_dir).with_token("test-token"))
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

fn tmp_workdir(tag: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let work = dir.path().join(format!("work-{tag}"));
    std::fs::create_dir_all(&work).unwrap();
    (dir, work)
}

/// 不透明 RGB PNG（纯色 + 中心亮块），给转格式/质量用例当源。
fn make_png(dir: &Path, name: &str, w: i32, h: i32) -> PathBuf {
    let mut rgb = vec![90u8; (w as usize) * (h as usize) * 3];
    for y in (h / 3)..(2 * h / 3) {
        for x in (w / 3)..(2 * w / 3) {
            let i = ((y * w + x) * 3) as usize;
            rgb[i] = 250;
            rgb[i + 1] = 200;
            rgb[i + 2] = 40;
        }
    }
    let path = dir.join(name);
    core_media::encode_rgb_to(w, h, &rgb, &path, None).expect("生成测试图失败");
    path
}

/// 带**真 alpha 梯度**的 RGBA PNG（左全透明 → 右不透明），给 alpha 用例当源。
fn make_rgba_png(dir: &Path, name: &str, w: i32, h: i32) -> PathBuf {
    let mut rgba = vec![0u8; (w as usize) * (h as usize) * 4];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            rgba[i] = 20;
            rgba[i + 1] = 160;
            rgba[i + 2] = 60;
            rgba[i + 3] = ((x * 255) / (w - 1).max(1)) as u8;
        }
    }
    let path = dir.join(name);
    core_media::encode_rgba_to(w, h, &rgba, &path, None, None).expect("生成 alpha 测试图失败");
    path
}

/// 给 JPEG 塞一个 `Exif IFD0.Orientation`（APP1），像素原样不动。
///
/// 语义：Orientation=6 表示「显示时顺时针转 90°」——即存储的是横图，
/// 摆正后应变成竖图（宽高互换）。造这种图是验证「方向处理正确」的唯一办法。
fn with_exif_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "源不是 JPEG");
    let mut tiff: Vec<u8> = Vec::new();
    tiff.extend_from_slice(b"II"); // 小端
    tiff.extend_from_slice(&42u16.to_le_bytes());
    tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 偏移
    tiff.extend_from_slice(&1u16.to_le_bytes()); // 1 个条目
    tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
    tiff.extend_from_slice(&3u16.to_le_bytes()); // SHORT
    tiff.extend_from_slice(&1u32.to_le_bytes()); // count
    tiff.extend_from_slice(&orientation.to_le_bytes()); // 值（SHORT 左对齐）
    tiff.extend_from_slice(&[0u8, 0u8]);
    tiff.extend_from_slice(&0u32.to_le_bytes()); // 无 IFD1
    let mut app1: Vec<u8> = Vec::new();
    app1.extend_from_slice(b"Exif\0\0");
    app1.extend_from_slice(&tiff);
    let len = (app1.len() + 2) as u16;
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(&[0xFF, 0xD8]);
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&app1);
    out.extend_from_slice(&jpeg[2..]);
    out
}

fn exif_jpeg(dir: &Path, name: &str, w: i32, h: i32, orientation: u16) -> PathBuf {
    let plain = make_png(dir, "tmp_plain.png", w, h);
    let tmp_jpg = dir.join("tmp_plain.jpg");
    let px = core_media::rgba8(&plain).unwrap();
    core_media::encode_rgba_to(px.0, px.1, &px.2, &tmp_jpg, Some(92), None).unwrap();
    let bytes = std::fs::read(&tmp_jpg).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, with_exif_orientation(&bytes, orientation)).unwrap();
    std::fs::remove_file(&tmp_jpg).ok();
    std::fs::remove_file(&plain).ok();
    path
}

fn magic4(path: &Path) -> [u8; 4] {
    let raw = std::fs::read(path).unwrap();
    [raw[0], raw[1], raw[2], raw[3]]
}

/// ISO-BMFF 的 `ftyp` box 品牌（avif 判定：offset 4..12 == "ftypavif"）。
fn ftyp_brand(path: &Path) -> String {
    let raw = std::fs::read(path).unwrap();
    String::from_utf8_lossy(&raw[4..12]).to_string()
}

// ---------------------------------------------------------------------------
// 01 转格式：png → jpg / webp / avif 都是真产物
// ---------------------------------------------------------------------------

#[test]
fn encode_converts_png_to_jpg_webp_avif() {
    let (tmp, work) = tmp_workdir("conv");
    let gw = start_gateway(&work);
    let src = make_png(tmp.path(), "src_320x240.png", 320, 240);

    for (name, magic) in [("o.jpg", Some([0xFF, 0xD8, 0xFF, 0xE0])), ("o.webp", None)] {
        let out = work.join(name);
        let r = post(
            &gw,
            "/op/encode",
            json!({ "path": src.to_string_lossy(), "out": out.to_string_lossy(), "quality": 80 }),
        );
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(r.body["width"], 320);
        assert_eq!(r.body["height"], 240);
        assert_eq!(r.body["bytes"], std::fs::metadata(&out).unwrap().len());
        assert!(r.body["bytes"].as_u64().unwrap() > 0);
        assert_eq!(core_media::image_size(&out).unwrap(), (320, 240));
        if let Some(m) = magic {
            // JPEG 起始就是 FFD8FF（APP0 起点）；不同编码器第 4 字节可能是 E0/E1
            assert_eq!(&magic4(&out)[..2], &m[..2], "{name} 魔数不对");
        } else {
            assert_eq!(&magic4(&out)[..4], b"RIFF", "webp 必须是 RIFF 容器");
        }
    }

    // avif：真 ISO-BMFF + ftypavif；尺寸由宿主 vips 解出来对得上
    let out = work.join("o.avif");
    let r = post(
        &gw,
        "/op/encode",
        json!({ "path": src.to_string_lossy(), "out": out.to_string_lossy(), "quality": 60 }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["format"], "avif");
    assert_eq!(ftyp_brand(&out), "ftypavif", "avif 必须是 ftypavif 品牌");
    assert_eq!(core_media::image_size(&out).unwrap(), (320, 240));
    assert!(r.body["quality_applied"].as_bool().unwrap());
}

#[test]
fn encode_format_field_must_agree_with_out_extension() {
    let (tmp, work) = tmp_workdir("fmtmismatch");
    let gw = start_gateway(&work);
    let src = make_png(tmp.path(), "src.png", 64, 48);

    // 一致 → 200
    let out = work.join("ok.png");
    let r = post(
        &gw,
        "/op/encode",
        json!({ "path": src.to_string_lossy(), "out": out.to_string_lossy(), "format": "png" }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    // jpg/jpeg 视为同一格式（不因别名报错）
    let out = work.join("ok2.jpeg");
    let r = post(
        &gw,
        "/op/encode",
        json!({ "path": src.to_string_lossy(), "out": out.to_string_lossy(), "format": "jpg" }),
    );
    assert_eq!(r.status, 200, "{}", r.body);

    // 不一致 → 400，且 detail 点名两边
    let out = work.join("bad.webp");
    let r = post(
        &gw,
        "/op/encode",
        json!({ "path": src.to_string_lossy(), "out": out.to_string_lossy(), "format": "png" }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    assert_eq!(r.body["error"], "invalid_params");
    let detail = r.body["detail"].as_str().unwrap_or("");
    assert!(detail.contains("不一致"), "detail 必须说清哪里不一致：{detail}");
    assert!(!out.exists(), "参数不合法时不许留下半成品");

    // 未知格式 → 400
    let out = work.join("bad.tif");
    let r = post(
        &gw,
        "/op/encode",
        json!({ "path": src.to_string_lossy(), "out": out.to_string_lossy() }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body["detail"].as_str().unwrap_or("").contains("tif"));
}

// ---------------------------------------------------------------------------
// 02 质量：jpeg/avif 生效且单调；png/webp 无损 → 不假装生效
// ---------------------------------------------------------------------------

#[test]
fn encode_quality_affects_lossy_and_is_reported_for_lossless() {
    let (tmp, work) = tmp_workdir("quality");
    let gw = start_gateway(&work);
    // 造一张有噪点的图（纯色图在 jpeg 下字节几乎不随 q 变，量不出单调）
    let mut rgb = vec![0u8; 400 * 300 * 3];
    for i in 0..rgb.len() {
        rgb[i] = ((i * 37 + i / 7) % 256) as u8;
    }
    let src = tmp.path().join("noise.png");
    core_media::encode_rgb_to(400, 300, &rgb, &src, None).unwrap();

    let q90 = work.join("q90.jpg");
    let q30 = work.join("q30.jpg");
    let r90 = post(
        &gw,
        "/op/encode",
        json!({ "path": src.to_string_lossy(), "out": q90.to_string_lossy(), "quality": 90 }),
    );
    assert_eq!(r90.status, 200, "{}", r90.body);
    assert_eq!(r90.body["quality"], 90);
    assert!(r90.body["quality_applied"].as_bool().unwrap(), "jpeg 的 quality 必须真生效");
    let r30 = post(
        &gw,
        "/op/encode",
        json!({ "path": src.to_string_lossy(), "out": q30.to_string_lossy(), "quality": 30 }),
    );
    assert_eq!(r30.status, 200, "{}", r30.body);
    let (b90, b30) = (
        r90.body["bytes"].as_u64().unwrap(),
        r30.body["bytes"].as_u64().unwrap(),
    );
    assert!(b30 < b90, "q30({b30}) 必须显著小于 q90({b90})");

    // png 无损：quality 无意义 → quality_applied=false（不假装生效）
    let out = work.join("q30.png");
    let r = post(
        &gw,
        "/op/encode",
        json!({ "path": src.to_string_lossy(), "out": out.to_string_lossy(), "quality": 30 }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["quality"], 30, "请求值原样回显");
    assert!(!r.body["quality_applied"].as_bool().unwrap(), "png 是无损容器");

    // quality 越界/类型错 → 400
    for bad in [json!(0), json!(101), json!("high"), json!(50.5)] {
        let out = work.join("bad.jpg");
        let r = post(
            &gw,
            "/op/encode",
            json!({ "path": src.to_string_lossy(), "out": out.to_string_lossy(), "quality": bad }),
        );
        assert_eq!(r.status, 400, "quality={bad} 应 400，实际 {}", r.body);
        assert_eq!(r.body["error"], "invalid_params");
    }
}

// ---------------------------------------------------------------------------
// 03 alpha：png 逐字节保留；jpeg 按 background 合成（不静默丢通道）
// ---------------------------------------------------------------------------

#[test]
fn encode_keeps_alpha_for_png_and_flattens_for_jpeg() {
    let (tmp, work) = tmp_workdir("alpha");
    let gw = start_gateway(&work);
    let src = make_rgba_png(tmp.path(), "grad_200x120.png", 200, 120);
    let (sw, sh, src_px) = core_media::rgba8(&src).unwrap();
    assert_eq!((sw, sh), (200, 120));
    assert!(core_media::has_transparency(&src_px), "夹具必须真的带透明像素");

    // png：alpha 通道**逐字节**一致
    let out_png = work.join("keep.png");
    let r = post(
        &gw,
        "/op/encode",
        json!({ "path": src.to_string_lossy(), "out": out_png.to_string_lossy() }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["source_alpha"], true);
    assert_eq!(r.body["alpha"], "kept", "png 必须保留 alpha");
    let (ow, oh, out_px) = core_media::rgba8(&out_png).unwrap();
    assert_eq!((ow, oh), (200, 120));
    let alpha_src: Vec<u8> = src_px.chunks_exact(4).map(|p| p[3]).collect();
    let alpha_out: Vec<u8> = out_px.chunks_exact(4).map(|p| p[3]).collect();
    assert_eq!(alpha_src, alpha_out, "png 输出的 alpha 通道必须与源逐像素一致");

    // jpeg：无 alpha 容器 → 按 background 合成；透明像素必须变成背景色
    let out_jpg = work.join("flat.jpg");
    let r = post(
        &gw,
        "/op/encode",
        json!({ "path": src.to_string_lossy(), "out": out_jpg.to_string_lossy(),
                "background": "#ff0000", "quality": 95 }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["source_alpha"], true);
    assert_eq!(r.body["alpha"], "flattened", "jpeg 只能合成，不能保留");
    let (jw, jh, jpg_px) = core_media::rgba8(&out_jpg).unwrap();
    assert_eq!((jw, jh), (200, 120));
    assert!(
        jpg_px.chunks_exact(4).all(|p| p[3] == 255),
        "jpeg 产物解码回来不应再有透明像素"
    );
    // 源图最左一列 alpha=0（纯透明）→ 合成后应≈背景色（jpeg 有量化误差，容差 12）
    let left = &jpg_px[..4];
    assert!(
        left[0] > 200 && left[1] < 60 && left[2] < 60,
        "透明像素应合成为 #ff0000，实际 {:?}",
        left
    );

    // 不透明的源回 absent（不要把"没 alpha"报成"保留了 alpha"）
    let flat_src = make_png(tmp.path(), "flat.png", 32, 32);
    let out = work.join("flat2.png");
    let r = post(
        &gw,
        "/op/encode",
        json!({ "path": flat_src.to_string_lossy(), "out": out.to_string_lossy() }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["source_alpha"], false);
    assert_eq!(r.body["alpha"], "absent");
}

// ---------------------------------------------------------------------------
// 04 EXIF 方向：Orientation=6 默认摆正（宽高互换 + 像素对位），可关
// ---------------------------------------------------------------------------

#[test]
fn encode_applies_exif_orientation() {
    let (tmp, work) = tmp_workdir("orient");
    let gw = start_gateway(&work);
    let src = exif_jpeg(tmp.path(), "rot6.jpg", 320, 180, 6);
    let (sw, sh, src_px) = core_media::rgba8(&src).unwrap();
    assert_eq!((sw, sh), (320, 180), "存储尺寸是横图，方向靠 EXIF 表达");

    let out = work.join("upright.png");
    let r = post(
        &gw,
        "/op/encode",
        json!({ "path": src.to_string_lossy(), "out": out.to_string_lossy() }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["orientation"], 6, "必须报告应用的 EXIF 方向");
    assert_eq!(r.body["width"], 180, "转 90° 后宽高互换");
    assert_eq!(r.body["height"], 320);

    // 像素对位：Orientation=6 → out(x,y) = src(y, h-1-x)
    let (ow, oh, out_px) = core_media::rgba8(&out).unwrap();
    assert_eq!((ow, oh), (180, 320));
    for (x, y) in [(0usize, 0usize), (10, 5), (179, 319), (90, 200)] {
        // Orientation=6 → out(x,y) = src(sx=y, sy=sh-1-x)
        let si = (((sh as usize) - 1 - x) * sw as usize + y) * 4;
        let di = (y * ow as usize + x) * 4;
        assert_eq!(
            &out_px[di..di + 4],
            &src_px[si..si + 4],
            "({x},{y}) 的像素没落到该到的地方"
        );
    }

    // auto_orient:false → 原样（尺寸不变、orientation=1）
    let out2 = work.join("raw.jpg");
    let r = post(
        &gw,
        "/op/encode",
        json!({ "path": src.to_string_lossy(), "out": out2.to_string_lossy(),
                "auto_orient": false }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["orientation"], 1);
    assert_eq!((r.body["width"].as_i64(), r.body["height"].as_i64()), (Some(320), Some(180)));

    // 没有 EXIF 的图：orientation=1（不假装摆正过）
    let plain = make_png(tmp.path(), "plain.png", 40, 30);
    let out3 = work.join("plain.jpg");
    let r = post(
        &gw,
        "/op/encode",
        json!({ "path": plain.to_string_lossy(), "out": out3.to_string_lossy() }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["orientation"], 1);
}

// ---------------------------------------------------------------------------
// 05 错误面：路径 / 能力 / 可读性
// ---------------------------------------------------------------------------

#[test]
fn encode_rejects_unsafe_or_unreadable_requests() {
    let (tmp, work) = tmp_workdir("errs");
    let gw = start_gateway(&work);
    let src = make_png(tmp.path(), "src.png", 32, 32);

    // out 等于源：原图只读，禁止原地重编码（源要落在写根里，才轮到这条校验）
    let inside = make_png(&work, "inside.png", 32, 32);
    let r = post(
        &gw,
        "/op/encode",
        json!({ "path": inside.to_string_lossy(), "out": inside.to_string_lossy() }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(
        r.body["detail"].as_str().unwrap_or("").contains("out 不能等于"),
        "必须点名禁止原地重编码：{}",
        r.body
    );

    // out 越界（写根之外）
    let outside = tmp.path().join("outside.png");
    let r = post(
        &gw,
        "/op/encode",
        json!({ "path": src.to_string_lossy(), "out": outside.to_string_lossy() }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    assert_eq!(r.body["error"], "out_of_workroot");
    assert!(!outside.exists());

    // 源不存在 → 404 asset_unreadable
    let out = work.join("x.jpg");
    let r = post(
        &gw,
        "/op/encode",
        json!({ "path": "/nonexistent/never.png", "out": out.to_string_lossy() }),
    );
    assert_eq!(r.status, 404, "{}", r.body);
    assert_eq!(r.body["error"], "asset_unreadable");

    // 源不是图 → 404（解不开就是解不开，不返回空产物）
    let junk = tmp.path().join("junk.bin");
    std::fs::write(&junk, b"not an image at all").unwrap();
    let r = post(
        &gw,
        "/op/encode",
        json!({ "path": junk.to_string_lossy(), "out": out.to_string_lossy() }),
    );
    assert_eq!(r.status, 404, "{}", r.body);

    // 缺 path / 相对 path → 400
    let r = post(&gw, "/op/encode", json!({ "out": out.to_string_lossy() }));
    assert_eq!(r.status, 400, "{}", r.body);
    let r = post(
        &gw,
        "/op/encode",
        json!({ "path": "relative.png", "out": out.to_string_lossy() }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body["detail"].as_str().unwrap_or("").contains("绝对路径"));

    // 未声明 op.geometric → 403 capability_missing
    use_empty_models_root();
    let narrow = Gateway::start(
        GatewayConfig::new(vec!["io.read".into()], &work).with_token("t2"),
    )
    .unwrap();
    let r = send(
        &narrow,
        "POST",
        "/op/encode",
        Some(narrow.token()),
        &json!({ "path": src.to_string_lossy(), "out": out.to_string_lossy() }).to_string(),
    );
    assert_eq!(r.status, 403, "{}", r.body);
    assert_eq!(r.body["error"], "capability_missing");
    assert_eq!(r.body["capability"], "op.geometric");
}
