//! 掩码探针（语料预检校准用）：打印 fg_ratio / 各边带网格前景 cell 数 / expand_to 后
//! 像素 4px 边带的前景像素数。
//!
//! 用法（容器内）：
//!   DAM_MODELS_ROOT=<root> cargo run --release -p engine-segment \
//!     --features local-infer --example mask_probe -- <img> [...]

#[cfg(feature = "local-infer")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("用法: mask_probe <img> [...]");
        std::process::exit(2);
    }
    let seg = engine_segment::segmenter("u2net").expect("segmenter");
    for p in &args {
        // 支持 "path@W"：先缩放到长边 W 再探（用于定位尺寸相关的预处理差异）
        let (path, resized) = match p.split_once('@') {
            Some((f, w)) => {
                let tmp = std::env::temp_dir().join(format!("probe_{w}.jpg"));
                let n = w.parse::<i32>().unwrap_or(600);
                core_media::resize_to(std::path::Path::new(f), &tmp, n).expect("resize");
                (tmp, true)
            }
            None => (std::path::PathBuf::from(p), false),
        };
        let p = &path.to_string_lossy().to_string();
        let (w, h, px) = core_media::rgb8(std::path::Path::new(p)).expect("decode");
        let (Ok(w), Ok(h)) = (u32::try_from(w), u32::try_from(h)) else { continue };
        if resized {
            println!("[resized] {p}: {w}x{h}");
        }
        let s = seg.run_rgb(w, h, &px).expect("run");
        let m = &s.mask;
        let side = m.side as usize;
        let band = |f: &dyn Fn(usize, usize, usize, usize) -> u32| f(0, 0, 0, 0);
        let _ = band;
        let cnt = |x0: usize, y0: usize, x1: usize, y1: usize| m.fg_in_window(x0, y0, x1, y1);
        println!(
            "{p}: {w}x{h} fg_ratio={:.4} dominant_bbox={:?}",
            s.fg_ratio,
            m.dominant_bbox()
        );
        println!(
            "  grid(side={side}) 边带 cell 前景: top={} bottom={} left={} right={} (band=4)",
            cnt(0, 0, side - 1, 3),
            cnt(0, side - 4, side - 1, side - 1),
            cnt(0, 0, 3, side - 1),
            cnt(side - 4, 0, side - 1, side - 1),
        );
        let full = m.fg_in_window(0, 0, side - 1, side - 1);
        println!("  全格前景 cell={full}/{}", side * side);
        let exp = m.expand_to(w, h);
        let (wu, hu) = (w as usize, h as usize);
        let b = 4usize;
        let mut edge_fg = 0usize;
        for y in 0..hu {
            for x in 0..wu {
                if x < b || y < b || x >= wu - b || y >= hu - b {
                    if exp[y * wu + x] == 255 {
                        edge_fg += 1;
                    }
                }
            }
        }
        println!("  expand_to 后 4px 边带前景像素={edge_fg}");
    }
}

#[cfg(not(feature = "local-infer"))]
fn main() {
    eprintln!("需要 --features local-infer");
    std::process::exit(2);
}
