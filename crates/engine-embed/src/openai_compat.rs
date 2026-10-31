//! `OpenAiCompatProvider` — OpenAI 兼容 `/v1/embeddings` 远程 embedding（PLAN.md §9 第二实现）。
//!
//! 可接任何 OpenAI 兼容多模态 embedding 服务：dam-ai server / dashscope / JinaCLIP API /
//! 自建 vLLM embedding 端点。图片按 data URL 提交（`image_url` content part），文本按 string。
//! 出口统一 L2 归一；维度以 endpoint 实测为准（registry 里 dims=0 表示「由配置决定」）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use base64::Engine;
use core_config::ModelSpec;
pub use core_config::RemoteEmbedSettings as RemoteEmbedConfig;

use crate::{l2_normalize, EmbedError, EmbedProvider, Result};

/// OpenAI 兼容远程 embedding provider。
pub struct OpenAiCompatProvider {
    spec: ModelSpec,
    cfg: RemoteEmbedConfig,
    agent: ureq::Agent,
    loaded: AtomicBool,
    /// 实测维度（load 后填）
    dims: Mutex<Option<u32>>,
}

impl OpenAiCompatProvider {
    pub fn new(spec: ModelSpec, cfg: RemoteEmbedConfig) -> Self {
        let agent = crate::http::agent_with_proxy(cfg.timeout_secs.max(1), cfg.proxy.as_deref());
        Self {
            spec,
            cfg,
            agent,
            loaded: AtomicBool::new(false),
            dims: Mutex::new(None),
        }
    }

    pub fn config(&self) -> &RemoteEmbedConfig {
        &self.cfg
    }

    /// 实测维度（未 load 时 None）。inherent 名字与 EmbedProvider::measured_dims 一致：
    /// 具体类型调用走 inherent，`dyn`/泛型调用走 trait（两者同值）。
    pub fn measured_dims(&self) -> Option<u32> {
        *self.dims.lock().unwrap()
    }

    fn endpoint(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.cfg.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    fn post_json(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let url = self.endpoint(path);
        let mut req = self
            .agent
            .post(&url)
            .header("content-type", "application/json");
        if !self.cfg.api_key.trim().is_empty() {
            req = req.header(
                "authorization",
                &format!("Bearer {}", self.cfg.api_key.trim()),
            );
        }
        let resp = req
            .send_json(body)
            .map_err(|e| EmbedError::Http(format!("POST {url}: {e}")))?;
        let mut resp = resp;
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| EmbedError::Http(format!("read {url}: {e}")))?;
        serde_json::from_str::<serde_json::Value>(&text)
            .map_err(|e| EmbedError::Http(format!("bad json from {url}: {e}; body={text}")))
    }

    fn embed_remote(&self, input: serde_json::Value) -> Result<Vec<Vec<f32>>> {
        let body = serde_json::json!({
            "model": self.cfg.model,
            "input": input,
            "encoding_format": "float",
        });
        let resp = self.post_json("/embeddings", &body)?;
        let mut vecs = parse_embeddings_response(&resp)?;
        // 出口统一归一（远程服务可能已归一，重归一无损）
        for v in vecs.iter_mut() {
            l2_normalize(v);
        }
        if self.cfg.dims > 0 {
            for v in &vecs {
                if v.len() as u32 != self.cfg.dims {
                    return Err(EmbedError::Dim {
                        expect: self.cfg.dims,
                        got: v.len() as u32,
                    });
                }
            }
        }
        *self.dims.lock().unwrap() = vecs.first().map(|v| v.len() as u32);
        Ok(vecs)
    }
}

impl EmbedProvider for OpenAiCompatProvider {
    fn load(&mut self) -> Result<()> {
        if self.loaded.load(Ordering::SeqCst) {
            return Ok(());
        }
        if !self.cfg.configured() {
            return Err(EmbedError::NotConfigured(
                "endpoint base_url / model 未配置".to_string(),
            ));
        }
        // 轻量连通性探针：真实发一次文本 embedding，顺带拿到实测维度
        let probe = self.embed_remote(serde_json::json!(["ping"]))?;
        if probe.is_empty() {
            return Err(EmbedError::Http("probe returned no embedding".into()));
        }
        self.loaded.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn embed_images(&self, paths: &[std::path::PathBuf]) -> Result<Vec<Vec<f32>>> {
        if !self.ready() {
            return Err(EmbedError::NotLoaded(self.spec.name.clone()));
        }
        let mut parts = Vec::with_capacity(paths.len());
        for p in paths {
            let bytes = std::fs::read(p)?;
            let mime = mime_for(p);
            parts.push(serde_json::json!({
                "type": "image_url",
                "image_url": { "url": image_data_url(&bytes, mime) }
            }));
        }
        self.embed_remote(serde_json::Value::Array(parts))
    }

    fn embed_texts(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if !self.ready() {
            return Err(EmbedError::NotLoaded(self.spec.name.clone()));
        }
        let arr: Vec<serde_json::Value> = texts
            .iter()
            .map(|t| serde_json::Value::String((*t).to_string()))
            .collect();
        self.embed_remote(serde_json::Value::Array(arr))
    }

    fn ready(&self) -> bool {
        self.loaded.load(Ordering::SeqCst)
    }

    fn spec(&self) -> &ModelSpec {
        &self.spec
    }

    fn measured_dims(&self) -> Option<u32> {
        OpenAiCompatProvider::measured_dims(self)
    }
}

/// 图片 → `data:<mime>;base64,...`（OpenAI 多模态 content part 通用格式）。
pub fn image_data_url(bytes: &[u8], mime: &str) -> String {
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

/// 解析 OpenAI 兼容响应：`data[i].embedding`（float 数组或 base64 字符串），
/// 兼容裸 `embeddings: [[...]]` 形态。
pub fn parse_embeddings_response(resp: &serde_json::Value) -> Result<Vec<Vec<f32>>> {
    if let Some(arr) = resp.get("embeddings").and_then(|v| v.as_array()) {
        return parse_vec_array(arr);
    }
    let data = resp
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| EmbedError::Http(format!("no data field: {resp}")))?;
    let mut out = Vec::with_capacity(data.len());
    for item in data {
        let emb = item
            .get("embedding")
            .ok_or_else(|| EmbedError::Http(format!("item without embedding: {item}")))?;
        out.push(parse_embedding_value(emb)?);
    }
    Ok(out)
}

fn parse_embedding_value(v: &serde_json::Value) -> Result<Vec<f32>> {
    if let Some(arr) = v.as_array() {
        return Ok(arr
            .iter()
            .filter_map(|x| x.as_f64().map(|f| f as f32))
            .collect());
    }
    if let Some(s) = v.as_str() {
        // base64 编码的 float32 小端缓冲（OpenAI encoding_format=base64）
        let raw = base64::engine::general_purpose::STANDARD
            .decode(s)
            .map_err(|e| EmbedError::Http(format!("base64 embedding: {e}")))?;
        if raw.len() % 4 != 0 {
            return Err(EmbedError::Http(
                "base64 embedding length not multiple of 4".into(),
            ));
        }
        return Ok(raw
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect());
    }
    Err(EmbedError::Http("unsupported embedding payload".into()))
}

fn parse_vec_array(arr: &[serde_json::Value]) -> Result<Vec<Vec<f32>>> {
    arr.iter().map(parse_embedding_value).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_config::load_registry;

    fn spec() -> ModelSpec {
        load_registry(None)
            .unwrap()
            .by_name("openai-compat-embedding")
            .unwrap()
            .clone()
    }

    #[test]
    fn parse_float_and_base64_payloads() {
        let float =
            serde_json::json!({"data":[{"embedding":[0.1,0.2,0.3]},{"embedding":[1.0,0.0,0.0]}]});
        let v = parse_embeddings_response(&float).unwrap();
        assert_eq!(v.len(), 2);
        assert!((v[0][1] - 0.2).abs() < 1e-6);

        // base64 float32 小端： [1.0, 2.0]
        let mut raw = Vec::new();
        raw.extend_from_slice(&1.0f32.to_le_bytes());
        raw.extend_from_slice(&2.0f32.to_le_bytes());
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
        let body = serde_json::json!({"data":[{"embedding": b64}]});
        let v2 = parse_embeddings_response(&body).unwrap();
        assert_eq!(v2[0], vec![1.0, 2.0]);

        // 裸 embeddings 形态
        let bare = serde_json::json!({"embeddings": [[0.5, 0.5]]});
        assert_eq!(parse_embeddings_response(&bare).unwrap()[0], vec![0.5, 0.5]);

        // 坏响应必须报错，不能静默返回空
        assert!(parse_embeddings_response(&serde_json::json!({"data":[{}]})).is_err());
        assert!(parse_embeddings_response(&serde_json::json!({"error":"nope"})).is_err());
    }

    #[test]
    fn unconfigured_provider_fails_load_with_clear_error() {
        let mut p = OpenAiCompatProvider::new(spec(), RemoteEmbedConfig::default());
        assert!(!p.ready());
        let err = p.load().unwrap_err();
        assert!(matches!(err, EmbedError::NotConfigured(_)), "got {err:?}");
        // 未 load → embed 必须报 NotLoaded，不能静默
        assert!(matches!(
            p.embed_texts(&["x"]).unwrap_err(),
            EmbedError::NotLoaded(_)
        ));
    }

    #[test]
    fn data_url_and_mime() {
        let u = image_data_url(b"abc", "image/png");
        assert_eq!(u, "data:image/png;base64,YWJj");
        assert_eq!(mime_for(std::path::Path::new("/a/b.PNG")), "image/png");
        assert_eq!(mime_for(std::path::Path::new("/a/b.webp")), "image/webp");
        assert_eq!(mime_for(std::path::Path::new("/a/b")), "image/jpeg");
    }
}
