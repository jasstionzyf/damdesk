//! C5 — compliance `executor: vl` 通道（R3 水印判定 + R5 属性打标）。
//!
//! 设计（对齐 PLAN.md §6.3 C5 + pack-examples/ecommerce-taobao.md）：
//! - **逐资产合并调用**：同一资产的所有 vl 规则（judge + 打标）合成**一次** chat/completions
//!   调用（结果按规则拆分），把 2000 张 × 2 规则的调用数压到 2000 次；资产级缓存挂在
//!   编排器 AssetCtx 上（VlSession.round 惰性执行一次）。
//! - **送图铁律**：短边 512 归一复用 engine-tag::vl_image::load_for_vl（与打标同口径）。
//! - **修复闭环**：judge fail → inpaint（区域来自 VL 返回 bbox：单件 → box 参数；
//!   多件 → 合成白=待补洞掩码 PNG）→ 对修复产物**重新**调用 VL 复检（仅该规则）。
//! - **降级语义**：未配置 VlConfig → verdict skipped(executor_not_available)（v1 契约同形，
//!   不静默判合格）；HTTP/解析失败 → 返回 Err（job 级重试，attempts 耗尽转 failed，响亮）。
//! - 打标型规则（outputs 非空）on_fail=pass：tags 缺失 → skipped(tags_missing)，不影响合格结论；
//!   词表外取值丢弃并记入 evidence.rejected（可审计）。
//! - 客户端复用 engine-tag::http（ureq 阻塞式，代理取 env，`Some("")` 显式直连）。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use engine_tag::http::{agent_with_proxy, image_data_url};
use serde::{Deserialize, Serialize};

/// VL 执行配置（桌面设置/驱动注入；未配置 = vl 规则 skipped(executor_not_available)）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VlConfig {
    /// OpenAI 兼容根（如 https://api.deepseek.com/v1）
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// 全局超时秒数（默认 90）
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// Some("") = 显式直连；None = env 代理（与 engine-tag::http 同语义）
    #[serde(default)]
    pub proxy: Option<String>,
}

fn default_timeout() -> u64 {
    90
}
/// VL 输出预算默认值。
///
/// `reasoning_content` 先吃预算：deepseek-flash 实测思考链 190~550 字符、本地
/// qwen3.5-4b 500~740 字符，900 在长思考时会把 `content` 挤成空串 → 下游误判成
/// 「VL 解析失败」。统一给 4096（上限口径），思考链长时也能留下正文；模型不吐
/// 这么多时不额外计费（按实际生成计）。
fn default_max_tokens() -> u32 {
    4096
}

/// 运行统计（e2e 报告用；线程安全）。
#[derive(Debug, Default)]
pub struct VlStats {
    pub calls: AtomicU64,
    pub http_errors: AtomicU64,
    /// K07-FIX-D：命中结果缓存而**没有**发出的调用数（`calls` 口径不变，仅此计数）
    pub cache_hits: AtomicU64,
    pub parse_failures: AtomicU64,
    pub retries: AtomicU64,
    pub ms_total: AtomicU64,
}

/// 跨线程共享句柄（编排器 per-job 克隆；stats 共享累计）。
#[derive(Clone)]
pub struct VlHandle {
    pub cfg: VlConfig,
    pub stats: Arc<VlStats>,
}

/// vl 通道检测器注册表（C8 起；C9 加组级档）。
///
/// C7 缺口 G3：`RuleExecutor::Vl` 之前**只看 executor**，`check.detector` 写什么都被忽略
///（`detector: vl_set_judge` 这种不存在的键装载通过、跑起来不报错）；`params` 里的未知键
/// 也被静默忽略。C8 起：detector 必须是本注册表里的键，params 必须是该键声明的键集合。
///
/// C9 起每个条目带 `scope`：`vl_judge` 只服务 per_asset 规则，`vl_group_judge` 只服务
/// per_group 规则 —— 作用域与检测器不匹配 = 装载期 lint 报错（不许「配了但不生效」）。
/// 打标型规则不写 `check`（词表由 `outputs` + taxonomy 驱动），不查这里。
pub struct VlDetectorEntry {
    pub id: &'static str,
    pub name: &'static str,
    /// 该检测器服务的判定作用域
    pub scope: core_config::JudgementScope,
}

pub static VL_DETECTORS: &[VlDetectorEntry] = &[
    VlDetectorEntry {
        id: "vl_judge",
        name: "远程语义判定（逐图 judge 型：prompt/expect）",
        scope: core_config::JudgementScope::PerAsset,
    },
    VlDetectorEntry {
        id: "vl_group_judge",
        name: "远程语义判定（组级：逐图归类 + 组内汇总，prompt/options/require_all）",
        scope: core_config::JudgementScope::PerGroup,
    },
];

/// 查表（id → 条目）；未知 id → None（装载期 lint 报「未知 vl 检测器」）。
pub fn vl_lookup(id: &str) -> Option<&'static VlDetectorEntry> {
    VL_DETECTORS.iter().find(|e| e.id == id)
}

/// 已知 vl 检测器 id 列表（错误信息用）。
pub fn vl_known_ids() -> String {
    VL_DETECTORS
        .iter()
        .map(|e| e.id)
        .collect::<Vec<_>>()
        .join(", ")
}

/// judge 型 vl 规则的 `check.params` schema（C8）：
/// `prompt` 必填（业务措辞**由 pack 提供**），`expect` 缺省「否」。
/// `deny_unknown_fields`：未知键 = 参数错误（不再是「写了没反应」）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VlJudgeParams {
    pub prompt: String,
    #[serde(default = "default_expect")]
    pub expect: String,
    /// COMPLIANCE-DOMAIN：**答案词表模式**（如 `[是, 否]`）。
    ///
    /// 为什么不复用 `violated` 布尔：实测（qwen3-vl-plus，2026-10-17）正向提问 + `violated`
    /// 布尔会被模型读成「违反/有问题」，非房源图也答 true —— 合格与不合格直接反号。
    /// 词表模式下提问改成「answer 只能原词回答其中之一」，解析走词表校验，
    /// `pass = (answer == expect)`，不再依赖模型对布尔键语义的理解。
    ///
    /// 空 = 历史布尔模式（`violated` true/false，电商 R3 等既有规则照旧）。
    #[serde(default)]
    pub options: Vec<String>,
}

fn default_expect() -> String {
    "否".to_string()
}

/// 解析 judge 型 vl 规则的 params（错误 → Err(人读消息)，装载期 lint 与运行时共用）。
pub fn vl_judge_params(params: &serde_yaml::Value) -> std::result::Result<VlJudgeParams, String> {
    let normalized = match params {
        serde_yaml::Value::Mapping(_) => params.clone(),
        _ => serde_yaml::Value::Mapping(Default::default()),
    };
    let p: VlJudgeParams = serde_yaml::from_value(normalized).map_err(|e| e.to_string())?;
    if !p.options.is_empty() {
        if p.options.len() < 2 {
            return Err("options 至少两个取值（二元判定如 [是, 否]）".into());
        }
        if !p.options.iter().any(|o| o.trim() == p.expect.trim()) {
            return Err(format!(
                "expect={:?} 不在 options {:?} 里（answer 词表模式要求 expect 是词表之一）",
                p.expect, p.options
            ));
        }
    }
    Ok(p)
}

// ---------- C9：组级 vl 判定（vl_group_judge） ----------

/// 组级 vl 规则的 `check.params` schema（C9）：
/// - `prompt` 必填：**逐图**提问（业务措辞由 pack 提供）；
/// - `options` 必填：可接受的答案词表（模型答词表外的取值 → 记 `offList` 并丢弃，不静默当命中）；
/// - `require_all` 必填：组内必须齐备的选项（组级判定对象）；
/// - `probe_max` 缺省 0 = 组内全部成员都提问（>0 = 按 assetId 升序最多问这么多张，
///   未提问成员记入证据 `unprobed`，**不静默**——大库抽样是显式语义，不是偷偷少判）。
///
/// `deny_unknown_fields`：未知键 = 参数错误（承接 C8 口径）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VlGroupJudgeParams {
    pub prompt: String,
    pub options: Vec<String>,
    pub require_all: Vec<String>,
    #[serde(default)]
    pub probe_max: u32,
}

/// 解析组级 vl 规则的 params（装载期 lint 与运行时共用）。
pub fn vl_group_params(
    params: &serde_yaml::Value,
) -> std::result::Result<VlGroupJudgeParams, String> {
    let normalized = match params {
        serde_yaml::Value::Mapping(_) => params.clone(),
        _ => serde_yaml::Value::Mapping(Default::default()),
    };
    let p: VlGroupJudgeParams = serde_yaml::from_value(normalized).map_err(|e| e.to_string())?;
    if p.options.is_empty() {
        return Err("options 不能为空（组级判定要有可接受的答案词表）".into());
    }
    if p.require_all.is_empty() {
        return Err("require_all 不能为空（组级判定要有必须齐备的选项）".into());
    }
    for r in &p.require_all {
        if !p.options.contains(r) {
            return Err(format!("require_all 里的 {r:?} 不在 options 里"));
        }
    }
    Ok(p)
}

/// 组级 vl 判定提问（来自 rule.check.params）。
#[derive(Debug, Clone)]
pub struct VlGroupAsk {
    pub rule_id: String,
    pub prompt: String,
    pub options: Vec<String>,
    pub require_all: Vec<String>,
    pub probe_max: u32,
}

/// 从 pack 静态推导**组级** vl 提问（per_group + executor vl + vl_group_judge）。
/// 未知检测器/非法参数 → 跳过（装载期已报错，编排器落 load_error，不静默变成正常提问）。
pub fn group_asks_from_pack(pack: &core_config::Pack) -> Vec<VlGroupAsk> {
    use core_config::RuleExecutor;
    let mut out = Vec::new();
    for rule in &pack.rules {
        if rule.executor != RuleExecutor::Vl || !rule.is_group_scoped() {
            continue;
        }
        let Some(check) = &rule.check else { continue };
        if vl_lookup(&check.detector).map(|e| e.scope)
            != Some(core_config::JudgementScope::PerGroup)
        {
            continue;
        }
        let Ok(p) = vl_group_params(&check.params) else {
            continue;
        };
        out.push(VlGroupAsk {
            rule_id: rule.id.clone(),
            prompt: p.prompt,
            options: p.options,
            require_all: p.require_all,
            probe_max: p.probe_max,
        });
    }
    out
}

/// 组级提问的 prompt（逐图问 + 要求只回一个词；答案词表内联，便于用词表校验兜底）。
pub fn build_group_probe_prompt(intro: &str, ask: &VlGroupAsk) -> String {
    let opts = ask.options.join(" / ");
    format!(
        "{intro}\n\n\
         当前任务：对**这一张图**做归类，供「整组是否齐备」的组级判定汇总（同一组的多张图会分别提问）。\n\
         问题：{}\n\
         可选答案（必须原词回答，不要解释）：{opts}\n\
         只输出 JSON：{{\"label\": \"<上面选项之一>\"}}",
        ask.prompt
    )
}

/// 解析逐图归类答案：JSON（`{"label": "..."}` 或 `{"rules":{"<rule_id>":{"label":"..."}}}`）
/// → 词表直接命中；都拿不到则退化为**在整段文本里按词表序扫子串**（模型爱加解释）；
/// 仍未命中 → Err（上层重试/响亮失败，绝不猜一个词当结论）。
pub fn parse_group_label(
    raw: &str,
    rule_id: &str,
    options: &[String],
) -> std::result::Result<String, VlError> {
    let pick = |s: &str| -> Option<String> {
        let s = s.trim();
        options
            .iter()
            .find(|o| o.trim() == s)
            .cloned()
            .or_else(|| options.iter().find(|o| s.contains(o.as_str())).cloned())
    };
    if let Ok(js) = extract_json(raw) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(js) {
            let candidates = [
                v["rules"][rule_id]["label"].as_str(),
                v["label"].as_str(),
                v["rules"][rule_id].as_str(),
                v["answer"].as_str(),
            ];
            for c in candidates.into_iter().flatten() {
                if let Some(hit) = pick(c) {
                    return Ok(hit);
                }
            }
        }
    }
    if let Some(hit) = pick(raw) {
        return Ok(hit);
    }
    Err(VlError::Parse(format!(
        "组级归类答案不在词表内（rule {rule_id}，词表: {options:?}，原始: {}）",
        truncate(raw, 200)
    )))
}

/// vl prompt 前言（C8）：pack 可在 pipeline.yaml 写 `vl_intro` 覆盖（如房产包写「这是一套
/// 房源照片」），缺省 = 业务中性措辞。替代原先硬编码的「对这张**商品图**执行判定」
///（房产/汽车包也被叫商品图是业务中性红线）。
pub const DEFAULT_VL_INTRO: &str = "你是图片合规判定引擎，对这张图片执行判定。";

pub fn pack_vl_intro(pack: &core_config::Pack) -> String {
    pack.pipeline
        .vl_intro
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_VL_INTRO.to_string())
}

/// 从 pack 静态推导 VL 提问：judge 型取 check.params.prompt/expect；打标型取
/// outputs × taxonomy 受控选项（词表缺字段 → 该字段跳过）。
///
/// C8：judge 型规则先过 vl 检测器注册表 + params schema；**未知检测器/未知参数键的规则
/// 不进 ask**（该规则在装载期已被 lint 列成错误 → 编排器给它落 load_error 判定，
/// 不会静默变成一次「正常的」VL 提问）。
pub fn asks_from_pack(pack: &core_config::Pack) -> (Vec<VlJudgeAsk>, Vec<VlTagAsk>) {
    use core_config::RuleExecutor;
    let mut judge_asks = Vec::new();
    let mut tag_asks = Vec::new();
    for rule in &pack.rules {
        if rule.executor != RuleExecutor::Vl {
            continue;
        }
        // C9：per_group 的 vl 规则由**组级 job** 用 vl_group_judge 提问，不进资产级合并提问
        // （否则每张图都会带上一条组级问题，语义与调用量都错）
        if rule.is_group_scoped() {
            continue;
        }
        if rule.is_tagging() {
            let fields = rule
                .outputs
                .iter()
                .filter_map(|f| {
                    pack.taxonomy
                        .fields
                        .get(f)
                        .cloned()
                        .map(|opts| (f.clone(), opts))
                })
                .collect();
            tag_asks.push(VlTagAsk {
                rule_id: rule.id.clone(),
                fields,
            });
        } else if let Some(check) = &rule.check {
            if vl_lookup(&check.detector).is_none() {
                continue; // 未知 vl 检测器：装载期已报错，不发问
            }
            let Ok(props) = vl_judge_params(&check.params) else {
                continue; // params 不合法：装载期已报错，不发问
            };
            judge_asks.push(VlJudgeAsk {
                rule_id: rule.id.clone(),
                prompt: props.prompt,
                expect: props.expect,
                // COMPLIANCE-DOMAIN：pack 声明 vl 自评置信度 → 提问里额外要一个 confidence
                self_confidence: rule
                    .check
                    .as_ref()
                    .and_then(|c| c.confidence.as_ref())
                    .map(|c| c.source == core_config::ConfidenceSource::VlSelfReport)
                    .unwrap_or(false),
                options: props.options.clone(),
                // 只有局部重绘类修复器需要 VL 定位；其余规则不问 where/regions
                need_region: rule
                    .fixer
                    .as_ref()
                    .map(|f| f.plugin == "inpaint")
                    .unwrap_or(false),
            });
        }
    }
    (judge_asks, tag_asks)
}

impl VlStats {
    pub fn snapshot(&self) -> VlStatsSnapshot {
        VlStatsSnapshot {
            calls: self.calls.load(Ordering::Relaxed),
            http_errors: self.http_errors.load(Ordering::Relaxed),
            parse_failures: self.parse_failures.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            ms_total: self.ms_total.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct VlStatsSnapshot {
    pub calls: u64,
    pub http_errors: u64,
    pub parse_failures: u64,
    /// 进程内重试次数（传输/空应答/解析失败后重发；不改变「调用有统计」语义）
    pub retries: u64,
    pub ms_total: u64,
}

impl VlStatsSnapshot {
    /// 平均单次时延 ms（零调用 → 0）。
    pub fn avg_ms(&self) -> u64 {
        self.ms_total.checked_div(self.calls).unwrap_or(0)
    }
}

/// judge 型 vl 规则的提问（来自 rule.check.params.prompt/expect）。
#[derive(Debug, Clone)]
pub struct VlJudgeAsk {
    pub rule_id: String,
    pub prompt: String,
    /// 期望答案（"否" = 不应出现；"是" = 应出现）；pass = (violated == expect_yes)
    pub expect: String,
    /// COMPLIANCE-DOMAIN：这条规则要不要 VL **自评置信度**
    /// （pack 声明 `check.confidence: {source: vl_self_report}`）。
    /// 为 true 时提问里加 `confidence` 字段，解析进 [`VlJudgeOut::confidence`]。
    pub self_confidence: bool,
    /// COMPLIANCE-DOMAIN：答案词表（空 = 历史 violated 布尔模式）。
    /// 非空时提问要求 `answer` 原词回答、解析走词表校验（见 [`VlJudgeParams::options`]）。
    pub options: Vec<String>,
    /// 这条规则的判定**需要定位区域**吗（只有 inpaint 类修复器要用 bbox/方位格）。
    ///
    /// 为什么必须显式区分（实测教训）：提问里只要写了「where 填违规内容所在方位格，
    /// regions 给 bbox」，模型就会**为了填 where 而把 violated 判成 true**
    /// （非房源图也 true → 领域 gate 反号）。不定位的规则一律不问 where/regions。
    pub need_region: bool,
}

/// 打标型 vl 规则的提问（来自 rule.outputs × pack.taxonomy）。
#[derive(Debug, Clone)]
pub struct VlTagAsk {
    pub rule_id: String,
    /// (词表字段, 受控选项)
    pub fields: Vec<(String, Vec<String>)>,
}

/// 单规则 judge 结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct VlJudgeOut {
    pub violated: bool,
    /// 违规件 bbox（0-1 归一化，已钳制）；VL 只给方位格时由此兜底生成
    pub regions: Vec<[f64; 4]>,
    /// VL 报的 3×3 方位格（top_left/center/...；可空）
    pub where_: Option<String>,
    /// COMPLIANCE-DOMAIN：词表模式下的原词答案（`是`/`否`）；布尔模式为 None。
    /// `pass` 用 `answer == expect` 判，不再让模型去理解 `violated` 的布尔语义。
    pub answer: Option<String>,
    /// COMPLIANCE-DOMAIN：VL **自评置信度**（只有 pack 声明 `vl_self_report` 的规则才问；
    /// 模型没给/非法 → None，上层落 `undetermined` + `confidence_missing`，不造数）
    pub confidence: Option<f64>,
    /// 该规则的原始应答片段（可审计）
    pub raw: serde_json::Value,
}

impl VlJudgeOut {
    /// 可重绘区域：优先精确 bbox，缺则方位格中心块（粗定位兜底）。
    pub fn fix_regions(&self) -> Vec<[f64; 4]> {
        if !self.regions.is_empty() {
            return self.regions.clone();
        }
        self.where_
            .as_deref()
            .and_then(cell_box)
            .map(|b| vec![b])
            .unwrap_or_default()
    }
}

/// 一次合并调用的完整结果（资产级缓存）。
#[derive(Debug, Clone)]
pub struct VlRound {
    pub judge: BTreeMap<String, VlJudgeOut>,
    /// rule_id → 通过词表校验的取值（taxonomy 字段序）
    pub tags: BTreeMap<String, Vec<String>>,
    /// rule_id → 词表外被丢弃的取值（审计）
    pub tags_rejected: BTreeMap<String, Vec<String>>,
    pub raw: String,
}

#[derive(Debug, thiserror::Error)]
pub enum VlError {
    #[error("vl http: {0}")]
    Http(String),
    #[error("vl parse: {0}")]
    Parse(String),
    /// COMPLIANCE-CLOSE：**输入不可解码**（本地解码失败 + 模型 4xx 拒收）。
    ///
    /// 与 `Http` 的区别是语义而非措辞：`Http` = 通道抖动（重试可能好，job 级）；
    /// `Decode` = 这张图本身判不了（重试无意义，规则级确定性错误 → 三态口径落
    /// `undetermined` + `reason=rule_error`，不是 fail、也不把整张图的其余判定作废）。
    #[error("vl decode: {0}")]
    Decode(String),
}

// ---------------------------------------------------------------------------
// K07-FIX-D：VL 结果缓存（(资产内容 sha256, prompt sha256, model) → 原始正文）
// ---------------------------------------------------------------------------

/// 结果缓存的默认容量（条）。一条正文只有几十~几百字节，1024 条约几百 KB。
const DEFAULT_CACHE_CAP: usize = 1024;

struct CacheInner {
    map: HashMap<String, String>,
    order: VecDeque<String>,
}

/// 进程级 VL 结果缓存（K07-FIX-D）。
///
/// **为什么需要**：插件通道一次资产判定里，同一张图会被问不止一次 —— judge 首判一轮，
/// repair 第 1 轮复判的还是**同一个文件**（同一内容、同一 prompt、同一模型），
/// 外加「修复后复判」「仍 fail 的规则复核」。没有缓存时这些全是真金白银的重复调用
/// （K07 实测 plugin 25.80 s/张 = legacy 的 5.49×，VL 往返是主因）。
///
/// **key 的三段**（卡面口径 `(assetId, prompt hash, model)`）：
/// * 资产身份用**内容 sha256**而不是 `assetId`：修复产物是同一资产的**新文件**，
///   内容不同就该是新的判定（用 assetId 会把修复前的旧结论安到修复后的图上，直接错判）；
/// * `prompt` 取 sha256 —— prompt 变了必须重问（不同规则问的不是一回事）；
/// * `model` + `max_tokens` —— 换模型/改预算都会改结论，不能复用。
///
/// **只存成功正文**：HTTP/解析失败一律不缓存（抖动重试本来就该再发）。
/// 进程内共享（`global()`），所以跨 run 也命中；`DAM_VL_CACHE=0` 可整体关掉。
pub struct VlCache {
    enabled: bool,
    cap: usize,
    inner: Mutex<CacheInner>,
}

impl VlCache {
    /// 建一个缓存（容量 0 = 关闭）。单测用它做隔离，不污染进程级全局缓存。
    pub fn new(cap: usize) -> Self {
        Self {
            enabled: cap > 0,
            cap,
            inner: Mutex::new(CacheInner {
                map: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    /// 进程级共享缓存（容量取 `DAM_VL_CACHE_MAX`，默认 1024；`DAM_VL_CACHE=0` 关闭）。
    pub fn global() -> &'static VlCache {
        static GLOBAL: OnceLock<VlCache> = OnceLock::new();
        GLOBAL.get_or_init(|| {
            let off = std::env::var("DAM_VL_CACHE")
                .map(|v| matches!(v.trim(), "0" | "off" | "false" | "no"))
                .unwrap_or(false);
            let cap = std::env::var("DAM_VL_CACHE_MAX")
                .ok()
                .and_then(|s| s.trim().parse::<usize>().ok())
                .unwrap_or(DEFAULT_CACHE_CAP);
            Self::new(if off { 0 } else { cap })
        })
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("vl cache lock").map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 缓存键：资产内容 sha256 | prompt sha256 | model | max_tokens。
    ///
    /// 图读不到时返回 `None`（调用方照常发请求 —— 缓存不该因为读不到文件就把判定拖挂）。
    pub fn key(image: &Path, prompt: &str, model: &str, max_tokens: u32) -> Option<String> {
        let bytes = std::fs::read(image).ok()?;
        Some(format!(
            "{}|{}|{}|{}",
            core_hash::sha256_hex(&bytes),
            core_hash::sha256_hex(prompt.as_bytes()),
            model,
            max_tokens
        ))
    }

    /// 取命中（顺带把该项挪到队尾，保持 FIFO 淘汰的「新近使用不淘汰」语义）。
    pub fn get(&self, key: &str) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let mut inner = self.inner.lock().expect("vl cache lock");
        let hit = inner.map.get(key).cloned();
        if hit.is_some() {
            inner.order.retain(|k| k != key);
            inner.order.push_back(key.to_string());
        }
        hit
    }

    /// 写回（超容量时按最久未用淘汰）。
    pub fn put(&self, key: String, raw: String) {
        if !self.enabled {
            return;
        }
        let mut inner = self.inner.lock().expect("vl cache lock");
        if inner.map.insert(key.clone(), raw).is_none() {
            inner.order.push_back(key);
        }
        while inner.map.len() > self.cap {
            let Some(old) = inner.order.pop_front() else {
                break;
            };
            inner.map.remove(&old);
        }
    }

    /// 清空（单测/切换 provider 时用）。
    pub fn clear(&self) {
        let mut inner = self.inner.lock().expect("vl cache lock");
        inner.map.clear();
        inner.order.clear();
    }
}

/// 资产级 VL 会话：合并调用一次（round），复检按规则单独调用（judge_file）。
pub struct VlSession {
    cfg: VlConfig,
    judge_asks: Vec<VlJudgeAsk>,
    tag_asks: Vec<VlTagAsk>,
    /// C8：prompt 前言（由 pack 提供，见 [`pack_vl_intro`]）
    intro: String,
    agent: ureq::Agent,
    round: Option<VlRound>,
    pub stats: Arc<VlStats>,
}

impl VlSession {
    pub fn new(
        cfg: VlConfig,
        judge_asks: Vec<VlJudgeAsk>,
        tag_asks: Vec<VlTagAsk>,
        stats: Arc<VlStats>,
    ) -> Self {
        Self::with_intro(
            cfg,
            judge_asks,
            tag_asks,
            DEFAULT_VL_INTRO.to_string(),
            stats,
        )
    }

    /// C8：带前言构造（编排器按 pack 的 `vl_intro` 传入）。
    pub fn with_intro(
        cfg: VlConfig,
        judge_asks: Vec<VlJudgeAsk>,
        tag_asks: Vec<VlTagAsk>,
        intro: String,
        stats: Arc<VlStats>,
    ) -> Self {
        let agent = agent_with_proxy(cfg.timeout_secs, cfg.proxy.as_deref());
        Self {
            cfg,
            judge_asks,
            tag_asks,
            intro,
            agent,
            round: None,
            stats,
        }
    }

    /// 合并轮次（惰性：首个 vl 步骤触发一次调用，后续步骤读缓存）。
    pub fn round(&mut self, image: &Path) -> std::result::Result<&VlRound, VlError> {
        if self.round.is_none() {
            let prompt = build_prompt(&self.intro, &self.judge_asks, &self.tag_asks);
            let parsed = self.parse_with_retry(image, &prompt, |raw| {
                parse_round(raw, &self.judge_asks, &self.tag_asks)
            })?;
            self.round = Some(parsed);
        }
        Ok(self.round.as_ref().expect("just filled"))
    }

    /// 对某文件（修复产物）重新判定单条规则（复检；不走资产缓存）。
    pub fn judge_file(
        &self,
        image: &Path,
        rule_id: &str,
    ) -> std::result::Result<VlJudgeOut, VlError> {
        let ask = self
            .judge_asks
            .iter()
            .find(|a| a.rule_id == rule_id)
            .ok_or_else(|| VlError::Parse(format!("规则 {rule_id} 无 judge 提问")))?;
        let prompt = build_prompt(&self.intro, std::slice::from_ref(ask), &[]);
        self.parse_with_retry(image, &prompt, |raw| {
            let mut out = parse_round(raw, std::slice::from_ref(ask), &[])?
                .judge
                .remove(rule_id)
                .ok_or_else(|| VlError::Parse(format!("应答缺少规则 {rule_id} 判定")))?;
            out.raw = serde_json::json!({ "recheck": true, "detail": out.raw });
            Ok(out)
        })
    }

    /// C9：组级判定的**逐图归类**提问（每张图一次调用；答案词表校验）。
    ///
    /// 与 `round`（资产级合并提问）分开：组级规则的调用粒度是「组成员张数」，
    /// 而资产 job 完全不提组级问题（见 `asks_from_pack`）。抖动语义与既有通道一致：
    /// HTTP/空应答/词表外答案 → 进程内重试（`chat` 有界重试 + `parse_with_retry` 二次解析），
    /// 仍失败 → Err（job 级重试，attempts 耗尽转 failed，响亮）。
    pub fn probe_group_label(
        &self,
        image: &Path,
        ask: &VlGroupAsk,
    ) -> std::result::Result<String, VlError> {
        let prompt = build_group_probe_prompt(&self.intro, ask);
        let rule_id = ask.rule_id.clone();
        let options = ask.options.clone();
        self.parse_with_retry(image, &prompt, |raw| {
            parse_group_label(raw, &rule_id, &options)
        })
    }

    /// 调用 + 解析（解析失败二次重发）：模型偶发返回非 JSON（实测复检出现过），
    /// 单发失败会放大成 job failed / 转人工 —— 与传输抖动同样按「重试再判」处置。
    fn parse_with_retry<T>(
        &self,
        image: &Path,
        prompt: &str,
        parse: impl Fn(&str) -> std::result::Result<T, VlError>,
    ) -> std::result::Result<T, VlError> {
        let mut last: Option<VlError> = None;
        for attempt in 0..2u32 {
            let raw = self.chat(image, prompt)?;
            match parse(&raw) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    self.stats.parse_failures.fetch_add(1, Ordering::Relaxed);
                    if attempt == 0 {
                        self.stats.retries.fetch_add(1, Ordering::Relaxed);
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| VlError::Parse("vl parse 未执行".into())))
    }

    /// 用**调用方给的 prompt** 做一次原始提问，返回模型正文（不做任何解析）。
    ///
    /// IP-K05：行业包插件经能力网关（`pack-host::gateway` 的 `/vl/judge`）提问时，prompt 由
    /// 插件自己写（`ruleset.PROMPT_*`），宿主不能假定提问结构 —— 所以这里提供一条
    /// **绕过 ask 表**的原始通道，传输/重试/代理/错误分类仍复用同一份实现
    /// （不许为插件另写一套 HTTP：VL 通道的抖动语义必须全局一致）。
    pub fn ask_raw(
        &self,
        image: &Path,
        prompt: &str,
        max_tokens: u32,
    ) -> std::result::Result<String, VlError> {
        // K07-FIX-D：先查结果缓存（同一内容 + 同一 prompt + 同一模型 = 同一答案）。
        // 只缓存**成功正文**；缓存读写失败（图读不到）不影响正常发请求。
        let cache = VlCache::global();
        let key = if cache.enabled() {
            VlCache::key(image, prompt, &self.cfg.model, max_tokens)
        } else {
            None
        };
        if let Some(hit) = key.as_deref().and_then(|k| cache.get(k)) {
            self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(hit);
        }
        let raw = self.chat_with(image, prompt, max_tokens)?;
        if let Some(k) = key {
            cache.put(k, raw.clone());
        }
        Ok(raw)
    }

    fn chat(&self, image: &Path, prompt: &str) -> std::result::Result<String, VlError> {
        self.chat_with(image, prompt, self.cfg.max_tokens)
    }

    /// 一次**绕过结果缓存**的原始提问（K07-FIX-H 翻转保护用）。
    ///
    /// 为什么需要它：缓存键是 (内容, prompt, 模型, max_tokens)，重复采样如果都走缓存，
    /// 第二、三份必然等于第一份 —— 「3 次一致」就变成了自证。要真的看见模型的抖动，
    /// 复采样必须拿新样本；但请求本身仍走同一份传输/重试/错误分类实现。
    pub fn ask_raw_fresh(
        &self,
        image: &Path,
        prompt: &str,
        max_tokens: u32,
    ) -> std::result::Result<String, VlError> {
        self.chat_with(image, prompt, max_tokens)
    }

    fn chat_with(
        &self,
        image: &Path,
        prompt: &str,
        max_tokens: u32,
    ) -> std::result::Result<String, VlError> {
        // 有界重试：并发下 deepseek 偶发空应答/传输抖动（实测 100 张 × 10 worker 时 14/127 次），
        // 单发失败会放大成 job failed / 转人工。语义不变（同一张图、同一个提问），只把抖动吸收掉；
        // 重试次数单独计数（stats.retries），成功路径的 calls/http_errors 口径不变。
        let attempts: u32 = std::env::var("DAM_VL_CHAT_ATTEMPTS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3)
            .max(1);
        let mut last: Option<VlError> = None;
        for attempt in 0..attempts {
            if attempt > 0 {
                self.stats.retries.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(std::time::Duration::from_millis(
                    400u64 << (attempt.min(4) - 1),
                ));
            }
            self.stats.calls.fetch_add(1, Ordering::Relaxed);
            let t0 = std::time::Instant::now();
            let result = self.chat_inner(image, prompt, max_tokens);
            self.stats
                .ms_total
                .fetch_add(t0.elapsed().as_millis() as u64, Ordering::Relaxed);
            match result {
                Ok(s) => return Ok(s),
                Err(VlError::Http(m)) => {
                    self.stats.http_errors.fetch_add(1, Ordering::Relaxed);
                    last = Some(VlError::Http(m));
                }
                Err(VlError::Parse(m)) => {
                    self.stats.parse_failures.fetch_add(1, Ordering::Relaxed);
                    last = Some(VlError::Parse(m));
                }
                // 输入不可解码：重试同一张图不会变好 → 直接冒泡给上层做规则级隔离
                Err(VlError::Decode(m)) => return Err(VlError::Decode(m)),
            }
        }
        Err(last.unwrap_or_else(|| VlError::Http("vl chat 未执行".into())))
    }

    fn chat_inner(
        &self,
        image: &Path,
        prompt: &str,
        max_tokens: u32,
    ) -> std::result::Result<String, VlError> {
        let (bytes, mime) =
            engine_tag::vl_image::load_for_vl(image).map_err(|e| VlError::Http(e.to_string()))?;
        // 解码参数固定化（卡 task K07-FIX-I）：temperature=0 + top_p=1，
        // 同图同 prompt 的输出分布压到最窄；插件侧另有 3 次多数票兜底。
        let body = serde_json::json!({
            "model": self.cfg.model,
            "temperature": 0,
            "top_p": 1,
            "max_tokens": max_tokens,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "image_url", "image_url": { "url": image_data_url(&bytes, &mime) } },
                    { "type": "text", "text": prompt },
                ],
            }],
        });
        let url = format!(
            "{}/chat/completions",
            self.cfg.base_url.trim_end_matches('/')
        );
        let mut req = self.agent.post(&url);
        // 本地端点（vLLM / Ollama / 内网网关）不需要 key：**空 key 就不发 Authorization 头**。
        // 为什么必须判空而不是发 "Bearer "：vLLM 直接 502 `Illegal header value b'Bearer '`
        // （头部值只有方案名、没有凭据），本地部署会整条 VL 通道挂掉。
        let key = self.cfg.api_key.trim();
        if !key.is_empty() {
            req = req.header("Authorization", &format!("Bearer {key}"));
        }
        let mut resp = req
            .send_json(&body)
            .map_err(|e| VlError::Http(format!("{url}: {e}")))?;
        let status = resp.status().as_u16();
        let text: String = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| VlError::Http(e.to_string()))?;
        if !(200..300).contains(&status) {
            // COMPLIANCE-CLOSE：模型对**图片本身**回 4xx（unsupported image 等）且本地也解不出来
            // → 输入不可解码（规则级确定性错误，三态口径落 undetermined）；否则才当通道抖动。
            // 只在错误路径做解码探测，正常路径不额外解码。
            if matches!(status, 400 | 415 | 422) && core_media::vl_jpeg(image, 512).is_err() {
                return Err(VlError::Decode(format!(
                    "图片无法解码（本地解码失败 + 模型 http {status}）：{}",
                    truncate(&text, 200)
                )));
            }
            return Err(VlError::Http(format!(
                "http {status}: {}",
                truncate(&text, 400)
            )));
        }
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| VlError::Http(format!("应答非 JSON: {e}")))?;
        let content = v["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| VlError::Http(format!("应答缺 content: {}", truncate(&text, 400))))?
            .to_string();
        if content.trim().is_empty() {
            return Err(VlError::Http(
                "应答 content 为空（模型可能不支持该图格式）".into(),
            ));
        }
        Ok(content)
    }
}

/// 提取 JSON 对象：容忍 ```json 围栏与前后废话（括号配平取第一个完整对象；
/// 无配平对象时退回首 `{` 到末 `}` 区间，交给上层按截断处理）。
fn extract_json(text: &str) -> std::result::Result<&str, VlError> {
    let s = text.trim();
    // 先做括号配平扫描：qwen3.5-4b 实测会在 JSON 前后夹无标签闲话（闲话里还可能
    // 再出现 `{`/`}`），旧的 first-{-到-last-} 区间会被拉宽导致 serde trailing
    // characters。配平扫描精确取第一个完整对象；找不到（真截断）再退回旧区间。
    if let Some(bal) = engine_tag::first_balanced_json(s) {
        return Ok(bal);
    }
    let (b, e) = (
        s.find('{')
            .ok_or_else(|| VlError::Parse("无 JSON 对象".into()))?,
        s.rfind('}')
            .ok_or_else(|| VlError::Parse("无 JSON 对象".into()))?,
    );
    if b >= e {
        return Err(VlError::Parse("JSON 区间非法".into()));
    }
    Ok(&s[b..=e])
}

fn clamp_bbox(b: &[f64]) -> Option<[f64; 4]> {
    if b.len() != 4 {
        return None;
    }
    let mut v = [0.0f64; 4];
    for (i, x) in b.iter().enumerate() {
        if !x.is_finite() {
            return None;
        }
        v[i] = x.clamp(0.0, 1.0);
    }
    if v[2] < v[0] {
        v.swap(0, 2);
    }
    if v[3] < v[1] {
        v.swap(1, 3);
    }
    if v[2] - v[0] < 1e-4 || v[3] - v[1] < 1e-4 {
        return None; // 退化矩形，不可用于重绘
    }
    Some(v)
}

fn parse_violated(v: &serde_json::Value) -> Option<bool> {
    match v {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::String(s) => match s.trim() {
            "是" | "true" | "yes" | "1" => Some(true),
            "否" | "false" | "no" | "0" | "" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// VL 自评置信度：接受 0..1 的浮点或数字字符串（模型常写 `"0.9"` / `90` / `90%`）。
///
/// 语义边界（COMPLIANCE-DOMAIN）：
/// - `90` / `"90%"` 按百分制折算成 0.9（模型偶发用百分数，硬拒会让本该有把握的判定转未判定）；
/// - 越界/非数字/布尔 → None（不猜、不钳制成 1.0——上层落 `confidence_missing` 转未判定）。
fn parse_confidence(v: &serde_json::Value) -> Option<f64> {
    let raw = match v {
        serde_json::Value::Number(n) => n.as_f64()?,
        serde_json::Value::String(s) => {
            let t = s.trim().trim_end_matches('%');
            let percent = s.trim().ends_with('%');
            let n: f64 = t.parse().ok()?;
            if percent {
                n / 100.0
            } else {
                n
            }
        }
        _ => return None,
    };
    if !raw.is_finite() {
        return None;
    }
    // 1 < v ≤ 100 → 视为百分制（模型写 90 而不是 0.9）
    let v01 = if raw > 1.0 && raw <= 100.0 {
        raw / 100.0
    } else {
        raw
    };
    if (0.0..=1.0).contains(&v01) {
        Some(v01)
    } else {
        None
    }
}

/// 3×3 方位格 → 归一化 bbox（VL 报不精 bbox 时的粗定位兜底）。
///
/// 只给方位格说明 VL 定位很粗，故按**整条带**取：上/中/下 → 整宽 × 1/3 高；左/右 → 1/3 宽 × 整高。
/// 带内缩 1% 防止退化零面积。理由：促销文字常横跨整行，只取 1/3 格的窄框会漏掉大半文字
/// （实测注水图 inpaint 残留 → VL 复检仍判违规）。
fn cell_box(where_: &str) -> Option<[f64; 4]> {
    let (ix, iy) = (0.01f64, 0.01f64);
    let w = where_.trim().to_ascii_lowercase().replace('-', "_");
    let box_ = |x0: f64, y0: f64, x1: f64, y1: f64| -> [f64; 4] {
        [
            (x0 + ix).min(x1),
            (y0 + iy).min(y1),
            (x1 - ix).max(x0 + 1e-3),
            (y1 - iy).max(y0 + 1e-3),
        ]
    };
    Some(match w.as_str() {
        "top_left" | "top_center" | "top_right" | "top" => box_(0.0, 0.0, 1.0, 1.0 / 3.0),
        "mid_left" | "center_left" | "left" => box_(0.0, 0.0, 1.0 / 3.0, 1.0),
        "center" | "middle" | "mid_center" => box_(0.0, 1.0 / 3.0, 1.0, 2.0 / 3.0),
        "mid_right" | "center_right" | "right" => box_(2.0 / 3.0, 0.0, 1.0, 1.0),
        "bottom_left" | "bottom_center" | "bottom_right" | "bottom" => {
            box_(0.0, 2.0 / 3.0, 1.0, 1.0)
        }
        _ => return None,
    })
}

fn parse_round(
    content: &str,
    judge_asks: &[VlJudgeAsk],
    tag_asks: &[VlTagAsk],
) -> std::result::Result<VlRound, VlError> {
    let json = extract_json(content)?;
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| VlError::Parse(format!("{e}: {}", truncate(content, 300))))?;
    let rules = v
        .get("rules")
        .and_then(|r| r.as_object())
        .ok_or_else(|| VlError::Parse(format!("应答缺 rules: {}", truncate(content, 300))))?;

    let mut judge = BTreeMap::new();
    for ask in judge_asks {
        let entry = rules
            .get(&ask.rule_id)
            .ok_or_else(|| VlError::Parse(format!("应答缺规则 {} 判定", ask.rule_id)))?;
        // 两种模式：
        // - 词表模式（ask.options 非空）：读 `answer`（兼容 label/result/字符串形式的 violated），
        //   词表校验后 `violated = (answer != expect)`；
        // - 布尔模式（历史）：读 `violated` true/false。
        let (answer, violated) = if ask.options.is_empty() {
            let v = parse_violated(entry.get("violated").unwrap_or(&serde_json::Value::Null))
                .ok_or_else(|| VlError::Parse(format!("规则 {} violated 非法", ask.rule_id)))?;
            (None, v)
        } else {
            let raw = ["answer", "label", "result", "violated"]
                .iter()
                .find_map(|k| entry.get(*k).and_then(|v| v.as_str()))
                .ok_or_else(|| {
                    VlError::Parse(format!(
                        "规则 {} 缺 answer 字段（词表模式必须原词回答 {:?}）: {}",
                        ask.rule_id,
                        ask.options,
                        truncate(&entry.to_string(), 200)
                    ))
                })?;
            let hit = raw.trim();
            // 词表命中（原词优先；容忍「是。」这类带标点的回答）
            let picked = ask
                .options
                .iter()
                .find(|o| o.trim() == hit)
                .cloned()
                .or_else(|| {
                    ask.options
                        .iter()
                        .find(|o| hit.starts_with(o.trim()))
                        .cloned()
                })
                .ok_or_else(|| {
                    VlError::Parse(format!(
                        "规则 {} 的 answer={:?} 不在词表 {:?} 内（不静默当命中）",
                        ask.rule_id, hit, ask.options
                    ))
                })?;
            // 与 `violated=true 表示违反该规则` 同口径：答案 ≠ 期望 = 问题成立
            let violated = picked.trim() != ask.expect.trim();
            (Some(picked), violated)
        };
        let mut regions = Vec::new();
        if let Some(rs) = entry.get("regions").and_then(|r| r.as_array()) {
            for r in rs {
                if let Some(b) = r.get("bbox").and_then(|b| b.as_array()) {
                    let nums: Vec<f64> = b.iter().filter_map(|x| x.as_f64()).collect();
                    if nums.len() == b.len() {
                        if let Some(bb) = clamp_bbox(&nums) {
                            regions.push(bb);
                        }
                    }
                }
            }
        }
        judge.insert(
            ask.rule_id.clone(),
            VlJudgeOut {
                violated,
                answer,
                regions,
                where_: entry
                    .get("where")
                    .and_then(|w| w.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                // COMPLIANCE-DOMAIN：只给声明了自评置信度的规则解析（其余规则）
                // 不看这个字段——不把模型顺手多写的数字当成置信度来源
                confidence: if ask.self_confidence {
                    entry.get("confidence").and_then(parse_confidence)
                } else {
                    None
                },
                raw: entry.clone(),
            },
        );
    }

    let mut tags = BTreeMap::new();
    let mut tags_rejected = BTreeMap::new();
    let tags_obj = v.get("tags").and_then(|t| t.as_object());
    for ask in tag_asks {
        let mut ok = Vec::new();
        let mut bad = Vec::new();
        for (field, options) in &ask.fields {
            let val = tags_obj
                .and_then(|t| t.get(field.as_str()))
                .and_then(|x| x.as_str())
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            if val.is_empty() {
                continue;
            }
            if options.iter().any(|o| o == &val) {
                ok.push(val);
            } else {
                bad.push(format!("{field}:{val}"));
            }
        }
        tags.insert(ask.rule_id.clone(), ok);
        tags_rejected.insert(ask.rule_id.clone(), bad);
    }

    Ok(VlRound {
        judge,
        tags,
        tags_rejected,
        raw: content.to_string(),
    })
}

/// 合并 prompt（judge + 打标一次问全；只许输出一个 JSON 对象）。
///
/// C8：`intro` = prompt 前言（pack 提供或引擎内置中性措辞，见 [`pack_vl_intro`]）——
/// 业务措辞不再硬编码在引擎里。
pub fn build_prompt(intro: &str, judge_asks: &[VlJudgeAsk], tag_asks: &[VlTagAsk]) -> String {
    // 提问只问**用得上的字段**（实测教训，2026-10-17）：
    // - `where`/`regions` 只在有规则需要定位（inpaint 类修复器）时才问：只要问方位格，模型就会
    //   「为了填 where 而把 violated 判成 true」，正向 gate 直接反号（狗/薯条被判成房源图）；
    // - 词表模式（`options`）的规则只问 `answer`（原词回答）+ 可选 `confidence`：
    //   布尔键 `violated` 在正向提问下语义会被模型读成「违反/有问题」，同样反号。
    let need_region = judge_asks.iter().any(|a| a.need_region);
    let mut p = format!("{intro}只输出一个 JSON 对象，禁止输出任何其他文字、解释或代码围栏。\n");
    if need_region {
        p.push_str(
            "一、规则判定（violated=true 表示违反该规则；\
             where 填违规内容所在方位格（top_left/top_center/top_right/mid_left/center/mid_right/\
             bottom_left/bottom_center/bottom_right，无违规给 \"\"）；regions 可选，若知道精确位置再给 \
             bbox=[x0,y0,x1,y1]（0-1 归一化相对坐标），无违规则 regions=[]）：\n",
        );
    } else {
        p.push_str("一、规则判定（violated=true 表示违反该规则；无违规则为 false）：\n");
    }
    for (i, a) in judge_asks.iter().enumerate() {
        if a.options.is_empty() {
            p.push_str(&format!(
                "{}. key=\"{}\"：{}（期望：{}）\n",
                i + 1,
                a.rule_id,
                a.prompt,
                if a.expect == "是" {
                    "应存在"
                } else {
                    "不应存在"
                }
            ));
        } else {
            // 词表模式：原词回答，answer 只能取词表之一（pass = answer == expect）
            p.push_str(&format!(
                "{}. key=\"{}\"：{}  —— answer 只能原样回答一个词（{}），不要解释；期望答案：{}\n",
                i + 1,
                a.rule_id,
                a.prompt,
                a.options.join(" / "),
                a.expect
            ));
        }
        if a.self_confidence {
            p.push_str(
                "   这条还要给 confidence：0 到 1 之间的小数，按你的真实把握给（很确定 0.9 以上；\
                 模糊/看不清就给低分），不要照抄示例数值\n",
            );
        }
    }
    if !tag_asks.is_empty() {
        p.push_str("二、属性打标（每个字段从选项中选最匹配的一个值，无法判断给 \"\"）：\n");
        for a in tag_asks {
            for (field, options) in &a.fields {
                p.push_str(&format!("key=\"{field}\"，选项：{options:?}\n"));
            }
        }
    }
    let mut example = serde_json::Map::new();
    let mut ex_rules = serde_json::Map::new();
    for a in judge_asks {
        let mut ex = if a.options.is_empty() {
            let mut m = serde_json::json!({ "violated": false });
            if need_region && a.need_region {
                m["where"] = serde_json::json!("");
                m["regions"] = serde_json::json!([]);
            }
            m
        } else {
            serde_json::json!({ "answer": a.options.first().cloned().unwrap_or_default() })
        };
        if a.self_confidence {
            ex["confidence"] = serde_json::json!(0.9);
        }
        ex_rules.insert(a.rule_id.clone(), ex);
    }
    let mut ex_tags = serde_json::Map::new();
    for a in tag_asks {
        for (field, _) in &a.fields {
            ex_tags.insert(field.clone(), serde_json::Value::String(String::new()));
        }
    }
    example.insert("rules".into(), serde_json::Value::Object(ex_rules));
    if !ex_tags.is_empty() {
        example.insert("tags".into(), serde_json::Value::Object(ex_tags));
    }
    p.push_str(&format!(
        "输出 JSON 结构（键名必须完全一致）：\n{}",
        serde_json::Value::Object(example)
    ));
    p
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

// ---------- 测试：prompt 组装 / 宽容解析 / bbox 钳制 / pass 语义 ----------

#[cfg(test)]
mod tests {
    use super::*;

    fn judge_ask() -> VlJudgeAsk {
        VlJudgeAsk {
            rule_id: "R3_no_watermark".into(),
            prompt: "是否存在促销文字、水印、logo 遮挡".into(),
            expect: "否".into(),
            self_confidence: false,
            options: Vec::new(),
            need_region: false,
        }
    }

    /// COMPLIANCE-DOMAIN：正向 gate 提问（expect=是 + VL 自评置信度）
    fn gate_ask() -> VlJudgeAsk {
        VlJudgeAsk {
            rule_id: "F0_domain".into(),
            prompt: "这张图是不是房源/室内实景照片".into(),
            expect: "是".into(),
            self_confidence: true,
            options: vec!["是".into(), "否".into()],
            need_region: false,
        }
    }

    fn tag_ask() -> VlTagAsk {
        VlTagAsk {
            rule_id: "R5_attribute".into(),
            fields: vec![
                ("dress_silhouette".into(), vec!["A字".into(), "直筒".into()]),
                ("scene".into(), vec!["平铺".into(), "白底图".into()]),
            ],
        }
    }

    #[test]
    fn prompt_contains_keys_and_example() {
        let p = build_prompt(DEFAULT_VL_INTRO, &[judge_ask()], &[tag_ask()]);
        assert!(p.contains("R3_no_watermark"));
        assert!(p.contains("dress_silhouette"));
        assert!(p.contains("只输出一个 JSON 对象"));
        // C8：前言由调用方给（pack 可覆盖），且不再硬编码行业措辞
        assert!(p.starts_with(DEFAULT_VL_INTRO));
        assert!(p.contains("对这张图片执行判定"));
        assert!(
            !p.contains("商品图"),
            "业务中性红线：模板不许再硬编码行业措辞"
        );
        let custom = build_prompt("这是一套房源照片。", &[judge_ask()], &[tag_ask()]);
        assert!(custom.starts_with("这是一套房源照片。"));
    }

    #[test]
    fn parse_plain_json() {
        let raw = r#"{"rules":{"R3_no_watermark":{"violated":false,"regions":[]}},"tags":{"dress_silhouette":"A字","scene":"白底图"}}"#;
        let r = parse_round(raw, &[judge_ask()], &[tag_ask()]).unwrap();
        assert!(!r.judge["R3_no_watermark"].violated);
        assert_eq!(
            r.tags["R5_attribute"],
            vec!["A字".to_string(), "白底图".to_string()]
        );
    }

    #[test]
    fn parse_fenced_with_prose_and_rejects_off_list() {
        let raw = "好的，结果如下：\n```json\n{\"rules\":{\"R3_no_watermark\":{\"violated\":true,\"regions\":[{\"what\":\"水印\",\"bbox\":[0.7,0.8,1.4,0.95]}]}},\"tags\":{\"dress_silhouette\":\"蓬蓬裙\",\"scene\":\"平铺\"}}\n```\n以上。";
        let r = parse_round(raw, &[judge_ask()], &[tag_ask()]).unwrap();
        let j = &r.judge["R3_no_watermark"];
        assert!(j.violated);
        assert_eq!(j.regions, vec![[0.7, 0.8, 1.0, 0.95]]); // 越界钳制
        assert_eq!(r.tags["R5_attribute"], vec!["平铺".to_string()]); // 词表外丢弃
        assert_eq!(
            r.tags_rejected["R5_attribute"],
            vec!["dress_silhouette:蓬蓬裙".to_string()]
        );
    }

    #[test]
    fn parse_missing_judge_rule_is_error() {
        let raw = r#"{"rules":{},"tags":{}}"#;
        assert!(parse_round(raw, &[judge_ask()], &[tag_ask()]).is_err());
    }

    #[test]
    fn expect_semantics() {
        // pass = (violated == (expect=="是"))；R3 expect=否 → violated=false 才过
        let a = judge_ask();
        let expect_yes = a.expect == "是";
        assert!(!(expect_yes), "violated=true + expect=否 → 不过");
        assert!(!expect_yes, "violated=false + expect=否 → 过");
    }

    // ---------- COMPLIANCE-DOMAIN：正向 gate + VL 自评置信度 ----------

    #[test]
    fn forward_gate_prompt_asks_answer_vocab_without_region_fields() {
        let p = build_prompt(DEFAULT_VL_INTRO, &[gate_ask()], &[]);
        // 词表模式：问 answer（原词回答），不问 violated 布尔
        assert!(p.contains("answer 只能原样回答一个词（是 / 否）"), "{p}");
        assert!(p.contains("期望答案：是"), "{p}");
        // 不定位的规则**不问 where/regions**（实测会诱导模型把 violated 判 true）
        assert!(!p.contains("方位格"), "不定位的规则不该问方位格: {p}");
        assert!(!p.contains("bbox"), "{p}");
        // 自评置信度：提问与示例都带上 confidence
        assert!(p.contains("这条还要给 confidence"));
        assert!(p.contains("\"confidence\":0.9"), "{p}");
        // 布尔模式的旧规则（电商 R3）文本保持：问 violated、不问 answer/confidence
        let neg = build_prompt(DEFAULT_VL_INTRO, &[judge_ask()], &[]);
        assert!(neg.contains("violated=true 表示违反该规则"), "{neg}");
        assert!(!neg.contains("answer"));
        assert!(!neg.contains("confidence"));
    }

    #[test]
    fn answer_vocab_is_parsed_and_offlist_is_rejected() {
        let raw = r#"{"rules":{"F0_domain":{"answer":"是","confidence":0.82}}}"#;
        let r = parse_round(raw, &[gate_ask()], &[]).unwrap();
        let out = &r.judge["F0_domain"];
        assert_eq!(out.answer.as_deref(), Some("是"));
        assert_eq!(out.confidence, Some(0.82));
        // violated 与 expect 对账：answer=是 + expect=是 → 没有问题
        assert!(!out.violated);
        // answer=否 → 问题成立
        let r2 = parse_round(
            r#"{"rules":{"F0_domain":{"answer":"否","confidence":0.9}}}"#,
            &[gate_ask()],
            &[],
        )
        .unwrap();
        assert!(r2.judge["F0_domain"].violated);
        // 词表外答案 → 解析失败（不静默当命中）
        let bad = parse_round(
            r#"{"rules":{"F0_domain":{"answer":"可能是吧"}}}"#,
            &[gate_ask()],
            &[],
        );
        assert!(bad.is_err(), "词表外答案必须报错");
        // 缺 answer → 报错
        let missing = parse_round(
            r#"{"rules":{"F0_domain":{"violated":true}}}"#,
            &[gate_ask()],
            &[],
        );
        assert!(missing.is_err(), "词表模式缺 answer 必须报错");
    }

    #[test]
    fn self_confidence_is_parsed_only_when_declared() {
        // 未声明自评的规则（布尔模式）：模型顺手回的 confidence 不采信
        let raw2 = r#"{"rules":{"R3_no_watermark":{"violated":false,"confidence":0.99}}}"#;
        let r2 = parse_round(raw2, &[judge_ask()], &[]).unwrap();
        assert_eq!(r2.judge["R3_no_watermark"].confidence, None);
        // 模型漏给 confidence → None（不是 0.0、也不是 1.0）
        let raw = r#"{"rules":{"F0_domain":{"answer":"是"}}}"#;
        let r = parse_round(raw, &[gate_ask()], &[]).unwrap();
        assert_eq!(r.judge["F0_domain"].confidence, None);
    }

    #[test]
    fn self_confidence_accepts_percent_and_strings_rejects_garbage() {
        let cases = [
            (serde_json::json!(0.7), Some(0.7)),
            (serde_json::json!("0.55"), Some(0.55)),
            (serde_json::json!("90%"), Some(0.9)),
            (serde_json::json!(85), Some(0.85)),
            (serde_json::json!(1), Some(1.0)),
            (serde_json::json!(0), Some(0.0)),
            // 不可解析 / 越界 → None（上层落 confidence_missing，不钳成 1.0）
            (serde_json::json!("很确定"), None),
            (serde_json::json!(-0.2), None),
            (serde_json::json!(250), None),
            (serde_json::json!(true), None),
            (serde_json::json!(null), None),
        ];
        for (v, want) in cases {
            assert_eq!(parse_confidence(&v), want, "input={v}");
        }
    }

    #[test]
    fn needs_region_only_asked_for_inpaint_rules() {
        // 只有需要定位的规则（inpaint）才问 where/regions；同一轮里混合时两套字段并存
        let mut gw = gate_ask();
        gw.need_region = true;
        let p = build_prompt(DEFAULT_VL_INTRO, &[gw, judge_ask()], &[]);
        assert!(p.contains("方位格"), "{p}");
        assert!(p.contains("bbox"), "{p}");
    }

    #[test]
    fn json_extraction_from_noise() {
        assert_eq!(extract_json("垃圾 {\"a\":1} 尾巴").unwrap(), "{\"a\":1}");
        assert!(extract_json("完全没有花括号").is_err());
    }

    // ---------- C9：组级 vl 判定（vl_group_judge） ----------

    fn group_ask() -> VlGroupAsk {
        VlGroupAsk {
            rule_id: "F4_scene_complete".into(),
            prompt: "这张照片属于哪一类场景".into(),
            options: vec![
                "客厅".into(),
                "卧室".into(),
                "厨房".into(),
                "卫生间".into(),
                "其它".into(),
            ],
            require_all: vec!["客厅".into(), "卧室".into(), "厨房".into(), "卫生间".into()],
            probe_max: 0,
        }
    }

    #[test]
    fn group_params_schema_and_whitelist() {
        let ok = serde_yaml::from_str(
            "prompt: p\noptions: [客厅, 卧室]\nrequire_all: [客厅]\nprobe_max: 0",
        )
        .unwrap();
        let p = vl_group_params(&ok).unwrap();
        assert_eq!(p.options.len(), 2);
        assert_eq!(p.probe_max, 0);
        // 未知键 → 拒（承接 C8 口径）
        let bad = serde_yaml::from_str(
            "prompt: p\noptions: [客厅]\nrequire_all: [客厅]\ngroup_by: 房源号",
        )
        .unwrap();
        assert!(vl_group_params(&bad).unwrap_err().contains("group_by"));
        // require_all 必须在 options 里
        let bad2 = serde_yaml::from_str("prompt: p\noptions: [客厅]\nrequire_all: [卧室]").unwrap();
        assert!(vl_group_params(&bad2).unwrap_err().contains("不在 options"));
        // 空 options / 空 require_all
        let bad3 = serde_yaml::from_str("prompt: p\noptions: []\nrequire_all: []").unwrap();
        assert!(vl_group_params(&bad3)
            .unwrap_err()
            .contains("options 不能为空"));
    }

    #[test]
    fn group_label_parsing_tolerates_shapes_and_rejects_offlist() {
        let ids: Vec<String> = group_ask().options.clone();
        let rid = "F4_scene_complete";
        // 首选形状：{"label": "..."}
        assert_eq!(
            parse_group_label("{\"label\": \"厨房\"}", rid, &ids).unwrap(),
            "厨房"
        );
        // 兼容 rules[ruleId].label
        assert_eq!(
            parse_group_label(
                "{\"rules\":{\"F4_scene_complete\":{\"label\":\"卫生间\"}}}",
                rid,
                &ids
            )
            .unwrap(),
            "卫生间"
        );
        // 带围栏/废话 + 解释句子 → 词表子串兜底
        let noisy = "```json\n{\"label\":\"这是一张客厅的照片\"}\n```";
        assert_eq!(parse_group_label(noisy, rid, &ids).unwrap(), "客厅");
        // 词表外答案 → Err（不猜）
        assert!(parse_group_label("{\"label\":\"车库\"}", rid, &ids).is_err());
        assert!(parse_group_label("我不确定", rid, &ids).is_err());
    }

    #[test]
    fn group_probe_prompt_embeds_options_and_asks_for_json() {
        let p = build_group_probe_prompt("中性开场", &group_ask());
        assert!(p.contains("中性开场"));
        assert!(p.contains("厨房"));
        assert!(p.contains("\"label\""));
    }

    #[test]
    fn group_asks_are_not_asked_per_asset() {
        // per_group 的 vl 规则不进资产级合并提问（否则每张图多问一条组级问题）
        let rules = r#"
- id: F1_sharpness
  executor: classic
  check: {detector: sharpness, params: {min: 100}}
  on_fail: retry_then_human
- id: F3_duplicate
  executor: classic
  judgement_scope: per_group
  group_by: {source: folder}
  check: {detector: phash_dup, params: {threshold: 8}}
  on_fail: pass
- id: F4_scene_complete
  executor: vl
  judgement_scope: per_group
  group_by: {source: folder}
  check:
    detector: vl_group_judge
    params:
      prompt: 这张照片属于哪一类场景
      options: [客厅, 卧室, 厨房, 卫生间, 其它]
      require_all: [客厅, 卧室, 厨房, 卫生间]
  on_fail: retry_then_human
- id: R5_tag
  executor: vl
  outputs: [scene]
  on_fail: pass
"#;
        let pipeline = "steps:\n  - {kind: judge, rule: F1_sharpness}\n  - {kind: judge, rule: F3_duplicate}\n  \
                        - {kind: judge, rule: F4_scene_complete}\n  - {kind: judge, rule: R5_tag}\n";
        let pack = core_config::Pack::from_yaml_strs(
            "g",
            "scene: [客厅, 厨房]\n",
            rules,
            pipeline,
            "detail_columns: [imageSN]\n",
        )
        .unwrap();
        let (judge, tag) = asks_from_pack(&pack);
        assert!(
            judge.is_empty(),
            "组级 vl 规则不进资产级 judge 提问: {judge:?}"
        );
        assert_eq!(tag.len(), 1, "打标型仍在资产级");
        let gasks = group_asks_from_pack(&pack);
        assert_eq!(gasks.len(), 1);
        assert_eq!(gasks[0].rule_id, "F4_scene_complete");
        assert_eq!(gasks[0].require_all.len(), 4);
    }

    /// COMPLIANCE-CLOSE：**坏图**（本地 decode 不出来 + 模型 4xx 拒收）必须报 `VlError::Decode`
    /// —— 上层据此走规则级隔离（`undetermined` + `reason=rule_error`），而不是 job 级重试。
    /// 反例护栏：如果哪天把它并回 `Http`，坏图就会变成「重试到 failed、图上一条判定都没有」。
    #[test]
    fn undecodable_image_maps_to_decode_not_http() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for req in listener.incoming() {
                let mut stream = req.unwrap();
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf);
                let body = r#"{"error":{"message":"You have uploaded an unsupported image."}}"#;
                let http = format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(http.as_bytes());
            }
        });
        // 截断的 JPEG：SOI/APP0 之后没有可解码的扫描数据
        let dir = std::env::temp_dir().join(format!("dam_vl_decode_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("broken.jpg");
        let mut junk = vec![0xFFu8, 0xD8, 0xFF, 0xE0];
        junk.extend_from_slice(&[0u8; 160]);
        std::fs::write(&p, &junk).unwrap();

        let cfg = VlConfig {
            base_url: format!("http://{addr}"),
            api_key: "mock-key".into(),
            model: "mock-vl".into(),
            timeout_secs: 15,
            max_tokens: 128,
            proxy: Some(String::new()),
        };
        let mut sess = VlSession::new(
            cfg,
            vec![gate_ask()],
            Vec::new(),
            Arc::new(VlStats::default()),
        );
        let e = sess.round(&p).expect_err("坏图必须报错");
        assert!(matches!(e, VlError::Decode(_)), "期望 Decode，实际 {e:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------- K07-FIX-D：VL 结果缓存 ----------

    /// 缓存键三段：内容 / prompt / 模型（+ 预算）任一不同 → 不同键。
    #[test]
    fn vl_cache_key_covers_content_prompt_model_and_budget() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.jpg");
        let b = dir.path().join("b.jpg");
        std::fs::write(&a, b"image-A").unwrap();
        std::fs::write(&b, b"image-B").unwrap();

        let base = VlCache::key(&a, "prompt", "deepseek-flash", 4096).unwrap();
        assert_eq!(
            base,
            VlCache::key(&a, "prompt", "deepseek-flash", 4096).unwrap()
        );
        // 内容不同
        assert_ne!(
            base,
            VlCache::key(&b, "prompt", "deepseek-flash", 4096).unwrap()
        );
        // prompt 不同（不同规则问的不是一回事）
        assert_ne!(
            base,
            VlCache::key(&a, "prompt2", "deepseek-flash", 4096).unwrap()
        );
        // 模型不同
        assert_ne!(base, VlCache::key(&a, "prompt", "qwen3-vl", 4096).unwrap());
        // 预算不同
        assert_ne!(
            base,
            VlCache::key(&a, "prompt", "deepseek-flash", 1024).unwrap()
        );
        // 图不存在 → None（调用方照常发请求，不因缓存读写失败把判定拖挂）
        assert!(VlCache::key(&dir.path().join("nope.jpg"), "p", "m", 1).is_none());
    }

    #[test]
    fn vl_cache_hits_misses_and_evicts_oldest() {
        let cache = VlCache::new(2);
        cache.put("k1".into(), "v1".into());
        cache.put("k2".into(), "v2".into());
        assert_eq!(cache.get("k1").as_deref(), Some("v1"));
        // 超过容量 → 最久未用的被淘汰（k1 刚被 get 过，应该是 k2 走）
        cache.put("k3".into(), "v3".into());
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get("k1").as_deref(), Some("v1"));
        assert_eq!(cache.get("k2"), None);
        assert_eq!(cache.get("k3").as_deref(), Some("v3"));
        // 关闭态（容量 0）永远不命中、也不存
        let off = VlCache::new(0);
        off.put("k".into(), "v".into());
        assert_eq!(off.get("k"), None);
        assert!(off.is_empty());
    }

    /// **不打网络**就能命中：证明 `ask_raw` 真的走缓存（base_url 指向不可达地址）。
    #[test]
    fn ask_raw_serves_from_cache_without_any_http() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("img.jpg");
        std::fs::write(&img, b"not-a-real-jpeg-but-cache-first").unwrap();
        let cfg = VlConfig {
            base_url: "http://127.0.0.1:9".into(), // discard 端口：连上就说明没走缓存
            api_key: "k".into(),
            model: "mock-vl".into(),
            timeout_secs: 2,
            max_tokens: 4096,
            proxy: Some(String::new()),
        };
        let stats = Arc::new(VlStats::default());
        let sess = VlSession::new(cfg.clone(), Vec::new(), Vec::new(), stats.clone());
        // 预热全局缓存（与 ask_raw 用同一个 key 计算）
        let key = VlCache::key(&img, "是不是纯白色背景", &cfg.model, 4096).unwrap();
        VlCache::global().put(key, "是".into());

        let raw = sess
            .ask_raw(&img, "是不是纯白色背景", 4096)
            .expect("二次问必须命中缓存");
        assert_eq!(raw, "是");
        assert_eq!(
            stats.calls.load(Ordering::Relaxed),
            0,
            "命中缓存不该发 HTTP"
        );
        assert!(stats.cache_hits.load(Ordering::Relaxed) >= 1);
        VlCache::global().clear();
    }
}
