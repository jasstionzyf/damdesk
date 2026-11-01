//! 语料派生诊断（C5 用）：复现 c5_e2e 的派生链，逐段测占比，定位「预测可闭合但产出不在带内」。
//!
//! 用法（容器内）：
//!   DAM_MODELS_ROOT=<root> cargo run --release -p engine-segment \
//!     --features local-infer --example derive_probe -- <img> [...]

#[cfg(feature = "local-infer")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let seg = engine_segment::segmenter("u2net").expect("segmenter");
    let side = engine_segment::SIDE as i32;
    let dir = std::env::temp_dir().join(format!("derive_probe_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // 与 c5_e2e.rs scale_min700 同口径：短边抬到 ≥700
    let scale_min700 = |src: &std::path::Path, dest: &std::path::Path| -> Result<(), String> {
        let (w, h) = core_media::image_size(src).map_err(|e| e.to_string())?;
        let (wf, hf) = (w.max(1) as f64, h.max(1) as f64);
        let k = (700.0 / wf.min(hf)).max(1.0);
        let target_long = ((wf.max(hf)) * k).ceil() as i32;
        core_media::scale_to(src, dest, target_long).map_err(|e| e.to_string())?;
        Ok(())
    };
    let measure = |p: &std::path::Path| -> f64 {
        (|| -> Option<f64> {
            let (w, h, px) = core_media::rgb8(p).ok()?;
            let s = seg.run_rgb(w as u32, h as u32, &px).ok()?;
            Some(s.fg_ratio)
        })()
        .unwrap_or(-1.0)
    };

    for (i, p) in args.iter().enumerate() {
        let path = std::path::Path::new(p);
        let (w, h, px) = match core_media::rgb8(path) {
            Ok(v) => v,
            Err(e) => {
                println!("ERR {p} {e}");
                continue;
            }
        };
        let s = seg.run_rgb(w as u32, h as u32, &px).expect("seg");
        let compact = match s.mask.dominant_bbox() {
            Some([x0, y0, x1, y1]) => {
                s.mask.fg_count() as f64 / ((x1 - x0 + 1) * (y1 - y0 + 1)) as f64
            }
            None => 0.0,
        };
        let r0 = s.fg_ratio;
        let win = s.crop_window(0.75).map(|(w_, _)| w_);
        // A) 只抬尺寸（不裁）→ 测占比：检验「缩放是否改变占比语义」
        let sc = dir.join(format!("sc{i}.jpg"));
        let r_scaled = if scale_min700(path, &sc).is_ok() { measure(&sc) } else { -2.0 };
        // B) 裁（原图网格窗）→ 抬尺寸 → 测占比：与 c5_e2e 主组同链
        let r_crop_scaled = match win {
            Some(w0) => {
                let (x0, y0, x1, y1) = (w0[0] as i32, w0[1] as i32, w0[2] as i32, w0[3] as i32);
                let x = (x0 * w / side).clamp(0, w - 1);
                let y = (y0 * h / side).clamp(0, h - 1);
                let x2 = (((x1 + 1) * w + side - 1) / side).clamp(x + 1, w);
                let y2 = (((y1 + 1) * h + side - 1) / side).clamp(y + 1, h);
                let tmp = dir.join(format!("cr{i}.jpg"));
                let dst = dir.join(format!("crs{i}.jpg"));
                if core_media::crop_to(path, &tmp, x, y, x2 - x, y2 - y).is_ok()
                    && scale_min700(&tmp, &dst).is_ok()
                {
                    measure(&dst)
                } else {
                    -3.0
                }
            }
            None => -4.0,
        };
        // C) 同链但产出名复用（复现 e2e 里 m-{n:05}.jpg 反复覆写的路径）→ 检验 libvips 同路径覆写读旧
        let r_crop_scaled_reused = match win {
            Some(w0) => {
                let (x0, y0, x1, y1) = (w0[0] as i32, w0[1] as i32, w0[2] as i32, w0[3] as i32);
                let x = (x0 * w / side).clamp(0, w - 1);
                let y = (y0 * h / side).clamp(0, h - 1);
                let x2 = (((x1 + 1) * w + side - 1) / side).clamp(x + 1, w);
                let y2 = (((y1 + 1) * h + side - 1) / side).clamp(y + 1, h);
                let tmp = dir.join("reuse_tmp.jpg");
                let dst = dir.join("reuse_dst.jpg");
                let _ = std::fs::remove_file(&dst);
                if core_media::crop_to(path, &tmp, x, y, x2 - x, y2 - y).is_ok()
                    && scale_min700(&tmp, &dst).is_ok()
                {
                    measure(&dst)
                } else {
                    -3.0
                }
            }
            None => -4.0,
        };
        println!(
            "n={:>5.3} compact={compact:.3} win={} scaled={r_scaled:.3} crop+scaled={r_crop_scaled:.3} reused={r_crop_scaled_reused:.3} {p}",
            r0,
            if win.is_some() { "Y" } else { "N" }
        );
        let _ = std::fs::remove_file(&sc);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(not(feature = "local-infer"))]
fn main() {
    eprintln!("需要 --features local-infer");
    std::process::exit(2);
}
