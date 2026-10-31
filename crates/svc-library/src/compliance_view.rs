#![allow(non_snake_case)] // 本模块全部视图字段 = JSON 线上 camelCase 契约，勿改
//! C6 合规状态视图层：**单资产合规状态的唯一口径** + 人工反馈（误判）读写 + 待人工队列。
//!
//! 为什么单独一个模块：合规状态此前只在导出报告里算过一次（`export::asset_status`），
//! 现在它同时是①报告汇总、②库视图「合规」筛选、③审核队列分桶的依据 —— 三处必须同源，
//! 否则「界面说 12 张待人工、报告说 13 张」这类漂移无法自证。故口径函数收在这里，
//! 报告与筛选都调它（`state_of_verdicts_with` / `state_condition`）。
//!
//! 两个口径的对账：Rust 口径（`state_of_verdicts_with`）与 SQL 口径（`state_condition`）由
//! 单测 `sql_and_rust_agree_on_every_state` 在覆盖三态 **+ 人工改判（规则级/资产级/撤销后）**
//! 的夹具上逐资产比对，不一致即测试失败（防两套实现悄悄分叉）。
//!
//! 反馈（`rule_feedback`）语义：机器结论由**库里的判定行**推导（不接受前端自报，
//! 防伪造）；`machineResult != humanResult` 即一次误判记录。v1 **不回写 `rule_verdict`**
//! ——队列项靠 `resolved`（该 (asset,rule) 已有反馈）标记人工已处置，机器判定保持可复现。
//!
//! COMPLIANCE-HUMAN（卡 task）：人工**改判**走独立表 `verdict_human_review`（0017），
//! 与上面的「误判标记」不是一回事 —— 改判**改变生效结论**（effectiveStatus = humanStatus ?? result，查询时合成），
//! 反馈只做误判统计。生效结论由 `ReviewIndex` 统一解释，资产状态/库筛选/统计/报告全走它，
//! 前端不用自己算。

use core_config::Pack;
use core_storage::compliance::{
    ComplianceRepo, FeedbackFilter, FeedbackRow, HumanReviewFilter, HumanReviewRow, NewFeedback,
    NewHumanReview, VerdictFilter, VerdictRow,
};
use core_storage::compliance_sqlite::SqliteComplianceRepo;
use core_types::ComplianceStateFilter;
use rusqlite::Connection;

use crate::export::csv_field;
use crate::{LibraryError, Result};

// ---------------------------------------------------------------- 状态口径

/// 单资产合规状态 = 该资产全部规则判定行的汇总（**唯一口径**，库视图/报告/统计同源）。
///
/// 2026-10-17 裁决（COMPLIANCE-TRISTATE）：只有三态 pass / fail / undetermined；
/// `fixed`（修复后合格）与 `pending_human`（待人工）两个派生档位已删除 ——
/// 修复成功就是 pass（fix 记录留 evidence），修不了/判不了就是 fail 或 undetermined。
/// `unjudged` 不是判定结果，是「这个 pack 下这张图一行判定都没有」（还没跑）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetComplianceState {
    /// 有判定行，无硬 fail、无 undetermined
    Pass,
    /// 任一硬 fail（压过 undetermined：不合格是结论，未判定只是没结论）
    Fail,
    /// 无硬 fail 但有 undetermined（判定过程出错：读图失败/VL 超时或不可解析/插件异常）
    Undetermined,
    /// 该 pack 下无任何判定行
    Unjudged,
}

impl AssetComplianceState {
    /// 四档（UI 顺序：先看要处理的，再看结论）
    pub const ALL: [AssetComplianceState; 4] = [
        AssetComplianceState::Fail,
        AssetComplianceState::Undetermined,
        AssetComplianceState::Pass,
        AssetComplianceState::Unjudged,
    ];

    /// 稳定词（HTTP/筛选/CSV 用；前端 live filter 只认这四个）
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Undetermined => "undetermined",
            Self::Unjudged => "unjudged",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pass" | "fixed" => Some(Self::Pass),
            "fail" => Some(Self::Fail),
            "undetermined" | "pending_human" => Some(Self::Undetermined),
            "unjudged" => Some(Self::Unjudged),
            _ => None,
        }
    }

    /// 中文名（界面/报告）
    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Pass => "合格",
            Self::Fail => "不合格",
            Self::Undetermined => "未判定",
            Self::Unjudged => "未判定（未跑）",
        }
    }

    /// 判定报告 `summary` 档位下标（0 合格 / 1 不合格 / 2 未判定）；Unjudged 无档位。
    pub fn report_index(self) -> Option<usize> {
        match self {
            Self::Pass => Some(0),
            Self::Fail => Some(1),
            Self::Undetermined => Some(2),
            Self::Unjudged => None,
        }
    }
}

/// 判定行 → 资产状态（**纯机器口径**；空行集 = 未判定）。
///
/// 等价于 `state_of_verdicts_with(rows, &ReviewIndex::default())`。生产路径（汇总/筛选/报告）
/// 一律走 `_with` 版本（人工改判也要参与），这里只保留给「只看机器结论」的对照场景（审计/测试）。
///
/// 软 fail 判定（evidence.on_fail = "pass"，打标型/软规则）不影响合格结论；evidence 非法 json 时
/// 视为硬 fail（保守：证据读不出来不能当作"没问题"）。
pub fn state_of_verdicts(rows: &[&VerdictRow]) -> AssetComplianceState {
    state_of_verdicts_with(rows, &ReviewIndex::default())
}

/// 判定行 + 人工改判 → 资产状态（**生效口径**：effectiveStatus = humanStatus ?? result，查询时合成）。
///
/// `rows` 必须是**同一个资产**的判定行（调用方已按 assetId 分组）。判定顺序与 SQL 口径
/// （`core_storage::compliance_sqlite::state_condition_sql`）严格同义：
///
/// 1. 空行集 → Unjudged（该 pack 下没跑过判定；资产级改判也救不了 —— 改判必须建立在机器判定之上）；
/// 2. 有资产级改判（`ruleId = NULL`）→ 直接取人工裁决，不看逐规则汇总；
/// 3. 否则逐行取**生效结论**：任一硬 fail → Fail（压过 undetermined）；再任一 undetermined →
///    Undetermined；**一行结论都没有（全是 skipped）→ Undetermined**；其余 → Pass。
///
/// 第 3 步最后那条是卡 task 的口径修正：`skipped` = 「这条规则对这张图不适用」，
/// 全 skipped 说明这张图在本次判定里**没有任何结论**（典型：非商品图 —— 领域 gate 判不适用、
/// 其余规则连带短路）。先前这里落 Pass，等于把「没判」算成「合格」：既进了合格统计，
/// 界面上又显示成「通过了」。现在归 Undetermined（与「判不了」同桶，不进 pass/fail 统计）。
///
/// 人工改判成 fail 一律算**硬 fail**（人的裁决没有软硬之分）；改判成 pass 则覆盖机器 fail
/// （软硬都覆盖 —— 修正低置信度误判正是改判的目的）。
pub fn state_of_verdicts_with(rows: &[&VerdictRow], reviews: &ReviewIndex) -> AssetComplianceState {
    let Some(first) = rows.first() else {
        return AssetComplianceState::Unjudged;
    };
    if let Some(ov) = reviews.asset_override(first.asset_id) {
        return match ov.humanStatus.as_str() {
            core_storage::compliance::result::FAIL => AssetComplianceState::Fail,
            _ => AssetComplianceState::Pass,
        };
    }
    let mut saw_undetermined = false;
    let mut saw_conclusion = false;
    for r in rows {
        let ov = reviews.rule_override(r.asset_id, &r.rule_id);
        let effective = ov
            .map(|o| o.humanStatus.as_str())
            .unwrap_or(r.result.as_str());
        match effective {
            // undetermined 只是「没结论」，不能压过别的规则的硬 fail
            core_storage::compliance::result::UNDETERMINED => saw_undetermined = true,
            core_storage::compliance::result::FAIL => {
                if ov.is_some() || !is_soft_fail(r) {
                    return AssetComplianceState::Fail;
                }
                // 软 fail（on_fail=pass 的打标型规则）= 判过了、且不推翻合格
                saw_conclusion = true;
            }
            core_storage::compliance::result::PASS => saw_conclusion = true,
            // skipped = 该规则对这张图不适用：既不压结论，也不构成结论
            _ => {}
        }
    }
    if saw_undetermined || !saw_conclusion {
        // 没有任何结论（全 skipped）与「判不了」同桶：都不许算成合格
        AssetComplianceState::Undetermined
    } else {
        AssetComplianceState::Pass
    }
}

// ---------------------------------------------------------------- 一次执行的逐资产结论（L2/L3）

/// 一次执行（`compliance_runs.runId`）里单资产的结论快照 —— **查询时合成，不落库**。
///
/// 用户裁决（2026-10-31）：图库域与 job 域解耦，「当前生效结论」不再挂在资产上；
/// 任务中心 L2/L3 需要时由 **job 账本（run/task 明细）+ human_review** 现算。
/// 本结构就是那次现算的结果（`machineStatus` 来自该 run 的判定行，`effectiveStatus` 叠加人审覆盖层）。
#[derive(Debug, Clone, PartialEq)]
pub struct RunAssetStatus {
    pub assetId: i64,
    /// 该 run 的判定行汇总（纯机器口径）
    pub machineStatus: AssetComplianceState,
    /// 叠加人审后（`humanStatus ?? machine`）
    pub effectiveStatus: AssetComplianceState,
    pub machineConfidence: Option<f32>,
    /// 生效中的**资产级**人审（整张图一条裁决；规则级人审见行级明细）
    pub overrideStatus: Option<String>,
    pub overrideReason: Option<String>,
    pub overrideAt: Option<i64>,
    /// 存在活人审但资产内容已变 → 该人审不再生效（界面要如实说「为什么失效」）
    pub overrideStale: bool,
}

impl RunAssetStatus {
    fn from_rows(asset_id: i64, rows: &[&VerdictRow], reviews: &ReviewIndex, stale: bool) -> Self {
        let machine = state_of_verdicts(rows);
        let effective = state_of_verdicts_with(rows, reviews);
        let ov = reviews.asset_override(asset_id);
        Self {
            assetId: asset_id,
            machineStatus: machine,
            effectiveStatus: effective,
            machineConfidence: asset_confidence(rows),
            overrideStatus: ov.map(|o| o.humanStatus.clone()),
            overrideReason: ov.map(|o| o.reason.clone()),
            overrideAt: ov.map(|o| o.at),
            overrideStale: stale && ov.is_none(),
        }
    }
}

/// 一次执行的逐资产结论（L3 下钻/统计的唯一口径）。
///
/// 只取**逐图**判定行（组级行不参与资产状态：C9 裁决，库视图筛选 SQL 同口径）。
pub fn run_asset_statuses(
    conn: &Connection,
    pack_id: &str,
    run_id: i64,
) -> Result<Vec<RunAssetStatus>> {
    let repo = SqliteComplianceRepo::new(conn);
    let rows = repo.find_verdicts(&VerdictFilter::run(pack_id, run_id))?;
    let reviews = ReviewIndex::load(conn, pack_id)?;
    let stale = stale_review_assets(conn, pack_id)?;
    let mut by_asset: std::collections::BTreeMap<i64, Vec<&VerdictRow>> = Default::default();
    for r in rows
        .iter()
        .filter(|r| r.judgement_scope.as_deref() != Some("per_group"))
    {
        by_asset.entry(r.asset_id).or_default().push(r);
    }
    Ok(by_asset
        .into_iter()
        .map(|(asset_id, rs)| {
            RunAssetStatus::from_rows(asset_id, &rs, &reviews, stale.contains(&asset_id))
        })
        .collect())
}

/// 有活人审、但资产内容已变（人审失效）的资产集合。
pub fn stale_review_assets(
    conn: &Connection,
    pack_id: &str,
) -> Result<std::collections::BTreeSet<i64>> {
    let repo = SqliteComplianceRepo::new(conn);
    let rows = repo.find_human_reviews(&HumanReviewFilter::stale(&review_scheme(pack_id)))?;
    Ok(rows.into_iter().map(|r| r.asset_id).collect())
}

// ---------------------------------------------------------------- 人工改判（COMPLIANCE-HUMAN）

/// 判定方案身份（`human_review.schemeId`）：当前判定账本只记 packId，故落库值 = `pack:<packId>`。
///
/// 为什么不是 specId：`rule_verdict` 没有规格列（判定是引擎按包跑的），人审要**跨 job 生效**，
/// 只能挂在判定账本能解析的身份上。规格化（`spec:<id>`）只需换前缀，读路径同一条 SQL。
pub fn review_scheme(pack_id: &str) -> String {
    format!("pack:{pack_id}")
}

/// 人工改判的接口视图（`effectiveStatus = humanStatus ?? machineResult` 的右半边）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct HumanReviewView {
    pub id: i64,
    /// None = 资产级改判（整张图一条结论）
    pub ruleId: Option<String>,
    pub scope: String,
    pub humanStatus: String,
    pub reason: String,
    pub at: i64,
    pub reviewedBy: Option<String>,
    /// 改判时的资产内容版本（与资产当前内容版本不等 = 这条人审已失效，见 `ReviewIndex::load`）
    pub contentVersion: String,
    /// 改判当时的机器结论**快照**（审计对照用；当前机器结论以判定行为准）
    pub machineResult: Option<String>,
    pub machineConfidence: Option<f64>,
}

impl From<&HumanReviewRow> for HumanReviewView {
    fn from(r: &HumanReviewRow) -> Self {
        Self {
            id: r.id,
            ruleId: r.rule_id.clone(),
            scope: r.judgement_scope.clone(),
            humanStatus: r.human_status.clone(),
            reason: r.human_reason.clone(),
            at: r.human_at,
            reviewedBy: r.reviewed_by.clone(),
            contentVersion: r.content_version.clone(),
            machineResult: r.machine_result.clone(),
            machineConfidence: r.machine_confidence,
        }
    }
}

/// 生效改判索引（一份事实，多处口径共用：资产状态 / 库筛选 / 统计 / 队列 / 报告）。
///
/// 只装**活记录**（`revokedAt IS NULL`）：撤销后自然回到机器结论，调用方不必判活。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReviewIndex {
    /// assetId → 资产级改判（ruleId = NULL）
    pub asset_level: std::collections::BTreeMap<i64, HumanReviewView>,
    /// (assetId, ruleId) → 规则级改判
    pub per_rule: std::collections::BTreeMap<(i64, String), HumanReviewView>,
}

impl ReviewIndex {
    /// 从行集建索引（非活记录忽略；同键取 id 最大者 = 最新，防御性 —— 部分唯一索引本不该出现双活）。
    pub fn from_rows(rows: &[HumanReviewRow]) -> Self {
        let mut out = Self::default();
        for r in rows.iter().filter(|r| r.is_active()) {
            let view = HumanReviewView::from(r);
            match &r.rule_id {
                None => {
                    let newer = out
                        .asset_level
                        .get(&r.asset_id)
                        .map(|v| v.id < view.id)
                        .unwrap_or(true);
                    if newer {
                        out.asset_level.insert(r.asset_id, view);
                    }
                }
                Some(rule) => {
                    let key = (r.asset_id, rule.clone());
                    let newer = out
                        .per_rule
                        .get(&key)
                        .map(|v| v.id < view.id)
                        .unwrap_or(true);
                    if newer {
                        out.per_rule.insert(key, view);
                    }
                }
            }
        }
        out
    }

    /// 该方案（pack）的生效改判索引（一次查询）。
    ///
    /// **失效条件在这里落地**：`HumanReviewFilter::active` 只返回「改判时的内容版本 == 资产当前内容版本」
    /// 的活记录 —— 资产修图/重导换了内容后，旧人审自动退出索引（回到机器结论），不需要任何写路径参与。
    pub fn load(conn: &Connection, pack_id: &str) -> Result<Self> {
        let repo = SqliteComplianceRepo::new(conn);
        let rows = repo.find_human_reviews(&HumanReviewFilter::active(&review_scheme(pack_id)))?;
        Ok(Self::from_rows(&rows))
    }

    pub fn asset_override(&self, asset_id: i64) -> Option<&HumanReviewView> {
        self.asset_level.get(&asset_id)
    }

    pub fn rule_override(&self, asset_id: i64, rule_id: &str) -> Option<&HumanReviewView> {
        self.per_rule.get(&(asset_id, rule_id.to_string()))
    }

    /// 该资产是否有人工改判（资产级或任一规则级）。
    pub fn reviewed(&self, asset_id: i64) -> bool {
        self.asset_level.contains_key(&asset_id)
            || self.per_rule.keys().any(|(a, _)| *a == asset_id)
    }

    /// 该资产有改判的规则 id 集合（资产级改判另算）。
    pub fn reviewed_rules(&self, asset_id: i64) -> std::collections::BTreeSet<&str> {
        self.per_rule
            .iter()
            .filter(|((a, _), _)| *a == asset_id)
            .map(|((_, r), _)| r.as_str())
            .collect()
    }
}

/// 单条判定行的**生效结论**（`humanStatus ?? result`）。
pub fn effective_result<'a>(r: &'a VerdictRow, reviews: &'a ReviewIndex) -> &'a str {
    reviews
        .rule_override(r.asset_id, &r.rule_id)
        .map(|o| o.humanStatus.as_str())
        .unwrap_or(r.result.as_str())
}

/// 资产级置信度 = 参与汇总的规则置信度的**最小值**（三态口径里「最弱的一环」决定整体把握）。
///
/// 只统计 pass / fail / undetermined 三类行（skipped 没有结论，不参与）；历史行 confidence 为
/// NULL（0016 迁移而来，置信度不可知）→ 若一行置信度都不知道则返回 None，不造数。
pub fn asset_confidence(rows: &[&VerdictRow]) -> Option<f32> {
    rows.iter()
        .filter(|r| r.result != "skipped")
        .filter_map(|r| r.confidence)
        .map(|c| c as f32)
        .fold(None, |acc, c| Some(acc.map_or(c, |a: f32| a.min(c))))
}

/// 软 fail（on_fail=pass 的原始 fail，如打标型规则没产出标签）。
pub fn is_soft_fail(r: &VerdictRow) -> bool {
    r.evidence
        .as_deref()
        .and_then(|e| serde_json::from_str::<serde_json::Value>(e).ok())
        .and_then(|v| {
            v.get("on_fail")
                .and_then(|o| o.as_str())
                .map(|s| s == "pass")
        })
        .unwrap_or(false)
}

/// 该 pack 下每 (asset, rule) 最新一行的判定（走 repo trait，不拼 SQL）。
pub fn verdicts_latest(conn: &Connection, pack_id: &str) -> Result<Vec<VerdictRow>> {
    let repo = SqliteComplianceRepo::new(conn);
    Ok(repo.find_verdicts(&VerdictFilter::pack(pack_id))?)
}

/// 合规状态分桶（含未判定 = 全库未删除资产 − 已判定资产）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatesSummary {
    pub pass: i64,
    pub fail: i64,
    pub undetermined: i64,
    pub unjudged: i64,
}

impl StatesSummary {
    pub fn get(&self, s: AssetComplianceState) -> i64 {
        match s {
            AssetComplianceState::Pass => self.pass,
            AssetComplianceState::Fail => self.fail,
            AssetComplianceState::Undetermined => self.undetermined,
            AssetComplianceState::Unjudged => self.unjudged,
        }
    }

    pub fn total(&self) -> i64 {
        self.pass + self.fail + self.undetermined + self.unjudged
    }

    /// 已判定数（total − unjudged）。
    pub fn judged(&self) -> i64 {
        self.total() - self.unjudged
    }
}

/// 全库合规状态分桶（未判定 = 库内未删除资产 − 已判定资产；口径与库视图筛选一致）。
///
/// COMPLIANCE-HUMAN：分桶按**生效结论**（人工改判参与）—— 合规页显示的三档计数必须与
/// 库视图筛选、导出报告 summary 同源，否则「页面上不合格 3 张，点进去筛选出 4 张」。
pub fn states_summary(conn: &Connection, pack_id: &str) -> Result<StatesSummary> {
    let rows = verdicts_latest(conn, pack_id)?;
    let reviews = ReviewIndex::load(conn, pack_id)?;
    let mut by_asset: std::collections::BTreeMap<i64, Vec<&VerdictRow>> = Default::default();
    for r in &rows {
        by_asset.entry(r.asset_id).or_default().push(r);
    }
    let mut out = StatesSummary::default();
    for rs in by_asset.values() {
        match state_of_verdicts_with(rs, &reviews) {
            AssetComplianceState::Pass => out.pass += 1,
            AssetComplianceState::Fail => out.fail += 1,
            AssetComplianceState::Undetermined => out.undetermined += 1,
            AssetComplianceState::Unjudged => {}
        }
    }
    let lib_total: i64 =
        conn.query_row("SELECT COUNT(*) FROM assets WHERE deleted = 0", [], |r| {
            r.get(0)
        })?;
    out.unjudged = (lib_total - out.judged()).max(0);
    Ok(out)
}

// ---------------------------------------------------------------- 库视图筛选（SQL 口径）

/// 合规状态 → SQL 布尔条件：**实现在存储层**（`core_storage::compliance_sqlite::state_condition_sql`，
/// 方言能力只许出现在 impl 内），这里只做转发，保证调用方（svc-library 读模型）不必直接引 rusqlite 方言。
///
/// 与 `state_of_verdicts` 的同义性由 `sql_and_rust_agree_on_every_state` 逐资产比对。
pub fn state_condition(f: &ComplianceStateFilter, next_index: usize) -> (String, Vec<String>) {
    core_storage::compliance_sqlite::state_condition_sql(f, next_index)
}

/// Rust 口径下的资产 id 集合（对账/审计用；小库直接算，全库筛走 SQL）。
///
/// 与 SQL 口径（`state_condition`）同义 —— 含人工改判（生效结论）。
pub fn asset_ids_by_state(
    conn: &Connection,
    pack_id: &str,
    state: AssetComplianceState,
) -> Result<Vec<i64>> {
    let rows = verdicts_latest(conn, pack_id)?;
    let reviews = ReviewIndex::load(conn, pack_id)?;
    let mut by_asset: std::collections::BTreeMap<i64, Vec<&VerdictRow>> = Default::default();
    for r in &rows {
        by_asset.entry(r.asset_id).or_default().push(r);
    }
    let mut out: Vec<i64> = by_asset
        .iter()
        .filter(|(_, rs)| state_of_verdicts_with(rs, &reviews) == state)
        .map(|(id, _)| *id)
        .collect();
    if state == AssetComplianceState::Unjudged {
        let mut stmt = conn.prepare("SELECT assetId FROM assets WHERE deleted = 0")?;
        let all: Vec<i64> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        out = all
            .into_iter()
            .filter(|id| !by_asset.contains_key(id))
            .collect();
    }
    out.sort_unstable();
    Ok(out)
}

// ---------------------------------------------------------------- 待人工队列

/// 队列里一条规则的判定快照（+ 人工处置状态）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct QueueRule {
    pub ruleId: String,
    pub ruleName: String,
    /// 规则名的原样 i18n 对象（i18n-F；字符串形态/包没给 = None，前端用 ruleName 兜底）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ruleNameI18n: Option<serde_json::Value>,
    /// 机器结论（pass/fail/undetermined/skipped；**永不被改判改写**）
    pub result: String,
    /// 该规则置信度（NULL = 0016 前的历史行，置信度不可知）
    pub confidence: Option<f64>,
    /// 该行是否属于「需要复核」的那一类（undetermined 或 confidence < 阈值）
    pub lowConfidence: bool,
    /// **生效结论** = humanStatus ?? result —— 查询时合成（不落资产；前端直接用，不要自己算）
    pub effectiveStatus: String,
    pub score: Option<f64>,
    /// 机器给不出结论的原因（evidence.reason / fixer 缺失 / external_data 等；无则 None）
    pub reason: Option<String>,
    /// 原始证据 json（判定可复现；界面折叠显示）
    pub evidence: Option<String>,
    pub fixAction: Option<String>,
    pub fixTier: Option<String>,
    pub recheckResult: Option<String>,
    pub ts: i64,
    /// 生效中的人工改判（None = 该规则仍是机器结论）
    pub humanReview: Option<HumanReviewView>,
    /// 人工是否已处置（旧口径的误判反馈；只作统计，不改变最终结论）
    pub feedback: Option<FeedbackView>,
}

/// 反馈的界面视图（misjudged 由 machine vs human 现算，不落冗余列）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FeedbackView {
    pub id: i64,
    pub machineResult: String,
    pub humanResult: String,
    pub reason: Option<String>,
    pub misjudged: bool,
    pub ts: i64,
}

impl From<&FeedbackRow> for FeedbackView {
    fn from(f: &FeedbackRow) -> Self {
        Self {
            id: f.id,
            machineResult: f.machine_result.clone(),
            humanResult: f.human_result.clone(),
            reason: f.reason.clone(),
            misjudged: f.is_misjudged(),
            ts: f.ts,
        }
    }
}

/// 队列卡片 = 一个资产（同资产的多条待人工规则合并）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct QueueCard {
    pub assetId: i64,
    pub fileName: String,
    pub absPath: String,
    /// 缩略图（resource.role = 'primary' 的 thumbPath；没生成过为 null）
    pub thumbPath: Option<String>,
    pub missing: i64,
    pub rules: Vec<QueueRule>,
    /// 该资产所有待人工项都已处置（有反馈或有生效改判）→ 界面归入"已处置"，不必再看
    pub resolved: bool,
    /// 该资产有**生效改判**（资产级或规则级）—— 与 resolved 不同：改判才是改变结论的处置
    pub reviewed: bool,
    /// 资产级改判（整张图一条结论；界面在卡片头部显示）
    pub assetOverride: Option<HumanReviewView>,
}

/// 复核列表排序（UI 要「按置信度排序」：最没把握的排最前，先干这些）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReviewSort {
    /// 缺省：资产序（队列卡片未处置优先 + assetId 升序；扁平列表 assetId 降序）
    #[default]
    Asset,
    /// 置信度升序（NULL = 不可知，排最前；资产级行取资产置信度）
    Confidence,
}

/// 复核队列的取数选项。
///
/// 三态口径下机器永远给结论，交人工的只有两类行：`undetermined`（过程出错，没有结论）与
/// 置信度低于阈值（含历史行 confidence = NULL：不可知 ≈ 值得复核）。但**用户主动查看**时
/// 也应能改判高置信度结果 → `low_confidence_only = false` 列出该资产全部有结论的行。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueueOpts {
    /// 低置信度门槛（pack.low_confidence，缺省 0.7）：`confidence < threshold` 才需要复核
    pub threshold: f64,
    /// true（缺省）= 只列需要复核的行；false = 该资产全部有结论的行（主动复核视图）
    pub low_confidence_only: bool,
    /// Some(false) = 只看还没人工改判的；Some(true) = 只看已改判的；None = 不限
    pub reviewed: Option<bool>,
    /// 只返回尚有未处置项的卡片（兼容 C6 旧语义：无反馈且无改判）
    pub only_unresolved: bool,
    /// 排序：资产序 / 置信度升序（最没把握的排最前）
    pub sort: ReviewSort,
    pub limit: i64,
    pub offset: i64,
}

impl Default for QueueOpts {
    fn default() -> Self {
        Self {
            threshold: 0.7,
            low_confidence_only: true,
            reviewed: None,
            only_unresolved: false,
            sort: ReviewSort::Asset,
            limit: 200,
            offset: 0,
        }
    }
}

/// 低置信度复核队列（**替代原来的「待人工」队列**）。
///
/// 排序：未处置优先，然后 assetId 升序（稳定）。
/// 每行自带 `effectiveStatus` 与 `humanReview`，前端不重算口径。
pub fn low_confidence_queue(
    conn: &Connection,
    pack_id: &str,
    pack: &Pack,
    opts: &QueueOpts,
    locale: &str,
) -> Result<(i64, Vec<QueueCard>)> {
    let rows = verdicts_latest(conn, pack_id)?;
    let feedback = feedback_index(conn, pack_id)?;
    let reviews = ReviewIndex::load(conn, pack_id)?;
    // 需要人工复核的行：undetermined（没结论）+ 置信度低于阈值（含 NULL=不可知）
    let mut by_asset: std::collections::BTreeMap<i64, Vec<&VerdictRow>> = Default::default();
    for r in &rows {
        let low = r.result == core_storage::compliance::result::UNDETERMINED
            || (r.result != core_storage::compliance::result::SKIPPED
                && r.confidence.map(|c| c < opts.threshold).unwrap_or(true));
        let include = if opts.low_confidence_only {
            low
        } else {
            // 主动复核视图：全部有结论的行（skipped 没有结论，改了也没意义）
            r.result != core_storage::compliance::result::SKIPPED
        };
        if include {
            by_asset.entry(r.asset_id).or_default().push(r);
        }
    }
    let mut cards: Vec<QueueCard> = Vec::new();
    for (asset_id, rs) in by_asset {
        let reviewed = reviews.reviewed(asset_id);
        // 复核过滤在分页前做：否则「只看未复核」会翻出整页空卡片
        if let Some(want) = opts.reviewed {
            if want != reviewed {
                continue;
            }
        }
        let (file_name, abs_path, missing, thumb_path) = asset_identity(conn, asset_id)?;
        let rules: Vec<QueueRule> = rs
            .iter()
            .map(|r| {
                let fb = feedback.get(&(r.asset_id, r.rule_id.clone()));
                let human = reviews.rule_override(r.asset_id, &r.rule_id).cloned();
                let low = r.result == core_storage::compliance::result::UNDETERMINED
                    || (r.result != core_storage::compliance::result::SKIPPED
                        && r.confidence.map(|c| c < opts.threshold).unwrap_or(true));
                QueueRule {
                    ruleId: r.rule_id.clone(),
                    ruleName: rule_name(pack, &r.rule_id, locale),
                    ruleNameI18n: rule_name_i18n(pack, &r.rule_id),
                    result: r.result.clone(),
                    confidence: r.confidence,
                    lowConfidence: low,
                    effectiveStatus: effective_result(r, &reviews).to_string(),
                    score: r.score,
                    reason: evidence_reason(r.evidence.as_deref()),
                    evidence: r.evidence.clone(),
                    fixAction: r.fix_action.clone(),
                    fixTier: r.fix_tier.clone(),
                    recheckResult: r.recheck_result.clone(),
                    ts: r.ts,
                    humanReview: human,
                    feedback: fb.cloned(),
                }
            })
            .collect();
        let resolved = rules
            .iter()
            .all(|r| r.feedback.is_some() || r.humanReview.is_some());
        if opts.only_unresolved && resolved {
            continue;
        }
        cards.push(QueueCard {
            assetId: asset_id,
            fileName: file_name,
            absPath: abs_path,
            thumbPath: thumb_path,
            missing,
            rules,
            resolved,
            reviewed,
            assetOverride: reviews.asset_override(asset_id).cloned(),
        });
    }
    // 排序：未处置优先；同组内按 置信度升序（最没把握的先看）或资产序
    match opts.sort {
        ReviewSort::Asset => cards.sort_by_key(|c| (c.resolved, c.assetId)),
        // 置信度排序是**用户显式要求**的顺序：不再按「已处置」分组（否则刚改判过的低置信度
        // 卡片会被排到高置信度未处置的后面，"最没把握的先看"就失效了）
        ReviewSort::Confidence => cards.sort_by(|a, b| {
            let key = |c: &QueueCard| -> f64 {
                c.rules
                    .iter()
                    .filter_map(|r| r.confidence)
                    .fold(f64::INFINITY, f64::min)
                    .clamp(0.0, 1.0)
            };
            key(a)
                .partial_cmp(&key(b))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.assetId.cmp(&b.assetId))
        }),
    }
    let total = cards.len() as i64;
    let offset = opts.offset.max(0) as usize;
    let limit = opts.limit.clamp(1, 1000) as usize;
    Ok((total, cards.into_iter().skip(offset).take(limit).collect()))
}

/// (asset, rule) → 最新反馈（同键多条取 id 最大者）。
fn feedback_index(
    conn: &Connection,
    pack_id: &str,
) -> Result<std::collections::BTreeMap<(i64, String), FeedbackView>> {
    let repo = SqliteComplianceRepo::new(conn);
    let rows = repo.find_feedback(&FeedbackFilter::pack(pack_id))?;
    let mut out: std::collections::BTreeMap<(i64, String), FeedbackView> = Default::default();
    for f in &rows {
        let key = (f.asset_id, f.rule_id.clone());
        let newer = out.get(&key).map(|v| v.id < f.id).unwrap_or(true);
        if newer {
            out.insert(key, FeedbackView::from(f));
        }
    }
    Ok(out)
}

/// (fileName, absPath, missing, thumbPath) —— 复核视图要缩略图，这里一次取全，
/// 不让前端为每张卡片再打一次接口（N+1）。
fn asset_identity(
    conn: &Connection,
    asset_id: i64,
) -> Result<(String, String, i64, Option<String>)> {
    Ok(conn.query_row(
        "SELECT a.fileName, a.absPath, a.missing, r.thumbPath
           FROM assets a
           LEFT JOIN resource r ON r.assetId = a.assetId AND r.role = 'primary'
          WHERE a.assetId = ?1",
        [asset_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?)
}

fn rule_name(pack: &Pack, rule_id: &str, locale: &str) -> String {
    pack.rule(rule_id)
        .map(|r| {
            let picked = r.name.pick(locale);
            if picked.trim().is_empty() {
                r.id.clone()
            } else {
                picked.to_string()
            }
        })
        .unwrap_or_else(|| rule_id.to_string())
}

/// 规则名的原样 i18n 对象（i18n-F：`{"locale": 文案}` 时透传给前端，切语言不重拉；
/// 字符串形态/规则不存在 = `None`）。
fn rule_name_i18n(pack: &Pack, rule_id: &str) -> Option<serde_json::Value> {
    let r = pack.rule(rule_id)?;
    r.name.as_map()?;
    serde_json::to_value(&r.name).ok()
}

/// 证据里的 `reason`（机器为何转人工：guaranteed_fix_failed / external_data / fixer_not_available …）。
pub fn evidence_reason(evidence: Option<&str>) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(evidence?).ok()?;
    v.get("reason")
        .and_then(|r| r.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            // 修复后复检仍败：把复检原因抬上来（reason 就在 recheck 里）
            v.get("recheck")
                .and_then(|rc| rc.get("reason"))
                .and_then(|r| r.as_str())
                .map(|s| s.to_string())
        })
}

// ---------------------------------------------------------------- 反馈读写

/// 写一条人工反馈（「误判」标记）。返回落库行。
///
/// 校验（全部响亮报错，不静默）：
/// * pack 里必须有该规则；资产必须存在；
/// * 该 (asset, rule) 必须已有机器判定（machineResult 由库里判定行推导 —— 前端不能自报机器结论）；
/// * humanResult ∈ pass | fail；
/// * 人工与机器结论不一致时自动补 reason（`误判：机器 X → 人工 Y`），前端给了 reason 则以其为准。
pub fn record_feedback(
    conn: &Connection,
    pack_id: &str,
    pack: &Pack,
    asset_id: i64,
    rule_id: &str,
    human_result: &str,
    reason: Option<&str>,
) -> Result<FeedbackRow> {
    if !matches!(human_result, "pass" | "fail") {
        return Err(LibraryError::Invalid(format!(
            "humanResult 只能是 pass|fail，收到 {human_result:?}"
        )));
    }
    if pack.rule(rule_id).is_none() {
        return Err(LibraryError::Invalid(format!(
            "pack {pack_id} 里没有规则 {rule_id}"
        )));
    }
    let machine = rule_machine_verdict(conn, pack_id, asset_id, rule_id)?;
    let Some(machine) = machine else {
        return Err(LibraryError::Invalid(format!(
            "资产 {asset_id} 在 pack {pack_id} 下没有规则 {rule_id} 的机器判定 —— 先跑判定再复核"
        )));
    };
    let auto_reason =
        if matches!(machine.result.as_str(), "pass" | "fail") && machine.result != human_result {
            Some(format!(
                "误判：机器 {} → 人工 {}",
                machine.result, human_result
            ))
        } else if machine.result == "undetermined" {
            Some(format!("人工复核转人工项 → {human_result}"))
        } else {
            None
        };
    let note = match reason.map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) => Some(r.to_string()),
        None => auto_reason,
    };
    let repo = SqliteComplianceRepo::new(conn);
    let id = repo.insert_feedback(&NewFeedback {
        pack_id: pack_id.to_string(),
        asset_id,
        rule_id: rule_id.to_string(),
        machine_result: machine.result.clone(),
        human_result: human_result.to_string(),
        reason: note.clone(),
    })?;
    let ts: i64 = conn.query_row("SELECT ts FROM rule_feedback WHERE id = ?1", [id], |r| {
        r.get(0)
    })?;
    Ok(FeedbackRow {
        id,
        pack_id: pack_id.to_string(),
        asset_id,
        rule_id: rule_id.to_string(),
        machine_result: machine.result,
        human_result: human_result.to_string(),
        reason: note,
        ts,
    })
}

/// 该 (asset, rule) 的最新机器判定（None = 没判过）。
fn rule_machine_verdict(
    conn: &Connection,
    pack_id: &str,
    asset_id: i64,
    rule_id: &str,
) -> Result<Option<VerdictRow>> {
    let rows = verdicts_latest(conn, pack_id)?;
    Ok(rows
        .into_iter()
        .filter(|r| r.asset_id == asset_id && r.rule_id == rule_id)
        .max_by_key(|r| r.id))
}

/// 反馈列表（asset/rule 可选收窄）。
pub fn list_feedback(
    conn: &Connection,
    pack_id: &str,
    asset_id: Option<i64>,
    rule_id: Option<&str>,
    limit: Option<i64>,
) -> Result<Vec<FeedbackRow>> {
    let repo = SqliteComplianceRepo::new(conn);
    let mut f = match asset_id {
        Some(a) => FeedbackFilter::asset(pack_id, a),
        None => FeedbackFilter::pack(pack_id),
    };
    f.rule_id = rule_id.map(|s| s.to_string());
    f.limit = limit;
    Ok(repo.find_feedback(&f)?)
}

/// 反馈条数（状态栏）。
pub fn feedback_count(conn: &Connection, pack_id: &str) -> Result<i64> {
    let repo = SqliteComplianceRepo::new(conn);
    Ok(repo.count_feedback(pack_id)?)
}

/// 反馈 CSV（审计/离线校准用；列固定，表头英文以利下游工具解析）。
pub fn feedback_csv(rows: &[FeedbackRow]) -> String {
    let mut out =
        String::from("packId,assetId,ruleId,machineResult,humanResult,misjudged,reason,ts\n");
    for f in rows {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{}\n",
            csv_field(&f.pack_id),
            f.asset_id,
            csv_field(&f.rule_id),
            csv_field(&f.machine_result),
            csv_field(&f.human_result),
            if f.is_misjudged() { 1 } else { 0 },
            csv_field(f.reason.as_deref().unwrap_or("")),
            f.ts,
        ));
    }
    out
}

// ---------------------------------------------------------------- 人工改判读写（COMPLIANCE-HUMAN）

/// 改判被拒的稳定错误码（前端按码定位输入框/给文案；**不要**在前端比中文串）。
pub mod review_code {
    /// reason 为空（含纯空白）
    pub const REASON_REQUIRED: &str = "review_reason_required";
    /// humanStatus 不是 pass|fail
    pub const BAD_STATUS: &str = "review_bad_status";
    /// pack 里没有这条规则
    pub const UNKNOWN_RULE: &str = "review_unknown_rule";
    /// 该 (asset, rule) 没有机器判定 —— 改判必须建立在机器结论之上
    pub const NO_MACHINE_VERDICT: &str = "review_no_machine_verdict";
    /// 不是低置信度结果且没传 force=true
    pub const NOT_LOW_CONFIDENCE: &str = "review_not_low_confidence";
    /// 撤销时没有生效中的改判
    pub const NOT_FOUND: &str = "review_not_found";
}

/// 改判被拒（校验失败）。
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewReject {
    pub code: &'static str,
    pub message: String,
}

/// 改判失败：`Reject` → HTTP 400（带错误码），`Store` → 500（库/存储错误）。
#[derive(Debug)]
pub enum ReviewError {
    Reject(ReviewReject),
    Store(LibraryError),
}

impl ReviewError {
    fn reject(code: &'static str, message: impl Into<String>) -> Self {
        Self::Reject(ReviewReject {
            code,
            message: message.into(),
        })
    }

    /// 稳定错误码（Store 类错误无码 → None，HTTP 层回 500）。
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::Reject(r) => Some(r.code),
            Self::Store(_) => None,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Reject(r) => r.message.clone(),
            Self::Store(e) => e.to_string(),
        }
    }
}

impl std::fmt::Display for ReviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reject(r) => write!(f, "{}: {}", r.code, r.message),
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

impl From<LibraryError> for ReviewError {
    fn from(e: LibraryError) -> Self {
        Self::Store(e)
    }
}

impl From<rusqlite::Error> for ReviewError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(LibraryError::Sqlite(e))
    }
}

impl From<core_storage::StorageError> for ReviewError {
    fn from(e: core_storage::StorageError) -> Self {
        Self::Store(LibraryError::Storage(e))
    }
}

/// 改判请求（HTTP body 直投）。
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewRequest<'a> {
    pub asset_id: i64,
    /// None = **资产级整体结论**（整张图一条裁决；UI 的「整张图改判」入口）
    pub rule_id: Option<&'a str>,
    /// pass | fail
    pub human_status: &'a str,
    /// **必填**（trim 后不能为空）
    pub reason: &'a str,
    pub reviewed_by: Option<&'a str>,
    /// 显式确认要改判「非低置信度」结果（否则拒绝 —— 防手滑改掉高把握结论）
    pub force: bool,
    /// 低置信度门槛（pack.low_confidence，缺省 0.7）
    pub threshold: f64,
}

/// 写一条人工改判（覆盖语义）。
///
/// 校验（全部响亮报错，带错误码）：reason 必填、humanStatus ∈ pass|fail、规则必须在 pack 里、
/// 该 (asset, rule) 必须已有机器判定（机器结论由库里判定行推导 + 快照存档，前端不能自报）、
/// **非低置信度结果必须 `force = true`**（`confidence < threshold` 或 undetermined 才免 force）。
///
/// 为什么「非低置信度」要拦一道：改判直接改变合规结论（不合格 → 合格可直接放行一张图），
/// 高置信度结果被误改的代价远大于多一次确认的成本；而低置信度/undetermined 本来就是
/// 机器「没把握」的区间，改判是设计内的常规动作。
pub fn record_human_review(
    conn: &Connection,
    pack_id: &str,
    pack: &Pack,
    req: &ReviewRequest<'_>,
) -> std::result::Result<HumanReviewRow, ReviewError> {
    let human_status = req.human_status.trim();
    if !matches!(human_status, "pass" | "fail") {
        return Err(ReviewError::reject(
            review_code::BAD_STATUS,
            format!("humanStatus 只能是 pass|fail，收到 {:?}", req.human_status),
        ));
    }
    let reason = req.reason.trim();
    if reason.is_empty() {
        return Err(ReviewError::reject(
            review_code::REASON_REQUIRED,
            "改判必须填写理由（reason）：请写清为什么机器结论不成立，便于后续复盘",
        ));
    }
    // 资产必须存在（否则落库会撞外键，报错信息对用户没意义）
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM assets WHERE assetId = ?1",
        [req.asset_id],
        |r| r.get(0),
    )?;
    if exists == 0 {
        return Err(ReviewError::reject(
            review_code::NO_MACHINE_VERDICT,
            format!("资产 {} 不存在，无法改判", req.asset_id),
        ));
    }
    let rows_all = verdicts_latest(conn, pack_id)?;
    let asset_rows: Vec<&VerdictRow> = rows_all
        .iter()
        .filter(|r| r.asset_id == req.asset_id)
        .collect();
    if asset_rows.is_empty() {
        return Err(ReviewError::reject(
            review_code::NO_MACHINE_VERDICT,
            format!(
                "资产 {} 在 pack {pack_id} 下还没有任何机器判定 —— 先跑判定再改判",
                req.asset_id
            ),
        ));
    }

    let (rule_id, scope, group_key, machine_result, machine_confidence, needs_force) =
        match req.rule_id.map(str::trim).filter(|r| !r.is_empty()) {
            Some(rule_id) => {
                if pack.rule(rule_id).is_none() {
                    return Err(ReviewError::reject(
                        review_code::UNKNOWN_RULE,
                        format!("pack {pack_id} 里没有规则 {rule_id}"),
                    ));
                }
                let row = asset_rows
                    .iter()
                    .filter(|r| r.rule_id == rule_id)
                    .max_by_key(|r| r.id)
                    .copied()
                    .ok_or_else(|| {
                        ReviewError::reject(
                            review_code::NO_MACHINE_VERDICT,
                            format!(
                            "资产 {} 在规则 {rule_id} 下没有机器判定 —— 改判必须建立在机器结论之上",
                            req.asset_id
                        ),
                        )
                    })?;
                // undetermined 免 force：机器本来就没结论，改判就是补结论
                let needs_force = row.result != core_storage::compliance::result::UNDETERMINED
                    && row.confidence.map(|c| c >= req.threshold).unwrap_or(false);
                (
                    Some(rule_id.to_string()),
                    row.judgement_scope
                        .clone()
                        .unwrap_or_else(|| "per_asset".to_string()),
                    row.group_key.clone().unwrap_or_default(),
                    Some(row.result.clone()),
                    row.confidence,
                    needs_force,
                )
            }
            // 资产级：整张图一条裁决（机器侧快照 = 汇总状态 + 资产置信度）
            None => {
                let state = state_of_verdicts(&asset_rows);
                if state == AssetComplianceState::Unjudged {
                    return Err(ReviewError::reject(
                        review_code::NO_MACHINE_VERDICT,
                        format!(
                            "资产 {} 在 pack {pack_id} 下还没有任何机器判定 —— 先跑判定再改判",
                            req.asset_id
                        ),
                    ));
                }
                let has_undetermined = asset_rows
                    .iter()
                    .any(|r| r.result == core_storage::compliance::result::UNDETERMINED);
                let conf = asset_confidence(&asset_rows).map(|c| c as f64);
                let needs_force = !has_undetermined
                    && state != AssetComplianceState::Undetermined
                    && conf.map(|c| c >= req.threshold).unwrap_or(false);
                (
                    None,
                    "per_asset".to_string(),
                    String::new(),
                    Some(state.as_str().to_string()),
                    conf,
                    needs_force,
                )
            }
        };
    if needs_force && !req.force {
        return Err(ReviewError::reject(
            review_code::NOT_LOW_CONFIDENCE,
            format!(
                "该结论置信度 {} ≥ 阈值 {:.2}，不属于低置信度结果；确需改判请显式确认（force=true）",
                machine_confidence
                    .map(|c| format!("{c:.2}"))
                    .unwrap_or_else(|| "未知".to_string()),
                req.threshold
            ),
        ));
    }

    let repo = SqliteComplianceRepo::new(conn);
    // 内容版本 = 改判这一刻的资产内容（工作版本 sha256 ?? 导入 fileHash ?? ''）。
    // 资产之后换了内容，这条人审就自动失效（读侧过滤，不删行）。
    let content_version =
        core_storage::compliance_sqlite::asset_content_version(conn, req.asset_id)?;
    let id = repo.save_human_review(&NewHumanReview {
        scheme_id: review_scheme(pack_id),
        asset_id: req.asset_id,
        rule_id: rule_id.clone(),
        judgement_scope: scope,
        group_key,
        human_status: human_status.to_string(),
        human_reason: reason.to_string(),
        machine_result,
        machine_confidence,
        reviewed_by: req
            .reviewed_by
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
        content_version,
    })?;
    let mut f = HumanReviewFilter::asset(&review_scheme(pack_id), req.asset_id);
    f.rule_id = Some(rule_id);
    let saved = repo.find_human_reviews(&f)?;
    saved.into_iter().find(|r| r.id == id).ok_or_else(|| {
        ReviewError::Store(LibraryError::Invalid(format!("改判 id {id} 落库后查不到")))
    })
}

/// 撤销生效中的人工改判（回到机器结论）。返回撤销行数（0 → `review_not_found`）。
///
/// 不删行、只落 `revokedAt`：改判历史要可追（谁在什么时候撤销、为什么）。
pub fn revoke_human_review(
    conn: &Connection,
    pack_id: &str,
    asset_id: i64,
    rule_id: Option<&str>,
    reason: Option<&str>,
) -> std::result::Result<usize, ReviewError> {
    let rule_id = rule_id.map(str::trim).filter(|r| !r.is_empty());
    let repo = SqliteComplianceRepo::new(conn);
    let scheme = review_scheme(pack_id);
    let n = repo.revoke_human_review(
        &scheme,
        asset_id,
        rule_id,
        reason.map(str::trim).filter(|r| !r.is_empty()),
    )?;
    if n == 0 {
        return Err(ReviewError::reject(
            review_code::NOT_FOUND,
            match rule_id {
                Some(r) => format!("资产 {asset_id} 的规则 {r} 没有生效中的人工改判（可能已撤销）"),
                None => format!("资产 {asset_id} 没有生效中的资产级人工改判（可能已撤销）"),
            },
        ));
    }
    Ok(n)
}

/// 复核列表的一行（**一个 (asset, rule)**；资产级改判的 ruleId = None）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ReviewItem {
    pub assetId: i64,
    pub fileName: String,
    pub absPath: String,
    /// 缩略图（resource.role = 'primary' 的 thumbPath；没生成过为 null）
    pub thumbPath: Option<String>,
    /// None = 资产级（整张图一条结论）
    pub ruleId: Option<String>,
    pub ruleName: Option<String>,
    /// 规则名的原样 i18n 对象（i18n-F；同 QueueRule 口径）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ruleNameI18n: Option<serde_json::Value>,
    pub judgementScope: Option<String>,
    /// 机器结论（pass/fail/undetermined/skipped；资产级 = 机器汇总状态词）
    pub machineResult: String,
    pub machineConfidence: Option<f64>,
    pub machineScore: Option<f64>,
    /// 机器为何得不出结论（undetermined 的原因；合格/不合格为 None）
    pub reason: Option<String>,
    /// 是否属于「需要复核」（undetermined 或 confidence < threshold）
    pub lowConfidence: bool,
    /// **生效结论** = humanStatus ?? machineResult（查询时合成，不落资产）
    pub effectiveStatus: String,
    /// 生效中的人工改判（None = 还是机器结论）
    pub humanReview: Option<HumanReviewView>,
    pub reviewed: bool,
    pub ts: i64,
}

/// `list_reviews` 的取数选项（HTTP query 直投）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReviewQuery {
    pub asset_id: Option<i64>,
    pub low_confidence_only: bool,
    pub threshold: f64,
    /// Some(false) = 只看还没人工改判的；Some(true) = 只看已改判的；None = 不限
    pub reviewed: Option<bool>,
    /// 排序：资产序 / 置信度升序
    pub sort: ReviewSort,
    pub limit: i64,
    pub offset: i64,
}

impl Default for ReviewQuery {
    fn default() -> Self {
        Self {
            asset_id: None,
            low_confidence_only: true,
            threshold: 0.7,
            reviewed: None,
            sort: ReviewSort::Asset,
            limit: 200,
            offset: 0,
        }
    }
}

/// 复核列表（扁平行，一个 (asset, rule) 一行 + 资产级改判单独一行）。
///
/// 与 `low_confidence_queue` 的差别：队列按资产聚合（卡片），这里按规则铺平（表格/审计），
/// 两者取数口径同源（同一 `ReviewIndex` + 同一阈值判定），只是展示形态不同。
pub fn list_reviews(
    conn: &Connection,
    pack_id: &str,
    pack: &Pack,
    q: &ReviewQuery,
    locale: &str,
) -> Result<(i64, Vec<ReviewItem>)> {
    let rows = verdicts_latest(conn, pack_id)?;
    let reviews = ReviewIndex::load(conn, pack_id)?;
    let mut by_asset: std::collections::BTreeMap<i64, Vec<&VerdictRow>> = Default::default();
    for r in &rows {
        if let Some(a) = q.asset_id {
            if r.asset_id != a {
                continue;
            }
        }
        by_asset.entry(r.asset_id).or_default().push(r);
    }
    if let Some(a) = q.asset_id {
        by_asset.entry(a).or_default();
    }
    let mut items: Vec<ReviewItem> = Vec::new();
    for (asset_id, rs) in &by_asset {
        let (file_name, abs_path, thumb_path) = if rs.is_empty() {
            match asset_identity(conn, *asset_id) {
                Ok((f, a, _, t)) => (f, a, t),
                // 资产不存在 → 跳过（不是列表里的噪音）
                Err(_) => continue,
            }
        } else {
            let (f, a, _, t) = asset_identity(conn, *asset_id)?;
            (f, a, t)
        };
        // 资产级改判（整张图一条结论）
        if let Some(ov) = reviews.asset_override(*asset_id) {
            let state = state_of_verdicts(rs);
            let machine_result = state.as_str().to_string();
            let asset_low = asset_confidence(rs)
                .map(|c| (c as f64) < q.threshold)
                .unwrap_or(true);
            // 低置信度过滤对资产级行同样生效（筛选口径统一，否则「只看需要复核的」会混进
            // 高置信度资产的整图改判行）
            let asset_low = asset_low
                || rs
                    .iter()
                    .any(|r| r.result == core_storage::compliance::result::UNDETERMINED);
            if q.reviewed != Some(false) && (!q.low_confidence_only || asset_low) {
                items.push(ReviewItem {
                    assetId: *asset_id,
                    fileName: file_name.clone(),
                    absPath: abs_path.clone(),
                    thumbPath: thumb_path.clone(),
                    ruleId: None,
                    ruleName: None,
                    ruleNameI18n: None,
                    judgementScope: Some("per_asset".to_string()),
                    machineResult: machine_result.clone(),
                    machineConfidence: asset_confidence(rs).map(|c| c as f64),
                    machineScore: None,
                    reason: None,
                    lowConfidence: asset_low,
                    effectiveStatus: ov.humanStatus.clone(),
                    humanReview: Some(ov.clone()),
                    reviewed: true,
                    ts: ov.at,
                });
            }
        }
        for r in rs {
            let human = reviews.rule_override(r.asset_id, &r.rule_id).cloned();
            let low = r.result == core_storage::compliance::result::UNDETERMINED
                || (r.result != core_storage::compliance::result::SKIPPED
                    && r.confidence.map(|c| c < q.threshold).unwrap_or(true));
            if q.low_confidence_only && !low {
                continue;
            }
            if !q.low_confidence_only && r.result == core_storage::compliance::result::SKIPPED {
                continue;
            }
            if let Some(want) = q.reviewed {
                if want != human.is_some() {
                    continue;
                }
            }
            items.push(ReviewItem {
                assetId: *asset_id,
                fileName: file_name.clone(),
                absPath: abs_path.clone(),
                thumbPath: thumb_path.clone(),
                ruleId: Some(r.rule_id.clone()),
                ruleName: Some(rule_name(pack, &r.rule_id, locale)),
                ruleNameI18n: rule_name_i18n(pack, &r.rule_id),
                judgementScope: r.judgement_scope.clone(),
                machineResult: r.result.clone(),
                machineConfidence: r.confidence,
                machineScore: r.score,
                reason: evidence_reason(r.evidence.as_deref()),
                lowConfidence: low,
                effectiveStatus: effective_result(r, &reviews).to_string(),
                reviewed: human.is_some(),
                humanReview: human,
                ts: r.ts,
            });
        }
    }
    match q.sort {
        ReviewSort::Asset => items.sort_by(|a, b| {
            b.assetId
                .cmp(&a.assetId)
                .then(a.ruleId.is_some().cmp(&b.ruleId.is_some()).reverse())
                .then(a.ruleId.cmp(&b.ruleId))
        }),
        // 置信度升序：最没把握的排最前（NULL = 不可知，排最前 → 视为 -1.0）
        ReviewSort::Confidence => items.sort_by(|a, b| {
            let key = |i: &ReviewItem| i.machineConfidence.unwrap_or(-1.0);
            key(a)
                .partial_cmp(&key(b))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.assetId.cmp(&a.assetId))
                .then(a.ruleId.cmp(&b.ruleId))
        }),
    }
    let total = items.len() as i64;
    let offset = q.offset.max(0) as usize;
    let limit = q.limit.clamp(1, 1000) as usize;
    Ok((total, items.into_iter().skip(offset).take(limit).collect()))
}

/// 人工改判历史（含撤销/被覆盖的行；审计 & 「改判前后对照」用）。
pub fn review_history(
    conn: &Connection,
    pack_id: &str,
    asset_id: Option<i64>,
    limit: Option<i64>,
) -> Result<Vec<HumanReviewRow>> {
    let repo = SqliteComplianceRepo::new(conn);
    let mut f = HumanReviewFilter::history(&review_scheme(pack_id));
    f.asset_id = asset_id;
    // 历史：连撤销/被覆盖/被内容变更顶掉的一起给（`history()` 已放开两个过滤位）
    f.limit = limit;
    Ok(repo.find_human_reviews(&f)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_storage::compliance::{ComplianceRepo, NewVerdict};

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&c).unwrap();
        c
    }

    fn asset(c: &Connection, id: i64, name: &str) {
        c.execute(
            "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (?1, ?2, 'image', ?3, 'jpg', 10, 0, 0)",
            rusqlite::params![id, format!("/x/{name}"), name],
        )
        .unwrap();
    }

    fn verdict(
        c: &Connection,
        asset_id: i64,
        rule: &str,
        result: &str,
        fix_action: Option<&str>,
        evidence: Option<&str>,
    ) {
        let repo = SqliteComplianceRepo::new(c);
        repo.insert_verdict(&NewVerdict {
            pack_id: "mini".into(),
            asset_id,
            job_id: 1,
            rule_id: rule.into(),
            result: result.into(),
            confidence: 0.9,
            score: Some(0.5),
            evidence: evidence.map(|e| e.to_string()),
            fix_action: fix_action.map(|s| s.to_string()),
            fix_tier: None,
            recheck_result: None,
            judgement_scope: Some("per_asset".into()),
            group_key: None,
        })
        .unwrap();
    }

    fn fixture() -> Connection {
        // 五档各一资产：1 合格 / 2 修复后合格 / 3 不合格 / 4 待人工 / 5 未判定
        // 6 = 软 fail（on_fail=pass）+ 硬 pass → 合格
        // 7 = fail 且 evidence 为 NULL（三值逻辑坑：NULL 不能把硬 fail 吞掉）
        // 8 = fail 且 evidence 是坏 json（同样必须算硬 fail）
        let c = db();
        for (id, name) in [
            (1, "a.jpg"),
            (2, "b.jpg"),
            (3, "c.jpg"),
            (4, "d.jpg"),
            (5, "e.jpg"),
            (6, "f.jpg"),
            (7, "g.jpg"),
            (8, "h.jpg"),
        ] {
            asset(&c, id, name);
        }
        verdict(&c, 1, "R1", "pass", None, None);
        verdict(&c, 2, "R1", "pass", Some("upscale"), None);
        verdict(
            &c,
            3,
            "R1",
            "fail",
            None,
            Some(r#"{"threshold":700,"ratio":0.5}"#),
        );
        // 修复后复检仍败在三态口径下就是 fail（这里用 undetermined 表示「执行过程出错」的场景）
        verdict(
            &c,
            4,
            "R1",
            "undetermined",
            None,
            Some(r#"{"reason":"vl_timeout"}"#),
        );
        verdict(&c, 4, "R2", "pass", None, None);
        verdict(
            &c,
            6,
            "R2",
            "fail",
            None,
            Some(r#"{"on_fail":"pass","detector":{"tags":[]}}"#),
        );
        verdict(&c, 6, "R1", "pass", None, None);
        verdict(&c, 7, "R1", "fail", None, None);
        verdict(&c, 8, "R1", "fail", None, Some("{not json"));
        c
    }

    fn pack() -> Pack {
        Pack::from_yaml_strs(
            "mini",
            "category: [连衣裙]\n",
            r#"
- id: R1
  name: 尺寸达标
  executor: classic
  check: {detector: size_min}
  on_fail: retry_then_human
- id: R2
  name: 白底
  executor: classic
  check: {detector: white_edge}
  on_fail: pass
"#,
            "steps:\n  - {kind: judge, rule: R1}\n  - {kind: judge, rule: R2}\n",
            "summary: [合格, 不合格, 未判定]\ndetail_columns: [imageSN, R1]\n",
        )
        .unwrap()
    }

    #[test]
    fn states_summary_covers_three_states() {
        let c = fixture();
        let s = states_summary(&c, "mini").unwrap();
        // 三态口径：pass 1/2/6（2 有 fixAction 也只是 pass，不再单列「修复后合格」；
        // 6 的软 fail 不影响结论）；fail 3/7/8；undetermined 4；5 未跑
        assert_eq!(s.pass, 3);
        assert_eq!(s.fail, 3, "资产 3 + 7（evidence NULL）+ 8（坏 json）");
        assert_eq!(s.undetermined, 1);
        assert_eq!(s.unjudged, 1);
        assert_eq!(s.total(), 8, "未判定 = 全库 − 已判定");
        assert_eq!(s.judged(), 7);
    }

    #[test]
    fn state_of_verdicts_priority_and_soft_fail() {
        assert_eq!(state_of_verdicts(&[]), AssetComplianceState::Unjudged);
        let c = fixture();
        let rows = verdicts_latest(&c, "mini").unwrap();
        let of = |asset_id: i64| {
            let rs: Vec<&VerdictRow> = rows.iter().filter(|r| r.asset_id == asset_id).collect();
            state_of_verdicts(&rs)
        };
        assert_eq!(of(1), AssetComplianceState::Pass);
        assert_eq!(
            of(2),
            AssetComplianceState::Pass,
            "修复成功 = pass（不再有「修复后合格」档）"
        );
        assert_eq!(of(3), AssetComplianceState::Fail);
        assert_eq!(
            of(4),
            AssetComplianceState::Undetermined,
            "同一资产 R2 pass 也压不过 undetermined"
        );
        assert_eq!(of(5), AssetComplianceState::Unjudged);
        assert_eq!(of(6), AssetComplianceState::Pass, "软 fail 不影响合格结论");
        assert_eq!(
            of(7),
            AssetComplianceState::Fail,
            "evidence NULL 仍是硬 fail"
        );
        assert_eq!(
            of(8),
            AssetComplianceState::Fail,
            "坏 json 证据按硬 fail（保守）"
        );
        // 口径词与报告档位映射（三档）
        assert_eq!(AssetComplianceState::Pass.as_str(), "pass");
        assert_eq!(AssetComplianceState::Fail.as_str(), "fail");
        assert_eq!(AssetComplianceState::Undetermined.as_str(), "undetermined");
        assert_eq!(AssetComplianceState::Undetermined.report_index(), Some(2));
        assert_eq!(AssetComplianceState::Fail.report_index(), Some(1));
        assert_eq!(AssetComplianceState::Pass.report_index(), Some(0));
        assert_eq!(AssetComplianceState::Unjudged.report_index(), None);
        // 旧词只作输入兼容（前端残留 ?compliance=pending_human → 归到未判定），不再被输出
        assert_eq!(
            AssetComplianceState::parse("pending_human"),
            Some(AssetComplianceState::Undetermined)
        );
    }

    /// 三态汇总**真值表**（含组级传导 / 多规则混合 / undetermined 低于 fail 的优先级 /
    /// 资产置信度取最小值 / skipped 不参与）。表驱动，一条断言一行场景。
    #[test]
    fn tristate_aggregation_truth_table() {
        use core_storage::compliance::result::{FAIL, PASS, SKIPPED, UNDETERMINED};
        let row = |result: &str, scope: &str, fix: Option<&str>, conf: Option<f64>| VerdictRow {
            id: 0,
            pack_id: "mini".into(),
            asset_id: 1,
            job_id: None,
            rule_id: "R".into(),
            result: result.into(),
            // N5b：生效判定词由 (result, verdict) 投影而来；测试里直接把判定词当结论，
            // exec_result 只用来表达「跑成了没有」
            exec_result: if result == SKIPPED {
                "skipped".into()
            } else {
                "ok".into()
            },
            verdict: if result == SKIPPED {
                None
            } else {
                Some(result.to_string())
            },
            confidence: conf,
            score: None,
            evidence: None,
            fix_action: fix.map(|s| s.to_string()),
            fix_tier: None,
            recheck_result: None,
            ts: 0,
            judgement_scope: Some(scope.into()),
            group_key: None,
        };
        type S = AssetComplianceState;
        // (名字, 行集, 期望状态, 期望资产置信度)
        let cases: Vec<(&str, Vec<VerdictRow>, S, Option<f32>)> = vec![
            ("空行集 = 未跑", vec![], S::Unjudged, None),
            (
                "全 pass",
                vec![row(PASS, "per_asset", None, Some(0.9))],
                S::Pass,
                Some(0.9),
            ),
            (
                "修复后 pass 仍是 pass",
                vec![row(PASS, "per_asset", Some("smart_crop"), Some(0.8))],
                S::Pass,
                Some(0.8),
            ),
            (
                "单 fail",
                vec![row(FAIL, "per_asset", None, Some(0.7))],
                S::Fail,
                Some(0.7),
            ),
            (
                "单 undetermined",
                vec![row(UNDETERMINED, "per_asset", None, Some(0.3))],
                S::Undetermined,
                Some(0.3),
            ),
            (
                "pass + undetermined（无 fail）→ undetermined",
                vec![
                    row(PASS, "per_asset", None, Some(0.9)),
                    row(UNDETERMINED, "per_asset", None, Some(0.4)),
                ],
                S::Undetermined,
                Some(0.4),
            ),
            (
                "fail 压过 undetermined（不合格是结论）",
                vec![
                    row(UNDETERMINED, "per_asset", None, Some(0.4)),
                    row(FAIL, "per_asset", None, Some(0.8)),
                ],
                S::Fail,
                Some(0.4),
            ),
            (
                "组级 fail 传导到成员（per_group 行参与汇总）",
                vec![
                    row(PASS, "per_asset", None, Some(0.95)),
                    row(FAIL, "per_group", None, Some(0.9)),
                ],
                S::Fail,
                Some(0.9),
            ),
            (
                "组级 undetermined 传导",
                vec![
                    row(PASS, "per_asset", None, Some(0.95)),
                    row(UNDETERMINED, "per_group", None, Some(0.5)),
                ],
                S::Undetermined,
                Some(0.5),
            ),
            (
                "组级 pass 不覆盖成员 fail",
                vec![
                    row(FAIL, "per_asset", None, Some(0.9)),
                    row(PASS, "per_group", None, Some(0.99)),
                ],
                S::Fail,
                Some(0.9),
            ),
            (
                "skipped 不参与也不污染",
                vec![
                    row(PASS, "per_asset", None, Some(0.9)),
                    row(SKIPPED, "per_asset", None, None),
                ],
                S::Pass,
                Some(0.9),
            ),
            (
                // 卡 task：全 skipped = 这张图一条结论都没有（非商品图 → 领域 gate 不适用）
                // → 未判定。不许落 Pass（否则「没判」被算成「合格」并进合格统计）
                "全 skipped（每条规则都不适用）= 未判定",
                vec![
                    row(SKIPPED, "per_asset", None, None),
                    row(SKIPPED, "per_asset", None, None),
                ],
                S::Undetermined,
                None,
            ),
            (
                "历史行 confidence=NULL 不参与最小值（不造数）",
                vec![
                    row(PASS, "per_asset", None, None),
                    row(PASS, "per_asset", None, Some(0.6)),
                ],
                S::Pass,
                Some(0.6),
            ),
        ];
        for (name, rows, want, want_conf) in cases {
            let refs: Vec<&VerdictRow> = rows.iter().collect();
            assert_eq!(state_of_verdicts(&refs), want, "场景: {name}");
            assert_eq!(asset_confidence(&refs), want_conf, "置信度: {name}");
        }
    }

    #[test]
    fn sql_and_rust_agree_on_every_state() {
        // 夹具含人工改判（规则级 pass/fail、资产级、撤销后、无判定行+资产级改判），
        // 两套口径必须在这些场景上逐资产一致 —— 这是「合规页说 X、筛选出 Y」的唯一防线。
        let c = fixture_with_reviews();
        for st in AssetComplianceState::ALL {
            let f = ComplianceStateFilter {
                packId: "mini".into(),
                state: st.as_str().into(),
            };
            let (cond, params) = state_condition(&f, 1);
            let sql = format!(
                "SELECT assetId FROM assets a WHERE a.deleted = 0 AND {cond} ORDER BY assetId"
            );
            let mut stmt = c.prepare(&sql).unwrap();
            let sql_ids: Vec<i64> = stmt
                .query_map([params[0].as_str()], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            let rust_ids = asset_ids_by_state(&c, "mini", st).unwrap();
            assert_eq!(
                sql_ids,
                rust_ids,
                "状态 {} SQL 口径与 Rust 口径必须一致",
                st.as_str()
            );
        }
        // 非法状态词 → 空集（不静默当"不过滤"）
        let (cond, _) = state_condition(
            &ComplianceStateFilter {
                packId: "mini".into(),
                state: "whatever".into(),
            },
            1,
        );
        assert_eq!(cond, "0");
    }

    #[test]
    fn versioned_states_follow_latest_verdict() {
        // 重跑批次：同 (asset, rule) 旧 fail → 新 pass，状态必须跟着最新行变
        let c = db();
        asset(&c, 1, "a.jpg");
        verdict(&c, 1, "R1", "fail", None, None);
        assert_eq!(
            states_summary(&c, "mini").unwrap(),
            StatesSummary {
                pass: 0,
                fail: 1,
                undetermined: 0,
                unjudged: 0
            }
        );
        let repo = SqliteComplianceRepo::new(&c);
        repo.insert_verdict(&NewVerdict {
            pack_id: "mini".into(),
            asset_id: 1,
            job_id: 2,
            rule_id: "R1".into(),
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
        assert_eq!(
            states_summary(&c, "mini").unwrap().pass,
            1,
            "取最新批次结论"
        );
        assert!(asset_ids_by_state(&c, "mini", AssetComplianceState::Fail)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn queue_groups_by_asset_with_reason_and_resolution() {
        let c = fixture();
        let pack = pack();
        let (total, cards) = low_confidence_queue(
            &c,
            "mini",
            &pack,
            &QueueOpts {
                threshold: 0.7,
                limit: 50,
                ..Default::default()
            },
            "zh-Hans",
        )
        .unwrap();
        assert_eq!(total, 1, "只有资产 4 需要复核（undetermined）");
        let card = &cards[0];
        assert_eq!(card.assetId, 4);
        assert_eq!(card.fileName, "d.jpg");
        assert_eq!(
            card.rules.len(),
            1,
            "只列需要复核的规则（同资产 pass 的不列）"
        );
        assert_eq!(card.rules[0].ruleId, "R1");
        assert_eq!(card.rules[0].ruleName, "尺寸达标", "规则名来自 pack");
        assert_eq!(card.rules[0].reason.as_deref(), Some("vl_timeout"));
        assert!(card.rules[0].feedback.is_none());
        assert!(!card.resolved);

        // 处置后：resolved=true；only_unresolved 过滤掉
        record_feedback(&c, "mini", &pack, 4, "R1", "fail", None).unwrap();
        let (_, cards) = low_confidence_queue(
            &c,
            "mini",
            &pack,
            &QueueOpts {
                threshold: 0.7,
                limit: 50,
                ..Default::default()
            },
            "zh-Hans",
        )
        .unwrap();
        assert!(cards[0].resolved);
        let fb = cards[0].rules[0].feedback.as_ref().unwrap();
        assert_eq!(fb.machineResult, "undetermined");
        assert_eq!(fb.humanResult, "fail");
        assert!(!fb.misjudged, "未判定的处置不算误判（机器本来没下结论）");
        let (total, cards) = low_confidence_queue(
            &c,
            "mini",
            &pack,
            &QueueOpts {
                threshold: 0.7,
                limit: 50,
                only_unresolved: true,
                ..Default::default()
            },
            "zh-Hans",
        )
        .unwrap();
        assert_eq!(total, 0);
        assert!(cards.is_empty());
    }

    #[test]
    fn record_feedback_derives_machine_result_and_flags_misjudge() {
        let c = fixture();
        let pack = pack();
        // 资产 3 机器 fail，人工 pass → 误判（reason 自动补）
        let row = record_feedback(&c, "mini", &pack, 3, "R1", "pass", None).unwrap();
        assert_eq!(row.machine_result, "fail");
        assert_eq!(row.human_result, "pass");
        assert!(row.is_misjudged());
        assert!(
            row.reason.as_deref().unwrap().contains("误判"),
            "{:?}",
            row.reason
        );
        // 人工给 reason → 以人工为准
        let row = record_feedback(&c, "mini", &pack, 3, "R1", "pass", Some("阈值太松")).unwrap();
        assert_eq!(row.reason.as_deref(), Some("阈值太松"));
        // 一致结论不算误判 + reason 为空
        let row = record_feedback(&c, "mini", &pack, 1, "R1", "pass", None).unwrap();
        assert!(!row.is_misjudged());
        assert_eq!(row.reason, None);
        assert_eq!(feedback_count(&c, "mini").unwrap(), 3);
    }

    #[test]
    fn record_feedback_rejects_bad_input() {
        let c = fixture();
        let pack = pack();
        // 未知规则
        assert!(record_feedback(&c, "mini", &pack, 1, "R9", "pass", None).is_err());
        // 没判过的 (asset, rule)
        assert!(record_feedback(&c, "mini", &pack, 5, "R1", "pass", None).is_err());
        // 非法人工结论
        assert!(record_feedback(&c, "mini", &pack, 1, "R1", "maybe", None).is_err());
        assert_eq!(feedback_count(&c, "mini").unwrap(), 0, "全部拒收，零落库");
    }

    #[test]
    fn feedback_csv_shape_and_misjudge_column() {
        let c = fixture();
        let pack = pack();
        record_feedback(&c, "mini", &pack, 3, "R1", "pass", Some("阈值太松, 需重标")).unwrap();
        record_feedback(&c, "mini", &pack, 4, "R1", "fail", None).unwrap();
        let rows = list_feedback(&c, "mini", None, None, None).unwrap();
        let csv = feedback_csv(&rows);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 3, "表头 + 2 行");
        assert!(lines[0]
            .starts_with("packId,assetId,ruleId,machineResult,humanResult,misjudged,reason,ts"));
        assert!(
            lines[1].contains("\"mini\",3,\"R1\",\"fail\",\"pass\",1,"),
            "{}",
            lines[1]
        );
        assert!(
            lines[1].contains("\"阈值太松, 需重标\""),
            "含逗号的 reason 必须整体加引号: {}",
            lines[1]
        );
        assert!(
            lines[2].contains(",0,"),
            "转人工处置 misjudged=0: {}",
            lines[2]
        );
        // 收窄查询
        assert_eq!(
            list_feedback(&c, "mini", Some(3), None, None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            list_feedback(&c, "mini", None, Some("R2"), None)
                .unwrap()
                .len(),
            0
        );
    }
    // ---------- COMPLIANCE-HUMAN：人工二次审核（改判 / 撤销 / 门槛 / 口径联动） ----------

    /// 直接落一条改判（跳过接口层校验 —— 夹具要造「无判定行 + 资产级改判」这类接口拒收的态）。
    fn review_row(
        c: &Connection,
        asset_id: i64,
        rule: Option<&str>,
        status: &str,
        reason: &str,
    ) -> i64 {
        let repo = SqliteComplianceRepo::new(c);
        repo.save_human_review(&NewHumanReview {
            scheme_id: "pack:mini".into(),
            content_version: core_storage::compliance_sqlite::asset_content_version(c, asset_id)
                .unwrap_or_default(),
            asset_id,
            rule_id: rule.map(|r| r.to_string()),
            judgement_scope: "per_asset".into(),
            group_key: String::new(),
            human_status: status.into(),
            human_reason: reason.into(),
            machine_result: None,
            machine_confidence: None,
            reviewed_by: Some("tester".into()),
        })
        .unwrap()
    }

    /// fixture + 六种改判形态（逐规则 pass/fail、资产级、软 fail 改判、撤销后、无判定行）。
    fn fixture_with_reviews() -> Connection {
        let c = fixture();
        review_row(&c, 1, Some("R1"), "fail", "带水印，不合格");
        review_row(&c, 3, Some("R1"), "pass", "白底判定过严");
        review_row(&c, 4, None, "fail", "整图模糊，整张不合格");
        review_row(&c, 5, None, "pass", "人工认定可放行（但这张图没跑过判定）");
        review_row(&c, 6, Some("R2"), "fail", "标签误检已人工确认");
        review_row(&c, 7, Some("R1"), "pass", "尺寸实际达标");
        review_row(&c, 8, Some("R1"), "pass", "先判合格");
        let repo = SqliteComplianceRepo::new(&c);
        repo.revoke_human_review("pack:mini", 8, Some("R1"), Some("复核后又改回机器结论"))
            .unwrap();
        c
    }

    #[test]
    fn human_review_flips_effective_state_and_revoke_restores() {
        let c = db();
        asset(&c, 1, "a.jpg");
        verdict(&c, 1, "R1", "fail", None, None); // confidence = 0.9（高置信度）
        let pack = pack();
        let req = ReviewRequest {
            asset_id: 1,
            rule_id: Some("R1"),
            human_status: "pass",
            reason: "阈值太松，人工确认尺寸达标",
            reviewed_by: Some("reviewer"),
            force: false,
            threshold: 0.7,
        };
        // 高置信度（0.9 ≥ 0.7）改判必须 force
        let e = record_human_review(&c, "mini", &pack, &req).unwrap_err();
        assert_eq!(e.code(), Some(review_code::NOT_LOW_CONFIDENCE), "{e}");
        assert!(
            e.message().contains("0.90"),
            "文案要给出实际置信度: {}",
            e.message()
        );
        // force=true 后生效：机器 fail（0.9）→ 人工 pass
        let row =
            record_human_review(&c, "mini", &pack, &ReviewRequest { force: true, ..req }).unwrap();
        assert_eq!(row.human_status, "pass");
        assert_eq!(
            row.machine_result.as_deref(),
            Some("fail"),
            "改判当时机器结论快照要存档"
        );
        assert_eq!(row.machine_confidence, Some(0.9));
        assert_eq!(row.reviewed_by.as_deref(), Some("reviewer"));
        assert_eq!(row.human_reason, "阈值太松，人工确认尺寸达标");
        // 生效口径：资产从 fail 变 pass（统计/筛选/报告同源）
        assert_eq!(states_summary(&c, "mini").unwrap().pass, 1);
        assert_eq!(states_summary(&c, "mini").unwrap().fail, 0);
        assert_eq!(
            asset_ids_by_state(&c, "mini", AssetComplianceState::Pass).unwrap(),
            vec![1]
        );
        // 机器判定行没被改写（可审计/可复现）
        let rows = verdicts_latest(&c, "mini").unwrap();
        assert_eq!(rows[0].result, "fail");
        // 撤销 → 回机器结论
        assert_eq!(
            revoke_human_review(&c, "mini", 1, Some("R1"), Some("误操作")).unwrap(),
            1
        );
        assert_eq!(states_summary(&c, "mini").unwrap().fail, 1);
        // 撤销不删行（历史可追）
        let hist = review_history(&c, "mini", Some(1), None).unwrap();
        assert_eq!(hist.len(), 1);
        assert!(!hist[0].is_active());
        assert_eq!(hist[0].revoke_reason.as_deref(), Some("误操作"));
        // 重复撤销 → 明确 not_found 语义
        let e = revoke_human_review(&c, "mini", 1, Some("R1"), None).unwrap_err();
        assert_eq!(e.code(), Some(review_code::NOT_FOUND));
    }

    #[test]
    fn review_rejects_blank_reason_bad_status_unknown_rule_and_unjudged_asset() {
        let c = db();
        asset(&c, 1, "a.jpg");
        asset(&c, 5, "e.jpg");
        // 低置信度（undetermined）→ 免 force
        verdict(
            &c,
            1,
            "R1",
            "undetermined",
            None,
            Some(r#"{"reason":"vl_timeout"}"#),
        );
        let pack = pack();
        let base = ReviewRequest {
            asset_id: 1,
            rule_id: Some("R1"),
            human_status: "pass",
            reason: "VL 超时，人工看图确认达标",
            reviewed_by: None,
            force: false,
            threshold: 0.7,
        };
        // reason 空白（含纯空格/换行）
        for blank in ["", "   ", "\n\t"] {
            let e = record_human_review(
                &c,
                "mini",
                &pack,
                &ReviewRequest {
                    reason: blank,
                    ..base
                },
            )
            .unwrap_err();
            assert_eq!(
                e.code(),
                Some(review_code::REASON_REQUIRED),
                "reason={blank:?}"
            );
        }
        // humanStatus 只能是 pass|fail（undetermined 是机器结论，不是人的裁决）
        for bad in ["undetermined", "maybe", "PASS"] {
            let e = record_human_review(
                &c,
                "mini",
                &pack,
                &ReviewRequest {
                    human_status: bad,
                    ..base
                },
            )
            .unwrap_err();
            assert_eq!(e.code(), Some(review_code::BAD_STATUS), "status={bad}");
        }
        // pack 里没有的规则
        let e = record_human_review(
            &c,
            "mini",
            &pack,
            &ReviewRequest {
                rule_id: Some("R9"),
                ..base
            },
        )
        .unwrap_err();
        assert_eq!(e.code(), Some(review_code::UNKNOWN_RULE));
        // 资产没跑过判定（ruleId 指定与资产级都拒）
        for rule in [Some("R1"), None] {
            let e = record_human_review(
                &c,
                "mini",
                &pack,
                &ReviewRequest {
                    asset_id: 5,
                    rule_id: rule,
                    ..base
                },
            )
            .unwrap_err();
            assert_eq!(
                e.code(),
                Some(review_code::NO_MACHINE_VERDICT),
                "rule={rule:?}"
            );
        }
        // 不存在的资产
        let e = record_human_review(
            &c,
            "mini",
            &pack,
            &ReviewRequest {
                asset_id: 999,
                ..base
            },
        )
        .unwrap_err();
        assert_eq!(e.code(), Some(review_code::NO_MACHINE_VERDICT));
        // 全部拒收 → 零落库
        assert!(review_history(&c, "mini", None, None).unwrap().is_empty());
    }

    #[test]
    fn repeated_review_supersedes_and_only_one_active() {
        let c = db();
        asset(&c, 1, "a.jpg");
        verdict(&c, 1, "R1", "undetermined", None, None); // 免 force
        let pack = pack();
        let req = |status: &'static str, reason: &'static str| ReviewRequest {
            asset_id: 1,
            rule_id: Some("R1"),
            human_status: status,
            reason,
            reviewed_by: None,
            force: false,
            threshold: 0.7,
        };
        let first = record_human_review(&c, "mini", &pack, &req("pass", "第一次：可放行")).unwrap();
        let second =
            record_human_review(&c, "mini", &pack, &req("fail", "第二次：看到水印，不合格"))
                .unwrap();
        assert!(second.id > first.id);
        // 活记录唯一（0017 的部分唯一索引 + 「覆盖 = 旧行落戳」双保险）
        let repo = SqliteComplianceRepo::new(&c);
        let active = repo
            .find_human_reviews(&HumanReviewFilter::active("pack:mini"))
            .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].human_status, "fail", "最终结论取最新改判");
        // 历史两条都在：旧行 revokeReason = superseded
        let hist = review_history(&c, "mini", Some(1), None).unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].human_status, "pass");
        assert_eq!(hist[0].revoke_reason.as_deref(), Some("superseded"));
        assert_eq!(states_summary(&c, "mini").unwrap().fail, 1);
    }

    #[test]
    fn asset_level_review_overrides_aggregate_but_needs_machine_verdict() {
        let c = db();
        asset(&c, 1, "a.jpg");
        // 机器：一条 undetermined（否则整张图未判定）+ 一条 pass
        verdict(&c, 1, "R1", "undetermined", None, None);
        verdict(&c, 1, "R2", "pass", None, None);
        let pack = pack();
        assert_eq!(
            asset_ids_by_state(&c, "mini", AssetComplianceState::Undetermined).unwrap(),
            vec![1]
        );
        // 资产级改判 fail → 整张图直接不合格（不看逐规则汇总）
        record_human_review(
            &c,
            "mini",
            &pack,
            &ReviewRequest {
                asset_id: 1,
                rule_id: None,
                human_status: "fail",
                reason: "整图模糊",
                reviewed_by: None,
                force: false,
                threshold: 0.7,
            },
        )
        .unwrap();
        assert_eq!(
            asset_ids_by_state(&c, "mini", AssetComplianceState::Fail).unwrap(),
            vec![1]
        );
        // 撤销资产级改判 → 回未判定
        revoke_human_review(&c, "mini", 1, None, None).unwrap();
        assert_eq!(
            asset_ids_by_state(&c, "mini", AssetComplianceState::Undetermined).unwrap(),
            vec![1]
        );
    }

    #[test]
    fn queue_and_list_reviews_carry_effective_status_and_review_filters() {
        let c = fixture_with_reviews();
        let pack = pack();
        // 规则级改判：资产 3 的机器 fail → 生效 pass（卡片直接给 effectiveStatus，前端不算）
        let (_, cards) = low_confidence_queue(
            &c,
            "mini",
            &pack,
            &QueueOpts {
                threshold: 0.7,
                limit: 50,
                low_confidence_only: false,
                ..Default::default()
            },
            "zh-Hans",
        )
        .unwrap();
        let c3 = cards
            .iter()
            .find(|x| x.assetId == 3)
            .expect("主动复核视图要含资产 3");
        let r1 = c3.rules.iter().find(|r| r.ruleId == "R1").unwrap();
        assert_eq!(r1.result, "fail", "机器结论保留原样");
        assert_eq!(r1.effectiveStatus, "pass", "生效结论 = 人工改判");
        assert_eq!(r1.humanReview.as_ref().unwrap().humanStatus, "pass");
        assert!(c3.reviewed);
        // 资产级改判：卡片带 assetOverride
        let c4 = cards.iter().find(|x| x.assetId == 4).unwrap();
        assert_eq!(c4.assetOverride.as_ref().unwrap().humanStatus, "fail");
        assert!(c4.reviewed);
        // reviewed 过滤：只看已改判的卡片 → 未改判的资产（2）不出现
        let (_, only_reviewed) = low_confidence_queue(
            &c,
            "mini",
            &pack,
            &QueueOpts {
                threshold: 0.7,
                limit: 50,
                low_confidence_only: false,
                reviewed: Some(true),
                ..Default::default()
            },
            "zh-Hans",
        )
        .unwrap();
        assert!(only_reviewed.iter().all(|x| x.reviewed));
        assert!(!only_reviewed.iter().any(|x| x.assetId == 2));

        // 扁平复核列表：缺省只列需要复核的行；reviewed=true 只列已改判的行
        let (_, low_only) = list_reviews(
            &c,
            "mini",
            &pack,
            &ReviewQuery {
                threshold: 0.7,
                limit: 500,
                ..Default::default()
            },
            "zh-Hans",
        )
        .unwrap();
        assert!(low_only.iter().all(|i| i.lowConfidence));
        let (_, reviewed) = list_reviews(
            &c,
            "mini",
            &pack,
            &ReviewQuery {
                threshold: 0.7,
                limit: 500,
                low_confidence_only: false,
                reviewed: Some(true),
                ..Default::default()
            },
            "zh-Hans",
        )
        .unwrap();
        // 资产 1 的 R1 是改判（机器 pass 0.9 → 人工 fail）；撤销过的资产 8 不应出现为已复核
        let a1 = reviewed
            .iter()
            .find(|i| i.assetId == 1 && i.ruleId.as_deref() == Some("R1"))
            .unwrap();
        assert_eq!(a1.machineResult, "pass");
        assert_eq!(a1.effectiveStatus, "fail");
        assert_eq!(a1.machineConfidence, Some(0.9));
        assert!(
            !reviewed.iter().any(|i| i.assetId == 8),
            "撤销后不再算已复核"
        );
        // 资产级改判单独一行（ruleId = None）
        assert!(reviewed
            .iter()
            .any(|i| i.assetId == 4 && i.ruleId.is_none()));
        // 检查器视角：给 assetId 时缺省列全部有结论的行
        let (_, detail) = list_reviews(
            &c,
            "mini",
            &pack,
            &ReviewQuery {
                asset_id: Some(4),
                low_confidence_only: false,
                threshold: 0.7,
                limit: 50,
                ..Default::default()
            },
            "zh-Hans",
        )
        .unwrap();
        assert_eq!(detail.len(), 3, "资产级一行 + R1/R2 两行");
    }

    /// 复核视图的排序：`sort=confidence` 按置信度升序（最没把握的先看），不再按「已处置」分组。
    #[test]
    fn review_sort_by_confidence_is_ascending() {
        let c = db();
        let verdict_conf = |asset_id: i64, result: &str, confidence: f64| {
            let repo = SqliteComplianceRepo::new(&c);
            repo.insert_verdict(&NewVerdict {
                pack_id: "mini".into(),
                asset_id,
                job_id: 1,
                rule_id: "R1".into(),
                result: result.into(),
                confidence,
                score: None,
                evidence: None,
                fix_action: None,
                fix_tier: None,
                recheck_result: None,
                judgement_scope: Some("per_asset".into()),
                group_key: None,
            })
            .unwrap();
        };
        for (id, name) in [(1, "a.jpg"), (2, "b.jpg"), (3, "c.jpg")] {
            asset(&c, id, name);
        }
        verdict_conf(1, "undetermined", 0.9);
        verdict_conf(2, "fail", 0.2);
        verdict_conf(3, "fail", 0.5);
        let pack = pack();
        let (_, cards) = low_confidence_queue(
            &c,
            "mini",
            &pack,
            &QueueOpts {
                threshold: 0.7,
                limit: 50,
                low_confidence_only: false,
                sort: ReviewSort::Confidence,
                ..Default::default()
            },
            "zh-Hans",
        )
        .unwrap();
        assert_eq!(
            cards.iter().map(|c| c.assetId).collect::<Vec<_>>(),
            vec![2, 3, 1],
            "置信度升序（0.2 / 0.5 / 0.9）"
        );
        let (_, items) = list_reviews(
            &c,
            "mini",
            &pack,
            &ReviewQuery {
                threshold: 0.7,
                limit: 50,
                low_confidence_only: false,
                sort: ReviewSort::Confidence,
                ..Default::default()
            },
            "zh-Hans",
        )
        .unwrap();
        assert_eq!(
            items.iter().map(|i| i.assetId).collect::<Vec<_>>(),
            vec![2, 3, 1],
            "扁平列表同一口径"
        );
    }
}
