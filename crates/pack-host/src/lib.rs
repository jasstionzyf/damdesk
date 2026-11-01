//! L2 `pack-host` — 行业包（子进程代码插件）宿主侧最小内核。
//!
//! 协议权威：`docs/industry-pack-protocol.md`（v1.0，冻结）。本 crate 只实现两件事：
//!
//! 1. [`manifest`]：`manifest.json` 解析与校验（唯一配置面，未知键即拒装、`api_version`
//!    严格相等、`pluginHash` = manifest 原文的 sha256）；
//! 2. [`process`]：子进程生命周期 —— 懒启动 + `health` 握手 + 调用超时（cancel → 宽限 →
//!    kill）+ 崩溃自动重启 ×2 + 冷却，以及「不可判定绝不冒充」（`undetermined` + `note`）。
//!
//! 不算力的部分（能力网关 10 端点、判定/修复算法）不在这里：网关在 `pack-host::gateway`
//! （K03 卡），算法复用 `engine-*` / `core-media` 现有实现。
//!
//! 分层：本 crate 属 L2（与 `svc-*` 同层，`scripts/check_layers.sh` 已登记），
//! 只向下依赖 L0/L1，**不**依赖任何 `svc-*`（避免同层互引）。

pub mod gateway;
pub mod manifest;
pub mod params;
pub mod process;
pub mod registry;
pub mod result;
pub mod result_columns;

pub use gateway::{
    capability_of, Gateway, GatewayConfig, GatewayError, GatewayStartError, VlError, VlJudge,
    VlReply, VlRule, ENDPOINTS, MAX_BODY_BYTES, VL_MAX_TOKENS_CAP,
};
pub use manifest::{
    load_pack, load_pack_migrating, migrate_installed_legacy, Manifest, ManifestError,
    PackIdentity, API_VERSION, KNOWN_CAPABILITIES, LEGACY_CONTRACT_HINT,
};
pub use params::{
    merge_layers, ParamError, ParamGroup, ParamSchema, ParamSpec, ParamType, RESERVED_KEYS,
    SCHEMA_VERSION,
};
pub use process::{
    PackHost, PackHostConfig, PackHostError, TimeoutInfo, Unavailable, DEFAULT_CANCEL_GRACE,
    DEFAULT_COOLDOWN, DEFAULT_HEALTH_TIMEOUT, DEFAULT_MAX_RESTARTS,
};
pub use registry::{
    compare_versions, decode_base64_archive, default_host_capabilities, missing_capabilities,
    parse_public_key, write_archive, InstalledPack, PackStore, RegistryError, SignaturePolicy,
    SignatureStatus, DIGESTS_FILE, MANIFEST_FILE, MAX_UNPACKED_BYTES, PACKS_SUBDIR, SIGNATURE_FILE,
};
pub use result::{
    failure_row, has_artifact, has_verdict, parse_row, parse_row_with, rows_from_reply,
    rows_from_reply_with, PluginOutput, ResultRow, ResultStatus, RowArtifact, RowError, RowVerdict,
    RowViolation, VerdictStatus,
};
