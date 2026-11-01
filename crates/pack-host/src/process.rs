//! 子进程生命周期与 stdio JSON-RPC 传输 —— 协议 §1 / §2 / §3。
//!
//! 一句话：**宿主不解释流程**，只负责把请求送进插件、把结果/失败带回来，并且
//! 「不可判定绝不冒充」——超时、崩溃、加载失败一律落 `undetermined` + `note`。
//!
//! 状态机：
//!
//! ```text
//! 懒启动（首次调用 / 设置页「测试」）
//!   ├─ 无包内 venv 解释器 → 用内置 runtime 解释器（config.runtime_python）
//!   ├─ 装配启动器：<解释器> -m dampack_launcher <pack_dir>
//!   │     （缺 launcher 目录时退化为直接跑 entrypoint.args，便于单测/诊断）
//!   ├─ 注入 DAMPACK_* 环境变量（协议 §1.1：地址/令牌/包根/工作目录/日志级别）
//!   └─ health 握手（config.health_timeout，默认 2s）
//!        失败 → 重启（config.max_restarts，默认 ×2）→ 仍失败 → cooldown（默认 300s）
//! 调用
//!   ├─ 正常应答 → Ok(Value)
//!   ├─ 错误应答 → Err(Rpc { code, message })（插件业务错误，原样上抛）
//!   ├─ 超时 → 发 cancel 通知（无 id）→ 宽限 config.cancel_grace（默认 2s）
//!   │        → 插件以 -32004 收尾则保留进程；否则 kill 进程组
//!   │        → 一律 Err(Unavailable::Timeout)，note = plugin_timeout
//!   └─ 断链（stdout EOF / 进程已退）→ 记一次崩溃 → 重启重试
//!            重试耗尽 → cooldown，Err(Unavailable::Crash)，note = plugin_crash
//! ```
//!
//! 时间常量与优先级以本文件常量为准；`manifest.timeout_secs` 是单次调用的预算。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde_json::{json, Value};

use crate::manifest::{load_pack, Manifest, ManifestError, PackIdentity, API_VERSION};

/// `health` 握手上限。协议 §2 冻结值 = 2s（卡面曾写 10s，以协议为准；可配）。
pub const DEFAULT_HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
/// 超时后给插件的收尾宽限（协议 §2.1）。
pub const DEFAULT_CANCEL_GRACE: Duration = Duration::from_secs(2);
/// 崩溃自动重启次数（协议 §2）。
pub const DEFAULT_MAX_RESTARTS: u32 = 2;
/// 重启耗尽后的冷却时长。
pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(300);
/// SIGTERM → SIGKILL 之间的等待。
const TERM_GRACE: Duration = Duration::from_secs(3);
/// 保留的 stderr 尾部行数（加载失败原因要能贴给用户看）。
const STDERR_TAIL: usize = 40;

/// `CREATE_NO_WINDOW`（Win32 `winbase.h`）：不为子进程新建控制台窗口。
///
/// 为什么需要（KI-42，2026-10-25 真机）：Windows 上 `python.exe` / `nvidia-smi.exe` 都是
/// **console 子系统**程序；宿主（`app-tauri.exe`，GUI 子系统）spawn 它们时若不指定该标志，
/// 系统会为子进程新分配一个终端窗口 —— 表现就是「跑高清放大时黑窗一闪而过」。
/// 只影响窗口可见性，**不改** stdio 管道语义（stdin/stdout/stderr 仍是 piped）。
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 抑制子进程的控制台窗口（Windows）。返回实际应用的 `creation_flags`，
/// 非 Windows 平台返回 `0`（Linux/macOS 无此概念，行为不变）。
///
/// 单测靠返回值断言「调用点真的接上了」，因为 `std::process::Command` 不暴露已设的 flags。
pub fn no_console_window(cmd: &mut Command) -> u32 {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
        CREATE_NO_WINDOW
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
        0
    }
}

/// 插件不可用的四类原因（宿主据此决定 `note`，绝不转成 pass/fail）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// 调用超时：已发 cancel、宽限后仍无收尾。
    Timeout,
    /// 崩溃/断链，且重启次数耗尽或正在冷却。
    Crash,
    /// `health` 握手超时（启动挂住）。
    HealthTimeout,
    /// 启动器/插件加载失败（venv 缺依赖、import 抛异常、缺 `__init__.py`）。
    LoadFailed,
}

impl Unavailable {
    /// 落进 Verdict.note 的稳定字符串。
    pub fn note(self) -> &'static str {
        match self {
            Unavailable::Timeout => "plugin_timeout",
            Unavailable::Crash => "plugin_crash",
            Unavailable::HealthTimeout => "plugin_health_timeout",
            Unavailable::LoadFailed => "plugin_load_failed",
        }
    }
}

/// 超时结构化（N19）：超时不能只有一句中文——落库/上报/界面要能机读
/// 「预算多少 / 实耗多少 / 该往哪一档走 / 卡在哪一段」。
///
/// 为什么要 `last_stage`：真机 300s 超时那次，宿主只知道「超了」，包内阶段日志
/// （`[segment] stage ... decode 6801 ms`）才是定位依据；它必须跟着错误一起出来，
/// 否则排障只能重跑。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeoutInfo {
    /// 本次调用的预算（= `manifest.timeout_secs` 或显式传入）。
    pub budget_ms: u64,
    /// 实耗（≥ budget：宿主到点才判超时）。
    pub elapsed_ms: u64,
    /// 建议档（下一档预算；已到顶 → 建议先预缩放/拆批）。
    pub suggested_tier: &'static str,
    /// 插件 stderr 最后一条非空日志（通常是包内阶段日志）。
    pub last_stage: Option<String>,
}

impl TimeoutInfo {
    /// 机读形状（进 evidence / 行错误 / 日志都同一套键）。
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "code": "timeout",
            "budgetMs": self.budget_ms,
            "elapsedMs": self.elapsed_ms,
            "suggestedTier": self.suggested_tier,
            "lastStage": self.last_stage,
        })
    }

    /// 「建议档」：预算落在哪一档就说下一档；到顶就说「别再涨预算」。
    pub fn suggest_tier(budget_ms: u64) -> &'static str {
        match budget_ms {
            b if b < 30_000 => "S(30s)",
            b if b < 90_000 => "M(90s)",
            b if b < 180_000 => "L(180s)",
            b if b < 600_000 => "XL(600s)",
            _ => "已到顶：先预缩放（maxSide）/拆批，不要再加预算",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PackHostError {
    #[error("manifest：{0}")]
    Manifest(#[from] ManifestError),

    /// 不可用（超时/崩溃/握手超时/加载失败）。`stderr` 是插件侧原始输出尾部。
    #[error("插件不可用（{}）：{message}", kind.note())]
    Unavailable {
        kind: Unavailable,
        message: String,
        stderr: Vec<String>,
        /// 只对 `kind = Timeout` 有值（N19 结构化；其余原因没有预算语义）。
        timeout: Option<TimeoutInfo>,
    },

    /// 插件返回的 JSON-RPC 错误（-32001..-32005 / -32601 / -32700）。
    #[error("插件返回错误 {code}：{message}")]
    Rpc { code: i64, message: String },

    /// 协议违例：应答既无 result 也无 error、方法缺失实现等。
    #[error("协议违例：{0}")]
    Protocol(String),

    #[error("子进程 IO 失败：{0}")]
    Io(String),
}

impl PackHostError {
    /// 映射成 Verdict.note：只有「不可判定」类才有 note；业务错误码不算不可判定。
    pub fn note(&self) -> Option<&'static str> {
        match self {
            PackHostError::Unavailable { kind, .. } => Some(kind.note()),
            _ => None,
        }
    }

    /// 本次失败是否「不可判定」（→ `undetermined`），反之为真错误（→ 由调用方决定）。
    pub fn is_undetermined(&self) -> bool {
        matches!(self, PackHostError::Unavailable { .. })
    }

    /// 超时结构化（N19）：`Some` = 这是超时，且带预算/实耗/建议档/最后一段日志。
    pub fn timeout_info(&self) -> Option<&TimeoutInfo> {
        match self {
            PackHostError::Unavailable { timeout, .. } => timeout.as_ref(),
            _ => None,
        }
    }

    /// 机读错误码（N19）：行错误/日志/上报都用这一套词，**不靠中文文案解析**。
    ///
    /// 与 [`Self::note`] 的分工：`note` 是「判定不可用」的落库词（plugin_timeout …），
    /// 这里是「这次调用错在哪」的稳定码（timeout / plugin_crash / …）。
    pub fn error_code(&self) -> &'static str {
        match self {
            PackHostError::Unavailable { kind, .. } => match kind {
                Unavailable::Timeout => "timeout",
                Unavailable::Crash => "crash",
                Unavailable::HealthTimeout | Unavailable::LoadFailed => "unavailable",
            },
            PackHostError::Rpc { .. } => "plugin_rpc_error",
            PackHostError::Protocol(_) => "plugin_protocol_error",
            PackHostError::Io(_) => "plugin_io_error",
            PackHostError::Manifest(_) => "pack_manifest_invalid",
        }
    }

    fn unavailable(kind: Unavailable, message: impl Into<String>, stderr: Vec<String>) -> Self {
        PackHostError::Unavailable {
            kind,
            message: message.into(),
            stderr,
            timeout: None,
        }
    }

    /// 超时专用构造：文案里塞上「预算/实耗/建议档/最后日志」，同时把同一份事实
    /// 存成机读的 [`TimeoutInfo`]（文案给人，`timeout_info()` 给程序）。
    fn unavailable_timeout(
        message: impl Into<String>,
        stderr: Vec<String>,
        info: TimeoutInfo,
    ) -> Self {
        PackHostError::Unavailable {
            kind: Unavailable::Timeout,
            message: message.into(),
            stderr,
            timeout: Some(info),
        }
    }
}

/// 宿主配置。`pack_dir` 由 [`PackHost::open`] 给出，不在这里。
#[derive(Debug, Clone)]
pub struct PackHostConfig {
    /// 能力网关地址（协议 §1.1）。每次启动随机端口，由宿主进程内网关提供。
    pub gateway_url: String,
    /// 一次性 Bearer 令牌（进程退出即废）。
    pub gateway_token: String,
    /// 本包可写的临时工作目录（产物默认落这里）。
    pub work_dir: PathBuf,
    /// `debug` / `info` / `warn` / `error`（只影响 stderr 详细度）。
    pub log_level: String,
    /// 含 `dampack_launcher.py` 的目录；`Some` 时以 `-m dampack_launcher` 启动。
    /// `None` 时退化为直接执行 `manifest.entrypoint.args`（单测/诊断用）。
    pub launcher_dir: Option<PathBuf>,
    /// 无包内 venv 时使用的解释器（内置 runtime 的 python）。`None` → `python3`。
    pub runtime_python: Option<PathBuf>,
    pub health_timeout: Duration,
    pub cancel_grace: Duration,
    pub max_restarts: u32,
    pub cooldown: Duration,
}

impl PackHostConfig {
    pub fn new(
        gateway_url: impl Into<String>,
        gateway_token: impl Into<String>,
        work_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            gateway_url: gateway_url.into(),
            gateway_token: gateway_token.into(),
            work_dir: work_dir.into(),
            log_level: "info".to_string(),
            launcher_dir: None,
            runtime_python: None,
            health_timeout: DEFAULT_HEALTH_TIMEOUT,
            cancel_grace: DEFAULT_CANCEL_GRACE,
            max_restarts: DEFAULT_MAX_RESTARTS,
            cooldown: DEFAULT_COOLDOWN,
        }
    }

    pub fn with_launcher_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.launcher_dir = Some(dir.into());
        self
    }
}

/// 一次调用的完整记录（§7 铁律 7：每条结果可追责）。
#[derive(Debug)]
pub struct CallOutcome {
    pub pack_id: String,
    pub version: String,
    pub plugin_hash: String,
    pub deterministic: bool,
    pub method: String,
    pub result: Result<Value, PackHostError>,
}

impl CallOutcome {
    /// 结果行里要落的身份标签：`id@version#hash12`。
    pub fn pack_label(&self) -> String {
        let short = &self.plugin_hash[..self.plugin_hash.len().min(12)];
        format!("{}@{}#{}", self.pack_id, self.version, short)
    }

    /// 失败时落 `undetermined` 的 note（`Some` = 不可判定）。
    pub fn note(&self) -> Option<&'static str> {
        self.result.as_ref().err().and_then(PackHostError::note)
    }
}

// ---------------------------------------------------------------------------
// 单个子进程连接
// ---------------------------------------------------------------------------
type FrameSender = Sender<Option<Value>>;

struct Conn {
    pid: u32,
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<HashMap<u64, FrameSender>>>,
    stray: Arc<Mutex<Vec<String>>>,
    notifications: Arc<Mutex<Vec<Value>>>,
    stderr: Arc<Mutex<Vec<String>>>,
    next_id: AtomicU64,
    /// 读取线程句柄：仅为生命周期留证（线程在 EOF 时自行退出），不 join。
    _readers: Mutex<Vec<JoinHandle<()>>>,
}

impl Conn {
    fn spawn(
        python: &Path,
        pack_dir: &Path,
        args: &[String],
        launcher_dir: Option<&Path>,
        cfg: &PackHostConfig,
    ) -> Result<Arc<Self>, PackHostError> {
        let mut cmd = Command::new(python);
        if let Some(dir) = launcher_dir {
            cmd.arg("-m").arg("dampack_launcher").arg(pack_dir);
            // 启动器自身所在目录要在 PYTHONPATH 里，`-m` 才能解析到它（venv 解释器同样认 PYTHONPATH）
            let mut paths = vec![dir.to_path_buf()];
            if let Some(prev) = std::env::var_os("PYTHONPATH") {
                paths.extend(std::env::split_paths(&prev));
            }
            let joined = std::env::join_paths(paths)
                .map_err(|e| PackHostError::Io(format!("PYTHONPATH 拼接失败：{e}")))?;
            cmd.env("PYTHONPATH", joined);
        } else {
            for rel in args {
                cmd.arg(pack_dir.join(rel));
            }
        }

        // 网关是 127.0.0.1，绝不能被宿主环境的代理劫持
        let no_proxy = match std::env::var("NO_PROXY") {
            Ok(existing) if !existing.is_empty() => format!("127.0.0.1,localhost,{existing}"),
            _ => "127.0.0.1,localhost".to_string(),
        };

        cmd.current_dir(pack_dir)
            .env("DAMPACK_GATEWAY_URL", &cfg.gateway_url)
            .env("DAMPACK_GATEWAY_TOKEN", &cfg.gateway_token)
            .env("DAMPACK_PACK_DIR", pack_dir)
            .env("DAMPACK_WORK_DIR", &cfg.work_dir)
            .env("DAMPACK_LOG_LEVEL", &cfg.log_level)
            .env("PYTHONUNBUFFERED", "1")
            .env("NO_PROXY", &no_proxy)
            .env("no_proxy", &no_proxy)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(unix)]
        {
            // 独立进程组：kill 时整组收掉，避免 venv 里的子进程残留
            cmd.process_group(0);
        }
        // KI-42：Windows 上不为插件进程弹控制台黑窗（见 `no_console_window`）
        no_console_window(&mut cmd);

        let mut child = cmd.spawn().map_err(|e| {
            PackHostError::unavailable(
                Unavailable::LoadFailed,
                format!("拉起解释器失败（{}）：{e}", python.display()),
                Vec::new(),
            )
        })?;
        let pid = child.id();
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let pending: Arc<Mutex<HashMap<u64, FrameSender>>> = Arc::default();
        let stray: Arc<Mutex<Vec<String>>> = Arc::default();
        let notifications: Arc<Mutex<Vec<Value>>> = Arc::default();
        let stderr_lines: Arc<Mutex<Vec<String>>> = Arc::default();

        let readers = vec![
            spawn_stdout_reader(
                stdout,
                pending.clone(),
                stray.clone(),
                notifications.clone(),
            ),
            spawn_stderr_reader(stderr, stderr_lines.clone()),
        ];

        Ok(Arc::new(Self {
            pid,
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            pending,
            stray,
            notifications,
            stderr: stderr_lines,
            next_id: AtomicU64::new(1),
            _readers: Mutex::new(readers),
        }))
    }

    fn pid(&self) -> u32 {
        self.pid
    }

    fn write_frame(&self, frame: &Value) -> Result<(), PackHostError> {
        let mut line = serde_json::to_string(frame)
            .map_err(|e| PackHostError::Protocol(format!("序列化请求失败：{e}")))?;
        line.push('\n');
        let mut guard = self.stdin.lock().unwrap();
        guard
            .write_all(line.as_bytes())
            .and_then(|_| guard.flush())
            .map_err(|e| PackHostError::Io(format!("写插件 stdin 失败：{e}")))
    }

    fn stderr_tail(&self) -> Vec<String> {
        self.stderr.lock().unwrap().clone()
    }

    fn exit_describe(&self) -> String {
        let mut guard = self.child.lock().unwrap();
        match guard.try_wait() {
            Ok(Some(status)) => format!("进程已退出（{status}）"),
            Ok(None) => "进程仍在运行（可能已关闭 stdout）".to_string(),
            Err(e) => format!("查询进程状态失败：{e}"),
        }
    }

    /// 断链判定：stdout EOF 后进程是否已经退出（用于区分「加载失败」与「崩溃」）。
    /// 进程退出与 EOF 之间可能有毫秒级时差，这里给最多 500ms。
    fn exit_code(&self) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            {
                let mut guard = self.child.lock().unwrap();
                if let Ok(Some(status)) = guard.try_wait() {
                    return status.code();
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// 终止子进程：先整组 SIGTERM，宽限 TERM_GRACE，再 SIGKILL。
    fn terminate(&self) {
        #[cfg(unix)]
        {
            let pgid = -(self.pid as i32);
            unsafe {
                libc::kill(pgid, libc::SIGTERM);
            }
            let deadline = Instant::now() + TERM_GRACE;
            loop {
                {
                    let mut child = self.child.lock().unwrap();
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        return;
                    }
                }
                if Instant::now() >= deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            unsafe {
                libc::kill(pgid, libc::SIGKILL);
            }
        }
        #[cfg(not(unix))]
        {
            let mut child = self.child.lock().unwrap();
            let _ = child.kill();
        }
        let mut child = self.child.lock().unwrap();
        let _ = child.wait();
    }

    fn kill_hard(&self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.pid as i32), libc::SIGKILL);
        }
        #[cfg(not(unix))]
        {
            let mut child = self.child.lock().unwrap();
            let _ = child.kill();
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_stdout_reader(
    stdout: ChildStdout,
    pending: Arc<Mutex<HashMap<u64, FrameSender>>>,
    stray: Arc<Mutex<Vec<String>>>,
    notifications: Arc<Mutex<Vec<Value>>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(frame) => {
                    // 只认「有 id 的应答」；无 id = 插件通知（progress/log），留档不串账
                    match frame.get("id").and_then(Value::as_u64) {
                        Some(id) => {
                            let tx = pending.lock().unwrap().remove(&id);
                            if let Some(tx) = tx {
                                let _ = tx.send(Some(frame));
                            }
                        }
                        None => notifications.lock().unwrap().push(frame),
                    }
                }
                Err(_) => {
                    // 非协议行 = stdout 污染（协议 §7 铁律 4），入证不丢弃
                    stray.lock().unwrap().push(trimmed.to_string());
                }
            }
        }
        // EOF：唤醒所有在飞请求（宿主据此判崩溃）
        let mut guard = pending.lock().unwrap();
        for (_, tx) in guard.drain() {
            let _ = tx.send(None);
        }
    })
}

fn spawn_stderr_reader(
    stderr: impl Read + Send + 'static,
    lines: Arc<Mutex<Vec<String>>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            let mut guard = lines.lock().unwrap();
            guard.push(line);
            if guard.len() > STDERR_TAIL {
                let drop_n = guard.len() - STDERR_TAIL;
                guard.drain(..drop_n);
            }
        }
    })
}

// ---------------------------------------------------------------------------
// 宿主
// ---------------------------------------------------------------------------
#[derive(Default)]
struct Inner {
    conn: Option<Arc<Conn>>,
    /// 启动失败累计（连续失败到 max_restarts 后进冷却）
    start_failures: u32,
    /// 崩溃重启次数（观测用；每次成功握手清零）
    restarts: u32,
    /// 累计拉起次数（冷却期断言用）
    spawns: u32,
    cooldown_until: Option<Instant>,
    last_failure: Option<(Unavailable, String)>,
}

/// 行业包宿主句柄。一个包一个，内部持有子进程；`&self` 可并发调用。
pub struct PackHost {
    pack_dir: PathBuf,
    manifest: Manifest,
    identity: PackIdentity,
    cfg: PackHostConfig,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for PackHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 子进程连接内部含管道/线程，不展开；调试只要身份与 pid（try_lock 防重入死锁）
        let pid = self
            .inner
            .try_lock()
            .ok()
            .and_then(|inner| inner.conn.as_ref().map(|c| c.pid()));
        f.debug_struct("PackHost")
            .field("pack_dir", &self.pack_dir)
            .field("identity", &self.identity)
            .field("pid", &pid)
            .finish()
    }
}

impl PackHost {
    /// 开包：只读 manifest（不执行包内代码）。子进程在首次调用时才拉起。
    pub fn open(pack_dir: impl AsRef<Path>, cfg: PackHostConfig) -> Result<Self, PackHostError> {
        let pack_dir = pack_dir.as_ref().to_path_buf();
        let (manifest, identity) = load_pack(&pack_dir)?;
        Ok(Self {
            pack_dir,
            manifest,
            identity,
            cfg,
            inner: Mutex::new(Inner::default()),
        })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn identity(&self) -> &PackIdentity {
        &self.identity
    }

    pub fn config(&self) -> &PackHostConfig {
        &self.cfg
    }

    pub fn pack_dir(&self) -> &Path {
        &self.pack_dir
    }

    /// 当前子进程 pid（未启动为 `None`）。
    pub fn pid(&self) -> Option<u32> {
        self.inner.lock().unwrap().conn.as_ref().map(|c| c.pid())
    }

    /// 累计拉起次数（冷却/重启断言用）。
    pub fn spawn_count(&self) -> u32 {
        self.inner.lock().unwrap().spawns
    }

    /// 丢弃子进程（下一次调用重新拉起）。测试与「改配置后生效」共用。
    pub fn drop_child(&self) {
        let conn = self.inner.lock().unwrap().conn.take();
        if let Some(conn) = conn {
            conn.terminate();
        }
    }

    /// 模拟崩溃（`kill -9`）——用于验证「崩溃 → 自动重启」链路，不参与业务。
    pub fn simulate_crash(&self) {
        let conn = self.inner.lock().unwrap().conn.take();
        if let Some(conn) = conn {
            conn.kill_hard();
            let _ = conn.child.lock().map(|mut c| c.wait());
        }
    }

    /// 非协议 stdout 行（pollution 证据；协议要求恒为空）。
    pub fn stray_stdout(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .conn
            .as_ref()
            .map(|c| c.stray.lock().unwrap().clone())
            .unwrap_or_default()
    }

    /// 插件发出的通知（progress/log）。
    pub fn notifications(&self) -> Vec<Value> {
        self.inner
            .lock()
            .unwrap()
            .conn
            .as_ref()
            .map(|c| c.notifications.lock().unwrap().clone())
            .unwrap_or_default()
    }

    pub fn shutdown(&self) {
        self.drop_child();
    }

    // --- 启动与握手 -----------------------------------------------------
    fn interpreter(&self) -> PathBuf {
        resolve_interpreter(&self.pack_dir, self.cfg.runtime_python.as_deref())
    }

    /// 确保有可用子进程：懒启动 + 握手；失败按 max_restarts 重试，再失败进冷却。
    fn ensure_ready(&self) -> Result<Arc<Conn>, PackHostError> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(conn) = &inner.conn {
            return Ok(conn.clone());
        }
        if let Some(until) = inner.cooldown_until {
            if Instant::now() < until {
                let (kind, msg) = inner
                    .last_failure
                    .clone()
                    .unwrap_or((Unavailable::Crash, "冷却中".to_string()));
                let left = until.saturating_duration_since(Instant::now());
                return Err(PackHostError::unavailable(
                    kind,
                    format!("包在冷却中（剩余 {}s）：{msg}", left.as_secs()),
                    Vec::new(),
                ));
            }
            inner.cooldown_until = None;
            inner.start_failures = 0;
        }

        let mut last: Option<PackHostError> = None;
        for _attempt in 0..=self.cfg.max_restarts {
            match self.spawn_and_handshake(&mut inner) {
                Ok(conn) => {
                    inner.conn = Some(conn.clone());
                    inner.start_failures = 0;
                    return Ok(conn);
                }
                Err(err) => {
                    inner.start_failures += 1;
                    let kind = match &err {
                        PackHostError::Unavailable { kind, .. } => *kind,
                        _ => Unavailable::Crash,
                    };
                    let msg = err.to_string();
                    inner.last_failure = Some((kind, msg));
                    last = Some(err);
                }
            }
        }
        let (kind, msg) = inner
            .last_failure
            .clone()
            .unwrap_or((Unavailable::Crash, "启动失败".to_string()));
        inner.cooldown_until = Some(Instant::now() + self.cfg.cooldown);
        let err = last.expect("至少尝试一次");
        Err(match err {
            PackHostError::Unavailable { stderr, .. } => PackHostError::unavailable(
                kind,
                format!(
                    "重启 {} 次仍失败，进入冷却 {}s：{msg}",
                    self.cfg.max_restarts,
                    self.cfg.cooldown.as_secs()
                ),
                stderr,
            ),
            other => other,
        })
    }

    fn spawn_and_handshake(&self, inner: &mut Inner) -> Result<Arc<Conn>, PackHostError> {
        inner.spawns += 1;
        let python = self.interpreter();
        // 内置 runtime 是**绝对路径**（壳从 `<resource_dir>` 解析后注入）；不在原位 = 安装包不完整。
        // 必须当场说清「缺哪个路径」：裸 spawn 失败只会给 `io error: No such file or directory`，
        // 用户/支持看不出是包坏了还是没装 python（KI-28 的可诊断性要求，卡 t_5c9accc5）。
        if let Some(rt) = self.cfg.runtime_python.as_deref() {
            // 只校验绝对路径：`python3` / `python` 这类是**命令名**，交给 PATH（PATH 里的绝对路径
            // 由壳/配置方自己负责），否则会把「开发机用 PATH 兜底」误判成运行时缺失。
            if rt.is_absolute() && python.as_path() == rt && !rt.is_file() {
                return Err(PackHostError::unavailable(
                    Unavailable::LoadFailed,
                    format!(
                        "内置 python 运行时缺失：{}（安装包不完整或 resources/runtime/python 未随包分发；\
                         可用 DAMPACK_RUNTIME_PYTHON 指定解释器）",
                        rt.display()
                    ),
                    Vec::new(),
                ));
            }
        }
        let conn = Conn::spawn(
            &python,
            &self.pack_dir,
            &self.manifest.entrypoint.args,
            self.cfg.launcher_dir.as_deref(),
            &self.cfg,
        )?;

        match self.roundtrip(&conn, "health", json!({}), self.cfg.health_timeout) {
            Roundtrip::Frame(frame) => {
                let result = frame.get("result").cloned();
                match result {
                    Some(res) if res.get("status").and_then(Value::as_str) == Some("ok") => {
                        let api = res.get("api_version").and_then(Value::as_str).unwrap_or("");
                        if api != API_VERSION {
                            let stderr = conn.stderr_tail();
                            conn.terminate();
                            return Err(PackHostError::unavailable(
                                Unavailable::LoadFailed,
                                format!("插件 api_version={api:?}，宿主只接受 {API_VERSION:?}"),
                                stderr,
                            ));
                        }
                        Ok(conn)
                    }
                    Some(res) => {
                        let stderr = conn.stderr_tail();
                        conn.terminate();
                        Err(PackHostError::unavailable(
                            Unavailable::LoadFailed,
                            format!("health 应答 status 非 ok：{res}"),
                            stderr,
                        ))
                    }
                    None => {
                        let stderr = conn.stderr_tail();
                        conn.terminate();
                        Err(PackHostError::unavailable(
                            Unavailable::LoadFailed,
                            format!("health 应答缺 result：{frame}"),
                            stderr,
                        ))
                    }
                }
            }
            Roundtrip::Rpc(err) => {
                let stderr = conn.stderr_tail();
                conn.terminate();
                Err(PackHostError::unavailable(
                    Unavailable::LoadFailed,
                    format!("health 返回错误：{err}"),
                    stderr,
                ))
            }
            Roundtrip::Closed(reason) => {
                let stderr = conn.stderr_tail();
                let code = conn.exit_code();
                let kind = classify_start_failure(code, &stderr);
                conn.terminate();
                let detail = if stderr.is_empty() {
                    reason
                } else {
                    format!("{reason}；stderr 尾部：{}", tail_join(&stderr))
                };
                Err(PackHostError::unavailable(kind, detail, stderr))
            }
            Roundtrip::TimedOut(kept_alive) => {
                let stderr = conn.stderr_tail();
                if !kept_alive {
                    conn.terminate();
                }
                Err(PackHostError::unavailable(
                    Unavailable::HealthTimeout,
                    format!(
                        "health 握手超过 {}ms 无应答",
                        self.cfg.health_timeout.as_millis()
                    ),
                    stderr,
                ))
            }
        }
    }

    // --- 调用 -----------------------------------------------------------
    /// 按 `manifest.timeout_secs` 预算调用插件方法。
    pub fn call(&self, method: &str, params: Value) -> Result<Value, PackHostError> {
        let timeout = Duration::from_secs(self.manifest.timeout_secs);
        self.call_with_timeout(method, params, timeout)
    }

    /// 带显式超时的调用（设置页「测试」/诊断可缩短预算）。
    pub fn call_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, PackHostError> {
        let mut restart_rounds = 0u32;
        loop {
            let conn = self.ensure_ready()?;
            let started = Instant::now();
            match self.roundtrip(&conn, method, params.clone(), timeout) {
                Roundtrip::Frame(frame) => {
                    return match frame.get("error") {
                        Some(err) => Err(PackHostError::Rpc {
                            code: err.get("code").and_then(Value::as_i64).unwrap_or(-32000),
                            message: err
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        }),
                        None => frame.get("result").cloned().ok_or_else(|| {
                            PackHostError::Protocol(format!("应答既无 result 也无 error：{frame}"))
                        }),
                    };
                }
                Roundtrip::Rpc(err) => return Err(err),
                Roundtrip::Closed(reason) => {
                    let stderr = conn.stderr_tail();
                    self.drop_conn_only(&conn);
                    if restart_rounds >= self.cfg.max_restarts {
                        let mut inner = self.inner.lock().unwrap();
                        inner.cooldown_until = Some(Instant::now() + self.cfg.cooldown);
                        inner.last_failure = Some((
                            Unavailable::Crash,
                            format!("{reason}；{}", tail_join(&stderr)),
                        ));
                        return Err(PackHostError::unavailable(
                            Unavailable::Crash,
                            format!(
                                "崩溃 {restart_rounds} 次仍失败，进入冷却 {}s：{reason}",
                                self.cfg.cooldown.as_secs()
                            ),
                            stderr,
                        ));
                    }
                    restart_rounds += 1;
                    {
                        let mut inner = self.inner.lock().unwrap();
                        inner.restarts += 1;
                    }
                    continue;
                }
                Roundtrip::TimedOut(kept_alive) => {
                    let stderr = conn.stderr_tail();
                    if !kept_alive {
                        self.drop_conn_only(&conn);
                    }
                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    let info = TimeoutInfo {
                        budget_ms: timeout.as_millis() as u64,
                        elapsed_ms,
                        suggested_tier: TimeoutInfo::suggest_tier(timeout.as_millis() as u64),
                        last_stage: stderr
                            .iter()
                            .rev()
                            .map(|l| l.trim())
                            .find(|l| !l.is_empty())
                            .map(str::to_string),
                    };
                    return Err(PackHostError::unavailable_timeout(
                        format!(
                            "{method} 超过预算 {}ms（实耗 ≥{}ms；建议档 {}）；{}{}",
                            info.budget_ms,
                            elapsed_ms,
                            info.suggested_tier,
                            if kept_alive {
                                "已发 cancel，插件以 -32004 收尾（进程保留）"
                            } else {
                                "cancel 宽限后无收尾，已 kill 子进程"
                            },
                            match &info.last_stage {
                                Some(line) => format!("；最后日志：{line}"),
                                None => String::new(),
                            }
                        ),
                        stderr,
                        info,
                    ));
                }
            }
        }
    }

    /// 带身份标签的调用记录（落库/落报告用）。
    pub fn call_recorded(&self, method: &str, params: Value) -> CallOutcome {
        let result = self.call(method, params);
        CallOutcome {
            pack_id: self.identity.id.clone(),
            version: self.identity.version.clone(),
            plugin_hash: self.identity.plugin_hash.clone(),
            deterministic: self.manifest.deterministic,
            method: method.to_string(),
            result,
        }
    }

    /// `pack.info` 与 manifest 的一致性核对（协议 §3.1：不一致即判包损坏）。
    /// 返回不一致项（空 = 一致）。
    pub fn verify_pack_info(&self) -> Result<Vec<String>, PackHostError> {
        let info = self.call_with_timeout("pack.info", json!({}), self.cfg.health_timeout)?;
        let mut problems = Vec::new();
        let get_str = |key: &str| {
            info.get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        for (key, expected) in [
            ("id", self.manifest.id.clone()),
            ("version", self.manifest.version.clone()),
            ("api_version", self.manifest.api_version.clone()),
        ] {
            let got = get_str(key);
            if got != expected {
                problems.push(format!("{key}: 插件回 {got:?}，manifest 为 {expected:?}"));
            }
        }
        let mut caps: Vec<String> = info
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        caps.sort();
        let mut want = self.manifest.capabilities.clone();
        want.sort();
        if caps != want {
            problems.push(format!(
                "capabilities: 插件回 {caps:?}，manifest 为 {want:?}"
            ));
        }
        if let Some(det) = info.get("deterministic").and_then(Value::as_bool) {
            if det != self.manifest.deterministic {
                problems.push(format!(
                    "deterministic: 插件回 {det}，manifest 为 {}",
                    self.manifest.deterministic
                ));
            }
        }
        if let Some(tos) = info.get("timeout_secs").and_then(Value::as_u64) {
            if tos != self.manifest.timeout_secs {
                problems.push(format!(
                    "timeout_secs: 插件回 {tos}，manifest 为 {}",
                    self.manifest.timeout_secs
                ));
            }
        }
        Ok(problems)
    }

    // --- 单次往返 -------------------------------------------------------
    fn roundtrip(
        &self,
        conn: &Arc<Conn>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Roundtrip {
        let id = conn.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx): (FrameSender, Receiver<Option<Value>>) = mpsc::channel();
        conn.pending.lock().unwrap().insert(id, tx);
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if trace_on() {
            eprintln!("[host-trace] rpc#{id} {method} 写请求（{}, {}B）", conn.pid, frame.to_string().len());
        }
        if let Err(err) = conn.write_frame(&frame) {
            conn.pending.lock().unwrap().remove(&id);
            return Roundtrip::Rpc(err);
        }
        if trace_on() {
            eprintln!("[host-trace] rpc#{id} {method} 请求已写出 → 等应答（预算 {}ms）", timeout.as_millis());
        }
        match rx.recv_timeout(timeout) {
            Ok(Some(frame)) => {
                if trace_on() {
                    eprintln!("[host-trace] rpc#{id} {method} 收到应答");
                }
                Roundtrip::Frame(frame)
            }
            Ok(None) => Roundtrip::Closed(conn.exit_describe()),
            Err(mpsc::RecvTimeoutError::Disconnected) => Roundtrip::Closed(conn.exit_describe()),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // 协议 §2：超时 → cancel 通知（无 id）→ 宽限 → 仍未收尾则 kill
                let _ = conn.write_frame(&json!({
                    "jsonrpc": "2.0",
                    "method": "cancel",
                    "params": {"job_id": params_job_id(&frame)}
                }));
                match rx.recv_timeout(self.cfg.cancel_grace) {
                    Ok(Some(_frame)) => {
                        conn.pending.lock().unwrap().remove(&id);
                        Roundtrip::TimedOut(true)
                    }
                    _ => {
                        conn.pending.lock().unwrap().remove(&id);
                        Roundtrip::TimedOut(false)
                    }
                }
            }
        }
    }

    fn drop_conn_only(&self, conn: &Arc<Conn>) {
        let mut inner = self.inner.lock().unwrap();
        let same = inner
            .conn
            .as_ref()
            .map(|c| Arc::ptr_eq(c, conn))
            .unwrap_or(false);
        if same {
            inner.conn = None;
        }
        drop(inner);
        conn.terminate();
    }
}

enum Roundtrip {
    Frame(Value),
    Rpc(PackHostError),
    Closed(String),
    /// `true` = 插件收到 cancel 后以 -32004 收尾（进程保留）
    TimedOut(bool),
}

/// `DAMPACK_HOST_TRACE=1`：逐帧打点（真机「卡在写还是卡在读」的取证开关；默认静默）。
/// 只在排障时开——它给的是时序，不是业务事实。
fn trace_on() -> bool {
    std::env::var("DAMPACK_HOST_TRACE")
        .map(|v| matches!(v.trim(), "1" | "on" | "true" | "yes"))
        .unwrap_or(false)
}

fn params_job_id(frame: &Value) -> String {
    frame
        .get("params")
        .and_then(|p| p.get("job_id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn tail_join(lines: &[String]) -> String {
    lines
        .iter()
        .rev()
        .take(6)
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" | ")
}

/// 启动失败归类：解释器/插件代码退出码非零 = 加载失败；被杀/无输出 = 崩溃。
fn classify_start_failure(exit_code: Option<i32>, stderr: &[String]) -> Unavailable {
    match exit_code {
        Some(0) => Unavailable::Crash,
        Some(_) => Unavailable::LoadFailed,
        None => {
            if stderr.is_empty() {
                Unavailable::Crash
            } else {
                Unavailable::LoadFailed
            }
        }
    }
}

/// 解释器选择：包内 venv → 内置 runtime（`config.runtime_python`）→ 裸 `python3`。
///
/// 三层优先级是 2026-10-20 运行时裁决（`tools/dampack_launcher.py` 文件头同口径）：
/// ① venv = 每包依赖隔离边界（**装包时**用内置 runtime 建）；
/// ② 内置 runtime = 纯净 Windows（无 `python3`）唯一可用解释器，壳从 `<resource_dir>/runtime/python` 解析后注入；
/// ③ `python3` = 开发机/容器兜底（是命令名不是路径，交给 PATH 解析，**不做 is_file 校验**）。
///
/// 单测直接钉这条优先级（`resolve_interpreter` 是公开的纯函数，不碰进程）。
pub fn resolve_interpreter(pack_dir: &Path, runtime_python: Option<&Path>) -> PathBuf {
    if let Some(p) = venv_python(pack_dir) {
        return p;
    }
    runtime_python
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("python3"))
}

/// 包内 venv 解释器（`<pack>/venv/bin/python3` / Windows `venv/Scripts/python.exe`）。
pub fn venv_python(pack_dir: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = ["venv/Scripts/python.exe", "venv/Scripts/python3.exe"];
    #[cfg(not(windows))]
    let candidates = ["venv/bin/python3", "venv/bin/python"];
    candidates
        .iter()
        .map(|rel| pack_dir.join(rel))
        .find(|p| p.is_file())
}

/// 用于外部（svc 层）把不可用结果落成 Verdict 形态的最小助手：
/// `status` 恒为 `undetermined`，`score` 为 null，`note` 为失败原因。
pub fn undetermined_verdict(rule_id: &str, err: &PackHostError) -> Value {
    json!({
        "ruleId": rule_id,
        "status": "undetermined",
        "score": Value::Null,
        "label": "plugin_unavailable",
        "evidence": {
            "reason": err.note().unwrap_or("plugin_error"),
            "detail": err.to_string(),
        },
        "fixed": false,
        "note": err.note().unwrap_or("plugin_error"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_are_protocol_strings() {
        assert_eq!(Unavailable::Timeout.note(), "plugin_timeout");
        assert_eq!(Unavailable::Crash.note(), "plugin_crash");
        assert_eq!(Unavailable::HealthTimeout.note(), "plugin_health_timeout");
        assert_eq!(Unavailable::LoadFailed.note(), "plugin_load_failed");
    }

    #[test]
    fn undetermined_verdict_has_all_seven_fields() {
        let err = PackHostError::unavailable(Unavailable::Timeout, "x", Vec::new());
        let v = undetermined_verdict("A2_min_short_side", &err);
        for key in [
            "ruleId", "status", "score", "label", "evidence", "fixed", "note",
        ] {
            assert!(v.get(key).is_some(), "缺字段 {key}");
        }
        assert_eq!(v["status"], "undetermined");
        assert!(v["score"].is_null());
        assert!(!err.is_undetermined() == false);
    }

    #[test]
    fn start_failure_classification() {
        assert_eq!(
            classify_start_failure(Some(3), &[]),
            Unavailable::LoadFailed
        );
        assert_eq!(classify_start_failure(Some(0), &[]), Unavailable::Crash);
        assert_eq!(classify_start_failure(None, &[]), Unavailable::Crash);
        assert_eq!(
            classify_start_failure(None, &["Traceback".to_string()]),
            Unavailable::LoadFailed
        );
    }

    // --------------------------------------------------------------- KI-42
    // Windows 跑高清放大时弹控制台黑窗：插件进程 spawn 未加 CREATE_NO_WINDOW。

    /// ①常量是 winbase.h 原值；②助手函数在 Windows 上确实把该值设进 `Command` 并回报。
    /// 反向验证：把 `no_console_window` 里的 `creation_flags` 那行删掉 → Windows 上 `applied` 变 0 → RED。
    #[test]
    fn console_window_is_suppressed_on_windows() {
        assert_eq!(
            CREATE_NO_WINDOW, 0x0800_0000,
            "CREATE_NO_WINDOW 必须是 Win32 原值 0x08000000"
        );
        let mut cmd = Command::new("python");
        let applied = no_console_window(&mut cmd);
        #[cfg(windows)]
        assert_eq!(
            applied, CREATE_NO_WINDOW,
            "Windows 上必须真的给 Command 设置 creation_flags"
        );
        #[cfg(not(windows))]
        assert_eq!(
            applied, 0,
            "非 Windows 平台不报告 flags（Linux 进程组/窗口行为必须不变）"
        );
    }

    /// 调用点不可被误删：`Conn::spawn` 起插件进程前必须过 `no_console_window`。
    ///
    /// 进程级可见性（真机有没有黑窗）由 Windows 真机脚本
    /// `scripts/win/ki42_nowindow_probe.ps1` 断言；这里守的是「接线还在」——
    /// `std::process::Command` 不暴露已设的 flags，光删那一行不会有别的红。
    #[test]
    fn spawn_site_applies_no_console_window() {
        let src = include_str!("process.rs");
        let body = src.split("fn spawn(").nth(1).expect("找不到 Conn::spawn");
        let head = &body[..body.len().min(4000)];
        assert!(
            head.contains("no_console_window(&mut cmd)"),
            "Conn::spawn 必须给插件进程设 CREATE_NO_WINDOW（KI-42）"
        );
    }
}
