//! L0 `core-media` — 图片解码、缩略图（256/1024 两档）、32x32 灰度网格。
//!
//! 后端裁决（D13 Windows 打包）：Linux/macOS 默认 libvips（D6 契约回归不动）；
//! Windows 无 libvips 交叉编译 SDK → 纯 Rust `image` 后端，公共 API 完全一致。
//! DESKTOP-THUMB-PORTRAIT-1：Linux/macOS 可用 feature `rustimg-backend` 强制启用
//! rustimg 后端，在无 Windows 真机的环境下单测/验收纯 Rust 缩略图路径（同一份代码）。
#[cfg(any(target_os = "windows", feature = "rustimg-backend"))]
#[path = "rustimg.rs"]
mod backend;
#[cfg(all(not(target_os = "windows"), not(feature = "rustimg-backend")))]
#[path = "vips.rs"]
mod backend;

pub use backend::{
    crop_to, image_size, init as init_backend, luma32, resize_to, rgb8, rgba8, scale_to,
    thumbnail_to, vl_jpeg,
};
// C4 do 型变换底座：共享纯 Rust 编码 + 文字水印（双后端同语义）
mod encode;
mod watermark;
pub use encode::{
    encode_rgb_to, encode_rgba_to, has_transparency, orient_rgba, rgba_to_rgb_with_background,
    OutFormat,
};
pub use watermark::{coverage, watermark_text, TextWatermark, WatermarkPosition};

// EXIF2：元数据解析（kamadak-exif 纯 Rust）+ 方向摆正纯函数（双后端共用）
pub mod exif;
pub use exif::{
    apply_orientation, orientation_of_file, oriented_dims, parse_file, strip_gps_jpeg, ExifSummary,
    EXIF_PARSER,
};

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("media backend init failed")]
    Init,
    #[error("libvips: {0}")]
    Vips(String),
    #[error("image: {0}")]
    Img(String),
    #[error("exif: {0}")]
    Exif(String),
    #[error("exif: unsupported container {0}")]
    ExifUnsupported(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, MediaError>;

/// 缩略图档位（M1：256 / 1024 两档）。
pub const THUMB_SIZES: &[i32] = &[256, 1024];

/// 当前编译选中的媒体后端（"vips" | "rustimg"）。Windows 目标平台恒 rustimg；
/// Linux/macOS 默认 vips，feature `rustimg-backend` 强制切 rustimg（单测/验收标注用）。
pub const BACKEND: &str = if cfg!(any(target_os = "windows", feature = "rustimg-backend")) {
    "rustimg"
} else {
    "vips"
};

/// 缩略图目标尺寸统一语义（DESKTOP-THUMB-PORTRAIT-1）：**长边 ≤ size 的等比内接，只缩不放**。
/// vips 后端 `ops::thumbnail(size)` 实测即该语义（直接复用，vips 构建下本函数仅测试引用）；
/// rustimg 后端由此函数算目标宽高，两平台竖图/横图/方图出图尺寸一致
/// （公共 API `thumbnail_to` 签名不变）。
/// 竖图例：800×3000 + 256 → 68×256（旧 rustimg 宽度约束出 256×960，Windows 包与
/// mac/Linux 缩略图尺寸差一个量级，已修复）。
#[allow(dead_code)] // vips 构建（非 test）下由 ops::thumbnail 直供长边语义，本函数仅 rustimg 构建与测试引用
pub(crate) fn thumb_dims(w: i32, h: i32, size: i32) -> (u32, u32) {
    let (w, h) = (w.max(1), h.max(1));
    let long = w.max(h);
    if size <= 0 || long <= size {
        // 只缩不放：非法档位或无需缩小时原样保留
        return (w as u32, h as u32);
    }
    let scale = size as f64 / long as f64;
    let dw = ((w as f64 * scale).round() as i32).clamp(1, size);
    let dh = ((h as f64 * scale).round() as i32).clamp(1, size);
    (dw as u32, dh as u32)
}

/// pHash 灰度网格边长（core-hash::phash 只接受 32×32=1024 字节）。
pub const PHASH_GRID: i32 = 32;

/// 把任意宽高的一通道灰度缓冲等比缩放进 32×32 画布（居中，余量填中灰 128）。
/// 纯 Rust 采样，确定性、无额外 libvips 调用；输入已是 32×32 时逐字节不变。
/// 两后端共用。
pub(crate) fn fit_grid_32(raw: &[u8], w: i32, h: i32) -> Vec<u8> {
    let side = PHASH_GRID as usize;
    let mut out = vec![128u8; side * side];
    if w <= 0 || h <= 0 || raw.len() < (w as usize) * (h as usize) {
        return out;
    }
    let scale = f64::min(PHASH_GRID as f64 / w as f64, PHASH_GRID as f64 / h as f64);
    let dw = ((w as f64 * scale).round() as i32).clamp(1, PHASH_GRID);
    let dh = ((h as f64 * scale).round() as i32).clamp(1, PHASH_GRID);
    let ox = (PHASH_GRID - dw) / 2;
    let oy = (PHASH_GRID - dh) / 2;
    for y in 0..dh {
        for x in 0..dw {
            let sx = (((x as f64 + 0.5) * w as f64 / dw as f64) as usize).min(w as usize - 1);
            let sy = (((y as f64 + 0.5) * h as f64 / dh as f64) as usize).min(h as usize - 1);
            out[(oy + y) as usize * side + (ox + x) as usize] = raw[sy * w as usize + sx];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EXIF2 跨后端一致性（验收 #1）：竖拍图（存储 4032x3024 + Orientation=6）
    /// 三个契约在同一后端先对齐，双后端再共用同一 apply_orientation 纯函数：
    ///   ① thumbnail 输出是竖图（h > w）
    ///   ② oriented_dims 给出的显示宽高 = 竖图（3000x4032）
    ///   ③ rustimg 派生链路调用同一 apply_orientation（代码单源，Windows 同语义）
    /// 注：Linux 容器无法编译 rustimg（Windows 后端），像素级对拍列报告「未覆盖」。
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn portrait_orientation_backend_consistency() {
        let dir = std::env::temp_dir().join(format!("dam_orient_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tiff = exif::tests::phone_tiff();
        let src = dir.join("portrait.jpg");
        std::fs::write(&src, exif::tests::jpeg_with_exif(&tiff, 4032, 3024)).unwrap();
        let ext = "jpg";

        // 解析层：orientation=6，oriented 宽高互换
        let summary = exif::parse_file(&src, ext, false).unwrap().unwrap();
        assert_eq!(summary.orientation, Some(6));
        assert_eq!(
            exif::oriented_dims(4032, 3024, summary.orientation),
            (3024, 4032)
        );

        // vips 后端：缩略图输出必须是竖图（摆正后的画面）
        let thumb = dir.join("t256.webp");
        backend_thumb(&src, &thumb);
        let (tw, th) = image_size(&thumb).unwrap();
        assert!(th > tw, "缩略图应为竖图，得 {tw}x{th}");

        // 共享纯函数与 vips 输出同语义：同方向（竖）
        let img = image::DynamicImage::new_rgb8(4032, 3024);
        let rot = exif::apply_orientation(img, 6);
        assert_eq!((rot.width(), rot.height()), (3024, 4032));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(not(target_os = "windows"))]
    fn backend_thumb(src: &std::path::Path, dest: &std::path::Path) {
        thumbnail_to(src, dest, 256).unwrap();
    }

    /// 方形输入保持逐字节不变（等价旧行为，避免存量方形 hash 语义漂移）。
    #[test]
    fn fit_grid_32_keeps_square_input_byte_identical() {
        let raw: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();
        assert_eq!(fit_grid_32(&raw, PHASH_GRID, PHASH_GRID), raw);
    }

    /// scale_to（素材准备）：长边目标放大与缩小都能做，短边按比例跟进。
    #[test]
    fn scale_to_upscales_and_downscales() {
        let dir = std::env::temp_dir().join(format!("dam-scale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = write_test_jpeg(&dir, "src.jpg", 200, 100);
        let up = dir.join("up.jpg");
        let (uw, uh) = scale_to(&src, &up, 700).unwrap();
        assert_eq!((uw, uh), (700, 350), "放大：长边 700，短边按比例");
        let down = dir.join("down.jpg");
        let (dw, dh) = scale_to(&src, &down, 100).unwrap();
        assert_eq!((dw, dh), (100, 50), "缩小：长边 100");
        assert!(scale_to(&src, &up, 0).is_err());
        assert!(scale_to(&src, &dir.join("bad.tiff"), 700).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// crop_to（C3 smart_crop 的像素裁剪底座）：精确裁剪 + 越界/非法矩形响亮报错。
    /// 源图用手工 PPM（P6，vips 后端可解码；Windows rustimg 后端与 Linux 的
    /// `--features rustimg-backend` 验证构建均无 pnm → 只在 vips 构建下跑此测试）。
    #[cfg(all(not(target_os = "windows"), not(feature = "rustimg-backend")))]
    #[test]
    fn crop_to_clips_and_validates() {
        let dir = std::env::temp_dir().join(format!("dam-crop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (w, h) = (64i32, 48i32);
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        for y in 0..h {
            for x in 0..w {
                ppm.extend_from_slice(&[(x * 4) as u8, (y * 5) as u8, 128]);
            }
        }
        let src = dir.join("src.ppm");
        std::fs::write(&src, &ppm).unwrap();
        let out = dir.join("crop.jpg");
        let (ow, oh) = crop_to(&src, &out, 10, 5, 20, 12).unwrap();
        assert_eq!((ow, oh), (20, 12));
        let (cw, ch) = image_size(&out).unwrap();
        assert_eq!((cw, ch), (20, 12));
        // 越界矩形 / 非法矩形 → 显式报错（不静默钳制，坐标是算出来的，钳制掩盖 bug）
        assert!(crop_to(&src, &out, 60, 40, 20, 20).is_err());
        assert!(crop_to(&src, &out, -1, 0, 10, 10).is_err());
        assert!(crop_to(&src, &out, 0, 0, 0, 10).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 造测试源图（JPEG，dev-dep image crate；两后端均可解码）。
    fn write_test_jpeg(dir: &std::path::Path, name: &str, w: u32, h: u32) -> std::path::PathBuf {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let p = dir.join(name);
        image::DynamicImage::ImageRgb8(img)
            .save_with_format(&p, image::ImageFormat::Jpeg)
            .unwrap();
        p
    }

    /// DESKTOP-THUMB-PORTRAIT-1 回归：`thumbnail_to` 长边语义，竖/横/方 ≥3 例。
    /// 本测试在两种构建下各跑一遍（默认=vips；`--features rustimg-backend`=rustimg，
    /// Windows 目标平台同款路径），两后端出图尺寸必须一致且与共享 `thumb_dims` 吻合。
    #[test]
    fn thumbnail_to_long_side_semantics_all_shapes() {
        let dir = std::env::temp_dir().join(format!(
            "dam_thumb_{}_{}",
            std::process::id(),
            crate::BACKEND
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, w, h, ew, eh) in [
            ("portrait_800x3000", 800u32, 3000u32, 68u32, 256u32),
            ("portrait_512x1920", 512, 1920, 68, 256),
            ("landscape_3000x800", 3000, 800, 256, 68),
            ("square_1000x1000", 1000, 1000, 256, 256),
        ] {
            let src = write_test_jpeg(&dir, &format!("{name}.jpg"), w, h);
            let out = dir.join(format!("{name}-256.webp"));
            thumbnail_to(&src, &out, 256).unwrap_or_else(|e| panic!("{name}: {e}"));
            let (gw, gh) = image_size(&out).unwrap();
            assert_eq!(
                (gw as u32, gh as u32),
                (ew, eh),
                "{name}: 出图 {gw}x{gh}，预期 {ew}x{eh}（长边语义）"
            );
            let (dw, dh) = thumb_dims(w as i32, h as i32, 256);
            assert_eq!(
                (gw as u32, gh as u32),
                (dw, dh),
                "{name}: 与 thumb_dims 不符"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `vl_image` 短边 512 归一契约不受本次改动影响（独立契约）：
    /// engine-tag 把「短边 512」换算成 target_long_side 后调 `vl_jpeg`。
    /// 竖图 800×3000 → 短边 512 ⇔ 长边 1920，必须精确出 512×1920；
    /// 已达标的 512×1920 竖图不再重采样。
    #[test]
    fn vl_jpeg_short_side_512_contract_unaffected() {
        let dir = std::env::temp_dir().join(format!(
            "dam_vljpeg_{}_{}",
            std::process::id(),
            crate::BACKEND
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let src = write_test_jpeg(&dir, "vl_portrait_800x3000.jpg", 800, 3000);
        let bytes = vl_jpeg(&src, 1920).unwrap();
        let img = image::load_from_memory(&bytes).unwrap();
        assert_eq!((img.width(), img.height()), (512, 1920));
        let src2 = write_test_jpeg(&dir, "vl_portrait_512x1920.jpg", 512, 1920);
        let bytes2 = vl_jpeg(&src2, 1920).unwrap();
        let img2 = image::load_from_memory(&bytes2).unwrap();
        assert_eq!((img2.width(), img2.height()), (512, 1920));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 验收脚本（scripts/thumb_acceptance.sh）取数口：设 THUMB_ACCEPTANCE_DIR 时，
    /// 把 800×3000 竖图的 256 档出图与 result-<backend>.json 落盘，供脚本打印
    /// 两后端尺寸表并断言一致；平时同样跑断言，只是不落盘。
    #[test]
    fn thumbnail_acceptance_portrait() {
        let dir = std::env::temp_dir().join(format!(
            "dam_thumb_acc_{}_{}",
            std::process::id(),
            crate::BACKEND
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let src = write_test_jpeg(&dir, "portrait_800x3000.jpg", 800, 3000);
        let out = dir.join("portrait_800x3000-256.webp");
        thumbnail_to(&src, &out, 256).unwrap();
        let (gw, gh) = image_size(&out).unwrap();
        assert_eq!(
            (gw as u32, gh as u32),
            (68, 256),
            "竖图 800×3000 + 256 应为 68×256（长边语义），得 {gw}x{gh}"
        );
        if let Ok(acc) = std::env::var("THUMB_ACCEPTANCE_DIR") {
            let acc = std::path::PathBuf::from(acc);
            std::fs::create_dir_all(&acc).unwrap();
            std::fs::copy(&out, acc.join(format!("portrait-{}.webp", crate::BACKEND))).unwrap();
            std::fs::copy(&src, acc.join("portrait_800x3000-src.jpg")).unwrap();
            let json = format!(
                "{{\"backend\":\"{}\",\"src\":\"800x3000\",\"size\":256,\"out\":\"{}x{}\"}}\n",
                crate::BACKEND,
                gw,
                gh
            );
            std::fs::write(acc.join(format!("result-{}.json", crate::BACKEND)), json).unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 共享语义函数本身的边界例（不含后端，纯逻辑）。
    #[test]
    fn thumb_dims_long_side_semantics() {
        // 竖图（本次回归主体）
        assert_eq!(thumb_dims(800, 3000, 256), (68, 256));
        assert_eq!(thumb_dims(512, 1920, 256), (68, 256));
        assert_eq!(thumb_dims(800, 3000, 1024), (273, 1024));
        // 横图/方图与旧宽度约束口径像素级一致（不顺手改语义）
        assert_eq!(thumb_dims(3000, 800, 256), (256, 68));
        assert_eq!(thumb_dims(1000, 1000, 256), (256, 256));
        // 只缩不放
        assert_eq!(thumb_dims(100, 50, 256), (100, 50));
        assert_eq!(thumb_dims(256, 256, 256), (256, 256));
        // 防御：非法档位/尺寸不 panic
        assert_eq!(thumb_dims(100, 50, 0), (100, 50));
        assert_eq!(thumb_dims(0, 0, 256), (1, 1));
    }
}
