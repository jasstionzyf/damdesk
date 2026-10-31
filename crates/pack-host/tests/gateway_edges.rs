//! 能力网关的**协议边界**覆盖：HTTP 组帧 / 鉴权顺序 / 参数校验 / 写根边界 /
//! VL 三态 / 错误体与调试输出。这是 K03 网关「容器内可一键跑」的边界层证据。
//!
//! 与 `gateway_endpoints.rs` 的分工：那份验「10 端点主链路 + 白名单」，
//! 这份只验**边界与错误面**（400/404/413/401/405 的每一类来源、越界写、
//! 掩码尺寸、像素上限、路径逃逸），一个用例一件事，失败信息直接指出是哪条边界。
//!
//! 模型根固定为空（`DAM_MODELS_ROOT` 指向空目录）：本文件里凡涉及模型的端点
//! 都必须落 403 `capability_missing`，真模型路径由 `gateway_pack_e2e.rs` 负责。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use pack_host::gateway::{
    capability_of, Gateway, GatewayConfig, GatewayError, GatewayStartError, VlError, VlJudge,
    VlReply, VlRule, ENDPOINTS, MAX_BODY_BYTES, VL_MAX_TOKENS_CAP,
};
use pack_host::Manifest;
use serde_json::{json, Value};

const TOKEN: &str = "edges-token";

fn empty_models_root() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let root =
            std::env::temp_dir().join(format!("pack-host-edges-models-{}", std::process::id()));
        std::fs::create_dir_all(root.join("models")).unwrap();
        std::env::set_var("DAM_MODELS_ROOT", &root);
        root
    })
    .clone()
}

fn all_caps() -> Vec<String> {
    ENDPOINTS.iter().map(|(_, cap)| cap.to_string()).collect()
}

fn workdir(tag: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let work = dir.path().join(format!("work-{tag}"));
    std::fs::create_dir_all(&work).unwrap();
    (dir, work)
}

fn gateway(work: &Path) -> Gateway {
    empty_models_root();
    Gateway::start(GatewayConfig::new(all_caps(), work).with_token(TOKEN)).expect("网关启动失败")
}

fn gateway_with_vl(work: &Path, vl: Arc<dyn VlJudge>) -> Gateway {
    empty_models_root();
    Gateway::start(
        GatewayConfig::new(all_caps(), work)
            .with_token(TOKEN)
            .with_vl(vl),
    )
    .expect("网关启动失败")
}

// ---------------------------------------------------------------------------
// 极简 HTTP 客户端（要能精确控制组帧，所以不用 reqwest）
// ---------------------------------------------------------------------------

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| {
            panic!("应答体不是 JSON（{}）：{:?}", e, self.body);
        })
    }
}

/// 应答头的最小契约：JSON + close + content-length 自洽。
/// （插件侧用 urllib 解析，头不自洽会变成「读不完整」这种极难定位的故障）
fn assert_json_headers(r: &Resp) {
    let get = |name: &str| {
        r.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(
        get("content-type"),
        Some("application/json; charset=utf-8"),
        "应答必须是 JSON（头：{:?}）",
        r.headers
    );
    assert_eq!(
        get("connection"),
        Some("close"),
        "一问一答必须关连接（头：{:?}）",
        r.headers
    );
    let len: usize = get("content-length")
        .unwrap_or_else(|| panic!("缺 content-length：{:?}", r.headers))
        .parse()
        .unwrap_or_else(|e| panic!("content-length 不是数字：{e}"));
    assert_eq!(
        len,
        r.body.len(),
        "content-length 与实际 body 长度不一致（头：{:?}）",
        r.headers
    );
}

fn parse(gw: &Gateway, raw: &[u8]) -> Resp {
    let text = String::from_utf8_lossy(raw).to_string();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("应答头体未分隔：{text:?}"));
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("状态行不合法：{head:?}"));
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let _ = gw;
    Resp {
        status,
        headers,
        body: body.to_string(),
    }
}

/// 发一段原样字节，读回全部应答（connection: close）。
fn send_raw(gw: &Gateway, raw: &str) -> Resp {
    let addr = gw.base_url().replace("http://", "");
    let mut stream = TcpStream::connect(&addr).expect("连网关失败");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    // 服务端可能在客户端写完前就回 413 并关连接 → 写失败不算测试失败，照样读应答
    let _ = stream.write_all(raw.as_bytes());
    let _ = stream.flush();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            // 服务端先 close 再被我们写（413 场景）会回 RST：数据通常已到手，按读到的算
            Err(_) => break,
        }
    }
    parse(gw, &buf)
}

fn request(gw: &Gateway, method: &str, path: &str, auth: Option<&str>, body: Option<&str>) -> Resp {
    let addr = gw.base_url().replace("http://", "");
    let body = body.unwrap_or("");
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nhost: {addr}\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    if let Some(token) = auth {
        head.push_str(&format!("authorization: {token}\r\n"));
    }
    send_raw(gw, &format!("{head}\r\n{body}"))
}

fn post(gw: &Gateway, path: &str, body: &Value) -> Resp {
    request(
        gw,
        "POST",
        path,
        Some(&format!("Bearer {TOKEN}")),
        Some(&body.to_string()),
    )
}

fn post_raw(gw: &Gateway, path: &str, body: &str) -> Resp {
    request(
        gw,
        "POST",
        path,
        Some(&format!("Bearer {TOKEN}")),
        Some(body),
    )
}

fn make_png(path: &Path, w: i32, h: i32) {
    let mut rgb = vec![180u8; (w as usize) * (h as usize) * 3];
    for y in h / 3..2 * h / 3 {
        for x in w / 3..2 * w / 3 {
            let i = ((y * w + x) * 3) as usize;
            rgb[i] = 30;
            rgb[i + 1] = 60;
            rgb[i + 2] = 200;
        }
    }
    core_media::encode_rgb_to(w, h, &rgb, path, None).expect("写测试图失败");
}

// ---------------------------------------------------------------------------
// 1. HTTP 组帧 / 鉴权 / 路由顺序
// ---------------------------------------------------------------------------

#[test]
fn http_auth_and_routing_order() {
    let (tmp, work) = workdir("auth");
    let gw = gateway(&work);
    let img = tmp.path().join("a.png");
    make_png(&img, 120, 80);
    let body = json!({ "path": img.to_string_lossy() });

    // 方法先于鉴权：GET + 错令牌 → 405（不泄露「令牌对不对」）
    let r = request(&gw, "GET", "/io/read", Some("Bearer wrong"), None);
    assert_eq!(r.status, 405, "{}", r.body);
    assert_eq!(r.json()["error"], "method_not_allowed");

    // Bearer 前缀大小写敏感（协议 §6 字面量）：小写 → 401
    let r = request(
        &gw,
        "POST",
        "/io/read",
        Some("bearer edges-token"),
        Some(&body.to_string()),
    );
    assert_eq!(r.status, 401, "{}", r.body);
    assert_json_headers(&r);

    // 前缀之后的空白由宿主 trim（避免客户端多打空格就 401）
    let r = request(
        &gw,
        "POST",
        "/io/read",
        Some("Bearer   edges-token  "),
        Some(&body.to_string()),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_json_headers(&r);

    // 令牌缺失 → 401
    let r = request(&gw, "POST", "/io/read", None, Some(&body.to_string()));
    assert_eq!(r.status, 401);
    assert_eq!(r.json()["error"], "unauthorized");

    // query 被忽略（只按路径路由）
    let r = request(
        &gw,
        "POST",
        "/io/read?trace=1&x=y",
        Some(&format!("Bearer {TOKEN}")),
        Some(&body.to_string()),
    );
    assert_eq!(r.status, 200, "{}", r.body);

    // 带结尾斜杠不是同一个端点（严格字面量路由） → 404
    let r = post(&gw, "/io/read/", &body);
    assert_eq!(r.status, 404, "{}", r.body);
    assert_eq!(r.json()["error"], "no_such_endpoint");

    // 404 应答必须回显路径，便于插件侧定位
    assert!(r.json()["detail"].as_str().unwrap().contains("/io/read/"));
}

// ---------------------------------------------------------------------------
// 2. HTTP 解析错误面（不崩进程、状态码可分辨）
// ---------------------------------------------------------------------------

#[test]
fn http_parse_errors_are_classified() {
    let (tmp, work) = workdir("parse");
    let gw = gateway(&work);
    let img = tmp.path().join("a.png");
    make_png(&img, 60, 40);

    // 请求头没有冒号 → 400
    let r = send_raw(
        &gw,
        "POST /io/read HTTP/1.1\r\nbad-header-line\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    );
    assert_eq!(r.status, 400, "{}", r.body);
    assert_json_headers(&r);
    assert!(r.json()["detail"]
        .as_str()
        .unwrap()
        .contains("请求头不合法"));

    // content-length 不是数字 → 400
    let r = send_raw(
        &gw,
        "POST /io/read HTTP/1.1\r\ncontent-length: 12ab\r\nconnection: close\r\n\r\n",
    );
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.json()["detail"]
        .as_str()
        .unwrap()
        .contains("content-length"));

    // 请求行缺 target → 400
    let r = send_raw(&gw, "POST\r\ncontent-length: 0\r\n\r\n");
    assert_eq!(r.status, 400, "{}", r.body);

    // 请求体短于 content-length（客户端提前关写）→ 400「请求体提前结束」
    let addr = gw.base_url().replace("http://", "");
    let mut stream = TcpStream::connect(&addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    stream
        .write_all(
            b"POST /io/read HTTP/1.1\r\ncontent-length: 100\r\nconnection: close\r\n\r\n{\"path\"",
        )
        .unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();
    let r = parse(&gw, &buf);
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(
        r.json()["detail"]
            .as_str()
            .unwrap()
            .contains("请求体提前结束"),
        "{}",
        r.body
    );

    // 请求头超 16 KiB（还没等到 \r\n\r\n）→ 413
    let mut huge = String::from("POST /io/read HTTP/1.1\r\n");
    while huge.len() < 20 * 1024 {
        huge.push_str("x-pad: 0123456789012345678901234567890123456789\r\n");
    }
    let r = send_raw(&gw, &huge);
    assert_eq!(r.status, 413, "{}", r.body);
    assert_json_headers(&r);
    assert!(r.json()["detail"].as_str().unwrap().contains("16 KiB"));

    // 连上就断（零字节）→ 不写应答、不 panic；下一条请求照常
    let addr = gw.base_url().replace("http://", "");
    let stream = TcpStream::connect(&addr).unwrap();
    drop(stream);
    let r = post(&gw, "/io/read", &json!({ "path": img.to_string_lossy() }));
    assert_eq!(r.status, 200, "空连接不应影响后续请求：{}", r.body);
}

#[test]
fn body_shape_and_size_edges() {
    let (tmp, work) = workdir("body");
    let gw = gateway(&work);
    let img = tmp.path().join("a.png");
    make_png(&img, 60, 40);

    // 空体能被接受为 {}：走到端点后因缺字段而 400（不是 400「不是 JSON」）
    let r = post_raw(&gw, "/io/read", "");
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.json()["detail"].as_str().unwrap().contains("缺字段 path"));

    // 合法 JSON 但不是对象 → 400（统一在 dispatch 拦，端点各自不重复判）
    for raw in ["[1,2,3]", "\"a string\"", "42", "null", "true"] {
        let r = post_raw(&gw, "/io/read", raw);
        assert_eq!(r.status, 400, "{raw} → {}", r.body);
        assert_eq!(r.json()["error"], "invalid_params");
        assert!(
            r.json()["detail"]
                .as_str()
                .unwrap()
                .contains("必须是 JSON 对象"),
            "{raw} → {}",
            r.body
        );
    }

    // 请求体超 1 MiB → 413（先读掉再回，避免客户端看到 RST）
    let big = format!("{{\"path\":\"{}\"}}", "x".repeat(MAX_BODY_BYTES));
    let r = post_raw(&gw, "/io/read", &big);
    assert_eq!(r.status, 413, "{}", r.body);
    assert_eq!(r.json()["error"], "invalid_params");

    // 拒绝之后网关仍然可用
    let r = post(&gw, "/io/read", &json!({ "path": img.to_string_lossy() }));
    assert_eq!(r.status, 200, "{}", r.body);
}

// ---------------------------------------------------------------------------
// 3. 几何算子参数边界（400 一律带可定位 detail）
// ---------------------------------------------------------------------------

#[test]
fn crop_param_edges() {
    let (tmp, work) = workdir("crop");
    let gw = gateway(&work);
    let img = tmp.path().join("src.png");
    make_png(&img, 400, 300);
    let p = img.to_string_lossy().to_string();
    let out = work.join("o.png").to_string_lossy().to_string();

    let cases: Vec<(Value, &str)> = vec![
        (json!({ "path": p, "out": out }), "缺字段 box"),
        (
            json!({ "path": p, "box": [0, 0, 10], "out": out }),
            "长度为 4",
        ),
        (
            json!({ "path": p, "box": [0, 0, 10.5, 10], "out": out }),
            "必须是整数",
        ),
        (
            json!({ "path": p, "box": [10, 10, 10, 20], "out": out }),
            "退化",
        ),
        (
            json!({ "path": p, "box": [0, 0, 500, 100], "out": out }),
            "越界",
        ),
        (
            json!({ "path": p, "box": [-1, 0, 10, 10], "out": out }),
            "越界",
        ),
        (json!({ "path": p, "box": [0, 0, 10, 10] }), "缺字段 out"),
    ];
    for (body, needle) in cases {
        let r = post(&gw, "/op/crop", &body);
        assert_eq!(r.status, 400, "{body} → {}", r.body);
        assert_eq!(r.json()["error"], "invalid_params");
        assert!(
            r.json()["detail"].as_str().unwrap().contains(needle),
            "{body} → {}",
            r.body
        );
    }

    // 边界内的最小值合法：1×1 裁剪
    let one = work.join("one.png");
    let r = post(
        &gw,
        "/op/crop",
        &json!({ "path": img.to_string_lossy(), "box": [399, 299, 400, 300],
                 "out": one.to_string_lossy() }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(core_media::image_size(&one).unwrap(), (1, 1));
}

#[test]
fn letterbox_param_edges() {
    let (tmp, work) = workdir("lb");
    let gw = gateway(&work);
    let img = tmp.path().join("src.png");
    make_png(&img, 300, 200);
    let p = img.to_string_lossy().to_string();
    let out = work.join("o.png").to_string_lossy().to_string();

    for (body, needle) in [
        (json!({ "path": p, "out": out }), "缺字段 target_ratio"),
        (
            json!({ "path": p, "target_ratio": 0.01, "out": out }),
            "越界",
        ),
        (
            json!({ "path": p, "target_ratio": 25.0, "out": out }),
            "越界",
        ),
        (
            json!({ "path": p, "target_ratio": 1.0, "background": "zzz", "out": out }),
            "不合法",
        ),
        (
            json!({ "path": p, "target_ratio": "1.0", "out": out }),
            "必须是数字",
        ),
    ] {
        let r = post(&gw, "/op/letterbox", &body);
        assert_eq!(r.status, 400, "{body} → {}", r.body);
        assert!(
            r.json()["detail"].as_str().unwrap().contains(needle),
            "{body} → {}",
            r.body
        );
    }

    // 背景色按解析后的 RGB 真的写到补边上（白图补黑边 → 角像素必须是黑的）
    let black = work.join("lb-black.png");
    let r = post(
        &gw,
        "/op/letterbox",
        &json!({ "path": img.to_string_lossy(), "target_ratio": 1.0,
                 "background": "#000000", "out": black.to_string_lossy() }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    let (w, h, px) = core_media::rgb8(&black).unwrap();
    assert_eq!((w, h), (300, 300));
    assert_eq!(&px[0..3], &[0u8, 0, 0], "左上角补边必须是背景色");
    // 原图中线仍是图像内容（补边不覆盖、不缩放）
    let mid = ((150 * w + 150) * 3) as usize;
    assert_ne!(&px[mid..mid + 3], &[0u8, 0, 0], "中线必须还是原图内容");

    // 像素上限：20000×10 @ratio=0.05 → 画布 20000×400000 > 1e8 → 400
    let wide = tmp.path().join("wide.png");
    make_png(&wide, 20000, 10);
    let r = post(
        &gw,
        "/op/letterbox",
        &json!({ "path": wide.to_string_lossy(), "target_ratio": 0.05,
                 "out": work.join("huge.png").to_string_lossy() }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(
        r.json()["detail"].as_str().unwrap().contains("像素上限"),
        "{}",
        r.body
    );
}

#[test]
fn upscale_param_edges() {
    let (tmp, work) = workdir("up");
    let gw = gateway(&work);
    let img = tmp.path().join("src.png");
    make_png(&img, 200, 100);
    let p = img.to_string_lossy().to_string();
    let out = work.join("o.png").to_string_lossy().to_string();
    // 长边超 20000 的用例需要一张长图（4000×100 @8 → 32000）
    let long_img = tmp.path().join("long.png");
    make_png(&long_img, 4000, 100);
    let long_p = long_img.to_string_lossy().to_string();

    for (body, needle) in [
        (json!({ "path": p, "out": out }), "缺字段 scale"),
        (json!({ "path": p, "scale": 0.05, "out": out }), "越界"),
        (json!({ "path": p, "scale": 65.0, "out": out }), "越界"),
        (
            json!({ "path": long_p, "scale": 8.0,
                    "out": work.join("cap.png").to_string_lossy() }),
            "超上限",
        ),
    ] {
        let r = post(&gw, "/op/upscale", &body);
        assert_eq!(r.status, 400, "{body} → {}", r.body);
        assert!(
            r.json()["detail"].as_str().unwrap().contains(needle),
            "{body} → {}",
            r.body
        );
    }

    // 9× 不再算「越界」：宿主内部分趟（K07 D7 修的就是这条——旧实现 8× 硬门槛
    // 让小图的几何修复（f005：scale≈18.6）连放大都走不到，插件只收到裸 -32003）
    let nine = work.join("nine.png");
    let r = post(
        &gw,
        "/op/upscale",
        &json!({ "path": img.to_string_lossy(), "scale": 9.0,
                 "out": nine.to_string_lossy() }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["op"], "upscale.staged");
    assert_eq!(core_media::image_size(&nine).unwrap(), (1800, 900));

    // 边界内：scale=0.5 允许（缩小），尺寸按 long side 取整
    let small = work.join("small.png");
    let r = post(
        &gw,
        "/op/upscale",
        &json!({ "path": img.to_string_lossy(), "scale": 0.5,
                 "out": small.to_string_lossy() }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["op"], "upscale.resample");
    assert!(core_media::image_size(&small).is_ok());
}

// ---------------------------------------------------------------------------
// 4. 写根边界（out 只能落在写根之下）
// ---------------------------------------------------------------------------

#[test]
fn out_path_boundaries() {
    let (tmp, work) = workdir("out");
    let gw = gateway(&work);
    let img = tmp.path().join("src.png");
    make_png(&img, 100, 100);
    let box_ = json!([0, 0, 10, 10]);

    // 空 / 相对 / 无文件名 / 无父目录
    for (out, needle) in [
        ("", "缺字段 out"),
        ("rel.png", "必须是绝对路径"),
        ("/tmp/..", "没有文件名"),
    ] {
        let r = post(
            &gw,
            "/op/crop",
            &json!({ "path": img.to_string_lossy(), "box": box_, "out": out }),
        );
        assert_eq!(r.status, 400, "out={out:?} → {}", r.body);
        assert!(
            r.json()["detail"].as_str().unwrap().contains(needle),
            "out={out:?} → {}",
            r.body
        );
    }
    let r = post(
        &gw,
        "/op/crop",
        &json!({ "path": img.to_string_lossy(), "box": box_, "out": "/" }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.json()["detail"].as_str().unwrap().contains("没有父目录"));

    // 父路径是文件 → 建目录失败 → 500 internal（不是 400 参数错：这是宿主侧 IO）
    let blocker = tmp.path().join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    let r = post(
        &gw,
        "/op/crop",
        &json!({ "path": img.to_string_lossy(), "box": box_,
                 "out": blocker.join("o.png").to_string_lossy() }),
    );
    assert_eq!(r.status, 500, "{}", r.body);
    assert_eq!(r.json()["error"], "internal");

    // 符号链接逃逸：写根里的 link 指向外部目录 → 规范化后越界 → 400 out_of_workroot
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, work.join("link")).ok();
    let via_link = work.join("link").join("escape.png");
    let r = post(
        &gw,
        "/op/crop",
        &json!({ "path": img.to_string_lossy(), "box": box_,
                 "out": via_link.to_string_lossy() }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    assert_eq!(r.json()["error"], "out_of_workroot");
    assert!(!outside.join("escape.png").exists(), "越界请求不得写出产物");

    // 未登记 / 不存在的额外写根不会被静默接受（canonicalize 失败 → 不加入）
    gw.allow_write_root(tmp.path().join("never-created"));
    let r = post(
        &gw,
        "/op/crop",
        &json!({ "path": img.to_string_lossy(), "box": box_,
                 "out": tmp.path().join("never-created/o.png").to_string_lossy() }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    assert_eq!(r.json()["error"], "out_of_workroot");

    // 说明（K03 反馈项，见报告 FINDING-2）：越界判定发生在 `create_dir_all(parent)` 之后，
    // 所以「越界路径的父目录」已经被建出来了。这里只钉住「不落产物」这条硬约束，
    // 不把当前的建目录副作用写成期望行为。
    let probe = tmp.path().join("probe-dir/deeper");
    let r = post(
        &gw,
        "/op/crop",
        &json!({ "path": img.to_string_lossy(), "box": box_,
                 "out": probe.join("o.png").to_string_lossy() }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(!probe.join("o.png").exists());
}

// ---------------------------------------------------------------------------
// 5. /io/read 的文件与 MIME 边界
// ---------------------------------------------------------------------------

#[test]
fn io_read_file_and_mime_edges() {
    let (tmp, work) = workdir("io");
    let gw = gateway(&work);
    let src = tmp.path().join("src.png");
    make_png(&src, 64, 48);
    let bytes = std::fs::read(&src).unwrap();

    // 扩展名 → mime 映射（协议 §6 表格外的扩展名一律 octet-stream）
    for (name, mime) in [
        ("a.png", "image/png"),
        ("a.jpg", "image/jpeg"),
        ("a.JPEG", "image/jpeg"),
        ("a.webp", "image/webp"),
        ("a.gif", "image/gif"),
        ("a.bmp", "image/bmp"),
        ("a.tif", "image/tiff"),
        ("a.tiff", "image/tiff"),
        ("a.heic", "image/heic"),
        ("a.unknown", "application/octet-stream"),
        ("noext", "application/octet-stream"),
    ] {
        let path = tmp.path().join(name);
        std::fs::write(&path, &bytes).unwrap();
        let r = post(&gw, "/io/read", &json!({ "path": path.to_string_lossy() }));
        assert_eq!(r.status, 200, "{name} → {}", r.body);
        assert_eq!(r.json()["mime"], mime, "{name}");
        assert_eq!(r.json()["width"], 64);
        assert_eq!(r.json()["height"], 48);
    }

    // 目录不是可读文件 → 404（不是 panic，也不是 200）
    let r = post(
        &gw,
        "/io/read",
        &json!({ "path": tmp.path().to_string_lossy() }),
    );
    assert_eq!(r.status, 404, "{}", r.body);
    assert_eq!(r.json()["error"], "asset_unreadable");

    // 不是图片 → 解码失败也是 404 asset_unreadable（可读但不可解码）
    let junk = tmp.path().join("junk.png");
    std::fs::write(&junk, b"not an image at all").unwrap();
    let r = post(&gw, "/io/read", &json!({ "path": junk.to_string_lossy() }));
    assert_eq!(r.status, 404, "{}", r.body);
    assert_eq!(r.json()["error"], "asset_unreadable");

    // 空串路径 → 400（req_str 拦空串，不是 404）
    let r = post(&gw, "/io/read", &json!({ "path": "   " }));
    assert_eq!(r.status, 400, "{}", r.body);
    // 相对路径 → 400「必须是绝对路径」（相对路径按插件 cwd 解析会让越权变得不可控）
    let r = post(&gw, "/io/read", &json!({ "path": "assets/a.png" }));
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(
        r.json()["detail"].as_str().unwrap().contains("绝对路径"),
        "{}",
        r.body
    );
}

#[test]
fn extra_write_root_must_exist_and_then_allows_writes() {
    let (tmp, work) = workdir("extra");
    let gw = gateway(&work);
    let img = tmp.path().join("src.png");
    make_png(&img, 80, 80);

    // 已存在的额外写根：登记后可在其中落产物（L3 派发 job 前的常规动作）
    let job_root = tmp.path().join("job-root");
    std::fs::create_dir_all(&job_root).unwrap();
    gw.allow_write_root(&job_root);
    let out = job_root.join("sub/o.png");
    let r = post(
        &gw,
        "/op/crop",
        &json!({ "path": img.to_string_lossy(), "box": [0, 0, 20, 20],
                 "out": out.to_string_lossy() }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(out.is_file(), "额外写根之下的产物必须落盘：{out:?}");

    // 重复登记幂等（不得让写根列表无限膨胀）
    gw.allow_write_root(&job_root);
    let r = post(
        &gw,
        "/op/crop",
        &json!({ "path": img.to_string_lossy(), "box": [0, 0, 20, 20],
                 "out": job_root.join("sub/o2.png").to_string_lossy() }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
}

#[test]
fn inpaint_mask_param_edges() {
    let (tmp, work) = workdir("inpaint");
    let gw = gateway(&work);
    let img = tmp.path().join("src.png");
    make_png(&img, 200, 200);
    let p = img.to_string_lossy().to_string();
    let out = work.join("o.png").to_string_lossy().to_string();

    // 缺 mask / mask 不存在
    for (body, status, needle) in [
        (json!({ "path": p, "out": out }), 400, "缺字段 mask"),
        (
            json!({ "path": p, "mask": tmp.path().join("nope.png").to_string_lossy(), "out": out }),
            404,
            "不是可读文件",
        ),
    ] {
        let r = post(&gw, "/op/inpaint", &body);
        assert_eq!(r.status, status, "{body} → {}", r.body);
        assert!(
            r.json()["detail"].as_str().unwrap().contains(needle),
            "{body} → {}",
            r.body
        );
    }

    // mask 尺寸与原图不符 → 400（尺寸校验先于模型加载：这张用例不依赖模型在盘）
    let bad_mask = tmp.path().join("bad_mask.png");
    make_png(&bad_mask, 100, 100);
    let r = post(
        &gw,
        "/op/inpaint",
        &json!({ "path": p, "mask": bad_mask.to_string_lossy(), "out": out }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    assert_eq!(r.json()["error"], "invalid_params");
    assert!(
        r.json()["detail"].as_str().unwrap().contains("掩码"),
        "{}",
        r.body
    );

    // mask 尺寸正确但本机没模型 → 403 capability_missing（绝不静默降级成「原图」）
    let good_mask = tmp.path().join("good_mask.png");
    make_png(&good_mask, 200, 200);
    let r = post(
        &gw,
        "/op/inpaint",
        &json!({ "path": p, "mask": good_mask.to_string_lossy(), "out": out }),
    );
    assert_eq!(r.status, 403, "{}", r.body);
    assert_eq!(r.json()["error"], "capability_missing");
    assert_eq!(r.json()["capability"], "op.generative");
    assert!(!Path::new(&out).exists(), "403 不得留下产物");
}

// ---------------------------------------------------------------------------
// 6. /vl/judge：参数 + 三态映射（后端注入桩）
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct RecordingVl {
    seen: Mutex<Vec<(String, u32)>>,
    mode: Mutex<&'static str>,
    flips: Mutex<u32>,
}

impl VlJudge for RecordingVl {
    fn judge(&self, _path: &Path, prompt: &str, max_tokens: u32) -> Result<VlReply, VlError> {
        self.seen
            .lock()
            .unwrap()
            .push((prompt.to_string(), max_tokens));
        let mode = *self.mode.lock().unwrap();
        if mode == "flip" {
            // K07-FIX-H：逐次翻转，用来证明 samples 拿的是**新样本**而不是同一份复制
            let mut flips = self.flips.lock().unwrap();
            *flips += 1;
            let status = if *flips % 2 == 1 { "pass" } else { "fail" };
            return Ok(VlReply::decided(
                format!("{{\"status\":\"{status}\"}}"),
                vec![VlRule {
                    status: status.into(),
                    label: "apparel".into(),
                    score: Some(0.6),
                }],
            ));
        }
        match mode {
            "badparams" => Err(VlError::BadParams("prompt 超长".into())),
            "silent" => Ok(VlReply {
                undetermined: true,
                raw: String::new(),
                reason: None,
                rules: Vec::new(),
            }),
            "norules" => Ok(VlReply::decided("{\"ok\":true}", Vec::new())),
            _ => Ok(VlReply::decided(
                "{\"status\":\"pass\"}",
                vec![VlRule {
                    status: "pass".into(),
                    label: "apparel".into(),
                    score: Some(0.9),
                }],
            )),
        }
    }
}

#[test]
fn vl_judge_param_and_reply_edges() {
    let (tmp, work) = workdir("vl");
    let img = tmp.path().join("asset.png");
    make_png(&img, 128, 128);
    let stub = Arc::new(RecordingVl::default());
    let gw = gateway_with_vl(&work, stub.clone());
    let p = img.to_string_lossy().to_string();

    // prompt 只有空白 → 400
    for body in [
        json!({ "path": p, "prompt": "   " }),
        json!({ "path": p }),
        json!({ "path": p, "prompt": "p", "max_tokens": "512" }),
        json!({ "path": p, "prompt": "p", "max_tokens": -1 }),
        json!({ "path": p, "prompt": "p", "max_tokens": 1.5 }),
    ] {
        let r = post(&gw, "/vl/judge", &body);
        assert_eq!(r.status, 400, "{body} → {}", r.body);
        assert_eq!(r.json()["error"], "invalid_params");
    }

    // max_tokens: null 等同于缺省 → 上限
    let r = post(
        &gw,
        "/vl/judge",
        &json!({ "path": p, "prompt": "p", "max_tokens": null }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        stub.seen.lock().unwrap().last().unwrap().1,
        VL_MAX_TOKENS_CAP
    );

    // 边界：恰好等于上限可用
    let r = post(
        &gw,
        "/vl/judge",
        &json!({ "path": p, "prompt": "p", "max_tokens": VL_MAX_TOKENS_CAP }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        stub.seen.lock().unwrap().last().unwrap().1,
        VL_MAX_TOKENS_CAP
    );

    // 后端说参数错 → 400（不是 undetermined：这是调用方的问题）
    *stub.mode.lock().unwrap() = "badparams";
    let r = post(&gw, "/vl/judge", &json!({ "path": p, "prompt": "p" }));
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.json()["detail"].as_str().unwrap().contains("prompt 超长"));

    // 后端说不可判但没给 reason → 仍 undetermined，且不带 reason 键（不臆造原因）
    *stub.mode.lock().unwrap() = "silent";
    let r = post(&gw, "/vl/judge", &json!({ "path": p, "prompt": "p" }));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["undetermined"], true);
    assert!(r.json().get("reason").is_none(), "{}", r.body);
    assert!(r.json().get("rules").is_none());

    // 判定成功但后端没给逐条 rules → 应答不带 rules 键（插件自行按 ruleId 映射）
    *stub.mode.lock().unwrap() = "norules";
    let r = post(&gw, "/vl/judge", &json!({ "path": p, "prompt": "p" }));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["undetermined"], false);
    assert_eq!(r.json()["raw"], "{\"ok\":true}");
    assert!(r.json().get("rules").is_none(), "{}", r.body);
}

/// K07-FIX-H 翻转保护：`samples` 是**协议 1.2 增量**，缺省 1（1.1 应答逐字不变）。
#[test]
fn vl_judge_samples_edges() {
    let (tmp, work) = workdir("vl-samples");
    let img = tmp.path().join("asset.png");
    make_png(&img, 128, 128);
    let stub = Arc::new(RecordingVl::default());
    let gw = gateway_with_vl(&work, stub.clone());
    let p = img.to_string_lossy().to_string();

    // 显式 samples=1 = 与 1.1 完全同形（不带 raws 键）
    let r = post(
        &gw,
        "/vl/judge",
        &json!({ "path": p, "prompt": "p", "samples": 1 }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["undetermined"], false);
    assert!(r.json().get("raws").is_none(), "{}", r.body);

    // 非法值：0 / 非整数 / 超上限 3
    for body in [
        json!({ "path": p, "prompt": "p", "samples": 0 }),
        json!({ "path": p, "prompt": "p", "samples": "3" }),
        json!({ "path": p, "prompt": "p", "samples": 1.5 }),
        json!({ "path": p, "prompt": "p", "samples": 4 }),
    ] {
        let r = post(&gw, "/vl/judge", &body);
        assert_eq!(r.status, 400, "{body} → {}", r.body);
        assert_eq!(r.json()["error"], "invalid_params");
    }

    // samples=3：后端被问 3 次，raws 三份，顶层 raw 仍是第一份（1.1 兼容）
    stub.seen.lock().unwrap().clear();
    let r = post(
        &gw,
        "/vl/judge",
        &json!({ "path": p, "prompt": "p", "samples": 3 }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        stub.seen.lock().unwrap().len(),
        3,
        "samples 必须真的问 N 次"
    );
    assert_eq!(r.json()["samples"], 3);
    let raws = r.json()["raws"].as_array().expect("raws 数组").clone();
    assert_eq!(raws.len(), 3);
    assert_eq!(raws[0]["raw"], r.json()["raw"]);

    // 后端逐次翻转 → raws 内容不同（证明不是把同一份复制三遍）
    *stub.mode.lock().unwrap() = "flip";
    *stub.flips.lock().unwrap() = 0;
    let r = post(
        &gw,
        "/vl/judge",
        &json!({ "path": p, "prompt": "p", "samples": 3 }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    let statuses: Vec<String> = r.json()["raws"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            item["rules"][0]["status"]
                .as_str()
                .unwrap_or("")
                .to_string()
        })
        .collect();
    assert_eq!(statuses, vec!["pass", "fail", "pass"], "{}", r.body);

    // 采样整体不可用 → undetermined（不冒充 pass/fail），raws 为空
    *stub.mode.lock().unwrap() = "silent";
    let r = post(
        &gw,
        "/vl/judge",
        &json!({ "path": p, "prompt": "p", "samples": 3 }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["undetermined"], true);
    assert_eq!(r.json()["raws"].as_array().unwrap().len(), 3);

    // 参数错（后端说 prompt 不合法）→ 400
    *stub.mode.lock().unwrap() = "badparams";
    let r = post(
        &gw,
        "/vl/judge",
        &json!({ "path": p, "prompt": "p", "samples": 2 }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
}

// ---------------------------------------------------------------------------
// 7. /report/emit 归账
// ---------------------------------------------------------------------------
#[test]
fn report_emit_accounting_edges() {
    let (_tmp, work) = workdir("emit");
    let gw = gateway(&work);

    for (body, needle) in [
        (json!({ "entry": {} }), "缺字段 job_id"),
        (json!({ "job_id": "  ", "entry": {} }), "缺字段 job_id"),
        (json!({ "job_id": "j" }), "缺字段 entry"),
        (
            json!({ "job_id": "j", "entry": "not-an-object" }),
            "JSON 对象",
        ),
    ] {
        let r = post(&gw, "/report/emit", &body);
        assert_eq!(r.status, 400, "{body} → {}", r.body);
        assert!(
            r.json()["detail"].as_str().unwrap().contains(needle),
            "{body} → {}",
            r.body
        );
    }

    // 空对象 entry 是合法的（插件可以先报空壳再补）—— 与上面「entry 缺失」区分开
    let r = post(&gw, "/report/emit", &json!({ "job_id": "j1", "entry": {} }));
    assert_eq!(r.status, 200, "{}", r.body);

    // emits() 返回副本：外部改写不得污染账本
    let mut got = gw.emits("j1");
    got.push(json!({ "injected": true }));
    assert_eq!(gw.emits("j1").len(), 1, "账本被外部改写污染了");
    assert_eq!(gw.emit_jobs(), vec!["j1".to_string()]);
}

// ---------------------------------------------------------------------------
// 8. 纯函数 / 错误体 / Debug 脱敏
// ---------------------------------------------------------------------------

#[test]
fn capability_mapping_and_error_bodies() {
    // 端点 → 能力：13 条全覆盖，未知路径返回 None
    assert_eq!(capability_of("/io/read"), Some("io.read"));
    assert_eq!(capability_of("/detect/bbox"), Some("detect.u2net"));
    assert_eq!(capability_of("/detect/ocr"), Some("detect.ocr"));
    assert_eq!(capability_of("/detect/white_stats"), Some("detect.white"));
    assert_eq!(capability_of("/op/watermark"), Some("op.overlay"));
    assert_eq!(capability_of("/op/inpaint"), Some("op.generative"));
    assert_eq!(capability_of("/report/emit"), Some("report.emit"));
    assert_eq!(capability_of("/io/read/"), None);
    assert_eq!(capability_of("io/read"), None);
    assert_eq!(capability_of(""), None);
    // 白名单不重复（重复会让「声明一次即全开」的语义失控）
    let mut caps: Vec<&str> = ENDPOINTS.iter().map(|(_, c)| *c).collect();
    caps.sort_unstable();
    caps.dedup();
    assert_eq!(caps.len(), 9, "13 端点应只映射到 9 项能力：{caps:?}");

    // 错误体：capability 只在 403 出现
    let invalid = GatewayError::invalid("坏参数");
    assert_eq!(invalid.status, 400);
    assert_eq!(invalid.body()["error"], "invalid_params");
    assert!(invalid.body().get("capability").is_none());

    let unreadable = GatewayError::unreadable("读不到");
    assert_eq!(unreadable.status, 404);
    assert_eq!(unreadable.body()["error"], "asset_unreadable");

    let missing = GatewayError::capability_missing("vl.judge", "没装");
    assert_eq!(missing.status, 403);
    assert_eq!(missing.body()["capability"], "vl.judge");

    let out = GatewayError::out_of_root("越界");
    assert_eq!(out.status, 400);
    assert_eq!(out.body()["error"], "out_of_workroot");

    let internal = GatewayError::internal("磁盘满了");
    assert_eq!(internal.status, 500);
    assert_eq!(internal.body()["error"], "internal");
}

#[test]
fn debug_output_redacts_secrets() {
    let cfg = GatewayConfig::new(all_caps(), "/tmp/work-root").with_token("super-secret-token");
    let text = format!("{cfg:?}");
    assert!(text.contains("<redacted>"), "{text}");
    assert!(
        !text.contains("super-secret-token"),
        "配置调试输出泄露令牌：{text}"
    );
    assert!(
        text.contains("vl: None"),
        "未注入 VL 时调试输出应显式写 None：{text}"
    );

    let (_tmp, work) = workdir("debug");
    let gw = gateway(&work);
    let text = format!("{gw:?}");
    assert!(text.contains("<redacted>"), "{text}");
    assert!(!text.contains(TOKEN), "网关调试输出泄露令牌：{text}");
}

// ---------------------------------------------------------------------------
// 9. 启动/停服错误面
// ---------------------------------------------------------------------------

#[test]
fn start_and_stop_errors() {
    empty_models_root();

    // 端口被占 → Bind（错误里带地址）
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap();
    let mut cfg = GatewayConfig::new(all_caps(), "/tmp");
    cfg.bind = addr;
    match Gateway::start(cfg) {
        Err(GatewayStartError::Bind { bind, .. }) => assert_eq!(bind, addr),
        other => panic!("期望 Bind 错误，实得 {other:?}"),
    }

    // 写根不存在 → WorkDir（不静默创建：写根是安全边界，必须显式存在）
    let dir = tempfile::tempdir().unwrap();
    let ghost = dir.path().join("not-there");
    let mut cfg = GatewayConfig::new(all_caps(), &ghost);
    cfg.token = Some(TOKEN.to_string());
    match Gateway::start(cfg) {
        Err(GatewayStartError::WorkDir { path, .. }) => assert_eq!(path, ghost),
        other => panic!("期望 WorkDir 错误，实得 {other:?}"),
    }

    // 停服幂等；停后端口不再接受连接
    let (_tmp, work) = workdir("stop");
    let gw = gateway(&work);
    let addr = gw.addr();
    gw.stop();
    gw.stop();
    assert!(
        TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_err(),
        "stop() 后端口仍在监听"
    );
}

// ---------------------------------------------------------------------------
// 10. 从 manifest 装配（L3 主入口）与能力子集授权
// ---------------------------------------------------------------------------

#[test]
fn from_manifest_limits_capabilities_to_declared_subset() {
    empty_models_root();
    let (_tmp, work) = workdir("subset");
    let manifest: Manifest = serde_json::from_value(json!({
        "id": "test.subset",
        "name": "子集授权",
        "version": "1.0.0",
        "api_version": "1.1",
        "entrypoint": { "cmd": "python3", "args": ["plugin/__init__.py"] },
        "capabilities": ["io.read", "op.geometric"],
        "deterministic": true,
        "timeout_secs": 10
    }))
    .unwrap();
    let gw =
        Gateway::start(GatewayConfig::from_manifest(&manifest, &work).with_token(TOKEN)).unwrap();
    let img = work.join("src.png");
    make_png(&img, 80, 60);

    // 声明过的能力放行
    let r = post(&gw, "/io/read", &json!({ "path": img.to_string_lossy() }));
    assert_eq!(r.status, 200, "{}", r.body);
    // 未声明的能力一律 403，并点名缺哪项（不静默降级）
    for (path, cap) in [
        ("/detect/u2net", "detect.u2net"),
        ("/detect/ocr", "detect.ocr"),
        ("/vl/judge", "vl.judge"),
        ("/report/emit", "report.emit"),
        ("/op/inpaint", "op.generative"),
    ] {
        let r = post(
            &gw,
            path,
            &json!({ "path": img.to_string_lossy(), "prompt": "p" }),
        );
        assert_eq!(r.status, 403, "{path} → {}", r.body);
        assert_eq!(r.json()["error"], "capability_missing", "{path}");
        assert_eq!(r.json()["capability"], cap, "{path}");
        assert!(
            r.json()["detail"].as_str().unwrap().contains("未声明"),
            "{path} → {}",
            r.body
        );
    }
}
