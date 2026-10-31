//! provider 工厂：按 registry spec + settings 造 `Box<dyn EmbedProvider>`。
//!
//! 加一个模型/provider = registry 加一条 ModelSpec + 这里加一个 match 分支（零改核心）。
//! embModel 存 spec.name，向量表按 name 分表 → 禁混算铁律天然满足（PLAN.md §9）。

use core_config::{InferenceSettings, ModelSpec, Registry};
use std::path::Path;

#[cfg(feature = "local-infer")]
use crate::onnx_dinov3::OnnxDinoV3Provider;
#[cfg(feature = "local-infer")]
use crate::onnx_siglip::OnnxSiglipProvider;
use crate::openai_compat::OpenAiCompatProvider;
use crate::{EmbedError, EmbedProvider, Result};

/// provider 实现标识（registry spec.provider 的取值）。
pub const PROVIDER_ONNX_SIGLIP: &str = "onnx_siglip";
pub const PROVIDER_ONNX_DINOV3: &str = "onnx_dinov3";
pub const PROVIDER_OPENAI_COMPAT: &str = "openai_compat";

/// 找激活的 embedding spec（settings.active_embedder）。
pub fn active_spec(registry: &Registry, settings: &InferenceSettings) -> Result<ModelSpec> {
    registry
        .by_name(&settings.active_embedder)
        .cloned()
        .ok_or_else(|| EmbedError::UnknownModel(settings.active_embedder.clone()))
}

/// 按 spec + settings 造 provider。
/// `models_root` = `{appData}/models`（内嵌 provider 从 `<root>/<spec.name>/` 读模型）。
pub fn build_embedder(
    registry: &Registry,
    settings: &InferenceSettings,
    models_root: &Path,
) -> Result<Box<dyn EmbedProvider>> {
    let spec = active_spec(registry, settings)?;
    build_from_spec(&spec, settings, models_root)
}

/// 指定 spec 造 provider（不依赖 settings.active_embedder；切换 provider 时用）。
pub fn build_from_spec(
    spec: &ModelSpec,
    settings: &InferenceSettings,
    models_root: &Path,
) -> Result<Box<dyn EmbedProvider>> {
    match spec.provider.as_str() {
        #[cfg(feature = "local-infer")]
        PROVIDER_ONNX_SIGLIP => {
            let manifest = crate::model_store::manifest_for(&spec.name);
            let dir = core_config::model_dir(models_root, &spec.name);
            Ok(Box::new(OnnxSiglipProvider::new(
                spec.clone(),
                dir,
                manifest.as_ref(),
            )))
        }
        #[cfg(feature = "local-infer")]
        PROVIDER_ONNX_DINOV3 => {
            let manifest = crate::model_store::manifest_for(&spec.name);
            let dir = core_config::model_dir(models_root, &spec.name);
            Ok(Box::new(OnnxDinoV3Provider::new(
                spec.clone(),
                dir,
                manifest.as_ref(),
            )))
        }
        // R6：web 构建（local-infer off）没有本地 ONNX 推理 —— 明确报错而非静默降级。
        #[cfg(not(feature = "local-infer"))]
        PROVIDER_ONNX_SIGLIP | PROVIDER_ONNX_DINOV3 => Err(EmbedError::Unsupported(format!(
            "本构建未编入本地 ONNX 推理（engine-embed/local-infer 关闭）：provider={}。\
             请改选 openai_compat 远程端点（dam-ai / dashscope / 自定义 base_url）",
            spec.provider
        ))),
        PROVIDER_OPENAI_COMPAT => Ok(Box::new(OpenAiCompatProvider::new(
            spec.clone(),
            settings.embedding.clone(),
        ))),
        other => Err(EmbedError::ProviderMismatch {
            spec: other.to_string(),
            requested: "onnx_siglip | onnx_dinov3 | openai_compat".to_string(),
        }),
    }
}

/// 该 spec 是否内嵌（需要模型文件下载）。远程 provider 不需要。
pub fn needs_download(spec: &ModelSpec) -> bool {
    spec.provider == PROVIDER_ONNX_SIGLIP || spec.provider == PROVIDER_ONNX_DINOV3
}

/// 按名字造指定 provider（D5：去重塔与 active_embedder 解耦，固定 DINOv3）。
pub fn build_dedup_embedder(
    registry: &Registry,
    settings: &InferenceSettings,
    models_root: &Path,
) -> Result<Box<dyn EmbedProvider>> {
    let spec = registry
        .by_name(core_config::DEDUP_EMBEDDER)
        .cloned()
        .ok_or_else(|| EmbedError::UnknownModel(core_config::DEDUP_EMBEDDER.to_string()))?;
    build_from_spec(&spec, settings, models_root)
}

/// 判重塔（DINOv3 ViT-S/16）权重两件套是否在盘 —— **不随安装包分发**（GH-5 合规裁决：
/// DINOv3 权重受 Meta 许可约束，禁止随包再分发），因此判重塔是「可选塔」。
///
/// 快路径只查文件名，与 `onnx_dinov3` 的加载前置检查同一口径（`model.onnx` +
/// `model.onnx_data`）；不触网、不做 sha256。缺件时上层（app-web）必须显式报
/// 「判重不可用，需自备模型（Meta 许可）」，不许 panic、不许静默拿 pHash 层当判重塔结论。
///
/// 自备方式：把两件放 `{models_root}/models/dinov3-vits16-onnx/`（`core_config::model_dir`
/// 契约，与引擎读取的根一致 —— 这里报「在盘」而引擎读不到 = 撒谎，不允许发生）。
pub fn dedup_files_present(models_root: &Path) -> bool {
    let dir = core_config::model_dir(models_root, core_config::DEDUP_EMBEDDER);
    dir.join("model.onnx").is_file() && dir.join("model.onnx_data").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_config::load_registry;

    #[cfg(feature = "local-infer")]
    #[test]
    fn builds_default_onnx_provider_from_settings() {
        let reg = load_registry(None).unwrap();
        let s = InferenceSettings::default();
        let p = build_embedder(&reg, &s, Path::new("/tmp/appdata/models")).unwrap();
        assert_eq!(p.spec().name, core_config::DEFAULT_EMBEDDER);
        assert_eq!(p.spec().dims, 768);
        assert!(!p.ready()); // 未 load
        assert!(needs_download(p.spec()));
    }

    #[test]
    fn builds_remote_provider_when_selected() {
        let reg = load_registry(None).unwrap();
        let mut s = InferenceSettings::default();
        s.active_embedder = "openai-compat-embedding".to_string();
        s.embedding.base_url = "http://127.0.0.1:9/v1".into();
        s.embedding.model = "bge".into();
        let p = build_embedder(&reg, &s, Path::new("/tmp/x")).unwrap();
        assert_eq!(p.spec().name, "openai-compat-embedding");
        assert!(!needs_download(p.spec()));
    }

    #[test]
    fn unknown_active_embedder_is_loud() {
        let reg = load_registry(None).unwrap();
        let mut s = InferenceSettings::default();
        s.active_embedder = "nope".into();
        match build_embedder(&reg, &s, Path::new("/tmp/x")) {
            Ok(_) => panic!("unknown active embedder must fail"),
            Err(e) => assert!(matches!(e, EmbedError::UnknownModel(_)), "got {e:?}"),
        }
    }

    /// GH-5：判重塔降级为可选 —— `dedup_files_present` 必须与引擎加载的前置检查同一口径：
    /// 两件套（model.onnx + model.onnx_data）齐 → true；缺任意一件/目录不存在 → false。
    /// 不 panic（缺件是常态，不是错误），不触网。
    #[test]
    fn dedup_files_present_requires_both_weight_files() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "gh5-dedup-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let dir = core_config::model_dir(&root, core_config::DEDUP_EMBEDDER);

        // 目录不存在 → false（不 panic）
        assert!(!dedup_files_present(&root));
        // 空目录 / 只有图件 → false
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!dedup_files_present(&root));
        std::fs::write(dir.join("model.onnx"), b"onnx-graph").unwrap();
        assert!(!dedup_files_present(&root), "缺外置权重必须报缺");
        // 两件齐 → true
        std::fs::write(dir.join("model.onnx_data"), b"onnx-data").unwrap();
        assert!(dedup_files_present(&root));
        let _ = std::fs::remove_dir_all(&root);
    }
}
