//! C2 合规判定数据访问（存储解耦裁决 2026-10-16）：**trait 定义与 SQL 实现分离**。
//!
//! svc-pipeline 编排器、svc-library 报告导出只依赖 [`ComplianceRepo`] trait，
//! 禁止直接拼 SQL / 直接依赖 rusqlite 连接；SQLite 实现见 [`crate::compliance_sqlite`]。
//! 后续 PG 化 = 新增平行 impl 模块，业务层零改动。
//!
//! 数据模型 = migration 0011 的 `rule_verdict`（判定逐步落，每张图每规则一行；
//! jobId 关联 job_queue kind='compliance' 批次）。plan 文档的 imageSN 在本地 = assetId。

/// 判定结果词表（migration 0016 起唯一合法取值）。
///
/// 三态语义（2026-10-17 用户裁决）：`pass` 合格 / `fail` 不合格 / `undetermined` 未判定
/// （**只表示判定过程出错**：图片读不出、VL 超时或返回不可解析、插件异常）；`skipped` 是该规则
/// 对这张图没有结论（depends_on 短路、打标型、作用域不符），既不参与也不污染资产结论。
/// `pending_human` / `repaired` / `failed` 三个旧词已从库里消失（0016 迁移映射）。
pub mod result {
    pub const PASS: &str = "pass";
    pub const FAIL: &str = "fail";
    pub const UNDETERMINED: &str = "undetermined";
    pub const SKIPPED: &str = "skipped";

    /// 合法结果词（迁移/写入口的校验白名单）。
    pub const ALL: [&str; 4] = [PASS, FAIL, UNDETERMINED, SKIPPED];

    pub fn is_valid(s: &str) -> bool {
        ALL.contains(&s)
    }
}

/// 生效判定词的 SQL 表达式（N5b / migration 0028）。
///
/// `alias` = 表别名（`"rule_verdict"` 或 `"v"` 都行）。**读判定结论必须用它** ——
/// 裸 `result` 列是「执行结果」（ok/failed/skipped），不是结论：
///
/// * `verdict` 有值 → 就是结论；
/// * `verdict IS NULL` → `result='skipped'` 给 `skipped`（该规则对这张图没结论），
///   其余（处理类行 / 行级执行失败）给 `undetermined`（**判不了 ≠ 通过**）。
///
/// 为什么要有这个公开 helper：测试/工具会自己拼 SQL（`dev-e2e` 的核对脚本就是），
/// 让它们抄一遍表达式 = 迟早抄错一处，然后出现「库里结论正确、报告说不合格 0 张」的分裂。
pub fn effective_verdict_expr(alias: &str) -> String {
    format!(
        "COALESCE({alias}.verdict, CASE {alias}.result WHEN 'skipped' THEN 'skipped' \
         ELSE 'undetermined' END)"
    )
}

/// 新判定行（编排器每步一条；result/confidence/score/evidence 语义见 migration 0016 注释）。
#[derive(Debug, Clone, PartialEq)]
pub struct NewVerdict {
    pub pack_id: String,
    pub asset_id: i64,
    pub job_id: i64,
    pub rule_id: String,
    /// [`result::PASS`] | [`result::FAIL`] | [`result::UNDETERMINED`] | [`result::SKIPPED`]
    ///
    /// **写入语义（N5b / 0028）**：这是「判定词」，落库时拆成两列 ——
    /// `skipped` → `result='skipped'` + `verdict=NULL`；其余 → `result='ok'` + `verdict=<本值>`。
    /// 拆列规则只在 `compliance_sqlite::insert_verdict` 一处实现，调用方继续按判定词写。
    pub result: String,
    /// 规则置信度 ∈ [0,1]（**必填**，非 Option：写入路径不可能造出 NULL；0016 前的历史行才是 NULL）。
    /// 语义：检测器/插件对自己这条结论的把握，用来决定是否进「低置信度复核」视图；
    /// 原始 `score` 只作证据，不参与比较。
    pub confidence: f64,
    pub score: Option<f64>,
    /// json 证据（检测器原始输出/阈值/插件输出/短路原因；判定可复现）
    pub evidence: Option<String>,
    pub fix_action: Option<String>,
    /// guaranteed | best_effort
    pub fix_tier: Option<String>,
    /// 复检结果 pass | fail（NULL = 未复检）
    pub recheck_result: Option<String>,
    /// C9：判定作用域 `per_asset` | `per_group`（None = 旧调用方语义，落库按 per_asset 也行——
    /// 0014 前的历史行就是 NULL）。组级判定按**成员展开**：组内每个成员一行，内容逐字相同。
    pub judgement_scope: Option<String>,
    /// C9：组键取值（per_group 行必填；per_asset 行为 None）。见 migration 0014 注释。
    pub group_key: Option<String>,
}

/// 已落库判定行。
///
/// N5b（migration 0028）后的列语义 —— **执行结果 ≠ 审核结论**（结果行契约）：
/// * [`VerdictRow::exec_result`] = 库里 `result` 列：`ok` | `failed` | `skipped`（跑成了没有）
/// * [`VerdictRow::verdict`] = 库里 `verdict` 列：`pass` | `fail` | `undetermined`（审核结论；
///   NULL = 这行没有结论，处理类行就是这样）
/// * [`VerdictRow::result`] = **生效判定词**（`pass`/`fail`/`undetermined`/`skipped`）：
///   读路径按 `COALESCE(verdict, result)` 投影出来的兼容视图，让历史上按判定词读行的
///   调用方（复核索引/导出/库视图筛选）零改动。新代码要区分「跑成没跑成」请读 `exec_result`。
#[derive(Debug, Clone, PartialEq)]
pub struct VerdictRow {
    pub id: i64,
    pub pack_id: String,
    pub asset_id: i64,
    pub job_id: Option<i64>,
    pub rule_id: String,
    /// 生效判定词（见类型注释；NULL 不会出现 —— 处理类行在 `result='ok'` 时也没结论，
    /// 投影成 `undetermined`，与「判不了 ≠ 通过」同口径）
    pub result: String,
    /// 执行结果（0028 `result` 列原值）：ok | failed | skipped
    pub exec_result: String,
    /// 审核结论（0028 `verdict` 列原值；None = 这行没有结论）
    pub verdict: Option<String>,
    /// 0..1；NULL 只出现在 migration 0016 之前的历史行（当时没有置信度概念，不造数）
    pub confidence: Option<f64>,
    pub score: Option<f64>,
    pub evidence: Option<String>,
    pub fix_action: Option<String>,
    pub fix_tier: Option<String>,
    pub recheck_result: Option<String>,
    pub ts: i64,
    /// C9：per_asset | per_group（NULL = 0014 前历史行）
    pub judgement_scope: Option<String>,
    /// C9：组键（per_group 行有值）
    pub group_key: Option<String>,
}

/// 编排器消费的资产快照（元数据档输入：不解码图；宽高来自 primary resource）。
#[derive(Debug, Clone, PartialEq)]
pub struct AssetSnapshot {
    pub asset_id: i64,
    pub abs_path: String,
    pub media_type: String,
    pub missing: i64,
    pub deleted: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// C9（组级判定的去重输入）：assets.fileHash（精确重复边）
    pub file_hash: Option<String>,
    /// C9：primary resource.pHash（hex 字符串；近似重复边）。缺 = 由编排器按需现算。
    pub p_hash: Option<String>,
}

/// 判定查询过滤（按 pack；job_id = 单批次，None = 每 (asset, rule) 取最新一行）。
#[derive(Debug, Clone, PartialEq)]
pub struct VerdictFilter {
    pub pack_id: String,
    pub job_id: Option<i64>,
    /// DECouple-BE：按**一次执行**（`compliance_runs.runId`）取判定行 ——
    /// 一次执行 = N 条 job_queue 行，判定行的 `jobId` 落在这些行上。
    /// 与 `job_id` 互斥（同时给 → job_id 优先，run_id 忽略）。
    pub run_id: Option<i64>,
    /// 只看单个资产（明细下钻用；None = 不限）。
    pub asset_id: Option<i64>,
}

impl VerdictFilter {
    pub fn pack(pack_id: &str) -> Self {
        Self {
            pack_id: pack_id.to_string(),
            job_id: None,
            run_id: None,
            asset_id: None,
        }
    }

    pub fn job(pack_id: &str, job_id: i64) -> Self {
        Self {
            job_id: Some(job_id),
            ..Self::pack(pack_id)
        }
    }

    /// 可选的单批次过滤（None = 不限批次）。
    pub fn job_opt(pack_id: &str, job_id: Option<i64>) -> Self {
        Self {
            job_id,
            ..Self::pack(pack_id)
        }
    }

    /// 按一次执行取（L2/L3 任务中心下钻的唯一口径）。
    pub fn run(pack_id: &str, run_id: i64) -> Self {
        Self {
            run_id: Some(run_id),
            ..Self::pack(pack_id)
        }
    }

    /// 只看某个资产（可与 run/job 叠加）。
    pub fn for_asset(mut self, asset_id: i64) -> Self {
        self.asset_id = Some(asset_id);
        self
    }
}

// ---------- C6：人工反馈（review 界面「误判」按钮） ----------

/// 新反馈行（C6；migration 0011 `rule_feedback`）。
///
/// 语义：machineResult = 当时机器判定（pass | fail | pending_human，取自对应 `rule_verdict` 行），
/// humanResult = 人工结论（pass | fail）。两者不一致 = 这条反馈就是一次「误判」记录
/// （不做单独布尔列：不一致本身就是误判，冗余列会漂移）。
#[derive(Debug, Clone, PartialEq)]
pub struct NewFeedback {
    pub pack_id: String,
    pub asset_id: i64,
    pub rule_id: String,
    pub machine_result: String,
    pub human_result: String,
    pub reason: Option<String>,
}

/// 已落库反馈行。
#[derive(Debug, Clone, PartialEq)]
pub struct FeedbackRow {
    pub id: i64,
    pub pack_id: String,
    pub asset_id: i64,
    pub rule_id: String,
    pub machine_result: String,
    pub human_result: String,
    pub reason: Option<String>,
    pub ts: i64,
}

impl FeedbackRow {
    /// 是否误判（机器结论 ≠ 人工结论）。pending_human 的机器结论无对错，只作待办来源，不算误判。
    pub fn is_misjudged(&self) -> bool {
        matches!(self.machine_result.as_str(), "pass" | "fail")
            && self.machine_result != self.human_result
    }
}

/// 反馈查询过滤（按 pack 全量；asset/rule 可选收窄，用于单图详情与审计）。
#[derive(Debug, Clone, PartialEq)]
pub struct FeedbackFilter {
    pub pack_id: String,
    pub asset_id: Option<i64>,
    pub rule_id: Option<String>,
    /// 返回条数上限（None = 不限）
    pub limit: Option<i64>,
}

impl FeedbackFilter {
    pub fn pack(pack_id: &str) -> Self {
        Self {
            pack_id: pack_id.to_string(),
            asset_id: None,
            rule_id: None,
            limit: None,
        }
    }

    pub fn asset(pack_id: &str, asset_id: i64) -> Self {
        Self {
            asset_id: Some(asset_id),
            ..Self::pack(pack_id)
        }
    }
}

// ---------- COMPLIANCE-HUMAN：人工二次审核（改判 / 撤销） ----------

/// 新人工改判（migration 0035 `human_review`；DECouple-BE 起人审是**独立覆盖层**）。
///
/// 语义：机器结论（`rule_verdict`）**永不改写**；这里只记「人看过了并给出结论」。
/// 生效结论 = `humanStatus ?? result`，由 svc-library 的 `ReviewIndex` 统一解释（查询时合成，不落资产）。
///
/// 关键差异（相对 0017 的 `verdict_human_review`）：键是 `(assetId, schemeId, ruleId)`（人审跨 job 生效），
/// 且带 `contentVersion` —— 资产内容变了（重导/修图）旧人审自动失效。
#[derive(Debug, Clone, PartialEq)]
pub struct NewHumanReview {
    /// 判定方案身份（当前 = `pack:<packId>`；规格化后可用 `spec:<id>`）
    pub scheme_id: String,
    pub asset_id: i64,
    /// None = **资产级整体结论**（改判入口 ruleId 缺省）：最终状态直接取 humanStatus
    pub rule_id: Option<String>,
    /// per_asset | per_group —— 只作审计（用户从哪条结果改的），生效匹配只按 (asset, rule)
    pub judgement_scope: String,
    /// 组键（per_group 结果才有值；审计用）
    pub group_key: String,
    /// pass | fail（人的裁决只有两档）
    pub human_status: String,
    /// 改判理由（**必填**；空白由接口层拒收，非空由 schema CHECK 兜底）
    pub human_reason: String,
    /// 改判当时的机器结论快照（审计对照；不参与生效口径）
    pub machine_result: Option<String>,
    pub machine_confidence: Option<f64>,
    /// 操作人（可选）
    pub reviewed_by: Option<String>,
    /// 改判时的资产内容版本（`asset_versions(current=1).sha256 ?? assets.fileHash ?? ''`）。
    /// **必须由调用方按当前值取**（core-storage 的 `asset_content_version`），否则失效条件失真。
    pub content_version: String,
}

/// 已落库人工改判行。
#[derive(Debug, Clone, PartialEq)]
pub struct HumanReviewRow {
    pub id: i64,
    pub scheme_id: String,
    pub asset_id: i64,
    pub rule_id: Option<String>,
    pub judgement_scope: String,
    pub group_key: String,
    pub human_status: String,
    pub human_reason: String,
    pub human_at: i64,
    pub reviewed_by: Option<String>,
    pub content_version: String,
    pub machine_result: Option<String>,
    pub machine_confidence: Option<f64>,
    /// 撤销/被新改判覆盖的时间戳（NULL = 生效中）
    pub revoked_at: Option<i64>,
    pub revoke_reason: Option<String>,
}

impl HumanReviewRow {
    /// 是否生效中（未被撤销、未被新改判覆盖）。
    ///
    /// **不包含内容版本判断** —— 版本判断在 SQL 侧（`HumanReviewFilter::current_content_only`），
    /// 走 repo 的读路径拿到的活记录已经是「内容版本对得上」的；历史查询才会混入过期版本的行。
    pub fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

/// 人工改判查询过滤。
#[derive(Debug, Clone, PartialEq)]
pub struct HumanReviewFilter {
    pub scheme_id: String,
    pub asset_id: Option<i64>,
    /// Some(None) = 只看资产级；Some(Some(rule)) = 只看该规则；None = 不限
    pub rule_id: Option<Option<String>>,
    /// true（缺省语义）= 只取生效中；false = 连历史（撤销/覆盖）一起取
    pub active_only: bool,
    /// true（缺省语义）= 只取「内容版本与资产当前内容版本相等」的行（人审失效条件的唯一实现点）；
    /// false 只有历史/审计查询才用（要看被内容变更顶掉的人审结论）。
    pub current_content_only: bool,
    /// true = 只取**已被内容变更顶掉**的活记录（`contentVersion <> 当前内容版本`）——
    /// 界面要能说清「这条人审为什么不再生效」，而不是静默消失。
    pub only_stale_content: bool,
    pub limit: Option<i64>,
}

impl HumanReviewFilter {
    pub fn scheme(scheme_id: &str) -> Self {
        Self {
            scheme_id: scheme_id.to_string(),
            asset_id: None,
            rule_id: None,
            active_only: true,
            current_content_only: true,
            only_stale_content: false,
            limit: None,
        }
    }

    pub fn active(scheme_id: &str) -> Self {
        Self::scheme(scheme_id)
    }

    /// 该资产在该方案下的生效人审（含内容版本判断）。
    pub fn asset(scheme_id: &str, asset_id: i64) -> Self {
        Self {
            asset_id: Some(asset_id),
            ..Self::scheme(scheme_id)
        }
    }

    /// 只看「已被内容变更顶掉」的活记录（人审失效名单）。
    pub fn stale(scheme_id: &str) -> Self {
        Self {
            // 与 current_content_only 互斥（SQL 侧 `=` 优先于 `<>`），这里必须显式关掉
            current_content_only: false,
            only_stale_content: true,
            ..Self::scheme(scheme_id)
        }
    }

    /// 历史/审计视图：连撤销、被覆盖、**以及被内容变更顶掉**的行一起给。
    pub fn history(scheme_id: &str) -> Self {
        Self {
            active_only: false,
            current_content_only: false,
            ..Self::scheme(scheme_id)
        }
    }
}

/// 组键解析输入行（C9）：资产 + 组键取值。
///
/// `value` = 按 `field` 取到的组键原文（`field=None` = `source: folder`，取 `abs_path`
/// 的父目录；取不到/为空 → None → 该成员缺键，落 `undetermined` + `group_key_missing`）。
#[derive(Debug, Clone, PartialEq)]
pub struct GroupSourceRow {
    pub asset_id: i64,
    pub abs_path: String,
    pub value: Option<String>,
}

/// 合规判定仓库（svc/engine 层唯一合法的触库通道）。
pub trait ComplianceRepo {
    fn insert_verdict(&self, v: &NewVerdict) -> crate::Result<i64>;
    /// IP-K05（migration 0027）：补写**插件来源列**（`packVersion` / `pluginHash`）。
    ///
    /// 为什么是补写而不是塞进 [`NewVerdict`]：来源只对**插件通道**产出的行有意义
    /// （legacy 通道两列留 NULL），而 `NewVerdict` 有十几处构造点（svc-library 报告/导出/
    /// 测试）——为两个可选列给全仓构造点加参数属无谓 churn。口径上「谁判的」与「判了什么」
    /// 是两个事实，分开写不损失任何东西：调用方在 `insert_verdict` 拿到 id 后立刻补写，
    /// 写失败即当作该 job 失败（不静默丢来源）。
    ///
    /// 返回影响行数（0 = 该 id 不存在，调用方必须当错误处理）。
    fn set_verdict_pack_source(
        &self,
        verdict_id: i64,
        pack_version: &str,
        plugin_hash: &str,
    ) -> crate::Result<usize>;
    /// 清掉某批次已落的判定（断点续跑重做的幂等前提：同 job 重跑不残留旧行）。
    fn delete_verdicts(&self, pack_id: &str, job_id: i64) -> crate::Result<usize>;
    /// 查判定（按 filter 语义排序：assetId 升序，同资产按 ruleId/id 稳定序）。
    fn find_verdicts(&self, f: &VerdictFilter) -> crate::Result<Vec<VerdictRow>>;
    /// 单资产判定明细（**按作用域分开取最新**：每 `(ruleId, judgementScope)` 一行）。
    ///
    /// 为什么单开一个方法而不是改 [`Self::find_verdicts`]：`find_verdicts(job_id=None)` 是
    /// **资产口径**读模型（`GROUP BY assetId, ruleId`），组级规则在资产 job 里只落一条
    /// `skipped` 占位行（C9：组级不污染资产口径）。但组级 job 先跑 → 组级真判定行 id 更小，
    /// 「每 (asset,rule) 取 MAX(id)」就会被**占位行遮住**（界面看到 skipped，真结论消失）。
    /// 详情接口要的是「这张图的逐图结论 + 它所在组的组级结论」，两族都必须在，
    /// 所以按作用域分开取最新，资产口径的桶/筛选口径（SQL `state_condition_sql`）保持不动。
    fn find_asset_verdicts(&self, pack_id: &str, asset_id: i64) -> crate::Result<Vec<VerdictRow>>;
    /// 资产快照（含 primary resource 宽高；资产不存在 → None）。
    fn asset_snapshot(&self, asset_id: i64) -> crate::Result<Option<AssetSnapshot>>;
    /// C9 组键解析：批量取资产 + 组键取值（`field = None` → folder 源；列名走白名单，
    /// 白名单外**响亮报错**，绝不静默拼进 SQL）。
    fn group_source_rows(
        &self,
        asset_ids: &[i64],
        field: Option<&str>,
    ) -> crate::Result<Vec<GroupSourceRow>>;
    /// C9「组内成员可查」：某 pack 某规则某组键的全部成员判定行（ts/id 稳定序）。
    fn find_group_verdicts(
        &self,
        pack_id: &str,
        rule_id: &str,
        group_key: &str,
    ) -> crate::Result<Vec<VerdictRow>>;
    /// 写一条人工反馈（C6；CHECK 由 schema 兜底：非法 machineResult/humanResult 落库报错）。
    fn insert_feedback(&self, f: &NewFeedback) -> crate::Result<i64>;
    /// 查反馈（按 filter；ts 升序 + id 稳定序，同秒多条不抖）。
    fn find_feedback(&self, f: &FeedbackFilter) -> crate::Result<Vec<FeedbackRow>>;
    /// 某 pack 的反馈条数（状态栏/导出计数）。
    fn count_feedback(&self, pack_id: &str) -> crate::Result<i64>;

    // ---------- COMPLIANCE-HUMAN：人工改判 ----------

    /// 保存一条人工改判（**覆盖语义**）：把同一 (asset, scheme, rule|资产级) 的活记录落
    /// `revokedAt`（revokeReason = "superseded"），再插新行；返回新行 id。
    ///
    /// 为什么不是原地 UPDATE：改判历史要可追（审计要能回答「谁在什么时候把它从 fail 改成
    /// pass，理由是什么」），原地改会把上一次的裁决抹掉。活记录唯一性由 0035 的部分唯一索引
    /// 兜底（并发/重复提交不可能双活）。
    fn save_human_review(&self, r: &NewHumanReview) -> crate::Result<i64>;
    /// 撤销生效中的改判（回到机器结论）；返回受影响行数（0 = 本来就没有生效改判）。
    fn revoke_human_review(
        &self,
        scheme_id: &str,
        asset_id: i64,
        rule_id: Option<&str>,
        reason: Option<&str>,
    ) -> crate::Result<usize>;
    /// 查人工改判（默认只看生效中 **且内容版本与资产当前内容一致**；`history()` 连历史一起给）。
    fn find_human_reviews(&self, f: &HumanReviewFilter) -> crate::Result<Vec<HumanReviewRow>>;
}
