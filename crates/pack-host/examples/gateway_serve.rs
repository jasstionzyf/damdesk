//! 独立能力网关（验收/调试用）：起一个真网关，把 URL + 一次性令牌打到 stdout 一行，
//! 之后一直服务，直到 stdin 关闭（`quit`/EOF）或进程被杀。
//!
//! 为什么要有这个 example：`Gateway` 是库 API，平时由宿主（`app-web`/`PackHost`）内嵌；
//! 但**插件侧验收**（Python 语料脚本直接打真网关）需要一个能单独拉起的进程——
//! 否则只能整包走 JSON-RPC，把「几何对不对」和「协议通不通」两件事搅在一起。
//!
//! 用法（容器内）：
//!   DAM_MODELS_ROOT=/ cargo run -q -p pack-host --example gateway_serve -- \
//!       --work-dir /tmp/gw-work --capabilities io.read,detect.u2net,op.geometric
//! → stdout: {"url":"http://127.0.0.1:PORT","token":"..."}（**只这一行**，其余日志走 stderr）
//!
//! `--capabilities` 缺省 = 本卡需要的三项；给了就按给的授权（协议 §8：未授权 → 403）。

use std::io::{BufRead, Write};
use std::net::SocketAddr;
use std::path::PathBuf;

use pack_host::{Gateway, GatewayConfig};

fn parse_args() -> Result<(PathBuf, Vec<String>, SocketAddr), String> {
    let mut work_dir: Option<PathBuf> = None;
    let mut caps: Option<Vec<String>> = None;
    let mut bind: SocketAddr = SocketAddr::from(([127, 0, 0, 1], 0));
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--work-dir" => work_dir = args.next().map(PathBuf::from),
            "--capabilities" => {
                let raw = args.next().ok_or("--capabilities 需要逗号分隔的能力列表")?;
                caps = Some(
                    raw.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                );
            }
            "--bind" => {
                let raw = args.next().ok_or("--bind 需要 addr:port")?;
                bind = raw.parse().map_err(|e| format!("--bind 解析失败：{e}"))?;
            }
            other => return Err(format!("未知参数：{other}")),
        }
    }
    let work_dir = work_dir.ok_or("必须给 --work-dir（网关写根）")?;
    let caps = caps.unwrap_or_else(|| {
        vec![
            "io.read".to_string(),
            "detect.u2net".to_string(),
            "op.geometric".to_string(),
        ]
    });
    Ok((work_dir, caps, bind))
}

fn main() {
    let (work_dir, capabilities, bind) = match parse_args() {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("[gateway_serve] {msg}");
            std::process::exit(2);
        }
    };
    std::fs::create_dir_all(&work_dir).expect("建 work-dir 失败");
    let mut cfg = GatewayConfig::new(capabilities.clone(), &work_dir);
    cfg.bind = bind;
    let gateway = match Gateway::start(cfg) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("[gateway_serve] 启动失败：{e}");
            std::process::exit(3);
        }
    };
    let line = serde_json::json!({ "url": gateway.base_url(), "token": gateway.token() });
    println!("{line}");
    let _ = std::io::stdout().flush();
    eprintln!(
        "[gateway_serve] work_dir={} capabilities={:?}",
        work_dir.display(),
        capabilities
    );

    // 服务到 stdin 关闭/收到 quit（父进程退出会撑开 stdin → 自动收尾）
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        match line {
            Ok(text) if text.trim() == "quit" => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    gateway.stop();
    eprintln!("[gateway_serve] 已停服");
}
