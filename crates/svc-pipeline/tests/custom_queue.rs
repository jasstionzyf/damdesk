//! D10 自定义打标队列集成测试：真实 SQLite + mock OpenAI 服务端（复用 D4 的
//! MockServer 模式，走 OpenAiCompatTagProvider 真实 HTTP inline 通道）。
//! 覆盖验收场景——建 2 任务跑 N 图、origin=custom 入库、空值约定、重跑 upsert
//! 不重复、停用/删除任务跳过、上游失败重试 3 次转 failed、取消回 pending。

mod common;

use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

use common::MockServer;
use core_config::{load_registry, TagSettings};
use core_storage::custom_tasks as tasks;
use engine_tag::openai_compat::OpenAiCompatTagProvider;
use engine_tag::TagProvider;
use rusqlite::Connection;
use svc_pipeline::run_custom_queue;

/// 1×1 PNG 真实字节（tag client 读文件后转 data URL）。与 D4 tag_queue 测试同源。
const PNG: [u8; 67] = [
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

fn setup(dir: &std::path::Path, n_images: usize) -> Connection {
    let conn = core_storage::open(&dir.join("test.db")).unwrap();
    for i in 0..n_images {
        let p = dir.join(format!("img{i}.png"));
        std::fs::write(&p, PNG).unwrap();
        conn.execute(
            "INSERT INTO assets (mediaType, absPath, storeMode, fileName, fileExt, fileSz, mtime,
                source, searchable, deleted, approvalStatus, starRating, importedAt)
             VALUES ('image', ?1, 'reference', ?2, 'png', 67, 0, 'local', 1, 0, 'approved', 0, unixepoch())",
            rusqlite::params![p.display().to_string(), format!("img{i}.png")],
        )
        .unwrap();
    }
    conn
}

fn provider(base_url: &str) -> OpenAiCompatTagProvider {
    let spec = load_registry(None)
        .unwrap()
        .models
        .into_iter()
        .find(|m| m.kind == core_config::SpecKind::Tagging)
        .unwrap();
    let cfg = TagSettings {
        enabled: true,
        base_url: format!("{base_url}/v1"),
        api_key: "test".into(),
        model: "mock-vl".into(),
        proxy: Some(String::new()), // 直连
        timeout_ms: 10_000,
        ..Default::default()
    };
    let mut p = OpenAiCompatTagProvider::new(spec, cfg);
    p.load().unwrap();
    p
}

fn chat_body(content: &str) -> String {
    serde_json::json!({"choices":[{"message":{"content":content}}]}).to_string()
}

/// 按请求体里出现的用户 prompt 分流：地点 → 围栏 JSON 数组（统一标签契约，带空白值
/// 顺带验归一）；色调 → 「无」（空值约定）；其他 → 旧 {"value": ...} 形态（兼容通道）。
/// 可直接断言 inline 约束模板已注入。
fn inline_responder() -> impl Fn(&str, &str) -> (u16, String) + Send + Sync {
    |_p, body| {
        let content = if body.contains("这张照片的拍摄地点是什么") {
            "```json\n[\"海边 日落\"]\n```"
        } else if body.contains("描述主色调与氛围") {
            "无"
        } else if body.contains("任意维度") {
            r#"{"value":"通用值"}"#
        } else {
            "未识别 prompt"
        };
        (200, chat_body(content))
    }
}

fn custom_rows(c: &Connection) -> Vec<(i64, String, String, Option<i64>, String)> {
    c.prepare(
        "SELECT t.assetId, COALESCE(r.name,''), t.value, t.taskId, COALESCE(t.model,'')
         FROM tag t LEFT JOIN tag_result r ON r.assetId = t.assetId AND r.taskId = t.taskId
         WHERE t.origin = 'custom' ORDER BY t.assetId, t.value",
    )
    .unwrap()
    .query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .unwrap()
    .map(|r| r.unwrap())
    .collect()
}

static NOS: Mutex<()> = Mutex::new(());

#[test]
fn two_tasks_over_images_tag_and_filter_names() {
    let _g = NOS.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let conn = setup(tmp.path(), 10); // 验收口径：10 张图
    let srv = MockServer::start(inline_responder());
    let p = provider(&srv.base_url);

    // 建 2 个自定义任务（验收口径：拍摄地点 / 色调氛围）
    let t_loc = tasks::create_task(&conn, "拍摄地点", "这张照片的拍摄地点是什么？", true).unwrap();
    let t_mood = tasks::create_task(&conn, "色调氛围", "描述主色调与氛围", true).unwrap();
    let mut queued = 0;
    for t in [t_loc, t_mood] {
        queued += tasks::enqueue_task_all(&conn, t).unwrap();
    }
    assert_eq!(queued, 20); // 2 任务 × 10 图

    let cancel = AtomicBool::new(false);
    let mut done_events = 0;
    let report = run_custom_queue(&conn, &p, &cancel, &mut |ev| {
        if ev.status == "done" {
            done_events += 1;
        }
    })
    .unwrap();
    assert_eq!((report.done, report.failed, report.skipped), (20, 0, 0));

    // 地点任务：10 行 origin=custom，数组回复 → 归一（空白→_）+ key-val 拍平；模型/taskId 溯源齐
    let rows = custom_rows(&conn);
    let spec_name = p.spec().name.clone();
    assert_eq!(rows.len(), 10);
    for (aid, dim, value, tid, model) in &rows {
        assert_eq!(
            (dim.as_str(), value.as_str(), *tid, model.as_str()),
            (
                "拍摄地点",
                "拍摄地点-海边_日落",
                Some(t_loc),
                spec_name.as_str()
            ),
            "asset {aid}"
        );
    }

    // 色调任务走「无」→ 空值约定，不写行
    let mood_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tag WHERE taskId = ?1",
            [t_mood],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(mood_rows, 0);
    assert_eq!(done_events, 20);

    // 搜索过滤面板清单（tag_cloud distinct 标签值）：拍平后的 key-val 标签在列，
    // 裸值与裸维度名都不进 tag
    let vals: Vec<String> = conn
        .prepare("SELECT DISTINCT value FROM tag ORDER BY value")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(vals.contains(&"拍摄地点-海边_日落".to_string()), "{vals:?}");
    assert!(
        !vals.contains(&"海边_日落".to_string()),
        "裸值不进 tag：{vals:?}"
    );
    assert!(
        !vals.contains(&"拍摄地点".to_string()),
        "裸维度名不进 tag：{vals:?}"
    );

    // inline 约束模板确实注入（用户要求原文 + 输出约束 + 数组契约 + 空内容约定 []）。
    // 注意：requests 锁必须在此作用域内释放，否则 mock 服务线程会被阻塞。
    {
        let bodies = srv.requests.lock().unwrap();
        assert!(bodies.len() >= 20);
        let first = &bodies[0].2;
        assert!(first.contains("这张照片的拍摄地点是什么"));
        assert!(first.contains("输出要求"));
        assert!(first.contains("JSON 字符串数组"));
        assert!(first.contains("[]"));
    }

    // 重跑（改 prompt 走「任意维度」→ JSON value 形态）：upsert 不重复插
    tasks::update_task(&conn, t_loc, "拍摄地点", "任意维度", true).unwrap();
    assert_eq!(tasks::enqueue_task_all(&conn, t_loc).unwrap(), 10);
    let report = run_custom_queue(&conn, &p, &cancel, &mut |_| {}).unwrap();
    assert_eq!(report.done, 10);
    let rows = custom_rows(&conn);
    assert_eq!(rows.len(), 10); // 无重复（重跑清旧再插）
    assert!(rows
        .iter()
        .all(|(_, _, v, _, _)| v.as_str() == "拍摄地点-通用值"));
}

#[test]
fn disabled_or_deleted_task_jobs_skip() {
    let _g = NOS.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let conn = setup(tmp.path(), 2);
    let srv = MockServer::start(inline_responder());
    let p = provider(&srv.base_url);

    let t = tasks::create_task(&conn, "维度A", "任意维度", true).unwrap();
    tasks::enqueue_task_all(&conn, t).unwrap();
    // 停用后跑：job 全 skipped 完成，不发起推理
    tasks::update_task(&conn, t, "维度A", "任意维度", false).unwrap();
    let report = run_custom_queue(&conn, &p, &AtomicBool::new(false), &mut |_| {}).unwrap();
    assert_eq!((report.done, report.skipped), (0, 2));
    assert_eq!(srv.count(), 0);
    assert_eq!(custom_rows(&conn).len(), 0);

    // 删除任务后残留 job 同样跳过；列表计数归零
    tasks::enqueue_task_all(&conn, t).unwrap(); // enabled 才入队 → 0
    conn.execute(
        "INSERT INTO job_queue (kind, targetId, payload, status, createdAt, updatedAt)
         VALUES ('custom_tag', (SELECT MIN(assetId) FROM assets), ?, 'pending', unixepoch(), unixepoch())",
        [tasks::custom_payload(t)],
    )
    .unwrap();
    tasks::update_task(&conn, t, "维度A", "任意维度", true).unwrap();
    tasks::delete_task(&conn, t, false).unwrap();
    let report = run_custom_queue(&conn, &p, &AtomicBool::new(false), &mut |_| {}).unwrap();
    assert_eq!((report.done, report.skipped), (0, 1));
}

#[test]
fn upstream_fail_retries_then_failed_and_retry_reset() {
    let _g = NOS.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let conn = setup(tmp.path(), 1);
    let srv = MockServer::start(|_p, _b| (500, r#"{"error":{"message":"mock down"}}"#.into()));
    let p = provider(&srv.base_url);

    let t = tasks::create_task(&conn, "维度B", "任意维度", true).unwrap();
    tasks::enqueue_task_all(&conn, t).unwrap();
    let report = run_custom_queue(&conn, &p, &AtomicBool::new(false), &mut |_| {}).unwrap();
    assert_eq!(report.done, 0);
    assert_eq!(report.failed, 1);
    assert_eq!(report.retry, 2); // 3 次尝试 = 1 终态 + 2 次回 pending
    assert_eq!(srv.count(), 3);
    assert!(report.errors[0].contains("mock down"));

    let failed = svc_pipeline::custom_failed_list(&conn, 10).unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].taskName, "维度B");

    // 重试失败项 → pending
    assert_eq!(tasks::retry_failed(&conn).unwrap(), 1);
    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM job_queue WHERE kind='custom_tag' AND status='pending'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending, 1);
}

#[test]
fn cancel_leaves_pending_for_resume() {
    let _g = NOS.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let conn = setup(tmp.path(), 2);
    let srv = MockServer::start(inline_responder());
    let p = provider(&srv.base_url);

    let t = tasks::create_task(&conn, "维度C", "任意维度", true).unwrap();
    tasks::enqueue_task_all(&conn, t).unwrap();
    let cancel = AtomicBool::new(true); // 一进来就取消
    let report = run_custom_queue(&conn, &p, &cancel, &mut |_| {}).unwrap();
    assert!(report.cancelled);
    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM job_queue WHERE kind='custom_tag' AND status='pending'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending, 2); // 未处理 job 回 pending（不丢任务）

    // 复位后可续跑干净
    let cancel = AtomicBool::new(false);
    let report = run_custom_queue(&conn, &p, &cancel, &mut |_| {}).unwrap();
    assert_eq!(report.done, 2);
    assert_eq!(custom_rows(&conn).len(), 2);
}
