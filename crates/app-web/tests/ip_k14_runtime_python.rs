//! KI-28 接线钉：`runtime_python` 从「安装包资源」到「`PackHostConfig`」的整条链路（卡 `t_5c9accc5`）。
//!
//! 出包实测（K14 `reports/evidence/K14/installer-7z-list.txt`）里 `grep -i python` 命中 0 条，
//! 而 `crates/app-web`、`crates/app-tauri` 对 `runtime_python` 的引用也是 0 —— 纯净 Windows
//! （无 `python3`）上行业包一律起不来。本文件钉的是**接线**，不是「pack-host 会不会用」：
//!
//! 1. `PackRuntime::resolve_with(desktop=true, <resource>)` 从包内资源里认出解释器与启动器，
//!    且**容器形态（desktop=false）一律不加载**（既有 `python3` 行为不回退）；
//! 2. `compliance_api::pack_host_config` 把解析结果写进 `PackHostConfig.runtime_python`
//!    —— **反向验证点**：把这一行接线摘掉，本文件的 `wiring_*` 用例即红；
//! 3. `GET /api/capabilities.packRuntime` 与 `isDesktop` 口径一致（缺运行时给「缺失」人话，
//!    形态判定仍以 capabilities 为唯一事实源，不在测试里硬编码期望值）。
use axum::body::Body;
use axum::http::Request;
use std::path::{Path, PathBuf};
use tower::ServiceExt;

/// 造一个「像安装包解出来的」资源根：runtime/python/<候选> + pack-launcher/dampack_launcher.py
fn fake_resource_dir(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("dam-rt-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let py = root.join("runtime").join("python").join(python_rel());
    std::fs::create_dir_all(py.parent().unwrap()).unwrap();
    std::fs::write(&py, b"#!/bin/sh\nexit 0\n").unwrap();
    let launcher = root.join("pack-launcher");
    std::fs::create_dir_all(&launcher).unwrap();
    // 真启动器（不是 stub）：这样「随包启动器」这条也被真跑一遍，stub 会让子进程静默退出
    let src = repo_root().join("tools/dampack_launcher.py");
    std::fs::copy(&src, launcher.join("dampack_launcher.py")).expect("拷仓库启动器");
    (root, py, launcher)
}

/// 与 `PackRuntime::resolve_with` 的候选表同口径（Windows 是 `python.exe`，否则 `bin/python3`）
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

fn python_rel() -> &'static str {
    if cfg!(windows) {
        "python.exe"
    } else {
        "bin/python3"
    }
}

fn tmp_data(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dam-rt-data-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 进程级 env（`DAMPACK_RUNTIME_PYTHON` / `DAM_WEB_FORM`）是全局状态，而 cargo test 默认多线程
/// 并行跑**同一个测试二进制里的所有用例**：不串行就会互相污染 ——
/// `env_override_wins_and_bogus_env_is_ignored` 设的 `DAMPACK_RUNTIME_PYTHON` 会被并发跑的
/// `desktop_form_resolves_bundled_runtime_and_launcher` 读到，
/// 断言变成「没认出包内解释器 left=/tmp/dam-rt-env-<pid>… right=/tmp/dam-rt-desktop-<pid>…」，
/// 与代码正确性无关、只与调度顺序有关（门禁随机红）。所有读写这两个 env 的用例都先拿这把锁。
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    // 中毒只可能来自别的用例 panic，这里不该再连锁 panic（否则真实失败被掩盖成二次 panic）
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------- 解析口径

#[test]
fn desktop_form_resolves_bundled_runtime_and_launcher() {
    let _g = env_guard();
    let (root, py, launcher) = fake_resource_dir("desktop");
    let rt = app_web::PackRuntime::resolve_with(true, Some(&root));
    assert_eq!(rt.python.as_deref(), Some(py.as_path()), "没认出包内解释器");
    assert_eq!(rt.launcher_dir.as_deref(), Some(launcher.as_path()));

    // 资源根本不存在的桌面包（= KI-28 的原状）：解析成 None，而不是编一个路径出来
    let empty = std::env::temp_dir().join("dam-rt-empty-desktop");
    let _ = std::fs::create_dir_all(&empty);
    let rt_missing = app_web::PackRuntime::resolve_with(true, Some(&empty));
    assert_eq!(rt_missing, app_web::PackRuntime::default());
}

#[test]
fn container_form_never_loads_bundled_runtime() {
    let _g = env_guard();
    let (root, _py, _launcher) = fake_resource_dir("container");
    // 容器形态即使资源根里真有 python 也不用（同一份安装目录被容器挂进去时不许改行为）
    assert_eq!(
        app_web::PackRuntime::resolve_with(false, Some(&root)),
        app_web::PackRuntime::default()
    );
}

#[test]
fn env_override_wins_and_bogus_env_is_ignored() {
    let _g = env_guard();
    let (root, py, _launcher) = fake_resource_dir("env");
    std::env::set_var("DAMPACK_RUNTIME_PYTHON", &py);
    let rt = app_web::PackRuntime::resolve_with(true, Some(&root));
    assert_eq!(rt.python.as_deref(), Some(py.as_path()));

    // 指到不存在的文件 = 当作没配（否则会把「路径打错」变成「运行时缺失」的黑盒）
    std::env::set_var("DAMPACK_RUNTIME_PYTHON", root.join("nope/python.exe"));
    let rt2 = app_web::PackRuntime::resolve_with(true, Some(&root));
    assert_eq!(rt2.python.as_deref(), Some(py.as_path()));
    std::env::remove_var("DAMPACK_RUNTIME_PYTHON");
}

// -------------------------------------------- 接线：配置真的被写进 PackHostConfig

#[test]
fn wiring_sets_runtime_python_on_pack_host_config() {
    let _g = env_guard();
    let (root, py, launcher) = fake_resource_dir("wiring");
    let rt = app_web::PackRuntime::resolve_with(true, Some(&root));
    let work = tmp_data("wiring-work");
    let cfg = app_web::compliance_api::pack_host_config(
        "http://127.0.0.1:1",
        "tok",
        &work,
        &rt,
    );
    // 反向验证点：`pack_host_config` 里若摘掉 `cfg.runtime_python = rt.python.clone()`，这里必红
    assert_eq!(
        cfg.runtime_python.as_deref(),
        Some(py.as_path()),
        "内置 runtime 没被接进 PackHostConfig（纯净 Windows 会回落裸 python3 → 插件起不来）"
    );
    assert_eq!(
        cfg.launcher_dir.as_deref(),
        Some(launcher.as_path()),
        "随包启动器目录没被接进 PackHostConfig"
    );

    // 容器形态：两者皆 None（`python3` + 仓库 tools/ 兜底，既有行为不回退）
    let rt_container = app_web::PackRuntime::resolve_with(false, Some(&root));
    let cfg_c = app_web::compliance_api::pack_host_config(
        "http://127.0.0.1:1",
        "tok",
        &work,
        &rt_container,
    );
    assert_eq!(cfg_c.runtime_python, None, "容器形态不许被塞内置 runtime");
}

// ------------------------------------- 可观测：/api/capabilities 里的人话状态

async fn caps(app: &axum::Router) -> serde_json::Value {
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/api/capabilities").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let raw = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    serde_json::from_slice(&raw).unwrap()
}

#[tokio::test]
async fn capabilities_reports_pack_runtime_consistently_with_form() {
    // 形态是进程级环境变量：与本文件其它用例共用一把锁，显式钉成桌面形态（与 os_reveal_web_form 同做法）
    let _g = env_guard();
    std::env::set_var("DAM_WEB_FORM", "desktop");
    let data = tmp_data("caps");

    // 有运行时：bundled=true 且报出真实路径
    let (root, py, launcher) = fake_resource_dir("caps");
    let mut s_bundled = app_web::test_support::state(data.clone(), open_db(&data));
    s_bundled.pack_runtime = app_web::PackRuntime::resolve_with(true, Some(&root));
    let body = caps(&app_web::router(s_bundled, data.clone())).await;
    let pr = &body["packRuntime"];
    assert_eq!(pr["bundled"], true, "capabilities.packRuntime 没报出 bundled：{body}");
    assert_eq!(
        pr["python"].as_str().map(PathBuf::from),
        Some(py.clone()),
        "capabilities 里的 python 路径不对：{body}"
    );
    assert_eq!(
        pr["launcherDir"].as_str().map(PathBuf::from),
        Some(launcher.clone()),
        "capabilities 里的启动器目录不对：{body}"
    );
    assert!(pr["note"].is_null(), "有运行时不该有缺失提示：{body}");

    // 桌面形态 + 没运行时（= KI-28 原状）：必须给「缺失」人话
    let mut s_missing = app_web::test_support::state(data.clone(), open_db(&data));
    s_missing.pack_runtime = app_web::PackRuntime::resolve_with(true, None);
    let body = caps(&app_web::router(s_missing, data.clone())).await;
    let note = body["packRuntime"]["note"].as_str().unwrap_or_default();
    assert!(
        note.contains("缺失") && note.contains("DAMPACK_RUNTIME_PYTHON"),
        "桌面形态缺运行时的提示要能指路，实际 {note:?}"
    );
    // 形态口径与 isDesktop 同源（不硬编码，跟着能力位走）
    assert_eq!(
        body["isDesktop"].as_bool().unwrap_or(false),
        app_web::is_desktop_form(),
        "capabilities.isDesktop 与 is_desktop_form 口径不一致"
    );
    // 容器形态文案（不走 HTTP，避免翻 env 与其它断言竞态）
    let container_note = app_web::PackRuntime::default().describe_for(false)["note"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        container_note.contains("python3"),
        "容器形态说明应提系统 python3，实际 {container_note:?}"
    );
}


// ------------------------- 端到端（假解释器）：接线断了这条必须红（反向验证点）

#[cfg(unix)]
#[test]
fn wiring_makes_host_spawn_the_bundled_interpreter() {
    use std::os::unix::fs::PermissionsExt;

    let _g = env_guard();
    // 资源根里放一个「会记录 argv 再 exec 真 python3」的假解释器
    let (root, py, _launcher) = fake_resource_dir("spawn");
    let rec = root.join("interpreter.argv");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$0\" \"$@\" > '{}'\nexec python3 \"$@\"\n",
        rec.display()
    );
    std::fs::write(&py, script).unwrap();
    let mut perm = std::fs::metadata(&py).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&py, perm).unwrap();

    let rt = app_web::PackRuntime::resolve_with(true, Some(&root));
    let work = tmp_data("spawn-work");
    let cfg = app_web::compliance_api::pack_host_config("http://127.0.0.1:1", "tok", &work, &rt);
    let pack = repo_root().join("tools/pack-conformance/fixtures/good");
    let host = pack_host::PackHost::open(&pack, cfg).expect("开包");
    let info = host
        .call("pack.info", serde_json::json!({}))
        .expect("内置 runtime 起子进程后 pack.info 必须可应答");
    assert_eq!(
        info.get("id").and_then(|v| v.as_str()),
        Some("conformance.good"),
        "pack.info 身份不对：{info}"
    );

    // 反向验证点：把 `pack_host_config` 里的 runtime_python 接线摘掉 → 假解释器不会被拉起 → 这里红
    let recorded = std::fs::read_to_string(&rec)
        .expect("内置 runtime 没被子进程拉起：pack_host_config 的 runtime_python 接线断了");
    let first = recorded.lines().next().unwrap_or("");
    assert_eq!(
        first,
        py.to_string_lossy(),
        "被拉起的解释器不是内置 runtime：{recorded}"
    );
}

fn open_db(dir: &Path) -> rusqlite::Connection {
    // 与其它路由级测试同源：core-storage 的 open 负责建库 + 迁移
    core_storage::open(&dir.join("dam.db")).unwrap()
}
