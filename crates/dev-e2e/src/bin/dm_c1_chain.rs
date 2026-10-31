//! DM-C1（task）真实链路验收：一张真图 × 真 OCR/真 inpaint（lama）× mock VL。
//!
//! 走的是**生产同一条队列**（`run_compliance_queue_par`）+ 同一条区域决议（`inpaint_region`），
//! 只把「VL 判官」换成确定性 mock（模型判定质量不在本卡口径内，K3 已证明单次 VL 复检本身抖动 50%）。
//!
//! 断言（任一不过就以非 0 退出）：
//! 1. `ocr_boxes` 入库行数 == det 输出框数（不丢框）；
//! 2. `region: ocr_text` 在队列里**不再报参数错**，注入的是 OCR 框（不是 VL bbox）；
//! 3. 真实 lama 产出修复图（文件存在、尺寸不变）；
//! 4. 复检 pass → verdict=pass，证据里 `region.source = ocr_text`；产物路径打印出来。
//!
//! 用法（容器内）：
//! ```text
//! DAM_C1_MODELS=/models DAM_C1_IMAGE=<真图> cargo run -p dev-e2e --bin dm_c1_chain
//! ```

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use core_config::Pack;
use svc_pipeline::compliance_queue::{compliance_enq, run_compliance_queue_par, ConnFactory};
use svc_pipeline::compliance_vl::VlConfig;

fn main() {
    if let Err(e) = run() {
        eprintln!("[DM-C1] FAIL: {e}");
        std::process::exit(1);
    }
}

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

fn run() -> Result<(), String> {
    let stamp = std::process::id();
    let work = std::env::temp_dir().join(format!("dm-c1-chain-{stamp}"));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(work.join("out")).map_err(|e| e.to_string())?;

    // ---- 模型根：<root>/models -> 真模型目录（core_config 布局契约） ----
    let real = PathBuf::from(env_or("DAM_C1_MODELS", "/models"));
    let root = work.join("models-root");
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    std::os::unix::fs::symlink(&real, root.join("models")).map_err(|e| {
        format!("模型根软链失败（{} → {}）: {e}", root.display(), real.display())
    })?;
    std::env::set_var("DAM_MODELS_ROOT", &root);
    println!("[0] models root = {} → {}", root.display(), real.display());

    // ---- 语料：真图（带文字的截图/主图） ----
    let img = PathBuf::from(env_or(
        "DAM_C1_IMAGE",
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../reports/w6-49-lightbox-zoom.png"
        ),
    ));
    if !img.is_file() {
        return Err(format!("语料不存在: {}（用 DAM_C1_IMAGE 指一张真图）", img.display()));
    }
    println!("[1] 语料 = {}", img.display());

    // ---- 库 + 资产 ----
    let db_path = work.join("library.db");
    let conn = core_storage::open(&db_path).map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, importedAt)
         VALUES (NULL, ?1, 'image', ?2, ?3, 1, 0, 0)",
        rusqlite::params![
            img.to_string_lossy(),
            img.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
            img.extension().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
        ],
    )
    .map_err(|e| e.to_string())?;
    let asset_id = conn.last_insert_rowid();
    let (iw, ih) = core_media::image_size(&img).map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1, 'primary', 0, ?2, ?3)",
        rusqlite::params![asset_id, iw as i64, ih as i64],
    )
    .map_err(|e| e.to_string())?;
    println!("[2] asset#{asset_id}  {iw}x{ih}");

    // ---- 真 OCR（PP-OCRv6 det+rec） ----
    let engine = engine_ocr::OcrEngine::load().map_err(|e| format!("OCR 引擎加载: {e}"))?;
    let text = svc_pipeline::ocr_one(&conn, &engine, asset_id).map_err(|e| format!("ocr_one: {e}"))?;
    let rows = core_storage::ocr_boxes::count(&conn, asset_id).map_err(|e| e.to_string())?;
    let (w, h, rgb) = core_media::rgb8(Path::new(&img)).map_err(|e| e.to_string())?;
    let lines = engine
        .run_rgb(w as u32, h as u32, &rgb)
        .map_err(|e| format!("OCR 复算 det: {e}"))?;
    let degenerate = lines
        .iter()
        .filter(|l| l.x1 <= l.x0 || l.y1 <= l.y0)
        .count();
    println!(
        "[3] OCR: det 框 {}（退化 {}） / 入库行框 {} / 文本 {} 字",
        lines.len(),
        degenerate,
        rows,
        text.chars().count()
    );
    if rows != (lines.len() - degenerate) as i64 {
        return Err(format!(
            "行框入库数 {} != det 有效框数 {}",
            rows,
            lines.len() - degenerate
        ));
    }
    if rows == 0 {
        return Err("这张图 det 没出框（换一张带文字的图）".into());
    }
    let rects = core_storage::ocr_boxes::load_rects(&conn, asset_id).map_err(|e| e.to_string())?;
    println!(
        "[4] 行框（归一化，共 {}）：{:?} …（末 3 条 {:?}）",
        rects.len(),
        &rects[..rects.len().min(3)],
        &rects[rects.len().saturating_sub(3)..]
    );

    // ---- pack：一条 R3 规则 + region: ocr_text 的 inpaint 修复器 ----
    let rules = r#"
- id: R3_no_watermark
  name: 无水印
  executor: vl
  check: {detector: vl_judge, params: {prompt: "是否有水印", expect: 否}, confidence: {source: fixed, value: 0.9}}
  fixer: {type: inpaint, tier: best_effort, params: {region: ocr_text}}
  on_fail: retry_then_human
"#;
    let pack = Pack::from_yaml_strs(
        "dmc1real",
        "category: [场景]\n",
        rules,
        "steps:\n  - {kind: judge, rule: R3_no_watermark}\nrecheck_max: 1\n",
        "summary: [合格, 不合格, 未判定]\ndetail_columns: [imageSN, R3_no_watermark]\n",
    )
    .map_err(|e| e.to_string())?;

    // ---- mock VL：① 判违规（带 bbox，故意指到与 OCR 框无重叠的位置）② 复检 pass ----
    let vl_bbox = [0.02, 0.02, 0.12, 0.08];
    let base = spawn_mock(vec![
        format!(
            r#"{{"rules":{{"R3_no_watermark":{{"violated":true,"regions":[{{"what":"水印","bbox":{vl_bbox:?}}}]}}}}}}"#
        ),
        r#"{"rules":{"R3_no_watermark":{"violated":false,"regions":[]}}}"#.to_string(),
    ]);
    let cfg = VlConfig {
        base_url: base,
        api_key: "mock".into(),
        model: "mock-vl".into(),
        timeout_secs: 60,
        max_tokens: 512,
        proxy: Some(String::new()),
    };

    let out_dir = work.join("out").to_string_lossy().to_string();
    compliance_enq(&conn, &[asset_id], pack.id.as_str(), Some(&out_dir))
        .map_err(|e| e.to_string())?;
    let db_for_worker = db_path.clone();
    let factory: ConnFactory = Arc::new(move || {
        core_storage::open(&db_for_worker).map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))
    });
    let (rep, vl_snap) = run_compliance_queue_par(
        factory,
        &pack,
        Some(cfg),
        1,
        &AtomicBool::new(false),
        |_| {},
    )
    .map_err(|e| format!("队列执行: {e}"))?;
    println!(
        "[5] 队列: done={} failed={} pending={} / VL 调用 {} 次",
        rep.done,
        rep.jobFailed,
        rep.jobPending,
        vl_snap.map(|s| s.calls).unwrap_or(0)
    );

    // ---- 结论 + 证据 ----
    let (verdict, fix, recheck, evidence): (String, Option<String>, Option<String>, String) = conn
        .query_row(
            "SELECT COALESCE(verdict,''), fixAction, recheckResult, COALESCE(evidence,'')
             FROM rule_verdict WHERE packId = ?1 AND ruleId = 'R3_no_watermark'",
            [&pack.id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map_err(|e| format!("读 verdict: {e}"))?;
    let ev: serde_json::Value = serde_json::from_str(&evidence).map_err(|e| e.to_string())?;
    println!("[6] verdict={verdict} fix={fix:?} recheck={recheck:?}");
    println!(
        "[7] evidence.region.source = {} / boxes {} 个 / VL 给的 bbox {vl_bbox:?} 未采用",
        ev["region"]["source"],
        ev["region"]["boxes"].as_array().map(|a| a.len()).unwrap_or(0)
    );
    if ev["region"]["source"] != "ocr_text" {
        return Err(format!("区域来源不是 ocr_text: {}", ev["region"]["source"]));
    }
    // 注入框 == 库里的 OCR 行框（逐元素 1e-9 容差：JSON 十进制往返有 1ulp 级差异）
    let injected = ev["region"]["boxes"].as_array().ok_or("证据里没有 boxes")?;
    if injected.len() != rects.len() {
        return Err(format!(
            "注入框数 {} != 库里 OCR 行框数 {}",
            injected.len(),
            rects.len()
        ));
    }
    for (i, (got, want)) in injected.iter().zip(rects.iter()).enumerate() {
        let got: [f64; 4] = serde_json::from_value(got.clone()).map_err(|e| e.to_string())?;
        for k in 0..4 {
            if (got[k] - want[k]).abs() > 1e-9 {
                return Err(format!("第 {i} 个框第 {k} 维不一致: {got:?} vs {want:?}"));
            }
        }
    }
    if ev["reason"].as_str().is_some() {
        return Err(format!("修复未跑通（reason={}）: {ev}", ev["reason"]));
    }
    let product = PathBuf::from(
        ev["fix"]["output"]["path"]
            .as_str()
            .ok_or("证据里没有产物路径")?,
    );
    if !product.is_file() {
        return Err(format!("产物不存在: {}", product.display()));
    }
    let (pw, ph) = core_media::image_size(&product).map_err(|e| e.to_string())?;
    println!("[8] 产物 = {} （{pw}x{ph}）", product.display());
    if (pw, ph) != (iw, ih) {
        return Err(format!("产物尺寸 {pw}x{ph} != 原图 {iw}x{ih}"));
    }
    // 持久证据（临时目录会被清）：产物复制到 reports/evidence/dm-c1/
    let keep = PathBuf::from(env_or(
        "DAM_C1_OUT",
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../reports/evidence/dm-c1"),
    ));
    std::fs::create_dir_all(&keep).map_err(|e| e.to_string())?;
    let kept = keep.join(format!(
        "{}-inpainted.png",
        img.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
    ));
    std::fs::copy(&product, &kept).map_err(|e| format!("复制产物: {e}"))?;
    println!("[9] 已归档产物 = {}", kept.display());
    println!("[10] 重绘区域来源 = ocr_text（VL 给的 bbox {vl_bbox:?} 未被采用）");
    println!("[DM-C1] PASS");
    // 临时工作目录保留（产物路径可查），只有 /tmp 会被系统清理
    println!("[11] 工作目录 = {}", work.display());
    Ok(())
}

/// 最小 OpenAI 兼容 mock（按顺序吐 answers；只为把链路跑通，判官质量不在本卡口径内）。
fn spawn_mock(answers: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut it = answers.into_iter();
        for req in listener.incoming() {
            let Some(answer) = it.next() else { break };
            let mut stream = req.unwrap();
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 16384];
            loop {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header_end = pos + 4;
                    let head = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= header_end + len {
                        break;
                    }
                }
            }
            let body = serde_json::json!({ "choices": [{ "message": { "content": answer } }] })
                .to_string();
            let http = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(http.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://{addr}")
}
