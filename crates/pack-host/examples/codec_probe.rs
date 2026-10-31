//! 编码组（N6b）语料验收用的像素探针：**只读文件、打印 JSON**，不做任何判定。
//!
//! 为什么要有它：插件的语料 E2E 跑在 python（容器里没有 PIL/numpy，也不该为测试
//! 往共享容器里装库），而「格式对不对、alpha 有没有保留、压缩后糊没糊」必须看**真像素**。
//! 这个 example 用 `image` crate 解码 + 纯 Rust 计算，把结论以 JSON 交给 python 断言：
//!
//! ```text
//! cargo run -q -p pack-host --example codec_probe -- info      <path>
//! cargo run -q -p pack-host --example codec_probe -- alpha-sig <path>
//! cargo run -q -p pack-host --example codec_probe -- ssim      <a> <b>
//! cargo run -q -p pack-host --example codec_probe -- ssim-to   <ref> <out>
//! ```
//!
//! SSIM 口径（写死，避免"每台机一个数"）：
//! - 亮度 `Y = 0.299R + 0.587G + 0.114B`（ITU-R BT.601，与 skimage 的
//!   `rgb2gray` 同系数），`data_range = 255`；
//! - 高斯窗 11×11 / σ=1.5（`skimage.metrics.structural_similarity` 的
//!   `gaussian_weights=True, sigma=1.5` 同窗口），边界裁掉 `(11-1)/2 = 5` 像素；
//! - `C1 = (0.01·255)²`、`C2 = (0.03·255)²`；`use_sample_covariance=False`
//!   （不做 N/(N-1) 修正——口径写在这里，报告里也写同一句）。
//!
//! `ssim-to` 是「压缩类」验收的正解：降采样后的产物与原图**天然不同尺寸**，
//! 拿原图当参考算 SSIM 会把"重采样"当成"糊图"；先把参考图按产物尺寸 lanczos3
//! 重采样，再比同尺寸，量的才是**编码损失**。

use std::path::Path;

use image::imageops::{resize, FilterType};
use image::{DynamicImage, GenericImageView, RgbaImage};
use serde_json::json;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("用法：codec_probe <info|alpha-sig|ssim|ssim-to> <path> [path2]");
        std::process::exit(2);
    }
    let mode = args[0].as_str();
    match mode {
        "info" => info(Path::new(&args[1])),
        "alpha-sig" => alpha_sig(Path::new(&args[1])),
        "ssim" | "ssim-to" => {
            if args.len() < 3 {
                eprintln!("{mode} 需要两个路径");
                std::process::exit(2);
            }
            compare(mode, Path::new(&args[1]), Path::new(&args[2]));
        }
        other => {
            eprintln!("未知模式：{other}");
            std::process::exit(2);
        }
    }
}

fn load(path: &Path) -> DynamicImage {
    image::open(path).unwrap_or_else(|e| {
        eprintln!("解码失败 {}：{e}", path.display());
        std::process::exit(3);
    })
}

/// 魔数级格式判定（不信任扩展名）。
fn magic_format(path: &Path) -> &'static str {
    let raw = std::fs::read(path).unwrap_or_default();
    if raw.len() >= 3 && raw[0] == 0xFF && raw[1] == 0xD8 {
        return "jpg";
    }
    if raw.len() >= 8 && &raw[..8] == b"\x89PNG\r\n\x1a\n" {
        return "png";
    }
    if raw.len() >= 12 && &raw[..4] == b"RIFF" && &raw[8..12] == b"WEBP" {
        return "webp";
    }
    if raw.len() >= 12 && &raw[4..8] == b"ftyp" {
        let brand = String::from_utf8_lossy(&raw[8..12]).to_string();
        if brand.starts_with("avif") || brand.starts_with("avis") {
            return "avif";
        }
        return "isobmff";
    }
    "unknown"
}

fn info(path: &Path) {
    let img = load(path);
    let (w, h) = img.dimensions();
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let rgba = img.to_rgba8();
    let has_alpha = rgba.pixels().any(|p| p.0[3] != 255);
    println!(
        "{}",
        json!({
            "path": path.to_string_lossy(),
            "magic": magic_format(path),
            "width": w,
            "height": h,
            "bytes": bytes,
            "hasAlpha": has_alpha,
        })
    );
}

fn alpha_sig(path: &Path) {
    let img = load(path);
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut alphas: Vec<u8> = Vec::with_capacity((w * h) as usize);
    let mut below = 0usize;
    let mut min = 255u8;
    let mut max = 0u8;
    for p in rgba.pixels() {
        let a = p.0[3];
        alphas.push(a);
        if a < 255 {
            below += 1;
        }
        min = min.min(a);
        max = max.max(a);
    }
    let sum: u64 = alphas.iter().map(|&a| a as u64).sum();
    // 简单自实现的 sha256（只对 alpha 平面）：用文件级 sha256 代替更省事，
    // 但这里要的是"alpha 通道指纹"，所以按 16 位分组做 FNV-1a 累加即可稳定比对。
    let mut fp: u64 = 0xcbf29ce484222325;
    for a in &alphas {
        fp ^= *a as u64;
        fp = fp.wrapping_mul(0x100000001b3);
    }
    println!(
        "{}",
        json!({
            "path": path.to_string_lossy(),
            "width": w,
            "height": h,
            "pixels": w * h,
            "alphaMin": min,
            "alphaMax": max,
            "alphaBelow255": below,
            "alphaMean": (sum as f64) / (alphas.len().max(1) as f64),
            "alphaFingerprint": format!("{fp:016x}"),
        })
    );
}

fn compare(mode: &str, a: &Path, b: &Path) {
    let ia = load(a);
    let ib = load(b);
    let (aw, ah) = ia.dimensions();
    let (bw, bh) = ib.dimensions();
    let luma_a: Vec<f64> = to_luma(&ia);
    let luma_b: Vec<f64> = to_luma(&ib);
    let (rw, rh) = (bw as usize, bh as usize);
    let (ssim, mad, maxd) = if (aw, ah) == (bw, bh) {
        let (m, x) = diff_stats_rgba(&ia.to_rgba8(), &ib.to_rgba8());
        (ssim_map(&luma_a, &luma_b, rw, rh), m, x)
    } else if mode == "ssim-to" && bw > 0 && bh > 0 {
        // 参考按产物尺寸重采样（lanczos3）后再比：量的只是编码损失
        let ref_small = resize(&ia.to_rgba8(), bw, bh, FilterType::Lanczos3);
        let ref_luma = to_luma(&DynamicImage::ImageRgba8(ref_small.clone()));
        let (m, x) = diff_stats_rgba(&ref_small, &ib.to_rgba8());
        (ssim_map(&ref_luma, &luma_b, rw, rh), m, x)
    } else {
        eprintln!("尺寸不一致（{aw}x{ah} vs {bw}x{bh}），ssim 需要同尺寸；用 ssim-to 先对齐");
        std::process::exit(4);
    };
    println!(
        "{}",
        json!({
            "ref": a.to_string_lossy(),
            "out": b.to_string_lossy(),
            "refSize": [aw, ah],
            "outSize": [bw, bh],
            "ssim": ssim,
            "meanAbsDiff": mad,
            "maxAbsDiff": maxd,
            "sizesMatched": (aw, ah) == (bw, bh),
        })
    );
}

fn to_luma(img: &DynamicImage) -> Vec<f64> {
    let rgba = img.to_rgba8();
    rgba.pixels()
        .map(|p| 0.299 * p.0[0] as f64 + 0.587 * p.0[1] as f64 + 0.114 * p.0[2] as f64)
        .collect()
}

fn diff_stats_rgba(a: &RgbaImage, b: &RgbaImage) -> (f64, u8) {
    let (w, h) = a.dimensions();
    let mut sum = 0f64;
    let mut maxd = 0u8;
    let n = (w as u64) * (h as u64) * 3;
    for (pa, pb) in a.pixels().zip(b.pixels()) {
        for c in 0..3 {
            let d = (pa.0[c] as i32 - pb.0[c] as i32).unsigned_abs() as u8;
            sum += d as f64;
            maxd = maxd.max(d);
        }
    }
    ((sum / (n.max(1) as f64)), maxd)
}

/// 分离式高斯卷积（11 抽头，σ=1.5），边界用「镜像」而不是零填充
/// （零填充会在图像边上造出假的强结构差异，SSIM 边上会异常低）。
fn gauss_kernel() -> Vec<f64> {
    const WIN: i32 = 11;
    const SIGMA: f64 = 1.5;
    let mut k: Vec<f64> = (-(WIN / 2)..=(WIN / 2))
        .map(|i| (-(i * i) as f64 / (2.0 * SIGMA * SIGMA)).exp())
        .collect();
    let sum: f64 = k.iter().sum();
    for v in k.iter_mut() {
        *v /= sum;
    }
    k
}

fn blur(src: &[f64], w: usize, h: usize, k: &[f64]) -> Vec<f64> {
    let r = (k.len() / 2) as isize;
    let mut tmp = vec![0f64; w * h];
    // 横向
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0f64;
            for (i, &kv) in k.iter().enumerate() {
                let sx = (x as isize + i as isize - r).clamp(0, w as isize - 1) as usize;
                acc += kv * src[y * w + sx];
            }
            tmp[y * w + x] = acc;
        }
    }
    // 纵向
    let mut out = vec![0f64; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0f64;
            for (i, &kv) in k.iter().enumerate() {
                let sy = (y as isize + i as isize - r).clamp(0, h as isize - 1) as usize;
                acc += kv * tmp[sy * w + x];
            }
            out[y * w + x] = acc;
        }
    }
    out
}

/// 同尺寸 SSIM（返回内部区域均值，边界裁 5 像素，见文件头口径）。
fn ssim_map(a: &[f64], b: &[f64], w: usize, h: usize) -> f64 {
    let k = gauss_kernel();
    let pad = k.len() / 2;
    let mu_a = blur(a, w, h, &k);
    let mu_b = blur(b, w, h, &k);
    let aa: Vec<f64> = a.iter().map(|v| v * v).collect();
    let bb: Vec<f64> = b.iter().map(|v| v * v).collect();
    let ab: Vec<f64> = a.iter().zip(b.iter()).map(|(x, y)| x * y).collect();
    let s_aa = blur(&aa, w, h, &k);
    let s_bb = blur(&bb, w, h, &k);
    let s_ab = blur(&ab, w, h, &k);
    let c1 = (0.01 * 255.0f64).powi(2);
    let c2 = (0.03 * 255.0f64).powi(2);
    let mut sum = 0f64;
    let mut n = 0usize;
    for y in pad..h.saturating_sub(pad) {
        for x in pad..w.saturating_sub(pad) {
            let i = y * w + x;
            let ma = mu_a[i];
            let mb = mu_b[i];
            let va = s_aa[i] - ma * ma;
            let vb = s_bb[i] - mb * mb;
            let cov = s_ab[i] - ma * mb;
            let s = ((2.0 * ma * mb + c1) * (2.0 * cov + c2))
                / ((ma * ma + mb * mb + c1) * (va + vb + c2));
            sum += s;
            n += 1;
        }
    }
    if n == 0 {
        return 0.0;
    }
    sum / n as f64
}
