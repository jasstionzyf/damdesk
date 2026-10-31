//! `manifest.json` 解析与校验 —— 协议 §4.0 / §4.1 / §8。
//!
//! 三条硬规则（宿主拒装条件）：
//! - **未知键 = 拒装**（`deny_unknown_fields`）：写错键名却静默生效比报错更危险；
//! - **`api_version` 严格相等**：`1.0` 的包装不进 `1.1` 宿主（不做兼容层，协议 §8）；
//! - **`pluginHash` = manifest 原文 sha256**：结果行落 `packId@version + pluginHash`（§7 铁律 7），
//!   所以 manifest 改一个字节就必须升 `version`。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::result::PluginKind;

/// 协议版本：宿主按**严格相等**匹配（协议 §8）。
///
/// 1.1 = 网关补 `/detect/bbox`（`detect.u2net`）+ `/detect/white_stats`（新能力 `detect.white`）。
pub const API_VERSION: &str = "1.1";

/// 能力白名单（协议 §6 网关 13 端点的去重能力集合）。
/// 新增能力必须先改协议文档 + `tools/pack-conformance/protocol.py` + 升 `api_version`。
/// 例外口径（N6c `task`）：`op.overlay` 是**纯增能力**（只多一个端点，已有端点语义
/// 一字未改），而 `api_version` 是**严格相等**匹配——升版本会让所有已装 1.1 包一次性失效。
/// 增能力的目的是让新包能用上，不是把老包挡在门外，所以按 2026-10-22 的 `instances` 先例：
/// 端点/能力纯增、`api_version` 保持 1.1，文档 §11 记账。
pub const KNOWN_CAPABILITIES: [&str; 9] = [
    "io.read",
    "detect.u2net",
    "detect.ocr",
    "detect.white",
    "op.geometric",
    "op.generative",
    "op.overlay",
    "vl.judge",
    "report.emit",
];

/// manifest 中的 `entrypoint`。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EntryPoint {
    /// 解释器名或路径。`python3` / `python` 由宿主替换为「包内 venv 解释器 → 内置 runtime」。
    pub cmd: String,
    /// 相对路径按包根目录解析。
    #[serde(default)]
    pub args: Vec<String>,
}

/// manifest 中的 `permissions`（缺省 `{read:true, net:false}`，协议 §4.0）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    #[serde(default = "default_true")]
    pub read: bool,
    #[serde(default)]
    pub net: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            read: true,
            net: false,
        }
    }
}

/// `manifest.json` —— 唯一配置面（宿主不读包内其他配置）。
///
/// `deny_unknown_fields`：出现协议未定义的键一律拒装，并在错误里点名该键。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    /// 完整展示名（i18n-F：字符串 = 全语言通用；对象 = {"locale": 文案}，见
    /// [`core_config::packs::LocalizedText`]）。协议不升 `api_version`：老包字符串原样装载。
    pub name: core_config::packs::LocalizedText,
    pub version: String,
    pub api_version: String,
    pub entrypoint: EntryPoint,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub deterministic: bool,
    pub timeout_secs: u64,
    /// 插件种类闭集 `["judge"|"process"]`（协议 §4.1，N5a）：宿主据此决定有没有审核列/
    /// 审核动作、要不要 `verdict`，**不靠猜**。
    ///
    /// 缺省（旧包）= 未声明 → [`Manifest::effective_kinds`] 回落 `["judge"]`
    /// （历史包装的都是判定类包；新包必须显式声明）。声明了就必须合法：
    /// 空数组/重复/未知值一律拒装。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<PluginKind>>,
    #[serde(default)]
    pub resources: Vec<String>,
    /// 参数 schema 的相对路径（相对包根，如 `resources/params.schema.json`）。
    ///
    /// 缺省 = 老包（宿主按协议 v1.1 的 `options` 自由透传，不校验、不填默认值）。
    /// 声明了就**安装期强制校验**（文件必须在归档里 + 能被 [`crate::params::ParamSchema`]
    /// 解析通过），坏 schema 不许落地 —— 否则界面拿到一份画不出控件的声明才发现装错了。
    #[serde(default)]
    pub params: Option<String>,
    /// **用户面短名**（DM-B1 / KI-36）：菜单、导航 tab、卸载确认这些窄位置用短名
    /// （§3 功能名，如「合规检查」）；`name` 保留完整展示名（可带副标题，详情页/管理面用）。
    ///
    /// 缺省 = 老包 → 短名回落到 [`Manifest::name`]（[`Manifest::short_label`]）。
    /// 与 `params` 同制：**v1.1 内追加的可选键，不升 `api_version`**，缺省行为不变。
    /// 加这个键是「展示元数据随包走」的落点 —— 界面不得再硬编码包名映射。
    /// i18n-F：值可本地化（字符串 = 全语言通用；对象 = {"locale": 文案}）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_name: Option<core_config::packs::LocalizedText>,
    /// 一句话「做什么 + 有什么用」（用户面描述，§3.1：每个功能入口都要有一句人话）。
    /// 缺省 = 未声明 → 界面不显示描述行（不编造）。i18n-F：同 `short_name` 可本地化。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<core_config::packs::LocalizedText>,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub signature: Option<String>,
}

impl Manifest {
    /// 界面上的能力名：`short_name`（去空白后非空）→ `name`。
    ///
    /// 空串按未声明处理（「声明了但是空的」不能变成一个空菜单项）。
    /// i18n-F：按请求 locale 取值（回退链 locale → zh-Hans → 任意 → 空串）。
    pub fn short_label(&self, locale: &str) -> &str {
        self.short_name
            .as_ref()
            .map(|t| t.pick(locale).trim())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.name.pick(locale).trim())
    }

    /// 一句话描述：`summary`（去空白后非空）→ `None`。i18n-F：按请求 locale 取值。
    pub fn summary_text(&self, locale: &str) -> Option<&str> {
        self.summary
            .as_ref()
            .map(|t| t.pick(locale).trim())
            .filter(|s| !s.is_empty())
    }

    /// 界面名对应的**原样 i18n 对象**（前端 `nameI18n` 透传；切换语言不用重拉接口）。
    /// 语义与 [`Manifest::short_label`] 同一来源：`short_name` 优先，回落 `name`；
    /// 字符串形态（全语言通用）→ `None`。
    pub fn label_i18n(&self) -> Option<&core_config::packs::LocalizedText> {
        let chosen = self.short_name.as_ref().unwrap_or(&self.name);
        chosen.as_map().map(|_| chosen)
    }

    /// 描述的原样 i18n 对象（同 [`Manifest::label_i18n`] 口径）。
    pub fn summary_i18n(&self) -> Option<&core_config::packs::LocalizedText> {
        let chosen = self.summary.as_ref()?;
        chosen.as_map().map(|_| chosen)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("读取 manifest 失败：{path}：{source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// JSON 解析/字段校验失败（未知键、缺字段、类型错都在这里）。
    /// serde 的消息里会点名具体键，例如 ``unknown field `fix_policy`, expected one of ...``。
    #[error("manifest.json 不合法（未知键/缺字段/类型错即拒装）：{0}")]
    Json(String),
    #[error("api_version={found:?}，宿主只接受 {expected:?}（严格相等，不兼容即拒装）")]
    IncompatibleApiVersion { found: String, expected: String },
    #[error("manifest 字段越界：{0}")]
    Invalid(String),
}

/// `kinds` 数组的手检（N5a）：非空、闭集、不重复。
///
/// 拒装理由必须能被人读懂 —— 「kinds 写了 `analyzer`」比 serde 的
/// ``unknown variant `analyzer` `` 有用得多。
fn validate_kinds_json(kinds: &serde_json::Value) -> Result<(), ManifestError> {
    let items = kinds.as_array().ok_or_else(|| {
        ManifestError::Invalid("kinds 必须是数组（[\"judge\"] / [\"process\"]）".into())
    })?;
    if items.is_empty() {
        return Err(ManifestError::Invalid(
            "kinds 不能为空数组（判定类写 [\"judge\"]，处理类写 [\"process\"]，两者都可写的写两个）"
                .into(),
        ));
    }
    let mut seen: Vec<&str> = Vec::new();
    for item in items {
        let s = item.as_str().ok_or_else(|| {
            ManifestError::Invalid(format!(
                "kinds 每项必须是字符串（可用：{:?}），收到 {item}",
                PluginKind::all()
            ))
        })?;
        if PluginKind::parse(s).is_none() {
            return Err(ManifestError::Invalid(format!(
                "kinds 含未知值 {s:?}（可用：{:?}）",
                PluginKind::all()
            )));
        }
        if seen.contains(&s) {
            return Err(ManifestError::Invalid(format!("kinds 重复声明 {s:?}")));
        }
        seen.push(s);
    }
    Ok(())
}

impl Manifest {
    /// 从字节解析 + 校验（`pack-host` 内部与安装期都走这一条）。
    pub fn from_slice(raw: &[u8]) -> Result<Self, ManifestError> {
        // `kinds` 先手检：serde 对 `Vec<enum>` 的 "unknown variant" 报错不点名键，
        // 而「kinds 写错」必须在装包期就被人一眼看懂（N5a）。
        if let Ok(probe) = serde_json::from_slice::<serde_json::Value>(raw) {
            if let Some(kinds) = probe.get("kinds") {
                validate_kinds_json(kinds)?;
            }
        }
        let manifest: Manifest =
            serde_json::from_slice(raw).map_err(|e| ManifestError::Json(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn from_path(path: &Path) -> Result<Self, ManifestError> {
        let raw = std::fs::read(path).map_err(|source| ManifestError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_slice(&raw)
    }

    /// 语义校验：`api_version` 严格相等、非空字段、能力白名单、超时区间。
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.api_version != API_VERSION {
            return Err(ManifestError::IncompatibleApiVersion {
                found: self.api_version.clone(),
                expected: API_VERSION.to_string(),
            });
        }
        for (name, value) in [
            ("id", &self.id),
            ("version", &self.version),
        ] {
            if value.trim().is_empty() {
                return Err(ManifestError::Invalid(format!("{name} 不能为空")));
            }
        }
        // i18n-F：name 可本地化 —— 至少要有一条非空文案（字符串形态 = 本身非空）
        if !self.name.has_text() {
            return Err(ManifestError::Invalid("name 不能为空".into()));
        }
        if self.entrypoint.cmd.trim().is_empty() {
            return Err(ManifestError::Invalid("entrypoint.cmd 不能为空".into()));
        }
        for arg in &self.entrypoint.args {
            if Path::new(arg).is_absolute() {
                return Err(ManifestError::Invalid(format!(
                    "entrypoint.args 必须是相对包根的路径：{arg}"
                )));
            }
        }
        if !(1..=3600).contains(&self.timeout_secs) {
            return Err(ManifestError::Invalid(format!(
                "timeout_secs={}，必须在 1..=3600",
                self.timeout_secs
            )));
        }
        if let Some(rel) = self.params.as_deref() {
            // 包内相对路径：绝对路径/`..` 一律拒（拼出去就成任意文件读取）
            if rel.trim().is_empty() {
                return Err(ManifestError::Invalid("params 不能是空串".into()));
            }
            if Path::new(rel).is_absolute() || rel.split(['/', '\\']).any(|c| c == "..") {
                return Err(ManifestError::Invalid(format!(
                    "params 必须是包内相对路径（不许绝对路径/..）：{rel}"
                )));
            }
        }
        let mut unknown: Vec<&str> = self
            .capabilities
            .iter()
            .map(String::as_str)
            .filter(|c| !KNOWN_CAPABILITIES.contains(c))
            .collect();
        unknown.sort_unstable();
        unknown.dedup();
        if !unknown.is_empty() {
            return Err(ManifestError::Invalid(format!(
                "未知能力 {unknown:?}（可用：{KNOWN_CAPABILITIES:?}）"
            )));
        }
        // `kinds`：声明了就必须合法（空/重复），未声明 = 旧包（回落 judge）。
        if let Some(kinds) = &self.kinds {
            if kinds.is_empty() {
                return Err(ManifestError::Invalid(
                    "kinds 不能为空数组（判定类写 [\"judge\"]，处理类写 [\"process\"]）".into(),
                ));
            }
            let mut seen: Vec<&str> = Vec::new();
            for k in kinds {
                if seen.contains(&k.as_str()) {
                    return Err(ManifestError::Invalid(format!(
                        "kinds 重复声明 {:?}",
                        k.as_str()
                    )));
                }
                seen.push(k.as_str());
            }
        }
        Ok(())
    }

    /// 生效的 `kinds`（运行期/落库都用它，别各处自己 `unwrap_or`）。
    ///
    /// 未声明 → `["judge"]`：历史包装进宿主的全是判定类包（`rules[]` 非空、
    /// `judge` 是必实现方法）；`kinds` 是 N5a 新增键，缺省必须等价于旧行为。
    pub fn effective_kinds(&self) -> Vec<PluginKind> {
        self.kinds
            .clone()
            .unwrap_or_else(|| vec![PluginKind::Judge])
    }

    /// 是否判定类（`verdict` 必填、审核列/审核动作存在）。
    pub fn is_judge(&self) -> bool {
        self.effective_kinds().contains(&PluginKind::Judge)
    }

    /// 是否处理类（只有执行结果与产物，返回 `verdict` 即拒收）。
    pub fn is_process(&self) -> bool {
        self.effective_kinds().contains(&PluginKind::Process)
    }

    /// 装机/登记用的 `kinds` 规范串（N5b，落 `packs.kinds`）：逗号分隔、固定顺序
    /// （judge 在前、process 在后），闭集只有 `judge` / `process` / `judge,process` 三种。
    ///
    /// 为什么不存 JSON 数组：这张列是给「宿主该不该画审核列」判的，固定顺序 + 闭集
    /// 让 DB 用 CHECK 兜住拼写错误，读侧也不用解析 JSON、不用排序。
    pub fn kinds_canonical(&self) -> String {
        let mut parts: Vec<&'static str> = Vec::new();
        for k in self.effective_kinds() {
            let s = k.as_str();
            if !parts.contains(&s) {
                parts.push(s);
            }
        }
        // 固定顺序（judge, process）：与 manifest 里声明顺序无关，落库值可比对
        parts.sort_by_key(|s| if *s == "judge" { 0 } else { 1 });
        parts.join(",")
    }

    /// 装机期校验：`resources[]` 必须存在（协议 §4.0）。返回缺失项（空 = 全部存在）。
    pub fn missing_resources(&self, pack_dir: &Path) -> Vec<String> {
        self.resources
            .iter()
            .filter(|rel| !pack_dir.join(rel).exists())
            .cloned()
            .collect()
    }

    /// 装机期校验：`entrypoint.args` 指向的包内文件必须存在（conformance 01 的对应项）。
    /// 返回缺失项（空 = 全部存在）。
    pub fn missing_entrypoint_files(&self, pack_dir: &Path) -> Vec<String> {
        self.entrypoint
            .args
            .iter()
            .filter(|rel| !pack_dir.join(rel).exists())
            .cloned()
            .collect()
    }
}

/// 结果行里必须落的身份三元组（§7 铁律 7）：`packId@version + pluginHash`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackIdentity {
    pub id: String,
    pub version: String,
    /// manifest.json 原文的 sha256（hex 小写）。
    pub plugin_hash: String,
}

impl PackIdentity {
    /// 稳定标签：`com.soujpg.pack.ecommerce@1.2.0#3f9a1c2d4e5f`。
    pub fn label(&self) -> String {
        let short = &self.plugin_hash[..self.plugin_hash.len().min(12)];
        format!("{}@{}#{}", self.id, self.version, short)
    }
}

/// 载入整包：读 `manifest.json`、校验、算 `pluginHash`。
///
/// 只读 manifest 原文，**不**执行包内任何代码（安装期验签/校验的入口）。
pub fn load_pack(pack_dir: &Path) -> Result<(Manifest, PackIdentity), ManifestError> {
    let path = pack_dir.join("manifest.json");
    let raw = std::fs::read(&path).map_err(|source| ManifestError::Io {
        path: path.clone(),
        source,
    })?;
    let manifest = Manifest::from_slice(&raw)?;
    let identity = PackIdentity {
        id: manifest.id.clone(),
        version: manifest.version.clone(),
        plugin_hash: core_hash::sha256_hex(&raw),
    };
    Ok((manifest, identity))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn good_json() -> serde_json::Value {
        serde_json::json!({
            "id": "com.soujpg.pack.ecommerce",
            "name": "电商主图合规",
            "version": "1.2.0",
            "api_version": "1.1",
            "entrypoint": {"cmd": "python3", "args": ["plugin/__init__.py"]},
            "capabilities": ["io.read", "op.geometric", "vl.judge"],
            "deterministic": false,
            "timeout_secs": 60
        })
    }

    #[test]
    fn parses_minimal_manifest_with_defaults() {
        let m = Manifest::from_slice(good_json().to_string().as_bytes()).unwrap();
        assert_eq!(m.id, "com.soujpg.pack.ecommerce");
        assert_eq!(
            m.permissions,
            Permissions {
                read: true,
                net: false
            }
        );
        assert!(m.signature.is_none());
        assert!(m.resources.is_empty());
    }

    #[test]
    fn rejects_unknown_key_and_names_it() {
        let mut v = good_json();
        v["fix_policy"] = serde_json::json!("generative");
        let err = Manifest::from_slice(v.to_string().as_bytes()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("fix_policy"), "{msg}");
        assert!(msg.contains("不合法"), "{msg}");
    }

    #[test]
    fn rejects_incompatible_api_version() {
        let mut v = good_json();
        v["api_version"] = serde_json::json!("1.0");
        let err = Manifest::from_slice(v.to_string().as_bytes()).unwrap_err();
        assert!(matches!(err, ManifestError::IncompatibleApiVersion { .. }));
        assert!(err.to_string().contains("1.0"));
    }

    #[test]
    fn rejects_unknown_capability() {
        let mut v = good_json();
        v["capabilities"] = serde_json::json!(["io.read", "op.telepathy"]);
        let err = Manifest::from_slice(v.to_string().as_bytes()).unwrap_err();
        assert!(err.to_string().contains("op.telepathy"), "{err}");
    }

    #[test]
    fn rejects_out_of_range_timeout() {
        for bad in [0, 3601] {
            let mut v = good_json();
            v["timeout_secs"] = serde_json::json!(bad);
            let err = Manifest::from_slice(v.to_string().as_bytes()).unwrap_err();
            assert!(err.to_string().contains("timeout_secs"), "{err}");
        }
    }

    #[test]
    fn identity_hash_is_over_raw_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let raw = serde_json::to_vec_pretty(&good_json()).unwrap();
        std::fs::write(dir.path().join("manifest.json"), &raw).unwrap();
        let (_, id) = load_pack(dir.path()).unwrap();
        assert_eq!(id.plugin_hash, core_hash::sha256_hex(&raw));
        assert_eq!(id.plugin_hash.len(), 64);
        assert!(id.label().starts_with("com.soujpg.pack.ecommerce@1.2.0#"));
    }

    // ---- N5a：manifest `kinds` 解析与校验 ----

    fn with_kinds(kinds: serde_json::Value) -> serde_json::Value {
        let mut v = good_json();
        v["kinds"] = kinds;
        v
    }

    #[test]
    fn kinds_absent_falls_back_to_judge_only() {
        let m = Manifest::from_slice(good_json().to_string().as_bytes()).unwrap();
        assert!(m.kinds.is_none());
        assert_eq!(m.effective_kinds(), vec![PluginKind::Judge]);
        assert!(m.is_judge() && !m.is_process());
    }

    #[test]
    fn kinds_declared_are_parsed_in_declaration_order() {
        let m =
            Manifest::from_slice(with_kinds(json!(["process"])).to_string().as_bytes()).unwrap();
        assert_eq!(m.effective_kinds(), vec![PluginKind::Process]);
        assert!(m.is_process() && !m.is_judge(), "处理类没有审核状态");

        let both = Manifest::from_slice(
            with_kinds(json!(["judge", "process"]))
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            both.effective_kinds(),
            vec![PluginKind::Judge, PluginKind::Process]
        );
        assert!(both.is_judge() && both.is_process());
    }

    #[test]
    fn kinds_empty_unknown_duplicate_or_wrong_type_are_rejected() {
        for (label, kinds) in [
            ("空数组", json!([])),
            ("未知值", json!(["analyzer"])),
            ("重复", json!(["judge", "judge"])),
            ("非字符串", json!([1])),
            ("不是数组", json!("judge")),
            ("大小写错", json!(["Judge"])),
        ] {
            let err =
                Manifest::from_slice(with_kinds(kinds.clone()).to_string().as_bytes()).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("kinds"),
                "{label}：错误信息必须点名 kinds —— {msg}"
            );
            assert!(
                msg.contains("judge") || msg.contains("process"),
                "{label}：错误信息必须给出闭集 —— {msg}"
            );
        }
    }

    #[test]
    fn kinds_serialisation_roundtrips_and_is_omitted_when_absent() {
        let json_absent =
            serde_json::to_value(Manifest::from_slice(good_json().to_string().as_bytes()).unwrap())
                .unwrap();
        assert!(json_absent.get("kinds").is_none(), "未声明就不写进回写体");

        let m =
            Manifest::from_slice(with_kinds(json!(["process"])).to_string().as_bytes()).unwrap();
        let back = serde_json::to_value(&m).unwrap();
        assert_eq!(back["kinds"], json!(["process"]));
        let re = Manifest::from_slice(back.to_string().as_bytes()).unwrap();
        assert_eq!(re, m);
    }
}
