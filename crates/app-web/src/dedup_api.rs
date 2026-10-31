#![allow(non_snake_case)] // 本模块 DTO 字段 = 前端 JSON camelCase 契约，勿改
//! 去重 API 面（W5 起步，KI-2 / KI-7 收口）。
//!
//! 路由：
//! - `GET  /api/dedup/groups?range=all|dir:<路径>|ids:<a,b,c>|batch:<id>`
//!   —— 全库/文件夹/选择集/导入批次四种范围的分组（两套判据并查集合并，kind 分层）；
//! - `POST /api/dedup/resolve` 组裁决（保留 1 项，其余软删进回收站）
//! - `POST /api/dedup/ignore` / `POST /api/dedup/unignore` —— 组级忽略名单（刷新后不复现）
//! - `POST /api/dedup/scan` —— **库外文件夹只读查重**（不入库、不写任何表）
//! - `GET  /api/dedup/export?format=csv|md&range=…&scope=library|scan:<id>` —— 报告导出
//! - `GET  /api/dedup/trash` / `POST /api/dedup/restore` —— 回收站（软删可恢复）
//! - `POST /api/dedup/backfill` —— 存量一次性补判（KI-2b，可重入分批）
//!
//! 分层口径（不许混）：
//! 1. **复用** svc-library 的 `dup_groups` / `dedup_resolve` / `list_deleted` /
//!    `restore_assets`（与桌面 D7 同一套回收站语义：软删可恢复，不物理删源文件）；
//!    本层只做「范围裁剪 + 忽略过滤 + 报告渲染 + 库外扫描编排」这类**壳层**工作。
//! 2. 阈值与塔全部写死（KI-2 裁决）：`engine_dedup::PHASH_THRESHOLD`（pHash 汉明 ≤8）、
//!    `engine_search::NEAR_DUP_THRESHOLD`(0.98) / `VARIANT_THRESHOLD`(0.90)、
//!    判重塔模型 `core_config::DEDUP_EMBEDDER`。**不接受调用方传模型名或阈值** ——
//!    签名层面就没有这些参数（用户不可选塔、不可调阈值）。
//! 3. 找不到向量时不得把「0 组」说成「无重复」：响应带 `judge`（vectors/edges/…），
//!    前端据此渲染 未检测 / 检测中 / 已确认 三态徽标。
//! 4. 库外扫描（`scan`）**只读**：只算 SHA-256 + pHash(D4)，产出报告；
//!    不写 `assets` / `resource` / `dedup_vectors` / `dup_edges`（单测里钉前后计数相等）。
//!
//! 未完成（诚实标注）：`scan` 目前是**请求线程内同步**执行（有 `maxFiles` 上限，
//! 超限即 `truncated=true` 并提示缩小范围），没有 jobId/进度/取消那一套作业化；
//! 大目录批量扫描的作业化拆成子卡（见 reports/<task>-KI-2.md「拆分说明」）。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error_codes::{codes, err_coded};
use crate::{is_desktop_form, AppState};
use svc_library::{DupGroupView, DupMemberView, LibraryError};

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

fn svc_err(e: LibraryError) -> Response {
    match e {
        LibraryError::Invalid(_) => err(StatusCode::BAD_REQUEST, e.to_string()),
        _ => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// 范围（range）：all / dir:<路径> / ids:<a,b,c> / batch:<导入批次 id>
// ---------------------------------------------------------------------------

/// 分组范围。默认 `All`（老调用方不带 `range` 参数 = 全库，行为与升级前一致）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Range {
    All,
    /// 库内某个文件夹（绝对路径前缀，按路径段比，`/a/b` 不含 `/a/bc`）
    Dir(String),
    /// 当前选择集（assetId 列表）—— 只在选择集内部判重
    Ids(Vec<i64>),
    /// 某次导入批次：**按 `assets.batchId` 等值过滤**（DM-A4 / 迁移 0033）——
    /// 「这一次导入事件真正写进库的那些图」，不再拿 `import_batches.rootPath` 当目录前缀猜。
    ///
    /// 为什么必须换：同一目录分两次导入 → 前缀完全相同，第一批的图会被算进第二批；
    /// 父子目录两个批次 → 前缀互相包含；批次根目录改名/搬迁 → 历史批次前缀直接对不上。
    ///
    /// 老库（0033 之前的行 `batchId` 全 NULL）：不编造归属，`resolve_scope` 回退成
    /// rootPath 前缀近似，并在 `rangeMeta.approx=true` + `rangeMeta.prefix` 里明示，
    /// 前端/报告不许把这个结果显示成「精确本批」。
    Batch(i64),
}

impl Range {
    /// 回显给前端/报告的规范串（URL 单一真源用同一个格式）。
    pub fn label(&self) -> String {
        match self {
            Range::All => "all".to_string(),
            Range::Dir(p) => format!("dir:{p}"),
            Range::Ids(ids) => format!(
                "ids:{}",
                ids.iter()
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Range::Batch(id) => format!("batch:{id}"),
        }
    }

    fn kind_str(&self) -> &'static str {
        match self {
            Range::All => "all",
            Range::Dir(_) => "dir",
            Range::Ids(_) => "ids",
            Range::Batch(_) => "batch",
        }
    }
}

/// 解析 `range` 查询参数。空 / 缺省 / `all` → 全库；未知前缀明确 400 报错，
/// 不静默退化成全库（否则用户以为在看选择集，实际看到的是全库）。
pub fn parse_range(raw: Option<&str>) -> Result<Range, String> {
    let s = raw.unwrap_or("").trim();
    if s.is_empty() || s.eq_ignore_ascii_case("all") {
        return Ok(Range::All);
    }
    if let Some(rest) = s.strip_prefix("dir:") {
        let p = rest.trim();
        if p.is_empty() {
            return Err("range=dir: 后面必须给文件夹路径".into());
        }
        return Ok(Range::Dir(p.to_string()));
    }
    if let Some(rest) = s.strip_prefix("ids:") {
        let mut ids: Vec<i64> = Vec::new();
        for tok in rest.split(',') {
            let t = tok.trim();
            if t.is_empty() {
                continue;
            }
            let id: i64 = t
                .parse()
                .map_err(|_| format!("range=ids: 里的 {t} 不是整数 assetId"))?;
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        if ids.is_empty() {
            return Err("range=ids: 后面必须给至少一个 assetId".into());
        }
        return Ok(Range::Ids(ids));
    }
    if let Some(rest) = s.strip_prefix("batch:") {
        let id: i64 = rest
            .trim()
            .parse()
            .map_err(|_| format!("range=batch: 后面必须是导入批次 id（整数），收到 {rest}"))?;
        return Ok(Range::Batch(id));
    }
    Err(format!(
        "未知 range：{s}（允许 all / dir:<路径> / ids:<a,b> / batch:<批次id>）"
    ))
}

/// 路径规范化（比较口径）：分隔符统一成 `/`、去掉尾部 `/`，ASCII 小写
/// （Windows 盘符/目录大小写不敏感；POSIX 上小写比较只会放宽「同一目录的不同写法」）。
fn norm_dir(p: &str) -> String {
    let t = p.replace('\\', "/");
    let t = t.trim_end_matches('/');
    if t.is_empty() {
        "/".to_string()
    } else {
        t.to_ascii_lowercase()
    }
}

/// `abs` 是否在 `prefix` 目录内（按路径段，边界安全：`/a/b` 不含 `/a/bc`）。
pub fn path_under(abs: &str, prefix: &str) -> bool {
    let a = norm_dir(abs);
    let p = norm_dir(prefix);
    p == "/" || a == p || a.starts_with(&format!("{p}/"))
}

/// 范围裁剪后的附加事实（前端/报告要说清「这个范围里为什么少了几张」）。
#[derive(Debug, Default, Clone)]
struct RangeMeta {
    /// ids 范围：选择集里的图在**库内**仍有重复，但重复对象在选择集外
    /// —— 不能因为本范围 0 组就说「这 5 张没有重复」。
    outside_dup_assets: usize,
    /// 被裁掉的成员（在库里重复，但不在本范围内）
    out_of_scope_assets: usize,
    /// 实际生效的目录前缀（dir 用；batch 走近似回退时才填）
    prefix: Option<String>,
    /// batch 范围：该批名下（未被物理删除的）资产数。`Some(0)` = 这批没有带批次的资产。
    batch_assets: Option<usize>,
    /// **本范围是否近似**：true = 这批名下没有任何 `batchId` 资产（迁移 0033 之前入的老库），
    /// 只能退回 rootPath 前缀圈范围。前端与报告必须明示「按目录近似」，不许当精确本批。
    approx: bool,
}

/// 范围解析后的**实际判据**（groups / export 共用同一个，避免两处口径漂移）。
#[derive(Debug)]
enum Scope {
    All,
    Dir(String),
    /// 选择集：只在选中的 assetId 内部判重
    Ids(Vec<i64>),
    /// 精确：`assets.batchId = <id>` 的那些资产（`live_ids` 已排除回收站行）
    Batch {
        live_ids: BTreeSet<i64>,
        total: usize,
    },
    /// 近似回退（老库）：这批名下 `batchId` 全 NULL，只能按该批次 rootPath 前缀圈
    BatchApprox {
        prefix: String,
    },
}

/// 批次名下的资产：`(存活 id 集合, 存活数)`。`deleted=1`（回收站）不算「在库里」。
fn batch_assets(conn: &Connection, batch_id: i64) -> rusqlite::Result<(BTreeSet<i64>, usize)> {
    let mut stmt = conn.prepare("SELECT assetId FROM assets WHERE batchId = ?1 AND deleted = 0")?;
    let rows = stmt.query_map([batch_id], |r| r.get::<_, i64>(0))?;
    let mut out = BTreeSet::new();
    for r in rows {
        out.insert(r?);
    }
    let n = out.len();
    Ok((out, n))
}

/// `Range` → `Scope`。批次不存在直接报错（不静默当全库、也不静默当 0 组）。
///
/// 老库回退口径（DM-A4 卡面第 4 条）：这批名下**一张带 `batchId` 的图都没有** →
/// 说明是 0033 之前入库的历史批次，回退成 rootPath 前缀近似 + `approx=true`。
/// 反过来只要有一张带批次，就按精确集合走（混合库里老行不进本批范围，
/// 这是诚实的：它们本来就不属于这次导入事件）。
fn resolve_scope(conn: &Connection, range: &Range) -> Result<Scope, String> {
    match range {
        Range::All => Ok(Scope::All),
        Range::Dir(p) => Ok(Scope::Dir(p.clone())),
        Range::Ids(ids) => Ok(Scope::Ids(ids.clone())),
        Range::Batch(id) => {
            let root: Option<String> = conn
                .query_row(
                    "SELECT rootPath FROM import_batches WHERE batchId = ?1",
                    [*id],
                    |r| r.get::<_, String>(0),
                )
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })
                .map_err(|e| format!("读导入批次 {id} 失败: {e}"))?;
            let Some(root) = root else {
                return Err(format!(
                    "导入批次 {id} 不存在（batch 范围需要先导入过一次）"
                ));
            };
            let (live_ids, total) =
                batch_assets(conn, *id).map_err(|e| format!("读批次 {id} 的资产失败: {e}"))?;
            if total == 0 {
                Ok(Scope::BatchApprox { prefix: root })
            } else {
                Ok(Scope::Batch { live_ids, total })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 组裁剪（范围 + 忽略）—— 全部在本层做，svc-library 的分组口径保持不变
// ---------------------------------------------------------------------------

/// 预选保留项启发式（与 svc-library `suggest_keep` 同口径；范围裁剪后重挑时用）。
fn pick_keep(kind: &str, members: &[DupMemberView]) -> i64 {
    let key = |m: &DupMemberView| match kind {
        "exact" => (m.imported_at, m.asset_id, 0i64, 0i64),
        _ => (
            -(m.width.unwrap_or(0) * m.height.unwrap_or(0)),
            -m.file_sz,
            m.imported_at,
            m.asset_id,
        ),
    };
    members
        .iter()
        .min_by_key(|m| key(m))
        .map(|m| m.asset_id)
        .unwrap_or_else(|| members.first().map(|m| m.asset_id).unwrap_or(0))
}

/// 成员集合变化后重算组（size / reclaimBytes / suggestedKeep / groupId / 呈现序）。
/// 返回 `None` = 裁剪后不足 2 张，不再算重复组。
fn refine_group(mut g: DupGroupView) -> Option<DupGroupView> {
    if g.members.len() < 2 {
        return None;
    }
    let keep = if g.members.iter().any(|m| m.asset_id == g.suggested_keep) {
        g.suggested_keep
    } else {
        pick_keep(&g.kind, &g.members)
    };
    g.size = g.members.len();
    g.reclaim_bytes = g
        .members
        .iter()
        .filter(|m| m.asset_id != keep)
        .map(|m| m.file_sz)
        .sum();
    g.suggested_keep = keep;
    g.members.sort_by_key(|m| (m.asset_id != keep, m.asset_id));
    g.group_id = g.members.iter().map(|m| m.asset_id).min().unwrap_or(keep);
    Some(g)
}

/// 组签名（忽略名单的主键）：成员 assetId 升序拼接。
///
/// 为什么按**成员集合**而不是 groupId：组是边的连通分量，成员会随新图入库/裁决变化；
/// groupId 只是「当前最小 assetId」，换个成员就变。签名口径的语义是
/// 「这一组（就是这些图）已被人工判为无需处理」；成员一变（又发现一张重复）
/// 签名对不上 → 重新出现，让用户再看一眼（宁可多问一次，不许静默吞掉新重复）。
fn group_signature(g: &DupGroupView) -> String {
    let mut ids: Vec<i64> = g.members.iter().map(|m| m.asset_id).collect();
    ids.sort_unstable();
    ids.iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// 忽略名单表。派生数据（可随时清空重算），不进版本化迁移：
/// 与 `dup_judge_state` 一样是「去重过程的账本」，没有跨版本结构演进诉求；
/// `CREATE TABLE IF NOT EXISTS` 幂等，老库首次调用时自动补上。
fn ensure_ignore_table(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS dup_ignores (
            signature  TEXT    PRIMARY KEY,
            groupId    INTEGER NOT NULL,
            assetIds   TEXT    NOT NULL,
            kind       TEXT    NOT NULL,
            ignoredAt  INTEGER NOT NULL
        );",
    )
}

fn ignored_signatures(conn: &Connection) -> rusqlite::Result<BTreeSet<String>> {
    ensure_ignore_table(conn)?;
    let mut stmt = conn.prepare("SELECT signature FROM dup_ignores")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = BTreeSet::new();
    for r in rows {
        out.insert(r?);
    }
    Ok(out)
}

/// 按范围裁剪全库分组（纯函数：忽略过滤已在调用前完成）。
fn apply_range(groups: Vec<DupGroupView>, scope: &Scope) -> (Vec<DupGroupView>, RangeMeta) {
    let mut meta = RangeMeta::default();
    let out = match scope {
        Scope::All => groups,
        Scope::Dir(prefix) => {
            meta.prefix = Some(prefix.clone());
            cut_members(groups, |m| path_under(&m.abs_path, prefix), &mut meta)
        }
        Scope::Batch { live_ids, total } => {
            meta.batch_assets = Some(*total);
            // 回收站里的本批资产：不算「在库里」，但要能解释「这批怎么只剩这么几张」
            meta.out_of_scope_assets += total.saturating_sub(live_ids.len());
            cut_members(groups, |m| live_ids.contains(&m.asset_id), &mut meta)
        }
        Scope::BatchApprox { prefix, .. } => {
            meta.prefix = Some(prefix.clone());
            meta.approx = true;
            meta.batch_assets = Some(0);
            cut_members(groups, |m| path_under(&m.abs_path, prefix), &mut meta)
        }
        Scope::Ids(ids) => cut_ids(groups, ids, &mut meta),
    };
    (out, meta)
}

/// `range=ids:` 的裁剪：除了「范围外裁剪数」，还要单独记账 `outsideDupAssets`
/// （选择集里的图在库内确实还有重复，但重复对象全在选择集外 —— 不许说成「没有重复」）。
fn cut_ids(groups: Vec<DupGroupView>, ids: &[i64], meta: &mut RangeMeta) -> Vec<DupGroupView> {
    let sel: BTreeSet<i64> = ids.iter().copied().collect();
    let mut outside: BTreeSet<i64> = BTreeSet::new();
    let out = groups
        .into_iter()
        .filter_map(|mut g| {
            let hit: Vec<i64> = g
                .members
                .iter()
                .filter(|m| sel.contains(&m.asset_id))
                .map(|m| m.asset_id)
                .collect();
            let before = g.members.len();
            g.members.retain(|m| sel.contains(&m.asset_id));
            meta.out_of_scope_assets += before - g.members.len();
            if hit.len() < 2 {
                outside.extend(hit);
                return None;
            }
            refine_group(g)
        })
        .collect();
    meta.outside_dup_assets = outside.len();
    out
}

/// 按成员谓词裁剪（dir / batch 共用）：裁完不足 2 张的组不再算重复。
fn cut_members(
    groups: Vec<DupGroupView>,
    keep: impl Fn(&DupMemberView) -> bool,
    meta: &mut RangeMeta,
) -> Vec<DupGroupView> {
    groups
        .into_iter()
        .filter_map(|mut g| {
            let before = g.members.len();
            g.members.retain(|m| keep(m));
            meta.out_of_scope_assets += before - g.members.len();
            refine_group(g)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// GET /api/dedup/groups
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct GroupsQuery {
    /// all（默认）/ dir:<路径> / ids:<a,b> / batch:<批次id>
    pub range: Option<String>,
}

/// GET /api/dedup/groups —— 去重分组（deleted=0；大组在前）。
///
/// KI-2b：分组由**两套判据并查集合并**而来（fileHash/pHash ∪ 判重塔向量边），
/// 组 `kind` 分开标注（exact / near / near_visual / variant，混合组取最保守层并给
/// `mixed=true`）。响应另带 `judge`（判重进度）与 `kindCounts`：`vectors == 0` 时
/// 前端必须呈现「未检测」，**不能**把空组列表说成「无重复」。
///
/// KI-2/KI-7：`range` 四种取值在此层裁剪（svc-library 只算全库，口径不动）：
/// - `dir:<路径>`：只看该文件夹内部（库内「此文件夹查重」）；
/// - `ids:<a,b>`：只看这批图互相之间（图库选区「查重」）；范围外仍有的重复通过
///   `rangeMeta.outsideDupAssets` 明示，不许说成「没有重复」；
/// - `batch:<id>`：该批次**精确**入库的那些图（`assets.batchId = <id>`，迁移 0033）；
///   老库该批 `batchId` 全 NULL 时退回 rootPath 近似并在 `rangeMeta.approx=true` 明示。
///
/// 忽略名单在**范围裁剪之前**按全组签名过滤：语义 = 「这一组以后不用再提示我」，
/// 与当前看的范围无关（在「全库」忽略的组，切到「选择集」也不该冒出来）。
pub async fn groups(State(state): State<AppState>, Query(q): Query<GroupsQuery>) -> Response {
    let range = match parse_range(q.range.as_deref()) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    let conn = state.db.lock().await;
    let scope = match resolve_scope(&conn, &range) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    let mut all = match svc_library::dup_groups(&conn) {
        Ok(g) => g,
        Err(e) => return svc_err(e),
    };
    let ignores = ignored_signatures(&conn).unwrap_or_default();
    let ignored_count = ignores.len();
    all.retain(|g| !ignores.contains(&group_signature(g)));

    let (out, meta) = apply_range(all, &scope);
    let total_assets: usize = out.iter().map(|g| g.size).sum();
    let reclaim: i64 = out.iter().map(|g| g.reclaim_bytes).sum();
    let judge =
        svc_pipeline::embed_queue::dup_judge_status(&conn, core_config::DEDUP_EMBEDDER).ok();
    let mut kind_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for g in &out {
        *kind_counts.entry(g.kind.as_str()).or_insert(0) += 1;
    }
    Json(json!({
        "range": range.label(),
        "rangeMeta": {
            "kind": range.kind_str(),
            "prefix": meta.prefix,
            "outOfScopeAssets": meta.out_of_scope_assets,
            "outsideDupAssets": meta.outside_dup_assets,
            // DM-A4：batch 范围的真实性标记。approx=true 时 prefix 是回退用的目录前缀，
            // 前端/报告必须写「按目录近似」，不许当「本批精确」。
            "approx": meta.approx,
            "batchAssets": meta.batch_assets,
        },
        "totalGroups": out.len(),
        "totalAssets": total_assets,
        "reclaimBytes": reclaim,
        "kindCounts": kind_counts,
        "ignoredCount": ignored_count,
        "judge": judge,
        "groups": out,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// 组级忽略 / 撤销忽略
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct IgnoreReq {
    /// 要忽略的组（groupId = 该组最小 assetId，与 `GET /api/dedup/groups` 一致）
    #[serde(default)]
    pub groupIds: Vec<i64>,
    /// 或直接给成员集合（服务端按当前分组反查；两个入口同一张表）
    #[serde(default)]
    pub assetIds: Vec<i64>,
}

/// POST /api/dedup/ignore —— 组级忽略：刷新后不再返回该组（成员集合变了会重新出现）。
pub async fn ignore(State(state): State<AppState>, Json(req): Json<IgnoreReq>) -> Response {
    if req.groupIds.is_empty() && req.assetIds.is_empty() {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::DEDUP_TARGET_REQUIRED,
            json!({}),
            "groupIds / assetIds 至少给一个",
        );
    }
    let conn = state.db.lock().await;
    if let Err(e) = ensure_ignore_table(&conn) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    let all = match svc_library::dup_groups(&conn) {
        Ok(g) => g,
        Err(e) => return svc_err(e),
    };
    let want: BTreeSet<i64> = req.groupIds.iter().copied().collect();
    let mut picked: Vec<&DupGroupView> =
        all.iter().filter(|g| want.contains(&g.group_id)).collect();
    if picked.is_empty() && !req.assetIds.is_empty() {
        let sel: BTreeSet<i64> = req.assetIds.iter().copied().collect();
        picked = all
            .iter()
            .filter(|g| g.members.iter().any(|m| sel.contains(&m.asset_id)))
            .collect();
    }
    if picked.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "没有匹配到任何重复组（可能已被裁决或组合已变化，刷新后再试）",
        );
    }
    let now = svc_import::now_secs();
    let mut saved: Vec<String> = Vec::new();
    for g in picked {
        let sig = group_signature(g);
        let asset_ids = g
            .members
            .iter()
            .map(|m| m.asset_id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        if let Err(e) = conn.execute(
            "INSERT OR REPLACE INTO dup_ignores (signature, groupId, assetIds, kind, ignoredAt)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![sig, g.group_id, asset_ids, g.kind, now],
        ) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
        saved.push(sig);
    }
    Json(json!({ "ok": true, "ignored": saved })).into_response()
}

#[derive(Deserialize)]
pub struct UnignoreReq {
    #[serde(default)]
    pub signatures: Vec<String>,
    /// 不留签名时的兜底：清空整个忽略名单
    #[serde(default)]
    pub all: bool,
}

/// POST /api/dedup/unignore —— 撤销忽略（toast 撤销 / 忽略名单清理）。
pub async fn unignore(State(state): State<AppState>, Json(req): Json<UnignoreReq>) -> Response {
    let conn = state.db.lock().await;
    if let Err(e) = ensure_ignore_table(&conn) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    let removed = if req.all {
        conn.execute("DELETE FROM dup_ignores", []).unwrap_or(0)
    } else if req.signatures.is_empty() {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::DEDUP_SIGNATURES_EMPTY,
            json!({}),
            "signatures 为空（要清空请传 all=true）",
        );
    } else {
        let mut n = 0usize;
        for sig in &req.signatures {
            n += conn
                .execute("DELETE FROM dup_ignores WHERE signature = ?1", [sig])
                .unwrap_or(0);
        }
        n
    };
    Json(json!({ "ok": true, "removed": removed })).into_response()
}

// ---------------------------------------------------------------------------
// 库外文件夹扫描（只读，不入库）
// ---------------------------------------------------------------------------

/// 一次扫描默认最多读多少个文件（请求线程内同步执行，靠它兜住时延）。
pub const SCAN_MAX_FILES_DEFAULT: usize = 500;
/// 硬上限：超过直接按上限截断（响应 truncated=true），防止一次请求把进程拖死。
pub const SCAN_MAX_FILES_HARD: usize = 5000;
/// 扫描结果内存缓存条数（导出要能拿到最近几次的结果；服务重启即失效，可接受）。
const SCAN_CACHE_ENTRIES: usize = 8;

#[derive(Deserialize)]
pub struct ScanReq {
    /// 库外文件夹绝对路径
    pub path: String,
    #[serde(default)]
    pub maxFiles: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanMemberOut {
    pub path: String,
    pub file_name: String,
    pub file_sz: i64,
    pub file_hash: String,
    #[serde(rename = "pHash")]
    pub p_hash: String,
    /// 预选保留项（文件最大者；同等大小取路径序小者）
    pub keep: bool,
    /// 该成员与组内保留项的关系（D4 8×8 最小汉明匹配：「原图方向（汉明距离 2）」）
    pub relation: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanGroupOut {
    /// 组序号（1 起；库外扫描没有 assetId，用序号标识）
    pub group_id: i64,
    pub kind: String,
    pub size: usize,
    pub reclaim_bytes: i64,
    pub evidence: Vec<String>,
    pub members: Vec<ScanMemberOut>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanUnreadable {
    pub path: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanReport {
    pub scan_id: String,
    pub path: String,
    pub scanned: usize,
    /// 目录里文件数超过 maxFiles → 只扫了前 maxFiles 个
    pub truncated: bool,
    pub pending_files: usize,
    pub unreadable: Vec<ScanUnreadable>,
    pub total_groups: usize,
    pub reclaim_bytes: i64,
    pub kind_counts: BTreeMap<String, usize>,
    pub groups: Vec<ScanGroupOut>,
    pub elapsed_ms: i64,
    pub generated_at: i64,
    /// 算法身份（只进报告元数据，不进 UI 文案 —— KI-6）
    pub algorithm: String,
    pub thresholds: String,
    /// 未实现/限制的明写，别让调用方以为有作业化
    pub note: String,
}

fn scan_cache() -> &'static Mutex<HashMap<String, ScanReport>> {
    static CACHE: OnceLock<Mutex<HashMap<String, ScanReport>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn new_scan_id() -> String {
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).expect("OS entropy unavailable");
    format!(
        "scan-{}",
        buf.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

fn cache_scan(report: &ScanReport) {
    if let Ok(mut m) = scan_cache().lock() {
        m.insert(report.scan_id.clone(), report.clone());
        while m.len() > SCAN_CACHE_ENTRIES {
            if let Some(oldest) = m
                .iter()
                .min_by_key(|(_, v)| v.generated_at)
                .map(|(k, _)| k.clone())
            {
                m.remove(&oldest);
            } else {
                break;
            }
        }
    }
}

fn cached_scan(scan_id: &str) -> Option<ScanReport> {
    scan_cache().lock().ok()?.get(scan_id).cloned()
}

/// 库外扫描是否可用（与 `GET /api/capabilities.features.scanExternalPath` 同源判定）。
fn scan_external_allowed(state: &AppState, conn: &Connection) -> bool {
    let is_desktop = is_desktop_form();
    let roots = svc_library::capabilities::probe_roots(&svc_library::capabilities::library_roots(
        conn,
        &state.data_dir,
    ));
    svc_library::capabilities::evaluate_features(is_desktop, &roots).scan_external_path
}

/// POST /api/dedup/scan —— 库外文件夹「仅检查重复（不入库）」。
///
/// 只做三件事：遍历目录 → 算 SHA-256 + pHash(D4 八变体) → 按 fileHash ∪ pHash 汉明
/// 分组。**不写任何表**（不写 assets / resource / dedup_vectors / dup_edges），
/// 结果只能看和导出（没有删除入口：库外的图没有可软删的资产记录）。
pub async fn scan(State(state): State<AppState>, Json(req): Json<ScanReq>) -> Response {
    let raw = req.path.trim().to_string();
    if raw.is_empty() {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::DEDUP_PATH_EMPTY,
            json!({}),
            "path 不能为空",
        );
    }
    let root = PathBuf::from(&raw);
    if !root.is_absolute() {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::DEDUP_PATH_NOT_ABSOLUTE,
            json!({ "path": raw }),
            format!("必须给绝对路径（收到 {raw}）"),
        );
    }
    if !root.is_dir() {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::DEDUP_DIR_NOT_FOUND,
            json!({ "path": raw }),
            format!("目录不存在或不是文件夹：{raw}"),
        );
    }
    {
        let conn = state.db.lock().await;
        if !scan_external_allowed(&state, &conn) {
            return err_coded(
                StatusCode::BAD_REQUEST,
                codes::DEDUP_LOCAL_DIR_UNSUPPORTED,
                json!({}),
                "本形态不支持读取本机库外文件夹（网页/容器形态的「本地路径」是服务器磁盘）；请在桌面版使用",
            );
        }
    }
    let cap = req
        .maxFiles
        .unwrap_or(SCAN_MAX_FILES_DEFAULT)
        .clamp(1, SCAN_MAX_FILES_HARD);

    let started = std::time::Instant::now();
    let files = match svc_import::scan_dir(&root) {
        Ok(f) => f,
        Err(e) => {
            return err_coded(
                StatusCode::BAD_REQUEST,
                codes::DEDUP_DIR_READ_FAILED,
                json!({ "path": raw }),
                format!("读取目录失败（{raw}）：{e}"),
            )
        }
    };
    let picked: Vec<PathBuf> = files.iter().take(cap).cloned().collect();
    let truncated = files.len() > picked.len();
    let pending_files = files.len().saturating_sub(picked.len());

    // 三张平行表：成员（分组输入）/ 文件元信息 / D4 变体集合，下标 = 密集 asset_id（0 起）
    let mut members: Vec<engine_dedup::DupMemberD4> = Vec::with_capacity(picked.len());
    let mut meta: Vec<(PathBuf, i64, String, String)> = Vec::with_capacity(picked.len());
    let mut phashes: Vec<Vec<u64>> = Vec::with_capacity(picked.len());
    let mut unreadable: Vec<ScanUnreadable> = Vec::new();
    for p in picked.iter() {
        let bytes = match std::fs::read(p) {
            Ok(b) => b,
            Err(e) => {
                unreadable.push(ScanUnreadable {
                    path: p.to_string_lossy().to_string(),
                    error: format!("读不了文件：{e}"),
                });
                continue;
            }
        };
        let hash = core_hash::sha256_hex(&bytes);
        let luma = match core_media::luma32(p) {
            Ok(l) => l,
            Err(e) => {
                unreadable.push(ScanUnreadable {
                    path: p.to_string_lossy().to_string(),
                    error: format!("解不出像素（不是图片/已损坏）：{e}"),
                });
                continue;
            }
        };
        let hs = match engine_dedup::d4_phashes(&luma) {
            Ok(h) => h,
            Err(e) => {
                unreadable.push(ScanUnreadable {
                    path: p.to_string_lossy().to_string(),
                    error: format!("pHash 计算失败：{e}"),
                });
                continue;
            }
        };
        let slot = members.len() as i64;
        members.push(engine_dedup::DupMemberD4 {
            asset_id: slot,
            file_hash: Some(hash.clone()),
            phashes: hs.to_vec(),
        });
        meta.push((
            p.clone(),
            bytes.len() as i64,
            hash,
            engine_dedup::phash_hex_of(hs[0]),
        ));
        phashes.push(hs.to_vec());
    }

    let found = engine_dedup::build_dup_groups_d4(&members, engine_dedup::PHASH_THRESHOLD);
    let mut groups: Vec<ScanGroupOut> = Vec::with_capacity(found.len());
    let mut kind_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut reclaim_total = 0i64;
    for (gi, g) in found.iter().enumerate() {
        // 预选保留：文件最大者优先（同等大小取路径序小者）
        let keep_idx = *g
            .members
            .iter()
            .min_by_key(|&&i| (-(meta[i].1), meta[i].0.to_string_lossy().to_string()))
            .unwrap_or(&g.members[0]);
        let mut out_members: Vec<ScanMemberOut> = Vec::with_capacity(g.members.len());
        for &i in g.members.iter() {
            let relation = if i == keep_idx {
                None
            } else {
                engine_dedup::best_variant_match(&phashes[keep_idx], &phashes[i])
                    .map(|m| format!("{}（汉明距离 {}）", m.transform.label(), m.distance))
            };
            let (path, sz, hash, ph) = &meta[i];
            out_members.push(ScanMemberOut {
                path: path.to_string_lossy().to_string(),
                file_name: path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                file_sz: *sz,
                file_hash: hash.clone(),
                p_hash: ph.clone(),
                keep: i == keep_idx,
                relation,
            });
        }
        out_members.sort_by_key(|m| (!m.keep, m.path.clone()));
        let reclaim: i64 = out_members
            .iter()
            .filter(|m| !m.keep)
            .map(|m| m.file_sz)
            .sum();
        let mut evidence: Vec<String> = g.edges.iter().map(|e| e.evidence_label()).collect();
        evidence.sort();
        evidence.dedup();
        reclaim_total += reclaim;
        *kind_counts.entry(g.kind.as_str().to_string()).or_insert(0) += 1;
        groups.push(ScanGroupOut {
            group_id: (gi + 1) as i64,
            kind: g.kind.as_str().to_string(),
            size: out_members.len(),
            reclaim_bytes: reclaim,
            evidence,
            members: out_members,
        });
    }
    // 大组在前（同等大小按组号升序：两次扫描顺序稳定）
    groups.sort_by(|a, b| b.size.cmp(&a.size).then(a.group_id.cmp(&b.group_id)));

    let report = ScanReport {
        scan_id: new_scan_id(),
        path: raw.clone(),
        scanned: members.len(),
        truncated,
        pending_files,
        unreadable,
        total_groups: groups.len(),
        reclaim_bytes: reclaim_total,
        kind_counts,
        groups,
        elapsed_ms: started.elapsed().as_millis() as i64,
        generated_at: svc_import::now_secs(),
        algorithm: "SHA-256 精确重复 ∪ pHash-D4 八变体（汉明距离）近似重复".to_string(),
        thresholds: format!("pHash 汉明 ≤{}", engine_dedup::PHASH_THRESHOLD),
        note: if truncated {
            format!(
                "目录内共 {} 个图片，本次只扫前 {cap} 个（maxFiles 上限）；缩小目录或分批再扫",
                files.len()
            )
        } else {
            "只读扫描：不入库、不写向量；结果只用于查看与导出".to_string()
        },
    };
    cache_scan(&report);
    Json(json!({ "ok": true, "report": report })).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/dedup/export
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ExportQuery {
    /// csv（默认）/ md
    pub format: Option<String>,
    /// 库内范围（与 groups 同一口径）
    pub range: Option<String>,
    /// library（默认）/ scan:<scanId>
    pub scope: Option<String>,
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// unixepoch 秒 → `YYYYMMDD`（只用于文件名；内容里有精确 generatedAt）。
fn ymd_utc(secs: i64) -> String {
    // Howard Hinnant 的 civil_from_days
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}")
}

fn attachment(format: &str, tag: &str, body: String) -> Response {
    let date = ymd_utc(svc_import::now_secs());
    let (mime, name) = if format == "csv" {
        ("text/csv; charset=utf-8", format!("dedup-{tag}-{date}.csv"))
    } else {
        (
            "text/markdown; charset=utf-8",
            format!("dedup-{tag}-{date}.md"),
        )
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        body,
    )
        .into_response()
}

/// GET /api/dedup/export —— 去重报告导出（CSV / Markdown，含算法与阈值元数据）。
///
/// 元数据里的模型名与阈值是**审计需要**（KI-6：只进报告，不进界面文案）。
pub async fn export(State(state): State<AppState>, Query(q): Query<ExportQuery>) -> Response {
    let format = q.format.as_deref().unwrap_or("csv").to_ascii_lowercase();
    if format != "csv" && format != "md" {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::DEDUP_BAD_FORMAT,
            json!({ "format": format }),
            "format 只支持 csv / md",
        );
    }
    let scope = q.scope.as_deref().unwrap_or("library");
    let now = svc_import::now_secs();

    if let Some(scan_id) = scope.strip_prefix("scan:") {
        let Some(report) = cached_scan(scan_id.trim()) else {
            return err_coded(
                StatusCode::NOT_FOUND,
                codes::DEDUP_SCAN_STALE,
                json!({ "scanId": scan_id.trim() }),
                "扫描结果已过期（服务重启或缓存被挤掉）：请重新扫描后再导出",
            );
        };
        let body = if format == "csv" {
            scan_csv(&report)
        } else {
            scan_md(&report)
        };
        return attachment(&format, "scan", body);
    }
    if scope != "library" {
        return err(
            StatusCode::BAD_REQUEST,
            format!("scope 只支持 library / scan:<id>，收到 {scope}"),
        );
    }

    let range = match parse_range(q.range.as_deref()) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    let conn = state.db.lock().await;
    let scope = match resolve_scope(&conn, &range) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    let mut all = match svc_library::dup_groups(&conn) {
        Ok(g) => g,
        Err(e) => return svc_err(e),
    };
    let ignores = ignored_signatures(&conn).unwrap_or_default();
    let ignored_count = ignores.len();
    all.retain(|g| !ignores.contains(&group_signature(g)));
    let (groups, _meta) = apply_range(all, &scope);
    let judge =
        svc_pipeline::embed_queue::dup_judge_status(&conn, core_config::DEDUP_EMBEDDER).ok();

    let body = if format == "csv" {
        library_csv(&groups, &range, &judge, ignored_count, now)
    } else {
        library_md(&groups, &range, &judge, ignored_count, now)
    };
    attachment(&format, "library", body)
}

/// 库内报告的元数据头（模型名/阈值只出现在这里，不进 UI —— KI-6）。
fn library_meta(
    range: &Range,
    judge: &Option<svc_pipeline::embed_queue::DupJudgeStatus>,
    ignored: usize,
    now: i64,
) -> Vec<String> {
    let mut out = vec![
        format!("generatedAt={now}"),
        format!("range={}", range.label()),
        "algorithm=fileHash(SHA-256) ∪ pHash-D4(汉明) ∪ 视觉判重塔(余弦)".to_string(),
        format!(
            "visualModel={} nearThreshold={} variantThreshold={} phashThreshold={}",
            core_config::DEDUP_EMBEDDER,
            engine_search::NEAR_DUP_THRESHOLD,
            engine_search::VARIANT_THRESHOLD,
            engine_dedup::PHASH_THRESHOLD
        ),
        format!("ignoredGroups={ignored}"),
    ];
    if let Some(j) = judge {
        out.push(format!(
            "judge vectors={} edges={} backfillDone={} backfillTruncated={}",
            j.vectors, j.edges, j.backfill_done, j.backfill_truncated
        ));
    }
    out
}

fn library_csv(
    groups: &[DupGroupView],
    range: &Range,
    judge: &Option<svc_pipeline::embed_queue::DupJudgeStatus>,
    ignored: usize,
    now: i64,
) -> String {
    let mut s = String::from("# DAMDesk 去重报告\n");
    for line in library_meta(range, judge, ignored, now) {
        s.push_str(&format!("# {line}\n"));
    }
    s.push_str("groupId,kind,size,keepAssetId,assetId,fileName,absPath,fileSz,fileHash,pHash,similarity,reclaimBytes\n");
    for g in groups {
        for m in &g.members {
            let sim = m.similarity.map(|v| format!("{v:.4}")).unwrap_or_default();
            s.push_str(&format!(
                "{},{},{},{},{},{},{},{},{},{},{},{}\n",
                g.group_id,
                csv_escape(&g.kind),
                g.size,
                g.suggested_keep,
                m.asset_id,
                csv_escape(&m.file_name),
                csv_escape(&m.abs_path),
                m.file_sz,
                m.file_hash.clone().unwrap_or_default(),
                m.p_hash.clone().unwrap_or_default(),
                sim,
                g.reclaim_bytes
            ));
        }
    }
    s
}

fn library_md(
    groups: &[DupGroupView],
    range: &Range,
    judge: &Option<svc_pipeline::embed_queue::DupJudgeStatus>,
    ignored: usize,
    now: i64,
) -> String {
    let mut s = String::from("# DAMDesk 去重报告\n\n");
    for line in library_meta(range, judge, ignored, now) {
        s.push_str(&format!("- `{line}`\n"));
    }
    s.push_str(&format!("\n共 {} 组\n\n", groups.len()));
    for g in groups {
        s.push_str(&format!(
            "## 组 #{} · {} · {} 张 · 可释放 {} 字节\n\n",
            g.group_id, g.kind, g.size, g.reclaim_bytes
        ));
        s.push_str("| 保留 | assetId | 文件名 | 大小 | 路径 |\n|---|---|---|---|---|\n");
        for m in &g.members {
            s.push_str(&format!(
                "| {} | {} | {} | {} | `{}` |\n",
                if m.asset_id == g.suggested_keep {
                    "✓"
                } else {
                    ""
                },
                m.asset_id,
                m.file_name.replace('|', "\\|"),
                m.file_sz,
                m.abs_path
            ));
        }
        s.push('\n');
    }
    s
}

fn scan_csv(r: &ScanReport) -> String {
    let mut s = String::from("# DAMDesk 库外文件夹查重报告（只读、未入库）\n");
    s.push_str(&format!("# generatedAt={}\n", r.generated_at));
    s.push_str(&format!("# path={}\n", r.path));
    s.push_str(&format!(
        "# scanned={} truncated={} pendingFiles={}\n",
        r.scanned, r.truncated, r.pending_files
    ));
    s.push_str(&format!("# algorithm={}\n", r.algorithm));
    s.push_str(&format!("# thresholds={}\n", r.thresholds));
    s.push_str("groupNo,kind,size,keep,fileName,absPath,fileSz,fileHash,pHash,relation\n");
    for g in &r.groups {
        for m in &g.members {
            s.push_str(&format!(
                "{},{},{},{},{},{},{},{},{},{}\n",
                g.group_id,
                csv_escape(&g.kind),
                g.size,
                if m.keep { "keep" } else { "dup" },
                csv_escape(&m.file_name),
                csv_escape(&m.path),
                m.file_sz,
                m.file_hash,
                m.p_hash,
                csv_escape(m.relation.as_deref().unwrap_or(""))
            ));
        }
    }
    s
}

fn scan_md(r: &ScanReport) -> String {
    let mut s = String::from("# 库外文件夹查重报告（只读，未入库）\n\n");
    s.push_str(&format!("- 目录：`{}`\n", r.path));
    s.push_str(&format!(
        "- 扫描 {} 个文件（truncated={}）· 耗时 {} ms\n",
        r.scanned, r.truncated, r.elapsed_ms
    ));
    s.push_str(&format!("- 判据：{}；{}\n", r.algorithm, r.thresholds));
    s.push_str(&format!("\n共 {} 组\n\n", r.total_groups));
    for g in &r.groups {
        s.push_str(&format!(
            "## 组 {} · {} · {} 张 · 可释放 {} 字节\n\n",
            g.group_id, g.kind, g.size, g.reclaim_bytes
        ));
        if !g.evidence.is_empty() {
            s.push_str(&format!("证据：{}\n\n", g.evidence.join("；")));
        }
        s.push_str("| 保留 | 文件名 | 大小 | 关系 | 路径 |\n|---|---|---|---|---|\n");
        for m in &g.members {
            s.push_str(&format!(
                "| {} | {} | {} | {} | `{}` |\n",
                if m.keep { "✓" } else { "" },
                m.file_name.replace('|', "\\|"),
                m.file_sz,
                m.relation.clone().unwrap_or_default(),
                m.path
            ));
        }
        s.push('\n');
    }
    s
}

// ---------------------------------------------------------------------------
// 裁决 / 回收站 / 补判（W5 + KI-2b 既有能力，保持不变）
// ---------------------------------------------------------------------------

/// POST /api/dedup/backfill 查询参数（`max_assets=0` = 全量，会长时间占库锁）。
#[derive(Deserialize)]
pub struct BackfillQuery {
    pub max_assets: Option<usize>,
}

/// 单次补判默认上限：补判在请求线程里同步跑并持库锁，默认只给一小批，
/// 让调用方（UI/脚本）循环调用直到 `truncated=false`（每次都可重入、幂等）。
pub const DEFAULT_BACKFILL_BATCH: usize = 500;

/// POST /api/dedup/resolve 请求体。
#[derive(Deserialize)]
pub struct ResolveReq {
    pub keepAssetId: i64,
    pub removeIds: Vec<i64>,
}

/// POST /api/dedup/resolve —— 组裁决：保留 keep，其余软删（回收站可恢复）。
pub async fn resolve(
    State(state): State<AppState>,
    Json(req): Json<ResolveReq>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match svc_library::dedup_resolve(&conn, req.keepAssetId, &req.removeIds) {
        Ok(removed) => Json(json!({ "ok": true, "removed": removed })).into_response(),
        Err(e) => svc_err(e),
    }
}

/// GET /api/dedup/trash 查询参数。
#[derive(Deserialize)]
pub struct TrashQuery {
    pub limit: Option<u32>,
}

/// GET /api/dedup/trash —— 回收站（deleted=1，最近在前）。
pub async fn trash(
    State(state): State<AppState>,
    Query(q): Query<TrashQuery>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match svc_library::list_deleted(&conn, q.limit.unwrap_or(200)) {
        Ok(items) => Json(json!({ "items": items })).into_response(),
        Err(e) => svc_err(e),
    }
}

/// POST /api/dedup/backfill —— 存量一次性补判（可重入）。
///
/// 场景：判重塔的向量是后加的（KI-2a），老库里的图早就有向量却没有「边」；
/// 也让「重嵌入/换模型后边过期」有自愈入口。语义：
/// - 可重入：边写入先删后写 + 主键，跑多少次都不会造出重复/脏边；
/// - 分批：默认一批 `DEFAULT_BACKFILL_BATCH` 个资产，响应 `truncated=true` 表示还有
///   剩余 → 调用方再调一次（`max_assets=0` 可一次全量，但会长时间占库锁）；
/// - 状态：`backfillDone/backfillTruncated` 落 `dup_judge_state`，供去重页三态徽标
///   （未检测 / 检测中 / 已确认）使用。
pub async fn backfill(
    State(state): State<AppState>,
    Query(q): Query<BackfillQuery>,
) -> impl IntoResponse {
    let max_assets = q.max_assets.unwrap_or(DEFAULT_BACKFILL_BATCH);
    let conn = state.db.lock().await;
    match svc_pipeline::embed_queue::backfill_dup_edges(
        &conn,
        core_config::DEDUP_EMBEDDER,
        max_assets,
    ) {
        Ok(report) => {
            let judge =
                svc_pipeline::embed_queue::dup_judge_status(&conn, core_config::DEDUP_EMBEDDER)
                    .ok();
            // vectors == 0 = 判重塔没跑过（web 容器关本地推理 / 桌面还没嵌入）：
            // 明说「未检测」，别让「0 组」被读成「没有重复」。
            let hint = (report.vectors == 0)
                .then_some("判重塔还没有向量（未嵌入或本形态不支持本地推理）：当前无法做视觉判重");
            Json(json!({
                "ok": true,
                "report": report,
                "judge": judge,
                "hint": hint,
            }))
            .into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// POST /api/dedup/restore 请求体。
#[derive(Deserialize)]
pub struct RestoreReq {
    pub ids: Vec<i64>,
}

/// POST /api/dedup/restore —— 从回收站恢复。
pub async fn restore(
    State(state): State<AppState>,
    Json(req): Json<RestoreReq>,
) -> impl IntoResponse {
    if req.ids.is_empty() {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::DEDUP_IDS_EMPTY,
            json!({}),
            "ids 不能为空",
        );
    }
    let conn = state.db.lock().await;
    match svc_library::restore_assets(&conn, &req.ids) {
        Ok(restored) => Json(json!({ "ok": true, "restored": restored })).into_response(),
        Err(e) => svc_err(e),
    }
}
/// 供单测断言「库外扫描不改库」：资产 / 资源 / 判重向量 / 判重边四类计数快照。
pub fn table_counts(conn: &Connection) -> BTreeMap<String, i64> {
    let mut out = BTreeMap::new();
    for (tag, sql) in [
        ("assets", "SELECT COUNT(*) FROM assets"),
        ("resource", "SELECT COUNT(*) FROM resource"),
        ("dup_edges", "SELECT COUNT(*) FROM dup_edges"),
    ] {
        let n: i64 = conn.query_row(sql, [], |r| r.get(0)).unwrap_or(-1);
        out.insert(tag.to_string(), n);
    }
    // 判重向量表按模型动态建（不存在 = 0）
    let vec_n: i64 = core_storage::vecstore::count(conn, core_config::DEDUP_EMBEDDER).unwrap_or(0);
    out.insert("dedup_vectors".to_string(), vec_n);
    out
}

/// 单次彻底删除的最大张数（与 /api/library/delete 同一上限口径）。
const PURGE_MAX_IDS: usize = 900;

/// POST /api/dedup/purge 请求体。
#[derive(Deserialize)]
pub struct PurgeReq {
    pub ids: Vec<i64>,
}

/// 彻底删除的人话拒绝理由（与 `GET /api/capabilities` 的 `featureReasons.physicalDelete`
/// **同一口径**：桌面形态说「库根不可写」，网页形态说「服务器磁盘不属于你」）。
///
/// 判定真源是 `svc_library::capabilities::evaluate_features`（与能力位同源），这里只
/// 补一句用户能懂的原因；`None` = 允许执行。
fn purge_block_reason(
    is_desktop: bool,
    roots: &[svc_library::capabilities::RootInfo],
) -> Option<String> {
    let feats = svc_library::capabilities::evaluate_features(is_desktop, roots);
    if feats.physical_delete {
        return None;
    }
    let ro_detail = roots
        .iter()
        .filter(|r| !r.writable)
        .map(|r| format!("{}（{}）", r.path, r.code.as_deref().unwrap_or("other")))
        .collect::<Vec<_>>()
        .join("、");
    Some(if is_desktop {
        format!("无法彻底删除：当前库根不可写 {ro_detail}。可改用「恢复」把图移出回收站。")
    } else {
        "网页版不支持彻底删除磁盘文件（原图在服务器磁盘上）；可改用「恢复」把图移出回收站。"
            .to_string()
    })
}

/// POST /api/dedup/purge —— 回收站 → 永久删除（不可逆）。
///
/// 三重门：① 只接受回收站内（deleted=1）的 id，其余进 `skippedIds`（不静默放过）；
/// ② 能力位 `physicalDelete` 为 false（网页容器 / 库根只读）→ 400 + 人话，不做假成功；
/// ③ 张数上限与 `/api/library/delete` 一致。
///
/// 返回 `{ok, purged, skipped, purgedIds, skippedIds, rows, vectors, thumbs, filesDeleted, freedBytes}`。
pub async fn purge(State(state): State<AppState>, Json(req): Json<PurgeReq>) -> impl IntoResponse {
    if req.ids.is_empty() {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::DEDUP_IDS_EMPTY,
            json!({}),
            "ids 不能为空",
        );
    }
    if req.ids.len() > PURGE_MAX_IDS {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::DEDUP_TOO_MANY_IDS,
            json!({ "max": PURGE_MAX_IDS, "got": req.ids.len() }),
            format!(
                "一次最多彻底删除 {PURGE_MAX_IDS} 张（收到 {}）",
                req.ids.len()
            ),
        );
    }
    // 能力门（与 capabilities 同源判定；形态信号由进程壳给）
    let is_desktop = crate::is_desktop_form();
    let roots = {
        let conn = state.db.lock().await;
        svc_library::capabilities::probe_roots(&svc_library::capabilities::library_roots(
            &conn,
            &state.data_dir,
        ))
    };
    if let Some(reason) = purge_block_reason(is_desktop, &roots) {
        return err(StatusCode::BAD_REQUEST, reason);
    }
    let conn = state.db.lock().await;
    match svc_library::purge_assets(&conn, &state.thumbs_root, &req.ids) {
        Ok(rep) => Json(json!({
            "ok": true,
            "purged": rep.purged.len(),
            "skipped": rep.skipped(),
            "purgedIds": rep.purged,
            "skippedIds": rep.not_in_trash,
            "rows": rep.rows,
            "vectors": rep.vectors,
            "thumbs": rep.thumbs,
            "filesDeleted": rep.files_deleted,
            "freedBytes": rep.freed_bytes,
        }))
        .into_response(),
        Err(e) => svc_err(e),
    }
}
