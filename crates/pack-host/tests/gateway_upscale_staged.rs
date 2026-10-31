//! `/op/upscale` 分趟放大 + 网关错误上下文（K07 修复卡 `task`）。
//!
//! 修的是 K07 等价门禁暴露的两个实现缺陷（`reports/task-K07.md` §5.2 / D7）：
//!
//! 1. **小图 R1 修复直接失败**：`f005`（554×500）几何闭式解给出 `scale≈18.6`，
//!    旧网关把 `scale ≤ 8` 当参数硬门槛回 400 → 插件修复链连放大都走不到。
//!    现在 `scale` 是**总倍数**，宿主内部按趟执行（每趟长边 ×≤8，末趟精确落目标），
//!    只有画布硬门槛（像素 ≤1e8、长边 ≤20000）才拒绝，且拒绝时给出可用上限。
//! 2. **裸 `-32003`**：旧错误帧只有 `{"error":"invalid_params"}`，插件侧只能上报
//!    「参数非法（400）invalid_params」，看不出哪个 op、什么参数、超了哪个门槛。
//!    现在错误帧带 `op` + `params`，插件（及 SDK）把它们拼进错误文案。
//!
//! 一份文件两类断言：**可达性**（该成功的必须成功、尺寸逐值对得上）与
//! **可定位性**（该失败的必须说清为什么，且带实际参数）。模型根固定为空，
//! 所以 `scale==4` 也只走重采样——分趟逻辑本身与模型无关。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use pack_host::gateway::{upscale_limits, Gateway, GatewayConfig, ENDPOINTS, MAX_OUTPUT_PIXELS};
use serde_json::{json, Value};

const TOKEN: &str = "upscale-token";

fn empty_models_root() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let root =
            std::env::temp_dir().join(format!("pack-host-upscale-models-{}", std::process::id()));
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

struct Resp {
    status: u16,
    body: String,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("应答体不是 JSON（{e}）：{:?}", self.body))
    }
    fn detail(&self) -> String {
        self.json()["detail"].as_str().unwrap_or("").to_string()
    }
}

fn post(gw: &Gateway, path: &str, body: &Value) -> Resp {
    let payload = body.to_string();
    for attempt in 0..3 {
        match try_post(gw, path, &payload) {
            Ok(r) => return r,
            Err(e) if attempt < 2 => {
                // 大图（f005 复现级）写盘偶发慢，重连一次足够；真失败仍要冒出去
                eprintln!("post 重试 {attempt}（{path}）：{e}");
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => panic!("POST {path} 失败：{e}"),
        }
    }
    unreachable!()
}

fn try_post(gw: &Gateway, path: &str, payload: &str) -> std::io::Result<Resp> {
    let addr = gw.addr();
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(120)))?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TOKEN}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream.write_all(req.as_bytes())?;
    stream.flush()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("应答行异常：{head:?}"));
    Ok(Resp {
        status,
        body: body.to_string(),
    })
}

/// 与 `gateway_edges::make_png` 同款：中间一块深色「主体」，其余浅底。
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
// 1. f005 复现：554×500 小图 + 小主体 → scale≈18.6 必须可达
// ---------------------------------------------------------------------------

#[test]
fn f005_small_asset_upscale_is_reachable() {
    let (tmp, work) = workdir("f005");
    let gw = gateway(&work);
    let img = tmp.path().join("f005.png");
    // f005 真实尺寸（K07 报告 §5.2）；主体取小方块，几何解才会给出十几倍
    make_png(&img, 554, 500);
    let out = work.join("f005-3upscale.png");

    let r = post(
        &gw,
        "/op/upscale",
        &json!({ "path": img.to_string_lossy(), "scale": 18.6,
                 "out": out.to_string_lossy() }),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    let body = r.json();

    // 尺寸逐值对得上：长边 = round(554×18.6)=10304，短边按比例 9300
    assert_eq!(body["width"], 10304, "{}", r.body);
    assert_eq!(body["height"], 9300, "{}", r.body);
    assert_eq!(core_media::image_size(&out).unwrap(), (10304, 9300));
    // 短边必须越过 R1 门槛（1600）——这就是 f005 修复要达成的事
    assert!(body["height"].as_i64().unwrap() >= 1600);
    // 多趟：554 →(×8) 4432 → 10304
    assert_eq!(body["op"], "upscale.staged");
    assert_eq!(body["passes"], 2, "{}", r.body);
    assert_eq!(body["steps"].as_array().unwrap().len(), 2);
    assert_eq!(body["steps"][0]["mode"], "resample");
    assert!(body["scale_max"].as_f64().unwrap() >= 18.6, "{}", r.body);

    // 中间产物不许留在工作目录（半成品会把人工核对带偏）
    let leftovers: Vec<String> = std::fs::read_dir(&work)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains(".pass"))
        .collect();
    assert!(leftovers.is_empty(), "残留中间产物：{leftovers:?}");
}

// ---------------------------------------------------------------------------
// 2. 硬门槛：超上限时错误必须给「实际值 + 可用上限 + 建议参数」
// ---------------------------------------------------------------------------

#[test]
fn beyond_hard_cap_reports_usable_limits() {
    let (tmp, work) = workdir("cap");
    let gw = gateway(&work);
    let long_img = tmp.path().join("long-4000x100.png");
    make_png(&long_img, 4000, 100);
    let out = work.join("cap.png");

    let r = post(
        &gw,
        "/op/upscale",
        &json!({ "path": long_img.to_string_lossy(), "scale": 8.0,
                 "out": out.to_string_lossy() }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    let body = r.json();
    assert_eq!(body["error"], "invalid_params");
    let detail = r.detail();
    // 实际值与硬门槛都要在文案里
    assert!(detail.contains("32000"), "detail 缺实际目标：{detail}");
    assert!(detail.contains("超上限"), "detail 缺门槛判定：{detail}");
    assert!(detail.contains("可用上限"), "detail 缺可用上限：{detail}");
    // 长边 4000 的图：像素先到顶（1e8/4e5 = 250 → √=15.8）
    let limits = upscale_limits(4000, 100);
    assert!(
        detail.contains(&format!("scale<={:.3}", limits.max_scale)),
        "detail 缺 scale 上限 {}：{detail}",
        limits.max_scale
    );
    assert!(limits.max_scale < 8.0, "该用例本该撞上限，实际 {:?}", limits);
    // 可用上限填回去一定过：这条门槛是「可达的」，不是拍脑袋的数
    let ok = post(
        &gw,
        "/op/upscale",
        &json!({ "path": long_img.to_string_lossy(), "scale": limits.max_scale,
                 "out": work.join("within.png").to_string_lossy() }),
    );
    assert_eq!(ok.status, 200, "填回上限仍失败：{}", ok.body);
    assert!(!out.exists(), "越界请求不许落盘");
}

#[test]
fn pixel_budget_never_exceeded() {
    // 1000×1000 的图，像素上限决定 scale ≤ 10（1e8/1e6 = 100 → √=10）
    let limits = upscale_limits(1000, 1000);
    assert!((limits.max_scale - 10.0).abs() < 1e-9, "{limits:?}");
    let (tmp, work) = workdir("px");
    let gw = gateway(&work);
    let img = tmp.path().join("k1000.png");
    make_png(&img, 1000, 1000);
    let r = post(
        &gw,
        "/op/upscale",
        &json!({ "path": img.to_string_lossy(), "scale": 10.5,
                 "out": work.join("over.png").to_string_lossy() }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.detail().contains("超上限"), "{}", r.body);
    // 恰好 10× 通过，且像素不超 1e8
    let ok = post(
        &gw,
        "/op/upscale",
        &json!({ "path": img.to_string_lossy(), "scale": 10.0,
                 "out": work.join("k10000.png").to_string_lossy() }),
    );
    assert_eq!(ok.status, 200, "{}", ok.body);
    let (w, h) = (
        ok.json()["width"].as_i64().unwrap(),
        ok.json()["height"].as_i64().unwrap(),
    );
    assert_eq!((w, h), (10_000, 10_000));
    assert!((w as u64) * (h as u64) <= MAX_OUTPUT_PIXELS);
}

#[test]
fn total_scale_guard_and_params_echo() {
    let (tmp, work) = workdir("total");
    let gw = gateway(&work);
    let img = tmp.path().join("tiny.png");
    make_png(&img, 100, 100);
    let out = work.join("x.png");

    // 防呆总倍数 64：再大就分不出有意义的结果，直接拒（且说明原因）
    let r = post(
        &gw,
        "/op/upscale",
        &json!({ "path": img.to_string_lossy(), "scale": 65.0,
                 "out": out.to_string_lossy() }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    let body = r.json();
    assert!(r.detail().contains("越界"), "{}", r.body);
    assert_eq!(body["op"], "/op/upscale", "错误帧必须点名 op：{}", r.body);
    assert_eq!(body["params"]["scale"], 65.0, "错误帧必须带实际参数：{}", r.body);
    assert_eq!(
        body["params"]["out"],
        json!(out.to_string_lossy()),
        "错误帧必须带回 out 原文：{}",
        r.body
    );
}

// ---------------------------------------------------------------------------
// 3. 错误上下文：任何端点的 400/403 都要能定位（不许裸 invalid_params）
// ---------------------------------------------------------------------------

#[test]
fn error_body_carries_op_and_params_for_every_endpoint() {
    let (tmp, work) = workdir("ctx");
    let gw = gateway(&work);
    let img = tmp.path().join("src.png");
    make_png(&img, 60, 40);
    let p = img.to_string_lossy().to_string();

    // 缺字段：detail 说清缺谁，op 说清哪个端点，params 回显已给的部分
    let r = post(&gw, "/op/upscale", &json!({ "path": p }));
    assert_eq!(r.status, 400, "{}", r.body);
    let body = r.json();
    assert_eq!(body["op"], "/op/upscale");
    assert!(r.detail().contains("缺字段"), "{}", r.body);
    assert_eq!(body["params"]["path"], json!(p));

    // 写根越界：错误类别不同，但 op/params 一样要在
    let r = post(
        &gw,
        "/op/crop",
        &json!({ "path": p, "box": [0, 0, 10, 10],
                 "out": tmp.path().join("outside.png").to_string_lossy() }),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    let body = r.json();
    assert_eq!(body["error"], "out_of_workroot");
    assert_eq!(body["op"], "/op/crop");
    assert_eq!(body["params"]["box"], json!([0, 0, 10, 10]));

    // 403 能力缺失（模型根为空 → 本地推理不可用）也必须点名端点
    let r = post(&gw, "/detect/u2net", &json!({ "path": p }));
    assert_eq!(r.status, 403, "{}", r.body);
    let body = r.json();
    assert_eq!(body["error"], "capability_missing");
    assert_eq!(body["op"], "/detect/u2net", "{}", r.body);
    assert_eq!(body["params"]["path"], json!(p));
}
