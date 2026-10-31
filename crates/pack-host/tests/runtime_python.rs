//! KI-28 回归钉：内置 python runtime 的解释器选择与缺失可诊断（卡 `task`）。
//!
//! 背景：出包 K14 实测安装包里 `grep -i python` 命中 0 条，而 `PackHostConfig.runtime_python`
//! 从没被 app-web/app-tauri 接线 → 纯净 Windows（无 `python3`）上插件子进程根本起不来。
//! 本文件钉三件事（与卡面「要做 4」逐条对应）：
//!
//! ① `runtime_python=Some(<不存在>)` → 失败信息必须**含该路径**且点明「内置 python 运行时缺失」
//!    （裸 spawn 只会给 `io error: No such file or directory`，用户看不出病灶）；
//! ② `Some(<假 python 脚本>)` → 宿主**真的**用这个解释器起子进程（假脚本记录 argv 后 exec 真 python3，
//!    握手照常成功）；接线被摘掉时这条会红（反向验证见报告）；
//! ③ `None` → 回落裸 `python3`（既有行为不回退）；
//! ④ 优先级：包内 venv > 内置 runtime > `python3`（2026-10-20 运行时裁决）。
//!
//! 只跑代码 + 单测，不上真机、不装 pip（假解释器不装任何东西）。
use std::path::{Path, PathBuf};

use pack_host::process::resolve_interpreter;
use pack_host::{PackHost, PackHostConfig};

/// 参考包（真 manifest + 真 plugin/__init__.py，`health`/`pack.info` 都能应答）。
const GOOD_PACK: &str = "tools/pack-conformance/fixtures/good";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

fn fixture_pack() -> PathBuf {
    repo_root().join(GOOD_PACK)
}

/// 独立临时目录（不共享，避免并行用例互相踩）。
fn work(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dam-pack-runtime-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

/// 最小合法配置：不起网关（`health` / `pack.info` 不需要网关）。
fn cfg(work: &Path, runtime_python: Option<PathBuf>) -> PackHostConfig {
    let mut c = PackHostConfig::new("http://127.0.0.1:1", "runtime-token", work);
    // 启动器目录 = 仓库 tools/（发行形态由 <resource_dir>/pack-launcher 替代，见 app-web）
    c.launcher_dir = Some(repo_root().join("tools"));
    c.runtime_python = runtime_python;
    // 重试/冷却压到最小：这里验的是「失败信息」，不是退避曲线
    c.max_restarts = 0;
    c.health_timeout = std::time::Duration::from_secs(10);
    c
}

// ---------------------------------------------------------------- ④ 优先级

#[test]
fn interpreter_priority_venv_then_runtime_then_python3() {
    // ③ None → 裸 python3（命令名，不解析成绝对路径）
    assert_eq!(
        resolve_interpreter(Path::new("/nonexistent-pack"), None),
        PathBuf::from("python3")
    );
    // ② Some → 内置 runtime
    let rt = PathBuf::from("/bundle/runtime/python/python.exe");
    assert_eq!(
        resolve_interpreter(Path::new("/nonexistent-pack"), Some(&rt)),
        rt
    );

    // ① 包内 venv 优先于内置 runtime（venv 是每包依赖隔离边界）
    let pack = work("venv");
    #[cfg(windows)]
    let venv_rel = "venv/Scripts/python.exe";
    #[cfg(not(windows))]
    let venv_rel = "venv/bin/python3";
    let venv_py = pack.join(venv_rel);
    std::fs::create_dir_all(venv_py.parent().unwrap()).unwrap();
    std::fs::write(&venv_py, b"#!/bin/sh\n").unwrap();
    assert_eq!(
        resolve_interpreter(&pack, Some(&rt)),
        venv_py,
        "包内 venv 必须优先于内置 runtime"
    );
}

// -------------------------------------------------- ① 缺失时给出可诊断信息

#[test]
fn missing_bundled_runtime_is_diagnosable() {
    let w = work("missing");
    // 绝对路径 + 文件不存在 = 「安装包里没有运行库」，必须当场报清路径
    let missing = PathBuf::from("/opt/definitely-not-there/runtime/python/python.exe");
    let host = PackHost::open(fixture_pack(), cfg(&w, Some(missing.clone()))).expect("开包");
    let err = host
        .call("pack.info", serde_json::json!({}))
        .expect_err("runtime 缺失时必须失败，不能静默回落 python3");
    let msg = err.to_string();
    assert!(
        msg.contains("内置 python 运行时缺失"),
        "错误信息要说明是「运行时缺失」而不是泛化崩溃：{msg}"
    );
    assert!(
        msg.contains(&missing.display().to_string()),
        "错误信息必须含缺失的路径（支持据此判断重装还是改配置）：{msg}"
    );
    // 分类仍是「不可判定」通道里的加载失败，而不是冒充 pass/fail
    assert_eq!(err.note(), Some("plugin_load_failed"), "{msg}");
}

// ----------------------------------- ② 假解释器：证明确实选中了内置 runtime

#[cfg(unix)]
#[test]
fn fake_bundled_runtime_is_actually_spawned() {
    use std::os::unix::fs::PermissionsExt;

    let w = work("fake");
    let rec = w.join("interpreter.argv");
    let fake = w.join("python3-bundled");
    // 记录 $0 与全部 argv 后 exec 真 python3：握手/包交互仍是真的，只有「谁被选中」是被钉住的
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$0\" \"$@\" > '{}'\nexec python3 \"$@\"\n",
        rec.display()
    );
    std::fs::write(&fake, script).unwrap();
    let mut perm = std::fs::metadata(&fake).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&fake, perm).unwrap();

    let host = PackHost::open(fixture_pack(), cfg(&w, Some(fake.clone()))).expect("开包");
    let info = host
        .call("pack.info", serde_json::json!({}))
        .expect("用内置 runtime 起的子进程必须能应答 pack.info");
    assert_eq!(
        info.get("id").and_then(|v| v.as_str()),
        Some("conformance.good"),
        "pack.info 返回的包身份不对：{info}"
    );

    let recorded = std::fs::read_to_string(&rec).expect("假解释器没被拉起（接线断了？）");
    let lines: Vec<&str> = recorded.lines().collect();
    let expected0 = fake.to_string_lossy().to_string();
    assert_eq!(
        lines.first().copied(),
        Some(expected0.as_str()),
        "被拉起的解释器不是内置 runtime：{recorded}"
    );
    assert!(
        lines.iter().any(|l| *l == "-m") && lines.iter().any(|l| *l == "dampack_launcher"),
        "启动器装配参数不对：{recorded}"
    );

    // 反向证据：把 runtime_python 摘掉后宿主该走裸 python3（假脚本不再被调用）
    std::fs::remove_file(&rec).unwrap();
    let host2 = PackHost::open(fixture_pack(), cfg(&w, None)).expect("开包");
    host2
        .call("pack.info", serde_json::json!({}))
        .expect("回落 python3 必须仍然可用（既有行为不回退）");
    assert!(
        !rec.exists(),
        "None 时不该再走内置 runtime，但假解释器又被拉起了：{}",
        rec.display()
    );
}
