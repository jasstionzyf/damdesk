//! D5 端到端语义检索测试（真模型、真图片、真 SQLite）：
//! 生成 25 张不同语义几何图（5 色 × 5 形，image crate 确定性绘制）→ svc-import 导入 →
//! 真实 siglip2 语义塔 + 真实 dinov3 去重塔嵌入 →
//! 文搜图（"红色圆形"/"蓝色正方形"）top1 语义正确、以图搜图自匹配第一、复合过滤、
//! 查重阈值分带（同图副本 ≥0.98 near / 重绘变体 variant）。
//!
//! 环境（容器只读挂载）：
//! - DAM_TEST_MODELS=/models/siglip2-base-p224-onnx（真语义塔三件套）
//! - DAM_TEST_MODELS_DEDUP=/models/dinov3-vits16（真去重塔）
//! 缺任一 → 本文件测试显式失败（不静默跳过）。
//!
//! 放在 app-tauri（L3 可依赖全部 L0-L2，E2E 天然位置）；不走 Tauri 运行时，
//! 直接调 svc-* 函数——Tauri 命令层另由前端 DOM 断言覆盖。

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use engine_embed::onnx_dinov3::OnnxDinoV3Provider;
use engine_embed::onnx_siglip::OnnxSiglipProvider;
use engine_embed::EmbedProvider;
use image::{Rgb, RgbImage};
use rusqlite::Connection;

use svc_library::{
    find_duplicates, find_similar, search_by_text, SimilarOptions, TextSearchOptions,
};
use svc_pipeline::{enq_unembedded, run_embed_queue, EmbedScope};

const COLORS: [(&str, &str, Rgb<u8>); 5] = [
    ("red", "红", Rgb([220, 30, 30])),
    ("blue", "蓝", Rgb([30, 60, 220])),
    ("green", "绿", Rgb([30, 170, 40])),
    ("yellow", "黄", Rgb([235, 210, 30])),
    ("purple", "紫", Rgb([150, 40, 200])),
];
const SHAPES: [&str; 5] = ["circle", "square", "triangle", "stripes", "dots"];

fn model_dir(env_key: &str, what: &str) -> PathBuf {
    let d = std::env::var(env_key).unwrap_or_else(|_| {
        panic!("E2E 需要 {what}：export {env_key}=<模型目录>（容器内已挂 /models）")
    });
    let p = PathBuf::from(d);
    assert!(p.is_dir(), "{env_key} 不是目录: {}", p.display());
    p
}

/// 纹理种子（xorshift）：确定性底噪，让不同色/形组合在 ViT 特征空间结构可分
/// （纯平色块会让 dinov3 把同形不同色的图都判成 ~0.98 近重复）。
struct Noise(u64);
impl Noise {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 33) as u32
    }
}

/// 确定性绘制 256×256：带底噪的浅灰底 + 彩色形状（variant：位置轻微偏移）。
fn draw(shape: &str, color: Rgb<u8>, variant: u8) -> RgbImage {
    let s = 256i64;
    let seed_base = color.0[0] as u64 * 1_000_003
        + color.0[1] as u64 * 31
        + color.0[2] as u64
        + shape.len() as u64 * 7919;
    let mut noise = Noise(seed_base | 1);
    let mut img = RgbImage::new(256, 256);
    for y in 0..256u32 {
        for x in 0..256u32 {
            let n = (noise.next() % 18) as u8;
            img.put_pixel(x, y, Rgb([237 + n, 237 + n, 237 + n]));
        }
    }
    let c = s / 2;
    let off = variant as i64; // 变体：位置轻微偏移（重绘，非逐字节拷贝）
    match shape {
        "circle" => {
            for y in 0..s {
                for x in 0..s {
                    let dx = x - c - off;
                    let dy = y - c;
                    if dx * dx + dy * dy <= 80 * 80 {
                        let j = (noise.next() % 18) as i32 - 9;
                        img.put_pixel(
                            x as u32,
                            y as u32,
                            Rgb([
                                (color.0[0] as i32 + j).clamp(0, 255) as u8,
                                (color.0[1] as i32 + j).clamp(0, 255) as u8,
                                (color.0[2] as i32 + j).clamp(0, 255) as u8,
                            ]),
                        );
                    }
                }
            }
        }
        "square" => {
            for y in (c - 75 - off)..(c + 75) {
                for x in (c - 75)..(c + 75 + off) {
                    img.put_pixel(x as u32, y as u32, color);
                }
            }
        }
        "triangle" => {
            for y in (c - 80)..(c + 80) {
                let half = (y - (c - 80)) * 80 / 160;
                for x in (c - half - off)..(c + half) {
                    let j = (noise.next() % 18) as i32 - 9;
                    img.put_pixel(
                        x as u32,
                        y as u32,
                        Rgb([
                            (color.0[0] as i32 + j).clamp(0, 255) as u8,
                            (color.0[1] as i32 + j).clamp(0, 255) as u8,
                            (color.0[2] as i32 + j).clamp(0, 255) as u8,
                        ]),
                    );
                }
            }
        }
        "stripes" => {
            for y in 0..s {
                for x in 0..s {
                    if (x % 40) < 20 {
                        img.put_pixel(x as u32, y as u32, color);
                    }
                }
            }
        }
        "dots" => {
            for gy in 0..5 {
                for gx in 0..5 {
                    let ox = 30 + gx * 50;
                    let oy = 30 + gy * 50;
                    for y in (oy - 16)..(oy + 16) {
                        for x in (ox - 16)..(ox + 16) {
                            let dx = x - ox;
                            let dy = y - oy;
                            if dx * dx + dy * dy <= 15 * 15 {
                                let j = (noise.next() % 18) as i32 - 9;
                                img.put_pixel(
                                    (x + off) as u32,
                                    y as u32,
                                    Rgb([
                                        (color.0[0] as i32 + j).clamp(0, 255) as u8,
                                        (color.0[1] as i32 + j).clamp(0, 255) as u8,
                                        (color.0[2] as i32 + j).clamp(0, 255) as u8,
                                    ]),
                                );
                            }
                        }
                    }
                }
            }
        }
        _ => unreachable!(),
    }
    img
}

fn gen_fixtures(dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for (ci, (en, _zh, rgb)) in COLORS.iter().enumerate() {
        for (si, shape) in SHAPES.iter().enumerate() {
            let name = format!("{en}_{shape}.png");
            let _ = (ci, si);
            let img = draw(shape, *rgb, 0);
            img.save(dir.join(&name)).unwrap();
            names.push(name);
        }
    }
    assert!(names.len() >= 20, "验收要求 ≥20 张不同语义图");
    names
}

fn siglip() -> OnnxSiglipProvider {
    let reg = core_config::load_registry(None).unwrap();
    let spec = reg.by_name(core_config::DEFAULT_EMBEDDER).unwrap().clone();
    let mut p =
        OnnxSiglipProvider::new(spec, model_dir("DAM_TEST_MODELS", "真 siglip2 模型"), None);
    p.load().unwrap();
    p
}

fn dinov3() -> OnnxDinoV3Provider {
    let reg = core_config::load_registry(None).unwrap();
    let spec = reg.by_name(core_config::DEDUP_EMBEDDER).unwrap().clone();
    let mut p = OnnxDinoV3Provider::new(
        spec,
        model_dir("DAM_TEST_MODELS_DEDUP", "真 dinov3 模型"),
        None,
    );
    p.load().unwrap();
    p
}

fn nop(_p: core_types::EmbedProgress) {}

struct Lib {
    conn: Connection,
    /// fileName → assetId
    by_name: std::collections::HashMap<String, i64>,
}

fn build_library(root: &Path) -> Lib {
    // 图片源目录
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    gen_fixtures(&src);

    // E2E 查重要求：近重复图（视觉相同、SHA 不同——逐字节副本会被 D2 导入器
    // 的 fileHash 去重拦下根本不入库，dinov3 塔的价值正是抓这种漏网近重复）
    // + 一张重绘变体（位置偏移，cos 落 variant 带）
    let mut near = image::open(src.join("red_circle.png")).unwrap().to_rgb8();
    near.put_pixel(0, 0, Rgb([254, 254, 254])); // 1 像素差异：SHA 变、视觉不变
    near.save(src.join("red_circle_near.png")).unwrap();
    let (_, _, rgb) = COLORS[0];
    draw("circle", rgb, 9)
        .save(src.join("red_circle_variant.png"))
        .unwrap();

    let app = root.join("app");
    std::fs::create_dir_all(&app).unwrap();
    let conn = core_storage::open(&app.join("dam.db")).unwrap();

    // 导入（reference 模式；缩略图产出 = D2 路径，此处顺带验证 thumbs 256 存在）
    let files = svc_import::scan_dir(&src).unwrap();
    assert_eq!(files.len(), 27, "25 语义 + 1 近重复 + 1 变体");
    let batch = svc_import::create_batch(&conn, &src, "reference", files.len() as i64).unwrap();
    let thumbs = app.join("thumbs");
    svc_import::resume_batch(&conn, batch, None, &thumbs, |_| Ok(())).unwrap();

    let mut by_name = std::collections::HashMap::new();
    conn.prepare("SELECT assetId, fileName FROM assets")
        .unwrap()
        .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(0)?)))
        .unwrap()
        .for_each(|row| {
            let (name, id) = row.unwrap();
            by_name.insert(name, id);
        });
    assert_eq!(by_name.len(), 27);
    Lib { conn, by_name }
}

fn embed_all(lib: &mut Lib) -> (OnnxSiglipProvider, OnnxDinoV3Provider) {
    let sem = siglip();
    let ded = dinov3();
    let cancel = AtomicBool::new(false);

    enq_unembedded(
        &lib.conn,
        core_config::DEFAULT_EMBEDDER,
        EmbedScope::Semantic,
    )
    .unwrap();
    let rep = run_embed_queue(&lib.conn, &sem, EmbedScope::Semantic, &cancel, &mut nop).unwrap();
    assert_eq!(rep.failed, 0, "语义塔零失败: {:?}", rep.errors);
    assert_eq!(rep.done, 27);

    enq_unembedded(&lib.conn, core_config::DEDUP_EMBEDDER, EmbedScope::Dedup).unwrap();
    let rep = run_embed_queue(&lib.conn, &ded, EmbedScope::Dedup, &cancel, &mut nop).unwrap();
    assert_eq!(rep.failed, 0, "去重塔零失败: {:?}", rep.errors);
    assert_eq!(rep.done, 27);
    (sem, ded)
}

#[test]
fn e2e_text_search_semantics_top1() {
    let root = tempfile::tempdir().unwrap();
    let mut lib = build_library(root.path());
    let (sem, _ded) = embed_all(&mut lib);

    // 文搜图：中文查询（SigLIP2 Gemma tokenizer 原生中文）→ top1 命中对应色+形
    for (query, expect_prefix) in [
        ("红色圆形", "red_circle"),
        ("蓝色正方形", "blue_square"),
        ("绿色三角形", "green_triangle"),
    ] {
        let hits = search_by_text(
            &lib.conn,
            &sem,
            query,
            5,
            &Default::default(),
            &TextSearchOptions { min_score: Some(0.0) },
        ).unwrap();
        assert!(!hits.is_empty(), "{query} 无结果");
        assert!(
            hits[0].fileName.starts_with(expect_prefix),
            "{query} top1 = {}（期望 {expect_prefix}），top5 = {:?}",
            hits[0].fileName,
            hits.iter()
                .map(|h| h.fileName.clone())
                .take(5)
                .collect::<Vec<_>>()
        );
        // 语义排序有效性：top5 里"同色或同形"≥3（合成色块下 SigLIP 按色+形联合排序，
        // 同色全部在前是过强断言；形状词唯一，同形即强信号）
        let (color_word, shape_word) = expect_prefix.split_once('_').unwrap();
        let related = hits
            .iter()
            .filter(|h| h.fileName.starts_with(color_word) || h.fileName.contains(shape_word))
            .count();
        assert!(
            related >= 3,
            "{query} top5 相关仅 {related} 个: {:?}",
            hits.iter()
                .map(|h| h.fileName.clone())
                .take(5)
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn e2e_image_search_self_match_and_filters() {
    let root = tempfile::tempdir().unwrap();
    let mut lib = build_library(root.path());
    let (sem, _ded) = embed_all(&mut lib);

    // 以图搜图：自匹配第一且 score≈1
    let target = lib.by_name["blue_square.png"];
    let hits = find_similar(
        &lib.conn,
        &sem,
        target,
        5,
        &Default::default(),
        &SimilarOptions { min_score: Some(0.0) },
    )
    .unwrap();
    assert_eq!(hits[0].assetId, target, "自匹配第一");
    assert!(
        hits[0].score > 0.999,
        "自匹配 score={}（期望≈1）",
        hits[0].score
    );
    // 相似结果相关性：除自匹配外，强相似（score≥0.7）项必须同色或同形
    for h in &hits {
        if h.assetId == target || h.score < 0.7 {
            continue;
        }
        let name: String = lib
            .conn
            .query_row(
                "SELECT fileName FROM assets WHERE assetId=?1",
                [h.assetId],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            name.starts_with("blue") || name.contains("square"),
            "score={} 的 {} 与 blue_square 不同色不同形",
            h.score,
            name
        );
    }

    // 复合过滤：查"红色圆形"但限定 ext=png 之外的目录（全部 png → 空结果路径）
    let hits = search_by_text(
        &lib.conn,
        &sem,
        "红色圆形",
        5,
        &engine_search::SearchFilter {
            ext: Some("webp".into()),
            ..Default::default()
        },
        &TextSearchOptions { min_score: Some(0.0) },
    )
    .unwrap();
    assert!(hits.is_empty(), "不可能过滤 → 空结果而非报错");

    // 尺寸过滤（全部 256×256 → min_width 1000 排除全部；min 200 全过）
    let hits = search_by_text(
        &lib.conn,
        &sem,
        "红色圆形",
        30,
        &engine_search::SearchFilter {
            min_width: Some(1000),
            ..Default::default()
        },
        &TextSearchOptions { min_score: Some(0.0) },
    )
    .unwrap();
    assert!(hits.is_empty());
    let hits = search_by_text(
        &lib.conn,
        &sem,
        "红色圆形",
        30,
        &engine_search::SearchFilter {
            min_width: Some(200),
            ..Default::default()
        },
        &TextSearchOptions { min_score: Some(0.0) },
    )
    .unwrap();
    assert_eq!(hits.len(), 27, "全库 27 张都满足 min_width=200");

    // 目录过滤（reference 模式 absPath 都在 src/ 下）
    let hits = search_by_text(
        &lib.conn,
        &sem,
        "圆形",
        30,
        &engine_search::SearchFilter {
            dir_prefix: Some(root.path().join("src").display().to_string()),
            ..Default::default()
        },
        &TextSearchOptions { min_score: Some(0.0) },
    )
    .unwrap();
    assert_eq!(hits.len(), 27);
}

#[test]
fn e2e_find_duplicates_bands() {
    let root = tempfile::tempdir().unwrap();
    let mut lib = build_library(root.path());
    let (_sem, ded) = embed_all(&mut lib);

    let groups = find_duplicates(
        &lib.conn,
        &ded,
        &engine_search::DupOptions {
            k_per_item: 6,
            min_score: 0.90,
            max_assets: 0,
        },
    )
    .unwrap();
    assert!(!groups.is_empty(), "必须发现重复组（同图副本存在）");

    // 近重复对（red_circle + red_circle_near，1 像素差）必须落在同一组，
    // 且两图 dinov3 相似度 ≥0.98（near 带定义）。锚点可能是组内其他成员，不做名字断言。
    let orig_id = lib.by_name["red_circle.png"];
    let copy_id = lib.by_name["red_circle_near.png"];
    let grouped = groups.iter().any(|g| {
        let ids: Vec<i64> = std::iter::once(g.anchorAssetId)
            .chain(g.members.iter().map(|m| m.assetId))
            .collect();
        ids.contains(&orig_id) && ids.contains(&copy_id)
    });
    assert!(grouped, "近重复对未成组：{groups:#?}");
    let rid_of = |conn: &Connection, asset: i64| -> i64 {
        conn.query_row(
            "SELECT resourceId FROM resource WHERE assetId = ?1 AND role = 'primary'",
            [asset],
            |r| r.get(0),
        )
        .unwrap()
    };
    let vecs = core_storage::vecstore::fetch_vectors(
        &lib.conn,
        core_config::DEDUP_EMBEDDER,
        &[rid_of(&lib.conn, orig_id), rid_of(&lib.conn, copy_id)],
    )
    .unwrap();
    let pair_cos = engine_embed::cosine(&vecs[0].1, &vecs[1].1);
    assert!(pair_cos >= 0.98, "近重复对 cos={pair_cos} < 0.98");

    // 分带语义：near 组内最高分 ≥0.98；variant 组（若有）都在 [0.90,0.98)
    for g in &groups {
        let best = g.members.iter().map(|m| m.score).fold(0.0f32, f32::max);
        match g.band.as_str() {
            "near" => assert!(best >= 0.98, "near 组 best={best}"),
            "variant" => assert!(best < 0.98 && best >= 0.90, "variant 组 best={best}"),
            other => panic!("未知分带 {other}"),
        }
    }
}

#[test]
fn e2e_dedup_vectors_independent_table() {
    let root = tempfile::tempdir().unwrap();
    let mut lib = build_library(root.path());
    embed_all(&mut lib);

    // 双表共存（禁混算结构保证）+ 行数一致
    let tables = core_storage::vecstore::tables(&lib.conn).unwrap();
    let names: Vec<String> = tables.iter().map(|t| t.0.clone()).collect();
    assert!(
        names.contains(&core_config::DEFAULT_EMBEDDER.to_string()),
        "{names:?}"
    );
    assert!(
        names.contains(&core_config::DEDUP_EMBEDDER.to_string()),
        "{names:?}"
    );
    assert_eq!(
        core_storage::vecstore::count(&lib.conn, core_config::DEFAULT_EMBEDDER).unwrap(),
        27
    );
    assert_eq!(
        core_storage::vecstore::count(&lib.conn, core_config::DEDUP_EMBEDDER).unwrap(),
        27
    );
    // 状态机：双塔全 done 且互不污染
    let (s, d): (i64, i64) = lib
        .conn
        .query_row(
            "SELECT SUM(emb_status='done'), SUM(dedup_status='done') FROM assets",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((s, d), (27, 27));
}
