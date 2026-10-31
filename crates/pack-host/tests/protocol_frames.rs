//! **宿主侧协议帧编码单测**（协议 §1 / §2 / §3 的「线上字节」层）。
//!
//! 契约套件（`tools/pack-conformance/run.py`）验的是插件行为；这里反过来验**宿主发出
//! 与解读的字节**：一行一帧、`jsonrpc:"2.0"`、id 单调递增且不回绕、cancel 是无 id 通知
//! 且带 `params.job_id`、params 原样透传、id 不匹配的帧不能被当成本次调用的结果、
//! 「既无 result 也无 error」必须报协议违例、业务错误码原样上抛。
//!
//! 取证方式：夹具包（`tests/fixtures/frame-recorder`）把收到的每一行原始字节落到
//! `$DAMPACK_WORK_DIR/frames.log`，断言直接读这个文件 —— 不是「看日志猜帧格式」。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pack_host::{PackHost, PackHostConfig, PackHostError, Unavailable};
use serde_json::{json, Value};

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
    let mut c = PackHostConfig::new("http://127.0.0.1:1", "frames-token", work);
    c.launcher_dir = Some(repo_root().join("tools"));
    c.health_timeout = Duration::from_secs(5);
    c.cancel_grace = Duration::from_millis(200);
    c
}

/// 读夹具落盘的原始帧行（去掉行尾换行；保留空行判断权交给断言）。
fn frames(work: &Path) -> Vec<String> {
    let path = work.join("frames.log");
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("夹具没有落盘帧记录 {path:?}：{e}"));
    raw.split_inclusive('\n').map(|l| l.to_string()).collect()
}

/// 只取「宿主 → 插件」的原始帧（夹具把无 id 通知另记成 `# notification <method>` 注释行，
/// 那是**插件侧解读**的证据，这里要的是线上字节，所以注释行滤掉）。
fn request_frames(work: &Path) -> Vec<(String, Value)> {
    frames(work)
        .into_iter()
        .filter(|l| !l.starts_with("# notification"))
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            assert!(
                line.ends_with('\n'),
                "每一帧必须以换行结束（宿主写帧不能漏换行）：{line:?}"
            );
            assert_eq!(
                line.matches('\n').count(),
                1,
                "一帧必须恰好一行（多行 JSON 会让插件按下标错位解析）：{line:?}"
            );
            let value: Value = serde_json::from_str(line.trim_end()).unwrap_or_else(|e| {
                panic!("宿主发出的不是单行合法 JSON：{line:?}（{e}）")
            });
            assert!(
                value.is_object(),
                "帧必须是 JSON 对象（协议 §1 行式 JSON-RPC）：{line:?}"
            );
            assert_eq!(
                value["jsonrpc"], "2.0",
                "帧必须带 jsonrpc=\"2.0\"：{line:?}"
            );
            assert!(
                value.get("method").and_then(Value::as_str).is_some(),
                "宿主 → 插件的帧必须带 method：{line:?}"
            );
            (line, value)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 1. 帧格式与 id 单调
// ---------------------------------------------------------------------------

#[test]
fn host_encodes_one_line_frames_with_monotonic_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let host = PackHost::open(recorder_pack(), cfg(&work)).unwrap();

    // 三次调用：health（握手时由宿主自己发）+ 两次 echo
    let info = host.call("pack.info", json!({})).unwrap();
    assert_eq!(info["id"], "test.frame-recorder");
    let echo = host
        .call("echo", json!({ "job_id": "job-1", "nested": { "k": [1, 2, 3] } }))
        .unwrap();
    assert_eq!(echo["echo"]["job_id"], "job-1");
    assert_eq!(
        echo["echo"]["nested"]["k"],
        json!([1, 2, 3]),
        "params 必须原样透传（宿主不得重写/裁剪插件参数）"
    );

    let reqs = request_frames(&work);
    assert_eq!(reqs.len(), 3, "应恰好发出 health + pack.info + echo 三帧");
    assert_eq!(reqs[0].1["method"], "health", "首帧必须是握手");
    assert_eq!(reqs[1].1["method"], "pack.info");
    assert_eq!(reqs[2].1["method"], "echo");

    // id 从 1 开始逐帧 +1（宿主侧 AtomicU64），不得重复/回绕
    let ids: Vec<u64> = reqs
        .iter()
        .map(|(line, v)| {
            v["id"]
                .as_u64()
                .unwrap_or_else(|| panic!("请求帧必须带整数 id：{line:?}"))
        })
        .collect();
    assert_eq!(ids, vec![1, 2, 3], "id 必须逐帧递增：{ids:?}");

    // 每帧 params 必须是对象（协议 §1：params 省略即视为 {}）
    for (line, v) in &reqs {
        assert!(
            v.get("params").map(Value::is_object).unwrap_or(true),
            "params 必须是对象：{line:?}"
        );
    }
    // 请求帧不得带「result/error」（那是应答的形状）
    for (line, v) in &reqs {
        assert!(v.get("result").is_none() && v.get("error").is_none(), "{line:?}");
    }

    host.shutdown();
}

#[test]
fn host_survives_plugin_stdout_pollution_and_captures_notifications() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let host = PackHost::open(recorder_pack(), cfg(&work)).unwrap();

    // 握手必须成功（夹具在 serve() 里先写了 1 行非 JSON + 1 条无 id 通知）
    assert_eq!(host.call("health", json!({})).unwrap()["status"], "ok");

    let stray = host.stray_stdout();
    assert!(
        stray.iter().any(|l| l.contains("boot: this line is not a frame")),
        "非协议 stdout 行必须被记录下来（协议 §7 铁律 4 的证据面）：{stray:?}"
    );

    let notes = host.notifications();
    assert!(
        notes.iter().any(|n| n["method"] == "log"),
        "无 id 通知必须进 notifications()，不能被当应答吞掉：{notes:?}"
    );
    host.shutdown();
}

// ---------------------------------------------------------------------------
// 2. cancel：无 id 通知 + job_id 透传
// ---------------------------------------------------------------------------

#[test]
fn timeout_sends_cancel_notification_without_id_and_keeps_job_id() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let mut c = cfg(&work);
    c.cancel_grace = Duration::from_millis(150);
    let host = PackHost::open(recorder_pack(), c).unwrap();
    assert_eq!(host.call("health", json!({})).unwrap()["status"], "ok");

    // slow 睡 400ms > 预算 150ms → 宿主发 cancel 通知、宽限 150ms 内无应答 → 杀进程
    let started = Instant::now();
    let err = host
        .call_with_timeout("slow", json!({ "job_id": "job-slow-7" }), Duration::from_millis(150))
        .unwrap_err();
    let elapsed = started.elapsed();
    match &err {
        PackHostError::Unavailable { kind, .. } => assert_eq!(*kind, Unavailable::Timeout, "{err}"),
        other => panic!("超时必须落 Unavailable::Timeout，实得 {other:?}"),
    }
    assert_eq!(err.note(), Some("plugin_timeout"));
    assert!(elapsed >= Duration::from_millis(150), "没有等够预算就报超时：{elapsed:?}");

    // 帧面证据：slow 请求之后必须紧跟一条 **无 id** 的 cancel 通知，且 job_id 原样透传
    let all = frames(&work);
    let reqs = request_frames(&work);
    let methods: Vec<&str> = reqs
        .iter()
        .map(|(_, v)| v["method"].as_str().unwrap())
        .collect();
    // 握手 health（id=1）+ 显式 health（id=2）+ slow（id=3）+ cancel（无 id）
    assert_eq!(
        methods,
        vec!["health", "health", "slow", "cancel"],
        "超时后必须发一条 cancel 通知帧：{all:?}"
    );

    // cancel 帧本体：无 id（否则会与真实请求串号） + params.job_id 透传
    let cancel = &reqs[3].1;
    assert!(
        cancel.get("id").is_none(),
        "cancel 必须是**无 id 通知**（协议 §2.1）：{cancel}"
    );
    assert_eq!(cancel["params"]["job_id"], "job-slow-7", "取消必须按 job 归账：{cancel}");
    // 插件侧确认：夹具把它当通知处理（没回帧），这正是「无 id」的行为证据
    assert!(
        all.iter().any(|l| l.starts_with("# notification cancel")),
        "插件没有把 cancel 识别为通知：{all:?}"
    );
    assert_eq!(
        all.iter()
            .filter(|l| l.starts_with("# notification"))
            .count(),
        1,
        "一次超时只应发一条 cancel：{all:?}"
    );
    let slow = &reqs[2].1;
    assert_eq!(
        slow["params"]["job_id"], "job-slow-7",
        "job_id 必须原样进 params（取消按 job 归账）"
    );
}

// ---------------------------------------------------------------------------
// 3. 应答解读的边界：id 不匹配 / 既无 result 也无 error / 业务错误码
// ---------------------------------------------------------------------------

#[test]
fn reply_with_foreign_id_is_not_taken_as_result() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let host = PackHost::open(recorder_pack(), cfg(&work)).unwrap();
    host.call("health", json!({})).unwrap();

    // 夹具用 mid+1000 应答：宿主必须不认，最终按超时处理（不得把别人的结果当自己的）
    let started = Instant::now();
    let err = host
        .call_with_timeout("ghost", json!({}), Duration::from_millis(300))
        .unwrap_err();
    assert!(
        matches!(&err, PackHostError::Unavailable { kind: Unavailable::Timeout, .. }),
        "id 不匹配的应答必须被忽略并落超时，实得 {err:?}"
    );
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert_eq!(err.note(), Some("plugin_timeout"));
}

#[test]
fn frame_without_result_or_error_is_protocol_violation() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let host = PackHost::open(recorder_pack(), cfg(&work)).unwrap();
    host.call("health", json!({})).unwrap();

    let err = host.call("naked", json!({})).unwrap_err();
    match &err {
        PackHostError::Protocol(msg) => {
            assert!(
                msg.contains("既无 result 也无 error"),
                "协议违例的说明必须点出缺的是什么：{msg}"
            );
        }
        other => panic!("期望 Protocol 违例，实得 {other:?}"),
    }
    // 协议违例不是「不可判定」：note 为空，调用方要按真错误处理
    assert_eq!(err.note(), None);
    assert!(!err.is_undetermined());
}

#[test]
fn plugin_error_codes_are_passed_through_verbatim() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let host = PackHost::open(recorder_pack(), cfg(&work)).unwrap();
    host.call("health", json!({})).unwrap();

    let err = host.call("rpc_error", json!({})).unwrap_err();
    match &err {
        PackHostError::Rpc { code, message } => {
            assert_eq!(*code, -32002, "业务错误码必须原样上抛：{err}");
            assert!(message.contains("资产不可读"), "{message}");
        }
        other => panic!("期望 Rpc 错误，实得 {other:?}"),
    }
    // 业务错误 ≠ 不可判定：不得被翻译成 undetermined（否则「资产缺失」会冒充「模型不可用」）
    assert_eq!(err.note(), None);
    assert!(!err.is_undetermined());

    // 未实现的方法：插件回 -32601，同样原样上抛
    let err = host.call("no.such.method", json!({})).unwrap_err();
    match &err {
        PackHostError::Rpc { code, .. } => assert_eq!(*code, -32601, "{err}"),
        other => panic!("期望 -32601，实得 {other:?}"),
    }
}
