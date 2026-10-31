//! **故障注入**：崩溃 / 超时 必须落 `undetermined` + `note`，绝不冒充 pass/fail。
//!
//! 这是「不可判定绝不冒充」这条铁律的可执行证据（协议 §2 / §2.1 / §7 铁律 1）。
//! 覆盖：
//! 1. 调用中途 `kill -9` → 宿主自动重启 + **重试成功**（用户不该看到一个假失败）；
//! 2. 每次重试都崩 → 重启耗尽 → 冷却 + `plugin_crash`（且冷却期不再拉起）；
//! 3. 调用超时 → cancel 宽限 → `plugin_timeout`，进程被杀、下一次仍能正常拉起；
//! 4. 插件**业务错误**（-32002）不得被翻译成 `undetermined`（否则「资产缺失」会
//!    冒充「插件不可用」，用户看到的是「判不了」而不是真实故障）。
//!
//! 每个用例都用真子进程；`undetermined_verdict` 的断言直接对齐 Verdict 七字段。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pack_host::process::undetermined_verdict;
use pack_host::{PackHost, PackHostConfig, PackHostError, Unavailable};
use serde_json::{json, Value};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repo_root() -> PathBuf {
    manifest_dir().join("../..")
}

fn fixture(name: &str) -> PathBuf {
    manifest_dir().join("tests/fixtures").join(name)
}

fn cfg(work: &Path) -> PackHostConfig {
    let mut c = PackHostConfig::new("http://127.0.0.1:1", "fault-token", work);
    c.launcher_dir = Some(repo_root().join("tools"));
    c.health_timeout = Duration::from_secs(5);
    c.cancel_grace = Duration::from_millis(150);
    c
}

fn workdir(tag: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join(format!("work-{tag}"));
    fs::create_dir_all(&work).unwrap();
    (dir, work)
}

/// Verdict 七字段 + 「不冒充」不变量。
fn assert_undetermined(verdict: &Value, note: &str) {
    for key in [
        "ruleId", "status", "score", "label", "evidence", "fixed", "note",
    ] {
        assert!(
            verdict.get(key).is_some(),
            "Verdict 缺字段 {key}：{verdict}"
        );
    }
    assert_eq!(verdict["status"], "undetermined", "{verdict}");
    assert_ne!(verdict["status"], "pass");
    assert_ne!(verdict["status"], "fail");
    assert!(verdict["score"].is_null(), "不可判定不得带分数：{verdict}");
    assert_eq!(verdict["fixed"], false, "不可判定不得声明已修复：{verdict}");
    assert_eq!(verdict["note"], note, "{verdict}");
    assert_eq!(verdict["evidence"]["reason"], note, "{verdict}");
    assert!(
        !verdict["evidence"]["detail"]
            .as_str()
            .unwrap_or("")
            .is_empty(),
        "evidence.detail 必须带可读原因：{verdict}"
    );
}

// ---------------------------------------------------------------------------
// 1. 调用中途 kill -9 → 重启 + 重试成功（不是假失败）
// ---------------------------------------------------------------------------

#[test]
fn kill9_during_inflight_call_restarts_and_retries() {
    let (_tmp, work) = workdir("kill9");
    let host = Arc::new(PackHost::open(fixture("frame-recorder"), cfg(&work)).unwrap());
    assert_eq!(host.call("health", json!({})).unwrap()["status"], "ok");
    let first_pid = host.pid().expect("握手后应有子进程");
    assert_eq!(host.spawn_count(), 1);

    // slow 会睡 400ms；在它应答之前把子进程 kill -9 掉
    let caller = {
        let host = host.clone();
        std::thread::spawn(move || {
            host.call_with_timeout(
                "slow",
                json!({ "job_id": "job-kill" }),
                Duration::from_secs(3),
            )
        })
    };
    std::thread::sleep(Duration::from_millis(120));
    host.simulate_crash();

    let result = caller.join().expect("调用线程 panic");
    let out = result.expect("崩溃后自动重启并重试，调用必须成功（否则用户会看到一个假失败）");
    assert_eq!(out["slept"], 0.4, "{out}");

    let second_pid = host.pid().expect("重试后应有新子进程");
    assert_ne!(first_pid, second_pid, "崩溃后应换进程");
    assert_eq!(host.spawn_count(), 2, "崩溃一次 → 只该多拉起一次");
    // 重试用的仍是同一个包（身份不变，结果行才能对得上账）
    assert_eq!(host.identity().id, "test.frame-recorder");

    // 帧面证据：同一个 job_id 被重发了一次（第二次是重试用）
    let raw = fs::read_to_string(work.join("frames.log")).unwrap();
    let resends = raw
        .lines()
        .filter(|l| l.contains("\"method\": \"slow\"") || l.contains("\"method\":\"slow\""))
        .count();
    assert_eq!(resends, 2, "崩溃重试应重发 slow 请求：{raw}");
    host.shutdown();
}

// ---------------------------------------------------------------------------
// 2. 每次重试都崩 → 冷却 + plugin_crash
// ---------------------------------------------------------------------------

#[test]
fn repeated_crash_exhausts_restarts_and_reports_plugin_crash() {
    let (_tmp, work) = workdir("crash");
    let mut c = cfg(&work);
    c.max_restarts = 2;
    c.cooldown = Duration::from_secs(30);
    let host = PackHost::open(fixture("crash-after-health"), c).unwrap();

    let err = host
        .call("judge", json!({ "job_id": "j-crash" }))
        .unwrap_err();
    match &err {
        PackHostError::Unavailable { kind, .. } => {
            assert_eq!(*kind, Unavailable::Crash, "{err}");
        }
        other => panic!("期望 Unavailable::Crash，实得 {other:?}"),
    }
    assert_eq!(err.note(), Some("plugin_crash"));
    assert!(err.is_undetermined(), "崩溃必须算「不可判定」");
    // 首次 + 2 次重启 = 3 次拉起
    assert_eq!(host.spawn_count(), 3, "max_restarts=2 应共拉起 3 次");

    let verdict = undetermined_verdict("A4_subject_ratio", &err);
    assert_undetermined(&verdict, "plugin_crash");

    // 冷却期：不再拉起新进程，错误类型仍是崩溃（带剩余秒数）
    let again = host.call("judge", json!({})).unwrap_err();
    assert_eq!(again.note(), Some("plugin_crash"), "{again}");
    assert!(again.to_string().contains("冷却中"), "{again}");
    assert_eq!(host.spawn_count(), 3, "冷却期内不得再拉起进程");
    assert!(host.pid().is_none(), "冷却期不应有活着的子进程");
}

// ---------------------------------------------------------------------------
// 3. 超时 → cancel → plugin_timeout；进程被杀但可再拉起
// ---------------------------------------------------------------------------

#[test]
fn timeout_reports_plugin_timeout_and_recovers_on_next_call() {
    let (_tmp, work) = workdir("timeout");
    let mut c = cfg(&work);
    c.cancel_grace = Duration::from_millis(150);
    let host = PackHost::open(fixture("frame-recorder"), c).unwrap();
    host.call("health", json!({})).unwrap();

    let started = Instant::now();
    let err = host
        .call_with_timeout(
            "slow",
            json!({ "job_id": "j-timeout" }),
            Duration::from_millis(200),
        )
        .unwrap_err();
    let elapsed = started.elapsed();
    match &err {
        PackHostError::Unavailable { kind, .. } => assert_eq!(*kind, Unavailable::Timeout, "{err}"),
        other => panic!("期望 Unavailable::Timeout，实得 {other:?}"),
    }
    assert_eq!(err.note(), Some("plugin_timeout"));
    assert!(err.is_undetermined());
    // 预算 200ms + 宽限 150ms 之后才判超时；但不得拖到插件自己回话（400ms）之后
    assert!(
        elapsed >= Duration::from_millis(200),
        "没等够预算：{elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(900),
        "判超时拖太久（宽限失效或阻塞在插件应答上）：{elapsed:?}"
    );
    let verdict = undetermined_verdict("A5_overlay_text", &err);
    assert_undetermined(&verdict, "plugin_timeout");

    // N19：超时必须**结构化**（不是只有一句中文）——预算/实耗/建议档/最后一段包内日志
    let info = err.timeout_info().expect("超时错误必须带 TimeoutInfo");
    assert_eq!(info.budget_ms, 200, "{info:?}");
    assert!(info.elapsed_ms >= 200, "实耗应 ≥ 预算：{info:?}");
    assert_eq!(
        info.suggested_tier, "S(30s)",
        "200ms 预算的下一档就是 S(30s)：{info:?}"
    );
    assert!(
        info.last_stage
            .as_deref()
            .unwrap_or_default()
            .contains("stage slow-sleep"),
        "超时文案必须摘到最后一段包内日志（真机排障靠它）：{info:?}"
    );
    assert_eq!(err.error_code(), "timeout");
    // 宿主造的失败行与包自己回的失败行同一形状（下游 parse_row 不分两条路）
    let row = pack_host::result::failure_row("asset-1", &err);
    assert_eq!(row["assetId"], "asset-1");
    assert_eq!(row["result"], "failed");
    assert_eq!(row["error"]["code"], "timeout");
    assert_eq!(row["error"]["detail"]["budgetMs"], 200);
    assert_eq!(row["error"]["detail"]["code"], "timeout");
    assert_eq!(row["error"]["detail"]["suggestedTier"], "S(30s)");
    assert!(
        row["error"]["detail"]["lastStage"].as_str().is_some(),
        "{row}"
    );
    assert_eq!(row["error"]["retryable"], true);

    // 插件没在宽限内收尾 → 被清理；下一次调用可正常拉起（不是永久不可用）
    assert!(host.pid().is_none(), "宽限超时后应清理子进程");
    assert_eq!(
        host.call("health", json!({})).unwrap()["status"],
        "ok",
        "超时后应能重新拉起插件"
    );
    assert_eq!(host.spawn_count(), 2);
}

// ---------------------------------------------------------------------------
// 4. 业务错误不得伪装成「不可判定」
// ---------------------------------------------------------------------------

#[test]
fn plugin_business_error_is_not_masked_as_undetermined() {
    let (_tmp, work) = workdir("biz");
    let host = PackHost::open(fixture("frame-recorder"), cfg(&work)).unwrap();
    host.call("health", json!({})).unwrap();

    let err = host.call("rpc_error", json!({})).unwrap_err();
    assert!(
        matches!(err, PackHostError::Rpc { code: -32002, .. }),
        "{err:?}"
    );
    assert!(
        !err.is_undetermined(),
        "业务错误（-32002 资产不可读）必须由调用方按真错误处理，不能算不可判定"
    );
    assert_eq!(err.note(), None, "业务错误没有 note（note 只属于不可判定）");
    // 业务错误之后插件仍在（不需要重启）
    assert_eq!(host.spawn_count(), 1);
    assert_eq!(host.call("health", json!({})).unwrap()["status"], "ok");
}

// ---------------------------------------------------------------------------
// 5. 加载失败（缺依赖/import 抛异常）→ plugin_load_failed，且原因可读
// ---------------------------------------------------------------------------

#[test]
fn load_failure_reports_readable_reason() {
    let (_tmp, work) = workdir("loadfail");
    let mut c = cfg(&work);
    c.max_restarts = 0;
    let host = PackHost::open(fixture("load-fail"), c).unwrap();

    let err = host.call("health", json!({})).unwrap_err();
    match &err {
        PackHostError::Unavailable { kind, stderr, .. } => {
            assert_eq!(*kind, Unavailable::LoadFailed, "{err}");
            assert!(
                !stderr.is_empty(),
                "加载失败必须带插件侧 stderr 尾部：{err}"
            );
        }
        other => panic!("期望 Unavailable::LoadFailed，实得 {other:?}"),
    }
    assert_eq!(err.note(), Some("plugin_load_failed"));
    let verdict = undetermined_verdict("A2_min_short_side", &err);
    assert_undetermined(&verdict, "plugin_load_failed");
    // 用户能看懂的那句话要在错误里（不是裸 traceback 或者空串）
    assert!(
        err.to_string().contains("插件不可用"),
        "错误文案应说明「插件不可用」：{err}"
    );
}
