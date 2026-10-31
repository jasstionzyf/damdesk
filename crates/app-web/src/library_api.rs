#![allow(non_snake_case)] // 本模块 DTO 字段 = 前端 JSON camelCase 契约，勿改
//! W2 图库读 API：资产列表（过滤+分页）/ 统计 / 目录树 / 单资产标签。
//! 复用 svc-library（L2 读模型），薄壳零容忍。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use crate::error_codes::{codes, err_coded};
use crate::AppState;
use core_types::ImportFilter;

fn err(status: StatusCode, msg: impl Into<String>) -> axum::response::Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

/// GET /api/library/assets 查询参数（ImportFilter 的 HTTP 形态）。
#[derive(Deserialize, Default)]
pub struct AssetQuery {
    pub mediaType: Option<String>,
    pub missing: Option<i64>,
    pub ext: Option<String>,
    pub tag: Option<String>,
    pub dirPrefix: Option<String>,
    pub untagged: Option<bool>,
    pub recent: Option<bool>,
    /// E1 筛选芯片：星级下限
    pub minRating: Option<i64>,
    pub colorLabel: Option<String>,
    /// E4：assetId 集合（逗号分隔，≤900）——相似结果临时集合回读
    pub ids: Option<String>,
    // ---- EXIF2 维度 ----
    /// 拍摄时间范围（unix 秒，含端点）
    pub takenFrom: Option<i64>,
    pub takenTo: Option<i64>,
    /// 相机/镜头型号精确匹配
    pub cameraModel: Option<String>,
    pub lensModel: Option<String>,
    pub isoMin: Option<i64>,
    pub isoMax: Option<i64>,
    /// 画幅过滤 landscape|portrait|square
    pub aspect: Option<String>,
    /// 排序键：缺省 importedAt；"takenAt" = 拍摄时间
    pub sort: Option<String>,
    /// **已下线**（DECouple-BE，用户裁决 2026-10-31）：图库列表不再接受判定维度过滤。
    ///
    /// 保留字段只为把旧客户端/深链显式挡回去（400 `compliance_filter_moved`），
    /// 不再静默忽略 —— 「筛了没生效」比报错更难查。判定维度查询请用
    /// `GET /api/compliance/jobs/{runId}/verdicts?status=`。
    pub compliance: Option<String>,
    /// D3 无法预览：1=只看解码失败（无尺寸）的资产；0=只看可预览；缺省=不过滤
    pub broken: Option<i64>,
    /// C7 关键词全库检索（`?q=`）：文件名 / 标题 / 描述 / 路径 的大小写不敏感子串。
    /// 为什么在服务端：客户端只能筛「已加载的这一页」，全库命中的图在首页之后永远搜不到。
    pub q: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

impl From<AssetQuery> for ImportFilter {
    fn from(q: AssetQuery) -> Self {
        // recent=最近导入（列表本身按 importedAt DESC 排序，只截 limit）
        let limit = if q.recent == Some(true) {
            Some(q.limit.unwrap_or(24).min(200))
        } else {
            q.limit
        };
        // ids：解析逗号分隔；非数字段忽略；≤900（sqlite 变量上限内）
        let ids = q.ids.map(|s| {
            s.split(',')
                .filter_map(|t| t.trim().parse::<i64>().ok())
                .take(900)
                .collect::<Vec<i64>>()
        });
        // ids 模式默认取全集（临时集合回读不该被默认分页截断）
        let limit = if ids.is_some() && q.limit.is_none() {
            ids.as_ref().map(|v| v.len().clamp(1, 1000) as i64)
        } else {
            limit
        };
        ImportFilter {
            mediaType: q.mediaType,
            missing: q.missing,
            ext: q.ext,
            tag: q.tag,
            dirPrefix: q.dirPrefix,
            untagged: q.untagged,
            minRating: q.minRating,
            colorLabel: q.colorLabel,
            ids,
            takenFrom: q.takenFrom,
            takenTo: q.takenTo,
            cameraModel: q.cameraModel,
            lensModel: q.lensModel,
            isoMin: q.isoMin,
            isoMax: q.isoMax,
            aspect: q.aspect,
            sort: q.sort,
            // D3 无法预览（与 stats.broken 同一段判定：svc_library::BROKEN_COND）
            broken: q.broken,
            // C7 关键词：URL 参数名 `q`（与图库页面 URL 的 `?q=` 同名），存储层字段叫 query
            query: q.q,
            // C6：compliance 需要激活 packId（这里没有 conn），由 assets handler 注入
            compliance: None,
            limit,
            offset: q.offset,
        }
    }
}

/// 资产列表 + 总数（分页由 offset/limit 驱动，前端「加载更多」）。
pub async fn assets(
    State(state): State<AppState>,
    Query(q): Query<AssetQuery>,
) -> impl IntoResponse {
    // EXIF2：aspect 非法值 = 明确 400（不静默当不过滤，防「筛选没生效」错觉）
    if let Some(a) = &q.aspect {
        if !matches!(a.as_str(), "landscape" | "portrait" | "square") {
            return err(
                StatusCode::BAD_REQUEST,
                format!("aspect 只支持 landscape|portrait|square，收到: {a}"),
            );
        }
    }
    if let Some(s) = &q.sort {
        if s != "takenAt" && s != "importedAt" {
            return err(
                StatusCode::BAD_REQUEST,
                format!("sort 只支持 importedAt|takenAt，收到: {s}"),
            );
        }
    }
    // D3：broken 只接受 1/0 —— 静默忽略非法值会造出"筛选没生效"的假象
    if let Some(b) = q.broken {
        if b != 0 && b != 1 {
            return err(
                StatusCode::BAD_REQUEST,
                format!("broken 只支持 1|0，收到: {b}"),
            );
        }
    }
    // DECouple-BE：图库域不读判定。`?compliance=` 一律 400 并指路 job 域端点
    // （不静默忽略：静默忽略会让人以为「筛选没生效」）。
    if q.compliance.is_some() {
        // 稳定错误码放在 `error` 字段（沿用本模块 err 的两参形态），文案指路 job 域端点。
        return err(
            StatusCode::BAD_REQUEST,
            "compliance_filter_moved：图库列表不再支持判定维度过滤（图库与 job 已解耦）。\
             请用 GET /api/compliance/jobs/{runId}/verdicts?status=pass|fail|undetermined|unjudged 按一次执行下钻",
        );
    }
    let f: ImportFilter = q.into();
    let conn = state.db.lock().await;
    let total = svc_library::export::count_assets(&conn, &f);
    let items = svc_library::list_assets(&conn, &f);
    drop(conn);
    match (total, items) {
        (Ok(t), Ok(v)) => (StatusCode::OK, Json(json!({ "total": t, "items": v }))).into_response(),
        (Err(e), _) | (_, Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// 库统计（侧栏智能筛选计数 + 状态栏）。
///
/// DESKTOP-MISSING-STAT-1：响应里 missing 附带 missingCheckedAt/missingStale —— 前端
/// 必须把"失联数"和"核查新鲜度"一起显示，过期时引导用户点「重新核查」（见 verify）。
///
/// GH-5：响应附加判重塔可用性（`dedupAvailable` / `dedupUnavailableReason`）——
/// DINOv3 不随安装包分发（Meta 许可禁止再分发）→ 判重塔是可选能力，缺件时这里**如实报缺**
/// 并给自备指引（含模型根绝对路径），不许 panic、不许静默把 pHash 层当判重塔结论
/// （判据三层口径在 dedup_api，`near_visual` 没有向量时按「未检测」呈现，与本字段一致）。
pub async fn stats(State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match svc_library::stats(&conn) {
        Ok(v) => {
            let mut body = serde_json::to_value(&v).unwrap_or_else(|_| json!({}));
            let available = engine_embed::LOCAL_INFER
                && engine_embed::factory::dedup_files_present(&state.models_root);
            if let Some(obj) = body.as_object_mut() {
                obj.insert("dedupAvailable".into(), json!(available));
                if !available {
                    let reason = if engine_embed::LOCAL_INFER {
                        format!(
                            "判重不可用，需自备模型（Meta 许可）：把 model.onnx 与 model.onnx_data \
                             放入 {}",
                            core_config::model_dir(&state.models_root, core_config::DEDUP_EMBEDDER)
                                .display()
                        )
                    } else {
                        "判重不可用：本构建未编入本地 ONNX 推理（web 容器形态无判重塔）".to_string()
                    };
                    obj.insert("dedupUnavailableReason".into(), json!(reason));
                }
            }
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// POST /api/library/verify —— 全库失联现场核查（DESKTOP-MISSING-STAT-1 的显式方案）。
///
/// 为什么不是"stats 每次现场 stat"：核查成本 = 每资产一次 stat（10k 库实测见
/// reports/MISSING-STAT-1.md），放进 GET 会让状态栏轮询拖住整个库。所以：
///   - 核查做成显式动作（本接口 + 启动时一次），结果落 settings；
///   - stats 如实带新鲜度标记，过期就显示"待核查"，不假装实时。
///
/// 三段式避免长持锁：持锁读目标 → **放锁** stat（spawn_blocking，不阻塞 tokio 线程）
/// → 持锁写回。核查期间的 UI 请求不会被几百毫秒~几秒的扫描卡死。
/// 单飞：已在核查中 → 409（不做队列，前端按钮轮询即可）。
pub async fn verify(State(state): State<AppState>) -> impl IntoResponse {
    let Some(_guard) = state.verify.try_begin() else {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "running": true, "error": "missing verify already running" })),
        )
            .into_response();
    };
    let started = std::time::Instant::now();
    let targets = {
        let conn = state.db.lock().await;
        match svc_import::collect_missing_targets(&conn) {
            Ok(t) => t,
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    };
    let join = {
        let thumbs_root = state.thumbs_root.clone();
        tokio::task::spawn_blocking(move || {
            svc_import::probe_paths_checked(&targets, Some(&thumbs_root))
        })
        .await
    };
    let probes = match join {
        Ok(p) => p,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("probe task: {e}"),
            )
        }
    };
    let report = {
        let conn = state.db.lock().await;
        match svc_import::apply_missing_probes_repairing(
            &conn,
            &probes,
            started,
            Some(&state.thumbs_root),
        ) {
            Ok(r) => r,
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    };
    (StatusCode::OK, Json(json!(report))).into_response()
}

/// 失联核查单飞状态（DESKTOP-MISSING-STAT-1）：同一时刻只允许一次全库扫描，
/// 避免重复点击/多标签页叠加 IO。不做队列 —— 按钮轮询看到 409 就等。
#[derive(Default)]
pub struct VerifyState {
    running: std::sync::atomic::AtomicBool,
}

impl VerifyState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 抢到即拿 guard（drop 释放）；已有核查在跑 → None。
    pub fn try_begin(self: &Arc<Self>) -> Option<VerifyGuard> {
        if self
            .running
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
        {
            Some(VerifyGuard(self.clone()))
        } else {
            None
        }
    }

    /// 是否有核查在跑（测试/状态接口用）。
    pub fn is_running(&self) -> bool {
        self.running.load(std::sync::atomic::Ordering::SeqCst)
    }
}

pub struct VerifyGuard(Arc<VerifyState>);

impl Drop for VerifyGuard {
    fn drop(&mut self) {
        self.0
            .running
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 单资产标签（检查器右栏）。
pub async fn asset_tags(
    State(state): State<AppState>,
    Path(asset_id): Path<i64>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match svc_library::asset_tags(&conn, asset_id) {
        Ok(v) => (StatusCode::OK, Json(json!(v))).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// 单资产打标任务结果（维度名: 值；与标签值分开返回）。
pub async fn asset_tag_results(
    State(state): State<AppState>,
    Path(asset_id): Path<i64>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match svc_library::asset_tag_results(&conn, asset_id) {
        Ok(v) => (StatusCode::OK, Json(json!(v))).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// 标签频次（E1 侧栏标签筛选，?limit= 默认 30）。
#[derive(Debug, Deserialize)]
pub struct TagsQuery {
    pub limit: Option<i64>,
    /// task：前缀候选（搜索页输入建议）。**带本参数**时响应是
    /// `{items:[{value,key,count}], total, hasMore}`；不带时维持旧数组形态
    ///（侧栏标签云/智能文件夹的既有消费方零改动）。
    pub prefix: Option<String>,
    /// task：组过滤（值 ∈ `['key-', 'key-'‖U+10FFFF)`，即 `key-` 前缀的全体值）。
    /// 可与 prefix 组合（服务端对两个范围求交）；单独用 = 列出该 key 下全部值。
    pub key: Option<String>,
    /// task：分页兼容（排序 `count DESC, value ASC` 已稳定，offset 只是位移；
    /// keyset 分页由调用方按该排序自取）。仅带 offset（无 prefix/key）= 全量候选翻页。
    pub offset: Option<i64>,
    /// task：`/tags` 总览页模式（值列表 = svc `tag_page`）。带本参数才启用
    /// `keyless`/`afterCount`/`afterValue`；不带 = 老语义（候选/标签云），选择器路径零改动。
    pub page: Option<i64>,
    /// 无前缀（裸值）组：值里没有 `-`（或 `-` 在首位），按 `value ASC` 提前终止分页
    pub keyless: Option<i64>,
    /// keyset 游标（上一页最后一行的 count + value）——排序 `(count DESC, value ASC)` 下无缝续页
    #[serde(rename = "afterCount")]
    pub after_count: Option<i64>,
    #[serde(rename = "afterValue")]
    pub after_value: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CamerasQuery {
    pub limit: Option<i64>,
}

/// GET /api/library/cameras — EXIF2 相机型号聚合（筛选下拉数据源）。
pub async fn cameras(
    State(state): State<AppState>,
    Query(q): Query<CamerasQuery>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match svc_library::camera_models(&conn, q.limit.unwrap_or(50).clamp(1, 100) as u32) {
        Ok(rows) => (
            StatusCode::OK,
            Json(json!({
                "cameras": rows
                    .into_iter()
                    .map(|(model, count)| json!({ "model": model, "count": count }))
                    .collect::<Vec<_>>(),
            })),
        )
            .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn tags(State(state): State<AppState>, Query(q): Query<TagsQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(30);
    let conn = state.db.lock().await;
    // task/task：带 prefix/key/offset 任一 → 候选形态
    // `{items:[{value,key,count}], total, hasMore}`；全不带 → 旧数组形态（兼容不破）。
    let cloud_items = |v: Vec<(String, i64)>| -> Vec<serde_json::Value> {
        v.into_iter()
            .map(|(value, count)| json!({ "value": value, "count": count }))
            .collect()
    };
    let candidate_items = |v: Vec<(String, i64)>| -> Vec<serde_json::Value> {
        v.into_iter()
            .map(|(value, count)| {
                // key = 第一个 `-` 前的段（key-val 形态）；裸值 / 前导 `-` → null
                let key = match value.find('-') {
                    Some(i) if i > 0 => Some(&value[..i]),
                    _ => None,
                };
                json!({ "value": value, "key": key, "count": count })
            })
            .collect()
    };
    if q.page.is_some() {
        // task `/tags` 总览页：值列表走 svc `tag_page`（key/prefix 求交 + keyless + keyset）。
        // 与候选路径分开：候选要 `(count DESC, value ASC)` 的 Top-N 快路径，总览要「翻页不重不漏」，
        // 后者必须能表达「上一页之后」的条件（keyset），候选路径不带游标。
        let after = match (q.after_count, q.after_value.clone()) {
            (Some(c), Some(v)) => Some((c, v)),
            _ => None,
        };
        let pq = svc_library::TagPageQuery {
            key: q.key.as_deref(),
            prefix: q.prefix.as_deref(),
            keyless: q.keyless.unwrap_or(0) != 0,
            limit,
            offset: q.offset.unwrap_or(0),
            after: after.clone(),
        };
        return match svc_library::tag_page(&conn, &pq) {
            Ok(page) => (
                StatusCode::OK,
                Json(json!({
                    "items": candidate_items(page.rows),
                    "total": page.total,
                    "hasMore": page.has_more,
                })),
            )
                .into_response(),
            Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
        };
    }
    if q.prefix.is_none() && q.key.is_none() && q.offset.is_none() {
        return match svc_library::tag_cloud(&conn, limit) {
            Ok(v) => (StatusCode::OK, Json(json!(cloud_items(v)))).into_response(),
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
    }
    match svc_library::tag_candidates(
        &conn,
        q.prefix.as_deref(),
        q.key.as_deref(),
        limit,
        q.offset.unwrap_or(0),
    ) {
        Ok((rows, total)) => {
            let has_more = total > rows.len() as i64 + q.offset.unwrap_or(0);
            (
                StatusCode::OK,
                Json(json!({
                    "items": candidate_items(rows),
                    "total": total,
                    "hasMore": has_more,
                })),
            )
                .into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// GET /api/library/tag-groups —— `/tags` 左栏：每个 key 的值数 + 值总数（task）。
///
/// key 由 value 前缀派生（不加列），来自一次 distinct 值字典扫描（7.5M 行实测 0.92s）；
/// **不给「key × 资产数」**——那是每行聚合（实测 3.26s），见 svc-library 顶部实测表。
pub async fn tag_groups(State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match svc_library::tag_key_groups(&conn) {
        Ok((items, total)) => (
            StatusCode::OK,
            Json(json!({ "items": items, "total": total })),
        )
            .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// POST /api/library/tag-samples —— 每值最多 N 张样本图（`/tags` 右栏缩略图）。
///
/// POST 而非 GET：值里可能带逗号/特殊字符，重复 query 参数在 serde_urlencoded 下不可靠。
#[derive(Debug, Deserialize)]
pub struct TagSamplesReq {
    pub values: Vec<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

pub async fn tag_samples(
    State(state): State<AppState>,
    Json(req): Json<TagSamplesReq>,
) -> impl IntoResponse {
    if req.values.is_empty() {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::LIBRARY_VALUES_EMPTY,
            json!({}),
            "values must not be empty",
        );
    }
    let conn = state.db.lock().await;
    match svc_library::tag_samples(&conn, &req.values, req.limit.unwrap_or(3)) {
        Ok(rows) => (
            StatusCode::OK,
            Json(json!({
                "items": rows
                    .into_iter()
                    .map(|(value, ids)| json!({ "value": value, "assetIds": ids }))
                    .collect::<Vec<_>>()
            })),
        )
            .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// POST /api/library/tags/merge —— 批量合并 {values:[...], into}（task）。
#[derive(Debug, Deserialize)]
pub struct TagMergeReq {
    pub values: Vec<String>,
    #[serde(rename = "into")]
    pub target: String,
}

pub async fn tag_merge(
    State(state): State<AppState>,
    Json(req): Json<TagMergeReq>,
) -> impl IntoResponse {
    if req.values.is_empty() {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::LIBRARY_VALUES_EMPTY,
            json!({}),
            "values must not be empty",
        );
    }
    let conn = state.db.lock().await;
    match svc_library::tag_merge(&conn, &req.values, &req.target) {
        Ok(out) => (StatusCode::OK, Json(json!(out))).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

/// POST /api/library/tags/merge-preview —— 只算不写：确认框里的「影响 N 张 / 将被删除的值」。
///
/// 与 `/tags/merge` 共用 `merge_plan`（同一段计数代码），避免确认框与执行回执数字不一致。
pub async fn tag_merge_preview(
    State(state): State<AppState>,
    Json(req): Json<TagMergeReq>,
) -> impl IntoResponse {
    if req.values.is_empty() {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::LIBRARY_VALUES_EMPTY,
            json!({}),
            "values must not be empty",
        );
    }
    let conn = state.db.lock().await;
    match svc_library::tag_merge_preview(&conn, &req.values, &req.target) {
        Ok(out) => (StatusCode::OK, Json(json!(out))).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

/// POST /api/library/tags/rename —— 单值改名 {from, to}（= 单源合并到新值）。
#[derive(Debug, Deserialize)]
pub struct TagRenameReq {
    pub from: String,
    pub to: String,
}

pub async fn tag_rename(
    State(state): State<AppState>,
    Json(req): Json<TagRenameReq>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match svc_library::tag_merge(&conn, std::slice::from_ref(&req.from), &req.to) {
        Ok(out) => (StatusCode::OK, Json(json!(out))).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

/// POST /api/library/tags/delete —— 删除值 {values:[...]}，只允许在库资产计数为 0。
pub async fn tag_delete(
    State(state): State<AppState>,
    Json(req): Json<TagSamplesReq>,
) -> impl IntoResponse {
    if req.values.is_empty() {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::LIBRARY_VALUES_EMPTY,
            json!({}),
            "values must not be empty",
        );
    }
    let conn = state.db.lock().await;
    // 有资产的值 → 400 + 可读原因（svc 侧给「「x」还有 N 张资产」）
    match svc_library::tag_delete(&conn, &req.values) {
        Ok(out) => (StatusCode::OK, Json(json!(out))).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

/// E2 打分请求（批量选择集；rating 0 = 清除）。
#[derive(Deserialize)]
pub struct RateReq {
    pub ids: Vec<i64>,
    pub rating: i64,
}

/// E2 拒选请求（批量选择集；rejected=true 置 approvalStatus='rejected'）。
#[derive(Deserialize)]
pub struct RejectReq {
    pub ids: Vec<i64>,
    #[serde(default = "default_rejected")]
    pub rejected: bool,
}

fn default_rejected() -> bool {
    true
}

/// POST /api/library/rate —— 批量打分，返回 {updated}。
pub async fn rate(State(state): State<AppState>, Json(req): Json<RateReq>) -> impl IntoResponse {
    if req.ids.is_empty() {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::LIBRARY_IDS_EMPTY,
            json!({}),
            "ids must not be empty",
        );
    }
    if !(0..=5).contains(&req.rating) {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::LIBRARY_RATING_OUT_OF_RANGE,
            json!({ "rating": req.rating }),
            "rating must be 0..5",
        );
    }
    let conn = state.db.lock().await;
    match svc_library::set_rating(&conn, &req.ids, req.rating) {
        Ok(n) => (StatusCode::OK, Json(json!({ "updated": n }))).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// POST /api/library/reject —— 批量拒选/取消拒选，返回 {updated}。
pub async fn reject(
    State(state): State<AppState>,
    Json(req): Json<RejectReq>,
) -> impl IntoResponse {
    if req.ids.is_empty() {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::LIBRARY_IDS_EMPTY,
            json!({}),
            "ids must not be empty",
        );
    }
    let conn = state.db.lock().await;
    match svc_library::set_rejected(&conn, &req.ids, req.rejected) {
        Ok(n) => (StatusCode::OK, Json(json!({ "updated": n }))).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// 批量删除 / 恢复请求体（P1-4：Delete 键与工具栏「删除」按钮共用）。
#[derive(Deserialize)]
pub struct IdsReq {
    pub ids: Vec<i64>,
}

/// 一次最多处理多少个 id：与 /api/library/assets 的 ids 上限（≤900）对齐，
/// 避免 SQLite 变量数上限（默认 999）被击穿后报看不懂的错。
const MAX_IDS_PER_CALL: usize = 900;

#[allow(clippy::result_large_err)] // 错误即 Response（axum handler 惯例）
fn check_ids(ids: &[i64]) -> Result<(), axum::response::Response> {
    if ids.is_empty() {
        return Err(err_coded(
            StatusCode::BAD_REQUEST,
            codes::LIBRARY_IDS_EMPTY,
            json!({}),
            "ids must not be empty",
        ));
    }
    if ids.len() > MAX_IDS_PER_CALL {
        return Err(err_coded(
            StatusCode::BAD_REQUEST,
            codes::LIBRARY_TOO_MANY_IDS,
            json!({ "max": MAX_IDS_PER_CALL, "got": ids.len() }),
            format!("一次最多处理 {MAX_IDS_PER_CALL} 张（收到 {}）", ids.len()),
        ));
    }
    if let Some(bad) = ids.iter().find(|id| **id <= 0) {
        return Err(err_coded(
            StatusCode::BAD_REQUEST,
            codes::LIBRARY_ID_INVALID,
            json!({ "got": bad }),
            format!("assetId 必须是正整数（收到 {bad}）"),
        ));
    }
    Ok(())
}

/// POST /api/library/delete —— 批量删除（软删进回收站：deleted=1，源文件不动）。
///
/// 为什么复用 `svc_library::soft_delete_assets`：去重裁决（/api/dedup/resolve）已经用同一套
/// 回收站语义（软删可恢复、不碰磁盘），删除入口再写一条 SQL 必然漂移。返回实际置位行数
/// （幂等：重复删不计），前端据此报「已删除 N 张」并给「撤销」。
pub async fn delete_assets(
    State(state): State<AppState>,
    Json(req): Json<IdsReq>,
) -> impl IntoResponse {
    if let Err(resp) = check_ids(&req.ids) {
        return resp;
    }
    let conn = state.db.lock().await;
    match svc_library::soft_delete_assets(&conn, &req.ids) {
        Ok(n) => (StatusCode::OK, Json(json!({ "deleted": n }))).into_response(),
        Err(e) => err_coded(
            StatusCode::INTERNAL_SERVER_ERROR,
            codes::LIBRARY_DELETE_FAILED,
            json!({ "count": req.ids.len() }),
            e.to_string(),
        ),
    }
}

/// POST /api/library/restore —— 撤销删除 / 回收站恢复（deleted=0）。
///
/// 与 /api/dedup/restore 同一实现（老入口保留不动），前端删除后的「撤销」toast 直接打这里。
pub async fn restore_assets(
    State(state): State<AppState>,
    Json(req): Json<IdsReq>,
) -> impl IntoResponse {
    if let Err(resp) = check_ids(&req.ids) {
        return resp;
    }
    let conn = state.db.lock().await;
    match svc_library::restore_assets(&conn, &req.ids) {
        Ok(n) => (StatusCode::OK, Json(json!({ "restored": n }))).into_response(),
        Err(e) => err_coded(
            StatusCode::INTERNAL_SERVER_ERROR,
            codes::LIBRARY_RESTORE_FAILED,
            json!({ "count": req.ids.len() }),
            e.to_string(),
        ),
    }
}

/// 目录树：distinct 目录（absPath 取父目录）+ 每目录资产数 + 批次根列表（前端做相对标签）。
/// 分隔符无关：Windows `D:\pics\a\1.jpg` 与 POSIX `/pics/a/1.jpg` 都按本机分隔符取父目录
/// （Windows 上写死 `/` 会导致目录树恒空 —— DESKTOP-DIRS-TREE-1）。
/// v1 全表扫 assets.absPath（万级 ~几十 ms），超十万再下推 SQL 优化。
pub async fn dirs(State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.db.lock().await;
    let mut counts: std::collections::BTreeMap<String, i64> = Default::default();
    let mut stmt = match conn.prepare("SELECT absPath FROM assets WHERE deleted=0") {
        Ok(s) => s,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let rows = stmt.query_map([], |r| r.get::<_, String>(0));
    let rows = match rows {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    for row in rows {
        let abs = match row {
            Ok(a) => a,
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        if let Some(parent) = svc_library::parent_dir_any_sep(&abs) {
            // 根下文件（`/a.jpg`）的父目录是空串 → 归到文件系统根 `/`
            // （Windows `D:\a.jpg` 的父目录是 `D:`，本来就非空）
            let dir = if parent.is_empty() { "/" } else { parent };
            *counts.entry(dir.to_string()).or_insert(0) += 1;
        }
    }
    let roots: Vec<String> = svc_library::import_batches(&conn)
        .map(|bs| {
            bs.into_iter()
                // 尾分隔符去掉（`D:\pics\` 与 `D:\pics` 必须同形，否则前端根匹配失败 → 树恒空）
                .map(|b| svc_library::trim_dir_sep(&b.rootPath).to_string())
                .collect()
        })
        .unwrap_or_default();
    let items: Vec<serde_json::Value> = counts
        .into_iter()
        .map(|(dir, count)| json!({ "dir": dir, "count": count }))
        .collect();
    (
        StatusCode::OK,
        Json(json!({ "dirs": items, "roots": roots })),
    )
        .into_response()
}

/// E4 批量回执响应：results 逐条 + ok/fail 计数（部分失败不静默）。
fn op_results_response(results: Vec<svc_library::AssetOpResult>) -> axum::response::Response {
    let ok_count = results.iter().filter(|r| r.ok).count();
    (
        StatusCode::OK,
        Json(json!({
            "results": results,
            "okCount": ok_count,
            "failCount": results.len() - ok_count,
        })),
    )
        .into_response()
}

/// POST /api/library/tag —— 批量打标签 {ids, values[]}，返回逐条回执。
/// values 是标签值本身（tag 表只有 value，没有维度名）。
#[derive(Deserialize)]
pub struct TagReq {
    pub ids: Vec<i64>,
    pub values: Vec<String>,
}

pub async fn tag(State(state): State<AppState>, Json(req): Json<TagReq>) -> impl IntoResponse {
    if req.ids.is_empty() {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::LIBRARY_IDS_EMPTY,
            json!({}),
            "ids must not be empty",
        );
    }
    if req.values.iter().all(|n| n.trim().is_empty()) {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::LIBRARY_VALUES_EMPTY,
            json!({}),
            "values must not be empty",
        );
    }
    let conn = state.db.lock().await;
    match svc_library::add_tags(&conn, &req.ids, &req.values) {
        Ok(results) => op_results_response(results),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

/// POST /api/library/untag —— 移除单资产一个标签 {assetId, value}。
#[derive(Deserialize)]
pub struct UntagReq {
    pub assetId: i64,
    pub value: String,
}

pub async fn untag(State(state): State<AppState>, Json(req): Json<UntagReq>) -> impl IntoResponse {
    if req.value.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "value must not be empty");
    }
    let conn = state.db.lock().await;
    match svc_library::remove_tag(&conn, req.assetId, &req.value) {
        Ok(n) => (StatusCode::OK, Json(json!({ "removed": n }))).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// POST /api/library/color —— 批量色标 {ids, color}（null=清除；非法色名 400）。
#[derive(Deserialize)]
pub struct ColorReq {
    pub ids: Vec<i64>,
    pub color: Option<String>,
}

pub async fn color(State(state): State<AppState>, Json(req): Json<ColorReq>) -> impl IntoResponse {
    if req.ids.is_empty() {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::LIBRARY_IDS_EMPTY,
            json!({}),
            "ids must not be empty",
        );
    }
    let conn = state.db.lock().await;
    match svc_library::set_color(&conn, &req.ids, req.color.as_deref()) {
        Ok(n) => (StatusCode::OK, Json(json!({ "updated": n }))).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

/// POST /api/library/move —— 批量移动 {ids, targetDir}（磁盘 rename + absPath 同步），
/// 返回逐条回执：成功 X / 失败 Y，失败单条带原因。
#[derive(Deserialize)]
pub struct MoveReq {
    pub ids: Vec<i64>,
    pub targetDir: String,
}

/// 运行形态：桌面构建（`app-tauri` 依赖本 crate 时开 `local-infer`）能操作用户本机磁盘；
/// 网页/容器构建只能操作服务端磁盘（常为只读挂载或跨设备）。
pub const IS_DESKTOP_FORM: bool = cfg!(feature = "local-infer");

/// 按运行形态把 svc-library 的失败 code 翻成人话（`raw` 只在无法归因时原样透出）。
pub fn humanize_fs_error(desktop: bool, code: &str, raw: &str) -> String {
    match (desktop, code) {
        (false, "cross_device") | (false, "read_only_fs") => {
            "网页版不支持在磁盘上移动文件（服务端目录只读，或与目标不属同一文件系统）。\
             请在桌面版操作，或改用「复制入库（copy-in）」导入到可写库目录。"
                .to_string()
        }
        (false, "permission_denied") => {
            "网页版没有写权限移动该文件（服务端只读挂载）。请在桌面版操作。".to_string()
        }
        (true, "cross_device") | (true, "read_only_fs") => {
            "无法移动：源文件与目标目录不在同一磁盘/分区，或目录只读——请改选同一磁盘内的目录。"
                .to_string()
        }
        (true, "permission_denied") => "无法移动：没有写权限，请检查目标目录权限。".to_string(),
        (_, "not_found") => {
            "磁盘文件不存在（可能已被外部移动或删除），请先「重新定位」再移动。".to_string()
        }
        _ => raw.to_string(),
    }
}

fn humanize_move_results(
    results: Vec<svc_library::AssetOpResult>,
) -> Vec<svc_library::AssetOpResult> {
    results
        .into_iter()
        .map(|mut r| {
            if !r.ok {
                if let (Some(code), Some(raw)) = (r.code.clone(), r.error.clone()) {
                    r.error = Some(humanize_fs_error(crate::is_desktop_form(), &code, &raw));
                }
            }
            r
        })
        .collect()
}

pub async fn move_assets(
    State(state): State<AppState>,
    Json(req): Json<MoveReq>,
) -> impl IntoResponse {
    if req.ids.is_empty() {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::LIBRARY_IDS_EMPTY,
            json!({}),
            "ids must not be empty",
        );
    }
    if req.targetDir.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "targetDir must not be empty");
    }
    let mut conn = state.db.lock().await;
    // WEBCAP ①（前置拦截）：目标不可写 / 不在可写库根内 / 跨设备 → 提交前 4xx + code，
    // 不产生任何磁盘副作用。判定源 = svc_library::capabilities（与 /api/capabilities 同源）。
    // ② 的逐条分类（下面 humanize）仍然保留作兜底：预检通过后磁盘状态仍可能变。
    if let Err(rej) = svc_library::capabilities::precheck_move(
        &conn,
        &req.ids,
        req.targetDir.trim(),
        &state.data_dir,
    ) {
        let text = humanize_fs_error(crate::is_desktop_form(), &rej.code, &rej.message);
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": text, "code": rej.code, "detail": rej.message })),
        )
            .into_response();
    }
    match svc_library::move_assets(&mut conn, &req.ids, req.targetDir.trim()) {
        Ok(results) => op_results_response(humanize_move_results(results)),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

#[cfg(test)]
mod move_error_text_tests {
    use super::humanize_fs_error;

    #[test]
    fn web_form_explains_unsupported_and_points_to_desktop() {
        let web = humanize_fs_error(
            false,
            "cross_device",
            "rename 失败：Invalid cross-device link",
        );
        assert!(web.contains("网页版不支持"), "{web}");
        assert!(web.contains("桌面版"), "{web}");
        let ro = humanize_fs_error(false, "read_only_fs", "raw");
        assert!(ro.contains("网页版不支持"), "{ro}");
        let perm = humanize_fs_error(false, "permission_denied", "raw");
        assert!(perm.contains("桌面版"), "{perm}");
    }

    #[test]
    fn desktop_form_gives_环境相关_hint_not_网页版() {
        let d = humanize_fs_error(true, "cross_device", "raw");
        assert!(!d.contains("网页版"), "{d}");
        assert!(d.contains("同一磁盘"), "{d}");
        let perm = humanize_fs_error(true, "permission_denied", "raw");
        assert!(perm.contains("写权限"), "{perm}");
    }

    #[test]
    fn unknown_code_falls_back_to_raw_and_not_found_is_shared() {
        assert_eq!(humanize_fs_error(false, "other", "原始错误"), "原始错误");
        let nf = humanize_fs_error(true, "not_found", "raw");
        assert!(nf.contains("重新定位"), "{nf}");
    }
}

// ---------------- MIGRATE-TO-LIBRARY（卡 task / DT6） ----------------

/// 库根：settings.library 可配，默认 `{data_dir}/library`（与导入向导 copyin 同源）。
fn library_root(conn: &rusqlite::Connection, data_dir: &std::path::Path) -> std::path::PathBuf {
    let s: core_config::LibrarySettings =
        core_storage::settings::get_or_default(conn, core_config::LIBRARY_SETTINGS_KEY)
            .unwrap_or_default();
    s.effective_root(data_dir)
}

/// 迁移任务快照（`GET .../status` 的唯一数据源）。
///
/// 为什么是进程内单槽而不是落库：进度是**运行态**（跑完就没有意义），事实留痕在
/// `migrate_audit`。单槽 + 单任务并发（同一时刻只允许一批）是刻意的：一次迁移会写库根，
/// 两批并行既看不出进度也没意义。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateJobSnapshot {
    pub batch_id: String,
    /// running | done | failed
    pub state: String,
    pub total: usize,
    pub processed: usize,
    pub migrated: usize,
    pub skipped: usize,
    pub failed: usize,
    pub bytes: i64,
    pub started_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 跑完后的逐条回执（skipped/failed 明细、warnings）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<svc_library::MigrateReport>,
}

fn snapshot_slot() -> &'static std::sync::Mutex<Option<MigrateJobSnapshot>> {
    static SLOT: std::sync::Mutex<Option<MigrateJobSnapshot>> = std::sync::Mutex::new(None);
    &SLOT
}

fn running_flag() -> &'static std::sync::atomic::AtomicBool {
    static FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    &FLAG
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 把进度回调写进快照（逐资产更新，长批次也能看到进展）。
fn update_progress(p: &svc_library::MigrateProgress) {
    if let Ok(mut g) = snapshot_slot().lock() {
        if let Some(s) = g.as_mut() {
            s.total = p.total;
            s.processed = p.processed;
            s.migrated = p.migrated;
            s.skipped = p.skipped;
            s.failed = p.failed;
            s.bytes = p.bytes;
        }
    }
}

/// 占用迁移槽：成功 → None；已有一批在跑 → Some(那批的 batchId)。
///
/// 单槽 + 单批并发是刻意的：一次迁移会写库根，两批并行既看不出进度也没意义。
pub fn migrate_slot_start(batch_id: &str, total: usize) -> Option<String> {
    use std::sync::atomic::Ordering;
    if running_flag()
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Some(
            snapshot_slot()
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|s| s.batch_id.clone()))
                .unwrap_or_default(),
        );
    }
    if let Ok(mut g) = snapshot_slot().lock() {
        *g = Some(MigrateJobSnapshot {
            batch_id: batch_id.to_string(),
            state: "running".into(),
            total,
            processed: 0,
            migrated: 0,
            skipped: 0,
            failed: 0,
            bytes: 0,
            started_at: now_secs(),
            finished_at: None,
            error: None,
            report: None,
        });
    }
    None
}

/// 收尾：写最终快照（done / failed）+ 释放槽。失败原因原样进 `error`。
pub fn migrate_slot_finish(result: std::result::Result<svc_library::MigrateReport, String>) {
    if let Ok(mut g) = snapshot_slot().lock() {
        if let Some(s) = g.as_mut() {
            s.finished_at = Some(now_secs());
            match result {
                Ok(rep) => {
                    s.state = "done".into();
                    s.total = rep.total;
                    s.migrated = rep.migrated;
                    s.skipped = rep.skipped.len();
                    s.failed = rep.failed.len();
                    s.processed = s.migrated + s.skipped + s.failed;
                    s.bytes = rep.bytes;
                    s.report = Some(rep);
                }
                Err(e) => {
                    s.state = "failed".into();
                    s.error = Some(e);
                }
            }
        }
    }
    running_flag().store(false, std::sync::atomic::Ordering::SeqCst);
}

/// 读快照：`batch_id` 给了且不匹配 → None（前端拿旧 batchId 轮询不该串台）。
pub fn migrate_slot_status(batch_id: Option<&str>) -> Option<MigrateJobSnapshot> {
    let snap = snapshot_slot().lock().ok().and_then(|g| g.clone())?;
    match batch_id {
        Some(want) if want != snap.batch_id => None,
        _ => Some(snap),
    }
}

/// 清空槽（诊断/测试用：上一次任务的状态不该污染下一次判断）。
pub fn migrate_slot_reset() {
    if let Ok(mut g) = snapshot_slot().lock() {
        *g = None;
    }
    running_flag().store(false, std::sync::atomic::Ordering::SeqCst);
}

/// `POST /api/library/migrate-to-library`：启动一次迁移（后台跑，立即返回 batchId）。
///
/// 请求：`{ ids: [i64], deleteSource?: false, batchId?: string, operator?: string }`
/// - `ids` 非空、≤900；`deleteSource` 默认 false（**源文件默认保留**）
/// - 立即返回 `202 { batchId, state:"running", total }`；进度/回执走 status 端点
/// - 同一时刻只允许一批（`409`），重复提交同一 batchId 不做特殊处理（续跑靠盘上事实判定）
#[derive(Deserialize)]
pub struct MigrateToLibraryReq {
    pub ids: Vec<i64>,
    #[serde(default)]
    pub deleteSource: Option<bool>,
    #[serde(default)]
    pub batchId: Option<String>,
    #[serde(default)]
    pub operator: Option<String>,
}

pub async fn migrate_to_library(
    State(state): State<AppState>,
    Json(req): Json<MigrateToLibraryReq>,
) -> impl IntoResponse {
    if req.ids.is_empty() {
        return err_coded(
            StatusCode::BAD_REQUEST,
            codes::LIBRARY_IDS_EMPTY,
            json!({}),
            "ids must not be empty",
        );
    }
    if req.ids.len() > svc_library::MIGRATE_MAX_IDS {
        return err(
            StatusCode::BAD_REQUEST,
            format!(
                "单批上限 {} 个资产，收到 {}（分批调用）",
                svc_library::MIGRATE_MAX_IDS,
                req.ids.len()
            ),
        );
    }
    if let Some(b) = &req.batchId {
        if b.trim().is_empty() {
            return err(StatusCode::BAD_REQUEST, "batchId 不能为空字符串");
        }
    }
    // 前置拦截（在任何磁盘副作用之前）：库根必须**实测可写**。
    // 与 /api/capabilities 同一套探针口径，但**不做跨设备拒绝** —— 迁移的正当用途就是
    // 把别的盘上的参考图收进库根，跨设备按「复制」处理（本卡产品定义）。
    {
        let conn = state.db.lock().await;
        let root = library_root(&conn, &state.data_dir);
        if let Err((code, raw)) = svc_library::capabilities::probe_writable(&root) {
            let text = humanize_fs_error(crate::is_desktop_form(), &code, &raw);
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": text, "code": code, "detail": raw })),
            )
                .into_response();
        }
        root
    };

    let batch_id = req
        .batchId
        .as_deref()
        .map(str::trim)
        .map(str::to_string)
        .unwrap_or_else(svc_library::new_migrate_batch_id);
    let total = req.ids.len();
    if let Some(cur) = migrate_slot_start(&batch_id, total) {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "已有迁移任务在跑，等它结束再提交", "batchId": cur })),
        )
            .into_response();
    }

    let st = state.clone();
    let ids = req.ids.clone();
    let mut opts = svc_library::MigrateOptions::default();
    opts.batch_id = Some(batch_id.clone());
    opts.operator = req.operator.clone().unwrap_or_default();
    opts.delete_source = req.deleteSource.unwrap_or(false);
    opts.progress = Some(std::sync::Arc::new(|p: &svc_library::MigrateProgress| {
        update_progress(p)
    }));
    tokio::task::spawn_blocking(move || {
        // 后台任务用自己的连接（与 import_api 同法）：不长时间占住 AppState 的锁
        let db_path = st.data_dir.join("dam.db");
        let result: std::result::Result<svc_library::MigrateReport, String> =
            match core_storage::open(&db_path) {
                Ok(mut conn) => {
                    let root = library_root(&conn, &st.data_dir);
                    svc_library::migrate_to_library(&mut conn, &ids, &root, &opts)
                        .map_err(|e| e.to_string())
                }
                Err(e) => Err(format!("open db: {e}")),
            };
        migrate_slot_finish(result);
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({ "batchId": batch_id, "state": "running", "total": total })),
    )
        .into_response()
}

/// `GET /api/library/migrate-to-library/status?batchId=` —— 进度快照。
///
/// 不带 batchId = 最近一次任务；没有任务 → `{state:"idle"}`；batchId 不匹配 → `{state:"idle"}`。
#[derive(Deserialize, Default)]
pub struct MigrateStatusQuery {
    #[serde(default)]
    #[allow(non_snake_case)]
    pub batchId: Option<String>,
}

pub async fn migrate_status(
    State(_state): State<AppState>,
    Query(q): Query<MigrateStatusQuery>,
) -> impl IntoResponse {
    let want = q
        .batchId
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match migrate_slot_status(want) {
        Some(s) => (StatusCode::OK, Json(json!(s))).into_response(),
        None => (StatusCode::OK, Json(json!({ "state": "idle" }))).into_response(),
    }
}

/// `GET /api/library/migrate-audit?batchId=&limit=` —— 迁移清单（审计证据 dump）。
#[derive(Deserialize, Default)]
pub struct MigrateAuditQuery {
    #[serde(default)]
    #[allow(non_snake_case)]
    pub batchId: Option<String>,
    pub limit: Option<u32>,
}

pub async fn migrate_audit(
    State(state): State<AppState>,
    Query(q): Query<MigrateAuditQuery>,
) -> impl IntoResponse {
    let conn = state.db.lock().await;
    match svc_library::migrate_audit_rows(
        &conn,
        q.batchId
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        q.limit.unwrap_or(500),
    ) {
        Ok(rows) => {
            let count = rows.len();
            (
                StatusCode::OK,
                Json(json!({ "rows": rows, "count": count })),
            )
                .into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
