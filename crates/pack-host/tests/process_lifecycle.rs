//! 生命周期集成测试 —— 六个必测场景（协议 §2 / §2.1 / §9）。
//!
//! 全部走**真实子进程 + 真实 stdio 管道**，fixtures 用 K01 的 Python 假插件
//! （`tools/pack-conformance/fixtures/good`）与本 crate 的三个故障夹具。
//!
//! 1. 正常握手（懒启动 + pack.info 与 manifest 一致）
//! 2. 健康超时（handshake 挂死 → plugin_health_timeout）
//! 3. 调用超时（cancel → 宽限 → 插件以 -32004 收尾则保留进程；结果 plugin_timeout）
//! 4. `kill -9` 后自动重启
//! 5. 连续崩溃 → 冷却（plugin_crash），冷却期不再拉起
//! 6. manifest 未知键 / api_version 不匹配 → 拒装
//! 附加：启动器 import 失败 → plugin_load_failed(<原因>) 可读

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::json;

use pack_host::{PackHost, PackHostConfig, PackHostError, Unavailable};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repo_root() -> PathBuf {
    manifest_dir().join("../..")
}

/// 契约套件目录（内含 `dampack_launcher.py`）。
fn launcher_dir() -> PathBuf {
    repo_root().join("tools")
}

/// K01 的参考实现包（正确的包长什么样）。
fn good_pack() -> PathBuf {
    launcher_dir().join("pack-conformance/fixtures/good")
}

fn fixture(name: &str) -> PathBuf {
    manifest_dir().join("tests/fixtures").join(name)
}

fn cfg(work: &Path) -> PackHostConfig {
    let mut c = PackHostConfig::new("http://127.0.0.1:1", "tok-unit-test", work);
    c.launcher_dir = Some(launcher_dir());
    c.health_timeout = Duration::from_secs(5);
    c.cancel_grace = Duration::from_secs(2);
    c
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let target = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

fn assert_unavailable(err: &PackHostError, expected: Unavailable) -> &'static str {
    match err {
        PackHostError::Unavailable { kind, .. } => {
            assert_eq!(*kind, expected, "期望 {expected:?}，实得 {err}");
            kind.note()
        }
        other => panic!("期望 Unavailable，实得 {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. 正常握手
// ---------------------------------------------------------------------------
#[test]
fn t01_handshake_is_lazy_and_pack_info_matches_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let host = PackHost::open(good_pack(), cfg(tmp.path())).unwrap();

    // 身份三元组（§7 铁律 7）
    assert_eq!(host.identity().id, "conformance.good");
    assert_eq!(host.identity().version, "1.0.0");
    assert_eq!(host.identity().plugin_hash.len(), 64);
    assert!(host
        .identity()
        .label()
        .starts_with("conformance.good@1.0.0#"));

    // 懒启动：open 不拉起子进程
    assert!(host.pid().is_none(), "open() 只读 manifest，不应拉子进程");
    assert_eq!(host.spawn_count(), 0);

    let health = host
        .call_with_timeout("health", json!({}), Duration::from_secs(5))
        .unwrap();
    assert_eq!(health["status"], "ok");
    assert_eq!(health["api_version"], "1.1");
    assert!(host.pid().is_some());
    assert_eq!(host.spawn_count(), 1);

    // pack.info 与 manifest 必须完全一致（§3.1）
    assert_eq!(host.verify_pack_info().unwrap(), Vec::<String>::new());
    // 环境变量注入（§1.1）：网关地址/令牌/包根/工作目录
    let info = host
        .call_with_timeout("pack.info", json!({}), Duration::from_secs(5))
        .unwrap();
    assert_eq!(info["id"], "conformance.good");
    // stdout 纯净（污染行必须为 0）
    assert!(host.stray_stdout().is_empty(), "{:?}", host.stray_stdout());

    host.shutdown();
}

// ---------------------------------------------------------------------------
// 2. 健康超时
// ---------------------------------------------------------------------------
#[test]
fn t02_health_timeout_marks_plugin_health_timeout_and_cools_down() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = cfg(tmp.path());
    c.health_timeout = Duration::from_millis(400);
    c.max_restarts = 1;

    let host = PackHost::open(fixture("hang-health"), c).unwrap();
    let started = Instant::now();
    let err = host.call("health", json!({})).unwrap_err();
    let note = assert_unavailable(&err, Unavailable::HealthTimeout);
    assert_eq!(note, "plugin_health_timeout");
    assert_eq!(host.spawn_count(), 2, "1 次启动 + 1 次重试");
    assert!(started.elapsed() < Duration::from_secs(10), "{started:?}");

    // 冷却期：不再拉起，快速失败
    let spawns = host.spawn_count();
    let began = Instant::now();
    let err2 = host.call("health", json!({})).unwrap_err();
    assert!(err2.to_string().contains("冷却"), "{err2}");
    assert!(began.elapsed() < Duration::from_millis(500), "{began:?}");
    assert_eq!(host.spawn_count(), spawns);
}

// ---------------------------------------------------------------------------
// 3. 调用超时 → cancel → 宽限 → 保留/终止
// ---------------------------------------------------------------------------
#[test]
fn t03_call_timeout_sends_cancel_and_keeps_acknowledging_plugin() {
    let tmp = tempfile::tempdir().unwrap();
    let pack = tmp.path().join("pack");
    copy_dir(&good_pack(), &pack);
    // 单次预算压到 1s（manifest.timeout_secs 下限），否则默认 60s 跑不完
    let manifest = fs::read_to_string(pack.join("manifest.json"))
        .unwrap()
        .replace("\"timeout_secs\": 60", "\"timeout_secs\": 1");
    fs::write(pack.join("manifest.json"), manifest).unwrap();

    let host = PackHost::open(&pack, cfg(tmp.path())).unwrap();
    let asset = tmp.path().join("assets/slow-sample_1600x1600.jpg");
    fs::create_dir_all(asset.parent().unwrap()).unwrap();
    fs::write(&asset, b"stub").unwrap();

    let started = Instant::now();
    let err = host
        .call(
            "judge",
            json!({
                "job_id": "job-t3",
                "asset": {"id": "asset-t3", "path": asset, "mime": "image/jpeg",
                          "width": 1600, "height": 1600},
                "rules": ["A2_min_short_side"]
            }),
        )
        .unwrap_err();
    let elapsed = started.elapsed();

    // 超时绝不落 pass/fail，一律 undetermined(plugin_timeout)
    assert_eq!(
        assert_unavailable(&err, Unavailable::Timeout),
        "plugin_timeout"
    );
    assert!(
        elapsed >= Duration::from_millis(900) && elapsed < Duration::from_secs(8),
        "超时应约等于预算 1s + 宽限，实得 {elapsed:?}"
    );
    // 参考实现收到 cancel 后以 -32004 收尾 → 进程保留、不重启
    let health = host
        .call_with_timeout("health", json!({}), Duration::from_secs(5))
        .unwrap();
    assert_eq!(health["status"], "ok");
    assert_eq!(host.spawn_count(), 1, "cancel 被应答时不该重启");
    host.shutdown();
}

// ---------------------------------------------------------------------------
// 4. kill -9 后自动重启
// ---------------------------------------------------------------------------
#[test]
fn t04_hard_crash_restarts_automatically() {
    let tmp = tempfile::tempdir().unwrap();
    let host = PackHost::open(good_pack(), cfg(tmp.path())).unwrap();
    let first = host
        .call_with_timeout("health", json!({}), Duration::from_secs(5))
        .unwrap();
    assert_eq!(first["status"], "ok");
    let pid_before = host.pid().expect("握手后应有 pid");

    // 等价于 `kill -9`：外部把子进程打死
    host.simulate_crash();
    std::thread::sleep(Duration::from_millis(150));

    let health = host
        .call_with_timeout("health", json!({}), Duration::from_secs(5))
        .unwrap();
    assert_eq!(health["status"], "ok", "崩溃后应自动重启并重新握手");
    assert_eq!(host.spawn_count(), 2);
    assert_ne!(host.pid().unwrap(), pid_before, "应是新进程");
    host.shutdown();
}

// ---------------------------------------------------------------------------
// 5. 连续崩溃 → 冷却
// ---------------------------------------------------------------------------
#[test]
fn t05_repeated_crash_enters_cooldown() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = cfg(tmp.path());
    c.max_restarts = 2;
    c.cooldown = Duration::from_secs(30);
    let host = PackHost::open(fixture("crash-after-health"), c).unwrap();

    let err = host
        .call(
            "judge",
            json!({"job_id": "job-t5", "asset": {"id": "a", "path": "/tmp/x.jpg"}, "rules": []}),
        )
        .unwrap_err();
    assert_eq!(assert_unavailable(&err, Unavailable::Crash), "plugin_crash");
    assert!(err.to_string().contains("崩溃"), "{err}");
    assert!(err.to_string().contains("冷却"), "{err}");
    assert_eq!(host.spawn_count(), 3, "1 次 + 2 次重启后放弃");

    let spawns = host.spawn_count();
    let began = Instant::now();
    let err2 = host.call("health", json!({})).unwrap_err();
    assert_eq!(err2.note(), Some("plugin_crash"));
    assert!(
        began.elapsed() < Duration::from_millis(500),
        "冷却期应立即失败"
    );
    assert_eq!(host.spawn_count(), spawns, "冷却期不得再拉起子进程");
}

// ---------------------------------------------------------------------------
// 6. manifest 拒装
// ---------------------------------------------------------------------------
#[test]
fn t06_manifest_unknown_key_and_bad_api_version_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();

    // 未知键：错误必须点名该键（写错键名静默生效比报错更危险）
    let pack = tmp.path().join("unknown-key");
    fs::create_dir_all(pack.join("plugin")).unwrap();
    fs::write(pack.join("plugin/__init__.py"), "").unwrap();
    fs::write(
        pack.join("manifest.json"),
        r#"{
            "id": "test.unknown-key",
            "name": "未知键",
            "version": "1.0.0",
            "api_version": "1.1",
            "entrypoint": {"cmd": "python3", "args": ["plugin/__init__.py"]},
            "capabilities": ["io.read"],
            "deterministic": true,
            "timeout_secs": 5,
            "fix_policy": "generative"
        }"#,
    )
    .unwrap();
    let err = PackHost::open(&pack, cfg(tmp.path())).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("fix_policy"), "{msg}");

    // api_version 严格相等：1.0 的包装不进 1.1 宿主
    let pack2 = tmp.path().join("api-version");
    fs::create_dir_all(pack2.join("plugin")).unwrap();
    fs::write(pack2.join("plugin/__init__.py"), "").unwrap();
    fs::write(
        pack2.join("manifest.json"),
        r#"{
            "id": "test.api-version",
            "name": "版本不匹配",
            "version": "1.0.0",
            "api_version": "1.0",
            "entrypoint": {"cmd": "python3", "args": ["plugin/__init__.py"]},
            "capabilities": [],
            "deterministic": true,
            "timeout_secs": 5
        }"#,
    )
    .unwrap();
    let err = PackHost::open(&pack2, cfg(tmp.path())).unwrap_err();
    assert!(err.to_string().contains("1.0"), "{err}");
}

// ---------------------------------------------------------------------------
// 附加：启动器失败 → plugin_load_failed(<原因>)，原因要能贴给用户
// ---------------------------------------------------------------------------
#[test]
fn t07_launcher_import_failure_is_reported_as_plugin_load_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = cfg(tmp.path());
    c.max_restarts = 0;
    let host = PackHost::open(fixture("load-fail"), c).unwrap();

    let err = host.call("health", json!({})).unwrap_err();
    assert_eq!(
        assert_unavailable(&err, Unavailable::LoadFailed),
        "plugin_load_failed"
    );
    let msg = err.to_string();
    // 原因透传：venv 缺依赖时用户要看到「缺哪个包」，而不是「插件不可用」
    assert!(msg.contains("onnxruntime"), "{msg}");
    assert!(msg.contains("import 失败"), "{msg}");
    assert_eq!(host.spawn_count(), 1);
}

// ---------------------------------------------------------------------------
// 附加：不可判定结果的 Verdict 形态（供 svc 层直接落库）
// ---------------------------------------------------------------------------
#[test]
fn t08_undetermined_verdict_shape() {
    let err = PackHostError::Unavailable {
        kind: Unavailable::Timeout,
        message: "judge 超过 1000ms".to_string(),
        stderr: vec![],
        timeout: None,
    };
    let v = pack_host::process::undetermined_verdict("A2_min_short_side", &err);
    assert_eq!(v["ruleId"], "A2_min_short_side");
    assert_eq!(v["status"], "undetermined");
    assert!(v["score"].is_null());
    assert_eq!(v["note"], "plugin_timeout");
    assert_eq!(v["fixed"], false);
    assert!(v["evidence"].is_object());
}
