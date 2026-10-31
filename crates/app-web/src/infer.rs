//! W3 推理 provider 装配 + 推理设置读写（R6：app-web 只编 openai_compat 远程实现）。
//!
//! 职责（薄壳，不放业务逻辑）：
//! - registry / 设置读写（settings 表 `inference` 键）
//! - **嵌入模型标识**：向量路由键 = `spec.name`，远程 provider 再加 endpoint 模型名后缀
//!   （`openai-compat-embedding:qwen3-vl-embedding-2b`）——同一库内换 endpoint 模型 =
//!   换表，禁混算铁律（registry 的 dims=0 spec 靠这个后缀区分实际模型）。
//! - provider 装配 + 进程内缓存（设置变更即失效）
//! - web 默认设置 seeding（首启写入远程 provider 默认值；env 可覆盖）

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use core_config::{InferenceSettings, ModelSpec, Registry, SpecKind};
use engine_embed::{factory, EmbedProvider};

/// 远程 provider 的 provider 实现名（registry/models.yaml）。
pub const REMOTE_PROVIDER: &str = "openai_compat";

/// 加载内置 registry（registry/models.yaml 编译期内嵌）。
pub fn registry() -> Result<Registry, String> {
    core_config::load_registry(None).map_err(|e| format!("load registry: {e}"))
}

/// registry 里的 embedding spec 列表（设置页「活跃 embedder」下拉用）
/// = **文搜图可用**的子集：`modalities` 含 `text`。
///
/// 为什么按 modalities 过滤（task）：单活跃分子同时承担文搜图和以图搜图，
/// 仅图像塔（去重塔 `dinov3-vits16-onnx`，modalities=[image]）被选成活跃分子后，
/// 对查询文本编不出向量 → 文搜图静默全空转（看不出是配置错）。
/// 远程 `openai_compat` spec 声明 [image,text]（端点真实能力静态不可知，见
/// `svc_library::similar_min_score_calibrated` 的 calibrated=false 约定），按声明放行。
pub fn embedding_specs(reg: &Registry) -> Vec<ModelSpec> {
    reg.models
        .iter()
        .filter(|m| m.kind == SpecKind::Embedding && text_capable(m))
        .cloned()
        .collect()
}

/// spec 是否具备文本能力（registry `modalities` 含 text）——文搜图前提。
pub fn text_capable(spec: &ModelSpec) -> bool {
    spec.modalities
        .iter()
        .any(|m| m.trim().eq_ignore_ascii_case("text"))
}

/// registry 里的「仅图像向量」embedding spec（不进活跃候选）。
///
/// **只供内部断言/测试用**：不通过任何 HTTP 响应暴露给界面（KI-6 裁决「设置界面不展示
/// 去重模型信息」——去重塔既不进候选，也不单列成「仅图像向量」说明项）。
pub fn image_only_embedding_specs(reg: &Registry) -> Vec<ModelSpec> {
    reg.models
        .iter()
        .filter(|m| m.kind == SpecKind::Embedding && !text_capable(m))
        .cloned()
        .collect()
}

/// 读推理设置（缺省 → 默认值）。
pub fn load_settings(conn: &rusqlite::Connection) -> InferenceSettings {
    core_storage::settings::get_or_default(conn, core_config::INFERENCE_SETTINGS_KEY)
        .unwrap_or_default()
}

/// 写推理设置。
pub fn save_settings(
    conn: &rusqlite::Connection,
    s: &InferenceSettings,
) -> Result<(), core_storage::StorageError> {
    core_storage::settings::set_json(conn, core_config::INFERENCE_SETTINGS_KEY, s)
}

/// 向量路由键（表名/emb_model/job kind 三处同一标识）。
/// 远程 provider：`{spec.name}:{endpoint model}`——换端点模型即换表（禁混算）。
/// 内嵌 provider：`{spec.name}`（模型身份由 spec 唯一确定）。
pub fn vector_model_key(spec: &ModelSpec, s: &InferenceSettings) -> String {
    if spec.provider == REMOTE_PROVIDER && !s.embedding.model.trim().is_empty() {
        format!("{}:{}", spec.name, s.embedding.model.trim())
    } else {
        spec.name.clone()
    }
}

/// 打标模型路由键（tag.model 列 / 报表展示统一用它，同向量键原则）：
/// `{provider 预设 id}:{endpoint model}`（如 `damai:qwen3.5-4b`）。
/// 换 provider 或换模型 → 键变 → 看板/覆盖度按模型分组，旧标签保留不混算
/// （tag 行按 (assetId,name) upsert，重跑由「全库重打」显式触发）。
pub fn tag_model_key(s: &InferenceSettings) -> String {
    format!("{}:{}", s.tag.provider, s.tag.model.trim())
}

/// 按设置装配打标器（enabled + configured 校验；registry tagging spec 名换路由键）。
pub fn build_tagger(conn: &rusqlite::Connection) -> Result<WebTagger, String> {
    let reg = registry()?;
    let settings = load_settings(conn);
    let mut spec = reg
        .models
        .iter()
        .find(|m| m.kind == SpecKind::Tagging)
        .cloned()
        .ok_or("registry 无 tagging spec")?;
    if !settings.tag.enabled {
        return Err("AI 打标未启用：请在设置页打开开关并保存".into());
    }
    if !settings.tag.configured() {
        return Err("打标端点未配置：请填 base_url + 模型名".into());
    }
    let model_key = tag_model_key(&settings);
    spec.name = model_key.clone();
    let mut provider = engine_tag::build_provider(spec, settings.tag.clone());
    provider.load().map_err(|e| format!("tag provider: {e}"))?;
    Ok(WebTagger {
        provider: Arc::from(provider),
        model_key,
    })
}

/// 已装配好的打标器（provider + 其身份）。
pub struct WebTagger {
    pub provider: Arc<dyn engine_tag::TagProvider>,
    /// 路由键（tag_model_key）
    pub model_key: String,
}

/// 首启 seeding：settings 无 `inference` 行时写入默认（远程 provider）。
/// env 覆盖项：DAM_WEB_EMBEDDER / DAM_WEB_EMBED_BASE_URL / DAM_WEB_EMBED_MODEL /
/// DAM_WEB_EMBED_DIMS / DAM_WEB_EMBED_API_KEY / DAM_WEB_EMBED_PROXY。
pub fn seed_defaults_if_absent(conn: &rusqlite::Connection) -> Result<bool, String> {
    seed_defaults_opts(conn, None)
}

/// 带调用方默认值的 seeding 变体（W7：桌面壳传本地 onnx，容器走 web 默认远程；
/// WIN-PRE2：tag 默认 provider 同样按形态分支——桌面 deepseek 公网 / 容器 dam-ai）。
/// 优先级：env DAM_WEB_EMBEDDER > default_embedder 参数 > web 默认（openai-compat-embedding）。
pub fn seed_defaults_opts(
    conn: &rusqlite::Connection,
    default_embedder: Option<&str>,
) -> Result<bool, String> {
    // 注意：迁移 0004 会先塞一行 ('inference','null') 占位 —— 必须把 null/空串视同「未设置」，
    // 否则 web 首启永远跳过 seeding，active_embedder 落到桌面默认（siglip2 onnx，web 不可用）。
    let raw = core_storage::settings::get_raw(conn, core_config::INFERENCE_SETTINGS_KEY)
        .map_err(|e| e.to_string())?;
    if !matches!(
        raw.as_deref().map(str::trim),
        None | Some("") | Some("null")
    ) {
        return Ok(false);
    }
    let env_or = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let mut s = InferenceSettings::default();
    // 默认 embedder：env 覆盖 > 调用方参数（桌面壳传本地 onnx）> web 默认远程（R6：web
    // 构建没有本地 ONNX；桌面 local-infer 构建离线默认 = 本地 siglip2）。
    s.active_embedder = std::env::var("DAM_WEB_EMBEDDER")
        .ok()
        .or_else(|| default_embedder.map(str::to_string))
        .unwrap_or_else(|| "openai-compat-embedding".to_string());
    s.embedding.base_url = env_or(
        "DAM_WEB_EMBED_BASE_URL",
        "http://host.docker.internal:8090/v1",
    );
    s.embedding.model = env_or("DAM_WEB_EMBED_MODEL", "qwen3-vl-embedding-2b");
    s.embedding.api_key = env_or("DAM_WEB_EMBED_API_KEY", "");
    s.embedding.dims = env_or("DAM_WEB_EMBED_DIMS", "0").parse().unwrap_or(0);
    // W4（R7）：打标默认 provider 预填 base_url、enabled 默认开（「开箱即用」语义；
    // 用户可在设置页切预设）。WIN-PRE2 裁决 2026-10-16：按形态分支——
    // 桌面壳（default_embedder=Some，同 embed 分支的形态信号）默认 DeepSeek 公网
    // 端点（用户机器上没有 host.docker.internal 容器口径）；web/容器形态保持 dam-ai。
    s.tag.provider = if default_embedder.is_some() {
        core_config::TAG_PROVIDER_DEEPSEEK.into()
    } else {
        core_config::TAG_PROVIDER_DAMAI.into()
    };
    let (tag_url_default, tag_model_default) = if default_embedder.is_some() {
        (
            core_config::tag_provider_preset(core_config::TAG_PROVIDER_DEEPSEEK)
                .expect("deepseek preset")
                .base_url,
            "deepseek-flash",
        )
    } else {
        (
            "http://host.docker.internal:8091/v1",
            "qwen3.5-4b",
        )
    };
    s.tag.base_url = env_or("DAM_WEB_TAG_BASE_URL", tag_url_default);
    s.tag.model = env_or("DAM_WEB_TAG_MODEL", tag_model_default);
    s.tag.enabled = env_or("DAM_WEB_TAG_ENABLED", "true") == "true";
    // api_key 两形态均默认空：桌面默认端点是公网计费 API，Key 必须用户自填；
    // TagTasks/设置页按 hasKey=false 给显式「去配置」引导（不许只靠失败计数兜底）。
    // 容器内默认直连（宿主/内网端点不走代理）；显式给 URL 才走代理
    s.embedding.proxy = Some(env_or("DAM_WEB_EMBED_PROXY", ""));
    save_settings(conn, &s).map_err(|e| e.to_string())?;
    Ok(true)
}

/// 已装配好的嵌入器（provider + 其身份）。
pub struct WebEmbedder {
    pub provider: Arc<dyn EmbedProvider>,
    /// 路由键（vector_model_key）
    pub model_key: String,
    pub dims: u32,
    /// 服务端 endpoint 模型名（设置页回显）
    pub endpoint_model: String,
}

/// 按设置装配嵌入器（load 探针 + 远程实测维度回写设置）。
///
/// `models_root` 为 **appData 式根**（factory 权重契约：`<root>/models/<name>`，
/// 与 app-tauri provider_root 语义一致；W3 版本此处错传 `{data_dir}/models`，
/// 但 web 构建从不装配本地 ONNX，故未暴露——W7 桌面内嵌首次真正走到该路径）。
///
/// dims 决议：settings.embedding.dims / spec.dims > 0 → 用它（装配后校验一致性）；
/// 0 → load 探针实测（EmbedProvider::measured_dims），把实测维度回写 settings 后重建。
pub fn build_embedder(
    conn: &rusqlite::Connection,
    models_root: &Path,
) -> Result<WebEmbedder, String> {
    let reg = registry()?;
    let mut settings = load_settings(conn);
    let mut spec = reg
        .by_name(&settings.active_embedder)
        .cloned()
        .ok_or_else(|| format!("registry 无模型 {}", settings.active_embedder))?;
    if spec.kind != SpecKind::Embedding {
        return Err(format!("{} 不是 embedding 模型", spec.name));
    }
    if spec.provider == REMOTE_PROVIDER && !settings.embedding.configured() {
        return Err("远程 embedding 端点未配置：请填 base_url + model".into());
    }
    if spec.dims == 0 {
        spec.dims = settings.embedding.dims;
    }
    // 路由键即 provider 身份：远程 spec 的 registry 名（openai-compat-embedding）不区分端点模型，
    // 而队列 kind / 向量表 / emb_model / report.model 全部走 spec.name —— 必须先把 name 换成
    // 「registry 名:端点模型」，否则入队 kind 与消费 kind 不一致（队列永远空转）。
    let model_key = vector_model_key(&spec, &settings);
    spec.name = model_key.clone();

    let mut provider = factory::build_from_spec(&spec, &settings, models_root)
        .map_err(|e| format!("build provider: {e}"))?;
    provider.load().map_err(|e| format!("load provider: {e}"))?;

    let measured = provider.measured_dims();
    if let Some(m) = measured {
        if m == 0 {
            return Err("endpoint 未返回向量维度".into());
        }
        if m != spec.dims {
            // 实测与声明不符：以实测为准（并回写设置，避免每次重建）
            spec.dims = m;
            settings.embedding.dims = m;
            save_settings(conn, &settings).map_err(|e| e.to_string())?;
            provider = factory::build_from_spec(&spec, &settings, models_root)
                .map_err(|e| format!("build provider: {e}"))?;
            provider.load().map_err(|e| format!("load provider: {e}"))?;
        }
    } else if spec.dims == 0 {
        return Err(format!(
            "无法确定向量维度（provider={} 未报实测维度，registry dims=0）",
            spec.provider
        ));
    }

    Ok(WebEmbedder {
        provider: Arc::from(provider),
        model_key,
        dims: spec.dims,
        endpoint_model: settings.embedding.model.clone(),
    })
}

/// provider 缓存：按「设置指纹」失效（settings 写入后指纹变化即重建）。
pub struct ProviderCache {
    inner: Mutex<Option<(String, WebEmbedder)>>,
}

impl ProviderCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    pub fn invalidate(&self) {
        *self.inner.lock().unwrap() = None;
    }

    /// 取当前嵌入器（缓存命中即复用；指纹 = active_embedder|base_url|model|dims|api_key）。
    pub fn get(&self, conn: &rusqlite::Connection, models_root: &Path) -> Result<WebEmbedder, String> {
        let s = load_settings(conn);
        let key = format!(
            "{}|{}|{}|{}|{}",
            s.active_embedder,
            s.embedding.base_url,
            s.embedding.model,
            s.embedding.dims,
            s.embedding.api_key
        );
        if let Some((k, e)) = self.inner.lock().unwrap().as_ref() {
            if *k == key {
                return Ok(clone_embedder(e));
            }
        }
        let built = build_embedder(conn, models_root)?;
        let out = clone_embedder(&built);
        *self.inner.lock().unwrap() = Some((key, built));
        Ok(out)
    }

    /// 测试连接：强制重建（不吃缓存），返回 (model_key, dims, endpoint_model)。
    pub fn test_connection(
        &self,
        conn: &rusqlite::Connection,
        models_root: &Path,
    ) -> Result<(String, u32, String), String> {
        let e = build_embedder(conn, models_root)?;
        Ok((e.model_key, e.dims, e.endpoint_model))
    }
}

fn clone_embedder(e: &WebEmbedder) -> WebEmbedder {
    WebEmbedder {
        provider: e.provider.clone(),
        model_key: e.model_key.clone(),
        dims: e.dims,
        endpoint_model: e.endpoint_model.clone(),
    }
}

impl Default for ProviderCache {
    fn default() -> Self {
        Self::new()
    }
}

/// 库文件路径（供 spawn_blocking 线程开独立连接）。
pub fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join("dam.db")
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_config::InferenceSettings;

    fn reg() -> Registry {
        registry().expect("registry")
    }

    fn names(specs: Vec<ModelSpec>) -> Vec<String> {
        specs.into_iter().map(|m| m.name).collect()
    }

    /// 去重塔（dinov3，modalities=[image]）不得成为「活跃 embedder」候选——
    /// 否则被选成文搜图活跃分子后查询文本编不出向量，搜图静默全空转（task）。
    #[test]
    fn embeddings_test_dedup_tower_excluded_from_active_candidates() {
        let r = reg();
        let picks = names(embedding_specs(&r));
        assert!(
            !picks.contains(&core_config::DEDUP_EMBEDDER.to_string()),
            "去重塔不该进活跃候选: {picks:?}"
        );
        assert!(
            picks.contains(&"openai-compat-embedding".to_string()),
            "远程端点（声明 [image,text]）仍是文搜图默认: {picks:?}"
        );
        assert!(picks.contains(&"siglip2-base-p224-onnx".to_string()));
        let image_only = names(image_only_embedding_specs(&r));
        assert_eq!(
            image_only,
            vec![core_config::DEDUP_EMBEDDER.to_string()],
            "仅图像向量 spec 应全部落进 imageOnlyEmbedders"
        );
        // KI-1 防回归（不按塔名写死）：任何 embedding spec 只要 modalities 无 text，
        // 就一定不能出现在活跃候选里——新增仅图像塔时这条自动生效。
        let mut checked = 0;
        for m in r.models.iter().filter(|m| m.kind == SpecKind::Embedding) {
            if text_capable(m) {
                continue;
            }
            checked += 1;
            assert!(
                !picks.contains(&m.name),
                "无 text modality 的 spec 混进活跃候选: {} (picks={picks:?})",
                m.name
            );
        }
        assert!(checked > 0, "registry 里应至少有一个仅图像塔用来覆盖本断言");
    }

    /// text_capable 只看 registry 的 modalities 声明（不猜端点真实能力）。
    #[test]
    fn embeddings_test_text_capable_reads_modalities() {
        let r = reg();
        let siglip = r.by_name("siglip2-base-p224-onnx").unwrap();
        let dedup = r.by_name(core_config::DEDUP_EMBEDDER).unwrap();
        let remote = r.by_name("openai-compat-embedding").unwrap();
        assert!(text_capable(siglip));
        assert!(!text_capable(dedup));
        // 远程 spec 声明 [image,text]：端点是不是纯图塔静态不可知 → 按声明放行
        assert!(text_capable(remote));
    }

    /// 路由键（spec 名 + `:endpoint model`）不改变 spec 身份：embedding_specs 里
    /// 出现的裸名与运行期向量路由键前缀一致。
    #[test]
    fn embeddings_test_route_key_prefix_matches_spec_name() {
        let r = reg();
        let remote = r.by_name("openai-compat-embedding").unwrap().clone();
        let mut s = InferenceSettings::default();
        s.active_embedder = "openai-compat-embedding".into();
        s.embedding.model = "qwen3-vl-embedding-2b".into();
        let key = vector_model_key(&remote, &s);
        assert_eq!(key, "openai-compat-embedding:qwen3-vl-embedding-2b");
        assert_eq!(key.split(':').next().unwrap(), remote.name);
        assert!(names(embedding_specs(&r)).contains(&remote.name));
    }
}
