//! L0 `core-config` — settings/provider 配置读写、models.yaml → ModelSpec 解析。
//! 对齐 dam-ai 的 registry/loader 模式（PLAN.md §9）。

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("yaml: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("registry file has no active provider")]
    NoActiveProvider,
    #[error("pack {pack}: {msg}")]
    PackLoad { pack: String, msg: String },
    #[error("pack {pack} schema: {msg}")]
    PackValidation { pack: String, msg: String },
    /// 装载期 lint（C8）：未知键 / 结构不符等「作者写错但引擎会静默吞掉」的错误。
    /// 消息 = 逐条 `文件: 键路径: [code] 说明（已知: ...）`。
    #[error("pack {pack} lint 未通过（{n} 条）:\n{schema_diff}", n = .issues.len(), schema_diff = .issues.join("\n  "))]
    PackLint { pack: String, issues: Vec<String> },
}

pub type Result<T> = std::result::Result<T, ConfigError>;

/// registry/models.yaml 单条模型声明（对齐 dam-ai ModelSpec）。
/// preprocess 变更必须重嵌入（禁混算铁律，PLAN.md §9）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelSpec {
    pub name: String,
    pub kind: SpecKind,
    pub dims: u32,
    #[serde(default)]
    pub modalities: Vec<String>,
    /// 输入归一化参数（image_size / mean / std 等），变更 = 重嵌入
    #[serde(default)]
    pub preprocess: serde_yaml::Value,
    /// 输出向量是否已归一化
    #[serde(default = "default_true")]
    pub normalized: bool,
    /// provider 实现：onnx_siglip / openai_compat
    pub provider: String,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpecKind {
    Embedding,
    Tagging,
    /// 主体分割（C3）：u2net ONNX 掩码模型（无 dims 语义，dims=0）。
    Segmentation,
    /// 超分（C4）：realesr-general-x4v3（4× 放大，dims=0）。
    SuperResolution,
    /// 局部重绘（C4）：big-lama FP32（512² 固定输入，dims=0）。
    Inpainting,
    /// 文字识别（OCR-E）：PP-OCRv6_medium det/rec（dims=0；检索加权融合数据源）。
    Ocr,
}

/// models.yaml 顶层结构：models 列表。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registry {
    pub models: Vec<ModelSpec>,
}

impl Registry {
    /// 从 YAML 文本解析。
    pub fn from_yaml_str(s: &str) -> Result<Self> {
        Ok(serde_yaml::from_str(s)?)
    }

    /// 从文件解析。
    pub fn from_file(path: &std::path::Path) -> Result<Self> {
        Self::from_yaml_str(&std::fs::read_to_string(path)?)
    }

    pub fn by_name(&self, name: &str) -> Option<&ModelSpec> {
        self.models.iter().find(|m| m.name == name)
    }
}

// ---------- 推理设置（D3） ----------

/// 云 VL 打标配置（唯一可选增强，PLAN.md §9；无本地 tagger，v1 留接口位）。
/// 向前兼容：老配置/部分字段缺失 → 用默认值兜底（settings 表可能存着旧形状，不能因此炸掉启动）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TagSettings {
    pub enabled: bool,
    /// provider 预设 id（D9）：dashscope / zhipu / ollama / custom（老配置缺字段 → custom）
    pub provider: String,
    /// OpenAI 兼容 base（含 /v1，如 https://api.example.com/v1）
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// 三合一提示词模板（title_ch/title_en/description/tags）
    pub prompt_template: String,
    pub max_tokens: u32,
    pub timeout_ms: u64,
    /// None = 走 env 代理；Some("") = 直连（本地 vLLM）；Some("http://host:port") = 指定代理
    #[serde(default)]
    pub proxy: Option<String>,
}

impl Default for TagSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: TAG_PROVIDER_CUSTOM.to_string(),
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            prompt_template: DEFAULT_TAG_PROMPT.to_string(),
            max_tokens: TagSettings::DEFAULT_MAX_TOKENS,
            timeout_ms: 60_000,
            proxy: None,
        }
    }
}

impl TagSettings {
    /// 三合一 JSON 输出 + 合规 VL 共用的默认 max_tokens（用户裁决 2026-10-20：**4096 上限**）。
    /// 预算要覆盖思考 token + 正文：中文 description + 8~12 个 tag 的 JSON 实测 1.3k
    /// 左右（WIN-E2E 在 1024 下真机 4/11 张被截断 `EOF while parsing`）；VL 侧
    /// deepseek-flash 思考链实测 190~550 字符、本地 qwen 500~740 字符，900 会把
    /// `content` 挤成空串（实测 20 张里 content_empty 7 / parse_failures 9）。
    /// 4096 是上限口径，模型不吐这么多时按实际生成计费，不会额外花钱。
    /// 注意：产品路径下 VL 与打标共用本值（`compliance_api` 取 `tag.max_tokens`）。
    pub const DEFAULT_MAX_TOKENS: u32 = 4096;

    /// 实际下发的 max_tokens：0 = 未设置 → 默认值。
    pub fn effective_max_tokens(&self) -> u32 {
        if self.max_tokens == 0 {
            Self::DEFAULT_MAX_TOKENS
        } else {
            self.max_tokens
        }
    }

    /// 生效值来源：`default`（未改，用的是默认值）/ `settings`（设置页覆盖）。
    /// UI 与错误文案都靠它区分「默认 2048」和「用户改了但改小了」。
    pub fn max_tokens_source(&self) -> &'static str {
        if self.max_tokens == 0 || self.max_tokens == Self::DEFAULT_MAX_TOKENS {
            "default"
        } else {
            "settings"
        }
    }

    /// 生效值的人读标签，如 `2048（默认）` / `1024（设置页覆盖）`。
    pub fn max_tokens_label(&self) -> String {
        match self.max_tokens_source() {
            "default" => format!("{}（默认）", self.effective_max_tokens()),
            _ => format!("{}（设置页覆盖）", self.effective_max_tokens()),
        }
    }

    /// 配置齐备（可发起请求）；enabled 与否由调用方决定是否使用。
    pub fn configured(&self) -> bool {
        !self.base_url.trim().is_empty() && !self.model.trim().is_empty()
    }
}

/// 远程 embedding 端点配置（第二 provider：OpenAI 兼容 /v1/embeddings）。
/// 与 TagSettings 不同：这是 embedding 侧，dims=0 表示「以 endpoint 实测为准」。
/// 向前兼容：缺字段 → 默认值兜底。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RemoteEmbedSettings {
    /// 含 /v1 的 base（如 https://api.example.com/v1）
    pub base_url: String,
    pub api_key: String,
    /// endpoint 侧模型名
    pub model: String,
    /// 期望维度（0 = 不校验，以实测为准）
    pub dims: u32,
    pub timeout_secs: u64,
    /// None = 走 env 代理；Some("") = 直连；Some("http://host:port") = 指定代理
    pub proxy: Option<String>,
}

impl Default for RemoteEmbedSettings {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            dims: 0,
            timeout_secs: 120,
            proxy: None,
        }
    }
}

impl RemoteEmbedSettings {
    pub fn configured(&self) -> bool {
        !self.base_url.trim().is_empty() && !self.model.trim().is_empty()
    }
}

/// 推理设置：active provider（按 spec.name 选择）+ 模型分发源 + 打标配置。
/// 向前兼容：缺字段 → 默认值兜底（新增设置项不破坏已落库的旧 json）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct InferenceSettings {
    /// 默认内嵌 ONNX provider（registry/models.yaml 的 spec.name）
    pub active_embedder: String,
    /// 模型下载源根（国内镜像默认 hf-mirror；测试可指本地 http/file）
    pub model_base_url: String,
    /// 首启静默下载（关掉则只在用户点「下载模型」时下载）
    pub auto_download: bool,
    /// 远程 embedding 端点（active_embedder 指向 openai_compat spec 时使用）
    #[serde(default)]
    pub embedding: RemoteEmbedSettings,
    pub tag: TagSettings,
}

impl Default for InferenceSettings {
    fn default() -> Self {
        Self {
            active_embedder: DEFAULT_EMBEDDER.to_string(),
            model_base_url: DEFAULT_MODEL_BASE_URL.to_string(),
            auto_download: true,
            embedding: RemoteEmbedSettings::default(),
            tag: TagSettings::default(),
        }
    }
}

/// 默认内嵌 embedding provider（与 registry/models.yaml 一致）。
pub const DEFAULT_EMBEDDER: &str = "siglip2-base-p224-onnx";

/// 默认模型分发源根（国内镜像；单文件 URL = base + "/" + manifest.url_path）。
pub const DEFAULT_MODEL_BASE_URL: &str =
    "https://hf-mirror.com/onnx-community/siglip2-base-patch16-224-ONNX/resolve/main";

/// 打标提示词默认值（内置默认模板：标题/描述/标签三合一）。
pub const DEFAULT_TAG_PROMPT: &str = r#"你是一名图片编目助手。请分析这张图片，返回 JSON：
{"title_ch": "中文标题(<=20字)", "title_en": "English title", "description": "中文描述(<=60字)", "tags": [{"name": "标签", "score": 0.95}]}
标签 5-10 个，中英不限，只输出 JSON。"#;

/// settings 表里推理配置的 key。
pub const INFERENCE_SETTINGS_KEY: &str = "inference";

// ---------- tag provider 预设（D9 裁决 2026-10-13） ----------

/// provider id 常量（TagSettings.provider 取值）。
pub const TAG_PROVIDER_DAMAI: &str = "damai";
pub const TAG_PROVIDER_DEEPSEEK: &str = "deepseek";
pub const TAG_PROVIDER_DASHSCOPE: &str = "dashscope";
pub const TAG_PROVIDER_ZHIPU: &str = "zhipu";
pub const TAG_PROVIDER_OLLAMA: &str = "ollama";
pub const TAG_PROVIDER_CUSTOM: &str = "custom";

/// 一个 tag provider 预设：设置页选 provider → base_url 预填（可改）+ 模型下拉候选。
/// 内置静态表（单一事实源在 Rust；UI 经 `tag_provider_presets` 命令读取）。
/// 只 Serialize（Rust → UI 单向）；含 &'static 引用字段故不 derive Deserialize。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TagProviderPreset {
    /// 预设 id（= TagSettings.provider）
    pub id: &'static str,
    /// UI 显示名
    pub label: &'static str,
    /// 预填 base_url（空 = 自填，custom）
    pub base_url: &'static str,
    /// 模型下拉候选（可改写为任意值；ollama = 动态扫描，此表为空）
    pub models: &'static [&'static str],
    /// 是否支持动态扫描模型（GET /api/tags，仅 ollama）
    pub scan: bool,
}

/// 内置 provider 预设表（R7 裁决 2026-10-14：dam-ai 自托管默认 + D9 全套）。
/// 首位 = 默认项（设置页下拉按表序展示）。
pub const TAG_PROVIDER_PRESETS: &[TagProviderPreset] = &[
    TagProviderPreset {
        id: TAG_PROVIDER_DAMAI,
        label: "dam-ai /v1/tagging（自托管默认）",
        // tagger 引擎代理口（GPU :8091，vLLM 后端 /v1/chat/completions 透传）
        base_url: "http://host.docker.internal:8091/v1",
        models: &["qwen3.5-4b"],
        scan: false,
    },
    TagProviderPreset {
        id: TAG_PROVIDER_DEEPSEEK,
        label: "DeepSeek 多模态 API（deepseek-flash）",
        // 公网官方端点（桌面形态默认；WIN-PRE2 裁决 2026-10-16：
        // 桌面用户机器上没有 host.docker.internal 容器口径，默认必须指向公网可达端点）
        base_url: "https://api.deepseek.com/v1",
        models: &["deepseek-flash"],
        scan: false,
    },
    TagProviderPreset {
        id: TAG_PROVIDER_DASHSCOPE,
        label: "阿里云百炼 DashScope（qwen-vl）",
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        // 实测 2026-10-13 base64 图打标正常
        models: &["qwen3-vl-plus"],
        scan: false,
    },
    TagProviderPreset {
        id: TAG_PROVIDER_ZHIPU,
        label: "智谱开放平台（glm-4.6v 系）",
        base_url: "https://open.bigmodel.cn/api/paas/v4",
        models: &["glm-4.6v", "glm-4.5v", "glm-4v-flash"],
        scan: false,
    },
    TagProviderPreset {
        id: TAG_PROVIDER_OLLAMA,
        label: "Ollama（本地，自动扫描已拉取模型）",
        base_url: "http://127.0.0.1:11434/v1",
        models: &[],
        scan: true,
    },
    TagProviderPreset {
        id: TAG_PROVIDER_CUSTOM,
        label: "自定义（OpenAI 兼容 /v1）",
        base_url: "",
        models: &[],
        scan: false,
    },
];

/// 按 id 查预设（未知 id / 老配置 → None，UI 按 custom 兜底）。
pub fn tag_provider_preset(id: &str) -> Option<&'static TagProviderPreset> {
    TAG_PROVIDER_PRESETS.iter().find(|p| p.id == id)
}

/// 去重塔（2026-10-13 双嵌入器裁决）：固定 DINOv3，与 active_embedder（语义塔）解耦。
pub const DEDUP_EMBEDDER: &str = "dinov3-vits16-onnx";

/// settings 表里库目录配置的 key（2026-10-13 裁决：libraryRoot 与 appData 分离）。
pub const LIBRARY_SETTINGS_KEY: &str = "library";

/// settings 表里 path 标签开关的 key（D8 裁决 2026-10-13）。
pub const PATH_TAG_SETTINGS_KEY: &str = "path_tags";

/// path 标签开关（D8 裁决 2026-10-13）：目录段/文件名解析为标签，**默认均 false 不开启**。
/// 关闭时导入不产标签、已产标签不回删；设置页可按当前开关重扫。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct PathTagSettings {
    /// 目录名按导入根的相对路径段逐段成标签（根本身不入）
    pub use_dirs: bool,
    /// 文件名去扩展名后按分隔符切词成标签（噪声词丢弃）
    pub use_filenames: bool,
}

impl PathTagSettings {
    /// 两开关全关 = 功能停用（导入/重解析零成本跳过）。
    pub fn disabled(&self) -> bool {
        !self.use_dirs && !self.use_filenames
    }
}

/// 库目录设置：copyin 模式原图落盘根。None/空 = 默认 `{appData}/library`。
/// db/thumbs/models 仍固定在 appData，不随库目录迁移。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LibrarySettings {
    pub root: Option<String>,
}

impl LibrarySettings {
    /// 生效库目录（根 = 传入的 appData）。
    pub fn effective_root(&self, app_data: &std::path::Path) -> std::path::PathBuf {
        match self.root.as_deref() {
            Some(r) if !r.trim().is_empty() => std::path::PathBuf::from(r),
            _ => app_data.join("library"),
        }
    }
}

/// EXIF2（task）— settings 表里 EXIF 解析开关的 key。
pub const EXIF_SETTINGS_KEY: &str = "exif";

// ---------------------------------------------------------------------------
// IP-K16（task）：合规执行通道唯一化
// ---------------------------------------------------------------------------
//
// 2026-10-22 用户裁决：一个行业的合规检测/处理 = **一个插件**（自带规则/词表/报告元数据/
// 流程代码）。宿主不再有 `legacy | plugin` 双跑开关，也没有「先装行业包再切引擎」两步 ——
// 执行通道恒为插件子进程，`COMPLIANCE_SETTINGS_KEY` / `DAM_COMPLIANCE_ENGINE` 一并下线。
// 旧的内联引擎（pipeline steps 驱动）只作为**基线参考实现**留在测试侧。

/// GPS 处理档位（隐私红线：默认 Off = 不解析不入库，不碰 GPS IFD）。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GpsMode {
    /// 不解析不存储（默认）
    #[default]
    Off,
    /// 只存经纬度坐标（gps_lat/gps_lon 列）
    Coords,
    /// 只存逆地理编码地名（坐标剥除不落库，地名进 exif json placeName）
    Place,
}

/// 逆地理编码 provider（与打标 provider 同哲学：默认未配置 = 不做逆编码）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct GpsProviderSettings {
    /// "nominatim"（OpenStreetMap 兼容 /reverse 接口）；空 = 未配置
    pub name: String,
    /// 在线 provider 的用户自有 key（不内置任何官方 key）
    pub api_key: String,
    /// 缺省用 provider 官方端点；可自建
    pub base_url: String,
}

impl GpsProviderSettings {
    pub fn configured(&self) -> bool {
        !self.name.trim().is_empty()
    }
}

/// EXIF 解析设置：导入时解析默认**开**（无隐私代价的底座能力）；GPS 默认**关**。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct ExifSettings {
    /// 导入时解析 EXIF（含画幅列回填）
    pub parse_on_import: bool,
    /// GPS 档位（默认 Off）
    pub gps: GpsMode,
    /// 逆地理编码 provider（gps=Place 且 provider 配置了才生效）
    pub gps_provider: GpsProviderSettings,
}

impl Default for ExifSettings {
    fn default() -> Self {
        Self {
            parse_on_import: true,
            gps: GpsMode::Off,
            gps_provider: GpsProviderSettings::default(),
        }
    }
}

impl ExifSettings {
    /// 是否需要读 GPS IFD（隐私闸门：唯一入口）。
    pub fn want_gps(&self) -> bool {
        self.gps != GpsMode::Off
    }
    /// Place 档需要逆编码（provider 未配置时如实跳过，地名留空 + 原因记录）。
    pub fn want_reverse_geocode(&self) -> bool {
        self.gps == GpsMode::Place && self.gps_provider.configured()
    }
}

/// 模型目录根：`{appData}/models`（首启下载落点，PLAN.md §9）。
pub fn models_root(app_data_dir: &std::path::Path) -> std::path::PathBuf {
    app_data_dir.join("models")
}

/// 单模型目录：`{appData}/models/<spec.name>`。
pub fn model_dir(app_data_dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    models_root(app_data_dir).join(name)
}

// ---------- 进程级模型根注入的互斥（测试隔离基础设施） ----------

/// 进程级模型根注入锁。
///
/// `engine-segment` / `engine-restore` 的 `set_models_root()` 写的是**进程级全局态**
/// （`DAM_MODELS_ROOT` 环境变量也是进程级兜底）。同一个测试二进制里并行的用例各自改根
/// 会互相踩（卡 task 实测：C3 真模型 e2e 把根指向真 u2net 目录 → C4 的
/// 「空根 → ModelMissing」用例假失败；反过来不带真模型变量时，engine-segment 的
/// registry 用例又被同二进制的写根用例带偏）。
///
/// 契约：**任何改变模型根的代码（测试为主）必须先取本锁**。锁只有一把、放 L0、
/// 跨 crate 共享 —— 这样「同时改 segment 根 + restore 根」的用例不会出现两把锁的
/// 加锁顺序死锁（两把锁的方案要求所有用例遵守同一顺序，正是本卡要消灭的「靠自觉」）。
///
/// 生产路径（桌面壳启动时注入一次、dev-e2e 的 `main`）不取锁：单写者、发生在起线程之前。
/// 测试里首选各 engine crate 的 `ModelsRootScope`（取锁 + drop 还原），
/// 需要一次同时改两个引擎的根时才直接用本函数（`ModelsRootScope` 不嵌套，见其文档）。
pub fn models_root_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// 内置 registry 文本（编译期内嵌，随安装包分发；运行时缺文件时兜底）。
pub const BUILTIN_REGISTRY_YAML: &str = include_str!("../../../registry/models.yaml");

/// 解析 registry：优先运行时 registry/models.yaml（开发态可改），缺文件时回退内嵌。
pub fn load_registry(registry_dir: Option<&std::path::Path>) -> Result<Registry> {
    if let Some(dir) = registry_dir {
        let p = dir.join("models.yaml");
        if p.is_file() {
            return Registry::from_file(&p);
        }
    }
    Registry::from_yaml_str(BUILTIN_REGISTRY_YAML)
}

pub mod packs;

// C8：装载期 schema lint（未知键/结构）—— L0 只做 schema 层，注册表层在 L2
pub mod lint;

// C2 起 pack 类型被编排器/报告导出/后续 C 链消费：顶层重导出（packs::* 为准）
pub use packs::{
    Check, ConfidenceSource, ConfidenceSpec, FixerTier, GroupBy, GroupSource, JudgementScope, Pack,
    Rule, RuleEvidence, RuleExecutor, RuleOnFail, StepKind,
};

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
models:
  - name: siglip2-base-p224-onnx
    kind: embedding
    dims: 768
    modalities: [image, text]
    provider: onnx_siglip
    preprocess:
      image_size: 224
      mean: [0.5, 0.5, 0.5]
      std: [0.5, 0.5, 0.5]
  - name: openai-compat-vl
    kind: tagging
    dims: 0
    provider: openai_compat
"#;

    #[test]
    fn parses_registry_and_looks_up() {
        let reg = Registry::from_yaml_str(SAMPLE).unwrap();
        assert_eq!(reg.models.len(), 2);
        let m = reg.by_name("siglip2-base-p224-onnx").unwrap();
        assert_eq!(m.dims, 768);
        assert!(m.normalized);
        assert_eq!(m.kind, SpecKind::Embedding);
        assert!(reg.by_name("nope").is_none());
    }

    // ---------- max_tokens（卡 task：1024 截断 → 生效值可见；2026-10-20 裁决统一上限 4096） ----------

    #[test]
    fn tag_settings_max_tokens_default_and_source() {
        let d = TagSettings::default();
        assert_eq!(TagSettings::DEFAULT_MAX_TOKENS, 4096);
        assert_eq!(d.max_tokens, 4096);
        assert_eq!(d.effective_max_tokens(), 4096);
        assert_eq!(d.max_tokens_source(), "default");
        assert_eq!(d.max_tokens_label(), "4096（默认）");

        // 0 = 未设置 → 用默认值（老 settings 行/手填 0 都不会发出 max_tokens=0）
        let zero = TagSettings {
            max_tokens: 0,
            ..Default::default()
        };
        assert_eq!(zero.effective_max_tokens(), 4096);
        assert_eq!(zero.max_tokens_source(), "default");

        // 已存的老配置（1024）：保留用户值，但标记为「设置页覆盖」并在错误文案里显示
        let old = TagSettings {
            max_tokens: 1024,
            ..Default::default()
        };
        assert_eq!(old.effective_max_tokens(), 1024);
        assert_eq!(old.max_tokens_source(), "settings");
        assert_eq!(old.max_tokens_label(), "1024（设置页覆盖）");
    }

    #[test]
    fn tag_settings_json_backward_compatible() {
        // 缺 max_tokens 字段的老形状 → serde(default) 兜到 4096
        let missing: TagSettings = serde_json::from_str(
            r#"{"enabled":true,"provider":"deepseek","base_url":"https://api.deepseek.com/v1",
                "api_key":"k","model":"deepseek-flash","prompt_template":"p","timeout_ms":60000}"#,
        )
        .unwrap();
        assert_eq!(missing.effective_max_tokens(), 4096);

        // 存量的 1024 原样读回（不做静默篡改用户设置）
        let stored: TagSettings = serde_json::from_str(
            r#"{"enabled":true,"provider":"deepseek","base_url":"https://api.deepseek.com/v1",
                "api_key":"k","model":"deepseek-flash","prompt_template":"p","max_tokens":1024,
                "timeout_ms":60000}"#,
        )
        .unwrap();
        assert_eq!(stored.max_tokens, 1024);
        assert_eq!(stored.max_tokens_source(), "settings");
    }

    #[test]
    fn builtin_registry_has_u2net_segmentation() {
        let reg = Registry::from_yaml_str(BUILTIN_REGISTRY_YAML).unwrap();
        let m = reg.by_name("u2net").expect("u2net in registry");
        assert_eq!(m.kind, SpecKind::Segmentation);
        assert_eq!(m.provider, "onnx_u2net");
        assert_eq!(m.dims, 0);
        assert!(m.modalities.contains(&"image".to_string()));
        // 预处理口径锁定（rembg/u2net 官方）：320² + ImageNet mean/std
        assert_eq!(m.preprocess.get("image_size").and_then(|v| v.as_u64()), Some(320));
    }

    #[test]
    fn builtin_registry_has_c4_restore_models() {
        let reg = Registry::from_yaml_str(BUILTIN_REGISTRY_YAML).unwrap();
        let sr = reg.by_name("realesr-general-x4v3").expect("x4v3 in registry");
        assert_eq!(sr.kind, SpecKind::SuperResolution);
        assert_eq!(sr.provider, "onnx_realesrgan");
        assert_eq!(sr.dims, 0);
        let lama = reg.by_name("lama-fp32").expect("lama in registry");
        assert_eq!(lama.kind, SpecKind::Inpainting);
        assert_eq!(lama.provider, "onnx_lama");
        assert_eq!(lama.dims, 0);
    }

    #[test]
    fn builtin_registry_has_ocr_models() {
        let reg = Registry::from_yaml_str(BUILTIN_REGISTRY_YAML).unwrap();
        let det = reg.by_name("ppocrv6-medium-det").expect("det in registry");
        assert_eq!(det.kind, SpecKind::Ocr);
        assert_eq!(det.provider, "onnx_ppocr_det");
        assert_eq!(det.dims, 0);
        assert!(det.modalities.contains(&"image".to_string()));
        let rec = reg.by_name("ppocrv6-medium-rec").expect("rec in registry");
        assert_eq!(rec.kind, SpecKind::Ocr);
        assert_eq!(rec.provider, "onnx_ppocr_rec");
        // rec 预处理口径锁定（PaddleX OCRReisizeNormImg）：高 48、基准宽 320、最大 3200
        assert_eq!(rec.preprocess.get("image_height").and_then(|v| v.as_u64()), Some(48));
        assert_eq!(rec.preprocess.get("max_width").and_then(|v| v.as_u64()), Some(3200));
    }

    #[test]
    fn builtin_registry_has_default_embedder_and_tagger() {
        let reg = Registry::from_yaml_str(BUILTIN_REGISTRY_YAML).unwrap();
        let m = reg
            .by_name(DEFAULT_EMBEDDER)
            .expect("default embedder in registry");
        assert_eq!(m.dims, 768);
        assert_eq!(m.provider, "onnx_siglip");
        assert!(m.modalities.contains(&"image".to_string()));
        assert!(m.modalities.contains(&"text".to_string()));
        assert!(m.normalized);
        assert!(reg.models.iter().any(|x| x.kind == SpecKind::Tagging));
        // D5 双嵌入器：去重塔在 registry，image-only，384 维
        let d = reg
            .by_name(DEDUP_EMBEDDER)
            .expect("dedup embedder in registry");
        assert_eq!(d.dims, 384);
        assert_eq!(d.provider, "onnx_dinov3");
        assert!(d.modalities.contains(&"image".to_string()));
        assert!(!d.modalities.contains(&"text".to_string()));
        assert!(d.normalized);
    }

    #[test]
    fn library_settings_effective_root() {
        let app = std::path::Path::new("/tmp/appdata");
        let s = LibrarySettings::default();
        assert_eq!(
            s.effective_root(app),
            std::path::PathBuf::from("/tmp/appdata/library")
        );
        let s = LibrarySettings {
            root: Some("  ".into()),
        };
        assert_eq!(
            s.effective_root(app),
            std::path::PathBuf::from("/tmp/appdata/library")
        );
        let s = LibrarySettings {
            root: Some("/data/pics".into()),
        };
        assert_eq!(
            s.effective_root(app),
            std::path::PathBuf::from("/data/pics")
        );
        // 向前兼容：老 key 缺字段
        let legacy: LibrarySettings = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.root, None);
    }

    #[test]
    fn path_tag_settings_default_off_and_camelcase() {
        // D8 裁决：默认均 false（不开启）；json key 为 camelCase
        let d = PathTagSettings::default();
        assert_eq!(
            (d.use_dirs, d.use_filenames),
            (false, false),
            "D8 裁决 2026-10-13：默认均 false"
        );
        assert!(d.disabled());
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, r#"{"useDirs":false,"useFilenames":false}"#);
        // 老配置缺字段 → 默认兜底；roundtrip
        let legacy: PathTagSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy, d);
        let on = PathTagSettings {
            use_dirs: true,
            use_filenames: true,
        };
        assert!(!on.disabled());
        assert_eq!(
            serde_json::from_str::<PathTagSettings>(&serde_json::to_string(&on).unwrap()).unwrap(),
            on
        );
    }

    #[test]
    fn tag_provider_presets_table() {
        // D9 裁决：内置预设，id/base_url/模型候选齐全；WIN-PRE2 新增 deepseek
        assert_eq!(TAG_PROVIDER_PRESETS.len(), 6);
        // R7：dam-ai /v1/tagging 是自托管默认（表首位，web/容器形态不变）
        let dm = tag_provider_preset(TAG_PROVIDER_DAMAI).unwrap();
        assert_eq!(
            dm.base_url,
            "http://host.docker.internal:8091/v1"
        );
        assert!(dm.models.contains(&"qwen3.5-4b"));
        assert!(!dm.scan);
        assert_eq!(TAG_PROVIDER_PRESETS[0].id, TAG_PROVIDER_DAMAI);
        // WIN-PRE2：deepseek 公网端点（桌面形态默认），非容器口径
        let dp = tag_provider_preset(TAG_PROVIDER_DEEPSEEK).unwrap();
        assert_eq!(dp.base_url, "https://api.deepseek.com/v1");
        assert!(dp.models.contains(&"deepseek-flash"));
        assert!(!dp.scan);
        let ds = tag_provider_preset(TAG_PROVIDER_DASHSCOPE).unwrap();
        assert_eq!(
            ds.base_url,
            "https://dashscope.aliyuncs.com/compatible-mode/v1"
        );
        assert!(ds.models.contains(&"qwen3-vl-plus"));
        assert!(!ds.scan);
        let zp = tag_provider_preset(TAG_PROVIDER_ZHIPU).unwrap();
        assert_eq!(zp.base_url, "https://open.bigmodel.cn/api/paas/v4");
        assert!(zp.models.contains(&"glm-4.6v"));
        let ol = tag_provider_preset(TAG_PROVIDER_OLLAMA).unwrap();
        assert_eq!(ol.base_url, "http://127.0.0.1:11434/v1");
        assert!(ol.models.is_empty(), "ollama 模型走动态扫描，静态表为空");
        assert!(ol.scan);
        let cu = tag_provider_preset(TAG_PROVIDER_CUSTOM).unwrap();
        assert_eq!(cu.base_url, "");
        // id 唯一；未知 id → None（UI 按 custom 兜底）
        let mut ids: Vec<_> = TAG_PROVIDER_PRESETS.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), TAG_PROVIDER_PRESETS.len());
        assert!(tag_provider_preset("nope").is_none());
        // 预设表可序列化给 UI（tag_provider_presets 命令）
        let j = serde_json::to_string(TAG_PROVIDER_PRESETS).unwrap();
        assert!(j.contains("dashscope"));
    }

    #[test]
    fn tag_settings_provider_backward_compat() {
        // D9：新字段 provider；老配置（D3-D8 落库形状，无 provider）→ serde default 兜底 custom
        let legacy: TagSettings =
            serde_json::from_str(r#"{"enabled":true,"base_url":"http://x/v1","model":"m"}"#)
                .unwrap();
        assert_eq!(legacy.provider, TAG_PROVIDER_CUSTOM);
        assert!(legacy.configured());
        // roundtrip 保留 provider
        let mut s = TagSettings::default();
        s.provider = TAG_PROVIDER_OLLAMA.to_string();
        s.base_url = "http://127.0.0.1:11434/v1".into();
        s.model = "qwen2.5vl".into();
        let rt: TagSettings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(rt.provider, TAG_PROVIDER_OLLAMA);
        assert_eq!(rt.model, "qwen2.5vl");
    }

    #[test]
    fn settings_defaults_and_serde_roundtrip() {
        let s = InferenceSettings::default();
        assert_eq!(s.active_embedder, DEFAULT_EMBEDDER);
        assert!(s.model_base_url.starts_with("https://hf-mirror.com"));
        assert!(!s.tag.configured());
        let j = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<InferenceSettings>(&j).unwrap(), s);

        let mut t = TagSettings::default();
        t.base_url = "http://127.0.0.1:9/v1".into();
        t.model = "vl".into();
        assert!(t.configured());
        assert!(t.prompt_template.contains("title_ch"));
        // 向前兼容：老配置缺字段 → serde default 兜底
        let legacy: TagSettings = serde_json::from_str("{\"enabled\":true}").unwrap();
        assert_eq!(legacy.model, "");
    }

    #[test]
    fn model_paths_layout() {
        let root = std::path::Path::new("/tmp/appdata");
        assert_eq!(
            models_root(root),
            std::path::PathBuf::from("/tmp/appdata/models")
        );
        assert_eq!(
            model_dir(root, "siglip2-base-p224-onnx"),
            std::path::PathBuf::from("/tmp/appdata/models/siglip2-base-p224-onnx")
        );
    }

    #[test]
    fn load_registry_falls_back_to_builtin() {
        let reg = load_registry(Some(std::path::Path::new("/nonexistent-dir"))).unwrap();
        assert!(reg.by_name(DEFAULT_EMBEDDER).is_some());
        let reg2 = load_registry(None).unwrap();
        assert_eq!(reg2.models.len(), reg.models.len());
    }

}
