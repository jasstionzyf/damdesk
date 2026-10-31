//! TEST-JOURNEY — 离线产品旅程集成测试（L1 档，卡 task）。
//!
//! 为什么要这一套：单元/分段测试各自绿，但「空库 → 导入 → 管线 → 合规 → 筛选 → 导出 →
//! 回收站 → 恢复」这条**用户真实路径**没有一条端到端用例。这里把整条链装进
//! `dev-e2e`（L3 harness，不进安装包），用临时 SQLite 库 + 固定种子图 + 假 provider，
//! **不联网、不依赖真模型权重**，可重复跑、失败能定位到阶段名。
//!
//! 五条链路（每条独立 `#[test]`、独立断言、各自的库与目录）：
//! 1. [`journey_1_happy_path`]        正常链路：5 张种子图 → 管线 → 合规三态 → 筛选 → 导出 → 回收站 → 恢复
//! 2. [`journey_2_partial_import`]    部分失败：坏图进失败清单 + 不静默当成功 + 只重跑失败项
//! 3. [`journey_3_interrupt_resume`]  中断续跑：崩溃残留 running → 重跑收敛，无重复/无丢项/无残留
//! 4. [`journey_4_concurrent_same_asset`] 并发冲突：同图同时进两个任务 → 各写一次，不可处理项显式 skipped
//! 5. [`journey_5_irreversible_purge`]    不可逆：彻底删 → 行/向量/缩略图全不可查，再恢复必须失败
//!
//! 离线边界（显式标注，不静默跳过）：
//! - 打标/自定义打标 → 进程内 HTTP mock（`mod mock`），响应体是固定 JSON；
//! - 嵌入 → `FakeEmbedder`（确定性 8 维向量，取路径/文本 hash）；
//! - OCR → 容器内无 PP-OCRv6 权重 → 断言 `ProviderNotReady` **显式报错**且 job 回 pending
//!   （「缺模型的降级路径必须响亮」，不是静默跳过）；
//! - EXIF → 真解析（无模型依赖），种子图无 EXIF 也算一次有效结论（`done`）。
//!
//! 跑法：
//! - `cargo test -p dev-e2e --test journey -- --nocapture`
//! - 或 `bash scripts/journey_acceptance.sh`（L1 门禁档，含三次重复跑与耗时汇总）

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use core_config::Pack;
use core_types::{ComplianceStateFilter, ImportFilter, TagRunReport};
use engine_tag::TagProvider;
use rusqlite::Connection;
use svc_library::compliance_view::{asset_ids_by_state, states_summary, AssetComplianceState};
use svc_library::export::{
    run_export, ExportContent, ExportSource, ExportSpec, ExportTarget,
};
use svc_pipeline::compliance_queue::{compliance_enq, compliance_enq_group, run_compliance_queue};
use svc_pipeline::EmbedScope;

// ===========================================================================
// 阶段断言：失败信息必须以阶段名开头（卡验收 #2：能定位到阶段名）
// ===========================================================================

/// 阶段断言：`st!("S2 管线", cond, "明细 {}", x)` —— 失败时 panic 带阶段名。
macro_rules! st {
    ($stage:expr, $cond:expr, $($arg:tt)*) => {
        if !$cond {
            panic!("[{}] {}", $stage, format!($($arg)*));
        }
    };
}

/// 阶段开始标记（`--nocapture` 时可见，便于对齐耗时与失败位置）。
macro_rules! stage {
    ($stage:expr) => {{
        eprintln!("▶ {}", $stage);
    }};
}

// ===========================================================================
// 环境：临时库 + 种子图目录 + 缩略图根
// ===========================================================================

struct Env {
    dir: PathBuf,
    src: PathBuf,
    thumbs: PathBuf,
    conn: Connection,
}

impl Env {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("dam-journey-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let src = dir.join("src");
        let thumbs = dir.join("thumbs");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&thumbs).unwrap();
        // core_storage::open = 真迁移 + FK ON + sqlite-vec 注册（与桌面/服务端同一入口）
        let conn = core_storage::open(&dir.join("journey.db")).unwrap();
        Self {
            dir,
            src,
            thumbs,
            conn,
        }
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self.src.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    /// 跑一次真导入（scan → 解码校验 → 入库 → 缩略图 → pHash）。
    fn import(&self) -> (i64, core_types::BatchOutcome) {
        let n = std::fs::read_dir(&self.src).unwrap().count() as i64;
        let bid = svc_import::create_batch(&self.conn, &self.src, "reference", n).unwrap();
        let out = svc_import::run_batch(&self.conn, bid, None, &self.thumbs, |_p| {
            Ok::<(), String>(())
        })
        .unwrap();
        (bid, out)
    }

    /// 全库可见资产 id（升序）
    fn ids(&self) -> Vec<i64> {
        let mut v: Vec<i64> = svc_library::list_assets(&self.conn, &ImportFilter::default())
            .unwrap()
            .iter()
            .map(|a| a.asset.assetId)
            .collect();
        v.sort();
        v
    }

    fn id_of(&self, file_name: &str) -> i64 {
        self.conn
            .query_row(
                "SELECT assetId FROM assets WHERE fileName = ?1",
                [file_name],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn count(&self, sql: &str) -> i64 {
        self.conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ===========================================================================
// 种子图（纯 Rust 编码，确定性字节；白边/红边用于合规判定）
// ===========================================================================

fn encode_png(img: &image::RgbImage) -> Vec<u8> {
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgb8(img.clone())
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .unwrap();
    buf
}

/// 白底合格图：四周 8px 近白边 + 中心深色块（white_edge 通过、对比够强）。
fn white_bg_png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        let edge = x < 8 || y < 8 || x + 8 >= w || y + 8 >= h;
        if edge {
            image::Rgb([250, 250, 250])
        } else {
            image::Rgb([40, 40, 44])
        }
    });
    encode_png(&img)
}

/// 红边不合格图：边缘大偏差（white_edge 必然 fail）。
fn red_bg_png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        let edge = x < 8 || y < 8 || x + 8 >= w || y + 8 >= h;
        if edge {
            image::Rgb([200, 10, 10])
        } else {
            image::Rgb([90, 90, 90])
        }
    });
    encode_png(&img)
}

/// 白底场景图：白边 + 位置/形状各异的深色块（variant 区分 → pHash 互不相同，
/// 组级 phash_dup 判「组内无重复」的正面样本；边缘仍近白 → white_edge 通过）。
fn scene_png(w: u32, h: u32, variant: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        let edge = x < 8 || y < 8 || x + 8 >= w || y + 8 >= h;
        if edge {
            return image::Rgb([250, 250, 250]);
        }
        // 深色块的左边界与宽度随 variant 变化（四分位错位）
        let bx = w / 4 * (variant % 4);
        let bw = (w / 3).max(16);
        let inset = 16;
        if x >= bx + inset && x < bx + inset + bw && y >= h / 4 && y < h / 4 + (h / 2).max(16) {
            image::Rgb([30 + (variant as u8) * 20, 30, 40])
        } else {
            image::Rgb([250, 250, 250])
        }
    });
    encode_png(&img)
}

/// 高频条纹图（与白边/色块图 pHash 相距很远，用于「组内不重复」）。
fn stripes_png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, _y| {
        if (x / 4) % 2 == 0 {
            image::Rgb([250, 250, 250])
        } else {
            image::Rgb([20, 20, 20])
        }
    });
    encode_png(&img)
}

fn broken_bytes() -> Vec<u8> {
    b"not an image at all".to_vec()
}

// ===========================================================================
// 假 provider（离线）：mock HTTP 打标 + 确定性嵌入
// ===========================================================================

/// 极简 HTTP mock：一次 accept 一个连接，responder 收到 (path, body) → (status, body)。
/// 只依赖 std（与 svc-pipeline/engine-tag 各自的测试工具同源，不引 mock 框架）。
mod mock {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    pub struct MockServer {
        pub base_url: String,
        hits: Arc<AtomicUsize>,
    }

    impl MockServer {
        pub fn start<F>(responder: F) -> Self
        where
            F: Fn(&str) -> (u16, String) + Send + Sync + 'static,
        {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
            let addr = listener.local_addr().unwrap();
            let hits = Arc::new(AtomicUsize::new(0));
            let h2 = hits.clone();
            let responder = Arc::new(responder);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let h2 = h2.clone();
                    let responder = responder.clone();
                    std::thread::spawn(move || {
                        if let Some((_method, _path, body)) = read_request(&mut stream) {
                            h2.fetch_add(1, Ordering::SeqCst);
                            let (status, resp) = responder(&body);
                            let _ = write_response(&mut stream, status, &resp);
                        }
                    });
                }
            });
            Self {
                base_url: format!("http://{addr}"),
                hits,
            }
        }

        pub fn count(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }

    fn read_request(stream: &mut TcpStream) -> Option<(String, String, String)> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let header_end = loop {
            let n = stream.read(&mut tmp).ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or_default().to_string();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let path = parts.next().unwrap_or_default().to_string();
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
        Some((method, path, String::from_utf8_lossy(&body).to_string()))
    }

    fn write_response(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
        let resp = format!(
            "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.as_bytes().len()
        );
        stream.write_all(resp.as_bytes())?;
        stream.flush()
    }
}

/// 三合一打标应答（第 idx 张图 → title/tag 唯一，避免断言依赖顺序）。
fn tag_body(idx: usize) -> String {
    format!(
        r#"{{"choices":[{{"message":{{"content":"{{\"title_ch\":\"图{idx}\",\"title_en\":\"img{idx}\",\"description\":\"desc{idx}\",\"tags\":[{{\"name\":\"t{idx}\",\"score\":0.9}}]}}"}}}}]}}"#
    )
}

/// 打标 provider（真 HTTP 客户端 → mock）。proxy = Some("") 强制直连（容器可带 env 代理）。
fn tag_provider(base_url: &str) -> engine_tag::openai_compat::OpenAiCompatTagProvider {
    let spec = core_config::load_registry(None)
        .unwrap()
        .models
        .into_iter()
        .find(|m| m.kind == core_config::SpecKind::Tagging)
        .expect("registry 里的 tagging spec");
    let cfg = core_config::TagSettings {
        enabled: true,
        base_url: format!("{base_url}/v1"),
        api_key: "test".into(),
        model: "mock-vl".into(),
        proxy: Some(String::new()),
        timeout_ms: 10_000,
        ..Default::default()
    };
    let mut p = engine_tag::openai_compat::OpenAiCompatTagProvider::new(spec, cfg);
    p.load().unwrap();
    p
}

/// 确定性假嵌入（8 维，L2 归一）：图像取路径 hash、文本取文本 hash。
/// 只为「向量真的落表 / 能召回」这条链路服务，语义无关（文字命中走字面融合通道）。
struct FakeEmbedder {
    spec: core_config::ModelSpec,
}

impl FakeEmbedder {
    const MODEL: &'static str = "journey-fake";
    const DIMS: u32 = 8;

    fn new() -> Self {
        Self {
            spec: core_config::ModelSpec {
                name: Self::MODEL.to_string(),
                kind: core_config::SpecKind::Embedding,
                dims: Self::DIMS,
                modalities: vec!["image".into(), "text".into()],
                preprocess: serde_yaml::Value::Null,
                normalized: true,
                provider: "fake".into(),
            },
        }
    }

    fn vec_of(seed: &str) -> Vec<f32> {
        let mut h: u64 = 1469598103934665603;
        for b in seed.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        let v: Vec<f32> = (0..Self::DIMS as usize)
            .map(|i| (((h >> (i * 5)) & 0xff) as f32 / 255.0) - 0.5 + 0.01)
            .collect();
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.into_iter().map(|x| x / n).collect()
    }
}

impl engine_embed::EmbedProvider for FakeEmbedder {
    fn load(&mut self) -> engine_embed::Result<()> {
        Ok(())
    }
    fn embed_images(&self, paths: &[PathBuf]) -> engine_embed::Result<Vec<Vec<f32>>> {
        Ok(paths
            .iter()
            .map(|p| Self::vec_of(&p.display().to_string()))
            .collect())
    }
    fn embed_texts(&self, texts: &[&str]) -> engine_embed::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| Self::vec_of(t)).collect())
    }
    fn ready(&self) -> bool {
        true
    }
    fn spec(&self) -> &core_config::ModelSpec {
        &self.spec
    }
}

// ===========================================================================
// 合规包（classic-only，无模型依赖）
// ===========================================================================

const TAXONOMY: &str = "category: [连衣裙]\n";
const REPORT: &str = "summary: [合格, 不合格, 未判定]\ndetail_columns: [imageSN, R1_size, R2_white, R5_soft, G1_dup]\n";

/// 主包：尺寸（元数据档）+ 白底（像素档）+ 软规则（fail 不改结论）+ 组级重复（per_group）。
const RULES_MAIN: &str = r#"
- id: R1_size
  name: 尺寸达标
  executor: classic
  check: {detector: size_min, params: {min: 700}}
  fixer: null
  on_fail: fail
- id: R2_white
  name: 白底纯净
  executor: classic
  check: {detector: white_edge, params: {max_dev: 8}}
  fixer: null
  on_fail: fail
- id: R5_soft
  name: 软规则（fail 不影响合格结论）
  executor: classic
  check: {detector: white_edge, params: {max_dev: 8}}
  fixer: null
  on_fail: pass
- id: G1_dup
  name: 组内重复
  executor: classic
  judgement_scope: per_group
  group_by: {source: folder}
  check: {detector: phash_dup, params: {threshold: 8}, confidence: {source: fixed, value: 0.9}}
  fixer: null
  on_fail: pass
"#;
const PIPELINE_MAIN: &str = r#"
steps:
  - {kind: judge, rule: R1_size}
  - {kind: judge, rule: R2_white}
  - {kind: judge, rule: R5_soft}
  - {kind: judge, rule: G1_dup}
recheck_max: 1
"#;

/// 隔离包：唯一规则用不存在的检测器 → 装载期 lint 判 rule_error →
/// 该图落 `undetermined`（判定过程出错，不是不合格结论）。用来覆盖三态里的「未判定」。
const RULES_ISOLATE: &str = r#"
- id: X1_bad
  name: 坏规则隔离
  executor: classic
  check: {detector: no_such_detector, params: {}}
  fixer: null
  on_fail: fail
"#;
const PIPELINE_ISOLATE: &str = r#"
steps:
  - kind: judge
    rule: X1_bad
recheck_max: 0
"#;

fn main_pack() -> Pack {
    Pack::from_yaml_strs("journeyA", TAXONOMY, RULES_MAIN, PIPELINE_MAIN, REPORT).unwrap()
}

fn isolate_pack() -> Pack {
    Pack::from_yaml_strs("journeyB", TAXONOMY, RULES_ISOLATE, PIPELINE_ISOLATE, REPORT).unwrap()
}

// ===========================================================================
// 流程 1：正常链路
// ===========================================================================

#[test]
fn journey_1_happy_path() {
    let env = Env::new("happy");
    let t0 = std::time::Instant::now();

    // ---------- S1 导入：5 张种子图（横/竖/方/坏/超大），坏图进失败清单 ----------
    stage!("S1 导入");
    env.write("wide.png", &scene_png(1200, 800, 1));
    env.write("tall.png", &scene_png(800, 1200, 2));
    env.write("square.png", &red_bg_png(900, 900));
    env.write("bad.jpg", &broken_bytes());
    env.write("huge.png", &scene_png(4200, 2800, 3));
    let (_bid, out) = env.import();
    st!("S1 导入", out.total == 5, "扫描到 5 个文件，实际 {}", out.total);
    st!("S1 导入", out.imported == 4, "4 张入库（坏图解不出），实际 {}", out.imported);
    st!("S1 导入", out.failed == 1, "坏图必须计入失败，实际 {}", out.failed);
    st!(
        "S1 导入",
        out.failedItems.len() == 1 && out.failedItems[0].fileName == "bad.jpg",
        "失败明细必须是 bad.jpg，实际 {:?}",
        out.failedItems
    );
    st!(
        "S1 导入",
        !out.failedItems[0].error.is_empty(),
        "失败原因不许为空（任务中心要显示为什么失败）"
    );
    let ids = env.ids();
    st!("S1 导入", ids.len() == 4, "库内可见资产 4 条，实际 {}", ids.len());

    // 缩略图两档都落盘（回收站/导出/彻底删都依赖这一事实）
    let square_id = env.id_of("square.png");
    for size in core_media::THUMB_SIZES {
        let tp = env.thumbs.join(size.to_string()).join(format!("{:02}", square_id % 100)).join(format!("{square_id}_{size}.webp"));
        st!("S1 导入", tp.is_file(), "缩略图缺失 {tp:?}");
    }

    // ---------- S2 管线：打标 / 嵌入 / EXIF / OCR（缺模型的降级必须显式） ----------
    stage!("S2 管线");
    let counter = Arc::new(AtomicUsize::new(0));
    let c2 = counter.clone();
    let srv = mock::MockServer::start(move |_body| {
        let i = c2.fetch_add(1, Ordering::SeqCst);
        (200, tag_body(i))
    });
    let provider = tag_provider(&srv.base_url);
    let cancel = AtomicBool::new(false);
    let queued = svc_pipeline::enq_untagged(&env.conn).unwrap();
    st!("S2 管线", queued == 4, "4 张未打标图入队，实际 {queued}");
    let tag_report = svc_pipeline::run_tag_queue(&env.conn, &provider, &cancel, &mut |_p| {}).unwrap();
    st!("S2 管线", tag_report.done == 4, "4 张打标完成，实际 {:?}", tag_report);
    st!("S2 管线", tag_report.failed == 0, "打标无失败，实际 {:?}", tag_report.errors);
    st!(
        "S2 管线",
        env.count("SELECT COUNT(*) FROM tag WHERE origin='auto'") == 4,
        "自动标签 4 行（每图 1 个），实际 {}",
        env.count("SELECT COUNT(*) FROM tag WHERE origin='auto'")
    );
    st!(
        "S2 管线",
        env.count("SELECT COUNT(*) FROM assets WHERE deleted=0 AND COALESCE(title_ch,'')<>''") == 4,
        "4 张都有中文标题（title_ch 落库）"
    );

    let embedder = FakeEmbedder::new();
    let eq = svc_pipeline::enq_unembedded(&env.conn, FakeEmbedder::MODEL, EmbedScope::Semantic).unwrap();
    st!("S2 管线", eq == 4, "4 张未嵌入图入队，实际 {eq}");
    let emb = svc_pipeline::run_embed_queue(
        &env.conn,
        &embedder,
        EmbedScope::Semantic,
        &cancel,
        &mut |_p| {},
    )
    .unwrap();
    st!("S2 管线", emb.done == 4, "4 张嵌入完成，实际 {:?}", emb);
    let vecs = core_storage::vecstore::count(&env.conn, FakeEmbedder::MODEL).unwrap();
    st!("S2 管线", vecs == 4, "向量表 4 行，实际 {vecs}");
    st!(
        "S2 管线",
        env.count("SELECT COUNT(*) FROM assets WHERE emb_status='done'") == 4,
        "4 张 emb_status=done"
    );

    svc_pipeline::exif_enqueue_missing(&env.conn, None, None).unwrap();
    let exif = svc_pipeline::run_exif_queue(&env.conn, &cancel, &mut |_a, _b, _c, _d| {}).unwrap();
    st!("S2 管线", exif.failed == 0, "EXIF 解析无失败，实际 {:?}", exif.errors);
    st!(
        "S2 管线",
        env.count("SELECT COUNT(*) FROM job_queue WHERE kind='exif' AND status IN ('pending','running')") == 0,
        "EXIF 队列无残留（pending/running 必须归零）"
    );

    // OCR：两种环境都要「不静默跳过」——
    //   · 容器内没有 PP-OCRv6 权重 → 必须显式 ProviderNotReady，且 job 回 pending 不丢；
    //   · 权重就位（dam-dev 把 /data 挂到 /models）→ 正常跑完，4 个 job 全部落 done。
    // 唯一不许出现的是「返回 Ok 却一个没处理」（静默跳过）。
    svc_pipeline::ocr_enq_untagged(&env.conn).unwrap();
    match svc_pipeline::run_ocr_queue(&env.conn, &cancel, &mut |_p| {}) {
        Err(svc_pipeline::PipelineError::ProviderNotReady(m)) => {
            eprintln!("   skip: OCR 引擎未就绪（{m}）—— 显式报错，不是静默跳过");
            st!(
                "S2 管线",
                env.count("SELECT COUNT(*) FROM job_queue WHERE kind='ocr' AND status='pending'") == 4,
                "OCR 未就绪时 4 个 job 必须留在 pending（不许丢任务）"
            );
        }
        Ok(rep) => {
            eprintln!("   OCR 引擎就绪：processed={} done={}", rep.processed, rep.done);
            st!(
                "S2 管线",
                rep.processed == 4 && rep.done == 4 && rep.failed == 0,
                "OCR 引擎就绪时 4 个 job 必须全部处理完成，实际 {:?}",
                rep
            );
            st!(
                "S2 管线",
                env.count("SELECT COUNT(*) FROM job_queue WHERE kind='ocr' AND status IN ('pending','running')") == 0,
                "OCR 队列无残留（pending/running 必须归零）"
            );
        }
        other => panic!("[S2 管线] OCR 只允许「显式 ProviderNotReady」或「全部完成」，实际 {other:?}"),
    }

    // ---------- S3 合规：三态 + 资产级/组级分族 ----------
    stage!("S3 合规");
    let pack = main_pack();
    let group_jobs = compliance_enq_group(&env.conn, &pack, &ids, None).unwrap();
    st!("S3 合规", group_jobs >= 1, "组级 job 至少 1 个（folder 作用域），实际 {group_jobs}");
    let asset_jobs = compliance_enq(&env.conn, &ids, pack.id.as_str(), None).unwrap();
    st!("S3 合规", asset_jobs == 4, "资产级 job 4 个，实际 {asset_jobs}");
    let rep = run_compliance_queue(&env.conn, &pack, &cancel, &mut |_p| {}).unwrap();
    st!("S3 合规", rep.done == 4, "4 张判定完成，实际 {:?}", rep);
    st!("S3 合规", rep.failed == 0, "判定无 job 失败，实际 {:?}", rep.errors);

    let summary = states_summary(&env.conn, &pack.id).unwrap();
    st!(
        "S3 合规",
        summary.pass == 3 && summary.fail == 1 && summary.undetermined == 0 && summary.unjudged == 0,
        "主包三态分桶应为 pass3/fail1/undet0/unjudged0，实际 {summary:?}"
    );
    let failed_ids = asset_ids_by_state(&env.conn, &pack.id, AssetComplianceState::Fail).unwrap();
    st!(
        "S3 合规",
        failed_ids == vec![env.id_of("square.png")],
        "不合格的必须是红边方图（white_edge fail，尺寸仍达标），实际 {failed_ids:?}"
    );

    // 未判定：隔离包（不存在的检测器 → 装载期 rule_error → undetermined）
    let iso = isolate_pack();
    let x = env.id_of("tall.png");
    compliance_enq(&env.conn, &[x], iso.id.as_str(), None).unwrap();
    run_compliance_queue(&env.conn, &iso, &cancel, &mut |_p| {}).unwrap();
    let iso_summary = states_summary(&env.conn, &iso.id).unwrap();
    st!(
        "S3 合规",
        iso_summary.undetermined == 1,
        "坏规则必须落 undetermined（未判定），实际 {iso_summary:?}"
    );
    st!(
        "S3 合规",
        env.count("SELECT COUNT(*) FROM rule_verdict WHERE packId='journeyB' AND evidence LIKE '%load_error%'") == 1,
        "undetermined 行必须带 load_error 证据（不许造结论）"
    );

    // 分族：组级规则 per_group + 组键 = 目录；资产级规则 per_asset
    let g_rows = env.count(
        "SELECT COUNT(*) FROM rule_verdict WHERE packId='journeyA' AND ruleId='G1_dup' AND judgementScope='per_group'",
    );
    st!("S3 合规", g_rows == 4, "组级判定按成员展开 = 4 行，实际 {g_rows}");
    let gkey: String = env
        .conn
        .query_row(
            "SELECT groupKey FROM rule_verdict
             WHERE packId='journeyA' AND ruleId='G1_dup' AND judgementScope='per_group' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    st!(
        "S3 合规",
        gkey == env.src.to_string_lossy(),
        "组键必须是源目录，实际 {gkey:?}"
    );
    let a_rows = env.count(
        "SELECT COUNT(*) FROM rule_verdict
         WHERE packId='journeyA' AND ruleId IN ('R1_size','R2_white','R5_soft')
           AND judgementScope='per_asset'",
    );
    st!("S3 合规", a_rows == 12, "资产级判定 = 4 图 × 3 规则 = 12 行，实际 {a_rows}");

    // ---------- S4 查询：标签 / 评分 / 目录 / 合规状态 / 字面检索 ----------
    stage!("S4 查询");
    let by_tag = |tag: &str| {
        svc_library::list_assets(
            &env.conn,
            &ImportFilter {
                tag: Some(tag.to_string()),
                ..Default::default()
            },
        )
        .unwrap()
        .len()
    };
    let mut tag_total = 0usize;
    for t in ["t0", "t1", "t2", "t3"] {
        let n = by_tag(t);
        st!("S4 查询", n == 1, "标签 {t} 应命中 1 张，实际 {n}");
        tag_total += n;
    }
    st!("S4 查询", tag_total == 4, "四个标签各命中 1 张、合计 4，实际 {tag_total}");

    let one = ids[0];
    svc_library::set_rating(&env.conn, &[one], 5).unwrap();
    let rated = svc_library::list_assets(
        &env.conn,
        &ImportFilter {
            minRating: Some(4),
            ..Default::default()
        },
    )
    .unwrap();
    st!(
        "S4 查询",
        rated.len() == 1 && rated[0].asset.assetId == one && rated[0].asset.starRating == 5,
        "评分筛选 4★+ 应只命中刚打 5★ 的那张，实际 {:?}",
        rated.iter().map(|a| a.asset.assetId).collect::<Vec<_>>()
    );

    let in_dir = svc_library::list_assets(
        &env.conn,
        &ImportFilter {
            dirPrefix: Some(env.src.to_string_lossy().into_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    st!("S4 查询", in_dir.len() == 4, "目录筛选命中 4 张，实际 {}", in_dir.len());

    let pass_n = svc_library::list_assets(
        &env.conn,
        &ImportFilter {
            compliance: Some(ComplianceStateFilter {
                packId: pack.id.clone(),
                state: "pass".into(),
            }),
            ..Default::default()
        },
    )
    .unwrap();
    st!("S4 查询", pass_n.len() == 3, "合规 pass 筛选 3 张，实际 {}", pass_n.len());
    let fail_n = svc_library::list_assets(
        &env.conn,
        &ImportFilter {
            compliance: Some(ComplianceStateFilter {
                packId: pack.id.clone(),
                state: "fail".into(),
            }),
            ..Default::default()
        },
    )
    .unwrap();
    st!("S4 查询", fail_n.len() == 1, "合规 fail 筛选 1 张，实际 {}", fail_n.len());

    // 字面通道检索：语义下限置 0.99（假向量不参与），命中只能来自字面证据
    let hits = svc_library::search_by_text(
        &env.conn,
        &embedder,
        "t1",
        10,
        &engine_search::SearchFilter::default(),
        &svc_library::TextSearchOptions {
            min_score: Some(0.99),
        },
    )
    .unwrap();
    let expect_hit = env.id_of("square.png");
    st!(
        "S4 查询",
        hits.len() == 1 && hits[0].assetId == expect_hit,
        "字面检索「t1」只命中被打上 t1 的那张（{expect_hit}），实际 {hits:?}"
    );

    // ---------- S5 导出：目录 + 清单 + zip + 取消不残留 ----------
    stage!("S5 导出");
    let out_dir = env.dir.join("export");
    let spec = ExportSpec {
        source: ExportSource::Filter(ImportFilter::default()),
        content: ExportContent::Original,
        target: ExportTarget::Dir(out_dir.clone()),
        manifest: true,
        report: None,
        strip_gps: false,
    };
    let rep = run_export(&env.conn, &spec, &env.thumbs, &AtomicBool::new(false), &mut |_p| {}).unwrap();
    st!("S5 导出", rep.total == 4, "导出选择集 4 张，实际 {}", rep.total);
    st!("S5 导出", rep.exported == 4, "实际写出 4 个文件，实际 {}", rep.exported);
    st!("S5 导出", rep.errors.is_empty(), "导出无错误，实际 {:?}", rep.errors);
    st!("S5 导出", rep.manifest_written, "清单必须写出");
    let files: Vec<String> = std::fs::read_dir(&out_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    st!("S5 导出", files.len() == 5, "导出目录 = 4 原图 + manifest.csv，实际 {files:?}");
    let manifest = std::fs::read_to_string(out_dir.join("manifest.csv")).unwrap();
    let rows = manifest.lines().filter(|l| !l.trim().is_empty()).count();
    st!("S5 导出", rows == 6, "清单 = 表头 + 4 行 + excludedRejected 行，实际 {rows} 行");
    st!("S5 导出", manifest.lines().last() == Some("excludedRejected,\"0\""),
        "KI-15：清单末行恒记拒选排除数（本旅程无拒选=0）");
    for name in ["wide.png", "tall.png", "square.png", "huge.png"] {
        st!("S5 导出", files.iter().any(|f| f == name), "{name} 未出现在导出产物里：{files:?}");
        let bytes = std::fs::read(out_dir.join(name)).unwrap();
        let src = std::fs::read(env.src.join(name)).unwrap();
        st!("S5 导出", bytes == src, "{name} 导出字节必须与源一致（原图档不许重编码）");
    }

    let zip_path = env.dir.join("bundle.zip");
    let spec_zip = ExportSpec {
        source: ExportSource::Ids(env.ids()),
        content: ExportContent::Original,
        target: ExportTarget::Zip(zip_path.clone()),
        manifest: false,
        report: None,
        strip_gps: false,
    };
    let zrep = run_export(&env.conn, &spec_zip, &env.thumbs, &AtomicBool::new(false), &mut |_p| {}).unwrap();
    st!("S5 导出", zrep.exported == 4, "zip 内 4 个文件，实际 {}", zrep.exported);
    st!("S5 导出", zip_path.is_file(), "zip 产物必须存在 {zip_path:?}");
    st!(
        "S5 导出",
        std::fs::metadata(&zip_path).unwrap().len() > 0,
        "zip 不许是空文件"
    );

    let cancel_dir = env.dir.join("export_cancelled");
    let spec_cancel = ExportSpec {
        source: ExportSource::Ids(env.ids()),
        content: ExportContent::Original,
        target: ExportTarget::Dir(cancel_dir.clone()),
        manifest: true,
        report: None,
        strip_gps: false,
    };
    let crep = run_export(
        &env.conn,
        &spec_cancel,
        &env.thumbs,
        &AtomicBool::new(true), // 开跑前就取消
        &mut |_p| {},
    )
    .unwrap();
    st!("S5 导出", crep.cancelled, "取消旗置位必须回报 cancelled");
    st!("S5 导出", crep.exported == 0, "取消后不许有产物，实际 {}", crep.exported);
    let leftovers: Vec<String> = std::fs::read_dir(&cancel_dir)
        .map(|it| it.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    st!("S5 导出", leftovers.is_empty(), "取消路径不许残留文件：{leftovers:?}");

    // ---------- S6 回收站 → 恢复 ----------
    stage!("S6 回收站");
    let victim = env.id_of("wide.png");
    st!(
        "S6 回收站",
        svc_library::soft_delete_assets(&env.conn, &[victim]).unwrap() == 1,
        "软删 1 行"
    );
    st!("S6 回收站", env.ids().len() == 3, "删除后图库可见 3 张");
    st!(
        "S6 回收站",
        svc_library::list_deleted(&env.conn, 50).unwrap().iter().any(|a| a.asset.assetId == victim),
        "回收站必须能看到被删的图"
    );
    // 软删不是物理删：文件的库行/标签都还在（恢复才能原样回来）
    let tag_rows_hidden: i64 = env
        .conn
        .query_row("SELECT COUNT(*) FROM tag WHERE assetId = ?1", [victim], |r| r.get(0))
        .unwrap();
    st!(
        "S6 回收站",
        tag_rows_hidden >= 1,
        "软删只改 deleted 标记，标签行必须保留（否则恢复回不到原状），实际 {tag_rows_hidden}"
    );
    let restored = svc_library::restore_assets_checked(&env.conn, &[victim]).unwrap();
    st!(
        "S6 回收站",
        restored.restored == vec![victim] && restored.not_in_trash.is_empty(),
        "恢复必须逐条回报 restored，实际 {:?}",
        restored
    );
    st!("S6 回收站", env.ids().len() == 4, "恢复后图库回到 4 张");
    st!(
        "S6 回收站",
        svc_library::list_deleted(&env.conn, 50).unwrap().is_empty(),
        "恢复后回收站必须清空"
    );

    eprintln!(
        "JOURNEY_1_SUMMARY {{\"phases\":6,\"elapsed_ms\":{}}}",
        t0.elapsed().as_millis()
    );
}

// ===========================================================================
// 流程 2：部分失败（坏图可见 + 只重跑失败项）
// ===========================================================================

#[test]
fn journey_2_partial_import() {
    let t0 = std::time::Instant::now();
    let env = Env::new("partial");

    stage!("P1 部分失败导入");
    env.write("ok1.png", &white_bg_png(1000, 700));
    env.write("ok2.png", &stripes_png(800, 800));
    env.write("broken.jpg", &broken_bytes());
    let (bid, out) = env.import();
    st!("P1 部分失败导入", out.total == 3, "3 个文件，实际 {}", out.total);
    st!("P1 部分失败导入", out.imported == 2, "2 张入库，实际 {}", out.imported);
    st!("P1 部分失败导入", out.failed == 1, "1 张失败，实际 {}", out.failed);

    stage!("P2 失败清单可见");
    let batch = svc_import::get_batch(&env.conn, bid).unwrap();
    // 批次记账口径（既有产品语义）：失败的批次仍记 done，失败由 failed/failedItems 表达，
    // 任务中心按 `status == 'done' && failed > 0` 呈现「部分失败」。这里断言的是
    // **不许吞失败**，不是另一种状态词 —— 若哪天把 status 改成 partial，此断言会提醒同步 UI。
    st!(
        "P2 失败清单可见",
        batch.doneFiles == batch.totalFiles,
        "进度必须走到头（{} / {}），否则任务中心卡在跑",
        batch.doneFiles,
        batch.totalFiles
    );
    st!(
        "P2 失败清单可见",
        out.failedItems.len() == 1 && out.failedItems[0].fileName == "broken.jpg",
        "失败清单只含 broken.jpg，实际 {:?}",
        out.failedItems
    );
    st!(
        "P2 失败清单可见",
        out.failedItems[0].path.ends_with("broken.jpg") && !out.failedItems[0].error.is_empty(),
        "失败项必须带绝对路径与原因：{:?}",
        out.failedItems[0]
    );
    st!(
        "P2 失败清单可见",
        env.count("SELECT COUNT(*) FROM assets WHERE fileName='broken.jpg'") == 0,
        "坏图不许留假资产行（D3：先解码再入库）"
    );

    stage!("P3 只重跑失败项");
    // 修好坏文件 → 只重跑这一个路径
    let fixed = env.write("broken.jpg", &white_bg_png(900, 600));
    let retry = svc_import::retry_failed(&env.conn, bid, &[fixed], None, &env.thumbs, |_p| {
        Ok::<(), String>(())
    })
    .unwrap();
    st!("P3 只重跑失败项", retry.total == 1, "只处理 1 个文件，实际 {}", retry.total);
    st!("P3 只重跑失败项", retry.imported == 1, "重跑后入库 1 张，实际 {}", retry.imported);
    st!("P3 只重跑失败项", retry.failed == 0, "重跑无失败，实际 {:?}", retry.failedItems);
    st!("P3 只重跑失败项", env.ids().len() == 3, "库内 3 张（无重复行）");
    st!(
        "P3 只重跑失败项",
        env.count("SELECT COUNT(*) FROM assets WHERE fileName='broken.jpg'") == 1,
        "重跑成功只落 1 行（幂等）"
    );
    // 越界路径整批拒绝（不许把任意路径塞进库）
    let outside = env.dir.join("outside.jpg");
    std::fs::write(&outside, white_bg_png(64, 64)).unwrap();
    let bad = svc_import::retry_failed(&env.conn, bid, &[outside], None, &env.thumbs, |_p| {
        Ok::<(), String>(())
    });
    st!(
        "P3 只重跑失败项",
        bad.is_err(),
        "批次根目录之外的路径必须整批拒绝（实际未报错）"
    );

    eprintln!(
        "JOURNEY_2_SUMMARY {{\"phases\":3,\"elapsed_ms\":{}}}",
        t0.elapsed().as_millis()
    );
}

// ===========================================================================
// 流程 3：中断 → 续跑（无重复、无丢项、无残留 running）
// ===========================================================================

#[test]
fn journey_3_interrupt_resume() {
    let t0 = std::time::Instant::now();
    let env = Env::new("resume");

    stage!("R1 崩溃现场");
    for i in 0..4 {
        env.write(&format!("r{i}.png"), &white_bg_png(1000, 700 + i as u32 * 10));
    }
    env.import();
    let ids = env.ids();
    st!("R1 崩溃现场", ids.len() == 4, "4 张入库，实际 {}", ids.len());
    let queued = svc_pipeline::enq_untagged(&env.conn).unwrap();
    st!("R1 崩溃现场", queued == 4, "4 个打标 job，实际 {queued}");

    // 模拟「跑到一半进程被杀」：两条 job 处于 running（attempts 已计），
    // 且第一条已经写了一半产物（tag 行 t0 —— 续跑必须 upsert 而不是再插一行）。
    env.conn
        .execute(
            "UPDATE job_queue SET status='running', attempts=1
             WHERE id IN (SELECT id FROM job_queue WHERE kind='tag' ORDER BY id LIMIT 2)",
            [],
        )
        .unwrap();
    env.conn
        .execute(
            "INSERT INTO tag (assetId, value, origin, score, createdAt)
             VALUES (?1, 't0', 'auto', 0.9, unixepoch())",
            [ids[0]],
        )
        .unwrap();
    // 嵌入侧同样留现场：一条 job running + 一条向量已写（重嵌入必须覆盖不叠加）
    svc_pipeline::enq_unembedded(&env.conn, FakeEmbedder::MODEL, EmbedScope::Semantic).unwrap();
    env.conn
        .execute(
            "UPDATE job_queue SET status='running' WHERE kind LIKE 'embed%' AND id = (SELECT MIN(id) FROM job_queue WHERE kind LIKE 'embed%')",
            [],
        )
        .unwrap();
    let rid0: i64 = env
        .conn
        .query_row(
            "SELECT resourceId FROM resource WHERE assetId = ?1 AND role='primary'",
            [ids[0]],
            |r| r.get(0),
        )
        .unwrap();
    let v = FakeEmbedder::vec_of("pre-existing");
    core_storage::vecstore::ensure_table(&env.conn, FakeEmbedder::MODEL, FakeEmbedder::DIMS).unwrap();
    core_storage::vecstore::upsert(&env.conn, FakeEmbedder::MODEL, &[(rid0, v.as_slice())]).unwrap();

    let running_tag = env.count("SELECT COUNT(*) FROM job_queue WHERE kind='tag' AND status='running'");
    let running_embed = env.count("SELECT COUNT(*) FROM job_queue WHERE kind LIKE 'embed%' AND status='running'");
    st!("R1 崩溃现场", running_tag == 2, "打标侧应留 2 条 running，实际 {running_tag}");
    st!("R1 崩溃现场", running_embed == 1, "嵌入侧应留 1 条 running，实际 {running_embed}");

    stage!("R2 续跑");
    let counter = Arc::new(AtomicUsize::new(0));
    let c2 = counter.clone();
    let srv = mock::MockServer::start(move |_b| {
        let i = c2.fetch_add(1, Ordering::SeqCst);
        (200, tag_body(i))
    });
    let provider = tag_provider(&srv.base_url);
    let cancel = AtomicBool::new(false);
    let report: TagRunReport =
        svc_pipeline::run_tag_queue(&env.conn, &provider, &cancel, &mut |_p| {}).unwrap();
    st!("R2 续跑", report.done == 4, "续跑把 4 张全部做完，实际 {:?}", report);
    st!("R2 续跑", report.failed == 0, "续跑无失败，实际 {:?}", report.errors);
    st!("R2 续跑", report.skipped == 0, "续跑不许丢项（skipped 必须为 0）");
    st!("R2 续跑", srv.count() == 4, "每张图只调用一次 = 4 次，实际 {}", srv.count());

    let embedder = FakeEmbedder::new();
    let emb = svc_pipeline::run_embed_queue(
        &env.conn,
        &embedder,
        EmbedScope::Semantic,
        &cancel,
        &mut |_p| {},
    )
    .unwrap();
    st!("R2 续跑", emb.done == 4, "嵌入续跑 4 张，实际 {:?}", emb);
    let vecs = core_storage::vecstore::count(&env.conn, FakeEmbedder::MODEL).unwrap();
    st!("R2 续跑", vecs == 4, "向量 4 行（已写的覆盖、不叠加），实际 {vecs}");

    stage!("R3 无重复 / 无残留");
    let auto_rows = env.count("SELECT COUNT(*) FROM tag WHERE origin='auto'");
    st!("R3 无重复 / 无残留", auto_rows == 4, "自动标签仍是 4 行（崩溃前那行被 upsert），实际 {auto_rows}");
    st!(
        "R3 无重复 / 无残留",
        env.count("SELECT COUNT(*) FROM assets WHERE COALESCE(title_ch,'')<>''") == 4,
        "4 张都有标题（没有丢项）"
    );
    st!(
        "R3 无重复 / 无残留",
        env.count("SELECT COUNT(*) FROM job_queue WHERE status IN ('pending','running')") == 0,
        "队列必须收敛：无 pending / running 残留"
    );
    st!(
        "R3 无重复 / 无残留",
        env.count("SELECT COUNT(*) FROM job_queue WHERE status='failed'") == 0,
        "无终态失败 job"
    );

    eprintln!(
        "JOURNEY_3_SUMMARY {{\"phases\":3,\"elapsed_ms\":{}}}",
        t0.elapsed().as_millis()
    );
}

// ===========================================================================
// 流程 4：同一 asset 同时进两个任务（只写一次 + 显式 skipped）
// ===========================================================================

#[test]
fn journey_4_concurrent_same_asset() {
    let t0 = std::time::Instant::now();
    let env = Env::new("concurrent");

    stage!("C1 双任务入队");
    env.write("c1.png", &white_bg_png(1000, 700));
    env.write("c2.png", &stripes_png(820, 820));
    env.import();
    let ids = env.ids();
    st!("C1 双任务入队", ids.len() == 2, "2 张入库，实际 {}", ids.len());
    let t_material =
        core_storage::custom_tasks::create_task(&env.conn, "材质", "这张图的材质是什么", true).unwrap();
    let t_color =
        core_storage::custom_tasks::create_task(&env.conn, "颜色", "这张图的主色是什么", true).unwrap();
    let n1 = core_storage::custom_tasks::enqueue_for_asset(&env.conn, ids[0]).unwrap();
    st!("C1 双任务入队", n1 == 2, "同一资产 × 2 个启用任务 → 2 个 job，实际 {n1}");
    // 重复入队必须幂等（NOT EXISTS：同 kind + target + payload 且非 done）
    let n2 = core_storage::custom_tasks::enqueue_for_asset(&env.conn, ids[0]).unwrap();
    st!("C1 双任务入队", n2 == 0, "重复入队必须 0 新增，实际 {n2}");
    st!(
        "C1 双任务入队",
        env.count("SELECT COUNT(*) FROM job_queue WHERE kind='custom_tag'") == 2,
        "队列里恰好 2 个自定义任务 job"
    );

    // 同一资产再进「普通打标」队列：两条链路同时在队列里（并发冲突的真实形态）
    svc_pipeline::enq_untagged(&env.conn).unwrap();

    stage!("C2 只写一次");
    let srv = mock::MockServer::start(move |body| {
        if body.contains("材质") {
            (200, r#"{"choices":[{"message":{"content":"{\"value\":\"丝绒\"}"}}]}"#.to_string())
        } else if body.contains("主色") {
            (200, r#"{"choices":[{"message":{"content":"{\"value\":\"红色\"}"}}]}"#.to_string())
        } else {
            (200, tag_body(0))
        }
    });
    let provider = tag_provider(&srv.base_url);
    let cancel = AtomicBool::new(false);

    let custom = svc_pipeline::run_custom_queue(&env.conn, &provider, &cancel, &mut |_p| {}).unwrap();
    st!("C2 只写一次", custom.done == 2, "两个任务各完成一次，实际 {:?}", custom);
    st!("C2 只写一次", custom.failed == 0, "无推理失败，实际 {:?}", custom.errors);
    let rows_material = env.count("SELECT COUNT(*) FROM tag_result WHERE name='材质'");
    let rows_color = env.count("SELECT COUNT(*) FROM tag_result WHERE name='颜色'");
    st!(
        "C2 只写一次",
        rows_material == 1,
        "材质维度 1 条结果行，实际 {rows_material}"
    );
    st!(
        "C2 只写一次",
        rows_color == 1,
        "颜色维度 1 条结果行，实际 {rows_color}"
    );
    // 结果值扇出成标签值（统一标签：key-val 拍平，tag 只存拍平后的值）
    st!(
        "C2 只写一次",
        env.count("SELECT COUNT(*) FROM tag WHERE value='材质-丝绒' AND origin='custom'") == 1
            && env.count("SELECT COUNT(*) FROM tag WHERE value='颜色-红色' AND origin='custom'") == 1,
        "两个维度的输出各扇出 1 个 key-val 标签（材质-丝绒/颜色-红色）"
    );
    let task_ids: Vec<i64> = {
        let mut stmt = env
            .conn
            .prepare("SELECT DISTINCT taskId FROM tag_result ORDER BY taskId")
            .unwrap();
        let v = stmt
            .query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        v
    };
    st!(
        "C2 只写一次",
        task_ids == vec![t_material, t_color],
        "两行分别归属两个任务（同图不同任务各一份），实际 {task_ids:?}"
    );
    // 重跑同一任务：幂等（upsert 不新增行）
    core_storage::custom_tasks::enqueue_task_all(&env.conn, t_material).unwrap();
    svc_pipeline::run_custom_queue(&env.conn, &provider, &cancel, &mut |_p| {}).unwrap();
    let target_rows: i64 = env
        .conn
        .query_row(
            "SELECT COUNT(*) FROM tag WHERE assetId = ?1 AND origin='custom'",
            [ids[0]],
            |r| r.get(0),
        )
        .unwrap();
    st!(
        "C2 只写一次",
        target_rows == 2,
        "重跑后目标图的自定义标签仍是 2 行（同图同值不重复插），实际 {target_rows}"
    );
    st!(
        "C2 只写一次",
        env.count("SELECT COUNT(*) FROM (SELECT assetId, value FROM tag WHERE origin='custom' GROUP BY assetId, value HAVING COUNT(*) > 1)") == 0,
        "任何 (资产, 标签值) 都不许出现重复行"
    );
    st!(
        "C2 只写一次",
        env.count("SELECT COUNT(*) FROM (SELECT assetId, taskId FROM tag_result GROUP BY assetId, taskId HAVING COUNT(*) > 1)") == 0,
        "任何 (资产, 任务) 只许有一条结果行"
    );

    stage!("C3 不可处理项显式 skipped");
    // 普通打标队列：把第二张图先软删 → 该 job 必须显式 skipped（不是 done 也不是 failed）
    svc_library::soft_delete_assets(&env.conn, &[ids[1]]).unwrap();
    let tag = svc_pipeline::run_tag_queue(&env.conn, &provider, &cancel, &mut |p| {
        if p.status == "skipped" {
            eprintln!("   progress: asset {} skipped（显式）", p.assetId);
        }
    })
    .unwrap();
    st!(
        "C3 不可处理项显式 skipped",
        tag.skipped == 1,
        "已删资产的 job 必须计入 skipped，实际 {:?}",
        tag
    );
    st!(
        "C3 不可处理项显式 skipped",
        tag.failed == 0,
        "跳过不是失败（不许计 failed），实际 {:?}",
        tag.errors
    );
    let no_tag_for_deleted = {
        let n: i64 = env
            .conn
            .query_row(
                "SELECT COUNT(*) FROM tag WHERE assetId = ?1 AND origin <> 'custom'",
                [ids[1]],
                |r| r.get(0),
            )
            .unwrap();
        n
    };
    st!(
        "C3 不可处理项显式 skipped",
        no_tag_for_deleted == 0,
        "被跳过的资产不许写自动标签，实际 {no_tag_for_deleted} 行"
    );
    st!(
        "C3 不可处理项显式 skipped",
        env.count("SELECT COUNT(*) FROM job_queue WHERE status IN ('pending','running')") == 0,
        "队列收敛（无 pending / running 残留）"
    );

    eprintln!(
        "JOURNEY_4_SUMMARY {{\"phases\":3,\"elapsed_ms\":{}}}",
        t0.elapsed().as_millis()
    );
}

// ===========================================================================
// 流程 5：不可逆（彻底删 → 不可查 + 恢复必须失败）
// ===========================================================================

#[test]
fn journey_5_irreversible_purge() {
    let t0 = std::time::Instant::now();
    let env = Env::new("purge");

    stage!("I1 回收站");
    env.write("p1.png", &white_bg_png(1000, 700));
    env.write("p2.png", &stripes_png(900, 900));
    env.import();
    let ids = env.ids();
    st!("I1 回收站", ids.len() == 2, "2 张入库，实际 {}", ids.len());
    // 造出「彻底删要清的全部痕迹」：自动标签 + 判定 + 向量 + 缩略图
    let srv = mock::MockServer::start(move |_b| (200, tag_body(0)));
    let provider = tag_provider(&srv.base_url);
    let cancel = AtomicBool::new(false);
    svc_pipeline::enq_untagged(&env.conn).unwrap();
    svc_pipeline::run_tag_queue(&env.conn, &provider, &cancel, &mut |_p| {}).unwrap();
    let embedder = FakeEmbedder::new();
    svc_pipeline::enq_unembedded(&env.conn, FakeEmbedder::MODEL, EmbedScope::Semantic).unwrap();
    svc_pipeline::run_embed_queue(&env.conn, &embedder, EmbedScope::Semantic, &cancel, &mut |_p| {})
        .unwrap();
    let pack = main_pack();
    compliance_enq(&env.conn, &ids, pack.id.as_str(), None).unwrap();
    run_compliance_queue(&env.conn, &pack, &cancel, &mut |_p| {}).unwrap();

    let victim = ids[0];
    let ridd: i64 = env
        .conn
        .query_row(
            "SELECT resourceId FROM resource WHERE assetId = ?1 AND role='primary'",
            [victim],
            |r| r.get(0),
        )
        .unwrap();
    let thumb_paths: Vec<PathBuf> = core_media::THUMB_SIZES
        .iter()
        .map(|s| {
            env.thumbs
                .join(s.to_string())
                .join(format!("{:02}", victim % 100))
                .join(format!("{victim}_{s}.webp"))
        })
        .collect();
    for p in &thumb_paths {
        st!("I1 回收站", p.is_file(), "前置：缩略图应存在 {p:?}");
    }
    st!(
        "I1 回收站",
        core_storage::vecstore::count(&env.conn, FakeEmbedder::MODEL).unwrap() == 2,
        "前置：2 行向量"
    );
    let pre_verdicts: i64 = env
        .conn
        .query_row(
            "SELECT COUNT(*) FROM rule_verdict WHERE packId='journeyA' AND assetId = ?1",
            [victim],
            |r| r.get(0),
        )
        .unwrap();
    st!(
        "I1 回收站",
        pre_verdicts >= 3,
        "前置：目标资产已有判定行（彻底删必须一并清掉），实际 {pre_verdicts}"
    );

    st!("I1 回收站", svc_library::soft_delete_assets(&env.conn, &[victim]).unwrap() == 1, "软删 1 行");
    st!(
        "I1 回收站",
        svc_library::list_deleted(&env.conn, 50).unwrap().iter().any(|a| a.asset.assetId == victim),
        "回收站可见"
    );

    stage!("I2 彻底删");
    let purge = svc_library::purge_assets(&env.conn, &env.thumbs, &[victim]).unwrap();
    st!("I2 彻底删", purge.purged == vec![victim], "purged 只含目标，实际 {:?}", purge);
    st!("I2 彻底删", purge.not_in_trash.is_empty(), "目标在回收站内应被接受，实际 {:?}", purge);
    st!("I2 彻底删", purge.rows >= 3, "至少删掉 assets+resource+tag 等行，实际 {}", purge.rows);
    st!("I2 彻底删", purge.vectors == 1, "向量行删 1，实际 {}", purge.vectors);
    st!("I2 彻底删", purge.thumbs == 2, "两档缩略图文件都删，实际 {}", purge.thumbs);

    stage!("I3 恢复必须失败");
    let again = svc_library::restore_assets_checked(&env.conn, &[victim]).unwrap();
    st!(
        "I3 恢复必须失败",
        again.restored.is_empty() && again.not_in_trash == vec![victim],
        "彻底删后恢复必须响亮失败（not_in_trash），实际 {:?}",
        again
    );
    // 未软删的资产也不能被 purge 顺手删掉（防误点全库）
    let loose = svc_library::purge_assets(&env.conn, &env.thumbs, &[ids[1]]).unwrap();
    st!(
        "I3 恢复必须失败",
        loose.purged.is_empty() && loose.not_in_trash == vec![ids[1]],
        "不在回收站的资产必须拒绝彻底删，实际 {:?}",
        loose
    );
    st!(
        "I3 恢复必须失败",
        svc_library::restore_assets(&env.conn, &[victim]).unwrap() == 0,
        "老接口（幂等）也不许回报恢复成功"
    );

    stage!("I4 不可查");
    let asset_row: i64 = env
        .conn
        .query_row("SELECT COUNT(*) FROM assets WHERE assetId = ?1", [victim], |r| r.get(0))
        .unwrap();
    st!("I4 不可查", asset_row == 0, "assets 行必须消失");
    let res_row: i64 = env
        .conn
        .query_row("SELECT COUNT(*) FROM resource WHERE assetId = ?1", [victim], |r| r.get(0))
        .unwrap();
    st!("I4 不可查", res_row == 0, "resource 行必须消失");
    let tag_row: i64 = env
        .conn
        .query_row("SELECT COUNT(*) FROM tag WHERE assetId = ?1", [victim], |r| r.get(0))
        .unwrap();
    st!("I4 不可查", tag_row == 0, "tag 行必须消失");
    let verdict_row: i64 = env
        .conn
        .query_row("SELECT COUNT(*) FROM rule_verdict WHERE assetId = ?1", [victim], |r| r.get(0))
        .unwrap();
    st!("I4 不可查", verdict_row == 0, "判定行必须消失（合规视图不许留幽灵）");
    let vec_row: i64 = env
        .conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM \"{}\" WHERE resourceId = ?1",
                core_storage::vecstore::table_name(FakeEmbedder::MODEL)
            ),
            [ridd],
            |r| r.get(0),
        )
        .unwrap();
    st!("I4 不可查", vec_row == 0, "向量行必须消失（查重/检索不许命中）");
    for p in &thumb_paths {
        st!("I4 不可查", !p.exists(), "缩略图文件必须消失 {p:?}");
    }
    st!(
        "I4 不可查",
        !env.ids().contains(&victim),
        "图库列表不许再出现该资产"
    );
    let still = env.ids();
    st!("I4 不可查", still == vec![ids[1]], "另一张资产不受影响，实际 {still:?}");

    eprintln!(
        "JOURNEY_5_SUMMARY {{\"phases\":4,\"elapsed_ms\":{}}}",
        t0.elapsed().as_millis()
    );
}
