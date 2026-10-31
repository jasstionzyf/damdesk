//! HTTP 客户端构造（engine-tag 自持一份；分层规则见 crate 头注释）。
//! 语义与 engine-embed::http 一致：env 代理（ALL_PROXY/HTTPS_PROXY/HTTP_PROXY + NO_PROXY）。
//! 指向 localhost 的调用方（本地 vLLM/云 VL/测试 mock）应显式传 `Some("")` 直连。

use std::time::Duration;

/// 构造带全局超时的 agent（秒），代理取 env。
/// http_status_as_error(false)：非 2xx 也返回 Response，由调用方统一转 Upstream
/// （否则 ureq 3 直接 Err，响应体里的 error.message 读不到，上游报错不可读）。
pub fn agent(timeout_secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(timeout_secs)))
        .proxy(ureq::Proxy::try_from_env())
        .http_status_as_error(false)
        .build()
        .into()
}

/// 构造带全局超时的 agent，并显式指定代理（Some("") = 直连）。
pub fn agent_with_proxy(timeout_secs: u64, proxy: Option<&str>) -> ureq::Agent {
    let mut b = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(timeout_secs)))
        .http_status_as_error(false);
    match proxy {
        Some(p) if p.trim().is_empty() => b = b.proxy(None),
        Some(p) => {
            b = b.proxy(ureq::Proxy::new(p.trim()).ok());
        }
        None => b = b.proxy(ureq::Proxy::try_from_env()),
    }
    b.build().into()
}

/// 图片字节 → `data:<mime>;base64,...`。
pub fn image_data_url(bytes: &[u8], mime: &str) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    format!("data:{mime};base64,{b64}")
}

/// 按扩展名猜 mime（默认 image/jpeg）。
pub fn mime_for(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("bmp") => "image/bmp",
        Some("tif") | Some("tiff") => "image/tiff",
        _ => "image/jpeg",
    }
}
