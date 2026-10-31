//! `vl_image::load_for_vl` 的回归测试（`DESKTOP-TAG-CTX-1`）。
//!
//! 契约（2026-10-15 裁决：按**短边 512** 归一，不是长边 768）：
//! 1. 任意尺寸 → 输出 JPEG 且**短边 == 512**（等比、不裁切；小图放大）；
//! 2. 极端长图（短边规则会给出超长边）→ 被 `VL_MAX_LONG_SIDE`(4096) 兜住；
//! 3. 解码失败（非图片字节）→ 回退原文件字节 + 按扩展名猜 mime（打标不因此变失败）；
//! 4. token 预算：512² = 262,144 px ≈ 334 token @28px/patch，远低于 8192。

use std::path::PathBuf;

/// vips 与自己算的取整可能差 1~2 px，断言留容差。
fn assert_short_side(w: u32, h: u32, want: u32, ctx: &str) {
    let got = w.min(h);
    assert!(
        (got as i64 - want as i64).abs() <= 4,
        "{ctx}: 短边应 ≈ {want}，实得 {w}x{h}"
    );
}

fn tmp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("vl-image-{}-{}", std::process::id(), name));
    p
}

fn write_flat(path: &PathBuf, w: u32, h: u32) {
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
    });
    img.save_with_format(path, image::ImageFormat::Png).unwrap();
}

fn decode(bytes: &[u8]) -> image::DynamicImage {
    image::load_from_memory(bytes).expect("输出必须是可解码图片")
}

#[test]
fn large_square_normalized_to_short_side() {
    let src = tmp("sq3072.png");
    write_flat(&src, 3072, 3072);
    let raw = std::fs::read(&src).unwrap();

    let (bytes, mime) = engine_tag::vl_image::load_for_vl(&src).unwrap();
    assert_eq!(mime, "image/jpeg");
    assert_eq!(&bytes[..2], &[0xFF, 0xD8], "应为 JPEG（SOI 头）");
    let img = decode(&bytes);
    assert_short_side(img.width(), img.height(), 512, "3072² ");
    assert_eq!((img.width(), img.height()), (512, 512), "方图应正好 512²");
    assert!(bytes.len() * 8 < raw.len(), "3072² PNG → 512² JPEG 应显著变小");
    let _ = std::fs::remove_file(&src);
}

#[test]
fn portrait_and_landscape_keep_aspect_on_short_side() {
    let portrait = tmp("portrait800x3000.png");
    write_flat(&portrait, 800, 3000);
    let (bytes, _) = engine_tag::vl_image::load_for_vl(&portrait).unwrap();
    let img = decode(&bytes);
    assert_short_side(img.width(), img.height(), 512, "800×3000 ");
    assert_eq!(img.width(), 512, "短边是宽：应缩到宽 512");
    assert!(
        (img.height() as i64 - 1920).abs() <= 4,
        "等比高应 ≈ 1920，实得 {}",
        img.height()
    );
    let _ = std::fs::remove_file(&portrait);

    let landscape = tmp("landscape3000x800.png");
    write_flat(&landscape, 3000, 800);
    let (bytes, _) = engine_tag::vl_image::load_for_vl(&landscape).unwrap();
    let img = decode(&bytes);
    assert_eq!(img.height(), 512, "短边是高：应缩到高 512");
    assert!((img.width() as i64 - 1920).abs() <= 4, "宽应 ≈ 1920");
    let _ = std::fs::remove_file(&landscape);
}

#[test]
fn small_image_is_upscaled_to_short_side() {
    let src = tmp("small200x150.png");
    write_flat(&src, 200, 150);
    let (bytes, mime) = engine_tag::vl_image::load_for_vl(&src).unwrap();
    assert_eq!(mime, "image/jpeg");
    let img = decode(&bytes);
    assert_eq!(img.height(), 512, "小图也放大到短边 512");
    assert!((img.width() as i64 - 683).abs() <= 4, "宽应 ≈ 683");
    let _ = std::fs::remove_file(&src);
}

#[test]
fn extreme_pano_capped_by_long_side() {
    let src = tmp("pano512x20000.png");
    write_flat(&src, 512, 20000);
    let (bytes, _) = engine_tag::vl_image::load_for_vl(&src).unwrap();
    let img = decode(&bytes);
    assert!(
        img.height() <= engine_tag::vl_image::VL_MAX_LONG_SIDE as u32 + 4,
        "极端长图长边须被 4096 兜住，实得 {}x{}",
        img.width(),
        img.height()
    );
    let _ = std::fs::remove_file(&src);
}

#[test]
fn undecodable_file_falls_back_to_raw_bytes() {
    let src = tmp("broken.png");
    let junk = vec![0x00u8, 0x01, 0x02, 0x03, 0xFF, 0xFE];
    std::fs::write(&src, &junk).unwrap();
    let (bytes, mime) = engine_tag::vl_image::load_for_vl(&src).unwrap();
    assert_eq!(bytes, junk, "解码失败须回退原字节（不能把打标变成失败）");
    assert_eq!(mime, "image/png", "mime 仍按扩展名给出");
    let _ = std::fs::remove_file(&src);
}
