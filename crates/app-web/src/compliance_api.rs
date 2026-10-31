//! C6 合规审核 API：行业包管理（内置列表 / 本地导入 / 激活切换 / 只读预览）+ 待人工队列 +
//! 「误判」反馈（写 `rule_feedback`）+ 反馈 CSV 导出。
//!
//! JUDGE-ENTRY（卡 task，WIN-E2E 缺陷 D1）：再加**产品级判定触发**——
//! `POST /compliance/judge` 入队（`compliance_enq` / `compliance_enq_group`）后由后台线程跑
//! `run_compliance_queue_par`，`/judge/status` 轮询进度、`/judge/cancel` 取消。
//! 在此之前判定只有 `dev-e2e` 二进制能触达，用户旅程「导入 → 判定 → 人工处置」在真机不可达。
//!
//! 分层：L3 进程壳，只做 HTTP 解析/校验 + 状态码映射；判定口径、反馈校验、队列聚合全在
//! `svc_library::compliance_view`（避免壳里长出第二套业务规则）。
//!
//! 激活包落 `settings`（key 见下），默认 = 内置电商包 —— 与 C2/C5 一路用的 pack id 一致，
//! 未显式切换时行为不变。激活只影响"界面看哪个包"，不动任何历史判定：
//! 换包 = 换个 packId 查 `rule_verdict`，旧包判定仍可按 packId 复现。

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use core_config::Pack;
use core_storage::compliance::ComplianceRepo;
use core_storage::compliance_runs::{run_status, ComplianceRunRepo, RunRow, TaskRow};
use core_storage::compliance_runs_sqlite::SqliteComplianceRunRepo;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::error_codes::{codes, err_coded};
use crate::AppState;

/// 处理插件包落盘根：`{data_dir}/packs/<plugin-id>/`（`.dampack` 解包后就在这儿）。
pub const PACKS_DIR: &str = "packs";

fn err(status: StatusCode, code: &str, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": code, "message": msg.into() }))).into_response()
}

/// E2（卡 task）：插件装卸族沿用 **legacy 形状**（`error` 恒为码，ip_k08 断言 error==码），
/// 只**新增** `code`（与 error 同值，前端词典键）与 `params`（译文占位符）两个字段 —— 老客户端读
/// error/message 不变；前端 en 词条走 `code`+`params`，与形状无关。
fn err_params(
    status: StatusCode,
    code: &str,
    params: serde_json::Value,
    msg: impl Into<String>,
) -> Response {
    (
        status,
        Json(json!({
            "error": code,
            "message": msg.into(),
            "code": code,
            "params": params,
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------- 判定前置错误（N18）

/// 判定前置失败的**结构化**形状：`code` 稳定可判，`message` 人读，`hint` + `action` 给界面引导。
///
/// 为什么不是一个裸字符串（N15 真机 400 的教训）：用户装好插件、建好规格后点「开始检查」，
/// 拿到的只有一句后端文案 —— 不知道去哪儿补救。三种**可操作**的失败必须各自带去处：
/// * `plugin_not_installed` —— 还没装处理插件 → 去「处理插件」装 `.dampack`；
/// * `spec_plugin_not_installed` —— 规格引用的插件已卸载 → 装回该插件，或改选别的规格；
/// * `spec_plugin_version_mismatch` —— 规格钉的版本与已装版本不同 → 在插件详情页改绑/新建规格。
///
/// 其余内部失败（DB/装载/入队）沿用 `judge_prepare` 一个码：那不是用户能操作的东西，
/// 分太细只会让界面去猜。
#[derive(Debug, Clone)]
struct JudgeFail {
    code: &'static str,
    message: String,
    hint: Option<String>,
    /// 界面动作：`open_packs_drawer` = 打开「处理插件」抽屉（安装/切版本/看规格）
    action: Option<&'static str>,
    /// 译文占位符参数（E2 契约：前端拿 `errors.<code>` 词条做 `{name}` 插值）；空对象省略字段
    params: serde_json::Value,
}

impl JudgeFail {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
            action: None,
            params: serde_json::Value::Null,
        }
    }

    fn guided(
        code: &'static str,
        message: impl Into<String>,
        hint: impl Into<String>,
        action: &'static str,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            hint: Some(hint.into()),
            action: Some(action),
            params: serde_json::Value::Null,
        }
    }

    /// 附译文参数（链式；只在 http() 序列化时落地）
    fn with_params(mut self, params: serde_json::Value) -> Self {
        self.params = params;
        self
    }

    /// 插件未安装（判定入口的第一道门）
    fn plugin_not_installed(plugin_id: &str, packs_dir: &Path) -> Self {
        Self::guided(
            "plugin_not_installed",
            format!("没装处理插件（{plugin_id}）：判定由插件执行，没有插件就没有判定"),
            format!(
                "点「去安装插件」把 .dampack 装到 {} （或在插件详情页切/装一个版本）后再跑",
                packs_dir.display()
            ),
            "open_packs_drawer",
        )
        .with_params(serde_json::json!({ "pluginId": plugin_id }))
    }

    fn http(&self) -> Response {
        let mut body = serde_json::Map::new();
        body.insert("error".into(), serde_json::json!(self.code));
        body.insert("message".into(), serde_json::json!(self.message));
        body.insert("code".into(), serde_json::json!(self.code));
        if self
            .params
            .as_object()
            .is_some_and(|m| !m.is_empty())
        {
            body.insert("params".into(), self.params.clone());
        }
        body.insert("hint".into(), serde_json::json!(self.hint));
        body.insert("action".into(), serde_json::json!(self.action));
        (StatusCode::BAD_REQUEST, Json(serde_json::Value::Object(body))).into_response()
    }
}

/// 内部失败 → `judge_prepare`（保留原始文案；不假装成用户可操作的问题）
impl From<String> for JudgeFail {
    fn from(message: String) -> Self {
        Self::new("judge_prepare", message)
    }
}

// ---------------------------------------------------------------- 行业元数据（插件自带）

fn packs_root(data_dir: &Path) -> PathBuf {
    data_dir.join(PACKS_DIR)
}

// 行业元数据 = **处理插件自带**的 `resources/rules.json`（IP-K16 用户裁决 2026-10-22）。
// 宿主不再内置 YAML 行业包，也没有「当前激活哪个包」这层：判定恒由插件产出，规则名字、
// 报告列名、低置信度阈值都从包里读。装载入口见 `plugin_meta_pack_checked`（错误必须到人）。

/// 空壳元数据（插件**没装**时读接口的占位形状：id = 内置插件常量、无规则名）。
///
/// 只在「压根没有包目录」时用；「包在但装载失败」绝不用它顶替（那会把坏包伪装成
/// available + 0 规则 = 「全都合格」的假通过）。
fn empty_meta_pack() -> Pack {
    Pack {
        id: PLUGIN_ECOMMERCE_ID.to_string(),
        taxonomy: core_config::packs::Taxonomy::default(),
        rules: Vec::new(),
        pipeline: core_config::packs::Pipeline::default(),
        report: core_config::packs::ReportTemplate::default(),
    }
}

/// 装载插件元数据（**错误不吞**）。
///
/// `Pack::from_plugin_dir` 是「配了不读一律装载期拒绝」的守卫所在（未知规则键、
/// `scope` 词表错、缺 rules.json 都在这里硬拒）。旧实现用 `if let Ok(p)` 把这个错误
/// 咽掉，界面上表现为**available=true + 0 条规则**、`/judge` 返回 200 + candidates=0 ——
/// 用户看到「审核成功」，实际一张都没判。这里把错误原样交出去：
/// * `/status` → 顶层 `packError` + `pluginState{available:false, reason:plugin_meta_invalid, note}`；
/// * `POST /judge` → 400 `judge_prepare`（点名的坏键进 message）。
///
/// 返回 `Ok(empty_meta_pack())` 只表示「这个 id 没装包」（由 `pluginState` 的
/// `plugin_pack_missing` 说明），不表示「包坏了也算通」。
pub fn plugin_meta_pack_checked(conn: &Connection, data_dir: &Path) -> Result<Pack, String> {
    match plugin_pack_dir(conn, data_dir) {
        Some(dir) => load_meta_pack(&dir),
        None => Ok(empty_meta_pack()),
    }
}

/// 目录 → 元数据；失败信息带目录（排障要点名「哪个包、哪个键」）。
fn load_meta_pack(dir: &Path) -> Result<Pack, String> {
    Pack::from_plugin_dir(dir).map_err(|e| format!("处理插件元数据装载失败（{}）：{e}", dir.display()))
}

/// 读接口降级用的宽松版本（只要规则名/阈值，不能因为坏包把整个读面打死）。
///
/// **降级不等于静默**：装载错误由 `/status` 的 `packError` 与 `pluginState.note` 透出，
/// 判定入口 `/judge` 用 `plugin_meta_pack_checked` 硬拒。
pub fn plugin_meta_pack(conn: &Connection, data_dir: &Path) -> Pack {
    plugin_meta_pack_checked(conn, data_dir).unwrap_or_else(|_| empty_meta_pack())
}

// ---------------------------------------------------------------- GET /compliance/status

#[derive(Debug, Deserialize)]
pub struct QueueQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// true = 隐藏已处置（有反馈**或**有生效改判）的项
    pub onlyUnresolved: Option<bool>,
    /// 低置信度阈值（0..1；缺省 = pack report.yaml low_confidence，再缺省 0.7）
    pub threshold: Option<f64>,
    /// false = 不限低置信度（列出该资产全部有结论的行）—— 用户主动复核高置信度结果时用
    pub lowConfidence: Option<bool>,
    /// true = 只看已人工改判的；false = 只看还没改判的；缺省不限
    pub reviewed: Option<bool>,
    /// 排序：asset（缺省）| confidence（置信度升序，最没把握的先看）
    pub sort: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FeedbackQuery {
    pub assetId: Option<i64>,
    pub ruleId: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct VerdictQuery {
    pub assetId: Option<i64>,
    /// 批量形态：`assetIds=1,2,3`（≤200）。
    /// 合规页「按状态浏览」的缩略图网格要一次拿到整页每张图的判定摘要，
    /// 单体形态会让前端发 N 次串行请求（15 张就是 15 个 RTT）。两条形态共用同一
    /// `asset_verdict_payload`，口径不允许分叉。
    pub assetIds: Option<String>,
    /// DECouple-BE：按**一次执行**看（`compliance_runs.runId`）。
    ///
    /// 给了它才返回合成结论（`machineStatus`/`effectiveStatus`/`override`）；
    /// 不给 = 只读判定行原文（资产级结论不在资产视图里出现）。
    #[serde(alias = "runId")]
    pub run_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct FeedbackReq {
    pub assetId: i64,
    pub ruleId: String,
    /// pass | fail（人工结论）
    pub humanResult: String,
    pub reason: Option<String>,
}

/// GET /api/compliance/status —— 插件身份 + 合规状态分桶 + 反馈统计。
pub async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    // 元数据装载错误**必须到人**：顶层 `packError` + `pluginState{available:false, note}`。
    // 不能 `if let Ok` 吞成「available + 0 条规则」—— 那是「全都合格」的假通过。
    let (pack, pack_error) = match plugin_meta_pack_checked(&conn, &state.data_dir) {
        Ok(p) => (p, serde_json::Value::Null),
        Err(e) => (empty_meta_pack(), json!(e)),
    };
    // IP-K10-FIX：判定行的读口径包 id（引擎感知）—— 插件通道的行写在插件包 id 下。
    let pack_id = verdict_pack_id(&conn, &state.data_dir);
    let summary = match svc_library::compliance_view::states_summary(&conn, &pack_id) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "states", e.to_string()),
    };
    let feedback = match svc_library::compliance_view::list_feedback(&conn, &pack_id, None, None, None) {
        Ok(rows) => rows,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "feedback", e.to_string()),
    };
    let misjudged = feedback.iter().filter(|f| f.is_misjudged()).count();
    // 队列（含已处置）：total/unresolved 两个口径都给，界面才敢说"还剩几张要人看"
    let (queue_total, cards) = match svc_library::compliance_view::low_confidence_queue(
        &conn,
        &pack_id,
        &pack,
        &svc_library::compliance_view::QueueOpts {
            threshold: pack.low_confidence(),
            limit: 1000,
            ..Default::default()
        },
        locale_from_headers(&headers),
    ) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "queue", e.to_string()),
    };
    let unresolved = cards.iter().filter(|c| !c.resolved).count();
    let reviewed = cards.iter().filter(|c| c.reviewed).count();

    (
        StatusCode::OK,
        Json(json!({
            // 行业元数据（插件自带）：规则条数用插件包的规则清单；
            // 装载失败时这里是空壳 + 下面 `packError` 点名坏键（前端据此报错，不当「0 规则=全合格」）
            "pack": { "id": pack.id, "rules": pack.rules.len() },
            // 元数据装载错误（null = 正常）；与 pluginState.note 同源
            "packError": pack_error,
            // 判定行的读口径包 id = 插件身份 id（判定只由插件产出）
            "verdictPackId": pack_id.clone(),
            // 插件状态（包身份、能不能用、为什么不能用）
            "pluginState": plugin_state(&conn, &state.data_dir),
            "states": {
                "pass": summary.pass, "fail": summary.fail,
                "undetermined": summary.undetermined, "unjudged": summary.unjudged,
                "judged": summary.judged(), "total": summary.total(),
            },
            "queue": { "total": queue_total, "unresolved": unresolved, "reviewed": reviewed },
            "feedback": { "total": feedback.len(), "misjudged": misjudged },
        })),
    )
        .into_response()
}

/// GET /api/compliance/queue —— 待人工队列（按资产聚合，规则名/证据/处置状态齐）。
pub async fn queue(
    State(state): State<AppState>,
    Query(q): Query<QueueQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    let pack = plugin_meta_pack(&conn, &state.data_dir);
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).max(0);
    let threshold = q
        .threshold
        .filter(|t| t.is_finite() && (0.0..=1.0).contains(t))
        .unwrap_or_else(|| pack.low_confidence());
    let sort = match parse_sort(q.sort.as_deref()) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    // IP-K10-FIX：队列查的是「判定行落在哪个包 id 下」—— 引擎感知，不是配置包 id
    let pack_id = verdict_pack_id(&conn, &state.data_dir);
    match svc_library::compliance_view::low_confidence_queue(
        &conn,
        &pack_id,
        &pack,
        &svc_library::compliance_view::QueueOpts {
            threshold,
            low_confidence_only: q.lowConfidence.unwrap_or(true),
            reviewed: q.reviewed,
            only_unresolved: q.onlyUnresolved.unwrap_or(false),
            sort,
            limit,
            offset,
        },
        locale_from_headers(&headers),
    ) {
        Ok((total, items)) => (
            StatusCode::OK,
            Json(json!({ "total": total, "items": items, "packId": pack_id })),
        )
            .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "queue", e.to_string()),
    }
}

/// 单资产判定 payload（判定行原文 + 可选的**一次执行**合成结论）。
///
/// DECouple-BE 口径（用户裁决 2026-10-31）：
/// * **不给 `runId` 时只回判定行原文** —— 资产级合成结论（`finalStatus`/`assetHumanReview`/
///   `state`/`machineState`）一律不下发，它们是 job 域概念，不能漏进资产视图；
/// * 给 `runId` 时按「这一次执行的 job 账本 + human_review」**查询时合成** `machineStatus`/
///   `effectiveStatus`/`override` —— 任务中心 L2/L3 下钻用它（`finalStatus` 这个名字已全仓废弃）。
///
/// 分桶口径只看**逐图**行（组级行不参与资产状态：C9 裁决，库视图筛选 SQL 同口径）；
/// 明细两族都给 —— 组级结论被资产 job 的 skipped 占位行遮住是这里最常见的误读。
fn asset_verdict_payload(
    conn: &Connection,
    pack: &Pack,
    pack_id: &str,
    asset_id: i64,
    run_id: Option<i64>,
    locale: &str,
) -> Result<serde_json::Value, String> {
    let repo = core_storage::compliance_sqlite::SqliteComplianceRepo::new(conn);
    // 给了 runId 就只取那次执行的判定行（L3 下钻口径）；否则取每 (asset, rule) 最新行
    let filter = match run_id {
        Some(run) => core_storage::compliance::VerdictFilter::run(pack_id, run),
        None => core_storage::compliance::VerdictFilter::pack(pack_id),
    }
    .for_asset(asset_id);
    let rows = repo.find_verdicts(&filter).map_err(|e| e.to_string())?;
    let asset_rows: Vec<&core_storage::compliance::VerdictRow> = rows
        .iter()
        .filter(|r| r.judgement_scope.as_deref() != Some("per_group"))
        .collect();
    let reviews = svc_library::compliance_view::ReviewIndex::load(conn, pack_id).map_err(|e| e.to_string())?;
    let machine_state = svc_library::compliance_view::state_of_verdicts(&asset_rows);
    let state_word = svc_library::compliance_view::state_of_verdicts_with(&asset_rows, &reviews);
    let feedback = svc_library::compliance_view::list_feedback(conn, pack_id, Some(asset_id), None, None)
        .unwrap_or_default();
    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let fb = feedback.iter().filter(|f| f.rule_id == r.rule_id).max_by_key(|f| f.id);
            json!({
                "ruleId": r.rule_id,
                // i18n-F：按请求 locale 取规则名 + 原样 i18n 对象（切语言前端即时切，不重拉）
                "ruleName": pack
                    .rule(&r.rule_id)
                    .map(|x| {
                        let picked = x.name.pick(locale);
                        if picked.trim().is_empty() {
                            r.rule_id.clone()
                        } else {
                            picked.to_string()
                        }
                    })
                    .unwrap_or_else(|| r.rule_id.clone()),
                "ruleNameI18n": pack
                    .rule(&r.rule_id)
                    .filter(|x| x.name.as_map().is_some())
                    .and_then(|x| serde_json::to_value(&x.name).ok())
                    .unwrap_or(serde_json::Value::Null),
                // JUDGE-ENTRY：判定作用域与组键（组级判定按成员展开成行 —— 界面要能区分
                // 「这张图的逐图结论」和「它所在组的组级结论」，否则两类行看起来一样）
                "judgementScope": r.judgement_scope,
                "groupKey": r.group_key,
                "result": r.result,
                // 行级生效结论（人工改判优先；查询时合成，不落资产）+ 改判明细
                "effectiveStatus": svc_library::compliance_view::effective_result(r, &reviews),
                "confidence": r.confidence,
                "lowConfidence": r.result
                    == core_storage::compliance::result::UNDETERMINED
                    || (r.result != core_storage::compliance::result::SKIPPED
                        && r.confidence.map(|c| c < pack.low_confidence()).unwrap_or(true)),
                "humanReview": reviews
                    .rule_override(r.asset_id, &r.rule_id)
                    .map(|o| json!(o)),
                "score": r.score,
                "reason": svc_library::compliance_view::evidence_reason(r.evidence.as_deref()),
                "evidence": r.evidence,
                "fixAction": r.fix_action,
                "fixTier": r.fix_tier,
                "recheckResult": r.recheck_result,
                "ts": r.ts,
                "feedback": fb.map(|f| json!({
                    "id": f.id, "machineResult": f.machine_result, "humanResult": f.human_result,
                    "reason": f.reason, "misjudged": f.is_misjudged(), "ts": f.ts,
                })),
            })
        })
        .collect();
    let mut out = json!({
        "assetId": asset_id,
        // 判定方案身份（人审覆盖层的键；当前 = pack:<packId>）
        "schemeId": svc_library::compliance_view::review_scheme(pack_id),
        "items": items,
    });
    // 合成结论只在**一次执行的上下文**里给（job 域）；没有 runId 的资产视图不带任何资产级结论。
    if let Some(run) = run_id {
        let ov = reviews.asset_override(asset_id);
        let stale = svc_library::compliance_view::stale_review_assets(conn, pack_id)
            .map(|s| s.contains(&asset_id))
            .unwrap_or(false);
        if let Some(obj) = out.as_object_mut() {
            obj.insert("runId".into(), json!(run));
            obj.insert("machineStatus".into(), json!(machine_state.as_str()));
            obj.insert("effectiveStatus".into(), json!(state_word.as_str()));
            obj.insert(
                "machineConfidence".into(),
                json!(svc_library::compliance_view::asset_confidence(&asset_rows)),
            );
            obj.insert(
                "override".into(),
                match ov {
                    Some(o) => json!({
                        "status": o.humanStatus, "reason": o.reason, "at": o.at,
                        "contentVersion": o.contentVersion,
                    }),
                    None => serde_json::Value::Null,
                },
            );
            // 有人审但资产内容已变 → 不生效（界面要如实说原因，不静默消失）
            obj.insert("overrideStale".into(), json!(stale && ov.is_none()));
        }
    }
    Ok(out)
}

/// `assetIds=a,b,c` → id 列表（≤200、非数字项直接 400，不静默丢弃）
fn parse_asset_ids(raw: &str) -> Result<Vec<i64>, String> {
    let mut ids: Vec<i64> = Vec::new();
    for tok in raw.split(',') {
        let t = tok.trim();
        if t.is_empty() {
            continue;
        }
        match t.parse::<i64>() {
            Ok(v) => ids.push(v),
            Err(_) => return Err(format!("assetIds 含非数字项: {t}")),
        }
    }
    if ids.is_empty() {
        return Err("assetIds 不能为空".into());
    }
    if ids.len() > 200 {
        return Err(format!("assetIds 最多 200 个，收到 {}", ids.len()));
    }
    Ok(ids)
}

#[cfg(test)]
mod asset_ids_tests {
    use super::parse_asset_ids;

    #[test]
    fn parses_comma_list_and_trims() {
        assert_eq!(parse_asset_ids("1,2,3").unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_asset_ids(" 7 , 8 ").unwrap(), vec![7, 8]);
        // 尾逗号/空段跳过，不制造 id=0
        assert_eq!(parse_asset_ids("9,,10,").unwrap(), vec![9, 10]);
    }

    #[test]
    fn rejects_non_numeric_instead_of_silently_dropping() {
        // 静默丢弃会让「页面上少了一张图」变成一个查不出的谜
        let e = parse_asset_ids("1,abc,3").unwrap_err();
        assert!(e.contains("abc"), "错误信息要点名坏值: {e}");
    }

    #[test]
    fn rejects_empty_and_over_limit() {
        assert!(parse_asset_ids("").is_err());
        assert!(parse_asset_ids(" , ,  ").is_err());
        let ok200: Vec<String> = (1..=200).map(|i| i.to_string()).collect();
        assert_eq!(parse_asset_ids(&ok200.join(",")).unwrap().len(), 200);
        let too_many: Vec<String> = (1..=201).map(|i| i.to_string()).collect();
        assert!(parse_asset_ids(&too_many.join(",")).is_err());
    }
}

/// GET /api/compliance/verdicts?assetId=<id> —— 单资产全规则判定（库视图/详情面板用）。
/// GET /api/compliance/verdicts?assetIds=1,2,3 —— 批量形态（合规页网格；≤200）。
pub async fn verdicts(
    State(state): State<AppState>,
    Query(q): Query<VerdictQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let locale = locale_from_headers(&headers);
    let batch_ids: Option<Vec<i64>> = match q.assetIds.as_deref() {
        None => None,
        Some(raw) => match parse_asset_ids(raw) {
            Ok(v) => Some(v),
            Err(msg) => return err(StatusCode::BAD_REQUEST, "bad_request", msg),
        },
    };
    if q.assetId.is_none() && batch_ids.is_none() {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::COMPLIANCE_ASSETS_REQUIRED,
            json!({}),
            "assetId 或 assetIds 必填",
        );
    }
    let conn = state.db.lock().await;
    let pack = plugin_meta_pack(&conn, &state.data_dir);
    // IP-K10-FIX：查哪一族判定行 = 引擎感知的「写侧包 id」（插件通道写插件包 id）
    let pack_id = verdict_pack_id(&conn, &state.data_dir);

    // 批量形态：一次返回整页每张图的 state + 明细（前端网格不再 N 次请求）。
    // 判定行口径与单体完全一致 —— 同一个 asset_verdict_payload。
    if let Some(ids) = batch_ids {
        let mut items: Vec<serde_json::Value> = Vec::with_capacity(ids.len());
        for asset_id in ids {
            match asset_verdict_payload(&conn, &pack, &pack_id, asset_id, q.run_id, locale) {
                Ok(v) => items.push(v),
                Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "verdicts", e),
            }
        }
        return (
            StatusCode::OK,
            Json(json!({ "packId": pack_id, "items": items })),
        )
            .into_response();
    }

    let asset_id = q.assetId.expect("assetId 已在上面校验");
    match asset_verdict_payload(&conn, &pack, &pack_id, asset_id, q.run_id, locale) {
        Ok(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("packId".into(), json!(pack_id));
            }
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "verdicts", e),
    }
}

// ------------------------------------------------- N5b：结果表（result_columns）

#[derive(Debug, Deserialize)]
pub struct ResultsQuery {
    /// 缺省 = 当前写侧包（与 `/verdicts` 同口径）
    #[serde(alias = "packId")]
    pub pack_id: Option<String>,
    /// 只看某一次执行；缺省 = 每 (asset, rule) 最新一行
    #[serde(alias = "jobId")]
    pub job_id: Option<i64>,
    /// N12：按**一次执行**（`/api/compliance/jobs` 的 runId）看结果。
    ///
    /// 与 `jobId` 的区别是硬的：`jobId` 是「单资产那次队列行」，`runId` 是「用户点的这一次执行」
    /// （一次执行 = N 个资产队列行）。任务中心按 runId 过滤才不会只显示第一张图的结果。
    #[serde(alias = "runId")]
    pub run_id: Option<i64>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// `packId` → (kinds 规范串, 包目录)。
///
/// kinds **先看安装记录**（0028 `packs.kinds`）：判定行的来源包可能已卸载/被覆盖，
/// 安装时的事实才是「这批判定该不该有审核列」的依据；账本没记录（手工拷包/内置包）
/// 才回落到包目录 manifest，最后回落 `judge`（与 manifest 缺省同口径）。
fn pack_kinds_and_dir(
    conn: &Connection,
    data_dir: &Path,
    pack_id: &str,
) -> (String, Option<PathBuf>) {
    use core_storage::packs::PackRepo as _;
    if let Ok(Some(rec)) = core_storage::packs::SqlitePackRepo::new(conn).current(pack_id) {
        return (rec.kinds, Some(PathBuf::from(rec.dir)));
    }
    if let Some(dir) = plugin_pack_dir_for(conn, data_dir, pack_id) {
        if let Ok((m, _)) = pack_host::load_pack(&dir) {
            return (m.kinds_canonical(), Some(dir));
        }
    }
    // 内置配置包（legacy 通道）= 判定类：它们只有 rules.yaml，没有插件 kinds 声明
    ("judge".to_string(), None)
}

/// `kinds` 规范串 → 数组（`["judge"]` / `["process"]` / `["judge","process"]`）。
fn kinds_array(kinds: &str) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for part in kinds.split(',') {
        match part.trim() {
            "judge" if !out.contains(&"judge") => out.push("judge"),
            "process" if !out.contains(&"process") => out.push("process"),
            _ => {}
        }
    }
    if out.is_empty() {
        out.push("judge");
    }
    out
}

/// GET /api/compliance/results —— 结果表数据源（N5b）。
///
/// 口径（结果行契约）：一行 = 一个 `(assetId, ruleId)` 的最新判定行；
/// * `result` = 执行结果（ok|failed|skipped，**所有**行都有）
/// * `verdict` = 审核结论（`{status,confidence,reasonCode}`，**只有判定类行**有；无 → null）
/// * `fields` = 行业自定义列（插件行 `evidence.fields` 原样搬出来，宿主当不透明字典）
///
/// 同时返回 `kinds`（要不要画审核列）+ `columns`（插件声明的列 schema，未声明 → null = 默认列）。
/// 前端只按这两个字段决定怎么画，**不猜**。
pub async fn results(
    State(state): State<AppState>,
    Query(q): Query<ResultsQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    let pack_id = match q.pack_id.as_deref().map(str::trim) {
        Some(p) if !p.is_empty() => p.to_string(),
        // PROC-RES-1：按 runId 看结果时，行跟着**那次执行的包**走（`run.params.packId`）——
        // 处理 run 的行落在插件包名下（如 com.soujpg.pack.imaging），不这么解析就会
        // 恒查当前配置包（电商）→ 处理 run 的明细永远空表。
        _ if q.run_id.is_some() => run_pack_id(&conn, q.run_id.unwrap())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| verdict_pack_id(&conn, &state.data_dir)),
        _ => verdict_pack_id(&conn, &state.data_dir),
    };
    let limit = q.limit.unwrap_or(200).clamp(1, 500);
    let offset = q.offset.unwrap_or(0).max(0);
    let (kinds, dir) = pack_kinds_and_dir(&conn, &state.data_dir, &pack_id);
    let parsed = match dir.as_ref() {
        Some(d) => pack_host::result_columns::load_result_columns(d, locale_from_headers(&headers)),
        None => pack_host::result_columns::ParsedColumns::default(),
    };

    // PROC-RES-1：处理类包（kinds 只含 process）的结果行在 `asset_versions`（0032 口径：
    // 不写 rule_verdict，避免空判定行污染「未判定」统计）。下面分支按同一行契约出
    // `asset_versions` 的行（`verdict` 恒 null、`artifacts` 带产物 path/sha256）。
    let kinds_list = kinds_array(&kinds);
    if !kinds_list.is_empty() && kinds_list.iter().all(|k| *k == "process") {
        return process_results_view(&conn, &pack_id, &q, &kinds, parsed);
    }

    // 每 (asset, rule, scope) 最新一行 —— 与 /verdicts 同一取行口径。
    // 指定 `jobId` 时**不加**最新行过滤：那是「看这一次执行写了什么」，混入后续重跑会把
    // 本次明细覆盖掉（历史审计要能对上当时的行）。
    let latest = if q.job_id.is_some() || q.run_id.is_some() {
        "1 = 1"
    } else {
        "v.id IN (SELECT MAX(id) FROM rule_verdict WHERE packId = ?1 \
                  GROUP BY assetId, ruleId, COALESCE(judgementScope, 'per_asset'))"
    };

    // 一次执行（runId）→ 该 run 下的所有资产队列行；单资产（jobId）→ 就那一行。
    //
    // 占位符用 ?2：一旦带了范围过滤，`latest` 恒为 `1 = 1`（不看最新行，只看这一次写了什么），
    // ?2 就空出来了 —— 编号必须与下面 params 的实际顺序对上（错一位 = `Wrong number of parameters`）。
    let select_scope: Option<(&'static str, i64)> = match (q.job_id, q.run_id) {
        (Some(job), _) => Some((" AND v.jobId = ?2", job)),
        (None, Some(run)) => Some((
            " AND v.jobId IN (SELECT id FROM job_queue WHERE runId = ?2)",
            run,
        )),
        (None, None) => None,
    };
    let total_res = match &select_scope {
        Some((clause, id)) => conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM rule_verdict v JOIN assets a ON a.assetId = v.assetId \
                 WHERE v.packId = ?1 AND {latest}{clause}"
            ),
            rusqlite::params![pack_id, id],
            |r| r.get::<_, i64>(0),
        ),
        None => conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM rule_verdict v JOIN assets a ON a.assetId = v.assetId \
                 WHERE v.packId = ?1 AND {latest}"
            ),
            rusqlite::params![pack_id],
            |r| r.get::<_, i64>(0),
        ),
    };
    // 计数失败必须响亮（静默 0 = 界面显示「没有结果」，这是最难查的一类假象）
    let total = match total_res {
        Ok(n) => n,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "results", e.to_string()),
    };

    // 参数编号固定：?1 packId（外层）/ ?2 = latest 子查询的 packId **或** 范围过滤的 id
    // （两者互斥：带范围过滤时 latest 恒为 `1 = 1`）/ ?3 limit / ?4 offset。
    // 编号写死而不是按条件拼串 —— 拼串最容易错一位；编号错位会直接报
    // `Wrong number of parameters passed to query`（N12 实测踩过一次）。
    let job_clause = select_scope.as_ref().map(|(c, _)| *c).unwrap_or("");
    let sql = format!(
        "SELECT v.assetId, a.fileName, a.absPath, v.ruleId, v.result, v.verdict,
                v.confidence, json_extract(v.evidence, '$.fields'),
                json_extract(v.evidence, '$.reason'), v.ts, v.judgementScope
         FROM rule_verdict v JOIN assets a ON a.assetId = v.assetId
         WHERE v.packId = ?1 AND {latest}{job_clause}
         ORDER BY v.assetId, v.ruleId, v.id LIMIT ?3 OFFSET ?4"
    );
    // ?1 packId / ?2 范围 id（仅带范围过滤时；否则是 latest 子查询的 packId）/ ?3 limit / ?4 offset
    let params: Vec<Box<dyn rusqlite::ToSql>> = vec![
        Box::new(pack_id.clone()),
        match &select_scope {
            Some((_, id)) => Box::new(*id) as Box<dyn rusqlite::ToSql>,
            None => Box::new(pack_id.clone()) as Box<dyn rusqlite::ToSql>,
        },
        Box::new(limit),
        Box::new(offset),
    ];
    let mut items: Vec<serde_json::Value> = Vec::new();
    let mapped = conn.prepare(&sql).and_then(|mut stmt| {
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), |r| {
            let exec: String = r.get(4)?;
            let verdict: Option<String> = r.get(5)?;
            let confidence: Option<f64> = r.get(6)?;
            let fields: Option<String> = r.get(7)?;
            let reason: Option<String> = r.get(8)?;
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                exec,
                verdict,
                confidence,
                fields,
                reason,
                r.get::<_, i64>(9)?,
                r.get::<_, Option<String>>(10)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok::<_, rusqlite::Error>(out)
    });
    let rows = match mapped {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "results", e.to_string()),
    };
    for (asset_id, file_name, abs_path, rule_id, exec, verdict, confidence, fields, reason, ts, scope) in rows {
        let fields_json: serde_json::Value = fields
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| json!({}));
        items.push(json!({
            "assetId": asset_id,
            "fileName": file_name,
            "absPath": abs_path,
            "ruleId": rule_id,
            "judgementScope": scope,
            // 执行结果（所有行都有）
            "result": exec,
            // 审核结论（只有判定类行有；处理类行 = null）
            "verdict": verdict.map(|status| json!({
                "status": status,
                "confidence": confidence,
                "reasonCode": reason,
            })),
            "reason": reason,
            "fields": fields_json,
            "ts": ts,
        }));
    }

    (
        StatusCode::OK,
        Json(json!({
            "packId": pack_id,
            "kinds": kinds_array(&kinds),
            "jobId": q.job_id,
            // N12：按执行过滤时把它回显（前端据此确认「看的就是这一次」）
            "runId": q.run_id,
            "total": total,
            "limit": limit,
            "offset": offset,
            // null = 插件没声明列 → 前端用默认列（资产/执行结果/产物/备注）
            "columns": parsed.columns,
            "columnWarnings": parsed.warnings,
            "items": items,
        })),
    )
        .into_response()
}

/// run 的 `params.packId`（judge 入队时落进 run 行的「这次执行的包」身份）。
///
/// 读不到（历史 run 没存 / params 坏）= `None`，调用方回落当前配置包口径。
fn run_pack_id(conn: &Connection, run_id: i64) -> Option<String> {
    let repo = SqliteComplianceRunRepo::new(conn);
    let row = repo.get(run_id).ok()??;
    let params: serde_json::Value = serde_json::from_str(row.params.as_deref()?).ok()?;
    params
        .get("packId")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// `asset_versions.chain` 原文 → 接口 `fields.chain`（**恒为数组**）。
///
/// 写侧 `crates/svc-pipeline/src/compliance_queue.rs`（`AssetVersionRec.chain`）落的是
/// `ChainOutcome::evidence()` 整块 —— **对象**
/// `{"chain":[{action,result,outPath,sha256,bytes,format,width,height,ms}, …],
///   "durationMs":..,"cancelled":..}`；历史/导出数据里也有裸数组。
/// 两种口径都认：对象取 `.chain`（不是数组 = 空数组），数组原样透传。
/// 不改写侧形状（已落库的都是对象，改形状要迁移历史数据）。
fn chain_view(raw: &str) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v @ serde_json::Value::Array(_)) => v,
        Ok(serde_json::Value::Object(map)) => map
            .get("chain")
            .filter(|v| v.is_array())
            .cloned()
            .unwrap_or_else(|| json!([])),
        _ => json!([]),
    }
}

/// PROC-RES-1：处理类包的结果行视图（数据源 `asset_versions`）。
///
/// 为什么不查 `rule_verdict`：migration 0032 的口径 —— 处理类没有审核结论，空判定行
/// 会污染「未判定」统计；产物账本本来就是 `asset_versions`（result/path/sha256/逐步链）。
///
/// 行契约与判定行**同形**：`result`（ok|failed）/ `verdict` 恒 null / `fields`（链证据
/// 搬进行业列字典），**外加 `artifacts`**：`[{kind:"export_file", path, sha256, …}]`
/// （处理产物唯一去处；导出/预览按它取）。`ruleId` 槽位放链的拼接（如 `resize→convert`）
/// —— 处理行没有规则 id，但行 key 要稳定可辨识。

fn process_results_view(
    conn: &Connection,
    pack_id: &str,
    q: &ResultsQuery,
    kinds: &str,
    parsed: pack_host::result_columns::ParsedColumns,
) -> Response {
    let limit = q.limit.unwrap_or(200).clamp(1, 500);
    let offset = q.offset.unwrap_or(0).max(0);
    let (job_id, run_id) = (q.job_id, q.run_id);
    // 与判定分支同一取行口径：无范围过滤 = 每资产最新一行（工作版本会反复生成），
    // 指定 jobId/runId = 只看这一次执行写了什么（历史审计不被后续重跑覆盖）。
    let latest = if job_id.is_some() || run_id.is_some() {
        "1 = 1"
    } else {
        "v.versionId IN (SELECT MAX(versionId) FROM asset_versions \
         WHERE packId = ?1 GROUP BY assetId)"
    };
    // ?2 = 范围过滤 id（带范围时 latest 恒 1=1，?2 让位）/ 无范围 = latest 子查询的 packId
    let select_scope: Option<(&'static str, i64)> = match (job_id, run_id) {
        (Some(job), _) => Some((" AND v.jobId = ?2", job)),
        (None, Some(run)) => Some((" AND v.runId = ?2", run)),
        (None, None) => None,
    };
    let scope_clause = select_scope.as_ref().map(|(c, _)| *c).unwrap_or("");
    let total = match &select_scope {
        Some((_, id)) => conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM asset_versions v JOIN assets a ON a.assetId = v.assetId \
                 WHERE v.packId = ?1 AND {latest}{scope_clause}"
            ),
            rusqlite::params![pack_id, id],
            |r| r.get::<_, i64>(0),
        ),
        None => conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM asset_versions v JOIN assets a ON a.assetId = v.assetId \
                 WHERE v.packId = ?1 AND {latest}"
            ),
            rusqlite::params![pack_id],
            |r| r.get::<_, i64>(0),
        ),
    };
    let total = match total {
        Ok(n) => n,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "results", e.to_string()),
    };
    let sql = format!(
        "SELECT v.assetId, a.fileName, a.absPath, v.actions, v.result, v.errorMessage,
                v.path, v.sha256, v.bytes, v.format, v.width, v.height, v.assetName,
                v.chain, v.durationMs, v.createdAt
         FROM asset_versions v JOIN assets a ON a.assetId = v.assetId
         WHERE v.packId = ?1 AND {latest}{scope_clause}
         ORDER BY v.assetId, v.versionId LIMIT ?3 OFFSET ?4"
    );
    let params: Vec<Box<dyn rusqlite::ToSql>> = vec![
        Box::new(pack_id.to_string()),
        match &select_scope {
            Some((_, id)) => Box::new(*id) as Box<dyn rusqlite::ToSql>,
            None => Box::new(pack_id.to_string()) as Box<dyn rusqlite::ToSql>,
        },
        Box::new(limit),
        Box::new(offset),
    ];
    let mapped = conn.prepare(&sql).and_then(|mut stmt| {
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<i64>>(8)?,
                r.get::<_, Option<String>>(9)?,
                r.get::<_, Option<i64>>(10)?,
                r.get::<_, Option<i64>>(11)?,
                r.get::<_, Option<String>>(12)?,
                r.get::<_, String>(13)?,
                r.get::<_, i64>(14)?,
                r.get::<_, i64>(15)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok::<_, rusqlite::Error>(out)
    });
    let rows = match mapped {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "results", e.to_string()),
    };
    let mut items: Vec<serde_json::Value> = Vec::new();
    for (asset_id, file_name, abs_path, actions, result, error_message, path, sha256, bytes,
         format, width, height, asset_name, chain, duration_ms, ts) in rows
    {
        let chain_json: serde_json::Value = chain_view(&chain);
        // 链拼接当 ruleId 槽位（"resize" / "resize→convert"）—— 稳定可辨识的行 key
        let action_list: Vec<String> =
            serde_json::from_str::<Vec<String>>(&actions).ok().unwrap_or_default();
        let rule_id = action_list.join("→");
        // 产物（仅 ok 行有）：export_file 契约（kind/path/sha256 + 尺寸格式元数据）
        let artifacts = if result == "ok" {
            path.as_deref()
                .map(|p| {
                    vec![json!({
                        "kind": "export_file",
                        "path": p,
                        "sha256": sha256,
                        "bytes": bytes,
                        "format": format,
                        "width": width,
                        "height": height,
                    })]
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        items.push(json!({
            "assetId": asset_id,
            "fileName": file_name,
            "absPath": abs_path,
            "ruleId": rule_id,
            // 链按单资产执行（无组语义）
            "judgementScope": "per_asset",
            // 执行结果（ok|failed；failed 时 reason 指向失败动作）
            "result": result,
            // 处理类没有审核结论，恒 null（前端按 kinds 不画审核列）
            "verdict": serde_json::Value::Null,
            "reason": error_message,
            // 行业列字典：链证据原样搬出（宿主当不透明字典）
            "fields": {
                "chain": chain_json,
                "actions": actions,
                "assetName": asset_name,
                "durationMs": duration_ms,
                "format": format,
                "width": width,
                "height": height,
                "bytes": bytes,
            },
            "artifacts": artifacts,
            "ts": ts,
        }));
    }
    (
        StatusCode::OK,
        Json(json!({
            "packId": pack_id,
            "kinds": kinds_array(kinds),
            "jobId": job_id,
            "runId": run_id,
            "total": total,
            "limit": limit,
            "offset": offset,
            // null = 插件没声明列 → 前端用默认列（资产/执行结果/产物/备注）
            "columns": parsed.columns,
            "columnWarnings": parsed.warnings,
            "items": items,
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------- 反馈（误判）

/// POST /api/compliance/feedback —— 写一条人工反馈（机器结论由库里的判定行推导）。
pub async fn feedback_create(
    State(state): State<AppState>,
    Json(req): Json<FeedbackReq>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    let pack = plugin_meta_pack(&conn, &state.data_dir);
    let pack_id = verdict_pack_id(&conn, &state.data_dir);
    match svc_library::compliance_view::record_feedback(
        &conn,
        &pack_id,
        &pack,
        req.assetId,
        &req.ruleId,
        &req.humanResult,
        req.reason.as_deref(),
    ) {
        Ok(row) => (
            StatusCode::OK,
            Json(json!({
                "ok": true, "packId": pack_id,
                "feedback": {
                    "id": row.id, "assetId": row.asset_id, "ruleId": row.rule_id,
                    "machineResult": row.machine_result, "humanResult": row.human_result,
                    "reason": row.reason, "misjudged": row.is_misjudged(), "ts": row.ts,
                }
            })),
        )
            .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, "feedback_rejected", e.to_string()),
    }
}

/// GET /api/compliance/feedback —— 反馈列表（审计/界面回显）。
pub async fn feedback_list(
    State(state): State<AppState>,
    Query(q): Query<FeedbackQuery>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
        let pack_id = verdict_pack_id(&conn, &state.data_dir);
    match svc_library::compliance_view::list_feedback(
        &conn,
        &pack_id,
        q.assetId,
        q.ruleId.as_deref(),
        q.limit,
    ) {
        Ok(rows) => {
            let items: Vec<serde_json::Value> = rows
                .iter()
                .map(|f| {
                    json!({
                        "id": f.id, "assetId": f.asset_id, "ruleId": f.rule_id,
                        "machineResult": f.machine_result, "humanResult": f.human_result,
                        "reason": f.reason, "misjudged": f.is_misjudged(), "ts": f.ts,
                    })
                })
                .collect();
            (
                StatusCode::OK,
                Json(json!({ "total": items.len(), "packId": pack_id, "items": items })),
            )
                .into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "feedback", e.to_string()),
    }
}

/// GET /api/compliance/feedback/export —— 反馈 CSV（UTF-8 BOM，Excel 直开不乱码）。
pub async fn feedback_export(State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.db.lock().await;
        let pack_id = verdict_pack_id(&conn, &state.data_dir);
    let rows = match svc_library::compliance_view::list_feedback(&conn, &pack_id, None, None, None) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "feedback", e.to_string()),
    };
    let mut body = String::from("\u{feff}"); // BOM：Excel 识别 UTF-8
    body.push_str(&svc_library::compliance_view::feedback_csv(&rows));
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                &format!("attachment; filename=\"rule-feedback-{}.csv\"", pack_id) as &str,
            ),
        ],
        body,
    )
        .into_response()
}

// ---------------------------------------------------------------- 处理插件（管理面）

/// GET /api/compliance/packs —— **处理插件**清单（版本 / 运行态 / 签名 / 是否随产品分发）。
///
/// IP-K16：旧的两族并列（内置 YAML 行业包 + 代码插件包）已收口成一族 —— 一个行业的
/// 合规检测/处理 = 一个插件，它自带规则/词表/报告元数据。所以这里只剩插件视图。
pub async fn pack_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    (
        StatusCode::OK,
        Json(json!({
            "plugins": plugin_packs_view(&conn, &state.data_dir, locale_from_headers(&headers)),
            "pluginState": plugin_state(&conn, &state.data_dir),
            "signaturePolicy": policy_json(&pack_signature_policy_quiet(&conn)),
            "hostCapabilities": pack_host_caps(&conn),
        })),
    )
        .into_response()
}

/// GET /api/compliance/packs/{id} —— 插件详情（manifest + 自带的 `resources/rules.json`；
/// `?probe=1` 时再拉一次 `pack.info`，那会起子进程，缺省不拉）。
pub async fn pack_preview(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<PackPreviewQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match plugin_pack_detail(
        &conn,
        &state.data_dir,
        &id,
        q.probe.unwrap_or(false),
        &state.pack_runtime,
        locale_from_headers(&headers),
    ) {
        Some(detail) => (StatusCode::OK, Json(detail)).into_response(),
        None => err(
            StatusCode::NOT_FOUND,
            "plugin_pack_missing",
            format!("没有这个处理插件：{id}（GET /api/compliance/packs 看已装的）"),
        ),
    }
}

#[derive(Debug, Deserialize)]
pub struct PackPreviewQuery {
    /// 详情预览时是否拉一次 `pack.info`（会起子进程，缺省不拉）
    #[serde(default)]
    pub probe: Option<bool>,
}

// ---------------------------------------------------------------- IP-K08：插件包安装 / 验签 / 回滚
//
// 与上面的**配置包**（taxonomy/rules/pipeline/report 四件 YAML，id 形如 `ecommerce-taobao`）
// 是两族东西：那族是 legacy 内联引擎的配置面，这族是子进程**代码插件**的 `.dampack` 归档
// （id 形如 `com.soujpg.pack.ecommerce`）。两族 id 不同族，所以端点共存在
// `/compliance/packs/*` 下不会互相覆盖：配置包走 `activate` / `import`，插件包走
// `install` / `{id}/activate` / `{id}/rollback` / `DELETE {id}`。
//
// 分层：本层只做 HTTP 解析 + 落账本 + 切指针；解包/摘要/验签在
// `pack_host::registry`（可单测，不碰 DB），账本在 `core_storage::packs`（不拼 SQL）。

/// 信任公钥清单（settings `packs.trustedKeys`，hex 或 base64 的 ed25519 公钥数组）。
/// 空 = 未启用验签（开发/离线部署）；非空 = **强制验签**（缺 `manifest.sig` 直接拒装）。
pub const PACK_TRUSTED_KEYS_KEY: &str = "packs.trustedKeys";
/// 宿主授权能力子集（settings `packs.hostCapabilities`）。缺省 = 编译期全集。
/// 协议 §8：「声明了不代表一定有权限（部署方可只授权子集）」——这里就是那个子集的落点。
pub const PACK_HOST_CAPS_KEY: &str = "packs.hostCapabilities";
/// 随产品分发、**不允许卸载**的插件包 id（升级 = 装新版本 + 切 current）。
pub const BUILTIN_PLUGIN_IDS: [&str; 1] = [PLUGIN_ECOMMERCE_ID];

/// 安装期签名策略（settings 里配了信任公钥 → 强制验签）。
fn pack_signature_policy(conn: &Connection) -> Result<pack_host::SignaturePolicy, String> {
    let keys = core_storage::settings::get_json::<Vec<String>>(conn, PACK_TRUSTED_KEYS_KEY)
        .ok()
        .flatten()
        .unwrap_or_default();
    if keys.is_empty() {
        return Ok(pack_host::SignaturePolicy::permissive());
    }
    pack_host::SignaturePolicy::from_encoded(&keys)
}

/// 本部署**实际授权**的能力集合（编译期全集 ∩ settings 子集）。
fn pack_host_caps(conn: &Connection) -> Vec<String> {
    let all: Vec<String> = pack_host::default_host_capabilities()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let subset = core_storage::settings::get_json::<Vec<String>>(conn, PACK_HOST_CAPS_KEY)
        .ok()
        .flatten()
        .filter(|v| !v.is_empty());
    match subset {
        None => all,
        Some(sub) => all.into_iter().filter(|c| sub.contains(c)).collect(),
    }
}

fn pack_db_err(e: impl std::fmt::Display) -> (StatusCode, &'static str, String) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "packs_db",
        e.to_string(),
    )
}

fn install_status(e: &pack_host::RegistryError) -> StatusCode {
    match e {
        pack_host::RegistryError::VersionConflict { .. } => StatusCode::CONFLICT,
        pack_host::RegistryError::Io { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    }
}

/// 登记一个已落地的版本行。`current` 一律先写 false：唯一索引保证同 id 只能有一个 current，
/// 切指针统一走 `set_current`（两步由 DB 兜底，不靠应用层自觉）。
fn upsert_pack_row(conn: &Connection, inst: &pack_host::InstalledPack) -> Result<(), String> {
    use core_storage::packs::PackRepo as _;
    let repo = core_storage::packs::SqlitePackRepo::new(conn);
    let rec = core_storage::packs::PackRecord {
        id: inst.identity.id.clone(),
        version: inst.identity.version.clone(),
        dir: inst.dir.to_string_lossy().to_string(),
        manifest_hash: inst.identity.plugin_hash.clone(),
        api_version: inst.manifest.api_version.clone(),
        deterministic: inst.manifest.deterministic,
        signature_status: inst.signature_status.as_str().to_string(),
        kinds: inst.manifest.kinds_canonical(),
        installed_at: unix_now(),
        current: false,
    };
    repo.upsert(&rec).map(|_| ()).map_err(|e| e.to_string())
}

/// `kinds` 规范串 → 前端数组（`judge` / `process`；空串兜底成 `["judge"]`，
/// 与 manifest 未声明 kinds 的旧包同口径）。
fn kinds_json(kinds: &str) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for part in kinds.split(',').map(str::trim) {
        match part {
            "judge" => out.push("judge"),
            "process" => out.push("process"),
            _ => {}
        }
    }
    if out.is_empty() {
        out.push("judge");
    }
    out
}

/// i18n-F：请求语言归一。`Accept-Language` 首个标签：`zh*` → `zh-Hans`、`en*` → `en-US`，
/// 其余/缺失 → `zh-Hans`（包文案的事实源语言，识别不了不猜）。
pub(crate) fn normalize_locale(raw: &str) -> &'static str {
    let first = raw.split(',').next().unwrap_or("").trim().to_lowercase();
    if first.starts_with("zh") {
        "zh-Hans"
    } else if first.starts_with("en") {
        "en-US"
    } else {
        "zh-Hans"
    }
}

/// i18n-F：请求语言（前端 `api.ts` 统一注入 `Accept-Language`；同时响应里带 `*I18n`
/// 原样对象，前端切语言即时重渲染不重拉）。
pub(crate) fn locale_from_headers(headers: &HeaderMap) -> &'static str {
    normalize_locale(
        headers
            .get(header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
    )
}

/// `pack_display_meta` 的「包不可解析」四元组（全 null，界面退显 id）。
fn pack_display_meta_none() -> (
    serde_json::Value,
    serde_json::Value,
    serde_json::Value,
    serde_json::Value,
) {
    (
        serde_json::Value::Null,
        serde_json::Value::Null,
        serde_json::Value::Null,
        serde_json::Value::Null,
    )
}

/// 包目录 → 界面用的 `(能力名, 一句话描述, 名字 i18n 原样对象, 描述 i18n 原样对象)`
/// （DM-B1 / KI-36 + i18n-F）。
///
/// 名字来源是**包自己**：`short_name` 优先（§3 功能名），缺省回落 manifest `name`
/// （老包形态）—— 宿主不认 id、不维护映射表。包不在 / manifest 坏 → 四个都 null，
/// 界面自己退 id（不编名字，缺口要一眼可见）。i18n 原样对象只在包写的是
/// `{"locale": 文案}` 对象形态时非 null（字符串形态 = 全语言通用，前端直接用名字）。
fn pack_display_meta(
    dir: &Path,
    locale: &str,
) -> (
    serde_json::Value,
    serde_json::Value,
    serde_json::Value,
    serde_json::Value,
) {
    match pack_host::load_pack(dir) {
        Ok((m, _)) => (
            json!(m.short_label(locale)),
            json!(m.summary_text(locale)),
            m.label_i18n().map(|t| json!(t)).unwrap_or(serde_json::Value::Null),
            m.summary_i18n().map(|t| json!(t)).unwrap_or(serde_json::Value::Null),
        ),
        Err(_) => pack_display_meta_none(),
    }
}

/// 账本行 → 前端视图（`present` = 版本目录还在）。
fn pack_row_json(rec: &core_storage::packs::PackRecord, locale: &str) -> serde_json::Value {
    let dir = std::path::PathBuf::from(&rec.dir);
    let present = dir.join("manifest.json").is_file();
    let (name, _, _, _) = if present {
        pack_display_meta(&dir, locale)
    } else {
        pack_display_meta_none()
    };
    json!({
        "name": name,
        "version": rec.version,
        "dir": rec.dir,
        "present": present,
        "manifestHash": rec.manifest_hash,
        "apiVersion": rec.api_version,
        "deterministic": rec.deterministic,
        "signatureStatus": rec.signature_status,
        // 插件种类（N5b）：界面据此决定画不画审核列 —— 不靠猜
        "kinds": kinds_json(&rec.kinds),
        "installedAt": rec.installed_at,
        "current": rec.current && present,
        "registered": true,
    })
}

/// 插件包清单（`GET /packs` 的 `plugins` 段）：版本 / 运行态 / 签名状态 / 是否内置。
///
/// 口径：
/// * `versions` = 账本行 ∪ 磁盘版本目录（`present` 区分在不在）；**卸载 = 删目录**，
///   账本行作为审计保留（0026：`installed_at` 是「何时进过这个库」），所以不会假存在；
/// * `currentVersion` 只在该版本目录还在时才给（否则填 `staleCurrent`，界面据此提示重装）；
/// * `runtime` 与 `/compliance/status.pluginState` 同源语义：包在 + manifest 解析得开 = available。
fn plugin_packs_view(conn: &Connection, data_dir: &Path, locale: &str) -> Vec<serde_json::Value> {
    use core_storage::packs::PackRepo as _;
    let store = pack_host::PackStore::new(data_dir);
    let rows = core_storage::packs::SqlitePackRepo::new(conn)
        .list()
        .unwrap_or_default();
    let mut ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
    ids.extend(store.installed_ids());
    ids.sort();
    ids.dedup();
    let mut out = Vec::new();
    for id in ids {
        let mut versions: Vec<serde_json::Value> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        for rec in rows.iter().filter(|r| r.id == id) {
            versions.push(pack_row_json(rec, locale));
            seen.push(rec.version.clone());
        }
        for ver in store.installed_versions(&id) {
            if seen.iter().any(|s| s == &ver) {
                continue;
            }
            let dir = store.version_dir(&id, &ver);
            let (hash, api, det, kinds, name) = match pack_host::load_pack(&dir) {
                Ok((m, ident)) => (
                    json!(ident.plugin_hash),
                    json!(m.api_version),
                    json!(m.deterministic),
                    json!(m.kinds_canonical()),
                    json!(m.short_label(locale)),
                ),
                Err(_) => (
                    serde_json::Value::Null,
                    serde_json::Value::Null,
                    serde_json::Value::Null,
                    serde_json::Value::Null,
                    serde_json::Value::Null,
                ),
            };
            versions.push(json!({
                "version": ver, "dir": dir.to_string_lossy(), "present": true,
                "name": name,
                "manifestHash": hash, "apiVersion": api, "deterministic": det,
                "kinds": kinds,
                "signatureStatus": serde_json::Value::Null, "installedAt": serde_json::Value::Null,
                "current": false, "registered": false,
            }));
        }
        let current_row = rows.iter().find(|r| r.id == id && r.current);
        let current_version = current_row
            .map(|r| r.version.clone())
            .filter(|v| store.version_dir(&id, v).join("manifest.json").is_file());
        let stale = current_row
            .map(|r| r.version.clone())
            .filter(|v| Some(v) != current_version.as_ref());
        let runtime = plugin_state_for(conn, data_dir, &id);
        // 能力名/一句话描述（DM-B1 / KI-36）：**包自己给**，宿主不维护 id→名字 映射。
        // 包目录解析口径与 runtime 同一处（`plugin_pack_dir_for`：账本 current → 已装目录 →
        // 仓库内置兜底），所以「可用」的行一定带得到名字。
        let (name, summary, name_i18n, summary_i18n) =
            match plugin_pack_dir_for(conn, data_dir, &id) {
                Some(dir) => pack_display_meta(&dir, locale),
                None => pack_display_meta_none(),
            };
        out.push(json!({
            "id": id,
            "name": name,
            "summary": summary,
            // i18n-F：原样对象（包写对象形态时才有；前端 pickI18n 切语言即时生效）
            "nameI18n": name_i18n,
            "summaryI18n": summary_i18n,
            "builtin": BUILTIN_PLUGIN_IDS.contains(&id.as_str()),
            "currentVersion": current_version,
            "staleCurrent": stale,
            "versions": versions,
            "runtime": runtime,
        }));
    }
    out
}

/// `GET /packs/{id}` 的插件分支：manifest + 规则文档（`resources/rules.json` 静态读；
/// `?probe=1` 时再拉一次 `pack.info`，那是包自己报的规则清单）。
fn plugin_pack_detail(
    conn: &Connection,
    data_dir: &Path,
    id: &str,
    probe: bool,
    rt: &crate::PackRuntime,
    locale: &str,
) -> Option<serde_json::Value> {
    use core_storage::packs::PackRepo as _;
    let store = pack_host::PackStore::new(data_dir);
    let repo = core_storage::packs::SqlitePackRepo::new(conn);
    let rows = repo.list().ok()?;
    let row_versions: Vec<String> = rows.iter().filter(|r| r.id == id).map(|r| r.version.clone()).collect();
    let dir_versions = store.installed_versions(id);
    if row_versions.is_empty() && dir_versions.is_empty() && !BUILTIN_PLUGIN_IDS.contains(&id) {
        return None;
    }
    let current_version = repo.current(id).ok().flatten().map(|r| r.version);
    // 预览哪个版本：current（目录在）→ 最新装好的 → 内置仓库包
    let target_dir = current_version
        .as_ref()
        .map(|v| store.version_dir(id, v))
        .filter(|d| d.join("manifest.json").is_file())
        .or_else(|| dir_versions.last().map(|v| store.version_dir(id, v)))
        .or_else(|| {
            if id == PLUGIN_ECOMMERCE_ID {
                plugin_pack_dir_for(conn, data_dir, id)
            } else {
                None
            }
        });
    let (manifest, ident, dir) = match target_dir {
        Some(dir) => match pack_host::load_pack(&dir) {
            Ok((m, i)) => (m, i, dir),
            Err(e) => {
                return Some(json!({
                    "id": id, "source": "plugin", "active": false, "builtin": BUILTIN_PLUGIN_IDS.contains(&id),
                    "manifest": serde_json::Value::Null,
                    "error": format!("包目录 {} 的 manifest 不可解析：{e}", dir.display()),
                }))
            }
        },
        None => {
            return Some(json!({
                "id": id, "source": "plugin", "active": false, "builtin": BUILTIN_PLUGIN_IDS.contains(&id),
                "manifest": serde_json::Value::Null, "error": "plugin_pack_missing",
            }))
        }
    };
    let rules = std::fs::read_to_string(dir.join("resources").join("rules.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("rules").cloned());
    let (info, info_err) = if probe {
        let work = data_dir.join("dam").join("pack-probe");
        match probe_pack_info(&dir, &work, rt) {
            Ok(v) => (v, serde_json::Value::Null),
            Err(e) => (serde_json::Value::Null, json!(e)),
        }
    } else {
        (serde_json::Value::Null, serde_json::Value::Null)
    };
    Some(json!({
        "id": id,
        "source": "plugin",
        "builtin": BUILTIN_PLUGIN_IDS.contains(&id),
        "active": current_version.as_deref() == Some(ident.version.as_str()),
        "currentVersion": current_version,
        "version": ident.version,
        "dir": dir.to_string_lossy(),
        "manifest": manifest,
        "pluginHash": ident.plugin_hash,
        "signatureStatus": if manifest.signature.is_some() { "declared" } else { "unsigned" },
        "signaturePolicy": policy_json(&pack_signature_policy_quiet(conn)),
        "rules": rules,
        "packInfo": info,
        "packInfoError": info_err,
        "versions": plugin_packs_view(conn, data_dir, locale).into_iter().find(|v| v["id"] == id).map(|v| v["versions"].clone()).unwrap_or(json!([])),
        "error": serde_json::Value::Null,
    }))
}

/// 读策略时把「配错公钥」压成 permissive + 打日志：只读接口不该因为配置脏就 500
/// （安装接口那边必须硬报错，那里才 decision）。
fn pack_signature_policy_quiet(conn: &Connection) -> pack_host::SignaturePolicy {
    pack_signature_policy(conn).unwrap_or_else(|_| pack_host::SignaturePolicy::permissive())
}

fn policy_json(p: &pack_host::SignaturePolicy) -> serde_json::Value {
    json!({ "required": p.requires_signature(), "trustedKeys": p.trusted_keys.len() })
}

/// 拉一次 `pack.info`（**会起子进程**，所以只在 `?probe=1` 时调）。
fn probe_pack_info(
    pack_dir: &Path,
    work_dir: &Path,
    rt: &crate::PackRuntime,
) -> Result<serde_json::Value, String> {
    let (manifest, _ident) = pack_host::load_pack(pack_dir).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(work_dir).map_err(|e| format!("建探针工作目录失败：{e}"))?;
    let gw_cfg = pack_host::GatewayConfig::from_manifest(&manifest, work_dir)
        .with_vl(Arc::new(NoVl));
    let gateway = pack_host::Gateway::start(gw_cfg).map_err(|e| format!("能力网关启动失败：{e}"))?;
    let cfg = pack_host_config(gateway.base_url(), gateway.token(), work_dir, rt);
    let host = pack_host::PackHost::open(pack_dir, cfg).map_err(|e| format!("插件宿主打开失败：{e}"))?;
    host.call_with_timeout("pack.info", json!({}), std::time::Duration::from_secs(10))
        .map_err(|e| format!("pack.info 失败：{e}"))
}

#[derive(Debug, Deserialize)]
pub struct InstallReq {
    /// 服务端可见的 `.dampack` 路径（桌面/容器同机；CLI 与自动化测试走这条）
    #[serde(default)]
    pub path: Option<String>,
    /// 归档字节（base64）——浏览器把 `<input type=file>` 读成 base64 POST 上来
    #[serde(default, rename = "archiveBase64")]
    pub archive_base64: Option<String>,
    /// 上传文件名（只用于错误信息）
    #[serde(default)]
    pub filename: Option<String>,
    /// 装完是否切 current。缺省：库里还没有该包的生效版本才切（升级/补装旧版本不抢指针）
    #[serde(default)]
    pub activate: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub struct VersionReq {
    /// 目标版本（缺省：activate = 最新已装；rollback = 上一个已装版本）
    #[serde(default)]
    pub version: Option<String>,
}

/// 装包时用**内置 runtime** 给包建独立 venv（`<pack>/venv`）——KI-28 与「每包独立 venv」的接缝。
///
/// 为什么装时建：① 首判不该为了建环境卡几十秒；② 「哪个解释器建的 venv」是一次性事实，
/// 运行时只负责优先探测它（`pack_host::process::venv_python`，探测逻辑不在这里重写）。
///
/// 失败**不阻塞安装**：插件仍能用内置 runtime 跑（venv 只用于包内私有依赖），
/// 但必须把原因回给调用方 —— 静默失败会让用户以为隔离生效了（下一个真问题会以「缺依赖」形式炸）。
fn ensure_pack_venv(pack_dir: &Path, rt: &crate::PackRuntime) -> serde_json::Value {
    use pack_host::process::venv_python;
    let venv = pack_dir.join("venv");
    let dir = venv.to_string_lossy().to_string();
    if let Some(existing) = venv_python(pack_dir) {
        return json!({ "created": false, "reused": true, "dir": dir, "python": existing.to_string_lossy() });
    }
    let Some(python) = rt.python.as_ref() else {
        // 容器/开发形态没有内置 runtime：不猜系统 python 去建（那会把开发机环境烙进包里）
        return json!({
            "created": false,
            "reused": false,
            "dir": dir,
            "skipped": "container_form",
            "note": "容器/开发形态无内置 python 运行时，跳过 per-pack venv（插件走系统 python3）",
        });
    };
    let mut venv_cmd = std::process::Command::new(python);
    venv_cmd.arg("-m").arg("venv").arg(&venv);
    // KI-42：Windows 上不弹控制台黑窗（首次准备插件运行环境时用户不该看到终端窗口）
    pack_host::process::no_console_window(&mut venv_cmd);
    match venv_cmd.output() {
        Ok(out) if out.status.success() => {
            json!({ "created": true, "reused": false, "dir": dir, "python": python.to_string_lossy() })
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let tail: Vec<&str> = stderr.lines().rev().take(3).collect();
            json!({
                "created": false,
                "reused": false,
                "dir": dir,
                "error": format!(
                    "python -m venv 失败（exit={:?}）：{}",
                    out.status.code(),
                    tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
                ),
            })
        }
        Err(e) => json!({
            "created": false,
            "reused": false,
            "dir": dir,
            "error": format!("拉起内置 runtime 失败（{}）：{e}", python.display()),
        }),
    }
}

/// POST /api/compliance/packs/install —— `.dampack` 安装（校验顺序见 `pack_host::registry`）。
pub async fn pack_install(
    State(state): State<AppState>,
    Json(req): Json<InstallReq>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    let policy = match pack_signature_policy(&conn) {
        Ok(p) => p,
        Err(e) => return err(StatusCode::BAD_REQUEST, "pack_signature_policy", e),
    };
    let caps = pack_host_caps(&conn);
    let cap_refs: Vec<&str> = caps.iter().map(String::as_str).collect();
    let store = pack_host::PackStore::new(&state.data_dir);

    let installed = if let Some(p) = req.path.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        store.install(Path::new(p), &policy, &cap_refs)
    } else if let Some(b64) = req.archive_base64.as_deref() {
        match pack_host::decode_base64_archive(b64) {
            Ok(bytes) => store.install_bytes(&bytes, &policy, &cap_refs),
            Err(e) => return err(StatusCode::BAD_REQUEST, "pack_archive", e),
        }
    } else {
        return err(
            StatusCode::BAD_REQUEST,
            "pack_install_args",
            "必须给 path（服务端文件）或 archiveBase64（归档字节）",
        );
    };
    let installed = match installed {
        Ok(i) => i,
        Err(e) => return err(install_status(&e), e.code(), e.to_string()),
    };
    if let Err(e) = upsert_pack_row(&conn, &installed) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "packs_db", e);
    }
    // KI-28 + 2026-10-20 运行时裁决：装包时用内置 runtime 建**每包独立 venv**（失败不阻塞安装）
    let venv = ensure_pack_venv(&installed.dir, &state.pack_runtime);
    // 切指针：只在「还没有生效版本」或显式 activate=true 时切（补装旧版本不该抢指针）
    use core_storage::packs::PackRepo as _;
    let repo = core_storage::packs::SqlitePackRepo::new(&conn);
    let prev = match repo.current(&installed.identity.id) {
        Ok(v) => v.map(|r| r.version),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "packs_db", e.to_string()),
    };
    let activate = req.activate.unwrap_or(prev.is_none());
    let mut current_version = prev.clone();
    if activate {
        match repo.set_current(&installed.identity.id, &installed.identity.version) {
            Ok(0) => {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "packs_db",
                    "切 current 未命中任何行（账本与目录不一致）",
                )
            }
            Ok(_) => current_version = Some(installed.identity.version.clone()),
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "packs_db", e.to_string()),
        }
    }
    // KI-40：装包 = 「能力出现」的语义事件 → 官方默认方案随装包派生。
    // `ensure_legacy_specs` 幂等（缺哪条补哪条、不覆盖用户改动）；装包后显式跑一遍，
    // 不依赖 `compliance.specsMigrated` 一次性标记（老库迁过就再也不补，装新包出不来方案）。
    // 失败不阻塞安装（包本体已装好），错误透出到响应 note 供排障。
    let seed_note = match crate::spec_seed::ensure_legacy_specs(&conn, &state.data_dir) {
        Ok(report) => {
            if report.created.is_empty() {
                None
            } else {
                Some(format!(
                    "已派生官方默认方案：{}",
                    report
                        .created
                        .iter()
                        .map(|c| c.name.clone())
                        .collect::<Vec<_>>()
                        .join("、")
                ))
            }
        }
        Err(e) => Some(format!("官方默认方案派生失败（可到 /api/specs/migrate 重跑）：{e}")),
    };
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "id": installed.identity.id,
            "version": installed.identity.version,
            "dir": installed.dir.to_string_lossy(),
            "manifestHash": installed.identity.plugin_hash,
            "apiVersion": installed.manifest.api_version,
            "deterministic": installed.manifest.deterministic,
            "capabilities": installed.manifest.capabilities,
            "signatureStatus": installed.signature_status.as_str(),
            "signaturePolicy": policy_json(&policy),
            "digestsChecked": installed.digests_checked,
            "reused": installed.reused,
            "venv": venv,
            "activated": activate,
            "previousVersion": prev,
            "currentVersion": current_version,
            "seedNote": seed_note,
            "filename": req.filename,
            "source": if req.path.is_some() { "path" } else { "upload" },
        })),
    )
        .into_response()
}

/// 切 current 指针（activate 与 rollback 共用骨架）。
///
/// 切之前三件核对：① 目标版本目录在；② 目录里的 manifest 解析得开；③ 与账本登记的
/// `manifest_hash` 一致（**装完被改过 = 拒绝切换**，这是「已发布版本目录永不就地修改」的
/// 唯一可执行检查）。
fn switch_current(
    conn: &Connection,
    data_dir: &Path,
    id: &str,
    req_version: Option<&str>,
    mode: &str,
) -> Result<serde_json::Value, (StatusCode, &'static str, String)> {
    use core_storage::packs::PackRepo as _;
    let store = pack_host::PackStore::new(data_dir);
    let repo = core_storage::packs::SqlitePackRepo::new(conn);
    let versions = store.installed_versions(id);
    if versions.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            "pack_not_installed",
            format!("包 {id} 没有装任何版本（先 POST /compliance/packs/install）"),
        ));
    }
    let current = repo.current(id).map_err(pack_db_err)?.map(|r| r.version);
    let target = match req_version {
        Some(v) => {
            if !versions.iter().any(|x| x == v) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "pack_version_not_installed",
                    format!("{id}@{v} 未安装（已装：{versions:?}）"),
                ));
            }
            v.to_string()
        }
        None if mode == "rollback" => {
            let cur = current.clone().ok_or((
                StatusCode::NOT_FOUND,
                "pack_no_current_version",
                format!("{id} 当前没有生效版本，无可回滚"),
            ))?;
            versions
                .iter()
                .filter(|v| pack_host::compare_versions(v, &cur) == std::cmp::Ordering::Less)
                .next_back()
                .cloned()
                .ok_or((
                    StatusCode::NOT_FOUND,
                    "pack_no_previous_version",
                    format!(
                        "{id} 没有更早的版本可回滚（当前 {cur}，已装：{versions:?}）"
                    ),
                ))?
        }
        None => versions.last().cloned().unwrap_or_default(),
    };
    let dir = store.version_dir(id, &target);
    let (manifest, ident) = pack_host::load_pack(&dir).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            "pack_load",
            format!("版本目录 {} 不可解析：{e}", dir.display()),
        )
    })?;
    let rows = repo.list().map_err(pack_db_err)?;
    match rows.iter().find(|r| r.id == id && r.version == target) {
        Some(r) if r.manifest_hash != ident.plugin_hash => {
            return Err((
                StatusCode::CONFLICT,
                "pack_tampered",
                format!(
                    "{id}@{target} 的目录内容与登记不符（登记 {} / 实物 {}）：拒绝切换，请重装该版本",
                    r.manifest_hash, ident.plugin_hash
                ),
            ))
        }
        Some(_) => {}
        None => {
            // 目录在但没登记（手工拷进来的）→ 补登记（按 manifest 现算身份）
            let rec = core_storage::packs::PackRecord {
                id: ident.id.clone(),
                version: ident.version.clone(),
                dir: dir.to_string_lossy().to_string(),
                manifest_hash: ident.plugin_hash.clone(),
                api_version: manifest.api_version.clone(),
                deterministic: manifest.deterministic,
                signature_status: if manifest.signature.is_some() {
                    "declared".to_string()
                } else {
                    "unsigned".to_string()
                },
                kinds: manifest.kinds_canonical(),
                installed_at: unix_now(),
                current: false,
            };
            repo.upsert(&rec).map_err(pack_db_err)?;
        }
    }
    let n = repo.set_current(id, &target).map_err(pack_db_err)?;
    if n == 0 {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "packs_db",
            "切 current 未命中任何行（账本与目录不一致）".to_string(),
        ));
    }
    Ok(json!({
        "ok": true,
        "id": id,
        "mode": mode,
        "from": current,
        "to": target,
        "version": target,
        "dir": dir.to_string_lossy(),
        "pluginHash": ident.plugin_hash,
        "apiVersion": manifest.api_version,
        "deterministic": manifest.deterministic,
        "capabilities": manifest.capabilities,
        "installedVersions": versions,
    }))
}

/// POST /api/compliance/packs/{id}/activate —— 把 current 切到指定/最新版本。
pub async fn pack_activate_version(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<VersionReq>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match switch_current(&conn, &state.data_dir, &id, req.version.as_deref(), "activate") {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err((code, word, msg)) => err(code, word, msg),
    }
}

/// POST /api/compliance/packs/{id}/rollback —— 回滚 = 切指针（旧版本目录保留）。
pub async fn pack_rollback(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<VersionReq>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match switch_current(&conn, &state.data_dir, &id, req.version.as_deref(), "rollback") {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err((code, word, msg)) => err(code, word, msg),
    }
}

#[derive(Debug, Deserialize)]
pub struct VersionQuery {
    #[serde(default)]
    pub version: Option<String>,
}

/// DELETE /api/compliance/packs/{id} —— 卸载（只删版本目录；账本行作为审计保留）。
///
/// 三条硬规则：内置包不许卸；正在生效的版本不许单独删（先切走）；版本目录不存在 → 404。
pub async fn pack_uninstall(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<VersionQuery>,
) -> impl IntoResponse {
    if BUILTIN_PLUGIN_IDS.contains(&id.as_str()) {
        return err_params(
            StatusCode::FORBIDDEN,
            codes::PACK_BUILTIN_PROTECTED,
            json!({ "id": id }),
            format!("{id} 是随产品分发的内置包，不可卸载（升级 = 装新版本 + 切 current）"),
        );
    }
    let conn = state.db.lock().await;
    use core_storage::packs::PackRepo as _;
    let repo = core_storage::packs::SqlitePackRepo::new(&conn);
    let store = pack_host::PackStore::new(&state.data_dir);
    let installed = store.installed_versions(&id);
    if installed.is_empty() {
        return err_params(
            StatusCode::NOT_FOUND,
            codes::PACK_NOT_INSTALLED,
            json!({ "id": id }),
            format!("包 {id} 没有已安装的版本"),
        );
    }
    let current = match repo.current(&id) {
        Ok(v) => v.map(|r| r.version),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "packs_db", e.to_string()),
    };
    let targets: Vec<String> = match q.version.as_deref() {
        Some(v) => {
            if !installed.iter().any(|x| x == v) {
                return err_params(
                    StatusCode::NOT_FOUND,
                    codes::PACK_VERSION_NOT_INSTALLED,
                    json!({ "id": id, "version": v, "installed": installed }),
                    format!("{id}@{v} 未安装（已装：{installed:?}）"),
                );
            }
            if current.as_deref() == Some(v) {
                return err_params(
                    StatusCode::CONFLICT,
                    codes::PACK_VERSION_CURRENT,
                    json!({ "id": id, "version": v }),
                    format!("{id}@{v} 是当前生效版本：先 activate 到别的版本再卸载它"),
                );
            }
            vec![v.to_string()]
        }
        None => installed.clone(),
    };
    let mut removed = Vec::new();
    for v in &targets {
        match store.uninstall_version(&id, v) {
            Ok(true) => removed.push(v.clone()),
            Ok(false) => {}
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "io", e.to_string()),
        }
    }
    let remaining = store.installed_versions(&id);
    let current_present = current
        .as_ref()
        .filter(|c| remaining.iter().any(|r| r == *c))
        .cloned();
    let stale_current = current
        .as_ref()
        .filter(|c| !remaining.iter().any(|r| r == *c))
        .cloned();
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "id": id,
            "removed": removed,
            "remaining": remaining,
            "currentVersion": current_present,
            "staleCurrent": stale_current,
            "note": "账本行作为审计保留（installed_at = 该版本何时进过这个库）；已删目录的版本不再算「已安装」",
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------- 判定触发（后台队列）

/// 判定运行状态：进程级单槽（与 `tag_api::TagState` / `ocr_api::OcrState` 同语义）——
/// 同一时刻只跑一轮，`running` 是互斥旗标；进度/汇总/错误都是"最近一轮"快照，
/// 前端 1s 轮询 `/judge/status` 取最新值。跨重启不留状态：队列本身在 `job_queue` 里
/// （断点续跑由 `requeue_stale` 兜底），进程态只服务这一个界面的进度条。
pub struct ComplianceRunState {
    pub running: AtomicBool,
    pub cancel: Arc<AtomicBool>,
    /// 逐 job 进度快照（`ComplianceProgress`：jobId/assetId/status/index/total/error）
    pub progress: Mutex<Option<svc_pipeline::ComplianceProgress>>,
    /// 最近一轮运行汇总（资产口径 + 组级口径 + 失败明细）
    pub report: Mutex<Option<svc_pipeline::ComplianceRunReport>>,
    pub error: Mutex<Option<String>>,
}

impl ComplianceRunState {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Mutex::new(None),
            report: Mutex::new(None),
            error: Mutex::new(None),
        }
    }
}

impl Default for ComplianceRunState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
pub struct JudgeReq {
    /// 显式资产集合（缺省 + 未给 `all` → 只判当前包下「未判定」的资产，即增量）
    #[serde(default)]
    pub assetIds: Option<Vec<i64>>,
    /// true = 全库重判（`deleted = 0` 的全部资产；旧判定不删，新判定行按 ts 覆盖"最新"口径）
    #[serde(default)]
    pub all: Option<bool>,
    /// 判哪个包（缺省 = 当前激活包）。切换判定口径不会动历史判定行。
    #[serde(default)]
    pub packId: Option<String>,
    /// asset（只逐图）| group（只组级）| both（缺省：逐图 + 组级一起入队）
    #[serde(default)]
    pub scope: Option<String>,
    /// 本轮最多入队 N 张（0/缺省 = 不限）；剩余留待下次，队列可续跑
    #[serde(default)]
    pub limit: Option<usize>,
    /// 并行 worker 数（缺省 4，clamp 1..16）——VL 并发即 worker 并发
    #[serde(default)]
    pub workers: Option<usize>,
    /// K07-I：**规格层**参数 diff（用户在插件详情页建的规格 = 参数集 + 动作链 + 输出）。
    /// 只放用户改过的键；缺省/`null` = 空层。
    #[serde(default)]
    pub specOptions: Option<serde_json::Value>,
    /// K07-I：**本任务覆盖**层（试跑「先跑 1 张」时当场调的参数）。
    /// 合并序：`插件默认值 ⊕ specOptions ⊕ options`（后写赢）。
    #[serde(default)]
    pub options: Option<serde_json::Value>,
    /// N6e：**动作链**（顺序即执行序，如 `["resize","convert","watermark"]`）。
    ///
    /// 给了它 = 这次 job 走「处理类插件动作链」：`packId` 必须点名**插件包 id**
    /// （`com.soujpg.pack.imaging`，处理类包没有配置包对应物），`assetIds` 必须显式列出
    /// （不提供「全库批量处理」），每张图产出**一张**工作版本。缺省 = 既有判定/修复语义。
    #[serde(default)]
    pub actions: Option<Vec<String>>,
    /// N1：引用的**规格**（`plugin_spec.specId`，替代临时参数堆）。
    ///
    /// 给了它就忽略 `specOptions`（规格是持久化的那一层，两边同时给等于没说清谁赢）：
    /// 规格贡献 `paramsDiff`（插件参数层）与 `thresholds`（规则级阈值）+ `rules`（判哪些规则），
    /// 生效参数快照里额外记一份 `spec` 段（重跑能说清「用哪条规格跑的」）。
    #[serde(default)]
    pub specId: Option<i64>,
}

/// 判定候选资产集合（按 mode 三选一；`all` 与显式 assetIds 都由 SQL 排除已删/失联）。
fn judge_candidates(
    conn: &Connection,
    pack_id: &str,
    req: &JudgeReq,
) -> Result<Vec<i64>, String> {
    let mut ids: Vec<i64> = if let Some(explicit) = &req.assetIds {
        let mut stmt = conn
            .prepare("SELECT assetId FROM assets WHERE deleted = 0 AND assetId = ?1")
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for id in explicit {
            if stmt.exists([id]).map_err(|e| e.to_string())? {
                out.push(*id);
            }
        }
        out
    } else if req.all.unwrap_or(false) {
        let mut stmt = conn
            .prepare("SELECT assetId FROM assets WHERE deleted = 0 ORDER BY assetId")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| r.get::<_, i64>(0))
            .map_err(|e| e.to_string())?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| e.to_string())?
    } else {
        svc_library::compliance_view::asset_ids_by_state(
            conn,
            pack_id,
            svc_library::compliance_view::AssetComplianceState::Unjudged,
        )
        .map_err(|e| e.to_string())?
    };
    ids.sort_unstable();
    ids.dedup();
    if let Some(limit) = req.limit {
        if limit > 0 {
            ids.truncate(limit);
        }
    }
    Ok(ids)
}

/// VL 通道配置：复用**打标 provider**（同一 OpenAI 兼容端点，设置页一处配置）。
///
/// 为什么不是独立设置项：C5 的 vl 通道与打标走同一类 chat/completions 端点（送图口径也同一套
/// `engine-tag::vl_image`），再开一套 VL key 只会让用户配两遍。
///
/// 判定档位（与 `infer::build_tagger` 同口径，不许出现"打标关了但 VL 还在偷偷发请求"）：
/// - `tag.enabled == false`（用户在设置页关了 AI 打标）→ `None` → vl 规则按设计
///   `skipped(executor_not_available)`，判定不会假绿；
/// - 端点配置不全（base_url/model 空）→ `None`，同上；
/// - 配了但**端点连不上**（网络/鉴权）→ 照常发请求并让 job 失败重试（响亮），
///   不降级成 skipped —— 那是"配错了"，不是"没配"。
fn vl_config(conn: &Connection) -> Option<svc_pipeline::VlConfig> {
    let s = crate::infer::load_settings(conn);
    let tag = &s.tag;
    if !tag.enabled || !tag.configured() {
        return None;
    }
    Some(svc_pipeline::VlConfig {
        base_url: tag.base_url.clone(),
        // api_key 原样取用（空 = 本地端点无需 key）
        api_key: tag.api_key.clone(),
        model: tag.model.clone(),
        timeout_secs: (tag.timeout_ms / 1000).max(1),
        max_tokens: tag.max_tokens,
        proxy: tag.proxy.clone(),
    })
}

/// 产物目录：修复/变换产物落 `{data_dir}/compliance/fixes`（不入库，复检与导出用）。
fn fixes_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("compliance").join("fixes")
}

// ---------------------------------------------------------------------------
// IP-K05：插件执行通道（L3 装配；端口在 svc-pipeline::PackJudge）
// ---------------------------------------------------------------------------

/// 官方电商插件的 manifest id（`packs/plugins/ecommerce/manifest.json`）。
///
/// 注意它与**内置 legacy 包 id**（`ecommerce-taobao`）不是同一个东西：前者是代码插件身份
/// （判定行 `packId` 写它 + 0027 来源列），后者是 `pipeline.yaml` 时代的配置包。两族判定行
/// 用 packId 天然隔离，A/B 对照时互不覆盖。
pub const PLUGIN_ECOMMERCE_ID: &str = "com.soujpg.pack.ecommerce";

/// 房产合规插件（`packs/plugins/realestate/manifest.json`）的 manifest id。
///
/// 它**不在** `BUILTIN_PLUGIN_IDS` 里：产品默认只带一个合规检测插件（电商），房产包按
/// 行业场景安装（`.dampack`）。但规格的 `pluginId` 必须写这个 id —— plugin 通道按规格
/// 解析包目录（KI-31），写配置包 id `realestate` 会解析不到代码插件。
pub const PLUGIN_REALESTATE_ID: &str = "com.soujpg.pack.realestate";

/// 插件包目录（三级解析，缺一不可，顺序固定）：
/// 1. `DAM_PACK_DIR`（显式指定，测试/排障用）；
/// 2. `{data_dir}/packs/<id>`（发行形态：安装流程把包解到这里，K07 落地）；
/// 3. 仓库内置路径 `packs/plugins/ecommerce`（容器/开发态，`cargo test` 同一份）。
///
/// 三级都没有 → `None`：引擎切到 plugin 时**报错**，绝不静默回退 legacy
/// （静默回退会让人以为插件在跑，实际判定还是老引擎出的）。
fn plugin_pack_dir(conn: &Connection, data_dir: &Path) -> Option<PathBuf> {
    plugin_pack_dir_for(conn, data_dir, PLUGIN_ECOMMERCE_ID)
}

/// 指定 id 的插件包目录（四级解析，顺序固定）：
/// 1. `DAM_PACK_DIR`（显式指定，测试/排障用）；
/// 2. **账本 current 版本目录**（IP-K08 安装形态 `{data_dir}/dam/packs/<id>/<version>/`：
///    `packs.current` 是唯一指针，回滚 = 切指针，所以这里只认 current 行的 dir）；
/// 3. `{data_dir}/packs/<id>`（K05 预留的扁平安装位，兼容老部署）；
/// 4. 仓库内置 `packs/plugins/ecommerce`（容器/开发态，`cargo test` 同一份）。
///
/// 都没有 → `None`：引擎切到 plugin 时**报错**，绝不静默回退 legacy。
pub fn plugin_pack_dir_for(conn: &Connection, data_dir: &Path, id: &str) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("DAM_PACK_DIR") {
        let p = PathBuf::from(dir);
        if p.join("manifest.json").is_file() {
            return Some(p);
        }
    }
    use core_storage::packs::PackRepo as _;
    if let Ok(Some(rec)) = core_storage::packs::SqlitePackRepo::new(conn).current(id) {
        let dir = PathBuf::from(&rec.dir);
        if dir.join("manifest.json").is_file() {
            return Some(dir);
        }
    }
    let installed = data_dir.join(PACKS_DIR).join(id);
    if installed.join("manifest.json").is_file() {
        return Some(installed);
    }
    // 5. 仓库内置 `packs/plugins/<dir>`（容器/开发态 + `cargo test` 同一份）。
    //    N9/KI-31：不再只认电商包 —— 按 **manifest id** 匹配任意内置插件目录，
    //    否则「一个引擎多行业包」只能在生产装包后才跑得动（单机开发态直接 400）。
    repo_builtin_pack_dir(id)
}

/// 仓库内置插件根目录（dev/容器形态）：`packs/plugins/`。
///
/// 发行形态里 `env!("CARGO_MANIFEST_DIR")` 指向构建机路径、通常不存在 → 自然返回 `None`，
/// 不会把开发机的包当成产品内置（内置只有一个：`BUILTIN_PLUGIN_IDS`）。
pub fn repo_builtin_packs_root() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("packs")
        .join("plugins");
    root.is_dir().then_some(root)
}

/// 仓库内置插件目录（dev/容器形态）：扫 `packs/plugins/*/manifest.json`，按 manifest id 命中。
///
/// 发行形态里 `env!("CARGO_MANIFEST_DIR")` 指向构建机路径、通常不存在 → 自然返回 `None`，
/// 不会把开发机的包当成产品内置（内置只有一个：`BUILTIN_PLUGIN_IDS`）。
pub fn repo_builtin_pack_dir(id: &str) -> Option<PathBuf> {
    let root = repo_builtin_packs_root()?;
    let entries = std::fs::read_dir(&root).ok()?;
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("manifest.json").is_file())
        .collect();
    // 目录序不确定：按路径排序保证同一部署里解析结果稳定（不靠 readdir 顺序）
    dirs.sort();
    dirs.into_iter().find(|dir| {
        std::fs::read_to_string(dir.join("manifest.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("id").and_then(|i| i.as_str()).map(str::to_string))
            .map(|found| found == id)
            .unwrap_or(false)
    })
}

/// 判定行**读口径**包 id：读侧与写侧必须同一来源，不许各算一份。
///
/// IP-K16 起只有一个来源 —— **插件身份 id**（`com.soujpg.pack.ecommerce`）：
/// 写侧 `judge()` 用 `PackJudge::identity(plugin).id`，读侧用同一份 manifest；
/// `verify_pack_info()` 强制 `pack.info` 与 manifest 一致（不一致直接 400），两侧恒等。
/// 插件包缺失/坏包时退回内置插件常量：老判定行仍按同一个 id 可查（迁移 0028 已把历史行
/// 从旧的 `ecommerce-taobao` 改键过来），不把「插件没装」伪装成「一张都没判过」。
pub fn verdict_pack_id(conn: &Connection, data_dir: &Path) -> String {
    plugin_identity_id(conn, data_dir).unwrap_or_else(|| PLUGIN_ECOMMERCE_ID.to_string())
}

/// 插件包身份 id（只读 manifest，不拉子进程 —— 读接口必须廉价）。
fn plugin_identity_id(conn: &Connection, data_dir: &Path) -> Option<String> {
    let dir = plugin_pack_dir(conn, data_dir)?;
    pack_host::load_pack(&dir).ok().map(|(_, ident)| ident.id)
}

/// 把 `compliance_vl::VlSession` 接进插件能力网关（`pack_host::VlJudge` 端口）。
///
/// 为什么不是另写一套 HTTP：插件的 prompt 由包自己写，宿主只能提供「原样提问、原样取正文」
/// 的通道 —— 传输/重试/代理/错误分类复用 `VlSession::ask_raw`（全局同一份抖动语义）。
/// 后端错误一律回 `undetermined`（网关契约：VL 判不了不许冒充，协议 §7 铁律 1）。
struct GatewayVl {
    handle: svc_pipeline::VlHandle,
}

impl pack_host::VlJudge for GatewayVl {
    fn judge(
        &self,
        path: &Path,
        prompt: &str,
        max_tokens: u32,
    ) -> Result<pack_host::VlReply, pack_host::VlError> {
        let session = svc_pipeline::VlSession::new(
            self.handle.cfg.clone(),
            Vec::new(),
            Vec::new(),
            self.handle.stats.clone(),
        );
        let cap = max_tokens.min(pack_host::VL_MAX_TOKENS_CAP).max(1);
        Ok(
            match session.ask_raw(path, prompt, cap) {
                Ok(raw) => pack_host::VlReply::decided(raw, Vec::new()),
                Err(e) => pack_host::VlReply::undetermined(e.to_string()),
            },
        )
    }

    /// 重复采样（协议 1.2，K07-FIX-H 翻转保护）：第 1 份走缓存（同图同 prompt 复用旧结论，
    /// 省钱），第 2..N 份**强制新样本**（`ask_raw_fresh`）—— 都走缓存就等于把「3 次一致」
    /// 变成自证，翻转保护也就失效了。
    fn judge_samples(
        &self,
        path: &Path,
        prompt: &str,
        max_tokens: u32,
        samples: u32,
    ) -> Result<Vec<pack_host::VlReply>, pack_host::VlError> {
        let session = svc_pipeline::VlSession::new(
            self.handle.cfg.clone(),
            Vec::new(),
            Vec::new(),
            self.handle.stats.clone(),
        );
        let cap = max_tokens.min(pack_host::VL_MAX_TOKENS_CAP).max(1);
        let mut replies = Vec::new();
        for i in 0..samples.max(1) {
            let asked = if i == 0 {
                session.ask_raw(path, prompt, cap)
            } else {
                session.ask_raw_fresh(path, prompt, cap)
            };
            match asked {
                Ok(raw) => replies.push(pack_host::VlReply::decided(raw, Vec::new())),
                // 第 1 份就不可用 → 整体不可用；后续某份抖动 → 该份记 undetermined
                Err(e) if i == 0 => return Err(pack_host::VlError::Unavailable(e.to_string())),
                Err(e) => replies.push(pack_host::VlReply::undetermined(e.to_string())),
            }
        }
        Ok(replies)
    }
}

/// 未配置 VL 时的兜底通道：**回 undetermined，不回 403**。
///
/// 为什么要这个：`vl.judge` 端点若「没装 VL 后端」就 403，插件按协议 §5 会整包报 -32001，
/// 于是「没配 key 的用户」连 R1 尺寸这种纯几何规则都拿不到结论（legacy 通道此时是
/// classic 照判、vl 规则 skipped）。回 `undetermined` 让 VL 规则自己灰掉，其余规则照判 ——
/// 与 legacy 的降级语义对齐，且不冒充结论。
struct NoVl;

impl pack_host::VlJudge for NoVl {
    fn judge(
        &self,
        _path: &Path,
        _prompt: &str,
        _max_tokens: u32,
    ) -> Result<pack_host::VlReply, pack_host::VlError> {
        Ok(pack_host::VlReply::undetermined(
            "vl_not_configured：设置页未配置 VL/打标模型，插件通道的语义规则判不了",
        ))
    }
}

/// 插件运行时：一个包一个（网关 + 子进程宿主 + 包身份），实现 `svc_pipeline::PackJudge`。
///
/// 生命周期由 `Arc` 决定：一次 run 一个实例，run 结束随 Arc 落地 drop → 网关停、子进程收。
/// `Drop` 顺序有讲究：`PackHost` 先（关子进程），`Gateway` 后（子进程还在时网关必须在）。
struct PluginRuntime {
    host: pack_host::PackHost,
    /// 只用来保活（`Drop` 时停服务）；字段名前缀下划线 = 不直接读
    _gateway: pack_host::Gateway,
    identity: svc_pipeline::PackIdentityInfo,
    /// K07-I：本次 run 的**全量生效参数**（schema 默认值 ⊕ 规格 ⊕ 任务覆盖，已校验）。
    ///
    /// 一次调用原样进 `params.options`（再叠宿主注入的 `workdir`）。老包（未声明
    /// `manifest.params`）= 透传合并后的空/用户给的键，行为与协议 v1.1 完全一致。
    options: serde_json::Value,
}

impl PluginRuntime {
    /// 开包 → 起网关 → 拉起子进程 → `pack.info` 核对身份与规则清单。
    ///
    /// 任何一步失败都返回**可读错误**（界面要能直接展示「包为什么不可用」）：
    /// 这不是判定失败，是环境配置错，必须在 `/judge` 的入队阶段就拒绝，而不是让 N 张图
    /// 都落一堆 undetermined 才让人猜。
    fn open(
        pack_dir: &Path,
        work_dir: &Path,
        vl: Option<svc_pipeline::VlConfig>,
        rt: &crate::PackRuntime,
        options: serde_json::Value,
    ) -> Result<Self, String> {
        // 有 VL 配置 → 走真实 VL 通道；没配 → 兜底「undetermined」通道（不回 403，
        // 免得插件整包 -32001 把几何规则也一起带走）
        let judge: Arc<dyn pack_host::VlJudge> = match vl {
            Some(cfg) => Arc::new(GatewayVl {
                handle: svc_pipeline::VlHandle {
                    cfg,
                    stats: Arc::new(svc_pipeline::VlStats::default()),
                },
            }),
            None => Arc::new(NoVl),
        };
        Self::open_with(pack_dir, work_dir, judge, rt, options)
    }

    /// 装配主路径（`open` 与单元测试共用）：网关必须装上一个 VL 端口。
    fn open_with(
        pack_dir: &Path,
        work_dir: &Path,
        vl_judge: Arc<dyn pack_host::VlJudge>,
        rt: &crate::PackRuntime,
        options: serde_json::Value,
    ) -> Result<Self, String> {
        let (manifest, ident) = pack_host::load_pack(pack_dir)
            .map_err(|e| format!("插件包装载失败（{}）：{e}", pack_dir.display()))?;
        std::fs::create_dir_all(work_dir)
            .map_err(|e| format!("建插件工作目录 {} 失败：{e}", work_dir.display()))?;
        let gw_cfg = pack_host::GatewayConfig::from_manifest(&manifest, work_dir).with_vl(vl_judge);
        let gateway = pack_host::Gateway::start(gw_cfg)
            .map_err(|e| format!("能力网关启动失败：{e}"))?;
        // 解释器 + 启动器：桌面形态走随包 runtime（KI-28），容器/开发态回落 python3 + 仓库 tools/
        let cfg = pack_host_config(gateway.base_url(), gateway.token(), work_dir, rt);
        let host = pack_host::PackHost::open(pack_dir, cfg)
            .map_err(|e| format!("插件宿主打开失败（{}）：{e}", pack_dir.display()))?;
        // 协议 §3.1：pack.info 与 manifest 不一致即判包损坏（这里会顺带拉起子进程做握手）
        let problems = host
            .verify_pack_info()
            .map_err(|e| format!("插件握手失败（{}）：{e}", ident.label()))?;
        if !problems.is_empty() {
            return Err(format!(
                "插件包与 manifest 不一致（{}）：{}",
                ident.label(),
                problems.join("；")
            ));
        }
        let info = host
            .call_with_timeout("pack.info", json!({}), std::time::Duration::from_secs(10))
            .map_err(|e| format!("读 pack.info 失败（{}）：{e}", ident.label()))?;
        let rule_ids: Vec<String> = info
            .get("rules")
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| r.get("id").and_then(|v| v.as_str()))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if rule_ids.is_empty() && manifest.is_judge() {
            return Err(format!(
                "判定类插件包 {} 未声明任何规则（pack.info.rules 为空）",
                ident.label()
            ));
        }
        let deterministic = info
            .get("deterministic")
            .and_then(|v| v.as_bool())
            .unwrap_or(manifest.deterministic);
        // N6e：处理类包的可执行动作清单（`pack.info.actions[].id`，顺序 = UI 顺序）。
        // 宿主拿它校验 `actions[]` 入参；判定类包没有这个字段 → 空清单（不许按 rule_ids 猜）。
        let actions: Vec<String> = info
            .get("actions")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| a.get("id").and_then(|v| v.as_str()))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let identity = svc_pipeline::PackIdentityInfo {
            id: ident.id.clone(),
            version: ident.version.clone(),
            plugin_hash: ident.plugin_hash.clone(),
            deterministic,
            rule_ids,
            kinds: manifest
                .effective_kinds()
                .iter()
                .map(|k| k.as_str().to_string())
                .collect(),
            actions,
            timeout_secs: manifest.timeout_secs,
        };
        Ok(Self {
            host,
            _gateway: gateway,
            identity,
            options,
        })
    }

    /// 一次协议调用（judge / repair 共用骨架）：失败分类 + 身份标签 + 时长 + 原始应答。
    fn call(&self, method: &str, req: &svc_pipeline::PluginAsset) -> svc_pipeline::PluginOutcome {
        self.call_with_action(method, req, None)
    }

    /// N6e：带**单动作**的调用 —— 与 [`Self::call`] 同一骨架，只在 `options.action` 上
    /// 覆盖一个值（协议不变，仍是 `judge`；处理类包按 `options.action` 选动作）。
    ///
    /// 覆盖**必须**在宿主侧做（不是让调用方改 options 再传）：`options` 是「全量生效值」，
    /// 动作是**这一步**的事，不该被写进参数快照/规格里（否则下一步会继承上一步的动作）。
    fn call_with_action(
        &self,
        method: &str,
        req: &svc_pipeline::PluginAsset,
        action: Option<&str>,
    ) -> svc_pipeline::PluginOutcome {
        let mut options = build_options(&self.options, &req.work_dir);
        if let (Some(a), Some(map)) = (action, options.as_object_mut()) {
            map.insert("action".to_string(), json!(a));
        }
        let params = json!({
            "job_id": format!("job-{}", req.job_id),
            "asset": {
                "id": format!("asset-{}", req.asset_id),
                "path": req.path,
                "mime": req.mime,
                "width": req.width,
                "height": req.height,
            },
            "rules": req.rule_ids,
            // 协议 §3.3：options 由包解释；这里发**全量生效值**（已合并默认值/规格/任务覆盖），
            // workdir 是协议 §3.4 的产物目录（judge 也要它来暂存掩码），由宿主覆盖注入。
            "options": options,
        });
        let t0 = std::time::Instant::now();
        let result = self.host.call(method, params);
        let duration_ms = t0.elapsed().as_millis() as u64;
        match result {
            Err(e) => {
                let kind = map_host_error(&e);
                svc_pipeline::PluginOutcome {
                    pack: self.identity.clone(),
                    verdicts: Vec::new(),
                    artifacts: Vec::new(),
                    rows: Vec::new(),
                    duration_ms,
                    failure: Some((kind, e.to_string())),
                    raw: json!({ "error": e.to_string(), "kind": kind.reason() }),
                }
            }
            Ok(v) => parse_reply(
                &self.identity,
                method,
                &format!("asset-{}", req.asset_id),
                v,
                duration_ms,
            ),
        }
    }
}

/// 一次调用的 `options`（协议 §3.3）：**全量生效值** + 宿主注入的 `workdir`。
///
/// `workdir` 是宿主保留键（schema 不许声明，见 `pack_host::params::RESERVED_KEYS`），
/// 这里**无条件覆盖**：插件拿到的产物目录永远是宿主给的那个，不会被参数值带跑。
fn build_options(effective: &serde_json::Value, work_dir: &str) -> serde_json::Value {
    let mut map = match effective {
        serde_json::Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    map.insert("workdir".to_string(), json!(work_dir));
    serde_json::Value::Object(map)
}

/// GET /api/plugins/{id}/schema —— 插件**参数声明**（K07-I）。
///
/// 只读包目录里的 `params.schema.json`（`manifest.params` 指的那个），**不启子进程**
/// （设计口径：读接口必须廉价）。三态都给结构，前端不用猜：
/// - 包在、声明了 schema → `declared:true` + `schema`（插件原文，界面据此画控件）；
/// - 包在、没声明（老包）→ `declared:false` + `schema:null`（协议 v1.1 自由透传，界面给空面板）；
/// - 包不在 → 404 `plugin_not_found`（不是空 schema —— 那会让界面以为「这插件没参数」）。
/// 声明了但读不出来/校验不过 → 500 `pack_params_schema`（装坏的包，排障要看见）。
pub async fn plugin_params_schema(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    // 读侧与写侧必须是**同一连接**（卡 task）：`install` 的 `upsert_pack_row` /
    // `set_current` 写的就是 `state.db`。早先这里另开 `core_storage::open(db_path)` 新连接，
    // 等于把「装完能不能读到」押在「另一份连接看到的是不是同一个库」上 —— 同进程装完立刻读
    // schema 却 404、而 `/api/compliance/packs`（走 state.db）显示包在，就是这么裂开的。
    // 顺带：每请求不再跑一遍迁移 / OCR 自愈探测（那本是进程启动该做的事）。
    let conn = state.db.lock().await;
    match schema_snapshot(&conn, &state.data_dir, &id) {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err((code, err_code, msg)) => err(code, err_code, msg),
    }
}

/// [`plugin_params_schema`] 的同步主体：解析「包目录 → manifest → schema 原文」。
fn schema_snapshot(
    conn: &Connection,
    data_dir: &Path,
    id: &str,
) -> Result<serde_json::Value, (StatusCode, &'static str, String)> {
    let Some(dir) = plugin_pack_dir_for(conn, data_dir, id) else {
        return Err((
            StatusCode::NOT_FOUND,
            "plugin_not_found",
            format!("未找到插件包 {id}（未安装，且不是仓库内置包）"),
        ));
    };
    let (manifest, ident) = pack_host::load_pack(&dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "pack_manifest",
            format!("包目录 {} 的 manifest 不可解析：{e}", dir.display()),
        )
    })?;
    // `DAM_PACK_DIR` 是开发/排障用的**强制目录**（四级解析的第 1 级不看 id），
    // 所以这里必须再核一次身份：**要什么就给我什么**，别把一个包当成另一个报上去
    // （界面会按错误 id 画表单、甚至把参数发给错的插件）。
    if ident.id != id {
        return Err((
            StatusCode::NOT_FOUND,
            "plugin_not_found",
            format!("未找到插件包 {id}（解析到的目录属于 {}）", ident.id),
        ));
    }
    let declared = manifest.params.clone();
    let raw = match declared.as_deref() {
        None => serde_json::Value::Null,
        Some(rel) => {
            // 解析校验一次：坏 schema 装不进来 → 这里的失败一定是「包被改坏了」
            pack_host::ParamSchema::load(&dir, rel)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.code(), e.to_string()))?;
            let raw = std::fs::read(dir.join(rel)).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "pack_params_missing",
                    format!("读不到 {rel}：{e}"),
                )
            })?;
            serde_json::from_slice(&raw).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "pack_params_schema",
                    format!("{rel} 不是合法 JSON：{e}"),
                )
            })?
        }
    };
    Ok(json!({
        "pluginId": ident.id,
        "version": ident.version,
        "pluginHash": ident.plugin_hash,
        "declared": declared.is_some(),
        "schemaFile": declared,
        "schema": raw,
    }))
}

/// K07-I：一次执行的**参数快照**（合并校验后的全量生效值）。
///
/// 语义是「这一次执行用的是哪套参数」——落进 `compliance_runs.params`，重跑/复盘据此
/// 复现（**不重新合并默认值**：插件升级默认值后，历史结果仍要说得清当时用的是什么）。
#[derive(Debug, Clone)]
struct JudgeParams {
    /// 插件是否声明了参数 schema（false = 老包，层原样透传、不校验）
    declared: bool,
    /// 全量生效值（不含宿主注入的 `workdir`，那个每次调用现算）
    effective: serde_json::Map<String, serde_json::Value>,
}

impl JudgeParams {
    fn effective_value(&self) -> serde_json::Value {
        serde_json::Value::Object(self.effective.clone())
    }

    /// N1：宿主注入**规则级阈值**（`options.thresholds`）。
    ///
    /// 为什么在 schema 校验之后注入：`thresholds` 不是插件 schema 声明的平铺键（它是协议里
    /// 既有的结构化入参），塞进 schema 层会被「未知键」拦掉；但它是规格形态的原文
    /// （`{ruleId: {key: value}}`，迁自旧行业包 YAML），只有插件知道怎么折成自己的阈值表。
    /// 宿主只搬运，不做业务映射（用户裁决：映射留在插件内）。
    ///
    /// 注入条件：规格里确有阈值；已存在同名键时不覆盖（声明值/任务覆盖优先）。
    fn inject_thresholds(&mut self, thresholds: Option<&serde_json::Value>) {
        let Some(serde_json::Value::Object(map)) = thresholds else {
            return;
        };
        if map.is_empty() || self.effective.contains_key("thresholds") {
            return;
        }
        self.effective
            .insert("thresholds".to_string(), serde_json::Value::Object(map.clone()));
    }

    /// 落 run 的 `params` 快照（人读 + 机读都在里面）。
    fn snapshot(&self) -> serde_json::Value {
        json!({
            "declared": self.declared,
            "effective": self.effective,
        })
    }
}

/// 解析一次执行的参数：`插件默认值 ⊕ 规格 diff ⊕ 任务覆盖` → `when` 剔除 → 校验。
///
/// 三条口径（设计文档「运行时链路」第 3-4 步）：
/// - 只读包目录（**不启子进程**）：读 schema 是廉价操作，不该为了校验参数去拉插件；
/// - 老包（`manifest.params` 缺省）走 [`pack_host::merge_layers`] 原样透传 —— 协议 v1.1
///   的 `options` 语义不变，插件换到 v2 声明时行为连续；
/// - 返回 `Ok(None)` = 包目录缺失（插件未安装）—— 由 prepare 阶段报准确错误，这里不抢口径。
fn resolve_judge_params(
    conn: &Connection,
    data_dir: &Path,
    // KI-31：参数 schema 属于**规格所属的插件**（不是「当前内置的那个」）
    plugin_id: &str,
    spec: Option<&serde_json::Value>,
    task: Option<&serde_json::Value>,
    // N1：规格迁移自旧行业包的**规则级阈值**（`{ruleId: {key: value}}`）。
    // 它不进 schema 层合并（schema 只声明平铺键），而是在合并校验**之后**由宿主注入
    // `options.thresholds` —— 协议里这个结构本来就存在（插件 `_merge_flat_thresholds`
    // 认它），宿主只是把规格里的原文递过去，业务映射仍在插件内。
    thresholds: Option<&serde_json::Value>,
) -> Result<Option<JudgeParams>, JudgeFail> {
    resolve_judge_params_for(conn, data_dir, Some(plugin_id), spec, task, thresholds)
}

/// N6e：同上，但可以**点名插件包**（动作链模式）。
///
/// `pack_id = Some(id)` 时：
/// * 参数按**这个包**的 schema 合并（不是默认判定包的）；
/// * 不看引擎档位 —— 动作链本来就走插件通道，能点名包就说明意图明确。
/// `pack_id = None` = 走默认判定插件包（N18：执行通道恒为插件，没有第二套引擎）。
fn resolve_judge_params_for(
    conn: &Connection,
    data_dir: &Path,
    pack_id: Option<&str>,
    spec: Option<&serde_json::Value>,
    task: Option<&serde_json::Value>,
    // N1：规格迁移自旧行业包的**规则级阈值**（`{ruleId: {key: value}}`），合并校验后由宿主
    // 注入 `options.thresholds`（N链合流：N6e 的 pack_id 与 N1 的 thresholds 同签名共存）。
    thresholds: Option<&serde_json::Value>,
) -> Result<Option<JudgeParams>, JudgeFail> {
    let pack_dir = match pack_id {
        Some(id) => plugin_pack_dir_for(conn, data_dir, id),
        None => plugin_pack_dir(conn, data_dir),
    };
    let Some(pack_dir) = pack_dir else {
        return Ok(None);
    };
    let (manifest, _ident) = pack_host::load_pack(&pack_dir)
        .map_err(|e| JudgeFail::new("pack_manifest", e.to_string()))?;
    let layers = [("规格", spec), ("任务覆盖", task)];
    let mut params = match pack_host::ParamSchema::load_declared(&pack_dir, manifest.params.as_deref())
    {
        Ok(Some(schema)) => {
            let effective = schema
                .resolve_json(&layers)
                .map_err(|e| JudgeFail::new(e.code(), e.to_string()))?;
            let mut params = JudgeParams {
                declared: true,
                effective,
            };
            // schema 自己声明了 `thresholds` 就尊重声明值（用户/规格显式给的不覆盖）
            if !schema.properties().contains_key("thresholds") {
                params.inject_thresholds(thresholds);
            }
            params
        }
        Ok(None) => {
            // 老包：不校验、不填默认值（宿主不知道它有哪些参数），只按层序合并
            let effective = pack_host::merge_layers(&layers).map_err(|e| JudgeFail::new(e.code(), e.to_string()))?;
            let mut params = JudgeParams {
                declared: false,
                effective,
            };
            params.inject_thresholds(thresholds);
            params
        }
        Err(e) => return Err(JudgeFail::new(e.code(), format!("{}：{e}", pack_dir.display()))),
    };
    Ok(Some(params))
}

/// 插件宿主错误 → 失败归档（决定证据里的 `reason` 词，报告按它分类统计）。
fn map_host_error(e: &pack_host::PackHostError) -> svc_pipeline::PluginFailure {
    match e {
        pack_host::PackHostError::Unavailable { kind, .. } => match kind {
            pack_host::Unavailable::Timeout => svc_pipeline::PluginFailure::Timeout,
            pack_host::Unavailable::Crash => svc_pipeline::PluginFailure::Crash,
            pack_host::Unavailable::HealthTimeout | pack_host::Unavailable::LoadFailed => {
                svc_pipeline::PluginFailure::Unavailable
            }
        },
        // 插件回的业务错误码（-32001..-32005：能力缺失/资产读不出/参数错/取消）＝规则级失败
        pack_host::PackHostError::Rpc { .. } => svc_pipeline::PluginFailure::RuleError,
        // 应答缺 result/error、方法未实现 → 协议违例
        pack_host::PackHostError::Protocol(_) => svc_pipeline::PluginFailure::Protocol,
        pack_host::PackHostError::Manifest(_) | pack_host::PackHostError::Io(_) => {
            svc_pipeline::PluginFailure::Unavailable
        }
    }
}

/// 行级 `fields`（行业自定义列）挂进证据的保留键 `fields`。
///
/// 为什么不另开一列：`fields` 与 `evidence` 在契约里都是「只展示、不参与逻辑」的自由结构；
/// 另开列会让「同一份行数据两处存」的漂移成为可能（改一处忘另一处）。
/// 保留键 `fields` 是宿主命名，插件自己的 evidence 里不许占用（N5a conformance 已约束）。
fn with_row_fields(
    mut evidence: serde_json::Value,
    fields: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    if fields.is_empty() {
        return evidence;
    }
    if let Some(obj) = evidence.as_object_mut() {
        obj.insert("fields".to_string(), serde_json::Value::Object(fields.clone()));
    }
    evidence
}

/// 解析 `judge`/`repair` 应答（协议 §3.3/§3.4 + N5a 结果行契约）。
///
/// 两道关，任何一道不过 → **整个应答判违例**（部分采信的代价是「不知道哪条丢了」）：
///
/// 1. **结果行契约**（`pack_host::result::rows_from_reply`）：`result` 三值、`failed` 带
///    `error`、`skipped` 带 `skipReason`、`verdict` 三态且只在 `kinds` 含 `judge` 时允许。
///    **处理类插件回 verdict → `PluginFailure::VerdictNotAllowed`**（记 `plugin_verdict_not_allowed`，
///    不折算成 pass/skipped，也不静默丢弃）。
/// 2. 旧 `verdicts[]` 严格解析（`ruleId`/`status` 必填），下游 `plugin_rows` 靠它做规则级合并。
fn parse_reply(
    identity: &svc_pipeline::PackIdentityInfo,
    method: &str,
    asset_id: &str,
    v: serde_json::Value,
    duration_ms: u64,
) -> svc_pipeline::PluginOutcome {
    let fail = |detail: String| svc_pipeline::PluginOutcome {
        pack: identity.clone(),
        verdicts: Vec::new(),
        artifacts: Vec::new(),
        rows: Vec::new(),
        duration_ms,
        failure: Some((svc_pipeline::PluginFailure::Protocol, detail)),
        raw: v.clone(),
    };
    // 运行期 kinds：identity 里存的是字符串（svc-pipeline 不认识 pack-host 的枚举）
    let kinds: Vec<pack_host::PluginKind> = if identity.kinds.is_empty() {
        vec![pack_host::PluginKind::Judge] // 旧调用方没写 kinds = 判定类（与 manifest 缺省同口径）
    } else {
        identity
            .kinds
            .iter()
            .filter_map(|k| pack_host::PluginKind::parse(k))
            .collect()
    };
    if kinds.is_empty() {
        return fail(format!(
            "{method} 调用的插件 kinds 声明非法（{}，可用：judge/process）",
            identity.kinds.join(",")
        ));
    }
    // ① 结果行契约：这是「处理类冒充判定」的拒收点（反向验证的靶子）
    let rows = match pack_host::result::rows_from_reply_with(&kinds, &v, Some(asset_id)) {
        Ok(rows) => rows,
        Err(violation) => {
            let (kind, detail) = match &violation {
                pack_host::result::RowViolation::VerdictNotAllowed => (
                    svc_pipeline::PluginFailure::VerdictNotAllowed,
                    format!("{method} 应答：{violation}（kinds={:?}）", identity.kinds),
                ),
                _ => (
                    svc_pipeline::PluginFailure::Protocol,
                    format!(
                        "{method} 应答不合结果行契约（{}）：{violation}",
                        violation.code()
                    ),
                ),
            };
            return svc_pipeline::PluginOutcome {
                pack: identity.clone(),
                verdicts: Vec::new(),
                artifacts: Vec::new(),
                rows: Vec::new(),
                duration_ms,
                failure: Some((kind, detail)),
                raw: v.clone(),
            };
        }
    };
    let is_judge = kinds.contains(&pack_host::PluginKind::Judge);
    // N6e：结果行原文（处理类插件逐动作调用时，宿主靠它读 `fields.outPath/sha256`）。
    // 用契约层的类型**回序列化**，而不是透传原始 JSON —— 拿到的就是宿主认过的字段。
    let rows_json: Vec<serde_json::Value> = rows
        .iter()
        .filter_map(|r| serde_json::to_value(r).ok())
        .collect();
    // ② 旧 verdict 通道：判定类必须有结论行（`verdicts[]` 或新契约 `rows[]` 二者其一）
    let verdicts = if let Some(items) = v.get("verdicts").and_then(|x| x.as_array()) {
        let mut verdicts = Vec::with_capacity(items.len());
        for item in items {
            let Some(rule_id) = item.get("ruleId").and_then(|x| x.as_str()) else {
                return fail(format!("{method} 应答有 verdict 缺 ruleId"));
            };
            let Some(status) = item.get("status").and_then(|x| x.as_str()) else {
                return fail(format!("{method} 应答有 verdict 缺 status（ruleId={rule_id}）"));
            };
            let legacy_fields = item
                .get("fields")
                .and_then(|f| f.as_object())
                .cloned()
                .unwrap_or_default();
            verdicts.push(svc_pipeline::PluginVerdict {
                rule_id: rule_id.to_string(),
                status: status.to_string(),
                score: item.get("score").and_then(|x| x.as_f64()),
                label: item.get("label").and_then(|x| x.as_str()).map(str::to_string),
                evidence: with_row_fields(
                    item.get("evidence")
                        .cloned()
                        .filter(|e| e.is_object())
                        .unwrap_or_else(|| json!({})),
                    &legacy_fields,
                ),
                fixed: item.get("fixed").and_then(|x| x.as_bool()).unwrap_or(false),
                note: item.get("note").and_then(|x| x.as_str()).map(str::to_string),
            });
        }
        verdicts
    } else if is_judge {
        // 新契约形态（只有 `rows[]`）：按行还原规则结论，缺 ruleId 就是行键不成立
        let mut verdicts = Vec::with_capacity(rows.len());
        for row in &rows {
            let Some(rule_id) = row.rule_id.as_deref() else {
                return fail(format!(
                    "{method} 应答的判定行缺 ruleId（判定类按规则出行，行键 = assetId + ruleId）"
                ));
            };
            let (status, score, note) = match (&row.verdict, row.result) {
                (Some(v), _) => (
                    v.status.as_str().to_string(),
                    v.confidence,
                    v.reason_code.clone(),
                ),
                (None, pack_host::ResultStatus::Skipped) => (
                    "skipped".to_string(),
                    None,
                    row.skip_reason.clone(),
                ),
                // 判不了 ≠ 通过：行级执行失败/无结论一律 undetermined（带原因）
                (None, _) => (
                    "undetermined".to_string(),
                    None,
                    Some(
                        row.error
                            .as_ref()
                            .map(|e| format!("{}：{}", e.code, e.message))
                            .unwrap_or_else(|| "no_verdict".to_string()),
                    ),
                ),
            };
            verdicts.push(svc_pipeline::PluginVerdict {
                rule_id: rule_id.to_string(),
                status,
                score,
                label: None,
                // 行业自定义列（N5b）：行级 `fields` 挂在证据的保留键上 —— `fields` 与
                // `evidence` 都是「只展示不参与逻辑」（契约原文），落库不另开列，
                // `/api/compliance/results` 用 `json_extract(evidence,'$.fields')` 取出即可。
                evidence: with_row_fields(
                    serde_json::Value::Object(row.evidence.clone()),
                    &row.fields,
                ),
                fixed: false,
                note,
            });
        }
        verdicts
    } else {
        // 处理类：没有判定结论，产物原样透传（下游只读 artifacts）
        let artifacts = v
            .get("artifacts")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        return svc_pipeline::PluginOutcome {
            pack: identity.clone(),
            verdicts: Vec::new(),
            artifacts,
            rows: rows_json,
            duration_ms,
            failure: None,
            raw: v,
        };
    };
    let artifacts = v
        .get("artifacts")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    svc_pipeline::PluginOutcome {
        pack: identity.clone(),
        verdicts,
        artifacts,
        rows: rows_json,
        duration_ms,
        failure: None,
        raw: v,
    }
}

impl svc_pipeline::PackJudge for PluginRuntime {
    fn identity(&self) -> svc_pipeline::PackIdentityInfo {
        self.identity.clone()
    }

    fn judge_asset(&self, req: &svc_pipeline::PluginAsset) -> svc_pipeline::PluginOutcome {
        self.call("judge", req)
    }

    fn repair_asset(&self, req: &svc_pipeline::PluginAsset) -> svc_pipeline::PluginOutcome {
        self.call("repair", req)
    }

    /// N6e：链上的一步 = 一次 `judge` + `options.action` 覆盖（协议零改动）。
    fn action_asset(
        &self,
        req: &svc_pipeline::PluginAsset,
        action: &str,
    ) -> svc_pipeline::PluginOutcome {
        self.call_with_action("judge", req, Some(action))
    }
}

/// 启动器目录（`-m dampack_launcher`）。发行形态优先随包分发的那份。
///
/// 三级（KI-28，卡 `task`）：`DAMPACK_LAUNCHER_DIR`（测试/排障显式）→ 随包 `<resource>/pack-launcher`
/// （壳解析后放在 [`crate::PackRuntime::launcher_dir`]）→ 仓库 `tools/`（开发机兜底）。
/// 发行包里 `CARGO_MANIFEST_DIR` 指的还是**构建机**路径，所以随包那份必须排在它前面。
fn launcher_dir(rt: &crate::PackRuntime) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("DAMPACK_LAUNCHER_DIR") {
        let p = PathBuf::from(dir);
        if p.join("dampack_launcher.py").is_file() {
            return Some(p);
        }
    }
    if let Some(p) = rt.launcher_dir.clone() {
        return Some(p);
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tools");
    repo.join("dampack_launcher.py").is_file().then(|| repo)
}

/// 插件宿主的统一装配入口（探针 / `pack.info` 核对 / 真判定三处共用）。
///
/// 为什么必须是**一个**函数：解释器与启动器若各装配一次，就会出现「设置页探针能拉起、
/// 判定通道拉不起」的分裂（KI-28 之前正是 runtime_python 从没被赋值 → 纯净 Windows 全挂）。
pub fn pack_host_config(
    gateway_url: impl Into<String>,
    gateway_token: impl Into<String>,
    work_dir: &Path,
    rt: &crate::PackRuntime,
) -> pack_host::PackHostConfig {
    let mut cfg = pack_host::PackHostConfig::new(gateway_url, gateway_token, work_dir);
    if let Some(dir) = launcher_dir(rt) {
        cfg = cfg.with_launcher_dir(dir);
    }
    // 内置 runtime 解释器：桌面形态由壳从 `<resource_dir>/runtime/python` 解析；
    // 容器/开发形态为 None → `PackHost` 回落裸 `python3`（既有行为不回退）。
    cfg.runtime_python = rt.python.clone();
    cfg
}

/// 处理插件状态快照（`/compliance/status` 的 `pluginState`）。
///
/// 只读 manifest（不拉子进程）：状态接口必须廉价 —— 「插件可用吗」= 包在不在 + manifest 解析
/// 得开，真正的握手发生在 `POST /judge`（那时才有子进程成本）。
fn plugin_state(conn: &Connection, data_dir: &Path) -> serde_json::Value {
    plugin_state_for(conn, data_dir, PLUGIN_ECOMMERCE_ID)
}

/// 指定 id 的插件状态（只读 manifest，不拉子进程）。
fn plugin_state_for(conn: &Connection, data_dir: &Path, id: &str) -> serde_json::Value {
    let Some(dir) = plugin_pack_dir_for(conn, data_dir, id) else {
        return json!({
            "available": false,
            "reason": "plugin_pack_missing",
            "note": "这个能力还没装：装好后就能用它检查（不需要再选什么规则）",
            "dir": serde_json::Value::Null,
        });
    };
    match pack_host::load_pack(&dir) {
        Ok((m, ident)) => {
            // manifest 好 ≠ 包能用：`resources/rules.json` 的键守卫（未知键/scope 词表错）
            // 在 `Pack::from_plugin_dir` 里，这里一并核 —— 否则状态接口会报
            // 「available=true、0 条规则」而判定端一张都发不出去（假可用的典型）。
            //
            // KI35-P4 走查缺口：rules.json 是**判定类**包的契约（规则名/报告列）；
            // kinds=["process"] 的处理类包（imaging/segment/upscale）天生没有 rules.json，
            // 按「缺 rules.json 即不可用」判会把所有处理类包永久挡在导航与执行之外。
            // 假通过防线只对能产出 verdict 的 judge 包有意义 → 处理类包跳过这道守卫。
            let judge_pack = m.is_judge();
            if judge_pack {
                if let Err(e) = load_meta_pack(&dir) {
                    return json!({
                        "available": false,
                        // 带 id：包**装了**，只是元数据坏 —— 界面据此显示「不可用」而不是「未安装」
                        "id": id,
                        "reason": "plugin_meta_invalid",
                        "note": e,
                        "dir": dir.to_string_lossy(),
                    });
                }
            }
            json!({
                "available": true,
                "id": ident.id,
                "version": ident.version,
                "pluginHash": ident.plugin_hash,
                "apiVersion": m.api_version,
                "deterministic": m.deterministic,
                "signatureStatus": if m.signature.is_some() { "declared" } else { "unsigned" },
                "timeoutSecs": m.timeout_secs,
                "capabilities": m.capabilities,
                "dir": dir.to_string_lossy(),
                "note": serde_json::Value::Null,
            })
        }
        Err(e) => json!({
            "available": false,
            "reason": "plugin_manifest_invalid",
            "note": e.to_string(),
            "dir": dir.to_string_lossy(),
        }),
    }
}

/// 把插件登记进 `packs` 账本（migration 0026）：判定来源列要能反查到「这版包从哪来的」。
///
/// 只在**真要用插件**时登记（run 起手），不在读接口里写库；同 `(id, version)` 幂等，
/// `current` 切到本次版本（回滚 = 用旧版本再登记一次）。
fn register_pack(
    conn: &Connection,
    manifest: &pack_host::Manifest,
    ident: &pack_host::PackIdentity,
    dir: &Path,
) -> Result<(), String> {
    use core_storage::packs::PackRepo as _;
    let repo = core_storage::packs::SqlitePackRepo::new(conn);
    let rec = core_storage::packs::PackRecord {
        id: ident.id.clone(),
        version: ident.version.clone(),
        dir: dir.to_string_lossy().to_string(),
        manifest_hash: ident.plugin_hash.clone(),
        api_version: manifest.api_version.clone(),
        deterministic: manifest.deterministic,
        signature_status: if manifest.signature.is_some() {
            "declared".to_string()
        } else {
            "unsigned".to_string()
        },
        kinds: manifest.kinds_canonical(),
        installed_at: unix_now(),
        current: true,
    };
    repo.upsert(&rec).map_err(|e| format!("登记行业包失败：{e}"))?;
    repo.set_current(&ident.id, &ident.version)
        .map_err(|e| format!("切换行业包生效版本失败：{e}"))?;
    Ok(())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// N1：一次执行引用的**规格**（已过「存在 / 启用 / 插件在装 / 版本钉 / 规则子集」检查）。
///
/// 为什么先在 CAS 之前把它读出来并校验：规格是「参数 + 规则 + 阈值」的持久化组合，
/// 配错了就是配错了 —— 必须当场 400（不占运行槽、不建 run 行、不入队），
/// 而不是让 500 张图各落一堆 undetermined 之后才让人去猜。
struct JudgeSpec {
    record: core_storage::specs::SpecRecord,
}

impl JudgeSpec {
    /// 规格层参数 diff（合并序里的「规格」层；空对象 = 不改默认值）
    fn params_layer(&self) -> serde_json::Value {
        self.record.params_diff.clone()
    }

    /// 规则级阈值（宿主注入 `options.thresholds`；空对象 = 不注入）
    fn thresholds(&self) -> serde_json::Value {
        self.record.thresholds.clone()
    }

    /// 只判这些规则（空 = 全部）
    fn rules(&self) -> Vec<String> {
        self.record.rules.clone()
    }

    /// PROC-RES-1：规格的**动作链**（有序动作名；空 = 判定类规格，不走链）。
    ///
    /// 形态（`/api/specs` 建规格时已校验）：`[{"action":"resize","params":{...},"when":...}]`。
    /// 这里只取动作名按序执行；每步的 `params` 由 [`Self::chain_params_layer`] 平铺进参数层。
    /// 脏形态（非数组 / 缺 `action`）不静默吞 —— 当场报错，不让「规格选了动作却没跑」再发生。
    fn action_names(&self) -> Result<Vec<String>, JudgeFail> {
        let serde_json::Value::Array(list) = &self.record.actions else {
            if self.record.actions.is_null() || self.record.actions == json!({}) {
                return Ok(Vec::new());
            }
            return Err(JudgeFail::new(
                "spec_actions_invalid",
                format!(
                    "规格「{}」的 actions 不是数组（收到 {}）",
                    self.record.name, self.record.actions
                ),
            ));
        };
        let mut out = Vec::with_capacity(list.len());
        for (i, item) in list.iter().enumerate() {
            let Some(name) = item.get("action").and_then(|a| a.as_str()) else {
                return Err(JudgeFail::new(
                    "spec_actions_invalid",
                    format!(
                        "规格「{}」的 actions[{i}] 缺 `action`（动作名）",
                        self.record.name
                    ),
                ));
            };
            out.push(name.to_string());
        }
        Ok(out)
    }

    /// PROC-RES-1：**处理类规格**的参数层 = `paramsDiff ⊕ 各动作 params 按链序平铺`。
    ///
    /// 既有链机制（`PackJudge::action_asset`）是**全链共享一份 options**（每步只覆盖
    /// `options.action`），没有按动作分参数的通道 —— 这里对齐现状：动作 params 平铺合并、
    /// 同名键后一动作胜，不发明第二套参数通道。局限（按卡注明）：多动作若对同一键给不同
    /// 值，只有链尾的值生效。
    fn chain_params_layer(&self) -> serde_json::Value {
        let mut layer = self.params_layer();
        let Some(obj) = layer.as_object_mut() else {
            return layer;
        };
        if let serde_json::Value::Array(list) = &self.record.actions {
            for item in list {
                if let Some(serde_json::Value::Object(params)) = item.get("params") {
                    for (k, v) in params {
                        obj.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        layer
    }


    /// 生效参数快照里的 `spec` 段（重跑要能说清「用哪条规格、它当时是什么」）
    fn snapshot(&self) -> serde_json::Value {
        json!({
            "specId": self.record.spec_id,
            "name": self.record.name,
            "pluginId": self.record.plugin_id,
            "pluginVersionPin": self.record.plugin_version_pin,
            "originKey": self.record.origin_key,
            "rules": self.record.rules,
            "thresholds": self.record.thresholds,
            "paramsDiff": self.record.params_diff,
            "actions": self.record.actions,
            "output": self.record.output,
        })
    }
}

/// 包声明的规则 id（`resources/rules.json`；读不到 = 空 = 无法校验，按「不猜」处理）。
fn plugin_declared_rules(pack_dir: &Path) -> Vec<String> {
    std::fs::read_to_string(pack_dir.join("resources").join("rules.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v.get("rules").and_then(|r| r.as_array()).map(|a| {
                a.iter()
                    .filter_map(|r| r.get("id").and_then(|i| i.as_str()).map(str::to_string))
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// 装载 + 校验规格（错误码稳定：`spec_not_found` / `spec_disabled` /
/// `spec_plugin_not_installed` / `spec_plugin_version_mismatch` / `spec_unknown_rule`）。
///
/// N18 口径：**执行通道只剩插件**，所以规格只需要过一道身份检查 —— 规格属于哪个插件，
/// 就开哪个包（KI-31）。包不在 = `spec_plugin_not_installed`（带「去装插件」的去处），
/// 版本钉死但装的是别的版本 = `spec_plugin_version_mismatch`（带「改绑/新建规格」的去处）。
fn load_judge_spec(
    conn: &Connection,
    data_dir: &Path,
    spec_id: i64,
) -> Result<JudgeSpec, JudgeFail> {
    use core_storage::specs::{SpecRepo as _, SqliteSpecRepo};
    let repo = SqliteSpecRepo::new(conn);
    let record = repo
        .get(spec_id)
        .map_err(|e| JudgeFail::new("db", e.to_string()))?
        .ok_or_else(|| {
            JudgeFail::new("spec_not_found", format!("规格 #{spec_id} 不存在"))
                .with_params(serde_json::json!({ "specId": spec_id }))
        })?;
    if !record.enabled {
        return Err(JudgeFail::guided(
            "spec_disabled",
            format!("规格「{}」已停用", record.name),
            "在插件详情页把这条规格启用后再跑（停用的规格不许被任务引用）",
            "open_packs_drawer",
        )
        .with_params(serde_json::json!({ "specName": record.name })));
    }
    let packs_dir = data_dir.join(PACKS_DIR);
    // 规格属于哪个插件就开哪个包（KI-31）；包不可装载 = 当场 400，参数绝不静默失效。
    let Some(spec_dir) = plugin_pack_dir_for(conn, data_dir, &record.plugin_id) else {
        return Err(JudgeFail::guided(
            "spec_plugin_not_installed",
            format!(
                "规格「{}」属于插件 {}，但那个插件现在没装",
                record.name, record.plugin_id
            ),
            format!(
                "把 {} 装回来（或换一条指向已装插件的规格）再跑；已判定过的历史结果不受影响",
                record.plugin_id
            ),
            "open_packs_drawer",
        )
        .with_params(serde_json::json!({
            "specName": record.name,
            "pluginId": record.plugin_id,
        })));
    };
    // 版本钉死：规格写了 `pluginVersionPin` 就必须跑在那个版本上（不自动漂移、不静默改用新版）
    if let Some(pin) = record.plugin_version_pin.as_deref() {
        let installed = pack_host::load_pack(&spec_dir)
            .map(|(manifest, _ident)| manifest.version)
            .unwrap_or_default();
        if !pin.is_empty() && installed != pin {
            return Err(JudgeFail::guided(
                "spec_plugin_version_mismatch",
                format!(
                    "规格「{}」钉在 {}@{pin}，现在装的是 {}@{installed}",
                    record.name, record.plugin_id, record.plugin_id
                ),
                "在插件详情页把规格改绑到已装版本，或按这条规格新建一条（阈值/规则不动）",
                "open_packs_drawer",
            )
            .with_params(serde_json::json!({
                "specName": record.name,
                "pluginId": record.plugin_id,
                "pinned": pin,
                "installed": installed,
            })));
        }
    }
    if !record.rules.is_empty() {
        let declared = plugin_declared_rules(&spec_dir);
        if !declared.is_empty() {
            if let Some(bad) = record
                .rules
                .iter()
                .find(|r| !declared.iter().any(|d| d == *r))
            {
                return Err(JudgeFail::new(
                    "spec_unknown_rule",
                    format!(
                        "规格「{}」选了 {} 没有的规则 {bad}（可用：{declared:?}）",
                        record.name, record.plugin_id
                    ),
                )
                .with_params(serde_json::json!({
                    "specName": record.name,
                    "pluginId": record.plugin_id,
                    "rule": bad,
                })));
            }
        }
    }
    Ok(JudgeSpec { record })
}

/// 一次 run 的身份 = **谁执行就记谁**（KI-39 修法 A，卡 task）。
///
/// * 链模式（显式 `actions[]` 或处理类规格）：run 记链插件包 id（产物挂在它名下）；
/// * 非链模式：记**实际开跑的插件身份**（规格的 `pluginId`，无规格 = 内置电商包）。
///
/// 旧实现非链模式恒记「当前行业元数据插件」→ 处理类规格（actions 为空）跑出来的 run 变成
/// `packId=com.soujpg.pack.ecommerce, kinds=["judge"], rows=[]`：判定面查不到产物、也不报错
/// （KI-39 真机现象 2 的第二个根因）。
fn run_pack_identity(chain_pack: &Option<String>, exec_pack: &str) -> String {
    chain_pack.clone().unwrap_or_else(|| exec_pack.to_string())
}

/// POST /api/compliance/judge —— 入队 + 后台判定。
///
/// body：`{assetIds?, all?, packId?, scope?: "asset"|"group"|"both", limit?, workers?}`。
/// 缺省语义（产品旅程默认值）：当前激活包 + 「未判定」资产 + 逐图与组级一起跑。
pub async fn judge(State(state): State<AppState>, Json(req): Json<JudgeReq>) -> Response {
    let scope = req.scope.clone().unwrap_or_else(|| "both".into());
    if !matches!(scope.as_str(), "asset" | "group" | "both") {
        return err(
            StatusCode::BAD_REQUEST,
            "bad_scope",
            format!("scope 只支持 asset|group|both，收到 {scope}"),
        );
    }
    // N1：`specId` → 规格层（存在/启用/插件在装/版本钉/规则子集；配错当场 400 且带去处）
    let judge_spec = match req.specId {
        Some(id) => {
            let data_dir = state.data_dir.clone();
            let loaded = tokio::task::spawn_blocking(move || -> Result<JudgeSpec, JudgeFail> {
                let conn = core_storage::open(&crate::infer::db_path(&data_dir))
                    .map_err(|e| JudgeFail::new("db_open", e.to_string()))?;
                load_judge_spec(&conn, &data_dir, id)
            })
            .await;
            match loaded {
                Ok(Ok(v)) => Some(v),
                Ok(Err(f)) => return f.http(),
                Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
            }
        }
        None => None,
    };
    // 规格的规则清单随 job 下发（plugin 通道 = 真正的子集；legacy 只接受「全部」，
    // 由 svc-pipeline 侧比对后决定放行/报错 —— 口径在数据层统一，不在入口分叉）
    let spec_rules: Vec<String> = judge_spec.as_ref().map(JudgeSpec::rules).unwrap_or_default();
    if !spec_rules.is_empty() && scope == "group" {
        return err(
            StatusCode::BAD_REQUEST,
            "spec_rules_scope",
            "规则子集只支持逐图判定（scope=asset|both）：组级判定必须跑包内全部规则",
        );
    }
    // KI-31：本次执行开哪个插件包 —— 有规格就按规格的 `pluginId`（规格属于哪个插件就开哪个），
    // 没规格 = 内置电商包（老调用方/无规格入口）。**不许**再硬编码电商包。
    let judge_plugin_id = judge_spec
        .as_ref()
        .map(|s| s.record.plugin_id.clone())
        .unwrap_or_else(|| PLUGIN_ECOMMERCE_ID.to_string());
    // PROC-RES-1：规格的 actions 非空 = **处理类规格** —— 与显式 `actions[]` 走同一条链机制
    // （链 = 规格动作序、链插件包 = 规格的 pluginId），不再落到「判定语义 + 当前配置包」
    // 的旧路径（旧路径三连坏：链没执行、packId 记成配置包、结果行查不到）。
    let spec_chain_actions: Vec<String> = match &judge_spec {
        Some(s) => match s.action_names() {
            Ok(v) => v,
            Err(f) => return f.http(),
        },
        None => Vec::new(),
    };
    let spec_chain_pack: Option<String> =
        (!spec_chain_actions.is_empty()).then(|| judge_plugin_id.clone());
    // K07-I：参数**先解析校验**（读包内 schema，不启子进程）。这一步刻意放在 CAS 之前：
    // 参数不合法就当场 400 —— 不占运行槽、不建 run 行、不入队（"参数错了先跑 1 张就看见"）。
    // N6e：动作链模式按**点名的插件包**解析参数（不是 commerce 包的）。
    let judge_params = {
        let data_dir = state.data_dir.clone();
        // 规格优先：给了 specId 就以规格的 paramsDiff 当「规格层」，`specOptions` 不再参与
        // （两层同时给等于没说清谁赢；规格是持久化那一层，任务覆盖仍走 `options`）。
        // PROC-RES-1：处理类规格（actions 非空）在此基础上把各动作的 params 平铺进来
        // —— 否则链上动作只拿到 schema 默认值（规格写 800、实际跑 1600 的根因）。
        let spec = match &judge_spec {
            Some(s) => Some(if spec_chain_actions.is_empty() {
                s.params_layer()
            } else {
                s.chain_params_layer()
            }),
            None => req.specOptions.clone(),
        };
        let thresholds = judge_spec.as_ref().map(JudgeSpec::thresholds);
        let task = req.options.clone();
        let chain_pack = req.actions.as_ref().and_then(|_| req.packId.clone());
        let plugin_id = judge_plugin_id.clone();
        let resolved = tokio::task::spawn_blocking(
            move || -> Result<Option<JudgeParams>, JudgeFail> {
                let conn = core_storage::open(&crate::infer::db_path(&data_dir))
                    .map_err(|e| JudgeFail::new("db_open", e.to_string()))?;
                resolve_judge_params_for(
                    &conn,
                    &data_dir,
                    chain_pack.as_deref().or(Some(plugin_id.as_str())),
                    spec.as_ref(),
                    task.as_ref(),
                    thresholds.as_ref(),
                )
            },
        )
        .await;
        match resolved {
            Ok(Ok(v)) => v,
            Ok(Err(f)) => return f.http(),
            Err(e) => {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "join",
                    format!("参数解析线程失败：{e}"),
                )
            }
        }
    };
    if state
        .compliance
        .running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return err(
            StatusCode::CONFLICT,
            "already_running",
            "已有合规判定任务在跑（/api/compliance/judge/status 可看进度）",
        );
    }
    state.compliance.cancel.store(false, Ordering::SeqCst);
    *state.compliance.error.lock().unwrap() = None;
    *state.compliance.report.lock().unwrap() = None;
    *state.compliance.progress.lock().unwrap() = None;

    let data_dir = state.data_dir.clone();
    // KI-28：运行时资源随闭包进阻塞线程（`state` 本身后面还要用：`state.compliance`/`spawn_judge_job`）
    let pack_runtime = state.pack_runtime.clone();
    let scope_for_job = scope.clone();
    let req2 = JudgeReq {
        assetIds: req.assetIds.clone(),
        all: req.all,
        packId: req.packId.clone(),
        scope: Some(scope.clone()),
        limit: req.limit,
        workers: req.workers,
        // 参数已在上面解析成快照，候选/入队不再需要原始层
        specOptions: None,
        options: None,
        actions: req.actions.clone(),
        specId: None,
    };
    // KI-28：参数快照随闭包进阻塞线程（开插件时要用它当 options；run 行要落它当快照）
    let params_for_job = judge_params.clone();
    // N1：规格快照 + 规则子集随闭包进阻塞线程（run 行落快照；入队带 rules）
    let spec_for_job = judge_spec.as_ref().map(JudgeSpec::snapshot);
    let rules_for_job = spec_rules.clone();
    // 库操作（装载包 / 取候选 / 入队）走独立连接：不长时间占住 HTTP 侧那把全局锁
    let prepared = tokio::task::spawn_blocking(move || -> Result<JudgePrepared, JudgeFail> {
        let db = crate::infer::db_path(&data_dir);
        let conn = core_storage::open(&db).map_err(|e| JudgeFail::from(e.to_string()))?;
        let runs = SqliteComplianceRunRepo::new(&conn);
        // 进程级上一次没收工的执行先自愈（否则新 run 与僵尸 run 会并排显示「都在跑」）
        let _ = runs.interrupt_stale(&svc_pipeline::COMPLIANCE_KINDS);
        // 动作链（N6e）：显式给了 `actions[]` 时 packId 点名的是**插件包 id**（处理类包没有
        // 配置包对应物），链不读判定插件的规则；`pack` 只为 run 的 lint/报告口径取当前插件。
        //
        // PROC-RES-1：处理类规格（specId 的 actions 非空）与显式 `actions[]` 同一条链机制 ——
        // 显式链优先（给了 `actions[]` 就用它），否则用规格动作链，链插件包恒为**规格的
        // pluginId**（不看请求里的 packId）：处理 run 的身份列在它自己名下，不再记成当前
        // 配置包（旧路径 run.params.packId=ecommerce，结果视图按它解析 kinds 把处理 run
        // 画成审核列的根因）。
        let chain_mode = req2.actions.is_some() || !spec_chain_actions.is_empty();
        let chain_pack_id: Option<String> = if chain_mode {
            if req2.actions.is_some() {
                let id = req2.packId.clone().ok_or_else(|| {
                    "动作链必须用 packId 点名插件包（如 com.soujpg.pack.imaging）".to_string()
                })?;
                Some(id)
            } else {
                spec_chain_pack.clone()
            }
        } else {
            None
        };
        let chain_actions: Vec<String> = req2
            .actions
            .clone()
            .unwrap_or_else(|| spec_chain_actions.clone());
        // N6e：链的**形态**校验（不需要子进程的三条）先做，错就当场 400，不白拉插件：
        // * 必须点名资产（不给「全库批量处理」一键误炸）；
        // * 只按单资产执行（组级属于判定作用域，链没有组语义）；
        // * 空链 = 没动作可跑（后面还有「未知动作/rename 链首/超长」三条，需读包内动作清单）。
        if chain_mode {
            if req2.assetIds.as_ref().map(Vec::is_empty).unwrap_or(true) {
                return Err(JudgeFail::new(
                    "chain_assets_missing",
                    "动作链必须用 assetIds 点名资产（不提供「全库批量处理」这种一键误炸）",
                ));
            }
            if scope_for_job == "group" {
                return Err(JudgeFail::new(
                    "chain_scope_unsupported",
                    "动作链按单资产执行（scope 只支持 asset|both）",
                ));
            }
            if chain_actions.is_empty() {
                return Err(JudgeFail::new(
                    "chain_empty",
                    "动作链为空：actions 至少要给一个动作",
                ));
            }
        }
        // 行业元数据 = 插件自带；判定行恒落在插件身份 id 下（IP-K16：没有第二套引擎/对象）
        //
        // **装载失败即拒**（400 judge_prepare）：旧实现吞成空壳包 → 200 + candidates=0 让
        // 用户以为「审核成功」，实际规则一条没读（`rules.json` 里「配了不读」的键正是这么
        // 被埋掉的）。错误信息点名坏键，直接进 HTTP message。
        let pack = plugin_meta_pack_checked(&conn, &data_dir)?;
        // 判定模式：packId 若给了就必须是当前插件（动作链模式的 packId 是链要用的处理插件，另算）
        if !chain_mode {
            if let Some(id) = req2.packId.as_deref() {
                if id != pack.id {
                    return Err(JudgeFail::new(
                        "pack_not_installed",
                        format!("包 {id} 不存在：合规判定只认已装的处理插件（当前 {}）", pack.id),
                    ));
                }
            }
        }
        let out_dir = fixes_dir(&data_dir);
        std::fs::create_dir_all(&out_dir).map_err(|e| format!("建产物目录失败: {e}"))?;
        let out_s = out_dir.to_string_lossy().to_string();

        // 执行通道恒为插件子进程（端口 svc_pipeline::PackJudge，本层装配）：没有第二套引擎，
        // 也没有引擎开关。插件在此处**一次性**开好：起网关 + 拉子进程 + pack.info 核对 ——
        // 失败即 400（配置错不许拖到 N 张图各落一堆 undetermined 才让人猜），且不产生 run 行。
        //
        // N6e：动作链模式按**点名的插件包**开通道（链本来就走插件通道）。
        let run_pack_id = chain_pack_id.clone().unwrap_or_else(|| judge_plugin_id.clone());
        let pack_dir = plugin_pack_dir_for(&conn, &data_dir, &run_pack_id).ok_or_else(|| {
            JudgeFail::plugin_not_installed(&run_pack_id, &data_dir.join(PACKS_DIR))
        })?;
        let (manifest, ident) = pack_host::load_pack(&pack_dir)
            .map_err(|e| format!("处理插件装载失败（{}）：{e}", pack_dir.display()))?;
        // 包身份必须与点名的 id 一致（DAM_PACK_DIR 强制目录会绕过 id 解析）
        if let Some(pid) = &chain_pack_id {
            if ident.id != *pid {
                return Err(JudgeFail::new(
                    "chain_pack_mismatch",
                    format!(
                        "动作链指定的插件包 {pid} 解析到了别的包（{}）：检查 DAM_PACK_DIR / 安装记录",
                        ident.id
                    ),
                ));
            }
        }
        register_pack(&conn, &manifest, &ident, &pack_dir)?;
        let vl_cfg = vl_config(&conn);
        let plugin = PluginRuntime::open(
            &pack_dir,
            &out_dir,
            vl_cfg,
            &pack_runtime,
            // K07-I：全量生效参数（老包 = 透传合并后的键；空 = 只有 workdir）
            params_for_job
                .as_ref()
                .map(JudgeParams::effective_value)
                .unwrap_or_else(|| json!({})),
        )
        .map_err(|e| format!("处理插件不可用：{e}"))?;
        // 动作清单校验（未知动作 400 点名 + rename 不许做链首 + 超长）——
        // 权威清单是 `pack.info.actions[]`，只有拉起插件才读得到，所以放在 open 之后。
        // KI-40（真机 run56「四倍高清」恒红）：**手建处理规格的链规整** —— 规格 actions
        // 为空、规格插件 ≠ 判定包、且包声明了动作时，规整成单动作链再校验：
        // ① paramsDiff 的 `action` 命中声明 → 用之（imaging 多动作包）；② 恰好声明一个
        // → 用之（segment/upscale）；③ 其余 400 点名，宿主不替用户猜动作。
        // 参数面不受影响（PluginRuntime::open 已带规格生效参数，链上动作拿到的就是它）。
        // 电商判定规格（pluginId = ecommerce）不走这条，维持 judge 语义不变。
        let mut chain_mode = chain_mode;
        let mut chain_actions = chain_actions;
        let mut chain_pack_id = chain_pack_id;
        if !chain_mode && judge_plugin_id != PLUGIN_ECOMMERCE_ID {
            let identity = svc_pipeline::PackJudge::identity(&plugin);
            if !identity.actions.is_empty() {
                let diff_action = params_for_job.as_ref().and_then(|p| {
                    p.effective_value()
                        .get("action")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                });
                let picked = diff_action
                    .filter(|a| identity.actions.iter().any(|d| d == a))
                    .or_else(|| {
                        (identity.actions.len() == 1).then(|| identity.actions[0].clone())
                    });
                match picked {
                    Some(action) => {
                        chain_actions = vec![action];
                        chain_pack_id = Some(judge_plugin_id.clone());
                        chain_mode = true;
                        // 链形态的两条前置校验在 open 之前跑过（当时还没规整），这里补：
                        if req2.assetIds.as_ref().map(Vec::is_empty).unwrap_or(true) {
                            return Err(JudgeFail::new(
                                "chain_assets_missing",
                                "动作链必须用 assetIds 点名资产（不提供「全库批量处理」这种一键误炸）",
                            ));
                        }
                        if scope_for_job == "group" {
                            return Err(JudgeFail::new(
                                "chain_scope_unsupported",
                                "动作链按单资产执行（scope 只支持 asset|both）",
                            ));
                        }
                    }
                    None => {
                        let names = identity
                            .actions
                            .iter()
                            .map(|a| a.as_str())
                            .collect::<Vec<_>>()
                            .join("、");
                        return Err(JudgeFail::new(
                            "spec_action_ambiguous",
                            format!(
                                "规格「{}」没有声明动作，而能力包声明了 {} 个动作（{names}）：\
                                 编辑方案带上动作，或改用「官方默认」方案",
                                judge_spec.as_ref().map(|s| s.record.name.as_str()).unwrap_or("?"),
                                identity.actions.len()
                            ),
                        ));
                    }
                }
            }
        }
        if chain_mode {
            let identity = svc_pipeline::PackJudge::identity(&plugin);
            svc_pipeline::validate_actions(&chain_actions, &identity.actions)
                .map_err(|e| format!("动作链参数不合法：{e}"))?;
        }
        // 默认 scope=both 时静默降级为 asset（界面语义仍是「审这批图」），显式要 group 才 400。
        if scope_for_job == "group" {
            return Err(JudgeFail::new(
                "scope_group_unsupported",
                "组级判定由处理插件自己执行（协议 v1.1 judge 为单资产调用）：请用 scope=asset|both",
            ));
        }
        let scope_used = "asset";
        // 候选 = 「该插件下还没判过的资产」；判定行落在插件身份 id 下，候选必须同口径查，
        // 否则每轮都会把同一批图当成「未判定」重跑。
        let candidate_pack_id = svc_pipeline::PackJudge::identity(&plugin).id.clone();
        let ids = judge_candidates(&conn, &candidate_pack_id, &req2)?;
        // N6e：动作链点名了资产却一个都查不到 → 400（不静默起一个空 run 让人以为跑了）
        if chain_mode && ids.is_empty() {
            return Err(JudgeFail::new(
                "chain_assets_not_found",
                "assetIds 里的资产都不存在或已删除：动作链没有可处理的对象",
            ));
        }

        // CMP-JOB-01：**一次批量审核 = 一个 run**（用户裁决）。
        // 落 run 行在入队之前：任务行必须带 runId，否则「这一次」的统计无从谈起
        // （旧口径只有全局累计，assetFailed=30 会永久挂在那儿与「本轮 failed=0」并排误导）。
        // N6e：任务行/参数的 packId 在动作链模式下是**插件包 id**（处理类产物挂在插件名下）。
        // KI-39（task）：非链模式下**不能用「当前行业元数据插件」当 run 身份** ——
        // 规格属于哪个插件，行就落在哪个插件名下（`candidate_pack_id` 就是实际开跑的插件
        // 身份），否则处理类规格会被记成电商判定 run（现象：packId=ecommerce / kinds=["judge"]
        // / rows=[]，产物查不到还不报错）。
        let job_pack_id = run_pack_identity(&chain_pack_id, &candidate_pack_id);
        let params = json!({
            "scope": scope_used,
            "packId": job_pack_id.clone(),
            // IP-K16：执行通道恒为插件（判定行落在插件身份 id 下，见 candidate_pack_id）
            "engine": "plugin",
            "verdictPackId": candidate_pack_id.clone(),
            "candidates": ids.len(),
            "limit": req2.limit,
            "workers": req2.workers,
            "all": req2.all.unwrap_or(false),
            // N6e：动作链（执行序）；缺省/空 = 既有判定/修复语义
            "actions": chain_actions.clone(),
            // K07-I：本次执行的参数快照（全量生效值 + 是否由插件 schema 声明）。
            // 重跑/复盘按这份快照复现，不重新合并默认值（否则插件升级默认值会让历史结果不可复现）。
            "params": params_for_job.as_ref().map(JudgeParams::snapshot),
            // N1：本次执行引用的规格（含规则/阈值/参数 diff 快照）—— 重跑要说得清「用哪条规格」
            "spec": spec_for_job,
        })
        .to_string();
        let run_id = runs
            .start(svc_pipeline::COMPLIANCE_KIND, Some(&params))
            .map_err(|e| format!("开 run 失败: {e}"))?;
        // 上一次取消/中断后留在队列里的 pending 任务认领到本轮：它们确实会在本轮被执行，
        // 不认领就会出现「跑了但没进本 run 的 total」（迁移前 runId IS NULL 的老行不认领）
        let adopted = runs
            .adopt_pending(run_id, &svc_pipeline::COMPLIANCE_KINDS)
            .map_err(|e| format!("认领上次未执行任务失败: {e}"))?;

        let enqueued = (|| -> Result<(usize, usize), String> {
            let mut asset_jobs = 0usize;
            if scope_used != "group" {
                // N6e × N1（N链合流）：动作链 + 规格规则子集一起进 payload（同一防重语义）
                asset_jobs = svc_pipeline::compliance_queue::compliance_enq_actions_rules_in_run(
                    &conn,
                    &ids,
                    &job_pack_id,
                    Some(&out_s),
                    Some(run_id),
                    &chain_actions,
                    &rules_for_job,
                )
                .map_err(|e| e.to_string())?;
            }
            let mut group_jobs = 0usize;
            if scope_used != "asset" {
                group_jobs = svc_pipeline::compliance_queue::compliance_enq_group_in_run(
                    &conn,
                    &pack,
                    &ids,
                    Some(&out_s),
                    Some(run_id),
                )
                .map_err(|e| e.to_string())?;
            }
            Ok((asset_jobs, group_jobs))
        })();
        let (asset_jobs, group_jobs) = match enqueued {
            Ok(v) => v,
            Err(e) => {
                // 入队失败 = 这次执行根本没跑起来 → run 落 failed（不许留一个永远 running 的空 run）
                let _ = runs.finish(
                    run_id,
                    run_status::FAILED,
                    Some(&e),
                    &svc_pipeline::COMPLIANCE_KINDS,
                );
                return Err(JudgeFail::from(e));
            }
        };
        Ok(JudgePrepared {
            job_pack_id,
            pack,
            plugin,
            // N6e：动作链（后台线程按 job payload 逐步执行，这里只回给界面看执行序）
            actions: chain_actions.clone(),
            candidates: ids.len(),
            asset_jobs,
            group_jobs,
            run_id,
            adopted,
        })
    })
    .await;

    let prepared = match prepared {
        Ok(Ok(p)) => p,
        Ok(Err(f)) => {
            state.compliance.running.store(false, Ordering::SeqCst);
            *state.compliance.error.lock().unwrap() = Some(f.message.clone());
            return f.http();
        }
        Err(e) => {
            state.compliance.running.store(false, Ordering::SeqCst);
            return err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string());
        }
    };

    let workers = req.workers.unwrap_or(4).clamp(1, 16);
    // IP-K05：插件通道把运行时（网关 + 子进程）交给后台线程；legacy 传 None（走内联引擎）
    let plugin: Option<Arc<dyn svc_pipeline::PackJudge>> =
        Some(Arc::new(prepared.plugin) as Arc<dyn svc_pipeline::PackJudge>);
    spawn_judge_job(
        state.clone(),
        prepared.pack.clone(),
        plugin,
        workers,
        prepared.run_id,
    );

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            // N6e：实际挂任务的包 id（动作链 = 插件包 id；判定 = 配置包 id）
            "packId": prepared.job_pack_id,
            "scope": scope,
            // N6e：动作链（执行序；空 = 判定/修复）
            "actions": prepared.actions,
            // IP-K05：实际执行通道（plugin 通道的 scope 恒为 asset，见下 groupSkipReason）
            "engine": "plugin",
            "groupSkipped": prepared.group_jobs == 0 && scope != "asset",
            "candidates": prepared.candidates,
            "queuedAssets": prepared.asset_jobs,
            "queuedGroups": prepared.group_jobs,
            // CMP-JOB-01：这一次执行（run）的 id + 从上次未完执行里认领的任务数。
            // 界面拿 runId 去 /api/compliance/jobs/{runId} 看这次自己的明细。
            "runId": prepared.run_id,
            "adoptedPending": prepared.adopted,
            "workers": workers,
        })),
    )
        .into_response()
}

/// 后台判定线程的入参（包已装载好，避免线程里再解析一遍 YAML）。
struct JudgePrepared {
    /// N6e：任务行/结果行/回执实际挂的包 id（动作链 = 插件包 id；判定 = 配置包 id）
    job_pack_id: String,
    pack: Pack,
    /// 处理插件运行时（网关 + 子进程已就绪）—— IP-K16 起这是**唯一**执行通道
    plugin: PluginRuntime,
    /// N6e：本次执行的动作链（空 = 既有判定/修复语义）
    actions: Vec<String>,
    candidates: usize,
    asset_jobs: usize,
    group_jobs: usize,
    /// CMP-JOB-01：本次执行（run）的 id —— task 全部挂它，收工时按它汇总统计
    run_id: i64,
    /// 从上次取消/中断遗留的 pending 任务里认领过来的条数（>0 说明这次在续跑）
    adopted: usize,
}

/// 后台线程：并行跑组级 → 逐图两阶段，逐 job 写进度快照。
///
/// CMP-JOB-01：入口带上本次 run 的 id —— 并行队列把「跳过」记进该 run，收工后这里落
/// `status/finishedAt/total/done/failed/skipped/cancelled`（一次执行一行，历史互不改写）。
fn spawn_judge_job(
    state: AppState,
    pack: Pack,
    plugin: Option<Arc<dyn svc_pipeline::PackJudge>>,
    workers: usize,
    run_id: i64,
) {
    let slot = state.compliance.clone();
    let cancel = slot.cancel.clone();
    let data_dir = state.data_dir.clone();
    tokio::task::spawn_blocking(move || {
        let db = crate::infer::db_path(&data_dir);
        let factory: svc_pipeline::ConnFactory = {
            let db = db.clone();
            Arc::new(move || {
                let c = rusqlite::Connection::open(&db)?;
                c.busy_timeout(std::time::Duration::from_secs(15))?;
                c.pragma_update(None, "journal_mode", "WAL")?;
                Ok(c)
            })
        };
        // VL 配置要读设置：独立连接（只读，不占全局锁）
        let vl = core_storage::open(&db).ok().and_then(|c| vl_config(&c));
        let res = svc_pipeline::run_compliance_queue_par_in_run_engine(
            factory,
            &pack,
            vl,
            plugin,
            workers,
            cancel.as_ref(),
            |p: svc_pipeline::ComplianceProgress| {
                *slot.progress.lock().unwrap() = Some(p);
            },
            Some(run_id),
        );
        // 收工记账：done 只表示「这次执行跑完了」——即使有 task 失败也仍然是 done；
        // 用户取消 → cancelled；引擎级错误 → failed（错误文本一并落库，界面可解释）
        let (status, err) = match &res {
            Ok((report, _vl_snap)) => (
                if report.cancelled {
                    run_status::CANCELLED
                } else {
                    run_status::DONE
                },
                None,
            ),
            Err(e) => (run_status::FAILED, Some(e.to_string())),
        };
        match core_storage::open(&db) {
            Ok(c) => {
                let runs = SqliteComplianceRunRepo::new(&c);
                if let Err(e) =
                    runs.finish(run_id, status, err.as_deref(), &svc_pipeline::COMPLIANCE_KINDS)
                {
                    eprintln!("compliance run{run_id} 收工记账失败: {e}");
                }
            }
            Err(e) => eprintln!("compliance run{run_id} 收工记账打不开库: {e}"),
        }
        match res {
            Ok((report, _vl_snap)) => {
                *slot.report.lock().unwrap() = Some(report);
            }
            Err(e) => {
                *slot.error.lock().unwrap() = Some(e.to_string());
            }
        }
        slot.cancel.store(false, Ordering::SeqCst);
        slot.running.store(false, Ordering::SeqCst);
    });
}

/// POST /api/compliance/judge/cancel —— 置取消旗标：本轮当前 job 跑完即停，
/// 已 claim 未处理回 pending（断点续跑口径，与 tag/ocr 一致）。
pub async fn judge_cancel(State(state): State<AppState>) -> impl IntoResponse {
    if !state.compliance.running.load(Ordering::SeqCst) {
        return err(StatusCode::CONFLICT, "not_running", "没有运行中的合规判定");
    }
    state.compliance.cancel.store(true, Ordering::SeqCst);
    (StatusCode::OK, Json(json!({ "ok": true, "cancelling": true }))).into_response()
}

/// GET /api/compliance/judge/status —— 运行态 + 进度 + 最近一轮汇总 + 队列计数
/// （资产口径与组级口径分开：组级 job 数 = 组数 × 作用域数，不是资产数）。
///
/// CMP-JOB-01（前端还没切到 `/jobs` 时保持字段兼容）：
/// - `queue` 保持旧字段名与**旧语义**（全库历史累计）→ 显式标 `legacyCumulative: true`，
///   免得再有人拿它当「这一次」的数字；
/// - `queueCurrent` = **本 run** 口径（同上字段名，换成 `WHERE runId = 本次`），
///   其中 `*Pending` / `queued` 是**排队中**，`cancelled` 才是「取消后没执行」（运行中恒 0）；
/// - `run` = 本 run 的结果统计（runId/status/时间/成功/失败/跳过/总数/耗时）。
pub async fn judge_status(State(state): State<AppState>) -> Response {
    struct Snapshot {
        queue: (i64, i64, i64, i64, i64, i64),
        run: Option<serde_json::Value>,
        queue_current: Option<serde_json::Value>,
    }
    let data_dir = state.data_dir.clone();
    let snap = tokio::task::spawn_blocking(move || -> Result<Snapshot, String> {
        let conn = core_storage::open(&crate::infer::db_path(&data_dir))
            .map_err(|e| e.to_string())?;
        let (ad, af, ap) = svc_pipeline::compliance_queue::compliance_stats(&conn)
            .map_err(|e| e.to_string())?;
        let (gd, gf, gp) = svc_pipeline::compliance_queue::compliance_group_stats(&conn)
            .map_err(|e| e.to_string())?;
        // 本 run：正在跑的那一次优先，否则最近一次（界面据此显示「这一次」的统计）
        let runs = SqliteComplianceRunRepo::new(&conn);
        let current = match runs.running().map_err(|e| e.to_string())? {
            Some(r) => Some(r),
            None => runs.latest().map_err(|e| e.to_string())?,
        };
        let (run, queue_current) = match &current {
            Some(r) => (Some(run_view(&conn, r)?), Some(run_queue_view(&conn, r)?)),
            None => (None, None),
        };
        Ok(Snapshot {
            queue: (ad, af, ap, gd, gf, gp),
            run,
            queue_current,
        })
    })
    .await;
    match snap {
        Ok(Ok(Snapshot {
            queue: (ad, af, ap, gd, gf, gp),
            run,
            queue_current,
        })) => (
            StatusCode::OK,
            Json(json!({
                "running": state.compliance.running.load(Ordering::SeqCst),
                "cancelling": state.compliance.cancel.load(Ordering::SeqCst),
                "progress": *state.compliance.progress.lock().unwrap(),
                "report": *state.compliance.report.lock().unwrap(),
                "error": *state.compliance.error.lock().unwrap(),
                // 历史累计（旧字段，保持兼容）——**不是**本 run 的数字
                "legacyCumulative": true,
                "queue": {
                    "assetDone": ad, "assetFailed": af, "assetPending": ap,
                    "groupDone": gd, "groupFailed": gf, "groupPending": gp,
                },
                // 本 run 口径（界面该用这个）
                "run": run,
                "queueCurrent": queue_current,
            })),
        )
            .into_response(),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, "stats", e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
    }
}

// ---------------------------------------------------------------- 执行（run）列表与明细
//
// CMP-JOB-01 的产品视图：**一次批量审核 = 一个独立 job**。用户要能按时间看到
// 「17:03 这次：成功 24/失败 0/跳过 0/耗时 18s」「17:20 这次：成功 3/失败 1/…」，
// 历史互不改写、不叠加；点开某一行看这次执行内部的 task 明细。

#[derive(Debug, Deserialize)]
pub struct JobsQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// run 行 → 界面视图。
///
/// 计数口径：**收工后**用行上的快照（一次执行的结果不会因为后人重跑而漂移）；
/// **运行中**用实时队列计数（行里的计数器只在收工时写实），保证进度条与列表一致。
///
/// 语义铁律（卡 task / GAP-2）：
/// - `cancelled` **只表示「取消/中断后没执行」的条数**，只有收工的行才可能有值；
///   运行中一律 `0` —— 以前这里复用 `live.cancelled()`（实现就是 pending），
///   把「6 条还在排队」显示成「6 条未执行」，任何消费方都会误读。
/// - 运行中没结算的条数走 `queued`（pending + 执行中）。所以恒等式分两段：
///   运行中 `total = done + failed + skipped + queued`；收工后
///   `total = done + failed + skipped + cancelled`。
fn run_view(conn: &Connection, run: &RunRow) -> Result<serde_json::Value, String> {
    let repo = SqliteComplianceRunRepo::new(conn);
    let counts = repo
        .counts(run.run_id, &svc_pipeline::COMPLIANCE_KINDS)
        .map_err(|e| e.to_string())?;
    // DM-B2（KI-37）：列表行要能一眼读出「这次判了多少张、过/没过/待看各几张」——
    // 一条 GROUP BY 拿到每张图的三态（不 N+1），聚合在 Rust 里按「一张图一个结论」收口。
    let verdicts = run_verdict_counts(conn, run.run_id)?;
    let live = counts.with_skipped(run.skipped);
    let (total, done, failed, skipped, cancelled, queued) = if run.is_running() {
        (
            live.total,
            live.done,
            live.failed,
            live.skipped,
            0,          // 运行中没有「未执行」这一说：任务都还在队列里
            live.queued(),
        )
    } else {
        (
            run.total,
            run.done,
            run.failed,
            run.skipped,
            run.cancelled,
            0, // 收工后队列不再属于这次执行：未执行数看 cancelled
        )
    };
    Ok(json!({
        "runId": run.run_id,
        "status": run.status,
        "statusLabel": run_status_label(&run.status),
        "startedAt": run.started_at,
        "finishedAt": run.finished_at,
        "total": total,
        "done": done,
        "failed": failed,
        "skipped": skipped,
        "cancelled": cancelled,
        // 运行中：排队中 + 执行中（没结算）。收工后恒为 0，别拿它当结果统计。
        "queued": queued,
        "durationMs": run.duration_ms(),
        "error": run.error,
        // 这次判了多少张图（distinct 资产），以及每张图的三态结论分布。
        // 处理类 run 没有判定行 → 0/None，界面不画判定分布（处理结果没有「合格/不合格」）。
        "totalAssets": verdicts.total_assets,
        "verdictCounts": if verdicts.total_assets > 0 {
            json!({
                "pass": verdicts.pass,
                "fail": verdicts.fail,
                "undetermined": verdicts.undetermined,
            })
        } else {
            serde_json::Value::Null
        },
        "params": run.params.as_ref().and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok()),
    }))
}

/// 一次执行的图级判定分布（DM-B2）。
///
/// 口径：**一张图一个结论**。同一张图可能有多条规则行，聚合优先级
/// `fail > undetermined > pass`（有一条不过就算不过；有判不了就不算通过）——
/// 这也是用户看「未过 Y 张」时要的口径，不是「判定行数」。
///
/// 只查 `rule_verdict`（判定账本）；处理类 run 没有判定行 → 全 0，调用方出 `null`。
/// 一条 `GROUP BY assetId` 完成，行数 = 本次执行的图数，不做 N+1。
struct RunVerdictCounts {
    total_assets: i64,
    pass: i64,
    fail: i64,
    undetermined: i64,
}

fn run_verdict_counts(conn: &Connection, run_id: i64) -> Result<RunVerdictCounts, String> {
    let mut stmt = conn
        .prepare(
            "SELECT v.assetId,
                    SUM(CASE WHEN v.verdict = 'pass' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN v.verdict = 'fail' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN v.verdict = 'undetermined' OR v.verdict IS NULL THEN 1 ELSE 0 END)
             FROM rule_verdict v
             WHERE v.jobId IN (SELECT id FROM job_queue WHERE runId = ?1)
             GROUP BY v.assetId",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(rusqlite::params![run_id], |r| {
            Ok((
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut out = RunVerdictCounts { total_assets: 0, pass: 0, fail: 0, undetermined: 0 };
    for row in rows {
        let (pass, fail, undet) = row.map_err(|e| e.to_string())?;
        out.total_assets += 1;
        if fail > 0 {
            out.fail += 1;
        } else if undet > 0 {
            out.undetermined += 1;
        } else if pass > 0 {
            out.pass += 1;
        } else {
            // 只有 skipped 行（没判）：这不算「通过」，算未判定
            out.undetermined += 1;
        }
    }
    Ok(out)
}

/// 状态词 → 人话（界面直接显示，不自己映射词表）。
fn run_status_label(status: &str) -> &'static str {
    match status {
        run_status::RUNNING => "进行中",
        run_status::DONE => "已完成",
        run_status::CANCELLED => "已取消",
        run_status::INTERRUPTED => "已中断（进程重启）",
        run_status::FAILED => "执行失败",
        _ => "未知状态",
    }
}

/// 按 kind 取 (done, failed, pending)。`done` 含跳过（终态同为 done），跳过单列在 run 视图。
fn kind_counts(conn: &Connection, run_id: i64, kind: &str) -> Result<(i64, i64, i64), String> {
    let c = SqliteComplianceRunRepo::new(conn)
        .counts(run_id, &[kind])
        .map_err(|e| e.to_string())?;
    Ok((c.done, c.failed, c.pending))
}

/// 本 run 口径的队列计数（字段名与旧 `queue` 一致，便于前端机械替换）。
///
/// 注意口径：这里是**队列行状态计数**（`done` 含「跳过」，因为跳过与成功同终态），
/// 用户可见的「成功数 / 跳过数」请读 run 视图的 `done` / `skipped`（`run.done + run.skipped`
/// 才等于这里的 done 行数）。
///
/// `queued`（= assetPending + groupPending）与 run 视图的 `queued` 同义：
/// 「还在排队/执行中」。`cancelled` 只有收工后才可能是非零（= 真未执行数），
/// 运行中恒 `0` —— 这里显式给出，免得消费方再把 `*Pending` 当「未执行」报出去。
fn run_queue_view(conn: &Connection, run: &RunRow) -> Result<serde_json::Value, String> {
    let (ad, af, ap) = kind_counts(conn, run.run_id, svc_pipeline::COMPLIANCE_KIND)?;
    let (gd, gf, gp) = kind_counts(conn, run.run_id, svc_pipeline::COMPLIANCE_GROUP_KIND)?;
    Ok(json!({
        "runId": run.run_id,
        "assetDone": ad, "assetFailed": af, "assetPending": ap,
        "groupDone": gd, "groupFailed": gf, "groupPending": gp,
        "skipped": run.skipped,
        "queued": ap + gp,
        "cancelled": if run.is_running() { 0 } else { run.cancelled },
    }))
}

/// `group_by` 作用域键 → 人话（`folder` / `asset_field:<列>`）。
fn scope_label(scope: &str) -> String {
    match scope {
        "folder" => "按目录分组".to_string(),
        other => match other.strip_prefix("asset_field:") {
            Some(col) => format!("按字段 {col} 分组"),
            None => format!("按 {other} 分组"),
        },
    }
}

/// 包内规则 id 列表（人话步骤名要能点名具体规则，如 `F0_domain`）。
fn rule_ids_label(pack: Option<&Pack>, group: bool) -> Option<String> {
    let pack = pack?;
    let ids: Vec<&str> = pack
        .rules
        .iter()
        .filter(|r| r.is_group_scoped() == group)
        .map(|r| r.id.as_str())
        .collect();
    if ids.is_empty() {
        return None;
    }
    let head: Vec<&str> = ids.iter().take(3).copied().collect();
    let tail = if ids.len() > head.len() { " 等" } else { "" };
    Some(format!("{}{tail}", head.join("、")))
}

/// 一条 task 的步骤名（**必须是人话**）。
///
/// 判据（用户裁决）：步骤名要是「规则名/阶段名」这类能对上业务的东西，
/// **禁止只吐 `job313` / `group47` 这类内部 id** —— 用户点开明细是为了看懂这次在干什么。
/// 包装载不了时退回阶段名（`逐图判定` / `组级判定`），绝不编造规则名。
fn task_step(row: &TaskRow, pack: Option<&Pack>) -> String {
    let payload: serde_json::Value = row
        .payload
        .as_deref()
        .and_then(|p| serde_json::from_str(p).ok())
        .unwrap_or(serde_json::Value::Null);
    // 组级 payload 带 members；逐图 payload 只有 packId/outDir
    if let Some(members) = payload.get("members").and_then(|m| m.as_array()) {
        let scope = payload.get("scope").and_then(|s| s.as_str()).unwrap_or("");
        let group = payload
            .get("groupKey")
            .and_then(|g| g.as_str())
            .unwrap_or("无组键");
        let rules = rule_ids_label(pack, true)
            .map(|r| format!(" · 规则 {r}"))
            .unwrap_or_default();
        return format!(
            "组级判定 · {}（组 {group}，{} 张）{rules}",
            scope_label(scope),
            members.len()
        );
    }
    match rule_ids_label(pack, false) {
        Some(rules) => format!("逐图判定 · 规则 {rules}"),
        None => "逐图判定".to_string(),
    }
}

/// task 行 → 界面视图。`step` = 人话步骤名；`lastError` 兼作跳过原因（见 `mark_skipped`）。
fn task_view(row: &TaskRow, pack: Option<&Pack>) -> serde_json::Value {
    json!({
        "jobId": row.job_id,
        "targetId": row.target_id,
        "step": task_step(row, pack),
        "assetId": row.target_id,
        "status": row.status,
        "statusLabel": match row.status.as_str() {
            "pending" => "排队中",
            "running" => "执行中",
            "done" => "已完成",
            "failed" => "失败",
            _ => "未知",
        },
        "attempts": row.attempts,
        "lastError": row.last_error,
        "updatedAt": row.updated_at,
    })
}

/// 按 payload 里的 packId 装载包（同一次请求内缓存；装载不了 → None，步骤名退回阶段名）。
fn load_pack_for_tasks(
    conn: &Connection,
    data_dir: &Path,
    rows: &[TaskRow],
    cache: &mut std::collections::HashMap<String, Option<Pack>>,
) -> Vec<Option<Pack>> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let pack_id = row
            .payload
            .as_deref()
            .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
            .and_then(|v| {
                v.get("packId")
                    .and_then(|p| p.as_str())
                    .map(|s| s.to_string())
            });
        let Some(pack_id) = pack_id else {
            out.push(None);
            continue;
        };
        let entry = cache
            .entry(pack_id.clone())
            .or_insert_with(|| Some(plugin_meta_pack(conn, data_dir)));
        out.push(entry.clone());
    }
    out
}

/// GET /api/compliance/jobs —— 执行列表（按开始时间倒序；每一行是**一次独立执行**）。
pub async fn jobs_list(
    State(state): State<AppState>,
    Query(q): Query<JobsQuery>,
) -> Response {
    let data_dir = state.data_dir.clone();
    let limit = q.limit.unwrap_or(20).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).max(0);
    let res = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, String> {
        let conn = core_storage::open(&crate::infer::db_path(&data_dir))
            .map_err(|e| e.to_string())?;
        let (total, rows) = SqliteComplianceRunRepo::new(&conn)
            .list(limit, offset)
            .map_err(|e| e.to_string())?;
        let mut jobs = Vec::with_capacity(rows.len());
        for r in &rows {
            jobs.push(run_view(&conn, r)?);
        }
        Ok(json!({ "total": total, "limit": limit, "offset": offset, "jobs": jobs }))
    })
    .await;
    match res {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)).into_response(),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, "jobs", e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
    }
}

/// GET /api/compliance/jobs/current —— 正在跑的那一次（没有则 `run: null`，界面据此收起进度条）。
pub async fn jobs_current(State(state): State<AppState>) -> Response {
    let data_dir = state.data_dir.clone();
    let res = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, String> {
        let conn = core_storage::open(&crate::infer::db_path(&data_dir))
            .map_err(|e| e.to_string())?;
        let runs = SqliteComplianceRunRepo::new(&conn);
        match runs.running().map_err(|e| e.to_string())? {
            Some(r) => Ok(json!({
                "run": run_view(&conn, &r)?,
                "queue": run_queue_view(&conn, &r)?,
            })),
            None => Ok(json!({ "run": serde_json::Value::Null })),
        }
    })
    .await;
    match res {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)).into_response(),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, "current", e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
    }
}

/// GET /api/compliance/jobs/{runId} —— 一次执行 + 它的 task 明细（步骤名是人话）。
pub async fn job_detail(
    State(state): State<AppState>,
    AxumPath(run_id): AxumPath<i64>,
    Query(q): Query<JobsQuery>,
) -> Response {
    let data_dir = state.data_dir.clone();
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let offset = q.offset.unwrap_or(0).max(0);
    let res = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, String> {
        let conn = core_storage::open(&crate::infer::db_path(&data_dir))
            .map_err(|e| e.to_string())?;
        let repo = SqliteComplianceRunRepo::new(&conn);
        let Some(run) = repo.get(run_id).map_err(|e| e.to_string())? else {
            return Err(format!("run {run_id} 不存在"));
        };
        let (task_total, rows) = repo.tasks(run_id, limit, offset).map_err(|e| e.to_string())?;
        let mut cache: std::collections::HashMap<String, Option<Pack>> = Default::default();
        let packs = load_pack_for_tasks(&conn, &data_dir, &rows, &mut cache);
        let tasks: Vec<serde_json::Value> = rows
            .iter()
            .zip(packs.iter())
            .map(|(r, p)| task_view(r, p.as_ref()))
            .collect();
        Ok(json!({
            "run": run_view(&conn, &run)?,
            "taskTotal": task_total,
            "limit": limit,
            "offset": offset,
            "tasks": tasks,
        }))
    })
    .await;
    match res {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)).into_response(),
        Ok(Err(e)) if e.contains("不存在") => {
            err(StatusCode::NOT_FOUND, "run_not_found", e)
        }
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, "job_detail", e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- DECouple-BE：L3 下钻 + 按 job 导出

#[derive(Debug, Deserialize)]
pub struct JobVerdictsQuery {
    /// pass | fail | undetermined | unjudged | all（缺省 all）
    pub status: Option<String>,
    /// 只看某一张图（L3 单资产下钻）
    #[serde(alias = "assetId")]
    pub asset_id: Option<i64>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// GET /api/compliance/jobs/{runId}/verdicts —— **一次执行的逐资产结论**（L2/L3 下钻）。
///
/// 口径（用户裁决 2026-10-31）：图库域与 job 域解耦，「当前生效结论」不落在资产上；
/// 这里按 **job 账本（该 run 的判定行）+ human_review** 查询时合成：
///   `effectiveStatus = humanStatus ?? machineStatus`，人审按内容版本判活（资产变过 → 失效）。
/// 图库/资产接口不再下发任何资产级判定字段（`finalStatus`/`assetHumanReview` 已废弃）。
pub async fn job_verdicts(
    State(state): State<AppState>,
    AxumPath(run_id): AxumPath<i64>,
    Query(q): Query<JobVerdictsQuery>,
) -> Response {
    let data_dir = state.data_dir.clone();
    let status = q.status.clone().unwrap_or_else(|| "all".to_string());
    let want = if status == "all" {
        None
    } else {
        match svc_library::compliance_view::AssetComplianceState::parse(&status) {
            Some(s) => Some(s),
            None => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "bad_status",
                    format!("status 只支持 pass|fail|undetermined|unjudged|all，收到: {status}"),
                )
            }
        }
    };
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let offset = q.offset.unwrap_or(0).max(0);
    let asset_filter = q.asset_id;
    let res = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, String> {
        let conn = core_storage::open(&crate::infer::db_path(&data_dir))
            .map_err(|e| e.to_string())?;
        // run 必须存在（不存在 → 404，而不是「空结果」假装跑过）
        if SqliteComplianceRunRepo::new(&conn)
            .get(run_id)
            .map_err(|e| e.to_string())?
            .is_none()
        {
            return Err(format!("run {run_id} 不存在"));
        }
        let pack = plugin_meta_pack(&conn, &data_dir);
        // IP-K10-FIX：判定行族 = 引擎写侧包 id
        let pack_id = verdict_pack_id(&conn, &data_dir);
        let mut statuses = svc_library::compliance_view::run_asset_statuses(&conn, &pack_id, run_id)
            .map_err(|e| e.to_string())?;
        // 分桶统计（合成后口径；人审覆盖会改变档位 —— 界面要按生效结论分桶）
        let mut counts = serde_json::Map::new();
        for st in &statuses {
            let k = st.effectiveStatus.as_str().to_string();
            let n = counts.get(&k).and_then(|v| v.as_i64()).unwrap_or(0);
            counts.insert(k, json!(n + 1));
        }
        let stale_overrides = statuses.iter().filter(|s| s.overrideStale).count();
        if let Some(w) = want {
            statuses.retain(|s| s.effectiveStatus == w);
        }
        if let Some(a) = asset_filter {
            statuses.retain(|s| s.assetId == a);
        }
        let total = statuses.len() as i64;
        let page: Vec<_> = statuses
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect();
        // 文件名/缩略图（走既有的图库读模型，不另写 SQL）
        let ids: Vec<i64> = page.iter().map(|s| s.assetId).collect();
        let mut views: std::collections::BTreeMap<i64, core_types::AssetView> = Default::default();
        if !ids.is_empty() {
            let f = core_types::ImportFilter {
                ids: Some(ids.clone()),
                limit: Some(1000),
                ..Default::default()
            };
            for v in svc_library::list_assets(&conn, &f).map_err(|e| e.to_string())? {
                views.insert(v.asset.assetId, v);
            }
        }
        let items: Vec<serde_json::Value> = page
            .iter()
            .map(|s| {
                let v = views.get(&s.assetId);
                json!({
                    "assetId": s.assetId,
                    "fileName": v.map(|x| x.asset.fileName.clone()),
                    "absPath": v.map(|x| x.asset.absPath.clone()),
                    "thumbPath": v.and_then(|x| x.thumbPath.clone()),
                    "missing": v.map(|x| x.asset.missing),
                    // 机器结论（该 run 的判定行汇总）与叠加人审后的生效结论
                    "machineStatus": s.machineStatus.as_str(),
                    "effectiveStatus": s.effectiveStatus.as_str(),
                    "machineConfidence": s.machineConfidence,
                    // 资产级人审覆盖（整图一条裁决）；规则级覆盖见单资产下钻
                    "override": match (&s.overrideStatus, s.overrideAt) {
                        (Some(st), at) => json!({
                            "status": st, "reason": s.overrideReason, "at": at,
                        }),
                        _ => serde_json::Value::Null,
                    },
                    // 人审存在但资产内容已变（不再生效）
                    "overrideStale": s.overrideStale,
                })
            })
            .collect();
        Ok(json!({
            "runId": run_id,
            "packId": pack_id,
            "schemeId": svc_library::compliance_view::review_scheme(&pack_id),
            "pack": { "id": pack.id, "rules": pack.rules.len() },
            "status": status,
            "states": counts,
            "total": total,
            "limit": limit,
            "offset": offset,
            "staleOverrides": stale_overrides,
            "items": items,
        }))
    })
    .await;
    match res {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)).into_response(),
        Ok(Err(e)) if e.contains("不存在") => err(StatusCode::NOT_FOUND, "run_not_found", e),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, "job_verdicts", e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
pub struct JobExportReq {
    /// pass | fail | undetermined | unjudged | all（缺省 all）——「过的一批 / 未过的一批」就是 pass / 其余档
    #[serde(default)]
    pub status: Option<String>,
    /// 任选档位：显式给一组资产 id（仍要求在该 run 内）
    #[serde(default, alias = "assetIds")]
    pub asset_ids: Option<Vec<i64>>,
    /// original | thumb256 | thumb1024（缺省 original）
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default, alias = "destDir")]
    pub dest_dir: Option<String>,
    #[serde(default, alias = "zipPath")]
    pub zip_path: Option<String>,
    /// 清单（缺省 true）
    #[serde(default)]
    pub manifest: Option<bool>,
}

/// POST /api/compliance/jobs/{runId}/export —— **按这一次 job 的结果导出子集**。
///
/// 与 F8 图库导出**互不相干**（用户裁决 2026-10-31）：
/// * 选择集 = 该 run 的资产 ∩ 合成生效结论命中 `status` 档（人审覆盖参与，口径与 `job_verdicts` 同源）；
/// * manifest 走行业包的判定报告列（`VerdictReportSpec{run_id}`），图库导出恒 `report: None`（不读任何判定字段）。
pub async fn job_export(
    State(state): State<AppState>,
    AxumPath(run_id): AxumPath<i64>,
    Json(req): Json<JobExportReq>,
) -> Response {
    let data_dir = state.data_dir.clone();
    let thumbs_root = state.thumbs_root.clone();
    let status = req.status.clone().unwrap_or_else(|| "all".to_string());
    let want = if status == "all" {
        None
    } else {
        match svc_library::compliance_view::AssetComplianceState::parse(&status) {
            Some(s) => Some(s),
            None => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "bad_status",
                    format!("status 只支持 pass|fail|undetermined|unjudged|all，收到: {status}"),
                )
            }
        }
    };
    let content = match req.content.as_deref().unwrap_or("original") {
        "original" => svc_library::export::ExportContent::Original,
        "thumb256" => svc_library::export::ExportContent::Thumb(256),
        "thumb1024" => svc_library::export::ExportContent::Thumb(1024),
        other => {
            return err(
                StatusCode::BAD_REQUEST,
                "bad_content",
                format!("content 只支持 original|thumb256|thumb1024，收到: {other}"),
            )
        }
    };
    let target = match (req.dest_dir.as_deref(), req.zip_path.as_deref()) {
        (Some(d), None) if !d.trim().is_empty() => {
            let pb = std::path::PathBuf::from(d);
            if !pb.is_absolute() {
                return err(
                    StatusCode::BAD_REQUEST,
                    "bad_dest",
                    format!("destDir 必须是绝对路径: {d}"),
                );
            }
            svc_library::export::ExportTarget::Dir(pb)
        }
        (None, Some(z)) if !z.trim().is_empty() => {
            let pb = std::path::PathBuf::from(z);
            if !pb.is_absolute() {
                return err(
                    StatusCode::BAD_REQUEST,
                    "bad_dest",
                    format!("zipPath 必须是绝对路径: {z}"),
                );
            }
            svc_library::export::ExportTarget::Zip(pb)
        }
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "dest_required",
                "destDir 与 zipPath 必须二选一且非空",
            )
        }
    };
    let picked = req.asset_ids.clone();
    let manifest = req.manifest.unwrap_or(true);
    let res = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, String> {
        let conn = core_storage::open(&crate::infer::db_path(&data_dir))
            .map_err(|e| e.to_string())?;
        let pack = plugin_meta_pack(&conn, &data_dir);
        let pack_id = verdict_pack_id(&conn, &data_dir);
        let mut statuses = svc_library::compliance_view::run_asset_statuses(&conn, &pack_id, run_id)
            .map_err(|e| e.to_string())?;
        if let Some(p) = &picked {
            statuses.retain(|s| p.contains(&s.assetId));
        }
        if let Some(w) = want {
            statuses.retain(|s| s.effectiveStatus == w);
        }
        let ids: Vec<i64> = statuses.iter().map(|s| s.assetId).collect();
        if ids.is_empty() {
            return Err("该档位没有命中的资产（导出取消）".to_string());
        }
        let spec = svc_library::export::ExportSpec {
            source: svc_library::export::ExportSource::Ids(ids.clone()),
            content,
            target,
            manifest,
            // 判定报告列 = 行业包 report 模板；按 **run** 取判定行（不是单资产 job 行）。
            // 没装行业包（pack 无规则）时退回普通清单 —— 报告列模板本身来自包，无从生成。
            report: if pack.rules.is_empty() {
                None
            } else {
                Some(svc_library::export::VerdictReportSpec {
                    pack: pack.clone(),
                    job_id: None,
                    run_id: Some(run_id),
                })
            },
            strip_gps: false,
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let report = svc_library::export::run_export(
            &conn,
            &spec,
            &thumbs_root,
            &cancel,
            &mut |_p| {},
        )
        .map_err(|e| e.to_string())?;
        Ok(json!({
            "ok": true,
            "runId": run_id,
            "status": status,
            "selected": ids.len(),
            "report": report,
        }))
    })
    .await;
    match res {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)).into_response(),
        Ok(Err(e)) if e.contains("导出取消") => err(
            StatusCode::BAD_REQUEST,
            "export_source_invalid",
            e,
        ),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, "job_export", e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
    }
}

// ---------------------------------------------------------------- 人工改判（COMPLIANCE-HUMAN）

/// POST /api/compliance/review 的 body。
#[derive(Debug, Deserialize)]
pub struct ReviewReq {
    pub assetId: i64,
    /// 缺省（null/""）= **资产级整体结论**（整张图一条裁决）
    pub ruleId: Option<String>,
    /// pass | fail（人的裁决只有两档）
    pub humanStatus: String,
    /// **必填**（trim 后为空 → 400 review_reason_required）
    pub reason: String,
    /// 操作人（可选；本地客户端可传用户名/机器名）
    pub reviewedBy: Option<String>,
    /// 非低置信度结果改判必须显式确认（true），否则 400 review_not_low_confidence
    pub force: Option<bool>,
}

/// GET /api/compliance/review 的 query（复核列表；一个 (asset, rule) 一行）。
#[derive(Debug, Deserialize)]
pub struct ReviewListQuery {
    pub assetId: Option<i64>,
    /// true（缺省）= 只列需要复核的行（undetermined / confidence < threshold）；
    /// false = 列出该资产全部有结论的行（主动复核）
    pub lowConfidence: Option<bool>,
    pub threshold: Option<f64>,
    /// true = 只看已人工改判的；false = 只看还没改判的；缺省不限
    pub reviewed: Option<bool>,
    /// 排序：asset（缺省）| confidence（置信度升序）
    pub sort: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// DELETE /api/compliance/review 的 query（撤销改判，回到机器结论）。
#[derive(Debug, Deserialize)]
pub struct ReviewRevokeQuery {
    pub assetId: i64,
    /// 缺省 = 撤销资产级改判
    pub ruleId: Option<String>,
    pub reason: Option<String>,
}

/// GET /api/compliance/review/history 的 query（含撤销/被覆盖的行）。
#[derive(Debug, Deserialize)]
pub struct ReviewHistoryQuery {
    pub assetId: Option<i64>,
    pub limit: Option<i64>,
}

/// `sort` 解析：非法值 → 400（不静默当「不排序」——排序没生效是最难查的假象）。
fn parse_sort(v: Option<&str>) -> std::result::Result<svc_library::compliance_view::ReviewSort, Response> {
    use svc_library::compliance_view::ReviewSort;
    match v.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("asset") | Some("assetId") => Ok(ReviewSort::Asset),
        Some("confidence") | Some("confidenceAsc") => Ok(ReviewSort::Confidence),
        Some(other) => Err(err(
            StatusCode::BAD_REQUEST,
            "bad_sort",
            format!("sort 只支持 asset|confidence，收到: {other}"),
        )),
    }
}

/// 改判错误 → HTTP：校验拒收 400（带稳定错误码），撤销不到 404，存储错误 500。
fn review_err(e: svc_library::compliance_view::ReviewError) -> Response {
    use svc_library::compliance_view::{review_code, ReviewError};
    match e {
        ReviewError::Reject(r) => {
            let status = if r.code == review_code::NOT_FOUND {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            (status, Json(json!({ "error": r.code, "message": r.message }))).into_response()
        }
        ReviewError::Store(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "review_store", e.to_string()),
    }
}

/// POST /api/compliance/review —— 人工改判（reason 必填；低置信度免 force，高置信度要 force）。
///
/// 不改写机器判定：`rule_verdict` 原样保留（可审计/可复现），改判落 `verdict_human_review`，
/// 生效结论 = `humanStatus ?? result`（资产状态/库筛选/统计/报告全走同一口径）。
pub async fn review_create(
    State(state): State<AppState>,
    Json(req): Json<ReviewReq>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    let pack = plugin_meta_pack(&conn, &state.data_dir);
    let threshold = pack.low_confidence();
    // IP-K10-FIX：改判挂在「判定行所属包 id」上（插件通道 = 插件包 id）
    let pack_id = verdict_pack_id(&conn, &state.data_dir);
    let saved = svc_library::compliance_view::record_human_review(
        &conn,
        &pack_id,
        &pack,
        &svc_library::compliance_view::ReviewRequest {
            asset_id: req.assetId,
            rule_id: req.ruleId.as_deref(),
            human_status: &req.humanStatus,
            reason: &req.reason,
            reviewed_by: req.reviewedBy.as_deref(),
            force: req.force.unwrap_or(false),
            threshold,
        },
    );
    let row = match saved {
        Ok(r) => r,
        Err(e) => return review_err(e),
    };
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "packId": pack_id,
            "assetId": row.asset_id,
            "ruleId": row.rule_id,
            // 机器结论 + 置信度（改判当时的快照；前端展示"机器说 X(0.42) → 人工 Y"）
            "machineResult": row.machine_result,
            "machineConfidence": row.machine_confidence,
            // 行级生效结论（刚写入的改判）；资产级合成结论只在 job 域给
            // （DECouple-BE：资产视图不下发 finalStatus/assetState/machineState）
            "effectiveStatus": row.human_status,
            "schemeId": row.scheme_id,
            "contentVersion": row.content_version,
            "humanReview": json!({
                "id": row.id,
                "ruleId": row.rule_id,
                "scope": row.judgement_scope,
                "humanStatus": row.human_status,
                "reason": row.human_reason,
                "at": row.human_at,
                "reviewedBy": row.reviewed_by,
                "contentVersion": row.content_version,
                "machineResult": row.machine_result,
                "machineConfidence": row.machine_confidence,
            }),
            "threshold": threshold,
        })),
    )
        .into_response()
}

/// GET /api/compliance/review —— 复核列表（扁平行：一个 (asset, rule) 一行 + 资产级改判一行）。
pub async fn review_list(
    State(state): State<AppState>,
    Query(q): Query<ReviewListQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    let pack = plugin_meta_pack(&conn, &state.data_dir);
    let pack_id = verdict_pack_id(&conn, &state.data_dir);
    let threshold = q
        .threshold
        .filter(|t| t.is_finite() && (0.0..=1.0).contains(t))
        .unwrap_or_else(|| pack.low_confidence());
    // 给了 assetId 时缺省列「全部有结论的行」（检查器要看到整张图的所有规则）；
    // 没给 assetId（全库复核视图）时缺省只列需要复核的行，否则一次吐出全库。
    let low_confidence_only = q
        .lowConfidence
        .unwrap_or_else(|| q.assetId.is_none());
    let sort = match parse_sort(q.sort.as_deref()) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let query = svc_library::compliance_view::ReviewQuery {
        asset_id: q.assetId,
        low_confidence_only,
        threshold,
        reviewed: q.reviewed,
        sort,
        limit: q.limit.unwrap_or(200),
        offset: q.offset.unwrap_or(0),
    };
    match svc_library::compliance_view::list_reviews(&conn, &pack_id, &pack, &query, locale_from_headers(&headers)) {
        Ok((total, items)) => (
            StatusCode::OK,
            Json(json!({
                "packId": pack_id, "total": total, "items": items,
                "threshold": threshold, "lowConfidenceOnly": low_confidence_only,
            })),
        )
            .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "review_list", e.to_string()),
    }
}

/// DELETE /api/compliance/review —— 撤销改判（回到机器结论；不删行，只落 revokedAt）。
pub async fn review_revoke(
    State(state): State<AppState>,
    Query(q): Query<ReviewRevokeQuery>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    let pack = plugin_meta_pack(&conn, &state.data_dir);
    let pack_id = verdict_pack_id(&conn, &state.data_dir);
    let revoked = svc_library::compliance_view::revoke_human_review(
        &conn,
        &pack_id,
        q.assetId,
        q.ruleId.as_deref(),
        q.reason.as_deref(),
    );
    let n = match revoked {
        Ok(n) => n,
        Err(e) => return review_err(e),
    };
    (
        StatusCode::OK,
        Json(json!({
            "ok": true, "revoked": n, "assetId": q.assetId, "ruleId": q.ruleId,
            // 撤销后该资产在该方案下没有生效人审；资产级结论由 job 域重新合成
        })),
    )
        .into_response()
}

/// GET /api/compliance/review/history —— 改判历史（含撤销/被覆盖；审计 + 改判前后对照）。
pub async fn review_history(
    State(state): State<AppState>,
    Query(q): Query<ReviewHistoryQuery>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    let pack = plugin_meta_pack(&conn, &state.data_dir);
    let pack_id = verdict_pack_id(&conn, &state.data_dir);
    match svc_library::compliance_view::review_history(
        &conn,
        &pack_id,
        q.assetId,
        Some(q.limit.unwrap_or(200).clamp(1, 1000)),
    ) {
        Ok(rows) => {
            let items: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    json!({
                        "id": r.id, "assetId": r.asset_id, "ruleId": r.rule_id,
                        "scope": r.judgement_scope, "groupKey": r.group_key,
                        "humanStatus": r.human_status, "reason": r.human_reason,
                        "at": r.human_at, "reviewedBy": r.reviewed_by,
                        "machineResult": r.machine_result, "machineConfidence": r.machine_confidence,
                        "revokedAt": r.revoked_at, "revokeReason": r.revoke_reason,
                        "contentVersion": r.content_version,
                        "active": r.is_active(),
                        // 资产内容已变（重导/修图）→ 这条人审在**当前内容上**不再生效
                        // （与 active 正交：被覆盖/撤销的活记录也可能同时是内容过期）
                        "contentStale": r.content_version
                            != core_storage::compliance_sqlite::asset_content_version(
                                &conn, r.asset_id,
                            )
                            .unwrap_or_default(),
                    })
                })
                .collect();
            (
                StatusCode::OK,
                Json(json!({ "packId": pack_id, "total": items.len(), "items": items })),
            )
                .into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "review_history", e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
pub struct VersionsQuery {
    pub assetId: Option<i64>,
}

/// 一行工作版本 → JSON（`chain` 原样透出：前端/复盘要看逐步证据）。
fn version_json(v: &core_storage::asset_versions::StoredVersion) -> serde_json::Value {
    json!({
        "versionId": v.version_id,
        "assetId": v.record.asset_id,
        "jobId": v.record.job_id,
        "runId": v.record.run_id,
        "packId": v.record.pack_id,
        "packVersion": v.record.pack_version,
        "pluginHash": v.record.plugin_hash,
        "actions": v.record.actions,
        "result": v.record.result,
        "errorCode": v.record.error_code,
        "errorMessage": v.record.error_message,
        "path": v.record.path,
        "sha256": v.record.sha256,
        "bytes": v.record.bytes,
        "format": v.record.format,
        "width": v.record.width,
        "height": v.record.height,
        "assetName": v.record.asset_name,
        "chain": v.record.chain,
        "durationMs": v.record.duration_ms,
        "createdAt": v.record.created_at,
        "current": v.current,
    })
}

/// GET /api/compliance/versions?assetId=<id> —— 该资产的**处理结果行 + 工作版本**。
///
/// 一张图一条链 = 一行（`result=ok|failed`）；`current=true` 的那一行是**当前工作版本**，
/// 至多一张（DB 部分唯一索引兜底）。界面/验收据此回答三件事：
/// 「这张图处理过几次」「现在生效的是哪一版（sha256 对得上）」「上一次为什么失败」。
pub async fn versions(
    State(state): State<AppState>,
    Query(q): Query<VersionsQuery>,
) -> Response {
    let Some(asset_id) = q.assetId else {
        return err(
            StatusCode::BAD_REQUEST,
            "asset_id_required",
            "必须给 assetId（一次查一张图的历史版本）",
        );
    };
    let conn = state.db.lock().await;
    let repo = core_storage::asset_versions::SqliteAssetVersionRepo::new(&conn);
    use core_storage::asset_versions::AssetVersionRepo as _;
    let rows = match repo.list_for_asset(asset_id) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "versions", e.to_string()),
    };
    let current = match repo.current_working(asset_id) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "versions", e.to_string()),
    };
    let ok_rows = rows.iter().filter(|v| v.record.result == "ok").count();
    (
        StatusCode::OK,
        Json(json!({
            "assetId": asset_id,
            "current": current.as_ref().map(version_json),
            "versions": rows.iter().map(version_json).collect::<Vec<_>>(),
            "count": rows.len(),
            "okCount": ok_rows,
            // 「只有一张工作版本」的自证字段（验收直接读它，不信代码自觉）
            "currentCount": rows.iter().filter(|v| v.current).count(),
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------- 处理产物（PROC-RES-2）

/// 产物白名单校验：`path` 必须 ①在 `asset_versions.path` 有行（产物账本，杜绝「猜一个
/// 路径就读盘」），②canonicalize 后落在 `{data_dir}/compliance/fixes` 之下。
///
/// 与 `/api/media/*`「路径永远来自 DB」的纪律同源：请求携带的 path 只是结果行
/// `artifacts[].path`（DB 值）的回放，两条校验任何一条不过就拒。
fn artifact_path_checked(
    conn: &Connection,
    fixes_root: &Path,
    raw: &str,
) -> Result<PathBuf, (StatusCode, &'static str, String)> {
    let known: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM asset_versions WHERE path = ?1",
            [raw],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false);
    if !known {
        return Err((
            StatusCode::NOT_FOUND,
            "artifact_unknown",
            format!("产物不在版本账本里：{raw}"),
        ));
    }
    let canon = std::fs::canonicalize(raw).map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            "artifact_missing",
            format!("产物文件不在了：{raw}（{e}）"),
        )
    })?;
    let root = std::fs::canonicalize(fixes_root).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "artifact_root",
            format!("产物根不可读：{}（{e}）", fixes_root.display()),
        )
    })?;
    if !canon.starts_with(&root) {
        return Err((
            StatusCode::FORBIDDEN,
            "artifact_outside",
            format!("产物不在产物目录之下：{raw}"),
        ));
    }
    Ok(canon)
}

fn artifact_content_type(p: &Path) -> &'static str {
    match p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        _ => "application/octet-stream",
    }
}

#[derive(Deserialize)]
pub struct ArtifactQuery {
    path: Option<String>,
    /// `1` = 下载（attachment 头 + 按文件名落盘）；缺省 = 内联（缩略图/打开共用）
    download: Option<u8>,
}

/// GET /api/compliance/artifact?path=…&download=1 —— 处理产物文件（结果行 `artifacts[].path`）。
///
/// 缩略图直接用产物本身：处理输出已是交付尺寸的 webp/jpg，前端 CSS 限宽即可，不生成
/// 第二份缩略图、**不碰资产工作版本**（硬边界）。`download=1` 加 attachment 头走下载。
pub async fn artifact_serve(
    State(state): State<AppState>,
    Query(q): Query<ArtifactQuery>,
) -> Response {
    let Some(raw) = q
        .path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        return err(
            StatusCode::BAD_REQUEST,
            "path_required",
            "必须给 path（结果行 artifacts[].path）",
        );
    };
    let fixes_root = fixes_dir(&state.data_dir);
    let checked = {
        let conn = state.db.lock().await;
        artifact_path_checked(&conn, &fixes_root, &raw)
    };
    let path = match checked {
        Ok(p) => p,
        Err((s, c, m)) => return err(s, c, m),
    };
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) => {
            return err(
                StatusCode::NOT_FOUND,
                "artifact_read",
                format!("读产物失败：{}（{e}）", path.display()),
            )
        }
    };
    let mut resp = (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, artifact_content_type(&path)),
            (header::CACHE_CONTROL, "private, max-age=3600"),
        ],
        bytes,
    )
        .into_response();
    if q.download.unwrap_or(0) == 1 {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("artifact")
            .replace(['"', '\\', '\r', '\n'], "_");
        if let Ok(v) = header::HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
            resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
        }
    }
    resp
}

#[derive(Deserialize)]
struct ArtifactsExportItem {
    path: String,
    /// 目标文件名（缺省 = 产物 basename）；只取文件名分量，路径分隔符当脏数据剥掉
    name: Option<String>,
}

#[derive(Deserialize)]
struct ArtifactsExportReq {
    items: Vec<ArtifactsExportItem>,
    /// 与既有 JSON 体同风格用 camelCase（`destDir`），兼容蛇形
    #[serde(rename = "destDir", alias = "dest_dir")]
    dest_dir: String,
}

/// 复制核心（抽出便于单测）：同名冲突自动加 `-2/-3` 序号（扩展名前插），绝不覆盖既有文件
/// —— 历史版本对不上账的教训（job 隔离目录的同源问题）不在这里重演。
fn copy_artifacts_to(items: Vec<(PathBuf, String)>, dest: &Path) -> std::io::Result<Vec<String>> {
    std::fs::create_dir_all(dest)?;
    let mut written: Vec<String> = Vec::new();
    for (src, raw_name) in items {
        let name = Path::new(&raw_name)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("artifact")
            .trim()
            .to_string();
        let name = if name.is_empty() { "artifact".to_string() } else { name };
        let stem = Path::new(&name).file_stem().and_then(|s| s.to_str()).unwrap_or("artifact");
        let ext = Path::new(&name).extension().and_then(|s| s.to_str());
        let mut target = dest.join(&name);
        let mut n = 2u32;
        while target.exists() {
            let candidate = match ext {
                Some(e) => format!("{stem}-{n}.{e}"),
                None => format!("{stem}-{n}"),
            };
            target = dest.join(candidate);
            n += 1;
        }
        std::fs::copy(&src, &target)?;
        written.push(target.to_string_lossy().to_string());
    }
    Ok(written)
}

/// POST /api/compliance/artifacts/export —— 把选中的处理产物收齐到一个目录。
///
/// 既有 `/api/export/start` 只认库内资产（选择集/筛选），不认 `compliance/fixes` 下的
/// 产物文件 —— 这里是最小补的产物收集入口：逐条走同一套 [`artifact_path_checked`]，
/// `std::fs::copy` 到目标目录（后端机器绝对路径），不碰库、不碰资产工作版本。
pub async fn artifacts_export(
    State(state): State<AppState>,
    Json(req): Json<ArtifactsExportReq>,
) -> Response {
    if req.items.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "items_empty",
            "没有可导出的产物（先勾选有产物的行）",
        );
    }
    let dest = PathBuf::from(req.dest_dir.trim());
    if !dest.is_absolute() {
        return err(
            StatusCode::BAD_REQUEST,
            "dest_not_absolute",
            "destDir 必须是后端机器上的绝对路径",
        );
    }
    let fixes_root = fixes_dir(&state.data_dir);
    // 先在 HTTP 侧校验（DB 锁 + canonicalize 都快），复制放 spawn_blocking
    let mut checked: Vec<(PathBuf, String)> = Vec::new();
    let mut missing: Vec<serde_json::Value> = Vec::new();
    {
        let conn = state.db.lock().await;
        for it in &req.items {
            match artifact_path_checked(&conn, &fixes_root, it.path.trim()) {
                Ok(p) => {
                    let name = it
                        .name
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    let name = name.unwrap_or_else(|| {
                        p.file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("artifact")
                            .to_string()
                    });
                    checked.push((p, name));
                }
                Err((_, c, m)) => missing.push(json!({ "path": it.path, "reason": m, "code": c })),
            }
        }
    }
    if checked.is_empty() {
        return err(
            StatusCode::NOT_FOUND,
            "artifacts_none_valid",
            format!("没有一条产物通过校验（missing {} 条）", missing.len()),
        );
    }
    let dest_for_copy = dest.clone();
    let copied = tokio::task::spawn_blocking(move || copy_artifacts_to(checked, &dest_for_copy))
        .await;
    let files = match copied {
        Ok(Ok(f)) => f,
        Ok(Err(e)) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "artifacts_copy",
                format!("复制产物失败：{}（{e}）", dest.display()),
            )
        }
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
    };
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "total": req.items.len(),
            "copied": files.len(),
            "files": files,
            "missing": missing,
            "dest": dest.to_string_lossy(),
        })),
    )
        .into_response()
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/queue", get(queue))
        .route("/verdicts", get(verdicts))
        // N5b：结果表（result_columns 驱动；judge 才画审核列）
        .route("/results", get(results))
        .route("/feedback", get(feedback_list).post(feedback_create))
        .route("/feedback/export", get(feedback_export))
        // COMPLIANCE-HUMAN：人工改判（POST 改判 / DELETE 撤销 / GET 复核列表 / GET 历史）
        .route(
            "/review",
            get(review_list).post(review_create).delete(review_revoke),
        )
        .route("/review/history", get(review_history))
        // 处理插件（.dampack）：清单 / 安装 / 切指针 / 回滚 / 卸载
        .route("/packs", get(pack_list))
        // KI35-P4 走查缺口：UI 走 archiveBase64 上传装包，imaging 这类带 python 资源的包
        // >2.6MB，axum 默认 body 上限 2MB → 413，界面装不进（API 按路径装不受影响）。
        // 装包是低频大负载动作：单独提到 64MB，不动全局默认。
        .route(
            "/packs/install",
            post(pack_install).layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route("/packs/{id}/activate", post(pack_activate_version))
        .route("/packs/{id}/rollback", post(pack_rollback))
        .route("/packs/{id}", get(pack_preview).delete(pack_uninstall))
        // JUDGE-ENTRY：产品级判定触发（入队 + 后台并行跑 + 进度/取消）
        .route("/judge", post(judge))
        .route("/judge/status", get(judge_status))
        .route("/judge/cancel", post(judge_cancel))
        // CMP-JOB-01：一次批量审核 = 一个独立 job（执行列表 / 单次明细 / 正在跑的那一次）
        .route("/jobs", get(jobs_list))
        .route("/jobs/current", get(jobs_current))
        .route("/jobs/{runId}", get(job_detail))
        // DECouple-BE：L3 下钻（job 账本 + human_review 查询时合成）与按 job 结果导出子集
        .route("/jobs/{runId}/verdicts", get(job_verdicts))
        .route("/jobs/{runId}/export", post(job_export))
        // N6e：动作链结果行 / 工作版本（一张图一条链 = 一行；`current=1` 至多一张）
        .route("/versions", get(versions))
        // PROC-RES-2：处理产物（结果行 artifacts 的文件本体）读取 + 选中收集导出
        .route("/artifact", get(artifact_serve))
        .route("/artifacts/export", post(artifacts_export))
}

// ---------------------------------------------------------------------------
// IP-K05 单元测试：插件通道的装配与应答解析
// ---------------------------------------------------------------------------
//
// 口径（卡内用户裁决 2026-10-20）：只验**功能代码 + 单元测试**；真机/E2E/出包延后。
// 这里三组用例覆盖三件事：
// 1. 真包真网关真子进程跑一次 `judge`（Python 缺失时**跳过并打印原因**，不假装通过）；
// 2. 宿主错误 → 失败归档词的映射（timeout/crash/load/rpc/protocol 全部穷举）；
// 3. 应答严格性（缺 verdicts / 缺 ruleId / 缺 status → 整批协议违例，不部分采信）。
#[cfg(test)]
mod k05_tests {
    use super::*;

    /// N5b：行级 `fields` 必须进证据的保留键（结果表的行业列数据来源）。
    ///
    /// 反向验证：把 `with_row_fields` 换成恒等函数 → 本用例 RED（表格所有行业列变 `—`）。
    #[test]
    fn row_fields_are_parked_under_evidence_reserved_key() {
        let mut fields = serde_json::Map::new();
        fields.insert("dev".into(), json!(0.31));
        fields.insert("bg".into(), json!("white"));
        let merged = with_row_fields(json!({ "threshold": 0.05 }), &fields);
        assert_eq!(merged["threshold"], 0.05, "插件自己的证据不许被覆盖");
        assert_eq!(merged["fields"]["dev"], 0.31);
        assert_eq!(merged["fields"]["bg"], "white");
        // 没有 fields 就不该凭空造出这个键（否则证据形状随包漂移）
        let empty = with_row_fields(json!({ "a": 1 }), &serde_json::Map::new());
        assert!(empty.get("fields").is_none());
    }

    fn repo_pack_dir() -> Option<PathBuf> {
        // 空库 + 空 data_dir：四级解析只会命中第 4 级（仓库内置包）
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        plugin_pack_dir(&conn, &std::env::temp_dir())
    }

    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .arg("-c")
            .arg("import sys")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// 模型根是否可用（能力网关的 detect/generative 端点要它）。
    ///
    /// 判据与引擎一致：`DAM_MODELS_ROOT` 指向的根下能找到 `u2net`（直接位于根或根/models）。
    /// 不猜别的路径 —— 探不到就当没有（宁可跳过断言，也不把「缺模型」写成「通过」）。
    fn models_available() -> bool {
        let Ok(root) = std::env::var("DAM_MODELS_ROOT") else {
            return false;
        };
        let root = PathBuf::from(root);
        root.join("u2net").is_dir() || root.join("models").join("u2net").is_dir()
    }

    /// 造一张纯净白底 + 居中深色方块的 PNG（R1 尺寸规则只吃几何，不依赖模型）。
    fn white_bg_png(path: &Path, w: u32, h: u32) {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            let (x0, x1) = (w / 5, w * 4 / 5);
            let (y0, y1) = (h / 5, h * 4 / 5);
            if x >= x0 && x < x1 && y >= y0 && y < y1 {
                image::Rgb([30, 30, 30])
            } else {
                image::Rgb([255, 255, 255])
            }
        });
        img.save(path).expect("写测试图");
    }

    /// 真包 × 真网关 × 真子进程：judge 必须覆盖包声明的每条规则，状态落在协议闭集。
    ///
    /// 断言的**不是**某条规则的判定值（那取决于模型与阈值，属 E2E 口径，本卡不验），
    /// 而是宿主契约：覆盖完整、状态合法、身份带 packVersion/pluginHash。
    #[test]
    fn real_ecommerce_pack_judges_every_rule() {
        let Some(pack_dir) = repo_pack_dir() else {
            eprintln!("skip: 未找到 packs/plugins/ecommerce（DAM_PACK_DIR / data_dir/packs / 仓库）");
            return;
        };
        if !python3_available() {
            eprintln!("skip: 本机没有 python3（插件是 Python 子进程）");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        // 真 VL 后端不在单测范围内（那是 C5/c5_acceptance 的事）；这里装一个**固定应答**的
        // VL 端口，让「网关 → 插件 → 判定行」全链路可测，并把模型依赖隔离出去。
        // 交付路径的 VL 适配器见 `GatewayVl`（走 `VlSession::ask_raw`，同一份传输实现）。
        struct StubVl;
        impl pack_host::VlJudge for StubVl {
            fn judge(
                &self,
                _path: &Path,
                _prompt: &str,
                _max_tokens: u32,
            ) -> Result<pack_host::VlReply, pack_host::VlError> {
                Ok(pack_host::VlReply::decided(
                    "是",
                    vec![pack_host::VlRule {
                        status: "pass".into(),
                        label: "stub".into(),
                        score: Some(0.9),
                    }],
                ))
            }
        }
        let runtime = match PluginRuntime::open_with(
            &pack_dir,
            &work,
            Arc::new(StubVl),
            &crate::PackRuntime::default(),
            serde_json::json!({}),
        ) {
            Ok(r) => r,
            Err(e) => panic!("插件装配失败：{e}"),
        };
        let ident = svc_pipeline::PackJudge::identity(&runtime);
        assert!(
            ident.rule_ids.len() >= 5,
            "电商包至少 5 条规则，实际 {:?}",
            ident.rule_ids
        );
        assert!(
            ident.plugin_hash.len() == 64,
            "pluginHash 必须是 sha256 全文（{:?}）",
            ident.plugin_hash
        );

        let img = tmp.path().join("a.png");
        white_bg_png(&img, 2000, 2000);
        let req = svc_pipeline::PluginAsset {
            asset_id: 1,
            job_id: 7,
            path: img.to_string_lossy().to_string(),
            mime: Some("image/png".into()),
            width: 2000,
            height: 2000,
            work_dir: work.to_string_lossy().to_string(),
            rule_ids: Vec::new(),
        };
        let out = svc_pipeline::PackJudge::judge_asset(&runtime, &req);
        if let Some((kind, detail)) = &out.failure {
            // 能力网关缺能力 → 插件整包报 -32001（协议 §5）。两种成因都是**环境**、不是宿主契约：
            // a) 本构建没编 local-infer（web 容器形态，镜像不背 ort/ONNX）；
            // b) 编了但**模型根没挂**（`DAM_MODELS_ROOT` 未设 / 目录里没有 u2net·OCR·x4v3）。
            // 此时协议链路（拉起子进程 → 握手 → 逐帧 JSON-RPC → 网关 403 → 错误归档）已经验证过，
            // 只是判不出值来 —— 如实跳过覆盖度断言，不假装通过；两种情况都不算回归。
            let capability_gap = detail.contains("capability_missing") || detail.contains("-32001");
            let missing = if !cfg!(feature = "local-infer") {
                Some("本构建未编 local-infer（web 容器形态）")
            } else if !models_available() {
                Some("模型根没挂（DAM_MODELS_ROOT 未指向含 u2net/ppocrv6/realesr 的目录）")
            } else {
                None
            };
            match missing {
                Some(why) => {
                    assert!(
                        capability_gap,
                        "整包失败原因不是「能力缺失」（{why}）：{kind:?} {detail}"
                    );
                    eprintln!(
                        "skip 覆盖度断言：{why} → 网关 detect/generative 端点按协议回 403、\
                         插件整包 -32001（宿主已归档为 {kind:?}）。协议链路仍然验证过；\
                         完整断言请跑 `DAM_MODELS_ROOT=/ cargo test -p app-web \
                         --features local-infer compliance_api::k05_tests::real_ecommerce`"
                    );
                    return;
                }
                None => panic!(
                    "整包失败不该发生在清晰 PNG 上（本构建有 local-infer + 模型）：{kind:?} {detail}"
                ),
            }
        }
        let got: Vec<&str> = out.verdicts.iter().map(|v| v.rule_id.as_str()).collect();
        for rule in &ident.rule_ids {
            assert!(got.contains(&rule.as_str()), "缺规则 {rule} 的 verdict：{got:?}");
        }
        for v in &out.verdicts {
            assert!(
                ["pass", "fail", "undetermined", "skipped"].contains(&v.status.as_str()),
                "状态越界：{} ({})",
                v.rule_id,
                v.status
            );
        }
        // R1_size 纯几何（不依赖 u2net/OCR/VL）：2000×2000 必须给结论
        let size = out
            .verdicts
            .iter()
            .find(|v| v.rule_id == "R1_size")
            .expect("R1_size");
        assert!(
            size.status == "pass" || size.status == "fail",
            "几何规则不该判不出来：{:?}",
            size
        );
        // 证据（`--nocapture` 可见）：逐规则状态 + 标签，报告里直接引用这行
        let table: Vec<String> = out
            .verdicts
            .iter()
            .map(|v| format!("{}={}{}", v.rule_id, v.status,
                v.score.map(|s| format!("({s:.2})")).unwrap_or_default()))
            .collect();
        eprintln!("plugin judge 结果（{}ms）：{}", out.duration_ms, table.join(" "));
    }

    /// 宿主错误 → 失败归档词（报告/证据按这些词聚合，改词就是改口径）。
    #[test]
    fn host_errors_map_to_failure_reasons() {
        let cases: Vec<(pack_host::PackHostError, &str)> = vec![
            (
                pack_host::PackHostError::Unavailable {
                    kind: pack_host::Unavailable::Timeout,
                    message: "x".into(),
                    stderr: vec![],
                    timeout: None,
                },
                "plugin_timeout",
            ),
            (
                pack_host::PackHostError::Unavailable {
                    kind: pack_host::Unavailable::Crash,
                    message: "x".into(),
                    stderr: vec![],
                    timeout: None,
                },
                "plugin_crash",
            ),
            (
                pack_host::PackHostError::Unavailable {
                    kind: pack_host::Unavailable::LoadFailed,
                    message: "x".into(),
                    stderr: vec![],
                    timeout: None,
                },
                "plugin_unavailable",
            ),
            (
                pack_host::PackHostError::Unavailable {
                    kind: pack_host::Unavailable::HealthTimeout,
                    message: "x".into(),
                    stderr: vec![],
                    timeout: None,
                },
                "plugin_unavailable",
            ),
            (
                pack_host::PackHostError::Rpc {
                    code: -32002,
                    message: "读不出".into(),
                },
                "plugin_rule_error",
            ),
            (
                pack_host::PackHostError::Protocol("无 result".into()),
                "plugin_protocol_error",
            ),
            (pack_host::PackHostError::Io("断管".into()), "plugin_unavailable"),
        ];
        for (e, want) in cases {
            let kind = map_host_error(&e);
            assert_eq!(kind.reason(), want, "错误映射不符：{e}");
            assert!(
                kind != svc_pipeline::PluginFailure::Timeout || want == "plugin_timeout",
                "词表串味"
            );
        }
    }

    /// 应答严格性：缺 verdicts / 缺 ruleId / 缺 status / 非对象 evidence 的处理。
    #[test]
    fn reply_parsing_is_strict() {
        let ident = svc_pipeline::PackIdentityInfo {
            id: PLUGIN_ECOMMERCE_ID.into(),
            version: "1.0.0".into(),
            plugin_hash: "h".repeat(64),
            deterministic: false,
            rule_ids: vec!["R1_size".into()],
            kinds: vec!["judge".into()],
            actions: Vec::new(),
            timeout_secs: 60,
        };
        // 1) 既没 verdicts 也没有 rows → 协议违例（结果行契约就不成立）
        let out = parse_reply(&ident, "judge", "asset-1", json!({}), 5);
        assert_eq!(
            out.failure.as_ref().map(|(k, _)| *k),
            Some(svc_pipeline::PluginFailure::Protocol)
        );
        // 2) verdict 缺 ruleId → 协议违例（不部分采信）
        let out = parse_reply(
            &ident,
            "judge",
            "asset-1",
            json!({ "verdicts": [{ "status": "pass" }] }),
            5,
        );
        assert_eq!(
            out.failure.as_ref().map(|(k, _)| *k),
            Some(svc_pipeline::PluginFailure::Protocol)
        );
        // 3) 缺 status → 协议违例
        let out = parse_reply(
            &ident,
            "judge",
            "asset-1",
            json!({ "verdicts": [{ "ruleId": "R1_size" }] }),
            5,
        );
        assert_eq!(
            out.failure.as_ref().map(|(k, _)| *k),
            Some(svc_pipeline::PluginFailure::Protocol)
        );
        // 4) 正常应答：字段齐全，evidence 非对象时归零成 {}
        let out = parse_reply(
            &ident,
            "judge",
            "asset-1",
            json!({ "verdicts": [{
                "ruleId": "R1_size", "status": "pass", "score": 0.8,
                "label": "size_ok", "evidence": "不是对象", "fixed": false, "note": null
            }] }),
            9,
        );
        assert!(out.failure.is_none());
        assert_eq!(out.verdicts.len(), 1);
        assert_eq!(out.verdicts[0].score, Some(0.8));
        assert_eq!(out.verdicts[0].evidence, json!({}));
        assert_eq!(out.duration_ms, 9);
    }

    /// IP-K16：不存在「执行引擎开关」—— 判定恒由处理插件的子进程执行。
    /// （旧的 `legacy | plugin` 双跑开关、`DAM_COMPLIANCE_ENGINE`、settings `compliance`
    /// 一并删除；下面这行编译期就钉住「枚举里没有 Legacy 这个值」。）
    /// N5a：处理类插件（kinds=["process"]）回 verdict → **拒收并记错**
    /// （`plugin_verdict_not_allowed`），绝不折算成 pass/skipped。
    #[test]
    fn process_pack_returning_verdict_is_rejected_and_recorded() {
        let ident = svc_pipeline::PackIdentityInfo {
            id: "com.soujpg.pack.imaging".into(),
            version: "0.1.0".into(),
            plugin_hash: "h".repeat(64),
            deterministic: true,
            rule_ids: vec![],
            kinds: vec!["process".into()],
            actions: vec!["resize".into()],
            timeout_secs: 120,
        };
        // 旧判定应答形态（verdicts[]）
        let out = parse_reply(
            &ident,
            "judge",
            "asset-7",
            json!({ "verdicts": [{ "ruleId": "R1", "status": "pass", "score": 1.0,
                                   "label": "", "evidence": {}, "fixed": false, "note": null }] }),
            3,
        );
        let (kind, detail) = out.failure.clone().expect("必须拒收");
        assert_eq!(kind, svc_pipeline::PluginFailure::VerdictNotAllowed);
        assert_eq!(kind.reason(), "plugin_verdict_not_allowed");
        assert!(detail.contains("处理类"), "{detail}");
        assert!(out.verdicts.is_empty(), "拒收后不得留下任何结论");

        // 新契约行形态（rows[].verdict）
        let out = parse_reply(
            &ident,
            "judge",
            "asset-7",
            json!({ "rows": [{ "assetId": "asset-7", "result": "ok",
                               "verdict": { "status": "pass", "confidence": 1.0 } }] }),
            3,
        );
        assert_eq!(
            out.failure.as_ref().map(|(k, _)| *k),
            Some(svc_pipeline::PluginFailure::VerdictNotAllowed)
        );

        // 处理类的正常应答：只有 result + 产物，不生成 verdicts
        let out = parse_reply(
            &ident,
            "judge",
            "asset-7",
            json!({ "rows": [{ "assetId": "asset-7", "result": "ok",
                               "artifacts": [{ "kind": "working_version",
                                               "assetId": "asset-7", "versionId": "v1" }] }],
                    "artifacts": [{ "rule_id": "resize", "path": "/tmp/a.jpg" }] }),
            3,
        );
        assert!(out.failure.is_none(), "{:?}", out.failure);
        assert!(out.verdicts.is_empty(), "处理类没有审核结论");
        assert_eq!(out.artifacts.len(), 1);
    }

    /// N5a：判定类按规则出行时（新契约只有 rows[]）也能还原规则结论，
    /// 行级失败 → `undetermined`（判不了 ≠ 通过）。
    #[test]
    fn judge_rows_without_legacy_verdicts_are_restored_per_rule() {
        let ident = svc_pipeline::PackIdentityInfo {
            id: PLUGIN_ECOMMERCE_ID.into(),
            version: "1.0.0".into(),
            plugin_hash: "h".repeat(64),
            deterministic: false,
            rule_ids: vec!["A4".into(), "A5".into()],
            kinds: vec!["judge".into()],
            actions: Vec::new(),
            timeout_secs: 60,
        };
        let out = parse_reply(
            &ident,
            "judge",
            "asset-9",
            json!({ "rows": [
                { "assetId": "asset-9", "ruleId": "A4", "result": "ok",
                  "verdict": { "status": "fail", "reasonCode": "R3_no_watermark", "confidence": 0.4 } },
                { "assetId": "asset-9", "ruleId": "A5", "result": "failed",
                  "error": { "code": "vl_unavailable", "message": "VL 超时" } }
            ]}),
            4,
        );
        assert!(out.failure.is_none(), "{:?}", out.failure);
        assert_eq!(out.verdicts.len(), 2);
        assert_eq!(out.verdicts[0].status, "fail");
        assert_eq!(out.verdicts[0].note.as_deref(), Some("R3_no_watermark"));
        assert_eq!(out.verdicts[1].status, "undetermined", "行级失败不得冒充 pass");
        assert!(out.verdicts[1].note.as_deref().unwrap().contains("vl_unavailable"));
    }

    #[test]
    fn plugin_is_the_only_execution_channel() {
        assert_eq!(PLUGIN_ECOMMERCE_ID, "com.soujpg.pack.ecommerce");
        let json = serde_json::json!({ "engine": "plugin" });
        assert_eq!(json["engine"], "plugin");
    }

    /// pluginState 只读 manifest：包在 → available，包不在 → available=false + 原因。
    #[test]
    fn plugin_state_reports_missing_pack() {
        let empty = tempfile::tempdir().unwrap();
        // 清掉 env 干扰：模拟「没装包」的机器
        std::env::remove_var("DAM_PACK_DIR");
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        let st = plugin_state(&conn, empty.path());
        if repo_pack_dir().is_some() {
            // 仓库里带着包 → 三级解析会命中第 3 级（开发态正常情况）
            assert_eq!(st["available"], true);
            assert!(st["pluginHash"].as_str().unwrap().len() == 64);
        } else {
            assert_eq!(st["available"], false);
            assert_eq!(st["reason"], "plugin_pack_missing");
        }
    }
}

// ---------------------------------------------------------------------------
// K07-I：参数 schema / 合并 / 下发（纯函数级单测，不起子进程、不碰语料）
// ---------------------------------------------------------------------------
#[cfg(test)]
mod k07i_tests {
    use super::*;

    /// 下发到插件的 `options` 必须是**全量生效值**，且 `workdir` 由宿主覆盖注入。
    #[test]
    fn build_options_sends_full_effective_values_and_host_workdir() {
        let effective = json!({"shortSideMin": 2000, "attribute": true, "workdir": "/tampered"});
        let opts = build_options(&effective, "/data/fixes");
        assert_eq!(opts["shortSideMin"], 2000, "{opts}");
        assert_eq!(opts["attribute"], true, "{opts}");
        assert_eq!(
            opts["workdir"], "/data/fixes",
            "workdir 是宿主保留键：参数值不许覆盖它（{opts}）"
        );
        // 空生效值 → 只有 workdir（协议 v1.1 的老行为不变）
        assert_eq!(build_options(&json!({}), "/w"), json!({"workdir": "/w"}));
        // 非对象（脏数据）也不许 panic / 丢 workdir
        assert_eq!(build_options(&json!(null), "/w"), json!({"workdir": "/w"}));
    }

    /// 判定走插件是**唯一**通道（N18）：这里只需要一个库 + 仓库内置包在。
    fn plugin_engine_conn() -> Option<rusqlite::Connection> {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        // 仓库内置包在才继续（发行形态可能没带）
        repo_pack_dir()?;
        Some(conn)
    }

    fn repo_pack_dir() -> Option<PathBuf> {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        plugin_pack_dir(&conn, &std::env::temp_dir())
    }

    // -- KI-31：插件通道按**规格的 pluginId** 解析包目录（不许硬编码电商包） ----------

    /// 仓库内置插件按 manifest id 命中（不再只认 `plugins/ecommerce` 这一条写死路径）。
    #[test]
    fn repo_builtin_pack_dir_matches_any_plugin_by_manifest_id() {
        let Some(dir) = repo_builtin_pack_dir(PLUGIN_ECOMMERCE_ID) else {
            eprintln!("skip: 本机没有仓库内置电商包");
            return;
        };
        assert_eq!(dir.file_name().unwrap(), "ecommerce", "{dir:?}");
        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["id"], PLUGIN_ECOMMERCE_ID);
        // 房产包（id `com.soujpg.pack.realestate`，目录 `plugins/realestate`）同样能按 id 命中
        let re = repo_builtin_pack_dir(PLUGIN_REALESTATE_ID).expect("房产插件必须可解析");
        assert_eq!(re.file_name().unwrap(), "realestate", "{re:?}");
        // 不存在的 id → None（不猜、不回落电商包）
        assert!(repo_builtin_pack_dir("com.example.nope").is_none());
    }

    fn spec_with_plugin(conn: &rusqlite::Connection, plugin_id: &str, rules: Vec<String>) -> i64 {
        use core_storage::specs::{NewSpec, SpecRepo as _, SqliteSpecRepo};
        SqliteSpecRepo::new(conn)
            .create(&NewSpec {
                plugin_id: plugin_id.to_string(),
                plugin_version_pin: None,
                name: format!("测试规格 {plugin_id} #{}", {
                    static SEQ: std::sync::atomic::AtomicUsize =
                        std::sync::atomic::AtomicUsize::new(0);
                    SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                }),
                industry: None,
                rules,
                thresholds: json!({}),
                actions: json!([]),
                output: json!({}),
                params_diff: json!({}),
                enabled: true,
                origin_key: None,
            })
            .unwrap()
    }

    /// 规格所属插件没装 → `spec_plugin_mismatch`（参数不静默失效），且错误文案点名插件 id。
    #[test]
    fn judge_spec_of_uninstalled_plugin_is_rejected_with_plugin_id() {
        let Some(conn) = plugin_engine_conn() else {
            eprintln!("skip: 本机没有仓库内置包");
            return;
        };
        let spec_id = spec_with_plugin(&conn, "com.example.missing", Vec::new());
        let err = match load_judge_spec(&conn, &std::env::temp_dir(), spec_id) {
            Ok(_) => panic!("规格所属插件不在 → 必须拒绝"),
            Err(e) => e,
        };
        assert_eq!(err.code, "spec_plugin_not_installed");
        assert!(err.message.contains("com.example.missing"), "{err:?}");
        assert_eq!(err.action, Some("open_packs_drawer"), "必须带界面去处：{err:?}");
    }

    /// KI-31 的反向证明：**房产规格在插件引擎下可装载**（改前必然 `spec_plugin_mismatch`），
    /// 且规则校验用的是**房产包自己的** rules.json（不是电商包的）。
    #[test]
    fn realestate_spec_loads_under_plugin_engine() {
        let Some(conn) = plugin_engine_conn() else {
            eprintln!("skip: 本机没有仓库内置包");
            return;
        };
        let dir = repo_builtin_pack_dir(PLUGIN_REALESTATE_ID)
            .expect("仓库内置房产包必须能按 manifest id 解析（packs/plugins/realestate）");
        assert_eq!(dir.file_name().unwrap(), "realestate", "{dir:?}");
        let declared = plugin_declared_rules(&dir);
        assert!(!declared.is_empty(), "房产包必须声明规则：{dir:?}");
        let spec_id = spec_with_plugin(&conn, PLUGIN_REALESTATE_ID, declared.clone());
        let ok = load_judge_spec(&conn, &std::env::temp_dir(), spec_id)
            .expect("房产规格在插件引擎下必须可装载（KI-31）");
        assert_eq!(ok.record.plugin_id, PLUGIN_REALESTATE_ID);
        // 拿电商包的规则去挂房产规格 → 必须被房产包的规则清单拒掉
        let bad_id = spec_with_plugin(&conn, PLUGIN_REALESTATE_ID, vec!["R0_product_main".into()]);
        let err = match load_judge_spec(&conn, &std::env::temp_dir(), bad_id) {
            Ok(_) => panic!("跨包规则必须被拒"),
            Err(e) => e,
        };
        assert_eq!(err.code, "spec_unknown_rule", "{err:?}");
    }

    /// 官方包声明了 schema → 合并出**全量生效值**（默认值 + 规格 + 任务覆盖，后写赢）。
    #[test]
    fn resolve_merges_defaults_spec_and_task_override() {
        let Some(conn) = plugin_engine_conn() else {
            eprintln!("skip: 本机没有仓库内置包");
            return;
        };
        let spec = json!({"shortSideMin": 2000});
        let task = json!({"attribute": true});
        let got = resolve_judge_params(&conn, &std::env::temp_dir(), PLUGIN_ECOMMERCE_ID, Some(&spec), Some(&task), None)
            .unwrap()
            .expect("plugin 引擎 + 包在 → 必须有参数快照");
        assert!(got.declared, "官方包声明了 params");
        assert_eq!(got.effective["shortSideMin"], json!(2000), "规格层覆盖默认值");
        assert_eq!(got.effective["attribute"], json!(true), "任务覆盖");
        assert_eq!(
            got.effective["whiteMaxDev"],
            json!(8),
            "没改的键要带插件默认值（插件不依赖宿主默认值逻辑）"
        );
        assert_eq!(got.effective["allowInpaint"], json!(true));
        // 快照能直接落 run（人读 + 机读）
        let snap = got.snapshot();
        assert_eq!(snap["declared"], json!(true));
        assert_eq!(snap["effective"]["shortSideMin"], json!(2000));
    }

    /// N1：规格的**规则级阈值**由宿主注入 `options.thresholds`（业务映射留在插件内）。
    ///
    /// 口径：阈值不进 schema 层（schema 只声明平铺键），但在合并校验**之后**注入；
    /// 平铺键优先（插件侧 `_merge_flat_thresholds` 的优先级就是这个），所以两者并存不打架。
    #[test]
    fn resolve_injects_spec_thresholds_after_schema_check() {
        let Some(conn) = plugin_engine_conn() else {
            eprintln!("skip: 本机没有仓库内置包");
            return;
        };
        let spec = json!({"shortSideMin": 700});
        let thresholds = json!({"R1_size": {"min": 700}, "R2_white_bg": {"max_dev": 4}});
        let got = resolve_judge_params(
            &conn,
            &std::env::temp_dir(),
            PLUGIN_ECOMMERCE_ID,
            Some(&spec),
            None,
            Some(&thresholds),
        )
        .unwrap()
        .expect("plugin 引擎 + 包在 → 必须有参数快照");
        assert_eq!(
            got.effective["thresholds"], thresholds,
            "规格里的规则级阈值必须原样递到插件（宿主不做业务映射）"
        );
        assert_eq!(got.effective["shortSideMin"], json!(700), "平铺键仍然生效");
        // 证据落盘用：这次「规格 → 全量生效参数」的原文（报告/复核直接引用）
        println!(
            "[N1] effective options (spec layer + thresholds) = {}",
            serde_json::to_string_pretty(&got.effective_value()).unwrap()
        );
        // 空阈值不注入（老包/无阈值的规格不该多出个空键）
        let got2 = resolve_judge_params(
            &conn,
            &std::env::temp_dir(),
            PLUGIN_ECOMMERCE_ID,
            Some(&spec),
            None,
            Some(&json!({})),
        )
        .unwrap()
        .unwrap();
        assert!(got2.effective.get("thresholds").is_none());
    }

    /// 不合法 → 稳定错误码（HTTP 层原样透出；不许静默改成默认值）。
    #[test]
    fn resolve_reports_stable_codes_for_bad_values() {
        let Some(conn) = plugin_engine_conn() else {
            eprintln!("skip: 本机没有仓库内置包");
            return;
        };
        let dir = std::env::temp_dir();
        for (layer, code) in [
            (json!({"shortSideMin": "1600"}), "params_type"),
            (json!({"shortSideMin": 10}), "params_range"),
            (json!({"shortSidMin": 1600}), "params_unknown_key"),
            (json!({"allowInpaint": "yes"}), "params_type"),
        ] {
            let err = resolve_judge_params(&conn, &dir, PLUGIN_ECOMMERCE_ID, None, Some(&layer), None).unwrap_err();
            assert_eq!(err.code, code, "{layer} → {err:?}");
        }
        // 层不是对象
        let err = resolve_judge_params(&conn, &dir, PLUGIN_ECOMMERCE_ID, None, Some(&json!(["x"])), None).unwrap_err();
        assert_eq!(err.code, "params_layer");
    }

    // -- 真包冒烟：自定义参数真的到了插件、且真的改变了判定 ---------------------

    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .args(["-c", "import sys"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// 白底 + 居中深色方块 PNG（R1 只吃宽高，不依赖模型）。
    fn white_png(path: &Path, w: u32, h: u32) {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            let (x0, x1) = (w / 5, w * 4 / 5);
            let (y0, y1) = (h / 5, h * 4 / 5);
            if x >= x0 && x < x1 && y >= y0 && y < y1 {
                image::Rgb([30, 30, 30])
            } else {
                image::Rgb([255, 255, 255])
            }
        });
        img.save(path).expect("写测试图");
    }

    struct StubVl;
    impl pack_host::VlJudge for StubVl {
        fn judge(
            &self,
            _path: &Path,
            _prompt: &str,
            _max_tokens: u32,
        ) -> Result<pack_host::VlReply, pack_host::VlError> {
            Ok(pack_host::VlReply::undetermined("smoke_stub"))
        }
    }

    /// **真包 × 真子进程冒烟**（K07-I 交付口径）：
    /// 1. 同一张 1800×1800 图，插件默认阈值（short_side_min=1600）→ R1 pass；
    /// 2. 自定义参数 `{shortSideMin: 2000}`（宿主按 schema 合并后下发）→ R1 fail，且证据里的
    ///    `threshold` 变成 2000 —— 证明**全量生效参数真的到了插件并被解释**，不是只在宿主里算了一遍；
    /// 3. 宿主放行但插件不认的跨键组合（subjectRatioMin > max）→ 插件回 -32003（插件侧二次校验）。
    ///
    /// 只判 `R1_size`（纯几何）：不依赖 u2net/OCR/VL 模型，容器里也能真跑。
    #[test]
    fn real_pack_receives_full_effective_params_and_honours_them() {
        let Some(pack_dir) = repo_pack_dir() else {
            eprintln!("skip: 未找到 packs/plugins/ecommerce");
            return;
        };
        if !python3_available() {
            eprintln!("skip: 本机没有 python3（插件是 Python 子进程）");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("a.png");
        white_png(&img, 1800, 1800);
        let asset = |work: &Path| svc_pipeline::PluginAsset {
            asset_id: 1,
            job_id: 7,
            path: img.to_string_lossy().to_string(),
            mime: Some("image/png".into()),
            width: 1800,
            height: 1800,
            work_dir: work.to_string_lossy().to_string(),
            rule_ids: vec!["R1_size".to_string()],
        };
        let open = |work: &Path, options: serde_json::Value| {
            PluginRuntime::open_with(
                &pack_dir,
                work,
                Arc::new(StubVl),
                &crate::PackRuntime::default(),
                options,
            )
            .expect("插件装配失败")
        };
        let r1 = |out: &svc_pipeline::PluginOutcome| {
            out.verdicts
                .iter()
                .find(|v| v.rule_id == "R1_size")
                .cloned()
                .expect("R1_size 必须有结论")
        };

        // 1) 插件默认阈值
        let work1 = tmp.path().join("w1");
        let rt_default = open(&work1, json!({ "whiteMaxDev": 8, "allowInpaint": true }));
        eprintln!(
            "K07I_EVIDENCE defaults options_sent={}",
            build_options(&json!({ "whiteMaxDev": 8, "allowInpaint": true }), "/w1")
        );
        let out1 = svc_pipeline::PackJudge::judge_asset(&rt_default, &asset(&work1));
        if let Some((kind, detail)) = &out1.failure {
            eprintln!("skip: 插件链路在本环境不可用（{kind:?}: {detail}）");
            return;
        }
        let v1 = r1(&out1);
        assert_eq!(v1.status, "pass", "1800px ≥ 默认 1600 → 应 pass：{v1:?}");
        assert_eq!(
            v1.evidence["threshold"], json!(1600),
            "证据里的阈值 = 包默认值（说明插件用的是自己的默认，不是宿主替它算）：{v1:?}"
        );

        // 2) 自定义参数（宿主已按 schema 合并过的工作版本）→ 同一张图必须改判
        let work2 = tmp.path().join("w2");
        let rt_custom = open(
            &work2,
            json!({ "shortSideMin": 2000, "whiteMaxDev": 8, "allowInpaint": true }),
        );
        let out2 = svc_pipeline::PackJudge::judge_asset(&rt_custom, &asset(&work2));
        assert!(out2.failure.is_none(), "自定义参数是合法值，不该失败：{:?}", out2.failure);
        let v2 = r1(&out2);
        assert_eq!(v2.status, "fail", "1800px < 自定义 2000 → 必须 fail：{v2:?}");
        eprintln!(
            "K07I_EVIDENCE custom options_sent={} verdict={}",
            build_options(
                &json!({ "shortSideMin": 2000, "whiteMaxDev": 8, "allowInpaint": true }),
                "/w2"
            ),
            serde_json::json!({
                "ruleId": v2.rule_id,
                "status": v2.status,
                "score": v2.score,
                "label": v2.label,
                "evidence": v2.evidence,
                "note": v2.note,
            })
        );
        assert_eq!(
            v2.evidence["threshold"], json!(2000),
            "证据里的阈值必须是下发的生效值（参数没到插件就是这个断言炸）：{v2:?}"
        );

        // 3) 插件侧二次校验：宿主只校单键，跨键约束由包自己拦
        let work3 = tmp.path().join("w3");
        let rt_bad = open(
            &work3,
            json!({ "subjectRatioMin": 0.9, "subjectRatioMax": 0.5 }),
        );
        let out3 = svc_pipeline::PackJudge::judge_asset(&rt_bad, &asset(&work3));
        let (kind, detail) = out3.failure.expect("跨键矛盾必须被插件拒绝");
        assert_eq!(
            kind,
            svc_pipeline::PluginFailure::RuleError,
            "应为规则级参数错（-32003），实际 {kind:?}: {detail}"
        );
        assert!(detail.contains("-32003"), "错误里要点名 -32003：{detail}");
        assert!(
            detail.contains("subjectRatioMin"),
            "错误里要点名参数：{detail}"
        );
    }

    /// legacy 引擎不消费 options：解析直接返回 None（/judge 因此不会因为新字段报错）。
    #[test]
    fn resolve_returns_none_when_plugin_dir_is_absent() {
        // N18：没有「legacy 通道」这回事了。唯一会返回 None 的情形是**插件包目录不在**
        // （装不了插件 = 参数无处校验），由 prepare 阶段报「插件未安装」那条准确错误。
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        let got = resolve_judge_params(
            &conn,
            &std::env::temp_dir(),
            "com.example.not-installed",
            None,
            Some(&json!({"whatever": 1})),
            None,
        )
        .unwrap();
        assert!(got.is_none(), "包目录不在 → None（不在这里抢错误口径）");
    }
}

// ---------------------------------------------------------------------------
// PROC-RES-1：specId 处理路径三缺陷（actions 不入链 / packId 记错 / 结果行不落）
// ----------------------------------------------------------------------------
#[cfg(test)]
mod proc_res1_tests {
    use super::*;

    fn spec_record(actions: serde_json::Value, params_diff: serde_json::Value) -> JudgeSpec {
        JudgeSpec {
            record: core_storage::specs::SpecRecord {
                spec_id: 1,
                plugin_id: "com.soujpg.pack.imaging".into(),
                plugin_version_pin: None,
                name: "长边 800".into(),
                industry: None,
                rules: Vec::new(),
                thresholds: json!({}),
                actions,
                output: json!({}),
                params_diff,
                enabled: true,
                origin_key: None,
                created_at: 0,
                updated_at: 0,
            },
        }
    }

    /// 规格动作链解析：`[{action,params}]` → 有序动作名；判定类规格（空/无）→ 空。
    #[test]
    fn spec_action_names_parse_in_order() {
        let s = spec_record(
            json!([
                {"action": "resize", "params": {"resizeLongEdge": 800}},
                {"action": "convert", "params": {"format": "webp"}}
            ]),
            json!({}),
        );
        assert_eq!(s.action_names().unwrap(), vec!["resize", "convert"]);
        // 判定类规格：空数组 / null → 空链（不走链语义）
        assert_eq!(
            spec_record(json!([]), json!({})).action_names().unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            spec_record(json!(null), json!({})).action_names().unwrap(),
            Vec::<String>::new()
        );
    }

    /// 脏形态不许静默吞：缺 `action` 的项当场报错（否则「规格选了动作却没跑」再现）。
    #[test]
    fn spec_action_names_reject_malformed() {
        let bad = spec_record(json!([{"params": {"resizeLongEdge": 800}}]), json!({}));
        let e = bad.action_names().unwrap_err();
        assert_eq!(e.code, "spec_actions_invalid");
        assert!(e.message.contains("actions[0]"), "{}", e.message);

        let not_array = spec_record(json!({"action": "resize"}), json!({}));
        assert_eq!(
            not_array.action_names().unwrap_err().code,
            "spec_actions_invalid"
        );
    }

    /// 处理类规格的参数层：`paramsDiff ⊕ 各动作 params 平铺`（同名键链尾胜 —— 对齐
    /// 既有链机制「全链共享一份 options」的现状，不开第二套参数通道）。
    #[test]
    fn chain_params_layer_flattens_action_params() {
        let s = spec_record(
            json!([
                {"action": "resize", "params": {"resizeMode": "long_edge", "resizeLongEdge": 800}},
                {"action": "convert", "params": {"format": "webp", "resizeLongEdge": 900}}
            ]),
            json!({"keepMeta": true}),
        );
        let layer = s.chain_params_layer();
        assert_eq!(layer["keepMeta"], json!(true), "params_diff 为底：{layer}");
        assert_eq!(layer["resizeLongEdge"], json!(900), "同名键后一动作胜：{layer}");
        assert_eq!(layer["format"], json!("webp"), "{layer}");
        // 动作名本身不是参数（它是链；每步由 action_asset 覆盖 options.action）
        assert!(layer.get("action").is_none(), "{layer}");
        // 判定类规格不受影响
        let judge_spec = spec_record(json!([]), json!({"shortSideMin": 2000}));
        assert_eq!(judge_spec.params_layer()["shortSideMin"], json!(2000));
    }

    /// run.params.packId 提取（结果视图按 run 解析包的依据）；坏数据 → None 回落。
    #[test]
    fn run_pack_id_reads_params_json() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        let repo = SqliteComplianceRunRepo::new(&conn);
        let run = repo
            .start(
                svc_pipeline::COMPLIANCE_KIND,
                Some(r#"{"packId":"com.soujpg.pack.imaging","candidates":1}"#),
            )
            .unwrap();
        assert_eq!(
            run_pack_id(&conn, run).as_deref(),
            Some("com.soujpg.pack.imaging")
        );
        // params 坏 / 不存在 → None（调用方回落当前配置包口径）
        assert_eq!(run_pack_id(&conn, run + 999), None);
    }

    /// 处理类包的结果行视图：数据源 `asset_versions`，行带 `artifacts`（export_file 的
    /// path/sha256），`verdict` 恒 null，failed 行 reason 指向错误信息。
    #[tokio::test]
    async fn process_results_view_serves_asset_versions_with_artifacts() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO assets (assetId, fileName, absPath, fileExt, fileSz, mtime, importedAt) \
             VALUES (7, 'a.jpg', '/imgs/a.jpg', '.jpg', 100, 1, 1)",
            [],
        )
        .unwrap();
        let repo = SqliteComplianceRunRepo::new(&conn);
        let run = repo
            .start(
                svc_pipeline::COMPLIANCE_KIND,
                Some(r#"{"packId":"com.soujpg.pack.imaging"}"#),
            )
            .unwrap();
        // 写侧落的是 `ChainOutcome::evidence()` **整块对象**（task 的现象来源）
        let chain_obj = serde_json::json!({
            "chain": [
                {"action": "resize", "result": "ok", "outPath": "/fixes/a.resize.jpg",
                 "sha256": "ab12", "bytes": 1024, "format": "jpg", "width": 800, "height": 600, "ms": 5},
                {"action": "resize", "result": "skipped", "ms": 1}
            ],
            "durationMs": 6,
            "cancelled": false
        })
        .to_string();
        let ok_id = conn
            .execute(
                "INSERT INTO asset_versions (assetId, runId, packId, packVersion, pluginHash, \
                 actions, result, path, sha256, bytes, format, width, height, chain, durationMs, \
                 createdAt, current) \
                 VALUES (7, ?1, 'com.soujpg.pack.imaging', '1.0.0', 'h', '[\"resize\"]', 'ok', \
                 '/fixes/a.resize.jpg', 'ab12', 1024, 'jpg', 800, 600, ?2, 5, 100, 1)",
                rusqlite::params![run, chain_obj],
            )
            .unwrap();
        assert_eq!(ok_id, 1);
        conn.execute(
            "INSERT INTO asset_versions (assetId, runId, packId, packVersion, pluginHash, \
             actions, result, errorCode, errorMessage, chain, durationMs, createdAt, current) \
             VALUES (7, ?1, 'com.soujpg.pack.imaging', '1.0.0', 'h', '[\"convert\"]', 'failed', \
             'action_failed', '转格式失败', '[]', 3, 200, 0)",
            rusqlite::params![run],
        )
        .unwrap();

        let q = ResultsQuery {
            pack_id: None,
            job_id: None,
            run_id: Some(run),
            limit: None,
            offset: None,
        };
        let resp = process_results_view(
            &conn,
            "com.soujpg.pack.imaging",
            &q,
            "process",
            pack_host::result_columns::ParsedColumns::default(),
        );
        let (parts, body) = resp.into_parts();
        assert_eq!(parts.status, axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let view: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(view["total"], 2, "run 内两行（ok + failed）：{view}");
        let items = view["items"].as_array().unwrap();
        // 排序按 assetId, versionId：ok 在前
        assert_eq!(items[0]["result"], "ok");
        assert_eq!(items[0]["verdict"], serde_json::Value::Null);
        assert_eq!(items[0]["ruleId"], "resize", "ruleId 槽位 = 链拼接：{}", items[0]);
        let arts = items[0]["artifacts"].as_array().unwrap();
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0]["kind"], "export_file");
        assert_eq!(arts[0]["path"], "/fixes/a.resize.jpg");
        assert_eq!(arts[0]["sha256"], "ab12");
        assert_eq!(
            items[0]["fields"]["width"], 800,
            "链尾产物尺寸进行业列：{}",
            items[0]
        );
        // 库里的 chain 是对象（写侧 evidence 整块）→ 接口 fields.chain 必须是数组，逐步证据可读
        let chain = items[0]["fields"]["chain"].as_array().unwrap_or_else(|| {
            panic!("fields.chain 必须是数组（库里存对象）：{}", items[0])
        });
        assert_eq!(chain.len(), 2, "两步链逐步证据：{}", items[0]);
        assert_eq!(chain[0]["action"], "resize");
        assert_eq!(chain[0]["result"], "ok");
        assert_eq!(
            chain[0]["sha256"], "ab12",
            "step1 产物 sha 可在接口读到：{}",
            chain[0]
        );
        assert_eq!(chain[1]["result"], "skipped", "step2 跳过：{}", chain[1]);
        // failed 行：没有产物、reason 指向错误
        assert_eq!(items[1]["result"], "failed");
        assert_eq!(items[1]["reason"], "转格式失败");
        assert_eq!(
            items[1]["artifacts"].as_array().unwrap().len(),
            0,
            "failed 行无产物：{}",
            items[1]
        );
    }

    /// `fields.chain` 口径：库里是对象（`ChainOutcome::evidence()`）→ 取 `.chain`；
    /// 裸数组原样透传；坏 JSON / 没 `chain` 键 → 空数组（不 panic、不吞行）。
    #[test]
    fn chain_view_accepts_object_and_bare_array() {
        assert_eq!(
            chain_view(r#"{"chain":[{"action":"resize","result":"ok"}],"durationMs":6,"cancelled":false}"#)
                [0]["action"],
            "resize"
        );
        assert_eq!(
            chain_view(r#"[{"action":"convert"}]"#)[0]["action"],
            "convert"
        );
        assert_eq!(chain_view(r#"{"durationMs":6}"#), json!([]));
        assert_eq!(chain_view(r#"{"chain":"oops"}"#), json!([]));
        assert_eq!(chain_view("not json"), json!([]));
        assert_eq!(chain_view("null"), json!([]));
    }
}

/// PROC-RES-2：处理产物读取/收集入口的三条底线 —— 账本校验、白名单根、复制不覆盖。
mod proc_res2_tests {
    use super::*;

    fn ledger_with_version(path: &str) -> Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO assets (assetId, fileName, absPath, fileExt, fileSz, mtime, importedAt) \
             VALUES (7, 'a.jpg', '/imgs/a.jpg', '.jpg', 100, 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO asset_versions (assetId, runId, packId, packVersion, pluginHash, \
             actions, result, path, sha256, bytes, format, width, height, chain, durationMs, \
             createdAt, current) \
             VALUES (7, 1, 'com.soujpg.pack.imaging', '1.0.0', 'h', '[\"resize\"]', 'ok', \
             ?1, 'ab12', 1024, 'jpg', 800, 600, '[]', 5, 100, 1)",
            [path],
        )
        .unwrap();
        conn
    }

    /// 账本里有 + fixes 根下 → 放行；账本外 / 根外 / 文件没了 → 各自稳定错误码。
    #[test]
    fn artifact_path_checked_gates_on_ledger_and_root() {
        let tmp = tempfile::tempdir().unwrap();
        let fixes = tmp.path().join("compliance").join("fixes");
        std::fs::create_dir_all(&fixes).unwrap();
        let art = fixes.join("job-1").join("a.resize.jpg");
        std::fs::create_dir_all(art.parent().unwrap()).unwrap();
        std::fs::write(&art, b"jpg-bytes").unwrap();

        // ① 账本 + 白名单根 → Ok（canonicalize 后的路径）
        let conn = ledger_with_version(art.to_str().unwrap());
        let got = artifact_path_checked(&conn, &fixes, art.to_str().unwrap());
        assert!(got.is_ok(), "账本内且根下应放行：{got:?}");

        // ② 账本里有、但文件在根外（/etc/hosts 型）→ FORBIDDEN artifact_outside
        let outside = tmp.path().join("outside.jpg");
        std::fs::write(&outside, b"x").unwrap();
        let conn2 = ledger_with_version(outside.to_str().unwrap());
        let err = artifact_path_checked(&conn2, &fixes, outside.to_str().unwrap()).unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
        assert_eq!(err.1, "artifact_outside");

        // ③ 账本里没有（猜路径）→ NOT_FOUND artifact_unknown
        let guess = fixes.join("job-1").join("never-ledgered.jpg");
        let err = artifact_path_checked(&conn, &fixes, guess.to_str().unwrap()).unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
        assert_eq!(err.1, "artifact_unknown");

        // ④ 账本里有、文件被清理了 → NOT_FOUND artifact_missing
        let gone = fixes.join("job-1").join("gone.jpg");
        std::fs::write(&gone, b"x").unwrap();
        let conn3 = ledger_with_version(gone.to_str().unwrap());
        std::fs::remove_file(&gone).unwrap();
        let err = artifact_path_checked(&conn3, &fixes, gone.to_str().unwrap()).unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
        assert_eq!(err.1, "artifact_missing");
    }

    /// 复制核心：同名冲突加序号不覆盖；名字里的路径分量剥掉（`../evil.txt` → `evil.txt`）。
    #[test]
    fn copy_artifacts_to_suffixes_collisions_and_strips_path_parts() {
        let tmp = tempfile::tempdir().unwrap();
        let src_dir = tmp.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let s1 = src_dir.join("a.webp");
        let s2 = src_dir.join("b.webp"); // 不同的源文件，导出名都叫 a.webp
        std::fs::write(&s1, b"one").unwrap();
        std::fs::write(&s2, b"two").unwrap();

        let dest = tmp.path().join("out");
        let written = copy_artifacts_to(
            vec![
                (s1.clone(), "a.webp".into()),
                (s2.clone(), "a.webp".into()),
                (s1, "../evil.txt".into()),
            ],
            &dest,
        )
        .unwrap();

        assert_eq!(written.len(), 3);
        assert!(dest.join("a.webp").is_file());
        assert_eq!(std::fs::read(dest.join("a.webp")).unwrap(), b"one", "先到的不被覆盖");
        assert!(dest.join("a-2.webp").is_file());
        assert_eq!(std::fs::read(dest.join("a-2.webp")).unwrap(), b"two");
        // 路径分量被剥掉：落在 dest 里而不是 dest 的上级
        assert!(dest.join("evil.txt").is_file());
        assert!(!tmp.path().join("evil.txt").exists());
    }
}

/// KI-39（task）：一次 run 记在哪个插件名下。
///
/// 真机现象（`reports/evidence/t_edec0b/win/win-wbf-params.json` 同型）：处理类规格
/// （`actions=[]`）跑出来的 run 是 `packId=com.soujpg.pack.ecommerce`、`kinds=["judge"]`、
/// 结果 `rows=[]` —— 静默把处理规格当判定跑。根因是非链模式用「当前行业元数据插件」
/// 当 run 身份，而不是**实际开跑的插件**。
#[cfg(test)]
mod ki39_run_identity_tests {
    use super::run_pack_identity;

    #[test]
    fn chain_mode_records_chain_pack() {
        assert_eq!(
            run_pack_identity(&Some("com.soujpg.pack.segment".into()), "com.soujpg.pack.imaging"),
            "com.soujpg.pack.segment",
            "链模式：run 记链插件（产物挂在它名下）"
        );
    }

    #[test]
    fn non_chain_mode_records_the_pack_that_actually_runs() {
        // 非链模式：规格属于哪个插件，run 就记哪个插件 —— 不再回落到「当前行业元数据插件」
        assert_eq!(
            run_pack_identity(&None, "com.soujpg.pack.segment"),
            "com.soujpg.pack.segment"
        );
        // 无规格的老调用方：实际开跑的就是内置电商包，行为不变
        assert_eq!(
            run_pack_identity(&None, crate::compliance_api::PLUGIN_ECOMMERCE_ID),
            crate::compliance_api::PLUGIN_ECOMMERCE_ID
        );
    }

    // --------------------------------------------------------------- KI-42
    /// 装包建 venv 也不许弹控制台窗口（Windows 首次准备插件环境时黑窗一闪）。
    ///
    /// 只守接线：`std::process::Command` 不暴露已设 flags，进程级可见性由真机脚本
    /// `scripts/win/ki42_nowindow_probe.ps1` 断言；这条防的是重构把调用删掉而没人发现。
    #[test]
    fn venv_creation_suppresses_console_window() {
        let src = include_str!("compliance_api.rs");
        assert!(
            src.contains("pack_host::process::no_console_window(&mut venv_cmd)"),
            "ensure_pack_venv 的 python -m venv 必须设 CREATE_NO_WINDOW（KI-42）"
        );
    }
}
