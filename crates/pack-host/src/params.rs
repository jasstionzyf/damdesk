//! 插件**参数 schema**（K07-I）：参数由插件声明，宿主通用渲染 + 合并 + 校验。
//!
//! 设计口径：`~/.agent/profiles/soujpg/skills/damdesk/damdesk-product/references/plugin-param-schema.md`
//! （六条硬规则 + 运行时链路）。本模块只做宿主侧的三件事，**不碰子进程、不碰数据库**：
//!
//! 1. [`ParamSchema::parse`]：JSON Schema 子集解析与校验（未知键即拒 —— 与 `manifest.json`
//!    同一条纪律：写错键名却静默生效，比报错危险得多）；
//! 2. [`ParamSchema::resolve`]：`effective = schema.default ⊕ 规格 diff ⊕ 本任务覆盖`，
//!    然后剔掉 `when` 不成立的键、校 `required`/类型/范围/枚举。**返回全量生效值** ——
//!    发给插件的 `options` 就是它（插件不许依赖宿主默认值逻辑，否则独立性与可复现双双失守）；
//! 3. [`merge_layers`]：没声明 schema 的老包走这条（逐层原样合并，不校验）—— 协议 v1.1 的
//!    `options` 语义不变（仍由包解释），v2 只是把「谁声明默认值/范围」搬到了插件里。
//!
//! 三条明确的「不做」：
//! - **不做表达式/脚本**：`when` 只支持「另一个参数等于某值才显示」这一种；
//! - **不做 `pattern`**：v1 不引正则依赖（写 `pattern` = 未知键即拒装），要即时校验就用
//!   `min`/`max`/`step`/`options` 表达；
//! - **不让插件指定控件**：`type` 是数据语义，控件映射是宿主唯一一套。
//!
//! 宿主保留键（`RESERVED_KEYS`）不许插件声明：`workdir` 是协议 §3.4 的产物目录，
//! 由宿主每次调用注入并**覆盖**同名值；`job_id` 只出现在 JSON-RPC 的 `params` 顶层。

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::{Map, Value};

/// 唯一支持的 schema 版本（`schemaVersion`；递增即不兼容）。
pub const SCHEMA_VERSION: u32 = 1;

/// `manifest.json` 里声明参数 schema 的键（相对包根路径）。
pub const MANIFEST_KEY: &str = "params";

/// 宿主保留键：schema 声明它们 = 拒装（宿主注入，插件不许抢）。
pub const RESERVED_KEYS: [&str; 2] = ["workdir", "job_id"];

/// 参数类型（数据语义；控件由宿主按这张表映射，插件不能指定控件）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    /// 整数（控件：数字框；有 min+max+step → 滑杆 + 可手输）
    Integer,
    /// 浮点（同上）
    Number,
    /// 开关
    Boolean,
    /// 枚举（≤8 项分段按钮，>8 项下拉；取值必须落在 `options` 里）
    Enum,
    /// 单行输入
    String,
    /// 取色器 + hex（值形如 `#rrggbb`）
    Color,
    /// 文件选择：值 = 资产引用 id（**不存路径**）
    AssetRef,
    /// 双值滑杆（值 = `[min, max]` 两元素数组）
    Range,
}

impl ParamType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ParamType::Integer => "integer",
            ParamType::Number => "number",
            ParamType::Boolean => "boolean",
            ParamType::Enum => "enum",
            ParamType::String => "string",
            ParamType::Color => "color",
            ParamType::AssetRef => "asset_ref",
            ParamType::Range => "range",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "integer" => ParamType::Integer,
            "number" => ParamType::Number,
            "boolean" => ParamType::Boolean,
            "enum" => ParamType::Enum,
            "string" => ParamType::String,
            "color" => ParamType::Color,
            "asset_ref" => ParamType::AssetRef,
            "range" => ParamType::Range,
            _ => return None,
        })
    }

    /// 该类型是否接受 `min`/`max`/`step`（其余类型写了就是 schema 错，不静默忽略）。
    fn numeric(&self) -> bool {
        matches!(
            self,
            ParamType::Integer | ParamType::Number | ParamType::Range
        )
    }
}

/// 一个参数的声明（解析后 = 已校验）。
#[derive(Debug, Clone, PartialEq)]
pub struct ParamSpec {
    pub key: String,
    pub ty: ParamType,
    /// 控件标签（必填：界面不许出现无名参数）
    pub title: String,
    /// 缺省值（`None` = 没有默认值，`required` 时用户必须给）
    pub default: Option<Value>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub step: Option<f64>,
    /// `enum` 的可选值（其它类型必须为空）
    pub options: Vec<String>,
    /// 单位后缀（`%` / `px`；纯展示）
    pub unit: Option<String>,
    /// `asset_ref` 的可选 MIME（纯展示/选择器过滤）
    pub accept: Vec<String>,
    /// 分组键（`groups[].key`；`None` = 未分组）
    pub group: Option<String>,
    /// 归入「高级」折叠区（界面默认收起）
    pub advanced: bool,
    /// 条件显隐：`另一个参数 ∈ 给定值` 才显示这条（唯一支持的条件形式）
    pub when: BTreeMap<String, Vec<Value>>,
    /// 危险参数：改动不可逆（如重绘本体）→ 界面二次确认 + 写进 manifest 可审计
    pub irreversible: bool,
    /// 影响图片本体（→ 只能在工作版本上改，原图不动）
    pub affects_body: bool,
}

/// 分组声明（界面分区 + 高级折叠）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamGroup {
    pub key: String,
    pub title: String,
    pub collapsed: bool,
}

/// 一份已校验的参数 schema。
#[derive(Debug, Clone, PartialEq)]
pub struct ParamSchema {
    schema_version: u32,
    groups: Vec<ParamGroup>,
    properties: BTreeMap<String, ParamSpec>,
    required: Vec<String>,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum ParamError {
    /// schema 本身不合法（未知键/缺字段/类型错/语义矛盾）—— 安装期即拒装。
    #[error("参数 schema 不合法：{0}")]
    Schema(String),
    /// 调用方给的层里出现了 schema 未声明的键（拼错键名的静默代价 = 参数没生效还以为生效了）。
    #[error("{layer} 参数里有 schema 未声明的键：{key}")]
    UnknownKey { layer: String, key: String },
    #[error("参数 {key} 的类型错：要 {expected}，实际 {actual}")]
    Type {
        key: String,
        expected: String,
        actual: String,
    },
    #[error("参数 {key}={value} 越界：允许范围 {range}")]
    Range {
        key: String,
        value: f64,
        range: String,
    },
    #[error("参数 {key}={value:?} 不在可选值 {options:?} 里")]
    Enum {
        key: String,
        value: String,
        options: Vec<String>,
    },
    #[error("参数 {key} 是必填项，但既没有默认值也没给值")]
    Required { key: String },
    /// 层不是 JSON 对象（`null` 允许 = 空层；数组/标量一律拒）。
    #[error("{layer} 必须是 JSON 对象（收到 {actual}）")]
    Layer { layer: String, actual: String },
}

impl ParamError {
    /// 稳定错误码（HTTP 层透给前端；**别改名**，前端与排障脚本按它分类）。
    pub fn code(&self) -> &'static str {
        match self {
            ParamError::Schema(_) => "params_schema",
            ParamError::UnknownKey { .. } => "params_unknown_key",
            ParamError::Type { .. } => "params_type",
            ParamError::Range { .. } => "params_range",
            ParamError::Enum { .. } => "params_enum",
            ParamError::Required { .. } => "params_required",
            ParamError::Layer { .. } => "params_layer",
        }
    }
}

// ---------------------------------------------------------------------------
// 解析（serde 严格模式：未知键即错）
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSchema {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    #[serde(default)]
    groups: Vec<RawGroup>,
    properties: BTreeMap<String, RawSpec>,
    #[serde(default)]
    required: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGroup {
    key: String,
    title: String,
    #[serde(default)]
    collapsed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSpec {
    #[serde(rename = "type")]
    ty: String,
    title: String,
    #[serde(default)]
    default: Option<Value>,
    #[serde(default)]
    min: Option<f64>,
    #[serde(default)]
    max: Option<f64>,
    #[serde(default)]
    step: Option<f64>,
    #[serde(default)]
    options: Vec<String>,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    accept: Vec<String>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    advanced: bool,
    #[serde(default)]
    when: BTreeMap<String, Vec<Value>>,
    #[serde(default)]
    irreversible: bool,
    #[serde(rename = "affectsBody", default)]
    affects_body: bool,
}

impl ParamSchema {
    /// 从字节解析 + 校验（安装期与读取 schema 接口共用这一条）。
    pub fn parse(raw: &[u8]) -> Result<Self, ParamError> {
        let raw: RawSchema = serde_json::from_slice(raw).map_err(|e| {
            ParamError::Schema(format!("JSON 解析失败（未知键/缺字段/类型错即拒）：{e}"))
        })?;
        Self::from_raw(raw)
    }

    /// 从文件解析（`{pack_dir}/{rel}`）。
    pub fn load(pack_dir: &Path, rel: &str) -> Result<Self, ParamError> {
        let path = pack_dir.join(rel);
        let raw = std::fs::read(&path).map_err(|e| {
            ParamError::Schema(format!("读不到参数 schema {}：{e}", path.display()))
        })?;
        Self::parse(&raw)
    }

    /// 读包目录里的参数 schema（`manifest.params` 为空 = 老包，返回 `Ok(None)`）。
    pub fn load_declared(
        pack_dir: &Path,
        declared: Option<&str>,
    ) -> Result<Option<Self>, ParamError> {
        match declared {
            None => Ok(None),
            Some(rel) => Self::load(pack_dir, rel).map(Some),
        }
    }

    fn from_raw(raw: RawSchema) -> Result<Self, ParamError> {
        if raw.schema_version != SCHEMA_VERSION {
            return Err(ParamError::Schema(format!(
                "schemaVersion={}，宿主只接受 {SCHEMA_VERSION}（版本递增即不兼容，不做兼容层）",
                raw.schema_version
            )));
        }
        let mut groups = Vec::with_capacity(raw.groups.len());
        let mut seen_groups: Vec<&str> = Vec::new();
        for g in &raw.groups {
            if g.key.trim().is_empty() {
                return Err(ParamError::Schema("groups[].key 不能为空".into()));
            }
            if g.title.trim().is_empty() {
                return Err(ParamError::Schema(format!(
                    "分组 {} 的 title 不能为空（界面不许出现无名分区）",
                    g.key
                )));
            }
            if seen_groups.contains(&g.key.as_str()) {
                return Err(ParamError::Schema(format!("分组键重复：{}", g.key)));
            }
            seen_groups.push(&g.key);
            groups.push(ParamGroup {
                key: g.key.clone(),
                title: g.title.clone(),
                collapsed: g.collapsed,
            });
        }

        let mut properties = BTreeMap::new();
        for (key, spec) in raw.properties {
            let parsed = spec.into_spec(&key, &seen_groups)?;
            properties.insert(key, parsed);
        }
        if properties.is_empty() {
            return Err(ParamError::Schema(
                "properties 为空（声明了 schema 就至少要有一个参数；老包请别声明 manifest.params）"
                    .into(),
            ));
        }

        // `when` 引用的键必须存在（否则条件永远不成立 = 参数永远不显示，属静默失效）
        for (key, spec) in &properties {
            for dep in spec.when.keys() {
                if !properties.contains_key(dep) {
                    return Err(ParamError::Schema(format!(
                        "参数 {key} 的 when 引用了不存在的参数 {dep}"
                    )));
                }
                if dep == key {
                    return Err(ParamError::Schema(format!(
                        "参数 {key} 的 when 不能引用自己"
                    )));
                }
            }
        }

        let mut required = Vec::with_capacity(raw.required.len());
        for key in raw.required {
            if !properties.contains_key(&key) {
                return Err(ParamError::Schema(format!(
                    "required 里的 {key} 不在 properties 里"
                )));
            }
            if required.contains(&key) {
                continue;
            }
            required.push(key);
        }

        Ok(Self {
            schema_version: raw.schema_version,
            groups,
            properties,
            required,
        })
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn groups(&self) -> &[ParamGroup] {
        &self.groups
    }

    pub fn properties(&self) -> &BTreeMap<String, ParamSpec> {
        &self.properties
    }

    pub fn required(&self) -> &[String] {
        &self.required
    }

    /// 危险参数（`irreversible || affectsBody`）—— 界面据此加二次确认；宿主只透出判定。
    pub fn danger_keys(&self) -> Vec<&str> {
        self.properties
            .iter()
            .filter(|(_, s)| s.irreversible || s.affects_body)
            .map(|(k, _)| k.as_str())
            .collect()
    }

    /// 全部声明了默认值的键（合并的第一层）。
    pub fn defaults(&self) -> Map<String, Value> {
        self.properties
            .iter()
            .filter_map(|(k, s)| s.default.clone().map(|v| (k.clone(), v)))
            .collect()
    }

    /// 合并 + 校验：`effective = schema.default ⊕ 各层（按顺序覆盖）`。
    ///
    /// 顺序固定：默认值 → 规格 diff → 本任务覆盖（后写赢）。返回**全量生效值**，
    /// 且已剔掉 `when` 不成立的键（不成立的分支直接不传，插件别指望收到）。
    pub fn resolve(
        &self,
        layers: &[(&str, &Map<String, Value>)],
    ) -> Result<Map<String, Value>, ParamError> {
        let mut effective = self.defaults();
        for (name, layer) in layers {
            for (key, value) in layer.iter() {
                if !self.properties.contains_key(key) {
                    return Err(ParamError::UnknownKey {
                        layer: (*name).to_string(),
                        key: key.clone(),
                    });
                }
                effective.insert(key.clone(), value.clone());
            }
        }
        self.prune_invisible(&mut effective);
        self.check(&effective)?;
        Ok(effective)
    }

    /// 合并 + 校验（层用 JSON 值给：`null` = 空层，其余非对象一律拒）。
    pub fn resolve_json(
        &self,
        layers: &[(&str, Option<&Value>)],
    ) -> Result<Map<String, Value>, ParamError> {
        let mut typed: Vec<(&str, Map<String, Value>)> = Vec::with_capacity(layers.len());
        for (name, value) in layers {
            let map = layer_map(name, *value)?;
            typed.push((*name, map));
        }
        let refs: Vec<(&str, &Map<String, Value>)> = typed.iter().map(|(n, m)| (*n, m)).collect();
        self.resolve(&refs)
    }

    /// 剔掉 `when` 不成立的键（多轮收敛：剔掉一个可能连带影响另一个的可见性）。
    fn prune_invisible(&self, effective: &mut Map<String, Value>) {
        for _ in 0..(self.properties.len() + 1) {
            let mut drop_keys: Vec<String> = Vec::new();
            for (key, spec) in &self.properties {
                if spec.when.is_empty() || !effective.contains_key(key) {
                    continue;
                }
                if !self.when_holds(spec, effective) {
                    drop_keys.push(key.clone());
                }
            }
            if drop_keys.is_empty() {
                return;
            }
            for key in drop_keys {
                effective.remove(&key);
            }
        }
    }

    fn when_holds(&self, spec: &ParamSpec, effective: &Map<String, Value>) -> bool {
        spec.when.iter().all(|(dep, allowed)| {
            let Some(actual) = effective.get(dep) else {
                return false; // 依赖项没值 → 条件不成立（不显示）
            };
            allowed.iter().any(|want| values_equal(actual, want))
        })
    }

    /// 逐键校验（类型 / 范围 / 枚举）—— **只校已存在的键**；`required` 另算。
    pub fn check(&self, effective: &Map<String, Value>) -> Result<(), ParamError> {
        for key in &self.required {
            if !effective.contains_key(key) {
                return Err(ParamError::Required { key: key.clone() });
            }
        }
        for (key, value) in effective {
            let Some(spec) = self.properties.get(key) else {
                continue; // resolve 已拦；直接调 check 时对未知键不重复报
            };
            validate_value(spec, value)?;
        }
        Ok(())
    }
}

impl RawSchema {}

impl RawSpec {
    fn into_spec(self, key: &str, groups: &[&str]) -> Result<ParamSpec, ParamError> {
        if RESERVED_KEYS.contains(&key) {
            return Err(ParamError::Schema(format!(
                "参数键 {key} 是宿主保留键（{}），插件不许声明",
                RESERVED_KEYS.join(" / ")
            )));
        }
        let ty = ParamType::parse(&self.ty).ok_or_else(|| {
            ParamError::Schema(format!(
                "参数 {key} 的 type={:?} 不在白名单（integer/number/boolean/enum/string/color/asset_ref/range）",
                self.ty
            ))
        })?;
        if self.title.trim().is_empty() {
            return Err(ParamError::Schema(format!(
                "参数 {key} 缺 title（界面不许出现无名参数）"
            )));
        }
        if let Some(group) = self.group.as_deref() {
            if !groups.contains(&group) {
                return Err(ParamError::Schema(format!(
                    "参数 {key} 引用了未声明的分组 {group}"
                )));
            }
        }
        // 类型与元数据的一致性（写错就拒，不静默忽略）
        if ty == ParamType::Enum && self.options.is_empty() {
            return Err(ParamError::Schema(format!(
                "enum 参数 {key} 必须给 options（至少一个取值）"
            )));
        }
        if ty != ParamType::Enum && !self.options.is_empty() {
            return Err(ParamError::Schema(format!(
                "参数 {key} 是 {} 类型，不能写 options（枚举请用 type=enum）",
                ty.as_str()
            )));
        }
        if !ty.numeric() && (self.min.is_some() || self.max.is_some() || self.step.is_some()) {
            return Err(ParamError::Schema(format!(
                "参数 {key} 是 {} 类型，不能写 min/max/step（只有 integer/number/range 有数值区间）",
                ty.as_str()
            )));
        }
        if let (Some(min), Some(max)) = (self.min, self.max) {
            if min > max {
                return Err(ParamError::Schema(format!(
                    "参数 {key} 的 min({min}) > max({max})"
                )));
            }
        }
        if let Some(step) = self.step {
            if step <= 0.0 {
                return Err(ParamError::Schema(format!(
                    "参数 {key} 的 step={step} 必须 > 0"
                )));
            }
        }
        if !self.accept.is_empty() && ty != ParamType::AssetRef {
            return Err(ParamError::Schema(format!(
                "参数 {key} 是 {} 类型，不能写 accept（只有 asset_ref 有文件类型过滤）",
                ty.as_str()
            )));
        }
        let spec = ParamSpec {
            key: key.to_string(),
            ty,
            title: self.title,
            default: self.default,
            min: self.min,
            max: self.max,
            step: self.step,
            options: self.options,
            unit: self.unit,
            accept: self.accept,
            group: self.group,
            advanced: self.advanced,
            when: self.when,
            irreversible: self.irreversible,
            affects_body: self.affects_body,
        };
        if let Some(default) = &spec.default {
            validate_value(&spec, default)
                .map_err(|e| ParamError::Schema(format!("参数 {key} 的默认值不合法：{e}")))?;
        }
        Ok(spec)
    }
}

// ---------------------------------------------------------------------------
// 取值校验
// ---------------------------------------------------------------------------

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// 数值比较（`1` 与 `1.0` 视为相等；`when` 的取值匹配也走这条）。
fn values_equal(a: &Value, b: &Value) -> bool {
    match (as_f64(a), as_f64(b)) {
        (Some(x), Some(y)) => (x - y).abs() <= f64::EPSILON,
        _ => a == b,
    }
}

fn range_text(spec: &ParamSpec) -> String {
    match (spec.min, spec.max) {
        (Some(min), Some(max)) => format!("{min}..={max}"),
        (Some(min), None) => format!(">={min}"),
        (None, Some(max)) => format!("<={max}"),
        (None, None) => "（schema 未设区间）".to_string(),
    }
}

fn check_bounds(spec: &ParamSpec, value: f64) -> Result<(), ParamError> {
    let bad = spec.min.map(|m| value < m).unwrap_or(false)
        || spec.max.map(|m| value > m).unwrap_or(false);
    if bad {
        return Err(ParamError::Range {
            key: spec.key.clone(),
            value,
            range: range_text(spec),
        });
    }
    Ok(())
}

fn is_hex_color(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('#') else {
        return false;
    };
    matches!(rest.len(), 3 | 6 | 8) && rest.chars().all(|c| c.is_ascii_hexdigit())
}

/// 单个取值按声明校验（类型 + 范围 + 枚举/格式）。
pub fn validate_value(spec: &ParamSpec, value: &Value) -> Result<(), ParamError> {
    let type_err = |expected: &str| ParamError::Type {
        key: spec.key.clone(),
        expected: expected.to_string(),
        actual: type_name(value).to_string(),
    };
    match spec.ty {
        ParamType::Integer => {
            let Some(n) = as_f64(value) else {
                return Err(type_err("integer"));
            };
            if n.fract() != 0.0 {
                return Err(ParamError::Type {
                    key: spec.key.clone(),
                    expected: "integer（不允许小数）".into(),
                    actual: value.to_string(),
                });
            }
            check_bounds(spec, n)?;
        }
        ParamType::Number => {
            let Some(n) = as_f64(value) else {
                return Err(type_err("number"));
            };
            check_bounds(spec, n)?;
        }
        ParamType::Boolean => {
            if !value.is_boolean() {
                return Err(type_err("boolean"));
            }
        }
        ParamType::Enum => {
            let Some(s) = value.as_str() else {
                return Err(type_err("enum(字符串取值)"));
            };
            if !spec.options.iter().any(|o| o == s) {
                return Err(ParamError::Enum {
                    key: spec.key.clone(),
                    value: s.to_string(),
                    options: spec.options.clone(),
                });
            }
        }
        ParamType::String => {
            if !value.is_string() {
                return Err(type_err("string"));
            }
        }
        ParamType::Color => {
            let Some(s) = value.as_str() else {
                return Err(type_err("color(#rrggbb)"));
            };
            if !is_hex_color(s) {
                return Err(ParamError::Type {
                    key: spec.key.clone(),
                    expected: "color(#rgb / #rrggbb / #rrggbbaa)".into(),
                    actual: s.to_string(),
                });
            }
        }
        ParamType::AssetRef => {
            let Some(s) = value.as_str() else {
                return Err(type_err("asset_ref(资产引用字符串)"));
            };
            if s.trim().is_empty() {
                return Err(ParamError::Type {
                    key: spec.key.clone(),
                    expected: "asset_ref(非空资产引用)".into(),
                    actual: "空字符串".into(),
                });
            }
        }
        ParamType::Range => {
            let Some(arr) = value.as_array() else {
                return Err(type_err("range([min, max])"));
            };
            if arr.len() != 2 {
                return Err(ParamError::Type {
                    key: spec.key.clone(),
                    expected: "range([min, max] 恰好两个数)".into(),
                    actual: format!("{} 个元素", arr.len()),
                });
            }
            let (Some(lo), Some(hi)) = (as_f64(&arr[0]), as_f64(&arr[1])) else {
                return Err(type_err("range([min, max] 两个数)"));
            };
            check_bounds(spec, lo)?;
            check_bounds(spec, hi)?;
            if lo > hi {
                return Err(ParamError::Range {
                    key: spec.key.clone(),
                    value: lo,
                    range: format!("双值下限必须 <= 上限（收到 [{lo}, {hi}]）"),
                });
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 层（老包 / 无 schema 兼容路径）
// ---------------------------------------------------------------------------

/// 把一层 JSON 值转成键值表（`null` = 空层；非对象 = 参数错）。
pub fn layer_map(name: &str, value: Option<&Value>) -> Result<Map<String, Value>, ParamError> {
    match value {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(m)) => Ok(m.clone()),
        Some(other) => Err(ParamError::Layer {
            layer: name.to_string(),
            actual: type_name(other).to_string(),
        }),
    }
}

/// 老包（未声明 `manifest.params`）兼容路径：逐层原样合并，**不校验**。
///
/// 协议 v1.1 的 `options` 语义不变（由包解释）；宿主只保证「后层覆盖前层」，
/// 与有 schema 时的层序一致，插件换到 v2 声明时行为连续。
pub fn merge_layers(layers: &[(&str, Option<&Value>)]) -> Result<Map<String, Value>, ParamError> {
    let mut out = Map::new();
    for (name, value) in layers {
        for (k, v) in layer_map(name, *value)? {
            out.insert(k, v);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc() -> Value {
        json!({
            "schemaVersion": 1,
            "groups": [
                {"key": "basic", "title": "基本"},
                {"key": "adv", "title": "高级", "collapsed": true}
            ],
            "properties": {
                "format": {"type": "enum", "title": "输出格式",
                           "options": ["jpg", "png", "webp", "avif"], "default": "webp",
                           "group": "basic"},
                "quality": {"type": "integer", "title": "质量", "min": 1, "max": 100,
                            "default": 82, "unit": "%", "group": "basic",
                            "when": {"format": ["jpg", "webp", "avif"]}},
                "maxWidth": {"type": "integer", "title": "最大宽度", "min": 16, "max": 20000,
                             "default": 1600, "unit": "px", "group": "basic"},
                "keepRatio": {"type": "boolean", "title": "等比缩放", "default": true,
                              "group": "basic"},
                "watermark": {"type": "asset_ref", "title": "水印文件",
                              "accept": ["image/png"], "group": "adv"},
                "opacity": {"type": "number", "title": "不透明度", "min": 0, "max": 1,
                            "step": 0.05, "default": 1, "group": "adv", "advanced": true}
            },
            "required": ["format"]
        })
    }

    fn schema() -> ParamSchema {
        ParamSchema::parse(doc().to_string().as_bytes()).unwrap()
    }

    #[test]
    fn parses_strict_schema_and_exposes_metadata() {
        let s = schema();
        assert_eq!(s.schema_version(), 1);
        assert_eq!(s.groups().len(), 2);
        assert!(s.groups()[1].collapsed);
        assert_eq!(s.required(), ["format".to_string()]);
        assert!(s.properties().contains_key("quality"));
        assert_eq!(s.danger_keys(), Vec::<&str>::new());
        assert_eq!(s.defaults().len(), 5);
    }

    #[test]
    fn unknown_key_in_schema_is_rejected_and_named() {
        let mut v = doc();
        v["properties"]["quality"]["pattern"] = json!("^\\d+$"); // v1 不引正则
        let err = ParamSchema::parse(v.to_string().as_bytes()).unwrap_err();
        assert_eq!(err.code(), "params_schema");
        assert!(err.to_string().contains("pattern"), "{err}");

        let mut v2 = doc();
        v2["fixPolicy"] = json!("generative");
        let err2 = ParamSchema::parse(v2.to_string().as_bytes()).unwrap_err();
        assert!(err2.to_string().contains("fixPolicy"), "{err2}");
    }

    #[test]
    fn rejects_reserved_and_unknown_type_and_bad_metadata() {
        let mut v = doc();
        v["properties"]["workdir"] = json!({"type": "string", "title": "产物目录"});
        let err = ParamSchema::parse(v.to_string().as_bytes()).unwrap_err();
        assert!(err.to_string().contains("保留键"), "{err}");

        let mut v2 = doc();
        v2["properties"]["weird"] = json!({"type": "slider", "title": "怪类型"});
        let err2 = ParamSchema::parse(v2.to_string().as_bytes()).unwrap_err();
        assert!(err2.to_string().contains("slider"), "{err2}");

        let mut v3 = doc();
        v3["properties"]["opacity"]["options"] = json!(["a", "b"]); // number 写 options
        let err3 = ParamSchema::parse(v3.to_string().as_bytes()).unwrap_err();
        assert!(err3.to_string().contains("options"), "{err3}");

        let mut v4 = doc();
        v4["properties"]["keepRatio"]["min"] = json!(0); // boolean 写区间
        let err4 = ParamSchema::parse(v4.to_string().as_bytes()).unwrap_err();
        assert!(err4.to_string().contains("min"), "{err4}");

        let mut v5 = doc();
        v5["properties"]["quality"]["min"] = json!(100);
        v5["properties"]["quality"]["max"] = json!(1);
        let err5 = ParamSchema::parse(v5.to_string().as_bytes()).unwrap_err();
        assert!(err5.to_string().contains("min"), "{err5}");

        let mut v6 = doc();
        v6["properties"]["quality"]["default"] = json!(0); // 默认值越界
        let err6 = ParamSchema::parse(v6.to_string().as_bytes()).unwrap_err();
        assert!(err6.to_string().contains("默认值"), "{err6}");

        let mut v7 = doc();
        v7["required"] = json!(["nope"]);
        let err7 = ParamSchema::parse(v7.to_string().as_bytes()).unwrap_err();
        assert!(err7.to_string().contains("nope"), "{err7}");

        let mut v8 = doc();
        v8["properties"]["quality"]["when"] = json!({"ghost": ["x"]});
        let err8 = ParamSchema::parse(v8.to_string().as_bytes()).unwrap_err();
        assert!(err8.to_string().contains("ghost"), "{err8}");

        let mut v9 = doc();
        v9["schemaVersion"] = json!(2);
        let err9 = ParamSchema::parse(v9.to_string().as_bytes()).unwrap_err();
        assert!(err9.to_string().contains('2'), "{err9}");
    }

    #[test]
    fn resolve_applies_defaults_then_spec_then_task_override() {
        let s = schema();
        let spec = json!({"maxWidth": 2048});
        let task = json!({"maxWidth": 800, "quality": 60});
        let eff = s
            .resolve_json(&[("规格", Some(&spec)), ("任务", Some(&task))])
            .unwrap();
        // 默认值在，且后层覆盖前层（任务 > 规格 > 插件默认）
        assert_eq!(eff["format"], json!("webp"));
        assert_eq!(eff["maxWidth"], json!(800));
        assert_eq!(eff["quality"], json!(60));
        assert_eq!(eff["keepRatio"], json!(true));
        assert_eq!(eff["opacity"], json!(1));
        // 没有默认值的键不会凭空出现
        assert!(!eff.contains_key("watermark"));
    }

    #[test]
    fn when_false_branch_is_dropped_not_sent() {
        let s = schema();
        let task = json!({"format": "png", "quality": 50});
        let eff = s.resolve_json(&[("任务", Some(&task))]).unwrap();
        // format=png → quality 的 when 不成立 → 即使显式传了也不发（否则插件收到互相矛盾的参数）
        assert!(!eff.contains_key("quality"), "{eff:?}");
        assert_eq!(eff["format"], json!("png"));

        let eff2 = s
            .resolve_json(&[("任务", Some(&json!({"format": "jpg"})))])
            .unwrap();
        assert_eq!(eff2["quality"], json!(82), "when 成立 → 默认值生效");
    }

    #[test]
    fn unknown_key_and_type_and_range_errors_carry_stable_codes() {
        let s = schema();
        let err = s
            .resolve_json(&[("任务", Some(&json!({"qualty": 60})))])
            .unwrap_err();
        assert_eq!(err.code(), "params_unknown_key");
        assert!(err.to_string().contains("qualty"), "{err}");

        let err = s
            .resolve_json(&[("任务", Some(&json!({"maxWidth": "huge"})))])
            .unwrap_err();
        assert_eq!(err.code(), "params_type");

        let err = s
            .resolve_json(&[("任务", Some(&json!({"maxWidth": 40000})))])
            .unwrap_err();
        assert_eq!(err.code(), "params_range");
        assert!(err.to_string().contains("40000"), "{err}");

        let err = s
            .resolve_json(&[("任务", Some(&json!({"maxWidth": 12.5})))])
            .unwrap_err();
        assert_eq!(err.code(), "params_type", "整数不接受小数");

        let err = s
            .resolve_json(&[("任务", Some(&json!({"format": "bmp"})))])
            .unwrap_err();
        assert_eq!(err.code(), "params_enum");

        let err = s
            .resolve_json(&[("任务", Some(&json!({"opacity": 1.5})))])
            .unwrap_err();
        assert_eq!(err.code(), "params_range");

        let err = s
            .resolve_json(&[("任务", Some(&json!(["x"])))])
            .unwrap_err();
        assert_eq!(err.code(), "params_layer");

        let err = s
            .resolve_json(&[("任务", Some(&json!({"watermark": "  "})))])
            .unwrap_err();
        assert_eq!(err.code(), "params_type", "asset_ref 不许空串");
    }

    #[test]
    fn required_without_default_must_be_provided() {
        let mut v = doc();
        v["properties"]["watermark"] = json!({"type": "asset_ref", "title": "水印"});
        v["required"] = json!(["format", "watermark"]);
        let s = ParamSchema::parse(v.to_string().as_bytes()).unwrap();
        let err = s
            .resolve_json(&[("任务", Some(&empty_value()))])
            .unwrap_err();
        assert_eq!(err.code(), "params_required");
        assert!(err.to_string().contains("watermark"), "{err}");
        // 给了就过
        let ok = s
            .resolve_json(&[("任务", Some(&json!({"watermark": "asset-42"})))])
            .unwrap();
        assert_eq!(ok["watermark"], json!("asset-42"));
    }

    fn empty_value() -> Value {
        json!({})
    }

    #[test]
    fn color_and_range_types_are_checked() {
        let doc = json!({
            "schemaVersion": 1,
            "properties": {
                "tint": {"type": "color", "title": "描边色", "default": "#ffcc00"},
                "sandbox": {"type": "range", "title": "区段", "min": 0, "max": 100,
                            "default": [10, 20]}
            }
        });
        let s = ParamSchema::parse(doc.to_string().as_bytes()).unwrap();
        let eff = s.resolve_json(&[]).unwrap();
        assert_eq!(eff["tint"], json!("#ffcc00"));
        assert_eq!(eff["sandbox"], json!([10, 20]));

        assert_eq!(
            s.resolve_json(&[("任务", Some(&json!({"tint": "ffcc00"})))])
                .unwrap_err()
                .code(),
            "params_type"
        );
        assert_eq!(
            s.resolve_json(&[("任务", Some(&json!({"sandbox": [30]})))])
                .unwrap_err()
                .code(),
            "params_type"
        );
        assert_eq!(
            s.resolve_json(&[("任务", Some(&json!({"sandbox": [80, 20]})))])
                .unwrap_err()
                .code(),
            "params_range"
        );
    }

    #[test]
    fn danger_flags_are_surfaced() {
        let doc = json!({
            "schemaVersion": 1,
            "properties": {
                "mode": {"type": "enum", "title": "修复方式", "options": ["keep", "repaint"],
                         "default": "keep", "irreversible": true, "affectsBody": true}
            }
        });
        let s = ParamSchema::parse(doc.to_string().as_bytes()).unwrap();
        assert_eq!(s.danger_keys(), vec!["mode"]);
    }

    #[test]
    fn legacy_merge_is_verbatim_and_ordered() {
        let legacy = json!({"quality": 50});
        let task = json!({"quality": 60, "whatever": "x"});
        let merged = merge_layers(&[("规格", Some(&legacy)), ("任务", Some(&task))]).unwrap();
        assert_eq!(merged["quality"], json!(60));
        assert_eq!(merged["whatever"], json!("x"), "老包不校验、原样透传");
        assert!(merge_layers(&[("任务", Some(&json!("不是对象")))])
            .unwrap_err()
            .to_string()
            .contains("JSON 对象"));
    }

    #[test]
    fn load_declared_returns_none_for_legacy_pack() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            ParamSchema::load_declared(dir.path(), None).unwrap(),
            None,
            "manifest 没声明 params = 老包（兼容路径）"
        );
        std::fs::create_dir_all(dir.path().join("resources")).unwrap();
        std::fs::write(
            dir.path().join("resources").join("params.schema.json"),
            doc().to_string(),
        )
        .unwrap();
        let s = ParamSchema::load_declared(dir.path(), Some("resources/params.schema.json"))
            .unwrap()
            .unwrap();
        assert_eq!(s.properties().len(), 6);

        let err = ParamSchema::load_declared(dir.path(), Some("resources/nope.json")).unwrap_err();
        assert_eq!(err.code(), "params_schema");
        assert!(err.to_string().contains("nope.json"), "{err}");
    }
}
