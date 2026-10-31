//! OCR-MATCH 端到端（卡 task）：字面检索下推 FTS5 后，`/api/search/text` 的
//! 语义 + 字面统一召回与 `ocrMatched`/`ocrSnippet` 暴露。
//!
//! 为什么这么写：本卡的核心是「OCR 字面证据在**真实 HTTP 检索链路上**被正确召回、
//! 正确标注、不吞不假」。这里用真实的 axum Router（oneshot）+ 真实的 OpenAI 兼容
//! mock 端点（std TCP，无外网、无 GPU）把链路整个跑一遍：
//!   建资产（含 ocr_text 直写 SQL —— 走触发器/自愈路径）→ 嵌入建向量表 →
//!   POST /api/search/text → 断言 hit 的分数、ocrMatched、ocrSnippet、排序、k 截断。
//!
//! 覆盖口径（每项一个断言群）：
//!   ① 语义独有命中：ocrMatched=false（不误标）
//!   ② 语义 + 字面同资产：抬分（弱语义 → 融合分）并标记
//!   ③ 字面独有命中：语义没召回也要补入（此前 `resources` 表名错误 → 静默丢弃）
//!   ④ minScore 语义下限不吞字面证据
//!   ⑤ 中文单字可命中（旧 LIKE 口径 ≥2 字符会漏）
//!   ⑥ 英文按整词（子串不命中）
//!   ⑦ 多词部分命中按比例折扣
//!   ⑧ 软删除资产不进结果
//!   ⑨ 外部 SQL 改写 ocr_text：旧索引行当场失效、下次 open 自愈（不陈旧）
//!   ⑩ 同分截断到 k 且顺序确定

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use tower::ServiceExt;

const DIMS: usize = 16;

/// 5 张「图片」的假字节（mock provider 不解码，只做 data URL 哈希）。
fn payload(i: i64) -> Vec<u8> {
    format!("OCR-MATCH-PAYLOAD-{i}-{}", "z".repeat(i as usize)).into_bytes()
}

/// 资产夹具：ocr_text 直写 SQL（模拟导入/外部写入）→ 触发器标 dirty → open 自愈。
const ASSETS: &[&str] = &[
    "数字资产管理平台\n发票号 A-001 合计 1200 元",
    "Digital Asset Management",
    "", // 无识别文本
    "SKU-8891降价促销",
    "发票 报销单 模板",
];

// ---------- 极简 mock OpenAI 兼容端点（std TCP） ----------

struct Mock {
    base_url: String,
    /// 文本 → 基向量下标（测试按查询词定向指到某个资产的向量）
    text_map: Arc<Mutex<Vec<(String, usize)>>>,
}

impl Mock {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let addr = listener.local_addr().unwrap();
        let text_map: Arc<Mutex<Vec<(String, usize)>>> = Arc::new(Mutex::new(Vec::new()));
        let tm = text_map.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let tm = tm.clone();
                std::thread::spawn(move || {
                    if let Some(body) = read_body(&mut s) {
                        let _ = write_json(&mut s, &respond(&body, &tm));
                    }
                });
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            text_map,
        }
    }

    /// 把查询词指到某个资产的基向量（该资产成为唯一语义命中）。
    fn map_text(&self, text: &str, idx: usize) {
        self.text_map.lock().unwrap().push((text.to_string(), idx));
    }
}

fn respond(body: &str, text_map: &Arc<Mutex<Vec<(String, usize)>>>) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let items: Vec<serde_json::Value> = match v.get("input").cloned().unwrap_or_default() {
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
                fnv1a(url.as_bytes()) % DIMS
            } else {
                let text = item.as_str().unwrap_or("");
                let map = text_map.lock().unwrap();
                // 子串命中优先（"发票 报销" 会命中 "发票" 的映射）
                map.iter()
                    .find(|(k, _)| text.contains(k.as_str()))
                    .map(|(_, idx)| *idx)
                    .unwrap_or(DIMS - 1) // 未映射 → 与所有资产正交（语义噪声）
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

// ---------- 夹具 ----------

struct Fixture {
    dir: std::path::PathBuf,
    app: Router,
    mock: Mock,
    /// (assetId, 文件路径) —— 顺序与 ASSETS 一致
    assets: Vec<(i64, std::path::PathBuf)>,
}

async fn fixture() -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "dam-web-ocrmatch-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let conn = core_storage::open(&dir.join("dam.db")).expect("open db");

    let mut assets = Vec::new();
    for (i, ocr) in ASSETS.iter().enumerate() {
        let id = (i + 1) as i64;
        let bytes = payload(id);
        let p = dir.join(format!("a{id}.png"));
        std::fs::write(&p, &bytes).unwrap();
        conn.execute(
            "INSERT INTO assets (assetId, absPath, fileName, fileExt, fileSz, mtime, importedAt, mediaType, ocr_text)
             VALUES (?1, ?2, ?3, 'png', ?4, 0, ?5, 'image', ?6)",
            rusqlite::params![
                id,
                p.to_string_lossy().to_string(),
                format!("a{id}.png"),
                bytes.len() as i64,
                id,
                ocr
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO resource (resourceId, assetId, role, ordinal) VALUES (?1, ?1, 'primary', 0)",
            rusqlite::params![id],
        )
        .unwrap();
        assets.push((id, p));
    }
    drop(conn);

    let state = app_web::test_support::state(dir.clone(), core_storage::open(&dir.join("dam.db")).unwrap());
    let app = app_web::router_without_dist(state);
    let mock = Mock::start();

    // 配置 mock 端点 → 探针实测维度 → 嵌入 5 张
    let (st, info) = get_json(&app, "/api/settings/inference").await;
    assert_eq!(st, StatusCode::OK);
    let mut settings = info["settings"].clone();
    settings["active_embedder"] = serde_json::json!("openai-compat-embedding");
    settings["embedding"]["base_url"] = serde_json::json!(format!("{}/v1", mock.base_url));
    settings["embedding"]["model"] = serde_json::json!("mock-embed");
    settings["embedding"]["dims"] = serde_json::json!(0);
    settings["embedding"]["proxy"] = serde_json::json!("");
    let (st, body) = put_json(&app, "/api/settings/inference", settings).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, body) = post_json(&app, "/api/settings/embed/test", serde_json::json!({})).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, body) = post_json(&app, "/api/embed/start", serde_json::json!({"mode":"pending"})).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let mut done = false;
    for _ in 0..200 {
        let (_, s) = get_json(&app, "/api/embed/status").await;
        if s["running"] == serde_json::json!(false) && s["report"]["done"] == serde_json::json!(5) {
            done = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(done, "5 张资产应嵌入完成");

    let fx = Fixture {
        dir,
        app,
        mock,
        assets,
    };
    // 语义定向控制：mock 的图片向量 = fnv1a(data URL) % DIMS，5 张可能撞桶（实测 2/5 同桶），
    // 那样「语义只命中某一张」的断言会失真。这里在**真实嵌入跑完之后**把向量改写为
    // 互不相同的基向量 e[i]（只改向量值，不改检索链路），使语义命中可定向断言。
    {
        let conn = fx.conn();
        let model = "openai-compat-embedding:mock-embed";
        for (i, (id, _)) in fx.assets.iter().enumerate() {
            let mut v = vec![0.0f32; DIMS];
            v[i] = 1.0;
            core_storage::vecstore::upsert(&conn, model, &[(*id, &v)]).unwrap();
        }
    }
    fx
}

impl Fixture {
    /// 每个资产在向量表里的基向量下标（用于把查询词指到指定资产）。
    fn vector_idx(&self, asset_id: i64) -> usize {
        let conn = core_storage::open(&self.dir.join("dam.db")).unwrap();
        let rows = core_storage::vecstore::fetch_vectors(
            &conn,
            "openai-compat-embedding:mock-embed",
            &[asset_id],
        )
        .unwrap();
        rows.first()
            .expect("该资产应有向量")
            .1
            .iter()
            .position(|x| *x > 0.5)
            .expect("基向量下标")
    }

    fn conn(&self) -> rusqlite::Connection {
        core_storage::open(&self.dir.join("dam.db")).unwrap()
    }

    async fn search(&self, query: &str, k: usize, min_score: Option<f32>) -> serde_json::Value {
        let mut body = serde_json::json!({ "query": query, "k": k });
        if let Some(m) = min_score {
            body["minScore"] = serde_json::json!(m);
        }
        let (st, v) = post_json(&self.app, "/api/search/text", body).await;
        assert_eq!(st, StatusCode::OK, "search {query}: {v}");
        println!("---- query {:?} → {}", query, serde_json::to_string(&v).unwrap());
        v
    }
}

fn hits_of(v: &serde_json::Value) -> Vec<(i64, f64, bool, Option<String>)> {
    v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            (
                h["assetId"].as_i64().unwrap(),
                h["score"].as_f64().unwrap(),
                h["ocrMatched"].as_bool().unwrap(),
                h["ocrSnippet"].as_str().map(|s| s.to_string()),
            )
        })
        .collect()
}

fn find(hits: &[(i64, f64, bool, Option<String>)], id: i64) -> Option<&(i64, f64, bool, Option<String>)> {
    hits.iter().find(|h| h.0 == id)
}

async fn get_json(app: &Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

async fn post_json(app: &Router, uri: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
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
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

async fn put_json(app: &Router, uri: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let res = app
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
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

// ---------- 用例 ----------

/// ① 语义独有 vs 字面独有 vs 抬分：三条路径的 ocrMatched/分数各就各位。
#[tokio::test]
async fn semantic_only_literal_only_and_boost_are_distinguished() {
    let fx = fixture().await;
    let idx2 = fx.vector_idx(2);
    fx.mock.map_text("管理", idx2); // 语义指到资产 2（Digital Asset Management）
    let v = fx.search("管理", 10, Some(0.0)).await;
    let hits = hits_of(&v);
    // 资产 2：语义满分 1.0，且没有「管理」二字 → 不得标 ocrMatched
    let h2 = find(&hits, 2).expect("语义命中");
    assert!((h2.1 - 1.0).abs() < 1e-4, "{h2:?}");
    assert!(!h2.2, "纯语义命中不得标 ocrMatched");
    assert!(h2.3.is_none());
    // 资产 1：语义噪声 0 + 文本含「管理」→ 抬到融合分并标字面
    let h1 = find(&hits, 1).expect("字面命中资产 1");
    assert!(h1.2, "{h1:?}");
    assert!((h1.1 - 0.35).abs() < 1e-5, "弱语义 → 融合分 0.35：{h1:?}");
    assert_eq!(h1.3.as_deref(), Some("数字资产管理平台"));
    assert_eq!(v["count"].as_i64().unwrap(), hits.len() as i64);
}

/// ② 字面独有命中必须补入（语义一个都没召回）；③ minScore 语义下限不吞字面证据。
#[tokio::test]
async fn literal_only_hit_survives_semantic_threshold() {
    let fx = fixture().await;
    fx.mock.map_text("报销", DIMS - 1); // 语义指向与所有资产正交的方向 → 语义分全 0
    // 默认 minScore（0.08）：语义噪声被滤掉，字面命中仍必须返回
    let v = fx.search("报销", 10, None).await;
    let hits = hits_of(&v);
    assert_eq!(hits.len(), 1, "只应有字面独有命中：{hits:?}");
    let h5 = &hits[0];
    assert_eq!(h5.0, 5);
    assert!(h5.2, "字面独有命中必须标 ocrMatched（此前被静默丢弃）");
    assert!((h5.1 - 0.35).abs() < 1e-5, "{h5:?}");
    assert_eq!(h5.3.as_deref(), Some("发票 报销单 模板"));

    // 显式 minScore=0 也一样（下限不参与字面路径）
    let v = fx.search("报销", 10, Some(0.0)).await;
    assert!(hits_of(&v).iter().any(|h| h.0 == 5 && h.2));
}

/// ④ 中文单字命中（旧口径 ≥2 字符漏），⑤ 英文按整词（子串不命中）。
#[tokio::test]
async fn cjk_single_char_matches_and_latin_is_word_level() {
    let fx = fixture().await;
    let idx4 = fx.vector_idx(4);
    fx.mock.map_text("票", idx4);
    let v = fx.search("票", 10, Some(0.0)).await;
    let hits = hits_of(&v);
    assert!(find(&hits, 4).unwrap().1 > 0.9, "语义命中资产 4");
    assert!(!find(&hits, 4).unwrap().2, "资产 4 无「票」字 → 不标字面");
    // 资产 1/5 含「票」（发票）→ 单字必须命中
    assert!(find(&hits, 1).expect("资产 1").2, "「票」应字面命中资产 1：{hits:?}");
    assert!(find(&hits, 5).expect("资产 5").2, "「票」应字面命中资产 5");
    assert!(find(&hits, 1).unwrap().3.as_deref().unwrap().contains("发票"));

    // 英文整词命中
    fx.mock.map_text("Management", DIMS - 1);
    let v = fx.search("Management", 10, None).await;
    let hits = hits_of(&v);
    assert_eq!(hits.len(), 1, "只有资产 2 含 Management：{hits:?}");
    assert!(hits[0].2 && hits[0].0 == 2);
    // 英文子串不命中（词级口径，响亮声明）
    fx.mock.map_text("anage", DIMS - 1);
    let v = fx.search("anage", 10, None).await;
    assert_eq!(hits_of(&v).len(), 0, "英文子串不应命中");
}

/// ⑥ 多词部分命中按比例折扣；⑦ 同分截断 k 且顺序确定。
#[tokio::test]
async fn partial_multiterm_discount_and_k_truncation() {
    let fx = fixture().await;
    fx.mock.map_text("发票 报销", DIMS - 1);
    let v = fx.search("发票 报销", 10, None).await;
    let hits = hits_of(&v);
    assert_eq!(hits.len(), 2, "只有资产 1（含发票）与 5（含发票+报销）命中：{hits:?}");
    let h5 = find(&hits, 5).expect("两词全命中");
    let h1 = find(&hits, 1).expect("只命中「发票」");
    assert!((h5.1 - 0.35).abs() < 1e-5, "全命中 = 融合分：{h5:?}");
    assert!((h1.1 - 0.35 * 0.6 * 0.5).abs() < 1e-5, "部分命中折扣：{h1:?}");
    assert_eq!(hits[0].0, 5, "全命中排最前");

    // k=1：只留最高分
    let v = fx.search("发票 报销", 1, None).await;
    assert_eq!(
        hits_of(&v).iter().map(|h| h.0).collect::<Vec<_>>(),
        vec![5],
        "截断到 k=1"
    );
    // 同分并列（资产 1/5 都得融合分）：按 assetId 升序截断，结果确定
    let v = fx.search("票", 1, None).await;
    assert_eq!(
        hits_of(&v).iter().map(|h| h.0).collect::<Vec<_>>(),
        vec![1],
        "同分按 assetId 升序"
    );
}

/// ⑧ 软删除资产不进结果（索引行留着但查询侧过滤）。
#[tokio::test]
async fn soft_deleted_asset_is_filtered_out() {
    let fx = fixture().await;
    fx.mock.map_text("报销", DIMS - 1);
    assert_eq!(hits_of(&fx.search("报销", 10, None).await).len(), 1);
    fx.conn()
        .execute("UPDATE assets SET deleted = 1 WHERE assetId = 5", [])
        .unwrap();
    let v = fx.search("报销", 10, None).await;
    assert_eq!(v["count"].as_i64().unwrap(), 0, "软删除后不得返回：{v}");
}

/// ⑨ 外部 SQL 改写 ocr_text：旧索引行当场失效（不陈旧）、下次 open 自愈（不漏召回）。
#[tokio::test]
async fn external_sql_rewrite_has_no_stale_index_and_self_heals() {
    let fx = fixture().await;
    fx.mock.map_text("增值税", DIMS - 1);
    fx.mock.map_text("降价", DIMS - 1);
    // 改写：SKU-8891降价促销 → 发票 增值税（绕过应用写路径）
    fx.conn()
        .execute("UPDATE assets SET ocr_text = '发票 增值税' WHERE assetId = 4", [])
        .unwrap();

    // 旧词不再命中资产 4（触发器已撤旧索引行）
    let v = fx.search("降价", 10, None).await;
    assert!(
        !hits_of(&v).iter().any(|h| h.0 == 4),
        "旧文本的字面命中必须消失：{v}"
    );
    // 新词经 open 自愈后命中资产 4
    let v = fx.search("增值税", 10, None).await;
    let hits = hits_of(&v);
    let h4 = find(&hits, 4).expect("新文本应字面命中资产 4（自愈）");
    assert!(h4.2);
    assert_eq!(h4.3.as_deref(), Some("发票 增值税"));
}

/// ⑩ 空 query 校验不变（回归）；无 ocr_text 资产不产生字面命中。
#[tokio::test]
async fn empty_query_rejected_and_textless_asset_not_literal_matched() {
    let fx = fixture().await;
    let (st, body) = post_json(&fx.app, "/api/search/text", serde_json::json!({"query":"   "})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], serde_json::json!("empty_query"));

    let idx3 = fx.vector_idx(3); // 资产 3 无 ocr_text
    fx.mock.map_text("管理", idx3);
    let v = fx.search("管理", 10, Some(0.0)).await;
    let hits = hits_of(&v);
    assert!(!find(&hits, 3).unwrap().2, "无 ocr_text 的资产不得标字面命中");
    assert!(find(&hits, 3).unwrap().3.is_none());
}
