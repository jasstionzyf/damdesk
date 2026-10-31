//! 插件**结果行契约**（N5a）—— B 轴（对外契约），冻结，不许插件自造。
//!
//! 权威口径：`damdesk-product/references/plugin-result-contract.md`
//! （协议文档对应节：`docs/industry-pack-protocol.md` §4.4）。
//!
//! 一句话：**执行结果 ≠ 判定结论**。
//! `result(ok|failed|skipped)` 人人必填；`verdict` 只有 `manifest.kinds` 含 `judge` 的插件
//! 才有（且 `result=ok` 时必填）。处理类插件（resize/换背景/超分…）只有执行结果与产物，
//! **返回 verdict 一律拒收并记错**（`plugin_verdict_not_allowed`）——伪造 pass 是审核信任红线。
//!
//! 行键：`assetId`（+ `ruleId`）。判定类插件按规则出行（与 `rule_verdict` 表 1:1，
//! 一条规则一行，`verdict` 就是该规则的结论）；处理类插件按资产出行，`ruleId` 为空。
//!
//! 本模块只做**纯函数**解析/校验，不碰 DB、不碰 UI（落库与渲染在 N5b）：
//!
//! - [`parse_row`]：单行严格校验（缺字段/越界/处理类带 verdict → [`RowViolation`]）；
//! - [`rows_from_reply`]：整个应答 → 结果行（含旧 `verdicts[]` 应答的 1:1 归一化）。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// manifest `kinds` 的闭集：`judge`（判定）/ `process`（处理）。
///
/// 宿主据此决定有没有审核列/审核动作，**不靠猜**（协议 §4.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    /// 判定类：给审核结论（pass/fail/undetermined）。
    Judge,
    /// 处理类：只产出工作版本/导出文件，没有审核状态。
    Process,
}

impl PluginKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Judge => "judge",
            Self::Process => "process",
        }
    }

    /// 闭集解析（未知值返回 `None`，由调用方点名拒装）。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "judge" => Some(Self::Judge),
            "process" => Some(Self::Process),
            _ => None,
        }
    }

    /// 闭集字符串（错误信息用）。
    pub fn all() -> [&'static str; 2] {
        ["judge", "process"]
    }
}

/// 执行结果三值（**所有**插件都必填）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResultStatus {
    /// 这一行跑成了（不等于「判定通过」）。
    Ok,
    /// 这一行跑失败了：必须带 `error{code,message}`。
    Failed,
    /// 这一行没跑：必须带 `skipReason`。
    Skipped,
}

impl ResultStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ok" => Some(Self::Ok),
            "failed" => Some(Self::Failed),
            "skipped" => Some(Self::Skipped),
            _ => None,
        }
    }

    pub fn all() -> [&'static str; 3] {
        ["ok", "failed", "skipped"]
    }
}

/// 审核结论三态（只有判定类才用；`skipped` 不是结论，走 `result=skipped`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerdictStatus {
    Pass,
    Fail,
    /// 判不了（模型不可用/证据不足）——**不是** fail，也不是 pass。
    Undetermined,
}

impl VerdictStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Undetermined => "undetermined",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pass" => Some(Self::Pass),
            "fail" => Some(Self::Fail),
            "undetermined" => Some(Self::Undetermined),
            _ => None,
        }
    }

    pub fn all() -> [&'static str; 3] {
        ["pass", "fail", "undetermined"]
    }
}

/// 执行失败的结构化原因（`result=failed` 必带）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowError {
    pub code: String,
    pub message: String,
}

/// 判定块（只有 `kinds` 含 `judge` 才允许出现）。
///
/// 自定义语义只能进 `reason_code`（机读码）/`fields`/`evidence`，**不许**自造状态
/// （「待复核」「已优化」这类状态前端无法渲染）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowVerdict {
    pub status: VerdictStatus,
    /// 把握度 0..=1（宿主不给 0 冒充把握度；缺失 = 包内未给）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// 机读原因码（如 `R3_no_watermark`），展示与筛选都靠它，不靠自然语言。
    #[serde(
        default,
        rename = "reasonCode",
        skip_serializing_if = "Option::is_none"
    )]
    pub reason_code: Option<String>,
}

/// 产物：只准两种去处（资产工作版本 / 导出文件），不许插件自建目录树。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RowArtifact {
    /// 资产工作版本（处理类插件的正常产物）。
    WorkingVersion {
        #[serde(rename = "assetId")]
        asset_id: String,
        #[serde(rename = "versionId")]
        version_id: String,
    },
    /// 导出文件（必须带 sha256，便于复现与对账）。
    ExportFile { path: String, sha256: String },
}

/// 一行结果（B 轴外壳，所有插件同一套）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultRow {
    /// 宿主的资产 id（数字/字符串都接受，落地统一为十进制字符串）。
    #[serde(rename = "assetId")]
    pub asset_id: String,
    /// 判定类按规则出行时带；处理类为空。
    #[serde(default, rename = "ruleId", skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    pub result: ResultStatus,
    /// 判定块：`kinds` 含 judge 且有结论时才有（`result=ok` 必填）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<RowVerdict>,
    /// `result=failed` 必填。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RowError>,
    /// `result=skipped` 必填。
    #[serde(
        default,
        rename = "skipReason",
        skip_serializing_if = "Option::is_none"
    )]
    pub skip_reason: Option<String>,
    #[serde(default)]
    pub artifacts: Vec<RowArtifact>,
    /// 自由结构（bbox/OCR/模型回复/耗时/参数）：只展示，不参与逻辑。
    #[serde(default)]
    pub evidence: Map<String, Value>,
    /// 行业自定义列（宿主当**不透明字典**渲染；列声明在 `result_columns`）。
    #[serde(default)]
    pub fields: Map<String, Value>,
}

/// 契约违例（`code()` 是稳定词，落 `evidence.reason`/错误记录，别改已有词）。
#[derive(Debug, Clone, PartialEq)]
pub enum RowViolation {
    /// 行不是 JSON 对象
    NotObject,
    /// 缺 `assetId`（或不是数字/非空字符串）
    AssetIdMissing,
    /// 缺 `result`
    ResultMissing,
    /// `result` 不在 ok|failed|skipped 内
    ResultInvalid(String),
    /// `result=failed` 但没带 `error{code,message}`
    ErrorMissing,
    /// `result=skipped` 但没带 `skipReason`
    SkipReasonMissing,
    /// **处理类插件返回了 verdict**（拒收，不采信、不折算）
    VerdictNotAllowed,
    /// 判定类 `result=ok` 却没给 verdict
    VerdictMissing,
    /// verdict 块不是对象
    VerdictInvalid(String),
    /// `verdict.status` 不在 pass|fail|undetermined 内（含 `skipped` 这类越界值）
    VerdictStatusInvalid(String),
    /// `verdict.confidence` 越界（必须 0..=1）
    VerdictConfidenceInvalid(f64),
    /// 产物不合契约（既不是工作版本也不是导出文件 / 缺必填键）
    ArtifactInvalid(String),
    /// `evidence` 不是对象
    EvidenceNotObject,
    /// `fields` 不是对象
    FieldsNotObject,
}

impl RowViolation {
    /// 稳定机读码（报告/错误记录按它聚合）。
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotObject => "row_not_object",
            Self::AssetIdMissing => "row_asset_id_missing",
            Self::ResultMissing => "row_result_missing",
            Self::ResultInvalid(_) => "row_result_invalid",
            Self::ErrorMissing => "row_error_missing",
            Self::SkipReasonMissing => "row_skip_reason_missing",
            Self::VerdictNotAllowed => "row_verdict_not_allowed",
            Self::VerdictMissing => "row_verdict_missing",
            Self::VerdictInvalid(_) => "row_verdict_invalid",
            Self::VerdictStatusInvalid(_) => "row_verdict_status_invalid",
            Self::VerdictConfidenceInvalid(_) => "row_verdict_confidence_invalid",
            Self::ArtifactInvalid(_) => "row_artifact_invalid",
            Self::EvidenceNotObject => "row_evidence_not_object",
            Self::FieldsNotObject => "row_fields_not_object",
        }
    }
}

impl std::fmt::Display for RowViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotObject => write!(f, "结果行不是 JSON 对象"),
            Self::AssetIdMissing => write!(f, "结果行缺 assetId（数字或非空字符串）"),
            Self::ResultMissing => write!(f, "结果行缺 result（{}）", join(ResultStatus::all())),
            Self::ResultInvalid(v) => write!(
                f,
                "result={v:?} 不在 {} 内（自造状态前端不可渲染）",
                join(ResultStatus::all())
            ),
            Self::ErrorMissing => write!(f, "result=failed 必须带 error{{code,message}}"),
            Self::SkipReasonMissing => write!(f, "result=skipped 必须带 skipReason"),
            Self::VerdictNotAllowed => write!(
                f,
                "处理类插件（kinds 不含 judge）不得返回 verdict —— 拒收（禁止处理冒充判定）"
            ),
            Self::VerdictMissing => write!(
                f,
                "判定类插件 result=ok 必须给出 verdict{{status}}（判不了用 undetermined）"
            ),
            Self::VerdictInvalid(why) => write!(f, "verdict 不合契约：{why}"),
            Self::VerdictStatusInvalid(v) => write!(
                f,
                "verdict.status={v:?} 不在 {} 内",
                join(VerdictStatus::all())
            ),
            Self::VerdictConfidenceInvalid(v) => {
                write!(f, "verdict.confidence={v} 越界（必须 0..=1）")
            }
            Self::ArtifactInvalid(why) => write!(f, "产物不合契约：{why}"),
            Self::EvidenceNotObject => write!(f, "evidence 必须是对象"),
            Self::FieldsNotObject => write!(f, "fields 必须是对象"),
        }
    }
}

fn join(items: [&'static str; 3]) -> String {
    format!("{} / {} / {}", items[0], items[1], items[2])
}

/// `kinds` 是否含判定（判定的列/动作只对这类插件存在）。
pub fn is_judge(kinds: &[PluginKind]) -> bool {
    kinds.contains(&PluginKind::Judge)
}

/// `kinds` 是否含处理。
pub fn is_process(kinds: &[PluginKind]) -> bool {
    kinds.contains(&PluginKind::Process)
}

/// 单行严格校验（一次只报第一个违例：拒绝采信的代价远小于「不知道哪条丢了」）。
pub fn parse_row(kinds: &[PluginKind], value: &Value) -> Result<ResultRow, RowViolation> {
    parse_row_with(kinds, value, None)
}

/// 同上，但允许宿主补 `assetId`：宿主本来就知道自己在判哪张图，
/// 包漏回 `assetId` 时用请求里的资产 id 补齐（不因一个可推导的字段整批作废）。
pub fn parse_row_with(
    kinds: &[PluginKind],
    value: &Value,
    fallback_asset_id: Option<&str>,
) -> Result<ResultRow, RowViolation> {
    let obj = value.as_object().ok_or(RowViolation::NotObject)?;
    let asset_id = parse_asset_id(obj, fallback_asset_id).ok_or(RowViolation::AssetIdMissing)?;
    let raw_result = obj
        .get("result")
        .and_then(Value::as_str)
        .ok_or(RowViolation::ResultMissing)?;
    let result = ResultStatus::parse(raw_result)
        .ok_or_else(|| RowViolation::ResultInvalid(raw_result.to_string()))?;

    let rule_id = obj
        .get("ruleId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let verdict = match obj.get("verdict") {
        None | Some(Value::Null) => None,
        Some(raw) => Some(parse_verdict(raw)?),
    };
    // 处理类插件带 verdict → 拒收（这一条是反向验证的靶子：去掉它测试必须 FAIL）
    if verdict.is_some() && !is_judge(kinds) {
        return Err(RowViolation::VerdictNotAllowed);
    }

    let error = match obj.get("error") {
        None | Some(Value::Null) => None,
        Some(raw) => Some(parse_error(raw)?),
    };
    let skip_reason = obj
        .get("skipReason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    // verdict 的强制面按包的 kinds 结构性判定（不靠猜）：
    // * 纯判定包（kinds=[judge]）：任何 result=ok 的行都必须是判定行 → verdict 必填；
    // * 混合包（judge+process）：带 `ruleId` 的行是判定行（verdict 必填），
    //   不带 `ruleId` 的行是资产级处理行（verdict 可选）；
    // * 纯处理包：verdict 一律不许出现（上面已拒收）。
    let judge_only = is_judge(kinds) && !is_process(kinds);
    match result {
        ResultStatus::Failed if error.is_none() => return Err(RowViolation::ErrorMissing),
        ResultStatus::Skipped if skip_reason.is_none() => {
            return Err(RowViolation::SkipReasonMissing)
        }
        ResultStatus::Ok
            if is_judge(kinds) && verdict.is_none() && (judge_only || rule_id.is_some()) =>
        {
            return Err(RowViolation::VerdictMissing)
        }
        _ => {}
    }

    let artifacts = match obj.get("artifacts") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(parse_artifact)
            .collect::<Result<Vec<_>, _>>()?,
        Some(other) => {
            return Err(RowViolation::ArtifactInvalid(format!(
                "artifacts 必须是数组，收到 {}",
                kind_of(other)
            )))
        }
    };

    Ok(ResultRow {
        asset_id,
        rule_id,
        result,
        verdict,
        error,
        skip_reason,
        artifacts,
        evidence: parse_map(obj.get("evidence"), RowViolation::EvidenceNotObject)?,
        fields: parse_map(obj.get("fields"), RowViolation::FieldsNotObject)?,
    })
}

fn parse_asset_id(obj: &Map<String, Value>, fallback: Option<&str>) -> Option<String> {
    // `assetId` 是新契约键；`asset_id` 是协议 §3.3 judge/repair 应答的原始键（旧包在用）。
    let Some(raw) = obj.get("assetId").or_else(|| obj.get("asset_id")) else {
        return fallback
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    };
    match raw {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn parse_verdict(raw: &Value) -> Result<RowVerdict, RowViolation> {
    let obj = raw.as_object().ok_or_else(|| {
        RowViolation::VerdictInvalid(format!("verdict 不是对象：{}", kind_of(raw)))
    })?;
    let raw_status = obj
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| RowViolation::VerdictInvalid("verdict 缺 status".into()))?;
    let status = VerdictStatus::parse(raw_status)
        .ok_or_else(|| RowViolation::VerdictStatusInvalid(raw_status.to_string()))?;
    let confidence = match obj.get("confidence") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => {
            let v = n.as_f64().unwrap_or(f64::NAN);
            if !(0.0..=1.0).contains(&v) {
                return Err(RowViolation::VerdictConfidenceInvalid(v));
            }
            Some(v)
        }
        Some(other) => {
            return Err(RowViolation::VerdictInvalid(format!(
                "confidence 必须是数字或 null，收到 {}",
                kind_of(other)
            )))
        }
    };
    Ok(RowVerdict {
        status,
        confidence,
        reason_code: obj
            .get("reasonCode")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

fn parse_error(raw: &Value) -> Result<RowError, RowViolation> {
    let obj = raw.as_object().ok_or(RowViolation::ErrorMissing)?;
    // KI-40：`code` 首选稳定词字符串；数字码（包把 RPC 闭集码带进行里）也收 ——
    // 转成字符串聚合，不为一个可归一化的字段把整条失败原因丢成 row_error_missing。
    // （upscale 1.0.1 起包侧已改词，这里兜的是已装出去的第三方包。）
    let code = match obj.get("code") {
        Some(Value::String(s)) => {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
    .ok_or(RowViolation::ErrorMissing)?;
    let message = obj
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(RowViolation::ErrorMissing)?;
    Ok(RowError {
        code,
        message: message.to_string(),
    })
}

fn parse_artifact(raw: &Value) -> Result<RowArtifact, RowViolation> {
    let obj = raw
        .as_object()
        .ok_or_else(|| RowViolation::ArtifactInvalid(format!("产物不是对象：{}", kind_of(raw))))?;
    let kind = obj
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| RowViolation::ArtifactInvalid("产物缺 kind".into()))?;
    let need = |key: &str| -> Result<String, RowViolation> {
        obj.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| RowViolation::ArtifactInvalid(format!("{kind} 产物缺 {key}")))
    };
    match kind {
        "working_version" => Ok(RowArtifact::WorkingVersion {
            asset_id: need("assetId")?,
            version_id: need("versionId")?,
        }),
        "export_file" => Ok(RowArtifact::ExportFile {
            path: need("path")?,
            sha256: need("sha256")?,
        }),
        other => Err(RowViolation::ArtifactInvalid(format!(
            "kind={other:?} 不是 working_version/export_file（插件不许自建目录树或平行资产表）"
        ))),
    }
}

fn parse_map(raw: Option<&Value>, err: RowViolation) -> Result<Map<String, Value>, RowViolation> {
    match raw {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(m)) => Ok(m.clone()),
        Some(_) => Err(err),
    }
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// 宿主侧「调用失败」行（N19）：处理类插件**调用就失败**时也要出一行**结构化**结果，
/// 而不是一句中文 —— 尤其超时要能读出 `error.code=timeout` + 预算/实耗/建议档。
///
/// 为什么由宿主造行而不是让包回：超时/崩溃时包根本没机会回应答（进程已被 kill），
/// 这一行的唯一可信来源是宿主。形状与包自己回的失败行完全一致（`result=failed` +
/// `error{code,message}`），下游 `parse_row` / 落库 / 界面不用分两条路。
pub fn failure_row(asset_id: &str, err: &crate::PackHostError) -> Value {
    let mut error = serde_json::json!({
        "code": err.error_code(),
        "message": err.to_string(),
    });
    if let Some(info) = err.timeout_info() {
        // 超时的机读事实（预算/实耗/建议档/最后一段包内日志）
        error["detail"] = info.to_json();
        error["retryable"] = serde_json::json!(true);
    }
    serde_json::json!({
        "assetId": asset_id,
        "result": "failed",
        "error": error,
    })
}

/// 整个应答 → 结果行（宿主唯一的归一化入口）。
///
/// 三种应答形态都收：
/// 1. `{"rows":[...]}` —— 新契约（处理类按资产出行、判定类按规则出行）；
/// 2. `{"row":{...}}` —— 单行简写；
/// 3. `{"asset_id":..,"verdicts":[Verdict...]}` —— 旧判定应答，**逐规则 1:1 归一化**
///    （与 `rule_verdict` 表现有行一一对应，N5b 的 backfill 就是这条映射）。
///
/// 处理类（kinds 不含 judge）拿到 2/3 里的 verdict 一律 [`RowViolation::VerdictNotAllowed`]。
pub fn rows_from_reply(
    kinds: &[PluginKind],
    reply: &Value,
) -> Result<Vec<ResultRow>, RowViolation> {
    rows_from_reply_with(kinds, reply, None)
}

/// 同 [`rows_from_reply`]，但包漏回 `assetId` 时用宿主已知的资产 id 补齐。
pub fn rows_from_reply_with(
    kinds: &[PluginKind],
    reply: &Value,
    fallback_asset_id: Option<&str>,
) -> Result<Vec<ResultRow>, RowViolation> {
    let obj = reply.as_object().ok_or(RowViolation::NotObject)?;
    if let Some(Value::Array(items)) = obj.get("rows") {
        return items
            .iter()
            .map(|v| parse_row_with(kinds, v, fallback_asset_id))
            .collect();
    }
    if let Some(raw) = obj.get("row") {
        return Ok(vec![parse_row_with(kinds, raw, fallback_asset_id)?]);
    }
    if let Some(Value::Array(verdicts)) = obj.get("verdicts") {
        return legacy_verdict_rows(kinds, obj, verdicts, fallback_asset_id);
    }
    Err(RowViolation::ResultMissing)
}

/// 旧判定应答 → 结果行（每条 Verdict 一行：`result=ok` + 该规则的 verdict）。
///
/// 归一化口径（写进协议文档，别在 N5b 里另立一套）：
/// * `pass`/`fail` → `result=ok` + `verdict{status, confidence=score 夹到 0..=1}`；
/// * `undetermined` → `result=ok` + `verdict{status:undetermined}`（**有结论**：判不了）；
/// * `skipped` → `result=skipped` + `skipReason`（无结论，不冒充）；
/// * `label`/`note` 原文进 `evidence`（人读文本不塞进机读的 `reasonCode`）。
fn legacy_verdict_rows(
    kinds: &[PluginKind],
    reply: &Map<String, Value>,
    verdicts: &[Value],
    fallback_asset_id: Option<&str>,
) -> Result<Vec<ResultRow>, RowViolation> {
    if !is_judge(kinds) {
        return Err(RowViolation::VerdictNotAllowed);
    }
    let asset_id = parse_asset_id(reply, fallback_asset_id).ok_or(RowViolation::AssetIdMissing)?;
    let mut rows = Vec::with_capacity(verdicts.len());
    for v in verdicts {
        let obj = v.as_object().ok_or_else(|| {
            RowViolation::VerdictInvalid(format!("verdict 不是对象：{}", kind_of(v)))
        })?;
        let rule_id = obj
            .get("ruleId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| RowViolation::VerdictInvalid("verdict 缺 ruleId".into()))?;
        let raw_status = obj
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| RowViolation::VerdictInvalid(format!("{rule_id} 缺 status")))?;
        let mut evidence = Map::new();
        for key in ["label", "note", "evidence"] {
            if let Some(val) = obj.get(key) {
                let empty =
                    val.is_null() || val.as_str().map(|s| s.trim().is_empty()).unwrap_or(false);
                if !empty {
                    evidence.insert(key.to_string(), val.clone());
                }
            }
        }
        let mut row = ResultRow {
            asset_id: asset_id.clone(),
            rule_id: Some(rule_id.to_string()),
            result: ResultStatus::Ok,
            verdict: None,
            error: None,
            skip_reason: None,
            artifacts: Vec::new(),
            evidence,
            fields: Map::new(),
        };
        if raw_status == "skipped" {
            row.result = ResultStatus::Skipped;
            row.skip_reason = Some(
                obj.get("note")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("rule_skipped")
                    .to_string(),
            );
        } else {
            let status = VerdictStatus::parse(raw_status)
                .ok_or_else(|| RowViolation::VerdictStatusInvalid(raw_status.to_string()))?;
            let confidence = obj
                .get("score")
                .and_then(Value::as_f64)
                .map(|s| s.clamp(0.0, 1.0));
            // `reasonCode`：机读码，N9 起判定行恒等**规则 id**（规格只改阈值/启用集，
            // 不改 ruleId）→ 老形态没带就回落 ruleId；老宿主不读该字段，行为不变。
            let reason_code = obj
                .get("reasonCode")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| Some(rule_id.to_string()));
            row.verdict = Some(RowVerdict {
                status,
                confidence,
                reason_code,
            });
        }
        rows.push(row);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn judge() -> Vec<PluginKind> {
        vec![PluginKind::Judge]
    }

    fn process() -> Vec<PluginKind> {
        vec![PluginKind::Process]
    }

    #[test]
    fn closed_sets_are_three_and_stable() {
        assert_eq!(ResultStatus::all(), ["ok", "failed", "skipped"]);
        assert_eq!(VerdictStatus::all(), ["pass", "fail", "undetermined"]);
        assert_eq!(PluginKind::all(), ["judge", "process"]);
        for s in ResultStatus::all() {
            assert_eq!(ResultStatus::parse(s).unwrap().as_str(), s);
        }
        for s in VerdictStatus::all() {
            assert_eq!(VerdictStatus::parse(s).unwrap().as_str(), s);
        }
        // `skipped` 是执行结果，绝不是审核结论（自造状态 = 前端不可渲染）
        assert!(VerdictStatus::parse("skipped").is_none());
        assert!(PluginKind::parse("analyzer").is_none());
    }

    #[test]
    fn process_row_ok_without_verdict_roundtrips() {
        let raw = json!({
            "assetId": 12345,
            "result": "ok",
            "artifacts": [{"kind":"working_version","assetId":"12345","versionId":"v3"}],
            "evidence": {"elapsed_ms": 812},
            "fields": {"out_width": 1600}
        });
        let row = parse_row(&process(), &raw).unwrap();
        assert_eq!(row.asset_id, "12345");
        assert_eq!(row.result, ResultStatus::Ok);
        assert!(row.verdict.is_none());
        assert_eq!(row.artifacts.len(), 1);
        let back = serde_json::to_value(&row).unwrap();
        assert_eq!(back["assetId"], "12345");
        assert!(
            back.get("verdict").is_none(),
            "处理类不得凭空长出 verdict：{back}"
        );
        assert_eq!(back["fields"]["out_width"], 1600);
    }

    #[test]
    fn process_row_with_verdict_is_rejected() {
        let raw = json!({
            "assetId": 7, "result": "ok",
            "verdict": {"status": "pass", "confidence": 1.0}
        });
        let err = parse_row(&process(), &raw).unwrap_err();
        assert_eq!(err, RowViolation::VerdictNotAllowed);
        assert_eq!(err.code(), "row_verdict_not_allowed");
    }

    #[test]
    fn judge_ok_without_verdict_is_rejected() {
        let err = parse_row(&judge(), &json!({"assetId": 1, "result": "ok"})).unwrap_err();
        assert_eq!(err, RowViolation::VerdictMissing);
    }

    #[test]
    fn judge_failed_row_needs_error_not_verdict() {
        let no_error = parse_row(&judge(), &json!({"assetId": 1, "result": "failed"})).unwrap_err();
        assert_eq!(no_error, RowViolation::ErrorMissing);
        let ok = parse_row(
            &judge(),
            &json!({"assetId": 1, "result": "failed",
                    "error": {"code": "asset_unreadable", "message": "解码失败"}}),
        )
        .unwrap();
        assert_eq!(ok.error.unwrap().code, "asset_unreadable");
        assert!(ok.verdict.is_none());
    }

    #[test]
    fn skipped_row_needs_reason() {
        assert_eq!(
            parse_row(&process(), &json!({"assetId": 1, "result": "skipped"})).unwrap_err(),
            RowViolation::SkipReasonMissing
        );
        let row = parse_row(
            &process(),
            &json!({"assetId": 1, "result": "skipped", "skipReason": "原图缺失"}),
        )
        .unwrap();
        assert_eq!(row.skip_reason.as_deref(), Some("原图缺失"));
    }

    #[test]
    fn invented_status_is_rejected() {
        let err = parse_row(&process(), &json!({"assetId": 1, "result": "optimized"})).unwrap_err();
        assert_eq!(err.code(), "row_result_invalid");
        // 判定类把 `skipped` 写进 verdict.status 也越界（它是执行结果，不是结论）
        let err = parse_row(
            &judge(),
            &json!({"assetId": 1, "result": "ok", "verdict": {"status": "skipped"}}),
        )
        .unwrap_err();
        assert_eq!(err.code(), "row_verdict_status_invalid");
    }

    #[test]
    fn confidence_must_be_in_unit_range() {
        let err = parse_row(
            &judge(),
            &json!({"assetId": 1, "result": "ok",
                    "verdict": {"status": "pass", "confidence": 1.4}}),
        )
        .unwrap_err();
        assert!(matches!(err, RowViolation::VerdictConfidenceInvalid(_)));
    }

    #[test]
    fn artifacts_only_two_destinations() {
        for bad in [
            json!({"kind":"own_dir","path":"x"}),
            json!({"kind":"working_version","assetId":"1"}),
            json!({"kind":"export_file","path":"exports/a.png"}),
            json!("not-an-object"),
        ] {
            let err = parse_row(
                &process(),
                &json!({"assetId": 1, "result": "ok", "artifacts": [bad]}),
            )
            .unwrap_err();
            assert_eq!(err.code(), "row_artifact_invalid", "{err}");
        }
        let row = parse_row(
            &process(),
            &json!({"assetId": 1, "result": "ok", "artifacts": [
                {"kind":"export_file","path":"exports/a.png","sha256":"ab12"}]}),
        )
        .unwrap();
        assert_eq!(
            row.artifacts[0],
            RowArtifact::ExportFile {
                path: "exports/a.png".into(),
                sha256: "ab12".into()
            }
        );
    }

    #[test]
    fn evidence_and_fields_must_be_objects() {
        assert_eq!(
            parse_row(
                &process(),
                &json!({"assetId":1,"result":"ok","evidence":[]})
            )
            .unwrap_err(),
            RowViolation::EvidenceNotObject
        );
        assert_eq!(
            parse_row(&process(), &json!({"assetId":1,"result":"ok","fields":"x"})).unwrap_err(),
            RowViolation::FieldsNotObject
        );
    }

    #[test]
    fn legacy_reply_normalises_one_row_per_rule() {
        let reply = json!({
            "asset_id": "asset-42",
            "verdicts": [
                {"ruleId":"A4","status":"fail","score":0.31,"label":"主体占比不足","evidence":{"fg":0.31},"fixed":false,"note":"ratio<0.55"},
                {"ruleId":"A5","status":"pass","score":0.97,"label":"无叠加文字","evidence":{},"fixed":false,"note":null},
                {"ruleId":"A6","status":"skipped","score":null,"label":"","evidence":{},"fixed":false,"note":"fix_policy=judge_only"}
            ]
        });
        let rows = rows_from_reply(&judge(), &reply).unwrap();
        assert_eq!(rows.len(), 3, "逐规则一行");
        assert_eq!(rows[0].rule_id.as_deref(), Some("A4"));
        assert_eq!(rows[0].result, ResultStatus::Ok);
        assert_eq!(
            rows[0].verdict.as_ref().unwrap().status,
            VerdictStatus::Fail
        );
        assert!((rows[0].verdict.as_ref().unwrap().confidence.unwrap() - 0.31).abs() < 1e-9);
        assert_eq!(
            rows[1].verdict.as_ref().unwrap().status,
            VerdictStatus::Pass
        );
        assert_eq!(rows[2].result, ResultStatus::Skipped);
        assert_eq!(
            rows[2].skip_reason.as_deref(),
            Some("fix_policy=judge_only")
        );
        assert!(rows[2].verdict.is_none());
        // 人读文本进 evidence，不塞进机读 reasonCode
        assert_eq!(rows[0].evidence["label"], "主体占比不足");
        assert_eq!(rows[0].evidence["note"], "ratio<0.55");
        // N9：老形态没带 reasonCode → 回落规则 id（机读码恒等 ruleId，规格只改阈值不改 id）
        assert_eq!(
            rows[0].verdict.as_ref().unwrap().reason_code.as_deref(),
            Some("A4")
        );
        assert_eq!(
            rows[1].verdict.as_ref().unwrap().reason_code.as_deref(),
            Some("A5")
        );
    }

    #[test]
    fn legacy_reply_to_process_pack_is_rejected() {
        let reply = json!({"asset_id": "asset-1",
            "verdicts": [{"ruleId":"A4","status":"pass","score":1.0,"label":"","evidence":{},"fixed":false,"note":null}]});
        assert_eq!(
            rows_from_reply(&process(), &reply).unwrap_err(),
            RowViolation::VerdictNotAllowed
        );
    }

    #[test]
    fn new_rows_reply_is_parsed_and_validated() {
        let reply = json!({"rows": [
            {"assetId": "a1", "result": "ok", "verdict": {"status": "undetermined", "reasonCode": "vl_unavailable"}},
            {"assetId": "a2", "result": "ok", "verdict": {"status": "pass", "confidence": 0.99}}
        ]});
        let rows = rows_from_reply(&judge(), &reply).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].verdict.as_ref().unwrap().reason_code.as_deref(),
            Some("vl_unavailable")
        );
        // 同一应答给处理类包：第二行合法、第一行越界 → 整批拒收（不部分采信）
        assert_eq!(
            rows_from_reply(&process(), &reply).unwrap_err(),
            RowViolation::VerdictNotAllowed
        );
    }

    #[test]
    fn fallback_asset_id_fills_missing_reply_field() {
        let reply = json!({"verdicts": [{"ruleId":"R1","status":"pass","score":1.0,"label":"","evidence":{},"fixed":false,"note":null}]});
        assert_eq!(
            rows_from_reply(&judge(), &reply).unwrap_err(),
            RowViolation::AssetIdMissing
        );
        let rows = rows_from_reply_with(&judge(), &reply, Some("asset-99")).unwrap();
        assert_eq!(rows[0].asset_id, "asset-99");
    }

    #[test]
    fn empty_reply_is_a_violation_not_an_ok_row() {
        assert_eq!(
            rows_from_reply(&judge(), &json!({"asset_id":"a1"})).unwrap_err(),
            RowViolation::ResultMissing
        );
        assert_eq!(
            rows_from_reply(&judge(), &json!({"asset_id":"a1","verdicts":[]}))
                .unwrap()
                .len(),
            0,
            "空数组 = 没有行（上层按覆盖度另判），不要在这里造 pass"
        );
    }
}
