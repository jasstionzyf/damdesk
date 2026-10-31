//! 宿主对外 API 的边界面：`verify_pack_info`（协议 §3.1 一致性核对）、
//! `call_recorded`（结果行三元组）、错误分类矩阵。
//!
//! 为什么单列：这三处是 svc 层真正调用的入口，坏掉的症状是「包损坏却照常判」或
//! 「结果行对不上账」，单测层面最便宜的防线就在这里。

use std::fs;
use std::path::{Path, PathBuf};

use pack_host::{PackHost, PackHostConfig, PackHostError, Unavailable};
use serde_json::json;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repo_root() -> PathBuf {
    manifest_dir().join("../..")
}

fn recorder_pack() -> PathBuf {
    manifest_dir().join("tests/fixtures/frame-recorder")
}

fn cfg(work: &Path) -> PackHostConfig {
    let mut c = PackHostConfig::new("http://127.0.0.1:1", "api-token", work);
    c.launcher_dir = Some(repo_root().join("tools"));
    c.health_timeout = std::time::Duration::from_secs(5);
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

fn temp_pack(tag: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let pack = dir.path().join(format!("pack-{tag}"));
    copy_dir(&recorder_pack(), &pack);
    (dir, pack)
}

// ---------------------------------------------------------------------------
// 1. pack.info 与 manifest 的一致性核对
// ---------------------------------------------------------------------------

#[test]
fn verify_pack_info_accepts_consistent_pack() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let host = PackHost::open(recorder_pack(), cfg(&work)).unwrap();
    let problems = host.verify_pack_info().expect("pack.info 调用失败");
    assert!(problems.is_empty(), "一致包不应有不一致项：{problems:?}");
}

#[test]
fn verify_pack_info_reports_version_and_capability_drift() {
    let (_tmp, pack) = temp_pack("drift");
    // 改 manifest：version 与 capabilities 与插件自报的 PACK_INFO 不一致
    let manifest_path = pack.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["version"] = json!("9.9.9");
    manifest["capabilities"] = json!(["io.read", "vl.judge"]);
    manifest["timeout_secs"] = json!(42);
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let host = PackHost::open(&pack, cfg(&work)).unwrap();
    let problems = host.verify_pack_info().unwrap();
    let joined = problems.join(" | ");
    assert!(
        joined.contains("version"),
        "版本漂移必须被点名（否则结果行的 pack@version 是假的）：{joined}"
    );
    assert!(joined.contains("9.9.9"), "{joined}");
    assert!(
        joined.contains("capabilities"),
        "能力集漂移必须被点名（插件实际能力与 manifest 授权不一致）：{joined}"
    );
    assert!(
        joined.contains("timeout_secs"),
        "超时漂移必须被点名：{joined}"
    );
    // id / api_version 一致 → 不应被误报
    assert!(!joined.contains("api_version"), "{joined}");
}

// ---------------------------------------------------------------------------
// 2. call_recorded：结果行的身份三元组
// ---------------------------------------------------------------------------

#[test]
fn call_recorded_carries_identity_and_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let host = PackHost::open(recorder_pack(), cfg(&work)).unwrap();

    let ok = host.call_recorded("echo", json!({ "job_id": "j-rec" }));
    assert_eq!(ok.pack_id, "test.frame-recorder");
    assert_eq!(ok.version, "1.0.0");
    assert_eq!(ok.plugin_hash.len(), 64, "pluginHash 必须是 sha256 hex");
    assert!(ok.deterministic, "manifest 声明 deterministic=true");
    assert_eq!(ok.method, "echo");
    assert_eq!(ok.result.as_ref().unwrap()["echo"]["job_id"], "j-rec");

    // 失败也要带同一套身份（结果行不能因为失败就丢三元组）
    let bad = host.call_recorded("rpc_error", json!({}));
    assert_eq!(bad.pack_id, "test.frame-recorder");
    assert_eq!(bad.version, "1.0.0");
    assert_eq!(bad.plugin_hash, ok.plugin_hash, "同一包的 hash 必须稳定");
    assert!(bad.result.is_err(), "{:?}", bad.result);

    // 身份三元组与 manifest 侧一致（结果行据此可追溯）
    assert_eq!(host.identity().plugin_hash, ok.plugin_hash);
}

// ---------------------------------------------------------------------------
// 3. 错误分类矩阵（note / undetermined 两个口径不能互相污染）
// ---------------------------------------------------------------------------

#[test]
fn error_classification_matrix() {
    let unavailable = [
        (Unavailable::Timeout, "plugin_timeout"),
        (Unavailable::Crash, "plugin_crash"),
        (Unavailable::HealthTimeout, "plugin_health_timeout"),
        (Unavailable::LoadFailed, "plugin_load_failed"),
    ];
    for (kind, note) in unavailable {
        assert_eq!(kind.note(), note);
        // 真实故障路径（超时/崩溃/握手超时/加载失败）在 fault_injection.rs 里用真子进程取证，
        // 这里只钉「note 常量 → Verdict.note 字面量」这条契约不许漂移（结果行/报告按字面量分流）。
    }

    // 真错误（协议违例 / 业务错误）既没有 note，也不算 undetermined
    let protocol = PackHostError::Protocol("应答既无 result 也无 error".into());
    assert_eq!(protocol.note(), None);
    assert!(!protocol.is_undetermined());
    let rpc = PackHostError::Rpc {
        code: -32003,
        message: "参数非法".into(),
    };
    assert_eq!(rpc.note(), None);
    assert!(!rpc.is_undetermined());
    let manifest = PackHostError::Manifest(pack_host::ManifestError::Json("坏".into()));
    assert_eq!(manifest.note(), None);
    assert!(!manifest.is_undetermined());
    let io = PackHostError::Io("管道断了".into());
    assert_eq!(io.note(), None);
    assert!(!io.is_undetermined());
}
