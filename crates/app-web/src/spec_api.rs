//! N1 规格（`plugin_spec`）HTTP 面 —— 卡 task。
//!
//! 口径锚点：damdesk-product `plugin-design-and-ui.md` §2/§5/§9、`plugin-param-schema.md` §9。
//!
//! 规格 = 「要做到什么标准」的一份配置（选哪些规则 + 阈值 + 动作链 + 输出 + 参数 diff）。
//! 三条不变量（本模块负责守住）：
//! 1. **不安装、不分发**：规格没有安装包、没有版本栈、没有「规格市场」；同一个插件下可以有多条。
//! 2. **只存 diff**：参数默认值唯一定义在插件 `resources/params.schema.json`；本表 `paramsDiff`
//!    只存用户改过的键（插件升级默认值时不锁死旧规格）。
//! 3. **卸载/升级插件不动规格**：读接口把 `pluginStatus` 标成 `plugin_missing` /
//!    `version_mismatch`，规格行**照旧可见可编辑**（静默消失等于把用户配置吃掉）。
//!
//! 路由（nest 在 `/api/specs`）：
//! * `GET    /api/specs?plugin=<id>` —— 列规格（含 `pluginStatus`）；老库首次访问惰性补默认规格
//! * `POST   /api/specs`             —— 新建规格
//! * `GET    /api/specs/{id}`        —— 单条规格
//! * `PATCH  /api/specs/{id}`        —— 局部更新（只写给了的字段）
//! * `DELETE /api/specs/{id}`        —— 删除规格（只删配置行；verdict/产物/插件包一律不动）
//! * `POST   /api/specs/{id}/try`    —— 「先跑 1 张」：用这条规格的单资产试跑（走 judge）
//! * `POST   /api/specs/migrate`     —— 显式重跑旧行业包迁移并返回回读校验报告（幂等）

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::compliance_api::locale_from_headers;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::path::Path;

use core_storage::specs::{NewSpec, SpecPatch, SpecRecord, SpecRepo, SqliteSpecRepo};

use crate::{compliance_api, spec_seed, AppState};

fn err(status: StatusCode, code: &str, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": code, "message": msg.into() }))).into_response()
}

/// 能力身份解析结果（读规格时现算，不落库 —— 插件装了/卸了是环境事实，不是规格属性）。
struct PluginFacts {
    id: String,
    installed: bool,
    version: Option<String>,
    /// 插件是否声明了参数 schema（`manifest.params`）
    declared: bool,
    schema: Option<pack_host::ParamSchema>,
    /// 可用规则 id（包声明；未知 = 空 → 此时不校验规则名，但要标注「无法校验」）
    rules: Vec<String>,
    /// 能力名 + 一句话描述（DM-B1 / KI-36：**包自己给**，界面不硬编码映射）
    name: Option<String>,
    summary: Option<String>,
    note: Option<String>,
}

/// 解析某能力身份的现状：先按插件目录找，再回落到旧配置包。
///
/// * 插件在（包目录 + `manifest.id == 请求 id`）→ `installed`，版本/规则/schema 全部可读；
/// * 插件不在 → `installed=false` + `note`（**不报 404**：规格是用户资产，包不在也要看得见）。
fn plugin_facts(conn: &Connection, data_dir: &Path, plugin_id: &str, locale: &str) -> PluginFacts {
    // 1) 代码插件目录（四级解析：DAM_PACK_DIR → 账本 current → data_dir/packs → 仓库内置）
    if let Some(dir) = compliance_api::plugin_pack_dir_for(conn, data_dir, plugin_id) {
        if let Ok((manifest, ident)) = pack_host::load_pack(&dir) {
            if ident.id == plugin_id {
                let rules = pack_rules(&dir);
                let schema = manifest
                    .params
                    .as_deref()
                    .and_then(|rel| pack_host::ParamSchema::load(&dir, rel).ok());
                return PluginFacts {
                    id: ident.id,
                    installed: true,
                    version: Some(ident.version),
                    declared: manifest.params.is_some(),
                    schema,
                    rules,
                    // i18n-F：按请求语言取包自带名/描述（对象形态回退链在 LocalizedText::pick）
                    name: Some(manifest.short_label(locale).to_string()),
                    summary: manifest.summary_text(locale).map(str::to_string),
                    note: None,
                };
            }
        }
    }
    // 2) 没有配置包回落（IP-K16 删了内置/导入行业包）：包不在就是「插件没装」，
    //    规格照常看得见（用户资产），但 `installed=false` + 可操作提示。
    PluginFacts {
        id: plugin_id.to_string(),
        installed: false,
        version: None,
        declared: false,
        schema: None,
        rules: Vec::new(),
        name: None,
        summary: None,
        note: Some("这个能力还没装：方案保留可见，装好后再跑（卸载/升级不删方案）".to_string()),
    }
}

/// 包里声明的规则 id（`resources/rules.json` 的 `rules[].id`；读不到 = 空 = 无法校验）。
fn pack_rules(pack_dir: &Path) -> Vec<String> {
    std::fs::read_to_string(pack_dir.join("resources").join("rules.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get("rules").and_then(|r| r.as_array()).map(|a| {
                a.iter()
                    .filter_map(|r| r.get("id").and_then(|i| i.as_str()).map(str::to_string))
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// 规格视图（界面直接渲染；`pluginStatus` 是**现算**的，不落库）。
fn view(rec: &SpecRecord, facts: &PluginFacts) -> Value {
    json!({
        "specId": rec.spec_id,
        "pluginId": rec.plugin_id,
        "pluginVersionPin": rec.plugin_version_pin,
        "name": rec.name,
        "industry": rec.industry,
        "rules": rec.rules,
        "thresholds": rec.thresholds,
        "actions": rec.actions,
        "output": rec.output,
        "paramsDiff": rec.params_diff,
        "enabled": rec.enabled,
        "originKey": rec.origin_key,
        "createdAt": rec.created_at,
        "updatedAt": rec.updated_at,
        "pluginStatus": {
            "installed": facts.installed,
            // 能力名 + 一句话描述（DM-B1 / KI-36）：注册表里可能还没有这行（内置包首次跑之前
            // 不进账本），能力面板从规格行派生能力项时靠这两个字段显示中文名
            "pluginName": facts.name,
            "pluginSummary": facts.summary,
            "version": facts.version,
            "versionMismatch": facts
                .version
                .as_deref()
                .map(|v| rec.version_mismatch(v))
                .unwrap_or(false),
            "paramsSchemaDeclared": facts.declared,
            "rulesAvailable": facts.rules,
            "note": facts.note,
        },
    })
}

// ---------------------------------------------------------------------------
// 入参校验（宿主侧唯一一处；插件侧还会再校一次，协议铁律）
// ---------------------------------------------------------------------------

fn as_object(v: &Value, field: &str) -> Result<Map<String, Value>, String> {
    match v {
        Value::Object(m) => Ok(m.clone()),
        other => Err(format!("{field} 必须是对象，收到 {other}")),
    }
}

/// 校验一份规格的「可执行语义」：规则名、阈值结构、参数 diff、动作链、输出。
///
/// 为什么校验放在这里（而不是写库时随便存）：规格是**会被任务引用的配置**，
/// 存进去一条永远跑不通的规格 = 让错误延后到「500 张图跑完才发现参数名写错」。
fn validate(
    facts: &PluginFacts,
    name: &str,
    rules: &[String],
    thresholds: &Value,
    actions: &Value,
    output: &Value,
    params_diff: &Value,
) -> Result<(), (StatusCode, &'static str, String)> {
    let bad = |code: &'static str, msg: String| (StatusCode::BAD_REQUEST, code, msg);

    if name.trim().is_empty() {
        return Err(bad("spec_invalid_name", "规格名不能为空".into()));
    }
    if name.chars().count() > 64 {
        return Err(bad("spec_invalid_name", "规格名最长 64 字".into()));
    }
    // 规则：去重 + 必须在该能力声明的规则集里（包不在/读不到规则时不猜，只标注）
    let mut seen = std::collections::BTreeSet::new();
    for r in rules {
        if !seen.insert(r.as_str()) {
            return Err(bad("spec_duplicate_rule", format!("规则 {r} 重复声明")));
        }
        if !facts.rules.is_empty() && !facts.rules.iter().any(|x| x == r) {
            return Err(bad(
                "spec_unknown_rule",
                format!("{} 没有规则 {r}（可用：{:?}）", facts.id, facts.rules),
            ));
        }
    }
    // 阈值：`{ruleId: {key: number}}`
    let th = as_object(thresholds, "thresholds").map_err(|m| bad("spec_invalid_threshold", m))?;
    for (rule, params) in &th {
        if !facts.rules.is_empty() && !facts.rules.iter().any(|x| x == rule) {
            return Err(bad(
                "spec_unknown_rule",
                format!("阈值指向未知规则 {rule}（可用：{:?}）", facts.rules),
            ));
        }
        let Value::Object(kv) = params else {
            return Err(bad(
                "spec_invalid_threshold",
                format!("阈值 {rule} 必须是对象（{{\"min\": 700}}），收到 {params}"),
            ));
        };
        for (k, v) in kv {
            if !v.is_number() {
                return Err(bad(
                    "spec_invalid_threshold",
                    format!("阈值 {rule}.{k} 必须是数值，收到 {v}"),
                ));
            }
        }
    }
    // 动作链：有序数组，元素必须有 `action`
    let Value::Array(list) = actions else {
        return Err(bad(
            "spec_invalid_action",
            format!("actions 必须是数组，收到 {actions}"),
        ));
    };
    for (i, item) in list.iter().enumerate() {
        let Value::Object(m) = item else {
            return Err(bad(
                "spec_invalid_action",
                format!("actions[{i}] 必须是对象（{{\"action\": \"resize\"}}）"),
            ));
        };
        match m.get("action").and_then(|a| a.as_str()) {
            Some(a) if !a.trim().is_empty() => {}
            _ => {
                return Err(bad(
                    "spec_invalid_action",
                    format!("actions[{i}] 缺 `action`（动作名）"),
                ))
            }
        }
        for key in m.keys() {
            if !matches!(key.as_str(), "action" | "params" | "when") {
                return Err(bad(
                    "spec_invalid_action",
                    format!("actions[{i}] 有未知键 {key}（可用：action / params / when）"),
                ));
            }
        }
    }
    as_object(output, "output").map_err(|m| bad("spec_invalid_output", m))?;
    // 参数 diff：有 schema 就按 schema 校（错误码沿用 K07-I 的稳定集合）；没 schema 就不许存
    let diff = as_object(params_diff, "paramsDiff").map_err(|m| bad("params_type", m))?;
    match (&facts.schema, facts.declared, diff.is_empty()) {
        (Some(schema), _, _) => {
            schema
                .resolve_json(&[("paramsDiff", Some(params_diff))])
                .map_err(|e| (StatusCode::BAD_REQUEST, e.code(), e.to_string()))?;
        }
        (None, true, _) => {
            return Err(bad(
                "pack_params_schema",
                format!("{} 声明了参数 schema 但读不出来（包装坏了）", facts.id),
            ))
        }
        (None, false, false) => {
            return Err(bad(
                "spec_params_unsupported",
                format!(
                    "{} 没有声明参数 schema（老配置包）：规格不能存 paramsDiff，\
                     阈值请放 thresholds（规则级）",
                    facts.id
                ),
            ))
        }
        (None, false, true) => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 请求体
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateReq {
    pub plugin_id: String,
    pub name: String,
    #[serde(default)]
    pub plugin_version_pin: Option<String>,
    #[serde(default)]
    pub industry: Option<String>,
    #[serde(default)]
    pub rules: Vec<String>,
    #[serde(default = "empty_obj")]
    pub thresholds: Value,
    #[serde(default = "empty_arr")]
    pub actions: Value,
    #[serde(default = "empty_obj")]
    pub output: Value,
    #[serde(default = "empty_obj")]
    pub params_diff: Value,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn empty_obj() -> Value {
    json!({})
}
fn empty_arr() -> Value {
    json!([])
}
fn default_true() -> bool {
    true
}

/// PATCH：字段缺席 = 不动；`null` 只允许出现在可空列（industry / pluginVersionPin）。
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchReq {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub industry: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub plugin_version_pin: Option<Option<String>>,
    #[serde(default)]
    pub rules: Option<Vec<String>>,
    #[serde(default, deserialize_with = "double_option_value")]
    pub thresholds: Option<Option<Value>>,
    #[serde(default, deserialize_with = "double_option_value")]
    pub actions: Option<Option<Value>>,
    #[serde(default, deserialize_with = "double_option_value")]
    pub output: Option<Option<Value>>,
    #[serde(default, deserialize_with = "double_option_value")]
    pub params_diff: Option<Option<Value>>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

fn double_option<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<String>::deserialize(d)?))
}

fn double_option_value<'de, D>(d: D) -> Result<Option<Option<Value>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<Value>::deserialize(d)?))
}

#[derive(Debug, Deserialize)]
struct TryReq {
    #[serde(default)]
    asset_id: Option<i64>,
}

// ---------------------------------------------------------------------------
// 热路径：库连接 / 惰性迁移
// ---------------------------------------------------------------------------

fn db(state: &AppState) -> std::path::PathBuf {
    crate::infer::db_path(&state.data_dir)
}

/// 一次性迁移标记（settings 表）：**默认规格只补一次**。
///
/// 为什么需要它：只用「有没有规格行」当判据的话，用户把三条默认规格全删了（他有这个权利），
/// 下次打开列表又会冒出来 —— 那不是迁移，是把删除当成没发生过。
const SPECS_MIGRATED_KEY: &str = "compliance.specsMigrated";

/// 老库惰性补齐：**只在没迁过时**跑一次（迁过 = 零成本，一条 settings 读）。
///
/// 为什么不放在启动路径：迁移要读 `core_config` 包并写库，放在每次启动会拖慢冷启动，
/// 且桌面壳/容器/测试三条装配路径都得记得调。读接口首次访问时补，语义一样（幂等）。
fn ensure_seeded(conn: &Connection, data_dir: &Path) -> Result<(), String> {
    if let Ok(Some(true)) = core_storage::settings::get_json::<bool>(conn, SPECS_MIGRATED_KEY) {
        return Ok(());
    }
    // 首次访问：跑一遍旧行业包迁移（幂等；`originKey` 唯一索引兜底），然后落标记
    spec_seed::ensure_legacy_specs(conn, data_dir).map_err(|e| e.to_string())?;
    core_storage::settings::set_json(conn, SPECS_MIGRATED_KEY, &true).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ListQuery {
    #[serde(default)]
    plugin: Option<String>,
}

/// GET /api/specs?plugin=<id>
pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
    headers: HeaderMap,
) -> Response {
    let locale = crate::compliance_api::locale_from_headers(&headers);
    let data_dir = state.data_dir.clone();
    let task = tokio::task::spawn_blocking(move || -> Result<Value, (StatusCode, &'static str, String)> {
        let conn = core_storage::open(&crate::infer::db_path(&data_dir))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "db_open", e.to_string()))?;
        ensure_seeded(&conn, &data_dir).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "spec_seed", e))?;
        let repo = SqliteSpecRepo::new(&conn);
        let rows = repo
            .list(q.plugin.as_deref())
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "db", e.to_string()))?;
        let mut out = Vec::with_capacity(rows.len());
        for rec in &rows {
            let facts = plugin_facts(&conn, &data_dir, &rec.plugin_id, locale);
            out.push(view(rec, &facts));
        }
        Ok(json!({ "total": out.len(), "items": out }))
    })
    .await;
    match task {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)).into_response(),
        Ok(Err((code, ec, msg))) => err(code, ec, msg),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
    }
}

/// POST /api/specs
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateReq>,
) -> Response {
    let locale = crate::compliance_api::locale_from_headers(&headers);
    let data_dir = state.data_dir.clone();
    let outcome = tokio::task::spawn_blocking(move || -> Result<Value, (StatusCode, &'static str, String)> {
        let conn = core_storage::open(&crate::infer::db_path(&data_dir))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "db_open", e.to_string()))?;
        ensure_seeded(&conn, &data_dir).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "spec_seed", e))?;
        let facts = plugin_facts(&conn, &data_dir, &req.plugin_id, locale);
        validate(
            &facts,
            &req.name,
            &req.rules,
            &req.thresholds,
            &req.actions,
            &req.output,
            &req.params_diff,
        )?;
        let repo = SqliteSpecRepo::new(&conn);
        let new = NewSpec {
            plugin_id: req.plugin_id.clone(),
            plugin_version_pin: req.plugin_version_pin.clone(),
            name: req.name.trim().to_string(),
            industry: req.industry.clone(),
            rules: req.rules.clone(),
            thresholds: req.thresholds.clone(),
            actions: req.actions.clone(),
            output: req.output.clone(),
            params_diff: req.params_diff.clone(),
            enabled: req.enabled,
            origin_key: None, // 用户自建：不占迁移幂等键
        };
        let id = repo.create(&new).map_err(|e| match e {
            core_storage::StorageError::Invalid(msg) => (StatusCode::CONFLICT, "spec_duplicate_name", msg),
            other => (StatusCode::INTERNAL_SERVER_ERROR, "db", other.to_string()),
        })?;
        let rec = repo
            .get(id)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "db", e.to_string()))?
            .ok_or_else(|| (StatusCode::INTERNAL_SERVER_ERROR, "db", "回读不到新建规格".to_string()))?;
        Ok(view(&rec, &facts))
    })
    .await;
    match outcome {
        Ok(Ok(v)) => (StatusCode::CREATED, Json(v)).into_response(),
        Ok(Err((code, ec, msg))) => err(code, ec, msg),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
    }
}

/// GET /api/specs/{id}
pub async fn detail(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<i64>,
    headers: HeaderMap,
) -> Response {
    let locale = crate::compliance_api::locale_from_headers(&headers);
    let data_dir = state.data_dir.clone();
    let task = tokio::task::spawn_blocking(move || -> Result<Value, (StatusCode, &'static str, String)> {
        let conn = core_storage::open(&crate::infer::db_path(&data_dir))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "db_open", e.to_string()))?;
        ensure_seeded(&conn, &data_dir).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "spec_seed", e))?;
        let repo = SqliteSpecRepo::new(&conn);
        let rec = repo
            .get(id)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "db", e.to_string()))?
            .ok_or_else(|| (StatusCode::NOT_FOUND, "spec_not_found", format!("规格 #{id} 不存在")))?;
        let facts = plugin_facts(&conn, &data_dir, &rec.plugin_id, locale);
        Ok(view(&rec, &facts))
    })
    .await;
    match task {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)).into_response(),
        Ok(Err((code, ec, msg))) => err(code, ec, msg),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
    }
}

/// PATCH /api/specs/{id}
pub async fn update(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<i64>,
    headers: HeaderMap,
    Json(req): Json<PatchReq>,
) -> Response {
    let locale = crate::compliance_api::locale_from_headers(&headers);
    let data_dir = state.data_dir.clone();
    let task = tokio::task::spawn_blocking(move || -> Result<Value, (StatusCode, &'static str, String)> {
        let conn = core_storage::open(&crate::infer::db_path(&data_dir))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "db_open", e.to_string()))?;
        let repo = SqliteSpecRepo::new(&conn);
        let cur = repo
            .get(id)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "db", e.to_string()))?
            .ok_or_else(|| (StatusCode::NOT_FOUND, "spec_not_found", format!("规格 #{id} 不存在")))?;
        // null 只允许出现在可空列：其余列传 null 明确报错（别让「传 null」变成「静默忽略」）
        for (field, is_null) in [
            ("thresholds", matches!(&req.thresholds, Some(None))),
            ("actions", matches!(&req.actions, Some(None))),
            ("output", matches!(&req.output, Some(None))),
            ("paramsDiff", matches!(&req.params_diff, Some(None))),
        ] {
            if is_null {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "spec_null_field",
                    format!("{field} 不能是 null（要清空请传 {{}} / []）"),
                ));
            }
        }
        let facts = plugin_facts(&conn, &data_dir, &cur.plugin_id, locale);
        // 校验用「合并后的结果」：PATCH 只给一部分字段时，其余字段按现值参与校验
        let name = req.name.clone().unwrap_or_else(|| cur.name.clone());
        let rules = req.rules.clone().unwrap_or_else(|| cur.rules.clone());
        let thresholds = match &req.thresholds {
            Some(Some(v)) => v.clone(),
            _ => cur.thresholds.clone(),
        };
        let actions = match &req.actions {
            Some(Some(v)) => v.clone(),
            _ => cur.actions.clone(),
        };
        let output = match &req.output {
            Some(Some(v)) => v.clone(),
            _ => cur.output.clone(),
        };
        let params_diff = match &req.params_diff {
            Some(Some(v)) => v.clone(),
            _ => cur.params_diff.clone(),
        };
        validate(&facts, &name, &rules, &thresholds, &actions, &output, &params_diff)?;
        let patch = SpecPatch {
            name: req.name.as_ref().map(|n| n.trim().to_string()),
            industry: req.industry.clone(),
            plugin_version_pin: req.plugin_version_pin.clone(),
            rules: req.rules.clone(),
            thresholds: req.thresholds.clone().flatten(),
            actions: req.actions.clone().flatten(),
            output: req.output.clone().flatten(),
            params_diff: req.params_diff.clone().flatten(),
            enabled: req.enabled,
        };
        let hit = repo.update(id, &patch).map_err(|e| match e {
            core_storage::StorageError::Invalid(msg) => (StatusCode::CONFLICT, "spec_duplicate_name", msg),
            other => (StatusCode::INTERNAL_SERVER_ERROR, "db", other.to_string()),
        })?;
        if !hit {
            return Err((StatusCode::NOT_FOUND, "spec_not_found", format!("规格 #{id} 不存在")));
        }
        let rec = repo
            .get(id)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "db", e.to_string()))?
            .ok_or_else(|| (StatusCode::INTERNAL_SERVER_ERROR, "db", "回读不到规格".to_string()))?;
        Ok(view(&rec, &facts))
    })
    .await;
    match task {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)).into_response(),
        Ok(Err((code, ec, msg))) => err(code, ec, msg),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
    }
}

/// DELETE /api/specs/{id}
///
/// 只删规格行：历史 verdict、产物、插件包一律不动（设计硬边界 4）。
pub async fn remove(State(state): State<AppState>, AxumPath(id): AxumPath<i64>) -> Response {
    let data_dir = state.data_dir.clone();
    let task = tokio::task::spawn_blocking(move || -> Result<Value, (StatusCode, &'static str, String)> {
        let conn = core_storage::open(&crate::infer::db_path(&data_dir))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "db_open", e.to_string()))?;
        let repo = SqliteSpecRepo::new(&conn);
        let hit = repo
            .delete(id)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "db", e.to_string()))?;
        if !hit {
            return Err((StatusCode::NOT_FOUND, "spec_not_found", format!("规格 #{id} 不存在")));
        }
        Ok(json!({ "specId": id, "deleted": true }))
    })
    .await;
    match task {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)).into_response(),
        Ok(Err((code, ec, msg))) => err(code, ec, msg),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
    }
}

/// POST /api/specs/{id}/try —— 「先跑 1 张」。
///
/// 参数错了当场看见，而不是跑 500 张才发现（设计口径 §3 第 6 条）。
/// 语义：用这条规格（含其参数 diff/阈值/规则）对**一张图**起一次判定 run；
/// 不传 `assetId` 时取库里最新一张可用图（用户没概念「哪张」也不该被弹窗拦住）。
pub async fn try_one(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<i64>,
    Json(req): Json<TryReq>,
) -> Response {
    let data_dir = state.data_dir.clone();
    // 1) 规格自检（启用 / 可执行）
    let pre = {
        let data_dir = data_dir.clone();
        tokio::task::spawn_blocking(move || -> Result<i64, (StatusCode, &'static str, String)> {
            let conn = core_storage::open(&crate::infer::db_path(&data_dir))
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "db_open", e.to_string()))?;
            let repo = SqliteSpecRepo::new(&conn);
            let rec = repo
                .get(id)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "db", e.to_string()))?
                .ok_or_else(|| (StatusCode::NOT_FOUND, "spec_not_found", format!("规格 #{id} 不存在")))?;
            if !rec.enabled {
                return Err((
                    StatusCode::CONFLICT,
                    "spec_disabled",
                    format!("规格「{}」已停用：先启用再试跑", rec.name),
                ));
            }
            let asset_id = match req.asset_id {
                Some(a) => {
                    let ok: bool = conn
                        .query_row(
                            "SELECT 1 FROM assets WHERE assetId = ?1 AND deleted = 0 AND missing = 0",
                            rusqlite::params![a],
                            |r| r.get::<_, i64>(0),
                        )
                        .map(|v| v == 1)
                        .unwrap_or(false);
                    if !ok {
                        return Err((
                            StatusCode::NOT_FOUND,
                            "spec_asset_not_found",
                            format!("资产 #{a} 不存在 / 已删 / 已失联"),
                        ));
                    }
                    a
                }
                None => conn
                    .query_row(
                        "SELECT assetId FROM assets WHERE deleted = 0 AND missing = 0 \
                         AND mediaType = 'image' ORDER BY assetId DESC LIMIT 1",
                        [],
                        |r| r.get::<_, i64>(0),
                    )
                    .map_err(|_| {
                        (
                            StatusCode::CONFLICT,
                            "spec_no_asset",
                            "库里还没有可试跑的图片：先导入再试跑".to_string(),
                        )
                    })?,
            };
            Ok(asset_id)
        })
        .await
    };
    let asset_id = match pre {
        Ok(Ok(v)) => v,
        Ok(Err((code, ec, msg))) => return err(code, ec, msg),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
    };
    // 2) 交给 judge（同一套入队/参数合并/快照路径：试跑不是第二条执行链路）
    let judge_req = compliance_api::JudgeReq {
        assetIds: Some(vec![asset_id]),
        all: None,
        packId: None,
        scope: Some("asset".to_string()),
        limit: None,
        workers: Some(1),
        specOptions: None,
        specId: Some(id),
        options: None,
        actions: None,
    };
    compliance_api::judge(State(state), Json(judge_req)).await
}

/// POST /api/specs/migrate —— 显式重跑旧行业包迁移（幂等）+ 返回回读校验报告。
///
/// 为什么要有显式入口：自动补齐藏在读接口里（用户感知不到），排障/验收需要一个
/// 「现在到底迁了什么、是不是幂等、阈值有没有丢」的可点开的证据面。
pub async fn migrate(State(state): State<AppState>) -> Response {
    let data_dir = state.data_dir.clone();
    let task = tokio::task::spawn_blocking(move || -> Result<Value, (StatusCode, &'static str, String)> {
        let conn = core_storage::open(&crate::infer::db_path(&data_dir))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "db_open", e.to_string()))?;
        let report = spec_seed::ensure_legacy_specs(&conn, &data_dir)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "spec_seed", e))?;
        // 显式跑过迁移 = 也落一次标记（此后读接口不再惰性补，用户删掉的默认规格不复活）
        core_storage::settings::set_json(&conn, SPECS_MIGRATED_KEY, &true)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, "db", e.to_string()))?;
        Ok(json!({
            "created": report.created.iter().map(|o| json!({
                "specId": o.spec_id, "originKey": o.origin_key, "name": o.name,
                "pluginId": o.plugin_id, "rules": o.rules,
                "thresholds": o.thresholds, "paramsDiff": o.params_diff, "notes": o.notes,
            })).collect::<Vec<_>>(),
            "existing": report.existing.iter().map(|o| json!({
                "specId": o.spec_id, "originKey": o.origin_key, "name": o.name,
                "pluginId": o.plugin_id, "rules": o.rules, "notes": o.notes,
            })).collect::<Vec<_>>(),
            "verification": {
                "ok": report.verification.ok(),
                "specsTotal": report.verification.specs_total,
                "originUnique": report.verification.origin_unique,
                "thresholdsMatchSource": report.verification.thresholds_match_source,
                "rulesNonEmpty": report.verification.rules_non_empty,
                "namesNonEmpty": report.verification.names_non_empty,
                "actionsMatchSource": report.verification.actions_match_source,
                "detail": report.verification.detail,
            }
        }))
    })
    .await;
    match task {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)).into_response(),
        Ok(Err((code, ec, msg))) => err(code, ec, msg),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "join", e.to_string()),
    }
}

/// 规格路由（nest 在 `/api/specs`）。
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/migrate", post(migrate))
        .route("/{id}", get(detail).patch(update).delete(remove))
        .route("/{id}/try", post(try_one))
}
