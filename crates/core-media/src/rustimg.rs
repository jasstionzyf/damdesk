//! 纯 Rust `image` 后端（Windows）。libvips 无 Windows 交叉编译 SDK（D13），
//! 用 image crate 复刻三个入口的语义；公共 API 与 vips 后端一致。
//!
//! 语义对齐说明：
//! - `thumbnail_to`：等比缩到**长边 ≤ size**（只缩不放），目标宽高由共享 `crate::thumb_dims`
//!   计算，与 vips 后端 `ops::thumbnail` 同语义（DESKTOP-THUMB-PORTRAIT-1：旧实现按宽度
//!   约束，竖图 800×3000 + 256 出 256×960，与 mac/Linux 的 68×256 差一个量级，已修复）。
//! - `luma32`：vips 先缩到短边 ≤32 再灰度 → 这里同样短边 ≤32（Lanczos3），灰度后走
//!   共用 `fit_grid_32`（fit + 居中灰底补边，契约 32×32=1024 字节不变）。
//! - 缩略图输出 webp（与 vips webpsave 对齐，前端 <img> 消费同一格式）。

use std::path::Path;

use image::imageops::FilterType;

use crate::{fit_grid_32, MediaError, Result, PHASH_GRID};

/// EXIF Orientation 原值（1..8；读不到 = 1）。
fn orientation_of(path: &Path) -> u32 {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    crate::exif::orientation_of_file(path, &ext)
        .and_then(|o| if (1..=8).contains(&o) { Some(o as u32) } else { None })
        .unwrap_or(1)
}

/// 解码 + EXIF 方向摆正（与 vips 后端 thumbnail 的 auto-rotate 对齐——
/// EXIF2 跨后端一致性裁决：**派生图路径（缩略图/VL/pHash/缩放）一律摆正输出**，
/// `rgb8` 例外（与 vips rgb8「不旋转」契约一致，跨平台像素统计可比）。
fn open_oriented(path: &Path) -> Result<image::DynamicImage> {
    let img = open(path)?;
    let o = orientation_of(path);
    if o == 1 {
        Ok(img)
    } else {
        Ok(crate::exif::apply_orientation(img, o))
    }
}

fn open(path: &Path) -> Result<image::DynamicImage> {
    image::ImageReader::open(path)?
        .with_guessed_format()
        .map_err(|e| MediaError::Img(e.to_string()))?
        .decode()
        .map_err(|e| MediaError::Img(e.to_string()))
}

/// 后端初始化空实现：纯 Rust `image` 后端没有进程级全局状态，与 vips 后端同签名。
pub fn init() -> Result<()> {
    Ok(())
}

/// 图片像素尺寸（宽, 高）。
pub fn image_size(path: &Path) -> Result<(i32, i32)> {
    let img = open(path)?;
    Ok((img.width() as i32, img.height() as i32))
}

/// 生成缩略图写盘：**长边 ≤ size 的等比内接，只缩不放**（`ops::thumbnail` 实测语义；
/// 目标宽高的统一口径见共享 `crate::thumb_dims` —— DESKTOP-THUMB-PORTRAIT-1 竖图语义
/// 双后端统一），不裁切，保持纵横比，格式 webp。
/// EXIF2：EXIF 方向自动摆正（与 vips 后端 thumbnail 对齐）—— 摆正后再算目标宽高，
/// 与 vips 的「thumbnail 自带 auto-rotate」同一语义（DESKTOP-THUMB-PORTRAIT-1 的
/// 长边口径按摆正后的画面算）。
pub fn thumbnail_to(path: &Path, dest: &Path, size: i32) -> Result<()> {
    let img = open_oriented(path)?;
    let (dw, dh) = crate::thumb_dims(img.width() as i32, img.height() as i32, size);
    let out = if (dw, dh) == (img.width(), img.height()) {
        img
    } else {
        img.resize_exact(dw, dh, FilterType::Lanczos3)
    };
    out.save_with_format(dest, image::ImageFormat::WebP)
        .map_err(|e| MediaError::Img(e.to_string()))?;
    Ok(())
}

/// VL 打标用的 JPEG（内存，不写盘）：**长边 = `target_long_side`**、等比不裁切、q=85。
/// 与 vips 后端语义一致（EXIF2 起方向差异也已消除：这里同样自动摆正）。
pub fn vl_jpeg(path: &Path, target_long_side: i32) -> Result<Vec<u8>> {
    let img = open_oriented(path)?;
    let (w, h) = (img.width(), img.height());
    let long = w.max(h) as i32;
    let out = if long != target_long_side && long > 0 {
        let scale = target_long_side as f64 / long as f64;
        img.resize_exact(
            ((w as f64 * scale).round().max(1.0)) as u32,
            ((h as f64 * scale).round().max(1.0)) as u32,
            FilterType::Lanczos3,
        )
    } else {
        img
    };
    let mut buf: Vec<u8> = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85)
        .encode_image(&out.to_rgb8())
        .map_err(|e| MediaError::Img(e.to_string()))?;
    Ok(buf)
}

/// 全分辨率 RGBA8 解码（N6b `/op/encode` 底座）。契约与 vips 后端 `rgba8` 一致：
/// 3 通道 → alpha 补 255（`to_rgba8` 语义）；EXIF 方向不在此旋转。
pub fn rgba8(path: &Path) -> Result<(i32, i32, Vec<u8>)> {
    let img = open(path)?;
    let (w, h) = (img.width() as i32, img.height() as i32);
    Ok((w, h, img.to_rgba8().into_raw()))
}

/// 全分辨率 RGB8 解码（C2 compliance classic 检测器像素档输入）。
/// 契约：返回 (宽, 高, 交错 RGB，len = w*h*3)。EXIF 方向不自动旋转（与 vips 后端
/// `rgb8` 一致，跨平台统计可比）；alpha 由 `to_rgb8` 直接丢弃（不做白底合成）。
pub fn rgb8(path: &Path) -> Result<(i32, i32, Vec<u8>)> {
    let img = open(path)?;
    let (w, h) = (img.width() as i32, img.height() as i32);
    Ok((w, h, img.to_rgb8().into_raw()))
}

/// 等比缩小：**长边 ≤ max_side，只缩不放**，按 dest 扩展名落盘
/// （jpg/jpeg/png/webp，其余显式报错）。返回输出 (宽, 高)。与 vips 后端同语义：
/// 无需缩小时同格式原样复制；跨格式仅转码不缩放。
pub fn resize_to(path: &Path, dest: &Path, max_side: i32) -> Result<(i32, i32)> {
    let img = open_oriented(path)?;
    let (ow, oh) = (img.width() as i32, img.height() as i32);
    let ext = dest
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let fmt = match ext.as_str() {
        "webp" => image::ImageFormat::WebP,
        "jpg" | "jpeg" => image::ImageFormat::Jpeg,
        "png" => image::ImageFormat::Png,
        other => {
            return Err(MediaError::Img(format!(
                "resize_to: 不支持的输出格式 .{other}"
            )))
        }
    };
    let long = ow.max(oh);
    let out = if long > max_side && long > 0 {
        let scale = max_side as f64 / long as f64;
        img.resize_exact(
            ((ow as f64 * scale).round().max(1.0)) as u32,
            ((oh as f64 * scale).round().max(1.0)) as u32,
            FilterType::Lanczos3,
        )
    } else {
        img
    };
    let (w, h) = (out.width() as i32, out.height() as i32);
    out.save_with_format(dest, fmt)
        .map_err(|e| MediaError::Img(e.to_string()))?;
    Ok((w, h))
}

/// 等比缩放（双向）：长边缩放到 long_side，可放大也可缩小，Lanczos3；
/// 按 dest 扩展名落盘（jpg/jpeg/png/webp）。素材准备用（把短边抬到规则门槛之上）。
pub fn scale_to(path: &Path, dest: &Path, long_side: i32) -> Result<(i32, i32)> {
    if long_side <= 0 {
        return Err(MediaError::Img(format!("scale_to: 非法长边 {long_side}")));
    }
    let img = open(path)?;
    let (ow, oh) = (img.width() as i32, img.height() as i32);
    let ext = dest
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let fmt = match ext.as_str() {
        "webp" => image::ImageFormat::WebP,
        "jpg" | "jpeg" => image::ImageFormat::Jpeg,
        "png" => image::ImageFormat::Png,
        other => {
            return Err(MediaError::Img(format!(
                "scale_to: 不支持的输出格式 .{other}"
            )))
        }
    };
    let long = ow.max(oh).max(1);
    let scale = long_side as f64 / long as f64;
    let out = img.resize_exact(
        ((ow as f64 * scale).round().max(1.0)) as u32,
        ((oh as f64 * scale).round().max(1.0)) as u32,
        FilterType::Lanczos3,
    );
    let (w, h) = (out.width() as i32, out.height() as i32);
    out.save_with_format(dest, fmt)
        .map_err(|e| MediaError::Img(e.to_string()))?;
    Ok((w, h))
}

/// 裁剪矩形（x, y, w, h，像素坐标，越界即报错），按 dest 扩展名落盘，与 vips 后端
/// `crop_to` 同语义（smart_crop 修复插件用，C3）。
pub fn crop_to(path: &Path, dest: &Path, x: i32, y: i32, w: i32, h: i32) -> Result<(i32, i32)> {
    if x < 0 || y < 0 || w <= 0 || h <= 0 {
        return Err(MediaError::Img(format!(
            "crop_to: 非法矩形 x={x} y={y} w={w} h={h}"
        )));
    }
    let img = open(path)?;
    let (ow, oh) = (img.width() as i32, img.height() as i32);
    if x + w > ow || y + h > oh {
        return Err(MediaError::Img(format!(
            "crop_to: 矩形 {x},{y} {w}x{h} 越界（源 {ow}x{oh}）"
        )));
    }
    let ext = dest
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let fmt = match ext.as_str() {
        "webp" => image::ImageFormat::WebP,
        "jpg" | "jpeg" => image::ImageFormat::Jpeg,
        "png" => image::ImageFormat::Png,
        other => {
            return Err(MediaError::Img(format!(
                "crop_to: 不支持的输出格式 .{other}"
            )))
        }
    };
    let rgb = img.to_rgb8();
    let out = image::imageops::crop_imm(&rgb, x as u32, y as u32, w as u32, h as u32).to_image();
    image::DynamicImage::ImageRgb8(out)
        .save_with_format(dest, fmt)
        .map_err(|e| MediaError::Img(e.to_string()))?;
    Ok((w, h))
}

/// 32x32 灰度网格（core-hash::phash 的输入），契约 1024 字节。
/// EXIF 方向自动摆正（EXIF2，与 vips 缩略链路对齐，pHash 跨平台可比）。
pub fn luma32(path: &Path) -> Result<Vec<u8>> {
    let img = open_oriented(path)?;
    let (w, h) = (img.width(), img.height());
    let gray = if w.min(h) > PHASH_GRID as u32 {
        let scale = PHASH_GRID as f64 / w.min(h) as f64;
        let nw = (((w as f64) * scale).round() as u32).max(1);
        let nh = (((h as f64) * scale).round() as u32).max(1);
        img.resize_exact(nw, nh, FilterType::Lanczos3).to_luma8()
    } else {
        img.to_luma8()
    };
    Ok(fit_grid_32(
        gray.as_raw(),
        gray.width() as i32,
        gray.height() as i32,
    ))
}
