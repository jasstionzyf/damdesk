#![allow(clippy::result_large_err)] // gateway 协议错误枚举本就 ≥128B，装箱会改热路径与错误形状
//! 能力网关 —— 协议 §6 的十二个端点（插件唯一的算力入口）。
//!
//! 宿主在拉起插件子进程前启这个网关：`127.0.0.1:<随机端口>` + 一次性 Bearer 令牌，
//! 按 `manifest.capabilities` 白名单放行。插件拿 `DAMPACK_GATEWAY_URL` / `DAMPACK_GATEWAY_TOKEN`
//! 调它，**不直接联网、不自己实现算法**（协议 §7 铁律 3）。
//!
//! 分层（`scripts/check_layers.sh`）：本 crate 是 L2，只能依赖 L0/L1 ——
//! 所以几何/分割/OCR/重绘全部**直接复用** `core-media`(L0) / `engine-segment`(L1) /
//! `engine-ocr`(L1) / `engine-restore`(L1) 的实现，不重写算法；
//! 唯一跨不过去的是 VL 通道（`svc-pipeline::compliance_vl` 是 L2，同层禁互引）→
//! 用 [`VlJudge`] 端口由 L3 装配注入（见 [`GatewayConfig::with_vl`]）。
//!
//! 端点 → 能力映射（协议 §6，顺序与文档表格一致）：
//!
//! | 端点 | 能力 | 实现来源 |
//! |---|---|---|
//! | `POST /io/read` | `io.read` | `core-media::image_size` + `core-hash::sha256_hex`（可选 `measure:["white"]` → `engine-classic::white_edge`，K07-FIX-D） |
//! | `POST /detect/u2net` | `detect.u2net` | `engine-segment`（u2net ONNX） |
//! | `POST /detect/bbox` | `detect.u2net` | `engine-segment`（同一次推理的掩码 → 主体 bbox，协议 1.1） |
//! | `POST /detect/ocr` | `detect.ocr` | `engine-ocr`（PP-OCRv6_medium） |
//! | `POST /detect/white_stats` | `detect.white` | `engine-classic::detectors::white_edge`（协议 1.1） |
//! | `POST /op/crop` | `op.geometric` | `core-media::crop_to` |
//! | `POST /op/letterbox` | `op.geometric` | 补边复合（本文件，纯几何） |
//! | `POST /op/upscale` | `op.geometric` | `engine-restore`（x4v3）或 `core-media::scale_to` |
//! | `POST /op/encode` | `op.geometric` | `core-media::rgba8` + `encode_rgba_to`（转格式/质量/alpha/EXIF 方向） |
//! | `POST /op/watermark` | `op.overlay` | `core-media::watermark_text`（ab_glyph + 内嵌 Noto Sans SC，协议 1.1 追加） |
//! | `POST /op/replace_bg` | `op.generative` | `engine-segment` 掩码 + 纯色复合（同 C4 口径） |
//! | `POST /op/inpaint` | `op.generative` | `engine-restore`（lama-fp32） |
//! | `POST /vl/judge` | `vl.judge` | [`VlJudge`] 端口（L3 注入；缺省 = 403 capability_missing） |
//! | `POST /report/emit` | `report.emit` | 进程内按 `job_id` 归账 |
//!
//! 三条硬口径：
//! 1. **能力缺失 → 403 `capability_missing`**（未声明/模型未装/本地推理没编进二进制），
//!    插件据此回 `-32001`；绝不静默降级成假结果（协议 §5 / §7 铁律 1）。
//! 2. **`out` 必须落在网关写根之下**（默认 = `work_dir`，L3 可用
//!    [`Gateway::allow_write_root`] 追加本 job 的 workdir）——越界 400，先建父目录再写。
//! 3. **VL 不可判不许冒充**：后端报错一律 `200 {undetermined:true, reason}`，
//!    由插件落 `undetermined`；只有「根本没配 VL 通道」才 403。

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};

use crate::manifest::Manifest;

// ---------------------------------------------------------------------------
// 协议常量
// ---------------------------------------------------------------------------

/// 端点 → 能力白名单（协议 §6 表格的机器可读版本；`tools/pack-conformance/gateway.py`
/// 的 `GATEWAY_ENDPOINTS` 必须与本表逐字一致）。
/// v1.1 增量（N6b，2026-10-22）：`/op/encode` 沿用能力 `op.geometric`
/// （确定性、无模型、只读原图 + 写产物 = 与原 crop/letterbox/upscale 同一权限档），
/// **不新增能力、不升 api_version**——升版本会让所有已装包的严格相等匹配失效。
/// v1.1 增量（N6c，2026-10-22）：`/op/watermark` 用能力 `op.overlay`（新能力，纯追加；
/// 未声明 `op.overlay` 的包看不到此端点 → 403 `capability_missing`，同样不升 api_version）。
/// v1.1 增量（N19，2026-10-23）：`/op/resize` 沿用 `op.geometric`（只缩不放，同 crop 权限档）。
pub const ENDPOINTS: [(&str, &str); 15] = [
    ("/io/read", "io.read"),
    ("/detect/u2net", "detect.u2net"),
    ("/detect/bbox", "detect.u2net"),
    ("/detect/ocr", "detect.ocr"),
    ("/detect/white_stats", "detect.white"),
    ("/op/crop", "op.geometric"),
    ("/op/resize", "op.geometric"),
    ("/op/letterbox", "op.geometric"),
    ("/op/upscale", "op.geometric"),
    ("/op/encode", "op.geometric"),
    ("/op/watermark", "op.overlay"),
    ("/op/replace_bg", "op.generative"),
    ("/op/inpaint", "op.generative"),
    ("/vl/judge", "vl.judge"),
    ("/report/emit", "report.emit"),
];

/// `/vl/judge` 的 `max_tokens` 上限（协议 §6；与 `core-config` 的打标预算同口径）。
pub const VL_MAX_TOKENS_CAP: u32 = 4096;
/// `/vl/judge` 的 `samples` 上限（协议 1.2 增量，K07-FIX-H）：翻转保护只需要少数几份采样，
/// 上限定 3 —— 再多就是拿真金白银换一点统计噪声，不该由包随意决定。
pub const VL_SAMPLES_CAP: u32 = 3;
/// 请求体上限（只传路径不传图，超过这个数一定是客户端搞错了）。
pub const MAX_BODY_BYTES: usize = 1 << 20;
/// 单图允许的最大像素数（防 `letterbox`/`upscale` 造出天文数字画布把进程打死）。
pub const MAX_OUTPUT_PIXELS: u64 = 100_000_000;
/// `/op/upscale` 单趟倍数上限（历史口径 8；超过它由宿主**内部拆成多趟**，不再拒绝请求）。
pub const UPSCALE_MAX_STEP: f64 = 8.0;
/// `/op/upscale` 单次请求允许的总倍数（防呆；真正的硬门槛是像素/长边，见 [`UpscaleLimits`]）。
pub const UPSCALE_MAX_TOTAL: f64 = 64.0;
/// `/op/upscale` 目标长边硬上限。
pub const UPSCALE_MAX_LONG_EDGE: i64 = 20_000;

/// 某张图的放大可用上限（`scale_max` + 对应的短边/长边可达值）。
///
/// 超门槛时宿主必须把这三个数回给插件（「可用上限 + 建议参数」），
/// 让插件/人能自己把 `scale` 调进合法区，而不是只看到一句 `invalid_params`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UpscaleLimits {
    /// 可用放大倍数上限（已向下取到 1e-3，保证填回去一定能过）。
    pub max_scale: f64,
    /// 该倍数下短边能达到的像素。
    pub max_short_side: i64,
    /// 长边硬上限（协议口径，与图无关）。
    pub max_long_edge: i64,
}

/// 按源图尺寸算放大上限：像素上限与长边上限取更紧的那个，再受 [`UPSCALE_MAX_TOTAL`] 约束。
pub fn upscale_limits(width: i32, height: i32) -> UpscaleLimits {
    let (w, h) = (width.max(1) as f64, height.max(1) as f64);
    let by_pixels = (MAX_OUTPUT_PIXELS as f64 / (w * h)).sqrt();
    let by_long = UPSCALE_MAX_LONG_EDGE as f64 / w.max(h);
    let raw = by_pixels.min(by_long).min(UPSCALE_MAX_TOTAL);
    let max_scale = (raw * 1000.0).floor() / 1000.0;
    UpscaleLimits {
        max_scale,
        max_short_side: (w.min(h) * max_scale).floor() as i64,
        max_long_edge: UPSCALE_MAX_LONG_EDGE,
    }
}
/// 接受连接的轮询间隔（`TcpListener` 非阻塞 + 该间隔，用于及时响应 `stop()`）。
const ACCEPT_POLL: Duration = Duration::from_millis(5);

/// 单连接读/写超时（阻塞语义下的兜底；插件是本地进程，正常毫秒级返回）。
const CONN_READ_TIMEOUT: Duration = Duration::from_secs(30);
const CONN_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// 优雅关闭时等对端残留输入的上限（Windows 上带着未读数据 close 会发 RST，见 KI-29）。
const DRAIN_WAIT: Duration = Duration::from_millis(200);
const MAX_DRAIN_BYTES: usize = 64 * 1024;

/// 应答写出失败计数（`dump_stats` 里可见；KI-29 要求「写失败不许静默」）。
static WRITE_ERRORS: AtomicU64 = AtomicU64::new(0);
static READ_ABORTS: AtomicU64 = AtomicU64::new(0);

/// 端点 → 所需能力。
pub fn capability_of(path: &str) -> Option<&'static str> {
    ENDPOINTS
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, cap)| *cap)
}

// ---------------------------------------------------------------------------
// VL 端口（L2 `svc-pipeline::compliance_vl` 由 L3 注入，避免同层互引）
// ---------------------------------------------------------------------------

/// VL 单轮结论。`undetermined=true` = 无法判定（解析失败/模型拒答/图不可读），
/// 插件必须落 `undetermined`（协议 §7 铁律 1）。
#[derive(Debug, Clone, PartialEq)]
pub struct VlReply {
    /// 无法判定（插件必须落 `undetermined`）。
    pub undetermined: bool,
    /// 模型原始输出（JSON 文本或空串）。
    pub raw: String,
    /// 无法判定的原因（`undetermined=true` 时给插件写 `evidence.reason` 用）。
    pub reason: Option<String>,
    /// 可选的逐条结论（插件自行映射到 ruleId）。
    pub rules: Vec<VlRule>,
}

impl VlReply {
    /// 无法判定。
    pub fn undetermined(reason: impl Into<String>) -> Self {
        Self {
            undetermined: true,
            raw: String::new(),
            reason: Some(reason.into()),
            rules: Vec::new(),
        }
    }

    /// 判定成功。
    pub fn decided(raw: impl Into<String>, rules: Vec<VlRule>) -> Self {
        Self {
            undetermined: false,
            raw: raw.into(),
            reason: None,
            rules,
        }
    }
}

/// `/vl/judge` 应答里的单条结论（`{status, label, score}`，协议 §6 第 9 行）。
#[derive(Debug, Clone, PartialEq)]
pub struct VlRule {
    /// `pass` / `fail` / `undetermined` / `skipped`（插件负责闭集校验）。
    pub status: String,
    /// 稳定的机器可读标签。
    pub label: String,
    /// 分数（无分数语义时 `None`，别填 0 冒充）。
    pub score: Option<f64>,
}

/// VL 后端端口：`compliance_vl::VlSession` 这类实现由 L3 装配进来。
pub trait VlJudge: Send + Sync {
    /// 对单图执行一次 VL 提问。`max_tokens` 已由网关校验 ≤ [`VL_MAX_TOKENS_CAP`]。
    fn judge(&self, path: &Path, prompt: &str, max_tokens: u32) -> Result<VlReply, VlError>;

    /// **重复采样**（协议 1.2 增量，K07-FIX-H 翻转保护）：同一 (内容, prompt, 模型) 问
    /// `samples` 次，返回每份的原始结论。
    ///
    /// 为什么默认实现是「连问 N 次」而不是「返回第一份 N 遍」：抖动是**跨请求**发生的，
    /// 复制同一份答案等于假装测过。真正接 VL 的实现（L3 `GatewayVl`）会覆写本方法，
    /// 让第 2..N 份**绕过结果缓存**去拿新样本 —— 否则缓存会把三次采样抹成一次。
    fn judge_samples(
        &self,
        path: &Path,
        prompt: &str,
        max_tokens: u32,
        samples: u32,
    ) -> Result<Vec<VlReply>, VlError> {
        let n = samples.max(1);
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            match self.judge(path, prompt, max_tokens) {
                Ok(reply) => out.push(reply),
                // 第 1 份就不可用 → 整体不可用；后续某份不可用 → 该份记 undetermined
                // （让插件的「一致才算」判据看见抖动，而不是被静默吞掉）
                Err(VlError::BadParams(m)) => return Err(VlError::BadParams(m)),
                Err(VlError::Unavailable(m)) if i == 0 => return Err(VlError::Unavailable(m)),
                Err(VlError::Unavailable(m)) => out.push(VlReply::undetermined(m)),
            }
        }
        Ok(out)
    }
}

/// VL 后端错误。
#[derive(Debug, thiserror::Error)]
pub enum VlError {
    /// 后端不可用（缺 key、端点连不上、超时）→ 网关回 `undetermined`。
    #[error("VL 后端不可用：{0}")]
    Unavailable(String),
    /// 参数非法（如 prompt 为空）→ 网关回 400。
    #[error("VL 参数非法：{0}")]
    BadParams(String),
}

// ---------------------------------------------------------------------------
// 配置与错误
// ---------------------------------------------------------------------------

/// 网关配置。
#[derive(Clone)]
pub struct GatewayConfig {
    /// `manifest.capabilities` 原样传入（部署方可只授权子集，协议 §8）。
    pub capabilities: Vec<String>,
    /// 写根之基：`out` 必须落在它（或 [`GatewayConfig::extra_write_roots`]）之下。
    /// 生产 = 宿主给该包分配的 `DAMPACK_WORK_DIR`。
    pub work_dir: PathBuf,
    /// 一次性令牌；`None` = 启动时随机生成（推荐）。
    pub token: Option<String>,
    /// 额外写根（L3 可在派发 job 前把 job workdir 加进来）。
    pub extra_write_roots: Vec<PathBuf>,
    /// VL 通道；`None` = 未安装 → `/vl/judge` 回 403 `capability_missing`。
    pub vl: Option<Arc<dyn VlJudge>>,
    /// 绑定地址（默认 `127.0.0.1:0` = 随机端口）。
    pub bind: SocketAddr,
}

impl std::fmt::Debug for GatewayConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayConfig")
            .field("capabilities", &self.capabilities)
            .field("work_dir", &self.work_dir)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("extra_write_roots", &self.extra_write_roots)
            .field("vl", &self.vl.as_ref().map(|_| "<injected>"))
            .field("bind", &self.bind)
            .finish()
    }
}

impl GatewayConfig {
    /// 只声明能力 + 写根；其余取默认（随机令牌、随机端口、无 VL）。
    pub fn new(capabilities: Vec<String>, work_dir: impl Into<PathBuf>) -> Self {
        Self {
            capabilities,
            work_dir: work_dir.into(),
            token: None,
            extra_write_roots: Vec::new(),
            vl: None,
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        }
    }

    /// 从 manifest 取能力与写根（L3 装配的主入口）。
    pub fn from_manifest(manifest: &Manifest, work_dir: impl Into<PathBuf>) -> Self {
        Self::new(manifest.capabilities.clone(), work_dir)
    }

    pub fn with_vl(mut self, vl: Arc<dyn VlJudge>) -> Self {
        self.vl = Some(vl);
        self
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }
}

/// 启动失败（端口绑定/写根规范化）。
#[derive(Debug, thiserror::Error)]
pub enum GatewayStartError {
    #[error("绑定 {bind} 失败：{source}")]
    Bind {
        bind: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("写根规范化失败：{path}：{source}")]
    WorkDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// 端点错误（映射到 HTTP 状态 + 协议 §5 的错误码语义）。
#[derive(Debug, Clone, PartialEq)]
pub struct GatewayError {
    pub status: u16,
    /// 机器可读原因：`invalid_params` / `asset_unreadable` / `capability_missing` /
    /// `out_of_workroot` / `no_such_endpoint` / `unauthorized` / `internal`。
    pub error: &'static str,
    /// 人读细节（路径、字段名、底层错误）。
    pub detail: String,
    /// 403 时点名缺哪项能力。
    pub capability: Option<String>,
    /// 出错端点（`/op/upscale` 等）：错误帧里点名「哪个 op 出错」，不让插件侧裸猜。
    pub op: Option<String>,
    /// 实际请求参数（标量/短数组，长串截断）：错误帧里带上「我要的是什么」。
    pub params: Option<Value>,
}

impl GatewayError {
    fn new(status: u16, error: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status,
            error,
            detail: detail.into(),
            capability: None,
            op: None,
            params: None,
        }
    }

    /// 挂上「哪个端点 + 实际参数」（[`dispatch`] 统一做，端点实现不用各自重复）。
    ///
    /// 目的：插件侧拿到的错误必须是可定位的（`op` + `params` + `detail`），
    /// 不许出现「只有一句 `参数非法（400）invalid_params`」这种裸错误（K07 D7）。
    pub fn with_context(mut self, endpoint: &str, body: &Value) -> Self {
        if self.op.is_none() {
            self.op = Some(endpoint.to_string());
        }
        if self.params.is_none() {
            self.params = Some(params_context(body));
        }
        self
    }

    /// 400：参数缺失/类型错/越界。
    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::new(400, "invalid_params", detail)
    }

    /// 404：资产不可读（不存在/解码失败/掩码尺寸不符以外的 IO 问题）。
    pub fn unreadable(detail: impl Into<String>) -> Self {
        Self::new(404, "asset_unreadable", detail)
    }

    /// 403：能力缺失（未声明 / 模型未安装 / 本地推理未编入）。
    pub fn capability_missing(capability: &str, detail: impl Into<String>) -> Self {
        let mut e = Self::new(403, "capability_missing", detail);
        e.capability = Some(capability.to_string());
        e
    }

    /// 400：产物路径越界。
    pub fn out_of_root(detail: impl Into<String>) -> Self {
        Self::new(400, "out_of_workroot", detail)
    }

    /// 500：实现内部错误（磁盘写失败等）。
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(500, "internal", detail)
    }

    /// 应答体（协议 §6：`{"error":..., "capability":...}` / `{"error":..., "detail":...}`）。
    ///
    /// 附加字段（v1.0 追加、向后兼容）：`op` = 出错端点，`params` = 实际请求参数
    /// ——插件侧据此把 `-32003` 之类的错误写成「哪个 op、什么参数、为什么不行」。
    pub fn body(&self) -> Value {
        let mut body = json!({ "error": self.error, "detail": self.detail });
        if let Some(cap) = &self.capability {
            body["capability"] = json!(cap);
        }
        if let Some(op) = &self.op {
            body["op"] = json!(op);
        }
        if let Some(params) = &self.params {
            body["params"] = params.clone();
        }
        body
    }
}

/// 错误帧里的「实际参数上下文」：只回显对象里的前 [`PARAMS_CONTEXT_MAX_FIELDS`] 个字段，
/// 字符串超 [`PARAMS_CONTEXT_MAX_STR`] 截断加省略号——够定位，又不把 1MB 请求体
/// 原样回显示给插件（协议 §6 的请求体上限是 1MB）。
const PARAMS_CONTEXT_MAX_FIELDS: usize = 12;
const PARAMS_CONTEXT_MAX_STR: usize = 240;

fn params_context(body: &Value) -> Value {
    let Value::Object(map) = body else {
        return json!({});
    };
    let mut out = serde_json::Map::new();
    for (k, v) in map.iter().take(PARAMS_CONTEXT_MAX_FIELDS) {
        let shown = match v {
            Value::String(s) => {
                if s.chars().count() > PARAMS_CONTEXT_MAX_STR {
                    let mut t: String = s.chars().take(PARAMS_CONTEXT_MAX_STR).collect();
                    t.push('…');
                    Value::String(t)
                } else {
                    v.clone()
                }
            }
            other => other.clone(),
        };
        out.insert(k.clone(), shown);
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// 网关实例
// ---------------------------------------------------------------------------

struct Inner {
    token: String,
    capabilities: Vec<String>,
    write_roots: Mutex<Vec<PathBuf>>,
    vl: Option<Arc<dyn VlJudge>>,
    /// `job_id` → 上报条目（`/report/emit`）。
    emits: Mutex<HashMap<String, Vec<Value>>>,
    /// OCR 引擎懒加载（det+rec 两个 ONNX 会话，加载贵，进程内共享）。
    #[cfg(feature = "local-infer")]
    ocr: Mutex<Option<Arc<engine_ocr::OcrEngine>>>,
    /// 最近一次分割结果（`/detect/u2net` 与 `/detect/bbox` 共用同一次推理：
    /// 一张图连调两个端点时 u2net 只跑一遍；键含文件 mtime/长度，换内容即失效）。
    #[cfg(feature = "local-infer")]
    seg_cache: Mutex<Option<SegCache>>,
    /// 端点级耗时账本（K07-FIX-D 性能定位）：`路径 → (调用次数, 累计毫秒)`。
    /// `DAMPACK_GATEWAY_STATS=1` 时在网关停止时打一行汇总（不进判定结果、不影响语义）。
    stats: Mutex<HashMap<String, (u64, u64)>>,
    /// 确定性检测结果缓存（K07-FIX-D）：`端点 + 内容 sha256 + 参数 sha256` → 应答。
    /// 同一张图被问两次（judge 首判 + repair 第一轮复判就是同一份文件）只算一遍。
    detect_cache: Mutex<DetectCache>,
    /// 缓存命中次数（`dump_stats` 用）。
    cache_hits: AtomicU64,
    shutdown: AtomicBool,
}

/// 分割结果缓存条目（只留最近一张图，内存 = 一个 320² 掩码）。
#[cfg(feature = "local-infer")]
struct SegCache {
    path: PathBuf,
    w: u32,
    h: u32,
    /// `(文件长度, mtime 纳秒)`：同路径换了内容必须重算，不许把旧掩码当新图的结果
    stamp: (u64, u128),
    seg: Arc<engine_segment::Segmentation>,
}

/// 确定性检测缓存（FIFO 淘汰、容量有上限、可整体关闭）。
struct DetectCache {
    map: HashMap<String, Value>,
    order: VecDeque<String>,
    cap: usize,
}

impl DetectCache {
    fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    fn get(&self, key: &str) -> Option<Value> {
        self.map.get(key).cloned()
    }

    fn put(&mut self, key: String, value: Value) {
        if self.cap == 0 {
            return;
        }
        if self.map.insert(key.clone(), value).is_none() {
            self.order.push_back(key);
        }
        while self.map.len() > self.cap {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            self.map.remove(&old);
        }
    }
}

impl Inner {
    fn allows(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }

    /// 写根快照（canonical 化的绝对路径）。
    fn write_roots_snapshot(&self) -> Vec<PathBuf> {
        self.write_roots.lock().expect("write roots lock").clone()
    }

    fn add_write_root(&self, root: &Path) {
        if let Ok(canon) = root.canonicalize() {
            let mut guard = self.write_roots.lock().expect("write roots lock");
            if !guard.iter().any(|r| r == &canon) {
                guard.push(canon);
            }
        }
    }

    /// `out` 参数校验：必须在某个写根之下；父目录不存在则先建。
    fn accept_out(&self, raw: &str) -> Result<PathBuf, GatewayError> {
        let path = PathBuf::from(raw);
        if raw.trim().is_empty() {
            return Err(GatewayError::invalid("out 不能为空"));
        }
        if !path.is_absolute() {
            return Err(GatewayError::invalid(format!(
                "out 必须是绝对路径（收到 {raw}）"
            )));
        }
        let parent = path
            .parent()
            .ok_or_else(|| GatewayError::invalid(format!("out 没有父目录：{raw}")))?;
        std::fs::create_dir_all(parent).map_err(|e| {
            GatewayError::internal(format!("建产物目录失败 {}：{e}", parent.display()))
        })?;
        let parent_canon = parent
            .canonicalize()
            .map_err(|e| GatewayError::invalid(format!("out 父目录不可解析 {raw}：{e}")))?;
        let file_name = path
            .file_name()
            .ok_or_else(|| GatewayError::invalid(format!("out 没有文件名：{raw}")))?;
        let full = parent_canon.join(file_name);
        let roots = self.write_roots_snapshot();
        if !roots.iter().any(|root| full.starts_with(root)) {
            return Err(GatewayError::out_of_root(format!(
                "out 越界：{} 不在写根之下（允许：{}）",
                full.display(),
                roots
                    .iter()
                    .map(|r| r.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        Ok(full)
    }
}

/// 运行中的网关句柄（`Drop` 即停服）。
pub struct Gateway {
    inner: Arc<Inner>,
    addr: SocketAddr,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway")
            .field("url", &self.base_url())
            .field("token", &"<redacted>")
            .finish()
    }
}

impl Gateway {
    /// 启动网关：绑定端口、规范化写根、起 accept 线程（线程随 `stop()`/`Drop` 退出）。
    pub fn start(cfg: GatewayConfig) -> Result<Self, GatewayStartError> {
        let listener = TcpListener::bind(cfg.bind).map_err(|source| GatewayStartError::Bind {
            bind: cfg.bind,
            source,
        })?;
        let addr = listener
            .local_addr()
            .map_err(|source| GatewayStartError::Bind {
                bind: cfg.bind,
                source,
            })?;
        listener
            .set_nonblocking(true)
            .map_err(|source| GatewayStartError::Bind {
                bind: cfg.bind,
                source,
            })?;

        let work_dir =
            cfg.work_dir
                .canonicalize()
                .map_err(|source| GatewayStartError::WorkDir {
                    path: cfg.work_dir.clone(),
                    source,
                })?;
        let mut roots = vec![work_dir];
        for extra in &cfg.extra_write_roots {
            if let Ok(canon) = extra.canonicalize() {
                roots.push(canon);
            }
        }

        let inner = Arc::new(Inner {
            token: cfg.token.unwrap_or_else(random_token),
            capabilities: cfg.capabilities,
            write_roots: Mutex::new(roots),
            vl: cfg.vl,
            emits: Mutex::new(HashMap::new()),
            #[cfg(feature = "local-infer")]
            ocr: Mutex::new(None),
            #[cfg(feature = "local-infer")]
            seg_cache: Mutex::new(None),
            stats: Mutex::new(HashMap::new()),
            detect_cache: Mutex::new(DetectCache::new(cache_cap())),
            cache_hits: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
        });

        let accept_inner = inner.clone();
        let join = thread::Builder::new()
            .name("pack-gateway".to_string())
            .spawn(move || accept_loop(listener, accept_inner))
            .map_err(|source| GatewayStartError::Bind { bind: addr, source })?;

        Ok(Self {
            inner,
            addr,
            join: Mutex::new(Some(join)),
        })
    }

    /// `http://127.0.0.1:<port>`（注入 `DAMPACK_GATEWAY_URL`）。
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// 一次性令牌（注入 `DAMPACK_GATEWAY_TOKEN`）。
    pub fn token(&self) -> &str {
        &self.inner.token
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// 追加写根（L3 在派发 job 前把该 job 的 workdir 登记进来，协议 §4.3）。
    pub fn allow_write_root(&self, root: impl AsRef<Path>) {
        self.inner.add_write_root(root.as_ref());
    }

    /// 某 `job_id` 已上报的条目（宿主出交付报告用，协议 §3.5）。
    pub fn emits(&self, job_id: &str) -> Vec<Value> {
        self.inner
            .emits
            .lock()
            .expect("emits lock")
            .get(job_id)
            .cloned()
            .unwrap_or_default()
    }

    /// 已上报的 job 数（测试/诊断用）。
    pub fn emit_jobs(&self) -> Vec<String> {
        let guard = self.inner.emits.lock().expect("emits lock");
        let mut jobs: Vec<String> = guard.keys().cloned().collect();
        jobs.sort();
        jobs
    }

    /// 停服（幂等）。不 join accept 线程（它最多再转一个轮询周期就自己退出）。
    pub fn stop(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.join.lock().expect("join lock").take() {
            let _ = handle.join();
        }
        // K07-FIX-D：端点耗时汇总（默认静默；`DAMPACK_GATEWAY_STATS=1` 才有输出）
        self.inner.dump_stats();
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.stop();
    }
}

fn accept_loop(listener: TcpListener, inner: Arc<Inner>) {
    while !inner.shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                let inner = inner.clone();
                // 每连接一线程：插件可能并发打网关（judge 期间还要能处理 cancel）
                let _ = thread::Builder::new()
                    .name("pack-gateway-conn".to_string())
                    .spawn(move || handle_conn(stream, inner));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL);
            }
            Err(_) => thread::sleep(ACCEPT_POLL),
        }
    }
}

// ---------------------------------------------------------------------------
// 最小 HTTP/1.1（只服务本机 JSON-RPC 调用：POST + Content-Length + close）
// ---------------------------------------------------------------------------

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug)]
enum HttpParseError {
    Eof,
    Malformed(String),
    TooLarge(String),
}

fn handle_conn(stream: TcpStream, inner: Arc<Inner>) {
    let mut conn = Conn { stream };
    handle_conn_inner(&mut conn.stream, inner);
    // conn 在作用域结束 drop：半关写方向 + 把对端残留输入读掉后再关，避免 Windows 上
    // 「带着未读数据 close → RST」把已经写出的应答吃掉（KI-29）
}

/// 单连接写句柄：`Drop` 做优雅关闭（KI-29 的第二层保险）。
struct Conn {
    stream: TcpStream,
}

impl Drop for Conn {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Write);
        let _ = self.stream.set_read_timeout(Some(DRAIN_WAIT));
        let mut sink = [0u8; 1024];
        let mut drained = 0usize;
        while drained < MAX_DRAIN_BYTES {
            match self.stream.read(&mut sink) {
                Ok(0) | Err(_) => break,
                Ok(n) => drained += n,
            }
        }
    }
}

/// 每连接显式复位为阻塞语义。
///
/// Windows 上 `accept()` 返回的 socket 会**继承**监听 socket 的非阻塞模式（Linux 不继承），
/// 而本文件的读写全按阻塞语义写。KI-29：真机（Windows）上插件侧读到
/// `WSAECONNABORTED(10053)`，而网关侧统计证明请求已收到并派发成功 —— 就是「服务端以为写完了，
/// 实际只写出部分/读到了非阻塞错误」这个形态。这里把每连接的语义钉死，不依赖平台默认行为。
fn prepare_conn(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(CONN_READ_TIMEOUT))?;
    stream.set_write_timeout(Some(CONN_WRITE_TIMEOUT))?;
    Ok(())
}

fn handle_conn_inner(stream: &mut TcpStream, inner: Arc<Inner>) {
    if let Err(e) = prepare_conn(stream) {
        // 连接语义没钉住就不许硬跑：宁可这条连接失败，也不要写半个应答
        eprintln!("[gateway] 连接初始化失败（{e}）：直接关闭");
        return;
    }
    let req = match read_request(stream) {
        Ok(req) => req,
        Err(HttpParseError::Eof) => return,
        Err(HttpParseError::Malformed(msg)) => {
            write_json(stream, 400, &GatewayError::invalid(msg).body());
            return;
        }
        Err(HttpParseError::TooLarge(msg)) => {
            write_json(stream, 413, &GatewayError::invalid(msg).body());
            return;
        }
    };

    if !req.method.eq_ignore_ascii_case("POST") {
        write_json(
            stream,
            405,
            &json!({ "error": "method_not_allowed", "detail": req.method }),
        );
        return;
    }

    // 一次性 Bearer 令牌（协议 §6）：不正确一律 401，连端点存在性都不透露
    let given = req
        .header("authorization")
        .map(|v| v.trim())
        .unwrap_or("")
        .strip_prefix("Bearer ")
        .unwrap_or("")
        .trim()
        .to_string();
    if given != inner.token {
        write_json(stream, 401, &json!({ "error": "unauthorized" }));
        return;
    }

    let Some(capability) = capability_of(&req.path) else {
        write_json(
            stream,
            404,
            &json!({ "error": "no_such_endpoint", "detail": req.path }),
        );
        return;
    };
    if !inner.allows(capability) {
        let err = GatewayError::capability_missing(capability, "manifest 未声明该能力")
            .with_context(&req.path, &json!({}));
        write_json(stream, 403, &err.body());
        return;
    }

    let body: Value = if req.body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(e) => {
                write_json(
                    stream,
                    400,
                    &GatewayError::invalid(format!("请求体不是合法 JSON：{e}")).body(),
                );
                return;
            }
        }
    };

    let t0 = std::time::Instant::now();
    // N19 真机取证：请求**到达**与**应答写出**各打一条（只在 `DAMPACK_GATEWAY_STATS=1`
    // 时开）。超时场景要能区分「请求根本没到网关」和「到了但没跑完」——两者是完全不同
    // 的故障面（插件 idling vs 网关/引擎卡住），靠日志先后顺序一眼看穿。
    if stats_on() {
        eprintln!("[gateway-req-in] {} raw={}B", req.path, req.body.len());
    }
    match dispatch(&inner, &req.path, &body) {
        Ok(payload) => {
            let elapsed = t0.elapsed();
            inner.note_stat(&req.path, elapsed);
            if stats_on() {
                eprintln!(
                    "[gateway-req-out] {} status=200 {}ms",
                    req.path,
                    elapsed.as_millis()
                );
            }
            write_json(stream, 200, &payload)
        }
        Err(err) => {
            let elapsed = t0.elapsed();
            inner.note_stat(&req.path, elapsed);
            if stats_on() {
                eprintln!(
                    "[gateway-req-out] {} status={} {}ms",
                    req.path,
                    err.status,
                    elapsed.as_millis()
                );
            }
            write_json(stream, err.status, &err.body())
        }
    }
}

impl Inner {
    /// 记一笔端点耗时（性能定位用；不改变任何语义）。
    fn note_stat(&self, path: &str, elapsed: Duration) {
        let mut guard = self.stats.lock().expect("stats lock");
        let slot = guard.entry(path.to_string()).or_insert((0, 0));
        slot.0 += 1;
        slot.1 += elapsed.as_millis() as u64;
    }

    /// 查确定性检测缓存（命中计数单独记，`calls` 语义不变）。
    fn cached(&self, key: &str) -> Option<Value> {
        let hit = self
            .detect_cache
            .lock()
            .expect("detect cache lock")
            .get(key);
        if hit.is_some() {
            self.cache_hits.fetch_add(1, Ordering::Relaxed);
        }
        hit
    }

    fn remember(&self, key: String, value: &Value) {
        self.detect_cache
            .lock()
            .expect("detect cache lock")
            .put(key, value.clone());
    }

    /// `DAMPACK_GATEWAY_STATS=1` 时打一行汇总（默认静默）。
    fn dump_stats(&self) {
        let on = std::env::var("DAMPACK_GATEWAY_STATS")
            .map(|v| matches!(v.trim(), "1" | "on" | "true" | "yes"))
            .unwrap_or(false);
        if !on {
            return;
        }
        let guard = self.stats.lock().expect("stats lock");
        let mut rows: Vec<(&String, &(u64, u64))> = guard.iter().collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.1 .1));
        let mut parts = Vec::new();
        for (path, (n, ms)) in rows {
            parts.push(format!("{path} n={n} ms={ms}"));
        }
        eprintln!(
            "[gateway-stats] cache_hits={} write_errors={} read_aborts={} {}",
            self.cache_hits.load(Ordering::Relaxed),
            WRITE_ERRORS.load(Ordering::Relaxed),
            READ_ABORTS.load(Ordering::Relaxed),
            parts.join(" | ")
        );
    }
}

/// 检测缓存容量：`DAMPACK_GATEWAY_CACHE_MAX`（默认 256），`DAMPACK_GATEWAY_CACHE=0` 关闭。
fn cache_cap() -> usize {
    let off = std::env::var("DAMPACK_GATEWAY_CACHE")
        .map(|v| matches!(v.trim(), "0" | "off" | "false" | "no"))
        .unwrap_or(false);
    if off {
        return 0;
    }
    std::env::var("DAMPACK_GATEWAY_CACHE_MAX")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(256)
}

/// 确定性检测的缓存键：`端点 | 内容 sha256 | 参数 sha256`（`path` 本身不进参数体 ——
/// 同一个文件换个路径名问是同一件事，内容 sha 已经表达了身份）。
fn detect_cache_key(endpoint: &str, body: &Value) -> Option<String> {
    let path = body.get("path")?.as_str()?;
    let bytes = std::fs::read(path).ok()?;
    let mut params = body.clone();
    if let Value::Object(map) = &mut params {
        map.remove("path");
    }
    // 键序无关：`serde_json::Value` 的 Map 默认按 key 排序输出（BTreeMap），天然稳定
    let params_json = serde_json::to_string(&params).unwrap_or_default();
    Some(format!(
        "{}|{}|{}",
        endpoint,
        core_hash::sha256_hex(&bytes),
        core_hash::sha256_hex(params_json.as_bytes())
    ))
}

/// 带缓存的检测调用：算成功才存（失败/错误不缓存 —— 缺模型是环境问题，装好后要能立刻生效）。
fn cached_detect<F>(
    inner: &Inner,
    endpoint: &str,
    body: &Value,
    compute: F,
) -> Result<Value, GatewayError>
where
    F: FnOnce(&Value) -> Result<Value, GatewayError>,
{
    let Some(key) = detect_cache_key(endpoint, body) else {
        return compute(body);
    };
    if let Some(hit) = inner.cached(&key) {
        return Ok(hit);
    }
    let out = compute(body)?;
    inner.remember(key, &out);
    Ok(out)
}

fn read_request(stream: &mut TcpStream) -> Result<Request, HttpParseError> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 16 * 1024 {
            return Err(HttpParseError::TooLarge("请求头超过 16 KiB".into()));
        }
        let n = stream
            .read(&mut chunk)
            .map_err(|e| read_err("读请求头失败", e))?;
        if n == 0 {
            return Err(HttpParseError::Eof);
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    if method.is_empty() || target.is_empty() {
        return Err(HttpParseError::Malformed(format!(
            "请求行不合法：{request_line:?}"
        )));
    }
    // 只路由路径（忽略 query，协议不带 query）
    let path = target.split('?').next().unwrap_or("").to_string();

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            return Err(HttpParseError::Malformed(format!("请求头不合法：{line:?}")));
        };
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }

    let content_length: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map(|(_, v)| {
            v.parse()
                .map_err(|_| HttpParseError::Malformed(format!("content-length 不合法：{v}")))
        })
        .transpose()?
        .unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        // 先把超限请求体读掉（有上限），再回 413 —— 直接关连接会让客户端写半边收到 RST，
        // 报错就变成了「连接被重置」，错误定位全错
        let drain_cap = content_length.min(8 * MAX_BODY_BYTES);
        let mut drained = buf.len().saturating_sub(head_end + 4);
        while drained < drain_cap {
            let n = match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            drained += n;
        }
        return Err(HttpParseError::TooLarge(format!(
            "请求体 {content_length} 字节超过上限 {MAX_BODY_BYTES}"
        )));
    }

    let body_start = head_end + 4;
    let mut body = buf[body_start..].to_vec();
    while body.len() < content_length {
        let n = stream
            .read(&mut chunk)
            .map_err(|e| read_err("读请求体失败", e))?;
        if n == 0 {
            return Err(HttpParseError::Malformed("请求体提前结束".into()));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    Ok(Request {
        method,
        path,
        headers,
        body,
    })
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// 读失败归类：**超时/非阻塞残留不是「请求不合法」**，按对端放弃（Eof）处理 ——
/// 否则真机取证只能看到一条 400 的假线索（KI-29 复盘：语义错标会把定位带偏）。
fn read_err(what: &str, e: std::io::Error) -> HttpParseError {
    let n = READ_ABORTS.fetch_add(1, Ordering::Relaxed) + 1;
    match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
            eprintln!("[gateway] {what}（超时/非阻塞残留，按对端放弃处理）read_aborts={n}：{e}");
            HttpParseError::Eof
        }
        _ => HttpParseError::Malformed(format!("{what}：{e}")),
    }
}

fn write_json(stream: &mut TcpStream, status: u16, payload: &Value) {
    let body = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json; charset=utf-8\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let head_len = head.len();
    let res = write_all_full(stream, head.as_bytes())
        .and_then(|_| write_all_full(stream, &body))
        .and_then(|_| stream.flush());
    if let Err(e) = res {
        // KI-29：写失败**不许静默** —— 真机上客户端只报「连接被中止」，服务端若也失声就无从定位。
        let n = WRITE_ERRORS.fetch_add(1, Ordering::Relaxed) + 1;
        eprintln!(
            "[gateway] 应答写出失败 status={status} head={head_len}B body={}B write_errors={n}：{e}",
            body.len()
        );
    } else if stats_on() {
        // 真机取证用：写成功也给一条，能对账「服务端写出的字节数 vs 客户端读到的」
        eprintln!(
            "[gateway-conn] 应答写出 status={status} head={head_len}B body={}B",
            body.len()
        );
    }
}

/// `DAMPACK_GATEWAY_STATS` 开关（与 `dump_stats` 同一口径）。
fn stats_on() -> bool {
    std::env::var("DAMPACK_GATEWAY_STATS")
        .map(|v| matches!(v.trim(), "1" | "on" | "true" | "yes"))
        .unwrap_or(false)
}

/// 全量写出：`write_all` 在部分写后直接返回错误会留下半个应答（客户端只看到连接被中止）。
/// 这里自己循环，`Interrupted` 重试、`WouldBlock` 短暂让出（非阻塞残留/发送缓冲满）。
fn write_all_full(stream: &mut TcpStream, buf: &[u8]) -> std::io::Result<()> {
    let mut off = 0usize;
    let mut stalls = 0u32;
    while off < buf.len() {
        match stream.write(&buf[off..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "对端不再接受数据",
                ))
            }
            Ok(n) => {
                off += n;
                stalls = 0;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                stalls += 1;
                if stalls > 6000 {
                    // 30s 兜底（6000 × 5ms），与写超时同量级
                    return Err(e);
                }
                thread::sleep(ACCEPT_POLL);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn random_token() -> String {
    let mut buf = [0u8; 32];
    #[cfg(unix)]
    {
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            if f.read_exact(&mut buf).is_ok() {
                return hex(&buf);
            }
        }
    }
    // 兜底（极少数平台）：时间 + pid + HashMap RandomState 的 OS 熵混合
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.write_u32(std::process::id());
    format!("{:016x}{:016x}", h.finish(), h.finish())
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ---------------------------------------------------------------------------
// 端点实现
// ---------------------------------------------------------------------------

fn dispatch(inner: &Inner, endpoint: &str, body: &Value) -> Result<Value, GatewayError> {
    dispatch_inner(inner, endpoint, body).map_err(|e| e.with_context(endpoint, body))
}

fn dispatch_inner(inner: &Inner, endpoint: &str, body: &Value) -> Result<Value, GatewayError> {
    if !body.is_object() {
        return Err(GatewayError::invalid("请求体必须是 JSON 对象"));
    }
    match endpoint {
        "/io/read" => io_read(body),
        // K07-FIX-D：确定性检测按「内容 + 参数」复用（同一张图问两次 = 算一遍）
        "/detect/u2net" => cached_detect(inner, endpoint, body, |b| detect_u2net(inner, b)),
        "/detect/bbox" => detect_bbox(inner, body),
        "/detect/ocr" => cached_detect(inner, endpoint, body, |b| detect_ocr(inner, b)),
        "/detect/white_stats" => detect_white_stats(body),
        "/op/crop" => op_crop(inner, body),
        "/op/resize" => op_resize(inner, body),
        "/op/letterbox" => op_letterbox(inner, body),
        "/op/upscale" => op_upscale(inner, body),
        "/op/encode" => op_encode(inner, body),
        "/op/watermark" => op_watermark(inner, body),
        "/op/replace_bg" => op_replace_bg(inner, body),
        "/op/inpaint" => op_inpaint(inner, body),
        "/vl/judge" => vl_judge(inner, body),
        "/report/emit" => report_emit(inner, body),
        other => Err(GatewayError::new(
            404,
            "no_such_endpoint",
            format!("网关无此端点：{other}"),
        )),
    }
}

fn req_str<'a>(body: &'a Value, field: &str) -> Result<&'a str, GatewayError> {
    body.get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| GatewayError::invalid(format!("缺字段 {field}（必须是非空字符串）")))
}

fn req_path(body: &Value) -> Result<PathBuf, GatewayError> {
    let raw = req_str(body, "path")?;
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(GatewayError::invalid(format!(
            "path 必须是绝对路径（收到 {raw}）"
        )));
    }
    Ok(path)
}

fn req_file(body: &Value, field: &str) -> Result<PathBuf, GatewayError> {
    let raw = req_str(body, field)?;
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(GatewayError::invalid(format!(
            "{field} 必须是绝对路径（收到 {raw}）"
        )));
    }
    if !path.is_file() {
        return Err(GatewayError::unreadable(format!(
            "{field} 不是可读文件：{raw}"
        )));
    }
    Ok(path)
}

fn req_number(body: &Value, field: &str) -> Result<f64, GatewayError> {
    body.get(field)
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
        .ok_or_else(|| GatewayError::invalid(format!("缺字段 {field}（必须是数字）")))
}

/// 可选数字字段：缺省用 `default`，给了但类型不对 → 400（不静默吞）。
fn opt_number(body: &Value, field: &str, default: f64) -> Result<f64, GatewayError> {
    match body.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_f64()
            .filter(|n| n.is_finite())
            .ok_or_else(|| GatewayError::invalid(format!("{field} 必须是数字（收到 {v}）"))),
    }
}

/// 可选整数字段：不接受 8.5 这类「差不多」的输入（静默截断 = 参数与产物不一致）。
fn opt_int(body: &Value, field: &str, default: i32) -> Result<i32, GatewayError> {
    match body.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => {
            let n = v
                .as_f64()
                .filter(|n| n.is_finite() && n.fract() == 0.0)
                .ok_or_else(|| GatewayError::invalid(format!("{field} 必须是整数（收到 {v}）")))?;
            if n < i32::MIN as f64 || n > i32::MAX as f64 {
                return Err(GatewayError::invalid(format!("{field}={n} 越界")));
            }
            Ok(n as i32)
        }
    }
}

fn parse_color(raw: &str) -> Result<[u8; 3], GatewayError> {
    let trimmed = raw.trim().trim_start_matches('#');
    core_media::TextWatermark::parse_color(trimmed)
        .map_err(|e| GatewayError::invalid(format!("background/color 不合法：{e}")))
}

fn mime_of(path: &Path) -> &'static str {
    match path
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
        "tif" | "tiff" => "image/tiff",
        "heic" => "image/heic",
        _ => "application/octet-stream",
    }
}

/// `POST /io/read` —— 只读元信息，不解码像素（除宽高）。
fn io_read(body: &Value) -> Result<Value, GatewayError> {
    let path = req_file(body, "path")?;
    let meta = std::fs::metadata(&path)
        .map_err(|e| GatewayError::unreadable(format!("stat 失败 {}：{e}", path.display())))?;
    let bytes = std::fs::read(&path)
        .map_err(|e| GatewayError::unreadable(format!("读文件失败 {}：{e}", path.display())))?;
    let (w, h) = core_media::image_size(&path)
        .map_err(|e| GatewayError::unreadable(format!("解码失败 {}：{e}", path.display())))?;
    let mut payload = json!({
        "path": path.to_string_lossy(),
        "size": meta.len(),
        "sha256": core_hash::sha256_hex(&bytes),
        "mime": mime_of(&path),
        "width": w,
        "height": h,
    });
    // 可选像素统计（K07-FIX-D）：`measure:["white"]` → 边缘白度，供 R2 走 classic 口径。
    //
    // 为什么挂在 `/io/read` 而不是新端点：插件对**每一个**路径本来就要读一次元信息
    // （judge 与每一轮复判都要宽高），把「边缘白度」做成同一请求的可选字段 → 零额外
    // 往返、零额外解码（`rgb8` 本来就要解一次图），也就不用动端点表/能力白名单。
    // 语义与旧引擎 classic `white_edge` 逐字一致（同一份 `engine-classic` 实现）：
    // 外圈 `band` 像素带内每像素取 `255 - min(R,G,B)`，返回全带均值 `white_dev`。
    if wants_white_measure(body) {
        let band = match body.get("band") {
            None | Some(Value::Null) => 4u32,
            Some(v) => v
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or_else(|| GatewayError::invalid("band 必须是正整数（像素）"))?
                .min(u32::MAX as u64) as u32,
        };
        let (pw, ph, px) = core_media::rgb8(&path)
            .map_err(|e| GatewayError::unreadable(format!("解码失败 {}：{e}", path.display())))?;
        let report = engine_classic::detectors::white_edge(
            &px,
            pw as u32,
            ph as u32,
            // `max_dev` 只用来算 `pass`，本端点只回统计值（判不判由插件按包阈值决定）
            // → 给无穷大，结论字段一律 pass，不进 payload。
            &engine_classic::detectors::WhiteEdgeParams {
                max_dev: f64::INFINITY,
                band,
            },
        )
        .map_err(|e| GatewayError::unreadable(format!("白度统计失败 {}：{e}", path.display())))?;
        payload["white_dev"] = json!(report.mean_dev);
        payload["white_worst_dev"] = json!(report.max_dev);
        payload["white_band"] = json!(report.band);
        payload["white_edge_pixels"] = json!(report.edge_pixels);
    }
    Ok(payload)
}

/// `/io/read` 是否要额外算边缘白度：`measure` 数组含 `"white"`（也接受 `measure:["white"]`
/// 之外的老写法 `measure:true`）。
fn wants_white_measure(body: &Value) -> bool {
    match body.get("measure") {
        Some(Value::Array(items)) => items.iter().any(|v| {
            v.as_str()
                .map(|s| s.eq_ignore_ascii_case("white"))
                .unwrap_or(false)
        }),
        _ => false,
    }
}

/// `POST /detect/u2net` —— 主体占比 + 掩码落盘（`<work>/masks/<sha16>.png`）+ 主体 bbox。
///
/// bbox 与占比来自**同一次**推理（两次调用会把模型跑两遍）：`bbox` = 全掩码外接框、
/// `dominant_bbox` = 最大连通域（主主体）外接框，均为源图像素 `[x1,y1,x2,y2]`（左闭右开）。
fn detect_u2net(inner: &Inner, body: &Value) -> Result<Value, GatewayError> {
    let path = req_file(body, "path")?;
    let (w, h, px) = core_media::rgb8(&path)
        .map_err(|e| GatewayError::unreadable(format!("解码失败 {}：{e}", path.display())))?;
    let (w, h) = (w as u32, h as u32);
    let seg = segment_cached(inner, "detect.u2net", &path, w, h, &px)?;
    let mask = seg.mask.expand_to(w, h);
    let mask_path = write_mask_png(inner, &path, w, h, &mask)?;
    let (bbox, dominant) = mask_boxes(seg.mask.bbox(), seg.mask.dominant_bbox(), w, h);
    let instances = mask_instances(&seg.mask, w, h, DEFAULT_MIN_INSTANCE_RATIO, MAX_INSTANCES);
    Ok(json!({
        "path": path.to_string_lossy(),
        "fg_ratio": seg.fg_ratio,
        "mask_path": mask_path.to_string_lossy(),
        "width": w,
        "height": h,
        "bbox": bbox,
        "dominant_bbox": dominant,
        "instances": instances,
    }))
}

/// `POST /detect/bbox` —— 主体包围盒 +（可选 `target_ratio`）主主体锚定的裁剪窗。
///
/// 为什么需要它：几何修复要「按主体位置裁」，只拿 `fg_ratio` 标量只能居中推定，
/// 主体贴边的图会被裁掉一部分（K07 门禁误判的根因之一）。本端点回的是**同一张掩码**
/// 的包围盒（协议 1.1）；给 `target_ratio` 时再回 `window`——主主体 bbox 中心对齐、
/// 掩码占比最接近目标的窗口（与 `engine-segment` 的 smart_crop 同一份数学，确定性）。
///
/// 无前景 → `bbox`/`dominant_bbox`/`window` 均为 `null`（不编造框，插件据此判不可修）。
fn detect_bbox(inner: &Inner, body: &Value) -> Result<Value, GatewayError> {
    let path = req_file(body, "path")?;
    let target = req_optional_ratio(body, "target_ratio")?;
    let min_instance =
        req_optional_ratio(body, "min_instance_ratio")?.unwrap_or(DEFAULT_MIN_INSTANCE_RATIO);
    let (w, h, px) = core_media::rgb8(&path)
        .map_err(|e| GatewayError::unreadable(format!("解码失败 {}：{e}", path.display())))?;
    let (w, h) = (w as u32, h as u32);
    let seg = segment_cached(inner, "detect.u2net", &path, w, h, &px)?;
    let (bbox, dominant) = mask_boxes(seg.mask.bbox(), seg.mask.dominant_bbox(), w, h);
    let instances = mask_instances(&seg.mask, w, h, min_instance, MAX_INSTANCES);
    let mut out = json!({
        "path": path.to_string_lossy(),
        "width": w,
        "height": h,
        "fg_ratio": seg.fg_ratio,
        "engine": "u2net",
        "grid": engine_segment::SIDE,
        "bbox": bbox,
        "dominant_bbox": dominant,
        // 多实例主体（K07 f002）：文字归属要按实例集合判，只认主主体会把其余商品
        // 的自带文字算成「主体外文字」。老宿主没有这个字段，插件退回单主体口径。
        "instances": instances,
        "min_instance_ratio": min_instance,
        "window": Value::Null,
    });
    if let Some(t) = target {
        if let Some((win, ratio)) = seg.crop_window(t) {
            out["window"] = json!({
                "box": grid_box_to_pixels(win, w, h),
                "ratio": ratio,
                "target_ratio": t,
            });
        }
    }
    Ok(out)
}

/// `POST /detect/white_stats` —— 白底纯净度像素统计（`engine-classic` 同一份实现）。
///
/// 语义（与旧引擎 `white_edge` 检测器逐值同口径）：外圈 `band` 像素带内，每像素取
/// `255 - min(R,G,B)`，返回全带均值 `mean_dev` 与最差单像素 `worst_dev`；
/// `pass = mean_dev <= threshold`。中心主体不参与统计，纯 Rust、不需要模型、同图同参数同结果。
fn detect_white_stats(body: &Value) -> Result<Value, GatewayError> {
    let path = req_file(body, "path")?;
    let band = match body.get("band") {
        None | Some(Value::Null) => 4u32,
        Some(v) => v
            .as_u64()
            .filter(|b| (1..=4096).contains(b))
            .ok_or_else(|| GatewayError::invalid("band 必须是 1..=4096 的整数（边缘带宽度 px）"))?
            as u32,
    };
    let threshold = match body.get("max_dev") {
        None | Some(Value::Null) => 8.0f64,
        Some(v) => v
            .as_f64()
            .filter(|t| t.is_finite() && (0.0..=255.0).contains(t))
            .ok_or_else(|| {
                GatewayError::invalid("max_dev 必须是 0..=255 的数字（均值偏差上限）")
            })?,
    };
    let (w, h, px) = core_media::rgb8(&path)
        .map_err(|e| GatewayError::unreadable(format!("解码失败 {}：{e}", path.display())))?;
    if w <= 0 || h <= 0 {
        return Err(GatewayError::unreadable(format!("非法图片尺寸 {w}x{h}")));
    }
    let params = engine_classic::detectors::WhiteEdgeParams {
        max_dev: threshold,
        band,
    };
    let report = engine_classic::detectors::white_edge(&px, w as u32, h as u32, &params)
        .map_err(|e| GatewayError::unreadable(format!("白边统计失败 {}：{e}", path.display())))?;
    Ok(json!({
        "path": path.to_string_lossy(),
        "width": w,
        "height": h,
        "engine": "classic-white-edge",
        "mean_dev": report.mean_dev,
        "worst_dev": report.max_dev,
        "band": report.band,
        "edge_pixels": report.edge_pixels,
        "threshold": threshold,
        "pass": report.pass,
    }))
}

/// 网格（320²）掩码框 → 源图像素框 `[x1,y1,x2,y2]`（左闭右开，与 `/op/crop` 同口径）。
///
/// 映射与 `Mask::expand_to` 的最近邻一致：网格 cell `g` 覆盖像素 `[g*W/side, (g+1)*W/side)`，
/// 所以紧框右/下边界向上取整，保证框内不丢任何前景像素。
fn grid_box_to_pixels(b: [usize; 4], w: u32, h: u32) -> [u32; 4] {
    let side = engine_segment::SIDE as u64;
    let (w64, h64) = (w as u64, h as u64);
    let span = |lo: usize, hi: usize, full: u64| -> (u32, u32) {
        let start = (lo as u64 * full / side) as u32;
        let end = ((hi as u64 + 1) * full).div_ceil(side).min(full) as u32;
        (
            start.min(end.saturating_sub(1)),
            end.max(start + 1).min(full as u32),
        )
    };
    let (x1, x2) = span(b[0], b[2], w64);
    let (y1, y2) = span(b[1], b[3], h64);
    [x1, y1, x2, y2]
}

/// 可选的比例参数（`0 < r <= 1`）；缺省/`null` → None。
fn req_optional_ratio(body: &Value, field: &str) -> Result<Option<f64>, GatewayError> {
    match body.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let r = v
                .as_f64()
                .filter(|r| r.is_finite())
                .ok_or_else(|| GatewayError::invalid(format!("{field} 必须是数字")))?;
            if !(r > 0.0 && r <= 1.0) {
                return Err(GatewayError::invalid(format!(
                    "{field}={r} 越界（允许 0 < r <= 1）"
                )));
            }
            Ok(Some(r))
        }
    }
}

/// u2net 分割（带「最近一次结果」缓存：`/detect/u2net` 与 `/detect/bbox` 连调只推理一次）。
#[cfg(feature = "local-infer")]
fn segment_cached(
    inner: &Inner,
    capability: &str,
    path: &Path,
    w: u32,
    h: u32,
    px: &[u8],
) -> Result<Arc<engine_segment::Segmentation>, GatewayError> {
    let stamp = file_stamp(path);
    {
        let cache = inner.seg_cache.lock().expect("seg cache lock");
        if let Some(hit) = cache.as_ref() {
            if hit.path == path && hit.w == w && hit.h == h && hit.stamp == stamp {
                return Ok(Arc::clone(&hit.seg));
            }
        }
    }
    let seg = engine_segment::segmenter(engine_segment::U2NET).map_err(|e| match e {
        engine_segment::SegmentError::ModelMissing(_)
        | engine_segment::SegmentError::NoModelsRoot => GatewayError::capability_missing(
            capability,
            format!("u2net 模型未安装（{}）：{e}", path.display()),
        ),
        other => GatewayError::internal(format!("加载 u2net 失败：{other}")),
    })?;
    let run = seg
        .run_rgb(w, h, px)
        .map_err(|e| GatewayError::internal(format!("u2net 推理失败：{e}")))?;
    let arc = Arc::new(run);
    let mut cache = inner.seg_cache.lock().expect("seg cache lock");
    *cache = Some(SegCache {
        path: path.to_path_buf(),
        w,
        h,
        stamp,
        seg: Arc::clone(&arc),
    });
    Ok(arc)
}

/// `(文件长度, mtime 纳秒)`：缓存键的失效依据（取不到就给全 0，退化成「同尺寸即命中」之外的
/// 保守行为——不命中，重算）。
#[cfg(feature = "local-infer")]
fn file_stamp(path: &Path) -> (u64, u128) {
    match std::fs::metadata(path) {
        Ok(m) => {
            let nanos = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            (m.len(), nanos)
        }
        Err(_) => (0, 0),
    }
}

#[cfg(not(feature = "local-infer"))]
fn segment_cached(
    _inner: &Inner,
    capability: &str,
    _path: &Path,
    _w: u32,
    _h: u32,
    _px: &[u8],
) -> Result<Arc<engine_segment::Segmentation>, GatewayError> {
    Err(GatewayError::capability_missing(
        capability,
        "本地推理未编入二进制（web 形态）",
    ))
}

/// 掩码 + 占比（`/op/replace_bg` 用：只要掩码，不要 bbox）。
fn segment_run(
    inner: &Inner,
    capability: &str,
    path: &Path,
    w: u32,
    h: u32,
    px: &[u8],
) -> Result<(f64, Vec<u8>), GatewayError> {
    let seg = segment_cached(inner, capability, path, w, h, px)?;
    Ok((seg.fg_ratio, seg.mask.expand_to(w, h)))
}

/// 掩码落盘为可视 PNG（灰度按 RGB 三通道写，任意看图工具都能打开）。
fn write_mask_png(
    inner: &Inner,
    src: &Path,
    w: u32,
    h: u32,
    mask: &[u8],
) -> Result<PathBuf, GatewayError> {
    let root = inner
        .write_roots_snapshot()
        .into_iter()
        .next()
        .ok_or_else(|| GatewayError::internal("网关没有写根"))?;
    let dir = root.join("masks");
    std::fs::create_dir_all(&dir)
        .map_err(|e| GatewayError::internal(format!("建掩码目录失败 {}：{e}", dir.display())))?;
    let key = core_hash::sha256_hex(format!("{}:{w}x{h}", src.display()).as_bytes());
    let out = dir.join(format!("{}.png", &key[..16]));
    let mut rgb = vec![0u8; (w as usize) * (h as usize) * 3];
    for (i, &m) in mask.iter().enumerate() {
        rgb[i * 3] = m;
        rgb[i * 3 + 1] = m;
        rgb[i * 3 + 2] = m;
    }
    core_media::encode_rgb_to(w as i32, h as i32, &rgb, &out, None)
        .map_err(|e| GatewayError::internal(format!("写掩码 PNG 失败：{e}")))?;
    Ok(out)
}

/// `POST /detect/ocr` —— 行文本 + 源图坐标框（阅读序）。
fn detect_ocr(inner: &Inner, body: &Value) -> Result<Value, GatewayError> {
    let file = req_file(body, "path")?;
    let (w, h, px) = core_media::rgb8(&file)
        .map_err(|e| GatewayError::unreadable(format!("解码失败 {}：{e}", file.display())))?;
    let lines = ocr_lines(inner, w as u32, h as u32, &px)?;
    Ok(json!({
        "path": file.to_string_lossy(),
        "lines": lines,
        "engine": OCR_ENGINE,
    }))
}

const OCR_ENGINE: &str = "ppocrv6-medium";

#[cfg(feature = "local-infer")]
fn ocr_lines(inner: &Inner, w: u32, h: u32, px: &[u8]) -> Result<Vec<Value>, GatewayError> {
    let engine = {
        let mut guard = inner.ocr.lock().expect("ocr lock");
        match guard.as_ref() {
            Some(e) => e.clone(),
            None => {
                let loaded = engine_ocr::OcrEngine::load().map_err(|e| {
                    GatewayError::capability_missing(
                        "detect.ocr",
                        format!("PP-OCRv6 模型未安装：{e}"),
                    )
                })?;
                let arc = Arc::new(loaded);
                *guard = Some(arc.clone());
                arc
            }
        }
    };
    let lines = engine
        .run_rgb(w, h, px)
        .map_err(|e| GatewayError::internal(format!("OCR 推理失败：{e}")))?;
    Ok(lines
        .iter()
        .map(|l| {
            // score = CTC 平均字符置信度；空文本行用 box 分（协议只要求 score 可比较）
            let score = if l.text.is_empty() {
                l.box_score as f64
            } else {
                l.text_score as f64
            };
            json!({
                "text": l.text,
                "box": [l.x0, l.y0, l.x1, l.y1],
                "score": round3(score),
            })
        })
        .collect())
}

#[cfg(not(feature = "local-infer"))]
fn ocr_lines(_inner: &Inner, _w: u32, _h: u32, _px: &[u8]) -> Result<Vec<Value>, GatewayError> {
    Err(GatewayError::capability_missing(
        "detect.ocr",
        "本地推理未编入二进制（web 形态）",
    ))
}

/// 三位小数（OCR 分数），web 形态（无 OCR）下不编译进去时允许未使用。
#[cfg_attr(not(feature = "local-infer"), allow(dead_code))]
fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// `/vl/judge` 的 `rules` 载荷（1.1 单份与 1.2 多份采样共用同一形状）。
fn rule_list(reply: &VlReply) -> Vec<Value> {
    reply
        .rules
        .iter()
        .map(|r| {
            json!({
                "status": r.status,
                "label": r.label,
                "score": r.score,
            })
        })
        .collect()
}

/// `POST /vl/judge` —— VL 通道（后端由 L3 注入；未装配 → 403 `capability_missing`）。
fn vl_judge(inner: &Inner, body: &Value) -> Result<Value, GatewayError> {
    let file = req_file(body, "path")?;
    let prompt = req_str(body, "prompt")?;
    let max_tokens = match body.get("max_tokens") {
        None | Some(Value::Null) => VL_MAX_TOKENS_CAP,
        Some(v) => {
            let raw = v
                .as_u64()
                .ok_or_else(|| GatewayError::invalid("max_tokens 必须是正整数"))?;
            if raw == 0 {
                return Err(GatewayError::invalid("max_tokens 必须 > 0"));
            }
            if raw > VL_MAX_TOKENS_CAP as u64 {
                return Err(GatewayError::invalid(format!(
                    "max_tokens={raw} 超上限 {VL_MAX_TOKENS_CAP}（协议 §6）"
                )));
            }
            raw as u32
        }
    };
    let Some(backend) = inner.vl.as_ref() else {
        return Err(GatewayError::capability_missing(
            "vl.judge",
            "未配置 VL 通道（设置页选一个 VL provider 后重试）",
        ));
    };
    // 协议 1.2 增量（K07-FIX-H 翻转保护）：`samples` = 同一 (内容, prompt, 模型) 的重复
    // 采样数，缺省 1（1.1 及更早行为逐字不变）。>1 时回 `raws` 数组，由插件自己判「一致」。
    let samples = match body.get("samples") {
        None | Some(Value::Null) => 1u32,
        Some(v) => {
            let raw = v
                .as_u64()
                .ok_or_else(|| GatewayError::invalid("samples 必须是正整数"))?;
            if raw == 0 {
                return Err(GatewayError::invalid("samples 必须 > 0"));
            }
            if raw > VL_SAMPLES_CAP as u64 {
                return Err(GatewayError::invalid(format!(
                    "samples={raw} 超上限 {VL_SAMPLES_CAP}（翻转保护不需要更多）"
                )));
            }
            raw as u32
        }
    };
    if samples > 1 {
        return match backend.judge_samples(&file, prompt, max_tokens, samples) {
            Ok(replies) => Ok(json!({
                "path": file.to_string_lossy(),
                // 顶层字段保持 1.1 语义 = 第一份采样（旧插件读不到 raws 也不至于空手）
                "undetermined": replies.first().map(|r| r.undetermined).unwrap_or(true),
                "raw": replies.first().map(|r| r.raw.clone()).unwrap_or_default(),
                "rules": replies.first().map(|r| json!(rule_list(r))).unwrap_or(json!([])),
                "samples": replies.len(),
                "raws": replies.iter().map(|r| json!({
                    "undetermined": r.undetermined,
                    "raw": r.raw,
                    "reason": r.reason,
                    "rules": rule_list(r),
                })).collect::<Vec<_>>(),
            })),
            // 采样整体不可用 ≠ 判定失败：回 undetermined（协议 §7 铁律 1），插件落灰色
            Err(VlError::Unavailable(msg)) => Ok(json!({
                "path": file.to_string_lossy(),
                "undetermined": true,
                "raw": "",
                "samples": 0,
                "raws": [],
                "reason": format!("vl_unavailable: {msg}"),
            })),
            Err(VlError::BadParams(msg)) => Err(GatewayError::invalid(msg)),
        };
    }
    match backend.judge(&file, prompt, max_tokens) {
        Ok(reply) => {
            let mut payload = json!({
                "path": file.to_string_lossy(),
                "undetermined": reply.undetermined,
                "raw": reply.raw,
            });
            if let Some(reason) = reply.reason.clone() {
                payload["reason"] = json!(reason);
            }
            if !reply.rules.is_empty() {
                payload["rules"] = json!(rule_list(&reply));
            }
            Ok(payload)
        }
        // 后端不可用 ≠ 判定失败：回 undetermined，让插件落灰色待人工（协议 §7 铁律 1）
        Err(VlError::Unavailable(msg)) => Ok(json!({
            "path": file.to_string_lossy(),
            "undetermined": true,
            "raw": "",
            "reason": format!("vl_unavailable: {msg}"),
        })),
        Err(VlError::BadParams(msg)) => Err(GatewayError::invalid(msg)),
    }
}

fn report_emit(inner: &Inner, body: &Value) -> Result<Value, GatewayError> {
    let job_id = req_str(body, "job_id")?.to_string();
    let entry = body
        .get("entry")
        .cloned()
        .ok_or_else(|| GatewayError::invalid("缺字段 entry（必须是对象）"))?;
    if !entry.is_object() {
        return Err(GatewayError::invalid("entry 必须是 JSON 对象"));
    }
    inner
        .emits
        .lock()
        .expect("emits lock")
        .entry(job_id.clone())
        .or_default()
        .push(entry);
    Ok(json!({ "ok": true, "job_id": job_id }))
}

// ---------------------------------------------------------------------------
// 几何 / 生成算子的落盘实现（复用 L0/L1）
// ---------------------------------------------------------------------------

/// `/op/crop`：`[x1,y1,x2,y2]` 源图绝对像素，越界即 400（协议 §6 约定）。
fn op_crop(inner: &Inner, body: &Value) -> Result<Value, GatewayError> {
    let src = req_path(body)?;
    let out = inner.accept_out(req_str(body, "out")?)?;
    let (w, h) = core_media::image_size(&src)
        .map_err(|e| GatewayError::unreadable(format!("解码失败 {}：{e}", src.display())))?;
    let box_ = req_box(body, w, h)?;
    let (x, y) = (box_[0], box_[1]);
    let (cw, ch) = (box_[2] - box_[0], box_[3] - box_[1]);
    let (ow, oh) = core_media::crop_to(&src, &out, x, y, cw, ch).map_err(|e| match e {
        core_media::MediaError::Io(_) => GatewayError::internal(format!("写产物失败：{e}")),
        other => GatewayError::unreadable(format!("裁剪失败：{other}")),
    })?;
    Ok(json!({
        "path": out.to_string_lossy(),
        "width": ow,
        "height": oh,
        "op": "crop",
    }))
}

fn req_box(body: &Value, w: i32, h: i32) -> Result<[i32; 4], GatewayError> {
    let raw = body
        .get("box")
        .ok_or_else(|| GatewayError::invalid("缺字段 box（[x1,y1,x2,y2] 源图绝对像素）"))?;
    let arr = raw
        .as_array()
        .filter(|a| a.len() == 4)
        .ok_or_else(|| GatewayError::invalid("box 必须是长度为 4 的数组"))?;
    let mut b = [0i32; 4];
    for (i, v) in arr.iter().enumerate() {
        let n = v
            .as_f64()
            .filter(|n| n.is_finite() && n.fract() == 0.0)
            .ok_or_else(|| GatewayError::invalid(format!("box[{i}] 必须是整数")))?;
        b[i] = n as i32;
    }
    let [x1, y1, x2, y2] = b;
    if x1 < 0 || y1 < 0 || x2 > w || y2 > h || x1 >= x2 || y1 >= y2 {
        return Err(GatewayError::invalid(format!(
            "box {b:?} 越界或退化（源图 {w}x{h}，要求 0≤x1<x2≤宽、0≤y1<y2≤高）"
        )));
    }
    Ok(b)
}

/// `POST /op/resize` —— 等比缩小到长边 ≤ `max_side`（**只缩不放**，协议 1.1 增量）。
///
/// 为什么这个端点在宿主而不是包里：包不解码原图（协议 §6「像素出入口只有网关」），
/// 而「大图先缩再干活」必须动真像素 —— 宿主 `core-media` 一次就好，包内纯 Python 缩
/// 24MP 只会更慢。只缩不放是硬语义：`max_side` 大于长边时原样返回尺寸（不改画质）。
fn op_resize(inner: &Inner, body: &Value) -> Result<Value, GatewayError> {
    let src = req_path(body)?;
    let out = inner.accept_out(req_str(body, "out")?)?;
    let max_side = opt_int(body, "max_side", 0)?;
    if !(16..=8192).contains(&max_side) {
        return Err(GatewayError::invalid(format!(
            "max_side={max_side} 越界（允许 16..=8192；0/缺省不接受——不想缩就别调这个端点）"
        )));
    }
    if out == src {
        return Err(GatewayError::invalid(
            "out 不能等于 path（原地重编码会毁掉原图；原图只读，产物必须新路径）",
        ));
    }
    let (sw, sh) = core_media::image_size(&src)
        .map_err(|e| GatewayError::unreadable(format!("解码失败 {}：{e}", src.display())))?;
    let (ow, oh) = core_media::resize_to(&src, &out, max_side).map_err(|e| match e {
        core_media::MediaError::Io(_) => GatewayError::internal(format!("写产物失败：{e}")),
        other => GatewayError::unreadable(format!("缩放失败：{other}（{}）", src.display())),
    })?;
    Ok(json!({
        "path": out.to_string_lossy(),
        "width": ow,
        "height": oh,
        "source_width": sw,
        "source_height": sh,
        "scaled": (ow, oh) != (sw, sh),
        "max_side": max_side,
        "op": "resize",
    }))
}

/// `/op/letterbox`：按目标宽高比补边（不缩放、不裁切），产物尺寸 = 内接该比的最小画布。
fn op_letterbox(inner: &Inner, body: &Value) -> Result<Value, GatewayError> {
    let src = req_path(body)?;
    let out = inner.accept_out(req_str(body, "out")?)?;
    let ratio = req_number(body, "target_ratio")?;
    if !(0.05..=20.0).contains(&ratio) {
        return Err(GatewayError::invalid(format!(
            "target_ratio={ratio} 越界（允许 0.05..=20，宽/高）"
        )));
    }
    let bg = match body.get("background").and_then(Value::as_str) {
        Some(raw) => parse_color(raw)?,
        None => [255, 255, 255],
    };
    let (w, h, px) = core_media::rgb8(&src)
        .map_err(|e| GatewayError::unreadable(format!("解码失败 {}：{e}", src.display())))?;
    if w <= 0 || h <= 0 {
        return Err(GatewayError::unreadable(format!("非法图片尺寸 {w}x{h}")));
    }
    let (cw, ch) = letterbox_canvas(w, h, ratio)?;
    let mut canvas = vec![0u8; (cw as usize) * (ch as usize) * 3];
    for i in (0..canvas.len()).step_by(3) {
        canvas[i] = bg[0];
        canvas[i + 1] = bg[1];
        canvas[i + 2] = bg[2];
    }
    let off_x = (cw - w) / 2;
    let off_y = (ch - h) / 2;
    for y in 0..h as usize {
        let src_row = y * w as usize * 3;
        let dst_row = ((y + off_y as usize) * cw as usize + off_x as usize) * 3;
        canvas[dst_row..dst_row + w as usize * 3]
            .copy_from_slice(&px[src_row..src_row + w as usize * 3]);
    }
    let (ow, oh) = core_media::encode_rgb_to(cw, ch, &canvas, &out, None)
        .map_err(|e| GatewayError::internal(format!("写 letterbox 产物失败：{e}")))?;
    Ok(json!({
        "path": out.to_string_lossy(),
        "width": ow,
        "height": oh,
        "op": "letterbox",
        "target_ratio": ratio,
    }))
}

/// `/op/encode`：**编码动作**（转格式 / 质量档 / alpha 保留 / EXIF 方向摆正）。
///
/// 为什么宿主出这个端点（N6b 实测结论，见 `imaging-plugin-codec.md`）：
/// 插件零第三方依赖、不自带编解码库，宿主也没有「按质量重编码」的端点——
/// `/op/crop`·`/op/letterbox`·`/op/upscale` 都按源扩展名/默认质量落盘，
/// 于是「转 avif」「压到 200KB」在协议面上无路可走。本端点只做一件事：
/// **把一张图解码 → （可选）按 EXIF 摆正 → 按目标格式/质量/alpha 策略重编码**，
/// 迭代逼近（compress 的质量搜索与降采样）留给插件编排（插件拥有策略，宿主拥有像素）。
///
/// 诚实语义（不许静默降级）：
/// - `format` 与 `out` 扩展名不一致 → **400**（不猜用户想要哪个）；
/// - 目标容器存不了 alpha（jpeg）→ 按 `background`（默认白）合成，回 `alpha="flattened"`；
/// - `quality` 对 png/webp 无意义（无损容器）→ 回 `quality_applied=false`，不假装生效；
/// - 解码失败 → 404 `asset_unreadable`；越界产物路径 → 400 `out_of_workroot`。
fn op_encode(inner: &Inner, body: &Value) -> Result<Value, GatewayError> {
    let src = req_path(body)?;
    let out_raw = req_str(body, "out")?;
    let out = inner.accept_out(out_raw)?;
    let out_ext = out.extension().and_then(|e| e.to_str()).ok_or_else(|| {
        GatewayError::invalid(format!("out 无扩展名，无法判定目标格式：{out_raw}"))
    })?;
    let out_format = core_media::OutFormat::from_ext(out_ext).map_err(|e| {
        GatewayError::invalid(format!(
            "out 扩展名 .{out_ext} 不是可编码格式（jpg/png/webp/avif）：{e}"
        ))
    })?;
    if let Some(raw) = body.get("format").and_then(Value::as_str) {
        let want = core_media::OutFormat::from_ext(raw)
            .map_err(|e| GatewayError::invalid(format!("format={raw} 不合法：{e}")))?;
        if want != out_format {
            return Err(GatewayError::invalid(format!(
                "format={raw}（{:?}）与 out 扩展名 .{out_ext}（{:?}）不一致——产物格式只认一个口径",
                want, out_format
            )));
        }
    }
    let quality = match body.get("quality") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let q = v
                .as_u64()
                .filter(|q| (1..=100).contains(q))
                .ok_or_else(|| {
                    GatewayError::invalid(format!("quality 必须是 1..=100 的整数，收到 {v}"))
                })?;
            Some(q as u8)
        }
    };
    let background = match body.get("background").and_then(Value::as_str) {
        Some(raw) => parse_color(raw)?,
        None => [255, 255, 255],
    };
    let auto_orient = body
        .get("auto_orient")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    // out 不许覆盖源：accept_out 会把父目录规范化（macOS `/tmp` → `/private/tmp`），
    // 所以两边都按 canonical 比对，别让「同一个文件两种写法」绕过原图只读。
    let src_canon = src.canonicalize().unwrap_or_else(|_| src.clone());
    if out == src || out == src_canon {
        return Err(GatewayError::invalid(
            "out 不能等于 path（原地重编码会毁掉原图；原图只读，产物必须新路径）",
        ));
    }

    let (w0, h0, raw_px) = core_media::rgba8(&src)
        .map_err(|e| GatewayError::unreadable(format!("解码失败 {}：{e}", src.display())))?;
    if w0 <= 0 || h0 <= 0 {
        return Err(GatewayError::unreadable(format!("非法图片尺寸 {w0}x{h0}")));
    }
    if (w0 as u64) * (h0 as u64) > MAX_OUTPUT_PIXELS {
        return Err(GatewayError::invalid(format!(
            "源图 {w0}x{h0} 超过像素上限 {MAX_OUTPUT_PIXELS}"
        )));
    }

    // EXIF 方向：只在请求里要（默认要）且源真带方向时才动像素
    let src_ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let orientation = if auto_orient {
        core_media::orientation_of_file(&src, &src_ext)
            .filter(|o| (1..=8).contains(o))
            .unwrap_or(1)
    } else {
        1
    };
    let (w, h, px) = core_media::orient_rgba(w0, h0, &raw_px, orientation as u32)
        .map_err(|e| GatewayError::internal(format!("按 EXIF 方向摆正失败：{e}")))?;

    let source_alpha = core_media::has_transparency(&px);
    let alpha = if !source_alpha {
        "absent"
    } else if out_format.supports_alpha() {
        "kept"
    } else {
        "flattened"
    };
    let bytes_before = std::fs::metadata(&src).map(|m| m.len()).unwrap_or(0);
    core_media::encode_rgba_to(
        w,
        h,
        &px,
        &out,
        quality,
        if out_format.supports_alpha() {
            None
        } else {
            Some(background)
        },
    )
    .map_err(|e| GatewayError::internal(format!("编码产物失败 {}：{e}", out.display())))?;
    let bytes = std::fs::metadata(&out)
        .map_err(|e| GatewayError::internal(format!("产物写盘后读不到：{e}")))?
        .len();

    Ok(json!({
        "path": out.to_string_lossy(),
        "width": w,
        "height": h,
        "format": out_format.ext(),
        "bytes": bytes,
        "bytes_in": bytes_before,
        "source_alpha": source_alpha,
        "alpha": alpha,
        "quality_applied": quality.is_some() && out_format.quality_applies(),
        "quality": quality,
        "orientation": orientation,
        "op": "encode",
    }))
}

/// 内接目标宽高比的最小画布（只补边，不缩放）：返回 (宽, 高)。
fn letterbox_canvas(w: i32, h: i32, ratio: f64) -> Result<(i32, i32), GatewayError> {
    let (cw, ch) = {
        let w_f = w as f64;
        let h_f = h as f64;
        if w_f / h_f > ratio {
            // 图比目标更宽 → 竖直补边
            (w, (w_f / ratio).round() as i32)
        } else {
            // 图比目标更高（或相等）→ 水平补边
            ((h_f * ratio).round() as i32, h)
        }
    };
    let (cw, ch) = (cw.max(w).max(1), ch.max(h).max(1));
    if (cw as u64) * (ch as u64) > MAX_OUTPUT_PIXELS {
        return Err(GatewayError::invalid(format!(
            "letterbox 画布 {cw}x{ch} 超过像素上限 {MAX_OUTPUT_PIXELS}"
        )));
    }
    Ok((cw, ch))
}

/// `/op/watermark`：文字水印叠加（协议 1.1 追加端点，能力 `op.overlay`）。
///
/// 复用 `core-media::watermark_text`（ab_glyph + 内嵌 Noto Sans SC，OFL 1.1）——宿主不加新依赖，
/// 插件不必自写字体光栅化/JPEG 编解码（处理类插件「零第三方依赖」这条铁律不能靠插件自己破）。
/// 尺寸不变（叠加不是缩放）；参数/落位口径与旧 C4 `watermark` 变换逐字相同，
/// 老规则迁移到插件后产物一致。
///
/// 请求 `{path,text,out,position?,opacity?,font_size?,margin?,color?}`：
/// - `position` ∈ `top_left|top_right|center|bottom_left|bottom_right`（缺省 `bottom_right`）
/// - `opacity` 1..=100（整数百分比，缺省 50；0 会出无墨产物 → 400）
/// - `font_size` 8..=512 px（缺省 48）；`margin` ≥0 且 `< min(w,h)/2` px（缺省 16）
/// - `color` 六位 `rrggbb`（缺省 `ffffff`）
///
/// 应答 `{path,width,height,op:"watermark",coverage}`；参数非法 → 400（插件回落 `-32003`），
/// 解码失败 → 404 `asset_unreadable`（`-32002`），字形缺失/单行超宽一律响亮失败。
fn op_watermark(inner: &Inner, body: &Value) -> Result<Value, GatewayError> {
    let src = req_path(body)?;
    let out = inner.accept_out(req_str(body, "out")?)?;
    let text = req_str(body, "text")?.to_string();
    if text.contains('\n') {
        return Err(GatewayError::invalid(
            "text 只支持单行（换行请拆成多次叠加）".to_string(),
        ));
    }
    let position_raw = body
        .get("position")
        .and_then(Value::as_str)
        .unwrap_or("bottom_right");
    let position = core_media::WatermarkPosition::parse(position_raw)
        .map_err(|e| GatewayError::invalid(format!("position 不合法：{e}")))?;
    let opacity = opt_int(body, "opacity", 50)?;
    if !(1..=100).contains(&opacity) {
        return Err(GatewayError::invalid(format!(
            "opacity={opacity} 越界（允许 1..=100 百分比；0 等于不出墨）"
        )));
    }
    let font_size = opt_number(body, "font_size", 48.0)?;
    if !(8.0..=512.0).contains(&font_size) {
        return Err(GatewayError::invalid(format!(
            "font_size={font_size} 越界（允许 8..=512 px）"
        )));
    }
    let margin = opt_int(body, "margin", 16)?;
    if margin < 0 {
        return Err(GatewayError::invalid(format!("margin={margin} 不能为负")));
    }
    let color = match body.get("color").and_then(Value::as_str) {
        Some(raw) => parse_color(raw)?,
        None => [255, 255, 255],
    };
    let (w, h) = core_media::image_size(&src)
        .map_err(|e| GatewayError::unreadable(format!("解码失败 {}：{e}", src.display())))?;
    if (margin as i64) * 2 >= i64::from(w.min(h)) {
        return Err(GatewayError::invalid(format!(
            "margin={margin} 过大（图 {w}x{h}，要求 2×margin < 短边）"
        )));
    }
    let wm = core_media::TextWatermark {
        text: text.clone(),
        position,
        opacity: opacity as u8,
        font_size: font_size as f32,
        margin,
        color,
    };
    let (ow, oh) = core_media::watermark_text(&src, &out, &wm).map_err(|e| match e {
        core_media::MediaError::Io(_) => GatewayError::internal(format!("写产物失败：{e}")),
        other => GatewayError::unreadable(format!("水印渲染失败：{other}")),
    })?;
    Ok(json!({
        "path": out.to_string_lossy(),
        "width": ow,
        "height": oh,
        "op": "watermark",
        "coverage": core_media::coverage(&text, font_size as f32),
    }))
}

/// `/op/upscale`：`scale` 可以超过单趟上限（内部拆成多趟，每趟 ≤ [`UPSCALE_MAX_STEP`]）。
///
/// 旧实现把 `scale ≤ 8` 当参数硬门槛直接 400，于是「小图 + 小主体」的几何修复
/// （`554x500` 的 `f005`：`ideal≈86px` → `scale≈18.6`）根本走不到放大，
/// 插件只能收到一句裸 `invalid_params`（K07 §5.2 / D7）。现在：
/// 1. `scale` 只受**画布硬门槛**约束（像素 ≤ [`MAX_OUTPUT_PIXELS`]、长边 ≤ [`UPSCALE_MAX_LONG_EDGE`]）
///    与防呆总倍数 [`UPSCALE_MAX_TOTAL`]；超硬门槛时错误里给出**可用上限 + 建议参数**；
/// 2. 放大按趟执行：每趟长边 ×≤8，末趟精确落到目标长边；`scale==4` 且模型在盘时
///    单趟走 x4v3（与旧行为一致，`op` 仍报 `upscale.realesr-x4v3`）；
/// 3. 应答带 `passes`/`steps`/`scale_max`，插件与报告能看清「几趟放大、每趟多大、上限多少」。
fn op_upscale(inner: &Inner, body: &Value) -> Result<Value, GatewayError> {
    let src = req_path(body)?;
    let out = inner.accept_out(req_str(body, "out")?)?;
    let scale = req_number(body, "scale")?;
    if !scale.is_finite() || !(0.1..=UPSCALE_MAX_TOTAL).contains(&scale) {
        return Err(GatewayError::invalid(format!(
            "scale={scale} 越界（允许 0.1..={UPSCALE_MAX_TOTAL}；放大超过单趟上限会自动分趟）"
        )));
    }
    let (w, h) = core_media::image_size(&src)
        .map_err(|e| GatewayError::unreadable(format!("解码失败 {}：{e}", src.display())))?;
    if w <= 0 || h <= 0 {
        return Err(GatewayError::unreadable(format!("非法图片尺寸 {w}x{h}")));
    }
    let limits = upscale_limits(w, h);
    let target_w = ((w as f64) * scale).round() as i64;
    let target_h = ((h as f64) * scale).round() as i64;
    let target_long = ((w.max(h) as f64) * scale).round() as i64;
    if target_w <= 0 || target_h <= 0 || target_long <= 0 {
        return Err(GatewayError::invalid(format!(
            "scale={scale} 过小，目标边长为 0（源图 {w}x{h}）"
        )));
    }
    if target_w * target_h > MAX_OUTPUT_PIXELS as i64 || target_long > UPSCALE_MAX_LONG_EDGE {
        return Err(GatewayError::invalid(format!(
            "upscale 目标 {target_w}x{target_h}（长边 {target_long}）超上限：像素 ≤{MAX_OUTPUT_PIXELS}、\
             长边 ≤{UPSCALE_MAX_LONG_EDGE}；源图 {w}x{h} 的可用上限 scale<={:.3}\
             （短边可达 {}px、长边可达 {}px），请调小 scale 或先缩小源图",
            limits.max_scale, limits.max_short_side, limits.max_long_edge
        )));
    }

    // 缩小 / 1:1：单趟重采样（无模型路径，模型只做 4× 放大）
    if scale <= 1.0 + 1e-9 {
        let (ow, oh) = core_media::scale_to(&src, &out, target_long as i32)
            .map_err(|e| GatewayError::internal(format!("重采样失败：{e}")))?;
        return Ok(json!({
            "path": out.to_string_lossy(),
            "width": ow,
            "height": oh,
            "op": "upscale.resample",
            "scale": scale,
            "passes": 1,
            "steps": [{ "from_long": w.max(h), "to_long": ow.max(oh), "mode": "resample" }],
            "scale_max": limits.max_scale,
        }));
    }

    // 分趟目标：每趟长边最多 ×8，末趟精确落到 target_long
    let mut step_longs: Vec<i64> = Vec::new();
    let mut cur_long = w.max(h) as f64;
    while (target_long as f64) > cur_long * (1.0 + 1e-9) {
        let remain = target_long as f64 / cur_long;
        let next = if remain <= UPSCALE_MAX_STEP {
            target_long as f64
        } else {
            (cur_long * UPSCALE_MAX_STEP).round()
        };
        let next = next.max(cur_long + 1.0).min(target_long as f64) as i64;
        step_longs.push(next);
        cur_long = next as f64;
    }

    let stem = out
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("upscale")
        .to_string();
    let dir = out
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut cur = src.clone();
    let mut steps: Vec<Value> = Vec::new();
    let mut last_mode = "resample";
    for (idx, to_long) in step_longs.iter().copied().enumerate() {
        let last = idx + 1 == step_longs.len();
        // 中间产物与产物同目录（已过写根校验），末趟直接写 out
        let dest = if last {
            out.clone()
        } else {
            dir.join(format!(".{stem}.pass{}.png", idx + 1))
        };
        let (cur_w, cur_h) = core_media::image_size(&cur)
            .map_err(|e| GatewayError::unreadable(format!("解码失败 {}：{e}", cur.display())))?;
        let factor = to_long as f64 / cur_w.max(cur_h).max(1) as f64;
        let mut mode = "resample";
        let mut dims: Option<(i32, i32)> = None;
        // 单趟恰为 4× 且输入 ≤ 模型护栏 → 走 x4v3（与旧行为逐值一致）
        if (factor - 4.0).abs() < 1e-6
            && (cur_w as u64) * (cur_h as u64) <= upscale_model_input_cap()
        {
            if let Some((ow, oh)) = upscale_by_model(&cur, &dest, factor)? {
                mode = "realesr-x4v3";
                dims = Some((ow, oh));
            }
        }
        let (ow, oh) = match dims {
            Some(d) => d,
            None => core_media::scale_to(&cur, &dest, to_long as i32)
                .map_err(|e| GatewayError::internal(format!("重采样失败：{e}")))?,
        };
        steps.push(json!({
            "from_long": cur_w.max(cur_h),
            "to_long": ow.max(oh),
            "width": ow,
            "height": oh,
            "mode": mode,
        }));
        last_mode = mode;
        // 上一趟的中间产物用完即删（源图绝不能删；产物就是最后一趟的 out）
        if cur != src {
            let _ = std::fs::remove_file(&cur);
        }
        cur = dest;
    }
    let (ow, oh) = core_media::image_size(&out)
        .map_err(|e| GatewayError::unreadable(format!("产物不可读 {}：{e}", out.display())))?;
    let op = if step_longs.len() > 1 {
        "upscale.staged"
    } else if last_mode == "realesr-x4v3" {
        "upscale.realesr-x4v3"
    } else {
        "upscale.resample"
    };
    Ok(json!({
        "path": out.to_string_lossy(),
        "width": ow,
        "height": oh,
        "op": op,
        "scale": scale,
        "passes": step_longs.len(),
        "steps": steps,
        "scale_max": limits.max_scale,
    }))
}

/// x4v3 的输入像素护栏（有本地推理时取引擎常量，纯几何构建时用同值——只为分趟决策，
/// 不参与任何算法；模型不可用时 `upscale_by_model` 直接回 `None`）。
#[cfg(feature = "local-infer")]
fn upscale_model_input_cap() -> u64 {
    engine_restore::upscale::MAX_INPUT_PIXELS
}
#[cfg(not(feature = "local-infer"))]
fn upscale_model_input_cap() -> u64 {
    4_000_000
}

#[cfg(feature = "local-infer")]
fn upscale_by_model(
    src: &Path,
    out: &Path,
    scale: f64,
) -> Result<Option<(i32, i32)>, GatewayError> {
    // x4v3 是固定 4 倍的权重；`scale` ≠ 4 时用重采样，避免「要 2 倍却出 4 倍」的静默偏差
    if (scale - 4.0).abs() > 1e-9
        || !engine_restore::model_ready(engine_restore::REALESR_GENERAL_X4V3)
    {
        return Ok(None);
    }
    let up = engine_restore::upscale::upscaler(engine_restore::REALESR_GENERAL_X4V3)
        .map_err(|e| GatewayError::internal(format!("加载超分模型失败：{e}")))?;
    let (w, h, px) = core_media::rgb8(src)
        .map_err(|e| GatewayError::unreadable(format!("解码失败 {}：{e}", src.display())))?;
    let (ow, oh, buf) = up
        .run_rgb(w as u32, h as u32, &px)
        .map_err(|e| GatewayError::internal(format!("超分推理失败：{e}")))?;
    let (ow, oh) = core_media::encode_rgb_to(ow as i32, oh as i32, &buf, out, None)
        .map_err(|e| GatewayError::internal(format!("写超分产物失败：{e}")))?;
    Ok(Some((ow, oh)))
}

#[cfg(not(feature = "local-infer"))]
fn upscale_by_model(
    _src: &Path,
    _out: &Path,
    _scale: f64,
) -> Result<Option<(i32, i32)>, GatewayError> {
    Ok(None)
}

/// `/op/replace_bg`：u2net 掩码 → 背景换纯色（与 C4 `replace_bg` 修复器同口径）。
fn op_replace_bg(inner: &Inner, body: &Value) -> Result<Value, GatewayError> {
    let src = req_path(body)?;
    let out = inner.accept_out(req_str(body, "out")?)?;
    let bg = match body.get("color").and_then(Value::as_str) {
        Some(raw) => parse_color(raw)?,
        None => [255, 255, 255],
    };
    let (w, h, px) = core_media::rgb8(&src)
        .map_err(|e| GatewayError::unreadable(format!("解码失败 {}：{e}", src.display())))?;
    if w <= 0 || h <= 0 {
        return Err(GatewayError::unreadable(format!("非法图片尺寸 {w}x{h}")));
    }
    let mask = segment_run(inner, "op.generative", &src, w as u32, h as u32, &px)?.1;
    let mut composited = px;
    for (i, &m) in mask.iter().enumerate() {
        if m == 0 {
            composited[i * 3] = bg[0];
            composited[i * 3 + 1] = bg[1];
            composited[i * 3 + 2] = bg[2];
        }
    }
    let (ow, oh) = core_media::encode_rgb_to(w, h, &composited, &out, None)
        .map_err(|e| GatewayError::internal(format!("写 replace_bg 产物失败：{e}")))?;
    Ok(json!({
        "path": out.to_string_lossy(),
        "width": ow,
        "height": oh,
        "op": "replace_bg",
    }))
}

/// `/op/inpaint`：`mask` 必须是与原图同宽高的图像文件（>127 视为待补洞）。
fn op_inpaint(inner: &Inner, body: &Value) -> Result<Value, GatewayError> {
    let src = req_path(body)?;
    let out = inner.accept_out(req_str(body, "out")?)?;
    let mask_path = req_file(body, "mask")?;
    let (w, h) = core_media::image_size(&src)
        .map_err(|e| GatewayError::unreadable(format!("解码失败 {}：{e}", src.display())))?;
    if w <= 0 || h <= 0 {
        return Err(GatewayError::unreadable(format!("非法图片尺寸 {w}x{h}")));
    }
    inpaint_run(&src, &mask_path, &out, w as u32, h as u32)
}

#[cfg(feature = "local-infer")]
fn inpaint_run(
    src: &Path,
    mask_path: &Path,
    out: &Path,
    w: u32,
    h: u32,
) -> Result<Value, GatewayError> {
    let expect = (w as usize) * (h as usize);
    let mask = engine_restore::inpaint::load_mask_path(mask_path, expect)
        .map_err(|e| GatewayError::invalid(format!("掩码不合法 {}：{e}", mask_path.display())))?;
    let painter =
        engine_restore::inpaint::inpainter(engine_restore::LAMA_FP32).map_err(|e| match e {
            engine_restore::RestoreError::ModelMissing(m) => GatewayError::capability_missing(
                "op.generative",
                format!("lama-fp32 模型未安装：{m}"),
            ),
            other => GatewayError::internal(format!("加载 lama 失败：{other}")),
        })?;
    let (iw, ih, px) = core_media::rgb8(src)
        .map_err(|e| GatewayError::unreadable(format!("解码失败 {}：{e}", src.display())))?;
    let buf = painter
        .run_rgb_mask(iw as u32, ih as u32, &px, &mask)
        .map_err(|e| GatewayError::internal(format!("lama 推理失败：{e}")))?;
    let (ow, oh) = core_media::encode_rgb_to(iw, ih, &buf, out, None)
        .map_err(|e| GatewayError::internal(format!("写 inpaint 产物失败：{e}")))?;
    Ok(json!({
        "path": out.to_string_lossy(),
        "width": ow,
        "height": oh,
        "op": "inpaint",
    }))
}

#[cfg(not(feature = "local-infer"))]
fn inpaint_run(
    _src: &Path,
    _mask_path: &Path,
    _out: &Path,
    _w: u32,
    _h: u32,
) -> Result<Value, GatewayError> {
    Err(GatewayError::capability_missing(
        "op.generative",
        "本地推理未编入二进制（web 形态）",
    ))
}

/// 掩码框 → 像素框（空 → `null` 而不是编造框）。
///
/// 抽成纯函数是为了让「空掩码 / 全前景 / 只贴一条边」三类边界在**不需要模型**的单测里
/// 也能验（`/detect/bbox` 的 HTTP 面对这三类要靠真 u2net 才跑得出来）。
fn mask_boxes(
    bbox: Option<[usize; 4]>,
    dominant: Option<[usize; 4]>,
    w: u32,
    h: u32,
) -> (Value, Value) {
    let to_json = |b: Option<[usize; 4]>| match b {
        Some(b) => json!(grid_box_to_pixels(b, w, h)),
        None => Value::Null,
    };
    (to_json(bbox), to_json(dominant))
}

/// `/detect/bbox` 的 `instances` 缺省门槛：连通域前景 cell 数 ≥ 画面 1%。
///
/// 口径来自真实语料（K07 咖啡胶囊 f002，`r3_probe` 实测）：主商品连通域占 32.1%、
/// 第二件商品占 4.0%，其余都是 u2net 散点噪声（≤0.3%）。1% 能把「第二件商品」留下、
/// 把噪声滤掉；插件侧判「文字是否落在商品上」只认这档框。
pub const DEFAULT_MIN_INSTANCE_RATIO: f64 = 0.01;
/// 单次最多回多少个实例（多商品图够用；防一屏散点把应答撑大）。
pub const MAX_INSTANCES: usize = 8;

/// 掩码全部连通域 → `instances`（多实例主体）：`[{"bbox": [...px], "area_ratio": r}]`。
///
/// `area_ratio` 是该连通域前景 cell 数占画面的比例（与 `fg_ratio` 同网格口径，可加总核对）；
/// 按面积降序、只留 `min_ratio` 以上的、最多 [`MAX_INSTANCES`] 个 —— 顺序稳定（同面积按扫描序），
/// 所以同图同参数同结果（确定性口径，插件据此判文字归属）。
fn mask_instances(
    mask: &engine_segment::Mask,
    w: u32,
    h: u32,
    min_ratio: f64,
    cap: usize,
) -> Value {
    let cells = (mask.side * mask.side) as f64;
    let items: Vec<Value> = mask
        .components()
        .into_iter()
        .filter(|c| (c.area as f64) / cells >= min_ratio)
        .take(cap)
        .map(|c| {
            json!({
                "bbox": grid_box_to_pixels(c.bbox, w, h),
                "area_ratio": (c.area as f64) / cells,
            })
        })
        .collect();
    Value::Array(items)
}

#[cfg(test)]
mod bbox_tests {
    use super::*;
    use engine_segment::{Mask, Segmentation, SIDE};

    fn mask_with(f: impl Fn(usize, usize) -> u8) -> Mask {
        let mut data = vec![0u8; SIDE * SIDE];
        for y in 0..SIDE {
            for x in 0..SIDE {
                data[y * SIDE + x] = f(x, y);
            }
        }
        Mask { side: SIDE, data }
    }

    fn seg(mask: Mask) -> Segmentation {
        Segmentation {
            fg_ratio: mask.fg_ratio(),
            bbox: mask.bbox(),
            mask,
        }
    }

    /// 多实例：`instances` 按面积降序、滤掉 < min_ratio 的噪声域、上限截断，area_ratio 同网格口径。
    #[test]
    fn instances_list_all_subjects_by_area() {
        // 主商品：100x100 = 10000 cell（320²=102400 → 9.77%）；第二件：40x40=1600（1.56%）；
        // 噪声：4x4=16（0.016%）
        let mask = mask_with(|x, y| {
            let big = (10..110).contains(&x) && (10..110).contains(&y);
            let second = (200..240).contains(&x) && (150..190).contains(&y);
            let noise = (300..304).contains(&x) && (300..304).contains(&y);
            u8::from(big || second || noise)
        });
        let inst = mask_instances(&mask, 640, 480, DEFAULT_MIN_INSTANCE_RATIO, MAX_INSTANCES);
        let arr = inst.as_array().expect("数组");
        assert_eq!(arr.len(), 2, "噪声域必须被 min_ratio 滤掉：{inst}");
        assert_eq!(
            arr[0]["bbox"],
            json!(grid_box_to_pixels([10, 10, 109, 109], 640, 480))
        );
        let r0 = arr[0]["area_ratio"].as_f64().unwrap();
        let r1 = arr[1]["area_ratio"].as_f64().unwrap();
        assert!((r0 - 10000.0 / 102400.0).abs() < 1e-12, "{r0}");
        assert!(r0 > r1, "必须面积降序：{inst}");
        assert!((r1 - 1600.0 / 102400.0).abs() < 1e-12, "{r1}");
        // 上限截断（cap=1 → 只回主主体，且与 dominant_bbox 一致）
        let one = mask_instances(&mask, 640, 480, DEFAULT_MIN_INSTANCE_RATIO, 1);
        assert_eq!(one.as_array().unwrap().len(), 1);
        assert_eq!(
            one[0]["bbox"],
            json!(grid_box_to_pixels(
                *mask.dominant_bbox().as_ref().unwrap(),
                640,
                480
            ))
        );
        // 空掩码 → 空数组（不是 null：插件对 `[]` 与缺字段同样退回单主体口径）
        assert_eq!(
            mask_instances(&mask_with(|_, _| 0), 640, 480, 0.01, 8),
            json!([])
        );
    }

    /// 空掩码：三个框都必须 `null`（不编造 (0,0,0,0) 这种看起来像真的值）。
    #[test]
    fn empty_mask_yields_nulls() {
        let seg = seg(mask_with(|_, _| 0));
        let (bbox, dominant) = mask_boxes(seg.mask.bbox(), seg.mask.dominant_bbox(), 800, 600);
        assert_eq!(bbox, Value::Null);
        assert_eq!(dominant, Value::Null);
        // 没有前景 → 也不该给裁剪窗（插件据此判「几何无解」）
        assert!(seg.crop_window(0.75).is_none());
    }

    /// 全前景：框必须覆盖整幅画布（右/下边界用向上取整，不能少一像素）。
    #[test]
    fn full_foreground_covers_whole_canvas() {
        let seg = seg(mask_with(|_, _| 1));
        let (bbox, dominant) = mask_boxes(seg.mask.bbox(), seg.mask.dominant_bbox(), 1280, 960);
        assert_eq!(bbox, json!([0, 0, 1280, 960]));
        assert_eq!(dominant, json!([0, 0, 1280, 960]));
        // 主体占满 → 窗就是全图，占比 1.0（裁剪无法再降占比）
        let (win, ratio) = seg.crop_window(0.75).expect("全前景也有窗");
        assert_eq!(win, [0, 0, SIDE - 1, SIDE - 1]);
        assert!((ratio - 1.0).abs() < 1e-9, "{ratio}");
    }

    /// 只贴左边一条：框的 x1 必须落在 0，且不越界（右/下不动）。
    #[test]
    fn edge_only_box_touches_border() {
        let seg = seg(mask_with(|x, _| if x < 8 { 1 } else { 0 }));
        let (bbox, dominant) = mask_boxes(seg.mask.bbox(), seg.mask.dominant_bbox(), 640, 480);
        let arr = bbox.as_array().expect("有前景必须给框");
        assert_eq!(arr[0], json!(0), "左边界必须贴 0：{bbox}");
        let x2 = arr[2].as_u64().unwrap();
        assert!(x2 > 0 && x2 < 640, "右边界必须落在画布内：{bbox}");
        assert_eq!(arr[1], json!(0));
        assert_eq!(arr[3], json!(480), "竖直方向铺满：{bbox}");
        assert_eq!(dominant, bbox);
    }

    /// 网格 → 像素的映射口径：cell g 覆盖 [g*W/side, (g+1)*W/side)，右/下向上取整。
    #[test]
    fn grid_box_maps_to_pixels_within_canvas() {
        assert_eq!(
            grid_box_to_pixels([0, 0, SIDE - 1, SIDE - 1], 100, 50),
            [0, 0, 100, 50]
        );
        let b = grid_box_to_pixels([SIDE / 2, SIDE / 2, SIDE / 2, SIDE / 2], 640, 480);
        assert_eq!(
            b,
            [320, 240, 322, 242],
            "单个网格 cell 至少占 1 像素：{b:?}"
        );
        // 极小图也不会退化出零宽框
        let b = grid_box_to_pixels([10, 10, 12, 12], 7, 5);
        assert!(b[2] > b[0] && b[3] > b[1], "{b:?}");
        assert!(b[2] <= 7 && b[3] <= 5, "{b:?}");
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    fn tmpfile(name: &str, bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pack-gateway-cache-{}-{}",
            std::process::id(),
            name
        ));
        std::fs::create_dir_all(dir.parent().unwrap()).ok();
        std::fs::write(&dir, bytes).expect("写临时文件");
        dir
    }

    /// 键的语义：**内容 + 参数**决定身份，路径名不算身份。
    #[test]
    fn detect_cache_key_is_content_and_params_based() {
        let a = tmpfile("k1-a.bin", b"same-bytes");
        let b = tmpfile("k1-b.bin", b"same-bytes");
        let c = tmpfile("k1-c.bin", b"other-bytes");
        let ka = detect_cache_key("/detect/ocr", &json!({ "path": a.to_string_lossy() })).unwrap();
        // 同内容不同路径 → 同一个键（同一个文件换个名字问是同一件事）
        assert_eq!(
            ka,
            detect_cache_key("/detect/ocr", &json!({ "path": b.to_string_lossy() })).unwrap()
        );
        // 内容变了 → 键变
        assert_ne!(
            ka,
            detect_cache_key("/detect/ocr", &json!({ "path": c.to_string_lossy() })).unwrap()
        );
        // 端点/参数变了 → 键变
        assert_ne!(
            ka,
            detect_cache_key("/detect/u2net", &json!({ "path": a.to_string_lossy() })).unwrap()
        );
        assert_ne!(
            ka,
            detect_cache_key(
                "/detect/ocr",
                &json!({ "path": a.to_string_lossy(), "lang": "eng" })
            )
            .unwrap()
        );
        // 参数键序无关（同一组参数换个写法仍是同一个键）
        let x = detect_cache_key(
            "/detect/ocr",
            &json!({ "path": a.to_string_lossy(), "lang": "chi_sim+eng", "n": 1 }),
        )
        .unwrap();
        let y = detect_cache_key(
            "/detect/ocr",
            &json!({ "n": 1, "lang": "chi_sim+eng", "path": a.to_string_lossy() }),
        )
        .unwrap();
        assert_eq!(x, y);
        // 文件不存在 → None（不缓存，调用方照常算）
        assert!(detect_cache_key("/detect/ocr", &json!({ "path": "/nope/never.png" })).is_none());
        assert!(detect_cache_key("/detect/ocr", &json!({})).is_none());
        for p in [a, b, c] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn detect_cache_is_bounded_and_respects_zero_cap() {
        let mut off = DetectCache::new(0);
        off.put("k".into(), json!({ "v": 1 }));
        assert!(off.get("k").is_none(), "容量 0 = 关闭");

        let mut cache = DetectCache::new(2);
        cache.put("k1".into(), json!({ "v": 1 }));
        cache.put("k2".into(), json!({ "v": 2 }));
        cache.put("k3".into(), json!({ "v": 3 }));
        assert_eq!(cache.map.len(), 2, "超容量必须淘汰");
        assert!(cache.get("k1").is_none(), "最旧的先走");
        assert!(cache.get("k3").is_some());
    }
}

#[cfg(test)]
mod conn_semantics_tests {
    use super::*;
    use std::net::TcpListener;

    /// KI-29 回归（真机缺陷）：`accept()` 出来的连接必须按**阻塞**语义使用。
    ///
    /// Windows 的 accepted SOCKET 会继承监听 socket 的非阻塞模式（Linux 不继承），
    /// 而 `handle_conn_inner` 的读写全按阻塞语义写：修复前在 Windows 上这里读到的是
    /// `WouldBlock`，随后的部分写 + 立即 close 就是客户端看到的 `WSAECONNABORTED(10053)`。
    /// 本用例在 Linux 上是平凡通过（Linux 本来就不继承），在 Windows 上修复前必红。
    #[test]
    fn accepted_conn_is_blocking_after_prepare() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("set_nonblocking");
        let addr = listener.local_addr().expect("local_addr");

        let client = thread::spawn(move || {
            let s = TcpStream::connect(addr).expect("connect");
            // 故意什么都不发：服务端那侧应当是「等超时」，不是「立刻 WouldBlock」
            thread::sleep(Duration::from_millis(300));
            drop(s);
        });

        let (mut server, _) = loop {
            match listener.accept() {
                Ok(v) => break v,
                Err(_) => thread::sleep(ACCEPT_POLL),
            }
        };
        prepare_conn(&server).expect("prepare_conn");

        // 把 30s 的正式读超时压到 150ms，只为让用例快
        server
            .set_read_timeout(Some(Duration::from_millis(150)))
            .expect("set_read_timeout");
        let mut buf = [0u8; 8];
        let t0 = std::time::Instant::now();
        let err = server.read(&mut buf).expect_err("对端没发数据，读必然失败");
        let waited = t0.elapsed();
        // 判据用「等了多久」而不是错误码：Linux 的 SO_RCVTIMEO 超时返回 EAGAIN（Rust 报 WouldBlock），
        // Windows 返回 WSAETIMEDOUT（报 TimedOut），错误码不可移植；但「阻塞 = 真等过」是共通的。
        assert!(
            waited >= Duration::from_millis(100),
            "accepted socket 仍是非阻塞语义（{:?} 后立刻返回 {:?}）—— KI-29 未修",
            waited,
            err.kind()
        );
        client.join().expect("client join");
    }

    /// 全量写出：小缓冲会被 `write` 分段，必须写满才算成功（部分写会让客户端读到残包）。
    #[test]
    fn write_all_full_writes_every_byte() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let reader = thread::spawn(move || {
            let mut s = TcpStream::connect(addr).expect("connect");
            let mut got = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                }
            }
            got
        });
        let (mut server, _) = listener.accept().expect("accept");
        prepare_conn(&server).expect("prepare_conn");
        let payload = vec![b'z'; 300_000]; // 远大于一次写能吃完的量
        write_all_full(&mut server, &payload).expect("写满");
        drop(server);
        let got = reader.join().expect("reader join");
        assert_eq!(got.len(), payload.len(), "写出的字节数必须等于载荷长度");
        assert!(got.iter().all(|b| *b == b'z'));
    }

    /// 读失败归类：超时/非阻塞残留 ≠ 请求不合法（KI-29 复盘：语义错标会把真机定位带偏）。
    #[test]
    fn read_err_maps_timeout_like_errors_to_eof() {
        for kind in [std::io::ErrorKind::WouldBlock, std::io::ErrorKind::TimedOut] {
            match read_err("读请求头失败", std::io::Error::new(kind, "probe")) {
                HttpParseError::Eof => {}
                other => panic!("{kind:?} 应归为 Eof（对端放弃），实际 {other:?}"),
            }
        }
        match read_err(
            "读请求头失败",
            std::io::Error::new(std::io::ErrorKind::ConnectionReset, "rst"),
        ) {
            HttpParseError::Malformed(m) => assert!(m.contains("读请求头失败"), "{m}"),
            other => panic!("真正的 IO 错误必须仍报 Malformed：{other:?}"),
        }
    }

    /// 写失败不许静默（KI-29 硬要求）：对端已 RST 时 `write_json` 不 panic，但必须计数。
    #[test]
    fn write_json_counts_failures_instead_of_swallowing_them() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let client = thread::spawn(move || {
            let s = TcpStream::connect(addr).expect("connect");
            // 关读方向：内核收到对端数据后回 RST，后续写必然失败
            let _ = s.shutdown(Shutdown::Read);
            thread::sleep(Duration::from_millis(200));
            drop(s);
        });
        let (mut server, _) = listener.accept().expect("accept");
        prepare_conn(&server).expect("prepare_conn");
        let before = WRITE_ERRORS.load(Ordering::Relaxed);
        let payload = json!({ "ok": true, "pad": "x".repeat(8 * 1024) });
        for _ in 0..10 {
            write_json(&mut server, 200, &payload);
            if WRITE_ERRORS.load(Ordering::Relaxed) > before {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let after = WRITE_ERRORS.load(Ordering::Relaxed);
        assert!(
            after > before,
            "对端已 RST，写失败必须计数（write_json 不许吞错）"
        );
        client.join().expect("client join");
    }
}
