//! Ollama 模型扫描（D9）：`GET {root}/api/tags` 列出本地已拉取模型。
//!
//! Ollama 的 OpenAI 兼容口在 `{root}/v1`，但模型清单在原生口 `{root}/api/tags`，
//! 故扫描 URL = base_url 去掉尾部 `/v1` 段 + `/api/tags`。
//! 扫描固定直连（不读 env 代理）：Ollama 语义就是本机服务，代理只会引入误判。

use crate::{Result, TagError};

/// base_url（含 /v1）→ Ollama 原生 API 根（去尾部 /v1 段）。
/// `http://127.0.0.1:11434/v1` → `http://127.0.0.1:11434`；无 /v1 原样返回。
pub fn ollama_api_root(base_url: &str) -> String {
    let t = base_url.trim().trim_end_matches('/');
    match t.strip_suffix("/v1") {
        Some(root) => root.to_string(),
        None => t.to_string(),
    }
}

/// 扫描 Ollama 已拉取模型（GET /api/tags → models[].name，排序去重）。
/// 连不上 → 带明确指引的错误（验收：无服务时扫描失败要有清晰报错）。
pub fn scan_ollama_models(base_url: &str, timeout_ms: u64) -> Result<Vec<String>> {
    let root = ollama_api_root(base_url);
    if root.is_empty() {
        return Err(TagError::NotConfigured(
            "base URL 未填写：请先填 Ollama 地址（如 http://127.0.0.1:11434/v1）".to_string(),
        ));
    }
    let url = format!("{root}/api/tags");
    let agent = crate::http::agent_with_proxy(timeout_ms / 1000 + 1, Some("")); // 直连
    let mut resp = agent
        .get(&url)
        .call()
        .map_err(|e| TagError::Http(format!("{url}: {e}")))
        .map_err(|e| hint_unreachable(&root, e))?;
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| TagError::Http(format!("read {url}: {e}")))?;
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| TagError::Parse(format!("{url} 返回的不是 JSON: {e}")))?;
    if !(200..300).contains(&status) {
        return Err(TagError::Upstream(format!(
            "HTTP {status} from {url}: {}",
            text.chars().take(200).collect::<String>()
        )));
    }
    let mut names: Vec<String> = parsed
        .get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        return Err(TagError::Upstream(format!(
            "Ollama 已连接（{root}）但没有任何模型：先 `ollama pull qwen2.5vl` 拉一个 VL 模型"
        )));
    }
    names.sort();
    names.dedup();
    Ok(names)
}

/// 连接类错误补「怎么修」的指引（验收要求：失败原因可读）。
fn hint_unreachable(root: &str, e: TagError) -> TagError {
    let s = e.to_string();
    let connectionish = [
        "Connection refused",
        "Network Error",
        "timed out",
        "Dns Failed",
        "NoRoute",
    ]
    .iter()
    .any(|k| s.contains(k));
    if connectionish {
        TagError::Http(format!(
            "无法连接 Ollama（{root}）：请确认已安装并正在运行（`ollama serve`），且 base URL 正确"
        ))
    } else {
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_root_strips_v1() {
        assert_eq!(
            ollama_api_root("http://127.0.0.1:11434/v1"),
            "http://127.0.0.1:11434"
        );
        assert_eq!(
            ollama_api_root("http://localhost:11434/v1/"),
            "http://localhost:11434"
        );
        // 用户填了不带 /v1 的地址也能扫
        assert_eq!(
            ollama_api_root("http://127.0.0.1:11434"),
            "http://127.0.0.1:11434"
        );
        assert_eq!(ollama_api_root("  "), "");
    }

    #[test]
    fn scan_requires_base_url() {
        let err = scan_ollama_models("", 3_000).unwrap_err();
        assert!(err.to_string().contains("base URL 未填写"), "got: {err}");
    }

    #[test]
    fn scan_unreachable_has_clear_hint() {
        // 127.0.0.1:9（discard 端口）必然拒绝连接 → 错误必须带可读指引
        let err = scan_ollama_models("http://127.0.0.1:9/v1", 3_000).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("无法连接 Ollama"), "got: {msg}");
        assert!(msg.contains("ollama serve"), "got: {msg}");
    }
}
