//! L0 `core-config` — 行业元数据装载（C0，PLAN.md §6.3）。
//!
//! IP-K16（2026-10-22 用户裁决）：**行业元数据的唯一真源 = 插件包自带**
//! （`manifest.json` + `resources/*.json`）—— `Pack::from_plugin_dir` 是生产侧唯一装载入口，
//! 读规则清单（id/name/on_fail/fixer）、词表、报告列名与低置信度阈值。
//! 宿主不再内置 YAML 行业包（`include_str!` 已删），也不再「装行业包 + 切引擎」两步。
//!
//! `from_yaml_strs` / `from_dir` 只保留给**基线参考实现**（旧内联引擎的测试对照）：
//! 四件 YAML 齐才算一个包，生产代码不得调用。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use crate::{ConfigError, Result};

// ---------- 词表（taxonomy.yaml） ----------

/// 受控词表：字段名 → 候选值列表（后续与 P1 行业模板库合流，不另发明格式）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Taxonomy {
    /// 顶层 key 即词表字段（category / dress_silhouette / scene / ...）
    #[serde(flatten)]
    pub fields: BTreeMap<String, Vec<String>>,
}

// ---------- 规则（rules.yaml） ----------

/// 检测执行档（deferral chain 三档语义）：classic（毫秒确定性）| local_model（ONNX 秒级）| vl（远程语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleExecutor {
    Classic,
    LocalModel,
    Vl,
}

/// 证据来源：image_only（本图可判）| external_data（external 规则 v1 直接报「不可自动判定」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleEvidence {
    ImageOnly,
    ExternalData,
}

/// 修复档位：guaranteed（修完必过）| best_effort（修完需复检，仍败转人工）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixerTier {
    Guaranteed,
    BestEffort,
}

/// 失败处置（三态口径，2026-10-17 裁决）：
/// - `pass`：fail 不影响合格结论（打标型/软规则，证据留 on_fail=pass）；
/// - **`fail`**：判定为不合格（无修复器可修时的唯一合法值）；
/// - `auto_fix`：修完即合格（配 guaranteed）；
/// - `retry_then_human`：复检仍败交低置信度复核 —— **必须配 fixer**，无 fixer 时由装载期
///   lint 硬拒（这个组合曾把 27 张真图里 12 张「不合格」全变成「待人工」，最终「不合格 0」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleOnFail {
    Pass,
    Fail,
    AutoFix,
    RetryThenHuman,
}

// ---------- C9：按组判定（judgement_scope / group_by） ----------

/// C9 判定作用域：逐图（缺省）| 按组。
///
/// `per_group` 的语义（卡 task，收 C7 缺口 G1）：
/// - **谁定义组** = 规则自己（`group_by`），不引入 pack 级全局组键——同一包里
///   「重复房照（同目录）」「必要场景齐套（同房源）」可以是不同的组；
/// - **执行次数** = 每组一次（组级 job，见 `svc-pipeline::compliance_group`），不是每图一次；
/// - **落点** = 按组成员展开（组内每成员一行，内容逐字相同，见 migration 0014）；
/// - **降级** = 成员不可判 / 组键缺失 → `pending_human` + evidence.reason（响亮，不静默）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgementScope {
    #[default]
    PerAsset,
    PerGroup,
}

/// C9 组键来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupSource {
    /// 同目录（`absPath` 的父目录）= 同一组。语义：一次拍摄/一套房源/一批同场景。
    Folder,
    /// `assets` 表的一列（白名单 `core-types::GROUP_ASSET_FIELDS`，取值 = 列名本体）。
    AssetField,
}

/// C9 组键声明（`judgement_scope: per_group` 规则的必填项）。
///
/// `deny_unknown_fields`：`group_by` 里写错键**装载即拒**（不是「写了没反应」——
/// C7 的 `params.group_by` 被静默忽略就是踩的这个坑）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupBy {
    pub source: GroupSource,
    /// `source: asset_field` 时必填（列名，见白名单）；`source: folder` 时不得携带。
    #[serde(default)]
    pub field: Option<String>,
}

impl GroupBy {
    /// 作用域键：组级 job 与规则的配对依据（写进 job payload 的 `scope`）。
    /// 同一个 (source, field) 的 per_group 规则共享一个组级 job（一次遍历判完）。
    pub fn scope_key(&self) -> String {
        match self.source {
            GroupSource::Folder => "folder".to_string(),
            GroupSource::AssetField => {
                format!("asset_field:{}", self.field.as_deref().unwrap_or_default())
            }
        }
    }

    /// 组键列名（folder 源 → None）。
    pub fn asset_field(&self) -> Option<&str> {
        match self.source {
            GroupSource::Folder => None,
            GroupSource::AssetField => self.field.as_deref(),
        }
    }
}

/// 检测声明：detector id（检测器注册表键，C1 起落地）+ 参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub detector: String,
    #[serde(default)]
    pub params: serde_yaml::Value,
    /// 置信度来源（**判定型规则必填**，装载期 lint 硬拒缺失）：
    /// 三态口径要求每条规则结果带 confidence，且不允许拿原始 score 冒充。
    #[serde(default)]
    pub confidence: Option<ConfidenceSpec>,
}

/// 置信度来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceSource {
    /// 检测器判定余量（|score − 阈值| / 阈值）：数据驱动的把握度，只有注册表声明了
    /// margin 的检测器可用（其余检测器装载期报错）
    DetectorMargin,
    /// 通道本身没有概率（VL 词表答案 / 组级离散检测器）→ pack 显式声明固定值
    Fixed,
    /// COMPLIANCE-DOMAIN：VL 通道**自评**置信度（同一 JSON 里回 `confidence` 0..1）。
    /// 与 `fixed` 的区别是它不是 pack 拍的常数，而是模型对这一张图的把握度：
    /// 拿到就逐图落库（低置信度自动进复核队列）；拿不到 = 该行转 `undetermined`
    /// （reason=confidence_missing），**绝不回退成 1.0 / fixed 值**。
    VlSelfReport,
}

/// `check.confidence` 声明。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfidenceSpec {
    pub source: ConfidenceSource,
    /// `source: fixed` 必填（0..1）；`detector_margin` / `vl_self_report` 不得携带
    /// （余量由检测器算、自评由 VL 逐图回，pack 拍的常数会覆盖真实把握度）
    #[serde(default)]
    pub value: Option<f64>,
}

/// 修复声明：修复插件 id（插件注册表键，C3/C4 落地）+ 档位。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fixer {
    /// 修复插件 id（YAML 字段名 type）
    #[serde(rename = "type")]
    pub plugin: String,
    pub tier: FixerTier,
    /// 插件参数（C3 起支持；smart_crop 用 {min, max} 声明目标占比区间，缺省 0.65/0.85）
    #[serde(default)]
    pub params: serde_yaml::Value,
}

/// 可本地化文本（i18n-F，卡 `task`）。
///
/// 行业包的**用户面展示文案**（manifest `name`/`short_name`/`summary`、rules `rules[].name`、
/// `result_columns[].label`）同时接受两种形态：
/// * 字符串 = 全语言通用（现状行为，唯一向后兼容形态；第三方包不提供 en 也不报错）；
/// * 对象 `{"zh-Hans": "…", "en-US": "…"}` = 按语言给值（键 = locale 闭集外自由扩展，值必须非空字符串）。
///
/// 取值 [`LocalizedText::pick`]：当前 locale → `zh-Hans` → 任意一条（BTreeMap 序稳定）→ 空串
/// （回退发生时 `warn!` 一次，提醒包作者补齐）。**不升 `api_version`**：纯可选形态。
/// 宿主不解释 locale 键的拼写（除 `zh-Hans` 回退键外按原文精确匹配）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LocalizedText {
    /// 全语言通用（现状行为）。
    Plain(String),
    /// 按语言给值；键是 locale 标签（`zh-Hans` / `en-US` / …），值非空字符串。
    Map(BTreeMap<String, String>),
}

impl Default for LocalizedText {
    fn default() -> Self {
        LocalizedText::Plain(String::new())
    }
}

impl From<&str> for LocalizedText {
    fn from(s: &str) -> Self {
        LocalizedText::Plain(s.to_string())
    }
}

impl From<String> for LocalizedText {
    fn from(s: String) -> Self {
        LocalizedText::Plain(s)
    }
}

/// 回退链里的优先键（当前 locale 不在 → 先试它）。
pub const I18N_FALLBACK_LOCALE: &str = "zh-Hans";

impl LocalizedText {
    /// 全语言通用形态的构造器（测试/内置默认值用）。
    pub fn plain(s: impl Into<String>) -> Self {
        LocalizedText::Plain(s.into())
    }

    /// 按回退链取展示值：`locale` → `zh-Hans` → 任意一条 → 空串。
    ///
    /// 回退到非请求 locale 时 `eprintln!` 一次（进程级 once；仓内日志约定是
    /// eprintln，无 tracing 依赖）：包没给当前语言的文案是包作者该补的事，
    /// 但**不能因此崩**——中文兜底是可接受边界（协议 §4.0）。
    pub fn pick(&self, locale: &str) -> &str {
        static WARNED: std::sync::Once = std::sync::Once::new();
        match self {
            LocalizedText::Plain(s) => s,
            LocalizedText::Map(m) => {
                if let Some(v) = m.get(locale) {
                    return v;
                }
                if let Some(v) = m.get(I18N_FALLBACK_LOCALE) {
                    WARNED.call_once(|| {
                        eprintln!(
                            "[i18n] 文案缺当前语言 locale={locale}，回退 {I18N_FALLBACK_LOCALE}（请补齐包内该 locale 条目）"
                        );
                    });
                    return v;
                }
                if let Some((k, v)) = m.iter().next() {
                    WARNED.call_once(|| {
                        eprintln!(
                            "[i18n] 文案既缺 locale={locale} 也缺 {I18N_FALLBACK_LOCALE}，回退 {k}"
                        );
                    });
                    return v;
                }
                WARNED.call_once(|| {
                    eprintln!("[i18n] 文案是空对象 locale={locale}，回退空串");
                });
                ""
            }
        }
    }

    /// 原样对象（前端 `nameI18n`/`labelI18n` 透传用）：字符串形态返回 `None`
    /// （前端直接用原字符串，不需要 i18n 对象）。
    pub fn as_map(&self) -> Option<&BTreeMap<String, String>> {
        match self {
            LocalizedText::Plain(_) => None,
            LocalizedText::Map(m) => Some(m),
        }
    }

    /// 是否至少有一条非空文案（manifest 装载校验「name 不能为空」用）：
    /// 字符串形态看本身；对象形态看**任意一条**非空（只给 en 的包也合法）。
    pub fn has_text(&self) -> bool {
        match self {
            LocalizedText::Plain(s) => !s.trim().is_empty(),
            LocalizedText::Map(m) => m.values().any(|v| !v.trim().is_empty()),
        }
    }

    /// 解析 JSON 值（宿主从 manifest/rules.json 手工取字段时用）：
    /// 字符串与 `{"locale": "text"}` 对象都接受，其余 → 默认空串（不让坏文案崩包装载）。
    pub fn from_json(v: Option<&serde_json::Value>) -> Self {
        match v {
            Some(serde_json::Value::String(s)) => LocalizedText::Plain(s.clone()),
            Some(serde_json::Value::Object(map)) => {
                let mut out = BTreeMap::new();
                for (k, val) in map {
                    if let serde_json::Value::String(s) = val {
                        out.insert(k.clone(), s.clone());
                    }
                }
                if out.is_empty() {
                    LocalizedText::default()
                } else {
                    LocalizedText::Map(out)
                }
            }
            _ => LocalizedText::default(),
        }
    }
}

/// 一条规则（六字段 + 打标型 outputs）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    /// 用户面规则名（i18n-F：字符串 = 全语言通用；对象 = 按语言给值，见 [`LocalizedText`]）。
    #[serde(default)]
    pub name: LocalizedText,
    pub executor: RuleExecutor,
    #[serde(default = "default_evidence")]
    pub evidence: RuleEvidence,
    /// 判定型规则的检测声明（打标型规则无 check）
    #[serde(default)]
    pub check: Option<Check>,
    /// 打标型规则：只填 taxonomy 字段，不打合格/不合格（复用既有 /custom 打标机制）
    #[serde(default)]
    pub outputs: Vec<String>,
    /// 失败修复器；None = 不可自动修复
    #[serde(default)]
    pub fixer: Option<Fixer>,
    pub on_fail: RuleOnFail,
    /// C9：判定作用域（缺省 per_asset）。per_group 规则的 step 由组级 job 执行一次/组。
    #[serde(default)]
    pub judgement_scope: JudgementScope,
    /// C9：组键声明（per_group 必填；per_asset 不得携带——写了就在装载期报错，
    /// 不允许「配了但引擎不读」）。
    #[serde(default)]
    pub group_by: Option<GroupBy>,
    /// COMPLIANCE-DOMAIN：判不合格时的**业务原因**（如「非房源图」）——落进证据
    /// `failReason`，让「为什么这张图不合格」不用去看 prompt 与引擎内部码。
    /// 可选（缺省 = 只有检测器证据），不影响判定本身。
    #[serde(default)]
    pub fail_label: Option<String>,
}

fn default_evidence() -> RuleEvidence {
    RuleEvidence::ImageOnly
}

impl Rule {
    /// 打标型规则（只填词表，不打合格结论）。
    pub fn is_tagging(&self) -> bool {
        !self.outputs.is_empty()
    }

    /// C9：按组判定的规则（由组级 job 执行、按成员展开落库）。
    pub fn is_group_scoped(&self) -> bool {
        self.judgement_scope == JudgementScope::PerGroup
    }
}

// ---------- 管道（pipeline.yaml） ----------

/// step 双语义（2026-10-16 补，吸收 D12 一般形态）：同一编排器、同一插件注册表——
/// 管道引擎是一般形态，compliance 是其判定特例。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// 规则步骤：检测 → fail 触发 fixer → 复检
    Judge,
    /// 无条件变换：resize / 格式转换 / 压缩 / 水印 / 重命名，不判定
    Do,
}

/// pipeline 的一个步骤（执行序 = steps 声明序）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineStep {
    pub kind: StepKind,
    /// 步骤 id（judge 缺省 = rule id；do 必填，供编排/日志/报告引用）
    #[serde(default)]
    pub id: Option<String>,
    /// judge：规则 id（rules.yaml）
    #[serde(default)]
    pub rule: Option<String>,
    /// do：变换插件 id（插件注册表键，C4 落地）
    #[serde(default)]
    pub plugin: Option<String>,
    /// do 步骤参数（如 {max_side: 800}）
    #[serde(default)]
    pub params: serde_yaml::Value,
    /// judge 短路声明：任一依赖规则 fail → 本步 skipped（编排器强制）
    #[serde(default)]
    pub depends_on: Vec<String>,
}

/// 管道：步骤序 + 复检策略。
///
/// ⚠️ **这两个字段不是可配置的执行路径**：生产执行序、修复策略与复检轮次**全部由处理插件
/// 自己决定**（协议 §3.3/3.4；包的 `fix_policy` / `max_rounds` 在包内）。宿主只把它们
/// 读进元数据（读面展示 / 装载期校验用），没有任何配置或界面能选中它们当执行通道
/// （IP-K16 删了引擎开关，N18 起连这个概念都不给用户）。
/// 不要在这两个字段上新增语义：新流程写进行业包，不写进 pipeline.yaml。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pipeline {
    /// 元数据字段（执行序以处理插件为准；宿主不据此调度）：steps 声明序。
    pub steps: Vec<PipelineStep>,
    /// 元数据字段（复检次数以处理插件为准）：best_effort 修复后复检次数声明值。
    /// plugin 通道的复检轮次由包的 `fix_policy` / `max_rounds` 决定（协议 §3.4）。
    #[serde(default = "default_recheck_max")]
    pub recheck_max: u32,
    /// C8：vl 通道 prompt 的**前言**（业务中性措辞由 pack 提供；缺省 = 引擎内置中性开场白）。
    /// 修的是 C7 缺口「模板硬编码『对这张商品图执行判定』」——房产包也被叫商品图。
    /// 只影响 vl 通道（远程语义判定）的措辞，不影响判定语义/阈值。
    #[serde(default)]
    pub vl_intro: Option<String>,
}

fn default_recheck_max() -> u32 {
    1
}

impl Default for Pipeline {
    fn default() -> Self {
        Self {
            steps: Vec::new(),
            recheck_max: default_recheck_max(),
            vl_intro: None,
        }
    }
}

// ---------- 报告（report.yaml） ----------

/// 报告模板：汇总档位 + manifest.csv 明细列（C2 落地导出）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReportTemplate {
    #[serde(default)]
    pub summary: Vec<String>,
    #[serde(default)]
    pub detail_columns: Vec<String>,
    /// 低置信度复核阈值（report.yaml 可选，默认 0.7）：`confidence < low_confidence`
    /// 或 `undetermined` 的行进「低置信度复核」视图（三态口径下交人工的唯一入口）。
    #[serde(default = "default_low_confidence")]
    pub low_confidence: f64,
}

fn default_low_confidence() -> f64 {
    0.7
}

// ---------- pack（四文件整包） ----------

/// 一个行业包的内存形态：**元数据容器**（规则清单/词表/报告列）。
///
/// IP-K16（2026-10-22 用户裁决）：行业元数据**唯一真源 = 插件包自带**
/// （`manifest.json` + `resources/*.json`）。宿主不再内置任何 YAML 行业包，
/// 也不再有「装行业包 + 切引擎」两步；`from_plugin_dir` 是生产侧唯一装载入口。
/// `from_yaml_strs` / `from_dir` 只服务**基线参考实现**（测试侧跑旧内联引擎），
/// 生产代码不得调用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pack {
    pub id: String,
    #[serde(default)]
    pub taxonomy: Taxonomy,
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub pipeline: Pipeline,
    #[serde(default)]
    pub report: ReportTemplate,
}

/// 四文件名（装载契约：目录下必须四件齐）。
pub const PACK_FILES: [&str; 4] = [
    "taxonomy.yaml",
    "rules.yaml",
    "pipeline.yaml",
    "report.yaml",
];

/// **宿主消费**的插件规则键：写了就一定被读进 `Rule`。
pub const HOST_RULE_KEYS: [&str; 6] = ["id", "name", "executor", "fixer", "on_fail", "fail_label"];

/// **插件私有**的规则键：宿主按设计**不解释**，由插件自己的代码读。
///
/// - `deterministic`：插件内部的判定语义说明（`ruleset.py` 里同一份事实源）。
/// - `params`：规则级阈值声明（`{key: number}`）。插件 `ruleset.py` 运行期**直接读它**
///   （`schema_params()` → 生效阈值；`resources/rules.json` 是判据权威）。宿主不解释这些键
///   的业务含义（协议 §3.3：options 由包解释），只把它们当作「插件拥有的阈值原文」；
///   规格里的 `thresholds` 走 `options.thresholds` 递过去，两条路都在插件里汇合。
/// - `scope`：`per_asset` | `per_group` —— 插件**自己**的成组口径。协议 v1.1 的 `judge`
///   是单资产调用（`docs/industry-pack-protocol.md` §judge），组级由插件在包内自行成组
///   （`packs/plugins/realestate/plugin/ruleset.py` 的 `GROUP_RULES` 消费它）；
///   宿主既不排组级 job、也不按它改 `judgement_scope`（`/api/compliance/judge` 显式要
///   组级会被 400 拒掉）。所以它**不是**「作者写了宿主不读」的键——它有读者，读者是插件。
pub const PLUGIN_PRIVATE_RULE_KEYS: [&str; 3] = ["deterministic", "params", "scope"];

/// `scope` 的合法取值（宿主只校验词表，取别的值即装载期硬拒——写错不许静默）。
const PLUGIN_SCOPE_VALUES: [&str; 2] = ["per_asset", "per_group"];

/// 插件规则键守卫（C8「配了不读一律装载期拒绝」在插件形态的落地）。
///
/// 三类键：宿主消费（`HOST_RULE_KEYS`）/ 插件私有（`PLUGIN_PRIVATE_RULE_KEYS`，有读者）
/// / 其余 = **谁也不读** → 硬拒并点名。老 YAML 行业包的键（`evidence`/`check`/`outputs`/
/// `group_by`）落到这里就会响亮失败，而不是「配了但宿主从不读」。
fn check_plugin_rule_keys(pack: &str, rid: &str, r: &serde_json::Value) -> Result<()> {
    let obj = match r.as_object() {
        Some(o) => o,
        None => {
            return Err(ConfigError::PackLoad {
                pack: pack.to_string(),
                msg: format!("resources/rules.json 的规则 {rid:?} 不是对象"),
            })
        }
    };
    for k in obj.keys() {
        let host = HOST_RULE_KEYS.contains(&k.as_str());
        let private = PLUGIN_PRIVATE_RULE_KEYS.contains(&k.as_str());
        if !host && !private {
            return Err(ConfigError::PackLoad {
                pack: pack.to_string(),
                msg: format!(
                    "规则 {rid}：键 {k:?} 宿主不解释（也没声明为插件私有）——\
                     「配了不读」一律装载期拒绝。宿主消费: {}；插件私有（由插件代码读）: {}",
                    HOST_RULE_KEYS.join("/"),
                    PLUGIN_PRIVATE_RULE_KEYS.join("/")
                ),
            });
        }
    }
    if let Some(scope) = obj.get("scope") {
        let v = scope.as_str().unwrap_or("");
        if !PLUGIN_SCOPE_VALUES.contains(&v) {
            return Err(ConfigError::PackLoad {
                pack: pack.to_string(),
                msg: format!(
                    "规则 {rid}：scope 只能是 {}，读到的不是这两个之一（写错不许静默当成逐图）",
                    PLUGIN_SCOPE_VALUES.join(" | ")
                ),
            });
        }
    }
    Ok(())
}

impl Pack {
    /// 低置信度复核阈值（report.yaml `low_confidence`，缺省/非法一律 0.7）。
    pub fn low_confidence(&self) -> f64 {
        let v = self.report.low_confidence;
        if v.is_finite() && (0.0..=1.0).contains(&v) {
            v
        } else {
            0.7
        }
    }

    /// 装载期 lint（C8）：目录级 schema 检查（未知键/结构），返回逐条 issue。
    /// 装载路径（`from_yaml_strs`/`from_dir`）内部已强制；此处供「装载前 lint」命令行
    /// 与界面预检直接调用（不产出 Pack）。
    pub fn lint_dir(dir: &Path) -> Vec<crate::lint::LintIssue> {
        crate::lint::lint_dir(dir)
    }

    /// 从四段 YAML 文本组装（整包原子：任一解析/校验失败即整体失败）。
    ///
    /// C8：装载前先跑 schema lint（未知键 = 作者以为写了、引擎从不读的键）→ 有 issue 即硬拒，
    /// 消息逐条点出「文件 + 键路径 + 已知取值」。
    pub fn from_yaml_strs(
        id: &str,
        taxonomy_yaml: &str,
        rules_yaml: &str,
        pipeline_yaml: &str,
        report_yaml: &str,
    ) -> Result<Self> {
        let issues = crate::lint::lint_texts(taxonomy_yaml, rules_yaml, pipeline_yaml, report_yaml);
        if !issues.is_empty() {
            return Err(ConfigError::PackLint {
                pack: id.to_string(),
                issues: issues.iter().map(|i| i.to_string()).collect(),
            });
        }
        let taxonomy: Taxonomy = serde_yaml::from_str(taxonomy_yaml)?;
        let rules: Vec<Rule> = serde_yaml::from_str(rules_yaml)?;
        let pipeline: Pipeline = serde_yaml::from_str(pipeline_yaml)?;
        let report: ReportTemplate = serde_yaml::from_str(report_yaml)?;
        let pack = Pack {
            id: id.to_string(),
            taxonomy,
            rules,
            pipeline,
            report,
        };
        pack.validate()?;
        Ok(pack)
    }

    /// 本地目录导入：`<dir>/{taxonomy,rules,pipeline,report}.yaml` 四件齐，packId = 目录名。
    /// 原子性：任一文件缺失/解析失败 → 整体 Err，不产出半包。
    pub fn from_dir(dir: &Path) -> Result<Self> {
        let id = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut texts = Vec::with_capacity(4);
        for name in PACK_FILES {
            let p = dir.join(name);
            if !p.is_file() {
                return Err(ConfigError::PackLoad {
                    pack: id.clone(),
                    msg: format!("missing {name}"),
                });
            }
            texts.push(std::fs::read_to_string(&p)?);
        }
        Self::from_yaml_strs(&id, &texts[0], &texts[1], &texts[2], &texts[3])
    }

    /// 插件包目录 → 行业元数据（**生产唯一装载入口**）。
    ///
    /// 读的是包自带的全部信息：`manifest.json`（id/version）+ `resources/rules.json`
    /// （规则清单 + 报告元数据 `report{summary,detail_columns,low_confidence}`）+
    /// `resources/taxonomy.json` | `resources/scenes.json`（受控词表）。缺 rules.json
    /// 即失败（没有规则清单的包，宿主无法给判定行配名字与报告列）。
    pub fn from_plugin_dir(dir: &Path) -> Result<Self> {
        let read = |rel: &str| -> Result<String> {
            let p = dir.join(rel);
            std::fs::read_to_string(&p).map_err(|e| ConfigError::PackLoad {
                pack: dir
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                msg: format!("读不出 {rel}: {e}"),
            })
        };
        let manifest = read("manifest.json")?;
        let rules = read("resources/rules.json")?;
        let mut pack = Self::from_plugin_json(&manifest, &rules)?;
        // 词表：电商包用 taxonomy.json、房产包用 scenes.json；有则并入，没有就是空词表
        for name in ["resources/taxonomy.json", "resources/scenes.json"] {
            if let Ok(txt) = std::fs::read_to_string(dir.join(name)) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                    merge_taxonomy(&mut pack.taxonomy, &v);
                }
            }
        }
        Ok(pack)
    }

    /// 同上，但入参已是文件文本（单测/无文件系统场景用；行为与 `from_plugin_dir` 一致）。
    pub fn from_plugin_json(manifest_json: &str, rules_json: &str) -> Result<Self> {
        let manifest: serde_json::Value =
            serde_json::from_str(manifest_json).map_err(|e| ConfigError::PackLoad {
                pack: "<plugin>".into(),
                msg: format!("manifest.json 解析失败: {e}"),
            })?;
        let meta: serde_json::Value =
            serde_json::from_str(rules_json).map_err(|e| ConfigError::PackLoad {
                pack: "<plugin>".into(),
                msg: format!("resources/rules.json 解析失败: {e}"),
            })?;
        let id = manifest
            .get("id")
            .and_then(|v| v.as_str())
            .or_else(|| meta.get("pack").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        if id.trim().is_empty() {
            return Err(ConfigError::PackLoad {
                pack: "<plugin>".into(),
                msg: "manifest.json 缺 id（插件身份是判定行归属的唯一键）".into(),
            });
        }
        let mut rules = Vec::new();
        for r in meta
            .get("rules")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let rid = r.get("id").and_then(|v| v.as_str()).unwrap_or("");
            check_plugin_rule_keys(&id, rid, &r)?;
            if rid.trim().is_empty() {
                return Err(ConfigError::PackLoad {
                    pack: id.clone(),
                    msg: "resources/rules.json 里有规则缺 id".into(),
                });
            }
            rules.push(Rule {
                id: rid.to_string(),
                // i18n-F：字符串 = 全语言通用；对象 = {"locale": 文案}。坏形态不拒装
                // （展示文案的容错与 `fail_label` 同级），取值时回退链兜底。
                name: LocalizedText::from_json(r.get("name")),
                executor: rule_executor_from_plugin(
                    r.get("executor").and_then(|v| v.as_str()).unwrap_or("vl"),
                ),
                evidence: RuleEvidence::ImageOnly,
                check: None,
                outputs: Vec::new(),
                fixer: r
                    .get("fixer")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty() && *s != "null")
                    .map(|f| Fixer {
                        plugin: f.to_string(),
                        tier: FixerTier::BestEffort,
                        params: serde_yaml::Value::Null,
                    }),
                on_fail: rule_on_fail_from_plugin(
                    r.get("on_fail").and_then(|v| v.as_str()).unwrap_or("pass"),
                ),
                // 宿主恒按**逐图**建规则：组级口径是插件私有的（见
                // `PLUGIN_PRIVATE_RULE_KEYS` 的说明），宿主不按 `scope` 改这里——
                // 而且硬拒「配了不读」的外来键（`group_by` 等），所以不存在「配了组级但宿主排逐图」。
                judgement_scope: JudgementScope::PerAsset,
                group_by: None,
                fail_label: r
                    .get("fail_label")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
            });
        }
        if rules.is_empty() {
            return Err(ConfigError::PackLoad {
                pack: id.clone(),
                msg: "resources/rules.json 未声明任何规则".into(),
            });
        }
        // 报告元数据：插件自带（缺省 = 三态汇总 + 0.7 阈值 + 无明细列）
        let report = meta.get("report").cloned().unwrap_or_default();
        let str_list = |key: &str| -> Vec<String> {
            report
                .get(key)
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default()
        };
        let summary = {
            let v = str_list("summary");
            if v.is_empty() {
                vec![
                    "合格".to_string(),
                    "不合格".to_string(),
                    "未判定".to_string(),
                ]
            } else {
                v
            }
        };
        let report = ReportTemplate {
            summary,
            detail_columns: str_list("detail_columns"),
            low_confidence: report
                .get("low_confidence")
                .and_then(|v| v.as_f64())
                .unwrap_or_else(default_low_confidence),
        };
        Ok(Pack {
            id,
            taxonomy: Taxonomy::default(),
            rules,
            pipeline: Pipeline::default(),
            report,
        })
    }

    pub fn rule(&self, id: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| r.id == id)
    }

    /// schema 语义校验（装载时强制；错误消息 = 面向 pack 作者的诊断）。
    pub fn validate(&self) -> Result<()> {
        let err = |msg: String| -> Result<()> {
            Err(ConfigError::PackValidation {
                pack: self.id.clone(),
                msg,
            })
        };
        if self.id.trim().is_empty() {
            return err("pack id is empty".into());
        }
        // 规则：id 非空唯一；判定/打标二选一；打标型必 pass 且无修复器；auto_fix 必有修复器
        let mut seen = std::collections::BTreeSet::new();
        for r in &self.rules {
            if r.id.trim().is_empty() {
                return err("rule id is empty".into());
            }
            if !seen.insert(r.id.clone()) {
                return err(format!("duplicate rule id: {}", r.id));
            }
            if r.check.is_none() && r.outputs.is_empty() {
                return err(format!("rule {}: neither check nor outputs", r.id));
            }
            if r.is_tagging() {
                if r.on_fail != RuleOnFail::Pass {
                    return err(format!(
                        "rule {}: tagging rule must on_fail: pass (打标失败不影响合格结论)",
                        r.id
                    ));
                }
                if r.fixer.is_some() {
                    return err(format!("rule {}: tagging rule must not carry fixer", r.id));
                }
            }
            if r.on_fail == RuleOnFail::AutoFix && r.fixer.is_none() {
                return err(format!("rule {}: on_fail auto_fix requires a fixer", r.id));
            }
            if let Some(f) = &r.fixer {
                if f.plugin.trim().is_empty() {
                    return err(format!("rule {}: fixer plugin is empty", r.id));
                }
            }
            // C9：作用域与组键的正式语义（谁定义组 / 键从哪来 / 缺键怎么办）
            match (r.judgement_scope, &r.group_by) {
                (JudgementScope::PerGroup, None) => {
                    return err(format!(
                        "rule {}: judgement_scope per_group requires group_by ({{source: folder}} \
                         或 {{source: asset_field, field: <列>}})",
                        r.id
                    ));
                }
                (JudgementScope::PerAsset, Some(_)) => {
                    return err(format!(
                        "rule {}: group_by 只对 judgement_scope per_group 生效（本规则是 per_asset）\
                         —— 配了不读的键一律装载期拒绝",
                        r.id
                    ));
                }
                (JudgementScope::PerGroup, Some(gb)) => {
                    if let Some(f) = &gb.field {
                        if !core_types::group_asset_field_ok(f) {
                            return err(format!(
                                "rule {}: group_by.field {f:?} 不在白名单（已知: {}）",
                                r.id,
                                core_types::group_asset_fields_joined()
                            ));
                        }
                    }
                    match gb.source {
                        GroupSource::Folder => {
                            if gb.field.is_some() {
                                return err(format!(
                                    "rule {}: group_by source folder 不携带 field（folder 源取 \
                                     absPath 父目录）",
                                    r.id
                                ));
                            }
                        }
                        GroupSource::AssetField => {
                            if gb.field.as_deref().unwrap_or("").trim().is_empty() {
                                return err(format!(
                                    "rule {}: group_by source asset_field 必须给 field（已知: {}）",
                                    r.id,
                                    core_types::group_asset_fields_joined()
                                ));
                            }
                        }
                    }
                    if r.check.is_none() {
                        return err(format!(
                            "rule {}: judgement_scope per_group 只支持判定型规则（打标型按图落库，\
                             没有组级形态）",
                            r.id
                        ));
                    }
                    if r.fixer.is_some() {
                        return err(format!(
                            "rule {}: judgement_scope per_group 不支持 fixer（组级修复语义不成立：\
                             修哪一张、修完组还算不算齐套都无从定义）——组级判定只出结论",
                            r.id
                        ));
                    }
                }
                (JudgementScope::PerAsset, None) => {}
            }
        }
        let rule_ids = seen;
        // C9：per_group 规则必须在 pipeline 里声明（否则永不执行 = 配了没反应的静默形态）
        for r in self.rules.iter().filter(|r| r.is_group_scoped()) {
            let declared = self
                .pipeline
                .steps
                .iter()
                .any(|s| s.kind == StepKind::Judge && s.rule.as_deref() == Some(r.id.as_str()));
            if !declared {
                return err(format!(
                    "rule {}: judgement_scope per_group 但 pipeline 未声明该规则（组级 job 只执行 \
                     pipeline 里的步骤）",
                    r.id
                ));
            }
        }
        // 步骤：judge 引用存在的规则；do 必填 plugin 且不携带 rule；depends_on 引用存在的规则
        for (i, s) in self.pipeline.steps.iter().enumerate() {
            let sid = s.id.clone().unwrap_or_else(|| format!("#{}", i));
            match s.kind {
                StepKind::Judge => {
                    let rid = s.rule.as_deref().unwrap_or("");
                    if !rule_ids.contains(rid) {
                        return err(format!("step {sid}: judge references unknown rule '{rid}'"));
                    }
                }
                StepKind::Do => {
                    if s.rule.is_some() {
                        return err(format!("step {sid}: do step must not carry rule"));
                    }
                    if s.plugin.as_deref().unwrap_or("").trim().is_empty() {
                        return err(format!("step {sid}: do step requires plugin"));
                    }
                    if s.id.as_deref().unwrap_or("").trim().is_empty() {
                        return err(format!("do step #{}: id is required", i));
                    }
                }
            }
            for dep in &s.depends_on {
                if !rule_ids.contains(dep) {
                    return err(format!("step {sid}: depends_on unknown rule '{dep}'"));
                }
            }
        }
        Ok(())
    }
}

/// 插件 `resources/{taxonomy,scenes}.json` → 受控词表（只收「字符串数组」型键）。
///
/// 键名归一：`scene_options` → `scene`，`require_all` 不是词表（是「齐套要求」）跳过；
/// `note` / `pack` / `version` 是文档键，跳过。
fn merge_taxonomy(t: &mut Taxonomy, v: &serde_json::Value) {
    let Some(obj) = v.as_object() else { return };
    for (k, val) in obj {
        if matches!(
            k.as_str(),
            "note" | "pack" | "version" | "api_version" | "require_all"
        ) {
            continue;
        }
        let Some(arr) = val.as_array() else { continue };
        let mut vals = Vec::new();
        for x in arr {
            if let Some(s) = x.as_str() {
                vals.push(s.to_string());
            }
        }
        if vals.is_empty() {
            continue;
        }
        let key = if k == "scene_options" {
            "scene"
        } else {
            k.as_str()
        };
        t.fields.entry(key.to_string()).or_insert(vals);
    }
}

/// 插件规则 executor 词 → 宿主枚举（插件自己执行判定，此值只用于展示/报告）。
fn rule_executor_from_plugin(s: &str) -> RuleExecutor {
    let s = s.trim().to_ascii_lowercase();
    if s == "classic" {
        RuleExecutor::Classic
    } else if s.starts_with("local_model") || s.contains("u2net") {
        RuleExecutor::LocalModel
    } else {
        RuleExecutor::Vl
    }
}

/// 插件规则 on_fail 词 → 宿主枚举（缺省/未知 = pass，插件已自行处置失败语义）。
fn rule_on_fail_from_plugin(s: &str) -> RuleOnFail {
    match s.trim().to_ascii_lowercase().as_str() {
        "fail" => RuleOnFail::Fail,
        "auto_fix" => RuleOnFail::AutoFix,
        "retry_then_human" => RuleOnFail::RetryThenHuman,
        _ => RuleOnFail::Pass,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAXONOMY: &str = "category: [连衣裙, 裤装]\nscene: [白底图, 平铺]\n";

    const RULES: &str = r#"
- id: R1_size
  name: 尺寸达标
  executor: classic
  evidence: image_only
  check: {detector: size_min, params: {min: 700}}
  fixer: {type: upscale, tier: best_effort}
  on_fail: retry_then_human
- id: R2_white_bg
  name: 白底纯白
  executor: classic
  check: {detector: white_edge, params: {max_dev: 8}}
  fixer: {type: replace_bg, tier: guaranteed}
  on_fail: auto_fix
- id: R5_attribute
  name: 属性打标
  executor: vl
  outputs: [category, scene]
  on_fail: pass
"#;

    const PIPELINE: &str = r#"
steps:
  - {kind: judge, rule: R1_size}
  - {kind: judge, rule: R2_white_bg}
  - {kind: judge, rule: R5_attribute, depends_on: [R1_size]}
  - {kind: do, id: resize_800, plugin: resize, params: {max_side: 800}}
recheck_max: 1
"#;

    const REPORT: &str = "summary: [合格, 不合格, 未判定]\ndetail_columns: [imageSN, R1, R2]\n";

    fn sample_pack(id: &str) -> Result<Pack> {
        Pack::from_yaml_strs(id, TAXONOMY, RULES, PIPELINE, REPORT)
    }

    #[test]
    fn parses_sample_pack_with_dual_semantics() {
        let p = sample_pack("sample").unwrap();
        assert_eq!(p.id, "sample");
        // 词表
        assert_eq!(p.taxonomy.fields.get("category").unwrap().len(), 2);
        // 规则三档 executor + 打标型
        assert_eq!(p.rules.len(), 3);
        assert_eq!(p.rules[0].executor, RuleExecutor::Classic);
        assert_eq!(
            p.rules[0].fixer.as_ref().unwrap().tier,
            FixerTier::BestEffort
        );
        assert_eq!(p.rules[1].fixer.as_ref().unwrap().plugin, "replace_bg");
        assert_eq!(
            p.rules[1].fixer.as_ref().unwrap().tier,
            FixerTier::Guaranteed
        );
        assert!(p.rules[2].is_tagging());
        assert_eq!(p.rules[2].on_fail, RuleOnFail::Pass);
        assert!(p.rules[2].fixer.is_none());
        assert!(p.rule("R2_white_bg").is_some());
        assert!(p.rule("nope").is_none());
        // step 双语义：judge/do 同管道
        let steps = &p.pipeline.steps;
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0].kind, StepKind::Judge);
        assert_eq!(steps[0].rule.as_deref(), Some("R1_size"));
        assert_eq!(steps[3].kind, StepKind::Do);
        assert_eq!(steps[3].plugin.as_deref(), Some("resize"));
        assert!(steps[3].rule.is_none());
        assert!(steps[2].depends_on.contains(&"R1_size".to_string()));
        assert_eq!(p.pipeline.recheck_max, 1);
        // 报告
        assert_eq!(p.report.summary.len(), 3, "三态口径：合格/不合格/未判定");
        assert!(p.report.detail_columns.contains(&"R1".to_string()));
        // serde roundtrip（UI 预览 / 存激活包快照用）
        let rt: Pack = serde_yaml::from_str(&serde_yaml::to_string(&p).unwrap()).unwrap();
        assert_eq!(rt, p);
    }

    // ---------- C9：按组判定（judgement_scope / group_by） ----------

    const GROUP_RULES: &str = r#"
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
- id: F4_scene
  executor: vl
  judgement_scope: per_group
  group_by: {source: asset_field, field: cameraModel}
  check: {detector: vl_group_judge, params: {prompt: p, options: [客厅], require_all: [客厅]}}
  on_fail: retry_then_human
"#;

    const GROUP_PIPELINE: &str = r#"
steps:
  - {kind: judge, rule: F1_sharpness}
  - {kind: judge, rule: F3_duplicate}
  - {kind: judge, rule: F4_scene}
"#;

    fn group_pack(rules: &str, pipeline: &str) -> Result<Pack> {
        Pack::from_yaml_strs("grouped", TAXONOMY, rules, pipeline, REPORT)
    }

    #[test]
    fn parses_group_scoped_pack_and_scope_keys() {
        let p = group_pack(GROUP_RULES, GROUP_PIPELINE).unwrap();
        assert_eq!(p.rules[0].judgement_scope, JudgementScope::PerAsset);
        assert!(!p.rules[0].is_group_scoped());
        let f3 = p.rule("F3_duplicate").unwrap();
        assert!(f3.is_group_scoped());
        let gb = f3.group_by.as_ref().unwrap();
        assert_eq!(gb.source, GroupSource::Folder);
        assert_eq!(gb.scope_key(), "folder");
        assert_eq!(gb.asset_field(), None);
        // 同 source 不同 field = 不同作用域键（各自一个组级 job）
        assert_eq!(
            p.rule("F4_scene")
                .unwrap()
                .group_by
                .as_ref()
                .unwrap()
                .scope_key(),
            "asset_field:cameraModel"
        );
        // serde roundtrip（激活包快照/UI 预览）
        let rt: Pack = serde_yaml::from_str(&serde_yaml::to_string(&p).unwrap()).unwrap();
        assert_eq!(rt, p);
    }

    #[test]
    fn group_scope_misconfigurations_are_rejected() {
        let base = r#"
- id: R_g
  executor: classic
  judgement_scope: per_group
  check: {detector: sharpness, params: {min: 1}}
  on_fail: pass
"#;
        // ① per_group 缺 group_by
        let e = group_pack(base, "steps:\n  - {kind: judge, rule: R_g}\n").unwrap_err();
        assert!(e.to_string().contains("requires group_by"), "{e}");
        // ② per_asset 带 group_by（配了不读 → 拒）
        let e = group_pack(
            "- id: R_g\n  executor: classic\n  group_by: {source: folder}\n  \
             check: {detector: sharpness, params: {min: 1}}\n  on_fail: pass\n",
            "steps:\n  - {kind: judge, rule: R_g}\n",
        )
        .unwrap_err();
        assert!(e.to_string().contains("per_asset"), "{e}");
        // ③ asset_field 缺 field
        let e = group_pack(
            "- id: R_g\n  executor: classic\n  judgement_scope: per_group\n  \
             group_by: {source: asset_field}\n  check: {detector: sharpness, params: {min: 1}}\n  \
             on_fail: pass\n",
            "steps:\n  - {kind: judge, rule: R_g}\n",
        )
        .unwrap_err();
        assert!(e.to_string().contains("必须给 field"), "{e}");
        // ④ 白名单外列名
        let e = group_pack(
            "- id: R_g\n  executor: classic\n  judgement_scope: per_group\n  \
             group_by: {source: asset_field, field: absPath}\n  \
             check: {detector: sharpness, params: {min: 1}}\n  on_fail: pass\n",
            "steps:\n  - {kind: judge, rule: R_g}\n",
        )
        .unwrap_err();
        assert!(e.to_string().contains("白名单"), "{e}");
        // ⑤ folder 源不该带 field
        let e = group_pack(
            "- id: R_g\n  executor: classic\n  judgement_scope: per_group\n  \
             group_by: {source: folder, field: cameraModel}\n  \
             check: {detector: sharpness, params: {min: 1}}\n  on_fail: pass\n",
            "steps:\n  - {kind: judge, rule: R_g}\n",
        )
        .unwrap_err();
        assert!(e.to_string().contains("不携带 field"), "{e}");
        // ⑥ per_group 但 pipeline 没声明（永不执行）
        let e = group_pack(
            "- id: R_g\n  executor: classic\n  judgement_scope: per_group\n  \
             group_by: {source: folder}\n  check: {detector: sharpness, params: {min: 1}}\n  \
             on_fail: pass\n",
            "steps:\n  - {kind: do, id: x, plugin: resize}\n",
        )
        .unwrap_err();
        assert!(e.to_string().contains("pipeline 未声明"), "{e}");
        // ⑦ group_by 里写错键 → serde 装载即拒（deny_unknown_fields）
        let e = group_pack(
            "- id: R_g\n  executor: classic\n  judgement_scope: per_group\n  \
             group_by: {source: folder, keys: [a]}\n  \
             check: {detector: sharpness, params: {min: 1}}\n  on_fail: pass\n",
            "steps:\n  - {kind: judge, rule: R_g}\n",
        )
        .unwrap_err();
        assert!(
            e.to_string().contains("group_by") || e.to_string().contains("keys"),
            "{e}"
        );
    }

    /// IP-K16：生产侧唯一装载入口 = 插件包自带的元数据（内置 YAML 行业包已删）。
    #[test]
    fn plugin_pack_metadata_is_the_single_source() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs/plugins");

        let ec = Pack::from_plugin_dir(&root.join("ecommerce")).expect("电商插件元数据装载");
        assert_eq!(ec.id, "com.soujpg.pack.ecommerce");
        assert_eq!(ec.rules.len(), 6, "六条规则（R0 gate + 4 判定 + 1 打标）");
        assert!(!ec
            .rule("R0_product_main")
            .expect("R0 在包内")
            .name
            .pick("zh-Hans")
            .is_empty());
        assert_eq!(
            ec.taxonomy.fields.len(),
            3,
            "电商词表三字段（taxonomy.json）"
        );
        assert_eq!(ec.report.summary.len(), 3, "三态口径：合格/不合格/未判定");
        assert!(
            ec.report.detail_columns.contains(&"R1_size".to_string()),
            "{:?}",
            ec.report.detail_columns
        );
        assert!(
            (ec.low_confidence() - 0.7).abs() < 1e-9,
            "阈值来自包内 report"
        );

        let re = Pack::from_plugin_dir(&root.join("realestate")).expect("房产插件元数据装载");
        assert_eq!(re.id, "com.soujpg.pack.realestate");
        let re_ids: Vec<&str> = re.rules.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            re_ids,
            [
                "F0_domain",
                "F1_sharpness",
                "F2_exposure",
                "F3_duplicate",
                "F4_scene_complete",
                "F5_min_size"
            ],
            "房产插件六条规则（F0~F5）按包内声明顺序"
        );
        assert!(
            !re.taxonomy.fields.is_empty(),
            "房产词表（scenes.json）必须读进来"
        );
        assert_eq!(
            re.report.detail_columns.len(),
            10,
            "报告明细 10 列（包内 report.detail_columns）: {:?}",
            re.report.detail_columns
        );
        assert!(
            re.report.detail_columns.contains(&"F0_domain".to_string()),
            "{:?}",
            re.report.detail_columns
        );
        // 规则语义（on_fail / fixer）也来自包内，不再有第二份 YAML
        assert_eq!(
            re.rule("F0_domain").unwrap().on_fail,
            RuleOnFail::Fail,
            "领域 gate 判不合格"
        );
        assert_eq!(
            re.rule("F5_min_size")
                .unwrap()
                .fixer
                .as_ref()
                .unwrap()
                .plugin,
            "upscale"
        );
        assert_eq!(
            re.rule("F3_duplicate").unwrap().on_fail,
            RuleOnFail::Pass,
            "重复房照是软规则（只标记）"
        );
        // 组级口径是**插件私有**的：包内 `scope: per_group`，宿主一侧恒为逐图 + 无组键
        // （协议 v1.1 judge 单资产调用；宿主不按 scope 排组级 job、也不认 group_by）。
        for id in ["F3_duplicate", "F4_scene_complete"] {
            let r = re.rule(id).unwrap();
            assert_eq!(
                r.judgement_scope,
                JudgementScope::PerAsset,
                "{id}: 宿主不按包内 scope 改判定作用域"
            );
            assert!(r.group_by.is_none(), "{id}: 宿主不产生组键");
        }
        // 插件包不驱动宿主内联执行序：pipeline 为空（执行序在插件代码里）
        assert!(ec.pipeline.steps.is_empty() && re.pipeline.steps.is_empty());
    }

    /// IP-K16：插件规则键三类归属（宿主消费 / 插件私有 / 谁也不读就拒）。
    ///
    /// 这条钉死上一轮报告的假证据：`scope: per_group` **不是**被宿主读成组级规则
    /// （宿主恒逐图），而是插件私有键（`ruleset.py::GROUP_RULES` 消费）；
    /// 而真正「配了不读」的键（老 YAML 的 `group_by` / 凭空写的 `severity`）装载即拒。
    #[test]
    fn plugin_rule_keys_are_consumed_private_or_rejected() {
        let manifest = r#"{"id":"com.soujpg.pack.x"}"#;
        // ① 宿主消费 + 插件私有（deterministic / scope）都能装进来
        let p = Pack::from_plugin_json(
            manifest,
            r#"{"rules":[
                {"id":"R1","name":"n","executor":"classic","deterministic":true,
                 "scope":"per_asset","fixer":"upscale","on_fail":"fail","fail_label":"糊"},
                {"id":"R2","name":"g","executor":"classic","deterministic":true,
                 "scope":"per_group","fixer":null,"on_fail":"pass"}
            ]}"#,
        )
        .expect("插件私有键不该被拒");
        assert_eq!(p.rules.len(), 2);
        assert_eq!(p.rules[1].fail_label, None);
        assert_eq!(
            p.rules[1].judgement_scope,
            JudgementScope::PerAsset,
            "包内 per_group 不改变宿主作用域"
        );
        // ② 老 YAML 的组键 → 谁也不读 → 拒（不许「配了宿主不读」）
        let e = Pack::from_plugin_json(
            manifest,
            r#"{"rules":[{"id":"R1","executor":"classic","on_fail":"pass",
                 "group_by":{"source":"folder"}}]}"#,
        )
        .unwrap_err();
        assert!(e.to_string().contains("group_by"), "{e}");
        assert!(e.to_string().contains("宿主不解释"), "{e}");
        // ③ 凭空写的键（宿主没这个能力）→ 拒，并列出可写键
        let e = Pack::from_plugin_json(
            manifest,
            r#"{"rules":[{"id":"R1","executor":"classic","on_fail":"pass","severity":"high"}]}"#,
        )
        .unwrap_err();
        assert!(e.to_string().contains("severity"), "{e}");
        assert!(e.to_string().contains("插件私有"), "{e}");
        // ④ scope 写错（拼错/自造）→ 拒，不静默当逐图
        let e = Pack::from_plugin_json(
            manifest,
            r#"{"rules":[{"id":"R1","executor":"classic","on_fail":"pass","scope":"per-group"}]}"#,
        )
        .unwrap_err();
        assert!(e.to_string().contains("scope"), "{e}");
        // ⑤ 规则不是对象 → 拒
        assert!(Pack::from_plugin_json(manifest, r#"{"rules":["R1"]}"#).is_err());
    }

    /// 插件元数据装载的响亮失败（坏包不许静默半包）。
    #[test]
    fn plugin_pack_metadata_fails_loudly() {
        let manifest = r#"{"id":"com.soujpg.pack.x"}"#;
        // 没有规则清单
        let e = Pack::from_plugin_json(manifest, r#"{"pack":"x","rules":[]}"#).unwrap_err();
        assert!(e.to_string().contains("未声明任何规则"), "{e}");
        // 缺 id
        let e =
            Pack::from_plugin_json(r#"{"name":"x"}"#, r#"{"rules":[{"id":"R1"}]}"#).unwrap_err();
        assert!(e.to_string().contains("缺 id"), "{e}");
        // 规则缺 id
        let e = Pack::from_plugin_json(manifest, r#"{"rules":[{"name":"n"}]}"#).unwrap_err();
        assert!(e.to_string().contains("缺 id"), "{e}");
        // 非 JSON
        assert!(Pack::from_plugin_json(manifest, "not json").is_err());
        // 没有 report 块 → 三态汇总 + 0.7 阈值（读不出就不猜，用冻结默认）
        let p = Pack::from_plugin_json(
            manifest,
            r#"{"rules":[{"id":"R1","name":"n","executor":"vl","on_fail":"fail"}]}"#,
        )
        .unwrap();
        assert_eq!(p.report.summary.len(), 3);
        assert!(p.report.detail_columns.is_empty());
        assert!((p.low_confidence() - 0.7).abs() < 1e-9);
    }

    #[test]
    fn from_dir_atomic_four_files() {
        let dir = std::env::temp_dir().join(format!("c0pack_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 缺文件 → 整体失败
        assert!(Pack::from_dir(&dir).is_err());
        std::fs::write(dir.join("taxonomy.yaml"), TAXONOMY).unwrap();
        assert!(Pack::from_dir(&dir).is_err());
        std::fs::write(dir.join("rules.yaml"), RULES).unwrap();
        std::fs::write(dir.join("pipeline.yaml"), PIPELINE).unwrap();
        std::fs::write(dir.join("report.yaml"), REPORT).unwrap();
        // 四件齐 → 成功，packId = 目录名
        let p = Pack::from_dir(&dir).unwrap();
        assert_eq!(p.id, dir.file_name().unwrap().to_string_lossy());
        assert_eq!(p.rules.len(), 3);
        // 坏 YAML（rules 写入非法枚举）→ 整体失败
        std::fs::write(
            dir.join("rules.yaml"),
            "- {id: X, executor: quantum, check: {detector: d}, on_fail: pass}",
        )
        .unwrap();
        assert!(Pack::from_dir(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_rejects_bad_schema() {
        let ok = sample_pack("v").unwrap();
        // 深拷贝改坏 → 各自报错
        let mut bad = ok.clone();
        bad.rules.push(bad.rules[0].clone()); // 重复 rule id
        assert!(bad.validate().is_err());

        let mut bad = ok.clone();
        bad.rules[0].check = None; // 既无 check 又无 outputs
        assert!(bad.validate().is_err());

        let mut bad = ok.clone();
        bad.rules[2].on_fail = RuleOnFail::AutoFix; // 打标型必须 pass
        assert!(bad.validate().is_err());

        let mut bad = ok.clone();
        bad.rules[2].fixer = Some(Fixer {
            plugin: "smart_crop".into(),
            tier: FixerTier::Guaranteed,
            params: serde_yaml::Value::Null,
        }); // 打标型不得带修复器
        assert!(bad.validate().is_err());

        let mut bad = ok.clone();
        bad.rules[1].fixer = None; // auto_fix 无修复器
        assert!(bad.validate().is_err());

        let mut bad = ok.clone();
        bad.pipeline.steps[0].rule = Some("R9_nope".into()); // 引用不存在规则
        assert!(bad.validate().is_err());

        let mut bad = ok.clone();
        bad.pipeline.steps[3].plugin = None; // do 缺 plugin
        assert!(bad.validate().is_err());

        let mut bad = ok.clone();
        bad.pipeline.steps[2].depends_on = vec!["R9_nope".into()]; // 依赖不存在规则
        assert!(bad.validate().is_err());

        // 全 do 步骤、零规则的管道合法（D12 一般形态：简单批量处理）
        let do_only = Pack {
            id: "do-only".into(),
            taxonomy: Taxonomy::default(),
            rules: vec![],
            pipeline: Pipeline {
                steps: vec![PipelineStep {
                    kind: StepKind::Do,
                    id: Some("resize_800".into()),
                    rule: None,
                    plugin: Some("resize".into()),
                    params: serde_yaml::Value::Null,
                    depends_on: vec![],
                }],
                recheck_max: 1,
                vl_intro: None,
            },
            report: ReportTemplate::default(),
        };
        do_only.validate().unwrap();
    }

    #[test]
    fn unknown_enums_fail_loudly() {
        // 未知枚举值 → 解析失败（不静默兜底，pack 作者必须改）
        assert!(serde_yaml::from_str::<Vec<Rule>>(
            "- {id: X, executor: quantum, check: {detector: d}, on_fail: pass}"
        )
        .is_err());
        assert!(serde_yaml::from_str::<Vec<Rule>>(
            "- {id: X, executor: classic, check: {detector: d}, fixer: {type: t, tier: instant}, on_fail: pass}"
        )
        .is_err());
        assert!(serde_yaml::from_str::<Vec<Rule>>(
            "- {id: X, executor: classic, check: {detector: d}, on_fail: maybe}"
        )
        .is_err());
    }

    // ---- i18n-F（卡 task）：LocalizedText 两形态 + 回退链 ----

    #[test]
    fn localized_text_plain_is_language_neutral() {
        let t = LocalizedText::plain("电商主图合规");
        assert_eq!(t.pick("zh-Hans"), "电商主图合规");
        assert_eq!(t.pick("en-US"), "电商主图合规", "字符串形态 = 全语言通用");
        assert!(t.as_map().is_none(), "字符串形态无 i18n 对象可透传");
    }

    #[test]
    fn localized_text_pick_fallback_chain() {
        let t = LocalizedText::Map(BTreeMap::from([
            ("zh-Hans".to_string(), "白底图纯白".to_string()),
            ("en-US".to_string(), "Pure-white background".to_string()),
        ]));
        assert_eq!(t.pick("en-US"), "Pure-white background");
        assert_eq!(t.pick("zh-Hans"), "白底图纯白");
        assert_eq!(t.pick("ja-JP"), "白底图纯白", "缺当前语言 → 回退 zh-Hans");
        let en_only = LocalizedText::Map(BTreeMap::from([(
            "en-US".to_string(),
            "Only English".to_string(),
        )]));
        assert_eq!(
            en_only.pick("zh-Hans"),
            "Only English",
            "缺 zh-Hans → 回退任意一条（BTreeMap 序稳定），不崩"
        );
        assert_eq!(LocalizedText::Map(BTreeMap::new()).pick("en-US"), "");
    }

    #[test]
    fn localized_text_parses_both_json_forms() {
        assert_eq!(
            LocalizedText::from_json(Some(&serde_json::json!("合规检查"))),
            LocalizedText::plain("合规检查")
        );
        assert_eq!(
            LocalizedText::from_json(Some(&serde_json::json!({
                "zh-Hans": "合规检查", "en-US": "Compliance Check"
            }))),
            LocalizedText::Map(BTreeMap::from([
                ("en-US".to_string(), "Compliance Check".to_string()),
                ("zh-Hans".to_string(), "合规检查".to_string()),
            ]))
        );
        // 非字符串值 / 空对象 / 缺字段 → 默认空串（不拒装，取值时空串由上层回落 id）
        assert_eq!(LocalizedText::from_json(None), LocalizedText::default());
        assert_eq!(
            LocalizedText::from_json(Some(&serde_json::json!({"zh-Hans": 3}))),
            LocalizedText::default()
        );
    }

    #[test]
    fn plugin_rules_accept_object_form_names() {
        // rules.json 的 rules[].name 对象形态可装载，且按 locale 取值（向后兼容不升 api_version）
        let manifest = r#"{"id":"com.t.i18n","version":"1.0.0","api_version":"1.1",
            "entrypoint":{"cmd":"python3","args":["p.py"]},"capabilities":["io.read"],
            "deterministic":true,"timeout_secs":60}"#;
        let rules = r#"{"rules":[
            {"id":"R1","name":{"zh-Hans":"尺寸达标","en-US":"Size ok"},"executor":"classic","on_fail":"fail"},
            {"id":"R2","name":"纯文本兜底","executor":"classic","on_fail":"fail"}]}"#;
        let p = Pack::from_plugin_json(manifest, rules).unwrap();
        assert_eq!(p.rule("R1").unwrap().name.pick("en-US"), "Size ok");
        assert_eq!(p.rule("R1").unwrap().name.pick("zh-Hans"), "尺寸达标");
        assert_eq!(
            p.rule("R2").unwrap().name.pick("en-US"),
            "纯文本兜底",
            "老字符串形态行为不变"
        );
    }
}
