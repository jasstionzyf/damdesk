//! `OnnxDinoV3Provider` — 去重塔（2026-10-13 双嵌入器裁决）。
//!
//! DINOv3 ViT-S/16 ONNX（fp32，单塔，`model.onnx` + 外置权重 `model.onnx_data`）：
//! - 输入 `pixel_values` [1,3,256,256]（动态分辨率图；res 256 为裁决锁定）
//! - 输出 `pooler_output` [1,384]（未归一化，出口统一 L2 归一）
//! - **仅 image**：`embed_texts` 返回 Unsupported（文本查询走语义塔 siglip）
//!
//! 预处理复用 [`crate::onnx_siglip::ImagePreprocess`]（mean/std 来自 registry
//! spec.preprocess = ImageNet 参数，锁定不改；改 = 全量重嵌入）。
//!
//! 分发：`{appData}/models/<spec.name>/`；hf-mirror 暂无公开 onnx 仓（2026-10-13
//! 实查 404），manifest 的 url_path 按 `<name>/model.onnx*` 相对路径声明，
//! base_url 可指向自建镜像或 `file://` 本地分发（见 reports/D5-spike-decision.md）。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use core_config::ModelSpec;
use ort::session::Session;

use crate::model_store::ModelManifest;
use crate::onnx_siglip::{pick_output, ImagePreprocess};
use crate::{EmbedError, EmbedProvider, Result};

struct Loaded {
    session: Session,
    out_name: String,
}

/// DINOv3 内嵌 provider（image-only）。
pub struct OnnxDinoV3Provider {
    spec: ModelSpec,
    dir: PathBuf,
    preprocess: ImagePreprocess,
    loaded: Mutex<Option<Loaded>>,
}

impl OnnxDinoV3Provider {
    pub fn new(
        spec: ModelSpec,
        dir: impl Into<PathBuf>,
        _manifest: Option<&ModelManifest>,
    ) -> Self {
        let preprocess = ImagePreprocess::from_spec(&spec);
        Self {
            spec,
            dir: dir.into(),
            preprocess,
            loaded: Mutex::new(None),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 模型文件齐备（图 + 外置权重两件套，快路径不做 sha）。
    pub fn files_present(&self) -> bool {
        self.dir.join("model.onnx").is_file() && self.dir.join("model.onnx_data").is_file()
    }

    pub fn preprocess_spec(&self) -> &ImagePreprocess {
        &self.preprocess
    }

    fn load_if_needed(&self) -> Result<()> {
        if self.loaded.lock().unwrap().is_some() {
            return Ok(());
        }
        let mut guard = self.loaded.lock().unwrap();
        if guard.is_some() {
            return Ok(());
        }
        let graph = self.dir.join("model.onnx");
        let weights = self.dir.join("model.onnx_data");
        for p in [&graph, &weights] {
            if !p.is_file() {
                return Err(EmbedError::ModelMissing(p.display().to_string()));
            }
        }
        let session = Session::builder()
            .and_then(|b| b.commit_from_file(&graph))
            .map_err(|e| EmbedError::Onnx(format!("{}: {e}", graph.display())))?;
        let out_name = pick_output(&session)
            .ok_or_else(|| EmbedError::Onnx("dinov3 has no outputs".to_string()))?;
        *guard = Some(Loaded { session, out_name });
        Ok(())
    }

    pub fn embed_image_bytes(&self, bytes: &[u8]) -> Result<Vec<f32>> {
        let data = self.preprocess.preprocess(bytes)?;
        let size = self.preprocess.image_size as usize;
        let dims = self.spec.dims;
        self.with_loaded(|l| {
            let t = ort::value::Tensor::from_array((
                vec![1usize, 3, size, size],
                data.into_boxed_slice(),
            ))
            .map_err(|e| EmbedError::Onnx(e.to_string()))?;
            let outputs = l
                .session
                .run(ort::inputs!["pixel_values" => t])
                .map_err(|e| EmbedError::Onnx(format!("run pixel_values: {e}")))?;
            let value = outputs
                .get(l.out_name.as_str())
                .ok_or_else(|| EmbedError::Onnx(format!("output {} missing", l.out_name)))?;
            let arr = value
                .try_extract_array::<f32>()
                .map_err(|e| EmbedError::Onnx(format!("extract: {e}")))?;
            let shape = arr.shape().to_vec();
            // pooler_output [1,384]；last_hidden_state 退化路径取首 token（CLS）
            let flat: Vec<f32> = arr.iter().copied().collect();
            let mut v = if shape.len() >= 3 {
                let d = *shape.last().unwrap();
                flat[..d].to_vec()
            } else {
                flat
            };
            if v.len() as u32 != dims {
                return Err(EmbedError::Dim {
                    expect: dims,
                    got: v.len() as u32,
                });
            }
            crate::l2_normalize(&mut v);
            Ok(v)
        })
    }

    fn with_loaded<R>(&self, f: impl FnOnce(&mut Loaded) -> Result<R>) -> Result<R> {
        self.load_if_needed()?;
        let mut guard = self.loaded.lock().unwrap();
        let l = guard
            .as_mut()
            .ok_or_else(|| EmbedError::NotLoaded(self.spec.name.clone()))?;
        f(l)
    }
}

impl EmbedProvider for OnnxDinoV3Provider {
    fn load(&mut self) -> Result<()> {
        self.load_if_needed()
    }

    fn embed_images(&self, paths: &[PathBuf]) -> Result<Vec<Vec<f32>>> {
        self.load_if_needed()?;
        let mut out = Vec::with_capacity(paths.len());
        for p in paths {
            let bytes = std::fs::read(p)?;
            out.push(self.embed_image_bytes(&bytes)?);
        }
        Ok(out)
    }

    fn embed_texts(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Err(EmbedError::Unsupported(format!(
            "{} is image-only（文本查询走语义塔）",
            self.spec.name
        )))
    }

    fn ready(&self) -> bool {
        self.loaded.lock().unwrap().is_some()
    }

    fn spec(&self) -> &ModelSpec {
        &self.spec
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ModelSpec {
        let reg = core_config::load_registry(None).unwrap();
        reg.by_name(core_config::DEDUP_EMBEDDER).unwrap().clone()
    }

    #[test]
    fn files_missing_report_clearly() {
        let p = OnnxDinoV3Provider::new(spec(), "/tmp/definitely-missing", None);
        assert!(!p.files_present());
        let err = p.embed_images(&[PathBuf::from("/tmp/x.jpg")]).unwrap_err();
        assert!(err.to_string().contains("model files missing"), "got {err}");
    }

    #[test]
    fn texts_unsupported() {
        let p = OnnxDinoV3Provider::new(spec(), "/tmp/x", None);
        let err = p.embed_texts(&["红色"]).unwrap_err();
        assert!(err.to_string().contains("image-only"), "got {err}");
    }
}
