//! L1 `engine-embed` — EmbedProvider trait（对齐 dam-ai LoadableModel，PLAN.md §9）。
//!
//! D1 落地 trait 契约与 registry 寻址；D3 实装两个 provider：
//! - [`onnx_siglip::OnnxSiglipProvider`]：内嵌默认（ort + SigLIP2 双塔 ONNX，CPU 零配置）
//! - [`openai_compat::OpenAiCompatProvider`]：OpenAI 兼容 `/v1/embeddings`（可接 dam-ai server /
//!   dashscope / JinaCLIP API 等）
//!
//! 模型分发落 `model_store`（首启静默下载 + sha256 校验）；provider 构造走 [`factory`]。
//!
//! R6 feature 门控：`local-infer`（默认开）编入 ort/ONNX 内嵌推理 + model_store；
//! web 容器构建 `default-features = false` → 只剩 openai_compat 远程 provider，
//! 镜像不背 ONNX 运行时与模型权重（factory 对 onnx_* spec 报明确的 Unsupported）。

pub mod factory;
pub mod http;
#[cfg(feature = "local-infer")]
pub mod model_store;
#[cfg(feature = "local-infer")]
pub mod onnx_dinov3;
#[cfg(feature = "local-infer")]
pub mod onnx_siglip;
pub mod openai_compat;

use core_config::ModelSpec;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("provider not loaded: {0}")]
    NotLoaded(String),
    #[error("batch size mismatch: {0} inputs, {1} vectors")]
    BatchSize(usize, usize),
    #[error("dim mismatch: expect {expect}, got {got}")]
    Dim { expect: u32, got: u32 },
    #[error("not normalized (L2={0})")]
    NotNormalized(f64),
    #[error("model files missing: {0}")]
    ModelMissing(String),
    #[error("model file corrupt: {0}")]
    ModelCorrupt(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("image decode: {0}")]
    Image(String),
    #[error("onnx: {0}")]
    Onnx(String),
    #[error("tokenizer: {0}")]
    Tokenizer(String),
    #[error("http: {0}")]
    Http(String),
    #[error("provider not configured: {0}")]
    NotConfigured(String),
    #[error("registry has no model named {0}")]
    UnknownModel(String),
    #[error("unsupported operation: {0}")]
    Unsupported(String),
    #[error("provider impl mismatch: spec.provider={spec}, requested={requested}")]
    ProviderMismatch { spec: String, requested: String },
}

pub type Result<T> = std::result::Result<T, EmbedError>;

/// R6 门控事实：本构建是否编入本地 ONNX 推理（cargo feature `local-infer`，默认开）。
/// web 容器构建为 `false` → provider 只能是 `openai_compat` 远程端点。
pub const LOCAL_INFER: bool = cfg!(feature = "local-infer");

/// Embedding provider 统一契约：load / embed_images / embed_texts / ready / spec。
pub trait EmbedProvider: Send + Sync {
    /// 加载模型/建立连接。幂等：已 ready 时直接 Ok。
    fn load(&mut self) -> Result<()>;
    /// 批量图片向量（输入为图片文件路径，解码在 provider 内做）。
    fn embed_images(&self, paths: &[PathBuf]) -> Result<Vec<Vec<f32>>>;
    /// 批量文本向量（文搜图查询侧）。
    fn embed_texts(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    /// 是否已可用。
    fn ready(&self) -> bool;
    /// 模型声明（name/dims/normalized 来自 registry）。
    fn spec(&self) -> &ModelSpec;
    /// 端点实测维度（load 后可得；内嵌 provider 维度由 spec 唯一确定 → 默认 None）。
    /// 用途：registry 里 dims=0 的远程 spec 由实测补齐（禁混算需真实维度）。
    fn measured_dims(&self) -> Option<u32> {
        None
    }
}

/// L2 归一化（provider 出口统一归一，满足 spec.normalized）。
pub fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// L2 范数。
pub fn l2_norm(v: &[f32]) -> f64 {
    v.iter()
        .map(|x| (*x as f64) * (*x as f64))
        .sum::<f64>()
        .sqrt()
}

/// 余弦相似度（对已归一化向量即点积，此处按原式算，不假设归一）。
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let (mut dot, mut na, mut nb) = (0f64, 0f64, 0f64);
    for i in 0..a.len().min(b.len()) {
        dot += a[i] as f64 * b[i] as f64;
        na += (a[i] as f64).powi(2);
        nb += (b[i] as f64).powi(2);
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// 向量自检：长度=1（normalized spec）或维度匹配（未归一化 spec）。
pub fn validate_vectors(spec: &ModelSpec, out: &[Vec<f32>]) -> Result<()> {
    for v in out {
        if v.len() as u32 != spec.dims {
            return Err(EmbedError::Dim {
                expect: spec.dims,
                got: v.len() as u32,
            });
        }
        if spec.normalized {
            let l2 = l2_norm(v);
            if (l2 - 1.0).abs() > 1e-3 {
                return Err(EmbedError::NotNormalized(l2));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_config::SpecKind;

    fn spec(dims: u32, normalized: bool) -> ModelSpec {
        ModelSpec {
            name: "t".into(),
            kind: SpecKind::Embedding,
            dims,
            modalities: vec![],
            preprocess: serde_yaml::Value::Null,
            normalized,
            provider: "test".into(),
        }
    }

    #[test]
    fn validate_catches_dim_and_norm() {
        let s = spec(3, true);
        let good = vec![vec![0.0, 0.0, 1.0]];
        assert!(validate_vectors(&s, &good).is_ok());
        assert!(validate_vectors(&s, &vec![vec![1.0, 0.0]]).is_err()); // dim
        assert!(validate_vectors(&s, &vec![vec![1.0, 1.0, 1.0]]).is_err()); // not unit
        let raw = spec(2, false);
        assert!(validate_vectors(&raw, &vec![vec![3.0, 4.0]]).is_ok()); // dims only
    }

    #[test]
    fn normalize_and_cosine() {
        let mut v = vec![3.0f32, 4.0];
        l2_normalize(&mut v);
        assert!((l2_norm(&v) - 1.0).abs() < 1e-6);
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-6);
        assert!(cosine(&v, &[-v[0], -v[1]]) < -0.999);
    }
}
