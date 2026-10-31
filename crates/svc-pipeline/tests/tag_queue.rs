//! D4 自动打标队列集成测试：真实 SQLite + mock OpenAI 服务端（复用 D3 的
//! MockServer 模式），覆盖验收全场景——正常批、断点续跑（崩溃残留 running）、
//! 失败重试 3 次转 failed、取消恢复、retag 幂等、非 eligible 跳过。

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::MockServer;
use core_config::{load_registry, TagSettings};
use engine_tag::openai_compat::OpenAiCompatTagProvider;
use engine_tag::TagProvider;
use rusqlite::Connection;
use svc_pipeline::{
    enq_all, enq_untagged, retry_failed, run_tag_queue, tag_failed_list, tag_stats,
};

/// 计数器 responder：按调用次数切换「成功 / 500」——同 batch 内可控注入失败。
/// 返回 (status, body)：成功时回带标签的 chat completion。
fn ok_body(idx: usize) -> String {
    format!(
        r#"{{"choices":[{{"message":{{"content":"{{\"title_ch\":\"图{idx}\",\"title_en\":\"img{idx}\",\"description\":\"desc{idx}\",\"tags\":[{{\"name\":\"t{idx}\",\"score\":0.9}}]}}"}}}}]}}"#
    )
}

/// 1×1 PNG 真实字节（tag client 读文件后转 data URL）。
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

fn tag_spec() -> core_config::ModelSpec {
    load_registry(None)
        .unwrap()
        .models
        .into_iter()
        .find(|m| m.kind == core_config::SpecKind::Tagging)
        .unwrap()
}

fn provider(base_url: &str) -> OpenAiCompatTagProvider {
    let cfg = TagSettings {
        enabled: true,
        base_url: format!("{base_url}/v1"),
        api_key: "test".into(),
        model: "mock-vl".into(),
        proxy: Some(String::new()), // 直连
        timeout_ms: 10_000,
        ..Default::default()
    };
    let mut p = OpenAiCompatTagProvider::new(tag_spec(), cfg);
    p.load().unwrap();
    p
}

/// 全成功 responder（每张图独立 title/tag，idx 来自请求体内文件差异无意义，
/// 这里用递增计数器保证每张图拿到不同标签）。
fn counting_ok(counter: &Arc<AtomicUsize>) -> impl Fn(&str, &str) -> (u16, String) + Send + Sync {
    let c = counter.clone();
    move |_p, _b| {
        let i = c.fetch_add(1, Ordering::SeqCst);
        (200, ok_body(i))
    }
}

static NOS: Mutex<()> = Mutex::new(()); // SQLite 文件互不冲突即可，无需全局锁

#[test]
fn normal_batch_tags_all_and_enq_untagged_is_incremental() {
    let _g = NOS.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let conn = setup(tmp.path(), 5);
    let counter = Arc::new(AtomicUsize::new(0));
    let srv = MockServer::start(counting_ok(&counter));
    let p = provider(&srv.base_url);

    let queued = enq_untagged(&conn).unwrap();
    assert_eq!(queued, 5);
    let cancel = AtomicBool::new(false);
    let mut events = 0usize;
    let report = run_tag_queue(&conn, &p, &cancel, &mut |_| events += 1).unwrap();
    assert_eq!(report.done, 5);
    assert_eq!(report.failed, 0);
    assert!(!report.cancelled);
    assert!(events >= 5);

    // 全部 tag origin=auto，title_ch 非空
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tag WHERE origin='auto' AND value LIKE 't%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 5);
    let titles: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM assets WHERE title_ch IS NOT NULL AND title_ch != ''",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(titles, 5);

    // 二次入队：已打标资产不再入队
    assert_eq!(enq_untagged(&conn).unwrap(), 0);
    let stats = tag_stats(&conn).unwrap();
    assert_eq!(stats.done, 5);
    assert_eq!(stats.pending, 0);
    assert_eq!(stats.untaggedAssets, 0);
}

#[test]
fn crash_midway_resumes_without_duplicate_jobs() {
    let _g = NOS.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let conn = setup(tmp.path(), 4);
    let counter = Arc::new(AtomicUsize::new(0));
    let srv = MockServer::start(counting_ok(&counter));
    let p = provider(&srv.base_url);

    enq_untagged(&conn).unwrap();
    // 模拟崩溃：直接把两个 job 置 running（进程死掉时的残留态）
    conn.execute(
        "UPDATE job_queue SET status='running' WHERE id IN (SELECT id FROM job_queue WHERE kind='tag' ORDER BY id LIMIT 2)",
        [],
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let report = run_tag_queue(&conn, &p, &cancel, &mut |_| {}).unwrap();
    // requeue_stale 回收 2 个 + 原有 2 个 = 4 全部完成
    assert_eq!(report.done, 4, "stale running 回收后续跑完成");
    // 无重复 job：4 资产恰好 4 行 done
    let (done, failed, pending) = {
        let s = tag_stats(&conn).unwrap();
        (s.done, s.failed, s.pending)
    };
    assert_eq!((done, failed, pending), (4, 0, 0));
    let jobs: i64 = conn
        .query_row("SELECT COUNT(*) FROM job_queue WHERE kind='tag'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(jobs, 4, "无重复 job 行");
}

#[test]
fn fail_three_times_then_failed_with_retry_count() {
    let _g = NOS.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let conn = setup(tmp.path(), 1);
    // 恒 500
    let srv = MockServer::start(|_p, _b| (500, r#"{"error":{"message"}}"#.to_string()));
    let p = provider(&srv.base_url);

    enq_untagged(&conn).unwrap();
    let cancel = AtomicBool::new(false);
    // 同一次 run 内：attempts 1→pending, 2→pending, 3→failed（循环 claim 会重取 pending）
    let report = run_tag_queue(&conn, &p, &cancel, &mut |_| {}).unwrap();
    assert_eq!(report.failed, 1);
    assert_eq!(report.retry, 2, "前两次失败进重试");
    assert_eq!(srv.count(), 3, "恰好请求 3 次");

    let row: (i64, String) = conn
        .query_row(
            "SELECT attempts, COALESCE(lastError,'') FROM job_queue WHERE kind='tag'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(row.0, 3, "retry_count=3");
    assert!(row.1.contains("500"), "lastError 含上游状态码: {}", row.1);

    let failed = tag_failed_list(&conn, 10).unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].attempts, 3);

    // retry_failed 重置后再跑：mock 仍 500 → 再次 3 连败（可重复触发，不炸）
    assert_eq!(retry_failed(&conn).unwrap(), 1);
    let report2 = run_tag_queue(&conn, &p, &cancel, &mut |_| {}).unwrap();
    assert_eq!(report2.failed, 1);
    assert_eq!(srv.count(), 6);
}

#[test]
fn retag_all_is_idempotent_tags_not_duplicated() {
    let _g = NOS.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let conn = setup(tmp.path(), 3);
    // 固定响应（同一模板重跑 → 标签名相同）才能验证幂等；计数版会产出新名字
    let srv = MockServer::start(|_p, _b| (200, ok_body(7)));
    let p = provider(&srv.base_url);
    let cancel = AtomicBool::new(false);

    // 第一轮
    enq_untagged(&conn).unwrap();
    run_tag_queue(&conn, &p, &cancel, &mut |_| {}).unwrap();
    let tags1: i64 = conn
        .query_row("SELECT COUNT(*) FROM tag", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tags1, 3);

    // retag_all：job 表清空重建，再跑
    assert_eq!(enq_all(&conn).unwrap(), 3);
    let report = run_tag_queue(&conn, &p, &cancel, &mut |_| {}).unwrap();
    assert_eq!(report.done, 3);
    let tags2: i64 = conn
        .query_row("SELECT COUNT(*) FROM tag", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tags2, 3, "ON CONFLICT 幂等：tag 行不重复追加");
    // 每个 asset 恰好 1 个 auto 标签
    let dup: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM (SELECT assetId FROM tag GROUP BY assetId HAVING COUNT(*) > 1)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dup, 0);
    let jobs: i64 = conn
        .query_row("SELECT COUNT(*) FROM job_queue WHERE kind='tag'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(jobs, 3, "retag 重建后无残留 job 行");
}

#[test]
fn cancel_returns_jobs_to_pending() {
    let _g = NOS.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let conn = setup(tmp.path(), 4);
    let counter = Arc::new(AtomicUsize::new(0));
    let srv = MockServer::start(counting_ok(&counter));
    let p = provider(&srv.base_url);

    enq_untagged(&conn).unwrap();
    // 第 2 个 job 处理前置 cancel → 批内剩余回 pending
    let cancel = AtomicBool::new(false);
    let mut seen = 0usize;
    let report = run_tag_queue(&conn, &p, &cancel, &mut |_| {
        seen += 1;
        if seen >= 2 {
            cancel.store(true, Ordering::SeqCst);
        }
    })
    .unwrap();
    assert!(report.cancelled);
    assert!(report.done < 4);
    let stats = tag_stats(&conn).unwrap();
    assert_eq!(stats.done + stats.pending, 4, "取消后未完成 job 回 pending");
    assert_eq!(stats.pending, 4 - stats.done);

    // 恢复跑完
    cancel.store(false, Ordering::SeqCst);
    let _report = run_tag_queue(&conn, &p, &cancel, &mut |_| {}).unwrap();
    let final_stats = tag_stats(&conn).unwrap();
    assert_eq!(final_stats.done, 4);
    assert_eq!(final_stats.pending, 0);
}

#[test]
fn ineligible_assets_are_skipped() {
    let _g = NOS.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let conn = setup(tmp.path(), 3);
    // 入队时全部 eligible；入队后 0 号失联、1 号软删 → run 时这两个 job 应跳过
    assert_eq!(enq_untagged(&conn).unwrap(), 3);
    conn.execute(
        "UPDATE assets SET missing = 1 WHERE fileName = 'img0.png'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE assets SET deleted = 1 WHERE fileName = 'img1.png'",
        [],
    )
    .unwrap();

    let counter = Arc::new(AtomicUsize::new(0));
    let srv = MockServer::start(counting_ok(&counter));
    let p = provider(&srv.base_url);
    let cancel = AtomicBool::new(false);

    let report = run_tag_queue(&conn, &p, &cancel, &mut |_| {}).unwrap();
    assert_eq!(report.skipped, 2, "失联/删除资产跳过不浪费重试");
    assert_eq!(report.done, 1);
    assert_eq!(report.jobDone, 3);
    let final_stats = tag_stats(&conn).unwrap();
    assert_eq!(final_stats.done, 3);
    // 二次入队只剩 0（1 删除、2 已打标）
    assert_eq!(
        enq_untagged(&conn).unwrap(),
        0,
        "已打标资产不再入队（0 号虽失联也不入）"
    );
}

// ---------- 失败分桶 + 按类重试（卡 task） ----------

#[test]
fn failure_class_buckets_and_class_scoped_retry() {
    use svc_pipeline::{classify_tag_failure, retry_failed_class};

    // 新文案（engine-tag TagError::Truncated）、老文案（EOF while parsing）、真错
    assert_eq!(
        classify_tag_failure(
            "response truncated at max_tokens=1024（设置页覆盖）: finish_reason=length"
        ),
        "truncated"
    );
    assert_eq!(
        classify_tag_failure(
            "tag: response parse: json: EOF while parsing a list at line 29 column 5; raw={"
        ),
        "truncated",
        "老版本落库的截断错误也要归到截断类（历史 job 一并可救）"
    );
    assert_eq!(
        classify_tag_failure("tag: upstream: HTTP 500 from http://x/v1: model not found"),
        "other"
    );
    assert_eq!(
        classify_tag_failure("tag: io: No such file or directory (os error 2)"),
        "other"
    );

    let tmp = tempfile::tempdir().unwrap();
    let conn = core_storage::open(&tmp.path().join("t.db")).unwrap();
    let errs = [
        "tag: response truncated at max_tokens=1024（设置页覆盖）: finish_reason=length",
        "tag: response parse: json: EOF while parsing a list at line 3 column 1; raw={",
        "tag: upstream: HTTP 500 from http://x/v1: model not found",
        "tag: io: No such file or directory (os error 2)",
    ];
    for (i, e) in errs.iter().enumerate() {
        conn.execute(
            "INSERT INTO job_queue (kind, targetId, payload, status, attempts, lastError, createdAt, updatedAt)
             VALUES ('tag', ?1, NULL, 'failed', 3, ?2, 0, 0)",
            rusqlite::params![i as i64 + 1, e],
        )
        .unwrap();
    }

    let items = tag_failed_list(&conn, 10).unwrap();
    assert_eq!(items.len(), 4);
    assert_eq!(
        items.iter().filter(|i| i.errorClass == "truncated").count(),
        2,
        "两条截断（新+老文案）"
    );
    assert_eq!(
        items.iter().filter(|i| i.errorClass == "other").count(),
        2,
        "两条真错"
    );

    // 只重试截断类：剩下 2 条 other 原样不动（不用重建整队列）
    assert_eq!(retry_failed_class(&conn, Some("truncated")).unwrap(), 2);
    let left = tag_failed_list(&conn, 10).unwrap();
    assert_eq!(left.len(), 2);
    assert!(left.iter().all(|i| i.errorClass == "other"));

    // 再按「其它」重试 → 清零
    assert_eq!(retry_failed_class(&conn, Some("other")).unwrap(), 2);
    assert!(tag_failed_list(&conn, 10).unwrap().is_empty());

    // 缺省/空 class = 全部（老 UI/老调用方兼容）
    conn.execute(
        "INSERT INTO job_queue (kind, targetId, payload, status, attempts, lastError, createdAt, updatedAt)
         VALUES ('tag', 99, NULL, 'failed', 3, 'any error', 0, 0)",
        [],
    )
    .unwrap();
    assert_eq!(retry_failed_class(&conn, None).unwrap(), 1);
    assert_eq!(retry_failed(&conn).unwrap(), 0, "已无失败项");
}
