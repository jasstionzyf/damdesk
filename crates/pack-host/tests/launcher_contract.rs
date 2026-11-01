//! 启动器契约测试 —— `tools/dampack_launcher.py` 的五个必测情形：
//!
//! 1. 正常加载（probe 到包本体，`plugin/` 在 sys.path 最前）
//! 2. import 抛异常 → 退出码 3 + 可读原因（stdout 仍为空）
//! 3. `vendor/` 覆盖 shared-site（包内私有依赖优先）
//! 4. 缺 `vendor/` 目录 → 照常工作（vendor 可选）
//! 5. 缺 `plugin/__init__.py` → 退出码 3 + 指明入口缺失
//!
//! 这些用例直接跑启动器（不经宿主），确保「装配规则」与「宿主启动路径」解耦可验。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn launcher_dir() -> PathBuf {
    manifest_dir().join("../../tools")
}

fn good_pack() -> PathBuf {
    launcher_dir().join("pack-conformance/fixtures/good")
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run_launcher(pack: &Path, extra: &[&str], envs: &[(&str, String)]) -> Run {
    let mut cmd = Command::new("python3");
    cmd.arg("-m")
        .arg("dampack_launcher")
        .arg(pack)
        .args(extra)
        .env("PYTHONPATH", launcher_dir())
        .env_remove("DAMPACK_SHARED_SITE_DIR");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd
        .output()
        .expect("拉起 python3 失败（测试环境需有 python3）");
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    }
}

fn write_pack(root: &Path, id: &str, plugin_body: &str) -> PathBuf {
    let pack = root.join(id);
    fs::create_dir_all(pack.join("plugin")).unwrap();
    fs::write(pack.join("plugin/__init__.py"), plugin_body).unwrap();
    fs::write(
        pack.join("manifest.json"),
        format!(
            r#"{{
  "id": "{id}",
  "name": "{id}",
  "version": "1.0.0",
  "api_version": "1.1",
  "entrypoint": {{"cmd": "python3", "args": ["plugin/__init__.py"]}},
  "capabilities": ["io.read"],
  "deterministic": true,
  "timeout_secs": 5
}}"#
        ),
    )
    .unwrap();
    pack
}

fn probe_json(run: &Run) -> serde_json::Value {
    assert_eq!(run.code, 0, "启动器应退出 0，stderr={}", run.stderr);
    serde_json::from_str(run.stdout.trim())
        .unwrap_or_else(|e| panic!("probe stdout 不是 JSON（{e}）：{:?}", run.stdout))
}

// 1. 正常加载
#[test]
fn l01_probe_loads_pack_body_first() {
    let run = run_launcher(&good_pack(), &["--probe", "plugin"], &[]);
    let v = probe_json(&run);
    assert_eq!(v["pack_id"], "conformance.good");
    let file = v["file"].as_str().unwrap();
    assert!(file.ends_with("plugin/__init__.py"), "{file}");
    let head: Vec<String> = v["sys_path_head"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        head[0].trim_end_matches('/').ends_with("fixtures/good"),
        "包根必须排在 sys.path 最前（`import plugin` 才能解析）：{head:?}"
    );
    assert!(
        head.iter()
            .any(|p| p.trim_end_matches('/').ends_with("/plugin")),
        "plugin/ 也要在 sys.path 里（脚本式包的平铺模块）：{head:?}"
    );
}

// 2. import 抛异常
#[test]
fn l02_import_error_reports_readable_reason_on_stderr_only() {
    let tmp = tempfile::tempdir().unwrap();
    let pack = write_pack(
        tmp.path(),
        "test.import-error",
        "raise ImportError(\"No module named 'onnxruntime'\")\n",
    );
    let run = run_launcher(&pack, &[], &[]);
    assert_eq!(run.code, 3, "加载失败必须是退出码 3，stderr={}", run.stderr);
    assert!(
        run.stdout.trim().is_empty(),
        "失败时 stdout 必须为空（协议流纯净）：{:?}",
        run.stdout
    );
    assert!(run.stderr.contains("import 失败"), "{}", run.stderr);
    assert!(run.stderr.contains("onnxruntime"), "{}", run.stderr);
}

// 3. vendor 覆盖 shared-site
#[test]
fn l03_vendor_wins_over_shared_site() {
    let tmp = tempfile::tempdir().unwrap();
    let pack = write_pack(
        tmp.path(),
        "test.vendor",
        "PACK_INFO = {\"id\": \"test.vendor\"}\n",
    );
    // 包内私有依赖
    fs::create_dir_all(pack.join("vendor")).unwrap();
    fs::write(pack.join("vendor/dupmod.py"), "VALUE = \"vendor\"\n").unwrap();
    // 宿主 shared-site（仅开发档存在）
    let shared = tmp.path().join("shared-site");
    fs::create_dir_all(&shared).unwrap();
    fs::write(shared.join("dupmod.py"), "VALUE = \"shared\"\n").unwrap();

    let run = run_launcher(
        &pack,
        &["--probe", "dupmod"],
        &[("DAMPACK_SHARED_SITE_DIR", shared.display().to_string())],
    );
    let v = probe_json(&run);
    assert_eq!(
        v["value"], "vendor",
        "包内 vendor 必须优先于 shared-site：{v}"
    );
    assert!(v["file"].as_str().unwrap().contains("/vendor/"), "{v}");
}

// 4. 缺 vendor 目录照常工作
#[test]
fn l04_missing_vendor_dir_is_fine() {
    let tmp = tempfile::tempdir().unwrap();
    let pack = write_pack(
        tmp.path(),
        "test.no-vendor",
        "PACK_INFO = {\"id\": \"test.no-vendor\"}\nVALUE = \"ok\"\n",
    );
    assert!(!pack.join("vendor").exists());
    let run = run_launcher(&pack, &["--probe", "plugin"], &[]);
    let v = probe_json(&run);
    assert_eq!(v["value"], "ok");
    let head: Vec<String> = v["sys_path_head"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        !head
            .iter()
            .any(|p| p.trim_end_matches('/').ends_with("/vendor")),
        "没有 vendor 目录就不该出现在 sys.path：{head:?}"
    );
}

// 5. 缺 plugin/__init__.py
#[test]
fn l05_missing_plugin_init_is_load_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let pack = tmp.path().join("test.missing-init");
    fs::create_dir_all(pack.join("plugin")).unwrap();
    // 目录在，但没有 __init__.py（打包漏文件 / 解压不完整）
    fs::write(pack.join("plugin/impl.py"), "VALUE = 1\n").unwrap();
    fs::write(
        pack.join("manifest.json"),
        r#"{
  "id": "test.missing-init",
  "name": "缺入口",
  "version": "1.0.0",
  "api_version": "1.1",
  "entrypoint": {"cmd": "python3", "args": ["plugin/__init__.py"]},
  "capabilities": ["io.read"],
  "deterministic": true,
  "timeout_secs": 5
}"#,
    )
    .unwrap();

    let run = run_launcher(&pack, &[], &[]);
    assert_eq!(run.code, 3, "stderr={}", run.stderr);
    assert!(run.stdout.trim().is_empty(), "{:?}", run.stdout);
    assert!(run.stderr.contains("插件入口缺失"), "{}", run.stderr);
    assert!(run.stderr.contains("__init__.py"), "{}", run.stderr);
}
