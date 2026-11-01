//! N6e（卡 `t_b6c2aa1e`）宿主侧**动作链**编排 —— 一条 job 串多个动作，只落一张工作版本。
//!
//! ## 为什么链在宿主、不在插件
//!
//! `plugin-design-and-ui.md` §8.1：**插件之间、动作之间不互相调用**。一个动作 = 一次
//! `judge` 调用（`options.action` 选一个），链的顺序/线程/清理/预算是**宿主职责**。
//! 好处是插件作者只写算子，用户能把任意动作拼成规格（先裁对 → 再压小 → 最后加水印）。
//!
//! ## 语义（本模块就是口径本身）
//!
//! * **线程**：第 i+1 个动作的 `asset.path` = 第 i 个动作的产物（`fields.outPath`）；
//!   一步失败整链终止，不跳步、不回退（容错不是这里的活）。
//! * **只留一张**：只有**链尾**动作的产物落成资产的工作版本；中间产物在下一步成功读到后
//!   立即删除（失败也不留半成品）。`packs/plugins/imaging/plugin/geometry.py` 的
//!   crop/letterbox 自己也是同一口径（中间产物用完即删）。
//! * **无产物动作**（`rename`）：只回 `fields.assetName`，不产文件。它**不能做链首**
//!   （链首要产出生意可用的图），做链尾时 = 前一动作的产物 + 资产名改写。
//! * **`skipped` 步**（插件判定本步不需要动，如「只缩不放」遇到小图）：不算失败 ——
//!   该步无产物，链用上一步的图继续（这正是「不放大」的正确表达，不是错误）。
//! * **预算**：`manifest.timeout_secs × 动作数`，上界 [`CHAIN_BUDGET_CAP_SECS`] 夹住；
//!   每一步本身仍受 `manifest.timeout_secs` 约束（`PackHost::call`），此处守总预算。
//! * **取消**：每步开始前看 `cancel`，已产出的中间产物全部删掉，返回 `cancelled`。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::compliance_queue::{PackIdentityInfo, PackJudge, PluginAsset};

/// 一条链最多几个动作（再长就该拆规格，而不是让一次 job 跑十分钟）。
pub const CHAIN_MAX_ACTIONS: usize = 8;
/// 总预算上界（`manifest.timeout_secs × 动作数` 不超过它）。
pub const CHAIN_BUDGET_CAP_SECS: u64 = 3600;
/// 缺省总预算（identity 没给 timeout 时的兜底，与协议 v1.1 的 60s 单步预算同量级）。
pub const CHAIN_DEFAULT_STEP_SECS: u64 = 60;

/// 一个动作的执行证据（= 结果行 `evidence.chain[]` 的一项）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainStep {
    /// 请求的动作名（顺序即执行序）
    pub action: String,
    /// 插件这一行的 `result`：`ok` / `skipped`（失败步也留一项，`error` 在链级）
    pub result: String,
    /// 宿主端点（插件 `fields.op`；没有就不编）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    /// 本步插件往返耗时（ms）
    pub ms: u64,
    /// 本步产物（相对/绝对路径原样，来自插件 `fields.outPath`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// `rename` 步：资产名改写结果（不产文件）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_name: Option<String>,
    /// `result=skipped` 的 `skipReason`（插件说「本步不用动」的理由）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    /// `result=failed` 的 `error{code,message}`：**插件自己给的可执行原因**（KI40-FIX）
    ///
    /// 不能丢：宿主兜底文案（「未返回产物」）对用户没有任何可照做的信息，
    /// 而插件给的「目标 36264×13040 超上限，请调小 scale」才是他要的答案。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ChainStepError>,
    /// 插件这一行的 `evidence` 原样搬出（失败时的限值/尺寸/步骤都在里面）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<serde_json::Value>,
}

/// 结果行里的结构化失败原因（协议 §4.4 `error{code,message}`，行级必带）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainStepError {
    /// 插件自定的机读码（如 `target_over_limit` / `invalid_params`）
    pub code: String,
    pub message: String,
}

/// 链尾产物 = 资产的工作版本（只此一张）。
#[derive(Debug, Clone, PartialEq)]
pub struct ChainArtifact {
    pub path: String,
    pub sha256: String,
    pub bytes: Option<i64>,
    pub format: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
}

/// 链级失败（`errorCode` 稳定词；`action` 指向失败的那一步）。
#[derive(Debug, Clone, PartialEq)]
pub struct ChainFailure {
    /// 失败动作（空 = 整链前置失败，如空链）
    pub action: String,
    /// 稳定词：`empty_chain` / `plugin_failure` / `action_failed` / `action_no_product`
    /// / `cancelled` / `timeout`
    pub code: &'static str,
    /// 插件失败的归档原因（`plugin_timeout` / `plugin_crash` …）；
    /// `action_failed` 时 = **插件自己给的机读码**（`target_over_limit` …）；其余为空
    pub kind: String,
    pub message: String,
}

impl ChainFailure {
    fn new(action: &str, code: &'static str, kind: &str, message: impl Into<String>) -> Self {
        Self {
            action: action.to_string(),
            code,
            kind: kind.to_string(),
            message: message.into(),
        }
    }

    /// 外部构造（调用方兜底用；正常路径由 [`run_chain`] 产出）。
    pub fn from_message(
        action: &str,
        code: &'static str,
        kind: &str,
        message: impl Into<String>,
    ) -> Self {
        Self::new(action, code, kind, message)
    }
}

/// 一次链的结局（成功 = `artifact` 有值且 `failure` 为空）。
#[derive(Debug, Clone, Default)]
pub struct ChainOutcome {
    pub steps: Vec<ChainStep>,
    /// 链尾产物（只有成功才有）
    pub artifact: Option<ChainArtifact>,
    /// 链尾 `rename` 的资产名改写（无 = None）
    pub asset_name: Option<String>,
    pub failure: Option<ChainFailure>,
    pub duration_ms: u64,
    pub cancelled: bool,
}

impl ChainOutcome {
    /// 逐步证据（落 `asset_versions.chain`；失败也保留已完成的部分）。
    pub fn evidence(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "chain": self.steps,
            "durationMs": self.duration_ms,
            "cancelled": self.cancelled,
        });
        // 失败原因也进证据：链级 `errorCode` + 插件原文，复盘时不必再翻 lastError
        if let Some(f) = &self.failure {
            v["error"] = serde_json::json!({
                "action": f.action,
                "code": f.code,
                "kind": f.kind,
                "message": f.message,
            });
        }
        v
    }

    pub fn ok(&self) -> bool {
        self.failure.is_none() && self.artifact.is_some()
    }
}

/// 链的总预算（`timeout_secs × 动作数`，夹到 `[1, CAP]`）。
pub fn chain_budget(identity: &PackIdentityInfo, actions: usize) -> Duration {
    let per = if identity.timeout_secs == 0 {
        CHAIN_DEFAULT_STEP_SECS
    } else {
        identity.timeout_secs
    };
    let total = per
        .saturating_mul(actions.max(1) as u64)
        .clamp(1, CHAIN_BUDGET_CAP_SECS);
    Duration::from_secs(total)
}

/// 动作清单校验（路由层用它换 400 点名，不静默丢动作）。
///
/// 规则：
/// 1. 空链 → 拒（处理类 job 必须至少一个动作）；
/// 2. 超过 [`CHAIN_MAX_ACTIONS`] → 拒（该拆规格）；
/// 3. 动作必须在插件 `pack.info.actions[].id` 里 → 否则点名（附插件声明的清单）；
/// 4. `rename` 不能做链首（它不产文件，链首会没有输入给下一步）。
pub fn validate_actions(actions: &[String], allowed: &[String]) -> Result<(), String> {
    if actions.is_empty() {
        return Err("动作链为空：至少要给一个动作".to_string());
    }
    if actions.len() > CHAIN_MAX_ACTIONS {
        return Err(format!(
            "动作链太长（{} 个，上限 {CHAIN_MAX_ACTIONS}）：请拆成多条规格",
            actions.len()
        ));
    }
    for (i, a) in actions.iter().enumerate() {
        if !allowed.iter().any(|x| x == a) {
            return Err(format!(
                "未知动作 `{a}`（插件声明的动作：{}）",
                if allowed.is_empty() {
                    "无".to_string()
                } else {
                    allowed.join(" / ")
                }
            ));
        }
        if i == 0 && a == "rename" {
            return Err("`rename` 不产生产物，不能做动作链的首个动作（放到链尾）".to_string());
        }
    }
    Ok(())
}

/// 跑一条链。**不碰数据库**：产物路径/证据全在返回值里，落库由调用方负责。
///
/// 失败/取消/超时都会把本次已产出的文件删干净（包括失败步的产物），
/// 因此调用方拿到 `failure` 时磁盘上不会留半成品。
pub fn run_chain(
    plugin: &dyn PackJudge,
    base: &PluginAsset,
    actions: &[String],
    out_dir: &Path,
    cancel: &AtomicBool,
) -> ChainOutcome {
    let started = Instant::now();
    let identity = plugin.identity();
    let budget = chain_budget(&identity, actions.len());
    let mut out = ChainOutcome::default();

    if actions.is_empty() {
        out.failure = Some(ChainFailure::new("", "empty_chain", "", "动作链为空"));
        return out;
    }

    let mut current = base.path.clone();
    // 本次已产出的文件（顺序 = 动作序）。成功时最后一个 = 工作版本，其余删掉。
    let mut produced: Vec<PathBuf> = Vec::new();

    for action in actions {
        if cancel.load(Ordering::SeqCst) {
            out.cancelled = true;
            out.failure = Some(ChainFailure::new(action, "cancelled", "", "用户取消"));
            cleanup(&produced);
            out.duration_ms = started.elapsed().as_millis() as u64;
            return out;
        }
        if started.elapsed() > budget {
            out.failure = Some(ChainFailure::new(
                action,
                "timeout",
                "plugin_timeout",
                format!("链总预算 {}s 用尽", budget.as_secs()),
            ));
            cleanup(&produced);
            out.duration_ms = started.elapsed().as_millis() as u64;
            return out;
        }

        let req = PluginAsset {
            path: current.clone(),
            work_dir: out_dir.to_string_lossy().to_string(),
            ..base.clone()
        };
        let rep = plugin.action_asset(&req, action);
        let step = ChainStep::from_outcome(action, &rep);

        // 插件层失败（超时/崩溃/协议违例/业务错）：整链失败，已完成部分进证据
        if let Some((kind, detail)) = &rep.failure {
            out.steps.push(step.clone());
            out.failure = Some(ChainFailure::new(
                action,
                "plugin_failure",
                kind.reason(),
                detail.clone(),
            ));
            // 失败步可能已经写了一半产物（插件在返回失败前落的盘）：一并删掉，不留半成品
            if let Some(p) = &step.out_path {
                let _ = std::fs::remove_file(p);
            }
            cleanup(&produced);
            out.duration_ms = started.elapsed().as_millis() as u64;
            return out;
        }

        let skipped = step.result == "skipped";
        // skipped 行的 `fields.outPath` 是「打算写的路径」（没写盘，如 compress 的
        // already_within_target）：不采信，输入保持上一步的图继续 —— 采信它会让下一步
        // 拿到不存在的文件 404 断链（t_382fa012 行为验收实测）。
        let product = if step.result == "ok" {
            step.out_path.clone()
        } else {
            None
        };
        out.steps.push(step.clone());

        // **插件报了结构化失败**（`result=failed` + `error{code,message}`）：
        // 这是「包说这次做不了」的正常出口（目标超限/参数不合法/内存不足），
        // 不是「插件说 ok 却没产物」。以前这里直接落到下面的 `None =>` 分支，
        // 把插件给的可执行原因整段丢掉、只剩宿主兜底文案（KI40-FIX 的根因）。
        if step.result == "failed" {
            let (kind, message) = match &step.error {
                Some(e) => (e.code.clone(), e.message.clone()),
                // 契约要求 failed 必带 error；真撞上违例也要响亮，不退回兜底话术
                None => (
                    "plugin_error_missing".to_string(),
                    format!("动作 `{action}` 返回 failed 但没给 error{{code,message}}"),
                ),
            };
            out.failure = Some(ChainFailure::new(action, "action_failed", &kind, message));
            if let Some(p) = &step.out_path {
                let _ = std::fs::remove_file(p);
            }
            cleanup(&produced);
            out.duration_ms = started.elapsed().as_millis() as u64;
            return out;
        }

        match product {
            Some(p) => {
                current = p.clone();
                produced.push(PathBuf::from(p));
            }
            // 本步是「不需要动」（如小图不放大）或「只改名不出字节」（rename）：
            // 不产文件，输入保持上一步的图继续（模块头第 3 条）
            None if skipped || step.asset_name.is_some() => {}
            None => {
                // 插件说 ok 却没产物：处理类插件不能凭空交付工作版本
                out.failure = Some(ChainFailure::new(
                    action,
                    "action_no_product",
                    "",
                    format!("动作 `{action}` 未返回产物（fields.outPath 缺失）"),
                ));
                cleanup(&produced);
                out.duration_ms = started.elapsed().as_millis() as u64;
                return out;
            }
        }
    }

    if started.elapsed() > budget {
        out.failure = Some(ChainFailure::new(
            "",
            "timeout",
            "plugin_timeout",
            format!("链总预算 {}s 用尽", budget.as_secs()),
        ));
        cleanup(&produced);
        out.duration_ms = started.elapsed().as_millis() as u64;
        return out;
    }

    // 收尾：链尾有产物 → 它当工作版本，其余（中间产物）立刻删；
    // 链尾是 rename/跳过 → 用最后一个有产物的步（rename 的特例见模块头）。
    let asset_name = out
        .steps
        .iter()
        .rev()
        .find_map(|s| s.asset_name.clone());
    match produced.pop() {
        Some(final_path) => {
            let tail = out
                .steps
                .iter()
                .rev()
                .find(|s| s.out_path.is_some())
                .cloned();
            let sha256 = tail
                .as_ref()
                .and_then(|s| s.sha256.clone())
                .unwrap_or_default();
            // 格式：插件给了就用；没给（如 watermark 只改像素不换容器）按产物扩展名兜底，
            // 不能让「工作版本」的格式字段空着 —— 下游导出/筛选都靠它。
            let format = tail
                .as_ref()
                .and_then(|s| s.format.clone())
                .or_else(|| {
                    Path::new(&final_path)
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.to_ascii_lowercase())
                });
            out.artifact = Some(ChainArtifact {
                path: final_path.to_string_lossy().to_string(),
                sha256,
                bytes: tail.as_ref().and_then(|s| s.bytes),
                format,
                width: tail.as_ref().and_then(|s| s.width),
                height: tail.as_ref().and_then(|s| s.height),
            });
            out.asset_name = asset_name;
            cleanup(&produced);
        }
        None => {
            out.asset_name = asset_name;
            out.failure = Some(ChainFailure::new(
                "",
                "action_no_product",
                "",
                "整条链没有任何动作产出文件（无可落库的工作版本）",
            ));
        }
    }
    out.duration_ms = started.elapsed().as_millis() as u64;
    out
}

/// 删掉一批文件（不存在/删不掉都不报错：清理是尽力而为，产物事实以 DB 为准）。
fn cleanup(paths: &[PathBuf]) {
    for p in paths {
        let _ = std::fs::remove_file(p);
    }
}

impl ChainStep {
    /// 从插件应答抽出一步证据（`rows[0].fields` 是协议 §4.4 的自定义列，宿主当不透明字典读）。
    fn from_outcome(action: &str, rep: &crate::compliance_queue::PluginOutcome) -> Self {
        let row = rep.rows.first();
        let fields = row
            .and_then(|r| r.get("fields"))
            .and_then(|f| f.as_object());
        let get = |k: &str| fields.and_then(|f| f.get(k));
        let out_arr = get("out").and_then(|v| v.as_array());
        let dim = |i: usize| out_arr.and_then(|a| a.get(i)).and_then(|v| v.as_i64());
        Self {
            action: action.to_string(),
            result: row
                .and_then(|r| r.get("result"))
                .and_then(|v| v.as_str())
                .unwrap_or(if rep.failure.is_some() { "failed" } else { "ok" })
                .to_string(),
            op: get("op").and_then(|v| v.as_str()).map(str::to_string),
            ms: rep.duration_ms,
            out_path: get("outPath").and_then(|v| v.as_str()).map(str::to_string),
            sha256: get("sha256").and_then(|v| v.as_str()).map(str::to_string),
            bytes: get("bytes")
                .or_else(|| get("outBytes"))
                .and_then(|v| v.as_i64()),
            width: dim(0),
            height: dim(1),
            format: get("format").and_then(|v| v.as_str()).map(str::to_string),
            asset_name: get("assetName")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            skip_reason: row
                .and_then(|r| r.get("skipReason"))
                .and_then(|v| v.as_str())
                .map(str::to_string),
            // 行级结构化失败（`error{code,message}`）原样带走 —— 后面的
            // `run_chain` 靠它把插件原因映射成 `action_failed`，不再兜底成
            // 「未返回产物」。传输层失败（rep.failure）没有行级 error，留 None。
            error: row
                .and_then(|r| r.get("error"))
                .and_then(|e| {
                    let code = e.get("code").and_then(|v| v.as_str())?;
                    Some(ChainStepError {
                        code: code.to_string(),
                        message: e
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    })
                })
                .or_else(|| {
                    rep.failure
                        .as_ref()
                        .map(|(kind, detail)| ChainStepError {
                            code: kind.reason().to_string(),
                            message: detail.clone(),
                        })
                }),
            evidence: row.and_then(|r| r.get("evidence")).cloned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::compliance_queue::{PluginFailure, PluginOutcome, PluginVerdict};

    /// 脚本化插件：按 `script` 顺序回帧；产物文件真写盘（清理断言要能看见）。
    struct MockJudge {
        identity: PackIdentityInfo,
        script: Arc<Mutex<Vec<ScriptedRow>>>,
        /// 每次调用的 (action, 输入 path)，按序记录 —— 验证「上一产物喂下一步」
        seen: Arc<Mutex<Vec<(String, String)>>>,
    }

    #[derive(Clone)]
    struct ScriptedRow {
        result: &'static str,
        product: Option<String>,
        asset_name: Option<String>,
        failure: Option<(PluginFailure, String)>,
        /// 行级结构化失败 `error{code,message}`（插件自报「这次做不了」）
        row_error: Option<(&'static str, &'static str)>,
        /// 行级 evidence（失败时的限值/尺寸都在这）
        evidence: Option<serde_json::Value>,
    }

    impl MockJudge {
        fn new(script: Vec<ScriptedRow>) -> Self {
            Self {
                identity: PackIdentityInfo {
                    id: "com.soujpg.pack.imaging".into(),
                    version: "1.0.0".into(),
                    plugin_hash: "hash".into(),
                    deterministic: true,
                    rule_ids: vec!["resize".into()],
                    outputs: vec!["artifact".into()],
                    actions: vec![
                        "resize".into(),
                        "crop".into(),
                        "convert".into(),
                        "compress".into(),
                        "watermark".into(),
                        "rename".into(),
                    ],
                    timeout_secs: 5,
                },
                script: Arc::new(Mutex::new(script)),
                seen: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl PackJudge for MockJudge {
        fn identity(&self) -> PackIdentityInfo {
            self.identity.clone()
        }

        fn judge_asset(&self, _req: &PluginAsset) -> PluginOutcome {
            panic!("链不该调 judge_asset")
        }

        fn repair_asset(&self, _req: &PluginAsset) -> PluginOutcome {
            panic!("处理类包不实现 repair")
        }

        fn action_asset(&self, req: &PluginAsset, action: &str) -> PluginOutcome {
            self.seen
                .lock()
                .unwrap()
                .push((action.to_string(), req.path.clone()));
            let row = self.script.lock().unwrap().remove(0);
            let mut fields = serde_json::Map::new();
            if let Some(p) = &row.product {
                std::fs::write(p, b"bytes").unwrap();
                fields.insert("outPath".into(), serde_json::json!(p));
                fields.insert("sha256".into(), serde_json::json!(format!("sha-{action}")));
                fields.insert("out".into(), serde_json::json!([100, 50]));
                fields.insert("format".into(), serde_json::json!("webp"));
                fields.insert("bytes".into(), serde_json::json!(5));
            }
            if let Some(n) = &row.asset_name {
                fields.insert("assetName".into(), serde_json::json!(n));
            }
            let mut row_json = serde_json::json!({
                "assetId": req.asset_id.to_string(),
                "result": row.result,
                "fields": fields,
            });
            if let Some((code, message)) = row.row_error {
                row_json["error"] = serde_json::json!({ "code": code, "message": message });
            }
            if let Some(ev) = &row.evidence {
                row_json["evidence"] = ev.clone();
            }
            PluginOutcome {
                pack: self.identity.clone(),
                verdicts: Vec::<PluginVerdict>::new(),
                artifacts: Vec::new(),
                rows: vec![row_json],
                duration_ms: 7,
                failure: row.failure,
                raw: serde_json::Value::Null,
            }
        }
    }

    fn base(path: &str) -> PluginAsset {
        PluginAsset {
            asset_id: 42,
            job_id: 7,
            path: path.to_string(),
            mime: None,
            width: 10,
            height: 10,
            work_dir: "/tmp".into(),
            rule_ids: vec![],
        }
    }

    fn ok_row(product: &str) -> ScriptedRow {
        ScriptedRow {
            result: "ok",
            product: Some(product.to_string()),
            asset_name: None,
            failure: None,
            row_error: None,
            evidence: None,
        }
    }

    /// 三个动作串一条链：后一步的输入 = 前一步的产物，只留链尾产物，中间产物被删。
    #[test]
    fn chain_threads_products_and_keeps_only_tail() {
        let dir = std::env::temp_dir().join(format!("n6e-chain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p1 = dir.join("step1.png");
        let p2 = dir.join("step2.webp");
        let p3 = dir.join("step3.png");
        let judge = MockJudge::new(vec![
            ok_row(&p1.to_string_lossy()),
            ok_row(&p2.to_string_lossy()),
            ok_row(&p3.to_string_lossy()),
        ]);
        let actions = vec!["resize".to_string(), "convert".to_string(), "watermark".to_string()];
        let out = run_chain(&judge, &base("/src/orig.jpg"), &actions, &dir, &AtomicBool::new(false));

        assert!(out.ok(), "链应成功：{:?}", out.failure);
        assert_eq!(out.steps.len(), 3);
        assert_eq!(
            out.steps.iter().map(|s| s.action.as_str()).collect::<Vec<_>>(),
            vec!["resize", "convert", "watermark"]
        );
        let seen = judge.seen.lock().unwrap().clone();
        assert_eq!(seen[0].1, "/src/orig.jpg", "第一步吃原图");
        assert_eq!(seen[1].1, p1.to_string_lossy(), "第二步吃第一步产物");
        assert_eq!(seen[2].1, p2.to_string_lossy(), "第三步吃第二步产物");
        let art = out.artifact.unwrap();
        assert_eq!(art.path, p3.to_string_lossy());
        assert_eq!(art.sha256, "sha-watermark");
        assert!(!p1.exists(), "中间产物用完即删");
        assert!(!p2.exists(), "中间产物用完即删");
        assert!(p3.exists(), "链尾产物保留");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 中途失败：整链 failed，已完成部分进证据，磁盘不留任何产物。
    #[test]
    fn failure_aborts_chain_and_leaves_no_files() {
        let dir = std::env::temp_dir().join(format!("n6e-chain-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p1 = dir.join("step1.png");
        let p2 = dir.join("step2.webp");
        let judge = MockJudge::new(vec![
            ok_row(&p1.to_string_lossy()),
            ScriptedRow {
                result: "failed",
                product: Some(p2.to_string_lossy().into()),
                asset_name: None,
                failure: Some((PluginFailure::RuleError, "水位参数非法".into())),
                row_error: None,
                evidence: None,
            },
        ]);
        let actions = vec!["resize".to_string(), "watermark".to_string()];
        let out = run_chain(&judge, &base("/src/orig.jpg"), &actions, &dir, &AtomicBool::new(false));

        assert!(!out.ok());
        let f = out.failure.unwrap();
        assert_eq!(f.action, "watermark", "错误指向失败动作");
        assert_eq!(f.code, "plugin_failure");
        assert_eq!(f.kind, "plugin_rule_error");
        assert_eq!(out.steps.len(), 2, "已完成部分保留");
        assert!(out.artifact.is_none(), "失败不落工作版本");
        assert!(!p1.exists() && !p2.exists(), "失败不留半成品");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 链尾 rename：不产文件，工作版本 = 前一动作产物 + 资产名改写。
    #[test]
    fn rename_tail_rewrites_name_on_previous_product() {
        let dir = std::env::temp_dir().join(format!("n6e-chain-rename-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p1 = dir.join("step1.png");
        let judge = MockJudge::new(vec![
            ok_row(&p1.to_string_lossy()),
            ScriptedRow {
                result: "ok",
                product: None,
                asset_name: Some("主图_800x800.png".into()),
                failure: None,
                row_error: None,
                evidence: None,
            },
        ]);
        let actions = vec!["resize".to_string(), "rename".to_string()];
        let out = run_chain(&judge, &base("/src/orig.jpg"), &actions, &dir, &AtomicBool::new(false));

        assert!(out.ok(), "{:?}", out.failure);
        assert_eq!(out.asset_name.as_deref(), Some("主图_800x800.png"));
        assert_eq!(out.artifact.unwrap().path, p1.to_string_lossy());
        assert!(p1.exists(), "rename 不改字节");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `skipped` 步（不吃不放的小图）不算失败：用上一步的图继续。
    #[test]
    fn skipped_step_passes_previous_product_through() {
        let dir = std::env::temp_dir().join(format!("n6e-chain-skip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p2 = dir.join("step2.webp");
        let judge = MockJudge::new(vec![
            ScriptedRow {
                result: "skipped",
                product: None,
                asset_name: None,
                failure: None,
                row_error: None,
                evidence: None,
            },
            ok_row(&p2.to_string_lossy()),
        ]);
        let actions = vec!["resize".to_string(), "convert".to_string()];
        let out = run_chain(&judge, &base("/src/orig.jpg"), &actions, &dir, &AtomicBool::new(false));

        assert!(out.ok(), "{:?}", out.failure);
        let seen = judge.seen.lock().unwrap().clone();
        assert_eq!(seen[1].1, "/src/orig.jpg", "跳过步不改输入");
        assert_eq!(out.artifact.unwrap().path, p2.to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// skipped 行**带 outPath** 也不许串链：那是「打算写的路径」，文件没落盘
    /// （imaging compress 的 already_within_target 实测把 watermark 404 断链，
    /// t_382fa012）。skipped 只认「上一步产物继续」这一种语义。
    #[test]
    fn skipped_row_with_outpath_is_not_threaded() {
        let dir = std::env::temp_dir().join(format!("n6e-chain-skip-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ghost = dir.join("step1.compress.jpg"); // 故意不创建：skipped 行没写盘
        let p2 = dir.join("step2.jpg");
        let judge = MockJudge::new(vec![
            ScriptedRow {
                result: "skipped",
                product: Some(ghost.to_string_lossy().into_owned()),
                asset_name: None,
                failure: None,
                row_error: None,
                evidence: None,
            },
            ok_row(&p2.to_string_lossy()),
        ]);
        let actions = vec!["compress".to_string(), "watermark".to_string()];
        let out = run_chain(&judge, &base("/src/orig.jpg"), &actions, &dir, &AtomicBool::new(false));

        assert!(out.ok(), "{:?}", out.failure);
        let seen = judge.seen.lock().unwrap().clone();
        assert_eq!(
            seen[1].1, "/src/orig.jpg",
            "skipped 行的 outPath 不许当下一步输入"
        );
        assert_eq!(out.artifact.unwrap().path, p2.to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 插件说 ok 却没产物 → 整链失败（不拿空产物冒充工作版本）。
    #[test]
    fn missing_product_fails_the_chain() {
        let dir = std::env::temp_dir().join(format!("n6e-chain-noprod-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let judge = MockJudge::new(vec![ScriptedRow {
            result: "ok",
            product: None,
            asset_name: None,
            failure: None,
            row_error: None,
            evidence: None,
        }]);
        let out = run_chain(&judge, &base("/src/orig.jpg"), &["crop".to_string()], &dir, &AtomicBool::new(false));
        assert_eq!(out.failure.unwrap().code, "action_no_product");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// KI40-FIX 反向验证靶子：插件给**结构化失败行**（`failed` + `error{code,message}`
    /// + evidence）时，宿主必须把插件原文与证据透传出去，且失败码是 `action_failed`。
    ///
    /// 退回旧行为（不认 `error`、只按「没 outPath」兜底）这条断言立刻变红：
    /// 旧代码给的是 `action_no_product` + 「未返回产物」，用户拿不到可照做的信息。
    #[test]
    fn failed_row_passes_plugin_reason_through() {
        let dir = std::env::temp_dir().join(format!("ki40-chain-reason-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let judge = MockJudge::new(vec![ScriptedRow {
            result: "failed",
            product: None,
            asset_name: None,
            failure: None,
            row_error: Some((
                "target_over_limit",
                "目标 36264×13040（473,102,880 像素）超产物上限：像素 ≤1e8、长边 ≤2e4；\
                 该源图可用上限 scale≤1.99；请调小 scale 或先缩小源图",
            )),
            evidence: Some(serde_json::json!({
                "inputWidth": 9066, "inputHeight": 3260,
                "expectedWidth": 36264, "expectedHeight": 13040,
                "limit": {"maxOutputPixels": 100000000, "maxLongEdge": 20000, "maxScale": 1.99},
            })),
        }]);
        let out = run_chain(&judge, &base("/src/big.jpg"), &["upscale".to_string()], &dir, &AtomicBool::new(false));

        let f = out.failure.clone().expect("失败行必须让链失败");
        assert_eq!(f.code, "action_failed", "不是宿主兜底的 action_no_product");
        assert_eq!(f.kind, "target_over_limit", "插件机读码原样带上");
        assert_eq!(f.action, "upscale");
        assert!(
            f.message.contains("目标 36264×13040") && f.message.contains("请调小 scale"),
            "插件原文必须透传，实际：{}",
            f.message
        );
        // 逐步证据里带上插件 error 与 evidence（结果表 fields.chain[i] 就读这里）
        let step = &out.steps[0];
        assert_eq!(step.result, "failed");
        let err = step.error.clone().expect("chain[0].error 不能丢");
        assert_eq!(err.code, "target_over_limit");
        assert!(err.message.contains("请调小 scale"));
        assert_eq!(
            step.evidence.as_ref().and_then(|e| e.get("limit")).and_then(|l| l.get("maxScale")),
            Some(&serde_json::json!(1.99)),
            "插件 evidence 原样搬出"
        );
        // 失败原因也进链级证据
        assert_eq!(out.evidence()["error"]["code"], "action_failed");
        assert_eq!(out.evidence()["error"]["kind"], "target_over_limit");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 取消：链停下、不留产物。
    #[test]
    fn cancel_between_steps_cleans_up() {
        let dir = std::env::temp_dir().join(format!("n6e-chain-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p1 = dir.join("step1.png");
        let judge = MockJudge::new(vec![
            ok_row(&p1.to_string_lossy()),
            ok_row(&dir.join("never.png").to_string_lossy()),
        ]);
        // 第一步前就取消 → 一步都不跑
        let out = run_chain(
            &judge,
            &base("/src/orig.jpg"),
            &["resize".to_string(), "convert".to_string()],
            &dir,
            &AtomicBool::new(true),
        );
        assert!(out.cancelled);
        assert_eq!(out.failure.unwrap().code, "cancelled");
        assert!(out.steps.is_empty());
        assert!(!p1.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 动作校验：未知动作点名 + rename 不许做链首 + 空链/超长。
    #[test]
    fn validate_actions_names_the_offender() {
        let allowed = vec!["resize".to_string(), "rename".to_string()];
        let e = validate_actions(&["blur".to_string()], &allowed).unwrap_err();
        assert!(e.contains("blur") && e.contains("resize"), "{e}");
        let e = validate_actions(&["rename".to_string(), "resize".to_string()], &allowed).unwrap_err();
        assert!(e.contains("rename"), "{e}");
        assert!(validate_actions(&[], &allowed).is_err());
        let long: Vec<String> = std::iter::repeat("resize".to_string())
            .take(CHAIN_MAX_ACTIONS + 1)
            .collect();
        assert!(validate_actions(&long, &allowed).is_err());
        assert!(validate_actions(&["resize".to_string()], &allowed).is_ok());
    }

    /// 总预算 = 单步预算 × 动作数，上界夹住。
    #[test]
    fn budget_is_per_step_times_actions_capped() {
        let mut id = MockJudge::new(vec![]).identity;
        id.timeout_secs = 120;
        assert_eq!(chain_budget(&id, 3).as_secs(), 360);
        id.timeout_secs = 3600;
        assert_eq!(
            chain_budget(&id, 8).as_secs(),
            CHAIN_BUDGET_CAP_SECS,
            "超上界按 CAP 夹住"
        );
        id.timeout_secs = 0;
        assert_eq!(chain_budget(&id, 1).as_secs(), CHAIN_DEFAULT_STEP_SECS);
    }
}
