//! K07-FIX-B 回归：`/detect/white_stats` 与 K00 旧引擎基线 **逐值一致**（R2 那 9 行）。
//!
//! 为什么这条必须存在：白底判定从「VL 猜」改成「像素统计」的唯一正当性就是**同口径**——
//! 端点内部调的就是旧引擎那份 `engine-classic::detectors::white_edge`，所以
//! `mean_dev / worst_dev / edge_pixels / pass` 必须与 `reports/t_dc40d71f-equivalence.json`
//! 里 legacy 通道的 R2 metrics 逐个数字相同（容差只留给浮点序列化）。
//!
//! 基线来源：`reports/t_dc40d71f-equivalence.json`（2026-10-21，legacy 通道，10 张 balanced 语料）。
//! 其中 `f001.jpg` 的 R2 在旧引擎被 R0 gate 短路成 `skipped`（无 metrics），故这里不列；
//! 其余 9 行逐值钉住（5 张真实白底 + 2 张偏白 fail + a004 的 2/34272 微量偏差）。
//!
//! 语料不在本机时整组跳过（容器/CI 无语料目录，env DAM_ECOMMERCE_CORPUS 指路），打印原因而不是假装通过。

use std::path::{Path, PathBuf};

use pack_host::gateway::ENDPOINTS;
use pack_host::{Gateway, GatewayConfig};
use serde_json::json;

const CORPUS_ENV: &str = "DAM_ECOMMERCE_CORPUS";

/// (文件名, mean_dev, worst_dev, edge_pixels, pass) —— 取自 K00/K07 基线的 legacy R2 metrics。
const BASELINE: [(&str, f64, f64, u64, bool); 9] = [
    ("a001.jpg", 0.0, 0.0, 100_584, true),
    ("a002.jpg", 0.0, 0.0, 35_776, true),
    ("a003.jpg", 0.0, 0.0, 45_944, true),
    ("a004.jpg", 0.00014589169000933706, 2.0, 34_272, true),
    ("a005.jpg", 0.0, 0.0, 46_656, true),
    ("f002.jpg", 167.54606394621638, 252.0, 51_168, false),
    ("f003.jpg", 132.2225027399405, 251.0, 51_096, false),
    ("f004.jpg", 0.0, 0.0, 11_952, true),
    ("f005.jpg", 0.0, 0.0, 8_368, true),
];

fn corpus_file(name: &str) -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var(CORPUS_ENV).ok()?);
    // 语料分两个目录（pass_fixtures / fail_fixtures），按文件名前缀找
    let sub = if name.starts_with('a') { "pass_fixtures" } else { "fail_fixtures" };
    let path = root.join(sub).join(name);
    path.is_file().then_some(path)
}

fn post(gw: &Gateway, path: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
    let addr = gw.base_url().replace("http://", "");
    let mut stream = std::net::TcpStream::connect(&addr).expect("连接网关失败");
    let text = body.to_string();
    let req = format!(
        "POST {path} HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nauthorization: Bearer {}\r\nconnection: close\r\n\r\n{text}",
        text.len(),
        gw.token()
    );
    use std::io::{Read, Write};
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let all = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = all.split_once("\r\n\r\n").expect("应答缺分隔");
    let code: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (code, serde_json::from_str(body).expect("应答不是 JSON"))
}

#[test]
fn white_stats_matches_legacy_baseline_on_corpus() {
    let caps: Vec<String> = ENDPOINTS.iter().map(|(_, c)| c.to_string()).collect();
    let work = tempfile::tempdir().expect("tempdir");
    let gw = Gateway::start(GatewayConfig::new(caps, work.path()).with_token("t")).unwrap();

    let mut checked = 0usize;
    for (name, mean_dev, worst_dev, edge_pixels, pass) in BASELINE {
        let Some(path) = corpus_file(name) else {
            eprintln!("跳过整组：语料未设（env {CORPUS_ENV}）或缺 {name}");
            return;
        };
        let (code, body) = post(
            &gw,
            "/detect/white_stats",
            json!({ "path": path.to_string_lossy(), "band": 4, "max_dev": 8 }),
        );
        assert_eq!(code, 200, "{name} → {body}");
        assert_eq!(body["band"], 4, "{name}");
        assert_eq!(body["edge_pixels"], edge_pixels, "{name} 边缘带像素数必须与旧引擎一致");
        assert_eq!(body["worst_dev"], worst_dev, "{name} 最差单像素偏差必须逐值一致");
        assert_eq!(body["pass"], pass, "{name} 判定必须与旧引擎一致");
        let got = body["mean_dev"].as_f64().expect("mean_dev 缺失");
        assert!(
            (got - mean_dev).abs() < 1e-12,
            "{name} mean_dev 必须逐值一致：旧引擎 {mean_dev} vs 端点 {got}"
        );
        checked += 1;
    }
    assert_eq!(checked, 9, "应逐值核对 9 行（f001 被 R0 gate 短路故不入基线）");
}

#[test]
fn white_stats_rejects_bad_params_and_missing_file() {
    let caps: Vec<String> = ENDPOINTS.iter().map(|(_, c)| c.to_string()).collect();
    let work = tempfile::tempdir().expect("tempdir");
    let gw = Gateway::start(GatewayConfig::new(caps, work.path()).with_token("t")).unwrap();

    let (code, body) = post(&gw, "/detect/white_stats", json!({ "path": "/nope/x.png" }));
    assert_eq!(code, 404, "{body}");
    let (code, body) = post(&gw, "/detect/white_stats", json!({}));
    assert_eq!(code, 400, "{body}");
    let (code, body) = post(&gw, "/detect/white_stats", json!({ "path": "rel.png" }));
    assert_eq!(code, 400, "相对路径必须 400：{body}");
    assert!(Path::new("/nope/x.png").exists() == false);
}
