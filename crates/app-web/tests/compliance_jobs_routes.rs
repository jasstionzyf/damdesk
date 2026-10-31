#![allow(clippy::await_holding_lock)] // vips 全局锁刻意贯穿 await：测试间串行化 libvips（非 DB 锁，无死锁面）
//! CMP-JOB-01（卡 task）HTTP 面回归钉：**一次批量审核 = 一个独立 job**。
//!
//! 用户裁决：跑完一次要能给这次出结果统计，各次之间明显分开；且点开某次执行要能看懂
//! 「这次在干什么」（步骤名是人话，不是 `job313`/`group47`）。
//!
//! 钉的四件事：
//! 1. `POST /api/compliance/judge` 返回 `runId`，本次任务的统计只算本次；
//! 2. `GET /api/compliance/jobs` 按时间倒序列出**多次执行**，两次执行互不叠加；
//! 3. `GET /api/compliance/jobs/{runId}` 的 task 明细步骤名是人话（含规则名）+ 不存在 → 404；
//! 4. `GET /api/compliance/judge/status` 保持旧字段兼容但显式标 `legacyCumulative: true`，
//!    并新增 `queueCurrent`（本 run 口径）/`run`（本 run 结果统计）。
use axum::body::Body;
use axum::http::Request;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tower::ServiceExt;

/// libvips 是进程级全局状态：**先用 `core_media::init_backend()` 显式初始化一次**，再串行化
/// 用 vips 的用例。只加 Mutex 不够——未 init 时首个 vips 调用会隐式初始化，多线程/多测试
/// 同时首次调用即在 libvips 内部并发注册 GObject class（GLib `assertion 'parent_type > 0' failed`
/// → `BlackError` / `PngsaveError`，严重时 SIGABRT）。
fn vips_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| {
        core_media::init_backend().expect("libvips 初始化失败");
        Mutex::new(())
    })
}

fn make_png(path: &Path, brighten: f64) {
    let black = libvips::ops::black(64, 64).unwrap();
    let img = libvips::ops::linear(&black, &mut [1.0], &mut [brighten]).unwrap();
    libvips::ops::pngsave(&img, &path.to_string_lossy()).unwrap();
}

/// 两张目录 × 每目录 2 张（组内字节相同 → 组级重复可判）= 4 张资产 / 2 个组
fn fixture_images(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for (gi, dir) in ["propA", "propB"].iter().enumerate() {
        let d = root.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        for k in 0..2 {
            let p = d.join(format!("img{k}.png"));
            make_png(&p, 40.0 + (gi * 2 + k) as f64 * 30.0);
            out.push(p);
        }
    }
    out
}

struct Fixture {
    app: axum::Router,
    db: PathBuf,
    #[allow(dead_code)]
    data: PathBuf,
}

fn setup(tag: &str) -> Fixture {
    let data = std::env::temp_dir().join(format!("cmpjob-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let db = data.join("dam.db");
    let conn = core_storage::open(&db).unwrap();
    for (i, p) in fixture_images(&data.join("src")).iter().enumerate() {
        let id = (i + 1) as i64;
        conn.execute(
            "INSERT INTO assets (assetId, absPath, mediaType, missing, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (?1, ?2, 'image', 0, ?3, 'png', 10, 0, ?1)",
            rusqlite::params![
                id,
                p.to_string_lossy().to_string(),
                p.file_name().unwrap().to_string_lossy().to_string()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1, 'primary', 0, 64, 64)",
            [id],
        )
        .unwrap();
    }
    drop(conn);
    let conn = core_storage::open(&db).unwrap();
    let state = app_web::test_support::state(data.clone(), conn);
    let app = app_web::router_without_dist(state);
    Fixture { app, db, data }
}

async fn get_json(app: &axum::Router, uri: &str) -> (u16, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let body = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

async fn post_json(app: &axum::Router, uri: &str, body: &str) -> (u16, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status().as_u16();
    let raw = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null),
    )
}

/// 恒等式：total = done + failed + skipped + cancelled（**收工后**的执行都必须成立）
fn assert_balanced(job: &serde_json::Value) {
    let g = |k: &str| job[k].as_i64().unwrap_or(-1);
    assert_eq!(
        g("total"),
        g("done") + g("failed") + g("skipped") + g("cancelled"),
        "total 恒等式破了: {job}"
    );
    assert_eq!(
        g("queued"),
        0,
        "收工后 queued 必须归零（未执行数看 cancelled）: {job}"
    );
}

/// 运行中的恒等式：total = done + failed + skipped + queued（cancelled 必须是 0）。
fn assert_balanced_running(job: &serde_json::Value) {
    let g = |k: &str| job[k].as_i64().unwrap_or(-1);
    assert_eq!(
        g("total"),
        g("done") + g("failed") + g("skipped") + g("queued"),
        "运行中恒等式破了（未结算条数必须落在 queued）: {job}"
    );
    assert_eq!(
        g("cancelled"),
        0,
        "运行中 cancelled 必须是 0（不是「未执行」）: {job}"
    );
}

/// 轮询判定状态直到 `running=false`（后台线程跑完），返回最后一份状态。
async fn wait_idle(app: &axum::Router, timeout: Duration) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    loop {
        let (code, j) = get_json(app, "/api/compliance/judge/status").await;
        assert_eq!(code, 200, "judge/status 必须 200: {j}");
        if j["running"] == serde_json::json!(false) {
            return j;
        }
        if Instant::now() > deadline {
            panic!("判定超时未收工，最后状态: {j}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// 主钉（IP-K16 插件通道版）：**一次批量审核 = 一个独立 run**，两次执行互不叠加。
///
/// 这条是 CMP-JOB-01 的 `each_judge_execution_is_its_own_job` 的执行账本等价物：
/// 上一轮把旧内联引擎夹具（`write_pack` + `packs/import`/`packs/activate`）整段删掉时，
/// 一并把「一个 batch = 一个 run、total = done+failed+skipped+cancelled、明细步骤名是人话、
/// 不存在 runId → 404」这几条**账本断言**也删了 —— 那是删夹具时误删被测语义。
/// 现在改由插件通道（`pack_host` 子进程 + 逐图 job）跑同一条链，断言一条不降：
///
/// 1. `POST /judge` 返回新 `runId`，真跑完；
/// 2. `/jobs` 每个 run 一行，统计只算本次，且 `total` 恒等式成立、`failed=0`；
/// 3. `/jobs/{runId}` 的 task 明细条数 = 本次入队条数，步骤名是人话（含规则名，不是内部 id）；
/// 4. 第二次执行拿到**新的** runId，`/jobs` 两行且新的在前；
/// 5. 不存在的 runId → 404（不编造空明细）。
///
/// 逐图通道只有 asset job（协议 v1.1 judge 单资产调用，宿主不排组级 job），4 张资产 = 4 条任务。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_judge_execution_is_its_own_run_on_plugin_path() {
    let _guard = vips_lock().lock().unwrap();
    let f = setup("runs");

    // ---- 第一次执行
    let (code, start) = post_json(&f.app, "/api/compliance/judge", "{}").await;
    if code != 200 {
        // 本环境装不了处理插件时（缺 python3 / 缺 packs/plugins）不静默放过：明确打出来。
        // cargo test 环境里仓库内置 `packs/plugins/ecommerce` 一定在 → 正常走 200 分支。
        let msg = start["message"].as_str().unwrap_or_default();
        panic!("判定入口必须 200（插件在仓库内置路径可装载），收到 {code}: {msg}");
    }
    let run1 = start["runId"].as_i64().expect("judge 必须返回 runId");
    assert!(run1 > 0, "{start}");
    assert_eq!(start["engine"], serde_json::json!("plugin"), "{start}");
    assert_eq!(start["candidates"], serde_json::json!(4), "{start}");
    assert_eq!(start["queuedAssets"], serde_json::json!(4), "{start}");
    assert_eq!(
        start["queuedGroups"],
        serde_json::json!(0),
        "宿主不排组级 job（插件自己成组）: {start}"
    );

    wait_idle(&f.app, Duration::from_secs(180)).await;

    // ---- 列表：第一行就是刚跑完的这次，数字自洽（4 条逐图任务）
    let (code, jobs) = get_json(&f.app, "/api/compliance/jobs").await;
    assert_eq!(code, 200, "/jobs 必须 200: {jobs}");
    assert_eq!(jobs["total"].as_i64(), Some(1), "只跑过一次: {jobs}");
    let j1 = &jobs["jobs"][0];
    assert_eq!(j1["runId"].as_i64(), Some(run1));
    assert_eq!(j1["status"], serde_json::json!("done"), "{j1}");
    assert_eq!(j1["statusLabel"], serde_json::json!("已完成"));
    assert_eq!(j1["failed"].as_i64(), Some(0), "不该有失败: {j1}");
    assert_eq!(
        j1["total"].as_i64(),
        Some(4),
        "4 张资产 = 4 条逐图任务: {j1}"
    );
    assert_eq!(j1["done"].as_i64(), Some(4), "{j1}");
    assert!(j1["durationMs"].as_i64().is_some(), "要能给耗时: {j1}");
    assert_balanced(j1);

    // ---- 明细：4 条 task，步骤名是人话（含插件自带规则名），不是内部 id
    let (code, detail) = get_json(&f.app, &format!("/api/compliance/jobs/{run1}")).await;
    assert_eq!(code, 200, "/jobs/{{runId}} 必须 200: {detail}");
    assert_eq!(detail["run"]["runId"].as_i64(), Some(run1), "{detail}");
    assert_eq!(detail["taskTotal"].as_i64(), Some(4), "{detail}");
    let tasks = detail["tasks"].as_array().expect("tasks 必须是数组");
    assert_eq!(tasks.len(), 4, "{detail}");
    for t in tasks {
        let step = t["step"].as_str().unwrap_or_default();
        assert!(step.contains("逐图判定"), "步骤名要说明阶段: {step:?}");
        assert!(
            step.contains("R0_product_main") || step.contains("R1_size"),
            "步骤名要点名插件自带规则: {step:?}"
        );
        assert!(
            !(step.starts_with("job") || step.starts_with("group")) && !step.is_empty(),
            "步骤名不许是内部 id: {step:?}"
        );
        assert!(t["statusLabel"].is_string(), "{t}");
        assert!(t.get("lastError").is_some(), "跳过/失败原因字段要在: {t}");
    }

    // ---- 不存在的 runId → 404（不编造空明细）
    let (code, missing) = get_json(&f.app, "/api/compliance/jobs/987654321").await;
    assert_eq!(code, 404, "不存在的 run 必须 404: {missing}");

    // ---- 第二次执行（全库重判 → 新 run，互不叠加）
    let (code, start2) = post_json(&f.app, "/api/compliance/judge", r#"{"all":true}"#).await;
    assert_eq!(code, 200, "第二次 judge 必须 200: {start2}");
    let run2 = start2["runId"].as_i64().expect("必须返回 runId");
    assert_ne!(run2, run1, "每次执行必须拿到新 runId");
    wait_idle(&f.app, Duration::from_secs(180)).await;

    let (code, jobs2) = get_json(&f.app, "/api/compliance/jobs").await;
    assert_eq!(code, 200, "{jobs2}");
    assert_eq!(jobs2["total"].as_i64(), Some(2), "两次执行两行: {jobs2}");
    let rows = jobs2["jobs"].as_array().unwrap();
    assert_eq!(
        rows[0]["runId"].as_i64(),
        Some(run2),
        "倒序：新的一次在前: {jobs2}"
    );
    assert_eq!(rows[1]["runId"].as_i64(), Some(run1), "{jobs2}");
    for r in rows {
        assert_balanced(r);
        assert_eq!(r["total"].as_i64(), Some(4), "两次各自 4 条，不叠加: {r}");
        assert_eq!(r["failed"].as_i64(), Some(0), "{r}");
    }

    let _ = std::fs::remove_dir_all(&f.data);
}

/// DM-B2（KI-37）：`/jobs` 行要能一眼读出「这次判了多少张、过/没过/待看各几张」。
///
/// 口径钉死：**一张图一个结论**（同一张图多条规则行时 `fail > undetermined > pass`），
/// 且只算本次 run（别的 run 的判定行不许串进来）。处理类 run 没有判定行 → `verdictCounts` 为 null。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jobs_list_reports_per_asset_verdict_counts() {
    use core_storage::compliance_runs::{run_status, ComplianceRunRepo};
    use core_storage::compliance_runs_sqlite::SqliteComplianceRunRepo;

    let _guard = vips_lock().lock().unwrap();
    let f = setup("verdict-counts");
    let conn = rusqlite::Connection::open(&f.db).unwrap();
    let repo = SqliteComplianceRunRepo::new(&conn);
    let run_judge = repo.start(svc_pipeline::COMPLIANCE_KIND, None).unwrap();
    let run_other = repo.start(svc_pipeline::COMPLIANCE_KIND, None).unwrap();
    let run_proc = repo.start(svc_pipeline::COMPLIANCE_KIND, None).unwrap();

    let put_job = |kind: &str, target: i64, run: i64| -> i64 {
        conn.execute(
            "INSERT INTO job_queue (kind, targetId, payload, status, createdAt, updatedAt, runId)
             VALUES (?1, ?2, NULL, 'done', 0, 0, ?3)",
            rusqlite::params![kind, target, run],
        )
        .unwrap();
        conn.last_insert_rowid()
    };
    let put_verdict = |job: i64, asset: i64, verdict: &str, rule: &str| {
        conn.execute(
            "INSERT INTO rule_verdict (packId, assetId, jobId, ruleId, result, verdict, confidence, ts)
             VALUES ('test-pack', ?1, ?2, ?3, 'ok', ?4, 0.9, 0)",
            rusqlite::params![asset, job, rule, verdict],
        )
        .unwrap();
    };

    // 本次 run：asset1 全过（pass）、asset2 一条不过（fail 压过 pass）、asset3 判不了（undetermined）
    let j1 = put_job(svc_pipeline::COMPLIANCE_KIND, 1, run_judge);
    let j2 = put_job(svc_pipeline::COMPLIANCE_KIND, 2, run_judge);
    let j3 = put_job(svc_pipeline::COMPLIANCE_KIND, 3, run_judge);
    put_verdict(j1, 1, "pass", "R1");
    put_verdict(j1, 1, "pass", "R2");
    put_verdict(j2, 2, "pass", "R1");
    put_verdict(j2, 2, "fail", "R2");
    put_verdict(j3, 3, "undetermined", "R1");
    // 别的 run：不该被算进上面那次
    let jo = put_job(svc_pipeline::COMPLIANCE_KIND, 4, run_other);
    put_verdict(jo, 4, "pass", "R1");
    // 处理类 run：同一队列 kind，只是没有判定行（plugin kinds 决定判/处理）
    let jp = put_job(svc_pipeline::COMPLIANCE_KIND, 1, run_proc);
    assert!(jp > 0);

    let (code, jobs) = get_json(&f.app, "/api/compliance/jobs").await;
    assert_eq!(code, 200, "{jobs}");
    let rows = jobs["jobs"].as_array().expect("jobs 是数组");
    let row = |run: i64| {
        rows.iter()
            .find(|r| r["runId"].as_i64() == Some(run))
            .unwrap_or_else(|| panic!("run {run} 不在列表: {jobs}"))
    };
    let mine = row(run_judge);
    assert_eq!(
        mine["totalAssets"].as_i64(),
        Some(3),
        "本次判了 3 张图: {mine}"
    );
    assert_eq!(mine["verdictCounts"]["pass"].as_i64(), Some(1), "{mine}");
    assert_eq!(mine["verdictCounts"]["fail"].as_i64(), Some(1), "{mine}");
    assert_eq!(
        mine["verdictCounts"]["undetermined"].as_i64(),
        Some(1),
        "{mine}"
    );
    let other = row(run_other);
    assert_eq!(
        other["totalAssets"].as_i64(),
        Some(1),
        "别的 run 不串账: {other}"
    );
    assert_eq!(other["verdictCounts"]["pass"].as_i64(), Some(1), "{other}");
    let proc = row(run_proc);
    assert!(
        proc["verdictCounts"].is_null(),
        "处理类没有判定分布（不许编造合格数）: {proc}"
    );

    // 收工后同一行口径不变（快照不漂）
    repo.finish(
        run_judge,
        run_status::DONE,
        None,
        &svc_pipeline::COMPLIANCE_KINDS,
    )
    .unwrap();
    let (code, jobs2) = get_json(&f.app, "/api/compliance/jobs").await;
    assert_eq!(code, 200, "{jobs2}");
    let row2 = jobs2["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["runId"].as_i64() == Some(run_judge))
        .unwrap();
    assert_eq!(row2["totalAssets"].as_i64(), Some(3), "{row2}");
    assert_eq!(row2["verdictCounts"]["fail"].as_i64(), Some(1), "{row2}");

    let _ = std::fs::remove_dir_all(&f.data);
}

/// GAP-2 钉（卡 task）：**运行中**的 `cancelled` 必须是 0，排队中的条数走 `queued`。
///
/// 场景是确定性的（不靠「跑到一半去抢一次轮询」）：直接造一个 running 的 run + 5 条
/// 未结算任务（3 逐图 pending + 2 组级 pending）+ 3 条已结算，然后读 HTTP。
/// 收工（cancelled）后同一行的 `cancelled` 才等于那 5 条，`queued` 归零。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_run_reports_queued_not_cancelled() {
    use core_storage::compliance_runs::{run_status, ComplianceRunRepo};
    use core_storage::compliance_runs_sqlite::SqliteComplianceRunRepo;

    let _guard = vips_lock().lock().unwrap();
    let f = setup("running-queued");
    let conn = rusqlite::Connection::open(&f.db).unwrap();
    let repo = SqliteComplianceRunRepo::new(&conn);
    let run = repo.start(svc_pipeline::COMPLIANCE_KIND, None).unwrap();
    let kind = svc_pipeline::COMPLIANCE_KIND;
    let gkind = svc_pipeline::COMPLIANCE_GROUP_KIND;
    let put = |k: &str, target: i64, status: &str| {
        conn.execute(
            "INSERT INTO job_queue (kind, targetId, payload, status, createdAt, updatedAt, runId)
             VALUES (?1, ?2, NULL, ?3, 0, 0, ?4)",
            rusqlite::params![k, target, status, run],
        )
        .unwrap();
    };
    // 已结算：2 成功 + 1 失败
    put(kind, 1, "done");
    put(kind, 2, "done");
    put(kind, 3, "failed");
    // 未结算：3 逐图 + 2 组级还在排队
    put(kind, 4, "pending");
    put(kind, 5, "pending");
    put(kind, 6, "running");
    put(gkind, 10, "pending");
    put(gkind, 11, "pending");

    // ---- 运行中：cancelled=0，未结算 5 条落在 queued
    let (code, cur) = get_json(&f.app, "/api/compliance/jobs/current").await;
    assert_eq!(code, 200, "{cur}");
    let live = &cur["run"];
    assert_eq!(live["runId"].as_i64(), Some(run), "{cur}");
    assert_eq!(live["status"], serde_json::json!("running"), "{cur}");
    assert_eq!(live["total"].as_i64(), Some(8), "{cur}");
    assert_eq!(live["done"].as_i64(), Some(2), "{cur}");
    assert_eq!(live["failed"].as_i64(), Some(1), "{cur}");
    assert_eq!(
        live["queued"].as_i64(),
        Some(5),
        "排队中必须落在 queued: {cur}"
    );
    assert_balanced_running(live);

    // queueCurrent 同口径：*Pending/queued 是排队，cancelled 必须是 0
    let q = &cur["queue"];
    assert_eq!(q["assetPending"].as_i64(), Some(3), "{cur}");
    assert_eq!(q["groupPending"].as_i64(), Some(2), "{cur}");
    assert_eq!(q["queued"].as_i64(), Some(5), "{cur}");
    assert_eq!(
        q["cancelled"].as_i64(),
        Some(0),
        "运行中 queueCurrent.cancelled 必须是 0: {cur}"
    );

    // 老入口 /judge/status 的 run / queueCurrent 也是同一口径（不是「未执行 5」）
    let (code, st) = get_json(&f.app, "/api/compliance/judge/status").await;
    assert_eq!(code, 200, "{st}");
    assert_eq!(st["run"]["cancelled"].as_i64(), Some(0), "{st}");
    assert_eq!(st["run"]["queued"].as_i64(), Some(5), "{st}");
    assert_balanced_running(&st["run"]);
    assert_eq!(st["queueCurrent"]["cancelled"].as_i64(), Some(0), "{st}");
    assert_eq!(st["queueCurrent"]["queued"].as_i64(), Some(5), "{st}");

    // ---- 收工（用户取消）：未执行的 5 条这时才叫 cancelled
    let row = repo
        .finish(
            run,
            run_status::CANCELLED,
            None,
            &svc_pipeline::COMPLIANCE_KINDS,
        )
        .unwrap();
    assert_eq!(
        (row.total, row.done, row.failed, row.cancelled),
        (8, 2, 1, 5),
        "{row:?}"
    );
    let (code, jobs) = get_json(&f.app, "/api/compliance/jobs").await;
    assert_eq!(code, 200, "{jobs}");
    let done_row = &jobs["jobs"][0];
    assert_eq!(done_row["status"], serde_json::json!("cancelled"), "{jobs}");
    assert_eq!(done_row["cancelled"].as_i64(), Some(5), "{jobs}");
    assert_balanced(done_row); // 同时断言 queued 归零

    let _ = std::fs::remove_dir_all(&f.data);
}
