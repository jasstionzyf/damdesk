//! KI-9 / KI-10（机制层）：`POST /api/os/reveal` —— 在系统文件管理器中「打开文件夹 / 在文件夹中显示」。
//!
//! 分层位置：L3 进程壳（薄壳）。判定口径全部复用既有单一事实源，本文件不另写一套：
//! - 形态门控 = `svc_library::capabilities::evaluate_features(..).open_in_os`（容器形态恒 false）
//! - 库根清单 = `svc_library::capabilities::library_roots`（数据目录 + 全部导入批次根）
//! - 路径归属 = `svc_library::capabilities::path_inside`（按路径段比，`/data` 不含 `/database`）
//! - 禁用文案 = `capabilities_api::open_in_os_reason()`（与 `featureReasons.openInOS` 同一句）
//!
//! 契约（与前端 `web/src/components/PathLink.tsx` / `web/src/lib/api.ts::osReveal` 对齐）：
//! `POST /api/os/reveal { path, mode: "reveal" | "open" }`
//! - `reveal`（资产级）= 「在文件夹中显示」：Windows `explorer /select,<path>`、macOS `open -R <path>`、
//!   Linux `xdg-open <父目录>`（xdg 没有「选中」语义，退化为打开所在目录）。
//! - `open`（目录级）= 「打开文件夹」：Windows `explorer <dir>`、macOS `open <dir>`、Linux `xdg-open <dir>`。
//!
//! 错误码（`{ error, code }`，前端按 code 给人话）：
//! | HTTP | code | 何时 |
//! |---|---|---|
//! | 400 | `bad_path` | 空 / 相对路径 / 含 `..` |
//! | 400 | `bad_mode` | mode 不是 reveal/open |
//! | 400 | `path_not_allowed` | **白名单外**（安全红线：不做任意路径打开器） |
//! | 400 | `feature_disabled` | 形态不支持（容器/web）→ 文案同 `featureReasons.openInOS` |
//! | 409 | `path_missing` | 路径不存在/已断开（可移动盘？） |
//! | 500 | `reveal_failed` | 系统命令拉不起来（找不到 explorer/xdg-open 等） |
//!
//! 检查顺序（刻意的）：
//! 路径形状 → 白名单 → 形态门控 → 存在性 → 执行。
//! 白名单在形态门控之前，是为了让「界面上的路径都能点」这条安全边界在**两种形态**下
//! 口径一致（网页形态拿库外路径来问也一样是 `path_not_allowed`，而不是被形态挡掉后含糊不清）；
//! 代价只是能探到「某路径是否属于本库」，不含文件内容、也不泄露存在性（缺失另有 409）。

use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::{is_desktop_form, AppState};

// ---------- 语义 ----------

/// 打开语义（两种语义必须分开，不许一个入口两种含义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 资产级：「在文件夹中显示」——打开所在目录并选中文件
    Reveal,
    /// 目录级：「打开文件夹」——直接打开该目录
    Open,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "reveal" => Some(Mode::Reveal),
            "open" => Some(Mode::Open),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Reveal => "reveal",
            Mode::Open => "open",
        }
    }

    /// 界面文案（与 `path-reveal-matrix.md` §1 一致：两种语义各有固定词）。
    pub fn label(self) -> &'static str {
        match self {
            Mode::Reveal => "在文件夹中显示",
            Mode::Open => "打开文件夹",
        }
    }
}

/// 目标平台。测试可注入：在 Linux 上也要能断言 Windows / macOS 的参数形状
/// （真机行为归链末收尾卡，这里守的是「参数拼对」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanOs {
    Windows,
    MacOs,
    Other,
}

pub fn current_os() -> PlanOs {
    #[cfg(target_os = "windows")]
    {
        PlanOs::Windows
    }
    #[cfg(target_os = "macos")]
    {
        PlanOs::MacOs
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        PlanOs::Other
    }
}

/// 要执行的一条系统命令（纯数据：测试不用真起进程就能断言）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevealPlan {
    pub program: String,
    pub args: Vec<String>,
    pub mode: Mode,
    /// 计划所针对的**原始**路径（用于响应/日志，不随 open 退化成父目录而改）
    pub path: String,
}

/// 平台命令拼装（唯一实现，桌面壳/容器共用）。
///
/// `is_file`：路径是文件（= 资产级）还是目录（= 目录级）。`mode=open` 时给文件也打开其父目录
/// （用户点「打开文件夹」不可能想要一个文件被某个 App 打开）。
pub fn plan_for(os: PlanOs, path: &str, mode: Mode, is_file: bool) -> RevealPlan {
    let parent = parent_of(path);
    let dir_target = if is_file { parent } else { path.to_string() };
    let (program, args) = match (os, mode) {
        // 资产级：打开目录并选中文件
        (PlanOs::Windows, Mode::Reveal) if is_file => {
            ("explorer".to_string(), vec![format!("/select,{path}")])
        }
        (PlanOs::MacOs, Mode::Reveal) if is_file => {
            ("open".to_string(), vec!["-R".to_string(), path.to_string()])
        }
        // xdg-open 没有「选中」语义：reveal 退化成打开所在目录（不是不做事）
        (PlanOs::Other, _) => ("xdg-open".to_string(), vec![dir_target]),
        // 目录级（以及「文件 + reveal」在 Windows/macOS 已在上分支处理）
        (PlanOs::Windows, _) => ("explorer".to_string(), vec![dir_target]),
        (PlanOs::MacOs, _) => ("open".to_string(), vec![dir_target]),
    };
    RevealPlan {
        program,
        args,
        mode,
        path: path.to_string(),
    }
}

fn parent_of(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| path.to_string())
}

// ---------- 错误 ----------

/// 拒绝原因：`status` + 稳定 `code` + 人话 `message`（前端按 code 归因，不解析文案）。
#[derive(Debug, Clone)]
pub struct RevealError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl RevealError {
    fn bad_path(message: impl Into<String>) -> Self {
        RevealError {
            status: StatusCode::BAD_REQUEST,
            code: "bad_path",
            message: message.into(),
        }
    }
    fn bad_mode(m: &str) -> Self {
        RevealError {
            status: StatusCode::BAD_REQUEST,
            code: "bad_mode",
            message: format!("mode 只能是 reveal（在文件夹中显示）或 open（打开文件夹）：{m}"),
        }
    }
    fn not_allowed(path: &str) -> Self {
        RevealError {
            status: StatusCode::BAD_REQUEST,
            code: "path_not_allowed",
            message: format!(
                "该路径不在允许打开的范围内（仅库根/导入源/导出目标/回收站原路径）：{path}"
            ),
        }
    }
    fn feature_disabled() -> Self {
        RevealError {
            status: StatusCode::BAD_REQUEST,
            code: "feature_disabled",
            message: crate::capabilities_api::open_in_os_reason(),
        }
    }
    fn missing(path: &str) -> Self {
        RevealError {
            status: StatusCode::CONFLICT,
            code: "path_missing",
            message: format!("目标不存在或已断开（可移动盘？）：{path}"),
        }
    }
    fn failed(message: impl Into<String>) -> Self {
        RevealError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "reveal_failed",
            message: message.into(),
        }
    }

    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({ "error": self.message, "code": self.code })),
        )
            .into_response()
    }
}

// ---------- 白名单 ----------

/// 路径形状校验（与白名单无关的硬约束）。
pub fn check_shape(path: &Path) -> Result<(), RevealError> {
    if path.as_os_str().is_empty() {
        return Err(RevealError::bad_path("path 不能为空"));
    }
    if !path.is_absolute() {
        return Err(RevealError::bad_path(format!(
            "path 必须是绝对路径：{}",
            path.display()
        )));
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(RevealError::bad_path(format!(
            "path 不允许包含 `..`：{}",
            path.display()
        )));
    }
    Ok(())
}

/// 路径是否落在允许范围内（**安全红线**：接口不许变成任意路径打开器）。
///
/// 允许三类：
/// 1. `roots`（库根：数据目录 + 全部导入批次根）及其子路径 —— 覆盖资产原图/目录/导入源/回收站原路径；
/// 2. `exact`（精确路径：导出目标 dest / 导出 zip、命中资产的 absPath）；
/// 3. `roots` 中的导出目录同样按子树放行（导出目录里的产物要能 reveal）。
///
/// 比较用 `canonicalize`（存在时）消除 `//`、符号链接差异；不存在时按原样比。
pub fn check_whitelist(
    path: &Path,
    roots: &[PathBuf],
    exact: &[PathBuf],
) -> Result<(), RevealError> {
    let cand = canon_or_self(path);
    if roots.iter().any(|r| {
        let root = canon_or_self(r);
        inside(&cand, &root)
    }) {
        return Ok(());
    }
    if exact.iter().any(|e| canon_or_self(e) == cand) {
        return Ok(());
    }
    Err(RevealError::not_allowed(&path.to_string_lossy()))
}

fn canon_or_self(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn inside(target: &Path, root: &Path) -> bool {
    let root_str = root.to_string_lossy();
    let t = target.to_string_lossy();
    svc_library::capabilities::path_inside(Path::new(&norm_key(&t)), &norm_key(&root_str))
}

/// Windows 盘符大小写不敏感（`c:\x` 与 `C:\x` 是同一位置）；其余平台原样。
fn norm_key(s: &str) -> String {
    #[cfg(windows)]
    {
        s.to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        s.to_string()
    }
}

/// 从 DB 采集白名单：库根 + 导出目标（`job_queue kind='export'` 最近 100 条的 payload.dest）
/// + 该路径恰好等于某个资产 absPath 的精确命中（回收站项/失联项可能已不在任何根下）。
pub fn allowlist(
    conn: &rusqlite::Connection,
    data_dir: &Path,
    path: &str,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let roots = svc_library::capabilities::library_roots(conn, data_dir);

    let mut exact: Vec<PathBuf> = Vec::new();
    // ① 导出目标（dest 可能是目录，也可能是 zip 文件路径）——按子树放行
    let mut export_roots: Vec<PathBuf> = Vec::new();
    if let Ok(mut stmt) =
        conn.prepare("SELECT payload FROM job_queue WHERE kind='export' ORDER BY id DESC LIMIT 100")
    {
        if let Ok(rows) = stmt.query_map([], |r| r.get::<_, Option<String>>(0)) {
            for payload in rows.flatten().flatten() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&payload) {
                    if let Some(dest) = v.get("dest").and_then(|d| d.as_str()) {
                        if !dest.trim().is_empty() {
                            let pb = PathBuf::from(dest.trim());
                            export_roots.push(pb.clone());
                            exact.push(pb);
                        }
                    }
                }
            }
        }
    }
    let mut all_roots = roots;
    all_roots.extend(export_roots);

    // ② 精确命中资产 absPath（不整表扫描：只按本条路径查）
    let hit: Option<i64> = conn
        .query_row(
            "SELECT assetId FROM assets WHERE absPath=?1 LIMIT 1",
            [path.trim()],
            |r| r.get(0),
        )
        .ok();
    if hit.is_some() {
        exact.push(PathBuf::from(path.trim()));
    }
    (all_roots, exact)
}

// ---------- 执行（可注入，测试不真拉文件管理器） ----------

/// 执行器签名。默认真起子进程；测试替换成「记录 + 返回 Ok」。
pub type Runner = fn(&RevealPlan) -> Result<(), String>;

fn spawn_runner(plan: &RevealPlan) -> Result<(), String> {
    Command::new(&plan.program)
        .args(&plan.args)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("启动 {} 失败：{e}（{}）", plan.program, plan.path))
}

fn runner() -> MutexGuard<'static, Runner> {
    static RUNNER: OnceLock<Mutex<Runner>> = OnceLock::new();
    RUNNER
        .get_or_init(|| Mutex::new(spawn_runner))
        .lock()
        .unwrap()
}

/// 测试钩子：替换执行器（避免真的拉起 Finder/Explorer）。
#[doc(hidden)]
pub fn set_runner(r: Runner) {
    *runner() = r;
}

/// 测试钩子：恢复默认真实执行器。
#[doc(hidden)]
pub fn reset_runner() {
    *runner() = spawn_runner;
}

// ---------- 端点 ----------

/// `POST /api/os/reveal` 请求体。
#[derive(Debug, Deserialize)]
pub struct RevealReq {
    pub path: String,
    /// 缺省 = `reveal`（资产级是绝大多数触点；目录级触点必须显式传 `open`）
    #[serde(default)]
    pub mode: Option<String>,
}

/// 纯逻辑：给定形态与白名单，决定「该不该做、做什么」（HTTP 层只是壳）。
pub fn decide(
    req: &RevealReq,
    is_desktop: bool,
    roots: &[PathBuf],
    exact: &[PathBuf],
    os: PlanOs,
) -> Result<RevealPlan, RevealError> {
    let mode = match req.mode.as_deref() {
        None => Mode::Reveal,
        Some(m) => Mode::parse(m).ok_or_else(|| RevealError::bad_mode(m))?,
    };
    let path = Path::new(req.path.trim());
    // 1) 形状 → 2) 白名单（安全红线，先于形态门控，两形态口径一致）
    check_shape(path)?;
    check_whitelist(path, roots, exact)?;
    // 3) 形态门控（容器形态直接拒绝，不做任何系统调用）
    if !svc_library::capabilities::evaluate_features(is_desktop, &[]).open_in_os {
        return Err(RevealError::feature_disabled());
    }
    // 4) 存在性（路径已不存在/盘已卸 → 409，不静默无反应）
    let md = std::fs::metadata(path).map_err(|_| RevealError::missing(&req.path))?;
    Ok(plan_for(os, req.path.trim(), mode, md.is_file()))
}

/// `POST /api/os/reveal` —— 在系统文件管理器中打开/选中。
pub async fn reveal(State(state): State<AppState>, Json(req): Json<RevealReq>) -> Response {
    let is_desktop = is_desktop_form();
    let (roots, exact) = {
        let conn = state.db.lock().await;
        allowlist(&conn, &state.data_dir, &req.path)
    };
    let plan = match decide(&req, is_desktop, &roots, &exact, current_os()) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let runner = *runner();
    match runner(&plan) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "path": plan.path,
                "mode": plan.mode.as_str(),
                "label": plan.mode.label(),
                "program": plan.program,
                "args": plan.args,
            })),
        )
            .into_response(),
        Err(e) => RevealError::failed(e).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "dam-os-reveal-{}-{}-{}",
            tag,
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 平台命令拼装：三种平台 × 两种语义（Linux 上断言 Windows/macOS 参数形状）。
    #[test]
    fn plan_matches_platform_and_mode() {
        let win_reveal_file = plan_for(PlanOs::Windows, "C:\\pics\\a.jpg", Mode::Reveal, true);
        assert_eq!(win_reveal_file.program, "explorer");
        assert_eq!(win_reveal_file.args, vec!["/select,C:\\pics\\a.jpg"]);

        let win_open_dir = plan_for(PlanOs::Windows, "C:\\pics", Mode::Open, false);
        assert_eq!(win_open_dir.args, vec!["C:\\pics"]);

        let mac_reveal = plan_for(PlanOs::MacOs, "/Users/u/pics/a.jpg", Mode::Reveal, true);
        assert_eq!(mac_reveal.program, "open");
        assert_eq!(mac_reveal.args, vec!["-R", "/Users/u/pics/a.jpg"]);

        let mac_open = plan_for(PlanOs::MacOs, "/Users/u/pics", Mode::Open, false);
        assert_eq!(mac_open.args, vec!["/Users/u/pics"]);

        // xdg-open 无「选中」语义 → reveal 退化成打开所在目录（不是不做事）
        let linux_reveal = plan_for(PlanOs::Other, "/data/lib/a.jpg", Mode::Reveal, true);
        assert_eq!(linux_reveal.program, "xdg-open");
        assert_eq!(linux_reveal.args, vec!["/data/lib"]);

        // open 给文件 → 打开父目录（用户点「打开文件夹」不该让文件被某个 App 打开）
        let linux_open_file = plan_for(PlanOs::Other, "/data/lib/a.jpg", Mode::Open, true);
        assert_eq!(linux_open_file.args, vec!["/data/lib"]);
        // 响应里保留原始路径，不改写成父目录
        assert_eq!(linux_open_file.path, "/data/lib/a.jpg");
    }

    /// 形状校验：空/相对/`..` 一律 bad_path（不做路径清洗后放行）。
    #[test]
    fn shape_rejects_relative_and_parent_dir() {
        assert_eq!(check_shape(Path::new("")).unwrap_err().code, "bad_path");
        assert_eq!(
            check_shape(Path::new("rel/a.jpg")).unwrap_err().code,
            "bad_path"
        );
        assert_eq!(
            check_shape(Path::new("/data/../etc/passwd"))
                .unwrap_err()
                .code,
            "bad_path"
        );
        assert!(check_shape(Path::new("/data/lib/a.jpg")).is_ok());
    }

    /// 白名单：根内子路径放行，根外路径拒绝（含 `/etc` 与 `C:\Windows` 这类经典越界）。
    #[test]
    fn whitelist_allows_inside_roots_only() {
        let root = tmpdir("whitelist");
        std::fs::write(root.join("a.jpg"), b"x").unwrap();
        let roots = vec![root.clone()];
        assert!(check_whitelist(&root.join("a.jpg"), &roots, &[]).is_ok());
        assert!(check_whitelist(&root, &roots, &[]).is_ok());

        for outside in [
            "/etc",
            "/etc/passwd",
            "C:\\Windows",
            "C:\\Windows\\System32",
        ] {
            let e = check_whitelist(Path::new(outside), &roots, &[]).unwrap_err();
            assert_eq!(e.code, "path_not_allowed", "应拒绝 {outside}");
            assert_eq!(e.status, StatusCode::BAD_REQUEST);
        }
        // 路径段边界：同前缀不同目录不算根内
        let sibling = root.with_file_name(format!(
            "{}-sibling",
            root.file_name().unwrap().to_string_lossy()
        ));
        assert_eq!(
            check_whitelist(&sibling, &roots, &[]).unwrap_err().code,
            "path_not_allowed"
        );
        // 精确白名单（导出目标/资产 absPath）放行
        assert!(check_whitelist(&sibling, &roots, std::slice::from_ref(&sibling)).is_ok());
    }

    /// 判定顺序与门控：容器形态（is_desktop=false）→ feature_disabled；
    /// 库内路径在桌面形态下回 plan；库外路径两形态都 path_not_allowed。
    #[test]
    fn decide_gates_form_and_existence() {
        let root = tmpdir("decide");
        let file = root.join("a.jpg");
        std::fs::write(&file, b"x").unwrap();
        let roots = vec![root.clone()];
        let req = |p: &str, m: Option<&str>| RevealReq {
            path: p.to_string(),
            mode: m.map(|s| s.to_string()),
        };

        // 容器形态：库内路径 → feature_disabled（文案与 featureReasons.openInOS 同一句）
        let e = decide(
            &req(file.to_str().unwrap(), None),
            false,
            &roots,
            &[],
            PlanOs::Other,
        )
        .unwrap_err();
        assert_eq!(e.code, "feature_disabled");
        assert!(e.message.contains("系统文件管理器"));

        // 桌面形态：库内文件 → reveal 计划
        let plan = decide(
            &req(file.to_str().unwrap(), Some("reveal")),
            true,
            &roots,
            &[],
            PlanOs::Other,
        )
        .unwrap();
        assert_eq!(plan.mode, Mode::Reveal);
        assert_eq!(plan.args, vec![root.to_string_lossy().to_string()]);

        // 库外路径：两形态都 path_not_allowed（白名单先于门控）
        for desktop in [true, false] {
            let e = decide(
                &req("/etc/passwd", None),
                desktop,
                &roots,
                &[],
                PlanOs::Other,
            )
            .unwrap_err();
            assert_eq!(e.code, "path_not_allowed");
        }

        // 不存在路径（桌面形态、库内）→ 409 path_missing
        let gone = root.join("gone.jpg");
        let e = decide(
            &req(gone.to_str().unwrap(), None),
            true,
            &roots,
            &[],
            PlanOs::Other,
        )
        .unwrap_err();
        assert_eq!(e.code, "path_missing");
        assert_eq!(e.status, StatusCode::CONFLICT);

        // 非法 mode → bad_mode
        let e = decide(
            &req(file.to_str().unwrap(), Some("explode")),
            true,
            &roots,
            &[],
            PlanOs::Other,
        )
        .unwrap_err();
        assert_eq!(e.code, "bad_mode");
    }

    /// 导出目标（job_queue payload.dest）进白名单：不在库根内的导出目录也能 open。
    #[test]
    fn allowlist_includes_export_dest_and_asset_path() {
        let data = tmpdir("allow-data");
        let lib = tmpdir("allow-lib");
        let out = tmpdir("allow-out");
        let conn = core_storage::open(&data.join("dam.db")).unwrap();
        conn.execute(
            "INSERT INTO import_batches (rootPath, storeMode, status, totalFiles, doneFiles, startedAt)
             VALUES (?1, 'reference', 'done', 1, 1, 0)",
            rusqlite::params![lib.to_str().unwrap()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO job_queue (kind, targetId, payload, status, createdAt, updatedAt)
             VALUES ('export', 0, ?1, 'done', 0, 0)",
            rusqlite::params![format!("{{\"dest\":\"{}\"}}", out.to_string_lossy())],
        )
        .unwrap();

        let (roots, exact) = allowlist(&conn, &data, "/elsewhere/none.jpg");
        assert!(roots.contains(&lib), "库根应含导入批次根");
        assert!(roots.contains(&out), "导出目标应进白名单");
        assert!(check_whitelist(&out.join("a.jpg"), &roots, &exact).is_ok());
        let _ = std::fs::remove_dir_all(&data);
        let _ = std::fs::remove_dir_all(&lib);
        let _ = std::fs::remove_dir_all(&out);
    }
}
