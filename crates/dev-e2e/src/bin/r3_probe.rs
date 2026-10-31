//! 多实例主体探针（R3 坑位标定用）：一张图 → u2net 掩码全部连通域 + PP-OCR 行，
//! 逐行打印「单主体 / 多实例主体区」两种口径下的文字区归属。
//!
//! 为什么需要它：R3（无水印）此前只把**最大连通域**当主体（`dominant_bbox`），多商品图
//! （一串胶囊、一排货架商品）里其余实体的自带文字会被算成「主体外文字」→ 判成牛皮癣。
//! 这里把真实几何（每个连边域 bbox/面积 + 每行 OCR 框/文本）打出来，让口径按实测数据定，
//! 而不是靠猜。
//!
//! 用法（容器内，模型根 `DAM_MODELS_ROOT=/`）：
//!   cargo run --release -p dev-e2e --bin r3_probe -- <img> [...]

use std::path::Path;

/// 一个矩形与一个实例框的交/含关系（像素口径，左闭右开）。
fn inter(a: [i64; 4], b: [i64; 4]) -> i64 {
    let w = (a[2].min(b[2]) - a[0].max(b[0])).max(0);
    let h = (a[3].min(b[3]) - a[1].max(b[1])).max(0);
    w * h
}

fn area(b: [i64; 4]) -> i64 {
    ((b[2] - b[0]).max(0)) * ((b[3] - b[1]).max(0))
}

fn center_inside(b: [i64; 4], s: [i64; 4]) -> bool {
    let cx = (b[0] + b[2]) as f64 / 2.0;
    let cy = (b[1] + b[3]) as f64 / 2.0;
    (s[0] as f64) <= cx && cx <= (s[2] as f64) && (s[1] as f64) <= cy && cy <= (s[3] as f64)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("用法: r3_probe <img> [...]  （模型根 DAM_MODELS_ROOT）");
        std::process::exit(2);
    }
    let seg = engine_segment::segmenter("u2net").expect("u2net segmenter");
    let ocr = engine_ocr::infer::engine().expect("ocr engine");
    const COVER_MAX: f64 = 0.08; // 与 pack 阈值同口径（subject_cover_ratio_max）
    const MIN_CHARS: usize = 2; // 与 pack 同口径（min_text_chars）
    for p in &args {
        let path = Path::new(p);
        let (w, h, px) = core_media::rgb8(path).expect("decode");
        let (wu, hu) = (w as u32, h as u32);
        let s = seg.run_rgb(wu, hu, &px).expect("segment");
        let comps = s.mask.components();
        let img_area = (wu as f64) * (hu as f64);
        println!(
            "== {p}  {wu}x{hu}  fg_ratio={:.4}  mask_bbox={:?}",
            s.fg_ratio,
            s.mask.bbox()
        );
        let scale = s.mask.side as f64;
        let to_px = |b: [usize; 4]| -> [i64; 4] {
            [
                (b[0] as f64 * wu as f64 / scale).round() as i64,
                (b[1] as f64 * hu as f64 / scale).round() as i64,
                ((b[2] as f64 + 1.0) * wu as f64 / scale).round() as i64,
                ((b[3] as f64 + 1.0) * hu as f64 / scale).round() as i64,
            ]
        };
        println!("   连通域 {} 个（按面积降序）：", comps.len());
        let mut inst_px: Vec<[i64; 4]> = Vec::new();
        for (i, c) in comps.iter().enumerate().take(12) {
            let b = to_px(c.bbox);
            let cell_ratio = c.area as f64 / (s.mask.side * s.mask.side) as f64;
            println!(
                "   #{i} grid={:?} px={:?} cells={} cell_ratio={:.4} bbox_area_ratio={:.4}",
                c.bbox,
                b,
                c.area,
                cell_ratio,
                area(b) as f64 / img_area
            );
            inst_px.push(b);
        }
        let lines = ocr.run_rgb(wu, hu, &px).expect("ocr");
        println!("   OCR 行 {} 条：", lines.len());
        for l in &lines {
            let b = [l.x0, l.y0, l.x1, l.y1];
            let txt = l.text.trim();
            if txt.chars().count() < MIN_CHARS {
                println!("   - box={b:?} text={txt:?} → noise（字符数 < {MIN_CHARS}）");
                continue;
            }
            // 单主体口径（现状）
            let single = s.mask.dominant_bbox().map(to_px);
            let one = match single {
                Some(sub) => {
                    let cover = inter(b, sub) as f64 / area(sub).max(1) as f64;
                    if cover >= COVER_MAX {
                        format!("watermark(cover={cover:.3})")
                    } else if center_inside(b, sub) {
                        format!("label(cover={cover:.3})")
                    } else {
                        "outside".to_string()
                    }
                }
                None => "outside(no subject)".to_string(),
            };
            // 多实例口径（候选）：中心落在任一实例内 = 该实例的自带文字
            let mut best: Option<(usize, f64)> = None;
            for (i, inst) in inst_px.iter().enumerate() {
                let cover = inter(b, *inst) as f64 / area(*inst).max(1) as f64;
                if center_inside(b, *inst) {
                    match best {
                        Some((_, c)) if c >= cover => {}
                        _ => best = Some((i, cover)),
                    }
                }
            }
            let multi = match best {
                Some((i, cover)) => format!("inside#{i}(cover={cover:.3})"),
                None => "outside".to_string(),
            };
            println!(
                "   - box={b:?} area_ratio={:.4} text={txt:?} score={:.3} → 单主体:{one} 多实例:{multi}",
                area(b) as f64 / img_area,
                l.text_score
            );
        }
    }
}
