//! `/op/watermark`（协议 1.1 追加端点，能力 `op.overlay`；N6c `task`）。
//!
//! 这一组验的是**叠加语义**本身，不依赖任何模型（纯 `core-media` + ab_glyph + 内嵌字体）：
//!
//! 1. **真的落墨**：指定角落出现与底色不同的像素，其他角落一个都不许变；
//! 2. **尺寸不变**：叠加不是缩放，返回的宽高必须等于源图（JPEG/PNG 两条编码路径都验）；
//! 3. **透明度单调**：`opacity` 越大，墨水离颜色越近（20 → 100 的平均亮度必须下降）；
//! 4. **参数错响亮**：空文字 / 换行 / opacity 0 或 101 / 字号越界 / 边距过大 / 未知锚点 /
//!    三位颜色 → 一律 400 `invalid_params`（绝不静默出个跟原图一样的产物）；
//! 5. **能力缺失 403**：没声明 `op.overlay` 的网关必须拒（插件据此回 `-32001`）；
//! 6. **写根边界**：`out` 落在 workdir 之外 → 400 `out_of_workroot`。
//!
//! 模型根固定为空目录：本端点不该碰任何 ONNX 模型，跑不通这条说明实现串了层。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use pack_host::gateway::{Gateway, GatewayConfig, ENDPOINTS};
use serde_json::{json, Value};

const TOKEN: &str = "watermark-token";

fn empty_models_root() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("pack-host-wm-models-{}", std::process::id()));
        std::fs::create_dir_all(root.join("models")).unwrap();
        std::env::set_var("DAM_MODELS_ROOT", &root);
        root
    })
    .clone()
}

fn all_caps() -> Vec<String> {
    ENDPOINTS.iter().map(|(_, cap)| cap.to_string()).collect()
}

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pack-host-wm-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn start_gateway(work: &Path, caps: Option<Vec<String>>) -> Gateway {
    empty_models_root();
    let cfg = GatewayConfig::new(caps.unwrap_or_else(all_caps), work).with_token(TOKEN);
    Gateway::start(cfg).expect("网关启动失败")
}

struct Response {
    status: u16,
    body: Value,
}

fn post(gw: &Gateway, path: &str, body: Value) -> Response {
    let addr = gw.base_url().replace("http://", "");
    let mut stream = TcpStream::connect(&addr).expect("连接网关失败");
    let payload = body.to_string();
    let req = format!(
        "POST {path} HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nauthorization: Bearer {}\r\nconnection: close\r\n\r\n{payload}",
        payload.len(),
        gw.token()
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").expect("应答没有分隔头体");
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .expect("应答状态行不合法");
    let body: Value = serde_json::from_str(body).expect("应答体不是 JSON");
    Response { status, body }
}

fn post_watermark(gw: &Gateway, body: Value) -> Response {
    post(gw, "/op/watermark", body)
}

/// 平底色图（PNG 或 JPEG，按扩展名）。
fn make_flat(dir: &Path, name: &str, w: i32, h: i32, rgb: [u8; 3], quality: Option<u8>) -> PathBuf {
    let px = rgb
        .iter()
        .copied()
        .cycle()
        .take((w as usize) * (h as usize) * 3)
        .collect::<Vec<u8>>();
    let path = dir.join(name);
    core_media::encode_rgb_to(w, h, &px, &path, quality).expect("生成测试图失败");
    path
}

/// 逐像素读回（core-media 的 rgb8：整图解码）。
fn read_rgb(path: &Path) -> (i32, i32, Vec<u8>) {
    core_media::rgb8(path).expect("读回产物失败")
}

/// 某个矩形内「与底色差异 > tol 的通道数」。
fn ink_count(px: &[u8], stride: i32, x0: i32, y0: i32, x1: i32, y1: i32, tol: i32) -> usize {
    let mut n = 0usize;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * stride + x) * 3) as usize;
            if (0..3).any(|c| (i32::from(px[i + c]) - 128).abs() > tol) {
                n += 1;
            }
        }
    }
    n
}

#[test]
fn watermark_lands_in_requested_corner_and_keeps_size() {
    let tmp = tmp_dir("corner");
    let work = tmp_dir("corner-work");
    let gw = start_gateway(&work, None);
    let (w, h) = (400, 300);
    let src = make_flat(&tmp, "src.png", w, h, [128, 128, 128], None);
    let out = work.join("wm-br.png");

    let r = post(
        &gw,
        "/op/watermark",
        json!({
            "path": src.to_string_lossy(),
            "text": "DAMDesk",
            "position": "bottom_right",
            "opacity": 100,
            "font_size": 32,
            "margin": 10,
            "color": "000000",
            "out": out.to_string_lossy(),
        }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["width"], w);
    assert_eq!(r.body["height"], h);
    assert_eq!(r.body["op"], "watermark");
    assert!(
        r.body["coverage"].as_f64().unwrap_or(0.0) > 0.0,
        "coverage 应 > 0（确实有字形面积）：{}",
        r.body
    );

    let (ow, oh, px) = read_rgb(&out);
    assert_eq!((ow, oh), (w, h), "叠加不得改变尺寸");

    // 右下 1/3 区必须落墨；左上 1/3 区一个像素都不许动（锚点真的生效）
    let br = ink_count(&px, ow, ow / 2, oh / 2, ow, oh, 8);
    let tl = ink_count(&px, ow, 0, 0, ow / 2, oh / 2, 8);
    assert!(br > 50, "右下角没落墨：{br}");
    assert_eq!(tl, 0, "左上角不该有墨：{tl}");
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn watermark_opacity_is_monotonic_and_supports_jpeg_source() {
    let tmp = tmp_dir("opacity");
    let work = tmp_dir("opacity-work");
    let gw = start_gateway(&work, None);
    let src = make_flat(&tmp, "src.jpg", 320, 240, [200, 200, 200], Some(92));
    let mut means = Vec::new();
    for opacity in [20u32, 100] {
        let out = work.join(format!("wm-{opacity}.jpg"));
        let r = post(
            &gw,
            "/op/watermark",
            json!({
                "path": src.to_string_lossy(),
                "text": "水印Watermark",
                "position": "center",
                "opacity": opacity,
                "font_size": 28,
                "margin": 4,
                "color": "000000",
                "out": out.to_string_lossy(),
            }),
        );
        assert_eq!(r.status, 200, "{}", r.body);
        // JPEG 源 + JPEG 产物：中英文混排都必须能渲染（字体缺字形 → 400）
        let (ow, oh, px) = read_rgb(&out);
        assert_eq!((ow, oh), (320, 240));
        let sum: u64 = px.iter().map(|&c| u64::from(c)).sum();
        means.push(sum as f64 / (px.len() as f64));
    }
    assert!(
        means[0] > means[1] + 0.5,
        "opacity 20 应比 100 更接近底色：{:?}",
        means
    );
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn bad_params_fail_loudly() {
    let tmp = tmp_dir("bad");
    let work = tmp_dir("bad-work");
    let gw = start_gateway(&work, None);
    let src = make_flat(&tmp, "src.png", 200, 150, [180, 180, 180], None);
    let out = work.join("x.png");
    let base = |extra: Value| {
        let mut v = json!({
            "path": src.to_string_lossy(),
            "text": "DAMDesk",
            "out": out.to_string_lossy(),
        });
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        v
    };
    let cases: Vec<(&str, Value)> = vec![
        ("空文字", base(json!({ "text": "   " }))),
        ("含换行", base(json!({ "text": "A\nB" }))),
        ("opacity=0", base(json!({ "opacity": 0 }))),
        ("opacity=101", base(json!({ "opacity": 101 }))),
        ("opacity 非整数", base(json!({ "opacity": 33.5 }))),
        ("字号过小", base(json!({ "font_size": 4 }))),
        ("字号过大", base(json!({ "font_size": 900 }))),
        ("边距过大", base(json!({ "margin": 100 }))),
        ("边距为负", base(json!({ "margin": -1 }))),
        ("未知锚点", base(json!({ "position": "middle_out" }))),
        ("三位颜色", base(json!({ "color": "fff" }))),
        (
            "缺 out",
            json!({ "path": src.to_string_lossy(), "text": "x" }),
        ),
    ];
    for (why, body) in cases {
        let r = post_watermark(&gw, body.clone());
        assert_eq!(r.status, 400, "{why} 应 400，实际 {}：{}", r.status, r.body);
        assert_eq!(r.body["error"], "invalid_params", "{why}：{}", r.body);
    }
    // 参数错必须**没有产物**（不静默出图）
    assert!(!out.exists(), "参数错不该写产物：{}", out.display());
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn capability_missing_is_403_and_not_silently_skipped() {
    let tmp = tmp_dir("caps");
    let work = tmp_dir("caps-work");
    // 只给 io.read：`op.overlay` 未声明 → 403（插件回 -32001，不假冒成功）
    let gw = start_gateway(&work, Some(vec!["io.read".into()]));
    let src = make_flat(&tmp, "src.png", 100, 80, [10, 10, 10], None);
    let out = work.join("y.png");
    let r = post(
        &gw,
        "/op/watermark",
        json!({
            "path": src.to_string_lossy(),
            "text": "DAMDesk",
            "out": out.to_string_lossy(),
        }),
    );
    assert_eq!(r.status, 403, "{}", r.body);
    assert_eq!(r.body["error"], "capability_missing");
    assert_eq!(r.body["capability"], "op.overlay");
    assert!(!out.exists());
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn out_outside_workroot_is_rejected() {
    let tmp = tmp_dir("root");
    let work = tmp_dir("root-work");
    let gw = start_gateway(&work, None);
    let src = make_flat(&tmp, "src.png", 100, 80, [10, 10, 10], None);
    let r = post(
        &gw,
        "/op/watermark",
        json!({
            "path": src.to_string_lossy(),
            "text": "DAMDesk",
            "out": tmp.join("escape.png").to_string_lossy(),
        }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(
        r.body["error"].as_str().unwrap_or("").contains("out_of"),
        "应报写根越界：{}",
        r.body
    );
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn unreadable_source_is_404() {
    let work = tmp_dir("404-work");
    let gw = start_gateway(&work, None);
    let r = post(
        &gw,
        "/op/watermark",
        json!({
            "path": work.join("nope.png").to_string_lossy(),
            "text": "DAMDesk",
            "out": work.join("z.png").to_string_lossy(),
        }),
    );
    assert_eq!(r.status, 404, "{}", r.body);
    assert_eq!(r.body["error"], "asset_unreadable");
    let _ = std::fs::remove_dir_all(&work);
}
