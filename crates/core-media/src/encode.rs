//! 共享纯 Rust 编码底座（C4 do 型变换）。
//!
//! 解码仍走各平台后端 `rgb8()`（vips / image 两后端同契约）；编码统一走 `image`
//! crate（纯 Rust，双平台逐字节同语义）——两后端差异只存在于解码侧，编码侧天然一致。
//!
//! 质量参数边界（诚实语义）：
//! - jpeg：`quality`（1..=100）生效（默认 85）；
//! - webp：`image`/`image-webp` 0.2 仅支持 **无损**（VP8L）编码——`quality` 不适用，
//!   输出无损 webp（文件可能大于有损 jpeg，语义是「转格式」不是「压缩」）；
//! - png：无损，`quality` 忽略；
//! - avif：`quality`（1..=100）生效（默认 80；AV1 有损 + 走 ravif/rav1e 纯 Rust）。
//!
//! alpha 边界（N6b 编码组，`/op/encode` 用的就是下面这两个函数）：
//! - 保留 alpha 的容器：png / webp / avif；**jpeg 没有 alpha** → 按 `background` 合成
//!   （默认白底），由 `encode_rgba_to` 显式做，绝不静默丢通道；
//! - EXIF 方向由纯函数 `orient_rgba` 在 RGBA8 缓冲上摆正（双后端同语义、可单测）。

use std::path::Path;

use image::ImageEncoder;

use crate::{MediaError, Result};

/// 输出格式（由目标文件扩展名决定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutFormat {
    Jpg,
    Png,
    Webp,
    Avif,
}

impl OutFormat {
    /// 扩展名 → 格式（未知扩展名显式报错，不猜）。
    pub fn from_ext(ext: &str) -> Result<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => Ok(Self::Jpg),
            "png" => Ok(Self::Png),
            "webp" => Ok(Self::Webp),
            "avif" => Ok(Self::Avif),
            other => Err(MediaError::Img(format!("不支持的输出格式: .{other}"))),
        }
    }

    /// 规范扩展名（jpeg → jpg）。
    pub fn ext(self) -> &'static str {
        match self {
            Self::Jpg => "jpg",
            Self::Png => "png",
            Self::Webp => "webp",
            Self::Avif => "avif",
        }
    }

    /// quality 是否对该格式有意义（compress 插件的响亮拒绝依据）。
    pub fn quality_applies(self) -> bool {
        matches!(self, Self::Jpg | Self::Avif)
    }

    /// 该容器能不能存 alpha（jpeg 不能 → 必须显式合成到背景色）。
    pub fn supports_alpha(self) -> bool {
        matches!(self, Self::Png | Self::Webp | Self::Avif)
    }
}

/// 把交错 RGB8 缓冲按目标扩展名编码写盘。
///
/// - `w`/`h` ≥ 1，`rgb.len() == (w*h)*3`，否则显式报错；
/// - `quality`：Some(1..=100) 对 jpeg 生效；None = 默认 85；png/webp 忽略。
pub fn encode_rgb_to(
    w: i32,
    h: i32,
    rgb: &[u8],
    dest: &Path,
    quality: Option<u8>,
) -> Result<(i32, i32)> {
    if w <= 0 || h <= 0 {
        return Err(MediaError::Img(format!("非法尺寸 {w}x{h}")));
    }
    let (w, h) = (w as u32, h as u32);
    if rgb.len() != (w as usize) * (h as usize) * 3 {
        return Err(MediaError::Img(format!(
            "RGB 缓冲长度 {} 与 {w}x{h}x3 不符",
            rgb.len()
        )));
    }
    let format = OutFormat::from_ext(
        dest.extension()
            .and_then(|e| e.to_str())
            .ok_or_else(|| MediaError::Img(format!("目标文件无扩展名: {}", dest.display())))?,
    )?;
    if let Some(q) = quality {
        if !(1..=100).contains(&q) {
            return Err(MediaError::Img(format!("quality 必须 1..=100，实际 {q}")));
        }
    }
    let file = std::fs::File::create(dest)?;
    let mut buf = std::io::BufWriter::new(file);
    match format {
        OutFormat::Jpg => {
            let q = quality.unwrap_or(85);
            let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, q);
            enc.write_image(rgb, w, h, image::ExtendedColorType::Rgb8)
                .map_err(|e| MediaError::Img(format!("jpeg 编码失败: {e}")))?;
        }
        OutFormat::Png => {
            let enc = image::codecs::png::PngEncoder::new(&mut buf);
            enc.write_image(rgb, w, h, image::ExtendedColorType::Rgb8)
                .map_err(|e| MediaError::Img(format!("png 编码失败: {e}")))?;
        }
        OutFormat::Webp => {
            // image-webp 0.2 仅无损（VP8L）；quality 无效——语义「转格式」非「压缩」
            let enc = image::codecs::webp::WebPEncoder::new_lossless(&mut buf);
            enc.write_image(rgb, w, h, image::ExtendedColorType::Rgb8)
                .map_err(|e| MediaError::Img(format!("webp 编码失败: {e}")))?;
        }
        OutFormat::Avif => {
            // speed 10 = 最快档（rav1e 变速；质量由 quality 控制，33MP 图 <=3s 量级）
            let q = quality.unwrap_or(80);
            let enc = image::codecs::avif::AvifEncoder::new_with_speed_quality(&mut buf, 10, q);
            enc.write_image(rgb, w, h, image::ExtendedColorType::Rgb8)
                .map_err(|e| MediaError::Img(format!("avif 编码失败: {e}")))?;
        }
    }
    use std::io::Write;
    buf.flush()?;
    Ok((w as i32, h as i32))
}

/// 把交错 RGBA8 缓冲按目标扩展名编码写盘（**alpha 感知**；`/op/encode` 的底座）。
///
/// - `rgba.len() == (w*h)*4`，否则显式报错；
/// - 目标容器支持 alpha（png/webp/avif）→ **原样保留 alpha 通道**（png/avif 逐字节保留，
///   webp 走无损 VP8L 也保留）；
/// - 目标容器不支持 alpha（jpeg）→ 按 `background`（默认白）做 source-over 合成，
///   合成公式 `(src*a + bg*(255-a) + 127) / 255`（四舍五入，双平台同值）；
/// - `quality`：jpeg/avif 生效（默认 85 / 80），png/webp 忽略。
pub fn encode_rgba_to(
    w: i32,
    h: i32,
    rgba: &[u8],
    dest: &Path,
    quality: Option<u8>,
    background: Option<[u8; 3]>,
) -> Result<(i32, i32)> {
    if w <= 0 || h <= 0 {
        return Err(MediaError::Img(format!("非法尺寸 {w}x{h}")));
    }
    let (wu, hu) = (w as u32, h as u32);
    let expect = (wu as usize) * (hu as usize) * 4;
    if rgba.len() != expect {
        return Err(MediaError::Img(format!(
            "RGBA 缓冲长度 {} 与 {wu}x{hu}x4 不符",
            rgba.len()
        )));
    }
    let format = OutFormat::from_ext(
        dest.extension()
            .and_then(|e| e.to_str())
            .ok_or_else(|| MediaError::Img(format!("目标文件无扩展名: {}", dest.display())))?,
    )?;
    if let Some(q) = quality {
        if !(1..=100).contains(&q) {
            return Err(MediaError::Img(format!("quality 必须 1..=100，实际 {q}")));
        }
    }
    let page = match format {
        OutFormat::Jpg => rgba_to_rgb_with_background(rgba, background.unwrap_or([255, 255, 255])),
        _ => rgba.to_vec(),
    };
    let file = std::fs::File::create(dest)?;
    let mut buf = std::io::BufWriter::new(file);
    match format {
        OutFormat::Jpg => {
            let q = quality.unwrap_or(85);
            let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, q);
            enc.write_image(&page, wu, hu, image::ExtendedColorType::Rgb8)
                .map_err(|e| MediaError::Img(format!("jpeg 编码失败: {e}")))?;
        }
        OutFormat::Png => {
            let enc = image::codecs::png::PngEncoder::new(&mut buf);
            enc.write_image(&page, wu, hu, image::ExtendedColorType::Rgba8)
                .map_err(|e| MediaError::Img(format!("png 编码失败: {e}")))?;
        }
        OutFormat::Webp => {
            let enc = image::codecs::webp::WebPEncoder::new_lossless(&mut buf);
            enc.write_image(&page, wu, hu, image::ExtendedColorType::Rgba8)
                .map_err(|e| MediaError::Img(format!("webp 编码失败: {e}")))?;
        }
        OutFormat::Avif => {
            // speed 10 = 最快档（rav1e 变速；质量由 quality 控制，33MP 图 <=3s 量级）
            let q = quality.unwrap_or(80);
            let enc = image::codecs::avif::AvifEncoder::new_with_speed_quality(&mut buf, 10, q);
            enc.write_image(&page, wu, hu, image::ExtendedColorType::Rgba8)
                .map_err(|e| MediaError::Img(format!("avif 编码失败: {e}")))?;
        }
    }
    use std::io::Write;
    buf.flush()?;
    Ok((w, h))
}

/// RGBA8 → 白底（或指定背景）source-over 合成的 RGB8。
///
/// 公式 `(src*a + bg*(255-a) + 127)/255` 是**四舍五入的定点版本**，
/// 两平台逐字节同值（避免 `image` crate 的浮点合成在不同目标上差 1）。
pub fn rgba_to_rgb_with_background(rgba: &[u8], bg: [u8; 3]) -> Vec<u8> {
    let n = rgba.len() / 4;
    let mut out = Vec::with_capacity(n * 3);
    for i in 0..n {
        let a = rgba[i * 4 + 3] as u32;
        for c in 0..3 {
            let s = rgba[i * 4 + c] as u32;
            let b = bg[c] as u32;
            let v = (s * a + b * (255 - a) + 127) / 255;
            out.push(v as u8);
        }
    }
    out
}

/// alpha 通道是否真的被用过（全 255 = 不透明）；`/op/encode` 用它回 `alpha` 字段。
pub fn has_transparency(rgba: &[u8]) -> bool {
    rgba.chunks_exact(4).any(|px| px[3] != 255)
}

/// 按 EXIF Orientation（1..=8）把 RGBA8 缓冲摆正（纯函数，双后端同语义）。
///
/// 语义与 `exif::apply_orientation` 的 `image` 版本逐条对齐（5=transpose、
/// 6=rotate90 CW、7=transverse、8=rotate270 CW）；1/非法值原样返回。
/// 5..=8 宽高互换——调用方必须用**返回的** (w,h)。
pub fn orient_rgba(w: i32, h: i32, rgba: &[u8], orientation: u32) -> Result<(i32, i32, Vec<u8>)> {
    if w <= 0 || h <= 0 {
        return Err(MediaError::Img(format!("非法尺寸 {w}x{h}")));
    }
    let expect = (w as usize) * (h as usize) * 4;
    if rgba.len() != expect {
        return Err(MediaError::Img(format!(
            "RGBA 缓冲长度 {} 与 {w}x{h}x4 不符",
            rgba.len()
        )));
    }
    if !(2..=8).contains(&orientation) {
        return Ok((w, h, rgba.to_vec()));
    }
    let swap = (5..=8).contains(&orientation);
    let (ow, oh) = if swap { (h, w) } else { (w, h) };
    let mut out = vec![0u8; rgba.len()];
    let pixel = |x: i32, y: i32| {
        let i = ((y as usize) * (w as usize) + x as usize) * 4;
        &rgba[i..i + 4]
    };
    for y in 0..oh {
        for x in 0..ow {
            // 目标 (x,y) ← 源 (sx,sy)，按 EXIF 定义逐个反解
            let (sx, sy) = match orientation {
                2 => (w - 1 - x, y),                       // 水平镜像
                3 => (w - 1 - x, h - 1 - y),               // 180°
                4 => (x, h - 1 - y),                       // 垂直镜像
                5 => (y, x),                               // transpose（主对角）
                6 => (y, h - 1 - x),                       // 90° CW
                7 => (w - 1 - y, h - 1 - x),               // transverse（副对角）
                _ => (w - 1 - y, x),                       // 8 = 90° CCW
            };
            let src = pixel(sx, sy);
            let d = ((y as usize) * (ow as usize) + x as usize) * 4;
            out[d..d + 4].copy_from_slice(src);
        }
    }
    Ok((ow, oh, out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_size;

    fn gradient(w: u32, h: u32) -> Vec<u8> {
        (0..w * h * 3)
            .map(|i| (i % 251) as u8)
            .collect()
    }

    #[test]
    fn encode_jpeg_quality_changes_bytes() {
        let dir = std::env::temp_dir().join(format!("dam-enc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let px = gradient(64, 48);
        let q90 = dir.join("q90.jpg");
        let q40 = dir.join("q40.jpg");
        encode_rgb_to(64, 48, &px, &q90, Some(90)).unwrap();
        encode_rgb_to(64, 48, &px, &q40, Some(40)).unwrap();
        let (s90, s40) = (
            std::fs::metadata(&q90).unwrap().len(),
            std::fs::metadata(&q40).unwrap().len(),
        );
        assert!(s40 < s90, "q40({s40}) 应显著小于 q90({s90})");
        // 回读尺寸一致
        assert_eq!(image_size(&q90).unwrap(), (64, 48));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn encode_png_webp_roundtrip() {
        let dir = std::env::temp_dir().join(format!("dam-enc2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let px = gradient(32, 32);
        for (name, expect) in [("a.png", 32), ("a.webp", 32)] {
            let p = dir.join(name);
            encode_rgb_to(32, 32, &px, &p, Some(50)).unwrap();
            assert_eq!(image_size(&p).unwrap(), (expect, expect));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_bad_input_loudly() {
        let dir = std::env::temp_dir().join(format!("dam-enc3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("out.tif");
        assert!(encode_rgb_to(8, 8, &vec![0u8; 64 * 3], &p, None).is_err());
        let p = dir.join("out.jpg");
        // 缓冲长度不符
        assert!(encode_rgb_to(8, 8, &vec![0u8; 8 * 8], &p, None).is_err());
        // 非法尺寸
        assert!(encode_rgb_to(0, 8, &[], &p, None).is_err());
        // 非法 quality
        assert!(encode_rgb_to(8, 8, &vec![0u8; 64 * 3], &p, Some(0)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------------------------------------------------------- N6b 编码组

    /// 2x2 RGBA：四角不同色 + 不同 alpha，用来逐像素验方向/合成。
    fn quad() -> Vec<u8> {
        // (0,0)=A  (1,0)=B
        // (0,1)=C  (1,1)=D
        vec![
            10, 0, 0, 255, // A 红
            0, 20, 0, 128, // B 绿（半透明）
            0, 0, 30, 0, // C 蓝（全透明）
            40, 40, 40, 255, // D 灰
        ]
    }

    fn px(buf: &[u8], w: i32, x: i32, y: i32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    #[test]
    fn orient_rgba_all_eight_cases() {
        let (w, h) = (2, 2);
        let src = quad();
        // 目标 (0,0) 取哪个源像素，按 EXIF 定义逐个写死（= 真值表，不是实现镜像）
        for (o, expect_00, swap) in [
            (2u32, [0, 20, 0, 128], false),    // 水平镜像：左上 ← 右上
            (3, [40, 40, 40, 255], false),     // 180°：左上 ← 右下
            (4, [0, 0, 30, 0], false),         // 垂直镜像：左上 ← 左下
            (5, [10, 0, 0, 255], true),        // transpose：左上不变
            (6, [0, 0, 30, 0], true),          // 90° CW：左上 ← 左下
            (7, [40, 40, 40, 255], true),      // transverse：左上 ← 右下
            (8, [0, 20, 0, 128], true),        // 90° CCW：左上 ← 右上
        ] {
            let (ow, oh, out) = orient_rgba(w, h, &src, o).unwrap();
            assert_eq!((ow, oh), if swap { (h, w) } else { (w, h) }, "orientation={o} 尺寸");
            assert_eq!(px(&out, ow, 0, 0), expect_00, "orientation={o} 左上角");
            assert_eq!(out.len(), src.len());
        }
        // 1 / 0 / 9 = 不处理（原样返回，尺寸不变）
        for o in [0u32, 1, 9] {
            let (ow, oh, out) = orient_rgba(w, h, &src, o).unwrap();
            assert_eq!((ow, oh), (2, 2), "orientation={o} 不该动尺寸");
            assert_eq!(out, src, "orientation={o} 不该动像素");
        }
        // 缓冲长度/尺寸不合法 → 响亮报错
        assert!(orient_rgba(2, 2, &[0u8; 3], 1).is_err());
        assert!(orient_rgba(0, 2, &[], 1).is_err());
    }

    #[test]
    fn rgba_flatten_uses_background_and_rounds_half_up() {
        // alpha=0 → 完全取背景；alpha=255 → 完全取源色；alpha=128 → 四舍五入
        let rgba = vec![
            10, 20, 30, 0, //
            10, 20, 30, 255, //
            200, 100, 50, 128, //
        ];
        let out = rgba_to_rgb_with_background(&rgba, [255, 255, 0]);
        assert_eq!(&out[0..3], &[255, 255, 0], "全透明 = 背景色");
        assert_eq!(&out[3..6], &[10, 20, 30], "不透明 = 源色");
        // 半透明：R=(200*128+255*127+127)/255=227、G=177、B=(50*128+0*127+127)/255=25
        assert_eq!(&out[6..9], &[227, 177, 25]);
    }

    #[test]
    fn has_transparency_detects_real_alpha_use() {
        assert!(!has_transparency(&[1, 2, 3, 255, 4, 5, 6, 255]));
        assert!(has_transparency(&[1, 2, 3, 255, 4, 5, 6, 254]));
    }

    #[test]
    fn encode_rgba_keeps_alpha_in_png_and_flattens_in_jpeg() {
        let dir = std::env::temp_dir().join(format!("dam-enc4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rgba = vec![200, 10, 10, 0, 200, 10, 10, 255];
        let png = dir.join("a.png");
        encode_rgba_to(2, 1, &rgba, &png, None, None).unwrap();
        let (_, _, back) = crate::rgba8(&png).unwrap();
        assert_eq!(&back[3], &0, "png 必须保留 alpha=0");
        assert_eq!(back, rgba, "png 双像素逐字节回读一致");

        let jpg = dir.join("a.jpg");
        encode_rgba_to(2, 1, &rgba, &jpg, Some(95), Some([0, 0, 255])).unwrap();
        let (_, _, back) = crate::rgba8(&jpg).unwrap();
        assert_eq!(back[3], 255, "jpeg 解开后 alpha 恒 255");
        assert!(back[2] > 200, "透明像素合成到蓝底：{back:?}");

        let avif = dir.join("a.avif");
        encode_rgba_to(2, 1, &rgba, &avif, Some(60), None).unwrap();
        let raw = std::fs::read(&avif).unwrap();
        assert_eq!(&raw[4..12], b"ftypavif", "avif 容器品牌");
        assert_eq!(crate::image_size(&avif).unwrap(), (2, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn encode_rgba_rejects_bad_input_loudly() {
        let dir = std::env::temp_dir().join(format!("dam-enc5-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("ok.png");
        // 2x2 RGBA = 16 字节；给 8 字节 → 长度不符
        assert!(encode_rgba_to(2, 2, &[0u8; 8], &p, None, None).is_err(), "长度不符");
        assert!(encode_rgba_to(0, 1, &[], &p, None, None).is_err(), "非法尺寸");
        assert!(
            encode_rgba_to(2, 1, &[0u8; 8], &p, Some(101), None).is_err(),
            "quality 越界"
        );
        let p = dir.join("x.tif");
        assert!(encode_rgba_to(2, 1, &[0u8; 8], &p, None, None).is_err(), "未知扩展名");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
