//! 行业包**安装 / 验签 / 回滚**（IP-K08）—— `.dampack` 归档的文件系统层。
//!
//! 这一层只管磁盘与密码学，不碰数据库：目录布局 + 归档解包 + 摘要/签名校验 + 幂等复用，
//! 结果交给 L3（`app-web::compliance_api`）去写 `packs` 账本、切 `current` 指针。
//! 这么切是为了让「安装」可单测（临时目录即可，不需要 DB / HTTP / 子进程）。
//!
//! ## 目录布局（勿改）
//!
//! ```text
//! {data_dir}/dam/packs/<id>/<version>/     ← 一个已发布版本一个目录，装完永不就地修改
//! {data_dir}/dam/packs/<id>/<version>/manifest.json
//! ```
//!
//! 「升级 = 装新版本目录 + 切 current 指针」「回滚 = 切回指针」「卸载 = 删版本目录」——
//! 协议 §8 的硬规则；版本目录一经落地不允许被覆盖（同 `id@version` 内容变了 = 必须升 version，
//! 见 [`RegistryError::VersionConflict`]）。
//!
//! ## 校验顺序（外部契约，逐步短路，每步有自己的错误码）
//!
//! 1. `pack_archive`：zip 合法性 / 路径越界（zip-slip）/ 符号链接 / 解包体积上限；
//! 2. `pack_manifest` / `pack_api_version`：`manifest.json` 未知键、缺字段、`api_version` 严格相等；
//! 3. `pack_capability_missing`：包声明的能力必须都被本宿主（或部署方授权子集）支持；
//! 4. `pack_digest_mismatch`：归档若带 `SHA256SUMS`，逐文件核对 sha256（必须覆盖 `manifest.json`）；
//! 5. `pack_signature_required` / `pack_signature_invalid`：**分离签名** `manifest.sig`
//!    （对 `manifest.json` 原文的 ed25519 签名，base64）按信任公钥校验。
//!
//! 签名口径（为什么是分离签名）：`manifest.signature` 字段写在被签的字节里会自指，
//! 无法验证 —— 它只是「声明」（`signatureStatus=declared`）；只有 `manifest.sig` 才能验成
//! `verified`。宿主没配信任公钥时（开发/离线部署）不强制签名，包以 `unsigned` 装并在
//! 界面标明；配了信任公钥 = 强制验签（缺签直接拒装）。

use std::fs::File;
use std::io::{Cursor, Read, Seek, Write};
use std::path::{Component, Path, PathBuf};

pub use crate::manifest::{load_pack, Manifest, ManifestError, PackIdentity};

/// 归档内的清单文件名（唯一配置面，协议 §4.0）。
pub const MANIFEST_FILE: &str = "manifest.json";
/// 分离签名文件（base64 的 ed25519 签名，签的是 `manifest.json` 原文）。
pub const SIGNATURE_FILE: &str = "manifest.sig";
/// 逐文件摘要清单（`<sha256-hex>  <相对路径>`，`#` 开头为注释）。
pub const DIGESTS_FILE: &str = "SHA256SUMS";
/// 库根相对 `data_dir` 的子路径。
pub const PACKS_SUBDIR: &str = "dam/packs";
/// 解包体积上限（防 zip 炸弹；行业包是几十 MB 量级，512MiB 足够宽松）。
pub const MAX_UNPACKED_BYTES: u64 = 512 * 1024 * 1024;

/// 签名状态（与 migration 0026 的 `signature_status` CHECK 值一致；`invalid` 只出现在拒装错误里）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureStatus {
    /// 既无分离签名也无声明
    Unsigned,
    /// manifest 里带了 `signature` 字段但无法自证（未配信任公钥 / 自指字符串）
    Declared,
    /// `manifest.sig` 已用信任公钥验过
    Verified,
}

impl SignatureStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SignatureStatus::Unsigned => "unsigned",
            SignatureStatus::Declared => "declared",
            SignatureStatus::Verified => "verified",
        }
    }
}

/// 安装期签名策略：信任公钥为空 = 不强制签名（开发/离线），非空 = 必须带 `manifest.sig` 且验过。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignaturePolicy {
    /// 32 字节原始 ed25519 公钥
    pub trusted_keys: Vec<Vec<u8>>,
}

impl SignaturePolicy {
    /// 不验签（包以 `unsigned`/`declared` 落地）。
    pub fn permissive() -> Self {
        Self::default()
    }

    /// 从十六进制 / base64 字符串建策略；非法项报错（**不静默丢弃**：配错公钥等于没验签）。
    pub fn from_encoded(keys: &[String]) -> Result<Self, String> {
        let mut trusted = Vec::with_capacity(keys.len());
        for (i, k) in keys.iter().enumerate() {
            let bytes =
                parse_public_key(k).map_err(|e| format!("第 {} 个信任公钥不合法：{e}", i + 1))?;
            trusted.push(bytes);
        }
        Ok(Self {
            trusted_keys: trusted,
        })
    }

    /// 配了信任公钥 = 强制验签。
    pub fn requires_signature(&self) -> bool {
        !self.trusted_keys.is_empty()
    }
}

/// 解析 ed25519 公钥：十六进制（64 hex）或 base64（32 字节）。
pub fn parse_public_key(s: &str) -> Result<Vec<u8>, String> {
    let t = s.trim();
    if t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()) {
        let mut out = Vec::with_capacity(32);
        let bytes = t.as_bytes();
        for i in (0..64).step_by(2) {
            let hi = (bytes[i] as char).to_digit(16).unwrap() as u8;
            let lo = (bytes[i + 1] as char).to_digit(16).unwrap() as u8;
            out.push((hi << 4) | lo);
        }
        return Ok(out);
    }
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(t)
        .map_err(|e| format!("既不是 64 位十六进制也不是 base64：{e}"))?;
    if decoded.len() != 32 {
        return Err(format!(
            "ed25519 公钥必须是 32 字节，实际 {} 字节",
            decoded.len()
        ));
    }
    Ok(decoded)
}

/// 解码 HTTP 上传的归档字节（base64）。
///
/// 浏览器 `FileReader.readAsDataURL` 会带 `data:application/zip;base64,` 前缀，这里容错剥掉；
/// 其余一律按标准 base64 解。失败**明确报错**，不猜编码。
pub fn decode_base64_archive(s: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let t = s.trim();
    let body = t.rsplit_once("base64,").map(|(_, tail)| tail).unwrap_or(t);
    base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .map_err(|e| format!("archiveBase64 不是合法 base64：{e}"))
}

/// 本宿主编译期支持的能力（= 网关实际能放行的端点集合）。
///
/// 与网关一致：`detect.*` / `op.generative` 依赖 `local-infer`（ONNX 运行时），
/// web 容器形态编译时关掉它们 → 声明了这些能力的包在本形态**装不进来**（而不是装进来后
/// 每次调用回 403，让用户以为包坏了）。
pub fn default_host_capabilities() -> Vec<&'static str> {
    // `detect.white` 是纯 Rust 像素统计（engine-classic），不依赖 ONNX → 两种形态都支持
    let mut caps: Vec<&'static str> = vec![
        "io.read",
        "detect.white",
        "op.geometric",
        // N6c：文字水印是纯 Rust 渲染（core-media + ab_glyph + 内嵌字体），不依赖 ONNX →
        // 两种形态都支持（web 容器形态也能给包放行 `op.overlay`）。
        "op.overlay",
        "vl.judge",
        "report.emit",
    ];
    if cfg!(feature = "local-infer") {
        caps.push("detect.u2net");
        caps.push("detect.ocr");
        caps.push("op.generative");
    }
    caps.sort_unstable();
    caps
}

/// 包声明了但宿主不支持的能力（空 = 齐备）。
pub fn missing_capabilities(manifest: &Manifest, host: &[&str]) -> Vec<String> {
    let mut missing: Vec<String> = manifest
        .capabilities
        .iter()
        .filter(|c| !host.contains(&c.as_str()))
        .cloned()
        .collect();
    missing.sort_unstable();
    missing.dedup();
    missing
}

/// 安装结果。
#[derive(Debug, Clone, PartialEq)]
pub struct InstalledPack {
    pub manifest: Manifest,
    pub identity: PackIdentity,
    /// 版本目录绝对路径（`{data_dir}/dam/packs/<id>/<version>`）
    pub dir: PathBuf,
    pub signature_status: SignatureStatus,
    /// true = 同 `id@version` 同内容已装过（幂等，没有产生第二个副本目录）
    pub reused: bool,
    /// 归档里带 `SHA256SUMS` 且逐文件核对通过（无该文件 = false，表示只验了 manifest 身份）
    pub digests_checked: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("文件操作失败（{path}）：{source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// 归档本身不合法（坏 zip / 缺 manifest / 路径越界 / 符号链接 / 体积超限 / 摘要清单格式）。
    #[error("归档不合法：{0}")]
    Archive(String),
    #[error("{0}")]
    Manifest(#[from] ManifestError),
    #[error("包声明了本宿主不支持的能力 {missing:?}（本宿主支持：{host:?}）")]
    CapabilityMissing {
        missing: Vec<String>,
        host: Vec<String>,
    },
    #[error("sha256 不符：{path}（期望 {expected}，实际 {actual}）")]
    DigestMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("本宿主已启用验签（配置了信任公钥），包缺分离签名 {SIGNATURE_FILE}")]
    SignatureRequired,
    #[error("签名校验失败：{0}")]
    SignatureInvalid(String),
    /// K07-I：manifest 声明了 `params`，但归档里没有这个文件。
    #[error("manifest.params 指向的文件不在归档里：{0}")]
    ParamsMissing(String),
    /// K07-I：参数 schema 解析/校验不通过（未知键、类型错、默认值越界……）→ 拒装。
    #[error("参数 schema 不合法（{0}）")]
    ParamsSchema(String),
    /// 同 `id@version` 已装但内容不同（协议 §7 铁律 7：改内容必须升 version）。
    #[error("版本冲突：{id}@{version} 已装且内容不同（已装 {installed}，本次 {incoming}）——改内容必须升 version")]
    VersionConflict {
        id: String,
        version: String,
        installed: String,
        incoming: String,
    },
}

impl RegistryError {
    /// 稳定错误码（HTTP 层直接透给前端；**别改名**，前端与报告按它分类）。
    pub fn code(&self) -> &'static str {
        match self {
            RegistryError::Io { .. } => "io",
            RegistryError::Archive(_) => "pack_archive",
            RegistryError::Manifest(ManifestError::IncompatibleApiVersion { .. }) => {
                "pack_api_version"
            }
            RegistryError::Manifest(_) => "pack_manifest",
            RegistryError::CapabilityMissing { .. } => "pack_capability_missing",
            RegistryError::DigestMismatch { .. } => "pack_digest_mismatch",
            RegistryError::SignatureRequired => "pack_signature_required",
            RegistryError::SignatureInvalid(_) => "pack_signature_invalid",
            RegistryError::ParamsMissing(_) => "pack_params_missing",
            RegistryError::ParamsSchema(_) => "pack_params_schema",
            RegistryError::VersionConflict { .. } => "pack_version_conflict",
        }
    }
}

fn io_err(path: &Path, source: std::io::Error) -> RegistryError {
    RegistryError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// 版本号比较（点分数字优先，非数字段退回字典序；`1.10.0 > 1.9.0`）。
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let split = |s: &str| -> Vec<String> { s.split(['.', '-', '+']).map(str::to_string).collect() };
    let (av, bv) = (split(a), split(b));
    for i in 0..av.len().max(bv.len()) {
        let (x, y) = (av.get(i), bv.get(i));
        let ord = match (x, y) {
            (Some(x), Some(y)) => match (x.parse::<u64>(), y.parse::<u64>()) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                _ => x.cmp(y),
            },
            (Some(_), None) => Ordering::Greater,
            (None, Some(_)) => Ordering::Less,
            (None, None) => Ordering::Equal,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// 版本目录名的安全校验：`id`/`version` 直接进路径，必须拒绝分隔符与 `..`。
fn safe_component(s: &str) -> bool {
    !s.is_empty()
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains('\0')
        && s != "."
        && s != ".."
        && !s.starts_with(' ')
}

/// 包库（`{data_dir}/dam/packs`）。
#[derive(Debug, Clone)]
pub struct PackStore {
    root: PathBuf,
}

impl PackStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join("dam").join("packs"),
        }
    }

    /// 库根（`{data_dir}/dam/packs`）。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 版本目录 `{root}/<id>/<version>`。
    pub fn version_dir(&self, id: &str, version: &str) -> PathBuf {
        self.root.join(id).join(version)
    }

    /// 已装版本（只列**目录存在**的版本，按版本序升序）。
    ///
    /// 「目录 = 真相源」：卸载就是删目录，账本行只是审计记录（0026：`installed_at` 是
    /// 「何时进过这个库」），所以已卸载的版本不会在列表里假存在。
    pub fn installed_versions(&self, id: &str) -> Vec<String> {
        let dir = self.root.join(id);
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| !n.starts_with('.'))
            .collect();
        out.sort_by(|a, b| compare_versions(a, b));
        out
    }

    /// 库里所有包 id（目录存在且含 manifest.json 的）。
    pub fn installed_ids(&self) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut out: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| !n.starts_with('.'))
            .collect();
        out.sort();
        out
    }

    /// 删掉一个版本目录（不存在 → `Ok(false)`）。父目录空了顺手删掉，保持库根干净。
    pub fn uninstall_version(&self, id: &str, version: &str) -> Result<bool, RegistryError> {
        if !safe_component(id) || !safe_component(version) {
            return Err(RegistryError::Archive(format!(
                "id/version 含非法路径字符：{id}@{version}"
            )));
        }
        let dir = self.version_dir(id, version);
        if !dir.is_dir() {
            return Ok(false);
        }
        std::fs::remove_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
        // 该 id 下已无版本 → 删空父目录（失败不影响结果）
        if self.installed_versions(id).is_empty() {
            let _ = std::fs::remove_dir(self.root.join(id));
        }
        Ok(true)
    }

    /// 从 `.dampack` 文件安装。
    pub fn install(
        &self,
        archive_path: &Path,
        policy: &SignaturePolicy,
        host_caps: &[&str],
    ) -> Result<InstalledPack, RegistryError> {
        let file = File::open(archive_path).map_err(|e| io_err(archive_path, e))?;
        self.install_reader(file, policy, host_caps)
    }

    /// 从内存字节安装（HTTP 上传走这条）。
    pub fn install_bytes(
        &self,
        bytes: &[u8],
        policy: &SignaturePolicy,
        host_caps: &[&str],
    ) -> Result<InstalledPack, RegistryError> {
        self.install_reader(Cursor::new(bytes), policy, host_caps)
    }

    fn install_reader<R: Read + Seek>(
        &self,
        reader: R,
        policy: &SignaturePolicy,
        host_caps: &[&str],
    ) -> Result<InstalledPack, RegistryError> {
        let mut zip = zip::ZipArchive::new(reader)
            .map_err(|e| RegistryError::Archive(format!("不是合法 zip：{e}")))?;

        // ---- 1. manifest（唯一配置面）
        let raw_manifest = read_optional(&mut zip, MANIFEST_FILE)?.ok_or_else(|| {
            RegistryError::Archive(format!(
                "归档根目录缺 {MANIFEST_FILE}（不要把包再套一层目录）"
            ))
        })?;
        let manifest = Manifest::from_slice(&raw_manifest)?; // api_version 严格相等在这里
        if !safe_component(&manifest.id) || !safe_component(&manifest.version) {
            return Err(RegistryError::Archive(format!(
                "manifest 的 id/version 含非法路径字符：{}@{}",
                manifest.id, manifest.version
            )));
        }
        let identity = PackIdentity {
            id: manifest.id.clone(),
            version: manifest.version.clone(),
            plugin_hash: core_hash::sha256_hex(&raw_manifest),
        };

        // ---- 2. 能力齐备（声明 ≠ 授权，协议 §8）
        let missing = missing_capabilities(&manifest, host_caps);
        if !missing.is_empty() {
            return Err(RegistryError::CapabilityMissing {
                missing,
                host: host_caps.iter().map(|c| c.to_string()).collect(),
            });
        }
        // 装机期还要确认 resources / entrypoint.args 指向的文件真的在（协议 §4.0）——
        // 在解包后按真实目录复检（见 extract 之后那段）。

        // ---- 3. sha256 逐文件核对（归档自带 SHA256SUMS 时）
        let digests_checked = match read_optional(&mut zip, DIGESTS_FILE)? {
            Some(raw) => {
                let sums = parse_sums(&String::from_utf8_lossy(&raw))?;
                if !sums.iter().any(|(p, _)| p == MANIFEST_FILE) {
                    return Err(RegistryError::Archive(format!(
                        "{DIGESTS_FILE} 未覆盖 {MANIFEST_FILE}（清单不完整，拒绝安装）"
                    )));
                }
                for (path, expected) in &sums {
                    let data = read_optional(&mut zip, path)?.ok_or_else(|| {
                        RegistryError::Archive(format!(
                            "{DIGESTS_FILE} 列出的文件在归档里不存在：{path}"
                        ))
                    })?;
                    let actual = core_hash::sha256_hex(&data);
                    if &actual != expected {
                        return Err(RegistryError::DigestMismatch {
                            path: path.clone(),
                            expected: expected.clone(),
                            actual,
                        });
                    }
                }
                true
            }
            None => false,
        };

        // ---- 4. 签名（分离签名才能验；manifest.signature 字段只能算声明）
        let signature_status = verify_signature(&mut zip, &raw_manifest, &manifest, policy)?;

        // ---- 5. 解包到暂存目录（越界/符号链接/体积在此拦截）
        std::fs::create_dir_all(&self.root).map_err(|e| io_err(&self.root, e))?;
        let staging = self.staging_dir(&manifest.id, &manifest.version);
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging).map_err(|e| io_err(&staging, e))?;
        let extract = extract_all(&mut zip, &staging);
        if let Err(e) = extract {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
        // 解包后复检：resources / entrypoint.args 必须真的存在（协议 §4.0 装机期校验）
        let missing_res = manifest.missing_resources(&staging);
        let missing_entry = manifest.missing_entrypoint_files(&staging);
        if !missing_res.is_empty() || !missing_entry.is_empty() {
            let _ = std::fs::remove_dir_all(&staging);
            let mut parts = Vec::new();
            if !missing_res.is_empty() {
                parts.push(format!("resources 缺失 {missing_res:?}"));
            }
            if !missing_entry.is_empty() {
                parts.push(format!("entrypoint.args 指向的文件缺失 {missing_entry:?}"));
            }
            return Err(RegistryError::Archive(parts.join("；")));
        }

        // ---- 5b. 参数 schema（K07-I）：声明了就必须在包内、且能被宿主解析校验通过。
        // 宁可装不进去，也不能让一份画不出控件/默认值越界的声明落地。
        if let Some(rel) = manifest.params.as_deref() {
            if !staging.join(rel).is_file() {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(RegistryError::ParamsMissing(rel.to_string()));
            }
            if let Err(e) = crate::params::ParamSchema::load(&staging, rel) {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(RegistryError::ParamsSchema(format!("{rel}：{e}")));
            }
        }

        // ---- 6. 落地：同版本同内容 → 复用；同版本不同内容 → 冲突；否则 rename
        let target = self.version_dir(&manifest.id, &manifest.version);
        if target.is_dir() {
            let (_, existing) = crate::manifest::load_pack(&target).map_err(|e| {
                let _ = std::fs::remove_dir_all(&staging);
                RegistryError::Archive(format!(
                    "已装目录 {} 无法解析（{}）；请先卸载该版本再装",
                    target.display(),
                    e
                ))
            })?;
            let _ = std::fs::remove_dir_all(&staging);
            if existing.plugin_hash == identity.plugin_hash {
                return Ok(InstalledPack {
                    manifest,
                    identity,
                    dir: target,
                    signature_status,
                    reused: true,
                    digests_checked,
                });
            }
            return Err(RegistryError::VersionConflict {
                id: manifest.id.clone(),
                version: manifest.version.clone(),
                installed: existing.plugin_hash,
                incoming: identity.plugin_hash.clone(),
            });
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
        }
        if let Err(e) = std::fs::rename(&staging, &target) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(io_err(&target, e));
        }
        Ok(InstalledPack {
            manifest,
            identity,
            dir: target,
            signature_status,
            reused: false,
            digests_checked,
        })
    }

    fn staging_dir(&self, id: &str, version: &str) -> PathBuf {
        let seq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        self.root.join(format!(
            ".staging-{id}-{version}-{}-{seq}",
            std::process::id()
        ))
    }
}

/// 读一个可选条目（不存在 = None；读失败 = Archive 错误）。
fn read_optional<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<Option<Vec<u8>>, RegistryError> {
    if zip.index_for_name(name).is_none() {
        return Ok(None);
    }
    let mut f = zip
        .by_name(name)
        .map_err(|e| RegistryError::Archive(format!("读取 {name} 失败：{e}")))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)
        .map_err(|e| RegistryError::Archive(format!("读取 {name} 失败：{e}")))?;
    Ok(Some(buf))
}

/// `SHA256SUMS`：`<hex>  <path>`（`#` 注释 / 空行忽略；path 允许 `*` 前缀）。
fn parse_sums(text: &str) -> Result<Vec<(String, String)>, RegistryError> {
    let mut out = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.splitn(2, char::is_whitespace);
        let hex = it.next().unwrap_or("").trim().to_ascii_lowercase();
        let path = it
            .next()
            .unwrap_or("")
            .trim()
            .trim_start_matches('*')
            .to_string();
        if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) || path.is_empty() {
            return Err(RegistryError::Archive(format!(
                "{DIGESTS_FILE} 第 {} 行格式错（要 `<sha256-hex>  <相对路径>`）：{line}",
                lineno + 1
            )));
        }
        out.push((path, hex));
    }
    if out.is_empty() {
        return Err(RegistryError::Archive(format!(
            "{DIGESTS_FILE} 没有任何条目（要么删掉它，要么写全）"
        )));
    }
    Ok(out)
}

/// 签名校验：返回落库用的签名状态。
fn verify_signature<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    raw_manifest: &[u8],
    manifest: &Manifest,
    policy: &SignaturePolicy,
) -> Result<SignatureStatus, RegistryError> {
    let sig_text = read_optional(zip, SIGNATURE_FILE)?
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
        .filter(|s| !s.is_empty());

    let Some(sig_text) = sig_text else {
        if policy.requires_signature() {
            return Err(RegistryError::SignatureRequired);
        }
        return Ok(if manifest.signature.is_some() {
            SignatureStatus::Declared
        } else {
            SignatureStatus::Unsigned
        });
    };

    // 有分离签名但部署方没配信任公钥 → 无法校验：如实记 declared（不冒充 verified）
    if !policy.requires_signature() {
        return Ok(SignatureStatus::Declared);
    }
    use base64::Engine as _;
    let sig = base64::engine::general_purpose::STANDARD
        .decode(sig_text.trim())
        .map_err(|e| {
            RegistryError::SignatureInvalid(format!("{SIGNATURE_FILE} 不是 base64：{e}"))
        })?;
    if sig.len() != 64 {
        return Err(RegistryError::SignatureInvalid(format!(
            "ed25519 签名必须是 64 字节，实际 {} 字节",
            sig.len()
        )));
    }
    let mut last = String::from("没有信任公钥");
    for (i, key) in policy.trusted_keys.iter().enumerate() {
        let pk = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key.as_slice());
        match pk.verify(raw_manifest, &sig) {
            Ok(()) => return Ok(SignatureStatus::Verified),
            Err(e) => last = format!("公钥 #{} 校验失败：{e}", i + 1),
        }
    }
    Err(RegistryError::SignatureInvalid(last))
}

/// 归档条目名（非法 UTF-8 → 可读占位，不吞错误）。
fn entry_name<R: Read + Seek>(f: &zip::read::ZipFile<'_, R>) -> String {
    f.name()
        .map(|s| s.to_string())
        .unwrap_or_else(|e| format!("<非法文件名：{e}>"))
}

/// 全量解包（只收普通文件与目录；拒绝符号链接与越界路径）。
fn extract_all<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    dest: &Path,
) -> Result<(), RegistryError> {
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let mut f = zip
            .by_index(i)
            .map_err(|e| RegistryError::Archive(format!("读取第 {i} 个条目失败：{e}")))?;
        if f.is_dir() {
            continue;
        }
        if f.is_symlink() {
            return Err(RegistryError::Archive(format!(
                "归档含符号链接（不允许）：{}",
                entry_name(&f)
            )));
        }
        let Some(rel) = f.enclosed_name() else {
            return Err(RegistryError::Archive(format!(
                "条目路径越界（abs / .. / 盘符）：{}",
                entry_name(&f)
            )));
        };
        if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
            return Err(RegistryError::Archive(format!(
                "条目路径不是纯相对路径：{}",
                rel.display()
            )));
        }
        total += f.size();
        if total > MAX_UNPACKED_BYTES {
            return Err(RegistryError::Archive(format!(
                "解包体积超过上限 {} MiB",
                MAX_UNPACKED_BYTES / 1024 / 1024
            )));
        }
        let out_path = dest.join(&rel);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
        }
        let mut out = File::create(&out_path).map_err(|e| io_err(&out_path, e))?;
        std::io::copy(&mut f, &mut out).map_err(|e| io_err(&out_path, e))?;
        out.flush().map_err(|e| io_err(&out_path, e))?;
    }
    Ok(())
}

/// 打包一个 `.dampack`（安装的对偶操作：测试夹具 / 发布脚本 / 官方包出包共用）。
///
/// `entries` 是 `(归档内相对路径, 内容)`；顺序即写入顺序。签名与摘要由调用方决定要不要放进去
/// （`SHA256SUMS` / `manifest.sig` 就是普通条目）。
pub fn write_archive<W: Write + Seek>(
    writer: W,
    entries: &[(String, Vec<u8>)],
) -> Result<(), RegistryError> {
    let mut zw = zip::ZipWriter::new(writer);
    let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, body) in entries {
        zw.start_file(name.as_str(), opts)
            .map_err(|e| RegistryError::Archive(format!("写归档条目 {name} 失败：{e}")))?;
        zw.write_all(body)
            .map_err(|e| RegistryError::Archive(format!("写归档条目 {name} 失败：{e}")))?;
    }
    zw.finish()
        .map_err(|e| RegistryError::Archive(format!("收尾归档失败：{e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE_MANIFEST: &str = r#"{
  "id": "com.example.pack.demo",
  "name": "示例包",
  "version": "1.0.0",
  "api_version": "1.1",
  "entrypoint": {"cmd": "python3", "args": ["plugin/__init__.py"]},
  "capabilities": ["io.read"],
  "deterministic": false,
  "timeout_secs": 30,
  "resources": ["resources/rules.json"]
}"#;

    fn demo_files(manifest: &str) -> Vec<(String, Vec<u8>)> {
        vec![
            (MANIFEST_FILE.to_string(), manifest.as_bytes().to_vec()),
            (
                "plugin/__init__.py".to_string(),
                b"def serve():\n    pass\n".to_vec(),
            ),
            (
                "resources/rules.json".to_string(),
                r#"{"rules":[{"id":"R1","name":"尺寸"}]}"#.as_bytes().to_vec(),
            ),
            ("README.md".to_string(), b"# demo\n".to_vec()),
        ]
    }

    fn write_zip(dir: &Path, name: &str, entries: &[(String, Vec<u8>)]) -> PathBuf {
        let path = dir.join(name);
        let f = File::create(&path).unwrap();
        write_archive(f, entries).unwrap();
        path
    }

    fn store() -> (tempfile::TempDir, PackStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = PackStore::new(tmp.path());
        (tmp, store)
    }

    /// `local-infer` 打开时宿主支持全部能力；关掉时只支持四个（测试按编译期取值断言）
    fn host_caps() -> Vec<&'static str> {
        default_host_capabilities()
    }

    #[test]
    fn installs_unsigned_pack_into_version_dir() {
        let (tmp, store) = store();
        let zip = write_zip(tmp.path(), "demo.dampack", &demo_files(BASE_MANIFEST));
        let out = store
            .install(&zip, &SignaturePolicy::permissive(), &host_caps())
            .unwrap();
        assert!(!out.reused);
        assert_eq!(out.signature_status, SignatureStatus::Unsigned);
        assert!(
            !out.digests_checked,
            "归档没带 SHA256SUMS → 只验 manifest 身份"
        );
        assert_eq!(out.identity.id, "com.example.pack.demo");
        assert_eq!(out.dir, store.version_dir("com.example.pack.demo", "1.0.0"));
        assert!(out.dir.join(MANIFEST_FILE).is_file());
        assert!(out.dir.join("plugin/__init__.py").is_file());
        assert_eq!(store.installed_versions("com.example.pack.demo"), ["1.0.0"]);
        assert_eq!(store.installed_ids(), ["com.example.pack.demo"]);
    }

    #[test]
    fn same_version_same_content_is_idempotent_no_duplicate_dir() {
        let (tmp, store) = store();
        let zip = write_zip(tmp.path(), "demo.dampack", &demo_files(BASE_MANIFEST));
        let first = store
            .install(&zip, &SignaturePolicy::permissive(), &host_caps())
            .unwrap();
        let second = store
            .install(&zip, &SignaturePolicy::permissive(), &host_caps())
            .unwrap();
        assert!(second.reused, "同版本同内容必须复用，不产生第二个副本");
        assert_eq!(first.dir, second.dir);
        assert_eq!(first.identity.plugin_hash, second.identity.plugin_hash);
        assert_eq!(store.installed_versions("com.example.pack.demo").len(), 1);
        // 暂存目录不残留
        let leftovers: Vec<_> = std::fs::read_dir(store.root())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".staging-"))
            .collect();
        assert!(leftovers.is_empty(), "暂存目录必须清理：{leftovers:?}");
    }

    #[test]
    fn same_version_different_content_is_version_conflict() {
        let (tmp, store) = store();
        let a = write_zip(tmp.path(), "a.dampack", &demo_files(BASE_MANIFEST));
        store
            .install(&a, &SignaturePolicy::permissive(), &host_caps())
            .unwrap();
        let changed = BASE_MANIFEST.replace("\"timeout_secs\": 30", "\"timeout_secs\": 45");
        let b = write_zip(tmp.path(), "b.dampack", &demo_files(&changed));
        let err = store
            .install(&b, &SignaturePolicy::permissive(), &host_caps())
            .unwrap_err();
        assert_eq!(err.code(), "pack_version_conflict");
        assert!(err.to_string().contains("1.0.0"), "{err}");
        // 旧目录没被覆盖
        let (m, _) =
            crate::manifest::load_pack(&store.version_dir("com.example.pack.demo", "1.0.0"))
                .unwrap();
        assert_eq!(m.timeout_secs, 30);
    }

    #[test]
    fn sha256_mismatch_is_rejected_and_nothing_installed() {
        let (tmp, store) = store();
        let mut entries = demo_files(BASE_MANIFEST);
        entries.push((
            DIGESTS_FILE.to_string(),
            format!("{}  {}\n", "0".repeat(64), MANIFEST_FILE).into_bytes(),
        ));
        let zip = write_zip(tmp.path(), "bad-sha.dampack", &entries);
        let err = store
            .install(&zip, &SignaturePolicy::permissive(), &host_caps())
            .unwrap_err();
        assert_eq!(err.code(), "pack_digest_mismatch", "{err}");
        assert!(store.installed_versions("com.example.pack.demo").is_empty());
    }

    #[test]
    fn correct_sha256sums_is_verified() {
        let (tmp, store) = store();
        let mut entries = demo_files(BASE_MANIFEST);
        let mut sums = String::new();
        for (name, body) in &entries {
            sums.push_str(&format!("{}  {name}\n", core_hash::sha256_hex(body)));
        }
        entries.push((DIGESTS_FILE.to_string(), sums.into_bytes()));
        let zip = write_zip(tmp.path(), "sums.dampack", &entries);
        let out = store
            .install(&zip, &SignaturePolicy::permissive(), &host_caps())
            .unwrap();
        assert!(out.digests_checked);
    }

    #[test]
    fn sums_without_manifest_coverage_is_rejected() {
        let (tmp, store) = store();
        let mut entries = demo_files(BASE_MANIFEST);
        entries.push((
            DIGESTS_FILE.to_string(),
            format!("{}  README.md\n", core_hash::sha256_hex(b"# demo\n")).into_bytes(),
        ));
        let zip = write_zip(tmp.path(), "partial.dampack", &entries);
        let err = store
            .install(&zip, &SignaturePolicy::permissive(), &host_caps())
            .unwrap_err();
        assert_eq!(err.code(), "pack_archive");
        assert!(err.to_string().contains("未覆盖"), "{err}");
    }

    #[test]
    fn incompatible_api_version_has_its_own_code() {
        let (tmp, store) = store();
        let bad = BASE_MANIFEST.replace("\"api_version\": \"1.1\"", "\"api_version\": \"1.0\"");
        let zip = write_zip(tmp.path(), "api.dampack", &demo_files(&bad));
        let err = store
            .install(&zip, &SignaturePolicy::permissive(), &host_caps())
            .unwrap_err();
        assert_eq!(err.code(), "pack_api_version", "{err}");
        assert!(err.to_string().contains("1.0"), "{err}");
    }

    #[test]
    fn unknown_capability_is_manifest_error() {
        let (tmp, store) = store();
        let bad = BASE_MANIFEST.replace(
            "\"capabilities\": [\"io.read\"]",
            "\"capabilities\": [\"io.read\", \"op.telepathy\"]",
        );
        let zip = write_zip(tmp.path(), "cap.dampack", &demo_files(&bad));
        let err = store
            .install(&zip, &SignaturePolicy::permissive(), &host_caps())
            .unwrap_err();
        assert_eq!(err.code(), "pack_manifest");
        assert!(err.to_string().contains("op.telepathy"), "{err}");
    }

    /// 「能力缺项」= 包声明的是合法能力，但本部署的宿主不提供（协议 §8：部署方可只授权子集）。
    #[test]
    fn capability_not_offered_by_host_is_rejected() {
        let (tmp, store) = store();
        let with_ocr = BASE_MANIFEST.replace(
            "\"capabilities\": [\"io.read\"]",
            "\"capabilities\": [\"io.read\", \"detect.ocr\"]",
        );
        let zip = write_zip(tmp.path(), "ocr.dampack", &demo_files(&with_ocr));
        // 只授权 io.read 的部署
        let err = store
            .install(&zip, &SignaturePolicy::permissive(), &["io.read"])
            .unwrap_err();
        assert_eq!(err.code(), "pack_capability_missing", "{err}");
        assert!(err.to_string().contains("detect.ocr"), "{err}");
        assert!(store.installed_versions("com.example.pack.demo").is_empty());
        // 同一份包在完整宿主上可装
        let ok = store.install(&zip, &SignaturePolicy::permissive(), &host_caps());
        if cfg!(feature = "local-infer") {
            assert!(ok.is_ok(), "{:?}", ok.err());
        } else {
            assert_eq!(ok.unwrap_err().code(), "pack_capability_missing");
        }
    }

    #[test]
    fn missing_resources_and_entrypoint_are_rejected() {
        let (tmp, store) = store();
        let mut entries = demo_files(BASE_MANIFEST);
        entries.retain(|(n, _)| n != "resources/rules.json");
        let zip = write_zip(tmp.path(), "nores.dampack", &entries);
        let err = store
            .install(&zip, &SignaturePolicy::permissive(), &host_caps())
            .unwrap_err();
        assert_eq!(err.code(), "pack_archive");
        assert!(err.to_string().contains("resources 缺失"), "{err}");
    }

    #[test]
    fn path_traversal_entry_is_rejected() {
        let (tmp, store) = store();
        let mut entries = demo_files(BASE_MANIFEST);
        entries.push(("../evil.sh".to_string(), b"rm -rf /\n".to_vec()));
        let zip = write_zip(tmp.path(), "slip.dampack", &entries);
        let err = store
            .install(&zip, &SignaturePolicy::permissive(), &host_caps())
            .unwrap_err();
        assert_eq!(err.code(), "pack_archive", "{err}");
        assert!(store.installed_versions("com.example.pack.demo").is_empty());
    }

    #[test]
    fn archive_without_manifest_is_rejected() {
        let (tmp, store) = store();
        let zip = write_zip(
            tmp.path(),
            "nomani.dampack",
            &[("README.md".to_string(), b"x".to_vec())],
        );
        let err = store
            .install(&zip, &SignaturePolicy::permissive(), &host_caps())
            .unwrap_err();
        assert_eq!(err.code(), "pack_archive");
        assert!(err.to_string().contains(MANIFEST_FILE), "{err}");
    }

    // ------------------------------------------------ 签名

    fn keypair(seed: u8) -> ring::signature::Ed25519KeyPair {
        ring::signature::Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).unwrap()
    }

    fn pubkey_hex(kp: &ring::signature::Ed25519KeyPair) -> String {
        use ring::signature::KeyPair as _;
        let pk = kp.public_key().as_ref().to_vec();
        pk.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn signed_pack(
        tmp: &Path,
        name: &str,
        kp: &ring::signature::Ed25519KeyPair,
        tamper: bool,
    ) -> PathBuf {
        use base64::Engine as _;
        let mut entries = demo_files(BASE_MANIFEST);
        let raw = entries[0].1.clone();
        let sig = kp.sign(&raw);
        entries.push((
            SIGNATURE_FILE.to_string(),
            base64::engine::general_purpose::STANDARD
                .encode(sig.as_ref())
                .into_bytes(),
        ));
        if tamper {
            // 签名后改 manifest 内容 → 验签必须失败
            entries[0].1 = BASE_MANIFEST
                .replace("\"timeout_secs\": 30", "\"timeout_secs\": 31")
                .into_bytes();
        }
        write_zip(tmp, name, &entries)
    }

    #[test]
    fn valid_detached_signature_verifies_against_trusted_key() {
        let (tmp, store) = store();
        let kp = keypair(7);
        let zip = signed_pack(tmp.path(), "signed.dampack", &kp, false);
        let policy = SignaturePolicy::from_encoded(&[pubkey_hex(&kp)]).unwrap();
        assert!(policy.requires_signature());
        let out = store.install(&zip, &policy, &host_caps()).unwrap();
        assert_eq!(out.signature_status, SignatureStatus::Verified);
    }

    #[test]
    fn tampered_manifest_fails_signature() {
        let (tmp, store) = store();
        let kp = keypair(7);
        let zip = signed_pack(tmp.path(), "tampered.dampack", &kp, true);
        let policy = SignaturePolicy::from_encoded(&[pubkey_hex(&kp)]).unwrap();
        let err = store.install(&zip, &policy, &host_caps()).unwrap_err();
        assert_eq!(err.code(), "pack_signature_invalid", "{err}");
        assert!(store.installed_versions("com.example.pack.demo").is_empty());
    }

    #[test]
    fn trusted_key_configured_but_pack_unsigned_is_rejected() {
        let (tmp, store) = store();
        let kp = keypair(9);
        let zip = write_zip(tmp.path(), "unsigned.dampack", &demo_files(BASE_MANIFEST));
        let policy = SignaturePolicy::from_encoded(&[pubkey_hex(&kp)]).unwrap();
        let err = store.install(&zip, &policy, &host_caps()).unwrap_err();
        assert_eq!(err.code(), "pack_signature_required", "{err}");
    }

    #[test]
    fn signature_without_trusted_keys_is_only_declared() {
        let (tmp, store) = store();
        let kp = keypair(7);
        let zip = signed_pack(tmp.path(), "signed2.dampack", &kp, false);
        let out = store
            .install(&zip, &SignaturePolicy::permissive(), &host_caps())
            .unwrap();
        assert_eq!(
            out.signature_status,
            SignatureStatus::Declared,
            "没配信任公钥 → 不许冒充 verified"
        );
    }

    #[test]
    fn public_key_parsing_accepts_hex_and_base64_only() {
        let kp = keypair(3);
        let hex = pubkey_hex(&kp);
        assert_eq!(parse_public_key(&hex).unwrap().len(), 32);
        use base64::Engine as _;
        use ring::signature::KeyPair as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(kp.public_key().as_ref());
        assert_eq!(parse_public_key(&b64).unwrap(), kp.public_key().as_ref());
        assert!(parse_public_key("not-a-key").is_err());
        assert!(SignaturePolicy::from_encoded(&["zz".to_string()]).is_err());
    }

    /// 真包回归：官方电商插件目录打成 `.dampack` 后能装进版本目录（不是只有手搓夹具能装）。
    ///
    /// 这条也顺带钉住「能力缺项」的反面：官方包声明 detect.u2net / detect.ocr / op.generative，
    /// 只有带 `local-infer` 的宿主（桌面形态）装配得出这三项；关掉该特征时安装会如实报
    /// `pack_capability_missing`，所以这里按编译期取值分支断言，不做一刀切。
    #[test]
    // cfg!(local-infer) 是编译期常量：无 local-infer 时该 Err 分支钉住「不该走到」
    #[allow(clippy::assertions_on_constants)]
    fn official_ecommerce_pack_installs_into_version_dir() {
        let pack_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("packs")
            .join("plugins")
            .join("ecommerce");
        if !pack_dir.join("manifest.json").is_file() {
            eprintln!("跳过：仓库里没有官方电商包目录 {}", pack_dir.display());
            return;
        }
        // 递归收集（顺序稳定，保证两次打包字节一致）
        fn collect(root: &Path, rel: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            let dir = root.join(rel);
            let mut entries: Vec<_> = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let name = e.file_name();
                let child = rel.join(&name);
                if e.file_type().unwrap().is_dir() {
                    collect(root, &child, out);
                } else {
                    let body = std::fs::read(root.join(&child)).unwrap();
                    out.push((child.to_string_lossy().replace('\\', "/"), body));
                }
            }
        }
        let mut entries = Vec::new();
        collect(&pack_dir, Path::new(""), &mut entries);
        assert!(
            entries.iter().any(|(n, _)| n == MANIFEST_FILE),
            "打包产物必须含 manifest.json"
        );

        let (tmp, store) = store();
        let zip = write_zip(tmp.path(), "ecommerce.dampack", &entries);
        let caps = host_caps();
        match store.install(&zip, &SignaturePolicy::permissive(), &caps) {
            Ok(out) => {
                assert_eq!(out.identity.id, "com.soujpg.pack.ecommerce");
                assert_eq!(out.signature_status, SignatureStatus::Unsigned);
                assert!(out.dir.join("plugin/__init__.py").is_file());
                assert!(out.dir.join("resources/rules.json").is_file());
                // 同版本重复装 → 复用
                let again = store
                    .install(&zip, &SignaturePolicy::permissive(), &caps)
                    .unwrap();
                assert!(again.reused);
            }
            Err(e) => {
                assert!(
                    !cfg!(feature = "local-infer"),
                    "带 local-infer 的宿主必须能装官方电商包，实际：{}（{}）",
                    e,
                    e.code()
                );
                assert_eq!(e.code(), "pack_capability_missing", "{e}");
            }
        }
    }

    #[test]
    fn version_ordering_is_numeric_per_segment() {
        use std::cmp::Ordering;
        assert_eq!(compare_versions("1.10.0", "1.9.0"), Ordering::Greater);
        assert_eq!(compare_versions("1.0.0", "1.0.0"), Ordering::Equal);
        assert_eq!(compare_versions("2.0.0", "10.0.0"), Ordering::Less);
    }

    #[test]
    fn uninstall_removes_dir_and_keeps_other_versions() {
        let (tmp, store) = store();
        let v2 = BASE_MANIFEST.replace("\"version\": \"1.0.0\"", "\"version\": \"2.0.0\"");
        let a = write_zip(tmp.path(), "v1.dampack", &demo_files(BASE_MANIFEST));
        let b = write_zip(tmp.path(), "v2.dampack", &demo_files(&v2));
        store
            .install(&a, &SignaturePolicy::permissive(), &host_caps())
            .unwrap();
        store
            .install(&b, &SignaturePolicy::permissive(), &host_caps())
            .unwrap();
        assert_eq!(store.installed_versions("com.example.pack.demo").len(), 2);
        assert!(store
            .uninstall_version("com.example.pack.demo", "1.0.0")
            .unwrap());
        assert_eq!(store.installed_versions("com.example.pack.demo"), ["2.0.0"]);
        assert!(!store
            .uninstall_version("com.example.pack.demo", "1.0.0")
            .unwrap());
        assert!(store
            .uninstall_version("com.example.pack.demo", "2.0.0")
            .unwrap());
        assert!(store.installed_ids().is_empty(), "空父目录要顺手清掉");
        assert!(
            store.uninstall_version("..", "x").is_err(),
            "路径越界必须报错"
        );
    }

    // -----------------------------------------------------------------------
    // K07-I：参数 schema 的装机期把关（声明了就必须在包内 + 能解析校验通过）
    // -----------------------------------------------------------------------

    const PARAMS_MANIFEST: &str = r#"{
  "id": "com.example.pack.params",
  "name": "带参数的示例包",
  "version": "1.0.0",
  "api_version": "1.1",
  "entrypoint": {"cmd": "python3", "args": ["plugin/__init__.py"]},
  "capabilities": ["io.read"],
  "deterministic": true,
  "timeout_secs": 30,
  "resources": ["resources/rules.json"],
  "params": "resources/params.schema.json"
}"#;

    const GOOD_SCHEMA: &str = r#"{
  "schemaVersion": 1,
  "groups": [{"key": "basic", "title": "基本"}],
  "properties": {
    "format": {"type": "enum", "title": "输出格式", "options": ["jpg", "webp"],
               "default": "webp", "group": "basic"},
    "quality": {"type": "integer", "title": "质量", "min": 1, "max": 100,
                "default": 82, "when": {"format": ["jpg", "webp"]}}
  },
  "required": ["format"]
}"#;

    fn params_files(schema: Option<&str>) -> Vec<(String, Vec<u8>)> {
        let mut files = demo_files(PARAMS_MANIFEST);
        if let Some(s) = schema {
            files.push((
                "resources/params.schema.json".to_string(),
                s.as_bytes().to_vec(),
            ));
        }
        files
    }

    #[test]
    fn installs_pack_with_valid_params_schema() {
        let (tmp, store) = store();
        let zip = write_zip(tmp.path(), "p.dampack", &params_files(Some(GOOD_SCHEMA)));
        let out = store
            .install(&zip, &SignaturePolicy::permissive(), &host_caps())
            .unwrap();
        // 装好后宿主能按 manifest 声明读到它（读 schema 接口走的就是这条）
        let (manifest, _) = crate::manifest::load_pack(&out.dir).unwrap();
        let schema =
            crate::params::ParamSchema::load_declared(&out.dir, manifest.params.as_deref())
                .unwrap()
                .expect("声明了 params 就应能读到 schema");
        assert_eq!(schema.required(), ["format".to_string()]);
    }

    #[test]
    fn rejects_pack_declaring_params_but_missing_file() {
        let (tmp, store) = store();
        let zip = write_zip(tmp.path(), "p.dampack", &params_files(None));
        let err = store
            .install(&zip, &SignaturePolicy::permissive(), &host_caps())
            .unwrap_err();
        assert_eq!(err.code(), "pack_params_missing", "{err}");
        assert!(err.to_string().contains("params.schema.json"), "{err}");
        assert!(store.installed_ids().is_empty(), "拒装后不许留下半装的目录");
    }

    #[test]
    fn rejects_pack_with_invalid_params_schema() {
        for bad in [
            // 未知键（v1 不支持 pattern）
            r#"{"schemaVersion":1,"properties":{"q":{"type":"integer","title":"质量","pattern":"^\\d+$"}}}"#,
            // 类型不在白名单
            r#"{"schemaVersion":1,"properties":{"q":{"type":"slider","title":"质量"}}}"#,
            // 默认值越界
            r#"{"schemaVersion":1,"properties":{"q":{"type":"integer","title":"质量","min":1,"max":10,"default":99}}}"#,
            // schemaVersion 不对
            r#"{"schemaVersion":2,"properties":{"q":{"type":"integer","title":"质量"}}}"#,
        ] {
            let (tmp, store) = store();
            let zip = write_zip(tmp.path(), "p.dampack", &params_files(Some(bad)));
            let err = store
                .install(&zip, &SignaturePolicy::permissive(), &host_caps())
                .unwrap_err();
            assert_eq!(err.code(), "pack_params_schema", "{bad} → {err}");
            assert!(store.installed_ids().is_empty(), "{bad} 拒装后不该落地");
        }
    }
}
