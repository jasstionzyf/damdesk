//! D4 验收基准：真实 SQLite + mock OpenAI 兼容服务端，跑 200 张图的全流程、
//! 断点续跑、取消恢复、失败重试、retag 幂等。进程级可复现：
//!
//! ```sh
//! cargo run -p svc-pipeline --example d4_bench --release -- 200 all
//! ```
//!
//! 场景矩阵（arg2，默认 all）：
//! - `full`   : 200 图入队→跑完，统计吞吐
//! - `resume` : 崩溃残留 running → requeue_stale 续跑
//! - `cancel` : 中途取消 → 回 pending → 恢复跑完
//! - `retry`  : 上游恒 500 → 3 次尝试转 failed → retry_failed 重置
//! - `retag`  : enq_all 全库重跑 → tag 行数不变（幂等）
//!
//! mock tag 服务端在进程内起（与 tests/common 的 MockServer 同构，独立实现避免
//! 测试工具跨 crate 依赖）。日志打 stdout，退出码 0 = 全部通过。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use core_config::{load_registry, TagSettings};
use engine_tag::openai_compat::OpenAiCompatTagProvider;
use engine_tag::TagProvider;
use rusqlite::Connection;
use svc_pipeline::{enq_all, enq_untagged, retry_failed, run_tag_queue, tag_stats};

// ---------- mock OpenAI 兼容 chat 服务（可注入故障） ----------

type Responder = Arc<dyn Fn(&str, &str) -> (u16, String) + Send + Sync>;

struct MockServer {
    base_url: String,
    hits: Arc<AtomicUsize>,
}

/// 每次调用返回递增编号的标签 JSON（chat completion 包装）。
fn ok_body(idx: usize) -> String {
    format!(
        r#"{{"choices":[{{"message":{{"content":"{{\"title_ch\":\"图{idx}\",\"title_en\":\"img{idx}\",\"description\":\"desc{idx}\",\"tags\":[{{\"name\":\"t{idx}\",\"score\":0.9}}]}}"}}}}]}}"#
    )
}

impl MockServer {
    fn start(responder: Responder) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        let responder2 = Arc::clone(&responder);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let hits3 = hits2.clone();
                let resp = Arc::clone(&responder2);
                std::thread::spawn(move || {
                    if let Some((method, _path, body)) = read_request(&mut s) {
                        if method != "GET" {
                            hits3.fetch_add(1, Ordering::SeqCst);
                        }
                        let (status, resp_body) = resp(&body, &body);
                        let _ = write_response(&mut s, status, &resp_body);
                    }
                });
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            hits,
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

fn read_request(stream: &mut TcpStream) -> Option<(String, String, String)> {
    let mut chunk = [0u8; 4096];
    // 先读到 header 结束（body 可能分包，简单起见循环读满 content-length）
    let mut total = String::new();
    let content_len = loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        total.push_str(&String::from_utf8_lossy(&chunk[..n]));
        if let Some(pos) = total.find("\r\n\r\n") {
            let head = &total[..pos];
            let cl = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    if k.trim().eq_ignore_ascii_case("content-length") {
                        v.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            if total.len() >= pos + 4 + cl {
                break cl;
            }
        }
    };
    let _ = content_len;
    let mut parts = total.splitn(3, ' ');
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let body = total.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    Some((method, path, body))
}

fn write_response(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let bytes = body.as_bytes();
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        bytes.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(bytes)
}

// ---------- 测试资产 ----------

const PNG: [u8; 67] = [
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

fn insert_asset(conn: &Connection, dir: &std::path::Path, file: &str) {
    let p = dir.join(file);
    std::fs::write(&p, PNG).unwrap();
    conn.execute(
        "INSERT INTO assets (mediaType, absPath, storeMode, fileName, fileExt, fileSz, mtime,
            source, searchable, deleted, approvalStatus, starRating, importedAt)
         VALUES ('image', ?1, 'reference', ?2, 'png', 67, 0, 'local', 1, 0, 'approved', 0, unixepoch())",
        rusqlite::params![p.display().to_string(), file],
    )
    .unwrap();
}

fn setup(dir: &std::path::Path, n_images: usize) -> Connection {
    let conn = core_storage::open(&dir.join("bench.db")).unwrap();
    for i in 0..n_images {
        insert_asset(&conn, dir, &format!("img{i}.png"));
    }
    conn
}

/// 持久目录下的库：已存在则复用（进程被 kill 后重启同一目录即续跑）。
fn open_existing(dir: &std::path::Path) -> Connection {
    core_storage::open(&dir.join("bench.db")).unwrap()
}

fn counts(conn: &Connection) -> (i64, i64, i64, i64, i64, i64, i64) {
    let jobs: i64 = conn
        .query_row("SELECT COUNT(*) FROM job_queue WHERE kind='tag'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM job_queue WHERE kind='tag' AND status='pending'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let running: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM job_queue WHERE kind='tag' AND status='running'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let done: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM job_queue WHERE kind='tag' AND status='done'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let failed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM job_queue WHERE kind='tag' AND status='failed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    (
        jobs,
        pending,
        running,
        done,
        failed,
        tag_count(conn),
        dup_assets(conn),
    )
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
        api_key: "bench".into(),
        model: "mock-vl".into(),
        proxy: Some(String::new()),
        timeout_ms: 10_000,
        ..Default::default()
    };
    let mut p = OpenAiCompatTagProvider::new(tag_spec(), cfg);
    p.load().unwrap();
    p
}

fn counting_ok(counter: &Arc<AtomicUsize>) -> Responder {
    let c = Arc::clone(counter);
    Arc::new(move |_p, _b| {
        let i = c.fetch_add(1, Ordering::SeqCst);
        (200, ok_body(i))
    })
}

fn always_500() -> Responder {
    Arc::new(move |_p, _b| (500, r#"{"error":{"message":"bench 500"}}"#.to_string()))
}

/// 按请求体哈希确定响应：同一张图两次调用得到同名标签（幂等验收用）。
fn hashed_ok() -> Responder {
    use std::hash::{Hash, Hasher};
    Arc::new(move |_p, body| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        body.hash(&mut h);
        let i = (h.finish() % 100_000) as usize;
        (200, ok_body(i))
    })
}

// ---------- 断言 ----------

static FAILED: AtomicUsize = AtomicUsize::new(0);

macro_rules! step {
    ($name:expr, $cond:expr) => {
        if $cond {
            println!("[PASS] {}", $name);
        } else {
            println!("[FAIL] {}", $name);
            FAILED.fetch_add(1, Ordering::SeqCst);
        }
    };
}

fn tag_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM tag", [], |r| r.get(0))
        .unwrap()
}

fn job_rows(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM job_queue WHERE kind='tag'", [], |r| {
        r.get(0)
    })
    .unwrap()
}

fn dup_assets(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM (SELECT assetId FROM tag GROUP BY assetId HAVING COUNT(*) > 1)",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

/// 重复的 (assetId, name) 对数（幂等：应为 0）。
fn dup_pairs(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM (SELECT assetId, name FROM tag GROUP BY assetId, name HAVING COUNT(*) > 1)",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

/// tag 表中不同标签名个数（观察 mock 是否按图区分）。
fn distinct_tag_names(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(DISTINCT name) FROM tag", [], |r| r.get(0))
        .unwrap()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n = args
        .get(1)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(200);
    let scene = args.get(2).cloned().unwrap_or_else(|| "all".into());
    println!("== D4 tag queue bench: n={n} scene={scene} ==");

    // 持久目录场景（进程级 kill→重启 验证用）：args[3] 或 env D4_DIR。
    // 例：seed → 后台 run（D4_SLOW_MS=30）→ kill -9 → run 续跑 → stats
    let dir = args
        .get(3)
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var("D4_DIR").ok().map(std::path::PathBuf::from));
    let slow_ms: u64 = std::env::var("D4_SLOW_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if let Some(dir) = &dir {
        std::fs::create_dir_all(dir).unwrap();
        match scene.as_str() {
            "seed" => {
                let conn = setup(dir, n);
                let q = enq_untagged(&conn).unwrap();
                println!("seed: dir={} assets={n} queued={q}", dir.display());
                return;
            }
            "run" => {
                let conn = open_existing(dir);
                let ext = std::env::var("D4_MOCK_URL").ok();
                let owned;
                let base = match &ext {
                    Some(u) => u.as_str(),
                    None => {
                        owned = MockServer::start(counting_ok(&Arc::new(AtomicUsize::new(0))));
                        owned.base_url.as_str()
                    }
                };
                let p = provider(base);
                let cancel = AtomicBool::new(false);
                let mut i = 0usize;
                let report = run_tag_queue(&conn, &p, &cancel, &mut |ev| {
                    std::thread::sleep(std::time::Duration::from_millis(slow_ms));
                    i += 1;
                    if i % 25 == 0 {
                        println!(
                            "run.progress: n={i} asset={} total={}",
                            ev.assetId, ev.total
                        );
                    }
                })
                .unwrap();
                println!(
                    "run: done={} failed={} retry={} cancelled={} jobDone={} jobFailed={} jobPending={}",
                    report.done,
                    report.failed,
                    report.retry,
                    report.cancelled,
                    report.jobDone,
                    report.jobFailed,
                    report.jobPending
                );
                for e in report.errors.iter().take(3) {
                    println!("run.error: {e}");
                }
                return;
            }
            "retryfailed" => {
                let conn = open_existing(dir);
                let n = retry_failed(&conn).unwrap();
                println!("retryfailed: reset={n}");
                return;
            }
            "stats" => {
                let conn = open_existing(dir);
                let (jobs, pending, running, done, failed, tags, dups) = counts(&conn);
                println!(
                    "stats: jobs={jobs} pending={pending} running={running} done={done} failed={failed} tagRows={tags} dupAssets={dups}"
                );
                let mut stmt = conn
                    .prepare("SELECT attempts, COUNT(*) FROM job_queue WHERE kind='tag' GROUP BY attempts ORDER BY attempts")
                    .unwrap();
                let hist: Vec<(i64, i64)> = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .unwrap()
                    .map(|x| x.unwrap())
                    .collect();
                println!("stats.attemptsHistogram: {hist:?}");
                return;
            }
            _ => {}
        }
    }

    match scene.as_str() {
        "full" | "all" => {
            let tmp = tempfile::tempdir().expect("tempdir");
            let conn = setup(tmp.path(), n);
            let counter = Arc::new(AtomicUsize::new(0));
            let srv = MockServer::start(counting_ok(&counter));
            let p = provider(&srv.base_url);
            let cancel = AtomicBool::new(false);
            let t0 = Instant::now();
            let queued = enq_untagged(&conn).unwrap();
            let report = run_tag_queue(&conn, &p, &cancel, &mut |_| {}).unwrap();
            let dt = t0.elapsed().as_millis();
            println!(
                "full: queued={queued} done={} failed={} retry={} skipped={} jobDone={} {dt}ms rate={:.1} img/s",
                report.done,
                report.failed,
                report.retry,
                report.skipped,
                report.jobDone,
                report.done as f64 / dt as f64 * 1000.0
            );
            step!("full.done == n", report.done == n);
            step!("full.failed == 0", report.failed == 0);
            step!("full.tag rows == n", tag_count(&conn) == n as i64);
            step!("full.hits == n", srv.hits() == n);
            step!(
                "full.enq_untagged again == 0",
                enq_untagged(&conn).unwrap() == 0
            );
            let stats = tag_stats(&conn).unwrap();
            step!(
                "full.stats (done,failed,pending,untagged)",
                stats.done == n as i64
                    && stats.failed == 0
                    && stats.pending == 0
                    && stats.untaggedAssets == 0
            );
        }
        _ => {}
    }

    match scene.as_str() {
        "resume" | "all" => {
            let tmp = tempfile::tempdir().expect("tempdir");
            let conn = setup(tmp.path(), n);
            let counter = Arc::new(AtomicUsize::new(0));
            let srv = MockServer::start(counting_ok(&counter));
            let p = provider(&srv.base_url);
            let cancel = AtomicBool::new(false);
            enq_untagged(&conn).unwrap();
            // 模拟崩溃残留：2 个 job 卡 running
            conn.execute(
                "UPDATE job_queue SET status='running' WHERE id IN \
                 (SELECT id FROM job_queue WHERE kind='tag' ORDER BY id LIMIT 2)",
                [],
            )
            .unwrap();
            let report = run_tag_queue(&conn, &p, &cancel, &mut |_| {}).unwrap();
            println!(
                "resume: done={} jobDone={} hits={}",
                report.done,
                report.jobDone,
                srv.hits()
            );
            step!("resume.done == n", report.done == n);
            step!(
                "resume.job rows == n (无重复行)",
                job_rows(&conn) == n as i64
            );
            step!("resume.tag rows == n", tag_count(&conn) == n as i64);
        }
        _ => {}
    }

    match scene.as_str() {
        "cancel" | "all" => {
            let tmp = tempfile::tempdir().expect("tempdir");
            let conn = setup(tmp.path(), n);
            let counter = Arc::new(AtomicUsize::new(0));
            let srv = MockServer::start(counting_ok(&counter));
            let p = provider(&srv.base_url);
            let cancel = AtomicBool::new(false);
            enq_untagged(&conn).unwrap();
            let mut seen = 0usize;
            let report = run_tag_queue(&conn, &p, &cancel, &mut |_| {
                seen += 1;
                if seen >= 2 {
                    cancel.store(true, Ordering::SeqCst);
                }
            })
            .unwrap();
            println!(
                "cancel: cancelled={} done={} seen={seen}",
                report.cancelled, report.done
            );
            step!("cancel.report.cancelled", report.cancelled);
            step!("cancel.done < n", report.done < n);
            let stats = tag_stats(&conn).unwrap();
            step!(
                "cancel.done+pending == n",
                stats.done + stats.pending == n as i64
            );
            // 恢复跑完
            cancel.store(false, Ordering::SeqCst);
            let report2 = run_tag_queue(&conn, &p, &cancel, &mut |_| {}).unwrap();
            let final_stats = tag_stats(&conn).unwrap();
            println!(
                "cancel.resume: done2={} finalDone={} hits={}",
                report2.done,
                final_stats.done,
                srv.hits()
            );
            step!(
                "cancel.resume final done == n",
                final_stats.done == n as i64
            );
            step!("cancel.resume total hits == n", srv.hits() == n);
        }
        _ => {}
    }

    match scene.as_str() {
        "retag" | "all" => {
            let tmp = tempfile::tempdir().expect("tempdir");
            let conn = setup(tmp.path(), n);
            let srv = MockServer::start(hashed_ok());
            let p = provider(&srv.base_url);
            let cancel = AtomicBool::new(false);
            // 第一轮：响应按请求体哈希确定（同图同标签），为幂等对比建立基线
            enq_untagged(&conn).unwrap();
            run_tag_queue(&conn, &p, &cancel, &mut |_| {}).unwrap();
            let tags_before = tag_count(&conn);
            let names_before = distinct_tag_names(&conn);
            // retag：enq_all 全库重跑（换模板/换模型场景），同一确定性上游
            let queued = enq_all(&conn).unwrap();
            let report = run_tag_queue(&conn, &p, &cancel, &mut |_| {}).unwrap();
            println!(
                "retag: queued={queued} done={} tagsBefore={} tagsAfter={} distinctNames={names_before}",
                report.done,
                tags_before,
                tag_count(&conn)
            );
            step!("retag.enq_all == n", queued == n);
            step!(
                "retag.tag rows unchanged（ON CONFLICT 幂等，不追加重复）",
                tag_count(&conn) == tags_before
            );
            step!(
                "retag.job rows == n（job 表清空重建，无重复）",
                job_rows(&conn) == n as i64
            );
            step!("retag.no dup (assetId,name)", dup_pairs(&conn) == 0);
            let titles_ok: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM assets WHERE title_ch IS NULL OR TRIM(title_ch) = ''",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            step!("retag.all assets have title_ch", titles_ok == 0);
        }
        _ => {}
    }

    match scene.as_str() {
        "retry" | "all" => {
            // 独立小库：1 张图 + 恒 500 上游
            let tmp = tempfile::tempdir().expect("tempdir");
            let conn = setup(tmp.path(), 0);
            insert_asset(&conn, tmp.path(), "extra.png");
            let srv = MockServer::start(always_500());
            let p = provider(&srv.base_url);
            let cancel = AtomicBool::new(false);
            let queued = enq_untagged(&conn).unwrap();
            let report = run_tag_queue(&conn, &p, &cancel, &mut |_| {}).unwrap();
            println!(
                "retry: queued={queued} failed={} retry={} hits={}",
                report.failed,
                report.retry,
                srv.hits()
            );
            step!("retry.failed == 1", report.failed == 1);
            step!("retry.retry == 2", report.retry == 2);
            step!("retry.hits == 3（恰好 3 次尝试）", srv.hits() == 3);
            let row: (i64, String) = conn
                .query_row(
                    "SELECT attempts, COALESCE(lastError,'') FROM job_queue \
                     WHERE kind='tag' AND status='failed'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            step!("retry.attempts == 3", row.0 == 3);
            step!("retry.lastError 含 500", row.1.contains("500"));
            step!(
                "retry.retry_failed() == 1",
                retry_failed(&conn).unwrap() == 1
            );
            // 重置后再跑仍 500 → 再 3 连败（可重复触发）
            let report2 = run_tag_queue(&conn, &p, &cancel, &mut |_| {}).unwrap();
            step!("retry.after-reset failed == 1", report2.failed == 1);
            step!("retry.after-reset hits == 6", srv.hits() == 6);
        }
        _ => {}
    }

    let f = FAILED.load(Ordering::SeqCst);
    if f == 0 {
        println!("== ALL PASS ==");
        std::process::exit(0);
    } else {
        println!("== {f} FAILURES ==");
        std::process::exit(1);
    }
}
