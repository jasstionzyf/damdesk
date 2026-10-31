#![allow(non_snake_case)] // 本模块 payload/进度字段 = job_queue JSON 线上 camelCase 契约，勿改
//! C2 — compliance job + 管道编排器（PLAN.md §6.3 / compliance plan §C2）。
//!
//! `job_queue`(kind='compliance', targetId=assetId, payload={"packId","outDir"}) 驱动，
//! 与 embed/tag/custom/export 并列；复用 engine-jobq 断点续跑（`requeue_stale`）、
//! 重试上限（3 次转 failed）、stats 进度——kill 后重跑续接（同 job 重做前先清残行）。
//!
//! 编排器职责（同一编排器、同一插件注册表，step 双语义）：
//! - **judge 步骤**（规则）：按 pipeline.yaml 声明序执行（成本升序由 pack 作者声明、
//!   编排器强制；像素档解码惰性一次复用）。classic 检测器走 engine-classic 注册表；
//!   C3 起 local_model 走 engine-segment 注册表（u2net，feature 门控：web 容器
//!   executor_not_available 降级、桌面模型未下载 model_not_downloaded 降级）；
//!   vl v1 未接入 → verdict skipped（executor_not_available）；
//!   external_data → undetermined（不可自动判定：未判定）。
//!   fail → fixer（未注册 → undetermined（未判定））→ 复检 ≤ recheck_max 次：
//!   guaranteed 修完必过（仍败 = 异常，转人工兜底）；best_effort 仍败转人工。
//! - **do 步骤**（无条件变换）：插件链式执行（上一步产物 = 下一步输入），不判定，
//!   按序执行；未知插件 → job 失败（响亮，重试耗尽转 failed）。
//! - **短路**：步骤 depends_on 任一依赖规则 fail → 本步 skipped（证据记 short_circuited_by）。
//! - **每步写 rule_verdict**（含证据 json）；合格结论只来自本次检测器/插件输出或人工
//!   （已有标签不作为证据，判定可复现铁律）。
//! - 修复/变换产物写 job outDir（不入库，复检与导出用；D12 免费/付费分界沿用）。
//!
//! 数据访问全部经 core-storage [`ComplianceRepo`]（存储解耦裁决：本模块零 SQL）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use core_config::{FixerTier, Pack, Rule, RuleExecutor, RuleOnFail, StepKind};
use core_storage::compliance::{ComplianceRepo, NewVerdict, VerdictRow};
use core_storage::compliance_runs::ComplianceRunRepo;
use core_storage::compliance_runs_sqlite::SqliteComplianceRunRepo;
use core_storage::compliance_sqlite::SqliteComplianceRepo;
use engine_classic::registry::{self, DetectorInput, DetectorOutput, InputKind};
use engine_jobq::{self as jobq};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::compliance_plugins::{self, PluginOutput};
use crate::compliance_vl::{self, VlConfig, VlHandle, VlJudgeOut, VlSession};
use crate::{PipelineError, Result};

/// job kind（C2 冻结值；与 embed:<model>/custom_tag/export 并列）。
pub const COMPLIANCE_KIND: &str = "compliance";

/// C9：**组级 job** 的 kind —— 一个 job = **一个组 × 一个 group_by 作用域**（不是一张图）。
///
/// 为什么单开 kind（而不是复用 `compliance` + payload 判形态）：
/// - 资产口径的进度/统计（`compliance_stats`、`done+failed == 资产数`）保持纯净，
///   组级 job 不污染资产计数（回归口径 c6/c7 不变）；
/// - 组级 job 与资产 job 可分别重试、分别观测（组级失败大概率是 VL 通道抖动，不是某张图坏）。
///
/// targetId = **组锚点**（组内最小 assetId；job_queue 的行需要一个整型目标），
/// 真正的组目标（组键 + 成员列表 + 作用域）在 payload 里（见 [`GroupPayload`]）。
pub const COMPLIANCE_GROUP_KIND: &str = "compliance_group";

/// CMP-JOB-01：一次执行（run）覆盖的**全部** job kind。
///
/// 用途只有一个：给「本 run 的任务计数 / 收工时按队列回填统计」划边界。
/// 读侧（run 账本、界面 job 明细）与写侧（`finish`）必须用同一份清单，否则组级 job
/// 会从 run 的 total 里悄悄消失（`total = done+failed+skipped+cancelled` 立刻对不上）。
pub const COMPLIANCE_KINDS: [&str; 2] = [COMPLIANCE_KIND, COMPLIANCE_GROUP_KIND];

/// verdict 常量（组级与逐图共用）。
pub const VERDICT_SCOPE_GROUP: &str = "per_group";
/// verdict 常量（组级与逐图共用）。
pub const VERDICT_SCOPE_ASSET: &str = "per_asset";

/// 每批 claim 的 job 数（classic 档毫秒级，与 tag 队列同量级起步）。
pub const COMPLIANCE_BATCH: i64 = 32;

/// verdict result 常量（对齐 migration 0016 CHECK 约束：三态 + skipped）。
///
/// 没有任何「待人工 / 修复后合格 / 失败」这类派生词：修复成功落 pass（证据留 fixAction/
/// recheckResult），修不了/判不了落 fail 或 undetermined。
pub mod verdict {
    pub use core_storage::compliance::result::{FAIL, PASS, SKIPPED, UNDETERMINED};

    /// 合法结果词判定（IP-K05：插件应答的状态必须在闭集内才可采信）。
    pub fn is_valid(s: &str) -> bool {
        core_storage::compliance::result::is_valid(s)
    }
}

/// 规则置信度（0..1）：**每条规则自己声明来源**（rules.yaml `check.confidence`），
/// 装载期 lint 硬性要求（没声明的判定规则不许装载）。
///
/// - `detector_margin`：检测器给出的**判定余量**（|score − 阈值| / 阈值，clamp 到 [0,1] 后
///   折半映射到 [0.5,1]）—— 不是拿原始 score 冒充置信度；
/// - `fixed`：通道本身没有概率（VL 应答只有词表答案、组级检测器只给离散距离），
///   由 pack 显式声明一个固定值，界面据此进「低置信度复核」。
///
/// 无 check 的行（do 步骤 / skipped / 打标型）没有判定语义 → 1.0（「确定没有结论」）。
/// 该规则是否声明了 VL 自评置信度（`check.confidence.source: vl_self_report`）。
fn is_vl_self_report(rule: &Rule) -> bool {
    rule.check
        .as_ref()
        .and_then(|c| c.confidence.as_ref())
        .map(|c| c.source == core_config::ConfidenceSource::VlSelfReport)
        .unwrap_or(false)
}

/// VL 自评规则的判定行置信度：用该次判定的自评值；模型没给 → 0.0（最低，自动进低置信度复核队列）。
/// 非自评规则 → None（由 `with_rule_confidence` 按余量/常数算）。
fn self_report_conf(rule: &Rule, out: &VlJudgeOut) -> Option<f64> {
    if is_vl_self_report(rule) {
        Some(out.confidence.unwrap_or(0.0))
    } else {
        None
    }
}

fn verdict_confidence(rule: Option<&Rule>, score: Option<f64>) -> f64 {
    let Some(rule) = rule else { return 1.0 };
    let Some(check) = rule.check.as_ref() else {
        return 1.0;
    };
    match check.confidence.as_ref().map(|c| c.source) {
        // 显式声明余量，或没声明但检测器本来就能给余量（classic 四件套）→ 自动派生
        Some(core_config::ConfidenceSource::DetectorMargin) | None
            if engine_classic::registry::supports_margin(&check.detector) =>
        {
            score
                .and_then(|sc| {
                    engine_classic::registry::margin_confidence(&check.detector, &check.params, sc)
                })
                .unwrap_or(0.0)
        }
        Some(core_config::ConfidenceSource::DetectorMargin) => score
            .and_then(|sc| {
                engine_classic::registry::margin_confidence(&check.detector, &check.params, sc)
            })
            .unwrap_or(0.0),
        Some(core_config::ConfidenceSource::Fixed) => check
            .confidence
            .as_ref()
            .and_then(|c| c.value)
            .unwrap_or(0.0),
        // COMPLIANCE-DOMAIN：VL 自评由 VL 通道逐行填（模型对这一张图的把握度）。
        // 走到这里说明没人填 —— 给 0.0（「不确定」），绝不伪装成 1.0；
        // 正常路径下 `with_rule_confidence` 已短路，不会覆盖通道填的值。
        Some(core_config::ConfidenceSource::VlSelfReport) => 0.0,
        // 不支持余量的检测器（vl / 组级 / 分割）又没声明 fixed → 装载期 lint 已硬拒；
        // 真走到这里给 0.0（「不确定」），绝不伪装成 1.0
        None => 0.0,
    }
}

/// 入队 payload。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompliancePayload {
    packId: String,
    /// 修复/do 产物输出目录（缺省 = 系统临时目录下按 job 隔离）
    #[serde(default)]
    outDir: Option<String>,
    /// N6e：动作链（顺序即执行序）。空/缺省 = 既有语义（判定插件 judge→repair；
    /// 处理类包按 `options.action` 单动作）。非空 = 逐动作调插件、只落一张工作版本。
    #[serde(default)]
    actions: Vec<String>,
    /// N1（规格实体）：只判这些规则（空 = 包内全部规则，协议 §3.3「省略 = 全部规则」）。
    ///
    /// 为什么进了 payload 而不是放在全局配置：**规格是逐任务的选择**（淘宝主图判 6 条、
    /// 用户自定义规格可能只判 3 条），同一个包的两批图可以是不同规格 → 只能跟着 job 走。
    #[serde(default)]
    rules: Vec<String>,
}

/// 入队（与 embed 同防重语义：同资产已有 pending/running 的 compliance job 则跳过）。
/// 返回实际新入队数。
///
/// 不带 run：历史调用方（dev-e2e 探针/单测）语义不变 —— 这些 job 的 `runId` 为 NULL，
/// 归入「迁移前/无 run」伪桶，**不会**出现在任何一次产品执行（run）的统计里。
pub fn compliance_enq(
    conn: &Connection,
    asset_ids: &[i64],
    pack_id: &str,
    out_dir: Option<&str>,
) -> Result<usize> {
    compliance_enq_in_run(conn, asset_ids, pack_id, out_dir, None)
}

/// CMP-JOB-01：带 run 的资产入队 —— 每行 `runId = run_id`（`None` = 历史调用方，落 NULL）。
///
/// 一次批量审核 = 一个 run；run 的统计只按 `WHERE runId = ?` 汇总，历史累计数不得掺进来。
pub fn compliance_enq_in_run(
    conn: &Connection,
    asset_ids: &[i64],
    pack_id: &str,
    out_dir: Option<&str>,
    run_id: Option<i64>,
) -> Result<usize> {
    compliance_enq_full_in_run(conn, asset_ids, pack_id, out_dir, run_id, &[], &[])
}

/// N1：带**规则子集**的资产入队（规格「选哪些规则」的落地点）。
///
/// `rules` 非空 = 只判这些规则（插件通道按协议 §3.3 `rules` 传下去；
/// `rules` 空 = 全部规则，行为与 [`compliance_enq_in_run`] 逐字相同）。
pub fn compliance_enq_in_run_rules(
    conn: &Connection,
    asset_ids: &[i64],
    pack_id: &str,
    out_dir: Option<&str>,
    run_id: Option<i64>,
    rules: &[String],
) -> Result<usize> {
    compliance_enq_full_in_run(conn, asset_ids, pack_id, out_dir, run_id, &[], rules)
}

/// N6e：带**动作链**的入队（`actions` 非空 = 该资产逐动作跑链、只落一张工作版本）。
///
/// 与 [`compliance_enq_in_run`] 同一份实现，只是 payload 多带 `actions[]`：
/// 防重语义/runId 口径完全一致（同资产已有 pending/running 的 compliance job 仍跳过）。
pub fn compliance_enq_actions_in_run(
    conn: &Connection,
    asset_ids: &[i64],
    pack_id: &str,
    out_dir: Option<&str>,
    run_id: Option<i64>,
    actions: &[String],
) -> Result<usize> {
    compliance_enq_full_in_run(conn, asset_ids, pack_id, out_dir, run_id, actions, &[])
}

/// N链合流（N6e × N1）：动作链 + 规则子集同时带上 —— 逐动作跑链，且只判规格选的规则。
pub fn compliance_enq_actions_rules_in_run(
    conn: &Connection,
    asset_ids: &[i64],
    pack_id: &str,
    out_dir: Option<&str>,
    run_id: Option<i64>,
    actions: &[String],
    rules: &[String],
) -> Result<usize> {
    compliance_enq_full_in_run(conn, asset_ids, pack_id, out_dir, run_id, actions, rules)
}

/// 内部唯一实现：`actions`/`rules` 皆空 = 历史语义。
fn compliance_enq_full_in_run(
    conn: &Connection,
    asset_ids: &[i64],
    pack_id: &str,
    out_dir: Option<&str>,
    run_id: Option<i64>,
    actions: &[String],
    rules: &[String],
) -> Result<usize> {
    let payload = serde_json::to_string(&CompliancePayload {
        packId: pack_id.to_string(),
        outDir: out_dir.map(|s| s.to_string()),
        actions: actions.to_vec(),
        rules: rules.to_vec(),
    })
    .map_err(|e| PipelineError::Compliance(format!("payload 序列化: {e}")))?;
    let mut n = 0usize;
    for &asset_id in asset_ids {
        let inserted = conn.execute(
            "INSERT INTO job_queue (kind, targetId, payload, status, createdAt, updatedAt, runId)
             SELECT 'compliance', ?1, ?2, 'pending', unixepoch(), unixepoch(), ?3
             WHERE NOT EXISTS (
               SELECT 1 FROM job_queue
               WHERE kind = 'compliance' AND targetId = ?1 AND status IN ('pending','running'))",
            rusqlite::params![asset_id, payload, run_id],
        )?;
        n += inserted;
    }
    Ok(n)
}

/// 组级 job 入队 payload（C9）：组目标（组键 + 成员 + group_by 作用域）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GroupPayload {
    packId: String,
    #[serde(default)]
    outDir: Option<String>,
    /// 组键取值；`None` = 缺键组（成员取不到组键 → 落 `undetermined` + `group_key_missing`）
    #[serde(default)]
    groupKey: Option<String>,
    /// 组成员 assetId（升序；非空）
    members: Vec<i64>,
    /// group_by 作用域键（`core_config::GroupBy::scope_key()`：`folder` / `asset_field:<col>`）
    scope: String,
}

/// 组级入队：按 pack 的 per_group 规则解析组键，**每组一个 job**（同作用域的规则共享 job）。
///
/// 返回实际新入队数。防重语义：同 kind + 同 payload（= 同组同作用域）已有
/// pending/running → 跳过（与 `compliance_enq` 同口径）。
///
/// 缺键成员：单独凑成一个 `groupKey = None` 的 job（判定落 `pending_human` +
/// `group_key_missing`）——「取不到组键」必须响亮，不许静默当合格、也不许悄悄并进别的组。
pub fn compliance_enq_group(
    conn: &Connection,
    pack: &Pack,
    asset_ids: &[i64],
    out_dir: Option<&str>,
) -> Result<usize> {
    compliance_enq_group_in_run(conn, pack, asset_ids, out_dir, None)
}

/// CMP-JOB-01：带 run 的组级入队（每行 `runId = run_id`；`None` = 历史调用方，落 NULL）。
pub fn compliance_enq_group_in_run(
    conn: &Connection,
    pack: &Pack,
    asset_ids: &[i64],
    out_dir: Option<&str>,
    run_id: Option<i64>,
) -> Result<usize> {
    let repo = SqliteComplianceRepo::new(conn);
    // 作用域 → 组键列（同作用域的规则共享一个组级 job）
    let mut scopes: std::collections::BTreeMap<String, Option<String>> = Default::default();
    for rule in &pack.rules {
        if let (true, Some(gb)) = (rule.is_group_scoped(), rule.group_by.as_ref()) {
            scopes
                .entry(gb.scope_key())
                .or_insert_with(|| gb.asset_field().map(|s| s.to_string()));
        }
    }
    let mut n = 0usize;
    for (scope, field) in scopes {
        let rows = repo.group_source_rows(asset_ids, field.as_deref())?;
        let resolved = crate::compliance_group::resolve_group_keys(&rows);
        let mut jobs: Vec<(Option<String>, Vec<i64>)> = resolved
            .by_key
            .into_iter()
            .map(|(k, v)| (Some(k), v))
            .collect();
        if !resolved.missing.is_empty() {
            jobs.push((None, resolved.missing));
        }
        for (group_key, members) in jobs {
            if members.is_empty() {
                continue;
            }
            let anchor = *members.iter().min().expect("members 非空");
            let payload = serde_json::to_string(&GroupPayload {
                packId: pack.id.clone(),
                outDir: out_dir.map(|s| s.to_string()),
                groupKey: group_key,
                members,
                scope: scope.clone(),
            })
            .map_err(|e| PipelineError::Compliance(format!("组级 payload 序列化: {e}")))?;
            let inserted = conn.execute(
                "INSERT INTO job_queue (kind, targetId, payload, status, createdAt, updatedAt, runId)
                 SELECT ?1, ?2, ?3, 'pending', unixepoch(), unixepoch(), ?4
                 WHERE NOT EXISTS (
                   SELECT 1 FROM job_queue
                   WHERE kind = ?1 AND payload = ?3 AND status IN ('pending','running'))",
                rusqlite::params![COMPLIANCE_GROUP_KIND, anchor, payload, run_id],
            )?;
            n += inserted;
        }
    }
    Ok(n)
}

/// 队列统计（进度条用）：(done, failed, pending+running)。
pub fn compliance_stats(conn: &Connection) -> Result<(i64, i64, i64)> {
    Ok(jobq::stats(conn, COMPLIANCE_KIND)?)
}

/// C9：组级队列统计（与资产口径分开：组级 job 数 = 组数 × 作用域数，不是资产数）。
pub fn compliance_group_stats(conn: &Connection) -> Result<(i64, i64, i64)> {
    Ok(jobq::stats(conn, COMPLIANCE_GROUP_KIND)?)
}

/// 失败 job 重置为 pending（attempts 清零重新计；重试入口）。C9：资产与组级一起重置。
pub fn compliance_retry_failed(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE job_queue SET status='pending', attempts=0, updatedAt=unixepoch()
         WHERE kind IN (?1, ?2) AND status = 'failed'",
        rusqlite::params![COMPLIANCE_KIND, COMPLIANCE_GROUP_KIND],
    )?)
}

/// 单资产处理结果（job 记账用）。
enum AssetOutcome {
    /// 完成（verdicts = 落库行数；rule_errors = 被隔离的规则级失败明细）
    Done {
        verdicts: usize,
        rule_errors: Vec<String>,
    },
    /// 不合格资产（已删/失联/非图片）→ 直接完成不判（custom_queue 同口径）
    Skipped,
    /// 确定性失败（插件说这次做不了 / 链上没有产物）：job 落 `failed`，**不回 pending**。
    ///
    /// 单独一个变体是刻意的：`Done` 会被调用方 `jobq::complete` 刷回 done（结果表说
    /// 「执行失败」、run 汇总却说 `failed=0 / done=1`，失败原因块还按 status 过滤成空）；
    /// `Err` 又会走 `jobq::fail` 的重试路径，把同一句「目标超限」再说 3 遍。
    Failed {
        /// 与结果行 `reason` **同一段文本**（`asset_versions.errorMessage`）——
        /// 两处展示同源，不一处兜底一处原文。
        message: String,
    },
    /// N6e：动作链在两步之间被取消 —— job 退回 pending（下一轮续跑），本轮不计完成。
    /// 单独一个变体是刻意的：`Skipped` 会被调用方 `jobq::complete`，取消就变成了「做过」。
    Cancelled,
}

/// 一轮运行汇总。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ComplianceRunReport {
    pub total: usize,
    pub done: usize,
    pub failed: usize,
    pub retry: usize,
    pub skipped: usize,
    pub cancelled: bool,
    /// 失败明细（`jobId/assetId: msg`），最多 50 条
    pub errors: Vec<String>,
    /// C8：**规则级失败隔离**明细（`asset<id> rule<id>: msg`，最多 50 条）。
    /// 与 `errors` 分开：这些不是 job 失败（资产那一代判定照落，只是被隔离的规则
    /// 落 `pending_human` + `reason=load_error|rule_error`），但同样必须响亮可见。
    pub ruleErrors: Vec<String>,
    /// C9：**组级 job** 计数（与资产评估分开：组级 job 数 = 组数 × 作用域数，不是资产数）
    pub groupTotal: usize,
    pub groupDone: usize,
    pub groupFailed: usize,
    pub groupRetry: usize,
    pub groupSkipped: usize,
    /// C9：组级失败明细（`job/group<key> scope: msg`，最多 50 条）
    pub groupErrors: Vec<String>,
    /// C9：组级 job 的队列快照（done/failed/pending+running）
    pub groupJobDone: i64,
    pub groupJobFailed: i64,
    pub groupJobPending: i64,
    pub jobDone: i64,
    pub jobFailed: i64,
    pub jobPending: i64,
}

/// C8：单条规则级失败明细上限（报告里截断，证据仍在 verdict 行里）。
const RULE_ERROR_CAP: usize = 50;

/// 逐资产进度回调载荷。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ComplianceProgress {
    pub jobId: i64,
    pub assetId: i64,
    /// done / failed / retry / skipped
    pub status: String,
    pub index: usize,
    pub total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// IP-K05：插件执行通道（端口 + 纯映射）
// ---------------------------------------------------------------------------
//
// 分层：行业包宿主内核 `pack-host` 与本 crate **同层（L2）**，同层禁互引（check_layers.sh），
// 所以这里只声明端口（与 `compliance_vl` 的 VL 通道同一套路：实现在 L3 注入）。
// `app-web::compliance_api` 用 `pack-host` 装配出实现，判定/修复全部走子进程插件。
//
// 宿主职责边界（协议 v1.1）：
// * 宿主**只做映射与落库**：不重算阈值、不改插件结论、不做「修完就算过」；
// * 插件失败**绝不冒充结论**：timeout / crash / 包拉不起来 / 协议违例 → 对应规则落
//   `undetermined` + `evidence.reason`，合格与不合格都不占；
// * 插件失败**绝不让宿主退出**：错误只落在判定行与运行报告里（job 仍按 done 收工，
//   除非连「有哪些规则」都不知道 → 那才 Err，让 attempts 重试并响亮可见）。

/// 一次插件调用的输入（资产 + 工作目录 + 可选规则子集）。
#[derive(Debug, Clone)]
pub struct PluginAsset {
    pub asset_id: i64,
    pub job_id: i64,
    /// 原图绝对路径（协议 §3.3 `asset.path`）
    pub path: String,
    pub mime: Option<String>,
    pub width: u32,
    pub height: u32,
    /// 修复产物目录（协议 §3.4 `options.workdir`；宿主已确保存在）
    pub work_dir: String,
    /// 只判这些规则（空 = 包内全部规则，协议 §3.3「省略 = 全部规则」）
    pub rule_ids: Vec<String>,
}

/// 插件单条结论 = 协议 §4.2 的七个字段（宿主只认这七个，多余字段进 evidence）。
#[derive(Debug, Clone, PartialEq)]
pub struct PluginVerdict {
    pub rule_id: String,
    /// 协议闭集：pass | fail | undetermined | skipped（越界由宿主改判 undetermined）
    pub status: String,
    pub score: Option<f64>,
    pub label: Option<String>,
    pub evidence: serde_json::Value,
    pub fixed: bool,
    pub note: Option<String>,
}

/// 插件失败的归档原因（决定落库 `evidence.reason` 的词，报告按它分类）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginFailure {
    /// 超预算（manifest.timeout_secs / 显式预算）
    Timeout,
    /// 子进程崩溃（重启仍崩）
    Crash,
    /// 包拉不起来 / 握手失败 / 冷却中（含解释器缺失）
    Unavailable,
    /// 单条规则级错误（协议错误码：参数错、资产读不出、包内业务异常）
    RuleError,
    /// 应答不合协议（缺 verdicts / 字段缺失 / 状态越界）
    Protocol,
    /// 处理类插件（`kinds` 不含 `judge`）回了 verdict —— 拒收（防「处理冒充判定」）
    VerdictNotAllowed,
}

impl PluginFailure {
    /// 落 `evidence.reason` 的稳定词（报告/筛选器按它聚合，别改已有词）。
    pub fn reason(self) -> &'static str {
        match self {
            Self::Timeout => "plugin_timeout",
            Self::Crash => "plugin_crash",
            Self::Unavailable => "plugin_unavailable",
            Self::RuleError => "plugin_rule_error",
            Self::Protocol => "plugin_protocol_error",
            Self::VerdictNotAllowed => "plugin_verdict_not_allowed",
        }
    }
}

/// 包身份（判定行来源列的来源；`judge_asset` 失败时宿主仍需要它铺 undetermined 行）。
#[derive(Debug, Clone, PartialEq)]
pub struct PackIdentityInfo {
    /// 插件包 id（写进 `rule_verdict.packId`：**判定由谁下的**就是它）
    pub id: String,
    pub version: String,
    /// `manifest.json` 原文 sha256（= `packs.manifest_hash`）
    pub plugin_hash: String,
    /// 包级确定性声明（true = 该包所有结论可复现；置信度口径见 [`plugin_confidence`]）
    pub deterministic: bool,
    /// 包声明规则清单（声明序 = 落库/报告列序）
    pub rule_ids: Vec<String>,
    /// manifest `kinds` 的生效值（`judge`/`process`，N5a）。
    ///
    /// 宿主据它决定「要不要 `verdict`、有没有审核列/审核动作」，**不靠猜**；
    /// 处理类（不含 `judge`）返回 verdict 即拒收（`plugin_verdict_not_allowed`）。
    pub kinds: Vec<String>,
    /// `pack.info.actions[].id`（N6e）：处理类包的可执行动作清单（顺序 = UI 顺序）。
    /// 宿主据此校验 `actions[]` 入参（未知动作 400 点名），判定类包为空。
    pub actions: Vec<String>,
    /// `manifest.timeout_secs`（单步预算；链总预算 = 它 × 动作数，见 `compliance_chain`）。
    pub timeout_secs: u64,
}

/// 一次插件调用（judge 或 repair）的结果。
#[derive(Debug, Clone)]
pub struct PluginOutcome {
    pub pack: PackIdentityInfo,
    pub verdicts: Vec<PluginVerdict>,
    /// 修复产物（协议 §4.3 原始条目，直接进 evidence）
    pub artifacts: Vec<serde_json::Value>,
    /// 结果行原文（协议 §4.4 `rows[]`，N6e）：处理类插件逐动作调用时，宿主靠它读
    /// `fields.outPath/sha256/assetName`。判定类也照留（结论已映射进 `verdicts`，这只是证据）。
    pub rows: Vec<serde_json::Value>,
    pub duration_ms: u64,
    /// 失败原因 + 原始错误文本（成功 = None）
    pub failure: Option<(PluginFailure, String)>,
    /// 原始应答（截断后进 evidence；排障时能回放）
    pub raw: serde_json::Value,
}

/// 行业包判定/修复端口（L3 注入）。
pub trait PackJudge: Send + Sync {
    /// 包身份 + 规则清单（进程内缓存；每次 run 取一次即可）。
    fn identity(&self) -> PackIdentityInfo;
    /// 协议 §3.3 `judge`。
    fn judge_asset(&self, req: &PluginAsset) -> PluginOutcome;
    /// 协议 §3.4 `repair`（修复后复判结论在 `verdicts` 里，产物在 `artifacts` 里）。
    fn repair_asset(&self, req: &PluginAsset) -> PluginOutcome;
    /// N6e：**单动作**调用（处理类包的链式编排口）。
    ///
    /// 协议不变 —— 就是一次 `judge`，`options.action` = `action`（`pack.info.actions[].id`），
    /// 产物在结果行 `fields.outPath`。默认实现**明确否认**（不静默退化成 judge）：
    /// 通道没实现动作调用时必须响亮失败，否则「链跑完了但什么都没变」是最坏的假成功。
    fn action_asset(&self, _req: &PluginAsset, action: &str) -> PluginOutcome {
        let identity = self.identity();
        PluginOutcome {
            pack: identity,
            verdicts: Vec::new(),
            artifacts: Vec::new(),
            rows: Vec::new(),
            duration_ms: 0,
            failure: Some((
                PluginFailure::Protocol,
                format!("该执行通道未实现单动作调用（action={action}）"),
            )),
            raw: serde_json::Value::Null,
        }
    }
}

/// 插件行的置信度（宿主对「这条结论有多可信」的唯一口径）。
///
/// * `undetermined` → 0.0（没有结论，必须进低置信度复核，不许伪装把握度）；
/// * `skipped` → 1.0（「确定没有结论」，与 legacy 口径一致）；
/// * 其余：确定性包 → 1.0；非确定性包 → 插件给的 `score` 夹到 [0,1]，**没给就 0.0**
///   （协议 §4.2 的 `score` 只作分数语义，宿主不拿 0 冒充把握度）。
fn plugin_confidence(pack: &PackIdentityInfo, v: &PluginVerdict) -> f64 {
    match v.status.as_str() {
        verdict::UNDETERMINED => 0.0,
        verdict::SKIPPED => 1.0,
        _ if pack.deterministic => 1.0,
        _ => v.score.map(|s| s.clamp(0.0, 1.0)).unwrap_or(0.0),
    }
}

/// 插件**失败**时铺行：包声明的每条规则一行 `undetermined`（不冒充结论、不许少行）。
fn plugin_failure_rows(
    pack: &PackIdentityInfo,
    failure: PluginFailure,
    detail: &str,
    // N1：规格只选了部分规则时，失败行只铺这些规则（空 = 包内全部 —— 历史口径不变）
    selected: &[String],
) -> Vec<StepVerdict> {
    let ids: Vec<&String> = if selected.is_empty() {
        pack.rule_ids.iter().collect()
    } else {
        selected.iter().collect()
    };
    ids.into_iter()
        .map(|rule_id| {
            let mut sv = StepVerdict::new(
                rule_id.clone(),
                verdict::UNDETERMINED,
                serde_json::json!({
                    "engine": "plugin",
                    "pack": format!("{}@{}#{}", pack.id, pack.version, short_hash(&pack.plugin_hash)),
                    "reason": failure.reason(),
                    "error": detail,
                }),
            );
            // 没有结论 = 最低把握度（必须进低置信度复核，不许伪装成 1.0）
            sv.confidence = Some(0.0);
            sv
        })
        .collect()
}

/// `pluginHash` 的短形态（报告/证据里人读用；落库仍存全量）。
fn short_hash(h: &str) -> String {
    h.chars().take(12).collect()
}

/// 判定结论（`pass`/`fail` 才算「有结论」；`skipped` = 这条规则对这张图没结论，
/// `undetermined` = 判不了。两者都不得当成结论去覆盖别人）。
fn is_conclusion(status: &str) -> bool {
    status == verdict::PASS || status == verdict::FAIL
}

/// 合并后的单行取值：`v` = 最终采信的那条结论，`from_repair` = 它是否来自 repair，
/// `suppressed` = 被拒绝覆盖的 repair 结论（只进 evidence，不影响行结论）。
#[derive(Clone, Copy)]
struct MergedVerdict<'a> {
    v: &'a PluginVerdict,
    from_repair: bool,
    suppressed: Option<&'a PluginVerdict>,
}

/// 把 judge/repair 两次调用合成判定行（**纯函数**，插件无关 → 单测可穷举所有分支）。
///
/// 合并规则（协议 §3.4）：
/// * `repair` 的**结论**（pass/fail）覆盖同 ruleId 的 `judge` 结论（修复后复判才是最终结论）；
/// * `repair` 的 `skipped`/`undetermined` **不得覆盖** judge 已给出的 pass/fail ——
///   「没有结论」不等于「结论被推翻」，否则不可修规则的 fail 会被抹成 skipped
///   （K07 实测：R0_product_main 等 3 行 fail 在库/界面里消失）；
/// * `repair` 失败**不推翻** judge 结论（判定已经成立，只是没修成 → 行保留原结论，
///   失败原因进 evidence，界面能解释「为什么没修」）；
/// * 协议违例（状态越界 / 缺规则）→ 该行 `undetermined` + `note` = 违反项，不静默采信。
#[allow(dead_code)] // 保留：无 scope 限制的旧口径入口（= plugin_rows_scoped 空集）
fn plugin_rows(judged: &PluginOutcome, repaired: Option<&PluginOutcome>) -> Vec<StepVerdict> {
    plugin_rows_scoped(judged, repaired, &[])
}

/// N1：同上，但知道「本任务只判了哪几条规则」（规格的 `rules` 子集）。
///
/// 只有失败铺行需要它：成功路径的结论行本来就只包含插件返回的规则（= 子集），
/// 失败路径若按包内全部规则铺 `undetermined`，用户会看到「我根本没选的规则出结论」。
fn plugin_rows_scoped(
    judged: &PluginOutcome,
    repaired: Option<&PluginOutcome>,
    selected: &[String],
) -> Vec<StepVerdict> {
    let pack = &judged.pack;
    // 失败且连规则清单都没有 → 调用方必须当 job 级错误（这里返回空，由上层判）
    if judged.failure.is_some() && pack.rule_ids.is_empty() {
        return Vec::new();
    }
    if let Some((kind, detail)) = &judged.failure {
        return plugin_failure_rows(pack, *kind, detail, selected);
    }
    plugin_rows_merged(judged, repaired, selected)
}

fn plugin_rows_merged(
    judged: &PluginOutcome,
    repaired: Option<&PluginOutcome>,
    // N1：本任务只判了哪些规则（空 = 包内全部）。输出序与「缺结论补 undetermined」都按它算 ——
    // 否则「规格只判 2 条」会被包内第 3 条规则补出一行「协议违例」，看着像插件出错。
    selected: &[String],
) -> Vec<StepVerdict> {
    let pack = &judged.pack;
    // 逐规则合并：judge 结论打底，repair 只在「给出结论」时才覆盖
    let mut judged_by_rule: BTreeMap<&str, &PluginVerdict> = BTreeMap::new();
    for v in &judged.verdicts {
        judged_by_rule.insert(v.rule_id.as_str(), v);
    }
    let mut by_rule: BTreeMap<&str, MergedVerdict<'_>> = BTreeMap::new();
    for (rid, jv) in &judged_by_rule {
        by_rule.insert(
            rid,
            MergedVerdict {
                v: jv,
                from_repair: false,
                suppressed: None,
            },
        );
    }
    if let Some(rep) = repaired {
        for rv in &rep.verdicts {
            let rid = rv.rule_id.as_str();
            let judge_has_conclusion = judged_by_rule
                .get(rid)
                .map(|jv| is_conclusion(&jv.status))
                .unwrap_or(false);
            // 闭集外的状态（协议 §4.2）不可采信 → 不算结论
            let repair_has_conclusion = verdict::is_valid(&rv.status) && is_conclusion(&rv.status);
            let merged = match judged_by_rule.get(rid) {
                Some(jv) if judge_has_conclusion && !repair_has_conclusion => MergedVerdict {
                    v: jv,
                    from_repair: false,
                    suppressed: Some(rv),
                },
                _ => MergedVerdict {
                    v: rv,
                    from_repair: true,
                    suppressed: None,
                },
            };
            by_rule.insert(rid, merged);
        }
    }
    let artifacts_for = |rule_id: &str| -> Vec<serde_json::Value> {
        repaired
            .map(|rep| {
                rep.artifacts
                    .iter()
                    .filter(|a| a.get("rule_id").and_then(|r| r.as_str()) == Some(rule_id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    };

    // 输出序 = 本任务请求的规则序（规格选的那批；空 = 包声明序），包外 ruleId 追加在后
    let mut order: Vec<&str> = if selected.is_empty() {
        pack.rule_ids.iter().map(String::as_str).collect()
    } else {
        selected.iter().map(String::as_str).collect()
    };
    for rid in by_rule.keys() {
        if !order.contains(rid) {
            order.push(rid);
        }
    }

    let mut out = Vec::new();
    for rule_id in order {
        let Some(merged) = by_rule.get(rule_id).copied() else {
            // 包声明了这条规则但没回结论：协议违例（§3.3「必须覆盖请求的每一条 ruleId」）
            out.push(StepVerdict::new(
                rule_id.to_string(),
                verdict::UNDETERMINED,
                serde_json::json!({
                    "engine": "plugin",
                    "pack": format!("{}@{}#{}", pack.id, pack.version, short_hash(&pack.plugin_hash)),
                    "reason": PluginFailure::Protocol.reason(),
                    "error": "插件未回该规则的 verdict（协议 §3.3 要求逐条覆盖）",
                }),
            ));
            continue;
        };
        let (v, from_repair) = (merged.v, merged.from_repair);
        let suppressed = merged.suppressed;
        // 状态越界 → 不可采信（协议闭集 pass|fail|undetermined|skipped）
        let mut sv = if verdict::is_valid(&v.status) {
            StepVerdict::new(rule_id.to_string(), &v.status, serde_json::Value::Null)
        } else {
            StepVerdict::new(
                rule_id.to_string(),
                verdict::UNDETERMINED,
                serde_json::Value::Null,
            )
        };
        let status_rejected = !verdict::is_valid(&v.status);
        sv.score = v.score;
        sv.confidence = Some(if status_rejected {
            0.0
        } else {
            plugin_confidence(pack, v)
        });
        if from_repair {
            // 只有插件明确标了 `fixed=true` 才算「修过」（协议 §4.2 第七字段）；
            // 复检结论一律记 recheck（协议 §3.4：修复后必须按规则复判）
            if v.fixed {
                sv.fix_action = Some("plugin_repair".to_string());
            }
            sv.recheck =
                Some(v.status.clone()).filter(|s| s == verdict::PASS || s == verdict::FAIL);
        }
        // 插件原始证据 + 宿主补充（谁判的、包身份、修复产物、失败原因）
        let mut evidence = serde_json::json!({
            "engine": "plugin",
            "pack": format!("{}@{}#{}", pack.id, pack.version, short_hash(&pack.plugin_hash)),
            "deterministic": pack.deterministic,
            "duration_ms": if from_repair { repaired.map(|r| r.duration_ms).unwrap_or(0) } else { judged.duration_ms },
            "fixed": v.fixed,
            "from_repair": from_repair,
        });
        if let Some(label) = &v.label {
            evidence["label"] = serde_json::json!(label);
        }
        if let Some(note) = &v.note {
            evidence["note"] = serde_json::json!(note);
        }
        if status_rejected {
            evidence["reason"] = serde_json::json!(PluginFailure::Protocol.reason());
            evidence["error"] =
                serde_json::json!(format!("插件回了闭集外的状态 {:?}（协议 §4.2）", v.status));
        }
        let arts = artifacts_for(rule_id);
        if !arts.is_empty() {
            evidence["artifacts"] = serde_json::json!(arts);
        }
        // repair 自身的失败不能吞：判定结论保留，修没修成照样可见
        if let Some((kind, detail)) = repaired.and_then(|r| r.failure.as_ref()) {
            evidence["repair_error"] =
                serde_json::json!({ "reason": kind.reason(), "error": detail });
        }
        let plugin_evidence = merge_evidence(evidence, v.evidence.clone());
        sv.evidence = plugin_evidence;
        // 被拒绝的 repair 结论（skipped/undetermined 不得降级 judge 的 pass/fail）：
        // 原文留在证据里，界面/报告能解释「为什么没按插件复判改结论」
        if let Some(rv) = suppressed {
            if let serde_json::Value::Object(map) = &mut sv.evidence {
                map.insert("repair_status".into(), serde_json::json!(rv.status));
                map.insert("repair_suppressed".into(), serde_json::json!(true));
                if let Some(label) = &rv.label {
                    map.insert("repair_label".into(), serde_json::json!(label));
                }
                if let Some(note) = &rv.note {
                    map.insert("repair_note".into(), serde_json::json!(note));
                }
            }
        }
        out.push(sv);
    }
    out
}

/// 跑 compliance 队列直到空或取消（断点续跑：内部先 requeue_stale）。
pub fn run_compliance_queue(
    conn: &Connection,
    pack: &Pack,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(ComplianceProgress),
) -> Result<ComplianceRunReport> {
    run_compliance_queue_with_limit(conn, pack, cancel, progress, 0)
}

/// CMP-JOB-01：带 run 的顺序入口（单测/探针用同一套 run 记账语义，别另写一份）。
pub fn run_compliance_queue_in_run(
    conn: &Connection,
    pack: &Pack,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(ComplianceProgress),
    run_id: Option<i64>,
) -> Result<ComplianceRunReport> {
    run_compliance_queue_with_limit_in_run(conn, pack, cancel, progress, 0, run_id)
}

/// 带 `limit` 变体：`limit > 0` 本轮最多处理 limit 个 job，其余回 pending（web 验收用）。
pub fn run_compliance_queue_with_limit(
    conn: &Connection,
    pack: &Pack,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(ComplianceProgress),
    limit: usize,
) -> Result<ComplianceRunReport> {
    run_compliance_queue_with_limit_in_run(conn, pack, cancel, progress, limit, None)
}

/// CMP-JOB-01：顺序入口的完整形态 —— `run_id = Some(..)` 时把「跳过」记进该 run 的计数器
/// （`job_queue` 里跳过与成功同为 done 终态，只有这里能区分）。
pub fn run_compliance_queue_with_limit_in_run(
    conn: &Connection,
    pack: &Pack,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(ComplianceProgress),
    limit: usize,
    run_id: Option<i64>,
) -> Result<ComplianceRunReport> {
    run_compliance_queue_with_limit_in_run_engine(conn, pack, cancel, progress, limit, run_id, None)
}

/// 顺序入口 + 执行通道（`plugin = Some(..)` → 逐图 job 全交处理插件）。
///
/// **`plugin = None` 不是「另一个引擎档位」**：宿主侧没有任何配置/接口能让它变成 None
/// （IP-K16 删了引擎开关、N18 把唯一入口锁在插件上）。它只服务内部维护路径 ——
/// 组级 job（`per_group` 规则）与历史单测：组级判定不在插件协议范围（协议 v1.1 的 `judge`
/// 是单资产调用），且插件包里没有 `per_group` 规则 —— 别在这里假装支持。
pub fn run_compliance_queue_with_limit_in_run_engine(
    conn: &Connection,
    pack: &Pack,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(ComplianceProgress),
    limit: usize,
    run_id: Option<i64>,
    plugin: Option<&dyn PackJudge>,
) -> Result<ComplianceRunReport> {
    let _ = jobq::requeue_stale(conn)?; // 断点续跑：崩溃残留 running 回 pending
    let repo = SqliteComplianceRepo::new(conn);
    let runs = run_id.map(|rid| (rid, SqliteComplianceRunRepo::new(conn)));
    let mut report = ComplianceRunReport::default();
    let mut limited = false;
    // C8：装载期 lint 判定的规则级错误（一次算好；坏规则不执行 → 落 pending_human 证据，
    // 同图其余规则照落）——原先一条坏规则会让整张图 verdicts=0（C7 缺口 G4）。
    let rule_errors_pack = crate::compliance_lint::rule_errors(pack);
    // 本轮已处理过的 job：fail 回 pending 后同轮不再重抢（重试交给下一轮运行），
    // 否则确定性失败的 job 会在单次运行内被无限 claim→fail 循环。
    let mut seen: std::collections::HashSet<i64> = Default::default();

    // ---------- 阶段顺序：先逐图（Asset）、后组级（Group） ----------
    // COMPLIANCE-DOMAIN 裁决：组级规则的 `depends_on` 指向逐图规则，而「先组级」时上游逐图
    // 判定行还不存在 → 跨作用域短路只能靠猜（C9 遗留缺口）。改成先资产后组级，组级 job
    // 开跑时成员的逐图结论已落库。
    loop {
        if cancel.load(Ordering::SeqCst) {
            report.cancelled = true;
            break;
        }
        let batch = jobq::claim_batch(conn, COMPLIANCE_KIND, COMPLIANCE_BATCH)?;
        if batch.is_empty() {
            break;
        }
        for (job_id, asset_id, payload) in &batch {
            if !seen.insert(*job_id) {
                // 上文刚失败回 pending 的 job：归位并结束本轮（断点续跑语义由外部重跑承接）
                back_to_pending(conn, *job_id)?;
                limited = true;
                break;
            }
            if cancel.load(Ordering::SeqCst) {
                back_to_pending(conn, *job_id)?;
                report.cancelled = true;
                break;
            }
            if limit > 0 && report.total >= limit {
                back_to_pending(conn, *job_id)?;
                limited = true;
                break;
            }
            let (_, _, pending) = jobq::stats(conn, COMPLIANCE_KIND)?;
            let index = report.total + 1;
            let total = pending as usize + index;
            let outcome = process_job(
                conn,
                &repo,
                pack,
                *job_id,
                *asset_id,
                payload.as_deref(),
                None,
                &rule_errors_pack,
                plugin,
                Some(cancel),
            );
            match outcome {
                Ok(AssetOutcome::Skipped) => {
                    jobq::complete(conn, *job_id)?;
                    report.skipped += 1;
                    // CMP-JOB-01：跳过也进 run 的计数（job_queue 里与 done 同终态，只能在这里记）
                    if let Some((rid, rrepo)) = &runs {
                        rrepo.note_skipped(*rid)?;
                    }
                    progress(ComplianceProgress {
                        jobId: *job_id,
                        assetId: *asset_id,
                        status: "skipped".into(),
                        index,
                        total,
                        error: None,
                    });
                }
                // N6e：动作链被取消 —— job 已退回 pending（下一轮续跑），本轮收工
                Ok(AssetOutcome::Cancelled) => {
                    report.cancelled = true;
                    progress(ComplianceProgress {
                        jobId: *job_id,
                        assetId: *asset_id,
                        status: "cancelled".into(),
                        index,
                        total,
                        error: Some("动作链已取消（job 退回队列）".into()),
                    });
                }
                // KI40-FIX：确定性失败 —— job 行已由 run_action_chain_job 落 failed
                // （`fail_terminal`），这里只做计数与进度，**不许再 complete**。
                Ok(AssetOutcome::Failed { message }) => {
                    report.failed += 1;
                    if report.errors.len() < 50 {
                        report
                            .errors
                            .push(format!("job{job_id}/asset{asset_id}: {message}"));
                    }
                    progress(ComplianceProgress {
                        jobId: *job_id,
                        assetId: *asset_id,
                        status: "failed".into(),
                        index,
                        total,
                        error: Some(message),
                    });
                }
                Ok(AssetOutcome::Done {
                    verdicts,
                    rule_errors,
                }) => {
                    jobq::complete(conn, *job_id)?;
                    report.done += 1;
                    for e in &rule_errors {
                        if report.ruleErrors.len() < RULE_ERROR_CAP {
                            report.ruleErrors.push(format!("asset{asset_id} {e}"));
                        }
                    }
                    progress(ComplianceProgress {
                        jobId: *job_id,
                        assetId: *asset_id,
                        status: "done".into(),
                        index,
                        total,
                        error: (!rule_errors.is_empty()).then(|| {
                            format!(
                                "{} 条规则级失败已隔离: {} 行判定已落库",
                                rule_errors.len(),
                                verdicts
                            )
                        }),
                    });
                }
                Err(e) => {
                    let msg = format!("{e}");
                    jobq::fail(conn, *job_id, &msg)?;
                    let status: String = conn.query_row(
                        "SELECT status FROM job_queue WHERE id = ?1",
                        [job_id],
                        |r| r.get(0),
                    )?;
                    let is_failed = status == jobq::status::FAILED;
                    if is_failed {
                        report.failed += 1;
                        if report.errors.len() < 50 {
                            report
                                .errors
                                .push(format!("job{job_id}/asset{asset_id}: {msg}"));
                        }
                    } else {
                        report.retry += 1;
                    }
                    progress(ComplianceProgress {
                        jobId: *job_id,
                        assetId: *asset_id,
                        status: if is_failed { "failed" } else { "retry" }.into(),
                        index,
                        total,
                        error: Some(msg),
                    });
                }
            }
            report.total += 1;
        }
        if report.cancelled || limited {
            break;
        }
    }

    if !limited && !report.cancelled {
        // ---------- C9：组级 job 阶段（资产判定完毕后） ----------
        // 资产判定全部落库后才跑：组级规则的 `depends_on`（逐图规则）此时可读。
        // 资产口径计数（total/done/failed/…）不受影响，组级单独记账（group*）。
        loop {
            if cancel.load(Ordering::SeqCst) {
                report.cancelled = true;
                break;
            }
            let batch = jobq::claim_batch(conn, COMPLIANCE_GROUP_KIND, COMPLIANCE_BATCH)?;
            if batch.is_empty() {
                break;
            }
            for (job_id, anchor_id, payload) in &batch {
                if !seen.insert(*job_id) {
                    back_to_pending(conn, *job_id)?;
                    break;
                }
                if cancel.load(Ordering::SeqCst) {
                    back_to_pending(conn, *job_id)?;
                    report.cancelled = true;
                    break;
                }
                let outcome = process_group_job(
                    conn,
                    &repo,
                    pack,
                    *job_id,
                    payload.as_deref(),
                    None,
                    &rule_errors_pack,
                );
                match outcome {
                    Ok(AssetOutcome::Skipped) => {
                        jobq::complete(conn, *job_id)?;
                        report.groupSkipped += 1;
                        if let Some((rid, rrepo)) = &runs {
                            rrepo.note_skipped(*rid)?;
                        }
                    }
                    // N6e：组级 job 不走动作链（处理类包单资产）；此臂只为穷尽 match
                    Ok(AssetOutcome::Cancelled) => {
                        back_to_pending(conn, *job_id)?;
                        report.cancelled = true;
                    }
                    // 同上：组级判定没有「动作链」，也就没有确定性失败这条出口
                    Ok(AssetOutcome::Failed { message }) => {
                        jobq::fail_terminal(conn, *job_id, &message)?;
                        report.groupFailed += 1;
                        if report.groupErrors.len() < 50 {
                            report
                                .groupErrors
                                .push(format!("job{job_id}/group{anchor_id}: {message}"));
                        }
                    }
                    Ok(AssetOutcome::Done {
                        verdicts,
                        rule_errors,
                    }) => {
                        jobq::complete(conn, *job_id)?;
                        report.groupDone += 1;
                        for e in &rule_errors {
                            if report.ruleErrors.len() < RULE_ERROR_CAP {
                                report.ruleErrors.push(format!("group{anchor_id} {e}"));
                            }
                        }
                        let _ = verdicts;
                    }
                    Err(e) => {
                        let msg = format!("{e}");
                        jobq::fail(conn, *job_id, &msg)?;
                        let status: String = conn.query_row(
                            "SELECT status FROM job_queue WHERE id = ?1",
                            [job_id],
                            |r| r.get(0),
                        )?;
                        if status == jobq::status::FAILED {
                            report.groupFailed += 1;
                        } else {
                            report.groupRetry += 1;
                        }
                        if report.groupErrors.len() < 50 {
                            report
                                .groupErrors
                                .push(format!("job{job_id}/group{anchor_id}: {msg}"));
                        }
                    }
                }
                report.groupTotal += 1;
            }
        }
    }

    let (d, f, p) = jobq::stats(conn, COMPLIANCE_KIND)?;
    report.jobDone = d;
    report.jobFailed = f;
    report.jobPending = p;
    // C9：组级队列快照（独立口径）
    let (gd, gf, gp) = jobq::stats(conn, COMPLIANCE_GROUP_KIND)?;
    report.groupJobDone = gd;
    report.groupJobFailed = gf;
    report.groupJobPending = gp;
    Ok(report)
}

fn back_to_pending(conn: &Connection, job_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE job_queue SET status = 'pending', updatedAt = unixepoch()
         WHERE id = ?1 AND status = 'running'",
        [job_id],
    )?;
    Ok(())
}

/// 把这条 task 收为 done 并留下「为什么跳过」的原因。
///
/// `job_queue` 里唯一可写的自由文本列是 `lastError`；跳过原因必须落库（界面点开 job 明细要给
/// 用户一句人话：「这张不是图片，已跳过」），否则 skipped 只是一个无法解释的数字。
/// 前缀词表见 [`crate::skip_reason`]（唯一口径，界面 / E2E 探针同口径）。
///
/// **覆盖语义**：`lastError = reason` 直接盖掉旧值——跳过必须是**本次**的理由，
/// 不能留着上一次的失败原文（否则界面显示「已完成 + 失败原因」，与 run 统计的「跳过」对不上）。
/// 随后调用方的 `jobq::complete()` 只改 status/updatedAt，不会抹掉这里的备注。
fn mark_skipped(conn: &Connection, job_id: i64, reason: &str) -> Result<()> {
    conn.execute(
        "UPDATE job_queue SET status = 'done', lastError = ?2, updatedAt = unixepoch()
         WHERE id = ?1",
        rusqlite::params![job_id, reason],
    )?;
    Ok(())
}

/// 判定成功落地：清掉上一次残留的失败原文。
///
/// CMP-JOB-FIX-1：`lastError` 是「这条 task 现在为什么没结果」的唯一出口。重试成功（或
/// 换一次执行重新判定成功）后若还留着上一轮的失败原文，界面就会显示「已完成 + 失败原因」，
/// 用户看到的是一个不存在的故障。成功即无原因 → 置 NULL。
fn clear_last_error(conn: &Connection, job_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE job_queue SET lastError = NULL WHERE id = ?1 AND lastError IS NOT NULL",
        [job_id],
    )?;
    Ok(())
}

// ---------- C5：并行队列驱动（VL 并发 = worker 数；单 claimer + 多 worker 连接） ----------

/// 连接工厂：并行驱动每个 worker 独立开连接（WAL + busy_timeout；生产为文件库，
/// 内存库不支持多连接共享——顺序入口 [`run_compliance_queue`] 不受影响）。
pub type ConnFactory = Arc<dyn Fn() -> rusqlite::Result<Connection> + Send + Sync>;

/// worker 单 job 的执行结果（main 线程记账用）。
#[allow(dead_code)]
struct WorkerOutcome {
    job_id: i64,
    /// 资产 job = assetId；组级 job = 组锚点 assetId（记账/日志用）
    asset_id: i64,
    /// C9：组级 job（记账分流到 group* 计数）
    group: bool,
    /// Err = job 失败（attempts 已计；is_failed=已转 failed）
    error: Option<String>,
    is_failed: bool,
    skipped: bool,
    verdict_rows: usize,
    /// C8：被隔离的规则级失败明细（资产仍算 done）
    rule_errors: Vec<String>,
}

impl WorkerOutcome {
    /// 空结果骨架（各错误路径复用；`group` 由调用处填）。
    fn stub(job_id: i64, asset_id: i64, group: bool) -> Self {
        Self {
            job_id,
            asset_id,
            group,
            error: None,
            is_failed: false,
            skipped: false,
            verdict_rows: 0,
            rule_errors: Vec::new(),
        }
    }
}

/// 派给 worker 的排队项（C9：资产 job 与组级 job 共用 worker 池）。
#[derive(Clone)]
enum QueuedJob {
    Asset {
        job_id: i64,
        asset_id: i64,
        payload: Option<String>,
    },
    Group {
        job_id: i64,
        /// 组锚点（组内最小 assetId）
        anchor_id: i64,
        payload: Option<String>,
    },
}

impl QueuedJob {
    fn job_id(&self) -> i64 {
        match self {
            QueuedJob::Asset { job_id, .. } | QueuedJob::Group { job_id, .. } => *job_id,
        }
    }

    /// 记账用的「目标 id」（资产 = assetId；组级 = 锚点）
    fn target_id(&self) -> i64 {
        match self {
            QueuedJob::Asset { asset_id, .. } => *asset_id,
            QueuedJob::Group { anchor_id, .. } => *anchor_id,
        }
    }

    fn is_group(&self) -> bool {
        matches!(self, QueuedJob::Group { .. })
    }
}

/// C9：并行驱动的阶段（组级 job 先跑完，再跑资产 job —— 两段各自 claim，共用 worker 池）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Group,
    Asset,
}

/// 并行运行 compliance 队列。
///
/// 与顺序入口同语义（claim 单点、seen 防同轮重抢、cancel、进度回调），差异：
/// - N 个 worker 线程各自持独立连接（工厂创建，busy_timeout 15s），verdict/job 记账
///   由 worker 自己写（WAL 写并发安全）；claim 只在主线程 → 无抢占。
/// - `vl = Some(cfg)` → vl 规则真实执行，worker 并发即 VL 并发（e2e 目标 10）；
///   `None` → vl skipped(executor_not_available)，与顺序入口一致。
/// - 单 worker panic 不炸运行（catch_unwind 转 job 失败记账）。
///
/// 返回 (运行报告, VL 统计快照（未配置 VL = None）)。
pub fn run_compliance_queue_par(
    factory: ConnFactory,
    pack: &Pack,
    vl: Option<VlConfig>,
    workers: usize,
    cancel: &AtomicBool,
    progress: impl FnMut(ComplianceProgress),
) -> Result<(ComplianceRunReport, Option<compliance_vl::VlStatsSnapshot>)> {
    run_compliance_queue_par_in_run(factory, pack, vl, workers, cancel, progress, None)
}

/// CMP-JOB-01：带 run 的并行入口 —— `run_id = Some(..)` 时把「跳过」记进该 run 计数器。
///
/// 其余语义与 [`run_compliance_queue_par`] 完全一致（同一份实现，不是复制）。
pub fn run_compliance_queue_par_in_run(
    factory: ConnFactory,
    pack: &Pack,
    vl: Option<VlConfig>,
    workers: usize,
    cancel: &AtomicBool,
    progress: impl FnMut(ComplianceProgress),
    run_id: Option<i64>,
) -> Result<(ComplianceRunReport, Option<compliance_vl::VlStatsSnapshot>)> {
    run_compliance_queue_par_in_run_engine(
        factory, pack, vl, None, workers, cancel, progress, run_id,
    )
}

/// IP-K05：并行入口 + 执行通道（`plugin = Some(..)` → 逐图 job 全交行业包插件）。
///
/// 语义与 [`run_compliance_queue_par_in_run`] 完全一致（同一份实现，不是复制）；
/// 插件句柄 `&self` 可并发调用（`pack-host` 内部串行化子进程往返），worker 数与 legacy 同口径。
#[allow(clippy::too_many_arguments)] // 编排入口全参固有宽度
pub fn run_compliance_queue_par_in_run_engine(
    factory: ConnFactory,
    pack: &Pack,
    vl: Option<VlConfig>,
    plugin: Option<Arc<dyn PackJudge>>,
    workers: usize,
    cancel: &AtomicBool,
    mut progress: impl FnMut(ComplianceProgress),
    run_id: Option<i64>,
) -> Result<(ComplianceRunReport, Option<compliance_vl::VlStatsSnapshot>)> {
    use std::sync::mpsc;
    let workers = workers.max(1);
    let stats = Arc::new(compliance_vl::VlStats::default());
    let (tx_job, rx_job) = mpsc::channel::<QueuedJob>();
    let rx_job = Arc::new(std::sync::Mutex::new(rx_job));
    let (tx_out, rx_out) = mpsc::channel::<WorkerOutcome>();
    let vl_handle = vl.map(|cfg| VlHandle {
        cfg,
        stats: stats.clone(),
    });

    let mut report = ComplianceRunReport::default();
    let mut seen: std::collections::HashSet<i64> = Default::default();
    // C8：装载期 lint 的规则级错误（worker 共享只读）
    let rule_errors_pack = crate::compliance_lint::rule_errors(pack);
    // 同轮重试记账：fail 回 pending 的 job 允许在本轮重新派发（≤ jobq 上限 3 次），
    // 耗尽则归位结束；避免永久失败 job 造成 claim→fail 死循环。
    let mut retries: std::collections::HashMap<i64, usize> = Default::default();

    std::thread::scope(|s| -> Result<()> {
        for _ in 0..workers {
            let factory = factory.clone();
            let rx_job = rx_job.clone();
            let tx_out = tx_out.clone();
            let vl_handle = vl_handle.clone();
            let plugin = plugin.clone();
            let rule_errors_pack = &rule_errors_pack;
            s.spawn(move || {
                let conn = match factory() {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx_out.send(WorkerOutcome {
                            job_id: -1,
                            asset_id: -1,
                            group: false,
                            error: Some(format!("worker 连接: {e}")),
                            is_failed: true,
                            skipped: false,
                            verdict_rows: 0,
                            rule_errors: Vec::new(),
                        });
                        return;
                    }
                };
                let _ = conn.busy_timeout(std::time::Duration::from_secs(15));
                let repo = SqliteComplianceRepo::new(&conn);
                loop {
                    let job = {
                        let guard = rx_job.lock().expect("job channel lock");
                        guard.recv()
                    };
                    let Ok(job) = job else {
                        return; // 主线程 channel 关闭 → 收工
                    };
                    let job_id = job.job_id();
                    let target_id = job.target_id();
                    let is_group = job.is_group();
                    if cancel.load(Ordering::SeqCst) {
                        let _ = back_to_pending(&conn, job_id);
                        let _ = tx_out.send(WorkerOutcome::stub(job_id, target_id, is_group));
                        continue;
                    }
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        match &job {
                            QueuedJob::Asset { payload, .. } => process_job(
                                &conn,
                                &repo,
                                pack,
                                job_id,
                                target_id,
                                payload.as_deref(),
                                vl_handle.as_ref(),
                                rule_errors_pack,
                                plugin.as_deref(),
                                Some(cancel),
                            ),
                            // C9：组级 job（一个 job = 一个组 × 一个作用域）
                            QueuedJob::Group { payload, .. } => process_group_job(
                                &conn,
                                &repo,
                                pack,
                                job_id,
                                payload.as_deref(),
                                vl_handle.as_ref(),
                                rule_errors_pack,
                            ),
                        }
                    }));
                    let asset_id = target_id;
                    let outcome = match result {
                        Ok(Ok(AssetOutcome::Done {
                            verdicts,
                            rule_errors,
                        })) => {
                            let _ = jobq::complete(&conn, job_id);
                            WorkerOutcome {
                                job_id,
                                asset_id,
                                group: is_group,
                                error: None,
                                is_failed: false,
                                skipped: false,
                                verdict_rows: verdicts,
                                rule_errors,
                            }
                        }
                        Ok(Ok(AssetOutcome::Skipped)) => {
                            let _ = jobq::complete(&conn, job_id);
                            WorkerOutcome {
                                job_id,
                                asset_id,
                                group: is_group,
                                error: None,
                                is_failed: false,
                                skipped: true,
                                verdict_rows: 0,
                                rule_errors: Vec::new(),
                            }
                        }
                        // N6e：动作链取消 → job 已退回 pending，本轮不算完成也不算失败
                        Ok(Ok(AssetOutcome::Cancelled)) => {
                            let _ = back_to_pending(&conn, job_id);
                            WorkerOutcome {
                                job_id,
                                asset_id,
                                group: is_group,
                                error: Some("动作链已取消（job 退回队列）".into()),
                                is_failed: false,
                                skipped: true,
                                verdict_rows: 0,
                                rule_errors: Vec::new(),
                            }
                        }
                        // KI40-FIX：确定性失败（job 行已由 run_action_chain_job 落 failed）
                        // —— 走 `is_failed=true` 记账，run 汇总与任务中心的「失败」同源。
                        Ok(Ok(AssetOutcome::Failed { message })) => WorkerOutcome {
                            job_id,
                            asset_id,
                            group: is_group,
                            error: Some(message),
                            is_failed: true,
                            skipped: false,
                            verdict_rows: 0,
                            rule_errors: Vec::new(),
                        },
                        Ok(Err(e)) => {
                            let msg = format!("{e}");
                            let _ = jobq::fail(&conn, job_id, &msg);
                            let is_failed = conn
                                .query_row(
                                    "SELECT status FROM job_queue WHERE id = ?1",
                                    [job_id],
                                    |r| r.get::<_, String>(0),
                                )
                                .map(|st| st == jobq::status::FAILED)
                                .unwrap_or(true);
                            WorkerOutcome {
                                job_id,
                                asset_id,
                                group: is_group,
                                error: Some(msg),
                                is_failed,
                                skipped: false,
                                verdict_rows: 0,
                                rule_errors: Vec::new(),
                            }
                        }
                        Err(_) => {
                            let msg = "worker panic".to_string();
                            let _ = jobq::fail(&conn, job_id, &msg);
                            WorkerOutcome {
                                job_id,
                                asset_id,
                                group: is_group,
                                error: Some(msg),
                                is_failed: true,
                                skipped: false,
                                verdict_rows: 0,
                                rule_errors: Vec::new(),
                            }
                        }
                    };
                    if tx_out.send(outcome).is_err() {
                        return;
                    }
                }
            });
        }
        drop(tx_out); // 全部 worker 就绪后主线程不再发结果（worker clone 持有）

        // 波次推进：claim → 派发 → 收齐本轮结果。任何错误路径先关闭 tx_job 再传播，
        // 否则 worker 阻在 recv 上，scope join 永不返回。
        let waves = (|| -> Result<()> {
            let conn = factory().map_err(|e| PipelineError::Compliance(format!("主连接: {e}")))?;
            let _ = conn.busy_timeout(std::time::Duration::from_secs(15));
            let _ = jobq::requeue_stale(&conn)?; // 断点续跑：崩溃残留 running 回 pending
                                                 // CMP-JOB-01：本轮的 run 账本（跳过计数唯一写入口）
            let runs = run_id.map(|rid| (rid, SqliteComplianceRunRepo::new(&conn)));
            let mut all_workers_dead = false;
            // COMPLIANCE-DOMAIN：**先逐图、后组级**。
            //
            // 原顺序（C9：先组级）与「组级规则 depends_on 逐图规则」直接冲突：组级 job 在跑的时候
            // 上游逐图判定行还不存在，跨作用域短路就只能靠猜。组级判定本来就是「组内聚合」，
            // 逻辑上就该排在逐图判定之后（成员结论先落地，再做组内汇总）。
            let mut phase = Phase::Asset;
            loop {
                if cancel.load(Ordering::SeqCst) {
                    report.cancelled = true;
                    return Ok(());
                }
                if all_workers_dead {
                    return Ok(());
                }
                let batch = match phase {
                    Phase::Group => {
                        jobq::claim_batch(&conn, COMPLIANCE_GROUP_KIND, COMPLIANCE_BATCH)?
                    }
                    Phase::Asset => jobq::claim_batch(&conn, COMPLIANCE_KIND, COMPLIANCE_BATCH)?,
                };
                if batch.is_empty() {
                    match phase {
                        Phase::Asset => {
                            phase = Phase::Group;
                            continue;
                        }
                        Phase::Group => return Ok(()),
                    }
                }
                let is_group = phase == Phase::Group;
                let mut fresh = 0usize;
                for (job_id, target_id, payload) in batch {
                    // 已处理过的 job：同轮重试 ≤3 次允许重派（attempts 耗尽转 failed 由 jobq 记账）；
                    // 超限归位并结束本轮（重试语义由下一次运行承接）
                    if seen.contains(&job_id) && retries.get(&job_id).copied().unwrap_or(0) >= 3 {
                        back_to_pending(&conn, job_id)?;
                        return Ok(());
                    }
                    seen.insert(job_id);
                    if cancel.load(Ordering::SeqCst) {
                        back_to_pending(&conn, job_id)?;
                        report.cancelled = true;
                        return Ok(());
                    }
                    let queued = if is_group {
                        QueuedJob::Group {
                            job_id,
                            anchor_id: target_id,
                            payload,
                        }
                    } else {
                        QueuedJob::Asset {
                            job_id,
                            asset_id: target_id,
                            payload,
                        }
                    };
                    match tx_job.send(queued) {
                        Ok(()) => fresh += 1,
                        Err(_) => {
                            // 全部 worker 已死：job 归位，结束（错误经统计与 errors 暴露）
                            back_to_pending(&conn, job_id)?;
                            all_workers_dead = true;
                            break;
                        }
                    }
                }
                if all_workers_dead || fresh == 0 {
                    return Ok(());
                }
                let (_, _, pending) = jobq::stats(&conn, COMPLIANCE_KIND)?;
                for _ in 0..fresh {
                    let o = rx_out
                        .recv_timeout(std::time::Duration::from_secs(1800))
                        .map_err(|_| PipelineError::Compliance("worker 结果通道中断".into()))?;
                    if let Some(msg) = &o.error {
                        eprintln!(
                            "compliance job{} asset{} error: {msg}",
                            o.job_id, o.asset_id
                        );
                    }
                    if o.job_id < 0 {
                        // worker 初始化失败（无 job 语义）：计入 errors，结束本轮
                        if report.errors.len() < 50 {
                            report
                                .errors
                                .extend(o.error.clone().map(|e| format!("worker: {e}")));
                        }
                        all_workers_dead = true;
                        continue;
                    }
                    // C9：组级 job 走独立记账，不污染资产口径（total/done/failed/…）
                    if o.group {
                        report.groupTotal += 1;
                        if let Some(msg) = &o.error {
                            eprintln!(
                                "compliance 组级 job{} group{} error: {msg}",
                                o.job_id, o.asset_id
                            );
                            if o.is_failed {
                                report.groupFailed += 1;
                                if report.groupErrors.len() < 50 {
                                    report.groupErrors.push(format!(
                                        "job{}/group{}: {msg}",
                                        o.job_id, o.asset_id
                                    ));
                                }
                            } else {
                                *retries.entry(o.job_id).or_insert(0) += 1;
                                report.groupRetry += 1;
                            }
                        } else if o.skipped {
                            report.groupSkipped += 1;
                            if let Some((rid, rrepo)) = &runs {
                                let _ = rrepo.note_skipped(*rid);
                            }
                        } else {
                            report.groupDone += 1;
                            for e in &o.rule_errors {
                                if report.ruleErrors.len() < RULE_ERROR_CAP {
                                    report.ruleErrors.push(format!("group{} {e}", o.asset_id));
                                }
                            }
                        }
                        continue;
                    }
                    report.total += 1;
                    let index = report.total;
                    let total = pending as usize + index;
                    if let Some(msg) = &o.error {
                        if o.is_failed {
                            report.failed += 1;
                            if report.errors.len() < 50 {
                                report
                                    .errors
                                    .push(format!("job{}/asset{}: {msg}", o.job_id, o.asset_id));
                            }
                        } else {
                            *retries.entry(o.job_id).or_insert(0) += 1;
                            report.retry += 1;
                        }
                    } else if o.skipped {
                        report.skipped += 1;
                        if let Some((rid, rrepo)) = &runs {
                            let _ = rrepo.note_skipped(*rid);
                        }
                    } else {
                        report.done += 1;
                        // C8：规则级失败隔离明细（资产已 done，但有规则被隔离 → 响亮可见）
                        for e in &o.rule_errors {
                            if report.ruleErrors.len() < RULE_ERROR_CAP {
                                report.ruleErrors.push(format!("asset{} {e}", o.asset_id));
                            }
                        }
                        if !o.rule_errors.is_empty() {
                            eprintln!(
                                "compliance asset{} 规则级失败已隔离（{} 条），{} 行判定已落库",
                                o.asset_id,
                                o.rule_errors.len(),
                                o.verdict_rows
                            );
                        }
                    }
                    let status = if o.error.is_some() {
                        if o.is_failed {
                            "failed"
                        } else {
                            "retry"
                        }
                    } else if o.skipped {
                        "skipped"
                    } else {
                        "done"
                    };
                    progress(ComplianceProgress {
                        jobId: o.job_id,
                        assetId: o.asset_id,
                        status: status.into(),
                        index,
                        total,
                        error: o.error,
                    });
                }
            }
        })();
        drop(tx_job); // 关闭 job 通道 → worker 逐个退出（先于 scope join）
        waves
    })?;

    let conn = factory().map_err(|e| PipelineError::Compliance(format!("主连接: {e}")))?;
    let (d, f, p) = jobq::stats(&conn, COMPLIANCE_KIND)?;
    report.jobDone = d;
    report.jobFailed = f;
    report.jobPending = p;
    // C9：组级队列快照（独立口径）
    let (gd, gf, gp) = jobq::stats(&conn, COMPLIANCE_GROUP_KIND)?;
    report.groupJobDone = gd;
    report.groupJobFailed = gf;
    report.groupJobPending = gp;
    let vl_snap = vl_handle.as_ref().map(|_| stats.snapshot());
    Ok((report, vl_snap))
}

/// 单 job = 单资产全管道（pipeline.yaml 步骤序 + 短路 + fixer/复检 + 每步 verdict）。
///
/// C8：`rule_errors` = 装载期 lint 已判定的坏规则（rule id → 错误说明）——这些规则**不执行**，
/// 直接落 `undetermined` + `reason=load_error` 证据；同图其余规则照常判定落库。
#[allow(clippy::too_many_arguments)]
fn process_job(
    conn: &Connection,
    repo: &dyn ComplianceRepo,
    pack: &Pack,
    job_id: i64,
    asset_id: i64,
    payload: Option<&str>,
    vl: Option<&VlHandle>,
    rule_errors: &std::collections::BTreeMap<String, String>,
    // IP-K05：`Some` = 走行业包插件；`None` = legacy 内联引擎
    plugin: Option<&dyn PackJudge>,
    // N6e：动作链的取消旗（沿链传；`None` = 单线程入口不给取消）
    cancel: Option<&AtomicBool>,
) -> Result<AssetOutcome> {
    let payload: CompliancePayload = payload
        .and_then(|p| serde_json::from_str(p).ok())
        .ok_or_else(|| PipelineError::Compliance(format!("job{job_id}: payload 非法")))?;
    // N6e：动作链 job 的 payload.packId 是**插件 id**（处理类包没有配置包对应物），
    // 与 `pack`（run 用的配置包）本就不同 —— 一致性校验挪到链执行处按插件身份做。
    if payload.actions.is_empty() && payload.packId != pack.id {
        // KI-40（真机 run56）：非链处理 run（规格 actions 为空，如「四倍高清」）的
        // payload.packId = 规格插件 id，而本次执行通道开的正是它 —— 与**已开插件**身份
        // 一致即放行。旧守卫只认 meta 配置包，激活了行业包后处理类规格恒被拒
        // （「payload packId=upscale 与装载 pack ecommerce 不一致」，任务三连重试全红）。
        if !plugin
            .map(|p| p.identity().id == payload.packId)
            .unwrap_or(false)
        {
            return Err(PipelineError::Compliance(format!(
                "job{job_id}: payload packId={} 与装载 pack {} 不一致",
                payload.packId, pack.id
            )));
        }
    }
    let snap = repo
        .asset_snapshot(asset_id)?
        .ok_or(PipelineError::AssetNotFound(asset_id))?;
    // CMP-JOB-FIX-1：资产状态分支（已删 / 失联 / 非图片媒体类型）**必须写跳过原因**。
    // 原先这里直接 `return Ok(AssetOutcome::Skipped)`：run 侧照常 `note_skipped(+1)`，
    // 明细里却没有原因（还留着上一次的失败原文）→「跳过 N」与「带原因的条数」对不上，
    // 界面上那一条显示成「已完成 + 失败原因」。原因词表见 [`crate::skip_reason`]。
    if let Some(note) =
        crate::skip_reason::for_asset_state(snap.deleted, snap.missing, &snap.media_type)
    {
        eprintln!("compliance job{job_id} asset{asset_id} 跳过（{note}）");
        mark_skipped(conn, job_id, &note)?;
        return Ok(AssetOutcome::Skipped);
    }
    // CMP-JOB-01：**坏图入口收敛** —— 消费前先看魔数，不看扩展名。
    //
    // 不这么做的后果（BATCH-01 实测）：垃圾字节 `.jpg` / 悬空符号链接一路走到解码 → 每次抛错 →
    // attempts 耗尽 → 成为**永久 failed** 任务，于是「历史累计失败 30」永远挂在界面上，用户
    // 以为系统一直在坏。这类文件不是「判不出来的图」，而是「不是图 / 已经不在了」：正确归宿是
    // skipped（本次没有可判对象），并把原因写进 task 的 lastError（界面上能解释为什么没结果）。
    //
    // 权限不足等其它 IO 错误**不吞**：那是环境问题，必须响亮失败（掩盖它等于假装判过了）。
    match crate::image_probe::probe(Path::new(&snap.abs_path)) {
        crate::image_probe::Probe::Image(_) => {}
        // 文件不在（DB 与磁盘不一致）：照常进管道 —— 元数据档规则可判，像素档规则的解码失败
        // 由 C8 规则级隔离兜住（不把「数据不一致」藏成一次静默跳过）
        crate::image_probe::Probe::Missing(_) => {}
        p if p.should_skip() => {
            let note = p.skip_reason().unwrap_or_else(|| "skipped".to_string());
            eprintln!("compliance job{job_id} asset{asset_id} 跳过（{note}）");
            mark_skipped(conn, job_id, &note)?;
            return Ok(AssetOutcome::Skipped);
        }
        crate::image_probe::Probe::Unreadable(why) => {
            return Err(PipelineError::Compliance(format!(
                "asset{asset_id} 读不出 {}: {why}",
                snap.abs_path
            )))
        }
        crate::image_probe::Probe::NotImage(_) => unreachable!("should_skip 已覆盖"),
    }
    let out_dir = match payload.outDir.as_ref() {
        Some(d) => PathBuf::from(d),
        None => std::env::temp_dir().join(format!("dam-compliance-{job_id}")),
    };
    // N6e：动作链的产物**按 job 隔离**（`<fixes>/job-<id>/`）。
    //
    // 为什么必须隔离：插件产物文件名是「源图 stem + 动作 + 扩展名」，同资产重跑或同名图
    // 落在同一目录时会**互相覆盖**；一旦覆盖，历史版本的 `sha256` 就和磁盘对不上了，
    // 「历史行」变成假账（复盘/回滚都会拿到错的字节）。jobId 天然唯一 → 目录天然不撞。
    let out_dir = if payload.actions.is_empty() {
        out_dir
    } else {
        out_dir.join(format!("job-{job_id}"))
    };
    std::fs::create_dir_all(&out_dir)
        .map_err(|e| PipelineError::Compliance(format!("建产物目录 {out_dir:?}: {e}")))?;

    // 执行通道：`Some` = 处理插件（生产唯一路径）；`None` = 内部维护路径（组级/历史单测，
    // 没有任何用户配置能选中它）。
    // 两条通道产出的行同表；插件通道额外写来源列（0027）。
    //
    // N6e：插件通道上带 `actions[]` 的 job 走**动作链**（逐动作调插件、只落一张工作版本），
    // 不写 `rule_verdict`（处理类包没有审核结论，写空判定会污染「未判定」统计）。
    if let Some(p) = plugin {
        if !payload.actions.is_empty() {
            return run_action_chain_job(conn, &snap, job_id, &out_dir, p, &payload, cancel);
        }
    }
    let executed = match plugin {
        Some(p) => execute_plugin(&snap, job_id, &out_dir, p, &payload.rules)?,
        None => {
            // N1：规则清单只在插件通道有消费方（协议 §3.3 `rules`）。legacy 内联引擎按
            // pack.pipeline 全量跑 —— 「等于全部规则」的清单放行（语义一致，规格只是把
            // 事实写清楚），**严格子集直接报错**（不静默跑全部：那会让「规格只判 3 条」
            // 变成「其实判了 6 条」，结论对不上账）。
            if !payload.rules.is_empty() {
                let same_as_all = payload.rules.len() == pack.rules.len()
                    && pack
                        .rules
                        .iter()
                        .all(|r| payload.rules.iter().any(|x| x == &r.id));
                if !same_as_all {
                    return Err(PipelineError::Compliance(format!(
                        "job{job_id}: legacy 通道不支持规则子集（收到 {} 条，包内 {} 条）：\
                         要么把引擎切到 plugin，要么让规格选全部规则",
                        payload.rules.len(),
                        pack.rules.len()
                    )));
                }
            }
            // C5：vl 规则存在且已配置 → 建资产级 VL 会话（judge/打标 ask 从 pack 静态推导；
            // C8：prompt 前言由 pack 的 `vl_intro` 提供，缺省中性措辞）
            let vl_session = vl.and_then(|h| {
                let (judge_asks, tag_asks) = compliance_vl::asks_from_pack(pack);
                (!judge_asks.is_empty() || !tag_asks.is_empty()).then(|| {
                    VlSession::with_intro(
                        h.cfg.clone(),
                        judge_asks,
                        tag_asks,
                        compliance_vl::pack_vl_intro(pack),
                        h.stats.clone(),
                    )
                })
            });
            let mut ctx = AssetCtx {
                snap,
                rgb: None,
                out_dir,
                asset_id,
                vl: vl_session,
                // DM-C1：重绘区域的本地证据（读失败不炸 job —— 退化为「无 OCR 文本」，
                // 区域决议会按 vl_region_unverified 记 warning，不静默当成已验证）
                ocr_boxes: core_storage::ocr_boxes::load_rects(conn, asset_id).unwrap_or_else(
                    |e| {
                        eprintln!(
                            "compliance asset{asset_id}: 读 OCR 行框失败（按无文本处理）: {e}"
                        );
                        Vec::new()
                    },
                ),
            };
            let (verdicts, isolated) = run_pipeline(&mut ctx, repo, pack, job_id, rule_errors)?;
            Executed {
                verdicts,
                isolated,
                pack_id: pack.id.clone(),
                source: None,
            }
        }
    };
    let verdicts = executed.verdicts;
    let isolated = executed.isolated;
    // 幂等重跑：先清本 job 上次残行再落（崩溃重做不留旧行/重复行）。
    // 清的是**本通道写的那一族的行**（插件通道的 packId 是插件 id，两族互不污染）。
    repo.delete_verdicts(&executed.pack_id, job_id)?;
    for v in &verdicts {
        let id = repo.insert_verdict(v)?;
        if let Some((version, hash)) = &executed.source {
            let n = repo.set_verdict_pack_source(id, version, hash)?;
            if n != 1 {
                return Err(PipelineError::Compliance(format!(
                    "判定来源列写入未命中（verdict id={id}, rows={n}）"
                )));
            }
        }
    }
    // CMP-JOB-FIX-1：判成功即清掉上一轮残留的失败原文（不留下「已完成 + 失败原因」这种假故障）
    clear_last_error(conn, job_id)?;
    Ok(AssetOutcome::Done {
        verdicts: verdicts.len(),
        rule_errors: isolated,
    })
}

/// N6e：动作链 job 的执行 + 落库（一行结果 + 至多一张工作版本）。
///
/// 与判定通道的分工（为什么单独一个函数，而不是塞进 [`execute_plugin`]）：
/// * **不写 `rule_verdict`**：处理类包没有审核结论；塞空判定行会让「未判定」候选查询以为
///   这张图判过了（下次增量判定跳过它），也会让审核表出现没有 verdict 的行；
/// * **产物有去处**：链尾产物落 `asset_versions`（`current=1` 唯一），中间产物在
///   [`crate::compliance_chain::run_chain`] 里用完即删；
/// * **失败的分类**：确定性失败（参数错/协议违例/无产物）→ 直接标 failed，不重试
///   （重试同样失败，只会把任务中心刷成 3 条噪音）；基础设施失败（包拉不起来/崩溃/超时）
///   → 交回 jobq 重试（这是既有的 attempts 语义）。
fn run_action_chain_job(
    conn: &Connection,
    snap: &core_storage::compliance::AssetSnapshot,
    job_id: i64,
    out_dir: &Path,
    plugin: &dyn PackJudge,
    payload: &CompliancePayload,
    cancel: Option<&AtomicBool>,
) -> Result<AssetOutcome> {
    use core_storage::asset_versions::{
        AssetVersionRecord, AssetVersionRepo, SqliteAssetVersionRepo,
    };

    let identity = plugin.identity();
    if identity.id != payload.packId {
        return Err(PipelineError::Compliance(format!(
            "job{job_id}: payload packId={} 与链执行插件身份 {} 不一致",
            payload.packId, identity.id
        )));
    }
    let base = PluginAsset {
        asset_id: snap.asset_id,
        job_id,
        path: snap.abs_path.clone(),
        mime: Some(snap.media_type.clone()),
        width: snap.width.unwrap_or(0).max(0) as u32,
        height: snap.height.unwrap_or(0).max(0) as u32,
        work_dir: out_dir.to_string_lossy().to_string(),
        rule_ids: Vec::new(),
    };
    let no_cancel = AtomicBool::new(false);
    let flag = cancel.unwrap_or(&no_cancel);
    let outcome =
        crate::compliance_chain::run_chain(plugin, &base, &payload.actions, out_dir, flag);

    let repo = SqliteAssetVersionRepo::new(conn);
    let run_id: Option<i64> = conn
        .query_row("SELECT runId FROM job_queue WHERE id = ?1", [job_id], |r| {
            r.get::<_, Option<i64>>(0)
        })
        .unwrap_or(None);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut rec = AssetVersionRecord::new(
        snap.asset_id,
        identity.id.clone(),
        payload.actions.clone(),
        outcome.evidence(),
    );
    rec.job_id = Some(job_id);
    rec.run_id = run_id;
    rec.pack_version = identity.version.clone();
    rec.plugin_hash = identity.plugin_hash.clone();
    rec.duration_ms = outcome.duration_ms as i64;
    rec.created_at = now;
    rec.asset_name = outcome.asset_name.clone();

    // 取消：链在两步之间被叫停 —— 退回 pending（下一轮续跑），本轮记跳过。
    if outcome.cancelled {
        eprintln!(
            "compliance job{job_id} asset{} 动作链取消（已产出的中间产物已清理）",
            snap.asset_id
        );
        back_to_pending(conn, job_id)?;
        return Ok(AssetOutcome::Cancelled);
    }

    match (&outcome.artifact, &outcome.failure) {
        (Some(art), None) => {
            rec.result = "ok".to_string();
            rec.path = Some(art.path.clone());
            rec.sha256 = Some(art.sha256.clone());
            rec.bytes = art.bytes;
            rec.format = art.format.clone();
            rec.width = art.width;
            rec.height = art.height;
            repo.insert(&rec).map_err(|e| {
                PipelineError::Compliance(format!("job{job_id}: 工作版本落库失败: {e}"))
            })?;
            // 链尾 rename = 资产名改写（只改名字，不动路径/字节）
            if let Some(name) = &outcome.asset_name {
                conn.execute(
                    "UPDATE assets SET fileName = ?1 WHERE assetId = ?2",
                    rusqlite::params![name, snap.asset_id],
                )?;
            }
            clear_last_error(conn, job_id)?;
            eprintln!(
                "compliance job{job_id} asset{} 动作链完成 {}（产物 {}，{}ms）",
                snap.asset_id,
                payload.actions.join("→"),
                art.path,
                outcome.duration_ms
            );
            Ok(AssetOutcome::Done {
                verdicts: 0,
                rule_errors: Vec::new(),
            })
        }
        (_, failure) => {
            let f = failure.clone().unwrap_or_else(|| {
                crate::compliance_chain::ChainFailure::from_message(
                    "",
                    "action_no_product",
                    "",
                    "动作链没有产出工作版本",
                )
            });
            rec.result = "failed".to_string();
            rec.error_code = Some(f.code.to_string());
            rec.error_message = Some(if f.action.is_empty() {
                f.message.clone()
            } else {
                format!("{}（失败动作 `{}`）", f.message, f.action)
            });
            // 失败行也落：这是「这张图这一次处理为什么没成」的唯一可复盘证据（不留产物）
            repo.insert(&rec).map_err(|e| {
                PipelineError::Compliance(format!("job{job_id}: 链失败行落库失败: {e}"))
            })?;
            let msg = format!(
                "asset{} 动作链失败于 `{}`：{} [{}]",
                snap.asset_id,
                if f.action.is_empty() { "-" } else { &f.action },
                f.message,
                f.code
            );
            // 基础设施类失败值得重试；确定性失败重试只会重复同样的错
            let retryable = f.code == "plugin_failure"
                && matches!(
                    f.kind.as_str(),
                    "plugin_unavailable" | "plugin_crash" | "plugin_timeout"
                );
            if retryable {
                return Err(PipelineError::Compliance(msg));
            }
            // KI40-FIX：**job 行必须跟着失败**。以前这里直接 `UPDATE ... status='failed'`
            // 却返回 `Done`，调用方紧接着 `jobq::complete` 把它刷回 done —— 结果表说
            // 「执行失败」，run 汇总却 `failed=0 / done=1`（任务中心「成功 1 · 失败 0」），
            // 界面的失败原因块又按 `status==failed` 过滤成空。
            // `lastError` 与结果行 `reason` 用同一段文本：一处展示两处同源。
            let user_msg = rec.error_message.clone().unwrap_or_else(|| msg.clone());
            jobq::fail_terminal(conn, job_id, &user_msg)?;
            eprintln!("compliance job{job_id} {msg}");
            Ok(AssetOutcome::Failed { message: user_msg })
        }
    }
}

/// 一次 job 的执行结果（两条通道的统一出口）。
struct Executed {
    verdicts: Vec<NewVerdict>,
    /// 规则级失败/降级明细（进运行报告的 `ruleErrors`：响亮可见，不当成功）
    isolated: Vec<String>,
    /// 这批行落在哪个 packId 下（legacy = 运行包；plugin = 插件包 id）
    pack_id: String,
    /// 插件来源列（`packVersion`, `pluginHash`）；legacy = None
    source: Option<(String, String)>,
}

/// IP-K05：插件通道执行一个资产 job（`judge` → 必要时 `repair` → 映射成判定行）。
///
/// 三条硬口径：
/// 1. **失败绝不冒充结论**：插件失败 = 包内每条规则一行 `undetermined` + `reason`，
///    合格/不合格都不占（报告里「未判定」会涨，这正是事实）；
/// 2. **失败绝不炸宿主**：只有「连包内有哪些规则都不知道」才向上 Err（那说明包没装载成功，
///    属于环境问题，必须让 attempts/重试与运行报告把它暴露出来）；
/// 3. **修复失败不推翻判定**：judge 的结论保留，`repair` 的错误写进 evidence。
fn execute_plugin(
    snap: &core_storage::compliance::AssetSnapshot,
    job_id: i64,
    out_dir: &Path,
    plugin: &dyn PackJudge,
    // N1：规格选了哪些规则（空 = 全部，协议 §3.3）
    rule_ids: &[String],
) -> Result<Executed> {
    let pack = plugin.identity();
    if pack.rule_ids.is_empty() {
        return Err(PipelineError::Compliance(format!(
            "行业包 {}@{} 未声明任何规则（manifest/包体损坏？）",
            pack.id, pack.version
        )));
    }
    let req = PluginAsset {
        asset_id: snap.asset_id,
        job_id,
        path: snap.abs_path.clone(),
        mime: (!snap.media_type.trim().is_empty()).then(|| snap.media_type.clone()),
        width: snap.width.and_then(|v| u32::try_from(v).ok()).unwrap_or(0),
        height: snap.height.and_then(|v| u32::try_from(v).ok()).unwrap_or(0),
        work_dir: out_dir.to_string_lossy().to_string(),
        // N1：规格的规则子集（空 = 包内全部规则，协议 §3.3「省略 = 全部规则」）
        rule_ids: rule_ids.to_vec(),
    };
    let judged = plugin.judge_asset(&req);
    let mut isolated = Vec::new();
    if let Some((kind, detail)) = &judged.failure {
        isolated.push(format!(
            "asset{} plugin {}({}): {detail}",
            snap.asset_id,
            kind.reason(),
            pack.id
        ));
    }
    // 只有判出了 fail 才修（协议 §3.4：修复后必须按规则复判，不是「改完就算过」）。
    // 判不了（undetermined）/没判（skipped）不修 —— 拿不准的东西去重绘等于造假。
    let repaired = if judged.failure.is_none() {
        let failed: Vec<String> = judged
            .verdicts
            .iter()
            .filter(|v| v.status == verdict::FAIL)
            .map(|v| v.rule_id.clone())
            .collect();
        if failed.is_empty() {
            None
        } else {
            let rep = plugin.repair_asset(&PluginAsset {
                rule_ids: failed,
                ..req
            });
            if let Some((kind, detail)) = &rep.failure {
                isolated.push(format!(
                    "asset{} plugin repair {}({}): {detail}",
                    snap.asset_id,
                    kind.reason(),
                    pack.id
                ));
            }
            Some(rep)
        }
    } else {
        None
    };
    let rows = plugin_rows_scoped(&judged, repaired.as_ref(), rule_ids)
        .into_iter()
        .map(|sv| sv.into_new(&pack.id, snap.asset_id, job_id))
        .collect();
    Ok(Executed {
        verdicts: rows,
        isolated,
        pack_id: pack.id.clone(),
        source: Some((pack.version.clone(), pack.plugin_hash.clone())),
    })
}

// ---------- C9：组级 job 执行（一个 job = 一个组 × 一个 group_by 作用域） ----------

/// 组成员的执行事实（判前一次性取好：快照 + 去重派生数据）。
struct GroupMemberCtx {
    asset_id: i64,
    abs_path: String,
    /// 文件失联/已删/非图片 → 该组「不可判」（响亮降级为 undetermined）
    unusable: bool,
    file_hash: Option<String>,
    phash: Option<u64>,
    phash_source: &'static str,
    file_hash_source: &'static str,
}

impl GroupMemberCtx {
    fn to_input(&self) -> crate::compliance_group::GroupMemberInput {
        crate::compliance_group::GroupMemberInput {
            asset_id: self.asset_id,
            file_hash: self.file_hash.clone(),
            phash: self.phash,
            phash_source: self.phash_source,
            file_hash_source: self.file_hash_source,
        }
    }
}

/// 取成员的去重派生数据：库里有就用库里的，没有就**按文件现算**
/// （`resource.pHash` 是导入期产物，历史库/快速导入路径可能没有；现算保证
/// 「库里有能力、只是没算过」不会变成「判定不出来」）。
///
/// 现算失败（文件读不出/解码失败）→ 返回 None（`*_source = none`），
/// 由调用方按「成员不可判」降级 —— 绝不静默当成「不同图」。
fn dup_data_for(
    abs_path: &str,
    file_hash: Option<String>,
    p_hash: Option<String>,
) -> (Option<String>, Option<u64>, &'static str, &'static str) {
    let (fh, fh_src) = match file_hash.filter(|h| !h.trim().is_empty()) {
        Some(h) => (Some(h), "db"),
        None => match std::fs::read(abs_path) {
            Ok(bytes) => (Some(core_hash::sha256_hex(&bytes)), "computed"),
            Err(_) => (None, "none"),
        },
    };
    let (ph, ph_src) = match p_hash
        .as_deref()
        .and_then(|h| u64::from_str_radix(h, 16).ok())
    {
        Some(p) => (Some(p), "db"),
        None => match core_media::luma32(Path::new(abs_path))
            .ok()
            .and_then(|l| core_hash::phash(&l).ok())
        {
            Some(p) => (Some(p), "computed"),
            None => (None, "none"),
        },
    };
    (fh, ph, ph_src, fh_src)
}

/// 组级判定（无 fixer 版）的 fail 决策表：与逐图 `handle_fix` 的无修复器分支同口径。
/// `on_fail: pass` → `fail`（不影响合格结论，报告聚合时按软 fail 排除）；
/// 其余 → `fail` + `reason=no_fixer`（组级规则不允许带 fixer，由 L0 装载期拒绝）。
fn group_fail_verdict(rule: &Rule, evidence: serde_json::Value, score: f64) -> StepVerdict {
    let mut sv = match rule.on_fail {
        RuleOnFail::Pass => StepVerdict::new(
            rule.id.clone(),
            verdict::FAIL,
            merge_evidence(evidence, serde_json::json!({ "on_fail": "pass" })),
        ),
        // 组级规则不允许带 fixer（装载期拒绝）→ 撞上 on_fail=retry_then_human 就是
        // 「没有修复器可修」= 该规则不合格，不是「机器不敢判」。这正是房产包
        // 「27 张里不合格 0」的根因：过去这里一律落 pending_human，于是组级语义规则判了等于没判。
        _ => StepVerdict::new(
            rule.id.clone(),
            verdict::FAIL,
            merge_evidence(
                evidence,
                serde_json::json!({ "reason": "no_fixer", "scope": "group" }),
            ),
        ),
    };
    sv.score = Some(score);
    sv
}

/// 组级降级 verdict（组键缺失 / 组内成员不可判 / 装载期坏规则）——一律 `pending_human` +
/// 明确的 `reason`（响亮：既不判合格、也不判不合格，交人工）。
fn group_degraded_verdict(rule_id: &str, evidence: serde_json::Value) -> StepVerdict {
    StepVerdict::new(rule_id, verdict::UNDETERMINED, evidence)
}

/// 组级规则的**跨作用域短路**（COMPLIANCE-DOMAIN）：ruleId → 该规则的短路门。
///
/// 为什么需要单独一套：`depends_on` 在**资产 job** 里天然生效（依赖规则就在同一条执行序列上），
/// 但组级规则由**组级 job** 执行 —— 逐图规则根本不在组级 job 的执行序列里，只按「同 job 结果表」
/// 判断就等于「声明了但没人读」（C7 缺口 G5 的同类问题）。所以这里按成员的**逐图判定行**
/// （`judgementScope != per_group`）取上游结论。
///
/// 语义边界（写清，不猜）：
/// - 上游判定行**存在且 fail** → 该成员被排除出组级判定（「狗的照片」不该成为「缺厨房」的证据，
///   也不该参与重复检测），落 `skipped` + `short_circuited_by` 证据；
/// - 上游行**不存在**（资产 job 还没跑 / 上游规则被隔离后没落库）→ 该成员**参与**判定，
///   证据里记 `gatePending`（不静默当成通过，也不假装它 fail）。执行序已改为
///   「逐图 → 组级」（见 `run_compliance_queue_par`），正常跑法下不会出现 pending；
/// - 依赖指向**组级规则**的不算跨作用域（同 job 内由声明序保证先跑，不在这里处理）。
#[derive(Debug, Clone, Default)]
struct GroupGate {
    /// 被上游逐图规则判 fail 的成员（不进组级判定、不产生 VL 调用）
    failed: Vec<i64>,
    /// 上游判定行缺失的成员（参与判定，证据里如实标注）
    pending: Vec<i64>,
    /// 触发短路的依赖规则 id（证据用）
    deps: Vec<String>,
}

fn group_short_circuits(
    repo: &dyn ComplianceRepo,
    pack: &Pack,
    members: &[GroupMemberCtx],
    deps_of: &BTreeMap<String, Vec<String>>,
) -> Result<BTreeMap<String, GroupGate>> {
    let mut out: BTreeMap<String, GroupGate> = BTreeMap::new();
    // 「组级规则 → 逐图规则」的依赖对（其余依赖不归本处处理）
    let cross: Vec<(String, Vec<String>)> = deps_of
        .iter()
        .filter(|(rid, deps)| {
            !deps.is_empty() && pack.rule(rid).map(|r| r.is_group_scoped()).unwrap_or(false)
        })
        .map(|(rid, deps)| {
            let per_asset: Vec<String> = deps
                .iter()
                .filter(|d| pack.rule(d).map(|r| !r.is_group_scoped()).unwrap_or(false))
                .cloned()
                .collect();
            (rid.clone(), per_asset)
        })
        .filter(|(_, deps)| !deps.is_empty())
        .collect();
    if cross.is_empty() {
        return Ok(out);
    }
    // 每成员查一次逐图判定行（组级 job 的组大小本来就不大；不做 N×M 次查询）
    let mut rows_by_asset: BTreeMap<i64, Vec<core_storage::compliance::VerdictRow>> =
        BTreeMap::new();
    for m in members {
        let rows = repo.find_asset_verdicts(&pack.id, m.asset_id)?;
        rows_by_asset.insert(m.asset_id, rows);
    }
    for (rid, deps) in cross {
        let mut g = GroupGate {
            deps: deps.clone(),
            ..Default::default()
        };
        for m in members {
            let rows = rows_by_asset.get(&m.asset_id).cloned().unwrap_or_default();
            let mut any_present = false;
            let mut any_failed = false;
            for d in &deps {
                // 逐图口径的最新一行（组级行不参与：它们的结论不是上游 gate）
                let row = rows
                    .iter()
                    .filter(|r| &r.rule_id == d)
                    .filter(|r| r.judgement_scope.as_deref() != Some(VERDICT_SCOPE_GROUP))
                    .max_by_key(|r| r.id);
                if let Some(r) = row {
                    any_present = true;
                    if r.result == verdict::FAIL {
                        any_failed = true;
                    }
                }
            }
            if any_failed {
                g.failed.push(m.asset_id);
            } else if !any_present {
                // 上游行一个都没有 → 参与判定但在证据里标注（不猜 fail、不假装通过）
                g.pending.push(m.asset_id);
            }
        }
        if !g.failed.is_empty() || !g.pending.is_empty() {
            out.insert(rid, g);
        }
    }
    Ok(out)
}

/// 组级 job 全流程（pipeline 声明序，只执行本作用域的 per_group judge 步骤）。
///
/// 降级语义（卡要求「写清」，这里是唯一实现处）：
/// 1. **组键缺失**（payload.groupKey = None）→ 每条规则落 `pending_human` +
///    `reason=group_key_missing`（成员清单进证据）；
/// 2. **组内成员不可判**（失联/已删/非图片）→ 该组全部组级规则落 `pending_human` +
///    `reason=group_incomplete` + `badMembers`（不拿残缺的组冒充结论）；
/// 3. **检测器所需派生数据缺失**（phash_dup 需要 pHash/fileHash）→ `pending_human` +
///    `reason=group_incomplete` + `membersWithoutDupData`；
/// 4. **装载期已知坏规则**（`rule_errors`）→ `pending_human` + `reason=load_error`（C8 同口径）；
/// 5. **VL 通道抖动**（HTTP/应答/词表外答案）→ `Err` 向上（job 级重试，attempts 耗尽转
///    failed，响亮）——与逐图通道完全一致，组级不另立一套静默兜底；
/// 6. 判定行**按成员展开**落库（组内每成员一行、内容逐字相同，`judgementScope=per_group`
///    + `groupKey`），使库视图筛选/报告导出/人工反馈等按 assetId 的读路径零改动可查。
#[allow(clippy::too_many_arguments)]
fn process_group_job(
    conn: &Connection,
    repo: &dyn ComplianceRepo,
    pack: &Pack,
    job_id: i64,
    payload: Option<&str>,
    vl: Option<&VlHandle>,
    rule_errors: &std::collections::BTreeMap<String, String>,
) -> Result<AssetOutcome> {
    let payload: GroupPayload = payload
        .and_then(|p| serde_json::from_str(p).ok())
        .ok_or_else(|| PipelineError::Compliance(format!("组级 job{job_id}: payload 非法")))?;
    if payload.packId != pack.id {
        return Err(PipelineError::Compliance(format!(
            "组级 job{job_id}: payload packId={} 与装载 pack {} 不一致",
            payload.packId, pack.id
        )));
    }
    if payload.members.is_empty() {
        return Err(PipelineError::Compliance(format!(
            "组级 job{job_id}: members 为空（组级 job 至少一个成员）"
        )));
    }
    // 本作用域的组级规则（pipeline 声明序 = 执行序）
    let rules: Vec<&Rule> = pack
        .pipeline
        .steps
        .iter()
        .filter(|s| s.kind == StepKind::Judge)
        .filter_map(|s| s.rule.as_deref().and_then(|r| pack.rule(r)))
        .filter(|r| {
            r.is_group_scoped()
                && r.group_by.as_ref().map(|g| g.scope_key()) == Some(payload.scope.clone())
        })
        .collect();
    if rules.is_empty() {
        return Err(PipelineError::Compliance(format!(
            "组级 job{job_id}: 作用域 {} 在 pack {} 里没有对应组级规则",
            payload.scope, pack.id
        )));
    }

    // 成员事实（一次取齐）
    let mut members: Vec<GroupMemberCtx> = Vec::with_capacity(payload.members.len());
    for &asset_id in &payload.members {
        let snap = repo
            .asset_snapshot(asset_id)?
            .ok_or(PipelineError::AssetNotFound(asset_id))?;
        let unusable = snap.deleted == 1
            || snap.missing == 1
            || snap.media_type != "image"
            // CMP-JOB-01：与逐图通道同口径 —— **不是图**的成员属于「不可判」，不能当证据
            // 参与组内聚合（否则一个坏文件会让整个组级 job 反复失败）。
            || crate::image_probe::probe(Path::new(&snap.abs_path)).should_skip();
        let (file_hash, phash, phash_source, file_hash_source) =
            dup_data_for(&snap.abs_path, snap.file_hash, snap.p_hash);
        members.push(GroupMemberCtx {
            asset_id,
            abs_path: snap.abs_path,
            unusable,
            file_hash,
            phash,
            phash_source,
            file_hash_source,
        });
    }

    let group_key = payload.groupKey.clone();
    let _member_ids: Vec<i64> = members.iter().map(|m| m.asset_id).collect();
    let _bad_members: Vec<i64> = members
        .iter()
        .filter(|m| m.unusable)
        .map(|m| m.asset_id)
        .collect();

    // COMPLIANCE-DOMAIN：组级规则的短路依赖（pipeline 声明序里的 depends_on）+ 跨作用域门。
    // 上游逐图规则判 fail 的成员不进组级判定（不参与聚合、不产生 VL 调用）。
    let deps_of: BTreeMap<String, Vec<String>> = pack
        .pipeline
        .steps
        .iter()
        .filter(|s| s.kind == StepKind::Judge)
        .filter_map(|s| s.rule.clone().map(|r| (r, s.depends_on.clone())))
        .collect();
    let gate = group_short_circuits(repo, pack, &members, &deps_of)?;

    let mut verdicts: Vec<NewVerdict> = Vec::new();
    let mut isolated: Vec<String> = Vec::new();
    let out_dir = match payload.outDir.clone() {
        Some(d) => PathBuf::from(d),
        None => std::env::temp_dir().join(format!("dam-compliance-group-{job_id}")),
    };
    // 组级 vl 会话（惰性：只有真需要提组级 vl 问题才建）
    let vl_intro = compliance_vl::pack_vl_intro(pack);
    let mut vl_session: Option<VlSession> = None;

    for rule in &rules {
        // 本规则被上游逐图规则短路的成员（空 = 全组参与）；active = 真正参与组级判定的成员
        let g = gate.get(&rule.id).cloned().unwrap_or_default();
        let gated: Vec<i64> = g.failed.clone();
        let short_deps: Vec<String> = deps_of
            .get(&rule.id)
            .map(|d| {
                d.iter()
                    .filter(|x| pack.rule(x).map(|r| !r.is_group_scoped()).unwrap_or(false))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let gate_pending: Vec<i64> = g.pending.clone();
        let active: Vec<&GroupMemberCtx> = members
            .iter()
            .filter(|m| !gated.contains(&m.asset_id))
            .collect();
        let active_ids: Vec<i64> = active.iter().map(|m| m.asset_id).collect();
        let bad_active: Vec<i64> = active
            .iter()
            .filter(|m| m.unusable)
            .map(|m| m.asset_id)
            .collect();
        let sv: StepVerdict = 'rule: {
            if !gated.is_empty() && active.is_empty() {
                // 全组被上游逐图 gate 短路：一行判定都不做（不建 VL 会话、不跑检测器、不落修复）
                break 'rule StepVerdict::new(
                    rule.id.clone(),
                    verdict::SKIPPED,
                    serde_json::json!({
                        "scope": "group",
                        "reason": "short_circuited_by",
                        "shortCircuitedBy": short_deps,
                        "groupKey": group_key,
                        "members": active_ids,
                        "detail": "组内全部成员被上游逐图规则判 fail → 本组级规则整组短路（不产生判定与 VL 调用）",
                    }),
                );
            }
            if group_key.is_none() {
                break 'rule group_degraded_verdict(
                    &rule.id,
                    serde_json::json!({
                        "scope": "group", "reason": "group_key_missing",
                        "groupBy": payload.scope, "members": active_ids,
                        "detail": "组键取不到（group_by 源为空）→ 该成员无法归组，交人工",
                    }),
                );
            } else if !bad_active.is_empty() {
                break 'rule group_degraded_verdict(
                    &rule.id,
                    serde_json::json!({
                        "scope": "group", "reason": "group_incomplete",
                        "groupKey": group_key, "members": active_ids, "badMembers": bad_active, "gatePending": gate_pending,
                        "detail": "组内有成员不可判（失联/已删/非图片）→ 不拿残缺的组冒充结论",
                    }),
                );
            } else if let Some(msg) = rule_errors.get(&rule.id) {
                isolated.push(format!(
                    "group<{}> rule{}: 装载期错误已隔离（{msg}）",
                    group_key.as_deref().unwrap_or("<no_key>"),
                    rule.id
                ));
                break 'rule group_degraded_verdict(
                    &rule.id,
                    serde_json::json!({
                        "scope": "group", "reason": "load_error", "error": msg,
                        "groupKey": group_key, "members": active_ids,
                    }),
                );
            }
            match rule.executor {
                RuleExecutor::Classic => {
                    let input = crate::compliance_group::GroupInput {
                        group_key: group_key.clone(),
                        members: active.iter().map(|m| m.to_input()).collect(),
                    };
                    let no_data: Vec<i64> = active
                        .iter()
                        .filter(|m| m.file_hash.is_none() && m.phash.is_none())
                        .map(|m| m.asset_id)
                        .collect();
                    // phash_dup 需要派生数据；都没有 = 该组不可判（响亮转人工）
                    if rule.check.as_ref().map(|c| c.detector.as_str()) == Some("phash_dup")
                        && !no_data.is_empty()
                    {
                        group_degraded_verdict(
                            &rule.id,
                            serde_json::json!({
                                "scope": "group", "reason": "group_incomplete",
                                "groupKey": group_key, "members": active_ids,
                                "membersWithoutDupData": no_data,
                                "detail": "成员拿不到 pHash/fileHash（现算也失败）→ 重复检测不可判",
                            }),
                        )
                    } else {
                        let params = rule
                            .check
                            .as_ref()
                            .map(|c| c.params.clone())
                            .unwrap_or(serde_yaml::Value::Null);
                        let detector = rule
                            .check
                            .as_ref()
                            .map(|c| c.detector.clone())
                            .unwrap_or_default();
                        let out = crate::compliance_group::run(&detector, &input, &params)
                            .map_err(|e| {
                                PipelineError::Compliance(format!(
                                    "组级 job{job_id} rule{}: {e}",
                                    rule.id
                                ))
                            })?;
                        if out.pass {
                            let mut sv =
                                StepVerdict::new(rule.id.clone(), verdict::PASS, out.evidence);
                            sv.score = Some(out.score);
                            sv
                        } else {
                            group_fail_verdict(rule, out.evidence, out.score)
                        }
                    }
                }
                RuleExecutor::Vl => {
                    // 逐成员归类 → 组内汇总（缺场景 = require_all 里没出现过的选项）
                    let asks = compliance_vl::group_asks_from_pack(pack);
                    let ask = asks.iter().find(|a| a.rule_id == rule.id).ok_or_else(|| {
                        PipelineError::Compliance(format!(
                            "组级 job{job_id} rule{}: 无组级 vl 提问（params 非法或检测器未知，\
                             装载期应已报错）",
                            rule.id
                        ))
                    })?;
                    let session = match vl_session.as_mut() {
                        Some(s) => s,
                        None => {
                            let Some(h) = vl else {
                                // 未配置 vl 通道：与逐图通道同口径 → skipped(executor_not_available)
                                // （不是 job 失败：没配 VL 是部署态能力缺失，与逐图 vl 规则一致）
                                break 'rule StepVerdict::new(
                                    rule.id.clone(),
                                    verdict::SKIPPED,
                                    serde_json::json!({
                                        "scope": "group", "reason": "executor_not_available",
                                        "executor": "vl", "groupKey": group_key,
                                        "members": active_ids,
                                    }),
                                );
                            };
                            vl_session = Some(VlSession::with_intro(
                                h.cfg.clone(),
                                Vec::new(),
                                Vec::new(),
                                vl_intro.clone(),
                                h.stats.clone(),
                            ));
                            vl_session.as_mut().expect("just built")
                        }
                    };
                    let probe_max = ask.probe_max as usize;
                    let (probed, unprobed): (Vec<&GroupMemberCtx>, Vec<i64>) =
                        if probe_max == 0 || probe_max >= active.len() {
                            (active.clone(), Vec::new())
                        } else {
                            (
                                active.iter().take(probe_max).cloned().collect(),
                                active.iter().skip(probe_max).map(|m| m.asset_id).collect(),
                            )
                        };
                    let mut per_member: Vec<serde_json::Value> = Vec::new();
                    let mut seen: Vec<String> = Vec::new();
                    for m in &probed {
                        let label = session
                            .probe_group_label(Path::new(&m.abs_path), ask)
                            .map_err(|e| {
                                PipelineError::Compliance(format!(
                                    "组级 job{job_id} rule{} asset{}: {e}",
                                    rule.id, m.asset_id
                                ))
                            })?;
                        if !seen.contains(&label) {
                            seen.push(label.clone());
                        }
                        per_member.push(serde_json::json!({
                            "assetId": m.asset_id,
                            "label": label,
                        }));
                    }
                    let missing: Vec<String> = ask
                        .require_all
                        .iter()
                        .filter(|r| !seen.contains(r))
                        .cloned()
                        .collect();
                    let evidence = serde_json::json!({
                        "detector": "vl_group_judge",
                        "scope": "group",
                        "groupKey": group_key,
                        "members": active_ids,
                        "requireAll": ask.require_all,
                        "seen": seen,
                        "missing": missing,
                        "perMember": per_member,
                        "unprobed": unprobed,
                        "probeMax": ask.probe_max,
                    });
                    if missing.is_empty() {
                        let mut sv = StepVerdict::new(rule.id.clone(), verdict::PASS, evidence);
                        sv.score = Some(0.0);
                        sv
                    } else {
                        group_fail_verdict(rule, evidence, missing.len() as f64)
                    }
                }
                RuleExecutor::LocalModel => group_degraded_verdict(
                    &rule.id,
                    serde_json::json!({
                        "scope": "group", "reason": "executor_not_available",
                        "detail": "per_group 不支持 local_model 执行档",
                        "groupKey": group_key, "members": active_ids,
                    }),
                ),
            }
        };
        // 按成员展开落库（参与判定的成员内容逐字相同；被上游短路的成员落 skipped + 短路证据）
        let mut sv = with_fail_label(sv.with_rule_confidence(Some(rule)), rule);
        sv.evidence = merge_evidence(
            sv.evidence,
            serde_json::json!({
                "groupKey": group_key,
                "scope": "group",
                "members": active_ids,
                "shortCircuitedMembers": gated,
                "gatePending": gate_pending,
                "ruleId": rule.id,
            }),
        );
        for m in &members {
            let mut v = if gated.contains(&m.asset_id) {
                let mut skip = StepVerdict::new(
                    rule.id.clone(),
                    verdict::SKIPPED,
                    serde_json::json!({
                        "scope": "group",
                        "reason": "short_circuited_by",
                        "shortCircuitedBy": g.deps,
                        "groupKey": group_key,
                        "ruleId": rule.id,
                        // 组内参与判定的成员 / 被短路的成员（与正常行同结构，方便前端按行渲染）
                        "members": active_ids,
                        "shortCircuitedMembers": gated,
                        "detail": "上游逐图规则判 fail → 本组级规则对该成员短路（不参与组内聚合）",
                    }),
                );
                // 无判定语义的行（skipped）置信度 = 1.0「确定没有结论」（父卡口径）
                skip.confidence = Some(1.0);
                skip.into_new(&pack.id, m.asset_id, job_id)
            } else {
                sv.clone().into_new(&pack.id, m.asset_id, job_id)
            };
            v.judgement_scope = Some(VERDICT_SCOPE_GROUP.to_string());
            v.group_key = group_key.clone();
            verdicts.push(v);
        }
    }

    let _ = out_dir; // v1 组级判定不落修复产物（组级规则不允许 fixer）
    let _ = conn;
    repo.delete_verdicts(&pack.id, job_id)?;
    for v in &verdicts {
        repo.insert_verdict(v)?;
    }
    Ok(AssetOutcome::Done {
        verdicts: verdicts.len(),
        rule_errors: isolated,
    })
}

/// COMPLIANCE-DOMAIN：把规则声明的**不合格业务原因**（`fail_label`）写进证据。
///
/// 只在 `fail` 行上加 `failReason`：合格/未判定行没有「为什么不合格」可言，不塞噪声。
fn with_fail_label(sv: StepVerdict, rule: &Rule) -> StepVerdict {
    let Some(label) = rule.fail_label.as_deref() else {
        return sv;
    };
    if sv.result != verdict::FAIL {
        return sv;
    }
    let mut sv = sv;
    sv.evidence = merge_evidence(sv.evidence, serde_json::json!({ "failReason": label }));
    sv
}

/// 证据合并：后写的键覆盖前者（组级上下文与检测器证据合并在一个 json 里，便于审计）。
fn merge_evidence(base: serde_json::Value, extra: serde_json::Value) -> serde_json::Value {
    match (base, extra) {
        (serde_json::Value::Object(mut a), serde_json::Value::Object(b)) => {
            for (k, v) in b {
                a.insert(k, v);
            }
            serde_json::Value::Object(a)
        }
        (a, _) => a,
    }
}

/// 资产执行上下文：元数据快照 + 惰性像素解码（成本升序：能不解码就不解码，
/// 第一个真正执行的像素档步骤才解码一次，全管道复用）。
struct AssetCtx {
    snap: core_storage::compliance::AssetSnapshot,
    rgb: Option<DetectorInput>,
    out_dir: PathBuf,
    asset_id: i64,
    /// C5：VL 会话（vl 规则资产级合并调用缓存；未配置/无 vl 规则 = None）
    vl: Option<VlSession>,
    /// DM-C1：OCR 文本行框（归一化 0..1；migration 0034）—— inpaint 重绘区域的本地证据，
    /// 与 VL bbox 交叉验证（[`crate::inpaint_region`]）。空 = 这张图没跑过 OCR / 没有文本。
    ocr_boxes: Vec<[f64; 4]>,
}

impl AssetCtx {
    /// 按检测器输入档取输入（Metadata = DB 宽高；Rgb = 惰性解码）。
    fn detector_input(&mut self, kind: InputKind) -> std::result::Result<DetectorInput, String> {
        match kind {
            InputKind::Metadata => {
                let w = self
                    .snap
                    .width
                    .and_then(|w| u32::try_from(w).ok())
                    .ok_or_else(|| {
                        format!(
                            "asset{}: primary resource 缺宽度（元数据档不可判）",
                            self.asset_id
                        )
                    })?;
                let h = self
                    .snap
                    .height
                    .and_then(|h| u32::try_from(h).ok())
                    .ok_or_else(|| {
                        format!(
                            "asset{}: primary resource 缺高度（元数据档不可判）",
                            self.asset_id
                        )
                    })?;
                Ok(DetectorInput::Metadata {
                    width: w,
                    height: h,
                })
            }
            InputKind::Rgb => {
                if self.rgb.is_none() {
                    let (w, h, px) =
                        core_media::rgb8(Path::new(&self.snap.abs_path)).map_err(|e| {
                            format!(
                                "asset{} 解码失败 {}: {e}",
                                self.asset_id, self.snap.abs_path
                            )
                        })?;
                    let w = u32::try_from(w)
                        .map_err(|_| format!("asset{}: 宽度非法 {w}", self.asset_id))?;
                    let h = u32::try_from(h)
                        .map_err(|_| format!("asset{}: 高度非法 {h}", self.asset_id))?;
                    self.rgb = Some(DetectorInput::Rgb {
                        width: w,
                        height: h,
                        pixels: px,
                    });
                }
                Ok(self.rgb.clone().expect("刚刚填充"))
            }
        }
    }

    /// 对某个具体文件（修复/do 产物）构造检测器输入。
    fn file_input(
        &self,
        path: &Path,
        kind: InputKind,
    ) -> std::result::Result<DetectorInput, String> {
        match kind {
            InputKind::Metadata => {
                let (w, h) = core_media::image_size(path)
                    .map_err(|e| format!("读产物尺寸 {path:?}: {e}"))?;
                let w = u32::try_from(w).map_err(|_| format!("产物宽度非法 {w}"))?;
                let h = u32::try_from(h).map_err(|_| format!("产物高度非法 {h}"))?;
                Ok(DetectorInput::Metadata {
                    width: w,
                    height: h,
                })
            }
            InputKind::Rgb => {
                let (w, h, px) =
                    core_media::rgb8(path).map_err(|e| format!("解码产物 {path:?}: {e}"))?;
                let w = u32::try_from(w).map_err(|_| format!("产物宽度非法 {w}"))?;
                let h = u32::try_from(h).map_err(|_| format!("产物高度非法 {h}"))?;
                Ok(DetectorInput::Rgb {
                    width: w,
                    height: h,
                    pixels: px,
                })
            }
        }
    }
}

/// 单步产出的 verdict（insert 前的内存形态）。
#[derive(Clone)]
struct StepVerdict {
    rule_id: String,
    result: String,
    /// 置信度（None = 未定；`into_new` 兜底 1.0 只给无判定语义的行：do 步骤/skipped/打标）
    confidence: Option<f64>,
    score: Option<f64>,
    evidence: serde_json::Value,
    fix_action: Option<String>,
    fix_tier: Option<String>,
    recheck: Option<String>,
}

impl StepVerdict {
    fn new(rule_id: impl Into<String>, result: &str, evidence: serde_json::Value) -> Self {
        Self {
            rule_id: rule_id.into(),
            result: result.to_string(),
            confidence: None,
            score: None,
            evidence,
            fix_action: None,
            fix_tier: None,
            recheck: None,
        }
    }

    /// 按规则声明算置信度（唯一入口：判定行的 confidence 不许由调用方手填）。
    ///
    /// COMPLIANCE-DOMAIN：`source: vl_self_report` 的规则**不重算** —— 它的置信度是 VL 通道
    /// 逐图自评的值（已由 judge_rule 填好）。在这里用余量/常数覆盖，就等于拿 pack 拍的数
    /// 冒充模型把握度（父卡明确禁止）。
    fn with_rule_confidence(mut self, rule: Option<&Rule>) -> Self {
        if rule.map(is_vl_self_report).unwrap_or(false) {
            return self;
        }
        self.confidence = Some(verdict_confidence(rule, self.score));
        self
    }

    fn into_new(self, pack_id: &str, asset_id: i64, job_id: i64) -> NewVerdict {
        NewVerdict {
            pack_id: pack_id.to_string(),
            asset_id,
            job_id,
            rule_id: self.rule_id,
            result: self.result,
            // 无判定语义的行（do 步骤 / skipped / 打标）→ 1.0 =「确定没有结论」
            confidence: self.confidence.unwrap_or(1.0),
            score: self.score,
            evidence: Some(self.evidence.to_string()),
            fix_action: self.fix_action,
            fix_tier: self.fix_tier,
            recheck_result: self.recheck,
            // 逐图判定行的作用域标记（组级 job 会把这两列覆写成 per_group + 组键）
            judgement_scope: Some(VERDICT_SCOPE_ASSET.to_string()),
            group_key: None,
        }
    }
}

/// 全管道：按 pipeline.yaml 声明序执行全部步骤，返回 (待落 verdict 行, 被隔离的规则级失败)。
///
/// C8 **规则级失败隔离**（修 C7 缺口 G4：一条坏规则吞掉整张图的全部判定）：
/// - 装载期 lint 已判定的坏规则（`rule_errors`）→ 不执行，落 `undetermined` + `reason=load_error`；
/// - judge 规则运行时抛**规则级确定性错误**（未知检测器/参数错/输入不符/解码失败/修复插件失败）
///   → 捕获，落 `undetermined` + `reason=rule_error`，同图其余规则（含已算完的）照常落库；
/// - **job 级错误**（VL 通道 HTTP/应答抖动、payload 不一致、do 步骤错误）→ 仍然 `Err` 向上
///   （保留 attempts/重试语义：VL 抖动必须能重试，不能变成静默的人工作业）。
///
/// 被隔离的规则对 `depends_on` 的语义 = **按 fail 处理**（依赖它的步骤 skipped）：判定输入不可信，
/// 不拿它去判下游。
fn run_pipeline(
    ctx: &mut AssetCtx,
    repo: &dyn ComplianceRepo,
    pack: &Pack,
    job_id: i64,
    rule_errors: &std::collections::BTreeMap<String, String>,
) -> Result<(Vec<NewVerdict>, Vec<String>)> {
    let _ = repo; // v1 repo 只在 process_job 用（insert/delete）；步骤内无查询
    let mut dep_results: BTreeMap<String, String> = BTreeMap::new();
    let mut out: Vec<NewVerdict> = Vec::new();
    let mut isolated: Vec<String> = Vec::new();
    let mut do_cur = PathBuf::from(&ctx.snap.abs_path);
    // C4：插件来源（重命名模板 {origName}/{assetId}；单 job 内不变）
    let origin = compliance_plugins::PluginOrigin {
        original_stem: Path::new(&ctx.snap.abs_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string(),
        asset_id: ctx.asset_id,
    };

    for (i, step) in pack.pipeline.steps.iter().enumerate() {
        match step.kind {
            StepKind::Judge => {
                let rule_id = step.rule.as_deref().unwrap_or_default();
                let rule = pack.rule(rule_id).ok_or_else(|| {
                    PipelineError::Compliance(format!(
                        "step#{i}: 规则 {rule_id} 不在 pack {}（装载校验应已拦截）",
                        pack.id
                    ))
                })?;
                // 短路：任一依赖规则 fail → 本步 skipped
                let short: Vec<String> = step
                    .depends_on
                    .iter()
                    .filter(|d| {
                        dep_results
                            .get(*d)
                            .map(|r| r == verdict::FAIL)
                            .unwrap_or(false)
                    })
                    .cloned()
                    .collect();
                let sv = if !short.is_empty() {
                    StepVerdict::new(
                        rule.id.clone(),
                        verdict::SKIPPED,
                        serde_json::json!({ "short_circuited_by": short }),
                    )
                } else if rule.is_group_scoped() {
                    // C9：组级规则（judgement_scope=per_group）**不在资产 job 里执行** ——
                    // 由组级 job（kind=compliance_group）一次/组判定。这里只落一行占位
                    // skipped（不是 fail、不是未判）：资产口径仍保持「每资产每规则一行」，
                    // 权威结论在同行 ruleId 的 per_group 行上（judgementScope 区分）。
                    StepVerdict::new(
                        rule.id.clone(),
                        verdict::SKIPPED,
                        serde_json::json!({
                            "scope": "group", "reason": "group_scoped",
                            "detail": "组级规则由组级 job 一次/组执行；本行为资产口径占位",
                        }),
                    )
                } else if let Some(msg) = rule_errors.get(&rule.id) {
                    // C8：装载期 lint 已判定该规则配置错误 → 不执行（不浪费解码/VL 调用），
                    // 规则级隔离：落 pending_human + load_error 证据，其余规则照落
                    let line = format!("rule {}: {msg}", rule.id);
                    eprintln!(
                        "compliance asset{} 装载期错误（规则隔离，未执行）: {line}",
                        ctx.asset_id
                    );
                    isolated.push(line);
                    dep_results.insert(rule.id.clone(), verdict::FAIL.to_string());
                    StepVerdict::new(
                        rule.id.clone(),
                        verdict::UNDETERMINED,
                        serde_json::json!({ "reason": "load_error", "error": msg }),
                    )
                } else {
                    match judge_rule(ctx, rule, pack.pipeline.recheck_max) {
                        Ok((sv, raw_fail)) => {
                            // 依赖语义看检测器原始结论（fail 即短路）——最终处置（修复/转人工）不影响下游
                            dep_results.insert(
                                rule.id.clone(),
                                if raw_fail {
                                    verdict::FAIL.to_string()
                                } else {
                                    sv.result.clone()
                                },
                            );
                            sv
                        }
                        Err(JudgeFailure::Rule(msg)) => {
                            // C8：规则级确定性失败 → 隔离（同图其余规则照落；job 不失败、不重试）
                            let line = format!("rule {}: {msg}", rule.id);
                            eprintln!(
                                "compliance asset{} 规则级失败（隔离，其余规则照落）: {line}",
                                ctx.asset_id
                            );
                            isolated.push(line);
                            dep_results.insert(rule.id.clone(), verdict::FAIL.to_string());
                            StepVerdict::new(
                                rule.id.clone(),
                                verdict::UNDETERMINED,
                                serde_json::json!({ "reason": "rule_error", "error": msg }),
                            )
                        }
                        // job 级错误（VL 抖动等）：保持原语义，向上抛（attempts/重试）
                        Err(JudgeFailure::Job(e)) => return Err(e),
                    }
                };
                out.push(
                    with_fail_label(sv.with_rule_confidence(Some(rule)), rule).into_new(
                        &pack.id,
                        ctx.asset_id,
                        job_id,
                    ),
                );
            }
            StepKind::Do => {
                let sid = step.id.clone().ok_or_else(|| {
                    PipelineError::Compliance(format!("step#{i}: do 步骤缺 id（装载校验应已拦截）"))
                })?;
                let plugin_id = step.plugin.as_deref().unwrap_or_default();
                let ext = file_ext(&do_cur);
                let dest = ctx
                    .out_dir
                    .join(format!("{}_{}.{}", ctx.asset_id, sid, ext));
                // C4：模型类修复器在 do 链上模型缺失 → skipped（不炸 job，证据可审计）；
                // 其余错误（坏参数/解码失败/未知插件）仍响亮失败
                let io = match compliance_plugins::plugin_run(
                    plugin_id,
                    &do_cur,
                    &step.params,
                    &dest,
                    &origin,
                ) {
                    Ok(io) => io,
                    Err(compliance_plugins::PluginError::ModelMissing { model, .. }) => {
                        out.push(
                            StepVerdict::new(
                                sid.clone(),
                                verdict::SKIPPED,
                                serde_json::json!({
                                    "reason": "model_not_downloaded",
                                    "plugin": plugin_id,
                                    "model": model,
                                }),
                            )
                            .into_new(&pack.id, ctx.asset_id, job_id),
                        );
                        continue; // do 链断点：后续步骤的输入仍是上一产物
                    }
                    Err(e) => return Err(PipelineError::Compliance(format!("do 步骤 {sid}: {e}"))),
                };
                out.push(
                    StepVerdict::new(
                        sid.clone(),
                        verdict::PASS,
                        serde_json::json!({
                            "plugin": plugin_id,
                            "params_yaml": yaml_str(&step.params),
                            "output": plugin_json(&io),
                        }),
                    )
                    .into_new(&pack.id, ctx.asset_id, job_id),
                );
                do_cur = io.path; // do 链：上一步产物 = 下一步输入
            }
        }
    }
    Ok((out, isolated))
}

/// C8：judge 规则失败的**两类**（决定隔离还是重试）。
#[derive(Debug)]
enum JudgeFailure {
    /// 规则级确定性失败（本地检测器/解码/修复插件）→ 只作废该规则判定（其余规则照落）
    Rule(String),
    /// job 级失败（VL 通道 HTTP/应答抖动等）→ 保持 attempts/重试语义，响亮失败
    Job(PipelineError),
}

impl From<PipelineError> for JudgeFailure {
    fn from(e: PipelineError) -> Self {
        JudgeFailure::Job(e)
    }
}

/// judge 单规则：检测 →（fail）fixer → 复检。
/// 返回 (verdict, 检测器原始结论是否 fail)——后者供 depends_on 短路判定。
///
/// C8 错误分类：**vl 通道**错误 = job 级（HTTP/应答抖动 → 重试，保持 C5/C6 门禁口径）；
/// **classic/local_model 通道**错误 = 规则级（未知检测器/参数错/输入不符/解码失败/修复插件失败
/// → 隔离，只作废该规则判定）。
fn judge_rule(
    ctx: &mut AssetCtx,
    rule: &Rule,
    recheck_max: u32,
) -> std::result::Result<(StepVerdict, bool), JudgeFailure> {
    /// 规则级失败（本地通道/解码/插件）→ 隔离
    fn as_rule(e: PipelineError) -> JudgeFailure {
        JudgeFailure::Rule(e.to_string())
    }
    // external_data：v1 不可自动判定 → 人工队列
    if matches!(rule.evidence, core_config::RuleEvidence::ExternalData) {
        return Ok((
            StepVerdict::new(
                rule.id.clone(),
                verdict::UNDETERMINED,
                serde_json::json!({ "reason": "external_data" }),
            ),
            false,
        ));
    }
    // 打标型规则（outputs 非空）：C5 起接 VL 通道（词表约束打标，on_fail=pass 不影响合格结论）；
    // 未配置 VL 或非 vl 执行档 → 显式 skipped（不静默判合格）
    if rule.is_tagging() {
        let sv = match (rule.executor, ctx.vl.as_mut()) {
            (RuleExecutor::Vl, Some(sess)) => {
                let image = ctx.snap.abs_path.clone();
                match sess.round(Path::new(&image)) {
                    Ok(round) => match round.tags.get(&rule.id) {
                        Some(tags) => {
                            let rejected = round
                                .tags_rejected
                                .get(&rule.id)
                                .cloned()
                                .unwrap_or_default();
                            StepVerdict::new(
                                rule.id.clone(),
                                verdict::PASS,
                                serde_json::json!({
                                    "tagging": true,
                                    "outputs": tags,
                                    "rejected": rejected,
                                    "raw": round.raw,
                                }),
                            )
                        }
                        None => StepVerdict::new(
                            rule.id.clone(),
                            verdict::SKIPPED,
                            serde_json::json!({ "reason": "tags_missing" }),
                        ),
                    },
                    Err(e) => {
                        return Err(JudgeFailure::Job(PipelineError::Compliance(format!(
                            "rule {} vl 打标: {e}",
                            rule.id
                        ))))
                    }
                }
            }
            _ => StepVerdict::new(
                rule.id.clone(),
                verdict::SKIPPED,
                serde_json::json!({ "reason": "tagging_not_wired", "executor": executor_name(rule) }),
            ),
        };
        return Ok((sv, false));
    }
    let check = rule.check.as_ref().ok_or_else(|| {
        as_rule(PipelineError::Compliance(format!(
            "rule {} 既无 check 又非打标（装载校验应已拦截）",
            rule.id
        )))
    })?;
    match rule.executor {
        // C5：vl judge 通道。未配置 VlConfig → skipped(executor_not_available)（v1 契约同形，
        // 不静默判合格）；HTTP/解析失败 → Err（job 级重试，attempts 耗尽转 failed，响亮）。
        RuleExecutor::Vl => {
            let image = ctx.snap.abs_path.clone();
            let Some(sess) = ctx.vl.as_mut() else {
                return Ok((
                    StepVerdict::new(
                        rule.id.clone(),
                        verdict::SKIPPED,
                        serde_json::json!({
                            "reason": "executor_not_available",
                            "executor": "vl",
                        }),
                    ),
                    false,
                ));
            };
            let out = sess
                .round(Path::new(&image))
                .map_err(|e| {
                    // COMPLIANCE-CLOSE：输入不可解码 = 确定性规则级失败（落 undetermined + rule_error），
                    // 与通道抖动（job 级重试）分开——坏图重试一万次也还是坏图。
                    match e {
                        compliance_vl::VlError::Decode(m) => as_rule(PipelineError::Compliance(
                            format!("rule {} vl: {m}", rule.id),
                        )),
                        other => JudgeFailure::Job(PipelineError::Compliance(format!(
                            "rule {} vl: {other}",
                            rule.id
                        ))),
                    }
                })?
                .judge
                .get(&rule.id)
                .cloned()
                .ok_or_else(|| {
                    JudgeFailure::Job(PipelineError::Compliance(format!(
                        "rule {}: VL 合并应答缺本规则判定",
                        rule.id
                    )))
                })?;
            // COMPLIANCE-DOMAIN：VL 自评置信度（pack 声明 `vl_self_report`）。
            // 模型没回 confidence → 该行转 `undetermined`（reason=confidence_missing）：
            // 不落假置信度（不默认 1.0、不拿 pack 常数顶替），也不当 fail（不误伤短路）。
            let conf = if is_vl_self_report(rule) {
                match out.confidence {
                    Some(c) => Some(c),
                    None => {
                        let mut sv = StepVerdict::new(
                            rule.id.clone(),
                            verdict::UNDETERMINED,
                            serde_json::json!({
                                "reason": "confidence_missing",
                                "detail": "VL 未回自评 confidence → 该行不给合格/不合格结论（不默认 1.0）",
                                "detector": vl_judge_evidence(check, &out),
                            }),
                        );
                        // 0.0 = 「模型没给把握度」→ 必然落进低置信度复核队列（不是 1.0）
                        sv.confidence = Some(0.0);
                        return Ok((sv, false));
                    }
                }
            } else {
                None
            };
            let pass = vl_pass(check, &out);
            if pass {
                let mut sv = StepVerdict::new(
                    rule.id.clone(),
                    verdict::PASS,
                    vl_judge_evidence(check, &out),
                );
                sv.score = Some(1.0);
                sv.confidence = conf;
                return Ok((sv, false));
            }
            // vl 复检同样走远程通道：错误保持 job 级（抖动可重试）
            let sv =
                handle_fix_vl(ctx, rule, check, &out, recheck_max).map_err(JudgeFailure::Job)?;
            Ok((sv, true))
        }
        // C3：local_model 接入 engine-segment 注册表（u2net）。模型未下载 →
        // skipped(model_not_downloaded)（部署态缺失是降级不是错误，与 v1 契约同形）；
        // feature 未启用（web 容器）→ skipped(executor_not_available)。
        #[cfg(feature = "local-infer")]
        RuleExecutor::LocalModel => {
            let model = engine_segment::registry::required_model(&check.detector, &check.params)
                .map_err(|e| {
                    as_rule(PipelineError::Compliance(format!("rule {}: {e}", rule.id)))
                })?;
            if !engine_segment::model_ready(&model) {
                return Ok((
                    StepVerdict::new(
                        rule.id.clone(),
                        verdict::SKIPPED,
                        serde_json::json!({
                            "reason": "model_not_downloaded",
                            "executor": "local_model",
                            "model": model,
                        }),
                    ),
                    false,
                ));
            }
            let input = ctx
                .detector_input(registry::InputKind::Rgb)
                .map_err(|e| as_rule(PipelineError::Compliance(e)))?;
            let out = run_detector(DetectChannel::Model, &check.detector, &input, &check.params)
                .map_err(|e| {
                    as_rule(PipelineError::Compliance(format!("rule {}: {e}", rule.id)))
                })?;
            if out.pass {
                return Ok((pass_verdict(rule, check, &out), false));
            }
            Ok((
                handle_fix(
                    ctx,
                    rule,
                    check,
                    &out,
                    registry::InputKind::Rgb,
                    DetectChannel::Model,
                    recheck_max,
                )
                .map_err(as_rule)?,
                true,
            ))
        }
        #[cfg(not(feature = "local-infer"))]
        RuleExecutor::LocalModel => Ok((
            StepVerdict::new(
                rule.id.clone(),
                verdict::SKIPPED,
                serde_json::json!({
                    "reason": "executor_not_available",
                    "executor": "local_model",
                }),
            ),
            false,
        )),
        RuleExecutor::Classic => {
            let entry = registry::lookup(&check.detector).ok_or_else(|| {
                as_rule(PipelineError::Compliance(format!(
                    "rule {} 未知检测器: {}（已知: {}）",
                    rule.id,
                    check.detector,
                    registry::known_ids()
                )))
            })?;
            let input = ctx
                .detector_input(entry.input)
                .map_err(|e| as_rule(PipelineError::Compliance(e)))?;
            let out = run_detector(
                DetectChannel::Classic,
                &check.detector,
                &input,
                &check.params,
            )
            .map_err(|e| as_rule(PipelineError::Compliance(format!("rule {}: {e}", rule.id))))?;
            if out.pass {
                return Ok((pass_verdict(rule, check, &out), false));
            }
            Ok((
                handle_fix(
                    ctx,
                    rule,
                    check,
                    &out,
                    entry.input,
                    DetectChannel::Classic,
                    recheck_max,
                )
                .map_err(as_rule)?,
                true,
            ))
        }
    }
}

/// 检测器执行通道：classic = engine-classic 纯函数注册表；model = engine-segment
/// ONNX 注册表（u2net，C3）。两套注册表同为 L1 禁止互引，L2 编排器在此分发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetectChannel {
    Classic,
    Model,
}

/// 统一检测执行入口：两通道输出归一到 DetectorOutput（score/pass/metrics）。
fn run_detector(
    channel: DetectChannel,
    id: &str,
    input: &DetectorInput,
    params: &serde_yaml::Value,
) -> std::result::Result<DetectorOutput, String> {
    match channel {
        DetectChannel::Classic => registry::run(id, input, params).map_err(|e| e.to_string()),
        #[cfg(feature = "local-infer")]
        DetectChannel::Model => {
            let si = as_segment_input(input, id)?;
            engine_segment::registry::run(id, &si, params)
                .map(|o| DetectorOutput {
                    score: o.score,
                    pass: o.pass,
                    metrics: o.metrics,
                })
                .map_err(|e| e.to_string())
        }
        #[cfg(not(feature = "local-infer"))]
        DetectChannel::Model => {
            Err("local-infer 未启用（web 容器），local_model 检测不可用".into())
        }
    }
}

/// 像素档 DetectorInput → engine-segment SegmentInput（元数据档 = 输入不符，响亮报错）。
#[cfg(feature = "local-infer")]
fn as_segment_input(
    input: &DetectorInput,
    detector: &str,
) -> std::result::Result<engine_segment::registry::SegmentInput, String> {
    match input {
        DetectorInput::Rgb {
            width,
            height,
            pixels,
        } => Ok(engine_segment::registry::SegmentInput {
            width: *width,
            height: *height,
            pixels: pixels.clone(),
        }),
        DetectorInput::Metadata { .. } => Err(format!(
            "{detector}: 输入不符: 分割检测器需要像素档（Rgb），收到元数据档"
        )),
    }
}

fn pass_verdict(rule: &Rule, check: &core_config::Check, out: &DetectorOutput) -> StepVerdict {
    let mut sv = StepVerdict::new(
        rule.id.clone(),
        verdict::PASS,
        detector_evidence(check, out),
    );
    sv.score = Some(out.score);
    sv
}

/// fail 处置：fixer 缺失/未注册/复检仍败的完整决策表（证据齐备可审计）。
#[allow(clippy::too_many_arguments)]
fn handle_fix(
    ctx: &mut AssetCtx,
    rule: &Rule,
    check: &core_config::Check,
    out: &DetectorOutput,
    kind: InputKind,
    channel: DetectChannel,
    recheck_max: u32,
) -> std::result::Result<StepVerdict, PipelineError> {
    let detector_out = detector_evidence(check, out);
    let Some(fixer) = &rule.fixer else {
        // 无修复器：on_fail=pass → fail（不影响合格结论，报告聚合时排除）；
        // 其余 → 转人工（无自动路径）
        return Ok(match rule.on_fail {
            RuleOnFail::Pass => {
                let mut sv = StepVerdict::new(
                    rule.id.clone(),
                    verdict::FAIL,
                    serde_json::json!({ "on_fail": "pass", "detector": detector_out }),
                );
                sv.score = Some(out.score);
                sv
            }
            // 没有修复器可修 → 该规则不合格（旧实现落 pending_human = 「不合格 0」根因）
            _ => {
                let mut sv = StepVerdict::new(
                    rule.id.clone(),
                    verdict::FAIL,
                    serde_json::json!({ "reason": "no_fixer", "detector": detector_out }),
                );
                sv.score = Some(out.score);
                sv
            }
        });
    };
    // on_fail=pass 的规则即使带 fixer 也不修（fail 不影响合格结论）
    if rule.on_fail == RuleOnFail::Pass {
        let mut sv = StepVerdict::new(
            rule.id.clone(),
            verdict::FAIL,
            serde_json::json!({ "on_fail": "pass", "detector": detector_out }),
        );
        sv.score = Some(out.score);
        return Ok(sv);
    }
    // 修复插件未注册（C3/C4 落地前）→ 转人工（不阻塞其他规则）
    let Some(_entry) = compliance_plugins::plugin_lookup(&fixer.plugin) else {
        let mut sv = StepVerdict::new(
            rule.id.clone(),
            verdict::UNDETERMINED,
            serde_json::json!({
                "reason": "fixer_not_available",
                "plugin": fixer.plugin,
                "detector": detector_out,
            }),
        );
        sv.score = Some(out.score);
        sv.fix_action = Some(fixer.plugin.clone());
        sv.fix_tier = Some(tier_name(fixer.tier).to_string());
        return Ok(sv);
    };
    // C4：插件来源（重命名模板等需要资产级事实）
    let origin = compliance_plugins::PluginOrigin {
        original_stem: Path::new(&ctx.snap.abs_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string(),
        asset_id: ctx.asset_id,
    };

    // 修复 → 复检 ≤ recheck_max 次（每次修复重写产物；确定性插件首查即终局，上限兜底）
    let src = PathBuf::from(&ctx.snap.abs_path);
    let dest = ctx.out_dir.join(format!(
        "{}_{}_{}.{}",
        ctx.asset_id,
        rule.id,
        fixer.plugin,
        file_ext(&src)
    ));
    let mut last_recheck: Option<DetectorOutput> = None;
    for _ in 0..recheck_max.max(1) {
        // C3：插件参数通道（fixer.params；smart_crop 声明目标占比区间）
        // C4：模型类修复器模型缺失 → 转人工（fixer_not_available 同语义，不炸 job）
        let io = match compliance_plugins::plugin_run(
            &fixer.plugin,
            &src,
            &fixer.params,
            &dest,
            &origin,
        ) {
            Ok(io) => io,
            Err(compliance_plugins::PluginError::ModelMissing { plugin, model }) => {
                let mut sv = StepVerdict::new(
                    rule.id.clone(),
                    verdict::UNDETERMINED,
                    serde_json::json!({
                        "reason": "model_not_downloaded",
                        "plugin": plugin,
                        "model": model,
                        "detector": detector_out,
                    }),
                );
                sv.score = Some(out.score);
                sv.fix_action = Some(fixer.plugin.clone());
                sv.fix_tier = Some(tier_name(fixer.tier).to_string());
                return Ok(sv);
            }
            Err(e) => {
                return Err(PipelineError::Compliance(format!(
                    "rule {} 修复: {e}",
                    rule.id
                )))
            }
        };
        let input = ctx
            .file_input(&io.path, kind)
            .map_err(PipelineError::Compliance)?;
        let re = run_detector(channel, &check.detector, &input, &check.params)
            .map_err(|e| PipelineError::Compliance(format!("rule {} 复检: {e}", rule.id)))?;
        if re.pass {
            let mut sv = StepVerdict::new(
                rule.id.clone(),
                verdict::PASS,
                serde_json::json!({
                    "detector": detector_out,
                    "fix": { "plugin": fixer.plugin, "tier": tier_name(fixer.tier), "output": plugin_json(&io) },
                    "recheck": detector_evidence(check, &re),
                }),
            );
            sv.score = Some(re.score);
            sv.fix_action = Some(fixer.plugin.clone());
            sv.fix_tier = Some(tier_name(fixer.tier).to_string());
            sv.recheck = Some(verdict::PASS.to_string());
            return Ok(sv);
        }
        last_recheck = Some(re);
    }
    let re = last_recheck.expect("循环至少执行一次");
    let reason = match fixer.tier {
        // guaranteed 修完必过被打破 = 异常 → 人工兜底（不静默记合格）
        FixerTier::Guaranteed => "guaranteed_fix_failed",
        FixerTier::BestEffort => "recheck_failed",
    };
    let mut sv = StepVerdict::new(
        rule.id.clone(),
        verdict::FAIL,
        serde_json::json!({
            "reason": reason,
            "detector": detector_out,
            "recheck": detector_evidence(check, &re),
        }),
    );
    sv.score = Some(out.score);
    sv.fix_action = Some(fixer.plugin.clone());
    sv.fix_tier = Some(tier_name(fixer.tier).to_string());
    sv.recheck = Some(verdict::FAIL.to_string());
    Ok(sv)
}

// ---------- C5：VL judge 修复闭环（bbox → inpaint → 重新调用 VL 复检） ----------

/// check.params.expect → violated 期望（"是" = 应存在才算过；缺省 "否"）。
fn vl_expect_yes(check: &core_config::Check) -> bool {
    check
        .params
        .get("expect")
        .and_then(|v| v.as_str())
        .map(|s| s.trim() == "是")
        .unwrap_or(false)
}

/// VL judge 判定证据（含原始应答片段，可审计；判定可复现铁律：证据只来自本次输出）。
/// VL 判定是否合格（**唯一口径**）。
///
/// COMPLIANCE-DOMAIN：词表模式（pack 声明 `check.params.options`，如 `[是, 否]`）用
/// `answer == expect` 判；历史布尔模式仍用 `violated == (expect=="是")`。
/// 为什么不统一成布尔：正向提问下模型把 `violated` 读成「违反/有问题」，
/// 非房源图也答 true（实测），领域 gate 会反号。
fn vl_pass(check: &core_config::Check, out: &VlJudgeOut) -> bool {
    match out.answer.as_deref() {
        Some(a) => {
            let expect = crate::compliance_vl::vl_judge_params(&check.params)
                .map(|p| p.expect)
                .unwrap_or_else(|_| "否".to_string());
            a.trim() == expect.trim()
        }
        None => out.violated == vl_expect_yes(check),
    }
}

fn vl_judge_evidence(check: &core_config::Check, out: &VlJudgeOut) -> serde_json::Value {
    serde_json::json!({
        "detector": "vl_judge",
        "params_yaml": yaml_str(&check.params),
        "violated": out.violated,
        "answer": out.answer,
        "where": out.where_,
        "regions": out.regions,
        "raw": out.raw,
    })
}

/// 多个违规件 → 白=待补洞掩码 PNG（黑底 + 白矩形，同源图尺寸）。
fn write_region_mask(ctx: &AssetCtx, rule_id: &str, regions: &[[f64; 4]]) -> Result<PathBuf> {
    let (w, h, _px) = core_media::rgb8(Path::new(&ctx.snap.abs_path))
        .map_err(|e| PipelineError::Compliance(format!("vl 掩码底图解码: {e}")))?;
    let (w, h) = (i64::from(w), i64::from(h));
    let (wi, hi) = (w as usize, h as usize);
    let mut buf = vec![0u8; wi * hi * 3];
    for r in regions {
        let (x0, y0) = (
            (r[0].clamp(0.0, 1.0) * w as f64).floor() as usize,
            (r[1].clamp(0.0, 1.0) * h as f64).floor() as usize,
        );
        let (x1, y1) = (
            (r[2].clamp(0.0, 1.0) * w as f64).ceil() as usize,
            (r[3].clamp(0.0, 1.0) * h as f64).ceil() as usize,
        );
        for y in y0.min(hi)..y1.min(hi) {
            for x in x0.min(wi)..x1.min(wi) {
                let o = (y * wi + x) * 3;
                buf[o] = 255;
                buf[o + 1] = 255;
                buf[o + 2] = 255;
            }
        }
    }
    let dest = ctx.out_dir.join(format!(
        "{asset_id}_{rule_id}_vlmask.png",
        asset_id = ctx.asset_id
    ));
    core_media::encode_rgb_to(w as i32, h as i32, &buf, &dest, None)
        .map_err(|e| PipelineError::Compliance(format!("vl 掩码编码: {e}")))?;
    Ok(dest)
}

// ---------- DM-C1：inpaint 重绘区域决议（三态 + VL bbox × OCR 行框交叉验证） ----------

/// 区域决议产物：矩形清单（evidence/box 注入）+ 可选整流掩码（u2net 通道的像素级掩码）。
struct ResolvedRegion {
    plan: crate::inpaint_region::RegionPlan,
    /// u2net 通道已写好的掩码 PNG（白 = 重绘区）；其余通道 None（多框时由 [`inject_region`] 合成）
    mask: Option<PathBuf>,
}

/// `fixer.params.region` 的取值（缺省 = 现行 VL bbox 通道）。
///
/// 未知取值**不许静默退回默认通道**：写了 `region: u2netx` 就是作者写错了，静默按 VL bbox
/// 跑会让「配置的人以为在去文字，实际在重绘 VL 指的地方」。
fn region_mode_param(
    params: &serde_yaml::Value,
) -> std::result::Result<Option<crate::inpaint_region::RegionMode>, serde_json::Value> {
    let bad = |v: String| {
        serde_json::json!({
            "reason": "region_mode_invalid",
            "value": v,
            "allowed": ["explicit", "ocr_text", "u2net"],
        })
    };
    match params.get("region") {
        None | Some(serde_yaml::Value::Null) => Ok(None),
        Some(v) => match v.as_str() {
            Some(s) => crate::inpaint_region::RegionMode::parse(s)
                .map(Some)
                .ok_or_else(|| bad(s.to_string())),
            None => Err(bad(format!("{v:?}"))),
        },
    }
}

fn ensure_mapping(params: &mut serde_yaml::Value) {
    if matches!(params, serde_yaml::Value::Null) {
        *params = serde_yaml::Mapping::new().into();
    }
}

/// 把决议出的区域注入 fixer.params（单框 → `box`；多框/u2net → `mask`）。
fn inject_region(
    ctx: &AssetCtx,
    rule: &Rule,
    params: &mut serde_yaml::Value,
    resolved: &ResolvedRegion,
) -> Result<()> {
    ensure_mapping(params);
    if let Some(mask) = &resolved.mask {
        params["mask"] = serde_yaml::Value::String(mask.to_string_lossy().to_string());
        return Ok(());
    }
    match resolved.plan.regions.len() {
        0 => Err(PipelineError::Compliance(format!(
            "rule {}: 区域决议返回空集（不该发生：空集应走拒绝分支）",
            rule.id
        ))),
        1 => {
            params["box"] = serde_yaml::to_value(resolved.plan.regions[0])
                .map_err(|e| PipelineError::Compliance(format!("box 序列化: {e}")))?;
            Ok(())
        }
        _ => {
            let mask = write_region_mask(ctx, &rule.id, &resolved.plan.regions)?;
            params["mask"] = serde_yaml::Value::String(mask.to_string_lossy().to_string());
            Ok(())
        }
    }
}

/// u2net 主体掩码通道（`region: u2net`，也是 `region: ocr_text` 无框时的回落）。
///
/// 面积闸（> 40% 全图）在写产物**之前**判：掩码过大时这张图不该被重绘，也就不该留下产物
/// （否则结果页会出现一张没被采用的 mask PNG，用户无从判断哪张是真正用过的）。
#[cfg(feature = "local-infer")]
fn u2net_region_plan(
    ctx: &mut AssetCtx,
    _rule: &Rule,
) -> std::result::Result<ResolvedRegion, (String, serde_json::Value)> {
    let fail = |reason: &str, detail: String| {
        (
            reason.to_string(),
            serde_json::json!({ "reason": reason, "mode": "u2net", "detail": detail }),
        )
    };
    let input = ctx
        .detector_input(InputKind::Rgb)
        .map_err(|e| fail("region_unavailable", e))?;
    let DetectorInput::Rgb {
        width,
        height,
        pixels,
    } = input
    else {
        return Err(fail("region_unavailable", "u2net 需要像素档输入".into()));
    };
    let seg = engine_segment::segmenter(engine_segment::U2NET)
        .map_err(|e| fail("region_unavailable", e.to_string()))?;
    let s = seg
        .run_rgb(width, height, &pixels)
        .map_err(|e| fail("region_unavailable", e.to_string()))?;
    let ratio = s.mask.fg_ratio();
    let bbox: Vec<[f64; 4]> = s
        .mask
        .dominant_bbox()
        .map(|[x0, y0, x1, y1]| {
            [
                x0 as f64 / f64::from(width.max(1)),
                y0 as f64 / f64::from(height.max(1)),
                x1 as f64 / f64::from(width.max(1)),
                y1 as f64 / f64::from(height.max(1)),
            ]
        })
        .into_iter()
        .collect();
    // 面积闸先判：过大 → 拒绝（不写掩码产物）
    let plan = crate::inpaint_region::plan_u2net(&bbox, ratio)
        .map_err(|r| (r.reason().to_string(), r.evidence()))?;
    let mask_bytes = s.mask.expand_to(width, height);
    let mut rgb = vec![0u8; mask_bytes.len() * 3];
    for (i, m) in mask_bytes.iter().enumerate() {
        if *m != 0 {
            rgb[i * 3] = 255;
            rgb[i * 3 + 1] = 255;
            rgb[i * 3 + 2] = 255;
        }
    }
    let dest = ctx.out_dir.join(format!("{}_u2netmask.png", ctx.asset_id));
    core_media::encode_rgb_to(width as i32, height as i32, &rgb, &dest, None)
        .map_err(|e| fail("region_unavailable", e.to_string()))?;
    Ok(ResolvedRegion {
        plan,
        mask: Some(dest),
    })
}

/// web 容器（无 local-infer）：u2net 不可用 → 明确拒绝（不假装修过）。
#[cfg(not(feature = "local-infer"))]
fn u2net_region_plan(
    _ctx: &mut AssetCtx,
    _rule: &Rule,
) -> std::result::Result<ResolvedRegion, (String, serde_json::Value)> {
    Err((
        "region_unavailable".to_string(),
        serde_json::json!({
            "reason": "region_unavailable",
            "mode": "u2net",
            "detail": "本构建未启用 local-infer（u2net 主体掩码不可用）",
        }),
    ))
}

/// inpaint 重绘区域的唯一决议入口（DM-C1）。
///
/// 返回 `Ok(evidence.region)`（params 已被注入 box/mask）；拒绝时返回 `Err((reason, evidence))`，
/// 编排器据此落 `undetermined` + 理由（不重绘、不假装修过）。
fn resolve_inpaint_region(
    ctx: &mut AssetCtx,
    rule: &Rule,
    params: &mut serde_yaml::Value,
    out: &VlJudgeOut,
) -> std::result::Result<serde_json::Value, (String, serde_json::Value)> {
    // 1) 作者显式声明：box/mask 谁的优先级都高于引擎决议（显式 > 一切）
    let declared_box: Option<[f64; 4]> = params
        .get("box")
        .and_then(|b| serde_yaml::from_value::<[f64; 4]>(b.clone()).ok());
    if params.get("mask").is_some() || params.get("box").is_some() {
        let rects: Vec<[f64; 4]> = declared_box.into_iter().collect();
        return Ok(crate::inpaint_region::plan_explicit(rects).evidence());
    }

    // 2) 通道取值
    let mode = match region_mode_param(params) {
        Ok(m) => m,
        Err(ev) => {
            let reason = ev
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("region_mode_invalid")
                .to_string();
            return Err((reason, ev));
        }
    };
    let vl_regions = out.fix_regions();
    let ocr = ctx.ocr_boxes.clone();

    // 3) 按通道决议
    let resolved = match mode {
        // 显式通道但作者没声明 → 明确转人工（这是「只准用作者给的区域」的语义）
        Some(crate::inpaint_region::RegionMode::Explicit) => {
            return Err((
                "vl_no_region".to_string(),
                serde_json::json!({
                    "reason": "vl_no_region",
                    "mode": "explicit",
                    "vlRegions": vl_regions,
                    "ocrBoxes": ocr,
                }),
            ))
        }
        // OCR 行框通道：无框 → 回落 u2net（卡内裁决：先文字行框，再主体掩码）
        Some(crate::inpaint_region::RegionMode::OcrText) => {
            match crate::inpaint_region::plan_ocr_text(&ocr) {
                Ok(plan) => ResolvedRegion { plan, mask: None },
                Err(_) => u2net_region_plan(ctx, rule)?,
            }
        }
        Some(crate::inpaint_region::RegionMode::U2Net) => u2net_region_plan(ctx, rule)?,
        // 默认通道：VL bbox（须与 OCR 行框交叉验证）
        None => match crate::inpaint_region::plan_vl_bbox(&vl_regions, &ocr) {
            Ok(plan) => ResolvedRegion { plan, mask: None },
            Err(r) => {
                let mut ev = r.evidence();
                if let Some(o) = ev.as_object_mut() {
                    o.insert("vlRegions".into(), serde_json::json!(vl_regions));
                    o.insert("ocrBoxes".into(), serde_json::json!(ocr));
                }
                return Err((r.reason().to_string(), ev));
            }
        },
    };

    let ev = resolved.plan.evidence();
    inject_region(ctx, rule, params, &resolved).map_err(|e| {
        (
            "region_inject_failed".to_string(),
            serde_json::json!({ "reason": "region_inject_failed", "detail": e.to_string() }),
        )
    })?;
    Ok(ev)
}

/// VL judge fail 的修复处置：inpaint 区域来自 VL 返回 bbox（单件 → box；多件 → 合成掩码；
/// 无区域 → 修不了转人工），复检 = 对修复产物重新调用 VL（仅本规则）。
fn handle_fix_vl(
    ctx: &mut AssetCtx,
    rule: &Rule,
    check: &core_config::Check,
    out: &VlJudgeOut,
    recheck_max: u32,
) -> Result<StepVerdict> {
    let detector_out = vl_judge_evidence(check, out);
    let Some(fixer) = &rule.fixer else {
        return Ok(match rule.on_fail {
            RuleOnFail::Pass => {
                let mut sv = StepVerdict::new(
                    rule.id.clone(),
                    verdict::FAIL,
                    serde_json::json!({ "on_fail": "pass", "detector": detector_out }),
                );
                sv.score = Some(0.0);
                sv
            }
            _ => {
                let mut sv = StepVerdict::new(
                    rule.id.clone(),
                    verdict::FAIL,
                    serde_json::json!({ "reason": "no_fixer", "detector": detector_out }),
                );
                sv.score = Some(0.0);
                sv.confidence = self_report_conf(rule, out);
                sv
            }
        });
    };
    if rule.on_fail == RuleOnFail::Pass {
        let mut sv = StepVerdict::new(
            rule.id.clone(),
            verdict::FAIL,
            serde_json::json!({ "on_fail": "pass", "detector": detector_out }),
        );
        sv.score = Some(0.0);
        sv.confidence = self_report_conf(rule, out);
        return Ok(sv);
    }
    let Some(_entry) = compliance_plugins::plugin_lookup(&fixer.plugin) else {
        let mut sv = StepVerdict::new(
            rule.id.clone(),
            verdict::UNDETERMINED,
            serde_json::json!({
                "reason": "fixer_not_available",
                "plugin": fixer.plugin,
                "detector": detector_out,
            }),
        );
        sv.score = Some(0.0);
        sv.fix_action = Some(fixer.plugin.clone());
        sv.fix_tier = Some(tier_name(fixer.tier).to_string());
        return Ok(sv);
    };
    let origin = compliance_plugins::PluginOrigin {
        original_stem: Path::new(&ctx.snap.abs_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string(),
        asset_id: ctx.asset_id,
    };
    // 区域注入（DM-C1）：fixer.params 未声明 mask/box 时按区域通道决议
    //   `explicit`（只认声明） | `ocr_text`（本地 OCR 行框，无框回落 u2net） | `u2net`（主体掩码）
    //   未声明 `region` = 现行默认通道，但 VL bbox 必须先与 OCR 行框交叉验证（见 inpaint_region）。
    let mut params = fixer.params.clone();
    let mut region_ev = serde_json::Value::Null;
    if fixer.plugin == "inpaint" {
        match resolve_inpaint_region(ctx, rule, &mut params, out) {
            Ok(ev) => region_ev = ev,
            Err((reason, mut ev)) => {
                // 区域决议拒绝 = 不拿模型幻觉去重绘（转人工，理由与证据都落库）
                if let Some(o) = ev.as_object_mut() {
                    o.insert("reason".into(), serde_json::json!(reason));
                }
                let mut sv = StepVerdict::new(
                    rule.id.clone(),
                    verdict::UNDETERMINED,
                    serde_json::json!({
                        "detector": detector_out,
                        "region": ev,
                    }),
                );
                sv.score = Some(0.0);
                sv.fix_action = Some(fixer.plugin.clone());
                sv.fix_tier = Some(tier_name(fixer.tier).to_string());
                return Ok(sv);
            }
        }
    }

    let src = PathBuf::from(&ctx.snap.abs_path);
    let dest = ctx.out_dir.join(format!(
        "{}_{}_{}.{}",
        ctx.asset_id,
        rule.id,
        fixer.plugin,
        file_ext(&src)
    ));
    let mut last_recheck: Option<VlJudgeOut> = None;
    for _ in 0..recheck_max.max(1) {
        let io = match compliance_plugins::plugin_run(&fixer.plugin, &src, &params, &dest, &origin)
        {
            Ok(io) => io,
            Err(compliance_plugins::PluginError::ModelMissing { plugin, model }) => {
                let mut sv = StepVerdict::new(
                    rule.id.clone(),
                    verdict::UNDETERMINED,
                    serde_json::json!({
                        "reason": "model_not_downloaded",
                        "plugin": plugin,
                        "model": model,
                        "detector": detector_out,
                        "region": region_ev,
                    }),
                );
                sv.score = Some(0.0);
                sv.fix_action = Some(fixer.plugin.clone());
                sv.fix_tier = Some(tier_name(fixer.tier).to_string());
                return Ok(sv);
            }
            Err(e) => {
                return Err(PipelineError::Compliance(format!(
                    "rule {} 修复: {e}",
                    rule.id
                )))
            }
        };
        let re = ctx
            .vl
            .as_ref()
            .expect("vl judge 路径必有会话")
            .judge_file(&io.path, &rule.id)
            .map_err(|e| PipelineError::Compliance(format!("rule {} vl 复检: {e}", rule.id)))?;
        if vl_pass(check, &re) {
            let mut sv = StepVerdict::new(
                rule.id.clone(),
                verdict::PASS,
                serde_json::json!({
                    "detector": detector_out,
                    "region": region_ev,
                    "fix": { "plugin": fixer.plugin, "tier": tier_name(fixer.tier), "output": plugin_json(&io) },
                    "recheck": vl_judge_evidence(check, &re),
                }),
            );
            sv.score = Some(1.0);
            sv.fix_action = Some(fixer.plugin.clone());
            sv.fix_tier = Some(tier_name(fixer.tier).to_string());
            sv.recheck = Some(verdict::PASS.to_string());
            // 修复后合格：结论来自复检样本 → 置信度取复检那次的自评
            sv.confidence = self_report_conf(rule, &re);
            return Ok(sv);
        }
        last_recheck = Some(re);
    }
    let re = last_recheck.expect("循环至少执行一次");
    let reason = match fixer.tier {
        FixerTier::Guaranteed => "guaranteed_fix_failed",
        FixerTier::BestEffort => "recheck_failed",
    };
    let mut sv = StepVerdict::new(
        rule.id.clone(),
        verdict::FAIL,
        serde_json::json!({
            "reason": reason,
            "detector": detector_out,
            "region": region_ev,
            "recheck": vl_judge_evidence(check, &re),
        }),
    );
    sv.score = Some(0.0);
    sv.fix_action = Some(fixer.plugin.clone());
    sv.fix_tier = Some(tier_name(fixer.tier).to_string());
    sv.recheck = Some(verdict::FAIL.to_string());
    sv.confidence = self_report_conf(rule, &re);
    Ok(sv)
}

// ---------- 小工具 ----------

fn detector_evidence(check: &core_config::Check, out: &DetectorOutput) -> serde_json::Value {
    serde_json::json!({
        "detector": check.detector,
        "params_yaml": yaml_str(&check.params),
        "score": out.score,
        "pass": out.pass,
        "metrics": out.metrics,
    })
}

fn plugin_json(io: &PluginOutput) -> serde_json::Value {
    serde_json::json!({ "path": io.path.to_string_lossy(), "width": io.width, "height": io.height })
}

fn yaml_str(v: &serde_yaml::Value) -> String {
    serde_yaml::to_string(v).unwrap_or_default()
}

fn executor_name(rule: &Rule) -> &'static str {
    match rule.executor {
        RuleExecutor::Classic => "classic",
        RuleExecutor::LocalModel => "local_model",
        RuleExecutor::Vl => "vl",
    }
}

fn tier_name(t: FixerTier) -> &'static str {
    match t {
        FixerTier::Guaranteed => "guaranteed",
        FixerTier::BestEffort => "best_effort",
    }
}

fn file_ext(p: &Path) -> String {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| "bin".to_string())
}

/// 供报告导出复用的判定行过滤入口（svc-library 经 core-storage repo 自查；此处仅冻结语义）。
pub type ComplianceVerdictRow = VerdictRow;

// ---------- 测试（卡验收：短路 / 断点续跑 / do-only / 降级语义 / 500 张） ----------

#[cfg(test)]
mod tests {
    use super::*;
    use core_config::Pack;

    /// N5b（0028）：判定结论在 `verdict` 列（可空），`result` 只是「跑成了没有」。
    /// 断言读结论一律走这个投影 —— 直接读裸 `result` 会把 `ok` 当成「通过」。
    const V: &str = "COALESCE(verdict, CASE result WHEN 'skipped' THEN 'skipped' \
                     ELSE 'undetermined' END)";

    const TAXONOMY: &str = "category: [连衣裙]\n";

    /// classic-only 迷你包（验收主场景）：R1 尺寸(元数据档) / R2 白底(像素档) /
    /// R4 依赖 R1 短路(像素档) / R5 软规则(on_fail=pass 原始 fail 行) / do resize。
    const MINI_RULES: &str = r#"
- id: R1_size
  name: 尺寸达标
  executor: classic
  check: {detector: size_min, params: {min: 700}}
  fixer: null
  on_fail: fail
- id: R2_white
  name: 白底纯净
  executor: classic
  check: {detector: white_edge, params: {max_dev: 8}}
  fixer: null
  on_fail: fail
- id: R4_dep
  name: 短路依赖规则
  executor: classic
  check: {detector: sharpness, params: {min: 1}}
  fixer: null
  on_fail: fail
- id: R5_soft
  name: 软规则（fail 不影响合格结论）
  executor: classic
  check: {detector: white_edge, params: {max_dev: 8}}
  fixer: null
  on_fail: pass
"#;
    const MINI_PIPELINE: &str = r#"
steps:
  - {kind: judge, rule: R1_size}
  - {kind: judge, rule: R2_white}
  - {kind: judge, rule: R4_dep, depends_on: [R1_size]}
  - {kind: judge, rule: R5_soft}
  - {kind: do, id: resize_64, plugin: resize, params: {max_side: 64}}
recheck_max: 1
"#;
    const MINI_REPORT: &str =
        "summary: [合格, 不合格, 未判定]\ndetail_columns: [imageSN, R1_size, R2_white, R4_dep, R5_soft, resize_64, R1_size_score]\n";

    fn mini_pack() -> Pack {
        Pack::from_yaml_strs("mini", TAXONOMY, MINI_RULES, MINI_PIPELINE, MINI_REPORT).unwrap()
    }

    /// 白底合格图：白底 + 中心深色块（白边纯净、对比强）。
    fn good_jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            let border = x < 8 || y < 8 || x >= w - 8 || y < 8 || y >= h - 8;
            if border || x < 8 || x >= w - 8 || y < 8 || y >= h - 8 {
                image::Rgb([250, 250, 250])
            } else {
                image::Rgb([40, 40, 44])
            }
        });
        encode_jpeg(&img)
    }

    /// 红边不合格图：边缘带大偏差（white_edge fail）。
    fn bad_jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            if x < 8 || y < 8 || x >= w - 8 || y >= h - 8 {
                image::Rgb([200, 10, 10])
            } else {
                image::Rgb([90, 90, 90])
            }
        });
        encode_jpeg(&img)
    }

    /// C9：高频条纹图（pHash 与「边框+色块」类图相距很远）——给组级重复判定当「无关图」用。
    fn stripes_jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, _| {
            let stripe = (x / 4) % 2 == 0;
            image::Rgb(if stripe {
                [250, 250, 250]
            } else {
                [20, 20, 20]
            })
        });
        encode_jpeg(&img)
    }

    fn encode_jpeg(img: &image::RgbImage) -> Vec<u8> {
        let mut buf = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut buf)
            .encode_image(img)
            .unwrap();
        buf
    }

    struct Fixture {
        dir: PathBuf,
        conn: Connection,
        good: Vec<i64>,
        bad: Vec<i64>,
    }

    impl Fixture {
        fn new(tag: &str, n: usize) -> Self {
            let dir = std::env::temp_dir().join(format!("damc2-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("src")).unwrap();
            let conn = Connection::open_in_memory().unwrap();
            core_storage::migrations::migrate(&conn).unwrap();
            let mut f = Fixture {
                dir,
                conn,
                good: Vec::new(),
                bad: Vec::new(),
            };
            for i in 0..n {
                let bad = i % 3 == 2; // 1/3 不合格（红边小图）
                let id = f.add_asset(i, bad);
                if bad {
                    f.bad.push(id);
                } else {
                    f.good.push(id);
                }
            }
            f
        }

        /// 加一张资产：type A(合格大图)/B(合格大图)…i%3==2 → 红边 800x600（R1 fail + R2 fail）。
        fn add_asset(&mut self, i: usize, bad: bool) -> i64 {
            let name = format!("img{i}.jpg");
            let path = self.dir.join("src").join(&name);
            let bytes = if bad {
                bad_jpeg(800, 600)
            } else {
                good_jpeg(1000, 800)
            };
            std::fs::write(&path, bytes).unwrap();
            self.conn
                .execute(
                    "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, importedAt)
                     VALUES (NULL, ?1, 'image', ?2, 'jpg', 1, 0, 0)",
                    rusqlite::params![path.to_string_lossy(), name],
                )
                .unwrap();
            let id = self.conn.last_insert_rowid();
            let (w, h) = if bad { (800, 600) } else { (1000, 800) };
            self.conn
                .execute(
                    "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1, 'primary', 0, ?2, ?3)",
                    rusqlite::params![id, w, h],
                )
                .unwrap();
            id
        }

        fn asset_ids(&self) -> Vec<i64> {
            let mut stmt = self
                .conn
                .prepare("SELECT assetId FROM assets ORDER BY assetId")
                .unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, i64>(0)).unwrap();
            rows.map(|r| r.unwrap()).collect()
        }

        fn out_dir(&self) -> PathBuf {
            self.dir.join("out")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn run_all(conn: &Connection, pack: &Pack, _out_dir: &Path) -> ComplianceRunReport {
        let cancel = AtomicBool::new(false);
        run_compliance_queue(conn, pack, &cancel, &mut |_| {}).expect("run")
    }

    /// 按「判定语义」计数：`where_sql` 里的 `result` 指的是**判定词**（pass/fail/undetermined/skipped），
    /// 不是库里的执行结果列 —— 0028 之后两者分列，这里把裸列投影成同义的 `result` 再套谓词，
    /// 免得每个调用点各写一遍 COALESCE（写错一处就是「库里对、断言过」的假绿）。
    fn verdict_count(conn: &Connection, where_sql: &str) -> i64 {
        let sql = format!(
            "SELECT COUNT(*) FROM (SELECT *, {V} AS verdict_word FROM rule_verdict) \
             WHERE {}",
            replace_result_token(where_sql, "verdict_word")
        );
        conn.query_row(&sql, [], |r| r.get(0)).unwrap()
    }

    /// 把谓词里的裸列 `result` 换成投影列名（不动 `recheckResult` 这类含同样子串的列）。
    fn replace_result_token(cond: &str, to: &str) -> String {
        const TOKEN: &str = "result";
        let mut out = String::with_capacity(cond.len());
        let mut rest = cond;
        while let Some(pos) = rest.find(TOKEN) {
            let before_ok = rest[..pos]
                .chars()
                .last()
                .map(|c| !c.is_alphanumeric() && c != '_')
                .unwrap_or(true);
            let after = &rest[pos + TOKEN.len()..];
            let after_ok = after
                .chars()
                .next()
                .map(|c| !c.is_alphanumeric() && c != '_')
                .unwrap_or(true);
            out.push_str(&rest[..pos]);
            out.push_str(if before_ok && after_ok { to } else { TOKEN });
            rest = after;
        }
        out.push_str(rest);
        out
    }

    fn verdict_of(conn: &Connection, asset: i64, rule: &str) -> (String, Option<String>) {
        conn.query_row(
            &format!("SELECT {V}, evidence FROM rule_verdict WHERE assetId = ?1 AND ruleId = ?2"),
            rusqlite::params![asset, rule],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    /// 验收主场景（卡验收原文）：classic-only 迷你包 500 张全跑通——
    /// 报告行数(=verdict 行数)、短路生效、逐规则判定正确、do 步骤产物落 outDir。
    #[test]
    fn classic_mini_pack_500_end_to_end() {
        let pack = mini_pack();
        let fx = Fixture::new("e2e", 500);
        let ids = fx.asset_ids();
        assert_eq!(ids.len(), 500);
        let n = compliance_enq(
            &fx.conn,
            &ids,
            &pack.id,
            Some(&fx.out_dir().to_string_lossy()),
        )
        .unwrap();
        assert_eq!(n, 500, "500 全部入队");

        let report = run_all(&fx.conn, &pack, &fx.out_dir());
        assert_eq!(report.done, 500, "全部完成");
        assert_eq!(
            (
                report.failed,
                report.retry,
                report.skipped,
                report.errors.len()
            ),
            (0, 0, 0, 0)
        );
        let (d, f, p) = compliance_stats(&fx.conn).unwrap();
        assert_eq!((d, f, p), (500, 0, 0));

        // 每资产 5 行 verdict（4 judge + 1 do），共 2500 行
        assert_eq!(verdict_count(&fx.conn, "packId='mini'"), 2500);

        let good = fx.good[0]; // 白底 1000×800
        let bad = fx.bad[0]; // 红边 800×600
                             // 合格资产：全 pass
        for rule in ["R1_size", "R2_white", "R4_dep", "R5_soft", "resize_64"] {
            let (r, _) = verdict_of(&fx.conn, good, rule);
            assert_eq!(r, verdict::PASS, "{rule}");
        }
        // 不合格资产：R1/R2 fail（无修复器可修 = 不合格；旧实现落 pending_human 是「不合格 0」根因）、
        // R4 短路 skipped、
        // R5 软规则原始 fail（on_fail=pass）、do 步骤仍执行
        let (r1, e1) = verdict_of(&fx.conn, bad, "R1_size");
        assert_eq!(r1, verdict::FAIL);
        assert!(e1.unwrap().contains("no_fixer"));
        let (r2, _) = verdict_of(&fx.conn, bad, "R2_white");
        assert_eq!(r2, verdict::FAIL);
        let (r4, e4) = verdict_of(&fx.conn, bad, "R4_dep");
        assert_eq!(r4, verdict::SKIPPED);
        assert!(e4.unwrap().contains("R1_size"), "证据记 short_circuited_by");
        let (r5, e5) = verdict_of(&fx.conn, bad, "R5_soft");
        assert_eq!(r5, verdict::FAIL);
        assert!(
            e5.as_deref().unwrap().contains("\"on_fail\":\"pass\""),
            "{e5:?}"
        );
        let (rd, _) = verdict_of(&fx.conn, bad, "resize_64");
        assert_eq!(rd, verdict::PASS);

        // do 产物：每资产一个，长边 ≤ 64，jpg 格式
        let mut files = 0;
        for entry in std::fs::read_dir(fx.out_dir()).unwrap() {
            let p = entry.unwrap().path();
            assert_eq!(p.extension().and_then(|e| e.to_str()), Some("jpg"));
            let (w, h) = core_media::image_size(&p).unwrap();
            assert!(w.max(h) <= 64, "{p:?} {w}x{h}");
            files += 1;
        }
        assert_eq!(files, 500);

        // 重判语义：done 批次允许重新入队（新批次），报告口径取每 (asset, rule) 最新一行
        assert_eq!(
            compliance_enq(&fx.conn, &ids, &pack.id, None).unwrap(),
            500,
            "done 允许重判（新批次）"
        );
        let again = run_all(&fx.conn, &pack, &fx.out_dir());
        assert_eq!(again.done, 500);
        assert_eq!(
            verdict_count(&fx.conn, "packId='mini'"),
            5000,
            "两批次 × 2500 行"
        );
        let (d, f, p) = compliance_stats(&fx.conn).unwrap();
        assert_eq!((d, f, p), (1000, 0, 0));
    }

    /// 断电重跑续接（卡验收原文）：kill 后 running 残留 + 半截 verdict 行 →
    /// 重跑续接、无重复行、已完成资产不重做。
    #[test]
    fn kill_resume_no_duplicate_verdicts() {
        let pack = mini_pack();
        let fx = Fixture::new("resume", 5);
        let ids = fx.asset_ids();
        compliance_enq(
            &fx.conn,
            &ids,
            &pack.id,
            Some(&fx.out_dir().to_string_lossy()),
        )
        .unwrap();

        // 模拟崩溃：claim 2 个置 running 后进程死掉（半截 verdict 行也留下）
        let claimed = jobq::claim_batch(&fx.conn, COMPLIANCE_KIND, 2).unwrap();
        assert_eq!(claimed.len(), 2);
        let (crash_job, crash_asset, _) = claimed[0];
        core_storage::compliance_sqlite::SqliteComplianceRepo::new(&fx.conn)
            .insert_verdict(&NewVerdict {
                pack_id: "mini".into(),
                asset_id: crash_asset,
                job_id: crash_job,
                rule_id: "R1_size".into(),
                result: "pass".into(),
                confidence: 0.9,
                score: None,
                evidence: None,
                fix_action: None,
                fix_tier: None,
                recheck_result: None,
                judgement_scope: Some("per_asset".into()),
                group_key: None,
            })
            .unwrap();

        // 重跑：requeue_stale 续接全部 5 个
        let report = run_all(&fx.conn, &pack, &fx.out_dir());
        assert_eq!(report.done, 5, "崩溃残留也被续跑");
        assert_eq!(
            verdict_count(&fx.conn, "packId='mini'"),
            25,
            "5 资产 × 5 行，无重复无残留"
        );
        // 半截行被幂等重做覆盖（该资产 5 行整、结果正确）
        let (r1, _) = verdict_of(&fx.conn, crash_asset, "R1_size");
        assert_eq!(r1, verdict::PASS);

        // 已 done 的资产重跑队列 = 空转
        let again = run_all(&fx.conn, &pack, &fx.out_dir());
        assert_eq!(again.total, 0);
        assert_eq!(verdict_count(&fx.conn, "packId='mini'"), 25);
    }

    /// do-only 管道（D12 简单批处理形态验收点）：零规则，resize→导出产物同样跑通。
    #[test]
    fn do_only_pipeline_resizes() {
        let pack = Pack::from_yaml_strs(
            "do-only",
            "",
            "[]",
            r#"
steps:
  - {kind: do, id: resize_48, plugin: resize, params: {max_side: 48}}
recheck_max: 1
"#,
            "summary: [合格, 不合格, 未判定]\ndetail_columns: [imageSN, resize_48]\n",
        )
        .unwrap();
        let fx = Fixture::new("doonly", 3);
        let ids = fx.asset_ids();
        compliance_enq(
            &fx.conn,
            &ids,
            &pack.id,
            Some(&fx.out_dir().to_string_lossy()),
        )
        .unwrap();
        let report = run_all(&fx.conn, &pack, &fx.out_dir());
        assert_eq!(report.done, 3);
        assert_eq!(verdict_count(&fx.conn, "packId='do-only'"), 3);
        for id in &ids {
            let (r, e) = verdict_of(&fx.conn, *id, "resize_48");
            assert_eq!(r, verdict::PASS);
            let ev: serde_json::Value = serde_json::from_str(&e.unwrap()).unwrap();
            assert_eq!(ev["plugin"], "resize");
        }
        let out_files = std::fs::read_dir(fx.out_dir()).unwrap().count();
        assert_eq!(out_files, 3);
    }

    /// 降级语义：local_model（模型未下载）→ skipped(model_not_downloaded)（C3；
    /// feature 未启用时 executor_not_available）；vl → skipped(executor_not_available)；
    /// external_data → pending_human；打标型 → skipped(tagging_not_wired)；
    /// 带修复器但插件未注册 → pending_human(fixer_not_available)。
    #[test]
    fn degradations_match_v1_contract() {
        let pack = Pack::from_yaml_strs(
            "degrade",
            "",
            r#"
- id: R_lm
  name: 本地模型规则
  executor: local_model
  check: {detector: fg_ratio, params: {model: u2net, min: 0.65, max: 0.85}, confidence: {source: fixed, value: 0.9}}
  fixer: null
  on_fail: fail
- id: R_vl
  name: 远程语义规则
  executor: vl
  check: {detector: vl_judge, params: {prompt: "x"}, confidence: {source: fixed, value: 0.9}}
  fixer: null
  on_fail: fail
- id: R_ext
  name: 外部数据规则
  executor: classic
  evidence: external_data
  check: {detector: size_min, params: {min: 700}}
  fixer: null
  on_fail: fail
- id: R_tag
  name: 打标型
  executor: vl
  outputs: [category]
  on_fail: pass
- id: R_fix
  name: 带未注册修复器
  executor: classic
  check: {detector: white_edge, params: {max_dev: 8}}
  fixer: {type: replace_bg, tier: guaranteed}
  on_fail: auto_fix
"#,
            r#"
steps:
  - {kind: judge, rule: R_lm}
  - {kind: judge, rule: R_vl}
  - {kind: judge, rule: R_ext}
  - {kind: judge, rule: R_tag}
  - {kind: judge, rule: R_fix}
recheck_max: 1
"#,
            "summary: [合格, 不合格, 未判定]\ndetail_columns: [imageSN]\n",
        )
        .unwrap();
        let fx = Fixture::new("degrade", 3);
        // C3：隔离分割模型根（指向空目录 → u2net 未下载路径）
        // 进程级全局态：ModelsRootScope 取 core_config::models_root_lock() + drop 还原
        let empty_root = fx.out_dir().join("empty-models-root");
        std::fs::create_dir_all(&empty_root).unwrap();
        let _seg_scope = engine_segment::ModelsRootScope::set(&empty_root);
        let ids = vec![fx.bad[0]]; // 红边图（R_fix 的 white_edge fail 触发修复路径）
        compliance_enq(&fx.conn, &ids, &pack.id, None).unwrap();
        let report = run_all(&fx.conn, &pack, &fx.out_dir());
        assert_eq!(report.done, 1);
        let a = fx.bad[0];
        let (r, e) = verdict_of(&fx.conn, a, "R_lm");
        assert_eq!(r, verdict::SKIPPED);
        let ev_lm = e.unwrap();
        #[cfg(feature = "local-infer")]
        assert!(
            ev_lm.contains("model_not_downloaded") && ev_lm.contains("u2net"),
            "{ev_lm}"
        );
        #[cfg(not(feature = "local-infer"))]
        assert!(ev_lm.contains("executor_not_available"), "{ev_lm}");
        let (r, _e) = verdict_of(&fx.conn, a, "R_vl");
        assert_eq!(r, verdict::SKIPPED);
        let (r, e) = verdict_of(&fx.conn, a, "R_ext");
        assert_eq!(
            r,
            verdict::UNDETERMINED,
            "external_data 需外部数据 → 未判定"
        );
        assert!(e.unwrap().contains("external_data"));
        let (r, e) = verdict_of(&fx.conn, a, "R_tag");
        assert_eq!(r, verdict::SKIPPED);
        assert!(e.unwrap().contains("tagging_not_wired"));
        let (r, e) = verdict_of(&fx.conn, a, "R_fix");
        assert_eq!(r, verdict::UNDETERMINED);
        // C4：replace_bg 已注册但模型未下载 → model_not_downloaded（执行态缺失 = 未判定）
        let ev = e.unwrap();
        #[cfg(feature = "local-infer")]
        assert!(
            ev.contains("model_not_downloaded")
                && ev.contains("replace_bg")
                && ev.contains("u2net"),
            "{ev}"
        );
        #[cfg(not(feature = "local-infer"))]
        assert!(
            ev.contains("fixer_not_available") && ev.contains("replace_bg"),
            "{ev}"
        );
    }

    /// 未知 do 插件 = 配置错误 → job 响亮失败（重试耗尽转 failed，不静默跳过）。
    #[test]
    fn unknown_do_plugin_fails_job_after_retries() {
        let pack = Pack::from_yaml_strs(
            "badplug",
            "",
            "[]",
            "steps:\n  - {kind: do, id: watermark, plugin: vaporize}\n",
            "summary: [合格, 不合格, 未判定]\ndetail_columns: [imageSN]\n",
        )
        .unwrap();
        let fx = Fixture::new("badplug", 1);
        let ids = fx.asset_ids();
        compliance_enq(&fx.conn, &ids, &pack.id, None).unwrap();
        for i in 0..jobq::MAX_ATTEMPTS {
            let report = run_all(&fx.conn, &pack, &fx.out_dir());
            if i + 1 < jobq::MAX_ATTEMPTS {
                assert_eq!(report.retry, 1, "第 {} 次应回 pending", i + 1);
            } else {
                assert_eq!(report.failed, 1, "耗尽转 failed");
                assert!(report.errors[0].contains("未知插件"), "{:?}", report.errors);
            }
        }
        let (d, f, p) = compliance_stats(&fx.conn).unwrap();
        assert_eq!((d, f, p), (0, 1, 0));
        // 重试入口：failed → pending 清零重来
        assert_eq!(compliance_retry_failed(&fx.conn).unwrap(), 1);
        let (_, _, p) = compliance_stats(&fx.conn).unwrap();
        assert_eq!(p, 1);
    }

    // ---------- C3：local_model（u2net）+ smart_crop 真模型端到端 ----------
    // 模型根是进程级全局态 → 一律走 engine_segment::ModelsRootScope /
    // engine_restore::ModelsRootScope（取 core_config::models_root_lock() + drop 还原）。
    // 本模块原先自立的 SEG_ROOT_LOCK 已删除：同二进制里的写根用例不止本文件
    // （compliance_plugins.rs 的 replace_bg 用例就漏了锁），各自一把锁挡不住。

    /// 电商 R4 形态：local_model u2net fg_ratio + smart_crop（guaranteed auto_fix）。
    const SEG_RULES: &str = r#"
- id: R4_subject_ratio
  name: 主体占比 65%~85%
  executor: local_model
  check: {detector: fg_ratio, params: {model: u2net, min: 0.65, max: 0.85}, confidence: {source: fixed, value: 0.9}}
  fixer: {type: smart_crop, tier: guaranteed, params: {min: 0.65, max: 0.85}}
  on_fail: auto_fix
"#;
    const SEG_PIPELINE: &str = r#"
steps:
  - {kind: judge, rule: R4_subject_ratio}
recheck_max: 1
"#;

    /// 白底小主体图（fg_ratio ≈ 9% → fail → smart_crop 放大 → 复检应回到目标区间）。
    fn small_subject_jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            let cx = (w / 2 - 60..w / 2 + 60).contains(&x);
            let cy = (h / 2 - 60..h / 2 + 60).contains(&y);
            if cx && cy {
                image::Rgb([30, 30, 34])
            } else {
                image::Rgb([252, 252, 250])
            }
        });
        encode_jpeg(&img)
    }

    /// 主体锚在左上角附近的图（越界钳制场景）：裁剪窗以 bbox 为中心会越出图像 →
    /// 钳到原点（窗口不丢主体、不越界）。主体不贴图像边缘（贴边主体的 u2net
    /// 显著性衰减是已知模型特性，会让复检失真——见 C3 报告「已知边界」）。
    fn border_subject_jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            let cx = (15..135).contains(&x);
            let cy = (15..135).contains(&y);
            if cx && cy {
                image::Rgb([30, 30, 34])
            } else {
                image::Rgb([252, 252, 250])
            }
        });
        encode_jpeg(&img)
    }

    /// 真模型 e2e（需 DAM_TEST_MODELS_U2NET=<u2net 模型目录，含 u2net.onnx>；未设置则跳过）。
    /// 断言：检测 fail → auto_fix 裁剪 → 复检回到目标区间（guaranteed 语义成立）；
    /// 主体贴边界（窗口钳进图像）同样修完必过；修复产物写 outDir 不入库。
    #[test]
    fn local_model_u2net_fg_ratio_detect_and_smart_crop_fix() {
        let Ok(model_dir) = std::env::var("DAM_TEST_MODELS_U2NET") else {
            eprintln!("skip: DAM_TEST_MODELS_U2NET 未设置（真 u2net 模型目录）");
            return;
        };
        let onnx = Path::new(&model_dir).join("u2net.onnx");
        if !onnx.is_file() {
            eprintln!("skip: {onnx:?} 不存在");
            return;
        }
        // 模型根 = 临时目录 + `models/u2net` 符号链接指向真模型目录
        // （布局契约 <root>/models/u2net/u2net.onnx；env 语义沿用 DAM_TEST_MODELS=模型目录）
        let link_root = std::env::temp_dir().join(format!("dam-seg-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&link_root);
        std::fs::create_dir_all(link_root.join("models")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            Path::new(&model_dir).canonicalize().unwrap(),
            link_root.join("models").join("u2net"),
        )
        .or_else(|_| {
            std::fs::hard_link(
                Path::new(&model_dir).canonicalize().unwrap(),
                link_root.join("models").join("u2net"),
            )
        })
        .unwrap();
        let root = link_root.clone();
        // 真模型：ModelsRootScope 取进程级锁 + drop 还原（用例体内不会被其他用例改根）
        let _seg_scope = engine_segment::ModelsRootScope::set(&root);

        let pack = Pack::from_yaml_strs(
            "seg-e2e",
            TAXONOMY,
            SEG_RULES,
            SEG_PIPELINE,
            "summary: [合格, 不合格, 未判定]\ndetail_columns: [imageSN, R4_subject_ratio, R4_subject_ratio_score]\n",
        )
        .unwrap();
        let fx = Fixture::new("seg-e2e", 1);
        // 两张自定义图（小主体 + 贴边主体），并更新宽高快照
        let p1 = fx.dir.join("src").join("small.jpg");
        std::fs::write(&p1, small_subject_jpeg(400, 400)).unwrap();
        let p2 = fx.dir.join("src").join("border.jpg");
        std::fs::write(&p2, border_subject_jpeg(400, 400)).unwrap();
        for p in [&p1, &p2] {
            fx.conn
                .execute(
                    "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, importedAt)
                     VALUES (NULL, ?1, 'image', ?2, 'jpg', 1, 0, 0)",
                    rusqlite::params![p.to_string_lossy(), p.file_name().unwrap().to_string_lossy()],
                )
                .unwrap();
            let id = fx.conn.last_insert_rowid();
            fx.conn
                .execute(
                    "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1, 'primary', 0, 400, 400)",
                    rusqlite::params![id],
                )
                .unwrap();
        }
        let ids = fx.asset_ids();
        assert_eq!(ids.len(), 3);
        compliance_enq(
            &fx.conn,
            &ids,
            &pack.id,
            Some(&fx.out_dir().to_string_lossy()),
        )
        .unwrap();
        let report = run_all(&fx.conn, &pack, &fx.out_dir());
        assert_eq!(report.done, 3, "errors={:?}", report.errors);

        // 小主体：fail → 裁剪 → 复检 pass（guaranteed）
        for (id, tag) in [(ids[0], "fixture"), (ids[1], "small"), (ids[2], "border")] {
            let (r, e) = verdict_of(&fx.conn, id, "R4_subject_ratio");
            if tag == "fixture" {
                // Fixture 自带图（红边大主体/白底大主体）不保证在区间内，只要求有判定
                assert!(!r.is_empty(), "fixture 无判定行");
                continue;
            }
            let ev: serde_json::Value = serde_json::from_str(&e.expect("evidence")).unwrap();
            assert_eq!(r, verdict::PASS, "{tag}: {ev}");
            assert_eq!(ev["fix"]["plugin"], "smart_crop", "{tag}: {ev}");
            assert_eq!(ev["fix"]["tier"], "guaranteed");
            assert_eq!(ev["recheck"]["pass"], true, "{tag}: {ev}");
            let ratio = ev["recheck"]["score"].as_f64().unwrap();
            assert!(
                (0.65..=0.85).contains(&ratio),
                "{tag}: 复检占比 {ratio} 出区间"
            );
        }
        // 修复产物在 outDir（不入库）
        let outs = std::fs::read_dir(fx.out_dir()).unwrap().count();
        assert!(outs >= 2, "out_dir 仅 {outs} 个产物");
        let _ = std::fs::remove_dir_all(&link_root);
    }

    // ---------- C8：规则级失败隔离（修 C7 缺口 G4：一条坏规则吞掉整张图全部判定） ----------

    /// 装载期 lint 已判定配置错误的规则（未注册检测器）→ 该规则**不执行**、
    /// 落 pending_human + reason=load_error；同图其余规则照常判定落库；job 不失败。
    #[test]
    fn load_error_rule_is_isolated_and_other_rules_still_land() {
        let rules = r#"
- id: K1_size
  name: 尺寸
  executor: classic
  check: {detector: size_min, params: {min: 700}}
  fixer: null
  on_fail: fail
- id: K2_phantom
  name: 未注册检测器（C7 F3 形态）
  executor: classic
  check: {detector: phash_dup, params: {group: within_batch}, confidence: {source: fixed, value: 0.9}}
  fixer: null
  on_fail: pass
"#;
        let pipeline = "steps:\n  - {kind: judge, rule: K1_size}\n  - {kind: judge, rule: K2_phantom}\nrecheck_max: 1\n";
        let pack = Pack::from_yaml_strs("iso-load", "", rules, pipeline, "").unwrap();
        // 装载期 lint 认出坏规则（K2）
        let errs = crate::compliance_lint::rule_errors(&pack);
        assert_eq!(
            errs.keys().collect::<Vec<_>>(),
            vec!["K2_phantom"],
            "{errs:?}"
        );

        let fx = Fixture::new("c8-load", 6);
        let ids = fx.asset_ids();
        compliance_enq(
            &fx.conn,
            &ids,
            &pack.id,
            Some(&fx.out_dir().to_string_lossy()),
        )
        .unwrap();
        let report = run_all(&fx.conn, &pack, &fx.out_dir());
        assert_eq!(
            report.failed, 0,
            "坏规则不许让 job 失败: {:?}",
            report.errors
        );
        assert_eq!(report.done, 6);
        assert_eq!(report.ruleErrors.len(), 6, "{:?}", report.ruleErrors);
        // 每张图两行判定：K1 真判定 + K2 隔离行（undetermined + load_error）
        assert_eq!(verdict_count(&fx.conn, "1=1"), 12);
        for id in fx.asset_ids() {
            let (r1, e1) = verdict_of(&fx.conn, id, "K1_size");
            let v1: serde_json::Value = serde_json::from_str(e1.as_deref().unwrap()).unwrap();
            assert_ne!(v1["reason"], "load_error", "K1 应有真判定: {r1} {e1:?}");
            assert!(
                e1.as_deref().unwrap().contains("size_min"),
                "K1 应有检测器证据: {e1:?}"
            );
            let (r2, e2) = verdict_of(&fx.conn, id, "K2_phantom");
            assert_eq!(r2, "undetermined", "装载期坏规则 = 判定过程出错");
            let ev: serde_json::Value = serde_json::from_str(e2.as_deref().unwrap()).unwrap();
            assert_eq!(ev["reason"], "load_error");
            assert!(
                ev["error"].as_str().unwrap().contains("phash_dup"),
                "错误要点名检测器: {ev}"
            );
        }
    }

    /// 运行时**规则级**确定性失败（本地检测器解码失败：文件不存在）→ 只作废该规则判定，
    /// 同图另一个规则（元数据档，不需要解码）照落；job 不失败、不重试。
    #[test]
    fn runtime_rule_error_is_isolated() {
        let rules = r#"
- id: K1_size
  executor: classic
  check: {detector: size_min, params: {min: 700}}
  fixer: null
  on_fail: fail
- id: K2_sharp
  executor: classic
  check: {detector: sharpness, params: {min: 100}}
  fixer: null
  on_fail: fail
"#;
        let pipeline = "steps:\n  - {kind: judge, rule: K1_size}\n  - {kind: judge, rule: K2_sharp}\nrecheck_max: 1\n";
        let pack = Pack::from_yaml_strs("iso-run", "", rules, pipeline, "").unwrap();
        // 配置合法（装载期 lint 干净）→ 失败只可能发生在运行时
        assert!(crate::compliance_lint::rule_errors(&pack).is_empty());

        let fx = Fixture::new("c8-run", 4);
        // 手工造一张「DB 有、磁盘没」的资产：元数据档可判、像素档解码失败
        let ghost = fx.conn.last_insert_rowid() + 1000;
        fx.conn
            .execute(
                "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, importedAt)
                 VALUES (?1, ?2, 'image', 'ghost.jpg', 'jpg', 1, 0, 0)",
                rusqlite::params![ghost, fx.dir.join("src/ghost.jpg").to_string_lossy()],
            )
            .unwrap();
        fx.conn
            .execute(
                "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1, 'primary', 0, 1000, 800)",
                rusqlite::params![ghost],
            )
            .unwrap();

        let mut ids = fx.asset_ids();
        ids.push(ghost);
        compliance_enq(
            &fx.conn,
            &ids,
            &pack.id,
            Some(&fx.out_dir().to_string_lossy()),
        )
        .unwrap();
        let report = run_all(&fx.conn, &pack, &fx.out_dir());
        assert_eq!(
            report.failed, 0,
            "运行时规则级失败不许炸 job: {:?}",
            report.errors
        );
        assert_eq!(report.done, 5);
        assert_eq!(report.retry, 0, "规则级失败不是抖动，不重试");
        assert_eq!(report.ruleErrors.len(), 1, "{:?}", report.ruleErrors);
        assert!(
            report.ruleErrors[0].contains("K2_sharp"),
            "{:?}",
            report.ruleErrors
        );
        // 幽灵图：元数据档规则照判；像素档规则隔离
        let (r1, e1) = verdict_of(&fx.conn, ghost, "K1_size");
        assert!(r1 == "pass" || r1 == "fail", "{r1} {e1:?}");
        let (r2, e2) = verdict_of(&fx.conn, ghost, "K2_sharp");
        assert_eq!(r2, "undetermined");
        let ev: serde_json::Value = serde_json::from_str(e2.as_deref().unwrap()).unwrap();
        assert_eq!(ev["reason"], "rule_error");
        // 隔离不外溢：只有幽灵图那一条规则落 rule_error，其余全是真判定
        assert_eq!(
            verdict_count(
                &fx.conn,
                "result = 'undetermined' AND evidence LIKE '%rule_error%'"
            ),
            1
        );
        assert_eq!(verdict_count(&fx.conn, "1=1"), 10);
    }

    /// 被隔离的规则对 depends_on 按 fail 处理：依赖它的步骤 skipped（不拿不可信输入判下游）。
    #[test]
    fn isolated_rule_short_circuits_dependents() {
        let rules = r#"
- id: K1_phantom
  executor: classic
  check: {detector: phash_dup, params: {}, confidence: {source: fixed, value: 0.9}}
  fixer: null
  on_fail: pass
- id: K2_after
  executor: classic
  check: {detector: size_min, params: {min: 700}}
  fixer: null
  on_fail: fail
"#;
        let pipeline = "steps:\n  - {kind: judge, rule: K1_phantom}\n  - {kind: judge, rule: K2_after, depends_on: [K1_phantom]}\nrecheck_max: 1\n";
        let pack = Pack::from_yaml_strs("iso-dep", "", rules, pipeline, "").unwrap();
        let fx = Fixture::new("c8-dep", 3);
        let ids = fx.asset_ids();
        compliance_enq(
            &fx.conn,
            &ids,
            &pack.id,
            Some(&fx.out_dir().to_string_lossy()),
        )
        .unwrap();
        let report = run_all(&fx.conn, &pack, &fx.out_dir());
        assert_eq!(report.failed, 0);
        assert_eq!(verdict_count(&fx.conn, "1=1"), 6);
        let (r1, _) = verdict_of(&fx.conn, fx.asset_ids()[0], "K1_phantom");
        assert_eq!(r1, "undetermined", "装载期坏规则 → 未判定");
        let (r2, e2) = verdict_of(&fx.conn, fx.asset_ids()[0], "K2_after");
        assert_eq!(r2, "skipped");
        assert!(e2.unwrap().contains("K1_phantom"));
    }

    /// C9：`judgement_scope: per_group` 的规则在**资产 job** 里只落占位 skipped(group_scoped) ——
    /// 不执行（不浪费解码/phash）、不写成 fail/undetermined、不把「未知检测器」噪声写进资产口径；
    /// 同图 per_asset 规则照常真判定。
    /// COMPLIANCE-DOMAIN：组级规则的**跨作用域短路** —— 上游逐图规则 fail 的成员不进组级判定。
    ///
    /// 场景：`src/` 两张 800×600（size_min 1200 fail）、`src2/` 一张 1000×800（pass）。
    /// 断言：src 两个成员的组级规则落 skipped + short_circuited_by；src2 的组级规则照常判定；
    /// 组级证据里的 `members` 只剩真正参与的成员（`shortCircuitedMembers` 单列）。
    #[test]
    fn group_rule_is_short_circuited_by_upstream_per_asset_gate() {
        let rules = r#"
- id: G0_gate
  executor: classic
  check: {detector: size_min, params: {min: 700}}
  fixer: null
  on_fail: fail
- id: G1_dup
  executor: classic
  judgement_scope: per_group
  group_by: {source: folder}
  check: {detector: phash_dup, params: {threshold: 8}, confidence: {source: fixed, value: 0.9}}
  fixer: null
  on_fail: pass
"#;
        let pipeline = "steps:\n  - {kind: judge, rule: G0_gate}\n  \
                        - {kind: judge, rule: G1_dup, depends_on: [G0_gate]}\n";
        let pack = Pack::from_yaml_strs("gate-short", "", rules, pipeline, "").unwrap();
        // src/：两张 800×600（gate fail）
        let mut fx = Fixture::new("gate-short", 0);
        fx.add_asset(1, true);
        fx.add_asset(2, true);
        // src2/：1000×800（gate pass）
        let dir2 = fx.dir.join("src2");
        std::fs::create_dir_all(&dir2).unwrap();
        let p2 = dir2.join("ok.jpg");
        std::fs::write(&p2, stripes_jpeg(1000, 800)).unwrap();
        fx.conn
            .execute(
                "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, importedAt)
                 VALUES (NULL, ?1, 'image', 'ok.jpg', 'jpg', 1, 0, 0)",
                rusqlite::params![p2.to_string_lossy()],
            )
            .unwrap();
        let ok_id = fx.conn.last_insert_rowid();
        fx.conn
            .execute(
                "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1, 'primary', 0, 1000, 800)",
                rusqlite::params![ok_id],
            )
            .unwrap();
        let ids = fx.asset_ids();
        assert_eq!(ids.len(), 3);
        let out_s = fx.out_dir().to_string_lossy().to_string();
        assert_eq!(
            compliance_enq(&fx.conn, &ids, &pack.id, Some(&out_s)).unwrap(),
            3,
            "逐图 job 三个"
        );
        let n = compliance_enq_group(&fx.conn, &pack, &ids, Some(&out_s)).unwrap();
        assert_eq!(n, 2, "两个目录 = 两个组级 job");
        let report = run_all(&fx.conn, &pack, &fx.out_dir());
        assert_eq!(report.failed, 0, "{:?}", report.errors);
        assert_eq!(report.groupFailed, 0, "{:?}", report.groupErrors);

        // src/ 两个成员：逐图 gate fail
        let g0: Vec<String> = {
            let mut stmt = fx
                .conn
                .prepare(&format!(
                    "SELECT {V} FROM rule_verdict WHERE ruleId = 'G0_gate' ORDER BY assetId"
                ))
                .unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        assert_eq!(g0, vec!["fail", "fail", "pass"], "{g0:?}");

        // 组级口径：src 两个成员 skipped + short_circuited_by；src2 一个成员真判定
        struct Row {
            gk: String,
            result: String,
            ev: serde_json::Value,
        }
        let rows: Vec<Row> = {
            let mut stmt = fx
                .conn
                .prepare(&format!(
                    "SELECT groupKey, {V}, evidence FROM rule_verdict
                     WHERE ruleId = 'G1_dup' AND groupKey IS NOT NULL ORDER BY groupKey, assetId"
                ))
                .unwrap();
            let it = stmt
                .query_map([], |r| {
                    Ok(Row {
                        gk: r.get::<_, String>(0)?,
                        result: r.get::<_, String>(1)?,
                        ev: serde_json::from_str(&r.get::<_, String>(2)?)
                            .unwrap_or(serde_json::Value::Null),
                    })
                })
                .unwrap();
            it.map(|r| r.unwrap()).collect()
        };
        assert_eq!(rows.len(), 3, "组级行按成员展开：src 2 + src2 1");
        let mut src_gated = 0;
        let mut src2_judged = 0;
        for row in &rows {
            let reason = row.ev.get("reason").and_then(|x| x.as_str());
            if row.gk.ends_with("src") {
                assert_eq!(
                    row.result, "skipped",
                    "src 成员必须被短路: {} {}",
                    row.gk, row.result
                );
                assert_eq!(reason, Some("short_circuited_by"), "{}", row.gk);
                assert_eq!(
                    row.ev.get("shortCircuitedBy"),
                    Some(&serde_json::json!(["G0_gate"])),
                    "{}",
                    row.gk
                );
                // 证据里 members 只剩参与判定的成员（空）——被短路成员单列
                assert_eq!(
                    row.ev.get("members"),
                    Some(&serde_json::json!([])),
                    "{}",
                    row.gk
                );
                assert_eq!(
                    row.ev.get("shortCircuitedMembers"),
                    Some(&serde_json::json!(ids[0..2])),
                    "{}",
                    row.gk
                );
                src_gated += 1;
            } else {
                assert!(row.gk.ends_with("src2"), "{}", row.gk);
                assert_eq!(row.result, "pass", "src2 成员照常判定（单成员无重复）");
                src2_judged += 1;
            }
        }
        assert_eq!(src_gated, 2, "src 两个成员各一行 skipped");
        assert_eq!(src2_judged, 1, "src2 一个成员真判定");
    }

    #[test]
    fn group_scoped_rule_lands_placeholder_skip_in_asset_job() {
        let rules = r#"
- id: G1_dup
  name: 同组重复
  executor: classic
  check: {detector: phash_dup, params: {threshold: 8}, confidence: {source: fixed, value: 0.9}}
  fixer: null
  on_fail: pass
  judgement_scope: per_group
  group_by: {source: folder}
- id: R1_size
  executor: classic
  check: {detector: size_min, params: {min: 700}}
  fixer: null
  on_fail: fail
"#;
        let pipeline = "steps:\n  - {kind: judge, rule: G1_dup}\n  - {kind: judge, rule: R1_size}\nrecheck_max: 1\n";
        let pack = Pack::from_yaml_strs("c9-asset", "", rules, pipeline, "").unwrap();
        let fx = Fixture::new("c9-asset", 3);
        let ids = fx.asset_ids();
        compliance_enq(
            &fx.conn,
            &ids,
            &pack.id,
            Some(&fx.out_dir().to_string_lossy()),
        )
        .unwrap();
        let report = run_all(&fx.conn, &pack, &fx.out_dir());
        assert_eq!(report.failed, 0, "{:?}", report.errors);
        // 每组级规则每资产一行占位：skipped + reason=group_scoped + scope=group
        assert_eq!(
            verdict_count(
                &fx.conn,
                "ruleId = 'G1_dup' AND judgementScope = 'per_asset' AND groupKey IS NULL \
                 AND result = 'skipped' AND evidence LIKE '%group_scoped%'"
            ),
            3
        );
        // 资产口径不得出现组级规则的非 skipped 行（无 load_error/pending_human 噪声）
        assert_eq!(
            verdict_count(
                &fx.conn,
                "ruleId = 'G1_dup' AND judgementScope = 'per_asset' AND result != 'skipped'"
            ),
            0
        );
        assert_eq!(
            verdict_count(
                &fx.conn,
                "ruleId = 'G1_dup' AND judgementScope = 'per_group'"
            ),
            0,
            "只有资产 job 跑过：组级行必须为 0（组级结论由组级 job 落）"
        );
        // per_asset 规则逐图真判定、不受影响（Fixture: i%3==2 → 800×600，短边 600 < 700 →
        // 原始 fail；无修复器 + on_fail=fail → 落 fail（三态口径））
        assert_eq!(
            verdict_count(
                &fx.conn,
                "ruleId = 'R1_size' AND judgementScope = 'per_asset' AND result = 'fail'"
            ),
            1
        );
        assert_eq!(
            verdict_count(
                &fx.conn,
                "ruleId = 'R1_size' AND judgementScope = 'per_asset' AND result = 'pass'"
            ),
            2
        );
        assert_eq!(verdict_count(&fx.conn, "1=1"), 6);
    }

    /// C9：组级 job（kind=compliance_group）一 job 一组，按成员展开落库 —— 组内每个成员一行、
    /// 内容逐字相同（组键 + 成员列表 + 重复组证据），且组内重复检测基于真 pHash/SHA。
    #[test]
    fn group_job_expands_verdict_per_member_and_flags_dups() {
        let rules = r#"
- id: G1_dup
  name: 同组重复
  executor: classic
  check: {detector: phash_dup, params: {threshold: 8}, confidence: {source: fixed, value: 0.9}}
  fixer: null
  on_fail: pass
  judgement_scope: per_group
  group_by: {source: folder}
"#;
        let pipeline = "steps:\n  - {kind: judge, rule: G1_dup}\nrecheck_max: 1\n";
        let pack = Pack::from_yaml_strs("c9-group", "", rules, pipeline, "").unwrap();
        // Fixture：src/ 下 2 张字节相同的 good 图（同目录一组 → 精确重复）
        let fx = Fixture::new("c9-group", 2);
        // 另建 src2/：单成员组（无重复 → pass）——证明「组键 = 目录」真的分组、不是一锅端
        let dir2 = fx.dir.join("src2");
        std::fs::create_dir_all(&dir2).unwrap();
        let p2 = dir2.join("solo.jpg");
        std::fs::write(&p2, stripes_jpeg(1000, 800)).unwrap();
        fx.conn
            .execute(
                "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, importedAt)
                 VALUES (NULL, ?1, 'image', 'solo.jpg', 'jpg', 1, 0, 0)",
                rusqlite::params![p2.to_string_lossy()],
            )
            .unwrap();
        let solo = fx.conn.last_insert_rowid();
        fx.conn
            .execute(
                "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1, 'primary', 0, 1000, 800)",
                rusqlite::params![solo],
            )
            .unwrap();
        let ids = fx.asset_ids();
        assert_eq!(ids.len(), 3);
        let n_group =
            compliance_enq_group(&fx.conn, &pack, &ids, Some(&fx.out_dir().to_string_lossy()))
                .unwrap();
        assert_eq!(n_group, 2, "一个目录一个 job（同作用域规则共享）");
        let report = run_all(&fx.conn, &pack, &fx.out_dir());
        assert_eq!(report.failed, 0, "{:?}", report.errors);
        assert_eq!(report.groupDone, 2);
        assert_eq!(report.groupFailed, 0);
        // 资产口径：组级 job 不改资产行（此 pack 无 per_asset 规则 → 一条资产行都没有）
        assert_eq!(verdict_count(&fx.conn, "judgementScope = 'per_asset'"), 0);
        // 组级口径：3 个成员各一行（按成员展开），组键非空且两个组
        assert_eq!(
            verdict_count(
                &fx.conn,
                "judgementScope = 'per_group' AND groupKey IS NOT NULL"
            ),
            3
        );
        let keys: Vec<String> = {
            let mut stmt = fx
                .conn
                .prepare("SELECT DISTINCT groupKey FROM rule_verdict WHERE judgementScope = 'per_group' ORDER BY groupKey")
                .unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        assert_eq!(keys.len(), 2, "{keys:?}");
        assert!(
            keys[0].ends_with("src") && keys[1].ends_with("src2"),
            "{keys:?}"
        );
        let group_key = keys[0].clone();
        let evidence_of = |k: &str| -> serde_json::Value {
            let ev: String = fx
                .conn
                .query_row(
                    "SELECT evidence FROM rule_verdict WHERE judgementScope = 'per_group' AND groupKey = ?1 LIMIT 1",
                    [k],
                    |r| r.get(0),
                )
                .unwrap();
            serde_json::from_str(&ev).unwrap()
        };
        // 组 1（src）：2 张字节相同 → 精确重复组，成员 = 两张 assetId
        let ev = evidence_of(&group_key);
        assert_eq!(ev["scope"], "group");
        assert_eq!(ev["groupKey"], serde_json::json!(group_key));
        assert_eq!(ev["memberCount"], 2);
        assert_eq!(ev["members"], serde_json::json!([ids[0], ids[1]]));
        let dups = ev["dupGroups"].as_array().unwrap();
        assert_eq!(dups.len(), 1, "{ev}");
        assert_eq!(dups[0]["kind"], "exact");
        assert_eq!(dups[0]["members"], serde_json::json!([ids[0], ids[1]]));
        // 组 2（src2）：单成员 → 无重复、pass，但判定照落（组级判定不是「必须有重复才落」）
        let ev2 = evidence_of(&keys[1]);
        assert_eq!(ev2["memberCount"], 1);
        assert_eq!(ev2["dupGroups"].as_array().unwrap().len(), 0);
        assert_eq!(
            fx.conn
                .query_row(
                    &format!("SELECT {V} FROM rule_verdict WHERE groupKey = ?1 LIMIT 1"),
                    [keys[1].as_str()],
                    |r| r.get::<_, String>(0),
                )
                .unwrap(),
            "pass"
        );
        // 每条成员行的证据逐字相同（同一组同一结论）：两个组 → 恰好 2 份不同证据
        assert_eq!(
            fx.conn
                .query_row(
                    "SELECT COUNT(DISTINCT evidence) FROM rule_verdict WHERE judgementScope = 'per_group'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
        // job_queue：组级 job 全部 done，资产 job 一条没有
        assert_eq!(fx.conn.query_row(
            "SELECT COUNT(*) FROM job_queue WHERE kind = 'compliance_group' AND status = 'done'",
            [], |r| r.get::<_, i64>(0)).unwrap(), 2);
        assert_eq!(
            fx.conn
                .query_row(
                    "SELECT COUNT(*) FROM job_queue WHERE kind = 'compliance'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }
    // -----------------------------------------------------------------
    // IP-K05：插件执行通道（判定行映射 + 队列落库 + 来源列）
    // -----------------------------------------------------------------

    const PLUGIN_ID: &str = "com.soujpg.pack.ecommerce";
    const PLUGIN_HASH: &str = "9f2c1b7d4e5a6f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708";

    fn plugin_identity(rule_ids: &[&str]) -> PackIdentityInfo {
        PackIdentityInfo {
            id: PLUGIN_ID.to_string(),
            version: "1.0.0".to_string(),
            plugin_hash: PLUGIN_HASH.to_string(),
            deterministic: false,
            rule_ids: rule_ids.iter().map(|s| s.to_string()).collect(),
            kinds: vec!["judge".to_string()],
            actions: Vec::new(),
            timeout_secs: 60,
        }
    }

    fn pv(rule: &str, status: &str, score: Option<f64>) -> PluginVerdict {
        PluginVerdict {
            rule_id: rule.to_string(),
            status: status.to_string(),
            score,
            label: Some(format!("{rule}_label")),
            evidence: serde_json::json!({ "detail": rule }),
            fixed: false,
            note: None,
        }
    }

    fn outcome(identity: PackIdentityInfo, verdicts: Vec<PluginVerdict>) -> PluginOutcome {
        PluginOutcome {
            pack: identity,
            verdicts,
            artifacts: Vec::new(),
            rows: Vec::new(),
            duration_ms: 7,
            failure: None,
            raw: serde_json::json!({}),
        }
    }

    fn failed_outcome(
        identity: PackIdentityInfo,
        kind: PluginFailure,
        detail: &str,
    ) -> PluginOutcome {
        PluginOutcome {
            pack: identity,
            verdicts: Vec::new(),
            artifacts: Vec::new(),
            rows: Vec::new(),
            duration_ms: 3,
            failure: Some((kind, detail.to_string())),
            raw: serde_json::json!({ "error": detail }),
        }
    }

    fn evidence_of(sv: &StepVerdict) -> &serde_json::Value {
        &sv.evidence
    }

    /// 成功路径：声明序输出、状态原样、非确定性包置信度 = score（没给 = 0.0 不伪装把握度）。
    #[test]
    fn plugin_success_keeps_declaration_order_and_status() {
        let id = plugin_identity(&["R0_gate", "R1_size", "R2_white"]);
        let judged = outcome(
            id,
            vec![
                pv("R2_white", verdict::PASS, Some(0.91)),
                pv("R0_gate", verdict::FAIL, None),
                pv("R1_size", verdict::SKIPPED, Some(0.5)),
            ],
        );
        let rows = plugin_rows(&judged, None);
        let order: Vec<&str> = rows.iter().map(|r| r.rule_id.as_str()).collect();
        assert_eq!(
            order,
            vec!["R0_gate", "R1_size", "R2_white"],
            "落库/报告列序 = 包声明序"
        );
        assert_eq!(rows[0].result, verdict::FAIL);
        assert_eq!(rows[1].result, verdict::SKIPPED);
        assert_eq!(rows[2].result, verdict::PASS);
        // 非确定性包：有 score 用 score，没 score 用 0.0（绝不伪装 1.0）
        assert_eq!(rows[0].confidence, Some(0.0));
        assert_eq!(rows[2].confidence, Some(0.91));
        // skipped = 「确定没有结论」= 1.0（与 legacy 口径一致）
        assert_eq!(rows[1].confidence, Some(1.0));
        // score 原样作证据列
        assert_eq!(rows[2].score, Some(0.91));
        // evidence 带引擎/包身份/原始细节
        let ev = evidence_of(&rows[2]);
        assert_eq!(ev["engine"], "plugin");
        assert!(ev["pack"].as_str().unwrap().starts_with(PLUGIN_ID));
        assert_eq!(ev["detail"], "R2_white", "插件原始 evidence 必须保留");
    }

    /// 确定性包：所有有结论的行 confidence = 1.0（可复现，不需要复核）。
    #[test]
    fn plugin_deterministic_pack_confidence_is_one() {
        let mut id = plugin_identity(&["R1_size"]);
        id.deterministic = true;
        let judged = outcome(id, vec![pv("R1_size", verdict::PASS, Some(0.2))]);
        let rows = plugin_rows(&judged, None);
        assert_eq!(rows[0].confidence, Some(1.0));
    }

    /// timeout / crash / 包拉不起来 → 包内**每条规则**一行 undetermined + 对应 reason。
    #[test]
    fn plugin_failures_never_impersonate_verdicts() {
        for (kind, reason) in [
            (PluginFailure::Timeout, "plugin_timeout"),
            (PluginFailure::Crash, "plugin_crash"),
            (PluginFailure::Unavailable, "plugin_unavailable"),
            (PluginFailure::RuleError, "plugin_rule_error"),
            (PluginFailure::Protocol, "plugin_protocol_error"),
        ] {
            let id = plugin_identity(&["R0_gate", "R1_size"]);
            let judged = failed_outcome(id, kind, "boom");
            let rows = plugin_rows(&judged, None);
            assert_eq!(rows.len(), 2, "{reason}: 每条规则都要有行（不许少）");
            for r in &rows {
                assert_eq!(
                    r.result,
                    verdict::UNDETERMINED,
                    "{reason}: 失败不得冒充 pass/fail"
                );
                assert_eq!(evidence_of(r)["reason"], reason);
                assert_eq!(r.confidence, Some(0.0), "没有结论 → 必须进复核");
            }
        }
    }

    /// 协议违例：包声明了规则却没回结论 → undetermined + protocol reason（不静默丢行）。
    #[test]
    fn plugin_missing_verdict_lands_undetermined() {
        let judged = outcome(
            plugin_identity(&["R0_gate", "R1_size"]),
            vec![pv("R0_gate", verdict::PASS, Some(0.9))],
        );
        let rows = plugin_rows(&judged, None);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].rule_id, "R1_size");
        assert_eq!(rows[1].result, verdict::UNDETERMINED);
        assert_eq!(evidence_of(&rows[1])["reason"], "plugin_protocol_error");
    }

    /// 状态越界（闭集外，例如旧词 failed）→ 不可采信，落 undetermined + protocol reason。
    #[test]
    fn plugin_invalid_status_is_not_trusted() {
        let judged = outcome(
            plugin_identity(&["R2_white"]),
            vec![pv("R2_white", "failed", Some(0.4))],
        );
        let rows = plugin_rows(&judged, None);
        assert_eq!(rows[0].result, verdict::UNDETERMINED);
        assert_eq!(rows[0].confidence, Some(0.0));
        assert_eq!(evidence_of(&rows[0])["reason"], "plugin_protocol_error");
    }

    /// repair 的复判结论覆盖 judge（同 ruleId），并留 fixAction/recheck/artifacts 证据。
    #[test]
    fn plugin_repair_overrides_judge_and_records_artifact() {
        let id = plugin_identity(&["R1_size", "R4_subject_ratio"]);
        let judged = outcome(
            id.clone(),
            vec![
                pv("R1_size", verdict::FAIL, Some(0.8)),
                pv("R4_subject_ratio", verdict::PASS, Some(0.7)),
            ],
        );
        let mut repaired = outcome(
            id,
            vec![
                {
                    let mut v = pv("R1_size", verdict::PASS, Some(0.9));
                    v.fixed = true; // 这条真修了 → fixAction 才落
                    v
                },
                pv("R4_subject_ratio", verdict::PASS, Some(0.7)),
            ],
        );
        repaired.artifacts = vec![serde_json::json!({
            "rule_id": "R1_size", "path": "/tmp/out/a1.png",
            "width": 2000, "height": 2000, "kind": "image/png"
        })];
        let rows = plugin_rows(&judged, Some(&repaired));
        assert_eq!(rows[0].result, verdict::PASS, "修复后复判才是最终结论");
        assert_eq!(rows[0].fix_action.as_deref(), Some("plugin_repair"));
        assert_eq!(rows[0].recheck.as_deref(), Some(verdict::PASS));
        let arts = evidence_of(&rows[0])["artifacts"].as_array().unwrap();
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0]["path"], "/tmp/out/a1.png");
        // 没被修的规则：结论照旧，且不冒充「修过」（fixed=false → 无 fixAction）
        assert_eq!(rows[1].result, verdict::PASS);
        assert!(rows[1].fix_action.is_none());
        assert_eq!(rows[1].recheck.as_deref(), Some(verdict::PASS));
        assert!(evidence_of(&rows[1]).get("artifacts").is_none());
    }

    /// repair 失败不推翻判定：judge 的 fail 保留，repair 错误进 evidence（界面能解释「为什么没修」）。
    #[test]
    fn plugin_repair_failure_keeps_judgement() {
        let id = plugin_identity(&["R1_size"]);
        let judged = outcome(id.clone(), vec![pv("R1_size", verdict::FAIL, Some(0.85))]);
        let repaired = failed_outcome(id, PluginFailure::Crash, "repair 子进程崩了");
        let rows = plugin_rows(&judged, Some(&repaired));
        assert_eq!(rows[0].result, verdict::FAIL, "修复失败≠判定作废");
        assert_eq!(rows[0].confidence, Some(0.85));
        assert_eq!(
            evidence_of(&rows[0])["repair_error"]["reason"],
            "plugin_crash"
        );
    }

    // K07-FIX（2026-10-21）：repair 的 skipped/undetermined 不得覆盖 judge 结论
    // （实测缺陷：R0_product_main 等 3 行 fail 被 repair 的 skipped 抹掉，DB 主行/界面显示成了无结论）。

    /// judge=fail + repair=skipped（不可修规则）→ **落库仍是 fail**，repair 结论进 evidence。
    #[test]
    fn plugin_repair_skipped_keeps_judge_fail_in_db() {
        let f = Fixture::new("plugin-repair-skip", 1);
        let pack = mini_pack();
        let cancel = AtomicBool::new(false);
        compliance_enq(&f.conn, &[f.good[0]], &pack.id, None).unwrap();
        let plugin = MockPlugin::new(&["R0_gate", "R1_size"], |_req| {
            outcome(
                plugin_identity(&["R0_gate", "R1_size"]),
                vec![
                    pv("R0_gate", verdict::FAIL, Some(0.8)),
                    pv("R1_size", verdict::PASS, Some(0.9)),
                ],
            )
        })
        .with_repair(|_req| {
            let mut v = pv("R0_gate", verdict::SKIPPED, None);
            v.fixed = false;
            v.note = Some("not_fixable: fix_policy=judge_only".into());
            outcome(plugin_identity(&["R0_gate"]), vec![v])
        });
        let report = run_compliance_queue_with_limit_in_run_engine(
            &f.conn,
            &pack,
            &cancel,
            &mut |_| {},
            0,
            None,
            Some(&plugin),
        )
        .expect("run");
        assert_eq!(report.done, 1);

        let (result, recheck, fix_action): (String, Option<String>, Option<String>) = f
            .conn
            .query_row(
                &format!(
                    "SELECT {V}, recheckResult, fixAction FROM rule_verdict
                     WHERE packId = ?1 AND ruleId = 'R0_gate'"
                ),
                [PLUGIN_ID],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            result,
            verdict::FAIL,
            "repair 回 skipped ≠ 判定被推翻：DB 主行必须还是 fail"
        );
        assert_eq!(recheck, None, "没有复判结论就不许写 recheck");
        assert!(fix_action.is_none(), "没修成不许标修过");

        let ev: String = f
            .conn
            .query_row(
                "SELECT evidence FROM rule_verdict WHERE packId = ?1 AND ruleId = 'R0_gate'",
                [PLUGIN_ID],
                |r| r.get(0),
            )
            .unwrap();
        let ev: serde_json::Value = serde_json::from_str(&ev).unwrap();
        assert_eq!(ev["repair_status"], verdict::SKIPPED, "repair 原结论进证据");
        assert_eq!(ev["repair_suppressed"], true);
        assert_eq!(ev["repair_note"], "not_fixable: fix_policy=judge_only");
        // 无关规则不受影响
        let pass: String = f
            .conn
            .query_row(
                &format!("SELECT {V} FROM rule_verdict WHERE packId = ?1 AND ruleId = 'R1_size'"),
                [PLUGIN_ID],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pass, verdict::PASS);
    }

    /// judge=fail + repair=undetermined（复判判不了）→ 仍是 fail，置信度/score 跟判定走。
    #[test]
    fn plugin_repair_undetermined_keeps_judge_fail() {
        let id = plugin_identity(&["R0_gate"]);
        let judged = outcome(id.clone(), vec![pv("R0_gate", verdict::FAIL, Some(0.8))]);
        let repaired = outcome(id, vec![pv("R0_gate", verdict::UNDETERMINED, None)]);
        let rows = plugin_rows(&judged, Some(&repaired));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].result, verdict::FAIL, "判不了 ≠ 推翻 fail");
        assert_eq!(
            rows[0].confidence,
            Some(0.8),
            "置信度跟判定走，不被 repair 抹平"
        );
        assert_eq!(rows[0].score, Some(0.8));
        assert_eq!(rows[0].recheck, None);
        assert!(rows[0].fix_action.is_none());
        let ev = evidence_of(&rows[0]);
        assert_eq!(ev["repair_status"], verdict::UNDETERMINED);
        assert_eq!(ev["repair_suppressed"], true);
        assert_eq!(ev["detail"], "R0_gate", "judge 原始证据必须保留");
    }

    /// judge=pass + repair 无关/回 skipped → 行不变（不因 repair 而转动结论、不写 recheck）。
    #[test]
    fn plugin_repair_skipped_leaves_judge_pass_untouched() {
        let id = plugin_identity(&["R2_white"]);
        let judged = outcome(id.clone(), vec![pv("R2_white", verdict::PASS, Some(0.9))]);
        let repaired = outcome(
            id,
            vec![
                pv("R2_white", verdict::SKIPPED, None),
                pv("R9_extra", verdict::PASS, Some(0.5)),
            ],
        );
        let rows = plugin_rows(&judged, Some(&repaired));
        let white = rows
            .iter()
            .find(|r| r.rule_id == "R2_white")
            .expect("R2_white 行必须在");
        assert_eq!(white.result, verdict::PASS, "judge 的 pass 不被 repair 改");
        assert_eq!(white.confidence, Some(0.9));
        assert_eq!(white.score, Some(0.9));
        assert_eq!(white.recheck, None);
        assert!(white.fix_action.is_none());
        // 声明序优先：包内规则在前，repair 多出来的包外 ruleId 追加在后
        assert_eq!(rows[0].rule_id, "R2_white");
        assert_eq!(rows[1].rule_id, "R9_extra");
    }

    // ---- 队列级：真库 + mock 插件（落库/来源列/运行报告） ----

    type JudgeFn = Box<dyn Fn(&PluginAsset) -> PluginOutcome + Send + Sync>;
    type ActionFn = Box<dyn Fn(&PluginAsset, &str) -> PluginOutcome + Send + Sync>;

    struct MockPlugin {
        id: PackIdentityInfo,
        judge: JudgeFn,
        repair: Option<JudgeFn>,
        /// N6e 动作链调用（`action_asset`）；不给 = 宿主默认实现（明确否认）
        action: Option<ActionFn>,
        seen_repair_rules: std::sync::Mutex<Vec<Vec<String>>>,
    }

    impl MockPlugin {
        fn new(
            rule_ids: &[&str],
            judge: impl Fn(&PluginAsset) -> PluginOutcome + Send + Sync + 'static,
        ) -> Self {
            let id = plugin_identity(rule_ids);
            Self {
                id,
                judge: Box::new(judge),
                repair: None,
                action: None,
                seen_repair_rules: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn with_action(
            mut self,
            action: impl Fn(&PluginAsset, &str) -> PluginOutcome + Send + Sync + 'static,
        ) -> Self {
            self.action = Some(Box::new(action));
            self
        }

        fn with_repair(
            mut self,
            repair: impl Fn(&PluginAsset) -> PluginOutcome + Send + Sync + 'static,
        ) -> Self {
            self.repair = Some(Box::new(repair));
            self
        }
    }

    impl PackJudge for MockPlugin {
        fn identity(&self) -> PackIdentityInfo {
            self.id.clone()
        }

        fn judge_asset(&self, req: &PluginAsset) -> PluginOutcome {
            (self.judge)(req)
        }

        fn repair_asset(&self, req: &PluginAsset) -> PluginOutcome {
            self.seen_repair_rules
                .lock()
                .unwrap()
                .push(req.rule_ids.clone());
            match &self.repair {
                Some(f) => f(req),
                None => failed_outcome(self.id.clone(), PluginFailure::Unavailable, "no repair"),
            }
        }

        fn action_asset(&self, req: &PluginAsset, action: &str) -> PluginOutcome {
            match &self.action {
                Some(f) => f(req, action),
                // 没有脚本 = 明确否认（与宿主默认实现同口径，不静默退化成 judge）
                None => failed_outcome(self.id.clone(), PluginFailure::Unavailable, "no action"),
            }
        }
    }

    /// KI40-FIX（队列级反向验证）：插件对动作返回**结构化失败行**
    /// （`result=failed` + `error{code,message}` + evidence）时：
    ///
    /// 1. job 行必须落 `failed`（旧行为：`Done` → 调用方 `jobq::complete` 刷回 `done`）；
    /// 2. run 账本收工回填 `failed=1 / done=0`（汇总与结果表不再自相矛盾）；
    /// 3. 结果行 `errorMessage` 是**插件原文**（不再是宿主兜底的「未返回产物」）；
    /// 4. `lastError` 与结果行 `reason` **同一段文本**（界面两处同源，不一处兜底一处原文）。
    #[test]
    fn action_failed_row_fails_job_and_run_counts_it() {
        let f = Fixture::new("ki40-fix", 1);
        let pack = mini_pack();
        let cancel = AtomicBool::new(false);
        let actions = vec!["upscale".to_string()];
        let runs = SqliteComplianceRunRepo::new(&f.conn);
        let run_id = ComplianceRunRepo::start(&runs, COMPLIANCE_KIND, None).unwrap();
        compliance_enq_actions_in_run(
            &f.conn,
            &[f.good[0]],
            PLUGIN_ID,
            None,
            Some(run_id),
            &actions,
        )
        .unwrap();
        let plugin = MockPlugin::new(&["R0_gate"], |_req| {
            panic!("动作链不该走判定通道")
        })
        .with_action(|req, action| {
            assert_eq!(action, "upscale");
            PluginOutcome {
                pack: plugin_identity(&["R0_gate"]),
                verdicts: Vec::new(),
                artifacts: Vec::new(),
                rows: vec![serde_json::json!({
                    "assetId": req.asset_id.to_string(),
                    "result": "failed",
                    "error": {
                        "code": "target_over_limit",
                        "message": "目标 36264×13040（473,102,880 像素）超产物上限：像素 ≤1e8、\
                                    长边 ≤2e4；该源图可用上限 scale≤1.99；请调小 scale 或先缩小源图",
                    },
                    "evidence": {"limit": {"maxScale": 1.99, "maxOutputPixels": 100000000i64}},
                })],
                duration_ms: 574,
                failure: None,
                raw: serde_json::Value::Null,
            }
        });
        let report = run_compliance_queue_with_limit_in_run_engine(
            &f.conn,
            &pack,
            &cancel,
            &mut |_| {},
            0,
            Some(run_id),
            Some(&plugin),
        )
        .expect("run");
        assert_eq!(
            (report.done, report.failed),
            (0, 1),
            "确定性失败必须计入 failed，不许当成完成"
        );

        // ① job 行 failed（这是 run 汇总与失败原因块的唯一真源）
        let (status, last_error): (String, Option<String>) = f
            .conn
            .query_row(
                "SELECT status, lastError FROM job_queue WHERE kind = ?1",
                [COMPLIANCE_KIND],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "failed", "job 行不许停在 done");
        let last_error = last_error.expect("lastError 必须写");
        assert!(
            last_error.contains("请调小 scale"),
            "lastError 要是插件原文：{last_error}"
        );

        // ② run 账本按队列事实回填
        let run_row = ComplianceRunRepo::finish(&runs, run_id, "done", None, &[COMPLIANCE_KIND])
            .expect("收工");
        assert_eq!(
            (run_row.done, run_row.failed),
            (0, 1),
            "汇总 failed=1 / done=0"
        );

        // ③④ 结果行：失败码区分「插件结构化失败」与宿主兜底，reason 与 lastError 同源
        let (result, code, message, chain): (String, Option<String>, Option<String>, String) = f
            .conn
            .query_row(
                "SELECT result, errorCode, errorMessage, chain FROM asset_versions \
                 ORDER BY versionId DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(result, "failed");
        assert_eq!(code.as_deref(), Some("action_failed"));
        let message = message.expect("errorMessage 必须写");
        assert!(
            message.contains("目标 36264×13040"),
            "插件原文透传：{message}"
        );
        assert!(
            !message.contains("未返回产物"),
            "不许退回宿主兜底文案：{message}"
        );
        assert_eq!(Some(message), Some(last_error), "两处展示必须同源");
        // fields.chain[i] 带插件 error 与 evidence
        let chain: serde_json::Value = serde_json::from_str(&chain).unwrap();
        let step = &chain["chain"][0];
        assert_eq!(step["result"], "failed");
        assert_eq!(step["error"]["code"], "target_over_limit");
        assert_eq!(step["evidence"]["limit"]["maxScale"], 1.99);
        assert_eq!(chain["error"]["code"], "action_failed");
    }

    /// 插件通道跑通：行落在**插件包 id** 下、来源列（packVersion/pluginHash）逐行写入、
    /// legacy 包 id 下一行不落（两族互不污染）。
    #[test]
    fn plugin_mode_lands_rows_with_source_columns() {
        let f = Fixture::new("plugin-ok", 3);
        let pack = mini_pack();
        let cancel = AtomicBool::new(false);
        let ids = [f.good[0], f.bad[0]];
        compliance_enq(&f.conn, &ids, &pack.id, None).unwrap();
        let plugin = MockPlugin::new(&["R0_gate", "R1_size"], |_req| {
            outcome(
                plugin_identity(&["R0_gate", "R1_size"]),
                vec![
                    pv("R0_gate", verdict::PASS, Some(0.9)),
                    pv("R1_size", verdict::FAIL, Some(0.6)),
                ],
            )
        });
        let report = run_compliance_queue_with_limit_in_run_engine(
            &f.conn,
            &pack,
            &cancel,
            &mut |_| {},
            0,
            None,
            Some(&plugin),
        )
        .expect("run");
        assert_eq!(report.done, 2, "两个资产 job 都必须收工");
        assert_eq!(report.failed, 0);
        let rows = verdict_count(&f.conn, &format!("packId = '{PLUGIN_ID}'"));
        assert_eq!(rows, 4, "2 资产 × 2 规则");
        assert_eq!(
            verdict_count(&f.conn, &format!("packId = '{}'", pack.id)),
            0,
            "插件模式不许在 legacy 包 id 下落行（否则报告口径串味）"
        );
        let (ver, hash): (Option<String>, Option<String>) = f
            .conn
            .query_row(
                "SELECT packVersion, pluginHash FROM rule_verdict WHERE packId = ?1 LIMIT 1",
                [PLUGIN_ID],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(ver.as_deref(), Some("1.0.0"));
        assert_eq!(hash.as_deref(), Some(PLUGIN_HASH));
        // 每张图都判出了 R1_size fail → 必须触发 repair，且只把 fail 的规则交给修复
        let seen = plugin.seen_repair_rules.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "两个资产都该修一次");
        for rules in seen {
            assert_eq!(rules, vec!["R1_size".to_string()], "只修 fail 的规则");
        }
    }

    /// N1（规格实体）：规格选的**规则子集**必须随 job payload 走到插件（协议 §3.3 `rules`），
    /// 且落库行只有子集内的规则 —— 否则「规格只判 3 条」在库里显示成 6 条，结论对不上账。
    #[test]
    fn spec_rule_subset_reaches_plugin_and_limits_rows() {
        let f = Fixture::new("n1-rules", 1);
        let pack = mini_pack();
        let cancel = AtomicBool::new(false);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let seen_in_judge = seen.clone();
        compliance_enq_in_run_rules(
            &f.conn,
            &[f.good[0]],
            &pack.id,
            None,
            None,
            &["R1_size".to_string()],
        )
        .unwrap();
        let plugin = MockPlugin::new(&["R0_gate", "R1_size"], move |req| {
            seen_in_judge.lock().unwrap().push(req.rule_ids.clone());
            outcome(
                plugin_identity(&["R0_gate", "R1_size"]),
                vec![pv("R1_size", verdict::PASS, Some(0.9))],
            )
        });
        let report = run_compliance_queue_with_limit_in_run_engine(
            &f.conn,
            &pack,
            &cancel,
            &mut |_| {},
            0,
            None,
            Some(&plugin),
        )
        .expect("run");
        assert_eq!(report.done, 1);
        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen[0],
            vec!["R1_size".to_string()],
            "插件必须收到规格选的规则子集（空 = 全部；非空 = 只判这些）"
        );
        let rows = verdict_count(&f.conn, &format!("packId = '{PLUGIN_ID}'"));
        assert_eq!(rows, 1, "只落子集内的结论行（不是包内全部规则）");
    }

    /// N1：legacy 内联引擎收到规则子集必须**响亮失败**（不静默跑全部规则）。
    #[test]
    fn spec_rule_subset_rejected_on_legacy_channel() {
        let f = Fixture::new("n1-rules-legacy", 1);
        let pack = mini_pack();
        let cancel = AtomicBool::new(false);
        compliance_enq_in_run_rules(
            &f.conn,
            &[f.good[0]],
            &pack.id,
            None,
            None,
            &["R1_size".to_string()],
        )
        .unwrap();
        let report = run_compliance_queue_with_limit_in_run_engine(
            &f.conn,
            &pack,
            &cancel,
            &mut |_| {},
            0,
            None,
            None,
        )
        .expect("job 级失败不许把引擎炸掉");
        // 任务进重试/失败，且**失败原因写在 job 上**（不是静默跑全部规则）
        assert!(
            report.failed >= 1 || report.retry >= 1,
            "legacy + 规则子集必须失败/重试而不是静默跑全部：{report:?}"
        );
        let err: Option<String> = f
            .conn
            .query_row(
                "SELECT lastError FROM job_queue WHERE kind = 'compliance' LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let err = err.unwrap_or_default();
        assert!(
            err.contains("legacy 通道不支持规则子集"),
            "失败原因必须点名「legacy 不支持规则子集」，实际：{err}"
        );
    }

    /// 插件失败：job 仍 done（宿主不因插件失败退出），但每条规则落 undetermined + 运行报告响亮记录。
    #[test]
    fn plugin_timeout_is_undetermined_and_job_still_done() {
        let f = Fixture::new("plugin-timeout", 1);
        let pack = mini_pack();
        let cancel = AtomicBool::new(false);
        compliance_enq(&f.conn, &[f.good[0]], &pack.id, None).unwrap();
        let plugin = MockPlugin::new(&["R0_gate", "R1_size"], |_req| {
            failed_outcome(
                plugin_identity(&["R0_gate", "R1_size"]),
                PluginFailure::Timeout,
                "judge 超过 90000ms",
            )
        });
        let report = run_compliance_queue_with_limit_in_run_engine(
            &f.conn,
            &pack,
            &cancel,
            &mut |_| {},
            0,
            None,
            Some(&plugin),
        )
        .expect("run");
        assert_eq!(report.done, 1, "插件超时不许把 job 判死");
        assert_eq!(report.failed, 0);
        assert!(
            report
                .ruleErrors
                .iter()
                .any(|e| e.contains("plugin_timeout")),
            "运行报告必须响亮记录插件失败：{:?}",
            report.ruleErrors
        );
        let (n, undet): (i64, i64) = f
            .conn
            .query_row(
                &format!("SELECT COUNT(*), SUM({V} = 'undetermined') FROM rule_verdict WHERE packId = ?1"),
                [PLUGIN_ID],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(undet, 2, "超时 = 未判定，不是不合格");
        let reason: String = f
            .conn
            .query_row(
                "SELECT evidence FROM rule_verdict WHERE packId = ?1 LIMIT 1",
                [PLUGIN_ID],
                |r| r.get(0),
            )
            .unwrap();
        let ev: serde_json::Value = serde_json::from_str(&reason).unwrap();
        assert_eq!(ev["reason"], "plugin_timeout");
    }

    /// 连规则清单都没有 → 环境级错误（不许假装判过）：job 进重试/失败，运行报告有明细。
    #[test]
    fn plugin_without_rules_fails_loudly() {
        let f = Fixture::new("plugin-norules", 1);
        let pack = mini_pack();
        let cancel = AtomicBool::new(false);
        compliance_enq(&f.conn, &[f.good[0]], &pack.id, None).unwrap();
        let plugin = MockPlugin::new(&[], |_req| {
            failed_outcome(plugin_identity(&[]), PluginFailure::Unavailable, "包未装载")
        });
        let report = run_compliance_queue_with_limit_in_run_engine(
            &f.conn,
            &pack,
            &cancel,
            &mut |_| {},
            0,
            None,
            Some(&plugin),
        )
        .expect("run");
        assert_eq!(report.done, 0);
        assert!(
            report.retry + report.failed >= 1,
            "没有规则清单 = 环境错误，必须重试/失败暴露：{}",
            report.retry + report.failed
        );
        assert_eq!(verdict_count(&f.conn, "1 = 1"), 0, "不许落任何假结论");
    }
}

// ---------- C5 测试：VL 通道编排（mock VL 服务）+ 并行驱动等价性 ----------
#[cfg(test)]
mod c5_tests {
    use super::*;
    use crate::compliance_vl::VlConfig;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// N5b（0028）：判定结论在 `verdict` 列（可空），`result` 只表示「跑成了没有」。
    const V: &str = "COALESCE(verdict, CASE result WHEN 'skipped' THEN 'skipped' \
                     ELSE 'undetermined' END)";

    /// 极简 HTTP mock：按请求序返回预设 content（读完请求体再应答，兼容大 data URL）。
    fn spawn_mock(responses: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut it = responses.into_iter();
            for req in listener.incoming() {
                let Some(answer) = it.next() else { break };
                let mut stream = req.unwrap();
                let mut buf: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 16384];
                loop {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header_end = pos + 4;
                        let head = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= header_end + len {
                            break;
                        }
                    }
                }
                let body = serde_json::json!({
                    "choices": [{ "message": { "content": answer } }]
                })
                .to_string();
                let http = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(http.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}")
    }

    fn vl_cfg(base: String) -> VlConfig {
        VlConfig {
            base_url: base,
            api_key: "mock-key".into(),
            model: "mock-vl".into(),
            timeout_secs: 30,
            max_tokens: 512,
            proxy: Some(String::new()), // 直连（本地 mock 不过 env 代理）
        }
    }

    const C5_RULES: &str = r#"
- id: R3_no_watermark
  name: 无水印
  executor: vl
  check: {detector: vl_judge, params: {prompt: "是否有水印", expect: 否}, confidence: {source: fixed, value: 0.9}}
  fixer: {type: inpaint, tier: best_effort}
  on_fail: retry_then_human
- id: R5_attribute
  name: 属性打标
  executor: vl
  outputs: [dress_silhouette, scene]
  fixer: null
  on_fail: pass
"#;
    const C5_PIPELINE: &str = "steps:\n  - {kind: judge, rule: R3_no_watermark}\n  - {kind: judge, rule: R5_attribute}\nrecheck_max: 1\n";
    const C5_REPORT: &str = "summary: [合格, 不合格, 未判定]\ndetail_columns: [imageSN, R3_no_watermark, R5_attribute, R5_attribute_tags]\n";

    fn c5_pack() -> Pack {
        Pack::from_yaml_strs(
            "c5mock",
            "dress_silhouette: [A字, 直筒]\nscene: [平铺, 白底图]\n",
            C5_RULES,
            C5_PIPELINE,
            C5_REPORT,
        )
        .unwrap()
    }

    /// 纯色 jpeg（VL 判定走 mock，图片内容无关紧要，但必须是可解码真文件）。
    fn write_jpeg(dir: &Path, name: &str, w: u32, h: u32) -> PathBuf {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([120, 130, 140]));
        let dest = dir.join(name);
        img.save_with_format(&dest, image::ImageFormat::Jpeg)
            .unwrap();
        dest
    }

    /// 文件库 + 注入资产（返回未打开的 Result 连接，调用方按顺序/并行各自 open）。
    fn setup_file_db(tag: &str, images: &[PathBuf]) -> (core_storage::Result<Connection>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("dam-c5-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("out")).unwrap();
        let db_path = dir.join("library.db");
        let conn = core_storage::open(&db_path);
        if let Ok(c) = &conn {
            for (i, p) in images.iter().enumerate() {
                let name = format!("c5-{i:02}.jpg");
                c.execute(
                    "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, importedAt)
                     VALUES (NULL, ?1, 'image', ?2, 'jpg', 1, 0, 0)",
                    rusqlite::params![p.to_string_lossy(), name],
                )
                .unwrap();
                let id = c.last_insert_rowid();
                let (w, h) = core_media::image_size(p).unwrap();
                c.execute(
                    "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1, 'primary', 0, ?2, ?3)",
                    rusqlite::params![id, w as i64, h as i64],
                )
                .unwrap();
            }
        }
        (conn, dir)
    }

    fn asset_ids(conn: &Connection) -> Vec<i64> {
        let mut stmt = conn.prepare("SELECT assetId FROM assets").unwrap();
        stmt.query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn factory_for(db: PathBuf) -> ConnFactory {
        Arc::new(move || {
            core_storage::open(&db).map_err(|e| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e)))
            })
        })
    }

    fn verdicts(
        conn: &Connection,
        pack_id: &str,
    ) -> Vec<(String, String, Option<String>, Option<String>)> {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT ruleId, {V}, fixAction, recheckResult FROM rule_verdict
                 WHERE packId = ?1 ORDER BY ruleId"
            ))
            .unwrap();
        stmt.query_map([pack_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
    }

    #[test]
    fn vl_judge_fail_inpaint_model_missing_to_human_and_tags_pass() {
        // 模型根指向空目录 → lama 文件缺失 → ModelMissing → pending_human（降级分支）
        // 模型根是进程级全局态 → ModelsRootScope 取 core_config::models_root_lock() + drop 还原
        // （并行 cargo test 不再互相踩；卡 task）
        let empty_root =
            std::env::temp_dir().join(format!("dam-c5-empty-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&empty_root);
        std::fs::create_dir_all(empty_root.join("models")).unwrap();
        let _restore_scope = engine_restore::ModelsRootScope::set(&empty_root);
        let tmp = std::env::temp_dir().join(format!("dam-c5-img-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let img = write_jpeg(&tmp, "a.jpg", 640, 480);
        let (conn, dir) = setup_file_db("vl1", &[img]);
        let conn = conn.unwrap();
        let ids = asset_ids(&conn);
        let pack = c5_pack();
        let out_dir = dir.join("out").to_string_lossy().to_string();
        let enqed = compliance_enq(&conn, &ids, pack.id.as_str(), Some(&out_dir)).unwrap();
        assert_eq!(enqed, 1);

        let base = spawn_mock(vec![
            // 合并轮：R3 违规（带 bbox）+ R5 打标（含一个词表外值）
            r#"{"rules":{"R3_no_watermark":{"violated":true,"regions":[{"what":"水印","bbox":[0.1,0.1,0.4,0.2]}]}},"tags":{"dress_silhouette":"A字","scene":"蓬蓬裙"}}"#.to_string(),
        ]);
        let (rep, vl_snap) = run_compliance_queue_par(
            factory_for(dir.join("library.db")),
            &pack,
            Some(vl_cfg(base)),
            1,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .expect("par run");
        assert_eq!(rep.done, 1, "{rep:?}");
        assert_eq!(vl_snap.expect("vl stats").calls, 1, "合并调用一次");

        let vs = verdicts(&conn, &pack.id);
        assert_eq!(vs.len(), 2, "{vs:?}");
        let r3 = &vs[0]; // 字典序 R3 在前
        let r5 = &vs[1];
        assert_eq!(r3.0, "R3_no_watermark");
        assert_eq!(r3.1, "undetermined", "模型缺失 = 执行态缺失 → 未判定");
        assert_eq!(r3.2.as_deref(), Some("inpaint"));
        let ev: String = conn
            .query_row(
                "SELECT evidence FROM rule_verdict WHERE packId=?1 AND ruleId='R3_no_watermark'",
                [&pack.id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(ev.contains("model_not_downloaded"), "{ev}");
        assert!(ev.contains("bbox"), "{ev}");
        assert_eq!(r3.3, None);

        assert_eq!(r5.0, "R5_attribute");
        assert_eq!(r5.1, "pass", "打标型 on_fail=pass");
        let ev5: String = conn
            .query_row(
                "SELECT evidence FROM rule_verdict WHERE packId=?1 AND ruleId='R5_attribute'",
                [&pack.id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(ev5.contains("A字"), "{ev5}");
        assert!(ev5.contains("scene:蓬蓬裙"), "词表外值进 rejected: {ev5}");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&empty_root);
    }

    // ---------- DM-C1：inpaint 重绘区域决议（VL bbox × OCR 行框） ----------

    /// 只带一条 inpaint 修复规则的最小 VL 包（`region` 可换成 explicit/ocr_text/u2net）。
    fn dm_c1_pack(region: Option<&str>) -> Pack {
        let fixer = match region {
            Some(r) => format!("{{type: inpaint, tier: best_effort, params: {{region: {r}}}}}"),
            None => "{type: inpaint, tier: best_effort}".to_string(),
        };
        let rules = format!(
            r#"
- id: R3_no_watermark
  name: 无水印
  executor: vl
  check: {{detector: vl_judge, params: {{prompt: "是否有水印", expect: 否}}, confidence: {{source: fixed, value: 0.9}}}}
  fixer: {fixer}
  on_fail: retry_then_human
"#
        );
        Pack::from_yaml_strs(
            "dmc1",
            "category: [连衣裙]\n",
            &rules,
            "steps:\n  - {kind: judge, rule: R3_no_watermark}\nrecheck_max: 1\n",
            "summary: [合格, 不合格, 未判定]\ndetail_columns: [imageSN, R3_no_watermark]\n",
        )
        .unwrap()
    }

    /// 给资产灌 OCR 行框（模拟 OCR 已跑过；归一化坐标）。
    fn seed_ocr_boxes(conn: &Connection, asset_id: i64, rects: &[[f64; 4]]) {
        let boxes: Vec<core_storage::ocr_boxes::OcrBox> = rects
            .iter()
            .map(|r| core_storage::ocr_boxes::OcrBox {
                x0: r[0],
                y0: r[1],
                x1: r[2],
                y1: r[3],
                text: "水印".into(),
                box_score: 0.9,
                text_score: 0.8,
            })
            .collect();
        core_storage::ocr_boxes::replace(conn, asset_id, &boxes).unwrap();
    }

    fn evidence_of(conn: &Connection, pack_id: &str, rule: &str) -> serde_json::Value {
        let ev: String = conn
            .query_row(
                "SELECT evidence FROM rule_verdict WHERE packId=?1 AND ruleId=?2",
                rusqlite::params![pack_id, rule],
                |r| r.get(0),
            )
            .unwrap();
        serde_json::from_str(&ev).unwrap()
    }

    /// VL 指的框与该图 OCR 文本行框一个都重叠不过半 → **拒绝重绘**（不拿幻觉去改真像素）。
    #[test]
    fn dm_c1_vl_bbox_without_ocr_support_is_refused() {
        let tmp = std::env::temp_dir().join(format!("dam-dmc1-mis-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let img = write_jpeg(&tmp, "m.jpg", 640, 480);
        let (conn, dir) = setup_file_db("dmc1mis", &[img]);
        let conn = conn.unwrap();
        let ids = asset_ids(&conn);
        let pack = dm_c1_pack(None);
        // OCR 文本在右下
        seed_ocr_boxes(&conn, ids[0], &[[0.60, 0.60, 0.90, 0.80]]);
        let out_dir = dir.join("out").to_string_lossy().to_string();
        compliance_enq(&conn, &ids, pack.id.as_str(), Some(&out_dir)).unwrap();

        // VL 说水印在左上（模型幻觉）
        let base = spawn_mock(vec![
            r#"{"rules":{"R3_no_watermark":{"violated":true,"regions":[{"what":"水印","bbox":[0.05,0.05,0.25,0.20]}]}}}"#.to_string(),
        ]);
        let (rep, _) = run_compliance_queue_par(
            factory_for(dir.join("library.db")),
            &pack,
            Some(vl_cfg(base)),
            1,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .expect("par run");
        assert_eq!(rep.done, 1, "{rep:?}");

        let vs = verdicts(&conn, &pack.id);
        assert_eq!(vs.len(), 1, "{vs:?}");
        assert_eq!(vs[0].1, "undetermined", "区域无文本支撑 → 未判定（不重绘）");
        assert_eq!(
            vs[0].2.as_deref(),
            Some("inpaint"),
            "失败的是修复（不是判定）"
        );
        assert_eq!(vs[0].3, None, "没有复检（根本没修）");
        let ev = evidence_of(&conn, &pack.id, "R3_no_watermark");
        assert_eq!(ev["region"]["reason"], "vl_region_mismatch", "{ev}");
        assert_eq!(
            ev["region"]["threshold"],
            crate::inpaint_region::VL_OCR_OVERLAP_MIN
        );
        assert_eq!(
            ev["region"]["ocrBoxes"].as_array().unwrap().len(),
            1,
            "{ev}"
        );
        assert_eq!(
            ev["region"]["vlRegions"].as_array().unwrap().len(),
            1,
            "{ev}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// VL 框与 OCR 行框对得上 → 用 OCR 框重绘（区域证据落库：来源 + 框）。
    #[test]
    fn dm_c1_vl_bbox_with_ocr_support_uses_ocr_box() {
        let empty_root = std::env::temp_dir().join(format!("dam-dmc1-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&empty_root);
        std::fs::create_dir_all(empty_root.join("models")).unwrap();
        let _restore_scope = engine_restore::ModelsRootScope::set(&empty_root);
        let tmp = std::env::temp_dir().join(format!("dam-dmc1-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let img = write_jpeg(&tmp, "o.jpg", 640, 480);
        let (conn, dir) = setup_file_db("dmc1ok", &[img]);
        let conn = conn.unwrap();
        let ids = asset_ids(&conn);
        let pack = dm_c1_pack(None);
        let ocr_box = [0.12, 0.12, 0.38, 0.18];
        seed_ocr_boxes(&conn, ids[0], &[ocr_box]);
        let out_dir = dir.join("out").to_string_lossy().to_string();
        compliance_enq(&conn, &ids, pack.id.as_str(), Some(&out_dir)).unwrap();

        // VL 给的框比行框松一圈（真实水印框的常态）
        let base = spawn_mock(vec![
            r#"{"rules":{"R3_no_watermark":{"violated":true,"regions":[{"what":"水印","bbox":[0.10,0.10,0.40,0.20]}]}}}"#.to_string(),
        ]);
        let (rep, _) = run_compliance_queue_par(
            factory_for(dir.join("library.db")),
            &pack,
            Some(vl_cfg(base)),
            1,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .expect("par run");
        assert_eq!(rep.done, 1, "{rep:?}");

        let ev = evidence_of(&conn, &pack.id, "R3_no_watermark");
        assert_eq!(ev["region"]["source"], "vl_bbox_verified", "{ev}");
        assert_eq!(
            ev["region"]["boxes"],
            serde_json::json!([ocr_box]),
            "注入的是 OCR 框而不是 VL 框: {ev}"
        );
        let overlap = ev["region"]["overlap"].as_f64().unwrap();
        assert!(overlap > crate::inpaint_region::VL_OCR_OVERLAP_MIN, "{ev}");
        // 模型缺失（空模型根）→ 未判定，但区域决议已经过了（这就是本用例要保的那一步）
        assert_eq!(ev["reason"], "model_not_downloaded", "{ev}");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&empty_root);
    }

    /// `region: ocr_text`：有框就按 OCR 行框注入；无框回落 u2net；u2net 不可用 → 明确拒绝
    /// （不是 `vl_no_region`，也不是静默退回 VL bbox）。
    #[test]
    fn dm_c1_region_modes_ocr_text_and_fallback() {
        let empty_root =
            std::env::temp_dir().join(format!("dam-dmc1-root2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&empty_root);
        std::fs::create_dir_all(empty_root.join("models")).unwrap();
        let _restore_scope = engine_restore::ModelsRootScope::set(&empty_root);
        let tmp = std::env::temp_dir().join(format!("dam-dmc1-ocr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // ---- ① 有 OCR 框：直接按行框注入（不依赖 VL 指哪） ----
        {
            let img = write_jpeg(&tmp, "t1.jpg", 640, 480);
            let (conn, dir) = setup_file_db("dmc1ocr1", &[img]);
            let conn = conn.unwrap();
            let ids = asset_ids(&conn);
            let pack = dm_c1_pack(Some("ocr_text"));
            let boxes = [[0.20, 0.30, 0.60, 0.40], [0.20, 0.50, 0.55, 0.60]];
            seed_ocr_boxes(&conn, ids[0], &boxes);
            let out_dir = dir.join("out").to_string_lossy().to_string();
            compliance_enq(&conn, &ids, pack.id.as_str(), Some(&out_dir)).unwrap();
            // VL 完全没给框（只要判定）
            let base = spawn_mock(vec![
                r#"{"rules":{"R3_no_watermark":{"violated":true,"regions":[]}}}"#.to_string(),
            ]);
            let (rep, _) = run_compliance_queue_par(
                factory_for(dir.join("library.db")),
                &pack,
                Some(vl_cfg(base)),
                1,
                &AtomicBool::new(false),
                &mut |_| {},
            )
            .expect("par run");
            assert_eq!(rep.done, 1, "{rep:?}");
            let ev = evidence_of(&conn, &pack.id, "R3_no_watermark");
            assert_eq!(ev["region"]["source"], "ocr_text", "{ev}");
            assert_eq!(
                ev["region"]["boxes"],
                serde_json::json!(boxes),
                "两行文本 → 两个框（多框走合成掩码）: {ev}"
            );
        }

        // ---- ② 无 OCR 框：回落 u2net；空模型根 → 明确 region_unavailable ----
        {
            let img = write_jpeg(&tmp, "t2.jpg", 640, 480);
            let (conn, dir) = setup_file_db("dmc1ocr2", &[img]);
            let conn = conn.unwrap();
            let ids = asset_ids(&conn);
            let pack = dm_c1_pack(Some("ocr_text"));
            let out_dir = dir.join("out").to_string_lossy().to_string();
            compliance_enq(&conn, &ids, pack.id.as_str(), Some(&out_dir)).unwrap();
            let base = spawn_mock(vec![
                r#"{"rules":{"R3_no_watermark":{"violated":true,"regions":[{"what":"水印","bbox":[0.1,0.1,0.3,0.2]}]}}}"#.to_string(),
            ]);
            let (rep, _) = run_compliance_queue_par(
                factory_for(dir.join("library.db")),
                &pack,
                Some(vl_cfg(base)),
                1,
                &AtomicBool::new(false),
                &mut |_| {},
            )
            .expect("par run");
            assert_eq!(rep.done, 1, "{rep:?}");
            let ev = evidence_of(&conn, &pack.id, "R3_no_watermark");
            assert_eq!(ev["region"]["reason"], "region_unavailable", "{ev}");
            assert_eq!(ev["region"]["mode"], "u2net", "回落的是 u2net 通道: {ev}");
        }

        // ---- ③ 未知 region 取值：响亮拒绝（不静默退回 VL bbox） ----
        {
            let img = write_jpeg(&tmp, "t3.jpg", 640, 480);
            let (conn, dir) = setup_file_db("dmc1ocr3", &[img]);
            let conn = conn.unwrap();
            let ids = asset_ids(&conn);
            let pack = dm_c1_pack(Some("u2netx"));
            seed_ocr_boxes(&conn, ids[0], &[[0.1, 0.1, 0.3, 0.2]]);
            let out_dir = dir.join("out").to_string_lossy().to_string();
            compliance_enq(&conn, &ids, pack.id.as_str(), Some(&out_dir)).unwrap();
            let base = spawn_mock(vec![
                r#"{"rules":{"R3_no_watermark":{"violated":true,"regions":[{"what":"水印","bbox":[0.1,0.1,0.3,0.2]}]}}}"#.to_string(),
            ]);
            let (rep, _) = run_compliance_queue_par(
                factory_for(dir.join("library.db")),
                &pack,
                Some(vl_cfg(base)),
                1,
                &AtomicBool::new(false),
                &mut |_| {},
            )
            .expect("par run");
            assert_eq!(rep.done, 1, "{rep:?}");
            let ev = evidence_of(&conn, &pack.id, "R3_no_watermark");
            assert_eq!(ev["region"]["reason"], "region_mode_invalid", "{ev}");
            assert_eq!(ev["region"]["allowed"].as_array().unwrap().len(), 3, "{ev}");
        }
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&empty_root);
    }

    /// 注入矩阵（卡内验收）：**哪种 region 注入什么、哪种绝对不注入 VL 框**。
    ///
    /// 反向验证：把 `resolve_inpaint_region` 里「declared / region 模式」的分支去掉（退回修前的
    /// 「非空 bbox 就注入」），本用例第 2/4 条立刻红 —— 因为 params 里会多出 VL 的 box。
    #[test]
    fn dm_c1_region_injection_matrix() {
        let tmp = std::env::temp_dir().join(format!("dam-dmc1-matrix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let img = write_jpeg(&tmp, "x.jpg", 640, 480);
        let vl_bbox = [0.05, 0.05, 0.25, 0.20];
        let out = VlJudgeOut {
            violated: true,
            regions: vec![vl_bbox],
            where_: None,
            answer: Some("否".into()),
            confidence: None,
            raw: serde_json::json!({}),
        };
        let make_ctx = |ocr: Vec<[f64; 4]>| AssetCtx {
            snap: core_storage::compliance::AssetSnapshot {
                asset_id: 1,
                abs_path: img.to_string_lossy().to_string(),
                media_type: "image".into(),
                missing: 0,
                deleted: 0,
                width: Some(640),
                height: Some(480),
                file_hash: None,
                p_hash: None,
            },
            rgb: None,
            out_dir: tmp.clone(),
            asset_id: 1,
            vl: None,
            ocr_boxes: ocr,
        };
        let rule_of = |pack: &Pack| pack.rules[0].clone();
        // fixer 参数原文（`region` 就在这里；编排器传进 resolve 的就是这份）
        let params_of = |rule: &core_config::Rule| {
            rule.fixer
                .as_ref()
                .map(|f| f.params.clone())
                .unwrap_or(serde_yaml::Value::Null)
        };

        // ① explicit + 声明的 box → 用声明的，原样不覆盖
        {
            let pack = dm_c1_pack(Some("explicit"));
            let rule = rule_of(&pack);
            let mut params = params_of(&rule);
            params["box"] = serde_yaml::from_str("[0.10, 0.10, 0.20, 0.20]").unwrap();
            let mut ctx = make_ctx(vec![]);
            let ev = resolve_inpaint_region(&mut ctx, &rule, &mut params, &out).expect("显式区域");
            assert_eq!(ev["source"], "explicit");
            assert!(params.get("mask").is_none(), "{params:?}");
            assert_eq!(
                serde_yaml::from_value::<[f64; 4]>(params.get("box").unwrap().clone()).unwrap(),
                [0.10, 0.10, 0.20, 0.20],
                "声明的 box 不许被 VL 框覆盖"
            );
        }

        // ② explicit 但没声明 box/mask → 拒绝，且**不许**把 VL 框注入进去
        {
            let pack = dm_c1_pack(Some("explicit"));
            let rule = rule_of(&pack);
            let mut params = params_of(&rule);
            let mut ctx = make_ctx(vec![]);
            let (reason, ev) =
                resolve_inpaint_region(&mut ctx, &rule, &mut params, &out).unwrap_err();
            assert_eq!(reason, "vl_no_region", "{ev}");
            assert!(
                params.get("box").is_none(),
                "修前 bug：这里会注入 VL bbox → {params:?}"
            );
            assert!(params.get("mask").is_none(), "{params:?}");
        }

        // ③ ocr_text + 单行框 → 注入 OCR 框（不是 VL 框）
        {
            let pack = dm_c1_pack(Some("ocr_text"));
            let rule = rule_of(&pack);
            let ocr = [0.30, 0.40, 0.70, 0.50];
            let mut params = params_of(&rule);
            let mut ctx = make_ctx(vec![ocr]);
            let ev = resolve_inpaint_region(&mut ctx, &rule, &mut params, &out).expect("OCR 区域");
            assert_eq!(ev["source"], "ocr_text");
            let injected: [f64; 4] =
                serde_yaml::from_value(params.get("box").expect("注入 box").clone()).unwrap();
            assert_eq!(injected, ocr);
            assert_ne!(injected, vl_bbox, "注入的必须是 OCR 框");
        }

        // ④ u2net（本机构建无模型）→ 拒绝；且不注入任何 VL 框/掩码
        {
            let pack = dm_c1_pack(Some("u2net"));
            let rule = rule_of(&pack);
            let mut params = params_of(&rule);
            let mut ctx = make_ctx(vec![]);
            let (reason, ev) =
                resolve_inpaint_region(&mut ctx, &rule, &mut params, &out).unwrap_err();
            assert_eq!(reason, "region_unavailable", "{ev}");
            assert!(params.get("box").is_none(), "{params:?}");
            assert!(params.get("mask").is_none(), "{params:?}");
        }

        // ⑤ 默认通道（未声明 region）：VL 框有 OCR 支撑 → 注入 OCR 框
        {
            let pack = dm_c1_pack(None);
            let rule = rule_of(&pack);
            let ocr = [0.06, 0.06, 0.24, 0.19];
            let mut params = params_of(&rule);
            let mut ctx = make_ctx(vec![ocr]);
            let ev = resolve_inpaint_region(&mut ctx, &rule, &mut params, &out).expect("已复核");
            assert_eq!(ev["source"], "vl_bbox_verified");
            let injected: [f64; 4] =
                serde_yaml::from_value(params.get("box").expect("注入 box").clone()).unwrap();
            assert_eq!(injected, ocr);
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn vl_pass_par_and_classic_par_matches_sequential() {
        // 断言「本用例不依赖注入根」的读侧用法：取锁 + 清根，drop 自动还原
        let _restore_scope = engine_restore::ModelsRootScope::clear();
        let tmp = std::env::temp_dir().join(format!("dam-c5-img2-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let imgs: Vec<PathBuf> = (0..4)
            .map(|i| write_jpeg(&tmp, &format!("b{i}.jpg"), 800, 800))
            .collect();

        // ---- VL pass 路径（par 入口 + mock；每资产一次合并调用）----
        {
            let (conn, dir) = setup_file_db("vl2", &imgs);
            let conn = conn.unwrap();
            let ids = asset_ids(&conn);
            let pack = c5_pack();
            let out_dir = dir.join("out").to_string_lossy().to_string();
            compliance_enq(&conn, &ids, pack.id.as_str(), Some(&out_dir)).unwrap();
            let answer = r#"{"rules":{"R3_no_watermark":{"violated":false,"regions":[]}},"tags":{"dress_silhouette":"直筒","scene":"白底图"}}"#.to_string();
            let base = spawn_mock(vec![answer.clone(), answer.clone(), answer.clone(), answer]);
            let (rep, vl_snap) = run_compliance_queue_par(
                factory_for(dir.join("library.db")),
                &pack,
                Some(vl_cfg(base)),
                2,
                &AtomicBool::new(false),
                &mut |_| {},
            )
            .expect("par run");
            assert_eq!(rep.done, 4, "{rep:?}");
            assert_eq!(vl_snap.expect("vl stats").calls, 4, "每资产一次合并调用");
            for (rule, result, _fix, _re) in verdicts(&conn, &pack.id) {
                assert_eq!(result, "pass", "{rule}");
            }
        }

        // ---- par vs 顺序等价（classic-only，3 workers）----
        {
            const P_RULES: &str = r#"
- id: R1_size
  name: 尺寸
  executor: classic
  check: {detector: size_min, params: {min: 700}}
  fixer: null
  on_fail: fail
"#;
            const P_PIPE: &str = "steps:\n  - {kind: judge, rule: R1_size}\nrecheck_max: 1\n";
            const P_REP: &str =
                "summary: [合格, 不合格, 未判定]\ndetail_columns: [imageSN, R1_size]\n";
            let pack =
                Pack::from_yaml_strs("c5par", "category: [x]\n", P_RULES, P_PIPE, P_REP).unwrap();
            let big: Vec<PathBuf> = (0..2)
                .map(|i| write_jpeg(&tmp, &format!("big{i}.jpg"), 900, 900))
                .collect();
            let small: Vec<PathBuf> = (0..2)
                .map(|i| write_jpeg(&tmp, &format!("small{i}.jpg"), 400, 400))
                .collect();
            let all: Vec<PathBuf> = big.iter().chain(small.iter()).cloned().collect();

            let (conn_s, _) = setup_file_db("seq", &all);
            let conn_s = conn_s.unwrap();
            let ids_s = asset_ids(&conn_s);
            compliance_enq(&conn_s, &ids_s, pack.id.as_str(), None).unwrap();
            run_compliance_queue(&conn_s, &pack, &AtomicBool::new(false), &mut |_| {}).unwrap();
            let mut seq_rows = verdicts(&conn_s, &pack.id);
            seq_rows.sort();

            let (conn_p, dir_p) = setup_file_db("par", &all);
            let conn_p = conn_p.unwrap();
            let ids_p = asset_ids(&conn_p);
            compliance_enq(&conn_p, &ids_p, pack.id.as_str(), None).unwrap();
            let (rep, _) = run_compliance_queue_par(
                factory_for(dir_p.join("library.db")),
                &pack,
                None,
                3,
                &AtomicBool::new(false),
                &mut |_| {},
            )
            .expect("par run");
            assert_eq!(rep.done, 4, "{rep:?}");
            let mut par_rows = verdicts(&conn_p, &pack.id);
            par_rows.sort();
            assert_eq!(
                seq_rows, par_rows,
                "同批 classic 规则：顺序与并行判定逐行一致"
            );
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// VL 只报方位格（不给精确 bbox）时，修复区域由 3×3 格中心块兜底——
    /// 保证「VL 判违规」不会因为拿不到 bbox 而白白转人工。
    #[test]
    fn vl_where_cell_box_fallback() {
        let out = crate::compliance_vl::VlJudgeOut {
            violated: true,
            answer: None,
            regions: vec![],
            where_: Some("bottom_right".into()),
            confidence: None,
            raw: serde_json::json!({"violated": true, "where": "bottom_right"}),
        };
        let b = out.fix_regions();
        assert_eq!(b.len(), 1);
        assert!(b[0][0] < 0.05, "x0={}", b[0][0]);
        assert!(b[0][1] > 0.6 && b[0][1] < 0.70, "y0={}", b[0][1]);
        assert!(b[0][2] > 0.95, "x1={}", b[0][2]);
        assert!(b[0][3] > 0.95 && b[0][3] < 0.995, "y1={}", b[0][3]);
        let out2 = crate::compliance_vl::VlJudgeOut {
            violated: true,
            answer: None,
            regions: vec![[0.1, 0.2, 0.3, 0.4]],
            where_: Some("bottom_right".into()),
            confidence: None,
            raw: serde_json::json!({}),
        };
        assert_eq!(out2.fix_regions(), vec![[0.1, 0.2, 0.3, 0.4]]);
        let out3 = crate::compliance_vl::VlJudgeOut {
            violated: true,
            answer: None,
            regions: vec![],
            where_: Some("nowhere".into()),
            confidence: None,
            raw: serde_json::json!({}),
        };
        assert!(out3.fix_regions().is_empty());
    }
}
