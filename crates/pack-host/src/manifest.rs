//! `manifest.json` 解析与校验 —— 协议 §4.0 / §4.1 / §8。
//!
//! 三条硬规则（宿主拒装条件）：
//! - **未知键 = 拒装**（`deny_unknown_fields`）：写错键名却静默生效比报错更危险；
//! - **`api_version` 严格相等**：v1.x 的包装不进 v2.0 宿主（不做兼容层/shim，协议 §8）；
//! - **`pluginHash` = manifest 原文 sha256**：结果行落 `packId@version + pluginHash`（§7 铁律 7），
//!   所以 manifest 改一个字节就必须升 `version`。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::result::PluginOutput;

/// 协议版本：宿主按**严格相等**匹配（协议 §8）。
///
/// 2.0 = 契约改名落地版：manifest `kinds` → `outputs`（judge→verdict / process→artifact）、
/// RPC judge/repair 合并为单方法 `run`（先判后修 = 两次 run 各自 job，fix_policy 走 options）、
/// report 计数 `judged` → `rows`。v1.x 包一律拒装（错误信息带映射指路）。
pub const API_VERSION: &str = "2.0";

/// v1.x 契约映射指路（拒装报错必带，协议 §4.0.2/§8 的原话）。
pub const LEGACY_CONTRACT_HINT: &str = "kinds 已更名 outputs：judge→verdict / process→artifact（执行方法 judge/repair 已合并为 run）";

/// 能力白名单（协议 §6 网关 13 端点的去重能力集合）。
/// 新增能力必须先改协议文档 + `tools/pack-conformance/protocol.py` + 升 `api_version`。
/// 例外口径（N6c `t_6b2d45e4`）：`op.overlay` 是**纯增能力**（只多一个端点，已有端点语义
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
    /// 输出种类闭集 `["verdict"|"artifact"]`（协议 §4.1，v2.0 必填、显式、无缺省）：
    /// 宿主据此决定有没有审核列/审核动作、行里允许不允许 `verdict`，**不靠猜**。
    ///
    /// 空/重复/未知值/缺字段一律拒装；出现旧键 `kinds` 或 `api_version<2` → 拒装并带
    /// [`LEGACY_CONTRACT_HINT`] 指路（不做兼容读，协议 §8 一次迁移无 shim）。
    pub outputs: Vec<PluginOutput>,
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
    #[error("api_version={found:?}，宿主只接受 {expected:?}（严格相等，不兼容即拒装；旧包请按 v2.0 重新导出：{LEGACY_CONTRACT_HINT}")]
    IncompatibleApiVersion { found: String, expected: String },
    #[error("manifest 字段越界：{0}")]
    Invalid(String),
}

/// `outputs` 数组的手检（v2.0 必填）：非空、闭集、不重复。
///
/// 拒装理由必须能被人读懂 —— 「outputs 写了 `analyzer`」比 serde 的
/// ``unknown variant `analyzer` `` 有用得多。
fn validate_outputs_json(outputs: &serde_json::Value) -> Result<(), ManifestError> {
    let items = outputs.as_array().ok_or_else(|| {
        ManifestError::Invalid(
            "outputs 必须是非空数组（[\"verdict\"] / [\"artifact\"]，两者都可出的写两个）".into(),
        )
    })?;
    if items.is_empty() {
        return Err(ManifestError::Invalid(
            "outputs 不能为空数组（判定类写 [\"verdict\"]，处理类写 [\"artifact\"]，两者都可出的写两个）"
                .into(),
        ));
    }
    let mut seen: Vec<&str> = Vec::new();
    for item in items {
        let s = item.as_str().ok_or_else(|| {
            ManifestError::Invalid(format!(
                "outputs 每项必须是字符串（可用：{:?}），收到 {item}",
                PluginOutput::all()
            ))
        })?;
        if PluginOutput::parse(s).is_none() {
            return Err(ManifestError::Invalid(format!(
                "outputs 含未知值 {s:?}（可用：{:?}）",
                PluginOutput::all()
            )));
        }
        if seen.contains(&s) {
            return Err(ManifestError::Invalid(format!("outputs 重复声明 {s:?}")));
        }
        seen.push(s);
    }
    Ok(())
}

impl Manifest {
    /// 从字节解析 + 校验（`pack-host` 内部与安装期都走这一条）。
    pub fn from_slice(raw: &[u8]) -> Result<Self, ManifestError> {
        // v2.0 拒装面先于 serde：旧键 `kinds` / `api_version<2` 必须回「映射指路」，
        // 不能落进 serde 的 unknown field/缺字段报错（装包的人要一眼看懂怎么改，协议 §4.0.2/§8）。
        if let Ok(probe) = serde_json::from_slice::<serde_json::Value>(raw) {
            if probe.get("kinds").is_some() {
                return Err(ManifestError::Invalid(format!(
                    "manifest 用了旧键 kinds：{LEGACY_CONTRACT_HINT}（拒装）"
                )));
            }
            if let Some(outputs) = probe.get("outputs") {
                validate_outputs_json(outputs)?;
            }
            let found = probe.get("api_version").and_then(|v| v.as_str());
            if let Some(found) = found {
                if found != API_VERSION {
                    return Err(ManifestError::IncompatibleApiVersion {
                        found: found.to_string(),
                        expected: API_VERSION.to_string(),
                    });
                }
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
        // `outputs`：v2.0 必填（serde 缺字段已拒），这里兜住空/重复的语义面。
        if self.outputs.is_empty() {
            return Err(ManifestError::Invalid(
                "outputs 不能为空数组（判定类写 [\"verdict\"]，处理类写 [\"artifact\"]）".into(),
            ));
        }
        let mut seen: Vec<&str> = Vec::new();
        for o in &self.outputs {
            if seen.contains(&o.as_str()) {
                return Err(ManifestError::Invalid(format!(
                    "outputs 重复声明 {:?}",
                    o.as_str()
                )));
            }
            seen.push(o.as_str());
        }
        Ok(())
    }

    /// 生效的 `outputs`（v2.0 必填、无缺省 —— 运行期/落库都用它）。
    pub fn effective_outputs(&self) -> &[PluginOutput] {
        &self.outputs
    }

    /// 是否判定类（`verdict` 必填、审核列/审核动作存在）。
    pub fn has_verdict(&self) -> bool {
        self.outputs.contains(&PluginOutput::Verdict)
    }

    /// 是否处理类（只有执行结果与产物，返回 `verdict` 即拒收）。
    pub fn has_artifact(&self) -> bool {
        self.outputs.contains(&PluginOutput::Artifact)
    }

    /// 装机/登记用的 `outputs` 规范串（落 `packs.outputs`）：逗号分隔、固定顺序
    /// （verdict 在前、artifact 在后），闭集只有 `verdict` / `artifact` / `verdict,artifact` 三种。
    ///
    /// 为什么不存 JSON 数组：这一列是给「宿主该不该画审核列」判的，固定顺序 + 闭集
    /// 让 DB 用 CHECK 兜住拼写错误，读侧也不用解析 JSON、不用排序。
    pub fn outputs_canonical(&self) -> String {
        let mut parts: Vec<&'static str> = Vec::new();
        for o in self.effective_outputs() {
            let s = o.as_str();
            if !parts.contains(&s) {
                parts.push(s);
            }
        }
        // 固定顺序（verdict, artifact）：与 manifest 里声明顺序无关，落库值可比对
        parts.sort_by_key(|s| if *s == "verdict" { 0 } else { 1 });
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

/// 载入已装包目录，并把「**盘上 manifest 是某次 v1.x→v2.0 加载迁移的产物**」这个事实
/// 连同**迁移前**原文 sha256 一起带出来（无迁移 / 凭据与盘上内容对不上 → `None`）。
///
/// 存在理由（t_82dcef02）：迁移会改写盘上 manifest → `pluginHash` 变化，而账本
/// `packs.manifest_hash` 记的是**迁移前**装进去的那份原文。做「目录是否被改过」的对账时
/// 若直接拿迁移后 hash 比账本，必然失配（存量版本必 409 `pack_tampered`）。
///
/// 迁移凭据落在包目录（[`MIGRATION_PROVENANCE_FILE`]）而不是内存/DB：迁移可能发生在
/// 任意一次加载（GET detail / 判定 / activate），发起对账的调用多半**不是**迁移那次；
/// 只信「凭据里的迁移后 hash == 盘上现内容 hash」才算数 —— 迁移后又被改过 → 不认凭据 → 仍红。
pub fn load_pack_migrating(
    pack_dir: &Path,
) -> Result<(Manifest, PackIdentity, Option<String>), ManifestError> {
    migrate_installed_legacy(pack_dir)?;
    let (manifest, identity) = load_pack(pack_dir)?;
    let migrated_from = read_migration_provenance(pack_dir).and_then(|p| {
        if p.to_hash == identity.plugin_hash {
            Some(p.from_hash)
        } else {
            None
        }
    });
    Ok((manifest, identity, migrated_from))
}

/// 加载迁移凭据文件（与 `manifest.json` 同目录，宿主侧产物，不进归档）。
pub const MIGRATION_PROVENANCE_FILE: &str = "manifest.migrated.json";

/// 「这份 manifest 是迁移来的」的可审计凭据：迁移前后原文 sha256 + 时间。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct MigrationProvenance {
    schema: u32,
    from_hash: String,
    to_hash: String,
    migrated_at: u64,
}

fn read_migration_provenance(pack_dir: &Path) -> Option<MigrationProvenance> {
    let raw = std::fs::read(pack_dir.join(MIGRATION_PROVENANCE_FILE)).ok()?;
    let p: MigrationProvenance = serde_json::from_slice(&raw).ok()?;
    if p.schema != 1 || p.from_hash.is_empty() || p.to_hash.is_empty() {
        return None;
    }
    Some(p)
}

fn write_migration_provenance(pack_dir: &Path, from_hash: &str, to_hash: &str) {
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let p = MigrationProvenance {
        schema: 1,
        from_hash: from_hash.to_string(),
        to_hash: to_hash.to_string(),
        migrated_at: at,
    };
    let Ok(bytes) = serde_json::to_vec_pretty(&p) else {
        return;
    };
    let tmp = pack_dir.join(format!("{MIGRATION_PROVENANCE_FILE}.tmp"));
    if std::fs::write(&tmp, &bytes).is_ok() {
        let _ = std::fs::rename(&tmp, pack_dir.join(MIGRATION_PROVENANCE_FILE));
    }
}

/// 已装旧包（v1.x manifest 在磁盘上）的一次性加载迁移：把 `kinds` 改名为 `outputs`
/// （值映射 judge→verdict / process→artifact）并把 `api_version` 提到 "2.0"，原文件
/// 原子重写。v2.0 目录原样返回 `Ok(false)` —— **幂等**（跑两次结果一致，第二次是 no-op）。
///
/// 口径（协议 §8 一次迁移无 shim）：
/// - 只对**已安装**目录做（装机是过去式的事实，迁移让存量包继续可用）；
///   安装通道（`registry::install*` → [`Manifest::from_slice`]）永远拒收旧契约；
/// - 只改契约面：包代码若仍是 v1.x 词汇（只会答 judge/repair），run 时会 -32601 ——
///   那是包作者要重导出的信号，宿主不代改代码；
/// - `pluginHash` 随新原文变化：这是「迁移发生过」的可审计事实，装机记录在下一次
///   upsert 时以新 hash 刷新（历史判定行的 pluginHash 是历史事实，不回改）。
pub fn migrate_installed_legacy(pack_dir: &Path) -> Result<bool, ManifestError> {
    let path = pack_dir.join("manifest.json");
    let raw = std::fs::read(&path).map_err(|source| ManifestError::Io {
        path: path.clone(),
        source,
    })?;
    if Manifest::from_slice(&raw).is_ok() {
        return Ok(false); // 已是 v2.0：no-op（幂等的第二跑走这里）
    }
    let mut probe: serde_json::Value = serde_json::from_slice(&raw)
        .map_err(|e| ManifestError::Json(format!("存量 manifest 不是合法 JSON：{e}")))?;
    let legacy_kinds = probe.get("kinds").cloned();
    let legacy_api = probe
        .get("api_version")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if legacy_kinds.is_none() && legacy_api.as_str() == API_VERSION {
        return Err(ManifestError::Invalid(
            "manifest 不合法且非 v1.x 存量（不迁移，人工排查）".into(),
        ));
    }
    // kinds → outputs（闭集映射；写错值的存量包如实报错，不静默猜）
    if let Some(kinds) = legacy_kinds {
        let mut outputs: Vec<serde_json::Value> = Vec::new();
        match kinds.as_array() {
            Some(items) => {
                for item in items {
                    let s = item.as_str().unwrap_or("");
                    let mapped = match s {
                        "judge" => "verdict",
                        "process" => "artifact",
                        _ => {
                            return Err(ManifestError::Invalid(format!(
                                "存量 manifest 的 kinds 含未知值 {s:?}，无法迁移；{LEGACY_CONTRACT_HINT}"
                            )))
                        }
                    };
                    if !outputs
                        .iter()
                        .any(|v| v.as_str() == Some(mapped))
                    {
                        outputs.push(serde_json::json!(mapped));
                    }
                }
            }
            None => {
                return Err(ManifestError::Invalid(format!(
                    "存量 manifest 的 kinds 不是数组，无法迁移；{LEGACY_CONTRACT_HINT}"
                )))
            }
        }
        if outputs.is_empty() {
            return Err(ManifestError::Invalid(format!(
                "存量 manifest 的 kinds 为空，无法迁移；{LEGACY_CONTRACT_HINT}"
            )));
        }
        let obj = probe.as_object_mut().unwrap();
        obj.remove("kinds");
        obj.insert("outputs".into(), serde_json::json!(outputs));
    }
    // 未声明 kinds 的 v1.x 包 = 旧缺省（判定类），显式落 verdict（迁移后无缺省）
    if let Some(obj) = probe.as_object_mut() {
        obj.entry("outputs")
            .or_insert_with(|| serde_json::json!(["verdict"]));
        obj.insert("api_version".into(), serde_json::json!(API_VERSION));
    }
    let new_raw = serde_json::to_vec_pretty(&probe)
        .map_err(|e| ManifestError::Json(format!("迁移序列化失败：{e}")))?;
    let tmp = pack_dir.join("manifest.json.migrating");
    std::fs::write(&tmp, &new_raw).map_err(|source| ManifestError::Io {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, &path).map_err(|source| ManifestError::Io {
        path: path.clone(),
        source,
    })?;
    // 迁移凭据：`from_hash` 是账本对账的唯一合法依据（见 [`load_pack_migrating`]）
    write_migration_provenance(
        pack_dir,
        &core_hash::sha256_hex(&raw),
        &core_hash::sha256_hex(&new_raw),
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn good_json() -> serde_json::Value {
        serde_json::json!({
            "id": "com.soujpg.pack.ecommerce",
            "name": "电商主图合规",
            "version": "2.0.0",
            "api_version": "2.0",
            "outputs": ["verdict"],
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
        assert!(id.label().starts_with("com.soujpg.pack.ecommerce@2.0.0#"));
    }

    // ---- v2.0：manifest `outputs` 解析/校验 + 旧契约拒装指路 + 存量迁移 ----

    fn with_outputs(outputs: serde_json::Value) -> serde_json::Value {
        let mut v = good_json();
        v["outputs"] = outputs;
        v
    }

    fn legacy_json() -> serde_json::Value {
        // v1.2 形态（kinds + api_version<2）
        let mut v = serde_json::json!({
            "id": "com.soujpg.pack.legacy",
            "name": "存量旧包",
            "version": "1.2.0",
            "api_version": "1.1",
            "entrypoint": {"cmd": "python3", "args": ["plugin/__init__.py"]},
            "capabilities": ["io.read", "op.geometric", "vl.judge"],
            "deterministic": false,
            "timeout_secs": 60
        });
        v["kinds"] = serde_json::json!(["process"]);
        v
    }

    #[test]
    fn outputs_required_no_default() {
        // 未声明 outputs = 拒装（v2.0 无缺省，旧回落 ["judge"] 废止）
        let err = Manifest::from_slice(good_json_nov2outputs().to_string().as_bytes()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("outputs"), "{msg}");
    }

    fn good_json_nov2outputs() -> serde_json::Value {
        let mut v = good_json();
        v.as_object_mut().unwrap().remove("outputs");
        v
    }

    #[test]
    fn outputs_declared_are_parsed_in_declaration_order() {
        let m =
            Manifest::from_slice(with_outputs(json!(["artifact"])).to_string().as_bytes()).unwrap();
        assert_eq!(m.effective_outputs(), &[PluginOutput::Artifact]);
        assert!(m.has_artifact() && !m.has_verdict(), "处理类没有审核状态");

        let both = Manifest::from_slice(
            with_outputs(json!(["artifact", "verdict"]))
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            both.effective_outputs(),
            &[PluginOutput::Artifact, PluginOutput::Verdict],
            "声明序保留；落库规范串才固定排序（outputs_canonical）"
        );
        assert!(both.has_verdict() && both.has_artifact());
    }

    #[test]
    fn outputs_empty_unknown_duplicate_or_wrong_type_are_rejected() {
        for (label, outputs) in [
            ("空数组", json!([])),
            ("未知值", json!(["analyzer"])),
            ("重复", json!(["verdict", "verdict"])),
            ("非字符串", json!([1])),
            ("不是数组", json!("verdict")),
            ("大小写错", json!(["Verdict"])),
        ] {
            let err = Manifest::from_slice(
                with_outputs(outputs.clone()).to_string().as_bytes(),
            )
            .unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("outputs"),
                "{label}：错误信息必须点名 outputs —— {msg}"
            );
            assert!(
                msg.contains("verdict") || msg.contains("artifact"),
                "{label}：错误信息必须给出闭集 —— {msg}"
            );
        }
    }

    #[test]
    fn outputs_serialisation_roundtrips() {
        let m =
            Manifest::from_slice(with_outputs(json!(["artifact"])).to_string().as_bytes()).unwrap();
        let back = serde_json::to_value(&m).unwrap();
        assert_eq!(back["outputs"], json!(["artifact"]));
        let re = Manifest::from_slice(back.to_string().as_bytes()).unwrap();
        assert_eq!(re, m);
    }

    #[test]
    fn outputs_canonical_is_fixed_order() {
        let both = Manifest::from_slice(
            with_outputs(json!(["artifact", "verdict"]))
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(both.outputs_canonical(), "verdict,artifact");
    }

    #[test]
    fn legacy_kinds_manifest_is_rejected_with_mapping_hint() {
        let err = Manifest::from_slice(legacy_json().to_string().as_bytes()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("kinds 已更名 outputs"), "{msg}");
        assert!(msg.contains("judge→verdict") && msg.contains("process→artifact"), "{msg}");
    }

    #[test]
    fn legacy_api_version_is_rejected_with_mapping_hint() {
        let mut v = legacy_json();
        v.as_object_mut().unwrap().remove("kinds"); // 只旧版本号、无 kinds 也要指路
        let err = Manifest::from_slice(v.to_string().as_bytes()).unwrap_err();
        let msg = err.to_string();
        assert!(
            matches!(err, ManifestError::IncompatibleApiVersion { .. }),
            "{msg}"
        );
        assert!(msg.contains("kinds 已更名 outputs"), "{msg}");
    }

    #[test]
    fn installed_legacy_pack_migrates_once_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&legacy_json()).unwrap()).unwrap();

        // 第一次加载前迁移：改写成功
        assert!(super::migrate_installed_legacy(dir.path()).unwrap());
        let (m, id) = load_pack(dir.path()).unwrap();
        assert_eq!(m.api_version, "2.0");
        assert_eq!(m.effective_outputs(), &[PluginOutput::Artifact]);
        assert_eq!(m.outputs_canonical(), "artifact");
        assert_eq!(id.version, "1.2.0", "版本号是包身份，迁移不改");

        // 幂等：二次迁移是 no-op，加载结果一致
        assert!(!super::migrate_installed_legacy(dir.path()).unwrap());
        let (m2, id2) = load_pack(dir.path()).unwrap();
        assert_eq!(m2, m);
        assert_eq!(id2, id);

        // 未声明 kinds 的 v1.x 包 = 旧缺省判定类，迁移显式落 verdict
        let mut bare = legacy_json();
        bare.as_object_mut().unwrap().remove("kinds");
        let dir2 = tempfile::tempdir().unwrap();
        std::fs::write(
            dir2.path().join("manifest.json"),
            serde_json::to_vec_pretty(&bare).unwrap(),
        )
        .unwrap();
        assert!(super::migrate_installed_legacy(dir2.path()).unwrap());
        let (m3, _) = load_pack(dir2.path()).unwrap();
        assert_eq!(m3.effective_outputs(), &[PluginOutput::Verdict]);

        // 写错值的存量包如实报错，不静默猜
        let mut bad = legacy_json();
        bad["kinds"] = json!(["analyzer"]);
        let dir3 = tempfile::tempdir().unwrap();
        std::fs::write(
            dir3.path().join("manifest.json"),
            serde_json::to_vec_pretty(&bad).unwrap(),
        )
        .unwrap();
        let err = super::migrate_installed_legacy(dir3.path()).unwrap_err();
        assert!(err.to_string().contains("无法迁移"), "{err}");
    }

    /// t_82dcef02：迁移凭据要能跨调用证明「盘上这份 v2.0 manifest 是这次迁移的产物」，
    /// 且迁移后被改过就**不再采信**（否则等于给篡改开后门）。
    #[test]
    fn migration_provenance_survives_later_loads_and_dies_on_edit() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_raw = serde_json::to_vec_pretty(&legacy_json()).unwrap();
        std::fs::write(dir.path().join("manifest.json"), &legacy_raw).unwrap();
        let from_hash = core_hash::sha256_hex(&legacy_raw);

        // 第一次加载（迁移就发生在这里）：返回迁移前 hash
        let (_, ident, migrated_from) = super::load_pack_migrating(dir.path()).unwrap();
        assert_eq!(migrated_from.as_deref(), Some(from_hash.as_str()));
        assert_eq!(ident.plugin_hash.len(), 64);

        // 后续任何一次加载（迁移已是 no-op）仍能证明来源 —— 这是 activate 晚于
        // detail 加载时还能对账的前提
        let (_, ident2, migrated_from2) = super::load_pack_migrating(dir.path()).unwrap();
        assert_eq!(ident2, ident);
        assert_eq!(migrated_from2.as_deref(), Some(from_hash.as_str()));

        // 迁移后被改（仍是合法 v2.0）：凭据的 to_hash 对不上盘上内容 → 不采信
        let path = dir.path().join("manifest.json");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace("存量旧包", "被改过的包")).unwrap();
        let (_, ident3, migrated_from3) = super::load_pack_migrating(dir.path()).unwrap();
        assert_ne!(ident3.plugin_hash, ident.plugin_hash);
        assert!(migrated_from3.is_none(), "迁移后被动过就不能再认迁移凭据");

        // 从未迁移过的目录：无凭据
        let clean = tempfile::tempdir().unwrap();
        std::fs::write(
            clean.path().join("manifest.json"),
            good_json().to_string().as_bytes(),
        )
        .unwrap();
        let (_, _, none) = super::load_pack_migrating(clean.path()).unwrap();
        assert!(none.is_none());
    }
}
