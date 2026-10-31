//! W3 端到端：设置页 provider 配置 → 嵌入队列建向量 → 文搜图 / 以图搜图（真实 HTTP，mock 端点）。
//!
//! 为什么这么写：W3 的核心是「provider 可插拔 + 向量按模型分表 + 检索编排」三件事在同一个
//! HTTP 进程里串起来。这里用一个真实的 mock OpenAI 兼容端点（std TCP，无外网、无 GPU）替换
//! dam-ai embedder，把链路真跑一遍：
//!   1. GET/PUT /api/settings/inference（web 构建拒绝本地 ONNX spec）
//!   2. POST /api/settings/embed/test（探针实测维度并回写设置）
//!   3. POST /api/embed/start + 轮询 /api/embed/status（队列消费 + 向量落 v_<model> 表）
//!   4. POST /api/search/text、/api/search/similar、/api/search/image（KNN 命中校验）
//!   5. 未建索引时 /api/search/text → 409 no_vector_index（引导文案由前端消费）
//!
//! mock 规则（确定性，便于断言）：
//!   - 图片请求：向量 = 单位基向量 e[fnv1a(data_url) % DIMS]（同一文件字节 → 同一向量）
//!   - 文本请求：向量 = e[文本表[query]]（"ping" 探针固定 e[DIMS-1]），表由测试在嵌入后填

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use tower::ServiceExt;

const DIMS: usize = 16;

// ---------- 极简 mock 端点（std TCP；不用 mock 框架，避免给 workspace 加依赖） ----------

struct Mock {
    base_url: String,
    image_requests: Arc<Mutex<usize>>,
    last_body: Arc<Mutex<String>>,
    /// 文本 → 基向量下标（测试在嵌入后填，用于定向断言）
    text_map: Arc<Mutex<Vec<(String, usize)>>>,
}

impl Mock {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let addr = listener.local_addr().unwrap();
        let image_requests = Arc::new(Mutex::new(0usize));
        let last_body = Arc::new(Mutex::new(String::new()));
        let text_map: Arc<Mutex<Vec<(String, usize)>>> = Arc::new(Mutex::new(Vec::new()));
        let (ir, lb, tm) = (image_requests.clone(), last_body.clone(), text_map.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let (ir, lb, tm) = (ir.clone(), lb.clone(), tm.clone());
                std::thread::spawn(move || {
                    if let Some(body) = read_body(&mut s) {
                        *lb.lock().unwrap() = body.clone();
                        let resp = respond(&body, &ir, &tm);
                        let _ = write_json(&mut s, &resp);
                    }
                });
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            image_requests,
            last_body,
            text_map,
        }
    }

    fn set_text_target(&self, text: &str, idx: usize) {
        self.text_map.lock().unwrap().push((text.to_string(), idx));
    }

    fn image_request_count(&self) -> usize {
        *self.image_requests.lock().unwrap()
    }

    fn last_embed_body(&self) -> String {
        self.last_body.lock().unwrap().clone()
    }
}

fn respond(
    body: &str,
    image_requests: &Arc<Mutex<usize>>,
    text_map: &Arc<Mutex<Vec<(String, usize)>>>,
) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let input = v.get("input").cloned().unwrap_or(serde_json::Value::Null);
    let items: Vec<serde_json::Value> = match input {
        serde_json::Value::Array(a) => a,
        serde_json::Value::String(s) => vec![serde_json::Value::String(s)],
        _ => vec![],
    };
    let data: Vec<serde_json::Value> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let idx = if item.get("type").and_then(|t| t.as_str()) == Some("image_url") {
                let url = item["image_url"]["url"].as_str().unwrap_or("");
                *image_requests.lock().unwrap() += 1;
                fnv1a(url.as_bytes()) % DIMS
            } else {
                let text = item.as_str().unwrap_or("");
                let map = text_map.lock().unwrap();
                map.iter()
                    .find(|(k, _)| text.contains(k.as_str()))
                    .map(|(_, idx)| *idx)
                    .unwrap_or(DIMS - 1)
            };
            let mut vec = vec![0.0f32; DIMS];
            vec[idx] = 1.0;
            serde_json::json!({ "object": "embedding", "index": i, "embedding": vec })
        })
        .collect();
    serde_json::json!({ "object": "list", "model": "mock-embed", "data": data }).to_string()
}

fn fnv1a(bytes: &[u8]) -> usize {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h % DIMS as u64) as usize
}

fn read_body(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let content_length: usize = head
        .lines()
        .find_map(|l| {
            let l = l.to_ascii_lowercase();
            l.strip_prefix("content-length:")
                .map(|v| v.trim().parse().unwrap_or(0))
        })
        .unwrap_or(0);
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    Some(String::from_utf8_lossy(&body).to_string())
}

fn write_json(stream: &mut TcpStream, body: &str) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes())?;
    stream.flush()
}

// ---------- 测试夹具 ----------

struct Fixture {
    dir: std::path::PathBuf,
    app: Router,
    mock: Mock,
    /// (assetId, 文件路径, bytes)
    assets: Vec<(i64, std::path::PathBuf, Vec<u8>)>,
}

async fn fixture() -> Fixture {
    fixture_n(3).await
}

/// 参数化夹具：`n` 张「图片」（BATCH-02 交付条数回归需要比 3 张更多的存活集）。
async fn fixture_n(n: i64) -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "dam-web-w3-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let conn = core_storage::open(&dir.join("dam.db")).expect("open db");

    // n 张「图片」（远程 provider 只读字节做 data URL，不解码 → 内容任意但需互不相同）
    let mut assets = Vec::new();
    for i in 1..=n {
        let bytes = format!("PNG-PAYLOAD-{i}-{}", "x".repeat(i as usize)).into_bytes();
        let p = dir.join(format!("a{i}.png"));
        std::fs::write(&p, &bytes).unwrap();
        conn.execute(
            "INSERT INTO assets (assetId, absPath, fileName, fileExt, fileSz, mtime, importedAt, mediaType)
             VALUES (?1, ?2, ?3, 'png', ?4, 0, ?5, 'image')",
            rusqlite::params![
                i,
                p.to_string_lossy().to_string(),
                format!("a{i}.png"),
                bytes.len() as i64,
                i
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO resource (resourceId, assetId, role, ordinal) VALUES (?1, ?1, 'primary', 0)",
            rusqlite::params![i],
        )
        .unwrap();
        assets.push((i, p, bytes));
    }

    let state = app_web::test_support::state(dir.clone(), conn);
    let app = app_web::router_without_dist(state);
    let mock = Mock::start();
    Fixture {
        dir,
        app,
        mock,
        assets,
    }
}

async fn get(fx: &Fixture, uri: &str) -> (StatusCode, serde_json::Value) {
    let res = fx
        .app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn post_json(
    fx: &Fixture,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let res = fx
        .app
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
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn put_json(
    fx: &Fixture,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let res = fx
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// 把设置指到 mock 端点（走真实 PUT 接口，不写库操纵）。
async fn configure_mock_endpoint(fx: &Fixture) {
    let (st, info) = get(fx, "/api/settings/inference").await;
    assert_eq!(st, StatusCode::OK);
    let mut settings = info["settings"].clone();
    settings["active_embedder"] = serde_json::json!("openai-compat-embedding");
    settings["embedding"]["base_url"] = serde_json::json!(format!("{}/v1", fx.mock.base_url));
    settings["embedding"]["model"] = serde_json::json!("mock-embed");
    settings["embedding"]["dims"] = serde_json::json!(0); // 以端点实测为准
    settings["embedding"]["proxy"] = serde_json::json!(""); // 直连（绕开 env 代理）
    let (st, _) = put_json(fx, "/api/settings/inference", settings).await;
    assert_eq!(st, StatusCode::OK, "保存远程 provider 设置");
}

/// 读出每个 assetId 的向量下标（用于把文本查询指到指定资产）。
fn stored_vector_index(fx: &Fixture, asset_id: i64) -> usize {
    let conn = core_storage::open(&fx.dir.join("dam.db")).unwrap();
    let model = read_active_model(&conn);
    let rows = core_storage::vecstore::fetch_vectors(&conn, &model, &[asset_id]).unwrap();
    let v = rows.first().expect("该资产应有向量").1.clone();
    v.iter().position(|x| *x > 0.5).expect("基向量下标")
}

fn read_active_model(conn: &rusqlite::Connection) -> String {
    let s: core_config::InferenceSettings =
        core_storage::settings::get_or_default(conn, core_config::INFERENCE_SETTINGS_KEY).unwrap();
    app_web::infer::vector_model_key(
        &core_config::load_registry(None)
            .unwrap()
            .by_name(&s.active_embedder)
            .unwrap()
            .clone(),
        &s,
    )
}

/// 配好 mock provider 并把这批资产全部嵌入（新用例共用）。
async fn embed_all(fx: &Fixture) {
    configure_mock_endpoint(fx).await;
    let (st, body) = post_json(fx, "/api/settings/embed/test", serde_json::json!({})).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, body) = post_json(fx, "/api/embed/start", serde_json::json!({"mode":"pending"})).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    for _ in 0..200 {
        let (_, s) = get(fx, "/api/embed/status").await;
        if s["running"] == serde_json::json!(false) && s["report"].is_object() {
            assert_eq!(
                s["report"]["done"],
                serde_json::json!(fx.assets.len()),
                "嵌入未完成：{s}"
            );
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("嵌入超时");
}

/// POST /api/search/image（原始字节 body + query）→ (状态码, JSON)。
async fn post_image(
    fx: &Fixture,
    uri: &str,
    bytes: Vec<u8>,
) -> (StatusCode, serde_json::Value) {
    let res = fx
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "image/png")
                .body(Body::from(bytes))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let raw = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    if raw.is_empty() {
        return (status, serde_json::Value::Null);
    }
    let v = serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null);
    (status, v)
}

// ---------- 用例 ----------

/// 设置接口：web 构建（local-infer off）拒绝本地 ONNX spec；远程 spec 可保存。
#[tokio::test]
async fn settings_reject_local_onnx_and_accept_remote() {
    let fx = fixture().await;
    let (st, info) = get(&fx, "/api/settings/inference").await;
    assert_eq!(st, StatusCode::OK);
    let local_infer = info["capabilities"]["localInfer"].as_bool().unwrap();
    let embedders = info["embedders"].as_array().unwrap();
    assert!(embedders.iter().any(|m| m["provider"] == "onnx_siglip"));
    assert!(embedders.iter().any(|m| m["provider"] == "openai_compat"));

    // local-infer 是编译期事实（web 容器构建关闭：cargo test -p app-web 走关；workspace 全量
    // 测试时被 app-tauri 的特性统一打开）。两种构建下不变量都要成立：
    //   关闭 → onnx spec 必须被拒（不静默降级）；开启 → 允许（桌面语义）。
    let mut onnx = info["settings"].clone();
    onnx["active_embedder"] = serde_json::json!("siglip2-base-p224-onnx");
    let (st, body) = put_json(&fx, "/api/settings/inference", onnx).await;
    if local_infer {
        assert_eq!(
            st,
            StatusCode::OK,
            "local-infer 开启时 onnx spec 可保存：{body}"
        );
    } else {
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], serde_json::json!("local_infer_disabled"));
    }

    let mut missing = info["settings"].clone();
    missing["active_embedder"] = serde_json::json!("openai-compat-embedding");
    missing["embedding"]["base_url"] = serde_json::json!("");
    let (st, body) = put_json(&fx, "/api/settings/inference", missing).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], serde_json::json!("provider_not_configured"));
}

/// 未建索引：搜索返回 409 no_vector_index（前端据此给「先嵌入」引导）。
#[tokio::test]
async fn search_without_index_is_actionable_409() {
    let fx = fixture().await;
    configure_mock_endpoint(&fx).await;
    let (st, body) = post_json(&fx, "/api/search/text", serde_json::json!({"query":"狗"})).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(body["code"], serde_json::json!("no_vector_index"));

    let (st, status) = get(&fx, "/api/search/status").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(status["searchable"], serde_json::json!(false));
    assert_eq!(status["activeRows"], serde_json::json!(0));
}

/// 全链路：测试连接（实测维度）→ 嵌入 3 张 → 文搜图/找相似/上传图搜索命中可核对。
#[tokio::test]
async fn embed_then_text_and_image_search() {
    let fx = fixture().await;
    configure_mock_endpoint(&fx).await;

    // 1. 测试连接：探针实测维度回写设置
    let (st, body) = post_json(&fx, "/api/settings/embed/test", serde_json::json!({})).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["dims"], serde_json::json!(DIMS));
    assert_eq!(body["endpointModel"], serde_json::json!("mock-embed"));
    assert_eq!(
        body["modelKey"],
        serde_json::json!("openai-compat-embedding:mock-embed"),
        "向量标识 = registry spec + endpoint 模型（禁混算）"
    );

    // 2. 增量嵌入 3 张
    let (st, body) = post_json(
        &fx,
        "/api/embed/start",
        serde_json::json!({"mode":"pending"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["queued"], serde_json::json!(3));

    // 3. 轮询到结束（真实后台线程消费队列）
    let mut final_status = serde_json::Value::Null;
    for _ in 0..200 {
        let (_, s) = get(&fx, "/api/embed/status").await;
        if s["running"] == serde_json::json!(false) && s["report"].is_object() {
            final_status = s;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        final_status["report"]["done"],
        serde_json::json!(3),
        "嵌入完成 3 张：{final_status}"
    );
    assert_eq!(final_status["report"]["failed"], serde_json::json!(0));
    assert_eq!(fx.mock.image_request_count(), 3, "每张图一次图片请求");
    assert_eq!(
        final_status["stats"]["semantic"]["vectors"],
        serde_json::json!(3)
    );

    // 向量落 v_<model> 表（禁混算：表名带端点模型）
    let conn = core_storage::open(&fx.dir.join("dam.db")).unwrap();
    let tables = core_storage::vecstore::tables(&conn).unwrap();
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].0, "openai-compat-embedding:mock-embed");
    assert_eq!(tables[0].1, DIMS as u32);

    // 每张图拿到不同基向量（mock 按 data URL 哈希）
    let idxs: Vec<usize> = fx
        .assets
        .iter()
        .map(|(id, _, _)| stored_vector_index(&fx, *id))
        .collect();
    let mut uniq = idxs.clone();
    uniq.sort_unstable();
    uniq.dedup();
    assert_eq!(uniq.len(), 3, "3 张图应得 3 个不同下标：{idxs:?}");

    // 4. 文搜图：把「狗」指到资产 3 的向量 → top1 必须是资产 3（score=1.0）
    //    minScore=0：mock 向量互相正交（cos≈0），默认 0.08 下限会滤掉——这里验证
    //    的是检索管线本身，显式关掉阈值；阈值行为由 svc-library 单测覆盖。
    let target = fx.assets[2].0;
    fx.mock.set_text_target("狗", idxs[2]);
    let (st, body) = post_json(
        &fx,
        "/api/search/text",
        serde_json::json!({"query":"一只狗","k":3,"minScore":0}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["count"], serde_json::json!(3));
    assert_eq!(body["hits"][0]["assetId"], serde_json::json!(target));
    let score = body["hits"][0]["score"].as_f64().unwrap();
    assert!((score - 1.0).abs() < 1e-4, "同向量 cos=1.0，got {score}");
    assert_eq!(
        body["model"],
        serde_json::json!("openai-compat-embedding:mock-embed")
    );
    // 文本请求体形态：query 原样字符串（不做任何改写）
    let last = fx.mock.last_embed_body();
    assert!(last.contains("一只狗"), "文本请求体应含原样 query：{last}");

    // 5. 找相似：资产 2 自身 top1、score≈1
    let (st, body) = post_json(
        &fx,
        "/api/search/similar",
        serde_json::json!({"assetId": fx.assets[1].0, "k": 3}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        body["hits"][0]["assetId"],
        serde_json::json!(fx.assets[1].0)
    );
    assert!((body["hits"][0]["score"].as_f64().unwrap() - 1.0).abs() < 1e-4);
    // W3 相关性下限：mock 模型（openai-compat-embedding:*）未标定 → 后端不过滤，
    // 且响应里把这件事说清楚（前端据此决定是否显示"相似度 ≥ N%"）。
    assert_eq!(body["minScore"], serde_json::json!(0.0), "{body}");
    assert_eq!(body["minScoreCalibrated"], serde_json::json!(false), "{body}");
    assert_eq!(body["count"], serde_json::json!(3), "未标定模型 = 不过滤（全部正交向量都在）");

    // 5b. 显式 minScore 生效：mock 向量两两正交（cos≈0），卡到 0.5 只剩自身（cos=1.0）
    let (st, body) = post_json(
        &fx,
        "/api/search/similar",
        serde_json::json!({"assetId": fx.assets[1].0, "k": 3, "minScore": 0.5}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["count"], serde_json::json!(1), "0.5 下限只留自身：{body}");
    assert_eq!(
        body["hits"][0]["assetId"],
        serde_json::json!(fx.assets[1].0)
    );
    assert_eq!(body["minScore"], serde_json::json!(0.5), "{body}");

    // 5c. 显式 minScore 一律走 query（body 是字节流）：0.5 → 正交命中全被滤掉
    let (_, _, bytes0) = &fx.assets[0];
    let res = fx
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/search/image?minScore=0.5")
                .header("content-type", "image/png")
                .body(Body::from(bytes0.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes_body = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes_body).unwrap();
    // 上传字节与库内 a1 完全一致 → 同一向量（cos=1.0）→ 恰好留下这一条
    assert_eq!(body["count"], serde_json::json!(1), "{body}");
    assert_eq!(
        body["hits"][0]["assetId"],
        serde_json::json!(fx.assets[0].0)
    );
    assert_eq!(body["minScore"], serde_json::json!(0.5));

    // 5d. 上传一张「库里没有」的图（字节末尾追加 1 字节 → mock 哈希不同 → 正交向量）
    //     → 0.5 下限下没有任何命中（这正是修复前"返回一堆无关图"的场景）
    let mut novel = bytes0.clone();
    novel.extend_from_slice(b"\x00");
    let res = fx
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/search/image?minScore=0.5")
                .header("content-type", "image/png")
                .body(Body::from(novel))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let nb = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&nb).unwrap();
    assert_eq!(body["count"], serde_json::json!(0), "库内无同向资产 → 显式空：{body}");

    // 6. 上传图搜索：上传资产 1 的同一份字节 → 同一向量 → top1 = 资产 1
    let (_, _, bytes) = &fx.assets[0];
    let res = fx
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/search/image")
                .header("content-type", "image/png")
                .body(Body::from(bytes.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        body["hits"][0]["assetId"],
        serde_json::json!(fx.assets[0].0),
        "上传同一字节 → 同一向量：{body}"
    );

    // 7. 空 query / 空 body 的输入校验
    let (st, body) = post_json(&fx, "/api/search/text", serde_json::json!({"query":"   "})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], serde_json::json!("empty_query"));
}

/// AUDIT-B2 §1 的回归：`/api/search/image`（上传图路径）此前把 k / 标量过滤静默丢掉
/// （只接 minScore），前端选条数、填扩展名都不生效。这里逐项钉死：
///   k 生效、过滤生效（含正向控制）、minScore 生效、非法入参 400（不静默 clamp/忽略）。
#[tokio::test]
async fn by_image_honours_k_filter_and_min_score() {
    let fx = fixture().await;
    embed_all(&fx).await;
    let (_, _, bytes0) = &fx.assets[0];
    let b = bytes0.clone();

    // a. 缺省：k=DEFAULT_K(24)、模型未标定 → 不过滤（3 张全在）
    let (st, body) = post_image(&fx, "/api/search/image", b.clone()).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["count"], serde_json::json!(3), "{body}");
    assert_eq!(body["k"], serde_json::json!(24), "{body}");
    println!("[EVIDENCE] default       -> {body}");

    // b. k 生效（修复前恒为 24）
    let (st, body) = post_image(&fx, "/api/search/image?k=1", b.clone()).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["count"], serde_json::json!(1), "k=1 只回 1 条：{body}");
    assert_eq!(body["k"], serde_json::json!(1), "{body}");
    println!("[EVIDENCE] k=1           -> {body}");

    // c. 标量过滤生效：库里全是 png → ext=jpg 必须 0 条（修复前是 3 条 = 过滤被丢）
    let (st, body) = post_image(&fx, "/api/search/image?ext=jpg", b.clone()).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["count"], serde_json::json!(0), "ext=jpg 不该命中 png 资产：{body}");
    println!("[EVIDENCE] ext=jpg       -> {body}");
    // 正向控制：ext=png 仍全部命中（别把过滤做成「一律清空」）
    let (st, body) = post_image(&fx, "/api/search/image?ext=png", b.clone()).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["count"], serde_json::json!(3), "ext=png 应全中：{body}");
    println!("[EVIDENCE] ext=png       -> {body}");
    // 目录前缀同样生效
    let (st, body) = post_image(&fx, "/api/search/image?dirPrefix=/definitely-nowhere", b.clone()).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["count"], serde_json::json!(0), "dirPrefix 不匹配应 0 条：{body}");

    // d. minScore 生效：同字节上传 → 与 a1 同向量（cos=1.0），0.99 阈值只剩它
    let (st, body) = post_image(&fx, "/api/search/image?minScore=0.99", b.clone()).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["count"], serde_json::json!(1), "minScore=0.99 只留自匹配：{body}");
    let score = body["hits"][0]["score"].as_f64().unwrap();
    assert!(score >= 0.99, "score={score}");
    assert_eq!(body["hits"][0]["assetId"], serde_json::json!(fx.assets[0].0));
    println!("[EVIDENCE] minScore=0.99 -> {body}");

    // e. k + 过滤组合：k=5 & ext=jpg → 仍 0（过滤先于条数）
    let (st, body) = post_image(&fx, "/api/search/image?k=5&ext=jpg", b.clone()).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["count"], serde_json::json!(0), "{body}");

    // f. 非法入参一律 400（不静默 clamp / 不忽略）
    for (uri, code) in [
        ("/api/search/image?k=0", "bad_k"),
        ("/api/search/image?k=999", "bad_k"),
        ("/api/search/image?minScore=2", "bad_min_score"),
        ("/api/search/image?minScore=-1", "bad_min_score"),
    ] {
        let (st, body) = post_image(&fx, uri, b.clone()).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{uri} 应 400：{body}");
        assert_eq!(body["code"], serde_json::json!(code), "{uri}：{body}");
        println!("[EVIDENCE] reject {uri} -> {body}");
    }
    // 非数字 → axum 提取层直接 400（不是 500 / 不是静默用默认值）
    let (st, _) = post_image(&fx, "/api/search/image?minScore=abc", b.clone()).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "minScore=abc 应 400");

    // g. similar() 同一套校验（避免两条路径行为分叉）
    let (st, body) = post_json(
        &fx,
        "/api/search/similar",
        serde_json::json!({"assetId": fx.assets[0].0, "k": 0}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], serde_json::json!("bad_k"), "{body}");
}

/// 交付条数回归（BATCH-02 / 卡 task）：软删进回收站**不动向量**（恢复要用），
/// 无过滤搜索若直接全表 KNN，top-k 名额会被回收站向量占掉 → 交付条数悄悄缩水
/// （真机症状：24 张库 k=24 只出 12、k=5 只出 2）。
///
/// 这条在 HTTP 全链路上钉死 k 的语义 = **交付条数**：回收站不得吃掉名额，恢复后即刻回归。
#[tokio::test]
async fn text_search_delivers_k_past_trashed_vectors() {
    let fx = fixture_n(6).await;
    embed_all(&fx).await;

    // 构造相似度梯度（mock 图片向量是正交基向量，造不出梯度）：a1..a3 与查询最近
    // （0.99/0.97/0.95）→ 随后软删进回收站；a4..a6 存活（0.80/0.70/0.60）。
    let conn = core_storage::open(&fx.dir.join("dam.db")).unwrap();
    let model = read_active_model(&conn);
    for (i, c) in [0.99f32, 0.97, 0.95, 0.80, 0.70, 0.60].iter().enumerate() {
        let rid = fx.assets[i].0;
        let mut v = vec![0.0f32; DIMS];
        v[0] = *c;
        v[1] = (1.0 - c * c).max(0.0).sqrt();
        core_storage::vecstore::upsert(&conn, &model, &[(rid, v.as_slice())]).unwrap();
    }
    // 查询文本 → 基向量 e0（与上面构造的梯度同一平面）
    fx.mock.set_text_target("repro", 0);

    // 1) 3 张最近的行进回收站（走真实 HTTP 路由，与前端「删除→撤销」同一套语义）
    let (st, body) = post_json(
        &fx,
        "/api/library/delete",
        serde_json::json!({"ids":[fx.assets[0].0, fx.assets[1].0, fx.assets[2].0]}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], serde_json::json!(3));

    // 2) k=3：修复前 top-3 全被回收站向量占满 → count=0；正确 = 交付 3 条存活命中
    let (st, body) = post_json(
        &fx,
        "/api/search/text",
        serde_json::json!({"query":"repro","k":3,"minScore":0}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["count"], serde_json::json!(3), "k=3 必须交付 3 条：{body}");
    let ids: Vec<i64> = body["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["assetId"].as_i64().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![fx.assets[3].0, fx.assets[4].0, fx.assets[5].0],
        "按相似度降序交付存活集，回收站不占名额"
    );

    // 3) k=2 → 恰好 2 条（交付条数紧随 k）
    let (st, body) = post_json(
        &fx,
        "/api/search/text",
        serde_json::json!({"query":"repro","k":2,"minScore":0}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["count"], serde_json::json!(2), "{body}");

    // 4) k=6（> 存活数）→ 只交付存在的 3 条存活向量，回收站的绝不补位
    let (st, body) = post_json(
        &fx,
        "/api/search/text",
        serde_json::json!({"query":"repro","k":6,"minScore":0}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["count"], serde_json::json!(3), "k 大于存活数时交付全部存活：{body}");

    // 5) 以图搜图的同一入口（找相似）也按交付条数收口
    let (st, body) = post_json(
        &fx,
        "/api/search/similar",
        serde_json::json!({"assetId": fx.assets[3].0, "k": 3, "minScore": 0}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["count"], serde_json::json!(3), "找相似同样不得被回收站吃掉：{body}");
    assert_eq!(body["hits"][0]["assetId"], serde_json::json!(fx.assets[3].0), "自身 top1");

    // 5b) 卡体验收表里的原始入口 `POST /api/search/image`（上传字节 → 图片向量 → KNN）：
    //     无过滤与带过滤（ext=png 命中全部 6 行）都只交付存活 3 条，回收站不补位。
    let (_, _, bytes) = &fx.assets[3];
    for uri in [
        "/api/search/image?k=6&minScore=0",
        "/api/search/image?k=6&minScore=0&ext=png",
    ] {
        let res = fx
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "image/png")
                    .body(Body::from(bytes.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{uri}");
        let b = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(body["count"], serde_json::json!(3), "{uri}: {body}");
        let trashed = [
            fx.assets[0].0,
            fx.assets[1].0,
            fx.assets[2].0,
        ];
        assert!(
            body["hits"]
                .as_array()
                .unwrap()
                .iter()
                .all(|h| !trashed.contains(&h["assetId"].as_i64().unwrap())),
            "{uri}: 回收站资产不得出现：{body}"
        );
    }

    // 6) 撤销删除 → 回收站资产（向量一直在）立即回到结果里
    let (st, body) = post_json(
        &fx,
        "/api/library/restore",
        serde_json::json!({"ids":[fx.assets[0].0, fx.assets[1].0, fx.assets[2].0]}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["restored"], serde_json::json!(3));
    let (st, body) = post_json(
        &fx,
        "/api/search/text",
        serde_json::json!({"query":"repro","k":6,"minScore":0}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["count"], serde_json::json!(6), "恢复后 6 条全回来：{body}");
    assert_eq!(body["hits"][0]["assetId"], serde_json::json!(fx.assets[0].0), "最近的那张回到首位");
}

// ---------- KI-5：找相似的「精准匹配」模式（mode=visual_precise）----------
//
// 判重向量在生产由 KI-2a 在入库时落 `dedup_vectors`；这里直接写库 —— 精准模式只**读**判重塔
// （不算），所以测试不需要本地 ONNX，web 构建也能覆盖整条 HTTP 路径。

/// 造一个与 e0 余弦恰好等于 `cos` 的 16 维单位向量。
fn vec_with_cos(cos: f32) -> Vec<f32> {
    let mut v = vec![0.0f32; DIMS];
    v[0] = cos;
    v[1] = (1.0 - cos * cos).max(0.0).sqrt();
    v
}

/// 落判重向量（rid = assetId，夹具里 resource.resourceId 与 assetId 同值）。
fn seed_dedup(fx: &Fixture, rows: &[(i64, f32)]) {
    let conn = core_storage::open(&fx.dir.join("dam.db")).unwrap();
    core_storage::vecstore::ensure_table(&conn, core_config::DEDUP_EMBEDDER, DIMS as u32).unwrap();
    let owned: Vec<(i64, Vec<f32>)> = rows.iter().map(|(id, c)| (*id, vec_with_cos(*c))).collect();
    let refs: Vec<(i64, &[f32])> = owned.iter().map(|(id, v)| (*id, v.as_slice())).collect();
    core_storage::vecstore::upsert(&conn, core_config::DEDUP_EMBEDDER, &refs).unwrap();
}

/// 精准匹配：按判重塔 cos 排序 + 0.90/0.98 分带 + 自身剔除 + 缺向量显式计数。
#[tokio::test]
async fn similar_precise_mode_reranks_by_dedup_tower() {
    let fx = fixture_n(5).await;
    embed_all(&fx).await;
    // a1(查询)=1.00 / a2=0.99(near) / a3=0.95(variant) / a4=0.50(截掉) / a5=无向量(missing)
    seed_dedup(&fx, &[(1, 1.0), (2, 0.99), (3, 0.95), (4, 0.50)]);

    let (st, body) = post_json(
        &fx,
        "/api/search/similar",
        serde_json::json!({"assetId": 1, "k": 10, "mode": "visual_precise"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["mode"], serde_json::json!("visual_precise"));
    assert_eq!(body["count"], serde_json::json!(2), "只留 ≥0.90 的两条：{body}");
    assert_eq!(body["candidatePool"], serde_json::json!(4), "候选池=自身之外的语义召回：{body}");
    assert_eq!(body["missingDedupVectors"], serde_json::json!(1), "a5 无判重向量必须计数");
    assert_eq!(body["queryMissingDedupVector"], serde_json::json!(false));
    assert_eq!(body["dedupIndexReady"], serde_json::json!(true));
    assert_eq!(body["selfExcluded"], serde_json::json!(true));
    assert!((body["nearThreshold"].as_f64().unwrap() - 0.98).abs() < 1e-6);
    assert!((body["variantThreshold"].as_f64().unwrap() - 0.90).abs() < 1e-6);

    let hits = body["hits"].as_array().unwrap();
    assert_eq!(hits[0]["assetId"], serde_json::json!(2));
    assert_eq!(hits[0]["band"], serde_json::json!("near_duplicate"));
    assert!((hits[0]["score"].as_f64().unwrap() - 0.99).abs() < 1e-3);
    assert_eq!(hits[1]["assetId"], serde_json::json!(3));
    assert_eq!(hits[1]["band"], serde_json::json!("variant"));
    assert!((hits[1]["score"].as_f64().unwrap() - 0.95).abs() < 1e-3);
    // 自身不进结果、0.50 的 a4 被截掉
    assert!(hits.iter().all(|h| h["assetId"] != serde_json::json!(1)));
    assert!(hits.iter().all(|h| h["assetId"] != serde_json::json!(4)));
    // 语义分随行回带（诊断用）
    assert!(hits[0]["semanticScore"].is_number());
}

/// 零回归：不传 mode = 语义相似，行为与 W3 完全一致（含自身、回带 minScore）。
#[tokio::test]
async fn similar_default_mode_is_unchanged_semantic() {
    let fx = fixture_n(5).await;
    embed_all(&fx).await;
    seed_dedup(&fx, &[(1, 1.0), (2, 0.99), (3, 0.95), (4, 0.50)]);
    let (st, body) = post_json(
        &fx,
        "/api/search/similar",
        serde_json::json!({"assetId": 1, "k": 10, "minScore": 0}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(body.get("mode").is_none(), "语义模式不回 mode：{body}");
    assert!(body["minScore"].is_number());
    let ids: Vec<i64> = body["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["assetId"].as_i64().unwrap())
        .collect();
    assert!(ids.contains(&1), "语义模式含自身（旧行为）：{ids:?}");
    assert_eq!(ids.len(), 5, "minScore=0 → 不过滤，5 条全出：{ids:?}");
}

/// 精准模式的分带写死；带 minScore 进来直接 400（不静默忽略）。
#[tokio::test]
async fn similar_precise_rejects_min_score() {
    let fx = fixture_n(2).await;
    embed_all(&fx).await;
    let (st, body) = post_json(
        &fx,
        "/api/search/similar",
        serde_json::json!({"assetId": 1, "k": 5, "mode": "visual_precise", "minScore": 0.9}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], serde_json::json!("bad_min_score"));
    let msg = body["error"].as_str().unwrap();
    assert!(msg.contains("90%") && msg.contains("98%"), "文案要点明两档：{msg}");
}

/// 全库没算过判重向量（KI-2a 之前的历史库）：200 + 空结果 + 三项显式标记，不是报错。
#[tokio::test]
async fn similar_precise_without_dedup_vectors_is_empty_not_error() {
    let fx = fixture_n(3).await;
    embed_all(&fx).await;
    let (st, body) = post_json(
        &fx,
        "/api/search/similar",
        serde_json::json!({"assetId": 1, "k": 10, "mode": "visual_precise"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "缺判重向量不许报错：{body}");
    assert_eq!(body["count"], serde_json::json!(0));
    assert_eq!(body["dedupIndexReady"], serde_json::json!(false));
    assert_eq!(body["queryMissingDedupVector"], serde_json::json!(true));
    assert_eq!(
        body["missingDedupVectors"], body["candidatePool"],
        "候选全部缺向量 → 计数必须等于候选数：{body}"
    );
    assert_eq!(body["candidatePool"], serde_json::json!(2));
}
