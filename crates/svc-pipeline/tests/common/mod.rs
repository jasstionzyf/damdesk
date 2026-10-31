#![allow(dead_code)] // 测试共享 HTTP 桩：探针方法按用例取用，未用到也保留
//! 极简 HTTP mock 服务器（测试专用）：单线程 accept 循环 + 可注入 responder。
//!
//! 为什么要自己写：D3 的 tag/embed 客户端必须能在「无外网、无 vLLM」的环境里被真实验证
//! （请求体形态 + 响应解析 + 错误路径）。仅依赖 std，避免给 workspace 引入 mock 框架依赖。
//! 注：engine-embed 与 engine-tag 各持一份（两层同层禁互引，测试工具不跨 crate 共享）。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// 记录到的请求（method, path, body）。
pub type Recorded = Arc<Mutex<Vec<(String, String, String)>>>;

/// 跑在后台线程的 mock 服务器。
pub struct MockServer {
    pub base_url: String,
    pub requests: Recorded,
    pub hits: Arc<Mutex<usize>>,
}

impl MockServer {
    /// responder 收到 (path, body) 返回 (status_code, body)。
    pub fn start<F>(responder: F) -> Self
    where
        F: Fn(&str, &str) -> (u16, String) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let addr = listener.local_addr().unwrap();
        let requests: Recorded = Arc::new(Mutex::new(Vec::new()));
        let hits = Arc::new(Mutex::new(0usize));
        let req2 = requests.clone();
        let hits2 = hits.clone();
        let responder = Arc::new(responder);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let req2 = req2.clone();
                let hits2 = hits2.clone();
                let responder = responder.clone();
                std::thread::spawn(move || {
                    if let Some((method, path, body)) = read_request(&mut stream) {
                        *hits2.lock().unwrap() += 1;
                        req2.lock()
                            .unwrap()
                            .push((method, path.clone(), body.clone()));
                        let (status, resp_body) = responder(&path, &body);
                        let _ = write_response(&mut stream, status, &resp_body);
                    }
                });
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            requests,
            hits,
        }
    }

    /// 最后一次请求体。
    pub fn last_body(&self) -> Option<String> {
        self.requests
            .lock()
            .unwrap()
            .last()
            .map(|(_, _, b)| b.clone())
    }

    pub fn last_path(&self) -> Option<String> {
        self.requests
            .lock()
            .unwrap()
            .last()
            .map(|(_, p, _)| p.clone())
    }

    pub fn count(&self) -> usize {
        *self.hits.lock().unwrap()
    }
}

fn read_request(stream: &mut TcpStream) -> Option<(String, String, String)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    // 读到 header 结束
    let header_end = loop {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let content_length: usize = head
        .lines()
        .find_map(|l| {
            let l = l.to_ascii_lowercase();
            l.strip_prefix("content-length:")
                .map(|v| v.trim().parse().unwrap_or(0))
        })
        .unwrap_or(0);
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    Some((method, path, String::from_utf8_lossy(&body).to_string()))
}

fn write_response(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes())?;
    stream.flush()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
